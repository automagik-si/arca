//! GPU inventory, telemetry and per-process attribution, all keyed by the card's PCI address.
//!
//! The inventory is read from Incus's `GET /1.0/resources`, `gpu.cards[]`, through the ordinary read client, so it describes
//! the machine the node's Incus serves whether that is this machine or a remote one. Telemetry and attribution are local
//! readings: they run `nvidia-smi` and read `/proc/<pid>/cgroup`, so they are only taken when the node's Incus is the local
//! daemon, exactly as the CPU readings are.
//!
//! The PCI address is the identity. A vendor UUID is carried when the card reports one, but it is never the key: two cards
//! of the same model have the same product string and must still be told apart, and a card that is bound to `vfio-pci` may
//! report no UUID at all.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex, PoisonError};

use arca_incus::IncusRead;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The path the inventory is read from.
pub const RESOURCES_PATH: &str = "/1.0/resources";

/// Why the inventory is absent: the node's Incus did not answer `GET /1.0/resources`, or answered a document that does not
/// carry the `gpu.cards` list.
pub const INVENTORY_REASON: &str = "the Incus resources read failed";

/// Why a card has no telemetry yet: the card is in the inventory, but `nvidia-smi` did not report it.
pub const NO_READING_YET: &str = "no reading yet";

/// Why a card has no telemetry or attribution here: the node's Incus is remote, so a local `nvidia-smi` would describe the
/// wrong machine.
pub const NOT_MEASURABLE: &str = "not measurable from here";

/// Why a card has no telemetry: `nvidia-smi` is not installed on this host.
pub const NVIDIA_MISSING: &str = "nvidia-smi is not installed";

/// Why a card has no telemetry: `nvidia-smi` ran, but the read failed.
pub const NVIDIA_FAILED: &str = "the nvidia-smi read failed";

/// How a GPU is currently used. It is derived from the card's kernel driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GpuUse {
    /// A kernel driver is bound to the card (the NVIDIA driver, or another display driver).
    DriverBound,
    /// The card is bound to `vfio-pci`, so it is reserved for passthrough and cannot be attached to a container as a
    /// physical GPU.
    VfioPci,
    /// No kernel driver is bound to the card.
    NoDriver,
}

impl GpuUse {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DriverBound => "driver-bound",
            Self::VfioPci => "vfio-pci",
            Self::NoDriver => "no-driver",
        }
    }
}

/// One card of the inventory, keyed by its PCI address. A field Incus does not report is `None`, never a guessed value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuCard {
    /// The PCI address, normalized to four domain hex digits, lowercase. This is the card's identity.
    pub pci: String,
    /// The NVIDIA UUID, when the card reports one. It is added identity, never the key.
    pub uuid: Option<String>,
    /// The vendor name, from the PCI database.
    pub vendor: String,
    /// The product name, from the PCI database.
    pub product: String,
    /// The kernel driver bound to the card, when one is.
    pub driver: Option<String>,
    /// The kernel driver's version, when Incus reports one.
    pub driver_version: Option<String>,
    /// The NUMA node the card belongs to.
    pub numa_node: u64,
    /// The card's total VRAM, in bytes. Incus's own resources document does not carry it, so it is filled from the local
    /// telemetry's `memory.total` when that is measured, and stays `None` on a remote host.
    pub vram_total_bytes: Option<u64>,
    /// How the card is used, derived from `driver`.
    #[serde(rename = "use")]
    pub usage: GpuUse,
}

impl GpuCard {
    /// A card with the given identity, for a test or a fake.
    pub fn new(pci: impl Into<String>, product: impl Into<String>) -> GpuCard {
        GpuCard {
            pci: normalize_pci(&pci.into()),
            uuid: None,
            vendor: "NVIDIA Corporation".to_string(),
            product: product.into(),
            driver: Some("nvidia".to_string()),
            driver_version: None,
            numa_node: 0,
            vram_total_bytes: None,
            usage: GpuUse::DriverBound,
        }
    }
}

