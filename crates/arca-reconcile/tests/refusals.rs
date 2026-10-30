//! Every refusal the reconciler makes, each with its own test: the gate's refusals, the plugin and config refusals, the retained set,
//! the approval checks on a run, and a declined plan. Each runs on the fake Incus through the real provider and verifier.

mod common;

use std::collections::BTreeMap;

use arca_approval::{ApprovalRefusal, ApprovalVerifier, FileNonces, RefuseAll};
use arca_kernel::KernelError;
use arca_kernel::canon::Digest;
use arca_kernel::desired::{
    DesiredStateGate, DocRefusal, Document, EntryId, PluginId, SignedDocument, sign_document,
};
use arca_kernel::identity::NodeKey;
use arca_kernel::journal::{DeletePlan, FileJournal, Journal};
use arca_kernel::ownership::OwnerClass;
use arca_kernel::plugin::{EffectOp, EffectRefusal, Provider};
use arca_providers::incus::RetainedSet;
use arca_reconcile::toy::Toy;
use arca_reconcile::{Outcome, PluginSet, Reconciler};
use common::*;
use serde_json::json;

fn toy_entry_id() -> EntryId {
    EntryId("toy-scratch".to_string())
}

/// A reconciler over `plugins`, sharing `retained`. The common helper always adds the toy, so a refusal that needs another set
/// of plugins builds its own.
fn bare(
    scratch: &Scratch,
    fake: &arca_incus::FakeIncus,
    key: &NodeKey,
    retained: &RetainedSet,
    plugins: PluginSet,
) -> Reconciler<FileJournal> {
    let journal = Journal::open(
        FileJournal::open(&scratch.path("journal.jsonl")).expect("the journal opens"),
    )
    .expect("the journal folds");
    Reconciler::open(
        DesiredStateGate::new(key.public(), None),
        journal,
        plugins,
        Box::new(provider(scratch, fake, retained)),
        retained.clone(),
        NOW,
    )
    .expect("the journal has no unresolved intent")
}

/// Applies the toy with tag `t1`, then removes it. Returns the removal document's digest and the removal's delete plan.
fn removed(
    fake: &arca_incus::FakeIncus,
    key: &NodeKey,
    r: &mut Reconciler<FileJournal>,
) -> (Digest, DeletePlan) {
    let first = signed(key, 1, None, vec![toy_entry("t1")]);
    assert_eq!(
        r.reconcile(&first, NOW),
        vec![Outcome::Applied {
            entry: toy_entry_id()
        }]
    );
    assert_eq!(
        instances_marked(fake, MARK, "toy-scratch"),
        vec!["toy-box-t1".to_string()]
    );
    let second = signed(key, 2, Some(first.document.digest()), vec![]);
    let outcomes = r.reconcile(&second, NOW);
    let Outcome::AwaitingApproval { plan } = outcomes[1].clone() else {
        panic!("the instance waits for approval: {outcomes:?}");
    };
    (second.document.digest(), plan)
}

#[test]
fn a_document_for_another_node_is_refused_before_any_write() {
    let scratch = Scratch::new("foreign");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let mut r = reconciler(&scratch, &fake, &key, &retained);
    let other = node_key();
    let doc = sign_document(
        Document {
            node: other.id(),
            seq: 1,
            prev: None,
            issued_at: at(0),
            expires_at: at(60 * 60_000),
            entries: vec![toy_entry("t1")],
        },
        &key,
    );

    assert_eq!(
        r.reconcile(&doc, NOW),
        vec![Outcome::Refused(DocRefusal::ForeignNode)]
    );
    assert!(fake.writes().is_empty(), "nothing was written");
}

#[test]
fn a_forked_document_is_refused_and_nothing_moves() {
    let scratch = Scratch::new("forked");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let mut r = reconciler(&scratch, &fake, &key, &retained);
    r.reconcile(&signed(&key, 1, None, vec![toy_entry("t1")]), NOW);
    let writes = fake.writes().len();
    let live = r.journal().live_entries();

    let forked = signed(
        &key,
        2,
        Some(Digest::of(b"not the applied document")),
        vec![],
    );
    let outcomes = r.reconcile(&forked, NOW);

    assert!(
        matches!(
            outcomes.as_slice(),
            [Outcome::Refused(DocRefusal::Forked { .. })]
        ),
        "{outcomes:?}"
    );
    assert_eq!(fake.writes().len(), writes, "nothing was written");
    assert_eq!(r.journal().live_entries(), live, "the journal did not move");
}

