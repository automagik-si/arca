//! Moving containers to a linked host: a frozen plan, one owner card, pre-copy rounds, a short cutover, checks, a
//! measured downtime and a rollback that works while the source exists. The source is never deleted by the move; the
//! owner finishes a verified move with a card of its own.
//!
//! - `instance_move_plan` runs the whole `instance_move_preflight` again and freezes, for containers that all pass, a plan
//!   record in the move journal, named by its digest: a SHA-256 over the canonical plan without its times. A source that
//!   changes (its uuid, its devices, its measured size) gives another digest. A no-go blocks the plan with the
//!   preflight's own reasons, and a virtual machine is answered plainly: it is not movable yet.
//! - The card is the action `instance_move` (`action_propose`), whose arguments carry the digest, the instances and the
//!   target; the owner's passkey approves that digest, and the grant it arms is spent once.
//! - `instance_move` (the approved run) refuses an expired, unknown or altered plan, re-checks the target's room against
//!   the moves that are running, journals `begin`, and runs the move in the background: per instance, Incus pushes the
//!   container and its custom volumes from this host to the target (`push`: the target listens; a relay through this
//!   kernel is the fallback the plan names and is not built yet), in rounds while it runs, then stops it, sends the last
//!   delta and starts the copy; then checks it. The downtime is measured from the stop to the passed checks. Any failure
//!   before the checks pass rolls that instance back: the target copy is stopped and deleted, the source is started again
//!   when it ran, and the source's volumes are never touched. The kernel orchestrates; it copies no storage itself.
//! - `instance_move_status` reads how a move stands; `instance_move_finish` (a card) deletes a verified move's sources.
//!
//! The journal, `instance-moves.jsonl` in the state directory, is append-only JSON lines (mode 0600) with the events
//! `plan`, `begin`, `round`, `cutover`, `verify`, `result`, `rollback`, `end` and `finish`. It holds no secret: no
//! websocket secret and no certificate, only the target certificate's fingerprint. A torn last line is skipped, and the
//! next append starts on a new line. A move a stopped kernel left unfinished is not resumed: the next start rolls back
//! each instance it had not finished, as a failure before the checks would, and journals why.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arca_api::Scope;
use arca_incus::migrate::{OpState, progress_of};
use arca_incus::{IncusError, MoveWriter, PushTarget};
use arca_kernel::canon::{Digest, canonical_json};
use arca_mcp::{CallError, Caller, ToolHandler};
use serde_json::{Value, json};

use crate::action_proposal::ActionKind;
use crate::config::{NodeConfig, host_name_ok};
use crate::handlers::Clock;
use crate::kernel::Kernel;
use crate::move_preflight::{MovePreflightHandler, human, instance_ok, project_ok, seg};

/// The move journal's file name in the state directory.
pub const MOVES_FILE: &str = "instance-moves.jsonl";
/// How long a frozen plan may be approved and run.
pub const PLAN_TTL_MS: u64 = 60 * 60 * 1000;
/// The copy rounds while the instance runs, unless the plan names another count.
pub const DEFAULT_ROUNDS: u64 = 2;
/// The file the checks compare when the plan names none.
pub const DEFAULT_SPOT_CHECK: &str = "/etc/hostname";
/// What every card and plan promises about a failure.
pub const ROLLBACK_PROMISE: &str = "If anything fails before the moved copy passes its checks, Arca removes the copy on the target and starts the source again; the source keeps running while it is copied, its volumes are never touched, and it is never deleted unless you finish the move.";
/// The largest journal this build reads.
const MAX_JOURNAL_BYTES: u64 = 64 * 1024 * 1024;
/// How long a graceful stop of the source may take before it is forced.
const STOP_GRACE: Duration = Duration::from_secs(60);
/// How long a start may take.
const START_DEADLINE: Duration = Duration::from_secs(120);
/// How long one copy of one object may run before it counts as failed.
const ROUND_DEADLINE: Duration = Duration::from_secs(24 * 60 * 60);
/// The rate a card's downtime estimate assumes, in bytes a second, and the share of the data it assumes changes between
/// the last round and the stop.
const ASSUMED_RATE: u64 = 50 * 1024 * 1024;
const ASSUMED_DELTA_PERCENT: u64 = 1;
/// The seconds a card's estimate gives the stop, the start and the checks.
const ASSUMED_STOP_START_SECONDS: u64 = 30;

fn refused(code: &str, message: impl Into<String>) -> CallError {
    CallError::Refused {
        code: code.to_string(),
        message: message.into(),
    }
}

fn invalid(message: impl Into<String>) -> CallError {
    CallError::Invalid(message.into())
}

fn internal() -> CallError {
    CallError::Internal("the kernel could not complete the call".to_string())
}

fn digest_of(value: &Value) -> String {
    let text = canonical_json(value).unwrap_or_default();
    Digest::of(text.as_bytes()).to_hex()
}

/// The append-only move journal.
pub struct Journal {
    path: PathBuf,
    lock: Mutex<()>,
}

impl Journal {
    pub fn at(state_dir: &Path) -> Self {
        Self {
            path: state_dir.join(MOVES_FILE),
            lock: Mutex::new(()),
        }
    }

