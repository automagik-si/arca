//! Arca's host sampler. It reads the node's procfs and sysfs trees when its Incus is the local daemon, and asks Incus for the
//! host totals of a remote machine, the storage pools its allow-filter admits, and the instances per project with GET requests
//! only. Each sample is served in the `node_metrics` shape, and no sample carries a renter gate. [`Readings`] keeps what Incus
//! answered at the latest sample, with a `None` for each read that failed, the newest latency probe round, and the incus-osd
//! update and Secure Boot reading.

pub mod gpu;
mod incus;
mod mode;
mod osd;
pub mod probe;
mod procfs;
mod sample;
mod smart;

pub use gpu::{
    FakeNvidia, GpuCard, GpuCardSample, GpuInventory, GpuOwner, GpuProcess, GpuSection,
    GpuTelemetry, GpuUse, INVENTORY_REASON, InstanceKey, NO_READING_YET, NOT_MEASURABLE,
    NVIDIA_BUSY_REASON, NVIDIA_FAILED, NVIDIA_MAX_OUTPUT, NVIDIA_MISSING, NVIDIA_TIMEOUT,
    NVIDIA_TIMEOUT_REASON, Nvidia, NvidiaError, NvidiaProcess, NvidiaReading, NvidiaSmi, attribute,
    cards_of, mean_gpu, normalize_pci, parse_processes, parse_readings, read_inventory,
};
pub use incus::{InstanceKind, InstanceReading};
pub use mode::{
    HostMode, HostModeChoice, MODE_PROBE_TIMEOUT, ModeRetry, choose_host_mode,
    choose_host_mode_within,
};
pub use osd::OsdReading;
pub use probe::ProbeTarget;
pub use sample::{
    Cpu, DEFAULT_MINUTES, HostSample, LatencyReading, Load, MODE_PROBE_EVERY_MS, Memory,
    MonotonicClock, Network, NicCounters, PROVISIONAL_WINDOW_MS, Pool, ProjectCount, Psi,
    REMOTE_HOST_REASON, RESOURCES_REASON, Readings, Sampler, SamplerConfig, mean, node_metrics,
};
pub use smart::{DriveReading, DrivesReading};
