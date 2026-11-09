//! The GPU assignment backend: the inventory and instances an action proposal checks, and the owner-approved device
//! changes an approved action runs.
//!
//! Everything is keyed by the PCI address. Two cards of the same product are two rows with two addresses, and an action
//! names exactly one of them. The inventory comes from Incus's `GET /1.0/resources`; the instances from the ordinary
//! `instances` listing; the device change from the narrow [`arca_incus::DeviceAssigner`].

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use arca_incus::{DeviceAssigner, GpuChange, IncusRead, UnixIncus};
use arca_metrics::gpu::{GpuCard, read_inventory};
use serde_json::Value;

pub use arca_incus::GpuChange as GpuChanged;

/// The instance list, unrecursed: each row carries its project, name and type.
pub const INSTANCES_PATH: &str = "/1.0/instances?all-projects=true&recursion=1";

/// One Incus instance an action may name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuInstance {
    pub project: String,
    pub name: String,
    /// `container` or `virtual-machine`.
    pub kind: String,
}

impl GpuInstance {
    pub fn is_container(&self) -> bool {
        self.kind == "container"
    }
}

/// Why an action's instance or card was refused. The text is shown to the caller and never carries a value the inventory
/// did not publish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GpuRefusal {
    /// The inventory could not be read.
    Inventory(String),
    /// No card in the inventory has this PCI address.
    NotInInventory(String),
    /// The card is bound to `vfio-pci`, so a container cannot take it as a physical GPU.
    Bound(Vec<String>),
    /// The instance could not be read, or no instance has this name.
    Instance(String),
}

impl std::fmt::Display for GpuRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Inventory(why) => write!(f, "the GPU inventory could not be read: {why}"),
            Self::NotInInventory(pci) => write!(f, "the PCI address {pci} is not in the inventory"),
            Self::Bound(pci) => write!(
                f,
                "the PCI address {} is bound to vfio-pci and cannot be attached to a container",
                pci.join(", ")
            ),
            Self::Instance(why) => write!(f, "the instance could not be read: {why}"),
        }
    }
}

/// The card in the inventory with this PCI address, or why it is not there.
pub fn card_for(cards: &[GpuCard], pci: &str) -> Result<GpuCard, GpuRefusal> {
    cards
        .iter()
        .find(|card| card.pci == pci)
        .cloned()
        .ok_or_else(|| GpuRefusal::NotInInventory(pci.to_string()))
}

/// The inventory and the instances an action proposal reads, and the device change an approved action runs. A test gives
/// [`FakeGpu`]; the node gives [`IncusGpu`].
pub trait GpuBackend: Send + Sync {
    fn inventory(&self) -> Result<Vec<GpuCard>, GpuRefusal>;
    fn instance(&self, name: &str) -> Result<GpuInstance, GpuRefusal>;
    fn attach(&self, instance: &GpuInstance, pci: &str) -> Result<GpuChange, String>;
    fn detach(&self, instance: &GpuInstance, pci: &str) -> Result<GpuChange, String>;
}

/// The node's backend: the ordinary GET client for reads, and the narrow device assigner for the approved write.
#[derive(Debug, Clone)]
pub struct IncusGpu {
    read: UnixIncus,
    assign: DeviceAssigner,
}

impl IncusGpu {
    pub fn new(socket: PathBuf) -> IncusGpu {
        IncusGpu {
            read: UnixIncus::new(socket.clone()),
            assign: DeviceAssigner::new(socket),
        }
    }
}

impl GpuBackend for IncusGpu {
    fn inventory(&self) -> Result<Vec<GpuCard>, GpuRefusal> {
        read_inventory(&self.read)
            .ok_or_else(|| GpuRefusal::Inventory("the resources read failed".to_string()))
    }