/// Why a telemetry or attribution read failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NvidiaError {
    /// The `nvidia-smi` program is not installed.
    Missing,
    /// The program ran, but it failed or its output could not be read. The text is a short reason, never a raw value.
    Failed(String),
}

impl fmt::Display for NvidiaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => f.write_str(NVIDIA_MISSING),
            Self::Failed(why) => write!(f, "{NVIDIA_FAILED}: {why}"),
        }
    }
}

/// One card's numbers from `nvidia-smi`. Every value is `None` when the tool reported no usable number, which is different
/// from a real zero.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuTelemetry {
    /// `utilization.gpu`, in percent.
    pub utilization_pct: Option<f64>,
    /// `memory.used`, in bytes.
    pub memory_used_bytes: Option<u64>,
    /// `memory.total`, in bytes.
    pub memory_total_bytes: Option<u64>,
    /// `power.draw`, in watts.
    pub power_draw_w: Option<f64>,
    /// `power.limit`, in watts.
    pub power_limit_w: Option<f64>,
    /// `temperature.gpu`, in degrees Celsius.
    pub temperature_c: Option<f64>,
    /// `clocks.sm`, in MHz.
    pub sm_clock_mhz: Option<u64>,
    /// `clocks_event_reasons.active`, as the tool spells it.
    pub event_reasons: Option<String>,
}

/// One card as `nvidia-smi` reported it: its PCI address, its optional UUID, and its numbers.
#[derive(Debug, Clone, PartialEq)]
pub struct NvidiaReading {
    pub pci: String,
    pub uuid: Option<String>,
    pub telemetry: GpuTelemetry,
}

/// One compute process as `nvidia-smi` reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NvidiaProcess {
    pub pci: String,
    pub pid: u32,
    pub vram_bytes: Option<u64>,
}

/// Where one GPU compute process runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum GpuOwner {
    /// An Incus instance. The name is the instance name, and the project is the instance's project.
    Instance { project: String, name: String },
    /// A Docker container, named by the id in its `docker-<id>.scope` cgroup.
    Container { name: String },
    /// A process on the host, or one whose cgroup does not name an instance or a container.
    Host,
}

/// One GPU compute process, attributed to its instance, container or host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuProcess {
    /// The PCI address of the card the process uses.
    pub pci: String,
    pub pid: u32,
    /// The memory the process has on the card, in bytes.
    pub vram_bytes: Option<u64>,
    pub owner: GpuOwner,
}

/// One card's inventory and latest telemetry, as a host sample carries it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuCardSample {
    pub pci: String,
    pub uuid: Option<String>,
    pub vendor: String,
    pub product: String,
    pub driver: Option<String>,
    pub driver_version: Option<String>,
    pub numa_node: u64,
    pub vram_total_bytes: Option<u64>,
    #[serde(rename = "use")]
    pub usage: GpuUse,
    /// The card's newest telemetry, or `None` with a reason.
    pub telemetry: Option<GpuTelemetry>,
    /// Why `telemetry` is absent, when it is.
    pub reason: Option<String>,
    /// The compute processes on the card, attributed. Empty when no process uses it.
    pub processes: Vec<GpuProcess>,
    /// Why the process list is absent, when it is.
    pub process_reason: Option<String>,
}

impl From<GpuCard> for GpuCardSample {
    fn from(card: GpuCard) -> GpuCardSample {
        GpuCardSample {
            pci: card.pci,
            uuid: card.uuid,
            vendor: card.vendor,
            product: card.product,
            driver: card.driver,
            driver_version: card.driver_version,
            numa_node: card.numa_node,
            vram_total_bytes: card.vram_total_bytes,
            usage: card.usage,
            telemetry: None,
            reason: Some(NO_READING_YET.to_string()),
            processes: Vec::new(),
            process_reason: Some(NO_READING_YET.to_string()),
        }
    }
}

