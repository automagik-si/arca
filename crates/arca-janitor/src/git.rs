//! Whether a folder holds git work that exists nowhere else: changes not committed, files not tracked, a stash, or
//! commits no remote has. A folder like that is never moved to the trash.
//!
//! Every repository, worktree and bare repository at or anywhere inside the folder is checked, including ones nested
//! in another repository and ignored by it (agents keep worktrees in `<repo>/.claude/worktrees/`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::util::{on_path, run_cmd};
use crate::walk::{Visit, Walk};

const TIMEOUT: Duration = Duration::from_secs(60);

/// What kind of repository a folder is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A folder with a work tree: it holds `.git`, as a folder (a repository) or a file (a worktree or submodule).
    WorkTree,
    /// A bare repository: `HEAD`, `objects/` and `refs/`, no work tree.
    Bare,
}

/// The kind of repository `dir` is, if it is one.
pub fn kind_of(dir: &Path) -> Option<Kind> {
    if dir.join(".git").symlink_metadata().is_ok() {
        return Some(Kind::WorkTree);
    }
    let is = |name: &str, dir_wanted: bool| {
        dir.join(name)
            .symlink_metadata()
            .is_ok_and(|md| md.is_dir() == dir_wanted && !md.is_symlink())
    };
    (is("HEAD", false) && is("objects", true) && is("refs", true)).then_some(Kind::Bare)
}

/// The repositories, worktrees and bare repositories at or anywhere inside `path`, on one filesystem, links not
/// followed. The walk goes on inside a repository's work tree, so nested worktrees and repositories are found too;
/// only `.git` folders and the insides of bare repositories are not searched.
pub fn repos_in(path: &Path, mounts: &HashSet<PathBuf>) -> Vec<(PathBuf, Kind)> {
    let mut found = Vec::new();
    match kind_of(path) {
        Some(Kind::Bare) => return vec![(path.to_path_buf(), Kind::Bare)],
        Some(kind) => found.push((path.to_path_buf(), kind)),
        None => {}
    }
    let _ = Walk::new(mounts, None)
        .prune(&[".git"])
        .run(path, &mut |p, md, _| {
            if !md.is_dir() {
                return Visit::Continue;
            }
            match kind_of(p) {
                Some(Kind::Bare) => {
                    found.push((p.to_path_buf(), Kind::Bare));
                    Visit::Prune
                }
                Some(kind) => {
                    found.push((p.to_path_buf(), kind));
                    Visit::Continue
                }
                None => Visit::Continue,
            }
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
            repos[0].0.display()
        ));
    }
    let within = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    repos
        .iter()
        .find_map(|(repo, kind)| repo_work(repo, *kind, &within))
}

/// What one repository, worktree or bare repository holds that would be lost with the folder `within`.
fn repo_work(repo: &Path, kind: Kind, within: &Path) -> Option<String> {
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

    if kind == Kind::WorkTree {
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
    }

    // Where the objects and refs live. When they go with the folder, every ref and every worktree's HEAD counts
    // (`--all`); a worktree of a repository kept elsewhere loses only what its own HEAD holds.
    let Some(common) = git(&["rev-parse", "--git-common-dir"]) else {
        return unreadable();
    };
    let common = Path::new(common.trim());
    let Ok(common) = repo.join(common).canonicalize() else {
        return unreadable();
    };
    let goes = common.starts_with(within);

    if goes && kind == Kind::WorkTree {
        let Some(stash) = git(&["stash", "list"]) else {
            return unreadable();
        };
        if !stash.trim().is_empty() {
            return Some(format!("a git stash in {}", repo.display()));
        }
    }

    let mut rev = vec!["rev-list", "-n", "1", if goes { "--all" } else { "HEAD" }];
    rev.extend(["--not", "--remotes"]);
    match git(&rev) {
        Some(out) if out.trim().is_empty() => {}
        Some(_) => return Some(format!("git commits not pushed in {}", repo.display())),
        None => return unreadable(),
    }

    // A worktree kept outside the folder would lose its repository.
    if goes {
        let Some(list) = git(&["worktree", "list", "--porcelain"]) else {
            return unreadable();
        };
        for line in list.lines() {
            let Some(wt) = line.strip_prefix("worktree ") else {
                continue;
            };
            if let Ok(wt) = Path::new(wt).canonicalize()
                && !wt.starts_with(within)
            {
                return Some(format!(
                    "a worktree of {} is outside it, at {}",
                    repo.display(),
                    wt.display()
                ));
            }
        }
    }
    None
}
