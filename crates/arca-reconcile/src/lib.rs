//! The reconciler: the one writer. It accepts signed desired state through the gate, applies new and changed entries through the
//! compiled-in plugins, and reverts what a removed entry left behind. Class-A effects are reverted at once. Class-B objects are
//! deleted only by [`Reconciler::run_plan`], after an approval over the plan's digest; a declined plan's objects are retained,
//! and nothing deletes them afterwards. Every effect is journalled as an intent before it runs and as its outcome after.
//!
//! An unresolved intent blocks every new effect. [`Reconciler::open`] refuses a journal that holds one, and a run that leaves one
//! open stops at once.

#[cfg(feature = "toy")]
pub mod toy;

use arca_approval::{ApprovalRefusal, ApprovalVerifier, ArcaAction, NonceStore, SignatureVerifier};
use arca_kernel::KernelError;
use arca_kernel::canon::Digest;
use arca_kernel::desired::{
    DesiredStateGate, DocRefusal, Entry, EntryId, PluginId, SignedDocument, Target,
};
use arca_kernel::journal::{
    DeletePlan, EffectClass, EffectRecord, EffectState, Inverse, Journal, JournalSink,
    JournalStore, removal,
};
use arca_kernel::ownership::ObjectRef;
use arca_kernel::plugin::{
    Ctx, EffectOp, EffectRefusal, Erased, ErasedPlugin, Manifest, Plugin, Provider, Retirement,
    op_digest,
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
    provider: Box<dyn Provider>,
    retained: RetainedSet,
    applied: BTreeMap<EntryId, Entry>,
    pending: Vec<Pending>,
}

