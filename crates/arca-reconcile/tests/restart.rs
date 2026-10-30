//! Restart and resolve: the reconciler opens a journal that a killed process left behind. Each kill point is one test. A kill is
//! simulated by a provider that unwinds after its write has landed or before it starts, so the journal holds exactly the records
//! a killed process would have written. Every case runs on the fake Incus through the real provider.

mod common;

use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use arca_incus::{Appears, FakeIncus};
use arca_kernel::KernelError;
use arca_kernel::canon::Digest;
use arca_kernel::desired::{Applied, DesiredStateGate, EntryId, PluginId, SignedDocument};
use arca_kernel::identity::NodeKey;
use arca_kernel::journal::{
    DeletePlan, EffectClass, EffectState, FileJournal, Inverse, Journal, JournalSink, JournalStore,
};
use arca_kernel::ownership::{IncusKind, KERNEL_RETIRING_KEY, ObjectRef};
use arca_kernel::plugin::{
    Ctx, EffectOp, EffectRefusal, Manifest, Observe, Planned, Plugin, PluginError, Provider,
    ReadBack, Retirement, op_digest,
};
use arca_providers::incus::{IncusProvider, RetainedSet};
use arca_reconcile::toy::Toy;
use arca_reconcile::{Outcome, PlanRun, PluginSet, Reconciler};
use common::*;
use serde::Deserialize;
use serde_json::{Value, json};

const TOY: &str = "toy-scratch";
const PATCHER: &str = "restart.patcher";
const PE: &str = "pr";

fn toy_id() -> EntryId {
    EntryId(TOY.to_string())
}

fn pe_id() -> EntryId {
    EntryId(PE.to_string())
}

fn acl(name: &str) -> ObjectRef {
    ObjectRef {
        kind: IncusKind::Acl,
        project: ACL_HOME.to_string(),
        parent: None,
        name: name.to_string(),
        uuid: None,
    }
}

fn instance_object(name: &str, uuid: Option<&str>) -> ObjectRef {
    ObjectRef {
        kind: IncusKind::Instance,
        project: PROJECT.to_string(),
        parent: None,
        name: name.to_string(),
        uuid: uuid.map(str::to_string),
    }
}

/// Makes one ACL (class A) and one instance (class B), then patches each: four effects in that order. Its entry carries a tag and
/// the value the patches set.
#[derive(Deserialize)]
struct PatchConfig {
    tag: String,
    value: String,
}

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
        let acl_made = ctx.effect(EffectOp::IncusCreate {
            kind: IncusKind::Acl,
            project: ACL_HOME.to_string(),
            parent: None,
            name: acl_name.clone(),
            body: json!({ "name": acl_name, "config": {} }),
        })?;
        let box_made = ctx.effect(EffectOp::IncusCreate {
            kind: IncusKind::Instance,
            project: PROJECT.to_string(),
            parent: None,
            name: box_name.clone(),
            body: json!({ "name": box_name, "config": {} }),
        })?;
        ctx.effect(EffectOp::IncusPatchConfig {
            object: acl_made,
            config: BTreeMap::from([("user.k".to_string(), Some(cfg.value.clone()))]),
        })?;
        ctx.effect(EffectOp::IncusPatchConfig {
            object: box_made,
            config: BTreeMap::from([("user.env".to_string(), Some(cfg.value.clone()))]),
        })?;
        Ok(())
    }
}

fn patcher_entry(value: &str) -> arca_kernel::desired::Entry {
    entry(PE, PATCHER, json!({ "tag": "q", "value": value }))
}

/// Both plugins compiled in, as the suite's restarts use them.
fn plugins() -> PluginSet {
    let mut set = PluginSet::new();
    set.add::<Toy>().expect("the toy plugin is valid");
    set.add::<Patcher>().expect("the patcher plugin is valid");
    set
}

/// Where a simulated kill lands: on the nth call of `effect` or of `retire`, before or after the provider's own work.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kill {
    EffectBefore(usize),
    EffectAfter(usize),
    RetireBefore(usize),
    RetireAfter(usize),
}

/// A provider that answers for the real one and unwinds at its kill point, as a killed process would stop.
struct Killing {
    inner: IncusProvider,
    kill: Kill,
    effects: usize,
    retires: usize,
}

fn die() -> ! {
    resume_unwind(Box::new("a simulated kill"))
}

impl Provider for Killing {
    fn observe(
        &self,
        entry: &EntryId,
        projects: &[String],
        query: &Observe,
    ) -> Result<Value, EffectRefusal> {
        self.inner.observe(entry, projects, query)
    }

    fn plan(&self, entry: &EntryId, op: &EffectOp) -> Result<Planned, EffectRefusal> {
        self.inner.plan(entry, op)
    }

    fn effect(
        &mut self,
        entry: &EntryId,
        op: &EffectOp,
        planned: &Planned,
    ) -> Result<(ObjectRef, Inverse), EffectRefusal> {
        self.effects += 1;
        if self.kill == Kill::EffectBefore(self.effects) {
            die();
        }
        let done = Provider::effect(&mut self.inner, entry, op, planned);
        if self.kill == Kill::EffectAfter(self.effects) {
            die();
        }
        done
    }

    fn read_back(
        &self,
        entry: &EntryId,
        target: &ObjectRef,
        pre_image: Option<&BTreeMap<String, Option<String>>>,
    ) -> Result<ReadBack, EffectRefusal> {
        self.inner.read_back(entry, target, pre_image)
    }

    fn retire(
        &mut self,
        entry: &EntryId,
        object: &ObjectRef,
        plan: &Digest,
    ) -> Result<Retirement, EffectRefusal> {
        self.retires += 1;
        if self.kill == Kill::RetireBefore(self.retires) {
            die();
        }
        let done = Provider::retire(&mut self.inner, entry, object, plan);
        if self.kill == Kill::RetireAfter(self.retires) {
            die();
        }
        done
    }
}

fn plain(scratch: &Scratch, fake: &FakeIncus, retained: &RetainedSet) -> Box<dyn Provider + Send> {
    Box::new(provider(scratch, fake, retained))
}

fn killing(
    scratch: &Scratch,
    fake: &FakeIncus,
    retained: &RetainedSet,
    kill: Kill,
) -> Box<dyn Provider + Send> {
    Box::new(Killing {
        inner: provider(scratch, fake, retained),
        kill,
        effects: 0,
        retires: 0,
    })
}

/// Opens the reconciler the way a restarted process does: the journal is read from its file, and the gate continues from
/// `applied`, the last document it accepted.
fn restarted(
    scratch: &Scratch,
    key: &NodeKey,
    provider: Box<dyn Provider + Send>,
    plugins: PluginSet,
    retained: &RetainedSet,
    applied: Option<Applied>,
) -> Result<Reconciler<FileJournal>, KernelError> {
    let journal = Journal::open(
        FileJournal::open(&scratch.path("journal.jsonl")).expect("the journal opens"),
    )
    .expect("the journal folds");
    Reconciler::open(
        DesiredStateGate::new(key.public(), applied),
        journal,
        plugins,
        provider,
        retained.clone(),
        NOW,
    )
}

/// The gate's applied state after `doc` was accepted.
fn applied_of(doc: &SignedDocument) -> Option<Applied> {
    Some(Applied {
        seq: doc.document.seq,
        digest: doc.document.digest(),
    })
}

/// Runs `doc` on a reconciler that is killed at `kill`, then drops it, as the killed process would be gone. The outcomes never
/// reach anyone.
fn run_killed(
    scratch: &Scratch,
    fake: &FakeIncus,
    key: &NodeKey,
    retained: &RetainedSet,
    applied: Option<Applied>,
    kill: Kill,
    doc: &SignedDocument,
) {
    run_killed_by(
        scratch,
        key,
        retained,
        applied,
        killing(scratch, fake, retained, kill),
        doc,
    );
}

