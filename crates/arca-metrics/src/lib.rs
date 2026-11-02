//! Arca's host sampler. It reads the node's procfs and sysfs trees, and asks Incus for the storage pools its allow-filter admits
//! and the instances per project with GET requests only. Each sample is served in the `node_metrics` shape, and no sample carries
//! a renter gate. [`Readings`] keeps what Incus answered at the latest sample, with a `None` for each read that failed, and the
//! newest latency probe round.

mod incus;
pub mod probe;
mod procfs;
mod sample;

pub use incus::{InstanceKind, InstanceReading};
pub use probe::ProbeTarget;
pub use sample::{
    Cpu, DEFAULT_MINUTES, HostSample, LatencyReading, Load, Memory, Network, NicCounters, Pool,
    ProjectCount, Psi, Readings, Sampler, SamplerConfig, mean, node_metrics,
};
