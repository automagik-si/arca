//! The reconciler: the one writer. It accepts signed desired state through the gate, applies new and changed entries through the
//! compiled-in plugins, and reverts what a removed entry left behind. Class-A effects are reverted at once, except an object
//! the entry's class-B objects use, which its plugin declares: that one is retired after them. Class-B objects are
//! deleted only by [`Reconciler::run_plan`], after an approval over the plan's digest; a declined plan's objects are retained,
//! and nothing deletes them afterwards. Every effect is journalled as an intent before it runs and as its outcome after.
//!
//! An unresolved intent blocks every new effect. [`Reconciler::open`] settles the intents a restart left open before it returns,
//! and refuses a journal whose intent it cannot settle. A run that leaves an intent open stops at once. A plugin's own writes pass
//! through a gate that refuses its next intent while one is open, so a plugin that swallows the refusal still cannot write past it.

pub mod probe;
mod restart;
#[cfg(feature = "toy")]
pub mod toy;

pub use restart::Resolution;

use arca_approval::{ApprovalRefusal, ApprovalVerifier, ArcaAction, NonceStore, SignatureVerifier};
use arca_kernel::KernelError;
use arca_kernel::canon::Digest;
use arca_kernel::desired::{
    DesiredStateGate, DocRefusal, Entry, EntryId, PluginId, SignedDocument, Target,
};
use arca_kernel::journal::{
    DeletePlan, EffectClass, EffectRecord, EffectState, IntentFields, Inverse, Journal,
    JournalSink, JournalStore, removal,
};
use arca_kernel::ownership::ObjectRef;
use arca_kernel::plugin::{
    Ctx, EffectOp, EffectRefusal, Erased, ErasedPlugin, Manifest, Plugin, Provider, ReadBack,
    Retirement, op_digest,
};
use arca_providers::incus::RetainedSet;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

/// The longest a delete plan waits for its approval, in milliseconds. A plan older than this needs a new approval over a plan
/// made now.
pub const PLAN_TTL_MS: u64 = 15 * 60 * 1000;

/// Why a step does not run while an earlier effect is unresolved. The text names the rule and no value.
const BLOCKED_WHY: &str = "an earlier effect is unresolved; the step does not run";

/// The compiled-in plugins, keyed by id. A plugin is added once, as its erased form, so the reconciler holds every plugin
/// through one object-safe type.
#[derive(Default)]
pub struct PluginSet {
    plugins: BTreeMap<String, Box<dyn ErasedPlugin>>,
}

impl PluginSet {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds plugin `P`. Refused when its id is not valid or is already in the set.
    pub fn add<P: Plugin + 'static>(&mut self) -> Result<(), KernelError> {
        let erased = Erased::<P>::new()?;
        if self.plugins.contains_key(P::ID) {
            return Err(KernelError::Plugin(
                "a plugin with this id is already added".to_string(),
            ));
        }
        self.plugins.insert(P::ID.to_string(), Box::new(erased));
        Ok(())
    }

    /// The plugin with `id`, when one is in the set.
    pub fn get(&self, id: &str) -> Option<&dyn ErasedPlugin> {
        self.plugins.get(id).map(|plugin| plugin.as_ref())
    }
}

/// What the reconciler did with one document, or with one entry of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The entry's plugin applied it.
    Applied { entry: EntryId },
    /// `effects` class-A effects of a removed entry were reverted.
    Reverted { entry: EntryId, effects: usize },
    /// A removed entry's class-B objects wait for an approval over `plan`'s digest.
    AwaitingApproval { plan: DeletePlan },
    /// The document was not applied, and the last applied state holds.
    Held { stale: bool },
    /// The gate refused the document; nothing moved.
    Refused(DocRefusal),
    /// An entry's apply, or one step of its removal, did not complete. `why` names the rule and never a value.
    Failed { entry: EntryId, why: String },
}

/// The result of an approved run: what was deleted, what was found changed (and so left in place), and whether the run was
/// partial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanRun {
    pub deleted: Vec<ObjectRef>,
    pub drift: Vec<ObjectRef>,
    pub partial: bool,
}

