//! The kernel: the desired-state store, the reconciler that acts on it, and the owner approvals that release a delete plan. Every
//! method takes `&mut self`, so the verbs reach the kernel through one lock, and no two calls run at once.

use std::path::Path;

use arca_approval::{
    ApprovalRefusal, ApprovalVerifier, ArcaAction, FileNonces, OwnerKeys, PasskeyLine,
};
use arca_kernel::KernelError;
use arca_kernel::canon::Digest;
use arca_kernel::desired::{DesiredStateGate, EntryId};
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

/// The reconciliation and approved plan run of one atomic service removal.
pub struct ServiceRemoval {
    pub outcomes: Vec<Outcome>,
    pub run: Result<Option<PlanRun>, ApprovalRefusal>,
}

/// A consumed approval, held across logout and consumed once by the proposal/run operation.
pub struct SpentServiceRemoval {
    entry: EntryId,
}

/// Whether removal stopped before approval was spent or during the proposal afterward.
#[derive(Debug)]
pub enum ServiceRemovalError {
    Approval(ApprovalRefusal),
    Proposal(KernelError),
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

    /// The sequence of the desired-state document the gate last applied, if any.
    pub fn applied_seq(&self) -> Option<u64> {
        self.store.applied().map(|applied| applied.seq)
    }

    /// The entries in the current desired-state document, in application order.
    pub fn entries(&self) -> &[arca_kernel::desired::Entry] {
        self.store
            .current()
            .map_or(&[], |s| s.document.entries.as_slice())
    }

    /// Identity from the newest live class-B instance effect, never from the desired record or a fresh Incus listing.
    pub fn service_uuid(&self, entry: &EntryId, instance: &str) -> Option<String> {
        self.reconciler
            .journal()
            .live(entry)
            .into_iter()
            .rev()
            .filter(|r| r.class == arca_kernel::journal::EffectClass::B)
            .filter_map(|r| r.object)
            .find(|o| {
                o.kind == arca_kernel::ownership::IncusKind::Instance
                    && o.name == instance
                    && o.project == arca_netproject::PROJECT
            })
            .and_then(|o| o.uuid)
    }

