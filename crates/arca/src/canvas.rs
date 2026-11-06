//! The remote canvas behind the approval passkey: the four exact `/approve/canvas` URLs, the sign-in statement store, the
//! 12-hour session cookie, the DSH proxy and the response headers that keep the shared approval origin first-party only.
//!
//! The remote switch never writes. A sign-in is one challenge lookup, a challenge spend before any signature check, then the
//! existing `PasskeyVerifier`; the session only admits the four canvas URLs. No caller address is read: every tailnet caller
//! reaches the reverse proxy from one address, so a caller bucket would be one shared bucket.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use getrandom::fill;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use arca_kernel::canon::canonical_json;

use crate::config::canvas_origin_addr;

/// The exact remote URL and the DSH path each of its four assets maps to.
pub const PAGE_PATH: &str = "/approve/canvas";
pub const CSS_PATH: &str = "/approve/canvas/page.css";
pub const JS_PATH: &str = "/approve/canvas/page.js";
pub const SNAPSHOT_PATH: &str = "/approve/canvas/snapshot";
pub const DSH_CSS_PATH: &str = "/arca-canvas/page.css";
pub const DSH_JS_PATH: &str = "/arca-canvas/page.js";
pub const DSH_SNAPSHOT_PATH: &str = "/arca-canvas/snapshot";

/// The one cookie the four canvas URLs read. `__Host-` requires Secure, Path=/ and no Domain.
pub const COOKIE: &str = "__Host-arca-canvas";
/// A session lives twelve hours.
pub const SESSION_TTL_MS: u64 = 12 * 60 * 60 * 1000;
/// A sign-in statement lives two minutes.
pub const CHALLENGE_TTL_MS: u64 = 2 * 60 * 1000;
/// The challenge store is fixed; eviction is a memory backstop.
const CHALLENGE_MAX: usize = 256;
/// The one global challenge cap; no per-caller bucket exists.
const CHALLENGE_PER_MINUTE: usize = 60;
/// A DSH connect or read lasts at most three seconds.
const DSH_TIMEOUT: Duration = Duration::from_secs(3);
/// A DSH body is bounded, so a broken upstream cannot grow the server's memory.
const DSH_BODY_MAX: usize = 2 * 1024 * 1024;
/// The sign-in statement's kind. The approval parser refuses it structurally, as this parser refuses an approval statement.
pub const STATEMENT_KIND: &str = "arca.canvas.view.v1";

/// The remote page. Its assets are relative to `/approve/canvas`; the page script reads the data attribute for its base.
pub const CANVAS_PAGE: &str = include_str!("canvas_page.html");
/// The page a signed-out caller sees; its script is the only inline script in Group 6.
pub const SIGN_IN_PAGE: &str = include_str!("canvas_signin.html");
/// The page a hung, down or non-200 DSH answers with.
pub const ERROR_PAGE: &str = include_str!("canvas_error.html");

/// A sign-in statement after the structural and time checks. No field is printed in a refusal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignInStatement {
    pub challenge: String,
    pub issued_ms: u64,
    pub expires_ms: u64,
}

/// The remote canvas state: its approval origin, its DSH loopback address, the two in-memory stores.
#[derive(Clone)]
pub struct Canvas {
    origin: String,
    addr: SocketAddr,
    challenges: Arc<Mutex<Challenges>>,
    sessions: Arc<Mutex<Sessions>>,
}

#[derive(Default)]
struct Challenges {
    map: HashMap<String, Challenge>,
    order: VecDeque<String>,
    issued: VecDeque<u64>,
}

struct Challenge {
    statement: String,
    expires: u64,
}

#[derive(Default)]
struct Sessions {
    map: HashMap<String, Session>,
}

struct Session {
    credential: String,
    expires: u64,
}

impl Canvas {
    /// Binds the approval origin and a literal loopback DSH origin. The caller has already validated both.
    pub fn new(origin: String, canvas_origin: &str) -> Option<Self> {
        let addr = canvas_origin_addr(canvas_origin)?;
        Some(Self {
            origin,
            addr,
            challenges: Arc::new(Mutex::new(Challenges::default())),
            sessions: Arc::new(Mutex::new(Sessions::default())),
        })
    }

