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

/// The binary, with no home directory and no variable to take a config or a token from: a call that names neither must
/// never read the files of the user who runs the tests.
fn arca() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_arca"));
    command
        .env("HOME", "/nonexistent/arca-test-home")
        .env_remove("ARCA_CONFIG")
        .env_remove("ARCA_TOKEN_FILE");
    command
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
    assert_eq!(status_row["mode"], json!("watch-only"));
    // A node that only looks says so in every answer, in the structured content and in the text.
    let only_looks = "this node only looks: nothing here can be changed until the node is running with changes turned on (arca init --enable-changes, then restart the node if init did not)";
    assert_eq!(status_row["watchOnly"], json!(only_looks));
    assert!(text.contains(only_looks), "{text}");
    assert_eq!(
        metrics["watchOnly"],
        json!(only_looks),
        "every answer, not one verb's"
    );
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
fn a_node_with_kernel_projects_reports_kernel_mode_before_any_document_applies() {
    let scratch = Scratch::new("kernel-status");
    let rig = rig(&scratch);
    let (hash_dir, secrets) = mint_tokens(&scratch);
    let config = write_config(&scratch, rig.fake.socket(), &hash_dir);
    let mut cfg: Value =
        serde_json::from_slice(&fs::read(&config).expect("config")).expect("config JSON");
    cfg["kernel"]["projects"] = json!([arca_netproject::PROJECT]);
    fs::write(&config, cfg.to_string()).expect("write kernel config");
    let socket = scratch.at("arca.sock");
    let mut serve = start_serve(&config);
    wait_for_server(&mut serve, &socket);

    let (status, text) = post(
        &socket,
        &[bearer(secret_of(&secrets, "doctor-1"))],
        &call(1, "arca_status", json!({})),
    );
    assert_eq!(status, 200);
    let row = tool_result(&text);
    conforming("arca_status", &row);
    assert_eq!(row["mode"], json!("kernel"));
    assert!(
        row.get("watchOnly").is_none() && !text.contains("only looks"),
        "a node that makes changes says nothing of the kind: {text}"
    );
    assert_eq!(row["incusReachable"], json!(true));
    assert_eq!(row["appliedSeq"], Value::Null);
    assert_eq!(serve.terminate().code(), Some(0));
}

/// `snapshot_list` as `arca serve` wires it: a call that names a tenant is answered for the tenant first. Without that
/// wiring the call would list nothing and succeed.
#[test]
fn the_served_snapshot_list_answers_for_its_tenant_as_every_verb_does() {
    for (tag, tenants, expected) in [
        (
            "snapshot-tenant-none",
            Value::Null,
            "this node has no tenants",
        ),
        (
            "snapshot-tenant-unknown",
            json!({
                "capacity": { "instances": 8, "cpu": 8, "memory": "16GiB", "disk": "100GiB" },
                "maxTenants": 2,
                "defaultCaps": { "instances": 2, "containers": 2, "virtualMachines": 0, "cpu": 2,
                    "memory": "4GiB", "disk": "40GiB" },
                "createRate": { "count": 5, "perSeconds": 3600 }
            }),
            "tenant abcdefgh: not found",
        ),
    ] {
        let scratch = Scratch::new(tag);
        let rig = rig(&scratch);
        let (hash_dir, secrets) = mint_tokens(&scratch);
        let config = write_config(&scratch, rig.fake.socket(), &hash_dir);
        let mut cfg: Value =
            serde_json::from_slice(&fs::read(&config).expect("config")).expect("config JSON");
        cfg["kernel"]["projects"] = json!([arca_netproject::PROJECT]);
        cfg["kernel"]["serviceCidr"] = json!("192.0.2.0/24");
        if !tenants.is_null() {
            cfg["kernel"]["tenants"] = tenants;
        }
        fs::write(&config, cfg.to_string()).expect("write kernel config");
        let socket = scratch.at("arca.sock");
        let mut serve = start_serve(&config);
        wait_for_server(&mut serve, &socket);
        let viewer = bearer(secret_of(&secrets, "viewer-1"));
        let (status, text) = post(
            &socket,
            &[viewer.clone()],
            &call(1, "snapshot_list", json!({ "tenant": "abcdefgh" })),
        );
        assert_eq!(status, 200);
        assert!(text.contains(expected), "{tag}: {text}");
        assert!(
            !text.contains("\"snapshots\""),
            "{tag}: nothing is listed: {text}"
        );
        // Without a tenant the same handler lists as before.
        let (status, text) = post(&socket, &[viewer], &call(2, "snapshot_list", json!({})));
        assert_eq!(status, 200);
        assert_eq!(tool_result(&text)["snapshots"], json!([]), "{tag}");
        assert_eq!(serve.terminate().code(), Some(0));
    }
}

