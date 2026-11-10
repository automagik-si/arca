//! Snapshot storage and snapshots: the owner plugs a disk into any Arca machine, picks it as snapshot storage, and copies
//! every app and data volume onto it, so a reinstalled machine can bring its apps back from that disk alone.
//!
//! `storage_list` reads the host's disks (`lsblk`) and the Incus pools, and gives each disk one plain role:
//!
//! - **system**: it holds `/`, `/boot` or swap, it is a RAID, LVM, LUKS or ZFS member, or it holds an Incus pool. It is never
//!   offered.
//! - **in use**: it is mounted somewhere else. It is offered only as a folder target: a `dir` pool in a folder on it.
//! - **free**: nothing on it is mounted or a member of anything. It is offered as a whole-disk target, which Incus erases and
//!   formats as btrfs.
//!
//! `storage_add`, `snapshot_create` and `snapshot_delete` are gated: a viewer proposes one, the owner approves it with the
//! passkey, and the approved action runs once. Every write goes through Incus ([`arca_incus::SnapshotWriter`]); this process
//! never formats, mounts or writes a disk. The facts a proposal depends on (the disk's model, size, serial, transport and
//! contents; a source's kind and identity; a snapshot's name) are read when it is made and bound into its arguments, so the
//! owner's digest covers them and the run refuses anything that changed. A watch-only node reads but has no writer.

use std::collections::BTreeSet;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arca_incus::snapshot::{
    CREATED_KEY, DROPPED_KEY, SNAPSHOT_LABEL_PREFIX, SOURCE_KIND_KEY, SOURCE_NAME_KEY,
    SOURCE_POOL_KEY, SOURCE_PROJECT_KEY, folder_pool_path, instance_copy_body, plain_name,
    volume_copy_body,
};
use arca_incus::{
    IncusError, IncusRead, PoolSource, Progress, SNAPSHOT_PROJECT, SnapshotWriter, UnixIncus,
};
use arca_mcp::{CallError, Caller, ToolHandler};
use serde_json::{Value, json};

use crate::action_proposal::{ActionDetail, ActionKind};
use crate::kernel::Kernel;

/// The `lsblk` columns the survey reads.
const LSBLK_COLUMNS: &str =
    "NAME,PATH,SIZE,MODEL,SERIAL,TRAN,RM,TYPE,FSTYPE,LABEL,UUID,MOUNTPOINTS";
/// How long one `lsblk` run may take.
const LSBLK_TIMEOUT: Duration = Duration::from_secs(5);
/// File systems that make a device part of something larger.
const MEMBER_FSTYPES: [&str; 6] = [
    "linux_raid_member",
    "LVM2_member",
    "crypto_LUKS",
    "zfs_member",
    "bcache",
    "ceph",
];
/// The longest snapshot name Incus accepts for an instance.
const MAX_SNAPSHOT_NAME: usize = 63;

/// The host's block devices as `lsblk --json` prints them. A test gives a fixture; the node runs `lsblk`.
pub trait BlockDevices: Send + Sync {
    fn read(&self) -> Result<Value, String>;
}

/// The node's `lsblk`, run read-only with a deadline.
pub struct Lsblk;

impl BlockDevices for Lsblk {
    fn read(&self) -> Result<Value, String> {
        let program = ["/usr/bin/lsblk", "/bin/lsblk"]
            .into_iter()
            .find(|path| Path::new(path).exists())
            .ok_or("lsblk is not installed")?;
        let mut child = Command::new(program)
            .args(["--json", "--bytes", "--tree", "--output", LSBLK_COLUMNS])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| "lsblk could not be started".to_string())?;
        let mut stdout = child.stdout.take().ok_or("lsblk has no output")?;
        let reader = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = stdout.read_to_string(&mut text);
            text
        });
        let until = Instant::now() + LSBLK_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => break,
                Ok(Some(_)) => return Err("lsblk failed".to_string()),
                Ok(None) if Instant::now() < until => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("lsblk did not answer in time".to_string());
                }
            }
        }
        let text = reader.join().map_err(|_| "lsblk output was lost")?;
        serde_json::from_str(&text).map_err(|_| "lsblk output is not JSON".to_string())
    }
}

/// A fixed `lsblk` answer, for tests.
pub struct FixedBlockDevices(pub Mutex<Result<Value, String>>);

impl FixedBlockDevices {
    pub fn new(value: Value) -> Self {
        Self(Mutex::new(Ok(value)))
    }

    pub fn set(&self, value: Result<Value, String>) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = value;
    }
}

impl BlockDevices for FixedBlockDevices {
    fn read(&self) -> Result<Value, String> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// One block device and the devices on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockDevice {
    pub name: String,
    pub path: String,
    pub size: u64,
    pub model: String,
    pub serial: String,
    pub transport: String,
    pub removable: bool,
    pub kind: String,
    pub fstype: Option<String>,
    pub label: Option<String>,
    pub uuid: Option<String>,
    pub mountpoints: Vec<String>,
    pub children: Vec<BlockDevice>,
}

impl BlockDevice {
    /// This device and every device on it, depth first.
    fn walk(&self) -> Vec<&BlockDevice> {
        let mut out = vec![self];
        for child in &self.children {
            out.extend(child.walk());
        }
        out
    }
}

fn text_field(row: &Value, key: &str) -> Option<String> {
    match row.get(key) {
        Some(Value::String(text)) => {
            let text = text.trim();
            (!text.is_empty()).then(|| text.to_string())
        }
        Some(Value::Number(number)) => Some(number.to_string()),
        _ => None,
    }
}

fn parse_device(row: &Value) -> Option<BlockDevice> {
    let name = text_field(row, "name")?;
    let size = match row.get("size") {
        Some(Value::Number(number)) => number.as_u64().unwrap_or(0),
        Some(Value::String(text)) => text.trim().parse().unwrap_or(0),
        _ => 0,
    };
    let removable = match row.get("rm") {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::String(text)) => text.trim() == "1" || text.trim() == "true",
        Some(Value::Number(number)) => number.as_u64() == Some(1),
        _ => false,
    };
    let mut mountpoints: Vec<String> = match row.get("mountpoints") {
        Some(Value::Array(list)) => list
            .iter()
            .filter_map(Value::as_str)
            .filter(|mount| !mount.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    };
    if let Some(mount) = text_field(row, "mountpoint")
        && !mountpoints.contains(&mount)
    {
        mountpoints.push(mount);
    }
    let children = row
        .get("children")
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(parse_device).collect())
        .unwrap_or_default();
    Some(BlockDevice {
        path: text_field(row, "path").unwrap_or_else(|| format!("/dev/{name}")),
        name,
        size,
        model: text_field(row, "model").unwrap_or_default(),
        serial: text_field(row, "serial").unwrap_or_default(),
        transport: text_field(row, "tran").unwrap_or_default(),
        removable,
        kind: text_field(row, "type").unwrap_or_default(),
        fstype: text_field(row, "fstype"),
        label: text_field(row, "label"),
        uuid: text_field(row, "uuid"),
        mountpoints,
        children,
    })
}

/// The whole disks of an `lsblk --json --tree` answer. Loop, optical, RAM and zram devices are not disks Arca offers and are
/// left out.
pub fn parse_lsblk(value: &Value) -> Result<Vec<BlockDevice>, String> {
    let rows = value
        .get("blockdevices")
        .and_then(Value::as_array)
        .ok_or("lsblk output has no device list")?;
    Ok(rows
        .iter()
        .filter_map(parse_device)
        .filter(|device| device.kind == "disk" && !device.name.starts_with("zram"))
        .collect())
}

/// One Incus storage pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolView {
    pub name: String,
    pub driver: String,
    /// Where the pool lives, as Incus names it: a path, a device, a UUID or a name.
    pub sources: Vec<String>,
    pub used: Option<u64>,
    pub total: Option<u64>,
    /// Whether the pool was added as snapshot storage.
    pub snapshots: bool,
}

impl PoolView {
    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "driver": self.driver,
            "source": self.sources.first().cloned().unwrap_or_default(),
            "used": self.used,
            "total": self.total,
            "snapshots": self.snapshots,
        })
    }
}

/// A disk's plain role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskRole {
    System,
    /// Holds Arca snapshots: a snapshot pool of this machine, or a disk labelled as snapshot storage by another one. It is
    /// never offered, so it is never erased.
    Snapshots,
    InUse,
    Free,
}

