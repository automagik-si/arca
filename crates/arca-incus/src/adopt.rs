//! The adoption writer: the one path that writes outside the kernel's project family. It takes one existing container from the
//! default project into an adoption project of its own, and back, through a closed table of steps. It is compiled only with
//! the `write` feature.
//!
//! Each step names the project, the owner class and the status it expects the instance in ([`AdoptWriter::expect`]). Before
//! each request the writer reads the instance there and refuses unless it is a container whose `volatile.uuid` is the
//! recorded pre-image's, in that class and that status. A step whose outcome already reads true sends nothing. Every request
//! is one of the table's ([`AdoptWriter::requests`]): no DELETE, no write to a network, a profile or an ACL, and no project
//! write but the one create. Every request passes `exchange`, whose guard refuses a project PATCH.
//!
//! The NIC is pinned on the device itself before the move and never unpinned: `devices.eth0` names the network, the pre-image
//! MAC and the pre-image IPv4 ([`pin_patch`]). No step removes or renames `eth0`, so Incus's `deviceVolatileReset` never clears a
//! key the instance's MAC depends on.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::time::Duration;

use arca_kernel::desired::EntryId;
use arca_kernel::ownership::{IncusKind, KERNEL_ENTRY_KEY, ObjectRef, OwnerClass, classify};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::read::{
    IncusError, IncusRead, TIMEOUT, Unconfirmed, UnixIncus, checked_envelope, operation_names,
    running_operations,
};
use crate::write::{ProjectFamily, UnixIncusWriter};

/// The prefix of an adopted instance's kernel entry: the entry is `adopt-<instance>`.
pub const ADOPT_ENTRY_PREFIX: &str = "adopt-";
/// The prefix every adoption project's name carries.
pub const ADOPT_PROJECT_PREFIX: &str = "arcak-adopt-";
/// The prefix of the adoption snapshot's name: `arca-adopt-<the plan digest's first 12 hex characters>`.
pub const SNAPSHOT_PREFIX: &str = "arca-adopt-";
/// The project an adopted instance comes from and is released to.
pub const SOURCE_PROJECT: &str = "default";
/// The one NIC an adopted instance has, and the device the pin sets.
pub const NIC: &str = "eth0";
/// The seconds a graceful stop is given before the forced one.
pub const STOP_TIMEOUT_SECONDS: u64 = 60;
/// The kernel's own config namespace. A Foreign instance carries no key in it.
const KERNEL_NAMESPACE: &str = "user.arcakernel.";

/// One allowlisted adoption: the instance in the default project and the adoption project it moves to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdoptTarget {
    pub instance: String,
    pub to: String,
}

impl AdoptTarget {
    /// Checks the names. The instance is `^[a-z][a-z0-9-]{0,56}$`, so its entry `adopt-<instance>` is a valid entry id. The
    /// project is `arcak-adopt-` and then letters, digits or hyphens, at most 63 bytes in all.
    pub fn check(&self) -> Result<(), &'static str> {
        let bytes = self.instance.as_bytes();
        let plain = |b: &u8| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-';
        if !(1..=57).contains(&bytes.len())
            || !bytes[0].is_ascii_lowercase()
            || !bytes.iter().all(plain)
        {
            return Err("an adopted instance's name is not plain");
        }
        let suffix = self.to.strip_prefix(ADOPT_PROJECT_PREFIX).unwrap_or("");
        if suffix.is_empty() || self.to.len() > 63 || !suffix.as_bytes().iter().all(plain) {
            return Err("an adoption project must be arcak-adopt- and a plain name");
        }
        Ok(())
    }

    /// The kernel entry the adopted instance is marked with: `adopt-<instance>`.
    pub fn entry(&self) -> Option<EntryId> {
        EntryId::parse(&format!("{ADOPT_ENTRY_PREFIX}{}", self.instance))
    }
}

/// The instance as the plan found it in the default project. `config` is its own config without any `volatile.*` key: its
/// uuid and MAC are kept in their own fields. `devices` are its expanded devices, so the NIC its profile gives is recorded.
/// `config_hash` is the watch's hash of its own config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PreImage {
    pub uuid: String,
    pub created_at: String,
    pub mac: String,
    pub ipv4: Ipv4Addr,
    pub profiles: Vec<String>,
    pub config: BTreeMap<String, String>,
    pub devices: BTreeMap<String, BTreeMap<String, String>>,
    pub config_hash: String,
}

