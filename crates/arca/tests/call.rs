//! `arca call`, run as the binary against a served node: the exit codes 0 to 4, the scope and token checks, and the refusal of an
//! approval named on the command line. The node reads its owner signers only from a scratch injection, so no
//! test reads a host's owner file. No test prints a token or an approval.

mod common;
#[path = "../../arca-approval/tests/common/webauthn.rs"]
mod webauthn;

use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use arca_kernel::canon::{Digest, canonical_json, params_digest};
use common::*;
use serde_json::{Value, json};
use webauthn::SoftPasskey;

const WAIT: Duration = Duration::from_secs(15);
/// A scratch project name for the served kernel. The node takes its owner signers from the test injection only when every
/// kernel project starts with `arcak-scratch-`.
const CALL_PROJECT: &str = "arcak-scratch-call";

fn arca() -> Command {
    Command::new(env!("CARGO_BIN_EXE_arca"))
}

/// A served node, killed when the test ends.
struct Node {
    scratch: Scratch,
    serve: Child,
    config: PathBuf,
    socket: PathBuf,
    viewer: PathBuf,
    doctor: PathBuf,
    owner: SkKey,
}

impl Node {
    /// Stops the server and returns what it wrote to stderr. The pipe is read here, so a line the server logged is seen.
    fn stop(&mut self) -> String {
        let _ = self.serve.kill();
        let _ = self.serve.wait();
        let mut text = String::new();
        if let Some(mut stderr) = self.serve.stderr.take() {
            let _ = stderr.read_to_string(&mut text);
        }
        text
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.serve.kill();
        let _ = self.serve.wait();
    }
}

fn write_mode(path: &Path, text: &str, mode: u32) {
    fs::write(path, text).expect("the file is written");
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("the mode is set");
}

/// Mints one token of `scope` with the binary, and returns its secret file (mode 0400).
fn mint(scratch: &Scratch, hash_dir: &Path, scope: &str, id: &str) -> PathBuf {
    let out = scratch.path(&format!("{id}.secret"));
    let status = arca()
        .args(["token", "mint", "--scope", scope, "--id", id, "--hash-dir"])
        .arg(hash_dir)
        .arg("--out")
        .arg(&out)
        .output()
        .expect("run token mint");
    assert!(status.status.success(), "minting {id} succeeds");
    out
}

fn start_node(tag: &str) -> Node {
    let owner = SkKey::from_seed(1);
    start_node_with(tag, &format!("{}\n", owner.line(NAMESPACE)))
}

/// A served node whose owner signers file holds exactly `signers`.
fn start_node_with(tag: &str, signers: &str) -> Node {
    start_node_keys(tag, signers, None)
}

/// A served node whose owner signers file holds exactly `signers`, and whose test passkeys file, when `passkeys` names one,
/// holds exactly that text at mode `0600`.
fn start_node_keys(tag: &str, signers: &str, passkeys: Option<&str>) -> Node {
    let scratch = Scratch::new(tag);
    let fake = start_fake(&scratch);
    let owner = SkKey::from_seed(1);
    let signers_path = scratch.path("owner-signers");
    write_mode(&signers_path, signers, 0o600);

    let state = scratch.path("state");
    fs::create_dir(&state).expect("the state directory");
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).expect("mode 0700");
    let hash_dir = scratch.path("tokens");
    fs::create_dir(&hash_dir).expect("the token directory");
    fs::set_permissions(&hash_dir, fs::Permissions::from_mode(0o700)).expect("mode 0700");
    let viewer = mint(&scratch, &hash_dir, "viewer", "viewer-1");
    let doctor = mint(&scratch, &hash_dir, "doctor", "doctor-1");

    let socket = scratch.path("arca.sock");
    let config = scratch.path("node.json");
    let value = json!({
        "stateDir": state.display().to_string(),
        "approvalHost": HOST,
        "incus": { "socket": fake.socket().display().to_string(), "pools": [] },
        "identityPhones": [],
        "listen": { "unix": socket.display().to_string(), "tailnet": null },
        "tokens": { "hashDir": hash_dir.display().to_string() },
        "kernel": {
            "projects": [CALL_PROJECT],
            "scratch": true,
            "droiddProjects": ["droid"],
            "reservedCidrs": []
        }
    });
    write_mode(&config, &value.to_string(), 0o600);

    let mut command = arca();
    command
        .args(["serve", "--config"])
        .arg(&config)
        .env("ARCA_TEST_OWNER_SIGNERS", &signers_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(text) = passkeys {
        let passkeys_path = scratch.path("owner-passkeys");
        write_mode(&passkeys_path, text, 0o600);
        command.env("ARCA_TEST_OWNER_PASSKEYS", &passkeys_path);
    }
    let serve = command.spawn().expect("start arca serve");
    let mut node = Node {
        scratch,
        serve,
        config,
        socket,
        viewer,
        doctor,
        owner,
    };
    wait_for(&mut node);
    node
}

