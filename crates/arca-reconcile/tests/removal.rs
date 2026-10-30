//! The removal suite, run on the fake Incus through the real provider and the real owner-approval verifier. A toy entry makes one
//! ACL (class A) and one instance (class B) in a scratch project. Each case is its own test.

mod common;

use arca_kernel::KernelError;
use arca_kernel::canon::Digest;
use arca_kernel::desired::{DesiredStateGate, DocRefusal, EntryId};
use arca_kernel::journal::{DeletePlan, EffectClass, FileJournal, Journal, JournalSink};
use arca_kernel::ownership::{IncusKind, ObjectRef};
use arca_providers::incus::RetainedSet;
use arca_reconcile::{Outcome, PluginSet, Reconciler};
use common::*;

const ENTRY: &str = "toy-scratch";

fn entry_id() -> EntryId {
    EntryId(ENTRY.to_string())
}

/// Applies the toy with tag `t1`, then removes it. Returns the applied document's digest and the removal's delete plan.
fn applied_then_removed(
    fake: &arca_incus::FakeIncus,
    key: &arca_kernel::identity::NodeKey,
    r: &mut Reconciler<FileJournal>,
) -> (Digest, DeletePlan) {
    let first = signed(key, 1, None, vec![toy_entry("t1")]);
    let applied = r.reconcile(&first, NOW);
    assert_eq!(applied, vec![Outcome::Applied { entry: entry_id() }]);
    assert_eq!(
        instances_marked(fake, MARK, ENTRY),
        vec!["toy-box-t1".to_string()]
    );
    let prev = first.document.digest();
    let removal = r.reconcile(&signed(key, 2, Some(prev.clone()), vec![]), NOW);
    assert_eq!(removal.len(), 2, "{removal:?}");
    assert_eq!(
        removal[0],
        Outcome::Reverted {
            entry: entry_id(),
            effects: 1
        }
    );
    let Outcome::AwaitingApproval { plan } = removal[1].clone() else {
        panic!("the instance waits for approval: {removal:?}");
    };
    (prev, plan)
}

#[test]
fn removal_leaves_zero_objects_with_the_entry_mark_and_an_empty_journal() {
    let scratch = Scratch::new("zero");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let mut r = reconciler(&scratch, &fake, &key, &retained);
    let (_, plan) = applied_then_removed(&fake, &key, &mut r);
    let owner = SkKey::from_seed(1);
    let mut verifier = verifier_for(&scratch, &owner, "zero");

    let run = r
        .run_plan(
            &plan.digest,
            &approval_for_plan(&owner, &plan.digest, 1),
            &mut verifier,
            NOW,
        )
        .expect("the owner approved this plan");

    assert_eq!(run.deleted.len(), 1, "{run:?}");
    assert!(run.drift.is_empty() && !run.partial, "{run:?}");
    assert!(
        instances_marked(&fake, MARK, ENTRY).is_empty(),
        "no instance carries the entry mark"
    );
    assert!(
        get(&fake, &acl_path("toy-acl-t1")).is_none(),
        "the ACL was reverted"
    );
    assert!(
        get(&fake, &instance_path("toy-box-t1")).is_none(),
        "the instance was deleted"
    );
    assert!(
        r.journal().live_entries().is_empty(),
        "no effect of the entry still counts"
    );
    assert!(
        r.journal().unresolved().is_empty(),
        "no intent is left open"
    );
}

