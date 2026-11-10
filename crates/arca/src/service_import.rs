//! Importing a service another manager created, in place: no move, no stop.
//!
//! The read side already exists: an instance whose own config carries `user.arcakernel.entry = adopt-<name>` beside the other
//! manager's service keys gets a `service-<name>` record with `adopted: true` (`handlers::adopt_services`). This module is the
//! write side. The import sets that one key on an instance `kernel.importServices` lists, and the release clears it. Nothing
//! else on the instance is written: no device, profile, ACL, state or other key.
//!
//! [`plan`] reads only. It runs every check, and gives the one request the import would send and the digest of the instance's
//! pre-image. `arca service import plan` prints it, `action_propose` refuses a proposal whose plan is not ready, and
//! [`import`] runs it again before it writes. Each write is recorded in the adoption record, intent first.
//!
//! The pre-image digest is the SHA-256 of the canonical JSON of the instance's project, name, type, uuid, profiles, own
//! devices, the names of its own non-volatile config keys, and the values of its `user.arca.*` and `user.arcakernel.*` keys.
//! No other config value feeds it, so no secret in the instance's environment does. The write itself is guarded by Incus's
//! own ETag, which covers the whole instance.

use std::collections::BTreeMap;

use arca_incus::{ImportRead, ImportTarget, ImportWriter, IncusError, IncusRead, UnixIncus};
use arca_kernel::canon::{Digest, canonical_json};
use arca_kernel::desired::Entry;
use arca_kernel::ownership::KERNEL_ENTRY_KEY;
use arca_mcp::CallError;
use serde_json::{Value, json};

use crate::adoptions::{AdoptionRecord, ImportEvent, Phase};
use crate::config::{ImportService, NodeConfig};
use crate::{Failure, Flags, parse_flags, usage_failure};

/// The namespace of every key the kernel writes on an object.
const KERNEL_NAMESPACE: &str = "user.arcakernel.";

/// One check of an import plan: its name, whether it passed (`None` when this reader cannot run it), and what it found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub check: &'static str,
    pub ok: Option<bool>,
    pub detail: String,
}

/// The identity and the pre-image digest of an instance, from one read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub uuid: String,
    pub digest: String,
}

/// What an import of one service would do, from reads alone.
#[derive(Debug)]
pub struct Plan {
    pub name: String,
    /// The allowlist entry, when `kernel.importServices` lists the name.
    pub listed: Option<ImportService>,
    pub checks: Vec<Check>,
    /// The read the write would be sent under.
    pub read: Option<ImportRead>,
    pub image: Option<Image>,
}

impl Plan {
    /// The first check that failed, when one did.
    pub fn refusal(&self) -> Option<&Check> {
        self.checks.iter().find(|check| check.ok == Some(false))
    }

    /// Whether every check that ran passed and the write can be sent.
    pub fn ready(&self) -> bool {
        self.refusal().is_none() && self.read.is_some() && self.image.is_some()
    }

    /// The one request the import sends, when the plan is ready.
    pub fn write(&self) -> Option<Value> {
        let listed = self.listed.as_ref().filter(|_| self.ready())?;
        Some(json!({
            "method": "PATCH",
            "path": format!("/1.0/instances/{}?project={}", self.name, listed.project),
            "ifMatch": true,
            "body": { "config": { KERNEL_ENTRY_KEY: mark(&self.name) } },
        }))
    }

    /// The plan as `arca service import plan` prints it. It holds no config value but the other manager's service keys'
    /// outcome in words.
    pub fn json(&self) -> Value {
        json!({
            "name": self.name,
            "project": self.listed.as_ref().map(|listed| listed.project.clone()),
            "cidr": self.listed.as_ref().map(|listed| listed.cidr.clone()),
            "ready": self.ready(),
            "checks": self.checks.iter().map(|check| json!({
                "check": check.check,
                "ok": check.ok,
                "detail": check.detail,
            })).collect::<Vec<_>>(),
            "uuid": self.image.as_ref().map(|image| image.uuid.clone()),
            "preImage": self.image.as_ref().map(|image| image.digest.clone()),
            "write": self.write(),
            "wrote": false,
        })
    }
}

/// The value the import writes: the instance's import entry, `adopt-<name>`.
pub fn mark(name: &str) -> String {
    format!("{}{name}", crate::adoptions::ADOPT_ENTRY_PREFIX)
}

fn target(listed: &ImportService) -> ImportTarget {
    ImportTarget {
        project: listed.project.clone(),
        instance: listed.name.clone(),
    }
}

