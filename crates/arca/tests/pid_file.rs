//! A failed second start preserves the live server's pid and never publishes a pid for another process's listener.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;

const WAIT: Duration = Duration::from_secs(15);

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("arca-pid-file-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        private_dir(&dir);
        Self(dir)
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

fn private_dir(path: &Path) {
    fs::create_dir_all(path).expect("create directory");
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("mode 0700");
}

fn write_config(scratch: &Scratch, name: &str, socket: &Path) -> (PathBuf, PathBuf) {
    let state = scratch.at(name);
    let tokens = state.join("tokens");
    private_dir(&state);
    private_dir(&tokens);
    let config = state.join("node.json");
    fs::write(&config, json!({
        "stateDir": state,
        "incus": { "socket": scratch.at("absent-incus.sock"), "pools": [] },
        "identityPhones": [],
        "listen": { "unix": socket, "tailnet": null },
        "tokens": { "hashDir": tokens },
        "kernel": { "projects": [], "scratch": false, "droiddProjects": [], "reservedCidrs": [] }
    }).to_string()).expect("write config");
    (config, state.join("serve.pid"))
}

struct Serve(Child);

impl Serve {
    fn start(config: &Path) -> Self {
        Self(
            Command::new(env!("CARGO_BIN_EXE_arca"))
                .args(["serve", "--config"])
                .arg(config)
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .expect("start serve"),
        )
    }

    fn wait_for_pid(&mut self, socket: &Path, pid: &Path) -> String {
        let expected = format!("{}\n", self.0.id());
        let deadline = Instant::now() + WAIT;
        loop {
            if fs::read_to_string(pid).ok().as_ref() == Some(&expected)
                && UnixStream::connect(socket).is_ok()
            {
                return expected;
            }
            assert!(
                self.0.try_wait().expect("poll serve").is_none(),
                "serve exited before publishing its pid"
            );
            assert!(
                Instant::now() < deadline,
                "serve did not listen and publish its pid"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn exit(&mut self) -> (ExitStatus, String) {
        let deadline = Instant::now() + WAIT;
        let status = loop {
            if let Some(status) = self.0.try_wait().expect("poll serve") {
                break status;
            }
            assert!(Instant::now() < deadline, "second serve did not exit");
            thread::sleep(Duration::from_millis(20));
        };
        let mut stderr = String::new();
        self.0
            .stderr
            .take()
            .expect("stderr")
            .read_to_string(&mut stderr)
            .expect("read stderr");
        (status, stderr)
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn assert_unauthorized(socket: &Path) {
    let mut stream =
        UnixStream::connect(socket).expect("the first server still accepts connections");
    stream.set_read_timeout(Some(WAIT)).expect("read timeout");
    stream.set_write_timeout(Some(WAIT)).expect("write timeout");
    stream
        .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .expect("GET without token");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("HTTP response");
    assert!(response.starts_with("HTTP/1.1 401"), "{response}");
}

#[test]
fn a_second_start_on_the_same_state_preserves_the_first_servers_pid() {
    let scratch = Scratch::new("same-state");
    let socket = scratch.at("arca.sock");
    let (config, pid) = write_config(&scratch, "state", &socket);
    let mut first = Serve::start(&config);
    let expected = first.wait_for_pid(&socket, &pid);
    let (status, stderr) = Serve::start(&config).exit();
    assert!(!status.success(), "{stderr}");
    assert_eq!(
        fs::read_to_string(&pid).expect("first pid remains"),
        expected
    );
    assert_unauthorized(&socket);
}

#[test]
fn another_process_listener_does_not_overwrite_a_pid_file() {
    let scratch = Scratch::new("other-peer");
    let socket = scratch.at("arca.sock");
    let (config, pid) = write_config(&scratch, "first", &socket);
    let mut first = Serve::start(&config);
    let expected = first.wait_for_pid(&socket, &pid);
    let (second_config, second_pid) = write_config(&scratch, "second", &socket);
    // A sentinel catches even a transient write: publishing the second pid would make cleanup delete it.
    fs::write(&second_pid, "sentinel\n").expect("sentinel pid file");
    fs::set_permissions(&second_pid, fs::Permissions::from_mode(0o600)).expect("mode 0600");
    let (status, stderr) = Serve::start(&second_config).exit();
    assert!(!status.success(), "{stderr}");
    assert!(
        stderr.contains("a server already listens on the socket path"),
        "{stderr}"
    );
    assert_eq!(
        fs::read_to_string(&second_pid).expect("sentinel remains"),
        "sentinel\n"
    );
    assert_eq!(
        fs::read_to_string(&pid).expect("first pid remains"),
        expected
    );
    assert_unauthorized(&socket);
}
