//! Starting, stopping and restarting one existing instance, outside the desired-state journal, and its Incus snapshots.
//!
//! An instance's power state is not a desired-state entry: it is a live change to one existing instance. The kernel's own
//! writer is entry-scoped, so this module is a separate, deliberately narrow path, as the device assigner is. It reads one
//! instance and writes only its `state`: a `start`, a graceful `stop` or a graceful `restart`. It never creates, deletes,
//! renames or reconfigures an instance, never forces a stop, and refuses a project outside the kernel family it was built
//! with before any request. Whether the instance is the kernel's own is the caller's check, made on the row this module
//! read. The one exception is a service's snapshots ([`StateChanger::snapshot_with_volumes`] and the writes beside it):
//! they take, restore and remove Incus snapshots of a service instance of the node's own service project and of the
//! custom volumes it mounts, check the instance's mark and each volume's themselves, and send no stop and no start.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arca_kernel::ownership::{KERNEL_ENTRY_KEY, KERNEL_RETIRING_KEY};

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
    /// The names of the instance's own devices (not the profiles'), from the same read.
    pub own_devices: BTreeSet<String>,
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
        Ok(self.read_with(project, name)?.0)
    }

    /// The same read, with the instance's whole document.
    fn read_with(&self, project: &str, name: &str) -> Result<(InstanceState, Value), IncusError> {
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
        let own_devices: BTreeSet<String> = metadata
            .get("devices")
            .and_then(Value::as_object)
            .map(|devices| devices.keys().cloned().collect())
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
        let state = InstanceState {
            project: project.to_string(),
            name: name.to_string(),
            kind,
            uuid,
            own_config,
            own_devices,
            status,
        };
        Ok((state, metadata))
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
    /// config, its devices and its uuid. It is the crate's own: [`crate::function_run`] sends it to a function's stopped
    /// instance, and [`StateChanger::rebuild_from_registry`] and [`StateChanger::rebuild_from_local`] to a service's.
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

    /// Empties `/run` on the root disk of one stopped container, so Incus mounts a memory `/run` at its next start
    /// ([`crate::run_dir`]). The instance is read again first: one that was recreated since `seen`, one that is not a
    /// container and one that is not stopped are refused before any removal. Returns how many entries were removed.
    pub fn empty_run(&self, seen: &InstanceState) -> Result<usize, IncusError> {
        self.allow(&seen.project)?;
        let fresh = self.read(&seen.project, &seen.name)?;
        if fresh.uuid != seen.uuid || fresh.kind != seen.kind {
            return Err(refused("the instance was recreated since it was read"));
        }
        if fresh.kind != "container" || !fresh.is_stopped() {
            return Err(refused("the instance is not a stopped container"));
        }
        let emptied = crate::run_dir::empty_run(
            &|method, path| self.send(method, path, None),
            &fresh.project,
            &fresh.name,
        );
        if let Some(uuid) = &fresh.uuid {
            crate::run_dir::note_emptied(
                &self.socket,
                &fresh.project,
                &fresh.name,
                uuid,
                emptied.is_ok(),
            );
        }
        emptied
    }

    /// Where one container's `/run` is: in memory or on its disk. A stopped one gets a memory `/run` at its next start
    /// exactly when its root holds nothing under `/run`, which is read. A running container's mount table cannot be
    /// read through the files API, so for one this answers what this process saw when it last emptied that container
    /// (by its uuid, so never a container made again since): emptied, so its starts since mounted a memory `/run`, or
    /// not. A running container this process has not emptied is not known, an error.
    pub fn run_in_memory(&self, seen: &InstanceState) -> Result<bool, IncusError> {
        self.allow(&seen.project)?;
        let fresh = self.read(&seen.project, &seen.name)?;
        if fresh.uuid != seen.uuid || fresh.kind != "container" {
            return Err(refused("the instance is not the container that was read"));
        }
        if fresh.is_stopped() {
            let send = |method: &str, path: &str| self.send(method, path, None);
            return crate::run_dir::run_is_empty(&send, &fresh.project, &fresh.name);
        }
        fresh
            .uuid
            .as_deref()
            .and_then(|uuid| {
                crate::run_dir::emptied(&self.socket, &fresh.project, &fresh.name, uuid)
            })
            .ok_or_else(|| refused("Arca has not emptied this container's /run since it began"))
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

    /// Takes a snapshot named `snapshot` of one custom volume the service instance `seen` mounts, and waits for its
    /// operation. It sends no stop and no start: the caller decides when the volume is snapshotted. The instance is
    /// checked as [`StateChanger::snapshot_with_volumes`] checks it, and the volume must carry the instance's own mark.
    pub fn snapshot_volume(
        &self,
        seen: &InstanceState,
        pool: &str,
        volume: &str,
        snapshot: &str,
    ) -> Result<(), IncusError> {
        let (fresh, metadata) = self.owned_service(seen)?;
        self.mounted_volume(&fresh, &metadata, pool, volume)?;
        named(snapshot)?;
        self.send_volume_snapshot(&fresh.project, pool, volume, snapshot)
    }

    /// Puts one custom volume the stopped service instance `seen` mounts back to its snapshot `snapshot`, and waits.
    pub fn restore_volume(
        &self,
        seen: &InstanceState,
        pool: &str,
        volume: &str,
        snapshot: &str,
    ) -> Result<(), IncusError> {
        let (fresh, metadata) = self.owned_service(seen)?;
        if !fresh.is_stopped() {
            return Err(refused("the instance is not stopped"));
        }
        self.mounted_volume(&fresh, &metadata, pool, volume)?;
        named(snapshot)?;
        self.send_volume_restore(&fresh.project, pool, volume, snapshot)
    }

    /// Removes the snapshot `snapshot` of the service instance `seen`, and waits. Nothing else of the instance changes.
    pub fn remove_instance_snapshot(
        &self,
        seen: &InstanceState,
        snapshot: &str,
    ) -> Result<(), IncusError> {
        let (fresh, _) = self.owned_service(seen)?;
        named(snapshot)?;
        self.send_removal(&fresh, &SnapshotPart::Instance, snapshot)
    }

    /// Removes the snapshot `snapshot` of one custom volume the service instance `seen` mounts, and waits.
    pub fn remove_volume_snapshot(
        &self,
        seen: &InstanceState,
        pool: &str,
        volume: &str,
        snapshot: &str,
    ) -> Result<(), IncusError> {
        let (fresh, metadata) = self.owned_service(seen)?;
        self.mounted_volume(&fresh, &metadata, pool, volume)?;
        named(snapshot)?;
        let part = SnapshotPart::Volume {
            pool: pool.to_string(),
            volume: volume.to_string(),
        };
        self.send_removal(&fresh, &part, snapshot)
    }

    /// Snapshots a service instance with its volumes under the one name `snapshot`: first the instance (its root, config
    /// and devices), then each custom volume it mounts, in device order. It sends no stop and no start, so a running
    /// instance is snapshotted running, one volume after the other. Every check is made before the first write: the
    /// instance is the node's own service's, as read, and each volume carries its mark. A disk that is not a custom
    /// volume (a path of the host) is in no Incus snapshot and is named in the answer. When a part fails, each part
    /// already taken is removed again, and the failure names the part, what was removed and what could not be.
    pub fn snapshot_with_volumes(
        &self,
        seen: &InstanceState,
        snapshot: &str,
    ) -> Result<InstanceSnapshot, Box<PartFailure>> {
        let early = |part: SnapshotPart| move |error| PartFailure::before(part, error);
        named(snapshot).map_err(early(SnapshotPart::Instance))?;
        let (fresh, metadata) = self
            .owned_service(seen)
            .map_err(early(SnapshotPart::Instance))?;
        let (volumes, not_taken) = disks(&metadata);
        let mark = fresh.own_config[KERNEL_ENTRY_KEY].clone();
        for part in &volumes {
            if let SnapshotPart::Volume { pool, volume } = part {
                self.volume_of(&fresh.project, &mark, pool, volume)
                    .map_err(early(part.clone()))?;
            }
        }
        let mut taken: Vec<TakenPart> = Vec::new();
        for part in std::iter::once(SnapshotPart::Instance).chain(volumes) {
            let sent = match &part {
                SnapshotPart::Instance => self.snapshot_instance(&fresh, snapshot),
                SnapshotPart::Volume { pool, volume } => {
                    self.send_volume_snapshot(&fresh.project, pool, volume, snapshot)
                }
            };
            if let Err(error) = sent {
                let done: Vec<SnapshotPart> = taken.iter().map(|t| t.part.clone()).collect();
                let (done, left) = self.remove_parts(&fresh, snapshot, &done);
                return Err(Box::new(PartFailure {
                    part,
                    error,
                    done,
                    left,
                }));
            }
            taken.push(TakenPart {
                part,
                at_ms: now_ms(),
            });
        }
        Ok(InstanceSnapshot {
            name: snapshot.to_string(),
            taken,
            not_taken,
        })
    }

    /// Reads the names directly. A recursive list can omit snapshots that Incus fails to render.
    pub fn instance_snapshots(&self, seen: &InstanceState) -> Result<Vec<String>, IncusError> {
        self.allow(&seen.project)?;
        object_url(&seen.project, &seen.name)?;
        let path = format!("/1.0/instances/{}/snapshots", seen.name);
        let url = format!("{path}?project={}", seen.project);
        let listed = checked_metadata(self.send("GET", &url, None)?)?;
        let rows = listed
            .as_array()
            .ok_or_else(|| decode("the snapshot list is not a list"))?;
        let prefix = format!("{path}/");
        rows.iter()
            .map(|row| {
                row.as_str()
                    .and_then(|url| url.strip_prefix(&prefix))
                    .map(|name| name.split_once('?').map_or(name, |(name, _)| name))
                    .filter(|name| !name.is_empty() && !name.contains('/'))
                    .map(str::to_owned)
                    .ok_or_else(|| decode("a snapshot is not named by its address"))
            })
            .collect()
    }

    /// Puts the parts `parts` of the stopped service instance `seen` back to their snapshot `snapshot`: the instance first
    /// when it is named, then each volume in the order given. Every check is made before the first write, that each part
    /// holds the snapshot among them. A restore cannot be undone, so a failure names the part and the parts already
    /// restored, and nothing is put back.
    pub fn restore_with_volumes(
        &self,
        seen: &InstanceState,
        snapshot: &str,
        parts: &[SnapshotPart],
    ) -> Result<Vec<SnapshotPart>, Box<PartFailure>> {
        let first = parts.first().cloned().unwrap_or(SnapshotPart::Instance);
        let early = |part: SnapshotPart| move |error| PartFailure::before(part, error);
        named(snapshot).map_err(early(first.clone()))?;
        let (fresh, metadata) = self.owned_service(seen).map_err(early(first.clone()))?;
        if !fresh.is_stopped() {
            return Err(PartFailure::before(
                first,
                refused("the instance is not stopped"),
            ));
        }
        for part in parts {
            if let SnapshotPart::Volume { pool, volume } = part {
                self.mounted_volume(&fresh, &metadata, pool, volume)
                    .map_err(early(part.clone()))?;
            }
            // Each part must hold the snapshot, so a missing one is refused before anything is restored.
            let url = snapshot_url(&fresh, part, snapshot).map_err(early(part.clone()))?;
            self.send("GET", &url, None)
                .and_then(checked_envelope)
                .map_err(early(part.clone()))?;
        }
        let mut ordered: Vec<SnapshotPart> = parts.to_vec();
        ordered.sort_by_key(|part| !matches!(part, SnapshotPart::Instance));
        let mut done = Vec::new();
        for part in ordered {
            let sent = match &part {
                SnapshotPart::Instance => self.restore_snapshot(&fresh, snapshot),
                SnapshotPart::Volume { pool, volume } => {
                    self.send_volume_restore(&fresh.project, pool, volume, snapshot)
                }
            };
            if let Err(error) = sent {
                return Err(Box::new(PartFailure {
                    part,
                    error,
                    done,
                    left: Vec::new(),
                }));
            }
            done.push(part);
        }
        Ok(done)
    }

    /// Removes each part's snapshot, the last taken first. Returns the parts removed and the ones that were not.
    fn remove_parts(
        &self,
        fresh: &InstanceState,
        snapshot: &str,
        parts: &[SnapshotPart],
    ) -> (Vec<SnapshotPart>, Vec<(SnapshotPart, IncusError)>) {
        let mut done = Vec::new();
        let mut left = Vec::new();
        for part in parts.iter().rev() {
            match self.send_removal(fresh, part, snapshot) {
                Ok(()) => done.push(part.clone()),
                Err(error) => left.push((part.clone(), error)),
            }
        }
        (done, left)
    }

    /// Reads the instance `seen` names again and checks, before any write, that it is a service instance of the node's
    /// own service project that Arca made and is not removing, and that it is the instance `seen` was read from. The
    /// checks on `seen` come first, so a target that is not a service's is refused before any request.
    fn owned_service(&self, seen: &InstanceState) -> Result<(InstanceState, Value), IncusError> {
        self.allow(&seen.project)?;
        service_target(seen)?;
        let (fresh, metadata) = self.read_with(&seen.project, &seen.name)?;
        if fresh.uuid != seen.uuid || fresh.kind != seen.kind {
            return Err(refused("the instance was recreated since it was read"));
        }
        service_target(&fresh)?;
        if fresh.own_config.get(KERNEL_ENTRY_KEY) != seen.own_config.get(KERNEL_ENTRY_KEY) {
            return Err(refused("the instance's owner changed since it was read"));
        }
        Ok((fresh, metadata))
    }

    /// Checks that the instance mounts the custom volume `pool`/`volume` and that the volume carries its mark.
    fn mounted_volume(
        &self,
        fresh: &InstanceState,
        metadata: &Value,
        pool: &str,
        volume: &str,
    ) -> Result<(), IncusError> {
        let part = SnapshotPart::Volume {
            pool: pool.to_string(),
            volume: volume.to_string(),
        };
        if !disks(metadata).0.contains(&part) {
            return Err(refused("the instance does not mount that volume"));
        }
        self.volume_of(
            &fresh.project,
            &fresh.own_config[KERNEL_ENTRY_KEY],
            pool,
            volume,
        )
    }

    /// Reads one custom volume and checks that it carries `mark`, the instance's own, and no tombstone.
    fn volume_of(
        &self,
        project: &str,
        mark: &str,
        pool: &str,
        volume: &str,
    ) -> Result<(), IncusError> {
        let url = format!("{}?project={project}", volume_path(project, pool, volume)?);
        let metadata = checked_metadata(self.send("GET", &url, None)?)?;
        let config = &metadata["config"];
        if config[KERNEL_ENTRY_KEY].as_str() != Some(mark) {
            return Err(refused("the volume is not this service's own"));
        }
        if config.get(KERNEL_RETIRING_KEY).is_some() {
            return Err(refused("the volume is being removed"));
        }
        Ok(())
    }

    fn send_volume_snapshot(
        &self,
        project: &str,
        pool: &str,
        volume: &str,
        snapshot: &str,
    ) -> Result<(), IncusError> {
        let url = format!(
            "{}/snapshots?project={project}",
            volume_path(project, pool, volume)?
        );
        let body = json!({ "name": snapshot });
        let envelope = checked_envelope(self.send("POST", &url, Some(&body))?)?;
        self.wait_for(&envelope)
    }

    fn send_volume_restore(
        &self,
        project: &str,
        pool: &str,
        volume: &str,
        snapshot: &str,
    ) -> Result<(), IncusError> {
        let url = format!("{}?project={project}", volume_path(project, pool, volume)?);
        let body = json!({ "restore": snapshot });
        let envelope = checked_envelope(self.send("PUT", &url, Some(&body))?)?;
        self.wait_for(&envelope)
    }

    fn send_removal(
        &self,
        fresh: &InstanceState,
        part: &SnapshotPart,
        snapshot: &str,
    ) -> Result<(), IncusError> {
        let url = snapshot_url(fresh, part, snapshot)?;
        let envelope = checked_envelope(self.send("DELETE", &url, None)?)?;
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

    /// Gives one stopped instance the root of a local image pinned by fingerprint: Incus rebuilds the root disk from
    /// the copy the project's own image store holds and keeps the config, the devices (a data volume among them) and
    /// the uuid. `source` must be exactly the three fields of such an image; its fingerprint decides which image it is,
    /// and the alias is only a name kept in the record.
    pub fn rebuild_from_local(
        &self,
        seen: &InstanceState,
        source: &Value,
    ) -> Result<(), IncusError> {
        let fresh = self.read(&seen.project, &seen.name)?;
        if fresh.uuid != seen.uuid || fresh.kind != seen.kind {
            return Err(refused("the instance was recreated since it was read"));
        }
        if !fresh.is_stopped() {
            return Err(refused("the instance is not stopped"));
        }
        if !local_source(source) {
            return Err(refused(
                "the source is not a local image pinned by fingerprint",
            ));
        }
        self.rebuild(&fresh.project, &fresh.name, &json!({ "source": source }))
    }

    /// Whether the store the project reads holds an image with this fingerprint. For [`crate::function_run`] and
    /// `service_update`, which requires a local image there before it rebuilds from it.
    pub fn image_present(&self, project: &str, fingerprint: &str) -> Result<bool, IncusError> {
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
    /// from it. Returns whether it was deleted. For [`crate::function_run`] and the Automagik follower.
    pub fn delete_unused_image(
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

/// The mark prefix of a service's own objects: the instance Arca made for the service `name` carries `service-<name>`.
const SERVICE_ENTRY_PREFIX: &str = "service-";

/// One part of an instance's snapshot: the instance itself (its root, its config and its devices), or one custom volume
/// it mounts. Every part of one snapshot has the same name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum SnapshotPart {
    Instance,
    Volume { pool: String, volume: String },
}

impl fmt::Display for SnapshotPart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Instance => write!(f, "the instance"),
            Self::Volume { pool, volume } => write!(f, "the volume {volume} of the pool {pool}"),
        }
    }
}

/// One part a snapshot took, with when its operation ended, in milliseconds since the epoch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TakenPart {
    pub part: SnapshotPart,
    pub at_ms: u64,
}