#[test]
fn droidd_and_foreign_config_hashes_are_unchanged_by_removal() {
    let scratch = Scratch::new("hashes");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let watched = [
        "/1.0/instances/db1?project=svc",
        "/1.0/instances/phone1?project=droid",
        "/1.0/instances/desk1?project=droid",
        "/1.0/instances/guest1?project=default",
        "/1.0/instances/x1?project=svc",
    ];
    let before: Vec<Option<String>> = watched
        .iter()
        .map(|path| config_hash(&fake, path))
        .collect();
    assert!(
        before.iter().all(Option::is_some),
        "every watched object is in the fixture"
    );
    let mut r = reconciler(&scratch, &fake, &key, &retained);
    let (_, plan) = applied_then_removed(&fake, &key, &mut r);
    let owner = SkKey::from_seed(1);
    let mut verifier = verifier_for(&scratch, &owner, "hashes");
    r.run_plan(
        &plan.digest,
        &approval_for_plan(&owner, &plan.digest, 1),
        &mut verifier,
        NOW,
    )
    .expect("the owner approved this plan");

    let after: Vec<Option<String>> = watched
        .iter()
        .map(|path| config_hash(&fake, path))
        .collect();
    assert_eq!(
        before, after,
        "droidd and foreign config hashes are unchanged"
    );
    // Every write the removal sent names the scratch project, so no request reached a droidd or foreign object.
    let writes = fake.writes();
    assert!(
        !writes.is_empty(),
        "the toy's creates and the removal's delete were sent"
    );
    assert!(
        writes
            .iter()
            .all(|write| write.path.contains(&format!("project={PROJECT}"))),
        "every write is in the scratch project: {writes:?}"
    );
}

#[test]
fn the_container_delete_waits_for_an_approval_over_the_plan_digest() {
    let scratch = Scratch::new("waits");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let mut r = reconciler(&scratch, &fake, &key, &retained);
    let (_, plan) = applied_then_removed(&fake, &key, &mut r);
    let owner = SkKey::from_seed(1);
    let mut verifier = verifier_for(&scratch, &owner, "waits");

    assert!(
        get(&fake, &instance_path("toy-box-t1")).is_some(),
        "the container exists before approval"
    );
    assert_eq!(
        deletes_for(&fake, "toy-box-t1"),
        0,
        "no delete was sent before approval"
    );

    // An approval over other arguments is refused, and so is one from a key the signers file does not list.
    let other = Digest::of(b"another plan");
    let wrong_params = approval(
        &owner,
        &plan_target(&plan.digest),
        &plan_args(&other),
        &nonce(2),
    );
    assert_eq!(
        r.run_plan(&plan.digest, &wrong_params, &mut verifier, NOW),
        Err(arca_approval::ApprovalRefusal::WrongParams)
    );
    let stranger = SkKey::from_seed(9);
    let untrusted = approval_for_plan(&stranger, &plan.digest, 3);
    assert_eq!(
        r.run_plan(&plan.digest, &untrusted, &mut verifier, NOW),
        Err(arca_approval::ApprovalRefusal::BadSignature)
    );
    assert!(
        get(&fake, &instance_path("toy-box-t1")).is_some(),
        "refused runs delete nothing"
    );
    assert_eq!(
        deletes_for(&fake, "toy-box-t1"),
        0,
        "refused runs send no delete"
    );

    let run = r
        .run_plan(
            &plan.digest,
            &approval_for_plan(&owner, &plan.digest, 1),
            &mut verifier,
            NOW,
        )
        .expect("the owner's approval over this digest is accepted");
    assert_eq!(run.deleted.len(), 1);
    assert!(get(&fake, &instance_path("toy-box-t1")).is_none());
}

