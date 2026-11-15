//! Regression tests from two live runs of an older cleanup kit.
//!
//! 1. Writes on a read-only target failed and the kit still logged "removed" and counted the bytes as freed. Here
//!    every action is verified on disk; a failed item is reported as failed with the OS error, is not counted as
//!    freed, and makes the job fail (a non-zero exit).
//! 2. Removing one build output changed its project's modification time, so a sibling checked afterwards looked
//!    fresh and was skipped. Here every verdict is decided before anything is changed.
//!
//! "Read-only" is made with mode 0555 folders (no root needed); the failure is the same class as a read-only
//! filesystem: the removal or move is refused by the kernel.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use arca_janitor::config::Config;
use arca_janitor::daemon::{Daemon, DaemonOptions};
use arca_janitor::jobs::clean::{Action, Item, Origin, Plan, apply_plan, make_plan};
use arca_janitor::jobs::{CleanPlanParams, Ctx, JobParams, Profile, TrashPurgeParams};
use arca_janitor::platform;
use arca_janitor::store::Store;
use arca_janitor::testutil::{TempDir, age_tree};

const BIN: &str = env!("CARGO_BIN_EXE_arca-janitor");

/// Makes a folder read-only, and writable again when dropped (so the temp tree can be removed).
struct ReadOnly(PathBuf);
impl ReadOnly {
    fn new(p: &Path) -> ReadOnly {
        fs::set_permissions(p, fs::Permissions::from_mode(0o555)).unwrap();
        ReadOnly(p.to_path_buf())
    }
}
impl Drop for ReadOnly {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
    }
}

/// Root ignores folder modes, so the read-only cases cannot be made as root (and the janitor refuses root anyway).
fn root() -> bool {
    platform::euid() == 0
}

fn config(home: &Path) -> Config {
    let mut c = Config::defaults(home, "testhost");
    c.project_roots = vec![home.join("work")];
    c.scratch_roots = vec![home.join("scratch")];
    c
}

fn home(t: &TempDir) -> PathBuf {
    let h = t.path().join("home");
    fs::create_dir_all(h.join("work")).unwrap();
    fs::create_dir_all(h.join("scratch")).unwrap();
    h
}

fn rust_project(home: &Path, name: &str) -> PathBuf {
    let p = home.join("work").join(name);
    fs::create_dir_all(p.join("target/debug")).unwrap();
    fs::write(p.join("Cargo.toml"), "").unwrap();
    fs::write(p.join("target/debug/blob"), vec![1u8; 40_000]).unwrap();
    age_tree(&p, 30);
    p
}

fn plan(c: &Config, only: &[&str]) -> Plan {
    let params = CleanPlanParams {
        profile: Profile::Full,
        only: only.iter().map(|s| s.to_string()).collect(),
    };
    make_plan(c, None, &params).unwrap()
}

fn hand_plan(items: Vec<Action>) -> Plan {
    Plan {
        id: "phand-1".into(),
        created: 0,
        host: "testhost".into(),
        os: std::env::consts::OS.into(),
        profile: Profile::Full,
        only: vec![],
        items: items
            .into_iter()
            .enumerate()
            .map(|(id, action)| Item {
                id,
                category: "test".into(),
                action,
                bytes: 1000,
                reason: String::new(),
                guards: vec![],
                origin: Origin::default(),
            })
            .collect(),
        skipped: vec![],
        would_free_bytes: 0,
        would_trash_bytes: 0,
        applied_by: None,
    }
}

#[test]
fn a_removal_the_kernel_refuses_is_failed_not_freed() {
    if root() {
        return;
    }
    let t = TempDir::new("ro-remove");
    let h = home(&t);
    let c = config(&h);
    let ok = rust_project(&h, "ok");
    let ro = rust_project(&h, "ro");
    let p = plan(&c, &["build-outputs"]);
    assert_eq!(p.items.len(), 2);
    let _lock = ReadOnly::new(&ro.join("target/debug"));
    let r = apply_plan(&p, &c, None).unwrap();
    assert_eq!(r.done.len(), 1, "{r:?}");
    assert_eq!(r.done[0].label, ok.join("target").display().to_string());
    assert_eq!(r.failed.len(), 1);
    assert_eq!(r.failed[0].label, ro.join("target").display().to_string());
    assert!(
        r.failed[0].error.contains("Permission denied"),
        "{}",
        r.failed[0].error
    );
    assert!(ro.join("target/debug/blob").exists(), "nothing was removed");
    assert_eq!(
        r.freed_bytes, r.done[0].bytes,
        "only the verified item counts"
    );
    assert_eq!(r.failed_bytes, r.failed[0].bytes);
    let why = r.failure().expect("a run with a failed item fails");
    assert!(why.starts_with("1 items failed"), "{why}");
}