/// Runs `doc` on a reconciler over `provider`, which must unwind at its kill point, then drops it.
fn run_killed_by(
    scratch: &Scratch,
    key: &NodeKey,
    retained: &RetainedSet,
    applied: Option<Applied>,
    provider: Box<dyn Provider + Send>,
    doc: &SignedDocument,
) {
    let mut r = restarted(scratch, key, provider, plugins(), retained, applied)
        .expect("the journal opens before the kill");
    let ran = catch_unwind(AssertUnwindSafe(|| r.reconcile(doc, NOW)));
    assert!(ran.is_err(), "the simulated kill reached the reconciler");
    drop(r);
}

/// Runs the owner's approval of `plan` on `r`.
fn approve_and_run(
    r: &mut Reconciler<FileJournal>,
    scratch: &Scratch,
    plan: &DeletePlan,
    tag: &str,
) -> PlanRun {
    let owner = SkKey::from_seed(1);
    let mut verifier = verifier_for(scratch, &owner, tag);
    r.run_plan(
        &plan.digest,
        &approval_for_plan(&owner, &plan.digest, 1),
        &mut verifier,
        NOW,
    )
    .expect("the owner approved this plan")
}

/// Nothing the toy or the patcher made is left: no instance carries an entry mark, and no named object exists.
fn nothing_left(fake: &FakeIncus) {
    assert!(
        instances_marked(fake, MARK, TOY).is_empty() && instances_marked(fake, MARK, PE).is_empty(),
        "no instance carries an entry mark"
    );
    for name in ["toy-acl-t1", "p-acl-q"] {
        assert!(get(fake, &acl_path(name)).is_none(), "{name} was reverted");
    }
    for name in ["toy-box-t1", "p-box-q"] {
        assert!(
            get(fake, &instance_path(name)).is_none(),
            "{name} was deleted"
        );
    }
}

/// Journals `intent` by hand, as a write the process made before its kill, with no outcome.
fn intend_by_hand(
    scratch: &Scratch,
    entry: &EntryId,
    class: EffectClass,
    op: Digest,
    target: ObjectRef,
    pre_image: Option<BTreeMap<String, Option<String>>>,
) {
    let mut journal = Journal::open(
        FileJournal::open(&scratch.path("journal.jsonl")).expect("the journal opens"),
    )
    .expect("the journal folds");
    journal
        .intend(entry, class, op, target, pre_image, NOW)
        .expect("the intent is journalled");
}

/// Journals a retirement's intent by hand, as a kill before the provider's answer would leave it.
fn intend_delete_by_hand(
    scratch: &Scratch,
    entry: &EntryId,
    class: EffectClass,
    object: ObjectRef,
) {
    let mut journal = Journal::open(
        FileJournal::open(&scratch.path("journal.jsonl")).expect("the journal opens"),
    )
    .expect("the journal folds");
    let op = op_digest(&EffectOp::IncusDelete {
        object: object.clone(),
        plan: Digest::of(b"a plan"),
    });
    journal
        .intend_delete(entry, class, op, object, NOW)
        .expect("the delete intent is journalled");
}

// Kill points of a create.

#[test]
fn a_restart_after_a_create_whose_outcome_was_not_reported_still_reverts_everything() {
    let scratch = Scratch::new("reported");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let first = signed(&key, 1, None, vec![toy_entry("t1")]);
    {
        let mut r = reconciler(&scratch, &fake, &key, &retained);
        assert_eq!(
            r.reconcile(&first, NOW),
            vec![Outcome::Applied { entry: toy_id() }]
        );
    }
    // The process is gone before anyone reads the outcome. The restart finds both creates in the journal.
    let mut r = restarted(
        &scratch,
        &key,
        plain(&scratch, &fake, &retained),
        plugins(),
        &retained,
        applied_of(&first),
    )
    .expect("the journal opens");
    assert_eq!(
        r.resolution().reverted,
        Vec::<u64>::new(),
        "nothing was half done"
    );

    let second = signed(&key, 2, Some(first.document.digest()), vec![]);
    let removal = r.reconcile(&second, NOW);
    assert_eq!(
        removal[0],
        Outcome::Reverted {
            entry: toy_id(),
            effects: 1
        },
        "the ACL is reverted at once: {removal:?}"
    );
    let [_, Outcome::AwaitingApproval { plan }] = removal.as_slice() else {
        panic!("the instance waits for approval: {removal:?}");
    };
    let run = approve_and_run(&mut r, &scratch, plan, "reported");
    assert_eq!(run.deleted.len(), 1, "{run:?}");
    assert!(run.drift.is_empty() && !run.partial, "{run:?}");
    nothing_left(&fake);
    assert!(
        r.journal().live_entries().is_empty(),
        "no effect of the entry still counts"
    );
    assert!(r.journal().unresolved().is_empty());
}

#[test]
fn a_kill_after_a_create_landed_adopts_the_object_by_its_mark_and_the_removal_reverts_it() {
    let scratch = Scratch::new("adopt");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let first = signed(&key, 1, None, vec![toy_entry("t1")]);
    run_killed(
        &scratch,
        &fake,
        &key,
        &retained,
        None,
        Kill::EffectAfter(1),
        &first,
    );

    let mut r = restarted(
        &scratch,
        &key,
        plain(&scratch, &fake, &retained),
        plugins(),
        &retained,
        applied_of(&first),
    )
    .expect("the journal opens");
    assert_eq!(r.resolution().adopted, vec![acl("toy-acl-t1")]);
    assert!(r.resolution().reverted.is_empty() && r.resolution().drift.is_empty());

    let second = signed(&key, 2, Some(first.document.digest()), vec![]);
    assert_eq!(
        r.reconcile(&second, NOW),
        vec![Outcome::Reverted {
            entry: toy_id(),
            effects: 1
        }],
        "the adopted ACL is reverted; the instance was never made"
    );
    nothing_left(&fake);
    assert_eq!(
        deletes_for(&fake, "toy-acl-t1"),
        1,
        "one delete, for the adopted ACL"
    );
    assert!(r.journal().live_entries().is_empty());
    assert!(r.journal().unresolved().is_empty());
}

#[test]
fn a_kill_before_a_create_lands_reverts_its_intent_and_the_next_document_applies_the_entry_afresh()
{
    let scratch = Scratch::new("absent");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let first = signed(&key, 1, None, vec![toy_entry("t1")]);
    run_killed(
        &scratch,
        &fake,
        &key,
        &retained,
        None,
        Kill::EffectBefore(1),
        &first,
    );

    let mut r = restarted(
        &scratch,
        &key,
        plain(&scratch, &fake, &retained),
        plugins(),
        &retained,
        applied_of(&first),
    )
    .expect("the journal opens");
    assert_eq!(r.resolution().reverted, vec![1]);
    assert!(r.resolution().adopted.is_empty());
    assert!(
        get(&fake, &acl_path("toy-acl-t1")).is_none(),
        "nothing was made"
    );

    let second = signed(
        &key,
        2,
        Some(first.document.digest()),
        vec![toy_entry("t1")],
    );
    assert_eq!(
        r.reconcile(&second, NOW),
        vec![Outcome::Applied { entry: toy_id() }]
    );
    let third = signed(&key, 3, Some(second.document.digest()), vec![]);
    let removal = r.reconcile(&third, NOW);
    assert_eq!(
        removal[0],
        Outcome::Reverted {
            entry: toy_id(),
            effects: 1
        },
        "{removal:?}"
    );
    let [_, Outcome::AwaitingApproval { plan }] = removal.as_slice() else {
        panic!("the instance waits for approval: {removal:?}");
    };
    approve_and_run(&mut r, &scratch, plan, "absent");
    nothing_left(&fake);
    assert!(r.journal().live_entries().is_empty());
}

// Kill points of a patch.