/// A removed entry's delete plan that the owner has not yet approved, with the plugin that owned the entry.
#[derive(Debug, Clone)]
struct Pending {
    plan: DeletePlan,
    plugin: PluginId,
}

/// What one journalled retirement came to. The delete's own intent is settled by the retirement; the caller settles the
/// record it undoes.
enum Retired {
    /// The object is gone, as the provider reports it.
    Deleted(ObjectRef),
    /// The object is kept, and the text names the rule that held it. The delete's intent is failed.
    Kept(String),
    /// The provider's outcome is not known. The delete's intent stays open, and every later step is refused.
    Open,
}

/// The one writer. It owns the gate, the journal, the plugins, the provider and the retained set. The provider and the
/// retained set share their state, so a decline is seen by every writer the provider makes.
pub struct Reconciler<S: JournalStore> {
    gate: DesiredStateGate,
    journal: Journal<S>,
    plugins: PluginSet,
    provider: Box<dyn Provider + Send>,
    retained: RetainedSet,
    /// The plugin that applied each entry the journal still holds effects for. A removal needs it. An entry is recorded here before
    /// its apply runs, so a failed apply that made partial effects can still be removed.
    applied_by: BTreeMap<EntryId, PluginId>,
    /// The config each entry last applied with, once that apply succeeded. An entry whose config is here is not applied again. A
    /// removal takes the entry out, and a failed apply never puts it in, so the same config is retried.
    converged: BTreeMap<EntryId, Entry>,
    pending: Vec<Pending>,
    /// What the last resolution settled.
    resolution: Resolution,
}

/// The journal as a plugin's context writes to it. An intent is refused while an earlier intent has no outcome, or while the
/// journal has stopped, so a plugin that swallows an unknown outcome cannot write past it. Outcomes pass through unchanged.
struct Gate<'a, S: JournalStore> {
    journal: &'a mut Journal<S>,
    /// The plugin whose apply this gate serves. Every intent it records names the plugin.
    plugin: PluginId,
}

impl<S: JournalStore> JournalSink for Gate<'_, S> {
    fn intend_acl(
        &mut self,
        fields: IntentFields<'_>,
        patch: arca_kernel::journal::AclPatch,
    ) -> Result<u64, KernelError> {
        if let Some(error) = self.journal.poisoned() {
            return Err(error.clone());
        }
        let unresolved = self.journal.unresolved();
        if !unresolved.is_empty() {
            return Err(KernelError::Unresolved {
                ids: unresolved.iter().map(|r| r.id).collect(),
            });
        }
        self.journal
            .intend_acl_by(Some(&self.plugin), fields, patch)
    }

    fn intend(
        &mut self,
        entry: &EntryId,
        class: EffectClass,
        op_digest: Digest,
        target: ObjectRef,
        pre_image: Option<BTreeMap<String, Option<String>>>,
        now_ms: u64,
    ) -> Result<u64, KernelError> {
        if let Some(error) = self.journal.poisoned() {
            return Err(error.clone());
        }
        let unresolved = self.journal.unresolved();
        if !unresolved.is_empty() {
            return Err(KernelError::Unresolved {
                ids: unresolved.iter().map(|record| record.id).collect(),
            });
        }
        self.journal.intend_by(
            &self.plugin,
            IntentFields {
                entry,
                class,
                op_digest,
                target,
                pre_image,
                now_ms,
            },
        )
    }

    fn applied(&mut self, id: u64, object: ObjectRef, inverse: Inverse) -> Result<(), KernelError> {
        self.journal.applied(id, object, inverse)
    }

    fn failed(&mut self, id: u64, why: &str) -> Result<(), KernelError> {
        self.journal.failed(id, why)
    }
}

/// Whether two references name the same object. The uuid is left out, since a create's intent does not carry one.
fn same_place(a: &ObjectRef, b: &ObjectRef) -> bool {
    a.kind == b.kind && a.project == b.project && a.parent == b.parent && a.name == b.name
}

