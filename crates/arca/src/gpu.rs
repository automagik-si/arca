//! The GPU assignment backend: the inventory and instances an action proposal checks, and the owner-approved device
//! changes an approved action runs.
//!
//! Everything is keyed by the PCI address. Two cards of the same product are two rows with two addresses, and an action
//! names exactly one of them. The inventory comes from Incus's `GET /1.0/resources`; the instances from the ordinary
//! `instances` listing; the device change from the narrow [`arca_incus::DeviceAssigner`].
//!
//! A write is available only when the node runs in kernel mode, and only to an instance in the kernel's own project family
//! (OWNERSHIP Rules 5 and 8). The project, the instance type and the instance's `volatile.uuid` are resolved when the
//! proposal is made and are part of its arguments, so the owner's digest binds them; execution re-reads the instance under
//! that project and refuses one that moved or was recreated. A watch-only node builds no device writer at all.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use arca_incus::{DeviceAssigner, GpuChange, IncusRead, ProjectFamily, UnixIncus};
use arca_kernel::ownership::IncusKind;
use arca_metrics::gpu::{GpuCard, Nvidia, NvidiaError, NvidiaSmi, normalize_pci, read_inventory};
use serde_json::Value;

pub use arca_incus::GpuChange as GpuChanged;

/// The instance list, recursed once: each row is the instance object, with its project, name, type and own config.
pub const INSTANCES_PATH: &str = "/1.0/instances?all-projects=true&recursion=1";

/// One Incus instance an action may name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuInstance {
    pub project: String,
    pub name: String,
    /// `container` or `virtual-machine`.
    pub kind: String,
    /// The instance's `volatile.uuid`, which tells a recreated instance from the one the proposal named.
    pub uuid: Option<String>,
}

impl GpuInstance {
    pub fn is_container(&self) -> bool {
        self.kind == "container"
    }

    pub fn is_vm(&self) -> bool {
        self.kind == "virtual-machine"
    }
}

/// What the instance holds for one PCI address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuDeviceState {
    /// Arca's exact `gpu-<pci>` device.
    Arca,
    /// A GPU device for the card that Arca did not add, or one that differs from its exact body.
    Foreign,
    /// No GPU device names the card.
    Absent,
}

/// Why an action's instance or card was refused. The text is shown to the caller and never carries a value the inventory
/// did not publish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GpuRefusal {
    /// The node is watch-only, so no device writer exists.
    Disabled,
    /// The inventory could not be read.
    Inventory(String),
    /// No card in the inventory has this PCI address.
    NotInInventory(String),
    /// The card is bound to `vfio-pci`, so a container cannot take it as a physical GPU.
    Bound(Vec<String>),
    /// A virtual machine may not take a driver-bound card another consumer uses.
    InUse(String),
    /// The instance is outside the kernel's project family.
    Project(String),
    /// The instance could not be read, or no instance has this name.
    Instance(String),
    /// The instance moved or was recreated after the proposal.
    Identity(String),
    /// The instance holds no Arca GPU device for the card.
    DeviceAbsent(String),
    /// The instance holds a GPU device for the card that Arca did not add.
    ForeignDevice(String),
}

impl std::fmt::Display for GpuRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => f.write_str("GPU assignment is disabled: this node is watch-only"),
            Self::Inventory(why) => write!(f, "the GPU inventory could not be read: {why}"),
            Self::NotInInventory(pci) => write!(f, "the PCI address {pci} is not in the inventory"),
            Self::Bound(pci) => write!(
                f,
                "the PCI address {} is bound to vfio-pci and cannot be attached to a container",
                pci.join(", ")
            ),
            Self::InUse(pci) => write!(
                f,
                "the PCI address {pci} is in use by other consumers; a virtual machine may not take it"
            ),
            Self::Project(_) => f.write_str("the instance is outside the kernel's project family"),
            Self::Instance(why) => write!(f, "the instance could not be read: {why}"),
            Self::Identity(why) => write!(f, "the instance changed after the proposal: {why}"),
            Self::DeviceAbsent(pci) => {
                write!(
                    f,
                    "nothing to remove: the instance holds no device for {pci}"
                )
            }
            Self::ForeignDevice(pci) => write!(f, "this GPU device was not added by Arca: {pci}"),
        }
    }
}

/// The card in the inventory with this PCI address, or why it is not there.
pub fn card_for(cards: &[GpuCard], pci: &str) -> Result<GpuCard, GpuRefusal> {
    let pci = normalize_pci(pci);
    cards
        .iter()
        .find(|card| card.pci == pci)
        .cloned()
        .ok_or(GpuRefusal::NotInInventory(pci))
}

