//! The node served end to end: `arca serve` in its own process, over a unix socket, reading a fake Incus through a recording
//! shim. A viewer token lists and calls the verbs it may; a doctor token gets 403 on `token_list`; a request with an `Origin`
//! or no bearer is refused; SIGTERM stops the server cleanly with its socket removed; and the shim's own log shows that every
//! request Arca sent was a GET. Tokens are minted by the binary itself, and no test prints a secret.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arca_api::{conforms, verb};
use arca_incus::{FakeIncus, FakeState, RecordingShim, ShimHandle};
use serde_json::{Value, json};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../arca-incus/tests/fixtures/state.json"
);
const WAIT: Duration = Duration::from_secs(15);

/// A fresh directory under the default temporary directory, removed when the test ends. The default is kept on purpose: a
/// long socket path breaks the unix socket's length limit.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .subsec_nanos();
        let dir =
            std::env::temp_dir().join(format!("arca-e2e-{}-{nanos}-{tag}", std::process::id()));
        fs::create_dir_all(&dir).expect("create the scratch directory");
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

/// A fake Incus serving the fixture, behind a recording shim.
struct Rig {
    fake: FakeIncus,
    shim: ShimHandle,
    log: PathBuf,
}

fn rig(scratch: &Scratch) -> Rig {
    let state = FakeState::load(Path::new(FIXTURE)).expect("load the fixture");
    let fake = FakeIncus::start(&scratch.0, state).expect("start the fake");
    let log = scratch.at("shim.log");
    let shim = RecordingShim::start(fake.socket(), &scratch.at("shim.sock"), &log)
        .expect("start the recording shim");
    Rig { fake, shim, log }
}

/// A server process, killed if the test ends before it is stopped.
struct Serve(Option<Child>);

impl Serve {
    fn child(&mut self) -> &mut Child {
        self.0.as_mut().expect("the server is running")
    }

