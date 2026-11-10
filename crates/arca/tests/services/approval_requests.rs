//! Group 7's owner approval request titles (droidd's approval-requests suite), and Arca's own test from the dropped
//! `owner_approval_deliver` title: the request store, its three verbs, and the page's begin, approve and deny, over the
//! service rig with a software passkey enrolled, so an approved request's approval is spent by the real `service_remove`.
//! Each test is named for its title with a `g7_` prefix. The passkey is a software P-256 key from a fixed seed; no test
//! prints an approval string, a proof or a credential.
use super::*;
use arca::approval_requests::{
    self as requests, APPROVED_AUDIT, AUDIT_FILE, ApprovalRequests, BEGUN_MAX_MS, DENIED_AUDIT,
    PAGE_PRINCIPAL, PENDING_PER_KERNEL, PENDING_PER_TOKEN, PageRefusal, REQUEST_TTL_MS,
    REQUESTED_AUDIT, REQUESTS_FILE,
};
use arca::handlers::{Clock, iso_utc};
use arca_approval::{ARCA_NAMESPACE, ArcaAction, challenge_for, iso_ms, parse_statement};
use arca_kernel::canon::{canonical_json, params_digest};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use std::time::{Duration, Instant};
use webauthn::{Assert, ORIGIN, RP, SoftPasskey, passkey_approval};

const MINUTE: u64 = 60_000;
const HOUR: u64 = 60 * MINUTE;

/// The service rig with one passkey enrolled, the request store in its state directory, the three verbs on a clock the
/// test moves, and the store's log lines captured.
struct G7 {
    rig: Rig,
    _owner: common::Scratch,
    key: SoftPasskey,
    requests: Arc<ApprovalRequests>,
    verbs: HashMap<&'static str, Arc<dyn ToolHandler>>,
    clock: Arc<AtomicU64>,
    logs: Arc<Mutex<Vec<String>>>,
}

fn empty_state() -> FakeState {
    FakeState {
        server: json!({}),
        projects: vec![],
        instances: vec![],
        networks: vec![],
        pools: vec![],
    }
}

fn caller(token: &str, scope: Scope) -> Caller {
    Caller {
        token_id: token.to_string(),
        scope,
        tools: None,
    }
}

/// A kind `arca` request to delete `target`, with the arguments `service_remove` takes.
fn delete(target: &str) -> Value {
    json!({ "kind": "arca", "action": "delete", "target": target, "args": { "name": target } })
}

fn id_of(created: &Value) -> String {
    created["id"].as_str().expect("an id").to_string()
}

fn text(value: &Value, key: &str) -> String {
    value[key].as_str().expect("a string").to_string()
}

impl G7 {
    fn new() -> Self {
        Self::with_lines(|key| format!("{}\n", key.line(ARCA_NAMESPACE)))
    }

    fn with_lines(lines: impl FnOnce(&SoftPasskey) -> String) -> Self {
        let key = SoftPasskey::from_seed(1);
        let owner = common::Scratch::new("g7-owner");
        let verifier = common::verifier_with_passkeys(&owner, &[], &lines(&key), "g7");
        let rig = Rig::with_verifier(
            empty_state(),
            vec![PROJECT.to_string()],
            Some("192.0.2.0/24"),
            Some(verifier),
            None,
        );
        let logs = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&logs);
        let requests = Arc::new(
            ApprovalRequests::open(&rig.dir, Some(format!("{ORIGIN}/approve")), NOW)
                .expect("the requests open")
                .with_log(Arc::new(move |line: &str| {
                    captured.lock().expect("logs").push(line.to_string())
                })),
        );
        let clock = Arc::new(AtomicU64::new(NOW));
        let read = Arc::clone(&clock);
        let clock_fn: Clock = Arc::new(move || read.load(Ordering::SeqCst));
        let verbs = requests::handlers(Arc::clone(&rig.kernel), clock_fn, Arc::clone(&requests))
            .into_iter()
            .collect();
        Self {
            rig,
            _owner: owner,
            key,
            requests,
            verbs,
            clock,
            logs,
        }
    }

    fn now(&self) -> u64 {
        self.clock.load(Ordering::SeqCst)
    }

    fn at(&self, ms: u64) {
        self.clock.store(ms, Ordering::SeqCst);
    }

    fn ask(&self, token: &str, scope: Scope, args: Value) -> Result<Value, CallError> {
        self.verbs["owner_approval_request"].call(args, &caller(token, scope))
    }

    fn asked(&self, args: Value) -> String {
        id_of(
            &self
                .ask("doctor", Scope::Doctor, args)
                .expect("the request is made"),
        )
    }

    fn status(&self, token: &str, scope: Scope, id: &str) -> Result<Value, CallError> {
        self.verbs["owner_approval_status"].call(json!({ "id": id }), &caller(token, scope))
    }

    fn mine(&self, id: &str) -> Value {
        self.status("doctor", Scope::Doctor, id)
            .expect("the requester reads its status")
    }

    fn list(&self) -> Vec<Value> {
        self.verbs["owner_approval_list"]
            .call(json!({}), &caller("phone", Scope::Viewer))
            .expect("a viewer lists")["requests"]
            .as_array()
            .expect("an array")
            .clone()
    }

    fn begin(&self, id: &str) -> Result<Value, PageRefusal> {
        let kernel = self.rig.kernel.lock().expect("kernel");
        self.requests.begin(&kernel, id, RP, self.now())
    }

    fn statement(&self, id: &str) -> String {
        text(
            &self.begin(id).expect("begin mints a statement"),
            "statement",
        )
    }

    fn approve(&self, id: &str, statement: &str, proof: &Value) -> Result<Value, PageRefusal> {
        let kernel = self.rig.kernel.lock().expect("kernel");
        self.requests
            .approve(&kernel, id, statement, proof, self.now())
    }

    fn deny(&self, id: &str) -> Result<Value, PageRefusal> {
        let kernel = self.rig.kernel.lock().expect("kernel");
        self.requests.deny(&kernel, id, self.now())
    }

    /// The passkey's answer over `statement`, bent by `bend`.
    fn proof(&self, statement: &str, bend: impl FnOnce(&mut Assert)) -> Value {
        let mut a = Assert::over(ARCA_NAMESPACE, statement.as_bytes());
        bend(&mut a);
        self.key.proof_value(&a)
    }

    /// One tap on the page: begin, the passkey over the minted statement, approve.
    fn tap(&self, id: &str) -> Value {
        let statement = self.statement(id);
        self.approve(id, &statement, &self.proof(&statement, |_| {}))
            .expect("the tap approves")
    }

    /// The approval the requester reads once the owner tapped.
    fn approval(&self, id: &str) -> String {
        text(&self.mine(id), "approval")
    }

    fn audit(&self) -> String {
        fs::read_to_string(self.rig.dir.join(AUDIT_FILE)).unwrap_or_default()
    }

    fn logs(&self) -> String {
        self.logs.lock().expect("logs").join("\n")
    }

    fn stored(&self) -> String {
        fs::read_to_string(self.rig.dir.join(REQUESTS_FILE)).unwrap_or_default()
    }

    /// The stopped service `pg`, which `service_remove` removes with an owner approval.
    fn service(&self) {
        self.rig.template("pg", stopped_service_template());
        self.rig
            .call("service_create", json!({ "name": "pg", "template": "pg" }))
            .expect("the service is created");
    }

    fn remove(&self, approval: &str) -> Result<Value, CallError> {
        self.rig.call(
            "service_remove",
            json!({ "name": "pg", "approval": approval }),
        )
    }

    fn spend(&self, target: &str, approval: &str) -> Result<(), arca_approval::ApprovalRefusal> {
        self.rig.kernel.lock().expect("kernel").check_approval(
            ArcaAction::Delete,
            target,
            &json!({ "name": target }),
            approval,
            NOW,
        )
    }
}

