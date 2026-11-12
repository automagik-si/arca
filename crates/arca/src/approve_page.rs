//! The owner approval page: an open view of the waiting plans, one passkey approval POST, one registration POST, and the
//! setup-code path that trusts the first passkey from the page and the machine's own screen.
//!
//! The page routes are mounted only when `approvalOrigin` is configured. They sit outside the MCP bearer check and outside
//! the MCP `Origin` refusal: GETs admit no `Origin` or the exact configured one, and POSTs admit only the exact configured
//! one, before any body is read. The canvas chat POST is the one exception: with no `Origin`, it accepts a browser's
//! `Sec-Fetch-Site: same-origin`. `/mcp` and every other MCP route keep refusing every `Origin`; `crates/arca-mcp/src/http.rs`
//! is unchanged, and this router is merged into its result in `cmd_serve`.
//!
//! `approvalOrigin` is the page origin and the WebAuthn rp id. It is not the host an approval statement names: the statement
//! host stays `approval_host(&cfg)`. The origin is `https://` and a host name, or `http://localhost:<port>`, the page of a
//! node on its own machine's loopback listener, whose rp id is `localhost`.
//!
//! The setup code reaches the owner on the machine's own screen, or on a terminal of the node's own user: the unix
//! socket, and no other listener, carries `POST /local/setup-code` (see [`Page::local_router`]), which `arca setup-code`
//! calls. It answers only a caller who has just made a file in the node's state directory, which nobody but the node's
//! user can, and a peer on a forwarded socket cannot.
//!
//! With an owner approval request store (`Page::with_approval_requests`), the page lists the requests agents made with
//! `owner_approval_request` in their own section, as data, and approves one with one tap: `POST /approve/requests/begin`
//! mints the statement, the passkey signs it, and `POST /approve/requests/approve` takes the request id, that statement and
//! the proof (see `crate::approval_requests`). `POST /approve/requests/deny` declines one without a passkey.
//!
//! With `approval.linkedHosts`, the page also lists the waiting action proposals of other kernels on this machine and relays
//! the owner's signed approval or decline for one of them to that kernel over its own socket (see `crate::linked`). The page
//! never checks or spends another kernel's approval, and never asks its own kernel about another kernel's id.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use arca_approval::passkey::{STATE_PASSKEYS_FILE, format_passkey_line};
use arca_approval::{ARCA_NAMESPACE, ArcaAction, PasskeyLine, load_state_passkeys, parse_passkeys};
use arca_kernel::canon::canonical_json;
use arca_mcp::CallError;
use axum::body::{Body, Bytes, to_bytes};
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodRouter, any, get, post};
use axum::{Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tokio_stream::StreamExt as _;
use tokio_stream::wrappers::ReceiverStream;

use crate::approval_requests::{ApprovalRequests, PageRefusal, passkey_summary};
use crate::canvas::{self, Canvas};
use crate::config::LinkedHost;
use crate::handlers::{
    ActionExecutor, Clock, action_statement, approve_plan, decline_plan, fresh_nonce, list_plans,
    run_approved_action,
};
use crate::kernel::Kernel;
use crate::linked::{self, RelayError};
use crate::passkey_change::{self, write_state_bytes};
use crate::peer_sync::PeerSyncer;

/// The page's own body cap. `arca-mcp`'s 64 KiB layer is private and does not reach merged routes.
const PAGE_BODY_BYTES: usize = 64 * 1024;
/// The refusal a linked-host name the page does not list gets, in the owner's words.
const NOT_LINKED: &str = "this machine is not linked for approvals yet";
/// A setup code lives for ten minutes.
const CODE_TTL_MS: u64 = 10 * 60 * 1000;
/// A setup-code request is spaced by sixty seconds.
const CODE_SPACING_MS: u64 = 60 * 1000;
/// The setup-code budget window.
const CODE_HOUR_MS: u64 = 60 * 60 * 1000;
/// The most setup-code requests and the most wrong setup codes in the window.
const MAX_CODE_REQUESTS: usize = 10;
const MAX_WRONG_CODES: usize = 5;
const MAX_GLOBAL_WRONG_CODES: usize = 50;
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
#[derive(Clone)]
struct PageState {
    origin: String,
    rp_id: String,
    kernel: Arc<Mutex<Kernel>>,
    clock: Clock,
    state_dir: PathBuf,
    base_passkeys: Vec<PasskeyLine>,
    setup: Arc<Mutex<SetupState>>,
    display: Arc<dyn SetupCodeDisplay>,
    log: Arc<dyn Fn(&str) + Send + Sync>,
    trust_forwarded_for: bool,
    canvas: Arc<Canvas>,
    chat_slots: Arc<Semaphore>,
    actions: ActionExecutor,
    linked: Arc<Vec<LinkedHost>>,
    peer_sync: Option<PeerSyncer>,
    requests: Option<Arc<ApprovalRequests>>,
    gate: Option<PageGate>,
    /// The line that prints the setup code on a terminal of this machine, for a machine with no screen.
    setup_command: String,
}

/// What the page shows for the terminal's command when `serve` gave it none: the command, with the config to fill in.
const SETUP_COMMAND: &str = "arca setup-code --config <this node's config>";

/// Why the page is closed right now, in words for the owner, or `None` while it is open. `serve` gives the page of this
/// machine alone a gate that is closed while the node does not hold both loopback addresses of the page's port.
pub type PageGate = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// The refusal code of a page that is closed by its gate.
pub const PAGE_CLOSED: &str = "page_closed";

/// The path the unix socket alone carries for a setup code on the terminal.
pub const LOCAL_SETUP_CODE_PATH: &str = "/local/setup-code";
/// The prefix of the file a terminal's request makes in the state directory to show it can write there. The rest of the
/// name is the request's own random hexadecimal.
pub const LOCAL_PROOF_PREFIX: &str = "setup-code-request-";
/// The length of that random part: 16 bytes in lowercase hexadecimal.
pub const LOCAL_PROOF_LEN: usize = 32;

/// The transient setup-code state. No code is ever persisted or logged. One is returned in a response only by the unix
/// socket's [`LOCAL_SETUP_CODE_PATH`], to a caller who can write the node's state directory.
#[derive(Default)]
struct SetupState {
    live: Option<LiveCode>,
    requests: Vec<u64>,
    wrong: Vec<u64>,
    callers: HashMap<String, Vec<u64>>,
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
        self.callers.retain(|_, attempts| {
            attempts.retain(|at| now.saturating_sub(*at) < CODE_HOUR_MS);
            !attempts.is_empty()
        });
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
    }

    fn caller_locked_minutes(&self, caller: &str, now: u64) -> Option<u64> {
        self.callers
            .get(caller)
            .filter(|attempts| attempts.len() >= MAX_WRONG_CODES)
            .and_then(|attempts| attempts.first())
            .map(|first| {
                first
                    .saturating_add(CODE_HOUR_MS)
                    .saturating_sub(now)
                    .div_ceil(60_000)
            })
    }

    fn wrong_code(&mut self, caller: String, now: u64) {
        self.callers.entry(caller).or_default().push(now);
        self.wrong.push(now);
        if self.wrong.len() >= MAX_GLOBAL_WRONG_CODES {
            self.locked_until = self
                .wrong
                .first()
                .map(|first| first.saturating_add(CODE_HOUR_MS));
        }
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
/// Panics when `origin` is not a validated approval origin. `cmd_serve` only calls this after the config check.
pub fn router(
    origin: String,
    kernel: Arc<Mutex<Kernel>>,
    clock: Clock,
    state_dir: PathBuf,
) -> Router {
    router_with(origin, kernel, clock, state_dir, Arc::new(DesktopDisplay))
}

/// Builds a direct page router with a display seam. Use [`Page`] to share state across listener variants.
///
/// # Panics
///
/// Panics when `origin` is not a validated approval origin.
pub fn router_with(
    origin: String,
    kernel: Arc<Mutex<Kernel>>,
    clock: Clock,
    state_dir: PathBuf,
    display: Arc<dyn SetupCodeDisplay>,
) -> Router {
    Page::new(origin, kernel, clock, state_dir, display).router(false)
}

/// One approval-page binding, shared by the direct and trusted-proxy routers.
pub struct Page {
    state: PageState,
}

impl Page {
    /// Binds the validated origin, kernel, clock, state directory and display once.
    ///
    /// # Panics
    ///
    /// Panics when `origin` is not a validated approval origin.
    pub fn new(
        origin: String,
        kernel: Arc<Mutex<Kernel>>,
        clock: Clock,
        state_dir: PathBuf,
        display: Arc<dyn SetupCodeDisplay>,
    ) -> Self {
        let rp_id = if arca_approval::localhost_origin_port(&origin).is_some() {
            arca_approval::LOCALHOST_RP.to_string()
        } else {
            origin
                .strip_prefix("https://")
                .and_then(|authority| authority.split(':').next())
                .filter(|host| !host.is_empty())
                .expect("approvalOrigin is validated before the page router is built")
                .to_string()
        };
        let base_passkeys = kernel
            .lock()
            .map(|kernel| kernel.owner_passkey_lines())
            .unwrap_or_default();
        let origin_for_canvas = origin.clone();
        Self {
            state: PageState {
                origin,
                rp_id,
                kernel,
                clock,
                state_dir,
                base_passkeys,
                setup: Arc::new(Mutex::new(SetupState::default())),
                display,
                log: Arc::new(|line| eprintln!("{line}")),
                trust_forwarded_for: false,
                canvas: Arc::new(
                    Canvas::new(
                        origin_for_canvas.clone(),
                        &crate::config::default_canvas_origin(),
                    )
                    .expect("the default canvas origin is loopback"),
                ),
                chat_slots: Arc::new(Semaphore::new(canvas::CHAT_CONCURRENCY)),
                actions: ActionExecutor::default(),
                linked: Arc::new(Vec::new()),
                peer_sync: None,
                requests: None,
                gate: None,
                setup_command: SETUP_COMMAND.to_string(),
            },
        }
    }

    /// Supplies the handlers an approved action runs through. `cmd_serve` passes the same handler set the MCP door holds;
    /// tests may pass a fake. Without them an action proposal can still be listed and declined, but not approved.
    /// Closes the page while `gate` says why: every page route, and the terminal's setup code, answer 503 with the
    /// gate's words and do nothing else. Without a gate the page is always open.
    pub fn with_gate(mut self, gate: PageGate) -> Self {
        self.state.gate = Some(gate);
        self
    }

    /// Names the exact line that prints the setup code on a terminal of this machine. The page shows it where it says
    /// the machine has no screen, and the answer of a setup-code request that found no screen carries it.
    pub fn with_setup_command(mut self, command: String) -> Self {
        self.state.setup_command = command;
        self
    }

    pub fn with_actions(mut self, actions: ActionExecutor) -> Self {
        self.state.actions = actions;
        self
    }

    /// The linked kernels on this machine whose waiting proposals the page lists, and to which it relays the owner's signed
    /// approvals and declines. The page never approves for another host: the linked kernel checks every approval itself.
    pub fn with_linked_hosts(mut self, hosts: Vec<LinkedHost>) -> Self {
        self.state.linked = Arc::new(hosts);
        self
    }

    /// Keeps this host's passkey list in sync on the linked hosts that ask for it, and shows a list that changed as a card
    /// the owner approves (see `crate::peer_sync`).
    pub fn with_peer_sync(mut self, syncer: PeerSyncer) -> Self {
        self.state.peer_sync = Some(syncer);
        self
    }

    /// Lists the owner approval requests agents make with `owner_approval_request`, and approves or declines one. The same
    /// store serves the three verbs.
    pub fn with_approval_requests(mut self, requests: Arc<ApprovalRequests>) -> Self {
        self.state.requests = Some(requests);
        self
    }

    /// Replaces the enrolment log sink; tests capture only the public log message.
    pub fn with_log(mut self, log: Arc<dyn Fn(&str) + Send + Sync>) -> Self {
        self.state.log = log;
        self
    }

    /// Replaces the loopback DSH origin. `cmd_serve` passes the validated `canvasOrigin`; tests pass a fresh loopback fake.
    pub fn with_canvas_origin(mut self, origin: &str) -> Self {
        self.state.canvas = Arc::new(
            Canvas::new(self.state.origin.clone(), origin).expect("canvasOrigin is validated"),
        );
        self
    }

    /// Replaces the chat stream idle and total caps. Tests use short values; `serve` keeps the sixty-second and ten-minute
    /// caps. Call it after [`Page::with_canvas_origin`], which builds a fresh canvas state.
    pub fn with_chat_timeouts(mut self, idle: Duration, total: Duration) -> Self {
        self.state.canvas = Arc::new(
            self.state
                .canvas
                .as_ref()
                .clone()
                .with_chat_timeouts(idle, total),
        );
        self
    }

    /// The canvas state, for a sign-in test to issue a session directly.
    pub fn canvas(&self) -> &Canvas {
        &self.state.canvas
    }

    /// The router of the unix socket alone: `POST /local/setup-code`, the setup code for a terminal of the node's own
    /// user. It shares this binding's setup state, so the code it answers with is the one the page then takes. No TCP
    /// listener and no services socket may carry it: it has no bearer check and no `Origin` rule of the page's, only
    /// its own proof (see [`local_setup_code`]).
    pub fn local_router(&self) -> Router {
        Router::new()
            .route(LOCAL_SETUP_CODE_PATH, post(local_setup_code))
            .layer(DefaultBodyLimit::max(1024))
            .with_state(Arc::new(self.state.clone()))
    }

    /// Builds a router sharing this binding's setup state. Only the proxy listener trusts forwarded caller addresses.
    pub fn router(&self, trust_forwarded_for: bool) -> Router {
        let state = Arc::new(PageState {
            trust_forwarded_for,
            ..self.state.clone()
        });
        // Every page route answers with and without a trailing slash: the same handler, behind the same Origin rule.
        // A browser, a bookmark or a proxy that adds the slash reaches the page it asked for, and a POST stays a POST.
        let both =
            |router: Router<Arc<PageState>>, path: &str, route: MethodRouter<Arc<PageState>>| {
                router
                    .route(path, route.clone())
                    .route(&format!("{path}/"), route)
            };
        let mut router = Router::new();
        for (path, route) in [
            ("/approve", get(page)),
            ("/approve/plans", get(plans)),
            ("/approve/approve", post(approve)),
            ("/approve/decline", post(decline)),
            ("/approve/requests/begin", post(request_begin)),
            ("/approve/requests/approve", post(request_approve)),
            ("/approve/requests/deny", post(request_deny)),
            ("/approve/peer-keys", post(peer_keys_trust)),
            ("/approve/peer-keys/sync", post(peer_keys_sync)),
            ("/approve/setup-code", post(setup_code)),
            ("/approve/setup-code/verify", post(verify_setup_code)),
            ("/approve/register", post(register)),
            ("/approve/passkeys", get(passkeys)),
            ("/approve/passkeys/change", post(passkey_change)),
            (canvas::PAGE_PATH, get(canvas_page).post(canvas_sign_in)),
            (canvas::CSS_PATH, get(canvas_css)),
            (canvas::JS_PATH, get(canvas_js)),
            (canvas::SNAPSHOT_PATH, get(canvas_snapshot)),
            (canvas::CHAT_PATH, post(canvas_chat)),
            (canvas::CHAT_HISTORY_PATH, get(canvas_history)),
        ] {
            router = both(router, path, route);
        }
        router
            .route("/approve/{*rest}", any(not_found))
            .layer(DefaultBodyLimit::max(PAGE_BODY_BYTES))
            .layer(middleware::from_fn_with_state(
                Arc::clone(&state),
                gate_rule,
            ))
            .layer(middleware::from_fn_with_state(
                Arc::clone(&state),
                origin_rule,
            ))
            .layer(middleware::from_fn(security_headers))
            .with_state(state)
    }
}

impl PageState {
    fn caller(&self, headers: &HeaderMap) -> String {
        if !self.trust_forwarded_for {
            return "direct".to_string();
        }
        headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .rfind(|token| !token.is_empty())
            })
            .unwrap_or("proxy-unknown")
            .to_string()
    }
}

