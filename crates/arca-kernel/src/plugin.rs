//! The plugin contract. A plugin reaches the host only through [`Ctx`]. Every effect it asks for is checked against its
//! manifest, planned read-only by the provider, journalled as an intent, applied, and then journalled with its inverse. The
//! reconciler holds plugins through [`ErasedPlugin`], one object-safe wrapper, so a plugin's own types never leave the crate
//! that defines them.

use std::collections::BTreeMap;
use std::fmt;
use std::marker::PhantomData;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::KernelError;
use crate::canon::{Digest, canonical_json};
use crate::desired::{EntryId, PluginId};
use crate::journal::{AclField, AclPatch, EffectClass, IntentFields, Inverse, JournalSink};
use crate::ownership::{IncusKind, ObjectRef, OwnerClass};

/// A key a plugin provides to other plugins, with the version it provides it at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeySpec {
    pub key: String,
    pub version: String,
}

/// What a plugin may do. Nothing outside the manifest is reachable through [`Ctx`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub id: PluginId,
    pub version: String,
    pub provides: Vec<KeySpec>,
    /// The object kinds the plugin may write, each with the class of effect it may produce for that kind.
    pub effects: Vec<(IncusKind, EffectClass)>,
    /// The projects the plugin may write in. A tenant prefix followed by `*` names every tenant's project
    /// ([`crate::ownership::manifest_names_project`]).
    pub projects: Vec<String>,
    /// Whether the plugin may read the host through [`Ctx::observe`].
    pub observes: bool,
}

/// A read the plugin may make of the host. The provider answers it; a plugin never learns how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observe {
    /// A GET of an Incus path.
    IncusGet { path: String },
    /// Every address of the host's own interfaces.
    HostAddresses,
}

/// One write a plugin asks for. The provider plans it before anything is journalled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "op")]
pub enum EffectOp {
    /// Creates an object. A project is created with `kind` `Project` and `project` equal to `name`. `parent` is the storage
    /// pool of a volume or a bucket and the network of a forward, and the op's JSON omits it for every other kind.
    IncusCreate {
        kind: IncusKind,
        project: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<String>,
        name: String,
        body: Value,
    },
    /// Sets and removes keys in an object's own config. A `None` removes a key.
    IncusPatchConfig {
        object: ObjectRef,
        config: BTreeMap<String, Option<String>>,
    },
    /// Replaces exactly one ACL rule array. The field is serialized as `ingress` or `egress`.
    IncusPatchAclField {
        object: ObjectRef,
        field: AclField,
        rules: Vec<Value>,
    },
    /// Deletes an object, under the approved delete plan named by `plan`. Only the reconciler runs one; [`Ctx::effect`]
    /// refuses it.
    IncusDelete { object: ObjectRef, plan: Digest },
}

impl EffectOp {
    /// The kind, project and name the op addresses. For a create these come from the op; for the others, from the object.
    fn target(&self) -> (IncusKind, &str, &str) {
        match self {
            EffectOp::IncusCreate {
                kind,
                project,
                name,
                ..
            } => (*kind, project, name),
            EffectOp::IncusPatchConfig { object, .. }
            | EffectOp::IncusPatchAclField { object, .. }
            | EffectOp::IncusDelete { object, .. } => (object.kind, &object.project, &object.name),
        }
    }
}

/// The digest an op is journalled under: the digest of its canonical JSON.
pub fn op_digest(op: &EffectOp) -> Digest {
    // An op holds only strings, integers, booleans, nulls, objects and arrays, all of which serialize and canonicalize.
    let value = serde_json::to_value(op).expect("an effect op serializes to JSON");
    let text = canonical_json(&value).expect("an effect op has a canonical form");
    Digest::of(text.as_bytes())
}

/// A provider's read-only answer for an op: the class it would journal under, the digest of the op it planned, the keys a
/// patch would change as they stand now, and the ETag of the object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    /// Previous and sent arrays for an ACL field patch; absent for existing operations.
    pub acl_patch: Option<AclPatch>,
    pub class: EffectClass,
    pub op_digest: Digest,
    pub pre_image: Option<BTreeMap<String, Option<String>>>,
    pub etag: Option<String>,
}