    fn instance(&self, name: &str) -> Result<GpuInstance, GpuRefusal> {
        let listing = self
            .read
            .get(INSTANCES_PATH)
            .map_err(|error| GpuRefusal::Instance(error.to_string()))?;
        let rows = listing
            .as_array()
            .ok_or_else(|| GpuRefusal::Instance("the listing is not a list".to_string()))?;
        let matches: Vec<GpuInstance> = rows
            .iter()
            .filter(|row| row.get("name").and_then(Value::as_str) == Some(name))
            .filter_map(|row| {
                Some(GpuInstance {
                    project: row.get("project")?.as_str()?.to_string(),
                    name: name.to_string(),
                    kind: row
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("container")
                        .to_string(),
                })
            })
            .collect();
        match matches.as_slice() {
            [instance] => Ok(instance.clone()),
            [] => Err(GpuRefusal::Instance(format!(
                "no instance named {name} exists"
            ))),
            _ => Err(GpuRefusal::Instance(format!(
                "more than one instance is named {name}; the action names an instance name only"
            ))),
        }
    }

    fn attach(&self, instance: &GpuInstance, pci: &str) -> Result<GpuChange, String> {
        let row = self
            .assign
            .read(&instance.project, &instance.name)
            .map_err(|error| error.to_string())?;
        if row.kind != instance.kind {
            return Err("the instance's type changed after the proposal".to_string());
        }
        self.assign
            .attach_gpu(&row, pci)
            .map_err(|error| error.to_string())
    }

    fn detach(&self, instance: &GpuInstance, pci: &str) -> Result<GpuChange, String> {
        let row = self
            .assign
            .read(&instance.project, &instance.name)
            .map_err(|error| error.to_string())?;
        if row.kind != instance.kind {
            return Err("the instance's type changed after the proposal".to_string());
        }
        self.assign
            .detach_gpu(&row, pci)
            .map_err(|error| error.to_string())
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
#[derive(Clone, Default)]
pub struct FakeGpu {
    cards: Arc<Mutex<Vec<GpuCard>>>,
    instances: Arc<Mutex<Vec<GpuInstance>>>,
    attached: Arc<Mutex<BTreeSet<(String, String, String)>>>,
    calls: Arc<Mutex<Vec<GpuCall>>>,
}

impl FakeGpu {
    pub fn new(cards: Vec<GpuCard>, instances: Vec<GpuInstance>) -> FakeGpu {
        FakeGpu {
            cards: Arc::new(Mutex::new(cards)),
            instances: Arc::new(Mutex::new(instances)),
            attached: Arc::new(Mutex::new(BTreeSet::new())),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
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

    fn instance(&self, name: &str) -> Result<GpuInstance, GpuRefusal> {
        let instances = self
            .instances
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let found: Vec<&GpuInstance> = instances.iter().filter(|row| row.name == name).collect();
        match found.as_slice() {
            [instance] => Ok((*instance).clone()),
            [] => Err(GpuRefusal::Instance(format!(
                "no instance named {name} exists"
            ))),
            _ => Err(GpuRefusal::Instance(format!(
                "more than one instance is named {name}"
            ))),
        }
    }

    fn attach(&self, instance: &GpuInstance, pci: &str) -> Result<GpuChange, String> {
        self.record(GpuCall::Attach {
            project: instance.project.clone(),
            instance: instance.name.clone(),
            pci: pci.to_string(),
        });
        let key = (
            instance.project.clone(),
            instance.name.clone(),
            pci.to_string(),
        );
        let changed = self
            .attached
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key);
        Ok(GpuChange {
            device: format!("gpu-{}", pci.replace([':', '.'], "-")),
            changed,
        })
    }

    fn detach(&self, instance: &GpuInstance, pci: &str) -> Result<GpuChange, String> {
        self.record(GpuCall::Detach {
            project: instance.project.clone(),
            instance: instance.name.clone(),
            pci: pci.to_string(),
        });
        let key = (
            instance.project.clone(),
            instance.name.clone(),
            pci.to_string(),
        );
        let changed = self
            .attached
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&key);
        Ok(GpuChange {
            device: format!("gpu-{}", pci.replace([':', '.'], "-")),
            changed,
        })
    }
}