/// The inventory and the instances an action proposal reads, and the device change an approved action runs. A test gives
/// [`FakeGpu`]; the node gives [`IncusGpu`].
pub trait GpuBackend: Send + Sync {
    fn inventory(&self) -> Result<Vec<GpuCard>, GpuRefusal>;
    /// Resolves an instance name across the kernel's family at proposal time.
    fn resolve(&self, name: &str) -> Result<GpuInstance, GpuRefusal>;
    /// Reads one instance in the project the proposal bound.
    fn instance(&self, project: &str, name: &str) -> Result<GpuInstance, GpuRefusal>;
    /// Whether another consumer uses the card. `None` when this node cannot measure it.
    fn in_use(&self, pci: &str) -> Result<Option<bool>, GpuRefusal>;
    fn device_state(&self, instance: &GpuInstance, pci: &str)
    -> Result<GpuDeviceState, GpuRefusal>;
    fn attach(&self, instance: &GpuInstance, pci: &str) -> Result<GpuChange, String>;
    fn detach(&self, instance: &GpuInstance, pci: &str) -> Result<GpuChange, String>;
}

/// The node's backend: the ordinary GET client for reads, and the narrow device assigner for the approved write.
#[derive(Debug, Clone)]
pub struct IncusGpu {
    read: UnixIncus,
    assign: DeviceAssigner,
    family: ProjectFamily,
    local_telemetry: bool,
    nvidia: NvidiaSmi,
}

impl IncusGpu {
    /// A backend for one kernel family. `local_telemetry` is true only when the node's Incus is the local daemon and the
    /// mode is settled: a remote or provisional source cannot say which local processes use a card.
    pub fn new(socket: PathBuf, family: ProjectFamily, local_telemetry: bool) -> IncusGpu {
        IncusGpu {
            read: UnixIncus::new(socket.clone()),
            assign: DeviceAssigner::new(socket, family.clone()),
            family,
            local_telemetry,
            nvidia: NvidiaSmi::new(),
        }
    }

    fn allows(&self, project: &str) -> bool {
        self.family.allows(IncusKind::Instance, project)
    }

    fn instance_of(&self, row: &Value, project: &str) -> Option<GpuInstance> {
        let name = row.get("name")?.as_str()?;
        let kind = row
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("container");
        Some(GpuInstance {
            project: project.to_string(),
            name: name.to_string(),
            kind: kind.to_string(),
            uuid: row
                .pointer("/config/volatile.uuid")
                .and_then(Value::as_str)
                .filter(|uuid| !uuid.is_empty())
                .map(str::to_string),
        })
    }
}

fn plain_name(text: &str) -> bool {
    !text.is_empty() && text.len() <= 128 && text.bytes().all(|byte| byte.is_ascii_graphic())
}

impl GpuBackend for IncusGpu {
    fn inventory(&self) -> Result<Vec<GpuCard>, GpuRefusal> {
        read_inventory(&self.read)
            .map(|inventory| inventory.cards)
            .ok_or_else(|| GpuRefusal::Inventory("the resources read failed".to_string()))
    }

    fn resolve(&self, name: &str) -> Result<GpuInstance, GpuRefusal> {
        let listing = self
            .read
            .get(INSTANCES_PATH)
            .map_err(|error| GpuRefusal::Instance(error.to_string()))?;
        let rows = listing
            .as_array()
            .ok_or_else(|| GpuRefusal::Instance("the listing is not a list".to_string()))?;
        let mut allowed = Vec::new();
        let mut outside = false;
        for row in rows {
            if row.get("name").and_then(Value::as_str) != Some(name) {
                continue;
            }
            let project = row
                .get("project")
                .and_then(Value::as_str)
                .unwrap_or("default");
            if !plain_name(project) || !plain_name(name) {
                continue;
            }
            if !self.allows(project) {
                outside = true;
                continue;
            }
            if let Some(instance) = self.instance_of(row, project) {
                allowed.push(instance);
            }
        }
        match allowed.as_slice() {
            [instance] => Ok(instance.clone()),
            [] if outside => Err(GpuRefusal::Project(
                "the instance exists only outside the kernel's project family".to_string(),
            )),
            [] => Err(GpuRefusal::Instance(format!(
                "no instance named {name} exists in the kernel's project family"
            ))),
            _ => Err(GpuRefusal::Instance(format!(
                "more than one instance in the kernel's project family is named {name}"
            ))),
        }
    }

    fn instance(&self, project: &str, name: &str) -> Result<GpuInstance, GpuRefusal> {
        if !plain_name(project) || !plain_name(name) {
            return Err(GpuRefusal::Instance(
                "the project or instance name is not plain".to_string(),
            ));
        }
        if !self.allows(project) {
            return Err(GpuRefusal::Project(
                "the instance's project is outside the kernel's family".to_string(),
            ));
        }
        let path = format!("/1.0/instances/{name}?project={project}");
        let row = self.read.get(&path).map_err(|error| match error {
            arca_incus::IncusError::Status { code: 404, .. } => GpuRefusal::Instance(format!(
                "no instance named {name} exists in project {project}"
            )),
            other => GpuRefusal::Instance(other.to_string()),
        })?;
        self.instance_of(&row, project)
            .ok_or_else(|| GpuRefusal::Instance("the instance has no readable name".to_string()))
    }

