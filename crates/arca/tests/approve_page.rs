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

use arca::approve_page::{self, SetupCodeDisplay};
use arca::canvas::Canvas;
use arca::desired_store::EntryChange;
use arca::handlers::{Clock, plan_handlers};
use arca::kernel::Kernel;
use arca_api::Scope;
use arca_approval::{
    PASSKEY_COUNTS_FILE, STATE_PASSKEYS_FILE, load_state_passkeys, parse_passkeys,
};
use arca_kernel::canon::canonical_json;
use arca_kernel::desired::EntryId;
use arca_mcp::{CallError, Caller, Server, TokenStore, serve_unix};
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
}

struct UpstreamInner {
    mode: Mutex<UpstreamMode>,
    stop: AtomicBool,
}

/// A loopback fake DSH: one response per request, with upstream headers serve must drop.
struct TestUpstream {
    addr: SocketAddr,
    inner: Arc<UpstreamInner>,
    thread: Option<JoinHandle<()>>,
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
        });
        let worker = Arc::clone(&inner);
        let thread = std::thread::spawn(move || {
            while !worker.stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mode = *worker.mode.lock().expect("upstream mode");
                        if mode == UpstreamMode::Timeout {
                            std::thread::sleep(Duration::from_secs(4));
                            continue;
                        }
                        let mut raw = Vec::new();
                        let mut chunk = [0u8; 1024];
                        while !raw.windows(4).any(|window| window == b"\r\n\r\n") {
                            match stream.read(&mut chunk) {
                                Ok(0) | Err(_) => break,
                                Ok(read) => raw.extend_from_slice(&chunk[..read]),
                            }
                        }
                        let request = String::from_utf8_lossy(&raw);
                        let path = request
                            .lines()
                            .next()
                            .and_then(|line| line.split_whitespace().nth(1))
                            .unwrap_or("/");
                        let (status, body) = if mode == UpstreamMode::Error {
                            ("500 Internal Server Error", "no")
                        } else {
                            match path {
                                "/arca-canvas/page.css" => ("200 OK", "body { color: black; }"),
                                "/arca-canvas/page.js" => ("200 OK", "/* canvas */"),
                                "/arca-canvas/snapshot" => {
                                    ("200 OK", "{\"instances\":[],\"devices\":\"ok\"}")
                                }
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
            thread: Some(thread),
        })
    }

    fn set_mode(&self, mode: UpstreamMode) {
        *self.inner.mode.lock().expect("upstream mode") = mode;
    }
}

impl Drop for TestUpstream {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
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
    _runtime: Runtime,
}

impl Rig {
    fn new(tag: &str, with_page: bool) -> (Rig, SoftPasskey) {
        Self::build(tag, with_page, true)
    }

    fn new_empty(tag: &str) -> (Rig, SoftPasskey) {
        Self::build(tag, true, false)
    }

    fn build(tag: &str, with_page: bool, enrolled: bool) -> (Rig, SoftPasskey) {
        Self::build_with_refusal(tag, with_page, enrolled, false)
    }

    fn build_with_refusal(
        tag: &str,
        with_page: bool,
        enrolled: bool,
        refusing: bool,
    ) -> (Rig, SoftPasskey) {
        let scratch = Scratch::new(tag);
        let upstream = TestUpstream::start();
        let fake = start_fake(&scratch);
        let key = SoftPasskey::from_seed(1);
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
        let display = Arc::new(FakeDisplay::default());
        let page_clock = Arc::clone(&clock);
        let clock_fn: Clock = Arc::new(move || page_clock.load(Ordering::SeqCst));
        let logs = Arc::new(Mutex::new(Vec::new()));
        let captured_logs = Arc::clone(&logs);
        let page = approve_page::Page::new(
            ORIGIN.to_string(),
            Arc::clone(&kernel),
            clock_fn,
            state_dir.clone(),
            Arc::clone(&display) as Arc<dyn SetupCodeDisplay>,
        )
        .with_canvas_origin(&format!("http://{}", upstream.addr))
        .with_log(Arc::new(move |line| {
            captured_logs
                .lock()
                .expect("log lock")
                .push(line.to_string())
        }));
        let canvas = page.canvas().clone();
        let proxy_router = if with_page {
            base.clone().merge(page.router(true))
        } else {
            base.clone()
        };
        let router = if with_page {
            base.merge(page.router(false))
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
        if !body.is_empty() {
            head.push_str("Content-Type: application/json\r\n");
        }
        head.push_str("\r\n");
        let mut bytes = head.into_bytes();
        bytes.extend_from_slice(body);
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
}

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
fn a_second_setup_code_is_refused_while_one_is_live_and_the_live_code_still_works() {
    let (rig, key) = Rig::new_empty("page-live-code");
    assert_eq!(rig.post_empty("/approve/setup-code", Some(ORIGIN)).0, 200);
    let code = rig.last_code();
    assert_eq!(
        rig.post_empty("/approve/setup-code", Some(ORIGIN)).0,
        429,
        "a live code is never replaced"
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
    assert_eq!(
        rig.post_empty("/approve/setup-code", Some(ORIGIN)).0,
        429,
        "the next request is spaced"
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
    assert!(String::from_utf8_lossy(&body).contains("locked"));
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
    assert!(String::from_utf8_lossy(&body).contains("A code is already on your screen"));
    let (status, body) = rig.proxy_post(
        Some("caller-a"),
        "/approve/register",
        register_body(&key, Some(&code)),
    );
    assert_eq!(
        status, 429,
        "the correct code cannot bypass the caller lock"
    );
    assert!(String::from_utf8_lossy(&body).contains("Setup is locked for 60 more minute(s)"));
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
    assert_eq!(status, 404, "only the four exact URLs exist");
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
fn canvas_challenge_cap_is_global_and_resets_after_a_minute() {
    let (rig, _) = Rig::new("canvas-cap", true);
    for index in 0..60 {
        let (status, _, _) = rig.post_full("/approve/canvas", Some(ORIGIN), json!({}));
        assert_eq!(status, 200, "challenge {index}");
    }
    let (status, _, _) = rig.post_full("/approve/canvas", Some(ORIGIN), json!({}));
    assert_eq!(
        status, 429,
        "the sixty-first challenge in a minute is capped"
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
fn canvas_url_vectors_never_return_bytes_and_head_matches_get() {
    let (rig, _) = Rig::new("canvas-vectors", true);
    let cookie = rig
        .canvas
        .issue_session("passkey:test", NOW)
        .expect("a session cookie");
    let pair = cookie.split(';').next().expect("cookie pair").to_string();
    for path in [
        "/approve/canvas/",
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
    assert_eq!(
        rig.get_with_cookie("/approve/canvas/", &pair).0,
        404,
        "a trailing slash is not the exact page URL"
    );

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
    let handlers = plan_handlers(Arc::clone(&rig.kernel), clock);
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
