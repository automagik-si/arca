//! The approval page: open GETs, exact-Origin POSTs, the same kernel approval path, setup-code enrolment, and registration.
//! The software P-256 passkey is the shared test authenticator; no hardware key is used.

mod common;
#[path = "../../arca-approval/tests/common/webauthn.rs"]
mod webauthn;

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use arca::approval_requests::{self, ApprovalRequests};
use arca::approve_page::{self, SetupCodeDisplay};
use arca::canvas::{
    CHAT_BODY_MAX, CHAT_CONCURRENCY, CHAT_HISTORY_PATH, CHAT_PATH, CHAT_STREAM_MAX, Canvas,
};
use arca::desired_store::EntryChange;
use arca::handlers::{ActionExecutor, Clock, plan_handlers};
use arca::kernel::Kernel;
use arca_api::Scope;
use arca_approval::{
    ArcaAction, PASSKEY_COUNTS_FILE, STATE_PASSKEYS_FILE, load_state_passkeys, parse_passkeys,
    parse_statement,
};
use arca_kernel::canon::canonical_json;
use arca_kernel::desired::EntryId;
use arca_mcp::{CallError, Caller, Server, TokenStore, ToolHandler, serve_unix};
use common::*;
use serde_json::{Value, json};
use tokio::runtime::Runtime;
use webauthn::{Assert, ORIGIN, SoftPasskey, passkey_approval};

/// The desktop display seam replaced in tests: it captures the code, or fails when asked.
#[derive(Default)]
struct FakeDisplay {
    codes: Mutex<Vec<String>>,
    fail: AtomicBool,
}

impl SetupCodeDisplay for FakeDisplay {
    fn show(&self, code: &str) -> Result<(), String> {
        if self.fail.load(Ordering::SeqCst) {
            return Err("No screen to show the code on".to_string());
        }
        self.codes
            .lock()
            .expect("display lock")
            .push(code.to_string());
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum UpstreamMode {
    Ok,
    Error,
    Timeout,
    ChatStream,
    ChatStall,
    ChatHold,
    ChatSilent,
    ChatDrip,
    ChatFlood,
    ChatTruncate,
    ChatCorrupt,
}

struct UpstreamInner {
    mode: Mutex<UpstreamMode>,
    stop: AtomicBool,
    connections: AtomicU64,
    chat_requests: Mutex<Vec<(String, Vec<u8>)>>,
    closed: AtomicBool,
}

/// A loopback fake DSH: one response per request, with upstream headers serve must drop. Each accepted connection runs on its
/// own thread so the concurrency test can hold several chat streams at once. Chat modes stream a chunked SSE body so the proxy
/// tests can observe arrival order, cut events, idle/total expiry and a client disconnect.
struct TestUpstream {
    addr: SocketAddr,
    inner: Arc<UpstreamInner>,
    listener: Option<JoinHandle<()>>,
    workers: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl TestUpstream {
    fn start() -> Arc<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a fake DSH listener");
        listener
            .set_nonblocking(true)
            .expect("a nonblocking fake DSH");
        let addr = listener.local_addr().expect("the fake DSH address");
        let inner = Arc::new(UpstreamInner {
            mode: Mutex::new(UpstreamMode::Ok),
            stop: AtomicBool::new(false),
            connections: AtomicU64::new(0),
            chat_requests: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
        });
        let worker = Arc::clone(&inner);
        let workers = Arc::new(Mutex::new(Vec::new()));
        let spawned = Arc::clone(&workers);
        let thread = std::thread::spawn(move || {
            while !worker.stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        worker.connections.fetch_add(1, Ordering::SeqCst);
                        let connection_worker = Arc::clone(&worker);
                        let handle = std::thread::spawn(move || {
                            handle_connection(stream, &connection_worker)
                        });
                        spawned.lock().expect("workers").push(handle);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Arc::new(Self {
            addr,
            inner,
            listener: Some(thread),
            workers,
        })
    }

    fn set_mode(&self, mode: UpstreamMode) {
        *self.inner.mode.lock().expect("upstream mode") = mode;
    }

    fn connections(&self) -> u64 {
        self.inner.connections.load(Ordering::SeqCst)
    }

    fn chat_requests(&self) -> Vec<(String, Vec<u8>)> {
        self.inner
            .chat_requests
            .lock()
            .expect("chat requests")
            .clone()
    }

    fn closed(&self) -> bool {
        self.inner.closed.load(Ordering::SeqCst)
    }
}

fn handle_connection(mut stream: std::net::TcpStream, worker: &UpstreamInner) {
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let mode = *worker.mode.lock().expect("upstream mode");
    if mode == UpstreamMode::Timeout {
        std::thread::sleep(Duration::from_secs(4));
        return;
    }
    let mut raw = Vec::new();
    let mut chunk = [0u8; 1024];
    while !raw.windows(4).any(|window| window == b"\r\n\r\n") {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => raw.extend_from_slice(&chunk[..read]),
        }
    }
    let head_end = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|split| split + 4)
        .unwrap_or(raw.len());
    let head = String::from_utf8_lossy(&raw[..head_end]).to_string();
    let content_length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    while raw.len() < head_end + content_length {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => raw.extend_from_slice(&chunk[..read]),
        }
    }
    let body = raw
        .get(head_end..head_end + content_length)
        .unwrap_or_default()
        .to_vec();
    let path = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_string();
    if path == "/arca-canvas/chat" {
        worker
            .chat_requests
            .lock()
            .expect("chat requests")
            .push((head.clone(), body));
    }
    match mode {
        UpstreamMode::ChatStream => {
            let _ = stream.write_all(sse_head().as_bytes());
            let _ = write_chunk(&mut stream, b"data: first\n\n");
            std::thread::sleep(Duration::from_millis(400));
            let _ = write_chunk(&mut stream, b"data: second\n\n");
            let _ = write_chunk(&mut stream, b"");
        }
        UpstreamMode::ChatStall => {
            let _ = stream.write_all(sse_head().as_bytes());
            let _ = write_chunk(&mut stream, b"data: first\n\n");
            for _ in 0..30 {
                if worker.stop.load(Ordering::SeqCst) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        UpstreamMode::ChatHold => {
            let _ = stream.write_all(sse_head().as_bytes());
            let _ = write_chunk(&mut stream, b"data: first\n\n");
            for _ in 0..600 {
                if worker.stop.load(Ordering::SeqCst) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
                if write_chunk(&mut stream, b"data: tick\n\n").is_err() {
                    worker.closed.store(true, Ordering::SeqCst);
                    break;
                }
            }
        }
        UpstreamMode::ChatSilent => {
            let _ = stream.write_all(sse_head().as_bytes());
            let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
            let mut buffer = [0u8; 64];
            loop {
                if worker.stop.load(Ordering::SeqCst) {
                    break;
                }
                match stream.read(&mut buffer) {
                    Ok(0) => {
                        worker.closed.store(true, Ordering::SeqCst);
                        break;
                    }
                    Ok(_) => {}
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            || error.kind() == std::io::ErrorKind::TimedOut => {}
                    Err(_) => {
                        worker.closed.store(true, Ordering::SeqCst);
                        break;
                    }
                }
            }
        }
        UpstreamMode::ChatDrip => {
            let head = sse_head().as_bytes();
            let part = head.len().div_ceil(8);
            for piece in head.chunks(part) {
                if stream.write_all(piece).is_err() {
                    return;
                }
                let _ = stream.flush();
                std::thread::sleep(Duration::from_millis(100));
            }
            for index in 0..30 {
                if worker.stop.load(Ordering::SeqCst) {
                    break;
                }
                let event = format!("data: drip{index}\n\n");
                if write_chunk(&mut stream, event.as_bytes()).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        UpstreamMode::ChatFlood => {
            let _ = stream.write_all(sse_head().as_bytes());
            let data = [b'a'; 1024];
            for _ in 0..2300 {
                if worker.stop.load(Ordering::SeqCst) {
                    break;
                }
                if write_chunk(&mut stream, &data).is_err() {
                    break;
                }
            }
        }
        UpstreamMode::ChatTruncate => {
            let _ = stream.write_all(sse_head().as_bytes());
            let _ = write_chunk(&mut stream, b"data: first\n\n");
        }
        UpstreamMode::ChatCorrupt => {
            let _ = stream.write_all(sse_head().as_bytes());
            let _ = stream.write_all(b"zz\r\n");
            let _ = stream.flush();
        }
        _ => {
            let (status, body) = if mode == UpstreamMode::Error {
                ("500 Internal Server Error", "no")
            } else {
                match path.as_str() {
                    "/arca-canvas/page.css" => ("200 OK", "body { color: black; }"),
                    "/arca-canvas/page.js" => ("200 OK", "/* canvas */"),
                    "/arca-canvas/snapshot" => ("200 OK", "{\"instances\":[],\"devices\":\"ok\"}"),
                    "/arca-canvas/chat/history" => ("200 OK", "{\"turns\":[]}"),
                    _ => ("404 Not Found", "missing"),
                }
            };
            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nSet-Cookie: upstream=bad\r\nLocation: /elsewhere\r\nCache-Control: max-age=120\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body.as_bytes());
        }
    }
}

/// The SSE head every chat mode sends, with upstream-only headers the proxy must not relay.
fn sse_head() -> &'static str {
    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nSet-Cookie: upstream=bad\r\nLocation: /elsewhere\r\nCache-Control: max-age=120\r\nConnection: close\r\n\r\n"
}

/// One HTTP/1.1 chunk; an empty body writes the terminating zero chunk.
fn write_chunk(stream: &mut std::net::TcpStream, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    write!(stream, "{:x}\r\n", data.len())?;
    stream.write_all(data)?;
    stream.write_all(b"\r\n")?;
    stream.flush()
}

impl Drop for TestUpstream {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.listener.take() {
            let _ = thread.join();
        }
        let workers = {
            let mut spawned = self.workers.lock().expect("workers");
            std::mem::take(&mut *spawned)
        };
        for worker in workers {
            let _ = worker.join();
        }
    }
}

/// A node with a fake provider and the merged router served on a unix socket. The passkey may start enrolled (Group 1
/// developer registration) or absent (the setup-code flow).
struct Rig {
    _scratch: Scratch,
    fake: arca_incus::FakeIncus,
    kernel: Arc<Mutex<Kernel>>,
    socket: PathBuf,
    proxy_socket: PathBuf,
    logs: Arc<Mutex<Vec<String>>>,
    state_dir: PathBuf,
    clock: Arc<AtomicU64>,
    display: Arc<FakeDisplay>,
    canvas: Canvas,
    upstream: Arc<TestUpstream>,
    action_runs: Arc<AtomicU64>,
    requests: Arc<ApprovalRequests>,
    /// Why the page is closed, while a test says it is: what `serve` reads from the loopback door of a localhost page.
    closed: Arc<Mutex<Option<String>>>,
    _runtime: Runtime,
}

/// The action executor's stand-in for `service_create`: it counts the runs, so the page test can prove exactly one.
struct CountingAction {
    runs: Arc<AtomicU64>,
}

impl ToolHandler for CountingAction {
    fn call(&self, args: Value, _caller: &Caller) -> Result<Value, CallError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        // Three names stand for a create that fails after the approval: one refused in words, one refused for want
        // of room, and one that breaks inside.
        match args["name"].as_str() {
            Some("stuck") => Err(CallError::Refused {
                code: "refused".to_string(),
                message: STUCK.to_string(),
            }),
            Some("full") => Err(CallError::capacity(
                FULL,
                arca_api::refusals::Room::of(
                    arca_api::refusals::Whose::Node,
                    arca_api::refusals::Limit::VirtualMachines,
                    4,
                    1,
                    4,
                ),
            )),
            Some("broken") => Err(CallError::Internal("a detail nobody is shown".to_string())),
            _ => Ok(json!({ "created": true })),
        }
    }
}

/// What the stand-in create answers for the service `stuck`: the words a real create whose start failed uses.
const STUCK: &str = "start service stuck: the instance was created, and Incus did not start it: Incus said \"Failed starting instance\". Its instance exists and is stopped: instance_start starts it once the cause is fixed, and service_remove removes it";

/// What the stand-in create answers for the service `full`: the words of a create over the node's cap.
const FULL: &str = "creating full would exceed this node's cap on virtual machines (kernel.vmLimits.virtualMachines): 4 in use, 1 asked, 4 allowed";

const TEST_CHAT_IDLE: Duration = Duration::from_millis(600);
const TEST_CHAT_TOTAL: Duration = Duration::from_secs(3);

impl Rig {
    fn new(tag: &str, with_page: bool) -> (Rig, SoftPasskey) {
        Self::build(tag, with_page, true)
    }

    fn new_empty(tag: &str) -> (Rig, SoftPasskey) {
        Self::build(tag, true, false)
    }

    fn build(tag: &str, with_page: bool, enrolled: bool) -> (Rig, SoftPasskey) {
        Self::build_full(
            tag,
            with_page,
            enrolled,
            false,
            TEST_CHAT_IDLE,
            TEST_CHAT_TOTAL,
        )
    }

    fn build_with_refusal(
        tag: &str,
        with_page: bool,
        enrolled: bool,
        refusing: bool,
    ) -> (Rig, SoftPasskey) {
        Self::build_full(
            tag,
            with_page,
            enrolled,
            refusing,
            TEST_CHAT_IDLE,
            TEST_CHAT_TOTAL,
        )
    }

    fn with_chat_timeouts(tag: &str, idle: Duration, total: Duration) -> (Rig, SoftPasskey) {
        Self::build_full(tag, true, true, false, idle, total)
    }

    /// The page of a node on its own machine, with no passkey yet: the origin is [`LOCAL_ORIGIN`] and the key that comes
    /// back is a browser's at that origin, for the rp `localhost`.
    fn new_local(tag: &str) -> (Rig, SoftPasskey) {
        Self::build_at(
            LOCAL_ORIGIN,
            tag,
            true,
            false,
            false,
            TEST_CHAT_IDLE,
            TEST_CHAT_TOTAL,
        )
    }

    fn build_full(
        tag: &str,
        with_page: bool,
        enrolled: bool,
        refusing: bool,
        chat_idle: Duration,
        chat_total: Duration,
    ) -> (Rig, SoftPasskey) {
        Self::build_at(
            ORIGIN, tag, with_page, enrolled, refusing, chat_idle, chat_total,
        )
    }

    fn build_at(
        origin: &str,
        tag: &str,
        with_page: bool,
        enrolled: bool,
        refusing: bool,
        chat_idle: Duration,
        chat_total: Duration,
    ) -> (Rig, SoftPasskey) {
        let scratch = Scratch::new(tag);
        let upstream = TestUpstream::start();
        let fake = start_fake(&scratch);
        let mut key = SoftPasskey::from_seed(1);
        if origin == LOCAL_ORIGIN {
            key.rp_id = "localhost".to_string();
            key.origin = origin.to_string();
        }
        let lines = if enrolled {
            format!("{}\n", key.line(NAMESPACE))
        } else {
            String::new()
        };
        let verifier = if refusing {
            arca_approval::ApprovalVerifier::new(
                HOST.to_string(),
                arca_approval::OwnerKeys::new(
                    arca_approval::SshSigVerifier::new(Vec::new()),
                    arca_approval::PasskeyVerifier::refusing(
                        arca_approval::ApprovalRefusal::NoVerifier,
                    ),
                ),
                arca_approval::FileNonces::open(&scratch.path("refusing-nonces.jsonl"))
                    .expect("nonces"),
            )
        } else {
            verifier_with_passkeys(&scratch, &[], &lines, tag)
        };
        let kernel = Arc::new(Mutex::new(
            kernel(&scratch, &fake, verifier, NOW).expect("the kernel opens"),
        ));
        let tokens_dir = scratch.path("tokens");
        fs::create_dir(&tokens_dir).expect("the token directory");
        fs::set_permissions(&tokens_dir, fs::Permissions::from_mode(0o700)).expect("mode 0700");
        let tokens = TokenStore::load(&tokens_dir).expect("an empty token store loads");
        let base = Server::new(arca_api::verbs(), tokens).router();
        let state_dir = scratch.path("state");
        fs::create_dir(&state_dir).expect("the state directory");
        fs::set_permissions(&state_dir, fs::Permissions::from_mode(0o700)).expect("mode 0700");
        let clock = Arc::new(AtomicU64::new(NOW));
        let requests = Arc::new(
            ApprovalRequests::open(&state_dir, Some(format!("{origin}/approve")), NOW)
                .expect("the approval requests open"),
        );
        let display = Arc::new(FakeDisplay::default());
        let action_runs = Arc::new(AtomicU64::new(0));
        let actions = ActionExecutor::new([(
            "service_create",
            Arc::new(CountingAction {
                runs: Arc::clone(&action_runs),
            }) as Arc<dyn ToolHandler>,
        )]);
        let page_clock = Arc::clone(&clock);
        let clock_fn: Clock = Arc::new(move || page_clock.load(Ordering::SeqCst));
        let logs = Arc::new(Mutex::new(Vec::new()));
        let captured_logs = Arc::clone(&logs);
        let page = approve_page::Page::new(
            origin.to_string(),
            Arc::clone(&kernel),
            clock_fn,
            state_dir.clone(),
            Arc::clone(&display) as Arc<dyn SetupCodeDisplay>,
        )
        .with_canvas_origin(&format!("http://{}", upstream.addr))
        .with_chat_timeouts(chat_idle, chat_total)
        .with_actions(actions)
        .with_approval_requests(Arc::clone(&requests))
        .with_log(Arc::new(move |line| {
            captured_logs
                .lock()
                .expect("log lock")
                .push(line.to_string())
        }));
        let closed: Arc<Mutex<Option<String>>> = Arc::default();
        let gate = Arc::clone(&closed);
        let page = page
            .with_gate(Arc::new(move || gate.lock().expect("gate lock").clone()))
            .with_setup_command("arca setup-code --config /the/node.json".to_string());
        let canvas = page.canvas().clone();
        let proxy_router = if with_page {
            base.clone().merge(page.router(true))
        } else {
            base.clone()
        };
        // The unix socket's router as `serve` builds it: the page, and the one route no other listener carries.
        let router = if with_page {
            base.merge(page.router(false)).merge(page.local_router())
        } else {
            base
        };
        let socket = scratch.path("serve.sock");
        let runtime = Runtime::new().expect("a runtime");
        let served_socket = socket.clone();
        runtime.spawn(async move {
            let _ = serve_unix(router, &served_socket).await;
        });
        let proxy_socket = scratch.path("proxy.sock");
        let served_proxy_socket = proxy_socket.clone();
        runtime.spawn(async move {
            let _ = serve_unix(proxy_router, &served_proxy_socket).await;
        });
        let until = Instant::now() + Duration::from_secs(5);
        while !socket.exists() || !proxy_socket.exists() {
            assert!(Instant::now() < until, "the test server started");
            std::thread::sleep(Duration::from_millis(10));
        }
        let rig = Rig {
            _scratch: scratch,
            fake,
            kernel,
            socket,
            proxy_socket,
            logs,
            state_dir,
            clock,
            display,
            canvas,
            upstream,
            action_runs,
            requests,
            closed,
            _runtime: runtime,
        };
        (rig, key)
    }

    fn last_code(&self) -> String {
        self.display
            .codes
            .lock()
            .expect("display lock")
            .last()
            .cloned()
            .expect("a captured setup code")
    }

    fn advance(&self, millis: u64) {
        self.clock.fetch_add(millis, Ordering::SeqCst);
    }

    fn waiting_plan(&self) -> (String, String) {
        let mut kernel = self.kernel.lock().expect("the kernel lock");
        kernel
            .propose(vec![EntryChange::Put(toy_entry("t1"))], NOW)
            .expect("applied");
        kernel
            .propose(
                vec![EntryChange::Remove(EntryId("toy-scratch".to_string()))],
                NOW + 1_000,
            )
            .expect("removed");
        drop(kernel);
        let (status, body) = self.get("/approve/plans", None);
        assert_eq!(status, 200);
        let body: Value = serde_json::from_slice(&body).expect("plans JSON");
        let plan = &body["plans"][0];
        (
            plan["plan"].as_str().expect("a digest").to_string(),
            plan["statement"].as_str().expect("a statement").to_string(),
        )
    }

    fn waiting_action(&self) -> Value {
        self.waiting_action_named("nats")
    }

    fn waiting_action_named(&self, name: &str) -> Value {
        let now = self.clock.load(Ordering::SeqCst);
        let mut kernel = self.kernel.lock().expect("the kernel lock");
        let proposal = kernel
            .propose_action(
                arca::action_proposal::ActionKind::ServiceCreate,
                // Bound, as the node binds a create when it is proposed: an unbound one is never approved.
                json!({ "name": name, "template": "nats", "templateDigest": "ab".repeat(32) }),
                now,
                None,
            )
            .expect("an action proposal");
        serde_json::to_value(proposal).expect("the proposal serializes")
    }

