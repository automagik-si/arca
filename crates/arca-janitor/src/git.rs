//! Whether a folder holds git work that exists nowhere else: changes not committed, files not tracked, a stash, or
//! commits no remote has. A folder like that is never moved to the trash.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::util::{on_path, run_cmd};
use crate::walk::{Visit, Walk};

/// How deep inside a candidate to look for repositories and worktrees.
const DEPTH: usize = 4;
const TIMEOUT: Duration = Duration::from_secs(60);

/// The repositories and worktrees at or inside `path` (a folder holding `.git`, as a folder or a file), down to a
/// few levels, on one filesystem, links not followed. A repository's own insides are not searched.
pub fn repos_in(path: &Path, mounts: &HashSet<PathBuf>) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if path.join(".git").symlink_metadata().is_ok() {
        found.push(path.to_path_buf());
        return found;
    }
    let _ = Walk::new(mounts, None)
        .depth(DEPTH)
        .run(path, &mut |p, md, _| {
            if md.is_dir() && p.join(".git").symlink_metadata().is_ok() {
                found.push(p.to_path_buf());
                return Visit::Prune;
            }
            Visit::Continue
        });
    found
}

/// Why `path` may not be moved away for its git work, if it holds any. A repository whose state cannot be read
/// counts as holding work.
pub fn unsaved_work(path: &Path, mounts: &HashSet<PathBuf>) -> Option<String> {
    let repos = repos_in(path, mounts);
    if repos.is_empty() {
        return None;
    }
    if !on_path("git") {
        return Some(format!(
            "holds a git repository ({}) and git is not installed to check it",
            repos[0].display()
        ));
    }
    repos.iter().find_map(|r| repo_work(r))
}

/// What one repository or worktree holds that exists nowhere else.
fn repo_work(repo: &Path) -> Option<String> {
    let dir = repo.to_string_lossy();
    // No optional locks (status would write the index otherwise), and no fsmonitor hook from the repo's config.
    let git = |args: &[&str]| {
        let mut argv = vec![
            "git",
            "--no-optional-locks",
            "-c",
            "core.fsmonitor=false",
            "-C",
            &dir,
        ];
        argv.extend_from_slice(args);
        run_cmd(&argv, TIMEOUT)
            .filter(|o| o.success)
            .map(|o| o.stdout)
    };
    let unreadable = || Some(format!("git state of {} cannot be read", repo.display()));
    let Some(status) = git(&["status", "--porcelain", "--untracked-files=normal"]) else {
        return unreadable();
    };
    if !status.trim().is_empty() {
        return Some(format!(
            "git work not committed in {} ({} paths)",
            repo.display(),
            status.lines().count()
        ));
    }
    let Some(stash) = git(&["stash", "list"]) else {
        return unreadable();
    };
    if !stash.trim().is_empty() {
        return Some(format!("a git stash in {}", repo.display()));
    }
    // A repository's own folder holds all its branches; a worktree's branches live in its repository.
    let own = repo.join(".git").is_dir();
    let mut rev = vec!["rev-list", "-n", "1", "HEAD"];
    if own {
        rev.push("--branches");
    }
    rev.extend(["--not", "--remotes"]);
    match git(&rev) {
        Some(out) if out.trim().is_empty() => None,
        Some(_) => Some(format!("git commits not pushed in {}", repo.display())),
        None => unreadable(),
    }
}
