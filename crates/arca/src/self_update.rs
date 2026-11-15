//! The kernel updater: every Arca program on this machine follows Arca's releases, with nobody at a terminal.
//!
//! `arca serve` looks on a schedule (the node config's `update` key: on by default, every six hours, releases only, at
//! any hour). When a look is due it starts `arca update run` as a transient systemd unit of its own manager, so the run
//! outlives the restarts it makes. One run:
//!
//! 1. takes the machine's update lock, so two nodes of one machine never update at once;
//! 2. reads the releases of `automagik-si/arca` (`update.source`) and picks the newest of the channel;
//! 3. finds every systemd unit of its manager whose `ExecStart` runs an Arca program: the installer's `arca.service`,
//!    the guide's hand-made units, a link, a second node with its own program path. A program is a file that answers
//!    `--version` with `Automagik Arca <version>`; units are grouped by the file their path resolves to;
//! 4. keeps each program that is as new as the release, or newer, as it is; never puts an older program over a newer one;
//! 5. waits, outside `update.window`, and while any node it would restart has an owner approval card waiting;
//! 6. downloads the binary for this machine's architecture, the release's `SHA256SUMS` and `install.sh`, and checks both
//!    files against `SHA256SUMS` and the binary's own `--version`; a file that does not match keeps every old program;
//! 7. puts the binary in place as the installer does: written beside the program and renamed over it in one step, with
//!    the old one kept as `<program>.previous`. A program on a file system it cannot write (Arca OS's image) is staged
//!    in the node's state directory instead, and that node's `serve` hands over to it at its start ([`hand_over`]);
//! 8. restarts the units of the programs it changed one at a time, shims first, then nodes, then links, and reads back
//!    the version each one's main process runs. A unit that does not come back, or comes back with another version,
//!    rolls every program back and restarts the units again, and the `kernel-update-failed` alert fires;
//! 9. runs `arca templates install` for each node that has templates, as the installer does on an upgrade.
//!
//! Every look and every step is journalled in `<stateDir>/kernel-update/journal.jsonl` of each node the run touched, and
//! the last result is in `<stateDir>/kernel-update/state.json`, which `arca_status` and the alert read.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::config::{NodeConfig, UpdateChannel, UpdateConfig};
use crate::handlers::ReleaseVersion as Version;
use crate::{Failure, Flags};

/// The updater's directory in a node's state directory.
pub const UPDATE_DIR: &str = "kernel-update";
/// The last result, in [`UPDATE_DIR`].
pub const STATE_FILE: &str = "state.json";
/// Every look and step, one JSON line each, in [`UPDATE_DIR`].
pub const JOURNAL_FILE: &str = "journal.jsonl";
/// How many owner approval cards wait on the node, written by its `serve` each minute, in [`UPDATE_DIR`].
pub const WAITING_FILE: &str = "waiting.json";
/// The program a node hands over to at its start, when its own program cannot be replaced, in [`UPDATE_DIR`].
pub const STAGED_FILE: &str = "current.json";
/// The staged program before the last one, for a rollback, in [`UPDATE_DIR`].
pub const STAGED_PREVIOUS_FILE: &str = "previous.json";
/// Set in the environment of a `serve` that a hand-over started, so it never hands over again.
pub const HANDED_OVER: &str = "ARCA_HANDED_OVER";
/// The lock one run holds, in the manager's runtime directory.
const LOCK_FILE: &str = "arca-update.lock";
/// A waiting count older than this, in milliseconds, is not read: its node is not running.
const WAITING_MAX_AGE_MS: u64 = 180_000;
/// The most a release listing may hold.
const MAX_LISTING: usize = 8 * 1024 * 1024;
/// The most one release asset may hold.
const MAX_ASSET: usize = 512 * 1024 * 1024;
/// How long a program has to say its version.
const VERSION_WAIT: Duration = Duration::from_secs(15);
/// How long one `systemctl` call may take. A restart waits for the unit's start, which waits for its own start steps.
const SYSTEMCTL_WAIT: Duration = Duration::from_secs(180);
/// The first look waits this long after a node starts, so a node that an update just restarted settles first.
const FIRST_LOOK_AFTER: Duration = Duration::from_secs(300);
/// A run that was put off (a waiting card, outside the window, no answer from the releases) is tried again this soon.
const RETRY_EVERY_MS: u64 = 900_000;

/// The product line every Arca program prints for `--version`.
const VERSION_PREFIX: &str = "Automagik Arca ";

/// The version in a `--version` line: `Automagik Arca <version> (<commit>, <target>)`.
pub fn said_version(line: &str) -> Option<String> {
    let rest = line.trim().strip_prefix(VERSION_PREFIX)?;
    let version = rest.split_whitespace().next()?;
    Version::parse(version).map(|_| version.to_string())
}

/// The version the program at `path` says it is, or `None` when it is not an Arca program or does not answer.
pub fn program_version(path: &Path) -> Option<String> {
    let mut command = Command::new(path);
    command.arg("--version");
    let (ok, out, _) = run_for(&mut command, VERSION_WAIT).ok()?;
    ok.then(|| out.lines().next().and_then(said_version))?
}

/// Runs a command for at most `wait`: whether it succeeded, its stdout and its stderr. A command still running at the
/// end is killed.
fn run_for(command: &mut Command, wait: Duration) -> Result<(bool, String, String), String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // A program written a moment ago may still be open for writing in a child another thread is forking (ETXTBSY):
    // that passes within moments.
    let mut tries = 0;
    let mut child = loop {
        match command.spawn() {
            Ok(child) => break child,
            Err(e) if e.raw_os_error() == Some(26) && tries < 20 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return Err(format!("could not start {:?}: {e}", command.get_program())),
        }
    };
    let drain = |pipe: Option<Box<dyn io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut text = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.by_ref().take(1 << 20).read_to_end(&mut text);
            }
            String::from_utf8_lossy(&text).into_owned()
        })
    };
    let out = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn io::Read + Send>),
    );
    let err = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn io::Read + Send>),
    );
    let deadline = Instant::now() + wait;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "{:?} did not end within {} s",
                    command.get_program(),
                    wait.as_secs()
                ));
            }
            Err(e) => return Err(e.to_string()),
        }
    };
    Ok((
        status.success(),
        out.join().unwrap_or_default(),
        err.join().unwrap_or_default(),
    ))
}

/// The epoch milliseconds now.
fn now_ms() -> u64 {
    crate::handlers::now_ms()
}

// -------------------------------------------------------------------------------------------------------------------
// Releases
// -------------------------------------------------------------------------------------------------------------------

