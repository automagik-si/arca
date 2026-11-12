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
/// The longest one state change's operation is waited on: the grace, and time for Incus to fail the operation after it.
const DEADLINE: Duration = Duration::from_secs(90);
/// The longest one caller's whole action may take: two reads before the change, the change's own request, the wait on its
/// operation, the read-back and one more read after a failure, each read and request bounded by the client's timeout. A
/// relay that waits for the answer must wait longer than this.
pub const CALL_BUDGET: Duration = Duration::from_secs(150);
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

/// A cached image whose record says it was fetched with a registry's credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedImage {
    pub fingerprint: String,
    /// Whether Incus would go back to the registry for it by itself.
    pub auto_update: bool,
    /// Whether an instance of the project says it was made from it.
    pub used: bool,
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

    /// The same writer, with the projects of the tenants whose entries exist added to its family.
    pub fn with_tenants<'a>(
        &self,
        tenants: impl IntoIterator<Item = &'a str>,
    ) -> Result<Self, String> {
        Ok(Self {
            family: self.family.clone().with_tenants(tenants)?,
            ..self.clone()
        })
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

    /// Sends one state body for one instance and waits for its operation. It is the crate's own: the one caller outside
    /// this module is [`crate::function_run`], which sends a forced stop to a function's instance and to nothing else.
    /// [`StateChanger::change`] is the only public state write, and it never forces.
    pub(crate) fn put_state(
        &self,
        project: &str,
        name: &str,
        body: &Value,
    ) -> Result<(), IncusError> {
        self.allow(project)?;
        let url = state_url(project, name)?;
        let envelope = checked_envelope(self.send("PUT", &url, Some(body))?)?;
        self.wait_for(&envelope)
    }

    /// Asks Incus to rebuild one instance's root disk from an image and waits for the operation. The instance keeps its
    /// config, its devices and its uuid. It is the crate's own, for [`crate::function_run`] alone, which sends it to a
    /// function's stopped instance and to nothing else.
    pub(crate) fn rebuild(
        &self,
        project: &str,
        name: &str,
        body: &Value,
    ) -> Result<(), IncusError> {
        self.allow(project)?;
        if !plain_name(project) || !plain_name(name) {
            return Err(refused("the project or instance name is not plain"));
        }
        let url = format!("/1.0/instances/{name}/rebuild?project={project}");
        let envelope = checked_envelope(self.send("POST", &url, Some(body))?)?;
        self.wait_for(&envelope)
    }

    /// Takes a snapshot of one stopped instance, named `snapshot`, and waits for its operation. For `service_update`:
    /// the snapshot it can go back to. A project that forbids snapshots (a tenant's does, unless it allows them)
    /// answers 403, which the caller sees as [`IncusError::Status`].
    pub fn snapshot_instance(
        &self,
        seen: &InstanceState,
        snapshot: &str,
    ) -> Result<(), IncusError> {
        self.allow(&seen.project)?;
        let fresh = self.read(&seen.project, &seen.name)?;
        if fresh.uuid != seen.uuid || fresh.kind != seen.kind {
            return Err(refused("the instance was recreated since it was read"));
        }
        if !plain_name(snapshot) {
            return Err(refused("the snapshot name is not plain"));
        }
        let url = format!(
            "/1.0/instances/{}/snapshots?project={}",
            seen.name, seen.project
        );
        let envelope = checked_envelope(self.send(
            "POST",
            &url,
            Some(&json!({ "name": snapshot, "stateful": false })),
        )?)?;
        self.wait_for(&envelope)
    }

    /// Puts one stopped instance back to the snapshot `snapshot` it holds: its root, its config and its devices, and
    /// nothing else. The instance is read again first, and one that was recreated since `seen` is refused.
    pub fn restore_snapshot(&self, seen: &InstanceState, snapshot: &str) -> Result<(), IncusError> {
        self.allow(&seen.project)?;
        let fresh = self.read(&seen.project, &seen.name)?;
        if fresh.uuid != seen.uuid || fresh.kind != seen.kind {
            return Err(refused("the instance was recreated since it was read"));
        }
        if !fresh.is_stopped() {
            return Err(refused("the instance is not stopped"));
        }
        if !plain_name(snapshot) {
            return Err(refused("the snapshot name is not plain"));
        }
        let url = object_url(&seen.project, &seen.name)?;
        let envelope =
            checked_envelope(self.send("PUT", &url, Some(&json!({ "restore": snapshot })))?)?;
        self.wait_for(&envelope)
    }

    /// Gives one stopped instance the root of another image, an OCI image by digest on a registry: Incus rebuilds the
    /// root disk and keeps the config, the devices (a data volume among them) and the uuid. `source` must be exactly
    /// the five fields of such an image. A registry that asks for a credential is named with it, in the address that is
    /// sent and nowhere else, and the image Incus fetches then is told not to go back to that address by itself.
    pub fn rebuild_from_registry(
        &self,
        seen: &InstanceState,
        source: &Value,
        registry: Option<&crate::write::RegistryCredential>,
    ) -> Result<(), IncusError> {
        let fresh = self.read(&seen.project, &seen.name)?;
        if fresh.uuid != seen.uuid || fresh.kind != seen.kind {
            return Err(refused("the instance was recreated since it was read"));
        }
        if !fresh.is_stopped() {
            return Err(refused("the instance is not stopped"));
        }
        if !crate::function_run::registry_source(source) {
            return Err(refused(
                "the source is not an image by digest on a registry",
            ));
        }
        let mut sent = source.clone();
        let address = registry
            .and_then(|registry| registry.address(source["server"].as_str().unwrap_or_default()));
        let with_credential = address.is_some();
        if let Some(address) = address {
            sent["server"] = json!(address);
        }
        let rebuilt = self.rebuild(&fresh.project, &fresh.name, &json!({ "source": sent }));
        if with_credential {
            let settled = self.fetched_with_credential(&fresh.project).map(|images| {
                images
                    .iter()
                    .filter(|image| image.auto_update)
                    .filter(|image| {
                        self.image_auto_update_off(&fresh.project, &image.fingerprint)
                            .is_err()
                    })
                    .count()
            });
            if settled != Ok(0) {
                eprintln!(
                    "arca: auto_update could not be turned off on the image {} was rebuilt from: Incus may ask its registry again by itself until the next sweep",
                    fresh.name
                );
            }
        }
        rebuilt
    }

    /// Whether the store the project reads holds an image with this fingerprint. For [`crate::function_run`] alone.
    pub(crate) fn image_present(
        &self,
        project: &str,
        fingerprint: &str,
    ) -> Result<bool, IncusError> {
        self.allow(project)?;
        if !plain_name(project) || !plain_name(fingerprint) {
            return Err(refused("the project or the fingerprint is not plain"));
        }
        let url = format!("/1.0/images/{fingerprint}?project={project}");
        match checked_envelope(self.send("GET", &url, None)?) {
            Ok(_) => Ok(true),
            Err(IncusError::Status { code: 404, .. }) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Tells Incus not to go back by itself to where a cached image came from. For [`crate::function_run`] alone: an
    /// image fetched again with a registry's credential keeps the address it was fetched from, and an image by
    /// digest never changes.
    pub(crate) fn image_auto_update_off(
        &self,
        project: &str,
        fingerprint: &str,
    ) -> Result<(), IncusError> {
        self.allow(project)?;
        if !plain_name(project) || !plain_name(fingerprint) {
            return Err(refused("the project or the fingerprint is not plain"));
        }
        let url = format!("/1.0/images/{fingerprint}?project={project}");
        let envelope =
            checked_envelope(self.send("PATCH", &url, Some(&json!({ "auto_update": false })))?)?;
        self.wait_for(&envelope)
    }

    /// The cached images of the store the project reads whose own record says they were fetched with a credential
    /// (the address Incus keeps for them holds one). An image with no source address, and one from an address
    /// without a credential, is never among them. For [`crate::function_run`] alone.
    pub(crate) fn fetched_with_credential(
        &self,
        project: &str,
    ) -> Result<Vec<FetchedImage>, IncusError> {
        self.allow(project)?;
        if !plain_name(project) {
            return Err(refused("the project name is not plain"));
        }
        let images = checked_envelope(self.send(
            "GET",
            &format!("/1.0/images?project={project}&recursion=1"),
            None,
        )?)?;
        let mut found: Vec<FetchedImage> = images["metadata"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|image| {
                image["cached"] == true
                    && image["update_source"]["protocol"] == "oci"
                    && image["update_source"]["server"]
                        .as_str()
                        .is_some_and(crate::write::address_holds_credential)
            })
            .filter_map(|image| {
                let fingerprint = image["fingerprint"].as_str()?;
                plain_name(fingerprint).then(|| FetchedImage {
                    fingerprint: fingerprint.to_string(),
                    auto_update: image["auto_update"] != false,
                    used: true,
                })
            })
            .collect();
        if found.is_empty() {
            return Ok(found);
        }
        // Which of them an instance of the project is made from. A listing that cannot be read is an error: no
        // image is called unused on a guess.
        let instances = checked_envelope(self.send(
            "GET",
            "/1.0/instances?all-projects=true&recursion=1",
            None,
        )?)?;
        let rows = instances["metadata"]
            .as_array()
            .ok_or_else(|| decode("the instances are not a list"))?;
        for image in &mut found {
            image.used = rows.iter().any(|row| {
                row["project"].as_str() == Some(project)
                    && row["config"]["volatile.base_image"].as_str()
                        == Some(image.fingerprint.as_str())
            });
        }
        Ok(found)
    }

    /// Deletes one cached image from the store the project reads, when no instance of the project says it was made
    /// from it. Returns whether it was deleted. For [`crate::function_run`] alone.
    pub(crate) fn delete_unused_image(
        &self,
        project: &str,
        fingerprint: &str,
    ) -> Result<bool, IncusError> {
        self.allow(project)?;
        if !plain_name(project) || !plain_name(fingerprint) {
            return Err(refused("the project or the fingerprint is not plain"));
        }
        let url = format!("/1.0/images/{fingerprint}?project={project}");
        let image = match checked_envelope(self.send("GET", &url, None)?) {
            Ok(envelope) => envelope,
            Err(IncusError::Status { code: 404, .. }) => return Ok(false),
            Err(error) => return Err(error),
        };
        if image["metadata"]["cached"] != true {
            return Ok(false);
        }
        // Every instance that reads this store counts: the listing is of all projects, and one of this project
        // that names the image keeps it. A listing that cannot be read keeps it too.
        let instances = checked_envelope(self.send(
            "GET",
            "/1.0/instances?all-projects=true&recursion=1",
            None,
        )?)?;
        let used = instances["metadata"].as_array().is_none_or(|rows| {
            rows.iter().any(|row| {
                row["project"].as_str() == Some(project)
                    && row["config"]["volatile.base_image"].as_str() == Some(fingerprint)
            })
        });
        if used {
            return Ok(false);
        }
        let envelope = checked_envelope(self.send("DELETE", &url, None)?)?;
        self.wait_for(&envelope)?;
        Ok(true)
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
            // One wait never outlasts what is left of the deadline, so the whole wait is bounded by it.
            let left = self.deadline.saturating_sub(started.elapsed());
            let framed = self.send_within(
                "GET",
                &path,
                None,
                (Duration::from_secs(WAIT_SECONDS) + self.timeout).min(left),
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

pub(crate) fn plain_name(text: &str) -> bool {
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

pub(crate) fn checked_envelope(framed: Framed) -> Result<Value, IncusError> {
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

pub(crate) fn refused(reason: &str) -> IncusError {
    IncusError::Refused(reason.to_string())
}

fn decode(reason: &str) -> IncusError {
    IncusError::Decode(reason.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_call_budget_covers_every_request_of_one_action() {
        // Two reads, the PUT, the wait, the read-back and the read after a failure.
        assert!(TIMEOUT * 5 + DEADLINE <= CALL_BUDGET);
        assert!(Duration::from_secs(GRACE_SECONDS) < DEADLINE);
    }
}
