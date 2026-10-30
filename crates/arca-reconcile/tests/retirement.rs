//! What a retirement settles and what a refused delete leaves behind. A plugin here creates an ACL (class A) and an instance (class B)
//! and patches both. Runs on the fake Incus through the real provider and the real approval verifier.

mod common;

use std::collections::BTreeMap;

use arca_incus::{FakeIncus, WriteRecord};
use arca_kernel::desired::{DesiredStateGate, EntryId, PluginId};
use arca_kernel::journal::{EffectClass, FileJournal, Journal};
use arca_kernel::ownership::IncusKind;
use arca_kernel::plugin::{Ctx, EffectOp, Manifest, Plugin, PluginError};
use arca_providers::incus::RetainedSet;
use arca_reconcile::{Outcome, PlanRun, PluginSet, Reconciler};
use common::*;
use serde::Deserialize;
use serde_json::{Value, json};

const PATCHER: &str = "scratch.patcher";
const ENTRY: &str = "pe";

#[derive(Deserialize)]
struct PatchConfig {
    tag: String,
    value: String,
}

/// Creates an ACL and an instance named by `tag`, then sets `user.k` on the ACL (class A) and `user.env` on the instance (class B).
struct Patcher;

impl Plugin for Patcher {
    const ID: &'static str = PATCHER;
    type Config = PatchConfig;

    fn manifest() -> Manifest {
        Manifest {
            id: PluginId(PATCHER.to_string()),
            version: "0.1.0".to_string(),
            provides: Vec::new(),
            effects: vec![
                (IncusKind::Acl, EffectClass::A),
                (IncusKind::Instance, EffectClass::B),
            ],
            projects: vec![PROJECT.to_string(), ACL_HOME.to_string()],
            observes: false,
        }
    }

    fn apply(ctx: &mut Ctx<'_>, cfg: &PatchConfig) -> Result<(), PluginError> {
        let acl_name = format!("p-acl-{}", cfg.tag);
        let box_name = format!("p-box-{}", cfg.tag);
        let acl = ctx.effect(EffectOp::IncusCreate {
            kind: IncusKind::Acl,
            project: ACL_HOME.to_string(),
            parent: None,
            name: acl_name.clone(),
            body: json!({ "name": acl_name, "config": {} }),
        })?;
        let instance = ctx.effect(EffectOp::IncusCreate {
            kind: IncusKind::Instance,
            project: PROJECT.to_string(),
            parent: None,
            name: box_name.clone(),
            body: json!({ "name": box_name, "config": {} }),
        })?;
        ctx.effect(EffectOp::IncusPatchConfig {
            object: acl,
            config: BTreeMap::from([("user.k".to_string(), Some(cfg.value.clone()))]),
        })?;
        ctx.effect(EffectOp::IncusPatchConfig {
            object: instance,
            config: BTreeMap::from([("user.env".to_string(), Some(cfg.value.clone()))]),
        })?;
        Ok(())
    }
}

fn patcher_entry(value: &str) -> arca_kernel::desired::Entry {
    entry(ENTRY, PATCHER, json!({ "tag": "q", "value": value }))
}

