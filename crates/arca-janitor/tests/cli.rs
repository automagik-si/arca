//! The real binary: `serve --foreground` lowers itself and reads its priority back, and the CLI never blocks: submit
//! prints an id at once, wait returns at its timeout.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};

use arca_janitor::testutil::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_arca-janitor");

struct Served {
    child: Child,
    state: PathBuf,
    socket: PathBuf,
    home: PathBuf,
}

impl Served {
    fn start(t: &TempDir) -> Served {
        let state = t.path().join("state");
        let socket = t.path().join("j.sock");
        let home = t.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let child = Command::new(BIN)
            .args(["serve", "--foreground"])
            .env("JANITOR_STATE_DIR", &state)
            .env("JANITOR_SOCKET", &socket)
            .env("JANITOR_CONFIG_DIR", t.path().join("conf"))
            .env("HOME", &home)
            .env_remove(arca_janitor::platform::REEXEC_GUARD)
            .spawn()
            .unwrap();
        let s = Served {
            child,
            state,
            socket,
            home,
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if s.run(&["list"]).status.success() {
                return s;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("the daemon did not answer");
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .args(args)
            .env("JANITOR_STATE_DIR", &self.state)
            .env("JANITOR_SOCKET", &self.socket)
            .env("HOME", &self.home)
            .output()
            .unwrap()
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.run(&["shutdown"]);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

#[cfg(target_os = "linux")]
fn ionice_of(pid: u32) -> Option<String> {
    let out = Command::new("ionice")
        .args(["-p", &pid.to_string()])
        .output()
        .ok()?;
    arca_janitor::platform::parse_ionice(&String::from_utf8_lossy(&out.stdout))
}

#[test]
#[cfg(target_os = "linux")]
fn the_daemon_runs_at_nice_19_with_idle_io_and_says_so() {
    let t = TempDir::new("cli-prio");
    let s = Served::start(&t);
    let pid = s.child.id();
    // Read back from outside: the re-exec keeps the pid, so this is the daemon itself.
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    assert_eq!(arca_janitor::platform::parse_stat_nice(&stat), Some(19));
    assert_eq!(ionice_of(pid).as_deref(), Some("idle"));
    // And from inside: a job records the priority the daemon read back at start.
    let id = stdout(&s.run(&["submit", "probe"]));
    assert_eq!(
        s.run(&["wait", &id, "--timeout", "10"]).status.code(),
        Some(0)
    );
    let result = stdout(&s.run(&["result", &id, "--json"]));
    let v: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(v["priority"]["nice"], 19);
    assert_eq!(v["priority"]["io"], "idle");
    assert_eq!(v["priority"]["lowered"], true);
}

#[test]
fn submit_returns_at_once_and_wait_stops_at_its_timeout() {
    let t = TempDir::new("cli-wait");
    let s = Served::start(&t);
    let start = Instant::now();
    let out = s.run(&["submit", "probe", "--seconds", "20"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let id = stdout(&out);
    assert!(id.starts_with('j'), "{id}");
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "submit took {:?}",
        start.elapsed()
    );

    let start = Instant::now();
    let out = s.run(&["wait", &id, "--timeout", "0.5"]);
    assert_eq!(out.status.code(), Some(3), "{}", stdout(&out));
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "wait took {:?}",
        start.elapsed()
    );
    assert!(stdout(&out).contains("still running"));

    let out = s.run(&["status", &id]);
    assert!(stdout(&out).contains("running"), "{}", stdout(&out));
    assert!(s.run(&["cancel", &id]).status.success());
    assert_eq!(
        s.run(&["wait", &id, "--timeout", "10"]).status.code(),
        Some(1)
    );
    let out = s.run(&["status", &id, "--json"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(v["state"], "cancelled");
}

#[test]
fn bad_input_is_a_usage_error_and_schema_needs_no_daemon() {
    let t = TempDir::new("cli-usage");
    let socket = t.path().join("none.sock");
    let run = |args: &[&str]| {
        Command::new(BIN)
            .args(args)
            .env("JANITOR_SOCKET", &socket)
            .env("JANITOR_STATE_DIR", t.path())
            .output()
            .unwrap()
    };
    assert_eq!(run(&["submit", "nonsense"]).status.code(), Some(2));
    let out = run(&["submit", "probe"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no janitor daemon"));
    let out = run(&["schema", "clean-plan"]);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert!(v["params"]["properties"]["profile"].is_object());
    let out = run(&["report", "--since", "1d"]);
    assert!(stdout(&out).contains("no clean runs"));
    assert!(Path::new(BIN).exists());
}
