//! Inert action proposals: a viewer may ask the owner to approve one of a closed set of service, database, GPU, storage,
//! snapshot and instance start, stop and restart actions.
//!
//! A proposal changes nothing. It carries the action's own arguments, a digest over the canonical `{action, args}`, the
//! exact arguments the owner is shown as `details`, and a plain-language summary built here from the validated arguments.
//! The kernel keeps at most [`MAX_WAITING`] unexpired proposals; a ninth is refused with `TooManyProposals` rather than
//! silently retiring an older one. A repeated digest returns the record that already waits.
//!
//! The proposals live in an append-only journal next to the effect journal, `action-proposals.jsonl`. Every record is one
//! line with an `event` of `proposed`, `approved`, `declined` or `result`. Opening the kernel replays the file, re-checks
//! every proposed record against its action's own validation and the kernel's configured image, keeps only the waiting,
//! unexpired proposals and rewrites it atomically. A file that cannot be read, holds a line that fails validation, or is too
//! large is quarantined as `<name>.bad-<ts>` and the kernel starts with no proposals: this file can never stop a start. The
//! file holds no approval string, proof or secret.
//!
//! Appending keeps the journal bounded as well: once the file passes [`MAX_COMPACT_BYTES`] it is compacted to the waiting set
//! before the call returns. A stale temporary file from a compaction that a crash interrupted is removed when the kernel next
//! opens the directory, so a reused process id cannot stand in the way.

use std::collections::{HashMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use arca_api::{conforms, verb};
use arca_kernel::canon::{Digest, canonical_json};
use arca_reconcile::PLAN_TTL_MS;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The proposals journal's file name in the state directory.
pub const ACTION_PROPOSALS_FILE: &str = "action-proposals.jsonl";
/// How many unexpired action proposals may wait at once. A ninth is refused.
pub const MAX_WAITING: usize = 8;
/// How many accepted proposals one token may make in [`PROPOSAL_RATE_WINDOW_MS`].
pub const PROPOSAL_RATE_MAX: usize = 30;
/// The per-token proposal window: ten minutes.
pub const PROPOSAL_RATE_WINDOW_MS: u64 = 10 * 60 * 1000;
/// The largest proposals journal this build will read. A larger one is quarantined.
const MAX_JOURNAL_BYTES: u64 = 16 * 1024 * 1024;
/// A journal past this size is compacted to its waiting set during an append, so a long-running kernel does not wait for a
/// restart to shed retired events.
const MAX_COMPACT_BYTES: u64 = 1024 * 1024;

/// One action an owner may be asked to approve. The names are the gated verbs themselves, so a statement and the action
/// it admits say the same word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    ServiceCreate,
    ServiceRemove,
    ServiceImport,
    ServiceRelease,
    ServicePublish,
    ServiceUnpublish,
    ServiceConnect,
    ServiceDisconnect,
    PostgresCreate,
    GpuAttach,
    GpuDetach,
    StorageAdd,
    SnapshotCreate,
    SnapshotDelete,
    InstanceStart,
    InstanceStop,
    InstanceRestart,
    TenantCreate,
    TenantRemove,
    DeployRemove,
}

/// `text` as one line of a card: control characters become spaces, and past `max` characters it ends in an ellipsis.
fn shown(text: &str, max: usize) -> String {
    let clean: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if clean.chars().count() <= max {
        return clean;
    }
    let mut cut: String = clean.chars().take(max).collect();
    cut.push('…');
    cut
}

/// The most characters one card row shows of a setting. A proposal whose setting would need more in one row is
/// refused when it is made ([`ActionKind::prepare`]), so a card never cuts what the owner approves.
pub const SETTING_ROW_MAX: usize = 160;

/// The card rows of one setting a create replaces: its name and its value in plain words. A list has one row for each
/// element, numbered from one, so a long list is shown whole; an element that is an object is shown as its fields.
pub fn setting_rows(key: &str, value: &Value) -> Vec<(String, String)> {
    let label = format!("Setting {}", shown(key, 64));
    match value {
        Value::Array(items) if items.is_empty() => vec![(label, "(an empty list)".to_string())],
        Value::Array(items) => items
            .iter()
            .enumerate()
            .map(|(index, item)| (format!("{label} {}", index + 1), setting_words(item)))
            .collect(),
        other => vec![(label, setting_words(other))],
    }
}

/// A setting's value in words: a string as it is, an object as `key: value; key: value`, a list as its elements with
/// commas between, and anything else as JSON writes it.
fn setting_words(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(setting_words)
            .collect::<Vec<_>>()
            .join(", "),
        Value::Object(map) => map
            .iter()
            .map(|(key, item)| format!("{key}: {}", setting_words(item)))
            .collect::<Vec<_>>()
            .join("; "),
        other => other.to_string(),
    }
}

impl ActionKind {
    /// Every allowed action, in the order the table documents them.
    pub const ALL: [ActionKind; 20] = [
        Self::ServiceCreate,
        Self::ServiceRemove,
        Self::ServiceImport,
        Self::ServiceRelease,
        Self::ServicePublish,
        Self::ServiceUnpublish,
        Self::ServiceConnect,
        Self::ServiceDisconnect,
        Self::PostgresCreate,
        Self::GpuAttach,
        Self::GpuDetach,
        Self::StorageAdd,
        Self::SnapshotCreate,
        Self::SnapshotDelete,
        Self::InstanceStart,
        Self::InstanceStop,
        Self::InstanceRestart,
        Self::TenantCreate,
        Self::TenantRemove,
        Self::DeployRemove,
    ];

