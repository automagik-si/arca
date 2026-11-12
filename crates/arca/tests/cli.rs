//! The command line of the `arca` binary: usage, the config refusals, the token mint modes, the one-shot reads through the
//! recording shim, and the shim command. Each test runs the built binary under its own scratch directory in the default
//! temporary directory. Nothing here reaches a real Incus socket, a tailnet address or a system service: every Incus socket
//! is a fake behind a recording shim, and a refused config never gets as far as a socket.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arca_api::{conforms, verb};
use arca_incus::{FakeIncus, FakeState, RecordingShim, ShimHandle};
use arca_mcp::TokenStore;
use serde_json::{Value, json};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../arca-incus/tests/fixtures/state.json"
);
const WAIT: Duration = Duration::from_secs(10);

/// A fresh directory under the default temporary directory, removed when the test ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .subsec_nanos();
        let dir =
            std::env::temp_dir().join(format!("arca-cli-{}-{nanos}-{tag}", std::process::id()));
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

/// The binary, with no home directory and no variable to take a config or a token from: a test that names neither must
/// never read the files of the user who runs the tests.
fn arca() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_arca"));
    command
        .env("HOME", "/nonexistent/arca-test-home")
        .env_remove("ARCA_CONFIG")
        .env_remove("ARCA_TOKEN_FILE")
        // No screen, unless a test says there is one: what `init` and `status` say next depends on it, and must not
        // depend on where the tests run.
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY");
    command
}

/// A home directory under `scratch` as the installer leaves it: `.config/arca`, with `node.json` when `config` is given.
fn installed_home(scratch: &Scratch, config: Option<&Value>) -> PathBuf {
    let home = scratch.at("home");
    let dir = home.join(".config/arca");
    fs::create_dir_all(&dir).expect("the installer's directory");
    if let Some(config) = config {
        fs::write(dir.join("node.json"), config.to_string()).expect("the installer's config");
    }
    home
}

/// What a test's node is started through: a shell that closes every file it was handed but its three standard ones,
/// then becomes the program. A test process may hold files that are not its own to pass on, such as the descriptor of a
/// lock its runner took; a node that inherited one would keep it for as long as it lived.
const CLOSE_THEN_EXEC: &str = r#"for fd in /proc/$$/fd/*; do fd=${fd##*/}; case $fd in 0|1|2) ;; *) eval "exec $fd>&-" 2>/dev/null ;; esac; done; exec "$0" "$@""#;

/// `arca serve --config <config>` as a command to spawn, with no file of the test's process but its standard three.
fn serve_command(config: &Path) -> Command {
    let mut command = Command::new("bash");
    command
        .arg("-c")
        .arg(CLOSE_THEN_EXEC)
        .arg(env!("CARGO_BIN_EXE_arca"))
        .env("HOME", "/nonexistent/arca-test-home")
        .env_remove("ARCA_CONFIG")
        .env_remove("ARCA_TOKEN_FILE")
        .args(["serve", "--config"])
        .arg(config);
    command
}

/// A node a test started. It is ended when this value goes away, on every path out of the test, a panic among them:
/// killed, and waited for, so no node outlives its test.
struct Node(std::process::Child);

impl std::ops::Deref for Node {
    type Target = std::process::Child;

    fn deref(&self) -> &std::process::Child {
        &self.0
    }
}