impl<S: JournalStore> Reconciler<S> {
    /// Opens the reconciler over `journal`. Before it returns, every intent a restart left open is settled from what the host
    /// shows (see [`Reconciler::resolve`]), and each entry's plugin is read back from the journal. Refused with
    /// [`KernelError::Unresolved`] when an intent cannot be settled, and then nothing runs and no effect is sent.
    pub fn open(
        gate: DesiredStateGate,
        journal: Journal<S>,
        plugins: PluginSet,
        provider: Box<dyn Provider + Send>,
        retained: RetainedSet,
        now_ms: u64,
    ) -> Result<Self, KernelError> {
        Self::open_resuming(
            gate,
            journal,
            plugins,
            provider,
            retained,
            Vec::new(),
            now_ms,
        )
    }

    /// As [`Reconciler::open`], resumed from `converged`: the entries that applied with their current config before the restart,
    /// as [`Reconciler::converged`] reported them. An entry with an intent the journal still holds unresolved is not resumed as
    /// converged, so the resolution decides what it is. Nothing is applied by this call.
    pub fn open_resuming(
        gate: DesiredStateGate,
        journal: Journal<S>,
        plugins: PluginSet,
        provider: Box<dyn Provider + Send>,
        retained: RetainedSet,
        converged: Vec<Entry>,
        now_ms: u64,
    ) -> Result<Self, KernelError> {
        if let Some(error) = journal.poisoned() {
            return Err(error.clone());
        }
        let unsettled: BTreeSet<EntryId> = journal
            .unresolved()
            .into_iter()
            .map(|record| record.entry)
            .collect();
        let converged = converged
            .into_iter()
            .filter(|entry| !unsettled.contains(&entry.id))
            .map(|entry| (entry.id.clone(), entry))
            .collect();
        let mut reconciler = Reconciler {
            gate,
            journal,
            plugins,
            provider,
            retained,
            applied_by: BTreeMap::new(),
            converged,
            pending: Vec::new(),
            resolution: Resolution::default(),
        };
        reconciler.rebuild_applied_by();
        reconciler.resolve(now_ms)?;
        Ok(reconciler)
    }

    /// The entries that applied with their current config and have not been removed since, in id order. A caller persists
    /// this and passes it to [`Reconciler::open_resuming`] after a restart.
    pub fn converged(&self) -> Vec<Entry> {
        self.converged.values().cloned().collect()
    }

    /// What the last resolution settled: the intents it adopted, reverted or left failed, the entries it held, and the objects
    /// whose live state differs from what their intent left.
    pub fn resolution(&self) -> &Resolution {
        &self.resolution
    }

    /// The journal, read only.
    pub fn journal(&self) -> &Journal<S> {
        &self.journal
    }

    /// Accepts `doc` at `now_ms` and reconciles toward it. Returns one outcome per applied entry, removal step or refusal.
    pub fn reconcile(&mut self, doc: &SignedDocument, now_ms: u64) -> Vec<Outcome> {
        match self.gate.accept(doc, now_ms) {
            Ok(Target::Apply(_)) => {}
            Ok(Target::Hold { stale, .. }) => return vec![Outcome::Held { stale }],
            Err(refusal) => return vec![Outcome::Refused(refusal)],
        }
        let carried: BTreeSet<EntryId> = doc
            .document
            .entries
            .iter()
            .map(|entry| entry.id.clone())
            .collect();
        // An entry the desired state carries again is no longer being removed, so its pending plan is withdrawn.
        self.pending
            .retain(|pending| !carried.contains(&pending.plan.entry));
        let mut outcomes = Vec::new();
        for entry in &doc.document.entries {
            // A suspended entry keeps its objects: it is neither applied nor removed.
            if entry.disabled {
                continue;
            }
            self.apply_entry(entry, now_ms, &mut outcomes);
        }
        let mut gone: BTreeSet<EntryId> = self.journal.live_entries().into_iter().collect();
        gone.extend(self.applied_by.keys().cloned());
        for id in gone {
            if carried.contains(&id) {
                continue;
            }
            match self.retained.holds(&id) {
                Ok(false) => self.remove_entry(&id, now_ms, &mut outcomes),
                // A declined entry is left alone for good: no removal writes to it again.
                Ok(true) => {}
                Err(refusal) => outcomes.push(Outcome::Failed {
                    entry: id,
                    why: refusal.to_string(),
                }),
            }
        }
        outcomes
    }

