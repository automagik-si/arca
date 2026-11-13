//! The kernel: the desired-state store, the reconciler that acts on it, and the owner approvals that release a delete plan. Every
//! method takes `&mut self`, so the verbs reach the kernel through one lock, and no two calls run at once.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

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
use arca_providers::incus::{RetainedSet, TenantProjects};
use arca_reconcile::{Outcome, PlanRun, PluginSet, Reconciler};
use serde_json::{Value, json};

use crate::action_proposal::{
    ACTION_PROPOSALS_FILE, ActionKind, ActionProposal, ActionProposalStore, ProposeError,
    action_args_digest,
};
use crate::desired_store::{DesiredStore, EntryChange, STATE_FILE};

/// The journal's file name in the state directory.
pub const JOURNAL_FILE: &str = "journal.jsonl";

/// The last applied sequence, readable without taking the kernel lock. Zero means no document has applied; valid sequences
/// start at one. This value publishes only the sequence, not any other kernel state.
#[derive(Clone, Default)]
pub struct AppliedSeq(Arc<AtomicU64>);

impl AppliedSeq {
    pub fn load(&self) -> Option<u64> {
        let seq = self.0.load(Ordering::Relaxed);
        (seq != 0).then_some(seq)
    }

    fn publish(&self, seq: Option<u64>) {
        self.0.store(seq.unwrap_or(0), Ordering::Relaxed);
    }
}

/// The kernel of one node: its desired-state store, its reconciler, the approval verifier for delete plans, and the inert
/// action proposals an owner may approve with a passkey.
pub struct Kernel {
    store: DesiredStore,
    applied_seq: AppliedSeq,
    reconciler: Reconciler<FileJournal>,
    retained: RetainedSet,
    tenants: TenantProjects,
    /// The one gate every handler that reads the node's records and then writes holds across both: a service, a
    /// database or a tenant create picks its address or block and records it under it, so no two pick from the same
    /// reading.
    writes: Arc<std::sync::Mutex<()>>,
    verifier: ApprovalVerifier<OwnerKeys, FileNonces>,
    host: String,
    action_proposals: ActionProposalStore,
    action_grants: Vec<ActionGrant>,
}

/// A one-shot permission to run an already approved action through its gated verb. The token is random, not the owner's
/// approval, and the handler consumes it before doing any work.
struct ActionGrant {
    /// The approved proposal's id, so a result that arrives later can be journalled against it.
    id: String,
    action: String,
    args_digest: String,
    token: String,
}

/// An action proposal whose owner approval verified and whose nonce was spent, ready to run once.
#[derive(Debug, Clone)]
pub struct ApprovedAction {
    pub id: String,
    pub action: String,
    pub args: Value,
    pub grant: String,
}

/// Why an action proposal could not be approved. Nothing ran.
#[derive(Debug)]
pub enum ActionApprovalError {
    /// No unexpired proposal waits under this id.
    NoSuch,
    /// The proposal's fifteen minutes have run.
    Expired,
    /// The owner's statement or proof was refused.
    Approval(ApprovalRefusal),
    /// The proposal is a create from the named template that does not bind the template's content: one recorded
    /// before the node did. It is refused before the approval is checked, so no approval is spent on it.
    Unbound(String),
    /// The proposals journal could not be read or made durable.
    Store(KernelError),
}

/// The reconciliation and approved plan run of one atomic service removal.
pub struct ServiceRemoval {
    pub outcomes: Vec<Outcome>,
    /// The entries this removal changed: the removed entry and every service whose links it revoked.
    pub changed: BTreeSet<EntryId>,
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
    /// A function of the same project links to the service: its names. Nothing was proposed and nothing changed. A
    /// function's links are part of the definition its owner approved, so the function is removed first.
    LinkedFunctions(Vec<String>),
}

