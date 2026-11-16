//! Recover an owned service's binding from a copy. Incus is read only here.
use std::collections::BTreeMap;

use crate::{config::NodeConfig, kernel::Kernel, service::ServiceConfig};
use arca_incus::{IncusRead, UnixIncus};
use arca_kernel::{
    KernelError,
    canon::{Digest, canonical_json},
    desired::Entry,
    journal::{BindingEvent, BindingState},
    ownership::{IncusKind, KERNEL_RETIRING_KEY, ObjectRef, OwnerClass, classify_object},
};
use arca_mcp::CallError;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Snapshot {
    pub from: BindingState,
    pub to: BindingState,
    pub record_digest: Digest,
    pub evidence_digest: Digest,
}
fn refused(why: &str) -> CallError {
    CallError::Refused {
        code: "service_rebind".into(),
        message: format!("re-bind service: {why}; nothing was rebound"),
    }
}
fn segment(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 255
        && text != "."
        && text != ".."
        && text
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
}
fn own(body: &Value) -> Result<BTreeMap<String, String>, CallError> {
    serde_json::from_value(body["config"].clone())
        .map_err(|_| refused("own config could not be read"))
}
fn owned(
    body: &Value,
    object: &ObjectRef,
    entry: &Entry,
    retained: &BTreeMap<arca_kernel::desired::EntryId, (arca_kernel::desired::PluginId, Digest)>,
) -> Result<(), CallError> {
    let config = own(body)?;
    if config.contains_key(KERNEL_RETIRING_KEY)
        || classify_object(object, &config, retained)
            != (OwnerClass::Kernel {
                entry: entry.id.clone(),
            })
    {
        return Err(refused(
            "the instance or data volume is not this entry's own unretained, uncontested object",
        ));
    }
    Ok(())
}
fn disk<'a>(devices: &'a Value, path: &str) -> Result<&'a Value, CallError> {
    let devices = devices
        .as_object()
        .ok_or_else(|| refused("devices could not be read"))?;
    let mut matches = devices
        .values()
        .filter(|device| device["type"] == "disk" && device["path"] == path);
    let found = matches
        .next()
        .ok_or_else(|| refused("a root or data disk is missing"))?;
    if matches.next().is_some() {
        return Err(refused("the disk layout is ambiguous"));
    }
    Ok(found)
}
fn pool(device: &Value) -> Result<String, CallError> {
    device["pool"]
        .as_str()
        .filter(|pool| segment(pool))
        .map(str::to_string)
        .ok_or_else(|| refused("a disk's pool is missing or invalid"))
}

