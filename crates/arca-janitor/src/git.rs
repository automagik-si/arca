//! Whether a folder holds git work that exists nowhere else: changes not committed, files not tracked, a stash, or
//! commits no remote has. A folder like that is never moved to the trash.
//!
//! Every repository, worktree and bare repository at or anywhere inside the folder is checked, including ones nested
//! in another repository and ignored by it (agents keep worktrees in `<repo>/.claude/worktrees/`), and worktrees kept
//! inside a bare repository's folder.
//!
//! The check only reads, and runs no command a scanned repository's config names: a repository whose own config sets
//! a filter command is kept unchecked, since `git status` would run that command; submodules' work trees are left to
//! their own check, never entered by an outer `status`; and a stash is found by its ref, not by `git log`, which could
//! run `gpg.program`; no git call fetches a missing object from a partial clone's promisor remote, which would run
//! the transport command the config names.
//!
//! The check honours the job: its walk and every git command wait for a pause under pressure and stop on cancel or
//! at the budget.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::jobs::{Ctx, Stop};
use crate::util::{CmdOutput, on_path, run_cmd_env};
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
pub fn repos_in(
    path: &Path,
    mounts: &HashSet<PathBuf>,
    ctx: Option<&Ctx>,
) -> Result<Vec<(PathBuf, Kind)>, Stop> {
    let mut found = Vec::new();
    let mut bare: HashSet<PathBuf> = HashSet::new();
    if let Some(kind) = kind_of(path) {
        found.push((path.to_path_buf(), kind));
        if kind == Kind::Bare {
            bare.insert(path.to_path_buf());
        }
    }
    Walk::new(mounts, ctx)
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
        })?;
    Ok(found)
}

/// Why `path` may not be moved away for its git work, if it holds any. A repository whose state cannot be read
/// counts as holding work. `Err` when the job stops (cancel or budget) before the check is done.
pub fn unsaved_work(
    path: &Path,
    mounts: &HashSet<PathBuf>,
    ctx: Option<&Ctx>,
) -> Result<Option<String>, Stop> {
    let repos = repos_in(path, mounts, ctx)?;
    if repos.is_empty() {
        return Ok(None);
    }
    if !on_path("git") {
        return Ok(Some(format!(
            "holds a git repository ({}) and git is not installed to check it",
            repos[0].0.display()
        )));
    }
    let check = Check {
        ctx,
        within: path.canonicalize().unwrap_or_else(|_| path.to_path_buf()),
        // Work trees the walk found are checked on their own; a repository's worktree list adds only the others.
        walked: repos
            .iter()
            .filter(|(_, kind)| *kind == Kind::WorkTree)
            .filter_map(|(r, _)| r.canonicalize().ok())
            .collect(),
    };
    for (repo, kind) in &repos {
        if let Some(why) = check.repo_work(repo, *kind)? {
            return Ok(Some(why));
        }
    }
    Ok(None)
}

fn unreadable(dir: &Path) -> Option<String> {
    Some(format!("git state of {} cannot be read", dir.display()))
}

/// The check of one folder.
struct Check<'a> {
    /// The job, for its pause, cancel and budget.
    ctx: Option<&'a Ctx>,
    /// The folder, in its real form.
    within: PathBuf,
    /// The work trees the walk found.
    walked: HashSet<PathBuf>,
}