impl PreImage {
    /// The pin of this pre-image: its MAC and IPv4, on the network its `eth0` names.
    pub fn pin(&self) -> Option<Pin> {
        let network = self.devices.get(NIC)?.get("network")?.clone();
        Some(Pin {
            mac: self.mac.clone(),
            ipv4: self.ipv4,
            network,
        })
    }
}

/// The NIC pin: the MAC and the IPv4 set on `eth0` itself, on its network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    pub mac: String,
    pub ipv4: Ipv4Addr,
    pub network: String,
}

/// The pinned `eth0` device, key by key.
pub fn pin_device(pin: &Pin) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("type".to_string(), "nic".to_string()),
        ("name".to_string(), NIC.to_string()),
        ("network".to_string(), pin.network.clone()),
        ("hwaddr".to_string(), pin.mac.clone()),
        ("ipv4.address".to_string(), pin.ipv4.to_string()),
    ])
}

/// The PATCH body that pins the NIC: `devices.eth0` alone, which Incus replaces whole and which overrides the profile's.
pub fn pin_patch(pin: &Pin) -> Value {
    json!({ "devices": { NIC: pin_device(pin) } })
}

/// The create body of the adoption project. It is marked `adopt-<instance>` from the moment it exists. It is restricted, and
/// its grant names the attach networks and nothing else; it allows snapshots and backups. It reads the default project's
/// networks, and has its own profiles, volumes and images.
pub fn adoption_project_body(target: &AdoptTarget, attach: &[String]) -> Value {
    json!({
        "name": target.to,
        "description": format!("Arca adoption of {}", target.instance),
        "config": {
            KERNEL_ENTRY_KEY: format!("{ADOPT_ENTRY_PREFIX}{}", target.instance),
            "restricted": "true",
            "restricted.networks.access": attach.join(","),
            "restricted.snapshots": "allow",
            "restricted.backups": "allow",
            "features.networks": "false",
            "features.profiles": "true",
            "features.storage.volumes": "true",
            "features.images": "true",
        },
    })
}

/// The steps, adoption then release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AdoptStep {
    CreateProject,
    Stop,
    Snapshot,
    PinNic,
    Move,
    Mark,
    Start,
    ReleaseStop,
    Unmark,
    MoveBack,
    ReleaseStart,
}

impl AdoptStep {
    /// The adoption, in order.
    pub const ADOPT: [AdoptStep; 7] = [
        AdoptStep::CreateProject,
        AdoptStep::Stop,
        AdoptStep::Snapshot,
        AdoptStep::PinNic,
        AdoptStep::Move,
        AdoptStep::Mark,
        AdoptStep::Start,
    ];
    /// The release, in order.
    pub const RELEASE: [AdoptStep; 4] = [
        AdoptStep::ReleaseStop,
        AdoptStep::Unmark,
        AdoptStep::MoveBack,
        AdoptStep::ReleaseStart,
    ];
}

/// What a step did: it sent its request, or its outcome already held and it sent nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StepOutcome {
    Sent,
    AlreadyDone,
}

/// The project a step expects the instance in: the default project or the adoption project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Default,
    To,
}

/// The owner class a step expects: no owner's mark, or this adoption's entry mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpectClass {
    Foreign,
    Adopted,
}

/// Where, in which class and in which status a step expects the instance before it sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Expect {
    pub project: Side,
    pub class: ExpectClass,
    pub status: &'static str,
}

/// One request of the step table: the step, the method, the path and the body; whether it carries the ETag of the read before
/// it; and, for the forced stop, the condition it is sent under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdoptRequest {
    pub step: AdoptStep,
    pub method: &'static str,
    pub path: String,
    pub if_match: bool,
    pub body: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub only_if: Option<&'static str>,
}

/// The instance as one read showed it.
struct Seen {
    etag: String,
    class: OwnerClass,
    status: String,
    kernel_keys: bool,
    devices: BTreeMap<String, BTreeMap<String, String>>,
    expanded: BTreeMap<String, BTreeMap<String, String>>,
}

