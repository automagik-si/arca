//! The effect journal. Every effect is planned read-only, journalled as an intent before it runs, and journalled again with
//! its data-only inverse the moment it applies. Recovery and removal read the journal, never a provider's memory.
//!
//! Records are JSON lines. `FileJournal` syncs after each append. A last line that a crash cut short is dropped on open, and
//! the drop is reported. Any other line that does not decode refuses the journal, so a damaged middle is never skipped.
//!
//! The inverse vocabulary is closed. An unknown `op`, or a field its variant does not name, fails to deserialize, including
//! inside the object reference an inverse carries.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::KernelError;
use crate::canon::{Digest, canonical_json};
use crate::desired::{EntryId, PluginId};
use crate::ownership::ObjectRef;

/// How an effect is undone when its entry leaves the desired state.
///
/// `A` is revertible without loss and is reverted at once. `B` is revertible but destructive, so its removal needs an approved
/// delete plan. `C` is an emission (spend, a message, a row written outside a provider), which is never reverted automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EffectClass {
    A,
    B,
    C,
}

/// The only top-level fields a plugin may replace on an ACL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AclField {
    Ingress,
    Egress,
}

impl AclField {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ingress => "ingress",
            Self::Egress => "egress",
        }
    }
}

/// The durable pre-image and intended value of an ACL array patch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AclPatch {
    pub field: AclField,
    pub previous: Vec<serde_json::Value>,
    pub rules: Vec<serde_json::Value>,
}

/// The data that undoes an applied effect. The vocabulary is closed: deserialization refuses any `op` this enum does not
/// name and any field a variant does not name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", try_from = "InverseWire")]
pub enum Inverse {
    /// Deletes an object the effect created.
    IncusDelete { object: ObjectRef },
    /// Restores the keys a patch changed. A `None` removes a key the patch added.
    IncusRestoreConfig {
        object: ObjectRef,
        config: BTreeMap<String, Option<String>>,
    },
    /// Restores the complete array replaced by an ACL field patch.
    IncusRestoreAclField {
        object: ObjectRef,
        field: AclField,
        rules: Vec<serde_json::Value>,
    },
    /// Removes an ACL rule, identified by its digest.
    AclRuleRemove {
        project: String,
        acl: String,
        rule: Digest,
    },
    /// Removes a file the effect wrote, identified by its digest.
    FileRemove { path: String, digest: Digest },
    /// Nothing to undo: the effect is an emission.
    Emission,
}

/// The wire form of [`Inverse`]. Every variant is a struct variant, so serde refuses an extra field in each one. A unit
/// variant of an internally tagged enum would accept one silently.
#[derive(Deserialize)]
#[serde(tag = "op", deny_unknown_fields)]
enum InverseWire {
    IncusDelete {
        #[serde(deserialize_with = "closed::object")]
        object: ObjectRef,
    },
    IncusRestoreConfig {
        #[serde(deserialize_with = "closed::object")]
        object: ObjectRef,
        config: BTreeMap<String, Option<String>>,
    },
    IncusRestoreAclField {
        #[serde(deserialize_with = "closed::object")]
        object: ObjectRef,
        field: AclField,
        rules: Vec<serde_json::Value>,
    },
    AclRuleRemove {
        project: String,
        acl: String,
        rule: Digest,
    },
    FileRemove {
        path: String,
        digest: Digest,
    },
    Emission {},
}

impl TryFrom<InverseWire> for Inverse {
    type Error = &'static str;
    fn try_from(wire: InverseWire) -> Result<Self, Self::Error> {
        Ok(match wire {
            InverseWire::IncusDelete { object } => Inverse::IncusDelete { object },
            InverseWire::IncusRestoreConfig { object, config } => {
                Inverse::IncusRestoreConfig { object, config }
            }
            InverseWire::IncusRestoreAclField {
                object,
                field,
                rules,
            } => {
                if object.kind != crate::ownership::IncusKind::Acl
                    || object.project != "default"
                    || object.parent.is_some()
                    || object.uuid.is_some()
                    || !rules.iter().all(serde_json::Value::is_object)
                {
                    return Err("invalid ACL array inverse");
                }
                Inverse::IncusRestoreAclField {
                    object,
                    field,
                    rules,
                }
            }
            InverseWire::AclRuleRemove { project, acl, rule } => {
                Inverse::AclRuleRemove { project, acl, rule }
            }
            InverseWire::FileRemove { path, digest } => Inverse::FileRemove { path, digest },
            InverseWire::Emission {} => Inverse::Emission,
        })
    }
}