#[test]
fn a_kill_after_a_patch_landed_holds_the_entry_with_drift_and_writes_nothing_until_the_next_document()
 {
    let scratch = Scratch::new("patched");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let first = signed(&key, 1, None, vec![patcher_entry("v1")]);
    run_killed(
        &scratch,
        &fake,
        &key,
        &retained,
        None,
        Kill::EffectAfter(3),
        &first,
    );
    let writes = fake.writes().len();

    let mut r = restarted(
        &scratch,
        &key,
        plain(&scratch, &fake, &retained),
        plugins(),
        &retained,
        applied_of(&first),
    )
    .expect("the journal opens");
    assert_eq!(fake.writes().len(), writes, "the resolution sent no write");
    assert_eq!(r.resolution().held, vec![pe_id()]);
    assert_eq!(r.resolution().drift, vec![acl("p-acl-q")]);
    assert!(r.resolution().adopted.is_empty() && r.resolution().reverted.is_empty());
    // The patch's inverse is its pre-image: the key was absent before the patch.
    let restore = Inverse::IncusRestoreConfig {
        object: acl("p-acl-q"),
        config: BTreeMap::from([("user.k".to_string(), None)]),
    };
    assert!(
        r.journal()
            .live(&pe_id())
            .iter()
            .any(|record| record.inverse.as_ref() == Some(&restore)),
        "the patch is settled with its pre-image as the inverse"
    );

    // The next accepted document applies the entry again. Its first create meets the ACL it made, and is refused before any write.
    let second = signed(
        &key,
        2,
        Some(first.document.digest()),
        vec![patcher_entry("v1")],
    );
    let outcomes = r.reconcile(&second, NOW);
    assert!(
        matches!(outcomes.as_slice(), [Outcome::Failed { entry, .. }] if entry == &pe_id()),
        "the re-apply fails closed on the ACL it cannot create again: {outcomes:?}"
    );
    assert_eq!(fake.writes().len(), writes, "the re-apply wrote nothing");

    let third = signed(&key, 3, Some(second.document.digest()), vec![]);
    let removal = r.reconcile(&third, NOW);
    assert_eq!(
        removal[0],
        Outcome::Reverted {
            entry: pe_id(),
            effects: 2
        },
        "the patch's restore and the ACL's delete: {removal:?}"
    );
    let [_, Outcome::AwaitingApproval { plan }] = removal.as_slice() else {
        panic!("the instance waits for approval: {removal:?}");
    };
    approve_and_run(&mut r, &scratch, plan, "patched");
    nothing_left(&fake);
    assert!(r.journal().live_entries().is_empty());
}

#[test]
fn a_kill_before_a_patch_lands_fails_its_intent_and_journals_no_inverse() {
    let scratch = Scratch::new("unpatched");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let first = signed(&key, 1, None, vec![patcher_entry("v1")]);
    run_killed(
        &scratch,
        &fake,
        &key,
        &retained,
        None,
        Kill::EffectBefore(3),
        &first,
    );

    let mut r = restarted(
        &scratch,
        &key,
        plain(&scratch, &fake, &retained),
        plugins(),
        &retained,
        applied_of(&first),
    )
    .expect("the journal opens");
    assert_eq!(r.resolution().held, vec![pe_id()]);
    assert!(
        r.resolution().drift.is_empty(),
        "the ACL shows its pre-image"
    );
    assert!(
        !r.journal()
            .live(&pe_id())
            .iter()
            .any(|record| matches!(record.inverse, Some(Inverse::IncusRestoreConfig { .. }))),
        "no patch inverse was journalled for a patch that did not land"
    );

    let second = signed(&key, 2, Some(first.document.digest()), vec![]);
    let removal = r.reconcile(&second, NOW);
    assert_eq!(
        removal[0],
        Outcome::Reverted {
            entry: pe_id(),
            effects: 1
        },
        "{removal:?}"
    );
    let [_, Outcome::AwaitingApproval { plan }] = removal.as_slice() else {
        panic!("the instance waits for approval: {removal:?}");
    };
    approve_and_run(&mut r, &scratch, plan, "unpatched");
    nothing_left(&fake);
}

#[test]
fn a_kill_inside_a_removal_during_a_patch_restore_is_settled_and_the_removal_completes() {
    let scratch = Scratch::new("restore");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let first = signed(&key, 1, None, vec![patcher_entry("v1")]);
    {
        let mut r = restarted(
            &scratch,
            &key,
            plain(&scratch, &fake, &retained),
            plugins(),
            &retained,
            None,
        )
        .expect("the journal opens");
        assert_eq!(
            r.reconcile(&first, NOW),
            vec![Outcome::Applied { entry: pe_id() }]
        );
    }
    // The removal restores the ACL's patch first. The restore lands, and the process dies before its outcome is journalled.
    let second = signed(&key, 2, Some(first.document.digest()), vec![]);
    run_killed(
        &scratch,
        &fake,
        &key,
        &retained,
        applied_of(&first),
        Kill::EffectAfter(1),
        &second,
    );

    let mut r = restarted(
        &scratch,
        &key,
        plain(&scratch, &fake, &retained),
        plugins(),
        &retained,
        applied_of(&second),
    )
    .expect("the journal opens");
    assert_eq!(r.resolution().held, vec![pe_id()]);
    assert_eq!(r.resolution().drift, vec![acl("p-acl-q")]);

    // The restore that died is settled with the value it replaced, so the next removal restores that value, then the patch's
    // own pre-image, then deletes the ACL. Each restore re-plans against the object as it is now.
    let third = signed(&key, 3, Some(second.document.digest()), vec![]);
    let removal = r.reconcile(&third, NOW);
    assert_eq!(
        removal[0],
        Outcome::Reverted {
            entry: pe_id(),
            effects: 3
        },
        "{removal:?}"
    );
    let [_, Outcome::AwaitingApproval { plan }] = removal.as_slice() else {
        panic!("the instance waits for approval: {removal:?}");
    };
    approve_and_run(&mut r, &scratch, plan, "restore");
    nothing_left(&fake);
    assert!(r.journal().live_entries().is_empty());
    assert!(r.journal().unresolved().is_empty());
}

// Kill points of a delete.

#[test]
fn a_kill_after_a_class_a_delete_landed_settles_its_intent_and_the_create_it_undid() {
    let scratch = Scratch::new("deleted");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let first = signed(&key, 1, None, vec![toy_entry("t1")]);
    {
        let mut r = restarted(
            &scratch,
            &key,
            plain(&scratch, &fake, &retained),
            plugins(),
            &retained,
            None,
        )
        .expect("the journal opens");
        r.reconcile(&first, NOW);
    }
    let second = signed(&key, 2, Some(first.document.digest()), vec![]);
    run_killed(
        &scratch,
        &fake,
        &key,
        &retained,
        applied_of(&first),
        Kill::RetireAfter(1),
        &second,
    );

    let mut r = restarted(
        &scratch,
        &key,
        plain(&scratch, &fake, &retained),
        plugins(),
        &retained,
        applied_of(&second),
    )
    .expect("the journal opens");
    assert_eq!(
        r.resolution().reverted,
        vec![3],
        "the delete intent is settled"
    );
    let third = signed(&key, 3, Some(second.document.digest()), vec![]);
    let removal = r.reconcile(&third, NOW);
    let [Outcome::AwaitingApproval { plan }] = removal.as_slice() else {
        panic!("only the instance is left to remove: {removal:?}");
    };
    approve_and_run(&mut r, &scratch, plan, "deleted");
    nothing_left(&fake);
    assert_eq!(
        deletes_for(&fake, "toy-acl-t1"),
        1,
        "the ACL was deleted once"
    );
    assert!(r.journal().live_entries().is_empty());
    drop(r);
    // The delete that landed is recorded as reverted, not as failed: its newest record says so.
    let (records, _) = FileJournal::open(&scratch.path("journal.jsonl"))
        .expect("the journal reopens")
        .load()
        .expect("the journal loads");
    let newest = records
        .iter()
        .rfind(|record| record.id == 3)
        .expect("the delete intent is journalled");
    assert_eq!(
        newest.state,
        EffectState::Reverted,
        "a delete that landed is settled as reverted: {newest:?}"
    );
}

