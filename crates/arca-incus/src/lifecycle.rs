//! Starting, stopping and restarting one existing instance, outside the desired-state journal.
//!
//! An instance's power state is not a desired-state entry: it is a live change to one existing instance. The kernel's own
//! writer is entry-scoped, so this module is a separate, deliberately narrow path, as the device assigner is. It reads one
//! instance and writes only its `state`: a `start`, a graceful `stop` or a graceful `restart`. It never creates, deletes,
//! renames or reconfigures an instance, never forces a stop, and refuses a project outside the kernel family it was built
//! with before any request. Whether the instance is the kernel's own is the caller's check, made on the row this module
//! read.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::read::{Framed, IncusError, TIMEOUT, exchange};
use crate::write::ProjectFamily;

/// How long Incus lets an instance shut down by itself before the stop, or the stop half of a restart, fails.
pub const GRACE_SECONDS: u64 = 60;
/// The longest one state change is waited on, its grace included.
const DEADLINE: Duration = Duration::from_secs(150);
/// How long one wait request asks Incus to hold.
const WAIT_SECONDS: u64 = 5;
/// The pause between two looks at an operation that is still running.
const POLL_PAUSE: Duration = Duration::from_millis(20);

/// The state change a caller asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateAction {
    Start,
    Stop,
    Restart,
}

impl StateAction {
    /// The action as Incus spells it in a state PUT.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
        }
    }
}

/// One instance as the changer read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceState {
    pub project: String,
    pub name: String,
    /// `container` or `virtual-machine`, as Incus spells the `type` field.
    pub kind: String,
    /// The instance's `volatile.uuid`, when Incus reports one.
    pub uuid: Option<String>,
    /// The instance's own config, from the same read. The kernel mark in it tells whose the instance is.
    pub own_config: BTreeMap<String, String>,
    /// The status as Incus spells it: `Running`, `Stopped`, `Frozen`, `Error`.
    pub status: String,
}

impl InstanceState {
    pub fn is_running(&self) -> bool {
        self.status == "Running"
    }

    pub fn is_stopped(&self) -> bool {
        self.status == "Stopped"
    }
}

/// The writer for one instance's power state.
#[derive(Debug, Clone)]
pub struct StateChanger {
    socket: PathBuf,
    family: ProjectFamily,
    timeout: Duration,
    deadline: Duration,
}

impl StateChanger {
    /// A writer that may address only the projects of `family`. It checks the family before every request, a read
    /// included, as the kernel's own writer does.
    pub fn new(socket: PathBuf, family: ProjectFamily) -> Self {
        Self {
            socket,
            family,
            timeout: TIMEOUT,
            deadline: DEADLINE,
        }
    }