#[test]
fn an_expired_document_holds_the_last_state_and_writes_nothing() {
    let scratch = Scratch::new("expired");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let mut r = reconciler(&scratch, &fake, &key, &retained);
    let expired = sign_document(
        Document {
            node: key.id(),
            seq: 1,
            prev: None,
            issued_at: at(-120 * 60_000),
            expires_at: at(-60 * 60_000),
            entries: vec![toy_entry("t1")],
        },
        &key,
    );

    assert_eq!(
        r.reconcile(&expired, NOW),
        vec![Outcome::Held { stale: true }]
    );
    assert!(
        fake.writes().is_empty(),
        "an expired document writes nothing"
    );
}

#[test]
fn an_entry_whose_plugin_is_not_compiled_in_fails_and_writes_nothing() {
    let scratch = Scratch::new("no-plugin");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let mut r = bare(&scratch, &fake, &key, &retained, PluginSet::new());

    let outcomes = r.reconcile(&signed(&key, 1, None, vec![toy_entry("t1")]), NOW);

    assert_eq!(
        outcomes,
        vec![Outcome::Failed {
            entry: toy_entry_id(),
            why: "plugin not known".to_string(),
        }]
    );
    assert!(fake.writes().is_empty(), "nothing was written");
}

#[test]
fn a_config_refusal_names_the_rule_and_never_the_value() {
    let scratch = Scratch::new("config");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let mut r = reconciler(&scratch, &fake, &key, &retained);
    // The tag is an integer, which the toy's config does not take. The value is distinctive so a leak is visible.
    let bad = entry("toy-scratch", "toy.scratch", json!({ "tag": 7123456 }));

    let outcomes = r.reconcile(&signed(&key, 1, None, vec![bad]), NOW);

    let [Outcome::Failed { why, .. }] = outcomes.as_slice() else {
        panic!("the config is refused: {outcomes:?}");
    };
    assert!(!why.is_empty(), "the refusal names a rule");
    assert!(
        !why.contains("7123456"),
        "the refusal never prints the value: {why}"
    );
    assert!(fake.writes().is_empty(), "nothing was written");
}

#[test]
fn a_retained_entry_is_not_adopted_when_it_comes_back() {
    let scratch = Scratch::new("adopt");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::new(BTreeMap::from([(
        toy_entry_id(),
        (
            PluginId("toy.scratch".to_string()),
            Digest::of(b"declined plan"),
        ),
    )]));
    let mut r = reconciler(&scratch, &fake, &key, &retained);

    let outcomes = r.reconcile(&signed(&key, 1, None, vec![toy_entry("t1")]), NOW);

    assert!(
        matches!(outcomes.as_slice(), [Outcome::Failed { entry, .. }] if entry == &toy_entry_id()),
        "a retained entry fails: {outcomes:?}"
    );
    assert!(
        fake.writes().is_empty(),
        "the retained entry's objects were not adopted"
    );
}

#[test]
fn a_suspended_entry_keeps_its_objects() {
    let scratch = Scratch::new("suspend");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let mut r = reconciler(&scratch, &fake, &key, &retained);
    let first = signed(&key, 1, None, vec![toy_entry("t1")]);
    r.reconcile(&first, NOW);
    let mut suspended = toy_entry("t1");
    suspended.disabled = true;

    let outcomes = r.reconcile(
        &signed(&key, 2, Some(first.document.digest()), vec![suspended]),
        NOW,
    );

    assert!(
        outcomes.is_empty(),
        "a suspended entry is neither applied nor removed: {outcomes:?}"
    );
    assert!(
        get(&fake, &acl_path("toy-acl-t1")).is_some(),
        "the ACL stays"
    );
    assert!(
        get(&fake, &instance_path("toy-box-t1")).is_some(),
        "the instance stays"
    );
    assert!(
        fake.writes().iter().all(|write| write.method != "DELETE"),
        "nothing was deleted"
    );
}

#[test]
fn a_plugin_is_added_once() {
    let mut plugins = PluginSet::new();
    plugins.add::<Toy>().expect("the first add is valid");

    assert!(matches!(plugins.add::<Toy>(), Err(KernelError::Plugin(_))));
}

#[test]
fn a_plan_older_than_fifteen_minutes_needs_a_new_approval() {
    let scratch = Scratch::new("ttl");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let mut r = reconciler(&scratch, &fake, &key, &retained);
    let (_, plan) = removed(&fake, &key, &mut r);
    let owner = SkKey::from_seed(1);
    let mut verifier = verifier_for(&scratch, &owner, "ttl");
    // The owner approved the plan at its own time, but the run comes sixteen minutes after the plan was made.
    let fresh = approval_window(
        &owner,
        &plan_target(&plan.digest),
        &plan_args(&plan.digest),
        &nonce(1),
        15 * 60_000,
        25 * 60_000,
    );

    assert_eq!(
        r.run_plan(&plan.digest, &fresh, &mut verifier, NOW + 16 * 60_000),
        Err(ApprovalRefusal::Expired)
    );
    assert!(
        get(&fake, &instance_path("toy-box-t1")).is_some(),
        "an expired plan deletes nothing"
    );
}