/// Why a write or a read was refused. Nothing is journalled for a refusal that comes before the intent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EffectRefusal {
    /// The plugin declares no effects.
    WatchOnly,
    /// The manifest does not declare this kind, class, or read.
    NotDeclared,
    /// The manifest does not name this project.
    ProjectNotAllowed,
    /// The object belongs to another owner.
    NotOwned(OwnerClass),
    /// The object changed between the plan and the effect.
    Stale,
    /// A reason the provider or the kernel gives, as text.
    Provider(String),
    /// The provider cannot tell whether its write landed. The intent stays open and is not journalled as failed.
    Unresolved,
    /// The write landed and the object is the entry's own, but the request did not finish: an instance that was created
    /// and then failed to start. [`Ctx::effect`] journals the effect as applied, so no intent stays open and a removal
    /// deletes the object, and hands the plugin `why` as a [`EffectRefusal::Provider`] refusal. No plugin ever sees this
    /// variant.
    Landed(Box<Landed>),
}

/// A write that landed without finishing: the object it made, the inverse that undoes it, and why it did not finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Landed {
    pub object: ObjectRef,
    pub inverse: Inverse,
    pub why: String,
}

impl fmt::Display for EffectRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WatchOnly => f.write_str("the plugin declares no effects"),
            Self::NotDeclared => f.write_str("the manifest does not declare it"),
            Self::ProjectNotAllowed => f.write_str("the manifest does not name the project"),
            Self::NotOwned(_) => f.write_str("the object belongs to another owner"),
            Self::Stale => f.write_str("the object changed after it was planned"),
            Self::Provider(why) => f.write_str(why),
            Self::Unresolved => f.write_str("the outcome is not known; the intent stays open"),
            Self::Landed(landed) => f.write_str(&landed.why),
        }
    }
}

impl std::error::Error for EffectRefusal {}

/// Why a plugin's apply failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginError {
    /// The config does not deserialize. The text is serde's, with the rejected values removed.
    Config(String),
    /// A call on [`Ctx`] was refused.
    Refused(EffectRefusal),
    /// The plugin failed for its own reason, given as text.
    Failed(String),
}

impl From<EffectRefusal> for PluginError {
    fn from(refusal: EffectRefusal) -> Self {
        PluginError::Refused(refusal)
    }
}

impl fmt::Display for PluginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(why) => write!(f, "the config does not deserialize: {why}"),
            Self::Refused(refusal) => write!(f, "refused: {refusal}"),
            Self::Failed(why) => write!(f, "failed: {why}"),
        }
    }
}

impl std::error::Error for PluginError {}

/// The host side a [`Ctx`] calls. `plan` and `observe` read only; `effect` is the one call that writes, and it runs only
/// after the intent is journalled.
pub trait Provider {
    /// Answers a read the manifest allows. `projects` are the projects the manifest names, so a provider can scope its reads
    /// to them.
    fn observe(
        &self,
        entry: &EntryId,
        projects: &[String],
        query: &Observe,
    ) -> Result<Value, EffectRefusal>;
    /// Plans an op without writing. A refusal here leaves no journal record.
    fn plan(&self, entry: &EntryId, op: &EffectOp) -> Result<Planned, EffectRefusal>;
    /// Applies a planned op and returns the object it touched with the inverse that undoes it. The inverse is built now, at
    /// the moment of application. For a patch it is the pre-image from `planned`.
    fn effect(
        &mut self,
        entry: &EntryId,
        op: &EffectOp,
        planned: &Planned,
    ) -> Result<(ObjectRef, Inverse), EffectRefusal>;
    /// Reads the object an unresolved intent names, to settle the intent after a restart. The read is a GET and nothing else. A
    /// read that cannot be answered is an error, and the intent then stays open. A provider that cannot read back keeps the
    /// default, which refuses, so every intent stays open.
    fn read_back(
        &self,
        entry: &EntryId,
        target: &ObjectRef,
        pre_image: Option<&BTreeMap<String, Option<String>>>,
    ) -> Result<ReadBack, EffectRefusal> {
        let _ = (entry, target, pre_image);
        Err(EffectRefusal::NotDeclared)
    }
    /// Reads back an ACL array patch after a restart. Unknown or differing values keep the intent open.
    fn read_back_acl(
        &self,
        entry: &EntryId,
        target: &ObjectRef,
        patch: &AclPatch,
    ) -> Result<ReadBack, EffectRefusal> {
        let _ = (entry, target, patch);
        Err(EffectRefusal::NotDeclared)
    }
    /// Deletes an object the entry owns, with `plan` as the digest its tombstone carries. Only the reconciler calls this:
    /// a class-B delete runs after an approval over `plan`, and a class-A revert runs at once with the digest of the create it
    /// undoes. A provider that never deletes keeps this default, which refuses.
    fn retire(
        &mut self,
        entry: &EntryId,
        object: &ObjectRef,
        plan: &Digest,
    ) -> Result<Retirement, EffectRefusal> {
        let _ = (entry, object, plan);
        Err(EffectRefusal::NotDeclared)
    }
}