#[test]
fn a_move_into_a_read_only_scratch_root_is_failed_and_the_folder_stays() {
    if root() {
        return;
    }
    let t = TempDir::new("ro-move");
    let h = home(&t);
    let c = config(&h);
    let old = h.join("scratch/old-run");
    fs::create_dir_all(&old).unwrap();
    fs::write(old.join("f"), "x").unwrap();
    age_tree(&old, 30);
    let p = plan(&c, &["stale-scratch"]);
    assert_eq!(p.items.len(), 1);
    let _lock = ReadOnly::new(&h.join("scratch"));
    let r = apply_plan(&p, &c, None).unwrap();
    assert!(r.done.is_empty(), "{r:?}");
    assert_eq!(r.trash_bytes, 0);
    assert!(
        r.failed[0].error.contains("cannot make"),
        "{}",
        r.failed[0].error
    );
    assert!(
        r.failed[0].error.contains("Permission denied"),
        "{}",
        r.failed[0].error
    );
    assert!(old.join("f").exists());
}

#[test]
fn emptying_and_commands_are_verified_too() {
    if root() {
        return;
    }
    let t = TempDir::new("ro-empty");
    let cache = t.path().join("cache");
    fs::create_dir_all(cache.join("locked")).unwrap();
    fs::write(cache.join("locked/keep"), "x").unwrap();
    fs::write(cache.join("loose"), "x").unwrap();
    let clean = t.path().join("clean-cache");
    fs::create_dir_all(clean.join("sub")).unwrap();
    fs::write(clean.join("sub/f"), "x").unwrap();
    let _lock = ReadOnly::new(&cache.join("locked"));
    let p = hand_plan(vec![
        Action::EmptyDir {
            path: cache.clone(),
        },
        Action::EmptyDir {
            path: clean.clone(),
        },
        Action::Command {
            argv: vec!["false".into()],
        },
        Action::Command {
            argv: vec!["true".into()],
        },
    ]);
    let r = apply_plan(&p, &Config::defaults(t.path(), "testhost"), None).unwrap();
    let done: Vec<&str> = r.done.iter().map(|d| d.label.as_str()).collect();
    assert_eq!(done, vec![clean.display().to_string().as_str(), "true"]);
    assert!(clean.is_dir() && fs::read_dir(&clean).unwrap().next().is_none());
    let errors: Vec<(&str, &str)> = r
        .failed
        .iter()
        .map(|f| (f.label.as_str(), f.error.as_str()))
        .collect();
    assert_eq!(errors.len(), 2, "{errors:?}");
    assert!(errors[0].1.contains("Permission denied"), "{errors:?}");
    assert_eq!(errors[1].0, "false");
    assert!(errors[1].1.starts_with("exit status 1"), "{errors:?}");
    assert_eq!(r.freed_bytes, 2000);
    assert_eq!(r.failed_bytes, 2000);
}

#[test]
fn siblings_in_one_idle_project_are_both_removed_in_one_apply() {
    let t = TempDir::new("siblings");
    let h = home(&t);
    let c = config(&h);
    let p = h.join("work/web");
    for out in ["node_modules/pkg", ".next/cache", ".turbo/x"] {
        fs::create_dir_all(p.join(out)).unwrap();
        fs::write(p.join(out).join("f"), vec![2u8; 10_000]).unwrap();
    }
    fs::write(p.join("package.json"), "{}").unwrap();
    age_tree(&p, 30);
    let plan = plan(&c, &["build-outputs"]);
    assert_eq!(plan.items.len(), 3, "{:?}", plan.skipped);
    let r = apply_plan(&plan, &c, None).unwrap();
    assert!(
        r.skipped.is_empty(),
        "the first removal made the others look fresh: {:?}",
        r.skipped
    );
    assert!(r.failed.is_empty(), "{:?}", r.failed);
    assert_eq!(r.done.len(), 3);
    for out in ["node_modules", ".next", ".turbo"] {
        assert!(!p.join(out).exists(), "{out}");
    }
}

#[test]
fn a_dry_run_says_would_and_frees_nothing() {
    let t = TempDir::new("dry");
    let h = home(&t);
    let day = h.join("scratch/.cleanup-trash/2000-01-01");
    fs::create_dir_all(day.join("x")).unwrap();
    // A file, not just a folder: an empty folder takes no blocks on tmpfs.
    fs::write(day.join("x/blob"), vec![1u8; 40_000]).unwrap();
    let ctx = Ctx::standalone(
        config(&h),
        Store::new(t.path().join("state")),
        JobParams::TrashPurge(TrashPurgeParams { dry_run: true }),
    );
    let r =
        arca_janitor::jobs::clean::trash_purge(&TrashPurgeParams { dry_run: true }, &ctx).unwrap();
    assert!(r.dry_run);
    assert_eq!(r.would.len(), 1);
    assert!(r.done.is_empty());
    assert_eq!(r.freed_bytes, 0);
    assert!(r.would_free_bytes > 0);
    assert!(day.exists(), "nothing changed");
}