#[test]
fn kernel_status_reports_the_applied_sequence_after_service_create() {
    status_after_service_create(false);
}

#[test]
fn kernel_status_reloads_the_applied_sequence_after_restart() {
    status_after_service_create(true);
}

fn status_after_service_create(restart: bool) {
    let scratch = Scratch::new("applied-status");
    let rig = rig(&scratch);
    rig.fake.add_profile(
        "default",
        json!({
            "name": "default", "config": {},
            "devices": {"root": {"type": "disk", "pool": "default", "path": "/"}}
        }),
    );
    let (hash_dir, secrets) = mint_tokens(&scratch);
    let config = write_config(&scratch, rig.fake.socket(), &hash_dir);
    let mut cfg: Value =
        serde_json::from_slice(&fs::read(&config).expect("config")).expect("config JSON");
    cfg["kernel"]["projects"] = json!([arca_netproject::PROJECT]);
    cfg["kernel"]["serviceCidr"] = json!("192.0.2.0/24");
    fs::write(&config, cfg.to_string()).expect("write kernel config");
    let templates = scratch.at("state/templates");
    fs::create_dir(&templates).expect("templates");
    fs::write(
        templates.join("nats.json"),
        json!({
            "name": "nats", "kind": "oci", "image": "docker.io/library/nats",
            "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "ports": [4222], "env": {}, "publishable": false
        })
        .to_string(),
    )
    .expect("template");
    let socket = scratch.at("arca.sock");
    let mut serve = start_serve(&config);
    wait_for_server(&mut serve, &socket);
    let doctor = secret_of(&secrets, "doctor-1");
    let (status, text) = post(
        &socket,
        &[bearer(doctor)],
        &call(
            1,
            "service_create",
            json!({"name":"nats", "template":"nats"}),
        ),
    );
    assert_eq!(status, 200);
    assert_eq!(tool_result(&text)["state"], "running");
    let desired: Value =
        serde_json::from_slice(&fs::read(scratch.at("state/desired.json")).expect("desired state"))
            .expect("desired JSON");
    let seq = desired["current"]["document"]["seq"]
        .as_u64()
        .expect("document sequence");
    assert!(seq >= 1);

    if restart {
        assert_eq!(serve.terminate().code(), Some(0));
        serve = start_serve(&config);
        wait_for_server(&mut serve, &socket);
    }
    let (status, text) = post(
        &socket,
        &[bearer(doctor)],
        &call(2, "arca_status", json!({})),
    );
    assert_eq!(status, 200);
    let row = tool_result(&text);
    conforming("arca_status", &row);
    assert_eq!(row["mode"], "kernel");
    assert_eq!(row["appliedSeq"], seq);
    assert_eq!(serve.terminate().code(), Some(0));
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

/// The tool names a doctor lists on `socket`, and the reply to its `action_approve` call.
fn doctor_sees_action_approve(socket: &Path, doctor: &str) -> (bool, Value) {
    let (status, text) = post(socket, &[bearer(doctor)], &rpc(1, "tools/list", json!({})));
    assert_eq!(status, 200);
    let listed: Value = serde_json::from_str(&text).expect("JSON");
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|t| t["name"].as_str().expect("a name"))
        .collect();
    assert!(
        names.contains(&"plan_list"),
        "the doctor lists the other verbs"
    );
    let (status, text) = post(
        socket,
        &[bearer(doctor)],
        &call(
            2,
            "action_approve",
            json!({ "plan": "act-000000000000", "approval": "a.b" }),
        ),
    );
    assert_eq!(status, 200);
    (
        names.contains(&"action_approve"),
        serde_json::from_str(&text).expect("JSON"),
    )
}

