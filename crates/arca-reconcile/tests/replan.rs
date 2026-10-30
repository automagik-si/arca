//! A delete plan older than its time limit is replaced by a plan made at the next reconcile, and only an approval given for that
//! plan deletes. Runs on the fake Incus through the real provider and the real approval verifier.

mod common;

use arca_approval::ApprovalRefusal;
use arca_providers::incus::RetainedSet;
use arca_reconcile::{Outcome, PLAN_TTL_MS};
use common::*;

/// Sixteen minutes after the plan was made: past the time limit, and inside the window of a fresh approval.
const LATER: u64 = NOW + PLAN_TTL_MS + 60_000;

#[test]
fn an_expired_plan_is_replaced_by_one_made_now_and_only_a_fresh_approval_deletes() {
    let scratch = Scratch::new("replan");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let mut r = reconciler(&scratch, &fake, &key, &retained);
    let first = signed(&key, 1, None, vec![toy_entry("t1")]);
    r.reconcile(&first, NOW);
    let second = signed(&key, 2, Some(first.document.digest()), vec![]);
    let removal = r.reconcile(&second, NOW);
    let Some(Outcome::AwaitingApproval { plan }) = removal.last().cloned() else {
        panic!("the instance waits for approval: {removal:?}");
    };
    let owner = SkKey::from_seed(1);
    let mut verifier = verifier_for(&scratch, &owner, "replan");
    // The owner approved the plan when it was made. That approval is the stale one.
    let stale = approval_for_plan(&owner, &plan.digest, 1);

    // A newer document arrives after the time limit. It still carries no toy, so the removal runs again.
    let third = signed(&key, 3, Some(second.document.digest()), vec![]);
    let again = r.reconcile(&third, LATER);
    let Some(Outcome::AwaitingApproval { plan: fresh_plan }) = again.last().cloned() else {
        panic!("the removal asks again: {again:?}");
    };
    assert_eq!(
        fresh_plan.digest, plan.digest,
        "the same objects give the same digest"
    );
    assert_eq!(
        fresh_plan.created_ms, LATER,
        "the expired plan is replaced by one made at this reconcile: {fresh_plan:?}"
    );

    assert_eq!(
        r.run_plan(&fresh_plan.digest, &stale, &mut verifier, LATER),
        Err(ApprovalRefusal::Expired),
        "the approval of the expired plan stays refused"
    );
    assert!(
        get(&fake, &instance_path("toy-box-t1")).is_some(),
        "a refused run deletes nothing"
    );

    let fresh = approval_window(
        &owner,
        &plan_target(&fresh_plan.digest),
        &plan_args(&fresh_plan.digest),
        &nonce(2),
        15 * 60_000,
        25 * 60_000,
    );
    let run = r
        .run_plan(&fresh_plan.digest, &fresh, &mut verifier, LATER)
        .expect("a fresh approval of the replaced plan runs");
    assert_eq!(run.deleted.len(), 1, "{run:?}");
    assert!(
        get(&fake, &instance_path("toy-box-t1")).is_none(),
        "the fresh approval deletes the instance"
    );
}
