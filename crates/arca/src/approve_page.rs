//! The owner approval page: an open view of the waiting plans, one passkey approval POST, one registration POST, and the
//! setup-code path that trusts the first passkey from the page and the machine's own screen.
//!
//! The page routes are mounted only when `approvalOrigin` is configured. They sit outside the MCP bearer check and outside
//! the MCP `Origin` refusal: GETs admit no `Origin` or the exact configured one, and POSTs admit only the exact configured
//! one, before any body is read. `/mcp` and every other MCP route keep refusing every `Origin`; `crates/arca-mcp/src/http.rs`
//! is unchanged, and this router is merged into its result in `cmd_serve`.
//!
//! `approvalOrigin` is the page origin and the WebAuthn rp id. It is not the host an approval statement names: the statement
//! host stays `approval_host(&cfg)`.

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use arca_approval::passkey::{STATE_PASSKEYS_FILE, format_passkey_line};
use arca_approval::{PasskeyLine, load_state_passkeys};
use arca_mcp::CallError;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::handlers::{Clock, approve_plan, list_plans};
use crate::kernel::Kernel;

/// The page's own body cap. `arca-mcp`'s 64 KiB layer is private and does not reach merged routes.
const PAGE_BODY_BYTES: usize = 64 * 1024;
/// A setup code lives for ten minutes.
const CODE_TTL_MS: u64 = 10 * 60 * 1000;
/// A setup-code request is spaced by sixty seconds.
const CODE_SPACING_MS: u64 = 60 * 1000;
/// The setup-code budget window.
const CODE_HOUR_MS: u64 = 60 * 60 * 1000;
/// The most setup-code requests and the most wrong setup codes in the window.
const MAX_CODE_REQUESTS: usize = 10;
const MAX_WRONG_CODES: usize = 5;
/// The digits of a setup code.
const CODE_DIGITS: u32 = 8;
const CODE_MODULUS: u32 = 100_000_000;

/// A display the setup code can be shown on. The only production implementation is the desktop notification; the Arca OS
/// console follow-up supplies another, and tests supply a fake that captures the code.
pub trait SetupCodeDisplay: Send + Sync {
    /// Shows `code` transiently. An error means no code should stay valid.
    fn show(&self, code: &str) -> Result<(), String>;
}

/// The desktop display: `notify-send -e "Arca setup code" "<code>"`, with no shell and a five-second timeout.
pub struct DesktopDisplay;

impl SetupCodeDisplay for DesktopDisplay {
    fn show(&self, code: &str) -> Result<(), String> {
        let mut child = std::process::Command::new("notify-send")
            .arg("-e")
            .arg("Arca setup code")
            .arg(code)
            .spawn()
            .map_err(|_| "No screen to show the code on".to_string())?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => return Ok(()),
                Ok(Some(_)) => return Err("No screen to show the code on".to_string()),
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("No screen to show the code on".to_string());
                }
                Err(_) => return Err("No screen to show the code on".to_string()),
            }
        }
    }
}

/// The one page state: the configured origin and its rp id, the kernel plans are read from and approved in, the clock, the
/// state-directory passkeys file, the setup-code budget and the display.
struct PageState {
    origin: String,
    rp_id: String,
    kernel: Arc<Mutex<Kernel>>,
    clock: Clock,
    state_dir: PathBuf,
    base_passkeys: Vec<PasskeyLine>,
    setup: Arc<Mutex<SetupState>>,
    display: Arc<dyn SetupCodeDisplay>,
}

/// The transient setup-code state. No code is ever persisted, logged or returned in a response.
#[derive(Default)]
struct SetupState {
    live: Option<LiveCode>,
    requests: Vec<u64>,
    wrong: Vec<u64>,
    locked_until: Option<u64>,
    last_request: Option<u64>,
}

struct LiveCode {
    code: String,
    expires: u64,
}

impl SetupState {
    fn prune(&mut self, now: u64) {
        self.requests
            .retain(|at| now.saturating_sub(*at) < CODE_HOUR_MS);
        self.wrong
            .retain(|at| now.saturating_sub(*at) < CODE_HOUR_MS);
        if let Some(until) = self.locked_until
            && now >= until
        {
            self.locked_until = None;
        }
    }

    fn locked_minutes(&self, now: u64) -> Option<u64> {
        self.locked_until
            .filter(|until| now < *until)
            .map(|until| until.saturating_sub(now).div_ceil(60_000))
    }

    fn lock(&mut self, now: u64) {
        self.locked_until = Some(now.saturating_add(CODE_HOUR_MS));
        self.live = None;
    }

    fn live_if_fresh(&mut self, now: u64) -> Option<&LiveCode> {
        if self.live.as_ref().is_some_and(|live| live.expires <= now) {
            self.live = None;
        }
        self.live.as_ref()
    }
}

/// Builds the page router for `origin` with the desktop display. The caller has already validated `origin`.
///
/// # Panics
///
/// Panics when `origin` is not the validated `https://` origin. `cmd_serve` only calls this after the config check.
pub fn router(
    origin: String,
    kernel: Arc<Mutex<Kernel>>,
    clock: Clock,
    state_dir: PathBuf,
) -> Router {
    router_with(origin, kernel, clock, state_dir, Arc::new(DesktopDisplay))
}

