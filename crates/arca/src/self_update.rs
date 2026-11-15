//! The kernel updater: every Arca program on this machine follows Arca's releases, with nobody at a terminal.
//!
//! `arca serve` looks on a schedule (the node config's `update` key: on by default, every six hours, releases only, at
//! any hour). When a look is due it starts `arca update run` as a transient systemd unit of its own manager, so the run
//! outlives the restarts it makes. One run:
//!
//! 1. takes the machine's update lock, so two runs of one manager never overlap, and first finishes or rolls back an
//!    update an earlier run left unconfirmed (its `pending.json`);
//! 2. reads the releases of `automagik-si/arca` (`update.source`) and picks the newest of the channel; a release this
//!    machine rolled back is held back until a newer one is published;
//! 3. finds every systemd unit of its manager whose `ExecStart` runs an Arca program: the installer's `arca.service`,
//!    the guide's hand-made units, a link, a second node with its own program path. A program is a file that answers
//!    `--version` with `Automagik Arca <version>`; units are grouped by the file their path resolves to;
//! 4. keeps each program that is as new as the release, and each that is not byte for byte the published binary of the
//!    version it says (a build from a branch); never puts an older program over a newer one. A program on a read-only file system is
//!    the OS's (Arca OS's image), and the OS update brings it: it is named `os-managed` and left alone;
//! 5. waits outside `update.window`, and while any node it would restart has an owner approval card waiting;
//! 6. downloads the binary for this machine's architecture, the release's `SHA256SUMS`, its signature and `install.sh`;
//!    the signature must be one of [`RELEASE_KEYS`] over `SHA256SUMS`, both files must match `SHA256SUMS`, and the
//!    binary must say the release's version; anything else keeps every old program;
//! 7. writes `pending.json`, then puts the binary in place as the installer does: written beside the program and renamed
//!    over it in one step, with the old one kept as `<program>.previous`. A program in a directory this user may not
//!    write, whose units all hand over ([`hand_over`]), is staged under the user's own state instead;
//! 8. restarts the units of the programs it changed one at a time, shims first, then nodes, then links, and watches
//!    each for `update.healthSeconds`: the same process, never restarted, a node answering on its own socket, and the
//!    release's version read from the process itself. Anything else rolls every program back, restarts the units
//!    again, and the `kernel-update-failed` alert fires;
//! 9. runs `arca templates install` for each node that has templates, as the installer does on an upgrade.
//!
//! Every look and every step is journalled in `<stateDir>/kernel-update/journal.jsonl` of each node the run found, and
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

/// The Ed25519 public keys a release's `SHA256SUMS.sig` must verify against, 32 bytes each; more than one while a key
/// is rotated.
///
/// PLACEHOLDER: NO RELEASE SIGNING KEY EXISTS YET. The list is empty on purpose, and while it is, the updater looks and
/// says what is available but installs nothing (`unsigned`). Making the key is the owner's call. Once the key pair is
/// made and its private half is the publish job's `ARCA_RELEASE_SIGNING_KEY` secret (`.github/workflows/release.yml`),
/// its public half goes here: `openssl pkey -in key.pem -pubout -outform DER | tail -c 32 | od -An -tu1` gives the
/// 32 bytes.
pub const RELEASE_KEYS: &[[u8; 32]] = &[];

/// The updater's directory in a node's state directory.
pub const UPDATE_DIR: &str = "kernel-update";
/// The last result, in [`UPDATE_DIR`].
pub const STATE_FILE: &str = "state.json";
/// Every look and step, one JSON line each, in [`UPDATE_DIR`].
pub const JOURNAL_FILE: &str = "journal.jsonl";
/// How many owner approval cards wait on the node, written by its `serve` each minute, in [`UPDATE_DIR`].
pub const WAITING_FILE: &str = "waiting.json";
/// An update whose programs are in place and whose units were not all confirmed yet, in [`UPDATE_DIR`].
pub const PENDING_FILE: &str = "pending.json";
/// In a program's staging directory: the staged program its units hand over to.
pub const STAGED_FILE: &str = "current.json";
/// In a program's staging directory: the staged program before the last one, for a rollback.
pub const STAGED_PREVIOUS_FILE: &str = "previous.json";
/// Set in the environment of a process that a hand-over started, so it never hands over again.
pub const HANDED_OVER: &str = "ARCA_HANDED_OVER";
/// The lock one run holds, in the manager's runtime directory.
const LOCK_FILE: &str = "arca-update.lock";
/// The lock's place in the staging root when the manager has no runtime directory. No staging step removes it.
const STAGE_LOCK_FILE: &str = "update.lock";
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
/// How long a node has to answer one request on its socket.
const ANSWER_WAIT: Duration = Duration::from_secs(5);
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
            Err(e) => {
                return Err(format!("could not start {:?}: {e}", command.get_program()));
            }
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
// Releases and their signature
// -------------------------------------------------------------------------------------------------------------------

/// One release: its version and its assets by name.
#[derive(Debug, Clone)]
pub struct Release {
    pub version: Version,
    pub version_text: String,
    pub tag: String,
    pub assets: BTreeMap<String, String>,
}

/// One release of a GitHub releases answer, when it is a `v<semantic version>` tag and not a draft.
fn release_of(release: &Value) -> Option<Release> {
    if release["draft"].as_bool().unwrap_or(false) {
        return None;
    }
    let tag = release["tag_name"].as_str()?;
    let text = tag.strip_prefix('v')?;
    let version = Version::parse(text)?;
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
    Some(Release {
        version,
        version_text: text.to_string(),
        tag: tag.to_string(),
        assets,
    })
}