/// What an intent is for, when it is not a plain create or patch. The marker is what tells a delete apart from a create whose
/// pre-image is absent, so recovery never reads one as the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IntentKind {
    /// The intent retires an object: a class-A revert of a create, or a class-B delete under an approved plan.
    Delete,
}

/// Where an effect stands. `Intent` is written before the provider applies anything; the other states are written after.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EffectState {
    Intent,
    Applied,
    Confirmed,
    Reverted,
    Failed,
}

/// One journal record. A record names its effect by `id`; each state change writes a full copy of the record, so the newest
/// record for an id is the effect's state. The intent fields (`target`, `pre_image`, `acl_patch`, `intent`) are copied unchanged to every
/// later record of the effect. An `Intent` has no object and no inverse; every later state of an applied effect carries both.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectRecord {
    pub id: u64,
    pub entry: EntryId,
    pub class: EffectClass,
    pub op_digest: Digest,
    /// The object the intent names. A create names the object it will make, which has no uuid until it applies.
    #[serde(deserialize_with = "closed::object")]
    pub target: ObjectRef,
    /// For a patch, the values its keys held when the plan was made, with `None` for a key that was absent. `None` for a
    /// create, so recovery can tell the two apart from the intent alone.
    pub pre_image: Option<BTreeMap<String, Option<String>>>,
    /// ACL patches carry typed arrays; omitted so old records retain their shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acl_patch: Option<AclPatch>,
    /// Set on the intent of a retirement and on every record of it. It is omitted when absent, so a record without it keeps the
    /// bytes it always had.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<IntentKind>,
    /// The plugin whose apply made the effect, set on its intent and on every later record of it. A restart reads it to find the
    /// plugin of each entry again. It is omitted when absent, so a record without it keeps the bytes it always had.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<PluginId>,
    #[serde(default, deserialize_with = "closed::optional")]
    pub object: Option<ObjectRef>,
    pub inverse: Option<Inverse>,
    pub state: EffectState,
    pub at_ms: u64,
}

/// Durable storage for journal records. `append` returns only once the record is durable.
pub trait JournalStore {
    /// Appends one record. An error means the record may not be durable, and the caller must not run the effect it names.
    fn append(&mut self, record: &EffectRecord) -> Result<(), KernelError>;
    /// Every record in append order, with a note when the last line was torn and dropped.
    fn load(&self) -> Result<(Vec<EffectRecord>, Option<String>), KernelError>;
}

/// The write side the reconciler and [`crate::plugin::Ctx`] use. A plan's effect is intended, then applied, then recorded.
pub trait JournalSink {
    /// Records the intent of an effect before it runs, and returns the id that names it. `target` is the object the effect
    /// names, and `pre_image` is the planned pre-image of a patch (`None` for a create). Both are durable before the effect.
    fn intend(
        &mut self,
        entry: &EntryId,
        class: EffectClass,
        op_digest: Digest,
        target: ObjectRef,
        pre_image: Option<BTreeMap<String, Option<String>>>,
        now_ms: u64,
    ) -> Result<u64, KernelError>;
    /// Records an ACL array intent before its write. Sinks without support refuse before any effect.
    fn intend_acl(
        &mut self,
        fields: IntentFields<'_>,
        patch: AclPatch,
    ) -> Result<u64, KernelError> {
        let _ = (fields, patch);
        Err(KernelError::Journal(
            "the journal sink cannot record an ACL array patch".into(),
        ))
    }
    /// Records that the effect applied, with the object it touched and the inverse that undoes it.
    fn applied(&mut self, id: u64, object: ObjectRef, inverse: Inverse) -> Result<(), KernelError>;
    /// Records that the effect was refused. The reason is returned to the caller and is not journalled: a record carries no
    /// free text, because provider messages can echo an Incus response body.
    fn failed(&mut self, id: u64, why: &str) -> Result<(), KernelError>;
}

/// A journal file: one record per JSON line. Each append syncs before it returns. After a failed append this handle refuses
/// further appends, so reopening the file is the only way to write to it again.
#[derive(Debug)]
pub struct FileJournal {
    path: PathBuf,
    file: File,
    torn: Option<String>,
    broken: bool,
}

