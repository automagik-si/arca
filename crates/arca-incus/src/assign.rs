//! Owner-approved instance device changes, outside the desired-state journal.
//!
//! A GPU assignment is not a desired-state entry: it is a live change to one existing instance, made after the owner
//! approves an action proposal and after the caller checked kernel mode and the kernel's project family. The kernel's own
//! writer is entry-scoped and refuses an object that does not carry its entry's mark, so this module is a separate,
//! deliberately narrow path. It reads one instance and writes only its `devices` map: it adds exactly one
//! `gpu-<normalized pci>` device whose body is `{type:gpu, gputype:physical, pci}`, and removes exactly that device by a PUT
//! of the instance's own writable fields with it left out. It never creates, deletes or renames an instance, never writes
//! another writer's GPU device, and refuses a project outside the kernel family it was built with before any request.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::read::{Framed, IncusError, TIMEOUT, exchange};
use crate::write::ProjectFamily;

/// The device name Arca gives a card: `gpu-` and the normalized PCI address, with `:` and `.` replaced by `-`.
const DEVICE_PREFIX: &str = "gpu-";

/// One instance as the assigner read it: the ETag the read carried, its kind and its own devices.
#[derive(Debug, Clone, PartialEq)]
pub struct InstanceDevices {
    pub project: String,
    pub name: String,
    /// `container` or `virtual-machine`, as Incus spells the `type` field.
    pub kind: String,
    /// The instance's `volatile.uuid`, when Incus reports one.
    pub uuid: Option<String>,
    pub etag: String,
    /// The instance metadata as read, so a PUT can send back the fields Incus replaces.
    pub body: Value,
    pub devices: BTreeMap<String, Value>,
}

impl InstanceDevices {
    /// The name of Arca's own GPU device for this PCI address, when the instance has one. Only the exact name and the exact
    /// three keys count: a device another writer made, a differently named one, and one with any extra key are not ours.
    pub fn arca_device(&self, pci: &str) -> Option<&str> {
        let pci = normalize_pci(pci);
        let expected = device_name(&pci);
        self.devices
            .get_key_value(&expected)
            .and_then(|(name, device)| (device == &gpu_body(&pci)).then_some(name.as_str()))
    }

    /// Whether any device, Arca's or another writer's, names this card as a GPU.
    pub fn gpu_devices_for<'a>(
        &'a self,
        pci: &'a str,
    ) -> impl Iterator<Item = (&'a str, &'a Value)> + 'a {
        let pci = normalize_pci(pci);
        self.devices.iter().filter_map(move |(name, device)| {
            (device.get("type").and_then(Value::as_str) == Some("gpu")
                && device
                    .get("pci")
                    .and_then(Value::as_str)
                    .is_some_and(|value| normalize_pci(value) == pci))
            .then_some((name.as_str(), device))
        })
    }
}

/// What one assignment changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuChange {
    pub device: String,
    /// False when the device was already there (attach) or already gone (detach).
    pub changed: bool,
}

/// The writer for one instance's GPU devices. It is only used by an action the owner approved.
#[derive(Debug, Clone)]
pub struct DeviceAssigner {
    socket: PathBuf,
    family: ProjectFamily,
    timeout: Duration,
}

impl DeviceAssigner {
    /// A writer that may address only the projects of `family`. It checks the family before every request, a read
    /// included, as the kernel's own writer does.
    pub fn new(socket: PathBuf, family: ProjectFamily) -> Self {
        Self {
            socket,
            family,
            timeout: TIMEOUT,
        }
    }

    /// Reads one instance's own devices and its ETag.
    pub fn read(&self, project: &str, name: &str) -> Result<InstanceDevices, IncusError> {
        self.allow(project)?;
        let url = object_url(project, name)?;
        let framed = self.send("GET", &url, None, None)?;
        let etag = framed
            .header("etag")
            .filter(|tag| plain_tag(tag))
            .map(str::to_string);
        let metadata = checked_metadata(framed)?;
        let etag = etag.ok_or_else(|| decode("the instance carries no usable ETag"))?;
        let kind = metadata
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("container")
            .to_string();
        let uuid = metadata
            .pointer("/config/volatile.uuid")
            .and_then(Value::as_str)
            .filter(|uuid| !uuid.is_empty())
            .map(str::to_string);
        let devices = devices_of(&metadata)?;
        Ok(InstanceDevices {
            project: project.to_string(),
            name: name.to_string(),
            kind,
            uuid,
            etag,
            body: metadata,
            devices,
        })
    }