#[test]
fn a_kill_before_a_class_a_delete_keeps_the_object_and_the_next_removal_reverts_it() {
    let scratch = Scratch::new("undeleted");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let first = signed(&key, 1, None, vec![toy_entry("t1")]);
    {
        let mut r = restarted(
            &scratch,
            &key,
            plain(&scratch, &fake, &retained),
            plugins(),
            &retained,
            None,
        )
        .expect("the journal opens");
        r.reconcile(&first, NOW);
    }
    let second = signed(&key, 2, Some(first.document.digest()), vec![]);
    run_killed(
        &scratch,
        &fake,
        &key,
        &retained,
        applied_of(&first),
        Kill::RetireBefore(1),
        &second,
    );

    let mut r = restarted(
        &scratch,
        &key,
        plain(&scratch, &fake, &retained),
        plugins(),
        &retained,
        applied_of(&second),
    )
    .expect("the journal opens: the delete intent was never sent, so it is not open");
    assert!(
        get(&fake, &acl_path("toy-acl-t1")).is_some(),
        "the ACL is still there"
    );
    assert!(r.journal().unresolved().is_empty());
    let third = signed(&key, 3, Some(second.document.digest()), vec![]);
    let removal = r.reconcile(&third, NOW);
    assert_eq!(
        removal[0],
        Outcome::Reverted {
            entry: toy_id(),
            effects: 1
        },
        "{removal:?}"
    );
    let [_, Outcome::AwaitingApproval { plan }] = removal.as_slice() else {
        panic!("the instance waits for approval: {removal:?}");
    };
    approve_and_run(&mut r, &scratch, plan, "undeleted");
    nothing_left(&fake);
    assert_eq!(deletes_for(&fake, "toy-acl-t1"), 1);
}

#[test]
fn a_kill_between_the_tombstone_and_the_delete_keeps_the_object_and_the_next_removal_retires_it() {
    let scratch = Scratch::new("tombstoned");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let first = signed(&key, 1, None, vec![toy_entry("t1")]);
    {
        let mut r = restarted(
            &scratch,
            &key,
            plain(&scratch, &fake, &retained),
            plugins(),
            &retained,
            None,
        )
        .expect("the journal opens");
        r.reconcile(&first, NOW);
    }
    // What a kill between the tombstone and the delete leaves: the ACL carries the tombstone, and its delete intent has no outcome.
    let create = {
        let journal = Journal::open(
            FileJournal::open(&scratch.path("journal.jsonl")).expect("the journal opens"),
        )
        .expect("the journal folds");
        journal
            .live(&toy_id())
            .into_iter()
            .find(|record| record.class == EffectClass::A)
            .expect("the ACL's create is live")
    };
    let tombstone = create.op_digest.clone();
    intend_delete_by_hand(&scratch, &toy_id(), EffectClass::A, acl("toy-acl-t1"));
    let (status, _) = raw(
        &fake,
        "PATCH",
        &acl_path("toy-acl-t1"),
        Some(&json!({ "config": { KERNEL_RETIRING_KEY: tombstone.to_hex() } })),
    );
    assert_eq!(status, 200, "the tombstone is set");

    let mut r = restarted(
        &scratch,
        &key,
        plain(&scratch, &fake, &retained),
        plugins(),
        &retained,
        applied_of(&first),
    )
    .expect("the journal opens");
    assert!(r.resolution().reverted.is_empty() && r.resolution().adopted.is_empty());
    let second = signed(&key, 2, Some(first.document.digest()), vec![]);
    let removal = r.reconcile(&second, NOW);
    assert_eq!(
        removal[0],
        Outcome::Reverted {
            entry: toy_id(),
            effects: 1
        },
        "{removal:?}"
    );
    let [_, Outcome::AwaitingApproval { plan }] = removal.as_slice() else {
        panic!("the instance waits for approval: {removal:?}");
    };
    approve_and_run(&mut r, &scratch, plan, "tombstoned");
    nothing_left(&fake);
    assert_eq!(deletes_for(&fake, "toy-acl-t1"), 1);
}

// Refusal paths.

#[test]
fn a_delete_intent_whose_object_shows_another_uuid_is_drift_and_no_delete_is_sent() {
    let scratch = Scratch::new("uuid");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let first = signed(&key, 1, None, vec![toy_entry("t1")]);
    {
        let mut r = restarted(
            &scratch,
            &key,
            plain(&scratch, &fake, &retained),
            plugins(),
            &retained,
            None,
        )
        .expect("the journal opens");
        r.reconcile(&first, NOW);
    }
    let real = get(&fake, &instance_path("toy-box-t1")).expect("the instance exists");
    let uuid = real["config"]["volatile.uuid"]
        .as_str()
        .expect("the instance has a uuid")
        .to_string();
    intend_delete_by_hand(
        &scratch,
        &toy_id(),
        EffectClass::B,
        instance_object("toy-box-t1", Some("u-other")),
    );

    let r = restarted(
        &scratch,
        &key,
        plain(&scratch, &fake, &retained),
        plugins(),
        &retained,
        applied_of(&first),
    )
    .expect("the journal opens");
    assert_eq!(
        r.resolution().drift,
        vec![instance_object("toy-box-t1", Some(&uuid))]
    );
    assert_eq!(deletes_for(&fake, "toy-box-t1"), 0, "no delete was sent");
    assert!(get(&fake, &instance_path("toy-box-t1")).is_some());
}

#[test]
fn a_delete_intent_over_another_entrys_object_is_drift_and_no_delete_is_sent() {
    let scratch = Scratch::new("other-owner");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let (status, _) = raw(
        &fake,
        "POST",
        "/1.0/network-acls?project=default",
        Some(
            &json!({ "name": "toy-acl-t1", "config": { "user.arcakernel.entry": "other-entry" } }),
        ),
    );
    assert_eq!(status, 201, "another entry's ACL is in place");
    intend_delete_by_hand(&scratch, &toy_id(), EffectClass::A, acl("toy-acl-t1"));

    let r = restarted(
        &scratch,
        &key,
        plain(&scratch, &fake, &retained),
        plugins(),
        &retained,
        None,
    )
    .expect("the journal opens");
    assert_eq!(r.resolution().drift, vec![acl("toy-acl-t1")]);
    assert_eq!(deletes_for(&fake, "toy-acl-t1"), 0, "no delete was sent");
    assert!(get(&fake, &acl_path("toy-acl-t1")).is_some());
}

#[test]
fn a_create_whose_name_holds_a_foreign_object_is_not_adopted_and_no_delete_is_sent() {
    let scratch = Scratch::new("foreign");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    intend_by_hand(
        &scratch,
        &toy_id(),
        EffectClass::A,
        Digest::of(b"a create"),
        acl("toy-acl-t1"),
        None,
    );
    let (status, _) = raw(
        &fake,
        "POST",
        "/1.0/network-acls?project=default",
        Some(&json!({ "name": "toy-acl-t1", "config": {} })),
    );
    assert_eq!(status, 201, "a foreign ACL is in place");
    let writes = fake.writes().len();

    let r = restarted(
        &scratch,
        &key,
        plain(&scratch, &fake, &retained),
        plugins(),
        &retained,
        None,
    )
    .expect("the journal opens");
    assert!(r.resolution().adopted.is_empty());
    assert_eq!(r.resolution().drift, vec![acl("toy-acl-t1")]);
    assert!(
        r.journal().live(&toy_id()).is_empty(),
        "the create's intent is settled as failed"
    );
    assert_eq!(fake.writes().len(), writes, "no write was sent");
    assert!(get(&fake, &acl_path("toy-acl-t1")).is_some());
}

#[test]
fn a_patch_whose_object_is_foreign_fails_without_an_inverse() {
    let scratch = Scratch::new("foreign-patch");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    intend_by_hand(
        &scratch,
        &toy_id(),
        EffectClass::A,
        Digest::of(b"a patch"),
        acl("toy-acl-t1"),
        Some(BTreeMap::from([("user.k".to_string(), None)])),
    );
    let (status, _) = raw(
        &fake,
        "POST",
        "/1.0/network-acls?project=default",
        Some(&json!({ "name": "toy-acl-t1", "config": {} })),
    );
    assert_eq!(status, 201, "a foreign ACL is in place");
    let writes = fake.writes().len();

    let r = restarted(
        &scratch,
        &key,
        plain(&scratch, &fake, &retained),
        plugins(),
        &retained,
        None,
    )
    .expect("the journal opens");
    assert_eq!(r.resolution().drift, vec![acl("toy-acl-t1")]);
    assert!(
        r.journal().live(&toy_id()).is_empty(),
        "no inverse was journalled for a foreign object"
    );
    assert_eq!(fake.writes().len(), writes, "no patch was sent");
}