/// The newest release of `channel` in a GitHub releases listing. A draft is never one; a pre-release only in the
/// `prerelease` channel; a tag that is not `v<semantic version>` is skipped.
pub fn newest_release(listing: &str, channel: UpdateChannel) -> Result<Option<Release>, String> {
    let releases: Vec<Value> = serde_json::from_str(listing)
        .map_err(|_| "the releases listing is not a JSON list".to_string())?;
    let mut newest: Option<Release> = None;
    for release in &releases {
        if release["prerelease"].as_bool().unwrap_or(false) && channel == UpdateChannel::Stable {
            continue;
        }
        let Some(release) = release_of(release) else {
            continue;
        };
        if newest
            .as_ref()
            .is_none_or(|newest| release.version > newest.version)
        {
            newest = Some(release);
        }
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

/// Whether `signature` (64 bytes, as `openssl pkeyutl -sign` writes it) is one of `keys`' Ed25519 signatures of `sums`.
pub fn signed_by(keys: &[[u8; 32]], sums: &[u8], signature: &[u8]) -> bool {
    signature.len() == 64
        && keys.iter().any(|key| {
            ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key)
                .verify(sums, signature)
                .is_ok()
        })
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256_of(mut reader: impl io::Read) -> io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 16];
    loop {
        let read = reader.read(&mut buffer)?;
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

fn sha256_file(path: &Path) -> io::Result<String> {
    sha256_of(File::open(path)?)
}

/// Why a fetch did not give a body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetch {
    /// The address answered 404.
    NotFound,
    /// Anything else, in words that hold no credential.
    Failed(String),
}

impl Fetch {
    fn words(self) -> String {
        match self {
            Self::NotFound => "not found".to_string(),
            Self::Failed(words) => words,
        }
    }
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

    /// The body at `url`, at most `max` bytes.
    pub fn get(&self, url: &str, max: usize) -> Result<Vec<u8>, Fetch> {
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
                .map_err(|e| Fetch::Failed(format!("{url} did not answer: {}", without_url(&e))))?;
            if response.status().as_u16() == 404 {
                return Err(Fetch::NotFound);
            }
            if !response.status().is_success() {
                return Err(Fetch::Failed(format!(
                    "{url} answered {}",
                    response.status().as_u16()
                )));
            }
            let mut body = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|e| Fetch::Failed(format!("{url} was cut short: {}", without_url(&e))))?
            {
                if body.len() + chunk.len() > max {
                    return Err(Fetch::Failed(format!("{url} is larger than {max} bytes")));
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

/// Why a release's checksums cannot be trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Sums {
    /// The release has no `SHA256SUMS.sig`, or this program holds no key to check one with.
    Unsigned(String),
    /// The signature does not verify, or a file is missing or cannot be read.
    Bad(String),
}

/// The release's `SHA256SUMS`, once its signature verified against `keys`.
fn signed_sums(http: &Http, release: &Release, keys: &[[u8; 32]]) -> Result<String, Sums> {
    if keys.is_empty() {
        return Err(Sums::Unsigned(
            "this program holds no release signing key, so it installs no release by itself"
                .to_string(),
        ));
    }
    let fetch = |name: &str| -> Result<Vec<u8>, Sums> {
        let url = release.assets.get(name).ok_or_else(|| {
            if name == "SHA256SUMS.sig" {
                Sums::Unsigned(format!(
                    "release {} is not signed (no SHA256SUMS.sig)",
                    release.tag
                ))
            } else {
                Sums::Bad(format!("release {} has no {name}", release.tag))
            }
        })?;
        http.get(url, MAX_LISTING).map_err(|e| Sums::Bad(e.words()))
    };
    let sums = fetch("SHA256SUMS")?;
    let signature = fetch("SHA256SUMS.sig")?;
    if !signed_by(keys, &sums, &signature) {
        return Err(Sums::Bad(format!(
            "the signature of release {}'s SHA256SUMS does not verify against this program's release keys; every program was kept",
            release.tag
        )));
    }
    Ok(String::from_utf8_lossy(&sums).into_owned())
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

    /// Whether a unit of this role hands over to a staged program at its start ([`hand_over`]).
    pub fn hands_over(self) -> bool {
        self != Role::Other
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
    /// Whether a node unit answers on its own socket now.
    fn answers(&self, unit: &Unit) -> bool;
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

/// Whether something answers HTTP on the unix socket `socket`: a tokenless `POST /mcp` gets a status line (401 from a
/// node) within [`ANSWER_WAIT`].
pub fn socket_answers(socket: &Path) -> bool {
    use std::os::unix::net::UnixStream;
    let Ok(mut stream) = UnixStream::connect(socket) else {
        return false;
    };
    let _ = stream.set_write_timeout(Some(ANSWER_WAIT));
    let _ = stream.set_read_timeout(Some(ANSWER_WAIT));
    let request =
        "POST /mcp HTTP/1.1\r\nHost: arca\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut head = [0u8; 12];
    let mut read = 0;
    while read < head.len() {
        match stream.read(&mut head[read..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => read += n,
        }
    }
    head[..read].starts_with(b"HTTP/1.")
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

    fn answers(&self, unit: &Unit) -> bool {
        node_config(unit).is_some_and(|config| socket_answers(&config.listen.unix))
    }
}

// -------------------------------------------------------------------------------------------------------------------
// Where a program goes: in place, staged, or left to the OS
// -------------------------------------------------------------------------------------------------------------------

/// What a write beside a program found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirWrite {
    /// The directory can be written.
    Writable,
    /// The file system is read-only: the program belongs to the OS image.
    ReadOnly,
    /// This user may not write the directory.
    Denied,
    /// Anything else, in words.
    Failed(String),
}

/// Where the release goes for one program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    /// Renamed over the program.
    InPlace,
    /// Staged in the user's staging root, for its units to hand over to.
    Staged,
    /// The OS brings this program; the updater leaves it alone. The words say why.
    OsManaged(String),
    /// It cannot be updated here; the words name why (the units that would not hand over).
    Refused(String),
}

/// Whether the directory `dir` can be written, found by making and removing one empty file there.
pub fn probe_write(dir: &Path) -> DirWrite {
    if mount_flags(dir).is_some_and(|flags| flags.contains(rustix::fs::StatVfsMountFlags::RDONLY)) {
        return DirWrite::ReadOnly;
    }
    let probe = dir.join(format!(".arca-update-probe.{}", std::process::id()));
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&probe)
    {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            DirWrite::Writable
        }
        Err(e) if e.kind() == io::ErrorKind::ReadOnlyFilesystem => DirWrite::ReadOnly,
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => DirWrite::Denied,
        Err(e) => DirWrite::Failed(format!("{} cannot be written: {e}", dir.display())),
    }
}

/// The mount flags of the file system `path` is on, or of its nearest ancestor that exists.
fn mount_flags(path: &Path) -> Option<rustix::fs::StatVfsMountFlags> {
    let mut path = path;
    loop {
        if let Ok(stat) = rustix::fs::statvfs(path) {
            return Some(stat.f_flag);
        }
        path = path.parent()?;
    }
}

/// Whether programs staged under `root` may run: its file system is neither read-only nor mounted `noexec`.
pub fn stage_runs(root: &Path) -> bool {
    use rustix::fs::StatVfsMountFlags as F;
    mount_flags(root).is_some_and(|flags| !flags.intersects(F::NOEXEC | F::RDONLY))
}

/// Where the release goes for a program in a directory that `write` found, whose staging root may run programs or
/// not, run by `units`.
pub fn placement(write: &DirWrite, stage_runs: bool, units: &[Unit]) -> Placement {
    match write {
        DirWrite::Writable => Placement::InPlace,
        DirWrite::ReadOnly => Placement::OsManaged(
            "it is on a read-only file system: the OS image brings it, through the OS update".to_string(),
        ),
        DirWrite::Denied if !stage_runs => Placement::OsManaged(
            "this user may not write it, and the place a release would be staged in may not run programs (noexec)"
                .to_string(),
        ),
        DirWrite::Denied => {
            let stuck: Vec<&str> = units
                .iter()
                .filter(|unit| unit.active && !unit.role().hands_over())
                .map(|unit| unit.name.as_str())
                .collect();
            if stuck.is_empty() {
                Placement::Staged
            } else {
                Placement::Refused(format!(
                    "this user may not write it, and {} would not run a staged release",
                    stuck.join(", ")
                ))
            }
        }
        DirWrite::Failed(words) => Placement::Refused(words.clone()),
    }
}

/// The user's staging root: `/var/lib/arca-update` for root, else `~/.local/state/arca-update`.
pub fn stage_root() -> Option<PathBuf> {
    if rustix::process::geteuid().is_root() {
        return Some(PathBuf::from("/var/lib/arca-update"));
    }
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| Path::new(&home).join(".local/state/arca-update"))
}

/// A program's own staging directory under `root`: named by the SHA-256 of the program's path, so two programs of one
/// user never share one.
pub fn staging_dir(root: &Path, program: &Path) -> PathBuf {
    let digest = sha256_hex(program.as_os_str().as_encoded_bytes());
    root.join(&digest[..16])
}

/// A staged program as its manifest names it.
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

/// Whether a staged file may run: a regular file (never a link) of this user's that nobody else may write.
fn staged_file_ok(meta: &fs::Metadata) -> bool {
    meta.file_type().is_file()
        && meta.uid() == rustix::process::geteuid().as_raw()
        && meta.mode() & 0o022 == 0
}

/// The program staged in the staging directory `dir`, when there is one this process may run and whose content is
/// the manifest's.
pub fn staged_program(dir: &Path) -> Option<Staged> {
    let staged = staged_manifest(dir, STAGED_FILE)?;
    let meta = fs::symlink_metadata(&staged.path).ok()?;
    if !staged_file_ok(&meta) {
        return None;
    }
    (sha256_file(&staged.path).ok()? == staged.sha256).then_some(staged)
}

/// Whether a program of version `own` hands over to a staged program of version `staged`: only to a newer one, so an
/// OS update or an install that brought a newer program wins.
pub fn should_hand_over(own: &str, staged: &str) -> bool {
    newer(staged, own)
}

/// At the start of `serve`, `shim` and `link serve`: when this program has a staged release newer than itself, the
/// process becomes that release, with the same arguments. The staged file is opened once, without following a link,
/// checked and hashed through that one descriptor, and that very file is run (`/proc/self/fd/<n>`). Nothing here stops
/// the start: a hand-over that cannot happen is said, and this program goes on.
pub fn hand_over() {
    use std::os::unix::io::AsRawFd as _;
    use std::os::unix::process::CommandExt as _;
    if std::env::var_os(HANDED_OVER).is_some() {
        return;
    }
    let (Some(root), Ok(own)) = (stage_root(), std::env::current_exe()) else {
        return;
    };
    let own = fs::canonicalize(&own).unwrap_or(own);
    let dir = staging_dir(&root, &own);
    let Some(staged) = staged_manifest(&dir, STAGED_FILE) else {
        return;
    };
    if !should_hand_over(env!("CARGO_PKG_VERSION"), &staged.version) {
        return;
    }
    let refuse = |why: &str| {
        eprintln!(
            "arca: the staged release {} at {} is not used: {why}; this program goes on",
            staged.version,
            staged.path.display()
        );
    };
    let Ok(file) = OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(&staged.path)
    else {
        return refuse("it cannot be opened");
    };
    if !file.metadata().is_ok_and(|meta| staged_file_ok(&meta)) {
        return refuse("it is not a file of this user's that only this user may write");
    }
    if sha256_of(&file).ok().as_deref() != Some(staged.sha256.as_str()) {
        return refuse("its content is not the manifest's");
    }
    let mut args = std::env::args_os();
    let arg0 = args.next().unwrap_or_else(|| OsString::from("arca"));
    eprintln!(
        "arca: this program is {}; handing over to the kernel update {} staged at {}",
        env!("CARGO_PKG_VERSION"),
        staged.version,
        staged.path.display()
    );
    let error = Command::new(format!("/proc/self/fd/{}", file.as_raw_fd()))
        .arg0(arg0)
        .args(args)
        .env(HANDED_OVER, "1")
        .exec();
    refuse(&format!("it did not start ({error})"));
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

/// An update left unconfirmed on the node with this state directory.
pub fn read_pending(state_dir: &Path) -> Option<Value> {
    let text = fs::read_to_string(state_dir.join(UPDATE_DIR).join(PENDING_FILE)).ok()?;
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
    fn state(&self, fields: &Value) {
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

    /// Writes the pending update into every node's record. It is written before the first program changes.
    fn pending(&self, pending: &Value) -> Result<(), String> {
        for dir in &self.dirs {
            let path = dir.join(UPDATE_DIR).join(PENDING_FILE);
            write_private(&path, &format!("{pending}\n")).map_err(|e| {
                format!(
                    "the pending record {} cannot be written: {e}",
                    path.display()
                )
            })?;
        }
        Ok(())
    }

    fn clear_pending(&self) {
        for dir in &self.dirs {
            let _ = fs::remove_file(dir.join(UPDATE_DIR).join(PENDING_FILE));
        }
    }
}

/// The count of waiting cards a node's `serve` wrote, when it is fresh. A stale or missing count is no card: its node
/// is not running, or cannot say.
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
    /// The version the program's units run: the file's, or a staged release's when that is newer.
    version: String,
    /// The SHA-256 of what the units run.
    sha256: Option<String>,
    units: Vec<Unit>,
    /// The state directories of the nodes that run it.
    state_dirs: BTreeSet<PathBuf>,
}

/// How one program was changed, so it can be put back.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Changed {
    /// Renamed over in place; the old file is at `previous`.
    InPlace { path: PathBuf, previous: PathBuf },
    /// Staged in the program's staging directory; the earlier manifest, if any, is kept as `previous.json`.
    Staged { dir: PathBuf, had_previous: bool },
}

impl Changed {
    fn json(&self) -> Value {
        match self {
            Self::InPlace { path, previous } => {
                json!({ "how": "in-place", "path": path, "previous": previous })
            }
            Self::Staged { dir, had_previous } => {
                json!({ "how": "staged", "dir": dir, "hadPrevious": had_previous })
            }
        }
    }

    fn of(value: &Value) -> Option<Self> {
        match value["how"].as_str()? {
            "in-place" => Some(Self::InPlace {
                path: PathBuf::from(value["path"].as_str()?),
                previous: PathBuf::from(value["previous"].as_str()?),
            }),
            "staged" => Some(Self::Staged {
                dir: PathBuf::from(value["dir"].as_str()?),
                had_previous: value["hadPrevious"].as_bool()?,
            }),
            _ => None,
        }
    }
}

/// One unit a run restarts, and the version it ran before.
#[derive(Debug, Clone)]
struct Restart {
    unit: Unit,
    from: String,
}

/// What one run needs from outside: the service manager, the release keys, the clock's pace, and its places.
pub struct Run<'a> {
    pub config: &'a NodeConfig,
    pub units: &'a dyn Units,
    /// This machine's architecture, as the release names it (`x86_64`, `aarch64`).
    pub arch: &'a str,
    /// The keys a release's `SHA256SUMS.sig` must verify against: [`RELEASE_KEYS`].
    pub keys: &'a [[u8; 32]],
    /// How long a restarted unit has to come back, and how long it must then stay up.
    pub health: Duration,
    pub poll: Duration,
    /// The installer's own note of the program it installed (`~/.local/state/arca-install/program.sha256`).
    pub install_note: Option<PathBuf>,
    /// The machine's update lock.
    pub lock: PathBuf,
    /// Where releases are staged for programs this user may not write: [`stage_root`].
    pub stage_root: Option<PathBuf>,
    /// The time of day, minutes in UTC, for the window.
    pub minute_of_day: u32,
}

