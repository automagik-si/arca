//! Whether a folder holds git work that exists nowhere else: changes not committed, files not tracked, a stash, or
//! commits no remote has. A folder like that is never moved to the trash.
//!
//! Every repository, worktree and bare repository at or anywhere inside the folder is checked, including ones nested
//! in another repository and ignored by it (agents keep worktrees in `<repo>/.claude/worktrees/`), and worktrees kept
//! inside a bare repository's folder.
//!
//! The check only reads. A repository whose own config sets a filter command is kept unchecked, since `git status`
//! would run that command.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::util::{CmdOutput, on_path, run_cmd};
use crate::walk::{Visit, Walk};

const TIMEOUT: Duration = Duration::from_secs(60);
/// A bare repository's own folders, which hold no work. Worktrees kept in a bare repository's folder sit beside them.
const BARE_INSIDES: &[&str] = &[
    "objects",
    "refs",
    "logs",
    "hooks",
    "info",
    "worktrees",
    "modules",
    "branches",
    "rr-cache",
    "lfs",
];

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
/// followed. The walk goes on inside every repository, so nested worktrees and repositories are found too; only
/// `.git` folders and a bare repository's own folders are not searched.
pub fn repos_in(path: &Path, mounts: &HashSet<PathBuf>) -> Vec<(PathBuf, Kind)> {
    let mut found = Vec::new();
    let mut bare: HashSet<PathBuf> = HashSet::new();
    if let Some(kind) = kind_of(path) {
        found.push((path.to_path_buf(), kind));
        if kind == Kind::Bare {
            bare.insert(path.to_path_buf());
        }
    }
    let _ = Walk::new(mounts, None)
        .prune(&[".git"])
        .run(path, &mut |p, md, _| {
            if !md.is_dir() {
                return Visit::Continue;
            }
            let in_bare = p.parent().is_some_and(|d| bare.contains(d));
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if in_bare && BARE_INSIDES.contains(&name) {
                return Visit::Prune;
            }
            if let Some(kind) = kind_of(p) {
                found.push((p.to_path_buf(), kind));
                if kind == Kind::Bare {
                    bare.insert(p.to_path_buf());
                }
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
            repos[0].0.display()
        ));
    }
    let within = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    // Work trees the walk found are checked on their own; a repository's worktree list adds only the others.
    let walked: HashSet<PathBuf> = repos
        .iter()
        .filter(|(_, kind)| *kind == Kind::WorkTree)
        .filter_map(|(r, _)| r.canonicalize().ok())
        .collect();
    repos
        .iter()
        .find_map(|(repo, kind)| repo_work(repo, *kind, &within, &walked))
}

/// Run git in `dir`. No optional locks (status would write the index otherwise), and no fsmonitor hook from the
/// repository's config.
fn git(dir: &Path, args: &[&str]) -> Option<CmdOutput> {
    let dir = dir.to_string_lossy();
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
}

/// Git's output in `dir`, when it succeeded.
fn git_out(dir: &Path, args: &[&str]) -> Option<String> {
    git(dir, args).filter(|o| o.success).map(|o| o.stdout)
}

fn unreadable(dir: &Path) -> Option<String> {
    Some(format!("git state of {} cannot be read", dir.display()))
}

/// The first filter driver the repository's own config sets (`filter.<name>.clean`, `.smudge`, `.process`), which
/// `git status` could run. `Err` when the config cannot be read.
fn own_filter(dir: &Path) -> Result<Option<String>, ()> {
    let first = |scope: &str| -> Result<Option<String>, ()> {
        match git(
            dir,
            &["config", scope, "--includes", "--get-regexp", r"^filter\."],
        ) {
            Some(o) if o.success => Ok(o.stdout.split_whitespace().next().map(str::to_string)),
            // Exit 1: nothing matches.
            Some(o) if o.status == Some(1) => Ok(None),
            _ => Err(()),
        }
    };
    if let Some(key) = first("--local")? {
        return Ok(Some(key));
    }
    let per_worktree = git_out(
        dir,
        &[
            "config",
            "--local",
            "--type=bool",
            "--get",
            "extensions.worktreeConfig",
        ],
    );
    if per_worktree.is_some_and(|v| v.trim() == "true") {
        return first("--worktree");
    }
    Ok(None)
}

/// Changes not committed or files not tracked in the work tree at `dir`.
fn work_tree_changes(dir: &Path) -> Option<String> {
    match own_filter(dir) {
        Ok(None) => {}
        Ok(Some(key)) => {
            return Some(format!(
                "git config of {} sets a filter command ({key}); not run to check it",
                dir.display()
            ));
        }
        Err(()) => return unreadable(dir),
    }
    let Some(status) = git_out(dir, &["status", "--porcelain", "--untracked-files=normal"]) else {
        return unreadable(dir);
    };
    if !status.trim().is_empty() {
        return Some(format!(
            "git work not committed in {} ({} paths)",
            dir.display(),
            status.lines().count()
        ));
    }
    None
}

/// What one repository, worktree or bare repository holds that would be lost with the folder `within`. `walked` are
/// the work trees the walk found, which are checked on their own.
fn repo_work(repo: &Path, kind: Kind, within: &Path, walked: &HashSet<PathBuf>) -> Option<String> {
    if kind == Kind::WorkTree
        && let Some(why) = work_tree_changes(repo)
    {
        return Some(why);
    }

    // Where the objects and refs live. When they go with the folder, every ref and every worktree's HEAD counts
    // (`--all`); a worktree of a repository kept elsewhere loses only what its own HEAD holds.
    let Some(common) = git_out(repo, &["rev-parse", "--git-common-dir"]) else {
        return unreadable(repo);
    };
    let Ok(common) = repo.join(common.trim()).canonicalize() else {
        return unreadable(repo);
    };
    let goes = common.starts_with(within);

    if goes && kind == Kind::WorkTree {
        let Some(stash) = git_out(repo, &["stash", "list"]) else {
            return unreadable(repo);
        };
        if !stash.trim().is_empty() {
            return Some(format!("a git stash in {}", repo.display()));
        }
    }

    let mut rev = vec!["rev-list", "-n", "1", if goes { "--all" } else { "HEAD" }];
    rev.extend(["--not", "--remotes"]);
    match git_out(repo, &rev) {
        Some(out) if out.trim().is_empty() => {}
        Some(_) => return Some(format!("git commits not pushed in {}", repo.display())),
        None => return unreadable(repo),
    }

    // Every worktree of a repository that goes with the folder: one kept outside would lose its repository, and
    // one kept inside (in a bare repository's folder, say) is checked for changes like any work tree.
    if goes {
        let Some(list) = git_out(repo, &["worktree", "list", "--porcelain"]) else {
            return unreadable(repo);
        };
        for entry in list.split("\n\n") {
            let mut lines = entry.lines();
            let Some(wt) = lines.next().and_then(|l| l.strip_prefix("worktree ")) else {
                continue;
            };
            if lines.any(|l| l == "bare") {
                continue;
            }
            // A worktree whose folder is gone holds nothing.
            let Ok(wt) = Path::new(wt).canonicalize() else {
                continue;
            };
            if !wt.starts_with(within) {
                return Some(format!(
                    "a worktree of {} is outside it, at {}",
                    repo.display(),
                    wt.display()
                ));
            }
            if !walked.contains(&wt)
                && let Some(why) = work_tree_changes(&wt)
            {
                return Some(why);
            }
        }
    }
    None
}