    /// Runs the approved delete plan `plan` under `approval`, which is verified as an `arca-kernel-approve` `delete` over the target
    /// `plan-<digest>` with the arguments `{"plan": digest}`. Nothing is deleted before the approval checks pass. A plan older than
    /// [`PLAN_TTL_MS`] is refused, and so is a run whose live objects are not all in the plan.
    pub fn run_plan(
        &mut self,
        plan: &Digest,
        approval: &str,
        verifier: &mut ApprovalVerifier<impl SignatureVerifier, impl NonceStore>,
        now_ms: u64,
    ) -> Result<PlanRun, ApprovalRefusal> {
        let delete_plan = self.delete_plan_for(plan, now_ms)?;
        verifier.check(
            ArcaAction::Delete,
            &format!("plan-{}", plan.to_hex()),
            &json!({ "plan": plan.to_hex() }),
            approval,
            now_ms,
        )?;
        self.execute_plan(plan, delete_plan, now_ms)
    }

    /// Runs a plan whose owner approval was already checked by the caller. The same TTL, blocked and live-object checks run.
    pub fn run_plan_authorized(
        &mut self,
        plan: &Digest,
        now_ms: u64,
    ) -> Result<PlanRun, ApprovalRefusal> {
        let delete_plan = self.delete_plan_for(plan, now_ms)?;
        self.execute_plan(plan, delete_plan, now_ms)
    }

    fn delete_plan_for(&self, plan: &Digest, now_ms: u64) -> Result<DeletePlan, ApprovalRefusal> {
        let delete_plan = self
            .pending
            .iter()
            .find(|pending| &pending.plan.digest == plan)
            .map(|pending| pending.plan.clone())
            .ok_or(ApprovalRefusal::WrongTarget)?;
        if now_ms.saturating_sub(delete_plan.created_ms) > PLAN_TTL_MS {
            return Err(ApprovalRefusal::Expired);
        }
        if self.blocked() {
            return Err(ApprovalRefusal::Unresolved);
        }
        let live = self.live_deletes(&delete_plan.entry);
        if live
            .iter()
            .any(|object| !delete_plan.objects.contains(object))
        {
            return Err(ApprovalRefusal::WrongParams);
        }
        Ok(delete_plan)
    }

    fn execute_plan(
        &mut self,
        plan: &Digest,
        delete_plan: DeletePlan,
        now_ms: u64,
    ) -> Result<PlanRun, ApprovalRefusal> {
        let live = self.live_deletes(&delete_plan.entry);
        let mut run = PlanRun {
            deleted: Vec::new(),
            drift: Vec::new(),
            partial: false,
        };
        for object in live.iter().rev() {
            if self.blocked() {
                run.partial = true;
                break;
            }
            let retired = self
                .retire_step(&delete_plan.entry, EffectClass::B, object, plan, now_ms)
                .map_err(|error| ApprovalRefusal::Storage(error.to_string()))?;
            match retired {
                Retired::Deleted(done) => {
                    self.settle_deleted(&delete_plan.entry, object);
                    run.deleted.push(done);
                }
                Retired::Kept(_) => {
                    run.drift.push(object.clone());
                    run.partial = true;
                }
                Retired::Open => {
                    run.partial = true;
                    break;
                }
            }
        }
        if !run.partial {
            self.pending.retain(|pending| pending.plan.digest != *plan);
            self.retire_held(&delete_plan.entry, &live, &mut run, now_ms);
            if self.revertible_live(&delete_plan.entry) == 0 {
                self.applied_by.remove(&delete_plan.entry);
            }
        }
        Ok(run)
    }