/// What an object shows when an unresolved intent is settled after a restart. It comes from a read only. `Absent` is an object the
/// host does not hold at the intent's place. `Present` carries the object as Incus names it, with its uuid when its kind has one,
/// and says whether the entry may change it and whether its config holds the intent's pre-image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadBack {
    Absent,
    Present {
        object: ObjectRef,
        owned: bool,
        holds_pre_image: bool,
    },
}

/// What a retirement did: the object was deleted, or the delete was skipped because the object no longer showed what the plan
/// approved. The text of a drift names the rule that did not hold and never a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Retirement {
    Deleted(ObjectRef),
    Drift(String),
}

/// What a plugin's apply runs against: its entry, its manifest, the provider, the journal and the clock.
pub struct Ctx<'a> {
    entry: EntryId,
    manifest: &'a Manifest,
    provider: &'a mut dyn Provider,
    journal: &'a mut dyn JournalSink,
    now_ms: u64,
    provided: BTreeMap<String, Value>,
}

impl<'a> Ctx<'a> {
    /// A context for one apply of the plugin whose manifest is `manifest`, for the entry `entry`.
    pub fn new(
        entry: EntryId,
        manifest: &'a Manifest,
        provider: &'a mut dyn Provider,
        journal: &'a mut dyn JournalSink,
        now_ms: u64,
    ) -> Self {
        Ctx {
            entry,
            manifest,
            provider,
            journal,
            now_ms,
            provided: BTreeMap::new(),
        }
    }

    /// The entry this apply is for.
    pub fn entry(&self) -> &EntryId {
        &self.entry
    }

    /// Reads the host. Refused unless the manifest sets `observes`.
    pub fn observe(&self, query: Observe) -> Result<Value, EffectRefusal> {
        if !self.manifest.observes {
            return Err(EffectRefusal::NotDeclared);
        }
        self.provider
            .observe(&self.entry, &self.manifest.projects, &query)
    }

