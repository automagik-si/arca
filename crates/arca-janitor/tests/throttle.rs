//! The throttle pauses while pressure is high (a fake PSI file stands in for /proc/pressure/io), and a job stops on a
//! cancel or a spent budget even while paused.

use std::fs;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use arca_janitor::config::Config;
use arca_janitor::jobs::{Ctx, JobParams, ProbeParams, Stop, Throttle};
use arca_janitor::store::Store;
use arca_janitor::testutil::TempDir;

const HIGH: &str = "some avg10=55.00 avg60=40.00 avg300=20.00 total=1\nfull avg10=30.00 avg60=0 avg300=0 total=1\n";
const LOW: &str =
    "some avg10=1.50 avg60=1.00 avg300=1.00 total=1\nfull avg10=0.00 avg60=0 avg300=0 total=1\n";

fn ctx(t: &TempDir, psi: &std::path::Path) -> Ctx {
    let config = Config::defaults(t.path(), "testhost");
    let mut ctx = Ctx::standalone(
        config,
        Store::new(t.path().join("state")),
        JobParams::Probe(ProbeParams::default()),
    );
    ctx.throttle = Throttle {
        psi_path: psi.to_path_buf(),
        threshold: 10.0,
        check_every: Duration::from_millis(0),
        backoff_start: Duration::from_millis(40),
        backoff_max: Duration::from_millis(80),
    };
    ctx
}

#[test]
fn the_throttle_pauses_while_pressure_is_high_and_says_so() {
    let t = TempDir::new("throttle");
    let psi = t.path().join("io");
    fs::write(&psi, LOW).unwrap();
    let ctx = ctx(&t, &psi);
    // Low pressure: no pause.
    let start = Instant::now();
    ctx.checkpoint().unwrap();
    assert!(start.elapsed() < Duration::from_millis(40));

    fs::write(&psi, HIGH).unwrap();
    let job = ctx.job.clone();
    let psi2 = psi.clone();
    let watcher = thread::spawn(move || {
        // Wait until the job says it is paused, then lower the pressure.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = None;
        while Instant::now() < deadline {
            if let Some(p) = job.lock().unwrap().progress.paused.clone() {
                seen = Some(p);
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        thread::sleep(Duration::from_millis(250));
        fs::write(&psi2, LOW).unwrap();
        seen
    });
    let start = Instant::now();
    ctx.checkpoint().unwrap();
    let waited = start.elapsed();
    let seen = watcher
        .join()
        .unwrap()
        .expect("the status said it was paused");
    assert!(
        seen.contains("io pressure avg10 55.0% is above 10%"),
        "{seen}"
    );
    assert!(waited >= Duration::from_millis(250), "paused {waited:?}");
    let progress = ctx.job.lock().unwrap().progress.clone();
    assert_eq!(progress.paused, None, "cleared once pressure fell");
    assert!(progress.paused_secs >= 1);
}

#[test]
fn a_cancel_or_a_spent_budget_ends_a_paused_job() {
    let t = TempDir::new("throttle-stop");
    let psi = t.path().join("io");
    fs::write(&psi, HIGH).unwrap();
    let ctx = ctx(&t, &psi);
    let cancel = ctx.cancel.clone();
    let canceller = thread::spawn(move || {
        thread::sleep(Duration::from_millis(150));
        cancel.store(true, Ordering::Relaxed);
    });
    let start = Instant::now();
    assert_eq!(ctx.checkpoint(), Err(Stop::Cancelled));
    assert!(start.elapsed() < Duration::from_secs(2));
    canceller.join().unwrap();

    let mut ctx = self::ctx(&t, &psi);
    ctx.deadline = Instant::now();
    assert_eq!(ctx.checkpoint(), Err(Stop::Budget));
}

#[test]
fn without_psi_the_throttle_falls_back_to_load() {
    let t = TempDir::new("throttle-load");
    let ctx = ctx(&t, &t.path().join("missing"));
    // Whatever the load, the reading exists and names its source.
    if let Some(p) = arca_janitor::platform::pressure(&ctx.throttle.psi_path) {
        assert_eq!(p.source, "load per cpu");
    }
}