/// The page's gate, after the Origin rule and before any handler or body: while the gate gives a reason, every page route
/// answers 503 with it, the page itself included. A closed page serves no card, takes no approval and gives no setup
/// code: the node takes nothing from the owner while it is closed, and no setup code exists to be typed elsewhere.
/// It does not undo a signature: a passkey approval the owner gave at another program's copy of the page is still a
/// valid approval of the card that program chose, until its statement expires. That program can spend it once the
/// page is open again, or through the MCP approval verbs, which this gate does not cover.
async fn gate_rule(State(state): State<Arc<PageState>>, request: Request, next: Next) -> Response {
    if let Some(why) = state.gate.as_ref().and_then(|gate| gate()) {
        return page_closed(why);
    }
    next.run(request).await
}

fn page_closed(why: String) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "refusal": PAGE_CLOSED, "message": why })),
    )
        .into_response()
}

/// The page's Origin rule, before any body is read: a POST needs the exact configured Origin; a GET needs none or the exact
/// one. The canvas chat POST also accepts a browser's `Sec-Fetch-Site: same-origin` when no Origin is sent. A present but
/// non-text Origin is never treated as missing, and a foreign Origin is always 403.
async fn origin_rule(
    State(state): State<Arc<PageState>>,
    request: Request,
    next: Next,
) -> Response {
    let origin = request
        .headers()
        .get(header::ORIGIN)
        .map(|value| value.to_str().ok());
    let origin_present = origin.is_some();
    let origin_valid = origin.is_some_and(|value| value.is_some_and(|value| value == state.origin));
    let same_origin_fetch = request
        .headers()
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok())
        == Some("same-origin");
    let path = request.uri().path();
    let chat = path.strip_suffix('/').unwrap_or(path) == canvas::CHAT_PATH;
    let allowed = if request.method() == Method::POST {
        origin_valid || (chat && !origin_present && same_origin_fetch)
    } else {
        origin_valid || !origin_present
    };
    if !allowed {
        return StatusCode::FORBIDDEN.into_response();
    }
    next.run(request).await
}