impl FileJournal {
    /// Opens the journal at `path`, creating it with mode 0600 when it does not exist.
    ///
    /// A symbolic link, a file that is not regular, or an existing file with a group or other permission bit is refused. The
    /// file is locked for this handle's life, so a second opener is refused rather than interleaving lines. A torn last line
    /// is dropped here, by truncation, so the next append starts on a fresh line; the drop is kept for [`JournalStore::load`]
    /// to report.
    pub fn open(path: &Path) -> Result<FileJournal, KernelError> {
        let shown = path.display().to_string();
        // The path's own metadata is read before anything is opened, so a symbolic link or any other non-file is refused
        // before open can follow the link or touch the object.
        let created = match fs::symlink_metadata(path) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(io_reason(&shown, "a symbolic link; refused"));
                }
                if !meta.is_file() {
                    return Err(io_reason(&shown, "not a regular file; refused"));
                }
                false
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => true,
            Err(e) => return Err(io_error(&shown, &e)),
        };
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| io_error(&shown, &e))?;
        let meta = file.metadata().map_err(|e| io_error(&shown, &e))?;
        if !meta.is_file() {
            return Err(io_reason(&shown, "not a regular file; refused"));
        }
        if meta.mode() & 0o077 != 0 {
            return Err(io_reason(
                &shown,
                "has a group or other permission bit; it must be mode 0600",
            ));
        }
        file.try_lock().map_err(|e| match e {
            TryLockError::WouldBlock => {
                KernelError::Journal(format!("{shown} is held by another process"))
            }
            TryLockError::Error(e) => io_error(&shown, &e),
        })?;
        let bytes = read_all(&file).map_err(|e| io_error(&shown, &e))?;
        let decoded = decode(&bytes)?;
        if decoded.torn.is_some() {
            file.set_len(decoded.keep as u64)
                .map_err(|e| io_error(&shown, &e))?;
            file.sync_all().map_err(|e| io_error(&shown, &e))?;
        }
        if created {
            sync_parent(path)?;
        }
        Ok(FileJournal {
            path: path.to_path_buf(),
            file,
            torn: decoded.torn,
            broken: false,
        })
    }

    fn shown(&self) -> String {
        self.path.display().to_string()
    }
}

impl JournalStore for FileJournal {
    fn append(&mut self, record: &EffectRecord) -> Result<(), KernelError> {
        let shown = self.shown();
        if self.broken {
            return Err(io_reason(
                &shown,
                "an earlier write failed; reopen the journal",
            ));
        }
        let mut line = serde_json::to_vec(record)
            .map_err(|_| KernelError::Journal("a record does not encode".to_string()))?;
        line.push(b'\n');
        let written = self
            .file
            .write_all(&line)
            .and_then(|()| self.file.sync_all());
        if let Err(e) = written {
            self.broken = true;
            return Err(io_error(&shown, &e));
        }
        Ok(())
    }

    fn load(&self) -> Result<(Vec<EffectRecord>, Option<String>), KernelError> {
        let bytes = read_all(&self.file).map_err(|e| io_error(&self.shown(), &e))?;
        let decoded = decode(&bytes)?;
        Ok((decoded.records, self.torn.clone().or(decoded.torn)))
    }
}

/// Reads the journal at `path` without taking its lock, for a read-only listing while a server holds the file. A missing
/// file is an empty journal. Damaged records refuse the read; a torn last line is dropped by the same decoder.
pub fn read_journal(path: &Path) -> Result<(Vec<EffectRecord>, Option<String>), KernelError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Vec::new(), None));
        }
        Err(error) => return Err(io_error(&path.display().to_string(), &error)),
    };
    decode(&bytes).map(|decoded| (decoded.records, decoded.torn))
}

/// An in-memory journal, for tests and for callers that need no durability.
#[derive(Debug, Default, Clone)]
pub struct MemJournal {
    records: Vec<EffectRecord>,
}

impl MemJournal {
    /// An empty journal. Its `append` never fails.
    pub fn new() -> MemJournal {
        MemJournal::default()
    }
}

impl JournalStore for MemJournal {
    fn append(&mut self, record: &EffectRecord) -> Result<(), KernelError> {
        self.records.push(record.clone());
        Ok(())
    }