impl DiskRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Snapshots => "snapshots",
            Self::InUse => "in use",
            Self::Free => "free",
        }
    }
}

/// One disk with its role and the reason in plain words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskView {
    pub disk: BlockDevice,
    pub role: DiskRole,
    pub reason: String,
}

impl DiskView {
    /// The mount points on the disk a folder target may use.
    pub fn folders(&self) -> Vec<String> {
        if self.role != DiskRole::InUse {
            return Vec::new();
        }
        let mut folders: Vec<String> = self
            .disk
            .walk()
            .into_iter()
            .flat_map(|device| device.mountpoints.iter().cloned())
            .filter(|mount| mount != "[SWAP]")
            .collect();
        folders.sort();
        folders.dedup();
        folders
    }

    fn to_json(&self) -> Value {
        let child = |device: &BlockDevice| {
            json!({
                "name": device.name,
                "path": device.path,
                "size": device.size,
                "type": device.kind,
                "fstype": device.fstype,
                "label": device.label,
                "mountpoints": device.mountpoints,
            })
        };
        let disk = &self.disk;
        json!({
            "name": disk.name,
            "path": disk.path,
            "size": disk.size,
            "model": disk.model,
            "serial": disk.serial,
            "transport": disk.transport,
            "removable": disk.removable,
            "fstype": disk.fstype,
            "label": disk.label,
            "mountpoints": disk.mountpoints,
            "children": disk.walk().into_iter().skip(1).map(child).collect::<Vec<_>>(),
            "role": self.role.as_str(),
            "reason": self.reason,
            "offer": match self.role {
                DiskRole::System | DiskRole::Snapshots => Value::Null,
                DiskRole::InUse => json!("folder"),
                DiskRole::Free => json!("whole disk"),
            },
            "folders": self.folders(),
        })
    }
}

fn system_mount(mount: &str) -> bool {
    mount == "/" || mount == "[SWAP]" || mount == "/boot" || mount.starts_with("/boot/")
}

/// Whether `path` lies at or under `mount`.
fn under(path: &str, mount: &str) -> bool {
    mount == "/"
        || path == mount
        || path
            .strip_prefix(mount)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// The disk index and mount point whose file system holds `path`: the longest mount point that `path` lies under.
fn mount_owner(disks: &[BlockDevice], path: &str) -> Option<(usize, String)> {
    let mut best: Option<(usize, String)> = None;
    for (index, disk) in disks.iter().enumerate() {
        for device in disk.walk() {
            for mount in &device.mountpoints {
                if mount == "[SWAP]" || !mount.starts_with('/') || !under(path, mount) {
                    continue;
                }
                if best
                    .as_ref()
                    .is_none_or(|(_, found)| mount.len() > found.len())
                {
                    best = Some((index, mount.clone()));
                }
            }
        }
    }
    best
}

/// A pool source that names a device, resolved through its link when the host has one (such as `/dev/disk/by-id/...`).
fn resolved_device(source: &str) -> String {
    std::fs::canonicalize(source)
        .ok()
        .and_then(|path| path.to_str().map(str::to_string))
        .unwrap_or_else(|| source.to_string())
}

/// Gives every disk its role. A pool is matched by the device it names, by a UUID or label, or, for a folder pool, by the
/// file system its folder is on.
pub fn classify(disks: &[BlockDevice], pools: &[PoolView]) -> Vec<DiskView> {
    let mut pool_disks: Vec<Option<&PoolView>> = vec![None; disks.len()];
    for pool in pools {
        for source in &pool.sources {
            if source.starts_with("/dev/") {
                let device = resolved_device(source);
                for (index, disk) in disks.iter().enumerate() {
                    if disk
                        .walk()
                        .iter()
                        .any(|node| node.path == device || node.path == *source)
                    {
                        pool_disks[index].get_or_insert(pool);
                    }
                }
            } else if source.starts_with('/') {
                if let Some((index, _)) = mount_owner(disks, source) {
                    pool_disks[index].get_or_insert(pool);
                }
            } else {
                for (index, disk) in disks.iter().enumerate() {
                    if disk.walk().iter().any(|node| {
                        node.uuid.as_deref() == Some(source)
                            || node.label.as_deref() == Some(source)
                    }) {
                        pool_disks[index].get_or_insert(pool);
                    }
                }
            }
        }
    }
    disks
        .iter()
        .zip(pool_disks)
        .map(|(disk, pool)| {
            let (role, reason) = role_of(disk, pool);
            DiskView {
                disk: disk.clone(),
                role,
                reason,
            }
        })
        .collect()
}

fn role_of(disk: &BlockDevice, pool: Option<&PoolView>) -> (DiskRole, String) {
    let nodes = disk.walk();
    for node in &nodes {
        for mount in &node.mountpoints {
            if system_mount(mount) {
                let what = if mount == "[SWAP]" {
                    "swap".to_string()
                } else {
                    mount.clone()
                };
                return (
                    DiskRole::System,
                    format!("holds {what} ({}); it is never offered", node.name),
                );
            }
        }
        if node.fstype.as_deref() == Some("swap") {
            return (
                DiskRole::System,
                format!("holds swap ({}); it is never offered", node.name),
            );
        }
        if node
            .fstype
            .as_deref()
            .is_some_and(|fstype| MEMBER_FSTYPES.contains(&fstype))
            || ["lvm", "crypt", "dm", "mpath"].contains(&node.kind.as_str())
            || node.kind.starts_with("raid")
        {
            return (
                DiskRole::System,
                format!(
                    "is part of a RAID, LVM, encrypted or ZFS set ({}); it is never offered",
                    node.name
                ),
            );
        }
    }
    if let Some(pool) = pool.filter(|pool| pool.snapshots) {
        return (
            DiskRole::Snapshots,
            format!(
                "holds the snapshot storage {}; it is never offered or erased",
                pool.name
            ),
        );
    }
    if let Some(pool) = pool {
        return (
            DiskRole::System,
            format!(
                "holds the Incus storage pool {}; it is never offered",
                pool.name
            ),
        );
    }
    if let Some(node) = nodes.iter().find(|node| {
        node.label
            .as_deref()
            .is_some_and(|label| label.starts_with(SNAPSHOT_LABEL_PREFIX))
    }) {
        return (
            DiskRole::Snapshots,
            format!(
                "holds Arca snapshots (file system label {}); it is never offered or erased",
                node.label.as_deref().unwrap_or_default()
            ),
        );
    }
    let mounts: BTreeSet<&str> = nodes
        .iter()
        .flat_map(|node| node.mountpoints.iter().map(String::as_str))
        .collect();
    if !mounts.is_empty() {
        let list: Vec<&str> = mounts.into_iter().collect();
        return (
            DiskRole::InUse,
            format!(
                "mounted at {}; it can hold snapshots in a folder",
                list.join(", ")
            ),
        );
    }
    (
        DiskRole::Free,
        "not mounted and not part of anything; it can be erased and used whole".to_string(),
    )
}

/// The disks and pools of one survey.
#[derive(Debug, Clone)]
pub struct Survey {
    pub disks: Vec<DiskView>,
    pub pools: Vec<PoolView>,
}

/// One snapshot copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotView {
    pub name: String,
    /// `container`, `virtual-machine` or `volume`.
    pub kind: String,
    pub source_project: String,
    pub source_name: String,
    pub source_pool: Option<String>,
    pub pool: String,
    pub size: Option<u64>,
    pub created: String,
    pub copying: bool,
    pub dropped: Vec<String>,
}

impl SnapshotView {
    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "kind": self.kind,
            "project": SNAPSHOT_PROJECT,
            "sourceProject": self.source_project,
            "sourceName": self.source_name,
            "sourcePool": self.source_pool,
            "pool": self.pool,
            "size": self.size,
            "created": self.created,
            "state": if self.copying { "copying" } else { "ready" },
            "droppedDevices": self.dropped,
        })
    }

    fn source(&self) -> String {
        format!("{}/{}", self.source_project, self.source_name)
    }

    fn delete_kind(&self) -> &'static str {
        if self.kind == "volume" {
            "volume"
        } else {
            "instance"
        }
    }
}

/// The node's storage access: `lsblk`, the Incus reads, and on a kernel-mode node the snapshot writer.
#[derive(Clone)]
pub struct Storage {
    devices: Arc<dyn BlockDevices>,
    read: UnixIncus,
    writer: Option<SnapshotWriter>,
}