/// The GPU part of one host sample: the inventory and telemetry of every card, keyed by PCI address.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuSection {
    /// Why the inventory is absent. `None` when the cards list was read, even when it is empty.
    pub reason: Option<String>,
    pub cards: Vec<GpuCardSample>,
}

impl GpuSection {
    pub fn empty() -> GpuSection {
        GpuSection::default()
    }

    /// The card with this PCI address, when the section holds it.
    pub fn card(&self, pci: &str) -> Option<&GpuCardSample> {
        let pci = normalize_pci(pci);
        self.cards.iter().find(|card| card.pci == pci)
    }
}

/// Reads the GPU cards of a `/1.0/resources` document. `None` when the document does not carry a usable `gpu.cards` list:
/// an absent or malformed list is unknown, never a shorter list that could read as known-empty. A host with no GPU answers
/// `gpu.total` 0 and `gpu.cards` `[]`, which is a known-empty list.
pub fn cards_of(document: &Value) -> Option<Vec<GpuCard>> {
    let cards = document.pointer("/gpu/cards")?.as_array()?;
    cards.iter().map(card_of).collect()
}

fn card_of(card: &Value) -> Option<GpuCard> {
    let pci = card
        .get("pci_address")
        .or_else(|| card.get("address"))
        .and_then(Value::as_str)
        .filter(|pci| !pci.is_empty())?;
    let nvidia = card.get("nvidia");
    let driver = card
        .get("driver")
        .and_then(Value::as_str)
        .filter(|driver| !driver.is_empty());
    let vendor = card
        .get("vendor")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let product = card
        .get("product")
        .and_then(Value::as_str)
        .filter(|product| !product.is_empty())
        .or_else(|| {
            nvidia
                .and_then(|nvidia| nvidia.get("model"))
                .and_then(Value::as_str)
        })
        .unwrap_or_default();
    Some(GpuCard {
        pci: normalize_pci(pci),
        uuid: nvidia
            .and_then(|nvidia| nvidia.get("uuid"))
            .and_then(Value::as_str)
            .filter(|uuid| !uuid.is_empty())
            .map(str::to_string),
        vendor: vendor.to_string(),
        product: product.to_string(),
        driver: driver.map(str::to_string),
        driver_version: card
            .get("driver_version")
            .and_then(Value::as_str)
            .filter(|version| !version.is_empty())
            .map(str::to_string),
        numa_node: card.get("numa_node").and_then(Value::as_u64).unwrap_or(0),
        vram_total_bytes: nvidia.and_then(vram_total),
        usage: match driver {
            Some("vfio-pci") => GpuUse::VfioPci,
            Some(_) => GpuUse::DriverBound,
            None => GpuUse::NoDriver,
        },
    })
}

/// The VRAM total an NVIDIA block carries, when the Incus version (or a patched daemon) reports one.
fn vram_total(nvidia: &Value) -> Option<u64> {
    nvidia
        .pointer("/memory/total")
        .or_else(|| nvidia.get("memory_total"))
        .or_else(|| nvidia.pointer("/memory/total_bytes"))
        .and_then(Value::as_u64)
        .filter(|bytes| *bytes > 0)
}

/// Reads the inventory through `incus`. A read that fails, or a document without a usable `gpu.cards` list, is `None`.
/// A 404 is treated as a known-empty list: an Incus without the resources endpoint reports no cards, and the sampler does
/// not invent an inventory failure for it.
pub fn read_inventory<R: IncusRead>(incus: &R) -> Option<Vec<GpuCard>> {
    match incus.get(RESOURCES_PATH) {
        Ok(document) => match cards_of(&document) {
            Some(cards) => Some(cards),
            // A resources document with no `gpu` object at all is a host without the extension; a present but malformed
            // list is unknown.
            None if document.get("gpu").is_none() => Some(Vec::new()),
            None => None,
        },
        Err(arca_incus::IncusError::Status { code: 404, .. }) => Some(Vec::new()),
        Err(_) => None,
    }
}