/// Headers on every `/approve` response, including the existing approval page, refusals and the proxied canvas. No response,
/// including a 403 or 404, is cacheable.
async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static(canvas::csp()),
    );
    headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        HeaderName::from_static("permissions-policy"),
        HeaderValue::from_static("publickey-credentials-get=(self)"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn page(State(state): State<Arc<PageState>>) -> Response {
    let ids = trusted_credential_ids(&state);
    let body = render_credentials(include_str!("approve_page.html"), &ids).replace(
        "__ARCA_SETUP_COMMAND__",
        &escape_attribute(&state.setup_command),
    );
    let mut response = body.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    response
}

async fn not_found() -> StatusCode {
    StatusCode::NOT_FOUND
}

async fn canvas_page(State(state): State<Arc<PageState>>, headers: HeaderMap) -> Response {
    let now = (state.clock)();
    if state.canvas.session(&headers, now).is_some() {
        return canvas_html(canvas::CANVAS_PAGE);
    }
    let ids = trusted_credential_ids(&state);
    canvas_html(render_credentials(canvas::SIGN_IN_PAGE, &ids))
}

async fn canvas_sign_in(State(state): State<Arc<PageState>>, body: Bytes) -> Response {
    let now = (state.clock)();
    let value: Value = if body.is_empty() {
        json!({})
    } else {
        match serde_json::from_slice(&body) {
            Ok(value) => value,
            Err(_) => return canvas_failure(&state, StatusCode::BAD_REQUEST, "request"),
        }
    };
    let Some(object) = value.as_object() else {
        return canvas_failure(&state, StatusCode::BAD_REQUEST, "request");
    };
    if object.is_empty() {
        let Some(statement) = state.canvas.issue_challenge(now) else {
            return canvas_json(
                StatusCode::TOO_MANY_REQUESTS,
                rate_body("Too many sign-in challenges", None),
            );
        };
        return canvas_json(StatusCode::OK, json!({ "statement": statement }));
    }
    let Some(statement) = object.get("statement").and_then(Value::as_str) else {
        return canvas_failure(&state, StatusCode::BAD_REQUEST, "request");
    };
    let Some(proof) = object.get("proof") else {
        return canvas_failure(&state, StatusCode::BAD_REQUEST, "request");
    };
    let Some(parsed) = state.canvas.parse_statement(statement, now) else {
        return canvas_failure(&state, StatusCode::FORBIDDEN, "statement");
    };
    // Required order: spend the exact live challenge before any signature work. A missing challenge costs this map lookup and
    // no verify at all.
    if !state.canvas.spend_challenge(&parsed, statement, now) {
        return canvas_failure(&state, StatusCode::FORBIDDEN, "challenge");
    }
    let Some(proof_bytes) = canonical_json(proof).ok() else {
        return canvas_failure(&state, StatusCode::FORBIDDEN, "proof");
    };
    let (verified, known) = match state.kernel.lock() {
        Ok(kernel) => (
            kernel.verify_passkey(ARCA_NAMESPACE, statement.as_bytes(), proof_bytes.as_bytes()),
            credential_known(&kernel, proof),
        ),
        Err(_) => return canvas_failure(&state, StatusCode::INTERNAL_SERVER_ERROR, "verify"),
    };
    let credential = match verified {
        Ok(credential) => credential,
        Err(refusal) => {
            let class = format!(
                "{} (credential {})",
                crate::handlers::refusal_code(&refusal),
                if known { "known" } else { "unknown" }
            );
            return canvas_failure(&state, StatusCode::FORBIDDEN, &class);
        }
    };
    let Some(cookie) = state.canvas.issue_session(&credential, now) else {
        return canvas_failure(&state, StatusCode::INTERNAL_SERVER_ERROR, "session");
    };
    let mut response = canvas_json(StatusCode::OK, json!({ "signedIn": true }));
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).expect("the canvas cookie is a header value"),
    );
    response
}

async fn canvas_css(State(state): State<Arc<PageState>>, headers: HeaderMap) -> Response {
    canvas_asset(
        &state,
        &headers,
        canvas::DSH_CSS_PATH,
        "text/css; charset=utf-8",
    )
    .await
}

async fn canvas_js(State(state): State<Arc<PageState>>, headers: HeaderMap) -> Response {
    canvas_asset(
        &state,
        &headers,
        canvas::DSH_JS_PATH,
        "text/javascript; charset=utf-8",
    )
    .await
}

