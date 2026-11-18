//! Multi-host approvals on one machine: the page host (alpha) lists a linked kernel's (beta's) waiting proposals and relays
//! approvals over beta's own socket. Beta verifies them using its own verifier and the keys it trusts from alpha.
//!
//! Every key is a fixture: a software P-256 passkey from a fixed seed, written into the test's own scratch directory.

mod common;
#[path = "../../arca-approval/tests/common/webauthn.rs"]
mod webauthn;

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arca::approval_requests::{self, ApprovalRequests};
use arca::approve_page::{Page, SetupCodeDisplay};
use arca::config::LinkedHost;
use arca::desired_store::EntryChange;
use arca::handlers::{
    ActionExecutor, ActionSlot, ApprovalStatus, Clock, StatusHandler, action_statement,
    plan_handlers_with,
};
use arca::kernel::{AppliedSeq, Kernel};
use arca::linked::{self, RelayError};
use arca_api::Scope;
use arca_approval::{
    ApprovalRefusal, ApprovalVerifier, FileNonces, FileSignCounts, ForeignPasskeys, OwnerKeys,
    PasskeyVerifier, PeerPasskeys, STATE_PASSKEYS_FILE, SshSigVerifier, normalize_set,
    parse_passkeys,
};
use arca_kernel::canon::{Digest, canonical_json, params_digest};
use arca_kernel::desired::EntryId;
use arca_mcp::{CallError, Caller, Server, TokenStore, ToolHandler, mint_with_tools, serve_unix};
use arca_metrics::{HostMode, HostModeChoice};
use common::*;
use serde_json::{Value, json};
use tokio::runtime::Runtime;
use webauthn::{Assert, ORIGIN, SoftPasskey, passkey_approval};

/// The host beta's approval statements name.
const BETA_HOST: &str = "beta.example";
/// The origin of a page beta itself would serve: its own passkey answers only there.
const BETA_ORIGIN: &str = "https://beta.example.com:8443";

struct NoDisplay;

impl SetupCodeDisplay for NoDisplay {
    fn show(&self, _code: &str) -> Result<(), String> {
        Err("No screen to show the code on".to_string())
    }
}

/// The stand-in for `service_create`: it counts its runs.
struct CountingAction {
    runs: Arc<AtomicU64>,
}

impl ToolHandler for CountingAction {
    fn call(&self, _args: Value, _caller: &Caller) -> Result<Value, CallError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        Ok(json!({ "created": true }))
    }
}