/// Normalizes a PCI address to four lowercase domain hex digits, so `00000000:01:00.0` from `nvidia-smi` and
/// `0000:01:00.0` from Incus name the same card. A value that is not `domain:bus:slot.function` is returned lowercased, so
/// it is still compared exactly rather than being guessed into another form.
pub fn normalize_pci(text: &str) -> String {
    let text = text.trim().to_ascii_lowercase();
    let Some((domain, rest)) = text.split_once(':') else {
        return text;
    };
    if domain.is_empty() || !domain.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return text;
    }
    let trimmed = domain.trim_start_matches('0');
    let domain = if trimmed.is_empty() { "0" } else { trimmed };
    format!("{domain:0>4}:{rest}")
}

/// Reads the cards from `nvidia-smi`. It is a trait so a test can give a fake and a node without the tool is a plain
/// [`NvidiaError::Missing`], never a zero reading.
pub trait Nvidia: Send + Sync {
    fn readings(&self) -> Result<Vec<NvidiaReading>, NvidiaError>;
    fn processes(&self) -> Result<Vec<NvidiaProcess>, NvidiaError>;
}

/// The tool on this host. The program is named `nvidia-smi`; a test may point [`NvidiaSmi::at`] elsewhere.
#[derive(Debug, Clone)]
pub struct NvidiaSmi {
    program: std::path::PathBuf,
}

impl Default for NvidiaSmi {
    fn default() -> Self {
        Self::new()
    }
}

impl NvidiaSmi {
    pub fn new() -> NvidiaSmi {
        NvidiaSmi {
            program: std::path::PathBuf::from("nvidia-smi"),
        }
    }

    /// The same reader over another program path.
    pub fn at(program: impl Into<std::path::PathBuf>) -> NvidiaSmi {
        NvidiaSmi {
            program: program.into(),
        }
    }

    fn run(&self, args: &[&str]) -> Result<String, NvidiaError> {
        match Command::new(&self.program).args(args).output() {
            Ok(output) if output.status.success() => {
                Ok(String::from_utf8_lossy(&output.stdout).into_owned())
            }
            Ok(_) => Err(NvidiaError::Failed(
                "the command answered an error".to_string(),
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(NvidiaError::Missing),
            Err(error) => Err(NvidiaError::Failed(error.kind().to_string())),
        }
    }
}

/// The columns the GPU telemetry query asks for.
const GPU_QUERY: &str = "pci.bus_id,uuid,utilization.gpu,memory.used,memory.total,power.draw,power.limit,temperature.gpu,clocks.sm,clocks_event_reasons.active";
/// The columns the compute-apps query asks for.
const APPS_QUERY: &str = "gpu_bus_id,pid,used_memory";

impl Nvidia for NvidiaSmi {
    fn readings(&self) -> Result<Vec<NvidiaReading>, NvidiaError> {
        let text = self.run(&[
            &format!("--query-gpu={GPU_QUERY}"),
            "--format=csv,noheader,nounits",
        ])?;
        parse_readings(&text)
            .ok_or_else(|| NvidiaError::Failed("the answer is not the query's CSV".to_string()))
    }

    fn processes(&self) -> Result<Vec<NvidiaProcess>, NvidiaError> {
        let text = self.run(&[
            &format!("--query-compute-apps={APPS_QUERY}"),
            "--format=csv,noheader,nounits",
        ])?;
        parse_processes(&text)
            .ok_or_else(|| NvidiaError::Failed("the answer is not the query's CSV".to_string()))
    }
}

/// Parses the ten-column GPU query. `None` when a line does not hold ten columns; an empty answer is an empty list.
pub fn parse_readings(text: &str) -> Option<Vec<NvidiaReading>> {
    let mut readings = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let fields: Vec<&str> = line.split(',').map(str::trim).collect();
        if fields.len() != 10 {
            return None;
        }
        let pci = normalize_pci(fields[0]);
        if pci.is_empty() {
            return None;
        }
        readings.push(NvidiaReading {
            pci,
            uuid: text_value(fields[1]),
            telemetry: GpuTelemetry {
                utilization_pct: number(fields[2]),
                memory_used_bytes: mib(fields[3]),
                memory_total_bytes: mib(fields[4]),
                power_draw_w: number(fields[5]),
                power_limit_w: number(fields[6]),
                temperature_c: number(fields[7]),
                sm_clock_mhz: whole(fields[8]),
                event_reasons: text_value(fields[9]),
            },
        });
    }
    Some(readings)
}

/// Parses the three-column compute-apps query. `None` when a line does not hold three columns; an empty answer is an empty
/// list.
pub fn parse_processes(text: &str) -> Option<Vec<NvidiaProcess>> {
    let mut processes = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let fields: Vec<&str> = line.split(',').map(str::trim).collect();
        if fields.len() != 3 {
            return None;
        }
        let pci = normalize_pci(fields[0]);
        let pid = fields[1].parse::<u32>().ok()?;
        if pci.is_empty() {
            return None;
        }
        processes.push(NvidiaProcess {
            pci,
            pid,
            vram_bytes: mib(fields[2]),
        });
    }
    Some(processes)
}