async fn canvas_snapshot(State(state): State<Arc<PageState>>, headers: HeaderMap) -> Response {
    canvas_asset(
        &state,
        &headers,
        canvas::DSH_SNAPSHOT_PATH,
        "application/json; charset=utf-8",
    )
    .await
}

async fn canvas_history(State(state): State<Arc<PageState>>, headers: HeaderMap) -> Response {
    canvas_asset(
        &state,
        &headers,
        canvas::DSH_CHAT_HISTORY_PATH,
        "application/json; charset=utf-8",
    )
    .await
}

/// One chat turn. The session is checked before the body is read, and the Origin, media type, size, JSON and concurrency
/// checks all run before the upstream connect, so a refused turn is never billed to the canvas model. The answer is relayed
/// as a stream; the kernel keeps no write.
async fn canvas_chat(
    State(state): State<Arc<PageState>>,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let now = (state.clock)();
    if state.canvas.session(&headers, now).is_none() {
        return chat_refusal(StatusCode::UNAUTHORIZED);
    }
    if !is_json_content_type(&headers) {
        return chat_refusal(StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
    let body = match to_bytes(request.into_body(), canvas::CHAT_BODY_MAX).await {
        Ok(body) => body,
        Err(_) => return chat_refusal(StatusCode::PAYLOAD_TOO_LARGE),
    };
    if serde_json::from_slice::<Value>(&body).is_err() {
        return chat_refusal(StatusCode::BAD_REQUEST);
    }
    let Ok(slot) = state.chat_slots.clone().try_acquire_owned() else {
        return chat_refusal(StatusCode::TOO_MANY_REQUESTS);
    };
    let Some(chat) = state.canvas.chat(&body).await else {
        return chat_refusal(StatusCode::BAD_GATEWAY);
    };
    let content_type = HeaderValue::from_str(&chat.content_type)
        .unwrap_or_else(|_| HeaderValue::from_static("text/event-stream"));
    // The permit lives inside the body stream, so a stalled stream keeps its chat slot until the client is gone.
    let body = ReceiverStream::new(chat.body).map(move |chunk| {
        let _held = &slot;
        chunk
    });
    let mut response = Response::new(Body::from_stream(body));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, content_type);
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    // Ask a buffering reverse proxy to pass each event through instead of holding the stream.
    response.headers_mut().insert(
        HeaderName::from_static("x-accel-buffering"),
        HeaderValue::from_static("no"),
    );
    response
}

/// A JSON media type with optional parameters, and nothing else. The body is parsed separately, so `text/plain` JSON is
/// refused rather than forwarded.
fn is_json_content_type(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"))
}

/// A chat refusal carries no upstream bytes and is never cached.
fn chat_refusal(status: StatusCode) -> Response {
    let mut response = status.into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn canvas_asset(
    state: &PageState,
    headers: &HeaderMap,
    path: &str,
    content_type: &'static str,
) -> Response {
    let now = (state.clock)();
    if state.canvas.session(headers, now).is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match state.canvas.proxy(path).await {
        Some(body) => {
            let mut response = (StatusCode::OK, body).into_response();
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            response
        }
        None => canvas_error(),
    }
}

fn canvas_html(body: impl Into<String>) -> Response {
    let mut response = body.into().into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn canvas_json(status: StatusCode, body: Value) -> Response {
    let mut response = (status, Json(body)).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn canvas_failure(state: &PageState, status: StatusCode, class: &str) -> Response {
    // Only the class: never the statement, proof, challenge, credential id, key material or verifier's message.
    (state.log)(&format!("arca: canvas sign-in failed: {class}"));
    canvas_json(status, json!({ "message": "Sign-in failed" }))
}

fn trusted_credential_ids(state: &PageState) -> Vec<String> {
    let Ok(kernel) = state.kernel.lock() else {
        return Vec::new();
    };
    kernel
        .owner_passkey_lines()
        .iter()
        .map(|line| URL_SAFE_NO_PAD.encode(&line.credential_id))
        .collect()
}

/// `text` as the value of a double-quoted HTML attribute.
fn escape_attribute(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn render_credentials(html: &str, ids: &[String]) -> String {
    let json = serde_json::to_string(ids).expect("credential ids serialize");
    let escaped = json
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    html.replace("__ARCA_CREDENTIAL_IDS__", &escaped)
}

fn credential_known(kernel: &Kernel, proof: &Value) -> bool {
    let Some(encoded) = proof.get("credentialId").and_then(Value::as_str) else {
        return false;
    };
    let Ok(credential) = URL_SAFE_NO_PAD.decode(encoded) else {
        return false;
    };
    kernel
        .owner_passkey_lines()
        .iter()
        .any(|line| line.credential_id == credential)
}

fn canvas_error() -> Response {
    let mut response = (StatusCode::BAD_GATEWAY, canvas::ERROR_PAGE).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn plans(State(state): State<Arc<PageState>>) -> Response {
    let now = (state.clock)();
    let mut body = {
        let kernel = match state.kernel.lock() {
            Ok(kernel) => kernel,
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
        match list_plans(&kernel, now, true) {
            Ok(mut body) => {
                body["passkeysTrusted"] = json!(kernel.owner_passkey_count());
                if let Some(requests) = &state.requests {
                    match requests.list(&kernel, now) {
                        Ok(listed) => body["approvalRequests"] = json!(listed),
                        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
                    }
                    body["approvalPasskeys"] = passkey_summary(&kernel, &state.rp_id);
                }
                body
            }
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    };
    if let Some(syncer) = &state.peer_sync {
        body["peerKeys"] = peer_cards(syncer);
    }
    if state.linked.is_empty() {
        return Json(body).into_response();
    }
    let linked = Arc::clone(&state.linked);
    let Ok(listed) = tokio::task::spawn_blocking(move || linked_plans(&linked, now)).await else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let (rows, hosts) = listed;
    if let Some(plans) = body["plans"].as_array_mut() {
        plans.extend(rows);
    }
    body["linkedHosts"] = json!(hosts);
    Json(body).into_response()
}

/// The changed passkey lists waiting for the owner, one card per linked host: the host, the statement it issued, how many
/// passkeys the new list holds, and the card's words. No line is listed.
fn peer_cards(syncer: &PeerSyncer) -> Value {
    let cards: Vec<Value> = syncer
        .pending()
        .into_iter()
        .map(|(host, pending)| {
            let expires = serde_json::from_str::<Value>(&pending.statement)
                .ok()
                .and_then(|statement| statement["expiresAt"].as_str().map(str::to_string));
            json!({
                "linkedHost": host,
                "title": format!("Trust your new passkey list on {host}"),
                "keys": pending.keys,
                "statement": pending.statement,
                "expiresAt": expires,
            })
        })
        .collect();
    json!(cards)
}

/// `POST /approve/peer-keys/sync`: syncs this host's passkey list with every linked host now (at most once per thirty
/// seconds), and answers with the cards still waiting.
async fn peer_keys_sync(State(state): State<Arc<PageState>>) -> Response {
    let Some(syncer) = state.peer_sync.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(outcomes) = tokio::task::spawn_blocking(move || {
        let outcomes = syncer.sync_on_demand();
        (outcomes, peer_cards(&syncer))
    })
    .await
    else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let (outcomes, cards) = outcomes;
    let hosts: Vec<Value> = outcomes
        .into_iter()
        .map(|(name, outcome)| match outcome {
            crate::peer_sync::SyncOutcome::NotAsked => {
                json!({ "name": name, "status": "not_asked" })
            }
            crate::peer_sync::SyncOutcome::Synced(status) => {
                json!({ "name": name, "status": status })
            }
            crate::peer_sync::SyncOutcome::Failed(why) => {
                json!({ "name": name, "status": "failed", "reason": why })
            }
        })
        .collect();
    Json(json!({ "hosts": hosts, "peerKeys": cards })).into_response()
}

/// `POST /approve/peer-keys` `{host, statement, proof}`: relays this host's passkey list to `host` with the owner's passkey
/// proof over the statement that host issued, and answers with what the host said. The page never checks the proof itself.
async fn peer_keys_trust(State(state): State<Arc<PageState>>, body: Bytes) -> Response {
    let Some(syncer) = state.peer_sync.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(args) = serde_json::from_slice::<Value>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let host = match linked_host(&state, &args) {
        Ok(Some(host)) => host,
        Ok(None) => return StatusCode::BAD_REQUEST.into_response(),
        Err(refusal) => return refusal.into_response(),
    };
    let (Some(statement), Some(proof)) = (
        args.get("statement")
            .and_then(Value::as_str)
            .map(str::to_string),
        args.get("proof"),
    ) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let proof = match proof {
        Value::String(text) => text.clone(),
        Value::Object(_) => match canonical_json(proof) {
            Ok(text) => text,
            Err(_) => return StatusCode::BAD_REQUEST.into_response(),
        },
        _ => return StatusCode::BAD_REQUEST.into_response(),
    };
    let name = host.name.clone();
    let outcome =
        tokio::task::spawn_blocking(move || syncer.submit(&host, &statement, &proof)).await;
    match outcome {
        Ok(Ok(mut value)) => {
            value["host"] = json!(name);
            Json(value).into_response()
        }
        Ok(Err(error)) => relay_error_response(&name, error),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// The waiting action proposals of every linked kernel, each with `linkedHost` naming it and a fresh statement that names
/// the host the linked kernel says its statements carry, and one `{name, read}` per linked kernel. A kernel that does not
/// answer adds no row. A linked kernel's delete plans are not listed: the page relays only action proposals.
fn linked_plans(linked: &[LinkedHost], now: u64) -> (Vec<Value>, Vec<Value>) {
    let mut rows = Vec::new();
    let mut hosts = Vec::new();
    for host in linked {
        let listed = linked::call(host, "plan_list", json!({}));
        hosts.push(json!({ "name": host.name, "read": listed.is_ok() }));
        let Ok(listed) = listed else {
            continue;
        };
        for row in listed["plans"].as_array().into_iter().flatten() {
            if row["kind"] != json!("action") {
                continue;
            }
            let (Some(target), Some(id), Some(action), Some(digest)) = (
                row["host"].as_str(),
                row["id"].as_str(),
                row["action"].as_str(),
                row["argsDigest"].as_str(),
            ) else {
                continue;
            };
            let Ok(nonce) = fresh_nonce() else {
                continue;
            };
            let Ok(statement) = action_statement(target, id, action, digest, now, &nonce) else {
                continue;
            };
            let mut row = row.clone();
            row["linkedHost"] = json!(host.name);
            row["statement"] = json!(statement);
            rows.push(row);
        }
    }
    (rows, hosts)
}

/// The linked host a request names, when it names one. `Ok(None)` is this page's own kernel; a name the page does not
/// list is refused with [`NOT_LINKED`].
fn linked_host(state: &PageState, args: &Value) -> Result<Option<LinkedHost>, HostRefusal> {
    match args.get("host") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(name)) => state
            .linked
            .iter()
            .find(|host| host.name == *name)
            .cloned()
            .map(Some)
            .ok_or(HostRefusal::NotLinked),
        Some(_) => Err(HostRefusal::NotAName),
    }
}

/// Why a page POST's `host` is refused before anything is relayed.
enum HostRefusal {
    /// The page does not list this host.
    NotLinked,
    /// `host` is not a string.
    NotAName,
}

impl IntoResponse for HostRefusal {
    fn into_response(self) -> Response {
        match self {
            Self::NotLinked => refused("NotLinked", NOT_LINKED),
            Self::NotAName => StatusCode::BAD_REQUEST.into_response(),
        }
    }
}

/// Relays one call to a linked kernel and answers with what it said: its result with `host` added, its exact refusal, or
/// a plain reason when it could not be asked.
async fn relay(host: LinkedHost, verb: &'static str, arguments: Value) -> Response {
    let name = host.name.clone();
    let outcome = tokio::task::spawn_blocking(move || linked::call(&host, verb, arguments)).await;
    match outcome {
        Ok(Ok(mut value)) => {
            if let Some(object) = value.as_object_mut() {
                object.remove("truncated");
            }
            value["host"] = json!(name);
            Json(value).into_response()
        }
        Ok(Err(error)) => relay_error_response(&name, error),
        Err(_) => relay_error_response(&name, RelayError::Unreachable),
    }
}

/// A relayed call's failure as the page answers it: the linked kernel's exact refusal, or a plain reason when it could not
/// be asked.
fn relay_error_response(name: &str, error: RelayError) -> Response {
    match error {
        RelayError::Refused { code, message } => refused(&code, &message),
        RelayError::Failed(message) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "refusal": "Failed", "message": message })),
        )
            .into_response(),
        RelayError::Token => (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "refusal": "LinkToken",
                "message": format!("{name} does not accept this page's token"),
            })),
        )
            .into_response(),
        RelayError::Unreachable => (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "refusal": "Unreachable",
                "message": format!("{name} could not be reached just now"),
            })),
        )
            .into_response(),
    }
}

