//! Arca's CI runners: a fresh machine for every GitHub Actions job.
//!
//! - [`scaleset`]: GitHub's runner scale set protocol, ported from `actions/scaleset` (the client
//!   actions-runner-controller's listener uses): scale sets, message sessions, the long poll, job acquisition and
//!   just-in-time runner configurations.
//! - [`access`]: the seam to whoever holds the GitHub credential. The kernel's signer implements it; the key never
//!   reaches this crate. [`access::RegistrationAccess`] derives the Actions service connection from an installation
//!   token, for the tests and for a credential held elsewhere.
//! - [`listener`]: one long poll per scale set, handing every message to the pool.
//! - [`pool`]: the controller. It keeps, per size class, `warm + assigned jobs` runners inside the pool's budget, starts
//!   one machine per runner, records each job and destroys the machine when its runner exits.
//! - [`launcher`]: the trait a job machine is made through, with the Incus virtual machine launcher ([`incus`]) and an
//!   in-memory one for tests ([`fake`]).
//! - [`signin`]: each pool's sign-in state, which Arca's alerts read.
//! - [`fake`]: a fake GitHub that speaks the protocol and plays GitHub's part with a queue of jobs.
//!
//! The design is in `docs/RUNNERS.md`.

pub mod access;
pub mod config;
pub mod fake;
pub mod incus;
pub mod launcher;
pub mod listener;
pub mod pool;
pub mod record;
pub mod scaleset;
pub mod secret;
pub mod signin;
pub mod time;

pub use access::{Access, AccessError, ActionsConnection, JobRef, MintRequest, Minted};
pub use config::{ClassConfig, PoolConfig};
pub use launcher::{LaunchError, Launcher, MachineSpec};
pub use pool::Pool;
pub use secret::Secret;
pub use signin::{SignInBoard, SignInReason};