/// The adoption writer for one allowlisted target and its recorded pre-image.
#[derive(Debug, Clone)]
pub struct AdoptWriter {
    writer: UnixIncusWriter,
    read: UnixIncus,
    target: AdoptTarget,
    attach: Vec<String>,
    pre: PreImage,
    pin: Pin,
    entry: EntryId,
    snapshot: Option<String>,
}

impl AdoptWriter {
    /// A writer for `target` over the Incus socket at `socket`. `attach` lists the networks the adoption project may grant, and
    /// it must hold the network the pre-image's NIC names. Nothing is read or sent here.
    pub fn new(
        socket: PathBuf,
        target: AdoptTarget,
        attach: Vec<String>,
        pre: PreImage,
    ) -> Result<Self, IncusError> {
        target.check().map_err(refused)?;
        let entry = target
            .entry()
            .ok_or_else(|| refused("the adoption entry is not a valid entry id"))?;
        if attach.is_empty() || !attach.iter().all(|name| plain_name(name)) {
            return Err(refused("an attach network is not a plain name"));
        }
        let pin = pre
            .pin()
            .ok_or_else(|| refused("the pre-image names no network on eth0"))?;
        if !attach.contains(&pin.network) {
            return Err(refused("the NIC's network is not an attach network"));
        }
        if !mac_ok(&pin.mac) || pre.uuid.is_empty() {
            return Err(refused("the pre-image has no plain MAC or uuid"));
        }
        let family = ProjectFamily::new([target.to.as_str()], [])
            .map_err(|_| refused("the adoption project cannot be named"))?;
        let writer = UnixIncusWriter::new(socket.clone(), entry.clone(), BTreeMap::new(), family);
        Ok(Self {
            writer,
            read: UnixIncus::new(socket),
            target,
            attach,
            pre,
            pin,
            entry,
            snapshot: None,
        })
    }

