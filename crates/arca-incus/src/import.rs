//! The service import writer: the one write Arca makes to an instance another manager created, in that manager's project.
//!
//! Every other writer in this crate refuses such an instance twice: its project is outside the kernel's family, and its own
//! config classes as the other manager's. This writer has no family and no classifier. It has one allowance,
//! [`import_allowance`], and it sends one request: a config PATCH of the kernel's entry mark, under the `If-Match` of the read
//! the caller checked. The allowance holds only when all of this is true:
//!
//! - the entry is `adopt-<instance>`, the import entry of that instance, and no other kind of entry;
//! - the object is an instance, named with no parent, and its `{project, instance}` is in the allowlist the node's
//!   configuration gives (`kernel.importServices`);
//! - the change names exactly one key, `user.arcakernel.entry`, and sets it to that entry's id (the import) or removes it
//!   (the release).
//!
//! A second key in the same change, another instance, a project or a name the allowlist does not hold, a device, a profile,
//! a state change, a PUT, a POST and a DELETE are never sent: nothing here can build them. A stale `If-Match` is Incus's 412
//! and changes nothing. `docs/OWNERSHIP.md` states the rule.

use std::collections::BTreeMap;
use std::path::PathBuf;

use arca_kernel::desired::EntryId;
use arca_kernel::ownership::{IncusKind, KERNEL_ENTRY_KEY, ObjectRef};
use serde_json::{Value, json};

use crate::adopt::ADOPT_ENTRY_PREFIX;
use crate::read::{IncusError, TIMEOUT, checked_envelope};
use crate::write::{ProjectFamily, UnixIncusWriter};

/// One instance the import may mark: the project it is in and its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportTarget {
    pub project: String,
    pub instance: String,
}

impl ImportTarget {
    /// The instance's address, by name.
    pub fn object(&self) -> ObjectRef {
        ObjectRef {
            kind: IncusKind::Instance,
            project: self.project.clone(),
            parent: None,
            name: self.instance.clone(),
            uuid: None,
        }
    }

    /// The import entry of the instance, `adopt-<instance>`.
    pub fn entry(&self) -> Option<EntryId> {
        EntryId::parse(&format!("{ADOPT_ENTRY_PREFIX}{}", self.instance))
    }
}

/// Whether `entry` may send `changes` to `object`: the whole of the import's allowance. See the module's rules.
pub fn import_allowance(
    allow: &[ImportTarget],
    entry: &EntryId,
    object: &ObjectRef,
    changes: &BTreeMap<String, Option<String>>,
) -> bool {
    object.kind == IncusKind::Instance
        && object.parent.is_none()
        && entry.0 == format!("{ADOPT_ENTRY_PREFIX}{}", object.name)
        && allow
            .iter()
            .any(|listed| listed.project == object.project && listed.instance == object.name)
        && changes.len() == 1
        && changes
            .get(KERNEL_ENTRY_KEY)
            .is_some_and(|value| value.as_deref().is_none_or(|mark| mark == entry.0))
}

/// An allowlisted instance as one read showed it: the whole answer and the ETag a PATCH sends back.
#[derive(Clone)]
pub struct ImportRead {
    pub etag: String,
    pub body: Value,
}

// The body holds the instance's config, which can hold a secret, so it is never printed.
impl std::fmt::Debug for ImportRead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImportRead").finish_non_exhaustive()
    }
}

/// The import writer of one node: the Incus socket and the allowlist its configuration gives.
#[derive(Debug, Clone)]
pub struct ImportWriter {
    socket: PathBuf,
    allow: Vec<ImportTarget>,
}

impl ImportWriter {
    /// A writer over the Incus socket at `socket` for the instances in `allow`. Nothing is read or sent here.
    pub fn new(socket: PathBuf, allow: Vec<ImportTarget>) -> Self {
        Self { socket, allow }
    }

    /// The transport for `entry`. Its family is empty, so none of its own checked writes can name a project; only this
    /// module's one request goes through it.
    fn transport(&self, entry: &EntryId) -> Result<UnixIncusWriter, IncusError> {
        let family = ProjectFamily::new([], [])
            .map_err(|_| refused("the import writer's family cannot be built"))?;
        Ok(UnixIncusWriter::new(
            self.socket.clone(),
            entry.clone(),
            BTreeMap::new(),
            family,
        ))
    }

    /// Reads an allowlisted instance with one GET: `None` when it is not there. An instance the allowlist does not hold is
    /// refused before any request.
    pub fn read(&self, target: &ImportTarget) -> Result<Option<ImportRead>, IncusError> {
        if !self.allow.contains(target) {
            return Err(refused(NOT_ALLOWED));
        }
        let entry = target
            .entry()
            .ok_or_else(|| refused("the instance has no import entry"))?;
        match self.transport(&entry)?.get_etag(&target.object()) {
            Ok((body, etag)) => Ok(Some(ImportRead { etag, body })),
            Err(IncusError::Status { code: 404, .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Sends `changes` to `object` for `entry` as one config PATCH under `etag`, and waits on its operation. Anything outside
    /// [`import_allowance`] is [`IncusError::Refused`] and nothing is sent. A stale `etag` is a 412 status and changes nothing.
    pub fn patch(
        &self,
        entry: &EntryId,
        object: &ObjectRef,
        changes: &BTreeMap<String, Option<String>>,
        etag: &str,
    ) -> Result<(), IncusError> {
        if !import_allowance(&self.allow, entry, object, changes) {
            return Err(refused(NOT_ALLOWED));
        }
        if etag.is_empty() {
            return Err(refused("the import's PATCH carries the ETag of its read"));
        }
        let config: serde_json::Map<String, Value> = changes
            .iter()
            .map(|(key, value)| (key.clone(), json!(value.as_deref().unwrap_or(""))))
            .collect();
        let path = format!("/1.0/instances/{}?project={}", object.name, object.project);
        let writer = self.transport(entry)?;
        let framed = writer.send_checked(
            "PATCH",
            &path,
            Some(etag),
            Some(&json!({ "config": config })),
            TIMEOUT,
        )?;
        let envelope = checked_envelope(&framed.into_raw())?;
        if envelope.get("type").and_then(Value::as_str) == Some("sync") {
            return Ok(());
        }
        match envelope.get("operation").and_then(Value::as_str) {
            None | Some("") => Ok(()),
            Some(url) => {
                let id = url
                    .strip_prefix("/1.0/operations/")
                    .filter(|id| {
                        !id.is_empty() && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
                    })
                    .ok_or_else(|| {
                        IncusError::Decode("the answer names no operation".to_string())
                    })?;
                writer.wait_on(id).map(|_| ())
            }
        }
    }
}

const NOT_ALLOWED: &str = "the import writes one key, user.arcakernel.entry, on an instance kernel.importServices lists, for that instance's import entry, and nothing else";

fn refused(reason: &str) -> IncusError {
    IncusError::Refused(reason.to_string())
}