/// Builds the page router with a display seam. Tests use a fake; `cmd_serve` uses [`router`].
///
/// # Panics
///
/// Panics when `origin` is not the validated `https://` origin.
pub fn router_with(
    origin: String,
    kernel: Arc<Mutex<Kernel>>,
    clock: Clock,
    state_dir: PathBuf,
    display: Arc<dyn SetupCodeDisplay>,
) -> Router {
    let rp_id = origin
        .strip_prefix("https://")
        .and_then(|authority| authority.split(':').next())
        .filter(|host| !host.is_empty())
        .expect("approvalOrigin is validated before the page router is built")
        .to_string();
    let base_passkeys = kernel
        .lock()
        .map(|kernel| kernel.owner_passkey_lines())
        .unwrap_or_default();
    let state = Arc::new(PageState {
        origin,
        rp_id,
        kernel,
        clock,
        state_dir,
        base_passkeys,
        setup: Arc::new(Mutex::new(SetupState::default())),
        display,
    });
    Router::new()
        .route("/approve", get(page))
        .route("/approve/plans", get(plans))
        .route("/approve/approve", post(approve))
        .route("/approve/setup-code", post(setup_code))
        .route("/approve/register", post(register))
        .layer(DefaultBodyLimit::max(PAGE_BODY_BYTES))
        .layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            origin_rule,
        ))
        .with_state(state)
}

/// The page's Origin rule, before any body is read: a POST needs the exact configured Origin; a GET needs none or the exact
/// one. Anything else is 403.
async fn origin_rule(
    State(state): State<Arc<PageState>>,
    request: Request,
    next: Next,
) -> Response {
    let origin = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok());
    let allowed = if request.method() == Method::POST {
        origin == Some(state.origin.as_str())
    } else {
        origin.is_none() || origin == Some(state.origin.as_str())
    };
    if !allowed {
        return StatusCode::FORBIDDEN.into_response();
    }
    next.run(request).await
}

async fn page() -> Response {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        include_str!("approve_page.html"),
    )
        .into_response()
}

async fn plans(State(state): State<Arc<PageState>>) -> Response {
    let now = (state.clock)();
    let kernel = match state.kernel.lock() {
        Ok(kernel) => kernel,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    match list_plans(&kernel, now) {
        Ok(mut body) => {
            body["passkeysTrusted"] = json!(kernel.owner_passkey_count());
            Json(body).into_response()
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn approve(State(state): State<Arc<PageState>>, body: Bytes) -> Response {
    let Ok(args) = serde_json::from_slice::<Value>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let now = (state.clock)();
    let mut kernel = match state.kernel.lock() {
        Ok(kernel) => kernel,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    match approve_plan(&mut kernel, &args, now) {
        Ok(result) => Json(result).into_response(),
        Err(CallError::Refused { code, message }) => (
            StatusCode::FORBIDDEN,
            Json(json!({ "refusal": code, "message": message })),
        )
            .into_response(),
        Err(CallError::Invalid(_)) => StatusCode::BAD_REQUEST.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn setup_code(State(state): State<Arc<PageState>>) -> Response {
    if trusted_count(&state) > 0 {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "message": "A passkey is already trusted" })),
        )
            .into_response();
    }
    let now = (state.clock)();
    let mut setup = match state.setup.lock() {
        Ok(setup) => setup,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    setup.prune(now);
    if let Some(minutes) = setup.locked_minutes(now) {
        return locked(minutes);
    }
    if setup.requests.len() >= MAX_CODE_REQUESTS {
        setup.lock(now);
        return locked(setup.locked_minutes(now).unwrap_or(1));
    }
    // Every request that passes the lock counts, including one refused while a code is live, so repeated clicks cannot
    // bypass the hourly bound.
    setup.requests.push(now);
    if setup.live_if_fresh(now).is_some() {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({ "message": "A code is already on your screen" })),
        )
            .into_response();
    }
    if setup.wrong.len() >= MAX_WRONG_CODES {
        setup.lock(now);
        return locked(setup.locked_minutes(now).unwrap_or(1));
    }
    if let Some(last) = setup.last_request
        && now.saturating_sub(last) < CODE_SPACING_MS
    {
        let seconds = (CODE_SPACING_MS - now.saturating_sub(last)).div_ceil(1000);
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({ "message": format!("Wait {seconds} more second(s)") })),
        )
            .into_response();
    }
    setup.last_request = Some(now);
    let Ok(code) = fresh_code() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    if state.display.show(&code).is_err() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "message": "No screen to show the code on" })),
        )
            .into_response();
    }
    setup.live = Some(LiveCode {
        code,
        expires: now.saturating_add(CODE_TTL_MS),
    });
    Json(json!({ "issued": true })).into_response()
}