    fn load(&self) -> Result<(Vec<EffectRecord>, Option<String>), KernelError> {
        Ok((self.records.clone(), None))
    }
}

/// The journal's state over a store: the newest record of each effect, the next free id, and a note when the store's last
/// line was torn.
///
/// A write that the store refuses poisons the journal. A poisoned journal refuses every later write, so an effect is never
/// applied after its record could not be made. The void methods [`Journal::confirmed`] and [`Journal::reverted`] cannot
/// return an error, so an invalid transition poisons the journal too.
#[derive(Debug)]
pub struct Journal<S: JournalStore> {
    store: S,
    latest: BTreeMap<u64, EffectRecord>,
    next_id: u64,
    torn: Option<String>,
    poison: Option<KernelError>,
}

impl<S: JournalStore> Journal<S> {
    /// Folds the store's records into the journal's state. A record that does not follow its effect's last record refuses
    /// the open, so the state is always a legal history. Unresolved intents do not refuse the open; the caller decides.
    pub fn open(store: S) -> Result<Self, KernelError> {
        let (records, torn) = store.load()?;
        let mut latest: BTreeMap<u64, EffectRecord> = BTreeMap::new();
        for record in records {
            check_outcome(&record)?;
            match latest.get(&record.id) {
                None if record.state == EffectState::Intent => {}
                None => return Err(corrupt(record.id, "has no intent")),
                Some(prev) => {
                    if let Some(why) = refusal_to_follow(prev, &record) {
                        return Err(corrupt(record.id, why));
                    }
                }
            }
            latest.insert(record.id, record);
        }
        // The journal never writes the top id, so a journal that holds it has no id left to continue from.
        let next_id = match latest.keys().next_back() {
            None => 1,
            Some(last) => last.checked_add(1).ok_or_else(|| {
                KernelError::Journal(format!(
                    "effect {last} is the last id; none is left to continue from"
                ))
            })?,
        };
        Ok(Journal {
            store,
            latest,
            next_id,
            torn,
            poison: None,
        })
    }

    /// The note the store gave when the last line was torn and dropped, if any.
    pub fn torn_note(&self) -> Option<&str> {
        self.torn.as_deref()
    }

    /// The error that stopped the journal, if it has stopped.
    pub fn poisoned(&self) -> Option<&KernelError> {
        self.poison.as_ref()
    }

    /// Marks an applied effect as confirmed. A missing or illegal transition poisons the journal.
    pub fn confirmed(&mut self, id: u64) {
        self.settle(id, EffectState::Confirmed);
    }

    /// Marks an applied effect as reverted. A missing or illegal transition poisons the journal.
    pub fn reverted(&mut self, id: u64) {
        self.settle(id, EffectState::Reverted);
    }

    /// The entry's effects that still count: each one's newest record in id order, leaving out reverted and failed effects.
    /// An intent with no outcome is included, so a caller can see it.
    pub fn live(&self, entry: &EntryId) -> Vec<EffectRecord> {
        self.latest
            .values()
            .filter(|r| &r.entry == entry)
            .filter(|r| !matches!(r.state, EffectState::Reverted | EffectState::Failed))
            .cloned()
            .collect()
    }

    /// Every entry with at least one effect that still counts, in id order. Recovery and removal start from this list, so an
    /// entry the desired state no longer carries is found from the journal alone.
    pub fn live_entries(&self) -> Vec<EntryId> {
        let entries: std::collections::BTreeSet<EntryId> = self
            .latest
            .values()
            .filter(|r| !matches!(r.state, EffectState::Reverted | EffectState::Failed))
            .map(|r| r.entry.clone())
            .collect();
        entries.into_iter().collect()
    }

    /// Every intent with no applied, failed or reverted record, in id order.
    pub fn unresolved(&self) -> Vec<EffectRecord> {
        self.latest
            .values()
            .filter(|r| r.state == EffectState::Intent)
            .cloned()
            .collect()
    }

    fn write(&mut self, record: EffectRecord) -> Result<(), KernelError> {
        if self.poison.is_some() {
            return Err(KernelError::Journal(
                "the journal stopped after an earlier failure; reopen it".to_string(),
            ));
        }
        if let Err(e) = self.store.append(&record) {
            self.poison = Some(e.clone());
            return Err(e);
        }
        self.latest.insert(record.id, record);
        Ok(())
    }