#[test]
fn an_ambiguous_read_back_keeps_the_intent_open_and_open_refuses_with_no_write() {
    let scratch = Scratch::new("ambiguous");
    let key = node_key();
    let retained = RetainedSet::empty();
    intend_delete_by_hand(&scratch, &toy_id(), EffectClass::A, acl("toy-acl-t1"));
    // No Incus answers on this socket, so the read cannot be answered at all.
    let unreachable = IncusProvider::new(
        scratch.path("missing.sock"),
        family(),
        scratch.path("secrets"),
        retained.clone(),
    )
    .expect("the family is valid");

    let refused = match restarted(
        &scratch,
        &key,
        Box::new(unreachable),
        plugins(),
        &retained,
        None,
    ) {
        Ok(_) => panic!("an intent whose read failed must not open"),
        Err(error) => error,
    };
    assert_eq!(refused, KernelError::Unresolved { ids: vec![1] });
    let journal = Journal::open(
        FileJournal::open(&scratch.path("journal.jsonl")).expect("the journal opens"),
    )
    .expect("the journal folds");
    assert_eq!(journal.unresolved().len(), 1, "the intent is still open");
}

#[test]
fn a_target_outside_the_family_is_not_read_and_its_intent_stays_open() {
    let scratch = Scratch::new("family");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let outside = ObjectRef {
        kind: IncusKind::Acl,
        project: "svc".to_string(),
        parent: None,
        name: "x1".to_string(),
        uuid: None,
    };
    intend_by_hand(
        &scratch,
        &toy_id(),
        EffectClass::A,
        Digest::of(b"outside"),
        outside,
        None,
    );

    let refused = match restarted(
        &scratch,
        &key,
        plain(&scratch, &fake, &retained),
        plugins(),
        &retained,
        None,
    ) {
        Ok(_) => panic!("a target outside the family must not settle"),
        Err(error) => error,
    };
    assert_eq!(refused, KernelError::Unresolved { ids: vec![1] });
    assert!(
        fake.requests().is_empty(),
        "the family is checked before any request"
    );
}

#[test]
fn a_class_b_plan_after_a_restart_with_the_plugin_missing_fails_closed_and_deletes_nothing() {
    let scratch = Scratch::new("no-plugin");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let first = signed(&key, 1, None, vec![toy_entry("t1")]);
    {
        let mut r = restarted(
            &scratch,
            &key,
            plain(&scratch, &fake, &retained),
            plugins(),
            &retained,
            None,
        )
        .expect("the journal opens");
        r.reconcile(&first, NOW);
    }
    let mut r = restarted(
        &scratch,
        &key,
        plain(&scratch, &fake, &retained),
        PluginSet::new(),
        &retained,
        applied_of(&first),
    )
    .expect("the journal opens");
    let second = signed(&key, 2, Some(first.document.digest()), vec![]);
    assert_eq!(
        r.reconcile(&second, NOW),
        vec![
            Outcome::Reverted {
                entry: toy_id(),
                effects: 1
            },
            Outcome::Failed {
                entry: toy_id(),
                why: "plugin not known".to_string()
            },
        ]
    );
    assert_eq!(
        deletes_for(&fake, "toy-box-t1"),
        0,
        "the instance was not deleted"
    );
    assert!(get(&fake, &instance_path("toy-box-t1")).is_some());
}

// The resolution as a whole.

#[test]
fn resolving_again_settles_nothing_and_sends_nothing() {
    let scratch = Scratch::new("again");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    intend_by_hand(
        &scratch,
        &toy_id(),
        EffectClass::A,
        Digest::of(b"a create"),
        acl("toy-acl-t1"),
        None,
    );
    let mut r = restarted(
        &scratch,
        &key,
        plain(&scratch, &fake, &retained),
        plugins(),
        &retained,
        None,
    )
    .expect("the journal opens");
    assert_eq!(r.resolution().reverted, vec![1]);
    let writes = fake.writes().len();

    let again = r.resolve(NOW + 1).expect("the second resolution settles");
    assert!(
        again.adopted.is_empty()
            && again.reverted.is_empty()
            && again.held.is_empty()
            && again.drift.is_empty(),
        "nothing is left to settle: {again:?}"
    );
    assert_eq!(fake.writes().len(), writes);
}

#[test]
fn no_effect_is_sent_before_the_resolution_returns() {
    let scratch = Scratch::new("reads-only");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let (status, _) = raw(
        &fake,
        "POST",
        "/1.0/network-acls?project=default",
        Some(&json!({ "name": "toy-acl-t2", "config": { "user.arcakernel.entry": TOY } })),
    );
    assert_eq!(status, 201);
    let (status, _) = raw(
        &fake,
        "POST",
        "/1.0/network-acls?project=default",
        Some(
            &json!({ "name": "toy-acl-t3", "config": { "user.arcakernel.entry": TOY, "user.k": "x" } }),
        ),
    );
    assert_eq!(status, 201);
    intend_by_hand(
        &scratch,
        &toy_id(),
        EffectClass::A,
        Digest::of(b"absent"),
        acl("toy-acl-t1"),
        None,
    );
    intend_by_hand(
        &scratch,
        &toy_id(),
        EffectClass::A,
        Digest::of(b"present"),
        acl("toy-acl-t2"),
        None,
    );
    intend_by_hand(
        &scratch,
        &toy_id(),
        EffectClass::A,
        Digest::of(b"patch"),
        acl("toy-acl-t3"),
        Some(BTreeMap::from([("user.k".to_string(), None)])),
    );
    let writes = fake.writes().len();
    let requests = fake.requests().len();

    let r = restarted(
        &scratch,
        &key,
        plain(&scratch, &fake, &retained),
        plugins(),
        &retained,
        None,
    )
    .expect("the journal opens");
    assert_eq!(fake.writes().len(), writes, "no write was sent");
    assert!(
        fake.requests()[requests..]
            .iter()
            .all(|request| request.method == "GET"),
        "every request was a read"
    );
    assert_eq!(r.resolution().reverted, vec![1]);
    assert_eq!(r.resolution().adopted, vec![acl("toy-acl-t2")]);
    assert_eq!(r.resolution().held, vec![toy_id()]);
    assert_eq!(r.resolution().drift, vec![acl("toy-acl-t3")]);
}

#[test]
fn a_provider_with_no_read_back_keeps_every_intent_open() {
    let scratch = Scratch::new("no-read-back");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    intend_by_hand(
        &scratch,
        &toy_id(),
        EffectClass::A,
        Digest::of(b"create"),
        acl("toy-acl-t1"),
        None,
    );
    let refused = match Reconciler::open(
        DesiredStateGate::new(key.public(), None),
        Journal::open(FileJournal::open(&scratch.path("journal.jsonl")).expect("opens"))
            .expect("folds"),
        plugins(),
        Box::new(Silent),
        retained.clone(),
        NOW,
    ) {
        Ok(_) => panic!("a provider that cannot read back must not settle an intent"),
        Err(error) => error,
    };
    assert_eq!(refused, KernelError::Unresolved { ids: vec![1] });
    assert!(fake.requests().is_empty());
}

/// A provider that keeps the trait's default read-back, which refuses.
struct Silent;

impl Provider for Silent {
    fn observe(&self, _: &EntryId, _: &[String], _: &Observe) -> Result<Value, EffectRefusal> {
        Err(EffectRefusal::NotDeclared)
    }

    fn plan(&self, _: &EntryId, _: &EffectOp) -> Result<Planned, EffectRefusal> {
        Err(EffectRefusal::NotDeclared)
    }

    fn effect(
        &mut self,
        _: &EntryId,
        _: &EffectOp,
        _: &Planned,
    ) -> Result<(ObjectRef, Inverse), EffectRefusal> {
        Err(EffectRefusal::NotDeclared)
    }
}

// Kill points between a delete's outcome and the settlement of the create it undid.

