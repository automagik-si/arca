//! Inert action proposals: a viewer may ask the owner to approve one of a closed set of service, database or GPU actions.
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
    ServicePublish,
    ServiceUnpublish,
    ServiceConnect,
    ServiceDisconnect,
    PostgresCreate,
    GpuAttach,
    GpuDetach,
}

impl ActionKind {
    /// Every allowed action, in the order the table documents them.
    pub const ALL: [ActionKind; 9] = [
        Self::ServiceCreate,
        Self::ServiceRemove,
        Self::ServicePublish,
        Self::ServiceUnpublish,
        Self::ServiceConnect,
        Self::ServiceDisconnect,
        Self::PostgresCreate,
        Self::GpuAttach,
        Self::GpuDetach,
    ];

    /// The verb name this action runs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ServiceCreate => "service_create",
            Self::ServiceRemove => "service_remove",
            Self::ServicePublish => "service_publish",
            Self::ServiceUnpublish => "service_unpublish",
            Self::ServiceConnect => "service_connect",
            Self::ServiceDisconnect => "service_disconnect",
            Self::PostgresCreate => "postgres_create",
            Self::GpuAttach => "gpu_attach",
            Self::GpuDetach => "gpu_detach",
        }
    }

    /// The action for a verb name, or `None` when it is not on the list.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == name)
    }

    /// The verb's own params schema, with the inline `approval` field removed: the proposal is what replaces that field.
    pub fn params_schema(self) -> Value {
        let mut schema = (verb(self.as_str())
            .expect("every allowed action is a verb in the table")
            .params)();
        if let Some(required) = schema.get_mut("required").and_then(Value::as_array_mut) {
            required.retain(|name| name.as_str() != Some("approval"));
        }
        if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
            properties.remove("approval");
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
            _ => {}
        }
        Ok(args)
    }

    /// The arguments the owner is shown, in plain words. Every field of the verb's params schema has a row. A GPU action
    /// also names the card's product when the inventory knows it; [`Self::details`] leaves that out.
    pub fn details(self, args: &Value) -> Vec<ActionDetail> {
        self.details_with_gpu(args, None)
    }

    /// The same rows, with the card's product filled in for an attach or detach when the inventory knows it. The GPU row
    /// always carries the PCI address, so two identical cards are never confused, and the effect row never claims that a
    /// card shared by more than one container has isolated VRAM.
    pub fn details_with_gpu(self, args: &Value, gpu_product: Option<&str>) -> Vec<ActionDetail> {
        let text = |key: &str| args[key].as_str().unwrap_or_default();
        let row = |label: &str, value: &str| ActionDetail {
            label: label.to_string(),
            value: value.to_string(),
        };
        match self {
            Self::ServiceCreate => {
                vec![row("Name", text("name")), row("Template", text("template"))]
            }
            Self::ServiceRemove | Self::ServiceUnpublish => vec![row("Name", text("name"))],
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
        }
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
        match self {
            Self::ServiceCreate => format!(
                "Create the service {} from template {}",
                text("name"),
                text("template")
            ),
            Self::ServiceRemove => format!("Remove the service {}", text("name")),
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
        }
    }
}

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
}

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
        })?;
        let retired = self.waiting.remove(index);
        self.compact_if_large();
        Ok(Some(retired))
    }

    /// Journals the result of one approved action: whether it ran, and the refusal code when it did not.
    pub fn journal_result(&mut self, id: &str, ok: bool, code: Option<&str>) -> io::Result<()> {
        self.append(ActionEvent {
            event: "result".to_string(),
            proposal: None,
            id: Some(id.to_string()),
            ok: Some(ok),
            code: code.map(str::to_string),
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
            .append(true)
            .mode(0o600)
            .open(&self.path)?;
        file.write_all(&bytes)?;
        file.sync_all()
    }
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
