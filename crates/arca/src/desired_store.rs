//! The desired-state store: the one signed document this node holds, the entries it carries, and the two sets the reconciler
//! keeps across a restart. The store lives in the state directory under an exclusive lock. Every document it holds is verified
//! against this node's own key when the store opens, and a new document is written whole and atomically, only after the gate
//! has accepted it.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, ErrorKind, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use arca_kernel::KernelError;
use arca_kernel::canon::Digest;
use arca_kernel::desired::{
    Applied, DesiredStateGate, DocRefusal, Document, Entry, EntryId, PluginId, SignedDocument,
    Target, sign_document,
};
use arca_kernel::identity::{NodeId, NodeKey};
use serde::{Deserialize, Serialize};

use crate::handlers::iso_utc;

/// The file the signed document, the converged set and the retained set are written to.
pub const STATE_FILE: &str = "desired.json";
/// The lock file. Its exclusive lock is held for as long as the store is open.
pub const LOCK_FILE: &str = "desired.lock";
/// The file a new state is written to before it replaces [`STATE_FILE`].
const TEMP_FILE: &str = "desired.json.tmp";
/// How long a document is valid from its issue. A document past its expiry holds the applied state, as the gate reports it stale.
pub const DOCUMENT_TTL_MS: u64 = 30 * 24 * 60 * 60 * 1000;

/// One change to the entries a document holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryChange {
    /// Adds the entry, or replaces the held entry with the same id in place.
    Put(Entry),
    /// Removes the held entry with this id. An entry the document does not hold is refused.
    Remove(EntryId),
}

impl EntryChange {
    fn id(&self) -> &EntryId {
        match self {
            Self::Put(entry) => &entry.id,
            Self::Remove(id) => id,
        }
    }
}

/// What the state file holds. Every key is named, and an unknown key refuses the file.
#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct StateFile {
    current: Option<SignedDocument>,
    converged: Vec<Entry>,
    retained: Vec<RetainedRecord>,
}

/// One retained entry: the plugin that owned it and the digest of the declined plan.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetainedRecord {
    entry: EntryId,
    plugin: PluginId,
    plan: Digest,
}

type Retained = BTreeMap<EntryId, (PluginId, Digest)>;

/// The desired-state store for one node. It holds the node key that signs every document it writes.
pub struct DesiredStore {
    dir: PathBuf,
    key: NodeKey,
    /// The lock on the state directory. It is released when the store is dropped.
    _lock: File,
    current: Option<SignedDocument>,
    applied: Option<Applied>,
    converged: Vec<Entry>,
    retained: Retained,
}

impl DesiredStore {
    /// Opens the store in `dir`, taking its lock. A store that another open holds is refused as [`KernelError::Locked`]. The
    /// state file, when there is one, must be private and must verify as this node's own signed document; anything else is
    /// refused and nothing is changed.
    pub fn open(dir: &Path, key: NodeKey) -> Result<Self, KernelError> {
        let lock_path = dir.join(LOCK_FILE);
        let lock = open_private(&lock_path)?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(KernelError::Locked {
                    path: lock_path.display().to_string(),
                });
            }
            Err(TryLockError::Error(error)) => return Err(io_error(&lock_path, &error)),
        }
        let state = read_state(&dir.join(STATE_FILE))?;
        let signer = key.public();
        let applied = match &state.current {
            None => None,
            Some(signed) => {
                let base = base_of(&signed.document).map_err(KernelError::Refused)?;
                let mut gate = DesiredStateGate::new(signer, base);
                // The stored document is judged at time zero, so an expired document is still verified as this node's own.
                match gate.accept(signed, 0) {
                    Ok(Target::Apply(applied)) => Some(applied),
                    Ok(Target::Hold { .. }) => {
                        return Err(KernelError::Refused(DocRefusal::Malformed(
                            "the stored document has no validity".to_string(),
                        )));
                    }
                    Err(refusal) => return Err(KernelError::Refused(refusal)),
                }
            }
        };
        let retained = state
            .retained
            .into_iter()
            .map(|record| (record.entry, (record.plugin, record.plan)))
            .collect();
        Ok(DesiredStore {
            dir: dir.to_path_buf(),
            key,
            _lock: lock,
            current: state.current,
            applied,
            converged: state.converged,
            retained,
        })
    }

    /// The public key of this node: the signer every document must carry.
    pub fn signer(&self) -> [u8; 32] {
        self.key.public()
    }

    /// The document the store holds, if one was ever proposed.
    pub fn current(&self) -> Option<&SignedDocument> {
        self.current.as_ref()
    }

    /// The document the gate last applied, if any.
    pub fn applied(&self) -> Option<&Applied> {
        self.applied.as_ref()
    }

    /// The entries that applied with their current config, as the reconciler last reported them.
    pub fn converged(&self) -> &[Entry] {
        &self.converged
    }

    /// The entries whose plans were declined, with the plugin that owned each and the declined plan's digest.
    pub fn retained(&self) -> &Retained {
        &self.retained
    }

    /// Proposes the change list as the next document. The change list is applied to the entries the current document holds,
    /// and the result is signed with the node key, checked through the gate, and written to the state file before it becomes
    /// the current document. A refused change list persists nothing: either every change lands, or none does.
    pub fn propose(
        &mut self,
        changes: Vec<EntryChange>,
        now_ms: u64,
    ) -> Result<SignedDocument, KernelError> {
        let mut named = BTreeSet::new();
        for change in &changes {
            if !named.insert(change.id().0.as_str()) {
                return Err(KernelError::Refused(DocRefusal::DuplicateEntry(
                    change.id().clone(),
                )));
            }
        }
        let (seq, prev, mut entries) = match &self.current {
            None => (1, None, Vec::new()),
            Some(signed) => (
                signed.document.seq + 1,
                Some(signed.document.digest()),
                signed.document.entries.clone(),
            ),
        };
        for change in changes {
            match change {
                EntryChange::Put(entry) => {
                    match entries.iter().position(|held| held.id == entry.id) {
                        Some(index) => entries[index] = entry,
                        None => entries.push(entry),
                    }
                }
                EntryChange::Remove(id) => match entries.iter().position(|held| held.id == id) {
                    Some(index) => {
                        entries.remove(index);
                    }
                    None => {
                        return Err(KernelError::Refused(DocRefusal::Malformed(
                            "a removed entry is not in the document".to_string(),
                        )));
                    }
                },
            }
        }
        let document = Document {
            node: NodeId::of_public(&self.signer()),
            seq,
            prev,
            issued_at: iso_utc(now_ms),
            expires_at: iso_utc(now_ms.saturating_add(DOCUMENT_TTL_MS)),
            entries,
        };
        let signed = sign_document(document, &self.key);
        let mut gate = DesiredStateGate::new(self.signer(), self.applied.clone());
        let applied = match gate.accept(&signed, now_ms) {
            Ok(Target::Apply(applied)) => applied,
            Ok(Target::Hold { .. }) => {
                return Err(KernelError::Refused(DocRefusal::Malformed(
                    "the document is already expired".to_string(),
                )));
            }
            Err(refusal) => return Err(KernelError::Refused(refusal)),
        };
        let next = StateFile {
            current: Some(signed.clone()),
            converged: self.converged.clone(),
            retained: records(&self.retained),
        };
        self.persist(&next)?;
        self.current = Some(signed.clone());
        self.applied = Some(applied);
        Ok(signed)
    }

    /// Persists the converged set and the retained set the reconciler now holds, keeping the current document. Nothing changes
    /// in memory unless the state is written.
    pub fn save_applied(
        &mut self,
        converged: Vec<Entry>,
        retained: Retained,
    ) -> Result<(), KernelError> {
        let next = StateFile {
            current: self.current.clone(),
            converged,
            retained: records(&retained),
        };
        self.persist(&next)?;
        self.converged = next.converged;
        self.retained = retained;
        Ok(())
    }

    /// Writes `state` to a new file, makes it durable, and renames it over the state file. A failure leaves the old file in
    /// place.
    fn persist(&self, state: &StateFile) -> Result<(), KernelError> {
        let bytes = serde_json::to_vec(state).map_err(|_| KernelError::Io {
            path: STATE_FILE.to_string(),
            why: "the state does not serialize".to_string(),
        })?;
        let temp = self.dir.join(TEMP_FILE);
        match fs::remove_file(&temp) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(io_error(&temp, &error)),
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
            .map_err(|error| io_error(&temp, &error))?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| io_error(&temp, &error))?;
        drop(file);
        let path = self.dir.join(STATE_FILE);
        fs::rename(&temp, &path).map_err(|error| io_error(&path, &error))?;
        File::open(&self.dir)
            .and_then(|dir| dir.sync_all())
            .map_err(|error| io_error(&self.dir, &error))?;
        Ok(())
    }
}

