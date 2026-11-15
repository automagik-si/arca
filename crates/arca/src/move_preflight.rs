//! `instance_move_preflight`: before any move, whether a target node can take each instance, and what to fix first.
//!
//! The target is one of this kernel's linked hosts (`approval.linkedHosts`), named as the config names it, and read through
//! its `incusSocket`: the target's Incus socket, on this machine or forwarded to a path here. The verb only reads. It sends
//! GETs, through the GET-only client, to this node's Incus and to the target's, and it walks the instance's storage folder
//! on this host to measure its real size. It stops, starts, copies and creates nothing, and writes no journal or state
//! file.
//!
//! Every finding is a check in the instance's row, ok or the reason it is not; the verb refuses nothing but arguments it
//! cannot read. A target that cannot be read gives every row a reason that says so, and the verb still answers.

use std::cell::{OnceCell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use arca_incus::{IncusError, IncusRead, UnixIncus};
use arca_mcp::{CallError, Caller, ToolHandler};
use serde_json::{Map, Value, json};

use crate::config::{NodeConfig, host_name_ok};

/// The most instances one call checks.
pub const MAX_INSTANCES: usize = 50;
/// The checks of every row, in the order each row lists them: the verb table's own list.
pub use arca_api::verbs::MOVE_PREFLIGHT_CHECKS as CHECKS;
/// Where Incus keeps its storage pools on a standard host.
const STANDARD_INCUS_DIR: &str = "/var/lib/incus";
/// How long the first read of the target may take before the target counts as unreachable.
const TARGET_PROBE: Duration = Duration::from_secs(5);
/// The CPU features of each x86-64 level above the first.
const X86_64_V2: &[&str] = &["cx16", "lahf_lm", "popcnt", "sse4_1", "sse4_2", "ssse3"];
const X86_64_V3: &[&str] = &[
    "avx", "avx2", "bmi1", "bmi2", "f16c", "fma", "abm", "movbe", "xsave",
];
const X86_64_V4: &[&str] = &["avx512f", "avx512bw", "avx512cd", "avx512dq", "avx512vl"];
/// Character devices every Linux host has, so a device that names one needs nothing from the target.
const STANDARD_DEVICES: &[&str] = &[
    "/dev/null",
    "/dev/zero",
    "/dev/full",
    "/dev/random",
    "/dev/urandom",
    "/dev/tty",
    "/dev/ptmx",
    "/dev/net/tun",
    "/dev/fuse",
];

/// `instance_move_preflight`.
pub struct MovePreflightHandler {
    cfg: Arc<NodeConfig>,
    storage_root: PathBuf,
}

impl MovePreflightHandler {
    /// The handler for the node `cfg`. The instances' storage folders are read under the folder that holds this node's
    /// Incus socket when it is the standard local one, and under `/var/lib/incus` otherwise.
    pub fn new(cfg: Arc<NodeConfig>) -> Self {
        let storage_root = cfg
            .incus
            .socket_is_local()
            .then(|| cfg.incus.socket.parent().map(Path::to_path_buf))
            .flatten()
            .unwrap_or_else(|| PathBuf::from(STANDARD_INCUS_DIR));
        Self { cfg, storage_root }
    }

    /// The same handler reading the storage folders under `root` (the folder that holds `storage-pools`).
    pub fn with_storage_root(mut self, root: PathBuf) -> Self {
        self.storage_root = root;
        self
    }
}

/// One instance reference: its project and its name.
struct Reference {
    project: String,
    name: String,
}

fn invalid(message: impl Into<String>) -> CallError {
    CallError::Invalid(message.into())
}

/// Whether `name` is an Incus project name the verb reads: a letter or digit, then up to 62 letters, digits, dots,
/// underscores or hyphens.
pub(crate) fn project_ok(name: &str) -> bool {
    let bytes = name.as_bytes();
    let Some((first, rest)) = bytes.split_first() else {
        return false;
    };
    first.is_ascii_alphanumeric()
        && rest.len() <= 62
        && rest
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Whether `name` is an Incus instance name: a host name label of at most 63 letters, digits and inner hyphens.
pub(crate) fn instance_ok(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 63
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'-')
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
}

fn parse_args(args: &Value) -> Result<(Vec<Reference>, String, bool), CallError> {
    let allowed = ["instances", "target", "live"];
    let object = args
        .as_object()
        .ok_or_else(|| invalid("arguments: expected an object"))?;
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(invalid(arca_api::unknown_field("", key, &allowed)));
    }
    let list = object
        .get("instances")
        .and_then(Value::as_array)
        .filter(|list| (1..=MAX_INSTANCES).contains(&list.len()))
        .ok_or_else(|| {
            invalid(format!(
                "instances: expected a list of 1 to {MAX_INSTANCES} {{project, name}} references"
            ))
        })?;
    let mut references = Vec::new();
    for (index, item) in list.iter().enumerate() {
        let item = item
            .as_object()
            .filter(|item| item.keys().all(|key| key == "project" || key == "name"))
            .ok_or_else(|| invalid(format!("instances[{index}]: expected {{project, name}}")))?;
        let project = item
            .get("project")
            .and_then(Value::as_str)
            .filter(|project| project_ok(project))
            .ok_or_else(|| {
                invalid(format!(
                    "instances[{index}].project: expected an Incus project name"
                ))
            })?;
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| instance_ok(name))
            .ok_or_else(|| {
                invalid(format!(
                    "instances[{index}].name: expected an Incus instance name"
                ))
            })?;
        references.push(Reference {
            project: project.to_string(),
            name: name.to_string(),
        });
    }
    let target = object
        .get("target")
        .and_then(Value::as_str)
        .filter(|target| host_name_ok(target))
        .ok_or_else(|| invalid("target: expected the name of a linked host"))?;
    let live = match object.get("live") {
        None => false,
        Some(value) => value
            .as_bool()
            .ok_or_else(|| invalid("live: expected true or false"))?,
    };
    Ok((references, target.to_string(), live))
}