pub(crate) fn observe(
    node: &NodeConfig,
    kernel: &Kernel,
    name: &str,
) -> Result<Snapshot, CallError> {
    let entry = kernel
        .entries()
        .iter()
        .find(|entry| entry.id.0 == format!("service-{name}") && entry.plugin.0 == "arca.service")
        .ok_or_else(|| refused("the owned service record was not found"))?;
    let cfg: ServiceConfig = serde_json::from_value(entry.config.clone())
        .map_err(|_| refused("the service record is unreadable"))?;
    if cfg.adopted || cfg.creating || cfg.name != name || cfg.instance != name {
        return Err(refused("this is not a settled service under its own name"));
    }
    let from = kernel
        .binding_source(entry, &cfg)
        .map_err(|error| match error {
            KernelError::Journal(why) => refused(&why),
            _ => refused("the journal or converged service binding is not current"),
        })?;
    let retained = kernel
        .retained_entries()
        .map_err(|_| refused("retained ownership is unreadable"))?;
    let family = kernel
        .binding_projects(&node.kernel.projects)
        .map_err(|_| refused("the project family is unreadable"))?;
    if !family.contains(&from.instance.project) {
        return Err(refused("the service is outside the project family"));
    }
    let read = UnixIncus::new(node.incus.socket.clone());
    let get = |path: &str| {
        read.get(path)
            .map_err(|_| refused("Incus evidence is missing or unreadable"))
    };
    let listed = get("/1.0/instances?all-projects=true&recursion=1")?;
    let rows = listed
        .as_array()
        .ok_or_else(|| refused("the instance listing is not complete"))?;
    let mut rows_in_family = Vec::new();
    for row in rows {
        let project = row["project"]
            .as_str()
            .ok_or_else(|| refused("an instance's project is unreadable"))?;
        if !family.contains(project) {
            continue;
        }
        if row["name"].as_str().is_none()
            || row["config"]["volatile.uuid"]
                .as_str()
                .is_none_or(str::is_empty)
        {
            return Err(refused("an instance's name or UUID is unreadable"));
        }
        rows_in_family.push(row);
    }
    let targets: Vec<_> = rows_in_family
        .iter()
        .filter(|row| row["name"] == cfg.instance && row["project"] == cfg.project)
        .collect();
    if targets.len() != 1 {
        return Err(refused("the target instance is missing or ambiguous"));
    }
    let body = get(&format!(
        "/1.0/instances/{}?project={}",
        cfg.instance, cfg.project
    ))?;
    let config = own(&body)?;
    let uuid = config
        .get("volatile.uuid")
        .filter(|uuid| !uuid.is_empty() && uuid.len() <= 128 && !uuid.chars().any(char::is_control))
        .ok_or_else(|| refused("the replacement UUID is missing or invalid"))?;
    if from.instance.uuid.as_ref() == Some(uuid) {
        return Err(refused("the instance still has the journalled UUID"));
    }
    if body["name"] != cfg.instance
        || body.get("project").is_some_and(|p| p != &cfg.project)
        || targets[0]["config"]["volatile.uuid"].as_str() != Some(uuid.as_str())
        || rows_in_family
            .iter()
            .filter(|row| row["config"]["volatile.uuid"].as_str() == Some(uuid.as_str()))
            .count()
            != 1
        || rows_in_family.iter().any(|row| {
            row["name"] == cfg.instance
                && row["config"]["volatile.uuid"].as_str() == from.instance.uuid.as_deref()
        })
    {
        return Err(refused(
            "the replacement identity or old name is ambiguous in the project family",
        ));
    }
    let mut instance = from.instance.clone();
    instance.uuid = Some(uuid.clone());
    owned(&body, &instance, entry, &retained)?;
    let devices = body
        .get("expanded_devices")
        .filter(|value| value.is_object())
        .unwrap_or(&body["devices"]);
    let root_pool = pool(disk(devices, "/")?)?;
    let mut volume_config = Value::Null;
    let data_volume = if let Some(data) = &cfg.data {
        let device = disk(devices, &data.path)?;
        let pool = pool(device)?;
        let source = device["source"]
            .as_str()
            .filter(|name| segment(name))
            .ok_or_else(|| refused("the custom data volume source is missing or invalid"))?;
        if device
            .get("project")
            .is_some_and(|project| project != &cfg.project)
        {
            return Err(refused("the data disk names another project"));
        }
        let object = ObjectRef {
            kind: IncusKind::Volume,
            project: cfg.project.clone(),
            parent: Some(pool.clone()),
            name: source.into(),
            uuid: None,
        };
        let volume = get(&format!(
            "/1.0/storage-pools/{pool}/volumes/custom/{source}?project={}",
            cfg.project
        ))?;
        if volume["type"] != "custom"
            || volume
                .get("project")
                .is_some_and(|project| project != &cfg.project)
        {
            return Err(refused(
                "the attached volume is not a custom volume in this project",
            ));
        }
        owned(&volume, &object, entry, &retained)?;
        volume_config = volume["config"].clone();
        Some(object)
    } else {
        None
    };
    let evidence = json!({"config":body["config"],"devices":body["devices"],"expandedDevices":devices,"profiles":body["profiles"],"type":body["type"],"volumeConfig":volume_config});
    let evidence_digest = Digest::of(
        canonical_json(&evidence)
            .map_err(|_| refused("evidence cannot be fingerprinted"))?
            .as_bytes(),
    );
    Ok(Snapshot {
        from,
        to: BindingState {
            instance,
            root_pool,
            data_volume,
        },
        record_digest: entry.digest.clone(),
        evidence_digest,
    })
}