/// One release of the channel: its version and its assets by name.
#[derive(Debug, Clone)]
pub struct Release {
    pub version: Version,
    pub version_text: String,
    pub tag: String,
    pub assets: BTreeMap<String, String>,
}

/// The newest release of `channel` in a GitHub releases listing. A draft is never one; a pre-release only in the
/// `prerelease` channel; a tag that is not `v<semantic version>` is skipped.
pub fn newest_release(listing: &str, channel: UpdateChannel) -> Result<Option<Release>, String> {
    let releases: Vec<Value> = serde_json::from_str(listing)
        .map_err(|_| "the releases listing is not a JSON list".to_string())?;
    let mut newest: Option<Release> = None;
    for release in releases {
        if release["draft"].as_bool().unwrap_or(false) {
            continue;
        }
        if release["prerelease"].as_bool().unwrap_or(false) && channel == UpdateChannel::Stable {
            continue;
        }
        let Some(tag) = release["tag_name"].as_str() else {
            continue;
        };
        let Some(text) = tag.strip_prefix('v') else {
            continue;
        };
        let Some(version) = Version::parse(text) else {
            continue;
        };
        if newest
            .as_ref()
            .is_some_and(|newest| newest.version >= version)
        {
            continue;
        }
        let assets = release["assets"]
            .as_array()
            .map(|assets| {
                assets
                    .iter()
                    .filter_map(|asset| {
                        Some((
                            asset["name"].as_str()?.to_string(),
                            asset["browser_download_url"].as_str()?.to_string(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        newest = Some(Release {
            version,
            version_text: text.to_string(),
            tag: tag.to_string(),
            assets,
        });
    }
    Ok(newest)
}

/// The release binary's asset name for this machine.
pub fn asset_name(version: &str, arch: &str) -> String {
    format!("arca-{version}-linux-{arch}")
}

/// The checksum `SHA256SUMS` gives `name`, in lowercase hex.
pub fn checksum_of(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (sum, file) = line.trim().split_once(char::is_whitespace)?;
        let file = file.trim_start();
        let file = file.strip_prefix('*').unwrap_or(file);
        (file == name && sum.len() == 64 && sum.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| sum.to_ascii_lowercase())
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 16];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// The updater's HTTP client: TLS through rustls and ring with the Mozilla roots, so a static program needs no system
/// certificate store. An `https` source follows only `https` redirects (GitHub's assets redirect to its storage).
pub struct Http {
    runtime: tokio::runtime::Runtime,
    client: reqwest::Client,
}

impl Http {
    pub fn new(source: &str) -> Result<Self, String> {
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|_| "TLS has no protocol version".to_string())?
        .with_root_certificates(roots)
        .with_no_client_auth();
        let client = reqwest::Client::builder()
            .tls_backend_preconfigured(tls)
            .redirect(reqwest::redirect::Policy::limited(10))
            .https_only(source.starts_with("https://"))
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(900))
            .user_agent(concat!("arca-updater/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| "the HTTPS client could not be built".to_string())?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self { runtime, client })
    }

    /// The body at `url`, at most `max` bytes, or why it could not be read.
    pub fn get(&self, url: &str, max: usize) -> Result<Vec<u8>, String> {
        self.runtime.block_on(async {
            let mut response = self
                .client
                .get(url)
                .header(
                    "Accept",
                    "application/vnd.github+json, application/octet-stream",
                )
                .send()
                .await
                .map_err(|e| format!("{url} did not answer: {}", without_url(&e)))?;
            if !response.status().is_success() {
                return Err(format!("{url} answered {}", response.status().as_u16()));
            }
            let mut body = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|e| format!("{url} was cut short: {}", without_url(&e)))?
            {
                if body.len() + chunk.len() > max {
                    return Err(format!("{url} is larger than {max} bytes"));
                }
                body.extend_from_slice(&chunk);
            }
            Ok(body)
        })
    }
}

fn without_url(error: &reqwest::Error) -> String {
    let mut text = error.to_string();
    if let Some(url) = error.url() {
        text = text.replace(url.as_str(), "the address");
    }
    text
}

// -------------------------------------------------------------------------------------------------------------------
// Units
// -------------------------------------------------------------------------------------------------------------------

/// What a unit's program does, which orders the restarts: shims, nodes, links, then the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Shim,
    Node,
    Link,
    Other,
}

impl Role {
    fn of(args: &[String]) -> Role {
        match args.first().map(String::as_str) {
            Some("shim") => Role::Shim,
            Some("serve") => Role::Node,
            Some("link") => Role::Link,
            _ => Role::Other,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shim => "shim",
            Self::Node => "node",
            Self::Link => "link",
            Self::Other => "other",
        }
    }
}

/// One unit that runs an Arca program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    pub name: String,
    /// The program path its `ExecStart` names.
    pub program: PathBuf,
    /// Its arguments after the program.
    pub args: Vec<String>,
    pub active: bool,
}

impl Unit {
    pub fn role(&self) -> Role {
        Role::of(&self.args)
    }

    /// The config a node unit names with `--config`.
    pub fn config(&self) -> Option<PathBuf> {
        let mut args = self.args.iter();
        while let Some(arg) = args.next() {
            if arg == "--config" {
                return args.next().map(PathBuf::from);
            }
            if let Some(path) = arg.strip_prefix("--config=") {
                return Some(PathBuf::from(path));
            }
        }
        None
    }
}

/// What a unit runs now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Running {
    pub active: bool,
    pub pid: u32,
    pub restarts: u64,
}

/// The service manager the updater works through. A node uses [`Systemd`]; a test gives a fake one.
pub trait Units {
    /// The units that may run an Arca program: their name, their program path and its arguments.
    fn list(&self) -> Result<Vec<Unit>, String>;
    fn restart(&self, unit: &str) -> Result<(), String>;
    fn running(&self, unit: &str) -> Result<Running, String>;
    /// The version the unit's main process runs, as that process's own program says.
    fn running_version(&self, unit: &str, running: &Running) -> Option<String>;
}

/// The systemd manager this process belongs to: the system's for root, else the user's.
pub struct Systemd {
    pub user: bool,
}

impl Systemd {
    pub fn of_this_process() -> Self {
        Self {
            user: !rustix::process::geteuid().is_root(),
        }
    }

    fn systemctl(&self) -> Command {
        let mut command = Command::new("systemctl");
        if self.user {
            command.arg("--user");
        }
        command.arg("--no-pager");
        command
    }

    fn call(&self, args: &[&str]) -> Result<String, String> {
        let mut command = self.systemctl();
        command.args(args);
        let (ok, out, err) = run_for(&mut command, SYSTEMCTL_WAIT)?;
        if ok {
            Ok(out)
        } else {
            Err(format!(
                "systemctl {} failed: {}",
                args.first().copied().unwrap_or_default(),
                err.lines().next().unwrap_or("it said nothing")
            ))
        }
    }
}

/// The `path=` and `argv[]=` of the first command in a `systemctl show` `ExecStart` value.
pub fn exec_start(value: &str) -> Option<(PathBuf, Vec<String>)> {
    let field = |key: &str| -> Option<&str> {
        let start = value.find(key)? + key.len();
        let rest = &value[start..];
        Some(rest.split(" ;").next().unwrap_or(rest).trim())
    };
    let path = field("path=")?;
    let argv: Vec<String> = field("argv[]=")?
        .split_whitespace()
        .map(str::to_string)
        .collect();
    (!path.is_empty() && !argv.is_empty()).then(|| (PathBuf::from(path), argv[1..].to_vec()))
}

/// The blocks of `systemctl show` for several units, each as its properties.
fn show_blocks(text: &str) -> Vec<BTreeMap<String, String>> {
    let mut blocks = Vec::new();
    let mut block = BTreeMap::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            if !block.is_empty() {
                blocks.push(std::mem::take(&mut block));
            }
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            block.insert(key.to_string(), value.to_string());
        }
    }
    if !block.is_empty() {
        blocks.push(block);
    }
    blocks
}

/// Whether a program path may be an Arca program, before it is asked: its file name starts with `arca`.
fn may_be_arca(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("arca"))
}