    /// Sends SIGTERM and waits for the exit, which the test then checks.
    fn terminate(&mut self) -> ExitStatus {
        let child = self.child();
        let kill = Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status()
            .expect("send SIGTERM");
        assert!(kill.success(), "SIGTERM was delivered");
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = child.try_wait().expect("poll the server") {
                self.0 = None;
                return status;
            }
            if Instant::now() > deadline {
                panic!("the server did not exit after SIGTERM");
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn arca() -> Command {
    Command::new(env!("CARGO_BIN_EXE_arca"))
}

/// A node config that reads through `socket`, names one identity phone and keeps the kernel out of every project.
fn write_config(scratch: &Scratch, socket: &Path, hash_dir: &Path) -> PathBuf {
    // The state directory must be private to its owner before serve starts: serve refuses any other mode.
    let state = scratch.at("state");
    fs::create_dir_all(&state).expect("create the state directory");
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
    let value = json!({
        "stateDir": scratch.at("state").display().to_string(),
        "incus": { "socket": socket.display().to_string(), "pools": [] },
        "identityPhones": ["phone1"],
        "listen": { "unix": scratch.at("arca.sock").display().to_string(), "tailnet": null },
        "tokens": { "hashDir": hash_dir.display().to_string() },
        "kernel": { "projects": [], "scratch": false, "droiddProjects": ["droid"], "reservedCidrs": [] }
    });
    let path = scratch.at("node.json");
    fs::write(&path, value.to_string()).expect("write the config");
    path
}

/// A token directory at mode 0700 with one token of each scope, minted by the binary. Returns each secret by its id.
fn mint_tokens(scratch: &Scratch) -> (PathBuf, Vec<(String, String)>) {
    let hash_dir = scratch.at("tokens");
    fs::create_dir(&hash_dir).expect("create the token directory");
    fs::set_permissions(&hash_dir, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
    let mut secrets = Vec::new();
    for (scope, id) in [
        ("viewer", "viewer-1"),
        ("doctor", "doctor-1"),
        ("operator", "operator-1"),
    ] {
        let out_file = scratch.at(&format!("{id}.secret"));
        let out = arca()
            .args(["token", "mint", "--scope", scope, "--id", id, "--hash-dir"])
            .arg(&hash_dir)
            .arg("--out")
            .arg(&out_file)
            .output()
            .expect("run token mint");
        assert!(out.status.success(), "minting {id} succeeds");
        let secret = fs::read_to_string(&out_file)
            .expect("read the secret")
            .trim_end()
            .to_string();
        secrets.push((id.to_string(), secret));
    }
    (hash_dir, secrets)
}

fn secret_of<'a>(secrets: &'a [(String, String)], id: &str) -> &'a str {
    &secrets
        .iter()
        .find(|(name, _)| name == id)
        .expect("the token was minted")
        .1
}

fn start_serve(config: &Path) -> Serve {
    let child = arca()
        .args(["serve", "--config"])
        .arg(config)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start arca serve");
    Serve(Some(child))
}

/// Waits until the server's unix socket accepts a connection. A server that exits first is a failure with its stderr.
fn wait_for_server(serve: &mut Serve, socket: &Path) {
    let deadline = Instant::now() + WAIT;
    loop {
        if UnixStream::connect(socket).is_ok() {
            return;
        }
        if let Some(status) = serve.child().try_wait().expect("poll the server") {
            let mut stderr = String::new();
            if let Some(mut pipe) = serve.child().stderr.take() {
                let _ = pipe.read_to_string(&mut stderr);
            }
            panic!("the server exited before listening ({status}): {stderr}");
        }
        assert!(
            Instant::now() < deadline,
            "the server listens within the deadline"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// One HTTP/1.1 POST to `/mcp` over the unix socket, with extra headers. Returns the status and the body text.
fn post(socket: &Path, headers: &[(&str, String)], body: &str) -> (u16, String) {
    let mut stream = UnixStream::connect(socket).expect("connect to the server");
    stream.set_read_timeout(Some(WAIT)).expect("set a deadline");
    let mut request = format!(
        "POST /mcp HTTP/1.1\r\nHost: arca\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);
    stream
        .write_all(request.as_bytes())
        .expect("send the request");
    let mut reply = String::new();
    stream.read_to_string(&mut reply).expect("read the reply");
    let status = reply
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .expect("a status line");
    let text = reply
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_string())
        .unwrap_or_default();
    (status, text)
}

fn bearer(secret: &str) -> (&'static str, String) {
    ("Authorization", format!("Bearer {secret}"))
}

/// A JSON-RPC request body.
fn rpc(id: u64, method: &str, params: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string()
}

/// A `tools/call` body for `name` with `arguments`.
fn call(id: u64, name: &str, arguments: Value) -> String {
    rpc(
        id,
        "tools/call",
        json!({ "name": name, "arguments": arguments }),
    )
}

/// The structured content of a successful tool call, after checking the reply is a JSON-RPC result.
fn tool_result(text: &str) -> Value {
    let reply: Value = serde_json::from_str(text).expect("the reply is JSON");
    assert!(reply.get("error").is_none(), "the call succeeded: {reply}");
    reply["result"]["structuredContent"].clone()
}

#[test]
fn a_viewer_reads_the_watch_and_the_metrics_and_the_shim_saw_only_gets() {
    let scratch = Scratch::new("viewer");
    let rig = rig(&scratch);
    let (hash_dir, secrets) = mint_tokens(&scratch);
    let config = write_config(&scratch, rig.shim.socket(), &hash_dir);
    let socket = scratch.at("arca.sock");
    let mut serve = start_serve(&config);
    wait_for_server(&mut serve, &socket);
    let viewer = secret_of(&secrets, "viewer-1");

    // tools/list: a viewer sees the read verbs and not token_list.
    let (status, text) = post(&socket, &[bearer(viewer)], &rpc(1, "tools/list", json!({})));
    assert_eq!(status, 200);
    let listed: Value = serde_json::from_str(&text).expect("JSON");
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|t| t["name"].as_str().expect("a name"))
        .collect();
    for wanted in ["arca_status", "node_metrics", "watch_report"] {
        assert!(names.contains(&wanted), "a viewer lists {wanted}");
    }
    assert!(
        !names.contains(&"token_list"),
        "a viewer does not list token_list"
    );

    // watch_report: every instance is classed and phone1 carries the identity flag.
    let (status, text) = post(
        &socket,
        &[bearer(viewer)],
        &call(2, "watch_report", json!({})),
    );
    assert_eq!(status, 200);
    let report = tool_result(&text);
    conforming("watch_report", &report);
    let phone = report["projects"]
        .as_array()
        .expect("projects")
        .iter()
        .flat_map(|p| p["instances"].as_array().into_iter().flatten())
        .find(|row| row["name"] == json!("phone1"))
        .expect("phone1 is listed");
    assert_eq!(phone["identity"], json!(true));

    // node_metrics: the sampler takes its first sample on its own thread, so the first row is published a moment after the
    // socket listens. The call is repeated until it is, and no renter gate exists at any point.
    let deadline = Instant::now() + WAIT;
    let metrics = loop {
        let (status, text) = post(
            &socket,
            &[bearer(viewer)],
            &call(3, "node_metrics", json!({ "minutes": 1 })),
        );
        assert_eq!(status, 200);
        assert!(!text.contains("renterGate"), "no renter gate at any depth");
        let metrics = tool_result(&text);
        if !metrics["latest"].is_null() {
            break metrics;
        }
        assert!(
            Instant::now() < deadline,
            "the first sample is published within the deadline"
        );
        thread::sleep(Duration::from_millis(20));
    };
    conforming("node_metrics", &metrics);

    // arca_status: Incus answers through the shim.
    let (status, text) = post(
        &socket,
        &[bearer(viewer)],
        &call(4, "arca_status", json!({})),
    );
    assert_eq!(status, 200);
    let status_row = tool_result(&text);
    conforming("arca_status", &status_row);
    assert_eq!(status_row["incusReachable"], json!(true));
    assert_eq!(status_row["appliedSeq"], Value::Null);

    // Refusals: a doctor may not list tokens, no bearer is 401, a wrong bearer is 401 and an Origin is 403.
    let doctor = secret_of(&secrets, "doctor-1");
    let (status, _) = post(
        &socket,
        &[bearer(doctor)],
        &call(5, "token_list", json!({})),
    );
    assert_eq!(status, 403, "a doctor calling token_list is refused");
    let (status, _) = post(&socket, &[], &call(6, "arca_status", json!({})));
    assert_eq!(status, 401, "no bearer is 401");
    let unknown = format!("arca_{}", "A".repeat(43));
    let (status, _) = post(
        &socket,
        &[bearer(&unknown)],
        &call(7, "arca_status", json!({})),
    );
    assert_eq!(status, 401, "an unknown bearer is 401");
    let (status, _) = post(
        &socket,
        &[
            bearer(viewer),
            ("Origin", "https://example.invalid".to_string()),
        ],
        &call(8, "arca_status", json!({})),
    );
    assert_eq!(status, 403, "a request with an Origin is 403");

    // An operator lists the tokens by id and scope, and never a secret or a digest.
    let operator = secret_of(&secrets, "operator-1");
    let (status, text) = post(
        &socket,
        &[bearer(operator)],
        &call(9, "token_list", json!({})),
    );
    assert_eq!(status, 200);
    let listed_tokens = tool_result(&text);
    let tokens = listed_tokens["tokens"].as_array().expect("tokens");
    assert_eq!(tokens.len(), 3, "the three minted tokens are listed");
    assert!(
        tokens
            .iter()
            .any(|t| t["id"] == json!("doctor-1") && t["scope"] == json!("doctor"))
    );
    for (_, secret) in &secrets {
        assert!(
            !text.contains(secret.as_str()),
            "token_list never shows a secret"
        );
    }
    assert!(!text.contains("sha256"), "token_list never shows a digest");

    // SIGTERM stops the server with exit 0, and the socket is gone.
    let status = serve.terminate();
    assert_eq!(status.code(), Some(0), "the server exits 0 on SIGTERM");
    assert!(!socket.exists(), "the socket is removed on exit");

    let summary = rig.shim.stop().expect("stop the shim");
    assert_eq!(summary.refused, 0, "the shim refused nothing");
    assert!(summary.get > 0, "Arca read through the shim");
    let lines = read_log(&rig.log);
    for line in &lines {
        assert_eq!(
            line["method"],
            json!("GET"),
            "every request Arca sent is a GET: {line}"
        );
    }
    assert!(
        rig.fake.requests().iter().all(|r| r.method == "GET"),
        "the fake saw only GET"
    );
}

#[test]
fn a_state_directory_another_user_may_read_is_refused_at_start_and_no_node_key_is_made() {
    let scratch = Scratch::new("statedir");
    let rig = rig(&scratch);
    let socket = rig.shim.socket().to_path_buf();
    let (hash_dir, _) = mint_tokens(&scratch);
    let config = write_config(&scratch, &socket, &hash_dir);
    fs::set_permissions(scratch.at("state"), fs::Permissions::from_mode(0o755))
        .expect("chmod 0755");

    let out = arca()
        .args(["serve", "--config"])
        .arg(&config)
        .output()
        .expect("run serve");
    assert_eq!(
        out.status.code(),
        Some(2),
        "a state directory that is not mode 0700 is a config failure"
    );
    assert!(
        !scratch.at("state").join("node.key").exists(),
        "no node key is written into a directory another user may read"
    );
    let _ = rig.shim.stop();
}

#[test]
fn a_token_directory_that_is_unsafe_or_missing_exits_with_a_clear_code() {
    let scratch = Scratch::new("tokdir");
    let rig = rig(&scratch);
    let socket = rig.shim.socket().to_path_buf();

    // A directory writable by its group is refused with exit 2 and the mode to use.
    let wide = scratch.at("wide-tokens");
    fs::create_dir(&wide).expect("create the directory");
    fs::set_permissions(&wide, fs::Permissions::from_mode(0o775)).expect("chmod 0775");
    let config = write_config(&scratch, &socket, &wide);
    let out = arca()
        .args(["serve", "--config"])
        .arg(&config)
        .output()
        .expect("run serve");
    assert_eq!(
        out.status.code(),
        Some(2),
        "an unsafe token directory is a config failure"
    );
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        stderr.contains("0700"),
        "the message names the mode to use: {stderr}"
    );

    // A directory that is missing is refused with exit 2.
    let missing = scratch.at("no-such-tokens");
    let config = write_config(&scratch, &socket, &missing);
    let out = arca()
        .args(["serve", "--config"])
        .arg(&config)
        .output()
        .expect("run serve");
    assert_eq!(
        out.status.code(),
        Some(2),
        "a missing token directory is a config failure"
    );

    // A directory the server cannot read is an I/O failure, exit 1. A process that may read any directory cannot
    // reach this case, so the test then has nothing to check.
    let unreadable = scratch.at("unreadable-tokens");
    fs::create_dir(&unreadable).expect("create the directory");
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o300)).expect("chmod 0300");
    if fs::read_dir(&unreadable).is_err() {
        let config = write_config(&scratch, &socket, &unreadable);
        let out = arca()
            .args(["serve", "--config"])
            .arg(&config)
            .output()
            .expect("run serve");
        assert_eq!(
            out.status.code(),
            Some(1),
            "an unreadable token directory is an I/O failure"
        );
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(
            stderr.contains("cannot read the token directory"),
            "the message says so: {stderr}"
        );
    }
    // Restore the mode so that the scratch directory can be removed.
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o700)).expect("chmod 0700");

    let _ = rig.shim.stop();
}

