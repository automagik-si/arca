//! Deletion safety: what an apply must never touch even when the world changes under it.
//!
//! - A root reached through a linked folder still sees live processes, which /proc reports by real path.
//! - Right before each item, live processes and the disk are looked at again: a process that starts during an
//!   apply keeps its folder, and an item that vanished or was swapped for a link is skipped, not failed.
//! - A removal never follows a link on the way to its target.
//! - Stale scratch holding git work that exists nowhere else is never moved to the trash, including work in
//!   repositories and worktrees nested inside a clean one, in bare repositories and the worktrees kept in their
//!   folders. A repository whose own config sets a filter command is kept, and the command is never run.
//!
//! The mount cases are in `mounts.rs`, which reports them as ignored where user namespaces are not available.

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

use arca_janitor::config::Config;
use arca_janitor::jobs::clean::{apply_plan_with, make_plan};
use arca_janitor::jobs::{CleanPlanParams, Profile};
use arca_janitor::testutil::{TempDir, age_tree};
use arca_janitor::walk::remove_tree;

fn config(home: &Path) -> Config {
    let mut c = Config::defaults(home, "testhost");
    c.project_roots = vec![home.join("work")];
    c.scratch_roots = vec![home.join("scratch")];
    c
}

fn plan_only(c: &Config, category: &str) -> arca_janitor::jobs::clean::Plan {
    let params = CleanPlanParams {
        profile: Profile::Full,
        only: vec![category.to_string()],
    };
    make_plan(c, None, &params).unwrap()
}

fn rust_project(p: &Path) {
    fs::create_dir_all(p.join("src")).unwrap();
    fs::create_dir_all(p.join("target/debug")).unwrap();
    fs::write(p.join("Cargo.toml"), "").unwrap();
    fs::write(p.join("target/debug/blob"), vec![1u8; 40_000]).unwrap();
}

/// A `sleep` with its working folder in `dir`, ended when dropped.
struct Sleeper(Child);
impl Sleeper {
    fn in_dir(dir: &Path) -> Sleeper {
        let child = Command::new("sleep")
            .arg("60")
            .current_dir(dir)
            .spawn()
            .unwrap();
        // Let /proc show it.
        std::thread::sleep(Duration::from_millis(100));
        Sleeper(child)
    }
}
impl Drop for Sleeper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// ------------------------------------------------------------------------------------------------ linked roots

#[test]
fn a_live_process_protects_its_project_when_the_root_is_reached_through_a_link() {
    let t = TempDir::new("linked-root");
    let real = t.path().join("real");
    let p = real.join("home/work/p");
    rust_project(&p);
    fs::create_dir_all(real.join("home/scratch")).unwrap();
    age_tree(&real, 30);
    let alias = t.path().join("alias");
    symlink(&real, &alias).unwrap();
    // The config names the roots through the link; /proc names the process's folder by its real path.
    let c = config(&alias.join("home"));

    let sleeper = Sleeper::in_dir(&p.join("src"));
    let plan = plan_only(&c, "build-outputs");
    assert!(
        plan.items.is_empty(),
        "planned under a live process: {:?}",
        plan.items
    );
    assert!(
        plan.skipped
            .iter()
            .any(|s| s.label.ends_with("p/target") && s.reason.contains("live process")),
        "{:?}",
        plan.skipped
    );

    // Control: with the process gone, the same project is planned.
    drop(sleeper);
    let plan = plan_only(&c, "build-outputs");
    assert_eq!(plan.items.len(), 1, "{:?}", plan.skipped);
}

// ------------------------------------------------------------------------------------------------ links on the way

#[test]
fn a_removal_never_follows_a_link_on_the_way_to_its_target() {
    let t = TempDir::new("link-on-way");
    let real = t.path().join("real");
    fs::create_dir_all(real.join("x/inner")).unwrap();
    fs::write(real.join("x/inner/precious"), b"keep").unwrap();
    let alias = t.path().join("alias");
    symlink(&real, &alias).unwrap();
    let e = remove_tree(&alias.join("x"), &HashSet::new(), None).expect_err("refused");
    assert!(e.to_string().contains("is a link"), "{e}");
    assert!(real.join("x/inner/precious").exists());
    // Control: by its real path, the same folder goes.
    remove_tree(&real.join("x"), &HashSet::new(), None).unwrap();
    assert!(!real.join("x").exists());
}