fn refusal(result: Result<Value, PageRefusal>) -> PageRefusal {
    result.expect_err("a page refusal")
}

fn invalid(result: Result<Value, CallError>) -> String {
    match result {
        Err(CallError::Invalid(why)) => why,
        other => panic!("an invalid call was expected, got {other:?}"),
    }
}

#[test]
fn g7_owner_approval_request_callers_pending_limits_and_dedupe() {
    let g = G7::new();
    assert!(
        matches!(
            g.ask("phone", Scope::Viewer, delete("pg")),
            Err(CallError::Forbidden { .. })
        ),
        "never a viewer's"
    );
    let mut named = delete("pg");
    named["droidd"] = json!("box2");
    assert!(
        invalid(g.ask("doctor", Scope::Doctor, named)).contains("names no droidd"),
        "no other droidd: Arca's statements name none"
    );
    let first = g
        .ask("doctor", Scope::Doctor, delete("pg"))
        .expect("a doctor asks");
    assert!(requests::valid_id(&id_of(&first)));
    assert_eq!(first["state"], "pending");
    assert_eq!(first["dashboard"], json!(format!("{ORIGIN}/approve")));
    let again = g
        .ask("doctor", Scope::Doctor, delete("pg"))
        .expect("asked again");
    assert_eq!(
        again["id"], first["id"],
        "an identical pending request is the same one"
    );
    let operator = g
        .ask("operator", Scope::Operator, delete("pg"))
        .expect("an operator asks");
    assert_ne!(operator["id"], first["id"], "the same request is per token");
    for n in 1..PENDING_PER_TOKEN {
        g.ask("doctor", Scope::Doctor, delete(&format!("svc{n}")))
            .expect("within the token's limit");
    }
    match g.ask("doctor", Scope::Doctor, delete("svc9")) {
        Err(CallError::Refused { code, message }) => {
            assert_eq!(code, "refused");
            assert!(
                message.contains("429") && message.contains("doctor has 5"),
                "{message}"
            );
        }
        other => panic!("the token's sixth pending request is refused, got {other:?}"),
    }
    // doctor 5 and operator 1 are pending; 14 more reach the kernel's 20.
    for n in 0..(PENDING_PER_KERNEL - PENDING_PER_TOKEN - 1) {
        g.ask(
            &format!("k{}", n / 5),
            Scope::Doctor,
            delete(&format!("t{n}")),
        )
        .expect("within the kernel's limit");
    }
    match g.ask("late", Scope::Doctor, delete("late")) {
        Err(CallError::Refused { code, message }) => {
            assert_eq!(code, "refused");
            assert!(
                message.contains("429") && message.contains("Arca has 20"),
                "{message}"
            );
        }
        other => panic!("the kernel's 21st pending request is refused, got {other:?}"),
    }
}