    /// The verb name this action runs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ServiceCreate => "service_create",
            Self::ServiceRemove => "service_remove",
            Self::ServiceImport => "service_import",
            Self::ServiceRelease => "service_release",
            Self::ServicePublish => "service_publish",
            Self::ServiceUnpublish => "service_unpublish",
            Self::ServiceConnect => "service_connect",
            Self::ServiceDisconnect => "service_disconnect",
            Self::PostgresCreate => "postgres_create",
            Self::GpuAttach => "gpu_attach",
            Self::GpuDetach => "gpu_detach",
            Self::StorageAdd => "storage_add",
            Self::SnapshotCreate => "snapshot_create",
            Self::SnapshotDelete => "snapshot_delete",
            Self::InstanceStart => "instance_start",
            Self::InstanceStop => "instance_stop",
            Self::InstanceRestart => "instance_restart",
            Self::TenantCreate => "tenant_create",
            Self::TenantRemove => "tenant_remove",
            Self::DeployRemove => "deploy_remove",
        }
    }

    /// Whether the action starts, stops or restarts one instance.
    pub fn is_lifecycle(self) -> bool {
        matches!(
            self,
            Self::InstanceStart | Self::InstanceStop | Self::InstanceRestart
        )
    }

    /// Whether the action adds snapshot storage or makes or deletes a snapshot.
    pub fn is_storage(self) -> bool {
        matches!(
            self,
            Self::StorageAdd | Self::SnapshotCreate | Self::SnapshotDelete
        )
    }

    /// The action for a verb name, or `None` when it is not on the list.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == name)
    }

    /// The verb's own params schema, with the inline `approval` field removed: the proposal is what replaces that field.
    /// A destructive verb's `confirm` is removed too: it is the door's, and an approved action never binds it.
    pub fn params_schema(self) -> Value {
        let mut schema = (verb(self.as_str())
            .expect("every allowed action is a verb in the table")
            .params)();
        if let Some(required) = schema.get_mut("required").and_then(Value::as_array_mut) {
            required.retain(|name| name.as_str() != Some("approval"));
        }
        if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
            properties.remove("approval");
            properties.remove(arca_api::verbs::CONFIRM);
        }
        schema
    }

    /// Checks `args` against the verb's own schema (without `approval`) and the verb's semantic rules, and returns the
    /// normalized arguments the owner will approve.
    ///
    /// `postgres_create` may not choose a container image. When the kernel has a configured pinned default, that image is
    /// inserted; an explicitly different image is refused, and an absent default refuses the proposal with a clear error.
    /// `service_create`'s template is already constrained to the catalog's name grammar by its schema.
    pub fn prepare(self, args: &Value, postgres_image: Option<&str>) -> Result<Value, String> {
        let mut args = args.clone();
        // A destructive verb's `confirm: true` is dropped, so the proposal (and the approval the owner signs) binds the
        // call without it; `false` proposes nothing. Any other value stays, and the schema below refuses it.
        let destructive = verb(self.as_str()).is_some_and(|v| v.destructive);
        if destructive && let Some(object) = args.as_object_mut() {
            match object.get(arca_api::verbs::CONFIRM) {
                Some(Value::Bool(true)) => {
                    object.remove(arca_api::verbs::CONFIRM);
                }
                Some(Value::Bool(false)) => {
                    return Err("args/confirm: confirm is false: nothing was proposed".to_string());
                }
                _ => {}
            }
        }
        if self == Self::PostgresCreate {
            let Some(default) = postgres_image else {
                return Err(
                    "the kernel has no configured postgres image; set kernel.postgresImage"
                        .to_string(),
                );
            };
            if arca_postgres::fingerprint(default).is_err() {
                return Err(
                    "kernel.postgresImage is not a digest-pinned image reference".to_string(),
                );
            }
            let Some(object) = args.as_object_mut() else {
                return Err("args: expected an object".to_string());
            };
            if let Some(chosen) = object.get("image")
                && chosen.as_str() != Some(default)
            {
                return Err(
                    "args/image: a proposal uses the kernel's configured pinned image; it cannot choose another"
                        .to_string(),
                );
            }
            object.insert("image".to_string(), Value::String(default.to_string()));
        }
        conforms(&self.params_schema(), &args)?;
        match self {
            Self::ServiceCreate => {
                // The digest the owner approves covers the MAC in the one form the record keeps.
                if let Some(mac) = args.get("mac") {
                    let normalized = mac
                        .as_str()
                        .and_then(crate::service::normalize_mac)
                        .ok_or_else(|| {
                            "args/mac: expected six hex octets joined by \":\", not a group or all-zero address"
                                .to_string()
                        })?;
                    args["mac"] = Value::String(normalized);
                }
                // The owner approves what the card shows, so a setting the card could not show whole is refused
                // here, before a proposal exists. A list is shown one element to a row.
                for (key, value) in args["settings"].as_object().into_iter().flatten() {
                    for (label, words) in setting_rows(key, value) {
                        let length = words.chars().count();
                        if length > SETTING_ROW_MAX {
                            return Err(format!(
                                "args/settings: the owner's card shows a setting in rows of at most {SETTING_ROW_MAX} characters, and \"{label}\" needs {length}: shorten it, or give a list more and shorter elements"
                            ));
                        }
                    }
                }
            }
            Self::ServicePublish => {
                if args["reach"].as_str() != Some("tailnet") {
                    return Err("args/reach: the only reach is the owner's tailnet".to_string());
                }
            }
            Self::PostgresCreate => {
                let name = args["name"].as_str().unwrap_or_default();
                if !arca_netproject::name_ok(name, "postgres", 31) {
                    return Err("args/name: expected a database name".to_string());
                }
            }
            Self::GpuAttach | Self::GpuDetach => {
                for key in ["project", "kind", "uuid"] {
                    if args.get(key).is_none() {
                        return Err(format!(
                            "args/{key}: the kernel resolves it when the proposal is made"
                        ));
                    }
                }
                let kind = args["kind"].as_str().unwrap_or_default();
                if !matches!(kind, "container" | "virtual-machine") {
                    return Err("args/kind: expected container or virtual-machine".to_string());
                }
                let pci = args["pci"].as_str().unwrap_or_default();
                let normalized = arca_metrics::normalize_pci(pci);
                let Some(object) = args.as_object_mut() else {
                    return Err("args: expected an object".to_string());
                };
                object.insert("pci".to_string(), Value::String(normalized));
            }
            Self::StorageAdd | Self::SnapshotCreate | Self::SnapshotDelete => {
                args = crate::storage::prepare(self, args)?;
            }
            _ => {}
        }
        Ok(args)
    }

    /// The arguments the owner is shown, in plain words. Every field of the verb's params schema has a row. A GPU action
    /// also names the card's product when the inventory knows it; [`Self::details`] leaves that out.
    pub fn details(self, args: &Value) -> Vec<ActionDetail> {
        self.details_with_gpu(args, None)
    }

    /// The same rows, with the card's product filled in for an attach or detach when the inventory knows it. For a
    /// `service_create` the same argument carries why its template is created stopped whatever the call says, when it
    /// is (`crate::service::template_stopped_reason`), and for a deploy's removal the deploy's parts. The GPU row
    /// always carries the PCI address, so two identical cards are never confused, and the effect row never claims that a
    /// card shared by more than one container has isolated VRAM.
    pub fn details_with_gpu(self, args: &Value, gpu_product: Option<&str>) -> Vec<ActionDetail> {
        let text = |key: &str| args[key].as_str().unwrap_or_default();
        let row = |label: &str, value: &str| ActionDetail {
            label: label.to_string(),
            value: value.to_string(),
        };
        let rows = match self {
            Self::ServiceCreate => {
                let mut rows = vec![row("Name", text("name")), row("Template", text("template"))];
                // Only a virtual machine on a host bridge names these.
                if args.get("bridge").is_some() {
                    rows.push(row("Host bridge", text("bridge")));
                }
                if args.get("mac").is_some() {
                    rows.push(row("MAC address", text("mac")));
                }
                // The template gives this service access to the kernel. The proposal names it (it is filled in from
                // the template when the proposal is made), so the approval binds it and the card says it in words.
                if let Some(port) = args["kernelAccess"]["port"].as_u64() {
                    rows.push(row("Kernel access", &kernel_access_words(port)));
                }
                // The row says what will happen, so the owner approves a stopped service knowingly: a create that
                // said `start: false`, and one whose template is created stopped whatever the call says (the reason
                // is read from the template when the proposal is made, and is not in its arguments).
                match (args.get("start").and_then(Value::as_bool), gpu_product) {
                    (Some(false), _) => rows.push(row("Start", start_words(false))),
                    (_, Some(reason)) => rows.push(row("Start", &stopped_words(reason))),
                    (Some(true), None) => rows.push(row("Start", start_words(true))),
                    (None, None) => {}
                }
                // One row for each setting the create replaces, with its value: a setting such as an address
                // decides where the new machine sends its credential, so the owner sees it before approving.
                for (key, value) in args["settings"].as_object().into_iter().flatten() {
                    for (label, words) in setting_rows(key, value) {
                        // A proposal never holds a longer value (`prepare` refuses one); the cut is for arguments
                        // that did not come through it.
                        rows.push(row(&label, &shown(&words, SETTING_ROW_MAX)));
                    }
                }
                rows
            }
            Self::ServiceRemove | Self::ServiceUnpublish => vec![row("Name", text("name"))],
            Self::ServiceImport => vec![
                row("Name", text("name")),
                row(
                    "Effect",
                    &format!(
                        "Arca takes over this service where it stands. It writes one key on the instance, user.arcakernel.entry = adopt-{}, and nothing else: the service is not stopped, moved or changed.",
                        text("name")
                    ),
                ),
            ],
            Self::ServiceRelease => vec![
                row("Name", text("name")),
                row(
                    "Effect",
                    "Arca gives this service back to the manager that created it. It clears the one key the import wrote and removes its own record: the service is not stopped, moved or changed.",
                ),
            ],
            Self::ServicePublish => vec![row("Name", text("name")), row("Reach", text("reach"))],
            Self::ServiceConnect | Self::ServiceDisconnect => {
                vec![row("Service", text("from")), row("To", text("to"))]
            }
            Self::PostgresCreate => {
                vec![row("Database", text("name")), row("Image", text("image"))]
            }
            Self::GpuAttach | Self::GpuDetach => {
                let instance = text("instance");
                let project = text("project");
                let kind = text("kind");
                let pci = text("pci");
                let gpu = match gpu_product.filter(|product| !product.is_empty()) {
                    Some(product) => format!("{product} ({pci})"),
                    None => format!("GPU at {pci}"),
                };
                let shown_instance = if kind.is_empty() {
                    instance.to_string()
                } else {
                    format!("{instance} ({kind})")
                };
                let effect = match (self, kind) {
                    (Self::GpuAttach, "virtual-machine") => format!(
                        "{instance} takes the whole card while it runs; the host and every container lose it until {instance} stops."
                    ),
                    (Self::GpuAttach, _) => format!(
                        "Adds the card to {instance} as a physical GPU device. Its memory is shared with every other user of the card, not isolated."
                    ),
                    (_, "virtual-machine") => format!(
                        "Removes the card from {instance}; the VM no longer takes the whole card when it runs."
                    ),
                    _ => format!(
                        "Removes the card from {instance}; the container no longer sees it."
                    ),
                };
                vec![
                    row("Project", project),
                    row("Instance", &shown_instance),
                    row("GPU", &gpu),
                    row("Effect", &effect),
                ]
            }
            Self::StorageAdd | Self::SnapshotCreate | Self::SnapshotDelete => {
                crate::storage::details(self, args)
            }
            Self::InstanceStart | Self::InstanceStop | Self::InstanceRestart => {
                let instance = text("instance");
                let effect = match self {
                    Self::InstanceStart => {
                        format!("Starts {instance}. If it already runs, nothing changes.")
                    }
                    Self::InstanceStop => format!(
                        "Shuts {instance} down; everything it serves is unavailable until it is started again. If it is already stopped, nothing changes."
                    ),
                    _ => format!(
                        "Shuts {instance} down and starts it again; everything it serves is unavailable for a moment. If it is stopped, it is started."
                    ),
                };
                vec![
                    row("Project", text("project")),
                    row("Instance", instance),
                    row("Effect", &effect),
                ]
            }
            Self::TenantCreate => {
                let mut rows = Vec::new();
                if args.get("id").is_some() {
                    rows.push(row("Tenant", text("id")));
                }
                let caps = &args["caps"];
                if caps.is_object() {
                    let number = |key: &str| caps[key].as_u64().unwrap_or(0).to_string();
                    rows.push(row("Instances", &number("instances")));
                    rows.push(row("Containers", &number("containers")));
                    rows.push(row("Virtual machines", &number("virtualMachines")));
                    rows.push(row("CPUs", &number("cpu")));
                    rows.push(row("Memory", caps["memory"].as_str().unwrap_or_default()));
                    rows.push(row("Disk", caps["disk"].as_str().unwrap_or_default()));
                } else {
                    rows.push(row("Caps", "this node's defaults"));
                }
                rows
            }
            Self::TenantRemove => vec![
                row("Tenant", text("id")),
                row(
                    "Effect",
                    "Deletes every service of this tenant, with its data, and then the tenant's project.",
                ),
            ],
            Self::DeployRemove => vec![
                row("Deploy", text("deploy")),
                // What the deploy holds as the proposal is made: the owner sees each part he approves the removal of.
                row("Parts", gpu_product.unwrap_or("read when the removal runs")),
                row(
                    "Effect",
                    "Deletes every service this deploy created in the tenant, with its data. Nothing else of the tenant is touched.",
                ),
            ],
        };
        // An action inside a tenant says so.
        let mut rows = rows;
        if !matches!(self, Self::TenantCreate | Self::TenantRemove)
            && let Some(tenant) = args.get("tenant").and_then(Value::as_str)
        {
            rows.push(row("Tenant", tenant));
        }
        // A create inside a deploy says so: the deploy is part of what the approval binds.
        if self != Self::DeployRemove
            && let Some(deploy) = args.get("deploy").and_then(Value::as_str)
        {
            rows.push(row("Deploy", deploy));
        }
        rows
    }

    /// A sentence in the owner's words, built only from fields the schema has already checked. It names the arguments
    /// that decide what the action does; the exact values are in [`ActionKind::details`].
    pub fn summary(self, args: &Value) -> String {
        let text = |key: &str| args[key].as_str().unwrap_or_default();
        let image = |key: &str| {
            text(key)
                .split_once('@')
                .map_or_else(|| text(key), |(reference, _)| reference)
                .to_string()
        };
        let sentence = match self {
            // Only a virtual machine on a host bridge names a bridge, and a proposal for one always does: a bridge left out
            // on a bridged node is filled in when the proposal is made.
            Self::ServiceCreate if args.get("bridge").is_some() => format!(
                "Create the virtual machine {} from template {} on the host bridge {}",
                text("name"),
                text("template"),
                text("bridge")
            ),
            Self::ServiceCreate => format!(
                "Create the service {} from template {}",
                text("name"),
                text("template")
            ),
            Self::ServiceRemove => format!("Remove the service {}", text("name")),
            Self::ServiceImport => format!(
                "Import the service {}, which another manager created, where it stands",
                text("name")
            ),
            Self::ServiceRelease => format!(
                "Release the imported service {} back to the manager that created it",
                text("name")
            ),
            Self::ServicePublish => {
                format!(
                    "Publish the service {} on the owner's tailnet",
                    text("name")
                )
            }
            Self::ServiceUnpublish => format!("Unpublish the service {}", text("name")),
            Self::ServiceConnect => {
                format!("Connect the service {} to {}", text("from"), text("to"))
            }
            Self::ServiceDisconnect => format!(
                "Disconnect the service {} from {}",
                text("from"),
                text("to")
            ),
            Self::PostgresCreate => format!(
                "Create the database {} from {}",
                text("name"),
                image("image")
            ),
            Self::GpuAttach if !text("project").is_empty() => format!(
                "Attach the GPU at {} to instance {} in project {}",
                text("pci"),
                text("instance"),
                text("project")
            ),
            Self::GpuAttach => format!(
                "Attach the GPU at {} to instance {}",
                text("pci"),
                text("instance")
            ),
            Self::GpuDetach if !text("project").is_empty() => format!(
                "Detach the GPU at {} from instance {} in project {}",
                text("pci"),
                text("instance"),
                text("project")
            ),
            Self::GpuDetach => format!(
                "Detach the GPU at {} from instance {}",
                text("pci"),
                text("instance")
            ),
            Self::StorageAdd | Self::SnapshotCreate | Self::SnapshotDelete => {
                crate::storage::summary(self, args)
            }
            Self::InstanceStart | Self::InstanceStop | Self::InstanceRestart => {
                let word = match self {
                    Self::InstanceStart => "Start",
                    Self::InstanceStop => "Stop",
                    _ => "Restart",
                };
                format!(
                    "{word} the instance {} in project {}",
                    text("instance"),
                    text("project")
                )
            }
            Self::TenantCreate if args.get("caps").is_some() => {
                "Create a tenant with caps above this node's defaults".to_string()
            }
            Self::TenantCreate => "Create a tenant".to_string(),
            Self::TenantRemove => format!("Remove the tenant {} and everything in it", text("id")),
            Self::DeployRemove => {
                format!("Remove everything the deploy {} created", text("deploy"))
            }
        };
        // A service that can call the kernel is named as one.
        let sentence = if self == Self::ServiceCreate && args.get("kernelAccess").is_some() {
            format!("{sentence} with access to this node's kernel")
        } else {
            sentence
        };
        // A service created stopped is named as one: nothing of it runs until it is started.
        let sentence =
            if self == Self::ServiceCreate && args.get("start") == Some(&Value::Bool(false)) {
                format!("{sentence} and leave it stopped")
            } else {
                sentence
            };
        match args.get("tenant").and_then(Value::as_str) {
            Some(tenant) if !matches!(self, Self::TenantCreate | Self::TenantRemove) => {
                format!("{sentence}, in tenant {tenant}")
            }
            _ => sentence,
        }
    }
}