/// A value the tool marks as unavailable. `[N/A]`, `[Not Supported]`, `[Unknown Error]` and an empty field are absent.
fn unavailable(text: &str) -> bool {
    let text = text.trim();
    text.is_empty()
        || text.starts_with("[N/A")
        || text.starts_with("[Not Supported")
        || text.starts_with("[Unknown")
}

fn text_value(text: &str) -> Option<String> {
    (!unavailable(text)).then(|| text.trim().to_string())
}

fn number(text: &str) -> Option<f64> {
    if unavailable(text) {
        return None;
    }
    text.parse::<f64>().ok().filter(|value| value.is_finite())
}

fn whole(text: &str) -> Option<u64> {
    if unavailable(text) {
        return None;
    }
    text.parse::<u64>().ok()
}

/// A MiB figure from the tool, in bytes. A real zero is kept; an unavailable field is `None`.
fn mib(text: &str) -> Option<u64> {
    whole(text).map(|mib| mib.saturating_mul(1024 * 1024))
}

/// Reads `/proc/<pid>/cgroup` under `proc_root`: the path of the process's cgroup, from its last `:` field.
pub fn read_cgroup(proc_root: &Path, pid: u32) -> Option<String> {
    let text = std::fs::read_to_string(proc_root.join(pid.to_string()).join("cgroup")).ok()?;
    text.lines()
        .filter_map(|line| line.splitn(3, ':').nth(2))
        .map(str::trim)
        .find(|path| !path.is_empty())
        .map(str::to_string)
}

/// The instance name from an `lxc.payload.` path, project-aware. Incus names a default-project container's payload
/// `lxc.payload.<instance>` and another project's `lxc.payload.<project>_<instance>`. The known instance list is consulted
/// first, so an instance whose own name holds an `_` is matched whole; without that list the first `_` separates the
/// project from the name, and a name without one is in the default project.
pub fn incus_instance(path: &str, instances: &[InstanceKey]) -> Option<(String, String)> {
    let suffix = path
        .split('/')
        .find_map(|segment| segment.strip_prefix("lxc.payload."))?;
    if suffix.is_empty() {
        return None;
    }
    for instance in instances {
        if instance.project == "default" && instance.name == suffix {
            return Some((instance.project.clone(), instance.name.clone()));
        }
        if format!("{}_{}", instance.project, instance.name) == suffix {
            return Some((instance.project.clone(), instance.name.clone()));
        }
    }
    match suffix.split_once('_') {
        Some((project, name)) if !project.is_empty() && !name.is_empty() => {
            Some((project.to_string(), name.to_string()))
        }
        _ => Some(("default".to_string(), suffix.to_string())),
    }
}

/// The Docker container id from a `docker-<id>.scope` cgroup segment.
pub fn docker_container(path: &str) -> Option<String> {
    path.split('/').find_map(|segment| {
        segment
            .strip_prefix("docker-")
            .and_then(|rest| rest.strip_suffix(".scope"))
            .filter(|id| !id.is_empty())
            .map(str::to_string)
    })
}