#[test]
fn g7_owner_approval_request_checks_its_fields_as_the_statement_will_carry_them() {
    let g = G7::new();
    let deep = (0..10).fold(json!("leaf"), |inner, _| json!({ "in": inner }));
    let cases: Vec<(Value, &str)> = vec![
        (
            json!({ "kind": "arca", "target": "pg", "args": {} }),
            "an arca request needs action",
        ),
        (
            json!({ "kind": "arca" }),
            "an arca request needs action, target, args",
        ),
        (
            json!({ "action": "delete", "target": "pg", "args": {} }),
            "kind must be arca or upgrade",
        ),
        (
            json!({ "kind": "arca", "action": "install", "target": "pg", "args": {} }),
            "action must be delete, publish or spend",
        ),
        (
            json!({ "kind": "arca", "action": "delete", "target": "-pg", "args": {} }),
            "target must match",
        ),
        (
            json!({ "kind": "arca", "action": "delete", "target": "p".repeat(129), "args": {} }),
            "target must match",
        ),
        (
            json!({ "kind": "arca", "action": "delete", "target": "pg", "args": [1] }),
            "args must be a JSON object",
        ),
        (
            json!({ "kind": "arca", "action": "delete", "target": "pg", "args": { "approval": "x" } }),
            "without approval",
        ),
        (
            json!({ "kind": "arca", "action": "delete", "target": "pg", "args": {}, "note": 1 }),
            "an arca request takes no note",
        ),
        (
            json!({ "kind": "arca", "action": "delete", "target": "pg", "args": {}, "step": "eyes" }),
            "an arca request takes no step",
        ),
        (
            json!({ "kind": "arca", "action": "delete", "target": "pg", "args": { "name": "p\u{202e}g" } }),
            "args.name holds a control or bidi character",
        ),
        (
            json!({ "kind": "arca", "action": "delete", "target": "pg", "args": { "name": "x".repeat(5000) } }),
            "more than 4096 bytes",
        ),
        (
            json!({ "kind": "arca", "action": "delete", "target": "pg", "args": { "deep": deep } }),
            "nested deeper than 8 levels",
        ),
    ];
    for (args, want) in cases {
        let why = invalid(g.ask("doctor", Scope::Doctor, args.clone()));
        assert!(why.contains(want), "{args}: {why}");
    }
    match g.ask(
        "doctor",
        Scope::Doctor,
        json!({ "kind": "upgrade", "droid": "phone1", "step": "eyes" }),
    ) {
        Err(CallError::Refused { code, message }) => {
            assert_eq!(code, "upgrade_not_here");
            assert!(message.contains("droidd's MCP"), "{message}");
        }
        other => panic!("kind upgrade is refused, got {other:?}"),
    }
    assert!(g.list().is_empty(), "no refused request was kept");
    let id = g.asked(delete("pg"));
    let listed = g.list();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["id"], json!(id));
    assert_eq!(listed[0]["args"], json!({ "name": "pg" }));
    assert_eq!(
        listed[0]["fields"],
        json!({
            "action": "delete",
            "target": "pg",
            "paramsDigest": params_digest(&json!({ "name": "pg" })).expect("digest"),
        }),
        "the fields its statement will carry"
    );
}

#[test]
fn g7_owner_approval_request_refuses_a_host_no_statement_can_name() {
    let scratch = common::Scratch::new("g7-host");
    let fake = common::start_fake(&scratch);
    let verifier = common::verifier_for(&scratch, &[], "g7-host");
    let kernel = common::kernel_on_host(&scratch, &fake, verifier, NOW, None, "box 1")
        .expect("the kernel opens");
    let store = ApprovalRequests::open(&scratch.dir, None, NOW).expect("the requests open");
    match store.request(
        &kernel,
        &delete("pg"),
        &caller("doctor", Scope::Doctor),
        NOW,
    ) {
        Err(CallError::Invalid(why)) => {
            assert!(why.contains("make no statement Arca spends"), "{why}");
        }
        other => panic!("a host no statement can name is refused, got {other:?}"),
    }
    assert!(store.list(&kernel, NOW).expect("the list reads").is_empty());
}

#[test]
fn g7_the_approval_page_mints_the_requests_statement() {
    let g = G7::with_lines(|key| {
        let mut other = SoftPasskey::from_seed(2);
        other.rp_id = "box2.example.com".to_string();
        other.origin = "https://box2.example.com".to_string();
        format!(
            "{}\n{}\n",
            key.line(ARCA_NAMESPACE),
            other.line(ARCA_NAMESPACE)
        )
    });
    let id = g.asked(delete("pg"));
    g.at(NOW + 1_000);
    let begun = g.begin(&id).expect("begin mints");
    let statement = text(&begun, "statement");
    let parsed = parse_statement(statement.as_bytes()).expect("the spend's parser reads it");
    assert_eq!(parsed.host, "node1", "this Arca");
    assert_eq!(parsed.action, ArcaAction::Delete);
    assert_eq!(parsed.target, "pg");
    assert_eq!(
        parsed.params_digest,
        params_digest(&json!({ "name": "pg" })).expect("digest")
    );
    let ms = |text: &str| iso_ms(text).and_then(|ms| u64::try_from(ms).ok());
    assert_eq!(ms(&parsed.issued_at), Some(NOW + 1_000), "issued now");
    assert_eq!(
        ms(&parsed.expires_at),
        Some(NOW + 1_000 + 15 * MINUTE),
        "valid 15 min"
    );
    assert_eq!(begun["namespace"], ARCA_NAMESPACE);
    assert_eq!(
        text(&begun, "challenge"),
        URL_SAFE_NO_PAD
            .encode(challenge_for(ARCA_NAMESPACE, statement.as_bytes()).expect("challenge"))
    );
    assert_eq!(begun["rpId"], RP);
    assert_eq!(
        begun["allowCredentials"],
        json!([{ "id": URL_SAFE_NO_PAD.encode(&g.key.credential_id) }]),
        "only the lines for this relying party"
    );
    assert_eq!(begun["timeoutMs"], json!(BEGUN_MAX_MS));
    let again = parse_statement(g.statement(&id).as_bytes()).expect("parses");
    assert_ne!(again.nonce, parsed.nonce, "a fresh nonce each begin");
    let kernel = g.rig.kernel.lock().expect("kernel");
    let moved = refusal(
        g.requests
            .begin(&kernel, &id, "box3.example.com", NOW + 2_000),
    );
    assert_eq!((moved.status, moved.code), (409, "passkey_rp_changed"));
    assert_eq!(
        moved.extra["passkeyRpIds"],
        json!(["box1.example.com", "box2.example.com"])
    );
    assert_eq!(moved.extra["rpId"], "box3.example.com");
}

