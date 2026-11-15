//! The console's token: with `console.tokenFile` set, `arca serve` makes one viewer token limited to the console's reads,
//! holds it in memory only and writes it to that file at mode 0400. A reload keeps it, a restart replaces it, a clean stop
//! removes the file, and the owner sees it in `token_list` and `arca status --security`.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(20);
const READS: [&str; 4] = ["node_metrics", "arca_status", "alerts_list", "service_list"];

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("arca-console-{}-{tag}", std::process::id()));
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

fn arca() -> Command {
    Command::new(env!("CARGO_BIN_EXE_arca"))
}

/// A node with no kernel projects, its socket and token directory in `scratch`, and `console.tokenFile` set to `console`
/// when it is given.
struct Node {
    config: PathBuf,
    socket: PathBuf,
}

fn node(scratch: &Scratch, console: Option<&Path>) -> Node {
    let state = scratch.at("state");
    let tokens = state.join("tokens");
    private_dir(&state);
    private_dir(&tokens);
    let socket = scratch.at("arca.sock");
    let mut config = json!({
        "stateDir": state,
        "incus": { "socket": scratch.at("absent-incus.sock"), "pools": [] },
        "identityPhones": [],
        "listen": { "unix": socket, "tailnet": null },
        "tokens": { "hashDir": tokens },
        "kernel": { "projects": [], "scratch": false, "droiddProjects": [], "reservedCidrs": [] }
    });
    if let Some(file) = console {
        config["console"] = json!({ "tokenFile": file });
    }
    let path = state.join("node.json");
    fs::write(&path, config.to_string()).expect("write config");
    Node {
        config: path,
        socket,
    }
}

struct Serve(Child);

impl Serve {
    fn start(node: &Node) -> Self {
        Self(
            arca()
                .args(["serve", "--config"])
                .arg(&node.config)
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .expect("start serve"),
        )
    }

    /// Waits until the node answers on its socket and its pid file names it.
    fn ready(&mut self, node: &Node) {
        let pid = node.config.with_file_name("serve.pid");
        let expected = format!("{}\n", self.0.id());
        let deadline = Instant::now() + WAIT;
        loop {
            if fs::read_to_string(&pid).ok().as_ref() == Some(&expected)
                && UnixStream::connect(&node.socket).is_ok()
            {
                return;
            }
            if let Some(status) = self.0.try_wait().expect("poll serve") {
                panic!(
                    "serve exited ({status}) before it listened: {}",
                    self.stderr()
                );
            }
            assert!(Instant::now() < deadline, "serve did not listen");
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// A clean stop: SIGTERM, then the exit.
    fn stop(mut self) -> std::process::ExitStatus {
        let killed = Command::new("kill")
            .args(["-TERM", &self.0.id().to_string()])
            .status()
            .expect("send SIGTERM");
        assert!(killed.success());
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.0.try_wait().expect("poll serve") {
                return status;
            }
            assert!(Instant::now() < deadline, "serve did not stop");
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// A start that must be refused: its exit status and its stderr.
    fn refused(mut self) -> (std::process::ExitStatus, String) {
        let deadline = Instant::now() + WAIT;
        let status = loop {
            if let Some(status) = self.0.try_wait().expect("poll serve") {
                break status;
            }
            assert!(Instant::now() < deadline, "serve did not exit");
            thread::sleep(Duration::from_millis(20));
        };
        (status, self.stderr())
    }

    fn stderr(&mut self) -> String {
        let mut text = String::new();
        if let Some(mut err) = self.0.stderr.take() {
            let _ = err.read_to_string(&mut text);
        }
        text
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// One `POST /mcp` over the node's socket: the status and the body.
fn post(node: &Node, token: &str, body: &Value) -> (u16, String) {
    let mut stream = UnixStream::connect(&node.socket).expect("connect");
    stream.set_read_timeout(Some(WAIT)).expect("read timeout");
    let bytes = body.to_string();
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{bytes}",
        bytes.len()
    );
    stream.write_all(request.as_bytes()).expect("write");
    let mut reply = String::new();
    stream.read_to_string(&mut reply).expect("read");
    let status = reply
        .get(9..12)
        .and_then(|code| code.parse().ok())
        .expect("a status line");
    let body = reply
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_string())
        .unwrap_or_default();
    (status, body)
}

fn initialize(node: &Node, token: &str) -> (u16, String) {
    post(
        node,
        token,
        &json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "console", "version": "1" } }
        }),
    )
}