/// Waits until the node's unix socket accepts a connection.
fn wait_for(node: &mut Node) {
    let deadline = Instant::now() + WAIT;
    loop {
        if UnixStream::connect(&node.socket).is_ok() {
            return;
        }
        if let Some(status) = node.serve.try_wait().expect("poll the server") {
            panic!("the server exited before listening ({status})");
        }
        assert!(
            Instant::now() < deadline,
            "the server listens within the deadline"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// `arca call <verb> --config <config> --token-file <token> <extra>`.
fn call(node: &Node, token: &Path, verb: &str, extra: &[&str]) -> Output {
    arca()
        .args(["call", verb, "--config"])
        .arg(&node.config)
        .arg("--token-file")
        .arg(token)
        .args(extra)
        .output()
        .expect("run arca call")
}

fn code(output: &Output) -> i32 {
    output.status.code().expect("the process exits with a code")
}

fn stdout_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).expect("stdout is one JSON document")
}

fn zeros() -> String {
    Digest::of(b"no such plan").to_hex()
}

/// A statement for plan `plan`, issued now and valid for ten minutes, in the exact canonical form the kernel reads.
fn statement_for(plan: &str, nonce: &str) -> String {
    let now = arca::handlers::now_ms();
    canonical_json(&json!({
        "action": "delete",
        "expiresAt": arca::handlers::iso_utc(now + 10 * 60 * 1000),
        "host": HOST,
        "issuedAt": arca::handlers::iso_utc(now),
        "nonce": nonce,
        "paramsDigest": params_digest(&json!({ "plan": plan })).expect("the arguments are an object"),
        "target": format!("plan-{plan}"),
    }))
    .expect("the statement canonicalizes")
}

#[test]
fn the_server_logs_the_count_of_the_keys_it_trusts_after_it_drops_the_other_lines() {
    // Two lines are not the Arca hardware key the verifier trusts: a key for another namespace and an ed25519 key. The count
    // logged at start is the one the verifier keeps.
    let owner = SkKey::from_seed(1);
    let other = SkKey::from_seed(2);
    let ed25519 = "owner namespaces=\"arca-kernel-approve\" ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    let text = format!(
        "# the owner's keys\n{}\n{}\n{ed25519}\n",
        owner.line(NAMESPACE),
        other.line("another-namespace"),
    );
    let mut node = start_node_with("serve-count", &text);
    let served = node.stop();
    assert!(
        served.contains("arca: 1 owner signing key(s), 0 passkey(s) kept"),
        "the count is the one the verifier keeps: {served}"
    );
}

