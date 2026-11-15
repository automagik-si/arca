//! Moving containers, and stopped virtual machines, to a linked host: a frozen plan, one owner card, copy rounds while
//! the container runs, a short cutover, checks, a measured downtime, and a rollback. The source is never deleted by the
//! move; the owner finishes a verified move with a card of its own.
//!
//! The rule above all: no path deletes the only copy of an instance, and no path leaves the source and its copy both
//! running. A rollback and a finish act only on objects whose uuid this move recorded, after reading the other side
//! back: a rollback deletes the target copy only when the source is read back with the plan's uuid, and starts the
//! source again only when the copy is read back gone or stopped; a finish deletes the source only when the copy is read
//! back on the target with the uuid this move recorded when it made it.
//!
//! - `instance_move_plan` runs the whole `instance_move_preflight` again and freezes, for instances that all pass, a
//!   plan named by its digest: a SHA-256 over the canonical plan without its times. A source that changes (its uuid,
//!   devices, config, profiles or measured size) gives another digest. A no-go blocks the plan with the preflight's own
//!   reasons; a virtual machine that is not stopped is not movable yet, and an ephemeral container, which Incus deletes
//!   when it stops, is not movable at all. Nor is an instance while a copy an earlier move recorded may still run, read
//!   on its target. Plans are kept one file each in `instance-move-plans/`, and pruned a day after they expire.
//! - The card is the action `instance_move` (`action_propose`), whose arguments carry the digest, the instances and the
//!   target; the owner's passkey approves that digest, and the grant it arms is spent once.
//! - `instance_move` (the approved run) refuses an expired, unknown, altered or already-run plan, an instance another
//!   move holds or with a copy an earlier move made that may still run, and a target without room for it beside the moves running to it. It journals the plan and `begin`,
//!   and runs in the background: per instance, Incus pushes the container and its custom volumes from this host to the
//!   target (`push`; each of the target's addresses is tried in order; a relay through this kernel is the fallback the
//!   plan names and is not built yet), in rounds while it runs (without freezing it), then the source is stopped (a
//!   source neither running nor stopped, such as frozen, fails the instance), the last round sent (today Incus resends
//!   the whole volume), the source read back stopped, and only then the copy started; then it is checked. The downtime is measured from the stop to the passed
//!   checks. The kernel orchestrates; it copies no storage itself.
//! - `instance_move_status` reads how a move stands; `instance_move_finish` (a card) deletes a verified move's sources.
//!
//! The journal, `instance-moves.jsonl` in the state directory, is append-only JSON lines (mode 0600) with the events
//! `plan`, `begin`, `round`, `created`, `cutover`, `verify`, `result`, `rollback`, `end` and `finish`. It holds no
//! secret: no websocket secret and no certificate, only the target certificate's fingerprint. A torn last line is
//! skipped, and the next append starts on a new line. A move cannot begin unless its `plan` and `begin` lines are
//! written, and an instance cannot go past its first round unless the `created` line (the uuid of the copy) is.
//!
//! A stopped virtual machine moves with no round while it runs and no stop: nothing here starts or stops it. Its copy is
//! made once, consistent, and the same journal lines (`created`, `cutover`, `round`, `verify`, `result`) carry it, so the
//! rollback, the recovery after a stopped kernel and the finish are the containers' own. It is copied one of two ways,
//! fixed in the plan (`copy`):
//!
//! - `incus`: Incus on this host pushes it, with its snapshots and custom volumes, as it pushes a container's last round.
//!   This needs the target to listen on the network, and is the way whenever the linked host names no Ceph cluster, or a
//!   pool the machine uses is not Ceph on both sides, or it has an ISO volume (Incus cannot make an empty one).
//! - `ceph`: the linked host names its Ceph cluster as this node reaches it (`approval.linkedHosts[].ceph`), and every
//!   pool the machine uses is a Ceph pool here and there: two clusters, either way (see the `rbd` module below). The plan
//!   freezes, per Incus pool, the cluster, client and OSD pool each side is read or written with. The run reads the
//!   source's `last_used_at`, then every source image (who holds it open: anyone does, and nothing is made; and its
//!   size), then, per custom volume and then for the machine, asks Incus on the target to make it empty at the source's
//!   exact size, journals its uuid, reads the image Incus made back at that size, and copies with `rbd export-diff | rbd
//!   import-diff`: the machine's config image (`qemu.nvram`, its UEFI variables, and each TPM's state) and then its
//!   disk. A source whose `last_used_at` moved during the copy was started and stopped again: the copy may be torn and
//!   is rolled back. The source's cluster is only read. Snapshots are not copied. Two sides naming one OSD pool of one
//!   cluster are refused: Incus would make the copy's images beside the source's own.
//!
//! The copy then starts on the target once the source still reads Stopped, and counts only when its guest answers:
//! its Incus agent (processes counted), or one of its addresses answering a TCP connection from this node, accepted or
//! refused, within five times the checks' wait. An address Incus lists is not an answer by itself: without the agent
//! Incus still lists the one it derives from the card's MAC, the MAC's leases and OVN's, for a guest that never
//! booted. A stopped machine's files
//! cannot be read, so the spot-check stands on that answer. The source is never deleted by the move; on a failure the
//! copy is deleted as for a container, and the source, never started, stopped or written, stays as it was.
//!
//! A move a stopped kernel left unfinished is not resumed. On the next start each instance without a result is settled:
//! one whose checks passed is recorded as moved; one whose copy had been started is left as it stands (the copy may have
//! served writes) with the source stopped, and named for the owner; any other is rolled back under the rule above.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arca_api::Scope;
use arca_incus::migrate::{OpState, progress_of};
use arca_incus::{IncusError, MoveWriter, PushTarget, Receive};
use arca_kernel::canon::{Digest, canonical_json};
use arca_mcp::{CallError, Caller, ToolHandler};
use serde_json::{Map, Value, json};

use crate::action_proposal::ActionKind;
use crate::config::{LinkedCeph, NodeConfig, host_name_ok};
use crate::handlers::Clock;
use crate::kernel::Kernel;
use crate::move_preflight::{MovePreflightHandler, human, instance_ok, project_ok, seg};
use arca_vmimport::{ProcessRunner, Runner};
use rbd::RbdImage;

/// The move journal's file name in the state directory.
pub const MOVES_FILE: &str = "instance-moves.jsonl";
/// The folder of frozen plans in the state directory, one `<digest>.json` each.
pub const PLANS_DIR: &str = "instance-move-plans";
/// How long a frozen plan may be approved and run.
pub const PLAN_TTL_MS: u64 = 60 * 60 * 1000;
/// How long an expired plan's file is kept before a later plan prunes it.
const PLAN_KEEP_MS: u64 = 24 * 60 * 60 * 1000;
/// The copy rounds while the instance runs, unless the plan names another count.
pub const DEFAULT_ROUNDS: u64 = 2;
/// The file the checks compare when the plan names none.
pub const DEFAULT_SPOT_CHECK: &str = "/etc/hostname";
/// What every card and plan promises about a failure.
pub const ROLLBACK_PROMISE: &str = "If anything fails before the moved copy passes its checks, Arca removes the copy it made on the target and starts the source again, but only after reading the source back as the same instance, and only once the copy is gone or stopped: it never deletes the only copy and never leaves both running. The source keeps running while it is copied, its volumes are never touched, and it is never deleted unless you finish the move.";

/// The rollback promise for a stopped virtual machine, which the move never starts or writes.
pub const VM_ROLLBACK_PROMISE: &str = "If anything fails before the moved copy passes its checks, Arca deletes the copy it made on the target (the machine and the volumes it made, by the uuid it recorded), but only after reading the source back as the same instance. The source stays stopped throughout: the move never starts it, never writes its disks or volumes, and never deletes it unless you finish the move.";

/// The rollback promise a plan of `instances` shows: the containers', the stopped virtual machines', or both.
fn rollback_promise(instances: &[Value]) -> String {
    let vms = instances.iter().filter(|one| is_vm(one)).count();
    match vms {
        0 => ROLLBACK_PROMISE.to_string(),
        n if n == instances.len() => VM_ROLLBACK_PROMISE.to_string(),
        _ => format!(
            "Containers: {ROLLBACK_PROMISE} Stopped virtual machines: {VM_ROLLBACK_PROMISE}"
        ),
    }
}
/// The longest journal line this build reads; a longer one is skipped.
const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;
/// How long a graceful stop of the source may take before it is forced.
const STOP_GRACE: Duration = Duration::from_secs(60);
/// How long a start may take.
const START_DEADLINE: Duration = Duration::from_secs(120);
/// How long one copy of one object may run before it counts as failed.
const ROUND_DEADLINE: Duration = Duration::from_secs(24 * 60 * 60);
/// How long a delete Incus answers as busy (an operation still holds the object) is tried again.
const BUSY_RETRY: Duration = Duration::from_secs(30);
/// The rate a card's downtime estimate assumes, in bytes a second, and the share of the data it assumes changes between
/// the last round and the stop.
const ASSUMED_RATE: u64 = 50 * 1024 * 1024;
/// The seconds a card's estimate gives the stop, the start and the checks.
const ASSUMED_STOP_START_SECONDS: u64 = 30;
/// How long a moved virtual machine's guest is given to answer, five times the checks' own wait: a machine boots slower
/// than a container starts.
const VM_ANSWER_FACTOR: u32 = 5;

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