    fn advance(
        &mut self,
        id: u64,
        to: EffectState,
        outcome: Option<(ObjectRef, Inverse)>,
    ) -> Result<(), KernelError> {
        let prev = self
            .latest
            .get(&id)
            .ok_or_else(|| corrupt(id, "is not in the journal"))?
            .clone();
        let (object, inverse) = match outcome {
            Some((object, inverse)) => (Some(object), Some(inverse)),
            None => (prev.object.clone(), prev.inverse.clone()),
        };
        let next = EffectRecord {
            id,
            entry: prev.entry.clone(),
            class: prev.class,
            op_digest: prev.op_digest.clone(),
            target: prev.target.clone(),
            pre_image: prev.pre_image.clone(),
            acl_patch: prev.acl_patch.clone(),
            intent: prev.intent,
            plugin: prev.plugin.clone(),
            object,
            inverse,
            state: to,
            at_ms: prev.at_ms,
        };
        if let Some(why) = refusal_to_follow(&prev, &next) {
            return Err(corrupt(id, why));
        }
        check_outcome(&next)?;
        self.write(next)
    }

    fn settle(&mut self, id: u64, to: EffectState) {
        if let Err(e) = self.advance(id, to, None) {
            // The first failure is the one reported; a later one never replaces it.
            self.poison = self.poison.take().or(Some(e));
        }
    }
}

/// The parts of an intent that a create, a patch and a retirement do not share: a patch's pre-image, and the marker a retirement
/// carries.
struct IntentShape {
    acl_patch: Option<AclPatch>,
    pre_image: Option<BTreeMap<String, Option<String>>>,
    intent: Option<IntentKind>,
    plugin: Option<PluginId>,
}

impl<S: JournalStore> Journal<S> {
    /// Records the intent of a retirement, marked as one, before the provider retires anything. It is refused on the same terms
    /// as [`JournalSink::intend`].
    pub fn intend_delete(
        &mut self,
        entry: &EntryId,
        class: EffectClass,
        op_digest: Digest,
        target: ObjectRef,
        now_ms: u64,
    ) -> Result<u64, KernelError> {
        let shape = IntentShape {
            acl_patch: None,
            pre_image: None,
            intent: Some(IntentKind::Delete),
            plugin: None,
        };
        self.intend_as(entry, class, op_digest, target, shape, now_ms)
    }

    /// Records a typed ACL patch intent, optionally attributed to a plugin.
    pub fn intend_acl_by(
        &mut self,
        plugin: Option<&PluginId>,
        fields: IntentFields<'_>,
        patch: AclPatch,
    ) -> Result<u64, KernelError> {
        if fields.class != EffectClass::A
            || fields.target.kind != crate::ownership::IncusKind::Acl
            || fields.target.project != "default"
            || fields.target.parent.is_some()
            || fields.target.uuid.is_some()
            || fields.pre_image.is_some()
            || !patch
                .previous
                .iter()
                .chain(&patch.rules)
                .all(serde_json::Value::is_object)
        {
            return Err(KernelError::Journal("invalid ACL array intent".into()));
        }
        let shape = IntentShape {
            acl_patch: Some(patch),
            pre_image: None,
            intent: None,
            plugin: plugin.cloned(),
        };
        self.intend_as(
            fields.entry,
            fields.class,
            fields.op_digest,
            fields.target,
            shape,
            fields.now_ms,
        )
    }

    fn intend_as(
        &mut self,
        entry: &EntryId,
        class: EffectClass,
        op_digest: Digest,
        target: ObjectRef,
        shape: IntentShape,
        now_ms: u64,
    ) -> Result<u64, KernelError> {
        let id = self.next_id;
        // The top id is never written, so the next id always exists. Both checks refuse before any record is written.
        let following = id
            .checked_add(1)
            .ok_or_else(|| KernelError::Journal("the effect ids are used up".to_string()))?;
        if self.latest.contains_key(&id) {
            return Err(KernelError::Journal(format!(
                "effect {id} is already in the journal"
            )));
        }
        self.write(EffectRecord {
            id,
            entry: entry.clone(),
            class,
            op_digest,
            target,
            pre_image: shape.pre_image,
            acl_patch: shape.acl_patch,
            intent: shape.intent,
            plugin: shape.plugin,
            object: None,
            inverse: None,
            state: EffectState::Intent,
            at_ms: now_ms,
        })?;
        self.next_id = following;
        Ok(id)
    }
}