    /// Adds a physical GPU device with `pci` to the instance. Incus merges a PATCH's named devices into the instance's
    /// own, so no other device is touched. A card already attached by its PCI address is returned unchanged.
    pub fn attach_gpu(
        &self,
        instance: &InstanceDevices,
        pci: &str,
    ) -> Result<GpuChange, IncusError> {
        self.allow(&instance.project)?;
        let pci = normalize_pci(pci);
        if let Some(device) = instance.arca_device(&pci) {
            return Ok(GpuChange {
                device: device.to_string(),
                changed: false,
            });
        }
        if instance.gpu_devices_for(&pci).next().is_some() {
            return Err(refused(
                "a GPU device for this card already exists but was not added by Arca",
            ));
        }
        // Detach finds Arca's device by the exact name `gpu-<pci>`. If another writer already holds that name, a second
        // name would be a device Arca could not detach, so the attach is refused instead.
        let device = device_name(&pci);
        if instance.devices.contains_key(&device) {
            return Err(refused(
                "the device name gpu-<pci> is already taken by another device",
            ));
        }
        let mut devices = Map::new();
        devices.insert(device.clone(), gpu_body(&pci));
        let body = json!({ "devices": Value::Object(devices) });
        let url = object_url(&instance.project, &instance.name)?;
        self.mutate("PATCH", &url, Some(&instance.etag), &body)?;
        let fresh = self.read(&instance.project, &instance.name)?;
        match fresh.arca_device(&pci) {
            Some(_) => Ok(GpuChange {
                device,
                changed: true,
            }),
            None => Err(IncusError::AppliedUnconfirmed(Box::new(
                crate::read::Unconfirmed {
                    target: target_of(instance),
                    uuid: instance.uuid.clone(),
                    owner: None,
                    reason: "the read-back does not show the GPU device".to_string(),
                },
            ))),
        }
    }

    /// Removes the GPU device with `pci` from the instance. A PUT replaces the writable fields, so the instance's own
    /// config, profiles and every other device are sent back exactly as read, with only this device left out. A card that
    /// is not attached is returned unchanged.
    pub fn detach_gpu(
        &self,
        instance: &InstanceDevices,
        pci: &str,
    ) -> Result<GpuChange, IncusError> {
        self.allow(&instance.project)?;
        let pci = normalize_pci(pci);
        let Some(device) = instance.arca_device(&pci).map(str::to_string) else {
            if instance.gpu_devices_for(&pci).next().is_some() {
                return Err(refused("this GPU device was not added by Arca"));
            }
            return Ok(GpuChange {
                device: String::new(),
                changed: false,
            });
        };
        let mut devices = instance.devices.clone();
        devices.remove(&device);
        let body = put_body(instance, &devices);
        let url = object_url(&instance.project, &instance.name)?;
        self.mutate("PUT", &url, Some(&instance.etag), &body)?;
        let fresh = self.read(&instance.project, &instance.name)?;
        // Only Arca's named device must be gone. Another writer's device for the same card is not ours to remove, and
        // its survival is not an unconfirmed removal.
        if fresh.devices.contains_key(&device) {
            return Err(IncusError::AppliedUnconfirmed(Box::new(
                crate::read::Unconfirmed {
                    target: target_of(instance),
                    uuid: instance.uuid.clone(),
                    owner: None,
                    reason: "the read-back still shows Arca's GPU device".to_string(),
                },
            )));
        }
        Ok(GpuChange {
            device,
            changed: true,
        })
    }

    /// Refuses a project outside the family before any request is sent, a read included.
    fn allow(&self, project: &str) -> Result<(), IncusError> {
        self.family
            .allows(arca_kernel::ownership::IncusKind::Instance, project)
            .then_some(())
            .ok_or_else(|| refused("the project is outside the kernel's family"))
    }

    fn send(
        &self,
        method: &str,
        path: &str,
        if_match: Option<&str>,
        body: Option<&Value>,
    ) -> Result<Framed, IncusError> {
        let body = body
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|_| decode("the request body does not serialize"))?;
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: incus\r\nAccept: application/json\r\nConnection: close\r\n"
        );
        if let Some(tag) = if_match {
            head.push_str(&format!("If-Match: {tag}\r\n"));
        }
        if let Some(body) = &body {
            head.push_str("Content-Type: application/json\r\n");
            head.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        head.push_str("\r\n");
        let mut request = head.into_bytes();
        if let Some(body) = body {
            request.extend_from_slice(&body);
        }
        exchange(&self.socket, self.timeout, &request)
    }

    fn mutate(
        &self,
        method: &str,
        path: &str,
        if_match: Option<&str>,
        body: &Value,
    ) -> Result<(), IncusError> {
        let framed = self.send(method, path, if_match, Some(body))?;
        let envelope = checked_envelope(framed)?;
        // An instance's device PATCH and PUT are synchronous in Incus. An async answer is still settled by the
        // read-back in the caller, so nothing else is waited on here.
        if envelope.get("type").and_then(Value::as_str) == Some("async")
            && envelope
                .get("operation")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
        {
            return Err(decode("the answer names no operation"));
        }
        Ok(())
    }
}