impl Units for Systemd {
    fn list(&self) -> Result<Vec<Unit>, String> {
        let listing = self.call(&[
            "list-units",
            "--type=service",
            "--all",
            "--plain",
            "--no-legend",
        ])?;
        let names: Vec<String> = listing
            .lines()
            .filter_map(|line| {
                line.split_whitespace()
                    .find(|word| word.ends_with(".service"))
                    .map(str::to_string)
            })
            // A run of the updater is never one of the units it restarts.
            .filter(|name| !name.starts_with("arca-update-"))
            .collect();
        let mut units = Vec::new();
        for chunk in names.chunks(64) {
            let mut args = vec![
                "show",
                "-p",
                "Id",
                "-p",
                "ExecStart",
                "-p",
                "ActiveState",
                "--",
            ];
            args.extend(chunk.iter().map(String::as_str));
            for block in show_blocks(&self.call(&args)?) {
                let (Some(name), Some(exec)) = (block.get("Id"), block.get("ExecStart")) else {
                    continue;
                };
                let Some((program, args)) = exec_start(exec) else {
                    continue;
                };
                if !may_be_arca(&program) || args.first().is_some_and(|arg| arg == "update") {
                    continue;
                }
                units.push(Unit {
                    name: name.clone(),
                    program,
                    args,
                    active: block
                        .get("ActiveState")
                        .is_some_and(|state| state == "active"),
                });
            }
        }
        Ok(units)
    }

    fn restart(&self, unit: &str) -> Result<(), String> {
        self.call(&["restart", "--", unit]).map(|_| ())
    }

    fn running(&self, unit: &str) -> Result<Running, String> {
        let text = self.call(&[
            "show",
            "-p",
            "ActiveState",
            "-p",
            "MainPID",
            "-p",
            "NRestarts",
            "--",
            unit,
        ])?;
        let block = show_blocks(&text).into_iter().next().unwrap_or_default();
        Ok(Running {
            active: block
                .get("ActiveState")
                .is_some_and(|state| state == "active"),
            pid: block
                .get("MainPID")
                .and_then(|pid| pid.parse().ok())
                .unwrap_or(0),
            restarts: block
                .get("NRestarts")
                .and_then(|count| count.parse().ok())
                .unwrap_or(0),
        })
    }

    fn running_version(&self, _unit: &str, running: &Running) -> Option<String> {
        (running.pid > 0)
            .then(|| program_version(&PathBuf::from(format!("/proc/{}/exe", running.pid))))?
    }
}

// -------------------------------------------------------------------------------------------------------------------
// Staged programs (a program on a file system the updater cannot write, such as Arca OS's image)
// -------------------------------------------------------------------------------------------------------------------

/// A program staged in a node's state directory, as its manifest names it and checked against it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Staged {
    pub path: PathBuf,
    pub version: String,
    pub sha256: String,
}

fn staged_manifest(dir: &Path, file: &str) -> Option<Staged> {
    let text = fs::read_to_string(dir.join(file)).ok()?;
    let manifest: Value = serde_json::from_str(&text).ok()?;
    let name = manifest["file"].as_str()?;
    if name.is_empty() || name.contains('/') || name.starts_with('.') {
        return None;
    }
    Some(Staged {
        path: dir.join(name),
        version: manifest["version"].as_str()?.to_string(),
        sha256: manifest["sha256"].as_str()?.to_string(),
    })
}

/// The program staged for the node with this state directory, when there is one this process may run: a regular file
/// of this user's that nobody else may write, whose content is the manifest's.
pub fn staged_program(state_dir: &Path) -> Option<Staged> {
    let staged = staged_manifest(&state_dir.join(UPDATE_DIR), STAGED_FILE)?;
    let meta = fs::symlink_metadata(&staged.path).ok()?;
    let own = rustix::process::geteuid().as_raw();
    if !meta.file_type().is_file() || meta.uid() != own || meta.mode() & 0o022 != 0 {
        return None;
    }
    (sha256_file(&staged.path).ok()? == staged.sha256).then_some(staged)
}

/// At the start of `serve`: when the node's state directory holds a staged program newer than this one, the process
/// becomes that program, with the same arguments. A staged program that is not newer is left alone, so an OS update
/// that brought a newer program wins. Nothing here stops the start: a hand-over that fails is said, and this program
/// goes on.
pub fn hand_over(state_dir: &Path) {
    use std::os::unix::process::CommandExt as _;
    if std::env::var_os(HANDED_OVER).is_some() {
        return;
    }
    let Some(staged) = staged_program(state_dir) else {
        return;
    };
    let (Some(staged_version), Some(own)) = (
        Version::parse(&staged.version),
        Version::parse(env!("CARGO_PKG_VERSION")),
    ) else {
        return;
    };
    if staged_version <= own {
        return;
    }
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    eprintln!(
        "arca: this program is {}; handing over to the kernel update {} staged at {}",
        env!("CARGO_PKG_VERSION"),
        staged.version,
        staged.path.display()
    );
    let error = Command::new(&staged.path)
        .args(args)
        .env(HANDED_OVER, "1")
        .exec();
    eprintln!(
        "arca: the hand-over to {} failed ({error}); this program goes on",
        staged.path.display()
    );
}