#[test]
fn g7_finish_within_120_s_approves_and_the_approval_spends_once_through_service_remove() {
    let g = G7::new();
    g.service();
    let id = g.asked(delete("pg"));
    let replaced = g.statement(&id);
    let current = g.statement(&id);
    let stale = refusal(g.approve(&id, &replaced, &g.proof(&replaced, |_| {})));
    assert_eq!(
        (stale.status, stale.code),
        (409, "stale"),
        "a replaced begin"
    );
    let other = g.asked(delete("svc2"));
    let others = g.statement(&other);
    let crossed = refusal(g.approve(&id, &others, &g.proof(&others, |_| {})));
    assert_eq!(
        (crossed.status, crossed.code),
        (403, "owner_approval"),
        "another request's"
    );
    g.at(NOW + BEGUN_MAX_MS + 1);
    let late = refusal(g.approve(&id, &current, &g.proof(&current, |_| {})));
    assert_eq!(
        (late.status, late.code),
        (409, "begin_expired"),
        "a late finish"
    );
    assert_eq!(
        g.mine(&id)["state"],
        "pending",
        "every refusal leaves it pending"
    );

    // The rig's service_remove spends at NOW, so the statement is begun at NOW and finished 120 s later.
    g.at(NOW);
    let statement = g.statement(&id);
    g.at(NOW + BEGUN_MAX_MS);
    let approved = g
        .approve(&id, &statement, &g.proof(&statement, |_| {}))
        .expect("a finish within 120 s approves");
    assert_eq!(approved["state"], "approved");
    assert!(
        approved.get("approval").is_none(),
        "the page never gets the approval string"
    );
    let approval = g.approval(&id);
    assert_eq!(
        g.remove(&approval).expect("service_remove spends it"),
        json!({ "removed": true })
    );
    assert_eq!(
        g.spend("pg", &approval),
        Err(arca_approval::ApprovalRefusal::Replayed),
        "once"
    );
    assert!(g.remove(&approval).is_err(), "no second removal");
    let after = g.mine(&id);
    assert_eq!(after["state"], "approved");
    assert!(
        after.get("approval").is_none(),
        "a spent approval is no longer handed out"
    );
}

#[test]
fn g7_owner_approval_status_gives_the_approval_to_the_requester_only_and_wait_ms_wakes_on_the_decision()
 {
    let g = G7::new();
    let id = g.asked(delete("pg"));
    assert!(matches!(
        g.status("phone", Scope::Viewer, &id),
        Err(CallError::Forbidden { .. })
    ));
    let status = Arc::clone(&g.verbs["owner_approval_status"]);
    let waited = id.clone();
    let started = Instant::now();
    let waiter = std::thread::spawn(move || {
        status.call(
            json!({ "id": waited, "waitMs": 20_000 }),
            &caller("doctor", Scope::Doctor),
        )
    });
    std::thread::sleep(Duration::from_millis(200));
    g.tap(&id);
    let woke = waiter
        .join()
        .expect("the waiter")
        .expect("the status answers");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "woken by the decision"
    );
    assert_eq!(woke["state"], "approved");
    assert!(
        woke["approval"].is_string(),
        "the requester gets the approval"
    );
    let other = g
        .status("other", Scope::Doctor, &id)
        .expect("another token reads the state");
    assert_eq!(other["state"], "approved");
    assert!(other.get("approval").is_none(), "never another token's");
    let operator = g
        .status("operator", Scope::Operator, &id)
        .expect("an operator reads the state");
    assert!(operator.get("approval").is_none(), "nor an operator's");
    assert!(
        invalid(g.verbs["owner_approval_status"].call(
            json!({ "id": id, "waitMs": 30_001 }),
            &caller("doctor", Scope::Doctor)
        ))
        .contains("waitMs")
    );
}

#[test]
fn g7_approval_request_and_approval_expiry_and_the_approval_column_cleared() {
    let g = G7::new();
    g.service();
    let spent = g.asked(delete("pg"));
    g.tap(&spent);
    let approval = g.approval(&spent);
    assert!(g.stored().contains(&approval), "kept for the requester");
    g.remove(&approval).expect("spent");
    assert_eq!(g.mine(&spent)["state"], "approved");
    assert!(!g.stored().contains(&approval), "cleared after the spend");

    let unspent = g.asked(delete("svc2"));
    g.tap(&unspent);
    let kept = g.approval(&unspent);
    let pending = g.asked(delete("svc3"));
    g.at(NOW + 15 * MINUTE - 1);
    assert_eq!(g.mine(&unspent)["state"], "approved");
    g.at(NOW + 15 * MINUTE);
    let expired = g.mine(&unspent);
    assert_eq!(
        expired["state"], "expired",
        "an approved unspent one after its statement"
    );
    assert!(expired.get("approval").is_none());
    assert!(!g.stored().contains(&kept), "cleared after its expiry");
    assert_eq!(g.mine(&pending)["state"], "pending");
    g.at(NOW + REQUEST_TTL_MS - 1);
    assert_eq!(g.mine(&pending)["state"], "pending");
    g.at(NOW + REQUEST_TTL_MS);
    assert_eq!(
        g.mine(&pending)["state"],
        "expired",
        "a pending one after 60 min"
    );
    assert_eq!(refusal(g.begin(&pending)).code, "not_pending");
}

#[test]
fn g7_deny_is_audited_and_final() {
    let g = G7::new();
    let id = g.asked(delete("pg"));
    let statement = g.statement(&id);
    assert_eq!(g.deny(&id).expect("denied"), json!({ "state": "denied" }));
    let audit = g.audit();
    let line = audit
        .lines()
        .find(|line| line.contains(DENIED_AUDIT))
        .expect("an audit row");
    let row: Value = serde_json::from_str(line).expect("JSON");
    assert_eq!(row["principal"], PAGE_PRINCIPAL);
    assert!(text(&row, "outcome").starts_with(&format!("denied:{id}:arca:delete:pg:")));
    assert_eq!(g.mine(&id)["state"], "denied");
    for refused in [
        g.deny(&id),
        g.begin(&id),
        g.approve(&id, &statement, &g.proof(&statement, |_| {})),
    ] {
        let refused = refusal(refused);
        assert_eq!((refused.status, refused.code), (409, "not_pending"));
    }
    assert_eq!(audit.matches(DENIED_AUDIT).count(), 1);
    assert_eq!(
        g.audit().matches(DENIED_AUDIT).count(),
        1,
        "final: no second row"
    );
    assert!(!g.audit().contains(APPROVED_AUDIT));
}