fn refused(code: &str, message: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({ "refusal": code, "message": message })),
    )
        .into_response()
}

/// The plan or proposal id a page POST names.
fn plan_id(args: &Value) -> Option<&str> {
    args.get("plan")
        .or_else(|| args.get("id"))
        .and_then(Value::as_str)
}

async fn approve(State(state): State<Arc<PageState>>, body: Bytes) -> Response {
    let Ok(args) = serde_json::from_slice::<Value>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let now = (state.clock)();
    let action = plan_id(&args).is_some_and(|id| id.starts_with("act-"));
    match linked_host(&state, &args) {
        Err(refusal) => return refusal.into_response(),
        Ok(Some(host)) => {
            // Another host's approval is never checked here: its bytes go to that kernel's own verifier.
            let (Some(id), Some(approval)) = (
                plan_id(&args).filter(|_| action),
                args.get("approval").and_then(Value::as_str),
            ) else {
                return StatusCode::BAD_REQUEST.into_response();
            };
            return relay(
                host,
                "action_approve",
                json!({ "plan": id, "approval": approval }),
            )
            .await;
        }
        Ok(None) => {}
    }
    if action {
        let kernel = Arc::clone(&state.kernel);
        let actions = state.actions.clone();
        let outcome =
            tokio::task::spawn_blocking(move || run_approved_action(&kernel, &actions, &args, now))
                .await;
        return match outcome {
            Ok(Ok(result)) => Json(result).into_response(),
            Ok(Err(error)) => call_error_response(error),
            Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
    }
    let mut kernel = match state.kernel.lock() {
        Ok(kernel) => kernel,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    match approve_plan(&mut kernel, &args, now) {
        Ok(result) => Json(result).into_response(),
        Err(error) => call_error_response(error),
    }
}

async fn decline(State(state): State<Arc<PageState>>, body: Bytes) -> Response {
    let Ok(args) = serde_json::from_slice::<Value>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let action = plan_id(&args).is_some_and(|id| id.starts_with("act-"));
    if !action {
        return call_error_response(CallError::Refused {
            code: "DoctorRequired".to_string(),
            message: "the page declines only action proposals; a delete plan needs the doctor-scope plan_decline"
                .to_string(),
        });
    }
    match linked_host(&state, &args) {
        Err(refusal) => return refusal.into_response(),
        Ok(Some(host)) => {
            let id = plan_id(&args).unwrap_or_default().to_string();
            return relay(host, "plan_decline", json!({ "plan": id })).await;
        }
        Ok(None) => {}
    }
    let mut kernel = match state.kernel.lock() {
        Ok(kernel) => kernel,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    match decline_plan(&mut kernel, &args) {
        Ok(result) => Json(result).into_response(),
        Err(error) => call_error_response(error),
    }
}

/// Which step of an owner approval request a page POST takes.
#[derive(Clone, Copy)]
enum RequestStep {
    Begin,
    Approve,
    Deny,
}

async fn request_begin(
    State(state): State<Arc<PageState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    request_step(state, &headers, body, RequestStep::Begin).await
}

async fn request_approve(
    State(state): State<Arc<PageState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    request_step(state, &headers, body, RequestStep::Approve).await
}

async fn request_deny(
    State(state): State<Arc<PageState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    request_step(state, &headers, body, RequestStep::Deny).await
}

/// One step of an owner approval request: `{id}` to begin or deny, `{id, statement, proof}` to approve, as
/// `application/json`. Nothing the body holds is echoed back.
async fn request_step(
    state: Arc<PageState>,
    headers: &HeaderMap,
    body: Bytes,
    step: RequestStep,
) -> Response {
    let Some(requests) = state.requests.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // As droidd's: a step is a JSON body, so a form or a text/plain post (which a page elsewhere could send) is refused.
    if !is_json_content_type(headers) {
        return page_refusal(PageRefusal {
            status: 415,
            code: "invalid",
            message: "the body must be sent as application/json".to_string(),
            extra: serde_json::Map::new(),
        });
    }
    let invalid = || {
        page_refusal(PageRefusal {
            status: 400,
            code: "invalid",
            message: "the body must be a JSON object naming the request's id".to_string(),
            extra: serde_json::Map::new(),
        })
    };
    let args: Value = if body.is_empty() {
        json!({})
    } else {
        match serde_json::from_slice(&body) {
            Ok(value) => value,
            Err(_) => return invalid(),
        }
    };
    let Some(id) = args.get("id").and_then(Value::as_str).map(str::to_string) else {
        return invalid();
    };
    let now = (state.clock)();
    let kernel = Arc::clone(&state.kernel);
    let rp_id = state.rp_id.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let kernel = kernel.lock().map_err(|_| PageRefusal {
            status: 500,
            code: "internal",
            message: "the kernel could not be read".to_string(),
            extra: serde_json::Map::new(),
        })?;
        match step {
            RequestStep::Begin => requests.begin(&kernel, &id, &rp_id, now),
            RequestStep::Deny => requests.deny(&kernel, &id, now),
            RequestStep::Approve => {
                let (Some(statement), Some(proof)) = (
                    args.get("statement").and_then(Value::as_str),
                    args.get("proof"),
                ) else {
                    return Err(PageRefusal {
                        status: 400,
                        code: "invalid",
                        message: "the body is {id, statement, proof}".to_string(),
                        extra: serde_json::Map::new(),
                    });
                };
                requests.approve(&kernel, &id, statement, proof, now)
            }
        }
    })
    .await;
    match outcome {
        Ok(Ok(value)) => canvas_json(StatusCode::OK, value),
        Ok(Err(refusal)) => page_refusal(refusal),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// A request step's refusal: `{refusal, message}` and any extra fields, at its own status.
fn page_refusal(refusal: PageRefusal) -> Response {
    let mut body = serde_json::Map::new();
    body.insert("refusal".to_string(), json!(refusal.code));
    body.insert("message".to_string(), json!(refusal.message));
    body.extend(refusal.extra);
    let status = StatusCode::from_u16(refusal.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    canvas_json(status, Value::Object(body))
}

fn call_error_response(error: CallError) -> Response {
    match error {
        CallError::Refused { code, message } | CallError::UntrustedRefused { code, message } => (
            StatusCode::FORBIDDEN,
            Json(json!({ "refusal": code, "message": message })),
        )
            .into_response(),
        // A refusal for want of room, or for asking too often: its fields stand beside the code, as the MCP door
        // writes them.
        CallError::RefusedWith {
            code,
            message,
            mut fields,
        } => {
            fields.insert("refusal".to_string(), json!(code));
            fields.insert("message".to_string(), json!(message));
            (StatusCode::FORBIDDEN, Json(Value::Object(fields))).into_response()
        }
        CallError::Invalid(_) => StatusCode::BAD_REQUEST.into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn passkeys(State(state): State<Arc<PageState>>) -> Response {
    let kernel = match state.kernel.lock() {
        Ok(kernel) => kernel,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    match passkey_change::listing(&kernel.owner_passkey_lines(), &state.state_dir) {
        Ok(keys) => canvas_json(StatusCode::OK, json!({ "passkeys": keys })),
        Err(error) => change_refused(error),
    }
}

/// Without a proof this prepares a removal statement by opaque target; with a proof it spends a signed change.
async fn passkey_change(State(state): State<Arc<PageState>>, body: Bytes) -> Response {
    let Ok(value) = serde_json::from_slice::<Value>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let mut kernel = match state.kernel.lock() {
        Ok(kernel) => kernel,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let now = (state.clock)();
    let line = if let Some(line) = value.get("line").and_then(Value::as_str) {
        line.to_string()
    } else if let Some(target) = value.get("target").and_then(Value::as_str) {
        match passkey_change::state_lines(&state.state_dir) {
            Ok(lines) => match lines
                .into_iter()
                .find(|line| passkey_change::target(line) == target)
            {
                Some(line) => line,
                None => {
                    return change_refused(arca_approval::ApprovalRefusal::Untrusted(
                        "only a state-file passkey can be removed".into(),
                    ));
                }
            },
            Err(error) => return change_refused(error),
        }
    } else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if let Some(statement) = value.get("statement").and_then(Value::as_str) {
        let Some(proof) = value.get("proof") else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        let Ok(proof) = canonical_json(proof) else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        match kernel.approve_passkey_change(
            &state.state_dir,
            &line,
            statement,
            proof.as_bytes(),
            now,
        ) {
            Ok(()) => {
                let count = kernel.owner_passkey_count();
                (state.log)(&format!("arca: passkeys changed, {count} passkey(s) kept"));
                canvas_json(StatusCode::OK, json!({ "trusted": true }))
            }
            Err(error) => change_refused(error),
        }
    } else {
        match passkey_change::statement(kernel.host(), ArcaAction::PasskeyRemove, &line, now) {
            Ok(statement) => canvas_json(StatusCode::OK, json!({ "statement": statement })),
            Err(error) => change_refused(error),
        }
    }
}

fn change_refused(error: arca_approval::ApprovalRefusal) -> Response {
    canvas_json(
        StatusCode::FORBIDDEN,
        json!({ "refusal": crate::handlers::refusal_code(&error), "message": error.to_string() }),
    )
}

async fn setup_code(State(state): State<Arc<PageState>>, headers: HeaderMap) -> Response {
    let now = (state.clock)();
    let mut setup = match state.setup.lock() {
        Ok(setup) => setup,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let kernel = match state.kernel.lock() {
        Ok(kernel) => kernel,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    if kernel.owner_passkey_count() > 0 {
        return already_trusted();
    }
    setup.prune(now);
    let caller = state.caller(&headers);
    if let Some(minutes) = setup
        .locked_minutes(now)
        .or_else(|| setup.caller_locked_minutes(&caller, now))
    {
        return locked(minutes);
    }
    if setup.requests.len() >= MAX_CODE_REQUESTS {
        setup.lock(now);
        return locked(setup.locked_minutes(now).unwrap_or(1));
    }
    // Every request that passes the lock counts, including one refused while a code is live, so repeated clicks cannot
    // bypass the hourly bound.
    setup.requests.push(now);
    if let Some(live) = setup.live_if_fresh(now) {
        // The next code can be asked for once this one has expired.
        let seconds = live.expires.saturating_sub(now).div_ceil(1000);
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(rate_body("A code is already on your screen", Some(seconds))),
        )
            .into_response();
    }
    if let Some(last) = setup.last_request
        && now.saturating_sub(last) < CODE_SPACING_MS
    {
        let seconds = (CODE_SPACING_MS - now.saturating_sub(last)).div_ceil(1000);
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(rate_body(
                format!("Wait {seconds} more second(s)"),
                Some(seconds),
            )),
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
            // The answer names the way that needs no screen, so the page can show the exact line.
            Json(json!({ "message": "No screen to show the code on", "command": state.setup_command })),
        )
            .into_response();
    }
    setup.live = Some(LiveCode {
        code,
        expires: now.saturating_add(CODE_TTL_MS),
    });
    Json(json!({ "issued": true })).into_response()
}

/// The setup code for a terminal: the code that is live now, or a new one, in the answer itself. It is for a machine with
/// no screen to show a code on, and it is the unix socket's route alone.
///
/// The caller proves it can write the node's state directory: before it asks, it makes an empty file there named
/// [`LOCAL_PROOF_PREFIX`] and 32 hexadecimal digits of its own choosing, and sends those digits. The handler finds that
/// file (a regular file of this process's own user), removes it, and only then answers. The state directory is mode
/// 0700, so the caller is the node's user on this machine. A peer that reaches the socket some other way, such as
/// through a forward from another machine, can make no such file and gets 403; so does a request that carries an
/// `Origin`. Whoever can write the state directory could already write the passkeys file in it, so the route gives
/// that user nothing new.
///
/// No passkey may be trusted yet, as for the page's own request. A new code counts against the same hourly bound and
/// the same lock; the 60-second spacing between requests, which keeps a page from filling a screen with notifications,
/// does not apply to a terminal.
async fn local_setup_code(
    State(state): State<Arc<PageState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if headers.contains_key(header::ORIGIN) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let proof = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|value| {
            value
                .get("proof")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
    let Some(proof) = proof.filter(|proof| {
        proof.len() == LOCAL_PROOF_LEN
            && proof
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if !take_local_proof(&state.state_dir, &proof) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "message": "The caller did not show that it can write this node's state directory" })),
        )
            .into_response();
    }
    // A code for a page that is closed would be typed into whatever answers at the page's address.
    if let Some(why) = state.gate.as_ref().and_then(|gate| gate()) {
        return page_closed(why);
    }
    let now = (state.clock)();
    let mut setup = match state.setup.lock() {
        Ok(setup) => setup,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let trusted = match state.kernel.lock() {
        Ok(kernel) => kernel.owner_passkey_count() > 0,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    if trusted {
        return already_trusted();
    }
    setup.prune(now);
    if let Some(minutes) = setup.locked_minutes(now) {
        return locked(minutes);
    }
    let answer = |code: &str, expires: u64| {
        Json(json!({
            "code": code,
            "expiresInSeconds": expires.saturating_sub(now).div_ceil(1000),
        }))
        .into_response()
    };
    if let Some(live) = setup.live_if_fresh(now) {
        return answer(&live.code, live.expires);
    }
    if setup.requests.len() >= MAX_CODE_REQUESTS {
        setup.lock(now);
        return locked(setup.locked_minutes(now).unwrap_or(1));
    }
    setup.requests.push(now);
    let Ok(code) = fresh_code() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let expires = now.saturating_add(CODE_TTL_MS);
    let reply = answer(&code, expires);
    setup.live = Some(LiveCode { code, expires });
    reply
}

/// How old a terminal request's proof file may be. The command makes it and asks at once.
const LOCAL_PROOF_FRESH: Duration = Duration::from_secs(60);

/// Takes the proof file of one terminal request: `true` when the state directory, which is still a directory of this
/// process's own user at mode 0700, held a regular file of that user under the request's name, made within the last
/// [`LOCAL_PROOF_FRESH`], and the file was removed. A link, a directory, another user's file, a file that is not there,
/// an old file, a file that cannot be removed, and a state directory that was opened up since `serve` checked it at
/// start are all no proof.
fn take_local_proof(state_dir: &Path, proof: &str) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let Ok(uid) = arca_approval::effective_uid() else {
        return false;
    };
    let private = fs::symlink_metadata(state_dir).is_ok_and(|dir| {
        dir.is_dir() && dir.uid() == uid && dir.permissions().mode() & 0o777 == 0o700
    });
    if !private {
        return false;
    }
    let path = state_dir.join(format!("{LOCAL_PROOF_PREFIX}{proof}"));
    let Ok(meta) = fs::symlink_metadata(&path) else {
        return false;
    };
    let fresh = meta
        .modified()
        .ok()
        .and_then(|made| made.elapsed().ok())
        .is_some_and(|age| age <= LOCAL_PROOF_FRESH);
    meta.is_file() && meta.uid() == uid && fresh && fs::remove_file(&path).is_ok()
}

/// Checks a setup code before the page creates a credential, using the same wrong-try budgets as registration. A correct
/// code stays live for the registration that follows; this route never trusts or consumes a credential.
async fn verify_setup_code(
    State(state): State<Arc<PageState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Ok(value) = serde_json::from_slice::<Value>(&body) else {
        return missing_setup_code();
    };
    let Some(setup_code) = value.get("setupCode").and_then(Value::as_str) else {
        return missing_setup_code();
    };
    if setup_code.trim().is_empty() {
        return missing_setup_code();
    }
    let now = (state.clock)();
    let mut setup = match state.setup.lock() {
        Ok(setup) => setup,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let kernel = match state.kernel.lock() {
        Ok(kernel) => kernel,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    if kernel.owner_passkey_count() > 0 {
        return already_trusted();
    }
    setup.prune(now);
    let caller = state.caller(&headers);
    if let Some(minutes) = setup
        .locked_minutes(now)
        .or_else(|| setup.caller_locked_minutes(&caller, now))
    {
        return locked(minutes);
    }
    let correct = setup
        .live_if_fresh(now)
        .is_some_and(|live| setup_code == live.code);
    if !correct {
        if setup.live.is_none() {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "message": "The setup code expired" })),
            )
                .into_response();
        }
        setup.wrong_code(caller.clone(), now);
        if let Some(minutes) = setup
            .locked_minutes(now)
            .or_else(|| setup.caller_locked_minutes(&caller, now))
        {
            return locked(minutes);
        }
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "message": "The setup code is wrong" })),
        )
            .into_response();
    }
    Json(json!({ "verified": true })).into_response()
}

async fn register(
    State(state): State<Arc<PageState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
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
    let now = (state.clock)();
    let label = value
        .get("label")
        .and_then(Value::as_str)
        .unwrap_or("My passkey");
    let line = match passkey_change::with_metadata(&line, label, now) {
        Ok(line) => line,
        Err(error) => return change_refused(error),
    };
    {
        let kernel = match state.kernel.lock() {
            Ok(kernel) => kernel,
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
        if kernel.owner_passkey_count() > 0 {
            if value.get("setupCode").is_some() {
                return already_trusted();
            }
            return match passkey_change::statement(
                kernel.host(),
                ArcaAction::PasskeyAdd,
                &line,
                now,
            ) {
                Ok(statement) => canvas_json(
                    StatusCode::OK,
                    json!({ "line": line, "statement": statement }),
                ),
                Err(error) => change_refused(error),
            };
        }
    }
    let Some(setup_code) = value.get("setupCode").and_then(Value::as_str) else {
        return missing_setup_code();
    };
    if setup_code.trim().is_empty() {
        return missing_setup_code();
    }
    let now = (state.clock)();
    let mut setup = match state.setup.lock() {
        Ok(setup) => setup,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let mut kernel = match state.kernel.lock() {
        Ok(kernel) => kernel,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    if kernel.owner_passkey_count() > 0 {
        return already_trusted();
    }
    setup.prune(now);
    let caller = state.caller(&headers);
    // Refuse locked callers before inspecting the code so the wrong-try budgets bound guessing.
    if let Some(minutes) = setup
        .locked_minutes(now)
        .or_else(|| setup.caller_locked_minutes(&caller, now))
    {
        return locked(minutes);
    }
    let correct = setup
        .live_if_fresh(now)
        .is_some_and(|live| setup_code == live.code);
    if !correct {
        if setup.live.is_none() {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "message": "The setup code expired" })),
            )
                .into_response();
        }
        setup.wrong_code(caller.clone(), now);
        if let Some(minutes) = setup
            .locked_minutes(now)
            .or_else(|| setup.caller_locked_minutes(&caller, now))
        {
            return locked(minutes);
        }
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "message": "The setup code is wrong" })),
        )
            .into_response();
    }
    let state_path = state.state_dir.join(STATE_PASSKEYS_FILE);
    let previous = match write_state_line(&state.state_dir, &line) {
        Ok(previous) => previous,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "message": "The passkey could not be saved" })),
            )
                .into_response();
        }
    };
    let state_lines = match load_state_passkeys(&state_path) {
        Ok(lines) => lines,
        Err(_) => {
            let _ = restore_state(&state.state_dir, previous.as_deref());
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
    kernel.replace_owner_passkeys(lines);
    let count = kernel.owner_passkey_count();
    if count == 0 {
        let _ = restore_state(&state.state_dir, previous.as_deref());
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "message": "The passkey could not be trusted" })),
        )
            .into_response();
    }
    setup.live = None;
    (state.log)(&format!("arca: passkey trusted, {count} passkey(s) kept"));
    Json(json!({ "trusted": true })).into_response()
}