impl fmt::Debug for DesiredStore {
    // The node key is left out: a store's debug text must never carry key material.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DesiredStore")
            .field("dir", &self.dir)
            .field(
                "seq",
                &self.current.as_ref().map(|signed| signed.document.seq),
            )
            .finish_non_exhaustive()
    }
}

/// The applied state a stored document continues from. The first document continues from nothing; any later one names the
/// document before it by its digest, and the sequence number before it.
fn base_of(doc: &Document) -> Result<Option<Applied>, DocRefusal> {
    match (doc.seq, &doc.prev) {
        (0, _) => Err(DocRefusal::Malformed("seq starts at 1".to_string())),
        (1, _) => Ok(None),
        (seq, Some(prev)) => Ok(Some(Applied {
            seq: seq - 1,
            digest: prev.clone(),
        })),
        (_, None) => Err(DocRefusal::Malformed(
            "a later document names the one it follows".to_string(),
        )),
    }
}

fn records(retained: &Retained) -> Vec<RetainedRecord> {
    retained
        .iter()
        .map(|(entry, (plugin, plan))| RetainedRecord {
            entry: entry.clone(),
            plugin: plugin.clone(),
            plan: plan.clone(),
        })
        .collect()
}

/// Opens `path` for reading and writing, creating it at mode 0600 when it is missing. The mode is set again on an open file,
/// so a file that existed with other bits is made private.
fn open_private(path: &Path) -> Result<File, KernelError> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(|error| io_error(path, &error))?;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|error| io_error(path, &error))?;
    Ok(file)
}

/// The state file as it is on disk. A file that is missing is an empty state. A link, a file that is not regular, a file with a
/// group or other bit, or one that does not parse is refused.
fn read_state(path: &Path) -> Result<StateFile, KernelError> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(StateFile::default()),
        Err(error) => return Err(io_error(path, &error)),
    };
    let refuse = |why: &str| KernelError::Io {
        path: path.display().to_string(),
        why: why.to_string(),
    };
    if meta.file_type().is_symlink() {
        return Err(refuse("the state file is a symbolic link"));
    }
    if !meta.is_file() {
        return Err(refuse("the state file is not a regular file"));
    }
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(refuse(
            "the state file has a group or other permission bit; it must be mode 0600",
        ));
    }
    let bytes = fs::read(path).map_err(|error| io_error(path, &error))?;
    serde_json::from_slice(&bytes).map_err(|_| refuse("the state file does not parse"))
}

fn io_error(path: &Path, error: &io::Error) -> KernelError {
    KernelError::Io {
        path: path.display().to_string(),
        why: error.to_string(),
    }
}