#[test]
fn g7_a_row_changed_between_the_check_and_the_write_is_refused_stale_and_nothing_is_decided() {
    let g = G7::new();
    let id = g.asked(delete("pg"));
    let statement = g.statement(&id);
    let proof = g.proof(&statement, |_| {});
    let store = Arc::downgrade(&g.requests);
    g.requests.set_decide_hook(Some(Box::new(move |kernel, id| {
        if let Some(store) = store.upgrade() {
            store
                .begin(kernel, id, RP, NOW)
                .expect("the row is begun again");
        }
    })));
    let approved = refusal(g.approve(&id, &statement, &proof));
    assert_eq!((approved.status, approved.code), (409, "stale"));
    let denied = refusal(g.deny(&id));
    assert_eq!((denied.status, denied.code), (409, "stale"));
    g.requests.set_decide_hook(None);
    let status = g.mine(&id);
    assert_eq!(status["state"], "pending", "nothing approved or denied");
    assert!(status.get("approval").is_none());
    let audit = g.audit();
    assert!(
        !audit.contains(APPROVED_AUDIT) && !audit.contains(DENIED_AUDIT),
        "nothing audited"
    );
}

#[test]
fn g7_approval_strings_and_credentials_reach_only_finish_and_owner_approval_status() {
    let g = G7::new();
    let id = g.asked(delete("pg"));
    let begun = g.begin(&id).expect("begin");
    let statement = text(&begun, "statement");
    let challenge = text(&begun, "challenge");
    let credential = text(&begun["allowCredentials"][0], "id");
    let finished = g
        .approve(&id, &statement, &g.proof(&statement, |_| {}))
        .expect("approved");
    let approval = g.approval(&id);
    let proof_part = approval
        .split('.')
        .nth(1)
        .expect("the proof half")
        .to_string();
    let mut seen = vec![
        ("the audit", g.audit()),
        ("the log", g.logs()),
        ("the finish", finished.to_string()),
        ("the list", json!(g.list()).to_string()),
        (
            "another token's status",
            g.status("other", Scope::Doctor, &id)
                .expect("status")
                .to_string(),
        ),
    ];
    {
        let kernel = g.rig.kernel.lock().expect("kernel");
        seen.push((
            "the view",
            g.requests
                .view(&kernel, &id, NOW)
                .expect("a view")
                .to_string(),
        ));
        seen.push((
            "the passkey summary",
            requests::passkey_summary(&kernel, RP).to_string(),
        ));
    }
    let again = refusal(g.approve(&id, &statement, &g.proof(&statement, |_| {})));
    seen.push(("a refusal", format!("{again:?}")));
    let error = g
        .remove(&format!("{approval}x"))
        .expect_err("a bent approval is refused");
    seen.push(("a tool error", format!("{error:?}")));
    for (place, text) in &seen {
        for (what, planted) in [
            ("the approval", approval.as_str()),
            ("the proof", proof_part.as_str()),
            ("the credential id", credential.as_str()),
            ("the challenge", challenge.as_str()),
        ] {
            assert!(!text.contains(planted), "{what} appears in {place}");
        }
    }
    assert!(
        g.mine(&id)["approval"].as_str() == Some(approval.as_str()),
        "status gives it"
    );
}

#[test]
fn g7_owner_approval_list_window_order_cap_and_passkey_summary() {
    let g = G7::new();
    let kernel = g.rig.kernel.lock().expect("kernel");
    let doctor = caller("doctor", Scope::Doctor);
    g.requests
        .request(&kernel, &delete("old"), &doctor, NOW)
        .expect("an old request");
    let start = NOW + 2 * HOUR;
    for n in 0..55u64 {
        let at = start + n * 1_000;
        let created = g
            .requests
            .request(
                &kernel,
                &delete(&format!("s{n}")),
                &caller(&format!("t{n}"), Scope::Doctor),
                at,
            )
            .expect("a request");
        g.requests
            .deny(&kernel, &id_of(&created), at)
            .expect("denied");
    }
    let listed = g.requests.list(&kernel, NOW + 25 * HOUR).expect("the list");
    assert_eq!(listed.len(), 50, "50 at most");
    let targets: Vec<&str> = listed
        .iter()
        .map(|row| row["fields"]["target"].as_str().expect("a target"))
        .collect();
    assert_eq!(targets[0], "s54", "newest first");
    assert_eq!(targets[49], "s5");
    assert!(!targets.contains(&"old"), "the last 24 h only");
    let created: Vec<&str> = listed
        .iter()
        .map(|row| row["createdAt"].as_str().expect("a time"))
        .collect();
    assert!(created.windows(2).all(|pair| pair[0] >= pair[1]));
    assert_eq!(listed[0]["droidd"], "arca");
    assert_eq!(listed[0]["host"], "node1");
    assert_eq!(
        listed[0]["requester"],
        json!({ "kind": "doctor", "name": "t54" })
    );
    assert_eq!(listed[0]["state"], "denied");
    assert_eq!(
        requests::passkey_summary(&kernel, RP),
        json!({ "enrolled": 1, "forThisAddress": 1, "rpId": RP, "otherRpIds": [] })
    );
    assert_eq!(
        requests::passkey_summary(&kernel, "box3.example.com"),
        json!({
            "enrolled": 1,
            "forThisAddress": 0,
            "rpId": "box3.example.com",
            "otherRpIds": [RP],
        })
    );
}

