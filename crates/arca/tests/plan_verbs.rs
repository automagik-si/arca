//! The plan verbs `plan_list`, `plan_decline` and `approval_submit`, run in-process against the kernel: the toy plugin on the fake
//! Incus, the write provider, the desired-state store and the real owner-approval verifier with scratch signers. Each refusal
//! path is its own test.

mod common;

use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use arca::kernel::Kernel;
use arca_approval::parse_statement;
use arca_kernel::canon::Digest;
use arca_kernel::desired::EntryId;
use arca_reconcile::Outcome;
use common::*;
use serde_json::{Value, json};

/// The instance a toy plan waits to delete, without its uuid. The uuid is the one Incus gave the instance when it was created, so a
/// test checks that a uuid is present (see `without_uuid`) rather than its value.
fn instance_obj(tag: &str) -> Value {
    json!({
        "kind": "instance",
        "project": PROJECT,
        "name": format!("toy-box-{tag}"),
    })
}

/// The objects of a plan or a run with each uuid removed. An instance's uuid is present and is not compared.
fn without_uuid(objects: &Value) -> Value {
    let stripped: Vec<Value> = objects
        .as_array()
        .expect("an array of objects")
        .iter()
        .map(|object| {
            assert!(
                object["uuid"].is_string(),
                "an instance names its uuid: {object}"
            );
            let mut bare = object.clone();
            bare.as_object_mut().expect("an object").remove("uuid");
            bare
        })
        .collect();
    Value::Array(stripped)
}

/// A kernel and its plan verbs, with the clock at `now`, over a fresh fake.
struct Rig {
    scratch: Scratch,
    fake: arca_incus::FakeIncus,
    kernel: Arc<Mutex<Kernel>>,
    clock: Arc<AtomicU64>,
    handlers: std::collections::HashMap<&'static str, Arc<dyn arca_mcp::ToolHandler>>,
}

impl Rig {
    fn new(tag: &str, signers: &[&SkKey]) -> Rig {
        let scratch = Scratch::new(tag);
        let fake = start_fake(&scratch);
        let verifier = verifier_for(&scratch, signers, tag);
        let kernel = Arc::new(Mutex::new(
            kernel(&scratch, &fake, verifier, NOW).expect("the kernel opens"),
        ));
        let clock = Arc::new(AtomicU64::new(NOW));
        let handlers = plan_verbs(Arc::clone(&kernel), Arc::clone(&clock));
        Rig {
            scratch,
            fake,
            kernel,
            clock,
            handlers,
        }
    }

    fn at(&self, ms: u64) {
        self.clock.store(ms, std::sync::atomic::Ordering::SeqCst);
    }

    fn propose(&self, changes: Vec<arca::desired_store::EntryChange>, now: u64) -> Vec<Outcome> {
        self.kernel
            .lock()
            .expect("the kernel lock")
            .propose(changes, now)
            .expect("the proposal is accepted")
    }

    fn plan_list(&self) -> Value {
        call_verb(&self.handlers, "plan_list", json!({})).expect("plan_list answers")
    }

    /// The first waiting plan's digest and statement.
    fn first_plan(&self) -> (String, String) {
        let list = self.plan_list();
        let plan = &list["plans"][0];
        (
            plan["plan"].as_str().expect("a digest").to_string(),
            plan["statement"].as_str().expect("a statement").to_string(),
        )
    }

    /// A toy removal: the entry `toy-scratch` is applied, then removed, so its instance waits for an approval.
    fn removed_toy(&self, tag: &str) {
        self.propose(
            vec![arca::desired_store::EntryChange::Put(toy_entry(tag))],
            NOW,
        );
        self.propose(
            vec![arca::desired_store::EntryChange::Remove(EntryId(
                "toy-scratch".to_string(),
            ))],
            NOW + 1_000,
        );
    }
}

fn submit(rig: &Rig, plan: &str, approval: &str) -> Result<Value, arca_mcp::CallError> {
    call_verb(
        &rig.handlers,
        "approval_submit",
        json!({ "plan": plan, "approval": approval }),
    )
}

fn refusal_code(error: &arca_mcp::CallError) -> &str {
    match error {
        arca_mcp::CallError::Refused { code, .. } => code.as_str(),
        other => panic!("a refusal was expected, got {other:?}"),
    }
}

#[test]
fn a_removal_proposed_through_the_store_leaves_one_plan_for_its_instance() {
    let rig = Rig::new("removal", &[&SkKey::from_seed(1)]);
    rig.removed_toy("t1");
    let list = rig.plan_list();
    let plans = list["plans"].as_array().expect("plans");
    assert_eq!(plans.len(), 1, "{list}");
    assert_eq!(plans[0]["entry"], json!("toy-scratch"));
    assert_eq!(
        without_uuid(&plans[0]["objects"]),
        json!([instance_obj("t1")])
    );
    assert_eq!(
        instances_marked(&rig.fake, MARK, "toy-scratch"),
        vec!["toy-box-t1".to_string()],
        "the removal reverted the ACL and left the instance"
    );
}