fn call(node: &Node, token: &str, tool: &str) -> (u16, String) {
    post(
        node,
        token,
        &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": tool, "arguments": {} } }),
    )
}

/// The token in the console's file, without its newline.
fn read_token(file: &Path) -> String {
    let text = fs::read_to_string(file).expect("the console's token file");
    text.strip_suffix('\n')
        .expect("one line with its newline")
        .to_string()
}

fn mint(node: &Node, scope: &str, id: &str, out: &Path) {
    let minted = arca()
        .args(["token", "mint", "--scope", scope, "--id", id, "--config"])
        .arg(&node.config)
        .arg("--out")
        .arg(out)
        .output()
        .expect("run token mint");
    assert!(minted.status.success(), "{}", text(&minted));
}

fn reload(node: &Node) {
    let reloaded = arca()
        .args(["token", "reload", "--config"])
        .arg(&node.config)
        .output()
        .expect("run token reload");
    assert!(reloaded.status.success(), "{}", text(&reloaded));
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn without_the_key_no_file_is_written_and_no_token_is_held() {
    let scratch = Scratch::new("absent");
    let node = node(&scratch, None);
    let operator = scratch.at("operator.secret");
    mint(&node, "operator", "ops", &operator);
    let mut serve = Serve::start(&node);
    serve.ready(&node);
    let token = read_token(&operator);
    let (status, body) = call(&node, &token, "token_list");
    assert_eq!(status, 200, "{body}");
    let listed: Value = serde_json::from_str(&body).expect("json");
    let ids: Vec<&str> = listed["result"]["structuredContent"]["tokens"]
        .as_array()
        .expect("tokens")
        .iter()
        .map(|token| token["id"].as_str().expect("id"))
        .collect();
    assert_eq!(ids, vec!["ops"], "only the stored token");
    let names: Vec<String> = fs::read_dir(scratch.at("state"))
        .expect("state")
        .map(|item| {
            item.expect("item")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert!(
        names.iter().all(|name| !name.contains("console")),
        "{names:?}"
    );
}

#[test]
fn with_the_key_the_file_is_0400_and_its_token_calls_each_of_the_four_reads() {
    let scratch = Scratch::new("reads");
    let run = scratch.at("run");
    private_dir(&run);
    let file = run.join("console.token");
    let node = node(&scratch, Some(&file));
    let mut serve = Serve::start(&node);
    serve.ready(&node);
    let meta = fs::symlink_metadata(&file).expect("the file exists");
    assert!(meta.is_file());
    assert_eq!(meta.mode() & 0o7777, 0o400, "mode 0400");
    assert_eq!(meta.nlink(), 1);
    let token = read_token(&file);
    assert!(token.starts_with("arca_"), "a token's shape");
    let (status, body) = initialize(&node, &token);
    assert_eq!(status, 200, "{body}");
    for verb in READS {
        let (status, body) = call(&node, &token, verb);
        assert_eq!(status, 200, "{verb}: {body}");
        let answer: Value = serde_json::from_str(&body).expect("json");
        assert!(answer.get("result").is_some(), "{verb}: {body}");
    }
    assert!(serve.stop().success());
}

#[test]
fn the_token_is_refused_a_verb_outside_its_list_and_the_refusal_names_the_verb() {
    let scratch = Scratch::new("outside");
    let run = scratch.at("run");
    private_dir(&run);
    let file = run.join("console.token");
    let node = node(&scratch, Some(&file));
    let mut serve = Serve::start(&node);
    serve.ready(&node);
    let token = read_token(&file);
    for verb in ["journal_list", "token_list", "plan_list"] {
        let (status, body) = call(&node, &token, verb);
        assert_eq!(status, 403, "{verb}: {body}");
        assert!(
            body.contains(verb),
            "{verb}: the refusal names it: {body:?}"
        );
        assert!(!body.contains(&token), "the token is never echoed");
    }
}

#[test]
fn a_reload_keeps_the_token_and_a_restart_replaces_it_and_a_clean_stop_removes_the_file() {
    let scratch = Scratch::new("lifetime");
    let run = scratch.at("run");
    private_dir(&run);
    let file = run.join("console.token");
    let node = node(&scratch, Some(&file));
    let mut serve = Serve::start(&node);
    serve.ready(&node);
    let first = read_token(&file);
    // A stored token minted while the node runs, then a reload: the console's token stays.
    mint(&node, "viewer", "later", &scratch.at("later.secret"));
    reload(&node);
    assert_eq!(
        call(&node, &first, "node_metrics").0,
        200,
        "kept by the reload"
    );
    assert_eq!(
        read_token(&file),
        first,
        "the file is not rewritten by a reload"
    );
    // A clean stop removes the file.
    assert!(serve.stop().success());
    assert!(
        fs::symlink_metadata(&file).is_err(),
        "a clean stop leaves no file"
    );
    // A new start makes a new token, and the old one is refused.
    let mut serve = Serve::start(&node);
    serve.ready(&node);
    let second = read_token(&file);
    assert_ne!(second, first, "a restart makes a new token");
    assert_eq!(
        call(&node, &first, "node_metrics").0,
        401,
        "the old one is dead"
    );
    assert_eq!(call(&node, &second, "node_metrics").0, 200);
    assert!(serve.stop().success());
}

#[test]
fn a_file_left_by_a_crash_is_replaced() {
    let scratch = Scratch::new("leftover");
    let run = scratch.at("run");
    private_dir(&run);
    let file = run.join("console.token");
    // What a node that was killed leaves: its own 0400 file with a dead token of the right shape.
    let dead = format!("arca_{}", "A".repeat(43));
    fs::write(&file, format!("{dead}\n")).expect("leftover");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o400)).expect("0400");
    let node = node(&scratch, Some(&file));
    let mut serve = Serve::start(&node);
    serve.ready(&node);
    let token = read_token(&file);
    assert_ne!(token, dead);
    assert_eq!(call(&node, &token, "arca_status").0, 200);
}

#[test]
fn a_directory_open_to_a_group_or_others_refuses_the_start() {
    for mode in [0o750, 0o705, 0o770] {
        let scratch = Scratch::new(&format!("open-{mode:o}"));
        let run = scratch.at("run");
        private_dir(&run);
        fs::set_permissions(&run, fs::Permissions::from_mode(mode)).expect("mode");
        let file = run.join("console.token");
        let node = node(&scratch, Some(&file));
        let (status, stderr) = Serve::start(&node).refused();
        assert_eq!(status.code(), Some(2), "{mode:o}: {stderr}");
        assert!(
            stderr.contains("console.tokenFile") && stderr.contains("group or others"),
            "{mode:o}: {stderr}"
        );
        assert!(fs::symlink_metadata(&file).is_err(), "nothing written");
    }
}

#[test]
fn a_link_at_the_files_path_refuses_the_start_and_is_left_alone() {
    let scratch = Scratch::new("link");
    let run = scratch.at("run");
    private_dir(&run);
    let target = scratch.at("elsewhere");
    fs::write(&target, "not the node's\n").expect("target");
    let file = run.join("console.token");
    symlink(&target, &file).expect("symlink");
    let node = node(&scratch, Some(&file));
    let (status, stderr) = Serve::start(&node).refused();
    assert_eq!(status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains("console.tokenFile") && stderr.contains("link"),
        "{stderr}"
    );
    assert!(
        fs::symlink_metadata(&file)
            .expect("the link is still there")
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::read_to_string(&target).expect("target"),
        "not the node's\n"
    );
}

#[test]
fn a_file_that_is_not_the_nodes_earlier_one_refuses_the_start_and_is_left_alone() {
    let scratch = Scratch::new("foreign");
    let run = scratch.at("run");
    private_dir(&run);
    let file = run.join("console.token");
    fs::write(&file, "someone's notes\n").expect("file");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).expect("0600");
    let node = node(&scratch, Some(&file));
    let (status, stderr) = Serve::start(&node).refused();
    assert_eq!(status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("console.tokenFile"), "{stderr}");
    assert_eq!(
        fs::read_to_string(&file).expect("file"),
        "someone's notes\n"
    );
}

