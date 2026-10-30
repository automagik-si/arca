use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use arca_kernel::KernelError;
use arca_kernel::canon::Digest;
use arca_kernel::identity::{NodeId, NodeKey, verify};

/// A private directory under the system temp directory, unique per process and per call, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "arca-kernel-identity-{tag}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("the scratch directory is created");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("scratch mode");
        Scratch(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_key(path: &Path, bytes: &[u8], mode: u32) {
    fs::write(path, bytes).expect("the key file is written");
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("the key mode is set");
}

fn mode_of(path: &Path) -> u32 {
    fs::metadata(path)
        .expect("the file exists")
        .permissions()
        .mode()
        & 0o777
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn a_missing_key_is_created_mode_0600_and_reloads_to_the_same_identity() {
    let dir = Scratch::new("create");
    let path = dir.path("node.key");
    let created = NodeKey::load_or_create(&path).expect("the key is created");
    assert_eq!(
        mode_of(&path),
        0o600,
        "a new key is owner read and write only"
    );
    assert_eq!(
        fs::read(&path).expect("read").len(),
        32,
        "the key is 32 raw bytes"
    );
    let reloaded = NodeKey::load_or_create(&path).expect("the key reloads");
    assert_eq!(reloaded.id(), created.id());
    assert_eq!(reloaded.public(), created.public());
}

#[test]
fn a_key_file_that_is_owner_read_only_is_accepted() {
    let dir = Scratch::new("owner-read");
    let path = dir.path("node.key");
    write_key(&path, &[3u8; 32], 0o400);
    let key = NodeKey::load_or_create(&path).expect("mode 0400 has no group or other bit");
    assert_eq!(
        key.id(),
        NodeKey::load_or_create(&path).expect("again").id()
    );
}

#[test]
fn a_key_file_with_any_group_or_other_bit_is_refused_and_not_rewritten() {
    for mode in [0o644, 0o640, 0o604, 0o601, 0o660] {
        let dir = Scratch::new("mode");
        let path = dir.path("node.key");
        let seed = [7u8; 32];
        write_key(&path, &seed, mode);
        let err = NodeKey::load_or_create(&path).expect_err("a group or other bit is refused");
        assert_eq!(
            err,
            KernelError::KeyMode {
                path: path.display().to_string()
            },
            "mode {mode:o}"
        );
        assert_eq!(
            fs::read(&path).expect("read"),
            seed.to_vec(),
            "the file is untouched"
        );
        assert_eq!(mode_of(&path), mode, "the mode is untouched");
    }
}

#[test]
fn a_symlinked_key_is_refused_even_when_its_target_is_private() {
    let dir = Scratch::new("symlink");
    let target = dir.path("real.key");
    write_key(&target, &[9u8; 32], 0o600);
    let link = dir.path("node.key");
    symlink(&target, &link).expect("the link is made");
    let err = NodeKey::load_or_create(&link).expect_err("a symlink is refused");
    assert_eq!(
        err,
        KernelError::KeySymlink {
            path: link.display().to_string()
        }
    );
}

#[test]
fn a_dangling_symlink_is_refused_and_its_target_is_not_created() {
    let dir = Scratch::new("dangling");
    let target = dir.path("absent.key");
    let link = dir.path("node.key");
    symlink(&target, &link).expect("the link is made");
    let err = NodeKey::load_or_create(&link).expect_err("a dangling symlink is refused");
    assert!(matches!(err, KernelError::KeySymlink { .. }), "got {err:?}");
    assert!(!target.exists(), "the symlink target is never created");
}

#[test]
fn a_key_file_that_is_not_32_bytes_is_refused() {
    let dir = Scratch::new("length");
    let path = dir.path("node.key");
    write_key(&path, &[1u8; 31], 0o600);
    let err = NodeKey::load_or_create(&path).expect_err("31 bytes is not a key");
    assert!(matches!(err, KernelError::Io { .. }), "got {err:?}");
}

#[test]
fn errors_name_the_path_and_never_the_key_bytes() {
    let dir = Scratch::new("display");
    let path = dir.path("node.key");
    let seed = [0xabu8; 32];
    write_key(&path, &seed, 0o644);
    let err = NodeKey::load_or_create(&path).expect_err("refused");
    let shown = err.to_string();
    assert!(
        shown.contains(&path.display().to_string()),
        "the path is named: {shown}"
    );
    assert!(!shown.contains(&hex(&seed)), "no key bytes in the message");
}

#[test]
fn the_debug_output_of_a_key_never_shows_its_secret() {
    let dir = Scratch::new("debug");
    let path = dir.path("node.key");
    let seed = [0x5au8; 32];
    write_key(&path, &seed, 0o600);
    let key = NodeKey::load_or_create(&path).expect("the key loads");
    let shown = format!("{key:?}");
    assert!(
        !shown.contains(&hex(&seed)),
        "the secret is not in Debug output"
    );
    assert!(
        shown.contains(&key.id().to_hex()),
        "the id is what Debug names"
    );
}

#[test]
fn node_id_is_the_sha256_of_the_public_key() {
    let key = NodeKey::generate();
    assert_eq!(key.id(), NodeId(Digest::of(&key.public()).0));
    assert_eq!(key.id(), NodeId::of_public(&key.public()));
}

#[test]
fn node_id_reads_back_its_own_hex_and_refuses_anything_else() {
    let id = NodeKey::generate().id();
    assert_eq!(NodeId::from_hex(&id.to_hex()), Ok(id));
    assert!(
        NodeId::from_hex(&"A".repeat(64)).is_err(),
        "uppercase is refused"
    );
    assert!(NodeId::from_hex("abcd").is_err(), "a short id is refused");
}

#[test]
fn node_id_serializes_as_hex_and_reads_back() {
    let id = NodeKey::generate().id();
    let text = serde_json::to_string(&id).expect("serializes");
    assert_eq!(text, format!("\"{}\"", id.to_hex()));
    let back: NodeId = serde_json::from_str(&text).expect("reads back");
    assert_eq!(back, id);
    assert!(serde_json::from_str::<NodeId>("\"not-hex\"").is_err());
}

#[test]
fn a_signature_verifies_only_for_its_key_and_its_bytes() {
    let key = NodeKey::generate();
    let sig = key.sign(b"message");
    assert!(verify(&key.public(), b"message", &sig));
    assert!(
        !verify(&key.public(), b"massage", &sig),
        "a changed message fails"
    );
    let other = NodeKey::generate();
    assert!(
        !verify(&other.public(), b"message", &sig),
        "another key fails"
    );
    let mut flipped = sig;
    flipped[0] ^= 1;
    assert!(
        !verify(&key.public(), b"message", &flipped),
        "a changed signature fails"
    );
}

#[test]
fn a_signature_by_a_small_order_key_is_refused() {
    // The identity point (0, 1) encoded as a little-endian y coordinate of 1 is a small-order public key.
    let mut identity_point = [0u8; 32];
    identity_point[0] = 1;
    let sig = [0u8; 64];
    assert!(
        !verify(&identity_point, b"message", &sig),
        "strict verification refuses it"
    );
}

#[test]
fn concurrent_creates_of_one_path_all_load_the_same_key() {
    use std::sync::{Arc, Barrier};
    use std::thread;

    let dir = Scratch::new("race");
    for round in 0..16 {
        let path = dir.path(&format!("node-{round}.key"));
        let barrier = Arc::new(Barrier::new(4));
        let racers: Vec<_> = (0..4)
            .map(|_| {
                let path = path.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    NodeKey::load_or_create(&path).map(|key| key.public())
                })
            })
            .collect();
        let results: Vec<_> = racers
            .into_iter()
            .map(|racer| racer.join().expect("a racer does not panic"))
            .collect();
        let first = results[0].clone().expect("the first racer gets a key");
        for result in results {
            assert_eq!(
                result.expect("every racer gets a key, none an error"),
                first,
                "every racer loads the one key round {round}"
            );
        }
    }
}
