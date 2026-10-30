//! Node identity: each node's ed25519 key, its id (the SHA-256 of the public key) and the file the key lives in.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};

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
    /// A symbolic link at `path` is refused, and so is anything at `path` that is not a regular file. The key file is then
    /// opened, and the checks that decide the key run on the opened file: it must be a regular file of 32 raw bytes with no
    /// group or other permission bit. The owner is not compared with the process's user id, which the standard library does
    /// not expose. The path must still name the opened file afterwards, or the key is refused. A missing key is created
    /// atomically: a creator that finds a key already linked at `path` loads that key and does not replace it.
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

fn io_reason(path: &str, why: &str) -> KernelError {
    KernelError::Io {
        path: path.to_string(),
        why: why.to_string(),
    }
}

/// Reads an existing key. The path's own metadata is taken first: a link is refused, and anything else that is not a regular
/// file is refused before it is opened, so a FIFO is never opened. Then the file is opened, and the checks that decide the
/// key run on the opened file, not on the metadata taken from the path. Last, the path must still name the opened file, so
/// a path replaced after the open is refused.
fn read_key(path: &Path, shown: &str, meta: &fs::Metadata) -> Result<NodeKey, KernelError> {
    if meta.file_type().is_symlink() {
        return Err(KernelError::KeySymlink {
            path: shown.to_string(),
        });
    }
    if !meta.file_type().is_file() {
        return Err(io_reason(shown, "a node key must be a regular file"));
    }
    let mut file = File::open(path).map_err(|e| io_error(shown, &e))?;
    let opened = file.metadata().map_err(|e| io_error(shown, &e))?;
    check_opened(&opened, shown)?;
    let again = fs::symlink_metadata(path).map_err(|e| io_error(shown, &e))?;
    if again.file_type().is_symlink() {
        return Err(KernelError::KeySymlink {
            path: shown.to_string(),
        });
    }
    if again.dev() != opened.dev() || again.ino() != opened.ino() {
        return Err(io_reason(shown, "the key file changed while it was opened"));
    }
    let mut seed = [0u8; 32];
    file.read_exact(&mut seed)
        .map_err(|e| io_error(shown, &e))?;
    Ok(NodeKey::from_seed(seed))
}

