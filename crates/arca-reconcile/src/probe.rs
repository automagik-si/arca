//! The probe harness: the one path a live probe writes and removes objects by. Each create runs through [`Ctx::effect`], so it is
//! journalled as an intent before it is sent and as its outcome after. Removal goes through the reconciler's own journalled
//! retirement: a class-A object reverts at once, and a class-B object is deleted only under an approval that the verifier checks.
//! A kill at any point leaves a journal record for every object the probe made, and [`Reconciler::open`] settles those records
//! on restart.

use std::fmt;

use arca_approval::{ApprovalRefusal, ApprovalVerifier, NonceStore, SignatureVerifier};
use arca_kernel::canon::Digest;
use arca_kernel::desired::{EntryId, PluginId};
use arca_kernel::journal::{JournalStore, removal};
use arca_kernel::ownership::{IncusKind, ObjectRef};
use arca_kernel::plugin::{Ctx, EffectOp, EffectRefusal, Manifest};
use arca_providers::incus::{NETWORK_ACCESS_KEY, NETWORK_HOME};
use serde_json::{Value, json};

use crate::{BLOCKED_WHY, Gate, Pending, PlanRun, Reconciler, same_place};

/// The plugin id the probe's journal records name.
pub const PROBE_PLUGIN: &str = "probe.scratch";

/// The bridge the probe makes in the default project. The probe's project grants it to the probe's instance through
/// `restricted.networks.access`, and the probe's forward hangs off it.
pub const PROBE_BRIDGE: &str = "probe-net";

/// The image alias the probe's instance is made from when a spec names none. It is a small Alpine root filesystem. The kernel
/// imports no image: the host imports the alias into the store the probe's project reads before a live probe runs. A spec may name
/// another alias, such as a busybox one.
pub const DEFAULT_PROBE_IMAGE: &str = "alpine/edge";

/// Where a probe writes: its entry, its scratch project, the bridge's IPv4 range, the forward's listen address inside that range,
/// the storage pool its volume, bucket and root disk live in, and the image alias its instance is made from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeSpec {
    pub entry: EntryId,
    pub project: String,
    pub cidr: String,
    pub listen: String,
    pub pool: String,
    pub image: String,
}

/// What one kind's create came to in a probe run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// The object is held: this run made it, or the journal already held it live.
    Created,
    /// The create was refused, with the reason the provider or the kernel gave.
    Refused(String),
    /// The create was not sent, because a kind it needs was not made. The kind names the first one that was not.
    NotAttempted { needs: IncusKind },
    /// An earlier effect has no outcome, so the kernel runs no create until it is settled.
    Blocked,
}

impl fmt::Display for ProbeOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProbeOutcome::Created => f.write_str("ok"),
            ProbeOutcome::Refused(reason) => write!(f, "refused: {reason}"),
            ProbeOutcome::NotAttempted { needs } => {
                write!(f, "not-attempted: needs {}", kind_name(*needs))
            }
            ProbeOutcome::Blocked => f.write_str("not-attempted: an earlier effect is unresolved"),
        }
    }
}

/// One kind's row of a probe run: the object its create names, and what came of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeRow {
    pub kind: IncusKind,
    pub target: ObjectRef,
    pub outcome: ProbeOutcome,
}

/// The name a kind goes by in a probe's table.
pub fn kind_name(kind: IncusKind) -> &'static str {
    match kind {
        IncusKind::Project => "project",
        IncusKind::Instance => "instance",
        IncusKind::Network => "network",
        IncusKind::Acl => "acl",
        IncusKind::Profile => "profile",
        IncusKind::Volume => "volume",
        IncusKind::Zone => "zone",
        IncusKind::Forward => "forward",
        IncusKind::Bucket => "bucket",
    }
}