// -------------------------------------------------------------------------------------------------------------------
// Records
// -------------------------------------------------------------------------------------------------------------------

/// The private file `path` written whole in one step.
fn write_private(path: &Path, text: &str) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let temp = dir.join(format!(
        ".{}.{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    fs::rename(&temp, path)
}

/// The last result of the updater on the node with this state directory, or `None` when it never ran there.
pub fn read_state(state_dir: &Path) -> Option<Value> {
    let text = fs::read_to_string(state_dir.join(UPDATE_DIR).join(STATE_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

/// The node state directories one run records in: each keeps its own journal and last result.
struct Records {
    dirs: BTreeSet<PathBuf>,
}

impl Records {
    fn line(&self, step: &str, detail: Value) {
        let mut line = json!({ "atMs": now_ms(), "step": step });
        if let (Some(line), Some(detail)) = (line.as_object_mut(), detail.as_object()) {
            line.extend(detail.clone());
        }
        let text = format!("{line}\n");
        for dir in &self.dirs {
            let path = dir.join(UPDATE_DIR).join(JOURNAL_FILE);
            let written = fs::create_dir_all(dir.join(UPDATE_DIR)).and_then(|()| {
                OpenOptions::new()
                    .append(true)
                    .create(true)
                    .mode(0o600)
                    .open(&path)?
                    .write_all(text.as_bytes())
            });
            if let Err(error) = written {
                eprintln!(
                    "arca: the update journal {} was not written: {error}",
                    path.display()
                );
            }
        }
    }

    /// The last result, merged over what the record held, so a look keeps the last update's time.
    fn state(&self, fields: Value) {
        for dir in &self.dirs {
            let mut state = read_state(dir).unwrap_or_else(|| json!({}));
            if let (Some(state), Some(fields)) = (state.as_object_mut(), fields.as_object()) {
                for (key, value) in fields {
                    state.insert(key.clone(), value.clone());
                }
            }
            let path = dir.join(UPDATE_DIR).join(STATE_FILE);
            if let Err(error) = write_private(&path, &format!("{state}\n")) {
                eprintln!(
                    "arca: the update state {} was not written: {error}",
                    path.display()
                );
            }
        }
    }
}

/// The count of waiting cards a node's `serve` wrote, when it is fresh.
fn waiting_on(state_dir: &Path, now: u64) -> Option<u64> {
    let text = fs::read_to_string(state_dir.join(UPDATE_DIR).join(WAITING_FILE)).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let at = value["atMs"].as_u64()?;
    (now.saturating_sub(at) <= WAITING_MAX_AGE_MS).then(|| value["count"].as_u64())?
}

/// Writes how many cards wait on this node, for the updater.
pub fn write_waiting(state_dir: &Path, count: usize) {
    let path = state_dir.join(UPDATE_DIR).join(WAITING_FILE);
    let _ = write_private(
        &path,
        &format!("{}\n", json!({ "count": count, "atMs": now_ms() })),
    );
}

// -------------------------------------------------------------------------------------------------------------------
// One run
// -------------------------------------------------------------------------------------------------------------------

/// One program file and the units that run it.
#[derive(Debug, Clone)]
struct Program {
    /// The file the units' paths resolve to.
    path: PathBuf,
    /// What the units' paths say, for the words.
    named: BTreeSet<PathBuf>,
    /// The version the file says, or a staged program's when that is newer.
    version: String,
    units: Vec<Unit>,
    /// The state directory of a node that runs it, where it may be staged.
    state_dir: Option<PathBuf>,
}

/// How one program was changed, so it can be put back.
#[derive(Debug, Clone)]
enum Changed {
    /// Renamed over in place; the old file is at `previous`.
    InPlace { path: PathBuf, previous: PathBuf },
    /// Staged in a node's state directory; the earlier manifest, if any, is kept as `previous.json`.
    Staged { dir: PathBuf, had_previous: bool },
}

/// What one run needs from outside: the service manager, the clock's pace and where the installer keeps its note.
pub struct Run<'a> {
    pub config: &'a NodeConfig,
    pub units: &'a dyn Units,
    /// This machine's architecture, as the release names it (`x86_64`, `aarch64`).
    pub arch: &'a str,
    /// How long a restarted unit may take to come back, and how long it must then stay up.
    pub health: Duration,
    pub settle: Duration,
    pub poll: Duration,
    /// The installer's own note of the program it installed (`~/.local/state/arca-install/program.sha256`).
    pub install_note: Option<PathBuf>,
    /// The machine's update lock.
    pub lock: PathBuf,
    /// The time of day, minutes in UTC, for the window.
    pub minute_of_day: u32,
}

/// What one run ended with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// `up-to-date`, `updated`, `deferred`, `waiting-for-window`, `check-failed`, `failed`, `rolled-back` or `busy`.
    pub result: &'static str,
    pub reason: Option<String>,
    pub available: Option<String>,
    pub detail: Value,
}

impl Outcome {
    fn new(result: &'static str, reason: Option<String>, available: Option<String>) -> Self {
        Self {
            result,
            reason,
            available,
            detail: json!({}),
        }
    }

    pub fn json(&self) -> Value {
        json!({
            "result": self.result,
            "reason": self.reason,
            "available": self.available,
            "detail": self.detail,
        })
    }

    /// Whether the run failed in a way the alert names: a release that did not verify, or one rolled back.
    pub fn failed(&self) -> bool {
        matches!(self.result, "failed" | "rolled-back")
    }
}

/// Whether `minute` is inside the window `start..end` (over midnight when `end < start`).
pub fn in_window(window: Option<(u32, u32)>, minute: u32) -> bool {
    match window {
        None => true,
        Some((start, end)) if start < end => (start..end).contains(&minute),
        Some((start, end)) => minute >= start || minute < end,
    }
}

impl Run<'_> {
    /// One look, and an update when one is due. Everything is journalled; only the lock held by another run is not.
    pub fn run(&self, install: bool) -> Outcome {
        let lock = match self.take_lock() {
            Ok(lock) => lock,
            Err(reason) => return Outcome::new("busy", Some(reason), None),
        };
        let mut records = Records {
            dirs: BTreeSet::from([self.config.state_dir.clone()]),
        };
        let outcome = self.look(install, &mut records);
        let now = now_ms();
        let mut fields = json!({
            "checkedAtMs": now,
            "result": outcome.result,
            "reason": outcome.reason,
            "failed": outcome.failed(),
        });
        if let Some(available) = &outcome.available {
            fields["available"] = json!(available);
        }
        if outcome.result == "updated" {
            fields["updatedAtMs"] = json!(now);
            fields["updated"] = outcome.detail["units"].clone();
        }
        // `arca update check` only looks: it writes no record.
        if install {
            records.state(fields);
            records.line(
                "check",
                json!({
                    "result": outcome.result,
                    "reason": outcome.reason,
                    "available": outcome.available,
                }),
            );
        }
        drop(lock);
        outcome
    }

    fn take_lock(&self) -> Result<File, String> {
        if let Some(dir) = self.lock.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&self.lock)
            .map_err(|e| {
                format!(
                    "the update lock {} cannot be opened: {e}",
                    self.lock.display()
                )
            })?;
        file.try_lock()
            .map_err(|_| "another update run holds the lock".to_string())?;
        Ok(file)
    }

    /// The programs the units run, grouped by the file each path resolves to. A path that does not answer as an Arca
    /// program is not one.
    fn programs(&self, units: Vec<Unit>) -> Vec<Program> {
        let mut by_file: BTreeMap<PathBuf, Program> = BTreeMap::new();
        let mut versions: BTreeMap<PathBuf, Option<String>> = BTreeMap::new();
        for unit in units {
            let file = fs::canonicalize(&unit.program).unwrap_or_else(|_| unit.program.clone());
            let version = versions
                .entry(file.clone())
                .or_insert_with(|| program_version(&file))
                .clone();
            let Some(version) = version else {
                continue;
            };
            let state_dir = (unit.role() == Role::Node)
                .then(|| node_config(&unit))
                .flatten()
                .map(|config| config.state_dir);
            let program = by_file.entry(file.clone()).or_insert_with(|| Program {
                path: file.clone(),
                named: BTreeSet::new(),
                version: version.clone(),
                units: Vec::new(),
                state_dir: None,
            });
            program.named.insert(unit.program.clone());
            if program.state_dir.is_none() {
                program.state_dir = state_dir;
            }
            program.units.push(unit);
        }
        // A program staged in a node's state directory is what that node runs when it is newer than the file.
        for program in by_file.values_mut() {
            if let Some(staged) = program.state_dir.as_deref().and_then(staged_program)
                && newer(&staged.version, &program.version)
            {
                program.version = staged.version;
            }
        }
        by_file.into_values().collect()
    }

    fn look(&self, install: bool, records: &mut Records) -> Outcome {
        let update = &self.config.update;
        let http = match Http::new(&update.source) {
            Ok(http) => http,
            Err(reason) => return Outcome::new("check-failed", Some(reason), None),
        };
        let listing = match http.get(
            &format!("{}/releases?per_page=30", update.source),
            MAX_LISTING,
        ) {
            Ok(body) => String::from_utf8_lossy(&body).into_owned(),
            Err(reason) => return Outcome::new("check-failed", Some(reason), None),
        };
        let release = match newest_release(&listing, update.channel) {
            Ok(Some(release)) => release,
            Ok(None) => {
                return Outcome::new(
                    "up-to-date",
                    Some(format!(
                        "no {} release is published",
                        update.channel.as_str()
                    )),
                    None,
                );
            }
            Err(reason) => return Outcome::new("check-failed", Some(reason), None),
        };
        let available = Some(release.version_text.clone());
        let units = match self.units.list() {
            Ok(units) => units,
            Err(reason) => return Outcome::new("check-failed", Some(reason), available),
        };
        let programs = self.programs(units);
        for program in &programs {
            if let Some(dir) = &program.state_dir {
                records.dirs.insert(dir.clone());
            }
        }
        let behind: Vec<&Program> = programs
            .iter()
            .filter(|program| newer(&release.version_text, &program.version))
            .collect();
        let mut outcome = Outcome::new("up-to-date", None, available.clone());
        outcome.detail = json!({
            "programs": programs.iter().map(|program| json!({
                "path": program.path,
                "version": program.version,
                "units": program.units.iter().map(|unit| &unit.name).collect::<Vec<_>>(),
                "behind": newer(&release.version_text, &program.version),
            })).collect::<Vec<_>>(),
        });
        if behind.is_empty() {
            if programs.is_empty() {
                outcome.reason = Some("no unit of this manager runs an Arca program".to_string());
            }
            return outcome;
        }
        if !install {
            outcome.result = "available";
            return outcome;
        }
        let window = update
            .window
            .as_deref()
            .and_then(crate::config::update_window);
        if !in_window(window, self.minute_of_day) {
            outcome.result = "waiting-for-window";
            outcome.reason = update
                .window
                .as_ref()
                .map(|w| format!("installs start inside {w} UTC"));
            return outcome;
        }
        // A node that would restart while the owner has a card open would lose the card's page and its session.
        let now = now_ms();
        let waiting: Vec<String> = behind
            .iter()
            .flat_map(|program| program.units.iter())
            .filter(|unit| unit.active && unit.role() == Role::Node)
            .filter_map(|unit| {
                let dir = node_config(unit)?.state_dir;
                let count = waiting_on(&dir, now)?;
                (count > 0).then(|| format!("{} ({count} waiting)", unit.name))
            })
            .collect();
        if !waiting.is_empty() {
            outcome.result = "deferred";
            outcome.reason = Some(format!(
                "an owner approval card waits on {}; the update starts once none waits",
                waiting.join(", ")
            ));
            records.line(
                "deferred",
                json!({ "release": release.version_text, "nodes": waiting }),
            );
            return outcome;
        }
        self.install(&http, &release, &behind, records, outcome)
    }

    fn install(
        &self,
        http: &Http,
        release: &Release,
        behind: &[&Program],
        records: &Records,
        mut outcome: Outcome,
    ) -> Outcome {
        let failed = |mut outcome: Outcome, reason: String| {
            outcome.result = "failed";
            records.line(
                "failed",
                json!({ "release": release.version_text, "reason": reason }),
            );
            outcome.reason = Some(reason);
            outcome
        };
        let asset = asset_name(&release.version_text, self.arch);
        let fetch = |name: &str| -> Result<Vec<u8>, String> {
            let url = release
                .assets
                .get(name)
                .ok_or_else(|| format!("release {} has no {name}", release.tag))?;
            http.get(url, MAX_ASSET)
        };
        records.line(
            "download",
            json!({ "release": release.version_text, "asset": asset }),
        );
        let (sums, binary, installer) =
            match (fetch("SHA256SUMS"), fetch(&asset), fetch("install.sh")) {
                (Ok(sums), Ok(binary), Ok(installer)) => (
                    String::from_utf8_lossy(&sums).into_owned(),
                    binary,
                    installer,
                ),
                (Err(reason), _, _) | (_, Err(reason), _) | (_, _, Err(reason)) => {
                    return failed(outcome, reason);
                }
            };
        for (name, bytes) in [(asset.as_str(), &binary), ("install.sh", &installer)] {
            let Some(want) = checksum_of(&sums, name) else {
                return failed(
                    outcome,
                    format!("SHA256SUMS of {} has no checksum for {name}", release.tag),
                );
            };
            let got = sha256_hex(bytes);
            if got != want {
                return failed(
                    outcome,
                    format!(
                        "the checksum of {name} is wrong: SHA256SUMS says {want}, the file is {got}; every program was kept"
                    ),
                );
            }
        }
        let sha256 = sha256_hex(&binary);
        // The program is written beside the first program it replaces and asked its version there, so a binary that
        // does not start on this machine changes nothing.
        let mut changed: Vec<Changed> = Vec::new();
        let mut moved: Vec<Value> = Vec::new();
        for program in behind {
            match self.put_in_place(program, release, &binary, &sha256) {
                Ok(change) => {
                    records.line(
                        "install",
                        json!({ "program": program.path, "from": program.version, "to": release.version_text,
                                "how": match &change { Changed::InPlace { .. } => "in-place", Changed::Staged { .. } => "staged" } }),
                    );
                    changed.push(change);
                }
                Err(reason) => {
                    rollback(&changed);
                    return failed(outcome, reason);
                }
            }
        }
        // Shims first, then nodes, then links, then the rest; within a role by name.
        let mut restarts: Vec<(&Program, &Unit)> = behind
            .iter()
            .flat_map(|program| program.units.iter().map(move |unit| (*program, unit)))
            .filter(|(_, unit)| unit.active)
            .collect();
        restarts.sort_by(|a, b| (a.1.role(), &a.1.name).cmp(&(b.1.role(), &b.1.name)));
        let mut done: Vec<(&Program, &Unit)> = Vec::new();
        for (program, unit) in &restarts {
            done.push((program, unit));
            match self.restart_and_read(unit, &release.version_text) {
                Ok(version) => {
                    records.line("restart", json!({ "unit": unit.name, "version": version }));
                    moved.push(json!({ "unit": unit.name, "role": unit.role().as_str(), "from": program.version, "to": version }));
                }
                Err(reason) => {
                    let reason = format!(
                        "{} did not come back on {}: {reason}",
                        unit.name, release.version_text
                    );
                    records.line("restart", json!({ "unit": unit.name, "error": reason }));
                    rollback(&changed);
                    let mut readback = Vec::new();
                    for (program, unit) in &done {
                        let read = self.restart_and_read(unit, &program.version);
                        readback.push(json!({ "unit": unit.name, "version": read.as_ref().ok(), "error": read.err() }));
                    }
                    records.line(
                        "rolled-back",
                        json!({ "release": release.version_text, "reason": reason, "units": readback }),
                    );
                    outcome.result = "rolled-back";
                    outcome.reason = Some(reason);
                    outcome.detail["units"] = json!(readback);
                    return outcome;
                }
            }
        }
        // The templates of each node that has them, as the installer brings them up to an upgrade.
        let mut templates = Vec::new();
        for program in behind {
            for unit in program
                .units
                .iter()
                .filter(|unit| unit.role() == Role::Node)
            {
                let Some(config_path) = unit.config() else {
                    continue;
                };
                let Some(config) = node_config(unit) else {
                    continue;
                };
                if !config.state_dir.join("templates").exists() {
                    continue;
                }
                let runs = program_to_run(program, &config.state_dir);
                let mut command = Command::new(runs);
                command.args([
                    OsString::from("templates"),
                    OsString::from("install"),
                    OsString::from("--config"),
                    config_path.into_os_string(),
                ]);
                let said = match run_for(&mut command, Duration::from_secs(120)) {
                    Ok((true, out, _)) => {
                        json!({ "unit": unit.name, "said": serde_json::from_str::<Value>(out.trim()).unwrap_or(json!(out.trim())) })
                    }
                    Ok((false, out, err)) => {
                        json!({ "unit": unit.name, "error": format!("{}{}", out.trim(), err.trim()) })
                    }
                    Err(reason) => json!({ "unit": unit.name, "error": reason }),
                };
                records.line("templates", said.clone());
                templates.push(said);
            }
        }
        // The old programs are kept as `.previous`, so a person can still put one back by hand.
        records.line(
            "updated",
            json!({ "release": release.version_text, "units": moved }),
        );
        outcome.result = "updated";
        outcome.reason = None;
        outcome.detail["units"] = json!(moved);
        outcome.detail["templates"] = json!(templates);
        outcome
    }

    /// Puts the release's binary where `program` is: in place where its directory can be written, else staged in its
    /// node's state directory.
    fn put_in_place(
        &self,
        program: &Program,
        release: &Release,
        binary: &[u8],
        sha256: &str,
    ) -> Result<Changed, String> {
        let dir = program.path.parent().unwrap_or(Path::new("/"));
        let temp = dir.join(format!(
            ".arca-update-{}.{}",
            release.version_text,
            std::process::id()
        ));
        match write_program(&temp, binary) {
            Ok(()) => {
                if let Err(reason) = check_written(&temp, &release.version_text) {
                    let _ = fs::remove_file(&temp);
                    return Err(reason);
                }
                let previous = previous_of(&program.path);
                let old_sha = sha256_file(&program.path).ok();
                let _ = fs::remove_file(&previous);
                if let Err(e) = fs::hard_link(&program.path, &previous)
                    .or_else(|_| fs::copy(&program.path, &previous).map(|_| ()))
                {
                    let _ = fs::remove_file(&temp);
                    return Err(format!(
                        "the old program {} could not be kept: {e}",
                        program.path.display()
                    ));
                }
                if let Err(e) = fs::rename(&temp, &program.path) {
                    let _ = fs::remove_file(&temp);
                    return Err(format!(
                        "{} could not be replaced: {e}",
                        program.path.display()
                    ));
                }
                if let (Some(note), Some(old_sha)) = (&self.install_note, old_sha) {
                    renew_install_note(note, &program.named, &old_sha, sha256);
                }
                Ok(Changed::InPlace {
                    path: program.path.clone(),
                    previous,
                })
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ReadOnlyFilesystem | io::ErrorKind::PermissionDenied
                ) =>
            {
                let Some(state_dir) = &program.state_dir else {
                    return Err(format!(
                        "{} cannot be written ({error}) and no node that runs it has a state directory to stage it in",
                        program.path.display()
                    ));
                };
                stage(state_dir, release, binary, sha256)
            }
            Err(error) => Err(format!(
                "the new program could not be written beside {}: {error}",
                program.path.display()
            )),
        }
    }

    /// Restarts one unit and waits for it to come back on `version` and stay up.
    fn restart_and_read(&self, unit: &Unit, version: &str) -> Result<String, String> {
        self.units.restart(&unit.name)?;
        let deadline = Instant::now() + self.health;
        let mut seen = None;
        while Instant::now() < deadline {
            if let Ok(running) = self.units.running(&unit.name)
                && running.active
                && running.pid > 0
            {
                seen = Some(running);
                break;
            }
            std::thread::sleep(self.poll);
        }
        let Some(first) = seen else {
            return Err(format!(
                "it was not running within {} s",
                self.health.as_secs()
            ));
        };
        std::thread::sleep(self.settle);
        let now = self.units.running(&unit.name)?;
        if !now.active || now.pid != first.pid || now.restarts != first.restarts {
            return Err("it stopped or restarted after it started".to_string());
        }
        let read = self
            .units
            .running_version(&unit.name, &now)
            .ok_or_else(|| "its process did not say its version".to_string())?;
        if read != version {
            return Err(format!("it runs {read}"));
        }
        Ok(read)
    }
}