impl std::fmt::Debug for Storage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Storage")
            .field("writes", &self.writer.is_some())
            .finish_non_exhaustive()
    }
}

/// Why a storage call was refused. The text is shown to the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageRefusal {
    pub code: &'static str,
    pub message: String,
}

impl StorageRefusal {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl From<StorageRefusal> for CallError {
    fn from(refusal: StorageRefusal) -> CallError {
        CallError::Refused {
            code: refusal.code.to_string(),
            message: refusal.message,
        }
    }
}

fn unavailable(what: &str, error: &IncusError) -> StorageRefusal {
    StorageRefusal::new(
        "StorageUnavailable",
        format!("{what} could not be read: {error}"),
    )
}

impl Storage {
    /// Storage over the Incus socket at `socket`. `writes` is true only on a kernel-mode node.
    pub fn new(socket: PathBuf, devices: Arc<dyn BlockDevices>, writes: bool) -> Storage {
        Storage {
            devices,
            read: UnixIncus::new(socket.clone()),
            writer: writes.then(|| SnapshotWriter::new(socket)),
        }
    }

    /// The same storage, waiting at most `wait` for a copy or delete before reporting it as still running.
    pub fn with_wait(mut self, wait: Duration) -> Storage {
        self.writer = self.writer.map(|writer| writer.with_wait(wait));
        self
    }

    fn writer(&self) -> Result<&SnapshotWriter, StorageRefusal> {
        self.writer.as_ref().ok_or_else(|| {
            StorageRefusal::new(
                "StorageDisabled",
                "snapshot storage changes are disabled: this node is watch-only",
            )
        })
    }

    /// Every Incus pool, with its space.
    pub fn pools(&self) -> Result<Vec<PoolView>, StorageRefusal> {
        let rows = self
            .read
            .get("/1.0/storage-pools?recursion=1")
            .map_err(|error| unavailable("the storage pools", &error))?;
        let rows = rows.as_array().ok_or_else(|| {
            StorageRefusal::new("StorageUnavailable", "the pool list is not a list")
        })?;
        let mut pools = Vec::new();
        for row in rows {
            let Some(name) = row.get("name").and_then(Value::as_str) else {
                continue;
            };
            if !plain_name(name) {
                continue;
            }
            let mut sources = Vec::new();
            for key in ["source", "volatile.initial_source"] {
                if let Some(source) = row
                    .pointer(&format!("/config/{key}"))
                    .and_then(Value::as_str)
                    .filter(|source| !source.is_empty())
                    && !sources.iter().any(|found| found == source)
                {
                    sources.push(source.to_string());
                }
            }
            let space = self
                .read
                .get(&format!("/1.0/storage-pools/{name}/resources"))
                .ok();
            let space_of = |key: &str| {
                space
                    .as_ref()
                    .and_then(|space| space.pointer(&format!("/space/{key}")))
                    .and_then(Value::as_u64)
            };
            pools.push(PoolView {
                name: name.to_string(),
                driver: row
                    .get("driver")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                sources,
                used: space_of("used"),
                total: space_of("total"),
                snapshots: arca_incus::snapshot::is_snapshot_pool(row),
            });
        }
        Ok(pools)
    }

    /// The disks with their roles, and the pools.
    pub fn survey(&self) -> Result<Survey, StorageRefusal> {
        let pools = self.pools()?;
        let lsblk = self.devices.read().map_err(|why| {
            StorageRefusal::new(
                "StorageUnavailable",
                format!("the disk list could not be read: {why}"),
            )
        })?;
        let disks =
            parse_lsblk(&lsblk).map_err(|why| StorageRefusal::new("StorageUnavailable", why))?;
        Ok(Survey {
            disks: classify(&disks, &pools),
            pools,
        })
    }

