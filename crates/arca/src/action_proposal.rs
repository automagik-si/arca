//! Inert action proposals: a viewer may ask the owner to approve one of a closed set of service or database actions.
//!
//! A proposal changes nothing. It carries the action's own arguments, a digest over the canonical `{action, args}`, and a
//! plain-language summary built here from the validated arguments. The kernel keeps at most [`MAX_WAITING`] unexpired
//! proposals; a ninth is refused with `TooManyProposals` rather than silently retiring an older one. A repeated digest
//! returns the record that already waits.
//!
//! The proposals live in an append-only journal next to the effect journal, `action-proposals.jsonl`. Every record is one
//! line with an `event` of `proposed`, `approved`, `declined` or `result`. Opening the kernel replays the file, so a waiting
//! proposal survives a restart. The file holds no approval string, proof or secret.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use arca_api::{conforms, verb};
use arca_kernel::canon::{Digest, canonical_json};
use arca_reconcile::PLAN_TTL_MS;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The proposals journal's file name in the state directory.
pub const ACTION_PROPOSALS_FILE: &str = "action-proposals.jsonl";
/// How many unexpired action proposals may wait at once. A ninth is refused.
pub const MAX_WAITING: usize = 8;
/// The largest proposals journal this build will read. A full one is far smaller.
const MAX_JOURNAL_BYTES: u64 = 16 * 1024 * 1024;

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
    PostgresDelete,
}

impl ActionKind {
    /// Every allowed action, in the order the table documents them.
    pub const ALL: [ActionKind; 8] = [
        Self::ServiceCreate,
        Self::ServiceRemove,
        Self::ServicePublish,
        Self::ServiceUnpublish,
        Self::ServiceConnect,
        Self::ServiceDisconnect,
        Self::PostgresCreate,
        Self::PostgresDelete,
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
            Self::PostgresDelete => "postgres_delete",
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

    /// Checks `args` against the verb's own schema (without `approval`) and the verb's semantic name rule.
    pub fn validate(self, args: &Value) -> Result<(), String> {
        conforms(&self.params_schema(), args)?;
        match self {
            Self::ServicePublish => {
                if args["reach"].as_str() != Some("tailnet") {
                    return Err("args/reach: the only reach is the owner's tailnet".to_string());
                }
            }
            Self::PostgresCreate | Self::PostgresDelete => {
                let name = args["name"].as_str().unwrap_or_default();
                if !arca_netproject::name_ok(name, "postgres", 31) {
                    return Err("args/name: expected a database name".to_string());
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// A sentence in the owner's words, built only from the fields the schema has already checked.
    pub fn summary(self, args: &Value) -> String {
        let text = |key: &str| args[key].as_str().unwrap_or_default();
        match self {
            Self::ServiceCreate => format!("Create the service {}", text("name")),
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
            Self::PostgresCreate => format!("Create the database {}", text("name")),
            Self::PostgresDelete => format!("Delete the database {}", text("name")),
        }
    }
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
}

impl ActionProposal {
    /// Whether the proposal's fifteen minutes have run.
    pub fn expired(&self, now: u64) -> bool {
        now > self.expires_at
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
    /// The journal could not be read or made durable.
    Store(io::Error),
}

impl ActionProposalStore {
    /// Opens the journal beside the effect journal and replays the proposals that still wait. A missing file is empty.
    pub fn open(state_dir: &Path) -> io::Result<Self> {
        let path = state_dir.join(ACTION_PROPOSALS_FILE);
        let mut waiting: Vec<ActionProposal> = Vec::new();
        match fs::symlink_metadata(&path) {
            Ok(meta) => {
                if !meta.is_file() || meta.file_type().is_symlink() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "the proposals journal is not a regular file",
                    ));
                }
                if meta.permissions().mode() & 0o077 != 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "the proposals journal is not mode 0600",
                    ));
                }
                if meta.len() > MAX_JOURNAL_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "the proposals journal is too large",
                    ));
                }
                let text = fs::read_to_string(&path)?;
                for line in text.lines().filter(|line| !line.trim().is_empty()) {
                    let event: ActionEvent = serde_json::from_str(line).map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "the proposals journal holds a line that is not an event",
                        )
                    })?;
                    match event.event.as_str() {
                        "proposed" => {
                            let Some(proposal) = event.proposal else {
                                return Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "a proposed event holds no proposal",
                                ));
                            };
                            waiting.push(proposal);
                        }
                        "approved" | "declined" | "result" => {
                            if let Some(id) = event.id {
                                waiting.retain(|proposal| proposal.id != id);
                            }
                        }
                        _ => {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "the proposals journal holds an unknown event",
                            ));
                        }
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        Ok(Self { path, waiting })
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
        Ok(Some(self.waiting.remove(index)))
    }

    /// Journals the result of one approved action: whether it ran, and the refusal code when it did not.
    pub fn journal_result(&mut self, id: &str, ok: bool, code: Option<&str>) -> io::Result<()> {
        self.append(ActionEvent {
            event: "result".to_string(),
            proposal: None,
            id: Some(id.to_string()),
            ok: Some(ok),
            code: code.map(str::to_string),
        })
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