struct SubstitutedRequestStatement(&'static str);

impl ToolHandler for SubstitutedRequestStatement {
    fn call(&self, args: Value, _caller: &Caller) -> Result<Value, CallError> {
        let entropy = args["nonce"].as_str().expect("page entropy");
        let id = args["id"].as_str().expect("request id");
        let mut statement = json!({
            "host": BETA_HOST,
            "action": "enroll",
            "target": "AB12CD",
            "paramsDigest": params_digest(&json!({ "service": "example-service" })).expect("digest"),
            "issuedAt": args["issuedAt"],
            "expiresAt": arca::handlers::iso_utc(NOW + 900_000),
            "nonce": format!("{}{entropy}", id.trim_start_matches("ar_")),
        });
        statement[self.0] = match self.0 {
            "host" => json!(format!("other-{BETA_HOST}")),
            "nonce" => {
                let different = if id == "ar_0123456789abcdef" {
                    "fedcba9876543210"
                } else {
                    "0123456789abcdef"
                };
                json!(format!("{different}{entropy}"))
            }
            "issuedAt" => json!(arca::handlers::iso_utc(NOW - 1_000)),
            "expiresAt" => json!(arca::handlers::iso_utc(NOW + 899_000)),
            "action" => json!("spend"),
            "target" => json!("CD34EF"),
            "paramsDigest" => json!("ab".repeat(32)),
            field => panic!("unexpected field {field}"),
        };
        Ok(json!({ "statement": canonical_json(&statement).expect("statement") }))
    }
}

/// How beta trusts alpha's passkeys.
enum BetaTrust {
    /// `approval.trustOwnerKeysFrom` names alpha's passkeys file, for alpha's origin.
    AlphaFile,
    /// `approval.peerKeys` keeps alpha's synced passkeys.
    AlphaPeer,
    /// No `approval` key: beta trusts only its own passkey.
    None,
    /// `approval.trustOwnerKeysFrom` names a file beta cannot read.
    Unreadable,
}

/// Two kernels on one machine. Alpha serves the approval page with beta linked; beta serves its MCP door on its own socket.
struct Pair {
    _alpha_scratch: Scratch,
    _beta_scratch: Scratch,
    _alpha_fake: arca_incus::FakeIncus,
    beta_fake: arca_incus::FakeIncus,
    /// The one clock both kernels and the page read.
    clock: Arc<AtomicU64>,
    /// The lines the page logged.
    log: Arc<Mutex<Vec<String>>>,
    alpha: Arc<Mutex<Kernel>>,
    beta: Arc<Mutex<Kernel>>,
    alpha_socket: PathBuf,
    alpha_runs: Arc<AtomicU64>,
    beta_runs: Arc<AtomicU64>,
    token_file: PathBuf,
    foreign: ForeignPasskeys,
    /// Beta's state directory, where its state-file passkeys live.
    beta_state: PathBuf,
    beta_requester: LinkedHost,
    _runtime: Runtime,
}

/// A passkey of beta's own, answering only beta's page origin.
fn beta_key(seed: u8) -> SoftPasskey {
    let mut key = SoftPasskey::from_seed(seed);
    key.rp_id = "beta.example.com".to_string();
    key.origin = BETA_ORIGIN.to_string();
    key
}

fn executor(runs: &Arc<AtomicU64>) -> ActionExecutor {
    ActionExecutor::new([(
        "service_create",
        Arc::new(CountingAction {
            runs: Arc::clone(runs),
        }) as Arc<dyn ToolHandler>,
    )])
}

fn private_dir(path: &Path) {
    fs::create_dir(path).expect("a directory");
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("mode 0700");
}

fn write_mode(path: &Path, text: &str, mode: u32) {
    fs::write(path, text).expect("the file is written");
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("the mode is set");
}

/// The verbs the page's relay token on beta admits, as the operating guide mints it for actions.
const ACTION_TOOLS: [&str; 3] = ["plan_list", "action_approve", "plan_decline"];

impl Pair {
    fn new(tag: &str, trust: BetaTrust) -> (Pair, SoftPasskey) {
        Pair::with_tools(tag, trust, &ACTION_TOOLS)
    }

    /// A pair whose relay token on beta admits `tools`.
    fn with_tools(tag: &str, trust: BetaTrust, tools: &[&str]) -> (Pair, SoftPasskey) {
        Self::with_begin(tag, trust, tools, None)
    }

    fn with_begin(
        tag: &str,
        trust: BetaTrust,
        tools: &[&str],
        begin: Option<Arc<dyn ToolHandler>>,
    ) -> (Pair, SoftPasskey) {
        let alpha_scratch = Scratch::new(&format!("{tag}-alpha"));
        let beta_scratch = Scratch::new(&format!("{tag}-beta"));
        let alpha_fake = start_fake(&alpha_scratch);
        let beta_fake = start_fake(&beta_scratch);
        let owner = SoftPasskey::from_seed(1);
        let clock = Arc::new(AtomicU64::new(NOW));

        // Alpha: the page host, whose own passkeys file holds the owner's passkey for its page origin.
        let alpha_passkeys = alpha_scratch.path("owner-passkeys");
        write_mode(
            &alpha_passkeys,
            &format!("{}\n", owner.line(NAMESPACE)),
            0o600,
        );
        let alpha_verifier = verifier_with_passkeys(
            &alpha_scratch,
            &[],
            &format!("{}\n", owner.line(NAMESPACE)),
            "alpha",
        );
        let alpha = Arc::new(Mutex::new(
            kernel(&alpha_scratch, &alpha_fake, alpha_verifier, NOW).expect("alpha opens"),
        ));

        // Beta: its own passkey answers only beta's page; alpha's file is trusted only as the config says.
        let beta_own = beta_key(2);
        let foreign = match trust {
            BetaTrust::AlphaFile => {
                ForeignPasskeys::new(vec![alpha_passkeys.clone()], vec![ORIGIN.to_string()])
            }
            BetaTrust::AlphaPeer | BetaTrust::None => ForeignPasskeys::default(),
            BetaTrust::Unreadable => {
                fs::set_permissions(&alpha_passkeys, fs::Permissions::from_mode(0o000))
                    .expect("mode 0000");
                ForeignPasskeys::new(vec![alpha_passkeys.clone()], vec![ORIGIN.to_string()])
            }
        };
        let beta_lines = parse_passkeys(format!("{}\n", beta_own.line(NAMESPACE)).as_bytes())
            .expect("beta's own line parses");
        let mut passkeys = PasskeyVerifier::new(
            beta_lines,
            FileSignCounts::open(&beta_scratch.path("beta-counts.jsonl")).expect("counts"),
        )
        .with_foreign(foreign.clone());
        if matches!(trust, BetaTrust::AlphaPeer) {
            let peer = PeerPasskeys::new(
                beta_scratch.path("peer-passkeys"),
                "alpha".to_string(),
                vec![ORIGIN.to_string()],
            );
            let lines = normalize_set(&[owner.line(NAMESPACE)]).expect("a key to sync");
            peer.write(&lines, NOW, None).expect("the synced keys");
            passkeys = passkeys.with_peer(peer);
        }
        let beta_verifier = ApprovalVerifier::new(
            BETA_HOST.to_string(),
            OwnerKeys::new(SshSigVerifier::new(Vec::new()), passkeys),
            FileNonces::open(&beta_scratch.path("beta-nonces.jsonl")).expect("nonces"),
        );
        let beta = Arc::new(Mutex::new(
            kernel_on_host(
                &beta_scratch,
                &beta_fake,
                beta_verifier,
                NOW,
                None,
                BETA_HOST,
            )
            .expect("beta opens"),
        ));

        // Beta's MCP door, with a doctor token limited to the relayed verbs.
        let tokens_dir = beta_scratch.path("tokens");
        private_dir(&tokens_dir);
        let tools: Vec<String> = tools.iter().map(|tool| tool.to_string()).collect();
        let (secret, hash) = mint_with_tools(Scope::Doctor, Some(&tools));
        write_mode(&tokens_dir.join("relay.hash"), &hash, 0o400);
        let (requester, hash) = mint_with_tools(Scope::Doctor, None);
        write_mode(&tokens_dir.join("requester.hash"), &hash, 0o400);
        let requester_file = beta_scratch.path("requester.secret");
        write_mode(&requester_file, &requester, 0o600);
        let token_file = alpha_scratch.path("beta-relay.secret");
        write_mode(&token_file, &format!("{secret}\n"), 0o600);
        let tokens = TokenStore::load(&tokens_dir).expect("the token store loads");
        let beta_runs = Arc::new(AtomicU64::new(0));
        let beta_clock = Arc::clone(&clock);
        let beta_clock: Clock = Arc::new(move || beta_clock.load(Ordering::SeqCst));
        let slot = ActionSlot::default();
        let mut beta_server = Server::new(arca_api::verbs(), tokens);
        for (verb, handler) in plan_handlers_with(
            Arc::clone(&beta),
            beta_clock.clone(),
            None,
            None,
            Arc::clone(&slot),
        ) {
            beta_server = beta_server.handle(verb, handler);
        }
        let _ = slot.set(executor(&beta_runs));
        let beta_socket = beta_scratch.path("beta.sock");
        let beta_state = beta_scratch.path("state");
        private_dir(&beta_state);
        let requests = Arc::new(ApprovalRequests::open(&beta_state, None, NOW).expect("requests"));
        for (verb, handler) in approval_requests::handlers(Arc::clone(&beta), beta_clock, requests)
        {
            beta_server = beta_server.handle(verb, handler);
        }
        if let Some(begin) = begin {
            beta_server = beta_server.handle("owner_approval_begin", begin);
        }

        // Alpha's page, linked to beta.
        let alpha_state = alpha_scratch.path("state");
        private_dir(&alpha_state);
        let alpha_runs = Arc::new(AtomicU64::new(0));
        let log = Arc::new(Mutex::new(Vec::new()));
        let alpha_clock = Arc::clone(&clock);
        let alpha_clock: Clock = Arc::new(move || alpha_clock.load(Ordering::SeqCst));
        let page = Page::new(
            ORIGIN.to_string(),
            Arc::clone(&alpha),
            alpha_clock,
            alpha_state,
            Arc::new(NoDisplay),
        )
        .with_actions(executor(&alpha_runs))
        .with_log({
            let log = Arc::clone(&log);
            Arc::new(move |line: &str| log.lock().expect("the log").push(line.to_string()))
        })
        .with_linked_hosts(vec![LinkedHost {
            name: "beta".to_string(),
            socket: beta_socket.clone(),
            token_file: token_file.clone(),
            incus_socket: None,
            ceph: None,
        }]);
        let alpha_tokens = alpha_scratch.path("alpha-tokens");
        private_dir(&alpha_tokens);
        let alpha_router = Server::new(
            arca_api::verbs(),
            TokenStore::load(&alpha_tokens).expect("tokens"),
        )
        .router()
        .merge(page.router(false));
        let alpha_socket = alpha_scratch.path("alpha.sock");

        let runtime = Runtime::new().expect("a runtime");
        let served_beta = beta_socket.clone();
        let beta_router = beta_server.router();
        runtime.spawn(async move {
            let _ = serve_unix(beta_router, &served_beta).await;
        });
        let served_alpha = alpha_socket.clone();
        runtime.spawn(async move {
            let _ = serve_unix(alpha_router, &served_alpha).await;
        });
        let until = Instant::now() + Duration::from_secs(5);
        while !beta_socket.exists() || !alpha_socket.exists() {
            assert!(Instant::now() < until, "both servers started");
            std::thread::sleep(Duration::from_millis(10));
        }
        let pair = Pair {
            _alpha_scratch: alpha_scratch,
            _beta_scratch: beta_scratch,
            _alpha_fake: alpha_fake,
            beta_fake,
            clock,
            log,
            alpha,
            beta,
            alpha_socket,
            alpha_runs,
            beta_runs,
            token_file,
            foreign,
            beta_state,
            beta_requester: LinkedHost {
                name: "beta".to_string(),
                socket: beta_socket,
                token_file: requester_file,
                incus_socket: None,
                ceph: None,
            },
            _runtime: runtime,
        };
        (pair, owner)
    }

    fn propose(kernel: &Arc<Mutex<Kernel>>) -> String {
        Pair::propose_named(kernel, "nats")
    }

    /// A bound `service_create` of `name` proposed on `kernel`.
    fn propose_named(kernel: &Arc<Mutex<Kernel>>, name: &str) -> String {
        kernel
            .lock()
            .expect("the kernel lock")
            .propose_action(
                arca::action_proposal::ActionKind::ServiceCreate,
                // Bound, as the node binds a create when it is proposed: an unbound one is never approved.
                json!({ "name": name, "template": "nats", "templateDigest": "ab".repeat(32) }),
                NOW,
                None,
            )
            .expect("a proposal")
            .id
    }

    /// A delete plan waiting on beta: a toy entry applied, then removed. Its digest.
    fn beta_delete_plan(&self) -> String {
        let mut kernel = self.beta.lock().expect("the kernel lock");
        kernel
            .propose(vec![EntryChange::Put(toy_entry("t1"))], NOW)
            .expect("applied");
        kernel
            .propose(
                vec![EntryChange::Remove(EntryId("toy-scratch".to_string()))],
                NOW + 1_000,
            )
            .expect("removed");
        kernel
            .pending_plans()
            .first()
            .expect("a waiting delete plan")
            .digest
            .to_hex()
    }

    /// The row the page lists for linked delete plan `plan`.
    fn delete_row(&self, plan: &str) -> Option<Value> {
        self.plans()["plans"]
            .as_array()
            .expect("plans")
            .iter()
            .find(|row| row["plan"] == json!(plan) && row["linkedHost"] == json!("beta"))
            .cloned()
    }

    fn statement_of(&self, id: &str) -> String {
        self.row(id, Some("beta")).expect("beta's row")["statement"]
            .as_str()
            .expect("a statement")
            .to_string()
    }

    fn request(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let payload = body.map(|value| value.to_string()).unwrap_or_default();
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: box1.example.com\r\nConnection: close\r\nContent-Length: {}\r\n",
            payload.len()
        );
        if method == "POST" {
            head.push_str(&format!(
                "Origin: {ORIGIN}\r\nContent-Type: application/json\r\n"
            ));
        }
        head.push_str("\r\n");
        let mut stream = UnixStream::connect(&self.alpha_socket).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("read timeout");
        stream
            .write_all(format!("{head}{payload}").as_bytes())
            .expect("write");
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).expect("read");
        let split = raw
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("a response head");
        let status = String::from_utf8_lossy(&raw[..split])
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|status| status.parse().ok())
            .expect("a status");
        let body = serde_json::from_slice(&raw[split + 4..]).unwrap_or(Value::Null);
        (status, body)
    }

    fn plans(&self) -> Value {
        let (status, body) = self.request("GET", "/approve/plans", None);
        assert_eq!(status, 200, "{body}");
        body
    }

    /// The row the page lists for `id`: alpha's own when `linked` is `None`, else the named linked host's.
    fn row(&self, id: &str, linked: Option<&str>) -> Option<Value> {
        self.plans()["plans"]
            .as_array()
            .expect("plans")
            .iter()
            .find(|row| {
                row["id"] == json!(id) && row.get("linkedHost").and_then(Value::as_str) == linked
            })
            .cloned()
    }

    fn waiting(kernel: &Arc<Mutex<Kernel>>, id: &str) -> bool {
        kernel
            .lock()
            .expect("the kernel lock")
            .action_proposals()
            .iter()
            .any(|proposal| proposal.id == id)
    }
}

