//! The kernel: the desired-state store, the reconciler that acts on it, and the owner approvals that release a delete plan. Every
//! method takes `&mut self`, so the verbs reach the kernel through one lock, and no two calls run at once.

use std::path::Path;

use arca_approval::{
    ApprovalRefusal, ApprovalVerifier, ArcaAction, FileNonces, OwnerKeys, PasskeyLine,
};
use arca_kernel::KernelError;
use arca_kernel::canon::Digest;
use arca_kernel::desired::DesiredStateGate;
use arca_kernel::identity::NodeKey;
use arca_kernel::journal::{DeletePlan, FileJournal, Journal};
use arca_kernel::ownership::ObjectRef;
use arca_kernel::plugin::Provider;
use arca_providers::incus::RetainedSet;
use arca_reconcile::{Outcome, PlanRun, PluginSet, Reconciler};
use serde_json::json;

use crate::desired_store::{DesiredStore, EntryChange, STATE_FILE};

/// The journal's file name in the state directory.
pub const JOURNAL_FILE: &str = "journal.jsonl";

/// The kernel of one node: its desired-state store, its reconciler and the approval verifier for delete plans.
pub struct Kernel {
    store: DesiredStore,
    reconciler: Reconciler<FileJournal>,
    retained: RetainedSet,
    verifier: ApprovalVerifier<OwnerKeys, FileNonces>,
    host: String,
}

impl Kernel {
    /// Opens the kernel over `state_dir`. The store takes its lock first, so a second kernel over the same directory is refused.
    /// The journal is opened and its intents settled, then the store's current document is reconciled once, so the delete plans
    /// it implies are waiting again. `provider` builds the provider from the retained set the reconciler shares with it, and
    /// `now_ms` is the clock every check of the start reads.
    pub fn open(
        state_dir: &Path,
        key: NodeKey,
        plugins: PluginSet,
        verifier: ApprovalVerifier<OwnerKeys, FileNonces>,
        host: String,
        provider: impl FnOnce(RetainedSet) -> Result<Box<dyn Provider + Send>, KernelError>,
        now_ms: u64,
    ) -> Result<Self, KernelError> {
        let mut store = DesiredStore::open(state_dir, key)?;
        let retained = RetainedSet::new(store.retained().clone());
        let provider = provider(retained.clone())?;
        let journal = Journal::open(FileJournal::open(&state_dir.join(JOURNAL_FILE))?)?;
        let gate = DesiredStateGate::new(store.signer(), store.applied().cloned());
        let mut reconciler = Reconciler::open_resuming(
            gate,
            journal,
            plugins,
            provider,
            retained.clone(),
            store.converged().to_vec(),
            now_ms,
        )?;
        if let Some(signed) = store.current().cloned() {
            // The outcomes of a start are not reported to a caller. A plan the current document implies is listed again.
            let _ = reconciler.reconcile(&signed, now_ms);
        }
        let converged = reconciler.converged();
        let retained_entries = retained.entries().map_err(retained_failure)?;
        store.save_applied(converged, retained_entries)?;
        Ok(Kernel {
            store,
            reconciler,
            retained,
            verifier,
            host,
        })
    }

    /// The entries in the current desired-state document, in application order.
    pub fn entries(&self) -> &[arca_kernel::desired::Entry] {
        self.store
            .current()
            .map_or(&[], |s| s.document.entries.as_slice())
    }

    /// The host name every approval statement of this kernel names.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The delete plans waiting for an owner's approval, in the order the reconciler holds them.
    pub fn pending_plans(&self) -> Vec<DeletePlan> {
        self.reconciler.pending_plans()
    }

    /// The passkey lines the owner verifier currently trusts, root-file and state-file lines together.
    pub fn owner_passkey_lines(&self) -> Vec<PasskeyLine> {
        self.verifier.verifier().passkey_lines().to_vec()
    }

