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

/// A served node, killed when the test ends.
struct Node {
    scratch: Scratch,
    serve: Child,
    config: PathBuf,
    socket: PathBuf,
    viewer: PathBuf,
    doctor: PathBuf,
    operator: PathBuf,
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
    start_node_prepared(tag, signers, passkeys, &|_, _| {})
}

/// [`start_node_keys`], with `prepare` run on the token directory after the node's three tokens are minted and
/// before the node starts.
fn start_node_prepared(
    tag: &str,
    signers: &str,
    passkeys: Option<&str>,
    prepare: &dyn Fn(&Scratch, &Path),
) -> Node {
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
    let operator = mint(&scratch, &hash_dir, "operator", "operator-1");
    prepare(&scratch, &hash_dir);

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
        operator,
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
    assert_eq!(result["mode"], json!("kernel"));
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

/// An argument the verb does not take is a tool failure, not a refusal of the call: exit 3, and its code and words on
/// stdout, read from the result's text since it has no structured content.
#[test]
fn an_argument_the_verb_does_not_take_is_a_tool_error_exit_3_with_its_code_and_words_on_stdout() {
    let node = start_node("invalid-arguments");
    let out = call(
        &node,
        &node.viewer,
        "plan_list",
        &["--args", r#"{"plna":1}"#],
    );
    assert_eq!(code(&out), 3, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        stdout_json(&out),
        json!({
            "code": "invalid_arguments",
            "message": "unknown field 'plna'; this verb takes no field"
        })
    );
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
    // The refusal names the scope the verb needs, how a token of it is made for this node, and how the node takes it.
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        format!(
            "arca: the token's scope may not make this call: token_list needs a token of the operator scope, and a token minted with --tools must list it. An owner mints one with: {}\n",
            mint_advice(&node, "operator")
        )
    );
}

#[test]
fn revoke_removes_the_token_and_a_live_serve_drops_it_without_a_restart() {
    let node = start_node("revoke");
    let before = call(&node, &node.viewer, "arca_status", &[]);
    assert_eq!(code(&before), 0);
    let out = arca()
        .args(["token", "revoke", "viewer-1", "--config"])
        .arg(&node.config)
        .output()
        .expect("run token revoke");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    thread::sleep(Duration::from_millis(300));
    let after = call(&node, &node.viewer, "arca_status", &[]);
    assert_eq!(code(&after), 4, "a revoked token is 401");
    let list = call(&node, &node.operator, "token_list", &[]);
    assert_eq!(code(&list), 0);
    let tokens = stdout_json(&list)["tokens"].clone();
    assert!(
        !tokens
            .as_array()
            .expect("tokens")
            .iter()
            .any(|token| token["id"] == "viewer-1"),
        "token_list no longer shows the revoked token"
    );
}

#[test]
fn a_token_the_server_does_not_hold_is_refused_401_and_the_call_exits_4() {
    let node = start_node("unknown-token");
    let stranger = node.scratch.path("stranger.secret");
    write_mode(&stranger, &format!("arca_{}\n", "A".repeat(43)), 0o400);
    let out = call(&node, &stranger, "arca_status", &[]);
    assert_eq!(code(&out), 4, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        format!(
            "arca: the node does not accept this token: it was minted after the node last read its tokens, it was revoked, or it is another node's. If it was just minted, have the node read it: {}. For a new one: {}\n",
            reload_command(&node),
            mint_advice(&node, "viewer")
        )
    );
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

/// A home directory as the installer leaves it for `node`: its config as `node.json` and its viewer token as
/// `assistant-1.secret`, in `.config/arca`.
fn installed_home(node: &Node) -> PathBuf {
    let home = node.scratch.path("home");
    let dir = home.join(".config/arca");
    fs::create_dir_all(&dir).expect("the installer's directory");
    fs::copy(&node.config, dir.join("node.json")).expect("the installer's config");
    let secret = fs::read_to_string(&node.viewer).expect("the minted secret");
    write_mode(&dir.join("assistant-1.secret"), &secret, 0o600);
    home
}

#[test]
fn status_with_no_flag_reads_the_installers_files_and_says_the_node_in_words() {
    let node = start_node("status-words");
    let home = installed_home(&node);
    let run = |args: &[&str]| {
        arca()
            .args(args)
            .env("HOME", &home)
            .env_remove("ARCA_CONFIG")
            .env_remove("ARCA_TOKEN_FILE")
            .output()
            .expect("run arca")
    };
    let out = run(&["status"]);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    let words = String::from_utf8_lossy(&out.stdout).into_owned();
    let lines: Vec<&str> = words.lines().collect();
    assert!(lines[0].starts_with("Automagik Arca "), "{words}");
    assert!(lines[1].starts_with("Mode: "), "{words}");
    assert!(lines[2].starts_with("Up for: "), "{words}");
    assert!(lines[3].starts_with("Incus: "), "{words}");
    assert!(
        words.contains(&format!(
            "MCP door: open, on the unix socket {}",
            node.socket.display()
        )),
        "{words}"
    );
    // The last two lines: the ONE thing to do next, and the one pointer to everything else, with the config it read.
    assert!(
        lines.len() >= 2 && lines[lines.len() - 2].starts_with("Next: "),
        "{words}"
    );
    assert_eq!(words.matches("Next: ").count(), 1, "{words}");
    assert!(
        lines
            .last()
            .is_some_and(|line| line.starts_with("More: arca status --security --config ")),
        "{words}"
    );

    // --json is the tool's own answer: the same document `arca call arca_status` prints, with no flag either.
    let raw = stdout_json(&run(&["status", "--json"]));
    let called = stdout_json(&run(&["call", "arca_status"]));
    let keys = |value: &Value| -> Vec<String> {
        value
            .as_object()
            .expect("an object")
            .keys()
            .cloned()
            .collect()
    };
    assert_eq!(keys(&raw), keys(&called));
    for key in ["version", "node", "mode", "incusReachable", "appliedSeq"] {
        assert_eq!(raw[key], called[key], "{key}");
    }
    assert_eq!(
        words.lines().nth(1),
        Some(
            format!(
                "Mode: {}",
                match raw["mode"].as_str() {
                    Some("watch-only") => "watch-only (it looks, and changes nothing)".to_string(),
                    other => format!("{} (it may make changes)", other.unwrap_or("unknown")),
                }
            )
            .as_str()
        )
    );

    // An explicit flag still wins over the installer's file: a token file that is not there is refused, not replaced.
    let out = run(&["status", "--token-file", "/nonexistent/arca-test.secret"]);
    assert_eq!(code(&out), 2);
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        format!(
            "arca: the token file does not exist: mint one with: arca token mint --scope viewer --id TOKEN_ID --out TOKEN_FILE --config {installed} (a name for the token in place of TOKEN_ID, and the path to write it to in place of TOKEN_FILE). A node reads its tokens only when it starts or on SIGHUP, so a node that is running needs this after the mint: arca token reload --config {installed}\n",
            installed = home.join(".config/arca/node.json").display()
        )
    );
}

/// A home as an upgraded node leaves it: its config, and each of `files` (a name and the secret file to copy) beside it
/// at mode 0600. No `assistant-1.secret`.
fn upgraded_home(node: &Node, name: &str, files: &[(&str, &Path)]) -> PathBuf {
    let home = node.scratch.path(name);
    let dir = home.join(".config/arca");
    fs::create_dir_all(&dir).expect("the config directory");
    fs::copy(&node.config, dir.join("node.json")).expect("the config");
    for (file, from) in files {
        let secret = fs::read_to_string(from).expect("the minted secret");
        write_mode(&dir.join(file), &secret, 0o600);
    }
    home
}

/// `arca status` in `home`, with what `with` adds to the command.
fn status_in(home: &Path, with: &dyn Fn(&mut Command)) -> Output {
    let mut command = arca();
    command.arg("status").env("HOME", home);
    with(&mut command);
    command.output().expect("run arca")
}

/// The fallback to a viewer token is `arca status`'s own step when nothing named a token file and the installer's is
/// not there, and no other. A file the flag or the variable names is read alone, even when it is missing and a viewer
/// token sits beside it; and an installer's file that is there but refused keeps its own refusal, so the owner fixes it.
#[test]
fn status_reads_a_named_token_file_alone_and_a_broken_installers_file_keeps_its_refusal() {
    let node = start_node("status-named-alone");
    let home = upgraded_home(&node, "named-home", &[("viewer.secret", &node.viewer)]);
    let dir = home.join(".config/arca");
    let missing = dir.join("missing.secret");
    for by_variable in [false, true] {
        let out = status_in(&home, &|command| {
            if by_variable {
                command.env("ARCA_TOKEN_FILE", &missing);
            } else {
                command.arg("--token-file").arg(&missing);
            }
        });
        let said = String::from_utf8_lossy(&out.stderr).into_owned();
        assert_eq!(code(&out), 2, "variable {by_variable}: {said}");
        assert!(
            said.starts_with("arca: the token file does not exist: "),
            "variable {by_variable}: {said}"
        );
    }
    // The installer's file is there, but others may read it: its own refusal, not the viewer token beside it.
    let installers = dir.join("assistant-1.secret");
    let secret = fs::read_to_string(&node.viewer).expect("the minted secret");
    write_mode(&installers, &secret, 0o644);
    let out = status_in(&home, &|_| {});
    let said = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(code(&out), 2, "{said}");
    assert!(
        said.contains("can be read by its group or by others"),
        "{said}"
    );
    // It is there and private, but holds no token: its own refusal again.
    write_mode(&installers, "not a token\n", 0o600);
    let out = status_in(&home, &|_| {});
    let said = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(code(&out), 2, "{said}");
    assert!(said.contains("does not hold a token"), "{said}");
}

/// A viewer token minted with a `--tools` list that leaves out `arca_status` is passed over: the node would refuse it.
/// It sorts before the viewer token that may ask, so a status that took it would be refused with 403.
#[test]
fn status_passes_over_a_viewer_token_whose_tools_leave_out_arca_status() {
    let narrow = std::cell::RefCell::new(PathBuf::new());
    let node = start_node_prepared(
        "status-narrow",
        &format!("{}\n", SkKey::from_seed(1).line(NAMESPACE)),
        None,
        &|scratch, hash_dir| {
            let out = scratch.path("a-narrow.secret");
            let minted = arca()
                .args([
                    "token",
                    "mint",
                    "--scope",
                    "viewer",
                    "--id",
                    "a-narrow",
                    "--tools",
                    "service_list",
                ])
                .arg("--hash-dir")
                .arg(hash_dir)
                .arg("--out")
                .arg(&out)
                .output()
                .expect("run token mint");
            assert!(minted.status.success(), "minting a-narrow succeeds");
            *narrow.borrow_mut() = out;
        },
    );
    let narrow = narrow.into_inner();
    let home = upgraded_home(
        &node,
        "narrow-home",
        &[
            ("a-narrow.secret", &narrow),
            ("viewer.secret", &node.viewer),
        ],
    );
    let out = status_in(&home, &|_| {});
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
}

/// A viewer token the node does not accept (minted after the node read its tokens) is refused as any token is, and the
/// refusal says which file `arca status` found, by its name, since the owner never named one. No secret is printed.
#[test]
fn a_refusal_of_the_token_status_found_names_its_file() {
    let node = start_node("status-found-refused");
    let late = mint(
        &node.scratch,
        &node.scratch.path("tokens"),
        "viewer",
        "late-1",
    );
    let home = upgraded_home(&node, "late-home", &[("late.secret", &late)]);
    let out = status_in(&home, &|_| {});
    let said = String::from_utf8_lossy(&out.stderr).into_owned();
    // First, with nothing of the output in the message: no secret is printed.
    let secret = fs::read_to_string(&late).expect("the minted secret");
    let printed = format!("{said}{}", String::from_utf8_lossy(&out.stdout));
    assert!(
        !printed.contains(secret.trim_end()),
        "the secret is printed"
    );
    assert_eq!(code(&out), 4, "{said}");
    assert!(
        said.starts_with("arca: the installer's token file assistant-1.secret is not there, so arca status used the viewer token in late.secret beside it, and the node does not accept this token"),
        "{said}"
    );
}

/// A node installed before the installer named its token `assistant-1.secret` keeps its viewer token beside the
/// config under another name. `arca status` with no flag finds that one, and the first command after an upgrade answers
/// instead of telling the owner to mint a token. A doctor token beside it is never taken in a viewer's place.
#[test]
fn status_on_an_upgraded_node_uses_the_viewer_token_beside_the_config() {
    let node = start_node("status-upgraded");
    let home = node.scratch.path("upgraded-home");
    let dir = home.join(".config/arca");
    fs::create_dir_all(&dir).expect("the config directory");
    fs::copy(&node.config, dir.join("node.json")).expect("the config");
    let copy = |from: &Path, name: &str| {
        let secret = fs::read_to_string(from).expect("the minted secret");
        write_mode(&dir.join(name), &secret, 0o600);
    };
    copy(&node.doctor, "doctor.secret");
    // Ordinary files beside it are passed over: a note and a file that holds no token.
    write_mode(&dir.join("notes.txt"), "not a token\n", 0o600);
    write_mode(&dir.join("broken.secret"), "not a token\n", 0o600);
    let status = || {
        arca()
            .arg("status")
            .env("HOME", &home)
            .output()
            .expect("run arca")
    };
    // A doctor token alone: no viewer is beside the config, so the refusal is the one it always was.
    let out = status();
    assert_eq!(code(&out), 2, "{}", String::from_utf8_lossy(&out.stderr));
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains("assistant-1.secret, where the installer writes it, does not exist"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The viewer's token under its older name: status answers.
    copy(&node.viewer, "viewer.secret");
    let out = status();
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert!(
        String::from_utf8_lossy(&out.stdout).starts_with("Automagik Arca "),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// How long a test waits for `serve` to write its pid file, and for a reload to take effect: generous, since the test
/// machine may be busy and neither is a judgement of speed.
const PID_WAIT: Duration = Duration::from_secs(120);

/// Waits until `node`'s `serve.pid` exists and holds a pid. `serve` writes it from a task that runs after the socket is
/// bound, so a node that already answers calls may not have written it yet.
fn wait_for_pid_file(node: &Node) {
    let path = node.scratch.path("state").join("serve.pid");
    let deadline = Instant::now() + PID_WAIT;
    loop {
        let pid = fs::read_to_string(&path)
            .ok()
            .and_then(|text| text.trim().parse::<u32>().ok());
        if pid.is_some() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "serve writes {} with its pid within the deadline",
            path.display()
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// The command a refusal gives to make `node` read its tokens again.
fn reload_command(node: &Node) -> String {
    format!("arca token reload --config {}", node.config.display())
}

/// What a refusal says about minting a token of `scope` for `node`.
fn mint_advice(node: &Node, scope: &str) -> String {
    format!(
        "arca token mint --scope {scope} --id TOKEN_ID --out TOKEN_FILE --config {} (a name for the token in place of TOKEN_ID, and the path to write it to in place of TOKEN_FILE). A node reads its tokens only when it starts or on SIGHUP, so a node that is running needs this after the mint: {}",
        node.config.display(),
        reload_command(node)
    )
}

#[test]
fn a_token_minted_while_the_node_runs_is_refused_until_the_reload_the_refusal_names() {
    let node = start_node("mint-reload");
    // The mint the 403 and 401 texts give, with this node's config: it writes the hash and sends no signal.
    let fresh = node.scratch.path("fresh.secret");
    let minted = arca()
        .args([
            "token", "mint", "--scope", "viewer", "--id", "fresh-1", "--out",
        ])
        .arg(&fresh)
        .arg("--config")
        .arg(&node.config)
        .output()
        .expect("run token mint");
    assert_eq!(
        code(&minted),
        0,
        "{}",
        String::from_utf8_lossy(&minted.stderr)
    );
    let before = call(&node, &fresh, "arca_status", &[]);
    assert_eq!(
        code(&before),
        4,
        "the running node has not read the new token"
    );
    let refusal = String::from_utf8_lossy(&before.stderr).into_owned();
    assert!(
        refusal.contains("it was minted after the node last read its tokens"),
        "the refusal lists the true cause: {refusal}"
    );
    let reload = reload_command(&node);
    assert!(
        refusal.contains(&reload),
        "the refusal names the reload: {refusal}"
    );

    // The reload the refusal names, run at once: the node is found whether or not its pid file is there yet.
    let ran = token_reload(&node);
    assert_eq!(code(&ran), 0, "{}", String::from_utf8_lossy(&ran.stderr));
    assert_eq!(
        String::from_utf8_lossy(&ran.stdout),
        format!(
            "reloaded: the node on this config (pid {}) read its tokens again and holds 4 token(s)\n",
            node.serve.id()
        )
    );
    // A bound, not a speed judgement: the node reloads in its own time.
    let until = Instant::now() + PID_WAIT;
    let mut after = call(&node, &fresh, "arca_status", &[]);
    while code(&after) != 0 && Instant::now() < until {
        thread::sleep(Duration::from_millis(100));
        after = call(&node, &fresh, "arca_status", &[]);
    }
    assert_eq!(
        code(&after),
        0,
        "{}",
        String::from_utf8_lossy(&after.stderr)
    );
}

/// The name an approved action runs under: no token's.
const APPROVED: &str = "owner-approval";

/// Puts a whole, well-formed token file under the name approved actions run under, as a release that still let one
/// be minted would have left it: a token minted under another id, then renamed. Returns its secret file.
fn old_reserved_token(scratch: &Scratch, hash_dir: &Path, id: &str) -> PathBuf {
    let secret = mint(scratch, hash_dir, "operator", id);
    fs::rename(
        hash_dir.join(format!("{id}.hash")),
        hash_dir.join(format!("{APPROVED}.hash")),
    )
    .expect("rename the hash file");
    secret
}

/// A token store that holds a file under the name approved actions run under, left by an older release. The node
/// starts; its other tokens work; the secret of that file is refused exactly as a secret no file holds, so the
/// answer tells a caller nothing about the name; and the node says once, each time it reads its tokens, that the
/// file is ignored and how it is removed. `token revoke` removes it.
#[test]
fn a_token_file_under_the_name_of_approved_actions_is_ignored_and_the_node_says_so_once_a_load() {
    let owner = SkKey::from_seed(1);
    let mut node = start_node_prepared(
        "reserved-token",
        &format!("{}\n", owner.line(NAMESPACE)),
        None,
        &|scratch, hash_dir| {
            old_reserved_token(scratch, hash_dir, "old-1");
        },
    );
    let old = node.scratch.path("old-1.secret");
    let hash_dir = node.scratch.path("tokens");
    assert!(hash_dir.join(format!("{APPROVED}.hash")).exists());
    // A secret of the right shape that no file holds: what any unknown token is told.
    let elsewhere = node.scratch.path("elsewhere");
    fs::create_dir(&elsewhere).expect("another token directory");
    fs::set_permissions(&elsewhere, fs::Permissions::from_mode(0o700)).expect("mode 0700");
    let unknown = mint(&node.scratch, &elsewhere, "operator", "nobody");
    let stranger = call(&node, &unknown, "arca_status", &[]);
    assert_eq!(code(&stranger), 4);
    let refused_as_unknown = |node: &Node| {
        let answer = call(node, &old, "arca_status", &[]);
        assert_eq!(code(&answer), code(&stranger));
        assert_eq!(
            String::from_utf8_lossy(&answer.stderr),
            String::from_utf8_lossy(&stranger.stderr),
            "the answer is the one an unknown token gets"
        );
        assert_eq!(answer.stdout, stranger.stdout);
    };
    refused_as_unknown(&node);
    let good = call(&node, &node.doctor, "arca_status", &[]);
    assert_eq!(code(&good), 0, "{}", String::from_utf8_lossy(&good.stderr));

    // A reload reads the directory again: the same three tokens, the same file left out.
    let ran = token_reload(&node);
    assert_eq!(code(&ran), 0, "{}", String::from_utf8_lossy(&ran.stderr));
    assert_eq!(
        String::from_utf8_lossy(&ran.stdout),
        format!(
            "reloaded: the node on this config (pid {}) read its tokens again and holds 3 token(s)\n",
            node.serve.id()
        )
    );
    refused_as_unknown(&node);
    let good = call(&node, &node.viewer, "arca_status", &[]);
    assert_eq!(code(&good), 0, "{}", String::from_utf8_lossy(&good.stderr));

    // No token is minted under the name, and the refusal says why.
    let minted = arca()
        .args([
            "token", "mint", "--scope", "viewer", "--id", APPROVED, "--out",
        ])
        .arg(node.scratch.path("never.secret"))
        .arg("--config")
        .arg(&node.config)
        .output()
        .expect("run token mint");
    assert_eq!(
        code(&minted),
        2,
        "{}",
        String::from_utf8_lossy(&minted.stderr)
    );
    assert!(
        String::from_utf8_lossy(&minted.stderr).contains(
            "--id owner-approval is the name an action runs under once the owner approved it, and no token takes it: choose another id"
        ),
        "{}",
        String::from_utf8_lossy(&minted.stderr)
    );
    assert!(!node.scratch.path("never.secret").exists());

    // The file is removed with the command the node's line names.
    let revoked = arca()
        .args(["token", "revoke", APPROVED, "--config"])
        .arg(&node.config)
        .output()
        .expect("run token revoke");
    assert_eq!(
        code(&revoked),
        0,
        "{}",
        String::from_utf8_lossy(&revoked.stderr)
    );
    assert!(!hash_dir.join(format!("{APPROVED}.hash")).exists());

    let log = node.stop();
    let line = "arca: token file owner-approval.hash is ignored: it uses the name an action runs under once the owner approved it, so no token of that name is accepted. Remove it with: arca token revoke owner-approval";
    assert_eq!(
        log.matches(line).count(),
        2,
        "once at the start and once at the reload, and not after the file is gone: {log}"
    );
}

/// `arca token reload --config <the node's config>`.
fn token_reload(node: &Node) -> Output {
    arca()
        .args(["token", "reload", "--config"])
        .arg(&node.config)
        .output()
        .expect("run token reload")
}

#[test]
fn token_reload_finds_a_running_node_whose_pid_file_is_not_there() {
    let node = start_node("reload-no-pid-file");
    // The file a node has not written yet, or could not write, is the same to the command: no file.
    wait_for_pid_file(&node);
    let pid_file = node.scratch.path("state").join("serve.pid");
    fs::remove_file(&pid_file).expect("remove the pid file");
    let out = token_reload(&node);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!(
            "reloaded: the node on this config (pid {}) read its tokens again and holds 3 token(s)\n",
            node.serve.id()
        )
    );
    assert!(out.stderr.is_empty());
}

#[test]
fn token_reload_follows_no_stale_pid_file_and_still_finds_the_node_itself() {
    let node = start_node("reload-stale-file");
    wait_for_pid_file(&node);
    // The file names a stranger: another program of this user. The node itself still runs on the config.
    let mut stranger = Command::new("sleep")
        .arg("300")
        .spawn()
        .expect("a stranger process");
    let pid_file = node.scratch.path("state").join("serve.pid");
    fs::write(&pid_file, format!("{}\n", stranger.id())).expect("the stale pid file");
    let out = token_reload(&node);
    let alive = stranger.try_wait().expect("poll the stranger").is_none();
    let _ = stranger.kill();
    let _ = stranger.wait();
    assert!(alive, "the stranger was not signalled");
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert!(
        String::from_utf8_lossy(&out.stdout).starts_with(&format!(
            "reloaded: the node on this config (pid {}) ",
            node.serve.id()
        )),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn token_reload_says_so_when_the_node_could_not_read_its_tokens_and_exits_non_zero() {
    let node = start_node("reload-refused");
    // A token directory others may write is one the node refuses to read, so it keeps the tokens it had.
    let hash_dir = node.scratch.path("tokens");
    fs::set_permissions(&hash_dir, fs::Permissions::from_mode(0o777)).expect("mode 0777");
    let out = token_reload(&node);
    fs::set_permissions(&hash_dir, fs::Permissions::from_mode(0o700)).expect("mode 0700");
    assert_eq!(code(&out), 1, "{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.stdout.is_empty(), "nothing is claimed on stdout");
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        format!(
            "arca: not reloaded: the node on this config (pid {}) could not read its token directory and keeps the tokens it had. Its own log says why\n",
            node.serve.id()
        )
    );
    let still = call(&node, &node.viewer, "arca_status", &[]);
    assert_eq!(code(&still), 0, "the node keeps the tokens it had");
}

#[test]
fn a_hangup_that_arrives_while_the_node_starts_is_answered_and_its_record_stays() {
    let mut node = start_node("reload-during-start");
    // The same node, started again; this time a hangup is sent at the first moment the new process can take one.
    node.stop();
    let serve = arca()
        .args(["serve", "--config"])
        .arg(&node.config)
        .env(
            "ARCA_TEST_OWNER_SIGNERS",
            node.scratch.path("owner-signers"),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start arca serve again");
    node.serve = serve;
    let pid = node.serve.id();
    let deadline = Instant::now() + PID_WAIT;
    let catches = || {
        fs::read_to_string(format!("/proc/{pid}/status"))
            .ok()
            .and_then(|status| {
                let mask = status
                    .lines()
                    .find_map(|line| line.strip_prefix("SigCgt:"))?;
                u64::from_str_radix(mask.trim(), 16).ok()
            })
            .is_some_and(|mask| mask & 1 == 1)
    };
    while !catches() {
        assert!(Instant::now() < deadline, "serve takes SIGHUP as it starts");
        thread::yield_now();
    }
    let sent = Command::new("kill")
        .args(["-HUP", &pid.to_string()])
        .status()
        .expect("send the hangup");
    assert!(sent.success());
    wait_for(&mut node);

    // The node answered the hangup with one reload once it was up, and the record of it is still there: it was not
    // taken for an older node's record and removed when the socket was bound.
    let record = node.scratch.path("state").join("serve.reload");
    let want = format!("{pid} 1 ok 3\n");
    while fs::read_to_string(&record).ok().as_deref() != Some(want.as_str()) {
        assert!(
            Instant::now() < deadline,
            "the record of the reload stays: {:?}",
            fs::read_to_string(&record)
        );
        thread::sleep(Duration::from_millis(20));
    }
    // A reload asked for now is the node's second, and is told apart from the first.
    let out = token_reload(&node);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        fs::read_to_string(&record).expect("the record"),
        format!("{pid} 2 ok 3\n")
    );
}