pub(crate) fn enrich(
    node: &NodeConfig,
    kernel: &Kernel,
    payload: &Value,
) -> Result<Value, CallError> {
    let object = payload
        .as_object()
        .ok_or_else(|| CallError::Invalid("args: expected an object".into()))?;
    if object
        .keys()
        .any(|key| !["name", "confirm"].contains(&key.as_str()))
    {
        return Err(CallError::Invalid(
            "args: name is the only caller-supplied re-bind fact".into(),
        ));
    }
    if payload
        .get("confirm")
        .is_some_and(|confirm| confirm != &Value::Bool(true))
    {
        return Err(CallError::Invalid(
            "args/confirm: expected true; nothing was proposed".into(),
        ));
    }
    let name = payload["name"]
        .as_str()
        .filter(|name| crate::service::service_name_ok(name))
        .ok_or_else(|| CallError::Invalid("args/name: expected a service name".into()))?;
    Ok(json!({"name":name,"binding":observe(node,kernel,name)?}))
}

pub(crate) fn execute(
    node: &NodeConfig,
    kernel: &mut Kernel,
    args: &Value,
    now: u64,
) -> Result<Value, CallError> {
    let name = args["name"]
        .as_str()
        .ok_or_else(|| CallError::Invalid("name: expected a service name".into()))?;
    let expected: Snapshot = serde_json::from_value(args["binding"].clone())
        .map_err(|_| refused("the approved binding facts are missing"))?;
    if observe(node, kernel, name)? != expected {
        return Err(refused(
            "identity, config or storage changed after the card was made",
        ));
    }
    // Re-read ownership, UUID, config and devices immediately before the durable append.
    if observe(node, kernel, name)? != expected {
        return Err(refused("identity, config or storage changed before commit"));
    }
    let event = BindingEvent {
        entry: arca_kernel::desired::EntryId(format!("service-{name}")),
        from: expected.from.clone(),
        to: expected.to.clone(),
        at_ms: now,
    };
    kernel.commit_binding(event, now).map_err(|failure| match failure {
        crate::kernel::BindingCommitFailure::NotAppended => refused("the journal refused the binding before appending it"),
        crate::kernel::BindingCommitFailure::Unconfirmed => CallError::Refused {code:"service_rebind".into(),message:"Re-bind was not confirmed. Restore state-directory writes and restart Arca so its durable journal recovers the binding and storage before retrying.".into()},
    })?;
    Ok(json!({"rebound":true,"name":name,"from":expected.from,"to":expected.to}))
}

/// Project only storage belonging to this live create. Later explicit storage
/// changes are left alone; an old binding cannot resurrect a removed incarnation.
pub(crate) fn project(entry: &mut Entry, event: &BindingEvent) -> Result<(), KernelError> {
    if entry.id != event.entry || entry.plugin.0 != "arca.service" {
        return Ok(());
    }
    let cfg: ServiceConfig = serde_json::from_value(entry.config.clone())
        .map_err(|_| KernelError::Journal("bound service record is unreadable".into()))?;
    let volume = cfg.data.as_ref().map(|_| cfg.data_locator().object());
    if cfg.instance != event.to.instance.name
        || cfg.project != event.to.instance.project
        || ![&event.from, &event.to]
            .iter()
            .any(|state| state.root_pool == cfg.pool && state.data_volume == volume)
    {
        eprintln!(
            "arca: binding recovery of {} skipped: its record does not match either side of the latest binding",
            entry.id.0
        );
        return Ok(());
    }
    entry.config["pool"] = json!(event.to.root_pool);
    if let Some(volume) = &event.to.data_volume {
        entry.config["dataVolume"] =
            json!({"project":volume.project,"pool":volume.parent,"name":volume.name});
    }
    entry.digest = Digest::of(
        canonical_json(&entry.config)
            .map_err(|_| KernelError::Journal("bound storage cannot be encoded".into()))?
            .as_bytes(),
    );
    Ok(())
}