/// A disk of the instance that no part of its snapshot holds, by its device name, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotTaken {
    pub device: String,
    pub why: &'static str,
}

/// What [`StateChanger::snapshot_with_volumes`] took: every part, in the order taken, and every disk not taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceSnapshot {
    pub name: String,
    pub taken: Vec<TakenPart>,
    pub not_taken: Vec<NotTaken>,
}

/// A part that failed: its error, the parts the call had done before it (removed again after a snapshot's failure,
/// restored after a restore's), and each part taken that could not be removed again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartFailure {
    pub part: SnapshotPart,
    pub error: IncusError,
    pub done: Vec<SnapshotPart>,
    pub left: Vec<(SnapshotPart, IncusError)>,
}

impl PartFailure {
    /// A failure before any write.
    fn before(part: SnapshotPart, error: IncusError) -> Box<Self> {
        Box::new(Self {
            part,
            error,
            done: Vec::new(),
            left: Vec::new(),
        })
    }
}

impl fmt::Display for PartFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} failed ({})", self.part, self.error)?;
        for (part, error) in &self.left {
            write!(f, "; {part} was not removed ({error})")?;
        }
        Ok(())
    }
}

/// Refuses, before any request, an instance that is not a service's of the node's own service project as Arca made it:
/// another project, a tenant's, an adopted one, one without a uuid, one without a service's mark, one being removed.
fn service_target(state: &InstanceState) -> Result<(), IncusError> {
    if arca_kernel::ownership::tenant_of_project(&state.project).is_some() {
        return Err(refused("a tenant's instance is not snapshotted here"));
    }
    let mark = state
        .own_config
        .get(KERNEL_ENTRY_KEY)
        .map_or("", String::as_str);
    if state
        .project
        .starts_with(crate::adopt::ADOPT_PROJECT_PREFIX)
        || mark.starts_with(crate::adopt::ADOPT_ENTRY_PREFIX)
    {
        return Err(refused("an adopted instance is not snapshotted here"));
    }
    if state.project != crate::exec::SERVICE_PROJECT {
        return Err(refused(
            "only an instance of the node's own service project is snapshotted here",
        ));
    }
    if mark.len() <= SERVICE_ENTRY_PREFIX.len() || !mark.starts_with(SERVICE_ENTRY_PREFIX) {
        return Err(refused("the instance is not a service's that Arca made"));
    }
    if state.uuid.is_none() {
        return Err(refused("the instance has no uuid"));
    }
    if state.own_config.contains_key(KERNEL_RETIRING_KEY) {
        return Err(refused("the instance is being removed"));
    }
    Ok(())
}