    /// The number of passkeys the owner verifier currently trusts.
    pub fn owner_passkey_count(&self) -> usize {
        self.verifier.verifier().key_counts().1
    }

    /// Replaces the owner passkey lines without a restart, keeping the sign-count store and the SSH keys.
    pub fn replace_owner_passkeys(&mut self, lines: Vec<PasskeyLine>) {
        self.verifier.verifier_mut().replace_passkey_lines(lines);
    }

    /// Proposes `changes` as the next desired-state document and reconciles toward it. The document is persisted before it is
    /// reconciled, and the converged and retained sets are persisted after.
    pub fn propose(
        &mut self,
        changes: Vec<EntryChange>,
        now_ms: u64,
    ) -> Result<Vec<Outcome>, KernelError> {
        let signed = self.store.propose(changes, now_ms)?;
        let outcomes = self.reconciler.reconcile(&signed, now_ms);
        self.save()?;
        Ok(outcomes)
    }

    /// Declines the waiting plan with `plan`. Its objects are retained, and the decline is persisted before it is reported.
    /// Returns `None` when no plan waits under that digest, and otherwise the plan's objects that still exist.
    pub fn decline(&mut self, plan: &Digest) -> Result<Option<Vec<ObjectRef>>, KernelError> {
        if !self.is_pending(plan) {
            return Ok(None);
        }
        let objects = self.reconciler.decline(plan);
        if self.is_pending(plan) {
            // The reconciler keeps a plan it could not retain, and so does the store.
            return Err(KernelError::Io {
                path: STATE_FILE.to_string(),
                why: "the plan could not be retained".to_string(),
            });
        }
        self.save()?;
        Ok(Some(objects))
    }

    /// Checks an owner approval for a service action without a delete plan and spends it.
    pub fn check_approval(
        &mut self,
        action: ArcaAction,
        target: &str,
        args: &serde_json::Value,
        approval: &str,
        now_ms: u64,
    ) -> Result<(), ApprovalRefusal> {
        self.verifier
            .check(action, target, args, approval, now_ms)?;
        Ok(())
    }

    /// Runs a delete plan whose owner approval was checked and spent by the caller.
    pub fn run_plan_authorized(
        &mut self,
        plan: &Digest,
        now_ms: u64,
    ) -> Result<arca_reconcile::PlanRun, ApprovalRefusal> {
        self.reconciler.run_plan_authorized(plan, now_ms)
    }

    /// Checks `approval` for the plan with `plan` and, when the plan waits, runs its delete. A plan that waits nowhere is still
    /// checked, so a spent approval reads as replayed; a valid approval for a plan that no longer waits is spent and names no
    /// plan, and it returns `None`. Nothing is spent on any refusal.
    pub fn approve(
        &mut self,
        plan: &Digest,
        approval: &str,
        now_ms: u64,
    ) -> Result<Option<PlanRun>, ApprovalRefusal> {
        if !self.is_pending(plan) {
            let target = format!("plan-{}", plan.to_hex());
            let args = json!({ "plan": plan.to_hex() });
            self.verifier
                .check(ArcaAction::Delete, &target, &args, approval, now_ms)?;
            return Ok(None);
        }
        self.reconciler
            .run_plan(plan, approval, &mut self.verifier, now_ms)
            .map(Some)
    }

    fn is_pending(&self, plan: &Digest) -> bool {
        self.reconciler
            .pending_plans()
            .iter()
            .any(|pending| &pending.digest == plan)
    }

    /// Persists the converged set and the retained set.
    fn save(&mut self) -> Result<(), KernelError> {
        let retained = self.retained.entries().map_err(retained_failure)?;
        self.store
            .save_applied(self.reconciler.converged(), retained)
    }
}

fn retained_failure(_refusal: arca_kernel::plugin::EffectRefusal) -> KernelError {
    KernelError::Io {
        path: STATE_FILE.to_string(),
        why: "the retained set could not be read".to_string(),
    }
}
