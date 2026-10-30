//! Node identity: each node's ed25519 key, its id (the SHA-256 of the public key) and the file the key lives in.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::KernelError;
use crate::canon::{CanonError, Digest};

/// A node's id: the SHA-256 of its ed25519 public key. Serialized as 64 lowercase hex characters.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub [u8; 32]);

impl NodeId {
    /// The id of the node whose public key is `public`.
    pub fn of_public(public: &[u8; 32]) -> NodeId {
        NodeId(Digest::of(public).0)
    }

    /// The id as 64 lowercase hex characters.
    pub fn to_hex(&self) -> String {
        Digest(self.0).to_hex()
    }

    /// Reads exactly 64 lowercase hex characters.
    pub fn from_hex(s: &str) -> Result<NodeId, CanonError> {
        Digest::from_hex(s).map(|d| NodeId(d.0))
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NodeId({})", self.to_hex())
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl Serialize for NodeId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for NodeId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        NodeId::from_hex(&text).map_err(serde::de::Error::custom)
    }
}

/// A node's ed25519 signing key. Its `Debug` output names the node id only, never the secret.
pub struct NodeKey {
    key: SigningKey,
}

impl fmt::Debug for NodeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NodeKey({})", self.id())
    }
}

impl NodeKey {
    /// A new key from the operating system's randomness. Panics only if the operating system has no randomness source.
    pub fn generate() -> NodeKey {
        let seed = try_seed().expect("the operating system provides randomness");
        NodeKey::from_seed(seed)
    }

    fn from_seed(seed: [u8; 32]) -> NodeKey {
        NodeKey {
            key: SigningKey::from_bytes(&seed),
        }
    }

    /// Loads the key at `path`, or creates it there when the path does not exist.
    ///
    /// The file must be a regular file of 32 raw bytes with no group or other permission bit. A symbolic link at `path` is
    /// refused, and so is a file that changes between the check and the read.
    pub fn load_or_create(path: &Path) -> Result<NodeKey, KernelError> {
        let shown = path.display().to_string();
        match fs::symlink_metadata(path) {
            Ok(meta) => read_key(path, &shown, &meta),
            Err(e) if e.kind() == io::ErrorKind::NotFound => create_key(path, &shown),
            Err(e) => Err(io_error(&shown, &e)),
        }
    }

    /// The node id, the SHA-256 of the public key.
    pub fn id(&self) -> NodeId {
        NodeId::of_public(&self.public())
    }

    /// The 32-byte ed25519 public key.
    pub fn public(&self) -> [u8; 32] {
        self.key.verifying_key().to_bytes()
    }

    /// An ed25519 signature over `msg`.
    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.key.sign(msg).to_bytes()
    }
}

/// Whether `sig` is a valid ed25519 signature over `msg` by `public`. Uses strict verification, which refuses a
/// small-order key and a non-canonical signature.
pub fn verify(public: &[u8; 32], msg: &[u8], sig: &[u8; 64]) -> bool {
    let Ok(key) = VerifyingKey::from_bytes(public) else {
        return false;
    };
    key.verify_strict(msg, &Signature::from_bytes(sig)).is_ok()
}

fn try_seed() -> Result<[u8; 32], getrandom::Error> {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed)?;
    Ok(seed)
}

fn io_error(path: &str, e: &io::Error) -> KernelError {
    KernelError::Io {
        path: path.to_string(),
        why: e.to_string(),
    }
}

/// Reads an existing key. The checks run on the path's own metadata first (a link is never followed), then on the opened
/// file, so a path swapped between the two is refused.
fn read_key(path: &Path, shown: &str, meta: &fs::Metadata) -> Result<NodeKey, KernelError> {
    if meta.file_type().is_symlink() {
        return Err(KernelError::KeySymlink {
            path: shown.to_string(),
        });
    }
    if !meta.file_type().is_file() {
        return Err(KernelError::Io {
            path: shown.to_string(),
            why: "a node key must be a regular file".to_string(),
        });
    }
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(KernelError::KeyMode {
            path: shown.to_string(),
        });
    }
    if meta.len() != 32 {
        return Err(KernelError::Io {
            path: shown.to_string(),
            why: "a node key is 32 raw bytes".to_string(),
        });
    }
    let mut file = File::open(path).map_err(|e| io_error(shown, &e))?;
    let opened = file.metadata().map_err(|e| io_error(shown, &e))?;
    let again = fs::symlink_metadata(path).map_err(|e| io_error(shown, &e))?;
    if again.file_type().is_symlink() {
        return Err(KernelError::KeySymlink {
            path: shown.to_string(),
        });
    }
    if again.dev() != opened.dev() || again.ino() != opened.ino() {
        return Err(KernelError::Io {
            path: shown.to_string(),
            why: "the key file changed while it was opened".to_string(),
        });
    }
    let mut seed = [0u8; 32];
    file.read_exact(&mut seed)
        .map_err(|e| io_error(shown, &e))?;
    Ok(NodeKey::from_seed(seed))
}

/// Creates a new key with `create_new` (which never follows a link) and mode 0600, then syncs it and its directory, so a
/// crash cannot leave a key that changes identity on the next start.
fn create_key(path: &Path, shown: &str) -> Result<NodeKey, KernelError> {
    let seed = try_seed().map_err(|_| KernelError::Io {
        path: shown.to_string(),
        why: "the operating system gave no randomness".to_string(),
    })?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| io_error(shown, &e))?;
    if let Err(e) = file.write_all(&seed).and_then(|()| file.sync_all()) {
        let _ = fs::remove_file(path);
        return Err(io_error(shown, &e));
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    File::open(parent)
        .and_then(|dir| dir.sync_all())
        .map_err(|e| io_error(shown, &e))?;
    Ok(NodeKey::from_seed(seed))
}