/// Whether version `a` is newer than version `b`. A text that is not a version is never newer, and nothing is newer
/// than it either, so an unreadable version changes nothing.
fn newer(a: &str, b: &str) -> bool {
    match (Version::parse(a), Version::parse(b)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

/// The config of a node unit, when it can be read: the one it names, else the installer's for this user.
fn node_config(unit: &Unit) -> Option<NodeConfig> {
    let path = unit.config().or_else(|| {
        std::env::var_os("HOME").map(|home| Path::new(&home).join(".config/arca/node.json"))
    })?;
    crate::config::load(&path).ok()
}

/// The program that runs for `program`: the staged one, when that is what its node runs.
fn program_to_run(program: &Program, state_dir: &Path) -> PathBuf {
    match staged_program(state_dir) {
        Some(staged)
            if newer(
                &staged.version,
                &program_version(&program.path).unwrap_or_default(),
            ) =>
        {
            staged.path
        }
        _ => program.path.clone(),
    }
}

fn previous_of(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(OsString::from).unwrap_or_default();
    name.push(".previous");
    path.with_file_name(name)
}

fn write_program(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o755)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
}

/// The written program must say the release's version on this machine.
fn check_written(path: &Path, version: &str) -> Result<(), String> {
    match program_version(path) {
        Some(said) if said == version => Ok(()),
        Some(said) => Err(format!(
            "the downloaded program says it is {said}, not {version}; every program was kept"
        )),
        None => Err(
            "the downloaded program does not start on this machine; every program was kept"
                .to_string(),
        ),
    }
}

/// Stages the release in the node's state directory, for its `serve` to hand over to at its start.
fn stage(
    state_dir: &Path,
    release: &Release,
    binary: &[u8],
    sha256: &str,
) -> Result<Changed, String> {
    let dir = state_dir.join(UPDATE_DIR);
    fs::create_dir_all(&dir).map_err(|e| format!("{} cannot be made: {e}", dir.display()))?;
    let name = format!("arca-{}", release.version_text);
    let path = dir.join(&name);
    let temp = dir.join(format!(".{name}.{}", std::process::id()));
    let _ = fs::remove_file(&temp);
    write_program(&temp, binary)
        .map_err(|e| format!("{} cannot be written: {e}", temp.display()))?;
    if let Err(reason) = check_written(&temp, &release.version_text) {
        let _ = fs::remove_file(&temp);
        return Err(reason);
    }
    fs::rename(&temp, &path).map_err(|e| format!("{} cannot be written: {e}", path.display()))?;
    let current = dir.join(STAGED_FILE);
    let previous = dir.join(STAGED_PREVIOUS_FILE);
    let had_previous = current.exists();
    if had_previous {
        fs::rename(&current, &previous)
            .map_err(|e| format!("{} cannot be kept: {e}", current.display()))?;
    }
    let manifest = json!({ "file": name, "version": release.version_text, "sha256": sha256 });
    write_private(&current, &format!("{manifest}\n"))
        .map_err(|e| format!("{} cannot be written: {e}", current.display()))?;
    // Staged programs that are neither the current one nor the one before it are no longer needed.
    let keep: BTreeSet<PathBuf> = [
        Some(path.clone()),
        staged_manifest(&dir, STAGED_PREVIOUS_FILE).map(|s| s.path),
    ]
    .into_iter()
    .flatten()
    .collect();
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let staged_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("arca-"));
            if staged_name && !keep.contains(&path) {
                let _ = fs::remove_file(path);
            }
        }
    }
    Ok(Changed::Staged { dir, had_previous })
}

