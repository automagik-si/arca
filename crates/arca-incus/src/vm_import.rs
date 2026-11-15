//! The writer of an instance import (`instance_import`): one virtual machine moved in from an outside hypervisor, with its
//! custom volumes and its network ACLs.
//!
//! This writer is outside the desired-state journal, like a runner pool's job machines: an imported VM is not declared,
//! and the journal writer classes it as no one's it may change. Every object it makes carries the import mark
//! ([`IMPORT_KEY`]) in its own config, and a create whose body does not is refused before it is sent. Every later write
//! (a start, a stop, a delete) reads the object first and refuses one without the mark, so no other instance, volume or
//! ACL is ever changed from here.
//!
//! What it sends: reads of a storage pool, a network, a project, an instance and its state; an ACL create (a name
//! starting `arca-import-`), a custom block volume create, an instance create (a virtual machine with `source: none`,
//! not started by the create), a start, a forced stop and the deletes of those three kinds. Nothing else.

use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::lifecycle::{checked_envelope, plain_name, refused};
use crate::read::{IncusError, exchange_capped};

/// The key, in the own config of everything an import makes, that marks it as an import's.
pub const IMPORT_KEY: &str = "user.arcakernel.import";
/// The prefix of every ACL name an import makes.
pub const IMPORT_ACL_PREFIX: &str = "arca-import-";
const TIMEOUT: Duration = Duration::from_secs(10);
const WAIT_SECONDS: u64 = 5;
const POLL_PAUSE: Duration = Duration::from_millis(200);

/// The import writer.
#[derive(Debug, Clone)]
pub struct VmImportWriter {
    socket: PathBuf,
    deadline: Duration,
}

/// Whether `metadata` (an object's own read) carries the import mark.
pub fn import_marked(metadata: &Value) -> bool {
    metadata
        .get("config")
        .and_then(|config| config.get(IMPORT_KEY))
        .and_then(Value::as_str)
        .is_some_and(|mark| !mark.is_empty())
}

impl VmImportWriter {
    pub fn new(socket: PathBuf) -> Self {
        Self {
            socket,
            deadline: Duration::from_secs(600),
        }
    }