impl<S: JournalStore> Journal<S> {
    /// Records an intent that `plugin`'s apply makes, so the record and every later record of the effect name the plugin. A restart
    /// reads the name to find which plugin an entry belongs to. Refused on the terms of [`JournalSink::intend`].
    pub fn intend_by(
        &mut self,
        plugin: &PluginId,
        fields: IntentFields<'_>,
    ) -> Result<u64, KernelError> {
        let shape = IntentShape {
            acl_patch: None,
            pre_image: fields.pre_image,
            intent: None,
            plugin: Some(plugin.clone()),
        };
        self.intend_as(
            fields.entry,
            fields.class,
            fields.op_digest,
            fields.target,
            shape,
            fields.now_ms,
        )
    }
}

impl<S: JournalStore> JournalSink for Journal<S> {
    fn intend_acl(
        &mut self,
        fields: IntentFields<'_>,
        patch: AclPatch,
    ) -> Result<u64, KernelError> {
        self.intend_acl_by(None, fields, patch)
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
        let shape = IntentShape {
            acl_patch: None,
            pre_image,
            intent: None,
            plugin: None,
        };
        self.intend_as(entry, class, op_digest, target, shape, now_ms)
    }

    fn applied(&mut self, id: u64, object: ObjectRef, inverse: Inverse) -> Result<(), KernelError> {
        self.advance(id, EffectState::Applied, Some((object, inverse)))
    }

    fn failed(&mut self, id: u64, _why: &str) -> Result<(), KernelError> {
        self.advance(id, EffectState::Failed, None)
    }
}

/// The fields of an intent a plugin's apply makes, for [`Journal::intend_by`]. They are the fields [`JournalSink::intend`] takes.
pub struct IntentFields<'a> {
    pub entry: &'a EntryId,
    pub class: EffectClass,
    pub op_digest: Digest,
    pub target: ObjectRef,
    pub pre_image: Option<BTreeMap<String, Option<String>>>,
    pub now_ms: u64,
}

/// The class-B objects of one entry, awaiting an approval over exactly this list.
///
/// The digest covers the entry and every object with its uuid. It does not cover `created_ms`, so the same objects give the
/// same digest whenever the plan is made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeletePlan {
    pub entry: EntryId,
    pub objects: Vec<ObjectRef>,
    pub digest: Digest,
    pub created_ms: u64,
}

/// What removing an entry does, partitioned by class. Each effect appears in exactly one part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removal {
    /// Class-A effects, most recent first, so each undo runs before the effect it depends on.
    pub revert_now: Vec<EffectRecord>,
    /// One plan over the class-B objects that a delete inverse names, each listed once, or `None` when there are none.
    pub plan: Option<DeletePlan>,
    /// Class-C emissions. They are never reverted automatically.
    pub never: Vec<EffectRecord>,
    /// Intents with no outcome, which the caller must resolve before it acts on the entry.
    pub unresolved: Vec<EffectRecord>,
}

/// Partitions the entry's live effects for removal. `now_ms` is only stamped on the plan.
pub fn removal(journal: &Journal<impl JournalStore>, entry: &EntryId, now_ms: u64) -> Removal {
    let mut revert_now = Vec::new();
    let mut objects = Vec::new();
    let mut never = Vec::new();
    let mut unresolved = Vec::new();
    for record in journal.live(entry) {
        if record.state == EffectState::Intent {
            unresolved.push(record);
            continue;
        }
        match record.class {
            EffectClass::A => revert_now.push(record),
            EffectClass::B => {
                // Only a delete inverse names an object a plan may remove. A patch's restore names an object that stays.
                if let Some(Inverse::IncusDelete { object }) = &record.inverse
                    && !objects.contains(object)
                {
                    objects.push(object.clone());
                }
            }
            EffectClass::C => never.push(record),
        }
    }
    revert_now.reverse();
    let plan = (!objects.is_empty()).then(|| {
        let digest = plan_digest(entry, &objects);
        DeletePlan {
            entry: entry.clone(),
            objects,
            digest,
            created_ms: now_ms,
        }
    });
    Removal {
        revert_now,
        plan,
        never,
        unresolved,
    }
}