/// Puts every changed program back as it was.
fn rollback(changed: &[Changed]) {
    for change in changed.iter().rev() {
        match change {
            Changed::InPlace { path, previous } => {
                // The previous program is linked back first, so the path never names nothing.
                let temp = previous.with_extension("rollback");
                let _ = fs::remove_file(&temp);
                let restored = fs::hard_link(previous, &temp)
                    .or_else(|_| fs::copy(previous, &temp).map(|_| ()))
                    .and_then(|()| fs::rename(&temp, path));
                if let Err(error) = restored {
                    eprintln!("arca: {} could not be put back: {error}", path.display());
                }
            }
            Changed::Staged { dir, had_previous } => {
                let current = dir.join(STAGED_FILE);
                let result = if *had_previous {
                    fs::rename(dir.join(STAGED_PREVIOUS_FILE), &current)
                } else {
                    fs::remove_file(&current)
                };
                if let Err(error) = result {
                    eprintln!(
                        "arca: the staged program in {} could not be put back: {error}",
                        dir.display()
                    );
                }
            }
        }
    }
}

/// The installer's note names the program it installed by its checksum, so `--uninstall` removes only that file. A
/// note that names a program this run replaced, with the old checksum, names the new one.
fn renew_install_note(note: &Path, named: &BTreeSet<PathBuf>, old: &str, new: &str) {
    let Ok(text) = fs::read_to_string(note) else {
        return;
    };
    let Some((sum, path)) = text.trim_end().split_once("  ") else {
        return;
    };
    if sum == old && named.contains(Path::new(path)) {
        let _ = write_private(note, &format!("{new}  {path}\n"));
    }
}

