//! Arca's host sampler. It reads the node's procfs and sysfs trees, and asks Incus for the storage pools its allow-filter admits
//! and the instances per project with GET requests only. Each sample is served in the `node_metrics` shape, and no sample carries
//! a renter gate. [`Readings`] keeps what Incus answered at the latest sample, with a `None` for each read that failed.

mod incus;
mod procfs;
mod sample;

pub use incus::{InstanceKind, InstanceReading};
pub use sample::{
    Cpu, DEFAULT_MINUTES, HostSample, Load, Memory, Network, NicCounters, Pool, ProjectCount, Psi,
    Readings, Sampler, SamplerConfig, mean, node_metrics,
};