/// A passkey change statement for beta: `action` is `passkey_add` or `passkey_remove`, over `line`.
fn passkey_change(action: &str, line: &str, nonce: &str) -> String {
    canonical_json(&json!({
        "action": action,
        "host": BETA_HOST,
        "target": format!("passkey-{}", Digest::of(line.as_bytes()).to_hex()),
        "paramsDigest": params_digest(&json!({ "line": line })).expect("the digest"),
        "issuedAt": arca::handlers::iso_utc(NOW),
        "expiresAt": arca::handlers::iso_utc(NOW + 600_000),
        "nonce": nonce,
    }))
    .expect("the statement")
}

fn approval(key: &SoftPasskey, statement: &str) -> String {
    passkey_approval(
        statement,
        &key.assert(&Assert::over(NAMESPACE, statement.as_bytes())),
    )
}

const REQUEST_TOOLS: [&str; 5] = [
    "owner_approval_list",
    "owner_approval_begin",
    "owner_approval_approve",
    "owner_approval_deny",
    "owner_approval_status",
];

fn enroll_request(pair: &Pair) -> (Value, Value) {
    let input = json!({ "target": "AB12CD", "args": { "service": "example-service" } });
    let mut args = input.clone();
    args["kind"] = json!("arca");
    args["action"] = json!("enroll");
    let made = linked::call(&pair.beta_requester, "owner_approval_request", args).expect("request");
    assert_eq!(made["dashboard"], Value::Null, "beta serves no page");
    (made["id"].clone(), input)
}

fn request_step(pair: &Pair, step: &str, body: Value) -> (u16, Value) {
    pair.request("POST", &format!("/approve/requests/{step}"), Some(body))
}

fn assert_request_audit(pair: &Pair, tool: &str, id: &Value) {
    let audit = fs::read_to_string(pair.beta_state.join(approval_requests::AUDIT_FILE))
        .expect("linked kernel audit");
    let prefix = format!("{}:", id.as_str().expect("request id"));
    let decision = audit
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("audit line"))
        .find(|line| line["tool"] == tool)
        .expect("recorded decision");
    assert_eq!(decision["principal"], "mcp:relay");
    assert!(
        decision["outcome"]
            .as_str()
            .expect("outcome")
            .contains(&prefix)
    );
}

#[test]
fn the_page_refuses_a_begun_statement_for_a_different_host_before_signing() {
    for field in [
        "host",
        "nonce",
        "issuedAt",
        "expiresAt",
        "action",
        "target",
        "paramsDigest",
    ] {
        let (pair, _) = Pair::with_begin(
            "req-foreign",
            BetaTrust::AlphaPeer,
            &REQUEST_TOOLS,
            Some(Arc::new(SubstitutedRequestStatement(field))),
        );
        let (id, _) = enroll_request(&pair);
        let row = pair.plans()["approvalRequests"][0].clone();
        assert_eq!(row["host"], BETA_HOST);
        assert_eq!(row["id"], id);
        let (status, body) = request_step(
            &pair,
            "begin",
            json!({ "host": "beta", "id": id, "card": row }),
        );
        assert_eq!(status, 502, "{field}: {body}");
        assert!(body.get("statement").is_none());
        assert_eq!(body["refusal"], "InvalidStatement");
        assert!(
            body["message"]
                .as_str()
                .unwrap_or_default()
                .contains("card")
        );
    }
}

#[test]
fn the_page_lists_a_linked_enroll_request_and_only_the_requester_can_spend_its_approval() {
    let (pair, owner) = Pair::with_tools("req-ok", BetaTrust::AlphaPeer, &REQUEST_TOOLS);
    let (id, input) = enroll_request(&pair);
    let plans = pair.plans();
    let rows = plans["approvalRequests"].as_array().expect("request cards");
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row["id"], id);
    assert_eq!(row["linkedHost"], "beta");
    assert_eq!(row["state"], "pending");
    assert_eq!(row["fields"]["action"], "enroll");
    assert_eq!(row["fields"]["target"], input["target"]);
    assert_eq!(row["args"], input["args"]);

    let begin = || {
        let (status, body) = request_step(
            &pair,
            "begin",
            json!({ "host": "beta", "id": id, "card": row }),
        );
        assert_eq!(status, 200, "{body}");
        body["statement"].as_str().expect("statement").to_string()
    };
    let first: Value = serde_json::from_str(&begin()).expect("statement JSON");
    let statement = begin();
    let fields: Value = serde_json::from_str(&statement).expect("statement JSON");
    assert_eq!(fields["host"], BETA_HOST);
    assert_eq!(fields["action"], "enroll");
    assert_eq!(fields["target"], input["target"]);
    assert_eq!(fields["paramsDigest"], row["fields"]["paramsDigest"]);
    assert_ne!(fields["nonce"], first["nonce"]);
    let request_hex = id.as_str().unwrap().strip_prefix("ar_").unwrap();
    let nonce = fields["nonce"].as_str().unwrap();
    assert_eq!(nonce.len(), 48);
    assert!(nonce.starts_with(request_hex));
    assert_eq!(fields["issuedAt"], arca::handlers::iso_utc(NOW));
    assert_eq!(fields["expiresAt"], arca::handlers::iso_utc(NOW + 900_000));

    let proof = owner.proof_value(&Assert::over(NAMESPACE, statement.as_bytes()));
    let (status, body) = request_step(
        &pair,
        "approve",
        json!({ "host": "beta", "id": id, "statement": statement, "proof": proof }),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["state"], "approved");
    assert!(body.get("approval").is_none());
    assert_request_audit(&pair, approval_requests::APPROVED_AUDIT, &id);
    assert_eq!(pair.plans()["approvalRequests"], json!([]));

    let status_args = json!({ "id": id });
    let mut relay = pair.beta_requester.clone();
    relay.token_file = pair.token_file.clone();
    let other =
        linked::call(&relay, "owner_approval_status", status_args.clone()).expect("other status");
    assert_eq!(other["state"], "approved");
    assert!(other.get("approval").is_none());
    let own = linked::call(&pair.beta_requester, "owner_approval_status", status_args)
        .expect("requester status");
    assert_eq!(own["state"], "approved");
    let mut confirm = input;
    confirm["approval"] = json!(own["approval"].as_str().expect("approval for requester"));
    assert_eq!(
        linked::call(&pair.beta_requester, "enroll_confirm", confirm.clone()),
        Ok(json!({ "confirmed": true }))
    );
    assert!(matches!(
        linked::call(&pair.beta_requester, "enroll_confirm", confirm),
        Err(RelayError::Refused { code, .. }) if code == "owner_approval"
    ));
}