// -------------------------------------------------------------------------------------------------------------------
// The schedule in `serve`, the status and the alert
// -------------------------------------------------------------------------------------------------------------------

/// The update part of `arca_status`.
pub fn status_json(update: &UpdateConfig, state_dir: &Path) -> Value {
    let state = read_state(state_dir).unwrap_or(Value::Null);
    let text = |key: &str| {
        state
            .get(key)
            .cloned()
            .filter(|v| !v.is_null())
            .unwrap_or(Value::Null)
    };
    json!({
        "automatic": update.automatic,
        "channel": update.channel.as_str(),
        "window": update.window,
        "current": env!("CARGO_PKG_VERSION"),
        "available": text("available"),
        "lastCheckAt": text("checkedAtMs"),
        "lastResult": text("result"),
        "lastReason": text("reason"),
        "lastUpdateAt": text("updatedAtMs"),
        "failed": state.get("failed").and_then(Value::as_bool).unwrap_or(false),
    })
}

/// The alert input: whether the updater's last run on this node failed. A node it never ran on has not failed.
pub fn alert_input(
    state_dir: &Path,
    now_ms: u64,
) -> arca_alerts::eval::Timed<arca_alerts::eval::KernelUpdateUse> {
    let failed = read_state(state_dir)
        .and_then(|state| state.get("failed").and_then(Value::as_bool))
        .unwrap_or(false);
    arca_alerts::eval::Timed {
        at: now_ms,
        value: arca_alerts::eval::KernelUpdateUse { failed },
    }
}