#[test]
fn the_statement_parses_with_the_kernel_parser_and_names_the_plan_and_its_arguments() {
    let rig = Rig::new("statement", &[&SkKey::from_seed(1)]);
    rig.removed_toy("t1");
    let list = rig.plan_list();
    let plan = &list["plans"][0];
    let digest = plan["plan"].as_str().expect("a digest");
    let statement = plan["statement"].as_str().expect("a statement");

    // The exact bytes: the keys in sorted order, no spaces, no trailing newline.
    assert!(
        statement.starts_with(r#"{"action":"delete","expiresAt":""#),
        "{statement}"
    );
    assert!(statement.ends_with(&format!(
        r#""paramsDigest":"{}","target":"plan-{digest}"}}"#,
        plan_args_digest(digest)
    )));
    assert!(!statement.ends_with('\n'));

    let parsed = parse_statement(statement.as_bytes()).expect("the kernel parser reads it");
    assert_eq!(parsed.host, HOST);
    assert_eq!(parsed.target, format!("plan-{digest}"));
    assert_eq!(parsed.params_digest, plan_args_digest(digest));
    assert_eq!(parsed.nonce.len(), 32);
    assert_eq!(parsed.issued_at, "2026-10-30T12:00:00.000Z");
    assert_eq!(parsed.expires_at, "2026-10-30T12:10:00.000Z");

    // The plan's own limit is fifteen minutes from its creation.
    let created = plan["createdAt"].as_u64().expect("createdAt");
    assert_eq!(plan["expiresAt"].as_u64(), Some(created + 15 * 60 * 1000));
}

#[test]
fn every_plan_list_builds_its_statement_fresh_with_a_new_nonce() {
    let rig = Rig::new("fresh", &[&SkKey::from_seed(1)]);
    rig.removed_toy("t1");
    let (_, first) = rig.first_plan();
    let (_, second) = rig.first_plan();
    let a = parse_statement(first.as_bytes()).expect("parses");
    let b = parse_statement(second.as_bytes()).expect("parses");
    assert_ne!(a.nonce, b.nonce, "each listing draws a new nonce");
}

#[test]
fn an_approval_signed_over_the_listed_statement_deletes_the_instance_once() {
    let owner = SkKey::from_seed(1);
    let rig = Rig::new("approve", &[&owner]);
    rig.removed_toy("t1");
    let (digest, statement) = rig.first_plan();
    let approval = approve_statement(&owner, &statement);

    let result = submit(&rig, &digest, &approval).expect("the approval deletes");
    assert_eq!(result["plan"], json!(digest));
    assert_eq!(
        without_uuid(&result["deleted"]),
        json!([instance_obj("t1")])
    );
    assert_eq!(result["drift"], json!([]));
    assert_eq!(result["partial"], json!(false));
    assert_eq!(
        deletes_for(&rig.fake, "toy-box-t1"),
        1,
        "one DELETE was sent"
    );
    assert!(instances_marked(&rig.fake, MARK, "toy-scratch").is_empty());
    assert_eq!(rig.plan_list()["plans"], json!([]), "the plan is retired");
}

#[test]
fn a_replayed_approval_is_refused_as_replayed_and_deletes_nothing_more() {
    let owner = SkKey::from_seed(1);
    let rig = Rig::new("replay", &[&owner]);
    rig.removed_toy("t1");
    let (digest, statement) = rig.first_plan();
    let approval = approve_statement(&owner, &statement);
    submit(&rig, &digest, &approval).expect("the first use deletes");

    let replay = submit(&rig, &digest, &approval).expect_err("the replay is refused");
    assert_eq!(refusal_code(&replay), "Replayed");
    assert_eq!(deletes_for(&rig.fake, "toy-box-t1"), 1, "still one DELETE");
}

#[test]
fn an_approval_from_a_key_the_signers_file_does_not_hold_is_refused_and_deletes_nothing() {
    let owner = SkKey::from_seed(1);
    let stranger = SkKey::from_seed(2);
    let rig = Rig::new("stranger", &[&owner]);
    rig.removed_toy("t1");
    let (digest, statement) = rig.first_plan();
    let approval = approve_statement(&stranger, &statement);

    let error = submit(&rig, &digest, &approval).expect_err("the stranger is refused");
    assert_eq!(refusal_code(&error), "BadSignature");
    assert_eq!(deletes_for(&rig.fake, "toy-box-t1"), 0);
    assert_eq!(
        instances_marked(&rig.fake, MARK, "toy-scratch"),
        vec!["toy-box-t1".to_string()],
        "the instance is still there"
    );
}

#[test]
fn with_no_owner_key_installed_every_approval_is_refused_no_verifier() {
    let rig = Rig::new("nokey", &[]);
    rig.removed_toy("t1");
    let (digest, statement) = rig.first_plan();
    let approval = approve_statement(&SkKey::from_seed(1), &statement);

    let error = submit(&rig, &digest, &approval).expect_err("no key is trusted");
    assert_eq!(refusal_code(&error), "NoVerifier");
    assert_eq!(deletes_for(&rig.fake, "toy-box-t1"), 0);
}

#[test]
fn an_approval_submitted_after_its_window_ended_is_refused_expired() {
    let owner = SkKey::from_seed(1);
    let rig = Rig::new("window", &[&owner]);
    rig.removed_toy("t1");
    let (digest, statement) = rig.first_plan();
    let approval = approve_statement(&owner, &statement);

    // The statement expires ten minutes after the listing. Submitting eleven minutes later is past its window, and the plan
    // itself is still within its fifteen minutes.
    rig.at(NOW + 11 * 60 * 1000);
    let error = submit(&rig, &digest, &approval).expect_err("the window has ended");
    assert_eq!(refusal_code(&error), "Expired");
    assert_eq!(deletes_for(&rig.fake, "toy-box-t1"), 0);
}

#[test]
fn a_plan_older_than_fifteen_minutes_is_refused_expired_even_with_a_fresh_approval() {
    let owner = SkKey::from_seed(1);
    let rig = Rig::new("planage", &[&owner]);
    rig.removed_toy("t1");
    let (digest, statement) = rig.first_plan();
    let approval = approve_statement(&owner, &statement);

    rig.at(NOW + 16 * 60 * 1000);
    let error = submit(&rig, &digest, &approval).expect_err("the plan is too old");
    assert_eq!(refusal_code(&error), "Expired");
    assert_eq!(deletes_for(&rig.fake, "toy-box-t1"), 0);
    assert_eq!(
        rig.plan_list()["plans"].as_array().map(Vec::len),
        Some(1),
        "the plan still waits"
    );
}

#[test]
fn an_approval_issued_more_than_five_minutes_ahead_of_the_clock_is_refused() {
    let owner = SkKey::from_seed(1);
    let rig = Rig::new("ahead", &[&owner]);
    rig.removed_toy("t1");
    let (digest, statement) = rig.first_plan();
    let approval = approve_statement(&owner, &statement);

    rig.at(NOW - 6 * 60 * 1000);
    let error = submit(&rig, &digest, &approval).expect_err("issued ahead");
    assert_eq!(refusal_code(&error), "IssuedAhead");
    assert_eq!(deletes_for(&rig.fake, "toy-box-t1"), 0);
}

#[test]
fn an_approval_for_another_waiting_plan_is_refused_as_the_wrong_target() {
    let owner = SkKey::from_seed(1);
    let rig = Rig::new("target", &[&owner]);
    rig.propose(
        vec![
            arca::desired_store::EntryChange::Put(toy_entry("t1")),
            arca::desired_store::EntryChange::Put(second_toy_entry("t2")),
        ],
        NOW,
    );
    rig.propose(
        vec![
            arca::desired_store::EntryChange::Remove(EntryId("toy-scratch".to_string())),
            arca::desired_store::EntryChange::Remove(EntryId("toy-second".to_string())),
        ],
        NOW + 1_000,
    );
    let list = rig.plan_list();
    let plans = list["plans"].as_array().expect("plans");
    assert_eq!(plans.len(), 2, "{list}");
    let (first_digest, first_statement) = (
        plans[0]["plan"].as_str().expect("digest").to_string(),
        plans[0]["statement"]
            .as_str()
            .expect("statement")
            .to_string(),
    );
    let second_digest = plans[1]["plan"].as_str().expect("digest").to_string();
    let approval = approve_statement(&owner, &first_statement);

    let error = submit(&rig, &second_digest, &approval).expect_err("another plan's approval");
    assert_eq!(refusal_code(&error), "WrongTarget");
    assert_eq!(rig.plan_list()["plans"].as_array().map(Vec::len), Some(2));
    let _ = first_digest;
}

#[test]
fn a_declined_plan_retains_its_instance_and_the_decline_survives_a_restart() {
    let owner = SkKey::from_seed(1);
    let rig = Rig::new("decline", &[&owner]);
    rig.removed_toy("t1");
    let (digest, statement) = rig.first_plan();

    let declined = call_verb(&rig.handlers, "plan_decline", json!({ "plan": digest }))
        .expect("the plan is declined");
    assert_eq!(declined["plan"], json!(digest));
    assert_eq!(
        without_uuid(&declined["retained"]),
        json!([instance_obj("t1")])
    );
    assert_eq!(rig.plan_list()["plans"], json!([]));

    // A declined plan's approval is refused: nothing waits, and no delete is sent.
    let approval = approve_statement(&owner, &statement);
    let late = submit(&rig, &digest, &approval).expect_err("nothing waits");
    assert_eq!(refusal_code(&late), "NoSuchPlan");
    assert_eq!(deletes_for(&rig.fake, "toy-box-t1"), 0);

    // The restart: the same state directory is opened again, and the decline is still in force.
    let Rig {
        scratch,
        fake,
        kernel: live,
        handlers,
        clock,
    } = rig;
    drop(handlers);
    drop(clock);
    drop(live);
    let restarted = kernel(
        &scratch,
        &fake,
        verifier_for(&scratch, &[&owner], "decline-restart"),
        NOW,
    )
    .expect("the kernel reopens");
    assert!(
        restarted.pending_plans().is_empty(),
        "the decline survives the restart"
    );
}

#[test]
fn a_pending_plan_is_listed_again_after_a_restart_because_nothing_was_retained() {
    let owner = SkKey::from_seed(1);
    let scratch = Scratch::new("pending-restart");
    let fake = start_fake(&scratch);
    {
        let mut live = kernel(
            &scratch,
            &fake,
            verifier_for(&scratch, &[&owner], "p1"),
            NOW,
        )
        .expect("opens");
        live.propose(
            vec![arca::desired_store::EntryChange::Put(toy_entry("t1"))],
            NOW,
        )
        .expect("applied");
        live.propose(
            vec![arca::desired_store::EntryChange::Remove(EntryId(
                "toy-scratch".to_string(),
            ))],
            NOW + 1_000,
        )
        .expect("removed");
        assert_eq!(live.pending_plans().len(), 1);
    }
    let restarted = kernel(
        &scratch,
        &fake,
        verifier_for(&scratch, &[&owner], "p2"),
        NOW,
    )
    .expect("reopens");
    assert_eq!(
        restarted.pending_plans().len(),
        1,
        "the waiting plan is listed again"
    );
}

#[test]
fn a_plan_decline_of_an_unknown_plan_is_refused_no_such_plan() {
    let rig = Rig::new("unknown", &[&SkKey::from_seed(1)]);
    let error = call_verb(
        &rig.handlers,
        "plan_decline",
        json!({ "plan": Digest::of(b"no plan").to_hex() }),
    )
    .expect_err("no plan has that digest");
    assert_eq!(refusal_code(&error), "NoSuchPlan");
}

#[test]
fn the_approval_string_is_never_in_a_result_or_in_any_file_the_kernel_writes() {
    let owner = SkKey::from_seed(1);
    let rig = Rig::new("secret", &[&owner]);
    rig.removed_toy("t1");
    let (digest, statement) = rig.first_plan();
    let approval = approve_statement(&owner, &statement);
    let result = submit(&rig, &digest, &approval).expect("the approval deletes");

    let signature_body = approval.split('.').nth(1).expect("the armoured part");
    assert!(!result.to_string().contains(&approval));
    assert!(!result.to_string().contains(signature_body));
    for name in ["desired.json", "journal.jsonl", "approval-nonces.jsonl"] {
        if let Ok(bytes) = std::fs::read(rig.scratch.path(name)) {
            let text = String::from_utf8_lossy(&bytes);
            assert!(!text.contains(&approval), "{name} holds the approval");
            assert!(!text.contains(signature_body), "{name} holds the signature");
        }
    }
}

#[test]
fn a_converged_entry_is_not_applied_again_after_a_restart() {
    let owner = SkKey::from_seed(1);
    let scratch = Scratch::new("converged");
    let fake = start_fake(&scratch);
    {
        let mut kernel_now = kernel(
            &scratch,
            &fake,
            verifier_for(&scratch, &[&owner], "c1"),
            NOW,
        )
        .expect("opens");
        kernel_now
            .propose(
                vec![arca::desired_store::EntryChange::Put(toy_entry("t1"))],
                NOW,
            )
            .expect("applied");
    }
    let writes_before = fake.writes().len();
    let restarted = kernel(
        &scratch,
        &fake,
        verifier_for(&scratch, &[&owner], "c2"),
        NOW,
    )
    .expect("reopens");
    assert!(restarted.pending_plans().is_empty());
    assert_eq!(
        fake.writes().len(),
        writes_before,
        "the restart sends no write for the converged entry"
    );
}