    /// Asks for one write. The checks run in this order, and a refusal before the intent leaves no record:
    ///
    /// 1. A manifest with no effects refuses with `WatchOnly`.
    /// 2. A delete is refused with `NotDeclared`: it runs only through the reconciler's approved plan.
    /// 3. The op's kind must be declared, and its project must be named.
    /// 4. The provider plans the op read-only. A plan that names another op, a class the manifest does not declare for the
    ///    kind, or a pre-image that does not fit the op (none for a create; one entry per patched key for a patch), is refused.
    /// 5. The intent is journalled with the op's target and the planned pre-image, and only then does the provider apply it.
    /// 6. The applied object and its inverse are journalled. The inverse must be the one the op requires: a create's delete of
    ///    the object it made, with its uuid, or a patch's restore of the planned pre-image. A provider that refuses has its
    ///    refusal journalled as `Failed`.
    ///
    /// If the provider answers for another object, or with another inverse, the intent is left open, since the effect may
    /// have happened. A journal failure is refused as `Provider`, because the enum has no journal variant, and the effect is
    /// not run when the intent could not be written.
    pub fn effect(&mut self, op: EffectOp) -> Result<ObjectRef, EffectRefusal> {
        let (kind, project, name) = op.target();
        if self.manifest.effects.is_empty() {
            return Err(EffectRefusal::WatchOnly);
        }
        if matches!(op, EffectOp::IncusDelete { .. }) {
            return Err(EffectRefusal::NotDeclared);
        }
        if !self
            .manifest
            .effects
            .iter()
            .any(|(declared, _)| *declared == kind)
        {
            return Err(EffectRefusal::NotDeclared);
        }
        if !crate::ownership::manifest_names_project(&self.manifest.projects, project) {
            return Err(EffectRefusal::ProjectNotAllowed);
        }
        if kind == IncusKind::Project && project != name {
            return Err(EffectRefusal::Provider(
                "a project's name is its project".to_string(),
            ));
        }
        if let EffectOp::IncusPatchAclField { object, rules, .. } = &op
            && (object.kind != IncusKind::Acl
                || object.project != "default"
                || object.parent.is_some()
                || object.uuid.is_some()
                || !rules.iter().all(Value::is_object))
        {
            return Err(EffectRefusal::NotDeclared);
        }
        let digest = op_digest(&op);
        let planned = self.provider.plan(&self.entry, &op)?;
        if planned.op_digest != digest {
            return Err(EffectRefusal::Provider(
                "the plan names another op".to_string(),
            ));
        }
        if !self.manifest.effects.contains(&(kind, planned.class)) {
            return Err(EffectRefusal::NotDeclared);
        }
        if !plan_has_op_shape(&op, &planned) {
            return Err(EffectRefusal::Provider(
                "the plan's pre-image does not fit the op".to_string(),
            ));
        }
        // The intent names the object the op addresses, and a patch's pre-image, before the provider applies anything.
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
            EffectOp::IncusPatchConfig { object, .. }
            | EffectOp::IncusPatchAclField { object, .. }
            | EffectOp::IncusDelete { object, .. } => object.clone(),
        };
        let id = if let Some(patch) = &planned.acl_patch {
            self.journal.intend_acl(
                IntentFields {
                    entry: &self.entry,
                    class: planned.class,
                    op_digest: digest,
                    target: target.clone(),
                    pre_image: None,
                    now_ms: self.now_ms,
                },
                patch.clone(),
            )
        } else {
            self.journal.intend(
                &self.entry,
                planned.class,
                digest,
                target.clone(),
                planned.pre_image.clone(),
                self.now_ms,
            )
        }
        .map_err(journal_refusal)?;
        // Journals an effect that landed. An answer for another object, or with another inverse, leaves the intent open.
        let mut landed = |object: &ObjectRef, inverse: Inverse| {
            if !same_place(object, &target) {
                return Err(EffectRefusal::Provider(
                    "the provider answered for another object; the intent stays open".to_string(),
                ));
            }
            if required_inverse(&op, object, &planned).as_ref() != Some(&inverse) {
                return Err(EffectRefusal::Provider(
                    "the provider's inverse is not the one the op requires; the intent stays open"
                        .to_string(),
                ));
            }
            self.journal
                .applied(id, object.clone(), inverse)
                .map_err(journal_refusal)
        };
        match self.provider.effect(&self.entry, &op, &planned) {
            Ok((object, inverse)) => {
                landed(&object, inverse)?;
                Ok(object)
            }
            // The object exists and is the entry's own, so the effect is recorded as applied; the plugin is told why the
            // request did not finish.
            Err(EffectRefusal::Landed(unfinished)) => {
                landed(&unfinished.object, unfinished.inverse)?;
                Err(EffectRefusal::Provider(unfinished.why))
            }
            Err(refusal) => {
                // A refusal that says the outcome is not known leaves the intent open: the write may have landed, and a
                // failed record would retire an effect that still exists. Any other refusal is recorded as failed. A journal
                // that cannot record it is poisoned and refuses every later write, so the refusal is returned either way.
                if refusal != EffectRefusal::Unresolved {
                    let _ = self.journal.failed(id, &refusal.to_string());
                }
                Err(refusal)
            }
        }
    }

    /// Provides one declared key's value to the reconciler. An undeclared key, or a key already provided, is refused.
    pub fn provide(&mut self, key: &str, value: Value) -> Result<(), EffectRefusal> {
        if !self.manifest.provides.iter().any(|spec| spec.key == key) {
            return Err(EffectRefusal::NotDeclared);
        }
        if self.provided.contains_key(key) {
            return Err(EffectRefusal::Provider(
                "the key is already provided".to_string(),
            ));
        }
        self.provided.insert(key.to_string(), value);
        Ok(())
    }

    /// The values the apply provided, once it is done.
    pub fn provided(self) -> BTreeMap<String, Value> {
        self.provided
    }
}