fn patcher_reconciler(
    scratch: &Scratch,
    fake: &FakeIncus,
    key: &arca_kernel::identity::NodeKey,
    retained: &RetainedSet,
) -> Reconciler<FileJournal> {
    let journal = Journal::open(
        FileJournal::open(&scratch.path("journal.jsonl")).expect("the journal opens"),
    )
    .expect("the journal folds");
    let mut plugins = PluginSet::new();
    plugins
        .add::<Patcher>()
        .expect("the patcher plugin is valid");
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

/// Applies the patcher entry, then removes it. Returns the reconciler's plan for the instance.
fn applied_then_removed(
    key: &arca_kernel::identity::NodeKey,
    r: &mut Reconciler<FileJournal>,
) -> arca_kernel::journal::DeletePlan {
    let first = signed(key, 1, None, vec![patcher_entry("v1")]);
    assert_eq!(
        r.reconcile(&first, NOW),
        vec![Outcome::Applied {
            entry: EntryId(ENTRY.to_string())
        }]
    );
    let second = signed(key, 2, Some(first.document.digest()), vec![]);
    let outcomes = r.reconcile(&second, NOW);
    let Some(Outcome::AwaitingApproval { plan }) = outcomes.last().cloned() else {
        panic!("the instance waits for approval: {outcomes:?}");
    };
    plan
}

fn approve_and_run(
    scratch: &Scratch,
    key_tag: &str,
    r: &mut Reconciler<FileJournal>,
    plan: &arca_kernel::journal::DeletePlan,
) -> Result<PlanRun, arca_approval::ApprovalRefusal> {
    let owner = SkKey::from_seed(1);
    let mut verifier = verifier_for(scratch, &owner, key_tag);
    r.run_plan(
        &plan.digest,
        &approval_for_plan(&owner, &plan.digest, 3),
        &mut verifier,
        NOW,
    )
}

#[test]
fn a_full_removal_settles_the_patch_on_the_instance_it_deletes_so_nothing_stays_live() {
    let scratch = Scratch::new("settle");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let mut r = patcher_reconciler(&scratch, &fake, &key, &retained);
    let plan = applied_then_removed(&key, &mut r);

    let run =
        approve_and_run(&scratch, "settle", &mut r, &plan).expect("the owner approved the plan");

    assert_eq!(run.deleted.len(), 1, "{run:?}");
    assert!(
        get(&fake, &instance_path("p-box-q")).is_none(),
        "the instance is gone"
    );
    assert!(
        r.journal().live_entries().is_empty(),
        "the instance's patch went with it, so no effect of the entry stays live: {:?}",
        r.journal().live(&EntryId(ENTRY.to_string()))
    );
}

#[test]
fn a_class_a_patch_is_restored_before_the_acl_it_patched_is_deleted() {
    let scratch = Scratch::new("restore");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let mut r = patcher_reconciler(&scratch, &fake, &key, &retained);
    applied_then_removed(&key, &mut r);

    // The ACL is class A, so its removal runs at once: the patch is undone, then the ACL is deleted.
    assert!(
        get(&fake, &acl_path("p-acl-q")).is_none(),
        "the ACL is reverted at once"
    );
    let writes = fake.writes();
    let sets_key = |write: &WriteRecord| {
        write.method == "PATCH"
            && write.path.contains("p-acl-q")
            && write.config_keys == ["user.k".to_string()]
    };
    let patches: Vec<usize> = writes
        .iter()
        .enumerate()
        .filter(|(_, write)| sets_key(write))
        .map(|(index, _)| index)
        .collect();
    let delete = writes
        .iter()
        .position(|write| write.method == "DELETE" && write.path.contains("p-acl-q"))
        .expect("the ACL is deleted");
    assert_eq!(
        patches.len(),
        2,
        "the patch is sent, then its restore is sent: {writes:?}"
    );
    assert!(patches[1] < delete, "the restore comes before the delete");
    assert!(
        r.journal()
            .live(&EntryId(ENTRY.to_string()))
            .iter()
            .all(|record| record.class != EffectClass::A),
        "no class-A record of the entry stays live"
    );
}

#[test]
fn a_refused_delete_closes_its_intent_reports_drift_and_leaves_the_reconciler_unblocked() {
    let scratch = Scratch::new("refused");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let mut r = reconciler(&scratch, &fake, &key, &retained);
    let first = signed(&key, 1, None, vec![toy_entry("t1")]);
    r.reconcile(&first, NOW);
    // A running instance cannot be deleted: Incus fails the delete's operation, and the instance stays.
    let (status, _) = raw(
        &fake,
        "PATCH",
        &instance_path("toy-box-t1"),
        Some(&json!({ "status": "Running" })),
    );
    assert_eq!(status, 200, "the instance is running");
    let second = signed(&key, 2, Some(first.document.digest()), vec![]);
    let outcomes = r.reconcile(&second, NOW);
    let Some(Outcome::AwaitingApproval { plan }) = outcomes.last().cloned() else {
        panic!("the instance waits for approval: {outcomes:?}");
    };
    let owner = SkKey::from_seed(1);
    let mut verifier = verifier_for(&scratch, &owner, "refused");

    let run = r
        .run_plan(
            &plan.digest,
            &approval_for_plan(&owner, &plan.digest, 1),
            &mut verifier,
            NOW,
        )
        .expect("the owner approved the plan");

    assert!(run.deleted.is_empty(), "{run:?}");
    assert_eq!(
        run.drift.len(),
        1,
        "the refused delete is drift, and the object stays: {run:?}"
    );
    assert!(run.partial, "{run:?}");
    assert!(
        get(&fake, &instance_path("toy-box-t1")).is_some(),
        "the running instance stays"
    );
    assert!(
        r.journal().unresolved().is_empty(),
        "the refused delete's intent is closed, not left open: {:?}",
        r.journal().unresolved()
    );

    // The reconciler is not blocked: a new entry applies.
    let third = signed(
        &key,
        3,
        Some(second.document.digest()),
        vec![entry("toy-2", "toy.scratch", json!({ "tag": "t2" }))],
    );
    let after = r.reconcile(&third, NOW);
    assert!(
        after.iter().any(|outcome| matches!(
            outcome,
            Outcome::Applied { entry } if entry == &EntryId("toy-2".to_string())
        )),
        "a new entry applies after a refused delete: {after:?}"
    );
}

#[test]
fn a_delete_intent_is_marked_in_the_journal_and_a_create_is_not() {
    let scratch = Scratch::new("marker");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let mut r = reconciler(&scratch, &fake, &key, &retained);
    let first = signed(&key, 1, None, vec![toy_entry("t1")]);
    r.reconcile(&first, NOW);
    let second = signed(&key, 2, Some(first.document.digest()), vec![]);
    let outcomes = r.reconcile(&second, NOW);
    let Some(Outcome::AwaitingApproval { plan }) = outcomes.last().cloned() else {
        panic!("the instance waits for approval: {outcomes:?}");
    };
    let owner = SkKey::from_seed(1);
    let mut verifier = verifier_for(&scratch, &owner, "marker");
    r.run_plan(
        &plan.digest,
        &approval_for_plan(&owner, &plan.digest, 1),
        &mut verifier,
        NOW,
    )
    .expect("the owner approved the plan");

    let text = std::fs::read_to_string(scratch.path("journal.jsonl")).expect("the journal reads");
    let lines: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("each line is a record"))
        .collect();
    let deletes: Vec<&Value> = lines
        .iter()
        .filter(|line| line["intent"] == json!("Delete"))
        .collect();
    assert!(
        !deletes.is_empty(),
        "the retirement's records carry the delete marker"
    );
    // The class-A revert of the ACL is a retirement too, and it is marked. The instance's class-B delete is among the marked records.
    assert!(
        deletes.iter().any(
            |line| line["class"] == json!("B") && line["target"]["name"] == json!("toy-box-t1")
        ),
        "the instance's class-B delete is marked: {deletes:?}"
    );
    assert!(
        lines.iter().any(|line| line.get("intent").is_none()
            && line["class"] == json!("B")
            && line["target"]["name"] == json!("toy-box-t1")),
        "the instance's create carries no marker"
    );
}
