//! Arca's host sampler. It reads the node's procfs and sysfs trees, and asks Incus for the configured pools' space and the
//! instances per project with GET requests only. Each sample is served in the `node_metrics` shape, and no sample carries a
//! renter gate.

mod incus;
mod procfs;
mod sample;

pub use sample::{
    Cpu, DEFAULT_MINUTES, HostSample, Load, Memory, Network, Pool, ProjectCount, Psi, Sampler,
    SamplerConfig, mean, node_metrics,
};