    /// The approval origin a sign-in statement names.
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// Issues one canonical sign-in statement, or `None` when the one global minute cap is full.
    pub fn issue_challenge(&self, now: u64) -> Option<String> {
        let mut store = self.challenges.lock().ok()?;
        store.prune(now);
        if store.issued.len() >= CHALLENGE_PER_MINUTE {
            return None;
        }
        let challenge = random_hex()?;
        let statement = statement_for(&self.origin, &challenge, now)?;
        store.issued.push_back(now);
        store.insert(
            challenge,
            Challenge {
                statement: statement.clone(),
                expires: now.saturating_add(CHALLENGE_TTL_MS),
            },
        );
        Some(statement)
    }

    /// Parses a sign-in statement. It refuses an approval statement by its exact key set and kind, and refuses a statement
    /// whose origin, challenge, times or TTL are not ours.
    pub fn parse_statement(&self, statement: &str, now: u64) -> Option<SignInStatement> {
        let value: Value = serde_json::from_str(statement).ok()?;
        if canonical_json(&value).ok().as_deref() != Some(statement) {
            return None;
        }
        let map = value.as_object()?;
        const KEYS: [&str; 5] = ["challenge", "expiresMs", "issuedMs", "kind", "origin"];
        if map.len() != KEYS.len() || !KEYS.iter().all(|key| map.contains_key(*key)) {
            return None;
        }
        if map.get("kind")?.as_str()? != STATEMENT_KIND {
            return None;
        }
        if map.get("origin")?.as_str()? != self.origin {
            return None;
        }
        let challenge = map.get("challenge")?.as_str()?;
        if !is_hex(challenge, 64) {
            return None;
        }
        let issued_ms = map.get("issuedMs")?.as_u64()?;
        let expires_ms = map.get("expiresMs")?.as_u64()?;
        let ttl = expires_ms.checked_sub(issued_ms)?;
        if ttl == 0
            || ttl > CHALLENGE_TTL_MS
            || expires_ms <= now
            || issued_ms > now.saturating_add(300_000)
        {
            return None;
        }
        Some(SignInStatement {
            challenge: challenge.to_string(),
            issued_ms,
            expires_ms,
        })
    }

    /// Spends the exact live challenge before a verifier is asked. A missing, expired, already-spent or different statement
    /// returns false and is not verified.
    pub fn spend_challenge(&self, parsed: &SignInStatement, statement: &str, now: u64) -> bool {
        let Ok(mut store) = self.challenges.lock() else {
            return false;
        };
        store.spend(&parsed.challenge, statement, now)
    }

    /// Issues a session cookie bound to the verified credential and expiring in twelve hours.
    pub fn issue_session(&self, credential: &str, now: u64) -> Option<String> {
        let id = random_hex()?;
        let Ok(mut store) = self.sessions.lock() else {
            return None;
        };
        store.map.insert(
            id.clone(),
            Session {
                credential: credential.to_string(),
                expires: now.saturating_add(SESSION_TTL_MS),
            },
        );
        Some(format!(
            "{COOKIE}={id}; Path=/; Secure; HttpOnly; SameSite=Strict; Max-Age=43200"
        ))
    }

    /// The credential a valid, unexpired canvas cookie names. Only the four canvas handlers call this.
    pub fn session(&self, headers: &axum::http::HeaderMap, now: u64) -> Option<String> {
        let id = cookie_id(headers)?;
        let Ok(mut store) = self.sessions.lock() else {
            return None;
        };
        let session = store.map.get(&id)?;
        if session.expires <= now {
            store.map.remove(&id);
            return None;
        }
        Some(session.credential.clone())
    }

    /// Fetches one fixed DSH asset over loopback, without cookies or auth headers. Only a 200 body passes; every other outcome
    /// is the caller's Arca-styled error page.
    pub async fn proxy(&self, path: &str) -> Option<Vec<u8>> {
        let mut stream = timeout(DSH_TIMEOUT, TcpStream::connect(self.addr))
            .await
            .ok()?
            .ok()?;
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            self.addr
        );
        let raw = timeout(DSH_TIMEOUT, async {
            stream.write_all(request.as_bytes()).await.ok()?;
            let mut raw = Vec::new();
            let mut limited = (&mut stream).take((DSH_BODY_MAX + 64 * 1024) as u64);
            limited.read_to_end(&mut raw).await.ok()?;
            Some(raw)
        })
        .await
        .ok()??;
        parse_upstream(&raw).ok()
    }
}

