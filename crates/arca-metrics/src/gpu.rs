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

/// Why a card has no telemetry: `nvidia-smi` did not answer by its deadline and was killed.
pub const NVIDIA_TIMEOUT_REASON: &str = "the nvidia-smi read timed out";

/// Why a card has no telemetry: a previous `nvidia-smi` read has not finished, even after it was killed.
pub const NVIDIA_BUSY_REASON: &str = "the previous nvidia-smi read has not finished";

/// How long one `nvidia-smi` query may run before it is killed.
pub const NVIDIA_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The most bytes of `nvidia-smi` output the sampler reads.
pub const NVIDIA_MAX_OUTPUT: usize = 1024 * 1024;

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
    /// The program did not answer by its deadline and was killed.
    TimedOut,
    /// A previous read is still stuck after being killed, so no new one is started.
    Busy,
    /// The program ran, but it failed or its output could not be read. The text is a short reason, never a raw value.
    Failed(String),
}

impl fmt::Display for NvidiaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => f.write_str(NVIDIA_MISSING),
            Self::TimedOut => f.write_str(NVIDIA_TIMEOUT_REASON),
            Self::Busy => f.write_str(NVIDIA_BUSY_REASON),
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
    /// An Incus instance. The name is the instance name, and the project is the instance's project. A Docker container
    /// inside the instance is extra detail, never a second owner.
    Instance {
        project: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        container: Option<String>,
    },
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
    /// Why some cards were left out of a read inventory, such as one with no PCI address. The rest still list.
    pub skipped: Option<String>,
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

/// The cards of one successful inventory, and why a card may have been left out. A skipped card is not a failed read: the
/// cards beside it still list, and the reason says one was dropped rather than that the host has no cards.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GpuInventory {
    pub cards: Vec<GpuCard>,
    pub skipped: Option<String>,
}

/// Reads the GPU cards of a `/1.0/resources` document. `None` when the document does not carry a usable `gpu.cards` list:
/// an absent or malformed list is unknown, never a shorter list that could read as known-empty. A host with no GPU answers
/// `gpu.total` 0 and `gpu.cards` `[]`, which is a known-empty list. A card without a PCI address is skipped, with a reason,
/// and the other cards still list.
pub fn cards_of(document: &Value) -> Option<GpuInventory> {
    let cards = document.pointer("/gpu/cards")?.as_array()?;
    let mut inventory = GpuInventory::default();
    let mut skipped = 0u64;
    for card in cards {
        match card_of(card) {
            Some(card) => inventory.cards.push(card),
            None => skipped = skipped.saturating_add(1),
        }
    }
    inventory.skipped = skipped_reason(skipped);
    Some(inventory)
}

