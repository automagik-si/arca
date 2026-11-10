//! Snapshot storage and snapshots: the owner plugs a disk into any Arca machine, picks it as snapshot storage, and copies
//! every app and data volume onto it, so a reinstalled machine can bring its apps back from that disk alone.
//!
//! `storage_list` reads the host's disks (`lsblk`) and the Incus pools, and gives each disk one plain role:
//!
//! - **system**: it holds `/`, `/boot` or swap, it is a RAID, LVM, LUKS or ZFS member (mounted or not), something holds it
//!   (`/sys/class/block/*/holders`), it is a member of a multi-device file system that holds `/` or is mounted, it is given
//!   to an app, or it holds an Incus pool. It is never offered.
//! - **snapshots**: it holds Arca snapshots. It is never offered, so it is never erased.
//! - **in use**: it is mounted somewhere else. It is offered only as a folder target: a `dir` pool in a folder on it.
//! - **free**: nothing on it is mounted or a member of anything. It is offered as a whole-disk target, which Incus erases and
//!   formats as btrfs.
//!
//! `snapshot_create` copies an app, a data volume, or a host folder such as `/home` (a folder snapshot, see
//! [`arca_incus::folder`]: Incus copies it through a short-lived read-only helper container; this process never reads or
//! writes the folder's files beyond measuring their sizes).
//!
//! `storage_add`, `snapshot_create` and `snapshot_delete` are gated: a viewer proposes one, the owner approves it with the
//! passkey, and the approved action runs once. Every write goes through Incus ([`arca_incus::SnapshotWriter`]); this process
//! never formats, mounts or writes a disk device, and makes only a folder pool's empty `arca-pool` folder. Before a whole
//! disk is erased the kernel must let this process open it and every partition exclusively. The facts a proposal depends on (the disk's model, size, serial, transport and
//! contents; a source's kind and identity; a snapshot's name) are read when it is made and bound into its arguments, so the
//! owner's digest covers them and the run refuses anything that changed. A watch-only node reads but has no writer.

use std::collections::BTreeSet;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arca_incus::folder::{
    CHANGED_KEY, FolderCopy, FolderOutcome, HELPER_KEY, INCOMPLETE_KEY, SEPARATE_SHOWN,
    SOURCE_PATH_KEY, changed_note, space_estimate,
};
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
/// Signatures that make a device part of something larger, besides any signature ending in `_member` (`linux_raid_member`,
/// firmware RAID such as `isw_raid_member` or `ddf_raid_member`, `LVM2_member`, `zfs_member` and the like). Swap is judged
/// on its own.
const MEMBER_FSTYPES: [&str; 6] = [
    "crypto_LUKS",
    "bcache",
    "ceph",
    "ceph_bluestore",
    "drbd",
    "swsuspend",
];

/// Whether a signature makes the device a member of something larger: a RAID (Linux or firmware), LVM, ZFS, encrypted,
/// bcache, Ceph, DRBD or hibernation device. Such a device is never free, assembled or not.
fn member_fstype(fstype: &str) -> bool {
    fstype.ends_with("_member") || MEMBER_FSTYPES.contains(&fstype)
}
/// The longest snapshot name Incus accepts for an instance.
const MAX_SNAPSHOT_NAME: usize = 63;

/// The host's block devices as `lsblk --json` prints them, the kernel's links between them, and the exclusive-open probe.
/// A test gives fixtures; the node runs `lsblk`, reads `/sys` and opens the device.
pub trait BlockDevices: Send + Sync {
    fn read(&self) -> Result<Value, String>;

    /// The members of mounted multi-device file systems and every device's holders, as the kernel lists them in `/sys`.
    fn links(&self) -> DeviceLinks {
        DeviceLinks::from_sysfs(Path::new("/sys"))
    }

    /// The same links for the in-use check before an erase, where a part that cannot be read is an error.
    fn checked_links(&self) -> Result<DeviceLinks, String> {
        DeviceLinks::try_from_sysfs(Path::new("/sys"))
    }

    /// Opens the block device at `path` read-only and exclusively, and closes it again.
    fn open_exclusive(&self, path: &str) -> ExclusiveOpen {
        open_exclusive(path)
    }

    /// The system's own lists of what uses a device, read when the exclusive open is not allowed.
    fn system_lists(&self) -> Result<SystemLists, String> {
        SystemLists::read(Path::new("/proc"), Path::new("/sys"))
    }
}

/// The system's own lists of what uses a device: every mount in this process's mount namespace, the active swap areas, and
/// each block device's `major:minor` number.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SystemLists {
    /// `/proc/self/mountinfo` as read.
    pub mountinfo: String,
    /// `/proc/swaps` as read.
    pub swaps: String,
    /// Each block device's kernel name and its `major:minor`, from `/sys/class/block/<name>/dev`.
    pub numbers: std::collections::BTreeMap<String, String>,
}

impl SystemLists {
    /// Reads the lists under `proc` and `sys`. The mount and swap lists must be readable; a check without them is no check.
    pub fn read(proc: &Path, sys: &Path) -> Result<SystemLists, String> {
        let mountinfo = std::fs::read_to_string(proc.join("self/mountinfo"))
            .map_err(|error| format!("the mount list could not be read: {error}"))?;
        let swaps = std::fs::read_to_string(proc.join("swaps"))
            .map_err(|error| format!("the swap list could not be read: {error}"))?;
        let mut numbers = std::collections::BTreeMap::new();
        for entry in std::fs::read_dir(sys.join("class/block"))
            .into_iter()
            .flatten()
            .flatten()
        {
            if let (Some(name), Ok(number)) = (
                entry.file_name().to_str().map(str::to_string),
                std::fs::read_to_string(entry.path().join("dev")),
            ) {
                numbers.insert(name, number.trim().to_string());
            }
        }
        Ok(SystemLists {
            mountinfo,
            swaps,
            numbers,
        })
    }

    /// Where the device with kernel name `name`, device number `number` and one of `paths` is mounted, if anywhere. A mount
    /// matches by its device number (field 3) or by its source (the field after the file system type).
    fn mount_of(&self, number: Option<&str>, paths: &[String]) -> Option<String> {
        self.mountinfo.lines().find_map(|line| {
            let (left, right) = line.split_once(" - ")?;
            let left: Vec<&str> = left.split_whitespace().collect();
            let source = right.split_whitespace().nth(1).map(unescape_octal);
            let by_number = number.is_some_and(|number| left.get(2) == Some(&number));
            let by_source = source.is_some_and(|source| paths.contains(&source));
            (by_number || by_source).then(|| {
                left.get(4)
                    .map(|mount| unescape_octal(mount))
                    .unwrap_or_default()
            })
        })
    }

    /// Whether one of `paths` is an active swap area.
    fn swap_of(&self, paths: &[String]) -> bool {
        self.swaps
            .lines()
            .skip(1)
            .filter_map(|line| line.split_whitespace().next())
            .any(|path| paths.contains(&unescape_octal(path)))
    }
}

/// The kernel's escaping in mount and swap lists: a space is `\040`, a tab `\011`, a newline `\012`, a backslash `\134`.
fn unescape_octal(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\'
            && index + 3 < bytes.len()
            && bytes[index + 1..index + 4]
                .iter()
                .all(|digit| (b'0'..=b'7').contains(digit))
        {
            let value = (bytes[index + 1] - b'0') * 64
                + (bytes[index + 2] - b'0') * 8
                + (bytes[index + 3] - b'0');
            out.push(value);
            index += 4;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// What the kernel says about a device the instant Arca asks to hold it alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExclusiveOpen {
    /// Nothing holds it: no mounted file system, RAID, volume group, encrypted mapping or swap.
    Free,
    /// The kernel refused (`EBUSY`): something holds it.
    Busy,
    /// This process may not open it, so it cannot tell.
    Denied(String),
    /// Any other answer, such as a device that is gone or a path that is not a block device.
    Failed(String),
}

/// The node's exclusive-open probe. `O_EXCL` on a block device without `O_CREAT` makes the kernel refuse with `EBUSY` while
/// a mounted file system, a RAID, LVM, device-mapper or swap holds it. It only reads, and the device is closed at once.
pub fn open_exclusive(path: &str) -> ExclusiveOpen {
    use rustix::fs::{FileType, Mode, OFlags};
    use rustix::io::Errno;
    let flags = OFlags::RDONLY | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOCTTY;
    match rustix::fs::open(path, flags, Mode::empty()) {
        Ok(fd) => match rustix::fs::fstat(&fd) {
            Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::BlockDevice => {
                ExclusiveOpen::Free
            }
            Ok(_) => ExclusiveOpen::Failed(format!("{path} is not a block device")),
            Err(error) => ExclusiveOpen::Failed(error.to_string()),
        },
        Err(Errno::BUSY) => ExclusiveOpen::Busy,
        Err(error @ (Errno::ACCESS | Errno::PERM)) => ExclusiveOpen::Denied(error.to_string()),
        Err(error) => ExclusiveOpen::Failed(error.to_string()),
    }
}

/// The kernel's own links between block devices.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceLinks {
    /// Each mounted multi-device file system the kernel lists: its type (`btrfs` or `bcachefs`), its UUID and the kernel
    /// names of its member devices.
    pub members: Vec<(String, String, Vec<String>)>,
    /// Each device that something holds (a RAID, LVM, device-mapper or bcache device on it), by kernel name, with the names
    /// of its holders.
    pub holders: std::collections::BTreeMap<String, Vec<String>>,
}

