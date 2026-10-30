//! A FIFO at the journal path is refused as not a regular file. This lives in its own test binary because the FIFO is made by
//! spawning a process. A spawned child holds a copy of every open descriptor until it execs, and a copy keeps a journal's
//! lock held, so the journal tests in other binaries must never run beside this one.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use arca_kernel::KernelError;
use arca_kernel::journal::FileJournal;

/// A private directory under the system temp directory, unique per process and per call, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "arca-kernel-journal-{tag}-{}-{nanos}",
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

#[test]
fn a_fifo_at_the_journal_path_is_refused_as_not_a_regular_file() {
    // A FIFO made at mode 0600 has no group or other bit, so only the regular-file check refuses it. A write to a non-file
    // would be lost without an error.
    let dir = Scratch::new("fifo");
    let path = dir.path("journal.jsonl");
    let made = std::process::Command::new("mkfifo")
        .arg("-m")
        .arg("600")
        .arg(&path)
        .status()
        .expect("mkfifo runs");
    assert!(made.success(), "the test needs a FIFO");
    assert!(matches!(
        FileJournal::open(&path),
        Err(KernelError::Io { .. })
    ));
}