fn skipped_reason(skipped: u64) -> Option<String> {
    match skipped {
        0 => None,
        1 => Some("1 card without a readable PCI address was skipped".to_string()),
        count => Some(format!(
            "{count} cards without a readable PCI address were skipped"
        )),
    }
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
pub fn read_inventory<R: IncusRead>(incus: &R) -> Option<GpuInventory> {
    match incus.get(RESOURCES_PATH) {
        Ok(document) => match cards_of(&document) {
            Some(inventory) => Some(inventory),
            // A resources document with no `gpu` object at all is a host without the extension; a present but malformed
            // list is unknown.
            None if document.get("gpu").is_none() => Some(GpuInventory::default()),
            None => None,
        },
        Err(arca_incus::IncusError::Status { code: 404, .. }) => Some(GpuInventory::default()),
        Err(_) => None,
    }
}

/// Normalizes a PCI address to four lowercase domain hex digits, so `00000000:01:00.0` from `nvidia-smi` and
/// `0000:01:00.0` from Incus name the same card. A value that is not `domain:bus:slot.function` is returned lowercased, so
/// it is still compared exactly rather than being guessed into another form.
pub fn normalize_pci(text: &str) -> String {
    let text = text.trim().to_ascii_lowercase();
    let parts: Vec<&str> = text.split(':').collect();
    match parts.as_slice() {
        // bus:slot.function, with no domain.
        [bus, rest] if bus.len() == 2 && bus.bytes().all(|byte| byte.is_ascii_hexdigit()) => {
            format!("0000:{bus}:{rest}")
        }
        // domain:bus:slot.function, with a domain of up to eight hex digits.
        [domain, bus, rest]
            if !domain.is_empty()
                && domain.len() <= 8
                && domain.bytes().all(|byte| byte.is_ascii_hexdigit()) =>
        {
            let trimmed = domain.trim_start_matches('0');
            let domain = if trimmed.is_empty() { "0" } else { trimmed };
            format!("{domain:0>4}:{bus}:{rest}")
        }
        _ => text,
    }
}

/// Reads the cards from `nvidia-smi`. It is a trait so a test can give a fake and a node without the tool is a plain
/// [`NvidiaError::Missing`], never a zero reading.
pub trait Nvidia: Send + Sync {
    fn readings(&self) -> Result<Vec<NvidiaReading>, NvidiaError>;
    fn processes(&self) -> Result<Vec<NvidiaProcess>, NvidiaError>;
}

/// The single-flight guard shared by every `nvidia-smi` reader in the process. One read holds it at a time; a killed child
/// that has not been reaped yet also holds it, because the driver may still hold the previous process. A dropped
/// [`NvidiaGuard`] always returns its slot, including when the child cannot be spawned.
#[derive(Debug, Default)]
struct NvidiaGate {
    state: std::sync::Mutex<GateState>,
}

/// The gate's two facts: how many reads hold it, and whether a killed child has not been reaped yet.
#[derive(Debug, Default)]
struct GateState {
    in_flight: usize,
    stuck: bool,
}

/// One read's hold on an [`NvidiaGate`]. Dropping it returns the slot.
#[derive(Debug)]
struct NvidiaGuard {
    gate: std::sync::Arc<NvidiaGate>,
}

impl NvidiaGate {
    /// Enters the gate for one read, or refuses it as busy while another read runs or a killed child waits to be reaped.
    fn begin(self: &std::sync::Arc<Self>) -> Result<NvidiaGuard, NvidiaError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.stuck || state.in_flight > 0 {
            return Err(NvidiaError::Busy);
        }
        state.in_flight = state.in_flight.saturating_add(1);
        Ok(NvidiaGuard {
            gate: std::sync::Arc::clone(self),
        })
    }

    fn mark_stuck(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .stuck = true;
    }

    fn clear(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .stuck = false;
    }
}

impl Drop for NvidiaGuard {
    fn drop(&mut self) {
        let mut state = self
            .gate
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.in_flight = state.in_flight.saturating_sub(1);
    }
}

/// The process's one gate: the sampler and the approval path's in-use read share it.
fn global_gate() -> std::sync::Arc<NvidiaGate> {
    static GATE: std::sync::OnceLock<std::sync::Arc<NvidiaGate>> = std::sync::OnceLock::new();
    std::sync::Arc::clone(GATE.get_or_init(|| std::sync::Arc::new(NvidiaGate::default())))
}