    fn action_from_page(&self) -> Value {
        let (status, body) = self.get("/approve/plans", None);
        assert_eq!(status, 200);
        let body: Value = serde_json::from_slice(&body).expect("plans JSON");
        body["plans"]
            .as_array()
            .expect("plans")
            .iter()
            .find(|plan| plan["kind"] == json!("action"))
            .cloned()
            .expect("the action proposal")
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        origin: Option<&str>,
        body: &[u8],
    ) -> (u16, Vec<u8>) {
        self.request_on(false, None, method, path, origin, body)
    }

    fn request_on(
        &self,
        proxy: bool,
        forwarded: Option<&str>,
        method: &str,
        path: &str,
        origin: Option<&str>,
        body: &[u8],
    ) -> (u16, Vec<u8>) {
        let (status, _, body) =
            self.request_full_on(proxy, forwarded, method, path, origin, body, None, &[]);
        (status, body)
    }

    #[allow(clippy::too_many_arguments)]
    fn raw_request(
        &self,
        method: &str,
        path: &str,
        forwarded: Option<&str>,
        origin: Option<&str>,
        body: &[u8],
        cookie: Option<&str>,
        extra: &[(&str, &str)],
    ) -> Vec<u8> {
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: box1.example.com\r\nConnection: close\r\nContent-Length: {}\r\n",
            body.len()
        );
        if let Some(forwarded) = forwarded {
            head.push_str(&format!("X-Forwarded-For: {forwarded}\r\n"));
        }
        if let Some(origin) = origin {
            head.push_str(&format!("Origin: {origin}\r\n"));
        }
        if let Some(cookie) = cookie {
            head.push_str(&format!("Cookie: {cookie}\r\n"));
        }
        for (name, value) in extra {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        let has_content_type = extra
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("content-type"));
        if !body.is_empty() && !has_content_type {
            head.push_str("Content-Type: application/json\r\n");
        }
        head.push_str("\r\n");
        let mut bytes = head.into_bytes();
        bytes.extend_from_slice(body);
        bytes
    }

    #[allow(clippy::too_many_arguments)]
    fn request_full_on(
        &self,
        proxy: bool,
        forwarded: Option<&str>,
        method: &str,
        path: &str,
        origin: Option<&str>,
        body: &[u8],
        cookie: Option<&str>,
        extra: &[(&str, &str)],
    ) -> (u16, String, Vec<u8>) {
        let socket = if proxy {
            &self.proxy_socket
        } else {
            &self.socket
        };
        let mut stream = UnixStream::connect(socket).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");
        let bytes = self.raw_request(method, path, forwarded, origin, body, cookie, extra);
        stream.write_all(&bytes).expect("write");
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).expect("read");
        let split = raw
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("a response head");
        let head = String::from_utf8_lossy(&raw[..split]).to_string();
        let status = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|status| status.parse().ok())
            .expect("a status");
        (status, head, raw[split + 4..].to_vec())
    }

    /// Sends a request and returns before the streamed body ends, so a test can time each chunk and then drop the socket.
    fn open_stream_on(
        &self,
        method: &str,
        path: &str,
        origin: Option<&str>,
        body: &[u8],
        cookie: Option<&str>,
        extra: &[(&str, &str)],
    ) -> (UnixStream, u16, String, Vec<u8>) {
        let bytes = self.raw_request(method, path, None, origin, body, cookie, extra);
        let mut stream = UnixStream::connect(&self.socket).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");
        stream.write_all(&bytes).expect("write");
        let mut raw = Vec::new();
        let mut buffer = [0u8; 1024];
        loop {
            if let Some(split) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&raw[..split]).to_string();
                let status = head
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .and_then(|status| status.parse().ok())
                    .expect("a status");
                return (stream, status, head, raw[split + 4..].to_vec());
            }
            let read = stream.read(&mut buffer).expect("read response head");
            assert!(read > 0, "the response head");
            raw.extend_from_slice(&buffer[..read]);
        }
    }

    /// Sends already-built request bytes, tolerating a reset after the server has answered.
    fn raw_request_response(&self, bytes: &[u8]) -> (u16, String, Vec<u8>) {
        let mut stream = UnixStream::connect(&self.socket).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");
        let _ = stream.write_all(bytes);
        let mut raw = Vec::new();
        let _ = stream.read_to_end(&mut raw);
        assert!(!raw.is_empty(), "a response head");
        let split = raw
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("a response head");
        let head = String::from_utf8_lossy(&raw[..split]).to_string();
        let status = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|status| status.parse().ok())
            .expect("a status");
        (status, head, raw[split + 4..].to_vec())
    }

    fn chunked_request(
        &self,
        path: &str,
        cookie: &str,
        chunks: &[&[u8]],
    ) -> (u16, String, Vec<u8>) {
        let mut bytes = format!(
            "POST {path} HTTP/1.1\r\nHost: box1.example.com\r\nConnection: close\r\nOrigin: {ORIGIN}\r\nCookie: {cookie}\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n"
        )
        .into_bytes();
        for chunk in chunks {
            bytes.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
            bytes.extend_from_slice(chunk);
            bytes.extend_from_slice(b"\r\n");
        }
        bytes.extend_from_slice(b"0\r\n\r\n");
        self.raw_request_response(&bytes)
    }

    fn get_full(&self, path: &str, origin: Option<&str>) -> (u16, String, Vec<u8>) {
        self.request_full_on(false, None, "GET", path, origin, &[], None, &[])
    }

    fn get_with_cookie(&self, path: &str, cookie: &str) -> (u16, String, Vec<u8>) {
        self.request_full_on(false, None, "GET", path, None, &[], Some(cookie), &[])
    }

    fn head_with_cookie(&self, path: &str, cookie: &str) -> (u16, String, Vec<u8>) {
        self.request_full_on(false, None, "HEAD", path, None, &[], Some(cookie), &[])
    }

    fn post_with_cookie(
        &self,
        path: &str,
        origin: Option<&str>,
        body: Value,
        cookie: &str,
    ) -> (u16, String, Vec<u8>) {
        self.request_full_on(
            false,
            None,
            "POST",
            path,
            origin,
            &serde_json::to_vec(&body).expect("the body"),
            Some(cookie),
            &[],
        )
    }

    fn post_full(&self, path: &str, origin: Option<&str>, body: Value) -> (u16, String, Vec<u8>) {
        self.request_full_on(
            false,
            None,
            "POST",
            path,
            origin,
            &serde_json::to_vec(&body).expect("the body"),
            None,
            &[],
        )
    }

    fn get(&self, path: &str, origin: Option<&str>) -> (u16, Vec<u8>) {
        self.request("GET", path, origin, &[])
    }

    fn post(&self, path: &str, origin: Option<&str>, body: Value) -> (u16, Vec<u8>) {
        self.request(
            "POST",
            path,
            origin,
            serde_json::to_vec(&body).expect("the body").as_slice(),
        )
    }

    fn post_empty(&self, path: &str, origin: Option<&str>) -> (u16, Vec<u8>) {
        self.request("POST", path, origin, &[])
    }

    fn proxy_post(&self, caller: Option<&str>, path: &str, body: Value) -> (u16, Vec<u8>) {
        self.request_on(
            true,
            caller,
            "POST",
            path,
            Some(ORIGIN),
            &serde_json::to_vec(&body).expect("body"),
        )
    }

    fn state_path(&self) -> PathBuf {
        self.state_dir.join(STATE_PASSKEYS_FILE)
    }

    /// The file a terminal's request makes in the state directory before it asks: its proof that it can write there.
    fn proof_file(&self, proof: &str) -> PathBuf {
        self.state_dir
            .join(format!("{}{proof}", approve_page::LOCAL_PROOF_PREFIX))
    }

    /// `arca setup-code`'s request on the unix socket (or the other socket, which stands for every other listener): the
    /// proof file is made first when `write` says so.
    fn terminal_code(&self, other_listener: bool, proof: &str, write: bool) -> (u16, Value) {
        if write {
            fs::write(self.proof_file(proof), b"").expect("the proof file");
        }
        let (status, body) = self.request_on(
            other_listener,
            None,
            "POST",
            approve_page::LOCAL_SETUP_CODE_PATH,
            None,
            &serde_json::to_vec(&json!({ "proof": proof })).expect("body"),
        );
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }
}

/// The page origin of a node on its own machine's loopback listener.
const LOCAL_ORIGIN: &str = "http://localhost:7471";
/// Three proofs of the right form: 32 lowercase hexadecimal digits.
const PROOF_A: &str = "0123456789abcdef0123456789abcdef";
const PROOF_B: &str = "1123456789abcdef0123456789abcdef";
const PROOF_C: &str = "2123456789abcdef0123456789abcdef";

fn sign_in_statement(origin: &str, challenge: &str, now: u64) -> String {
    canonical_json(&json!({
        "kind": arca::canvas::STATEMENT_KIND,
        "origin": origin,
        "challenge": challenge,
        "issuedMs": now,
        "expiresMs": now + arca::canvas::CHALLENGE_TTL_MS,
    }))
    .expect("a canonical sign-in statement")
}

fn header_value(head: &str, name: &str) -> Option<String> {
    head.lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(key, _)| key.eq_ignore_ascii_case(name))
        })
        .map(|(_, value)| value.trim().to_string())
}

fn set_cookie(head: &str) -> String {
    header_value(head, "set-cookie")
        .expect("a Set-Cookie")
        .split(';')
        .next()
        .expect("a cookie pair")
        .to_string()
}

fn canvas_cookie(rig: &Rig) -> String {
    rig.canvas
        .issue_session("passkey:test", NOW)
        .expect("a session cookie")
        .split(';')
        .next()
        .expect("a cookie pair")
        .to_string()
}

fn chat_json() -> Vec<u8> {
    serde_json::to_vec(&json!({ "message": "hello" })).expect("chat JSON")
}

fn sse_events(body: &[u8]) -> Vec<(String, String)> {
    let text = String::from_utf8_lossy(body);
    let mut events = Vec::new();
    for block in text.split("\n\n") {
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

fn cut_reason(body: &[u8]) -> String {
    let events = sse_events(body);
    let (event, data) = events.last().expect("a final event");
    assert_eq!(event, "arca-cut", "the final event is the cut");
    serde_json::from_str::<Value>(data).expect("cut JSON")["reason"]
        .as_str()
        .expect("a cut reason")
        .to_string()
}

fn assert_cut(body: &[u8], reason: &str) {
    assert_eq!(cut_reason(body), reason, "the cut event names {reason}");
}

/// Reads until the stream body contains `needle`; a read timeout or an early end fails with the deadline and phrase.
fn read_until(
    stream: &mut UnixStream,
    mut body: Vec<u8>,
    needle: &str,
    deadline: Instant,
) -> Vec<u8> {
    let mut buffer = [0u8; 1024];
    while !String::from_utf8_lossy(&body).contains(needle) {
        assert!(
            Instant::now() < deadline,
            "the stream did not contain {needle:?} before the deadline"
        );
        let read = stream.read(&mut buffer).expect("read stream");
        assert!(read > 0, "the stream ended before {needle:?}");
        body.extend_from_slice(&buffer[..read]);
    }
    body
}

fn approval(key: &SoftPasskey, statement: &str) -> String {
    let assert = Assert::over(NAMESPACE, statement.as_bytes());
    passkey_approval(statement, &key.assert(&assert))
}

fn refusal(status: u16) -> bool {
    (400..=499).contains(&status)
}

fn register_body(key: &SoftPasskey, setup_code: Option<&str>) -> Value {
    let mut body = json!({
        "credentialId": key.cred_text(),
        "publicKey": key.key_text(),
        "publicKeyAlgorithm": -7,
    });
    if let Some(code) = setup_code {
        body["setupCode"] = json!(code);
    }
    body
}

#[test]
fn without_an_origin_the_page_routes_are_absent_and_mcp_refuses_an_origin() {
    let (rig, _) = Rig::new("page-absent", false);
    assert_eq!(rig.get("/approve", None).0, 404);
    assert_eq!(rig.get("/approve/plans", None).0, 404);
    assert_eq!(
        rig.request("POST", "/mcp", Some(ORIGIN), b"{}").0,
        403,
        "/mcp refuses every Origin"
    );
}

/// Every route of the approval page, as its router names them.
const PAGE_ROUTES: [(&str, &str); 21] = [
    ("GET", "/approve"),
    ("GET", "/approve/plans"),
    ("POST", "/approve/approve"),
    ("POST", "/approve/decline"),
    ("POST", "/approve/requests/begin"),
    ("POST", "/approve/requests/approve"),
    ("POST", "/approve/requests/deny"),
    ("POST", "/approve/peer-keys"),
    ("POST", "/approve/peer-keys/sync"),
    ("POST", "/approve/setup-code"),
    ("POST", "/approve/setup-code/verify"),
    ("POST", "/approve/register"),
    ("GET", "/approve/passkeys"),
    ("POST", "/approve/passkeys/change"),
    ("GET", "/approve/canvas"),
    ("POST", "/approve/canvas"),
    ("GET", "/approve/canvas/page.css"),
    ("GET", "/approve/canvas/page.js"),
    ("GET", "/approve/canvas/snapshot"),
    ("POST", "/approve/canvas/chat"),
    ("GET", "/approve/canvas/chat/history"),
];

#[test]
fn every_page_route_answers_the_same_with_and_without_a_trailing_slash_on_every_listener() {
    // A fresh node for each spelling and each listener, asked the same things in the same order, so a route that
    // changes state (a setup code, a sign-in challenge) is in the same state for both spellings.
    let ask = |proxy: bool, slash: bool| -> (Vec<u16>, Rig) {
        let tag = format!("slash-{}{}", u8::from(proxy), u8::from(slash));
        let (rig, _key) = Rig::new_empty(&tag);
        let answers = PAGE_ROUTES
            .iter()
            .map(|(method, path)| {
                let path = if slash {
                    format!("{path}/")
                } else {
                    path.to_string()
                };
                let body: &[u8] = if *method == "POST" { b"{}" } else { b"" };
                rig.request_on(proxy, None, method, &path, Some(ORIGIN), body)
                    .0
            })
            .collect();
        (answers, rig)
    };
    for proxy in [false, true] {
        let (plain, _rig) = ask(proxy, false);
        let (slashed, rig) = ask(proxy, true);
        for (((method, path), plain), slashed) in PAGE_ROUTES.iter().zip(&plain).zip(&slashed) {
            assert_eq!(
                slashed, plain,
                "{method} {path}/ answers as {method} {path} does (proxy listener: {proxy})"
            );
            // Not the catch-all's 404 and not a wrong method: the request reached the route's own handler. The two
            // peer-key routes answer 404 themselves on a node that keeps no peer in sync, as this one.
            if !path.starts_with("/approve/peer-keys") {
                assert!(
                    *slashed != 404 && *slashed != 405,
                    "{method} {path}/ did not reach its handler: {slashed}"
                );
            }
        }
        // The pages themselves are served, not redirected.
        for page in [
            "/approve/",
            "/approve/plans/",
            "/approve/passkeys/",
            "/approve/canvas/",
        ] {
            assert_eq!(rig.get(page, None).0, 200, "{page}");
        }
        // A POST with the slash is still a POST that did its work: the setup code it asked for was issued.
        assert_eq!(rig.last_code().len(), 8);
        // A query string stays with the request.
        assert_eq!(rig.get("/approve/plans/?after=1", None).0, 200);
        assert_eq!(rig.get("/approve/plans?after=1", None).0, 200);
        // The Origin rule is the route's, whichever way it is spelled: a POST with no Origin, or a foreign one, is
        // refused before its body is read.
        for path in [
            "/approve/register/",
            "/approve/canvas/chat/",
            "/approve/decline/",
        ] {
            assert_eq!(
                rig.request_on(proxy, None, "POST", path, None, b"{}").0,
                403,
                "{path}"
            );
            assert_eq!(
                rig.request_on(
                    proxy,
                    None,
                    "POST",
                    path,
                    Some("https://elsewhere.example"),
                    b"{}"
                )
                .0,
                403,
                "{path}"
            );
        }
        // What is not a route is not one with a slash either, and a route does not take another method.
        for path in [
            "/approve/nothing",
            "/approve/nothing/",
            "/approve//",
            "/approve/plans//",
        ] {
            assert_eq!(rig.get(path, None).0, 404, "{path}");
        }
        assert_eq!(rig.get("/approve/register/", None).0, 405);
        assert_eq!(rig.get("/approve/register", None).0, 405);
    }
}

#[test]
fn the_page_and_plan_list_are_open_and_mcp_still_refuses_every_origin() {
    let (rig, _) = Rig::new("page-open", true);
    assert_eq!(rig.get("/approve", None).0, 200);
    assert_eq!(rig.get("/approve", Some(ORIGIN)).0, 200);
    assert_eq!(
        rig.get("/approve", Some("https://elsewhere.example")).0,
        403
    );
    assert_eq!(rig.get("/approve/plans", None).0, 200);
    assert_eq!(
        rig.request("POST", "/mcp", Some(ORIGIN), b"{}").0,
        403,
        "/mcp refuses the configured Origin too"
    );
    assert_eq!(
        rig.request("POST", "/mcp", None, b"{}").0,
        401,
        "/mcp still needs its bearer without an Origin"
    );
}

#[test]
fn a_valid_approval_runs_the_plan_once_and_every_bad_post_deletes_nothing() {
    let (rig, key) = Rig::new("page-approve", true);
    let (plan, statement) = rig.waiting_plan();
    let good = approval(&key, &statement);
    let body = json!({ "plan": plan, "approval": good });

    assert!(
        refusal(
            rig.post(
                "/approve/approve",
                Some("https://elsewhere.example"),
                body.clone()
            )
            .0
        ),
        "a wrong Origin is refused"
    );
    assert!(
        refusal(rig.post("/approve/approve", None, body.clone()).0),
        "a missing Origin is refused"
    );

    let tampered = json!({
        "plan": plan,
        "approval": approval(&key, &format!("{statement} ")),
    });
    assert!(
        refusal(rig.post("/approve/approve", Some(ORIGIN), tampered).0),
        "a tampered statement is refused"
    );

    let mut wrong_key = Assert::over(NAMESPACE, statement.as_bytes());
    wrong_key.credential_id = Some(vec![0x99; 20]);
    let unknown = passkey_approval(&statement, &key.assert(&wrong_key));
    assert!(
        refusal(
            rig.post(
                "/approve/approve",
                Some(ORIGIN),
                json!({ "plan": plan, "approval": unknown })
            )
            .0
        ),
        "an unknown credential is refused"
    );
    assert_eq!(
        deletes_for(&rig.fake, "toy-box-t1"),
        0,
        "nothing deleted yet"
    );

    let (status, answer) = rig.post("/approve/approve", Some(ORIGIN), body.clone());
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&answer));
    assert_eq!(
        deletes_for(&rig.fake, "toy-box-t1"),
        1,
        "one DELETE was sent"
    );

    assert!(
        refusal(rig.post("/approve/approve", Some(ORIGIN), body).0),
        "the replay is refused"
    );
    assert_eq!(deletes_for(&rig.fake, "toy-box-t1"), 1, "still one DELETE");
}

