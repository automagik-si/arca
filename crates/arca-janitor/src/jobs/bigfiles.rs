//! big-files: files over N MB, optionally only those changed in the last M hours. One filesystem, no links.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::jobs::{BigFilesParams, Ctx, JobError};
use crate::util::now;
use crate::walk::{Visit, Walk, mtime};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BigFile {
    pub path: PathBuf,
    pub bytes: u64,
    pub modified: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BigFiles {
    pub root: PathBuf,
    pub min_mb: u64,
    pub changed_hours: Option<u64>,
    pub count: usize,
    pub total: u64,
    pub files: Vec<BigFile>,
}

pub fn run(p: &BigFilesParams, ctx: &Ctx) -> Result<BigFiles, JobError> {
    let root = p.path.clone().unwrap_or_else(|| ctx.config.home.clone());
    ctx.phase(&format!(
        "looking for files over {} MB in {}",
        p.min_mb,
        root.display()
    ));
    let min = p.min_mb * 1_048_576;
    let since = p.changed_hours.map(|h| now().saturating_sub(h * 3600));
    let mounts = crate::platform::mount_points();
    let mut files = Vec::new();
    Walk::new(&mounts, Some(ctx)).run(&root, &mut |path, md, _| {
        if md.is_file() && md.len() >= min && since.is_none_or(|s| mtime(md) >= s) {
            files.push(BigFile {
                path: path.to_path_buf(),
                bytes: md.len(),
                modified: mtime(md),
            });
        }
        Visit::Continue
    })?;
    files.sort_by_key(|f| std::cmp::Reverse(f.bytes));
    let count = files.len();
    let total = files.iter().map(|f| f.bytes).sum();
    files.truncate(p.limit);
    Ok(BigFiles {
        root,
        min_mb: p.min_mb,
        changed_hours: p.changed_hours,
        count,
        total,
        files,
    })
}
