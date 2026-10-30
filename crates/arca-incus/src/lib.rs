//! Arca's Incus access. Every request this crate can send is a GET. The client has no other method, and the recording
//! shim answers any other method with 405 without forwarding it. The fake Incus serves fixed routes from a fixture.

pub mod fake;
pub mod read;
pub mod shim;

pub use fake::{FakeIncus, FakeState, RecordedRequest};
pub use read::{IncusError, IncusRead, UnixIncus};
pub use shim::{RecordingShim, ShimHandle, ShimSummary};