/// Leaves the state a kill between the delete intent's `reverted` append and `settle_deleted`'s append leaves: the delete landed,
/// its intent is settled, and the create it undid is still live.
fn half_settled_delete(scratch: &Scratch, entry: &EntryId, class: EffectClass, object: ObjectRef) {
    let mut journal = Journal::open(
        FileJournal::open(&scratch.path("journal.jsonl")).expect("the journal opens"),
    )
    .expect("the journal folds");
    let op = op_digest(&EffectOp::IncusDelete {
        object: object.clone(),
        plan: Digest::of(b"a plan"),
    });
    let id = journal
        .intend_delete(entry, class, op, object, NOW)
        .expect("the delete intent is journalled");
    journal.reverted(id);
    assert!(journal.poisoned().is_none());
}

/// The delete plans an outcome list asks the owner to approve.
fn asked(outcomes: &[Outcome]) -> Vec<DeletePlan> {
    outcomes
        .iter()
        .filter_map(|outcome| match outcome {
            Outcome::AwaitingApproval { plan } => Some(plan.clone()),
            _ => None,
        })
        .collect()
}

/// The names of the objects a plan asks about.
fn names(objects: &[ObjectRef]) -> Vec<&str> {
    objects.iter().map(|object| object.name.as_str()).collect()
}

/// The `volatile.uuid` the fake gave the toy instance.
fn toy_instance_uuid(fake: &FakeIncus) -> String {
    let real = get(fake, &instance_path("toy-box-t1")).expect("the instance exists");
    real["config"]["volatile.uuid"]
        .as_str()
        .expect("the instance has a uuid")
        .to_string()
}

#[test]
fn a_kill_between_a_class_a_delete_settled_and_its_create_settled_does_not_wedge_the_removal() {
    let scratch = Scratch::new("atk-a");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let first = signed(&key, 1, None, vec![toy_entry("t1")]);
    {
        let mut r = restarted(
            &scratch,
            &key,
            plain(&scratch, &fake, &retained),
            plugins(),
            &retained,
            None,
        )
        .expect("opens");
        assert_eq!(
            r.reconcile(&first, NOW),
            vec![Outcome::Applied { entry: toy_id() }]
        );
    }
    // The ACL's class-A delete landed, its intent was settled, and the process died before the create was settled.
    half_settled_delete(&scratch, &toy_id(), EffectClass::A, acl("toy-acl-t1"));
    let (status, _) = raw(&fake, "DELETE", &acl_path("toy-acl-t1"), None);
    assert!(status < 300, "the ACL is gone: {status}");

    let mut r = restarted(
        &scratch,
        &key,
        plain(&scratch, &fake, &retained),
        plugins(),
        &retained,
        applied_of(&first),
    )
    .expect("opens");
    let second = signed(&key, 2, Some(first.document.digest()), vec![]);
    let removal = r.reconcile(&second, NOW);
    assert!(
        removal.contains(&Outcome::Reverted {
            entry: toy_id(),
            effects: 1
        }),
        "the gone ACL's create is settled without a delete: {removal:?}"
    );
    let plans = asked(&removal);
    assert_eq!(
        plans.len(),
        1,
        "only the instance waits for the owner: {removal:?}"
    );
    assert_eq!(names(&plans[0].objects), vec!["toy-box-t1"]);

    let run = approve_and_run(&mut r, &scratch, &plans[0], "atk-a");
    assert!(
        !run.partial && run.drift.is_empty(),
        "the approved run completes: {run:?}"
    );
    let third = signed(&key, 3, Some(second.document.digest()), vec![]);
    let again = r.reconcile(&third, NOW);
    assert!(again.is_empty(), "nothing is asked again: {again:?}");
    assert!(r.pending_plans().is_empty(), "no plan is pending");
    assert!(
        r.journal().live_entries().is_empty(),
        "the live journal empties after the removal"
    );
    nothing_left(&fake);
    assert_eq!(
        deletes_for(&fake, "toy-acl-t1"),
        1,
        "the reconciler sent no delete for the gone ACL"
    );
}

#[test]
fn a_kill_between_a_class_b_delete_settled_and_its_create_settled_asks_nothing_about_the_gone_instance()
 {
    let scratch = Scratch::new("atk-b");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let first = signed(&key, 1, None, vec![toy_entry("t1")]);
    let second = signed(&key, 2, Some(first.document.digest()), vec![]);
    let uuid;
    {
        let mut r = restarted(
            &scratch,
            &key,
            plain(&scratch, &fake, &retained),
            plugins(),
            &retained,
            None,
        )
        .expect("opens");
        r.reconcile(&first, NOW);
        let removal = r.reconcile(&second, NOW);
        assert_eq!(
            asked(&removal).len(),
            1,
            "the instance waits for the owner: {removal:?}"
        );
        uuid = toy_instance_uuid(&fake);
    }
    // The approved class-B delete landed, its intent was settled, and the process died before the create was settled.
    half_settled_delete(
        &scratch,
        &toy_id(),
        EffectClass::B,
        instance_object("toy-box-t1", Some(&uuid)),
    );
    let (status, _) = raw(&fake, "DELETE", &instance_path("toy-box-t1"), None);
    assert!(status < 300, "the instance is gone: {status}");

    let mut r = restarted(
        &scratch,
        &key,
        plain(&scratch, &fake, &retained),
        plugins(),
        &retained,
        applied_of(&second),
    )
    .expect("opens");
    let third = signed(&key, 3, Some(second.document.digest()), vec![]);
    let removal = r.reconcile(&third, NOW);
    assert!(
        asked(&removal).is_empty(),
        "nothing is asked of the owner for a gone object: {removal:?}"
    );
    assert!(r.pending_plans().is_empty(), "no plan is pending");
    assert!(
        r.journal().live_entries().is_empty(),
        "the live journal empties after the removal"
    );
    nothing_left(&fake);
    assert_eq!(
        deletes_for(&fake, "toy-box-t1"),
        1,
        "the reconciler sent no delete for the gone instance"
    );
}

#[test]
fn an_async_class_b_delete_that_completes_after_its_read_back_kept_the_object_settles_on_retry() {
    let scratch = Scratch::new("atk-async");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let first = signed(&key, 1, None, vec![toy_entry("t1")]);
    {
        let mut r = restarted(
            &scratch,
            &key,
            plain(&scratch, &fake, &retained),
            plugins(),
            &retained,
            None,
        )
        .expect("opens");
        assert_eq!(
            r.reconcile(&first, NOW),
            vec![Outcome::Applied { entry: toy_id() }]
        );
    }
    let uuid = toy_instance_uuid(&fake);
    // The approved delete was sent, and the process died before its outcome was journalled.
    intend_delete_by_hand(
        &scratch,
        &toy_id(),
        EffectClass::B,
        instance_object("toy-box-t1", Some(&uuid)),
    );
    // The restart reads the instance back, finds it still there, and fails the delete intent as kept.
    let mut r = restarted(
        &scratch,
        &key,
        plain(&scratch, &fake, &retained),
        plugins(),
        &retained,
        applied_of(&first),
    )
    .expect("opens");
    assert!(r.journal().unresolved().is_empty(), "the intent is settled");
    assert!(
        get(&fake, &instance_path("toy-box-t1")).is_some(),
        "the read-back saw the instance"
    );
    // The async delete completes after the read-back.
    let (status, _) = raw(&fake, "DELETE", &instance_path("toy-box-t1"), None);
    assert!(status < 300, "the instance is gone: {status}");

    // The retry removes the entry. The instance is gone, so the owner is not asked about it.
    let second = signed(&key, 2, Some(first.document.digest()), vec![]);
    let removal = r.reconcile(&second, NOW);
    assert!(
        asked(&removal).is_empty(),
        "nothing is asked of the owner for a gone object: {removal:?}"
    );
    assert!(r.pending_plans().is_empty(), "no plan is pending");
    assert!(
        r.journal().live_entries().is_empty(),
        "the live journal empties after the removal"
    );
    nothing_left(&fake);
    assert_eq!(
        deletes_for(&fake, "toy-box-t1"),
        1,
        "the reconciler sent no delete for the gone instance"
    );
}

