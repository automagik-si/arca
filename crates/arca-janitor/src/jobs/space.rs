//! space-report: the size of every folder down to N levels, on one filesystem, links not followed, mount points
//! skipped. Each report is stored as a snapshot, so growth can compare without a fresh scan.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::jobs::{Ctx, JobError, SpaceReportParams, Stop};
use crate::util::{new_id, now};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Snapshot {
    pub id: String,
    pub taken: u64,
    pub root: PathBuf,
    pub depth: u32,
    pub total: u64,
    /// Bytes of every folder down to `depth` levels below the root, by path.
    pub dirs: BTreeMap<PathBuf, u64>,
    /// Folders that could not be read.
    pub unreadable: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DirSize {
    pub path: PathBuf,
    pub bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SpaceReport {
    pub snapshot: String,
    pub root: PathBuf,
    pub total: u64,
    pub unreadable: u64,
    pub biggest: Vec<DirSize>,
}

struct Scan<'a> {
    dev: u64,
    depth: u32,
    mounts: &'a HashSet<PathBuf>,
    ctx: Option<&'a Ctx>,
    dirs: BTreeMap<PathBuf, u64>,
    links: HashSet<(u64, u64)>,
    unreadable: u64,
    seen: u64,
}

impl Scan<'_> {
    fn dir(&mut self, dir: &Path, level: u32) -> Result<u64, Stop> {
        let mut total = fs::symlink_metadata(dir)
            .map(|m| m.blocks() * 512)
            .unwrap_or(0);
        let Ok(entries) = fs::read_dir(dir) else {
            self.unreadable += 1;
            return Ok(total);
        };
        for entry in entries.flatten() {
            self.seen += 1;
            if self.seen.is_multiple_of(256)
                && let Some(ctx) = self.ctx
            {
                ctx.checkpoint()?;
                if self.seen.is_multiple_of(4096) {
                    ctx.with_job(|j| j.progress.items = self.seen);
                }
            }
            let path = entry.path();
            let Ok(md) = fs::symlink_metadata(&path) else {
                continue;
            };
            if md.is_dir() {
                if md.dev() != self.dev || self.mounts.contains(&path) {
                    continue;
                }
                total += self.dir(&path, level + 1)?;
            } else {
                if md.nlink() > 1 && !self.links.insert((md.dev(), md.ino())) {
                    continue;
                }
                total += md.blocks() * 512;
            }
        }
        if level <= self.depth {
            self.dirs.insert(dir.to_path_buf(), total);
        }
        Ok(total)
    }
}

/// Measure a tree and keep the sizes down to `depth` levels.
pub fn scan(root: &Path, depth: u32, ctx: Option<&Ctx>) -> Result<Snapshot, JobError> {
    let md = fs::symlink_metadata(root)
        .map_err(|e| JobError::Failed(format!("{}: {e}", root.display())))?;
    if !md.is_dir() {
        return Err(JobError::Failed(format!(
            "{} is not a folder",
            root.display()
        )));
    }
    let mounts = crate::platform::mount_points();
    let mut s = Scan {
        dev: md.dev(),
        depth,
        mounts: &mounts,
        ctx,
        dirs: BTreeMap::new(),
        links: HashSet::new(),
        unreadable: 0,
        seen: 0,
    };
    let total = s.dir(root, 0)?;
    Ok(Snapshot {
        id: new_id("s"),
        taken: now(),
        root: root.to_path_buf(),
        depth,
        total,
        dirs: s.dirs,
        unreadable: s.unreadable,
    })
}

/// Scan, store the snapshot, and return the biggest folders.
pub fn take(
    root: &Path,
    depth: u32,
    top: usize,
    ctx: &Ctx,
) -> Result<(Snapshot, SpaceReport), JobError> {
    ctx.phase(&format!("measuring {}", root.display()));
    let snap = scan(root, depth, Some(ctx))?;
    ctx.store
        .save_snapshot(&snap.id, &snap)
        .map_err(|e| JobError::Failed(format!("cannot store the snapshot: {e}")))?;
    let mut biggest: Vec<DirSize> = snap
        .dirs
        .iter()
        .filter(|(p, _)| p.as_path() != root)
        .map(|(p, b)| DirSize {
            path: p.clone(),
            bytes: *b,
        })
        .collect();
    biggest.sort_by_key(|d| std::cmp::Reverse(d.bytes));
    biggest.truncate(top);
    let report = SpaceReport {
        snapshot: snap.id.clone(),
        root: snap.root.clone(),
        total: snap.total,
        unreadable: snap.unreadable,
        biggest,
    };
    Ok((snap, report))
}

pub fn run(p: &SpaceReportParams, ctx: &Ctx) -> Result<SpaceReport, JobError> {
    let root = p.path.clone().unwrap_or_else(|| ctx.config.home.clone());
    take(&root, p.depth, p.top, ctx).map(|(_, r)| r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_add_up_to_the_root_and_stop_at_the_depth() {
        let t = crate::testutil::TempDir::new("space");
        fs::create_dir_all(t.path().join("a/b/c")).unwrap();
        fs::write(t.path().join("a/b/c/big"), vec![1u8; 200_000]).unwrap();
        fs::write(t.path().join("small"), vec![1u8; 10]).unwrap();
        let snap = scan(t.path(), 2, None).unwrap();
        let a = snap.dirs[&t.path().join("a")];
        let ab = snap.dirs[&t.path().join("a/b")];
        assert!(a >= 200_000 && ab >= 200_000 && a >= ab);
        assert!(
            !snap.dirs.contains_key(&t.path().join("a/b/c")),
            "deeper than 2"
        );
        assert!(snap.total >= a);
    }
}