    /// Every snapshot copy: the instances in [`SNAPSHOT_PROJECT`] and the custom volumes it holds on snapshot pools.
    pub fn snapshots(&self) -> Result<Vec<SnapshotView>, StorageRefusal> {
        let project_path = format!("/1.0/projects/{SNAPSHOT_PROJECT}");
        match self.read.get(&project_path) {
            Ok(_) => {}
            Err(IncusError::Status { code: 404, .. }) => return Ok(Vec::new()),
            Err(error) => return Err(unavailable("the snapshot project", &error)),
        }
        let running = arca_incus::read::running_operations(&self.read).unwrap_or_default();
        let busy = |kind: arca_kernel::ownership::IncusKind, parent: Option<&str>, name: &str| {
            running
                .iter()
                .any(|op| arca_incus::operation_names(op, kind, SNAPSHOT_PROJECT, parent, name))
        };
        let mut out = Vec::new();
        let rows = self
            .read
            .get(&format!(
                "/1.0/instances?project={SNAPSHOT_PROJECT}&recursion=1"
            ))
            .map_err(|error| unavailable("the snapshots", &error))?;
        for row in rows.as_array().into_iter().flatten() {
            let Some(name) = row.get("name").and_then(Value::as_str) else {
                continue;
            };
            if !plain_name(name) {
                continue;
            }
            let config = |key: &str| {
                row.pointer(&format!("/config/{}", key.replace('/', "~1")))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            };
            let size = self
                .read
                .get(&format!(
                    "/1.0/instances/{name}/state?project={SNAPSHOT_PROJECT}"
                ))
                .ok()
                .and_then(|state| state.pointer("/disk/root/usage").and_then(Value::as_u64));
            let pool = row
                .pointer("/expanded_devices/root/pool")
                .or_else(|| row.pointer("/devices/root/pool"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            out.push(SnapshotView {
                name: name.to_string(),
                kind: config(SOURCE_KIND_KEY).unwrap_or_else(|| {
                    row.get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("container")
                        .to_string()
                }),
                source_project: config(SOURCE_PROJECT_KEY).unwrap_or_default(),
                source_name: config(SOURCE_NAME_KEY).unwrap_or_default(),
                source_pool: None,
                pool,
                size,
                created: config(CREATED_KEY).unwrap_or_else(|| {
                    row.get("created_at")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string()
                }),
                copying: busy(arca_kernel::ownership::IncusKind::Instance, None, name),
                dropped: config(DROPPED_KEY)
                    .map(|list| {
                        list.split(',')
                            .filter(|item| !item.is_empty())
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default(),
            });
        }
        for pool in self.pools()?.into_iter().filter(|pool| pool.snapshots) {
            let rows = self
                .read
                .get(&format!(
                    "/1.0/storage-pools/{}/volumes/custom?project={SNAPSHOT_PROJECT}&recursion=1",
                    pool.name
                ))
                .map_err(|error| unavailable("the snapshot volumes", &error))?;
            for row in rows.as_array().into_iter().flatten() {
                let Some(name) = row.get("name").and_then(Value::as_str) else {
                    continue;
                };
                if !plain_name(name) {
                    continue;
                }
                let config = |key: &str| {
                    row.get("config")
                        .and_then(|config| config.get(key))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                };
                let size = self
                    .read
                    .get(&format!(
                        "/1.0/storage-pools/{}/volumes/custom/{name}/state?project={SNAPSHOT_PROJECT}",
                        pool.name
                    ))
                    .ok()
                    .and_then(|state| state.pointer("/usage/used").and_then(Value::as_u64));
                out.push(SnapshotView {
                    name: name.to_string(),
                    kind: "volume".to_string(),
                    source_project: config(SOURCE_PROJECT_KEY).unwrap_or_default(),
                    source_name: config(SOURCE_NAME_KEY).unwrap_or_default(),
                    source_pool: config(SOURCE_POOL_KEY),
                    pool: pool.name.clone(),
                    size,
                    created: config(CREATED_KEY).unwrap_or_else(|| {
                        row.get("created_at")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string()
                    }),
                    copying: busy(
                        arca_kernel::ownership::IncusKind::Volume,
                        Some(&pool.name),
                        name,
                    ),
                    dropped: Vec::new(),
                });
            }
        }
        Ok(out)
    }

    fn snapshot_named(&self, name: &str) -> Result<Option<SnapshotView>, StorageRefusal> {
        Ok(self
            .snapshots()?
            .into_iter()
            .find(|snapshot| snapshot.name == name))
    }

    fn require_snapshot_pool(&self, pool: &str) -> Result<(), StorageRefusal> {
        let found = self
            .pools()?
            .into_iter()
            .find(|found| found.name == pool)
            .ok_or_else(|| {
                StorageRefusal::new(
                    "NotSnapshotStorage",
                    format!("there is no storage pool named {pool}; add snapshot storage first"),
                )
            })?;
        if !found.snapshots {
            return Err(StorageRefusal::new(
                "NotSnapshotStorage",
                format!("the pool {pool} was not added as snapshot storage"),
            ));
        }
        Ok(())
    }

    fn source_instance(&self, project: &str, name: &str) -> Result<Value, StorageRefusal> {
        if !plain_name(project) || !plain_name(name) {
            return Err(StorageRefusal::new(
                "SourceUnknown",
                "the project or instance name is not plain",
            ));
        }
        self.read
            .get(&format!("/1.0/instances/{name}?project={project}"))
            .map_err(|error| match error {
                IncusError::Status { code: 404, .. } => StorageRefusal::new(
                    "SourceUnknown",
                    format!("no app named {name} exists in project {project}"),
                ),
                other => unavailable("the app", &other),
            })
    }

    fn source_volume(
        &self,
        project: &str,
        name: &str,
        pool: Option<&str>,
    ) -> Result<(String, Value), StorageRefusal> {
        if !plain_name(project) || !plain_name(name) {
            return Err(StorageRefusal::new(
                "SourceUnknown",
                "the project or volume name is not plain",
            ));
        }
        let candidates: Vec<String> = match pool {
            Some(pool) => vec![pool.to_string()],
            None => self.pools()?.into_iter().map(|pool| pool.name).collect(),
        };
        let mut found = Vec::new();
        for candidate in candidates {
            if !plain_name(&candidate) {
                continue;
            }
            match self.read.get(&format!(
                "/1.0/storage-pools/{candidate}/volumes/custom/{name}?project={project}"
            )) {
                Ok(volume) => found.push((candidate, volume)),
                Err(IncusError::Status { code: 404, .. }) => {}
                Err(error) => return Err(unavailable("the data volume", &error)),
            }
        }
        match found.len() {
            1 => Ok(found.remove(0)),
            0 => Err(StorageRefusal::new(
                "SourceUnknown",
                format!("no data volume named {name} exists in project {project}"),
            )),
            _ => Err(StorageRefusal::new(
                "SourceUnknown",
                format!(
                    "more than one pool holds a data volume named {name} in project {project}; name its pool"
                ),
            )),
        }
    }

    fn project_has_networks(&self, project: &str) -> Result<bool, StorageRefusal> {
        if project == "default" {
            return Ok(false);
        }
        let found = self
            .read
            .get(&format!("/1.0/projects/{project}"))
            .map_err(|error| unavailable("the source project", &error))?;
        Ok(found
            .pointer("/config/features.networks")
            .and_then(Value::as_str)
            == Some("true"))
    }
}

// ---- Proposal arguments: validation, the owner's words, enrichment and the checks a run repeats. ----

fn text<'a>(args: &'a Value, key: &str) -> &'a str {
    args[key].as_str().unwrap_or_default()
}

/// Sizes in decimal GB with one decimal, as disk labels write them.
pub fn plain_size(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1_000_000_000.0)
}

fn plain_transport(transport: &str) -> String {
    match transport {
        "usb" => "USB".to_string(),
        "sata" => "SATA".to_string(),
        "nvme" => "NVMe".to_string(),
        "ata" => "ATA".to_string(),
        "scsi" => "SCSI".to_string(),
        "" => String::new(),
        other => other.to_uppercase(),
    }
}

fn disk_words(args: &Value) -> String {
    let model = text(args, "model");
    let name = text(args, "disk");
    let shown = if model.is_empty() {
        format!("Disk {name}")
    } else {
        model.to_string()
    };
    let size = plain_size(args["size"].as_u64().unwrap_or(0));
    let transport = plain_transport(text(args, "transport"));
    if transport.is_empty() {
        format!("{shown} ({size})")
    } else {
        format!("{shown} ({size}, {transport})")
    }
}

/// What is on a disk now, in plain words: each partition with its file system and label, or that it has none.
pub fn contents_words(disk: &BlockDevice) -> String {
    let describe = |device: &BlockDevice| {
        let mut parts = vec![device.name.clone()];
        match &device.fstype {
            Some(fstype) => parts.push(fstype.clone()),
            None => parts.push("no file system".to_string()),
        }
        if let Some(label) = &device.label {
            parts.push(format!("\"{label}\""));
        }
        parts.push(plain_size(device.size));
        parts.join(" ")
    };
    let inner: Vec<String> = disk.walk().into_iter().skip(1).map(describe).collect();
    if !inner.is_empty() {
        return inner.join("; ");
    }
    if disk.fstype.is_some() {
        return format!("{} on the whole disk", describe(disk));
    }
    "no partitions or file systems".to_string()
}

/// The sentence a whole-disk card always carries.
pub const ERASE_SENTENCE: &str = "Everything on this disk will be erased.";

/// Checks the kernel-resolved fields an action's arguments must carry and normalizes them. Called by
/// [`ActionKind::prepare`] after the verb's schema passed.
pub fn prepare(action: ActionKind, mut args: Value) -> Result<Value, String> {
    let has = |args: &Value, key: &str| args.get(key).is_some_and(|value| !value.is_null());
    let resolved = |args: &Value, keys: &[&str]| -> Result<(), String> {
        for key in keys {
            if args.get(*key).is_none() {
                return Err(format!(
                    "args/{key}: the kernel resolves it when the proposal is made"
                ));
            }
        }
        Ok(())
    };
    match action {
        ActionKind::StorageAdd => match (has(&args, "path"), has(&args, "device")) {
            (true, true) | (false, false) => {
                return Err("args: name exactly one of path and device".to_string());
            }
            (true, false) => {
                let path = text(&args, "path").to_string();
                let clean = clean_folder(&path)?;
                resolved(
                    &args,
                    &[
                        "disk",
                        "model",
                        "serial",
                        "transport",
                        "size",
                        "contents",
                        "mount",
                    ],
                )?;
                if let Some(object) = args.as_object_mut() {
                    object.insert("path".to_string(), json!(clean));
                }
            }
            (false, true) => {
                let device = text(&args, "device");
                let name = device.strip_prefix("/dev/").unwrap_or(device).to_string();
                resolved(
                    &args,
                    &["disk", "model", "serial", "transport", "size", "contents"],
                )?;
                if args.get("mount").is_some() {
                    return Err("args/mount: a whole-disk target has no mount".to_string());
                }
                if text(&args, "disk") != name {
                    return Err("args/disk: it must name the device".to_string());
                }
                let pool = whole_disk_pool_name(text(&args, "name"))?;
                if let Some(object) = args.as_object_mut() {
                    object.insert("device".to_string(), json!(format!("/dev/{name}")));
                    object.insert("name".to_string(), json!(pool));
                }
            }
        },
        ActionKind::SnapshotCreate => {
            let (source, kinds): (&str, &[&str]) =
                match (has(&args, "instance"), has(&args, "volume")) {
                    (true, false) => ("instance", &["container", "virtual-machine"]),
                    (false, true) => ("volume", &["volume"]),
                    _ => return Err("args: name exactly one of instance and volume".to_string()),
                };
            resolved(&args, &["name", "kind"])?;
            if !kinds.contains(&text(&args, "kind")) {
                return Err("args/kind: it does not match the source".to_string());
            }
            if source == "volume" {
                resolved(&args, &["sourcePool"])?;
            } else if args.get("sourcePool").is_some() {
                return Err("args/sourcePool: only a volume has a source pool".to_string());
            }
            if source == "instance" {
                resolved(&args, &["uuid"])?;
            }
            if text(&args, "project") == SNAPSHOT_PROJECT {
                return Err("args/project: a snapshot is not snapshotted again".to_string());
            }
            let expected_prefix = name_parts(text(&args, "project"), text(&args, source));
            let name = text(&args, "name");
            if !snapshot_name_shape(name) || !name.starts_with(&expected_prefix) {
                return Err("args/name: it is not this source's snapshot name".to_string());
            }
        }
        ActionKind::SnapshotDelete => {
            resolved(&args, &["kind", "pool", "source"])?;
        }
        _ => {}
    }
    Ok(args)
}

/// The pool name of a whole-disk snapshot storage: `arca-snapshots-<name>`. Incus labels the btrfs file system it makes
/// with the pool's name, so the label says the disk holds snapshots on any machine it is plugged into.
fn whole_disk_pool_name(name: &str) -> Result<String, String> {
    let pool = if name.starts_with(SNAPSHOT_LABEL_PREFIX) {
        name.to_string()
    } else {
        format!("{SNAPSHOT_LABEL_PREFIX}{name}")
    };
    if pool.len() > 32 || pool.len() == SNAPSHOT_LABEL_PREFIX.len() {
        return Err(format!(
            "args/name: a whole disk's storage is named {SNAPSHOT_LABEL_PREFIX}<name>, at most 32 characters"
        ));
    }
    Ok(pool)
}

/// A folder as the host resolves it: every link in the part of the path that exists is followed, and the rest is kept as
/// written. A folder that does not exist yet stays on the disk its nearest existing parent is on.
pub fn resolved_folder(path: &str) -> String {
    let mut existing = std::path::PathBuf::from(path);
    let mut rest = Vec::new();
    loop {
        if let Ok(real) = std::fs::canonicalize(&existing) {
            let mut real = real;
            for part in rest.iter().rev() {
                real.push(part);
            }
            return real
                .to_str()
                .map(str::to_string)
                .unwrap_or_else(|| path.to_string());
        }
        match (existing.file_name(), existing.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name.to_os_string());
                existing = parent.to_path_buf();
            }
            _ => return path.to_string(),
        }
    }
}