// ------------------------------------------------------------------------------------------------ during an apply

fn idle_project(t: &TempDir) -> (Config, PathBuf) {
    let home = t.path().join("home");
    let p = home.join("work/p");
    rust_project(&p);
    fs::create_dir_all(home.join("scratch")).unwrap();
    age_tree(&home, 30);
    (config(&home), p)
}

#[test]
fn a_process_that_starts_during_the_apply_keeps_its_folder() {
    let t = TempDir::new("starts-mid-apply");
    let (c, p) = idle_project(&t);
    let plan = plan_only(&c, "build-outputs");
    assert_eq!(plan.items.len(), 1, "{:?}", plan.skipped);
    let mut sleeper = None;
    let r = apply_plan_with(&plan, &c, None, &mut || {
        sleeper = Some(Sleeper::in_dir(&p.join("src")));
    })
    .unwrap();
    assert!(
        r.done.is_empty(),
        "removed under a live process: {:?}",
        r.done
    );
    assert!(
        r.skipped.iter().any(|s| s.reason.contains("live process")),
        "{:?}",
        r.skipped
    );
    assert!(p.join("target/debug/blob").exists());
    drop(sleeper);

    // Control: with nothing started in between, the same plan applies.
    let r = apply_plan_with(&plan, &c, None, &mut || {}).unwrap();
    assert_eq!(r.done.len(), 1, "{:?} {:?}", r.skipped, r.failed);
}

#[test]
fn an_item_that_vanished_or_became_a_link_is_skipped_and_nothing_behind_the_link_is_touched() {
    let t = TempDir::new("swapped");
    let home = t.path().join("home");
    for name in ["gone", "swapped", "kept"] {
        rust_project(&home.join("work").join(name));
    }
    fs::create_dir_all(home.join("scratch")).unwrap();
    let outside = t.path().join("outside");
    fs::create_dir_all(outside.join("debug")).unwrap();
    fs::write(outside.join("debug/precious"), b"keep").unwrap();
    age_tree(&home, 30);
    let c = config(&home);
    let plan = plan_only(&c, "build-outputs");
    assert_eq!(plan.items.len(), 3, "{:?}", plan.skipped);
    let work = home.join("work");
    let r = apply_plan_with(&plan, &c, None, &mut || {
        fs::remove_dir_all(work.join("gone/target")).unwrap();
        fs::remove_dir_all(work.join("swapped/target")).unwrap();
        symlink(&outside, work.join("swapped/target")).unwrap();
    })
    .unwrap();
    assert!(r.failed.is_empty(), "{:?}", r.failed);
    let reason = |p: &str| {
        r.skipped
            .iter()
            .find(|s| s.label.ends_with(p))
            .map(|s| s.reason.clone())
    };
    assert_eq!(reason("gone/target").as_deref(), Some("gone"));
    assert_eq!(reason("swapped/target").as_deref(), Some("refused: a link"));
    assert!(outside.join("debug/precious").exists());
    assert_eq!(r.done.len(), 1);
    assert!(!work.join("kept/target").exists());
}

// ------------------------------------------------------------------------------------------------ git work

fn git(dir: &Path, args: &[&str]) {
    git_with(dir, args, None);
}

/// Run git in `dir`, with `input` on its stdin if given, as a test author with no global config; its stdout.
fn git_with(dir: &Path, args: &[&str], input: Option<&str>) -> String {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "acme")
        .env("GIT_AUTHOR_EMAIL", "dev@example.com")
        .env("GIT_COMMITTER_NAME", "acme")
        .env("GIT_COMMITTER_EMAIL", "dev@example.com")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    if let Some(input) = input {
        stdin.write_all(input.as_bytes()).unwrap();
    }
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn repo(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    fs::write(dir.join("README"), "x").unwrap();
    git(dir, &["add", "README"]);
    git(dir, &["commit", "-q", "-m", "first"]);
}