/// The custom volumes an instance's disks name, in device order, and every other disk but the root. Its own devices
/// are laid over the ones its profiles give it, as Incus expands them.
fn disks(metadata: &Value) -> (Vec<SnapshotPart>, Vec<NotTaken>) {
    let mut devices = metadata
        .get("expanded_devices")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if let Some(own) = metadata.get("devices").and_then(Value::as_object) {
        devices.extend(own.clone());
    }
    let mut named: BTreeMap<String, Value> = devices.into_iter().collect();
    named.retain(|_, device| device["type"] == "disk" && device["path"] != "/");
    let mut volumes = Vec::new();
    let mut not_taken = Vec::new();
    for (device, disk) in named {
        let pool = disk["pool"].as_str().unwrap_or("");
        let source = disk["source"].as_str().unwrap_or("");
        if !pool.is_empty() && plain_name(pool) && plain_name(source) {
            let part = SnapshotPart::Volume {
                pool: pool.to_string(),
                volume: source.to_string(),
            };
            if !volumes.contains(&part) {
                volumes.push(part);
            }
        } else if source.starts_with('/') {
            not_taken.push(NotTaken {
                device,
                why: "a path of the host is in no Incus snapshot",
            });
        } else {
            not_taken.push(NotTaken {
                device,
                why: "it is not a custom volume of a storage pool",
            });
        }
    }
    (volumes, not_taken)
}

