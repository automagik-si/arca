//! The move writer: what `instance_move` asks of two Incus daemons, and nothing else.
//!
//! Incus does the copy. The receiving side (the target) is asked to create an instance or a custom volume whose source is
//! a migration in `push` mode, with `refresh` on every round after the first; its operation holds the secrets of the
//! websockets the sending side connects to. The sending side (the source) is then asked to push the instance or the
//! volume to that operation, over the network, with the target's certificate. The kernel moves no byte itself: it starts
//! the two operations, follows their progress, and reads what landed.
//!
//! A stopped virtual machine on Ceph that moves to a linked host on its own Ceph cluster is copied by `rbd`, not by Incus:
//! the writer then asks the target to create the machine and its custom volumes empty (`source: none`), of the source's
//! exact sizes, for the kernel's copy to fill.
//!
//! Besides the copy the writer starts and stops an instance, reads a file inside one (the spot-check), sets
//! `boot.autostart` (off on a kept source, the source's own value on a verified copy), and deletes an instance or a custom volume: a target copy when a move is rolled back,
//! and a source when the owner finishes a move.
//! Every write is one of the closed shapes [`write_allowed`] lists, with plain names and a `project` query, and every
//! body is built here: no caller sends a body of its own. The websocket secrets are never logged or returned in a
//! `Debug` form.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::read::{IncusError, IncusRead, UnixIncus, exchange};
use crate::snapshot::plain_name;

/// How long one request may take.
const REQUEST: Duration = Duration::from_secs(30);
/// How long one wait request may hold the connection.
const WAIT_STEP: Duration = Duration::from_secs(2);
/// The largest file the spot-check reads.
const MAX_FILE: usize = 4 * 1024 * 1024;

/// Where the receiving side of a push waits: its operation, and the secrets of its websockets. The secrets are shown to
/// the sending side only, and never in a `Debug` form.
#[derive(Clone, PartialEq, Eq)]
pub struct Receive {
    pub operation: String,
    secrets: BTreeMap<String, String>,
}

impl fmt::Debug for Receive {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Receive")
            .field("operation", &self.operation)
            .field("secrets", &self.secrets.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Where a push goes: the target's Incus address (`host:port`), the certificate it serves there, and its receiving
/// operation.
pub struct PushTarget<'a> {
    pub address: &'a str,
    pub certificate: &'a str,
    pub receive: &'a Receive,
}

impl PushTarget<'_> {
    fn body(&self) -> Value {
        json!({
            "certificate": self.certificate,
            "operation": format!("https://{}/1.0/operations/{}", self.address, self.receive.operation),
            // Incus 7.6 reads the secrets under `secrets` (InstancePostTarget and StorageVolumePostTarget).
            "secrets": self.receive.secrets,
        })
    }
}

/// How an operation stands.
#[derive(Debug, Clone, PartialEq)]
pub enum OpState {
    /// Still running, with the operation as Incus answered it (its progress metadata included).
    Running(Value),
    /// Finished with success.
    Done(Value),
    /// Finished with failure: Incus's own message.
    Failed(String),
    /// Incus no longer knows it.
    Gone,
}

/// The narrow writer one side of a move uses.
#[derive(Debug, Clone)]
pub struct MoveWriter {
    socket: PathBuf,
}

