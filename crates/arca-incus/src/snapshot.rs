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
//!
//! Nothing else is ever written: no `PUT` or `PATCH` of any kind, no instance state change (so a copy is never started),
//! and no request that names another project except as the read-only source of a copy. A copy's target pool must carry
//! [`SNAPSHOT_POOL_DESCRIPTION`], which only a pool this module created has.
//!
//! A copy is made self-contained, so it can be restored from the disk alone after the machine is reinstalled: it uses no
//! profile (the source's expanded config and devices are written into the copy itself), its root disk is on the snapshot
//! pool, and every device that would tie it to the source project or to this host (a custom volume, a host folder, a GPU, a
//! proxy, a network that belongs to the source project) is left out and named in [`DROPPED_KEY`]. NIC addresses are left
//! out too, so the copy can never claim its source's address.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::read::{Framed, IncusError, IncusRead, TIMEOUT, UnixIncus, exchange};

/// The project every snapshot copy lives in.
pub const SNAPSHOT_PROJECT: &str = "arcak-snapshots";
/// The description a pool added for snapshots carries. A copy is written only to a pool with exactly this description.
pub const SNAPSHOT_POOL_DESCRIPTION: &str = "Arca snapshot storage";
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

/// Where a new snapshot pool lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PoolSource {
    /// A folder on a mounted disk: an Incus `dir` pool at `<folder>/arca-pool`.
    Folder(String),
    /// A whole block device, such as `/dev/sdb`: an Incus `btrfs` pool, which Incus formats.
    Device(String),
}

/// The body that creates a snapshot pool. A folder pool lives in [`POOL_DIR_NAME`] under the folder; a device pool is btrfs
/// with zstd compression.
pub fn pool_body(name: &str, source: &PoolSource) -> Value {
    match source {
        PoolSource::Folder(folder) => json!({
            "name": name,
            "driver": "dir",
            "description": SNAPSHOT_POOL_DESCRIPTION,
            "config": { "source": folder_pool_path(folder) },
        }),
        PoolSource::Device(device) => json!({
            "name": name,
            "driver": "btrfs",
            "description": SNAPSHOT_POOL_DESCRIPTION,
            "config": {
                "source": device,
                "btrfs.mount_options": "user_subvol_rm_allowed,compress=zstd",
            },
        }),
    }
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

    /// Adds a snapshot pool. Incus creates the folder, or formats the whole device; this process touches no disk.
    pub fn create_pool(&self, name: &str, source: &PoolSource) -> Result<(), IncusError> {
        if !plain_name(name) {
            return Err(refused("the pool name is not plain"));
        }
        self.send_write("POST", "/1.0/storage-pools", &pool_body(name, source))?;
        let pool = self.get(&format!("/1.0/storage-pools/{name}"))?;
        if pool.get("description").and_then(Value::as_str) != Some(SNAPSHOT_POOL_DESCRIPTION) {
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
        if found.get("description").and_then(Value::as_str) != Some(SNAPSHOT_POOL_DESCRIPTION) {
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

    /// Copies one instance into the snapshot project on `pool`, with a body from [`instance_copy_body`]. The copy is read
    /// back once Incus finishes: it must be stopped and must not start on its own.
    pub fn copy_instance(&self, pool: &str, body: &Value) -> Result<Progress, IncusError> {
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
        if progress == Progress::Done {
            let copy = self.get(&format!("/1.0/instances/{name}?project={SNAPSHOT_PROJECT}"))?;
            if copy.get("status").and_then(Value::as_str) == Some("Running") {
                return Err(decode("the snapshot copy reads back as running"));
            }
            if copy
                .pointer("/config/boot.autostart")
                .and_then(Value::as_str)
                != Some("false")
            {
                return Err(decode(
                    "the snapshot copy reads back without boot.autostart=false",
                ));
            }
        }
        Ok(progress)
    }

    /// Copies one custom volume into the snapshot project on `pool`, with a body from [`volume_copy_body`].
    pub fn copy_volume(&self, pool: &str, body: &Value) -> Result<Progress, IncusError> {
        self.snapshot_pool(pool)?;
        body_name(body)?;
        let path = format!("/1.0/storage-pools/{pool}/volumes/custom?project={SNAPSHOT_PROJECT}");
        self.write_and_wait("POST", &path, Some(body))
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
            ("GET", "/1.0/instances/web?project=shop".to_string()),
        ] {
            assert!(write_allowed(method, &target), "{method} {target}");
        }
        for (method, target) in [
            ("POST", "/1.0/instances?project=shop"),
            ("POST", "/1.0/instances"),
            ("DELETE", "/1.0/instances/web?project=shop"),
            ("DELETE", "/1.0/instances/web"),
            ("PUT", "/1.0/instances/a?project=arcak-snapshots"),
            ("PUT", "/1.0/instances/a/state?project=arcak-snapshots"),
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
        let folder = pool_body("usb1", &PoolSource::Folder("/media/usb/".to_string()));
        assert_eq!(folder["driver"], json!("dir"));
        assert_eq!(folder["config"]["source"], json!("/media/usb/arca-pool"));
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
            snapshot_project_body()["config"]["features.networks"],
            json!("false")
        );
    }
}