fn plan_digest(entry: &EntryId, objects: &[ObjectRef]) -> Digest {
    let value = serde_json::json!({ "entry": entry.0, "objects": objects });
    // Strings, digests and integers only, so the value has a canonical form.
    let text = canonical_json(&value).expect("a delete plan has a canonical form");
    Digest::of(text.as_bytes())
}

/// Why `next` may not follow `prev` for the same effect, or `None` when it may: a legal state move, with the same entry, class,
/// digest, time, target and pre-image. A later record keeps the outcome its predecessor carries. An intent that never applied
/// may be reverted with no outcome at all. An applied outcome names the intent's object, and its inverse names that object.
fn refusal_to_follow(prev: &EffectRecord, next: &EffectRecord) -> Option<&'static str> {
    if prev.id != next.id
        || prev.entry != next.entry
        || prev.plugin != next.plugin
        || prev.class != next.class
        || prev.op_digest != next.op_digest
        || prev.at_ms != next.at_ms
        || prev.target != next.target
        || prev.pre_image != next.pre_image
        || prev.acl_patch != next.acl_patch
        || prev.intent != next.intent
    {
        return Some("changes what its intent says");
    }
    let legal = matches!(
        (prev.state, next.state),
        (
            EffectState::Intent,
            EffectState::Applied | EffectState::Failed | EffectState::Reverted
        ) | (
            EffectState::Applied,
            EffectState::Confirmed | EffectState::Reverted
        ) | (EffectState::Confirmed, EffectState::Reverted)
    );
    if !legal {
        return Some("moves to a state its last record does not allow");
    }
    if prev.state != EffectState::Intent {
        if next.object != prev.object || next.inverse != prev.inverse {
            return Some("changes the outcome it applied");
        }
        return None;
    }
    match next.state {
        EffectState::Applied => applied_names_intent_target(next),
        EffectState::Failed | EffectState::Reverted
            if next.object.is_some() || next.inverse.is_some() =>
        {
            Some("carries an outcome its intent never had")
        }
        _ => None,
    }
}

/// An applied outcome names the object its intent names: the same kind, project, parent and name, and the same uuid when the
/// intent carries one. A create's intent carries no uuid, since the uuid is learned only as the create applies.
fn applied_names_intent_target(record: &EffectRecord) -> Option<&'static str> {
    let object = record.object.as_ref()?;
    let same_place = record.target.kind == object.kind
        && record.target.project == object.project
        && record.target.parent == object.parent
        && record.target.name == object.name;
    let same_uuid = record.target.uuid.is_none() || record.target.uuid == object.uuid;
    if !same_place || !same_uuid {
        return Some("applies to an object its intent does not name");
    }
    inverse_names_other_object(record)
}

/// Refuses an applied outcome whose inverse names an object other than the one the outcome names. Only the object
/// references are compared, by kind, project and name, since a create's uuid is learned by the time it applies.
fn inverse_names_other_object(record: &EffectRecord) -> Option<&'static str> {
    let (Some(object), Some(inverse)) = (&record.object, &record.inverse) else {
        return None;
    };
    let named = match inverse {
        Inverse::IncusDelete { object }
        | Inverse::IncusRestoreConfig { object, .. }
        | Inverse::IncusRestoreAclField { object, .. } => object,
        Inverse::AclRuleRemove { .. } | Inverse::FileRemove { .. } | Inverse::Emission => {
            return None;
        }
    };
    let same_place = named.kind == object.kind
        && named.project == object.project
        && named.parent == object.parent
        && named.name == object.name;
    (!same_place).then_some("has an inverse for another object")
}