/// Whether an answer names the object an intent names: the same kind, project, parent and name. The uuid is not compared,
/// since a create learns it only as it applies.
fn same_place(object: &ObjectRef, target: &ObjectRef) -> bool {
    object.kind == target.kind
        && object.project == target.project
        && object.parent == target.parent
        && object.name == target.name
}

/// The inverse an op's effect must return, given the object it applied to. A create's must be the delete of the object it
/// made. A kind with a uuid is deleted by it, so the object must carry one; a kind without one is deleted by its kind,
/// project, parent and name, so the object must carry none. A patch's must restore the planned pre-image on the object the op
/// names. `None` when no inverse can be right: a create whose uuid does not fit its kind, a patch with no pre-image, or a
/// delete, which a plugin never runs.
fn required_inverse(op: &EffectOp, object: &ObjectRef, planned: &Planned) -> Option<Inverse> {
    match op {
        EffectOp::IncusCreate { .. } => {
            (object.uuid.is_some() == object.kind.has_uuid()).then(|| Inverse::IncusDelete {
                object: object.clone(),
            })
        }
        EffectOp::IncusPatchConfig { object: named, .. } => Some(Inverse::IncusRestoreConfig {
            object: named.clone(),
            config: planned.pre_image.clone()?,
        }),
        EffectOp::IncusPatchAclField { object, field, .. } => Some(Inverse::IncusRestoreAclField {
            object: object.clone(),
            field: *field,
            rules: planned.acl_patch.as_ref()?.previous.clone(),
        }),
        EffectOp::IncusDelete { .. } => None,
    }
}

/// Whether a plan's pre-image has the shape its op needs: none for a create, and for a patch exactly one entry for each key
/// the patch sets or removes, so the restore it implies covers the patch and nothing else.
fn plan_has_op_shape(op: &EffectOp, planned: &Planned) -> bool {
    if let EffectOp::IncusPatchAclField { field, rules, .. } = op {
        return planned.class == EffectClass::A
            && planned.pre_image.is_none()
            && planned.acl_patch.as_ref().is_some_and(|patch| {
                patch.field == *field
                    && patch.rules == *rules
                    && patch.previous.iter().all(Value::is_object)
            });
    }
    if planned.acl_patch.is_some() {
        return false;
    }
    match (op, &planned.pre_image) {
        (EffectOp::IncusCreate { .. }, None) => true,
        (EffectOp::IncusPatchConfig { config, .. }, Some(pre)) => pre.keys().eq(config.keys()),
        _ => false,
    }
}

fn journal_refusal(e: crate::KernelError) -> EffectRefusal {
    EffectRefusal::Provider(format!("the journal refused the write: {e}"))
}