    /// Retires the class-A objects of `entry` that waited for its class-B objects (see [`Plugin::retired_after_plan`]), most
    /// recent first, once the plan's run has deleted every one of `deleted` and each reads back absent. An object that does
    /// not read back absent keeps them waiting, and the run is partial. One Incus keeps is drift and makes the run partial;
    /// its record stays live, so the next removal of the entry retries it.
    fn retire_held(
        &mut self,
        entry: &EntryId,
        deleted: &[ObjectRef],
        run: &mut PlanRun,
        now_ms: u64,
    ) {
        let held = self.held_records(entry);
        if held.is_empty() {
            return;
        }
        let gone = deleted.iter().all(|object| {
            matches!(
                self.provider.read_back(entry, object, None),
                Ok(ReadBack::Absent)
            )
        });
        if !gone {
            run.partial = true;
            return;
        }
        for record in held {
            if self.blocked() {
                run.partial = true;
                return;
            }
            let Some(Inverse::IncusDelete { object }) = record.inverse.clone() else {
                continue;
            };
            match self.revert_a(&record, now_ms) {
                Ok(()) => run.deleted.push(object),
                Err(_) => {
                    run.drift.push(object);
                    run.partial = true;
                }
            }
        }
    }

    /// The class-A kinds of `entry` that wait for its class-B objects, as the plugin that applied it declares them.
    fn held_kinds(&self, entry: &EntryId) -> &'static [arca_kernel::ownership::IncusKind] {
        self.applied_by
            .get(entry)
            .and_then(|plugin| self.plugins.get(&plugin.0))
            .map_or(&[], |plugin| plugin.retired_after_plan())
    }

    /// Whether `record` is the create of a class-A object of one of `kinds`.
    fn is_held(record: &EffectRecord, kinds: &[arca_kernel::ownership::IncusKind]) -> bool {
        record.class == EffectClass::A
            && matches!(&record.inverse, Some(Inverse::IncusDelete { object }) if kinds.contains(&object.kind))
    }

    /// The entry's live creates of class-A objects that wait for its class-B objects, most recent first.
    fn held_records(&self, entry: &EntryId) -> Vec<EffectRecord> {
        let kinds = self.held_kinds(entry);
        let mut held: Vec<EffectRecord> = self
            .journal
            .live(entry)
            .into_iter()
            .filter(|record| record.state == EffectState::Applied && Self::is_held(record, kinds))
            .collect();
        held.reverse();
        held
    }

    /// Declines the pending plan `plan`: its objects become retained, which no later run or removal writes, and the decline is
    /// final. Returns the plan's objects that still exist. A plan that is not pending, or whose retention is refused, is left as it
    /// is and returns nothing.
    pub fn decline(&mut self, plan: &Digest) -> Vec<ObjectRef> {
        let Some(index) = self
            .pending
            .iter()
            .position(|pending| &pending.plan.digest == plan)
        else {
            return Vec::new();
        };
        let entry = self.pending[index].plan.entry.clone();
        let plugin = self.pending[index].plugin.clone();
        if self
            .retained
            .retain(entry.clone(), plugin, plan.clone())
            .is_err()
        {
            return Vec::new();
        }
        let declined = self.pending.remove(index);
        self.live_deletes(&entry)
            .into_iter()
            .filter(|object| declined.plan.objects.contains(object))
            .collect()
    }

    /// The delete plans waiting for an approval.
    pub fn pending_plans(&self) -> Vec<DeletePlan> {
        self.pending
            .iter()
            .map(|pending| pending.plan.clone())
            .collect()
    }

    /// Whether an intent has no outcome, or the journal has stopped. Every new effect is refused while this holds.
    fn blocked(&self) -> bool {
        self.journal.poisoned().is_some() || !self.journal.unresolved().is_empty()
    }

    /// The entry's class-B objects that still count: each one a live create whose inverse deletes it.
    fn live_deletes(&self, entry: &EntryId) -> Vec<ObjectRef> {
        self.journal
            .live(entry)
            .into_iter()
            .filter(|record| record.class == EffectClass::B)
            .filter_map(|record| match record.inverse {
                Some(Inverse::IncusDelete { object }) => Some(object),
                _ => None,
            })
            .collect()
    }

    /// How many of the entry's class-A and class-B effects still count. A class-C emission is never reverted, so it is not counted.
    fn revertible_live(&self, entry: &EntryId) -> usize {
        self.journal
            .live(entry)
            .iter()
            .filter(|record| record.class != EffectClass::C)
            .count()
    }

    /// The manifest of the plugin that applied `entry`. A removal that needs it fails closed when the entry is not known.
    fn manifest_of(&self, entry: &EntryId) -> Result<Manifest, String> {
        self.applied_by
            .get(entry)
            .and_then(|plugin| self.plugins.get(&plugin.0))
            .map(|plugin| plugin.manifest())
            .ok_or_else(|| "plugin not known".to_string())
    }

    /// Applies one entry that changed, or that is new. An entry the journal shows as retained is not adopted.
    fn apply_entry(&mut self, entry: &Entry, now_ms: u64, outcomes: &mut Vec<Outcome>) {
        let id = entry.id.clone();
        match self.retained.holds(&id) {
            Ok(false) => {}
            Ok(true) => {
                outcomes.push(Outcome::Failed {
                    entry: id,
                    why: "the entry's objects are retained after a declined plan; they are not adopted".to_string(),
                });
                return;
            }
            Err(refusal) => {
                outcomes.push(Outcome::Failed {
                    entry: id,
                    why: refusal.to_string(),
                });
                return;
            }
        }
        // An entry that applied with this config and has not been removed since is not applied again, unless its plugin
        // asks to reconcile every pass for state it repairs outside the entry config.
        if self.converged.get(&id) == Some(entry)
            && !self
                .plugins
                .get(&entry.plugin.0)
                .is_some_and(|plugin| plugin.reconciles_when_unchanged())
        {
            return;
        }
        if self.blocked() {
            outcomes.push(Outcome::Failed {
                entry: id,
                why: BLOCKED_WHY.to_string(),
            });
            return;
        }
        let Some(plugin) = self.plugins.get(&entry.plugin.0) else {
            outcomes.push(Outcome::Failed {
                entry: id,
                why: "plugin not known".to_string(),
            });
            return;
        };
        let manifest = plugin.manifest();
        // The plugin is recorded before the apply runs, even when the apply fails, so that a later removal finds the plugin that
        // made any partial effects.
        self.applied_by.insert(id.clone(), entry.plugin.clone());
        let result = {
            let mut gate = Gate {
                journal: &mut self.journal,
                plugin: manifest.id.clone(),
            };
            let mut ctx = Ctx::new(
                id.clone(),
                &manifest,
                &mut *self.provider,
                &mut gate,
                now_ms,
            );
            plugin.apply(&mut ctx, &entry.config)
        };
        match result {
            // A plugin that swallowed a refusal can return Ok with an intent still open. That is not an apply.
            Ok(()) if self.blocked() => outcomes.push(Outcome::Failed {
                entry: id,
                why: BLOCKED_WHY.to_string(),
            }),
            Ok(()) => {
                self.converged.insert(id.clone(), entry.clone());
                outcomes.push(Outcome::Applied { entry: id });
            }
            Err(error) => outcomes.push(Outcome::Failed {
                entry: id,
                why: error.to_string(),
            }),
        }
    }

    /// Removes an entry the desired state no longer carries: reverts its class-A effects, most recent first, and queues its
    /// class-B objects for an approval. Class-C emissions are left alone.
    fn remove_entry(&mut self, id: &EntryId, now_ms: u64, outcomes: &mut Vec<Outcome>) {
        // A removed entry has not converged: a document that carries it again applies it again.
        self.converged.remove(id);
        if !self.blocked() {
            self.settle_gone(id);
        }
        let split = removal(&self.journal, id, now_ms);
        // A class-A object the entry's class-B objects use waits for them: Incus refuses to delete an ACL a NIC still
        // names. It is retired by the plan's run, after every class-B object is gone, or here once none is left.
        let held = if split.plan.is_some() {
            self.held_kinds(id)
        } else {
            &[]
        };
        let mut reverted = 0;
        let mut stopped = false;
        for record in &split.revert_now {
            if Self::is_held(record, held) {
                continue;
            }
            if self.blocked() {
                stopped = true;
                break;
            }
            match self.revert_a(record, now_ms) {
                Ok(()) => reverted += 1,
                Err(why) => outcomes.push(Outcome::Failed {
                    entry: id.clone(),
                    why,
                }),
            }
        }
        if reverted > 0 {
            outcomes.push(Outcome::Reverted {
                entry: id.clone(),
                effects: reverted,
            });
        }
        if stopped || self.blocked() {
            outcomes.push(Outcome::Failed {
                entry: id.clone(),
                why: BLOCKED_WHY.to_string(),
            });
            return;
        }
        match split.plan {
            Some(plan) => self.plan_removal(id, plan, outcomes),
            // Nothing of the entry is left to delete, so a plan queued for it asks about objects that are gone.
            None => self.pending.retain(|pending| &pending.plan.entry != id),
        }
        if self.revertible_live(id) == 0 {
            self.applied_by.remove(id);
        }
    }

    /// Queues the entry's class-B objects under `plan`. A pending plan for the same objects keeps its creation time while it is
    /// within [`PLAN_TTL_MS`], so a later reconcile does not restart its age. Once it is older, it is replaced by `plan`, which
    /// was made now and needs an approval of its own.
    fn plan_removal(&mut self, id: &EntryId, plan: DeletePlan, outcomes: &mut Vec<Outcome>) {
        let plugin = self
            .applied_by
            .get(id)
            .cloned()
            .filter(|plugin| self.plugins.get(&plugin.0).is_some());
        let Some(plugin) = plugin else {
            outcomes.push(Outcome::Failed {
                entry: id.clone(),
                why: "plugin not known".to_string(),
            });
            return;
        };
        let waiting = match self
            .pending
            .iter()
            .position(|pending| pending.plan.digest == plan.digest)
        {
            Some(index)
                if plan
                    .created_ms
                    .saturating_sub(self.pending[index].plan.created_ms)
                    <= PLAN_TTL_MS =>
            {
                self.pending[index].plan.clone()
            }
            Some(index) => {
                self.pending[index].plan = plan.clone();
                plan
            }
            None => {
                self.pending.retain(|pending| &pending.plan.entry != id);
                self.pending.push(Pending {
                    plan: plan.clone(),
                    plugin,
                });
                plan
            }
        };
        outcomes.push(Outcome::AwaitingApproval { plan: waiting });
    }

    /// Reverts one class-A record. The record is settled only when its undo is done; otherwise it stays live for the next
    /// reconcile, and the text says why.
    fn revert_a(&mut self, record: &EffectRecord, now_ms: u64) -> Result<(), String> {
        match &record.inverse {
            Some(Inverse::IncusDelete { object }) => {
                // A create is undone with the digest of the create's own op as its tombstone, so no approval is needed.
                match self.retire_step(
                    &record.entry,
                    EffectClass::A,
                    object,
                    &record.op_digest,
                    now_ms,
                ) {
                    Ok(Retired::Deleted(_)) => {
                        self.settle_deleted(&record.entry, object);
                        Ok(())
                    }
                    Ok(Retired::Kept(why)) => Err(why),
                    Ok(Retired::Open) => {
                        Err("the undo's outcome is not known; its intent stays open".to_string())
                    }
                    Err(error) => Err(error.to_string()),
                }
            }
            Some(Inverse::IncusRestoreConfig { object, config }) => self.restore(
                record,
                EffectOp::IncusPatchConfig {
                    object: object.clone(),
                    config: config.clone(),
                },
                now_ms,
            ),
            Some(Inverse::IncusRestoreAclField {
                object,
                field,
                rules,
            }) => self.restore(
                record,
                EffectOp::IncusPatchAclField {
                    object: object.clone(),
                    field: *field,
                    rules: rules.clone(),
                },
                now_ms,
            ),
            _ => Err("removal does not run this undo".to_string()),
        }
    }

    /// Restores a patched object's keys through the plugin's own manifest, then settles the restore and the record it undoes.
    fn restore(&mut self, record: &EffectRecord, op: EffectOp, now_ms: u64) -> Result<(), String> {
        let manifest = self.manifest_of(&record.entry)?;
        let digest = op_digest(&op);
        {
            let mut gate = Gate {
                journal: &mut self.journal,
                plugin: manifest.id.clone(),
            };
            let mut ctx = Ctx::new(
                record.entry.clone(),
                &manifest,
                &mut *self.provider,
                &mut gate,
                now_ms,
            );
            ctx.effect(op).map_err(|refusal| refusal.to_string())?;
        }
        let restore = self
            .journal
            .live(&record.entry)
            .iter()
            .filter(|found| found.state == EffectState::Applied && found.op_digest == digest)
            .map(|found| found.id)
            .max()
            .ok_or_else(|| "the restore is not in the journal".to_string())?;
        self.journal.reverted(restore);
        self.journal.reverted(record.id);
        Ok(())
    }

    /// Journals one retirement: the delete's intent first, then the provider's retire, then the delete's outcome. An outcome the
    /// provider cannot vouch for leaves the intent open.
    fn retire_step(
        &mut self,
        entry: &EntryId,
        class: EffectClass,
        object: &ObjectRef,
        plan: &Digest,
        now_ms: u64,
    ) -> Result<Retired, KernelError> {
        let op = EffectOp::IncusDelete {
            object: object.clone(),
            plan: plan.clone(),
        };
        let delete =
            self.journal
                .intend_delete(entry, class, op_digest(&op), object.clone(), now_ms)?;
        match self.provider.retire(entry, object, plan) {
            Ok(Retirement::Deleted(done)) => {
                self.journal.reverted(delete);
                Ok(Retired::Deleted(done))
            }
            Ok(Retirement::Drift(why)) => {
                self.journal.failed(delete, &why)?;
                Ok(Retired::Kept(why))
            }
            Err(EffectRefusal::Unresolved) => Ok(Retired::Open),
            Err(refusal) => {
                let why = refusal.to_string();
                self.journal.failed(delete, &why)?;
                Ok(Retired::Kept(why))
            }
        }
    }

    /// Settles the entry's class-B objects that read back absent, before a plan asks the owner about them. Nothing is left to delete
    /// for such an object, so it is not put in a plan, and its records are settled as an absent create is at restart. An object
    /// whose read cannot be answered is left as it is, so the plan still asks about it.
    fn settle_gone(&mut self, entry: &EntryId) {
        let gone: Vec<ObjectRef> = self
            .live_deletes(entry)
            .into_iter()
            .filter(|object| {
                matches!(
                    self.provider.read_back(entry, object, None),
                    Ok(ReadBack::Absent)
                )
            })
            .collect();
        for object in &gone {
            self.settle_deleted(entry, object);
        }
    }

    /// Settles every live record of `entry` that names `object`, once the object is gone: its create, its patches, and the intent
    /// of the delete. A class-C emission is never reverted, so it is left live.
    fn settle_deleted(&mut self, entry: &EntryId, object: &ObjectRef) {
        let ids: Vec<u64> = self
            .journal
            .live(entry)
            .iter()
            .filter(|record| record.class != EffectClass::C)
            .filter(|record| {
                same_place(&record.target, object)
                    || record
                        .object
                        .as_ref()
                        .is_some_and(|named| same_place(named, object))
            })
            .map(|record| record.id)
            .collect();
        for id in ids {
            self.journal.reverted(id);
        }
    }
}

#[cfg(test)]
mod tests;