impl Challenges {
    fn prune(&mut self, now: u64) {
        self.issued.retain(|at| now.saturating_sub(*at) < 60_000);
        self.map.retain(|_, challenge| challenge.expires > now);
    }

    fn insert(&mut self, challenge: String, value: Challenge) {
        while self.map.len() >= CHALLENGE_MAX {
            match self.order.pop_front() {
                Some(old) => {
                    self.map.remove(&old);
                }
                None => break,
            }
        }
        self.order.push_back(challenge.clone());
        self.map.insert(challenge, value);
    }

    fn spend(&mut self, challenge: &str, statement: &str, now: u64) -> bool {
        let Some(live) = self.map.remove(challenge) else {
            return false;
        };
        live.expires > now && live.statement == statement
    }
}

/// The canonical sign-in statement: sorted keys, no whitespace, exactly the five fields.
fn statement_for(origin: &str, challenge: &str, now: u64) -> Option<String> {
    canonical_json(&json!({
        "kind": STATEMENT_KIND,
        "origin": origin,
        "challenge": challenge,
        "issuedMs": now,
        "expiresMs": now.saturating_add(CHALLENGE_TTL_MS),
    }))
    .ok()
}

fn random_hex() -> Option<String> {
    let mut bytes = [0u8; 32];
    fill(&mut bytes).ok()?;
    Some(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn is_hex(text: &str, len: usize) -> bool {
    text.len() == len
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn cookie_id(headers: &axum::http::HeaderMap) -> Option<String> {
    for value in headers.get_all(axum::http::header::COOKIE) {
        let Ok(value) = value.to_str() else {
            continue;
        };
        for part in value.split(';') {
            let Some(id) = part.trim().strip_prefix(&format!("{COOKIE}=")) else {
                continue;
            };
            if is_hex(id, 64) {
                return Some(id.to_string());
            }
        }
    }
    None
}

/// The CSP for every `/approve` response. The inline approval script/style, sign-in script/style and error style are hashed;
/// no inline source is allowed without its hash.
static CSP: LazyLock<String> = LazyLock::new(|| {
    let mut scripts = vec!["'self'".to_string()];
    for (html, tag) in [
        (include_str!("approve_page.html"), "script"),
        (SIGN_IN_PAGE, "script"),
    ] {
        if let Some(hash) = inline_hash(html, tag) {
            scripts.push(hash);
        }
    }
    let mut styles = vec!["'self'".to_string()];
    for (html, tag) in [
        (include_str!("approve_page.html"), "style"),
        (SIGN_IN_PAGE, "style"),
        (ERROR_PAGE, "style"),
    ] {
        if let Some(hash) = inline_hash(html, tag) {
            styles.push(hash);
        }
    }
    format!(
        "default-src 'none'; connect-src 'self'; img-src 'self' data:; frame-ancestors 'none'; \
         base-uri 'none'; object-src 'none'; form-action 'self'; script-src {}; style-src {}",
        scripts.join(" "),
        styles.join(" ")
    )
});

pub fn csp() -> &'static str {
    CSP.as_str()
}

fn inline_hash(html: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = html.find(&open)? + open.len();
    let end = start + html[start..].find(&close)?;
    let digest = Sha256::digest(&html.as_bytes()[start..end]);
    Some(format!("'sha256-{}'", STANDARD.encode(digest)))
}

/// Parses one HTTP/1.1 response from DSH. Only a 200 passes, and the body is bounded.
fn parse_upstream(raw: &[u8]) -> Result<Vec<u8>, ()> {
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or(())?;
    let head = std::str::from_utf8(&raw[..split]).map_err(|_| ())?;
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|status| status.parse::<u16>().ok())
        .ok_or(())?;
    if status != 200 {
        return Err(());
    }
    let mut content_length = None;
    let mut chunked = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.parse::<usize>().ok();
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = value
                .split(',')
                .any(|part| part.trim().eq_ignore_ascii_case("chunked"));
        }
    }
    let body = &raw[split + 4..];
    let body = if chunked {
        decode_chunked(body)?
    } else if let Some(length) = content_length {
        body.get(..length).ok_or(())?.to_vec()
    } else {
        body.to_vec()
    };
    if body.len() > DSH_BODY_MAX {
        return Err(());
    }
    Ok(body)
}