/// The `Kernel access` row of a `service_create` whose template declares `kernelAccess`, in the owner's words.
pub fn kernel_access_words(port: u64) -> String {
    format!(
        "Yes: this service can call this node's kernel from inside, at http://127.0.0.1:{port}/mcp, with whatever token it is given. The token's scope and tool list decide what it may do, and a call without a token is refused."
    )
}

/// What a `service_create` proposal's `start` means, in the owner's words.
pub fn start_words(start: bool) -> &'static str {
    if start {
        "Yes: it is started once it is created"
    } else {
        "No: it is created stopped, and instance_start starts it later"
    }
}

/// The `Start` row of a `service_create` whose template is created stopped whatever the call says: `reason` finishes
/// "it is created stopped because".
pub fn stopped_words(reason: &str) -> String {
    format!("{STOPPED_BECAUSE}{reason}; instance_start starts it later")
}

/// How every [`stopped_words`] row begins.
const STOPPED_BECAUSE: &str = "No: it is created stopped because ";

/// One argument the owner is shown, as a label and its exact value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionDetail {
    pub label: String,
    pub value: String,
}

/// The digest an action proposal and its statement name: SHA-256 of the canonical `{ "action": ..., "args": ... }`.
pub fn action_args_digest(action: &str, args: &Value) -> Option<String> {
    let canonical = canonical_json(&json!({ "action": action, "args": args })).ok()?;
    Some(Digest::of(canonical.as_bytes()).to_hex())
}