/// The import writer of `cfg`: every `kernel.importServices` entry, and nothing else.
pub fn writer(cfg: &NodeConfig) -> ImportWriter {
    ImportWriter::new(
        cfg.incus.socket.clone(),
        cfg.kernel.import_services.iter().map(target).collect(),
    )
}

fn own_config(body: &Value) -> BTreeMap<String, String> {
    body["config"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(key, value)| value.as_str().map(|text| (key.clone(), text.to_string())))
        .collect()
}

/// The identity and the pre-image digest of the instance `body` reads, with the import's own mark left out, so the image
/// before an import and the image after its release are the same when nothing else changed. `None` when the instance has
/// no uuid.
pub fn image(project: &str, name: &str, body: &Value) -> Option<Image> {
    let mut config = own_config(body);
    if config.get(KERNEL_ENTRY_KEY).map(String::as_str) == Some(mark(name).as_str()) {
        config.remove(KERNEL_ENTRY_KEY);
    }
    let uuid = config
        .get("volatile.uuid")
        .filter(|uuid| !uuid.is_empty())?
        .clone();
    let keys: Vec<&String> = config
        .keys()
        .filter(|key| !key.starts_with("volatile."))
        .collect();
    let owners: BTreeMap<&String, &String> = config
        .iter()
        .filter(|(key, _)| key.starts_with("user.arca.") || key.starts_with(KERNEL_NAMESPACE))
        .collect();
    let document = json!({
        "project": project,
        "name": name,
        "type": body["type"],
        "uuid": uuid,
        "profiles": body["profiles"],
        "devices": body["devices"],
        "keys": keys,
        "owners": owners,
    });
    let text = canonical_json(&document).ok()?;
    Some(Image {
        uuid,
        digest: Digest::of(text.as_bytes()).to_hex(),
    })
}

/// Plans the import of the service `name` with reads only. `entries` is the kernel's desired document when the caller holds
/// it; without it the name's freedom among Arca's own services is left to the kernel, and the check says so.
pub fn plan(
    cfg: &NodeConfig,
    name: &str,
    entries: Option<&[Entry]>,
    now: u64,
) -> Result<Plan, IncusError> {
    let mut plan = Plan {
        name: name.to_string(),
        listed: cfg
            .kernel
            .import_services
            .iter()
            .find(|listed| listed.name == name)
            .cloned(),
        checks: Vec::new(),
        read: None,
        image: None,
    };
    let mut check = |check: &'static str, ok: Option<bool>, detail: &str| {
        plan.checks.push(Check {
            check,
            ok,
            detail: detail.to_string(),
        });
    };
    let Some(listed) = plan.listed.clone() else {
        check(
            "listed",
            Some(false),
            "kernel.importServices does not list this service, so Arca may not write to it",
        );
        return Ok(plan);
    };
    check("listed", Some(true), "kernel.importServices lists it");
    let Some(read) = writer(cfg).read(&target(&listed))? else {
        check(
            "found",
            Some(false),
            "no instance of this name is in the listed project",
        );
        return Ok(plan);
    };
    check("found", Some(true), "the instance is in the listed project");
    let mut row = read.body.clone();
    row["project"] = json!(listed.project);
    row["name"] = json!(name);
    let config = own_config(&row);

    let container = row["type"].as_str() == Some("container");
    check(
        "container",
        Some(container),
        if container {
            "the instance is a container"
        } else {
            "the instance is not a container"
        },
    );

    let kernel_keys: Vec<&String> = config
        .keys()
        .filter(|key| key.starts_with(KERNEL_NAMESPACE))
        .collect();
    let imported = config.get(KERNEL_ENTRY_KEY).map(String::as_str) == Some(mark(name).as_str());
    check(
        "unmarked",
        Some(kernel_keys.is_empty()),
        if kernel_keys.is_empty() {
            "the instance carries no Arca key"
        } else if imported {
            "the instance already carries this import's mark: it is imported, and nothing is written again"
        } else {
            "the instance already carries an Arca key that this import did not write"
        },
    );

    let keys = [
        crate::service::TEMPLATE_KEY,
        crate::service::ADDRESS_KEY,
        crate::service::PORTS_KEY,
    ]
    .iter()
    .all(|key| config.contains_key(*key));
    check(
        "serviceKeys",
        Some(keys),
        if keys {
            "the instance carries the other manager's service keys (template, address, ports)"
        } else {
            "the instance does not carry the other manager's service keys (template, address, ports), so it is not a service of that manager's"
        },
    );

    let address = config.get(crate::service::ADDRESS_KEY);
    let inside = address
        .and_then(|text| text.parse::<std::net::Ipv4Addr>().ok())
        .is_some_and(|ip| crate::handlers::ipv4_in_cidr(ip, &listed.cidr));
    let nic = row["devices"]["eth0"]["ipv4.address"].as_str();
    let pinned = address.is_some() && nic == address.map(String::as_str);
    check(
        "address",
        Some(inside && pinned),
        if !inside {
            "the service's address is not inside the listed cidr"
        } else if !pinned {
            "the service's address is not the one its eth0 device pins"
        } else {
            "the service's address is inside the listed cidr and is the one its eth0 device pins"
        },
    );

    // A link names its provider by name and uuid, so the record is read against every instance, as the kernel reads it.
    let rows = UnixIncus::new(cfg.incus.socket.clone())
        .get("/1.0/instances?all-projects=true&recursion=2")?
        .as_array()
        .cloned()
        .unwrap_or_default();
    let gateway = crate::service::gateway(&listed.cidr)
        .map(|gateway| gateway.to_string())
        .unwrap_or_default();
    let record =
        crate::handlers::adopted_config(&row, &rows, &listed.cidr, &gateway, now).is_some();
    check(
        "record",
        Some(record),
        if record {
            "its keys read as a service record"
        } else {
            "its keys do not read as a service record (a template, a port list, a link or a published name is not valid)"
        },
    );

    match entries {
        Some(entries) => {
            let id = format!("service-{name}");
            let free = !entries.iter().any(|entry| entry.id.0 == id);
            check(
                "nameFree",
                Some(free),
                if free {
                    "Arca has no service of this name"
                } else {
                    "Arca already has a service of this name"
                },
            );
        }
        None => check(
            "nameFree",
            None,
            "not checked here: the running kernel checks that Arca has no service of this name when the import is proposed and again when it runs",
        ),
    }

    let image = image(&listed.project, name, &row);
    check(
        "identity",
        Some(image.is_some()),
        if image.is_some() {
            "the instance has a uuid"
        } else {
            "the instance has no uuid, so it cannot be told from another of the same name"
        },
    );
    plan.image = image;
    plan.read = Some(read);
    Ok(plan)
}