/// Refuses a snapshot name that is not a plain path segment.
fn named(snapshot: &str) -> Result<(), IncusError> {
    if plain_name(snapshot) {
        Ok(())
    } else {
        Err(refused("the snapshot name is not plain"))
    }
}

/// The path of one part's snapshot, with its project.
fn snapshot_url(
    fresh: &InstanceState,
    part: &SnapshotPart,
    snapshot: &str,
) -> Result<String, IncusError> {
    let project = &fresh.project;
    Ok(match part {
        SnapshotPart::Instance => {
            object_url(project, &fresh.name)?;
            format!(
                "/1.0/instances/{}/snapshots/{snapshot}?project={project}",
                fresh.name
            )
        }
        SnapshotPart::Volume { pool, volume } => format!(
            "{}/snapshots/{snapshot}?project={project}",
            volume_path(project, pool, volume)?
        ),
    })
}

/// The path of one custom volume, without its query.
fn volume_path(project: &str, pool: &str, volume: &str) -> Result<String, IncusError> {
    if !plain_name(project) || !plain_name(pool) || !plain_name(volume) {
        return Err(refused("the project, pool or volume name is not plain"));
    }
    Ok(format!("/1.0/storage-pools/{pool}/volumes/custom/{volume}"))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
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

/// Whether `source` is a local image pinned by fingerprint, and nothing more: the only source a rebuild from the
/// project's own image store may name.
fn local_source(source: &Value) -> bool {
    let Some(fields) = source.as_object() else {
        return false;
    };
    fields.len() == 3
        && source["type"] == "image"
        && source["alias"]
            .as_str()
            .is_some_and(|alias| !alias.is_empty())
        && source["fingerprint"].as_str().is_some_and(|hex| {
            hex.len() == 64
                && hex
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
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