/// The full PUT body for an instance: the fields Incus replaces, with `devices` as given.
fn put_body(instance: &InstanceDevices, devices: &BTreeMap<String, Value>) -> Value {
    let mut body = Map::new();
    body.insert(
        "config".to_string(),
        instance
            .body
            .get("config")
            .cloned()
            .unwrap_or_else(|| json!({})),
    );
    body.insert("devices".to_string(), json!(devices));
    body.insert(
        "profiles".to_string(),
        instance
            .body
            .get("profiles")
            .cloned()
            .unwrap_or_else(|| json!([])),
    );
    for key in ["architecture", "description", "ephemeral", "stateful"] {
        if let Some(value) = instance.body.get(key) {
            body.insert(key.to_string(), value.clone());
        }
    }
    Value::Object(body)
}

fn target_of(instance: &InstanceDevices) -> crate::read::Target {
    crate::read::Target {
        kind: arca_kernel::ownership::IncusKind::Instance,
        project: instance.project.clone(),
        parent: None,
        name: instance.name.clone(),
        uuid: instance.uuid.clone(),
    }
}

fn object_url(project: &str, name: &str) -> Result<String, IncusError> {
    if !plain_name(project) || !plain_name(name) {
        return Err(refused("the project or instance name is not plain"));
    }
    Ok(format!("/1.0/instances/{name}?project={project}"))
}

fn plain_name(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 128
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn plain_tag(tag: &str) -> bool {
    !tag.is_empty() && tag.len() <= 256 && tag.bytes().all(|byte| byte.is_ascii_graphic())
}

/// The device name for a PCI address. It names the address in full, so two identical cards get two devices.
pub fn device_name(pci: &str) -> String {
    format!(
        "{DEVICE_PREFIX}{}",
        normalize_pci(pci).replace([':', '.'], "-")
    )
}

/// The exact device body Arca writes. A device with any other key, or another value, is not Arca's.
fn gpu_body(pci: &str) -> Value {
    json!({ "type": "gpu", "gputype": "physical", "pci": pci })
}

/// Normalizes a PCI address to four lowercase domain hex digits, so `01:00.0` and `0000:01:00.0` name the same card. A
/// value that is not `domain:bus:slot.function` is returned lowercased unchanged.
pub fn normalize_pci(text: &str) -> String {
    let text = text.trim().to_ascii_lowercase();
    let parts: Vec<&str> = text.split(':').collect();
    match parts.as_slice() {
        // bus:slot.function, with no domain.
        [bus, rest] if bus.len() == 2 && bus.bytes().all(|byte| byte.is_ascii_hexdigit()) => {
            format!("0000:{bus}:{rest}")
        }
        // domain:bus:slot.function, with a domain of up to eight hex digits.
        [domain, bus, rest]
            if !domain.is_empty()
                && domain.len() <= 8
                && domain.bytes().all(|byte| byte.is_ascii_hexdigit()) =>
        {
            let trimmed = domain.trim_start_matches('0');
            let domain = if trimmed.is_empty() { "0" } else { trimmed };
            format!("{domain:0>4}:{bus}:{rest}")
        }
        _ => text,
    }
}

fn devices_of(metadata: &Value) -> Result<BTreeMap<String, Value>, IncusError> {
    let Some(devices) = metadata.get("devices") else {
        return Ok(BTreeMap::new());
    };
    let Some(devices) = devices.as_object() else {
        return Err(decode("the instance's devices are not an object"));
    };
    let mut out = BTreeMap::new();
    for (name, device) in devices {
        if !device.is_object() {
            return Err(decode("an instance device is not an object"));
        }
        out.insert(name.clone(), device.clone());
    }
    Ok(out)
}

fn checked_metadata(framed: Framed) -> Result<Value, IncusError> {
    let envelope = checked_envelope(framed)?;
    envelope
        .get("metadata")
        .cloned()
        .ok_or_else(|| decode("the answer has no metadata"))
}

fn checked_envelope(framed: Framed) -> Result<Value, IncusError> {
    let status = framed.status;
    let reason = framed.reason.clone();
    if !(200..300).contains(&status) {
        return Err(IncusError::Status {
            code: status,
            message: reason,
        });
    }
    let value: Value =
        serde_json::from_slice(&framed.body).map_err(|_| decode("the answer is not JSON"))?;
    if value.get("type").and_then(Value::as_str) == Some("error") {
        return Err(IncusError::Status {
            code: value
                .get("error_code")
                .and_then(Value::as_u64)
                .and_then(|code| u16::try_from(code).ok())
                .unwrap_or(status),
            message: value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("the Incus answer carries an error")
                .to_string(),
        });
    }
    Ok(value)
}

fn refused(reason: &str) -> IncusError {
    IncusError::Refused(reason.to_string())
}

fn decode(reason: &str) -> IncusError {
    IncusError::Decode(reason.to_string())
}
