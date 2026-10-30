//! A fresh private directory, and this process's uid read from a file it has just made. Neither needs a libc call: a
//! file's owner is the uid its creator ran as, and a directory created here is private from its first moment.

use std::fs::{self, File};
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Creates a new directory of mode 0700 inside `parent`, under a random name. It never reuses a path: a name already
/// taken by anything, a symlink included, fails the call.
pub(crate) fn fresh_dir(parent: &Path) -> io::Result<PathBuf> {
    let mut raw = [0u8; 8];
    getrandom::fill(&mut raw)
        .map_err(|_| io::Error::other("the operating system's random source failed"))?;
    let dir = parent.join(format!(".arca-{}", hex(&raw)));
    fs::create_dir(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

/// The uid this process runs as, read from the owner of a file it creates now in a fresh private directory.
pub(crate) fn process_uid() -> io::Result<u32> {
    let dir = fresh_dir(&std::env::temp_dir())?;
    let made = File::create_new(dir.join("uid")).and_then(|file| file.metadata());
    let _ = fs::remove_dir_all(&dir);
    Ok(made?.uid())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