/// Whether this process may make entries in the folder `dir` (write and search), judged from its owner, group and mode
/// bits the way the host does (an ACL or a read-only mount is not seen here; the folder write itself reports those).
fn may_write_in(dir: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = std::fs::metadata(dir) else {
        return false;
    };
    if !meta.is_dir() {
        return false;
    }
    // `/proc/self` belongs to the process's effective user; its status lists the effective group and the others.
    let Ok(uid) = std::fs::metadata("/proc/self").map(|me| me.uid()) else {
        return false;
    };
    if uid == 0 {
        return true;
    }
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let mut groups = Vec::new();
    for line in status.lines() {
        if let Some(gids) = line.strip_prefix("Gid:") {
            groups.extend(
                gids.split_whitespace()
                    .nth(1)
                    .and_then(|g| g.parse::<u32>().ok()),
            );
        } else if let Some(gids) = line.strip_prefix("Groups:") {
            groups.extend(
                gids.split_whitespace()
                    .filter_map(|g| g.parse::<u32>().ok()),
            );
        }
    }
    let bits = if meta.uid() == uid {
        meta.mode() >> 6
    } else if groups.contains(&meta.gid()) {
        meta.mode() >> 3
    } else {
        meta.mode()
    };
    bits & 0o3 == 0o3
}

/// The plain-folder test for `<folder>/arca-pool` when it is already there: a real folder (not a link) with nothing in it,
/// which is what Incus needs to make a pool in it.
fn pool_folder_usable(pool_dir: &std::path::Path) -> Result<(), StorageRefusal> {
    let shown = pool_dir.display();
    let meta = std::fs::symlink_metadata(pool_dir).map_err(|error| {
        StorageRefusal::new(
            "PoolFolderInUse",
            format!("{shown} cannot be read: {error}"),
        )
    })?;
    if !meta.file_type().is_dir() {
        return Err(StorageRefusal::new(
            "PoolFolderInUse",
            format!("{shown} is already there and is not a plain folder; choose another folder"),
        ));
    }
    let empty = std::fs::read_dir(pool_dir)
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(false);
    if !empty {
        return Err(StorageRefusal::new(
            "PoolFolderInUse",
            format!("{shown} is already there and has files in it; choose another folder"),
        ));
    }
    Ok(())
}

/// Checks that Arca can make the folder Incus keeps a folder pool in, `<folder>/arca-pool`, without writing anything:
/// either it is already there as an empty plain folder, or the nearest part of its path that exists is a folder this
/// process may write in.
fn check_pool_folder(folder: &str) -> Result<(), StorageRefusal> {
    let pool_dir = std::path::PathBuf::from(folder_pool_path(folder));
    if std::fs::symlink_metadata(&pool_dir).is_ok() {
        return pool_folder_usable(&pool_dir);
    }
    let mut existing = std::path::Path::new(folder);
    while std::fs::symlink_metadata(existing).is_err() {
        match existing.parent() {
            Some(parent) => existing = parent,
            None => break,
        }
    }
    if may_write_in(existing) {
        Ok(())
    } else {
        Err(StorageRefusal::new(
            "FolderNotWritable",
            format!(
                "Arca may not create folders in {}, so it cannot keep the snapshot storage there; choose a folder Arca can write in",
                existing.display()
            ),
        ))
    }
}

/// Makes `<folder>/arca-pool` for a folder pool, as Incus needs it to exist and be empty: each missing part of the folder
/// one at a time (0755), then `arca-pool` itself (0700). No step follows a link: a part that is a link or not a folder,
/// or an `arca-pool` that is not an empty plain folder, is refused, and the result is checked to be where it was asked
/// for. Returns whether `arca-pool` was made here, so a pool create that then fails can remove it again.
pub fn make_pool_folder(folder: &str) -> Result<bool, StorageRefusal> {
    use std::os::unix::fs::DirBuilderExt;
    check_pool_folder(folder)?;
    let changed = || {
        StorageRefusal::new(
            "DiskChanged",
            "the folder now leads somewhere else through a link; propose it again",
        )
    };
    let mut path = std::path::PathBuf::new();
    for part in std::path::Path::new(folder).components() {
        path.push(part);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_dir() => {}
            Ok(_) => return Err(changed()),
            Err(_) => match std::fs::DirBuilder::new().mode(0o755).create(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if !std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_dir()) {
                        return Err(changed());
                    }
                }
                Err(error) => {
                    return Err(StorageRefusal::new(
                        "FolderNotWritable",
                        format!("Arca could not create {}: {error}", path.display()),
                    ));
                }
            },
        }
    }
    let pool_dir = std::path::PathBuf::from(folder_pool_path(folder));
    let made = match std::fs::DirBuilder::new().mode(0o700).create(&pool_dir) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            pool_folder_usable(&pool_dir)?;
            false
        }
        Err(error) => {
            return Err(StorageRefusal::new(
                "FolderNotWritable",
                format!("Arca could not create {}: {error}", pool_dir.display()),
            ));
        }
    };
    // A part swapped for a link while the folders were made shows here: the folder is not where it was asked for.
    if std::fs::canonicalize(&pool_dir).ok().as_deref() != Some(pool_dir.as_path()) {
        if made && let Ok(real) = std::fs::canonicalize(&pool_dir) {
            let _ = std::fs::remove_dir(real);
        }
        return Err(changed());
    }
    Ok(made)
}

/// A folder target's path, checked: absolute, without `.` or `..` parts, doubled or trailing slashes, or control
/// characters.
fn clean_folder(path: &str) -> Result<String, String> {
    let trimmed = if path.len() > 1 {
        path.trim_end_matches('/')
    } else {
        path
    };
    let plain = trimmed.starts_with('/')
        && !trimmed.contains("//")
        && trimmed
            .split('/')
            .skip(1)
            .all(|part| !part.is_empty() && part != "." && part != "..")
        && !trimmed.chars().any(char::is_control);
    if plain {
        Ok(trimmed.to_string())
    } else {
        Err("args/path: expected an absolute folder path without . or .. parts".to_string())
    }
}