#[test]
fn g7_owner_approval_request_refuses_args_that_read_as_an_approval_in_plain_words() {
    let g = G7::new();
    let statement = canonical_json(&json!({
        "host": "node1",
        "action": "delete",
        "target": "pg",
        "paramsDigest": params_digest(&json!({ "name": "pg" })).expect("digest"),
        "issuedAt": iso_utc(NOW),
        "expiresAt": iso_utc(NOW + 5 * MINUTE),
        "nonce": "b".repeat(32),
    }))
    .expect("canonical");
    let ssh = common::approve_statement(&common::SkKey::from_seed(3), &statement);
    let passkey = passkey_approval(
        &statement,
        &g.key
            .assert(&Assert::over(ARCA_NAMESPACE, statement.as_bytes())),
    );
    for (planted, args, at) in [
        (
            &ssh,
            json!({ "note": ssh }),
            "args.note is an approval string",
        ),
        (
            &passkey,
            json!({ "list": ["x", passkey] }),
            "args.list[1] is an approval string",
        ),
        (
            &ssh,
            json!({ ssh.clone(): 1 }),
            "args key is an approval string",
        ),
    ] {
        let why = invalid(g.ask(
            "doctor",
            Scope::Doctor,
            json!({ "kind": "arca", "action": "delete", "target": "pg", "args": args }),
        ));
        assert!(why.contains(at), "{why}");
        assert!(!why.contains(planted.as_str()), "never quoting it");
        assert!(!why.contains(&planted[..16]), "nor a part of it");
    }
    assert!(g.list().is_empty());
}

#[test]
fn g7_owner_approval_list_keeps_every_pending_request_first() {
    let g = G7::new();
    let kernel = g.rig.kernel.lock().expect("kernel");
    let start = NOW + HOUR;
    let pending: Vec<String> = (0..3)
        .map(|n| {
            id_of(
                &g.requests
                    .request(
                        &kernel,
                        &delete(&format!("p{n}")),
                        &caller(&format!("p{n}"), Scope::Doctor),
                        start,
                    )
                    .expect("pending"),
            )
        })
        .collect();
    for n in 0..55u64 {
        let at = start + 1_000 + n * 100;
        let created = g
            .requests
            .request(
                &kernel,
                &delete(&format!("s{n}")),
                &caller(&format!("t{n}"), Scope::Doctor),
                at,
            )
            .expect("a request");
        g.requests
            .deny(&kernel, &id_of(&created), at)
            .expect("denied");
    }
    let listed = g.requests.list(&kernel, start + MINUTE).expect("the list");
    assert_eq!(listed.len(), 50, "50 in all");
    let ids: Vec<String> = listed.iter().map(|row| text(row, "id")).collect();
    for id in &pending {
        assert!(ids.contains(id), "every pending request is listed");
    }
    let others: Vec<&str> = listed
        .iter()
        .filter(|row| row["state"] == "denied")
        .map(|row| row["fields"]["target"].as_str().expect("a target"))
        .collect();
    assert_eq!(others.len(), 47);
    assert_eq!(others[0], "s54", "then the newest others");
    assert_eq!(others[46], "s8");
}

#[test]
fn g7_every_audit_row_of_a_request_takes_its_digest_from_the_stored_subject() {
    let g = G7::new();
    let want = params_digest(&json!({ "name": "pg" })).expect("digest");
    let approved = g.asked(delete("pg"));
    let denied = g.asked(delete("svc2"));
    let path = g.rig.dir.join(REQUESTS_FILE);
    let mut rows: Value =
        serde_json::from_slice(&fs::read(&path).expect("the file")).expect("rows");
    let planted = "f".repeat(64);
    for row in rows.as_array_mut().expect("rows") {
        row["paramsDigest"] = json!(planted);
    }
    fs::write(&path, serde_json::to_vec(&rows).expect("JSON")).expect("rewritten");
    let reopened = ApprovalRequests::open(&g.rig.dir, None, NOW).expect("reopened");
    {
        let kernel = g.rig.kernel.lock().expect("kernel");
        let begun = reopened.begin(&kernel, &approved, RP, NOW).expect("begin");
        let statement = text(&begun, "statement");
        assert_eq!(
            parse_statement(statement.as_bytes())
                .expect("parses")
                .params_digest,
            want,
            "the statement's digest is the subject's"
        );
        reopened
            .approve(
                &kernel,
                &approved,
                &statement,
                &g.proof(&statement, |_| {}),
                NOW,
            )
            .expect("approved");
        reopened.deny(&kernel, &denied, NOW).expect("denied");
    }
    let audit = g.audit();
    let rows: Vec<&str> = audit.lines().collect();
    assert_eq!(rows.len(), 4, "two requested, one approved, one denied");
    for (row, tool) in rows.iter().zip([
        REQUESTED_AUDIT,
        REQUESTED_AUDIT,
        APPROVED_AUDIT,
        DENIED_AUDIT,
    ]) {
        assert!(row.contains(tool), "{tool}");
        assert!(
            !row.contains(&planted),
            "{tool}: never the params_digest column"
        );
    }
    assert!(rows[2].contains(&format!("params={want}")));
    assert!(rows[3].contains(&format!(
        "params={}",
        params_digest(&json!({ "name": "svc2" })).expect("digest")
    )));
}

