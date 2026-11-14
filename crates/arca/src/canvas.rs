//! The remote canvas behind the approval passkey: the exact `/approve/canvas` URLs, the sign-in statement store, the
//! 12-hour session cookie, the DSH proxy and the response headers that keep the shared approval origin first-party only.
//!
//! The remote switch never writes. A sign-in is one challenge lookup, a challenge spend before any signature check, then the
//! existing `PasskeyVerifier`; the session only admits the canvas URLs. No caller address is read: every tailnet caller
//! reaches the reverse proxy from one address, so a caller bucket would be one shared bucket.

use std::collections::{HashMap, VecDeque};
use std::io;
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
use tokio::sync::mpsc;
use tokio::time::{Instant, timeout, timeout_at};

use arca_kernel::canon::canonical_json;

use crate::config::canvas_origin_addr;

/// The exact remote URL and the DSH path each of its assets maps to.
pub const PAGE_PATH: &str = "/approve/canvas";
pub const CSS_PATH: &str = "/approve/canvas/page.css";
pub const JS_PATH: &str = "/approve/canvas/page.js";
pub const SNAPSHOT_PATH: &str = "/approve/canvas/snapshot";
pub const DSH_CSS_PATH: &str = "/arca-canvas/page.css";
pub const DSH_JS_PATH: &str = "/arca-canvas/page.js";
pub const DSH_SNAPSHOT_PATH: &str = "/arca-canvas/snapshot";
/// The one chat turn URL and its DSH path. The response is relayed as a stream.
pub const CHAT_PATH: &str = "/approve/canvas/chat";
pub const DSH_CHAT_PATH: &str = "/arca-canvas/chat";
/// The chat history URL and its DSH path. It is a plain, bounded forward like the assets.
pub const CHAT_HISTORY_PATH: &str = "/approve/canvas/chat/history";
pub const DSH_CHAT_HISTORY_PATH: &str = "/arca-canvas/chat/history";

/// The one cookie the canvas URLs read. `__Host-` requires Secure, Path=/ and no Domain.
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
/// One chat turn is bounded; the page request cap is the same 64 KiB.
pub const CHAT_BODY_MAX: usize = 64 * 1024;
/// A chat stream ends after this much silence between chunks.
pub const CHAT_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// A chat stream lasts at most ten minutes, even while it keeps trickling.
pub const CHAT_TOTAL_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// A chat response relays at most two MiB, so a broken upstream cannot stream without bound.
pub const CHAT_STREAM_MAX: usize = 2 * 1024 * 1024;
/// The data chunks the relay may buffer for a slow client. One of the channel's permits is reserved at spawn for the
/// final cut event, so data sends keep normal backpressure and the cut never waits for room.
const RELAY_CHANNEL: usize = 16;
/// At most two chat streams hold a listener connection; a third gets 429 before any upstream connect.
pub const CHAT_CONCURRENCY: usize = 2;
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

/// The remote canvas state: its approval origin, its DSH loopback address, the two in-memory stores and the chat stream caps.
#[derive(Clone)]
pub struct Canvas {
    origin: String,
    addr: SocketAddr,
    challenges: Arc<Mutex<Challenges>>,
    sessions: Arc<Mutex<Sessions>>,
    chat_idle: Duration,
    chat_total: Duration,
}