    /// The same writer with another limit on one operation.
    pub fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }

    /// A storage pool's document. `None` when Incus has no such pool.
    pub fn pool(&self, name: &str) -> Result<Option<Value>, IncusError> {
        named(name)?;
        self.get(&format!("/1.0/storage-pools/{name}"))
    }

    /// A network's document, a host bridge Incus does not manage included. `None` when Incus knows no such network.
    pub fn network(&self, name: &str) -> Result<Option<Value>, IncusError> {
        named(name)?;
        self.get(&format!("/1.0/networks/{name}?project=default"))
    }

    /// A project's document. `None` when Incus has no such project.
    pub fn project(&self, name: &str) -> Result<Option<Value>, IncusError> {
        named(name)?;
        self.get(&format!("/1.0/projects/{name}"))
    }

    /// An instance's document. `None` when the project has no such instance.
    pub fn instance(&self, project: &str, name: &str) -> Result<Option<Value>, IncusError> {
        named(project)?;
        named(name)?;
        self.get(&format!("/1.0/instances/{name}?project={project}"))
    }

    /// Makes an ACL. Its name starts with [`IMPORT_ACL_PREFIX`] and it carries the mark.
    pub fn create_acl(&self, project: &str, body: &Value) -> Result<(), IncusError> {
        named(project)?;
        let name = body_name(body)?;
        if !name.starts_with(IMPORT_ACL_PREFIX) || !import_marked(body) {
            return Err(refused(
                "an import's ACL is named arca-import-… and carries the import mark",
            ));
        }
        self.send(
            "POST",
            &format!("/1.0/network-acls?project={project}"),
            Some(body),
        )
    }

    /// Makes a custom block volume carrying the mark.
    pub fn create_volume(&self, project: &str, pool: &str, body: &Value) -> Result<(), IncusError> {
        named(project)?;
        named(pool)?;
        body_name(body)?;
        if body.get("content_type").and_then(Value::as_str) != Some("block") || !import_marked(body)
        {
            return Err(refused(
                "an import's volume is a block volume carrying the import mark",
            ));
        }
        self.send(
            "POST",
            &format!("/1.0/storage-pools/{pool}/volumes/custom?project={project}"),
            Some(body),
        )
    }

    /// Makes the virtual machine, stopped, with an empty root. The body carries the mark, `source: none` and
    /// `start: false`; the name must be free.
    pub fn create_instance(&self, project: &str, body: &Value) -> Result<(), IncusError> {
        named(project)?;
        let name = body_name(body)?;
        let shape = body.get("type").and_then(Value::as_str) == Some("virtual-machine")
            && body.pointer("/source/type").and_then(Value::as_str) == Some("none")
            && body.get("start") == Some(&json!(false))
            && import_marked(body);
        if !shape {
            return Err(refused(
                "an import makes a stopped virtual machine with an empty root, carrying the import mark",
            ));
        }
        if self.instance(project, name)?.is_some() {
            return Err(refused("an instance of that name exists"));
        }
        self.send(
            "POST",
            &format!("/1.0/instances?project={project}"),
            Some(body),
        )?;
        match self.instance(project, name)? {
            Some(made) if import_marked(&made) => Ok(()),
            _ => Err(refused(
                "the created instance does not carry the import mark",
            )),
        }
    }

    /// Starts an imported instance.
    pub fn start(&self, project: &str, name: &str) -> Result<(), IncusError> {
        self.marked_instance(project, name)?
            .ok_or_else(|| refused("there is no such instance"))?;
        self.send(
            "PUT",
            &format!("/1.0/instances/{name}/state?project={project}"),
            Some(&json!({ "action": "start", "timeout": 60 })),
        )
    }

    /// Whether the guest's Incus agent answers: the instance's state counts its processes, which only the agent can.
    pub fn agent_answers(&self, project: &str, name: &str) -> bool {
        if !plain_name(project) || !plain_name(name) {
            return false;
        }
        self.get(&format!("/1.0/instances/{name}/state?project={project}"))
            .ok()
            .flatten()
            .and_then(|state| state.get("processes").and_then(Value::as_i64))
            .is_some_and(|processes| processes > 0)
    }

    /// Stops an imported instance at once and deletes it. Returns its document as read before, `None` when it is gone.
    pub fn remove_instance(&self, project: &str, name: &str) -> Result<Option<Value>, IncusError> {
        let Some(read) = self.marked_instance(project, name)? else {
            return Ok(None);
        };
        if read.get("status").and_then(Value::as_str) != Some("Stopped") {
            let stop = self.send(
                "PUT",
                &format!("/1.0/instances/{name}/state?project={project}"),
                Some(&json!({ "action": "stop", "force": true, "timeout": -1 })),
            );
            // A machine that stopped by itself meanwhile fails the stop; the delete below still runs.
            match stop {
                Ok(()) | Err(IncusError::Status { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        match self.send(
            "DELETE",
            &format!("/1.0/instances/{name}?project={project}"),
            None,
        ) {
            Ok(()) | Err(IncusError::Status { code: 404, .. }) => Ok(Some(read)),
            Err(error) => Err(error),
        }
    }

    /// Deletes an imported custom volume. `false` when it is gone.
    pub fn remove_volume(&self, project: &str, pool: &str, name: &str) -> Result<bool, IncusError> {
        named(project)?;
        named(pool)?;
        named(name)?;
        let path = format!("/1.0/storage-pools/{pool}/volumes/custom/{name}?project={project}");
        self.remove_marked(&path, "volume")
    }

    /// Deletes an imported ACL. `false` when it is gone.
    pub fn remove_acl(&self, project: &str, name: &str) -> Result<bool, IncusError> {
        named(project)?;
        named(name)?;
        if !name.starts_with(IMPORT_ACL_PREFIX) {
            return Err(refused("an import's ACL is named arca-import-…"));
        }
        self.remove_marked(
            &format!("/1.0/network-acls/{name}?project={project}"),
            "ACL",
        )
    }

    fn remove_marked(&self, path: &str, what: &str) -> Result<bool, IncusError> {
        let Some(read) = self.get(path)? else {
            return Ok(false);
        };
        if !import_marked(&read) {
            return Err(refused(&format!(
                "the {what} does not carry the import mark"
            )));
        }
        match self.send("DELETE", path, None) {
            Ok(()) => Ok(true),
            Err(IncusError::Status { code: 404, .. }) => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn marked_instance(&self, project: &str, name: &str) -> Result<Option<Value>, IncusError> {
        match self.instance(project, name)? {
            None => Ok(None),
            Some(read)
                if import_marked(&read)
                    && read.get("type").and_then(Value::as_str) == Some("virtual-machine") =>
            {
                Ok(Some(read))
            }
            Some(_) => Err(refused("the instance is not an import's")),
        }
    }

    /// A GET's metadata, `None` on 404.
    fn get(&self, path: &str) -> Result<Option<Value>, IncusError> {
        match self.json("GET", path, None) {
            Ok(envelope) => Ok(envelope.get("metadata").cloned()),
            Err(IncusError::Status { code: 404, .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// A write, waited on to its end when it names an operation.
    fn send(&self, method: &str, path: &str, body: Option<&Value>) -> Result<(), IncusError> {
        let envelope = self.json(method, path, body)?;
        self.wait_for(&envelope)
    }

    fn wait_for(&self, envelope: &Value) -> Result<(), IncusError> {
        if envelope.get("type").and_then(Value::as_str) != Some("async") {
            return Ok(());
        }
        let Some(id) = envelope
            .get("operation")
            .and_then(Value::as_str)
            .and_then(|operation| operation.strip_prefix("/1.0/operations/"))
            .map(|id| id.split('?').next().unwrap_or(id).to_string())
            .filter(|id| {
                !id.is_empty()
                    && id.len() <= 64
                    && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            })
        else {
            return Err(IncusError::Decode("the answer names no operation".into()));
        };
        let started = Instant::now();
        loop {
            if started.elapsed() >= self.deadline {
                return Err(IncusError::Timeout);
            }
            let path = format!("/1.0/operations/{id}/wait?timeout={WAIT_SECONDS}");
            let framed = self.raw(
                "GET",
                &path,
                &[],
                Duration::from_secs(WAIT_SECONDS) + TIMEOUT,
            )?;
            if framed.status == 404 {
                return Ok(());
            }
            let answer = checked_envelope(framed)?;
            let code = answer
                .pointer("/metadata/status_code")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            match code {
                200 => return Ok(()),
                100..=199 => thread::sleep(POLL_PAUSE),
                _ => {
                    return Err(IncusError::Status {
                        code: u16::try_from(code).unwrap_or(500),
                        message: answer
                            .pointer("/metadata/err")
                            .and_then(Value::as_str)
                            .unwrap_or("the operation failed")
                            .chars()
                            .take(256)
                            .collect(),
                    });
                }
            }
        }
    }

    fn json(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, IncusError> {
        let bytes = body
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|_| IncusError::Decode("the request body does not serialize".into()))?
            .unwrap_or_default();
        let framed = self.raw(method, path, &bytes, TIMEOUT)?;
        checked_envelope(framed)
    }

    fn raw(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        timeout: Duration,
    ) -> Result<crate::read::Framed, IncusError> {
        let mut head = format!("{method} {path} HTTP/1.1\r\nHost: incus\r\nConnection: close\r\n");
        if !body.is_empty() {
            head.push_str("Content-Type: application/json\r\n");
        }
        if !body.is_empty() || method == "POST" || method == "PUT" {
            head.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        head.push_str("\r\n");
        let mut request = head.into_bytes();
        request.extend_from_slice(body);
        exchange_capped(&self.socket, timeout, &request, 4 * 1024 * 1024)
    }
}

fn named(name: &str) -> Result<(), IncusError> {
    if plain_name(name) {
        Ok(())
    } else {
        Err(refused("a name that is not plain"))
    }
}

fn body_name(body: &Value) -> Result<&str, IncusError> {
    let name = body.get("name").and_then(Value::as_str).unwrap_or_default();
    named(name)?;
    Ok(name)
}