/// Only an intent, a failure, and a revert of an intent that never applied carry no outcome. Every other state carries the
/// object and its inverse together.
fn check_outcome(record: &EffectRecord) -> Result<(), KernelError> {
    if matches!(record.inverse, Some(Inverse::IncusRestoreAclField { .. }))
        && record.acl_patch.is_none()
    {
        return Err(corrupt(
            record.id,
            "has an ACL array inverse without its intent",
        ));
    }
    if let Some(patch) = &record.acl_patch {
        let valid = record.class == EffectClass::A
            && record.target.kind == crate::ownership::IncusKind::Acl
            && record.target.project == "default"
            && record.target.parent.is_none()
            && record.target.uuid.is_none()
            && record.pre_image.is_none()
            && record.intent.is_none()
            && patch
                .previous
                .iter()
                .chain(&patch.rules)
                .all(serde_json::Value::is_object);
        if !valid {
            return Err(corrupt(record.id, "carries an invalid ACL array intent"));
        }
        if let Some(inverse) = &record.inverse {
            let expected = Inverse::IncusRestoreAclField {
                object: record.target.clone(),
                field: patch.field,
                rules: patch.previous.clone(),
            };
            if inverse != &expected {
                return Err(corrupt(record.id, "carries another ACL array inverse"));
            }
        }
    }
    let bare = record.object.is_none() && record.inverse.is_none();
    let whole = record.object.is_some() && record.inverse.is_some();
    let fits = match record.state {
        EffectState::Intent | EffectState::Failed => bare,
        EffectState::Reverted => bare || whole,
        EffectState::Applied | EffectState::Confirmed => whole,
    };
    if fits {
        Ok(())
    } else {
        Err(corrupt(
            record.id,
            "carries the wrong outcome for its state",
        ))
    }
}

fn corrupt(id: u64, why: &str) -> KernelError {
    KernelError::Journal(format!("effect {id} {why}"))
}

/// A decoded journal file. `keep` is the length of the prefix that holds whole lines, so a torn tail is cut at `keep`.
struct Decoded {
    records: Vec<EffectRecord>,
    keep: usize,
    torn: Option<String>,
}

/// Decodes whole lines. A last segment with no newline is torn and is dropped. Any newline-terminated line that does not
/// decode refuses the file: it was written whole, so it is damage, and skipping it could hide a recorded effect.
fn decode(bytes: &[u8]) -> Result<Decoded, KernelError> {
    let mut records = Vec::new();
    let mut start = 0;
    let mut number = 0usize;
    while start < bytes.len() {
        number += 1;
        let rest = &bytes[start..];
        let Some(end) = rest.iter().position(|&b| b == b'\n') else {
            return Ok(Decoded {
                records,
                keep: start,
                torn: Some(format!(
                    "a torn last line of {} bytes was dropped",
                    rest.len()
                )),
            });
        };
        let record = serde_json::from_slice::<EffectRecord>(&rest[..end])
            .map_err(|_| KernelError::Journal(format!("record {number} does not decode")))?;
        records.push(record);
        start += end + 1;
    }
    Ok(Decoded {
        records,
        keep: start,
        torn: None,
    })
}

/// Reads the whole journal through `file`, the handle this journal holds locked, so the bytes read are the bytes the lock
/// covers. Positional reads leave the handle's offset alone, and appends always go to the end.
fn read_all(file: &File) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match file.read_at(&mut chunk, bytes.len() as u64) {
            Ok(0) => return Ok(bytes),
            Ok(n) => bytes.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

fn sync_parent(path: &Path) -> Result<(), KernelError> {
    let dir = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    File::open(dir)
        .and_then(|handle| handle.sync_all())
        .map_err(|e| io_error(&dir.display().to_string(), &e))
}

fn io_error(path: &str, e: &io::Error) -> KernelError {
    KernelError::Io {
        path: path.to_string(),
        why: e.to_string(),
    }
}

fn io_reason(path: &str, why: &str) -> KernelError {
    KernelError::Io {
        path: path.to_string(),
        why: why.to_string(),
    }
}

/// An object reference read with its fields closed. `ObjectRef` ignores unknown fields; a journal record may not.
mod closed {
    use serde::{Deserialize, Deserializer};

    use crate::ownership::{IncusKind, ObjectRef};

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Wire {
        kind: IncusKind,
        project: String,
        // Absent in records written before the parent field existed, so it is read as none there.
        #[serde(default)]
        parent: Option<String>,
        name: String,
        uuid: Option<String>,
    }

    impl From<Wire> for ObjectRef {
        fn from(wire: Wire) -> ObjectRef {
            ObjectRef {
                kind: wire.kind,
                project: wire.project,
                parent: wire.parent,
                name: wire.name,
                uuid: wire.uuid,
            }
        }
    }

    pub fn object<'de, D: Deserializer<'de>>(deserializer: D) -> Result<ObjectRef, D::Error> {
        Wire::deserialize(deserializer).map(ObjectRef::from)
    }

    pub fn optional<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<ObjectRef>, D::Error> {
        Option::<Wire>::deserialize(deserializer).map(|wire| wire.map(ObjectRef::from))
    }
}