/// A started chat stream: the upstream's content type and the chunks as they arrive. The receiver closes when the upstream
/// ends, the idle or total cap fires, or the client disconnects.
pub(crate) struct ChatResponse {
    pub(crate) content_type: String,
    pub(crate) body: mpsc::Receiver<Result<Vec<u8>, io::Error>>,
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
            chat_idle: CHAT_IDLE_TIMEOUT,
            chat_total: CHAT_TOTAL_TIMEOUT,
        })
    }

    /// Replaces the chat stream caps. Tests use short values; `serve` keeps the sixty-second idle and ten-minute total caps.
    pub(crate) fn with_chat_timeouts(mut self, idle: Duration, total: Duration) -> Self {
        self.chat_idle = idle;
        self.chat_total = total;
        self
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

    /// The credential a valid, unexpired canvas cookie names. Only the canvas handlers call this.
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

    /// Starts one DSH chat turn and relays its body without buffering it. Only a 200 with a streamed content type starts a
    /// stream; the caller has already checked the session, the Origin and the JSON body. Dropping the response receiver aborts
    /// the upstream connection. One deadline, taken here, covers the connect, the write, the response head and the relay.
    pub(crate) async fn chat(&self, body: &[u8]) -> Option<ChatResponse> {
        let deadline = Instant::now() + self.chat_total;
        let connect_wait = deadline
            .saturating_duration_since(Instant::now())
            .min(DSH_TIMEOUT);
        if connect_wait.is_zero() {
            return None;
        }
        let mut stream = timeout(connect_wait, TcpStream::connect(self.addr))
            .await
            .ok()?
            .ok()?;
        let request = format!(
            "POST {DSH_CHAT_PATH} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nAccept: text/event-stream\r\nConnection: close\r\n\r\n",
            self.addr,
            body.len()
        );
        let written = timeout_at(deadline, async {
            stream.write_all(request.as_bytes()).await.ok()?;
            stream.write_all(body).await.ok()?;
            Some(())
        })
        .await;
        if !matches!(written, Ok(Some(()))) {
            return None;
        }
        let (head, leftover) = read_chat_head(&mut stream, deadline, self.chat_idle).await?;
        let sse = is_sse(&head.content_type);
        Some(ChatResponse {
            content_type: head.content_type,
            body: spawn_chat_relay(
                stream,
                leftover,
                head.chunked,
                head.content_length,
                deadline,
                self.chat_idle,
                sse,
            ),
        })
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
        (include_str!("github_page.html"), "style"),
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

/// The parsed head of one streamed DSH chat response.
struct ChatHead {
    content_type: String,
    chunked: bool,
    content_length: Option<usize>,
}

/// Reads the chat response head, bounded, leaving any body bytes already received for the relay. Every read shares the one
/// deadline taken in [`Canvas::chat`], so a slowly dripped head cannot extend the stream's total.
async fn read_chat_head(
    stream: &mut TcpStream,
    deadline: Instant,
    idle: Duration,
) -> Option<(ChatHead, Vec<u8>)> {
    let mut raw = Vec::new();
    let mut buffer = [0u8; 8 * 1024];
    loop {
        if let Some(split) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = parse_chat_head(&raw[..split])?;
            return Some((head, raw[split + 4..].to_vec()));
        }
        if raw.len() >= 64 * 1024 {
            return None;
        }
        let wait = deadline.saturating_duration_since(Instant::now()).min(idle);
        if wait.is_zero() {
            return None;
        }
        let read = timeout(wait, stream.read(&mut buffer)).await.ok()?.ok()?;
        if read == 0 {
            return None;
        }
        raw.extend_from_slice(&buffer[..read]);
    }
}

/// Only a 200 with a streamed content type starts a chat stream. Upstream hops, cookies and cache headers are dropped.
fn parse_chat_head(head: &[u8]) -> Option<ChatHead> {
    let head = std::str::from_utf8(head).ok()?;
    let mut lines = head.split("\r\n");
    let status = lines
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse::<u16>()
        .ok()?;
    if status != 200 {
        return None;
    }
    let mut content_type = None;
    let mut chunked = false;
    let mut content_length = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-type") {
            content_type = Some(chat_content_type(value)?);
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = value
                .split(',')
                .any(|part| part.trim().eq_ignore_ascii_case("chunked"));
        } else if name.eq_ignore_ascii_case("content-length") {
            content_length = value.parse::<usize>().ok();
        }
    }
    Some(ChatHead {
        content_type: content_type?,
        chunked,
        content_length,
    })
}

/// The content type the relay preserves. Every other type is refused rather than sniffed past the `nosniff` header.
fn chat_content_type(value: &str) -> Option<String> {
    let media = value.split(';').next()?.trim();
    if media.eq_ignore_ascii_case("text/event-stream")
        || media.eq_ignore_ascii_case("application/x-ndjson")
        || media.eq_ignore_ascii_case("application/json")
    {
        Some(value.to_string())
    } else {
        None
    }
}

fn is_sse(content_type: &str) -> bool {
    content_type
        .split(';')
        .next()
        .is_some_and(|media| media.trim().eq_ignore_ascii_case("text/event-stream"))
}