    fn in_use(&self, pci: &str) -> Result<Option<bool>, GpuRefusal> {
        if !self.local_telemetry {
            return Ok(None);
        }
        let pci = normalize_pci(pci);
        match self.nvidia.processes() {
            Ok(processes) => Ok(Some(
                processes
                    .iter()
                    .any(|process| normalize_pci(&process.pci) == pci),
            )),
            Err(NvidiaError::Missing | NvidiaError::TimedOut | NvidiaError::Failed(_)) => Ok(None),
        }
    }

    fn device_state(
        &self,
        instance: &GpuInstance,
        pci: &str,
    ) -> Result<GpuDeviceState, GpuRefusal> {
        let row = self
            .assign
            .read(&instance.project, &instance.name)
            .map_err(|error| GpuRefusal::Instance(error.to_string()))?;
        Ok(device_state_of(&row, pci))
    }

    fn attach(&self, instance: &GpuInstance, pci: &str) -> Result<GpuChange, String> {
        if !self.allows(&instance.project) {
            return Err("the instance's project is outside the kernel's family".to_string());
        }
        let row = self
            .assign
            .read(&instance.project, &instance.name)
            .map_err(|error| error.to_string())?;
        self.meets(instance, &row)?;
        self.assign
            .attach_gpu(&row, pci)
            .map_err(|error| error.to_string())
    }

    fn detach(&self, instance: &GpuInstance, pci: &str) -> Result<GpuChange, String> {
        if !self.allows(&instance.project) {
            return Err("the instance's project is outside the kernel's family".to_string());
        }
        let row = self
            .assign
            .read(&instance.project, &instance.name)
            .map_err(|error| error.to_string())?;
        self.meets(instance, &row)?;
        self.assign
            .detach_gpu(&row, pci)
            .map_err(|error| error.to_string())
    }
}

impl IncusGpu {
    /// Refuses an instance that is no longer the one the proposal named: another kind, or another `volatile.uuid`.
    fn meets(&self, wanted: &GpuInstance, row: &arca_incus::InstanceDevices) -> Result<(), String> {
        if row.kind != wanted.kind {
            return Err("the instance's type changed after the proposal".to_string());
        }
        if wanted.uuid.is_some() && row.uuid != wanted.uuid {
            return Err("the instance was recreated after the proposal".to_string());
        }
        Ok(())
    }
}

fn device_state_of(row: &arca_incus::InstanceDevices, pci: &str) -> GpuDeviceState {
    if row.arca_device(pci).is_some() {
        GpuDeviceState::Arca
    } else if row.gpu_devices_for(pci).next().is_some() {
        GpuDeviceState::Foreign
    } else {
        GpuDeviceState::Absent
    }
}

/// One backend call a test's [`FakeGpu`] recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GpuCall {
    Attach {
        project: String,
        instance: String,
        pci: String,
    },
    Detach {
        project: String,
        instance: String,
        pci: String,
    },
}

/// A backend for tests: a fixed inventory and instance list, an in-memory set of attached cards, and a log of every call.
#[derive(Clone)]
pub struct FakeGpu {
    cards: Arc<Mutex<Vec<GpuCard>>>,
    instances: Arc<Mutex<Vec<GpuInstance>>>,
    family: Arc<Mutex<BTreeSet<String>>>,
    attached: Arc<Mutex<BTreeSet<(String, String, String)>>>,
    foreign: Arc<Mutex<BTreeSet<(String, String, String)>>>,
    in_use: Arc<Mutex<BTreeMap<String, Option<bool>>>>,
    calls: Arc<Mutex<Vec<GpuCall>>>,
}

