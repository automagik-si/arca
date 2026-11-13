//! The root part: reads the declared file and the host, finds the drift, applies the declared state when asked, and writes
//! the status file. [`run`] is what the system unit runs.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::Paths;
use crate::declared::{self, DeclaredError};
use crate::plan::{self, Drift};
use crate::runner::Runner;
use crate::socket;
use crate::status::{self, DriftItem, Status};

/// The two things the socket may ask of the root part, and nothing else: a fresh reading, and putting back what the
/// declared file says.
pub trait Ops {
    /// Reads the host against the declared file and changes nothing.
    fn read(&self) -> Status;
    /// Puts back what the declared file says, then reads the host again. Writes the status file.
    fn apply(&self) -> Status;
}

/// The root part over one host.
pub struct Host<R> {
    runner: R,
    paths: Paths,
    trusted_owner: Option<u32>,
    clock: fn() -> u64,
    serial: Mutex<()>,
}

/// The time now, in epoch milliseconds.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

impl<R: Runner> Host<R> {
    /// A root part over `runner`. `trusted_owner` is the user who must own the declared file: root, on a real host.
    pub fn new(runner: R, paths: Paths, trusted_owner: Option<u32>) -> Host<R> {
        Host {
            runner,
            paths,
            trusted_owner,
            clock: now_ms,
            serial: Mutex::new(()),
        }
    }

