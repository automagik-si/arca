//! The probe harness: the one path a live probe writes and removes objects by. Each create runs through [`Ctx::effect`], so it is
//! journalled as an intent before it is sent and as its outcome after. Removal goes through the reconciler's own journalled
//! retirement: a class-A object reverts at once, and a class-B object is deleted only under an approval that the verifier checks.
//! A kill at any point leaves a journal record for every object the probe made, and [`Reconciler::open`] settles those records
//! on restart.

use arca_approval::{ApprovalRefusal, ApprovalVerifier, NonceStore, SignatureVerifier};
use arca_kernel::canon::Digest;
use arca_kernel::desired::{EntryId, PluginId};
use arca_kernel::journal::{JournalStore, removal};
use arca_kernel::ownership::{IncusKind, ObjectRef};
use arca_kernel::plugin::{Ctx, EffectOp, EffectRefusal, Manifest};
use serde_json::{Value, json};

use crate::{BLOCKED_WHY, Gate, Pending, PlanRun, Reconciler, same_place};

/// The plugin id the probe's journal records name.
pub const PROBE_PLUGIN: &str = "probe.scratch";

/// Where a probe writes: its entry, its scratch project, the bridge's IPv4 range, the forward's listen address inside that range,
/// and the storage pool its volume and bucket live in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeSpec {
    pub entry: EntryId,
    pub project: String,
    pub cidr: String,
    pub listen: String,
    pub pool: String,
}

/// Why a probe step did not complete.
#[derive(Debug)]
pub enum ProbeError {
    /// A create or a refusal from [`Ctx::effect`].
    Effect(EffectRefusal),
    /// A class-A revert did not finish, with the reason it gave.
    Revert(String),
    /// The removal's approval or run was refused.
    Approval(ApprovalRefusal),
}

impl ProbeSpec {
    /// The creates, in the order they are made: the project first, then one object of each kind inside it. A network comes before
    /// its forward, so the forward's parent exists when it is made.
    pub fn ops(&self) -> Vec<EffectOp> {
        let create = |kind: IncusKind, parent: Option<&str>, name: &str, body: Value| {
            EffectOp::IncusCreate {
                kind,
                project: self.project.clone(),
                parent: parent.map(str::to_string),
                name: name.to_string(),
                body,
            }
        };
        vec![
            create(
                IncusKind::Project,
                None,
                &self.project,
                json!({ "name": self.project, "config": {} }),
            ),
            create(
                IncusKind::Instance,
                None,
                "probe-box",
                json!({ "name": "probe-box", "config": {} }),
            ),
            create(
                IncusKind::Network,
                None,
                "probe-net",
                json!({
                    "name": "probe-net",
                    "type": "bridge",
                    "config": { "ipv4.address": self.cidr, "ipv6.address": "none" },
                }),
            ),
            create(
                IncusKind::Acl,
                None,
                "probe-acl",
                json!({ "name": "probe-acl", "config": {} }),
            ),
            create(
                IncusKind::Profile,
                None,
                "probe-profile",
                json!({ "name": "probe-profile", "config": {} }),
            ),
            create(
                IncusKind::Volume,
                Some(&self.pool),
                "probe-volume",
                json!({ "name": "probe-volume", "config": {} }),
            ),
            create(
                IncusKind::Zone,
                None,
                "probe-zone",
                json!({ "name": "probe-zone", "config": {} }),
            ),
            create(
                IncusKind::Forward,
                Some("probe-net"),
                &self.listen,
                json!({ "config": {} }),
            ),
            create(
                IncusKind::Bucket,
                Some(&self.pool),
                "probe-bucket",
                json!({ "name": "probe-bucket", "config": {} }),
            ),
        ]
    }

    /// The object each create names, in the order of [`ProbeSpec::ops`].
    pub fn targets(&self) -> Vec<ObjectRef> {
        self.ops()
            .iter()
            .map(|op| match op {
                EffectOp::IncusCreate {
                    kind,
                    project,
                    parent,
                    name,
                    ..
                } => ObjectRef {
                    kind: *kind,
                    project: project.clone(),
                    parent: parent.clone(),
                    name: name.clone(),
                    uuid: None,
                },
                _ => unreachable!("the probe makes only creates"),
            })
            .collect()
    }

