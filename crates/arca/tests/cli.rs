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
        .env_remove("ARCA_TOKEN_FILE");
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
fn token_mint_needs_a_hash_dir_or_a_config() {
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
    // With neither, the mint reads the config every other command reads; here there is none.
    assert_eq!(out.status.code(), Some(2), "neither is refused");
    assert!(out.stdout.is_empty(), "nothing on stdout");
    assert!(
        stderr_text(&out).starts_with(
            "arca: there is no config at /nonexistent/arca-test-home/.config/arca/node.json"
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
    // A port is free when it is probed, but another process can take it before `serve` binds it. Then `serve` exits with
    // "Address already in use", and a connect to that port reaches the other process's listener, so a connect proves nothing
    // about the owner. The loop waits for the child's own listening socket on the port (`child_listens_on`). A `serve` that
    // exits first, because the address is already in use, is started again on a fresh port, at most five times. Any other
    // early exit fails at once with the server's stderr.
    let mut attempts = 0;
    let (mut child, at, mut stream) = loop {
        attempts += 1;
        let at = free_loopback_port();
        let mut value = config(&scratch, rig.shim.socket());
        value["listen"]["proxy"] = json!({ "addr": at.to_string(), "peer": "127.0.0.1" });
        let path = write_config(&scratch, "node.json", &value);
        let mut child = arca()
            .args(["serve", "--config"])
            .arg(&path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start arca serve");

        let deadline = Instant::now() + WAIT;
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
                    "the proxy port was taken five times: {status}\n{stderr}"
                );
                break false;
            }
            assert!(Instant::now() < deadline, "the proxy listener comes up");
            thread::sleep(Duration::from_millis(20));
        };
        if owned {
            let stream = std::net::TcpStream::connect(at).expect("connect to the proxy listener");
            break (child, at, stream);
        }
    };
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

/// Mints a viewer token `id` with the arguments `with` adds, and returns the run.
fn mint_with(scratch: &Scratch, id: &str, with: &dyn Fn(&mut Command)) -> Output {
    let mut command = arca();
    command
        .args(["token", "mint", "--scope", "viewer", "--id", id, "--out"])
        .arg(scratch.at(&format!("{id}.secret")));
    with(&mut command);
    command.output().expect("run the binary")
}

#[test]
fn the_config_is_the_flag_then_the_variable_then_the_file_the_installer_writes() {
    let scratch = Scratch::new("config-order");
    let by_default = token_dir(&scratch, "tokens-default");
    let by_variable = token_dir(&scratch, "tokens-variable");
    let by_flag = token_dir(&scratch, "tokens-flag");
    let naming = |dir: &Path| {
        let mut value = config(&scratch, &scratch.at("incus.sock"));
        value["tokens"]["hashDir"] = json!(dir.display().to_string());
        value
    };
    let home = installed_home(&scratch, Some(&naming(&by_default)));
    let variable_config = write_config(&scratch, "variable.json", &naming(&by_variable));
    let flag_config = write_config(&scratch, "flag.json", &naming(&by_flag));

    // No flag and no variable: the installer's file under the home directory.
    let out = mint_with(&scratch, "default-1", &|command| {
        command.env("HOME", &home);
    });
    assert_eq!(out.status.code(), Some(0), "{}", stderr_text(&out));
    assert!(
        by_default.join("default-1.hash").exists(),
        "the default config was read"
    );

    // The variable wins over the default.
    let out = mint_with(&scratch, "variable-1", &|command| {
        command
            .env("HOME", &home)
            .env("ARCA_CONFIG", &variable_config);
    });
    assert_eq!(out.status.code(), Some(0), "{}", stderr_text(&out));
    assert!(
        by_variable.join("variable-1.hash").exists(),
        "the variable's config was read"
    );
    assert!(!by_default.join("variable-1.hash").exists());

    // The flag wins over both.
    let out = mint_with(&scratch, "flag-1", &|command| {
        command
            .env("HOME", &home)
            .env("ARCA_CONFIG", &variable_config)
            .arg("--config")
            .arg(&flag_config);
    });
    assert_eq!(out.status.code(), Some(0), "{}", stderr_text(&out));
    assert!(
        by_flag.join("flag-1.hash").exists(),
        "the flag's config was read"
    );
    assert!(!by_variable.join("flag-1.hash").exists());

    // A hash directory alone reads no config at all, as the installer's own mint line relies on.
    let alone = token_dir(&scratch, "tokens-alone");
    let out = mint_with(&scratch, "alone-1", &|command| {
        command
            .env("HOME", &home)
            .env("ARCA_CONFIG", scratch.at("no-such-config.json"))
            .arg("--hash-dir")
            .arg(&alone);
    });
    assert_eq!(out.status.code(), Some(0), "{}", stderr_text(&out));
    assert!(alone.join("alone-1.hash").exists());
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
            "arca: there is no config at {}/.config/arca/node.json, where the installer writes it: install Arca for this user, or name another config with --config <file> or ARCA_CONFIG",
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

#[test]
fn serve_and_adopt_read_no_default_config_and_no_variable() {
    let scratch = Scratch::new("config-named");
    let value = config(&scratch, &scratch.at("incus.sock"));
    let home = installed_home(&scratch, Some(&value));
    let named = write_config(&scratch, "named.json", &value);
    for (args, why) in [
        (vec!["serve"], "serve starts a node"),
        (
            vec!["adopt", "status", "--instance", "svc1", "--json"],
            "adopt changes a node",
        ),
    ] {
        let out = arca()
            .args(&args)
            .env("HOME", &home)
            .env("ARCA_CONFIG", &named)
            .output()
            .expect("run the binary");
        assert_eq!(
            out.status.code(),
            Some(2),
            "{args:?}: {}",
            stderr_text(&out)
        );
        assert!(
            stderr_text(&out).starts_with(&format!(
                "arca: --config is required: {why}, so it reads no default config\nusage:\n  arca {} ",
                args[0]
            )),
            "{args:?}: {}",
            stderr_text(&out)
        );
        assert!(out.stdout.is_empty());
    }
}

#[test]
fn help_says_the_defaults_and_the_order_they_are_taken_in() {
    let out = arca().arg("--help").output().expect("run the binary");
    assert_eq!(out.status.code(), Some(0));
    let help = stdout_text(&out);
    for line in [
        "  status [--json] [--config <file>] [--token-file <file>]",
        "  --config      ~/.config/arca/node.json",
        "  --token-file  ~/.config/arca/assistant-1.secret",
        "  The order is: the flag, then the variable (ARCA_CONFIG, ARCA_TOKEN_FILE), then the default.",
        "  serve --config <file>",
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
            "arca: that is not a token action: it is one of mint, revoke\n",
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
fn status_of_a_node_that_is_not_running_says_so_and_how_to_start_it() {
    let scratch = Scratch::new("status-down");
    let home = installed_home(&scratch, Some(&config(&scratch, &scratch.at("incus.sock"))));
    token_file(&home.join(".config/arca/assistant-1.secret"));
    let out = arca()
        .arg("status")
        .env("HOME", &home)
        .output()
        .expect("run the binary");
    assert_eq!(out.status.code(), Some(1), "{}", stderr_text(&out));
    assert!(out.stdout.is_empty());
    assert_eq!(
        stderr_text(&out),
        "arca: the node's socket does not answer: the node is not running, or listen.unix in the config names another socket. Start the node with: systemctl --user start arca.service\n"
    );
}

#[test]
fn a_token_file_that_is_not_there_says_so_and_how_to_mint_one() {
    let scratch = Scratch::new("token-absent");
    let home = installed_home(&scratch, Some(&config(&scratch, &scratch.at("incus.sock"))));
    // The installer's own path is named: nobody typed it.
    let out = arca()
        .arg("status")
        .env("HOME", &home)
        .output()
        .expect("run the binary");
    assert_eq!(out.status.code(), Some(2), "{}", stderr_text(&out));
    assert_eq!(
        stderr_text(&out),
        format!(
            "arca: the token file {}/.config/arca/assistant-1.secret, where the installer writes it, does not exist: mint one with: arca token mint --scope viewer --id <id> --out <file> (then pass --token-file <file>)\n",
            home.display()
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
            "arca: the token file does not exist: mint one with: arca token mint --scope viewer --id <id> --out <file>\n"
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
            .starts_with("arca: call needs a verb: arca call <verb>. The verbs are: arca_status, "),
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
