//! `arca serve` reads `<stateDir>/alerts.json` before it listens. A malformed file stops the start with exit 2 and no socket.
//! The same config with a valid file listens, so the refusal is the file's and nothing else's. Every path is in a temporary
//! directory, and the Incus socket is absent: nothing answers it.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;

/// How long a start has to exit or listen before the test gives up.
const WAIT: Duration = Duration::from_secs(15);

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A directory under the default temporary directory, removed when the test ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("arca-alerts-file-{}-{n}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("the scratch directory is created");
        Scratch(dir)
    }

    fn at(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A directory that only its owner can read and write, as the state and token directories must be.
fn private_dir(path: &Path) {
    fs::create_dir_all(path).expect("the directory is created");
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("mode 0700");
}

/// A node config with its state, socket and token directories in `scratch`. Its Incus socket does not exist.
fn write_config(scratch: &Scratch) -> PathBuf {
    private_dir(&scratch.at("state"));
    private_dir(&scratch.at("tokens"));
    let value = json!({
        "stateDir": scratch.at("state").display().to_string(),
        "incus": { "socket": scratch.at("incus.sock").display().to_string(), "pools": [] },
        "identityPhones": [],
        "listen": { "unix": scratch.at("arca.sock").display().to_string(), "tailnet": null },
        "tokens": { "hashDir": scratch.at("tokens").display().to_string() },
        "kernel": { "projects": [], "scratch": false, "droiddProjects": [], "reservedCidrs": [] }
    });
    let path = scratch.at("node.json");
    fs::write(&path, value.to_string()).expect("write the config");
    path
}

fn start(config: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_arca"))
        .args(["serve", "--config"])
        .arg(config)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start arca serve")
}

/// Waits for `child` to exit, and kills it and fails the test when it does not exit within [`WAIT`].
fn exit_status(child: &mut Child) -> ExitStatus {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(status) = child.try_wait().expect("poll the start") {
            return status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("serve did not exit within the deadline");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// A server process, killed if the test ends before it is stopped.
struct Serve(Child);

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn a_malformed_alerts_file_stops_serve_with_exit_2_before_it_listens() {
    let scratch = Scratch::new("malformed");
    let config = write_config(&scratch);
    fs::write(
        scratch.at("state").join("alerts.json"),
        r#"{"rules":[{"id":"cpu-hot","bogusKey":1}]}"#,
    )
    .expect("write the malformed file");
    let mut child = start(&config);
    let status = exit_status(&mut child);
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        use std::io::Read as _;
        let _ = pipe.read_to_string(&mut stderr);
    }
    assert_eq!(status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("alerts.json"), "{stderr}");
    assert!(stderr.contains("bogusKey"), "the key is named: {stderr}");
    assert!(!scratch.at("arca.sock").exists(), "serve listened");
}

#[test]
fn a_valid_alerts_file_lets_serve_listen() {
    let scratch = Scratch::new("valid");
    let config = write_config(&scratch);
    fs::write(
        scratch.at("state").join("alerts.json"),
        r#"{"rules":[{"id":"disk-pool-full","threshold":85}]}"#,
    )
    .expect("write the file");
    let mut serve = Serve(start(&config));
    let deadline = Instant::now() + WAIT;
    loop {
        if UnixStream::connect(scratch.at("arca.sock")).is_ok() {
            break;
        }
        if let Some(status) = serve.0.try_wait().expect("poll the server") {
            panic!("serve exited before listening: {status}");
        }
        assert!(
            Instant::now() < deadline,
            "serve did not listen within the deadline"
        );
        thread::sleep(Duration::from_millis(20));
    }
}
