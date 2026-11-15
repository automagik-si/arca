//! The git check of stale scratch honours the job's budget: a slow git cannot hold a plan or an apply past it, and
//! a check cut short ends the job as stopped, never as a refusal or, worse, as an item acted on late.
//!
//! The test runs itself again with a `git` first on PATH that logs each call and waits the seconds written in a file
//! before running the real git.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use arca_janitor::config::Config;
use arca_janitor::jobs::clean::{ApplyResult, STOPPED, apply_plan_with, make_plan};
use arca_janitor::jobs::{CleanPlanParams, Ctx, JobError, JobParams, Profile, Stop, Throttle};
use arca_janitor::store::Store;
use arca_janitor::testutil::{TempDir, age_tree};

const DIR: &str = "JANITOR_TEST_GIT_BUDGET_DIR";
const TEST: &str = "a_slow_git_check_stops_at_the_job_budget";
const BUDGET: Duration = Duration::from_secs(2);

fn real_git() -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|d| d.join("git"))
        .find(|g| g.is_file())
}

fn git(git: &Path, dir: &Path, args: &[&str]) {
    let out = Command::new(git)
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "acme")
        .env("GIT_AUTHOR_EMAIL", "dev@example.com")
        .env("GIT_COMMITTER_NAME", "acme")
        .env("GIT_COMMITTER_EMAIL", "dev@example.com")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
}

struct Run {
    dir: PathBuf,
    config: Config,
    params: CleanPlanParams,
}

impl Run {
    /// A job context whose budget ends `budget` from now, with a calm fake pressure file so the host's own pressure
    /// never pauses the run.
    fn ctx(&self, budget: Duration) -> Ctx {
        let mut ctx = Ctx::standalone(
            self.config.clone(),
            Store::new(self.dir.join("state")),
            JobParams::CleanPlan(self.params.clone()),
        );
        let psi = self.dir.join("psi");
        fs::write(&psi, "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\n").unwrap();
        ctx.throttle = Throttle {
            psi_path: psi,
            threshold: 10.0,
            check_every: Duration::from_millis(0),
            backoff_start: Duration::from_millis(40),
            backoff_max: Duration::from_millis(80),
        };
        ctx.deadline = Instant::now() + budget;
        ctx
    }

    /// Seconds each git call waits from now on.
    fn git_delay(&self, secs: u32) {
        fs::write(self.dir.join("delay"), secs.to_string()).unwrap();
    }

    fn calls(&self) -> usize {
        fs::read_to_string(self.dir.join("calls"))
            .unwrap_or_default()
            .lines()
            .count()
    }

    fn assert_stopped_untouched(&self, r: &ApplyResult, took: Duration, calls: usize, phase: &str) {
        assert!(
            r.stopped.is_some() && r.done.is_empty() && r.failed.is_empty(),
            "{phase}: {r:?} after {took:?}"
        );
        assert!(
            !r.skipped.iter().any(|s| s.reason == STOPPED),
            "{phase}: a stop recorded as a refusal: {:?}",
            r.skipped
        );
        assert!(
            self.dir.join("home/scratch/r/README").exists(),
            "{phase}: moved"
        );
        // The check makes seven calls; it stops at the first one that would start past the budget.
        assert!(calls < 6, "{phase}: {calls} git calls");
        assert!(took < Duration::from_secs(4), "{phase}: {took:?}");
    }
}