#[test]
fn action_approve_is_served_only_to_a_node_that_trusts_another_kernels_passkeys() {
    // No approval key: the verb is neither listed nor answered, as before it existed.
    let scratch = Scratch::new("relay-off");
    let off = rig(&scratch);
    let (hash_dir, secrets) = mint_tokens(&scratch);
    let config = write_config(&scratch, off.shim.socket(), &hash_dir);
    let socket = scratch.at("arca.sock");
    let mut serve = start_serve(&config);
    wait_for_server(&mut serve, &socket);
    let doctor = secret_of(&secrets, "doctor-1");
    let (listed, reply) = doctor_sees_action_approve(&socket, doctor);
    assert!(!listed, "action_approve is not listed");
    assert_eq!(reply["error"]["message"], json!("not_found"), "{reply}");
    let (_, text) = post(
        &socket,
        &[bearer(doctor)],
        &call(3, "arca_status", json!({})),
    );
    assert!(tool_result(&text).get("approval").is_none());
    assert_eq!(serve.terminate().code(), Some(0));

    // An approval key that trusts another kernel's passkeys file: the verb is listed and reaches its handler, and
    // arca_status names the file by its index and file name only.
    let scratch = Scratch::new("relay-on");
    let on = rig(&scratch);
    let (hash_dir, secrets) = mint_tokens(&scratch);
    let config = write_config(&scratch, on.shim.socket(), &hash_dir);
    let trusted = scratch.at("other-kernel").join("owner-passkeys");
    let mut cfg: Value =
        serde_json::from_slice(&fs::read(&config).expect("config")).expect("config JSON");
    cfg["approval"] = json!({
        "trustOwnerKeysFrom": [trusted.display().to_string()],
        "origins": ["https://alpha.example.com:8443"],
        "linkedHosts": []
    });
    fs::write(&config, cfg.to_string()).expect("write the config");
    let socket = scratch.at("arca.sock");
    let mut serve = start_serve(&config);
    wait_for_server(&mut serve, &socket);
    let doctor = secret_of(&secrets, "doctor-1");
    let (listed, reply) = doctor_sees_action_approve(&socket, doctor);
    assert!(listed, "action_approve is listed");
    assert_ne!(reply["error"]["message"], json!("not_found"), "{reply}");
    let (_, text) = post(
        &socket,
        &[bearer(doctor)],
        &call(3, "arca_status", json!({})),
    );
    let status = tool_result(&text);
    conforming("arca_status", &status);
    assert_eq!(
        status["approval"]["trustedKeyFiles"],
        json!([{ "index": 0, "name": "owner-passkeys", "keys": 0, "reason": "the file is missing" }])
    );
    assert!(
        !text.contains("other-kernel"),
        "no path reaches a status reader"
    );
    assert_eq!(serve.terminate().code(), Some(0));
}