    /// Appends one line. A file that is not a private regular file is refused; a torn last line is closed first, so the
    /// new line starts on its own.
    pub fn append(&self, line: &Value) -> std::io::Result<()> {
        let _held = self
            .lock
            .lock()
            .map_err(|_| std::io::Error::other("poisoned"))?;
        match fs::symlink_metadata(&self.path) {
            Ok(meta) if !meta.is_file() || meta.permissions().mode() & 0o077 != 0 => {
                return Err(std::io::Error::other(
                    "the move journal is not a private file",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .mode(0o600)
            .open(&self.path)?;
        let mut bytes = Vec::new();
        let length = file.metadata()?.len();
        if length > 0 {
            file.seek(SeekFrom::Start(length - 1))?;
            let mut last = [0u8; 1];
            file.read_exact(&mut last)?;
            if last[0] != b'\n' {
                bytes.push(b'\n');
            }
        }
        bytes.extend(serde_json::to_vec(line).map_err(std::io::Error::other)?);
        bytes.push(b'\n');
        file.write_all(&bytes)?;
        file.sync_all()
    }

    /// Every line that reads as JSON. A torn last line (no newline after it) is skipped; any other line that does not
    /// read is skipped and said on the log.
    pub fn lines(&self) -> Vec<Value> {
        let _held = self.lock.lock();
        let Ok(meta) = fs::symlink_metadata(&self.path) else {
            return Vec::new();
        };
        if !meta.is_file() || meta.len() > MAX_JOURNAL_BYTES {
            eprintln!("arca: the move journal is not a regular file under 64 MiB; it is not read");
            return Vec::new();
        }
        let Ok(text) = fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        let complete = text.ends_with('\n');
        let rows: Vec<&str> = text.lines().collect();
        let mut out = Vec::new();
        for (index, row) in rows.iter().enumerate() {
            match serde_json::from_str::<Value>(row) {
                Ok(value) if value.is_object() => out.push(value),
                _ if index + 1 == rows.len() && !complete => {}
                _ => eprintln!("arca: a line of the move journal does not read and is skipped"),
            }
        }
        out
    }

    /// The frozen plan with `digest`, as its `plan` line holds it.
    pub fn plan(&self, digest: &str) -> Option<Value> {
        self.lines()
            .into_iter()
            .rev()
            .find(|line| line["event"] == "plan" && line["plan"]["digest"] == digest)
            .map(|line| line["plan"].clone())
    }
}

/// One instance of a plan request.
#[derive(Debug, Clone)]
struct Wanted {
    project: String,
    name: String,
    spot_check: String,
}

fn parse_refs(list: &Value, spot_checks: bool) -> Result<Vec<Wanted>, CallError> {
    let list = list
        .as_array()
        .filter(|list| (1..=crate::move_preflight::MAX_INSTANCES).contains(&list.len()))
        .ok_or_else(|| {
            invalid("instances: expected a list of 1 to 50 {project, name} references")
        })?;
    let mut out = Vec::new();
    for (index, item) in list.iter().enumerate() {
        let object = item
            .as_object()
            .ok_or_else(|| invalid(format!("instances[{index}]: expected {{project, name}}")))?;
        let allowed: &[&str] = if spot_checks {
            &["project", "name", "spotCheck"]
        } else {
            &["project", "name"]
        };
        if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
            return Err(invalid(arca_api::unknown_field(
                &format!("/instances/{index}"),
                key,
                allowed,
            )));
        }
        let project = object
            .get("project")
            .and_then(Value::as_str)
            .filter(|project| project_ok(project))
            .ok_or_else(|| {
                invalid(format!(
                    "instances[{index}].project: expected an Incus project name"
                ))
            })?;
        let name = object
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| instance_ok(name))
            .ok_or_else(|| {
                invalid(format!(
                    "instances[{index}].name: expected an Incus instance name"
                ))
            })?;
        let spot_check = match object.get("spotCheck") {
            None => DEFAULT_SPOT_CHECK.to_string(),
            Some(path) => path
                .as_str()
                .filter(|path| spot_check_ok(path))
                .ok_or_else(|| {
                    invalid(format!(
                        "instances[{index}].spotCheck: expected an absolute file path"
                    ))
                })?
                .to_string(),
        };
        if out
            .iter()
            .any(|seen: &Wanted| seen.project == project && seen.name == name)
        {
            return Err(invalid(format!(
                "instances[{index}]: {project}/{name} is named twice"
            )));
        }
        out.push(Wanted {
            project: project.to_string(),
            name: name.to_string(),
            spot_check,
        });
    }
    Ok(out)
}

fn spot_check_ok(path: &str) -> bool {
    path.starts_with('/')
        && path.len() >= 2
        && path.len() <= 1024
        && !path.split('/').any(|part| part == "..")
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/'))
}

fn target_arg(args: &Value) -> Result<String, CallError> {
    args.get("target")
        .and_then(Value::as_str)
        .filter(|target| host_name_ok(target))
        .map(str::to_string)
        .ok_or_else(|| invalid("target: expected the name of a linked host"))
}

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
}

/// The instance's devices: expanded, or its own when Incus gave no expansion.
fn devices(instance: &Value) -> Vec<(String, Value)> {
    let map = instance
        .get("expanded_devices")
        .and_then(Value::as_object)
        .or_else(|| instance.get("devices").and_then(Value::as_object));
    let mut list: Vec<(String, Value)> = map
        .map(|map| map.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    list.sort_by(|a, b| a.0.cmp(&b.0));
    list
}

fn is_root(device: &Value) -> bool {
    text(device, "type") == Some("disk") && text(device, "path") == Some("/")
}

/// A target's Incus, as this node reaches it: the socket the linked host's config names.
struct Target {
    socket: PathBuf,
}

fn linked_target(cfg: &NodeConfig, name: &str) -> Result<Target, String> {
    let linked = cfg
        .approval
        .as_ref()
        .and_then(|approval| approval.linked_hosts.iter().find(|host| host.name == name))
        .ok_or_else(|| format!("no linked host named {name} in approval.linkedHosts"))?;
    let socket = linked.incus_socket.clone().ok_or_else(|| {
        format!(
            "the linked host {name} has no incusSocket, so its Incus cannot be written from here"
        )
    })?;
    Ok(Target { socket })
}

/// The transport facts of a target read from its server document: the first address it listens on, and its
/// certificate's PEM.
fn transport_facts(server: &Value) -> (Option<String>, Option<String>) {
    let address = server
        .pointer("/environment/addresses")
        .and_then(Value::as_array)
        .and_then(|list| list.iter().filter_map(Value::as_str).next())
        .map(str::to_string);
    let certificate = server
        .pointer("/environment/certificate")
        .and_then(Value::as_str)
        .filter(|pem| !pem.is_empty())
        .map(str::to_string);
    (address, certificate)
}

/// The custom volumes an instance takes with it, each `{device, pool, name, project, path}`: the project is the one Incus
/// keeps the volume in (the instance's, when that project sets `features.storage.volumes`, else `default`).
fn volumes_of(source: &MoveWriter, project: &str, instance: &Value) -> Vec<Value> {
    let volume_project = if project != "default"
        && source
            .get(&format!("/1.0/projects/{}", seg(project)))
            .ok()
            .and_then(|doc| {
                doc.pointer("/config/features.storage.volumes")
                    .and_then(Value::as_str)
                    .map(|value| value == "true")
            })
            .unwrap_or(false)
    {
        project
    } else {
        "default"
    };
    devices(instance)
        .into_iter()
        .filter(|(_, device)| text(device, "type") == Some("disk") && !is_root(device))
        .filter_map(|(device_name, device)| {
            let pool = text(&device, "pool")?;
            let name = text(&device, "source")?;
            Some(json!({
                "device": device_name,
                "pool": pool,
                "name": name,
                "project": volume_project,
                "path": text(&device, "path"),
            }))
        })
        .collect()
}

/// The words of a row's failing checks, for a blocked plan: `size: <detail>`. A `devices` finding about a custom volume
/// the target lacks does not count when that volume is one of `moved` (the devices whose volumes the move takes along).
fn no_go_reasons(row: &Value, moved: &[String]) -> Vec<String> {
    let mut reasons = Vec::new();
    for check in row["checks"].as_array().into_iter().flatten() {
        if check["ok"] != false {
            continue;
        }
        let name = check["check"].as_str().unwrap_or("?");
        if name == "devices" {
            let failing: Vec<&str> = check["findings"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|finding| finding["ok"] == false)
                .filter(|finding| {
                    !finding["subject"]
                        .as_str()
                        .is_some_and(|subject| moved.iter().any(|device| device == subject))
                })
                .map(|finding| finding["detail"].as_str().unwrap_or("not ok"))
                .collect();
            if !failing.is_empty() {
                reasons.push(format!("devices: {}", failing.join("; ")));
            }
            continue;
        }
        reasons.push(format!(
            "{name}: {}",
            check["detail"].as_str().unwrap_or("not ok")
        ));
    }
    if reasons.is_empty()
        && row["go"] != true
        && row["checks"].as_array().is_none_or(Vec::is_empty)
        && let Some(reason) = row["reason"].as_str()
    {
        reasons.push(reason.to_string());
    }
    reasons
}

/// The downtime a card estimates, in seconds: the stop, the start and the checks, and a last delta of 1% of the data at
/// 50 MiB/s. The move measures the real one.
fn estimated_downtime(bytes: u64) -> u64 {
    let delta = bytes.saturating_mul(ASSUMED_DELTA_PERCENT) / 100;
    ASSUMED_STOP_START_SECONDS + delta.div_ceil(ASSUMED_RATE)
}

/// What one plan call answers: whether it is a go, the frozen plan, and what blocked it.
pub struct Planned {
    pub go: bool,
    pub plan: Option<Value>,
    pub blocked: Vec<Value>,
    pub preflight: Value,
    pub target: String,
}

/// The moves of this node: the plans, the runs, their progress and their journal.
pub struct Moves {
    cfg: Arc<NodeConfig>,
    storage_root: Option<PathBuf>,
    kernel: Arc<Mutex<Kernel>>,
    clock: Clock,
    journal: Journal,
    /// The progress of each running copy, by move, project and name.
    live: Mutex<BTreeMap<(String, String, String), Progress>>,
    /// The moves running in this process.
    running: Mutex<Vec<String>>,
    poll: Duration,
    verify_wait: Duration,
}

/// How a running copy stands.
#[derive(Debug, Clone, Default)]
struct Progress {
    round: u64,
    phase: String,
    /// Bytes of the objects this round already finished.
    done: u64,
    /// The bytes Incus says it moved of the object it copies now, and its rate.
    processed: Option<u64>,
    speed: Option<u64>,
}

impl Moves {
    pub fn new(cfg: Arc<NodeConfig>, kernel: Arc<Mutex<Kernel>>, clock: Clock) -> Self {
        let journal = Journal::at(&cfg.state_dir);
        Self {
            cfg,
            storage_root: None,
            kernel,
            clock,
            journal,
            live: Mutex::new(BTreeMap::new()),
            running: Mutex::new(Vec::new()),
            poll: Duration::from_secs(2),
            verify_wait: Duration::from_secs(60),
        }
    }

    /// The same, measuring the instances' storage folders under `root` (the folder that holds `storage-pools`).
    pub fn with_storage_root(mut self, root: PathBuf) -> Self {
        self.storage_root = Some(root);
        self
    }

    /// The same, polling a running copy every `poll`, and giving the moved copy `verify_wait` to come up.
    pub fn with_timing(mut self, poll: Duration, verify_wait: Duration) -> Self {
        self.poll = poll;
        self.verify_wait = verify_wait;
        self
    }

    pub fn journal(&self) -> &Journal {
        &self.journal
    }

    fn now(&self) -> u64 {
        (self.clock)()
    }

    fn note(&self, line: Value) {
        if let Err(error) = self.journal.append(&line) {
            eprintln!("arca: the move journal could not be written: {error}");
        }
    }

    fn preflight(&self) -> MovePreflightHandler {
        let handler = MovePreflightHandler::new(Arc::clone(&self.cfg));
        match &self.storage_root {
            Some(root) => handler.with_storage_root(root.clone()),
            None => handler,
        }
    }

    fn source_name(&self) -> String {
        self.cfg
            .approval_host
            .clone()
            .unwrap_or_else(|| "this node".to_string())
    }

    /// Plans a move: the whole preflight again, then a frozen record for containers that all pass.
    fn compute(
        &self,
        wanted: &[Wanted],
        target_name: &str,
        rounds: u64,
    ) -> Result<Planned, CallError> {
        let refs: Vec<Value> = wanted
            .iter()
            .map(|one| json!({ "project": one.project, "name": one.name }))
            .collect();
        let preflight = self.preflight().call(
            json!({ "instances": refs, "target": target_name }),
            &Caller {
                token_id: "instance-move-plan".to_string(),
                scope: Scope::Viewer,
                tools: None,
            },
        )?;
        let source = MoveWriter::new(self.cfg.incus.socket.clone());
        let target = linked_target(&self.cfg, target_name);
        let target_writer = target
            .as_ref()
            .ok()
            .map(|t| MoveWriter::new(t.socket.clone()));
        let server = target_writer.as_ref().and_then(|w| w.get("/1.0").ok());
        let (address, certificate) = server.as_ref().map(transport_facts).unwrap_or((None, None));
        let mut blocked = Vec::new();
        let mut instances = Vec::new();
        for (index, one) in wanted.iter().enumerate() {
            let row = &preflight["rows"][index];
            let doc = source
                .get(&format!(
                    "/1.0/instances/{}?project={}",
                    seg(&one.name),
                    seg(&one.project)
                ))
                .ok();
            let moved: Vec<String> = doc
                .as_ref()
                .map(|doc| {
                    volumes_of(&source, &one.project, doc)
                        .iter()
                        .filter_map(|volume| volume["device"].as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let mut reasons = no_go_reasons(row, &moved);
            if let Some(doc) = &doc {
                if text(doc, "type") == Some("virtual-machine") {
                    reasons.insert(
                        0,
                        "virtual machines are not movable yet: this build moves containers only; a virtual machine's move (not live first) comes later".to_string(),
                    );
                }
                for (device_name, device) in devices(doc) {
                    if text(&device, "type") == Some("disk")
                        && text(&device, "pool").is_none()
                        && text(&device, "source").is_some_and(|source| source.starts_with('/'))
                    {
                        reasons.push(format!(
                            "the host folder {} ({device_name}) is not copied by Incus; move what it holds into a custom volume first",
                            text(&device, "source").unwrap_or("?")
                        ));
                    }
                }
                if let Some(writer) = &target_writer {
                    let there = writer.get(&format!(
                        "/1.0/instances/{}?project={}",
                        seg(&one.name),
                        seg(&one.project)
                    ));
                    if there.is_ok() {
                        reasons.push(format!(
                            "{target_name} already has an instance {}/{}; a move never replaces one",
                            one.project, one.name
                        ));
                    }
                    for volume in volumes_of(&source, &one.project, doc) {
                        let found = writer.get(&format!(
                            "/1.0/storage-pools/{}/volumes/custom/{}?project={}",
                            seg(volume["pool"].as_str().unwrap_or_default()),
                            seg(volume["name"].as_str().unwrap_or_default()),
                            seg(volume["project"].as_str().unwrap_or_default())
                        ));
                        if found.is_ok() {
                            reasons.push(format!(
                                "{target_name} already has the custom volume {}/{}; a move never overwrites one",
                                volume["pool"].as_str().unwrap_or("?"),
                                volume["name"].as_str().unwrap_or("?")
                            ));
                        }
                    }
                }
            }
            if !reasons.is_empty() {
                blocked
                    .push(json!({ "project": one.project, "name": one.name, "reasons": reasons }));
                continue;
            }
            let Some(doc) = doc else {
                blocked.push(json!({
                    "project": one.project, "name": one.name,
                    "reasons": ["the instance could not be read on this node"],
                }));
                continue;
            };
            let size = row["checks"]
                .as_array()
                .and_then(|checks| checks.iter().find(|check| check["check"] == "size"))
                .map(|check| check["measure"].clone())
                .unwrap_or(Value::Null);
            let driver = row["checks"]
                .as_array()
                .and_then(|checks| {
                    checks
                        .iter()
                        .find(|check| check["check"] == "storageDriver")
                })
                .cloned()
                .unwrap_or(Value::Null);
            let measured = size["measuredBytes"].as_u64().unwrap_or(0);
            let expanded = doc
                .get("expanded_devices")
                .cloned()
                .or_else(|| doc.get("devices").cloned())
                .unwrap_or(json!({}));
            instances.push(json!({
                "project": one.project,
                "name": one.name,
                "uuid": doc.pointer("/config/volatile.uuid").and_then(Value::as_str),
                "type": text(&doc, "type").unwrap_or("container"),
                "status": text(&doc, "status"),
                "devicesDigest": digest_of(&expanded),
                "profiles": doc.get("profiles").cloned().unwrap_or(json!([])),
                "sourcePool": size["sourcePool"],
                "targetPool": size["targetPool"],
                "sourceDriver": driver["sourceDriver"],
                "targetDriver": driver["targetDriver"],
                "bytes": {
                    "instance": size["instanceBytes"],
                    "snapshots": size["snapshotBytes"],
                    "volumes": size["volumeBytes"],
                    "hostFolders": size["hostFolderBytes"],
                    "measured": measured,
                    "needed": size["neededBytes"],
                },
                "volumes": volumes_of(&source, &one.project, &doc),
                "rounds": {
                    "preCopy": rounds,
                    "order": (1..=rounds)
                        .map(|round| if round == 1 {
                            "a full copy while it runs, with its snapshots and custom volumes".to_string()
                        } else {
                            format!("refresh {} while it runs: only what changed", round - 1)
                        })
                        .collect::<Vec<_>>(),
                },
                "cutover": ["stop the source", "send the last delta", "start the copy on the target"],
                "verify": [
                    "the copy runs",
                    "each network card that had an address has one",
                    "a proxy's port answers",
                    "each custom volume is attached",
                    format!("the file {} is the same on both", one.spot_check),
                ],
                "spotCheck": one.spot_check,
                "estimatedDowntimeSeconds": estimated_downtime(measured),
            }));
        }
        let transport = match (&address, &certificate) {
            (Some(address), Some(certificate)) => json!({
                "mode": "push",
                "address": address,
                "certificateFingerprint": Digest::of(certificate.as_bytes()).to_hex(),
                "fallback": "relay",
                "fallbackAvailable": false,
            }),
            _ => json!({
                "mode": "relay",
                "address": null,
                "certificateFingerprint": null,
                "fallback": "relay",
                "fallbackAvailable": false,
            }),
        };
        if blocked.is_empty() && transport["mode"] == "relay" {
            let why = match &target {
                Err(why) => why.clone(),
                Ok(_) if server.is_none() => format!("{target_name}'s Incus could not be read"),
                Ok(_) => format!(
                    "{target_name}'s Incus does not listen on the network (no core.https_address), so this host cannot push to it"
                ),
            };
            for one in wanted {
                blocked.push(json!({
                    "project": one.project, "name": one.name,
                    "reasons": [format!("{why}; a relay through this kernel is the fallback, and it is not available yet")],
                }));
            }
        }
        if !blocked.is_empty() {
            return Ok(Planned {
                go: false,
                plan: None,
                blocked,
                preflight,
                target: target_name.to_string(),
            });
        }
        let socket = target
            .as_ref()
            .map(|t| t.socket.display().to_string())
            .unwrap_or_default();
        let mut plan = json!({
            "version": 1,
            "source": { "node": self.source_name(), "incusSocket": self.cfg.incus.socket.display().to_string() },
            "target": { "name": target_name, "incusSocket": socket },
            "credential": format!("Arca writes to {target_name}'s Incus through the linked host's incusSocket {socket}, which the owner configured"),
            "transport": transport,
            "rounds": rounds,
            "instances": instances,
            "rollback": ROLLBACK_PROMISE,
        });
        let digest = digest_of(&plan);
        let now = self.now();
        plan["digest"] = json!(digest);
        plan["createdAt"] = json!(now);
        plan["expiresAt"] = json!(now.saturating_add(PLAN_TTL_MS));
        Ok(Planned {
            go: true,
            plan: Some(plan),
            blocked,
            preflight,
            target: target_name.to_string(),
        })
    }

    /// The rows of a plan's card: one per instance, then the transport, the credential and the rollback.
    pub fn card_rows(plan: &Value) -> Vec<(String, String)> {
        let target = plan["target"]["name"].as_str().unwrap_or("?");
        let mut rows = Vec::new();
        for instance in plan["instances"].as_array().into_iter().flatten() {
            let needed = instance["bytes"]["needed"].as_u64().unwrap_or(0);
            let rounds = instance["rounds"]["preCopy"].as_u64().unwrap_or(0);
            let volumes = instance["volumes"].as_array().map_or(0, Vec::len);
            rows.push((
                format!(
                    "Move {}/{}",
                    instance["project"].as_str().unwrap_or("?"),
                    instance["name"].as_str().unwrap_or("?")
                ),
                format!(
                    "needs {} on {target}'s pool {}{}; {rounds} copy round(s) while it runs, then the stop, the last delta and the start; downtime about {} s (an estimate: the move measures the real one)",
                    human(needed),
                    instance["targetPool"].as_str().unwrap_or("?"),
                    if volumes == 0 {
                        String::new()
                    } else {
                        format!(", with {volumes} custom volume(s)")
                    },
                    instance["estimatedDowntimeSeconds"].as_u64().unwrap_or(0)
                ),
            ));
        }
        rows.push((
            "Transport".to_string(),
            match plan["transport"]["mode"].as_str() {
                Some("push") => format!(
                    "Incus on this host pushes to {target}'s Incus at {}",
                    plan["transport"]["address"].as_str().unwrap_or("?")
                ),
                _ => "a relay through this kernel (not available yet)".to_string(),
            },
        ));
        rows.push((
            "Credential".to_string(),
            plan["credential"].as_str().unwrap_or_default().to_string(),
        ));
        rows.push(("Rollback".to_string(), ROLLBACK_PROMISE.to_string()));
        rows.push((
            "Source".to_string(),
            "kept, stopped, on this host after a verified move: it is deleted only when you finish the move".to_string(),
        ));
        rows
    }

    fn refs_of(plan: &Value) -> Vec<Value> {
        plan["instances"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|one| json!({ "project": one["project"], "name": one["name"] }))
            .collect()
    }

    fn move_id(digest: &str) -> String {
        format!("mv-{}", &digest[..12])
    }

    /// What has changed between the approved plan and the one the node makes now: each difference in words.
    fn differences(approved: &Value, fresh: &Value) -> Vec<String> {
        let mut out = Vec::new();
        if approved["transport"]["certificateFingerprint"]
            != fresh["transport"]["certificateFingerprint"]
            || approved["transport"]["address"] != fresh["transport"]["address"]
        {
            out.push("the target's Incus address or certificate".to_string());
        }
        if approved["target"]["incusSocket"] != fresh["target"]["incusSocket"] {
            out.push("the target's incusSocket".to_string());
        }
        let empty = Vec::new();
        let old = approved["instances"].as_array().unwrap_or(&empty);
        let new = fresh["instances"].as_array().unwrap_or(&empty);
        for (a, b) in old.iter().zip(new) {
            let label = format!(
                "{}/{}",
                a["project"].as_str().unwrap_or("?"),
                a["name"].as_str().unwrap_or("?")
            );
            for (key, words) in [
                ("uuid", "its uuid (it is not the same instance)"),
                ("type", "its type"),
                ("devicesDigest", "its devices"),
                ("volumes", "its custom volumes"),
                ("targetPool", "the target pool"),
            ] {
                if a[key] != b[key] {
                    out.push(format!("{label}: {words}"));
                }
            }
            let needed = a["bytes"]["needed"].as_u64().unwrap_or(0);
            let measured = b["bytes"]["measured"].as_u64().unwrap_or(u64::MAX);
            if measured > needed {
                out.push(format!(
                    "{label}: it grew to {}, past the {} the approved plan counted with its headroom",
                    human(measured),
                    human(needed)
                ));
            }
        }
        out
    }

    /// The moves running in this process to `target`, and the bytes each instance of them may still land on each pool.
    fn pending_on(&self, target: &str) -> BTreeMap<String, u64> {
        let running = self.running.lock().map(|r| r.clone()).unwrap_or_default();
        let mut out = BTreeMap::new();
        if running.is_empty() {
            return out;
        }
        let records = fold(&self.journal.lines());
        for id in running {
            let Some(record) = records.get(&id) else {
                continue;
            };
            if record.target != target {
                continue;
            }
            for instance in &record.instances {
                if instance.result.is_some() {
                    continue;
                }
                *out.entry(instance.target_pool.clone()).or_insert(0) += instance.needed;
            }
        }
        out
    }

    /// Starts an approved plan's run in the background, after every check that refuses it.
    fn start(self: &Arc<Self>, plan: Value, proposal: Option<String>) -> Result<Value, CallError> {
        let digest = plan["digest"].as_str().unwrap_or_default().to_string();
        let id = Self::move_id(&digest);
        let lines = self.journal.lines();
        if lines
            .iter()
            .any(|line| line["event"] == "begin" && line["plan"] == digest.as_str())
        {
            return Err(refused(
                "AlreadyRun",
                format!("this plan already ran as move {id}; plan the move again for a new card"),
            ));
        }
        let target_name = plan["target"]["name"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        // The node reads everything again now: an altered plan is refused, and nothing moves.
        let wanted: Vec<Wanted> = plan["instances"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|one| Wanted {
                project: one["project"].as_str().unwrap_or_default().to_string(),
                name: one["name"].as_str().unwrap_or_default().to_string(),
                spot_check: one["spotCheck"]
                    .as_str()
                    .unwrap_or(DEFAULT_SPOT_CHECK)
                    .to_string(),
            })
            .collect();
        let rounds = plan["rounds"].as_u64().unwrap_or(DEFAULT_ROUNDS);
        let fresh = self.compute(&wanted, &target_name, rounds)?;
        if !fresh.go {
            let reasons: Vec<String> = fresh
                .blocked
                .iter()
                .map(|row| {
                    format!(
                        "{}/{}: {}",
                        row["project"].as_str().unwrap_or("?"),
                        row["name"].as_str().unwrap_or("?"),
                        row["reasons"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join("; ")
                    )
                })
                .collect();
            return Err(refused(
                "PlanChanged",
                format!(
                    "the move is no longer a go, so nothing moved: {}. Plan it again",
                    reasons.join(". ")
                ),
            ));
        }
        let fresh_plan = fresh.plan.unwrap_or(Value::Null);
        let changed = Self::differences(&plan, &fresh_plan);
        if !changed.is_empty() {
            return Err(refused(
                "PlanChanged",
                format!(
                    "the instances changed since the owner approved the plan, so nothing moved: {}. Plan it again for a new card",
                    changed.join("; ")
                ),
            ));
        }
        // The target's room, less what the moves running now may still land there.
        let pending = self.pending_on(&target_name);
        if !pending.is_empty() {
            let mut need: BTreeMap<String, u64> = BTreeMap::new();
            for one in fresh_plan["instances"].as_array().into_iter().flatten() {
                *need
                    .entry(one["targetPool"].as_str().unwrap_or_default().to_string())
                    .or_insert(0) += one["bytes"]["needed"].as_u64().unwrap_or(0);
            }
            let target = linked_target(&self.cfg, &target_name)
                .map_err(|why| refused("TargetUnavailable", why))?;
            let writer = MoveWriter::new(target.socket);
            for (pool, bytes) in need {
                let others = pending.get(&pool).copied().unwrap_or(0);
                let free = pool_free(&writer, &pool).unwrap_or(0);
                if bytes.saturating_add(others) > free {
                    return Err(refused(
                        "TargetFull",
                        format!(
                            "{target_name}'s pool {pool} has {} free, and the moves running to it may still need {}; this one needs {}. Nothing moved",
                            human(free),
                            human(others),
                            human(bytes)
                        ),
                    ));
                }
            }
        }
        let now = self.now();
        let source = MoveWriter::new(self.cfg.incus.socket.clone());
        let instances: Vec<Value> = plan["instances"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|one| {
                let running = source
                    .get(&format!(
                        "/1.0/instances/{}?project={}",
                        seg(one["name"].as_str().unwrap_or_default()),
                        seg(one["project"].as_str().unwrap_or_default())
                    ))
                    .ok()
                    .and_then(|doc| doc.get("status").cloned())
                    == Some(json!("Running"));
                json!({
                    "project": one["project"], "name": one["name"],
                    "wasRunning": running,
                    "roundsTotal": one["rounds"]["preCopy"].as_u64().unwrap_or(0) + 1,
                    "bytesTotal": one["bytes"]["measured"],
                    "needed": one["bytes"]["needed"],
                    "targetPool": one["targetPool"],
                    "volumes": one["volumes"],
                })
            })
            .collect();
        self.note(json!({
            "event": "begin", "at": now, "move": id, "plan": digest, "target": target_name,
            "proposal": proposal, "instances": instances,
        }));
        if let Ok(mut running) = self.running.lock() {
            running.push(id.clone());
        }
        let moves = Arc::clone(self);
        let (thread_id, thread_plan) = (id.clone(), plan.clone());
        let spawned = std::thread::Builder::new()
            .name(format!("arca-{id}"))
            .spawn(move || moves.run(&thread_id, &thread_plan, &instances, proposal));
        if spawned.is_err() {
            self.note(json!({ "event": "end", "at": self.now(), "move": id, "reason": "the move's thread could not start" }));
            return Err(internal());
        }
        Ok(json!({
            "move": id,
            "plan": digest,
            "target": target_name,
            "phase": "copying",
            "instances": Self::refs_of(&plan),
            "message": format!(
                "the move {id} to {target_name} has started; follow it with instance_move_status {{\"move\": \"{id}\"}}"
            ),
        }))
    }

    /// The whole run, one instance after the other. A failed instance is rolled back and the next one still moves.
    fn run(&self, id: &str, plan: &Value, begun: &[Value], proposal: Option<String>) {
        let mut moved = 0usize;
        for (index, one) in plan["instances"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            let was_running = begun
                .get(index)
                .is_some_and(|row| row["wasRunning"] == true);
            if self.move_one(id, plan, one, was_running) {
                moved += 1;
            }
        }
        let total = plan["instances"].as_array().map_or(0, Vec::len);
        self.note(
            json!({ "event": "end", "at": self.now(), "move": id, "moved": moved, "of": total }),
        );
        if let Ok(mut running) = self.running.lock() {
            running.retain(|running| running != id);
        }
        if let (Some(proposal), Ok(mut kernel)) = (proposal, self.kernel.lock()) {
            let code = if moved == total {
                "MoveVerified"
            } else if moved == 0 {
                "MoveRolledBack"
            } else {
                "MovePartlyRolledBack"
            };
            kernel.journal_late_result(&proposal, moved == total, code);
        }
    }

    fn set_live(&self, id: &str, project: &str, name: &str, update: impl FnOnce(&mut Progress)) {
        if let Ok(mut live) = self.live.lock() {
            let entry = live
                .entry((id.to_string(), project.to_string(), name.to_string()))
                .or_default();
            update(entry);
        }
    }

    fn clear_live(&self, id: &str, project: &str, name: &str) {
        if let Ok(mut live) = self.live.lock() {
            live.remove(&(id.to_string(), project.to_string(), name.to_string()));
        }
    }

    /// Moves one instance; answers whether it moved and passed its checks.
    fn move_one(&self, id: &str, plan: &Value, one: &Value, was_running: bool) -> bool {
        let project = one["project"].as_str().unwrap_or_default().to_string();
        let name = one["name"].as_str().unwrap_or_default().to_string();
        let target_name = plan["target"]["name"].as_str().unwrap_or_default();
        let target = match linked_target(&self.cfg, target_name) {
            Ok(target) => target,
            Err(why) => {
                self.note(json!({
                    "event": "result", "at": self.now(), "move": id, "project": project, "name": name,
                    "result": "failed", "downtimeMs": null, "reason": why,
                }));
                return false;
            }
        };
        let source = MoveWriter::new(self.cfg.incus.socket.clone());
        let dest = MoveWriter::new(target.socket.clone());
        let mut stopped = false;
        let outcome = self.attempt(id, plan, one, &source, &dest, was_running, &mut stopped);
        self.clear_live(id, &project, &name);
        match outcome {
            Ok(downtime) => {
                self.note(json!({
                    "event": "result", "at": self.now(), "move": id, "project": project, "name": name,
                    "result": "moved", "downtimeMs": downtime, "reason": null,
                }));
                true
            }
            Err(reason) => {
                self.set_live(id, &project, &name, |progress| {
                    progress.phase = "rolling_back".to_string();
                });
                let rollback = rollback(&source, &dest, one, was_running);
                self.clear_live(id, &project, &name);
                self.note(json!({
                    "event": "rollback", "at": self.now(), "move": id, "project": project, "name": name,
                    "reason": reason, "targetRemoved": rollback.target_removed,
                    "sourceRunning": rollback.source_running, "detail": rollback.detail,
                }));
                self.note(json!({
                    "event": "result", "at": self.now(), "move": id, "project": project, "name": name,
                    "result": "rolled_back", "downtimeMs": null, "reason": reason,
                }));
                false
            }
        }
    }

    /// The rounds, the cutover and the checks. Answers the measured downtime in milliseconds, or why it failed.
    #[allow(clippy::too_many_arguments)]
    fn attempt(
        &self,
        id: &str,
        plan: &Value,
        one: &Value,
        source: &MoveWriter,
        dest: &MoveWriter,
        was_running: bool,
        stopped: &mut bool,
    ) -> Result<u64, String> {
        let project = one["project"].as_str().unwrap_or_default();
        let name = one["name"].as_str().unwrap_or_default();
        let pool = one["targetPool"].as_str().unwrap_or_default();
        let pre = one["rounds"]["preCopy"].as_u64().unwrap_or(DEFAULT_ROUNDS);
        let total_rounds = pre + 1;
        let target_name = plan["target"]["name"].as_str().unwrap_or("the target");
        let server = dest
            .get("/1.0")
            .map_err(|error| format!("{target_name}'s Incus could not be read: {error}"))?;
        let (address, certificate) = transport_facts(&server);
        let (Some(address), Some(certificate)) = (address, certificate) else {
            return Err(format!(
                "{target_name}'s Incus no longer listens on the network, so this host cannot push to it"
            ));
        };
        if Some(Digest::of(certificate.as_bytes()).to_hex().as_str())
            != plan["transport"]["certificateFingerprint"].as_str()
        {
            return Err(format!(
                "{target_name}'s Incus certificate is not the one the plan named"
            ));
        }
        let needed = one["bytes"]["needed"].as_u64().unwrap_or(0);
        let headroom = needed.saturating_sub(one["bytes"]["measured"].as_u64().unwrap_or(0));
        let instance_path = format!("/1.0/instances/{}?project={}", seg(name), seg(project));
        let addressed = nics_with_addresses(source, project, name);
        for round in 1..=total_rounds {
            let last = round == total_rounds;
            if last {
                // The cutover: the source stops here, and the downtime runs from now.
                let began = Instant::now();
                self.set_live(id, project, name, |progress| {
                    progress.phase = "cutover".to_string();
                    progress.round = round;
                });
                if was_running {
                    stop(source, project, name)?;
                }
                *stopped = true;
                self.note(json!({
                    "event": "cutover", "at": self.now(), "move": id, "project": project, "name": name,
                    "step": "stopped", "wasRunning": was_running,
                }));
                self.check_room(
                    dest,
                    pool,
                    if round == 1 { needed } else { headroom },
                    target_name,
                )?;
                self.copy_round(
                    id,
                    one,
                    source,
                    dest,
                    &address,
                    &certificate,
                    round,
                    total_rounds,
                )?;
                dest.set_state(project, name, "start", false, START_DEADLINE)
                    .map_err(|error| format!("the copy on {target_name} did not start: {error}"))?;
                self.note(json!({
                    "event": "cutover", "at": self.now(), "move": id, "project": project, "name": name,
                    "step": "started", "elapsedMs": began.elapsed().as_millis() as u64,
                }));
                self.set_live(id, project, name, |progress| {
                    progress.phase = "verifying".to_string()
                });
                let checks = self.verify(source, dest, one, &address, &addressed);
                let ok = checks.iter().all(|check| check["ok"] == true);
                let downtime = began.elapsed().as_millis() as u64;
                self.note(json!({
                    "event": "verify", "at": self.now(), "move": id, "project": project, "name": name,
                    "ok": ok, "checks": checks, "downtimeMs": if ok { json!(downtime) } else { Value::Null },
                }));
                if !ok {
                    let failing: Vec<String> = checks
                        .iter()
                        .filter(|check| check["ok"] == false)
                        .map(|check| {
                            format!(
                                "{}: {}",
                                check["check"].as_str().unwrap_or("?"),
                                check["detail"].as_str().unwrap_or("not ok")
                            )
                        })
                        .collect();
                    return Err(format!(
                        "the copy on {target_name} did not pass its checks: {}",
                        failing.join("; ")
                    ));
                }
                return Ok(downtime);
            }
            self.check_room(
                dest,
                pool,
                if round == 1 { needed } else { headroom },
                target_name,
            )?;
            self.copy_round(
                id,
                one,
                source,
                dest,
                &address,
                &certificate,
                round,
                total_rounds,
            )?;
            if round == 1 {
                let uuid = dest
                    .get(&instance_path)
                    .ok()
                    .and_then(|doc| doc.pointer("/config/volatile.uuid").cloned());
                self.note(json!({
                    "event": "created", "at": self.now(), "move": id, "project": project, "name": name,
                    "targetUuid": uuid,
                }));
            }
        }
        Err("the move ended without a cutover".to_string())
    }

    /// Refuses a round when the target pool has less room than it may take.
    fn check_room(
        &self,
        dest: &MoveWriter,
        pool: &str,
        needed: u64,
        target: &str,
    ) -> Result<(), String> {
        let free = pool_free(dest, pool)
            .ok_or_else(|| format!("the free space of {target}'s pool {pool} could not be read"))?;
        if needed > free {
            return Err(format!(
                "{target}'s pool {pool} has {} free, less than the {} this round may take: the target filled up during the move",
                human(free),
                human(needed)
            ));
        }
        Ok(())
    }

    /// One round: each custom volume, then the instance, pushed by the source's Incus to the target's. Journals the
    /// round with what it moved, its rate, how long it took and the time left.
    #[allow(clippy::too_many_arguments)]
    fn copy_round(
        &self,
        id: &str,
        one: &Value,
        source: &MoveWriter,
        dest: &MoveWriter,
        address: &str,
        certificate: &str,
        round: u64,
        of: u64,
    ) -> Result<(), String> {
        let project = one["project"].as_str().unwrap_or_default();
        let name = one["name"].as_str().unwrap_or_default();
        let refresh = round > 1;
        let started = Instant::now();
        self.set_live(id, project, name, |progress| {
            progress.round = round;
            progress.done = 0;
            progress.processed = None;
            progress.speed = None;
            if progress.phase != "cutover" {
                progress.phase = "copying".to_string();
            }
        });
        let mut moved = 0u64;
        for volume in one["volumes"].as_array().into_iter().flatten() {
            let pool = volume["pool"].as_str().unwrap_or_default();
            let volume_name = volume["name"].as_str().unwrap_or_default();
            let volume_project = volume["project"].as_str().unwrap_or("default");
            let what = format!("the custom volume {pool}/{volume_name}");
            let doc = source
                .get(&format!(
                    "/1.0/storage-pools/{}/volumes/custom/{}?project={}",
                    seg(pool),
                    seg(volume_name),
                    seg(volume_project)
                ))
                .map_err(|error| format!("{what} could not be read here: {error}"))?;
            let receive = dest
                .receive_volume(volume_project, pool, volume_name, &doc, refresh)
                .map_err(|error| format!("the target did not take {what}: {error}"))?;
            let to = PushTarget {
                address,
                certificate,
                receive: &receive,
            };
            let push = source
                .push_volume(volume_project, pool, volume_name, &to)
                .map_err(|error| format!("this host did not push {what}: {error}"))?;
            moved += self.follow(
                id,
                project,
                name,
                source,
                dest,
                &push,
                &receive.operation,
                &what,
            )?;
            self.set_live(id, project, name, |progress| {
                progress.done = moved;
                progress.processed = None;
            });
        }
        let what = format!("the container {project}/{name}");
        let doc = source
            .get(&format!(
                "/1.0/instances/{}?project={}",
                seg(name),
                seg(project)
            ))
            .map_err(|error| format!("{what} could not be read here: {error}"))?;
        let receive = dest
            .receive_instance(project, name, &doc, refresh)
            .map_err(|error| format!("the target did not take {what}: {error}"))?;
        let to = PushTarget {
            address,
            certificate,
            receive: &receive,
        };
        let push = source
            .push_instance(project, name, &to)
            .map_err(|error| format!("this host did not push {what}: {error}"))?;
        moved += self.follow(
            id,
            project,
            name,
            source,
            dest,
            &push,
            &receive.operation,
            &what,
        )?;
        let elapsed = started.elapsed().as_millis() as u64;
        let rate = (elapsed > 0).then(|| moved.saturating_mul(1000) / elapsed.max(1));
        self.note(json!({
            "event": "round", "at": self.now(), "move": id, "project": project, "name": name,
            "round": round, "of": of, "refresh": refresh, "final": round == of,
            "bytes": moved, "elapsedMs": elapsed, "rateBytesPerSecond": rate,
            "timeLeftMs": if round == of { json!(0) } else { Value::Null },
        }));
        Ok(())
    }

    /// Follows one push to its end, keeping its progress readable, and then the target's receiving side. Answers the
    /// bytes Incus says it moved (0 when it did not say).
    #[allow(clippy::too_many_arguments)]
    fn follow(
        &self,
        id: &str,
        project: &str,
        name: &str,
        source: &MoveWriter,
        dest: &MoveWriter,
        push: &str,
        receiving: &str,
        what: &str,
    ) -> Result<u64, String> {
        let until = Instant::now() + ROUND_DEADLINE;
        let mut bytes = 0u64;
        loop {
            match source.wait(push, self.poll) {
                Ok(OpState::Running(operation)) => {
                    let (processed, speed) = progress_of(&operation);
                    if let Some(processed) = processed {
                        bytes = processed;
                    }
                    self.set_live(id, project, name, |progress| {
                        progress.processed = processed.or(progress.processed);
                        progress.speed = speed.or(progress.speed);
                    });
                }
                Ok(OpState::Done(operation)) => {
                    if let (Some(processed), _) = progress_of(&operation) {
                        bytes = processed;
                    }
                    break;
                }
                Ok(OpState::Gone) => break,
                Ok(OpState::Failed(message)) => {
                    return Err(format!("the push of {what} failed: {message}"));
                }
                Err(error) => {
                    return Err(format!("the push of {what} could not be followed: {error}"));
                }
            }
            if Instant::now() >= until {
                return Err(format!("the push of {what} did not end in a day"));
            }
            std::thread::sleep(self.poll.min(Duration::from_millis(500)));
        }
        loop {
            match dest.wait(receiving, self.poll) {
                Ok(OpState::Done(_) | OpState::Gone) => return Ok(bytes),
                Ok(OpState::Running(_)) if Instant::now() < until => {}
                Ok(OpState::Running(_)) => {
                    return Err(format!("the target did not finish taking {what}"));
                }
                Ok(OpState::Failed(message)) => {
                    return Err(format!("the target failed taking {what}: {message}"));
                }
                Err(error) => {
                    return Err(format!(
                        "the target's side of {what} could not be followed: {error}"
                    ));
                }
            }
        }
    }

    /// The checks of a moved copy. Each is `{check, ok, detail}`.
    fn verify(
        &self,
        source: &MoveWriter,
        dest: &MoveWriter,
        one: &Value,
        address: &str,
        addressed: &[String],
    ) -> Vec<Value> {
        let project = one["project"].as_str().unwrap_or_default();
        let name = one["name"].as_str().unwrap_or_default();
        let until = Instant::now() + self.verify_wait;
        let mut checks = Vec::new();
        let state_path = format!(
            "/1.0/instances/{}/state?project={}",
            seg(name),
            seg(project)
        );
        let instance_path = format!("/1.0/instances/{}?project={}", seg(name), seg(project));
        // Running now; the check is written last, when the copy is read again after the others.
        let state = dest.get(&state_path).ok();
        let first = state
            .as_ref()
            .and_then(|state| text(state, "status"))
            .unwrap_or("not readable")
            .to_string();
        // Each card that had an address has one again.
        let mut missing: Vec<String> = addressed.to_vec();
        loop {
            if let Ok(state) = dest.get(&state_path) {
                missing.retain(|card| !has_global_address(&state, card));
            }
            if missing.is_empty() || Instant::now() >= until {
                break;
            }
            std::thread::sleep(self.poll.min(Duration::from_millis(500)));
        }
        checks.push(json!({
            "check": "network", "ok": missing.is_empty(),
            "detail": if addressed.is_empty() {
                "no network card had an address on the source; nothing to compare".to_string()
            } else if missing.is_empty() {
                format!("{} has an address again", addressed.join(", "))
            } else {
                format!("no address on {} after {} s", missing.join(", "), self.verify_wait.as_secs())
            },
        }));
        // Ports: each TCP proxy of the copy answers on the target host.
        let doc = dest.get(&instance_path).ok();
        let host = address
            .rsplit_once(':')
            .map(|(host, _)| {
                host.trim_start_matches('[')
                    .trim_end_matches(']')
                    .to_string()
            })
            .unwrap_or_default();
        let ports: Vec<u16> = doc
            .as_ref()
            .map(devices)
            .unwrap_or_default()
            .iter()
            .filter(|(_, device)| text(device, "type") == Some("proxy"))
            .filter_map(|(_, device)| {
                let listen = text(device, "listen")?;
                let rest = listen.strip_prefix("tcp:")?;
                rest.rsplit_once(':')?.1.parse().ok()
            })
            .collect();
        let mut closed = Vec::new();
        for port in &ports {
            let mut open = false;
            loop {
                if let Ok(mut found) = (host.as_str(), *port).to_socket_addrs() {
                    open = found.any(|address: SocketAddr| {
                        TcpStream::connect_timeout(&address, Duration::from_secs(3)).is_ok()
                    });
                }
                if open || Instant::now() >= until {
                    break;
                }
                std::thread::sleep(self.poll.min(Duration::from_millis(500)));
            }
            if !open {
                closed.push(port.to_string());
            }
        }
        checks.push(json!({
            "check": "ports", "ok": closed.is_empty(),
            "detail": if ports.is_empty() {
                "the copy has no TCP proxy to probe".to_string()
            } else if closed.is_empty() {
                format!("port(s) {} answer on {host}", ports.iter().map(u16::to_string).collect::<Vec<_>>().join(", "))
            } else {
                format!("port(s) {} do not answer on {host}", closed.join(", "))
            },
        }));
        // A container has no guest agent.
        checks.push(json!({
            "check": "guestAgent", "ok": true,
            "detail": "a container has no guest agent: not applicable",
        }));
        // The custom volumes are there and attached.
        let mut detached = Vec::new();
        for volume in one["volumes"].as_array().into_iter().flatten() {
            let pool = volume["pool"].as_str().unwrap_or_default();
            let volume_name = volume["name"].as_str().unwrap_or_default();
            let there = dest
                .get(&format!(
                    "/1.0/storage-pools/{}/volumes/custom/{}?project={}",
                    seg(pool),
                    seg(volume_name),
                    seg(volume["project"].as_str().unwrap_or("default"))
                ))
                .is_ok();
            let attached = doc.as_ref().is_some_and(|doc| {
                devices(doc).iter().any(|(_, device)| {
                    text(device, "type") == Some("disk")
                        && text(device, "pool") == Some(pool)
                        && text(device, "source") == Some(volume_name)
                })
            });
            if !there || !attached {
                detached.push(format!("{pool}/{volume_name}"));
            }
        }
        let volumes = one["volumes"].as_array().map_or(0, Vec::len);
        checks.push(json!({
            "check": "volumes", "ok": detached.is_empty(),
            "detail": if volumes == 0 {
                "the container has no custom volume".to_string()
            } else if detached.is_empty() {
                format!("{volumes} custom volume(s) on the target and attached")
            } else {
                format!("not there or not attached: {}", detached.join(", "))
            },
        }));
        // The spot-check file is the same on both sides.
        let path = one["spotCheck"].as_str().unwrap_or(DEFAULT_SPOT_CHECK);
        let (ok, detail) = match source.read_file(project, name, path) {
            Err(error) => (
                true,
                format!("{path} could not be read on the source ({error}); nothing to compare"),
            ),
            Ok(here) => match dest.read_file(project, name, path) {
                Ok(there) if there == here => (
                    true,
                    format!(
                        "{path} is the same on both ({})",
                        &Digest::of(&here).to_hex()[..12]
                    ),
                ),
                Ok(_) => (
                    false,
                    format!("{path} differs between the source and the copy"),
                ),
                Err(error) => (
                    false,
                    format!("{path} could not be read on the copy: {error}"),
                ),
            },
        };
        checks.push(json!({ "check": "spotCheck", "ok": ok, "detail": detail }));
        // Still running after the checks: a program that exits right after its start fails here.
        let last = dest
            .get(&instance_path)
            .ok()
            .and_then(|doc| text(&doc, "status").map(str::to_string))
            .unwrap_or_else(|| "not readable".to_string());
        let running = first == "Running" && last == "Running";
        checks.insert(
            0,
            json!({
                "check": "running", "ok": running,
                "detail": if running {
                    "the copy runs, and still runs after its checks".to_string()
                } else if first != "Running" {
                    format!("the copy is {first}")
                } else {
                    format!("the copy is {last} after its checks")
                },
            }),
        );
        checks
    }

    /// Rolls back the moves a stopped kernel left unfinished: each instance without a result, as a failure before its
    /// checks would be. Runs in the background, once, when the node starts.
    pub fn recover(self: &Arc<Self>) {
        let lines = self.journal.lines();
        let records = fold(&lines);
        let unfinished: Vec<MoveRecord> = records
            .into_values()
            .filter(|record| record.ended_at.is_none())
            .collect();
        if unfinished.is_empty() {
            return;
        }
        let moves = Arc::clone(self);
        let _ = std::thread::Builder::new()
            .name("arca-move-recover".to_string())
            .spawn(move || {
                for record in unfinished {
                    moves.recover_one(&record);
                }
            });
    }

    fn recover_one(&self, record: &MoveRecord) {
        let reason =
            "the kernel stopped during the move; the next start rolled it back".to_string();
        let target = linked_target(&self.cfg, &record.target);
        let source = MoveWriter::new(self.cfg.incus.socket.clone());
        for instance in record.instances.iter().filter(|one| one.result.is_none()) {
            let one = json!({
                "project": instance.project, "name": instance.name, "volumes": instance.volumes,
            });
            let outcome = match &target {
                Ok(target) => rollback(
                    &source,
                    &MoveWriter::new(target.socket.clone()),
                    &one,
                    instance.was_running,
                ),
                Err(why) => RolledBack {
                    target_removed: false,
                    source_running: start_if(
                        &source,
                        &instance.project,
                        &instance.name,
                        instance.was_running,
                    ),
                    detail: vec![format!("the target was not reached: {why}")],
                },
            };
            self.note(json!({
                "event": "rollback", "at": self.now(), "move": record.id, "project": instance.project,
                "name": instance.name, "reason": reason, "targetRemoved": outcome.target_removed,
                "sourceRunning": outcome.source_running, "detail": outcome.detail, "interrupted": true,
            }));
            self.note(json!({
                "event": "result", "at": self.now(), "move": record.id, "project": instance.project,
                "name": instance.name, "result": "rolled_back", "downtimeMs": null, "reason": reason,
            }));
        }
        let moved = record
            .instances
            .iter()
            .filter(|one| one.result.as_deref() == Some("moved"))
            .count();
        self.note(json!({
            "event": "end", "at": self.now(), "move": record.id, "moved": moved,
            "of": record.instances.len(), "interrupted": true,
        }));
    }

    /// The status of one move, or of the recent ones.
    pub fn status(&self, id: Option<&str>) -> Vec<Value> {
        let records = fold(&self.journal.lines());
        let now = self.now();
        let running = self.running.lock().map(|r| r.clone()).unwrap_or_default();
        let live = self
            .live
            .lock()
            .map(|live| live.clone())
            .unwrap_or_default();
        let mut list: Vec<&MoveRecord> = match id {
            Some(id) => records.get(id).into_iter().collect(),
            None => records.values().collect(),
        };
        list.sort_by_key(|record| std::cmp::Reverse(record.started_at));
        list.truncate(20);
        list.into_iter()
            .map(|record| record.json(now, running.contains(&record.id), &live))
            .collect()
    }
}

/// The free bytes of a pool, as its resources say.
fn pool_free(writer: &MoveWriter, pool: &str) -> Option<u64> {
    let space = writer
        .get(&format!("/1.0/storage-pools/{}/resources", seg(pool)))
        .ok()?;
    let total = space.pointer("/space/total")?.as_u64()?;
    let used = space.pointer("/space/used")?.as_u64()?;
    Some(total.saturating_sub(used))
}

/// The network cards of a running instance that hold a global address now.
fn nics_with_addresses(source: &MoveWriter, project: &str, name: &str) -> Vec<String> {
    let Ok(state) = source.get(&format!(
        "/1.0/instances/{}/state?project={}",
        seg(name),
        seg(project)
    )) else {
        return Vec::new();
    };
    let mut out: Vec<String> = state
        .get("network")
        .and_then(Value::as_object)
        .map(|cards| {
            cards
                .keys()
                .filter(|card| card.as_str() != "lo" && has_global_address(&state, card))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

fn has_global_address(state: &Value, card: &str) -> bool {
    state["network"][card]["addresses"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|address| {
            address["scope"] == "global"
                && address["address"].as_str().is_some_and(|a| !a.is_empty())
        })
}

/// Stops the source gracefully, then by force.
fn stop(source: &MoveWriter, project: &str, name: &str) -> Result<(), String> {
    if source
        .set_state(project, name, "stop", false, STOP_GRACE)
        .is_ok()
    {
        return Ok(());
    }
    source
        .set_state(project, name, "stop", true, STOP_GRACE)
        .map_err(|error| format!("the source did not stop: {error}"))
}

/// Starts the source again when it ran before the move, and answers whether it runs.
fn start_if(source: &MoveWriter, project: &str, name: &str, was_running: bool) -> bool {
    let path = format!("/1.0/instances/{}?project={}", seg(name), seg(project));
    let status = |source: &MoveWriter| {
        source.get(&path).ok().and_then(|doc| {
            doc.get("status")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
    };
    if status(source).as_deref() == Some("Running") {
        return true;
    }
    if !was_running {
        return false;
    }
    let _ = source.set_state(project, name, "start", false, START_DEADLINE);
    status(source).as_deref() == Some("Running")
}

/// What a rollback did.
struct RolledBack {
    target_removed: bool,
    source_running: bool,
    detail: Vec<String>,
}

/// Rolls one instance back: the copy on the target is stopped and deleted with the custom volumes the move made there,
/// and the source is started again when it ran. The source's own volumes are never touched. The plan made sure the
/// target held none of these names before the move, so what is there now is the move's own.
fn rollback(source: &MoveWriter, dest: &MoveWriter, one: &Value, was_running: bool) -> RolledBack {
    let project = one["project"].as_str().unwrap_or_default();
    let name = one["name"].as_str().unwrap_or_default();
    let mut detail = Vec::new();
    let path = format!("/1.0/instances/{}?project={}", seg(name), seg(project));
    let target_removed = match dest.get(&path) {
        Err(IncusError::Status { code: 404, .. }) => {
            detail.push("no copy was left on the target".to_string());
            true
        }
        Err(error) => {
            detail.push(format!("the target could not be read: {error}"));
            false
        }
        Ok(doc) => {
            if doc.get("status") == Some(&json!("Running")) {
                let _ = dest.set_state(project, name, "stop", true, STOP_GRACE);
            }
            match dest.delete_instance(project, name) {
                Ok(()) => {
                    detail.push("the copy on the target was deleted".to_string());
                    true
                }
                Err(error) => {
                    detail.push(format!(
                        "the copy on the target could not be deleted: {error}"
                    ));
                    false
                }
            }
        }
    };
    for volume in one["volumes"].as_array().into_iter().flatten() {
        let pool = volume["pool"].as_str().unwrap_or_default();
        let volume_name = volume["name"].as_str().unwrap_or_default();
        let volume_project = volume["project"].as_str().unwrap_or("default");
        let found = dest.get(&format!(
            "/1.0/storage-pools/{}/volumes/custom/{}?project={}",
            seg(pool),
            seg(volume_name),
            seg(volume_project)
        ));
        if found.is_ok() {
            match dest.delete_volume(volume_project, pool, volume_name) {
                Ok(()) => detail.push(format!(
                    "the copy of the volume {pool}/{volume_name} was deleted"
                )),
                Err(error) => detail.push(format!(
                    "the copy of the volume {pool}/{volume_name} could not be deleted: {error}"
                )),
            }
        }
    }
    let source_running = start_if(source, project, name, was_running);
    detail.push(if source_running {
        "the source runs".to_string()
    } else if was_running {
        "the source did not start again".to_string()
    } else {
        "the source was stopped before the move and stays stopped".to_string()
    });
    RolledBack {
        target_removed,
        source_running,
        detail,
    }
}

/// One move as its journal lines tell it.
#[derive(Debug, Clone, Default)]
struct MoveRecord {
    id: String,
    plan: String,
    target: String,
    started_at: u64,
    ended_at: Option<u64>,
    interrupted: bool,
    finished: Option<Value>,
    instances: Vec<InstanceRecord>,
}

#[derive(Debug, Clone, Default)]
struct InstanceRecord {
    project: String,
    name: String,
    was_running: bool,
    rounds_total: u64,
    rounds_done: u64,
    bytes_total: Option<u64>,
    bytes_moved: u64,
    last_rate: Option<u64>,
    needed: u64,
    target_pool: String,
    volumes: Value,
    phase: String,
    result: Option<String>,
    downtime: Option<u64>,
    reason: Option<String>,
    checks: Vec<Value>,
    ended_at: Option<u64>,
}

/// Folds the journal into its moves, by id.
fn fold(lines: &[Value]) -> BTreeMap<String, MoveRecord> {
    let mut out: BTreeMap<String, MoveRecord> = BTreeMap::new();
    for line in lines {
        let Some(id) = line["move"].as_str() else {
            continue;
        };
        let at = line["at"].as_u64().unwrap_or(0);
        if line["event"] == "begin" {
            let instances = line["instances"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|one| InstanceRecord {
                    project: one["project"].as_str().unwrap_or_default().to_string(),
                    name: one["name"].as_str().unwrap_or_default().to_string(),
                    was_running: one["wasRunning"] == true,
                    rounds_total: one["roundsTotal"].as_u64().unwrap_or(0),
                    bytes_total: one["bytesTotal"].as_u64(),
                    needed: one["needed"].as_u64().unwrap_or(0),
                    target_pool: one["targetPool"].as_str().unwrap_or_default().to_string(),
                    volumes: one["volumes"].clone(),
                    phase: "waiting".to_string(),
                    ..InstanceRecord::default()
                })
                .collect();
            out.insert(
                id.to_string(),
                MoveRecord {
                    id: id.to_string(),
                    plan: line["plan"].as_str().unwrap_or_default().to_string(),
                    target: line["target"].as_str().unwrap_or_default().to_string(),
                    started_at: at,
                    instances,
                    ..MoveRecord::default()
                },
            );
            continue;
        }
        let Some(record) = out.get_mut(id) else {
            continue;
        };
        match line["event"].as_str() {
            Some("end") => {
                record.ended_at = Some(at);
                record.interrupted |= line["interrupted"] == true;
            }
            Some("finish") => record.finished = Some(line.clone()),
            _ => {}
        }
        let Some(instance) = record.instances.iter_mut().find(|one| {
            line["project"].as_str() == Some(one.project.as_str())
                && line["name"].as_str() == Some(one.name.as_str())
        }) else {
            continue;
        };
        match line["event"].as_str() {
            Some("round") => {
                instance.rounds_done += 1;
                instance.bytes_moved += line["bytes"].as_u64().unwrap_or(0);
                instance.last_rate = line["rateBytesPerSecond"].as_u64().or(instance.last_rate);
                instance.phase = "copying".to_string();
            }
            Some("cutover") => instance.phase = "cutover".to_string(),
            Some("verify") => {
                instance.checks = line["checks"].as_array().cloned().unwrap_or_default();
                instance.phase = "verifying".to_string();
            }
            Some("rollback") => {
                instance.phase = "rolling_back".to_string();
                record.interrupted |= line["interrupted"] == true;
            }
            Some("result") => {
                instance.result = line["result"].as_str().map(str::to_string);
                instance.phase = instance.result.clone().unwrap_or_default();
                instance.downtime = line["downtimeMs"].as_u64();
                instance.reason = line["reason"].as_str().map(str::to_string);
                instance.ended_at = Some(at);
            }
            _ => {}
        }
    }
    out
}

impl MoveRecord {
    /// What finishing this move would delete, and whether it may be finished now.
    fn finish_view(&self) -> (bool, String, Vec<String>) {
        if self.finished.is_some() {
            return (false, "the move was finished".to_string(), Vec::new());
        }
        let moved: Vec<&InstanceRecord> = self
            .instances
            .iter()
            .filter(|one| one.result.as_deref() == Some("moved"))
            .collect();
        if self.ended_at.is_none() {
            return (
                false,
                "the move still runs; it is offered only after its checks pass".to_string(),
                Vec::new(),
            );
        }
        if moved.is_empty() {
            return (
                false,
                "no instance of this move passed its checks, so there is nothing to finish"
                    .to_string(),
                Vec::new(),
            );
        }
        let mut deletes = Vec::new();
        for one in &moved {
            deletes.push(format!(
                "the source instance {}/{} on this host",
                one.project, one.name
            ));
            for volume in one.volumes.as_array().into_iter().flatten() {
                deletes.push(format!(
                    "its custom volume {}/{} (project {}) on this host",
                    volume["pool"].as_str().unwrap_or("?"),
                    volume["name"].as_str().unwrap_or("?"),
                    volume["project"].as_str().unwrap_or("?")
                ));
            }
        }
        (
            true,
            "the moved copies passed their checks: the owner may finish the move with a card"
                .to_string(),
            deletes,
        )
    }

    fn json(
        &self,
        now: u64,
        running: bool,
        live: &BTreeMap<(String, String, String), Progress>,
    ) -> Value {
        let (available, why, deletes) = self.finish_view();
        let phase = if self.finished.is_some() {
            "finished"
        } else if self.ended_at.is_some() {
            if self
                .instances
                .iter()
                .all(|one| one.result.as_deref() == Some("moved"))
            {
                "moved"
            } else if self
                .instances
                .iter()
                .any(|one| one.result.as_deref() == Some("moved"))
            {
                "partly_moved"
            } else {
                "rolled_back"
            }
        } else if running {
            "running"
        } else {
            "interrupted"
        };
        let instances: Vec<Value> = self
            .instances
            .iter()
            .map(|one| {
                let progress = live.get(&(self.id.clone(), one.project.clone(), one.name.clone()));
                let current = progress.map(|progress| progress.done + progress.processed.unwrap_or(0));
                let bytes_moved = match (progress, one.rounds_done) {
                    (Some(p), 0) if p.processed.is_none() && p.done == 0 => None,
                    _ => Some(one.bytes_moved + current.unwrap_or(0)),
                };
                let rate = progress.and_then(|p| p.speed).or(one.last_rate);
                let time_left = if one.result.is_some() {
                    Some(0)
                } else {
                    match (progress, rate, one.bytes_total) {
                        (Some(p), Some(rate), Some(total)) if p.round == 1 && rate > 0 => {
                            Some(total.saturating_sub(current.unwrap_or(0)).saturating_mul(1000) / rate)
                        }
                        _ => None,
                    }
                };
                let phase = progress
                    .map(|p| p.phase.clone())
                    .filter(|phase| !phase.is_empty())
                    .unwrap_or_else(|| one.phase.clone());
                let finished = self.finished.as_ref().is_some_and(|line| {
                    line["deleted"].as_array().into_iter().flatten().any(|deleted| {
                        deleted.as_str() == Some(format!("{}/{}", one.project, one.name).as_str())
                    })
                });
                json!({
                    "project": one.project,
                    "name": one.name,
                    "phase": if finished { "finished".to_string() } else { phase },
                    "result": if finished { Some("finished".to_string()) } else { one.result.clone() },
                    "roundsDone": one.rounds_done,
                    "roundsTotal": one.rounds_total,
                    "bytesMoved": bytes_moved,
                    "bytesTotal": one.bytes_total,
                    "rateBytesPerSecond": rate,
                    "elapsedMs": one.ended_at.unwrap_or(now).saturating_sub(self.started_at),
                    "timeLeftMs": time_left,
                    "downtimeMs": one.downtime,
                    "reason": one.reason,
                    "checks": one.checks,
                })
            })
            .collect();
        json!({
            "move": self.id,
            "plan": self.plan,
            "target": self.target,
            "phase": phase,
            "startedAt": self.started_at,
            "endedAt": self.ended_at,
            "interrupted": self.interrupted || (!running && self.ended_at.is_none()),
            "instances": instances,
            "finish": { "available": available, "reason": why, "deletes": deletes },
        })
    }
}

/// `instance_move_plan`.
pub struct MovePlanHandler {
    pub moves: Arc<Moves>,
}

impl ToolHandler for MovePlanHandler {
    fn call(&self, args: Value, _caller: &Caller) -> Result<Value, CallError> {
        let object = args
            .as_object()
            .ok_or_else(|| invalid("arguments: expected an object"))?;
        let allowed = ["instances", "target", "rounds"];
        if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
            return Err(invalid(arca_api::unknown_field("", key, &allowed)));
        }
        let wanted = parse_refs(args.get("instances").unwrap_or(&Value::Null), true)?;
        let target = target_arg(&args)?;
        let rounds = match args.get("rounds") {
            None => DEFAULT_ROUNDS,
            Some(value) => value
                .as_u64()
                .filter(|rounds| (1..=5).contains(rounds))
                .ok_or_else(|| invalid("rounds: expected 1 to 5"))?,
        };
        let planned = self.moves.compute(&wanted, &target, rounds)?;
        let (digest, expires) = match &planned.plan {
            Some(plan) => {
                self.moves
                    .note(json!({ "event": "plan", "at": self.moves.now(), "plan": plan }));
                (plan["digest"].clone(), plan["expiresAt"].clone())
            }
            None => (Value::Null, Value::Null),
        };
        let message = if planned.go {
            format!(
                "planned {} container(s) to {target}: propose it with action_propose instance_move and the digest; the plan expires in an hour",
                wanted.len()
            )
        } else {
            format!(
                "not planned: {} of {} instance(s) cannot move to {target} as they stand",
                planned.blocked.len(),
                wanted.len()
            )
        };
        Ok(json!({
            "go": planned.go,
            "digest": digest,
            "expiresAt": expires,
            "target": planned.target,
            "plan": planned.plan,
            "blocked": planned.blocked,
            "preflight": planned.preflight,
            "message": message,
        }))
    }
}

/// The approved plan the arguments of an `instance_move` name, checked against them: it is there, it has not expired,
/// and its instances and target are the arguments'. Answers the plan.
fn approved_plan(journal: &Journal, args: &Value, now: u64) -> Result<Value, CallError> {
    let digest = args["plan"].as_str().unwrap_or_default();
    let plan = journal.plan(digest).ok_or_else(|| {
        refused(
            "NoSuchPlan",
            "no move plan has this digest on this node: plan the move with instance_move_plan first",
        )
    })?;
    if plan["expiresAt"].as_u64().unwrap_or(0) < now {
        return Err(refused(
            "PlanExpired",
            "the move plan has expired: plan the move again",
        ));
    }
    if plan["target"]["name"] != args["target"]
        || Value::Array(Moves::refs_of(&plan)) != args["instances"]
    {
        return Err(refused(
            "PlanMismatch",
            "the instances or the target are not the plan's: name the plan's own, in its order",
        ));
    }
    Ok(plan)
}

/// What `action_propose` checks and shows for a move's card or a finish's: the plan or the move must be there and fit,
/// and the card's rows are read from it. Answers the fact the card is built from.
pub fn check_proposal(
    cfg: &NodeConfig,
    action: ActionKind,
    args: &Value,
    now: u64,
) -> Result<String, CallError> {
    let journal = Journal::at(&cfg.state_dir);
    let rows = match action {
        ActionKind::InstanceMove => {
            let plan = approved_plan(&journal, args, now)?;
            if journal
                .lines()
                .iter()
                .any(|line| line["event"] == "begin" && line["plan"] == plan["digest"])
            {
                return Err(refused(
                    "AlreadyRun",
                    "this plan already ran: plan the move again for a new card",
                ));
            }
            Moves::card_rows(&plan)
        }
        _ => {
            let id = args["move"].as_str().unwrap_or_default();
            let records = fold(&journal.lines());
            let record = records
                .get(id)
                .ok_or_else(|| refused("NoSuchMove", format!("no move {id} on this node")))?;
            let (available, why, deletes) = record.finish_view();
            if !available {
                return Err(refused(
                    "NotVerified",
                    format!("move {id} cannot be finished: {why}"),
                ));
            }
            let mut rows: Vec<(String, String)> = deletes
                .into_iter()
                .map(|what| ("Delete".to_string(), what))
                .collect();
            rows.push((
                "Moved to".to_string(),
                format!("{}, where each copy passed its checks", record.target),
            ));
            rows
        }
    };
    Ok(serde_json::to_string(&json!({ "rows": rows })).unwrap_or_default())
}

/// The rows a card fact carries, as [`check_proposal`] wrote them.
pub fn fact_rows(fact: Option<&str>) -> Option<Vec<(String, String)>> {
    let value: Value = serde_json::from_str(fact?).ok()?;
    value["rows"]
        .as_array()?
        .iter()
        .map(|row| {
            let pair = row.as_array()?;
            Some((
                pair.first()?.as_str()?.to_string(),
                pair.get(1)?.as_str()?.to_string(),
            ))
        })
        .collect()
}

fn take_grant(args: Value) -> Result<(Value, String), CallError> {
    let mut bare = args;
    let grant = bare
        .as_object_mut()
        .and_then(|object| object.remove("approval"))
        .and_then(|value| value.as_str().map(str::to_string))
        .ok_or_else(|| invalid("approval is required"))?;
    Ok((bare, grant))
}

fn spend(
    kernel: &Mutex<Kernel>,
    verb: &str,
    args: &Value,
    grant: &str,
) -> Result<Option<String>, CallError> {
    let mut kernel = kernel.lock().map_err(|_| internal())?;
    let proposal = kernel.action_grant_id(grant);
    if !kernel.consume_action_grant(verb, args, grant) {
        return Err(refused(
            "NotApproved",
            "this action runs only from an approved proposal: propose it with action_propose, and it runs once the owner approves it",
        ));
    }
    Ok(proposal)
}

/// `instance_move`: runs only from an approved card, once.
pub struct MoveHandler {
    pub moves: Arc<Moves>,
}

impl ToolHandler for MoveHandler {
    fn call(&self, args: Value, caller: &Caller) -> Result<Value, CallError> {
        let verb = ActionKind::InstanceMove.as_str();
        if caller.scope < Scope::Doctor {
            return Err(CallError::Forbidden {
                verb: verb.to_string(),
            });
        }
        let (bare, grant) = take_grant(args)?;
        let args = ActionKind::InstanceMove
            .prepare(&bare, None)
            .map_err(CallError::Invalid)?;
        let plan = approved_plan(&self.moves.journal, &args, self.moves.now())?;
        let proposal = spend(&self.moves.kernel, verb, &args, &grant)?;
        self.moves.start(plan, proposal)
    }
}

/// `instance_move_status`.
pub struct MoveStatusHandler {
    pub moves: Arc<Moves>,
}

impl ToolHandler for MoveStatusHandler {
    fn call(&self, args: Value, _caller: &Caller) -> Result<Value, CallError> {
        let object = args
            .as_object()
            .ok_or_else(|| invalid("arguments: expected an object"))?;
        if let Some(key) = object.keys().find(|key| key.as_str() != "move") {
            return Err(invalid(arca_api::unknown_field("", key, &["move"])));
        }
        let id = match object.get("move") {
            None => None,
            Some(value) => Some(
                value
                    .as_str()
                    .filter(|id| move_id_ok(id))
                    .ok_or_else(|| invalid("move: expected mv- and twelve hex digits"))?,
            ),
        };
        let moves = self.moves.status(id);
        if let Some(id) = id
            && moves.is_empty()
        {
            return Err(refused("NoSuchMove", format!("no move {id} on this node")));
        }
        Ok(json!({ "moves": moves }))
    }
}

fn move_id_ok(id: &str) -> bool {
    id.strip_prefix("mv-").is_some_and(|hex| {
        hex.len() == 12
            && hex
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    })
}

/// `instance_move_finish`: deletes a verified move's sources, only from an approved card, once.
pub struct MoveFinishHandler {
    pub moves: Arc<Moves>,
}

impl ToolHandler for MoveFinishHandler {
    fn call(&self, args: Value, caller: &Caller) -> Result<Value, CallError> {
        let verb = ActionKind::InstanceMoveFinish.as_str();
        if caller.scope < Scope::Doctor {
            return Err(CallError::Forbidden {
                verb: verb.to_string(),
            });
        }
        let (bare, grant) = take_grant(args)?;
        let args = ActionKind::InstanceMoveFinish
            .prepare(&bare, None)
            .map_err(CallError::Invalid)?;
        let id = args["move"].as_str().unwrap_or_default().to_string();
        let records = fold(&self.moves.journal.lines());
        let record = records
            .get(&id)
            .cloned()
            .ok_or_else(|| refused("NoSuchMove", format!("no move {id} on this node")))?;
        let (available, why, _) = record.finish_view();
        if !available {
            return Err(refused(
                "NotVerified",
                format!("move {id} cannot be finished: {why}"),
            ));
        }
        spend(&self.moves.kernel, verb, &args, &grant)?;
        let source = MoveWriter::new(self.moves.cfg.incus.socket.clone());
        let (mut deleted, mut kept) = (Vec::new(), Vec::new());
        for one in record
            .instances
            .iter()
            .filter(|one| one.result.as_deref() == Some("moved"))
        {
            let label = format!("{}/{}", one.project, one.name);
            let path = format!(
                "/1.0/instances/{}?project={}",
                seg(&one.name),
                seg(&one.project)
            );
            match source.get(&path) {
                Ok(doc) if doc.get("status") == Some(&json!("Running")) => {
                    kept.push(format!(
                        "{label}: it runs again on this host, so it was kept"
                    ));
                    continue;
                }
                Ok(_) => match source.delete_instance(&one.project, &one.name) {
                    Ok(()) => deleted.push(label.clone()),
                    Err(error) => {
                        kept.push(format!("{label}: it could not be deleted: {error}"));
                        continue;
                    }
                },
                Err(IncusError::Status { code: 404, .. }) => deleted.push(label.clone()),
                Err(error) => {
                    kept.push(format!("{label}: it could not be read: {error}"));
                    continue;
                }
            }
            for volume in one.volumes.as_array().into_iter().flatten() {
                let pool = volume["pool"].as_str().unwrap_or_default();
                let name = volume["name"].as_str().unwrap_or_default();
                let project = volume["project"].as_str().unwrap_or("default");
                let volume_label = format!("volume {pool}/{name}");
                let used = source
                    .get(&format!(
                        "/1.0/storage-pools/{}/volumes/custom/{}?project={}",
                        seg(pool),
                        seg(name),
                        seg(project)
                    ))
                    .ok()
                    .and_then(|doc| doc.get("used_by").and_then(Value::as_array).map(Vec::len))
                    .unwrap_or(0);
                if used > 0 {
                    kept.push(format!(
                        "{volume_label}: another instance uses it, so it was kept"
                    ));
                    continue;
                }
                match source.delete_volume(project, pool, name) {
                    Ok(()) | Err(IncusError::Status { code: 404, .. }) => {
                        deleted.push(volume_label)
                    }
                    Err(error) => {
                        kept.push(format!("{volume_label}: it could not be deleted: {error}"))
                    }
                }
            }
        }
        self.moves.note(json!({
            "event": "finish", "at": self.moves.now(), "move": id, "deleted": deleted, "kept": kept,
        }));
        let message = if kept.is_empty() {
            format!("move {id} finished: {} deleted on this host", deleted.len())
        } else {
            format!(
                "move {id} finished in part: {} deleted, {} kept",
                deleted.len(),
                kept.len()
            )
        };
        Ok(json!({ "move": id, "deleted": deleted, "kept": kept, "message": message }))
    }
}

/// The four verbs of a move, sharing one [`Moves`].
pub fn handlers(moves: Arc<Moves>) -> Vec<(&'static str, Arc<dyn ToolHandler>)> {
    vec![
        (
            "instance_move_plan",
            Arc::new(MovePlanHandler {
                moves: Arc::clone(&moves),
            }) as Arc<dyn ToolHandler>,
        ),
        (
            "instance_move",
            Arc::new(MoveHandler {
                moves: Arc::clone(&moves),
            }) as Arc<dyn ToolHandler>,
        ),
        (
            "instance_move_status",
            Arc::new(MoveStatusHandler {
                moves: Arc::clone(&moves),
            }) as Arc<dyn ToolHandler>,
        ),
        (
            "instance_move_finish",
            Arc::new(MoveFinishHandler { moves }) as Arc<dyn ToolHandler>,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_torn_last_line_is_skipped_and_the_next_append_starts_on_its_own_line() {
        let dir = std::env::temp_dir().join(format!("arca-move-journal-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("dir");
        let journal = Journal::at(&dir);
        journal
            .append(&json!({ "event": "begin", "move": "mv-000000000001" }))
            .expect("append");
        // A crash leaves half a line with no newline.
        let mut file = OpenOptions::new()
            .append(true)
            .open(dir.join(MOVES_FILE))
            .expect("open");
        file.write_all(br#"{"event":"round","mo"#).expect("torn");
        drop(file);
        assert_eq!(journal.lines().len(), 1, "the torn tail is skipped");
        journal
            .append(&json!({ "event": "end", "move": "mv-000000000001" }))
            .expect("append after a torn tail");
        let lines = journal.lines();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1]["event"], "end");
        let mode = fs::metadata(dir.join(MOVES_FILE))
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_estimate_is_the_stop_and_start_and_a_one_percent_delta() {
        assert_eq!(estimated_downtime(0), 30);
        assert_eq!(estimated_downtime(100 * ASSUMED_RATE), 31);
    }

    #[test]
    fn a_move_id_is_mv_and_twelve_lowercase_hex_digits() {
        assert!(move_id_ok("mv-0123456789ab"));
        assert!(!move_id_ok("mv-0123456789AB"));
        assert!(!move_id_ok("mv-0123"));
    }
}