    /// The manifest the probe's effects run under: the nine kinds it writes, and its one project. A project and the instance,
    /// volume and bucket are class B; the rest are class A.
    pub fn manifest(&self) -> Manifest {
        use arca_kernel::journal::EffectClass::{A, B};
        Manifest {
            id: PluginId(PROBE_PLUGIN.to_string()),
            version: "0.1.0".to_string(),
            provides: Vec::new(),
            effects: vec![
                (IncusKind::Project, B),
                (IncusKind::Instance, B),
                (IncusKind::Network, A),
                (IncusKind::Acl, A),
                (IncusKind::Profile, A),
                (IncusKind::Volume, B),
                (IncusKind::Zone, A),
                (IncusKind::Forward, A),
                (IncusKind::Bucket, B),
            ],
            projects: vec![self.project.clone()],
            observes: false,
        }
    }
}

impl<S: JournalStore> Reconciler<S> {
    /// Makes the probe's objects in order, each through [`Ctx::effect`]. An object the journal already holds live is not made
    /// again, so a probe that a restart interrupted completes without a second create. Returns the objects, in the order of
    /// [`ProbeSpec::targets`].
    pub fn probe_create(
        &mut self,
        spec: &ProbeSpec,
        now_ms: u64,
    ) -> Result<Vec<ObjectRef>, ProbeError> {
        if self.blocked() {
            return Err(ProbeError::Effect(EffectRefusal::Provider(
                BLOCKED_WHY.to_string(),
            )));
        }
        let manifest = spec.manifest();
        let mut made = Vec::new();
        for op in spec.ops() {
            let target = match &op {
                EffectOp::IncusCreate {
                    kind,
                    project,
                    parent,
                    name,
                    ..
                } => ObjectRef {
                    kind: *kind,
                    project: project.clone(),
                    parent: parent.clone(),
                    name: name.clone(),
                    uuid: None,
                },
                _ => unreachable!("the probe makes only creates"),
            };
            let held = self
                .journal
                .live(&spec.entry)
                .iter()
                .any(|record| same_place(&record.target, &target));
            if !held {
                let mut gate = Gate {
                    journal: &mut self.journal,
                    plugin: manifest.id.clone(),
                };
                let mut ctx = Ctx::new(
                    spec.entry.clone(),
                    &manifest,
                    &mut *self.provider,
                    &mut gate,
                    now_ms,
                );
                ctx.effect(op).map_err(ProbeError::Effect)?;
            }
            made.push(target);
        }
        Ok(made)
    }

    /// Removes the probe's objects through the journalled retirement. Class-A objects revert at once, most recent first. The
    /// class-B objects form one plan; `approve` returns the approval over that plan's digest, and the run deletes them under it.
    /// Nothing is removed while an intent is unresolved.
    pub fn probe_remove<V: SignatureVerifier, N: NonceStore>(
        &mut self,
        spec: &ProbeSpec,
        now_ms: u64,
        approve: impl FnOnce(&Digest) -> String,
        verifier: &mut ApprovalVerifier<V, N>,
    ) -> Result<PlanRun, ProbeError> {
        if self.blocked() {
            return Err(ProbeError::Approval(ApprovalRefusal::Unresolved));
        }
        let split = removal(&self.journal, &spec.entry, now_ms);
        for record in &split.revert_now {
            self.revert_a(record, now_ms).map_err(ProbeError::Revert)?;
        }
        let Some(plan) = split.plan else {
            return Ok(PlanRun {
                deleted: Vec::new(),
                drift: Vec::new(),
                partial: false,
            });
        };
        let approval = approve(&plan.digest);
        self.pending
            .retain(|pending| pending.plan.digest != plan.digest);
        self.pending.push(Pending {
            plan: plan.clone(),
            plugin: PluginId(PROBE_PLUGIN.to_string()),
        });
        self.run_plan(&plan.digest, &approval, verifier, now_ms)
            .map_err(ProbeError::Approval)
    }
}
