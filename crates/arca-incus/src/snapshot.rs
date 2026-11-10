//! Owner-approved storage and snapshot writes, outside the desired-state journal.
//!
//! A snapshot is a copy of one app (an instance) or one data volume onto storage the owner plugged in. It is not a
//! desired-state entry: the owner approves each copy, and the copy is kept in its own project, [`SNAPSHOT_PROJECT`], on a pool
//! the owner added for snapshots. This module is the one narrow path that writes for it, and every request it sends passes
//! [`write_allowed`] first:
//!
//! - `POST /1.0/storage-pools` adds a snapshot pool. Incus itself formats a whole disk; this process never does.
//! - `POST /1.0/projects` creates [`SNAPSHOT_PROJECT`], and only that project.
//! - `POST` of an instance or a custom volume, and `DELETE` of one, only in [`SNAPSHOT_PROJECT`].
//! - One `PUT` of a new instance copy in [`SNAPSHOT_PROJECT`], which rewrites it to its clean config and devices.
//!
//! Nothing else is ever written: no `PATCH` of any kind, no instance state change (so a copy is never started), and no
//! request that names another project except as the read-only source of a copy. A copy's target pool must carry
//! [`SNAPSHOT_STORAGE_KEY`] in its config, which only a pool this module created has.
//!
//! A copy is made self-contained, so it can be restored from the disk alone after the machine is reinstalled: it uses no
//! profile (the source's expanded config and devices are written into the copy itself), its root disk is on the snapshot
//! pool, and every device that would tie it to the source project or to this host (a custom volume, a host folder, a GPU, a
//! proxy, a network that belongs to the source project) is left out and named in [`DROPPED_KEY`]. NIC addresses are left
//! out too, so the copy can never claim its source's address.
//!
//! Incus fills every config key and device a copy request leaves unset from the source's own (local) ones, so leaving a
//! device or a mark out does not drop it. The create request therefore masks each local key it drops with an empty value,
//! which Incus does not store, and each local device it drops with a `none` device. Once Incus has made the copy, one `PUT`
//! writes the clean config and devices (without the `none` masks), and the copy is read back: a copy that still carries a
//! source device, a live ownership mark, a profile or a start is deleted and the snapshot is refused.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::read::{Framed, IncusError, IncusRead, TIMEOUT, UnixIncus, exchange};

/// The project every snapshot copy lives in.
pub const SNAPSHOT_PROJECT: &str = "arcak-snapshots";
/// The description a pool added for snapshots carries, for people reading `incus storage list`. It is not the marker.
pub const SNAPSHOT_POOL_DESCRIPTION: &str = "Arca snapshot storage";
/// The config key a pool added for snapshots carries, set when the pool is created, with the pool's name as its value. A
/// copy is written only to a pool whose config carries it.
pub const SNAPSHOT_STORAGE_KEY: &str = "user.arcasnapshot.storage";
/// The filesystem label prefix of a whole disk added for snapshots: Incus labels a btrfs pool it formats with the pool's
/// name, and a whole-disk snapshot pool is named `arca-snapshots-<name>`, so the disk is known as snapshot storage even on a
/// machine that has no pool for it yet.
pub const SNAPSHOT_LABEL_PREFIX: &str = "arca-snapshots-";
/// The file system UUID of the disk a folder snapshot pool's folder is on, bound into the owner's approval and written
/// into the pool's config when it is created, so the disk is known as snapshot storage even while it is not mounted.
pub const FS_UUID_KEY: &str = "user.arcasnapshot.fs-uuid";
/// Written by the one clean-up `PUT` of an instance copy. A copy without it never finished its clean-up (the process
/// stopped, or the clean-up failed and the copy could not be deleted), and is listed as incomplete.
pub const VERIFIED_KEY: &str = "user.arcasnapshot.verified";
/// The folder a folder-backed snapshot pool lives in, under the folder the owner picked.
pub const POOL_DIR_NAME: &str = "arca-pool";
/// The namespace of a copy's own keys. It is outside every ownership namespace (`user.arcakernel.`, and droidd's
/// `user.droid.`, `user.tactu.` and `user.arca.`), so neither the kernel nor droidd ever takes a copy for its own.
pub const SNAPSHOT_KEY_PREFIX: &str = "user.arcasnapshot.";
/// The copy's source project.
pub const SOURCE_PROJECT_KEY: &str = "user.arcasnapshot.source-project";
/// The copy's source name: the instance or the volume.
pub const SOURCE_NAME_KEY: &str = "user.arcasnapshot.source-name";
/// What was copied: `container`, `virtual-machine` or `volume`.
pub const SOURCE_KIND_KEY: &str = "user.arcasnapshot.source-kind";
/// A volume copy's source pool.
pub const SOURCE_POOL_KEY: &str = "user.arcasnapshot.source-pool";
/// When the copy was made, in UTC.
pub const CREATED_KEY: &str = "user.arcasnapshot.created";
/// The source devices the copy leaves out, comma-separated.
pub const DROPPED_KEY: &str = "user.arcasnapshot.dropped-devices";
/// Where a source's ownership marks are kept: `user.arcasnapshot.mark.<original key>`. The copy carries no live mark, so
/// no owner acts on it, and a restore can still read who owned the source.
pub const MARK_PREFIX: &str = "user.arcasnapshot.mark.";
/// How long one wait request may hold.
const WAIT_STEP: Duration = Duration::from_secs(5);
/// How long a copy that is still running when the snapshot verb answers is followed before it is left as it is.
const FINISH_WAIT: Duration = Duration::from_secs(24 * 60 * 60);

/// Where a new snapshot pool lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PoolSource {
    /// A folder on a mounted disk: an Incus `dir` pool at `<path>/arca-pool`. `fs_uuid` is the UUID of the file system
    /// the folder is on, written as [`FS_UUID_KEY`].
    Folder {
        path: String,
        fs_uuid: Option<String>,
    },
    /// A whole block device, such as `/dev/sdb`: an Incus `btrfs` pool, which Incus formats.
    Device(String),
}