/// Decodes a chunked body without trusting any size prefix. A missing terminator or an overlong body is refused.
fn decode_chunked(mut body: &[u8]) -> Result<Vec<u8>, ()> {
    let mut out = Vec::new();
    loop {
        let line_end = body
            .windows(2)
            .position(|window| window == b"\r\n")
            .ok_or(())?;
        let size =
            usize::from_str_radix(std::str::from_utf8(&body[..line_end]).map_err(|_| ())?, 16)
                .map_err(|_| ())?;
        body = &body[line_end + 2..];
        if size == 0 {
            return Ok(out);
        }
        if size > DSH_BODY_MAX || out.len().saturating_add(size) > DSH_BODY_MAX {
            return Err(());
        }
        let chunk = body.get(..size).ok_or(())?;
        out.extend_from_slice(chunk);
        body = body.get(size + 2..).ok_or(())?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_in_statement_is_canonical_and_single_use() {
        let canvas = Canvas::new(
            "https://box1.example.com:8443".to_string(),
            "http://127.0.0.1:3081",
        )
        .expect("a loopback canvas");
        let statement = canvas.issue_challenge(1_000).expect("a challenge");
        assert_eq!(
            statement,
            canonical_json(&serde_json::from_str::<Value>(&statement).expect("JSON"))
                .expect("canonical")
        );
        let parsed = canvas
            .parse_statement(&statement, 1_000)
            .expect("our statement");
        assert!(canvas.spend_challenge(&parsed, &statement, 1_000));
        assert!(!canvas.spend_challenge(&parsed, &statement, 1_000));
        assert!(canvas.parse_statement(&statement, 1_000).is_some());
        assert!(!canvas.spend_challenge(&parsed, &statement, 1_002));
    }

    #[test]
    fn the_sign_in_store_is_fixed_and_evicts_the_oldest() {
        let canvas = Canvas::new(
            "https://box1.example.com".to_string(),
            "http://127.0.0.1:3081",
        )
        .expect("a loopback canvas");
        let mut store = canvas.challenges.lock().expect("store");
        for index in 0..(CHALLENGE_MAX + 5) {
            store.insert(
                format!("{index:064x}"),
                Challenge {
                    statement: String::new(),
                    expires: u64::MAX,
                },
            );
        }
        assert_eq!(store.map.len(), CHALLENGE_MAX);
        assert!(!store.map.contains_key(&format!("{:064x}", 0)));
        assert!(
            store
                .map
                .contains_key(&format!("{:064x}", CHALLENGE_MAX + 4))
        );
    }

    #[test]
    fn sessions_are_bound_to_credentials_and_expire() {
        let canvas = Canvas::new(
            "https://box1.example.com".to_string(),
            "http://127.0.0.1:3081",
        )
        .expect("a loopback canvas");
        let cookie = canvas.issue_session("passkey:test", 10).expect("a session");
        assert!(cookie.starts_with("__Host-arca-canvas="));
        assert!(cookie.contains("Path=/"));
        assert!(cookie.contains("Secure"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Strict"));
        assert!(cookie.contains("Max-Age=43200"));
        assert!(!cookie.contains("Domain="));
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::COOKIE,
            cookie.split(';').next().unwrap().parse().unwrap(),
        );
        assert_eq!(
            canvas.session(&headers, 10),
            Some("passkey:test".to_string())
        );
        assert_eq!(canvas.session(&headers, 10 + SESSION_TTL_MS), None);
    }

    #[test]
    fn csp_hashes_every_inline_source_and_forbids_unsafe_inline() {
        let policy = csp();
        assert!(!policy.contains("unsafe-inline"));
        for (html, tag) in [
            (include_str!("approve_page.html"), "script"),
            (include_str!("approve_page.html"), "style"),
            (SIGN_IN_PAGE, "script"),
            (SIGN_IN_PAGE, "style"),
            (ERROR_PAGE, "style"),
        ] {
            let hash = inline_hash(html, tag).expect("an inline source");
            assert!(policy.contains(&hash), "{tag} hash is in the CSP");
        }
        assert!(!include_str!("approve_page.html").contains("style=\""));
    }
}