/// The action id: `act-` and the first twelve hex digits of [`action_args_digest`].
pub fn action_id(digest: &str) -> String {
    let prefix = digest.get(..12).unwrap_or(digest);
    format!("act-{prefix}")
}

/// A waiting action proposal, exactly as a listing carries it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionProposal {
    pub id: String,
    pub action: String,
    pub args: Value,
    pub args_digest: String,
    pub created_ms: u64,
    pub expires_at: u64,
    pub summary: String,
    #[serde(default)]
    pub details: Vec<ActionDetail>,
}

impl ActionProposal {
    /// Whether the proposal's fifteen minutes have run.
    pub fn expired(&self, now: u64) -> bool {
        now > self.expires_at
    }
}

/// The summary and details to show for a proposal read from the journal. A GPU proposal's product is not in its arguments:
/// the inventory that named it at proposal time may be gone when the kernel next opens. The journal is the kernel's own
/// private file, so its words are kept for a GPU action when they still carry the proposal's instance and PCI address;
/// otherwise the rows are rebuilt without a product. Every other action is rebuilt from its arguments, as before.
fn loaded_words(
    action: ActionKind,
    args: &Value,
    stored_summary: &str,
    stored_details: &[ActionDetail],
) -> (String, Vec<ActionDetail>) {
    // The parts of a deploy's removal were read from the desired state when it was proposed, and are not in its
    // arguments either: the journal's own rows are kept when they are the rows this action writes for these arguments.
    if action == ActionKind::DeployRemove {
        let rebuilt = action.details(args);
        let fits = stored_details.len() == rebuilt.len()
            && stored_details.iter().zip(&rebuilt).all(|(stored, built)| {
                stored.label == built.label
                    && (stored.label == "Parts" || stored.value == built.value)
            });
        return (
            action.summary(args),
            if fits {
                stored_details.to_vec()
            } else {
                rebuilt
            },
        );
    }
    // Why a template is created stopped was read from the template when the create was proposed, and is not in its
    // arguments either: the journal's `Start` row is kept when it is such a row and every other row is the one this
    // action writes for these arguments.
    if action == ActionKind::ServiceCreate {
        let rebuilt = action.details(args);
        let without_start = |rows: &[ActionDetail]| -> Vec<ActionDetail> {
            rows.iter()
                .filter(|row| row.label != "Start")
                .cloned()
                .collect()
        };
        let stored_start = stored_details.iter().find(|row| row.label == "Start");
        let fits = args.get("start") != Some(&Value::Bool(false))
            && stored_start.is_some_and(|row| row.value.starts_with(STOPPED_BECAUSE))
            && without_start(stored_details) == without_start(&rebuilt);
        return (
            action.summary(args),
            if fits {
                stored_details.to_vec()
            } else {
                rebuilt
            },
        );
    }
    if !matches!(action, ActionKind::GpuAttach | ActionKind::GpuDetach) {
        return (action.summary(args), action.details(args));
    }
    let instance = args["instance"].as_str().unwrap_or_default();
    let project = args["project"].as_str().unwrap_or_default();
    let pci = args["pci"].as_str().unwrap_or_default();
    let rows_fit = stored_details.len() == 4
        && stored_details[0].label == "Project"
        && stored_details[0].value == project
        && stored_details[1].label == "Instance"
        && stored_details[1].value.starts_with(instance)
        && stored_details[2].label == "GPU"
        && stored_details[2].value.contains(pci)
        && stored_details[3].label == "Effect";
    if rows_fit && stored_summary.contains(pci) && stored_summary.contains(instance) {
        (stored_summary.to_string(), stored_details.to_vec())
    } else {
        (action.summary(args), action.details(args))
    }
}

