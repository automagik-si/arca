//! The pool against the fake GitHub and the fake launcher: scale sets, sessions, demand, one fresh machine per job,
//! the budget, warm runners, the state-leak test and the sign-in alert's state.

use std::sync::Arc;
use std::time::{Duration, Instant};

use arca_runners::access::RegistrationAccess;
use arca_runners::config::PoolConfig;
use arca_runners::fake::{FakeGitHub, FakeLauncher, JobSpec};
use arca_runners::record::Recorder;
use arca_runners::scaleset::{Http, ScaleSetClient};
use arca_runners::signin::SignInReason;
use arca_runners::{Pool, Secret, SignInBoard};
use serde_json::json;
use tokio::sync::watch;

const TOKEN: &str = "fake-installation-token";
const LABEL: &str = "acme-4vcpu-ubuntu-2404";

struct Rig {
    github: FakeGitHub,
    launcher: Arc<FakeLauncher>,
    pool: Arc<Pool<RegistrationAccess, FakeLauncher>>,
    signin: SignInBoard,
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
    dir: std::path::PathBuf,
}

fn config(warm: u32, max: u32, budget_cpu: u32) -> PoolConfig {
    let config: PoolConfig = serde_json::from_value(json!({
        "name": "acme",
        "binding": "acme-org",
        "url": "https://github.com/acme",
        "runnerGroupId": 3,
        "budget": { "cpu": budget_cpu, "memory": "256GiB" },
        "project": "arcak-runners",
        "image": "runner-ubuntu-2404",
        "classes": [
            { "labels": [LABEL], "cpu": 4, "memory": "16GiB", "warm": warm, "max": max },
            { "labels": ["acme-2vcpu-ubuntu-2404", "acme-small"], "cpu": 2, "memory": "8GiB", "max": 2 }
        ]
    }))
    .expect("a pool");
    config.check().expect("valid");
    config
}

async fn rig(config: PoolConfig, setup: impl FnOnce(&FakeGitHub, &FakeLauncher)) -> Rig {
    let github = FakeGitHub::start(TOKEN).await.expect("fake GitHub");
    let launcher = Arc::new(FakeLauncher::new(github.clone()));
    setup(&github, &launcher);
    let http = Http::new().expect("http");
    let access = Arc::new(
        RegistrationAccess::new(
            http.clone(),
            github.api_url(),
            &config.url,
            Secret::new(TOKEN),
        )
        .expect("access"),
    );
    let client = Arc::new(ScaleSetClient::new(http, access, &config.binding));
    let signin = SignInBoard::new();
    let dir = std::env::temp_dir().join(format!(
        "arca-runners-test-{}-{}",
        std::process::id(),
        arca_runners::time::now_ms()
    ));
    let recorder = Recorder::open(dir.clone(), 7).expect("recorder");
    let pool = Pool::new(
        config,
        client,
        Arc::clone(&launcher),
        signin.clone(),
        recorder,
        "node-a",
    );
    let (stop, receiver) = watch::channel(false);
    let task = tokio::spawn(Arc::clone(&pool).run(receiver));
    Rig {
        github,
        launcher,
        pool,
        signin,
        stop,
        task,
        dir,
    }
}