#[test]
fn action_proposals_are_listed_next_to_delete_plans_and_decline_removes_one() {
    let (rig, _key) = Rig::new("page-action-list", true);
    rig.waiting_plan();
    rig.waiting_action();
    let action = rig.action_from_page();
    assert_eq!(action["kind"], json!("action"));
    assert_eq!(action["action"], json!("service_create"));
    assert_eq!(
        action["summary"],
        json!("Create the service nats from template nats")
    );
    assert_eq!(
        action["args"],
        json!({ "name": "nats", "template": "nats", "templateDigest": "ab".repeat(32) })
    );
    assert_eq!(
        action["details"],
        json!([
            { "label": "Name", "value": "nats" },
            { "label": "Template", "value": "nats" },
            {
                "label": "Template content",
                "value": format!(
                    "{}. Arca creates this only if the template on the node is still exactly this one.",
                    "ab".repeat(32)
                )
            }
        ])
    );
    let id = action["id"].as_str().expect("an id");
    assert!(id.starts_with("act-"));
    assert_eq!(id.len(), 16);
    let digest = action["argsDigest"].as_str().expect("a digest");
    assert_eq!(digest.len(), 64);
    let statement = action["statement"].as_str().expect("a statement");
    let parsed = parse_statement(statement.as_bytes()).expect("the kernel parser reads it");
    assert_eq!(parsed.action, ArcaAction::ServiceCreate);
    assert_eq!(parsed.target, id);
    assert_eq!(parsed.params_digest, digest);
    assert_eq!(
        action["expiresAt"].as_u64(),
        action["createdAt"].as_u64().map(|at| at + 15 * 60 * 1000)
    );

    let (status, body) = rig.get("/approve/plans", None);
    assert_eq!(status, 200);
    let listed: Value = serde_json::from_slice(&body).expect("plans JSON");
    assert_eq!(listed["plans"].as_array().map(Vec::len), Some(2));
    let delete_id = listed["plans"]
        .as_array()
        .expect("plans")
        .iter()
        .find_map(|plan| plan["plan"].as_str())
        .expect("the delete plan is still listed");
    let (status, body) = rig.post(
        "/approve/decline",
        Some(ORIGIN),
        json!({ "plan": delete_id }),
    );
    assert!(
        refusal(status),
        "the open page cannot decline a delete plan"
    );
    assert!(
        String::from_utf8_lossy(&body).contains("DoctorRequired"),
        "the refusal names the doctor door"
    );
    let (status, body) = rig.get("/approve/plans", None);
    assert_eq!(status, 200);
    let listed: Value = serde_json::from_slice(&body).expect("plans JSON");
    assert_eq!(
        listed["plans"].as_array().map(Vec::len),
        Some(2),
        "the refused decline left the delete plan waiting"
    );

    let (status, body) = rig.post("/approve/decline", Some(ORIGIN), json!({ "plan": id }));
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let declined: Value = serde_json::from_slice(&body).expect("decline JSON");
    assert_eq!(declined["plan"], json!(id));
    assert_eq!(declined["retained"], json!([]));
    let (status, body) = rig.get("/approve/plans", None);
    assert_eq!(status, 200);
    let listed: Value = serde_json::from_slice(&body).expect("plans JSON");
    assert_eq!(
        listed["plans"].as_array().map(Vec::len),
        Some(1),
        "only the delete plan remains"
    );
    assert!(
        listed["plans"]
            .as_array()
            .expect("plans")
            .iter()
            .all(|plan| plan["kind"] != json!("action"))
    );
    assert!(
        refusal(
            rig.post("/approve/decline", Some(ORIGIN), json!({ "plan": id }))
                .0
        ),
        "a second decline finds nothing"
    );
}

#[test]
fn a_passkey_approval_runs_an_action_proposal_exactly_once() {
    let (rig, key) = Rig::new("page-action-approve", true);
    rig.waiting_action();
    let action = rig.action_from_page();
    let id = action["id"].as_str().expect("an id").to_string();
    let statement = action["statement"].as_str().expect("a statement");
    let body = json!({ "plan": id, "approval": approval(&key, statement) });

    let (status, answer) = rig.post("/approve/approve", Some(ORIGIN), body.clone());
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&answer));
    let answer: Value = serde_json::from_slice(&answer).expect("approval JSON");
    assert_eq!(answer["plan"], json!(id));
    assert_eq!(answer["action"], json!("service_create"));
    assert_eq!(answer["result"], json!({ "created": true }));
    assert_eq!(rig.action_runs.load(Ordering::SeqCst), 1);
    let (status, listed_body) = rig.get("/approve/plans", None);
    assert_eq!(status, 200);
    let listed: Value = serde_json::from_slice(&listed_body).expect("plans JSON");
    assert_eq!(listed["plans"], json!([]), "the action proposal is retired");

    assert!(
        refusal(rig.post("/approve/approve", Some(ORIGIN), body).0),
        "the replay is refused"
    );
    assert_eq!(
        rig.action_runs.load(Ordering::SeqCst),
        1,
        "the action did not run again"
    );
}

/// F4: an approval that Arca accepted, for an action that then failed, answers with the action's own message, keeps it
/// in the proposals journal and retires the card, so the page has words to show in place of its spinner. A failure for
/// want of room keeps its code and its fields beside the message, and says the action must be proposed again.
#[test]
fn an_approved_action_that_fails_answers_its_message_and_journals_it() {
    let (rig, key) = Rig::new("page-action-failed", true);
    for (name, code, message, fields) in [
        (
            "stuck",
            "refused",
            format!("The approval was accepted, but service_create did not complete: {STUCK}"),
            json!({}),
        ),
        (
            "full",
            "capacity",
            format!("The approval was accepted, but service_create did not complete: {FULL}"),
            // The approval is spent: the page is told the action must be proposed again.
            json!({ "scope": "node", "limit": "virtualMachines", "have": 4, "want": 1, "max": 4,
                "proposeAgain": true }),
        ),
        (
            "broken",
            "Internal",
            "The approval was accepted, but service_create did not complete: Arca hit an internal error while running it".to_string(),
            json!({}),
        ),
    ] {
        rig.waiting_action_named(name);
        let action = rig.action_from_page();
        let id = action["id"].as_str().expect("an id").to_string();
        let statement = action["statement"].as_str().expect("a statement");
        let body = json!({ "plan": id, "approval": approval(&key, statement) });
        let (status, answer) = rig.post("/approve/approve", Some(ORIGIN), body);
        assert_eq!(status, 403, "{name}");
        let answer: Value = serde_json::from_slice(&answer).expect("a JSON answer, never an empty one");
        let mut expected = fields;
        expected["refusal"] = json!(code);
        expected["message"] = json!(message);
        assert_eq!(answer, expected, "{name}");
        assert!(message.starts_with(arca::handlers::APPROVED_BUT_FAILED));

        // The kernel's own state directory is the scratch root, one level above the page's.
        let journal = fs::read_to_string(
            rig.state_dir
                .parent()
                .expect("the scratch root")
                .join("action-proposals.jsonl"),
        )
        .expect("the proposals journal");
        let result: Value = journal
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .rfind(|event| event["event"] == "result" && event["id"] == json!(id))
            .expect("a result event");
        assert_eq!(
            result,
            json!({ "event": "result", "proposal": null, "id": id, "ok": false, "code": code, "message": message }),
            "{name}"
        );
        let (_, listed) = rig.get("/approve/plans", None);
        let listed: Value = serde_json::from_slice(&listed).expect("plans JSON");
        assert_eq!(listed["plans"], json!([]), "the spent proposal is gone: {name}");
    }
    assert_eq!(rig.action_runs.load(Ordering::SeqCst), 3);

    // The page shows a message that begins this way in full. Its script and the handler share the words.
    let (status, page) = rig.get("/approve", None);
    assert_eq!(status, 200);
    let page = String::from_utf8(page).expect("the page is text");
    assert!(page.contains(&format!(
        "const APPROVED_BUT_FAILED = \"{}\";",
        arca::handlers::APPROVED_BUT_FAILED
    )));
    assert!(page.contains("body.message.startsWith(APPROVED_BUT_FAILED)"));
}

#[test]
fn an_expired_action_or_a_statement_for_another_id_is_refused() {
    let (rig, key) = Rig::new("page-action-refusals", true);
    rig.waiting_action();
    let action = rig.action_from_page();
    let id = action["id"].as_str().expect("an id").to_string();
    let statement = action["statement"].as_str().expect("a statement");
    let expired = approval(&key, statement);
    rig.advance(15 * 60 * 1000 + 1);
    assert!(
        refusal(
            rig.post(
                "/approve/approve",
                Some(ORIGIN),
                json!({ "plan": id, "approval": expired })
            )
            .0
        ),
        "the proposal lapsed"
    );
    assert_eq!(rig.action_runs.load(Ordering::SeqCst), 0);

    rig.waiting_action();
    let action = rig.action_from_page();
    let id = action["id"].as_str().expect("an id").to_string();
    let digest = action["argsDigest"].as_str().expect("a digest");
    let wrong_target = arca::handlers::action_statement(
        HOST,
        "act-000000000000",
        "service_create",
        digest,
        NOW + 15 * 60 * 1000 + 1,
        &"9".repeat(32),
    )
    .expect("a statement");
    let (status, body) = rig.post(
        "/approve/approve",
        Some(ORIGIN),
        json!({ "plan": id, "approval": approval(&key, &wrong_target) }),
    );
    assert!(refusal(status));
    assert!(
        String::from_utf8_lossy(&body).contains("WrongTarget"),
        "the target differs"
    );
    assert_eq!(rig.action_runs.load(Ordering::SeqCst), 0);
}

#[test]
fn registration_round_trips_through_parse_passkeys_and_refuses_bad_keys() {
    let (rig, key) = Rig::new_empty("page-register");
    assert_eq!(rig.post_empty("/approve/setup-code", Some(ORIGIN)).0, 200);
    let code = rig.last_code();
    let (status, body) = rig.post(
        "/approve/register",
        Some(ORIGIN),
        register_body(&key, Some(&code)),
    );
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let body: Value = serde_json::from_slice(&body).expect("registration JSON");
    assert_eq!(body["trusted"], true);
    assert!(body.get("line").is_none());
    let line = fs::read_to_string(rig.state_path()).expect("the state passkeys file");
    let parsed = parse_passkeys(line.as_bytes()).expect("the line parses");
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].credential_id, key.credential_id);
    assert_eq!(parsed[0].fingerprint, key.fingerprint());

    let bad_alg = json!({
        "credentialId": key.cred_text(),
        "publicKey": key.key_text(),
        "publicKeyAlgorithm": -8,
    });
    assert!(refusal(
        rig.post("/approve/register", Some(ORIGIN), bad_alg).0
    ));

    let bad_key = json!({
        "credentialId": key.cred_text(),
        "publicKey": "AAAA",
        "publicKeyAlgorithm": -7,
    });
    assert!(refusal(
        rig.post("/approve/register", Some(ORIGIN), bad_key).0
    ));

    assert!(refusal(
        rig.post("/approve/register", None, register_body(&key, None))
            .0
    ));
}

#[test]
fn registration_without_a_setup_code_is_refused_without_a_developer_line() {
    let (rig, key) = Rig::new_empty("page-no-code");
    let (status, body) = rig.post("/approve/register", Some(ORIGIN), register_body(&key, None));
    assert_eq!(status, 400);
    let body: Value = serde_json::from_slice(&body).expect("refusal JSON");
    assert_eq!(body["message"], "Enter the setup code first");
    assert!(body.get("line").is_none());
    assert!(!rig.state_path().exists());
    assert_eq!(rig.kernel.lock().expect("kernel").owner_passkey_count(), 0);
}

#[test]
fn setup_code_verification_does_not_trust_or_consume_the_code() {
    let (rig, key) = Rig::new_empty("page-verify");
    let (status, body) = rig.post("/approve/setup-code/verify", Some(ORIGIN), json!({}));
    assert_eq!(status, 400);
    assert!(String::from_utf8_lossy(&body).contains("Enter the setup code first"));
    assert!(!rig.state_path().exists());
    let (status, body) = rig.post(
        "/approve/setup-code/verify",
        Some(ORIGIN),
        json!({ "setupCode": "12345678" }),
    );
    assert_eq!(status, 403, "{}", String::from_utf8_lossy(&body));
    assert!(String::from_utf8_lossy(&body).contains("expired"));
    assert!(!rig.state_path().exists());
    assert_eq!(rig.kernel.lock().expect("kernel").owner_passkey_count(), 0);

    assert_eq!(rig.post_empty("/approve/setup-code", Some(ORIGIN)).0, 200);
    let code = rig.last_code();
    let (status, body) = rig.post(
        "/approve/setup-code/verify",
        Some(ORIGIN),
        json!({ "setupCode": code }),
    );
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let body: Value = serde_json::from_slice(&body).expect("verification JSON");
    assert_eq!(body["verified"], true);
    assert_eq!(rig.kernel.lock().expect("kernel").owner_passkey_count(), 0);
    assert!(!rig.state_path().exists());

    let (status, body) = rig.post(
        "/approve/register",
        Some(ORIGIN),
        register_body(&key, Some(&code)),
    );
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert_eq!(rig.kernel.lock().expect("kernel").owner_passkey_count(), 1);
}

#[test]
fn wrong_setup_codes_use_the_registration_try_budget_before_any_create() {
    let (rig, _key) = Rig::new_empty("page-verify-budget");
    assert_eq!(rig.post_empty("/approve/setup-code", Some(ORIGIN)).0, 200);
    for _ in 0..5 {
        let (status, _) = rig.post(
            "/approve/setup-code/verify",
            Some(ORIGIN),
            json!({ "setupCode": "wrong" }),
        );
        assert!(refusal(status), "a wrong code is refused");
    }
    let (status, body) = rig.post(
        "/approve/setup-code/verify",
        Some(ORIGIN),
        json!({ "setupCode": "wrong" }),
    );
    assert_eq!(status, 429);
    assert!(String::from_utf8_lossy(&body).contains("locked"));
    assert!(!rig.state_path().exists());
    assert_eq!(rig.kernel.lock().expect("kernel").owner_passkey_count(), 0);
}

#[test]
fn setup_code_enrols_the_first_passkey_and_approval_works_without_restart() {
    let (rig, key) = Rig::new_empty("page-enrol");
    assert!(refusal(
        rig.post_empty("/approve/setup-code", Some("https://elsewhere.example"))
            .0
    ));
    let (status, answer) = rig.post_empty("/approve/setup-code", Some(ORIGIN));
    assert_eq!(status, 200);
    let code = rig.last_code();
    assert_eq!(code.len(), 8);
    assert!(
        !String::from_utf8_lossy(&answer).contains(&code),
        "the setup code is not in the HTTP answer"
    );

    let (status, answer) = rig.post(
        "/approve/register",
        Some(ORIGIN),
        register_body(&key, Some(&code)),
    );
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&answer));
    let body: Value = serde_json::from_slice(&answer).expect("registration JSON");
    assert_eq!(body["trusted"], json!(true));
    assert!(
        !String::from_utf8_lossy(&answer).contains(&code),
        "the setup code is not in the registration answer"
    );
    let state = fs::metadata(rig.state_path()).expect("the state passkeys file");
    assert_eq!(state.permissions().mode() & 0o777, 0o600);
    let lines = load_state_passkeys(&rig.state_path()).expect("the state line loads");
    assert_eq!(lines.len(), 1);
    assert!(
        !fs::read_to_string(rig.state_path())
            .expect("the state file reads")
            .contains(&code)
    );

    let (plan, statement) = rig.waiting_plan();
    let (status, answer) = rig.post(
        "/approve/approve",
        Some(ORIGIN),
        json!({ "plan": plan, "approval": approval(&key, &statement) }),
    );
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&answer));
    assert_eq!(deletes_for(&rig.fake, "toy-box-t1"), 1);
}

#[test]
fn the_page_at_http_localhost_enrols_a_passkey_for_the_rp_localhost_and_approves_from_that_origin_alone()
 {
    let (rig, key) = Rig::new_local("page-local-origin");
    // The page's POSTs take its exact origin: the IP address is another origin, and so is https.
    for other in [
        "http://127.0.0.1:7471",
        "http://localhost:7472",
        "https://localhost:7471",
        ORIGIN,
    ] {
        assert_eq!(
            rig.post_empty("/approve/setup-code", Some(other)).0,
            403,
            "{other}"
        );
    }
    assert_eq!(
        rig.post_empty("/approve/setup-code", Some(LOCAL_ORIGIN)).0,
        200
    );
    let code = rig.last_code();
    let (status, answer) = rig.post(
        "/approve/register",
        Some(LOCAL_ORIGIN),
        register_body(&key, Some(&code)),
    );
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&answer));
    let lines = load_state_passkeys(&rig.state_path()).expect("the state line loads");
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].rp_id, "localhost");
    assert_eq!(lines[0].origin, LOCAL_ORIGIN);

    // An assertion a browser made at the IP address names that origin, and is no approval here.
    let (plan, statement) = rig.waiting_plan();
    let mut elsewhere = Assert::over(NAMESPACE, statement.as_bytes());
    elsewhere.origin = Some("http://127.0.0.1:7471".to_string());
    let (status, _) = rig.post(
        "/approve/approve",
        Some(LOCAL_ORIGIN),
        json!({ "plan": plan, "approval": passkey_approval(&statement, &key.assert(&elsewhere)) }),
    );
    assert!(refusal(status), "{status}");
    assert_eq!(deletes_for(&rig.fake, "toy-box-t1"), 0);
    // The same passkey at the page's own origin approves.
    let (status, answer) = rig.post(
        "/approve/approve",
        Some(LOCAL_ORIGIN),
        json!({ "plan": plan, "approval": approval(&key, &statement) }),
    );
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&answer));
    assert_eq!(deletes_for(&rig.fake, "toy-box-t1"), 1);
}

#[test]
fn the_terminal_setup_code_answers_only_a_caller_that_wrote_the_state_directory() {
    let (rig, key) = Rig::new_empty("page-terminal-code");

    // No file in the state directory under the request's name: no code. This is a peer that reached the socket without
    // being the node's user on this machine, such as one behind a forward.
    let (status, body) = rig.terminal_code(false, PROOF_A, false);
    assert_eq!(status, 403, "{body}");
    assert!(body.get("code").is_none());
    // A name that is not 32 lowercase hexadecimal digits is not looked for at all: nothing outside the state
    // directory is ever named.
    for bad in [
        "",
        "0123456789abcdef",
        "0123456789ABCDEF0123456789ABCDEF",
        "../../../../../../etc/passwd-0000",
        "0123456789abcdef0123456789abcde/",
    ] {
        assert_eq!(rig.terminal_code(false, bad, false).0, 400, "{bad:?}");
    }
    // A link or a directory under the name is no proof, and is left where it is.
    symlink(rig.state_path(), rig.proof_file(PROOF_A)).expect("a link");
    assert_eq!(rig.terminal_code(false, PROOF_A, false).0, 403);
    assert!(
        rig.proof_file(PROOF_A).is_symlink(),
        "the link is not removed"
    );
    fs::remove_file(rig.proof_file(PROOF_A)).expect("remove the link");
    fs::create_dir(rig.proof_file(PROOF_A)).expect("a directory");
    assert_eq!(rig.terminal_code(false, PROOF_A, false).0, 403);
    fs::remove_dir(rig.proof_file(PROOF_A)).expect("remove the directory");
    // A request that carries an Origin is a browser's, and no browser is a terminal.
    fs::write(rig.proof_file(PROOF_A), b"").expect("the proof file");
    let (status, _) = rig.request(
        "POST",
        approve_page::LOCAL_SETUP_CODE_PATH,
        Some(ORIGIN),
        &serde_json::to_vec(&json!({ "proof": PROOF_A })).expect("body"),
    );
    assert_eq!(status, 403);
    assert!(rig.proof_file(PROOF_A).exists(), "the file was not taken");
    // No other listener carries the route, whatever the caller shows.
    assert_eq!(rig.terminal_code(true, PROOF_A, false).0, 404);
    assert!(rig.proof_file(PROOF_A).exists());

    // The node's user: the file is there, the code comes back, and the file is gone, so the same request again is
    // refused.
    let (status, body) = rig.terminal_code(false, PROOF_A, false);
    assert_eq!(status, 200, "{body}");
    let code = body["code"].as_str().expect("a code").to_string();
    assert!(code.len() == 8 && code.bytes().all(|b| b.is_ascii_digit()));
    assert_eq!(body["expiresInSeconds"], json!(600));
    assert!(!rig.proof_file(PROOF_A).exists(), "the proof is used once");
    assert_eq!(rig.terminal_code(false, PROOF_A, false).0, 403);
    assert!(
        rig.display.codes.lock().expect("display lock").is_empty(),
        "nothing was sent to a screen"
    );
    assert!(
        !rig.logs
            .lock()
            .expect("log lock")
            .iter()
            .any(|line| line.contains(&code)),
        "the code is in no log line"
    );

    // While that code is live, a second terminal request reads the same one, and the page's own request is told a
    // code is live, as before.
    rig.advance(60_000);
    let (status, body) = rig.terminal_code(false, PROOF_B, true);
    assert_eq!(status, 200);
    assert_eq!(body["code"], json!(code));
    assert_eq!(body["expiresInSeconds"], json!(540));
    assert_eq!(rig.post_empty("/approve/setup-code", Some(ORIGIN)).0, 429);

    // It is the page's code: it enrols the first passkey there. After that there is no setup code for anyone.
    let (status, answer) = rig.post(
        "/approve/register",
        Some(ORIGIN),
        register_body(&key, Some(&code)),
    );
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&answer));
    let (status, body) = rig.terminal_code(false, PROOF_C, true);
    assert_eq!(status, 409, "{body}");
    assert!(body.get("code").is_none());
    assert!(
        !rig.proof_file(PROOF_C).exists(),
        "a refused request leaves no file either"
    );
}