fn missing_setup_code() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "message": "Enter the setup code first" })),
    )
        .into_response()
}

fn already_trusted() -> Response {
    (
        StatusCode::CONFLICT,
        Json(json!({ "message": "A passkey is already trusted" })),
    )
        .into_response()
}

fn locked(minutes: u64) -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        Json(rate_body(
            format!("Setup is locked for {minutes} more minute(s)"),
            Some(minutes.saturating_mul(60)),
        )),
    )
        .into_response()
}

/// The body of a page answer that says the caller asked too often (429): the sentence as `message`, the `rate` refusal
/// code beside it, and `retryAfterSeconds` when the wait is known. The page reads only `message` from these answers.
fn rate_body(message: impl Into<String>, retry_after_seconds: Option<u64>) -> Value {
    let mut body = arca_api::refusals::retry_after(retry_after_seconds);
    body.insert("refusal".to_string(), json!(arca_api::refusals::RATE));
    body.insert("message".to_string(), json!(message.into()));
    Value::Object(body)
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
/// process; the state directory is already 0700. Existing credentials or fingerprints are left unchanged. The returned
/// bytes allow rollback if reloading or trusting fails; `None` means the file did not exist.
fn write_state_line(dir: &Path, line: &str) -> std::io::Result<Option<Vec<u8>>> {
    let path = dir.join(STATE_PASSKEYS_FILE);
    let previous = match fs::read(&path) {
        Ok(body) => Some(body),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let invalid = || std::io::Error::new(std::io::ErrorKind::InvalidData, "Invalid passkeys file");
    let existing =
        parse_passkeys(previous.as_deref().unwrap_or_default()).map_err(|_| invalid())?;
    let incoming = parse_passkeys(line.as_bytes()).map_err(|_| invalid())?;
    let incoming = incoming.first().ok_or_else(invalid)?;
    if existing.iter().any(|seen| {
        seen.credential_id == incoming.credential_id || seen.fingerprint == incoming.fingerprint
    }) {
        return Ok(previous);
    }
    let mut body = previous.clone().unwrap_or_default();
    if !body.is_empty() && !body.ends_with(b"\n") {
        body.push(b'\n');
    }
    body.extend_from_slice(line.as_bytes());
    body.push(b'\n');
    write_state_bytes(dir, &body)?;
    Ok(previous)
}

fn restore_state(dir: &Path, previous: Option<&[u8]>) -> std::io::Result<()> {
    match previous {
        Some(body) => write_state_bytes(dir, body),
        None => fs::remove_file(dir.join(STATE_PASSKEYS_FILE)).or_else(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                Ok(())
            } else {
                Err(error)
            }
        }),
    }
}