/// Inside the run with the slow git first on PATH.
fn inside(dir: &Path) {
    let home = dir.join("home");
    let mut config = Config::defaults(&home, "testhost");
    config.project_roots = vec![home.join("work")];
    config.scratch_roots = vec![home.join("scratch")];
    let run = Run {
        dir: dir.to_path_buf(),
        config,
        params: CleanPlanParams {
            profile: Profile::Full,
            only: vec!["stale-scratch".to_string()],
        },
    };

    // A plan whose git check is slow stops at the budget.
    run.git_delay(1);
    let ctx = run.ctx(BUDGET);
    let start = Instant::now();
    let before = run.calls();
    let r = make_plan(&run.config, Some(&ctx), &run.params);
    let (took, calls) = (start.elapsed(), run.calls() - before);
    assert!(
        matches!(r, Err(JobError::Stopped(Stop::Budget))),
        "the plan ran past its budget: {:?} after {took:?}, {calls} git calls",
        r.map(|p| p.skipped)
    );
    assert!(calls < 6, "plan: {calls} git calls");
    assert!(took < Duration::from_secs(4), "plan: {took:?}");

    // With a fast git the same folder is planned.
    run.git_delay(0);
    let plan = make_plan(
        &run.config,
        Some(&run.ctx(Duration::from_secs(600))),
        &run.params,
    )
    .unwrap();
    assert_eq!(plan.items.len(), 1, "{:?}", plan.skipped);

    // Slow in the check before anything changes.
    run.git_delay(1);
    let ctx = run.ctx(BUDGET);
    let (start, before) = (Instant::now(), run.calls());
    let r = apply_plan_with(&plan, &run.config, Some(&ctx), &mut || {}).unwrap();
    run.assert_stopped_untouched(&r, start.elapsed(), run.calls() - before, "first check");

    // Slow only in the check right before acting.
    run.git_delay(0);
    let ctx = run.ctx(BUDGET);
    let (start, mut before) = (Instant::now(), 0);
    let r = apply_plan_with(&plan, &run.config, Some(&ctx), &mut || {
        before = run.calls();
        run.git_delay(1);
    })
    .unwrap();
    run.assert_stopped_untouched(
        &r,
        start.elapsed(),
        run.calls() - before,
        "check before acting",
    );
}

#[test]
fn a_slow_git_check_stops_at_the_job_budget() {
    if let Ok(dir) = std::env::var(DIR) {
        inside(Path::new(&dir));
        return;
    }
    let Some(real) = real_git() else {
        eprintln!("skipped: git is not installed");
        return;
    };
    let t = TempDir::new("git-budget");
    let home = t.path().join("home");
    fs::create_dir_all(home.join("work")).unwrap();
    // A clean repository, pushed, so the check runs every call it has and the folder may be planned.
    let remote = t.path().join("remote.git");
    fs::create_dir_all(&remote).unwrap();
    git(&real, &remote, &["init", "-q", "--bare", "-b", "main"]);
    let r = home.join("scratch/r");
    fs::create_dir_all(&r).unwrap();
    git(&real, &r, &["init", "-q", "-b", "main"]);
    fs::write(r.join("README"), "x").unwrap();
    git(&real, &r, &["add", "README"]);
    git(&real, &r, &["commit", "-q", "-m", "first"]);
    git(
        &real,
        &r,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&real, &r, &["push", "-q", "origin", "main"]);
    age_tree(&home, 30);

    let bin = t.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let slow = bin.join("git");
    fs::write(
        &slow,
        format!(
            "#!/bin/sh\necho \"$*\" >> {calls}\nsleep \"$(cat {delay})\"\nexec {real} \"$@\"\n",
            calls = t.path().join("calls").display(),
            delay = t.path().join("delay").display(),
            real = real.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&slow, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(t.path().join("delay"), "0").unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new(std::env::current_exe().unwrap())
        .args([TEST, "--exact", "--nocapture", "--test-threads=1"])
        .env(DIR, t.path())
        .env("PATH", path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The walk that looks for repositories stops when the job is cancelled.
#[test]
fn a_cancelled_job_stops_the_walk_for_repositories() {
    let t = TempDir::new("git-walk-cancel");
    let folder = t.path().join("scratch/big");
    for i in 0..300 {
        fs::create_dir_all(folder.join(format!("d{i}"))).unwrap();
    }
    let run = Run {
        dir: t.path().to_path_buf(),
        config: Config::defaults(t.path(), "testhost"),
        params: CleanPlanParams {
            profile: Profile::Full,
            only: vec!["stale-scratch".to_string()],
        },
    };
    let ctx = run.ctx(Duration::from_secs(600));
    ctx.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    let r = arca_janitor::git::unsaved_work(&folder, &Default::default(), Some(&ctx));
    assert_eq!(r, Err(Stop::Cancelled));
}