/// The final event a cut-off answer carries, matching the stream's media type. Only the fixed reason reaches the client.
/// It starts with a blank line, so a cut that lands in the middle of an event terminates that partial event first: SSE
/// ends a block with two newlines, and NDJSON ends a line with one.
fn cut_event(sse: bool, reason: &str) -> Vec<u8> {
    let data = json!({ "reason": reason });
    if sse {
        format!("\n\nevent: arca-cut\ndata: {data}\n\n").into_bytes()
    } else {
        format!("\n{{\"arcaCut\":{data}}}\n").into_bytes()
    }
}

/// The channel item the relay carries: one body chunk or its final cut event.
type RelayChunk = Result<Vec<u8>, io::Error>;

/// Emits the cut event over the permit reserved at spawn, so it is delivered even when every other permit is queued
/// behind a slow client. No send has to wait for room, and the body closes straight after it.
fn cut(permit: &mut Option<mpsc::OwnedPermit<RelayChunk>>, sse: bool, reason: &str) {
    if let Some(permit) = permit.take() {
        let _ = permit.send(Ok(cut_event(sse, reason)));
    }
}

/// Relays the DSH body as it arrives: each chunk is sent over the channel immediately, so nothing is buffered whole. One
/// deadline covers the whole stream, while the idle cap limits the wait for each next chunk. Every non-normal ending emits
/// a cut event, and dropping the receiver (a client disconnect) closes the upstream socket.
fn spawn_chat_relay(
    stream: TcpStream,
    initial: Vec<u8>,
    chunked: bool,
    content_length: Option<usize>,
    deadline: Instant,
    idle: Duration,
    sse: bool,
) -> mpsc::Receiver<Result<Vec<u8>, io::Error>> {
    let (sender, receiver) = mpsc::channel(RELAY_CHANNEL);
    tokio::spawn(chat_relay(
        sender,
        stream,
        initial,
        chunked,
        content_length,
        deadline,
        idle,
        sse,
    ));
    receiver
}

/// The same relay, with its task handle. A test waits for the task before it drains the channel, so a full queue is still
/// full when the cut is sent and the reserved permit is the only room it can use.
#[cfg(test)]
fn spawn_chat_relay_observed(
    stream: TcpStream,
    initial: Vec<u8>,
    chunked: bool,
    content_length: Option<usize>,
    deadline: Instant,
    idle: Duration,
    sse: bool,
) -> (
    mpsc::Receiver<Result<Vec<u8>, io::Error>>,
    tokio::task::JoinHandle<()>,
) {
    let (sender, receiver) = mpsc::channel(RELAY_CHANNEL);
    let task = tokio::spawn(chat_relay(
        sender,
        stream,
        initial,
        chunked,
        content_length,
        deadline,
        idle,
        sse,
    ));
    (receiver, task)
}