impl Rig {
    async fn finish(self) {
        let _ = self.stop.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(10), self.task).await;
        let _ = std::fs::remove_dir_all(&self.dir);
    }

    fn jobs_file(&self) -> Vec<serde_json::Value> {
        std::fs::read_to_string(self.dir.join("jobs.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("a job line"))
            .collect()
    }
}

async fn until(what: &str, limit: Duration, mut check: impl FnMut() -> bool) {
    let start = Instant::now();
    while !check() {
        assert!(start.elapsed() < limit, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn job(name: &str, labels: &[&str], seconds: f64) -> JobSpec {
    JobSpec {
        name: name.to_string(),
        repo: "omni".to_string(),
        labels: labels.iter().map(|label| (*label).to_string()).collect(),
        seconds,
        ..JobSpec::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_pool_makes_one_scale_set_per_class_with_the_persons_labels() {
    let rig = rig(config(0, 4, 32), |_, _| {}).await;
    until("both sessions", Duration::from_secs(10), || {
        rig.github.session_open(LABEL) && rig.github.session_open("acme-2vcpu-ubuntu-2404")
    })
    .await;
    let mut sets = rig.github.scale_sets();
    sets.sort();
    assert_eq!(
        sets,
        vec![
            (
                "acme-2vcpu-ubuntu-2404".to_string(),
                vec![
                    "acme-2vcpu-ubuntu-2404".to_string(),
                    "acme-small".to_string()
                ]
            ),
            (LABEL.to_string(), vec![LABEL.to_string()]),
        ]
    );
    // Nothing runs when the queue is empty and no class keeps warm runners.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(rig.launcher.created().is_empty());
    assert_eq!(rig.signin.read()["acme"].failing, None);
    // Every Actions service call named the API version; the polls told GitHub the class's capacity.
    let calls = rig.github.calls();
    assert!(
        calls
            .iter()
            .filter(|call| call.path.starts_with("/svc/"))
            .all(|call| call.query.contains("api-version=6.0-preview"))
    );
    assert!(
        calls
            .iter()
            .any(|call| call.path.starts_with("/queue/") && call.capacity == Some(4))
    );
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_job_gets_a_fresh_machine_of_its_class_that_is_destroyed_after_it() {
    let rig = rig(config(0, 4, 32), |_, _| {}).await;
    until("the session", Duration::from_secs(10), || {
        rig.github.session_open(LABEL)
    })
    .await;
    for index in 0..3 {
        rig.github
            .queue_job(job(&format!("plugin-gate {index}"), &[LABEL], 0.2));
    }
    rig.github.queue_job(job("small", &["acme-small"], 0.1));
    until("four finished jobs", Duration::from_secs(20), || {
        rig.github.finished().len() == 4
    })
    .await;
    until("every machine gone", Duration::from_secs(10), || {
        rig.launcher.live().is_empty()
    })
    .await;

    let finished = rig.github.finished();
    assert!(
        finished.iter().all(|job| job.result == "succeeded"),
        "{finished:?}"
    );
    // Four jobs, four machines, four distinct runners: none reused.
    let created = rig.launcher.created();
    assert_eq!(created.len(), 4, "{created:?}");
    let mut runners: Vec<&str> = finished.iter().map(|job| job.runner.as_str()).collect();
    runners.sort_unstable();
    runners.dedup();
    assert_eq!(runners.len(), 4);
    // Each machine has its class's size.
    let small = created
        .iter()
        .find(|spec| spec.cpu == 2)
        .expect("a small machine");
    assert_eq!(small.memory_mib, 8 * 1024);
    assert_eq!(
        created
            .iter()
            .filter(|spec| spec.cpu == 4 && spec.memory_mib == 16 * 1024)
            .count(),
        3
    );
    // Every machine was destroyed, and a runner that ran its job is not removed by hand (GitHub removes it).
    assert_eq!(rig.launcher.destroyed().len(), 4);
    assert!(rig.github.removed_runners().is_empty());
    until("four job lines", Duration::from_secs(5), || {
        rig.jobs_file().len() == 4
    })
    .await;
    let lines = rig.jobs_file();
    for line in &lines {
        assert_eq!(line["result"], "succeeded");
        assert_eq!(line["repository"], "acme/omni");
        assert!(line["queueSeconds"].as_f64().is_some());
        assert!(
            line["logs"]
                .as_str()
                .is_some_and(|path| path.ends_with("logs.tar.gz"))
        );
    }
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_budget_caps_the_machines_and_the_queue_drains() {
    // A budget of 8 CPUs holds two 4-CPU machines at once.
    let rig = rig(config(0, 10, 8), |_, _| {}).await;
    until("the session", Duration::from_secs(10), || {
        rig.github.session_open(LABEL)
    })
    .await;
    for index in 0..6 {
        rig.github
            .queue_job(job(&format!("j{index}"), &[LABEL], 0.3));
    }
    until("six finished jobs", Duration::from_secs(30), || {
        rig.github.finished().len() == 6
    })
    .await;
    assert!(rig.launcher.peak() <= 2, "peak {}", rig.launcher.peak());
    assert_eq!(rig.launcher.created().len(), 6);
    // The capacity GitHub was told never passed what the budget holds.
    assert!(
        rig.github
            .calls()
            .iter()
            .filter_map(|call| call.capacity)
            .all(|capacity| capacity <= 2)
    );
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_warm_runner_waits_ready_and_is_replaced_after_its_job() {
    let rig = rig(config(1, 4, 32), |_, _| {}).await;
    until("a warm runner", Duration::from_secs(10), || {
        rig.pool.status().classes[0].idle == 1
    })
    .await;
    let warm = rig.launcher.created()[0].name.clone();
    rig.github.queue_job(job("fast", &[LABEL], 0.1));
    until("the job", Duration::from_secs(10), || {
        rig.github.finished().len() == 1
    })
    .await;
    // The warm machine took the job, and a new warm one is ready after it.
    assert_eq!(rig.github.finished()[0].runner, warm);
    until("the next warm runner", Duration::from_secs(10), || {
        let status = rig.pool.status();
        status.classes[0].idle == 1 && !rig.launcher.live().contains(&warm)
    })
    .await;
    assert!(
        rig.github.finished()[0].queue_ms < 2_000,
        "{:?}",
        rig.github.finished()
    );
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn job_n_plus_one_finds_nothing_job_n_left() {
    let rig = rig(config(0, 1, 32), |_, _| {}).await;
    until("the session", Duration::from_secs(10), || {
        rig.github.session_open(LABEL)
    })
    .await;
    let markers: Vec<String> = [
        "/home/runner/marker",
        "/tmp/marker",
        "/opt/marker",
        "docker-image:leak-test",
    ]
    .iter()
    .map(|path| (*path).to_string())
    .collect();
    let mut writer = job("leak writer", &[LABEL], 0.1);
    writer.writes = markers.clone();
    let mut reader = job("leak reader", &[LABEL], 0.1);
    reader.absent = markers;
    rig.github.queue_job(writer);
    until("the writer", Duration::from_secs(10), || {
        rig.github.finished().len() == 1
    })
    .await;
    rig.github.queue_job(reader);
    until("the reader", Duration::from_secs(10), || {
        rig.github.finished().len() == 2
    })
    .await;
    let finished = rig.github.finished();
    assert_eq!(
        finished[1].result, "succeeded",
        "job N+1 found what job N left"
    );
    assert_ne!(finished[0].runner, finished[1].runner);
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_leak_test_fails_on_a_machine_that_keeps_its_disk() {
    // The same test against the fixed-slot behaviour proves the test can see a leak.
    let rig = rig(config(0, 1, 32), |_, launcher| launcher.reuse_disks(true)).await;
    until("the session", Duration::from_secs(10), || {
        rig.github.session_open(LABEL)
    })
    .await;
    let mut writer = job("leak writer", &[LABEL], 0.1);
    writer.writes = vec!["/tmp/marker".to_string()];
    let mut reader = job("leak reader", &[LABEL], 0.1);
    reader.absent = vec!["/tmp/marker".to_string()];
    rig.github.queue_job(writer);
    until("the writer", Duration::from_secs(10), || {
        rig.github.finished().len() == 1
    })
    .await;
    rig.github.queue_job(reader);
    until("the reader", Duration::from_secs(10), || {
        rig.github.finished().len() == 2
    })
    .await;
    assert_eq!(rig.github.finished()[1].result, "failed");
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn machines_left_by_an_earlier_run_are_removed_at_start() {
    let rig = rig(config(0, 4, 32), |_, launcher| {
        launcher.leave_machine("acme-0-deadbeef");
    })
    .await;
    until("the leftover gone", Duration::from_secs(10), || {
        rig.launcher.live().is_empty()
    })
    .await;
    assert_eq!(
        rig.launcher.destroyed(),
        vec!["acme-0-deadbeef".to_string()]
    );
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_machine_that_fails_to_start_is_retried_and_its_runner_never_lingers() {
    let rig = rig(config(0, 4, 32), |_, launcher| launcher.fail_creates(1)).await;
    until("the session", Duration::from_secs(10), || {
        rig.github.session_open(LABEL)
    })
    .await;
    rig.github.queue_job(job("after a failure", &[LABEL], 0.1));
    until("the job", Duration::from_secs(30), || {
        rig.github.finished().len() == 1
    })
    .await;
    assert_eq!(rig.github.finished()[0].result, "succeeded");
    until("the failure's line", Duration::from_secs(5), || {
        rig.jobs_file().iter().any(|line| {
            line["note"]
                .as_str()
                .is_some_and(|note| note.contains("could not be made"))
        })
    })
    .await;
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_uninstalled_app_puts_the_pool_on_the_signin_board() {
    let rig = rig(config(0, 4, 32), |github, _| github.set_installed(false)).await;
    until("a sign-in failure", Duration::from_secs(10), || {
        rig.signin
            .read()
            .get("acme")
            .and_then(|state| state.failing)
            == Some(SignInReason::AppNotInstalled)
    })
    .await;
    // Reinstalled: the next try signs in and the state clears.
    rig.github.set_installed(true);
    until("signed in again", Duration::from_secs(20), || {
        rig.signin
            .read()
            .get("acme")
            .is_some_and(|state| state.failing.is_none())
    })
    .await;
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bad_key_puts_the_pool_on_the_signin_board() {
    let rig = rig(config(0, 4, 32), |github, _| {
        github.set_token("fake-other-token")
    })
    .await;
    until("a sign-in failure", Duration::from_secs(10), || {
        rig.signin
            .read()
            .get("acme")
            .and_then(|state| state.failing)
            == Some(SignInReason::BadCredentials)
    })
    .await;
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stopping_closes_the_sessions_and_removes_idle_machines() {
    let rig = rig(config(1, 4, 32), |_, _| {}).await;
    until("a warm runner", Duration::from_secs(10), || {
        rig.pool.status().classes[0].idle == 1
    })
    .await;
    let github = rig.github.clone();
    let launcher = Arc::clone(&rig.launcher);
    let _ = rig.stop.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(10), rig.task).await;
    assert!(!github.session_open(LABEL));
    assert!(launcher.live().is_empty());
    // The idle runner never took a job, so it is removed from GitHub.
    assert_eq!(github.removed_runners().len(), 1);
    let _ = std::fs::remove_dir_all(&rig.dir);
}