#[test]
fn a_closed_page_serves_nothing_takes_nothing_and_gives_no_setup_code() {
    // The page of this machine alone is closed while the node does not hold both loopback addresses of its port:
    // some other program may then be answering at the page's own origin. Whatever a person typed or signed there
    // must be worth nothing here.
    let (rig, key) = Rig::new_local("page-closed");
    let why = "The approval page is closed: this node does not hold [::1]:7471 (address in use: another program holds this port).";
    *rig.closed.lock().expect("gate lock") = Some(why.to_string());
    let closed = |status: u16, body: &[u8]| {
        assert_eq!(status, 503);
        let body: Value = serde_json::from_slice(body).expect("a refusal in JSON");
        assert_eq!(body["refusal"], json!("page_closed"));
        assert_eq!(body["message"], json!(why));
    };
    // Every route of the page, the page itself and its open list of cards among them.
    for path in [
        "/approve",
        "/approve/plans",
        "/approve/passkeys",
        "/approve/canvas",
    ] {
        let (status, body) = rig.get(path, None);
        closed(status, &body);
    }
    for path in [
        "/approve/setup-code",
        "/approve/setup-code/verify",
        "/approve/register",
        "/approve/approve",
        "/approve/decline",
        "/approve/requests/begin",
        "/approve/passkeys/change",
    ] {
        let (status, body) = rig.post(path, Some(LOCAL_ORIGIN), json!({}));
        closed(status, &body);
    }
    assert!(
        rig.display.codes.lock().expect("display lock").is_empty(),
        "no setup code was sent to a screen"
    );
    // The terminal gets no code either, though it shows that it is the node's user: the code would be typed into
    // whatever answers at the page's address.
    let (status, body) = rig.terminal_code(false, PROOF_A, true);
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["message"], json!(why));
    assert!(body.get("code").is_none());
    // The Origin rule still comes first: a foreign origin learns nothing, not even that the page is closed.
    assert_eq!(
        rig.post_empty("/approve/setup-code", Some("http://127.0.0.1:7471"))
            .0,
        403
    );

    // The node holds both addresses again: the page is the page.
    *rig.closed.lock().expect("gate lock") = None;
    assert_eq!(rig.get("/approve", None).0, 200);
    let (status, body) = rig.terminal_code(false, PROOF_B, true);
    assert_eq!(status, 200, "{body}");
    let code = body["code"].as_str().expect("a code").to_string();
    let (status, answer) = rig.post(
        "/approve/register",
        Some(LOCAL_ORIGIN),
        register_body(&key, Some(&code)),
    );
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&answer));
}

#[test]
fn the_terminals_proof_must_be_fresh_and_the_state_directory_still_private() {
    let (rig, _key) = Rig::new_empty("page-proof-fresh");
    // A file that was made two minutes ago is not a request that was made just now.
    let stale = rig.proof_file(PROOF_A);
    fs::write(&stale, b"").expect("the proof file");
    fs::File::options()
        .write(true)
        .open(&stale)
        .expect("open the proof file")
        .set_modified(std::time::SystemTime::now() - Duration::from_secs(120))
        .expect("set its time");
    assert_eq!(rig.terminal_code(false, PROOF_A, false).0, 403);
    assert!(stale.exists(), "a stale file is not taken");
    fs::remove_file(&stale).expect("remove it");

    // The state directory was opened to others after the node started: a file in it no longer shows who made it.
    fs::set_permissions(&rig.state_dir, fs::Permissions::from_mode(0o755)).expect("chmod 0755");
    assert_eq!(rig.terminal_code(false, PROOF_B, true).0, 403);
    assert!(rig.proof_file(PROOF_B).exists(), "nothing is taken from it");
    fs::set_permissions(&rig.state_dir, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
    assert_eq!(rig.terminal_code(false, PROOF_B, false).0, 200);
}

#[test]
fn the_terminal_setup_code_works_where_no_screen_does_and_shares_the_hourly_bound() {
    let (rig, _key) = Rig::new_empty("page-terminal-no-screen");
    // The machine has no screen: the page's request fails, and says so.
    rig.display.fail.store(true, Ordering::SeqCst);
    let (status, body) = rig.post_empty("/approve/setup-code", Some(ORIGIN));
    assert_eq!(status, 503);
    // The answer does not leave the person there: it names the exact line that needs no screen, with this node's own
    // config, and the page shows the same line before anything was pressed.
    assert_eq!(
        serde_json::from_slice::<Value>(&body).expect("a refusal in JSON"),
        json!({
            "message": "No screen to show the code on",
            "command": "arca setup-code --config /the/node.json"
        })
    );
    let (status, page) = rig.get("/approve", None);
    assert_eq!(status, 200);
    let page = String::from_utf8(page).expect("the page is text");
    assert!(
        page.contains("data-setup-command=\"arca setup-code --config /the/node.json\""),
        "the page carries the line"
    );
    assert!(!page.contains("__ARCA_SETUP_COMMAND__"));
    assert!(page.contains("In a terminal on that computer, run: \" + SETUP_COMMAND"));
    // Eight more page requests, each refused and each counted: nine of the hour's ten are spent.
    for _ in 0..8 {
        assert!(refusal(
            rig.post_empty("/approve/setup-code", Some(ORIGIN)).0
        ));
    }
    // The terminal's request right after them is not asked to wait: the 60-second spacing is the screen's. It is
    // the tenth request of the hour.
    let (status, body) = rig.terminal_code(false, PROOF_A, true);
    assert_eq!(status, 200, "{body}");
    // While that code lives it is read again, and reading it spends nothing.
    assert_eq!(
        rig.terminal_code(false, PROOF_B, true).1["code"],
        body["code"]
    );
    // Once it has expired, a new code would be the eleventh request: setup locks, for the terminal as for the page.
    rig.advance(10 * 60_000 + 1_000);
    let (status, body) = rig.terminal_code(false, PROOF_C, true);
    assert_eq!(status, 429, "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .starts_with("Setup is locked"),
        "{body}"
    );
    assert_eq!(rig.post_empty("/approve/setup-code", Some(ORIGIN)).0, 429);
}

#[test]
fn a_second_setup_code_is_refused_while_one_is_live_and_the_live_code_still_works() {
    let (rig, key) = Rig::new_empty("page-live-code");
    assert_eq!(rig.post_empty("/approve/setup-code", Some(ORIGIN)).0, 200);
    let code = rig.last_code();
    let (status, body) = rig.post_empty("/approve/setup-code", Some(ORIGIN));
    assert_eq!(status, 429, "a live code is never replaced");
    // The answer says it was asked too soon, in words and as the `rate` code, with the wait: the code's ten minutes.
    assert_eq!(
        rate_answer(&body),
        ("A code is already on your screen".to_string(), Some(600))
    );
    let (status, _) = rig.post(
        "/approve/register",
        Some(ORIGIN),
        register_body(&key, Some(&code)),
    );
    assert_eq!(status, 200, "the first code still works");
}

#[test]
fn setup_code_spacing_survives_a_failed_display_and_the_hourly_budget_locks() {
    let (rig, _) = Rig::new_empty("page-limits");
    rig.display.fail.store(true, Ordering::SeqCst);
    assert_eq!(
        rig.post_empty("/approve/setup-code", Some(ORIGIN)).0,
        503,
        "no screen means no code"
    );
    let (status, body) = rig.post_empty("/approve/setup-code", Some(ORIGIN));
    assert_eq!(status, 429, "the next request is spaced");
    assert_eq!(
        rate_answer(&body),
        ("Wait 60 more second(s)".to_string(), Some(60))
    );
    rig.display.fail.store(false, Ordering::SeqCst);
    rig.advance(61_000);
    assert_eq!(rig.post_empty("/approve/setup-code", Some(ORIGIN)).0, 200);
    // The live code refuses replacement but every request counts toward ten; the eleventh locks.
    for _ in 0..9 {
        let status = rig.post_empty("/approve/setup-code", Some(ORIGIN)).0;
        assert!(status == 429 || status == 200);
    }
    let (status, body) = rig.post_empty("/approve/setup-code", Some(ORIGIN));
    assert_eq!(status, 429);
    assert_eq!(
        rate_answer(&body),
        (
            "Setup is locked for 60 more minute(s)".to_string(),
            Some(3600)
        )
    );
}

/// A 429 answer of the page: its sentence, and the wait in seconds when it tells one. The body carries the `rate`
/// refusal code beside the sentence.
fn rate_answer(body: &[u8]) -> (String, Option<u64>) {
    let body: Value = serde_json::from_slice(body).expect("a JSON body");
    assert_eq!(body["refusal"], "rate", "{body}");
    (
        body["message"].as_str().expect("a message").to_string(),
        body["retryAfterSeconds"].as_u64(),
    )
}

#[test]
fn five_wrong_setup_codes_lock_for_an_hour_and_then_unlock() {
    let (rig, key) = Rig::new_empty("page-wrong");
    assert_eq!(rig.post_empty("/approve/setup-code", Some(ORIGIN)).0, 200);
    let code = rig.last_code();
    let wrong = if code == "99999999" {
        "11111111"
    } else {
        "99999999"
    };
    for _ in 0..5 {
        let (status, _) = rig.post(
            "/approve/register",
            Some(ORIGIN),
            register_body(&key, Some(wrong)),
        );
        assert!(refusal(status), "a wrong code is refused");
    }
    let (status, body) = rig.post_empty("/approve/setup-code", Some(ORIGIN));
    assert_eq!(status, 429);
    assert!(String::from_utf8_lossy(&body).contains("locked"));
    rig.advance(10 * 60 * 1000);
    assert_eq!(
        rig.post(
            "/approve/register",
            Some(ORIGIN),
            register_body(&key, Some(wrong))
        )
        .0,
        429,
        "a lock refuses wrong attempts even after the live code expires"
    );
    rig.advance(50 * 60 * 1000);
    assert_eq!(
        rig.post_empty("/approve/setup-code", Some(ORIGIN)).0,
        200,
        "the lock is timed, not until restart"
    );
}

#[test]
fn an_expired_setup_code_is_refused_and_no_passkey_is_trusted() {
    let (rig, key) = Rig::new_empty("page-expired");
    assert_eq!(rig.post_empty("/approve/setup-code", Some(ORIGIN)).0, 200);
    let code = rig.last_code();
    rig.advance(10 * 60 * 1000 + 1);
    let (status, _) = rig.post(
        "/approve/register",
        Some(ORIGIN),
        register_body(&key, Some(&code)),
    );
    assert_eq!(status, 403);
    assert!(!rig.state_path().exists());
    assert_eq!(
        rig.kernel
            .lock()
            .expect("kernel lock")
            .owner_passkey_count(),
        0
    );
}

#[test]
fn a_setup_code_is_refused_once_a_passkey_is_trusted() {
    let (rig, key) = Rig::new_empty("page-trusted");
    assert_eq!(rig.post_empty("/approve/setup-code", Some(ORIGIN)).0, 200);
    let code = rig.last_code();
    assert_eq!(
        rig.post(
            "/approve/register",
            Some(ORIGIN),
            register_body(&key, Some(&code))
        )
        .0,
        200
    );
    assert_eq!(rig.post_empty("/approve/setup-code", Some(ORIGIN)).0, 409);
    assert_eq!(
        rig.post(
            "/approve/register",
            Some(ORIGIN),
            register_body(&key, Some(&code))
        )
        .0,
        409
    );
}

#[test]
fn the_state_passkeys_loader_refuses_a_0644_file_and_a_link() {
    let scratch = Scratch::new("page-state-rule");
    let state = scratch.path("state");
    fs::create_dir(&state).expect("state dir");
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).expect("state mode");
    let key = SoftPasskey::from_seed(1);
    let path = state.join(STATE_PASSKEYS_FILE);
    fs::write(&path, format!("{}\n", key.line(NAMESPACE))).expect("state file");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("0644");
    assert!(
        load_state_passkeys(&path).is_err(),
        "a 0644 file is refused"
    );
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("0600");
    assert_eq!(
        load_state_passkeys(&path).expect("a 0600 file loads").len(),
        1
    );
    let link = state.join("link-passkeys");
    symlink(&path, &link).expect("symlink");
    assert!(load_state_passkeys(&link).is_err(), "a link is refused");
}

fn wrong_code(code: &str) -> &str {
    if code == "99999999" {
        "11111111"
    } else {
        "99999999"
    }
}

#[test]
fn owner_page_hides_the_developer_line_and_requires_a_checked_code() {
    let (rig, _) = Rig::new_empty("page-owner-source");
    let (status, body) = rig.get("/approve", None);
    assert_eq!(status, 200);
    let source = String::from_utf8(body).expect("page text");
    // Static source check: no DOM runner is available. The browser ceremony proves the guards at runtime.
    assert!(!source.contains("For developers"));
    assert!(!source.contains("id=\"line\""));
    assert!(!source.contains("body.line"));
    assert!(!source.contains("Passkey registered"));
    assert!(source.contains("Enter the 8-digit setup code first."));
    assert!(source.contains("/approve/setup-code/verify"));
    assert!(source.contains("showTrusted(body.passkeysTrusted || 0)"));
    assert!(source.contains("await loadPlans();"));
}

#[test]
fn enrolment_logs_only_the_trusted_count() {
    let (rig, key) = Rig::new_empty("page-log");
    assert_eq!(rig.post_empty("/approve/setup-code", Some(ORIGIN)).0, 200);
    let code = rig.last_code();
    assert_eq!(
        rig.post(
            "/approve/register",
            Some(ORIGIN),
            register_body(&key, Some(&code))
        )
        .0,
        200
    );
    let logs = rig.logs.lock().expect("logs");
    assert_eq!(
        logs.as_slice(),
        ["arca: passkey trusted, 1 passkey(s) kept"]
    );
    assert!(!logs[0].contains(&code));
    assert!(!logs[0].contains(&key.cred_text()));
    assert!(
        !logs[0]
            .as_bytes()
            .windows(8)
            .any(|digits| digits.iter().all(u8::is_ascii_digit))
    );
}

#[test]
fn failed_display_leaves_no_valid_code_and_later_issuance_works() {
    let (rig, key) = Rig::new_empty("page-no-display");
    rig.display.fail.store(true, Ordering::SeqCst);
    assert_eq!(rig.post_empty("/approve/setup-code", Some(ORIGIN)).0, 503);
    let (status, body) = rig.post(
        "/approve/register",
        Some(ORIGIN),
        register_body(&key, Some("11111111")),
    );
    assert_eq!(status, 403);
    assert!(String::from_utf8_lossy(&body).contains("The setup code expired"));
    assert!(!rig.state_path().exists());
    assert_eq!(rig.kernel.lock().expect("kernel").owner_passkey_count(), 0);
    rig.display.fail.store(false, Ordering::SeqCst);
    rig.advance(60_000);
    assert_eq!(rig.post_empty("/approve/setup-code", Some(ORIGIN)).0, 200);
    assert_eq!(
        rig.post(
            "/approve/register",
            Some(ORIGIN),
            register_body(&key, Some(&rig.last_code()))
        )
        .0,
        200
    );
}

#[test]
fn locked_caller_is_refused_but_another_caller_enrols_with_the_same_live_code() {
    let (rig, key) = Rig::new_empty("page-callers");
    assert_eq!(
        rig.proxy_post(Some("caller-a"), "/approve/setup-code", json!({}))
            .0,
        200
    );
    let code = rig.last_code();
    let wrong = register_body(&key, Some(wrong_code(&code)));
    // The rightmost non-empty token wins; spoofing earlier tokens cannot change A's budget.
    for attempt in 0..6 {
        let (status, _) = rig.proxy_post(
            Some("caller-b, caller-a,  "),
            "/approve/register",
            wrong.clone(),
        );
        assert_eq!(status, if attempt < 4 { 403 } else { 429 });
    }
    assert_eq!(
        rig.proxy_post(Some("caller-a"), "/approve/setup-code", json!({}))
            .0,
        429
    );
    let (status, body) = rig.proxy_post(Some("caller-b"), "/approve/register", wrong.clone());
    assert_eq!(status, 403);
    assert!(String::from_utf8_lossy(&body).contains("wrong"));
    let (status, body) = rig.proxy_post(Some("caller-b"), "/approve/setup-code", json!({}));
    assert_eq!(status, 429);
    let (message, wait) = rate_answer(&body);
    assert_eq!(message, "A code is already on your screen");
    assert!(wait.is_some_and(|seconds| (1..=600).contains(&seconds)));
    let (status, body) = rig.proxy_post(
        Some("caller-a"),
        "/approve/register",
        register_body(&key, Some(&code)),
    );
    assert_eq!(
        status, 429,
        "the correct code cannot bypass the caller lock"
    );
    assert_eq!(
        rate_answer(&body),
        (
            "Setup is locked for 60 more minute(s)".to_string(),
            Some(3600)
        )
    );
    assert!(!rig.state_path().exists());
    assert_eq!(rig.kernel.lock().expect("kernel").owner_passkey_count(), 0);
    assert_eq!(
        rig.proxy_post(
            Some("caller-b"),
            "/approve/register",
            register_body(&key, Some(&code))
        )
        .0,
        200,
        "the caller lock does not invalidate the live code for another caller"
    );
}

#[test]
fn another_caller_can_issue_after_a_callers_live_code_expires() {
    let (rig, key) = Rig::new_empty("page-other-issue");
    assert_eq!(
        rig.proxy_post(Some("caller-a"), "/approve/setup-code", json!({}))
            .0,
        200
    );
    let code = rig.last_code();
    for _ in 0..6 {
        rig.proxy_post(
            Some("caller-a"),
            "/approve/register",
            register_body(&key, Some(wrong_code(&code))),
        );
    }
    rig.advance(10 * 60 * 1000);
    assert_eq!(
        rig.proxy_post(Some("caller-b"), "/approve/setup-code", json!({}))
            .0,
        200
    );
    assert_eq!(
        rig.proxy_post(
            Some("caller-b"),
            "/approve/register",
            register_body(&key, Some(&rig.last_code()))
        )
        .0,
        200
    );
}

#[test]
fn fifty_wrong_tries_lock_global_issuance_and_refuse_the_correct_code() {
    let (rig, key) = Rig::new_empty("page-global-lock");
    // Issue on the direct router and attack on the proxy router: the global lock covers both.
    assert_eq!(rig.post_empty("/approve/setup-code", Some(ORIGIN)).0, 200);
    let code = rig.last_code();
    for caller in 0..10 {
        for attempt in 0..5 {
            let status = rig
                .proxy_post(
                    Some(&format!("caller-{caller}")),
                    "/approve/register",
                    register_body(&key, Some(wrong_code(&code))),
                )
                .0;
            assert_eq!(status, if attempt < 4 { 403 } else { 429 });
        }
    }
    for proxy in [false, true] {
        let (status, body) = rig.request_on(
            proxy,
            Some("fresh-caller"),
            "POST",
            "/approve/setup-code",
            Some(ORIGIN),
            b"{}",
        );
        assert_eq!(status, 429);
        assert!(String::from_utf8_lossy(&body).contains("Setup is locked for 60 more minute(s)"));
    }
    assert_eq!(
        rig.proxy_post(
            Some("fresh-caller"),
            "/approve/register",
            register_body(&key, Some(wrong_code(&code)))
        )
        .0,
        429
    );
    let body = serde_json::to_vec(&register_body(&key, Some(&code))).expect("body");
    for proxy in [false, true] {
        let (status, body) = rig.request_on(
            proxy,
            Some("fresh-caller"),
            "POST",
            "/approve/register",
            Some(ORIGIN),
            &body,
        );
        assert_eq!(
            status, 429,
            "the correct code cannot bypass the global lock"
        );
        assert!(String::from_utf8_lossy(&body).contains("Setup is locked for 60 more minute(s)"));
    }
    assert!(!rig.state_path().exists());
    assert_eq!(rig.kernel.lock().expect("kernel").owner_passkey_count(), 0);
}

#[test]
fn direct_router_ignores_forwarded_addresses_and_proxy_unknown_is_separate() {
    let (rig, key) = Rig::new_empty("page-direct-caller");
    assert_eq!(rig.post_empty("/approve/setup-code", Some(ORIGIN)).0, 200);
    let code = rig.last_code();
    let body = serde_json::to_vec(&register_body(&key, Some(wrong_code(&code)))).expect("body");
    for attempt in 0..6 {
        let status = rig
            .request_on(
                false,
                Some(&format!("spoof-{attempt}")),
                "POST",
                "/approve/register",
                Some(ORIGIN),
                &body,
            )
            .0;
        assert_eq!(status, if attempt < 4 { 403 } else { 429 });
    }
    for caller in [None, Some(" , , ")] {
        assert_eq!(
            rig.proxy_post(
                caller,
                "/approve/register",
                register_body(&key, Some(wrong_code(&code)))
            )
            .0,
            403
        );
    }
    assert_eq!(
        rig.proxy_post(None, "/approve/register", register_body(&key, Some(&code)))
            .0,
        200
    );
}