/// The owner's rows for a storage or snapshot proposal. A whole-disk card always carries [`ERASE_SENTENCE`].
pub fn details(action: ActionKind, args: &Value) -> Vec<ActionDetail> {
    let row = |label: &str, value: String| ActionDetail {
        label: label.to_string(),
        value,
    };
    match action {
        ActionKind::StorageAdd if args.get("device").is_some_and(|device| !device.is_null()) => {
            vec![
                row("Disk", disk_words(args)),
                row("Device", text(args, "device").to_string()),
                row("On it now", text(args, "contents").to_string()),
                row("Storage name", text(args, "name").to_string()),
                row(
                    "Effect",
                    format!(
                        "{ERASE_SENTENCE} Incus wipes and formats the whole disk as snapshot storage {} (btrfs, compressed); the disk is labelled with that name. Apps and data are copied onto it only when you approve each snapshot.",
                        text(args, "name")
                    ),
                ),
            ]
        }
        ActionKind::StorageAdd => vec![
            row("Disk", disk_words(args)),
            row("On it now", text(args, "contents").to_string()),
            row("Mounted at", text(args, "mount").to_string()),
            row("Folder", folder_pool_path(text(args, "path"))),
            row("Storage name", text(args, "name").to_string()),
            row(
                "Effect",
                format!(
                    "Snapshots are kept in the folder {} on this disk. Nothing else on the disk changes.",
                    folder_pool_path(text(args, "path"))
                ),
            ),
        ],
        ActionKind::SnapshotCreate => {
            let project = text(args, "project");
            let pool = text(args, "pool");
            let name = text(args, "name");
            if text(args, "kind") == "volume" {
                let volume = text(args, "volume");
                vec![
                    row(
                        "Source",
                        format!(
                            "Data volume {volume} on pool {} in project {project}",
                            text(args, "sourcePool")
                        ),
                    ),
                    row("Copy to", format!("Snapshot storage {pool}")),
                    row("Snapshot name", name.to_string()),
                    row(
                        "Effect",
                        format!(
                            "Copies the data volume {volume} onto {pool}. The original volume is only read, never changed."
                        ),
                    ),
                ]
            } else {
                let instance = text(args, "instance");
                let what = if text(args, "kind") == "virtual-machine" {
                    "virtual machine"
                } else {
                    "container"
                };
                vec![
                    row(
                        "Source",
                        format!("App {instance} ({what}) in project {project}"),
                    ),
                    row("Copy to", format!("Snapshot storage {pool}")),
                    row("Snapshot name", name.to_string()),
                    row(
                        "Effect",
                        format!(
                            "Copies {instance} onto {pool}. The copy never starts on its own, and {instance} is only read: it is not stopped or changed."
                        ),
                    ),
                ]
            }
        }
        ActionKind::SnapshotDelete => {
            let what = if text(args, "kind") == "volume" {
                "data volume"
            } else {
                "app"
            };
            vec![
                row("Snapshot", text(args, "name").to_string()),
                row("Copied from", format!("{what} {}", text(args, "source"))),
                row("Storage", text(args, "pool").to_string()),
                row(
                    "Effect",
                    format!(
                        "Deletes this snapshot copy from {}. The {what} it was copied from is not touched.",
                        text(args, "pool")
                    ),
                ),
            ]
        }
        _ => Vec::new(),
    }
}

/// The one-sentence summary of a storage or snapshot proposal.
pub fn summary(action: ActionKind, args: &Value) -> String {
    match action {
        ActionKind::StorageAdd if args.get("device").is_some_and(|device| !device.is_null()) => {
            format!(
                "Erase the disk {} ({}) and add it as snapshot storage {}",
                if text(args, "model").is_empty() {
                    text(args, "disk")
                } else {
                    text(args, "model")
                },
                text(args, "device"),
                text(args, "name")
            )
        }
        ActionKind::StorageAdd => format!(
            "Add the folder {} as snapshot storage {}",
            folder_pool_path(text(args, "path")),
            text(args, "name")
        ),
        ActionKind::SnapshotCreate if text(args, "kind") == "volume" => format!(
            "Snapshot the data volume {} from project {} onto {}",
            text(args, "volume"),
            text(args, "project"),
            text(args, "pool")
        ),
        ActionKind::SnapshotCreate => format!(
            "Snapshot the app {} from project {} onto {}",
            text(args, "instance"),
            text(args, "project"),
            text(args, "pool")
        ),
        ActionKind::SnapshotDelete => format!(
            "Delete the snapshot {} from {}",
            text(args, "name"),
            text(args, "pool")
        ),
        _ => String::new(),
    }
}

/// A name part: letters, digits and `-`; anything else becomes `-`.
fn name_part(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// The `<project>--<name>--` prefix of a snapshot name, each part shortened so the whole name fits Incus's 63 characters.
fn name_parts(project: &str, name: &str) -> String {
    let budget = MAX_SNAPSHOT_NAME - "----".len() - "yyyymmddThhmm".len();
    let project = name_part(project);
    let name = name_part(name);
    let (project, name) = if project.len() + name.len() > budget {
        let project_budget = project
            .len()
            .min(budget.saturating_sub(name.len()).max(budget / 2));
        let name_budget = budget - project_budget;
        (
            project[..project_budget].to_string(),
            name[..name.len().min(name_budget)].to_string(),
        )
    } else {
        (project, name)
    };
    format!(
        "{}--{}--",
        project.trim_end_matches('-'),
        name.trim_end_matches('-')
    )
}

/// The snapshot name `<project>--<name>--<UTC yyyymmddThhmm>`. A name Incus would refuse (one that starts with a digit) is
/// refused here.
pub fn snapshot_name(project: &str, name: &str, now_ms: u64) -> Result<String, String> {
    let iso = crate::handlers::iso_utc(now_ms);
    let stamp = format!(
        "{}{}{}T{}{}",
        &iso[0..4],
        &iso[5..7],
        &iso[8..10],
        &iso[11..13],
        &iso[14..16]
    );
    let full = format!("{}{stamp}", name_parts(project, name));
    if !full.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return Err(
            "the snapshot name would start with a digit, which Incus does not allow".to_string(),
        );
    }
    Ok(full)
}

fn snapshot_name_shape(name: &str) -> bool {
    let Some((head, stamp)) = name.rsplit_once("--") else {
        return false;
    };
    name.len() <= MAX_SNAPSHOT_NAME
        && head.contains("--")
        && name.starts_with(|c: char| c.is_ascii_alphabetic())
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && stamp.len() == 13
        && stamp.bytes().enumerate().all(|(index, b)| {
            if index == 8 {
                b == b'T'
            } else {
                b.is_ascii_digit()
            }
        })
}

/// Adds the facts the kernel reads to a proposal's arguments, before validation, so the owner's digest binds them. The
/// caller's own copies of those fields are replaced.
pub fn enrich(
    storage: &Storage,
    action: ActionKind,
    args: &Value,
    now_ms: u64,
) -> Result<Value, CallError> {
    storage.writer()?;
    let mut object = args
        .as_object()
        .cloned()
        .ok_or_else(|| CallError::Invalid("args: expected an object".to_string()))?;
    match action {
        ActionKind::StorageAdd => {
            for key in [
                "disk",
                "model",
                "serial",
                "transport",
                "size",
                "contents",
                "mount",
            ] {
                object.remove(key);
            }
            let survey = storage.survey()?;
            let (view, mount) = if let Some(device) = object.get("device").and_then(Value::as_str) {
                (find_whole_disk(&survey, device)?, None)
            } else if let Some(path) = object.get("path").and_then(Value::as_str) {
                let path = clean_folder(path).map_err(CallError::Invalid)?;
                // The disk is decided by where the folder really is, after its links are followed.
                let path = resolved_folder(&path);
                let (view, mount) = find_folder_disk(&survey, &path)?;
                object.insert("path".to_string(), json!(path));
                (view, Some(mount))
            } else {
                return Err(CallError::Invalid(
                    "args: name exactly one of path and device".to_string(),
                ));
            };
            for (key, value) in disk_facts(view) {
                object.insert(key.to_string(), value);
            }
            if let Some(mount) = mount {
                object.insert("mount".to_string(), json!(mount));
            }
        }
        ActionKind::SnapshotCreate => {
            for key in ["name", "kind", "uuid"] {
                object.remove(key);
            }
            let project = object
                .get("project")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if let Some(instance) = object.get("instance").and_then(Value::as_str) {
                let instance = instance.to_string();
                object.remove("sourcePool");
                let row = storage.source_instance(&project, &instance)?;
                object.insert(
                    "kind".to_string(),
                    json!(
                        row.get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("container")
                    ),
                );
                object.insert(
                    "uuid".to_string(),
                    json!(row.pointer("/config/volatile.uuid").and_then(Value::as_str)),
                );
                let name = snapshot_name(&project, &instance, now_ms)
                    .map_err(|why| StorageRefusal::new("SnapshotName", why))?;
                object.insert("name".to_string(), json!(name));
            } else if let Some(volume) = object.get("volume").and_then(Value::as_str) {
                let volume = volume.to_string();
                let pool = object
                    .get("sourcePool")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let (pool, _) = storage.source_volume(&project, &volume, pool.as_deref())?;
                object.insert("sourcePool".to_string(), json!(pool));
                object.insert("kind".to_string(), json!("volume"));
                let name = snapshot_name(&project, &volume, now_ms)
                    .map_err(|why| StorageRefusal::new("SnapshotName", why))?;
                object.insert("name".to_string(), json!(name));
            }
        }
        ActionKind::SnapshotDelete => {
            for key in ["kind", "pool", "source"] {
                object.remove(key);
            }
            let name = object
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let snapshot = storage.snapshot_named(&name)?.ok_or_else(|| {
                StorageRefusal::new(
                    "SnapshotUnknown",
                    format!("there is no snapshot named {name}"),
                )
            })?;
            object.insert("kind".to_string(), json!(snapshot.delete_kind()));
            object.insert("pool".to_string(), json!(snapshot.pool));
            object.insert("source".to_string(), json!(snapshot.source()));
        }
        _ => {}
    }
    Ok(Value::Object(object))
}