#[test]
fn the_page_relays_request_refusals_and_declines_to_the_linked_kernel() {
    let (pair, owner) = Pair::with_tools("req-no", BetaTrust::None, &REQUEST_TOOLS);
    let (id, _) = enroll_request(&pair);
    let named = json!({ "host": "beta", "id": id });
    let status = || {
        linked::call(
            &pair.beta_requester,
            "owner_approval_status",
            json!({ "id": id }),
        )
        .expect("status")["state"]
            .clone()
    };
    let (code, body) = request_step(&pair, "deny", json!({ "host": "unknown", "id": id }));
    assert_eq!(code, 403);
    assert_eq!(body["refusal"], "NotLinked");
    assert_eq!(status(), "pending");

    let row = pair.plans()["approvalRequests"][0].clone();
    let (code, body) = request_step(
        &pair,
        "begin",
        json!({ "host": "beta", "id": id, "card": row }),
    );
    assert_eq!(code, 200, "{body}");
    let statement = body["statement"].as_str().expect("statement");
    let proof = owner.proof_value(&Assert::over(NAMESPACE, statement.as_bytes()));
    let (code, body) = request_step(
        &pair,
        "approve",
        json!({ "host": "beta", "id": id, "statement": statement, "proof": proof }),
    );
    let Err(RelayError::Refused {
        code: expected,
        message,
    }) = linked::call(
        &pair.beta_requester,
        "owner_approval_approve",
        json!({ "id": id, "statement": statement, "proof": proof }),
    )
    else {
        panic!("the target kernel must refuse the untrusted passkey");
    };
    assert_eq!(code, 403);
    assert_eq!(expected, "assertion_refused");
    assert_eq!(body, json!({ "refusal": expected, "message": message }));
    assert_eq!(status(), "pending");

    let (code, body) = request_step(&pair, "deny", named.clone());
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["state"], "denied");
    assert_request_audit(&pair, approval_requests::DENIED_AUDIT, &id);
    assert_eq!(status(), "denied");
    assert_eq!(pair.plans()["approvalRequests"], json!([]));
}

#[test]
fn a_request_relay_reports_a_refused_token_and_an_unreachable_socket() {
    let (pair, _) = Pair::new("req-link", BetaTrust::AlphaPeer);
    let (id, _) = enroll_request(&pair);
    let request = json!({ "host": "beta", "id": id });
    let (status, body) = request_step(&pair, "deny", request.clone());
    assert_eq!(status, 502);
    assert_eq!(body["refusal"], "LinkToken");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("owner_approval_deny")
    );
    assert_eq!(
        linked::call(
            &pair.beta_requester,
            "owner_approval_status",
            json!({ "id": id })
        )
        .expect("status")["state"],
        "pending"
    );

    fs::remove_file(&pair.beta_requester.socket).expect("unlink the target test socket");
    let (status, body) = request_step(&pair, "deny", request);
    assert_eq!(status, 502);
    assert_eq!(body["refusal"], "Unreachable");
}

#[test]
fn a_linked_hosts_proposal_is_listed_with_its_host_and_approved_by_the_target_once() {
    let (pair, owner) = Pair::new("mh-approve", BetaTrust::AlphaFile);
    let alpha_id = Pair::propose(&pair.alpha);
    let beta_id = Pair::propose(&pair.beta);
    assert_eq!(alpha_id, beta_id, "the same change has the same id on both");
    let id = beta_id;

    let plans = pair.plans();
    assert_eq!(
        plans["linkedHosts"],
        json!([{ "name": "beta", "read": true }])
    );
    let own = pair.row(&id, None).expect("alpha's own row");
    let linked = pair.row(&id, Some("beta")).expect("beta's row, labelled");
    assert_eq!(own["host"], json!(HOST));
    assert_eq!(linked["host"], json!(BETA_HOST));
    let statement = linked["statement"].as_str().expect("a statement");
    assert_eq!(
        serde_json::from_str::<Value>(statement).expect("the statement")["host"],
        json!(BETA_HOST),
        "the statement names the target's host"
    );

    let body = json!({ "plan": id, "approval": approval(&owner, statement), "host": "beta" });
    let (status, answer) = pair.request("POST", "/approve/approve", Some(body.clone()));
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["plan"], json!(id));
    assert_eq!(answer["host"], json!("beta"));
    assert_eq!(answer["result"], json!({ "created": true }));
    assert_eq!(pair.beta_runs.load(Ordering::SeqCst), 1, "beta ran it once");
    assert_eq!(
        pair.alpha_runs.load(Ordering::SeqCst),
        0,
        "alpha ran nothing"
    );
    assert!(!Pair::waiting(&pair.beta, &id), "beta retired its proposal");
    assert!(
        Pair::waiting(&pair.alpha, &id),
        "alpha's proposal with the same id still waits: the page never approved it"
    );
    assert!(pair.row(&id, Some("beta")).is_none());
}

#[test]
fn a_replayed_approval_is_refused_by_the_target() {
    let (pair, owner) = Pair::new("mh-replay", BetaTrust::AlphaFile);
    let id = Pair::propose(&pair.beta);
    let statement = pair.row(&id, Some("beta")).expect("beta's row")["statement"]
        .as_str()
        .expect("a statement")
        .to_string();
    let body = json!({ "plan": id, "approval": approval(&owner, &statement), "host": "beta" });
    assert_eq!(
        pair.request("POST", "/approve/approve", Some(body.clone()))
            .0,
        200
    );

    // The same change proposed again has the same id; the spent approval must not release it.
    assert_eq!(Pair::propose(&pair.beta), id);
    let (status, answer) = pair.request("POST", "/approve/approve", Some(body));
    assert_eq!(status, 403, "{answer}");
    assert_eq!(
        answer["refusal"],
        json!("Replayed"),
        "beta's nonce store refuses it"
    );
    assert_eq!(pair.beta_runs.load(Ordering::SeqCst), 1);
    assert!(Pair::waiting(&pair.beta, &id));
}

#[test]
fn a_statement_naming_another_host_is_refused_by_the_target() {
    let (pair, owner) = Pair::new("mh-wrong-host", BetaTrust::AlphaFile);
    let id = Pair::propose(&pair.beta);
    let row = pair.row(&id, Some("beta")).expect("beta's row");
    let wrong = action_statement(
        HOST,
        &id,
        "service_create",
        row["argsDigest"].as_str().expect("a digest"),
        NOW,
        &"7".repeat(32),
    )
    .expect("a statement");
    let (status, answer) = pair.request(
        "POST",
        "/approve/approve",
        Some(json!({ "plan": id, "approval": approval(&owner, &wrong), "host": "beta" })),
    );
    assert_eq!(status, 403, "{answer}");
    assert_eq!(answer["refusal"], json!("WrongHost"));
    assert_eq!(pair.beta_runs.load(Ordering::SeqCst), 0);
    assert!(Pair::waiting(&pair.beta, &id));
}

#[test]
fn a_target_without_the_trust_key_refuses_the_owners_passkey_as_unknown() {
    let (pair, owner) = Pair::new("mh-no-trust", BetaTrust::None);
    let id = Pair::propose(&pair.beta);
    let statement = pair.row(&id, Some("beta")).expect("beta's row")["statement"]
        .as_str()
        .expect("a statement")
        .to_string();
    let (status, answer) = pair.request(
        "POST",
        "/approve/approve",
        Some(json!({ "plan": id, "approval": approval(&owner, &statement), "host": "beta" })),
    );
    assert_eq!(status, 403, "{answer}");
    assert_eq!(
        answer["refusal"],
        json!("BadSignature"),
        "alpha's passkey is not one beta knows"
    );
    assert_eq!(pair.beta_runs.load(Ordering::SeqCst), 0);
    assert!(Pair::waiting(&pair.beta, &id));
}

