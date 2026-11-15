//! Walking a tree the way `find -xdev` does: one filesystem, links never followed, mount points skipped.

use std::collections::HashSet;
use std::fs::{self, Metadata};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::jobs::{Ctx, Stop};

/// What a visitor wants next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visit {
    /// Go on, and into this folder.
    Continue,
    /// Go on, but not into this folder.
    Prune,
    /// End the whole walk.
    Stop,
}

pub struct Walk<'a> {
    /// The deepest level visited; children of the root are level 1.
    pub max_depth: Option<usize>,
    /// Folder names neither visited nor entered.
    pub prune: &'a [&'a str],
    pub mounts: &'a HashSet<PathBuf>,
    pub ctx: Option<&'a Ctx>,
}

impl<'a> Walk<'a> {
    pub fn new(mounts: &'a HashSet<PathBuf>, ctx: Option<&'a Ctx>) -> Walk<'a> {
        Walk {
            max_depth: None,
            prune: &[],
            mounts,
            ctx,
        }
    }

    pub fn depth(mut self, depth: usize) -> Self {
        self.max_depth = Some(depth);
        self
    }

    pub fn prune(mut self, names: &'a [&'a str]) -> Self {
        self.prune = names;
        self
    }

    /// Visit everything below `root` (not the root itself). Returns false when the visitor stopped the walk.
    pub fn run(
        &self,
        root: &Path,
        visit: &mut dyn FnMut(&Path, &Metadata, usize) -> Visit,
    ) -> Result<bool, Stop> {
        let Ok(root_md) = fs::symlink_metadata(root) else {
            return Ok(true);
        };
        if !root_md.is_dir() {
            return Ok(true);
        }
        let dev = root_md.dev();
        let mut stack = vec![(root.to_path_buf(), 0usize)];
        let mut seen = 0u64;
        while let Some((dir, depth)) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                seen += 1;
                if seen.is_multiple_of(256)
                    && let Some(ctx) = self.ctx
                {
                    ctx.checkpoint()?;
                }
                let path = entry.path();
                let Ok(md) = fs::symlink_metadata(&path) else {
                    continue;
                };
                let level = depth + 1;
                if md.is_dir() {
                    if md.dev() != dev || self.mounts.contains(&path) {
                        continue;
                    }
                    let name = entry.file_name();
                    if self.prune.iter().any(|p| name.to_str() == Some(p)) {
                        continue;
                    }
                }
                match visit(&path, &md, level) {
                    Visit::Stop => return Ok(false),
                    Visit::Prune => continue,
                    Visit::Continue => {}
                }
                if md.is_dir() && self.max_depth.is_none_or(|m| level < m) {
                    stack.push((path, level));
                }
            }
        }
        Ok(true)
    }
}

/// Disk bytes a file or folder takes (blocks, so sparse files count what they use), on its own filesystem only,
/// each hard-linked file once.
pub fn usage(path: &Path, mounts: &HashSet<PathBuf>, ctx: Option<&Ctx>) -> Result<u64, Stop> {
    let Ok(md) = fs::symlink_metadata(path) else {
        return Ok(0);
    };
    let mut total = md.blocks() * 512;
    if !md.is_dir() {
        return Ok(total);
    }
    let mut links = HashSet::new();
    Walk::new(mounts, ctx).run(path, &mut |_, md, _| {
        if md.nlink() > 1 && !md.is_dir() && !links.insert((md.dev(), md.ino())) {
            return Visit::Continue;
        }
        total += md.blocks() * 512;
        Visit::Continue
    })?;
    Ok(total)
}

/// Whether anything at or below `path` (to `max_depth`, skipping `prune`) changed after `cutoff` (epoch seconds):
/// `find path -xdev [-maxdepth N] -newermt @cutoff`.
pub fn changed_since(
    path: &Path,
    cutoff: u64,
    max_depth: Option<usize>,
    prune: &[&str],
    mounts: &HashSet<PathBuf>,
) -> bool {
    let Ok(md) = fs::symlink_metadata(path) else {
        return false;
    };
    if mtime(&md) > cutoff {
        return true;
    }
    let mut walk = Walk::new(mounts, None).prune(prune);
    walk.max_depth = max_depth;
    let mut found = false;
    let _ = walk.run(path, &mut |_, md, _| {
        if mtime(md) > cutoff {
            found = true;
            Visit::Stop
        } else {
            Visit::Continue
        }
    });
    found
}

pub fn mtime(md: &Metadata) -> u64 {
    u64::try_from(md.mtime()).unwrap_or(0)
}

/// Remove a file, a link, or a folder and everything in it, never crossing into another filesystem and never
/// following a link (a link is removed, not what it points at). A mount inside leaves its parent in place.
pub fn remove_tree(path: &Path, ctx: Option<&Ctx>) -> std::io::Result<()> {
    let md = fs::symlink_metadata(path)?;
    if !md.is_dir() {
        return fs::remove_file(path);
    }
    remove_dir_on(path, md.dev(), ctx)
}

fn remove_dir_on(dir: &Path, dev: u64, ctx: Option<&Ctx>) -> std::io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let md = fs::symlink_metadata(&path)?;
        if md.is_dir() {
            if md.dev() != dev {
                return Err(std::io::Error::other(format!(
                    "{} is another filesystem; left in place",
                    path.display()
                )));
            }
            remove_dir_on(&path, dev, ctx)?;
        } else {
            fs::remove_file(&path)?;
        }
        if let Some(ctx) = ctx {
            // Pressure pauses apply inside a big removal too; a cancel waits for the item to finish.
            let _ = ctx.throttle_wait();
        }
    }
    fs::remove_dir(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn links_are_not_followed_and_pruned_names_are_skipped() {
        let dir = crate::testutil::TempDir::new("walk");
        let outside = crate::testutil::TempDir::new("walk-outside");
        fs::write(outside.path().join("big"), vec![0u8; 100_000]).unwrap();
        fs::create_dir_all(dir.path().join("a/b/c")).unwrap();
        fs::create_dir_all(dir.path().join(".git/objects")).unwrap();
        fs::write(dir.path().join("a/b/c/f"), b"x").unwrap();
        symlink(outside.path(), dir.path().join("a/link")).unwrap();
        let mounts = HashSet::new();
        let mut seen = Vec::new();
        Walk::new(&mounts, None)
            .prune(&[".git"])
            .run(dir.path(), &mut |p, _, depth| {
                seen.push((p.strip_prefix(dir.path()).unwrap().to_path_buf(), depth));
                Visit::Continue
            })
            .unwrap();
        seen.sort();
        let names: Vec<String> = seen.iter().map(|(p, _)| p.display().to_string()).collect();
        assert_eq!(names, vec!["a", "a/b", "a/b/c", "a/b/c/f", "a/link"]);
        // The link is counted as a link, not as the 100 KB behind it.
        assert!(usage(dir.path(), &mounts, None).unwrap() < 100_000);
        // Removing the tree removes the link, not its target.
        remove_tree(&dir.path().join("a"), None).unwrap();
        assert!(outside.path().join("big").exists());
        assert!(!dir.path().join("a").exists());
    }
}