#[test]
fn setup_code_waits_for_the_kernel_then_checks_the_trusted_count() {
    let (rig, key) = Rig::new_empty("page-kernel-lock");
    let mut kernel = rig.kernel.lock().expect("kernel");
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let rig = &rig;
        scope.spawn(move || {
            started_tx.send(()).expect("started");
            done_tx
                .send(rig.post_empty("/approve/setup-code", Some(ORIGIN)))
                .expect("done");
        });
        started_rx.recv().expect("request thread started");
        assert!(matches!(
            done_rx.recv_timeout(Duration::from_millis(100)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        kernel.replace_owner_passkeys(
            parse_passkeys(key.line(NAMESPACE).as_bytes()).expect("key line"),
        );
        drop(kernel);
        let (status, body) = done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("response");
        assert_eq!(status, 409);
        assert!(String::from_utf8_lossy(&body).contains("A passkey is already trusted"));
    });
    assert!(rig.display.codes.lock().expect("display").is_empty());
}

#[test]
fn repeated_registration_with_fresh_codes_keeps_one_parseable_line() {
    let (rig, key) = Rig::new_empty("page-repeat-register");
    for _ in 0..2 {
        assert_eq!(rig.post_empty("/approve/setup-code", Some(ORIGIN)).0, 200);
        assert_eq!(
            rig.post(
                "/approve/register",
                Some(ORIGIN),
                register_body(&key, Some(&rig.last_code()))
            )
            .0,
            200
        );
        let bytes = fs::read(rig.state_path()).expect("file");
        assert_eq!(parse_passkeys(&bytes).expect("parseable file").len(), 1);
        assert_eq!(
            bytes
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
                .count(),
            1
        );
        // Model a subsequent zero-owner verifier state while retaining the state file for a retry.
        rig.kernel
            .lock()
            .expect("kernel")
            .replace_owner_passkeys(Vec::new());
        rig.advance(60_000);
    }
}

#[test]
fn refusing_verifier_rolls_back_existing_bytes_and_removes_a_new_file() {
    let (rig, key) = Rig::build_with_refusal("page-refusing", true, false, true);
    let original = format!(
        "# existing bytes without a trailing newline\n{}",
        key.line(NAMESPACE)
    );
    for existing in [false, true] {
        if existing {
            fs::write(rig.state_path(), &original).expect("existing file");
            fs::set_permissions(rig.state_path(), fs::Permissions::from_mode(0o600)).expect("mode");
        }
        for _ in 0..2 {
            assert_eq!(rig.post_empty("/approve/setup-code", Some(ORIGIN)).0, 200);
            let code = rig.last_code();
            let (status, body) = rig.post(
                "/approve/register",
                Some(ORIGIN),
                register_body(&key, Some(&code)),
            );
            assert_eq!(status, 500);
            assert!(String::from_utf8_lossy(&body).contains("The passkey could not be trusted"));
            assert_eq!(rig.kernel.lock().expect("kernel").owner_passkey_count(), 0);
            if existing {
                let bytes = fs::read(rig.state_path()).expect("existing bytes");
                assert!(bytes == original.as_bytes());
                assert_eq!(parse_passkeys(&bytes).expect("parseable file").len(), 1);
            } else {
                assert!(!rig.state_path().exists());
            }
            rig.advance(10 * 60 * 1000);
        }
    }
    assert!(rig.logs.lock().expect("logs").is_empty());
}

#[test]
fn canvas_signed_out_gets_the_sign_in_page_and_no_snapshot_bytes() {
    let (rig, _) = Rig::new("canvas-signed-out", true);
    let (status, head, body) = rig.get_full("/approve/canvas", None);
    assert_eq!(status, 200);
    assert!(String::from_utf8_lossy(&body).contains("Sign in with passkey"));
    assert_eq!(
        header_value(&head, "content-type").as_deref(),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(
        header_value(&head, "cache-control").as_deref(),
        Some("no-store")
    );
    for path in [
        "/approve/canvas/page.css",
        "/approve/canvas/page.js",
        "/approve/canvas/snapshot",
    ] {
        let (status, _, body) = rig.get_full(path, None);
        assert_eq!(status, 401, "{path} without a session");
        assert!(body.is_empty(), "{path} leaked bytes without a session");
    }
    let (status, head, _) = rig.get_full("/approve/canvas/extra", None);
    assert_eq!(status, 404, "only the exact canvas URLs exist");
    assert!(
        header_value(&head, "content-security-policy").is_some(),
        "a refused /approve path still carries the headers"
    );
    let (status, _, _) = rig.get_full("/approve/canvas/page.css/..", None);
    assert_eq!(status, 404, "a traversal path is refused");
    let (status, _, _) = rig.get_full("/approve/canvas/../plans", None);
    assert_eq!(status, 404, "a parent traversal is refused");
}

#[test]
fn canvas_sign_in_spends_the_challenge_and_sets_the_host_cookie() {
    let (rig, key) = Rig::new("canvas-sign-in", true);
    let (status, _, body) = rig.post_full("/approve/canvas", Some(ORIGIN), json!({}));
    assert_eq!(status, 200);
    let issued: Value = serde_json::from_slice(&body).expect("challenge JSON");
    let statement = issued["statement"]
        .as_str()
        .expect("a statement")
        .to_string();
    let assertion = Assert::over(NAMESPACE, statement.as_bytes());
    let request = json!({
        "statement": statement,
        "proof": key.proof_value(&assertion),
    });
    let (status, head, body) = rig.post_full("/approve/canvas", Some(ORIGIN), request.clone());
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let cookie = set_cookie(&head);
    assert!(cookie.starts_with("__Host-arca-canvas="));
    let set = header_value(&head, "set-cookie").expect("a Set-Cookie");
    for attribute in [
        "Path=/",
        "Secure",
        "HttpOnly",
        "SameSite=Strict",
        "Max-Age=43200",
    ] {
        assert!(set.contains(attribute), "the cookie carries {attribute}");
    }
    assert!(!set.contains("Domain="));

    let (status, head, body) = rig.get_with_cookie("/approve/canvas", &cookie);
    assert_eq!(status, 200);
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("data-arca-base=\"/approve/canvas\""));
    assert_eq!(
        header_value(&head, "content-type").as_deref(),
        Some("text/html; charset=utf-8")
    );

    let (status, head, body) = rig.get_with_cookie("/approve/canvas/snapshot", &cookie);
    assert_eq!(status, 200);
    assert_eq!(body, b"{\"instances\":[],\"devices\":\"ok\"}");
    assert_eq!(
        header_value(&head, "content-type").as_deref(),
        Some("application/json; charset=utf-8")
    );
    assert_eq!(
        header_value(&head, "cache-control").as_deref(),
        Some("no-store")
    );
    assert!(header_value(&head, "set-cookie").is_none());
    assert!(header_value(&head, "location").is_none());
    assert!(!head.contains("upstream=bad"));
    assert!(!head.contains("max-age=120"));

    // The challenge is single use, so the same signed request is refused.
    let (status, _, _) = rig.post_full("/approve/canvas", Some(ORIGIN), request);
    assert_eq!(status, 403, "a spent challenge is refused");
}

#[test]
fn canvas_rejects_a_request_without_a_live_challenge_before_verifying() {
    let (rig, key) = Rig::new("canvas-no-live", true);
    let statement = sign_in_statement(ORIGIN, &"ab".repeat(32), NOW);
    let mut assertion = Assert::over(NAMESPACE, statement.as_bytes());
    assertion.sign_count = 1;
    let proof = key.proof_value(&assertion);
    let counts = rig.state_dir.join(PASSKEY_COUNTS_FILE);
    let before = fs::read(&counts).unwrap_or_default();
    let (status, _, _) = rig.post_full(
        "/approve/canvas",
        Some(ORIGIN),
        json!({ "statement": statement, "proof": proof }),
    );
    assert_eq!(status, 403);
    let after = fs::read(&counts).unwrap_or_default();
    assert_eq!(
        before, after,
        "no signature verification ran without a challenge"
    );
    assert!(before.is_empty());
}

#[test]
fn canvas_spends_the_challenge_before_verifying() {
    let (rig, key) = Rig::new("canvas-spend-first", true);
    let (status, _, body) = rig.post_full("/approve/canvas", Some(ORIGIN), json!({}));
    assert_eq!(status, 200);
    let issued: Value = serde_json::from_slice(&body).expect("challenge JSON");
    let statement = issued["statement"]
        .as_str()
        .expect("a statement")
        .to_string();
    let mut wrong = Assert::over(NAMESPACE, statement.as_bytes());
    wrong.origin = Some("https://elsewhere.example".to_string());
    let (status, _, _) = rig.post_full(
        "/approve/canvas",
        Some(ORIGIN),
        json!({ "statement": statement, "proof": key.proof_value(&wrong) }),
    );
    assert_eq!(status, 403, "the wrong-origin assertion is refused");
    let good = Assert::over(NAMESPACE, statement.as_bytes());
    let (status, _, _) = rig.post_full(
        "/approve/canvas",
        Some(ORIGIN),
        json!({ "statement": statement, "proof": key.proof_value(&good) }),
    );
    assert_eq!(
        status, 403,
        "the challenge was spent before the failed verification"
    );
}

#[test]
fn canvas_expired_and_plan_statements_are_refused() {
    let (rig, key) = Rig::new("canvas-expired", true);
    let (status, _, body) = rig.post_full("/approve/canvas", Some(ORIGIN), json!({}));
    assert_eq!(status, 200);
    let issued: Value = serde_json::from_slice(&body).expect("challenge JSON");
    let statement = issued["statement"]
        .as_str()
        .expect("a statement")
        .to_string();
    rig.advance(arca::canvas::CHALLENGE_TTL_MS + 1);
    let assertion = Assert::over(NAMESPACE, statement.as_bytes());
    let (status, _, _) = rig.post_full(
        "/approve/canvas",
        Some(ORIGIN),
        json!({ "statement": statement, "proof": key.proof_value(&assertion) }),
    );
    assert_eq!(status, 403, "an expired statement is refused");

    let (_plan, plan_statement) = rig.waiting_plan();
    let assertion = Assert::over(NAMESPACE, plan_statement.as_bytes());
    let (status, _, _) = rig.post_full(
        "/approve/canvas",
        Some(ORIGIN),
        json!({ "statement": plan_statement, "proof": key.proof_value(&assertion) }),
    );
    assert_eq!(status, 403, "an approval statement is not a sign-in");
    assert!(
        arca_approval::parse_statement(sign_in_statement(ORIGIN, &"cd".repeat(32), NOW).as_bytes())
            .is_err(),
        "the approval parser refuses a sign-in statement"
    );
}

#[test]
fn canvas_forged_and_expired_sessions_fail_closed() {
    let (rig, _) = Rig::new("canvas-session", true);
    let cookie = rig
        .canvas
        .issue_session("passkey:test", NOW)
        .expect("a session cookie");
    assert!(cookie.contains("SameSite=Strict"));
    let pair = cookie.split(';').next().expect("cookie pair").to_string();
    assert_eq!(
        rig.get_with_cookie("/approve/canvas/snapshot", &pair).0,
        200
    );
    rig.advance(arca::canvas::SESSION_TTL_MS + 1);
    assert_eq!(
        rig.get_with_cookie("/approve/canvas/snapshot", &pair).0,
        401
    );
    let forged = format!("__Host-arca-canvas={}", "00".repeat(32));
    let (status, _, body) = rig.get_with_cookie("/approve/canvas", &forged);
    assert_eq!(status, 200);
    assert!(String::from_utf8_lossy(&body).contains("Sign in with passkey"));
    assert_eq!(
        rig.get_with_cookie("/approve/canvas/snapshot", &forged).0,
        401
    );
}

#[test]
fn canvas_headers_and_csp_cover_approval_and_canvas_urls() {
    let (rig, _) = Rig::new("canvas-headers", true);
    let cookie = rig
        .canvas
        .issue_session("passkey:test", NOW)
        .expect("a session cookie");
    let pair = cookie.split(';').next().expect("cookie pair").to_string();
    for (path, signed) in [
        ("/approve", false),
        ("/approve/plans", false),
        ("/approve/canvas", false),
        ("/approve/canvas/page.css", true),
        ("/approve/canvas/page.js", true),
        ("/approve/canvas/snapshot", true),
        (CHAT_HISTORY_PATH, true),
    ] {
        let (status, head, _) = if signed {
            rig.get_with_cookie(path, &pair)
        } else {
            rig.get_full(path, None)
        };
        assert_eq!(status, 200, "{path} rendered");
        let csp = header_value(&head, "content-security-policy").expect("a CSP");
        assert!(csp.contains("default-src 'none'"), "{path}");
        assert!(csp.contains("script-src 'self'"), "{path}");
        assert!(csp.contains("sha256-"), "{path}");
        assert!(csp.contains("style-src 'self'"), "{path}");
        assert!(csp.contains("connect-src 'self'"), "{path}");
        assert!(!csp.contains("unsafe-inline"), "{path}");
        assert!(csp.contains("frame-ancestors 'none'"), "{path}");
        assert_eq!(
            header_value(&head, "x-content-type-options").as_deref(),
            Some("nosniff")
        );
        assert_eq!(
            header_value(&head, "referrer-policy").as_deref(),
            Some("no-referrer")
        );
        assert_eq!(
            header_value(&head, "permissions-policy").as_deref(),
            Some("publickey-credentials-get=(self)"),
            "{path}: WebAuthn get() is allowed for this origin"
        );
    }
    let (status, head, _) = rig.get_full("/approve/canvas", Some("https://elsewhere.example"));
    assert_eq!(status, 403, "a foreign Origin is refused");
    assert!(
        header_value(&head, "content-security-policy").is_some(),
        "the refusal carries the same headers"
    );
}

#[test]
fn canvas_dsh_error_and_timeout_show_the_arca_error_page() {
    let (rig, _) = Rig::new("canvas-upstream", true);
    let cookie = rig
        .canvas
        .issue_session("passkey:test", NOW)
        .expect("a session cookie");
    let pair = cookie.split(';').next().expect("cookie pair").to_string();
    rig.upstream.set_mode(UpstreamMode::Error);
    let (status, head, body) = rig.get_with_cookie("/approve/canvas/snapshot", &pair);
    assert_eq!(status, 502);
    assert!(String::from_utf8_lossy(&body).contains("The canvas is not running"));
    assert_eq!(
        header_value(&head, "content-type").as_deref(),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(
        header_value(&head, "cache-control").as_deref(),
        Some("no-store")
    );
    rig.upstream.set_mode(UpstreamMode::Timeout);
    let (status, _, body) = rig.get_with_cookie("/approve/canvas/snapshot", &pair);
    assert_eq!(status, 502);
    assert!(String::from_utf8_lossy(&body).contains("The canvas is not running"));
}

#[test]
fn canvas_chat_needs_a_session_before_any_upstream_connect() {
    let (rig, _) = Rig::new("canvas-chat-401", true);
    let body = chat_json();
    let (status, head, response) = rig.request_full_on(
        false,
        None,
        "POST",
        CHAT_PATH,
        Some(ORIGIN),
        &body,
        None,
        &[],
    );
    assert_eq!(status, 401, "a chat turn needs the canvas session");
    assert!(response.is_empty(), "an unsigned refusal returns no bytes");
    assert_eq!(
        header_value(&head, "cache-control").as_deref(),
        Some("no-store")
    );
    assert!(header_value(&head, "content-security-policy").is_some());
    let forged = format!("__Host-arca-canvas={}", "00".repeat(32));
    let (status, _, response) = rig.request_full_on(
        false,
        None,
        "POST",
        CHAT_PATH,
        Some(ORIGIN),
        &body,
        Some(&forged),
        &[],
    );
    assert_eq!(status, 401, "an invalid session is the same refusal");
    assert!(response.is_empty());
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        rig.upstream.connections(),
        0,
        "a refused chat turn never connects upstream"
    );
    assert!(rig.upstream.chat_requests().is_empty());
}

#[test]
fn canvas_chat_refuses_a_foreign_origin_before_any_upstream_connect() {
    let (rig, _) = Rig::new("canvas-chat-csrf", true);
    let cookie = canvas_cookie(&rig);
    let body = chat_json();
    for (origin, label) in [
        (None, "no Origin"),
        (Some("https://elsewhere.example"), "a foreign Origin"),
        (Some("http://box1.example.com"), "a different scheme"),
    ] {
        let (status, head, response) = rig.request_full_on(
            false,
            None,
            "POST",
            CHAT_PATH,
            origin,
            &body,
            Some(&cookie),
            &[],
        );
        assert_eq!(status, 403, "{label} is refused");
        assert!(response.is_empty(), "{label} returns no bytes");
        assert!(
            header_value(&head, "content-security-policy").is_some(),
            "{label} keeps the canvas headers"
        );
    }
    let (status, _, _) = rig.request_full_on(
        false,
        None,
        "POST",
        CHAT_PATH,
        None,
        &body,
        Some(&cookie),
        &[("Sec-Fetch-Site", "cross-site")],
    );
    assert_eq!(status, 403, "a cross-site fetch is refused");
    let (status, _, _) = rig.request_full_on(
        false,
        None,
        "POST",
        CHAT_PATH,
        Some("https://elsewhere.example"),
        &body,
        Some(&cookie),
        &[("Sec-Fetch-Site", "same-origin")],
    );
    assert_eq!(
        status, 403,
        "same-origin fetch metadata never overrides a foreign Origin"
    );
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        rig.upstream.connections(),
        0,
        "no refused chat turn connects upstream"
    );
}

#[test]
fn canvas_chat_refuses_non_json_and_oversize_before_any_upstream_connect() {
    let (rig, _) = Rig::new("canvas-chat-body", true);
    let cookie = canvas_cookie(&rig);

    let (status, head, _) = rig.request_full_on(
        false,
        None,
        "POST",
        CHAT_PATH,
        Some(ORIGIN),
        &chat_json(),
        Some(&cookie),
        &[("Content-Type", "text/plain")],
    );
    assert_eq!(status, 415, "a non-JSON media type is refused");
    assert_eq!(
        header_value(&head, "cache-control").as_deref(),
        Some("no-store")
    );

    let (status, _, _) = rig.request_full_on(
        false,
        None,
        "POST",
        CHAT_PATH,
        Some(ORIGIN),
        b"{not json",
        Some(&cookie),
        &[],
    );
    assert_eq!(status, 400, "a body that is not JSON is refused");

    let (status, _, _) = rig.request_full_on(
        false,
        None,
        "POST",
        CHAT_PATH,
        Some(ORIGIN),
        &[],
        Some(&cookie),
        &[],
    );
    assert_eq!(status, 415, "a body with no JSON media type is refused");

    let mut oversize = Vec::with_capacity(CHAT_BODY_MAX + 1);
    oversize.extend_from_slice(b"{\"message\":\"");
    oversize.resize(CHAT_BODY_MAX + 1, b'a');
    let bytes = rig.raw_request("POST", CHAT_PATH, None, Some(ORIGIN), &oversize, None, &[]);
    let (status, _, _) = rig.raw_request_response(&bytes);
    assert_eq!(
        status, 401,
        "the session is checked before an oversize body is read"
    );
    let bytes = rig.raw_request(
        "POST",
        CHAT_PATH,
        None,
        Some(ORIGIN),
        &oversize,
        Some(&cookie),
        &[],
    );
    let (status, head, _) = rig.raw_request_response(&bytes);
    assert_eq!(status, 413, "an oversize turn is refused");
    assert_eq!(
        header_value(&head, "cache-control").as_deref(),
        Some("no-store")
    );
    assert!(header_value(&head, "content-security-policy").is_some());

    let first = [b'x'; 40 * 1024];
    let second = [b'y'; 40 * 1024];
    let (status, head, _) =
        rig.chunked_request(CHAT_PATH, &cookie, &[first.as_slice(), second.as_slice()]);
    assert_eq!(status, 413, "an oversize chunked turn is refused");
    assert_eq!(
        header_value(&head, "cache-control").as_deref(),
        Some("no-store")
    );
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        rig.upstream.connections(),
        0,
        "no refused chat turn connects upstream"
    );
}

#[test]
fn canvas_chat_relays_the_stream_chunk_by_chunk() {
    let (rig, _) = Rig::new("canvas-chat-stream", true);
    let cookie = canvas_cookie(&rig);
    rig.upstream.set_mode(UpstreamMode::ChatStream);
    let body = chat_json();
    let started = Instant::now();
    // The browser alternative to an Origin: same-origin fetch metadata is enough for the chat POST.
    let (mut stream, status, head, initial) = rig.open_stream_on(
        "POST",
        CHAT_PATH,
        None,
        &body,
        Some(&cookie),
        &[("Sec-Fetch-Site", "same-origin")],
    );
    assert_eq!(status, 200);
    assert_eq!(
        header_value(&head, "content-type").as_deref(),
        Some("text/event-stream")
    );
    assert_eq!(
        header_value(&head, "cache-control").as_deref(),
        Some("no-store")
    );
    assert_eq!(
        header_value(&head, "x-accel-buffering").as_deref(),
        Some("no")
    );
    assert_eq!(
        header_value(&head, "transfer-encoding").as_deref(),
        Some("chunked")
    );
    for name in [
        "content-security-policy",
        "x-content-type-options",
        "referrer-policy",
    ] {
        assert!(header_value(&head, name).is_some(), "the stream has {name}");
    }
    assert!(!head.contains("upstream=bad"));
    assert!(!head.contains("max-age=120"));

    let body_so_far = read_until(
        &mut stream,
        initial,
        "data: first",
        started + Duration::from_millis(300),
    );
    assert!(
        started.elapsed() < Duration::from_millis(300),
        "the first event arrived while the upstream still held the second"
    );
    let mut rest = body_so_far;
    stream.read_to_end(&mut rest).expect("the streamed body");
    let text = String::from_utf8_lossy(&rest);
    let events = sse_events(&rest);
    assert_eq!(
        events,
        vec![
            ("message".to_string(), "first".to_string()),
            ("message".to_string(), "second".to_string()),
        ],
        "{text}"
    );
    assert!(
        events.iter().all(|(event, _)| event != "arca-cut"),
        "a complete stream carries no cut event"
    );
    assert!(
        started.elapsed() >= Duration::from_millis(400),
        "the second event waited for the upstream"
    );

    let requests = rig.upstream.chat_requests();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0]
            .0
            .starts_with("POST /arca-canvas/chat HTTP/1.1\r\n")
    );
    assert!(
        requests[0]
            .0
            .lines()
            .any(|line| line.eq_ignore_ascii_case("content-type: application/json"))
    );
    assert_eq!(requests[0].1, body);
}

