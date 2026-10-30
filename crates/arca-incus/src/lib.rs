//! Arca's Incus access. The read client sends only GET. The writer is the one module that sends POST, PATCH or DELETE, and every
//! write it sends is gated by the ownership classifier. The recording shim answers any method other than GET with 405 without
//! forwarding it. The fake Incus serves a fixture and accepts the writer's requests in memory.

pub mod fake;
pub mod read;
pub mod shim;
pub mod write;

pub use fake::{FakeIncus, FakeState, RecordedRequest, WriteRecord};
pub use read::{IncusError, IncusRead, UnixIncus};
pub use shim::{RecordingShim, ShimHandle, ShimSummary};
pub use write::{Owned, Target, UnixIncusWriter};
