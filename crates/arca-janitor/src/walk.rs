//! Walking a tree the way `find -xdev` does: one filesystem, links never followed, mount points skipped.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, Metadata};
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use rustix::fs::{AtFlags, CWD, Dir, FileType, Mode, OFlags, openat, renameat, unlinkat};
use rustix::io::Errno;

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

// ------------------------------------------------------------------------------------------------ removal

// Removal works on folder handles, not paths. A folder is opened relative to its parent's handle with O_NOFOLLOW,
// and every entry is unlinked relative to the handle of the folder holding it. A folder swapped for a link halfway
// through is refused, never followed (the class of CVE-2022-21658). The path to the target is opened from `/` one
// component at a time the same way, so a link anywhere on the way is refused too.

const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

fn os(e: Errno) -> io::Error {
    io::Error::from_raw_os_error(e.raw_os_error())
}

/// An OS error with the path it happened on.
fn os_at(e: Errno, path: &Path) -> io::Error {
    let e = os(e);
    io::Error::new(e.kind(), format!("{}: {e}", path.display()))
}

fn refused(why: String) -> io::Error {
    io::Error::other(why)
}

/// Open a folder by its absolute path, one component at a time from `/`, never following a link.
pub fn open_dir_nofollow(path: &Path) -> io::Result<OwnedFd> {
    if !path.is_absolute() {
        return Err(refused(format!("{}: not an absolute path", path.display())));
    }
    let mut fd = openat(CWD, "/", DIR_FLAGS, Mode::empty()).map_err(os)?;
    let mut here = PathBuf::from("/");
    for c in path.components() {
        match c {
            Component::RootDir => {}
            Component::Normal(name) => {
                here.push(name);
                fd = openat(&fd, name, DIR_FLAGS, Mode::empty()).map_err(|e| match e {
                    Errno::LOOP | Errno::NOTDIR => refused(format!(
                        "{} is a link or not a folder; refused",
                        here.display()
                    )),
                    e => os(e),
                })?;
            }
            _ => return Err(refused(format!("{}: odd path", path.display()))),
        }
    }
    Ok(fd)
}

/// The device a folder handle is on.
fn dev_of(fd: OwnedFd) -> io::Result<(OwnedFd, u64)> {
    let file = File::from(fd);
    let dev = file.metadata()?.dev();
    Ok((OwnedFd::from(file), dev))
}

/// Why a removal may not enter this folder: it is a mount point, or on another filesystem. One check for both the
/// look-first pass and the removal itself.
fn crossing(path: &Path, dev: u64, root_dev: u64, mounts: &HashSet<PathBuf>) -> Option<String> {
    if mounts.contains(path) {
        return Some(format!("{} is a mount point", path.display()));
    }
    if dev != root_dev {
        return Some(format!("{} is another filesystem", path.display()));
    }
    None
}

/// The entries of an open folder, without `.` and `..`, read before anything in it changes.
fn entries(fd: &OwnedFd) -> io::Result<Vec<(OsString, FileType)>> {
    let mut out = Vec::new();
    for entry in Dir::read_from(fd).map_err(os)? {
        let entry = entry.map_err(os)?;
        let name = entry.file_name().to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        out.push((OsStr::from_bytes(name).to_os_string(), entry.file_type()));
    }
    Ok(out)
}

/// Open a child folder of `dir` without following a link; None when the entry is not a folder (or is a link).
fn open_child_dir(dir: &OwnedFd, name: &OsStr) -> io::Result<Option<OwnedFd>> {
    match openat(dir, name, DIR_FLAGS, Mode::empty()) {
        Ok(fd) => Ok(Some(fd)),
        Err(Errno::NOTDIR | Errno::LOOP) => Ok(None),
        Err(e) => Err(os(e)),
    }
}

/// Look through the whole tree first: any mount point or other filesystem inside refuses the item before anything
/// is deleted.
fn look_first(
    dir: &OwnedFd,
    path: &Path,
    root_dev: u64,
    mounts: &HashSet<PathBuf>,
    ctx: Option<&Ctx>,
) -> io::Result<()> {
    for (name, kind) in entries(dir)? {
        if !matches!(kind, FileType::Directory | FileType::Unknown) {
            continue;
        }
        let child = path.join(&name);
        let Some(fd) = open_child_dir(dir, &name)? else {
            continue;
        };
        let (fd, dev) = dev_of(fd)?;
        if let Some(why) = crossing(&child, dev, root_dev, mounts) {
            return Err(refused(format!("{why}; nothing removed")));
        }
        look_first(&fd, &child, root_dev, mounts, ctx)?;
        if let Some(ctx) = ctx {
            // The look is read-only, but a big tree still pauses under pressure.
            let _ = ctx.throttle_wait();
        }
    }
    Ok(())
}