#[test]
fn an_unreadable_trust_file_adds_no_key_and_arca_status_says_why() {
    let (pair, owner) = Pair::new("mh-unreadable", BetaTrust::Unreadable);
    let id = Pair::propose(&pair.beta);
    let statement = pair.row(&id, Some("beta")).expect("beta's row")["statement"]
        .as_str()
        .expect("a statement")
        .to_string();
    let (status, answer) = pair.request(
        "POST",
        "/approve/approve",
        Some(json!({ "plan": id, "approval": approval(&owner, &statement), "host": "beta" })),
    );
    assert_eq!(status, 403, "{answer}");
    assert_eq!(
        answer["refusal"],
        json!("BadSignature"),
        "the unreadable file trusted nothing"
    );
    assert_eq!(pair.beta_runs.load(Ordering::SeqCst), 0);

    let status = StatusHandler {
        update: None,
        node: "b".repeat(64),
        mode: "kernel",
        applied_seq: AppliedSeq::default(),
        started: Instant::now(),
        socket: PathBuf::from("/nonexistent/incus.sock"),
        metrics_source: Arc::new(Mutex::new(HostModeChoice {
            mode: HostMode::Local,
            reason: "the test names the local mode".to_string(),
            provisional: false,
        })),
        approval: Some(ApprovalStatus {
            linked_hosts: Vec::new(),
            foreign: pair.foreign.clone(),
            peer: None,
            peer_sources: None,
        }),
        loopback: None,
        page: None,
    }
    .call(json!({}), &caller(Scope::Viewer))
    .expect("arca_status answers");
    // The approval check just read the file: the status reports that read, by index and file name, never the path.
    assert_eq!(
        status["approval"]["trustedKeyFiles"],
        json!([{
            "index": 0,
            "name": "owner-passkeys",
            "keys": 0,
            "reason": "the file cannot be read by this kernel",
        }])
    );
    assert!(
        !status
            .to_string()
            .contains(&*pair._alpha_scratch.dir.display().to_string())
    );
    assert!(
        arca_api::check::conforms(
            &(arca_api::verb("arca_status").expect("the verb").result)(),
            &status
        )
        .is_ok(),
        "the status still fits its schema"
    );
}

#[test]
fn the_page_host_never_approves_a_foreign_id_itself_and_refuses_an_unlinked_host() {
    let (pair, owner) = Pair::new("mh-foreign-id", BetaTrust::AlphaFile);
    let id = Pair::propose(&pair.beta);
    let statement = pair.row(&id, Some("beta")).expect("beta's row")["statement"]
        .as_str()
        .expect("a statement")
        .to_string();
    let signed = approval(&owner, &statement);

    // Without a host the page asks only its own kernel, which holds no such proposal.
    let (status, answer) = pair.request(
        "POST",
        "/approve/approve",
        Some(json!({ "plan": id, "approval": signed })),
    );
    assert_eq!(status, 403, "{answer}");
    assert_eq!(answer["refusal"], json!("NoSuchPlan"));

    // A host the page does not list is refused in plain words, and nothing is relayed.
    let (status, answer) = pair.request(
        "POST",
        "/approve/approve",
        Some(json!({ "plan": id, "approval": signed, "host": "gamma" })),
    );
    assert_eq!(status, 403, "{answer}");
    assert_eq!(answer["refusal"], json!("NotLinked"));
    assert_eq!(
        answer["message"],
        json!("this machine is not linked for approvals yet")
    );
    let (status, _) = pair.request(
        "POST",
        "/approve/decline",
        Some(json!({ "plan": id, "host": "gamma" })),
    );
    assert_eq!(status, 403);

    assert_eq!(pair.alpha_runs.load(Ordering::SeqCst), 0);
    assert_eq!(pair.beta_runs.load(Ordering::SeqCst), 0);
    assert!(
        Pair::waiting(&pair.beta, &id),
        "beta's proposal still waits"
    );
}

#[test]
fn a_decline_is_relayed_to_the_linked_host_only() {
    let (pair, _owner) = Pair::new("mh-decline", BetaTrust::AlphaFile);
    let id = Pair::propose(&pair.alpha);
    assert_eq!(Pair::propose(&pair.beta), id);
    let (status, answer) = pair.request(
        "POST",
        "/approve/decline",
        Some(json!({ "plan": id, "host": "beta" })),
    );
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["host"], json!("beta"));
    assert!(!Pair::waiting(&pair.beta, &id));
    assert!(
        Pair::waiting(&pair.alpha, &id),
        "alpha's own proposal is untouched"
    );
    assert_eq!(
        *pair.log.lock().expect("the log"),
        vec![format!("arca: approval of {id} on beta: declined")],
        "one line, and it says declined"
    );
}

#[test]
fn a_token_the_linked_host_refuses_relays_nothing() {
    let (pair, owner) = Pair::new("mh-token", BetaTrust::AlphaFile);
    let id = Pair::propose(&pair.beta);
    let statement = pair.row(&id, Some("beta")).expect("beta's row")["statement"]
        .as_str()
        .expect("a statement")
        .to_string();
    write_mode(
        &pair.token_file,
        &format!("arca_{}\n", "A".repeat(43)),
        0o600,
    );
    let plans = pair.plans();
    assert_eq!(
        plans["linkedHosts"],
        json!([{ "name": "beta", "read": false }])
    );
    let (status, answer) = pair.request(
        "POST",
        "/approve/approve",
        Some(json!({ "plan": id, "approval": approval(&owner, &statement), "host": "beta" })),
    );
    assert_eq!(status, 502, "{answer}");
    assert_eq!(answer["refusal"], json!("LinkToken"));
    assert_eq!(pair.beta_runs.load(Ordering::SeqCst), 0);
    assert!(Pair::waiting(&pair.beta, &id));
}

#[test]
fn a_passkey_trusted_from_another_kernel_cannot_add_or_remove_the_targets_own_passkeys() {
    let (pair, owner) = Pair::new("mh-passkeys", BetaTrust::AlphaFile);
    let beta_own = beta_key(2);
    let state = pair.beta_state.join(STATE_PASSKEYS_FILE);
    let change = |action: &str, line: &str, key: &SoftPasskey, nonce: &str| {
        let statement = passkey_change(action, line, nonce);
        let proof = key.assert(&Assert::over(NAMESPACE, statement.as_bytes()));
        pair.beta
            .lock()
            .expect("the kernel lock")
            .approve_passkey_change(&pair.beta_state, line, &statement, &proof, NOW)
    };
    let count = || {
        pair.beta
            .lock()
            .expect("the kernel lock")
            .owner_passkey_count()
    };
    let foreign_refused = |result: Result<(), ApprovalRefusal>| match result {
        Err(ApprovalRefusal::Untrusted(why)) => {
            assert!(why.contains("another kernel"), "a plain reason: {why}")
        }
        other => panic!("the foreign passkey is refused, not {other:?}"),
    };

    // Alpha's passkey is trusted on beta for actions, but adding a key to beta with it is refused and writes nothing.
    let added = beta_key(3).line(NAMESPACE);
    foreign_refused(change(
        "passkey_add",
        &added,
        &owner,
        "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1",
    ));
    assert!(!state.exists(), "nothing is written");
    assert_eq!(count(), 1);

    // Beta's own passkey still adds it.
    change(
        "passkey_add",
        &added,
        &beta_own,
        "b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2",
    )
    .expect("beta's own passkey adds a key");
    let before = fs::read(&state).expect("the state file");
    assert_eq!(count(), 2);

    // Alpha's passkey cannot remove it; beta's own passkey can.
    foreign_refused(change(
        "passkey_remove",
        &added,
        &owner,
        "c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3",
    ));
    assert_eq!(
        fs::read(&state).expect("the state file"),
        before,
        "nothing is written"
    );
    assert_eq!(count(), 2);
    change(
        "passkey_remove",
        &added,
        &beta_own,
        "d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4",
    )
    .expect("beta's own passkey removes the key");
    assert_eq!(count(), 1);

    // The same alpha passkey still approves beta's actions: the trust is for approvals only.
    let id = Pair::propose(&pair.beta);
    let statement = pair.row(&id, Some("beta")).expect("beta's row")["statement"]
        .as_str()
        .expect("a statement")
        .to_string();
    let (status, answer) = pair.request(
        "POST",
        "/approve/approve",
        Some(json!({ "plan": id, "approval": approval(&owner, &statement), "host": "beta" })),
    );
    assert_eq!(status, 200, "{answer}");
    assert_eq!(pair.beta_runs.load(Ordering::SeqCst), 1);
}