/// The body that creates a snapshot pool. A folder pool lives in [`POOL_DIR_NAME`] under the folder; a device pool is btrfs
/// with zstd compression, and Incus wipes the device's old partition table and filesystem signatures (`source.wipe`) before
/// it formats it, since `mkfs.btrfs` refuses a disk that still has them. Both carry [`SNAPSHOT_STORAGE_KEY`].
pub fn pool_body(name: &str, source: &PoolSource) -> Value {
    match source {
        PoolSource::Folder { path, fs_uuid } => {
            let mut config =
                json!({ "source": folder_pool_path(path), SNAPSHOT_STORAGE_KEY: name });
            if let Some(uuid) = fs_uuid {
                config[FS_UUID_KEY] = json!(uuid);
            }
            json!({
                "name": name,
                "driver": "dir",
                "description": SNAPSHOT_POOL_DESCRIPTION,
                "config": config,
            })
        }
        PoolSource::Device(device) => json!({
            "name": name,
            "driver": "btrfs",
            "description": SNAPSHOT_POOL_DESCRIPTION,
            "config": {
                "source": device,
                "source.wipe": "true",
                "btrfs.mount_options": "user_subvol_rm_allowed,compress=zstd",
                SNAPSHOT_STORAGE_KEY: name,
            },
        }),
    }
}

/// Whether a pool, as Incus answers it, is snapshot storage: its config carries [`SNAPSHOT_STORAGE_KEY`] with its own name.
pub fn is_snapshot_pool(pool: &Value) -> bool {
    let name = pool.get("name").and_then(Value::as_str);
    name.is_some()
        && pool
            .pointer(&format!(
                "/config/{}",
                SNAPSHOT_STORAGE_KEY.replace('/', "~1")
            ))
            .and_then(Value::as_str)
            == name
}

/// The folder a folder pool uses: [`POOL_DIR_NAME`] under `folder`.
pub fn folder_pool_path(folder: &str) -> String {
    format!("{}/{POOL_DIR_NAME}", folder.trim_end_matches('/'))
}

/// The body that creates [`SNAPSHOT_PROJECT`]. Networks are not the project's own, so a copy's NIC resolves the default
/// project's network by name; profiles, images and volumes are its own, so nothing in it depends on another project.
pub fn snapshot_project_body() -> Value {
    json!({
        "name": SNAPSHOT_PROJECT,
        "description": "Arca snapshots: copies of apps and data volumes. They never start on their own.",
        "config": {
            "features.networks": "false",
            "features.profiles": "true",
            "features.images": "true",
            "features.storage.volumes": "true",
        },
    })
}

/// Whether a config key is an ownership mark: the kernel's, or one of droidd's namespaces or bare namespace keys, matched
/// without regard to ASCII case as the classifier matches them.
pub fn ownership_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    lower.starts_with("user.arcakernel.")
        || arca_kernel::ownership::DROIDD_MARK_PREFIXES
            .iter()
            .any(|prefix| lower.starts_with(prefix) || lower == prefix.trim_end_matches('.'))
}

/// Copies the source's config keys into `config`. Volatile keys belong to the running source and the snapshot's own keys
/// are written fresh; an ownership mark is moved under [`MARK_PREFIX`].
fn copy_config(
    source: &Map<String, Value>,
    config: &mut Map<String, Value>,
    skip: &dyn Fn(&str) -> bool,
) {
    for (key, value) in source {
        let Some(text) = value.as_str() else {
            continue;
        };
        if key.starts_with("volatile.") || key.starts_with(SNAPSHOT_KEY_PREFIX) || skip(key) {
            continue;
        }
        if ownership_key(key) {
            config.insert(format!("{MARK_PREFIX}{key}"), json!(text));
        } else {
            config.insert(key.clone(), json!(text));
        }
    }
}

/// The body that copies one instance into [`SNAPSHOT_PROJECT`] on `pool`, and the names of the devices it left out.
///
/// `source` is the instance as `GET /1.0/instances/<name>?project=<project>` answers it. `project_networks` is whether the
/// source project has its own networks: a NIC on such a network would not resolve in the snapshot project and is left out.
/// The copy is stateless (`stateful: false`, `live: false`), carries only the instance and not its own snapshots, uses no
/// profile, sets `boot.autostart` to `false`, and is never started: the body carries no `start`.
pub fn instance_copy_body(
    source: &Value,
    project: &str,
    project_networks: bool,
    snapshot: &str,
    pool: &str,
    created: &str,
) -> Result<(Value, Vec<String>), String> {
    let name = source
        .get("name")
        .and_then(Value::as_str)
        .ok_or("the source instance has no name")?;
    let kind = source
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("container");
    let config_source = source
        .get("expanded_config")
        .or_else(|| source.get("config"))
        .and_then(Value::as_object);
    let mut config = Map::new();
    if let Some(found) = config_source {
        copy_config(found, &mut config, &|key| key == "boot.autostart");
    }
    config.insert("boot.autostart".to_string(), json!("false"));
    config.insert(SOURCE_PROJECT_KEY.to_string(), json!(project));
    config.insert(SOURCE_NAME_KEY.to_string(), json!(name));
    config.insert(SOURCE_KIND_KEY.to_string(), json!(kind));
    config.insert(CREATED_KEY.to_string(), json!(created));

    let devices_source = source
        .get("expanded_devices")
        .or_else(|| source.get("devices"))
        .and_then(Value::as_object);
    let mut devices = Map::new();
    let mut dropped = Vec::new();
    let mut root_seen = false;
    if let Some(found) = devices_source {
        for (device_name, device) in found {
            match copied_device(device, pool, project_networks) {
                Copied::Root(body) if !root_seen => {
                    root_seen = true;
                    devices.insert(device_name.clone(), body);
                }
                Copied::Keep(body) => {
                    devices.insert(device_name.clone(), body);
                }
                Copied::Root(_) | Copied::Drop => dropped.push(device_name.clone()),
            }
        }
    }
    if !root_seen {
        devices.insert(
            "root".to_string(),
            json!({ "type": "disk", "path": "/", "pool": pool }),
        );
    }
    if !dropped.is_empty() {
        config.insert(DROPPED_KEY.to_string(), json!(dropped.join(",")));
    }
    // Incus fills every key and device the request leaves unset from the source's own ones. Mask the ones the copy drops:
    // an empty value is not stored, and a `none` device stands in for a dropped one until the clean-up rewrites the copy.
    if let Some(local) = source.get("config").and_then(Value::as_object) {
        for key in local.keys() {
            if !key.starts_with("volatile.") && !config.contains_key(key) {
                config.insert(key.clone(), json!(""));
            }
        }
    }
    if let Some(local) = source.get("devices").and_then(Value::as_object) {
        for device_name in local.keys() {
            if !devices.contains_key(device_name) {
                devices.insert(device_name.clone(), json!({ "type": "none" }));
            }
        }
    }
    let body = json!({
        "name": snapshot,
        "type": kind,
        "description": format!("Snapshot of {name} from project {project}"),
        "stateful": false,
        "profiles": [],
        "config": Value::Object(config),
        "devices": Value::Object(devices),
        "source": {
            "type": "copy",
            "source": name,
            "project": project,
            "instance_only": true,
            "live": false,
        },
    });
    Ok((body, dropped))
}