#[test]
fn a_hanging_incus_does_not_hold_up_serve_or_node_metrics() {
    // The Incus socket accepts a connection and never answers. `serve` must listen at once, and `node_metrics` must answer
    // from the samples it has, which is none yet, so no call waits on Incus.
    let scratch = Scratch::new("hang");
    let hung_path = scratch.at("hung-incus.sock");
    let hung = UnixListener::bind(&hung_path).expect("bind the hanging socket");
    thread::spawn(move || {
        let mut held = Vec::new();
        for stream in hung.incoming().flatten() {
            held.push(stream);
        }
    });
    let (hash_dir, secrets) = mint_tokens(&scratch);
    let config = write_config(&scratch, &hung_path, &hash_dir);
    let socket = scratch.at("arca.sock");

    let started = Instant::now();
    let mut serve = start_serve(&config);
    wait_for_server(&mut serve, &socket);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "serve listens without waiting on Incus: {:?}",
        started.elapsed()
    );

    let viewer = secret_of(&secrets, "viewer-1");
    let asked = Instant::now();
    let (status, text) = post(
        &socket,
        &[bearer(viewer)],
        &call(1, "node_metrics", json!({ "minutes": 1 })),
    );
    assert!(
        asked.elapsed() < Duration::from_secs(2),
        "node_metrics answers at once: {:?}",
        asked.elapsed()
    );
    assert_eq!(status, 200);
    let metrics = tool_result(&text);
    conforming("node_metrics", &metrics);
    assert!(metrics["latest"].is_null(), "no sample exists yet");
    assert!(metrics["mean"].is_null(), "no mean exists yet");

    assert_eq!(serve.terminate().code(), Some(0), "the server exits 0");
}

fn read_log(log: &Path) -> Vec<Value> {
    fs::read_to_string(log)
        .expect("read the shim log")
        .lines()
        .map(|line| serde_json::from_str(line).expect("a log line is JSON"))
        .collect()
}

fn conforming(verb_name: &str, value: &Value) {
    let verb = verb(verb_name).expect("the verb is in the table");
    if let Err(why) = conforms(&(verb.result)(), value) {
        panic!("{verb_name} does not conform: {why}\n{value}");
    }
}