/// Whether a request is one this module may send. Every GET may; a write must be one of these, with plain names and
/// `project=<plain>` as its only query:
///
/// - `POST /1.0/instances` (a create whose source is a migration push: the receiving side; or an empty virtual machine)
/// - `POST /1.0/instances/<name>` (a push: the sending side)
/// - `PUT /1.0/instances/<name>/state` (a start or a stop)
/// - `DELETE /1.0/instances/<name>`
/// - `POST /1.0/storage-pools/<pool>/volumes/custom` (a custom volume received by a migration push, or made empty)
/// - `POST /1.0/storage-pools/<pool>/volumes/custom/<name>` (a custom volume pushed)
/// - `DELETE /1.0/storage-pools/<pool>/volumes/custom/<name>`
pub fn write_allowed(method: &str, target: &str) -> bool {
    if method == "GET" {
        return true;
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let project_query = query
        .strip_prefix("project=")
        .is_some_and(|project| plain_name(project) && !project.contains('&'));
    if !project_query {
        return false;
    }
    let segments: Vec<&str> = path.split('/').collect();
    if segments.first() != Some(&"") || segments.get(1) != Some(&"1.0") {
        return false;
    }
    let rest = &segments[2..];
    if !rest.iter().all(|segment| plain_name(segment)) {
        return false;
    }
    matches!(
        (method, rest),
        ("POST", ["instances"])
            | ("POST", ["instances", _])
            | ("PUT", ["instances", _, "state"])
            | ("PATCH", ["instances", _])
            | ("DELETE", ["instances", _])
            | ("POST", ["storage-pools", _, "volumes", "custom"])
            | ("POST", ["storage-pools", _, "volumes", "custom", _])
            | ("DELETE", ["storage-pools", _, "volumes", "custom", _])
    )
}

fn refused(reason: &str) -> IncusError {
    IncusError::Refused(reason.to_string())
}

fn decode(reason: &str) -> IncusError {
    IncusError::Decode(reason.to_string())
}

fn names_ok(names: &[&str]) -> Result<(), IncusError> {
    if names.iter().all(|name| plain_name(name)) {
        Ok(())
    } else {
        Err(refused("a name is not plain"))
    }
}

/// The config a moved instance takes: its own keys, without the volatile ones Incus keeps for one host (its uuid, its
/// last state, its id map), but with the MAC addresses, so its cards keep their addresses.
pub fn moved_config(config: &Map<String, Value>) -> Map<String, Value> {
    config
        .iter()
        .filter(|(key, _)| {
            !key.starts_with("volatile.")
                || (key.ends_with(".hwaddr") && key.matches('.').count() == 2)
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

impl MoveWriter {
    pub fn new(socket: PathBuf) -> Self {
        Self { socket }
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    fn reader(&self) -> UnixIncus {
        UnixIncus::new(self.socket.clone()).with_timeout(REQUEST)
    }

    /// Reads one path.
    pub fn get(&self, path: &str) -> Result<Value, IncusError> {
        self.reader().get(path)
    }

    /// Reads the file at `path` inside the instance, at most 4 MiB of it.
    pub fn read_file(&self, project: &str, name: &str, path: &str) -> Result<Vec<u8>, IncusError> {
        names_ok(&[project, name])?;
        if !path.starts_with('/') || path.contains("..") || path.len() > 1024 {
            return Err(refused("the file path is not a plain absolute path"));
        }
        let encoded: String = path
            .bytes()
            .map(|byte| {
                if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-') {
                    (byte as char).to_string()
                } else {
                    format!("%{byte:02X}")
                }
            })
            .collect();
        let request = format!(
            "GET /1.0/instances/{name}/files?path={encoded}&project={project} HTTP/1.1\r\nHost: incus\r\nConnection: close\r\n\r\n"
        );
        let framed =
            crate::read::exchange_capped(&self.socket, REQUEST, request.as_bytes(), MAX_FILE)?;
        if !(200..300).contains(&framed.status) {
            return Err(IncusError::Status {
                code: framed.status,
                message: framed.reason,
            });
        }
        Ok(framed.body)
    }

    /// Asks the receiving side to create the instance `name` in `project` from a migration push, or, with `refresh`, to
    /// bring the copy it holds up to date. `source` is the instance as the sending side reads it: its architecture, its own
    /// config (see [`moved_config`]), devices, profiles and description go with it. The copy is made with
    /// `boot.autostart` false, whatever the source's is: a copy a failed move leaves on the target must not start by
    /// itself on a restart of that host, beside the source. A move gives it the source's own value once it is verified
    /// ([`MoveWriter::set_autostart`]).
    pub fn receive_instance(
        &self,
        project: &str,
        name: &str,
        source: &Value,
        refresh: bool,
    ) -> Result<Receive, IncusError> {
        names_ok(&[project, name])?;
        let kind = source.get("type").and_then(Value::as_str);
        if !matches!(kind, Some("container" | "virtual-machine")) {
            return Err(refused("only a container or a virtual machine is moved"));
        }
        let mut config = source
            .get("config")
            .and_then(Value::as_object)
            .map(moved_config)
            .unwrap_or_default();
        config.insert("boot.autostart".to_string(), json!("false"));
        let body = json!({
            "name": name,
            "type": kind,
            "architecture": source.get("architecture").cloned().unwrap_or(json!("")),
            "config": config,
            "devices": source.get("devices").cloned().unwrap_or(json!({})),
            "profiles": source.get("profiles").cloned().unwrap_or(json!([])),
            "description": source.get("description").cloned().unwrap_or(json!("")),
            "ephemeral": false,
            "source": {
                "type": "migration",
                "mode": "push",
                "refresh": refresh,
                "instance_only": false,
                "live": false,
            },
        });
        self.receive(&format!("/1.0/instances?project={project}"), &body)
    }

    /// Asks the receiving side to create the custom volume `name` on `pool` in `project` from a migration push, or to
    /// refresh the copy it holds.
    pub fn receive_volume(
        &self,
        project: &str,
        pool: &str,
        name: &str,
        source: &Value,
        refresh: bool,
    ) -> Result<Receive, IncusError> {
        names_ok(&[project, pool, name])?;
        let config: Map<String, Value> = source
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
        let body = json!({
            "name": name,
            "type": "custom",
            "content_type": source.get("content_type").cloned().unwrap_or(json!("filesystem")),
            "config": config,
            "description": source.get("description").cloned().unwrap_or(json!("")),
            "source": {
                "type": "migration",
                "mode": "push",
                "refresh": refresh,
                "volume_only": false,
            },
        });
        self.receive(
            &format!("/1.0/storage-pools/{pool}/volumes/custom?project={project}"),
            &body,
        )
    }

    /// Creates the virtual machine `name` in `project` empty (`source: none`), stopped, for a disk copy by Ceph: the
    /// source's architecture, own config (see [`moved_config`]), devices, profiles and description, `boot.autostart`
    /// false as in [`MoveWriter::receive_instance`], and its root disk (the device `root_device` of its expanded devices,
    /// `root`) on `pool` with the exact size `bytes`, so the empty disk Incus makes is the size of the one copied into it.
    pub fn create_empty_vm(
        &self,
        project: &str,
        name: &str,
        source: &Value,
        (root_device, root): (&str, &Value),
        (pool, bytes): (&str, u64),
    ) -> Result<(), IncusError> {
        names_ok(&[project, name, root_device, pool])?;
        if source.get("type").and_then(Value::as_str) != Some("virtual-machine") {
            return Err(refused("only a virtual machine is made empty"));
        }
        let mut config = source
            .get("config")
            .and_then(Value::as_object)
            .map(moved_config)
            .unwrap_or_default();
        config.insert("boot.autostart".to_string(), json!("false"));
        let mut devices = source
            .get("devices")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let mut disk = root.as_object().cloned().unwrap_or_default();
        disk.insert("type".to_string(), json!("disk"));
        disk.insert("path".to_string(), json!("/"));
        disk.insert("pool".to_string(), json!(pool));
        disk.insert("size".to_string(), json!(format!("{bytes}B")));
        devices.insert(root_device.to_string(), Value::Object(disk));
        let body = json!({
            "name": name,
            "type": "virtual-machine",
            "architecture": source.get("architecture").cloned().unwrap_or(json!("")),
            "config": config,
            "devices": devices,
            "profiles": source.get("profiles").cloned().unwrap_or(json!([])),
            "description": source.get("description").cloned().unwrap_or(json!("")),
            "ephemeral": false,
            "source": { "type": "none" },
        });
        let envelope = self.send(
            "POST",
            &format!("/1.0/instances?project={project}"),
            Some(&body),
        )?;
        self.finish(&envelope, Duration::from_secs(300))
    }

    /// Creates the custom volume `name` on `pool` in `project` empty, for a disk copy by Ceph: the source volume's
    /// content type, description and own config (no volatile key), with the exact size `bytes`.
    pub fn create_empty_volume(
        &self,
        project: &str,
        pool: &str,
        name: &str,
        source: &Value,
        bytes: u64,
    ) -> Result<(), IncusError> {
        names_ok(&[project, pool, name])?;
        let mut config: Map<String, Value> = source
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
        config.insert("size".to_string(), json!(format!("{bytes}B")));
        let body = json!({
            "name": name,
            "type": "custom",
            "content_type": source.get("content_type").cloned().unwrap_or(json!("filesystem")),
            "config": config,
            "description": source.get("description").cloned().unwrap_or(json!("")),
        });
        let envelope = self.send(
            "POST",
            &format!("/1.0/storage-pools/{pool}/volumes/custom?project={project}"),
            Some(&body),
        )?;
        self.finish(&envelope, Duration::from_secs(300))
    }

    fn receive(&self, path: &str, body: &Value) -> Result<Receive, IncusError> {
        let envelope = self.send("POST", path, Some(body))?;
        let operation = operation_id(&envelope)?;
        let secrets: BTreeMap<String, String> = envelope
            .pointer("/metadata/metadata")
            .and_then(Value::as_object)
            .map(|map| {
                map.iter()
                    .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        if !secrets.contains_key("fs") || !secrets.contains_key("control") {
            return Err(decode(
                "the receiving operation names no websocket secrets to push to",
            ));
        }
        Ok(Receive { operation, secrets })
    }

    /// Asks the sending side to push the instance `name` of `project`, with its snapshots, to `to`. Answers the sending
    /// operation's id. A round while the instance runs is sent `consistent: false`: Incus then copies without freezing
    /// the instance, on the drivers (dir, lvm, block-mode zfs) that would freeze it for a consistent copy; the last
    /// round, after the stop, is consistent.
    pub fn push_instance(
        &self,
        project: &str,
        name: &str,
        to: &PushTarget,
        consistent: bool,
    ) -> Result<String, IncusError> {
        names_ok(&[project, name])?;
        let body = json!({
            "name": name,
            "migration": true,
            "live": false,
            "instance_only": false,
            "allow_inconsistent": !consistent,
            "target": to.body(),
        });
        let envelope = self.send(
            "POST",
            &format!("/1.0/instances/{name}?project={project}"),
            Some(&body),
        )?;
        operation_id(&envelope)
    }

    /// Asks the sending side to push the custom volume `name` on `pool` of `project`, with its snapshots, to `to`.
    pub fn push_volume(
        &self,
        project: &str,
        pool: &str,
        name: &str,
        to: &PushTarget,
    ) -> Result<String, IncusError> {
        names_ok(&[project, pool, name])?;
        let body = json!({
            "name": name,
            "migration": true,
            "volume_only": false,
            "target": to.body(),
        });
        let envelope = self.send(
            "POST",
            &format!("/1.0/storage-pools/{pool}/volumes/custom/{name}?project={project}"),
            Some(&body),
        )?;
        operation_id(&envelope)
    }

    /// Waits at most `step` (a few seconds) on operation `id`, and answers how it stands.
    pub fn wait(&self, id: &str, step: Duration) -> Result<OpState, IncusError> {
        names_ok(&[id])?;
        let seconds = step.min(WAIT_STEP).as_secs().max(1);
        let path = format!("/1.0/operations/{id}/wait?timeout={seconds}");
        let answer = match self
            .reader()
            .get_within(&path, Duration::from_secs(seconds) + REQUEST)
        {
            Ok(answer) => answer,
            Err(IncusError::Status { code: 404, .. }) => return Ok(OpState::Gone),
            Err(error) => return Err(error),
        };
        Ok(state_of(answer))
    }

    /// Starts or stops the instance and waits for it, at most `deadline`. A stop is graceful for `timeout` seconds;
    /// `force` stops it at once.
    pub fn set_state(
        &self,
        project: &str,
        name: &str,
        action: &str,
        force: bool,
        deadline: Duration,
    ) -> Result<(), IncusError> {
        names_ok(&[project, name])?;
        if !matches!(action, "start" | "stop") {
            return Err(refused("only a start or a stop"));
        }
        let timeout = deadline.as_secs().clamp(1, 600);
        let body =
            json!({ "action": action, "timeout": timeout, "force": force, "stateful": false });
        let envelope = self.send(
            "PUT",
            &format!("/1.0/instances/{name}/state?project={project}"),
            Some(&body),
        )?;
        self.finish(&envelope, deadline)
    }

    /// Turns `boot.autostart` off on the instance `name` of `project`: a kept source must not start by itself on a host
    /// reboot next to its moved copy. The body is this one key and nothing else.
    pub fn disable_autostart(&self, project: &str, name: &str) -> Result<(), IncusError> {
        self.set_autostart(project, name, "false")
    }

    /// Sets `boot.autostart` on the instance `name` of `project` to `value`, or unsets it when `value` is empty (Incus
    /// keeps no key written empty): a verified copy takes its source's own value. The body is this one key and nothing
    /// else.
    pub fn set_autostart(&self, project: &str, name: &str, value: &str) -> Result<(), IncusError> {
        names_ok(&[project, name])?;
        if value.len() > 16
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(refused("the boot.autostart value is not plain"));
        }
        let body = json!({ "config": { "boot.autostart": value } });
        let envelope = self.send(
            "PATCH",
            &format!("/1.0/instances/{name}?project={project}"),
            Some(&body),
        )?;
        self.finish(&envelope, Duration::from_secs(60))
    }

    /// Deletes the instance `name` of `project`, which must be stopped.
    pub fn delete_instance(&self, project: &str, name: &str) -> Result<(), IncusError> {
        names_ok(&[project, name])?;
        let envelope = self.send(
            "DELETE",
            &format!("/1.0/instances/{name}?project={project}"),
            None,
        )?;
        self.finish(&envelope, Duration::from_secs(120))
    }

    /// Deletes the custom volume `name` on `pool` of `project`.
    pub fn delete_volume(&self, project: &str, pool: &str, name: &str) -> Result<(), IncusError> {
        names_ok(&[project, pool, name])?;
        let envelope = self.send(
            "DELETE",
            &format!("/1.0/storage-pools/{pool}/volumes/custom/{name}?project={project}"),
            None,
        )?;
        self.finish(&envelope, Duration::from_secs(120))
    }

    /// Waits for an answer's operation, when it has one, at most `deadline`.
    fn finish(&self, envelope: &Value, deadline: Duration) -> Result<(), IncusError> {
        if envelope.get("type").and_then(Value::as_str) != Some("async") {
            return Ok(());
        }
        let id = operation_id(envelope)?;
        let until = Instant::now() + deadline;
        loop {
            match self.wait(&id, WAIT_STEP)? {
                OpState::Done(_) | OpState::Gone => return Ok(()),
                OpState::Failed(message) => {
                    return Err(IncusError::Status { code: 400, message });
                }
                OpState::Running(_) if Instant::now() >= until => return Err(IncusError::Timeout),
                OpState::Running(_) => {}
            }
        }
    }

    fn send(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, IncusError> {
        if !write_allowed(method, path) {
            return Err(refused("the request is outside the move writer's shapes"));
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
        let framed = exchange(&self.socket, REQUEST, &request)?;
        crate::read::checked_envelope(&framed.into_raw())
    }
}

/// How an operation as Incus answers it stands.
pub fn state_of(operation: Value) -> OpState {
    match operation.get("status_code").and_then(Value::as_u64) {
        Some(200) => OpState::Done(operation),
        Some(code) if code >= 400 => OpState::Failed(
            operation
                .get("err")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .unwrap_or("the Incus operation failed")
                .chars()
                .take(512)
                .collect(),
        ),
        _ => OpState::Running(operation),
    }
}

/// The bytes an operation has moved of the stream it sends now, and its rate in bytes a second, as Incus 7.6 writes
/// them into its metadata: `fs_progress` (or `block_progress`) is the text "<volume>: 1.20GB (3.00MB/s)", in decimal
/// units (`units.GetByteSizeString`, two decimals). The figures restart with each stream (each snapshot, then the
/// volume). `None` for a figure that is not there or does not read.
pub fn progress_of(operation: &Value) -> (Option<u64>, Option<u64>) {
    let text = ["fs_progress", "block_progress"].iter().find_map(|key| {
        operation
            .pointer(&format!("/metadata/{key}"))
            .and_then(Value::as_str)
    });
    let Some(text) = text else {
        return (None, None);
    };
    // The volume's name comes first, before ": "; the figures are after the last one.
    let figures = text.rsplit_once(": ").map_or(text, |(_, figures)| figures);
    let (bytes, rate) = match figures.split_once(" (") {
        Some((bytes, rate)) => (
            bytes,
            Some(rate.trim_end_matches(')').trim_end_matches("/s")),
        ),
        None => (figures, None),
    };
    (
        decimal_bytes(bytes.trim()),
        rate.and_then(|rate| decimal_bytes(rate.trim())),
    )
}

/// "1.20GB" or "512B" as bytes, in the decimal units Incus prints.
fn decimal_bytes(text: &str) -> Option<u64> {
    let split = text.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
    let (number, unit) = text.split_at(split);
    let factor: u64 = match unit {
        "B" => 1,
        "kB" => 1_000,
        "MB" => 1_000_000,
        "GB" => 1_000_000_000,
        "TB" => 1_000_000_000_000,
        "PB" => 1_000_000_000_000_000,
        "EB" => 1_000_000_000_000_000_000,
        _ => return None,
    };
    let value: f64 = number.parse().ok()?;
    (value.is_finite() && value >= 0.0).then(|| (value * factor as f64).round() as u64)
}

fn operation_id(envelope: &Value) -> Result<String, IncusError> {
    envelope
        .get("operation")
        .and_then(Value::as_str)
        .and_then(|url| url.rsplit('/').next())
        .filter(|id| plain_name(id))
        .map(str::to_string)
        .ok_or_else(|| decode("the answer names no operation"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_moves_own_writes_are_allowed() {
        for (method, target) in [
            ("POST", "/1.0/instances?project=shop"),
            ("POST", "/1.0/instances/web?project=shop"),
            ("PUT", "/1.0/instances/web/state?project=shop"),
            ("PATCH", "/1.0/instances/web?project=shop"),
            ("DELETE", "/1.0/instances/web?project=shop"),
            (
                "POST",
                "/1.0/storage-pools/default/volumes/custom?project=default",
            ),
            (
                "POST",
                "/1.0/storage-pools/default/volumes/custom/data?project=default",
            ),
            (
                "DELETE",
                "/1.0/storage-pools/default/volumes/custom/data?project=default",
            ),
        ] {
            assert!(write_allowed(method, target), "{method} {target}");
        }
        for (method, target) in [
            ("POST", "/1.0/instances"),
            ("PUT", "/1.0/instances/web?project=shop"),
            ("DELETE", "/1.0/projects/shop?project=shop"),
            ("POST", "/1.0/instances/web/exec?project=shop"),
            ("DELETE", "/1.0/instances/web?project=shop&recursion=1"),
            ("DELETE", "/1.0/instances/../x?project=shop"),
            ("PUT", "/1.0/storage-pools/default?project=shop"),
        ] {
            assert!(!write_allowed(method, target), "{method} {target}");
        }
    }

    #[test]
    fn a_moved_config_keeps_its_own_keys_and_mac_addresses_only() {
        let config = json!({
            "limits.cpu": "2", "user.note": "x", "volatile.uuid": "u", "volatile.eth0.hwaddr": "00:16:3e:00:00:01",
            "volatile.last_state.power": "RUNNING", "volatile.idmap.current": "[]", "volatile.eth0.host_name": "veth1"
        });
        let kept = moved_config(config.as_object().expect("object"));
        let keys: Vec<&str> = kept.keys().map(String::as_str).collect();
        assert_eq!(keys, ["limits.cpu", "user.note", "volatile.eth0.hwaddr"]);
    }

    #[test]
    fn progress_is_read_from_the_text_incus_writes() {
        let op = json!({ "metadata": { "fs_progress": "web: 1.20GB (3.00MB/s)" } });
        assert_eq!(progress_of(&op), (Some(1_200_000_000), Some(3_000_000)));
        let op = json!({ "metadata": { "block_progress": "data: 512B (0B/s)" } });
        assert_eq!(progress_of(&op), (Some(512), Some(0)));
        // A name with ": " in it, and a figure Incus never prints.
        let op = json!({ "metadata": { "fs_progress": "a: b: 2.50kB (1.00kB/s)" } });
        assert_eq!(progress_of(&op), (Some(2500), Some(1000)));
        assert_eq!(
            progress_of(&json!({ "metadata": { "fs_progress": "x: 3QB (1QB/s)" } })),
            (None, None)
        );
        // The field the first build read is not one Incus writes.
        let op = json!({ "metadata": { "progress": { "processed": "1048576", "speed": "2048" } } });
        assert_eq!(progress_of(&op), (None, None));
        assert_eq!(progress_of(&json!({ "metadata": null })), (None, None));
    }

    #[test]
    fn a_push_names_the_secrets_as_incus_reads_them() {
        let receive = Receive {
            operation: "op1".into(),
            secrets: BTreeMap::from([
                ("control".to_string(), "c".to_string()),
                ("fs".to_string(), "f".to_string()),
            ]),
        };
        let body = PushTarget {
            address: "203.0.113.20:8443",
            certificate: "PEM",
            receive: &receive,
        }
        .body();
        assert_eq!(body["secrets"], json!({ "control": "c", "fs": "f" }));
        assert!(body.get("websockets").is_none());
        assert_eq!(
            body["operation"],
            "https://203.0.113.20:8443/1.0/operations/op1"
        );
    }

    #[test]
    fn the_secrets_are_not_in_the_debug_form() {
        let receive = Receive {
            operation: "op1".into(),
            secrets: BTreeMap::from([("fs".to_string(), "s3cr3t".to_string())]),
        };
        assert!(!format!("{receive:?}").contains("s3cr3t"));
    }
}