impl<S: JournalStore> Reconciler<S> {
    /// Opens the reconciler over `journal`. Refused with [`KernelError::Unresolved`] when the journal holds an intent with no
    /// outcome, and then nothing runs and no request is sent.
    pub fn open(
        gate: DesiredStateGate,
        journal: Journal<S>,
        plugins: PluginSet,
        provider: Box<dyn Provider>,
        retained: RetainedSet,
    ) -> Result<Self, KernelError> {
        if let Some(error) = journal.poisoned() {
            return Err(error.clone());
        }
        let unresolved = journal.unresolved();
        if !unresolved.is_empty() {
            return Err(KernelError::Unresolved {
                ids: unresolved.iter().map(|record| record.id).collect(),
            });
        }
        Ok(Reconciler {
            gate,
            journal,
            plugins,
            provider,
            retained,
            applied: BTreeMap::new(),
            pending: Vec::new(),
        })
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
        gone.extend(self.applied.keys().cloned());
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
        // A live object the plan does not name is a new step, and it needs a new approval over a new plan.
        let live = self.live_deletes(&delete_plan.entry);
        if live
            .iter()
            .any(|(_, object)| !delete_plan.objects.contains(object))
        {
            return Err(ApprovalRefusal::WrongParams);
        }
        verifier.check(
            ArcaAction::Delete,
            &format!("plan-{}", plan.to_hex()),
            &json!({ "plan": plan.to_hex() }),
            approval,
            now_ms,
        )?;
        let mut run = PlanRun {
            deleted: Vec::new(),
            drift: Vec::new(),
            partial: false,
        };
        // Most recent first, so a later object is gone before the one it was made on.
        for (record_id, object) in live.iter().rev() {
            if self.blocked() {
                run.partial = true;
                break;
            }
            let retired = self
                .retire_step(&delete_plan.entry, EffectClass::B, object, plan, now_ms)
                .map_err(|error| ApprovalRefusal::Storage(error.to_string()))?;
            match retired {
                Retired::Deleted(done) => {
                    self.journal.reverted(*record_id);
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
        // A partial run keeps its plan, so a retry needs a new approval. A full run retires the plan.
        if !run.partial {
            self.pending.retain(|pending| pending.plan.digest != *plan);
            if self.revertible_live(&delete_plan.entry) == 0 {
                self.applied.remove(&delete_plan.entry);
            }
        }
        Ok(run)
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
            .map(|(_, object)| object)
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

    /// The entry's class-B objects that still count, each with the record that created the delete it names.
    fn live_deletes(&self, entry: &EntryId) -> Vec<(u64, ObjectRef)> {
        self.journal
            .live(entry)
            .into_iter()
            .filter(|record| record.class == EffectClass::B)
            .filter_map(|record| match record.inverse {
                Some(Inverse::IncusDelete { object }) => Some((record.id, object)),
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
        self.applied
            .get(entry)
            .and_then(|applied| self.plugins.get(&applied.plugin.0))
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
        if self.applied.get(&id) == Some(entry) {
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
        // The entry is recorded as applied even when the apply fails, so that a later removal finds the plugin that made any
        // partial effects.
        self.applied.insert(id.clone(), entry.clone());
        let result = {
            let mut ctx = Ctx::new(
                id.clone(),
                &manifest,
                &mut *self.provider,
                &mut self.journal,
                now_ms,
            );
            plugin.apply(&mut ctx, &entry.config)
        };
        match result {
            Ok(()) => outcomes.push(Outcome::Applied { entry: id }),
            Err(error) => outcomes.push(Outcome::Failed {
                entry: id,
                why: error.to_string(),
            }),
        }
    }

    /// Removes an entry the desired state no longer carries: reverts its class-A effects, most recent first, and queues its
    /// class-B objects for an approval. Class-C emissions are left alone.
    fn remove_entry(&mut self, id: &EntryId, now_ms: u64, outcomes: &mut Vec<Outcome>) {
        let split = removal(&self.journal, id, now_ms);
        let mut reverted = 0;
        let mut stopped = false;
        for record in &split.revert_now {
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
        if let Some(plan) = split.plan {
            self.plan_removal(id, plan, outcomes);
        }
        if self.revertible_live(id) == 0 {
            self.applied.remove(id);
        }
    }

    /// Queues the entry's class-B objects under `plan`. A plan already pending for the same objects keeps its creation time, so
    /// its age is not reset by a later reconcile.
    fn plan_removal(&mut self, id: &EntryId, plan: DeletePlan, outcomes: &mut Vec<Outcome>) {
        let plugin = self
            .applied
            .get(id)
            .map(|applied| applied.plugin.clone())
            .filter(|plugin| self.plugins.get(&plugin.0).is_some());
        let Some(plugin) = plugin else {
            outcomes.push(Outcome::Failed {
                entry: id.clone(),
                why: "plugin not known".to_string(),
            });
            return;
        };
        if !self
            .pending
            .iter()
            .any(|pending| pending.plan.digest == plan.digest)
        {
            self.pending.retain(|pending| &pending.plan.entry != id);
            self.pending.push(Pending {
                plan: plan.clone(),
                plugin,
            });
        }
        let waiting = self
            .pending
            .iter()
            .find(|pending| pending.plan.digest == plan.digest)
            .map(|pending| pending.plan.clone())
            .unwrap_or(plan);
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
                        self.journal.reverted(record.id);
                        Ok(())
                    }
                    Ok(Retired::Kept(why)) => Err(why),
                    Ok(Retired::Open) => {
                        Err("the undo's outcome is not known; its intent stays open".to_string())
                    }
                    Err(error) => Err(error.to_string()),
                }
            }
            Some(Inverse::IncusRestoreConfig { object, config }) => {
                self.restore(record, object, config, now_ms)
            }
            _ => Err("removal does not run this undo".to_string()),
        }
    }

    /// Restores a patched object's keys through the plugin's own manifest, then settles the restore and the record it undoes.
    fn restore(
        &mut self,
        record: &EffectRecord,
        object: &ObjectRef,
        config: &BTreeMap<String, Option<String>>,
        now_ms: u64,
    ) -> Result<(), String> {
        let manifest = self.manifest_of(&record.entry)?;
        let op = EffectOp::IncusPatchConfig {
            object: object.clone(),
            config: config.clone(),
        };
        let digest = op_digest(&op);
        {
            let mut ctx = Ctx::new(
                record.entry.clone(),
                &manifest,
                &mut *self.provider,
                &mut self.journal,
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
                .intend(entry, class, op_digest(&op), object.clone(), None, now_ms)?;
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
}

#[cfg(test)]
mod tests;