#[test]
fn an_instance_deleted_after_the_owner_was_asked_is_settled_by_the_approved_run() {
    let scratch = Scratch::new("atk-run");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let first = signed(&key, 1, None, vec![toy_entry("t1")]);
    let second = signed(&key, 2, Some(first.document.digest()), vec![]);
    let mut r = restarted(
        &scratch,
        &key,
        plain(&scratch, &fake, &retained),
        plugins(),
        &retained,
        None,
    )
    .expect("opens");
    r.reconcile(&first, NOW);
    let removal = r.reconcile(&second, NOW);
    let plans = asked(&removal);
    assert_eq!(
        plans.len(),
        1,
        "the instance waits for the owner: {removal:?}"
    );
    // The delete completes before the owner approves, so the approved run finds the instance gone.
    let (status, _) = raw(&fake, "DELETE", &instance_path("toy-box-t1"), None);
    assert!(status < 300, "the instance is gone: {status}");

    let run = approve_and_run(&mut r, &scratch, &plans[0], "atk-run");
    assert!(
        !run.partial && run.drift.is_empty(),
        "the run settles the gone instance and completes: {run:?}"
    );
    let third = signed(&key, 3, Some(second.document.digest()), vec![]);
    assert!(r.reconcile(&third, NOW).is_empty());
    assert!(r.pending_plans().is_empty(), "no plan is pending");
    assert!(r.journal().live_entries().is_empty());
    nothing_left(&fake);
    assert_eq!(deletes_for(&fake, "toy-box-t1"), 1);
}

// A patch whose object is another object.

#[test]
fn a_patch_whose_object_shows_another_uuid_is_failed_with_no_inverse_and_no_write_is_sent() {
    let scratch = Scratch::new("patch-uuid");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let first = signed(&key, 1, None, vec![toy_entry("t1")]);
    {
        let mut r = restarted(
            &scratch,
            &key,
            plain(&scratch, &fake, &retained),
            plugins(),
            &retained,
            None,
        )
        .expect("opens");
        r.reconcile(&first, NOW);
    }
    let uuid = toy_instance_uuid(&fake);
    // The intent names an instance with another uuid at the same place. Its pre-image is not held, so a settlement that ignored
    // the uuid would call the patch landed and journal an inverse.
    intend_by_hand(
        &scratch,
        &toy_id(),
        EffectClass::B,
        Digest::of(b"a patch"),
        instance_object("toy-box-t1", Some("u-other")),
        Some(BTreeMap::from([(
            "user.env".to_string(),
            Some("before".to_string()),
        )])),
    );
    let writes = fake.writes().len();

    let r = restarted(
        &scratch,
        &key,
        plain(&scratch, &fake, &retained),
        plugins(),
        &retained,
        applied_of(&first),
    )
    .expect("the journal opens");
    assert_eq!(
        r.resolution().drift,
        vec![instance_object("toy-box-t1", Some(&uuid))],
        "the object that is there is reported"
    );
    assert_eq!(r.resolution().held, vec![toy_id()]);
    assert!(
        r.journal()
            .live(&toy_id())
            .iter()
            .all(|record| record.target.uuid.as_deref() != Some("u-other")),
        "the patch is failed, so no record of it stays live"
    );
    assert!(r.journal().unresolved().is_empty());
    assert_eq!(fake.writes().len(), writes, "no patch or delete was sent");
    assert!(
        get(&fake, &instance_path("toy-box-t1"))
            .is_some_and(|real| { real["config"].get("user.env").is_none() })
    );
}

// Another writer after a patch's kill.

#[test]
fn attack_other_writer_value_after_patch_kill_is_reverted() {
    let scratch = Scratch::new("atk-p");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let first = signed(&key, 1, None, vec![patcher_entry("v1")]);
    run_killed(
        &scratch,
        &fake,
        &key,
        &retained,
        None,
        Kill::EffectAfter(3),
        &first,
    );
    // Another writer sets the key after the kill.
    let (status, _) = raw(
        &fake,
        "PATCH",
        &acl_path("p-acl-q"),
        Some(&json!({ "config": { "user.k": "z" } })),
    );
    assert_eq!(status, 200);
    let r = restarted(
        &scratch,
        &key,
        plain(&scratch, &fake, &retained),
        plugins(),
        &retained,
        applied_of(&first),
    )
    .expect("opens");
    eprintln!("RESOLUTION: {:?}", r.resolution());
    eprintln!(
        "LIVE: {:?}",
        r.journal()
            .live(&pe_id())
            .iter()
            .map(|x| (&x.id, &x.state, &x.inverse))
            .collect::<Vec<_>>()
    );
}

#[test]
fn a_plan_whose_instance_is_gone_before_the_next_removal_is_withdrawn_and_nothing_is_pending() {
    let scratch = Scratch::new("stale-plan");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let first = signed(&key, 1, None, vec![toy_entry("t1")]);
    let second = signed(&key, 2, Some(first.document.digest()), vec![]);
    let third = signed(&key, 3, Some(second.document.digest()), vec![]);
    let mut r = restarted(
        &scratch,
        &key,
        plain(&scratch, &fake, &retained),
        plugins(),
        &retained,
        None,
    )
    .expect("opens");
    r.reconcile(&first, NOW);
    let removal = r.reconcile(&second, NOW);
    assert_eq!(asked(&removal).len(), 1, "the instance waits: {removal:?}");
    // The instance's delete completes while the owner has not answered.
    let (status, _) = raw(&fake, "DELETE", &instance_path("toy-box-t1"), None);
    assert!(status < 300, "the instance is gone: {status}");

    let again = r.reconcile(&third, NOW);
    assert!(asked(&again).is_empty(), "nothing is asked: {again:?}");
    assert!(
        r.pending_plans().is_empty(),
        "a plan for a gone instance is withdrawn: {:?}",
        r.pending_plans()
    );
    assert!(r.journal().live_entries().is_empty());
    nothing_left(&fake);
    assert_eq!(deletes_for(&fake, "toy-box-t1"), 1);
}

// Running operations: an intent is never settled while an operation still names its target.

/// How long a write or a read-back waits on a running operation in these cases: short, so a held operation outlasts it.
const SHORT: Duration = Duration::from_millis(200);
/// A deadline no held operation in these cases reaches, so a wait ends with its operation.
const LONG: Duration = Duration::from_secs(20);

/// The Incus provider over `fake` with every operation's deadline at `deadline`, for the create and for the read-back alike.
fn within(
    scratch: &Scratch,
    fake: &FakeIncus,
    retained: &RetainedSet,
    deadline: Duration,
) -> IncusProvider {
    provider(scratch, fake, retained).with_operation_deadline(deadline)
}

/// Runs the toy's document and kills the process right after the instance's create was sent, while Incus still runs it: the
/// writer gives up waiting at [`SHORT`], and the kill lands before anything else. The ACL's create is journalled as applied and
/// the instance's intent is left open.
fn kill_while_the_instance_create_runs(
    scratch: &Scratch,
    fake: &FakeIncus,
    key: &NodeKey,
    retained: &RetainedSet,
    doc: &SignedDocument,
) -> u64 {
    run_killed_by(
        scratch,
        key,
        retained,
        None,
        Box::new(Killing {
            inner: within(scratch, fake, retained, SHORT),
            kill: Kill::EffectAfter(2),
            effects: 0,
            retires: 0,
        }),
        doc,
    );
    let ids = open_ids(scratch);
    assert_eq!(ids.len(), 1, "the instance's intent is open: {ids:?}");
    ids[0]
}

/// The ids of the intents the journal file holds open.
fn open_ids(scratch: &Scratch) -> Vec<u64> {
    Journal::open(FileJournal::open(&scratch.path("journal.jsonl")).expect("the journal opens"))
        .expect("the journal folds")
        .unresolved()
        .iter()
        .map(|record| record.id)
        .collect()
}