/// One line of the proposals journal.
#[derive(Debug, Serialize, Deserialize)]
struct ActionEvent {
    event: String,
    #[serde(default)]
    proposal: Option<ActionProposal>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    ok: Option<bool>,
    #[serde(default)]
    code: Option<String>,
    /// Why an approved action did not run, in the words its caller was given. Absent on every other event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

/// The longest refusal message a result event keeps, in characters.
const RESULT_MESSAGE_MAX: usize = 1024;

/// The waiting proposals and the journal they are written to.
pub struct ActionProposalStore {
    path: PathBuf,
    waiting: Vec<ActionProposal>,
}

/// Why a proposal was not recorded. A refusal is not a kernel failure.
#[derive(Debug)]
pub enum ProposeError {
    /// Eight unexpired proposals already wait.
    TooMany,
    /// The first twelve digits of the digest already name another proposal.
    IdCollision,
    /// The journal could not be made durable.
    Store(io::Error),
}

impl ActionProposalStore {
    /// Opens the journal, quarantines an unreadable, unsafe or invalid one, keeps only waiting unexpired proposals and
    /// rewrites the file atomically. Every proposed record is re-checked against its action's validation and the kernel's
    /// configured `postgres_image`; the journal never supplies that image itself. No failure here stops the kernel: a file
    /// that cannot be read or rewritten is logged and the store starts empty or keeps what it read.
    pub fn open(state_dir: &Path, now: u64, postgres_image: Option<&str>) -> Self {
        let path = state_dir.join(ACTION_PROPOSALS_FILE);
        Self::remove_stale_temps(state_dir);
        let waiting = match Self::read(&path, postgres_image) {
            Ok(waiting) => waiting,
            Err(reason) => {
                Self::quarantine(&path, now, &reason);
                Vec::new()
            }
        };
        let mut store = Self { path, waiting };
        store.waiting.retain(|proposal| !proposal.expired(now));
        store.compact();
        store
    }

    /// The waiting proposals, in the order they were made.
    pub fn waiting(&self) -> &[ActionProposal] {
        &self.waiting
    }

    /// The waiting proposal with `id`.
    pub fn get(&self, id: &str) -> Option<&ActionProposal> {
        self.waiting.iter().find(|proposal| proposal.id == id)
    }

    /// Records a new proposal unless its digest waits already, the same digest waits twice, or eight wait.
    pub fn propose(
        &mut self,
        action: ActionKind,
        args: Value,
        now: u64,
        gpu_product: Option<&str>,
    ) -> Result<ActionProposal, ProposeError> {
        self.waiting.retain(|proposal| !proposal.expired(now));
        let digest = action_args_digest(action.as_str(), &args).ok_or_else(|| {
            ProposeError::Store(io::Error::other("arguments do not canonicalize"))
        })?;
        if let Some(existing) = self
            .waiting
            .iter()
            .find(|proposal| proposal.args_digest == digest)
        {
            return Ok(existing.clone());
        }
        if self.waiting.len() >= MAX_WAITING {
            return Err(ProposeError::TooMany);
        }
        let id = action_id(&digest);
        if self.waiting.iter().any(|proposal| proposal.id == id) {
            return Err(ProposeError::IdCollision);
        }
        let proposal = ActionProposal {
            id,
            action: action.as_str().to_string(),
            summary: action.summary(&args),
            details: action.details_with_gpu(&args, gpu_product),
            args,
            args_digest: digest,
            created_ms: now,
            expires_at: now.saturating_add(PLAN_TTL_MS),
        };
        self.append(ActionEvent {
            event: "proposed".to_string(),
            proposal: Some(proposal.clone()),
            id: None,
            ok: None,
            code: None,
            message: None,
        })
        .map_err(ProposeError::Store)?;
        self.waiting.push(proposal.clone());
        self.compact_if_large();
        Ok(proposal)
    }

    /// Journals an approval and retires the proposal. The caller has already checked its proof.
    pub fn approve(&mut self, id: &str) -> io::Result<Option<ActionProposal>> {
        self.retire(id, "approved")
    }

    /// Journals a decline and retires the proposal.
    pub fn decline(&mut self, id: &str) -> io::Result<Option<ActionProposal>> {
        self.retire(id, "declined")
    }

    fn retire(&mut self, id: &str, event: &str) -> io::Result<Option<ActionProposal>> {
        let Some(index) = self.waiting.iter().position(|proposal| proposal.id == id) else {
            return Ok(None);
        };
        self.append(ActionEvent {
            event: event.to_string(),
            proposal: None,
            id: Some(id.to_string()),
            ok: None,
            code: None,
            message: None,
        })?;
        let retired = self.waiting.remove(index);
        self.compact_if_large();
        Ok(Some(retired))
    }

    /// Journals the result of one approved action: whether it ran, and the refusal code and message when it did not.
    pub fn journal_result(
        &mut self,
        id: &str,
        ok: bool,
        code: Option<&str>,
        message: Option<&str>,
    ) -> io::Result<()> {
        self.append(ActionEvent {
            event: "result".to_string(),
            proposal: None,
            id: Some(id.to_string()),
            ok: Some(ok),
            code: code.map(str::to_string),
            message: message.map(|message| message.chars().take(RESULT_MESSAGE_MAX).collect()),
        })?;
        self.compact_if_large();
        Ok(())
    }

