//! Arca's Incus access. The read client sends only GET. The journal writer changes kernel-family objects under ownership
//! checks. The adoption writer has its separate closed migration table. The device assigner changes only one existing
//! instance's `devices` map, only for a kernel-family instance after the owner approved an action proposal; it is not part
//! of the desired-state journal. The snapshot writer adds snapshot pools and copies apps and data volumes into its own
//! `arcak-snapshots` project after the owner approved it, and writes nowhere else. The
//! service exec module adds only closed commands, fixed file destinations and exact recording cleanup; it is called by
//! serve's audited seam, never by Provider or plugins.
//! The recording shim refuses every method except GET. The fake serves fixtures and accepts writes only in memory.

#[cfg(feature = "write")]
pub mod adopt;
#[cfg(feature = "write")]
pub mod assign;
#[cfg(feature = "write")]
pub mod exec;
pub mod fake;
pub mod fake_storage;
pub mod read;
pub mod shim;
#[cfg(feature = "write")]
pub mod snapshot;
#[cfg(feature = "write")]
pub mod write;

#[cfg(feature = "write")]
pub use adopt::{
    ADOPT_ENTRY_PREFIX, AdoptRequest, AdoptStep, AdoptTarget, AdoptWriter, Allowances, Device,
    Devices, Expect, ExpectClass, NETWORKS_ACCESS, NIC_KEPT_KEYS, Needed, PIN_KEYS, Pin, PreImage,
    Side, StepOutcome, adoption_project_body, needed_allowances, pin_patch,
};
#[cfg(feature = "write")]
pub use assign::{DeviceAssigner, GpuChange, InstanceDevices, device_name, normalize_pci};
pub use fake::{Appears, OpId};
pub use fake::{FakeIncus, FakeState, RecordedRequest, WriteRecord};
pub use read::{IncusError, IncusRead, Target, Unconfirmed, UnixIncus};
pub use read::{
    OPERATIONS_PATH, RunningOp, SESSION_DESCRIPTIONS, TOKEN_CLASS, operation_names, running_on,
    running_operations, wait_operation, writing_on,
};
pub use shim::{RecordingShim, ShimHandle, ShimSummary};
#[cfg(feature = "write")]
pub use snapshot::{
    PoolSource, Progress, SNAPSHOT_POOL_DESCRIPTION, SNAPSHOT_PROJECT, SNAPSHOT_STORAGE_KEY,
    SnapshotWriter,
};
#[cfg(feature = "write")]
pub use write::{Owned, ProjectFamily, Reading, UnixIncusWriter};