impl Default for FakeGpu {
    fn default() -> Self {
        FakeGpu {
            cards: Arc::new(Mutex::new(Vec::new())),
            instances: Arc::new(Mutex::new(Vec::new())),
            family: Arc::new(Mutex::new(BTreeSet::new())),
            attached: Arc::new(Mutex::new(BTreeSet::new())),
            foreign: Arc::new(Mutex::new(BTreeSet::new())),
            in_use: Arc::new(Mutex::new(BTreeMap::new())),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl FakeGpu {
    pub fn new(cards: Vec<GpuCard>, instances: Vec<GpuInstance>) -> FakeGpu {
        let family = instances
            .iter()
            .map(|instance| instance.project.clone())
            .collect();
        FakeGpu {
            cards: Arc::new(Mutex::new(cards)),
            instances: Arc::new(Mutex::new(instances)),
            family: Arc::new(Mutex::new(family)),
            ..FakeGpu::default()
        }
    }

    /// The projects this fake's kernel family holds. Empty means the instance list's projects.
    pub fn with_family(self, projects: impl IntoIterator<Item = String>) -> FakeGpu {
        *self.family.lock().unwrap_or_else(PoisonError::into_inner) =
            projects.into_iter().collect();
        self
    }

    pub fn set_cards(&self, cards: Vec<GpuCard>) {
        *self.cards.lock().unwrap_or_else(PoisonError::into_inner) = cards;
    }

    pub fn set_instances(&self, instances: Vec<GpuInstance>) {
        *self
            .instances
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = instances;
    }

    pub fn set_in_use(&self, pci: &str, state: Option<bool>) {
        self.in_use
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(normalize_pci(pci), state);
    }

    pub fn set_foreign_device(&self, instance: &GpuInstance, pci: &str) {
        self.foreign
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert((
                instance.project.clone(),
                instance.name.clone(),
                normalize_pci(pci),
            ));
    }

    pub fn calls(&self) -> Vec<GpuCall> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn attach_calls(&self) -> Vec<GpuCall> {
        self.calls()
            .into_iter()
            .filter(|call| matches!(call, GpuCall::Attach { .. }))
            .collect()
    }

    fn allowed(&self, project: &str) -> bool {
        let family = self.family.lock().unwrap_or_else(PoisonError::into_inner);
        family.is_empty() || family.contains(project)
    }

    fn record(&self, call: GpuCall) {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(call);
    }
}

impl GpuBackend for FakeGpu {
    fn inventory(&self) -> Result<Vec<GpuCard>, GpuRefusal> {
        Ok(self
            .cards
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone())
    }

    fn resolve(&self, name: &str) -> Result<GpuInstance, GpuRefusal> {
        let instances = self
            .instances
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut allowed = Vec::new();
        let mut outside = false;
        for instance in instances.iter().filter(|row| row.name == name) {
            if self.allowed(&instance.project) {
                allowed.push(instance.clone());
            } else {
                outside = true;
            }
        }
        match allowed.as_slice() {
            [instance] => Ok(instance.clone()),
            [] if outside => Err(GpuRefusal::Project(
                "the instance exists only outside the kernel's project family".to_string(),
            )),
            [] => Err(GpuRefusal::Instance(format!(
                "no instance named {name} exists in the kernel's project family"
            ))),
            _ => Err(GpuRefusal::Instance(format!(
                "more than one instance in the kernel's project family is named {name}"
            ))),
        }
    }

    fn instance(&self, project: &str, name: &str) -> Result<GpuInstance, GpuRefusal> {
        if !self.allowed(project) {
            return Err(GpuRefusal::Project(
                "the instance's project is outside the kernel's family".to_string(),
            ));
        }
        let instances = self
            .instances
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        instances
            .iter()
            .find(|row| row.project == project && row.name == name)
            .cloned()
            .ok_or_else(|| {
                GpuRefusal::Instance(format!(
                    "no instance named {name} exists in project {project}"
                ))
            })
    }

    fn in_use(&self, pci: &str) -> Result<Option<bool>, GpuRefusal> {
        let pci = normalize_pci(pci);
        Ok(self
            .in_use
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&pci)
            .copied()
            .unwrap_or(Some(false)))
    }

    fn device_state(
        &self,
        instance: &GpuInstance,
        pci: &str,
    ) -> Result<GpuDeviceState, GpuRefusal> {
        let pci = normalize_pci(pci);
        let key = (instance.project.clone(), instance.name.clone(), pci);
        if self
            .attached
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&key)
        {
            return Ok(GpuDeviceState::Arca);
        }
        if self
            .foreign
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&key)
        {
            return Ok(GpuDeviceState::Foreign);
        }
        Ok(GpuDeviceState::Absent)
    }

    fn attach(&self, instance: &GpuInstance, pci: &str) -> Result<GpuChange, String> {
        self.record(GpuCall::Attach {
            project: instance.project.clone(),
            instance: instance.name.clone(),
            pci: normalize_pci(pci),
        });
        let pci = normalize_pci(pci);
        let device = arca_incus::assign::device_name(&pci);
        let key = (instance.project.clone(), instance.name.clone(), pci);
        let changed = self
            .attached
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key);
        Ok(GpuChange { device, changed })
    }

    fn detach(&self, instance: &GpuInstance, pci: &str) -> Result<GpuChange, String> {
        self.record(GpuCall::Detach {
            project: instance.project.clone(),
            instance: instance.name.clone(),
            pci: normalize_pci(pci),
        });
        let pci = normalize_pci(pci);
        let device = arca_incus::assign::device_name(&pci);
        let key = (instance.project.clone(), instance.name.clone(), pci);
        let changed = self
            .attached
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&key);
        Ok(GpuChange { device, changed })
    }
}