fn disk_facts(view: &DiskView) -> Vec<(&'static str, Value)> {
    let disk = &view.disk;
    vec![
        ("disk", json!(disk.name)),
        ("model", json!(disk.model)),
        ("serial", json!(disk.serial)),
        ("transport", json!(disk.transport)),
        ("size", json!(disk.size)),
        ("contents", json!(contents_words(disk))),
    ]
}

fn find_whole_disk<'a>(survey: &'a Survey, device: &str) -> Result<&'a DiskView, StorageRefusal> {
    let name = device.strip_prefix("/dev/").unwrap_or(device);
    if let Some(view) = survey.disks.iter().find(|view| view.disk.name == name) {
        return match view.role {
            DiskRole::Free => Ok(view),
            DiskRole::Snapshots => Err(StorageRefusal::new(
                "SnapshotDisk",
                format!("{name} already {}", view.reason),
            )),
            DiskRole::System => Err(StorageRefusal::new(
                "SystemDisk",
                format!("{name} is a system disk: it {}", view.reason),
            )),
            DiskRole::InUse => Err(StorageRefusal::new(
                "DiskMounted",
                format!(
                    "{name} is {}; a mounted disk can only hold snapshots in a folder",
                    view.reason
                ),
            )),
        };
    }
    if let Some(view) = survey.disks.iter().find(|view| {
        view.disk
            .walk()
            .iter()
            .skip(1)
            .any(|node| node.name == name)
    }) {
        return Err(StorageRefusal::new(
            "NotWholeDisk",
            format!(
                "{name} is a part of the disk {}; pick the whole disk",
                view.disk.name
            ),
        ));
    }
    Err(StorageRefusal::new(
        "DiskNotListed",
        format!("{name} is not a disk in storage_list"),
    ))
}

fn find_folder_disk<'a>(
    survey: &'a Survey,
    path: &str,
) -> Result<(&'a DiskView, String), StorageRefusal> {
    let disks: Vec<BlockDevice> = survey.disks.iter().map(|view| view.disk.clone()).collect();
    let (index, mount) = mount_owner(&disks, path).ok_or_else(|| {
        StorageRefusal::new(
            "DiskNotListed",
            format!("{path} is not on any disk in storage_list"),
        )
    })?;
    let view = &survey.disks[index];
    match view.role {
        DiskRole::InUse => {}
        DiskRole::Snapshots => {
            return Err(StorageRefusal::new(
                "SnapshotDisk",
                format!(
                    "{path} is on the disk {}, which {}",
                    view.disk.name, view.reason
                ),
            ));
        }
        DiskRole::System => {
            return Err(StorageRefusal::new(
                "SystemDisk",
                format!(
                    "{path} is on the system disk {}, which {}",
                    view.disk.name, view.reason
                ),
            ));
        }
        DiskRole::Free => {
            return Err(StorageRefusal::new(
                "DiskNotListed",
                format!("{path} is not on a mounted disk"),
            ));
        }
    }
    let pool_dir = folder_pool_path(path);
    for pool in &survey.pools {
        if pool
            .sources
            .iter()
            .any(|source| under(source, &pool_dir) || under(&pool_dir, source))
        {
            return Err(StorageRefusal::new(
                "PoolExists",
                format!(
                    "the folder is already used by the storage pool {}",
                    pool.name
                ),
            ));
        }
    }
    Ok((view, mount))
}

/// What a checked action will write.
enum Target {
    Pool(String, PoolSource),
    Instance { pool: String, body: Value },
    Volume { pool: String, body: Value },
    DeleteInstance(String),
    DeleteVolume { pool: String, name: String },
}

/// Checks a prepared storage or snapshot action against the host as it is now, and returns what it will write. A proposal
/// runs it once when it is made, and an approved action runs it again before it writes, so any change in between refuses.
fn check(
    storage: &Storage,
    action: ActionKind,
    args: &Value,
    now_ms: u64,
) -> Result<Target, CallError> {
    storage.writer()?;
    match action {
        ActionKind::StorageAdd => {
            let survey = storage.survey()?;
            let name = text(args, "name");
            if survey.pools.iter().any(|pool| pool.name == name) {
                return Err(StorageRefusal::new(
                    "PoolExists",
                    format!("a storage pool named {name} already exists"),
                )
                .into());
            }
            let (view, mount, source) = if args.get("device").is_some_and(|d| !d.is_null()) {
                let view = find_whole_disk(&survey, text(args, "device"))?;
                (view, None, PoolSource::Device(view.disk.path.clone()))
            } else {
                let path = text(args, "path");
                if resolved_folder(path) != path {
                    return Err(StorageRefusal::new(
                        "DiskChanged",
                        "the folder now leads somewhere else through a link; propose it again",
                    )
                    .into());
                }
                let (view, mount) = find_folder_disk(&survey, path)?;
                (view, Some(mount), PoolSource::Folder(path.to_string()))
            };
            let changed = disk_facts(view)
                .into_iter()
                .any(|(key, value)| args.get(key) != Some(&value))
                || mount
                    .as_deref()
                    .is_some_and(|mount| text(args, "mount") != mount);
            if changed {
                return Err(StorageRefusal::new(
                    "DiskChanged",
                    "the disk changed after the proposal; propose it again",
                )
                .into());
            }
            if let PoolSource::Folder(folder) = &source {
                check_pool_folder(folder)?;
            }
            Ok(Target::Pool(name.to_string(), source))
        }
        ActionKind::SnapshotCreate => {
            let pool = text(args, "pool");
            storage.require_snapshot_pool(pool)?;
            let project = text(args, "project");
            let name = text(args, "name");
            if storage.snapshot_named(name)?.is_some() {
                return Err(StorageRefusal::new(
                    "SnapshotExists",
                    format!("a snapshot named {name} already exists"),
                )
                .into());
            }
            let created = crate::handlers::iso_utc(now_ms);
            if text(args, "kind") == "volume" {
                let (source_pool, volume) = storage.source_volume(
                    project,
                    text(args, "volume"),
                    Some(text(args, "sourcePool")),
                )?;
                let body = volume_copy_body(&volume, project, &source_pool, name, &created)
                    .map_err(|why| StorageRefusal::new("SourceUnknown", why))?;
                Ok(Target::Volume {
                    pool: pool.to_string(),
                    body,
                })
            } else {
                let row = storage.source_instance(project, text(args, "instance"))?;
                let kind = row
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("container");
                let uuid = row.pointer("/config/volatile.uuid").and_then(Value::as_str);
                if kind != text(args, "kind") || uuid != args["uuid"].as_str() {
                    return Err(StorageRefusal::new(
                        "SourceChanged",
                        "the app changed type or was recreated after the proposal",
                    )
                    .into());
                }
                let networks = storage.project_has_networks(project)?;
                let (body, _) = instance_copy_body(&row, project, networks, name, pool, &created)
                    .map_err(|why| StorageRefusal::new("SourceUnknown", why))?;
                Ok(Target::Instance {
                    pool: pool.to_string(),
                    body,
                })
            }
        }
        ActionKind::SnapshotDelete => {
            let name = text(args, "name");
            let snapshot = storage.snapshot_named(name)?.ok_or_else(|| {
                StorageRefusal::new(
                    "SnapshotUnknown",
                    format!("there is no snapshot named {name}"),
                )
            })?;
            if snapshot.delete_kind() != text(args, "kind")
                || snapshot.pool != text(args, "pool")
                || snapshot.source() != text(args, "source")
            {
                return Err(StorageRefusal::new(
                    "SnapshotChanged",
                    "the snapshot changed after the proposal; propose it again",
                )
                .into());
            }
            if snapshot.kind == "volume" {
                Ok(Target::DeleteVolume {
                    pool: snapshot.pool,
                    name: snapshot.name,
                })
            } else {
                Ok(Target::DeleteInstance(snapshot.name))
            }
        }
        _ => Err(CallError::Invalid("not a storage action".to_string())),
    }
}