#[test]
fn an_approval_with_no_verifier_installed_is_refused_as_no_verifier() {
    let scratch = Scratch::new("no-verifier");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let mut r = reconciler(&scratch, &fake, &key, &retained);
    let (_, plan) = removed(&fake, &key, &mut r);
    let owner = SkKey::from_seed(1);
    let mut refuse_all = ApprovalVerifier::new(
        HOST.to_string(),
        RefuseAll,
        FileNonces::open(&scratch.path("nonces-none.jsonl")).expect("the nonce file opens"),
    );

    assert_eq!(
        r.run_plan(
            &plan.digest,
            &approval_for_plan(&owner, &plan.digest, 1),
            &mut refuse_all,
            NOW
        ),
        Err(ApprovalRefusal::NoVerifier)
    );
    assert_eq!(deletes_for(&fake, "toy-box-t1"), 0, "no delete was sent");
}

#[test]
fn an_approval_over_another_target_is_wrong_target() {
    let scratch = Scratch::new("target");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let mut r = reconciler(&scratch, &fake, &key, &retained);
    let (_, plan) = removed(&fake, &key, &mut r);
    let owner = SkKey::from_seed(1);
    let mut verifier = verifier_for(&scratch, &owner, "target");
    let other_target = Digest::of(b"another plan");
    let wrong = approval(
        &owner,
        &plan_target(&other_target),
        &plan_args(&plan.digest),
        &nonce(1),
    );

    assert_eq!(
        r.run_plan(&plan.digest, &wrong, &mut verifier, NOW),
        Err(ApprovalRefusal::WrongTarget)
    );
    assert!(get(&fake, &instance_path("toy-box-t1")).is_some());
}

#[test]
fn a_plan_that_is_not_pending_is_wrong_target() {
    let scratch = Scratch::new("not-pending");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let mut r = reconciler(&scratch, &fake, &key, &retained);
    removed(&fake, &key, &mut r);
    let owner = SkKey::from_seed(1);
    let mut verifier = verifier_for(&scratch, &owner, "not-pending");
    let unknown = Digest::of(b"no such plan");

    assert_eq!(
        r.run_plan(
            &unknown,
            &approval_for_plan(&owner, &unknown, 1),
            &mut verifier,
            NOW
        ),
        Err(ApprovalRefusal::WrongTarget)
    );
    assert_eq!(deletes_for(&fake, "toy-box-t1"), 0, "no delete was sent");
}

#[test]
fn a_declined_plan_retains_its_objects_and_cannot_run() {
    let scratch = Scratch::new("declined");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let mut r = reconciler(&scratch, &fake, &key, &retained);
    let (removal_digest, plan) = removed(&fake, &key, &mut r);
    let owner = SkKey::from_seed(1);
    let mut verifier = verifier_for(&scratch, &owner, "declined");

    let still = r.decline(&plan.digest);

    assert_eq!(still.len(), 1, "{still:?}");
    assert_eq!(still[0].name, "toy-box-t1");
    assert!(
        r.pending_plans().is_empty(),
        "the declined plan is no longer pending"
    );
    assert_eq!(
        r.run_plan(
            &plan.digest,
            &approval_for_plan(&owner, &plan.digest, 1),
            &mut verifier,
            NOW
        ),
        Err(ApprovalRefusal::WrongTarget),
        "a declined plan cannot run"
    );
    assert!(
        get(&fake, &instance_path("toy-box-t1")).is_some(),
        "the declined object stays"
    );
    assert!(
        retained
            .holds(&toy_entry_id())
            .expect("the set is readable"),
        "the entry is retained"
    );

    // A provider over the same set refuses a write to the declined object: it is classed Retained, which is not drift.
    let probe = provider(&scratch, &fake, &retained);
    let probed = probe.plan(
        &toy_entry_id(),
        &EffectOp::IncusPatchConfig {
            object: plan.objects[0].clone(),
            config: BTreeMap::from([("user.probe".to_string(), Some("x".to_string()))]),
        },
    );
    assert!(
        matches!(
            probed,
            Err(EffectRefusal::NotOwned(OwnerClass::Retained { .. }))
        ),
        "the object is retained: {probed:?}"
    );

    // A later document does not plan the declined entry's objects again.
    let later: SignedDocument = signed(&key, 3, Some(removal_digest), vec![]);
    assert!(
        r.reconcile(&later, NOW).is_empty(),
        "a retained entry is left alone"
    );
    assert!(r.pending_plans().is_empty());
}