impl Kernel {
    /// Opens the kernel over `state_dir`. The store takes its lock first, so a second kernel over the same directory is refused.
    /// The journal is opened and its intents settled, then the store's current document is reconciled once, so the delete plans
    /// it implies are waiting again. `provider` builds the provider from the retained set the reconciler shares with it,
    /// `postgres_image` is the kernel's configured pinned default (the authority every replayed proposal is checked against),
    /// and `now_ms` is the clock every check of the start reads.
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        state_dir: &Path,
        key: NodeKey,
        plugins: PluginSet,
        verifier: ApprovalVerifier<OwnerKeys, FileNonces>,
        host: String,
        postgres_image: Option<&str>,
        provider: impl FnOnce(RetainedSet) -> Result<Box<dyn Provider + Send>, KernelError>,
        now_ms: u64,
    ) -> Result<Self, KernelError> {
        Self::open_with_tenants(
            state_dir,
            key,
            plugins,
            verifier,
            host,
            postgres_image,
            provider,
            TenantProjects::default(),
            now_ms,
        )
    }

    /// As [`Kernel::open`], keeping `tenants`: the set of tenant projects the provider admits, which the caller also gave
    /// the provider. It holds the project of each tenant whose entry is in the desired state or still has an effect in
    /// the journal, from before the journal's intents are settled and after every change of either.
    #[allow(clippy::too_many_arguments)]
    pub fn open_with_tenants(
        state_dir: &Path,
        key: NodeKey,
        plugins: PluginSet,
        verifier: ApprovalVerifier<OwnerKeys, FileNonces>,
        host: String,
        postgres_image: Option<&str>,
        provider: impl FnOnce(RetainedSet) -> Result<Box<dyn Provider + Send>, KernelError>,
        tenants: TenantProjects,
        now_ms: u64,
    ) -> Result<Self, KernelError> {
        let mut store = DesiredStore::open(state_dir, key)?;
        let action_proposals = ActionProposalStore::open(state_dir, now_ms, postgres_image);
        let retained = RetainedSet::new(store.retained().clone());
        let provider = provider(retained.clone())?;
        let journal = Journal::open(FileJournal::open(&state_dir.join(JOURNAL_FILE))?)?;
        {
            let mut held: BTreeSet<EntryId> = journal.live_entries().into_iter().collect();
            held.extend(
                store
                    .current()
                    .into_iter()
                    .flat_map(|signed| signed.document.entries.iter())
                    .map(|entry| entry.id.clone()),
            );
            tenants.set(&held);
        }
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
            let outcomes = reconciler.reconcile(&signed, now_ms);
            for outcome in outcomes {
                if let Outcome::Failed { entry, why } = outcome {
                    eprintln!("arca: startup reconciliation of {} failed: {why}", entry.0);
                }
            }
        }
        let converged = reconciler.converged();
        let retained_entries = retained.entries().map_err(retained_failure)?;
        store.save_applied(converged, retained_entries)?;
        let applied_seq = AppliedSeq::default();
        applied_seq.publish(store.applied().map(|applied| applied.seq));
        Ok(Kernel {
            store,
            applied_seq,
            reconciler,
            retained,
            tenants,
            writes: Arc::default(),
            verifier,
            host,
            action_proposals,
            action_grants: Vec::new(),
        })
    }

    /// The sequence of the desired-state document the gate last applied, if any.
    pub fn applied_seq(&self) -> Option<u64> {
        self.applied_seq.load()
    }

    /// A reader that stays current while another caller holds the kernel lock.
    pub fn applied_seq_reader(&self) -> AppliedSeq {
        self.applied_seq.clone()
    }

    /// The entries in the current desired-state document, in application order.
    pub fn entries(&self) -> &[arca_kernel::desired::Entry] {
        self.store
            .current()
            .map_or(&[], |s| s.document.entries.as_slice())
    }

    /// Identity from the newest live class-B instance effect, never from the desired record or a fresh Incus listing.
    pub fn service_uuid(&self, entry: &EntryId, instance: &str) -> Option<String> {
        self.service_object(entry, instance)
            .and_then(|object| object.uuid)
    }

    /// The instance `instance` that `entry` created, as the effect journal records it: its project and its uuid. This
    /// is Arca's own record of where the instance is; a caller's word for the project is never used in its place.
    pub fn service_object(
        &self,
        entry: &EntryId,
        instance: &str,
    ) -> Option<arca_kernel::ownership::ObjectRef> {
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
                    && (o.project == arca_netproject::PROJECT
                        || arca_kernel::ownership::tenant_of_project(&o.project).is_some())
            })
    }

    pub fn retained_entries(
        &self,
    ) -> Result<
        std::collections::BTreeMap<EntryId, (arca_kernel::desired::PluginId, Digest)>,
        KernelError,
    > {
        self.retained.entries().map_err(retained_failure)
    }

    /// The gate of this kernel's writing handlers. It is taken before the kernel's own lock, never after it.
    pub fn write_gate(&self) -> Arc<std::sync::Mutex<()>> {
        Arc::clone(&self.writes)
    }

    /// The host name every approval statement of this kernel names.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The delete plans waiting for an owner's approval, in the order the reconciler holds them.
    pub fn pending_plans(&self) -> Vec<DeletePlan> {
        self.reconciler.pending_plans()
    }

    /// The action proposals still on disk. Callers leave out the lapsed ones.
    pub fn action_proposals(&self) -> &[ActionProposal] {
        self.action_proposals.waiting()
    }

    /// Records an inert action proposal. The action's arguments have already been checked against its verb's schema. A
    /// ninth unexpired proposal is refused, and the same digest returns the record that already waits.
    pub fn propose_action(
        &mut self,
        action: ActionKind,
        args: Value,
        now_ms: u64,
        gpu_product: Option<&str>,
    ) -> Result<ActionProposal, ProposeError> {
        self.propose_action_using(action, args, now_ms, gpu_product, None)
    }

    /// The same, with what a create takes of the node, read from the template the proposal binds; the proposal keeps it.
    pub fn propose_action_using(
        &mut self,
        action: ActionKind,
        args: Value,
        now_ms: u64,
        gpu_product: Option<&str>,
        uses: Option<String>,
    ) -> Result<ActionProposal, ProposeError> {
        self.action_proposals
            .propose_using(action, args, now_ms, gpu_product, uses)
    }

    /// Declines one waiting action proposal, whatever its age. Returns whether one waited under `id`.
    pub fn decline_action(&mut self, id: &str) -> Result<bool, KernelError> {
        self.action_proposals
            .decline(id)
            .map(|proposal| proposal.is_some())
            .map_err(|error| KernelError::Io {
                path: ACTION_PROPOSALS_FILE.to_string(),
                why: error.to_string(),
            })
    }

    /// Checks the owner's approval for the action proposal `id`, spends its nonce, retires the proposal and arms one
    /// execution grant. Nothing runs here: the caller runs the action after the lock is released, then journals its result.
    pub fn approve_action(
        &mut self,
        id: &str,
        approval: &str,
        now_ms: u64,
    ) -> Result<ApprovedAction, ActionApprovalError> {
        self.approve_action_proved(id, Some(approval), None, now_ms)
    }

    /// Approves the action proposal `id` in the terminal of the admin token `token`, with no passkey proof: the same checks
    /// as [`Kernel::approve_action`] but the owner verifier's, and the journal says who approved it and where. That is a
    /// client's answer, never the owner's passkey, and the journal never says "by the owner".
    pub fn approve_action_in_terminal(
        &mut self,
        id: &str,
        token: &str,
        now_ms: u64,
    ) -> Result<ApprovedAction, ActionApprovalError> {
        self.approve_action_proved(id, None, Some(token), now_ms)
    }

    fn approve_action_proved(
        &mut self,
        id: &str,
        approval: Option<&str>,
        terminal_token: Option<&str>,
        now_ms: u64,
    ) -> Result<ApprovedAction, ActionApprovalError> {
        let proposal = self
            .action_proposals
            .get(id)
            .cloned()
            .ok_or(ActionApprovalError::NoSuch)?;
        if proposal.expired(now_ms) {
            return Err(ActionApprovalError::Expired);
        }
        let action = ActionKind::from_name(&proposal.action).ok_or_else(|| {
            ActionApprovalError::Store(KernelError::Io {
                path: ACTION_PROPOSALS_FILE.to_string(),
                why: "the waiting proposal names an action this build does not allow".to_string(),
            })
        })?;
        let digest = action_args_digest(&proposal.action, &proposal.args).ok_or_else(|| {
            ActionApprovalError::Store(KernelError::Io {
                path: ACTION_PROPOSALS_FILE.to_string(),
                why: "the waiting proposal's arguments do not canonicalize".to_string(),
            })
        })?;
        if digest != proposal.args_digest {
            return Err(ActionApprovalError::Store(KernelError::Io {
                path: ACTION_PROPOSALS_FILE.to_string(),
                why: "the waiting proposal's digest does not match its arguments".to_string(),
            }));
        }
        // A create that binds no template content can never run: it is refused here, before the owner's approval
        // is checked and its nonce spent.
        if let Some(template) = action.unbound_template(&proposal.args) {
            return Err(ActionApprovalError::Unbound(template.to_string()));
        }
        let statement_action = ArcaAction::from_name(&proposal.action).ok_or_else(|| {
            ActionApprovalError::Store(KernelError::Io {
                path: ACTION_PROPOSALS_FILE.to_string(),
                why: "the waiting proposal names an action the verifier does not know".to_string(),
            })
        })?;
        let grant = fresh_grant().ok_or_else(|| {
            ActionApprovalError::Store(KernelError::Io {
                path: ACTION_PROPOSALS_FILE.to_string(),
                why: "a one-shot execution grant could not be generated".to_string(),
            })
        })?;
        let payload = json!({ "action": proposal.action.clone(), "args": proposal.args.clone() });
        if let Some(approval) = approval {
            self.verifier
                .check(statement_action, id, &payload, approval, now_ms)
                .map_err(ActionApprovalError::Approval)?;
        }
        let by =
            terminal_token.map(|token| format!("approved in the terminal by admin token {token}"));
        self.action_proposals
            .approve_noting(id, by.as_deref())
            .map_err(|error| {
                ActionApprovalError::Store(KernelError::Io {
                    path: ACTION_PROPOSALS_FILE.to_string(),
                    why: error.to_string(),
                })
            })?
            .ok_or(ActionApprovalError::NoSuch)?;
        self.action_grants.push(ActionGrant {
            id: proposal.id.clone(),
            action: action.as_str().to_string(),
            args_digest: digest,
            token: grant.clone(),
        });
        Ok(ApprovedAction {
            id: proposal.id,
            action: action.as_str().to_string(),
            args: proposal.args,
            grant,
        })
    }

    /// Consumes the one-shot grant that lets an approved action's own handler skip its inline approval. The handler's
    /// arguments may still carry the grant under `approval`; that key is not part of the digest.
    pub fn consume_action_grant(&mut self, action: &str, args: &Value, token: &str) -> bool {
        let mut bare = args.clone();
        if let Some(object) = bare.as_object_mut() {
            object.remove("approval");
        }
        let Some(digest) = action_args_digest(action, &bare) else {
            return false;
        };
        let Some(index) = self.action_grants.iter().position(|grant| {
            grant.token == token && grant.action == action && grant.args_digest == digest
        }) else {
            return false;
        };
        self.action_grants.remove(index);
        true
    }

    /// The approved proposal a live grant belongs to.
    pub fn action_grant_id(&self, token: &str) -> Option<String> {
        self.action_grants
            .iter()
            .find(|grant| grant.token == token)
            .map(|grant| grant.id.clone())
    }

    /// Journals that an approved action which had already answered failed later, in work it left running (a snapshot copy
    /// finished in the background). The newest result for the id is this failure.
    pub fn journal_late_failure(&mut self, id: &str, code: &str) {
        self.journal_late_result(id, false, code);
    }

    /// Journals how work an approved action left running ended (a folder snapshot copied in the background): `ok` with
    /// `SnapshotCopied`, or a failure code. The newest result for the id is this one.
    pub fn journal_late_result(&mut self, id: &str, ok: bool, code: &str) {
        if let Err(error) = self
            .action_proposals
            .journal_result(id, ok, Some(code), None)
        {
            eprintln!("arca: the late result of action proposal {id} was not journalled: {error}");
        }
    }

    /// Drops a grant that was never consumed, so no later call can use it.
    pub fn revoke_action_grant(&mut self, token: &str) {
        self.action_grants.retain(|grant| grant.token != token);
    }

    /// Journals whether one approved action ran. Called after the action handler returns, so the record is its result.
    pub fn journal_action_result(
        &mut self,
        approved: &ApprovedAction,
        ok: bool,
        code: Option<&str>,
        message: Option<&str>,
    ) {
        if let Err(error) = self
            .action_proposals
            .journal_result(&approved.id, ok, code, message)
        {
            eprintln!(
                "arca: the result of action proposal {} was not journalled: {error}",
                approved.id
            );
        }
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
        let assertion = parse_proof(proof)?;
        // Only this kernel's own passkeys (the root file and its state file) may change its owner trust. A passkey trusted
        // read-only from another kernel's file, or kept in sync from another machine's page host, approves actions only: it is refused here with a plain reason, and any other
        // unknown passkey with the own-keys check's own refusal, before any nonce or count is spent or anything is written.
        // The verifier puts own lines before trusted ones, so the check below verifies against the own line found here.
        if !self
            .owner_passkey_lines()
            .iter()
            .any(|own| own.credential_id == assertion.credential_id)
        {
            let owner = self.verifier.verifier();
            let peer: Vec<_> = owner
                .peers()
                .iter()
                .flat_map(|peer| peer.load().0)
                .collect();
            if owner
                .foreign()
                .load()
                .0
                .iter()
                .chain(peer.iter())
                .any(|line| line.credential_id == assertion.credential_id)
            {
                return Err(ApprovalRefusal::Untrusted(
                    "only this machine's own passkeys can change its passkeys; a passkey trusted from another kernel cannot"
                        .into(),
                ));
            }
            return Err(
                match owner.verify_passkey(
                    arca_approval::ARCA_NAMESPACE,
                    statement.as_bytes(),
                    proof,
                ) {
                    Err(refusal) => refusal,
                    Ok(_) => ApprovalRefusal::BadSignature,
                },
            );
        }
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

    /// `peer_keys_sync`: keeps the owner passkeys of the page host `approval.peerKeys.from` names in the peer passkeys file.
    ///
    /// `lines` is the page host's whole set. While the file holds no line, the first sync is kept as is (trust on first use:
    /// the operator enabled it by turning `peerKeys` on). A set equal to the kept one changes nothing. Any other set is kept
    /// only with `statement` and `proof`: a passkey approval of a statement naming this host, the action `peer_keys_set` and
    /// the new set's digest, signed by a passkey already kept in the file, checked and spent by this kernel's own verifier
    /// (nonce, window, sign count). Without them the answer is the statement to sign, and nothing is written. An empty set is
    /// refused: the last key cannot be removed. The caller holds the kernel lock across the check and the write.
    ///
    /// This is the first source's sync: [`Kernel::peer_keys_sync_from`] with no `from`.
    pub fn peer_keys_sync(
        &mut self,
        lines: &[String],
        signed: Option<(&str, &[u8])>,
        now_ms: u64,
    ) -> Result<PeerSync, ApprovalRefusal> {
        self.peer_keys_sync_from(None, lines, signed, now_ms)
    }

    /// [`Kernel::peer_keys_sync`] for the source whose page host is `from`; `None` is the first source. Each source keeps
    /// its own file and its own set: a changed set is kept only with an approval by a passkey already kept for that same
    /// source, verified with that source's own key. A further source's statement also binds its `from`, so an approval of
    /// one source's set is never one of another's.
    pub fn peer_keys_sync_from(
        &mut self,
        from: Option<&str>,
        lines: &[String],
        signed: Option<(&str, &[u8])>,
        now_ms: u64,
    ) -> Result<PeerSync, ApprovalRefusal> {
        use arca_approval::{normalize_set, parse_proof, parse_statement, peer_target, set_digest};
        use base64::{Engine as _, engine::general_purpose::STANDARD};

        let sources = self.verifier.verifier().peers();
        let several = sources.len() > 1;
        if sources.is_empty() {
            return Err(ApprovalRefusal::Untrusted(
                "this machine does not keep another machine's passkeys".into(),
            ));
        }
        let index = match from {
            None => 0,
            Some(name) => sources
                .iter()
                .position(|peer| peer.from().eq_ignore_ascii_case(name))
                .ok_or_else(|| {
                    ApprovalRefusal::Untrusted(
                        "this machine does not keep that page host's passkeys".into(),
                    )
                })?,
        };
        let peer = sources[index].clone();
        let next = normalize_set(lines)?;
        if next.is_empty() {
            return Err(ApprovalRefusal::Untrusted(
                "the last trusted passkey cannot be removed".into(),
            ));
        }
        let kept = peer.current()?;
        let keys = next.len();
        if kept.lines.is_empty() {
            peer.write(&next, now_ms, None)
                .map_err(|_| peer_storage())?;
            peer.load();
            return Ok(PeerSync::Trusted { keys });
        }
        if kept.lines == next {
            return Ok(PeerSync::Unchanged { keys });
        }
        let digest = set_digest(&next);
        let target = peer_target(&digest);
        // The first source's statement is what it has always been; a further source's names its page host too.
        let args = if index == 0 {
            json!({ "sha256": digest })
        } else {
            json!({ "sha256": digest, "from": peer.from() })
        };
        let Some((statement, proof)) = signed else {
            return Ok(PeerSync::NeedsApproval {
                keys,
                statement: peer_statement(&self.host, &target, &args, now_ms)?,
            });
        };
        let parsed = parse_statement(statement.as_bytes())?;
        if parsed.action != ArcaAction::PeerKeysSet {
            return Err(ApprovalRefusal::WrongAction);
        }
        // Only a passkey already kept from the page host may change the set: this kernel's own passkeys and other kernels'
        // files cannot, and an SSH signature is not a passkey proof.
        let assertion = parse_proof(proof)?;
        let (trusted, _) = peer.load();
        let Some(kept_line) = trusted
            .iter()
            .find(|line| line.credential_id == assertion.credential_id)
        else {
            return Err(ApprovalRefusal::BadSignature);
        };
        let approval = format!("{}.{}", STANDARD.encode(statement), STANDARD.encode(proof));
        // With several sources, the key that verifies must be this source's own line: a line another source sent under the
        // same credential id never stands in for it. Checked before anything is spent.
        if several {
            let spent = self.verifier.preview(
                ArcaAction::PeerKeysSet,
                &target,
                &args,
                &approval,
                now_ms,
            )?;
            if spent.key != format!("passkey:{}", kept_line.fingerprint) {
                return Err(ApprovalRefusal::BadSignature);
            }
        }
        self.verifier
            .check(ArcaAction::PeerKeysSet, &target, &args, &approval, now_ms)?;
        peer.write(&next, kept.since.unwrap_or(now_ms), Some(now_ms))
            .map_err(|_| peer_storage())?;
        peer.load();
        Ok(PeerSync::Changed { keys })
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
        self.applied_seq.publish(Some(signed.document.seq));
        // A tenant the document adds is in the family before its entry applies; one it removes stays until its removal
        // has left nothing in the journal.
        self.refresh_tenants();
        let outcomes = self.reconciler.reconcile(&signed, now_ms);
        self.refresh_tenants();
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

    /// Checks an owner approval exactly as [`Kernel::check_approval`] would, and spends nothing: no nonce, no sign count. The
    /// approval page approves an owner approval request with this, and the requester's call spends the approval later.
    pub fn preview_approval(
        &self,
        action: ArcaAction,
        target: &str,
        args: &serde_json::Value,
        approval: &str,
        now_ms: u64,
    ) -> Result<arca_approval::Spent, ApprovalRefusal> {
        self.verifier
            .preview(action, target, args, approval, now_ms)
    }

    /// Why every passkey approval is refused, when the owner's passkeys file could not be trusted or read.
    pub fn owner_passkey_refusal(&self) -> Option<ApprovalRefusal> {
        self.verifier.verifier().passkey_refusal().cloned()
    }

    /// Whether an approval's nonce is spent.
    pub fn approval_nonce_spent(&self, nonce: &str) -> bool {
        self.verifier.nonce_spent(nonce)
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
        let spent =
            self.spend_service_removal(entry, name, &json!({ "name": name }), approval, now_ms)?;
        self.remove_service_spent(spent, now_ms)
    }

    /// Checks and spends the owner's approval of a removal: a `delete` over `target` with the arguments `args`. For a
    /// service of the node's own project those are its name and `{"name": name}`.
    pub fn spend_service_removal(
        &mut self,
        entry: &EntryId,
        target: &str,
        args: &Value,
        approval: &str,
        now_ms: u64,
    ) -> Result<SpentServiceRemoval, ServiceRemovalError> {
        self.check_approval(ArcaAction::Delete, target, args, approval, now_ms)
            .map_err(ServiceRemovalError::Approval)?;
        Ok(SpentServiceRemoval {
            entry: entry.clone(),
        })
    }

    /// The same spent removal as [`Kernel::spend_service_removal`], for an action whose owner approval was already checked
    /// against its proposal and whose one-shot grant the caller consumed.
    pub fn spend_service_removal_authorized(&mut self, entry: &EntryId) -> SpentServiceRemoval {
        SpentServiceRemoval {
            entry: entry.clone(),
        }
    }

    pub fn remove_service_spent(
        &mut self,
        spent: SpentServiceRemoval,
        now_ms: u64,
    ) -> Result<ServiceRemoval, ServiceRemovalError> {
        let entry = &spent.entry;
        // The removed service's name and project, from its record: a link is revoked only among the services of one
        // project, so a removal in one tenant never touches a service of the same name elsewhere.
        let removed = self
            .entries()
            .iter()
            .find(|found| &found.id == entry)
            .and_then(|found| {
                serde_json::from_value::<crate::service::ServiceConfig>(found.config.clone()).ok()
            });
        let name = removed.as_ref().map_or_else(
            || entry.0.strip_prefix("service-").unwrap_or("").to_string(),
            |record| record.name.clone(),
        );
        let name = name.as_str();
        let project = removed.map(|record| record.project);
        // A service a function links to is not removed while the function exists. The check is made here, on the
        // entries the removal itself changes and under the lock that changes them, so no path and no order of two
        // approved actions gets round it: a function's record with a link to nothing is one the plugin refuses.
        let linked: Vec<String> = self
            .entries()
            .iter()
            .filter(|other| &other.id != entry && other.plugin.0 == "arca.service")
            .filter_map(|other| {
                serde_json::from_value::<crate::service::ServiceConfig>(other.config.clone()).ok()
            })
            .filter(|cfg| {
                cfg.function.is_some()
                    && project
                        .as_ref()
                        .is_none_or(|project| *project == cfg.project)
                    && cfg.links.iter().any(|link| link.to == name)
            })
            .map(|cfg| cfg.name)
            .collect();
        if !linked.is_empty() {
            return Err(ServiceRemovalError::LinkedFunctions(linked));
        }
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
            if cfg.adopted
                || project
                    .as_ref()
                    .is_some_and(|project| *project != cfg.project)
            {
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
            .collect::<BTreeSet<_>>();
        changes.push(EntryChange::Remove(entry.clone()));
        let mut changed = linked.clone();
        changed.insert(entry.clone());
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
                changed,
                run: Ok(None),
            });
        }
        let run = self
            .pending_plans()
            .into_iter()
            .find(|plan| &plan.entry == entry)
            .map(|plan| self.run_plan_authorized(&plan.digest, now_ms))
            .transpose();
        Ok(ServiceRemoval {
            outcomes,
            changed,
            run,
        })
    }

    /// Runs a delete plan whose owner approval was checked and spent by the caller.
    pub fn run_plan_authorized(
        &mut self,
        plan: &Digest,
        now_ms: u64,
    ) -> Result<arca_reconcile::PlanRun, ApprovalRefusal> {
        let run = self.reconciler.run_plan_authorized(plan, now_ms);
        self.refresh_tenants();
        run
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
        let run = self
            .reconciler
            .run_plan(plan, approval, &mut self.verifier, now_ms)
            .map(Some);
        self.refresh_tenants();
        run
    }

    /// Sets the tenant projects the provider admits: those of the tenant entries in the desired state, and of the ones
    /// the journal still holds an effect for.
    fn refresh_tenants(&mut self) {
        let mut held: BTreeSet<EntryId> = self
            .reconciler
            .journal()
            .live_entries()
            .into_iter()
            .collect();
        held.extend(self.entries().iter().map(|entry| entry.id.clone()));
        self.tenants.set(&held);
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

/// A random one-shot token for an approved action. It is not the owner's approval and carries no secret.
fn fresh_grant() -> Option<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).ok()?;
    Some(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// What `peer_keys_sync` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerSync {
    /// The file held no passkey, so this first set is kept as is.
    Trusted { keys: usize },
    /// The set is the one already kept; nothing was written.
    Unchanged { keys: usize },
    /// The set differs and carried no approval: nothing was written, and `statement` is what the owner signs to keep it.
    NeedsApproval { keys: usize, statement: String },
    /// The set differs and its approval held: it is kept now.
    Changed { keys: usize },
}

fn peer_storage() -> ApprovalRefusal {
    ApprovalRefusal::Storage("the peer passkeys file could not be saved".into())
}

/// A fresh `peer_keys_set` statement naming `host`, the set's `target` and its digest `args`, valid for ten minutes.
fn peer_statement(
    host: &str,
    target: &str,
    args: &Value,
    now: u64,
) -> Result<String, ApprovalRefusal> {
    use arca_kernel::canon::{canonical_json, params_digest};
    let malformed = || ApprovalRefusal::Malformed("the peer passkey statement is invalid".into());
    let nonce = crate::handlers::fresh_nonce().map_err(|_| peer_storage())?;
    let text = canonical_json(&json!({
        "action": ArcaAction::PeerKeysSet.as_str(),
        "host": host,
        "target": target,
        "paramsDigest": params_digest(args).map_err(|_| malformed())?,
        "issuedAt": crate::handlers::iso_utc(now),
        "expiresAt": crate::handlers::iso_utc(now.saturating_add(600_000)),
        "nonce": nonce,
    }))
    .map_err(|_| malformed())?;
    arca_approval::parse_statement(text.as_bytes())?;
    Ok(text)
}