#[allow(clippy::too_many_arguments)]
async fn chat_relay(
    sender: mpsc::Sender<RelayChunk>,
    mut stream: TcpStream,
    initial: Vec<u8>,
    chunked: bool,
    content_length: Option<usize>,
    deadline: Instant,
    idle: Duration,
    sse: bool,
) {
    let cut_sender = sender.clone();
    // The permit reserved at spawn keeps room for the cut even when a slow client fills the queue. A receiver that
    // is already gone refuses it; the relay then has nothing to deliver and ends without a panic.
    let Ok(cut_permit) = cut_sender.reserve_owned().await else {
        return;
    };
    let mut cut_permit = Some(cut_permit);
    let mut pending = initial;
    let mut decoder = chunked.then(ChunkedBody::new);
    let mut relayed = 0usize;
    let mut received = 0usize;
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let data = std::mem::take(&mut pending);
        received = received.saturating_add(data.len());
        let pieces = match decoder.as_mut() {
            Some(decoder) => match decoder.push(&data) {
                Ok(pieces) => pieces,
                Err(()) => {
                    cut(&mut cut_permit, sse, "upstream");
                    return;
                }
            },
            None if data.is_empty() => Vec::new(),
            None => vec![data],
        };
        for piece in pieces {
            if piece.is_empty() {
                continue;
            }
            if relayed.saturating_add(piece.len()) > CHAT_STREAM_MAX {
                cut(&mut cut_permit, sse, "cap");
                return;
            }
            relayed = relayed.saturating_add(piece.len());
            match timeout_at(deadline, sender.send(Ok(piece))).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => return,
                Err(_) => {
                    // A full queue at the total deadline is the one "full" cut. Until then a slow client applies
                    // normal backpressure, and no briefly full queue ends the answer.
                    let reason = if sender.capacity() == 0 {
                        "full"
                    } else {
                        "total"
                    };
                    cut(&mut cut_permit, sse, reason);
                    return;
                }
            }
        }
        if decoder.as_ref().is_some_and(|decoder| decoder.done) {
            return;
        }
        let now = Instant::now();
        if now >= deadline {
            cut(&mut cut_permit, sse, "total");
            return;
        }
        let wait = deadline.saturating_duration_since(now).min(idle);
        let read = tokio::select! {
            _ = sender.closed() => return,
            read = timeout(wait, stream.read(&mut buffer)) => read,
        };
        match read {
            Ok(Ok(0)) => {
                let early = match decoder.as_ref() {
                    Some(decoder) => !decoder.done,
                    None => content_length.is_some_and(|length| received < length),
                };
                if early {
                    cut(&mut cut_permit, sse, "upstream");
                }
                return;
            }
            Ok(Err(_)) => {
                cut(&mut cut_permit, sse, "upstream");
                return;
            }
            Err(_) => {
                let reason = if Instant::now() >= deadline {
                    "total"
                } else {
                    "idle"
                };
                cut(&mut cut_permit, sse, reason);
                return;
            }
            Ok(Ok(read)) => pending.extend_from_slice(&buffer[..read]),
        }
    }
}

/// One incremental `Transfer-Encoding: chunked` decoder. A chunk is forwarded as soon as its bytes and CRLF are complete.
struct ChunkedBody {
    buffer: Vec<u8>,
    state: ChunkState,
    done: bool,
}

enum ChunkState {
    Size,
    Data(usize),
}

impl ChunkedBody {
    fn new() -> Self {
        Self {
            buffer: Vec::new(),
            state: ChunkState::Size,
            done: false,
        }
    }