/// One instance the attribution knows: its project and name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceKey {
    pub project: String,
    pub name: String,
}

/// Attributes each process from its cgroup. A process whose cgroup names an Incus instance or a Docker container is
/// reported there; every other process, including one whose cgroup cannot be read, is on the host. The PID is the host's
/// PID, so a process in another PID namespace resolves the same way.
pub fn attribute(
    proc_root: &Path,
    processes: &[NvidiaProcess],
    instances: &[InstanceKey],
) -> Vec<GpuProcess> {
    processes
        .iter()
        .map(|process| {
            let owner = read_cgroup(proc_root, process.pid)
                .map(|path| owner_of(&path, instances))
                .unwrap_or(GpuOwner::Host);
            GpuProcess {
                pci: normalize_pci(&process.pci),
                pid: process.pid,
                vram_bytes: process.vram_bytes,
                owner,
            }
        })
        .collect()
}

fn owner_of(path: &str, instances: &[InstanceKey]) -> GpuOwner {
    if let Some(container) = docker_container(path) {
        return GpuOwner::Container { name: container };
    }
    match incus_instance(path, instances) {
        Some((project, name)) => GpuOwner::Instance { project, name },
        None => GpuOwner::Host,
    }
}

/// A fake `nvidia-smi` for tests: fixed readings and processes, or a `Missing`/`Failed` answer. It records which queries
/// it was asked for.
#[derive(Clone)]
pub struct FakeNvidia {
    readings: Arc<Mutex<Result<Vec<NvidiaReading>, NvidiaError>>>,
    processes: Arc<Mutex<Result<Vec<NvidiaProcess>, NvidiaError>>>,
    calls: Arc<Mutex<Vec<&'static str>>>,
}