/// Whether a doctor sees `peer_keys_sync` listed on `socket`, and the reply to a call of it.
fn doctor_sees_peer_keys_sync(socket: &Path, doctor: &str) -> (bool, Value) {
    let (status, text) = post(socket, &[bearer(doctor)], &rpc(1, "tools/list", json!({})));
    assert_eq!(status, 200);
    let listed: Value = serde_json::from_str(&text).expect("JSON");
    let listed = listed["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .any(|t| t["name"] == json!("peer_keys_sync"));
    let (status, text) = post(
        socket,
        &[bearer(doctor)],
        &call(2, "peer_keys_sync", json!({ "lines": [] })),
    );
    assert_eq!(status, 200);
    (listed, serde_json::from_str(&text).expect("JSON"))
}

#[test]
fn peer_keys_sync_is_served_only_to_a_node_that_keeps_another_machines_passkeys() {
    // No peerKeys: the verb is neither listed nor answered.
    let scratch = Scratch::new("peer-off");
    let off = rig(&scratch);
    let (hash_dir, secrets) = mint_tokens(&scratch);
    let config = write_config(&scratch, off.shim.socket(), &hash_dir);
    let socket = scratch.at("arca.sock");
    let mut serve = start_serve(&config);
    wait_for_server(&mut serve, &socket);
    let doctor = secret_of(&secrets, "doctor-1");
    let (listed, reply) = doctor_sees_peer_keys_sync(&socket, doctor);
    assert!(!listed, "peer_keys_sync is not listed");
    assert_eq!(reply["error"]["message"], json!("not_found"), "{reply}");
    assert_eq!(serve.terminate().code(), Some(0));

    // peerKeys alone: peer_keys_sync and action_approve are served, and arca_status names the page host with no path.
    let scratch = Scratch::new("peer-on");
    let on = rig(&scratch);
    let (hash_dir, secrets) = mint_tokens(&scratch);
    let config = write_config(&scratch, on.shim.socket(), &hash_dir);
    let mut cfg: Value =
        serde_json::from_slice(&fs::read(&config).expect("config")).expect("config JSON");
    cfg["approval"] = json!({
        "peerKeys": { "from": "alpha", "origins": ["https://alpha.example.com:8443"] }
    });
    fs::write(&config, cfg.to_string()).expect("write the config");
    let socket = scratch.at("arca.sock");
    let mut serve = start_serve(&config);
    wait_for_server(&mut serve, &socket);
    let doctor = secret_of(&secrets, "doctor-1");
    let (listed, reply) = doctor_sees_peer_keys_sync(&socket, doctor);
    assert!(listed, "peer_keys_sync is listed");
    assert_eq!(
        reply["error"]["data"]["refusal"],
        json!("Untrusted"),
        "an empty list is refused by the handler: {reply}"
    );
    let (approve_listed, _) = doctor_sees_action_approve(&socket, doctor);
    assert!(
        approve_listed,
        "action_approve is served to a peerKeys node"
    );
    let (_, text) = post(
        &socket,
        &[bearer(doctor)],
        &call(3, "arca_status", json!({})),
    );
    let status = tool_result(&text);
    conforming("arca_status", &status);
    assert_eq!(
        status["approval"]["peerKeys"],
        json!({ "from": "alpha", "keys": 0, "since": null, "reason": "no passkey has been synced from alpha yet" })
    );
    assert!(
        !text.contains(&*scratch.0.display().to_string()),
        "no path reaches a status reader"
    );
    // The single object's status is what it has always been: no list of sources beside it.
    assert_eq!(
        status["approval"],
        json!({
            "linkedHosts": [],
            "trustedKeyFiles": [],
            "peerKeys": { "from": "alpha", "keys": 0, "since": null, "reason": "no passkey has been synced from alpha yet" },
        })
    );
    assert_eq!(serve.terminate().code(), Some(0));

    // peerKeys as a list: the first source is still `peerKeys`, and `peerKeySources` reports every source.
    let scratch = Scratch::new("peer-list");
    let listed_rig = rig(&scratch);
    let (hash_dir, secrets) = mint_tokens(&scratch);
    let config = write_config(&scratch, listed_rig.shim.socket(), &hash_dir);
    let mut cfg: Value =
        serde_json::from_slice(&fs::read(&config).expect("config")).expect("config JSON");
    cfg["approval"] = json!({ "peerKeys": [
        { "from": "alpha", "origins": ["https://alpha.example.com:8443"] },
        { "from": "cloud.example", "origins": ["https://cloud.example.com"] },
    ] });
    fs::write(&config, cfg.to_string()).expect("write the config");
    let socket = scratch.at("arca.sock");
    let mut serve = start_serve(&config);
    wait_for_server(&mut serve, &socket);
    let doctor = secret_of(&secrets, "doctor-1");
    let (listed, _) = doctor_sees_peer_keys_sync(&socket, doctor);
    assert!(listed, "peer_keys_sync is listed");
    let (_, text) = post(
        &socket,
        &[bearer(doctor)],
        &call(3, "arca_status", json!({})),
    );
    let status = tool_result(&text);
    conforming("arca_status", &status);
    let first = json!({ "from": "alpha", "keys": 0, "since": null, "reason": "no passkey has been synced from alpha yet" });
    assert_eq!(
        status["approval"],
        json!({
            "linkedHosts": [],
            "trustedKeyFiles": [],
            "peerKeys": first,
            "peerKeySources": [
                first,
                { "from": "cloud.example", "keys": 0, "since": null, "reason": "no passkey has been synced from cloud.example yet" },
            ],
        })
    );
    assert_eq!(serve.terminate().code(), Some(0));
}

/// The Doctor's token: the 16 tools droidd moved to Arca, which are also the golden 16 the parity token carries.
const DOCTOR_16: [&str; 16] = [
    "service_create",
    "service_list",
    "service_status",
    "service_logs",
    "service_remove",
    "service_connect",
    "service_disconnect",
    "service_secret_set",
    "service_publish",
    "service_unpublish",
    "omni_instance_create",
    "omni_pair",
    "omni_status",
    "owner_approval_request",
    "owner_approval_status",
    "owner_approval_list",
];

/// The canvas forward's token: the two service reads and the journal.
const FORWARD_3: [&str; 3] = ["service_list", "service_logs", "journal_list"];

/// Mints `id` through the binary with `--tools` set to `tools`, and returns its secret.
fn mint_with_list(
    scratch: &Scratch,
    hash_dir: &Path,
    id: &str,
    scope: &str,
    tools: &[&str],
) -> String {
    let out_file = scratch.at(&format!("{id}.secret"));
    let out = arca()
        .args(["token", "mint", "--scope", scope, "--id", id, "--tools"])
        .arg(tools.join(","))
        .arg("--hash-dir")
        .arg(hash_dir)
        .arg("--out")
        .arg(&out_file)
        .output()
        .expect("run token mint");
    assert!(
        out.status.success(),
        "minting {id} with its list succeeds: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    fs::read_to_string(&out_file)
        .expect("read the secret")
        .trim_end()
        .to_string()
}

#[test]
fn g8_the_doctors_16_and_the_forwards_3_each_reach_exactly_their_list() {
    let scratch = Scratch::new("exact-lists");
    let rig = rig(&scratch);
    let (hash_dir, _) = mint_tokens(&scratch);
    let lists: [(&str, &str, &[&str]); 2] = [
        ("doctor-16", "doctor", &DOCTOR_16),
        ("forward-3", "viewer", &FORWARD_3),
    ];
    let secrets: Vec<String> = lists
        .iter()
        .map(|(id, scope, tools)| mint_with_list(&scratch, &hash_dir, id, scope, tools))
        .collect();
    // On disk each hash file carries its list as the third line, in the order it was minted.
    for (id, _, tools) in &lists {
        let text = fs::read_to_string(hash_dir.join(format!("{id}.hash"))).expect("the hash file");
        assert_eq!(
            text.lines().nth(2),
            Some(format!("tools={}", tools.join(",")).as_str()),
            "{id}"
        );
    }
    let config = write_config(&scratch, rig.shim.socket(), &hash_dir);
    let socket = scratch.at("arca.sock");
    let mut serve = start_serve(&config);
    wait_for_server(&mut serve, &socket);

    let mut id = 0;
    for ((name, _, tools), secret) in lists.iter().zip(&secrets) {
        id += 1;
        let (status, text) = post(
            &socket,
            &[bearer(secret)],
            &rpc(id, "tools/list", json!({})),
        );
        assert_eq!(status, 200, "{name}: tools/list");
        let listed: Value = serde_json::from_str(&text).expect("JSON");
        let mut names: Vec<&str> = listed["result"]["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .map(|t| t["name"].as_str().expect("a name"))
            .collect();
        names.sort_unstable();
        let mut wanted = tools.to_vec();
        wanted.sort_unstable();
        assert_eq!(names, wanted, "{name}: tools/list holds exactly its list");

        // Every verb of the table: one in the list reaches its handler (any answer but 403, here a refusal of the empty
        // arguments or a read), one outside it is refused before any handler runs.
        for verb in arca_api::verbs() {
            id += 1;
            let (status, text) = post(&socket, &[bearer(secret)], &call(id, verb.name, json!({})));
            if tools.contains(&verb.name) {
                assert_ne!(status, 403, "{name} reaches {}: {text}", verb.name);
            } else if matches!(verb.name, "action_approve" | "peer_keys_sync") {
                // This node serves neither (no relayed approvals, no peer keys), so neither is in its table at all.
                let reply: Value = serde_json::from_str(&text).expect("JSON");
                assert_eq!(reply["error"]["message"], "not_found", "{name}: {text}");
            } else {
                assert_eq!(status, 403, "{name} is refused {}: {text}", verb.name);
            }
        }
    }
    assert_eq!(serve.terminate().code(), Some(0));
}

/// One HTTP/1.1 request, sent whole, and the reply's status and body.
fn raw<S: Read + Write>(mut stream: S, request: &str) -> (u16, String) {
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
    let body = reply
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_string())
        .unwrap_or_default();
    (status, body)
}

fn get(path: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nHost: arca\r\nConnection: close\r\n\r\n")
}

fn post_to(path: &str, headers: &[(&str, String)], body: &str) -> String {
    let mut request = format!(
        "POST {path} HTTP/1.1\r\nHost: arca\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);
    request
}

/// A connection to a unix socket, once it listens.
fn unix(path: &Path) -> UnixStream {
    let deadline = Instant::now() + WAIT;
    loop {
        match UnixStream::connect(path) {
            Ok(stream) => {
                stream.set_read_timeout(Some(WAIT)).expect("set a deadline");
                return stream;
            }
            Err(_) => {
                assert!(
                    Instant::now() < deadline,
                    "the socket listens within the deadline"
                );
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// The paths of the approval page, each with the method the page uses.
const PAGE_GETS: [&str; 4] = [
    "/approve",
    "/approve/plans",
    "/approve/passkeys",
    "/approve/canvas",
];
const PAGE_POSTS: [&str; 3] = [
    "/approve/decline",
    "/approve/approve",
    "/approve/setup-code",
];

/// The node a service with kernel access runs on: it serves its approval page (`approvalOrigin`) and has the services
/// socket. The page answers on the unix socket. The services socket answers the MCP routes alone: the page's paths are
/// not there, a call without a token is 401, a request with an Origin is 403, and a call with a token is served.
#[test]
fn the_services_socket_of_a_node_that_serves_its_approval_page_serves_mcp_alone() {
    let scratch = Scratch::new("services-socket");
    let rig = rig(&scratch);
    let (hash_dir, secrets) = mint_tokens(&scratch);
    let viewer = secret_of(&secrets, "viewer-1");
    let config = write_config(&scratch, rig.shim.socket(), &hash_dir);
    // The socket's directory, and the one above it, are the server's own and nobody else's, as serve requires.
    fs::set_permissions(&scratch.0, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
    let run = scratch.at("run");
    fs::create_dir(&run).expect("the socket's directory");
    fs::set_permissions(&run, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
    let services = run.join("services.sock");
    let mut value: Value =
        serde_json::from_str(&fs::read_to_string(&config).expect("read the config"))
            .expect("the config is JSON");
    value["approvalOrigin"] = json!("https://arca.example.com");
    value["listen"]["services"] = json!(services.display().to_string());
    fs::write(&config, value.to_string()).expect("write the config");
    let socket = scratch.at("arca.sock");
    let mut serve = start_serve(&config);
    wait_for_server(&mut serve, &socket);

    // This node does serve its approval page: the unix socket answers it, with no token.
    let (status, page) = raw(unix(&socket), &get("/approve"));
    assert_eq!(status, 200);
    assert!(page.contains("<html"), "the approval page is served there");

    // The services socket is the server's own, mode 0600, and has none of the page, with a token or without one.
    drop(unix(&services));
    let meta = fs::symlink_metadata(&services).expect("stat the socket");
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    for path in PAGE_GETS {
        let (status, body) = raw(unix(&services), &get(path));
        assert_eq!(status, 404, "GET {path}");
        assert!(body.is_empty(), "GET {path} answers no page");
    }
    for path in PAGE_POSTS {
        for headers in [vec![], vec![bearer(viewer)]] {
            let (status, body) = raw(unix(&services), &post_to(path, &headers, "{}"));
            assert_eq!(status, 404, "POST {path}");
            assert!(body.is_empty(), "POST {path} answers nothing");
        }
        // The page's own POSTs carry its Origin: refused before the path is looked at.
        let origin = [("Origin", "https://arca.example.com".to_string())];
        let (status, _) = raw(unix(&services), &post_to(path, &origin, "{}"));
        assert_eq!(status, 403, "POST {path} with the page's Origin");
    }

    // A call without a token is refused, one with an Origin is refused, and one with a token is served as on any
    // listener.
    let initialize = rpc(1, "initialize", json!({ "protocolVersion": "2025-11-25" }));
    let (status, body) = raw(unix(&services), &post_to("/mcp", &[], &initialize));
    assert_eq!(status, 401);
    assert!(body.is_empty(), "a 401 says nothing about the node");
    let (status, _) = raw(
        unix(&services),
        &post_to("/mcp", &[], &call(2, "arca_status", json!({}))),
    );
    assert_eq!(status, 401);
    let with_origin = [
        bearer(viewer),
        ("Origin", "https://arca.example.com".to_string()),
    ];
    let (status, _) = raw(unix(&services), &post_to("/mcp", &with_origin, &initialize));
    assert_eq!(status, 403);
    let (status, body) = raw(
        unix(&services),
        &post_to("/mcp", &[bearer(viewer)], &initialize),
    );
    assert_eq!(status, 200);
    let reply: Value = serde_json::from_str(&body).expect("the reply is JSON");
    assert_eq!(reply["result"]["protocolVersion"], "2025-11-25");

    // A clean stop takes both sockets away.
    assert_eq!(serve.terminate().code(), Some(0));
    assert!(fs::symlink_metadata(&services).is_err());
    assert!(fs::symlink_metadata(&socket).is_err());
}

/// A node without `approvalOrigin` serves no approval page on any listener, its proxy listener included: the page's
/// paths are 404 there, and the MCP route still needs a token.
#[test]
fn a_proxy_listener_without_an_approval_origin_serves_no_page_route() {
    let scratch = Scratch::new("proxy-no-page");
    let rig = rig(&scratch);
    let (hash_dir, secrets) = mint_tokens(&scratch);
    let viewer = secret_of(&secrets, "viewer-1");
    let config = write_config(&scratch, rig.shim.socket(), &hash_dir);
    // A free port of 127.0.0.1, read from a probe and released for the server.
    let at = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind a probe")
        .local_addr()
        .expect("the probe's address");
    let mut value: Value =
        serde_json::from_str(&fs::read_to_string(&config).expect("read the config"))
            .expect("the config is JSON");
    assert!(value.get("approvalOrigin").is_none());
    value["listen"]["proxy"] = json!({ "addr": at.to_string(), "peer": "127.0.0.1" });
    fs::write(&config, value.to_string()).expect("write the config");
    let socket = scratch.at("arca.sock");
    let mut serve = start_serve(&config);
    wait_for_server(&mut serve, &socket);
    let proxy = || {
        let deadline = Instant::now() + WAIT;
        loop {
            match std::net::TcpStream::connect(at) {
                Ok(stream) => {
                    stream.set_read_timeout(Some(WAIT)).expect("set a deadline");
                    return stream;
                }
                Err(_) => {
                    assert!(
                        Instant::now() < deadline,
                        "the proxy listener listens within the deadline"
                    );
                    thread::sleep(Duration::from_millis(20));
                }
            }
        }
    };
    for path in PAGE_GETS {
        let (status, body) = raw(proxy(), &get(path));
        assert_eq!(status, 404, "GET {path} on the proxy listener");
        assert!(body.is_empty(), "GET {path} answers no page");
        let (status, _) = raw(unix(&socket), &get(path));
        assert_eq!(status, 404, "GET {path} on the unix socket");
    }
    for path in PAGE_POSTS {
        for headers in [vec![], vec![bearer(viewer)]] {
            let (status, body) = raw(proxy(), &post_to(path, &headers, "{}"));
            assert_eq!(status, 404, "POST {path} on the proxy listener");
            assert!(body.is_empty(), "POST {path} answers nothing");
        }
        let origin = [("Origin", "https://arca.example.com".to_string())];
        let (status, _) = raw(proxy(), &post_to(path, &origin, "{}"));
        assert_eq!(status, 403, "POST {path} with an Origin");
    }
    let initialize = rpc(1, "initialize", json!({ "protocolVersion": "2025-11-25" }));
    let (status, _) = raw(proxy(), &post_to("/mcp", &[], &initialize));
    assert_eq!(status, 401);
    let (status, _) = raw(proxy(), &post_to("/mcp", &[bearer(viewer)], &initialize));
    assert_eq!(status, 200);
    assert_eq!(serve.terminate().code(), Some(0));
}