#[test]
fn g7_arca_checks_a_proof_in_full_against_its_request_and_spends_it_exactly_once() {
    // Arca's own test from the dropped owner_approval_deliver title: kind, fields, digest, host, expiry and an unused nonce.
    let g = G7::new();
    g.service();
    let spent = g.asked(delete("svc2"));
    g.tap(&spent);
    let spent_approval = g.approval(&spent);
    g.spend("svc2", &spent_approval)
        .expect("another approval is spent");
    let nonce_of = |approval: &str| {
        let (statement, _) = arca_approval::split_approval(approval).expect("splits");
        parse_statement(&statement).expect("parses").nonce
    };
    let spent_nonce = nonce_of(&spent_approval);
    let held = g.asked(delete("svc3"));
    g.tap(&held);
    let held_nonce = nonce_of(&g.approval(&held));

    let id = g.asked(delete("pg"));
    let statement = g.statement(&id);
    let craft = |overrides: Value| {
        let mut fields: Value = serde_json::from_str(&statement).expect("JSON");
        for (key, value) in overrides.as_object().expect("an object") {
            fields[key.as_str()] = value.clone();
        }
        canonical_json(&fields).expect("canonical")
    };
    let cases: Vec<(&str, String, &str)> = vec![
        (
            "another kind (droidd's)",
            craft(json!({ "droidd": "box1" })),
            "droidd",
        ),
        (
            "another action",
            craft(json!({ "action": "publish" })),
            "another action than the request's",
        ),
        (
            "another target",
            craft(json!({ "target": "other" })),
            "another target than the request's",
        ),
        (
            "another digest",
            craft(json!({ "paramsDigest": "0".repeat(64) })),
            "another paramsDigest than the request's",
        ),
        (
            "another host",
            craft(json!({ "host": "node2" })),
            "it is for host node2",
        ),
        (
            "expired",
            craft(
                json!({ "issuedAt": iso_utc(NOW - 10 * MINUTE), "expiresAt": iso_utc(NOW - MINUTE) }),
            ),
            "it has expired",
        ),
        (
            "too long",
            craft(json!({ "expiresAt": iso_utc(NOW + 16 * MINUTE) })),
            "at most 15 min",
        ),
        (
            "issued ahead",
            craft(
                json!({ "issuedAt": iso_utc(NOW + 6 * MINUTE), "expiresAt": iso_utc(NOW + 10 * MINUTE) }),
            ),
            "issued in the future",
        ),
        (
            "a spent nonce",
            craft(json!({ "nonce": spent_nonce })),
            "its nonce is spent",
        ),
        (
            "another request's nonce",
            craft(json!({ "nonce": held_nonce })),
            "approved for another request already",
        ),
    ];
    for (name, crafted, want) in cases {
        let refused = refusal(g.approve(&id, &crafted, &g.proof(&crafted, |_| {})));
        assert_eq!(
            (refused.status, refused.code),
            (403, "owner_approval"),
            "{name}"
        );
        assert!(
            refused.message.contains(want),
            "{name}: {}",
            refused.message
        );
        assert!(
            !refused.message.contains(&crafted),
            "{name}: never the statement's bytes"
        );
    }
    let other = g.proof(&craft(json!({ "nonce": "c".repeat(32) })), |_| {});
    let wrong_origin = g.proof(&statement, |a| {
        a.origin = Some("https://box1.example.com:9443".to_string())
    });
    for (name, proof) in [
        ("a proof over another statement", other),
        ("another origin", wrong_origin),
    ] {
        let refused = refusal(g.approve(&id, &statement, &proof));
        assert_eq!(
            (refused.status, refused.code),
            (403, "assertion_refused"),
            "{name}"
        );
    }
    assert_eq!(
        g.mine(&id)["state"],
        "pending",
        "every refusal leaves it pending"
    );
    assert!(!g.audit().contains(&format!("approved:{id}")));

    g.approve(&id, &statement, &g.proof(&statement, |_| {}))
        .expect("the genuine proof approves");
    let approval = g.approval(&id);
    assert_eq!(
        g.remove(&approval).expect("spent"),
        json!({ "removed": true })
    );
    assert_eq!(
        g.spend("pg", &approval),
        Err(arca_approval::ApprovalRefusal::Replayed),
        "exactly once"
    );
}

// ---- fix round 1 ---------------------------------------------------------------------------------------------------------

/// The MCP door over the rig's three verbs: `arca_mcp::Server` with the real verb table, so every result is checked against
/// its verb's result schema as it is in production. A doctor's and a viewer's token are minted into a private directory.
struct Door {
    socket: std::path::PathBuf,
    doctor: String,
    viewer: String,
    _runtime: tokio::runtime::Runtime,
}

impl Door {
    fn over(g: &G7) -> Door {
        use arca_mcp::{Server, TokenStore, mint, serve_unix};
        let tokens = g.rig.dir.join("door-tokens");
        fs::create_dir(&tokens).expect("the token directory");
        fs::set_permissions(&tokens, fs::Permissions::from_mode(0o700)).expect("0700");
        let mut secrets = Vec::new();
        for (id, scope) in [("doctor", Scope::Doctor), ("phone", Scope::Viewer)] {
            let (secret, hash) = mint(scope);
            let path = tokens.join(format!("{id}.hash"));
            fs::write(&path, hash).expect("the hash file");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).expect("0400");
            secrets.push(secret);
        }
        let mut server = Server::new(
            arca_api::verbs(),
            TokenStore::load(&tokens).expect("the tokens load"),
        );
        for (verb, handler) in &g.verbs {
            server = server.handle(verb, Arc::clone(handler));
        }
        let socket = g.rig.dir.join("door.sock");
        let runtime = tokio::runtime::Runtime::new().expect("a runtime");
        let served = socket.clone();
        let router = server.router();
        runtime.spawn(async move {
            let _ = serve_unix(router, &served).await;
        });
        let until = Instant::now() + Duration::from_secs(5);
        while !socket.exists() {
            assert!(Instant::now() < until, "the door started");
            std::thread::sleep(Duration::from_millis(10));
        }
        Door {
            socket,
            viewer: secrets.pop().expect("the viewer's"),
            doctor: secrets.pop().expect("the doctor's"),
            _runtime: runtime,
        }
    }

    /// A `tools/call` through the door; the JSON-RPC reply.
    fn call(&self, secret: &str, name: &str, arguments: Value) -> Value {
        use std::io::{Read as _, Write as _};
        let body = serde_json::to_vec(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        }))
        .expect("the call");
        let head = format!(
            "POST /mcp HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {secret}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let mut stream =
            std::os::unix::net::UnixStream::connect(&self.socket).expect("connect to the door");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("a read timeout");
        stream.write_all(head.as_bytes()).expect("the head");
        stream.write_all(&body).expect("the body");
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).expect("the answer");
        let split = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("a response head");
        serde_json::from_slice(&raw[split + 4..]).expect("JSON-RPC")
    }

    /// The structured result of a call the door answered without error.
    fn ok(&self, secret: &str, name: &str, arguments: Value) -> Value {
        let reply = self.call(secret, name, arguments);
        assert_eq!(reply["result"]["isError"], json!(false), "{name}: {reply}");
        reply["result"]["structuredContent"].clone()
    }
}