#[test]
fn stale_scratch_with_git_work_found_nowhere_else_is_never_moved() {
    if !arca_janitor::util::on_path("git") {
        eprintln!("skipped: git is not installed");
        return;
    }
    let t = TempDir::new("git-scratch");
    let home = t.path().join("home");
    let scratch = home.join("scratch");
    fs::create_dir_all(home.join("work")).unwrap();
    let remote = t.path().join("remote.git");
    fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "-q", "--bare", "-b", "main"]);

    // Pushed and clean: may go.
    repo(&scratch.join("pushed"));
    git(
        &scratch.join("pushed"),
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&scratch.join("pushed"), &["push", "-q", "origin", "main"]);
    // A file nobody committed.
    repo(&scratch.join("untracked"));
    fs::write(scratch.join("untracked/notes.txt"), "x").unwrap();
    // A commit no remote has.
    repo(&scratch.join("unpushed"));
    // A stash.
    repo(&scratch.join("stashed"));
    fs::write(scratch.join("stashed/README"), "changed").unwrap();
    git(&scratch.join("stashed"), &["stash", "-q"]);
    // A worktree with an uncommitted change, of a repository kept elsewhere.
    repo(&home.join("work/main-repo"));
    git(
        &home.join("work/main-repo"),
        &[
            "worktree",
            "add",
            "-q",
            scratch.join("wt").to_str().unwrap(),
        ],
    );
    fs::write(scratch.join("wt/README"), "changed").unwrap();
    // A repository a level down in an otherwise plain folder.
    repo(&scratch.join("holder/inner"));
    // No git at all.
    fs::create_dir_all(scratch.join("plain")).unwrap();
    fs::write(scratch.join("plain/f"), "x").unwrap();

    // Clean, pushed repositories that hold work further in, where the outer repository does not see it.
    let pushed_repo = |name: &str, ignore: &str| -> PathBuf {
        let r = scratch.join(name);
        repo(&r);
        git(&r, &["remote", "add", "origin", remote.to_str().unwrap()]);
        git(&r, &["push", "-q", "origin", &format!("main:{name}")]);
        git(&r, &["fetch", "-q", "origin"]);
        git(&r, &["branch", "-q", "-u", &format!("origin/{name}")]);
        fs::write(r.join(".git/info/exclude"), format!("{ignore}\n")).unwrap();
        r
    };
    // An ignored agent worktree on a pushed branch, with an edit and a new file.
    let r = pushed_repo("nested-worktree", ".claude/");
    git(
        &r,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feat",
            ".claude/worktrees/feat",
        ],
    );
    git(&r, &["push", "-q", "origin", "feat"]);
    fs::write(r.join(".claude/worktrees/feat/README"), "edited").unwrap();
    fs::write(r.join(".claude/worktrees/feat/new.txt"), "new").unwrap();
    // The same, one level down in a plain folder: the search goes on past the first repository it finds.
    let r = pushed_repo("deep/inner", ".claude/");
    git(
        &r,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feat3",
            ".claude/worktrees/f",
        ],
    );
    git(&r, &["push", "-q", "origin", "feat3"]);
    fs::write(r.join(".claude/worktrees/f/README"), "edited").unwrap();
    // The same, clean: may go.
    let r = pushed_repo("nested-clean", ".claude/");
    git(
        &r,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feat2",
            ".claude/worktrees/feat",
        ],
    );
    git(&r, &["push", "-q", "origin", "feat2"]);
    git(&r, &["fetch", "-q", "origin"]);
    // A detached worktree holding a commit only it has.
    let r = pushed_repo("nested-detached", ".worktrees/");
    git(&r, &["worktree", "add", "-q", "--detach", ".worktrees/x"]);
    fs::write(r.join(".worktrees/x/README"), "only here").unwrap();
    git(
        &r.join(".worktrees/x"),
        &["commit", "-q", "-am", "only here"],
    );
    // An ignored repository inside, with a commit no remote has.
    let r = pushed_repo("nested-repo", "vendor/");
    repo(&r.join("vendor/lib"));
    // A commit only a tag reaches: no branch has it.
    let r = pushed_repo("tagged", "");
    git(&r, &["checkout", "-q", "--detach"]);
    fs::write(r.join("README"), "tagged").unwrap();
    git(&r, &["commit", "-q", "-am", "tagged"]);
    git(&r, &["tag", "local-only"]);
    git(&r, &["checkout", "-q", "main"]);
    // A worktree kept outside the folder, which would lose its repository.
    let r = pushed_repo("outside-wt", "");
    git(
        &r,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            home.join("work/outside").to_str().unwrap(),
        ],
    );
    // A bare repository with a commit no other repository has.
    git(
        &scratch,
        &[
            "clone",
            "-q",
            "--bare",
            remote.to_str().unwrap(),
            "bare.git",
        ],
    );
    let src = t.path().join("src");
    repo(&src);
    git(
        &src,
        &[
            "push",
            "-q",
            scratch.join("bare.git").to_str().unwrap(),
            "main:only-in-bare",
        ],
    );
    // Bare clones that track their remote (`clone --bare`, the standard fetch refspec, fetched), with worktrees kept
    // inside their folders.
    let tracking_bare = |rel: &str| -> PathBuf {
        let b = scratch.join(rel);
        let parent = b.parent().unwrap();
        fs::create_dir_all(parent).unwrap();
        git(
            parent,
            &[
                "clone",
                "-q",
                "--bare",
                remote.to_str().unwrap(),
                b.file_name().unwrap().to_str().unwrap(),
            ],
        );
        git(
            &b,
            &[
                "config",
                "remote.origin.fetch",
                "+refs/heads/*:refs/remotes/origin/*",
            ],
        );
        git(&b, &["fetch", "-q", "origin"]);
        b
    };
    // At the top, with an edit and a new file in the worktree.
    let b = tracking_bare("bare-wt-top");
    git(&b, &["worktree", "add", "-q", "wt-main", "main"]);
    fs::write(b.join("wt-main/README"), "edited").unwrap();
    fs::write(b.join("wt-main/new.txt"), "new").unwrap();
    // Nested in a plain folder, with an edit.
    let b = tracking_bare("bare-holder/proj.git");
    git(&b, &["worktree", "add", "-q", "main", "main"]);
    fs::write(b.join("main/README"), "edited").unwrap();
    // The same, clean: may go.
    let b = tracking_bare("bare-clean");
    git(&b, &["worktree", "add", "-q", "w", "main"]);
    // A worktree under a name a bare repository uses for its own folders, which the walk does not enter: found
    // through the worktree list.
    let b = tracking_bare("bare-odd");
    git(&b, &["worktree", "add", "-q", "modules", "main"]);
    fs::write(b.join("modules/README"), "edited").unwrap();
    // An unrelated repository kept in a bare repository's folder, with a commit no remote has: found by the walk.
    let b = tracking_bare("bare-inner");
    repo(&b.join("tools"));
    // The same, one level down in a plain folder: the walk goes on inside a bare repository it finds.
    let b = tracking_bare("bare-deep/x.git");
    repo(&b.join("tools"));
    // A repository whose own config sets a filter command, which `git status` would run: kept, never run.
    let r = pushed_repo("filtered", "");
    fs::write(r.join(".gitattributes"), "* filter=x\n").unwrap();
    git(&r, &["add", ".gitattributes"]);
    git(&r, &["commit", "-q", "-m", "attributes"]);
    git(&r, &["push", "-q", "origin", "main:filtered"]);
    let marker = t.path().join("filter-ran");
    git(
        &r,
        &[
            "config",
            "filter.x.clean",
            &format!("touch {}; cat", marker.display()),
        ],
    );
    age_tree(&home, 30);

    let plan = plan_only(&config(&home), "stale-scratch");
    let mut planned: Vec<String> = plan
        .items
        .iter()
        .map(|i| i.action.label().rsplit('/').next().unwrap().to_string())
        .collect();
    planned.sort();
    assert_eq!(
        planned,
        vec!["bare-clean", "nested-clean", "plain", "pushed"],
        "{:?}",
        plan.skipped
    );
    for (name, why) in [
        ("untracked", "not committed"),
        ("unpushed", "not pushed"),
        ("stashed", "stash"),
        ("wt", "not committed"),
        ("holder", "not pushed"),
        ("nested-worktree", "not committed in"),
        ("deep", "not committed in"),
        ("nested-detached", "not pushed"),
        ("nested-repo", "not pushed in"),
        ("tagged", "not pushed"),
        ("outside-wt", "worktree"),
        ("bare.git", "not pushed"),
        ("bare-wt-top", "not committed in"),
        ("bare-holder", "not committed in"),
        ("bare-odd", "not committed in"),
        ("bare-inner", "not pushed in"),
        ("bare-deep", "not pushed in"),
        ("filtered", "filter"),
    ] {
        assert!(
            plan.skipped
                .iter()
                .any(|s| s.label.ends_with(&format!("/{name}")) && s.reason.contains(why)),
            "{name}: {:?}",
            plan.skipped
        );
    }
    assert!(!marker.exists(), "the repository's filter command was run");
}