#[test]
fn token_list_shows_the_consoles_token_and_says_it_is_not_stored_and_revoke_is_refused() {
    let scratch = Scratch::new("list");
    let run = scratch.at("run");
    private_dir(&run);
    let file = run.join("console.token");
    let node = node(&scratch, Some(&file));
    let operator = scratch.at("operator.secret");
    mint(&node, "operator", "ops", &operator);
    let mut serve = Serve::start(&node);
    serve.ready(&node);
    let (status, body) = call(&node, &read_token(&operator), "token_list");
    assert_eq!(status, 200, "{body}");
    let listed: Value = serde_json::from_str(&body).expect("json");
    let tokens = listed["result"]["structuredContent"]["tokens"]
        .as_array()
        .expect("tokens")
        .clone();
    let console = tokens
        .iter()
        .find(|token| token["console"] == json!(true))
        .unwrap_or_else(|| panic!("the console's token is listed: {tokens:?}"));
    assert_eq!(console["id"], json!("node:console"));
    assert_eq!(console["scope"], json!("viewer"));
    assert_eq!(console["tools"], json!(READS));
    assert_eq!(console["stored"], json!(false));
    assert!(
        !body.contains(&read_token(&file)),
        "the list never carries the token"
    );
    let ops = tokens
        .iter()
        .find(|token| token["id"] == json!("ops"))
        .expect("the stored token");
    assert!(ops.get("console").is_none() && ops.get("stored").is_none());

    let revoke = arca()
        .args(["token", "revoke", "node:console", "--config"])
        .arg(&node.config)
        .output()
        .expect("run token revoke");
    assert_eq!(revoke.status.code(), Some(2), "{}", text(&revoke));
    let said = text(&revoke);
    assert!(
        said.contains("console.tokenFile") && said.contains("remove"),
        "the refusal says how to turn it off: {said}"
    );
    assert_eq!(
        call(&node, &read_token(&file), "node_metrics").0,
        200,
        "still works"
    );
}