    /// Reads the journal into the proposals that were waiting at its end. Every proposed record is validated again, now,
    /// with the kernel's configured pinned image: stored arguments are never trusted on their own. Any unsafe file,
    /// oversized file or malformed line is a reason to quarantine the whole file, except a malformed last line without its
    /// trailing newline, which a crash can leave torn; that line is dropped and the compaction writes the rest.
    fn read(path: &Path, postgres_image: Option<&str>) -> Result<Vec<ActionProposal>, String> {
        let meta = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.to_string()),
        };
        if !meta.is_file() || meta.file_type().is_symlink() {
            return Err("it is not a regular file".to_string());
        }
        if meta.permissions().mode() & 0o077 != 0 {
            return Err("it is not mode 0600".to_string());
        }
        if meta.len() > MAX_JOURNAL_BYTES {
            return Err("it is larger than 16 MiB".to_string());
        }
        let text = fs::read_to_string(path).map_err(|error| error.to_string())?;
        let lines: Vec<&str> = text.lines().collect();
        let last_line_is_torn = !text.ends_with('\n');
        let mut waiting: Vec<ActionProposal> = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let event: ActionEvent = match serde_json::from_str(line) {
                Ok(event) => event,
                // A crash can leave the last write half-done. Only a line with no trailing newline is that torn tail; a
                // complete but malformed line, and a malformed line anywhere else, still quarantine the file.
                Err(_) if last_line_is_torn && index + 1 == lines.len() => break,
                Err(_) => return Err("it holds a line that is not an event".to_string()),
            };
            match event.event.as_str() {
                "proposed" => {
                    let proposal = event
                        .proposal
                        .ok_or_else(|| "a proposed event holds no proposal".to_string())?;
                    let action = ActionKind::from_name(&proposal.action).ok_or_else(|| {
                        "a proposed event names an action this build lacks".to_string()
                    })?;
                    let args = action
                        .prepare(&proposal.args, postgres_image)
                        .map_err(|why| format!("a proposed event fails its validation: {why}"))?;
                    let digest = action_args_digest(action.as_str(), &args).ok_or_else(|| {
                        "a proposed event's arguments do not canonicalize".to_string()
                    })?;
                    if proposal.id != action_id(&digest) || proposal.args_digest != digest {
                        return Err("a proposed event does not match its arguments".to_string());
                    }
                    let (summary, details) =
                        loaded_words(action, &args, &proposal.summary, &proposal.details);
                    waiting.push(ActionProposal {
                        id: proposal.id,
                        action: action.as_str().to_string(),
                        summary,
                        details,
                        args,
                        args_digest: digest,
                        created_ms: proposal.created_ms,
                        expires_at: proposal.expires_at,
                    });
                }
                "approved" | "declined" | "result" => {
                    if let Some(id) = event.id {
                        waiting.retain(|proposal| proposal.id != id);
                    }
                }
                _ => return Err("it holds an unknown event".to_string()),
            }
        }
        Ok(waiting)
    }

    /// Rewrites the journal with only the waiting proposals, through a private temporary file and a rename.
    fn compact(&mut self) {
        if !self.path.exists() {
            return;
        }
        let temp = self.path.with_file_name(compaction_temp_name());
        let result = (|| -> io::Result<()> {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&temp)?;
            for proposal in &self.waiting {
                let mut bytes = serde_json::to_vec(&ActionEvent {
                    event: "proposed".to_string(),
                    proposal: Some(proposal.clone()),
                    id: None,
                    ok: None,
                    code: None,
                    message: None,
                })
                .map_err(io::Error::other)?;
                bytes.push(b'\n');
                file.write_all(&bytes)?;
            }
            file.sync_all()?;
            fs::rename(&temp, &self.path)?;
            if let Some(dir) = self.path.parent()
                && let Ok(dir) = fs::File::open(dir)
            {
                let _ = dir.sync_all();
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
            eprintln!("arca: the action proposals journal could not be compacted");
        }
    }

    /// Compacts the journal in place once it has outgrown [`MAX_COMPACT_BYTES`]. A failed compaction is logged and the
    /// journal keeps its events, so the next append tries again.
    fn compact_if_large(&mut self) {
        let large = fs::metadata(&self.path).is_ok_and(|meta| meta.len() > MAX_COMPACT_BYTES);
        if large {
            self.compact();
        }
    }

    /// Removes temporary files this module's compaction left behind when a crash interrupted it. Only the exact private
    /// naming is touched, and only regular files: a directory entry or a link that merely looks similar is left alone. A
    /// reused process id must not make the next compaction collide with a stale temporary.
    fn remove_stale_temps(state_dir: &Path) {
        let Ok(entries) = fs::read_dir(state_dir) else {
            return;
        };
        let mut removed = 0usize;
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if file_type.is_file()
                && is_compaction_temp(name)
                && fs::remove_file(entry.path()).is_ok()
            {
                removed = removed.saturating_add(1);
            }
        }
        if removed > 0 {
            eprintln!("arca: removed {removed} stale action proposals journal temporary file(s)");
        }
    }

    /// Renames an untrusted journal aside and reports whether the path is clear for a new file.
    fn quarantine(path: &Path, now: u64, reason: &str) -> bool {
        let base = path.with_file_name(format!("{ACTION_PROPOSALS_FILE}.bad-{now}"));
        let mut target = base.clone();
        let mut index = 1u32;
        while target.exists() {
            target = base.with_file_name(format!("{ACTION_PROPOSALS_FILE}.bad-{now}-{index}"));
            index = index.saturating_add(1);
        }
        match fs::rename(path, &target) {
            Ok(()) => {
                eprintln!(
                    "arca: the action proposals journal was quarantined as {}: {reason}",
                    target.display()
                );
                true
            }
            Err(error) => {
                eprintln!("arca: the action proposals journal could not be quarantined: {error}");
                false
            }
        }
    }

    fn append(&mut self, event: ActionEvent) -> io::Result<()> {
        if let Ok(meta) = fs::symlink_metadata(&self.path)
            && (!meta.is_file()
                || meta.file_type().is_symlink()
                || meta.permissions().mode() & 0o077 != 0)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the proposals journal is not a private regular file",
            ));
        }
        let mut bytes = serde_json::to_vec(&event).map_err(io::Error::other)?;
        bytes.push(b'\n');
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .mode(0o600)
            .open(&self.path)?;
        // A crash can leave the last line torn without its newline. Start the new event on a fresh line, so the torn tail
        // cannot glue itself to it and the line stays readable.
        if journal_tail_needs_newline(&mut file)? {
            file.write_all(b"\n")?;
        }
        file.write_all(&bytes)?;
        file.sync_all()
    }
}

/// Whether the journal's last byte is not a newline, so the next event must start on a fresh line. An empty journal needs
/// no leading newline.
fn journal_tail_needs_newline(file: &mut fs::File) -> io::Result<bool> {
    use std::io::{Read as _, Seek as _, SeekFrom};

    if file.metadata()?.len() == 0 {
        return Ok(false);
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last)?;
    Ok(last[0] != b'\n')
}

fn monotonic_nonce() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NONCE: AtomicU64 = AtomicU64::new(0);
    NONCE.fetch_add(1, Ordering::Relaxed)
}

/// The private temporary file one compaction writes before its rename. A crash can leave one behind.
fn compaction_temp_name() -> String {
    format!(
        ".{ACTION_PROPOSALS_FILE}.tmp-{}-{:x}",
        std::process::id(),
        monotonic_nonce()
    )
}

