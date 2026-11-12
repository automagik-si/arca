//! Multi-host approvals on one machine: the page host (alpha) lists a linked kernel's (beta's) waiting proposals and relays
//! the owner's signed approval to beta's `action_approve` over beta's own socket. Beta verifies it with its own verifier,
//! trusting alpha's passkeys file read-only. The page host never approves for another host.
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

use arca::approve_page::{Page, SetupCodeDisplay};
use arca::config::LinkedHost;
use arca::handlers::{
    ActionExecutor, ActionSlot, ApprovalStatus, Clock, StatusHandler, action_statement,
    plan_handlers_with,
};
use arca::kernel::{AppliedSeq, Kernel};
use arca_api::Scope;
use arca_approval::{
    ApprovalRefusal, ApprovalVerifier, FileNonces, FileSignCounts, ForeignPasskeys, OwnerKeys,
    PasskeyVerifier, STATE_PASSKEYS_FILE, SshSigVerifier, parse_passkeys,
};
use arca_kernel::canon::{Digest, canonical_json, params_digest};
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

/// How beta trusts alpha's passkeys.
enum BetaTrust {
    /// `approval.trustOwnerKeysFrom` names alpha's passkeys file, for alpha's origin.
    AlphaFile,
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
    _beta_fake: arca_incus::FakeIncus,
    alpha: Arc<Mutex<Kernel>>,
    beta: Arc<Mutex<Kernel>>,
    alpha_socket: PathBuf,
    alpha_runs: Arc<AtomicU64>,
    beta_runs: Arc<AtomicU64>,
    token_file: PathBuf,
    foreign: ForeignPasskeys,
    /// Beta's state directory, where its state-file passkeys live.
    beta_state: PathBuf,
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

impl Pair {
    fn new(tag: &str, trust: BetaTrust) -> (Pair, SoftPasskey) {
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
            BetaTrust::None => ForeignPasskeys::default(),
            BetaTrust::Unreadable => {
                fs::set_permissions(&alpha_passkeys, fs::Permissions::from_mode(0o000))
                    .expect("mode 0000");
                ForeignPasskeys::new(vec![alpha_passkeys.clone()], vec![ORIGIN.to_string()])
            }
        };
        let beta_lines = parse_passkeys(format!("{}\n", beta_own.line(NAMESPACE)).as_bytes())
            .expect("beta's own line parses");
        let beta_verifier = ApprovalVerifier::new(
            BETA_HOST.to_string(),
            OwnerKeys::new(
                SshSigVerifier::new(Vec::new()),
                PasskeyVerifier::new(
                    beta_lines,
                    FileSignCounts::open(&beta_scratch.path("beta-counts.jsonl")).expect("counts"),
                )
                .with_foreign(foreign.clone()),
            ),
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

        // Beta's MCP door, with a doctor token limited to the three relayed verbs.
        let tokens_dir = beta_scratch.path("tokens");
        private_dir(&tokens_dir);
        let tools = [
            "plan_list".to_string(),
            "action_approve".to_string(),
            "plan_decline".to_string(),
        ];
        let (secret, hash) = mint_with_tools(Scope::Doctor, Some(&tools));
        write_mode(&tokens_dir.join("relay.hash"), &hash, 0o400);
        let token_file = alpha_scratch.path("beta-relay.secret");
        write_mode(&token_file, &format!("{secret}\n"), 0o600);
        let tokens = TokenStore::load(&tokens_dir).expect("the token store loads");
        let beta_runs = Arc::new(AtomicU64::new(0));
        let beta_clock = Arc::clone(&clock);
        let beta_clock: Clock = Arc::new(move || beta_clock.load(Ordering::SeqCst));
        let slot = ActionSlot::default();
        let mut beta_server = Server::new(arca_api::verbs(), tokens);
        for (verb, handler) in
            plan_handlers_with(Arc::clone(&beta), beta_clock, None, None, Arc::clone(&slot))
        {
            beta_server = beta_server.handle(verb, handler);
        }
        let _ = slot.set(executor(&beta_runs));
        let beta_socket = beta_scratch.path("beta.sock");
        let beta_state = beta_scratch.path("state");
        private_dir(&beta_state);

        // Alpha's page, linked to beta.
        let alpha_state = alpha_scratch.path("state");
        private_dir(&alpha_state);
        let alpha_runs = Arc::new(AtomicU64::new(0));
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
        .with_linked_hosts(vec![LinkedHost {
            name: "beta".to_string(),
            socket: beta_socket.clone(),
            token_file: token_file.clone(),
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
            _beta_fake: beta_fake,
            alpha,
            beta,
            alpha_socket,
            alpha_runs,
            beta_runs,
            token_file,
            foreign,
            beta_state,
            _runtime: runtime,
        };
        (pair, owner)
    }

    fn propose(kernel: &Arc<Mutex<Kernel>>) -> String {
        kernel
            .lock()
            .expect("the kernel lock")
            .propose_action(
                arca::action_proposal::ActionKind::ServiceCreate,
                json!({ "name": "nats", "template": "nats" }),
                NOW,
                None,
            )
            .expect("a proposal")
            .id
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