/// A path segment, percent-encoded past the characters Incus names use.
pub(crate) fn seg(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// One host's Incus, read once per path in a call.
struct Host {
    incus: UnixIncus,
    cache: RefCell<BTreeMap<String, Result<Value, IncusError>>>,
    interfaces: OnceCell<Vec<String>>,
    addresses: OnceCell<Vec<(IpAddr, String)>>,
}

impl Host {
    fn new(socket: PathBuf) -> Self {
        Self {
            incus: UnixIncus::new(socket),
            cache: RefCell::new(BTreeMap::new()),
            interfaces: OnceCell::new(),
            addresses: OnceCell::new(),
        }
    }

    fn get(&self, path: &str) -> Result<Value, IncusError> {
        if let Some(found) = self.cache.borrow().get(path) {
            return found.clone();
        }
        let answer = self.incus.get(path);
        self.cache
            .borrow_mut()
            .insert(path.to_string(), answer.clone());
        answer
    }

    fn opt(&self, path: &str) -> Option<Value> {
        self.get(path).ok()
    }

    /// The object at `path`: `None` when Incus answers that it is not there, the error for any other failure.
    fn found(&self, path: &str) -> Result<Option<Value>, IncusError> {
        match self.get(path) {
            Ok(value) => Ok(Some(value)),
            Err(IncusError::Status { code: 404, .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn resources(&self) -> Option<Value> {
        self.opt("/1.0/resources")
    }

    /// The CPU features every core of the host has, when Incus lists them.
    fn cpu_flags(&self) -> Option<BTreeSet<String>> {
        let resources = self.resources()?;
        let mut common: Option<BTreeSet<String>> = None;
        for socket in resources.pointer("/cpu/sockets")?.as_array()? {
            for core in socket
                .get("cores")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(flags) = core.get("flags").and_then(Value::as_array) else {
                    continue;
                };
                let flags: BTreeSet<String> = flags
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect();
                common = Some(match common {
                    None => flags,
                    Some(seen) => seen.intersection(&flags).cloned().collect(),
                });
            }
        }
        common.filter(|flags| !flags.is_empty())
    }

    fn cpu_architecture(&self) -> Option<String> {
        self.resources()?
            .pointer("/cpu/architecture")?
            .as_str()
            .map(str::to_string)
    }

    /// The architectures the host runs instances of, from its server document.
    fn architectures(&self) -> Option<Vec<String>> {
        let server = self.opt("/1.0")?;
        let list = server.pointer("/environment/architectures")?.as_array()?;
        Some(
            list.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
        )
    }

    fn gpu_cards(&self) -> Vec<Value> {
        self.resources()
            .and_then(|resources| resources.pointer("/gpu/cards").cloned())
            .and_then(|cards| cards.as_array().cloned())
            .unwrap_or_default()
    }

    /// The names of the host's networks and interfaces, as Incus lists them.
    fn interfaces(&self) -> &[String] {
        self.interfaces.get_or_init(|| {
            let mut names: Vec<String> = self
                .opt("/1.0/networks?recursion=1")
                .and_then(|list| list.as_array().cloned())
                .unwrap_or_default()
                .iter()
                .filter_map(|row| row.get("name").and_then(Value::as_str))
                .filter(|name| *name != "lo")
                .map(str::to_string)
                .collect();
            names.sort();
            names.dedup();
            names
        })
    }

    fn has_interface(&self, name: &str) -> bool {
        self.interfaces().iter().any(|listed| listed == name)
            || self.opt(&format!("/1.0/networks/{}", seg(name))).is_some()
    }

    /// Every address the host's interfaces hold, each with its interface.
    fn addresses(&self) -> &[(IpAddr, String)] {
        self.addresses.get_or_init(|| {
            let mut out = Vec::new();
            for name in self.interfaces() {
                let Some(state) = self.opt(&format!("/1.0/networks/{}/state", seg(name))) else {
                    continue;
                };
                for row in state
                    .get("addresses")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if let Some(address) = row
                        .get("address")
                        .and_then(Value::as_str)
                        .and_then(|text| text.parse::<IpAddr>().ok())
                    {
                        out.push((address, name.clone()));
                    }
                }
            }
            out
        })
    }
}

/// One finding of a check: what it is about, whether it is ok, why, and what to do.
fn finding(
    subject: impl Into<String>,
    ok: bool,
    detail: impl Into<String>,
    proposal: Option<String>,
) -> Value {
    json!({ "subject": subject.into(), "ok": ok, "detail": detail.into(), "proposal": proposal })
}

/// One check of a row.
struct Check {
    name: &'static str,
    ok: bool,
    detail: String,
    findings: Vec<Value>,
    risks: Vec<String>,
    extra: Map<String, Value>,
}

impl Check {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            ok: true,
            detail: String::new(),
            findings: Vec::new(),
            risks: Vec::new(),
            extra: Map::new(),
        }
    }

    fn not_ok(name: &'static str, detail: impl Into<String>) -> Self {
        let mut check = Self::new(name);
        check.ok = false;
        check.detail = detail.into();
        check
    }

    /// Adds `finding`; a finding that is not ok makes the check not ok.
    fn add(&mut self, finding: Value) {
        if finding["ok"] == false {
            self.ok = false;
        }
        self.findings.push(finding);
    }

    /// The detail from the findings: `all_ok` when every one is ok, else the subjects that are not.
    fn summarize(&mut self, none: &str, all_ok: &str) {
        if self.findings.is_empty() {
            self.detail = none.to_string();
            return;
        }
        let failing: Vec<&str> = self
            .findings
            .iter()
            .filter(|finding| finding["ok"] == false)
            .filter_map(|finding| finding["subject"].as_str())
            .collect();
        self.detail = if failing.is_empty() {
            all_ok.to_string()
        } else {
            format!("not ok: {}", failing.join(", "))
        };
    }

    fn json(self) -> Value {
        let mut out = Map::new();
        out.insert("check".into(), json!(self.name));
        out.insert("ok".into(), json!(self.ok));
        out.insert("detail".into(), json!(self.detail));
        out.insert("findings".into(), Value::Array(self.findings));
        out.insert("risks".into(), json!(self.risks));
        out.extend(self.extra);
        Value::Object(out)
    }
}

/// A size in binary units with one decimal, such as `4.4 MiB`.
pub(crate) fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Measures `path` without following links: a file's size or its blocks, whichever is larger (a compressed or
/// deduplicated source takes fewer blocks than its copy will), and for a folder the same for every entry under it, into
/// nested file systems (a btrfs instance's nested subvolumes), with a file of several links counted once and the blocks of
/// everything else. Nothing is read but names and sizes. The first entry the kernel's user cannot read or list makes the
/// size not measured, with the reason.
fn measure_path(path: &Path) -> Result<u64, String> {
    use std::os::unix::fs::MetadataExt;
    let cannot = |what: &Path, error: &std::io::Error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            format!("{} is not there", what.display())
        } else {
            format!(
                "the kernel's user cannot read {}: {error}; the size needs read access there (the Incus storage folders, and each host folder the instance mounts)",
                what.display()
            )
        }
    };
    let top = std::fs::symlink_metadata(path).map_err(|error| cannot(path, &error))?;
    let taken = |meta: &std::fs::Metadata| {
        let blocks = meta.blocks().saturating_mul(512);
        if meta.is_file() {
            blocks.max(meta.len())
        } else {
            blocks
        }
    };
    if top.is_file() {
        return Ok(taken(&top));
    }
    if !top.is_dir() {
        return Err(format!("{} is not a folder or a file", path.display()));
    }
    let mut bytes = taken(&top);
    let mut linked: HashSet<(u64, u64)> = HashSet::new();
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|error| cannot(&dir, &error))?;
        for entry in entries {
            let entry = entry.map_err(|error| cannot(&dir, &error))?;
            let entry_path = entry.path();
            let meta = std::fs::symlink_metadata(&entry_path)
                .map_err(|error| cannot(&entry_path, &error))?;
            if !meta.is_dir() && meta.nlink() > 1 && !linked.insert((meta.dev(), meta.ino())) {
                continue;
            }
            bytes = bytes.saturating_add(taken(&meta));
            if meta.is_dir() {
                stack.push(entry_path);
            }
        }
    }
    Ok(bytes)
}

/// The bytes a size Incus writes names: a plain count of bytes, or a count with a unit (`10GiB`, `512MB`).
fn size_of(text: &str) -> Option<u64> {
    if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) {
        return text.parse().ok();
    }
    crate::service::size_bytes(text).and_then(|bytes| u64::try_from(bytes).ok())
}

/// Whether `path` is where a file system is mounted: its device is not its parent's. A zfs, lvm or ceph volume of a
/// container is there only while Incus has it mounted.
fn is_mount_point(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Some(parent) = path.parent() else {
        return true;
    };
    match (std::fs::symlink_metadata(path), std::fs::metadata(parent)) {
        (Ok(own), Ok(parent)) => own.is_dir() && own.dev() != parent.dev(),
        _ => false,
    }
}

/// The name Incus gives an instance of `project` in a pool's folders: `<project>_<name>`, or `<name>` in `default`. A
/// custom volume is always prefixed, `default_` included (see [`Run::measure`]).
fn stored(project: &str, name: &str) -> String {
    if project == "default" {
        name.to_string()
    } else {
        format!("{project}_{name}")
    }
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

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
}

fn is_root_disk(device: &Value) -> bool {
    text(device, "type") == Some("disk") && text(device, "path") == Some("/")
}

/// A subnet in Incus's spelling, `192.0.2.1/24`: the host address and the prefix length.
fn subnet(text: &str) -> Option<(IpAddr, u8)> {
    let (address, length) = text.split_once('/')?;
    let address: IpAddr = address.parse().ok()?;
    let length: u8 = length.parse().ok()?;
    let max = if address.is_ipv4() { 32 } else { 128 };
    (length <= max).then_some((address, length))
}

fn contains(net: (IpAddr, u8), address: IpAddr) -> bool {
    let (base, length) = net;
    match (base, address) {
        (IpAddr::V4(base), IpAddr::V4(address)) => {
            let mask = if length == 0 {
                0
            } else {
                u32::MAX << (32 - u32::from(length))
            };
            u32::from(base) & mask == u32::from(address) & mask
        }
        (IpAddr::V6(base), IpAddr::V6(address)) => {
            let mask = if length == 0 {
                0
            } else {
                u128::MAX << (128 - u32::from(length))
            };
            u128::from(base) & mask == u128::from(address) & mask
        }
        _ => false,
    }
}

fn baseline(architecture: &str, flags: &BTreeSet<String>) -> Option<String> {
    if architecture != "x86_64" {
        return None;
    }
    let has = |list: &[&str]| list.iter().all(|flag| flags.contains(*flag));
    let level = if !has(X86_64_V2) {
        1
    } else if !has(X86_64_V3) {
        2
    } else if !has(X86_64_V4) {
        3
    } else {
        4
    };
    Some(format!("x86-64-v{level}"))
}

/// What the size check measured, part by part. A part is `None` when the measuring stopped before it.
#[derive(Default)]
struct Parts {
    /// How the size was found: `walked` (the folders here) or `incus` (a virtual machine's block disk, by its size in
    /// Incus).
    method: Option<&'static str>,
    path: Option<String>,
    instance: Option<u64>,
    snapshots: Option<u64>,
    volumes: Option<u64>,
    host_folders: Option<u64>,
    risks: Vec<String>,
}

/// The pool of the target an instance would land on: its name and its document.
struct TargetPool {
    name: String,
    doc: Value,
}

/// One call: the two hosts, the target's name and why it cannot be read, and the moves already counted per target pool.
struct Run<'a> {
    source: &'a Host,
    target: Option<&'a Host>,
    target_name: &'a str,
    target_why: Option<String>,
    storage_root: &'a Path,
    live: bool,
    /// Per target pool: each earlier instance of this call whose size fits there, and the bytes it needs.
    pending: BTreeMap<String, Vec<(String, u64)>>,
}

