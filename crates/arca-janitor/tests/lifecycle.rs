//! The job runner in-process: submit, one job at a time, cancel queued and running, refusal when the priority is not
//! lowered, and recovery after a restart.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arca_janitor::config::Config;
use arca_janitor::daemon::{Daemon, DaemonOptions, Request, Response, request};
use arca_janitor::jobs::{Job, JobParams, ProbeParams, SpaceReportParams, State};
use arca_janitor::platform::Priority;
use arca_janitor::store::Store;
use arca_janitor::testutil::TempDir;

fn priority(lowered: bool) -> Priority {
    Priority {
        nice: Some(if lowered { 19 } else { 0 }),
        io: Some(if lowered { "idle" } else { "none" }.to_string()),
        lowered,
        // The in-process daemon runs inside the test; its priority is stated, not read. The read-back itself is
        // proven against the real binary in tests/cli.rs.
        detail: "stated by the test".to_string(),
    }
}

fn start(t: &TempDir, lowered: bool) -> (Daemon, std::path::PathBuf) {
    let home = t.path().to_path_buf();
    let socket = t.path().join("j.sock");
    let daemon = Daemon::start(DaemonOptions {
        store: Store::new(t.path().join("state")),
        socket: socket.clone(),
        priority: priority(lowered),
        config: Arc::new(move || Ok(Config::defaults(&home, "testhost"))),
        throttle: None,
    })
    .unwrap();
    (daemon, socket)
}

fn submit(socket: &Path, job: JobParams) -> Response {
    request(
        socket,
        &Request::Submit {
            job,
            budget_min: None,
        },
    )
    .unwrap()
}

fn probe(seconds: u64) -> JobParams {
    JobParams::Probe(ProbeParams { seconds })
}

fn status(socket: &Path, id: &str) -> Job {
    request(socket, &Request::Status { id: id.to_string() })
        .unwrap()
        .job
        .unwrap()
}

fn wait_for(socket: &Path, id: &str, state: State) -> Job {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let job = status(socket, id);
        if job.state == state || Instant::now() > deadline {
            return job;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn one_job_runs_at_a_time_and_both_kinds_of_cancel_work() {
    let t = TempDir::new("lifecycle");
    let (daemon, socket) = start(&t, true);
    let first = submit(&socket, probe(30)).id.unwrap();
    let second = submit(&socket, probe(30)).id.unwrap();
    assert_eq!(
        wait_for(&socket, &first, State::Running).state,
        State::Running
    );
    assert_eq!(
        status(&socket, &second).state,
        State::Queued,
        "one at a time"
    );

    let r = request(&socket, &Request::Cancel { id: second.clone() }).unwrap();
    assert!(r.ok, "{r:?}");
    assert_eq!(status(&socket, &second).state, State::Cancelled);

    let start = Instant::now();
    assert!(
        request(&socket, &Request::Cancel { id: first.clone() })
            .unwrap()
            .ok
    );
    let job = wait_for(&socket, &first, State::Cancelled);
    assert_eq!(job.state, State::Cancelled);
    assert!(start.elapsed() < Duration::from_secs(3));
    assert!(
        !request(&socket, &Request::Cancel { id: first.clone() })
            .unwrap()
            .ok,
        "already ended"
    );

    // The next job runs to the end and keeps its result on disk.
    let third = submit(&socket, probe(0)).id.unwrap();
    let job = wait_for(&socket, &third, State::Done);
    assert_eq!(job.state, State::Done);
    assert_eq!(job.result.unwrap()["seconds"], 0);
    let list = request(&socket, &Request::List).unwrap().jobs.unwrap();
    assert_eq!(list.len(), 3);
    daemon.stop();
    let stored: Job = Store::new(t.path().join("state")).load_job(&third).unwrap();
    assert_eq!(stored.state, State::Done);
}

#[test]
fn heavy_jobs_are_refused_when_the_priority_is_not_lowered() {
    let t = TempDir::new("refuse");
    let (daemon, socket) = start(&t, false);
    let r = submit(
        &socket,
        JobParams::SpaceReport(SpaceReportParams::default()),
    );
    assert!(!r.ok);
    let e = r.error.unwrap();
    assert!(
        e.contains("refused") && e.contains("nice 0") && e.contains("io none"),
        "{e}"
    );
    // Light jobs still run, and status still answers.
    let id = submit(&socket, probe(0)).id.unwrap();
    assert_eq!(wait_for(&socket, &id, State::Done).state, State::Done);
    let ping = request(&socket, &Request::Ping).unwrap();
    assert!(!ping.priority.unwrap().lowered);
    daemon.stop();
}

#[test]
fn a_restart_fails_the_running_job_and_keeps_the_queue() {
    let t = TempDir::new("restart");
    let store = Store::new(t.path().join("state"));
    let mut running = Job::new("jrunning-1".into(), probe(1), 60);
    running.state = State::Running;
    store.save_job(&running.id, &running).unwrap();
    let queued = Job::new("jqueued-1".into(), probe(0), 60);
    store.save_job(&queued.id, &queued).unwrap();
    let (daemon, socket) = start(&t, true);
    let job = status(&socket, "jrunning-1");
    assert_eq!(job.state, State::Failed);
    assert!(job.error.unwrap().contains("stopped while"));
    assert_eq!(
        wait_for(&socket, "jqueued-1", State::Done).state,
        State::Done
    );
    daemon.stop();
}

#[test]
fn a_second_daemon_on_the_same_socket_is_refused() {
    let t = TempDir::new("twice");
    let (daemon, _socket) = start(&t, true);
    let home = t.path().to_path_buf();
    let second = Daemon::start(DaemonOptions {
        store: Store::new(t.path().join("state2")),
        socket: t.path().join("j.sock"),
        priority: priority(true),
        config: Arc::new(move || Ok(Config::defaults(&home, "testhost"))),
        throttle: None,
    });
    assert!(second.is_err());
    daemon.stop();
}