#[test]
fn a_token_file_that_is_a_link_or_open_to_others_is_never_read() {
    let (pair, owner) = Pair::new("mh-token-file", BetaTrust::AlphaFile);
    let id = Pair::propose(&pair.beta);
    let statement = pair.row(&id, Some("beta")).expect("beta's row")["statement"]
        .as_str()
        .expect("a statement")
        .to_string();
    let secret = fs::read_to_string(&pair.token_file).expect("the token file");
    let real = pair.token_file.with_extension("real");
    write_mode(&real, &secret, 0o600);
    let refused = |why: &str| {
        assert_eq!(
            pair.plans()["linkedHosts"],
            json!([{ "name": "beta", "read": false }]),
            "{why}"
        );
        let (status, answer) = pair.request(
            "POST",
            "/approve/approve",
            Some(json!({ "plan": id, "approval": approval(&owner, &statement), "host": "beta" })),
        );
        assert_eq!(status, 502, "{why}: {answer}");
        assert_eq!(answer["refusal"], json!("LinkToken"), "{why}");
        assert_eq!(pair.beta_runs.load(Ordering::SeqCst), 0, "{why}");
    };

    // A link to a good token file is not followed.
    fs::remove_file(&pair.token_file).expect("the token file is removed");
    std::os::unix::fs::symlink(&real, &pair.token_file).expect("a link");
    refused("a link");

    // The right token in a file group or others may write is not read.
    fs::remove_file(&pair.token_file).expect("the link is removed");
    write_mode(&pair.token_file, &secret, 0o664);
    refused("a group-writable file");
    fs::set_permissions(&pair.token_file, fs::Permissions::from_mode(0o640)).expect("mode 0640");
    refused("a group-readable file");

    // The same token at mode 0600 relays.
    fs::set_permissions(&pair.token_file, fs::Permissions::from_mode(0o600)).expect("mode 0600");
    let (status, answer) = pair.request(
        "POST",
        "/approve/approve",
        Some(json!({ "plan": id, "approval": approval(&owner, &statement), "host": "beta" })),
    );
    assert_eq!(status, 200, "{answer}");
    assert_eq!(pair.beta_runs.load(Ordering::SeqCst), 1);
}

/// A stand-in for `watch_report`: one instance, named for the kernel that answers.
struct Report(&'static str);

impl ToolHandler for Report {
    fn call(&self, _args: Value, _caller: &Caller) -> Result<Value, CallError> {
        Ok(json!({
            "at": NOW,
            "projects": [{ "project": "default", "instances": [{
                "name": self.0,
                "kind": "container",
                "status": "Running",
                "owner": { "class": "foreign" },
                "privileged": false,
                "identity": false,
                "configHash": "cd".repeat(32),
            }]}],
            "drift": [],
        }))
    }
}

/// The pair, with one MCP door more on each kernel: alpha's door relays a call that names `node` to beta's second door,
/// with a viewer token beta keeps for alpha's door. `gamma` is linked too, at a socket nothing serves. The page and the
/// passkey path are the pair's own.
struct Fleet {
    pair: Pair,
    owner: SoftPasskey,
    door: PathBuf,
    /// A viewer token of alpha's door that may call every tool its scope admits.
    agent: String,
    /// A viewer token of alpha's door that may read, and may not propose.
    reader: String,
    /// A doctor token of alpha's door, for the writes no linked node is asked.
    doctor: String,
    _scratch: Scratch,
    _runtime: Runtime,
}

impl Fleet {
    fn new(tag: &str) -> Fleet {
        let (pair, owner) = Pair::new(tag, BetaTrust::AlphaFile);
        let scratch = Scratch::new(&format!("{tag}-doors"));
        let clock: Clock = Arc::new(|| NOW);

        let beta_tokens = scratch.path("beta-tokens");
        private_dir(&beta_tokens);
        let (relay, relay_hash) = mint_with_tools(Scope::Viewer, None);
        write_mode(&beta_tokens.join("fleet.hash"), &relay_hash, 0o400);
        let relay_file = scratch.path("beta-fleet.secret");
        write_mode(&relay_file, &format!("{relay}\n"), 0o600);
        let mut beta_door = Server::new(
            arca_api::verbs(),
            TokenStore::load(&beta_tokens).expect("beta's tokens"),
        )
        .handle("watch_report", Arc::new(Report("beta-web")))
        .handle("arca_status", Arc::new(status_of(&"b".repeat(64))));
        for (verb, handler) in plan_handlers_with(
            Arc::clone(&pair.beta),
            Arc::clone(&clock),
            None,
            None,
            ActionSlot::default(),
        ) {
            beta_door = beta_door.handle(verb, handler);
        }

        let alpha_tokens = scratch.path("alpha-tokens");
        private_dir(&alpha_tokens);
        let (agent, agent_hash) = mint_with_tools(Scope::Viewer, None);
        write_mode(&alpha_tokens.join("agent.hash"), &agent_hash, 0o400);
        let reads = ["watch_report".to_string(), "plan_list".to_string()];
        let (reader, reader_hash) = mint_with_tools(Scope::Viewer, Some(&reads));
        write_mode(&alpha_tokens.join("reader.hash"), &reader_hash, 0o400);
        let (doctor, doctor_hash) = mint_with_tools(Scope::Doctor, None);
        write_mode(&alpha_tokens.join("doctor.hash"), &doctor_hash, 0o400);
        let beta_socket = scratch.path("beta-fleet.sock");
        let mut alpha_door = Server::new(
            arca_api::verbs(),
            TokenStore::load(&alpha_tokens).expect("alpha's tokens"),
        )
        .handle("watch_report", Arc::new(Report("alpha-web")))
        .linked_nodes(Arc::new(arca::linked::LinkedNodes {
            hosts: vec![
                LinkedHost {
                    name: "beta".to_string(),
                    socket: beta_socket.clone(),
                    token_file: relay_file.clone(),
                    incus_socket: None,
                    ceph: None,
                },
                LinkedHost {
                    name: "gamma".to_string(),
                    socket: scratch.path("gamma.sock"),
                    token_file: relay_file,
                    incus_socket: None,
                    ceph: None,
                },
                // Beta's page door, with the page's token: it admits only the three verbs the page relays.
                LinkedHost {
                    name: "beta-page".to_string(),
                    socket: pair._beta_scratch.path("beta.sock"),
                    token_file: pair.token_file.clone(),
                    incus_socket: None,
                    ceph: None,
                },
            ],
        }));
        for (verb, handler) in plan_handlers_with(
            Arc::clone(&pair.alpha),
            clock,
            None,
            None,
            ActionSlot::default(),
        ) {
            alpha_door = alpha_door.handle(verb, handler);
        }
        let door = scratch.path("alpha-door.sock");

        let runtime = Runtime::new().expect("a runtime");
        let (served_beta, beta_router) = (beta_socket.clone(), beta_door.router());
        runtime.spawn(async move {
            let _ = serve_unix(beta_router, &served_beta).await;
        });
        let (served_alpha, alpha_router) = (door.clone(), alpha_door.router());
        runtime.spawn(async move {
            let _ = serve_unix(alpha_router, &served_alpha).await;
        });
        let until = Instant::now() + Duration::from_secs(5);
        while !beta_socket.exists() || !door.exists() {
            assert!(Instant::now() < until, "both doors started");
            std::thread::sleep(Duration::from_millis(10));
        }
        Fleet {
            pair,
            owner,
            door,
            agent,
            reader,
            doctor,
            _scratch: scratch,
            _runtime: runtime,
        }
    }

    /// One `tools/call` of `verb` on alpha's door with `token`: the HTTP status and the JSON-RPC reply.
    fn call(&self, token: &str, verb: &str, arguments: Value) -> (u16, Value) {
        self.rpc(
            token,
            "tools/call",
            json!({ "name": verb, "arguments": arguments }),
        )
    }

    /// One JSON-RPC request of `method` on alpha's door with `token`: the HTTP status and the reply.
    fn rpc(&self, token: &str, method: &str, params: Value) -> (u16, Value) {
        let payload =
            json!({ "jsonrpc": "2.0", "id": 7, "method": method, "params": params }).to_string();
        let request = format!(
            "POST /mcp HTTP/1.1\r\nHost: arca\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
            payload.len()
        );
        let mut stream = UnixStream::connect(&self.door).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("read timeout");
        stream.write_all(request.as_bytes()).expect("write");
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).expect("read");
        let split = raw
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("a response head");
        let status = String::from_utf8_lossy(&raw[..split])
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|status| status.parse().ok())
            .expect("a status");
        (
            status,
            serde_json::from_slice(&raw[split + 4..]).unwrap_or(Value::Null),
        )
    }

    fn propose_on(&self, token: &str, node: Option<&str>) -> (u16, Value) {
        self.propose_named(token, node, "nats")
    }

    fn propose_named(&self, token: &str, node: Option<&str>, name: &str) -> (u16, Value) {
        let mut arguments = json!({
            "action": "service_create",
            "args": { "name": name, "template": "nats", "templateDigest": "ab".repeat(32) },
        });
        if let Some(node) = node {
            arguments["node"] = json!(node);
        }
        self.call(token, "action_propose", arguments)
    }
}