    fn push(&mut self, bytes: &[u8]) -> Result<Vec<Vec<u8>>, ()> {
        self.buffer.extend_from_slice(bytes);
        if self.buffer.len() > CHAT_STREAM_MAX + 64 * 1024 {
            return Err(());
        }
        let mut out = Vec::new();
        loop {
            match self.state {
                ChunkState::Size => {
                    let Some(end) = self.buffer.windows(2).position(|window| window == b"\r\n")
                    else {
                        break;
                    };
                    let line = std::str::from_utf8(&self.buffer[..end]).map_err(|_| ())?;
                    let size =
                        usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16)
                            .map_err(|_| ())?;
                    self.buffer.drain(..end + 2);
                    if size == 0 {
                        self.done = true;
                        break;
                    }
                    if size > CHAT_STREAM_MAX {
                        return Err(());
                    }
                    self.state = ChunkState::Data(size);
                }
                ChunkState::Data(size) => {
                    if self.buffer.len() < size + 2 {
                        break;
                    }
                    if &self.buffer[size..size + 2] != b"\r\n" {
                        return Err(());
                    }
                    out.push(self.buffer[..size].to_vec());
                    self.buffer.drain(..size + 2);
                    self.state = ChunkState::Size;
                }
            }
        }
        Ok(out)
    }
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

    #[test]
    fn only_a_200_streamed_chat_head_passes() {
        let parsed = parse_chat_head(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
        )
        .expect("an SSE head");
        assert_eq!(parsed.content_type, "text/event-stream");
        assert!(parsed.chunked);
        let parsed = parse_chat_head(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nContent-Length: 12\r\n\r\n",
        )
        .expect("an NDJSON head");
        assert!(!parsed.chunked);
        assert_eq!(parsed.content_length, Some(12));
        assert!(
            parse_chat_head(b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n").is_none(),
            "another content type is refused"
        );
        assert!(
            parse_chat_head(b"HTTP/1.1 500 Oops\r\nContent-Type: text/event-stream\r\n\r\n")
                .is_none(),
            "a non-200 answer is refused"
        );
        assert!(
            parse_chat_head(b"HTTP/1.1 200 OK\r\n\r\n").is_none(),
            "a missing content type is refused"
        );
    }

    /// The SSE events of a body, as `(event, data)`. A leading blank line, which a cut writes to terminate a partial
    /// event, is an empty block and is skipped.
    fn sse_events(body: &str) -> Vec<(String, String)> {
        let mut events = Vec::new();
        for block in body.split("\n\n") {
            let mut event = "message".to_string();
            let mut data = Vec::new();
            for line in block.lines() {
                if let Some(value) = line.strip_prefix("event: ") {
                    event = value.to_string();
                } else if let Some(value) = line.strip_prefix("data: ") {
                    data.push(value.to_string());
                }
            }
            if !data.is_empty() {
                events.push((event, data.join("\n")));
            }
        }
        events
    }

    #[test]
    fn cut_events_match_the_stream_media_type() {
        assert_eq!(
            cut_event(true, "idle"),
            b"\n\nevent: arca-cut\ndata: {\"reason\":\"idle\"}\n\n"
        );
        assert_eq!(
            cut_event(false, "cap"),
            b"\n{\"arcaCut\":{\"reason\":\"cap\"}}\n"
        );
    }

    #[tokio::test]
    async fn a_full_relay_queue_delivers_the_cut_through_its_reserved_permit() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback listener");
        let addr = listener.local_addr().expect("the listener address");
        let (client, accepted) =
            tokio::join!(tokio::net::TcpStream::connect(addr), listener.accept());
        let _client = client.expect("the client connects");
        let (server, _) = accepted.expect("the server accepts");
        // One chunk more than the channel can buffer once its cut permit is reserved. The body arrives whole, so the
        // relay fills every data permit without reading or waiting.
        let mut initial = Vec::new();
        for _ in 0..=RELAY_CHANNEL {
            initial.extend_from_slice(b"1\r\nx\r\n");
        }
        initial.extend_from_slice(b"0\r\n\r\n");
        // The deadline has already passed: each send that finds room still lands at once, and the first send with no
        // room ends the relay through the reserved permit. The receiver is not drained until the task has ended, so the
        // queue is full when the cut is sent and no sleep or client race decides the outcome.
        let (mut receiver, task) = spawn_chat_relay_observed(
            server,
            initial,
            true,
            None,
            Instant::now(),
            Duration::from_secs(5),
            true,
        );
        task.await.expect("the relay task ends with its cut");
        let mut body = Vec::new();
        while let Some(chunk) = receiver.recv().await {
            body.extend(chunk.expect("a relayed chunk"));
        }
        let text = String::from_utf8(body).expect("UTF-8");
        assert_eq!(
            text.matches('x').count(),
            RELAY_CHANNEL - 1,
            "every data permit was used, so the queue was full when the cut was sent"
        );
        let events = sse_events(&text);
        let (event, data) = events.last().expect("the final event");
        assert_eq!(event, "arca-cut", "the cut survived a full relay queue");
        assert_eq!(
            serde_json::from_str::<Value>(data).expect("cut JSON"),
            json!({ "reason": "full" })
        );
    }

    #[test]
    fn chunked_bodies_decode_across_read_boundaries() {
        let wire = b"d\r\ndata: first\n\n\r\ne\r\ndata: second\n\n\r\n0\r\n\r\n";
        let mut decoder = ChunkedBody::new();
        let mut chunks = Vec::new();
        for part in wire.chunks(3) {
            chunks.extend(decoder.push(part).expect("a valid chunked body"));
        }
        assert!(decoder.done);
        assert_eq!(
            chunks,
            vec![b"data: first\n\n".to_vec(), b"data: second\n\n".to_vec()]
        );
        let mut bad = ChunkedBody::new();
        assert!(bad.push(b"zz\r\n").is_err(), "a bad size is refused");
        let mut bad = ChunkedBody::new();
        assert!(
            bad.push(b"5\r\ndata!\rX").is_err(),
            "a chunk without its CRLF is refused"
        );
    }
}