/// A finished import: the digest of the instance's pre-image, as the adoption record holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Imported {
    pub pre_image: String,
}

/// A finished release: whether a write was sent, the digest of the instance's image now (when it is still there), and
/// whether that is the pre-image the import recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Released {
    pub wrote: bool,
    pub image: Option<String>,
    pub restored: bool,
}

fn refused(message: impl Into<String>) -> CallError {
    CallError::Refused {
        code: "refused".into(),
        message: message.into(),
    }
}

fn record_failure(failure: Failure) -> CallError {
    CallError::Internal(format!(
        "the adoption record could not be written: {}",
        failure.message()
    ))
}

fn read_failure(what: &str, error: IncusError) -> CallError {
    match error {
        IncusError::Refused(reason) => refused(format!("{what}: {reason}")),
        _ => refused(format!("{what}: Incus could not be read")),
    }
}

/// Imports the service `name`: plans it again, records the intent, sends the one PATCH under the plan's ETag, reads the
/// instance back and records the outcome. A refusal before the PATCH writes nothing; a stale ETag is Incus's refusal and
/// writes nothing.
pub fn import(
    cfg: &NodeConfig,
    name: &str,
    entries: &[Entry],
    now: u64,
) -> Result<Imported, CallError> {
    let what = format!("import service {name}");
    let plan = plan(cfg, name, Some(entries), now).map_err(|error| read_failure(&what, error))?;
    if let Some(failed) = plan.refusal() {
        return Err(refused(format!(
            "{what}: {}; nothing was written",
            failed.detail
        )));
    }
    let (Some(listed), Some(read), Some(image)) = (&plan.listed, &plan.read, &plan.image) else {
        return Err(refused(format!("{what}: nothing was written")));
    };
    let target = target(listed);
    let entry = target
        .entry()
        .ok_or_else(|| refused(format!("{what}: the name has no import entry")))?;
    let mut object = target.object();
    object.uuid = Some(image.uuid.clone());
    let event = |phase: Phase, why: Option<&str>| ImportEvent {
        name: name.to_string(),
        project: listed.project.clone(),
        uuid: image.uuid.clone(),
        pre_image: image.digest.clone(),
        phase,
        why: why.map(str::to_string),
    };
    let mut record = AdoptionRecord::open(&cfg.state_dir).map_err(record_failure)?;
    record
        .import(event(Phase::Intent, None))
        .map_err(record_failure)?;
    let changes = BTreeMap::from([(KERNEL_ENTRY_KEY.to_string(), Some(entry.0.clone()))]);
    let writer = writer(cfg);
    match writer.patch(&entry, &object, &changes, &read.etag) {
        Ok(()) => {}
        Err(IncusError::Status { code: 412, .. }) => {
            record
                .import(event(
                    Phase::Failed,
                    Some("the instance changed after it was read"),
                ))
                .map_err(record_failure)?;
            return Err(refused(format!(
                "{what}: the instance changed while it was being imported, so nothing was written; propose the import again"
            )));
        }
        Err(IncusError::Refused(reason)) => {
            record
                .import(event(Phase::Failed, Some("the write was not allowed")))
                .map_err(record_failure)?;
            return Err(refused(format!("{what}: {reason}; nothing was written")));
        }
        // Any other answer may hide a write that landed: the read-back below decides.
        Err(_) => {}
    }
    let landed = match writer.read(&target) {
        Ok(Some(after)) => Some(own_config(&after.body).get(KERNEL_ENTRY_KEY) == Some(&entry.0)),
        Ok(None) => Some(false),
        Err(_) => None,
    };
    match landed {
        Some(true) => {
            record
                .import(event(Phase::Done, None))
                .map_err(record_failure)?;
            Ok(Imported {
                pre_image: image.digest.clone(),
            })
        }
        Some(false) => {
            record
                .import(event(Phase::Failed, Some("the mark did not land")))
                .map_err(record_failure)?;
            Err(refused(format!(
                "{what}: Incus did not take the write, so nothing was written"
            )))
        }
        None => {
            record
                .import(event(
                    Phase::Failed,
                    Some("the instance could not be read back"),
                ))
                .map_err(record_failure)?;
            Err(refused(format!(
                "{what}: the write could not be confirmed; service_list shows the service when it landed"
            )))
        }
    }
}