/// The object an effect op names, for a probe's creates.
fn target_of(op: &EffectOp) -> ObjectRef {
    match op {
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
    }
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
    /// The creates, in the order they are made. The bridge comes first, since the project grants it and the forward hangs off it.
    /// The project comes next, then one object of each remaining kind: the instance from an image, with a root disk on the pool and
    /// a NIC on the bridge.
    pub fn ops(&self) -> Vec<EffectOp> {
        let create =
            |kind: IncusKind, project: &str, parent: Option<&str>, name: &str, body: Value| {
                EffectOp::IncusCreate {
                    kind,
                    project: project.to_string(),
                    parent: parent.map(str::to_string),
                    name: name.to_string(),
                    body,
                }
            };
        vec![
            create(
                IncusKind::Network,
                NETWORK_HOME,
                None,
                PROBE_BRIDGE,
                json!({
                    "name": PROBE_BRIDGE,
                    "type": "bridge",
                    "config": { "ipv4.address": self.cidr, "ipv6.address": "none" },
                }),
            ),
            create(
                IncusKind::Project,
                &self.project,
                None,
                &self.project,
                json!({ "name": self.project, "config": { NETWORK_ACCESS_KEY: PROBE_BRIDGE } }),
            ),
            create(
                IncusKind::Instance,
                &self.project,
                None,
                "probe-box",
                json!({
                    "name": "probe-box",
                    "config": {},
                    "source": { "type": "image", "alias": self.image },
                    "devices": {
                        "root": { "type": "disk", "path": "/", "pool": self.pool },
                        "eth0": { "type": "nic", "network": PROBE_BRIDGE },
                    },
                }),
            ),
            create(
                IncusKind::Acl,
                NETWORK_HOME,
                None,
                "probe-acl",
                json!({ "name": "probe-acl", "config": {} }),
            ),
            create(
                IncusKind::Profile,
                &self.project,
                None,
                "probe-profile",
                json!({ "name": "probe-profile", "config": {} }),
            ),
            create(
                IncusKind::Volume,
                &self.project,
                Some(&self.pool),
                "probe-volume",
                json!({ "name": "probe-volume", "config": {} }),
            ),
            create(
                IncusKind::Zone,
                &self.project,
                None,
                "probe-zone",
                json!({ "name": "probe-zone", "config": {} }),
            ),
            create(
                IncusKind::Forward,
                NETWORK_HOME,
                Some(PROBE_BRIDGE),
                &self.listen,
                json!({ "config": {} }),
            ),
            create(
                IncusKind::Bucket,
                &self.project,
                Some(&self.pool),
                "probe-bucket",
                json!({ "name": "probe-bucket", "config": {} }),
            ),
        ]
    }

    /// The kinds a create of `kind` needs made before it. A bridge needs nothing. The project needs the bridge it grants. An
    /// instance needs its project and its bridge. A forward needs its bridge. Every other kind needs its project, an ACL included:
    /// it is addressed in the default project, but it is the probe project's ACL.
    pub fn needs(kind: IncusKind) -> &'static [IncusKind] {
        match kind {
            IncusKind::Network => &[],
            IncusKind::Project => &[IncusKind::Network],
            IncusKind::Instance => &[IncusKind::Project, IncusKind::Network],
            IncusKind::Forward => &[IncusKind::Network],
            _ => &[IncusKind::Project],
        }
    }

    /// The object each create names, in the order of [`ProbeSpec::ops`].
    pub fn targets(&self) -> Vec<ObjectRef> {
        self.ops().iter().map(target_of).collect()
    }

    /// The manifest the probe's effects run under: the nine kinds it writes, and two projects, its scratch project and the default
    /// project that holds the bridge, its forward and the ACL. The bridge is class B, not A, since the probe's instance attaches to it: a
    /// class-A revert of the bridge would run before the instance is deleted, and Incus refuses to delete a bridge in use. A
    /// declared class that is stricter than the default is not checked by the kernel. The project, the instance, the volume and
    /// the bucket are class B; the rest are class A.
    pub fn manifest(&self) -> Manifest {
        use arca_kernel::journal::EffectClass::{A, B};
        Manifest {
            id: PluginId(PROBE_PLUGIN.to_string()),
            version: "0.1.0".to_string(),
            provides: Vec::new(),
            effects: vec![
                (IncusKind::Project, B),
                (IncusKind::Instance, B),
                (IncusKind::Network, B),
                (IncusKind::Acl, A),
                (IncusKind::Profile, A),
                (IncusKind::Volume, B),
                (IncusKind::Zone, A),
                (IncusKind::Forward, A),
                (IncusKind::Bucket, B),
            ],
            projects: vec![self.project.clone(), NETWORK_HOME.to_string()],
            observes: false,
        }
    }
}

impl<S: JournalStore> Reconciler<S> {
    /// Makes the probe's objects in order, each through [`Ctx::effect`], and records what each kind came to. A refused create is
    /// recorded and the run goes on with the other kinds. A kind whose needed kind was not made is not sent, and is recorded as
    /// [`ProbeOutcome::NotAttempted`]. An object the journal already holds live is not made again, so a probe that a restart
    /// interrupted completes without a second create. One row per kind is returned, in the order of [`ProbeSpec::targets`].
    pub fn probe_create(
        &mut self,
        spec: &ProbeSpec,
        now_ms: u64,
    ) -> Result<Vec<ProbeRow>, ProbeError> {
        if self.blocked() {
            return Err(ProbeError::Effect(EffectRefusal::Provider(
                BLOCKED_WHY.to_string(),
            )));
        }
        let manifest = spec.manifest();
        let mut rows: Vec<ProbeRow> = Vec::new();
        for op in spec.ops() {
            let target = target_of(&op);
            let kind = target.kind;
            let missing = ProbeSpec::needs(kind).iter().copied().find(|needed| {
                !rows
                    .iter()
                    .any(|row| row.kind == *needed && row.outcome == ProbeOutcome::Created)
            });
            let outcome = if let Some(needs) = missing {
                ProbeOutcome::NotAttempted { needs }
            } else if self.blocked() {
                ProbeOutcome::Blocked
            } else {
                self.probe_one(spec, &manifest, op, &target, now_ms)
            };
            rows.push(ProbeRow {
                kind,
                target,
                outcome,
            });
        }
        Ok(rows)
    }

    /// Makes one probe create through [`Ctx::effect`], or finds it already held, and says what came of it.
    fn probe_one(
        &mut self,
        spec: &ProbeSpec,
        manifest: &Manifest,
        op: EffectOp,
        target: &ObjectRef,
        now_ms: u64,
    ) -> ProbeOutcome {
        let held = self
            .journal
            .live(&spec.entry)
            .iter()
            .any(|record| same_place(&record.target, target));
        if held {
            return ProbeOutcome::Created;
        }
        let mut gate = Gate {
            journal: &mut self.journal,
            plugin: manifest.id.clone(),
        };
        let mut ctx = Ctx::new(
            spec.entry.clone(),
            manifest,
            &mut *self.provider,
            &mut gate,
            now_ms,
        );
        match ctx.effect(op) {
            Ok(_) => ProbeOutcome::Created,
            Err(refusal) => ProbeOutcome::Refused(refusal.to_string()),
        }
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