impl DeviceLinks {
    /// [`DeviceLinks::from_sysfs`], failing on any part that cannot be read. `/sys/fs/btrfs` and `/sys/fs/bcachefs` may be
    /// absent (no such file system is loaded); `/sys/class/block` and every listed directory under them must read.
    pub fn try_from_sysfs(root: &Path) -> Result<DeviceLinks, String> {
        let names = |dir: &Path, may_be_absent: bool| -> Result<Vec<String>, String> {
            let entries = match std::fs::read_dir(dir) {
                Ok(entries) => entries,
                Err(error) if may_be_absent && error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(Vec::new());
                }
                Err(error) => return Err(format!("{} could not be read: {error}", dir.display())),
            };
            let mut out = Vec::new();
            for entry in entries {
                let entry = entry
                    .map_err(|error| format!("{} could not be read: {error}", dir.display()))?;
                out.push(entry.file_name().to_string_lossy().into_owned());
            }
            out.sort();
            Ok(out)
        };
        let mut links = DeviceLinks::default();
        let btrfs = root.join("fs/btrfs");
        for uuid in names(&btrfs, true)? {
            let devices = btrfs.join(&uuid).join("devices");
            if !devices.is_dir() {
                continue;
            }
            let devices = names(&devices, false)?;
            if !devices.is_empty() {
                links.members.push(("btrfs".to_string(), uuid, devices));
            }
        }
        let bcachefs = root.join("fs/bcachefs");
        for uuid in names(&bcachefs, true)? {
            let dir = bcachefs.join(&uuid);
            if !dir.is_dir() {
                continue;
            }
            let mut devices = Vec::new();
            for entry in names(&dir, false)?
                .into_iter()
                .filter(|entry| entry.starts_with("dev-"))
            {
                let link = std::fs::read_link(dir.join(&entry).join("block")).map_err(|error| {
                    format!("{}/{entry}/block could not be read: {error}", dir.display())
                })?;
                devices.extend(
                    link.file_name()
                        .and_then(|name| name.to_str())
                        .map(str::to_string),
                );
            }
            if !devices.is_empty() {
                links.members.push(("bcachefs".to_string(), uuid, devices));
            }
        }
        let block = root.join("class/block");
        for device in names(&block, false)? {
            let holders = names(&block.join(&device).join("holders"), false)?;
            if !holders.is_empty() {
                links.holders.insert(device, holders);
            }
        }
        Ok(links)
    }

    /// Reads `/sys/fs/btrfs/<uuid>/devices/`, `/sys/fs/bcachefs/<uuid>/dev-*/block` and `/sys/class/block/*/holders/`
    /// under `root`, for the roles `storage_list` shows. A part that cannot be read adds nothing here; an erase never rests on
    /// this read alone, since the in-use check opens the disk exclusively or reads the links with
    /// [`DeviceLinks::try_from_sysfs`], where an unreadable part refuses.
    pub fn from_sysfs(root: &Path) -> DeviceLinks {
        let names = |dir: &Path| -> Vec<String> {
            let mut out: Vec<String> = std::fs::read_dir(dir)
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
                .collect();
            out.sort();
            out
        };
        let mut links = DeviceLinks::default();
        let btrfs = root.join("fs/btrfs");
        for uuid in names(&btrfs) {
            let devices = names(&btrfs.join(&uuid).join("devices"));
            if !devices.is_empty() {
                links.members.push(("btrfs".to_string(), uuid, devices));
            }
        }
        let bcachefs = root.join("fs/bcachefs");
        for uuid in names(&bcachefs) {
            let devices: Vec<String> = names(&bcachefs.join(&uuid))
                .into_iter()
                .filter(|entry| entry.starts_with("dev-"))
                .filter_map(|entry| {
                    let link =
                        std::fs::read_link(bcachefs.join(&uuid).join(entry).join("block")).ok()?;
                    link.file_name()?.to_str().map(str::to_string)
                })
                .collect();
            if !devices.is_empty() {
                links.members.push(("bcachefs".to_string(), uuid, devices));
            }
        }
        let block = root.join("class/block");
        for device in names(&block) {
            let holders = names(&block.join(&device).join("holders"));
            if !holders.is_empty() {
                links.holders.insert(device, holders);
            }
        }
        links
    }
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

/// A fixed `lsblk` answer, device links and exclusive-open answers, for tests. Every device opens as free unless a test
/// says otherwise.
pub struct FixedBlockDevices {
    lsblk: Mutex<Result<Value, String>>,
    links: Mutex<DeviceLinks>,
    opens: Mutex<std::collections::BTreeMap<String, ExclusiveOpen>>,
    opened: Mutex<Vec<String>>,
    lists: Mutex<Result<SystemLists, String>>,
    links_error: Mutex<Option<String>>,
}

fn held<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl FixedBlockDevices {
    pub fn new(value: Value) -> Self {
        Self {
            lsblk: Mutex::new(Ok(value)),
            links: Mutex::new(DeviceLinks::default()),
            opens: Mutex::new(std::collections::BTreeMap::new()),
            opened: Mutex::new(Vec::new()),
            lists: Mutex::new(Ok(SystemLists::default())),
            links_error: Mutex::new(None),
        }
    }

    /// Makes the checked read of the device links fail from now on.
    pub fn set_links_error(&self, why: &str) {
        *held(&self.links_error) = Some(why.to_string());
    }

    /// What the system's own lists say from now on.
    pub fn set_lists(&self, lists: Result<SystemLists, String>) {
        *held(&self.lists) = lists;
    }

    pub fn set(&self, value: Result<Value, String>) {
        *held(&self.lsblk) = value;
    }

    pub fn set_links(&self, links: DeviceLinks) {
        *held(&self.links) = links;
    }

    /// What the exclusive open of `path` answers from now on.
    pub fn set_open(&self, path: &str, answer: ExclusiveOpen) {
        held(&self.opens).insert(path.to_string(), answer);
    }

    /// Every path opened exclusively so far, in order.
    pub fn opened(&self) -> Vec<String> {
        held(&self.opened).clone()
    }
}

impl BlockDevices for FixedBlockDevices {
    fn read(&self) -> Result<Value, String> {
        held(&self.lsblk).clone()
    }

    fn links(&self) -> DeviceLinks {
        held(&self.links).clone()
    }

    fn open_exclusive(&self, path: &str) -> ExclusiveOpen {
        held(&self.opened).push(path.to_string());
        held(&self.opens)
            .get(path)
            .cloned()
            .unwrap_or(ExclusiveOpen::Free)
    }

    fn system_lists(&self) -> Result<SystemLists, String> {
        held(&self.lists).clone()
    }