/// The checks that decide a key, run on the file that was opened.
fn check_opened(opened: &fs::Metadata, shown: &str) -> Result<(), KernelError> {
    if !opened.file_type().is_file() {
        return Err(io_reason(shown, "a node key must be a regular file"));
    }
    if opened.permissions().mode() & 0o077 != 0 {
        return Err(KernelError::KeyMode {
            path: shown.to_string(),
        });
    }
    if opened.len() != 32 {
        return Err(io_reason(shown, "a node key is 32 raw bytes"));
    }
    Ok(())
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A name for a temporary file beside the key: unique within this process, and across processes by the process id.
fn temp_name(name: &OsStr) -> OsString {
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut out = OsString::from(".");
    out.push(name);
    out.push(format!(".{}.{n}.tmp", process::id()));
    out
}

/// Writes `seed` to a new file at `temp`, mode 0600 whatever the umask, and syncs it.
fn write_temp(temp: &Path, seed: &[u8; 32]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(temp)?;
    file.write_all(seed)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.sync_all()
}

/// Creates a new key atomically. The seed is written to a temporary file in the key's directory and synced, then
/// hard-linked to `path`. A link never replaces a file, so an existing key is never overwritten: a creator that finds one
/// loads it. The directory is synced after the link, so a crash cannot leave a key that changes identity on the next start.
fn create_key(path: &Path, shown: &str) -> Result<NodeKey, KernelError> {
    let seed =
        try_seed().map_err(|_| io_reason(shown, "the operating system gave no randomness"))?;
    let name = path
        .file_name()
        .ok_or_else(|| io_reason(shown, "the key path names no file"))?;
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let temp = dir.join(temp_name(name));
    if let Err(e) = write_temp(&temp, &seed) {
        let _ = fs::remove_file(&temp);
        return Err(io_error(shown, &e));
    }
    if let Err(e) = fs::hard_link(&temp, path) {
        let _ = fs::remove_file(&temp);
        if e.kind() == io::ErrorKind::AlreadyExists {
            // Another creator linked its key first: load that key, which this call never replaces.
            let meta = fs::symlink_metadata(path).map_err(|e| io_error(shown, &e))?;
            return read_key(path, shown, &meta);
        }
        return Err(io_error(shown, &e));
    }
    fs::remove_file(&temp)
        .and_then(|()| File::open(dir))
        .and_then(|parent| parent.sync_all())
        .map_err(|e| io_error(shown, &e))?;
    Ok(NodeKey::from_seed(seed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A private directory under the system temp directory, removed on drop.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir().join(format!(
                "arca-kernel-identity-unit-{tag}-{}-{nanos}",
                process::id()
            ));
            fs::create_dir_all(&dir).expect("the scratch directory is created");
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("scratch mode");
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write(path: &Path, bytes: &[u8], mode: u32) {
        fs::write(path, bytes).expect("written");
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("mode set");
    }

    /// `read_key` takes the path's own metadata from one file while the path names another. That is the state after a
    /// replacement lands between the lstat and the open, and the checks must judge the file that was opened.
    #[test]
    fn the_mode_is_judged_on_the_opened_file_not_on_the_metadata_taken_from_the_path() {
        let dir = Scratch::new("mode");
        let good = dir.0.join("good.key");
        write(&good, &[1u8; 32], 0o600);
        let good_meta = fs::symlink_metadata(&good).expect("metadata");
        let loose = dir.0.join("loose.key");
        write(&loose, &[2u8; 32], 0o644);
        let shown = loose.display().to_string();
        assert_eq!(
            read_key(&loose, &shown, &good_meta).expect_err("a group bit is refused"),
            KernelError::KeyMode { path: shown }
        );
    }

    #[test]
    fn the_length_is_judged_on_the_opened_file_not_on_the_metadata_taken_from_the_path() {
        let dir = Scratch::new("length");
        let good = dir.0.join("good.key");
        write(&good, &[1u8; 32], 0o600);
        let good_meta = fs::symlink_metadata(&good).expect("metadata");
        let short = dir.0.join("short.key");
        write(&short, &[2u8; 31], 0o600);
        let shown = short.display().to_string();
        let err = read_key(&short, &shown, &good_meta).expect_err("31 bytes is not a key");
        assert!(
            matches!(&err, KernelError::Io { why, .. } if why.contains("32 raw bytes")),
            "got {err:?}"
        );
    }

    #[test]
    fn a_directory_is_refused_as_not_a_regular_file_on_the_opened_file() {
        let dir = Scratch::new("dir");
        let good = dir.0.join("good.key");
        write(&good, &[1u8; 32], 0o600);
        let good_meta = fs::symlink_metadata(&good).expect("metadata");
        let sub = dir.0.join("sub");
        fs::create_dir(&sub).expect("directory");
        let shown = sub.display().to_string();
        let err = read_key(&sub, &shown, &good_meta).expect_err("a directory is not a key");
        assert!(
            matches!(&err, KernelError::Io { why, .. } if why.contains("regular file")),
            "got {err:?}"
        );
    }

    /// `create_key` on a path that already holds a key must load that key and leave it alone. This is the outcome of two
    /// creators racing: the loser must not fail, and must not replace the winner's key.
    #[test]
    fn a_create_that_finds_a_key_at_the_path_loads_it_and_leaves_it() {
        let dir = Scratch::new("exists");
        let path = dir.0.join("node.key");
        let existing = [5u8; 32];
        write(&path, &existing, 0o600);
        let shown = path.display().to_string();
        let key = create_key(&path, &shown).expect("the existing key loads");
        assert_eq!(key.public(), NodeKey::from_seed(existing).public());
        assert_eq!(fs::read(&path).expect("read"), existing.to_vec());
        let names: Vec<_> = fs::read_dir(&dir.0)
            .expect("list")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("node.key")]);
    }

    #[test]
    fn a_new_key_leaves_no_temporary_file_behind() {
        let dir = Scratch::new("temp");
        let path = dir.0.join("node.key");
        let shown = path.display().to_string();
        create_key(&path, &shown).expect("created");
        let names: Vec<_> = fs::read_dir(&dir.0)
            .expect("list")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("node.key")]);
        assert_eq!(
            fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
            0o600
        );
    }
}