async fn register(State(state): State<Arc<PageState>>, body: Bytes) -> Response {
    let Ok(value) = serde_json::from_slice::<Value>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(credential_id) = value.get("credentialId").and_then(Value::as_str) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(public_key) = value.get("publicKey").and_then(Value::as_str) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if value.get("publicKeyAlgorithm").and_then(Value::as_i64) != Some(-7) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Ok(line) = format_passkey_line(&state.rp_id, &state.origin, credential_id, public_key)
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(setup_code) = value.get("setupCode") else {
        return Json(json!({ "line": line })).into_response();
    };
    let Some(setup_code) = setup_code.as_str() else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if trusted_count(&state) > 0 {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "message": "A passkey is already trusted" })),
        )
            .into_response();
    }
    let now = (state.clock)();
    let mut setup = match state.setup.lock() {
        Ok(setup) => setup,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    setup.prune(now);
    if let Some(minutes) = setup.locked_minutes(now) {
        return locked(minutes);
    }
    let Some(live) = setup.live_if_fresh(now) else {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "message": "The setup code expired" })),
        )
            .into_response();
    };
    if setup_code != live.code {
        setup.wrong.push(now);
        if setup.wrong.len() >= MAX_WRONG_CODES {
            setup.lock(now);
            return locked(setup.locked_minutes(now).unwrap_or(1));
        }
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "message": "The setup code is wrong" })),
        )
            .into_response();
    }
    let state_path = state.state_dir.join(STATE_PASSKEYS_FILE);
    if write_state_line(&state.state_dir, &line).is_err() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "message": "The passkey could not be saved" })),
        )
            .into_response();
    }
    let state_lines = match load_state_passkeys(&state_path) {
        Ok(lines) => lines,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "message": "The passkey file could not be read" })),
            )
                .into_response();
        }
    };
    let mut lines = state.base_passkeys.clone();
    for line in state_lines {
        if !lines.iter().any(|seen| {
            seen.credential_id == line.credential_id || seen.fingerprint == line.fingerprint
        }) {
            lines.push(line);
        }
    }
    let (trusted, count) = match state.kernel.lock() {
        Ok(mut kernel) => {
            kernel.replace_owner_passkeys(lines);
            (true, kernel.owner_passkey_count())
        }
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    if count == 0 {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "message": "The passkey could not be trusted" })),
        )
            .into_response();
    }
    setup.live = None;
    eprintln!("arca: passkey trusted, {count} passkey(s) kept");
    Json(json!({ "trusted": trusted, "line": line })).into_response()
}

fn trusted_count(state: &PageState) -> usize {
    state
        .kernel
        .lock()
        .map(|kernel| kernel.owner_passkey_count())
        .unwrap_or(0)
}

fn locked(minutes: u64) -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        Json(json!({ "message": format!("Setup is locked for {minutes} more minute(s)") })),
    )
        .into_response()
}

fn fresh_code() -> Result<String, ()> {
    let mut bytes = [0u8; 4];
    if getrandom::fill(&mut bytes).is_err() {
        // The caller's process has no usable entropy; the code must not fall back to a weaker source.
        return Err(());
    }
    Ok(format!(
        "{:0width$}",
        u32::from_le_bytes(bytes) % CODE_MODULUS,
        width = CODE_DIGITS as usize
    ))
}

/// Writes `line` as a new line of the state passkeys file by atomic rename in `dir`. The file is mode 0600 and owned by this
/// process; the state directory is already 0700.
fn write_state_line(dir: &Path, line: &str) -> std::io::Result<()> {
    let path = dir.join(STATE_PASSKEYS_FILE);
    let mut body = match fs::read(&path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error),
    };
    if !body.is_empty() && !body.ends_with(b"\n") {
        body.push(b'\n');
    }
    body.extend_from_slice(line.as_bytes());
    body.push(b'\n');
    let tmp = dir.join(format!(".{STATE_PASSKEYS_FILE}.{}.tmp", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(&body)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, &path)?;
    // Keep the directory entry durable too; a failure here is a storage failure, not a trust change.
    fs::File::open(dir)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> u64 {
        1_793_361_600_000
    }

    #[test]
    fn setup_limits_lock_after_ten_requests_and_unlock_after_the_hour() {
        let mut setup = SetupState::default();
        for _ in 0..MAX_CODE_REQUESTS {
            setup.prune(now());
            setup.requests.push(now());
        }
        assert!(setup.requests.len() >= MAX_CODE_REQUESTS);
        setup.lock(now());
        assert_eq!(setup.locked_minutes(now()), Some(60));
        assert_eq!(setup.locked_minutes(now() + CODE_HOUR_MS - 1), Some(1));
        setup.prune(now() + CODE_HOUR_MS);
        assert_eq!(setup.locked_minutes(now() + CODE_HOUR_MS), None);
        assert!(setup.requests.is_empty());
    }

    #[test]
    fn a_live_code_is_never_replaced_by_a_fresh_one() {
        let mut setup = SetupState::default();
        setup.live = Some(LiveCode {
            code: "12345678".to_string(),
            expires: now() + CODE_TTL_MS,
        });
        assert!(setup.live_if_fresh(now()).is_some());
        assert!(setup.live_if_fresh(now() + CODE_TTL_MS).is_none());
        assert!(setup.live.is_none());
    }
}