#[test]
fn a_read_that_names_a_linked_node_answers_with_that_nodes_instances_and_names_it() {
    let fleet = Fleet::new("mh-node-read");
    let (status, reply) = fleet.call(&fleet.agent, "watch_report", json!({ "node": "beta" }));
    assert_eq!(status, 200, "{reply}");
    let structured = &reply["result"]["structuredContent"];
    assert_eq!(
        structured["projects"][0]["instances"][0]["name"],
        json!("beta-web"),
        "{reply}"
    );
    assert_eq!(structured["linkedNode"], json!("beta"));

    // Without `node` the call is this kernel's own, as it always was: no field is added.
    let (status, reply) = fleet.call(&fleet.agent, "watch_report", json!({}));
    assert_eq!(status, 200, "{reply}");
    let structured = &reply["result"]["structuredContent"];
    assert_eq!(
        structured["projects"][0]["instances"][0]["name"],
        json!("alpha-web")
    );
    assert!(structured.get("linkedNode").is_none(), "{reply}");
}

#[test]
fn a_proposal_on_a_linked_node_waits_there_and_the_owner_approves_it_on_this_page() {
    let fleet = Fleet::new("mh-node-propose");
    let pair = &fleet.pair;
    let (status, reply) = fleet.propose_on(&fleet.agent, Some("beta"));
    assert_eq!(status, 200, "{reply}");
    let proposal = &reply["result"]["structuredContent"];
    assert_eq!(proposal["linkedNode"], json!("beta"), "{reply}");
    let id = proposal["id"].as_str().expect("an id").to_string();
    assert!(Pair::waiting(&pair.beta, &id), "it waits on beta");
    assert!(!Pair::waiting(&pair.alpha, &id), "nothing waits on alpha");

    // Alpha's page lists it under beta, and the owner's passkey approval is relayed to beta, which runs it once.
    assert!(pair.row(&id, None).is_none());
    let card = pair
        .row(&id, Some("beta"))
        .expect("beta's card on alpha's page");
    let statement = card["statement"].as_str().expect("a statement");
    let body = json!({ "plan": id, "approval": approval(&fleet.owner, statement), "host": "beta" });
    let (status, answer) = pair.request("POST", "/approve/approve", Some(body));
    assert_eq!(status, 200, "{answer}");
    assert_eq!(pair.beta_runs.load(Ordering::SeqCst), 1, "beta ran it once");
    assert_eq!(
        pair.alpha_runs.load(Ordering::SeqCst),
        0,
        "alpha ran nothing"
    );
    assert!(!Pair::waiting(&pair.beta, &id));
}

#[test]
fn a_node_that_is_not_linked_is_an_invalid_argument_naming_the_linked_nodes() {
    let fleet = Fleet::new("mh-node-unknown");
    let (status, reply) = fleet.call(&fleet.agent, "watch_report", json!({ "node": "delta" }));
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["result"]["isError"], json!(true), "{reply}");
    let text = reply["result"]["content"][0]["text"]
        .as_str()
        .expect("a text");
    assert!(
        text.starts_with("invalid_arguments: /node: not a linked node"),
        "{text}"
    );
    assert!(
        text.ends_with("the linked nodes are beta, gamma, beta-page"),
        "{text}"
    );
    assert!(
        !text.contains("delta"),
        "the refusal never echoes the value"
    );
    assert!(reply["result"].get("structuredContent").is_none());
}

#[test]
fn a_linked_node_that_does_not_answer_is_a_refusal_naming_it_never_this_kernels_answer() {
    let fleet = Fleet::new("mh-node-down");
    let (status, reply) = fleet.call(&fleet.agent, "watch_report", json!({ "node": "gamma" }));
    assert_eq!(status, 200, "{reply}");
    assert!(reply.get("result").is_none(), "{reply}");
    assert_eq!(reply["error"]["code"], json!(-32000));
    assert_eq!(reply["error"]["data"]["refusal"], json!("Unreachable"));
    assert_eq!(reply["error"]["data"]["linkedNode"], json!("gamma"));
    assert!(
        reply["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("gamma")),
        "{reply}"
    );
}

#[test]
fn a_caller_that_may_not_propose_here_may_not_propose_on_a_linked_node() {
    let fleet = Fleet::new("mh-node-scope");
    // The reader's tool list admits no proposal on this kernel, and so none on beta, though beta's token would.
    assert_eq!(fleet.propose_on(&fleet.reader, None).0, 403);
    assert_eq!(fleet.propose_on(&fleet.reader, Some("beta")).0, 403);
    // A verb above the caller's scope is refused here too, whatever the linked token holds.
    let (status, _) = fleet.call(&fleet.agent, "token_list", json!({ "node": "beta" }));
    assert_eq!(status, 403);
    assert!(
        fleet
            .pair
            .beta
            .lock()
            .expect("the kernel lock")
            .action_proposals()
            .is_empty()
    );
    // The reader still reads beta.
    let (status, reply) = fleet.call(&fleet.reader, "watch_report", json!({ "node": "beta" }));
    assert_eq!(status, 200);
    assert_eq!(
        reply["result"]["structuredContent"]["linkedNode"],
        json!("beta")
    );
}

/// A status handler for a kernel whose node id is `node`, with no Incus to reach.
fn status_of(node: &str) -> StatusHandler {
    StatusHandler {
        update: None,
        node: node.to_string(),
        mode: "kernel",
        applied_seq: AppliedSeq::default(),
        started: Instant::now(),
        socket: PathBuf::from("/nonexistent/incus.sock"),
        metrics_source: Arc::new(Mutex::new(HostModeChoice {
            mode: HostMode::Local,
            reason: "the test names the local mode".to_string(),
            provisional: false,
        })),
        approval: None,
        loopback: None,
        page: None,
    }
}

/// A relayed answer keeps every field the node wrote, its own `node` id among them, and names the linked node in a field
/// no result has: it fits the tool's advertised `outputSchema` and the verb's own result schema, so an SDK client that
/// checks structured content against the schema accepts it.
#[test]
fn a_relayed_answer_fits_its_tools_output_schema_and_keeps_the_nodes_own_fields() {
    let fleet = Fleet::new("mh-node-schema");
    let (status, listed) = fleet.rpc(&fleet.agent, "tools/list", json!({}));
    assert_eq!(status, 200, "{listed}");
    let tools = listed["result"]["tools"].as_array().expect("tools");
    for verb in ["arca_status", "watch_report"] {
        let advertised = tools
            .iter()
            .find(|tool| tool["name"] == json!(verb))
            .map(|tool| tool["outputSchema"].clone())
            .expect("the tool is listed");
        let (status, reply) = fleet.call(&fleet.agent, verb, json!({ "node": "beta" }));
        assert_eq!(status, 200, "{reply}");
        let structured = &reply["result"]["structuredContent"];
        assert_eq!(structured["linkedNode"], json!("beta"), "{reply}");
        assert_eq!(
            arca_api::conforms(&advertised, structured),
            Ok(()),
            "{verb}: the relayed answer fits the advertised outputSchema"
        );
        let own = (arca_api::verb(verb).expect("the verb").result)();
        assert_eq!(
            arca_api::conforms(&own, structured),
            Ok(()),
            "{verb}: the relayed answer fits the verb's result schema"
        );
    }
    let (_, reply) = fleet.call(&fleet.agent, "arca_status", json!({ "node": "beta" }));
    let structured = &reply["result"]["structuredContent"];
    assert_eq!(
        structured["node"],
        json!("b".repeat(64)),
        "beta's own node id"
    );
    assert!(structured.get("nodeId").is_none());
}

/// A refusal of the linked node's own comes back as it wrote it, its code and words, with the node named in `data`.
#[test]
fn a_refusal_of_the_linked_node_is_passed_on_with_the_node_named() {
    let fleet = Fleet::new("mh-node-refusal");
    // Eight proposals may wait on a kernel; beta refuses a ninth with its own code.
    for n in 1..=8 {
        let (status, reply) = fleet.propose_named(&fleet.agent, Some("beta"), &format!("nats{n}"));
        assert_eq!(status, 200, "{reply}");
        assert!(reply.get("result").is_some(), "{reply}");
    }
    let (status, reply) = fleet.propose_named(&fleet.agent, Some("beta"), "nats9");
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["error"]["code"], json!(-32000), "{reply}");
    assert_eq!(reply["error"]["data"]["refusal"], json!("TooManyProposals"));
    assert_eq!(reply["error"]["data"]["linkedNode"], json!("beta"));
    assert_eq!(
        fleet
            .pair
            .beta
            .lock()
            .expect("the kernel lock")
            .action_proposals()
            .len(),
        8
    );
    assert!(
        fleet
            .pair
            .alpha
            .lock()
            .expect("the kernel lock")
            .action_proposals()
            .is_empty(),
        "nothing waits on alpha"
    );
}