#[test]
fn canvas_chat_idle_timeout_ends_a_stalled_stream() {
    let (rig, _) = Rig::new("canvas-chat-idle", true);
    let cookie = canvas_cookie(&rig);
    rig.upstream.set_mode(UpstreamMode::ChatStall);
    let started = Instant::now();
    let (mut stream, status, _, initial) = rig.open_stream_on(
        "POST",
        CHAT_PATH,
        Some(ORIGIN),
        &chat_json(),
        Some(&cookie),
        &[],
    );
    assert_eq!(status, 200);
    let mut body = initial;
    stream.read_to_end(&mut body).expect("the streamed body");
    let elapsed = started.elapsed();
    assert!(
        String::from_utf8_lossy(&body).contains("data: first"),
        "the first event was relayed before the stall"
    );
    assert!(
        elapsed >= Duration::from_millis(550),
        "the stream lasted for the idle cap, not less"
    );
    assert!(
        elapsed < Duration::from_millis(1200),
        "the idle cap ended the stalled stream before the upstream did"
    );
    assert_cut(&body, "idle");
}

#[test]
fn canvas_chat_client_disconnect_closes_the_upstream() {
    let (rig, _) = Rig::new("canvas-chat-disconnect", true);
    let cookie = canvas_cookie(&rig);
    rig.upstream.set_mode(UpstreamMode::ChatHold);
    let (mut stream, status, _, initial) = rig.open_stream_on(
        "POST",
        CHAT_PATH,
        Some(ORIGIN),
        &chat_json(),
        Some(&cookie),
        &[],
    );
    assert_eq!(status, 200);
    let _ = read_until(
        &mut stream,
        initial,
        "data: first",
        Instant::now() + Duration::from_secs(1),
    );
    drop(stream);
    let deadline = Instant::now() + Duration::from_secs(2);
    while !rig.upstream.closed() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        rig.upstream.closed(),
        "dropping the client socket closed the upstream connection"
    );
}

#[test]
fn canvas_chat_cancel_while_the_upstream_is_silent_closes_it() {
    let (rig, _) = Rig::new("canvas-chat-silent-cancel", true);
    let cookie = canvas_cookie(&rig);
    rig.upstream.set_mode(UpstreamMode::ChatSilent);
    let (stream, status, _, _) = rig.open_stream_on(
        "POST",
        CHAT_PATH,
        Some(ORIGIN),
        &chat_json(),
        Some(&cookie),
        &[],
    );
    assert_eq!(status, 200);
    drop(stream);
    let deadline = Instant::now() + Duration::from_secs(2);
    while !rig.upstream.closed() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        rig.upstream.closed(),
        "a cancel while the upstream was silent closed it"
    );
}

#[test]
fn canvas_chat_rejects_other_methods() {
    let (rig, _) = Rig::new("canvas-chat-methods", true);
    let cookie = canvas_cookie(&rig);
    for method in ["GET", "HEAD", "PUT", "PATCH", "DELETE"] {
        let (status, head, body) = rig.request_full_on(
            false,
            None,
            method,
            CHAT_PATH,
            Some(ORIGIN),
            &[],
            Some(&cookie),
            &[],
        );
        assert_eq!(status, 405, "{method} on the chat path");
        assert!(body.is_empty(), "{method} returns no bytes");
        assert_eq!(
            header_value(&head, "cache-control").as_deref(),
            Some("no-store")
        );
    }
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        rig.upstream.connections(),
        0,
        "a refused method never connects upstream"
    );
}

#[test]
fn canvas_chat_upstream_non_200_is_a_bad_gateway() {
    let (rig, _) = Rig::new("canvas-chat-upstream-status", true);
    let cookie = canvas_cookie(&rig);
    rig.upstream.set_mode(UpstreamMode::Error);
    let (status, head, body) = rig.request_full_on(
        false,
        None,
        "POST",
        CHAT_PATH,
        Some(ORIGIN),
        &chat_json(),
        Some(&cookie),
        &[],
    );
    assert_eq!(status, 502, "a non-200 upstream is a bad gateway");
    assert!(body.is_empty(), "no upstream bytes cross the refusal");
    assert_eq!(
        header_value(&head, "cache-control").as_deref(),
        Some("no-store")
    );
    assert!(header_value(&head, "content-security-policy").is_some());
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(rig.upstream.connections(), 1);
}

#[test]
fn canvas_chat_upstream_end_and_decode_errors_are_cut() {
    for (tag, mode) in [
        ("canvas-chat-truncate", UpstreamMode::ChatTruncate),
        ("canvas-chat-corrupt", UpstreamMode::ChatCorrupt),
    ] {
        let (rig, _) = Rig::new(tag, true);
        let cookie = canvas_cookie(&rig);
        rig.upstream.set_mode(mode);
        let (mut stream, status, _, initial) = rig.open_stream_on(
            "POST",
            CHAT_PATH,
            Some(ORIGIN),
            &chat_json(),
            Some(&cookie),
            &[],
        );
        assert_eq!(status, 200);
        let mut body = initial;
        stream.read_to_end(&mut body).expect("the streamed body");
        assert_cut(&body, "upstream");
    }
}

#[test]
fn canvas_chat_stream_cap_is_cut() {
    let (rig, _) = Rig::new("canvas-chat-cap", true);
    let cookie = canvas_cookie(&rig);
    rig.upstream.set_mode(UpstreamMode::ChatFlood);
    let (mut stream, status, _, initial) = rig.open_stream_on(
        "POST",
        CHAT_PATH,
        Some(ORIGIN),
        &chat_json(),
        Some(&cookie),
        &[],
    );
    assert_eq!(status, 200);
    let mut body = initial;
    stream.read_to_end(&mut body).expect("the streamed body");
    let reason = cut_reason(&body);
    assert!(
        matches!(reason.as_str(), "cap" | "full"),
        "a flood is cut by the byte cap or by the full relay queue, not {reason}"
    );
    assert!(
        body.len() < CHAT_STREAM_MAX + 64 * 1024,
        "the cut stopped the upstream stream before its cap"
    );
}

#[test]
fn canvas_chat_total_deadline_covers_head_and_body() {
    let (rig, _) = Rig::with_chat_timeouts(
        "canvas-chat-total",
        Duration::from_secs(5),
        Duration::from_millis(1200),
    );
    let cookie = canvas_cookie(&rig);
    rig.upstream.set_mode(UpstreamMode::ChatDrip);
    let started = Instant::now();
    let (mut stream, status, _, initial) = rig.open_stream_on(
        "POST",
        CHAT_PATH,
        Some(ORIGIN),
        &chat_json(),
        Some(&cookie),
        &[],
    );
    assert_eq!(status, 200);
    let mut body = initial;
    stream.read_to_end(&mut body).expect("the streamed body");
    let elapsed = started.elapsed();
    assert_cut(&body, "total");
    assert!(
        elapsed >= Duration::from_millis(1100),
        "the total deadline did not fire early"
    );
    assert!(
        elapsed < Duration::from_millis(1600),
        "the head and body shared one total deadline"
    );
}

#[test]
fn canvas_chat_concurrency_is_capped_and_other_pages_still_answer() {
    let (rig, _) = Rig::new("canvas-chat-slots", true);
    let cookie = canvas_cookie(&rig);
    rig.upstream.set_mode(UpstreamMode::ChatHold);
    let mut held = Vec::new();
    for index in 0..CHAT_CONCURRENCY {
        let (stream, status, _, _) = rig.open_stream_on(
            "POST",
            CHAT_PATH,
            Some(ORIGIN),
            &chat_json(),
            Some(&cookie),
            &[],
        );
        assert_eq!(status, 200, "held chat {index}");
        held.push(stream);
    }
    let (status, head, body) = rig.request_full_on(
        false,
        None,
        "POST",
        CHAT_PATH,
        Some(ORIGIN),
        &chat_json(),
        Some(&cookie),
        &[],
    );
    assert_eq!(status, 429, "the extra chat is capped");
    assert!(body.is_empty(), "the capped chat returns no bytes");
    assert_eq!(
        header_value(&head, "cache-control").as_deref(),
        Some("no-store")
    );
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        rig.upstream.connections(),
        CHAT_CONCURRENCY as u64,
        "the capped chat never connected upstream"
    );
    let (status, _, _) = rig.request_full_on(false, None, "GET", "/approve", None, &[], None, &[]);
    assert_eq!(status, 200, "/approve still answers while chats are held");

    drop(held);
    rig.upstream.set_mode(UpstreamMode::ChatStream);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let (mut stream, status, _, initial) = rig.open_stream_on(
            "POST",
            CHAT_PATH,
            Some(ORIGIN),
            &chat_json(),
            Some(&cookie),
            &[],
        );
        if status == 429 {
            assert!(Instant::now() < deadline, "a chat slot was released");
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        assert_eq!(status, 200, "a released slot admits the next chat");
        let mut body = initial;
        stream.read_to_end(&mut body).expect("the streamed body");
        break;
    }
}

#[test]
fn canvas_chat_invalid_origin_is_never_treated_as_missing() {
    let (rig, _) = Rig::new("canvas-chat-bad-origin", true);
    let cookie = canvas_cookie(&rig);
    let body = chat_json();
    let mut request = format!(
        "POST {CHAT_PATH} HTTP/1.1\r\nHost: box1.example.com\r\nConnection: close\r\nContent-Length: {}\r\nCookie: {cookie}\r\nSec-Fetch-Site: same-origin\r\nOrigin: ",
        body.len()
    )
    .into_bytes();
    request.push(0xff);
    request.extend_from_slice(b"\r\n\r\n");
    request.extend_from_slice(&body);
    let (status, head, response) = rig.raw_request_response(&request);
    assert_eq!(status, 403, "a non-text Origin is not missing");
    assert!(response.is_empty());
    assert!(header_value(&head, "content-security-policy").is_some());

    let mut get =
        b"GET /approve/plans HTTP/1.1\r\nHost: box1.example.com\r\nConnection: close\r\nOrigin: "
            .to_vec();
    get.push(0xfe);
    get.extend_from_slice(b"\r\n\r\n");
    let (status, _, _) = rig.raw_request_response(&get);
    assert_eq!(status, 403, "a non-text Origin is refused on a GET too");
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        rig.upstream.connections(),
        0,
        "a bad Origin never connects upstream"
    );
}

#[test]
fn canvas_chat_history_is_gated_and_forwards_to_the_history_path() {
    let (rig, _) = Rig::new("canvas-chat-history", true);
    let (status, head, body) = rig.get_full(CHAT_HISTORY_PATH, None);
    assert_eq!(status, 401, "history needs the canvas session");
    assert!(
        body.is_empty(),
        "a signed-out history refusal returns no bytes"
    );
    assert!(header_value(&head, "content-security-policy").is_some());
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        rig.upstream.connections(),
        0,
        "signed-out history never connects upstream"
    );

    let cookie = canvas_cookie(&rig);
    let (status, head, body) = rig.get_with_cookie(CHAT_HISTORY_PATH, &cookie);
    assert_eq!(status, 200);
    assert_eq!(body, b"{\"turns\":[]}");
    assert_eq!(
        header_value(&head, "content-type").as_deref(),
        Some("application/json; charset=utf-8")
    );
    assert_eq!(
        header_value(&head, "cache-control").as_deref(),
        Some("no-store")
    );
    assert!(!head.contains("upstream=bad"));
    assert!(!head.contains("max-age=120"));
    assert_eq!(rig.upstream.connections(), 1);
}

#[test]
fn canvas_challenge_cap_is_global_and_resets_after_a_minute() {
    let (rig, _) = Rig::new("canvas-cap", true);
    for index in 0..60 {
        let (status, _, _) = rig.post_full("/approve/canvas", Some(ORIGIN), json!({}));
        assert_eq!(status, 200, "challenge {index}");
    }
    let (status, _, body) = rig.post_full("/approve/canvas", Some(ORIGIN), json!({}));
    assert_eq!(
        status, 429,
        "the sixty-first challenge in a minute is capped"
    );
    // The body names the `rate` code beside the sentence, and tells no wait.
    assert_eq!(
        rate_answer(&body),
        ("Too many sign-in challenges".to_string(), None)
    );
    rig.advance(60_000);
    let (status, _, _) = rig.post_full("/approve/canvas", Some(ORIGIN), json!({}));
    assert_eq!(status, 200, "the cap clears after the minute");
}

#[test]
fn canvas_failure_logs_name_only_the_class_and_do_not_lock_the_next_sign_in() {
    let (rig, key) = Rig::new("canvas-log", true);
    let (status, _, body) = rig.post_full("/approve/canvas", Some(ORIGIN), json!({}));
    assert_eq!(status, 200);
    let issued: Value = serde_json::from_slice(&body).expect("challenge JSON");
    let statement = issued["statement"]
        .as_str()
        .expect("a statement")
        .to_string();
    let mut wrong = Assert::over(NAMESPACE, statement.as_bytes());
    wrong.origin = Some("https://elsewhere.example".to_string());
    let proof = key.proof_value(&wrong);
    let (status, _, _) = rig.post_full(
        "/approve/canvas",
        Some(ORIGIN),
        json!({ "statement": &statement, "proof": &proof }),
    );
    assert_eq!(status, 403);
    let logs = rig.logs.lock().expect("logs").clone();
    assert_eq!(
        logs.last().map(String::as_str),
        Some("arca: canvas sign-in failed: WrongOrigin (credential known)")
    );
    let lines = logs.join("\n");
    assert!(!lines.contains(&statement));
    assert!(!lines.contains(&serde_json::to_string(&proof).expect("proof JSON")));
    assert!(!lines.contains(&key.cred_text()));
    assert!(!lines.contains(&key.key_text()));

    let (status, _, body) = rig.post_full("/approve/canvas", Some(ORIGIN), json!({}));
    assert_eq!(status, 200);
    let issued: Value = serde_json::from_slice(&body).expect("challenge JSON");
    let statement = issued["statement"]
        .as_str()
        .expect("a statement")
        .to_string();
    let mut unknown = Assert::over(NAMESPACE, statement.as_bytes());
    unknown.credential_id = Some(vec![0x99; 20]);
    let proof = key.proof_value(&unknown);
    let (status, _, _) = rig.post_full(
        "/approve/canvas",
        Some(ORIGIN),
        json!({ "statement": &statement, "proof": &proof }),
    );
    assert_eq!(status, 403);
    let logs = rig.logs.lock().expect("logs").clone();
    assert_eq!(
        logs.last().map(String::as_str),
        Some("arca: canvas sign-in failed: BadSignature (credential unknown)")
    );
    let lines = logs.join("\n");
    assert!(!lines.contains(&statement));
    assert!(!lines.contains(&serde_json::to_string(&proof).expect("proof JSON")));
    assert!(!lines.contains(&key.cred_text()));
    assert!(!lines.contains(&key.key_text()));

    let (status, _, body) = rig.post_full("/approve/canvas", Some(ORIGIN), json!({}));
    assert_eq!(status, 200, "no failure lock");
    let issued: Value = serde_json::from_slice(&body).expect("challenge JSON");
    let statement = issued["statement"]
        .as_str()
        .expect("a statement")
        .to_string();
    let good = Assert::over(NAMESPACE, statement.as_bytes());
    let (status, _, _) = rig.post_full(
        "/approve/canvas",
        Some(ORIGIN),
        json!({ "statement": statement, "proof": key.proof_value(&good) }),
    );
    assert_eq!(status, 200, "a fresh challenge still signs in");
}

#[test]
fn the_canvas_page_names_assets_that_load_whichever_way_the_page_was_asked_for() {
    let (rig, _) = Rig::new("canvas-assets", true);
    let cookie = rig
        .canvas
        .issue_session("passkey:test", NOW)
        .expect("a session cookie");
    let pair = cookie.split(';').next().expect("cookie pair").to_string();
    // What a browser asks for when a page at `page` names `reference`: an absolute path as it is, and a relative one
    // under the page's own directory, which is where the two spellings of the page differ.
    let resolve = |page: &str, reference: &str| -> String {
        if reference.starts_with('/') {
            return reference.to_string();
        }
        let directory = &page[..=page.rfind('/').expect("a path")];
        format!("{directory}{reference}")
    };
    for page in ["/approve/canvas", "/approve/canvas/"] {
        let (status, _, body) = rig.get_with_cookie(page, &pair);
        assert_eq!(status, 200, "{page}");
        let html = String::from_utf8(body).expect("the page is text");
        let named = |attribute: &str| -> Vec<String> {
            html.split(&format!("{attribute}=\""))
                .skip(1)
                .map(|rest| rest.split('"').next().expect("a closing quote").to_string())
                .collect()
        };
        let assets: Vec<String> = named("href").into_iter().chain(named("src")).collect();
        assert_eq!(
            assets.len(),
            2,
            "{page} names its stylesheet and its script: {assets:?}"
        );
        for (asset, want) in assets
            .iter()
            .zip(["body { color: black; }", "/* canvas */"])
        {
            let (status, _, body) = rig.get_with_cookie(&resolve(page, asset), &pair);
            assert_eq!(status, 200, "{page} names {asset}, which must load");
            assert_eq!(String::from_utf8_lossy(&body), want, "{page} names {asset}");
        }
        // The script reads its other addresses from this attribute, and it is not relative to the page either.
        assert!(
            html.contains("data-arca-base=\"/approve/canvas\""),
            "{page}"
        );
    }
}

#[test]
fn canvas_url_vectors_never_return_bytes_and_head_matches_get() {
    let (rig, _) = Rig::new("canvas-vectors", true);
    let cookie = rig
        .canvas
        .issue_session("passkey:test", NOW)
        .expect("a session cookie");
    let pair = cookie.split(';').next().expect("cookie pair").to_string();
    for path in [
        "/approve/canvas//",
        "/approve/canvas%2F",
        "/approve%2Fcanvas",
        "/approve/canvas/%2E%2E/plans",
        "/approve/canvas%2E%2E/plans",
        "/approve/canvas/page%2Ecss",
        "/approve/canvas/page.css%2F..",
    ] {
        let (status, _, body) = rig.get_with_cookie(path, &pair);
        assert!(
            status != 200 || body.is_empty(),
            "{path} answered 200 with bytes"
        );
    }
    // One trailing slash is the same route, and nothing more is: the page with the slash is the page, and a second
    // slash, or an encoded one, is no route.
    assert_eq!(
        rig.get_with_cookie("/approve/canvas/", &pair).0,
        rig.get_with_cookie("/approve/canvas", &pair).0,
        "a trailing slash is the same page"
    );
    for path in ["/approve/canvas//", "/approve/canvas%2F"] {
        assert_eq!(rig.get_with_cookie(path, &pair).0, 404, "{path}");
    }

    for signed in [false, true] {
        for path in [
            "/approve/canvas",
            "/approve/canvas/page.css",
            "/approve/canvas/page.js",
            "/approve/canvas/snapshot",
        ] {
            let (get_status, get_head, get_body) = if signed {
                rig.get_with_cookie(path, &pair)
            } else {
                rig.get_full(path, None)
            };
            let (head_status, head_head, head_body) = if signed {
                rig.head_with_cookie(path, &pair)
            } else {
                rig.request_full_on(false, None, "HEAD", path, None, &[], None, &[])
            };
            assert_eq!(
                head_status, get_status,
                "{path} HEAD status (signed={signed})"
            );
            assert!(
                head_body.is_empty(),
                "{path} HEAD body must be empty (signed={signed})"
            );
            for name in [
                "content-security-policy",
                "x-content-type-options",
                "referrer-policy",
            ] {
                assert_eq!(
                    header_value(&head_head, name),
                    header_value(&get_head, name),
                    "{path} {name} (signed={signed})"
                );
            }
            if !signed && path != "/approve/canvas" {
                assert!(get_body.is_empty(), "{path} signed-out GET body");
            }
        }
    }
}