impl std::ops::DerefMut for Node {
    fn deref_mut(&mut self) -> &mut std::process::Child {
        &mut self.0
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn stdout_text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr_text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// A fake Incus serving the fixture, and a recording shim in front of it.
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

/// A node config that reads through `socket`, names one identity phone, and keeps the kernel out of every project.
fn config(scratch: &Scratch, socket: &Path) -> Value {
    json!({
        "stateDir": scratch.at("state").display().to_string(),
        "incus": { "socket": socket.display().to_string(), "pools": [] },
        "identityPhones": ["phone1"],
        "listen": { "unix": scratch.at("arca.sock").display().to_string(), "tailnet": null },
        "tokens": { "hashDir": scratch.at("tokens").display().to_string() },
        "kernel": {
            "projects": [],
            "scratch": false,
            "droiddProjects": ["droid"],
            "reservedCidrs": []
        }
    })
}

fn write_config(scratch: &Scratch, name: &str, value: &Value) -> PathBuf {
    let path = scratch.at(name);
    fs::write(&path, value.to_string()).expect("write the config");
    path
}

/// The shim's log, one parsed line per request.
fn log_lines(log: &Path) -> Vec<Value> {
    fs::read_to_string(log)
        .expect("read the shim log")
        .lines()
        .map(|line| serde_json::from_str(line).expect("a log line is JSON"))
        .collect()
}

/// Every logged request is a GET that the shim forwarded, and none was refused.
fn assert_only_gets(log: &Path) {
    let lines = log_lines(log);
    assert!(!lines.is_empty(), "the shim logged its requests");
    for line in &lines {
        assert_eq!(
            line["method"],
            json!("GET"),
            "a logged request is a GET: {line}"
        );
        assert_eq!(
            line["forwarded"],
            json!(true),
            "a logged request was forwarded: {line}"
        );
    }
}

fn conforming(verb_name: &str, value: &Value) {
    let verb = verb(verb_name).expect("the verb is in the table");
    if let Err(why) = conforms(&(verb.result)(), value) {
        panic!("{verb_name} does not conform: {why}\n{value}");
    }
}

#[test]
fn usage_errors_exit_2_and_say_what_was_wrong() {
    let scratch = Scratch::new("usage");
    let config_path = write_config(
        &scratch,
        "node.json",
        &config(&scratch, &scratch.at("incus.sock")),
    );
    let config_arg = config_path.display().to_string();
    let cases: Vec<Vec<&str>> = vec![
        vec![],
        vec!["frobnicate"],
        vec!["watch", "--json", "--config", &config_arg],
        vec!["watch", "--once", "--config", &config_arg],
        vec!["watch", "--once", "--json", "--bogus", "x"],
        vec!["metrics", "--sample-for", "5", "--config", &config_arg],
        vec![
            "metrics",
            "--json",
            "--sample-for",
            "61",
            "--config",
            &config_arg,
        ],
        vec![
            "metrics",
            "--json",
            "--sample-for",
            "soon",
            "--config",
            &config_arg,
        ],
        vec!["serve"],
        vec!["token", "rotate"],
        vec![
            "token",
            "mint",
            "--scope",
            "root",
            "--id",
            "x",
            "--hash-dir",
            ".",
            "--out",
            "y",
        ],
        vec![
            "token",
            "mint",
            "--scope",
            "viewer",
            "--id",
            "Bad_ID",
            "--hash-dir",
            ".",
            "--out",
            "y",
        ],
        vec!["shim", "--upstream", "a", "--listen", "b"],
        vec![
            "shim",
            "--upstream",
            "a",
            "--listen",
            "b",
            "--log",
            "c",
            "--log-max-bytes",
            "0",
        ],
        vec![
            "shim",
            "--upstream",
            "a",
            "--listen",
            "b",
            "--log",
            "c",
            "--log-max-bytes",
            "lots",
        ],
    ];
    for args in cases {
        let out = arca().args(&args).output().expect("run the binary");
        assert_eq!(out.status.code(), Some(2), "{args:?} is a usage error");
        assert!(out.stdout.is_empty(), "{args:?} prints nothing on stdout");
        assert!(
            !stderr_text(&out).is_empty(),
            "{args:?} says what was wrong"
        );
    }
}

#[test]
fn version_prints_the_product_name_the_version_the_commit_and_the_target_and_exits_0() {
    let mut lines = Vec::new();
    for spelling in ["--version", "version", "-V"] {
        let out = arca().arg(spelling).output().expect("run the binary");
        assert_eq!(out.status.code(), Some(0), "{spelling} exits 0");
        assert!(out.stderr.is_empty(), "{spelling} prints nothing on stderr");
        lines.push(stdout_text(&out));
    }
    assert!(
        lines.iter().all(|line| line == &lines[0]),
        "every spelling prints the same line: {lines:?}"
    );
    let line = lines[0].strip_suffix('\n').expect("one line");
    assert!(!line.contains('\n'), "one line only: {line:?}");

    // "Automagik Arca <version> (<short commit>, <target triple>)"
    let prefix = concat!("Automagik Arca ", env!("CARGO_PKG_VERSION"), " (");
    let inside = line
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_suffix(')'))
        .unwrap_or_else(|| panic!("{line:?} starts with {prefix:?} and ends with a bracket"));
    let (commit, target) = inside.split_once(", ").expect("a commit and a target");
    let hex = (7..=40).contains(&commit.len())
        && commit
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    assert!(
        hex || commit == "unknown",
        "{commit:?} is a short commit, or unknown for a build with no git checkout"
    );
    assert!(
        target.split('-').count() >= 3 && target.starts_with(std::env::consts::ARCH),
        "{target:?} is this build's target triple"
    );

    // The usage text lists the command, and the command takes no argument.
    let help = arca().arg("--help").output().expect("run the binary");
    assert_eq!(help.status.code(), Some(0));
    assert!(
        stdout_text(&help).contains("\n  version (or --version, -V)\n"),
        "the usage text lists version"
    );
    let extra = arca()
        .args(["--version", "now"])
        .output()
        .expect("run the binary");
    assert_eq!(extra.status.code(), Some(2), "version takes no argument");
    assert!(extra.stdout.is_empty());
}

#[test]
fn sample_for_takes_up_to_60_seconds_and_refuses_61() {
    // A sampling run of 60 seconds would take a minute, so the test stops at the config: a refused config exits 2 before any
    // sample is taken. The length is checked first, so a length the command accepts gets past it and reaches the config
    // refusal, and a length it refuses says so on stderr.
    let scratch = Scratch::new("sample-for");
    let path = write_config(&scratch, "refused.json", &json!({ "bogus": 1 }));
    for seconds in ["60", "61"] {
        let out = arca()
            .args(["metrics", "--json", "--sample-for", seconds, "--config"])
            .arg(&path)
            .output()
            .expect("run the binary");
        assert_eq!(out.status.code(), Some(2), "{seconds}: exit 2");
        assert!(out.stdout.is_empty(), "{seconds}: nothing on stdout");
        let stderr = stderr_text(&out);
        let refused_by_length = stderr.contains("--sample-for");
        match seconds {
            "60" => assert!(
                !refused_by_length,
                "60 seconds is accepted and the config is what refuses: {stderr}"
            ),
            _ => assert!(
                refused_by_length,
                "61 seconds is refused by its length: {stderr}"
            ),
        }
    }
}

#[test]
fn a_config_the_kernel_must_not_start_with_is_refused_before_any_read() {
    let scratch = Scratch::new("refuse");
    // No Incus socket exists at this path: a refusal must come before any read, so the run never reaches it.
    let base = config(&scratch, &scratch.at("incus.sock"));
    let with = |edit: &dyn Fn(&mut Value)| {
        let mut value = base.clone();
        edit(&mut value);
        value
    };
    let cases: Vec<(&str, Value)> = vec![
        ("an unknown top-level key", with(&|v| v["bogus"] = json!(1))),
        (
            "an unknown key inside incus",
            with(&|v| v["incus"]["extra"] = json!(1)),
        ),
        (
            "a tailnet address of 0.0.0.0",
            with(&|v| v["listen"]["tailnet"] = json!("0.0.0.0:7450")),
        ),
        (
            "a tailnet address on a reserved port",
            with(&|v| v["listen"]["tailnet"] = json!("100.64.0.1:443")),
        ),
        (
            "a tailnet address outside the tailnet ranges",
            with(&|v| v["listen"]["tailnet"] = json!("198.51.100.7:7450")),
        ),
        (
            "a proxy address of 0.0.0.0",
            with(&|v| {
                v["listen"]["proxy"] = json!({ "addr": "0.0.0.0:7460", "peer": "192.0.2.20" })
            }),
        ),
        (
            "a proxy address of [::]",
            with(&|v| v["listen"]["proxy"] = json!({ "addr": "[::]:7460", "peer": "192.0.2.20" })),
        ),
        (
            "a proxy address in the tailnet ranges",
            with(&|v| {
                v["listen"]["proxy"] = json!({ "addr": "100.64.0.1:7460", "peer": "192.0.2.20" })
            }),
        ),
        (
            "a proxy address on port 443",
            with(&|v| {
                v["listen"]["proxy"] = json!({ "addr": "192.0.2.1:443", "peer": "192.0.2.20" })
            }),
        ),
        (
            "a proxy address on port 7443",
            with(&|v| {
                v["listen"]["proxy"] = json!({ "addr": "192.0.2.1:7443", "peer": "192.0.2.20" })
            }),
        ),
        (
            "a proxy address on port 0",
            with(&|v| {
                v["listen"]["proxy"] = json!({ "addr": "192.0.2.1:0", "peer": "192.0.2.20" })
            }),
        ),
        (
            "a proxy peer that is a wildcard",
            with(&|v| {
                v["listen"]["proxy"] = json!({ "addr": "192.0.2.1:7460", "peer": "0.0.0.0" })
            }),
        ),
        (
            "a proxy with no peer",
            with(&|v| v["listen"]["proxy"] = json!({ "addr": "192.0.2.1:7460" })),
        ),
        (
            "a loopback address that is not loopback",
            with(&|v| v["listen"]["loopback"] = json!("192.0.2.1:7471")),
        ),
        (
            "a loopback address of 0.0.0.0",
            with(&|v| v["listen"]["loopback"] = json!("0.0.0.0:7471")),
        ),
        (
            "a loopback address on a reserved port",
            with(&|v| v["listen"]["loopback"] = json!("127.0.0.1:443")),
        ),
        (
            "a proxy with an unknown key",
            with(&|v| {
                v["listen"]["proxy"] =
                    json!({ "addr": "192.0.2.1:7460", "peer": "192.0.2.20", "tls": true });
            }),
        ),
        (
            "a kernel project named default",
            with(&|v| v["kernel"]["projects"] = json!(["default"])),
        ),
        (
            "a kernel project named bench",
            with(&|v| v["kernel"]["projects"] = json!(["bench"])),
        ),
        (
            "a kernel project named as a droidd project",
            with(&|v| v["kernel"]["projects"] = json!(["droid"])),
        ),
        (
            "kernel projects with no droidd projects listed",
            with(&|v| {
                v["kernel"]["projects"] = json!(["arcak-svc"]);
                v["kernel"]["droiddProjects"] = json!([]);
            }),
        ),
        (
            "scratch with a project outside the scratch prefix",
            with(&|v| {
                v["kernel"]["scratch"] = json!(true);
                v["kernel"]["projects"] = json!(["arcak-svc"]);
            }),
        ),
        (
            "a config with no droiddProjects key",
            with(&|v| {
                v["kernel"]
                    .as_object_mut()
                    .expect("kernel object")
                    .remove("droiddProjects");
            }),
        ),
    ];
    for (index, (name, value)) in cases.into_iter().enumerate() {
        let path = write_config(&scratch, &format!("case-{index}.json"), &value);
        let out = arca()
            .args(["watch", "--once", "--json", "--config"])
            .arg(&path)
            .output()
            .expect("run the binary");
        assert_eq!(
            out.status.code(),
            Some(2),
            "{name}: refused with exit 2\n{}",
            stderr_text(&out)
        );
        assert!(out.stdout.is_empty(), "{name}: nothing on stdout");
    }
}

#[test]
fn a_valid_config_with_a_kernel_project_a_tailnet_and_a_proxy_address_is_accepted() {
    // The control for the refusals above: the same shape, made valid, runs to a result.
    let scratch = Scratch::new("accept");
    let rig = rig(&scratch);
    let mut value = config(&scratch, rig.shim.socket());
    value["listen"]["tailnet"] = json!("100.64.0.1:7450");
    value["listen"]["proxy"] = json!({ "addr": "192.0.2.1:7460", "peer": "192.0.2.20" });
    value["kernel"]["projects"] = json!(["arcak-svc", "arcak-scratch-probe"]);
    value["kernel"]["scratch"] = json!(false);
    let path = write_config(&scratch, "node.json", &value);
    let out = arca()
        .args(["metrics", "--json", "--sample-for", "0", "--config"])
        .arg(&path)
        .output()
        .expect("run the binary");
    assert!(
        out.status.success(),
        "a valid config runs: {}",
        stderr_text(&out)
    );
    let summary = rig.shim.stop().expect("stop the shim");
    assert_eq!(summary.refused, 0, "the shim refused nothing");
}

#[test]
fn token_mint_writes_both_files_at_mode_0400_and_prints_no_secret() {
    let scratch = Scratch::new("mint");
    let hash_dir = scratch.at("tokens");
    fs::create_dir(&hash_dir).expect("create the token directory");
    fs::set_permissions(&hash_dir, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
    let out_file = scratch.at("viewer-1.secret");
    let out = arca()
        .args([
            "token",
            "mint",
            "--scope",
            "viewer",
            "--id",
            "viewer-1",
            "--hash-dir",
        ])
        .arg(&hash_dir)
        .arg("--out")
        .arg(&out_file)
        .output()
        .expect("run the binary");
    assert!(
        out.status.success(),
        "the mint succeeds: {}",
        stderr_text(&out)
    );

    let secret = fs::read_to_string(&out_file).expect("read the secret file");
    let secret = secret.trim_end().to_string();
    assert!(
        secret.starts_with("arca_") && secret.len() == 48,
        "a secret has the arca_ shape"
    );
    let hash_file = hash_dir.join("viewer-1.hash");
    let hash_text = fs::read_to_string(&hash_file).expect("read the hash file");
    assert!(
        hash_text.starts_with("scope=viewer\nsha256="),
        "the hash file names the scope"
    );

    for file in [&out_file, &hash_file] {
        let mode = fs::metadata(file).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o400, "{} is mode 0400", file.display());
    }

    // The printed line names the id and the scope and nothing else.
    let printed: Value = serde_json::from_str(stdout_text(&out).trim()).expect("stdout is JSON");
    assert_eq!(printed, json!({ "id": "viewer-1", "scope": "viewer" }));
    let digest = hash_text
        .lines()
        .nth(1)
        .expect("the digest line")
        .trim_start_matches("sha256=");
    for stream in [stdout_text(&out), stderr_text(&out)] {
        assert!(
            !stream.contains(&secret),
            "the secret never reaches stdout or stderr"
        );
        assert!(
            !stream.contains(digest),
            "the digest never reaches stdout or stderr"
        );
        assert!(
            !stream.contains("arca_"),
            "no token-shaped text reaches stdout or stderr"
        );
    }

    // The files are what the server reads: the store loads them and maps the secret to its scope.
    let store = TokenStore::load(&hash_dir).expect("the store loads the minted directory");
    let caller = store
        .lookup(&secret)
        .expect("the minted secret authenticates");
    assert_eq!(caller.token_id, "viewer-1");
}

#[test]
fn token_mint_refuses_what_it_must_not_write_and_leaks_nothing() {
    let scratch = Scratch::new("mint-refuse");
    let hash_dir = scratch.at("tokens");
    fs::create_dir(&hash_dir).expect("create the token directory");
    fs::set_permissions(&hash_dir, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
    let existing_out = scratch.at("taken.secret");
    fs::write(&existing_out, "already here").expect("write an existing file");

    let good = |id: &str, out: &Path| {
        vec![
            "token".to_string(),
            "mint".to_string(),
            "--scope".to_string(),
            "doctor".to_string(),
            "--id".to_string(),
            id.to_string(),
            "--hash-dir".to_string(),
            hash_dir.display().to_string(),
            "--out".to_string(),
            out.display().to_string(),
        ]
    };
    // A first mint so that a second one with the same id is a real collision.
    let first = arca()
        .args(good("doctor-1", &scratch.at("first.secret")))
        .output()
        .expect("run");
    assert!(
        first.status.success(),
        "the first mint succeeds: {}",
        stderr_text(&first)
    );

    let mut cases: Vec<(&str, Vec<String>)> = vec![
        (
            "an id already minted",
            good("doctor-1", &scratch.at("second.secret")),
        ),
        (
            "an output file that exists",
            good("doctor-2", &existing_out),
        ),
    ];
    let missing_dir = scratch.at("no-such-dir");
    let mut missing = good("doctor-3", &scratch.at("third.secret"));
    missing[7] = missing_dir.display().to_string();
    cases.push(("a hash directory that is missing", missing));

    let mut widened = good("doctor-4", &scratch.at("fourth.secret"));
    let wide = scratch.at("wide");
    fs::create_dir(&wide).expect("create the wide directory");
    fs::set_permissions(&wide, fs::Permissions::from_mode(0o775)).expect("chmod 0775");
    widened[7] = wide.display().to_string();
    cases.push(("a hash directory writable by its group", widened));

    for (name, args) in cases {
        let out = arca().args(&args).output().expect("run the binary");
        assert_eq!(out.status.code(), Some(2), "{name}: refused with exit 2");
        assert!(out.stdout.is_empty(), "{name}: nothing on stdout");
        let stream = stderr_text(&out);
        assert!(
            !stream.contains("arca_"),
            "{name}: no token-shaped text on stderr"
        );
    }
    // Nothing was written for the refused id, and the existing file was left alone.
    assert!(
        !hash_dir.join("doctor-2.hash").exists(),
        "a refused mint wrote no hash file"
    );
    assert_eq!(
        fs::read_to_string(&existing_out).expect("read"),
        "already here"
    );
}

/// A token directory at mode 0700 named `name` under `scratch`.
fn token_dir(scratch: &Scratch, name: &str) -> PathBuf {
    let dir = scratch.at(name);
    fs::create_dir(&dir).expect("create the token directory");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
    dir
}

/// A node config whose `tokens.hashDir` is `hash_dir`.
fn config_naming_hash_dir(scratch: &Scratch, hash_dir: &Path) -> PathBuf {
    let mut value = config(scratch, &scratch.at("incus.sock"));
    value["tokens"]["hashDir"] = json!(hash_dir.display().to_string());
    write_config(scratch, "mint.json", &value)
}

fn mode_of(path: &Path) -> u32 {
    fs::metadata(path).expect("stat").permissions().mode() & 0o777
}

#[test]
fn token_mint_takes_the_hash_dir_of_a_config_alone() {
    let scratch = Scratch::new("mint-config");
    let hash_dir = token_dir(&scratch, "tokens");
    let config_path = config_naming_hash_dir(&scratch, &hash_dir);
    let out_file = scratch.at("cfg-1.secret");
    let out = arca()
        .args([
            "token", "mint", "--scope", "viewer", "--id", "cfg-1", "--config",
        ])
        .arg(&config_path)
        .arg("--out")
        .arg(&out_file)
        .output()
        .expect("run the binary");
    assert!(
        out.status.success(),
        "the mint succeeds: {}",
        stderr_text(&out)
    );
    assert_eq!(
        mode_of(&hash_dir.join("cfg-1.hash")),
        0o400,
        "hash file 0400"
    );
    assert_eq!(mode_of(&out_file), 0o400, "secret file 0400");
    let printed: Value = serde_json::from_str(stdout_text(&out).trim()).expect("stdout is JSON");
    assert_eq!(printed, json!({ "id": "cfg-1", "scope": "viewer" }));
}

#[test]
fn token_mint_accepts_a_hash_dir_that_matches_the_config() {
    let scratch = Scratch::new("mint-match");
    let hash_dir = token_dir(&scratch, "tokens");
    let config_path = config_naming_hash_dir(&scratch, &hash_dir);
    let out_file = scratch.at("match-1.secret");
    let out = arca()
        .args([
            "token", "mint", "--scope", "viewer", "--id", "match-1", "--config",
        ])
        .arg(&config_path)
        .arg("--hash-dir")
        .arg(&hash_dir)
        .arg("--out")
        .arg(&out_file)
        .output()
        .expect("run the binary");
    assert!(
        out.status.success(),
        "a matching hash-dir is accepted: {}",
        stderr_text(&out)
    );
    assert_eq!(mode_of(&hash_dir.join("match-1.hash")), 0o400);
}

#[test]
fn token_mint_refuses_a_hash_dir_that_differs_from_the_config_and_writes_nothing() {
    let scratch = Scratch::new("mint-differ");
    let configured = token_dir(&scratch, "tokens");
    let other = token_dir(&scratch, "other-tokens");
    let config_path = config_naming_hash_dir(&scratch, &configured);
    let out_file = scratch.at("differ-1.secret");
    let out = arca()
        .args([
            "token", "mint", "--scope", "viewer", "--id", "differ-1", "--config",
        ])
        .arg(&config_path)
        .arg("--hash-dir")
        .arg(&other)
        .arg("--out")
        .arg(&out_file)
        .output()
        .expect("run the binary");
    assert_eq!(
        out.status.code(),
        Some(2),
        "a differing hash-dir is a usage error"
    );
    assert!(out.stdout.is_empty(), "nothing on stdout");
    assert!(
        stderr_text(&out).contains("differs"),
        "the message says the directories differ: {}",
        stderr_text(&out)
    );
    assert!(
        !configured.join("differ-1.hash").exists(),
        "no hash in the config's directory"
    );
    assert!(
        !other.join("differ-1.hash").exists(),
        "no hash in the given directory"
    );
    assert!(!out_file.exists(), "no secret file");
}

#[test]
fn token_mint_with_neither_flag_takes_the_config_every_command_takes() {
    let scratch = Scratch::new("mint-neither");
    let out_file = scratch.at("neither-1.secret");
    let out = arca()
        .args([
            "token",
            "mint",
            "--scope",
            "viewer",
            "--id",
            "neither-1",
            "--out",
        ])
        .arg(&out_file)
        .output()
        .expect("run the binary");
    // With neither, the mint takes the config every command takes; here there is none installed.
    assert_eq!(out.status.code(), Some(2), "neither is refused");
    assert!(out.stdout.is_empty(), "nothing on stdout");
    assert!(
        stderr_text(&out).starts_with(
            "arca: there is no config at /nonexistent/arca-test-home/.config/arca/node.json, where the installer writes it"
        ),
        "the message says which config was looked for: {}",
        stderr_text(&out)
    );
    assert!(!out_file.exists(), "no secret file");
}

#[test]
fn watch_once_through_the_shim_prints_a_conforming_report_and_the_shim_refuses_nothing() {
    let scratch = Scratch::new("watch");
    let rig = rig(&scratch);
    let path = write_config(&scratch, "node.json", &config(&scratch, rig.shim.socket()));
    let out = arca()
        .args(["watch", "--once", "--json", "--config"])
        .arg(&path)
        .output()
        .expect("run the binary");
    assert!(
        out.status.success(),
        "the watch runs: {}",
        stderr_text(&out)
    );
    let value: Value = serde_json::from_str(stdout_text(&out).trim()).expect("stdout is JSON");
    conforming("watch_report", &value);

    let projects = value["projects"].as_array().expect("projects");
    let total: usize = projects
        .iter()
        .map(|p| p["instances"].as_array().map_or(0, Vec::len))
        .sum();
    assert_eq!(total, 6, "every fixture instance is classed once");
    let phone = projects
        .iter()
        .flat_map(|p| p["instances"].as_array().into_iter().flatten())
        .find(|row| row["name"] == json!("phone1"))
        .expect("phone1 is listed");
    assert_eq!(
        phone["identity"],
        json!(true),
        "phone1 is the identity phone"
    );
    assert_eq!(phone["privileged"], json!(true), "phone1 is privileged");

    let summary = rig.shim.stop().expect("stop the shim");
    assert_eq!(summary.refused, 0, "the shim refused nothing");
    assert_only_gets(&rig.log);
    assert!(
        rig.fake.requests().iter().all(|r| r.method == "GET"),
        "the upstream saw only GET"
    );
}

#[test]
fn metrics_prints_a_node_metrics_result_with_no_renter_gate() {
    let scratch = Scratch::new("metrics");
    let rig = rig(&scratch);
    let path = write_config(&scratch, "node.json", &config(&scratch, rig.shim.socket()));
    let out = arca()
        .args(["metrics", "--json", "--sample-for", "0", "--config"])
        .arg(&path)
        .output()
        .expect("run the binary");
    assert!(
        out.status.success(),
        "the metrics run: {}",
        stderr_text(&out)
    );
    let text = stdout_text(&out);
    assert!(!text.contains("renterGate"), "no renter gate at any depth");
    let value: Value = serde_json::from_str(text.trim()).expect("stdout is JSON");
    conforming("node_metrics", &value);
    assert!(!value["latest"].is_null(), "one sample was taken");

    let summary = rig.shim.stop().expect("stop the shim");
    assert_eq!(summary.refused, 0, "the shim refused nothing");
    assert_only_gets(&rig.log);
    assert!(
        rig.fake.requests().iter().all(|r| r.method == "GET"),
        "the upstream saw only GET"
    );
}

/// Waits until a unix socket accepts a connection, or the deadline passes.
fn wait_for_socket(path: &Path) -> bool {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if UnixStream::connect(path).is_ok() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

/// One raw HTTP/1.1 exchange over a unix socket. Returns the status code and the body text.
fn raw_http(socket: &Path, request: &str) -> (u16, String) {
    let mut stream = UnixStream::connect(socket).expect("connect");
    stream.set_read_timeout(Some(WAIT)).expect("set a deadline");
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

#[test]
fn the_shim_command_forwards_gets_refuses_other_methods_and_prints_its_summary_on_sigterm() {
    let scratch = Scratch::new("shim");
    let state = FakeState::load(Path::new(FIXTURE)).expect("load the fixture");
    let fake = FakeIncus::start(&scratch.0, state).expect("start the fake");
    let listen = scratch.at("shim.sock");
    let log = scratch.at("shim.log");
    let mut child = arca()
        .args(["shim", "--upstream"])
        .arg(fake.socket())
        .arg("--listen")
        .arg(&listen)
        .arg("--log")
        .arg(&log)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the shim command");
    assert!(wait_for_socket(&listen), "the shim listens");

    let (get_status, _) = raw_http(
        &listen,
        "GET /1.0 HTTP/1.1\r\nHost: arca\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(get_status, 200, "a GET is forwarded and answered");
    let (post_status, _) = raw_http(
        &listen,
        "POST /1.0 HTTP/1.1\r\nHost: arca\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(post_status, 405, "a POST is refused");

    let kill = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("send SIGTERM");
    assert!(kill.success(), "SIGTERM was delivered");
    let status = wait_exit(&mut child);
    assert_eq!(
        status.code(),
        Some(0),
        "the shim exits 0 on SIGTERM: {}",
        status
    );

    let mut stdout = String::new();
    child
        .stdout
        .take()
        .expect("stdout")
        .read_to_string(&mut stdout)
        .expect("read stdout");
    let summary: Value = serde_json::from_str(stdout.trim()).expect("the summary is JSON");
    assert_eq!(summary, json!({ "get": 1, "refused": 1 }));

    let lines = log_lines(&log);
    assert_eq!(lines.len(), 2, "two requests were logged");
    assert_eq!(lines[0]["method"], json!("GET"));
    assert_eq!(lines[0]["forwarded"], json!(true));
    assert_eq!(lines[1]["method"], json!("POST"));
    assert_eq!(lines[1]["forwarded"], json!(false));
    let methods: Vec<Value> = fake
        .requests()
        .into_iter()
        .map(|r| json!(r.method))
        .collect();
    assert!(
        methods.iter().all(|m| m == &json!("GET")),
        "the upstream saw only GET"
    );
}

/// Waits for a child to exit, for at most the wait; a child that will not exit is killed and the test fails.
fn wait_exit(child: &mut std::process::Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(status) = child.try_wait().expect("poll the child") {
            return status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("the child did not exit after SIGTERM");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn the_shim_command_prints_a_live_summary_on_sigusr1_and_keeps_serving_with_a_capped_log() {
    use std::io::{BufRead, BufReader};

    let scratch = Scratch::new("shim-usr1");
    let state = FakeState::load(Path::new(FIXTURE)).expect("load the fixture");
    let fake = FakeIncus::start(&scratch.0, state).expect("start the fake");
    let listen = scratch.at("shim.sock");
    let log = scratch.at("shim.log");
    let mut child = arca()
        .args(["shim", "--upstream"])
        .arg(fake.socket())
        .arg("--listen")
        .arg(&listen)
        .arg("--log")
        .arg(&log)
        .args(["--log-max-bytes", "120"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the shim command");
    assert!(wait_for_socket(&listen), "the shim listens");
    let get = "GET /1.0 HTTP/1.1\r\nHost: arca\r\nConnection: close\r\n\r\n";
    for _ in 0..4 {
        assert_eq!(raw_http(&listen, get).0, 200);
    }

    let usr1 = Command::new("kill")
        .args(["-USR1", &child.id().to_string()])
        .status()
        .expect("send SIGUSR1");
    assert!(usr1.success(), "SIGUSR1 was delivered");
    let mut stderr = BufReader::new(child.stderr.take().expect("stderr"));
    let mut line = String::new();
    stderr.read_line(&mut line).expect("read the live summary");
    let live: Value = serde_json::from_str(line.trim()).expect("the live summary is JSON");
    assert_eq!(live, json!({ "get": 4, "refused": 0 }));
    assert_eq!(
        raw_http(&listen, get).0,
        200,
        "the shim keeps serving after SIGUSR1"
    );

    let kill = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("send SIGTERM");
    assert!(kill.success(), "SIGTERM was delivered");
    assert_eq!(wait_exit(&mut child).code(), Some(0));
    let mut stdout = String::new();
    child
        .stdout
        .take()
        .expect("stdout")
        .read_to_string(&mut stdout)
        .expect("read stdout");
    let summary: Value = serde_json::from_str(stdout.trim()).expect("the summary is JSON");
    assert_eq!(summary, json!({ "get": 5, "refused": 0 }));
    assert!(
        scratch.at("shim.log.1").exists(),
        "the capped log was rotated"
    );
    assert!(
        !scratch.at("shim.log.2").exists(),
        "one rotated file is kept"
    );
}

/// A free loopback TCP port, bound once and released for the server under test.
fn free_loopback_port() -> std::net::SocketAddr {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a probe");
    probe.local_addr().expect("the probe's address")
}

/// Whether process `pid` holds a listening IPv4 or IPv6 TCP socket on `port`, read from procfs (Linux). A socket is in the
/// process's descriptor table with its inode, and the kernel lists it as LISTEN in `/proc/net/tcp*`, only once its own `bind`
/// and `listen` have succeeded. So this holds for the server under test and never for another process's listener.
#[cfg(target_os = "linux")]
fn child_listens_on(pid: u32, port: u16) -> bool {
    let Ok(fds) = fs::read_dir(format!("/proc/{pid}/fd")) else {
        return false;
    };
    let sockets: std::collections::BTreeSet<String> = fds
        .filter_map(|fd| fs::read_link(fd.ok()?.path()).ok())
        .filter_map(|link| {
            let text = link.to_str()?;
            Some(
                text.strip_prefix("socket:[")?
                    .trim_end_matches(']')
                    .to_string(),
            )
        })
        .collect();
    ["/proc/net/tcp", "/proc/net/tcp6"].iter().any(|table| {
        fs::read_to_string(table)
            .unwrap_or_default()
            .lines()
            .skip(1)
            .any(|line| {
                // The columns are: slot, local address, remote address, state (`0A` is LISTEN), queues, timers, retransmits,
                // uid, timeout, and the inode, which is the tenth.
                let fields: Vec<&str> = line.split_whitespace().collect();
                let Some(local_port) = fields
                    .get(1)
                    .and_then(|local| local.rsplit(':').next())
                    .and_then(|hex| u16::from_str_radix(hex, 16).ok())
                else {
                    return false;
                };
                local_port == port
                    && fields.get(3) == Some(&"0A")
                    && fields.get(9).is_some_and(|inode| sockets.contains(*inode))
            })
    })
}

/// Starts `arca serve` with a TCP listener that `listen` writes into the config for a free loopback port, and returns the
/// server once it listens there, with the address.
#[cfg(target_os = "linux")]
fn serve_on_a_free_port(
    scratch: &Scratch,
    incus: &Path,
    listen: &dyn Fn(&mut Value, std::net::SocketAddr),
) -> (Node, std::net::SocketAddr) {
    // A port is free when it is probed, but another process can take it before `serve` binds it. Then `serve` exits with
    // "Address already in use", and a connect to that port reaches the other process's listener, so a connect proves nothing
    // about the owner. The loop waits for the child's own listening socket on the port (`child_listens_on`). A `serve` that
    // exits first, because the address is already in use, is started again on a fresh port, at most five times. Any other
    // early exit fails at once with the server's stderr.
    let mut attempts = 0;
    loop {
        attempts += 1;
        let at = free_loopback_port();
        let mut value = config(scratch, incus);
        listen(&mut value, at);
        let path = write_config(scratch, "node.json", &value);
        let mut child = Node(
            serve_command(&path)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("start arca serve"),
        );

        let deadline = Instant::now() + LISTEN_WAIT;
        let owned = loop {
            if child_listens_on(child.id(), at.port()) {
                break true;
            }
            if let Some(status) = child.try_wait().expect("poll the server") {
                let mut stderr = String::new();
                if let Some(mut pipe) = child.stderr.take() {
                    pipe.read_to_string(&mut stderr).expect("read stderr");
                }
                assert!(
                    stderr.contains("Address already in use"),
                    "the server exited before listening: {status}\n{stderr}"
                );
                assert!(
                    attempts < 5,
                    "the port was taken five times: {status}\n{stderr}"
                );
                break false;
            }
            assert!(Instant::now() < deadline, "the listener comes up");
            thread::sleep(Duration::from_millis(20));
        };
        if owned {
            return (child, at);
        }
    }
}

#[test]
#[cfg(target_os = "linux")]
fn serve_answers_on_the_proxy_listener_and_stops_every_listener_on_sigterm() {
    let scratch = Scratch::new("serve-proxy");
    let rig = rig(&scratch);
    let state = scratch.at("state");
    fs::create_dir_all(&state).expect("create the state directory");
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
    let tokens = scratch.at("tokens");
    fs::create_dir_all(&tokens).expect("create the token directory");
    fs::set_permissions(&tokens, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
    let (mut child, at) = serve_on_a_free_port(&scratch, rig.shim.socket(), &|value, at| {
        value["listen"]["proxy"] = json!({ "addr": at.to_string(), "peer": "127.0.0.1" });
    });
    let mut stream = connect_when_bound(at);
    stream.set_read_timeout(Some(WAIT)).expect("set a deadline");
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: arca\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .expect("send the request");
    let mut reply = String::new();
    stream.read_to_string(&mut reply).expect("read the reply");
    assert!(
        reply.starts_with("HTTP/1.1 401"),
        "the configured peer reaches the router, which wants a bearer: {reply}"
    );

    let kill = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("send SIGTERM");
    assert!(kill.success(), "SIGTERM was delivered");
    assert_eq!(
        wait_exit(&mut child).code(),
        Some(0),
        "serve exits 0 on SIGTERM"
    );
    assert!(
        std::net::TcpStream::connect(at).is_err(),
        "the proxy listener is closed once serve stops"
    );
    assert!(
        !scratch.at("arca.sock").exists(),
        "the unix socket is removed"
    );
    let summary = rig.shim.stop().expect("stop the shim");
    assert_eq!(summary.refused, 0, "serve sent Incus only GETs");
}

#[test]
fn a_reading_command_takes_the_flag_then_the_variable_then_the_file_the_installer_writes() {
    let scratch = Scratch::new("config-order");
    // Three configs, each refused for a field of its own, so the refusal says which file was read.
    let broken = |field: &str| {
        let mut value = config(&scratch, &scratch.at("incus.sock"));
        value[field] = json!(1);
        value
    };
    let home = installed_home(&scratch, Some(&broken("fromTheDefault")));
    let variable_config = write_config(&scratch, "variable.json", &broken("fromTheVariable"));
    let flag_config = write_config(&scratch, "flag.json", &broken("fromTheFlag"));
    let read = |with: &dyn Fn(&mut Command)| {
        let mut command = arca();
        command
            .args(["watch", "--once", "--json"])
            .env("HOME", &home);
        with(&mut command);
        let out = command.output().expect("run the binary");
        assert_eq!(out.status.code(), Some(2), "{}", stderr_text(&out));
        stderr_text(&out)
    };
    let refused = |path: &Path, field: &str| {
        format!(
            "arca: in the config file {}: unknown field {field}; ",
            path.display()
        )
    };
    let by_default = read(&|_| {});
    assert!(
        by_default.starts_with(&refused(
            &home.join(".config/arca/node.json"),
            "fromTheDefault"
        )),
        "{by_default}"
    );
    let by_variable = read(&|command| {
        command.env("ARCA_CONFIG", &variable_config);
    });
    assert!(
        by_variable.starts_with(&refused(&variable_config, "fromTheVariable")),
        "{by_variable}"
    );
    let by_flag = read(&|command| {
        command
            .env("ARCA_CONFIG", &variable_config)
            .arg("--config")
            .arg(&flag_config);
    });
    assert!(
        by_flag.starts_with(&refused(&flag_config, "fromTheFlag")),
        "{by_flag}"
    );
}

#[test]
fn the_token_file_is_the_flag_then_the_variable_then_the_file_the_installer_writes() {
    let scratch = Scratch::new("token-order");
    let home = installed_home(&scratch, Some(&config(&scratch, &scratch.at("incus.sock"))));
    // The default holds a token; the variable names no file; the flag names a file that is too open. Each is refused, or
    // gets as far as the socket, in its own words.
    token_file(&home.join(".config/arca/assistant-1.secret"));
    let loose = scratch.at("loose.secret");
    token_file(&loose);
    fs::set_permissions(&loose, fs::Permissions::from_mode(0o640)).expect("chmod 0640");
    let status = |with: &dyn Fn(&mut Command)| {
        let mut command = arca();
        command.arg("status").env("HOME", &home);
        with(&mut command);
        let out = command.output().expect("run the binary");
        (out.status.code(), stderr_text(&out))
    };
    let (code, text) = status(&|_| {});
    assert_eq!(
        code,
        Some(1),
        "the default token was read, and the node is not running: {text}"
    );
    assert!(
        text.starts_with("arca: the node's socket does not answer"),
        "{text}"
    );
    let (code, text) = status(&|command| {
        command.env("ARCA_TOKEN_FILE", scratch.at("no-such.secret"));
    });
    assert_eq!(code, Some(2), "{text}");
    assert!(
        text.starts_with("arca: the token file does not exist: "),
        "{text}"
    );
    let (code, text) = status(&|command| {
        command
            .env("ARCA_TOKEN_FILE", scratch.at("no-such.secret"))
            .arg("--token-file")
            .arg(&loose);
    });
    assert_eq!(code, Some(2), "{text}");
    assert!(
        text.starts_with("arca: the token file can be read by its group or by others"),
        "{text}"
    );
}

#[test]
fn a_default_config_that_is_not_there_says_where_it_was_looked_for_and_what_to_do() {
    let scratch = Scratch::new("config-absent");
    let home = installed_home(&scratch, None);
    let out = arca()
        .args(["watch", "--once", "--json"])
        .env("HOME", &home)
        .output()
        .expect("run the binary");
    assert_eq!(out.status.code(), Some(2), "{}", stderr_text(&out));
    assert_eq!(
        stderr_text(&out).trim_end(),
        format!(
            "arca: there is no config at {}/.config/arca/node.json, where the installer writes it: install Arca for this user, or name another config with --config CONFIG_FILE (its path in place of CONFIG_FILE) or ARCA_CONFIG",
            home.display()
        )
    );
    // With no home directory there is no default to name.
    let out = arca()
        .args(["watch", "--once", "--json"])
        .env_remove("HOME")
        .output()
        .expect("run the binary");
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr_text(&out).starts_with(
            "arca: --config is required here: neither ARCA_CONFIG nor HOME is set, so there is no default to read"
        ),
        "{}",
        stderr_text(&out)
    );
}

/// One node of a test: its config file, its state directory and its token directory, each of its own under `scratch`.
struct TestNode {
    config: PathBuf,
    state: PathBuf,
    tokens: PathBuf,
}

/// A node named `name` under `scratch`. Its config is written to `at`, or to `<name>.json` when `at` is `None`.
fn test_node(scratch: &Scratch, name: &str, at: Option<&Path>) -> TestNode {
    let state = scratch.at(&format!("{name}-state"));
    fs::create_dir(&state).expect("the state directory");
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
    let tokens = token_dir(scratch, &format!("{name}-tokens"));
    let mut value = config(scratch, &scratch.at("incus.sock"));
    value["stateDir"] = json!(state.display().to_string());
    value["tokens"]["hashDir"] = json!(tokens.display().to_string());
    value["listen"]["unix"] = json!(scratch.at(&format!("{name}.sock")).display().to_string());
    let config = match at {
        Some(at) => {
            fs::write(at, value.to_string()).expect("the config");
            at.to_path_buf()
        }
        None => write_config(scratch, &format!("{name}.json"), &value),
    };
    TestNode {
        config,
        state,
        tokens,
    }
}

/// Three nodes: the one the installer's default path names, one `ARCA_CONFIG` can name, and one `--config` can name.
fn three_nodes(scratch: &Scratch) -> (PathBuf, TestNode, TestNode, TestNode) {
    let home = installed_home(scratch, None);
    let installed = test_node(
        scratch,
        "installed",
        Some(&home.join(".config/arca/node.json")),
    );
    let variable = test_node(scratch, "variable", None);
    let flag = test_node(scratch, "flag", None);
    (home, installed, variable, flag)
}

#[test]
fn a_command_that_acts_finds_its_config_as_every_command_does_and_says_which_before_it_acts() {
    let scratch = Scratch::new("config-acts");
    let (home, installed, variable, flag) = three_nodes(&scratch);
    let install = |with: &dyn Fn(&mut Command)| {
        let mut command = arca();
        command.args(["templates", "install"]).env("HOME", &home);
        with(&mut command);
        let out = command.output().expect("run the binary");
        assert_eq!(out.status.code(), Some(0), "{}", stderr_text(&out));
        stderr_text(&out)
    };
    // No flag and no variable: the installer's node, and the command says so first.
    assert_eq!(
        install(&|_| {}),
        format!(
            "arca: using the config {} (the default)\n",
            installed.config.display()
        )
    );
    assert!(
        installed.state.join("templates").is_dir(),
        "the default node was acted on"
    );
    assert!(!variable.state.join("templates").exists());
    // The variable wins over the default, and is named as where the config came from.
    assert_eq!(
        install(&|command| {
            command.env("ARCA_CONFIG", &variable.config);
        }),
        format!(
            "arca: using the config {} (from ARCA_CONFIG)\n",
            variable.config.display()
        )
    );
    assert!(
        variable.state.join("templates").is_dir(),
        "the variable's node was acted on"
    );
    assert!(!flag.state.join("templates").exists());
    // The flag wins over both, and a config the caller named needs no saying.
    assert_eq!(
        install(&|command| {
            command
                .env("ARCA_CONFIG", &variable.config)
                .arg("--config")
                .arg(&flag.config);
        }),
        ""
    );
    assert!(
        flag.state.join("templates").is_dir(),
        "the flag's node was acted on"
    );

    // A mint with neither flag takes the same config, and says so.
    let mint = |id: &str, with: &dyn Fn(&mut Command)| {
        let mut command = arca();
        command
            .args(["token", "mint", "--scope", "viewer", "--id", id, "--out"])
            .arg(scratch.at(&format!("{id}.secret")))
            .env("HOME", &home);
        with(&mut command);
        let out = command.output().expect("run the binary");
        assert_eq!(out.status.code(), Some(0), "{}", stderr_text(&out));
        stderr_text(&out)
    };
    assert_eq!(
        mint("default-1", &|_| {}),
        format!(
            "arca: using the config {} (the default)\n",
            installed.config.display()
        )
    );
    assert!(installed.tokens.join("default-1.hash").exists());
    // A hash directory given alone names the tokens by itself: no config is read, not even the variable's.
    assert_eq!(
        mint("alone-1", &|command| {
            command
                .env("ARCA_CONFIG", scratch.at("no-such-config.json"))
                .arg("--hash-dir")
                .arg(&flag.tokens);
        }),
        ""
    );
    assert!(flag.tokens.join("alone-1.hash").exists());
}

#[test]
fn a_command_that_removes_names_the_node_and_its_state_directory_before_it_acts() {
    let scratch = Scratch::new("config-removes");
    let (home, installed, variable, flag) = three_nodes(&scratch);
    let naming = |node: &TestNode, found: &str| {
        format!(
            "arca: using the config {} ({found}); this node's state directory is {}\n",
            node.config.display(),
            node.state.display()
        )
    };
    let minted = arca()
        .args([
            "token", "mint", "--scope", "viewer", "--id", "gone-1", "--out",
        ])
        .arg(scratch.at("gone-1.secret"))
        .arg("--config")
        .arg(&installed.config)
        .output()
        .expect("run the binary");
    assert_eq!(minted.status.code(), Some(0), "{}", stderr_text(&minted));

    // A revoke with no flag: the default node, named whole as the first line, and then the token is gone.
    let out = arca()
        .args(["token", "revoke", "gone-1"])
        .env("HOME", &home)
        .output()
        .expect("run the binary");
    assert_eq!(out.status.code(), Some(0), "{}", stderr_text(&out));
    assert_eq!(stderr_text(&out), naming(&installed, "the default"));
    assert!(!installed.tokens.join("gone-1.hash").exists());

    // Commands that remove and then find nothing to remove still said which node they looked in, first.
    for args in [
        vec!["probe", "consumer", "remove", "--name", "web"],
        vec!["link", "remove", "peer-1"],
        vec!["adopt", "release", "--instance", "svc1"],
        vec![
            "adopt",
            "apply",
            "--instance",
            "svc1",
            "--plan-digest",
            "00",
        ],
        // Replacing the shipped templates overwrites what the owner edited, so it names the node whole too.
        vec!["templates", "install", "--replace"],
    ] {
        let out = arca()
            .args(&args)
            .env("HOME", &home)
            .output()
            .expect("run the binary");
        assert!(
            stderr_text(&out).starts_with(&naming(&installed, "the default")),
            "{args:?}: {}",
            stderr_text(&out)
        );
    }
    // The variable names its node the same way, for a command it may steer.
    let out = arca()
        .args(["templates", "install", "--replace"])
        .env("HOME", &home)
        .env("ARCA_CONFIG", &variable.config)
        .output()
        .expect("run the binary");
    assert_eq!(stderr_text(&out), naming(&variable, "from ARCA_CONFIG"));
    // A config the caller named with the flag is not said back.
    let out = arca()
        .args(["token", "revoke", "never-1", "--config"])
        .arg(&flag.config)
        .env("HOME", &home)
        .output()
        .expect("run the binary");
    assert_eq!(stderr_text(&out), "arca: no token with id never-1 exists\n");
}

#[test]
fn a_command_that_only_reads_says_nothing_and_a_start_says_its_config_before_it_refuses() {
    let scratch = Scratch::new("config-reads");
    let (home, installed, _variable, _flag) = three_nodes(&scratch);
    // Reading commands find the same config and say nothing about it.
    for args in [
        vec!["link", "peers"],
        vec!["adopt", "status", "--instance", "svc1", "--json"],
        vec!["watch", "--once", "--json"],
    ] {
        let out = arca()
            .args(&args)
            .env("HOME", &home)
            .output()
            .expect("run the binary");
        assert!(
            !stderr_text(&out).contains("using the config"),
            "{args:?}: {}",
            stderr_text(&out)
        );
    }
    // `serve` with no flag starts the default node, and says which before anything else. Here its state directory is
    // gone, so the start is refused, and the refusal is the second line.
    fs::remove_dir(&installed.state).expect("remove the state directory");
    let using = format!(
        "arca: using the config {} (the default)\n",
        installed.config.display()
    );
    let out = arca()
        .arg("serve")
        .env("HOME", &home)
        .output()
        .expect("run the binary");
    assert_eq!(out.status.code(), Some(2), "{}", stderr_text(&out));
    assert_eq!(
        stderr_text(&out),
        format!(
            "{using}arca: the state directory {} is missing\n",
            installed.state.display()
        )
    );
    // `setup-code` asks the node for a code: it acts, so it says its config too.
    let out = arca()
        .arg("setup-code")
        .env("HOME", &home)
        .output()
        .expect("run the binary");
    assert!(
        stderr_text(&out).starts_with(&using),
        "{}",
        stderr_text(&out)
    );
    // With no config installed at all, a start is refused as every command is.
    let empty = scratch.at("empty-home");
    fs::create_dir(&empty).expect("an empty home");
    let out = arca()
        .arg("serve")
        .env("HOME", &empty)
        .output()
        .expect("run the binary");
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr_text(&out).starts_with(&format!(
            "arca: there is no config at {}/.config/arca/node.json, where the installer writes it",
            empty.display()
        )),
        "{}",
        stderr_text(&out)
    );
}

#[test]
fn a_peer_call_takes_its_token_from_the_flag_alone_and_never_sends_this_nodes_own() {
    let scratch = Scratch::new("peer-token");
    let value = config(&scratch, &scratch.at("incus.sock"));
    let home = installed_home(&scratch, Some(&value));
    // Both of this node's own token sources hold a token: neither may be read for a call that leaves the node.
    let own = home.join(".config/arca/assistant-1.secret");
    token_file(&own);
    let named = write_config(&scratch, "named.json", &value);
    for prefix in [true, false] {
        let mut command = arca();
        command.arg("call");
        if prefix {
            command.args(["--peer", "host", "arca_status"]);
        } else {
            command.args(["arca_status", "--peer", "host"]);
        }
        let out = command
            .arg("--config")
            .arg(&named)
            .env("HOME", &home)
            .env("ARCA_TOKEN_FILE", &own)
            .output()
            .expect("run the binary");
        assert_eq!(out.status.code(), Some(2), "{}", stderr_text(&out));
        assert!(
            stderr_text(&out).starts_with(
                "arca: --token-file is required with --peer: the token is sent to the peer, so it is one the peer minted and the caller names, never this node's own token from the default file or from ARCA_TOKEN_FILE\n"
            ),
            "{}",
            stderr_text(&out)
        );
    }
    // The config of a peer call is found as every command finds it; only its token must be named. The call acts through
    // this node's link, so it says which config it found.
    let out = arca()
        .args(["call", "--peer", "host", "arca_status", "--token-file"])
        .arg(&own)
        .env("HOME", &home)
        .output()
        .expect("run the binary");
    assert_eq!(out.status.code(), Some(1), "{}", stderr_text(&out));
    assert_eq!(
        stderr_text(&out),
        format!(
            "arca: using the config {default} (the default)\narca: the link is not running: start it with: arca link serve --config {default}, and keep it running\n",
            default = home.join(".config/arca/node.json").display()
        )
    );
    // With the flag, the call goes on to the link, which is not running here: the token was taken from the flag.
    let out = arca()
        .args(["call", "--peer", "host", "arca_status", "--config"])
        .arg(&named)
        .arg("--token-file")
        .arg(&own)
        .env("HOME", &home)
        .output()
        .expect("run the binary");
    assert_eq!(out.status.code(), Some(1), "{}", stderr_text(&out));
    assert_eq!(
        stderr_text(&out),
        format!(
            "arca: the link is not running: start it with: arca link serve --config {}, and keep it running\n",
            named.display()
        )
    );
}

#[test]
fn help_says_the_defaults_and_the_order_they_are_taken_in() {
    let out = arca().arg("--help").output().expect("run the binary");
    assert_eq!(out.status.code(), Some(0));
    let help = stdout_text(&out);
    for line in [
        "  status [--json | --security] [--config <file>] [--token-file <file>]",
        "  --config      ~/.config/arca/node.json",
        "  --token-file  ~/.config/arca/assistant-1.secret",
        "  The order is: the flag, then the variable (ARCA_CONFIG, ARCA_TOKEN_FILE), then the default.",
        "  status alone takes one step more: with no token file named and none at the default, it uses a viewer token of this",
        "  node from the default's directory (the viewer.secret of a node installed before that default existed).",
        "  Every command finds its config this way. One that writes to a node or acts on it, without --config, first says on",
        "  stderr which config it uses; one that removes or cuts something off also names the node's state directory.",
        "  A call with --peer sends its token to another node, so its --token-file is always named: it has no default.",
        "  token mint, token revoke, token reload, call --peer, link pair, link join, link remove, probe consumer remove,",
        "  adopt apply, adopt release and init hand out, remove or rewrite something on one node, so the variable alone never",
        "  picks it: when ARCA_CONFIG names another file than the default, they refuse and say the --config to run them with.",
        "  init --watch-only [--config <file>] (back to a node that only looks)",
        "  call --peer <name> <verb> --token-file <file> [--config <file>] [--args <json> | --args-file <file>]",
        "  token mint --scope <viewer|doctor|operator> --id <id> --out <file> [--tools <comma list>] [--config <file> | --hash-dir <dir>]",
        "  token reload [--config <file>] (the running node reads its tokens again)",
        "  serve [--config <file>]",
    ] {
        assert!(
            help.lines().any(|have| have == line),
            "the help has the line {line:?}:\n{help}"
        );
    }
}

#[test]
fn a_mistyped_command_action_or_flag_is_answered_with_the_closest_known_one() {
    let cases: [(&[&str], &str); 6] = [
        (
            &["stauts"],
            "arca: that is not an arca command; did you mean status? Run arca --help for all of them\n",
        ),
        (
            &["frobnicate"],
            "arca: that is not an arca command: run arca --help for all of them\n",
        ),
        (
            &["link", "piar"],
            "arca: that is not a link action; did you mean pair? It is one of serve, pair, join, peers, remove\n",
        ),
        (
            &["token", "rotate"],
            "arca: that is not a token action: it is one of mint, revoke, reload\n",
        ),
        (
            &["adopt"],
            "arca: an adopt action is missing: it is one of plan, apply, release, verify, status\n",
        ),
        (
            &["watch", "--once", "--jsno"],
            "arca: unknown flag --jsno; did you mean --json?\nusage:\n  arca watch --once --json [--config <file>]\n",
        ),
    ];
    for (args, want) in cases {
        let out = arca().args(args).output().expect("run the binary");
        assert_eq!(out.status.code(), Some(2), "{args:?} is a usage error");
        assert!(out.stdout.is_empty(), "{args:?} prints nothing on stdout");
        assert_eq!(stderr_text(&out), want, "{args:?}");
    }
}

#[test]
fn a_flag_that_is_missing_shows_the_usage_of_its_command() {
    let out = arca()
        .args(["shim", "--upstream", "a", "--listen", "b"])
        .output()
        .expect("run the binary");
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        stderr_text(&out),
        "arca: --log is required\nusage:\n  arca shim --upstream <socket> --listen <socket> --log <file> [--log-max-bytes <n>]\n"
    );
}

/// A file that holds a well-formed token no node knows, at mode 0600.
fn token_file(path: &Path) {
    fs::write(path, format!("arca_{}\n", "A".repeat(43))).expect("the token file");
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("chmod 0600");
}

#[test]
fn status_of_a_node_that_is_not_running_says_so_and_how_to_start_that_node() {
    let scratch = Scratch::new("status-down");
    let value = config(&scratch, &scratch.at("incus.sock"));
    let home = installed_home(&scratch, Some(&value));
    token_file(&home.join(".config/arca/assistant-1.secret"));
    let down = "arca: the node's socket does not answer: the node is not running, or listen.unix in its config names another socket. Start the node with: ";
    // The installer's node: its service, or serve with its config.
    let out = arca()
        .arg("status")
        .env("HOME", &home)
        .output()
        .expect("run the binary");
    assert_eq!(out.status.code(), Some(1), "{}", stderr_text(&out));
    assert!(out.stdout.is_empty());
    assert_eq!(
        stderr_text(&out),
        format!(
            "{down}systemctl --user start arca.service (the installer's service), or: arca serve --config {}/.config/arca/node.json\n",
            home.display()
        )
    );
    // Another node, named by the flag or by the variable: the advice names that node, never the installer's service.
    let other = write_config(&scratch, "other.json", &value);
    for by_variable in [false, true] {
        let mut command = arca();
        command.arg("status").env("HOME", &home);
        if by_variable {
            command.env("ARCA_CONFIG", &other);
        } else {
            command.arg("--config").arg(&other);
        }
        let out = command.output().expect("run the binary");
        assert_eq!(out.status.code(), Some(1), "{}", stderr_text(&out));
        assert_eq!(
            stderr_text(&out),
            format!("{down}arca serve --config {}\n", other.display())
        );
    }
}

#[test]
fn a_token_file_that_is_not_there_says_so_and_how_to_mint_one_the_node_will_read() {
    let scratch = Scratch::new("token-absent");
    let home = installed_home(&scratch, Some(&config(&scratch, &scratch.at("incus.sock"))));
    let installed = format!("{}/.config/arca", home.display());
    // What every mint advice ends with: the node reads tokens at its start or on SIGHUP, and the command that has a
    // running node read them, for this node's own config.
    let reload = format!(
        "A node reads its tokens only when it starts or on SIGHUP, so a node that is running needs this after the mint: arca token reload --config {installed}/node.json"
    );
    // The installer's own path is named, since nobody typed it, and the mint writes to it, so no flag is needed after.
    let out = arca()
        .arg("status")
        .env("HOME", &home)
        .output()
        .expect("run the binary");
    assert_eq!(out.status.code(), Some(2), "{}", stderr_text(&out));
    assert_eq!(
        stderr_text(&out),
        format!(
            "arca: the token file {installed}/assistant-1.secret, where the installer writes it, does not exist: mint one with: arca token mint --scope viewer --id TOKEN_ID --out {installed}/assistant-1.secret --config {installed}/node.json (a name for the token in place of TOKEN_ID). {reload}\n"
        )
    );
    // A path the caller gave, in the flag or in the variable, is never repeated.
    let typed = scratch.at("typed-by-the-caller.secret");
    for by_variable in [false, true] {
        let mut command = arca();
        command.args(["call", "arca_status"]).env("HOME", &home);
        if by_variable {
            command.env("ARCA_TOKEN_FILE", &typed);
        } else {
            command.arg("--token-file").arg(&typed);
        }
        let out = command.output().expect("run the binary");
        assert_eq!(out.status.code(), Some(2), "{}", stderr_text(&out));
        assert_eq!(
            stderr_text(&out),
            format!(
                "arca: the token file does not exist: mint one with: arca token mint --scope viewer --id TOKEN_ID --out TOKEN_FILE --config {installed}/node.json (a name for the token in place of TOKEN_ID, and the path to write it to in place of TOKEN_FILE). {reload}\n"
            )
        );
    }
    // A file that is there and too open is a different refusal, with its own fix.
    token_file(&typed);
    fs::set_permissions(&typed, fs::Permissions::from_mode(0o640)).expect("chmod 0640");
    let out = arca()
        .args(["call", "arca_status", "--token-file"])
        .arg(&typed)
        .env("HOME", &home)
        .output()
        .expect("run the binary");
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        stderr_text(&out),
        "arca: the token file can be read by its group or by others: run chmod 600 on it, so only its owner reads it\n"
    );
}

#[test]
fn a_mistyped_verb_is_answered_with_the_closest_one_and_call_alone_lists_them() {
    let out = arca()
        .args(["call", "watch_reprot"])
        .output()
        .expect("run the binary");
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        stderr_text(&out),
        "arca: the verb is not one the node answers; did you mean watch_report? Run arca call alone to list the verbs\n"
    );
    let out = arca().arg("call").output().expect("run the binary");
    assert_eq!(out.status.code(), Some(2));
    let listed = stderr_text(&out);
    assert!(
        listed
            .starts_with("arca: call needs a verb: arca call VERB, with one of these in place of VERB: arca_status, "),
        "{listed}"
    );
    assert!(listed.contains(", watch_report"), "{listed}");
}

#[test]
fn a_config_with_a_mistyped_field_names_the_field_and_the_file() {
    let scratch = Scratch::new("config-field");
    let mut value = config(&scratch, &scratch.at("incus.sock"));
    value["kernel"]["serviceCdir"] = json!("192.0.2.0/24");
    let path = write_config(&scratch, "typo.json", &value);
    let out = arca()
        .args(["watch", "--once", "--json", "--config"])
        .arg(&path)
        .output()
        .expect("run the binary");
    assert_eq!(out.status.code(), Some(2));
    let refusal = stderr_text(&out);
    assert!(
        refusal.starts_with(&format!(
            "arca: in the config file {}: unknown field kernel.serviceCdir; did you mean kernel.serviceCidr? (line ",
            path.display()
        )),
        "{refusal}"
    );
}

/// How long a test waits for a listener of the server under test to be bound. `serve` binds its loopback addresses
/// after it has started, each on its own task, and a node with a page binds two; on a loaded machine that takes time.
#[cfg(target_os = "linux")]
const LISTEN_WAIT: Duration = Duration::from_secs(120);

/// A connection to `at`, made as soon as something listens there: a connect that is refused is tried again until
/// [`LISTEN_WAIT`] has passed. A test never fails because it asked before the server had bound its address.
#[cfg(target_os = "linux")]
fn connect_when_bound(at: std::net::SocketAddr) -> std::net::TcpStream {
    let deadline = Instant::now() + LISTEN_WAIT;
    loop {
        match std::net::TcpStream::connect(at) {
            Ok(stream) => return stream,
            Err(error) => {
                assert!(
                    Instant::now() < deadline,
                    "nothing listens on {at} after {LISTEN_WAIT:?}: {error}"
                );
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// One HTTP/1.1 request to `at`, with `Connection: close`, and the whole reply.
#[cfg(target_os = "linux")]
fn reply_of(at: std::net::SocketAddr, head: &str, body: &str) -> String {
    let mut stream = connect_when_bound(at);
    stream.set_read_timeout(Some(WAIT)).expect("set a deadline");
    let request = format!(
        "{head}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .expect("send the request");
    let mut reply = String::new();
    let _ = stream.read_to_string(&mut reply);
    reply
}

/// The status line of [`reply_of`].
#[cfg(target_os = "linux")]
fn status_of(at: std::net::SocketAddr, head: &str, body: &str) -> String {
    reply_of(at, head, body)
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}

#[test]
#[cfg(target_os = "linux")]
fn serve_answers_on_the_loopback_listener_only_by_its_own_name_and_with_a_token() {
    let scratch = Scratch::new("serve-loopback");
    let rig = rig(&scratch);
    let state = scratch.at("state");
    fs::create_dir_all(&state).expect("create the state directory");
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
    let tokens = scratch.at("tokens");
    fs::create_dir_all(&tokens).expect("create the token directory");
    fs::set_permissions(&tokens, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
    let secret_file = scratch.at("viewer-1.secret");
    let minted = arca()
        .args(["token", "mint", "--scope", "viewer", "--id", "viewer-1"])
        .arg("--hash-dir")
        .arg(&tokens)
        .arg("--out")
        .arg(&secret_file)
        .output()
        .expect("mint a token");
    assert!(minted.status.success(), "the token is minted");
    let secret = fs::read_to_string(&secret_file).expect("the secret");
    let secret = secret.trim();

    let (mut child, at) = serve_on_a_free_port(&scratch, rig.shim.socket(), &|value, at| {
        value["listen"]["loopback"] = json!(at.to_string());
    });
    let port = at.port();
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
    let post = |host: &str, bearer: bool| {
        let mut head = format!("POST /mcp HTTP/1.1\r\nHost: {host}");
        if bearer {
            head.push_str(&format!("\r\nAuthorization: Bearer {secret}"));
        }
        status_of(at, &head, body)
    };

    // The listener's own two names reach the door, which wants the token.
    for own in [format!("127.0.0.1:{port}"), format!("localhost:{port}")] {
        assert!(
            post(&own, false).starts_with("HTTP/1.1 401"),
            "{own}: no bearer"
        );
        assert!(
            post(&own, true).starts_with("HTTP/1.1 200"),
            "{own}: the viewer"
        );
    }
    // Any other name is refused before the token is looked at: a rebound name, the right name on another port, and a
    // name with no port.
    for foreign in [
        format!("rebound.example:{port}"),
        "127.0.0.1:1".to_string(),
        "localhost".to_string(),
    ] {
        assert!(
            post(&foreign, true).starts_with("HTTP/1.1 403"),
            "{foreign}: a valid token does not help a foreign Host"
        );
    }
    // The approval page is not on this listener, and a GET is under the same rule.
    let get = |host: &str| status_of(at, &format!("GET /approve HTTP/1.1\r\nHost: {host}"), "");
    assert!(get(&format!("127.0.0.1:{port}")).starts_with("HTTP/1.1 404"));
    assert!(get(&format!("rebound.example:{port}")).starts_with("HTTP/1.1 403"));

    let kill = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("send SIGTERM");
    assert!(kill.success(), "SIGTERM was delivered");
    assert_eq!(
        wait_exit(&mut child).code(),
        Some(0),
        "serve exits 0 on SIGTERM"
    );
    assert!(
        std::net::TcpStream::connect(at).is_err(),
        "the loopback listener is closed once serve stops"
    );
    let summary = rig.shim.stop().expect("stop the shim");
    assert_eq!(summary.refused, 0, "serve sent Incus only GETs");
}

#[test]
#[cfg(target_os = "linux")]
fn a_loopback_port_another_program_holds_does_not_stop_the_node_and_the_status_says_why() {
    let scratch = Scratch::new("serve-loopback-held");
    let rig = rig(&scratch);
    let state = scratch.at("state");
    fs::create_dir_all(&state).expect("create the state directory");
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
    let tokens = scratch.at("tokens");
    fs::create_dir_all(&tokens).expect("create the token directory");
    fs::set_permissions(&tokens, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
    let secret_file = scratch.at("viewer-1.secret");
    let minted = arca()
        .args(["token", "mint", "--scope", "viewer", "--id", "viewer-1"])
        .arg("--hash-dir")
        .arg(&tokens)
        .arg("--out")
        .arg(&secret_file)
        .output()
        .expect("mint a token");
    assert!(minted.status.success(), "the token is minted");

    // Another program holds the door's port before the node starts. It stands for any process of any user.
    let held = std::net::TcpListener::bind("127.0.0.1:0").expect("another program's listener");
    let at = held.local_addr().expect("its address");
    let mut value = config(&scratch, rig.shim.socket());
    value["listen"]["loopback"] = json!(at.to_string());
    let path = write_config(&scratch, "node.json", &value);
    let log = scratch.at("serve.log");
    let mut child = Node(
        serve_command(&path)
            .stdout(Stdio::null())
            .stderr(fs::File::create(&log).expect("the server's log"))
            .spawn()
            .expect("start arca serve"),
    );
    let status = || -> Option<Value> {
        let out = arca()
            .args(["call", "arca_status", "--config"])
            .arg(&path)
            .arg("--token-file")
            .arg(&secret_file)
            .args(["--args", "{}"])
            .output()
            .expect("run arca call");
        out.status
            .success()
            .then(|| serde_json::from_slice(&out.stdout).expect("the status is JSON"))
    };
    let until = |what: &str, wait: Duration, done: &dyn Fn(&Value) -> bool| -> Value {
        let deadline = Instant::now() + wait;
        loop {
            if let Some(answer) = status()
                && done(&answer)
            {
                return answer;
            }
            assert!(
                Instant::now() < deadline,
                "{what}; the server's log:\n{}",
                fs::read_to_string(&log).unwrap_or_default()
            );
            thread::sleep(Duration::from_millis(100));
        }
    };

    // The node starts and answers on its unix socket, and says that its door is closed and why.
    let answer = until("the node answers on its unix socket", LISTEN_WAIT, &|_| {
        true
    });
    assert_eq!(answer["loopback"]["addr"], json!(at.to_string()));
    assert_eq!(answer["loopback"]["open"], json!(false), "{answer}");
    assert!(
        answer["loopback"]["reason"]
            .as_str()
            .is_some_and(|why| why.starts_with("address in use")),
        "{answer}"
    );
    let said = fs::read_to_string(&log).expect("the server's log");
    assert!(
        said.contains(&format!("the loopback door {at} is closed: address in use")),
        "{said}"
    );
    assert_eq!(
        said.matches("is closed").count(),
        1,
        "a port that stays held is said once: {said}"
    );

    // The other program lets go: the node takes the port without being started again, and says so.
    drop(held);
    let answer = until(
        "the door opens once the port is free",
        LISTEN_WAIT,
        &|answer| answer["loopback"]["open"] == json!(true),
    );
    assert_eq!(answer["loopback"]["reason"], Value::Null);
    assert!(
        status_of(
            at,
            &format!("POST /mcp HTTP/1.1\r\nHost: {at}"),
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#
        )
        .starts_with("HTTP/1.1 401"),
        "the door wants its token"
    );
    assert!(
        fs::read_to_string(&log)
            .expect("the server's log")
            .contains(&format!("the loopback door {at} is open"))
    );

    let kill = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("send SIGTERM");
    assert!(kill.success(), "SIGTERM was delivered");
    assert_eq!(
        wait_exit(&mut child).code(),
        Some(0),
        "serve exits 0 on SIGTERM"
    );
    let summary = rig.shim.stop().expect("stop the shim");
    assert_eq!(summary.refused, 0, "serve sent Incus only GETs");
}

#[test]
#[cfg(target_os = "linux")]
fn the_page_of_this_machine_is_on_the_loopback_listener_and_its_setup_code_comes_to_the_terminal() {
    let scratch = Scratch::new("serve-local-page");
    let rig = rig(&scratch);
    let state = scratch.at("state");
    fs::create_dir_all(&state).expect("create the state directory");
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
    let tokens = scratch.at("tokens");
    fs::create_dir_all(&tokens).expect("create the token directory");
    fs::set_permissions(&tokens, fs::Permissions::from_mode(0o700)).expect("chmod 0700");

    // Without a page there is nowhere to type a code, and the command says so before it touches anything.
    let plain = write_config(&scratch, "plain.json", &config(&scratch, rig.shim.socket()));
    let out = arca()
        .args(["setup-code", "--config"])
        .arg(&plain)
        .output()
        .expect("run arca setup-code");
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("approvalOrigin is not set"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let (mut child, at) = serve_on_a_free_port(&scratch, rig.shim.socket(), &|value, at| {
        value["listen"]["loopback"] = json!(at.to_string());
        value["approvalOrigin"] = json!(format!("http://localhost:{}", at.port()));
    });
    let port = at.port();
    let origin = format!("http://localhost:{port}");
    let get =
        |path: &str, host: &str| status_of(at, &format!("GET {path} HTTP/1.1\r\nHost: {host}"), "");

    // The node takes the port's other loopback address too, a moment after the first; the page opens once it holds
    // both (or once it has found that this machine has no IPv6 loopback).
    let deadline = Instant::now() + LISTEN_WAIT;
    while !get("/approve", &format!("localhost:{port}")).starts_with("HTTP/1.1 200") {
        assert!(Instant::now() < deadline, "the page opens");
        thread::sleep(Duration::from_millis(50));
    }
    // Where the machine has an IPv6 loopback, the page is there as well, under the same rules: a browser that
    // tries ::1 first for the name localhost finds this node, not an empty port another program could take.
    let six: std::net::SocketAddr = format!("[::1]:{port}").parse().expect("an address");
    if std::net::TcpStream::connect(six).is_ok() {
        let on_six = |path: &str, host: &str| {
            status_of(six, &format!("GET {path} HTTP/1.1\r\nHost: {host}"), "")
        };
        assert!(on_six("/approve", &format!("localhost:{port}")).starts_with("HTTP/1.1 200"));
        assert!(on_six("/approve", &format!("[::1]:{port}")).starts_with("HTTP/1.1 200"));
        assert!(on_six("/approve", &format!("rebound.example:{port}")).starts_with("HTTP/1.1 403"));
    }

    // The page and its cards answer the listener's own two names, with no token, and no other name.
    for own in [format!("localhost:{port}"), format!("127.0.0.1:{port}")] {
        assert!(get("/approve", &own).starts_with("HTTP/1.1 200"), "{own}");
        assert!(
            get("/approve/plans", &own).starts_with("HTTP/1.1 200"),
            "{own}"
        );
    }
    let rebound = format!("rebound.example:{port}");
    assert!(get("/approve", &rebound).starts_with("HTTP/1.1 403"));
    assert!(get("/approve/plans", &rebound).starts_with("HTTP/1.1 403"));

    // A POST needs the page's exact origin. The same page opened at the IP address is another origin, and so is a
    // rebound name that sends the right Host.
    let post = |path: &str, sent_origin: &str, body: &str| {
        reply_of(
            at,
            &format!("POST {path} HTTP/1.1\r\nHost: localhost:{port}\r\nOrigin: {sent_origin}"),
            body,
        )
    };
    for other in [
        format!("http://127.0.0.1:{port}"),
        format!("http://rebound.example:{port}"),
        format!("https://localhost:{port}"),
    ] {
        assert!(
            post(
                "/approve/setup-code/verify",
                &other,
                r#"{"setupCode":"00000000"}"#
            )
            .starts_with("HTTP/1.1 403"),
            "{other}"
        );
    }
    // The terminal's route is the unix socket's alone: the TCP listener does not carry it.
    assert!(
        status_of(
            at,
            &format!("POST /local/setup-code HTTP/1.1\r\nHost: localhost:{port}"),
            r#"{"proof":"0123456789abcdef0123456789abcdef"}"#
        )
        .starts_with("HTTP/1.1 404")
    );

    // The code comes to the terminal of the node's user, and it is the page's code.
    let node = scratch.at("node.json");
    let out = arca()
        .args(["setup-code", "--config"])
        .arg(&node)
        .output()
        .expect("run arca setup-code");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let printed: Value = serde_json::from_slice(&out.stdout).expect("JSON on stdout");
    let code = printed["code"].as_str().expect("a code");
    assert!(code.len() == 8 && code.bytes().all(|b| b.is_ascii_digit()));
    assert_eq!(printed["page"], json!(format!("{origin}/approve")));
    assert_eq!(printed["expiresInSeconds"], json!(600));
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains(code),
        "the code is printed once, on stdout"
    );
    let left: Vec<String> = fs::read_dir(&state)
        .expect("the state directory")
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name.starts_with("setup-code-request-"))
        .collect();
    assert_eq!(left, Vec::<String>::new(), "no request file is left behind");
    let verified = post(
        "/approve/setup-code/verify",
        &origin,
        &json!({ "setupCode": code }).to_string(),
    );
    assert!(verified.starts_with("HTTP/1.1 200"), "{verified}");
    assert!(verified.contains("\"verified\":true"), "{verified}");

    let kill = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("send SIGTERM");
    assert!(kill.success(), "SIGTERM was delivered");
    assert_eq!(
        wait_exit(&mut child).code(),
        Some(0),
        "serve exits 0 on SIGTERM"
    );

    // With no node running there is no code, and the request file is not left behind either.
    let out = arca()
        .args(["setup-code", "--config"])
        .arg(&node)
        .output()
        .expect("run arca setup-code");
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert_eq!(
        fs::read_dir(&state)
            .expect("the state directory")
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .filter(|name| name.starts_with("setup-code-request-"))
            .count(),
        0
    );
    let summary = rig.shim.stop().expect("stop the shim");
    assert_eq!(summary.refused, 0, "serve sent Incus only GETs");
}

#[test]
#[cfg(target_os = "linux")]
fn status_says_whether_changes_are_on_which_page_approves_and_what_to_do_next_from_the_nodes_own_answer()
 {
    let scratch = Scratch::new("status-approvals");
    let rig = rig(&scratch);
    let state = scratch.at("state");
    fs::create_dir_all(&state).expect("create the state directory");
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
    let tokens = scratch.at("tokens");
    fs::create_dir_all(&tokens).expect("create the token directory");
    fs::set_permissions(&tokens, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
    let secret_file = scratch.at("viewer-1.secret");
    let minted = arca()
        .args(["token", "mint", "--scope", "viewer", "--id", "viewer-1"])
        .arg("--hash-dir")
        .arg(&tokens)
        .arg("--out")
        .arg(&secret_file)
        .output()
        .expect("mint a token");
    assert!(minted.status.success(), "the token is minted");

    let (mut child, at) = serve_on_a_free_port(&scratch, rig.shim.socket(), &|value, at| {
        value["listen"]["loopback"] = json!(at.to_string());
        value["approvalOrigin"] = json!(format!("http://localhost:{}", at.port()));
    });
    let port = at.port();
    let page = format!("http://localhost:{port}/approve");
    let node = scratch.at("node.json");
    let command = format!(
        "arca setup-code --config {}",
        fs::canonicalize(&node)
            .expect("the config's path")
            .display()
    );
    let status = |arguments: &[&str]| {
        let out = arca()
            .arg("status")
            .args(arguments)
            .arg("--config")
            .arg(&node)
            .arg("--token-file")
            .arg(&secret_file)
            .output()
            .expect("run arca status");
        assert!(out.status.success(), "{}", stderr_text(&out));
        stdout_text(&out)
    };
    // The page opens once the node holds both loopback addresses of its port.
    let deadline = Instant::now() + LISTEN_WAIT;
    let answer = loop {
        let answer: Value = serde_json::from_str(&status(&["--json"])).expect("the status is JSON");
        if answer["approvalPage"]["open"] == json!(true) {
            break answer;
        }
        assert!(Instant::now() < deadline, "the page opens: {answer}");
        thread::sleep(Duration::from_millis(100));
    };
    // The node's own answer: the page's address, that it is open, and that no passkey is trusted.
    assert_eq!(
        answer["approvalPage"],
        json!({ "url": page, "open": true, "reason": null, "passkeys": 0 })
    );
    // In words: this node only looks, so changes are off; the page and the passkeys are still the node's word.
    let words = status(&[]);
    for said in [
        "Changes: off. This node only looks".to_string(),
        format!("Approval page: {page} (open)"),
        "Passkeys: none trusted yet, so nothing can be approved".to_string(),
        "Next: this node only looks. To let it make changes".to_string(),
        format!("arca init --enable-changes --config {}", node.display()),
    ] {
        assert!(words.contains(&said), "{said:?} is missing:\n{words}");
    }
    // One next step, and one pointer to the rest as the last line.
    assert_eq!(words.matches("Next: ").count(), 1, "{words}");
    assert!(
        words.trim_end().ends_with(&format!(
            "\nMore: arca status --security --config {}",
            node.display()
        )),
        "{words}"
    );
    // The page itself names the exact line for a machine with no screen, with this node's config by its whole path.
    let html = reply_of(
        at,
        &format!("GET /approve HTTP/1.1\r\nHost: localhost:{port}"),
        "",
    );
    assert!(
        html.contains(&format!("data-setup-command=\"{command}\"")),
        "the page carries the line for this node"
    );
    // And that line's own words give the way to the person's own computer, and promise no phone.
    let out = arca()
        .args(["setup-code", "--config"])
        .arg(&node)
        .output()
        .expect("run arca setup-code");
    assert!(out.status.success(), "{}", stderr_text(&out));
    let said = stderr_text(&out);
    assert!(
        said.contains(&format!(
            "ssh -o ExitOnForwardFailure=yes -L localhost:{port}:127.0.0.1:{port} THIS_MACHINE (this machine's name or address in place of THIS_MACHINE)"
        )),
        "{said}"
    );
    assert!(
        said.contains(&format!("then open {page} in its browser")),
        "{said}"
    );
    assert!(
        said.contains("a phone cannot open this page. Approving from a phone needs the node reachable under a name"),
        "{said}"
    );

    let kill = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("send SIGTERM");
    assert!(kill.success(), "SIGTERM was delivered");
    assert_eq!(
        wait_exit(&mut child).code(),
        Some(0),
        "serve exits 0 on SIGTERM"
    );

    // The cautions are the config's to tell: no node is asked, so they are there with the node stopped.
    let out = arca()
        .args(["status", "--security", "--config"])
        .arg(&node)
        .output()
        .expect("run arca status --security");
    assert!(out.status.success(), "{}", stderr_text(&out));
    let cautions = stdout_text(&out);
    for said in [
        format!(
            "The HTTP door, http://127.0.0.1:{port}/mcp, answers this machine only, and only with a token."
        ),
        "keeps the token in its own config, in clear text".to_string(),
        format!("The approval page, {page}, is the page of this machine alone."),
        "A phone cannot open this page.".to_string(),
    ] {
        assert!(cautions.contains(&said), "{said:?} is missing:\n{cautions}");
    }
    let both = arca()
        .args(["status", "--security", "--json", "--config"])
        .arg(&node)
        .output()
        .expect("run arca status");
    assert_eq!(both.status.code(), Some(2), "one of the two");
    let summary = rig.shim.stop().expect("stop the shim");
    assert_eq!(summary.refused, 0, "serve sent Incus only GETs");
}

#[test]
#[cfg(target_os = "linux")]
fn while_another_program_holds_the_ports_other_loopback_address_the_page_is_closed_and_no_code_is_given()
 {
    let scratch = Scratch::new("serve-twin-held");
    let rig = rig(&scratch);
    let state = scratch.at("state");
    fs::create_dir_all(&state).expect("create the state directory");
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
    let tokens = scratch.at("tokens");
    fs::create_dir_all(&tokens).expect("create the token directory");
    fs::set_permissions(&tokens, fs::Permissions::from_mode(0o700)).expect("chmod 0700");

    // Another program, of any user, takes [::1]:<port> and keeps it: the address a browser tries first for the name
    // localhost. The node's config names 127.0.0.1:<port>, which stays free for it.
    let Ok(held) = std::net::TcpListener::bind("[::1]:0") else {
        eprintln!(
            "this machine has no IPv6 loopback, so no program can hold one: the case cannot be made here \
             (the door's own test covers what the node does then)"
        );
        return;
    };
    let port = held.local_addr().expect("its address").port();
    let at: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().expect("an address");
    let origin = format!("http://localhost:{port}");
    let mut value = config(&scratch, rig.shim.socket());
    value["listen"]["loopback"] = json!(at.to_string());
    value["approvalOrigin"] = json!(origin);
    let node = write_config(&scratch, "node.json", &value);
    let log = scratch.at("serve.log");
    let mut child = Node(
        serve_command(&node)
            .stdout(Stdio::null())
            .stderr(fs::File::create(&log).expect("the server's log"))
            .spawn()
            .expect("start arca serve"),
    );
    let said = || fs::read_to_string(&log).unwrap_or_default();
    drop(connect_when_bound(at));
    let get = |path: &str| {
        reply_of(
            at,
            &format!("GET {path} HTTP/1.1\r\nHost: localhost:{port}"),
            "",
        )
    };
    let code = || {
        arca()
            .args(["setup-code", "--config"])
            .arg(&node)
            .output()
            .expect("run arca setup-code")
    };

    // The node holds its own address, and its door for MCP clients is open as ever.
    assert!(
        status_of(
            at,
            &format!("POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}"),
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#
        )
        .starts_with("HTTP/1.1 401")
    );
    // The page is closed on it, and says why: nothing is served, no code is checked, none is given.
    let closed = format!(
        "this node does not hold [::1]:{port} (address in use: another program holds this port)"
    );
    for path in ["/approve", "/approve/plans"] {
        let reply = get(path);
        assert!(reply.starts_with("HTTP/1.1 503"), "{path}: {reply}");
        assert!(reply.contains("\"refusal\":\"page_closed\""), "{reply}");
        assert!(reply.contains(&closed), "{reply}");
    }
    let verify = reply_of(
        at,
        &format!(
            "POST /approve/setup-code/verify HTTP/1.1\r\nHost: localhost:{port}\r\nOrigin: {origin}"
        ),
        r#"{"setupCode":"00000000"}"#,
    );
    assert!(verify.starts_with("HTTP/1.1 503"), "{verify}");
    let out = code();
    assert_eq!(
        out.status.code(),
        Some(2),
        "no setup code while the page is closed"
    );
    assert!(out.stdout.is_empty(), "nothing on stdout");
    let refused = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(refused.contains(&closed), "{refused}");
    assert!(
        refused.contains("type no setup code and approve nothing there"),
        "{refused}"
    );
    assert!(
        said().contains(&format!(
            "the approval page's other address [::1]:{port} is not this node's: address in use"
        )),
        "{}",
        said()
    );

    // The other program lets go. The node takes the address without being started again, the page opens, and the
    // terminal gets its code.
    drop(held);
    let deadline = Instant::now() + LISTEN_WAIT;
    while !get("/approve").starts_with("HTTP/1.1 200") {
        assert!(
            Instant::now() < deadline,
            "the page opens once both addresses are the node's: {}",
            said()
        );
        thread::sleep(Duration::from_millis(200));
    }
    let out = code();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let printed: Value = serde_json::from_slice(&out.stdout).expect("JSON on stdout");
    assert_eq!(printed["code"].as_str().map(str::len), Some(8));

    let kill = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("send SIGTERM");
    assert!(kill.success(), "SIGTERM was delivered");
    assert_eq!(
        wait_exit(&mut child).code(),
        Some(0),
        "serve exits 0 on SIGTERM"
    );
    let summary = rig.shim.stop().expect("stop the shim");
    assert_eq!(summary.refused, 0, "serve sent Incus only GETs");
}

/// A config whose state directory exists, so a test can put a pid file in it.
fn config_with_state(scratch: &Scratch) -> (PathBuf, PathBuf) {
    let state = scratch.at("state");
    fs::create_dir(&state).expect("the state directory");
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
    let path = write_config(
        scratch,
        "node.json",
        &config(scratch, &scratch.at("incus.sock")),
    );
    (path, state)
}

#[test]
fn token_reload_with_no_node_on_the_config_says_so_and_exits_non_zero() {
    let scratch = Scratch::new("reload-none");
    let (config_path, _state) = config_with_state(&scratch);
    let out = arca()
        .args(["token", "reload", "--config"])
        .arg(&config_path)
        .output()
        .expect("run the binary");
    assert_eq!(out.status.code(), Some(1), "{}", stderr_text(&out));
    assert!(out.stdout.is_empty(), "nothing is claimed on stdout");
    assert_eq!(
        stderr_text(&out),
        format!(
            "arca: no node runs on this config, so nothing was reloaded. A node reads its tokens when it starts: arca serve --config {}\n",
            config_path.display()
        )
    );
}

#[test]
fn token_reload_never_signals_the_stranger_a_stale_pid_file_names() {
    let scratch = Scratch::new("reload-stale");
    let (config_path, state) = config_with_state(&scratch);
    // A node crashed and left its pid file; the pid now belongs to another program of this user. SIGHUP would end it.
    let mut stranger = Command::new("sleep")
        .arg("300")
        .spawn()
        .expect("a stranger process");
    fs::write(state.join("serve.pid"), format!("{}\n", stranger.id())).expect("the stale pid file");
    let out = arca()
        .args(["token", "reload", "--config"])
        .arg(&config_path)
        .output()
        .expect("run the binary");
    let alive = stranger.try_wait().expect("poll the stranger").is_none();
    let _ = stranger.kill();
    let _ = stranger.wait();
    assert!(alive, "the stranger was not signalled");
    assert_eq!(out.status.code(), Some(1), "{}", stderr_text(&out));
    assert!(
        stderr_text(&out)
            .starts_with("arca: no node runs on this config, so nothing was reloaded."),
        "{}",
        stderr_text(&out)
    );
}

#[test]
fn a_program_that_only_names_arcas_words_is_never_taken_for_the_node() {
    use std::os::unix::process::CommandExt;
    let scratch = Scratch::new("reload-lookalike");
    let (config_path, _state) = config_with_state(&scratch);
    // The config's token directory, for the token the revoke below removes.
    token_dir(&scratch, "tokens");
    let minted = arca()
        .args([
            "token", "mint", "--scope", "viewer", "--id", "look-1", "--out",
        ])
        .arg(scratch.at("look-1.secret"))
        .arg("--config")
        .arg(&config_path)
        .output()
        .expect("run the binary");
    assert_eq!(minted.status.code(), Some(0), "{}", stderr_text(&minted));

    // A program of this user that is called arca and has `serve` and `--config <this config>` among its arguments, but
    // whose command is not `serve`: a shell that reads its input until the test lets go of it.
    let mut lookalike = Command::new("sh")
        .arg0("arca")
        .args(["-c", "while read -r line; do :; done", "serve", "--config"])
        .arg(&config_path)
        .stdin(Stdio::piped())
        .spawn()
        .expect("the look-alike");
    let reload = arca()
        .args(["token", "reload", "--config"])
        .arg(&config_path)
        .output()
        .expect("run the binary");
    let after_reload = lookalike.try_wait().expect("poll").is_none();
    let revoke = arca()
        .args(["token", "revoke", "look-1", "--config"])
        .arg(&config_path)
        .output()
        .expect("run the binary");
    let after_revoke = lookalike.try_wait().expect("poll").is_none();
    let _ = lookalike.kill();
    let _ = lookalike.wait();

    assert!(after_reload, "token reload did not signal the look-alike");
    assert_eq!(reload.status.code(), Some(1), "{}", stderr_text(&reload));
    assert!(
        stderr_text(&reload)
            .starts_with("arca: no node runs on this config, so nothing was reloaded."),
        "{}",
        stderr_text(&reload)
    );
    assert!(after_revoke, "token revoke did not signal the look-alike");
    assert_eq!(revoke.status.code(), Some(0), "{}", stderr_text(&revoke));
    let answer: Value = serde_json::from_slice(&revoke.stdout).expect("the revoke's answer");
    assert_eq!(answer["reloadSignalled"], json!(false));
}

#[test]
fn a_node_that_does_not_catch_sighup_yet_is_never_signalled_and_is_called_starting() {
    use std::os::unix::process::CommandExt;
    let scratch = Scratch::new("reload-starting");
    let (config_path, _state) = config_with_state(&scratch);
    // The config's token directory, for the token the revoke below removes.
    token_dir(&scratch, "tokens");
    let minted = arca()
        .args([
            "token", "mint", "--scope", "viewer", "--id", "start-1", "--out",
        ])
        .arg(scratch.at("start-1.secret"))
        .arg("--config")
        .arg(&config_path)
        .output()
        .expect("run the binary");
    assert_eq!(minted.status.code(), Some(0), "{}", stderr_text(&minted));

    // A process that is everything the check asks of a node: this user's, called arca, with the command line
    // `arca serve --config <this config>`. But SIGHUP is at its default, as it is in a node that has only just started,
    // so the signal would end it. `xargs` waits on its input, which the test holds open, and runs nothing.
    let mut starting = Command::new("xargs")
        .arg0("arca")
        .args(["serve", "--config"])
        .arg(&config_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the starting node's stand-in");
    let pid = starting.id();
    let reload = arca()
        .args(["token", "reload", "--config"])
        .arg(&config_path)
        .output()
        .expect("run the binary");
    let after_reload = starting.try_wait().expect("poll").is_none();
    let revoke = arca()
        .args(["token", "revoke", "start-1", "--config"])
        .arg(&config_path)
        .output()
        .expect("run the binary");
    let after_revoke = starting.try_wait().expect("poll").is_none();
    let _ = starting.kill();
    let _ = starting.wait();

    assert!(
        after_reload,
        "token reload did not signal the starting node"
    );
    assert_eq!(reload.status.code(), Some(1), "{}", stderr_text(&reload));
    assert!(reload.stdout.is_empty(), "nothing is claimed on stdout");
    assert_eq!(
        stderr_text(&reload),
        format!(
            "arca: the node on this config (pid {pid}) is still starting; nothing was signalled. Run this again in a moment\n"
        )
    );
    assert!(
        after_revoke,
        "token revoke did not signal the starting node"
    );
    assert_eq!(revoke.status.code(), Some(0), "{}", stderr_text(&revoke));
    let answer: Value = serde_json::from_slice(&revoke.stdout).expect("the revoke's answer");
    assert_eq!(answer["reloadSignalled"], json!(false));
    assert_eq!(
        stderr_text(&revoke),
        format!(
            "arca: the node on this config (pid {pid}) is still starting, so it was not signalled, and it may have read the token before this revoke. Run this in a moment to be sure: arca token reload --config {}\n",
            config_path.display()
        )
    );
}

#[test]
fn the_variable_alone_never_picks_the_node_for_a_command_that_hands_out_or_removes() {
    let scratch = Scratch::new("config-variable-alone");
    let (home, installed, variable, _flag) = three_nodes(&scratch);
    // A token that exists on the other node only, under an id a person would type for their own node.
    let minted = arca()
        .args([
            "token",
            "mint",
            "--scope",
            "viewer",
            "--id",
            "assistant-1",
            "--out",
        ])
        .arg(scratch.at("other-assistant-1.secret"))
        .arg("--config")
        .arg(&variable.config)
        .output()
        .expect("run the binary");
    assert_eq!(minted.status.code(), Some(0), "{}", stderr_text(&minted));
    let own = scratch.at("own.secret");
    token_file(&own);
    let own_arg = own.display().to_string();
    let out_arg = scratch.at("minted-1.secret").display().to_string();
    let cases: Vec<Vec<&str>> = vec![
        vec![
            "token", "mint", "--scope", "viewer", "--id", "minted-1", "--out", &out_arg,
        ],
        vec!["token", "revoke", "assistant-1"],
        // The reload is the step that makes a revoke take effect; pairing hands out a peer.
        vec!["token", "reload"],
        vec!["link", "pair"],
        vec![
            "call",
            "--peer",
            "host",
            "arca_status",
            "--token-file",
            &own_arg,
        ],
        vec!["link", "remove", "peer-1"],
        vec!["probe", "consumer", "remove", "--name", "web"],
        vec![
            "adopt",
            "apply",
            "--instance",
            "svc1",
            "--plan-digest",
            "00",
        ],
        vec!["adopt", "release", "--instance", "svc1"],
    ];
    // Both modes of init rewrite what a node is, so they are refused the same way. They are not run in the second half
    // below, which lets a command go on: tests/init.rs does that, with a node made for init.
    let rewriting: Vec<Vec<&str>> = vec![
        vec!["init", "--watch-only"],
        vec!["init", "--enable-changes", "--yes"],
    ];
    let refusal = format!(
        "arca: ARCA_CONFIG names {other}, not the default config ({default}). This command hands out, removes or rewrites something on one node, so the variable alone does not choose that node, and nothing was done. To act on the node ARCA_CONFIG names, run the same command with: --config {other}\n",
        other = variable.config.display(),
        default = installed.config.display()
    );
    for args in cases.iter().chain(&rewriting) {
        // The variable was left set for another node: the command refuses, does nothing, and says the flag to use.
        let out = arca()
            .args(args)
            .env("HOME", &home)
            .env("ARCA_CONFIG", &variable.config)
            .output()
            .expect("run the binary");
        assert_eq!(
            out.status.code(),
            Some(2),
            "{args:?}: {}",
            stderr_text(&out)
        );
        assert!(out.stdout.is_empty(), "{args:?} answers nothing");
        assert_eq!(stderr_text(&out), refusal, "{args:?}");
        assert!(
            variable.tokens.join("assistant-1.hash").exists(),
            "{args:?}: the other node's token was not revoked"
        );
        assert!(
            !variable.tokens.join("minted-1.hash").exists() && !Path::new(&out_arg).exists(),
            "{args:?}: no token was minted on the other node"
        );
    }
    for args in &cases {
        // The variable names the very file the default names: nothing is chosen by it, so the command goes on, and says
        // where its config came from.
        let out = arca()
            .args(args)
            .env("HOME", &home)
            .env("ARCA_CONFIG", &installed.config)
            .output()
            .expect("run the binary");
        assert!(
            stderr_text(&out).starts_with(&format!(
                "arca: using the config {} (from ARCA_CONFIG)",
                installed.config.display()
            )),
            "{args:?}: {}",
            stderr_text(&out)
        );
        // The flag names the other node outright, variable or no variable: the command acts there.
        let out = arca()
            .args(args)
            .arg("--config")
            .arg(&variable.config)
            .env("HOME", &home)
            .env("ARCA_CONFIG", &installed.config)
            .output()
            .expect("run the binary");
        assert!(
            !stderr_text(&out).contains("ARCA_CONFIG names")
                && !stderr_text(&out).contains("using the config"),
            "{args:?}: {}",
            stderr_text(&out)
        );
    }
    assert!(
        !variable.tokens.join("assistant-1.hash").exists(),
        "with the flag, the revoke reached the node it named"
    );
    // With no home directory there is no default to be the same as, and the variable still does not choose alone.
    let out = arca()
        .args(["token", "revoke", "assistant-1"])
        .env_remove("HOME")
        .env("ARCA_CONFIG", &variable.config)
        .output()
        .expect("run the binary");
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr_text(&out).starts_with(&format!(
            "arca: ARCA_CONFIG names {}, and there is no default config to compare it with. ",
            variable.config.display()
        )),
        "{}",
        stderr_text(&out)
    );
    // A command the variable may steer says a relative path in it as the whole path.
    let out = arca()
        .args(["templates", "install"])
        .current_dir(variable.config.parent().expect("the config's directory"))
        .env("HOME", &home)
        .env("ARCA_CONFIG", "variable.json")
        .output()
        .expect("run the binary");
    assert_eq!(
        stderr_text(&out),
        format!(
            "arca: using the config {} (from ARCA_CONFIG)\n",
            variable.config.display()
        )
    );
}