fn digest_ok(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Whether an instance may run, as its document reads: anything but Stopped (Running, Frozen, a status in between).
fn running(doc: &Value) -> bool {
    doc.get("status") != Some(&json!("Stopped"))
}

/// Whether a plan's instance, or an Incus document, is a virtual machine.
fn is_vm(doc: &Value) -> bool {
    doc.get("type") == Some(&json!("virtual-machine"))
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

    /// Every line that reads as a JSON object, however long the journal grows. A torn last line (no newline after it)
    /// is skipped; any other line that does not read, or is longer than 8 MiB, is skipped and said on the log.
    pub fn lines(&self) -> Vec<Value> {
        let _held = self.lock.lock();
        let Ok(meta) = fs::symlink_metadata(&self.path) else {
            return Vec::new();
        };
        if !meta.is_file() {
            eprintln!("arca: the move journal is not a regular file; it is not read");
            return Vec::new();
        }
        let Ok(file) = fs::File::open(&self.path) else {
            return Vec::new();
        };
        let mut reader = BufReader::new(file);
        let mut out = Vec::new();
        let mut row = Vec::new();
        loop {
            row.clear();
            let read = (&mut reader)
                .take(MAX_LINE_BYTES as u64 + 1)
                .read_until(b'\n', &mut row);
            match read {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let complete = row.last() == Some(&b'\n');
            if !complete && row.len() > MAX_LINE_BYTES {
                eprintln!("arca: a line of the move journal is longer than 8 MiB and is skipped");
                let mut rest = Vec::new();
                let _ = reader.read_until(b'\n', &mut rest);
                continue;
            }
            match serde_json::from_slice::<Value>(&row) {
                Ok(value) if value.is_object() => out.push(value),
                _ if !complete => {}
                _ => eprintln!("arca: a line of the move journal does not read and is skipped"),
            }
        }
        out
    }
}

/// The frozen plans: one private file each, named by the digest.
pub struct Plans {
    dir: PathBuf,
}

impl Plans {
    pub fn at(state_dir: &Path) -> Self {
        Self {
            dir: state_dir.join(PLANS_DIR),
        }
    }

    /// The plan with `digest`, as it was frozen.
    pub fn get(&self, digest: &str) -> Option<Value> {
        if !digest_ok(digest) {
            return None;
        }
        let path = self.dir.join(format!("{digest}.json"));
        let meta = fs::symlink_metadata(&path).ok()?;
        if !meta.is_file() || meta.len() > MAX_LINE_BYTES as u64 {
            return None;
        }
        serde_json::from_slice(&fs::read(path).ok()?).ok()
    }

    /// Freezes `plan`. A plan with the same digest that has not expired is kept as it is, its expiry included: planning
    /// the same state again never renews an approved plan. Plans that expired a day ago or more are pruned. Answers the
    /// plan as kept.
    pub fn save(&self, plan: Value, now: u64) -> std::io::Result<Value> {
        let digest = plan["digest"].as_str().unwrap_or_default().to_string();
        if !digest_ok(&digest) {
            return Err(std::io::Error::other("the plan has no digest"));
        }
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)?;
        self.prune(now);
        if let Some(kept) = self.get(&digest)
            && kept["expiresAt"].as_u64().unwrap_or(0) >= now
        {
            return Ok(kept);
        }
        let path = self.dir.join(format!("{digest}.json"));
        let staging = self.dir.join(format!(".{digest}.json.tmp"));
        let _ = fs::remove_file(&staging);
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&staging)?;
        file.write_all(&serde_json::to_vec(&plan).map_err(std::io::Error::other)?)?;
        file.sync_all()?;
        fs::rename(&staging, &path)?;
        Ok(plan)
    }

    fn prune(&self, now: u64) {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(digest) = name.strip_suffix(".json") else {
                continue;
            };
            let gone = self.get(digest).is_some_and(|plan| {
                plan["expiresAt"]
                    .as_u64()
                    .unwrap_or(0)
                    .saturating_add(PLAN_KEEP_MS)
                    < now
            });
            if gone {
                let _ = fs::remove_file(entry.path());
            }
        }
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

fn uuid_of(doc: &Value) -> Option<String> {
    doc.pointer("/config/volatile.uuid")
        .and_then(Value::as_str)
        .filter(|uuid| !uuid.is_empty())
        .map(str::to_string)
}

/// The instance's own config without its volatile keys: what the plan's digest covers of it.
fn config_digest(doc: &Value) -> String {
    let kept: Map<String, Value> = doc
        .get("config")
        .and_then(Value::as_object)
        .map(|config| {
            config
                .iter()
                .filter(|(key, _)| !key.starts_with("volatile."))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default();
    digest_of(&Value::Object(kept))
}

/// A target's Incus, as this node reaches it: the socket the linked host's config names, and its Ceph cluster when the
/// config names one.
struct Target {
    socket: PathBuf,
    ceph: Option<LinkedCeph>,
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
    Ok(Target {
        socket,
        ceph: linked.ceph.clone(),
    })
}

/// The transport facts of a target read from its server document: every address it listens on, in its order, and its
/// certificate's PEM.
fn transport_facts(server: &Value) -> (Vec<String>, Option<String>) {
    let addresses = server
        .pointer("/environment/addresses")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(Value::as_str)
                .filter(|address| !address.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let certificate = server
        .pointer("/environment/certificate")
        .and_then(Value::as_str)
        .filter(|pem| !pem.is_empty())
        .map(str::to_string);
    (addresses, certificate)
}

/// The project a custom volume of `project` is kept in on a host: the project itself when it sets
/// `features.storage.volumes`, else `default`.
fn volume_project(writer: &MoveWriter, project: &str) -> String {
    let own = project != "default"
        && writer
            .get(&format!("/1.0/projects/{}", seg(project)))
            .ok()
            .and_then(|doc| {
                doc.pointer("/config/features.storage.volumes")
                    .and_then(Value::as_str)
                    .map(|value| value == "true")
            })
            .unwrap_or(false);
    if own { project } else { "default" }.to_string()
}

/// The custom volumes an instance takes with it, each `{device, pool, name, project, targetProject, path}`: the project
/// Incus keeps it in here, and the one it lands in on the target, each by that host's own project setting.
fn volumes_of(
    source: &MoveWriter,
    target: Option<&MoveWriter>,
    project: &str,
    instance: &Value,
) -> Vec<Value> {
    let found: Vec<(String, Value)> = devices(instance)
        .into_iter()
        .filter(|(_, device)| text(device, "type") == Some("disk") && !is_root(device))
        .filter(|(_, device)| text(device, "pool").is_some() && text(device, "source").is_some())
        .collect();
    if found.is_empty() {
        return Vec::new();
    }
    let here = volume_project(source, project);
    let there = target.map_or_else(|| here.clone(), |target| volume_project(target, project));
    found
        .into_iter()
        .map(|(device_name, device)| {
            json!({
                "device": device_name,
                "pool": text(&device, "pool"),
                "name": text(&device, "source"),
                "project": here,
                "targetProject": there,
                "path": text(&device, "path"),
            })
        })
        .collect()
}

fn volume_path(pool: &str, name: &str, project: &str) -> String {
    format!(
        "/1.0/storage-pools/{}/volumes/custom/{}?project={}",
        seg(pool),
        seg(name),
        seg(project)
    )
}

fn instance_path(project: &str, name: &str) -> String {
    format!("/1.0/instances/{}?project={}", seg(name), seg(project))
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

/// The downtime a card estimates, in seconds: the stop, the start and the checks, and the last round at 50 MiB/s. Today
/// that round resends the whole volume (the hosts share no snapshot for Incus to send a delta from), so the estimate
/// counts all of `bytes`. The move measures the real one.
fn estimated_downtime(bytes: u64) -> u64 {
    ASSUMED_STOP_START_SECONDS + bytes.div_ceil(ASSUMED_RATE)
}

/// A plan's rounds for one instance: `rounds` copy rounds while a container runs; none for a stopped virtual machine,
/// whose one copy is its last round.
fn rounds_of(vm: bool, rounds: u64) -> Value {
    if vm {
        return json!({ "preCopy": 0, "order": [] });
    }
    json!({
        "preCopy": rounds,
        "order": (1..=rounds)
            .map(|round| if round == 1 {
                "a full copy while it runs, not frozen, with its snapshots and custom volumes".to_string()
            } else {
                format!("refresh {} while it runs, not frozen (today Incus resends the whole volume: the hosts share no snapshot)", round - 1)
            })
            .collect::<Vec<_>>(),
    })
}

/// A plan's cutover for one instance, in order.
fn cutover_of(vm: bool, by_ceph: bool, target: &str) -> Value {
    if !vm {
        return json!([
            "stop the source",
            "send the last round, consistent: the whole volume today",
            "read the source back stopped",
            "start the copy on the target"
        ]);
    }
    let copy = if by_ceph {
        format!(
            "Incus on {target} makes the virtual machine and its custom volumes empty, of the source's sizes, and this node copies each disk by rbd export-diff | rbd import-diff, reading the source only"
        )
    } else {
        "Incus pushes the virtual machine, with its snapshots and custom volumes, consistent: it is stopped".to_string()
    };
    json!([
        "the virtual machine stays stopped: nothing here starts it",
        copy,
        "read the source back stopped",
        "start the copy on the target"
    ])
}

/// What a plan says it checks on one moved instance.
fn verify_of(vm: bool, spot_check: &str) -> Value {
    if vm {
        return json!([
            "the copy runs, and still runs after the checks",
            "its guest answers: its Incus agent, or one of its addresses answering a connection from this node",
            "each custom volume is attached",
        ]);
    }
    json!([
        "the copy runs, and still runs after the checks",
        "each network card that had an address has one",
        "each TCP proxy on every address that answered here answers on the target",
        "each custom volume is attached",
        format!("the file {spot_check} is the same on both"),
    ])
}

/// A stopped virtual machine's card row: the two nodes, its disk's size, what it needs there, that it stays stopped,
/// and how it is copied.
fn vm_row(instance: &Value, source: &str, target: &str) -> (String, String) {
    let volumes = instance["volumes"].as_array().map_or(0, Vec::len);
    let how = if instance["copy"] == "ceph" {
        "by Ceph (rbd), disk by disk, with its config image (its UEFI variables and TPM state) and without its snapshots, which stay with the source"
    } else {
        "by an Incus push from this node, with its snapshots"
    };
    (
        format!(
            "Move {}/{} (virtual machine, stopped)",
            instance["project"].as_str().unwrap_or("?"),
            instance["name"].as_str().unwrap_or("?")
        ),
        format!(
            "from {source} to {target}; its disk is {}{}; it needs {} on {target}'s pool {}; it stays stopped for the move and is copied {how}; about {} s at 50 MiB/s (the move measures the real time)",
            human(instance["bytes"]["instance"].as_u64().unwrap_or(0)),
            if volumes == 0 {
                String::new()
            } else {
                format!(", with {volumes} custom volume(s)")
            },
            human(instance["bytes"]["needed"].as_u64().unwrap_or(0)),
            instance["targetPool"].as_str().unwrap_or("?"),
            instance["estimatedDowntimeSeconds"].as_u64().unwrap_or(0)
        ),
    )
}

/// The card's Ceph row: per virtual machine copied by Ceph, each pool's cluster, OSD pool and client on both sides.
fn ceph_words(instances: &[&&Value], target: &str) -> String {
    let side = |facts: &Value| {
        format!(
            "the cluster {}, pool {}, as client.{}",
            facts["cluster"].as_str().unwrap_or("?"),
            facts["pool"].as_str().unwrap_or("?"),
            facts["client"].as_str().unwrap_or("?")
        )
    };
    let mut words = Vec::new();
    for instance in instances {
        for (pool, facts) in instance["ceph"].as_object().into_iter().flatten() {
            words.push(format!(
                "{}/{} (pool {pool}): read only from {} here, written into {} for {target}",
                instance["project"].as_str().unwrap_or("?"),
                instance["name"].as_str().unwrap_or("?"),
                side(&facts["from"]),
                side(&facts["to"])
            ));
        }
    }
    format!(
        "{}. rbd export-diff | rbd import-diff, two processes and a pipe on this node; the source is never written",
        words.join("; ")
    )
}

/// One Incus pool's Ceph facts, as its document says: its OSD pool, the client Incus uses, and its cluster. `None` when
/// the pool cannot be read or is not a Ceph pool.
fn ceph_side(writer: &MoveWriter, pool: &str) -> Option<(String, String, String)> {
    let doc = writer
        .get(&format!("/1.0/storage-pools/{}", seg(pool)))
        .ok()?;
    if text(&doc, "driver") != Some("ceph") {
        return None;
    }
    let config = |key: &str, default: &str| {
        doc["config"][key]
            .as_str()
            .filter(|value| !value.is_empty())
            .unwrap_or(default)
            .to_string()
    };
    Some((
        config("ceph.osd.pool_name", pool),
        config("ceph.user.name", "admin"),
        config("ceph.cluster_name", "ceph"),
    ))
}

/// How a stopped virtual machine is copied: by Ceph when the linked host names its Ceph cluster (`linked`) and every
/// pool the machine uses, its root's and each custom volume's, is a Ceph pool here and there; by an Incus push otherwise
/// (`Ok(Null)`), which also takes a custom volume on another driver and an ISO volume (Incus cannot make an empty one).
/// By Ceph, answers the facts per Incus pool here: the target pool, and the cluster, client and OSD pool each side is
/// read or written with. The one error is data safety: two sides that name one OSD pool of one cluster, where Incus
/// making the copy's images would meet the source's own.
fn ceph_plan(
    source: &MoveWriter,
    dest: Option<&MoveWriter>,
    linked: Option<&LinkedCeph>,
    (root_pool, root_target): (&str, &str),
    volumes: &[Value],
    target_name: &str,
) -> Result<Value, Vec<String>> {
    let (Some(linked), Some(dest)) = (linked, dest) else {
        return Ok(Value::Null);
    };
    let (Some(_), Some(_)) = (ceph_side(source, root_pool), ceph_side(dest, root_target)) else {
        return Ok(Value::Null);
    };
    let mut pools = vec![(root_pool.to_string(), root_target.to_string())];
    for volume in volumes {
        let pool = volume["pool"].as_str().unwrap_or_default().to_string();
        let iso = source
            .get(&volume_path(
                &pool,
                volume["name"].as_str().unwrap_or_default(),
                volume["project"].as_str().unwrap_or("default"),
            ))
            .is_ok_and(|doc| doc["content_type"] == "iso");
        if iso {
            return Ok(Value::Null);
        }
        if !pools.iter().any(|(here, _)| *here == pool) {
            pools.push((pool.clone(), pool));
        }
    }
    let mut sides = Vec::new();
    for (here, there) in pools {
        match (ceph_side(source, &here), ceph_side(dest, &there)) {
            (Some(from), Some(to)) => sides.push((here, there, from, to)),
            _ => return Ok(Value::Null),
        }
    }
    let mut plan = Map::new();
    let mut reasons = Vec::new();
    for (here, there, from, to) in sides {
        let (from_pool, from_client, from_cluster) = from;
        let (to_pool, to_client, _) = to;
        let from = json!({
            "cluster": from_cluster,
            "client": linked.read_client.clone().unwrap_or(from_client),
            "pool": from_pool,
        });
        let to = json!({
            "cluster": linked.cluster,
            "client": linked.client.clone().unwrap_or(to_client),
            "pool": to_pool,
        });
        if from["cluster"] == to["cluster"] && from["pool"] == to["pool"] {
            reasons.push(format!(
                "this node's pool {here} and {target_name}'s pool {there} name one OSD pool, {}, of one Ceph cluster, {}: the copy's images would be made beside the source's own",
                to["pool"].as_str().unwrap_or("?"),
                to["cluster"].as_str().unwrap_or("?")
            ));
            continue;
        }
        plan.insert(here, json!({ "targetPool": there, "from": from, "to": to }));
    }
    if reasons.is_empty() {
        Ok(Value::Object(plan))
    } else {
        Err(reasons)
    }
}

/// The TCP port of a proxy that listens on every address of its host (`0.0.0.0` or `::`).
fn wildcard(listen: &str) -> Option<u16> {
    let rest = listen.strip_prefix("tcp:")?;
    let (host, port) = rest.rsplit_once(':')?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if matches!(host, "0.0.0.0" | "::" | "") {
        port.parse().ok()
    } else {
        None
    }
}

/// Whether something answers on `host:port`.
fn answers(host: &str, port: u16) -> bool {
    (host, port).to_socket_addrs().is_ok_and(|mut found| {
        found.any(|address: SocketAddr| {
            TcpStream::connect_timeout(&address, Duration::from_secs(2)).is_ok()
        })
    })
}

/// What a result says when [`keep_down`] could not turn `boot.autostart` off.
const AUTOSTART_LEFT_ON: &str = "boot.autostart could not be turned off on the stopped source here: a restart of this host would start it beside its copy; turn it off";

/// What a result says when [`keep_down`] turned `boot.autostart` off.
const AUTOSTART_TURNED_OFF: &str = "boot.autostart was true on the stopped source here and is now false, so a restart of this host does not start it beside its copy; once you keep the source, set it back to true";

/// What [`keep_down`] did to a stopped source's `boot.autostart`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeptDown {
    /// Nothing of the plan's: the source is gone, or it is another instance of the name (another uuid), left alone.
    NotOurs,
    /// The key is not true (unset, `last-state` or false): a restart of this host keeps the stopped source stopped, so it
    /// was left as it was.
    Down,
    /// The key was true (or could not be read) and is now `"false"`.
    TurnedOff,
    /// The key was true (or could not be read) and could not be turned off.
    LeftOn,
}

impl KeptDown {
    /// The result line's `autostartOff`: whether a restart of this host keeps the source stopped, or null when nothing
    /// of the plan's was there.
    fn field(self) -> Value {
        match self {
            KeptDown::NotOurs => Value::Null,
            KeptDown::Down | KeptDown::TurnedOff => json!(true),
            KeptDown::LeftOn => json!(false),
        }
    }

    /// `reason`, saying what was changed or could not be.
    fn reason(self, reason: String) -> String {
        match self {
            KeptDown::TurnedOff => format!("{reason}; {AUTOSTART_TURNED_OFF}"),
            KeptDown::LeftOn => format!("{reason}; {AUTOSTART_LEFT_ON}"),
            KeptDown::NotOurs | KeptDown::Down => reason,
        }
    }
}

/// Whether an Incus boolean reads true, as Incus reads one.
fn truthy(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "true" | "1" | "yes" | "on"
    )
}

/// Keeps a source a move leaves stopped, while a copy of it may run, from starting by itself on a restart of this host.
/// Only a `boot.autostart` that reads true (in the expanded config, so a profile's counts) is changed, to `"false"`:
/// unset or `last-state` restores the last state, stopped, and is left as the owner set it. A source that cannot be
/// read is turned off. A source that is gone, or another instance of the name (another uuid), is left alone.
fn keep_down(source: &MoveWriter, project: &str, name: &str, uuid: Option<&str>) -> KeptDown {
    match source.get(&instance_path(project, name)) {
        Err(IncusError::Status { code: 404, .. }) => return KeptDown::NotOurs,
        Ok(doc) if uuid.is_some() && uuid_of(&doc).as_deref() != uuid => return KeptDown::NotOurs,
        Ok(doc) => {
            let key = doc["expanded_config"]["boot.autostart"]
                .as_str()
                .or(doc["config"]["boot.autostart"].as_str());
            if !key.is_some_and(truthy) {
                return KeptDown::Down;
            }
        }
        Err(_) => {}
    }
    if source.disable_autostart(project, name).is_ok() {
        KeptDown::TurnedOff
    } else {
        KeptDown::LeftOn
    }
}

/// What became of a verified copy's `boot.autostart`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CopyAutostart {
    /// It was set to the source's.
    Set,
    /// The write to the copy failed: the copy keeps the `false` it was made with.
    Failed,
    /// The target's instance of the name is not read as the copy this move recorded (another uuid, or not read), so
    /// nothing was written to it.
    NotTheCopy,
}

/// The reason of a `moved` result that could not set the copy's `boot.autostart` to the source's, or turn the kept
/// source's off; `None` when both were done.
fn moved_trouble(copy: CopyAutostart, autostart_off: bool, target: &str) -> Option<String> {
    let mut trouble = Vec::new();
    match copy {
        CopyAutostart::Set => {}
        CopyAutostart::Failed => trouble.push(format!(
            "boot.autostart could not be set back on the copy on {target}: it stays false, so a restart of {target} leaves the moved instance stopped; set it as it was on the source"
        )),
        CopyAutostart::NotTheCopy => trouble.push(format!(
            "the instance of this name on {target} is not read as the copy this move recorded (another uuid, or it could not be read), so its boot.autostart was left as it is; check which instance that is before you change it"
        )),
    }
    if !autostart_off {
        trouble.push(
            "boot.autostart could not be turned off on the kept source here: a reboot of this host would start it beside its copy; turn it off, or finish the move"
                .to_string(),
        );
    }
    (!trouble.is_empty()).then(|| trouble.join("; "))
}

/// Reads a copy an earlier move recorded (`uuid` on `target`): `None` when it cannot run there (gone, another instance
/// of the name, or Stopped), or why it may (it runs, or the target cannot be read).
type CopyProbe<'a> = &'a dyn Fn(&str, &str, &str, &str) -> Option<String>;

/// [`CopyProbe`] against the linked hosts of `cfg`, read live through each one's Incus socket.
fn probe_copy(
    cfg: &NodeConfig,
    target: &str,
    project: &str,
    name: &str,
    uuid: &str,
) -> Option<String> {
    let writer = match linked_target(cfg, target) {
        Ok(linked) => MoveWriter::new(linked.socket),
        Err(error) => return Some(format!("{target} could not be read: {error}")),
    };
    match writer.get(&instance_path(project, name)) {
        Err(IncusError::Status { code: 404, .. }) => None,
        Err(error) => Some(format!("{target} could not be read: {error}")),
        Ok(doc) if uuid_of(&doc).as_deref() != Some(uuid) => None,
        Ok(doc) if !running(&doc) => None,
        Ok(doc) => Some(format!(
            "it reads {} on {target}",
            text(&doc, "status").unwrap_or("no status")
        )),
    }
}

/// A copy an earlier move made of an instance that may still run: the move, its target, whether that move may still be
/// finished, and why the copy may run.
struct HeldCopy {
    id: String,
    target: String,
    finishable: bool,
    why: String,
}

/// The newest earlier move whose recorded copy of `project/name` (the instance of `uuid`, when both sides know it) may
/// still run on its target, whatever that move's result: a second move of the instance could then leave two copies
/// running. A copy its rollback journaled removed holds nothing and is not read; each other recorded copy is read on its
/// target through `probe`, and a copy read gone or stopped releases it.
fn held_copy(
    records: &BTreeMap<String, MoveRecord>,
    project: &str,
    name: &str,
    uuid: Option<&str>,
    probe: CopyProbe,
) -> Option<HeldCopy> {
    let mut made: Vec<(&MoveRecord, &InstanceRecord, &str)> = records
        .values()
        .filter_map(|record| {
            let one = record.instances.iter().find(|one| {
                one.project == project
                    && one.name == name
                    && (uuid.is_none() || one.uuid.is_none() || one.uuid.as_deref() == uuid)
            })?;
            // A copy the rollback journaled removed holds nothing, and its target need not be read.
            if one.target_removed {
                return None;
            }
            Some((record, one, one.target_uuid.as_deref()?))
        })
        .collect();
    made.sort_by_key(|(record, _, _)| std::cmp::Reverse(record.started_at));
    made.into_iter().find_map(|(record, one, copy)| {
        probe(&record.target, project, name, copy).map(|why| HeldCopy {
            id: record.id.clone(),
            target: record.target.clone(),
            finishable: one.result.as_deref() == Some("moved") && record.finished.is_none(),
            why,
        })
    })
}

/// Why `project/name` may not be moved now: [`held_copy`] in words, with what the owner can do.
fn held_reason(project: &str, name: &str, held: &HeldCopy) -> String {
    let HeldCopy {
        id,
        target,
        finishable,
        why,
    } = held;
    let next = if *finishable {
        format!(
            "finish move {id} first (it deletes the stopped source here), or stop or delete the copy on {target} once you have kept what it holds: it is the instance that has served since the cutover"
        )
    } else {
        format!("stop or delete the copy on {target} first, once you have kept what it holds")
    };
    format!(
        "{project}/{name} has a copy on {target} from move {id} that may still run ({why}): a second move would leave two copies running; {next}"
    )
}

/// [`held_copy`] as a refusal: `AlreadyMoved` while that move may still be finished, `CopyMayRun` otherwise.
fn held_refusal(project: &str, name: &str, held: &HeldCopy) -> CallError {
    refused(
        if held.finishable {
            "AlreadyMoved"
        } else {
            "CopyMayRun"
        },
        held_reason(project, name, held),
    )
}

/// Why a move of `refs` (project, name, uuid) under the plan `digest` may not begin now: the plan already ran, another
/// move that has not ended holds one of the instances (from the journal), or a copy an earlier move made of one may still
/// run (read on its target through `probe`). `None` when it may begin.
fn start_refusal(
    lines: &[Value],
    digest: &str,
    refs: &[MoveRef],
    probe: CopyProbe,
) -> Option<CallError> {
    if lines
        .iter()
        .any(|line| line["event"] == "begin" && line["plan"] == digest)
    {
        return Some(refused(
            "AlreadyRun",
            format!(
                "this plan already ran as move {}; plan the move again for a new card",
                Moves::move_id(digest)
            ),
        ));
    }
    for record in fold(lines)
        .values()
        .filter(|record| record.ended_at.is_none())
    {
        if let Some(held) = record.instances.iter().find(|one| {
            refs.iter()
                .any(|(p, n, _)| *p == one.project && *n == one.name)
        }) {
            return Some(refused(
                "InstanceMoving",
                format!(
                    "{}/{} is in move {}, which has not ended; wait for it, or for the next start to settle it",
                    held.project, held.name, record.id
                ),
            ));
        }
    }
    let records = fold(lines);
    refs.iter().find_map(|(project, name, uuid)| {
        held_copy(&records, project, name, uuid.as_deref(), probe)
            .map(|held| held_refusal(project, name, &held))
    })
}

/// An instance a plan names: its project, name and uuid.
type MoveRef = (String, String, Option<String>);

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
    plans: Plans,
    /// The progress of each running copy, by move, project and name.
    live: Mutex<BTreeMap<(String, String, String), Progress>>,
    /// The moves running in this process.
    running: Mutex<Vec<String>>,
    /// Held from the checks that refuse a start to its `begin` line, so two starts never both pass them.
    starting: Mutex<()>,
    poll: Duration,
    verify_wait: Duration,
    /// What runs `rbd` for a virtual machine copied by Ceph.
    ceph: Arc<dyn Runner>,
}

/// How a running copy stands.
#[derive(Debug, Clone, Default)]
struct Progress {
    round: u64,
    phase: String,
    /// Bytes of the objects this round already finished.
    done: u64,
    /// The bytes Incus says it moved of the stream it sends now, and its rate.
    processed: Option<u64>,
    speed: Option<u64>,
}

/// What one instance's run made and did, for its rollback: the uuid of the copy it made on the target, the custom
/// volumes it made there, whether it stopped the source, and whether it started the copy.
#[derive(Debug, Clone, Default)]
struct Made {
    target_uuid: Option<String>,
    volumes: Vec<Value>,
    stopped_source: bool,
    /// The source's own `boot.autostart` when its copy was made ("" for unset): the copy takes it once verified.
    source_autostart: Option<String>,
}

/// What the source looked like just before the stop: its file for the spot-check, and the TCP ports of its wildcard
/// proxies that answered here.
#[derive(Debug, Clone, Default)]
struct Before {
    spot: Option<Vec<u8>>,
    ports: Vec<u16>,
    unprobed: Vec<String>,
}

impl Moves {
    pub fn new(cfg: Arc<NodeConfig>, kernel: Arc<Mutex<Kernel>>, clock: Clock) -> Self {
        let journal = Journal::at(&cfg.state_dir);
        let plans = Plans::at(&cfg.state_dir);
        Self {
            cfg,
            storage_root: None,
            kernel,
            clock,
            journal,
            plans,
            live: Mutex::new(BTreeMap::new()),
            running: Mutex::new(Vec::new()),
            starting: Mutex::new(()),
            poll: Duration::from_secs(2),
            verify_wait: Duration::from_secs(60),
            ceph: Arc::new(ProcessRunner),
        }
    }

    /// The same, running `rbd` through `runner` for the virtual machines it copies by Ceph.
    pub fn with_ceph_runner(mut self, runner: Arc<dyn Runner>) -> Self {
        self.ceph = runner;
        self
    }

    /// The same, measuring the instances' storage folders under `root` (the folder that holds `storage-pools`).
    pub fn with_storage_root(mut self, root: PathBuf) -> Self {
        self.storage_root = Some(root);
        self
    }

    /// The same, polling a running copy every `poll`, and giving the moved copy `verify_wait` to come up (and a failed
    /// copy's receiving operation as long to end).
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

    /// Appends a journal line; answers whether it was written. The callers that cannot go on without the line stop.
    fn note(&self, line: Value) -> bool {
        match self.journal.append(&line) {
            Ok(()) => true,
            Err(error) => {
                eprintln!("arca: the move journal could not be written: {error}");
                false
            }
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

    fn source(&self) -> MoveWriter {
        MoveWriter::new(self.cfg.incus.socket.clone())
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
        let source = self.source();
        let target = linked_target(&self.cfg, target_name);
        let target_writer = target
            .as_ref()
            .ok()
            .map(|t| MoveWriter::new(t.socket.clone()));
        let server = target_writer.as_ref().and_then(|w| w.get("/1.0").ok());
        let (addresses, certificate) = server
            .as_ref()
            .map(transport_facts)
            .unwrap_or((Vec::new(), None));
        let mut blocked = Vec::new();
        let mut instances = Vec::new();
        // The instances Incus pushes (every one not copied by Ceph): they need the target to listen.
        let mut pushed = Vec::new();
        let records = fold(&self.journal.lines());
        for (index, one) in wanted.iter().enumerate() {
            let row = &preflight["rows"][index];
            let size = row["checks"]
                .as_array()
                .and_then(|checks| checks.iter().find(|check| check["check"] == "size"))
                .map(|check| check["measure"].clone())
                .unwrap_or(Value::Null);
            let mut ceph = Value::Null;
            let doc = source.get(&instance_path(&one.project, &one.name)).ok();
            let volumes = doc
                .as_ref()
                .map(|doc| volumes_of(&source, target_writer.as_ref(), &one.project, doc))
                .unwrap_or_default();
            let moved: Vec<String> = volumes
                .iter()
                .filter_map(|volume| volume["device"].as_str().map(str::to_string))
                .collect();
            let mut reasons = no_go_reasons(row, &moved);
            if let Some(doc) = &doc {
                let vm = is_vm(doc);
                if vm && text(doc, "status") != Some("Stopped") {
                    reasons.insert(
                        0,
                        format!(
                            "the virtual machine is {}: this build moves a virtual machine only while it is stopped; stop it first (a running virtual machine's move comes later)",
                            text(doc, "status").unwrap_or("in no status Incus names")
                        ),
                    );
                }
                if doc.get("ephemeral") == Some(&json!(true)) {
                    reasons.push(
                        "the container is ephemeral: Incus deletes it when it stops, so the cutover would delete the source; make it non-ephemeral first".to_string(),
                    );
                }
                if uuid_of(doc).is_none() {
                    reasons.push(
                        "the instance has no volatile.uuid, so the move could not tell it from another of its name".to_string(),
                    );
                }
                let probe = |target: &str, project: &str, name: &str, uuid: &str| {
                    probe_copy(&self.cfg, target, project, name, uuid)
                };
                if let Some(held) = held_copy(
                    &records,
                    &one.project,
                    &one.name,
                    uuid_of(doc).as_deref(),
                    &probe,
                ) {
                    reasons.push(held_reason(&one.project, &one.name, &held));
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
                // The spot-check file must be there now: a check that could never compare would always pass. A stopped
                // virtual machine's files cannot be read: its guest's answer stands for the check.
                if !vm
                    && let Err(error) = source.read_file(&one.project, &one.name, &one.spot_check)
                {
                    reasons.push(format!(
                        "the spot-check file {} cannot be read in {}/{} ({error}); name a file the container holds",
                        one.spot_check, one.project, one.name
                    ));
                }
                if let Some(writer) = &target_writer {
                    if writer.get(&instance_path(&one.project, &one.name)).is_ok() {
                        reasons.push(format!(
                            "{target_name} already has an instance {}/{}; a move never replaces one",
                            one.project, one.name
                        ));
                    }
                    for volume in &volumes {
                        let found = writer.get(&volume_path(
                            volume["pool"].as_str().unwrap_or_default(),
                            volume["name"].as_str().unwrap_or_default(),
                            volume["targetProject"].as_str().unwrap_or_default(),
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
                if vm {
                    let pools = (
                        size["sourcePool"].as_str().unwrap_or_default(),
                        size["targetPool"].as_str().unwrap_or_default(),
                    );
                    match ceph_plan(
                        &source,
                        target_writer.as_ref(),
                        target.as_ref().ok().and_then(|t| t.ceph.as_ref()),
                        pools,
                        &volumes,
                        target_name,
                    ) {
                        Ok(plan) => ceph = plan,
                        Err(why) => reasons.extend(why),
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
            let vm = is_vm(&doc);
            if ceph.is_null() {
                pushed.push(json!({ "project": one.project, "name": one.name }));
            }
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
            let on_volume = volumes.iter().any(|volume| {
                volume["path"].as_str().is_some_and(|mount| {
                    one.spot_check
                        .starts_with(&format!("{}/", mount.trim_end_matches('/')))
                })
            });
            instances.push(json!({
                "project": one.project,
                "name": one.name,
                "uuid": uuid_of(&doc),
                "type": text(&doc, "type").unwrap_or("container"),
                "status": text(&doc, "status"),
                "devicesDigest": digest_of(&expanded),
                "configDigest": config_digest(&doc),
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
                "volumes": volumes,
                "copy": if ceph.is_null() { "incus" } else { "ceph" },
                "ceph": ceph,
                "rounds": rounds_of(vm, rounds),
                "cutover": cutover_of(vm, !ceph.is_null(), target_name),
                "verify": verify_of(vm, &one.spot_check),
                "spotCheck": one.spot_check,
                "spotCheckOnVolume": on_volume,
                "estimatedDowntimeSeconds": estimated_downtime(measured),
            }));
        }
        let transport = match (addresses.first(), &certificate) {
            (Some(address), Some(certificate)) => json!({
                "mode": "push",
                "address": address,
                "addresses": addresses,
                "certificateFingerprint": Digest::of(certificate.as_bytes()).to_hex(),
                "fallback": "relay",
                "fallbackAvailable": false,
            }),
            _ => json!({
                "mode": "relay",
                "address": null,
                "addresses": [],
                "certificateFingerprint": null,
                "fallback": "relay",
                "fallbackAvailable": false,
            }),
        };
        if blocked.is_empty() && transport["mode"] == "relay" && !pushed.is_empty() {
            let why = match &target {
                Err(why) => why.clone(),
                Ok(_) if server.is_none() => format!("{target_name}'s Incus could not be read"),
                Ok(_) => format!(
                    "{target_name}'s Incus does not listen on the network (no core.https_address), so this host cannot push to it"
                ),
            };
            for one in pushed {
                blocked.push(json!({
                    "project": one["project"], "name": one["name"],
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
            "version": 2,
            "source": { "node": self.source_name(), "incusSocket": self.cfg.incus.socket.display().to_string() },
            "target": { "name": target_name, "incusSocket": socket },
            "credential": format!("Arca writes to {target_name}'s Incus through the linked host's incusSocket {socket}, which the owner configured"),
            "transport": transport,
            "rounds": rounds,
            "instances": instances,
            "rollback": rollback_promise(&instances),
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
        let source = plan["source"]["node"].as_str().unwrap_or("this node");
        let mut rows = Vec::new();
        let instances: Vec<&Value> = plan["instances"].as_array().into_iter().flatten().collect();
        for instance in &instances {
            if is_vm(instance) {
                rows.push(vm_row(instance, source, target));
                continue;
            }
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
                    "needs {} on {target}'s pool {}{}; {rounds} copy round(s) while it runs, not frozen, then the stop, the last round and the start; downtime about {} s: the last round, after the stop, resends the whole volume today, estimated at 50 MiB/s (the move measures the real one)",
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
        let pushes = instances.iter().any(|one| one["copy"] != "ceph");
        let by_ceph: Vec<&&Value> = instances
            .iter()
            .filter(|one| one["copy"] == "ceph")
            .collect();
        rows.push((
            "Transport".to_string(),
            match plan["transport"]["mode"].as_str() {
                _ if !pushes => format!(
                    "no Incus push: this node copies each disk by Ceph; it writes to {target}'s Incus to make the empty virtual machine and its empty custom volumes, to start it, to set its boot.autostart, and on a rollback to delete what it made"
                ),
                Some("push") => format!(
                    "Incus on this host pushes to {target}'s Incus at {}",
                    plan["transport"]["addresses"]
                        .as_array()
                        .map(|list| list
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(", then "))
                        .unwrap_or_default()
                ),
                _ => "a relay through this kernel (not available yet)".to_string(),
            },
        ));
        if !by_ceph.is_empty() {
            rows.push(("Ceph".to_string(), ceph_words(&by_ceph, target)));
        }
        rows.push((
            "Credential".to_string(),
            plan["credential"].as_str().unwrap_or_default().to_string(),
        ));
        rows.push((
            "Rollback".to_string(),
            plan["rollback"]
                .as_str()
                .map_or_else(|| ROLLBACK_PROMISE.to_string(), str::to_string),
        ));
        if instances.iter().any(|one| is_vm(one)) {
            rows.push((
                "Stopped virtual machines".to_string(),
                format!(
                    "each stays stopped for its move: nothing here starts it, and its copy on {target} is started only once every disk is copied and the source still reads stopped. The move counts only when the copy runs and its guest answers; otherwise the copy is deleted and the source is left as it was"
                ),
            ));
        }
        rows.push((
            "Source".to_string(),
            "kept, stopped, with boot.autostart off, on this host after a verified move: it is deleted only when you finish the move".to_string(),
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
        format!("mv-{}", &digest[..12.min(digest.len())])
    }

    /// What has changed between the approved plan and the one the node makes now: each difference in words.
    fn differences(approved: &Value, fresh: &Value) -> Vec<String> {
        let mut out = Vec::new();
        if approved["transport"]["certificateFingerprint"]
            != fresh["transport"]["certificateFingerprint"]
            || approved["transport"]["addresses"] != fresh["transport"]["addresses"]
        {
            out.push("the target's Incus addresses or certificate".to_string());
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
                ("configDigest", "its config"),
                ("profiles", "its profiles"),
                ("volumes", "its custom volumes"),
                ("targetPool", "the target pool"),
                ("spotCheckOnVolume", "where its spot-check file lives"),
                ("copy", "how it is copied (an Incus push or Ceph)"),
                ("ceph", "its Ceph clusters, pools or clients"),
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

    /// The bytes the moves running in this process to `target` may still land, per pool.
    fn pending_on(&self, lines: &[Value], target: &str) -> BTreeMap<String, u64> {
        let running = self.running.lock().map(|r| r.clone()).unwrap_or_default();
        let mut out = BTreeMap::new();
        if running.is_empty() {
            return out;
        }
        let records = fold(lines);
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
        let refs = refs_pairs(&plan);
        // From here to the `begin` line no other start passes these checks.
        let _starting = self.starting.lock().map_err(|_| internal())?;
        let lines = self.journal.lines();
        let probe = |target: &str, project: &str, name: &str, uuid: &str| {
            probe_copy(&self.cfg, target, project, name, uuid)
        };
        if let Some(refusal) = start_refusal(&lines, &digest, &refs, &probe) {
            return Err(refusal);
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
        let pending = self.pending_on(&lines, &target_name);
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
        let instances: Vec<Value> = plan["instances"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|one| {
                json!({
                    "project": one["project"], "name": one["name"], "uuid": one["uuid"],
                    "status": one["status"],
                    "roundsTotal": one["rounds"]["preCopy"].as_u64().unwrap_or(0) + 1,
                    "bytesTotal": one["bytes"]["measured"],
                    "needed": one["bytes"]["needed"],
                    "targetPool": one["targetPool"],
                    "volumes": one["volumes"],
                })
            })
            .collect();
        // Without its plan and begin lines in the journal a move cannot be settled after a stop: it does not begin.
        let written = self.note(json!({ "event": "plan", "at": now, "move": id, "plan": plan }))
            && self.note(json!({
                "event": "begin", "at": now, "move": id, "plan": digest, "target": target_name,
                "proposal": proposal, "instances": instances,
            }));
        if !written {
            return Err(refused(
                "JournalUnwritable",
                "the move journal could not be written, so the move did not begin and nothing moved",
            ));
        }
        if let Ok(mut running) = self.running.lock() {
            running.push(id.clone());
        }
        let moves = Arc::clone(self);
        let (thread_id, thread_plan) = (id.clone(), plan.clone());
        let spawned = std::thread::Builder::new()
            .name(format!("arca-{id}"))
            .spawn(move || moves.run(&thread_id, &thread_plan, proposal));
        if spawned.is_err() {
            self.note(json!({ "event": "end", "at": self.now(), "move": id, "reason": "the move's thread could not start" }));
            if let Ok(mut running) = self.running.lock() {
                running.retain(|running| *running != id);
            }
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
    fn run(&self, id: &str, plan: &Value, proposal: Option<String>) {
        let mut results = Vec::new();
        for one in plan["instances"].as_array().into_iter().flatten() {
            results.push(self.move_one(id, plan, one));
        }
        let moved = results
            .iter()
            .filter(|result| result.starts_with("moved"))
            .count();
        let total = results.len();
        self.note(
            json!({ "event": "end", "at": self.now(), "move": id, "moved": moved, "of": total }),
        );
        if let Ok(mut running) = self.running.lock() {
            running.retain(|running| running != id);
        }
        self.late_result(proposal.as_deref(), &results);
    }

    fn late_result(&self, proposal: Option<&str>, results: &[&str]) {
        let Some(proposal) = proposal else {
            return;
        };
        let moved = results
            .iter()
            .filter(|result| result.starts_with("moved"))
            .count();
        let code = if results.contains(&"failed") || results.contains(&"moved_autostart_on") {
            "MoveNeedsOwner"
        } else if moved == results.len() {
            "MoveVerified"
        } else if moved == 0 {
            "MoveRolledBack"
        } else {
            "MovePartlyRolledBack"
        };
        if let Ok(mut kernel) = self.kernel.lock() {
            kernel.journal_late_result(proposal, moved == results.len(), code);
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

    /// Moves one instance; answers its result: `moved`, `rolled_back` or `failed`.
    fn move_one(&self, id: &str, plan: &Value, one: &Value) -> &'static str {
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
                return "failed";
            }
        };
        let source = self.source();
        let dest = MoveWriter::new(target.socket.clone());
        let mut made = Made::default();
        let outcome = self.attempt(id, plan, one, &source, &dest, &mut made);
        self.clear_live(id, &project, &name);
        match outcome {
            Ok(downtime) => {
                // The verified copy takes the source's own boot.autostart (it was made with it off), and the kept source
                // must not come back by itself on a reboot of this host, next to its copy.
                let copy = if dest
                    .set_autostart(
                        &project,
                        &name,
                        made.source_autostart.as_deref().unwrap_or_default(),
                    )
                    .is_ok()
                {
                    CopyAutostart::Set
                } else {
                    CopyAutostart::Failed
                };
                let copy_set = copy == CopyAutostart::Set;
                let autostart_off = source.disable_autostart(&project, &name).is_ok();
                let reason = moved_trouble(copy, autostart_off, target_name)
                    .map(|trouble| format!("moved, but {trouble}"));
                self.note(json!({
                    "event": "result", "at": self.now(), "move": id, "project": project, "name": name,
                    "result": "moved", "downtimeMs": downtime, "reason": reason,
                    "targetUuid": made.target_uuid, "autostartOff": autostart_off,
                    "copyAutostartSet": copy_set,
                }));
                if copy_set && autostart_off {
                    "moved"
                } else {
                    "moved_autostart_on"
                }
            }
            Err(reason) => {
                self.set_live(id, &project, &name, |progress| {
                    progress.phase = "rolling_back".to_string();
                });
                let rolled = rollback(&source, &dest, one, &made, target_name);
                self.clear_live(id, &project, &name);
                self.note(json!({
                    "event": "rollback", "at": self.now(), "move": id, "project": project, "name": name,
                    "reason": reason, "sourceIntact": rolled.source_intact,
                    "targetRemoved": rolled.target_removed, "sourceRunning": rolled.source_running,
                    "detail": rolled.detail,
                }));
                let (result, reason) = if rolled.clean {
                    ("rolled_back", reason)
                } else {
                    (
                        "failed",
                        format!(
                            "{reason}; the rollback did not finish, so the owner decides: {}",
                            rolled.detail.join("; ")
                        ),
                    )
                };
                // A source left stopped while its copy may run must not come back by itself on a restart of this host.
                let kept = if !rolled.clean && !rolled.source_running && !rolled.target_removed {
                    keep_down(&source, &project, &name, one["uuid"].as_str())
                } else {
                    KeptDown::NotOurs
                };
                self.note(json!({
                    "event": "result", "at": self.now(), "move": id, "project": project, "name": name,
                    "result": result, "downtimeMs": null, "reason": kept.reason(reason),
                    "autostartOff": kept.field(),
                }));
                result
            }
        }
    }

    /// The rounds, the cutover and the checks. Answers the measured downtime in milliseconds, or why it failed. `made`
    /// collects what the run made and did, for the rollback.
    fn attempt(
        &self,
        id: &str,
        plan: &Value,
        one: &Value,
        source: &MoveWriter,
        dest: &MoveWriter,
        made: &mut Made,
    ) -> Result<u64, String> {
        if is_vm(one) {
            return self.attempt_vm(id, plan, one, source, dest, made);
        }
        let project = one["project"].as_str().unwrap_or_default();
        let name = one["name"].as_str().unwrap_or_default();
        let pool = one["targetPool"].as_str().unwrap_or_default();
        let pre = one["rounds"]["preCopy"].as_u64().unwrap_or(DEFAULT_ROUNDS);
        let total_rounds = pre + 1;
        let target_name = plan["target"]["name"].as_str().unwrap_or("the target");
        let (mut addresses, certificate) = push_facts(plan, dest, target_name)?;
        let needed = one["bytes"]["needed"].as_u64().unwrap_or(0);
        let headroom = needed.saturating_sub(one["bytes"]["measured"].as_u64().unwrap_or(0));
        let addressed = nics_with_addresses(source, project, name);
        let round_room = |round: u64| if round == 1 { needed } else { headroom };
        // The rounds while it runs, not frozen.
        for round in 1..=pre {
            self.check_room(dest, pool, round_room(round), target_name)?;
            self.copy_round(
                id,
                one,
                (source, dest),
                &mut addresses,
                &certificate,
                (round, total_rounds),
                made,
            )?;
        }
        // The cutover. Whether the source runs is read now, not at the begin: it may have been started or stopped
        // during the rounds.
        let round = total_rounds;
        self.set_live(id, project, name, |progress| {
            progress.phase = "cutover".to_string();
            progress.round = round;
        });
        let status = source
            .get(&instance_path(project, name))
            .ok()
            .and_then(|doc| text(&doc, "status").map(str::to_string))
            .ok_or_else(|| "the source could not be read at the cutover".to_string())?;
        let before = self.before_stop(source, one);
        let began = Instant::now();
        match status.as_str() {
            "Running" => {
                stop(source, project, name)?;
                made.stopped_source = true;
            }
            "Stopped" => {}
            other => {
                return Err(format!(
                    "the source is {other} at the cutover, neither running nor stopped (Incus freezes a container for a snapshot on some pools): the move stops here, starts nothing beside it and deletes only its own copy"
                ));
            }
        }
        if !self.note(json!({
            "event": "cutover", "at": self.now(), "move": id, "project": project, "name": name,
            "step": "stopped", "wasRunning": made.stopped_source,
        })) {
            return Err(
                "the move journal could not record the stop, so the move stops here".to_string(),
            );
        }
        self.check_room(dest, pool, round_room(round), target_name)?;
        self.copy_round(
            id,
            one,
            (source, dest),
            &mut addresses,
            &certificate,
            (round, total_rounds),
            made,
        )?;
        let target_host = host_of(&addresses);
        self.start_and_check(
            id,
            plan,
            one,
            (source, dest),
            made,
            began,
            (&target_host, &addressed, &before),
        )
    }

    /// The end of an instance's move, once its last copy landed: the source read back Stopped, the copy started on the
    /// target and checked. Answers the downtime from `began`, or why it failed.
    #[allow(clippy::too_many_arguments)]
    fn start_and_check(
        &self,
        id: &str,
        plan: &Value,
        one: &Value,
        (source, dest): (&MoveWriter, &MoveWriter),
        made: &Made,
        began: Instant,
        (target_host, addressed, before): (&str, &[String], &Before),
    ) -> Result<u64, String> {
        let project = one["project"].as_str().unwrap_or_default();
        let name = one["name"].as_str().unwrap_or_default();
        let target_name = plan["target"]["name"].as_str().unwrap_or("the target");
        if made.target_uuid.is_none() {
            return Err(
                "the copy on the target was never recorded, so it is not started".to_string(),
            );
        }
        // Nothing may have started the source since the stop: read it back Stopped, or the copy stays off.
        let now = source
            .get(&instance_path(project, name))
            .ok()
            .and_then(|doc| text(&doc, "status").map(str::to_string))
            .unwrap_or_else(|| "unreadable".to_string());
        if now != "Stopped" {
            return Err(format!(
                "the source reads {now} just before the copy would start, not Stopped: the copy was not started"
            ));
        }
        if !self.note(json!({
            "event": "cutover", "at": self.now(), "move": id, "project": project, "name": name,
            "step": "starting",
        })) {
            return Err(
                "the move journal could not record the start of the copy, so the copy was not started".to_string(),
            );
        }
        dest.set_state(project, name, "start", false, START_DEADLINE)
            .map_err(|error| format!("the copy on {target_name} did not start: {error}"))?;
        self.note(json!({
            "event": "cutover", "at": self.now(), "move": id, "project": project, "name": name,
            "step": "started", "elapsedMs": began.elapsed().as_millis() as u64,
        }));
        self.set_live(id, project, name, |progress| {
            progress.phase = "verifying".to_string()
        });
        let checks = self.verify(source, dest, one, target_host, addressed, before);
        let ok = checks.iter().all(|check| check["ok"] == true);
        let downtime = began.elapsed().as_millis() as u64;
        let verified = self.note(json!({
            "event": "verify", "at": self.now(), "move": id, "project": project, "name": name,
            "ok": ok, "checks": checks, "downtimeMs": if ok { json!(downtime) } else { Value::Null },
            "targetUuid": made.target_uuid,
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
        if !verified {
            return Err("the move journal could not record the passed checks".to_string());
        }
        Ok(downtime)
    }

    /// A stopped virtual machine's move: no rounds while it runs (it does not), one consistent copy, by an Incus push or
    /// by Ceph as the plan says, then the start and the checks. Nothing here starts or stops the source: one that does not
    /// read Stopped now fails before anything is made.
    fn attempt_vm(
        &self,
        id: &str,
        plan: &Value,
        one: &Value,
        source: &MoveWriter,
        dest: &MoveWriter,
        made: &mut Made,
    ) -> Result<u64, String> {
        let project = one["project"].as_str().unwrap_or_default();
        let name = one["name"].as_str().unwrap_or_default();
        let pool = one["targetPool"].as_str().unwrap_or_default();
        let target_name = plan["target"]["name"].as_str().unwrap_or("the target");
        self.set_live(id, project, name, |progress| {
            progress.phase = "cutover".to_string();
            progress.round = 1;
        });
        let status = source
            .get(&instance_path(project, name))
            .ok()
            .and_then(|doc| text(&doc, "status").map(str::to_string))
            .ok_or_else(|| "the source could not be read".to_string())?;
        if status != "Stopped" {
            return Err(format!(
                "the virtual machine reads {status}, not Stopped: it moves only while it is stopped, so nothing was copied or made"
            ));
        }
        let began = Instant::now();
        if !self.note(json!({
            "event": "cutover", "at": self.now(), "move": id, "project": project, "name": name,
            "step": "stopped", "wasRunning": false,
        })) {
            return Err(
                "the move journal could not record the cutover, so the move stops here".to_string(),
            );
        }
        let needed = one["bytes"]["needed"].as_u64().unwrap_or(0);
        self.check_room(dest, pool, needed, target_name)?;
        let target_host = if one["copy"] == "ceph" {
            self.copy_by_ceph(id, one, (source, dest), made)?;
            String::new()
        } else {
            let (mut addresses, certificate) = push_facts(plan, dest, target_name)?;
            self.copy_round(
                id,
                one,
                (source, dest),
                &mut addresses,
                &certificate,
                (1, 1),
                made,
            )?;
            host_of(&addresses)
        };
        self.start_and_check(
            id,
            plan,
            one,
            (source, dest),
            made,
            began,
            (&target_host, &[], &Before::default()),
        )
    }

    /// Copies a stopped virtual machine and its custom volumes by Ceph. Every source image is read first: none may be
    /// held open (a machine that runs somewhere holds its disk), and each one's size is read. Then, each custom volume and
    /// then the machine: Incus on the target makes it empty, of its source's size; its uuid is journaled; the image Incus
    /// made is read back at that size; and `rbd export-diff | rbd import-diff` copies the source's into it. The source is
    /// only read.
    fn copy_by_ceph(
        &self,
        id: &str,
        one: &Value,
        (source, dest): (&MoveWriter, &MoveWriter),
        made: &mut Made,
    ) -> Result<(), String> {
        let project = one["project"].as_str().unwrap_or_default();
        let name = one["name"].as_str().unwrap_or_default();
        let started = Instant::now();
        self.set_live(id, project, name, |progress| {
            progress.done = 0;
            progress.processed = None;
            progress.speed = None;
        });
        let runner = self.ceph.as_ref();
        let ceph = &one["ceph"];
        let image = |pool: &str, side: &str, name: String| -> Result<RbdImage, String> {
            let facts = &ceph[pool][side];
            let field = |key: &str| {
                facts[key]
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| format!("the plan names no Ceph {key} for the pool {pool}"))
            };
            Ok(RbdImage {
                cluster: field("cluster")?,
                client: field("client")?,
                pool: field("pool")?,
                image: name,
            })
        };
        let what = format!("the virtual machine {project}/{name}");
        // The source as it is before anything is read: a start between here and the copy's end changes `last_used_at`.
        let doc = source
            .get(&instance_path(project, name))
            .map_err(|error| format!("{what} could not be read here: {error}"))?;
        let used_before = doc["last_used_at"].clone();
        // What to copy: each custom volume, then the machine's config image (its UEFI variables and TPM state), then its
        // own disk, each with its source image.
        let mut volumes = Vec::new();
        for volume in one["volumes"].as_array().into_iter().flatten() {
            let pool = volume["pool"].as_str().unwrap_or_default();
            let volume_name = volume["name"].as_str().unwrap_or_default();
            let here = volume["project"].as_str().unwrap_or("default");
            let what = format!("the custom volume {pool}/{volume_name}");
            let doc = source
                .get(&volume_path(pool, volume_name, here))
                .map_err(|error| format!("{what} could not be read here: {error}"))?;
            let block = doc["content_type"] == "block";
            let from = image(
                pool,
                "from",
                rbd::incus_image("custom", here, volume_name, block),
            )?;
            volumes.push((volume, doc, block, from, what));
        }
        let root_pool = one["sourcePool"].as_str().unwrap_or_default();
        let config = image(
            root_pool,
            "from",
            rbd::incus_image("virtual-machine", project, name, false),
        )?;
        let root = image(
            root_pool,
            "from",
            rbd::incus_image("virtual-machine", project, name, true),
        )?;
        let mut sizes = Vec::new();
        for from in volumes
            .iter()
            .map(|(_, _, _, from, _)| from)
            .chain([&config, &root])
        {
            let held = rbd::watchers(runner, from)
                .map_err(|why| format!("who holds {} could not be read: {why}", from.spec()))?;
            if !held.is_empty() {
                return Err(format!(
                    "the image {} is held open by {}: the virtual machine may run somewhere, so nothing was copied or made",
                    from.spec(),
                    held.join(", ")
                ));
            }
            sizes.push(
                rbd::size(runner, from).map_err(|why| {
                    format!("the size of {} could not be read: {why}", from.spec())
                })?,
            );
        }
        let planned = one["bytes"]["instance"].as_u64();
        let root_bytes = sizes.last().copied().unwrap_or(0);
        if planned.is_some_and(|planned| planned != root_bytes) {
            self.note(json!({
                "event": "sized", "at": self.now(), "move": id, "project": project, "name": name,
                "plannedBytes": planned, "rbdBytes": root_bytes,
                "detail": format!("{what}'s disk reads {root_bytes} bytes with rbd info, not the {} bytes the plan measured from Incus; the copy uses rbd's", planned.unwrap_or(0)),
            }));
        }
        let mut moved = 0u64;
        for ((volume, doc, block, from, what), bytes) in volumes.iter().zip(&sizes) {
            let pool = volume["pool"].as_str().unwrap_or_default();
            let volume_name = volume["name"].as_str().unwrap_or_default();
            let there = volume["targetProject"].as_str().unwrap_or("default");
            dest.create_empty_volume(there, pool, volume_name, doc, *bytes)
                .map_err(|error| format!("the target did not make {what}: {error}"))?;
            self.record(id, one, dest, made, Some(volume))?;
            let to = image(
                pool,
                "to",
                rbd::incus_image("custom", there, volume_name, *block),
            )?;
            self.copy_image(from, &to, *bytes, what)?;
            moved += bytes;
            self.set_live(id, project, name, |progress| progress.done = moved);
        }
        made.source_autostart = Some(
            doc["config"]["boot.autostart"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        );
        let (root_device, root_disk) = ["expanded_devices", "devices"]
            .iter()
            .filter_map(|key| doc[*key].as_object())
            .flat_map(|devices| devices.iter())
            .find(|(_, device)| is_root(device))
            .map(|(device_name, device)| (device_name.clone(), device.clone()))
            .ok_or_else(|| format!("{what} has no root disk device"))?;
        let config_bytes = sizes[sizes.len() - 2];
        let target_pool = ceph[root_pool]["targetPool"].as_str().unwrap_or(root_pool);
        dest.create_empty_vm(
            project,
            name,
            &doc,
            (&root_device, &root_disk),
            (target_pool, root_bytes),
        )
        .map_err(|error| format!("the target did not make {what}: {error}"))?;
        self.record(id, one, dest, made, None)?;
        let to_config = image(
            root_pool,
            "to",
            rbd::incus_image("virtual-machine", project, name, false),
        )?;
        self.copy_image(
            &config,
            &to_config,
            config_bytes,
            &format!("{what}'s config image (UEFI variables, TPM state)"),
        )?;
        moved += config_bytes;
        let to = image(
            root_pool,
            "to",
            rbd::incus_image("virtual-machine", project, name, true),
        )?;
        self.copy_image(&root, &to, root_bytes, &format!("{what}'s disk"))?;
        moved += root_bytes;
        // A start and a stop of the source while it was read leave a torn copy, though it reads stopped now.
        let after = source
            .get(&instance_path(project, name))
            .map_err(|error| format!("{what} could not be read here after the copy: {error}"))?;
        if after["last_used_at"] != used_before {
            return Err(format!(
                "{what} was started here while it was copied (its last start moved from {} to {}): the copy may be torn, so it is not used",
                used_before.as_str().unwrap_or("never"),
                after["last_used_at"].as_str().unwrap_or("unknown")
            ));
        }
        let elapsed = started.elapsed().as_millis() as u64;
        let rate = (elapsed > 0).then(|| moved.saturating_mul(1000) / elapsed.max(1));
        self.note(json!({
            "event": "round", "at": self.now(), "move": id, "project": project, "name": name,
            "round": 1, "of": 1, "refresh": false, "final": true, "copy": "ceph",
            "bytes": moved, "elapsedMs": elapsed, "rateBytesPerSecond": rate, "timeLeftMs": 0,
        }));
        Ok(())
    }

    /// Reads the image Incus made back at `bytes`, then copies `from` into it.
    fn copy_image(
        &self,
        from: &RbdImage,
        to: &RbdImage,
        bytes: u64,
        what: &str,
    ) -> Result<(), String> {
        let runner = self.ceph.as_ref();
        let made = rbd::size(runner, to).map_err(|why| {
            format!(
                "the image {} Incus made for {what} could not be read: {why}",
                to.spec()
            )
        })?;
        if made != bytes {
            return Err(format!(
                "the image {} Incus made for {what} is {made} bytes and the source's {bytes}: nothing was copied into it",
                to.spec()
            ));
        }
        rbd::copy(runner, from, to).map_err(|why| {
            format!(
                "the copy of {what} from {} into {} failed: {why}",
                from.spec(),
                to.spec()
            )
        })
    }

    /// Records an object the first round made on the target, as soon as it landed: the copy's uuid, or a custom
    /// volume's. The run does not go on without this line: a rollback, now or after a stop of the kernel, deletes on the
    /// target only what these lines name.
    fn record(
        &self,
        id: &str,
        one: &Value,
        dest: &MoveWriter,
        made: &mut Made,
        volume: Option<&Value>,
    ) -> Result<(), String> {
        let project = one["project"].as_str().unwrap_or_default();
        let name = one["name"].as_str().unwrap_or_default();
        let line = match volume {
            Some(volume) => {
                let doc = dest
                    .get(&volume_path(
                        volume["pool"].as_str().unwrap_or_default(),
                        volume["name"].as_str().unwrap_or_default(),
                        volume["targetProject"].as_str().unwrap_or("default"),
                    ))
                    .ok();
                let made_volume = json!({
                    "pool": volume["pool"], "name": volume["name"], "project": volume["targetProject"],
                    "uuid": doc.as_ref().and_then(uuid_of),
                });
                made.volumes.push(made_volume.clone());
                json!({
                    "event": "created", "at": self.now(), "move": id, "project": project, "name": name,
                    "volume": made_volume,
                })
            }
            None => {
                made.target_uuid = dest
                    .get(&instance_path(project, name))
                    .ok()
                    .as_ref()
                    .and_then(uuid_of);
                if made.target_uuid.is_none() {
                    return Err("the copy on the target has no uuid to record".to_string());
                }
                json!({
                    "event": "created", "at": self.now(), "move": id, "project": project, "name": name,
                    "targetUuid": made.target_uuid, "sourceAutostart": made.source_autostart,
                })
            }
        };
        if self.note(line) {
            Ok(())
        } else {
            Err(
                "the move journal could not record the copy it made, so the move stops here"
                    .to_string(),
            )
        }
    }

    /// What the source looks like just before its stop: its spot-check file (a file on a custom volume can be read only
    /// while it runs) and the TCP ports of its proxies on every address that answer on this host.
    fn before_stop(&self, source: &MoveWriter, one: &Value) -> Before {
        let project = one["project"].as_str().unwrap_or_default();
        let name = one["name"].as_str().unwrap_or_default();
        let path = one["spotCheck"].as_str().unwrap_or(DEFAULT_SPOT_CHECK);
        let mut before = Before {
            spot: source.read_file(project, name, path).ok(),
            ..Before::default()
        };
        let doc = source.get(&instance_path(project, name)).ok();
        for (device_name, device) in doc.as_ref().map(devices).unwrap_or_default() {
            if text(&device, "type") != Some("proxy") {
                continue;
            }
            let listen = text(&device, "listen").unwrap_or("?");
            match wildcard(listen) {
                Some(port) if answers("127.0.0.1", port) => before.ports.push(port),
                Some(port) => before.unprobed.push(format!(
                    "{device_name} ({port} did not answer here before the stop)"
                )),
                None => before.unprobed.push(format!(
                    "{device_name} (listens on {listen}, not on every address)"
                )),
            }
        }
        before
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

    /// One round: each custom volume, then the instance, pushed by the source's Incus to the target's. The rounds while
    /// it runs do not freeze it; the last one, after the stop, is consistent. Journals the round with what it moved, its
    /// rate, how long it took and the time left.
    #[allow(clippy::too_many_arguments)]
    fn copy_round(
        &self,
        id: &str,
        one: &Value,
        (source, dest): (&MoveWriter, &MoveWriter),
        addresses: &mut Vec<String>,
        certificate: &str,
        (round, of): (u64, u64),
        made: &mut Made,
    ) -> Result<(), String> {
        let project = one["project"].as_str().unwrap_or_default();
        let name = one["name"].as_str().unwrap_or_default();
        let refresh = round > 1;
        let last = round == of;
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
            let here = volume["project"].as_str().unwrap_or("default");
            let there = volume["targetProject"].as_str().unwrap_or(here);
            let what = format!("the custom volume {pool}/{volume_name}");
            let doc = source
                .get(&volume_path(pool, volume_name, here))
                .map_err(|error| format!("{what} could not be read here: {error}"))?;
            let receive = dest
                .receive_volume(there, pool, volume_name, &doc, refresh)
                .map_err(|error| format!("the target did not take {what}: {error}"))?;
            let push = |to: &PushTarget| source.push_volume(here, pool, volume_name, to);
            let landed =
                |dest: &MoveWriter| dest.get(&volume_path(pool, volume_name, there)).is_ok();
            moved += self.push_to(
                (id, project, name),
                source,
                dest,
                addresses,
                certificate,
                &receive,
                &what,
                push,
                landed,
            )?;
            if !refresh {
                self.record(id, one, dest, made, Some(volume))?;
            }
            self.set_live(id, project, name, |progress| {
                progress.done = moved;
                progress.processed = None;
            });
        }
        let kind = if is_vm(one) {
            "virtual machine"
        } else {
            "container"
        };
        let what = format!("the {kind} {project}/{name}");
        let doc = source
            .get(&instance_path(project, name))
            .map_err(|error| format!("{what} could not be read here: {error}"))?;
        if made.source_autostart.is_none() {
            made.source_autostart = Some(
                doc["config"]["boot.autostart"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            );
        }
        let receive = dest
            .receive_instance(project, name, &doc, refresh)
            .map_err(|error| format!("the target did not take {what}: {error}"))?;
        let push = |to: &PushTarget| source.push_instance(project, name, to, last);
        let landed = |dest: &MoveWriter| dest.get(&instance_path(project, name)).is_ok();
        moved += self.push_to(
            (id, project, name),
            source,
            dest,
            addresses,
            certificate,
            &receive,
            &what,
            push,
            landed,
        )?;
        if !refresh {
            self.record(id, one, dest, made, None)?;
        }
        let elapsed = started.elapsed().as_millis() as u64;
        let rate = (elapsed > 0).then(|| moved.saturating_mul(1000) / elapsed.max(1));
        self.note(json!({
            "event": "round", "at": self.now(), "move": id, "project": project, "name": name,
            "round": round, "of": of, "refresh": refresh, "final": last,
            "bytes": moved, "elapsedMs": elapsed, "rateBytesPerSecond": rate,
            "timeLeftMs": if last { json!(0) } else { Value::Null },
        }));
        Ok(())
    }

    /// Pushes one object to its receiving operation, trying each of the target's addresses in order, as Incus's own
    /// client does: an address that does not connect leaves the receiving operation waiting, and the next one is tried.
    /// The address that worked goes first for the next objects. Answers the bytes Incus said it moved. On a failure it
    /// waits for the receiving operation to end before answering, so a rollback never deletes what an operation still
    /// holds.
    #[allow(clippy::too_many_arguments)]
    fn push_to(
        &self,
        who: (&str, &str, &str),
        source: &MoveWriter,
        dest: &MoveWriter,
        addresses: &mut Vec<String>,
        certificate: &str,
        receive: &Receive,
        what: &str,
        push: impl Fn(&PushTarget) -> Result<String, IncusError>,
        landed: impl Fn(&MoveWriter) -> bool,
    ) -> Result<u64, String> {
        let mut failures = Vec::new();
        for (index, address) in addresses.clone().iter().enumerate() {
            let to = PushTarget {
                address,
                certificate,
                receive,
            };
            let outcome = match push(&to) {
                Ok(operation) => self.follow_push(who, source, &operation, what),
                Err(error) => Err(format!("this host did not push {what}: {error}")),
            };
            match outcome {
                Ok(bytes) => {
                    self.follow_receive(dest, &receive.operation, what, &landed)?;
                    if index > 0 {
                        let worked = addresses.remove(index);
                        addresses.insert(0, worked);
                    }
                    return Ok(bytes);
                }
                Err(error) => {
                    failures.push(format!("{address}: {error}"));
                    // The receiving side still waits: the push never reached it, so the next address may.
                    if matches!(
                        dest.wait(&receive.operation, Duration::from_millis(1)),
                        Ok(OpState::Running(_))
                    ) {
                        continue;
                    }
                    break;
                }
            }
        }
        self.settle(dest, &receive.operation);
        Err(format!(
            "the push of {what} failed: {}",
            failures.join("; ")
        ))
    }

    /// Waits, at most the verify wait, for a receiving operation to end.
    fn settle(&self, dest: &MoveWriter, operation: &str) {
        let until = Instant::now() + self.verify_wait;
        while Instant::now() < until {
            match dest.wait(operation, self.poll) {
                Ok(OpState::Running(_)) => {
                    std::thread::sleep(self.poll.min(Duration::from_millis(200)))
                }
                _ => return,
            }
        }
    }

    /// Follows the source's push to its end, keeping its progress readable. Answers the bytes Incus said it moved (0
    /// when it did not say). An operation Incus no longer knows is not taken as a success here: the receiving side
    /// decides.
    fn follow_push(
        &self,
        (id, project, name): (&str, &str, &str),
        source: &MoveWriter,
        push: &str,
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
                    return Ok(bytes);
                }
                Ok(OpState::Gone) => return Ok(bytes),
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
    }

    /// Follows the target's receiving operation to its end. An operation Incus no longer knows counts only when the
    /// object is read back on the target.
    fn follow_receive(
        &self,
        dest: &MoveWriter,
        receiving: &str,
        what: &str,
        landed: &impl Fn(&MoveWriter) -> bool,
    ) -> Result<(), String> {
        let until = Instant::now() + ROUND_DEADLINE;
        loop {
            match dest.wait(receiving, self.poll) {
                Ok(OpState::Done(_)) => return Ok(()),
                Ok(OpState::Gone) if landed(dest) => return Ok(()),
                Ok(OpState::Gone) => {
                    return Err(format!(
                        "the target no longer knows the operation that took {what}, and it is not there"
                    ));
                }
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
        target_host: &str,
        addressed: &[String],
        before: &Before,
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
        let path_of = instance_path(project, name);
        // Running now; the check is written first, once the copy is read again after the others.
        let first = dest
            .get(&state_path)
            .ok()
            .and_then(|state| text(&state, "status").map(str::to_string))
            .unwrap_or_else(|| "not readable".to_string());
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
        // Ports: each TCP proxy on every address that answered on this host before the stop answers on the target.
        let mut closed = Vec::new();
        for port in &before.ports {
            let mut open = false;
            loop {
                if answers(target_host, *port) {
                    open = true;
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
        let unprobed = if before.unprobed.is_empty() {
            String::new()
        } else {
            format!("; not probed: {}", before.unprobed.join(", "))
        };
        let ports: Vec<String> = before.ports.iter().map(u16::to_string).collect();
        checks.push(json!({
            "check": "ports", "ok": closed.is_empty(),
            "detail": if before.ports.is_empty() {
                format!("no TCP proxy on every address answered here before the stop, so none is required{unprobed}")
            } else if closed.is_empty() {
                format!("port(s) {} answer on {target_host}{unprobed}", ports.join(", "))
            } else {
                format!("port(s) {} answered here before the stop and do not answer on {target_host}{unprobed}", closed.join(", "))
            },
        }));
        // A container has no guest agent. A virtual machine's guest must answer, given five times the checks' wait to
        // boot: its Incus agent (processes counted), or one of its addresses answering a TCP connection from this node,
        // accepted or refused. An address alone proves nothing: without the agent Incus still lists the address it derives
        // from the card's MAC on an IPv6 bridge, the MAC's leases and OVN's address, for a guest that never booted.
        let vm = is_vm(one);
        if vm {
            let answer_until = Instant::now() + self.verify_wait * VM_ANSWER_FACTOR;
            let mut answered = None;
            loop {
                if let Ok(state) = dest.get(&state_path) {
                    if state["processes"].as_i64().is_some_and(|count| count > 0) {
                        answered = Some("its Incus agent answers".to_string());
                    } else if let Some((card, address)) = global_addresses(&state)
                        .into_iter()
                        .find(|(_, address)| guest_answers(address))
                    {
                        answered = Some(format!(
                            "{card}'s address {address} answers a connection from this node (port {GUEST_PROBE_PORT})"
                        ));
                    }
                }
                if answered.is_some() || Instant::now() >= answer_until {
                    break;
                }
                std::thread::sleep(self.poll.min(Duration::from_millis(500)));
            }
            checks.push(json!({
                "check": "guestAgent", "ok": answered.is_some(),
                "detail": match answered {
                    Some(how) => format!("the guest answers: {how}"),
                    None => format!(
                        "the guest did not answer within {} s: no Incus agent, and none of its addresses answered a connection from this node (port {GUEST_PROBE_PORT}); an address Incus lists without the agent does not count",
                        (self.verify_wait * VM_ANSWER_FACTOR).as_secs()
                    ),
                },
            }));
        } else {
            checks.push(json!({
                "check": "guestAgent", "ok": true,
                "detail": "a container has no guest agent: not applicable",
            }));
        }
        // The custom volumes are there and attached.
        let doc = dest.get(&path_of).ok();
        let mut detached = Vec::new();
        for volume in one["volumes"].as_array().into_iter().flatten() {
            let pool = volume["pool"].as_str().unwrap_or_default();
            let volume_name = volume["name"].as_str().unwrap_or_default();
            let there = dest
                .get(&volume_path(
                    pool,
                    volume_name,
                    volume["targetProject"].as_str().unwrap_or("default"),
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
                format!("the {} has no custom volume", if vm { "virtual machine" } else { "container" })
            } else if detached.is_empty() {
                format!("{volumes} custom volume(s) on the target and attached")
            } else {
                format!("not there or not attached: {}", detached.join(", "))
            },
        }));
        // The spot-check file is the same on both sides: the source's as read after the stop, or, for a file on a
        // custom volume (a stopped container's volumes are not mounted), as read just before it. A stopped virtual
        // machine's files cannot be read: its guest's answer stands for this check.
        let path = one["spotCheck"].as_str().unwrap_or(DEFAULT_SPOT_CHECK);
        let (reference, when) = match source.read_file(project, name, path) {
            _ if vm => (None, ""),
            Ok(after) => (Some(after), "after the stop"),
            Err(_) => (before.spot.clone(), "just before the stop"),
        };
        let (ok, detail) = match reference {
            None if vm => (
                true,
                "a stopped virtual machine's files cannot be read, so there is nothing to compare: its guest's answer stands for it".to_string(),
            ),
            None => (
                false,
                format!(
                    "{path} could not be read on the source, before or after the stop, so the check cannot pass"
                ),
            ),
            Some(here) => match dest.read_file(project, name, path) {
                Ok(there) if there == here => (
                    true,
                    format!(
                        "{path} is the same on both ({}; the source's read {when})",
                        &Digest::of(&here).to_hex()[..12]
                    ),
                ),
                Ok(_) => (
                    false,
                    format!("{path} differs between the source ({when}) and the copy"),
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
            .get(&path_of)
            .ok()
            .and_then(|doc| text(&doc, "status").map(str::to_string))
            .unwrap_or_else(|| "not readable".to_string());
        let ok = first == "Running" && last == "Running";
        checks.insert(
            0,
            json!({
                "check": "running", "ok": ok,
                "detail": if ok {
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

    /// Settles the moves a stopped kernel left unfinished, in the background, once, when the node starts. A move that
    /// the next start has not settled yet holds its instances: a new move of one is refused until it has.
    pub fn recover(self: &Arc<Self>) {
        let lines = self.journal.lines();
        let unfinished: Vec<MoveRecord> = fold(&lines)
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
        let stopped = "the kernel stopped during the move";
        let target = linked_target(&self.cfg, &record.target);
        let source = self.source();
        let mut results: Vec<&str> = Vec::new();
        for instance in &record.instances {
            if let Some(result) = instance.result.as_deref() {
                results.push(match result {
                    "moved" => "moved",
                    "rolled_back" => "rolled_back",
                    _ => "failed",
                });
                continue;
            }
            // The checks passed and were journaled: the copy is the moved instance. Nothing is deleted or started.
            if instance.verified {
                // As at the end of a run: the copy takes the source's boot.autostart (as its creation recorded it, or
                // as the source reads now), and the kept source's is turned off.
                let value = instance.source_autostart.clone().or_else(|| {
                    source
                        .get(&instance_path(&instance.project, &instance.name))
                        .ok()
                        .map(|doc| {
                            doc["config"]["boot.autostart"]
                                .as_str()
                                .unwrap_or_default()
                                .to_string()
                        })
                });
                // Only the copy this move recorded is written: another instance of the name on the target (another
                // uuid, or a copy with no recorded uuid) is left alone, and the copy's key is reported not set.
                let copy = match (&target, &value) {
                    (Ok(target), Some(value)) => {
                        let dest = MoveWriter::new(target.socket.clone());
                        let ours = instance.target_uuid.is_some()
                            && dest
                                .get(&instance_path(&instance.project, &instance.name))
                                .ok()
                                .as_ref()
                                .and_then(uuid_of)
                                == instance.target_uuid;
                        if !ours {
                            CopyAutostart::NotTheCopy
                        } else if dest
                            .set_autostart(&instance.project, &instance.name, value)
                            .is_ok()
                        {
                            CopyAutostart::Set
                        } else {
                            CopyAutostart::Failed
                        }
                    }
                    _ => CopyAutostart::Failed,
                };
                let copy_set = copy == CopyAutostart::Set;
                let autostart_off = source
                    .disable_autostart(&instance.project, &instance.name)
                    .is_ok();
                let reason = format!(
                    "{stopped} after the copy passed its checks; the next start recorded it as moved"
                );
                let reason = match moved_trouble(copy, autostart_off, &record.target) {
                    Some(trouble) => format!("{reason}, but {trouble}"),
                    None => reason,
                };
                self.note(json!({
                    "event": "result", "at": self.now(), "move": record.id, "project": instance.project,
                    "name": instance.name, "result": "moved", "downtimeMs": instance.verify_downtime,
                    "reason": reason, "targetUuid": instance.target_uuid, "recovered": true,
                    "autostartOff": autostart_off, "copyAutostartSet": copy_set,
                }));
                results.push(if copy_set && autostart_off {
                    "moved"
                } else {
                    "moved_autostart_on"
                });
                continue;
            }
            // The copy was started, and may have served writes: nothing is deleted, and the source is not started next
            // to it. The owner decides.
            if instance.copy_started {
                let reason = format!(
                    "{stopped} after the copy was started on {target} and before its checks were journaled: the copy may have served writes, so it was left as it is and the source stays stopped. Keep one of them: to come back, stop the copy on {target} and start the source here; to keep the copy, leave the source stopped. Until the copy on {target} is stopped or gone, no other move takes this instance",
                    target = record.target
                );
                let kept = keep_down(
                    &source,
                    &instance.project,
                    &instance.name,
                    instance.uuid.as_deref(),
                );
                self.note(json!({
                    "event": "result", "at": self.now(), "move": record.id, "project": instance.project,
                    "name": instance.name, "result": "failed", "downtimeMs": null, "reason": kept.reason(reason),
                    "targetUuid": instance.target_uuid, "recovered": true, "autostartOff": kept.field(),
                }));
                results.push("failed");
                continue;
            }
            let one = json!({
                "project": instance.project, "name": instance.name, "uuid": instance.uuid,
                "volumes": instance.volumes,
            });
            let made = Made {
                target_uuid: instance.target_uuid.clone(),
                volumes: instance.created_volumes.clone(),
                stopped_source: instance.stopped_source,
                source_autostart: instance.source_autostart.clone(),
            };
            let rolled = match &target {
                Ok(target) => rollback(
                    &source,
                    &MoveWriter::new(target.socket.clone()),
                    &one,
                    &made,
                    &record.target,
                ),
                // Nothing was read on the target; the source is read as it stands, so one the move never stopped is
                // not taken for stopped.
                Err(why) => RolledBack {
                    source_intact: false,
                    target_removed: false,
                    source_running: source
                        .get(&instance_path(&instance.project, &instance.name))
                        .is_ok_and(|doc| running(&doc)),
                    clean: false,
                    detail: vec![format!(
                        "the target was not reached ({why}), so nothing was deleted or started"
                    )],
                },
            };
            let reason = format!("{stopped}; the next start rolled it back");
            self.note(json!({
                "event": "rollback", "at": self.now(), "move": record.id, "project": instance.project,
                "name": instance.name, "reason": reason, "sourceIntact": rolled.source_intact,
                "targetRemoved": rolled.target_removed, "sourceRunning": rolled.source_running,
                "detail": rolled.detail, "interrupted": true,
            }));
            let (result, reason) = if rolled.clean {
                ("rolled_back", reason)
            } else {
                (
                    "failed",
                    format!(
                        "{reason}, but it did not finish, so the owner decides: {}",
                        rolled.detail.join("; ")
                    ),
                )
            };
            let kept = if !rolled.clean && !rolled.source_running && !rolled.target_removed {
                keep_down(
                    &source,
                    &instance.project,
                    &instance.name,
                    instance.uuid.as_deref(),
                )
            } else {
                KeptDown::NotOurs
            };
            self.note(json!({
                "event": "result", "at": self.now(), "move": record.id, "project": instance.project,
                "name": instance.name, "result": result, "downtimeMs": null, "reason": kept.reason(reason),
                "autostartOff": kept.field(),
            }));
            results.push(result);
        }
        let moved = results
            .iter()
            .filter(|result| result.starts_with("moved"))
            .count();
        self.note(json!({
            "event": "end", "at": self.now(), "move": record.id, "moved": moved,
            "of": record.instances.len(), "interrupted": true,
        }));
        self.late_result(record.proposal.as_deref(), &results);
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

fn refs_pairs(plan: &Value) -> Vec<MoveRef> {
    plan["instances"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|one| {
            (
                one["project"].as_str().unwrap_or_default().to_string(),
                one["name"].as_str().unwrap_or_default().to_string(),
                one["uuid"].as_str().map(str::to_string),
            )
        })
        .collect()
}

/// The addresses the target's Incus listens on and its certificate, read now: the certificate must be the one the plan
/// named, and the target must still listen.
fn push_facts(
    plan: &Value,
    dest: &MoveWriter,
    target_name: &str,
) -> Result<(Vec<String>, String), String> {
    let server = dest
        .get("/1.0")
        .map_err(|error| format!("{target_name}'s Incus could not be read: {error}"))?;
    let (addresses, certificate) = transport_facts(&server);
    let Some(certificate) = certificate.filter(|_| !addresses.is_empty()) else {
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
    Ok((addresses, certificate))
}

/// The host part of the first address (`host:port`, `[v6]:port`), the one the checks probe.
fn host_of(addresses: &[String]) -> String {
    addresses
        .first()
        .and_then(|address| address.rsplit_once(':'))
        .map(|(host, _)| {
            host.trim_start_matches('[')
                .trim_end_matches(']')
                .to_string()
        })
        .unwrap_or_default()
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

/// The port a virtual machine's address is asked on when its Incus agent does not answer.
const GUEST_PROBE_PORT: u16 = 22;

/// Every global address on an instance's cards but `lo`, with its card, in card order.
fn global_addresses(state: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (card, value) in state["network"].as_object().into_iter().flatten() {
        if card == "lo" {
            continue;
        }
        for address in value["addresses"].as_array().into_iter().flatten() {
            if let Some(text) = address["address"].as_str().filter(|a| !a.is_empty())
                && address["scope"] == "global"
            {
                out.push((card.clone(), text.to_string()));
            }
        }
    }
    out
}

/// Whether the guest behind `address` answers a TCP connection from this node: accepted, or refused by its own stack.
/// A timeout or an unreachable network is no answer.
fn guest_answers(address: &str) -> bool {
    let Ok(ip) = address.parse::<std::net::IpAddr>() else {
        return false;
    };
    match TcpStream::connect_timeout(
        &SocketAddr::new(ip, GUEST_PROBE_PORT),
        Duration::from_millis(800),
    ) {
        Ok(_) => true,
        Err(error) => error.kind() == std::io::ErrorKind::ConnectionRefused,
    }
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

/// Deletes with `delete`, trying again while Incus answers that an operation still holds the object.
fn delete_patiently(delete: impl Fn() -> Result<(), IncusError>) -> Result<(), IncusError> {
    let until = Instant::now() + BUSY_RETRY;
    loop {
        match delete() {
            Err(IncusError::Status { message, .. })
                if message.to_lowercase().contains("busy") && Instant::now() < until =>
            {
                std::thread::sleep(Duration::from_millis(200));
            }
            other => return other,
        }
    }
}

/// What a rollback did, and whether it left the source as it was before the move with no copy of this move running.
struct RolledBack {
    source_intact: bool,
    target_removed: bool,
    source_running: bool,
    clean: bool,
    detail: Vec<String>,
}

/// Rolls one instance back, under the rule: no copy is deleted unless the source is read back as the plan's instance,
/// nothing on the target is deleted unless this move recorded making it (its uuid), and the source is started again
/// only when this move stopped it and the copy is read back gone or stopped. The source's own volumes are never touched.
fn rollback(
    source: &MoveWriter,
    dest: &MoveWriter,
    one: &Value,
    made: &Made,
    target_name: &str,
) -> RolledBack {
    let project = one["project"].as_str().unwrap_or_default();
    let name = one["name"].as_str().unwrap_or_default();
    let mut detail = Vec::new();
    let path = instance_path(project, name);
    // The source, read back: is it still the instance the plan named?
    let source_doc = source.get(&path).ok();
    let source_intact = source_doc
        .as_ref()
        .and_then(uuid_of)
        .is_some_and(|uuid| Some(uuid.as_str()) == one["uuid"].as_str());
    if !source_intact {
        detail.push(format!(
            "the source {project}/{name} was not read back here as the plan's instance, so any copy on {target_name} was kept: it may be the only one"
        ));
    }
    // The copy on the target: only the one this move made, and only while the source is intact.
    let (target_removed, copy_quiet) = match dest.get(&path) {
        Err(IncusError::Status { code: 404, .. }) => {
            detail.push("no copy was left on the target".to_string());
            (true, true)
        }
        Err(error) => {
            detail.push(format!(
                "the target could not be read ({error}), so the source was not started next to a copy that may run"
            ));
            (false, false)
        }
        Ok(doc) if made.target_uuid.is_none() || uuid_of(&doc) != made.target_uuid => {
            detail.push(format!(
                "{target_name}'s {project}/{name} is not a copy this move recorded making, so it was left alone"
            ));
            // Not a copy this move made. The source may run beside it only when the move made no copy at all (a
            // copy whose uuid was never recorded may be this one).
            (
                made.target_uuid.is_some(),
                made.target_uuid.is_some() && !running(&doc),
            )
        }
        Ok(doc) if !source_intact => (false, !running(&doc)),
        Ok(doc) => {
            if running(&doc) {
                let _ = dest.set_state(project, name, "stop", true, STOP_GRACE);
            }
            match delete_patiently(|| dest.delete_instance(project, name)) {
                Ok(()) => {
                    detail.push("the copy on the target was deleted".to_string());
                    (true, true)
                }
                Err(error) => {
                    detail.push(format!(
                        "the copy on the target could not be deleted: {error}"
                    ));
                    let quiet = dest.get(&path).ok().is_some_and(|doc| !running(&doc));
                    (false, quiet)
                }
            }
        }
    };
    // The volumes this move made on the target, each while the source's own is read back.
    let mut volumes_clean = true;
    for volume in &made.volumes {
        let pool = volume["pool"].as_str().unwrap_or_default();
        let volume_name = volume["name"].as_str().unwrap_or_default();
        let there = volume["project"].as_str().unwrap_or("default");
        let here = one["volumes"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|planned| planned["pool"] == pool && planned["name"] == volume_name)
            .and_then(|planned| planned["project"].as_str())
            .unwrap_or("default");
        let Ok(doc) = dest.get(&volume_path(pool, volume_name, there)) else {
            continue;
        };
        if volume["uuid"].is_string() && uuid_of(&doc).as_deref() != volume["uuid"].as_str() {
            detail.push(format!(
                "{target_name}'s volume {pool}/{volume_name} is not the copy this move made, so it was left alone"
            ));
            continue;
        }
        if !source_intact || source.get(&volume_path(pool, volume_name, here)).is_err() {
            detail.push(format!(
                "the source's volume {pool}/{volume_name} was not read back here, so its copy on {target_name} was kept"
            ));
            volumes_clean = false;
            continue;
        }
        match delete_patiently(|| dest.delete_volume(there, pool, volume_name)) {
            Ok(()) => detail.push(format!(
                "the copy of the volume {pool}/{volume_name} was deleted"
            )),
            Err(error) => {
                volumes_clean = false;
                detail.push(format!(
                    "the copy of the volume {pool}/{volume_name} could not be deleted: {error}"
                ));
            }
        }
    }
    // The source again: only when this move stopped it, it is the plan's instance, and no copy of it may run.
    let mut source_running = source_doc.as_ref().is_some_and(running);
    if made.stopped_source && source_intact && !source_running {
        if copy_quiet {
            let _ = source.set_state(project, name, "start", false, START_DEADLINE);
            source_running = source.get(&path).ok().is_some_and(|doc| running(&doc));
            detail.push(if source_running {
                "the source runs again".to_string()
            } else {
                "the source did not start again".to_string()
            });
        } else {
            detail.push(
                "the source was not started: a copy of it may still run on the target".to_string(),
            );
        }
    } else if !made.stopped_source && source_intact {
        detail.push(if source_running {
            "the source was not stopped by the move and runs".to_string()
        } else {
            "the source was not running when the move would have stopped it, and stays stopped"
                .to_string()
        });
    }
    let clean = source_intact
        && target_removed
        && volumes_clean
        && (source_running || !made.stopped_source);
    RolledBack {
        source_intact,
        target_removed,
        source_running,
        clean,
        detail,
    }
}

/// One move as its journal lines tell it.
#[derive(Debug, Clone, Default)]
struct MoveRecord {
    id: String,
    plan: String,
    target: String,
    proposal: Option<String>,
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
    uuid: Option<String>,
    rounds_total: u64,
    rounds_done: u64,
    bytes_total: Option<u64>,
    bytes_moved: u64,
    last_rate: Option<u64>,
    needed: u64,
    target_pool: String,
    volumes: Value,
    target_uuid: Option<String>,
    /// The source's own `boot.autostart` as the copy's creation recorded it ("" for unset).
    source_autostart: Option<String>,
    /// The rollback journaled the copy on the target removed (deleted, or read back gone).
    target_removed: bool,
    created_volumes: Vec<Value>,
    stopped_source: bool,
    copy_started: bool,
    /// The copy went by Ceph: the source's snapshots were not copied and live only on the source.
    by_ceph: bool,
    verified: bool,
    verify_downtime: Option<u64>,
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
                    uuid: one["uuid"].as_str().map(str::to_string),
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
                    proposal: line["proposal"].as_str().map(str::to_string),
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
                instance.by_ceph |= line["copy"] == "ceph";
            }
            Some("created") => {
                if let Some(uuid) = line["targetUuid"].as_str() {
                    instance.target_uuid = Some(uuid.to_string());
                }
                if let Some(value) = line["sourceAutostart"].as_str() {
                    instance.source_autostart = Some(value.to_string());
                }
                if line["volume"].is_object() {
                    instance.created_volumes.push(line["volume"].clone());
                }
            }
            Some("cutover") => {
                instance.phase = "cutover".to_string();
                match line["step"].as_str() {
                    Some("stopped") => instance.stopped_source = line["wasRunning"] == true,
                    Some("starting" | "started") => instance.copy_started = true,
                    _ => {}
                }
            }
            Some("verify") => {
                instance.checks = line["checks"].as_array().cloned().unwrap_or_default();
                instance.phase = "verifying".to_string();
                instance.verified = line["ok"] == true;
                instance.verify_downtime = line["downtimeMs"].as_u64();
            }
            Some("rollback") => {
                instance.phase = "rolling_back".to_string();
                instance.target_removed |= line["targetRemoved"] == true;
                record.interrupted |= line["interrupted"] == true;
            }
            Some("result") => {
                instance.result = line["result"].as_str().map(str::to_string);
                instance.phase = instance.result.clone().unwrap_or_default();
                instance.downtime = line["downtimeMs"].as_u64();
                instance.reason = line["reason"].as_str().map(str::to_string);
                instance.ended_at = Some(at);
                if let Some(uuid) = line["targetUuid"].as_str() {
                    instance.target_uuid = Some(uuid.to_string());
                }
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
        if self.ended_at.is_none() {
            return (
                false,
                "the move still runs; it is offered only after its checks pass".to_string(),
                Vec::new(),
            );
        }
        let moved: Vec<&InstanceRecord> = self
            .instances
            .iter()
            .filter(|one| one.result.as_deref() == Some("moved"))
            .collect();
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
            deletes.push(if one.by_ceph {
                format!(
                    "the source instance {}/{} on this host, with its snapshots: they were not copied (a copy by Ceph takes none), so they are gone with it",
                    one.project, one.name
                )
            } else {
                format!(
                    "the source instance {}/{} on this host, with its snapshots",
                    one.project, one.name
                )
            });
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
            format!(
                "the moved copies passed their checks: the owner may finish the move with a card; each source is deleted only once its copy is read back on {}",
                self.target
            ),
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
        let results: Vec<Option<&str>> = self
            .instances
            .iter()
            .map(|one| one.result.as_deref())
            .collect();
        let phase = if self.finished.is_some() {
            "finished"
        } else if self.ended_at.is_some() {
            if results.contains(&Some("failed")) {
                "needs_owner"
            } else if results.iter().all(|result| *result == Some("moved")) {
                "moved"
            } else if results.contains(&Some("moved")) {
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
                let current =
                    progress.map(|progress| progress.done + progress.processed.unwrap_or(0));
                let bytes_moved = match (progress, one.rounds_done) {
                    (Some(p), 0) if p.processed.is_none() && p.done == 0 => None,
                    _ => Some(one.bytes_moved + current.unwrap_or(0)),
                };
                let rate = progress.and_then(|p| p.speed).or(one.last_rate);
                let time_left = if one.result.is_some() {
                    Some(0)
                } else {
                    match (progress, rate, one.bytes_total) {
                        (Some(p), Some(rate), Some(total)) if p.round == 1 && rate > 0 => Some(
                            total
                                .saturating_sub(current.unwrap_or(0))
                                .saturating_mul(1000)
                                / rate,
                        ),
                        _ => None,
                    }
                };
                let phase = progress
                    .map(|p| p.phase.clone())
                    .filter(|phase| !phase.is_empty())
                    .unwrap_or_else(|| one.phase.clone());
                let finished = self.finished.as_ref().is_some_and(|line| {
                    line["deleted"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|deleted| {
                            deleted.as_str()
                                == Some(format!("{}/{}", one.project, one.name).as_str())
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
        let plan =
            match planned.plan {
                Some(plan) => Some(self.moves.plans.save(plan, self.moves.now()).map_err(
                    |error| {
                        eprintln!("arca: a move plan could not be kept: {error}");
                        internal()
                    },
                )?),
                None => None,
            };
        let (digest, expires) = plan.as_ref().map_or((Value::Null, Value::Null), |plan| {
            (plan["digest"].clone(), plan["expiresAt"].clone())
        });
        let message = if planned.go {
            format!(
                "planned {} instance(s) to {target}: propose it with action_propose instance_move and the digest; the plan expires an hour after it was first made",
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
            "plan": plan,
            "blocked": planned.blocked,
            "preflight": planned.preflight,
            "message": message,
        }))
    }
}

/// The approved plan the arguments of an `instance_move` name, checked against them: it is there, it has not expired,
/// and its instances and target are the arguments'. Answers the plan.
fn approved_plan(plans: &Plans, args: &Value, now: u64) -> Result<Value, CallError> {
    let digest = args["plan"].as_str().unwrap_or_default();
    let plan = plans.get(digest).ok_or_else(|| {
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
            let plan = approved_plan(&Plans::at(&cfg.state_dir), args, now)?;
            let probe = |target: &str, project: &str, name: &str, uuid: &str| {
                probe_copy(cfg, target, project, name, uuid)
            };
            if let Some(refusal) = start_refusal(
                &journal.lines(),
                plan["digest"].as_str().unwrap_or_default(),
                &refs_pairs(&plan),
                &probe,
            ) {
                return Err(refusal);
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
                format!(
                    "{}, where each copy passed its checks; a source is deleted only when its copy is read back there, and kept while it runs or another instance uses its volume",
                    record.target
                ),
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
        let plan = approved_plan(&self.moves.plans, &args, self.moves.now())?;
        // A plan that ran, an instance another move holds, or one with a copy that may run keeps the grant unspent.
        let probe = |target: &str, project: &str, name: &str, uuid: &str| {
            probe_copy(&self.moves.cfg, target, project, name, uuid)
        };
        if let Some(refusal) = start_refusal(
            &self.moves.journal.lines(),
            plan["digest"].as_str().unwrap_or_default(),
            &refs_pairs(&plan),
            &probe,
        ) {
            return Err(refusal);
        }
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

/// `instance_move_finish`: deletes a verified move's sources, only from an approved card, once, and each source only
/// when its copy is read back on the target as the one this move made.
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
        let source = self.moves.source();
        let dest = linked_target(&self.moves.cfg, &record.target)
            .ok()
            .map(|target| MoveWriter::new(target.socket));
        let (mut deleted, mut kept) = (Vec::new(), Vec::new());
        for one in record
            .instances
            .iter()
            .filter(|one| one.result.as_deref() == Some("moved"))
        {
            let label = format!("{}/{}", one.project, one.name);
            let path = instance_path(&one.project, &one.name);
            // The other side first: the copy must be on the target, as the one this move made.
            let copy_there = match (dest.as_ref().map(|dest| dest.get(&path)), &one.target_uuid) {
                (Some(Ok(doc)), Some(uuid)) => uuid_of(&doc).as_deref() == Some(uuid.as_str()),
                _ => false,
            };
            if !copy_there {
                kept.push(format!(
                    "{label}: its copy was not read back on {} as the one this move made, so the source was kept",
                    record.target
                ));
                continue;
            }
            match source.get(&path) {
                Ok(doc) if uuid_of(&doc) != one.uuid => {
                    kept.push(format!(
                        "{label}: the instance here is not the one that was moved, so it was kept"
                    ));
                    continue;
                }
                Ok(doc) if running(&doc) => {
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
                let here = volume["project"].as_str().unwrap_or("default");
                let there = volume["targetProject"].as_str().unwrap_or(here);
                let volume_label = format!("volume {pool}/{name}");
                let copied = dest
                    .as_ref()
                    .is_some_and(|dest| dest.get(&volume_path(pool, name, there)).is_ok());
                if !copied {
                    kept.push(format!(
                        "{volume_label}: its copy was not read back on {}, so it was kept",
                        record.target
                    ));
                    continue;
                }
                let used = match source.get(&volume_path(pool, name, here)) {
                    Ok(doc) => doc
                        .get("used_by")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len),
                    Err(IncusError::Status { code: 404, .. }) => {
                        deleted.push(volume_label);
                        continue;
                    }
                    Err(error) => {
                        kept.push(format!("{volume_label}: it could not be read: {error}"));
                        continue;
                    }
                };
                if used > 0 {
                    kept.push(format!(
                        "{volume_label}: another instance uses it, so it was kept"
                    ));
                    continue;
                }
                match source.delete_volume(here, pool, name) {
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

/// The `rbd` command lines of a stopped virtual machine's copy between two Ceph clusters.
///
/// Each side names its own cluster (`--cluster <name>` reads `/etc/ceph/<name>.conf` and the client's keyring beside it)
/// and its own client, so the same code runs whichever way the move goes. The source is only read: `rbd status`, `rbd
/// info` and `rbd export-diff`. The copy is `rbd export-diff <source> - | rbd import-diff - <target>`, run by
/// `arca_vmimport`'s [`Runner`]: two processes and a pipe, never a shell; the bytes stream from the reader to the writer.
// TODO(PR 201): once `arca_vmimport::copy` has `PipeFailure` and side-named errors, map them here, and fold
// `incus_image` into `arca_vmimport::incus_rbd_image` with the content type.
mod rbd {
    use arca_vmimport::Runner;
    use arca_vmimport::source::ceph_name_ok;
    use serde_json::Value;

    /// One Ceph image, with the cluster and the client that reach it from this node.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct RbdImage {
        /// The cluster, by the name of its config file here (`/etc/ceph/<cluster>.conf`).
        pub cluster: String,
        /// The Ceph client, without its `client.` prefix.
        pub client: String,
        /// The OSD pool.
        pub pool: String,
        pub image: String,
    }

    impl RbdImage {
        pub fn spec(&self) -> String {
            format!("{}/{}", self.pool, self.image)
        }

        fn names_ok(&self) -> Result<(), String> {
            match [&self.cluster, &self.client, &self.pool, &self.image]
                .into_iter()
                .find(|name| !ceph_name_ok(name))
            {
                Some(name) => Err(format!("{name:?} is not a Ceph name")),
                None => Ok(()),
            }
        }

        fn argv(&self, rest: &[&str]) -> Vec<String> {
            [
                "rbd",
                "--cluster",
                &self.cluster,
                "--id",
                &self.client,
                "--no-progress",
            ]
            .iter()
            .chain(rest)
            .map(|part| (*part).to_string())
            .collect()
        }
    }

    /// The Ceph image Incus keeps a volume in: `virtual-machine_<project>_<name>` for a virtual machine's config volume
    /// (its UEFI variables and TPM state), with `.block` for its disk; `custom_<project>_<name>` for a custom volume,
    /// with `.block` for a block one. The project prefix is left out in the `default` project.
    pub fn incus_image(kind: &str, project: &str, name: &str, block: bool) -> String {
        let volume = if project == "default" {
            name.to_string()
        } else {
            format!("{project}_{name}")
        };
        let suffix = if block { ".block" } else { "" };
        format!("{kind}_{volume}{suffix}")
    }

    /// Who holds `image` open, by address. Empty when no one does.
    pub fn watchers(runner: &dyn Runner, image: &RbdImage) -> Result<Vec<String>, String> {
        image.names_ok()?;
        let out = runner.output(&image.argv(&["status", "--format", "json", &image.spec()]))?;
        let status: Value =
            serde_json::from_str(&out).map_err(|_| "rbd status did not answer JSON".to_string())?;
        Ok(status["watchers"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|watcher| {
                watcher["address"]
                    .as_str()
                    .unwrap_or("an unnamed client")
                    .to_string()
            })
            .collect())
    }

    /// The size of `image`, in bytes.
    pub fn size(runner: &dyn Runner, image: &RbdImage) -> Result<u64, String> {
        image.names_ok()?;
        let out = runner.output(&image.argv(&["info", "--format", "json", &image.spec()]))?;
        let info: Value =
            serde_json::from_str(&out).map_err(|_| "rbd info did not answer JSON".to_string())?;
        info["size"]
            .as_u64()
            .ok_or_else(|| "rbd info names no size".to_string())
    }

    /// Copies every byte `source` holds into `target`, which exists and has the source's size.
    pub fn copy(runner: &dyn Runner, source: &RbdImage, target: &RbdImage) -> Result<(), String> {
        source.names_ok()?;
        target.names_ok()?;
        runner.pipe(
            &source.argv(&["export-diff", &source.spec(), "-"]),
            &target.argv(&["import-diff", "-", &target.spec()]),
        )
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::Mutex;

        #[derive(Default)]
        struct Recorder {
            calls: Mutex<Vec<Vec<String>>>,
            answer: String,
        }

        impl Runner for Recorder {
            fn output(&self, argv: &[String]) -> Result<String, String> {
                self.calls.lock().expect("calls").push(argv.to_vec());
                Ok(self.answer.clone())
            }
            fn pipe(&self, first: &[String], second: &[String]) -> Result<(), String> {
                let mut calls = self.calls.lock().expect("calls");
                calls.push(first.to_vec());
                calls.push(second.to_vec());
                Ok(())
            }
        }

        fn image(cluster: &str, client: &str, pool: &str) -> RbdImage {
            RbdImage {
                cluster: cluster.into(),
                client: client.into(),
                pool: pool.into(),
                image: incus_image("virtual-machine", "shop", "db", true),
            }
        }

        #[test]
        fn each_side_names_its_own_cluster_and_client() {
            let runner = Recorder::default();
            let from = image("east", "incus-east", "osd-east");
            let to = image("west", "incus-west", "osd-west");
            copy(&runner, &from, &to).expect("copied");
            let calls = runner.calls.lock().expect("calls").clone();
            assert_eq!(
                calls[0],
                [
                    "rbd",
                    "--cluster",
                    "east",
                    "--id",
                    "incus-east",
                    "--no-progress",
                    "export-diff",
                    "osd-east/virtual-machine_shop_db.block",
                    "-"
                ]
            );
            assert_eq!(
                calls[1],
                [
                    "rbd",
                    "--cluster",
                    "west",
                    "--id",
                    "incus-west",
                    "--no-progress",
                    "import-diff",
                    "-",
                    "osd-west/virtual-machine_shop_db.block"
                ]
            );
        }

        #[test]
        fn watchers_and_size_are_read_from_rbd_json() {
            let runner = Recorder {
                answer: r#"{"watchers":[{"address":"192.0.2.9:0/1"}],"size":2147483648}"#.into(),
                ..Recorder::default()
            };
            let from = image("east", "incus-east", "osd-east");
            assert_eq!(watchers(&runner, &from).expect("read"), ["192.0.2.9:0/1"]);
            assert_eq!(size(&runner, &from).expect("read"), 2_147_483_648);
            let calls = runner.calls.lock().expect("calls").clone();
            assert_eq!(calls[0][6..], ["status", "--format", "json", &from.spec()]);
            assert_eq!(calls[1][6], "info");
        }

        #[test]
        fn a_name_that_is_not_a_ceph_name_runs_nothing() {
            let runner = Recorder::default();
            let mut bad = image("east", "incus-east", "osd-east");
            bad.image = "--help".into();
            assert!(copy(&runner, &bad, &image("west", "w", "p")).is_err());
            bad.image = "a b".into();
            assert!(size(&runner, &bad).is_err());
            assert!(runner.calls.lock().expect("calls").is_empty());
        }

        #[test]
        fn incus_names_its_images_by_kind_project_and_name() {
            assert_eq!(
                incus_image("virtual-machine", "default", "db", true),
                "virtual-machine_db.block"
            );
            assert_eq!(
                incus_image("virtual-machine", "shop", "db", false),
                "virtual-machine_shop_db"
            );
            assert_eq!(
                incus_image("custom", "shop", "data", false),
                "custom_shop_data"
            );
            assert_eq!(
                incus_image("custom", "shop", "raw", true),
                "custom_shop_raw.block"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("arca-move-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("dir");
        dir
    }

    #[test]
    fn a_torn_last_line_is_skipped_and_the_next_append_starts_on_its_own_line() {
        let dir = scratch("torn");
        let journal = Journal::at(&dir);
        journal
            .append(&json!({ "event": "begin", "move": "mv-000000000001" }))
            .expect("append");
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
    fn a_journal_past_64_mib_is_still_read_and_an_overlong_line_is_skipped() {
        let dir = scratch("long");
        let journal = Journal::at(&dir);
        journal
            .append(&json!({ "event": "begin", "move": "mv-000000000001" }))
            .expect("append");
        let filler = "x".repeat(1024 * 1024 - 64);
        let mut file = OpenOptions::new()
            .append(true)
            .open(dir.join(MOVES_FILE))
            .expect("open");
        for _ in 0..65 {
            writeln!(file, "{}", json!({ "event": "note", "pad": filler })).expect("pad");
        }
        writeln!(
            file,
            "{}",
            json!({ "event": "note", "pad": "y".repeat(9 * 1024 * 1024) })
        )
        .expect("long");
        drop(file);
        journal
            .append(&json!({ "event": "end", "move": "mv-000000000001" }))
            .expect("append");
        let lines = journal.lines();
        assert_eq!(lines.len(), 67, "every line but the overlong one");
        assert_eq!(lines.last().expect("last")["event"], "end");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_plan_is_not_renewed_by_planning_again_and_expired_plans_are_pruned() {
        let dir = scratch("plans");
        let plans = Plans::at(&dir);
        let plan = |digest: &str, at: u64| json!({ "digest": digest, "createdAt": at, "expiresAt": at + PLAN_TTL_MS });
        let first = "a".repeat(64);
        let kept = plans.save(plan(&first, 1_000), 1_000).expect("saved");
        assert_eq!(kept["expiresAt"], 1_000 + PLAN_TTL_MS);
        let half = 1_000 + PLAN_TTL_MS / 2;
        let again = plans.save(plan(&first, half), half).expect("saved");
        assert_eq!(again["expiresAt"], 1_000 + PLAN_TTL_MS, "not renewed");
        assert_eq!(
            plans.get(&first).expect("kept")["expiresAt"],
            1_000 + PLAN_TTL_MS
        );
        let later = 1_000 + 2 * PLAN_KEEP_MS;
        plans
            .save(plan(&"b".repeat(64), later), later)
            .expect("saved");
        assert!(plans.get(&first).is_none(), "the expired plan was pruned");
        let mode = fs::metadata(dir.join(PLANS_DIR).join(format!("{}.json", "b".repeat(64))))
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(plans.get("../x").is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_plan_that_ran_or_an_instance_another_move_holds_does_not_start() {
        let digest = "c".repeat(64);
        let web = vec![(
            "shop".to_string(),
            "web".to_string(),
            Some("u-web".to_string()),
        )];
        let begin = |id: &str, plan: &str, name: &str| {
            json!({ "event": "begin", "move": id, "plan": plan, "target": "node-b",
                "instances": [{ "project": "shop", "name": name }] })
        };
        let none = |_: &str, _: &str, _: &str, _: &str| None;
        assert!(start_refusal(&[], &digest, &web, &none).is_none());
        let ran = vec![
            begin("mv-cccccccccccc", &digest, "web"),
            json!({ "event": "end", "move": "mv-cccccccccccc" }),
        ];
        assert!(matches!(
            start_refusal(&ran, &digest, &web, &none),
            Some(CallError::Refused { code, .. }) if code == "AlreadyRun"
        ));
        let other = vec![begin("mv-dddddddddddd", &"d".repeat(64), "web")];
        assert!(matches!(
            start_refusal(&other, &digest, &web, &none),
            Some(CallError::Refused { code, .. }) if code == "InstanceMoving"
        ));
        let mut ended = other.clone();
        ended.push(json!({ "event": "end", "move": "mv-dddddddddddd" }));
        assert!(start_refusal(&ended, &digest, &web, &none).is_none());
        let api = vec![begin("mv-eeeeeeeeeeee", &"e".repeat(64), "api")];
        assert!(start_refusal(&api, &digest, &web, &none).is_none());
    }

    #[test]
    fn an_instance_with_a_copy_that_may_run_is_not_moved_again() {
        let digest = "c".repeat(64);
        let web = vec![(
            "shop".to_string(),
            "web".to_string(),
            Some("u-web".to_string()),
        )];
        let runs = |target: &str, _: &str, _: &str, copy: &str| {
            Some(format!("{copy} reads Running on {target}"))
        };
        let gone = |_: &str, _: &str, _: &str, _: &str| None;
        let made = |id: &str, at: u64, target: &str, uuid: &str, result: &str| {
            vec![
                json!({ "event": "begin", "move": id, "plan": "d".repeat(64), "target": target,
                    "at": at, "instances": [{ "project": "shop", "name": "web", "uuid": uuid }] }),
                json!({ "event": "created", "move": id, "project": "shop", "name": "web",
                    "targetUuid": format!("copy-{id}") }),
                json!({ "event": "result", "move": id, "project": "shop", "name": "web", "result": result }),
                json!({ "event": "end", "move": id }),
            ]
        };
        let refusal =
            |lines: &[Value], probe: CopyProbe| start_refusal(lines, &digest, &web, probe);
        // A moved copy that runs: the move may still be finished.
        let moved = made("mv-dddddddddddd", 5, "node-b", "u-web", "moved");
        assert!(
            matches!(refusal(&moved, &runs), Some(CallError::Refused { code, message })
                if code == "AlreadyMoved" && message.contains("finish move mv-dddddddddddd")
                    && message.contains("copy-mv-dddddddddddd reads Running on node-b")),
        );
        // Read gone or stopped on its target, it holds nothing.
        assert!(refusal(&moved, &gone).is_none());
        // Another instance of the name (a new uuid) is not the one that moved.
        assert!(
            refusal(
                &made("mv-dddddddddddd", 5, "node-b", "u-other", "moved"),
                &runs
            )
            .is_none()
        );
        // A failed move's copy holds it too, with no finish offered; so does a moved one whose finish kept the source.
        let failed = made("mv-dddddddddddd", 5, "node-b", "u-web", "failed");
        assert!(
            matches!(refusal(&failed, &runs), Some(CallError::Refused { code, message })
                if code == "CopyMayRun" && !message.contains("finish move")),
        );
        let mut kept = moved.clone();
        kept.push(json!({ "event": "finish", "move": "mv-dddddddddddd", "kept": ["shop/web"] }));
        assert!(
            matches!(refusal(&kept, &runs), Some(CallError::Refused { code, .. }) if code == "CopyMayRun")
        );
        // A copy its rollback journaled removed holds nothing, and is not read.
        let mut removed = made("mv-dddddddddddd", 5, "node-b", "u-web", "rolled_back");
        removed.insert(
            2,
            json!({ "event": "rollback", "move": "mv-dddddddddddd", "project": "shop", "name": "web",
                "targetRemoved": true }),
        );
        assert!(refusal(&removed, &runs).is_none());
        removed[2]["targetRemoved"] = json!(false);
        assert!(refusal(&removed, &runs).is_some());
        // A move that recorded no copy holds nothing.
        let mut none_made = moved.clone();
        none_made.remove(1);
        assert!(refusal(&none_made, &runs).is_none());
        // Any earlier copy that may run holds it, not only the latest move's; the newest is named.
        let mut both = made("mv-dddddddddddd", 5, "node-b", "u-web", "moved");
        both.extend(made("mv-eeeeeeeeeeee", 9, "node-c", "u-web", "failed"));
        let only_b = |target: &str, _: &str, _: &str, _: &str| {
            (target == "node-b").then(|| "it reads Running on node-b".to_string())
        };
        assert!(
            matches!(refusal(&both, &only_b), Some(CallError::Refused { message, .. })
                if message.contains("mv-dddddddddddd")),
        );
        assert!(
            matches!(refusal(&both, &runs), Some(CallError::Refused { message, .. })
                if message.contains("mv-eeeeeeeeeeee")),
        );
    }

    #[test]
    fn the_estimate_is_the_stop_and_start_and_the_whole_volume() {
        assert_eq!(estimated_downtime(0), 30);
        assert_eq!(estimated_downtime(100 * ASSUMED_RATE), 130);
        assert_eq!(estimated_downtime(100 * ASSUMED_RATE + 1), 131);
    }

    #[test]
    fn a_move_id_is_mv_and_twelve_lowercase_hex_digits() {
        assert!(move_id_ok("mv-0123456789ab"));
        assert!(!move_id_ok("mv-0123456789AB"));
        assert!(!move_id_ok("mv-0123"));
    }

    #[test]
    fn only_a_proxy_on_every_address_is_probed() {
        assert_eq!(wildcard("tcp:0.0.0.0:8080"), Some(8080));
        assert_eq!(wildcard("tcp:[::]:443"), Some(443));
        assert_eq!(wildcard("tcp:127.0.0.1:8080"), None);
        assert_eq!(wildcard("tcp:192.0.2.5:80"), None);
        assert_eq!(wildcard("udp:0.0.0.0:53"), None);
    }
}
