//! Action proposals: the inert record, its kernel-built summary, the closed action list, the waiting limit, and the
//! approval checks that gate exactly one run. All of it runs in-process against the toy kernel and fake Incus; no service or
//! database is touched.

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use arca::action_proposal::{ACTION_PROPOSALS_FILE, MAX_WAITING};
use arca::handlers::{ActionExecutor, plan_handlers};
use arca::kernel::Kernel;
use arca_api::{Scope, verb};
use arca_approval::{ApprovalRefusal, ArcaAction, parse_statement};
use arca_mcp::{CallError, Caller, ToolHandler};
use arca_reconcile::PLAN_TTL_MS;
use common::*;
use serde_json::{Value, json};

/// A handler that counts its runs. It stands in for the gated verb, so the test can prove the page ran it once.
struct Counting {
    runs: Arc<AtomicU64>,
}

impl ToolHandler for Counting {
    fn call(&self, _args: Value, _caller: &Caller) -> Result<Value, CallError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        Ok(json!({ "created": true }))
    }
}

struct Rig {
    _scratch: Scratch,
    _fake: arca_incus::FakeIncus,
    kernel: Arc<Mutex<Kernel>>,
    clock: Arc<AtomicU64>,
    handlers: std::collections::HashMap<&'static str, Arc<dyn ToolHandler>>,
    runs: Arc<AtomicU64>,
    executor: ActionExecutor,
}

impl Rig {
    fn new(tag: &str, key: &SkKey) -> Rig {
        let scratch = Scratch::new(tag);
        let fake = start_fake(&scratch);
        let verifier = verifier_for(&scratch, &[key], tag);
        let kernel = Arc::new(Mutex::new(
            kernel(&scratch, &fake, verifier, NOW).expect("the kernel opens"),
        ));
        let clock = Arc::new(AtomicU64::new(NOW));
        let handlers = plan_handlers(Arc::clone(&kernel), {
            let clock = Arc::clone(&clock);
            Arc::new(move || clock.load(Ordering::SeqCst))
        })
        .into_iter()
        .collect();
        let runs = Arc::new(AtomicU64::new(0));
        let executor = ActionExecutor::new([(
            "service_create",
            Arc::new(Counting {
                runs: Arc::clone(&runs),
            }) as Arc<dyn ToolHandler>,
        )]);
        Rig {
            _scratch: scratch,
            _fake: fake,
            kernel,
            clock,
            handlers,
            runs,
            executor,
        }
    }

    fn propose(&self, action: &str, args: Value) -> Result<Value, CallError> {
        call_verb(
            &self.handlers,
            "action_propose",
            json!({ "action": action, "args": args }),
        )
    }

    fn list(&self) -> Value {
        call_verb(&self.handlers, "plan_list", json!({})).expect("plan_list answers")
    }

    fn at(&self, now: u64) {
        self.clock.store(now, Ordering::SeqCst);
    }

    fn approve(
        &self,
        id: &str,
        approval: &str,
        now: u64,
    ) -> Result<arca::kernel::ApprovedAction, CallError> {
        let mut kernel = self.kernel.lock().expect("the kernel lock");
        arca::handlers::approve_action(
            &mut kernel,
            &json!({ "plan": id, "approval": approval }),
            now,
        )
    }
}

fn create_args(name: &str) -> Value {
    json!({ "name": name, "template": "nats" })
}

fn statement(host: &str, id: &str, action: &str, digest: &str, nonce: &str) -> String {
    arca::handlers::action_statement(host, id, action, digest, NOW, nonce)
        .expect("a kernel-built statement")
}

#[test]
fn a_viewer_may_propose_and_nothing_changes() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("proposal-inert", &key);
    let before = rig.kernel.lock().expect("lock").entries().to_vec();
    let viewer = Caller {
        token_id: "viewer".to_string(),
        scope: Scope::Viewer,
        tools: None,
    };
    let proposed = rig.handlers["action_propose"]
        .call(
            json!({ "action": "service_create", "args": create_args("nats") }),
            &viewer,
        )
        .expect("a viewer may propose");
    assert_eq!(
        proposed["kind"],
        json!(null),
        "the record has no listing kind"
    );
    assert_eq!(proposed["action"], json!("service_create"));
    assert_eq!(proposed["summary"], json!("Create the service nats"));
    assert!(
        proposed["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("act-") && id.len() == 16)
    );
    assert_eq!(
        rig.kernel.lock().expect("lock").entries(),
        before.as_slice(),
        "the desired state is unchanged"
    );
    assert_eq!(rig.list()["plans"].as_array().map(Vec::len), Some(1));
    assert!(
        rig.list()["plans"][0].get("statement").is_none(),
        "plan_list leaves the action statement to the approval page"
    );
    assert_eq!(
        arca_api::may_call(Scope::Viewer, verb("approval_submit").expect("present")),
        false,
        "a viewer can never approve"
    );
    for name in [
        "service_create",
        "service_remove",
        "service_publish",
        "service_unpublish",
        "service_connect",
        "service_disconnect",
        "postgres_create",
        "postgres_delete",
    ] {
        assert!(
            !arca_api::may_call(Scope::Viewer, verb(name).expect("present")),
            "a viewer can never apply {name}"
        );
    }
}