#[test]
fn canvas_cookie_cannot_approve_or_reach_mcp() {
    let (rig, key) = Rig::new("canvas-cookie-scope", true);
    let cookie = rig
        .canvas
        .issue_session("passkey:test", NOW)
        .expect("a session cookie");
    let pair = cookie.split(';').next().expect("cookie pair").to_string();
    let (plan, _) = rig.waiting_plan();
    let (status, _, body) = rig.post_with_cookie(
        "/approve/approve",
        Some(ORIGIN),
        json!({ "plan": plan }),
        &pair,
    );
    assert_eq!(status, 400, "a canvas cookie cannot approve");
    assert!(!String::from_utf8_lossy(&body).contains("approved"));
    assert_eq!(deletes_for(&rig.fake, "toy-box-t1"), 0);
    let clock: Clock = Arc::new(|| NOW);
    let handlers = plan_handlers(Arc::clone(&rig.kernel), clock, None, None);
    let submit = handlers
        .iter()
        .find(|(name, _)| *name == "approval_submit")
        .map(|(_, handler)| Arc::clone(handler))
        .expect("the approval handler");
    let caller = Caller {
        token_id: "test".to_string(),
        scope: Scope::Viewer,
        tools: None,
    };
    let typed = submit
        .call(json!({ "plan": plan }), &caller)
        .expect_err("a missing approval is refused");
    match typed {
        CallError::Invalid(message) => assert_eq!(message, "approval is required"),
        other => panic!("unexpected refusal: {other:?}"),
    }
    let signin = sign_in_statement(ORIGIN, &"cd".repeat(32), NOW);
    let signed = approval(&key, &signin);
    let (status, _, body) = rig.post_with_cookie(
        "/approve/approve",
        Some(ORIGIN),
        json!({ "plan": plan, "approval": signed }),
        &pair,
    );
    assert_eq!(
        status, 403,
        "the approval parser refuses a sign-in statement"
    );
    let refusal: Value = serde_json::from_slice(&body).expect("a typed refusal");
    assert_eq!(refusal["refusal"], "Malformed");
    assert!(
        refusal["message"]
            .as_str()
            .is_some_and(|message| message.contains("key Arca does not read")),
        "{refusal}"
    );
    assert_eq!(deletes_for(&rig.fake, "toy-box-t1"), 0);
    let (status, _, _) = rig.post_with_cookie("/mcp", None, json!({}), &pair);
    assert_eq!(status, 401, "a canvas cookie is not an MCP bearer");
}

#[test]
fn canvas_failure_classes_name_statement_and_challenge_without_payloads() {
    let (rig, key) = Rig::new("canvas-classes", true);
    let bad = json!({ "statement": "{}", "proof": {} });
    let (status, _, _) = rig.post_full("/approve/canvas", Some(ORIGIN), bad);
    assert_eq!(status, 403);
    let logs = rig.logs.lock().expect("logs").clone();
    assert_eq!(
        logs.last().map(String::as_str),
        Some("arca: canvas sign-in failed: statement")
    );

    let statement = sign_in_statement(ORIGIN, &"ab".repeat(32), NOW);
    let assertion = Assert::over(NAMESPACE, statement.as_bytes());
    let proof = key.proof_value(&assertion);
    let (status, _, _) = rig.post_full(
        "/approve/canvas",
        Some(ORIGIN),
        json!({ "statement": &statement, "proof": &proof }),
    );
    assert_eq!(status, 403);
    let logs = rig.logs.lock().expect("logs").clone();
    assert_eq!(
        logs.last().map(String::as_str),
        Some("arca: canvas sign-in failed: challenge")
    );
    let lines = logs.join("\n");
    assert!(!lines.contains(&statement));
    assert!(!lines.contains(&serde_json::to_string(&proof).expect("proof JSON")));
    assert!(!lines.contains(&key.cred_text()));
    assert!(!lines.contains(&key.key_text()));
}

#[test]
fn pages_serve_trusted_credentials_and_check_trust_before_create_source_order() {
    // Source-order only. The runtime proof that no create happens is the browser ceremony step 5.
    let (rig, key) = Rig::new("canvas-credentials", true);
    let (status, body) = rig.get("/approve", None);
    assert_eq!(status, 200);
    let source = String::from_utf8(body).expect("page text");
    assert!(source.contains("data-credential-ids=\""));
    assert!(source.contains(&key.cred_text()));
    assert!(!source.contains("__ARCA_CREDENTIAL_IDS__"));
    let register = source
        .split("async function register()")
        .nth(1)
        .expect("the register function");
    let check = register
        .find("passkeysTrusted")
        .expect("the pre-create trust check");
    let empty = register
        .find("if (!setupCode)")
        .expect("the empty-code guard");
    let verify = register
        .find("/approve/setup-code/verify")
        .expect("the setup-code check");
    let create = register
        .find("navigator.credentials.create")
        .expect("the create call");
    assert!(
        check < empty,
        "trust is checked before the empty-code guard"
    );
    assert!(
        empty < verify,
        "the empty-code guard runs before verification"
    );
    assert!(verify < create, "the setup code is checked before create");
    assert!(source.contains("allowCredentials,"));
    assert!(source.contains("No Arca passkey was used. If you closed the prompt, try again. If this keeps happening, this device has no passkey Arca knows."));

    let (status, body) = rig.get("/approve/canvas", None);
    assert_eq!(status, 200);
    let signin = String::from_utf8(body).expect("sign-in text");
    assert!(signin.contains("data-credential-ids=\""));
    assert!(signin.contains(&key.cred_text()));
    assert!(!signin.contains("__ARCA_CREDENTIAL_IDS__"));
    assert!(signin.contains("allowCredentials"));
    assert!(signin.contains("No Arca passkey was used. If you closed the prompt, try again. If this keeps happening, this device has no passkey Arca knows."));
}

#[test]
fn zero_keys_pages_serve_empty_ids_and_skip_webauthn() {
    let (rig, _) = Rig::new_empty("canvas-zero-keys");
    for path in ["/approve", "/approve/canvas"] {
        let (status, body) = rig.get(path, None);
        assert_eq!(status, 200);
        let source = String::from_utf8(body).expect("page text");
        assert!(source.contains("data-credential-ids=\"[]\""), "{path}");
        assert!(!source.contains("__ARCA_CREDENTIAL_IDS__"), "{path}");
        assert!(source.contains("if (!allowCredentials.length)"), "{path}");
    }
}

#[test]
fn action_cards_render_every_kernel_detail_row_as_text() {
    let html = include_str!("../src/approve_page.html");
    assert!(
        html.contains("const details = Array.isArray(plan.details) ? plan.details : [];")
            && html
                .contains("for (const detail of details) fact(detail.label, detail.value, false);"),
        "the action card renders every details row"
    );
    assert!(
        !html.contains("innerHTML"),
        "the page builds rows with textContent, never innerHTML"
    );
}

#[test]
fn the_page_is_served_an_enroll_request_with_its_code_and_service_and_approves_it() {
    let (rig, key) = Rig::new("page-enroll", true);
    let read = Arc::clone(&rig.clock);
    let clock: Clock = Arc::new(move || read.load(Ordering::SeqCst));
    let verbs: std::collections::HashMap<_, _> =
        approval_requests::handlers(Arc::clone(&rig.kernel), clock, Arc::clone(&rig.requests))
            .into_iter()
            .collect();
    let doctor = Caller {
        token_id: "doctor".to_string(),
        scope: Scope::Doctor,
        tools: None,
    };
    let created = verbs["owner_approval_request"]
        .call(
            json!({ "kind": "arca", "action": "enroll", "target": "K7QX2M", "args": { "service": "cloud" } }),
            &doctor,
        )
        .expect("the request is made");
    let id = created["id"].as_str().expect("an id").to_string();

    // What the page's request card is drawn from: the action that picks the enroll card, the code it shows large, and
    // the service its sentence names.
    let (status, body) = rig.get("/approve/plans", None);
    assert_eq!(status, 200);
    let listed: Value = serde_json::from_slice(&body).expect("plans JSON");
    let request = &listed["approvalRequests"][0];
    assert_eq!(request["id"], json!(id));
    assert_eq!(request["state"], "pending");
    assert_eq!(request["fields"]["action"], "enroll");
    assert_eq!(request["fields"]["target"], "K7QX2M");
    assert_eq!(request["args"], json!({ "service": "cloud" }));
    assert_eq!(request["requester"]["name"], "doctor");
    assert!(request.get("approval").is_none());

    // One tap on that card: the statement the passkey signs names enroll and the code, and the request is approved.
    let (status, begun) = rig.post("/approve/requests/begin", Some(ORIGIN), json!({ "id": id }));
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&begun));
    let begun: Value = serde_json::from_slice(&begun).expect("begin JSON");
    let statement = begun["statement"].as_str().expect("a statement");
    let signed: Value = serde_json::from_str(statement).expect("the statement");
    assert_eq!(signed["action"], "enroll");
    assert_eq!(signed["target"], "K7QX2M");
    let proof = key.proof_value(&Assert::over(NAMESPACE, statement.as_bytes()));
    let (status, answer) = rig.post(
        "/approve/requests/approve",
        Some(ORIGIN),
        json!({ "id": id, "statement": statement, "proof": proof }),
    );
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&answer));
    let approval = verbs["owner_approval_status"]
        .call(json!({ "id": id }), &doctor)
        .expect("the status")["approval"]
        .as_str()
        .expect("the approval")
        .to_string();
    assert_eq!(
        verbs["enroll_confirm"]
            .call(
                json!({ "target": "K7QX2M", "args": { "service": "cloud" }, "approval": approval }),
                &doctor
            )
            .expect("confirmed"),
        json!({ "confirmed": true })
    );
}