#[test]
fn a_socket_or_state_folder_that_cannot_be_made_fails_loudly() {
    if root() {
        return;
    }
    let t = TempDir::new("loud");
    let locked = t.path().join("locked");
    fs::create_dir_all(&locked).unwrap();
    let _lock = ReadOnly::new(&locked);
    let home = t.path().to_path_buf();
    let start = |state: PathBuf, socket: PathBuf| {
        let home = home.clone();
        Daemon::start(DaemonOptions {
            store: Store::new(state),
            socket,
            priority: platform::Priority {
                nice: Some(19),
                io: Some("idle".into()),
                lowered: true,
                detail: String::new(),
            },
            config: std::sync::Arc::new(move || Ok(Config::defaults(&home, "testhost"))),
            throttle: None,
        })
    };
    let e = start(t.path().join("state"), locked.join("j.sock"))
        .err()
        .expect("refused");
    assert!(e.to_string().contains("Permission denied"), "{e}");
    assert!(
        !e.to_string().contains("already"),
        "not mistaken for another daemon: {e}"
    );
    let e = start(locked.join("state"), t.path().join("j.sock"))
        .err()
        .expect("refused");
    assert!(e.to_string().contains("Permission denied"), "{e}");

    // The binary says so and exits 2, not 0.
    let out = Command::new(BIN)
        .args(["serve", "--foreground"])
        .env("JANITOR_SOCKET", locked.join("j.sock"))
        .env("JANITOR_STATE_DIR", t.path().join("state2"))
        .env("JANITOR_CONFIG_DIR", t.path().join("conf"))
        .env("HOME", t.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("Permission denied"));
}

/// Stops the daemon when the test ends, also when an assertion fails first (a leaked daemon keeps the test
/// harness's output pipe open).
struct Daemonized(std::process::Child);
impl Drop for Daemonized {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A config whose pressure source reads zero, so the host's own IO load cannot pause the test's jobs.
fn calm_config(t: &Path) {
    let conf = t.join("conf");
    fs::create_dir_all(&conf).unwrap();
    let psi = t.join("psi");
    fs::write(&psi, "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\n").unwrap();
    fs::write(
        conf.join("cleanup.conf"),
        format!("PSI_PATH={}\n", psi.display()),
    )
    .unwrap();
}

fn cli(t: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .env("JANITOR_SOCKET", t.join("j.sock"))
        .env("JANITOR_STATE_DIR", t.join("state"))
        .env("JANITOR_CONFIG_DIR", t.join("conf"))
        .env("HOME", home)
        .output()
        .unwrap()
}

#[test]
fn through_the_binary_a_failed_item_fails_the_job_and_the_report_says_so() {
    if root() {
        return;
    }
    let t = TempDir::new("ro-cli");
    let h = t.path().join("home");
    let ro = h.join("proj");
    fs::create_dir_all(ro.join("target/debug")).unwrap();
    fs::write(ro.join("Cargo.toml"), "").unwrap();
    fs::write(ro.join("target/debug/blob"), vec![1u8; 40_000]).unwrap();
    age_tree(&h, 30);
    calm_config(t.path());
    let _daemon = Daemonized(
        Command::new(BIN)
            .args(["serve", "--foreground"])
            .env("JANITOR_SOCKET", t.path().join("j.sock"))
            .env("JANITOR_STATE_DIR", t.path().join("state"))
            .env("JANITOR_CONFIG_DIR", t.path().join("conf"))
            .env("HOME", &h)
            .spawn()
            .unwrap(),
    );
    // The daemon runs with idle IO, so on a busy disk a job can take minutes; `wait` returns as soon as it ends.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cli(t.path(), &h, &["list"]).status.success() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let id = String::from_utf8_lossy(
        &cli(
            t.path(),
            &h,
            &["submit", "clean-plan", "--only", "build-outputs"],
        )
        .stdout,
    )
    .trim()
    .to_string();
    assert_eq!(
        cli(t.path(), &h, &["wait", &id, "--timeout", "300"])
            .status
            .code(),
        Some(0)
    );
    let plan: serde_json::Value =
        serde_json::from_slice(&cli(t.path(), &h, &["result", &id, "--json"]).stdout).unwrap();
    assert_eq!(plan["items"], 1, "{plan}");
    let plan_id = plan["plan"].as_str().unwrap().to_string();
    let lock = ReadOnly::new(&ro.join("target/debug"));
    let apply =
        String::from_utf8_lossy(&cli(t.path(), &h, &["submit", "clean-apply", &plan_id]).stdout)
            .trim()
            .to_string();
    let waited = cli(t.path(), &h, &["wait", &apply, "--timeout", "300"]);
    assert_eq!(
        waited.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&waited.stdout)
    );
    let shown = cli(t.path(), &h, &["result", &apply]);
    assert_eq!(shown.status.code(), Some(1));
    let text = String::from_utf8_lossy(&shown.stdout).to_string();
    assert!(
        text.contains("done and verified: 0 items, freed 0 B"),
        "{text}"
    );
    assert!(
        text.contains("FAILED: 1 items") && text.contains("Permission denied"),
        "{text}"
    );
    let report = String::from_utf8_lossy(&cli(t.path(), &h, &["report"]).stdout).to_string();
    assert!(
        report.contains("done 0: freed 0 B") && report.contains("FAILED 1"),
        "{report}"
    );
    drop(lock);
    let _ = cli(t.path(), &h, &["shutdown"]);
}