    /// The same root part with another clock, for a test.
    pub fn with_clock(mut self, clock: fn() -> u64) -> Host<R> {
        self.clock = clock;
        self
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    /// One reading, and when `apply` is set one apply first. Reads and applies never overlap.
    fn evaluate(&self, apply: bool) -> Status {
        let _serial = self
            .serial
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut status = Status {
            at: (self.clock)(),
            declared: false,
            in_sync: false,
            drift: Vec::new(),
            applied: Vec::new(),
            errors: Vec::new(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        };
        let declared = match declared::load(&self.paths.declared, self.trusted_owner) {
            Ok(declared) => declared,
            Err(DeclaredError::Missing) => {
                status.errors.push(DeclaredError::Missing.to_string());
                return status;
            }
            Err(error) => {
                status.drift.push(DriftItem {
                    item: "declared-file".to_string(),
                    detail: error.to_string(),
                });
                return status;
            }
        };
        status.declared = true;
        let mut drifts = plan::drift(&declared, &plan::observe(&self.runner));
        if apply && !drifts.is_empty() {
            for command in plan::plan(&declared, &drifts) {
                status.applied.push(command.line());
                match self.runner.run(&command) {
                    Ok(output) if output.code == 0 => {}
                    Ok(output) => status.errors.push(format!(
                        "{} exited {}: {}",
                        command.line(),
                        output.code,
                        output.stderr.trim().chars().take(200).collect::<String>()
                    )),
                    Err(why) => status.errors.push(format!("{}: {why}", command.line())),
                }
            }
            drifts = plan::drift(&declared, &plan::observe(&self.runner));
        }
        status.drift = drifts.iter().map(Drift::as_item).collect();
        status.in_sync = status.drift.is_empty() && status.errors.is_empty();
        status
    }

    /// What the unit does at boot: puts back the declared state and writes the status file.
    pub fn boot(&self) -> Status {
        let status = self.evaluate(true);
        self.publish(&status);
        status
    }

    /// What the unit does every minute: reads the host and writes the status file, and changes nothing.
    pub fn reread(&self) -> Status {
        let status = self.evaluate(false);
        self.publish(&status);
        status
    }

    fn publish(&self, status: &Status) {
        if let Err(error) = status::write(&self.paths.status, status) {
            eprintln!("arca host-network: the status file could not be written: {error}");
        }
    }
}

impl<R: Runner> Ops for Host<R> {
    fn read(&self) -> Status {
        self.evaluate(false)
    }

    fn apply(&self) -> Status {
        let status = self.evaluate(true);
        self.publish(&status);
        status
    }
}

/// What the system unit runs: puts back the declared state, listens on the socket, and reads the host again every
/// `interval` until `stop` is set. Returns when `stop` is set, or at once when the socket cannot be bound.
pub fn run<R: Runner + Send + Sync + 'static>(
    host: Arc<Host<R>>,
    interval: Duration,
    stop: Arc<AtomicBool>,
) -> io::Result<()> {
    // The declared state is put back first: a socket that cannot be bound must not leave the host without its rules.
    host.boot();
    let listener = socket::bind(&host.paths.socket)?;
    let server = {
        let (ops, stop) = (
            Arc::clone(&host) as Arc<dyn Ops + Send + Sync>,
            Arc::clone(&stop),
        );
        thread::spawn(move || socket::serve(&listener, ops, &stop))
    };
    let mut waited = Duration::ZERO;
    let tick = Duration::from_secs(1).min(interval);
    while !stop.load(Ordering::SeqCst) {
        thread::sleep(tick);
        waited += tick;
        if waited >= interval {
            waited = Duration::ZERO;
            host.reread();
        }
    }
    let _ = server.join();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeHost;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let dir = std::env::temp_dir()
                .join(format!("arca-hostnet-root-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("scratch");
            Scratch(dir)
        }

        fn paths(&self) -> Paths {
            Paths {
                declared: self.0.join("host.json"),
                status: self.0.join("status.json"),
                socket: self.0.join("host.sock"),
                unit_file: self.0.join("unit.service"),
                installed_binary: self.0.join("bin/arca"),
            }
        }

        fn declare(&self, text: &str) {
            std::fs::write(self.0.join("host.json"), text).expect("declared");
            std::fs::set_permissions(
                self.0.join("host.json"),
                std::fs::Permissions::from_mode(0o644),
            )
            .expect("mode");
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const DECLARED: &str = r#"{"forwarding":true,"bridges":["incusbr0"],"forwardPolicy":"DROP"}"#;

    fn host(scratch: &Scratch, fake: FakeHost) -> Host<FakeHost> {
        Host::new(fake, scratch.paths(), None).with_clock(|| 5_000)
    }

    fn items(status: &Status) -> Vec<&str> {
        status.drift.iter().map(|d| d.item.as_str()).collect()
    }

    #[test]
    fn a_host_that_lost_its_forwarding_and_rules_at_boot_is_put_back() {
        let scratch = Scratch::new("boot");
        scratch.declare(DECLARED);
        // The state after a reboot with Docker 29: forwarding off, Docker's chain up, nothing accepted.
        let root = host(&scratch, FakeHost::new().docker().policy("DROP"));
        let status = root.boot();
        assert!(status.declared && status.in_sync, "{status:?}");
        assert!(status.drift.is_empty() && status.errors.is_empty());
        assert_eq!(status.at, 5_000);
        assert_eq!(
            status.applied,
            [
                "sysctl -w net.ipv4.ip_forward=1",
                "sysctl -w net.ipv6.conf.all.forwarding=1",
                "iptables -w 5 -I DOCKER-USER -i incusbr0 -j ACCEPT",
                "iptables -w 5 -I DOCKER-USER -o incusbr0 -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT",
            ]
        );
        // The status file holds what the call returned.
        assert_eq!(status::read(&scratch.paths().status), Some(status));
    }

    #[test]
    fn applying_again_adds_nothing_twice() {
        let scratch = Scratch::new("idempotent");
        scratch.declare(DECLARED);
        let root = host(&scratch, FakeHost::new().docker());
        root.boot();
        let rules_after_first = root.runner.docker_user();
        let changes_after_first = root.runner.changes().len();
        let again = root.apply();
        assert!(again.in_sync && again.applied.is_empty(), "{again:?}");
        assert_eq!(root.runner.docker_user(), rules_after_first);
        assert_eq!(
            root.runner.changes().len(),
            changes_after_first,
            "the second apply ran no change"
        );
        // Docker's own last rule is still last, and each accept is there once.
        assert_eq!(rules_after_first.len(), 3);
        assert_eq!(
            rules_after_first.last().map(String::as_str),
            Some("-j RETURN")
        );
    }

    #[test]
    fn a_hand_made_fix_is_adopted_not_doubled() {
        let scratch = Scratch::new("adopt");
        scratch.declare(DECLARED);
        let root = host(
            &scratch,
            FakeHost::new()
                .forwarding(true, true)
                .docker()
                .hand_fix("incusbr0"),
        );
        let status = root.boot();
        assert!(status.in_sync && status.applied.is_empty());
        assert!(root.runner.changes().is_empty());
        assert_eq!(root.runner.docker_user().len(), 3);
    }

    #[test]
    fn the_minute_reread_reports_drift_and_changes_nothing() {
        let scratch = Scratch::new("reread");
        scratch.declare(DECLARED);
        let root = host(
            &scratch,
            FakeHost::new()
                .forwarding(true, false)
                .docker()
                .hand_fix("incusbr0")
                .policy("ACCEPT"),
        );
        let status = root.reread();
        assert!(status.declared && !status.in_sync);
        assert_eq!(items(&status), ["ipv6-forwarding", "forward-policy"]);
        assert!(status.drift[0].detail.contains("IPv6 forwarding is off"));
        assert!(status.applied.is_empty());
        assert!(root.runner.changes().is_empty(), "a reread changes nothing");
        assert_eq!(status::read(&scratch.paths().status), Some(status));
        // The socket's read gives the same answer and leaves the status file alone.
        let live = root.read();
        assert_eq!(items(&live), ["ipv6-forwarding", "forward-policy"]);
        assert!(root.runner.changes().is_empty());
    }

    #[test]
    fn a_command_that_fails_is_reported_and_the_rest_still_run() {
        let scratch = Scratch::new("failing");
        scratch.declare(DECLARED);
        let root = host(&scratch, FakeHost::new().docker().failing("-w net.ipv6"));
        let status = root.boot();
        assert!(!status.in_sync);
        assert_eq!(items(&status), ["ipv6-forwarding"]);
        assert_eq!(status.errors.len(), 1);
        assert!(
            status.errors[0].contains("sysctl -w net.ipv6.conf.all.forwarding=1"),
            "{:?}",
            status.errors
        );
        assert_eq!(
            root.runner.sysctl("net.ipv4.ip_forward").as_deref(),
            Some("1")
        );
        assert_eq!(
            root.runner.docker_user().len(),
            3,
            "the rules went in after the failed sysctl"
        );
    }

    #[test]
    fn no_declared_file_and_a_bad_one_are_told_apart_and_touch_nothing() {
        let scratch = Scratch::new("undeclared");
        let root = host(&scratch, FakeHost::new().docker());
        let none = root.boot();
        assert!(
            !none.declared && !none.in_sync && none.drift.is_empty(),
            "{none:?}"
        );
        assert_eq!(none.errors, ["there is no declared state"]);
        scratch.declare(r#"{"forwarding":true,"bridges":["a b"]}"#);
        let bad = root.boot();
        assert!(!bad.declared && !bad.in_sync);
        assert_eq!(items(&bad), ["declared-file"]);
        assert!(root.runner.changes().is_empty());
    }

    #[test]
    fn a_socket_that_cannot_be_bound_does_not_stop_the_start_apply() {
        let scratch = Scratch::new("nobind");
        scratch.declare(DECLARED);
        let mut paths = scratch.paths();
        paths.socket = scratch.0.join("missing-directory/host.sock");
        let root = Arc::new(Host::new(FakeHost::new().docker(), paths, None).with_clock(|| 5_000));
        let stop = Arc::new(AtomicBool::new(false));
        assert!(run(Arc::clone(&root), Duration::from_millis(20), stop).is_err());
        assert_eq!(
            root.runner.docker_user().len(),
            3,
            "the rules went in before the socket was tried"
        );
        assert_eq!(
            status::read(&scratch.paths().status).map(|s| s.in_sync),
            Some(true)
        );
    }

    #[test]
    fn the_unit_applies_at_boot_listens_and_rereads_until_stopped() {
        let scratch = Scratch::new("run");
        scratch.declare(DECLARED);
        let root = Arc::new(host(&scratch, FakeHost::new().docker()));
        let stop = Arc::new(AtomicBool::new(false));
        let runner = {
            let (root, stop) = (Arc::clone(&root), Arc::clone(&stop));
            thread::spawn(move || run(root, Duration::from_millis(20), stop))
        };
        let socket = scratch.paths().socket;
        let mut answered = None;
        for _ in 0..200 {
            if let Ok(status) = crate::client::read(&socket) {
                answered = Some(status);
                break;
            }
            thread::sleep(Duration::from_millis(25));
        }
        let answered = answered.expect("the socket answers");
        assert!(answered.in_sync, "{answered:?}");
        assert_eq!(
            status::read(&scratch.paths().status).map(|s| s.in_sync),
            Some(true)
        );
        // The socket is open to every user, because its verbs can only put back what root's own file says.
        let mode = std::fs::metadata(&socket)
            .expect("socket")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o666);
        stop.store(true, Ordering::SeqCst);
        runner.join().expect("joins").expect("ran");
        assert_eq!(root.runner.docker_user().len(), 3);
    }
}