/// Delete everything inside an open folder, relative to its handle.
fn empty_fd(
    dir: &OwnedFd,
    path: &Path,
    root_dev: u64,
    mounts: &HashSet<PathBuf>,
    ctx: Option<&Ctx>,
) -> io::Result<()> {
    for (name, kind) in entries(dir)? {
        let child = path.join(&name);
        let sub = if matches!(kind, FileType::Directory | FileType::Unknown) {
            open_child_dir(dir, &name)?
        } else {
            None
        };
        match sub {
            Some(fd) => {
                let (fd, dev) = dev_of(fd)?;
                if let Some(why) = crossing(&child, dev, root_dev, mounts) {
                    return Err(refused(format!("{why}; left in place")));
                }
                empty_fd(&fd, &child, root_dev, mounts, ctx)?;
                unlinkat(dir, &name, AtFlags::REMOVEDIR).map_err(|e| os_at(e, &child))?;
            }
            // A file or a link: the entry itself goes, never what a link points at.
            None => unlinkat(dir, &name, AtFlags::empty()).map_err(|e| os_at(e, &child))?,
        }
        if let Some(ctx) = ctx {
            // Pressure pauses apply inside a big removal too; a cancel waits for the item to finish.
            let _ = ctx.throttle_wait();
        }
    }
    Ok(())
}

/// Remove a file, a link, or a folder and everything in it. Nothing is followed: a link is removed, not what it
/// points at, and a link on the way to `path` refuses the removal. A mount point or another filesystem anywhere
/// inside refuses the whole item before anything is deleted; `mounts` is the mount table to respect, and a
/// filesystem it does not list is still caught by its device number.
pub fn remove_tree(path: &Path, mounts: &HashSet<PathBuf>, ctx: Option<&Ctx>) -> io::Result<()> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(refused(format!("{}: no parent folder", path.display())));
    };
    let (parent_fd, parent_dev) = dev_of(open_dir_nofollow(parent)?)?;
    let Some(fd) = open_child_dir(&parent_fd, name)? else {
        return unlinkat(&parent_fd, name, AtFlags::empty()).map_err(os);
    };
    let (fd, dev) = dev_of(fd)?;
    if let Some(why) = crossing(path, dev, parent_dev, mounts) {
        return Err(refused(format!("{why}; nothing removed")));
    }
    look_first(&fd, path, dev, mounts, ctx)?;
    empty_fd(&fd, path, dev, mounts, ctx)?;
    unlinkat(&parent_fd, name, AtFlags::REMOVEDIR).map_err(os)
}

/// Delete everything inside a folder and keep the folder, with the same rules as `remove_tree`.
pub fn empty_dir(path: &Path, mounts: &HashSet<PathBuf>, ctx: Option<&Ctx>) -> io::Result<()> {
    let (fd, dev) = dev_of(open_dir_nofollow(path)?)?;
    if mounts.contains(path) {
        return Err(refused(format!(
            "{} is a mount point; nothing removed",
            path.display()
        )));
    }
    look_first(&fd, path, dev, mounts, ctx)?;
    empty_fd(&fd, path, dev, mounts, ctx)
}

/// Move a folder into `dest_dir` as `dest_name`, renaming relative to both parents' handles, which are opened
/// without following a link.
pub fn move_nofollow(path: &Path, dest_dir: &Path, dest_name: &OsStr) -> io::Result<()> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(refused(format!("{}: no parent folder", path.display())));
    };
    let from = open_dir_nofollow(parent)?;
    let to = open_dir_nofollow(dest_dir)?;
    renameat(&from, name, &to, dest_name).map_err(os)
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
        remove_tree(&dir.path().join("a"), &mounts, None).unwrap();
        assert!(outside.path().join("big").exists());
        assert!(!dir.path().join("a").exists());
    }
}