#[cfg(test)]
#[path = "../../arca-approval/tests/common/webauthn.rs"]
mod test_passkey;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_line_append_is_idempotent_by_credential_or_fingerprint() {
        let dir = std::env::temp_dir().join(format!(
            "arca-page-write-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        fs::create_dir(&dir).expect("test directory");
        let first = super::test_passkey::SoftPasskey::from_seed(1);
        let second = super::test_passkey::SoftPasskey::from_seed(2);
        let line = first.line(arca_approval::ARCA_NAMESPACE);
        assert!(
            write_state_line(&dir, &line)
                .expect("first write")
                .is_none()
        );
        let original = fs::read(dir.join(STATE_PASSKEYS_FILE)).expect("original bytes");
        let same_credential = format_passkey_line(
            super::test_passkey::RP,
            super::test_passkey::ORIGIN,
            &first.cred_text(),
            &second.key_text(),
        )
        .expect("same credential");
        let same_key = format_passkey_line(
            super::test_passkey::RP,
            super::test_passkey::ORIGIN,
            &second.cred_text(),
            &first.key_text(),
        )
        .expect("same fingerprint");
        for duplicate in [&line, &same_credential, &same_key] {
            assert!(
                write_state_line(&dir, duplicate)
                    .expect("idempotent write")
                    .as_deref()
                    == Some(original.as_slice())
            );
            let kept = fs::read(dir.join(STATE_PASSKEYS_FILE)).expect("kept bytes");
            assert!(kept == original);
            assert_eq!(parse_passkeys(&kept).expect("parseable file").len(), 1);
        }
        restore_state(&dir, None).expect("remove newly created file");
        assert!(!dir.join(STATE_PASSKEYS_FILE).exists());
        restore_state(&dir, Some(&original)).expect("restore existing file");
        assert!(fs::read(dir.join(STATE_PASSKEYS_FILE)).expect("restored bytes") == original);
        fs::remove_dir_all(dir).expect("remove test directory");
    }

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
        let mut setup = SetupState {
            live: Some(LiveCode {
                code: "12345678".to_string(),
                expires: now() + CODE_TTL_MS,
            }),
            ..Default::default()
        };
        assert!(setup.live_if_fresh(now()).is_some());
        setup.lock(now());
        assert!(
            setup.live_if_fresh(now()).is_some(),
            "a lock preserves the live code"
        );
        assert!(setup.live_if_fresh(now() + CODE_TTL_MS).is_none());
        assert!(setup.live.is_none());
    }
}