#[test]
fn status_security_counts_the_consoles_token_among_those_that_only_look() {
    let scratch = Scratch::new("security");
    let run = scratch.at("run");
    private_dir(&run);
    let file = run.join("console.token");
    let node = node(&scratch, Some(&file));
    mint(&node, "viewer", "reader", &scratch.at("reader.secret"));
    let output = arca()
        .args(["status", "--security", "--config"])
        .arg(&node.config)
        .output()
        .expect("run status --security");
    assert!(output.status.success(), "{}", text(&output));
    let said = text(&output);
    assert!(
        said.contains("Tokens that only look: 2, the console's among them."),
        "{said}"
    );
    assert!(
        said.contains("node_metrics, arca_status, alerts_list, service_list"),
        "{said}"
    );
    assert!(said.contains(&file.display().to_string()), "{said}");

    let without = Scratch::new("security-absent");
    let plain = node_plain(&without);
    let output = arca()
        .args(["status", "--security", "--config"])
        .arg(&plain.config)
        .output()
        .expect("run status --security");
    let said = text(&output);
    assert!(output.status.success(), "{said}");
    assert!(
        !said.contains("console's") && !said.contains("console.tokenFile"),
        "no word of a console without the key: {said}"
    );
}

fn node_plain(scratch: &Scratch) -> Node {
    node(scratch, None)
}