/// What one run ended with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// `up-to-date`, `updated`, `deferred`, `waiting-for-window`, `check-failed`, `failed`, `rolled-back`, `held-back`,
    /// `os-managed`, `branch-build`, `unsigned`, `available` (a look of `arca update check`) or `busy`.
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

    /// Whether the run failed in a way the alert names: a release that did not verify, one rolled back, or one held
    /// back because it was rolled back.
    pub fn failed(&self) -> bool {
        matches!(self.result, "failed" | "rolled-back" | "held-back")
    }

    /// Whether the run settles the alert: a failure sets it, and an update, an up-to-date look or a program the
    /// updater leaves alone clears it. A put-off look leaves it as it was.
    fn settles(&self) -> bool {
        self.failed()
            || matches!(
                self.result,
                "updated" | "up-to-date" | "os-managed" | "branch-build" | "unsigned"
            )
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
    /// One look, and an update when one is due. Everything is journalled; only the lock held by another run is not, and
    /// `arca update check` (`install` false) writes nothing.
    pub fn run(&self, install: bool) -> Outcome {
        let lock = match self.take_lock() {
            Ok(lock) => lock,
            Err(reason) => return Outcome::new("busy", Some(reason), None),
        };
        let mut records = Records {
            dirs: BTreeSet::from([self.config.state_dir.clone()]),
        };
        let pending = install
            .then(|| read_pending(&self.config.state_dir))
            .flatten();
        let outcome = match pending {
            Some(pending) => self.settle_pending(&pending, &mut records),
            None => self.look(install, &mut records),
        };
        if install {
            let now = now_ms();
            let mut fields = json!({
                "checkedAtMs": now,
                "result": outcome.result,
                "reason": outcome.reason,
            });
            if let Some(available) = &outcome.available {
                fields["available"] = json!(available);
            }
            if outcome.settles() {
                fields["failed"] = json!(outcome.failed());
            }
            match outcome.result {
                "updated" => {
                    fields["updatedAtMs"] = json!(now);
                    fields["updated"] = outcome.detail["units"].clone();
                    fields["rolledBack"] = Value::Null;
                }
                "rolled-back" => fields["rolledBack"] = json!(outcome.available),
                _ => {}
            }
            records.state(&fields);
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

    /// The staging directory of a program, when this user has a staging root.
    fn staging(&self, program: &Path) -> Option<PathBuf> {
        self.stage_root
            .as_deref()
            .map(|root| staging_dir(root, program))
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
                sha256: sha256_file(&file).ok(),
                units: Vec::new(),
                state_dirs: BTreeSet::new(),
            });
            program.named.insert(unit.program.clone());
            program.state_dirs.extend(state_dir);
            program.units.push(unit);
        }
        // A release staged for a program is what its units run when it is newer than the file.
        for program in by_file.values_mut() {
            if let Some(staged) = self
                .staging(&program.path)
                .and_then(|dir| staged_program(&dir))
                && newer(&staged.version, &program.version)
            {
                program.version = staged.version;
                program.sha256 = Some(staged.sha256);
            }
        }
        by_file.into_values().collect()
    }

    /// Whether `program` is, byte for byte, the published binary of its own version: a release this updater may replace.
    /// A build from a branch says a release's version and is another file; a version that was never published is one
    /// too. The checksums only name what a program is, so their signature is not needed here: a false listing could at
    /// most keep a program, or have it replaced by a release whose own signature verified.
    fn is_release_build(
        &self,
        http: &Http,
        program: &Program,
        known: &mut BTreeMap<String, Option<String>>,
    ) -> Result<bool, String> {
        let want = match known.get(&program.version) {
            Some(want) => want.clone(),
            None => {
                let url = format!(
                    "{}/releases/tags/v{}",
                    self.config.update.source, program.version
                );
                let release = match http.get(&url, MAX_LISTING) {
                    Err(Fetch::NotFound) => None,
                    Err(Fetch::Failed(words)) => return Err(words),
                    Ok(body) => serde_json::from_slice::<Value>(&body)
                        .ok()
                        .as_ref()
                        .and_then(release_of),
                };
                let want = match release
                    .as_ref()
                    .and_then(|release| release.assets.get("SHA256SUMS"))
                {
                    None => None,
                    Some(url) => {
                        let sums = http.get(url, MAX_LISTING).map_err(Fetch::words)?;
                        checksum_of(
                            &String::from_utf8_lossy(&sums),
                            &asset_name(&program.version, self.arch),
                        )
                    }
                };
                known.insert(program.version.clone(), want.clone());
                want
            }
        };
        Ok(want.is_some() && want == program.sha256)
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
            Err(fetch) => return Outcome::new("check-failed", Some(fetch.words()), None),
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
            records.dirs.extend(program.state_dirs.iter().cloned());
        }
        let mut outcome = Outcome::new("up-to-date", None, available.clone());
        let mut rows = Vec::new();
        let mut behind: Vec<(&Program, Placement)> = Vec::new();
        let mut kept: Vec<(&'static str, String)> = Vec::new();
        let mut known = BTreeMap::new();
        for program in &programs {
            let mut row = json!({
                "path": program.path,
                "version": program.version,
                "units": program.units.iter().map(|unit| &unit.name).collect::<Vec<_>>(),
                "behind": newer(&release.version_text, &program.version),
            });
            if newer(&release.version_text, &program.version) {
                let dir = program.path.parent().unwrap_or(Path::new("/"));
                let stage_ok = self.stage_root.as_deref().is_some_and(stage_runs);
                match placement(&probe_write(dir), stage_ok, &program.units) {
                    Placement::OsManaged(why) => {
                        row["kept"] = json!("os-managed");
                        kept.push(("os-managed", format!("{}: {why}", program.path.display())));
                    }
                    placement => match self.is_release_build(&http, program, &mut known) {
                        Err(reason) => {
                            return Outcome::new("check-failed", Some(reason), available);
                        }
                        Ok(false) => {
                            row["kept"] = json!("branch-build");
                            kept.push((
                                    "branch-build",
                                    format!(
                                        "{} says {} but is not that release's published binary (a build from a branch): it is kept",
                                        program.path.display(),
                                        program.version
                                    ),
                                ));
                        }
                        Ok(true) => {
                            row["placement"] = json!(match &placement {
                                Placement::InPlace => "in-place",
                                Placement::Staged => "staged",
                                _ => "refused",
                            });
                            behind.push((program, placement));
                        }
                    },
                }
            }
            rows.push(row);
        }
        outcome.detail = json!({ "programs": rows });
        if programs.is_empty() {
            outcome.reason = Some("no unit of this manager runs an Arca program".to_string());
        }
        let kept_words = (!kept.is_empty()).then(|| {
            kept.iter()
                .map(|(_, words)| words.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        });
        if behind.is_empty() {
            if let Some((kind, _)) = kept.first() {
                outcome.result = *kind;
                outcome.reason = kept_words;
            }
            return outcome;
        }
        if !install {
            outcome.result = "available";
            outcome.reason = kept_words;
            return outcome;
        }
        if self.keys.is_empty() {
            outcome.result = "unsigned";
            outcome.reason = Some(
                "this program holds no release signing key, so it installs no release by itself"
                    .to_string(),
            );
            return outcome;
        }
        let rolled_back = read_state(&self.config.state_dir)
            .and_then(|state| state["rolledBack"].as_str().map(str::to_string));
        if rolled_back.as_deref() == Some(release.version_text.as_str()) {
            outcome.result = "held-back";
            outcome.reason = Some(format!(
                "release {} was rolled back on this machine; the next newer release is installed",
                release.version_text
            ));
            return outcome;
        }
        if let Some((program, Placement::Refused(why))) = behind
            .iter()
            .find(|(_, placement)| matches!(placement, Placement::Refused(_)))
        {
            let reason = format!(
                "{} cannot be updated: {why}; every program was kept",
                program.path.display()
            );
            records.line(
                "failed",
                json!({ "release": release.version_text, "reason": reason }),
            );
            outcome.result = "failed";
            outcome.reason = Some(reason);
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
            .flat_map(|(program, _)| program.units.iter())
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
        behind: &[(&Program, Placement)],
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
        records.line(
            "download",
            json!({ "release": release.version_text, "asset": asset }),
        );
        let sums = match signed_sums(http, release, self.keys) {
            Ok(sums) => sums,
            Err(Sums::Unsigned(reason)) => {
                outcome.result = "unsigned";
                outcome.reason = Some(reason);
                return outcome;
            }
            Err(Sums::Bad(reason)) => return failed(outcome, reason),
        };
        let fetch = |name: &str| -> Result<Vec<u8>, String> {
            let url = release
                .assets
                .get(name)
                .ok_or_else(|| format!("release {} has no {name}", release.tag))?;
            http.get(url, MAX_ASSET).map_err(Fetch::words)
        };
        let (binary, installer) = match (fetch(&asset), fetch("install.sh")) {
            (Ok(binary), Ok(installer)) => (binary, installer),
            (Err(reason), _) | (_, Err(reason)) => return failed(outcome, reason),
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
        // The units to restart: the active ones of each program that changes, shims first, then nodes, then links.
        let mut restarts: Vec<Restart> = behind
            .iter()
            .flat_map(|(program, _)| {
                program.units.iter().map(move |unit| Restart {
                    unit: unit.clone(),
                    from: program.version.clone(),
                })
            })
            .filter(|restart| restart.unit.active)
            .collect();
        restarts.sort_by(|a, b| (a.unit.role(), &a.unit.name).cmp(&(b.unit.role(), &b.unit.name)));
        // Every program is written and checked beside its place before the first one is renamed, and the pending
        // record names what will change, so a run that is killed half-way is finished or undone by the next one.
        let mut prepared = Vec::new();
        for (program, placement) in behind {
            match self.prepare(program, placement, release, &binary) {
                Ok(step) => prepared.push((program, step)),
                Err(reason) => {
                    for (_, step) in &prepared {
                        step.discard();
                    }
                    return failed(outcome, reason);
                }
            }
        }
        let planned: Vec<Value> = prepared
            .iter()
            .map(|(_, step)| step.change().json())
            .collect();
        let pending = json!({
            "release": release.version_text,
            "atMs": now_ms(),
            "changes": planned,
            "units": restarts.iter().map(|r| json!({ "name": r.unit.name, "program": r.unit.program, "args": r.unit.args, "from": r.from })).collect::<Vec<_>>(),
        });
        if let Err(reason) = records.pending(&pending) {
            for (_, step) in &prepared {
                step.discard();
            }
            records.clear_pending();
            return failed(outcome, reason);
        }
        let mut changed: Vec<Changed> = Vec::new();
        let mut prepared = prepared.into_iter();
        while let Some((program, step)) = prepared.next() {
            match step.commit(release, &sha256) {
                Ok(change) => {
                    if let (Some(note), Changed::InPlace { .. }) = (&self.install_note, &change)
                        && let Some(old) = &program.sha256
                    {
                        renew_install_note(note, &program.named, old, &sha256);
                    }
                    records.line(
                        "install",
                        json!({ "program": program.path, "from": program.version, "to": release.version_text, "how": change.json()["how"] }),
                    );
                    changed.push(change);
                }
                Err(reason) => {
                    for (_, rest) in prepared {
                        rest.discard();
                    }
                    rollback(&changed);
                    records.clear_pending();
                    return failed(outcome, reason);
                }
            }
        }
        let mut moved: Vec<Value> = Vec::new();
        for (index, restart) in restarts.iter().enumerate() {
            match self.restart_and_watch(&restart.unit, &release.version_text) {
                Ok(version) => {
                    records.line(
                        "restart",
                        json!({ "unit": restart.unit.name, "version": version }),
                    );
                    moved.push(json!({ "unit": restart.unit.name, "role": restart.unit.role().as_str(), "from": restart.from, "to": version }));
                }
                Err(reason) => {
                    let reason = format!(
                        "{} did not come back on {}: {reason}",
                        restart.unit.name, release.version_text
                    );
                    records.line(
                        "restart",
                        json!({ "unit": restart.unit.name, "error": reason }),
                    );
                    return self.roll_back(
                        release.version_text.clone(),
                        &changed,
                        &restarts[..=index],
                        reason,
                        records,
                        outcome,
                    );
                }
            }
        }
        records.clear_pending();
        let templates = self.templates(behind);
        for said in &templates {
            records.line("templates", said.clone());
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

    /// Puts every changed program back, restarts `restarted` again on what they ran before, and reads their versions
    /// back.
    fn roll_back(
        &self,
        release: String,
        changed: &[Changed],
        restarted: &[Restart],
        reason: String,
        records: &Records,
        mut outcome: Outcome,
    ) -> Outcome {
        rollback(changed);
        let mut readback = Vec::new();
        for restart in restarted {
            let read = self.restart_and_watch(&restart.unit, &restart.from);
            readback.push(json!({ "unit": restart.unit.name, "version": read.as_ref().ok(), "error": read.err() }));
        }
        records.line(
            "rolled-back",
            json!({ "release": release, "reason": reason, "units": readback }),
        );
        records.clear_pending();
        outcome.result = "rolled-back";
        outcome.reason = Some(reason);
        outcome.available = Some(release);
        outcome.detail["units"] = json!(readback);
        outcome
    }

    /// An update an earlier run left pending (it was stopped after it changed programs and before it confirmed every
    /// unit): finished when every unit it named runs the release and is healthy now, else rolled back.
    fn settle_pending(&self, pending: &Value, records: &mut Records) -> Outcome {
        let release = pending["release"].as_str().unwrap_or_default().to_string();
        let changes: Vec<Changed> = pending["changes"]
            .as_array()
            .map(|changes| changes.iter().filter_map(Changed::of).collect())
            .unwrap_or_default();
        let restarts: Vec<Restart> = pending["units"]
            .as_array()
            .map(|units| {
                units
                    .iter()
                    .filter_map(|unit| {
                        Some(Restart {
                            unit: Unit {
                                name: unit["name"].as_str()?.to_string(),
                                program: PathBuf::from(unit["program"].as_str()?),
                                args: unit["args"]
                                    .as_array()?
                                    .iter()
                                    .filter_map(|arg| arg.as_str().map(str::to_string))
                                    .collect(),
                                active: true,
                            },
                            from: unit["from"].as_str()?.to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        for restart in &restarts {
            if let Some(config) = (restart.unit.role() == Role::Node)
                .then(|| node_config(&restart.unit))
                .flatten()
            {
                records.dirs.insert(config.state_dir);
            }
        }
        let mut outcome = Outcome::new("updated", None, Some(release.clone()));
        let unconfirmed: Vec<&str> = restarts
            .iter()
            .filter(|restart| !self.healthy_now(&restart.unit, &release))
            .map(|restart| restart.unit.name.as_str())
            .collect();
        if unconfirmed.is_empty() {
            records.line("finished", json!({ "release": release }));
            records.clear_pending();
            outcome.detail["units"] = json!(restarts.iter().map(|r| json!({ "unit": r.unit.name, "role": r.unit.role().as_str(), "from": r.from, "to": release })).collect::<Vec<_>>());
            return outcome;
        }
        let reason = format!(
            "an earlier run changed the programs and stopped before it confirmed {}",
            unconfirmed.join(", ")
        );
        self.roll_back(release, &changes, &restarts, reason, records, outcome)
    }

    /// Whether a unit runs `version` now and is healthy: active, and a node answers on its socket.
    fn healthy_now(&self, unit: &Unit, version: &str) -> bool {
        let Ok(running) = self.units.running(&unit.name) else {
            return false;
        };
        running.active
            && running.pid > 0
            && self.units.running_version(&unit.name, &running).as_deref() == Some(version)
            && (unit.role() != Role::Node || self.units.answers(unit))
    }

    /// Restarts one unit and watches it: it must be running within `health`, a node must answer on its socket, and it
    /// must then stay the same process, never restarted, for the whole of `health`, and say `version`.
    fn restart_and_watch(&self, unit: &Unit, version: &str) -> Result<String, String> {
        self.units.restart(&unit.name)?;
        let wait_until = |done: &dyn Fn() -> bool| {
            let deadline = Instant::now() + self.health;
            loop {
                if done() {
                    return true;
                }
                if Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(self.poll);
            }
        };
        let up = |running: &Result<Running, String>| {
            running
                .as_ref()
                .is_ok_and(|running| running.active && running.pid > 0)
        };
        if !wait_until(&|| up(&self.units.running(&unit.name))) {
            return Err(format!(
                "it was not running within {} s",
                self.health.as_secs_f32()
            ));
        }
        let first = self.units.running(&unit.name)?;
        if unit.role() == Role::Node && !wait_until(&|| self.units.answers(unit)) {
            return Err(format!(
                "it did not answer on its socket within {} s",
                self.health.as_secs_f32()
            ));
        }
        let watch_until = Instant::now() + self.health;
        loop {
            let now = self.units.running(&unit.name)?;
            if !now.active || now.pid != first.pid || now.restarts != first.restarts {
                return Err("it stopped or restarted after it started".to_string());
            }
            if Instant::now() >= watch_until {
                break;
            }
            std::thread::sleep(self.poll);
        }
        let now = self.units.running(&unit.name)?;
        let read = self
            .units
            .running_version(&unit.name, &now)
            .ok_or_else(|| "its process did not say its version".to_string())?;
        if read != version {
            return Err(format!("it runs {read}"));
        }
        Ok(read)
    }

    /// Writes the release beside where it goes, checked, ready to be put in place by one rename.
    fn prepare(
        &self,
        program: &Program,
        placement: &Placement,
        release: &Release,
        binary: &[u8],
    ) -> Result<Prepared, String> {
        match placement {
            Placement::InPlace => {
                let dir = program.path.parent().unwrap_or(Path::new("/"));
                let temp = dir.join(format!(
                    ".arca-update-{}.{}",
                    release.version_text,
                    std::process::id()
                ));
                write_checked(&temp, binary, &release.version_text)?;
                Ok(Prepared::InPlace {
                    temp,
                    path: program.path.clone(),
                })
            }
            Placement::Staged => {
                let dir = self
                    .staging(&program.path)
                    .ok_or("this user has no place to stage a release in")?;
                fs::create_dir_all(&dir)
                    .and_then(|()| fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)))
                    .map_err(|e| format!("{} cannot be made: {e}", dir.display()))?;
                let name = format!("arca-{}", release.version_text);
                let temp = dir.join(format!(".{name}.{}", std::process::id()));
                write_checked(&temp, binary, &release.version_text)?;
                Ok(Prepared::Staged { temp, dir, name })
            }
            Placement::OsManaged(why) | Placement::Refused(why) => Err(why.clone()),
        }
    }

    /// The templates of each node that has them, brought up to the release by the program its node now runs.
    fn templates(&self, behind: &[(&Program, Placement)]) -> Vec<Value> {
        let mut said = Vec::new();
        for (program, _) in behind {
            for unit in program
                .units
                .iter()
                .filter(|unit| unit.role() == Role::Node)
            {
                let (Some(config_path), Some(config)) = (unit.config(), node_config(unit)) else {
                    continue;
                };
                if !config.state_dir.join("templates").exists() {
                    continue;
                }
                let runs = self
                    .staging(&program.path)
                    .and_then(|dir| staged_program(&dir))
                    .filter(|staged| {
                        newer(
                            &staged.version,
                            &program_version(&program.path).unwrap_or_default(),
                        )
                    })
                    .map_or_else(|| program.path.clone(), |staged| staged.path);
                let mut command = Command::new(runs);
                command.args([
                    OsString::from("templates"),
                    OsString::from("install"),
                    OsString::from("--config"),
                    config_path.into_os_string(),
                ]);
                said.push(match run_for(&mut command, Duration::from_secs(120)) {
                    Ok((true, out, _)) => {
                        json!({ "unit": unit.name, "said": serde_json::from_str::<Value>(out.trim()).unwrap_or(json!(out.trim())) })
                    }
                    Ok((false, out, err)) => {
                        json!({ "unit": unit.name, "error": format!("{}{}", out.trim(), err.trim()) })
                    }
                    Err(reason) => json!({ "unit": unit.name, "error": reason }),
                });
            }
        }
        said
    }
}

/// A release written beside where it goes, and checked.
enum Prepared {
    InPlace {
        temp: PathBuf,
        path: PathBuf,
    },
    Staged {
        temp: PathBuf,
        dir: PathBuf,
        name: String,
    },
}

impl Prepared {
    /// What committing it changes.
    fn change(&self) -> Changed {
        match self {
            Self::InPlace { path, .. } => Changed::InPlace {
                path: path.clone(),
                previous: previous_of(path),
            },
            Self::Staged { dir, .. } => Changed::Staged {
                dir: dir.clone(),
                had_previous: dir.join(STAGED_FILE).exists(),
            },
        }
    }

    fn discard(&self) {
        let (Self::InPlace { temp, .. } | Self::Staged { temp, .. }) = self;
        let _ = fs::remove_file(temp);
    }

    /// Puts it in place: the old program is kept, then the new one is renamed over it in one step.
    fn commit(self, release: &Release, sha256: &str) -> Result<Changed, String> {
        let change = self.change();
        match self {
            Self::InPlace { temp, path } => {
                let previous = previous_of(&path);
                let _ = fs::remove_file(&previous);
                if let Err(e) = fs::hard_link(&path, &previous)
                    .or_else(|_| fs::copy(&path, &previous).map(|_| ()))
                {
                    let _ = fs::remove_file(&temp);
                    return Err(format!(
                        "the old program {} could not be kept: {e}",
                        path.display()
                    ));
                }
                if let Err(e) = fs::rename(&temp, &path) {
                    let _ = fs::remove_file(&temp);
                    return Err(format!("{} could not be replaced: {e}", path.display()));
                }
            }
            Self::Staged { temp, dir, name } => {
                let path = dir.join(&name);
                fs::rename(&temp, &path)
                    .map_err(|e| format!("{} cannot be written: {e}", path.display()))?;
                let current = dir.join(STAGED_FILE);
                if current.exists() {
                    fs::rename(&current, dir.join(STAGED_PREVIOUS_FILE))
                        .map_err(|e| format!("{} cannot be kept: {e}", current.display()))?;
                }
                let manifest =
                    json!({ "file": name, "version": release.version_text, "sha256": sha256 });
                write_private(&current, &format!("{manifest}\n"))
                    .map_err(|e| format!("{} cannot be written: {e}", current.display()))?;
                prune_staged(&dir);
            }
        }
        Ok(change)
    }
}

/// Removes the staged releases that are neither the current one nor the one before it. Only files named
/// `arca-<version>` are staged releases; nothing else in the directory is touched.
fn prune_staged(dir: &Path) {
    let keep: BTreeSet<PathBuf> = [STAGED_FILE, STAGED_PREVIOUS_FILE]
        .into_iter()
        .filter_map(|file| staged_manifest(dir, file).map(|staged| staged.path))
        .collect();
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let release = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("arca-"))
            .is_some_and(|version| Version::parse(version).is_some());
        if release && !keep.contains(&path) {
            let _ = fs::remove_file(path);
        }
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

fn previous_of(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(OsString::from).unwrap_or_default();
    name.push(".previous");
    path.with_file_name(name)
}

/// Writes the release's binary at `path`, and checks that it says `version` on this machine. A file that cannot be
/// written whole, or does not say it, is removed again.
fn write_checked(path: &Path, bytes: &[u8], version: &str) -> Result<(), String> {
    let written = (|| -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o755)
            .open(path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(path);
        return Err(format!(
            "the new program {} cannot be written: {e}",
            path.display()
        ));
    }
    let said = program_version(path);
    if said.as_deref() == Some(version) {
        return Ok(());
    }
    let _ = fs::remove_file(path);
    Err(match said {
        Some(said) => format!(
            "the downloaded program says it is {said}, not {version}; every program was kept"
        ),
        None => "the downloaded program does not start on this machine; every program was kept"
            .to_string(),
    })
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

/// The alert input: whether the updater's last settled run on this node failed. A node it never ran on has not failed.
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

/// The machine's update lock: in the manager's runtime directory, so one run per manager at a time; else in the
/// staging root, under a name no staging step removes.
fn lock_path(state_dir: &Path) -> PathBuf {
    if rustix::process::geteuid().is_root() {
        return PathBuf::from("/run").join(LOCK_FILE);
    }
    match std::env::var_os("XDG_RUNTIME_DIR").filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir).join(LOCK_FILE),
        None => stage_root()
            .unwrap_or_else(|| state_dir.join(UPDATE_DIR))
            .join(STAGE_LOCK_FILE),
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
        keys: RELEASE_KEYS,
        health: Duration::from_secs(config.update.health_seconds),
        poll: Duration::from_millis(500),
        install_note,
        lock: lock_path(&config.state_dir),
        stage_root: stage_root(),
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