#[test]
fn the_summary_is_built_by_the_kernel_and_an_extra_field_is_refused() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("proposal-summary", &key);
    let proposed = rig
        .propose("service_remove", json!({ "name": "nats" }))
        .expect("a removal proposal");
    assert_eq!(proposed["summary"], json!("Remove the service nats"));
    let injected = rig
        .propose(
            "service_create",
            json!({ "name": "nats", "template": "nats", "summary": "caller text" }),
        )
        .expect_err("the schema refuses an unlisted field");
    assert!(matches!(injected, CallError::Invalid(_)));
    assert_eq!(
        rig.list()["plans"].as_array().map(Vec::len),
        Some(1),
        "the refusal recorded nothing"
    );
}

#[test]
fn a_disallowed_action_is_refused_and_records_nothing() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("proposal-disallowed", &key);
    for action in ["service_secret_set", "reboot", "install", ""] {
        let error = rig
            .propose(action, json!({ "name": "nats" }))
            .expect_err("not on the list");
        assert!(matches!(error, CallError::Invalid(_)), "{action}");
    }
    assert_eq!(rig.list()["plans"], json!([]));
}

#[test]
fn the_same_digest_returns_the_same_record_and_a_ninth_is_refused() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("proposal-limit", &key);
    let first = rig
        .propose("service_create", create_args("nats"))
        .expect("the first");
    let again = rig
        .propose("service_create", create_args("nats"))
        .expect("the duplicate");
    assert_eq!(first, again, "the same digest returns the existing record");
    for index in 1..MAX_WAITING {
        rig.propose("service_create", create_args(&format!("svc{index}")))
            .expect("a distinct proposal");
    }
    assert_eq!(
        rig.list()["plans"].as_array().map(Vec::len),
        Some(MAX_WAITING)
    );
    let ninth = rig
        .propose("service_create", create_args("ninth"))
        .expect_err("the limit is eight");
    match ninth {
        CallError::Refused { code, .. } => assert_eq!(code, "TooManyProposals"),
        other => panic!("a refusal was expected, got {other:?}"),
    }
    assert_eq!(
        rig.propose("service_create", create_args("nats"))
            .expect("the dedupe still answers at the limit"),
        first
    );
}

#[test]
fn a_lapsed_proposal_is_left_out_and_a_fresh_one_fits() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("proposal-lapse", &key);
    for index in 0..MAX_WAITING {
        rig.propose("service_create", create_args(&format!("svc{index}")))
            .expect("a proposal");
    }
    rig.at(NOW + PLAN_TTL_MS + 1);
    assert_eq!(rig.list()["plans"], json!([]), "all eight lapsed");
    rig.propose("service_create", create_args("fresh"))
        .expect("the lapse made room");
}

#[test]
fn plan_decline_accepts_an_action_id_and_removes_it() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("proposal-decline", &key);
    let proposed = rig
        .propose("service_create", create_args("nats"))
        .expect("a proposal");
    let id = proposed["id"].as_str().expect("an id");
    let declined = call_verb(&rig.handlers, "plan_decline", json!({ "plan": id }))
        .expect("plan_decline declines an action proposal");
    assert_eq!(declined["plan"], json!(id));
    assert_eq!(declined["retained"], json!([]));
    assert_eq!(rig.list()["plans"], json!([]));
    let again = call_verb(&rig.handlers, "plan_decline", json!({ "plan": id }))
        .expect_err("a second decline finds nothing");
    assert!(matches!(
        again,
        CallError::Refused { ref code, .. } if code == "NoSuchPlan"
    ));
}