/// The tool on this host. The program is named `nvidia-smi`; a test may point [`NvidiaSmi::at`] elsewhere.
#[derive(Debug, Clone)]
pub struct NvidiaSmi {
    program: std::path::PathBuf,
    timeout: std::time::Duration,
    gate: std::sync::Arc<NvidiaGate>,
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
            timeout: NVIDIA_TIMEOUT,
            gate: global_gate(),
        }
    }

    /// The same reader over another program path.
    pub fn at(program: impl Into<std::path::PathBuf>) -> NvidiaSmi {
        NvidiaSmi {
            program: program.into(),
            timeout: NVIDIA_TIMEOUT,
            gate: global_gate(),
        }
    }

    /// The same reader with another deadline, so a test can prove the kill without waiting five seconds.
    pub fn with_timeout(mut self, timeout: std::time::Duration) -> NvidiaSmi {
        self.timeout = timeout;
        self
    }

    fn run(&self, args: &[&str]) -> Result<String, NvidiaError> {
        use std::io::Read as _;

        // One read at a time, and no read while a killed child is unreaped. The guard returns the slot however this
        // function ends, including when the child cannot be spawned at all.
        let _guard = self.gate.begin()?;
        let mut child = Command::new(&self.program)
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::NotFound => NvidiaError::Missing,
                kind => NvidiaError::Failed(kind.to_string()),
            })?;
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            self.gate.mark_stuck();
            spawn_reaper(
                child,
                std::thread::spawn(Vec::new),
                std::sync::Arc::clone(&self.gate),
            );
            return Err(NvidiaError::Failed(
                "the command has no output pipe".to_string(),
            ));
        };
        // Read beside the child so a full pipe never blocks it. One byte past the cap proves the cap was crossed.
        let reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = stdout
                .take(NVIDIA_MAX_OUTPUT.saturating_add(1) as u64)
                .read_to_end(&mut bytes);
            bytes
        });
        self.finish(child, reader, std::sync::Arc::clone(&self.gate))
    }

    /// Waits for a spawned child under the deadline. A child that does not answer is killed, marked stuck, and handed to a
    /// background reaper with its output reader; the caller returns at once. The reaper clears the gate only after the child
    /// is reaped and the reader has ended, so a child the kernel cannot kill keeps every later read behind the guard.
    fn finish<C: Waitable>(
        &self,
        mut child: C,
        reader: std::thread::JoinHandle<Vec<u8>>,
        gate: std::sync::Arc<NvidiaGate>,
    ) -> Result<String, NvidiaError> {
        let deadline = std::time::Instant::now() + self.timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if std::time::Instant::now() >= deadline => {
                    let _ = child.kill();
                    gate.mark_stuck();
                    spawn_reaper(child, reader, gate);
                    return Err(NvidiaError::TimedOut);
                }
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
                Err(error) => {
                    let _ = child.kill();
                    gate.mark_stuck();
                    spawn_reaper(child, reader, gate);
                    return Err(NvidiaError::Failed(error.kind().to_string()));
                }
            }
        };
        let bytes = reader
            .join()
            .map_err(|_| NvidiaError::Failed("the output reader stopped".to_string()))?;
        if bytes.len() > NVIDIA_MAX_OUTPUT {
            return Err(NvidiaError::Failed(
                "the answer is over the 1 MiB cap".to_string(),
            ));
        }
        if !status.success() {
            return Err(NvidiaError::Failed(
                "the command answered an error".to_string(),
            ));
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}

/// A child the deadline can wait on. The real `std::process::Child` implements it, and a test gives a handle whose kill
/// does nothing and whose wait blocks until the test releases it, proving the caller does not wait for a stuck child.
trait Waitable: Send + 'static {
    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>>;
    fn kill(&mut self) -> std::io::Result<()>;
    fn wait(&mut self) -> std::io::Result<std::process::ExitStatus>;
}

impl Waitable for std::process::Child {
    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        std::process::Child::try_wait(self)
    }

    fn kill(&mut self) -> std::io::Result<()> {
        std::process::Child::kill(self)
    }

    fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        std::process::Child::wait(self)
    }
}