impl Check<'_> {
    /// Run git in `dir`, once the job may go on. No optional locks (status would write the index otherwise), no
    /// fsmonitor hook from the repository's config, and no lazy fetch: in a partial clone a missing object would make
    /// git fetch it from the promisor remote, which runs the transport command the repository's config names
    /// (`core.sshCommand`, `remote.<name>.uploadpack`, an `ext::` URL). Git reads the variable since 2.45 and an older
    /// git ignores it; the `--no-lazy-fetch` option would instead make an older git fail every call.
    fn git(&self, dir: &Path, args: &[&str]) -> Result<Option<CmdOutput>, Stop> {
        if let Some(ctx) = self.ctx {
            ctx.checkpoint()?;
        }
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
        Ok(run_cmd_env(&argv, &[("GIT_NO_LAZY_FETCH", "1")], TIMEOUT))
    }

    /// Git's output in `dir`, when it succeeded.
    fn git_out(&self, dir: &Path, args: &[&str]) -> Result<Option<String>, Stop> {
        Ok(self.git(dir, args)?.filter(|o| o.success).map(|o| o.stdout))
    }

    /// Whether the repository's own config sets a filter driver (`filter.<name>.clean`, `.smudge`, `.process`), which
    /// `git status` could run: in its config or a file that config includes, or in the worktree's own config.
    /// `git config` runs nothing.
    fn own_filter(&self, dir: &Path) -> Result<Option<String>, Stop> {
        for scope in ["--local", "--worktree"] {
            if scope == "--worktree" {
                let per_worktree = self.git_out(
                    dir,
                    &[
                        "config",
                        "--local",
                        "--type=bool",
                        "--get",
                        "extensions.worktreeConfig",
                    ],
                )?;
                if !per_worktree.is_some_and(|v| v.trim() == "true") {
                    break;
                }
            }
            match self.git(
                dir,
                &["config", scope, "--includes", "--get-regexp", r"^filter\."],
            )? {
                Some(o) if o.success => {
                    let key = o.stdout.split_whitespace().next().unwrap_or("filter");
                    return Ok(Some(format!(
                        "git config of {} sets a filter command ({key}); not run to check it",
                        dir.display()
                    )));
                }
                // Exit 1: nothing matches.
                Some(o) if o.status == Some(1) => {}
                _ => return Ok(unreadable(dir)),
            }
        }
        Ok(None)
    }

    /// Changes not committed or files not tracked in the work tree at `dir`. A submodule's own work tree is not
    /// entered (`--ignore-submodules=dirty`, so its config's commands never run); the walk finds it and checks it on
    /// its own. A submodule moved to another commit still shows.
    fn work_tree_changes(&self, dir: &Path) -> Result<Option<String>, Stop> {
        if let Some(why) = self.own_filter(dir)? {
            return Ok(Some(why));
        }
        let Some(status) = self.git_out(
            dir,
            &[
                "status",
                "--porcelain",
                "--untracked-files=normal",
                "--ignore-submodules=dirty",
            ],
        )?
        else {
            return Ok(unreadable(dir));
        };
        if !status.trim().is_empty() {
            return Ok(Some(format!(
                "git work not committed in {} ({} paths)",
                dir.display(),
                status.lines().count()
            )));
        }
        Ok(None)
    }

    /// What one repository, worktree or bare repository holds that would be lost with the folder.
    fn repo_work(&self, repo: &Path, kind: Kind) -> Result<Option<String>, Stop> {
        if kind == Kind::WorkTree
            && let Some(why) = self.work_tree_changes(repo)?
        {
            return Ok(Some(why));
        }

        // Where the objects and refs live. When they go with the folder, every ref and every worktree's HEAD counts
        // (`--all`); a worktree of a repository kept elsewhere loses only what its own HEAD holds.
        let Some(common) = self.git_out(repo, &["rev-parse", "--git-common-dir"])? else {
            return Ok(unreadable(repo));
        };
        let Ok(common) = repo.join(common.trim()).canonicalize() else {
            return Ok(unreadable(repo));
        };
        let goes = common.starts_with(&self.within);

        // The stash by its ref: `git stash list` runs `git log`, which can run the repository's `gpg.program`.
        if goes && kind == Kind::WorkTree {
            match self.git(repo, &["rev-parse", "-q", "--verify", "refs/stash"])? {
                Some(o) if o.success => {
                    return Ok(Some(format!("a git stash in {}", repo.display())));
                }
                // Exit 1: no stash.
                Some(o) if o.status == Some(1) => {}
                _ => return Ok(unreadable(repo)),
            }
        }

        let mut rev = vec!["rev-list", "-n", "1", if goes { "--all" } else { "HEAD" }];
        rev.extend(["--not", "--remotes"]);
        match self.git_out(repo, &rev)? {
            Some(out) if out.trim().is_empty() => {}
            Some(_) => {
                return Ok(Some(format!(
                    "git commits not pushed in {}",
                    repo.display()
                )));
            }
            None => return Ok(unreadable(repo)),
        }

        // Every worktree of a repository that goes with the folder: one kept outside would lose its repository, and
        // one kept inside (in a bare repository's folder, say) is checked for changes like any work tree.
        if goes {
            let Some(list) = self.git_out(repo, &["worktree", "list", "--porcelain"])? else {
                return Ok(unreadable(repo));
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
                if !wt.starts_with(&self.within) {
                    return Ok(Some(format!(
                        "a worktree of {} is outside it, at {}",
                        repo.display(),
                        wt.display()
                    )));
                }
                if !self.walked.contains(&wt)
                    && let Some(why) = self.work_tree_changes(&wt)?
                {
                    return Ok(Some(why));
                }
            }
        }
        Ok(None)
    }
}