/// The count of cards waiting for the owner on this node, as the schedule reads it each minute.
pub type Waiting = Arc<dyn Fn() -> Option<usize> + Send + Sync>;

/// The schedule of one `serve`: each minute it writes how many owner cards wait on the node, and when a look is due it
/// starts `arca update run` outside the node's own unit. It runs only for a node that systemd runs as a service: one
/// started by hand has no unit to restart.
pub fn spawn_schedule(
    config_path: PathBuf,
    config: NodeConfig,
    waiting: Waiting,
) -> io::Result<()> {
    let under_systemd = std::env::var_os("INVOCATION_ID").is_some();
    std::thread::Builder::new()
        .name("arca-update".to_string())
        .spawn(move || {
            let started = Instant::now();
            let mut launched: Option<u64> = None;
            loop {
                if let Some(count) = waiting() {
                    write_waiting(&config.state_dir, count);
                }
                if config.update.automatic
                    && under_systemd
                    && started.elapsed() >= FIRST_LOOK_AFTER
                    && due(
                        &config.update,
                        read_state(&config.state_dir).as_ref(),
                        launched,
                        now_ms(),
                    )
                {
                    launched = Some(now_ms());
                    if let Err(reason) = launch(&config_path) {
                        eprintln!("arca: the kernel update could not be started: {reason}");
                    }
                }
                std::thread::sleep(Duration::from_secs(60));
            }
        })
        .map(|_| ())
}

/// Whether a look is due: the last one is older than the interval, or than the retry time when the last one was put off
/// or got no answer; and this node has not started one within the retry time itself.
pub fn due(update: &UpdateConfig, state: Option<&Value>, launched: Option<u64>, now: u64) -> bool {
    if launched.is_some_and(|at| now.saturating_sub(at) < RETRY_EVERY_MS) {
        return false;
    }
    let Some(state) = state else { return true };
    let last = state["checkedAtMs"].as_u64().unwrap_or(0);
    let every = match state["result"].as_str() {
        Some("deferred" | "waiting-for-window" | "check-failed" | "busy") => {
            RETRY_EVERY_MS.min(update.check_every_seconds * 1000)
        }
        _ => update.check_every_seconds * 1000,
    };
    now.saturating_sub(last) >= every
}

/// The program this process runs, by the path it was started from.
fn own_program() -> Result<PathBuf, String> {
    let link = fs::read_link("/proc/self/exe").map_err(|e| e.to_string())?;
    let text = link.to_string_lossy();
    Ok(PathBuf::from(
        text.strip_suffix(" (deleted)").unwrap_or(&text),
    ))
}

/// Starts one `arca update run` as a transient unit of this process's own manager, so the restarts it makes do not end it.
fn launch(config_path: &Path) -> Result<(), String> {
    let program = own_program()?;
    let manager = Systemd::of_this_process();
    let mut command = Command::new("systemd-run");
    if manager.user {
        command.arg("--user");
    }
    command
        .args(["--collect", "--quiet", "--no-block"])
        .arg(format!("--unit=arca-update-{}", now_ms()))
        .arg("--description=Arca kernel update")
        .arg("--")
        .arg(program)
        .args(["update", "run", "--config"])
        .arg(config_path);
    let (ok, _, err) = run_for(&mut command, Duration::from_secs(60))?;
    if ok {
        Ok(())
    } else {
        Err(err
            .lines()
            .next()
            .unwrap_or("systemd-run failed")
            .to_string())
    }
}

// -------------------------------------------------------------------------------------------------------------------
// The command
// -------------------------------------------------------------------------------------------------------------------

/// The machine's update lock: in the manager's runtime directory, so one run per manager at a time.
fn lock_path(state_dir: &Path) -> PathBuf {
    if rustix::process::geteuid().is_root() {
        return PathBuf::from("/run").join(LOCK_FILE);
    }
    match std::env::var_os("XDG_RUNTIME_DIR").filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir).join(LOCK_FILE),
        None => state_dir.join(UPDATE_DIR).join(LOCK_FILE),
    }
}

fn minute_now() -> u32 {
    u32::try_from((now_ms() / 60_000) % 1440).unwrap_or(0)
}

/// `arca update check` (`install` false) and `arca update run` (`install` true).
pub fn command(flags: &Flags, install: bool) -> Result<Option<Value>, Failure> {
    let does = if install {
        crate::Does::Acts
    } else {
        crate::Does::Reads
    };
    let (_, config) = crate::load_config_to(flags, does)?;
    let manager = Systemd::of_this_process();
    let install_note = std::env::var_os("HOME")
        .map(|home| Path::new(&home).join(".local/state/arca-install/program.sha256"));
    let run = Run {
        config: &config,
        units: &manager,
        arch: std::env::consts::ARCH,
        health: Duration::from_secs(config.update.health_seconds),
        settle: Duration::from_secs(5),
        poll: Duration::from_millis(500),
        install_note,
        lock: lock_path(&config.state_dir),
        minute_of_day: minute_now(),
    };
    let outcome = run.run(install);
    let mut answer = outcome.json();
    answer["current"] = json!(env!("CARGO_PKG_VERSION"));
    if outcome.failed() {
        return Err(Failure::Runtime(format!(
            "the kernel update {}: {}",
            outcome.result,
            outcome.reason.unwrap_or_default()
        )));
    }
    Ok(Some(answer))
}

#[cfg(test)]
mod tests;