/// A repository's own config can name commands git runs: a filter driver (set directly, through an include, in one
/// worktree's own config, or in a submodule's config), `gpg.program`, and the transport command of a partial clone's
/// promisor remote, which git runs to fetch a missing object. The check reads such a repository without running any
/// of them, and keeps it.
#[test]
fn commands_named_in_a_scanned_repositorys_config_are_never_run() {
    if !arca_janitor::util::on_path("git") {
        eprintln!("skipped: git is not installed");
        return;
    }
    let t = TempDir::new("git-commands");
    let home = t.path().join("home");
    let scratch = home.join("scratch");
    fs::create_dir_all(home.join("work")).unwrap();
    let remote = t.path().join("remote.git");
    fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "-q", "--bare", "-b", "main"]);
    let markers = t.path().join("markers");
    fs::create_dir_all(&markers).unwrap();
    // A filter command that leaves a marker named after its case.
    let leaves = |name: &str| format!("touch {}; cat", markers.join(name).display());
    let pushed_repo = |name: &str, ignore: &str| -> PathBuf {
        let r = scratch.join(name);
        repo(&r);
        fs::write(r.join(".gitattributes"), "* filter=x\n").unwrap();
        git(&r, &["add", ".gitattributes"]);
        git(&r, &["commit", "-q", "-m", "attributes"]);
        git(&r, &["remote", "add", "origin", remote.to_str().unwrap()]);
        git(&r, &["push", "-q", "origin", &format!("main:{name}")]);
        git(&r, &["fetch", "-q", "origin"]);
        git(&r, &["branch", "-q", "-u", &format!("origin/{name}")]);
        fs::write(r.join(".git/info/exclude"), format!("{ignore}\n")).unwrap();
        r
    };

    // A filter set through an include file.
    let r = pushed_repo("include", "");
    fs::write(
        r.join(".git/extra.conf"),
        format!("[filter \"x\"]\n\tclean = {}\n", leaves("include")),
    )
    .unwrap();
    git(&r, &["config", "include.path", "extra.conf"]);

    // A filter set in one worktree's own config.
    let r = pushed_repo("wtconfig", ".wt/");
    git(&r, &["config", "extensions.worktreeConfig", "true"]);
    git(&r, &["worktree", "add", "-q", "-b", "wt-feat", ".wt/feat"]);
    git(&r, &["push", "-q", "origin", "wt-feat"]);
    git(
        &r.join(".wt/feat"),
        &[
            "config",
            "--worktree",
            "filter.x.clean",
            &leaves("wtconfig"),
        ],
    );

    // A submodule whose own config sets a filter: the outer repository's status must not run it.
    let sub_src = t.path().join("sub-src");
    repo(&sub_src);
    fs::write(sub_src.join(".gitattributes"), "* filter=x\n").unwrap();
    git(&sub_src, &["add", ".gitattributes"]);
    git(&sub_src, &["commit", "-q", "-m", "attributes"]);
    let r = pushed_repo("submodule", "");
    git(
        &r,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            sub_src.to_str().unwrap(),
            "sub",
        ],
    );
    git(&r, &["commit", "-q", "-m", "submodule"]);
    git(&r, &["push", "-q", "origin", "main:submodule"]);
    git(
        &r.join("sub"),
        &["config", "filter.x.clean", &leaves("submodule")],
    );

    // A stash whose commit carries a signature, with gpg.program set to a command and signatures shown in logs.
    let r = pushed_repo("signed-stash", "");
    let tree = git_with(&r, &["rev-parse", "HEAD^{tree}"], None);
    let head = git_with(&r, &["rev-parse", "HEAD"], None);
    let commit = format!(
        "tree {tree}\nparent {head}\nauthor acme <dev@example.com> 1700000000 +0000\n\
         committer acme <dev@example.com> 1700000000 +0000\n\
         gpgsig -----BEGIN PGP SIGNATURE-----\n \n iQEz\n -----END PGP SIGNATURE-----\n\nstash\n"
    );
    let sha = git_with(
        &r,
        &["hash-object", "-t", "commit", "-w", "--stdin"],
        Some(&commit),
    );
    git(
        &r,
        &[
            "update-ref",
            "--create-reflog",
            "-m",
            "stash",
            "refs/stash",
            &sha,
        ],
    );
    let gpg = t.path().join("fake-gpg");
    fs::write(
        &gpg,
        format!(
            "#!/bin/sh\ntouch {}\nexit 1\n",
            markers.join("gpg").display()
        ),
    )
    .unwrap();
    fs::set_permissions(&gpg, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    git(&r, &["config", "gpg.program", gpg.to_str().unwrap()]);
    git(&r, &["config", "log.showSignature", "true"]);

    // A partial clone whose HEAD tree is missing, with an upload-pack command for its promisor remote: a lazy fetch
    // of the tree would run it.
    let r = scratch.join("promisor");
    repo(&r);
    git(&r, &["remote", "add", "origin", remote.to_str().unwrap()]);
    git(&r, &["push", "-q", "origin", "main:promisor"]);
    git(&r, &["fetch", "-q", "origin"]);
    for (key, value) in [
        ("core.repositoryformatversion", "1".to_string()),
        ("extensions.partialClone", "origin".to_string()),
        ("remote.origin.promisor", "true".to_string()),
        (
            "remote.origin.uploadpack",
            format!(
                "touch {}; git-upload-pack",
                markers.join("promisor").display()
            ),
        ),
    ] {
        git(&r, &["config", key, &value]);
    }
    let tree = git_with(&r, &["rev-parse", "HEAD^{tree}"], None);
    fs::remove_file(r.join(".git/objects").join(&tree[..2]).join(&tree[2..])).unwrap();
    age_tree(&home, 30);

    let plan = plan_only(&config(&home), "stale-scratch");
    let ran: Vec<String> = fs::read_dir(&markers)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        ran.is_empty(),
        "commands from a scanned config ran: {ran:?}"
    );
    assert!(plan.items.is_empty(), "{:?}", plan.items);
    for (name, why) in [
        ("include", "filter"),
        ("wtconfig", "filter"),
        ("submodule", "filter"),
        ("signed-stash", "stash"),
        ("promisor", "cannot be read"),
    ] {
        assert!(
            plan.skipped
                .iter()
                .any(|s| s.label.ends_with(&format!("/{name}")) && s.reason.contains(why)),
            "{name}: {:?}",
            plan.skipped
        );
    }
}