    pub fn retained_entries(
        &self,
    ) -> Result<
        std::collections::BTreeMap<EntryId, (arca_kernel::desired::PluginId, Digest)>,
        KernelError,
    > {
        self.retained.entries().map_err(retained_failure)
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

    /// Checks a single-use statement signed by an already trusted passkey, then atomically changes only the state file.
    /// The caller holds the kernel lock across the check, disk write and live key replacement. SSH proofs are refused.
    pub fn approve_passkey_change(
        &mut self,
        state_dir: &Path,
        line: &str,
        statement: &str,
        proof: &[u8],
        now_ms: u64,
    ) -> Result<(), ApprovalRefusal> {
        use crate::passkey_change as change;
        use arca_approval::{parse_passkeys, parse_proof, parse_statement};
        use base64::{Engine as _, engine::general_purpose::STANDARD};

        let incoming = change::single_line(line)?;
        let parsed = parse_statement(statement.as_bytes())?;
        if !matches!(
            parsed.action,
            ArcaAction::PasskeyAdd | ArcaAction::PasskeyRemove
        ) {
            return Err(ApprovalRefusal::WrongAction);
        }
        // A parsed WebAuthn proof is mandatory before OwnerKeys' dispatch; no other proof can change trust.
        parse_proof(proof)?;
        let approval = format!("{}.{}", STANDARD.encode(statement), STANDARD.encode(proof));
        self.verifier.check(
            parsed.action,
            &change::target(line),
            &json!({ "line": line }),
            &approval,
            now_ms,
        )?;

        let mut state = change::state_lines(state_dir)?;
        let previous = parse_passkeys(state.join("\n").as_bytes())?;
        let mut roots = self.owner_passkey_lines();
        roots.retain(|key| {
            !previous
                .iter()
                .any(|old| old.credential_id == key.credential_id)
        });
        match parsed.action {
            ArcaAction::PasskeyAdd => {
                if self.owner_passkey_lines().iter().any(|key| {
                    key.credential_id == incoming.credential_id
                        || key.fingerprint == incoming.fingerprint
                }) {
                    return Err(change::malformed());
                }
                state.push(line.to_string());
            }
            ArcaAction::PasskeyRemove => {
                // Membership is the exact state-file line, never a root-file line or an equivalent reformatting.
                let index = state
                    .iter()
                    .position(|stored| stored == line)
                    .ok_or_else(|| {
                        ApprovalRefusal::Untrusted(
                            "only a state-file passkey can be removed".into(),
                        )
                    })?;
                if self.owner_passkey_count() <= 1 {
                    return Err(ApprovalRefusal::Untrusted(
                        "the last trusted passkey cannot be removed".into(),
                    ));
                }
                state.remove(index);
            }
            _ => return Err(ApprovalRefusal::WrongAction),
        }
        let body = if state.is_empty() {
            String::new()
        } else {
            format!("{}\n", state.join("\n"))
        };
        let mut next = parse_passkeys(body.as_bytes())?;
        // Validate the union before writing: the root keys stay in memory and are never written by serve.
        if next.iter().any(|key| {
            roots.iter().any(|root| {
                root.credential_id == key.credential_id || root.fingerprint == key.fingerprint
            })
        }) {
            return Err(change::malformed());
        }
        next.extend(roots);
        change::write_state_bytes(state_dir, body.as_bytes()).map_err(|_| change::storage())?;
        self.replace_owner_passkeys(next);
        Ok(())
    }

    /// Verifies a WebAuthn assertion over `statement` against the owner's passkeys, advancing the shared sign count. The
    /// sign-in route uses this after it has spent its own challenge; approvals use the same store.
    pub fn verify_passkey(
        &self,
        namespace: &str,
        statement: &[u8],
        signature: &[u8],
    ) -> Result<String, ApprovalRefusal> {
        self.verifier
            .verifier()
            .verify_passkey(namespace, statement, signature)
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

    /// Spends one service-action approval, removes its entry and runs its pending plan under the caller's one kernel lock.
    /// A concurrent plan approval cannot observe the plan between the proposal and its execution.
    pub fn remove_service(
        &mut self,
        entry: &EntryId,
        name: &str,
        approval: &str,
        now_ms: u64,
    ) -> Result<ServiceRemoval, ServiceRemovalError> {
        let spent = self.spend_service_removal(entry, name, approval, now_ms)?;
        self.remove_service_spent(spent, now_ms)
    }

    pub fn spend_service_removal(
        &mut self,
        entry: &EntryId,
        name: &str,
        approval: &str,
        now_ms: u64,
    ) -> Result<SpentServiceRemoval, ServiceRemovalError> {
        self.check_approval(
            ArcaAction::Delete,
            name,
            &json!({ "name": name }),
            approval,
            now_ms,
        )
        .map_err(ServiceRemovalError::Approval)?;
        Ok(SpentServiceRemoval {
            entry: entry.clone(),
        })
    }

    pub fn remove_service_spent(
        &mut self,
        spent: SpentServiceRemoval,
        now_ms: u64,
    ) -> Result<ServiceRemoval, ServiceRemovalError> {
        let entry = &spent.entry;
        let name = entry.0.strip_prefix("service-").unwrap_or("");
        let mut changes = Vec::new();
        for other in self.entries() {
            if &other.id == entry || other.plugin.0 != "arca.service" {
                continue;
            }
            let Ok(mut cfg) =
                serde_json::from_value::<crate::service::ServiceConfig>(other.config.clone())
            else {
                continue;
            };
            if cfg.adopted {
                continue;
            }
            let incoming = cfg.incoming.len();
            let links = cfg.links.len();
            cfg.incoming.retain(|g| g.consumer != name);
            let removed_keys = cfg
                .links
                .iter()
                .filter(|l| l.to == name)
                .flat_map(|l| l.env.clone())
                .collect();
            cfg.environment_patch = Some(removed_keys);
            for link in cfg.links.iter().filter(|l| l.to == name) {
                for key in &link.env {
                    cfg.environment.remove(key);
                }
            }
            cfg.links.retain(|l| l.to != name);
            if cfg.incoming.len() != incoming || cfg.links.len() != links {
                let mut updated = other.clone();
                updated.config = serde_json::to_value(cfg).map_err(|_| {
                    ServiceRemovalError::Proposal(KernelError::Journal(
                        "service links cannot be encoded".into(),
                    ))
                })?;
                changes.push(EntryChange::Put(updated));
            }
        }
        let linked = changes
            .iter()
            .filter_map(|change| match change {
                EntryChange::Put(entry) => Some(entry.id.clone()),
                _ => None,
            })
            .collect::<std::collections::BTreeSet<_>>();
        changes.push(EntryChange::Remove(entry.clone()));
        let outcomes = self
            .propose(changes, now_ms)
            .map_err(ServiceRemovalError::Proposal)?;
        // A failed link revocation cannot release this address for another service. Failures retiring this service's own
        // class-A objects retain the established partial-removal behavior.
        if outcomes
            .iter()
            .any(|outcome| matches!(outcome, Outcome::Failed{entry,..} if linked.contains(entry)))
        {
            return Ok(ServiceRemoval {
                outcomes,
                run: Ok(None),
            });
        }
        let run = self
            .pending_plans()
            .into_iter()
            .find(|plan| &plan.entry == entry)
            .map(|plan| self.run_plan_authorized(&plan.digest, now_ms))
            .transpose();
        Ok(ServiceRemoval { outcomes, run })
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