    /// The same writer with the adoption snapshot's name, `arca-adopt-` and 12 lowercase hex characters. The Snapshot step
    /// refuses without it.
    pub fn with_snapshot(mut self, name: &str) -> Result<Self, IncusError> {
        let hex = name.strip_prefix(SNAPSHOT_PREFIX).unwrap_or("");
        if hex.len() != 12
            || !hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(refused(
                "the snapshot name is not arca-adopt- and 12 hex characters",
            ));
        }
        self.snapshot = Some(name.to_string());
        Ok(self)
    }

    /// The same writer with another overall deadline for an operation.
    pub fn with_operation_deadline(mut self, deadline: Duration) -> Self {
        self.writer = self.writer.with_operation_deadline(deadline);
        self
    }

    /// The pin this writer sets.
    pub fn pin(&self) -> &Pin {
        &self.pin
    }

    /// Where, in which class and in which status `step` expects the instance (Decision table in `docs/OWNERSHIP.md`).
    pub fn expect(step: AdoptStep) -> Expect {
        use AdoptStep as S;
        use ExpectClass::{Adopted, Foreign};
        use Side::{Default, To};
        let (project, class, status) = match step {
            S::CreateProject | S::Stop => (Default, Foreign, "Running"),
            S::Snapshot | S::PinNic | S::Move => (Default, Foreign, "Stopped"),
            S::Mark => (To, Foreign, "Stopped"),
            S::Start | S::Unmark => (To, Adopted, "Stopped"),
            S::ReleaseStop => (To, Adopted, "Running"),
            S::MoveBack => (To, Foreign, "Stopped"),
            S::ReleaseStart => (Default, Foreign, "Stopped"),
        };
        Expect {
            project,
            class,
            status,
        }
    }

    /// Every request `step` may send, in the order it sends them. A step sends the forced stop only under its condition.
    pub fn requests(&self, step: AdoptStep) -> Vec<AdoptRequest> {
        let i = &self.target.instance;
        let project = self.side(Self::expect(step).project);
        let instance = format!("/1.0/instances/{i}?project={project}");
        let state = format!("/1.0/instances/{i}/state?project={project}");
        let request = |method, path: &str, if_match, body| AdoptRequest {
            step,
            method,
            path: path.to_string(),
            if_match,
            body,
            only_if: None,
        };
        let mark = |value: String| json!({ "config": { KERNEL_ENTRY_KEY: value } });
        match step {
            AdoptStep::CreateProject => vec![request(
                "POST",
                "/1.0/projects",
                false,
                adoption_project_body(&self.target, &self.attach),
            )],
            AdoptStep::Stop | AdoptStep::ReleaseStop => vec![
                request(
                    "PUT",
                    &state,
                    false,
                    json!({ "action": "stop", "timeout": STOP_TIMEOUT_SECONDS, "force": false }),
                ),
                AdoptRequest {
                    only_if: Some("the instance still runs after the graceful stop"),
                    ..request(
                        "PUT",
                        &state,
                        false,
                        json!({ "action": "stop", "force": true }),
                    )
                },
            ],
            AdoptStep::Snapshot => {
                let name = self.snapshot.clone().unwrap_or_default();
                let path = format!("/1.0/instances/{i}/snapshots?project={project}");
                vec![request(
                    "POST",
                    &path,
                    false,
                    json!({ "name": name, "stateful": false }),
                )]
            }
            AdoptStep::PinNic => vec![request("PATCH", &instance, true, pin_patch(&self.pin))],
            AdoptStep::Move => {
                let body = json!({ "name": i, "project": self.target.to });
                vec![request("POST", &instance, false, body)]
            }
            AdoptStep::MoveBack => {
                let body = json!({ "name": i, "project": SOURCE_PROJECT });
                vec![request("POST", &instance, false, body)]
            }
            AdoptStep::Mark => vec![request(
                "PATCH",
                &instance,
                true,
                mark(self.entry.0.clone()),
            )],
            AdoptStep::Unmark => vec![request("PATCH", &instance, true, mark(String::new()))],
            AdoptStep::Start | AdoptStep::ReleaseStart => {
                vec![request("PUT", &state, false, json!({ "action": "start" }))]
            }
        }
    }

    /// Runs one step. No step runs while an operation names the instance, in either project, or the adoption project. The
    /// instance is read first; a step whose outcome already holds returns [`StepOutcome::AlreadyDone`] and sends nothing.
    /// Otherwise the step's expected project, class and status are checked, then its request is sent and its operation waited
    /// on, and the instance is read back. [`IncusError::Refused`] means nothing was sent by this step.
    pub fn run(&self, step: AdoptStep) -> Result<StepOutcome, IncusError> {
        self.idle()?;
        match step {
            AdoptStep::CreateProject => self.create_project(),
            AdoptStep::Move | AdoptStep::MoveBack => self.move_step(step),
            _ => self.instance_step(step),
        }
    }

    fn side(&self, side: Side) -> &str {
        match side {
            Side::Default => SOURCE_PROJECT,
            Side::To => &self.target.to,
        }
    }

    fn instance_ref(&self, project: &str) -> ObjectRef {
        ObjectRef {
            kind: IncusKind::Instance,
            project: project.to_string(),
            parent: None,
            name: self.target.instance.clone(),
            uuid: Some(self.pre.uuid.clone()),
        }
    }

    fn project_ref(&self) -> ObjectRef {
        ObjectRef {
            kind: IncusKind::Project,
            project: self.target.to.clone(),
            parent: None,
            name: self.target.to.clone(),
            uuid: None,
        }
    }

    /// Refuses while an operation names the instance in either project, or the adoption project. A listing that cannot be read
    /// is an error, and the step is not attempted.
    fn idle(&self) -> Result<(), IncusError> {
        let (i, to) = (&self.target.instance, &self.target.to);
        let busy = running_operations(&self.read)?.iter().any(|op| {
            operation_names(op, IncusKind::Instance, SOURCE_PROJECT, None, i)
                || operation_names(op, IncusKind::Instance, to, None, i)
                || operation_names(op, IncusKind::Project, to, None, to)
        });
        if busy {
            return Err(refused(
                "an operation is still running on the instance or its project",
            ));
        }
        Ok(())
    }

    /// Reads the instance in `project`: `None` when it is not there. A non-container, or one whose uuid is not the recorded
    /// one, is refused.
    fn read_in(&self, project: &str) -> Result<Option<Seen>, IncusError> {
        let (metadata, etag) = match self.writer.get_etag(&self.instance_ref(project)) {
            Ok(found) => found,
            Err(IncusError::Status { code: 404, .. }) => return Ok(None),
            Err(error) => return Err(error),
        };
        if metadata.get("type").and_then(Value::as_str) != Some("container") {
            return Err(refused("the instance is not a container"));
        }
        let config = text_map(metadata.get("config"))
            .ok_or_else(|| decode("the instance's config is not a map of text"))?;
        if config.get("volatile.uuid") != Some(&self.pre.uuid) {
            return Err(refused("the instance is not the one the plan recorded"));
        }
        Ok(Some(Seen {
            etag,
            class: classify(&config, &BTreeMap::new()),
            status: metadata
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            kernel_keys: config.keys().any(|key| key.starts_with(KERNEL_NAMESPACE)),
            devices: devices_of(metadata.get("devices"))?,
            expanded: devices_of(metadata.get("expanded_devices"))?,
        }))
    }

    /// Whether a read shows the class a step expects. Foreign is no owner's mark and no key in the kernel's namespace.
    fn class_is(&self, seen: &Seen, class: ExpectClass) -> bool {
        match class {
            ExpectClass::Foreign => seen.class == OwnerClass::Foreign && !seen.kernel_keys,
            ExpectClass::Adopted => {
                seen.class
                    == OwnerClass::Kernel {
                        entry: self.entry.clone(),
                    }
            }
        }
    }

    fn pinned(&self, seen: &Seen) -> bool {
        seen.devices.get(NIC) == Some(&pin_device(&self.pin))
    }

    /// Refuses a read that is not in the class and the status `step` expects.
    fn before(&self, step: AdoptStep, seen: &Seen) -> Result<(), IncusError> {
        let expect = Self::expect(step);
        if !self.class_is(seen, expect.class) {
            return Err(refused(
                "the instance is not in the owner class the step expects",
            ));
        }
        if seen.status != expect.status {
            return Err(refused(
                "the instance is not in the status the step expects",
            ));
        }
        Ok(())
    }

    /// The instance read in the step's own project, which it must be in.
    fn located(&self, step: AdoptStep) -> Result<Seen, IncusError> {
        self.read_in(self.side(Self::expect(step).project))?
            .ok_or_else(|| refused("the instance is not in the project the step expects"))
    }

    /// Whether the adoption project is this adoption's: it carries this entry's mark.
    fn project_is_ours(&self) -> Result<Option<bool>, IncusError> {
        match self.writer.get_etag(&self.project_ref()) {
            Ok((metadata, _)) => {
                let config = text_map(metadata.get("config"))
                    .ok_or_else(|| decode("the project's config is not a map of text"))?;
                let ours = classify(&config, &BTreeMap::new())
                    == OwnerClass::Kernel {
                        entry: self.entry.clone(),
                    };
                Ok(Some(ours))
            }
            Err(IncusError::Status { code: 404, .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Whether the adoption project holds no instance but this one.
    fn holds_only_ours(&self) -> Result<bool, IncusError> {
        let listing = self
            .read
            .get("/1.0/instances?all-projects=true&recursion=1")?;
        let rows = listing
            .as_array()
            .ok_or_else(|| decode("the instance listing is not a list"))?;
        Ok(rows.iter().all(|row| {
            row.get("project").and_then(Value::as_str) != Some(self.target.to.as_str())
                || row.get("name").and_then(Value::as_str) == Some(self.target.instance.as_str())
        }))
    }

    fn create_project(&self) -> Result<StepOutcome, IncusError> {
        match self.project_is_ours()? {
            Some(true) if self.holds_only_ours()? => return Ok(StepOutcome::AlreadyDone),
            Some(_) => {
                return Err(refused(
                    "the adoption project exists and is not this adoption's",
                ));
            }
            None => {}
        }
        let seen = self.located(AdoptStep::CreateProject)?;
        self.before(AdoptStep::CreateProject, &seen)?;
        self.send_all(AdoptStep::CreateProject, None)?;
        match self.project_is_ours() {
            Ok(Some(true)) => Ok(StepOutcome::Sent),
            _ => Err(self.unconfirmed(
                &self.project_ref(),
                "the adoption project does not read back marked",
            )),
        }
    }

    fn move_step(&self, step: AdoptStep) -> Result<StepOutcome, IncusError> {
        let (from, to) = if step == AdoptStep::Move {
            (SOURCE_PROJECT, self.target.to.as_str())
        } else {
            (self.target.to.as_str(), SOURCE_PROJECT)
        };
        let seen = match (self.read_in(from)?, self.read_in(to)?) {
            (None, Some(_)) => return Ok(StepOutcome::AlreadyDone),
            (Some(_), Some(_)) => return Err(refused("the instance shows in both projects")),
            (None, None) => return Err(refused("the instance is in neither project")),
            (Some(seen), None) => seen,
        };
        self.before(step, &seen)?;
        if !self.pinned(&seen) {
            return Err(refused("the instance's NIC is not pinned"));
        }
        if self.project_is_ours()? != Some(true) {
            return Err(refused("the adoption project is not this adoption's"));
        }
        self.send_all(step, None)?;
        match self.read_in(to) {
            Ok(Some(_)) => Ok(StepOutcome::Sent),
            _ => Err(self.unconfirmed(
                &self.instance_ref(to),
                "the instance does not read back in its new project",
            )),
        }
    }

    /// Whether a step's outcome already holds on a read of the instance in the step's own project.
    fn done(&self, step: AdoptStep, seen: &Seen) -> Result<bool, IncusError> {
        use ExpectClass::{Adopted, Foreign};
        Ok(match step {
            AdoptStep::Stop => self.class_is(seen, Foreign) && seen.status == "Stopped",
            AdoptStep::ReleaseStop => self.class_is(seen, Adopted) && seen.status == "Stopped",
            AdoptStep::Snapshot => self.class_is(seen, Foreign) && self.snapshot_exists()?,
            AdoptStep::PinNic => self.class_is(seen, Foreign) && self.pinned(seen),
            AdoptStep::Mark => self.class_is(seen, Adopted),
            AdoptStep::Unmark => self.class_is(seen, Foreign),
            AdoptStep::Start => self.class_is(seen, Adopted) && seen.status == "Running",
            AdoptStep::ReleaseStart => self.class_is(seen, Foreign) && seen.status == "Running",
            AdoptStep::CreateProject | AdoptStep::Move | AdoptStep::MoveBack => false,
        })
    }

    fn snapshot_exists(&self) -> Result<bool, IncusError> {
        let name = self
            .snapshot
            .as_deref()
            .ok_or_else(|| refused("the writer was given no snapshot name"))?;
        let path = format!(
            "/1.0/instances/{}/snapshots/{name}?project={SOURCE_PROJECT}",
            self.target.instance
        );
        match self.read.get(&path) {
            Ok(_) => Ok(true),
            Err(IncusError::Status { code: 404, .. }) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Stop, Snapshot, PinNic, Mark, Start and their release twins: each acts on the instance in one project.
    fn instance_step(&self, step: AdoptStep) -> Result<StepOutcome, IncusError> {
        let seen = self.located(step)?;
        if self.done(step, &seen)? {
            return Ok(StepOutcome::AlreadyDone);
        }
        self.before(step, &seen)?;
        if step == AdoptStep::PinNic && seen.expanded.get(NIC) != self.pre.devices.get(NIC) {
            return Err(refused("the instance's NIC changed since the plan"));
        }
        match step {
            AdoptStep::Stop | AdoptStep::ReleaseStop => return self.stop(step),
            // A config or device change is sent under the ETag of the read its checks were made on.
            AdoptStep::PinNic | AdoptStep::Mark | AdoptStep::Unmark => {
                self.send_all(step, Some(&seen.etag))?
            }
            _ => self.send_all(step, None)?,
        }
        let project = self.side(Self::expect(step).project).to_string();
        let back = self.read_in(&project);
        let confirmed = match &back {
            Ok(Some(seen)) => self.done(step, seen).unwrap_or(false),
            _ => false,
        };
        if confirmed {
            Ok(StepOutcome::Sent)
        } else {
            Err(self.unconfirmed(
                &self.instance_ref(&project),
                "the read-back does not show the step's outcome",
            ))
        }
    }

    /// The stop: a graceful stop with its timeout, then a forced stop when the instance still runs. A graceful stop whose
    /// operation fails is not an error by itself; a forced stop that still leaves the instance running is.
    fn stop(&self, step: AdoptStep) -> Result<StepOutcome, IncusError> {
        let [graceful, forced] = <[AdoptRequest; 2]>::try_from(self.requests(step))
            .map_err(|_| refused("the stop is not two requests"))?;
        match self.send(&graceful, None) {
            Ok(()) | Err(IncusError::Status { .. }) => {}
            Err(error) => return Err(error),
        }
        let still = self.located(step)?;
        if still.status == "Running" {
            match self.send(&forced, None) {
                Ok(()) | Err(IncusError::Status { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        let project = self.side(Self::expect(step).project).to_string();
        match self.read_in(&project) {
            Ok(Some(seen)) if self.done(step, &seen)? => Ok(StepOutcome::Sent),
            _ => Err(self.unconfirmed(
                &self.instance_ref(&project),
                "the forced stop did not stop the instance",
            )),
        }
    }

    /// Sends every request of `step` that carries no condition.
    fn send_all(&self, step: AdoptStep, etag: Option<&str>) -> Result<(), IncusError> {
        for request in self.requests(step).iter().filter(|r| r.only_if.is_none()) {
            self.send(request, etag)?;
        }
        Ok(())
    }

    /// Sends one request and waits on its operation. A request that is not one of the table's is refused before any byte is
    /// written, and so is one whose ETag does not match its `if_match`.
    fn send(&self, request: &AdoptRequest, etag: Option<&str>) -> Result<(), IncusError> {
        let listed = AdoptStep::ADOPT
            .iter()
            .chain(AdoptStep::RELEASE.iter())
            .any(|step| self.requests(*step).contains(request));
        if !listed {
            return Err(refused("the request is not in the adoption step table"));
        }
        if request.if_match != etag.is_some() {
            return Err(refused("the request's ETag does not match the table"));
        }
        if request.step == AdoptStep::Snapshot && self.snapshot.is_none() {
            return Err(refused("the writer was given no snapshot name"));
        }
        let framed = self.writer.send_checked(
            request.method,
            &request.path,
            etag,
            Some(&request.body),
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
                    .ok_or_else(|| decode("the answer names no operation"))?;
                self.writer.wait_on(id)
            }
        }
    }

    fn unconfirmed(&self, target: &ObjectRef, reason: &str) -> IncusError {
        IncusError::AppliedUnconfirmed(Box::new(Unconfirmed {
            target: target.clone(),
            uuid: target.uuid.clone(),
            owner: None,
            reason: reason.to_string(),
        }))
    }
}

/// A network name that is safe in a grant: letters, digits, hyphens and underscores.
fn plain_name(name: &str) -> bool {
    (1..=63).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A MAC as Incus writes it: six lowercase hex pairs joined by colons.
pub fn mac_ok(mac: &str) -> bool {
    mac.len() == 17
        && mac.split(':').count() == 6
        && mac.split(':').all(|pair| {
            pair.len() == 2
                && pair
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
}

fn text_map(value: Option<&Value>) -> Option<BTreeMap<String, String>> {
    match value {
        None | Some(Value::Null) => Some(BTreeMap::new()),
        Some(Value::Object(map)) => map
            .iter()
            .map(|(key, value)| value.as_str().map(|text| (key.clone(), text.to_string())))
            .collect(),
        Some(_) => None,
    }
}

/// A device map: each device a map of text.
pub fn devices_of(
    value: Option<&Value>,
) -> Result<BTreeMap<String, BTreeMap<String, String>>, IncusError> {
    let Some(Value::Object(devices)) = value else {
        return Ok(BTreeMap::new());
    };
    devices
        .iter()
        .map(|(name, device)| {
            text_map(Some(device))
                .map(|device| (name.clone(), device))
                .ok_or_else(|| decode("a device is not a map of text"))
        })
        .collect()
}

fn refused(reason: &str) -> IncusError {
    IncusError::Refused(reason.to_string())
}

fn decode(reason: &str) -> IncusError {
    IncusError::Decode(reason.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{FakeIncus, FakeState};

    fn writer(socket: PathBuf) -> AdoptWriter {
        let device = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        let pre = PreImage {
            uuid: "00000000-0000-4000-8000-0000000000a1".to_string(),
            created_at: String::new(),
            mac: "10:66:6a:00:00:01".to_string(),
            ipv4: Ipv4Addr::new(192, 0, 2, 131),
            profiles: vec!["default".to_string()],
            config: BTreeMap::new(),
            devices: BTreeMap::from([(
                NIC.to_string(),
                device(&[("type", "nic"), ("name", NIC), ("network", "br1")]),
            )]),
            config_hash: String::new(),
        };
        let target = AdoptTarget {
            instance: "svc1".to_string(),
            to: "arcak-adopt-svc1".to_string(),
        };
        AdoptWriter::new(socket, target, vec!["br1".to_string()], pre)
            .and_then(|writer| writer.with_snapshot("arca-adopt-0123456789ab"))
            .expect("the writer")
    }

    #[test]
    fn a_request_outside_the_step_table_is_refused_before_any_byte_is_sent() {
        let dir = std::env::temp_dir().join(format!("arca-adopt-unit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the directory");
        let state = FakeState {
            server: json!({}),
            projects: vec![json!({ "name": "default", "config": {} })],
            instances: Vec::new(),
            pools: Vec::new(),
            networks: Vec::new(),
        };
        let fake = FakeIncus::start(&dir, state).expect("the fake");
        let writer = writer(fake.socket().to_path_buf());
        let request = |method, path: &str, body| AdoptRequest {
            step: AdoptStep::Mark,
            method,
            path: path.to_string(),
            if_match: false,
            body,
            only_if: None,
        };
        let outside = [
            request("DELETE", "/1.0/instances/svc1?project=default", Value::Null),
            request("DELETE", "/1.0/projects/arcak-adopt-svc1", Value::Null),
            request(
                "PATCH",
                "/1.0/projects/arcak-adopt-svc1",
                json!({ "config": {} }),
            ),
            request(
                "PUT",
                "/1.0/projects/arcak-adopt-svc1",
                json!({ "config": {} }),
            ),
            request(
                "PATCH",
                "/1.0/networks/br1",
                json!({ "config": { "ipv4.address": "none" } }),
            ),
            request(
                "PUT",
                "/1.0/profiles/default?project=default",
                json!({ "devices": {} }),
            ),
            request(
                "POST",
                "/1.0/instances?project=default",
                json!({ "name": "svc2" }),
            ),
            request(
                "POST",
                "/1.0/instances/svc1?project=default",
                json!({ "name": "svc1", "project": "bench" }),
            ),
            request(
                "PATCH",
                "/1.0/instances/svc1?project=default",
                json!({ "config": { "security.privileged": "true" } }),
            ),
            request(
                "PATCH",
                "/1.0/instances/svc1?project=default",
                json!({ "devices": { "eth0": {} } }),
            ),
            request(
                "PUT",
                "/1.0/instances/svc1/state?project=default",
                json!({ "action": "restart" }),
            ),
            request(
                "POST",
                "/1.0/network-acls?project=default",
                json!({ "name": "a1" }),
            ),
        ];
        for request in &outside {
            let sent = writer.send(request, None);
            assert!(
                matches!(sent, Err(IncusError::Refused(_))),
                "{request:?}: {sent:?}"
            );
        }
        // A table request is refused too when its ETag does not match the table: Mark is sent under If-Match only.
        let mark = writer.requests(AdoptStep::Mark).remove(0);
        assert!(matches!(
            writer.send(&mark, None),
            Err(IncusError::Refused(_))
        ));
        let start = writer.requests(AdoptStep::Start).remove(0);
        assert!(matches!(
            writer.send(&start, Some("\"x\"")),
            Err(IncusError::Refused(_))
        ));
        assert!(
            fake.requests().is_empty(),
            "no byte reached the fake: {:?}",
            fake.requests()
        );
        drop(fake);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