#[test]
fn the_server_logs_the_count_of_the_passkeys_it_keeps_and_never_a_key() {
    let owner = SkKey::from_seed(1);
    let (kept, other) = (SoftPasskey::from_seed(1), SoftPasskey::from_seed(2));
    let passkeys = format!(
        "# the owner's passkeys\n{}\n{}\n",
        kept.line(NAMESPACE),
        other.line("arca-approve"),
    );
    let signers = format!("{}\n", owner.line(NAMESPACE));
    let mut node = start_node_keys("serve-passkeys", &signers, Some(&passkeys));
    let served = node.stop();
    assert!(
        served.contains("arca: 1 owner signing key(s), 1 passkey(s) kept"),
        "only the Arca passkey is kept: {served}"
    );
    for key in [&kept, &other] {
        assert!(!served.contains(&key.cred_text()) && !served.contains(&key.key_text()));
    }
}

#[test]
fn a_malformed_passkeys_file_refuses_passkeys_by_line_number_and_the_server_still_serves() {
    let key = SoftPasskey::from_seed(1);
    let passkeys = format!(
        "{}\nnot a passkey line {}\n",
        key.line(NAMESPACE),
        key.cred_text()
    );
    let signers = format!("{}\n", SkKey::from_seed(1).line(NAMESPACE));
    let mut node = start_node_keys("serve-passkeys-bad", &signers, Some(&passkeys));
    let out = call(&node, &node.viewer, "arca_status", &[]);
    assert_eq!(code(&out), 0, "the server serves");
    let served = node.stop();
    assert!(
        served.contains("arca: 1 owner signing key(s), 0 passkey(s) kept"),
        "the SSH key is kept and no passkey: {served}"
    );
    assert!(
        served.contains("every passkey approval is refused") && served.contains("line 2 "),
        "the refusal names the line: {served}"
    );
    assert!(!served.contains(&key.cred_text()) && !served.contains(&key.key_text()));
}

#[test]
fn a_test_passkeys_file_outside_a_scratch_kernel_refuses_start_with_exit_2() {
    let scratch = Scratch::new("serve-passkeys-prod");
    let state = scratch.path("state");
    fs::create_dir(&state).expect("the state directory");
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).expect("mode 0700");
    let hash_dir = scratch.path("tokens");
    fs::create_dir(&hash_dir).expect("the token directory");
    fs::set_permissions(&hash_dir, fs::Permissions::from_mode(0o700)).expect("mode 0700");
    let passkeys = scratch.path("owner-passkeys");
    write_mode(&passkeys, "", 0o600);
    // A scratch kernel with a project outside the prefix is refused by the config itself, before the injection is read.
    for (tag, scratch_kernel, projects) in [
        ("not-scratch", false, json!([])),
        ("not-scratch-prefixed", false, json!([CALL_PROJECT])),
        ("no-projects", true, json!([])),
    ] {
        let config = scratch.path(&format!("node-{tag}.json"));
        let value = json!({
            "stateDir": state.display().to_string(),
            "approvalHost": HOST,
            "incus": { "socket": scratch.path("no-incus.sock").display().to_string(), "pools": [] },
            "identityPhones": [],
            "listen": { "unix": scratch.path(&format!("{tag}.sock")).display().to_string(), "tailnet": null },
            "tokens": { "hashDir": hash_dir.display().to_string() },
            "kernel": {
                "projects": projects,
                "scratch": scratch_kernel,
                "droiddProjects": ["droid"],
                "reservedCidrs": []
            }
        });
        write_mode(&config, &value.to_string(), 0o600);
        // No signers file is injected, so the refusal is the passkeys injection's alone.
        let out = arca()
            .args(["serve", "--config"])
            .arg(&config)
            .env("ARCA_TEST_OWNER_PASSKEYS", &passkeys)
            .output()
            .expect("run arca serve");
        assert_eq!(
            out.status.code(),
            Some(2),
            "{tag}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("passkeys"),
            "{tag}: the refusal names the passkeys injection: {stderr}"
        );
    }
}

#[test]
fn a_viewer_reads_the_node_status_and_the_call_exits_0_with_the_result() {
    let node = start_node("status");
    let out = call(&node, &node.viewer, "arca_status", &[]);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    let result = stdout_json(&out);
    assert_eq!(result["mode"], json!("watch-only"));
    assert!(result["node"].is_string());
}

