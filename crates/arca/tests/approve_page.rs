//! The approval page: open GETs, exact-Origin POSTs, the same kernel approval path, setup-code enrolment, and registration.
//! The software P-256 passkey is the shared test authenticator; no hardware key is used.

mod common;
#[path = "../../arca-approval/tests/common/webauthn.rs"]
mod webauthn;

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arca::approve_page::{self, SetupCodeDisplay};
use arca::desired_store::EntryChange;
use arca::handlers::Clock;
use arca::kernel::Kernel;
use arca_approval::{STATE_PASSKEYS_FILE, load_state_passkeys, parse_passkeys};
use arca_kernel::desired::EntryId;
use arca_mcp::{Server, TokenStore, serve_unix};
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
        .with_log(Arc::new(move |line| {
            captured_logs
                .lock()
                .expect("log lock")
                .push(line.to_string())
        }));
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
        let head = String::from_utf8_lossy(&raw[..split]);
        let status = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|status| status.parse().ok())
            .expect("a status");
        (status, raw[split + 4..].to_vec())
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
    let (rig, key) = Rig::new("page-register", true);
    let (status, body) = rig.post("/approve/register", Some(ORIGIN), register_body(&key, None));
    assert_eq!(status, 200);
    let body: Value = serde_json::from_slice(&body).expect("registration JSON");
    let line = body["line"].as_str().expect("the line");
    let parsed = parse_passkeys(format!("{line}\n").as_bytes()).expect("the line parses");
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
fn owner_page_hides_the_developer_line_when_a_setup_code_was_sent() {
    let (rig, _) = Rig::new_empty("page-owner-source");
    let (status, body) = rig.get("/approve", None);
    assert_eq!(status, 200);
    let source = String::from_utf8(body).expect("page text");
    // Static source check: no DOM runner is available. The request's setupCode guards the owner branch.
    assert!(source.contains("if (setupCode) {\n      line.hidden = true;\n      line.textContent = \"\";\n      status(\"Passkey trusted.\");\n    } else if (body.line) {"));
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