/// Every instance that carries the toy's mark is an object the journal holds live for the toy.
fn every_marked_instance_is_journalled(fake: &FakeIncus, r: &Reconciler<FileJournal>) {
    let live = r.journal().live(&toy_id());
    for name in instances_marked(fake, MARK, TOY) {
        assert!(
            live.iter().any(|record| record
                .object
                .as_ref()
                .is_some_and(|object| object.name == name)),
            "{name} carries the mark and the journal knows it"
        );
    }
}

#[test]
fn a_restart_while_a_create_still_runs_keeps_its_intent_open_and_the_open_refuses() {
    let scratch = Scratch::new("op-running");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    let first = signed(&key, 1, None, vec![toy_entry("t1")]);
    let held = fake.hold_create(&instance_object("toy-box-t1", None), Appears::OnCompletion);
    let open = kill_while_the_instance_create_runs(&scratch, &fake, &key, &retained, &first);
    assert!(
        get(&fake, &instance_path("toy-box-t1")).is_none(),
        "the instance is not visible while its create runs"
    );
    let writes = fake.writes().len();

    // Before this rule, one GET saw nothing at the name and settled the create as reverted, and the instance then landed with
    // the toy's mark and no record of it.
    let refused = match restarted(
        &scratch,
        &key,
        Box::new(within(&scratch, &fake, &retained, SHORT)),
        plugins(),
        &retained,
        applied_of(&first),
    ) {
        Ok(_) => panic!("an intent whose create still runs must not settle"),
        Err(error) => error,
    };
    assert_eq!(refused, KernelError::Unresolved { ids: vec![open] });
    assert_eq!(open_ids(&scratch), vec![open], "the intent is still open");
    assert_eq!(fake.writes().len(), writes, "the open sent no write");

    // The operation completes. The next open journals the instance the create made.
    fake.release_operation(&held);
    let r = restarted(
        &scratch,
        &key,
        plain(&scratch, &fake, &retained),
        plugins(),
        &retained,
        applied_of(&first),
    )
    .expect("the journal opens once the create has ended");
    let uuid = toy_instance_uuid(&fake);
    assert_eq!(
        r.resolution().adopted,
        vec![instance_object("toy-box-t1", Some(&uuid))]
    );
    assert!(r.resolution().reverted.is_empty());
    every_marked_instance_is_journalled(&fake, &r);
}

#[test]
fn a_create_that_completes_while_the_restart_waits_is_adopted_by_that_open() {
    let scratch = Scratch::new("op-completes");
    let fake = Arc::new(start_fake(&scratch));
    let key = node_key();
    let retained = RetainedSet::empty();
    let first = signed(&key, 1, None, vec![toy_entry("t1")]);
    let held = fake.hold_create(&instance_object("toy-box-t1", None), Appears::OnCompletion);
    kill_while_the_instance_create_runs(&scratch, &fake, &key, &retained, &first);

    let releaser = {
        let fake = Arc::clone(&fake);
        thread::spawn(move || {
            thread::sleep(SHORT);
            fake.release_operation(&held);
        })
    };
    let r = restarted(
        &scratch,
        &key,
        Box::new(within(&scratch, &fake, &retained, LONG)),
        plugins(),
        &retained,
        applied_of(&first),
    )
    .expect("the open waits for the create, then settles it");
    releaser.join().expect("the releaser ends");
    let uuid = toy_instance_uuid(&fake);
    assert_eq!(
        r.resolution().adopted,
        vec![instance_object("toy-box-t1", Some(&uuid))]
    );
    assert!(r.resolution().reverted.is_empty() && r.journal().unresolved().is_empty());
    every_marked_instance_is_journalled(&fake, &r);
}

#[test]
fn a_create_whose_operation_fails_while_the_restart_waits_is_reverted_and_leaves_nothing() {
    // The instance shows while its create runs, and Incus removes it when the create fails. Before this rule, the open read it
    // while it ran and journalled an instance that never landed.
    let scratch = Scratch::new("op-fails");
    let fake = Arc::new(start_fake(&scratch));
    let key = node_key();
    let retained = RetainedSet::empty();
    let first = signed(&key, 1, None, vec![toy_entry("t1")]);
    let held = fake.hold_create(&instance_object("toy-box-t1", None), Appears::WhileRunning);
    let open = kill_while_the_instance_create_runs(&scratch, &fake, &key, &retained, &first);
    assert!(
        get(&fake, &instance_path("toy-box-t1")).is_some(),
        "the instance shows while its create runs"
    );

    let failer = {
        let fake = Arc::clone(&fake);
        thread::spawn(move || {
            thread::sleep(SHORT);
            fake.fail_operation(&held, "the create failed");
        })
    };
    let mut r = restarted(
        &scratch,
        &key,
        Box::new(within(&scratch, &fake, &retained, LONG)),
        plugins(),
        &retained,
        applied_of(&first),
    )
    .expect("the open waits for the create, then settles it");
    failer.join().expect("the failer ends");
    assert_eq!(r.resolution().reverted, vec![open]);
    assert!(r.resolution().adopted.is_empty());
    assert!(instances_marked(&fake, MARK, TOY).is_empty());

    let second = signed(&key, 2, Some(first.document.digest()), vec![]);
    assert_eq!(
        r.reconcile(&second, NOW),
        vec![Outcome::Reverted {
            entry: toy_id(),
            effects: 1
        }],
        "only the ACL is left to revert; nothing is asked about an instance that never landed"
    );
    nothing_left(&fake);
    assert!(r.journal().live_entries().is_empty());
}

#[test]
fn a_running_operation_on_another_object_holds_no_intent() {
    let scratch = Scratch::new("op-other");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    intend_by_hand(
        &scratch,
        &toy_id(),
        EffectClass::B,
        Digest::of(b"a create"),
        instance_object("toy-box-t1", None),
        None,
    );
    fake.hold_operation(&instance_object("toy-box-t2", None));
    fake.hold_operation(&ObjectRef {
        project: "default".to_string(),
        ..instance_object("toy-box-t1", None)
    });
    fake.hold_operation(&acl("toy-box-t1"));
    fake.hold_operation(&ObjectRef {
        kind: IncusKind::Profile,
        ..instance_object("toy-box-t1", None)
    });

    let r = restarted(
        &scratch,
        &key,
        Box::new(within(&scratch, &fake, &retained, LONG)),
        plugins(),
        &retained,
        None,
    )
    .expect("an operation on another object holds nothing");
    assert_eq!(r.resolution().reverted, vec![1]);
    assert!(
        !fake
            .requests()
            .iter()
            .any(|request| request.path.contains("/wait")),
        "no operation was waited on"
    );
}

#[test]
fn a_failed_operations_read_holds_every_intent_and_the_open_refuses_with_no_write() {
    let scratch = Scratch::new("op-unreadable");
    let fake = start_fake(&scratch);
    let key = node_key();
    let retained = RetainedSet::empty();
    intend_by_hand(
        &scratch,
        &toy_id(),
        EffectClass::A,
        Digest::of(b"a create"),
        acl("toy-acl-t1"),
        None,
    );
    intend_by_hand(
        &scratch,
        &toy_id(),
        EffectClass::B,
        Digest::of(b"another create"),
        instance_object("toy-box-t1", None),
        None,
    );
    fake.set_operations_unreadable(true);

    let refused = match restarted(
        &scratch,
        &key,
        plain(&scratch, &fake, &retained),
        plugins(),
        &retained,
        None,
    ) {
        Ok(_) => panic!("an intent whose operations cannot be read must not settle"),
        Err(error) => error,
    };
    assert_eq!(refused, KernelError::Unresolved { ids: vec![1, 2] });
    assert!(fake.writes().is_empty(), "nothing was written");
    assert!(
        fake.requests()
            .iter()
            .all(|request| request.path == arca_incus::OPERATIONS_PATH),
        "no object was read while the operations could not be: {:?}",
        fake.requests()
    );

    fake.set_operations_unreadable(false);
    let r = restarted(
        &scratch,
        &key,
        plain(&scratch, &fake, &retained),
        plugins(),
        &retained,
        None,
    )
    .expect("the open settles once the operations read");
    assert_eq!(r.resolution().reverted, vec![1, 2]);
}