#[test]
fn a_viewer_lists_the_waiting_plans_and_the_call_exits_0_with_none() {
    let node = start_node("list");
    let out = call(&node, &node.viewer, "plan_list", &["--args", "{}"]);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(stdout_json(&out), json!({ "plans": [] }));
}

#[test]
fn an_unknown_plan_is_a_tool_error_exit_3_with_the_error_object_on_stdout() {
    let node = start_node("tool-error");
    let args = json!({ "plan": zeros() }).to_string();
    let out = call(&node, &node.doctor, "plan_decline", &["--args", &args]);
    assert_eq!(code(&out), 3, "{}", String::from_utf8_lossy(&out.stderr));
    let reply = stdout_json(&out);
    assert_eq!(reply["code"], json!(-32000));
    assert_eq!(reply["data"]["refusal"], json!("NoSuchPlan"));
}

#[test]
fn a_viewer_calling_a_write_verb_is_refused_403_and_the_call_exits_4() {
    let node = start_node("viewer-write");
    let args = json!({ "plan": zeros() }).to_string();
    let out = call(&node, &node.viewer, "plan_decline", &["--args", &args]);
    assert_eq!(code(&out), 4, "{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.stdout.is_empty(), "a refused call prints no result");
}

#[test]
fn a_doctor_calling_token_list_is_refused_403_and_the_call_exits_4() {
    let node = start_node("doctor-operator");
    let out = call(&node, &node.doctor, "token_list", &["--args", "{}"]);
    assert_eq!(code(&out), 4, "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn a_token_the_server_does_not_hold_is_refused_401_and_the_call_exits_4() {
    let node = start_node("unknown-token");
    let stranger = node.scratch.path("stranger.secret");
    write_mode(&stranger, &format!("arca_{}\n", "A".repeat(43)), 0o400);
    let out = call(&node, &stranger, "arca_status", &[]);
    assert_eq!(code(&out), 4, "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn args_that_name_an_approval_are_refused_as_usage_before_any_connection() {
    let node = start_node("args-approval");
    let args =
        json!({ "plan": zeros(), "approval": "APPROVAL-TEXT-THAT-MUST-NOT-LEAK" }).to_string();
    let out = call(&node, &node.doctor, "approval_submit", &["--args", &args]);
    assert_eq!(code(&out), 2, "{}", String::from_utf8_lossy(&out.stderr));
    let shown = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!shown.contains("APPROVAL-TEXT-THAT-MUST-NOT-LEAK"));
}

#[test]
fn a_token_file_with_a_group_bit_is_refused_as_usage_and_the_call_exits_2() {
    let node = start_node("token-mode");
    let loose = node.scratch.path("loose.secret");
    let secret = fs::read_to_string(&node.doctor).expect("the minted secret");
    write_mode(&loose, &secret, 0o640);
    let out = call(&node, &loose, "arca_status", &[]);
    assert_eq!(code(&out), 2, "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!String::from_utf8_lossy(&out.stderr).contains(secret.trim()));
}

#[test]
fn an_args_file_with_an_other_bit_is_refused_as_usage_and_the_call_exits_2() {
    let node = start_node("args-mode");
    let args = node.scratch.path("args.json");
    write_mode(&args, "{}", 0o604);
    let out = arca()
        .args(["call", "plan_list", "--config"])
        .arg(&node.config)
        .arg("--token-file")
        .arg(&node.viewer)
        .arg("--args-file")
        .arg(&args)
        .output()
        .expect("run arca call");
    assert_eq!(code(&out), 2, "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn an_unknown_verb_is_a_usage_error_and_the_call_exits_2() {
    let node = start_node("unknown-verb");
    let out = call(&node, &node.viewer, "no_such_verb", &[]);
    assert_eq!(code(&out), 2, "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn a_viewer_submitting_an_approval_is_refused_403_before_the_approval_is_checked() {
    let node = start_node("viewer-approval");
    let approval_file = node.scratch.path("viewer-approval.json");
    let body = json!({ "plan": zeros(), "approval": "x.y" }).to_string();
    write_mode(&approval_file, &body, 0o600);
    let out = arca()
        .args(["call", "approval_submit", "--config"])
        .arg(&node.config)
        .arg("--token-file")
        .arg(&node.viewer)
        .arg("--args-file")
        .arg(&approval_file)
        .output()
        .expect("run arca call");
    assert_eq!(code(&out), 4, "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn an_approval_for_no_waiting_plan_is_spent_once_then_refused_replayed_and_never_echoed() {
    // The approval is well formed and signed by the owner's scratch key. No plan waits, so the first use is refused as no such
    // plan (its nonce is spent), and the replay is refused as replayed. Neither result carries the approval.
    let mut node = start_node("approval-replay");
    let plan = zeros();
    let statement = statement_for(&plan, &"c".repeat(32));
    let approval = approve_statement(&node.owner, &statement);
    let args_file = node.scratch.path("approve.json");
    write_mode(
        &args_file,
        &json!({ "plan": plan, "approval": approval }).to_string(),
        0o600,
    );

    let run = |label: &str| {
        arca()
            .args(["call", "approval_submit", "--config"])
            .arg(&node.config)
            .arg("--token-file")
            .arg(&node.doctor)
            .arg("--args-file")
            .arg(&args_file)
            .output()
            .unwrap_or_else(|_| panic!("run arca call ({label})"))
    };
    let first = run("first");
    assert_eq!(
        code(&first),
        3,
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(stdout_json(&first)["data"]["refusal"], json!("NoSuchPlan"));

    let second = run("replay");
    assert_eq!(
        code(&second),
        3,
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(stdout_json(&second)["data"]["refusal"], json!("Replayed"));

    for output in [&first, &second] {
        let shown = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!shown.contains(&approval), "the approval is never echoed");
    }
    let served = node.stop();
    assert!(
        served.contains("owner signing key(s), 0 passkey(s) kept"),
        "the server's stderr is read: {served}"
    );
    assert!(
        !served.contains(&approval),
        "the server never logs the approval"
    );
}

#[test]
fn a_token_file_refused_for_its_mode_names_no_path_in_its_message() {
    let node = start_node("token-mode-path");
    let loose = node.scratch.path("loose.secret");
    let secret = fs::read_to_string(&node.doctor).expect("the minted secret");
    write_mode(&loose, &secret, 0o640);
    let out = call(&node, &loose, "arca_status", &[]);
    assert_eq!(code(&out), 2, "{}", String::from_utf8_lossy(&out.stderr));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains(&loose.display().to_string()),
        "the message does not name the file: {stderr}"
    );
}

#[test]
fn a_token_file_that_holds_no_token_names_no_path_in_its_message() {
    let node = start_node("token-shape-path");
    let not_a_token = node.scratch.path("not-a-token.secret");
    write_mode(&not_a_token, "this is not a token\n", 0o600);
    let out = call(&node, &not_a_token, "arca_status", &[]);
    assert_eq!(code(&out), 2, "{}", String::from_utf8_lossy(&out.stderr));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains(&not_a_token.display().to_string()),
        "the message does not name the file: {stderr}"
    );
}

#[test]
fn an_args_file_refused_for_its_mode_names_no_path_in_its_message() {
    let node = start_node("args-mode-path");
    let args = node.scratch.path("args.json");
    write_mode(&args, "{}", 0o604);
    let out = arca()
        .args(["call", "plan_list", "--config"])
        .arg(&node.config)
        .arg("--token-file")
        .arg(&node.viewer)
        .arg("--args-file")
        .arg(&args)
        .output()
        .expect("run arca call");
    assert_eq!(code(&out), 2, "{}", String::from_utf8_lossy(&out.stderr));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains(&args.display().to_string()),
        "the message does not name the file: {stderr}"
    );
}
