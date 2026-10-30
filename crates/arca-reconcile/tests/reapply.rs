//! An entry that is removed and then put back, and an apply that failed, are both applied again. Runs on the fake Incus through the
//! real provider.

mod common;

use arca_kernel::desired::EntryId;
use arca_providers::incus::RetainedSet;
use arca_reconcile::Outcome;
use common::*;
use serde_json::json;

const ENTRY: &str = "toy-scratch";

#[test]
fn a_removed_entry_that_comes_back_has_its_acl_applied_again_and_its_pending_plan_withdrawn() {
    let scratch = Scratch::new("readd");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let mut r = reconciler(&scratch, &fake, &key, &retained);
    let first = signed(&key, 1, None, vec![toy_entry("t1")]);
    assert_eq!(
        r.reconcile(&first, NOW),
        vec![Outcome::Applied {
            entry: EntryId(ENTRY.to_string())
        }]
    );
    let second = signed(&key, 2, Some(first.document.digest()), vec![]);
    r.reconcile(&second, NOW);
    assert_eq!(
        r.pending_plans().len(),
        1,
        "the instance waits for approval"
    );
    assert!(
        get(&fake, &acl_path("toy-acl-t1")).is_none(),
        "the ACL was reverted"
    );

    // The owner puts the same entry back before approving the plan.
    let third = signed(
        &key,
        3,
        Some(second.document.digest()),
        vec![toy_entry("t1")],
    );
    let outcomes = r.reconcile(&third, NOW);

    assert!(
        get(&fake, &acl_path("toy-acl-t1")).is_some(),
        "the ACL is applied again: {outcomes:?}"
    );
    assert!(
        r.pending_plans().is_empty(),
        "the entry is wanted again, so its pending plan is withdrawn"
    );
    assert!(
        get(&fake, &instance_path("toy-box-t1")).is_some(),
        "a re-add deletes nothing"
    );
    assert_eq!(
        deletes_for(&fake, "toy-box-t1"),
        0,
        "no delete was sent for the instance"
    );
    // The instance still exists under its name, so the create that follows it is refused and the entry fails closed.
    assert!(
        matches!(outcomes.as_slice(), [Outcome::Failed { entry, .. }] if entry == &EntryId(ENTRY.to_string())),
        "the re-add fails closed on the instance it cannot create again: {outcomes:?}"
    );
    assert!(
        r.journal().unresolved().is_empty(),
        "no intent is left open"
    );
}

#[test]
fn a_failed_apply_is_applied_again_on_the_next_reconcile_with_the_same_config() {
    let scratch = Scratch::new("retry");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let mut r = reconciler(&scratch, &fake, &key, &retained);
    // A name the kernel does not own is taken, so the first apply's ACL create is refused before anything is written.
    let (status, _) = raw(
        &fake,
        "POST",
        "/1.0/network-acls?project=arcak-scratch-toy",
        Some(&json!({ "name": "toy-acl-t1", "config": {} })),
    );
    assert_eq!(status, 201, "the foreign ACL is in place");

    let first = signed(&key, 1, None, vec![toy_entry("t1")]);
    let failed = r.reconcile(&first, NOW);
    assert!(
        matches!(failed.as_slice(), [Outcome::Failed { .. }]),
        "the first apply fails: {failed:?}"
    );

    // The foreign ACL goes, and the same config arrives again.
    let (status, _) = raw(&fake, "DELETE", &acl_path("toy-acl-t1"), None);
    assert_eq!(status, 200, "the foreign ACL is removed");
    let second = signed(
        &key,
        2,
        Some(first.document.digest()),
        vec![toy_entry("t1")],
    );
    assert_eq!(
        r.reconcile(&second, NOW),
        vec![Outcome::Applied {
            entry: EntryId(ENTRY.to_string())
        }],
        "a failed apply is not remembered as applied, so it runs again"
    );
    assert!(get(&fake, &acl_path("toy-acl-t1")).is_some());
    assert!(get(&fake, &instance_path("toy-box-t1")).is_some());
}
