//! Arca's Incus access. The read client sends only GET. The writer is the one module that sends POST, PATCH, PUT or DELETE, and it
//! sends a PUT only for a project's config. It is compiled only with the `write` feature, and every write it sends is gated by the
//! project family and the ownership classifier.
//! The recording shim answers any method other than GET with 405 without forwarding it. The fake Incus serves a fixture and
//! accepts the writer's requests in memory.

pub mod fake;
pub mod read;
pub mod shim;
#[cfg(feature = "write")]
pub mod write;

pub use fake::{Appears, OpId};
pub use fake::{FakeIncus, FakeState, RecordedRequest, WriteRecord};
pub use read::{IncusError, IncusRead, Target, Unconfirmed, UnixIncus};
pub use read::{
    OPERATIONS_PATH, RunningOp, operation_names, running_on, running_operations, wait_operation,
};
pub use shim::{RecordingShim, ShimHandle, ShimSummary};
#[cfg(feature = "write")]
pub use write::{Owned, ProjectFamily, Reading, UnixIncusWriter};