/// Reaps a killed child and its reader on a background thread, then clears the gate. The caller has already returned.
fn spawn_reaper<C: Waitable>(
    mut child: C,
    reader: std::thread::JoinHandle<Vec<u8>>,
    gate: std::sync::Arc<NvidiaGate>,
) {
    let _ = std::thread::Builder::new()
        .name("nvidia-smi-reaper".to_string())
        .spawn(move || {
            let _ = child.wait();
            let _ = reader.join();
            gate.clear();
        });
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

/// Reads `/proc/<pid>/cgroup` under `proc_root`: the cgroup v2 `0::` path when there is one, else the first v1 path that
/// is not `/`. A v1 `devices` line names the container's payload; the v2 unified line names it too.
pub fn read_cgroup(proc_root: &Path, pid: u32) -> Option<String> {
    let text = std::fs::read_to_string(proc_root.join(pid.to_string()).join("cgroup")).ok()?;
    let mut fallback = None;
    for line in text.lines() {
        let mut parts = line.splitn(3, ':');
        let Some(hierarchy) = parts.next() else {
            continue;
        };
        let Some(controllers) = parts.next() else {
            continue;
        };
        let Some(path) = parts.next().map(str::trim) else {
            continue;
        };
        if path.is_empty() {
            continue;
        }
        if hierarchy == "0" && controllers.is_empty() && path != "/" {
            return Some(path.to_string());
        }
        if fallback.is_none() && path != "/" {
            fallback = Some(path.to_string());
        }
    }
    fallback
}

/// The first path segment of a cgroup path, which is the only place an Incus payload may appear.
fn first_segment(path: &str) -> &str {
    path.trim_start_matches('/').split('/').next().unwrap_or("")
}

/// The instance name from an `lxc.payload.` path, project-aware. Incus names a default-project container's payload
/// `lxc.payload.<instance>` and another project's `lxc.payload.<project>_<instance>`. The known instance list is consulted
/// first, so an instance whose own name holds an `_` is matched whole; without that list the first `_` separates the
/// project from the name, and a name without one is in the default project. The literal `lxc.payload.` is matched only as
/// the first path segment: a name deeper in the path cannot claim to be an instance.
pub fn incus_instance(path: &str, instances: &[InstanceKey]) -> Option<(String, String)> {
    let suffix = first_segment(path).strip_prefix("lxc.payload.")?;
    if !printable(suffix, 128) {
        return None;
    }
    for instance in instances {
        if !printable(&instance.project, 128) || !printable(&instance.name, 128) {
            continue;
        }
        if instance.project == "default" && instance.name == suffix {
            return Some((instance.project.clone(), instance.name.clone()));
        }
        if format!("{}_{}", instance.project, instance.name) == suffix {
            return Some((instance.project.clone(), instance.name.clone()));
        }
    }
    let split = suffix.split_once('_');
    let (project, name) = match split {
        Some((project, name)) if printable(project, 128) && printable(name, 128) => (project, name),
        _ => ("default", suffix),
    };
    (printable(project, 128) && printable(name, 128))
        .then(|| (project.to_string(), name.to_string()))
}

/// The Docker container id from a `docker-<id>.scope` cgroup segment. The id is exactly 64 ASCII hex characters; a segment
/// that only looks like a Docker scope is ignored, never attributed.
pub fn docker_container(path: &str) -> Option<String> {
    path.split('/').find_map(docker_scope)
}

/// One cgroup segment as a Docker id, exactly 64 hex characters inside `docker-<id>.scope`.
fn docker_scope(segment: &str) -> Option<String> {
    let id = segment
        .strip_prefix("docker-")
        .and_then(|rest| rest.strip_suffix(".scope"))?;
    (id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| id.to_ascii_lowercase())
}

/// Whether a name is printable ASCII and within its length cap. A name that is not is never shown.
fn printable(text: &str, max: usize) -> bool {
    !text.is_empty() && text.len() <= max && text.bytes().all(|byte| byte.is_ascii_graphic())
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
    // The first segment is checked before any Docker scope, so a Docker cgroup inside an Incus instance belongs to the
    // instance. A first segment that names `lxc.payload.` but resolves no instance is not retried as Docker: it is host
    // attribution, never a second guess at a spoofed path.
    if first_segment(path).starts_with("lxc.payload.") {
        return match incus_instance(path, instances) {
            Some((project, name)) => GpuOwner::Instance {
                project,
                name,
                container: docker_container(path),
            },
            None => GpuOwner::Host,
        };
    }
    match host_docker_container(path) {
        Some(name) => GpuOwner::Container { name },
        None => GpuOwner::Host,
    }
}

/// A Docker container on the host: a strict `docker-<64 hex>.scope` segment below `system.slice`, and nowhere else. A
/// cgroup under `user.slice` or any other hierarchy, or a path with `.`/`..` segments, is not a host Docker container.
fn host_docker_container(path: &str) -> Option<String> {
    let segments: Vec<&str> = path
        .trim_start_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.is_empty()
        || segments
            .iter()
            .any(|segment| matches!(*segment, "." | ".."))
    {
        return None;
    }
    if segments.first() != Some(&"system.slice") {
        return None;
    }
    segments[1..]
        .iter()
        .find_map(|segment| docker_scope(segment))
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
        skipped: last.skipped.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, mpsc};
    use std::time::{Duration, Instant};

    /// A child whose kill does nothing and whose wait blocks until the test releases it. It models a process the kernel
    /// cannot reap, which is exactly the case the old kill-then-wait branch blocked on.
    struct StuckChild {
        release: mpsc::Receiver<()>,
    }

    impl Waitable for StuckChild {
        fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
            Ok(None)
        }

        fn kill(&mut self) -> std::io::Result<()> {
            Ok(())
        }

        fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
            use std::os::unix::process::ExitStatusExt as _;
            let _ = self.release.recv();
            Ok(std::process::ExitStatus::from_raw(0))
        }
    }

    #[test]
    fn every_reader_in_the_process_shares_one_gate() {
        let first = NvidiaSmi::new();
        let second = NvidiaSmi::at("nvidia-smi");
        assert!(
            std::sync::Arc::ptr_eq(&first.gate, &second.gate),
            "the sampler and the approval path's in-use read share the guard"
        );
    }

    #[test]
    fn a_second_read_is_busy_until_the_first_read_finishes() {
        let gate = Arc::new(NvidiaGate::default());
        let first = gate.begin().expect("the first read enters");
        let refused = gate.begin().expect_err("a concurrent read is refused");
        assert_eq!(refused, NvidiaError::Busy);
        assert_eq!(
            NvidiaError::Busy.to_string(),
            "the previous nvidia-smi read has not finished"
        );
        drop(first);
        let third = gate.begin().expect("the slot is free again");
        drop(third);
    }

    #[test]
    fn a_child_that_cannot_be_spawned_releases_the_gate_slot() {
        let gate = Arc::new(NvidiaGate::default());
        let smi = NvidiaSmi {
            program: std::path::PathBuf::from("/nonexistent/arca-test-nvidia-smi"),
            timeout: Duration::from_millis(20),
            gate: Arc::clone(&gate),
        };
        assert_eq!(
            smi.readings().expect_err("the program is missing"),
            NvidiaError::Missing
        );
        assert_eq!(
            smi.readings()
                .expect_err("the next read is missing, not busy"),
            NvidiaError::Missing,
            "the spawn failure returned its gate slot"
        );
    }

    #[test]
    fn a_stuck_child_is_reaped_in_the_background_and_the_gate_reports_busy() {
        let gate = Arc::new(NvidiaGate::default());
        let (child_release, child_wait) = mpsc::channel();
        let (reader_release, reader_wait) = mpsc::channel();
        let reader_done = Arc::new(AtomicBool::new(false));
        let done = Arc::clone(&reader_done);
        let reader = std::thread::spawn(move || {
            let _ = reader_wait.recv();
            done.store(true, Ordering::SeqCst);
            Vec::new()
        });
        let smi = NvidiaSmi {
            program: std::path::PathBuf::from("nvidia-smi"),
            timeout: Duration::from_millis(20),
            gate: Arc::clone(&gate),
        };

        let started = Instant::now();
        let error = smi
            .finish(
                StuckChild {
                    release: child_wait,
                },
                reader,
                Arc::clone(&gate),
            )
            .expect_err("the deadline expires");
        assert_eq!(error, NvidiaError::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the caller returned without waiting for the stuck child"
        );

        // The guard refuses a new read, with the exact plain reason, while the child is unreaped.
        assert!(gate.begin().is_err(), "an unreaped child holds the gate");
        assert_eq!(
            NvidiaError::Busy.to_string(),
            "the previous nvidia-smi read has not finished"
        );
        assert_eq!(
            smi.readings().expect_err("the guard refuses a new run"),
            NvidiaError::Busy,
            "the public read path checks the guard before spawning"
        );

        // The child is released, but its reader is still held: the reaper must wait for both before clearing.
        child_release.send(()).expect("release the child");
        std::thread::sleep(Duration::from_millis(30));
        assert!(
            gate.begin().is_err(),
            "the reader still holds the gate with the child reaped"
        );
        assert!(!reader_done.load(Ordering::SeqCst));

        reader_release.send(()).expect("release the reader");
        let until = Instant::now() + Duration::from_secs(1);
        while gate.begin().is_err() {
            assert!(Instant::now() < until, "the reaper cleared the gate");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(reader_done.load(Ordering::SeqCst), "the reader ended");
    }
}