/// A linked node that refuses the token for the verb (the page's token admits three verbs) is refused `LinkToken`,
/// naming it: never this kernel's own answer.
#[test]
fn a_linked_node_that_refuses_the_token_is_refused_link_token_naming_it() {
    let fleet = Fleet::new("mh-node-token");
    let (status, reply) = fleet.call(&fleet.agent, "watch_report", json!({ "node": "beta-page" }));
    assert_eq!(status, 200, "{reply}");
    assert!(reply.get("result").is_none(), "{reply}");
    assert_eq!(reply["error"]["code"], json!(-32000));
    assert_eq!(reply["error"]["data"]["refusal"], json!("LinkToken"));
    assert_eq!(reply["error"]["data"]["linkedNode"], json!("beta-page"));
    let message = reply["error"]["message"].as_str().expect("a message");
    assert!(
        message.contains("beta-page") && message.contains("watch_report"),
        "{message}"
    );
}

/// No write takes `node`: a write that names one is refused at this kernel's argument check, and nothing is relayed.
#[test]
fn a_write_that_names_a_node_is_refused_here_and_relays_nothing() {
    let fleet = Fleet::new("mh-node-write");
    let id = Pair::propose(&fleet.pair.beta);
    let (status, reply) = fleet.call(
        &fleet.doctor,
        "plan_decline",
        json!({ "plan": id, "node": "beta" }),
    );
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["result"]["isError"], json!(true), "{reply}");
    let text = reply["result"]["content"][0]["text"]
        .as_str()
        .expect("a text");
    assert!(text.starts_with("invalid_arguments: "), "{text}");
    assert!(text.contains("node"), "{text}");
    assert!(
        Pair::waiting(&fleet.pair.beta, &id),
        "beta's plan still waits"
    );
}

/// The 2026-11-15 sitting: the owner approves one card, then another whose statement the page listed more than ten minutes
/// earlier. Beta refuses that one as `Expired`, and the same card read again approves at once.
#[test]
fn a_second_card_signed_over_a_stale_statement_is_expired_and_approves_after_a_relist() {
    let (pair, owner) = Pair::new("mh-two-cards", BetaTrust::AlphaFile);
    let first = Pair::propose_named(&pair.beta, "nats");
    let second = Pair::propose_named(&pair.beta, "redis");
    let first_statement = pair.statement_of(&first);
    let stale = pair.statement_of(&second);

    let (status, answer) = pair.request(
        "POST",
        "/approve/approve",
        Some(json!({ "plan": first, "approval": approval(&owner, &first_statement), "host": "beta" })),
    );
    assert_eq!(status, 200, "{answer}");

    // Eleven minutes on: the proposal (fifteen) still waits, the statement (ten) does not.
    pair.clock.store(NOW + 11 * 60_000, Ordering::SeqCst);
    let (status, answer) = pair.request(
        "POST",
        "/approve/approve",
        Some(json!({ "plan": second, "approval": approval(&owner, &stale), "host": "beta" })),
    );
    assert_eq!(status, 403, "{answer}");
    assert_eq!(answer["refusal"], json!("Expired"));
    assert!(Pair::waiting(&pair.beta, &second), "nothing was spent");

    let fresh = pair.statement_of(&second);
    assert_ne!(fresh, stale, "the re-list mints a fresh statement");
    let (status, answer) = pair.request(
        "POST",
        "/approve/approve",
        Some(json!({ "plan": second, "approval": approval(&owner, &fresh), "host": "beta" })),
    );
    assert_eq!(status, 200, "{answer}");
    assert_eq!(pair.beta_runs.load(Ordering::SeqCst), 2, "both cards ran");
    assert!(!Pair::waiting(&pair.beta, &second));

    // One line per approval, with its outcome code alone: never the statement, the nonce or the proof.
    let lines: Vec<String> = pair
        .log
        .lock()
        .expect("the log")
        .iter()
        .filter(|line| line.starts_with("arca: approval of "))
        .cloned()
        .collect();
    assert_eq!(
        lines,
        vec![
            format!("arca: approval of {first} on beta: approved"),
            format!("arca: approval of {second} on beta: refused Expired"),
            format!("arca: approval of {second} on beta: approved"),
        ]
    );
    let nonce = serde_json::from_str::<Value>(&fresh).expect("the statement")["nonce"]
        .as_str()
        .expect("a nonce")
        .to_string();
    for line in pair.log.lock().expect("the log").iter() {
        assert!(
            !line.contains(&nonce) && !line.contains("webauthn"),
            "{line}"
        );
    }
}

/// A linked kernel's delete plan is listed with its host and a statement for that host, and the owner's approval is
/// relayed to its `approval_submit`, which deletes there once.
#[test]
fn a_linked_delete_plan_is_listed_and_approved_through_the_relay() {
    let mut tools = ACTION_TOOLS.to_vec();
    tools.push("approval_submit");
    let (pair, owner) = Pair::with_tools("mh-delete", BetaTrust::AlphaFile, &tools);
    let plan = pair.beta_delete_plan();
    let row = pair
        .delete_row(&plan)
        .expect("beta's delete plan, labelled");
    assert!(
        row.get("relayNote").is_none(),
        "the token admits approval_submit: {row}"
    );
    let statement = row["statement"].as_str().expect("a statement");
    let parsed: Value = serde_json::from_str(statement).expect("the statement");
    assert_eq!(
        parsed["host"],
        json!(BETA_HOST),
        "the statement names the target's host"
    );
    assert_eq!(parsed["action"], json!("delete"));

    let (status, answer) = pair.request(
        "POST",
        "/approve/approve",
        Some(json!({ "plan": plan, "approval": approval(&owner, statement), "host": "beta" })),
    );
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["plan"], json!(plan));
    assert_eq!(answer["host"], json!("beta"));
    assert_eq!(
        deletes_for(&pair.beta_fake, "toy-box-t1"),
        1,
        "beta deleted once"
    );
    assert!(pair.delete_row(&plan).is_none(), "the plan is gone");
    assert!(pair.alpha.lock().expect("alpha").pending_plans().is_empty());
}

/// With the action-only relay token, a linked delete plan is still listed, and its card says what the owner changes on the
/// linked host; an approval sent anyway is refused by that host's door as the token, with the same words.
#[test]
fn a_linked_delete_plan_says_what_to_change_when_the_token_does_not_admit_approval_submit() {
    let (pair, owner) = Pair::new("mh-delete-token", BetaTrust::AlphaFile);
    let plan = pair.beta_delete_plan();
    let row = pair
        .delete_row(&plan)
        .expect("beta's delete plan, labelled");
    let note = row["relayNote"].as_str().expect("a note on the card");
    assert!(
        note.contains("approval_submit") && note.contains("beta") && note.contains("list of tools"),
        "{note}"
    );
    let statement = row["statement"].as_str().expect("a statement");
    let (status, answer) = pair.request(
        "POST",
        "/approve/approve",
        Some(json!({ "plan": plan, "approval": approval(&owner, statement), "host": "beta" })),
    );
    assert_eq!(status, 502, "{answer}");
    assert_eq!(answer["refusal"], json!("LinkToken"));
    assert_eq!(answer["message"], json!(note));
    assert_eq!(
        deletes_for(&pair.beta_fake, "toy-box-t1"),
        0,
        "nothing deleted"
    );
}
