//! Temporary trees for tests. Every folder is made under the system's temp folder with a `janitor-test-` name, and
//! only such a folder is ever removed.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(label: &str) -> TempDir {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir()
            .canonicalize()
            .unwrap_or_else(|_| std::env::temp_dir());
        let path = base.join(format!(
            "janitor-test-{label}-{}-{n}-{}",
            std::process::id(),
            crate::util::now()
        ));
        fs::create_dir_all(&path).expect("create a temp dir");
        TempDir { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let name_ok = self
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("janitor-test-"));
        if name_ok {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

/// Set a path's modification time to `days` ago (links are left alone).
pub fn age(path: &Path, days: u64) {
    if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return;
    }
    let when = std::time::SystemTime::now() - std::time::Duration::from_secs(days * 86_400);
    fs::File::open(path)
        .and_then(|f| f.set_modified(when))
        .unwrap_or_else(|e| panic!("age {}: {e}", path.display()));
}

/// Age a whole tree (every entry, deepest first).
pub fn age_tree(root: &Path, days: u64) {
    let mut all = vec![root.to_path_buf()];
    let mut i = 0;
    while i < all.len() {
        let p = all[i].clone();
        if let Ok(md) = fs::symlink_metadata(&p)
            && md.is_dir()
            && let Ok(entries) = fs::read_dir(&p)
        {
            for e in entries.flatten() {
                all.push(e.path());
            }
        }
        i += 1;
    }
    for p in all.iter().rev() {
        age(p, days);
    }
}