#[test]
fn a_valid_owner_approval_runs_once_and_a_replay_does_not_run_again() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("proposal-once", &key);
    let proposed = rig
        .propose("service_create", create_args("nats"))
        .expect("a proposal");
    let id = proposed["id"].as_str().expect("an id");
    let digest = proposed["argsDigest"].as_str().expect("a digest");
    let statement = statement(HOST, id, "service_create", digest, &"a".repeat(32));
    let approval = approve_statement(&key, &statement);
    let approved = rig
        .approve(id, &approval, NOW)
        .expect("the approval verifies");
    assert_eq!(approved.id, id);
    rig.executor
        .run(&approved.action, &approved.args, &approved.grant)
        .expect("the action runs");
    assert_eq!(rig.runs.load(Ordering::SeqCst), 1);
    rig.kernel
        .lock()
        .expect("lock")
        .journal_action_result(&approved, true, None);
    let journal = std::fs::read_to_string(rig._scratch.path(ACTION_PROPOSALS_FILE))
        .expect("the proposals journal");
    let events: Vec<Value> = journal
        .lines()
        .map(|line| serde_json::from_str(line).expect("an event"))
        .collect();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0]["event"], json!("proposed"));
    assert_eq!(events[1]["event"], json!("approved"));
    assert_eq!(events[2]["event"], json!("result"));
    assert_eq!(events[2]["ok"], json!(true));
    assert_eq!(rig.list()["plans"], json!([]), "the proposal is retired");

    let replay = rig
        .approve(id, &approval, NOW)
        .expect_err("the replay is refused");
    match replay {
        CallError::Refused { code, .. } => assert_eq!(code, "NoSuchPlan"),
        other => panic!("a refusal was expected, got {other:?}"),
    }
    assert_eq!(rig.runs.load(Ordering::SeqCst), 1, "still one run");
}

#[test]
fn an_expired_proposal_is_refused_before_its_approval_is_checked() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("proposal-expired", &key);
    let proposed = rig
        .propose("service_create", create_args("nats"))
        .expect("a proposal");
    let id = proposed["id"].as_str().expect("an id");
    let digest = proposed["argsDigest"].as_str().expect("a digest");
    let approval = approve_statement(
        &key,
        &statement(HOST, id, "service_create", digest, &"b".repeat(32)),
    );
    let error = rig
        .approve(id, &approval, NOW + PLAN_TTL_MS + 1)
        .expect_err("the proposal lapsed");
    match error {
        CallError::Refused { code, .. } => assert_eq!(code, "Expired"),
        other => panic!("a refusal was expected, got {other:?}"),
    }
}

#[test]
fn tampered_arguments_or_another_id_are_wrong_params_and_wrong_target() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("proposal-tamper", &key);
    let proposed = rig
        .propose("service_create", create_args("nats"))
        .expect("a proposal");
    let id = proposed["id"].as_str().expect("an id");
    let digest = proposed["argsDigest"].as_str().expect("a digest");

    let other_digest = "c".repeat(64);
    let tampered = approve_statement(
        &key,
        &statement(HOST, id, "service_create", &other_digest, &"d".repeat(32)),
    );
    let error = rig
        .approve(id, &tampered, NOW)
        .expect_err("the digest differs");
    assert!(matches!(
        error,
        CallError::Refused { ref code, .. } if code == "WrongParams"
    ));

    let wrong_id = "act-000000000000";
    let other_target = approve_statement(
        &key,
        &statement(HOST, wrong_id, "service_create", digest, &"e".repeat(32)),
    );
    let error = rig
        .approve(id, &other_target, NOW)
        .expect_err("the target differs");
    assert!(matches!(
        error,
        CallError::Refused { ref code, .. } if code == "WrongTarget"
    ));
    assert!(
        rig.list()["plans"]
            .as_array()
            .is_some_and(|plans| plans.len() == 1)
    );
}

#[test]
fn the_action_statement_names_the_verb_the_id_and_the_args_digest() {
    let statement = statement(
        HOST,
        "act-0123456789ab",
        "service_publish",
        &"f".repeat(64),
        &"1".repeat(32),
    );
    let parsed = parse_statement(statement.as_bytes()).expect("the kernel parser reads it");
    assert_eq!(parsed.action, ArcaAction::ServicePublish);
    assert_eq!(parsed.action.as_str(), "service_publish");
    assert_eq!(parsed.target, "act-0123456789ab");
    assert_eq!(parsed.params_digest, "f".repeat(64));
    assert!(!statement.ends_with('\n'));

    let text = arca::handlers::action_statement(
        HOST,
        "act-0123456789ab",
        "service_publish",
        &"f".repeat(64),
        NOW,
        &"1".repeat(32),
    )
    .expect("a statement");
    assert!(text.starts_with(r#"{"action":"service_publish","expiresAt":""#));
    assert!(text.ends_with(r#""target":"act-0123456789ab"}"#));
}

#[test]
fn a_statement_with_an_unlisted_action_does_not_parse() {
    let text = statement(
        HOST,
        "act-0123456789ab",
        "service_create",
        &"f".repeat(64),
        &"1".repeat(32),
    )
    .replace("service_create", "reboot");
    assert!(matches!(
        parse_statement(text.as_bytes()),
        Err(ApprovalRefusal::Malformed(_))
    ));
}