#[test]
fn an_object_changed_between_plan_and_run_survives_is_drift_and_a_retry_without_approval_is_refused()
 {
    let scratch = Scratch::new("drift");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let mut r = reconciler(&scratch, &fake, &key, &retained);
    let (_, plan) = applied_then_removed(&fake, &key, &mut r);
    let owner = SkKey::from_seed(1);
    let mut verifier = verifier_for(&scratch, &owner, "drift");
    let approved = approval_for_plan(&owner, &plan.digest, 1);

    // The instance is replaced under the same name: its uuid changes between the plan and the run.
    let (status, _) = raw(
        &fake,
        "PATCH",
        &instance_path("toy-box-t1"),
        Some(
            &serde_json::json!({ "config": { "volatile.uuid": "00000000-0000-4000-8000-00000000d1ff" } }),
        ),
    );
    assert_eq!(status, 200, "the test changes the instance");

    let run = r
        .run_plan(&plan.digest, &approved, &mut verifier, NOW)
        .expect("the approval is valid");
    assert!(run.deleted.is_empty(), "{run:?}");
    assert_eq!(run.drift.len(), 1, "{run:?}");
    assert_eq!(run.drift[0].name, "toy-box-t1");
    assert!(run.partial, "{run:?}");
    assert!(
        get(&fake, &instance_path("toy-box-t1")).is_some(),
        "the changed object survives"
    );
    assert_eq!(
        deletes_for(&fake, "toy-box-t1"),
        0,
        "no delete reached the changed object"
    );

    // The same approval cannot be spent twice, and a run with no approval is refused.
    assert_eq!(
        r.run_plan(&plan.digest, &approved, &mut verifier, NOW),
        Err(arca_approval::ApprovalRefusal::Replayed),
        "a retry with the spent approval is refused"
    );
    assert!(matches!(
        r.run_plan(&plan.digest, "", &mut verifier, NOW),
        Err(arca_approval::ApprovalRefusal::Malformed(_))
    ));
}

#[test]
fn a_replayed_older_valid_document_is_refused_with_no_inverse_run() {
    let scratch = Scratch::new("replay");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let mut r = reconciler(&scratch, &fake, &key, &retained);
    applied_then_removed(&fake, &key, &mut r);
    // The first document, signed and valid, offered again after the removal was applied.
    let older = signed(&key, 1, None, vec![toy_entry("t1")]);
    let writes_before = fake.writes().len();
    let live_before = r.journal().live_entries();
    let pending_before = r.pending_plans();

    let outcomes = r.reconcile(&older, NOW);

    assert_eq!(
        outcomes,
        vec![Outcome::Refused(DocRefusal::Stale { applied: 2, got: 1 })]
    );
    assert_eq!(
        fake.writes().len(),
        writes_before,
        "no request was sent for the replay"
    );
    assert_eq!(
        r.journal().live_entries(),
        live_before,
        "the journal did not move"
    );
    assert_eq!(
        r.pending_plans(),
        pending_before,
        "the pending plan did not move"
    );
    assert!(
        get(&fake, &acl_path("toy-acl-t1")).is_none(),
        "the ACL stays reverted"
    );
    assert!(
        get(&fake, &instance_path("toy-box-t1")).is_some(),
        "the instance waits for approval"
    );
}

#[test]
fn open_on_a_journal_with_an_unresolved_intent_fails_unresolved_and_sends_no_request() {
    let scratch = Scratch::new("unresolved");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    {
        let mut journal = Journal::open(
            FileJournal::open(&scratch.path("journal.jsonl")).expect("the journal opens"),
        )
        .expect("the journal folds");
        let id = journal
            .intend(
                &entry_id(),
                EffectClass::A,
                Digest::of(b"an intent with no outcome"),
                ObjectRef {
                    kind: IncusKind::Acl,
                    project: PROJECT.to_string(),
                    parent: None,
                    name: "toy-acl-t9".to_string(),
                    uuid: None,
                },
                None,
                NOW,
            )
            .expect("the intent is journalled");
        assert_eq!(id, 1);
    }
    let journal =
        Journal::open(FileJournal::open(&scratch.path("journal.jsonl")).expect("reopens"))
            .expect("the journal folds");
    let mut plugins = PluginSet::new();
    plugins
        .add::<arca_reconcile::toy::Toy>()
        .expect("the toy plugin is valid");
    let refused = match Reconciler::open(
        DesiredStateGate::new(key.public(), None),
        journal,
        plugins,
        Box::new(provider(&scratch, &fake, &retained)),
        retained.clone(),
    ) {
        Ok(_) => panic!("a journal with an unresolved intent must not open"),
        Err(error) => error,
    };

    assert_eq!(refused, KernelError::Unresolved { ids: vec![1] });
    assert!(
        fake.requests().is_empty(),
        "no request was sent: {:?}",
        fake.requests()
    );
}