enum Copied {
    Root(Value),
    Keep(Value),
    Drop,
}

/// What one source device becomes in the copy. The root disk moves to the snapshot pool; a NIC keeps its network unless that
/// network is the source project's own, and loses its fixed addresses; every other device is left out.
fn copied_device(device: &Value, pool: &str, project_networks: bool) -> Copied {
    let Some(object) = device.as_object() else {
        return Copied::Drop;
    };
    match object.get("type").and_then(Value::as_str) {
        Some("disk") if object.get("path").and_then(Value::as_str) == Some("/") => {
            let mut root = Map::new();
            root.insert("type".to_string(), json!("disk"));
            root.insert("path".to_string(), json!("/"));
            root.insert("pool".to_string(), json!(pool));
            for key in ["size", "size.state"] {
                if let Some(value) = object.get(key).and_then(Value::as_str) {
                    root.insert(key.to_string(), json!(value));
                }
            }
            Copied::Root(Value::Object(root))
        }
        Some("nic") => {
            if object.contains_key("network") && project_networks {
                return Copied::Drop;
            }
            let mut nic = Map::new();
            for (key, value) in object {
                let fixed = matches!(key.as_str(), "hwaddr" | "ipv4.address" | "ipv6.address");
                if !fixed && let Some(text) = value.as_str() {
                    nic.insert(key.clone(), json!(text));
                }
            }
            Copied::Keep(Value::Object(nic))
        }
        _ => Copied::Drop,
    }
}