/// The proposal-time check `action_propose` runs after the arguments are prepared.
pub fn check_proposal(
    storage: &Storage,
    action: ActionKind,
    args: &Value,
    now_ms: u64,
) -> Result<(), CallError> {
    check(storage, action, args, now_ms).map(|_| ())
}

fn write_failed(error: IncusError) -> CallError {
    CallError::Refused {
        code: "StorageWriteFailed".to_string(),
        message: error.to_string(),
    }
}

/// `storage_add`, `snapshot_create` and `snapshot_delete`: the gated verbs an approved action proposal runs. The random
/// one-shot grant is passed in `approval` and consumed before any work, so a direct call with no grant is refused. Every
/// fact the owner approved is checked again here.
pub struct StorageActionHandler {
    pub action: ActionKind,
    pub kernel: Arc<Mutex<Kernel>>,
    pub storage: Option<Arc<Storage>>,
    pub clock: crate::handlers::Clock,
}

impl ToolHandler for StorageActionHandler {
    fn call(&self, args: Value, _caller: &Caller) -> Result<Value, CallError> {
        let Some(storage) = &self.storage else {
            return Err(StorageRefusal::new(
                "StorageDisabled",
                "snapshot storage changes are disabled: this node is watch-only",
            )
            .into());
        };
        let mut bare = args;
        let grant = bare
            .as_object_mut()
            .and_then(|object| object.remove("approval"))
            .and_then(|value| value.as_str().map(str::to_string))
            .ok_or_else(|| CallError::Invalid("approval is required".to_string()))?;
        let args = self
            .action
            .prepare(&bare, None)
            .map_err(CallError::Invalid)?;
        {
            let mut kernel = self
                .kernel
                .lock()
                .map_err(|_| CallError::Internal("the kernel lock is poisoned".to_string()))?;
            if !kernel.consume_action_grant(self.action.as_str(), &args, &grant) {
                return Err(CallError::Refused {
                    code: "NotApproved".to_string(),
                    message: "this action runs only from an approved proposal".to_string(),
                });
            }
        }
        let now = (self.clock)();
        let target = check(storage, self.action, &args, now)?;
        let writer = storage.writer()?;
        let state = |progress: Progress, done: &str, running: &str| match progress {
            Progress::Done => done.to_string(),
            Progress::Running(_) => running.to_string(),
        };
        match target {
            Target::Pool(name, source) => {
                // Incus makes a folder pool only in a folder that is already there and empty, so Arca makes it first.
                let made = match &source {
                    PoolSource::Folder(folder) => make_pool_folder(folder)?,
                    PoolSource::Device(_) => false,
                };
                if let Err(error) = writer.create_pool(&name, &source) {
                    if let (true, PoolSource::Folder(folder)) = (made, &source) {
                        // Only the empty folder made just now goes; anything Incus left in it keeps it.
                        let _ = std::fs::remove_dir(folder_pool_path(folder));
                    }
                    return Err(write_failed(error));
                }
                let (driver, path) = match &source {
                    PoolSource::Folder(folder) => ("dir", folder_pool_path(folder)),
                    PoolSource::Device(device) => ("btrfs", device.clone()),
                };
                Ok(json!({ "pool": name, "driver": driver, "source": path }))
            }
            Target::Instance { pool, body } => {
                writer.ensure_project().map_err(write_failed)?;
                let progress = writer.copy_instance(&pool, &body).map_err(write_failed)?;
                Ok(json!({
                    "snapshot": body["name"], "pool": pool, "kind": text(&args, "kind"),
                    "state": state(progress, "copied", "copying"),
                }))
            }
            Target::Volume { pool, body } => {
                writer.ensure_project().map_err(write_failed)?;
                let progress = writer.copy_volume(&pool, &body).map_err(write_failed)?;
                Ok(json!({
                    "snapshot": body["name"], "pool": pool, "kind": "volume",
                    "state": state(progress, "copied", "copying"),
                }))
            }
            Target::DeleteInstance(name) => {
                let progress = writer.delete_instance(&name).map_err(write_failed)?;
                Ok(json!({
                    "snapshot": name, "pool": text(&args, "pool"), "kind": "instance",
                    "state": state(progress, "deleted", "deleting"),
                }))
            }
            Target::DeleteVolume { pool, name } => {
                let progress = writer.delete_volume(&pool, &name).map_err(write_failed)?;
                Ok(json!({
                    "snapshot": name, "pool": pool, "kind": "volume",
                    "state": state(progress, "deleted", "deleting"),
                }))
            }
        }
    }
}

/// `storage_list`: the disks with their roles and the pools.
pub struct StorageListHandler {
    pub storage: Arc<Storage>,
}

impl ToolHandler for StorageListHandler {
    fn call(&self, _args: Value, _caller: &Caller) -> Result<Value, CallError> {
        let survey = self.storage.survey()?;
        Ok(json!({
            "disks": survey.disks.iter().map(DiskView::to_json).collect::<Vec<_>>(),
            "pools": survey.pools.iter().map(PoolView::to_json).collect::<Vec<_>>(),
        }))
    }
}

/// `snapshot_list`: every snapshot copy.
pub struct SnapshotListHandler {
    pub storage: Arc<Storage>,
}

impl ToolHandler for SnapshotListHandler {
    fn call(&self, _args: Value, _caller: &Caller) -> Result<Value, CallError> {
        let snapshots = self.storage.snapshots()?;
        Ok(json!({
            "snapshots": snapshots.iter().map(SnapshotView::to_json).collect::<Vec<_>>(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk(name: &str, children: Vec<Value>) -> Value {
        json!({
            "name": name, "path": format!("/dev/{name}"), "size": 1_000_204_886_016u64,
            "model": "Acme Portable 1T", "serial": format!("FAKE-{name}"), "tran": "usb", "rm": true,
            "type": "disk", "fstype": null, "label": null, "uuid": null, "mountpoints": [null],
            "children": children,
        })
    }

    fn part(name: &str, fstype: Option<&str>, label: Option<&str>, mounts: &[&str]) -> Value {
        json!({
            "name": name, "path": format!("/dev/{name}"), "size": 500_000_000_000u64,
            "type": "part", "fstype": fstype, "label": label, "uuid": format!("uuid-{name}"),
            "mountpoints": if mounts.is_empty() { json!([null]) } else { json!(mounts) },
        })
    }

    #[test]
    fn snapshot_names_follow_the_shape_and_fit_incus() {
        // 2026-11-09T14:05:33Z
        let now = 1_794_233_133_000;
        assert_eq!(
            snapshot_name("shop", "web", now).expect("a name"),
            "shop--web--20261109T1405"
        );
        let long = snapshot_name(&"p".repeat(60), &"n".repeat(60), now).expect("a long name");
        assert!(long.len() <= MAX_SNAPSHOT_NAME, "{long}");
        assert!(snapshot_name_shape(&long));
        assert!(long.ends_with("--20261109T1405"));
        assert_eq!(
            snapshot_name("my_proj", "db.main", now).expect("a name"),
            "my-proj--db-main--20261109T1405"
        );
        assert!(snapshot_name("1st", "web", now).is_err());
    }

    #[test]
    fn contents_name_partitions_and_labels() {
        let disks = parse_lsblk(&json!({ "blockdevices": [
            disk("sdb", vec![part("sdb1", Some("ext4"), Some("PHOTOS"), &[]), part("sdb2", None, None, &[])])
        ]}))
        .expect("disks");
        assert_eq!(
            contents_words(&disks[0]),
            "sdb1 ext4 \"PHOTOS\" 500.0 GB; sdb2 no file system 500.0 GB"
        );
    }
}