/// A compiled-in plugin. Its config is a typed value, and the reconciler never sees that type.
pub trait Plugin {
    /// The plugin's id, which is a valid [`PluginId`] and equals `manifest().id`.
    const ID: &'static str;
    /// The config the plugin reads from an entry.
    type Config: DeserializeOwned;
    /// What the plugin may do.
    fn manifest() -> Manifest;
    /// Applies the config through `ctx`.
    fn apply(ctx: &mut Ctx<'_>, cfg: &Self::Config) -> Result<(), PluginError>;
    /// Whether the reconciler applies this plugin's entry again when its config has not changed since it last applied. A plugin
    /// opts in for state it repairs outside the entry config; the default keeps a converged entry skipped.
    fn reconciles_when_unchanged() -> bool {
        false
    }
    /// The class-A kinds this plugin's class-B objects use, such as the ACL an instance's NIC names. Incus refuses to delete
    /// such an object while its user exists, so a removal retires it only after an approved plan has deleted every class-B
    /// object of the entry and each has read back absent. The default is none: every class-A effect reverts at once.
    fn retired_after_plan() -> &'static [IncusKind] {
        &[]
    }
}

/// A plugin with its config type erased, so the reconciler can hold any plugin in one collection.
pub trait ErasedPlugin: Send + Sync {
    fn id(&self) -> &'static str;
    fn manifest(&self) -> Manifest;
    /// Deserializes `config` into the plugin's type and applies it. A config that does not deserialize never reaches apply.
    fn apply(&self, ctx: &mut Ctx<'_>, config: &Value) -> Result<(), PluginError>;
    /// Whether a converged, unchanged entry is applied again. A plugin that does not opt in keeps the reconciler's skip.
    fn reconciles_when_unchanged(&self) -> bool {
        false
    }
    /// The class-A kinds whose objects wait for the entry's class-B objects to be deleted (see
    /// [`Plugin::retired_after_plan`]).
    fn retired_after_plan(&self) -> &'static [IncusKind] {
        &[]
    }
}

/// The erased form of a plugin `P`.
pub struct Erased<P: Plugin>(PhantomData<fn() -> P>);

impl<P: Plugin> Erased<P> {
    /// The erased form of `P`, refused unless `P::ID` is a valid plugin id and equals the id its manifest names. A plugin's
    /// name is then one value everywhere the reconciler sees it.
    pub fn new() -> Result<Self, KernelError> {
        if PluginId::parse(P::ID).is_none() {
            return Err(KernelError::Plugin(
                "its id is not a valid plugin id".to_string(),
            ));
        }
        if P::manifest().id.0 != P::ID {
            return Err(KernelError::Plugin(
                "its id is not the id its manifest names".to_string(),
            ));
        }
        Ok(Erased(PhantomData))
    }
}

impl<P: Plugin> ErasedPlugin for Erased<P> {
    fn id(&self) -> &'static str {
        P::ID
    }

    fn manifest(&self) -> Manifest {
        P::manifest()
    }

    fn apply(&self, ctx: &mut Ctx<'_>, config: &Value) -> Result<(), PluginError> {
        let cfg = <P::Config as Deserialize>::deserialize(config)
            .map_err(|e| PluginError::Config(redact(&e.to_string())))?;
        P::apply(ctx, &cfg)
    }

    fn reconciles_when_unchanged(&self) -> bool {
        P::reconciles_when_unchanged()
    }

    fn retired_after_plan(&self) -> &'static [IncusKind] {
        P::retired_after_plan()
    }
}

/// serde quotes or backticks the value it rejected, and a config's value must never reach an error. Those spans are
/// replaced with an ellipsis. Field names are kept for the three messages that name only a field.
fn redact(message: &str) -> String {
    const NAME_ONLY: [&str; 3] = ["missing field", "unknown field", "duplicate field"];
    if NAME_ONLY.iter().any(|prefix| message.starts_with(prefix)) {
        return message.to_string();
    }
    let mut out = String::with_capacity(message.len());
    let mut chars = message.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                out.push('…');
                while let Some(d) = chars.next() {
                    match d {
                        '\\' => {
                            chars.next();
                        }
                        '"' => break,
                        _ => {}
                    }
                }
            }
            '`' => {
                out.push('…');
                for d in chars.by_ref() {
                    if d == '`' {
                        break;
                    }
                }
            }
            _ => out.push(c),
        }
    }
    out
}
