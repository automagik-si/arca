//! The approval page: open GETs, exact-Origin POSTs, the same kernel approval path, and registration line round-trip. The
//! software P-256 passkey is the shared test authenticator; no hardware key is used.

mod common;
#[path = "../../arca-approval/tests/common/webauthn.rs"]
mod webauthn;

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arca::approve_page;
use arca::desired_store::EntryChange;
use arca::handlers::Clock;
use arca::kernel::Kernel;
use arca_approval::parse_passkeys;
use arca_kernel::desired::EntryId;
use arca_mcp::{Server, TokenStore, serve_unix};
use common::*;
use serde_json::{Value, json};
use tokio::runtime::Runtime;
use webauthn::{Assert, ORIGIN, SoftPasskey, passkey_approval};

/// A node with a fake provider, one enrolled passkey, and the merged router served on a unix socket.
struct Rig {
    _scratch: Scratch,
    fake: arca_incus::FakeIncus,
    kernel: Arc<Mutex<Kernel>>,
    socket: std::path::PathBuf,
    _runtime: Runtime,
}

impl Rig {
    fn new(tag: &str, with_page: bool) -> (Rig, SoftPasskey) {
        let scratch = Scratch::new(tag);
        let fake = start_fake(&scratch);
        let key = SoftPasskey::from_seed(1);
        let lines = format!("{}\n", key.line(NAMESPACE));
        let verifier = verifier_with_passkeys(&scratch, &[], &lines, tag);
        let kernel = Arc::new(Mutex::new(
            kernel(&scratch, &fake, verifier, NOW).expect("the kernel opens"),
        ));
        let tokens_dir = scratch.path("tokens");
        fs::create_dir(&tokens_dir).expect("the token directory");
        fs::set_permissions(&tokens_dir, fs::Permissions::from_mode(0o700)).expect("mode 0700");
        let tokens = TokenStore::load(&tokens_dir).expect("an empty token store loads");
        let base = Server::new(arca_api::verbs(), tokens).router();
        let clock: Clock = Arc::new(|| NOW);
        let router = if with_page {
            base.merge(approve_page::router(
                ORIGIN.to_string(),
                Arc::clone(&kernel),
                clock,
            ))
        } else {
            base
        };
        let socket = scratch.path("serve.sock");
        let runtime = Runtime::new().expect("a runtime");
        let served_socket = socket.clone();
        runtime.spawn(async move {
            let _ = serve_unix(router, &served_socket).await;
        });
        let until = Instant::now() + Duration::from_secs(5);
        while !socket.exists() {
            assert!(Instant::now() < until, "the test server started");
            std::thread::sleep(Duration::from_millis(10));
        }
        let rig = Rig {
            _scratch: scratch,
            fake,
            kernel,
            socket,
            _runtime: runtime,
        };
        (rig, key)
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
        let mut stream = UnixStream::connect(&self.socket).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: box1.example.com\r\nConnection: close\r\nContent-Length: {}\r\n",
            body.len()
        );
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
}

fn approval(key: &SoftPasskey, statement: &str) -> String {
    let assert = Assert::over(NAMESPACE, statement.as_bytes());
    passkey_approval(statement, &key.assert(&assert))
}

fn refusal(status: u16) -> bool {
    (400..=499).contains(&status)
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
    let registration = json!({
        "credentialId": key.cred_text(),
        "publicKey": key.key_text(),
        "publicKeyAlgorithm": -7,
    });
    let (status, body) = rig.post("/approve/register", Some(ORIGIN), registration);
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

    let no_origin = json!({
        "credentialId": key.cred_text(),
        "publicKey": key.key_text(),
        "publicKeyAlgorithm": -7,
    });
    assert!(refusal(rig.post("/approve/register", None, no_origin).0));
}