/// Releases the imported service `name`, whose record names `project`: clears the import's mark when the instance still
/// carries it, under the ETag of the read that showed it, and records the outcome. The caller removes the record after.
/// It refuses, and writes nothing, when the instance is no longer listed, is not the one that was imported, carries another
/// Arca key, or carries a mark that is not this import's. An instance that is gone, or whose mark is already cleared, takes
/// no write.
pub fn release(cfg: &NodeConfig, name: &str, project: &str) -> Result<Released, CallError> {
    let what = format!("release service {name}");
    let Some(listed) = cfg.kernel.import_service(name, project) else {
        return Err(refused(format!(
            "{what}: kernel.importServices no longer lists it, so Arca may not write to its instance; list it again with the same name, project and cidr, then release it. Nothing was written"
        )));
    };
    let target = target(listed);
    let entry = target
        .entry()
        .ok_or_else(|| refused(format!("{what}: the name has no import entry")))?;
    let writer = writer(cfg);
    let mut record = AdoptionRecord::open(&cfg.state_dir).map_err(record_failure)?;
    let imported = record.imported(name);
    let event = |uuid: &str, digest: &str, phase: Phase, why: Option<&str>| ImportEvent {
        name: name.to_string(),
        project: project.to_string(),
        uuid: uuid.to_string(),
        pre_image: digest.to_string(),
        phase,
        why: why.map(str::to_string),
    };
    let Some(read) = writer
        .read(&target)
        .map_err(|error| read_failure(&what, error))?
    else {
        let (uuid, digest) = imported
            .as_ref()
            .map(|imported| (imported.uuid.as_str(), imported.pre_image.as_str()))
            .unwrap_or_default();
        record
            .release(event(
                uuid,
                digest,
                Phase::Done,
                Some("the instance is gone; nothing was written"),
            ))
            .map_err(record_failure)?;
        return Ok(Released {
            wrote: false,
            image: None,
            restored: false,
        });
    };
    let config = own_config(&read.body);
    let other_keys = config
        .keys()
        .any(|key| key.starts_with(KERNEL_NAMESPACE) && key != KERNEL_ENTRY_KEY);
    if other_keys {
        return Err(refused(format!(
            "{what}: the instance carries another Arca key that the import did not write, so clearing the import's key would not put it back as it was. Nothing was written"
        )));
    }
    let now_image = image(project, name, &read.body).ok_or_else(|| {
        refused(format!(
            "{what}: the instance has no uuid, so it cannot be told from another of the same name. Nothing was written"
        ))
    })?;
    if let Some(imported) = &imported
        && imported.uuid != now_image.uuid
    {
        return Err(refused(format!(
            "{what}: the instance of this name is not the one that was imported; it was replaced since. Arca does not write to it. Nothing was written"
        )));
    }
    let restored = |digest: &str| {
        imported
            .as_ref()
            .is_some_and(|imported| imported.pre_image == digest)
    };
    match config.get(KERNEL_ENTRY_KEY) {
        None => {
            record
                .release(event(
                    &now_image.uuid,
                    &now_image.digest,
                    Phase::Done,
                    Some("the mark was already cleared; nothing was written"),
                ))
                .map_err(record_failure)?;
            return Ok(Released {
                wrote: false,
                restored: restored(&now_image.digest),
                image: Some(now_image.digest),
            });
        }
        Some(value) if *value != entry.0 => {
            return Err(refused(format!(
                "{what}: the instance's Arca mark no longer names this import, so Arca does not clear a mark it did not write. Nothing was written"
            )));
        }
        Some(_) => {}
    }
    record
        .release(event(
            &now_image.uuid,
            &now_image.digest,
            Phase::Intent,
            None,
        ))
        .map_err(record_failure)?;
    let mut object = target.object();
    object.uuid = Some(now_image.uuid.clone());
    let changes = BTreeMap::from([(KERNEL_ENTRY_KEY.to_string(), None)]);
    let failed = |record: &mut AdoptionRecord, why: &str| {
        record
            .release(event(
                &now_image.uuid,
                &now_image.digest,
                Phase::Failed,
                Some(why),
            ))
            .map_err(record_failure)
    };
    match writer.patch(&entry, &object, &changes, &read.etag) {
        Ok(()) => {}
        Err(IncusError::Status { code: 412, .. }) => {
            failed(&mut record, "the instance changed after it was read")?;
            return Err(refused(format!(
                "{what}: the instance changed while it was being released, so nothing was written; propose the release again"
            )));
        }
        Err(IncusError::Refused(reason)) => {
            failed(&mut record, "the write was not allowed")?;
            return Err(refused(format!("{what}: {reason}; nothing was written")));
        }
        Err(_) => {}
    }
    match writer.read(&target) {
        Ok(Some(after)) if !own_config(&after.body).contains_key(KERNEL_ENTRY_KEY) => {
            let after = image(project, name, &after.body).unwrap_or(now_image.clone());
            record
                .release(event(&after.uuid, &after.digest, Phase::Done, None))
                .map_err(record_failure)?;
            Ok(Released {
                wrote: true,
                restored: restored(&after.digest),
                image: Some(after.digest),
            })
        }
        Ok(None) => {
            record
                .release(event(
                    &now_image.uuid,
                    &now_image.digest,
                    Phase::Done,
                    Some("the instance is gone"),
                ))
                .map_err(record_failure)?;
            Ok(Released {
                wrote: false,
                image: None,
                restored: false,
            })
        }
        Ok(Some(_)) => {
            failed(&mut record, "the mark is still there")?;
            Err(refused(format!(
                "{what}: Incus did not take the write, so the service is still imported"
            )))
        }
        Err(_) => {
            failed(&mut record, "the instance could not be read back")?;
            Err(refused(format!(
                "{what}: the write could not be confirmed; service_list still shows the service while the key is there"
            )))
        }
    }
}

/// `arca service import plan --name <n> --config <file>`: prints the plan as JSON and writes nothing. It reads Incus with
/// GET alone and never opens the state directory, so it runs beside a serving kernel.
pub fn command(argv: &[String]) -> Result<Option<Value>, Failure> {
    let rest = match argv {
        [import, plan, rest @ ..] if import == "import" && plan == "plan" => rest,
        _ => return Err(usage_failure()),
    };
    let flags: Flags = parse_flags(rest, &["name", "config"], &["json"])?;
    let cfg = crate::config::load(std::path::Path::new(flags.required("config")?))?;
    let name = flags.required("name")?;
    if !crate::service::service_name_ok(name) {
        return Err(Failure::Usage("--name is a service name".to_string()));
    }
    let plan = plan(&cfg, name, None, crate::handlers::now_ms()).map_err(|error| {
        Failure::Runtime(match error {
            IncusError::Refused(reason) => reason,
            _ => "Incus could not be read".to_string(),
        })
    })?;
    Ok(Some(plan.json()))
}