/// The clean config and devices of a copy: the create body without its masks (empty values and `none` devices).
pub fn clean_parts(body: &Value) -> (Map<String, Value>, Map<String, Value>) {
    let config = body
        .get("config")
        .and_then(Value::as_object)
        .map(|config| {
            config
                .iter()
                .filter(|(_, value)| value.as_str().is_some_and(|text| !text.is_empty()))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default();
    let devices = body
        .get("devices")
        .and_then(Value::as_object)
        .map(|devices| {
            devices
                .iter()
                .filter(|(_, device)| device.get("type").and_then(Value::as_str) != Some("none"))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default();
    (config, devices)
}

/// What is wrong with an instance copy as Incus answers it, against the clean devices it should have: it runs or may start
/// on its own, uses a profile, carries a live ownership mark, a device it should not have (a source device, a mask), or a
/// root disk off the snapshot pool. An empty list is a clean copy.
pub fn copy_problems(copy: &Value, pool: &str, devices: &Map<String, Value>) -> Vec<String> {
    let mut problems = Vec::new();
    if copy.get("status").and_then(Value::as_str) == Some("Running") {
        problems.push("the copy is running".to_string());
    }
    if copy
        .pointer("/config/boot.autostart")
        .and_then(Value::as_str)
        != Some("false")
    {
        problems.push("the copy may start on its own".to_string());
    }
    if copy
        .get("profiles")
        .and_then(Value::as_array)
        .is_some_and(|profiles| !profiles.is_empty())
    {
        problems.push("the copy uses a profile".to_string());
    }
    if copy
        .pointer(&format!("/config/{VERIFIED_KEY}"))
        .and_then(Value::as_str)
        != Some("true")
    {
        problems.push("the copy's clean-up was not written".to_string());
    }
    for field in ["config", "expanded_config"] {
        if let Some(config) = copy.get(field).and_then(Value::as_object) {
            for key in config.keys() {
                if ownership_key(key) {
                    problems.push(format!("the copy carries the live mark {key}"));
                }
            }
        }
    }
    for field in ["devices", "expanded_devices"] {
        let Some(found) = copy.get(field).and_then(Value::as_object) else {
            continue;
        };
        for (name, device) in found {
            if devices.get(name) != Some(device) {
                problems.push(format!("the copy carries the device {name}"));
            }
        }
        for name in devices.keys() {
            if !found.contains_key(name) {
                problems.push(format!("the copy lacks the device {name}"));
            }
        }
    }
    let root_pool = copy
        .get("devices")
        .and_then(Value::as_object)
        .and_then(|found| {
            found
                .values()
                .find(|device| device.get("path").and_then(Value::as_str) == Some("/"))
        })
        .and_then(|root| root.get("pool"))
        .and_then(Value::as_str);
    if root_pool != Some(pool) {
        problems.push("the copy's root disk is not on the snapshot pool".to_string());
    }
    problems.sort();
    problems.dedup();
    problems
}

/// The live ownership marks a volume copy carries, as Incus answers it.
pub fn volume_problems(copy: &Value) -> Vec<String> {
    copy.get("config")
        .and_then(Value::as_object)
        .map(|config| {
            config
                .keys()
                .filter(|key| ownership_key(key))
                .map(|key| format!("the copy carries the live mark {key}"))
                .collect()
        })
        .unwrap_or_default()
}

/// The body that copies one custom volume into [`SNAPSHOT_PROJECT`]. `source` is the volume as Incus answers it. Only the
/// volume is copied, not its own snapshots. Driver-specific keys of the source pool are left behind.
pub fn volume_copy_body(
    source: &Value,
    project: &str,
    source_pool: &str,
    snapshot: &str,
    created: &str,
) -> Result<Value, String> {
    let name = source
        .get("name")
        .and_then(Value::as_str)
        .ok_or("the source volume has no name")?;
    let content_type = source
        .get("content_type")
        .and_then(Value::as_str)
        .unwrap_or("filesystem");
    let mut config = Map::new();
    if let Some(found) = source.get("config").and_then(Value::as_object) {
        // The source pool's driver keys do not apply on the snapshot pool.
        copy_config(found, &mut config, &|key| {
            ["block.", "btrfs.", "zfs.", "lvm.", "ceph", "cephfs."]
                .iter()
                .any(|prefix| key.starts_with(prefix))
        });
    }
    config.insert(SOURCE_PROJECT_KEY.to_string(), json!(project));
    config.insert(SOURCE_NAME_KEY.to_string(), json!(name));
    config.insert(SOURCE_KIND_KEY.to_string(), json!("volume"));
    config.insert(SOURCE_POOL_KEY.to_string(), json!(source_pool));
    config.insert(CREATED_KEY.to_string(), json!(created));
    Ok(json!({
        "name": snapshot,
        "type": "custom",
        "content_type": content_type,
        "description": format!("Snapshot of volume {name} from project {project}"),
        "config": Value::Object(config),
        "source": {
            "type": "copy",
            "name": name,
            "pool": source_pool,
            "project": project,
            "volume_only": true,
        },
    }))
}

/// Whether a request is one this module may send. Every GET may; a write must be one of the closed shapes listed in the
/// module documentation, with plain names, and every snapshot write names [`SNAPSHOT_PROJECT`] as its only query.
pub fn write_allowed(method: &str, target: &str) -> bool {
    if method == "GET" {
        return true;
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let snapshot_query = query == format!("project={SNAPSHOT_PROJECT}");
    let segments: Vec<&str> = path.split('/').collect();
    if segments.first() != Some(&"") || segments.get(1) != Some(&"1.0") {
        return false;
    }
    let rest = &segments[2..];
    if !rest.iter().all(|segment| plain_name(segment)) {
        return false;
    }
    match (method, rest) {
        ("POST", ["storage-pools"]) | ("POST", ["projects"]) => query.is_empty(),
        ("POST", ["instances"]) => snapshot_query,
        ("PUT", ["instances", _]) => snapshot_query,
        ("DELETE", ["instances", _]) => snapshot_query,
        ("POST", ["storage-pools", _, "volumes", "custom"]) => snapshot_query,
        ("DELETE", ["storage-pools", _, "volumes", "custom", _]) => snapshot_query,
        _ => false,
    }
}

/// How far a started copy or delete got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    /// Incus finished it.
    Done,
    /// Incus is still working on it; the operation id says which.
    Running(String),
}

/// Told why a copy finished in the background failed, once, so the failure is journalled: it has no caller left to answer.
pub type LateReport = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

/// The narrow writer for snapshot storage and copies.
#[derive(Debug, Clone)]
pub struct SnapshotWriter {
    socket: PathBuf,
    timeout: Duration,
    /// How long a copy or a delete is waited on before it is reported as still running.
    wait: Duration,
}

impl SnapshotWriter {
    pub fn new(socket: PathBuf) -> Self {
        Self {
            socket,
            timeout: TIMEOUT,
            wait: Duration::from_secs(20),
        }
    }

    /// The same writer, waiting at most `wait` for a copy or a delete to finish.
    pub fn with_wait(mut self, wait: Duration) -> Self {
        self.wait = wait;
        self
    }

    fn reader(&self) -> UnixIncus {
        UnixIncus::new(self.socket.clone()).with_timeout(self.timeout)
    }

    /// Reads one path.
    pub fn get(&self, path: &str) -> Result<Value, IncusError> {
        self.reader().get(path)
    }

    /// Adds a snapshot pool. Incus makes the pool in the folder (which must already be there and be empty), or wipes and
    /// formats the whole device.
    pub fn create_pool(&self, name: &str, source: &PoolSource) -> Result<(), IncusError> {
        if !plain_name(name) {
            return Err(refused("the pool name is not plain"));
        }
        self.send_write("POST", "/1.0/storage-pools", &pool_body(name, source))?;
        let pool = self.get(&format!("/1.0/storage-pools/{name}"))?;
        if !is_snapshot_pool(&pool) {
            return Err(decode(
                "the new pool does not read back as snapshot storage",
            ));
        }
        Ok(())
    }

    /// Reads a pool and refuses one that is not snapshot storage.
    pub fn snapshot_pool(&self, pool: &str) -> Result<Value, IncusError> {
        if !plain_name(pool) {
            return Err(refused("the pool name is not plain"));
        }
        let found = self.get(&format!("/1.0/storage-pools/{pool}"))?;
        if !is_snapshot_pool(&found) {
            return Err(refused("the pool was not added as snapshot storage"));
        }
        Ok(found)
    }

    /// Creates [`SNAPSHOT_PROJECT`] when it is missing. Returns whether it was created. An existing project with its own
    /// networks is refused, since a copy's NIC would then not resolve the default project's network.
    pub fn ensure_project(&self) -> Result<bool, IncusError> {
        match self.get(&format!("/1.0/projects/{SNAPSHOT_PROJECT}")) {
            Ok(project) => {
                let networks = project
                    .pointer("/config/features.networks")
                    .and_then(Value::as_str);
                if networks == Some("true") {
                    return Err(refused(
                        "the snapshot project has its own networks; it must use the default project's",
                    ));
                }
                Ok(false)
            }
            Err(IncusError::Status { code: 404, .. }) => {
                self.send_write("POST", "/1.0/projects", &snapshot_project_body())?;
                Ok(true)
            }
            Err(error) => Err(error),
        }
    }

    /// Copies one instance into the snapshot project on `pool`, with a body from [`instance_copy_body`]. Once Incus has made
    /// the copy, one `PUT` rewrites it to its clean config and devices and it is read back; a copy that is not clean is
    /// deleted and refused. A copy still running when the wait ends is followed in the background and finished the same way.
    pub fn copy_instance(&self, pool: &str, body: &Value) -> Result<Progress, IncusError> {
        self.copy_instance_reporting(pool, body, std::sync::Arc::new(|_: &str| {}))
    }

    /// [`Self::copy_instance`], telling `late` why a copy finished in the background failed.
    pub fn copy_instance_reporting(
        &self,
        pool: &str,
        body: &Value,
        late: LateReport,
    ) -> Result<Progress, IncusError> {
        self.snapshot_pool(pool)?;
        if body
            .get("start")
            .is_some_and(|start| start != &json!(false))
        {
            return Err(refused("a snapshot copy is never started"));
        }
        let name = body_name(body)?;
        let path = format!("/1.0/instances?project={SNAPSHOT_PROJECT}");
        let progress = self.write_and_wait("POST", &path, Some(body))?;
        match &progress {
            Progress::Done => self.finish_instance(&name, pool, body)?,
            Progress::Running(operation) => {
                let writer = self.clone();
                let (operation, pool, body) = (operation.clone(), pool.to_string(), body.clone());
                std::thread::spawn(move || {
                    let followed = writer.clone().with_wait(FINISH_WAIT);
                    let outcome = match followed.wait_for(&operation) {
                        Ok(Progress::Done) => writer.finish_instance(&name, &pool, &body),
                        Ok(Progress::Running(_)) => Err(decode("the copy did not finish in a day")),
                        Err(error) => Err(error),
                    };
                    if let Err(error) = outcome {
                        late(&format!(
                            "the snapshot copy {name} failed in the background: {error}"
                        ));
                    }
                });
            }
        }
        Ok(progress)
    }

    /// Rewrites a new copy to its clean config and devices with one `PUT`, then reads it back. Anything left of the source
    /// (a device, a live mark, a profile) or a start deletes the copy and refuses the snapshot.
    fn finish_instance(&self, name: &str, pool: &str, body: &Value) -> Result<(), IncusError> {
        let path = format!("/1.0/instances/{name}?project={SNAPSHOT_PROJECT}");
        let (mut config, devices) = clean_parts(body);
        let cleaned = (|| {
            let copy = self.get(&path)?;
            if copy.get("status").and_then(Value::as_str) == Some("Running") {
                return Err(decode("the snapshot copy reads back as running"));
            }
            config.insert(VERIFIED_KEY.to_string(), json!("true"));
            // Incus keeps a copy's volatile keys and refuses a rewrite that drops its idmap ones.
            if let Some(found) = copy.get("config").and_then(Value::as_object) {
                for (key, value) in found {
                    if key.starts_with("volatile.") {
                        config.insert(key.clone(), value.clone());
                    }
                }
            }
            let put = json!({
                "architecture": copy.get("architecture").cloned().unwrap_or(json!("")),
                "config": Value::Object(config),
                "devices": Value::Object(devices.clone()),
                "profiles": [],
                "ephemeral": false,
                "stateful": false,
                "description": body.get("description").cloned().unwrap_or(json!("")),
            });
            if self.write_and_wait("PUT", &path, Some(&put))? != Progress::Done {
                return Err(decode("the clean-up of the snapshot copy did not finish"));
            }
            let copy = self.get(&path)?;
            let problems = copy_problems(&copy, pool, &devices);
            if !problems.is_empty() {
                return Err(decode(&problems.join("; ")));
            }
            Ok(())
        })();
        cleaned.map_err(|error| {
            let deleted = self.write_and_wait("DELETE", &path, None).is_ok();
            decode(&format!(
                "the snapshot copy was not clean ({error}); {}",
                if deleted {
                    "the copy was deleted"
                } else {
                    "deleting the copy failed too"
                }
            ))
        })
    }

    /// Copies one custom volume into the snapshot project on `pool`, with a body from [`volume_copy_body`].
    /// The copy is read back once Incus finishes; one that carries a live ownership mark is deleted and refused.
    pub fn copy_volume(&self, pool: &str, body: &Value) -> Result<Progress, IncusError> {
        self.copy_volume_reporting(pool, body, std::sync::Arc::new(|_: &str| {}))
    }

    /// [`Self::copy_volume`], telling `late` why a copy finished in the background failed.
    pub fn copy_volume_reporting(
        &self,
        pool: &str,
        body: &Value,
        late: LateReport,
    ) -> Result<Progress, IncusError> {
        self.snapshot_pool(pool)?;
        let name = body_name(body)?;
        let path = format!("/1.0/storage-pools/{pool}/volumes/custom?project={SNAPSHOT_PROJECT}");
        let progress = self.write_and_wait("POST", &path, Some(body))?;
        match &progress {
            Progress::Done => self.finish_volume(pool, &name)?,
            Progress::Running(operation) => {
                let writer = self.clone();
                let (operation, pool) = (operation.clone(), pool.to_string());
                std::thread::spawn(move || {
                    let followed = writer.clone().with_wait(FINISH_WAIT);
                    let outcome = match followed.wait_for(&operation) {
                        Ok(Progress::Done) => writer.finish_volume(&pool, &name),
                        Ok(Progress::Running(_)) => Err(decode("the copy did not finish in a day")),
                        Err(error) => Err(error),
                    };
                    if let Err(error) = outcome {
                        late(&format!(
                            "the snapshot copy {name} failed in the background: {error}"
                        ));
                    }
                });
            }
        }
        Ok(progress)
    }

    fn finish_volume(&self, pool: &str, name: &str) -> Result<(), IncusError> {
        let path =
            format!("/1.0/storage-pools/{pool}/volumes/custom/{name}?project={SNAPSHOT_PROJECT}");
        let problems = volume_problems(&self.get(&path)?);
        if problems.is_empty() {
            return Ok(());
        }
        let deleted = self.write_and_wait("DELETE", &path, None).is_ok();
        Err(decode(&format!(
            "the snapshot copy was not clean ({}); {}",
            problems.join("; "),
            if deleted {
                "the copy was deleted"
            } else {
                "deleting the copy failed too"
            }
        )))
    }

    /// Deletes one snapshot copy of an instance. A running copy is refused: Arca never started it.
    pub fn delete_instance(&self, name: &str) -> Result<Progress, IncusError> {
        if !plain_name(name) {
            return Err(refused("the snapshot name is not plain"));
        }
        let path = format!("/1.0/instances/{name}?project={SNAPSHOT_PROJECT}");
        let copy = self.get(&path)?;
        if copy.get("status").and_then(Value::as_str) == Some("Running") {
            return Err(refused(
                "the snapshot copy is running; stop it before deleting it",
            ));
        }
        self.write_and_wait("DELETE", &path, None)
    }

    /// Deletes one snapshot copy of a volume, on a snapshot pool.
    pub fn delete_volume(&self, pool: &str, name: &str) -> Result<Progress, IncusError> {
        self.snapshot_pool(pool)?;
        if !plain_name(name) {
            return Err(refused("the snapshot name is not plain"));
        }
        let path =
            format!("/1.0/storage-pools/{pool}/volumes/custom/{name}?project={SNAPSHOT_PROJECT}");
        self.get(&path)?;
        self.write_and_wait("DELETE", &path, None)
    }

    fn write_and_wait(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Progress, IncusError> {
        let envelope = self.send(method, path, body)?;
        if envelope.get("type").and_then(Value::as_str) != Some("async") {
            return Ok(Progress::Done);
        }
        let operation = envelope
            .get("operation")
            .and_then(Value::as_str)
            .and_then(|url| url.rsplit('/').next())
            .filter(|id| plain_name(id))
            .ok_or_else(|| decode("the answer names no operation"))?
            .to_string();
        self.wait_for(&operation)
    }

    /// Waits on one operation for at most the writer's wait, in short steps. A failed operation is a status error with
    /// Incus's own message.
    fn wait_for(&self, operation: &str) -> Result<Progress, IncusError> {
        let until = Instant::now() + self.wait;
        let reader = self.reader();
        loop {
            let left = until.saturating_duration_since(Instant::now());
            let step = left.min(WAIT_STEP);
            let seconds = step.as_secs();
            let path = format!("/1.0/operations/{operation}/wait?timeout={seconds}");
            let answer = match reader.get_within(&path, step + TIMEOUT) {
                Ok(answer) => answer,
                Err(IncusError::Status { code: 404, .. }) => return Ok(Progress::Done),
                Err(error) => return Err(error),
            };
            match answer.get("status_code").and_then(Value::as_u64) {
                Some(200) => return Ok(Progress::Done),
                Some(code) if code >= 400 => {
                    let message = answer
                        .get("err")
                        .and_then(Value::as_str)
                        .filter(|text| !text.is_empty())
                        .unwrap_or("the Incus operation failed");
                    return Err(IncusError::Status {
                        code: u16::try_from(code).unwrap_or(400),
                        message: message.chars().take(512).collect(),
                    });
                }
                _ => {}
            }
            if left.is_zero() || seconds == 0 {
                return Ok(Progress::Running(operation.to_string()));
            }
        }
    }

    fn send_write(&self, method: &str, path: &str, body: &Value) -> Result<(), IncusError> {
        self.send(method, path, Some(body)).map(|_| ())
    }

    fn send(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, IncusError> {
        if !write_allowed(method, path) {
            return Err(refused("the request is outside the snapshot family"));
        }
        let bytes = body
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|_| decode("the request body does not serialize"))?;
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: incus\r\nAccept: application/json\r\nConnection: close\r\n"
        );
        if let Some(bytes) = &bytes {
            head.push_str("Content-Type: application/json\r\n");
            head.push_str(&format!("Content-Length: {}\r\n", bytes.len()));
        }
        head.push_str("\r\n");
        let mut request = head.into_bytes();
        if let Some(bytes) = bytes {
            request.extend_from_slice(&bytes);
        }
        let framed = exchange(&self.socket, self.timeout, &request)?;
        envelope_of(framed)
    }
}

fn body_name(body: &Value) -> Result<String, IncusError> {
    body.get("name")
        .and_then(Value::as_str)
        .filter(|name| plain_name(name))
        .map(str::to_string)
        .ok_or_else(|| refused("the snapshot name is not plain"))
}

fn envelope_of(framed: Framed) -> Result<Value, IncusError> {
    crate::read::checked_envelope(&framed.into_raw())
}

/// A plain Incus name segment: letters, digits, `-`, `_` and `.`, and not `.` or `..`.
pub fn plain_name(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 128
        && text != "."
        && text != ".."
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn refused(reason: &str) -> IncusError {
    IncusError::Refused(reason.to_string())
}

fn decode(reason: &str) -> IncusError {
    IncusError::Decode(reason.to_string())
}

/// The devices of a copy that would tie it to its source project: a profile, a custom volume, or a network the source project
/// owns. An empty list is a copy that can be restored from its pool alone.
pub fn source_dependencies(copy: &Value, source_project: &str) -> Vec<String> {
    let mut found = Vec::new();
    if copy
        .get("profiles")
        .and_then(Value::as_array)
        .is_some_and(|profiles| !profiles.is_empty())
    {
        found.push("profiles".to_string());
    }
    let devices: BTreeMap<String, Value> = copy
        .get("devices")
        .and_then(Value::as_object)
        .map(|devices| devices.clone().into_iter().collect())
        .unwrap_or_default();
    for (name, device) in devices {
        let kind = device.get("type").and_then(Value::as_str).unwrap_or("");
        let custom_volume = kind == "disk"
            && device.get("path").and_then(Value::as_str) != Some("/")
            && device.get("pool").is_some();
        let host_path = kind == "disk"
            && device
                .get("source")
                .and_then(Value::as_str)
                .is_some_and(|source| source.starts_with('/'));
        let project_value = device.as_object().is_some_and(|map| {
            map.values()
                .any(|value| value.as_str() == Some(source_project))
        });
        if custom_volume || host_path || project_value {
            found.push(name);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> Value {
        json!({
            "name": "web",
            "type": "container",
            "profiles": ["default", "apps"],
            "config": { "volatile.uuid": "u-1", "user.note": "own" },
            "expanded_config": {
                "volatile.uuid": "u-1",
                "volatile.eth0.hwaddr": "00:16:3e:00:00:01",
                "boot.autostart": "true",
                "limits.cpu": "2",
                "user.note": "own"
            },
            "expanded_devices": {
                "root": { "type": "disk", "path": "/", "pool": "default", "size": "20GiB" },
                "eth0": { "type": "nic", "network": "incusbr0", "name": "eth0", "ipv4.address": "192.0.2.10", "hwaddr": "00:16:3e:00:00:01" },
                "data": { "type": "disk", "path": "/srv", "pool": "default", "source": "web-data" },
                "share": { "type": "disk", "path": "/mnt/share", "source": "/srv/share" },
                "gpu0": { "type": "gpu", "pci": "0000:01:00.0" },
                "http": { "type": "proxy", "listen": "tcp:0.0.0.0:80", "connect": "tcp:127.0.0.1:80" }
            }
        })
    }

    #[test]
    fn an_instance_copy_is_self_contained_and_never_starts() {
        let (body, dropped) = instance_copy_body(
            &source(),
            "shop",
            false,
            "shop--web--20261109T1405",
            "usb1",
            "2026-11-09T14:05:00.000Z",
        )
        .expect("a body");
        assert_eq!(body["profiles"], json!([]));
        assert_eq!(body["stateful"], json!(false));
        assert!(body.get("start").is_none(), "the copy is never started");
        assert_eq!(body["config"]["boot.autostart"], json!("false"));
        assert_eq!(
            body["config"]["limits.cpu"],
            json!("2"),
            "profile config is carried in"
        );
        assert!(body["config"].get("volatile.uuid").is_none());
        assert_eq!(body["config"][SOURCE_PROJECT_KEY], json!("shop"));
        assert_eq!(body["config"][SOURCE_NAME_KEY], json!("web"));
        assert_eq!(
            body["devices"]["root"],
            json!({ "type": "disk", "path": "/", "pool": "usb1", "size": "20GiB" })
        );
        assert_eq!(
            body["devices"]["eth0"],
            json!({ "type": "nic", "network": "incusbr0", "name": "eth0" })
        );
        assert_eq!(dropped, vec!["data", "gpu0", "http", "share"]);
        assert_eq!(body["config"][DROPPED_KEY], json!("data,gpu0,http,share"));
        assert_eq!(body["source"]["project"], json!("shop"));
        assert_eq!(body["source"]["instance_only"], json!(true));
        assert_eq!(body["source"]["live"], json!(false));
        assert!(
            source_dependencies(&body, "shop").is_empty(),
            "no dependency on the source project"
        );
    }

    #[test]
    fn ownership_marks_never_travel_live_with_a_copy() {
        let mut source = source();
        source["expanded_config"]["user.arcakernel.entry"] = json!("service-web");
        source["expanded_config"]["user.arca.template"] = json!("nginx");
        source["expanded_config"]["USER.Droid.owner"] = json!("x");
        source["expanded_config"]["user.tactu"] = json!("y");
        let (body, _) =
            instance_copy_body(&source, "shop", false, "s", "usb1", "t").expect("a body");
        let config = body["config"].as_object().expect("config");
        for key in config.keys() {
            assert!(!ownership_key(key), "{key} is an ownership mark");
        }
        assert_eq!(
            config[&format!("{MARK_PREFIX}user.arcakernel.entry")],
            json!("service-web")
        );
        assert_eq!(
            config[&format!("{MARK_PREFIX}user.arca.template")],
            json!("nginx")
        );
        assert!(!ownership_key(SOURCE_PROJECT_KEY));
        assert!(!ownership_key(MARK_PREFIX));
        let volume = json!({ "name": "v", "config": { "user.arcakernel.entry": "pg-orders" } });
        let body = volume_copy_body(&volume, "shop", "fast", "s", "t").expect("a body");
        assert!(
            body["config"]
                .as_object()
                .expect("config")
                .keys()
                .all(|key| !ownership_key(key))
        );
    }

    /// A source as Incus answers it with its own (local) devices and marks, which Incus would carry into any copy whose
    /// request leaves them unset.
    fn source_with_local_devices() -> Value {
        let mut source = source();
        source["config"] = json!({
            "volatile.uuid": "u-1", "user.note": "own", "boot.autostart": "true",
            "user.arca.app": "web", "user.arcakernel.entry": "service-web", "user.droid.slot": "7",
        });
        for (key, value) in source["config"].clone().as_object().expect("config") {
            source["expanded_config"][key] = value.clone();
        }
        source["devices"] = json!({
            "data": { "type": "disk", "path": "/srv", "pool": "default", "source": "web-data" },
            "share": { "type": "disk", "path": "/mnt/share", "source": "/srv/share" },
            "http": { "type": "proxy", "listen": "tcp:0.0.0.0:80", "connect": "tcp:127.0.0.1:80" }
        });
        source
    }

    #[test]
    fn the_create_masks_every_local_key_and_device_the_copy_drops() {
        let (body, _) = instance_copy_body(
            &source_with_local_devices(),
            "shop",
            false,
            "s",
            "usb1",
            "t",
        )
        .expect("a body");
        for key in ["user.arca.app", "user.arcakernel.entry", "user.droid.slot"] {
            assert_eq!(body["config"][key], json!(""), "{key} is masked");
            assert!(body["config"][format!("{MARK_PREFIX}{key}")].is_string());
        }
        assert!(
            body["config"].get("volatile.uuid").is_none(),
            "Incus never copies it"
        );
        for device in ["data", "share", "http"] {
            assert_eq!(
                body["devices"][device],
                json!({ "type": "none" }),
                "{device}"
            );
        }
        let (config, devices) = clean_parts(&body);
        assert!(config.keys().all(|key| !ownership_key(key)));
        assert!(config.values().all(|value| value != ""));
        assert_eq!(
            devices.keys().cloned().collect::<Vec<_>>(),
            vec!["eth0".to_string(), "root".to_string()]
        );
    }

    #[test]
    fn a_copy_with_anything_left_of_its_source_is_not_clean() {
        let (body, _) = instance_copy_body(
            &source_with_local_devices(),
            "shop",
            false,
            "s",
            "usb1",
            "t",
        )
        .expect("a body");
        let (mut config, devices) = clean_parts(&body);
        config.insert(VERIFIED_KEY.to_string(), json!("true"));
        let clean = json!({
            "status": "Stopped", "profiles": [],
            "config": config, "expanded_config": config,
            "devices": devices, "expanded_devices": devices,
        });
        assert_eq!(
            copy_problems(&clean, "usb1", &devices),
            Vec::<String>::new()
        );
        let mut merged = clean.clone();
        merged["config"]["user.arca.app"] = json!("web");
        merged["devices"]["http"] = json!({ "type": "proxy", "listen": "tcp:0.0.0.0:80" });
        merged["expanded_devices"]["data"] = json!({ "type": "none" });
        let problems = copy_problems(&merged, "usb1", &devices);
        assert!(
            problems
                .iter()
                .any(|p| p.contains("live mark user.arca.app")),
            "{problems:?}"
        );
        assert!(
            problems.iter().any(|p| p.contains("device http")),
            "{problems:?}"
        );
        assert!(
            problems.iter().any(|p| p.contains("device data")),
            "{problems:?}"
        );
        let mut unverified = clean.clone();
        unverified["config"]
            .as_object_mut()
            .expect("config")
            .remove(VERIFIED_KEY);
        assert_eq!(
            copy_problems(&unverified, "usb1", &devices),
            vec!["the copy's clean-up was not written".to_string()]
        );
        let mut started = clean.clone();
        started["status"] = json!("Running");
        started["config"]["boot.autostart"] = json!("true");
        assert_eq!(copy_problems(&started, "usb1", &devices).len(), 2);
        assert!(
            !copy_problems(&clean, "fast", &devices).is_empty(),
            "root off the snapshot pool"
        );
        assert_eq!(
            volume_problems(&json!({ "config": { "user.droid.slot": "7", "size": "1GiB" } })).len(),
            1
        );
    }

    #[test]
    fn a_nic_on_the_source_projects_own_network_is_left_out() {
        let (body, dropped) =
            instance_copy_body(&source(), "shop", true, "s", "usb1", "t").expect("a body");
        assert!(body["devices"].get("eth0").is_none());
        assert!(dropped.contains(&"eth0".to_string()));
    }

    #[test]
    fn a_volume_copy_names_its_source_and_leaves_driver_keys_behind() {
        let volume = json!({
            "name": "photos", "content_type": "filesystem",
            "config": { "size": "50GiB", "zfs.remove_snapshots": "true", "volatile.idmap.last": "x" }
        });
        let body = volume_copy_body(&volume, "shop", "fast", "shop--photos--20261109T1405", "t")
            .expect("a body");
        assert_eq!(body["config"]["size"], json!("50GiB"));
        assert!(body["config"].get("zfs.remove_snapshots").is_none());
        assert!(body["config"].get("volatile.idmap.last").is_none());
        assert_eq!(
            body["source"],
            json!({
                "type": "copy", "name": "photos", "pool": "fast", "project": "shop", "volume_only": true
            })
        );
        assert_eq!(body["config"][SOURCE_POOL_KEY], json!("fast"));
    }

    #[test]
    fn the_write_family_is_closed() {
        let p = SNAPSHOT_PROJECT;
        for (method, target) in [
            ("POST", "/1.0/storage-pools".to_string()),
            ("POST", "/1.0/projects".to_string()),
            ("POST", format!("/1.0/instances?project={p}")),
            (
                "DELETE",
                format!("/1.0/instances/a--b--20261109T1405?project={p}"),
            ),
            (
                "POST",
                format!("/1.0/storage-pools/usb1/volumes/custom?project={p}"),
            ),
            (
                "DELETE",
                format!("/1.0/storage-pools/usb1/volumes/custom/x?project={p}"),
            ),
            (
                "PUT",
                format!("/1.0/instances/a--b--20261109T1405?project={p}"),
            ),
            ("GET", "/1.0/instances/web?project=shop".to_string()),
        ] {
            assert!(write_allowed(method, &target), "{method} {target}");
        }
        for (method, target) in [
            ("POST", "/1.0/instances?project=shop"),
            ("POST", "/1.0/instances"),
            ("DELETE", "/1.0/instances/web?project=shop"),
            ("DELETE", "/1.0/instances/web"),
            ("PUT", "/1.0/instances/web?project=shop"),
            ("PUT", "/1.0/instances/web"),
            ("PUT", "/1.0/instances/a/state?project=arcak-snapshots"),
            (
                "PUT",
                "/1.0/storage-pools/usb1/volumes/custom/x?project=arcak-snapshots",
            ),
            ("PATCH", "/1.0/instances/a?project=arcak-snapshots"),
            ("POST", "/1.0/instances/a?project=arcak-snapshots"),
            ("DELETE", "/1.0/storage-pools/usb1"),
            ("DELETE", "/1.0/projects/arcak-snapshots"),
            ("PUT", "/1.0/storage-pools/usb1"),
            (
                "POST",
                "/1.0/storage-pools/usb1/volumes/custom?project=shop",
            ),
            (
                "DELETE",
                "/1.0/storage-pools/usb1/volumes/custom/x?project=shop",
            ),
            ("POST", "/1.0/storage-pools?project=shop"),
            ("DELETE", "/1.0/instances/..?project=arcak-snapshots"),
            ("DELETE", "/1.0/instances/a%2fb?project=arcak-snapshots"),
            ("POST", "/1.0/instances?project=arcak-snapshots&target=x"),
        ] {
            assert!(!write_allowed(method, target), "{method} {target}");
        }
    }

    #[test]
    fn pool_bodies_say_what_incus_will_do() {
        let folder = pool_body(
            "usb1",
            &PoolSource::Folder {
                path: "/media/usb/".to_string(),
                fs_uuid: Some("uuid-usb".to_string()),
            },
        );
        assert_eq!(folder["driver"], json!("dir"));
        assert_eq!(folder["config"]["source"], json!("/media/usb/arca-pool"));
        assert_eq!(folder["config"][FS_UUID_KEY], json!("uuid-usb"));
        let device = pool_body("usb2", &PoolSource::Device("/dev/sdb".to_string()));
        assert_eq!(device["driver"], json!("btrfs"));
        assert_eq!(device["config"]["source"], json!("/dev/sdb"));
        assert!(
            device["config"]["btrfs.mount_options"]
                .as_str()
                .unwrap_or("")
                .contains("compress=zstd")
        );
        assert_eq!(device["description"], json!(SNAPSHOT_POOL_DESCRIPTION));
        assert_eq!(
            device["config"]["source.wipe"],
            json!("true"),
            "mkfs.btrfs refuses a disk with an old partition table"
        );
        assert!(folder["config"].get("source.wipe").is_none());
        for (name, body) in [("usb1", &folder), ("usb2", &device)] {
            assert_eq!(body["config"][SNAPSHOT_STORAGE_KEY], json!(name));
            assert!(is_snapshot_pool(body));
        }
        let mut renamed = device.clone();
        renamed["name"] = json!("other");
        assert!(!is_snapshot_pool(&renamed), "the marker names its own pool");
        assert!(!is_snapshot_pool(
            &json!({ "name": "usb3", "description": SNAPSHOT_POOL_DESCRIPTION, "config": {} })
        ));
        assert_eq!(
            snapshot_project_body()["config"]["features.networks"],
            json!("false")
        );
    }
}