    /// The same writer, waiting at most `deadline` on one state change.
    pub fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }

    /// Whether the family this writer was built with holds `project`.
    pub fn allows(&self, project: &str) -> bool {
        self.family
            .allows(arca_kernel::ownership::IncusKind::Instance, project)
    }

    /// Reads one instance: its type, its uuid, its own config and its status.
    pub fn read(&self, project: &str, name: &str) -> Result<InstanceState, IncusError> {
        self.allow(project)?;
        let url = object_url(project, name)?;
        let metadata = checked_metadata(self.send("GET", &url, None)?)?;
        let kind = metadata
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("container")
            .to_string();
        let own_config: BTreeMap<String, String> = metadata
            .get("config")
            .and_then(Value::as_object)
            .map(|config| {
                config
                    .iter()
                    .filter_map(|(key, value)| {
                        value.as_str().map(|value| (key.clone(), value.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let uuid = own_config
            .get("volatile.uuid")
            .filter(|uuid| !uuid.is_empty())
            .cloned();
        let status = metadata
            .get("status")
            .and_then(Value::as_str)
            .filter(|status| !status.is_empty())
            .ok_or_else(|| decode("the instance has no status"))?
            .to_string();
        Ok(InstanceState {
            project: project.to_string(),
            name: name.to_string(),
            kind,
            uuid,
            own_config,
            status,
        })
    }

    /// Sends one state change for the instance `seen` was read from, waits for its operation and reads the instance back.
    /// A stop and a restart are graceful: Incus gives the instance [`GRACE_SECONDS`] to shut down and fails the operation
    /// after that, leaving it running. The instance is read again first, and one that was recreated since `seen` is refused
    /// before any write. Returns the instance as the read-back shows it.
    pub fn change(
        &self,
        seen: &InstanceState,
        action: StateAction,
    ) -> Result<InstanceState, IncusError> {
        self.allow(&seen.project)?;
        let fresh = self.read(&seen.project, &seen.name)?;
        if fresh.uuid != seen.uuid || fresh.kind != seen.kind {
            return Err(refused("the instance was recreated since it was read"));
        }
        let url = state_url(&seen.project, &seen.name)?;
        let body = match action {
            StateAction::Start => json!({ "action": "start" }),
            StateAction::Stop | StateAction::Restart => {
                json!({ "action": action.as_str(), "timeout": GRACE_SECONDS, "force": false })
            }
        };
        let envelope = checked_envelope(self.send("PUT", &url, Some(&body))?)?;
        self.wait_for(&envelope)?;
        let after = self.read(&seen.project, &seen.name)?;
        let wanted = match action {
            StateAction::Start | StateAction::Restart => after.is_running(),
            StateAction::Stop => after.is_stopped(),
        };
        if !wanted {
            return Err(IncusError::AppliedUnconfirmed(Box::new(
                crate::read::Unconfirmed {
                    target: crate::read::Target {
                        kind: arca_kernel::ownership::IncusKind::Instance,
                        project: seen.project.clone(),
                        parent: None,
                        name: seen.name.clone(),
                        uuid: after.uuid.clone(),
                    },
                    uuid: after.uuid.clone(),
                    owner: None,
                    reason: "the read-back does not show the state the change asked for"
                        .to_string(),
                },
            )));
        }
        Ok(after)
    }

    /// Refuses a project outside the family before any request is sent, a read included.
    fn allow(&self, project: &str) -> Result<(), IncusError> {
        self.allows(project)
            .then_some(())
            .ok_or_else(|| refused("the project is outside the kernel's family"))
    }

    /// Waits on the operation an async answer names until it ends or the deadline passes. Only status code 200 with the
    /// status `Success` is done; a 4xx or 5xx in the operation is the operation's own failure.
    fn wait_for(&self, envelope: &Value) -> Result<(), IncusError> {
        if envelope.get("type").and_then(Value::as_str) == Some("sync") {
            return Ok(());
        }
        let id = envelope
            .get("operation")
            .and_then(Value::as_str)
            .and_then(|operation| operation.strip_prefix("/1.0/operations/"))
            .map(|id| id.split('?').next().unwrap_or(id))
            .filter(|id| operation_id(id))
            .ok_or_else(|| decode("the answer names no operation"))?;
        let started = Instant::now();
        loop {
            if started.elapsed() >= self.deadline {
                return Err(IncusError::Timeout);
            }
            let path = format!("/1.0/operations/{id}/wait?timeout={WAIT_SECONDS}");
            let framed = self.send_within(
                "GET",
                &path,
                None,
                Duration::from_secs(WAIT_SECONDS) + self.timeout,
            )?;
            let answer = checked_envelope(framed).map_err(|error| match error {
                IncusError::Status { .. } => {
                    decode("the wait answered without the operation's status")
                }
                other => other,
            })?;
            let operation = answer
                .get("metadata")
                .and_then(Value::as_object)
                .ok_or_else(|| decode("the wait has no operation"))?;
            if operation.get("id").and_then(Value::as_str) != Some(id) {
                return Err(decode("the wait answered for another operation"));
            }
            let code = operation
                .get("status_code")
                .and_then(Value::as_u64)
                .ok_or_else(|| decode("the operation has no status code"))?;
            let status = operation
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("");
            match code {
                200 if status == "Success" => return Ok(()),
                100..=199 => thread::sleep(POLL_PAUSE),
                400..=599 => {
                    let message = operation
                        .get("err")
                        .and_then(Value::as_str)
                        .filter(|text| !text.is_empty())
                        .unwrap_or(status);
                    return Err(IncusError::Status {
                        code: u16::try_from(code).unwrap_or(500),
                        message: message.chars().take(256).collect(),
                    });
                }
                _ => return Err(decode("the operation ended without success")),
            }
        }
    }

    fn send(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Framed, IncusError> {
        self.send_within(method, path, body, self.timeout)
    }

    fn send_within(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        timeout: Duration,
    ) -> Result<Framed, IncusError> {
        let body = body
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|_| decode("the request body does not serialize"))?;
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: incus\r\nAccept: application/json\r\nConnection: close\r\n"
        );
        if let Some(body) = &body {
            head.push_str("Content-Type: application/json\r\n");
            head.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        head.push_str("\r\n");
        let mut request = head.into_bytes();
        if let Some(body) = body {
            request.extend_from_slice(&body);
        }
        exchange(&self.socket, timeout, &request)
    }
}

fn object_url(project: &str, name: &str) -> Result<String, IncusError> {
    if !plain_name(project) || !plain_name(name) {
        return Err(refused("the project or instance name is not plain"));
    }
    Ok(format!("/1.0/instances/{name}?project={project}"))
}

fn state_url(project: &str, name: &str) -> Result<String, IncusError> {
    if !plain_name(project) || !plain_name(name) {
        return Err(refused("the project or instance name is not plain"));
    }
    Ok(format!("/1.0/instances/{name}/state?project={project}"))
}

fn plain_name(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 128
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn operation_id(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn checked_metadata(framed: Framed) -> Result<Value, IncusError> {
    checked_envelope(framed)?
        .get("metadata")
        .cloned()
        .ok_or_else(|| decode("the answer has no metadata"))
}

fn checked_envelope(framed: Framed) -> Result<Value, IncusError> {
    let status = framed.status;
    if !(200..300).contains(&status) {
        // An error answer still names its reason in the envelope, which says more than the reason phrase.
        let message = serde_json::from_slice::<Value>(&framed.body)
            .ok()
            .and_then(|value| {
                value
                    .get("error")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .map(|text| text.chars().take(256).collect::<String>())
            })
            .unwrap_or_else(|| framed.reason.clone());
        return Err(IncusError::Status {
            code: status,
            message,
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
                .chars()
                .take(256)
                .collect(),
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