/// The enroll branch of the request card: the sentence the owner reads and the code as its own large element. The route
/// test above covers the data the card is drawn from; this fails when the card's own markup is taken out of the page.
#[test]
fn the_request_card_keeps_its_enroll_sentence_and_its_large_code_element() {
    let html = include_str!("../src/approve_page.html");
    assert!(
        html.contains(r#"const enroll = fields.action === "enroll";"#)
            && html.contains(
                r#""Approve " + String(args.service) + " to make the person showing code " + String(fields.target) + " an owner""#
            ),
        "the card says what the owner approves, with the service and the code"
    );
    assert!(
        html.contains(r#"el("p", "enroll-code", String(fields.target))"#)
            && html
                .contains("card.append(meta, heading, ...(code ? [code] : []), facts, buttons);"),
        "the code is its own element, set as text, on the card"
    );
    let rule = html
        .split(".enroll-code {")
        .nth(1)
        .and_then(|rest| rest.split('}').next())
        .expect("the code's style");
    assert!(
        rule.contains("font-size: 2.25rem"),
        "the code is large: {rule}"
    );
}

#[test]
fn every_inline_page_carries_the_open_intelligence_tag_once_as_a_picture_and_never_a_link() {
    // The footer cut exactly as `assets/open-intelligence/render.mjs` draws it from the we unfold Badge component.
    let tag =
        include_str!("../../../assets/open-intelligence/open-intelligence-tag.svg").trim_end();
    assert!(
        tag.starts_with(
            r#"<svg viewBox="0 0 340 64" width="170" height="32" role="img" aria-label="Open Intelligence badge">"#
        ),
        "the footer cut is 170 px wide and names itself"
    );
    assert!(
        tag.ends_with(">OPEN INTELLIGENCE</text></svg>"),
        "the words are OPEN INTELLIGENCE"
    );
    assert!(
        tag.contains(r##"fill="#0e1017" stroke="#ebe6da""##) && !tag.contains("var("),
        "the tag carries plain colours as attributes: no browser has to read var() in an attribute"
    );
    assert_eq!(
        tag.matches("stroke=\"").count(),
        tag.matches("stroke=\"#ebe6da\"").count(),
        "the tag is the one-colour cut: the pill's outline and every stroke of the mark share one colour"
    );
    assert!(
        tag.matches("<path ").count() > 100,
        "the mark is in the tag"
    );
    for (label, html) in [
        ("approval", include_str!("../src/approve_page.html")),
        ("sign-in", include_str!("../src/canvas_signin.html")),
        ("error", include_str!("../src/canvas_error.html")),
    ] {
        assert_eq!(
            html.matches(tag).count(),
            1,
            "{label}: the vendored tag, byte for byte"
        );
        assert_eq!(html.matches("OPEN INTELLIGENCE").count(), 1, "{label}");
        let at = html.find(tag).expect("the tag");
        let before = &html[..at];
        assert!(
            before.ends_with("<footer>")
                || before.ends_with(r#"<footer class="open-intelligence">"#),
            "{label}: the tag opens the footer"
        );
        let mut rest = html[at + tag.len()..].to_string();
        assert!(
            rest.starts_with("</footer>"),
            "{label}: the footer holds the tag and nothing else"
        );
        if let (Some(start), Some(end)) = (rest.find("<script>"), rest.find("</script>")) {
            rest.replace_range(start..end + "</script>".len(), "");
        }
        for closing in ["</footer>", "</main>", "</body>", "</html>"] {
            rest = rest.replace(closing, "");
        }
        assert!(
            rest.trim().is_empty(),
            "{label}: the footer is the last thing the page shows"
        );
        assert!(
            !html.contains("<a ") && !html.contains("<a>"),
            "{label}: the page has no link, so the tag is not one"
        );
        assert!(!html.contains("style=\""), "{label}: no style attribute");
        // The colours come from the page's own stylesheet, which the CSP hash covers: the pill takes the page's ground
        // and ink, the words its ink, in light and dark.
        let style = &html[html.find("<style>").expect("a style block")
            ..html.find("</style>").expect("a closed style block")];
        let scope = if label == "approval" {
            ".open-intelligence"
        } else {
            "footer"
        };
        assert!(
            style.contains(&format!(
                "{scope} rect {{ fill: var(--arca-bg); stroke: var(--arca-ink); }}"
            )) && style.contains(&format!("{scope} path {{ stroke: var(--arca-ink); }}"))
                && style.contains(&format!("{scope} text {{ fill: var(--arca-ink); }}")),
            "{label}: the stylesheet colours the pill, the mark and the words from the page's tokens"
        );
        assert!(
            style.contains("--arca-bg:") && style.contains("--arca-ink:"),
            "{label}: the page defines the two tokens the tag takes"
        );
    }
}

#[test]
fn credential_placeholder_is_only_in_the_body_attribute() {
    for (label, html) in [
        ("approval", include_str!("../src/approve_page.html")),
        ("sign-in", include_str!("../src/canvas_signin.html")),
    ] {
        let marker = "__ARCA_CREDENTIAL_IDS__";
        assert_eq!(html.matches(marker).count(), 1, "{label}");
        let at = html.find(marker).expect("the placeholder");
        let body = html
            .find("<body data-credential-ids=\"")
            .expect("the body attribute");
        assert!(at > body, "{label}: inside the body attribute");
        for (open, close) in [("<script>", "</script>"), ("<style>", "</style>")] {
            if let Some(start) = html.find(open) {
                let end = html[start..]
                    .find(close)
                    .map(|offset| start + offset)
                    .expect("a closed block");
                assert!(!(at >= start && at < end), "{label}: placeholder in {open}");
            }
        }
    }
}

fn pending_passkey(rig: &Rig, key: &SoftPasskey) -> Value {
    let mut request = register_body(key, None);
    request["label"] = json!("Second phone");
    let (status, body) = rig.post("/approve/register", Some(ORIGIN), request);
    assert_eq!(status, 200);
    let pending: Value = serde_json::from_slice(&body).expect("pending change");
    assert!(pending["statement"].is_string());
    assert!(
        pending.get("trusted").is_none(),
        "registration alone does not trust the new key"
    );
    pending
}

fn signed_change(mut pending: Value, key: &SoftPasskey) -> Value {
    let statement = pending["statement"].as_str().expect("statement");
    pending["proof"] = key.proof_value(&Assert::over(NAMESPACE, statement.as_bytes()));
    pending
}

fn removal(rig: &Rig, label: &str) -> Value {
    let (status, body) = rig.get("/approve/passkeys", None);
    assert_eq!(status, 200);
    let list: Value = serde_json::from_slice(&body).expect("keys");
    let key = list["passkeys"]
        .as_array()
        .expect("list")
        .iter()
        .find(|key| key["label"] == label)
        .expect("label");
    let (status, body) = rig.post(
        "/approve/passkeys/change",
        Some(ORIGIN),
        json!({ "target": key["target"] }),
    );
    assert_eq!(status, 200);
    let mut pending: Value = serde_json::from_slice(&body).expect("removal statement");
    pending["target"] = key["target"].clone();
    pending
}

#[test]
fn passkey_change_trusted_tap_adds_once_and_removes_without_restart() {
    let (rig, trusted) = Rig::new("passkey-change", true);
    let new = SoftPasskey::from_seed(2);
    let pending = pending_passkey(&rig, &new);
    assert_eq!(rig.kernel.lock().expect("kernel").owner_passkey_count(), 1);
    assert!(!rig.state_path().exists());
    let statement = pending["statement"].as_str().expect("statement");
    let parsed = arca_approval::parse_statement(statement.as_bytes()).expect("accepted action");
    assert_eq!(parsed.action, arca_approval::ArcaAction::PasskeyAdd);
    let line = pending["line"].as_str().expect("line");
    assert_eq!(
        parsed.target,
        format!(
            "passkey-{}",
            arca_kernel::canon::Digest::of(line.as_bytes()).to_hex()
        )
    );
    assert_eq!(
        parsed.params_digest,
        arca_kernel::canon::params_digest(&json!({ "line": line })).expect("digest")
    );
    let request = signed_change(pending, &trusted);
    assert_eq!(
        rig.post("/approve/passkeys/change", Some(ORIGIN), request.clone())
            .0,
        200
    );
    let bytes = fs::read(rig.state_path()).expect("state file");
    let (status, body) = rig.post("/approve/passkeys/change", Some(ORIGIN), request);
    assert_eq!(status, 403);
    assert!(String::from_utf8_lossy(&body).contains("Replayed"));
    assert_eq!(fs::read(rig.state_path()).expect("unchanged"), bytes);
    assert_eq!(rig.kernel.lock().expect("kernel").owner_passkey_count(), 2);
    // The new key can approve immediately, with the same verifier and nonce/count stores.
    let (plan, statement) = rig.waiting_plan();
    assert_eq!(
        rig.post(
            "/approve/approve",
            Some(ORIGIN),
            json!({ "plan": plan, "approval": approval(&new, &statement) })
        )
        .0,
        200
    );
    let request = signed_change(removal(&rig, "Second phone"), &trusted);
    assert_eq!(
        rig.post("/approve/passkeys/change", Some(ORIGIN), request.clone())
            .0,
        200
    );
    assert!(refusal(
        rig.post("/approve/passkeys/change", Some(ORIGIN), request)
            .0
    ));
    assert!(
        load_state_passkeys(&rig.state_path())
            .expect("empty state")
            .is_empty()
    );
    assert_eq!(rig.kernel.lock().expect("kernel").owner_passkey_count(), 1);
    let pending = pending_passkey(&rig, &SoftPasskey::from_seed(3));
    assert_eq!(
        rig.post(
            "/approve/passkeys/change",
            Some(ORIGIN),
            signed_change(pending, &new)
        )
        .0,
        403,
        "removed key loses authority immediately"
    );
}

#[test]
fn passkey_change_refuses_a_fresh_duplicate_add_as_malformed() {
    let (rig, trusted) = Rig::new("passkey-duplicate-add", true);
    let new = SoftPasskey::from_seed(2);
    let pending = pending_passkey(&rig, &new);
    let line = pending["line"].clone();
    let first_statement = pending["statement"].clone();
    assert_eq!(
        rig.post(
            "/approve/passkeys/change",
            Some(ORIGIN),
            signed_change(pending, &trusted)
        )
        .0,
        200
    );
    let before = fs::read(rig.state_path()).expect("state file");
    let count = rig.kernel.lock().expect("kernel").owner_passkey_count();
    assert_eq!(count, 2);
    let pending = pending_passkey(&rig, &new);
    assert_eq!(pending["line"], line);
    assert_ne!(pending["statement"], first_statement, "a fresh nonce");
    let (status, body) = rig.post(
        "/approve/passkeys/change",
        Some(ORIGIN),
        signed_change(pending, &trusted),
    );
    assert_eq!(status, 403);
    let body: Value = serde_json::from_slice(&body).expect("refusal JSON");
    assert_eq!(body["refusal"], "Malformed");
    assert_eq!(fs::read(rig.state_path()).expect("unchanged"), before);
    assert_eq!(
        fs::read_to_string(rig.state_path())
            .expect("state file")
            .lines()
            .collect::<Vec<_>>(),
        vec![line.as_str().expect("line")]
    );
    assert_eq!(
        rig.kernel.lock().expect("kernel").owner_passkey_count(),
        count
    );
}

#[test]
fn register_with_trusted_keys_only_prepares_a_change_and_refuses_setup_code() {
    let (rig, trusted) = Rig::new_empty("passkey-register-existing");
    assert_eq!(rig.post_empty("/approve/setup-code", Some(ORIGIN)).0, 200);
    let code = rig.last_code();
    assert_eq!(
        rig.post(
            "/approve/register",
            Some(ORIGIN),
            register_body(&trusted, Some(&code))
        )
        .0,
        200
    );
    let before = fs::read(rig.state_path()).expect("state file");
    let count = rig.kernel.lock().expect("kernel").owner_passkey_count();
    assert_eq!(count, 1);

    let (status, body) = rig.post(
        "/approve/register",
        Some(ORIGIN),
        register_body(&SoftPasskey::from_seed(3), Some(&code)),
    );
    assert_eq!(status, 409);
    let body: Value = serde_json::from_slice(&body).expect("already trusted JSON");
    assert_eq!(body, json!({ "message": "A passkey is already trusted" }));
    assert_eq!(fs::read(rig.state_path()).expect("unchanged"), before);
    assert_eq!(
        rig.kernel.lock().expect("kernel").owner_passkey_count(),
        count
    );

    let new = SoftPasskey::from_seed(2);
    let (status, body) = rig.post("/approve/register", Some(ORIGIN), register_body(&new, None));
    assert_eq!(status, 200);
    let pending: Value = serde_json::from_slice(&body).expect("pending change");
    let fields = pending.as_object().expect("registration object");
    assert_eq!(
        fields.len(),
        2,
        "registration returns only line and statement"
    );
    assert!(pending["line"].is_string());
    assert!(pending["statement"].is_string());
    assert!(pending.get("trusted").is_none());
    assert_eq!(fs::read(rig.state_path()).expect("unchanged"), before);
    assert_eq!(
        rig.kernel.lock().expect("kernel").owner_passkey_count(),
        count
    );

    let (status, body) = rig.post(
        "/approve/passkeys/change",
        Some(ORIGIN),
        signed_change(pending.clone(), &new),
    );
    assert_eq!(status, 403, "the new key cannot approve its own addition");
    assert!(String::from_utf8_lossy(&body).contains("BadSignature"));
    assert_eq!(fs::read(rig.state_path()).expect("unchanged"), before);
    assert_eq!(
        rig.kernel.lock().expect("kernel").owner_passkey_count(),
        count
    );
    assert_eq!(
        rig.post(
            "/approve/passkeys/change",
            Some(ORIGIN),
            signed_change(pending, &trusted)
        )
        .0,
        200
    );
    assert_eq!(
        rig.kernel.lock().expect("kernel").owner_passkey_count(),
        count + 1
    );
    assert_eq!(
        load_state_passkeys(&rig.state_path())
            .expect("state keys")
            .len(),
        2
    );
}

#[test]
fn passkey_change_refuses_new_key_tampered_line_digest_origin_and_non_passkey() {
    let (rig, trusted) = Rig::new("passkey-change-refuse", true);
    let new = SoftPasskey::from_seed(2);
    let pending = pending_passkey(&rig, &new);
    let (status, body) = rig.post(
        "/approve/passkeys/change",
        Some(ORIGIN),
        signed_change(pending.clone(), &new),
    );
    assert_eq!(status, 403);
    assert!(String::from_utf8_lossy(&body).contains("BadSignature"));
    let mut tampered = pending.clone();
    tampered["line"] = json!(format!(
        "{} changed",
        pending["line"].as_str().expect("line")
    ));
    let (status, body) = rig.post(
        "/approve/passkeys/change",
        Some(ORIGIN),
        signed_change(tampered.clone(), &trusted),
    );
    assert_eq!(status, 403);
    assert!(String::from_utf8_lossy(&body).contains("WrongTarget"));
    let mut statement: Value =
        serde_json::from_str(pending["statement"].as_str().expect("statement")).expect("JSON");
    statement["target"] = json!(format!(
        "passkey-{}",
        arca_kernel::canon::Digest::of(tampered["line"].as_str().expect("line").as_bytes())
            .to_hex()
    ));
    tampered["statement"] = json!(canonical_json(&statement).expect("canonical"));
    let (status, body) = rig.post(
        "/approve/passkeys/change",
        Some(ORIGIN),
        signed_change(tampered, &trusted),
    );
    assert_eq!(status, 403);
    assert!(String::from_utf8_lossy(&body).contains("WrongParams"));
    for origin in [None, Some("https://other.example.com")] {
        assert_eq!(
            rig.post(
                "/approve/passkeys/change",
                origin,
                signed_change(pending.clone(), &trusted)
            )
            .0,
            403
        );
        assert_eq!(
            rig.get("/approve/passkeys", origin).0,
            if origin.is_none() { 200 } else { 403 }
        );
    }
    let mut invalid = pending.clone();
    invalid["proof"] = json!("not a WebAuthn proof");
    assert_eq!(
        rig.post("/approve/passkeys/change", Some(ORIGIN), invalid)
            .0,
        403
    );
    assert!(!rig.state_path().exists());
    assert_eq!(rig.kernel.lock().expect("kernel").owner_passkey_count(), 1);
    assert_eq!(rig.post_empty("/approve/setup-code", Some(ORIGIN)).0, 409);
    assert_eq!(
        rig.post(
            "/approve/register",
            Some(ORIGIN),
            register_body(&new, Some("00000000"))
        )
        .0,
        409
    );
    // Refused binding checks have not spent the nonce, so the original statement still works.
    assert_eq!(
        rig.post(
            "/approve/passkeys/change",
            Some(ORIGIN),
            signed_change(pending, &trusted)
        )
        .0,
        200
    );
}

#[test]
fn passkey_change_refuses_last_key_and_root_file_removal() {
    let (rig, key) = Rig::new_empty("passkey-last-remove");
    assert_eq!(rig.post_empty("/approve/setup-code", Some(ORIGIN)).0, 200);
    assert_eq!(
        rig.post(
            "/approve/register",
            Some(ORIGIN),
            register_body(&key, Some(&rig.last_code()))
        )
        .0,
        200
    );
    let before = fs::read(rig.state_path()).expect("state");
    let request = signed_change(removal(&rig, "My passkey"), &key);
    let (status, body) = rig.post("/approve/passkeys/change", Some(ORIGIN), request);
    assert_eq!(status, 403);
    assert!(String::from_utf8_lossy(&body).contains("last trusted passkey"));
    assert_eq!(fs::read(rig.state_path()).expect("unchanged"), before);

    let (rig, root) = Rig::new("passkey-root-remove", true);
    let new = SoftPasskey::from_seed(2);
    let pending = pending_passkey(&rig, &new);
    assert_eq!(
        rig.post(
            "/approve/passkeys/change",
            Some(ORIGIN),
            signed_change(pending, &root)
        )
        .0,
        200
    );
    let before = fs::read(rig.state_path()).expect("state");
    // Exercise the kernel boundary directly too, even when a caller supplies a root line rather than a list target.
    let line = root.line(NAMESPACE);
    let statement = canonical_json(&json!({
        "action": "passkey_remove", "host": HOST,
        "target": format!("passkey-{}", arca_kernel::canon::Digest::of(line.as_bytes()).to_hex()),
        "paramsDigest": arca_kernel::canon::params_digest(&json!({ "line": line })).expect("digest"),
        "issuedAt": arca::handlers::iso_utc(NOW), "expiresAt": arca::handlers::iso_utc(NOW + 600_000),
        "nonce": "12121212121212121212121212121212",
    })).expect("statement");
    let result = rig.kernel.lock().expect("kernel").approve_passkey_change(
        &rig.state_dir,
        &line,
        &statement,
        &root.assert(&Assert::over(NAMESPACE, statement.as_bytes())),
        NOW,
    );
    assert!(matches!(
        result,
        Err(arca_approval::ApprovalRefusal::Untrusted(_))
    ));
    assert_eq!(fs::read(rig.state_path()).expect("unchanged"), before);
    assert_eq!(rig.kernel.lock().expect("kernel").owner_passkey_count(), 2);
}

#[test]
fn passkey_list_exposes_only_labels_dates_and_opaque_targets_and_persists_metadata() {
    let (rig, root) = Rig::new("passkey-list", true);
    let new = SoftPasskey::from_seed(2);
    let pending = pending_passkey(&rig, &new);
    assert_eq!(
        rig.post(
            "/approve/passkeys/change",
            Some(ORIGIN),
            signed_change(pending, &root)
        )
        .0,
        200
    );
    let (status, bytes) = rig.get("/approve/passkeys", None);
    assert_eq!(status, 200);
    let text = String::from_utf8(bytes).expect("UTF-8");
    for key in [&root, &new] {
        assert!(!text.contains(&key.key_text()));
        assert!(!text.contains(&key.cred_text()));
    }
    let list: Value = serde_json::from_str(&text).expect("list");
    for key in list["passkeys"].as_array().expect("keys") {
        let fields = key.as_object().expect("entry");
        assert_eq!(fields.len(), 3);
        assert!(
            fields.contains_key("label")
                && fields.contains_key("addedAt")
                && fields.contains_key("target")
        );
    }
    assert!(text.contains("Second phone"));
    assert!(text.contains(&arca::handlers::iso_utc(NOW)));
    assert!(
        fs::read_to_string(rig.state_path())
            .expect("metadata persisted")
            .contains("label=Second phone")
    );
}

#[test]
fn passkey_change_preserves_the_trusted_keys_sign_count_across_reload() {
    let (rig, trusted) = Rig::new("passkey-change-count", true);
    let first = SoftPasskey::from_seed(2);
    let mut pending = pending_passkey(&rig, &first);
    let mut assertion = Assert::over(
        NAMESPACE,
        pending["statement"].as_str().expect("statement").as_bytes(),
    );
    assertion.sign_count = 5;
    pending["proof"] = trusted.proof_value(&assertion);
    assert_eq!(
        rig.post("/approve/passkeys/change", Some(ORIGIN), pending)
            .0,
        200
    );
    let second = SoftPasskey::from_seed(3);
    let mut pending = pending_passkey(&rig, &second);
    let mut assertion = Assert::over(
        NAMESPACE,
        pending["statement"].as_str().expect("statement").as_bytes(),
    );
    assertion.sign_count = 4;
    pending["proof"] = trusted.proof_value(&assertion);
    let (status, body) = rig.post("/approve/passkeys/change", Some(ORIGIN), pending.clone());
    assert_eq!(status, 403);
    assert!(String::from_utf8_lossy(&body).contains("SignCountRegressed"));
    assertion.sign_count = 6;
    pending["proof"] = trusted.proof_value(&assertion);
    assert_eq!(
        rig.post("/approve/passkeys/change", Some(ORIGIN), pending)
            .0,
        200
    );
    assert_eq!(rig.kernel.lock().expect("kernel").owner_passkey_count(), 3);
}

#[test]
fn g7_a_kind_arca_request_is_approved_by_one_tap_on_the_page_and_kind_upgrade_is_refused() {
    let (rig, key) = Rig::new("page-g7-tap", true);
    let read = Arc::clone(&rig.clock);
    let clock: Clock = Arc::new(move || read.load(Ordering::SeqCst));
    let verbs: std::collections::HashMap<_, _> =
        approval_requests::handlers(Arc::clone(&rig.kernel), clock, Arc::clone(&rig.requests))
            .into_iter()
            .collect();
    let doctor = Caller {
        token_id: "doctor".to_string(),
        scope: Scope::Doctor,
        tools: None,
    };
    match verbs["owner_approval_request"].call(
        json!({ "kind": "upgrade", "droid": "phone1", "step": "eyes" }),
        &doctor,
    ) {
        Err(CallError::Refused { code, message }) => {
            assert_eq!(code, "upgrade_not_here");
            assert!(message.contains("droidd's MCP"), "{message}");
        }
        other => panic!("kind upgrade is refused, got {other:?}"),
    }
    let args = json!({ "name": "nats" });
    let ask = |target: &str, args: &Value| {
        let created = verbs["owner_approval_request"]
            .call(
                json!({ "kind": "arca", "action": "delete", "target": target, "args": args }),
                &doctor,
            )
            .expect("the request is made");
        created["id"].as_str().expect("an id").to_string()
    };
    let id = ask("nats", &args);

    let (status, body) = rig.get("/approve/plans", None);
    assert_eq!(status, 200);
    let listed: Value = serde_json::from_slice(&body).expect("plans JSON");
    assert_eq!(listed["approvalRequests"][0]["id"], json!(id));
    assert_eq!(listed["approvalRequests"][0]["state"], "pending");
    assert_eq!(listed["approvalRequests"][0]["args"], args);
    assert!(listed["approvalRequests"][0].get("approval").is_none());
    assert_eq!(listed["approvalPasskeys"]["forThisAddress"], 1);

    for origin in [None, Some("https://box1.example.com:9443")] {
        assert!(
            refusal(
                rig.post("/approve/requests/begin", origin, json!({ "id": id }))
                    .0
            ),
            "begin needs the page's exact origin: {origin:?}"
        );
    }

    // One tap: begin mints the statement, the software authenticator signs it, approve takes id, statement and proof.
    let (status, begun) = rig.post("/approve/requests/begin", Some(ORIGIN), json!({ "id": id }));
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&begun));
    let begun: Value = serde_json::from_slice(&begun).expect("begin JSON");
    let statement = begun["statement"].as_str().expect("a statement");
    let proof = key.proof_value(&Assert::over(NAMESPACE, statement.as_bytes()));
    let wrong = key.proof_value(&Assert::over(NAMESPACE, b"another statement"));
    let (status, _) = rig.post(
        "/approve/requests/approve",
        Some(ORIGIN),
        json!({ "id": id, "statement": statement, "proof": wrong }),
    );
    assert_eq!(status, 403, "a proof over another statement is refused");
    let body = json!({ "id": id, "statement": statement, "proof": proof });
    for origin in [None, Some("https://box1.example.com:9443")] {
        assert!(
            refusal(
                rig.post("/approve/requests/approve", origin, body.clone())
                    .0
            ),
            "approve needs the page's exact origin: {origin:?}"
        );
    }
    let (status, answer) = rig.post("/approve/requests/approve", Some(ORIGIN), body.clone());
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&answer));
    let answer: Value = serde_json::from_slice(&answer).expect("approve JSON");
    assert_eq!(answer["state"], "approved");
    assert!(
        answer.get("approval").is_none(),
        "the page gets the state only"
    );
    let (status, _) = rig.post("/approve/requests/approve", Some(ORIGIN), body);
    assert_eq!(
        status, 409,
        "a second tap on an approved request is refused"
    );

    let status = verbs["owner_approval_status"]
        .call(json!({ "id": id }), &doctor)
        .expect("the requester reads its status");
    assert_eq!(status["state"], "approved");
    let approval = status["approval"].as_str().expect("the approval string");
    let spend = |approval: &str| {
        rig.kernel.lock().expect("kernel").check_approval(
            ArcaAction::Delete,
            "nats",
            &args,
            approval,
            NOW,
        )
    };
    spend(approval).expect("the kernel spends the tapped approval");
    assert_eq!(
        spend(approval),
        Err(arca_approval::ApprovalRefusal::Replayed),
        "once"
    );

    let other = ask("svc2", &json!({ "name": "svc2" }));
    for path in [
        "/approve/requests/begin",
        "/approve/requests/approve",
        "/approve/requests/deny",
    ] {
        let body = json!({ "id": other }).to_string();
        let (status, _, _) = rig.request_full_on(
            false,
            None,
            "POST",
            path,
            Some(ORIGIN),
            body.as_bytes(),
            None,
            &[("Content-Type", "text/plain")],
        );
        assert_eq!(status, 415, "{path} takes application/json only");
    }
    let (status, denied) = rig.post(
        "/approve/requests/deny",
        Some(ORIGIN),
        json!({ "id": other }),
    );
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&denied));
    assert_eq!(
        serde_json::from_slice::<Value>(&denied).expect("deny JSON"),
        json!({ "state": "denied" })
    );

    let (status, page) = rig.get("/approve", None);
    assert_eq!(status, 200);
    let page = String::from_utf8_lossy(&page);
    assert!(
        page.contains("id=\"requests-section\""),
        "the requests section"
    );
    for unsafe_sink in ["innerHTML", "outerHTML", "insertAdjacentHTML"] {
        assert!(!page.contains(unsafe_sink), "text only: {unsafe_sink}");
    }
}

#[test]
fn g7_the_approve_route_takes_what_a_delivery_would_and_refuses_another_target_another_host_and_an_ssh_signature()
 {
    // Arca has no verb that delivers an approval into a request: `POST /approve/requests/approve` is the one way in.
    let (rig, key) = Rig::new("page-g7-deliver", true);
    let read = Arc::clone(&rig.clock);
    let clock: Clock = Arc::new(move || read.load(Ordering::SeqCst));
    let verbs: std::collections::HashMap<_, _> =
        approval_requests::handlers(Arc::clone(&rig.kernel), clock, Arc::clone(&rig.requests))
            .into_iter()
            .collect();
    let caller = |token: &str| Caller {
        token_id: token.to_string(),
        scope: Scope::Doctor,
        tools: None,
    };
    let created = verbs["owner_approval_request"]
        .call(
            json!({ "kind": "arca", "action": "delete", "target": "nats", "args": { "name": "nats" } }),
            &caller("doctor"),
        )
        .expect("the request is made");
    let id = created["id"].as_str().expect("an id").to_string();
    let (status, begun) = rig.post("/approve/requests/begin", Some(ORIGIN), json!({ "id": id }));
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&begun));
    let begun: Value = serde_json::from_slice(&begun).expect("begin JSON");
    let statement = begun["statement"]
        .as_str()
        .expect("a statement")
        .to_string();
    let passkey = |statement: &str| key.proof_value(&Assert::over(NAMESPACE, statement.as_bytes()));
    let crafted = |field: &str, value: &str| {
        let mut fields: Value = serde_json::from_str(&statement).expect("JSON");
        fields[field] = json!(value);
        canonical_json(&fields).expect("canonical")
    };
    let approve = |statement: &str, proof: Value| {
        let (status, body) = rig.post(
            "/approve/requests/approve",
            Some(ORIGIN),
            json!({ "id": id, "statement": statement, "proof": proof }),
        );
        let body: Value = serde_json::from_slice(&body).expect("the route's JSON");
        (status, body)
    };

    let other_target = crafted("target", "svc2");
    let other_host = crafted("host", "node2");
    let ssh = SkKey::from_seed(3);
    let signature = ssh.sign(NAMESPACE, statement.as_bytes());
    for (name, sent, proof, want_status, want_code, want) in [
        (
            "the passkey over another target",
            other_target.as_str(),
            passkey(&other_target),
            403,
            "owner_approval",
            "it approves another target than the request's",
        ),
        (
            "the passkey over another host",
            other_host.as_str(),
            passkey(&other_host),
            403,
            "owner_approval",
            "it is for host node2, and this is node1",
        ),
        (
            "an ssh signature",
            statement.as_str(),
            json!(signature),
            400,
            "invalid",
            "the proof is the passkey's answer",
        ),
        (
            "an ssh approval string",
            statement.as_str(),
            json!(approve_statement(&ssh, &statement)),
            400,
            "invalid",
            "the proof is the passkey's answer",
        ),
    ] {
        let (status, body) = approve(sent, proof);
        assert_eq!(status, want_status, "{name}: {body}");
        assert_eq!(body["refusal"], want_code, "{name}");
        let message = body["message"].as_str().expect("words");
        assert!(message.contains(want), "{name}: {message}");
        assert!(
            !message.contains(sent) && !message.contains(&signature[..40]),
            "{name}: nothing sent is echoed"
        );
    }
    let pending = verbs["owner_approval_status"]
        .call(json!({ "id": id }), &caller("doctor"))
        .expect("the requester reads its status");
    assert_eq!(
        pending["state"], "pending",
        "every refusal leaves it pending"
    );
    assert!(pending.get("approval").is_none());

    // The matching passkey answer approves it, for its asker only.
    let (status, body) = approve(&statement, passkey(&statement));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["state"], "approved");
    assert!(
        body.get("approval").is_none(),
        "the page gets the state only"
    );
    let mine = verbs["owner_approval_status"]
        .call(json!({ "id": id }), &caller("doctor"))
        .expect("the requester reads its status");
    assert_eq!(mine["state"], "approved");
    let approval = mine["approval"].as_str().expect("the asker's approval");
    let theirs = verbs["owner_approval_status"]
        .call(json!({ "id": id }), &caller("other"))
        .expect("another token reads the state");
    assert_eq!(theirs["state"], "approved");
    assert!(theirs.get("approval").is_none(), "never another token's");
    rig.kernel
        .lock()
        .expect("kernel")
        .check_approval(
            ArcaAction::Delete,
            "nats",
            &json!({ "name": "nats" }),
            approval,
            NOW,
        )
        .expect("the kernel spends the asker's approval");
}