/// Whether `name` is exactly one of this module's compaction temporary files. The process id and the nonce are the only
/// variable parts, and the nonce is lower-case hex.
fn is_compaction_temp(name: &str) -> bool {
    let Some(rest) = name.strip_prefix(&format!(".{ACTION_PROPOSALS_FILE}.tmp-")) else {
        return false;
    };
    let Some((pid, nonce)) = rest.split_once('-') else {
        return false;
    };
    !pid.is_empty()
        && pid.bytes().all(|byte| byte.is_ascii_digit())
        && !nonce.is_empty()
        && nonce
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// A per-token sliding window over newly recorded proposals. The window is in memory; a restart forgets it.
#[derive(Default)]
pub struct ProposalRateLimiter {
    seen: Mutex<HashMap<String, VecDeque<u64>>>,
}

impl ProposalRateLimiter {
    /// Reserves one place in `token`'s window, or refuses when the token already made the window's maximum. The caller
    /// reserves only for a proposal that will be recorded as new; a deduplicated or refused proposal must refund or never
    /// reserve, so it counts nothing.
    pub fn allow(&self, token: &str, now: u64) -> bool {
        let Ok(mut seen) = self.seen.lock() else {
            return false;
        };
        let window = seen.entry(token.to_string()).or_default();
        while window
            .front()
            .is_some_and(|at| now.saturating_sub(*at) >= PROPOSAL_RATE_WINDOW_MS)
        {
            window.pop_front();
        }
        if window.len() >= PROPOSAL_RATE_MAX {
            return false;
        }
        window.push_back(now);
        true
    }

    /// Gives back the place [`Self::allow`] just reserved when the proposal was refused after all, so a refusal never
    /// counts toward the window. It removes only a place taken at `now`; an older one stays.
    pub fn refund(&self, token: &str, now: u64) {
        let Ok(mut seen) = self.seen.lock() else {
            return;
        };
        let Some(window) = seen.get_mut(token) else {
            return;
        };
        if window.back() == Some(&now) {
            window.pop_back();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    struct Temp(PathBuf);

    impl Temp {
        fn new(tag: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("the clock is after the epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "arca-proposals-{}-{nanos}-{tag}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("the temporary directory");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("mode 0700");
            Self(path)
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn create_args(name: &str) -> Value {
        json!({ "name": name, "template": "nats" })
    }

    /// A proposed journal line whose id and digest match its arguments, as the store writes them.
    fn proposed_line(action: &str, args: &Value, now: u64) -> String {
        let kind = ActionKind::from_name(action).expect("a known action");
        let digest = action_args_digest(action, args).expect("canonical arguments");
        serde_json::to_string(&json!({
            "event": "proposed",
            "proposal": {
                "id": action_id(&digest),
                "action": action,
                "args": args,
                "argsDigest": digest,
                "createdMs": now,
                "expiresAt": now.saturating_add(PLAN_TTL_MS),
                "summary": kind.summary(args),
                "details": kind.details(args),
            }
        }))
        .expect("a journal line")
    }

    fn write_journal(dir: &Path, text: &str) -> PathBuf {
        let path = dir.join(ACTION_PROPOSALS_FILE);
        fs::write(&path, text).expect("the journal");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("mode 0600");
        path
    }

    fn quarantined(dir: &Path) -> bool {
        fs::read_dir(dir)
            .expect("the directory")
            .filter_map(Result::ok)
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&format!("{ACTION_PROPOSALS_FILE}.bad-"))
            })
    }

    /// A digest-pinned image distinct from [`OTHER_IMAGE`].
    const PINNED_IMAGE: &str = "docker.io/library/postgres@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    /// Another pinned image, as a kernel configured differently would name.
    const OTHER_IMAGE: &str = "docker.io/library/postgres@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn a_twenty_mib_journal_is_quarantined_and_the_store_starts_empty() {
        let dir = Temp::new("large");
        let path = dir.0.join(ACTION_PROPOSALS_FILE);
        let mut file = fs::File::create(&path).expect("the journal");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("mode 0600");
        let chunk = vec![b'x'; 64 * 1024];
        for _ in 0..(20 * 1024 * 1024 / chunk.len()) {
            file.write_all(&chunk).expect("the large journal");
        }
        file.sync_all().expect("sync");
        drop(file);

        let store = ActionProposalStore::open(&dir.0, 1_000, None);
        assert!(store.waiting().is_empty(), "the large file was not read");
        assert!(!path.exists(), "the large file was moved aside");
        let quarantined = fs::read_dir(&dir.0)
            .expect("the directory")
            .filter_map(Result::ok)
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("action-proposals.jsonl.bad-")
            });
        assert!(quarantined, "the large file was quarantined");
    }

    #[test]
    fn opening_compacts_to_waiting_unexpired_proposals() {
        let dir = Temp::new("compact");
        let mut store = ActionProposalStore::open(&dir.0, 1_000, None);
        let first = store
            .propose(ActionKind::ServiceCreate, create_args("first"), 1_000, None)
            .expect("the first proposal");
        let second = store
            .propose(
                ActionKind::ServiceCreate,
                create_args("second"),
                1_000,
                None,
            )
            .expect("the second proposal");
        store.approve(&first.id).expect("the approval event");
        drop(store);

        let store = ActionProposalStore::open(&dir.0, 2_000, None);
        assert_eq!(store.waiting().len(), 1);
        assert_eq!(store.waiting()[0].id, second.id);
        let text =
            fs::read_to_string(dir.0.join(ACTION_PROPOSALS_FILE)).expect("the compacted journal");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 1, "only the waiting proposal is kept");
        assert!(lines[0].contains(&second.id));
        assert!(!lines[0].contains(&first.id));
        assert!(!text.contains("approved"));
        drop(store);

        let store = ActionProposalStore::open(&dir.0, second.expires_at + 1, None);
        assert!(store.waiting().is_empty(), "the lapsed proposal is dropped");
        assert_eq!(
            fs::read_to_string(dir.0.join(ACTION_PROPOSALS_FILE)).expect("the compacted journal"),
            ""
        );
    }

    #[test]
    fn a_gpu_product_survives_a_restart_without_the_inventory() {
        let dir = Temp::new("gpu-product");
        let mut store = ActionProposalStore::open(&dir.0, 1_000, None);
        let proposed = store
            .propose(
                ActionKind::GpuAttach,
                json!({
                    "project": "arcak-gpu", "instance": "alpha", "kind": "container",
                    "uuid": "00000000-0000-4000-8000-0000000000a1", "pci": "0000:02:00.0"
                }),
                1_000,
                Some("NVIDIA GeForce RTX 4060 Ti"),
            )
            .expect("a GPU proposal");
        assert_eq!(
            proposed.details[2],
            ActionDetail {
                label: "GPU".to_string(),
                value: "NVIDIA GeForce RTX 4060 Ti (0000:02:00.0)".to_string(),
            }
        );
        drop(store);

        // A restart without an inventory still shows the product the owner approved; the journal is the kernel's own.
        let store = ActionProposalStore::open(&dir.0, 2_000, None);
        assert_eq!(store.waiting().len(), 1);
        assert_eq!(store.waiting()[0].details, proposed.details);
        assert_eq!(store.waiting()[0].summary, proposed.summary);
        assert!(store.waiting()[0].details[3].value.contains("shared"));
    }

    #[test]
    fn a_replayed_proposal_is_validated_again_and_a_failed_one_quarantines() {
        let dir = Temp::new("replay-invalid");
        // The id and digest match these arguments, so only the action's own validation can refuse them.
        let line = proposed_line(
            "service_create",
            &json!({ "name": "BAD NAME", "template": "nats" }),
            1_000,
        );
        let path = write_journal(&dir.0, &format!("{line}\n"));

        let store = ActionProposalStore::open(&dir.0, 1_000, None);
        assert!(
            store.waiting().is_empty(),
            "an invalid proposal is not accepted from the journal"
        );
        assert!(!path.exists(), "the journal was moved aside");
        assert!(quarantined(&dir.0), "the invalid journal was quarantined");
    }

    #[test]
    fn a_creates_stopped_row_is_kept_on_replay_only_when_it_is_such_a_row_for_these_arguments() {
        let reason = "its template needs the secret TUNNEL_TOKEN, which is not set yet";
        let action = ActionKind::ServiceCreate;
        // Proposed with the template's reason: the journal's rows carry it, and a replay keeps them.
        let args = json!({ "name": "tunnel", "template": "tunnel" });
        let stored = action.details_with_gpu(&args, Some(reason));
        assert_eq!(stored.last().map(|row| row.label.as_str()), Some("Start"));
        assert_eq!(stored.last().unwrap().value, stopped_words(reason));
        assert_eq!(loaded_words(action, &args, "", &stored).1, stored);
        // The same with `start: true`: "Yes" is never shown for a template that is created stopped.
        let named = json!({ "name": "tunnel", "template": "tunnel", "start": true });
        let stored_named = action.details_with_gpu(&named, Some(reason));
        assert_eq!(stored_named.last().unwrap().value, stopped_words(reason));
        assert_eq!(
            loaded_words(action, &named, "", &stored_named).1,
            stored_named
        );
        // Rows that do not fit the arguments, or a Start row in other words, are rebuilt from the arguments alone.
        let other = json!({ "name": "other", "template": "tunnel" });
        assert_eq!(
            loaded_words(action, &other, "", &stored).1,
            action.details(&other)
        );
        let mut forged = stored.clone();
        forged.last_mut().unwrap().value = "Yes: anything".to_string();
        assert_eq!(
            loaded_words(action, &args, "", &forged).1,
            action.details(&args)
        );
        // The call's own `start: false` is in the arguments, and its row is always rebuilt from them.
        let asked = json!({ "name": "tunnel", "template": "tunnel", "start": false });
        let rebuilt = loaded_words(
            action,
            &asked,
            "",
            &action.details_with_gpu(&asked, Some(reason)),
        )
        .1;
        assert_eq!(rebuilt, action.details(&asked));
        assert_eq!(rebuilt.last().unwrap().value, start_words(false));
    }

    #[test]
    fn the_kernels_configured_image_is_the_authority_on_replay() {
        let dir = Temp::new("replay-image");
        let args = json!({ "name": "orders", "image": PINNED_IMAGE });
        let line = proposed_line("postgres_create", &args, 1_000);
        let path = write_journal(&dir.0, &format!("{line}\n"));

        // The configured image matches the journal's, so the proposal waits and carries the configured image.
        let store = ActionProposalStore::open(&dir.0, 1_000, Some(PINNED_IMAGE));
        assert_eq!(store.waiting().len(), 1);
        assert_eq!(store.waiting()[0].args["image"], json!(PINNED_IMAGE));
        drop(store);

        // A restart configured with another pinned image must not trust the copy the journal carries.
        let store = ActionProposalStore::open(&dir.0, 1_000, Some(OTHER_IMAGE));
        assert!(
            store.waiting().is_empty(),
            "a different configured image refuses the replayed proposal"
        );
        assert!(!path.exists());
        assert!(quarantined(&dir.0));

        // Nor does a kernel with no configured image read one out of the journal.
        let path = write_journal(&dir.0, &format!("{line}\n"));
        let store = ActionProposalStore::open(&dir.0, 1_000, None);
        assert!(
            store.waiting().is_empty(),
            "the journal's image is never the configured default"
        );
        assert!(!path.exists());
    }

    #[test]
    fn a_torn_last_line_without_its_newline_is_dropped_and_the_journal_compacted() {
        let dir = Temp::new("torn-tail");
        let valid = proposed_line("service_create", &create_args("nats"), 1_000);
        let path = write_journal(
            &dir.0,
            &format!("{valid}\n{{\"event\":\"proposed\",\"proposal\":{{"),
        );

        let store = ActionProposalStore::open(&dir.0, 1_000, None);
        assert_eq!(store.waiting().len(), 1, "the complete event survives");
        assert!(!quarantined(&dir.0), "a torn tail does not quarantine");
        let text = fs::read_to_string(&path).expect("the compacted journal");
        assert_eq!(text.lines().count(), 1, "the torn tail was dropped");
        assert!(text.ends_with('\n'), "the rewrite terminates every line");
    }

    #[test]
    fn a_malformed_line_before_the_last_still_quarantines() {
        let dir = Temp::new("malformed-middle");
        let valid = proposed_line("service_create", &create_args("nats"), 1_000);
        let path = write_journal(&dir.0, &format!("{valid}\nthis is not an event\n{valid}\n"));

        let store = ActionProposalStore::open(&dir.0, 1_000, None);
        assert!(store.waiting().is_empty());
        assert!(!path.exists());
        assert!(quarantined(&dir.0));
    }

    #[test]
    fn a_malformed_last_line_with_its_newline_quarantines() {
        let dir = Temp::new("malformed-last");
        let valid = proposed_line("service_create", &create_args("nats"), 1_000);
        let path = write_journal(&dir.0, &format!("{valid}\nthis is not an event\n"));

        let store = ActionProposalStore::open(&dir.0, 1_000, None);
        assert!(store.waiting().is_empty());
        assert!(!path.exists());
        assert!(quarantined(&dir.0));
    }

    #[test]
    fn opening_removes_only_its_own_stale_compaction_temps() {
        let dir = Temp::new("stale-temps");
        let stale = dir.0.join(format!(".{ACTION_PROPOSALS_FILE}.tmp-4242-0"));
        fs::write(&stale, b"half written").expect("a stale temp");
        let stale_hex = dir
            .0
            .join(format!(".{ACTION_PROPOSALS_FILE}.tmp-1-deadbeef"));
        fs::write(&stale_hex, b"half written").expect("a stale temp");
        let foreign = dir.0.join("notes.txt");
        fs::write(&foreign, b"keep").expect("a foreign file");
        let lookalike = dir
            .0
            .join(format!(".{ACTION_PROPOSALS_FILE}.tmp-not-a-pid-0"));
        fs::write(&lookalike, b"keep").expect("a lookalike file");
        let temp_dir = dir.0.join(format!(".{ACTION_PROPOSALS_FILE}.tmp-7-aa"));
        fs::create_dir(&temp_dir).expect("a directory with a similar name");

        let store = ActionProposalStore::open(&dir.0, 1_000, None);
        assert!(store.waiting().is_empty());
        assert!(!stale.exists(), "the exact temp naming is removed");
        assert!(
            !stale_hex.exists(),
            "another process id's temp is removed too"
        );
        assert!(foreign.exists(), "an unrelated file is never touched");
        assert!(
            lookalike.exists(),
            "a similar name that is not the exact naming stays"
        );
        assert!(temp_dir.exists(), "a directory is never removed");
    }

    #[test]
    fn the_journal_compacts_after_it_passes_its_size_threshold() {
        let dir = Temp::new("threshold");
        let path = dir.0.join(ACTION_PROPOSALS_FILE);
        let mut store = ActionProposalStore::open(&dir.0, 1_000, None);
        let mut appended = 0u64;
        let mut compacted = false;
        for index in 0..6_000u32 {
            let before = fs::metadata(&path).map_or(0, |meta| meta.len());
            let proposal = store
                .propose(
                    ActionKind::ServiceCreate,
                    create_args(&format!("bulk{index}")),
                    1_000,
                    None,
                )
                .expect("a proposal");
            let after = fs::metadata(&path).expect("the journal").len();
            if after < before {
                compacted = true;
                break;
            }
            appended = appended.saturating_add(after.saturating_sub(before));

            store.approve(&proposal.id).expect("the approval");
            let approved = fs::metadata(&path).expect("the journal").len();
            if approved < after {
                compacted = true;
                break;
            }
            appended = appended.saturating_add(approved.saturating_sub(after));
        }
        assert!(
            appended > MAX_COMPACT_BYTES / 2,
            "the journal grew through many appended events"
        );
        assert!(
            compacted,
            "an append compacted the journal once it passed its size threshold"
        );
        let text = fs::read_to_string(&path).expect("the compacted journal");
        assert_eq!(
            text.lines().count(),
            store.waiting().len(),
            "the journal holds exactly the waiting set"
        );
        for proposal in store.waiting() {
            assert!(text.contains(&proposal.id));
        }
    }

    #[test]
    fn an_append_after_a_torn_tail_starts_on_a_new_line() {
        let dir = Temp::new("torn-append");
        let first = proposed_line("service_create", &create_args("nats"), 1_000);
        // A complete last event whose terminating newline was never written.
        let path = write_journal(&dir.0, &first);
        let mut store = ActionProposalStore {
            path: path.clone(),
            waiting: Vec::new(),
        };
        let fresh = store
            .propose(ActionKind::ServiceCreate, create_args("fresh"), 2_000, None)
            .expect("the append");

        let text = fs::read_to_string(&path).expect("the journal");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines.len(),
            2,
            "the new event is not glued to the torn line"
        );
        assert_eq!(lines[0], first, "the torn line keeps its own content");
        assert!(lines[1].contains(&fresh.id));
    }

    #[test]
    fn the_per_token_window_allows_thirty_and_then_waits() {
        let limiter = ProposalRateLimiter::default();
        for index in 0..PROPOSAL_RATE_MAX as u64 {
            assert!(limiter.allow("one", 1_000 + index));
        }
        assert!(!limiter.allow("one", 1_000 + PROPOSAL_RATE_MAX as u64));
        assert!(
            limiter.allow("two", 1_000),
            "another token has its own window"
        );
        assert!(
            limiter.allow("one", 1_000 + PROPOSAL_RATE_WINDOW_MS),
            "the first entry left the window"
        );
    }
}