impl Run<'_> {
    /// The target, or the sentence that says why its facts are missing.
    fn target(&self) -> Result<&Host, String> {
        self.target.ok_or_else(|| {
            format!(
                "needs {}'s facts, which could not be read: {}",
                self.target_name,
                self.target_why.as_deref().unwrap_or("unknown")
            )
        })
    }

    fn row(&mut self, reference: &Reference) -> Value {
        let label = format!("{}/{}", reference.project, reference.name);
        let instance = self.source.get(&format!(
            "/1.0/instances/{}?project={}",
            seg(&reference.name),
            seg(&reference.project)
        ));
        let (checks, reason) = match instance {
            Err(error) => {
                let why = format!("{label} could not be read on this node: {error}");
                let checks = CHECKS
                    .iter()
                    .map(|name| Check::not_ok(name, why.clone()))
                    .collect::<Vec<_>>();
                (checks, Some(why))
            }
            Ok(instance) => {
                let project = reference.project.as_str();
                let pool = self.target_pool(project, &instance);
                let checks = vec![
                    self.size(&label, project, &reference.name, &instance, &pool),
                    self.storage_driver(&instance, &pool),
                    self.cpu(&instance),
                    self.devices(project, &reference.name, &instance, &pool),
                    self.networks(project, &instance),
                    self.projects_profiles_acls(project, &instance),
                    self.proxies(&instance),
                    self.macvlan(project, &instance),
                ];
                let failing: Vec<&str> = checks
                    .iter()
                    .filter(|check| !check.ok)
                    .map(|check| check.name)
                    .collect();
                let reason = if let Some(why) = &self.target_why {
                    Some(format!("{} cannot be read: {why}", self.target_name))
                } else if failing.is_empty() {
                    None
                } else {
                    Some(format!("not ok: {}", failing.join(", ")))
                };
                (checks, reason)
            }
        };
        let go = reason.is_none() && checks.iter().all(|check| check.ok);
        json!({
            "project": reference.project,
            "name": reference.name,
            "target": self.target_name,
            "go": go,
            "reason": reason,
            "checks": checks.into_iter().map(Check::json).collect::<Vec<_>>(),
        })
    }

    /// The instance's root pool here, from its root disk device.
    fn source_pool(instance: &Value) -> Option<String> {
        devices(instance)
            .into_iter()
            .find(|(_, device)| is_root_disk(device))
            .and_then(|(_, device)| text(&device, "pool").map(str::to_string))
    }

    /// The target pool: the one with the same name as the instance's pool here, else the root pool the target's default
    /// profile in the instance's project names.
    fn target_pool(&self, project: &str, instance: &Value) -> Result<TargetPool, String> {
        let target = self.target()?;
        let wanted = Self::source_pool(instance)
            .ok_or_else(|| "the instance has no root disk with a pool".to_string())?;
        let read = |name: &str| target.found(&format!("/1.0/storage-pools/{}", seg(name)));
        match read(&wanted) {
            Ok(Some(doc)) => return Ok(TargetPool { name: wanted, doc }),
            Ok(None) => {}
            Err(error) => {
                return Err(format!(
                    "{}'s pool {wanted} could not be read: {error}",
                    self.target_name
                ));
            }
        }
        let profile_pool = target
            .opt(&format!("/1.0/profiles/default?project={}", seg(project)))
            .and_then(|profile| {
                profile
                    .get("devices")
                    .and_then(Value::as_object)?
                    .values()
                    .find(|device| is_root_disk(device))
                    .and_then(|device| text(device, "pool").map(str::to_string))
            });
        if let Some(name) = profile_pool
            && let Ok(Some(doc)) = read(&name)
        {
            return Ok(TargetPool { name, doc });
        }
        Err(format!(
            "{} has no pool named {wanted}, and its default profile in {project} names no root pool",
            self.target_name
        ))
    }

    fn size(
        &mut self,
        label: &str,
        project: &str,
        name: &str,
        instance: &Value,
        pool: &Result<TargetPool, String>,
    ) -> Check {
        let mut check = Check::new("size");
        let mut measure = json!({
            "method": null, "path": null, "instanceBytes": null, "snapshotBytes": null, "volumeBytes": null,
            "hostFolderBytes": null, "notMeasured": null, "measuredBytes": null, "headroomBytes": null,
            "neededBytes": null, "sourcePool": Self::source_pool(instance),
            "targetPool": pool.as_ref().ok().map(|pool| pool.name.clone()),
            "targetFreeBytes": null, "pendingBytes": 0, "pendingFor": [], "availableBytes": null,
        });
        let mut parts = Parts::default();
        let measured = self.measure(project, name, instance, pool, &mut parts);
        measure["path"] = json!(parts.path);
        measure["instanceBytes"] = json!(parts.instance);
        measure["snapshotBytes"] = json!(parts.snapshots);
        measure["volumeBytes"] = json!(parts.volumes);
        measure["hostFolderBytes"] = json!(parts.host_folders);
        check.risks.append(&mut parts.risks);
        let (bytes, said) = match measured {
            Ok(found) => found,
            Err(why) => {
                check.ok = false;
                check.detail = format!("not measured: {why}");
                measure["notMeasured"] = json!(why);
                check.extra.insert("measure".into(), measure);
                return check;
            }
        };
        measure["method"] = json!(parts.method.unwrap_or("walked"));
        let headroom = bytes / 10;
        let needed = bytes.saturating_add(headroom);
        measure["measuredBytes"] = json!(bytes);
        measure["headroomBytes"] = json!(headroom);
        measure["neededBytes"] = json!(needed);
        // The same need without the snapshots, which are counted in full and so may make a false no-go.
        let without = bytes.saturating_sub(parts.snapshots.unwrap_or(0));
        let needed_without = without.saturating_add(without / 10);
        measure["neededWithoutSnapshotsBytes"] = json!(needed_without);
        let pool = match pool {
            Ok(pool) => pool,
            Err(why) => {
                check.ok = false;
                check.detail = format!(
                    "{said}; with 10% headroom it needs {}, but {why}",
                    human(needed)
                );
                check.extra.insert("measure".into(), measure);
                return check;
            }
        };
        let space = self.target.and_then(|target| {
            target.opt(&format!("/1.0/storage-pools/{}/resources", seg(&pool.name)))
        });
        let free = space.as_ref().and_then(|space| {
            let total = space.pointer("/space/total")?.as_u64()?;
            let used = space.pointer("/space/used")?.as_u64()?;
            Some(total.saturating_sub(used))
        });
        let Some(free) = free else {
            check.ok = false;
            check.detail = format!(
                "{said}; with 10% headroom it needs {}, but the free space of {}'s pool {} could not be read",
                human(needed),
                self.target_name,
                pool.name
            );
            check.extra.insert("measure".into(), measure);
            return check;
        };
        let earlier = self.pending.get(&pool.name).cloned().unwrap_or_default();
        let pending: u64 = earlier.iter().map(|(_, bytes)| *bytes).sum();
        let available = free.saturating_sub(pending);
        measure["targetFreeBytes"] = json!(free);
        measure["pendingBytes"] = json!(pending);
        measure["pendingFor"] = json!(earlier.iter().map(|(label, _)| label).collect::<Vec<_>>());
        measure["availableBytes"] = json!(available);
        let pending_words = if earlier.is_empty() {
            "minus 0 B pending for earlier moves to it".to_string()
        } else {
            format!(
                "minus {} pending for {}",
                human(pending),
                earlier
                    .iter()
                    .map(|(label, _)| label.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        check.ok = needed <= available;
        let fits_without = needed_without <= available;
        measure["fitsWithoutSnapshots"] = json!(fits_without);
        check.detail = format!(
            "{said}; with 10% headroom it needs {}. {}'s pool {} has {} free, {pending_words} = {} available: {}",
            human(needed),
            self.target_name,
            pool.name,
            human(free),
            human(available),
            if check.ok {
                "it fits".to_string()
            } else if fits_without {
                format!(
                    "{} needed > {} available, it does not fit with its snapshots counted in full; without them it needs {}, so it fits without snapshots (move it without them, or delete some first)",
                    human(needed),
                    human(available),
                    human(needed_without)
                )
            } else {
                format!(
                    "{} needed > {} available, it does not fit, and without its snapshots it still needs {}",
                    human(needed),
                    human(available),
                    human(needed_without)
                )
            }
        );
        if check.ok {
            self.pending
                .entry(pool.name.clone())
                .or_default()
                .push((label.to_string(), needed));
        }
        check.extra.insert("measure".into(), measure);
        check
    }

    /// The driver of this node's pool `pool`.
    fn pool_driver(&self, pool: &str) -> Option<String> {
        self.source
            .opt(&format!("/1.0/storage-pools/{}", seg(pool)))
            .and_then(|doc| text(&doc, "driver").map(str::to_string))
    }

    /// The bytes of one volume's folder at `path` on the pool `pool`: walked on btrfs and dir, walked on zfs, lvm and ceph
    /// only where Incus has the volume mounted, and never Incus's own usage figure, which leaves out data a zfs clone
    /// shares with its image. A virtual machine's disk on a block driver is not walked ([`Run::vm_disk_bytes`] sizes it), and
    /// an unknown driver is not measured.
    fn measure_volume(&self, path: &Path, pool: &str, vm: bool, what: &str) -> Result<u64, String> {
        match self.pool_driver(pool).as_deref() {
            Some("btrfs" | "dir") => measure_path(path),
            Some(driver @ ("zfs" | "lvm" | "ceph")) if !vm => {
                if is_mount_point(path) {
                    measure_path(path)
                } else {
                    Err(format!(
                        "{what}'s {driver} volume is not mounted at {} (Incus mounts it while the container runs), and Incus's own usage figure leaves out data shared with the image, so it is not used",
                        path.display()
                    ))
                }
            }
            Some(driver @ ("zfs" | "lvm" | "ceph")) => Err(format!(
                "{what} is a virtual machine whose disk is a {driver} block volume this host cannot walk"
            )),
            driver => Err(format!(
                "{what} is on pool {pool}, whose driver {} this check cannot measure (btrfs, dir, or a mounted zfs, lvm or ceph volume)",
                driver.unwrap_or("unknown")
            )),
        }
    }

    /// Everything of the instance that lands on the target pool, measured on this host: its own volume, its snapshots,
    /// each custom volume it mounts that is not on the target yet and lands on the same pool, and each host folder it
    /// mounts (the proposal copies it into a custom volume there). Fills `parts`, and answers the total and its words.
    fn measure(
        &self,
        project: &str,
        name: &str,
        instance: &Value,
        pool: &Result<TargetPool, String>,
        parts: &mut Parts,
    ) -> Result<(u64, String), String> {
        let label = format!("{project}/{name}");
        let source_pool = Self::source_pool(instance)
            .ok_or_else(|| "the instance has no root disk with a pool here".to_string())?;
        let vm = text(instance, "type") == Some("virtual-machine");
        let driver = self.pool_driver(&source_pool);
        // A virtual machine's disk on a block driver is one block volume this host cannot walk: its size is the one
        // Incus gives it, the most it can hold.
        let block_vm = vm && matches!(driver.as_deref(), Some("zfs" | "lvm" | "ceph"));
        let (folder, snapshot_folder) = if vm {
            ("virtual-machines", "virtual-machines-snapshots")
        } else {
            ("containers", "containers-snapshots")
        };
        let pools = self.storage_root.join("storage-pools");
        let path = pools
            .join(&source_pool)
            .join(folder)
            .join(stored(project, name));
        parts.path = Some(path.display().to_string());
        let (own, first) = if block_vm {
            let (bytes, from) =
                self.vm_disk_bytes(project, name, &source_pool, instance, &label)?;
            parts.method = Some("incus");
            parts.risks.push(
                "a virtual machine's block disk is counted at its whole size, the most its copy can take; a thin copy takes less"
                    .to_string(),
            );
            (
                bytes,
                format!(
                    "the virtual machine's {} disk {} ({from})",
                    driver.as_deref().unwrap_or("block"),
                    human(bytes)
                ),
            )
        } else {
            let own = self.measure_volume(&path, &source_pool, vm, &label)?;
            (
                own,
                format!("the instance {} at {}", human(own), path.display()),
            )
        };
        parts.instance = Some(own);
        let mut words = vec![first];

        let listed = self
            .source
            .opt(&format!(
                "/1.0/instances/{}/snapshots?project={}",
                seg(name),
                seg(project)
            ))
            .and_then(|list| list.as_array().map(Vec::len))
            .unwrap_or(0);
        let snapshot_path = pools
            .join(&source_pool)
            .join(snapshot_folder)
            .join(stored(project, name));
        let walkable = matches!(
            self.pool_driver(&source_pool).as_deref(),
            Some("btrfs" | "dir")
        );
        let snapshots = match std::fs::symlink_metadata(&snapshot_path) {
            Ok(_) if walkable => measure_path(&snapshot_path)?,
            Err(error) if walkable && error.kind() != std::io::ErrorKind::NotFound => {
                return Err(format!(
                    "the kernel's user cannot read {}: {error}; the size needs read access there (the Incus storage folders, and each host folder the instance mounts)",
                    snapshot_path.display()
                ));
            }
            _ if listed == 0 => 0,
            // Each snapshot of a block disk is counted at the disk's whole size: the most it can take.
            _ if block_vm => own.saturating_mul(listed as u64),
            _ if walkable => {
                return Err(format!(
                    "Incus lists {listed} snapshot(s) of {label}, but {} is not there",
                    snapshot_path.display()
                ));
            }
            _ => {
                return Err(format!(
                    "{label} has {listed} snapshot(s) on pool {source_pool}, which this host cannot walk"
                ));
            }
        };
        parts.snapshots = Some(snapshots);
        if snapshots > 0 {
            words.push(format!("its snapshots {}", human(snapshots)));
            parts.risks.push(
                "each snapshot is counted in full; snapshots that share data take less, so the need is an upper bound"
                    .to_string(),
            );
        }

        let target_pool = pool.as_ref().ok().map(|pool| pool.name.as_str());
        let (mut volumes, mut host_folders) = (0u64, 0u64);
        for (device_name, device) in devices(instance) {
            if text(&device, "type") != Some("disk") || is_root_disk(&device) {
                continue;
            }
            let Some(source) = text(&device, "source") else {
                continue;
            };
            let mount = text(&device, "path").unwrap_or("?");
            if let Some(volume_pool) = text(&device, "pool") {
                let on_target = self.target.is_some_and(|target| {
                    target
                        .found(&format!(
                            "/1.0/storage-pools/{}/volumes/custom/{}?project={}",
                            seg(volume_pool),
                            seg(source),
                            seg(project)
                        ))
                        .ok()
                        .flatten()
                        .is_some()
                });
                if on_target {
                    continue;
                }
                if target_pool.is_some_and(|target_pool| target_pool != volume_pool) {
                    parts.risks.push(format!(
                        "the custom volume {volume_pool}/{source} ({device_name}) moves to {}'s pool {volume_pool}, which this check does not size",
                        self.target_name
                    ));
                    continue;
                }
                // Incus keeps a custom volume at custom/<project>_<name>, `default_` included, in the instance's
                // project only when that project sets features.storage.volumes, and in default otherwise.
                let volume_project = if project != "default"
                    && self
                        .source
                        .opt(&format!("/1.0/projects/{}", seg(project)))
                        .and_then(|doc| {
                            doc.get("config")
                                .and_then(|config| text(config, "features.storage.volumes"))
                                .map(|value| value == "true")
                        })
                        .unwrap_or(false)
                {
                    project
                } else {
                    "default"
                };
                let volume_path = pools
                    .join(volume_pool)
                    .join("custom")
                    .join(format!("{volume_project}_{source}"));
                let what = format!("the custom volume {volume_pool}/{source}");
                let block_pool = matches!(
                    self.pool_driver(volume_pool).as_deref(),
                    Some("zfs" | "lvm" | "ceph")
                );
                // A stopped virtual machine's volume on a block driver is not mounted: its size is the one Incus gives.
                let bytes = if vm && block_pool {
                    let configured = |path: String, key: &str| {
                        self.source
                            .opt(&path)
                            .and_then(|doc| doc.get("config")?.get(key)?.as_str().and_then(size_of))
                    };
                    configured(
                        format!(
                            "/1.0/storage-pools/{}/volumes/custom/{}?project={}",
                            seg(volume_pool),
                            seg(source),
                            seg(volume_project)
                        ),
                        "size",
                    )
                    .or_else(|| {
                        configured(
                            format!("/1.0/storage-pools/{}", seg(volume_pool)),
                            "volume.size",
                        )
                    })
                    .ok_or_else(|| {
                        format!(
                            "{what} of the virtual machine {label} has no size Incus names (its own size or the pool's volume.size)"
                        )
                    })?
                } else {
                    self.measure_volume(&volume_path, volume_pool, false, &what)?
                };
                volumes = volumes.saturating_add(bytes);
                words.push(format!(
                    "the custom volume {source} {} at {}",
                    human(bytes),
                    volume_path.display()
                ));
            } else if source.starts_with('/') {
                let bytes = measure_path(Path::new(source))?;
                host_folders = host_folders.saturating_add(bytes);
                words.push(format!(
                    "the host folder {source} at {mount} {}",
                    human(bytes)
                ));
            }
        }
        parts.volumes = Some(volumes);
        parts.host_folders = Some(host_folders);
        let total = own
            .saturating_add(snapshots)
            .saturating_add(volumes)
            .saturating_add(host_folders);
        Ok((
            total,
            format!(
                "measured {} ({total} bytes) {} {}",
                human(total),
                if block_vm {
                    "from the sizes Incus gives:"
                } else {
                    "by walking"
                },
                words.join(", ")
            ),
        ))
    }

    /// The size of a virtual machine's block disk on the pool `pool` here, and where it was read: its volume's `size`, else
    /// its root disk device's `size`, else the size its volume's state reports, else the pool's `volume.size`. It is the
    /// disk's whole size (what the copy may take), not what the guest has written.
    fn vm_disk_bytes(
        &self,
        project: &str,
        name: &str,
        pool: &str,
        instance: &Value,
        label: &str,
    ) -> Result<(u64, &'static str), String> {
        let volume = format!(
            "/1.0/storage-pools/{}/volumes/virtual-machine/{}",
            seg(pool),
            seg(name)
        );
        let query = format!("?project={}", seg(project));
        let configured = |doc: Option<Value>, key: &str| {
            doc.and_then(|doc| doc.get("config")?.get(key)?.as_str().and_then(size_of))
        };
        if let Some(bytes) = configured(self.source.opt(&format!("{volume}{query}")), "size") {
            return Ok((bytes, "its volume's size"));
        }
        let root = instance
            .get("expanded_devices")
            .and_then(Value::as_object)
            .and_then(|devices| devices.values().find(|device| is_root_disk(device)))
            .and_then(|device| text(device, "size"))
            .and_then(size_of);
        if let Some(bytes) = root {
            return Ok((bytes, "its root disk device's size"));
        }
        let total = self
            .source
            .opt(&format!("{volume}/state{query}"))
            .and_then(|state| state.pointer("/usage/total")?.as_u64())
            .filter(|bytes| *bytes > 0);
        if let Some(bytes) = total {
            return Ok((bytes, "the size its volume's state reports"));
        }
        if let Some(bytes) = configured(
            self.source
                .opt(&format!("/1.0/storage-pools/{}", seg(pool))),
            "volume.size",
        ) {
            return Ok((bytes, "the pool's volume.size"));
        }
        Err(format!(
            "{label} is a virtual machine whose disk on pool {pool} has no size Incus names (its volume, its root disk device, its volume's state and the pool's volume.size say none)"
        ))
    }

    fn storage_driver(&self, instance: &Value, pool: &Result<TargetPool, String>) -> Check {
        let mut check = Check::new("storageDriver");
        let source_pool = Self::source_pool(instance);
        let source = source_pool
            .as_deref()
            .and_then(|pool| {
                self.source
                    .opt(&format!("/1.0/storage-pools/{}", seg(pool)))
            })
            .and_then(|doc| text(&doc, "driver").map(str::to_string));
        let target = pool
            .as_ref()
            .ok()
            .and_then(|pool| text(&pool.doc, "driver").map(str::to_string));
        let crosses = match (&source, &target) {
            (Some(source), Some(target)) => Some(source != target),
            _ => None,
        };
        check.extra.insert("sourceDriver".into(), json!(source));
        check.extra.insert("targetDriver".into(), json!(target));
        check.extra.insert("crossesDriver".into(), json!(crosses));
        match (source, target, pool) {
            (_, _, Err(why)) => {
                check.ok = false;
                check.detail = why.clone();
            }
            (Some(source), Some(target), Ok(pool)) if source == target => {
                check.detail = format!(
                    "{source} here and {source} on {}'s pool {}: the same driver, so Incus can send the {source} data as it is",
                    self.target_name, pool.name
                );
                let _ = target;
            }
            (Some(source), Some(target), Ok(pool)) => {
                check.detail = format!(
                    "the move crosses storage drivers: {source} here, {target} on {}'s pool {}",
                    self.target_name, pool.name
                );
                check.risks.push(format!(
                    "a {source} to {target} move is not a {source} send: Incus copies the files one by one, which is slower, and the snapshots are made again on {target}"
                ));
            }
            (source, target, Ok(_)) => {
                check.ok = false;
                check.detail = format!(
                    "the driver could not be read: {} here, {} on {}",
                    source.as_deref().unwrap_or("unknown"),
                    target.as_deref().unwrap_or("unknown"),
                    self.target_name
                );
            }
        }
        check
    }

    fn cpu(&self, instance: &Value) -> Check {
        let mut check = Check::new("cpu");
        check.extra.insert("live".into(), json!(self.live));
        let guest = text(instance, "architecture").map(str::to_string);
        let target = match self.target() {
            Ok(target) => target,
            Err(why) => {
                check.ok = false;
                check.detail = why;
                check.extra.insert("missingFeatures".into(), json!([]));
                check.extra.insert("sourceBaseline".into(), Value::Null);
                check.extra.insert("targetBaseline".into(), Value::Null);
                return check;
            }
        };
        let source_flags = self.source.cpu_flags();
        let target_flags = target.cpu_flags();
        let source_arch = self.source.cpu_architecture();
        let target_arch = target.cpu_architecture();
        let source_baseline = source_arch
            .as_deref()
            .zip(source_flags.as_ref())
            .and_then(|(arch, flags)| baseline(arch, flags));
        let target_baseline = target_arch
            .as_deref()
            .zip(target_flags.as_ref())
            .and_then(|(arch, flags)| baseline(arch, flags));
        let missing: Vec<String> = match (&source_flags, &target_flags) {
            (Some(source), Some(target)) => source.difference(target).cloned().collect(),
            _ => Vec::new(),
        };
        check.extra.insert("missingFeatures".into(), json!(missing));
        check
            .extra
            .insert("sourceBaseline".into(), json!(source_baseline));
        check
            .extra
            .insert("targetBaseline".into(), json!(target_baseline));
        let mut problems = Vec::new();
        if let (Some(guest), Some(runs)) = (&guest, target.architectures())
            && !runs.iter().any(|arch| arch == guest)
        {
            problems.push(format!(
                "the instance is {guest} and {} runs {}",
                self.target_name,
                runs.join(", ")
            ));
        }
        let compared = source_flags.is_some() && target_flags.is_some();
        if self.live {
            if !compared {
                problems.push(format!(
                    "a live move needs {}'s CPU features to cover this host's, and they could not be compared",
                    self.target_name
                ));
            } else if !missing.is_empty() {
                problems.push(format!(
                    "a live move needs every CPU feature of this host on {}, which lacks: {}",
                    self.target_name,
                    missing.join(", ")
                ));
            }
        }
        if !missing.is_empty() {
            let levels = match (&source_baseline, &target_baseline) {
                (Some(source), Some(target)) if source != target => {
                    format!(" (this host is {source}, {} is {target})", self.target_name)
                }
                _ => String::new(),
            };
            check.risks.push(format!(
                "programs in the guest built for this host's CPU{levels} may use features {} lacks: {}",
                self.target_name,
                missing.join(", ")
            ));
        } else if !compared {
            check.risks.push(format!(
                "the CPU features of this host and {} could not be compared",
                self.target_name
            ));
        }
        if problems.is_empty() {
            check.detail = match (&guest, self.live, missing.is_empty() && compared) {
                (_, true, _) => format!(
                    "{} has every CPU feature of this host, so a live move can keep the guest's CPU",
                    self.target_name
                ),
                (_, false, true) => {
                    format!("{} has every CPU feature of this host", self.target_name)
                }
                (_, false, false) => {
                    "a cold move restarts the guest on the target's CPU; see the risks".to_string()
                }
            };
        } else {
            check.ok = false;
            check.detail = problems.join("; ");
        }
        check
    }

    fn devices(
        &self,
        project: &str,
        name: &str,
        instance: &Value,
        pool: &Result<TargetPool, String>,
    ) -> Check {
        let mut check = Check::new("devices");
        let target = match self.target() {
            Ok(target) => target,
            Err(why) => return Check::not_ok("devices", why),
        };
        let t = self.target_name;
        let cards = target.gpu_cards();
        let resources = target.resources();
        for (device_name, device) in devices(instance) {
            let kind = text(&device, "type").unwrap_or("");
            // Incus makes a usb or unix-hotplug device optional unless it says `required=true`, and every other device
            // required unless it says `required=false`.
            let optional = match kind {
                "usb" | "unix-hotplug" => text(&device, "required") != Some("true"),
                _ => text(&device, "required") == Some("false"),
            };
            match kind {
                "gpu" => {
                    let keys = [
                        ("pci", "/pci_address"),
                        ("vendorid", "/vendor_id"),
                        ("productid", "/product_id"),
                    ];
                    let id = text(&device, "id");
                    let matches = cards.iter().any(|card| {
                        keys.iter().all(|(key, pointer)| {
                            text(&device, key).is_none_or(|wanted| {
                                card.pointer(pointer)
                                    .and_then(Value::as_str)
                                    .is_some_and(|have| {
                                        have.eq_ignore_ascii_case(wanted) || have.ends_with(wanted)
                                    })
                            })
                        }) && id.is_none_or(|id| {
                            card.pointer("/drm/id").map(Value::to_string).as_deref() == Some(id)
                        })
                    });
                    let wanted = ["pci", "vendorid", "productid", "id"]
                        .iter()
                        .filter_map(|key| text(&device, key).map(|value| format!("{key} {value}")))
                        .collect::<Vec<_>>();
                    let wanted = if wanted.is_empty() {
                        "any GPU".to_string()
                    } else {
                        format!("the GPU with {}", wanted.join(", "))
                    };
                    check.add(if matches {
                        finding(&device_name, true, format!("{t} has {wanted}"), None)
                    } else {
                        finding(
                            &device_name,
                            false,
                            format!("{t} has no {}", wanted.trim_start_matches("the ")),
                            Some(format!("move it to a node with {wanted}, or remove the gpu device {device_name} first")),
                        )
                    });
                }
                "unix-char" | "unix-block" => {
                    let path = text(&device, "source")
                        .or_else(|| text(&device, "path"))
                        .unwrap_or("");
                    let (ok, detail, proposal) = if let Some(node) = path.strip_prefix("/dev/dri/")
                    {
                        let have: Vec<String> = cards
                            .iter()
                            .flat_map(|card| {
                                ["/drm/card_name", "/drm/render_name"]
                                    .iter()
                                    .filter_map(|pointer| {
                                        card.pointer(pointer).and_then(Value::as_str)
                                    })
                                    .map(str::to_string)
                                    .collect::<Vec<_>>()
                            })
                            .collect();
                        if have.iter().any(|listed| listed == node) {
                            (true, format!("{t} has the GPU node {path}"), None)
                        } else {
                            (
                                false,
                                format!(
                                    "{t} has no GPU node {path} (its GPU nodes: {})",
                                    if have.is_empty() {
                                        "none".to_string()
                                    } else {
                                        have.join(", ")
                                    }
                                ),
                                Some(format!(
                                    "point {device_name} at a GPU node {t} has, or move it to a node with {node}"
                                )),
                            )
                        }
                    } else if STANDARD_DEVICES.contains(&path) {
                        (true, format!("{path} is on every Linux host"), None)
                    } else if path == "/dev/kvm" {
                        let qemu = target
                            .opt("/1.0")
                            .and_then(|server| {
                                text(server.get("environment").unwrap_or(&Value::Null), "driver")
                                    .map(str::to_string)
                            })
                            .is_some_and(|driver| driver.contains("qemu"));
                        if qemu {
                            (
                                true,
                                format!("{t} runs virtual machines, so it has /dev/kvm"),
                                None,
                            )
                        } else {
                            (
                                false,
                                format!(
                                    "{t} does not run virtual machines, so /dev/kvm may be missing"
                                ),
                                None,
                            )
                        }
                    } else {
                        (
                            false,
                            format!(
                                "{path} is a host device Incus does not list, so it cannot be confirmed on {t}"
                            ),
                            Some(format!(
                                "confirm {path} exists on {t}, or set required=false on {device_name} if the instance runs without it"
                            )),
                        )
                    };
                    if !ok && optional {
                        check.add(finding(
                            &device_name,
                            true,
                            format!("{detail}; the device is not required"),
                            None,
                        ));
                        check.risks.push(format!("{device_name} ({path}) may be missing on {t}; the instance starts without it"));
                    } else {
                        check.add(finding(&device_name, ok, detail, proposal));
                    }
                }
                "disk" if is_root_disk(&device) => {}
                "disk" => {
                    let source = text(&device, "source").unwrap_or("");
                    let mount = text(&device, "path").unwrap_or("");
                    if let Some(volume_pool) = text(&device, "pool") {
                        let present = target
                            .found(&format!(
                                "/1.0/storage-pools/{}/volumes/custom/{}?project={}",
                                seg(volume_pool),
                                seg(source),
                                seg(project)
                            ))
                            .ok()
                            .flatten()
                            .is_some();
                        check.add(if present {
                            finding(&device_name, true, format!("{t} has the custom volume {volume_pool}/{source}"), None)
                        } else {
                            finding(
                                &device_name,
                                false,
                                format!("the custom volume {volume_pool}/{source} mounted at {mount} is not on {t}"),
                                Some(format!("move the volume {volume_pool}/{source} to {t} before the instance")),
                            )
                        });
                    } else if source.starts_with('/') {
                        let target_pool = pool
                            .as_ref()
                            .map(|pool| pool.name.clone())
                            .unwrap_or_else(|_| "default".to_string());
                        let volume = format!("{name}-{device_name}");
                        check.add(finding(
                            &device_name,
                            false,
                            format!("the host folder {source} mounted at {mount} is on this host only; its data does not move with the instance"),
                            Some(format!(
                                "copy {source} into a custom volume {volume} on {t}'s pool {target_pool} and attach that volume at {mount} in place of the host folder"
                            )),
                        ));
                    } else {
                        check.add(finding(
                            &device_name,
                            true,
                            format!("{source} is network storage, not a folder of this host"),
                            None,
                        ));
                    }
                }
                "usb" | "unix-hotplug" | "pci" => {
                    let (list, keys): (&str, &[(&str, &str)]) = if kind != "pci" {
                        (
                            "/usb/devices",
                            &[("vendorid", "/vendor_id"), ("productid", "/product_id")],
                        )
                    } else {
                        ("/pci/devices", &[("address", "/pci_address")])
                    };
                    let present = resources
                        .as_ref()
                        .and_then(|resources| resources.pointer(list))
                        .and_then(Value::as_array)
                        .is_some_and(|devices| {
                            devices.iter().any(|candidate| {
                                keys.iter().all(|(key, pointer)| {
                                    text(&device, key).is_none_or(|wanted| {
                                        candidate.pointer(pointer).and_then(Value::as_str)
                                            == Some(wanted)
                                    })
                                })
                            })
                        });
                    let wanted = keys
                        .iter()
                        .filter_map(|(key, _)| {
                            text(&device, key).map(|value| format!("{key} {value}"))
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    if !present && optional {
                        check.risks.push(format!(
                            "{device_name} may be missing on {t}; the instance starts without it"
                        ));
                    }
                    check.add(if present {
                        finding(
                            &device_name,
                            true,
                            format!("{t} has the {kind} device {wanted}"),
                            None,
                        )
                    } else if optional {
                        finding(
                            &device_name,
                            true,
                            format!(
                                "{t} lists no {kind} device {wanted}; the device is not required"
                            ),
                            None,
                        )
                    } else {
                        finding(
                            &device_name,
                            false,
                            format!("{t} lists no {kind} device {wanted}"),
                            Some(format!(
                                "plug the device into {t}, or remove {device_name} first"
                            )),
                        )
                    });
                }
                _ => {}
            }
        }
        check.summarize(
            &format!("no device needs anything of {t}'s hardware or folders"),
            &format!("{t} has every device the instance uses"),
        );
        check
    }

    fn networks(&self, project: &str, instance: &Value) -> Check {
        let mut check = Check::new("networks");
        let target = match self.target() {
            Ok(target) => target,
            Err(why) => return Check::not_ok("networks", why),
        };
        let t = self.target_name;
        let config = instance
            .get("expanded_config")
            .or_else(|| instance.get("config"))
            .cloned()
            .unwrap_or(Value::Null);
        for (device_name, device) in devices(instance) {
            if text(&device, "type") != Some("nic") {
                continue;
            }
            let nictype = text(&device, "nictype");
            if let Some(network) = text(&device, "network") {
                let here = self.source.opt(&format!(
                    "/1.0/networks/{}?project={}",
                    seg(network),
                    seg(project)
                ));
                let there = target.found(&format!(
                    "/1.0/networks/{}?project={}",
                    seg(network),
                    seg(project)
                ));
                let same_subnet = |family: &str| -> Option<String> {
                    here.as_ref()
                        .and_then(|net| net.get("config"))
                        .and_then(|config| text(config, &format!("{family}.address")))
                        .filter(|value| subnet(value).is_some())
                        .map(str::to_string)
                };
                let there = match there {
                    Ok(there) => there,
                    Err(error) => {
                        check.add(finding(
                            &device_name,
                            false,
                            format!("{t}'s network {network} could not be read: {error}"),
                            None,
                        ));
                        continue;
                    }
                };
                let Some(there) = there else {
                    let kind = here
                        .as_ref()
                        .and_then(|net| text(net, "type"))
                        .unwrap_or("bridge");
                    let subnets = ["ipv4", "ipv6"]
                        .iter()
                        .filter_map(|family| {
                            same_subnet(family).map(|cidr| format!("{family}.address={cidr}"))
                        })
                        .collect::<Vec<_>>();
                    let proposal = if subnets.is_empty() {
                        format!("create the {kind} network {network} on {t}")
                    } else {
                        format!(
                            "create the {kind} network {network} on {t} with {}, the same subnet as here",
                            subnets.join(" and ")
                        )
                    };
                    check.add(finding(
                        &device_name,
                        false,
                        format!("{t} has no network {network}"),
                        Some(proposal),
                    ));
                    continue;
                };
                let mut problems = Vec::new();
                let mut proposals = Vec::new();
                for family in ["ipv4", "ipv6"] {
                    let theirs = there
                        .get("config")
                        .and_then(|config| text(config, &format!("{family}.address")))
                        .filter(|value| *value != "none");
                    let ours = same_subnet(family);
                    let Some(fixed) = text(&device, &format!("{family}.address")) else {
                        if let (Some(ours), Some(theirs)) = (&ours, theirs)
                            && ours != theirs
                        {
                            check.risks.push(format!(
                                "{device_name} gets an address of {theirs} on {t} instead of {ours} here"
                            ));
                        }
                        continue;
                    };
                    let Ok(address) = fixed.parse::<IpAddr>() else {
                        continue;
                    };
                    let same = ours.as_deref().map(|cidr| {
                        format!(
                            "set {network}'s {family}.address on {t} to {cidr}, the subnet here"
                        )
                    });
                    match theirs.and_then(subnet) {
                        None => {
                            problems.push(format!("{t}'s {network} has no {family} subnet for the fixed address {fixed}"));
                            proposals.extend(same);
                        }
                        Some(net) if !contains(net, address) => {
                            problems.push(format!(
                                "the fixed address {fixed} is outside {t}'s {network} subnet {}",
                                theirs.unwrap_or_default()
                            ));
                            proposals.extend(same.map(|same| {
                                format!(
                                    "{same}, or give {device_name} an address in {}",
                                    theirs.unwrap_or_default()
                                )
                            }));
                        }
                        Some(_) => {
                            let own_mac = text(&config, &format!("volatile.{device_name}.hwaddr"))
                                .or_else(|| text(&device, "hwaddr"));
                            let leases = target
                                .opt(&format!(
                                    "/1.0/networks/{}/leases?project={}",
                                    seg(network),
                                    seg(project)
                                ))
                                .and_then(|leases| leases.as_array().cloned())
                                .unwrap_or_default();
                            if let Some(lease) = leases.iter().find(|lease| {
                                text(lease, "address") == Some(fixed)
                                    && text(lease, "hwaddr").is_some_and(|mac| Some(mac) != own_mac)
                            }) {
                                let holder = text(lease, "hwaddr").unwrap_or("another card");
                                let host = text(lease, "hostname")
                                    .map(|h| format!(" ({h})"))
                                    .unwrap_or_default();
                                problems.push(format!("the fixed address {fixed} is already held on {t}'s {network} by {holder}{host}"));
                                proposals.push(format!("free {fixed} on {t}, or give {device_name} another address in {}", theirs.unwrap_or_default()));
                            }
                        }
                    }
                }
                check.add(if problems.is_empty() {
                    finding(
                        &device_name,
                        true,
                        format!("{t} has the network {network} with room for the card's address"),
                        None,
                    )
                } else {
                    finding(
                        &device_name,
                        false,
                        problems.join("; "),
                        (!proposals.is_empty()).then(|| proposals.join("; ")),
                    )
                });
            } else if nictype == Some("macvlan") {
                let parent = text(&device, "parent").unwrap_or("?");
                check.add(finding(
                    &device_name,
                    true,
                    format!("a macvlan card on {parent}; its parent is checked in macvlanParents"),
                    None,
                ));
            } else if let Some(parent) = text(&device, "parent") {
                let kind = nictype.unwrap_or("nic");
                if target.has_interface(parent) {
                    if text(&device, "ipv4.address").is_some()
                        || text(&device, "ipv6.address").is_some()
                    {
                        check.risks.push(format!(
                            "{device_name}'s fixed address on {parent} cannot be checked: {parent} is not a network Incus manages"
                        ));
                    }
                    check.add(finding(
                        &device_name,
                        true,
                        format!("{t} has {parent} for this {kind} card"),
                        None,
                    ));
                } else {
                    check.add(finding(
                        &device_name,
                        false,
                        format!("{t} has no interface {parent} for this {kind} card"),
                        Some(format!(
                            "point {device_name} at one of {t}'s own interfaces: {}",
                            target.interfaces().join(", ")
                        )),
                    ));
                }
            } else {
                check.add(finding(
                    &device_name,
                    true,
                    "the card names no network or parent of this host",
                    None,
                ));
            }
        }
        check.summarize(
            "the instance has no network card",
            &format!("{t} has every network and address the instance uses"),
        );
        check
    }

    fn projects_profiles_acls(&self, project: &str, instance: &Value) -> Check {
        let mut check = Check::new("projectsProfilesAcls");
        let target = match self.target() {
            Ok(target) => target,
            Err(why) => return Check::not_ok("projectsProfilesAcls", why),
        };
        let t = self.target_name;
        let features = |doc: &Value| -> BTreeMap<String, String> {
            doc.get("config")
                .and_then(Value::as_object)
                .map(|config| {
                    config
                        .iter()
                        .filter(|(key, _)| key.starts_with("features."))
                        .map(|(key, value)| {
                            (key.clone(), value.as_str().unwrap_or_default().to_string())
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        let here = self.source.opt(&format!("/1.0/projects/{}", seg(project)));
        let ours = here.as_ref().map(features).unwrap_or_default();
        let said = if ours.is_empty() {
            "the default features".to_string()
        } else {
            ours.iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let subject = format!("project {project}");
        check.add(
            match target.found(&format!("/1.0/projects/{}", seg(project))) {
                Ok(Some(there)) => {
                    let theirs = features(&there);
                    let differ: Vec<String> = ours
                        .keys()
                        .chain(theirs.keys())
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .filter(|key| ours.get(*key) != theirs.get(*key))
                        .cloned()
                        .collect();
                    if differ.is_empty() {
                        finding(
                            subject,
                            true,
                            format!("{t} has the project {project} with the same features"),
                            None,
                        )
                    } else {
                        finding(
                            subject,
                            false,
                            format!("{t}'s project {project} differs in {}", differ.join(", ")),
                            Some(format!("set {t}'s project {project} to {said}")),
                        )
                    }
                }
                Ok(None) => finding(
                    subject,
                    false,
                    format!("{t} has no project {project}"),
                    Some(format!("create the project {project} on {t} with {said}")),
                ),
                Err(error) => finding(
                    subject,
                    false,
                    format!("{t}'s project {project} could not be read: {error}"),
                    None,
                ),
            },
        );
        let profiles: Vec<String> = instance
            .get("profiles")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        for profile in profiles {
            let subject = format!("profile {profile}");
            let path = format!("/1.0/profiles/{}?project={}", seg(&profile), seg(project));
            let ours = self.source.opt(&path);
            check.add(match target.found(&path) {
                Ok(Some(theirs)) => {
                    let differ = ours.as_ref().map(|ours| differences(ours, &theirs)).unwrap_or_default();
                    if differ.is_empty() {
                        finding(subject, true, format!("{t} has the profile {profile}, the same as here"), None)
                    } else {
                        finding(
                            subject,
                            false,
                            format!("{t}'s profile {profile} differs in {}", differ.join(", ")),
                            Some(format!("make {t}'s profile {profile} the same as here, or check that the difference is meant")),
                        )
                    }
                }
                Ok(None) => finding(
                    subject,
                    false,
                    format!("{t} has no profile {profile} in {project}"),
                    Some(format!("create the profile {profile} in {project} on {t} with the same config and devices as here")),
                ),
                Err(error) => finding(subject, false, format!("{t}'s profile {profile} could not be read: {error}"), None),
            });
        }
        let mut acls = BTreeSet::new();
        for (_, device) in devices(instance) {
            if text(&device, "type") != Some("nic") {
                continue;
            }
            let mut lists = vec![text(&device, "security.acls").map(str::to_string)];
            if let Some(network) = text(&device, "network") {
                lists.push(
                    self.source
                        .opt(&format!(
                            "/1.0/networks/{}?project={}",
                            seg(network),
                            seg(project)
                        ))
                        .and_then(|net| {
                            net.get("config")
                                .and_then(|c| text(c, "security.acls"))
                                .map(str::to_string)
                        }),
                );
            }
            for list in lists.into_iter().flatten() {
                acls.extend(
                    list.split(',')
                        .map(str::trim)
                        .filter(|acl| !acl.is_empty())
                        .map(str::to_string),
                );
            }
        }
        for acl in acls {
            let subject = format!("acl {acl}");
            let path = format!("/1.0/network-acls/{}?project={}", seg(&acl), seg(project));
            let ours = self.source.opt(&path);
            check.add(match target.found(&path) {
                Ok(Some(theirs)) => {
                    let rules =
                        |doc: &Value| (doc.get("ingress").cloned(), doc.get("egress").cloned());
                    if ours
                        .as_ref()
                        .is_none_or(|ours| rules(ours) == rules(&theirs))
                    {
                        finding(
                            subject,
                            true,
                            format!("{t} has the ACL {acl} with the same rules"),
                            None,
                        )
                    } else {
                        finding(
                            subject,
                            false,
                            format!("{t}'s ACL {acl} has other rules than here"),
                            Some(format!("give {t}'s ACL {acl} the rules it has here")),
                        )
                    }
                }
                Ok(None) => finding(
                    subject,
                    false,
                    format!("{t} has no ACL {acl} for {project}"),
                    Some(format!(
                        "create the ACL {acl} for {project} on {t} with the same rules as here"
                    )),
                ),
                Err(error) => finding(
                    subject,
                    false,
                    format!("{t}'s ACL {acl} could not be read: {error}"),
                    None,
                ),
            });
        }
        check.summarize(
            "",
            &format!("{t} has the project, the profiles and the ACLs, the same as here"),
        );
        check
    }

    fn proxies(&self, instance: &Value) -> Check {
        let mut check = Check::new("proxyListen");
        let target = match self.target() {
            Ok(target) => target,
            Err(why) => return Check::not_ok("proxyListen", why),
        };
        let t = self.target_name;
        for (device_name, device) in devices(instance) {
            if text(&device, "type") != Some("proxy") {
                continue;
            }
            let listen = text(&device, "listen").unwrap_or("");
            if text(&device, "bind") == Some("instance") {
                check.add(finding(
                    &device_name,
                    true,
                    format!("{listen} is inside the instance, so it moves with it"),
                    None,
                ));
                continue;
            }
            let Some((protocol, rest)) = listen.split_once(':') else {
                check.add(finding(
                    &device_name,
                    true,
                    format!("{listen} names no host address"),
                    None,
                ));
                continue;
            };
            if protocol == "unix" {
                check.risks.push(format!(
                    "{device_name} listens on the host socket {rest}; its folder must exist on {t}"
                ));
                check.add(finding(
                    &device_name,
                    true,
                    format!("{device_name} listens on the host socket {rest}"),
                    None,
                ));
                continue;
            }
            let (address, ports) = match rest.strip_prefix('[') {
                Some(v6) => match v6.split_once("]:") {
                    Some((address, ports)) => (address, ports),
                    None => (v6.trim_end_matches(']'), ""),
                },
                None => rest.rsplit_once(':').unwrap_or((rest, "")),
            };
            let Ok(ip) = address.parse::<IpAddr>() else {
                check.risks.push(format!("{device_name} listens on {address}, which is not an address; check it resolves on {t}"));
                check.add(finding(
                    &device_name,
                    true,
                    format!("{device_name} listens on the name {address}"),
                    None,
                ));
                continue;
            };
            let port = if ports.is_empty() {
                String::new()
            } else {
                format!(" port {ports}")
            };
            if ip.is_unspecified() || ip.is_loopback() {
                check.add(finding(
                    &device_name,
                    true,
                    format!("{device_name} listens on {ip}{port}, which every host has"),
                    None,
                ));
                continue;
            }
            let ours = self.source.addresses().iter().find(|(held, _)| *held == ip);
            let theirs = target.addresses().iter().any(|(held, _)| *held == ip);
            match (ours, theirs) {
                (_, true) => check.add(finding(
                    &device_name,
                    true,
                    format!("{device_name} listens on {ip}{port}, which {t} holds too"),
                    None,
                )),
                (Some((_, interface)), false) => {
                    let choices: Vec<String> = target
                        .addresses()
                        .iter()
                        .filter(|(held, _)| {
                            held.is_ipv4() == ip.is_ipv4()
                                && !held.is_loopback()
                                && !is_link_local(*held)
                        })
                        .map(|(held, interface)| format!("{held} ({interface})"))
                        .collect();
                    let choices = if choices.is_empty() {
                        format!("an address {t} holds")
                    } else {
                        format!("one of {t}'s own addresses: {}", choices.join(", "))
                    };
                    check.add(finding(
                        &device_name,
                        false,
                        format!(
                            "the proxy {device_name} listens on {ip}{port}, an address of this host ({interface}) that {t} does not have; on {t} it would not listen"
                        ),
                        Some(format!("re-point {device_name}'s listen to {choices}, or to 0.0.0.0 for every address")),
                    ));
                }
                (None, false) => {
                    check.risks.push(format!(
                        "{device_name} listens on {ip}, which neither this host nor {t} holds"
                    ));
                    check.add(finding(
                        &device_name,
                        true,
                        format!("{device_name} listens on {ip}{port}, not an address of this host"),
                        None,
                    ));
                }
            }
        }
        check.summarize(
            "the instance has no proxy device",
            &format!("every proxy listens on an address {t} can take"),
        );
        check
    }

    fn macvlan(&self, project: &str, instance: &Value) -> Check {
        let mut check = Check::new("macvlanParents");
        let target = match self.target() {
            Ok(target) => target,
            Err(why) => return Check::not_ok("macvlanParents", why),
        };
        let t = self.target_name;
        for (device_name, device) in devices(instance) {
            if text(&device, "type") != Some("nic") {
                continue;
            }
            if let Some(network) = text(&device, "network") {
                let here = self.source.opt(&format!(
                    "/1.0/networks/{}?project={}",
                    seg(network),
                    seg(project)
                ));
                if here.as_ref().and_then(|net| text(net, "type")) != Some("macvlan") {
                    continue;
                }
                let present = target
                    .found(&format!(
                        "/1.0/networks/{}?project={}",
                        seg(network),
                        seg(project)
                    ))
                    .ok()
                    .flatten()
                    .is_some();
                check.add(if present {
                    finding(
                        &device_name,
                        true,
                        format!("{t} has its own macvlan network {network}"),
                        None,
                    )
                } else {
                    let parent = here
                        .as_ref()
                        .and_then(|net| net.get("config"))
                        .and_then(|config| text(config, "parent"))
                        .unwrap_or("?");
                    finding(
                        &device_name,
                        false,
                        format!("{t} has no macvlan network {network} (here on {parent})"),
                        Some(format!(
                            "create {network} on {t} on one of its own interfaces: {}",
                            parents(target).join(", ")
                        )),
                    )
                });
                continue;
            }
            if text(&device, "nictype") != Some("macvlan") {
                continue;
            }
            let parent = text(&device, "parent").unwrap_or("");
            if target.has_interface(parent) {
                check.add(finding(
                    &device_name,
                    true,
                    format!("{t} has the macvlan parent {parent}"),
                    None,
                ));
                continue;
            }
            let names = target.interfaces();
            let candidates = parents(target);
            let mut proposal = if candidates.is_empty() {
                format!("set {device_name}'s parent to an interface {t} has")
            } else {
                format!(
                    "set {device_name}'s parent to one of {t}'s own names: {}",
                    candidates.join(", ")
                )
            };
            if names.iter().any(|name| name.starts_with("_p")) || parent.starts_with("_p") {
                proposal.push_str(" (on IncusOS the bridge carries the network card's name and a _p port is not a parent)");
            }
            check.add(finding(
                &device_name,
                false,
                format!(
                    "the macvlan parent {parent} is not an interface on {t}; {t}'s interfaces are {}",
                    if names.is_empty() { "unknown".to_string() } else { names.join(", ") }
                ),
                Some(proposal),
            ));
        }
        check.summarize(
            "the instance has no macvlan card",
            &format!("{t} has every macvlan parent the instance asks for"),
        );
        check
    }
}

/// The interfaces of `host` a macvlan card may sit on: its own names, an IncusOS bridge port (`_p…`) left out.
fn parents(host: &Host) -> Vec<String> {
    host.interfaces()
        .iter()
        .filter(|name| !name.starts_with("_p"))
        .cloned()
        .collect()
}

fn is_link_local(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => v4.is_link_local(),
        IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
    }
}

/// Where two profiles differ: each config key and each device whose value is not the same.
fn differences(ours: &Value, theirs: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for (field, label) in [("config", "config"), ("devices", "device")] {
        let empty = Map::new();
        let a = ours.get(field).and_then(Value::as_object).unwrap_or(&empty);
        let b = theirs
            .get(field)
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        let keys: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
        for key in keys {
            if a.get(key) != b.get(key) {
                out.push(format!("{label} {key}"));
            }
        }
    }
    out
}

impl MovePreflightHandler {
    /// The target's Incus, or why it cannot be read.
    fn open_target(&self, name: &str) -> Result<Host, String> {
        let linked = self
            .cfg
            .approval
            .as_ref()
            .and_then(|approval| approval.linked_hosts.iter().find(|host| host.name == name))
            .ok_or_else(|| format!("no linked host named {name} in approval.linkedHosts"))?;
        let socket = linked.incus_socket.clone().ok_or_else(|| {
            format!(
                "the linked host {name} has no incusSocket, so its Incus cannot be read from here"
            )
        })?;
        let host = Host::new(socket.clone());
        match host.incus.get_within("/1.0", TARGET_PROBE) {
            Ok(server) => {
                host.cache
                    .borrow_mut()
                    .insert("/1.0".to_string(), Ok(server));
                Ok(host)
            }
            Err(error) => Err(format!(
                "{name}'s Incus could not be read through {}: {error}",
                socket.display()
            )),
        }
    }
}

impl ToolHandler for MovePreflightHandler {
    fn call(&self, args: Value, _caller: &Caller) -> Result<Value, CallError> {
        let (references, target_name, live) = parse_args(&args)?;
        let source = Host::new(self.cfg.incus.socket.clone());
        let opened = self.open_target(&target_name);
        let (target, target_why) = match opened {
            Ok(host) => (Some(host), None),
            Err(why) => (None, Some(why)),
        };
        let mut run = Run {
            source: &source,
            target: target.as_ref(),
            target_name: &target_name,
            target_why: target_why.clone(),
            storage_root: &self.storage_root,
            live,
            pending: BTreeMap::new(),
        };
        let rows: Vec<Value> = references
            .iter()
            .map(|reference| run.row(reference))
            .collect();
        Ok(json!({
            "target": { "name": target_name, "reachable": target_why.is_none(), "reason": target_why },
            "live": live,
            "rows": rows,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subnets_hold_their_own_addresses_only() {
        let net = subnet("192.0.2.1/24").expect("subnet");
        assert!(contains(net, "192.0.2.50".parse().expect("ip")));
        assert!(!contains(net, "198.51.100.50".parse().expect("ip")));
        let net = subnet("2001:db8:1::1/64").expect("subnet");
        assert!(contains(net, "2001:db8:1::99".parse().expect("ip")));
        assert!(!contains(net, "2001:db8:2::1".parse().expect("ip")));
        assert!(subnet("none").is_none());
    }

    #[test]
    fn baselines_follow_the_x86_64_levels() {
        let flags = |list: &[&str]| list.iter().map(|f| f.to_string()).collect::<BTreeSet<_>>();
        assert_eq!(
            baseline("x86_64", &flags(&["sse2"])).as_deref(),
            Some("x86-64-v1")
        );
        assert_eq!(
            baseline("x86_64", &flags(X86_64_V2)).as_deref(),
            Some("x86-64-v2")
        );
        let v3: Vec<&str> = X86_64_V2.iter().chain(X86_64_V3).copied().collect();
        assert_eq!(
            baseline("x86_64", &flags(&v3)).as_deref(),
            Some("x86-64-v3")
        );
        let v4: Vec<&str> = v3.iter().chain(X86_64_V4).copied().collect();
        assert_eq!(
            baseline("x86_64", &flags(&v4)).as_deref(),
            Some("x86-64-v4")
        );
        assert_eq!(baseline("aarch64", &flags(&v4)), None);
    }

    #[test]
    fn a_mount_point_is_a_folder_on_another_device_than_its_parent() {
        assert!(is_mount_point(Path::new("/proc")));
        let dir = std::env::temp_dir().join(format!("arca-mount-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("inner")).expect("dir");
        assert!(!is_mount_point(&dir.join("inner")));
        assert!(!is_mount_point(&dir.join("missing")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sizes_read_in_binary_units() {
        assert_eq!(human(512), "512 B");
        assert_eq!(human(4 * 1024 * 1024 + 400 * 1024), "4.4 MiB");
        assert_eq!(human(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }
}
