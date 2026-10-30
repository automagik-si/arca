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
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::KernelError;
use crate::canon::{Digest, canonical_json};
use crate::desired::EntryId;
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

impl From<InverseWire> for Inverse {
    fn from(wire: InverseWire) -> Self {
        match wire {
            InverseWire::IncusDelete { object } => Inverse::IncusDelete { object },
            InverseWire::IncusRestoreConfig { object, config } => {
                Inverse::IncusRestoreConfig { object, config }
            }
            InverseWire::AclRuleRemove { project, acl, rule } => {
                Inverse::AclRuleRemove { project, acl, rule }
            }
            InverseWire::FileRemove { path, digest } => Inverse::FileRemove { path, digest },
            InverseWire::Emission {} => Inverse::Emission,
        }
    }
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
/// record for an id is the effect's state. An `Intent` has no object and no inverse; every later state of an applied effect
/// carries both.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectRecord {
    pub id: u64,
    pub entry: EntryId,
    pub class: EffectClass,
    pub op_digest: Digest,
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
    /// Records the intent of an effect before it runs, and returns the id that names it.
    fn intend(
        &mut self,
        entry: &EntryId,
        class: EffectClass,
        op_digest: Digest,
        now_ms: u64,
    ) -> Result<u64, KernelError>;
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
        let created = match fs::symlink_metadata(path) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(io_reason(&shown, "a symbolic link; refused"));
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
        let bytes = fs::read(path).map_err(|e| io_error(&shown, &e))?;
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
        let bytes = fs::read(&self.path).map_err(|e| io_error(&self.shown(), &e))?;
        let decoded = decode(&bytes)?;
        Ok((decoded.records, self.torn.clone().or(decoded.torn)))
    }
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
                Some(prev) if !is_successor(prev, &record) => {
                    return Err(corrupt(record.id, "does not follow its last record"));
                }
                Some(_) => {}
            }
            latest.insert(record.id, record);
        }
        let next_id = latest.keys().next_back().map_or(1, |last| last + 1);
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
            object,
            inverse,
            state: to,
            at_ms: prev.at_ms,
        };
        if !is_successor(&prev, &next) {
            return Err(corrupt(
                id,
                &format!("cannot move from {:?} to {:?}", prev.state, to),
            ));
        }
        self.write(next)
    }

    fn settle(&mut self, id: u64, to: EffectState) {
        if let Err(e) = self.advance(id, to, None) {
            // The first failure is the one reported; a later one never replaces it.
            self.poison = self.poison.take().or(Some(e));
        }
    }
}

impl<S: JournalStore> JournalSink for Journal<S> {
    fn intend(
        &mut self,
        entry: &EntryId,
        class: EffectClass,
        op_digest: Digest,
        now_ms: u64,
    ) -> Result<u64, KernelError> {
        let id = self.next_id;
        self.write(EffectRecord {
            id,
            entry: entry.clone(),
            class,
            op_digest,
            object: None,
            inverse: None,
            state: EffectState::Intent,
            at_ms: now_ms,
        })?;
        self.next_id += 1;
        Ok(id)
    }

    fn applied(&mut self, id: u64, object: ObjectRef, inverse: Inverse) -> Result<(), KernelError> {
        self.advance(id, EffectState::Applied, Some((object, inverse)))
    }

    fn failed(&mut self, id: u64, _why: &str) -> Result<(), KernelError> {
        self.advance(id, EffectState::Failed, None)
    }
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
    /// One plan for every class-B object, or `None` when there are none.
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
                if let Some(object) = record.object {
                    objects.push(object);
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

/// Whether `next` may follow `prev` for the same effect: a legal state move, with the same entry, class, digest and time.
fn is_successor(prev: &EffectRecord, next: &EffectRecord) -> bool {
    let legal = matches!(
        (prev.state, next.state),
        (
            EffectState::Intent,
            EffectState::Applied | EffectState::Failed
        ) | (
            EffectState::Applied,
            EffectState::Confirmed | EffectState::Reverted
        ) | (EffectState::Confirmed, EffectState::Reverted)
    );
    legal
        && prev.id == next.id
        && prev.entry == next.entry
        && prev.class == next.class
        && prev.op_digest == next.op_digest
        && prev.at_ms == next.at_ms
}

/// Only an intent carries no outcome; every state after it carries an object and an inverse, except a failure.
fn check_outcome(record: &EffectRecord) -> Result<(), KernelError> {
    let bare = record.object.is_none() && record.inverse.is_none();
    let whole = record.object.is_some() && record.inverse.is_some();
    let fits = match record.state {
        EffectState::Intent | EffectState::Failed => bare,
        EffectState::Applied | EffectState::Confirmed | EffectState::Reverted => whole,
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
        name: String,
        uuid: Option<String>,
    }

    impl From<Wire> for ObjectRef {
        fn from(wire: Wire) -> ObjectRef {
            ObjectRef {
                kind: wire.kind,
                project: wire.project,
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