/// A request at every bound the list's result carries: a 64-character args key, a 128-character target, 4096 bytes of args
/// and args nested 8 levels deep.
fn at_every_bound() -> Value {
    let deep = (0..7).fold(json!("leaf"), |inner, _| json!({ "in": inner }));
    let target = format!("t{}", "x".repeat(127));
    let mut args = json!({ "k".repeat(64): 1, "deep": deep, "pad": "" });
    let size = serde_json::to_string(&args).expect("JSON").len();
    args["pad"] = json!("p".repeat(requests::ARGS_MAX - size));
    assert_eq!(
        serde_json::to_string(&args).expect("JSON").len(),
        requests::ARGS_MAX
    );
    json!({ "kind": "arca", "action": "delete", "target": target, "args": args })
}

#[test]
fn m1_through_the_door_a_65_character_args_key_is_refused_and_a_request_at_every_bound_lists_and_answers()
 {
    let g = G7::new();
    let door = Door::over(&g);
    let long = json!({ "kind": "arca", "action": "delete", "target": "pg", "args": { "k".repeat(65): 1 } });
    let refused = door.call(&door.doctor, "owner_approval_request", long);
    let message = refused["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("longer than 64 characters"), "{refused}");
    assert!(!message.contains(&"k".repeat(65)), "the key is not quoted");
    let listed = door.ok(&door.viewer, "owner_approval_list", json!({}));
    assert_eq!(listed["requests"], json!([]), "nothing was kept");

    let created = door.ok(&door.doctor, "owner_approval_request", at_every_bound());
    let id = text(&created, "id");
    let listed = door.ok(&door.viewer, "owner_approval_list", json!({}));
    assert_eq!(
        listed["requests"][0]["id"],
        json!(id),
        "the list still answers"
    );
    let status = door.ok(&door.doctor, "owner_approval_status", json!({ "id": id }));
    assert_eq!(status["state"], "pending");
    g.tap(&id);
    let status = door.ok(&door.doctor, "owner_approval_status", json!({ "id": id }));
    assert_eq!(status["state"], "approved");
    assert!(
        status["approval"].is_string(),
        "the approval string passes the result check"
    );
    let listed = door.ok(&door.viewer, "owner_approval_list", json!({}));
    assert_eq!(listed["requests"][0]["state"], "approved");
}

#[test]
fn l6_a_target_that_names_a_delete_plan_is_refused() {
    let g = G7::new();
    let plan = format!("plan-{}", "e3".repeat(32));
    let why = invalid(g.ask(
        "doctor",
        Scope::Doctor,
        json!({ "kind": "arca", "action": "delete", "target": plan, "args": { "plan": "x" } }),
    ));
    assert!(why.contains("names a delete plan"), "{why}");
    g.asked(delete("plan-b"));
}

#[test]
fn l1_decided_rows_leave_the_file_once_out_of_the_list_and_an_unspent_approval_never_does() {
    let g = G7::new();
    let kept = g.asked(delete("svc1"));
    g.tap(&kept);
    let approval = g.approval(&kept);
    let rows = |g: &G7| {
        serde_json::from_str::<Vec<Value>>(&g.stored())
            .expect("rows")
            .len()
    };
    let kernel = g.rig.kernel.lock().expect("kernel");
    for n in 0..70u64 {
        let at = NOW + 1_000 + n * 100;
        let created = g
            .requests
            .request(
                &kernel,
                &delete(&format!("s{n}")),
                &caller(&format!("t{n}"), Scope::Doctor),
                at,
            )
            .expect("a request");
        g.requests
            .deny(&kernel, &id_of(&created), at)
            .expect("denied");
    }
    let listed = g.requests.list(&kernel, NOW + 10_000).expect("the list");
    drop(kernel);
    assert_eq!(listed.len(), 50);
    assert_eq!(
        rows(&g),
        51,
        "the unspent approval and the newest 50 others; the rest are gone"
    );
    assert_eq!(
        g.approval(&kept),
        approval,
        "an unspent approval is never dropped"
    );
    // After the approval expires and the window passes, the file holds only what the list could show.
    g.at(NOW + 25 * HOUR);
    assert!(g.list().is_empty());
    assert_eq!(rows(&g), 0, "nothing older than 24 h is kept");
    assert!(!g.stored().contains(&approval));
}

#[test]
fn l3_a_change_that_cannot_be_saved_is_never_audited() {
    let g = G7::new();
    let id = g.asked(delete("pg"));
    let statement = g.statement(&id);
    let lines = g.audit().lines().count();
    let locked = |mode: u32| {
        fs::set_permissions(&g.rig.dir, fs::Permissions::from_mode(mode)).expect("the mode");
    };
    locked(0o500);
    let asked = g.ask("doctor", Scope::Doctor, delete("svc2"));
    let approved = g.approve(&id, &statement, &g.proof(&statement, |_| {}));
    let denied = g.deny(&id);
    locked(0o700);
    assert!(matches!(asked, Err(CallError::Internal(_))), "{asked:?}");
    assert_eq!(refusal(approved).status, 500);
    assert_eq!(refusal(denied).status, 500);
    assert_eq!(
        g.audit().lines().count(),
        lines,
        "no audit line for a change not saved"
    );
    assert_eq!(g.mine(&id)["state"], "pending");
    assert_eq!(g.list().len(), 1, "the request not saved was not kept");
}