    fn checked_links(&self) -> Result<DeviceLinks, String> {
        match held(&self.links_error).clone() {
            Some(why) => Err(why),
            None => Ok(self.links()),
        }
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
    /// For a folder snapshot pool: the UUID of the file system its folder is on ([`arca_incus::FS_UUID_KEY`]).
    pub fs_uuid: Option<String>,
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

/// What the host says about its disks beyond `lsblk`: the kernel's links between devices, and the devices given to apps.
#[derive(Debug, Clone, Default)]
pub struct HostFacts {
    pub links: DeviceLinks,
    /// Each host device passed through to an instance (a `unix-block` device, or a `disk` device whose source is under
    /// `/dev/`), with the app it is given to as `<project>/<instance>`.
    pub passthrough: Vec<(String, String)>,
}

/// What one device shares with the other members of its file system.
#[derive(Debug, Clone, Default)]
struct SharedFs {
    fstype: String,
    /// Mount points that `lsblk` lists on another member only.
    borrowed: Vec<String>,
    /// The other members.
    with: Vec<String>,
    /// The kernel lists the file system as mounted.
    mounted: bool,
}

/// Gives every disk its role, from `lsblk` and the pools alone.
pub fn classify(disks: &[BlockDevice], pools: &[PoolView]) -> Vec<DiskView> {
    classify_with(disks, pools, &HostFacts::default())
}

/// Gives every disk its role. Every member of one file system (the same type and UUID, or listed together by the kernel)
/// shares the mount points of all of them, since `lsblk` lists a multi-device btrfs or bcachefs mount on one member only. A
/// pool is matched by the device it names, by a UUID or label, by the file system UUID a folder snapshot pool recorded
/// (mounted or not), or, for any other folder pool, by the file system its folder is on.
pub fn classify_with(
    disks: &[BlockDevice],
    pools: &[PoolView],
    facts: &HostFacts,
) -> Vec<DiskView> {
    // The members of each file system, by (type, UUID).
    let mut groups: std::collections::BTreeMap<(String, String), BTreeSet<String>> =
        std::collections::BTreeMap::new();
    let mut kernel_mounted: BTreeSet<(String, String)> = BTreeSet::new();
    for disk in disks {
        for node in disk.walk() {
            if let (Some(fstype), Some(uuid)) = (&node.fstype, &node.uuid) {
                groups
                    .entry((fstype.clone(), uuid.clone()))
                    .or_default()
                    .insert(node.name.clone());
            }
        }
    }
    for (fstype, uuid, members) in &facts.links.members {
        let key = (fstype.clone(), uuid.clone());
        groups
            .entry(key.clone())
            .or_default()
            .extend(members.iter().cloned());
        kernel_mounted.insert(key);
    }
    let mounts_of = |name: &str| -> Vec<String> {
        disks
            .iter()
            .flat_map(|disk| disk.walk())
            .filter(|node| node.name == name)
            .flat_map(|node| node.mountpoints.clone())
            .collect()
    };
    let mut shared: std::collections::BTreeMap<String, SharedFs> =
        std::collections::BTreeMap::new();
    for ((fstype, uuid), members) in &groups {
        let mounted = kernel_mounted.contains(&(fstype.clone(), uuid.clone()));
        if members.len() < 2 && !mounted {
            continue;
        }
        for member in members {
            let own = mounts_of(member);
            let mut borrowed: Vec<String> = members
                .iter()
                .filter(|other| *other != member)
                .flat_map(|other| mounts_of(other))
                .filter(|mount| !own.contains(mount))
                .collect();
            borrowed.sort();
            borrowed.dedup();
            shared.insert(
                member.clone(),
                SharedFs {
                    fstype: fstype.clone(),
                    borrowed,
                    with: members
                        .iter()
                        .filter(|other| *other != member)
                        .cloned()
                        .collect(),
                    mounted,
                },
            );
        }
    }
    // The disks as the roles see them: each member carries the mount points of its whole file system.
    let disks: Vec<BlockDevice> = disks
        .iter()
        .map(|disk| {
            fn spread(
                device: &mut BlockDevice,
                shared: &std::collections::BTreeMap<String, SharedFs>,
            ) {
                if let Some(fs) = shared.get(&device.name) {
                    for mount in &fs.borrowed {
                        if !device.mountpoints.contains(mount) {
                            device.mountpoints.push(mount.clone());
                        }
                    }
                }
                for child in &mut device.children {
                    spread(child, shared);
                }
            }
            let mut disk = disk.clone();
            spread(&mut disk, &shared);
            disk
        })
        .collect();

    let mut pool_disks: Vec<Option<&PoolView>> = vec![None; disks.len()];
    for pool in pools {
        let by_fs_uuid: Vec<usize> = pool
            .fs_uuid
            .as_deref()
            .map(|fs_uuid| {
                disks
                    .iter()
                    .enumerate()
                    .filter(|(_, disk)| {
                        disk.walk()
                            .iter()
                            .any(|node| node.uuid.as_deref() == Some(fs_uuid))
                    })
                    .map(|(index, _)| index)
                    .collect()
            })
            .unwrap_or_default();
        for index in &by_fs_uuid {
            pool_disks[*index].get_or_insert(pool);
        }
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
                // A folder pool whose file system is known is on that disk, whether it is mounted now or not; only one
                // without it is judged by where its folder is mounted.
                if by_fs_uuid.is_empty()
                    && let Some((index, _)) = mount_owner(&disks, source)
                {
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
    let passthrough: Vec<(String, &str)> = facts
        .passthrough
        .iter()
        .flat_map(|(source, app)| {
            [
                (source.clone(), app.as_str()),
                (resolved_device(source), app.as_str()),
            ]
        })
        .collect();
    disks
        .iter()
        .zip(pool_disks)
        .map(|(disk, pool)| {
            let (role, reason) = role_of(disk, pool, &shared, &facts.links.holders, &passthrough);
            DiskView {
                disk: disk.clone(),
                role,
                reason,
            }
        })
        .collect()
}

fn role_of(
    disk: &BlockDevice,
    pool: Option<&PoolView>,
    shared: &std::collections::BTreeMap<String, SharedFs>,
    holders: &std::collections::BTreeMap<String, Vec<String>>,
    passthrough: &[(String, &str)],
) -> (DiskRole, String) {
    let nodes = disk.walk();
    for node in &nodes {
        if let Some((_, app)) = passthrough.iter().find(|(source, _)| *source == node.path) {
            return (
                DiskRole::System,
                format!(
                    "is given to the app {app} ({}); it is never offered",
                    node.name
                ),
            );
        }
        for mount in &node.mountpoints {
            if system_mount(mount) {
                let what = if mount == "[SWAP]" {
                    "swap".to_string()
                } else {
                    mount.clone()
                };
                if let Some(fs) = shared
                    .get(&node.name)
                    .filter(|fs| fs.borrowed.contains(mount))
                {
                    return (
                        DiskRole::System,
                        format!(
                            "is part of the {} file system that holds {what} ({} with {}); it is never offered",
                            fs.fstype,
                            node.name,
                            fs.with.join(", ")
                        ),
                    );
                }
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
        if node.fstype.as_deref().is_some_and(member_fstype)
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
        if let Some(found) = holders.get(&node.name) {
            return (
                DiskRole::System,
                format!(
                    "is held by {} ({}); it is never offered",
                    found.join(", "),
                    node.name
                ),
            );
        }
        if let Some(fs) = shared.get(&node.name).filter(|fs| fs.mounted)
            && node.mountpoints.is_empty()
        {
            return (
                DiskRole::System,
                format!(
                    "is part of a mounted {} file system ({}); it is never offered",
                    fs.fstype, node.name
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
    /// A folder snapshot's source folder.
    pub source_path: Option<String>,
    pub pool: String,
    pub size: Option<u64>,
    pub created: String,
    pub copying: bool,
    /// An instance copy's clean-up was written, or a folder copy's counts matched ([`arca_incus::VERIFIED_KEY`]). A volume
    /// copy is made with its final config in one request and needs none, so it is always true.
    pub verified: bool,
    /// Why a folder snapshot is incomplete ([`INCOMPLETE_KEY`]).
    pub reason: Option<String>,
    /// How many files changed while a verified folder copy read them ([`CHANGED_KEY`]).
    pub changed: Option<u64>,
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
            "sourcePath": self.source_path,
            "pool": self.pool,
            "size": self.size,
            "created": self.created,
            "state": if self.copying {
                "copying"
            } else if self.verified {
                "ready"
            } else {
                "incomplete"
            },
            "reason": if self.copying || self.verified { None } else { self.reason.clone() },
            "changedDuringCopy": self.changed,
            "note": self.changed.filter(|_| self.verified && !self.copying).map(changed_note),
            "droppedDevices": self.dropped,
        })
    }

    fn source(&self) -> String {
        match &self.source_path {
            Some(path) => path.clone(),
            None => format!("{}/{}", self.source_project, self.source_name),
        }
    }

    fn delete_kind(&self) -> &'static str {
        if self.kind == "volume" || self.kind == "folder" {
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
    /// How long an approved copy is waited on before it answers that it is still copying.
    wait: Duration,
    /// The folder snapshots this process is copying now. After a restart it is empty: a copy is never resumed.
    copies: Arc<Mutex<BTreeSet<String>>>,
    /// The machine's own pools, in the configured order (`incus.pools`): the first regular one holds a copy helper's
    /// root disk.
    helper_pools: Vec<String>,
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
            wait: Duration::from_secs(20),
            copies: Arc::new(Mutex::new(BTreeSet::new())),
            helper_pools: Vec::new(),
        }
    }

    /// The same storage, with the machine's own pools (`incus.pools`) a copy helper's root disk may go on, in order.
    pub fn with_helper_pools(mut self, pools: Vec<String>) -> Storage {
        self.helper_pools = pools;
        self
    }

    /// The regular pool a folder copy helper's root disk goes on: the first configured pool (`incus.pools`) that exists
    /// and is not snapshot storage, else the default profile's root pool when it is regular. Never a snapshot pool: an
    /// unprivileged container cannot walk a snapshot pool's folder, so it would not start.
    fn helper_root_pool(&self) -> Result<String, StorageRefusal> {
        let pools = self.pools()?;
        let regular = |name: &str| {
            plain_name(name)
                && pools
                    .iter()
                    .any(|pool| pool.name == name && !pool.snapshots)
        };
        if let Some(name) = self.helper_pools.iter().find(|name| regular(name)) {
            return Ok(name.clone());
        }
        if let Some(pool) = self
            .read
            .get("/1.0/profiles/default?project=default")
            .ok()
            .and_then(|profile| {
                profile
                    .pointer("/devices/root/pool")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .filter(|pool| regular(pool))
        {
            return Ok(pool);
        }
        Err(StorageRefusal::new(
            "HelperPoolMissing",
            "this machine has no regular storage pool for the short-lived copy helper (its default profile has no root disk), and the helper never runs on snapshot storage",
        ))
    }

    /// The same storage, waiting at most `wait` for a copy or delete before reporting it as still running.
    pub fn with_wait(mut self, wait: Duration) -> Storage {
        self.writer = self.writer.map(|writer| writer.with_wait(wait));
        self.wait = wait;
        self
    }

    fn copying_now(&self, name: &str) -> bool {
        self.copies
            .lock()
            .map(|copies| copies.contains(name))
            .unwrap_or(false)
    }

    /// Removes every folder copy helper a stopped process left in the snapshot project and marks its unfinished copy
    /// incomplete. The kernel runs it once when it starts, before it takes any call. Returns the helpers removed.
    pub fn sweep_copy_helpers(&self) -> Result<Vec<String>, StorageRefusal> {
        let copies = Arc::clone(&self.copies);
        self.writer()?
            .sweep_copy_helpers(&|name: &str| {
                copies
                    .lock()
                    .map(|copies| copies.contains(name))
                    .unwrap_or(true)
            })
            .map_err(|error| unavailable("the copy helpers", &error))
    }

    fn writer(&self) -> Result<&SnapshotWriter, StorageRefusal> {
        self.writer.as_ref().ok_or_else(|| {
            StorageRefusal::new(
                "StorageDisabled",
                "snapshot storage changes are disabled: this node is watch-only",
            )
        })
    }

    /// Every Incus pool, with its space. A pool whose name is not plain is kept, so the disk under it is still known to be
    /// taken, but its space is not read and nothing is ever written to it.
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
            let space = plain_name(name)
                .then(|| {
                    self.read
                        .get(&format!("/1.0/storage-pools/{name}/resources"))
                        .ok()
                })
                .flatten();
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
                snapshots: plain_name(name) && arca_incus::snapshot::is_snapshot_pool(row),
                fs_uuid: row
                    .pointer(&format!("/config/{}", arca_incus::FS_UUID_KEY))
                    .and_then(Value::as_str)
                    .filter(|uuid| !uuid.is_empty())
                    .map(str::to_string),
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
        let facts = HostFacts {
            links: self.devices.links(),
            passthrough: self.passthrough()?,
        };
        Ok(Survey {
            disks: classify_with(&disks, &pools, &facts),
            pools,
        })
    }

    /// Every host device given to an instance, in any project: a `unix-block` device (its source, or its path when it has
    /// none), or a `disk` device whose source is under `/dev/`.
    fn passthrough(&self) -> Result<Vec<(String, String)>, StorageRefusal> {
        let rows = self
            .read
            .get("/1.0/instances?all-projects=true&recursion=1")
            .map_err(|error| unavailable("the apps", &error))?;
        let mut out = Vec::new();
        for row in rows.as_array().into_iter().flatten() {
            let app = format!(
                "{}/{}",
                row.get("project")
                    .and_then(Value::as_str)
                    .unwrap_or("default"),
                row.get("name").and_then(Value::as_str).unwrap_or_default()
            );
            let devices = row
                .get("expanded_devices")
                .filter(|devices| devices.as_object().is_some_and(|map| !map.is_empty()))
                .or_else(|| row.get("devices"));
            for device in devices
                .and_then(Value::as_object)
                .into_iter()
                .flat_map(|map| map.values())
            {
                let source = device
                    .get("source")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty());
                let host = match device.get("type").and_then(Value::as_str) {
                    Some("unix-block") => {
                        source.or_else(|| device.get("path").and_then(Value::as_str))
                    }
                    Some("disk") => source.filter(|source| source.starts_with("/dev/")),
                    _ => None,
                };
                if let Some(host) = host {
                    out.push((host.to_string(), app.clone()));
                }
            }
        }
        Ok(out)
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
            if !plain_name(name) || row.pointer(&format!("/config/{HELPER_KEY}")).is_some() {
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
                source_path: None,
                changed: None,
                pool,
                size,
                created: config(CREATED_KEY).unwrap_or_else(|| {
                    row.get("created_at")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string()
                }),
                copying: busy(arca_kernel::ownership::IncusKind::Instance, None, name),
                verified: config(arca_incus::VERIFIED_KEY).as_deref() == Some("true"),
                reason: None,
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
        for pool in self
            .pools()?
            .into_iter()
            .filter(|pool| pool.snapshots && plain_name(&pool.name))
        {
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
                let folder = config(SOURCE_KIND_KEY).as_deref() == Some("folder");
                out.push(SnapshotView {
                    name: name.to_string(),
                    kind: if folder { "folder" } else { "volume" }.to_string(),
                    source_project: config(SOURCE_PROJECT_KEY).unwrap_or_default(),
                    source_name: config(SOURCE_NAME_KEY).unwrap_or_default(),
                    source_pool: config(SOURCE_POOL_KEY),
                    source_path: folder.then(|| config(SOURCE_PATH_KEY)).flatten(),
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
                    ) || (folder && self.copying_now(name)),
                    verified: !folder
                        || config(arca_incus::VERIFIED_KEY).as_deref() == Some("true"),
                    reason: folder.then(|| config(INCOMPLETE_KEY)).flatten(),
                    changed: folder
                        .then(|| config(CHANGED_KEY))
                        .flatten()
                        .and_then(|count| count.parse().ok())
                        .filter(|count| *count > 0),
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
                        "fsUuid",
                    ],
                )?;
                if args.get("inUseCheck").is_some() {
                    return Err("args/inUseCheck: a folder target is not opened".to_string());
                }
                if let Some(object) = args.as_object_mut() {
                    object.insert("path".to_string(), json!(clean));
                }
            }
            (false, true) => {
                let device = text(&args, "device");
                let name = device.strip_prefix("/dev/").unwrap_or(device).to_string();
                resolved(
                    &args,
                    &[
                        "disk",
                        "model",
                        "serial",
                        "transport",
                        "size",
                        "contents",
                        "inUseCheck",
                    ],
                )?;
                if !["exclusive open", "system lists"].contains(&text(&args, "inUseCheck")) {
                    return Err("args/inUseCheck: it is exclusive open or system lists".to_string());
                }
                if args.get("mount").is_some() || args.get("fsUuid").is_some() {
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
            let (source, kinds): (&str, &[&str]) = match (
                has(&args, "instance"),
                has(&args, "volume"),
                has(&args, "folder"),
            ) {
                (true, false, false) => ("instance", &["container", "virtual-machine"]),
                (false, true, false) => ("volume", &["volume"]),
                (false, false, true) => ("folder", &["folder"]),
                _ => {
                    return Err("args: name exactly one of instance, volume and folder".to_string());
                }
            };
            resolved(&args, &["name", "kind"])?;
            let folder_keys = FOLDER_KEYS;
            if source == "folder" {
                let clean = clean_folder(text(&args, "folder"))
                    .map_err(|why| why.replace("args/path", "args/folder"))?;
                resolved(&args, &folder_keys)?;
                let separate = args["separate"]
                    .as_array()
                    .ok_or("args/separate: a list of folders")?;
                if separate.len() > SEPARATE_SHOWN
                    || separate.iter().any(|path| {
                        path.as_str()
                            .is_none_or(|path| !path.starts_with(&format!("{clean}/")))
                    })
                {
                    return Err("args/separate: folders inside the folder, at most 100".to_string());
                }
                if args["separateCount"].as_u64().unwrap_or(0) < separate.len() as u64 {
                    return Err("args/separateCount: fewer than the folders listed".to_string());
                }
                if text(&args, "kind") != "folder" {
                    return Err("args/kind: it does not match the source".to_string());
                }
                for key in ["project", "sourcePool", "uuid"] {
                    if args.get(key).is_some() {
                        return Err(format!("args/{key}: a folder snapshot has none"));
                    }
                }
                let name = text(&args, "name");
                if !snapshot_name_shape(name)
                    || !name.starts_with(&name_parts(FOLDER_PREFIX, &folder_part(&clean)))
                {
                    return Err("args/name: it is not this folder's snapshot name".to_string());
                }
                if let Some(object) = args.as_object_mut() {
                    object.insert("folder".to_string(), json!(clean));
                }
                return Ok(args);
            }
            if let Some(key) = folder_keys.iter().find(|key| args.get(**key).is_some()) {
                return Err(format!("args/{key}: only a folder snapshot has it"));
            }
            if !has(&args, "project") {
                return Err(
                    "args/project: an app or data volume snapshot names its project".to_string(),
                );
            }
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

/// The `arca-pool` folder made for a new folder pool, held by its parent folder's handle.
pub struct PoolFolder {
    parent: std::os::fd::OwnedFd,
    made: bool,
}

impl PoolFolder {
    /// Whether `arca-pool` was made just now.
    pub fn made(&self) -> bool {
        self.made
    }

    /// Removes `arca-pool` again if it was made just now and is still empty, through the parent's handle (never by path).
    pub fn remove_if_made(&self) {
        if self.made {
            let _ = rustix::fs::unlinkat(
                &self.parent,
                arca_incus::snapshot::POOL_DIR_NAME,
                rustix::fs::AtFlags::REMOVEDIR,
            );
        }
    }
}

/// Makes `<folder>/arca-pool` for a folder pool, as Incus needs it to exist and be empty. The walk starts at `/` and opens
/// each part of the folder relative to the part before it, never following a link (`openat` with `O_NOFOLLOW`); a part
/// that is missing is made there (`mkdirat`, 0755) and then opened the same way, and `arca-pool` itself is made in the last
/// one (0700). A part that is a link or not a folder is refused, and so is an `arca-pool` that is not an empty plain
/// folder. Nothing is looked up by path twice.
pub fn make_pool_folder(folder: &str) -> Result<PoolFolder, StorageRefusal> {
    use rustix::fs::{Mode, OFlags};
    use rustix::io::Errno;
    check_pool_folder(folder)?;
    let changed = || {
        StorageRefusal::new(
            "DiskChanged",
            "the folder now leads somewhere else through a link; propose it again",
        )
    };
    let failed = |what: &str, error: Errno| {
        StorageRefusal::new(
            "FolderNotWritable",
            format!("Arca could not create {what}: {error}"),
        )
    };
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut dir =
        rustix::fs::open("/", flags, Mode::empty()).map_err(|error| failed("/", error))?;
    let mut walked = PathBuf::from("/");
    for part in Path::new(folder).components() {
        let name = match part {
            std::path::Component::RootDir => continue,
            std::path::Component::Normal(name) => name,
            _ => return Err(changed()),
        };
        walked.push(name);
        let next = match rustix::fs::openat(&dir, name, flags, Mode::empty()) {
            Ok(next) => next,
            Err(Errno::NOENT) => {
                match rustix::fs::mkdirat(&dir, name, Mode::from_raw_mode(0o755)) {
                    Ok(()) | Err(Errno::EXIST) => {}
                    Err(error) => return Err(failed(&walked.display().to_string(), error)),
                }
                rustix::fs::openat(&dir, name, flags, Mode::empty()).map_err(|_| changed())?
            }
            Err(Errno::LOOP | Errno::NOTDIR) => return Err(changed()),
            Err(error) => return Err(failed(&walked.display().to_string(), error)),
        };
        dir = next;
    }
    let pool_name = arca_incus::snapshot::POOL_DIR_NAME;
    let pool_path = folder_pool_path(folder);
    let made = match rustix::fs::mkdirat(&dir, pool_name, Mode::from_raw_mode(0o700)) {
        Ok(()) => true,
        Err(Errno::EXIST) => false,
        Err(error) => return Err(failed(&pool_path, error)),
    };
    let in_use = |why: &str| {
        StorageRefusal::new(
            "PoolFolderInUse",
            format!("{pool_path} is already there and {why}; choose another folder"),
        )
    };
    let pool = match rustix::fs::openat(&dir, pool_name, flags, Mode::empty()) {
        Ok(pool) => pool,
        Err(Errno::LOOP | Errno::NOTDIR) => return Err(in_use("is not a plain folder")),
        Err(error) => return Err(failed(&pool_path, error)),
    };
    if !made {
        let empty = rustix::fs::Dir::read_from(&pool)
            .map(|entries| {
                entries.flatten().all(|entry| {
                    let name = entry.file_name().to_bytes();
                    name == b"." || name == b".."
                })
            })
            .unwrap_or(false);
        if !empty {
            return Err(in_use("has files in it"));
        }
    }
    Ok(PoolFolder { parent: dir, made })
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

/// The first part of a folder snapshot's name: `folder--<path>--<UTC yyyymmddThhmm>`.
const FOLDER_PREFIX: &str = "folder";

/// Folders a folder snapshot never copies, nor anything under them: the kernel's live views, devices, boot files, scratch
/// space and Incus's own storage. `/` itself is refused too.
const SYSTEM_FOLDERS: [&str; 7] = [
    "/proc",
    "/sys",
    "/dev",
    "/run",
    "/boot",
    "/tmp",
    "/var/lib/incus",
];

/// The name part a folder gives its snapshot: the path without its leading `/` (`home`, `mnt-ai`). A path too long for a
/// snapshot name keeps its end and gains 8 hex digits of its SHA-256, so two long folders never share a name.
fn folder_part(folder: &str) -> String {
    let part = name_part(folder.trim_start_matches('/'));
    let budget = MAX_SNAPSHOT_NAME - FOLDER_PREFIX.len() - "----".len() - "yyyymmddThhmm".len();
    if part.len() <= budget {
        return part;
    }
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(folder.as_bytes());
    let hash: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    let tail = &part[part.len() - (budget - hash.len() - 1)..];
    format!("{}-{hash}", tail.trim_start_matches('-'))
}

/// Whether `path` is `root` or lies under it.
fn within(path: &str, root: &str) -> bool {
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// A folder snapshot's name: `folder--<path>--<UTC yyyymmddThhmm>` ([`folder_part`]).
pub fn folder_snapshot_name(folder: &str, now_ms: u64) -> Result<String, String> {
    snapshot_name(FOLDER_PREFIX, &folder_part(folder), now_ms)
}

/// The copy helper's instance name for a folder snapshot: `arca-copy-` and 16 hex digits of the snapshot name's SHA-256.
pub fn helper_name(snapshot: &str) -> String {
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(snapshot.as_bytes());
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("arca-copy-{hex}")
}

/// The rules a folder must pass that need only its path: not `/` or a system folder, there, a folder, and no link in any
/// part of the path.
fn folder_rules(folder: &str) -> Result<(), StorageRefusal> {
    let refused = |why: String| StorageRefusal::new("FolderRefused", why);
    if folder == "/" {
        return Err(refused(
            "the whole system (/) is not copied as a folder; choose a folder such as /home"
                .to_string(),
        ));
    }
    if let Some(root) = SYSTEM_FOLDERS.iter().find(|root| within(folder, root)) {
        return Err(refused(format!(
            "{folder} is in {root}, which holds the system's own live state rather than your files; it is never copied"
        )));
    }
    let mut walked = PathBuf::from("/");
    for part in Path::new(folder).components().skip(1) {
        walked.push(part);
        match std::fs::symlink_metadata(&walked) {
            Err(_) => {
                return Err(StorageRefusal::new(
                    "FolderNotFound",
                    format!("there is no folder at {folder}"),
                ));
            }
            Ok(meta) if meta.file_type().is_symlink() => {
                let target = std::fs::read_link(&walked)
                    .map(|target| format!(" to {}", target.display()))
                    .unwrap_or_default();
                return Err(StorageRefusal::new(
                    "FolderLink",
                    format!(
                        "{} is a link{target}; name the real folder instead, so the copy cannot be sent somewhere else",
                        walked.display()
                    ),
                ));
            }
            Ok(meta) if !meta.is_dir() => {
                return Err(StorageRefusal::new(
                    "FolderNotFound",
                    format!("{} is not a folder", walked.display()),
                ));
            }
            Ok(_) => {}
        }
    }
    Ok(())
}

/// The facts the kernel resolves for a folder snapshot when it is proposed; the owner's digest binds them all.
const FOLDER_KEYS: [&str; 8] = [
    "fsUuid",
    "size",
    "allocated",
    "files",
    "unreadable",
    "separate",
    "separateCount",
    "poolFree",
];

/// What walking a folder found: the bytes of its regular files, the bytes its paths take on disk, every path under it,
/// the folders it could not open, and the separate file systems inside it. It stays on the folder's own file system, as
/// the copy does: a mount or btrfs subvolume inside it counts as one path and is listed in `separate`, not walked. A
/// top-level `lost+found`, sockets and device nodes are left out, as the copy and its count leave them out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FolderMeasure {
    pub bytes: u64,
    /// The blocks the measured paths take, in bytes (sparse files count only what is written).
    pub allocated: u64,
    pub paths: u64,
    pub unreadable: u64,
    /// The paths where another file system starts, sorted.
    pub separate: Vec<String>,
}

/// Walks a folder without following links and measures it ([`FolderMeasure`]). Nothing is read but names and sizes.
pub fn measure_folder(folder: &str) -> FolderMeasure {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let root = Path::new(folder);
    let mut out = FolderMeasure::default();
    let Ok(top) = std::fs::symlink_metadata(root) else {
        return out;
    };
    let device = top.dev();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            out.unreadable += 1;
            continue;
        };
        for entry in entries {
            let Ok(entry) = entry else {
                out.unreadable += 1;
                continue;
            };
            if dir == root && entry.file_name() == "lost+found" {
                continue;
            }
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                out.paths += 1;
                continue;
            };
            let kind = meta.file_type();
            if kind.is_socket() || kind.is_block_device() || kind.is_char_device() {
                continue;
            }
            out.paths += 1;
            if meta.dev() != device {
                out.separate.push(path.to_string_lossy().into_owned());
                continue;
            }
            out.allocated = out
                .allocated
                .saturating_add(meta.blocks().saturating_mul(512));
            if meta.is_file() {
                out.bytes = out.bytes.saturating_add(meta.len());
            } else if meta.is_dir() {
                stack.push(path);
            }
        }
    }
    out.separate.sort();
    out
}

/// Whether a copy estimated at `size` bytes ([`space_estimate`]) fits in `free` bytes with 10% headroom.
fn fits(size: u64, free: u64) -> bool {
    size.saturating_add(size / 10) <= free
}

impl Storage {
    /// Checks a folder for a snapshot onto `pool` ([`folder_rules`], and not inside snapshot storage nor holding the target
    /// pool) and returns the UUID of the file system it is on (empty when `lsblk` shows none).
    fn folder_source(&self, folder: &str, pool: &str) -> Result<String, StorageRefusal> {
        folder_rules(folder)?;
        for found in self
            .pools()?
            .iter()
            .filter(|found| found.snapshots && found.driver == "dir")
        {
            for source in &found.sources {
                let source = resolved_folder(source);
                if within(folder, &source) {
                    return Err(StorageRefusal::new(
                        "FolderInSnapshotStorage",
                        format!(
                            "{folder} is inside the snapshot storage {} ({source}); snapshots are never copied into snapshot storage",
                            found.name
                        ),
                    ));
                }
                if found.name == pool && within(&source, folder) {
                    return Err(StorageRefusal::new(
                        "FolderInSnapshotStorage",
                        format!(
                            "the snapshot storage {pool} is inside {folder} ({source}), so the copy would copy itself; choose other snapshot storage"
                        ),
                    ));
                }
            }
        }
        let lsblk = self.devices.read().map_err(|why| {
            StorageRefusal::new(
                "StorageUnavailable",
                format!("the disk list could not be read: {why}"),
            )
        })?;
        let disks =
            parse_lsblk(&lsblk).map_err(|why| StorageRefusal::new("StorageUnavailable", why))?;
        let (index, mount) = mount_owner(&disks, folder).ok_or_else(|| {
            StorageRefusal::new(
                "FolderNotOnDisk",
                format!("{folder} is not on any disk in storage_list"),
            )
        })?;
        Ok(disks[index]
            .walk()
            .into_iter()
            .find(|node| node.mountpoints.contains(&mount))
            .and_then(|node| node.uuid.clone())
            .unwrap_or_default())
    }

    /// A snapshot pool's free space now.
    fn pool_free(&self, pool: &str) -> Result<u64, StorageRefusal> {
        self.pools()?
            .into_iter()
            .find(|found| found.name == pool)
            .and_then(|found| Some(found.total?.saturating_sub(found.used?)))
            .ok_or_else(|| {
                StorageRefusal::new(
                    "SpaceUnknown",
                    format!("the free space of the snapshot storage {pool} could not be read"),
                )
            })
    }
}

/// Checks an approved or proposed folder snapshot against the host now and returns the copy it will run: the folder
/// passes every rule, is on the file system the owner approved, and still fits the pool with 10% headroom.
fn folder_target(storage: &Storage, args: &Value, created: String) -> Result<Target, CallError> {
    let folder = text(args, "folder");
    let pool = text(args, "pool");
    let name = text(args, "name");
    let fs_uuid = storage.folder_source(folder, pool)?;
    if fs_uuid != text(args, "fsUuid") {
        return Err(StorageRefusal::new(
            "FolderChanged",
            format!(
                "{folder} is on another file system than when it was proposed; propose it again"
            ),
        )
        .into());
    }
    let size = args["size"].as_u64().unwrap_or(u64::MAX);
    let number = |key: &str| args[key].as_u64().unwrap_or(u64::MAX);
    let (allocated, paths) = (number("allocated"), number("files"));
    let needed = space_estimate(allocated, paths);
    let root_pool = storage.helper_root_pool()?;
    let free = storage.pool_free(pool)?;
    if !fits(needed, free) {
        return Err(StorageRefusal::new(
            "FolderTooBig",
            format!(
                "{folder} takes about {} on disk and the snapshot storage {pool} has {} free; it needs room for the folder and 10% more",
                short_size(needed),
                short_size(free)
            ),
        )
        .into());
    }
    Ok(Target::Folder(FolderCopy {
        name: name.to_string(),
        helper: helper_name(name),
        pool: pool.to_string(),
        root_pool,
        folder: folder.to_string(),
        fs_uuid,
        size,
        paths,
        allocated,
        unreadable: args["unreadable"].as_u64().unwrap_or(0),
        separate: args["separate"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        separate_count: args["separateCount"].as_u64().unwrap_or(0),
        created,
    }))
}

/// What the owner should know about a verified folder copy, in plain words: files that changed while being copied,
/// sockets and device nodes left out, separate file systems not copied, and anything that went wrong after the copy
/// (such as a helper that could not be deleted).
fn copy_notes(outcome: &FolderOutcome, copy: &FolderCopy) -> Vec<String> {
    let mut notes = Vec::new();
    if outcome.changed > 0 {
        notes.push(changed_note(outcome.changed));
    }
    if outcome.skipped_sockets + outcome.skipped_devices > 0 {
        notes.push(format!(
            "{} socket(s) and {} device node(s) were left out; apps make them again",
            outcome.skipped_sockets, outcome.skipped_devices
        ));
    }
    if copy.separate_count > 0 {
        notes.push(format!(
            "{} separate file system(s) inside the folder were not copied; snapshot them separately",
            copy.separate_count
        ));
    }
    if let Some(reason) = &outcome.reason {
        notes.push(reason.clone());
    }
    notes
}

/// A size in decimal units, as a card says it: `177 GB`, `4.2 GB`, `512 MB`.
pub fn short_size(bytes: u64) -> String {
    for (scale, unit) in [(1e12, "TB"), (1e9, "GB"), (1e6, "MB"), (1e3, "kB")] {
        let value = bytes as f64 / scale;
        if value >= 1.0 {
            return if value >= 10.0 {
                format!("{value:.0} {unit}")
            } else {
                format!("{value:.1} {unit}")
            };
        }
    }
    format!("{bytes} bytes")
}

/// A count with thousands separators: `1,234,567`.
fn grouped(count: u64) -> String {
    let digits = count.to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// A count as a card's summary says it: `1.2 M` from a million up, otherwise [`grouped`].
fn short_count(count: u64) -> String {
    if count >= 1_000_000 {
        format!("{:.1} M", count as f64 / 1e6)
    } else {
        grouped(count)
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
            let mut rows = vec![
                row("Disk", disk_words(args)),
                row("Device", text(args, "device").to_string()),
                row("On it now", text(args, "contents").to_string()),
            ];
            if text(args, "inUseCheck") == "system lists" {
                rows.push(row("In-use check", SYSTEM_LISTS_SENTENCE.to_string()));
            }
            rows.extend([
                row("Storage name", text(args, "name").to_string()),
                row(
                    "Effect",
                    format!(
                        "{ERASE_SENTENCE} Incus wipes and formats the whole disk as snapshot storage {} (btrfs, compressed); the disk is labelled with that name. Apps and data are copied onto it only when you approve each snapshot.",
                        text(args, "name")
                    ),
                ),
            ]);
            rows
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
        ActionKind::SnapshotCreate if text(args, "kind") == "folder" => {
            let folder = text(args, "folder");
            let pool = text(args, "pool");
            let number = |key: &str| args[key].as_u64().unwrap_or(0);
            let (size, unreadable) = (number("size"), number("unreadable"));
            let needed = space_estimate(number("allocated"), number("files"));
            let estimate = if unreadable > 0 {
                format!(
                    " (an estimate: {} folder(s) could not be measured)",
                    grouped(unreadable)
                )
            } else {
                String::new()
            };
            let mut rows = vec![
                row("Folder", folder.to_string()),
                row("Size", format!("{}{estimate}", short_size(size))),
                row(
                    "Files",
                    format!("{} files and folders", grouped(number("files"))),
                ),
                row("Target", format!("Snapshot storage {pool}")),
                row(
                    "Space it takes",
                    format!("about {}{estimate}", short_size(needed)),
                ),
                row(
                    "Free space after",
                    format!(
                        "about {}",
                        short_size(number("poolFree").saturating_sub(needed))
                    ),
                ),
                row("Snapshot name", text(args, "name").to_string()),
            ];
            if unreadable > 0 {
                rows.push(row(
                    "Not measured",
                    format!(
                        "{} folder(s) Arca could not open; they are copied, but their files are not counted above, so the snapshot gets no size limit",
                        grouped(unreadable)
                    ),
                ));
            }
            let separate: Vec<&str> = args["separate"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let separate_count = number("separateCount").max(separate.len() as u64);
            if separate_count > 0 {
                let shown = separate
                    .iter()
                    .take(10)
                    .copied()
                    .collect::<Vec<_>>()
                    .join(", ");
                let more = separate_count.saturating_sub(separate.len().min(10) as u64);
                rows.push(row(
                    "Not copied (separate file system)",
                    format!(
                        "{shown}{}; snapshot them separately",
                        if more > 0 {
                            format!(" and {} more", grouped(more))
                        } else {
                            String::new()
                        }
                    ),
                ));
            }
            rows.push(row("Read-only", "The folder is not changed".to_string()));
            rows
        }
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
            let what = if text(args, "source").starts_with('/') {
                "folder"
            } else if text(args, "kind") == "volume" {
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
        ActionKind::SnapshotCreate if text(args, "kind") == "folder" => format!(
            "Copy the folder {} ({}, {} files) into snapshot storage {}",
            text(args, "folder"),
            short_size(args["size"].as_u64().unwrap_or(0)),
            short_count(args["files"].as_u64().unwrap_or(0)),
            text(args, "pool")
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
                "fsUuid",
                "inUseCheck",
            ] {
                object.remove(key);
            }
            let survey = storage.survey()?;
            let (view, mount) = if let Some(device) = object.get("device").and_then(Value::as_str) {
                let view = find_whole_disk(&survey, device)?;
                // How the disk was found unused is bound too, so the card can say it plainly.
                let how = nothing_holds(storage, view)?;
                object.insert("inUseCheck".to_string(), json!(how.as_str()));
                (view, None)
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
                object.insert("fsUuid".to_string(), json!(folder_fs_uuid(view, &mount)?));
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
            } else if let Some(folder) = object
                .get("folder")
                .and_then(Value::as_str)
                .map(str::to_string)
            {
                for key in FOLDER_KEYS {
                    object.remove(key);
                }
                if !folder.is_empty() && folder.bytes().all(|b| b == b'/') {
                    folder_rules("/")?;
                }
                let folder = clean_folder(&folder)
                    .map_err(|why| CallError::Invalid(why.replace("args/path", "args/folder")))?;
                let pool = object
                    .get("pool")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                storage.require_snapshot_pool(&pool)?;
                // The cheap refusals first; the folder is walked only once they pass.
                let fs_uuid = storage.folder_source(&folder, &pool)?;
                let measured = measure_folder(&folder);
                let free = storage.pool_free(&pool)?;
                let name = folder_snapshot_name(&folder, now_ms)
                    .map_err(|why| StorageRefusal::new("SnapshotName", why))?;
                for (key, value) in [
                    ("folder", json!(folder)),
                    ("kind", json!("folder")),
                    ("name", json!(name)),
                    ("fsUuid", json!(fs_uuid)),
                    ("size", json!(measured.bytes)),
                    ("allocated", json!(measured.allocated)),
                    ("files", json!(measured.paths)),
                    ("unreadable", json!(measured.unreadable)),
                    (
                        "separate",
                        json!(
                            measured
                                .separate
                                .iter()
                                .take(SEPARATE_SHOWN)
                                .collect::<Vec<_>>()
                        ),
                    ),
                    ("separateCount", json!(measured.separate.len())),
                    ("poolFree", json!(free)),
                ] {
                    object.insert(key.to_string(), value);
                }
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

/// File systems a folder pool cannot use: they have no Linux owners or permissions, so they cannot hold app copies.
const FOLDER_UNFIT_FSTYPES: [&str; 6] = ["vfat", "exfat", "ntfs", "ntfs3", "msdos", "fat"];

/// The UUID of the file system a folder target's mount point belongs to (empty when `lsblk` shows none), after refusing a
/// file system that cannot hold app copies.
fn folder_fs_uuid(view: &DiskView, mount: &str) -> Result<String, StorageRefusal> {
    let node = view
        .disk
        .walk()
        .into_iter()
        .find(|node| node.mountpoints.iter().any(|found| found == mount));
    if let Some(fstype) = node
        .and_then(|node| node.fstype.as_deref())
        .filter(|fstype| FOLDER_UNFIT_FSTYPES.contains(fstype))
    {
        return Err(StorageRefusal::new(
            "FolderFileSystem",
            format!(
                "the folder is on a {fstype} file system, which cannot keep app copies (it has no Linux owners or permissions); choose a folder on a Linux file system such as ext4 or btrfs, or use a free disk whole"
            ),
        ));
    }
    Ok(node.and_then(|node| node.uuid.clone()).unwrap_or_default())
}

/// Refuses every whole-disk erase while a folder snapshot pool cannot be reached: its disk may be the one about to be
/// erased, unmounted and so not recognised.
fn no_unreachable_snapshot_folder(survey: &Survey) -> Result<(), StorageRefusal> {
    for pool in survey
        .pools
        .iter()
        .filter(|pool| pool.snapshots && pool.driver == "dir")
    {
        let source = pool.sources.first().map(String::as_str).unwrap_or_default();
        let folder_there = std::fs::symlink_metadata(source).is_ok_and(|meta| meta.is_dir());
        let disk_mounted = pool.fs_uuid.as_deref().is_none_or(|fs_uuid| {
            survey.disks.iter().any(|view| {
                view.disk.walk().iter().any(|node| {
                    node.uuid.as_deref() == Some(fs_uuid) && !node.mountpoints.is_empty()
                })
            })
        });
        if !folder_there || !disk_mounted {
            return Err(StorageRefusal::new(
                "SnapshotStorageUnreachable",
                format!(
                    "a snapshot storage folder isn't reachable: {} keeps its snapshots in {source}, whose disk isn't mounted now; plug it in or remove it first",
                    pool.name
                ),
            ));
        }
    }
    Ok(())
}

/// How a whole disk was found unused before it is erased.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InUseCheck {
    /// The kernel let this process open the disk and every partition exclusively.
    ExclusiveOpen,
    /// This process may not open the disk, so the system's own lists were read instead, and every one of them passed.
    SystemLists,
}

impl InUseCheck {
    fn as_str(self) -> &'static str {
        match self {
            Self::ExclusiveOpen => "exclusive open",
            Self::SystemLists => "system lists",
        }
    }
}

/// The card's line for a disk checked through the system's lists.
pub const SYSTEM_LISTS_SENTENCE: &str =
    "In-use check through system lists (Arca can't open the disk directly).";

fn disk_busy(name: &str, why: &str) -> StorageRefusal {
    StorageRefusal::new(
        "DiskBusy",
        format!("{name} is in use right now ({why}); it is never erased"),
    )
}

/// Opens the whole disk and every partition on it read-only and exclusively. The kernel refuses that while a mounted file
/// system, a RAID, a volume group, an encrypted mapping or swap holds it, whatever `lsblk` showed. When this process may not
/// open the disk (`EACCES` or `EPERM`, and only then), the system's own lists are read instead and every one must pass; any
/// other failure to open it means it cannot be checked, and it is not erased.
fn nothing_holds(storage: &Storage, view: &DiskView) -> Result<InUseCheck, StorageRefusal> {
    let mut denied = false;
    for node in view.disk.walk() {
        match storage.devices.open_exclusive(&node.path) {
            ExclusiveOpen::Free => {}
            ExclusiveOpen::Busy => {
                return Err(disk_busy(
                    &node.name,
                    "the system holds it: a mounted file system, a RAID, a volume group or swap",
                ));
            }
            ExclusiveOpen::Denied(_) => denied = true,
            ExclusiveOpen::Failed(why) => {
                return Err(StorageRefusal::new(
                    "DiskUncheckable",
                    format!(
                        "Arca could not open {} to check that nothing uses it ({why}); a disk is erased only after that check",
                        node.path
                    ),
                ));
            }
        }
    }
    if !denied {
        return Ok(InUseCheck::ExclusiveOpen);
    }
    listed_unused(storage, view)?;
    Ok(InUseCheck::SystemLists)
}

/// The in-use check through the system's lists, for a disk this process may not open. Every check must pass, for the disk
/// and every partition: no mount in `/proc/self/mountinfo` (by device number or source), no swap in `/proc/swaps`, no
/// holders, no membership of a mounted btrfs or bcachefs, no RAID, LVM, ZFS, LUKS or swap signature, and the classifier's
/// own rules (the file system siblings of a mount, and devices given to apps) must still find the disk free.
fn listed_unused(storage: &Storage, view: &DiskView) -> Result<(), StorageRefusal> {
    if view.role != DiskRole::Free {
        return Err(disk_busy(&view.disk.name, &view.reason));
    }
    let lists = storage.devices.system_lists().map_err(|why| {
        StorageRefusal::new(
            "DiskUncheckable",
            format!(
                "Arca could not check that {} is unused: {why}",
                view.disk.name
            ),
        )
    })?;
    let uncheckable = |why: String| {
        StorageRefusal::new(
            "DiskUncheckable",
            format!(
                "Arca could not check that {} is unused: {why}",
                view.disk.name
            ),
        )
    };
    let links = storage.devices.checked_links().map_err(uncheckable)?;
    for node in view.disk.walk() {
        let mut paths = vec![node.path.clone(), resolved_device(&node.path)];
        paths.dedup();
        let number = lists
            .numbers
            .get(&node.name)
            .map(String::as_str)
            .ok_or_else(|| {
                uncheckable(format!(
                    "the system does not list {}'s device number",
                    node.name
                ))
            })?;
        let number = Some(number);
        if let Some(mount) = lists.mount_of(number, &paths) {
            return Err(disk_busy(&node.name, &format!("it is mounted at {mount}")));
        }
        if lists.swap_of(&paths) {
            return Err(disk_busy(&node.name, "it is used as swap"));
        }
        if let Some(holders) = links.holders.get(&node.name) {
            return Err(disk_busy(
                &node.name,
                &format!("it is held by {}", holders.join(", ")),
            ));
        }
        if let Some((fstype, _, _)) = links
            .members
            .iter()
            .find(|(_, _, members)| members.contains(&node.name))
        {
            return Err(disk_busy(
                &node.name,
                &format!("it is part of a mounted {fstype} file system"),
            ));
        }
        if let Some(fstype) = node
            .fstype
            .as_deref()
            .filter(|fstype| member_fstype(fstype) || *fstype == "swap")
        {
            return Err(disk_busy(&node.name, &format!("it is a {fstype} member")));
        }
    }
    Ok(())
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
    Folder(FolderCopy),
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
                no_unreachable_snapshot_folder(&survey)?;
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
                let fs_uuid = folder_fs_uuid(view, &mount)?;
                if text(args, "fsUuid") != fs_uuid {
                    return Err(StorageRefusal::new(
                        "DiskChanged",
                        "the folder's file system changed after the proposal; propose it again",
                    )
                    .into());
                }
                let source = PoolSource::Folder {
                    path: path.to_string(),
                    fs_uuid: (!fs_uuid.is_empty()).then_some(fs_uuid),
                };
                (view, Some(mount), source)
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
            match &source {
                PoolSource::Folder { path, .. } => check_pool_folder(path)?,
                // Last, on the disk the owner approved: the kernel itself says whether anything holds it now.
                PoolSource::Device(_) => {
                    let how = nothing_holds(storage, view)?;
                    if text(args, "inUseCheck") != how.as_str() {
                        return Err(StorageRefusal::new(
                            "DiskChanged",
                            "the disk's in-use check changed after the proposal; propose it again",
                        )
                        .into());
                    }
                }
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
            if text(args, "kind") == "folder" {
                return folder_target(storage, args, created);
            }
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
            if snapshot.kind == "folder" && snapshot.copying {
                return Err(StorageRefusal::new(
                    "SnapshotBusy",
                    format!("{name} is still being copied; delete it once the copy ends"),
                )
                .into());
            }
            if snapshot.delete_kind() == "volume" {
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

impl StorageActionHandler {
    /// Where a snapshot copy finished in the background reports a failure: the service log, and the action journal against
    /// the approved proposal, as `SnapshotCleanupFailed`.
    fn late_report(&self, approved_id: Option<String>) -> arca_incus::LateReport {
        let kernel = Arc::clone(&self.kernel);
        Arc::new(move |message: &str| {
            eprintln!("arca: {message}");
            if let (Some(id), Ok(mut kernel)) = (&approved_id, kernel.lock()) {
                kernel.journal_late_failure(id, "SnapshotCleanupFailed");
            }
        })
    }

    /// Runs an approved folder copy on its own thread and waits for it as long as the storage's wait. A copy that ends in
    /// time answers its outcome: `copied`, or `SnapshotIncomplete` with the reason (the volume is kept, marked incomplete).
    /// A longer one answers `copying`; when it ends, its outcome is journalled against the approved proposal
    /// (`SnapshotCopied`, `SnapshotIncomplete` or `StorageWriteFailed`) and logged. `snapshot_list` shows it as copying
    /// until then.
    fn run_folder_copy(
        &self,
        storage: &Storage,
        writer: &SnapshotWriter,
        copy: FolderCopy,
        approved_id: Option<String>,
    ) -> Result<Value, CallError> {
        type Ended = Result<FolderOutcome, IncusError>;
        struct Handoff {
            ended: Option<Ended>,
            abandoned: bool,
        }
        /// Takes the copy off the live list however the thread ends.
        struct Live(Arc<Mutex<BTreeSet<String>>>, String);
        impl Drop for Live {
            fn drop(&mut self) {
                if let Ok(mut copies) = self.0.lock() {
                    copies.remove(&self.1);
                }
            }
        }
        {
            let mut copies = storage
                .copies
                .lock()
                .map_err(|_| CallError::Internal("the copy list lock is poisoned".to_string()))?;
            if !copies.insert(copy.name.clone()) {
                return Err(StorageRefusal::new(
                    "SnapshotExists",
                    format!("{} is being copied already", copy.name),
                )
                .into());
            }
        }
        let live = Live(Arc::clone(&storage.copies), copy.name.clone());
        let handoff = Arc::new((
            Mutex::new(Handoff {
                ended: None,
                abandoned: false,
            }),
            std::sync::Condvar::new(),
        ));
        let (thread_handoff, thread_writer, thread_copy) =
            (Arc::clone(&handoff), writer.clone(), copy.clone());
        let kernel = Arc::clone(&self.kernel);
        std::thread::spawn(move || {
            let _live = live;
            let ended: Ended = thread_writer
                .create_folder_volume(&thread_copy)
                .map(|()| thread_writer.run_folder_copy(&thread_copy));
            let (lock, wake) = &*thread_handoff;
            let Ok(mut state) = lock.lock() else { return };
            if !state.abandoned {
                state.ended = Some(ended);
                wake.notify_all();
                return;
            }
            drop(state);
            let (ok, code, message) = match &ended {
                Ok(outcome) if outcome.verified => (
                    true,
                    "SnapshotCopied",
                    format!(
                        "the folder snapshot {} is copied and verified{}",
                        thread_copy.name,
                        copy_notes(outcome, &thread_copy)
                            .iter()
                            .map(|note| format!("; {note}"))
                            .collect::<String>()
                    ),
                ),
                Ok(outcome) => (
                    false,
                    "SnapshotIncomplete",
                    format!(
                        "the folder snapshot {} is incomplete: {}",
                        thread_copy.name,
                        outcome
                            .reason
                            .as_deref()
                            .unwrap_or("the copy did not finish")
                    ),
                ),
                Err(error) => (
                    false,
                    "StorageWriteFailed",
                    format!("the folder snapshot {} failed: {error}", thread_copy.name),
                ),
            };
            eprintln!("arca: {message}");
            if let (Some(id), Ok(mut kernel)) = (&approved_id, kernel.lock()) {
                kernel.journal_late_result(id, ok, code);
            }
        });
        let (lock, wake) = &*handoff;
        let state = lock
            .lock()
            .map_err(|_| CallError::Internal("the copy hand-off lock is poisoned".to_string()))?;
        let (mut state, _) = wake
            .wait_timeout_while(state, storage.wait, |state| state.ended.is_none())
            .map_err(|_| CallError::Internal("the copy hand-off lock is poisoned".to_string()))?;
        let answer = |state: &str| {
            json!({
                "snapshot": copy.name, "pool": copy.pool, "kind": "folder", "state": state,
                "folder": copy.folder,
            })
        };
        match state.ended.take() {
            None => {
                state.abandoned = true;
                Ok(answer("copying"))
            }
            Some(Ok(outcome)) if outcome.verified => {
                let mut done = answer("copied");
                done["ioCaps"] = json!(outcome.io_caps);
                done["io"] = json!(if outcome.io_caps {
                    "incus-caps"
                } else {
                    "throttled-by-tar"
                });
                if let Some(copied) = outcome.copy {
                    done["files"] = json!(copied.paths);
                    done["bytes"] = json!(copied.bytes);
                }
                done["changedDuringCopy"] = json!(outcome.changed);
                if !outcome.changed_head.is_empty() {
                    done["changedFiles"] = json!(outcome.changed_head);
                }
                done["skipped"] = json!({
                    "sockets": outcome.skipped_sockets,
                    "devices": outcome.skipped_devices,
                });
                done["separate"] = json!(copy.separate);
                done["separateCount"] = json!(copy.separate_count);
                let notes = copy_notes(&outcome, &copy);
                if !notes.is_empty() {
                    done["notes"] = json!(notes);
                }
                Ok(done)
            }
            Some(Ok(outcome)) => Err(StorageRefusal::new(
                "SnapshotIncomplete",
                format!(
                    "the copy of {} is kept but marked incomplete: {}",
                    copy.folder,
                    outcome
                        .reason
                        .as_deref()
                        .unwrap_or("the copy did not finish")
                ),
            )
            .into()),
            Some(Err(error)) => Err(write_failed(error)),
        }
    }
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
        let approved_id = {
            let mut kernel = self
                .kernel
                .lock()
                .map_err(|_| CallError::Internal("the kernel lock is poisoned".to_string()))?;
            let approved_id = kernel.action_grant_id(&grant);
            if !kernel.consume_action_grant(self.action.as_str(), &args, &grant) {
                return Err(CallError::Refused {
                    code: "NotApproved".to_string(),
                    message: "this action runs only from an approved proposal".to_string(),
                });
            }
            approved_id
        };
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
                    PoolSource::Folder { path, .. } => Some(make_pool_folder(path)?),
                    PoolSource::Device(_) => None,
                };
                if let Err(error) = writer.create_pool(&name, &source) {
                    if let Some(folder) = &made {
                        folder.remove_if_made();
                    }
                    return Err(write_failed(error));
                }
                let (driver, path) = match &source {
                    PoolSource::Folder { path, .. } => ("dir", folder_pool_path(path)),
                    PoolSource::Device(device) => ("btrfs", device.clone()),
                };
                Ok(json!({ "pool": name, "driver": driver, "source": path }))
            }
            Target::Instance { pool, body } => {
                writer.ensure_project().map_err(write_failed)?;
                let progress = writer
                    .copy_instance_reporting(&pool, &body, self.late_report(approved_id))
                    .map_err(write_failed)?;
                Ok(json!({
                    "snapshot": body["name"], "pool": pool, "kind": text(&args, "kind"),
                    "state": state(progress, "copied", "copying"),
                }))
            }
            Target::Volume { pool, body } => {
                writer.ensure_project().map_err(write_failed)?;
                let progress = writer
                    .copy_volume_reporting(&pool, &body, self.late_report(approved_id))
                    .map_err(write_failed)?;
                Ok(json!({
                    "snapshot": body["name"], "pool": pool, "kind": "volume",
                    "state": state(progress, "copied", "copying"),
                }))
            }
            Target::Folder(copy) => {
                writer.ensure_project().map_err(write_failed)?;
                self.run_folder_copy(storage, writer, copy, approved_id)
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

    #[test]
    fn the_kernels_device_links_are_read_from_sysfs() {
        let root = std::env::temp_dir().join(format!(
            "arca-sysfs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let make = |path: &str| std::fs::create_dir_all(root.join(path)).expect("a folder");
        let link = |target: &str, path: &str| {
            std::os::unix::fs::symlink(target, root.join(path)).expect("a link")
        };
        make("fs/btrfs/uuid-a/devices");
        make("fs/btrfs/features");
        link("../../../../devices/x/sda2", "fs/btrfs/uuid-a/devices/sda2");
        link("../../../../devices/x/sdb", "fs/btrfs/uuid-a/devices/sdb");
        make("fs/bcachefs/uuid-b/dev-0");
        make("fs/bcachefs/uuid-b/options");
        link(
            "../../../../devices/y/block/sdc",
            "fs/bcachefs/uuid-b/dev-0/block",
        );
        make("class/block/sdd1/holders");
        make("class/block/sde/holders");
        link("../../dm-0", "class/block/sdd1/holders/md127");
        let links = DeviceLinks::from_sysfs(&root);
        assert_eq!(
            DeviceLinks::try_from_sysfs(&root),
            Ok(links.clone()),
            "the checked read agrees"
        );
        assert_eq!(
            links.members,
            vec![
                (
                    "btrfs".to_string(),
                    "uuid-a".to_string(),
                    vec!["sda2".to_string(), "sdb".to_string()]
                ),
                (
                    "bcachefs".to_string(),
                    "uuid-b".to_string(),
                    vec!["sdc".to_string()]
                ),
            ]
        );
        assert_eq!(
            links.holders,
            [("sdd1".to_string(), vec!["md127".to_string()])]
                .into_iter()
                .collect()
        );
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(
            DeviceLinks::from_sysfs(&root),
            DeviceLinks::default(),
            "no sysfs, no links"
        );
        assert!(
            DeviceLinks::try_from_sysfs(&root).is_err(),
            "but the checked read refuses a missing /sys/class/block"
        );
    }

    fn device(
        name: &str,
        kind: &str,
        fstype: Option<&str>,
        children: Vec<BlockDevice>,
    ) -> BlockDevice {
        BlockDevice {
            name: name.into(),
            path: format!("/dev/{name}"),
            size: 1,
            model: String::new(),
            serial: String::new(),
            transport: String::new(),
            removable: false,
            kind: kind.into(),
            fstype: fstype.map(str::to_string),
            label: None,
            uuid: None,
            mountpoints: Vec::new(),
            children,
        }
    }

    /// A disk the classifier called free, with one partition carrying `fstype`.
    fn free_view(fstype: Option<&str>) -> DiskView {
        DiskView {
            disk: device(
                "sdb",
                "disk",
                None,
                vec![device("sdb1", "part", fstype, Vec::new())],
            ),
            role: DiskRole::Free,
            reason: String::new(),
        }
    }

    fn numbered(names: &[(&str, &str)]) -> SystemLists {
        SystemLists {
            numbers: names
                .iter()
                .map(|(name, number)| (name.to_string(), number.to_string()))
                .collect(),
            ..SystemLists::default()
        }
    }

    #[test]
    fn the_list_check_refuses_members_and_anything_it_cannot_read() {
        let devices = Arc::new(FixedBlockDevices::new(json!({ "blockdevices": [] })));
        let storage = Storage::new(
            PathBuf::from("/nonexistent/incus.sock"),
            devices.clone(),
            false,
        );
        devices.set_lists(Ok(numbered(&[("sdb", "8:16"), ("sdb1", "8:17")])));
        assert_eq!(listed_unused(&storage, &free_view(Some("ext4"))), Ok(()));
        // A signature the lists know as a member refuses, even when the classifier somehow said free.
        for fstype in [
            "isw_raid_member",
            "ddf_raid_member",
            "linux_raid_member",
            "drbd",
            "swsuspend",
            "swap",
        ] {
            let refused = listed_unused(&storage, &free_view(Some(fstype))).expect_err(fstype);
            assert_eq!(refused.code, "DiskBusy", "{fstype}");
        }
        // A partition the system lists no device number for cannot be checked.
        devices.set_lists(Ok(numbered(&[("sdb", "8:16")])));
        let refused = listed_unused(&storage, &free_view(Some("ext4"))).expect_err("no number");
        assert_eq!(refused.code, "DiskUncheckable");
        assert!(refused.message.contains("sdb1"), "{}", refused.message);
        // Links that cannot be read cannot be checked either.
        devices.set_lists(Ok(numbered(&[("sdb", "8:16"), ("sdb1", "8:17")])));
        devices.set_links_error("/sys/class/block could not be read");
        let refused = listed_unused(&storage, &free_view(Some("ext4"))).expect_err("no links");
        assert_eq!(refused.code, "DiskUncheckable");
    }

    #[test]
    fn every_member_signature_is_a_member() {
        for fstype in [
            "linux_raid_member",
            "isw_raid_member",
            "ddf_raid_member",
            "via_raid_member",
            "LVM2_member",
            "zfs_member",
            "crypto_LUKS",
            "bcache",
            "ceph",
            "ceph_bluestore",
            "drbd",
            "swsuspend",
        ] {
            assert!(member_fstype(fstype), "{fstype}");
        }
        for fstype in ["ext4", "btrfs", "vfat", "exfat", "swap"] {
            assert!(!member_fstype(fstype), "{fstype}");
        }
    }

    #[test]
    fn the_system_lists_are_read_and_matched() {
        assert_eq!(unescape_octal("/mnt/a\\040b\\134c"), "/mnt/a b\\c");
        assert_eq!(unescape_octal("/x\\04"), "/x\\04", "a short escape is kept");
        let lists = SystemLists {
            mountinfo: "30 1 8:17 / /srv/a rw - ext4 /dev/sdb1 rw\n31 1 0:45 / /srv/b rw - btrfs /dev/sdc rw\n"
                .to_string(),
            swaps: "Filename Type Size Used Priority\n/dev/sdd2 partition 1 0 -2\n".to_string(),
            numbers: Default::default(),
        };
        assert_eq!(
            lists.mount_of(Some("8:17"), &[]),
            Some("/srv/a".to_string())
        );
        assert_eq!(
            lists.mount_of(None, &["/dev/sdc".to_string()]),
            Some("/srv/b".to_string())
        );
        assert_eq!(
            lists.mount_of(Some("8:32"), &["/dev/sde".to_string()]),
            None
        );
        assert!(lists.swap_of(&["/dev/sdd2".to_string()]));
        assert!(
            !lists.swap_of(&["Filename".to_string()]),
            "the header is not a swap"
        );
        // This host's own lists read.
        let here = SystemLists::read(Path::new("/proc"), Path::new("/sys")).expect("readable");
        assert!(here.mountinfo.contains(" / / "), "the root mount is listed");
    }

    #[test]
    fn an_exclusive_open_of_something_that_is_not_a_block_device_fails() {
        assert!(matches!(
            open_exclusive("/proc/self/status"),
            ExclusiveOpen::Failed(_)
        ));
        assert!(matches!(
            open_exclusive("/dev/arca-no-such-disk"),
            ExclusiveOpen::Failed(_)
        ));
    }

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
    fn folder_snapshot_names_fit_and_long_folders_never_share_one() {
        let now = 1_793_361_600_000;
        assert_eq!(
            folder_snapshot_name("/home", now).expect("a name"),
            "folder--home--20261030T1200"
        );
        assert_eq!(
            folder_snapshot_name("/mnt/ai", now).expect("a name"),
            "folder--mnt-ai--20261030T1200"
        );
        let deep = "/srv/projects/customers/very-long-customer-name/archive";
        let folders = [format!("{deep}/2025"), format!("{deep}/2026")];
        let [a, b] = folders
            .clone()
            .map(|folder| folder_snapshot_name(&folder, now).expect("a name"));
        assert_ne!(a, b);
        for (folder, name) in folders.iter().zip([&a, &b]) {
            assert!(snapshot_name_shape(name), "{name}");
            assert!(
                name.starts_with(&name_parts(FOLDER_PREFIX, &folder_part(folder))),
                "prepare finds the folder's own prefix in {name}"
            );
        }
        assert!(a.contains("archive-2025-"), "{a}");
        assert!(helper_name(&a).len() <= 63 && plain_name(&helper_name(&a)));
        assert_ne!(helper_name(&a), helper_name(&b));
    }

    /// Folders that could not be measured make the size an estimate with no limit, and separate file systems are named
    /// (ten of them, then how many more) with the advice to snapshot them separately.
    #[test]
    fn a_folder_card_names_estimates_and_separate_file_systems() {
        let separate: Vec<String> = (0..12).map(|n| format!("/home/sub{n:02}")).collect();
        let args = json!({
            "folder": "/home", "pool": "usb1", "kind": "folder", "name": "folder--home--20261109T1405",
            "size": 177_000_000_000u64, "allocated": 100_000_000_000u64, "files": 1_000_000,
            "unreadable": 2, "separate": separate, "separateCount": 12, "poolFree": 2_000_000_000_000u64,
            "fsUuid": "uuid-home",
        });
        let rows = details(ActionKind::SnapshotCreate, &args);
        let row = |label: &str| {
            rows.iter()
                .find(|row| row.label == label)
                .map(|row| row.value.clone())
                .unwrap_or_else(|| panic!("no {label} row in {rows:?}"))
        };
        assert_eq!(
            row("Size"),
            "177 GB (an estimate: 2 folder(s) could not be measured)"
        );
        assert_eq!(
            row("Space it takes"),
            "about 104 GB (an estimate: 2 folder(s) could not be measured)",
            "the blocks plus 4 KiB a path"
        );
        assert!(row("Not measured").contains("no size limit"));
        let not_copied = row("Not copied (separate file system)");
        assert!(
            not_copied.starts_with("/home/sub00, /home/sub01"),
            "{not_copied}"
        );
        assert!(not_copied.contains("/home/sub09") && !not_copied.contains("/home/sub10"));
        assert!(
            not_copied.ends_with(" and 2 more; snapshot them separately"),
            "{not_copied}"
        );
        assert!(
            prepare(ActionKind::SnapshotCreate, {
                let mut wrong = args.clone();
                wrong["separate"] = json!(["/etc/elsewhere"]);
                wrong
            })
            .is_err_and(|why| why.contains("args/separate")),
            "a separate file system outside the folder is refused"
        );
    }

    #[test]
    fn card_sizes_and_counts_read_plainly() {
        assert_eq!(short_size(177_000_000_000), "177 GB");
        assert_eq!(short_size(4_200_000_000), "4.2 GB");
        assert_eq!(short_size(1_500_000_000_000), "1.5 TB");
        assert_eq!(short_size(512), "512 bytes");
        assert_eq!(short_count(1_234_567), "1.2 M");
        assert_eq!(short_count(12_345), "12,345");
        assert_eq!(grouped(1_000), "1,000");
        assert_eq!(grouped(999), "999");
        assert!(fits(100, 110) && !fits(100, 109));
    }

    #[test]
    fn a_folder_is_measured_without_following_links_and_counts_what_it_cannot_open() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("arca-measure-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("a/b")).expect("folders");
        std::fs::create_dir_all(root.join("lost+found/x")).expect("lost+found");
        std::fs::write(root.join("a/one"), vec![0u8; 100]).expect("a file");
        std::fs::write(root.join("lost+found/x/skip"), vec![0u8; 50]).expect("a file");
        std::os::unix::fs::symlink("/etc", root.join("a/link")).expect("a link");
        let _socket =
            std::os::unix::net::UnixListener::bind(root.join("a/s.sock")).expect("a socket");
        std::fs::File::create(root.join("a/holes"))
            .and_then(|file| file.set_len(64 << 20))
            .expect("a sparse file");
        std::fs::create_dir(root.join("closed")).expect("a folder");
        std::fs::write(root.join("closed/hidden"), b"x").expect("a file");
        std::fs::set_permissions(root.join("closed"), std::fs::Permissions::from_mode(0o000))
            .expect("mode");
        let measured = measure_folder(root.to_str().expect("utf-8"));
        let running_as_root = std::fs::read_dir(root.join("closed")).is_ok();
        // a, a/b, a/one, a/link, a/holes, closed (and closed/hidden when this runs as root); lost+found and the socket
        // are left out.
        let holes = 64 << 20;
        assert_eq!(
            measured.bytes,
            holes + if running_as_root { 101 } else { 100 }
        );
        assert_eq!(measured.paths, if running_as_root { 7 } else { 6 });
        assert_eq!(measured.unreadable, u64::from(!running_as_root));
        assert!(
            measured.allocated > 0 && measured.allocated < holes,
            "a sparse file counts its blocks, not its length: {measured:?}"
        );
        assert!(measured.separate.is_empty());
        std::fs::set_permissions(root.join("closed"), std::fs::Permissions::from_mode(0o755))
            .expect("mode");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A file system mounted inside a folder is not walked and is listed. `/dev` has mounts inside it on most hosts
    /// (`/dev/pts`, `/dev/shm`); where it has none, there is nothing to check.
    #[test]
    fn a_separate_file_system_inside_a_folder_is_listed_and_not_walked() {
        use std::os::unix::fs::MetadataExt;
        let dev = |path: &str| std::fs::symlink_metadata(path).map(|meta| meta.dev()).ok();
        let mounted: Vec<&str> = ["/dev/pts", "/dev/shm", "/dev/mqueue"]
            .into_iter()
            .filter(|path| dev(path).is_some() && dev(path) != dev("/dev"))
            .collect();
        if mounted.is_empty() {
            eprintln!("nothing is mounted inside /dev here; nothing to check");
            return;
        }
        let measured = measure_folder("/dev");
        for path in mounted {
            assert!(
                measured.separate.iter().any(|found| found == path),
                "{path} is listed: {:?}",
                measured.separate
            );
        }
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