impl Default for FakeNvidia {
    fn default() -> Self {
        Self {
            readings: Arc::new(Mutex::new(Ok(Vec::new()))),
            processes: Arc::new(Mutex::new(Ok(Vec::new()))),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl FakeNvidia {
    /// A tool that reports no card and no process.
    pub fn empty() -> FakeNvidia {
        FakeNvidia::default()
    }

    /// A tool that is not installed.
    pub fn missing() -> FakeNvidia {
        let fake = FakeNvidia::default();
        fake.fail_readings(NvidiaError::Missing);
        fake.fail_processes(NvidiaError::Missing);
        fake
    }

    /// A tool that runs, but whose reads fail.
    pub fn failing() -> FakeNvidia {
        let fake = FakeNvidia::default();
        fake.fail_readings(NvidiaError::Failed("the query failed".to_string()));
        fake.fail_processes(NvidiaError::Failed("the query failed".to_string()));
        fake
    }

    pub fn with_readings(readings: Vec<NvidiaReading>) -> FakeNvidia {
        let fake = FakeNvidia::default();
        fake.set_readings(readings);
        fake
    }

    pub fn set_readings(&self, readings: Vec<NvidiaReading>) {
        *self.readings.lock().unwrap_or_else(PoisonError::into_inner) = Ok(readings);
    }

    pub fn fail_readings(&self, error: NvidiaError) {
        *self.readings.lock().unwrap_or_else(PoisonError::into_inner) = Err(error);
    }

    pub fn set_processes(&self, processes: Vec<NvidiaProcess>) {
        *self
            .processes
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Ok(processes);
    }

    pub fn fail_processes(&self, error: NvidiaError) {
        *self
            .processes
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Err(error);
    }

    /// The queries this fake was asked for, in order: `readings` and `processes`.
    pub fn calls(&self) -> Vec<&'static str> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Nvidia for FakeNvidia {
    fn readings(&self) -> Result<Vec<NvidiaReading>, NvidiaError> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push("readings");
        self.readings
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn processes(&self) -> Result<Vec<NvidiaProcess>, NvidiaError> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push("processes");
        self.processes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// A card reading with every measurement set, for a test.
pub fn reading(
    pci: &str,
    uuid: Option<&str>,
    utilization: f64,
    used: u64,
    total: u64,
) -> NvidiaReading {
    NvidiaReading {
        pci: normalize_pci(pci),
        uuid: uuid.map(str::to_string),
        telemetry: GpuTelemetry {
            utilization_pct: Some(utilization),
            memory_used_bytes: Some(used),
            memory_total_bytes: Some(total),
            power_draw_w: Some(42.5),
            power_limit_w: Some(115.0),
            temperature_c: Some(51.0),
            sm_clock_mhz: Some(1800),
            event_reasons: Some("0x0000000000000000".to_string()),
        },
    }
}

/// The mean of the GPU sections over a window. A card is keyed by PCI, so two cards of the same product never merge. The
/// inventory fields come from the newest sample that holds the card, each telemetry number is averaged over the samples
/// that hold it, and the process list is the newest sample's: a process count is not a rate.
pub fn mean_gpu(samples: &[&GpuSection], last: &GpuSection) -> GpuSection {
    if last.reason.is_some() {
        return last.clone();
    }
    let mut cards = Vec::new();
    for card in &last.cards {
        let held: Vec<&GpuCardSample> = samples
            .iter()
            .flat_map(|section| section.cards.iter())
            .filter(|other| other.pci == card.pci)
            .collect();
        let telemetry = mean_telemetry(&held);
        let reason = match telemetry {
            Some(_) => None,
            None => held.iter().rev().find_map(|card| card.reason.clone()),
        };
        cards.push(GpuCardSample {
            pci: card.pci.clone(),
            uuid: card.uuid.clone(),
            vendor: card.vendor.clone(),
            product: card.product.clone(),
            driver: card.driver.clone(),
            driver_version: card.driver_version.clone(),
            numa_node: card.numa_node,
            vram_total_bytes: card.vram_total_bytes,
            usage: card.usage,
            telemetry,
            reason,
            processes: card.processes.clone(),
            process_reason: card.process_reason.clone(),
        });
    }
    GpuSection {
        reason: None,
        cards,
    }
}

fn mean_telemetry(cards: &[&GpuCardSample]) -> Option<GpuTelemetry> {
    let held: Vec<&GpuTelemetry> = cards
        .iter()
        .filter_map(|card| card.telemetry.as_ref())
        .collect();
    if held.is_empty() {
        return None;
    }
    Some(GpuTelemetry {
        utilization_pct: average(held.iter().filter_map(|t| t.utilization_pct)),
        memory_used_bytes: average_whole(held.iter().filter_map(|t| t.memory_used_bytes)),
        memory_total_bytes: average_whole(held.iter().filter_map(|t| t.memory_total_bytes)),
        power_draw_w: average(held.iter().filter_map(|t| t.power_draw_w)),
        power_limit_w: average(held.iter().filter_map(|t| t.power_limit_w)),
        temperature_c: average(held.iter().filter_map(|t| t.temperature_c)),
        sm_clock_mhz: average_whole(held.iter().filter_map(|t| t.sm_clock_mhz)),
        event_reasons: held.iter().rev().find_map(|t| t.event_reasons.clone()),
    })
}

fn average(values: impl Iterator<Item = f64>) -> Option<f64> {
    let (sum, count) = values.fold((0.0, 0usize), |(sum, count), value| {
        (sum + value, count + 1)
    });
    (count > 0).then(|| sum / count as f64)
}

fn average_whole(values: impl Iterator<Item = u64>) -> Option<u64> {
    average(values.map(|value| value as f64)).map(|value| value.round() as u64)
}

/// The inventory cards by PCI, when the section holds a successful inventory.
pub fn inventory_of(section: &GpuSection) -> Option<BTreeMap<String, GpuCardSample>> {
    section.reason.is_none().then(|| {
        section
            .cards
            .iter()
            .map(|card| (card.pci.clone(), card.clone()))
            .collect()
    })
}
