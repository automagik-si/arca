//! Owner-approved instance device changes, outside the desired-state journal.
//!
//! A GPU assignment is not a desired-state entry: it is a live change to one existing instance, made after the owner
//! approves an action proposal. The kernel's own writer is entry-scoped and refuses an object that does not carry its
//! entry's mark, so this module is a separate, deliberately narrow path. It reads one instance and writes only its
//! `devices` map: a GPU device is added by a PATCH, and removed by a PUT of the instance's own writable fields with the
//! device left out. It never creates, deletes or renames an instance, and it never writes an object outside the projects
//! the caller validated. The caller holds the owner's approval; this module only performs the change.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::read::{Framed, IncusError, TIMEOUT, exchange};

/// One instance as the assigner read it: the ETag the read carried, its kind and its own devices.
#[derive(Debug, Clone, PartialEq)]
pub struct InstanceDevices {
    pub project: String,
    pub name: String,
    /// `container` or `virtual-machine`, as Incus spells the `type` field.
    pub kind: String,
    pub etag: String,
    /// The instance metadata as read, so a PUT can send back the fields Incus replaces.
    pub body: Value,
    pub devices: BTreeMap<String, Value>,
}

impl InstanceDevices {
    /// The name of the GPU device with this PCI address, when the instance has one.
    pub fn gpu_device(&self, pci: &str) -> Option<&str> {
        self.devices.iter().find_map(|(name, device)| {
            (device.get("type").and_then(Value::as_str) == Some("gpu")
                && device.get("pci").and_then(Value::as_str) == Some(pci))
            .then_some(name.as_str())
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
    timeout: Duration,
}

impl DeviceAssigner {
    pub fn new(socket: PathBuf) -> Self {
        Self {
            socket,
            timeout: TIMEOUT,
        }
    }

    /// Reads one instance's own devices and its ETag.
    pub fn read(&self, project: &str, name: &str) -> Result<InstanceDevices, IncusError> {
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
        let devices = devices_of(&metadata)?;
        Ok(InstanceDevices {
            project: project.to_string(),
            name: name.to_string(),
            kind,
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
        if let Some(device) = instance.gpu_device(pci) {
            return Ok(GpuChange {
                device: device.to_string(),
                changed: false,
            });
        }
        let device = free_device_name(instance, pci);
        let mut devices = Map::new();
        devices.insert(
            device.clone(),
            json!({ "type": "gpu", "gputype": "physical", "pci": pci }),
        );
        let body = json!({ "devices": Value::Object(devices) });
        let url = object_url(&instance.project, &instance.name)?;
        self.mutate("PATCH", &url, Some(&instance.etag), &body)?;
        let fresh = self.read(&instance.project, &instance.name)?;
        match fresh.gpu_device(pci) {
            Some(_) => Ok(GpuChange {
                device,
                changed: true,
            }),
            None => Err(IncusError::AppliedUnconfirmed(Box::new(
                crate::read::Unconfirmed {
                    target: target_of(instance),
                    uuid: None,
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
        let Some(device) = instance.gpu_device(pci).map(str::to_string) else {
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
        if fresh.gpu_device(pci).is_some() {
            return Err(IncusError::AppliedUnconfirmed(Box::new(
                crate::read::Unconfirmed {
                    target: target_of(instance),
                    uuid: None,
                    owner: None,
                    reason: "the read-back still shows the GPU device".to_string(),
                },
            )));
        }
        Ok(GpuChange {
            device,
            changed: true,
        })
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
        uuid: None,
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
fn free_device_name(instance: &InstanceDevices, pci: &str) -> String {
    let base: String = format!("gpu-{}", pci.replace([':', '.'], "-"));
    if !instance.devices.contains_key(&base) {
        return base;
    }
    for suffix in 2..=u32::MAX {
        let candidate = format!("{base}-{suffix}");
        if !instance.devices.contains_key(&candidate) {
            return candidate;
        }
    }
    base
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
