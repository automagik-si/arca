//! The job machine, behind a trait: the controller asks for a machine of a size, hands it one runner configuration,
//! waits for its runner to exit, reads its logs and destroys it. How the machine is made is the launcher's: an Incus
//! virtual machine today ([`crate::incus::IncusLauncher`]), a Cloud Hypervisor microVM later.
//!
//! The calls block: the controller makes them on tokio's blocking threads.

use std::time::Duration;

use crate::secret::Secret;

/// One machine to make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineSpec {
    /// Unique among the pool's machines; also the runner's name on GitHub.
    pub name: String,
    pub cpu: u32,
    pub memory_mib: u64,
    pub disk_gib: Option<u64>,
}

/// What the runner was given to start with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerStart {
    /// The runner's own folder in the image.
    pub runner_dir: String,
}

/// Why a launcher call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchError(pub String);

impl std::fmt::Display for LaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A maker of job machines.
pub trait Launcher: Send + Sync + 'static {
    /// Makes the machine and starts it. On return it is booting.
    fn create(&self, spec: &MachineSpec) -> Result<(), LaunchError>;

    /// Whether the machine has booted far enough to be handed its runner.
    fn ready(&self, name: &str) -> Result<bool, LaunchError>;

    /// Hands the machine its one runner configuration and starts the runner.
    fn start_runner(
        &self,
        name: &str,
        jit: &Secret,
        start: &RunnerStart,
    ) -> Result<(), LaunchError>;

    /// Waits up to `wait` for the runner to exit. `Some(code)` once it has.
    fn runner_exited(&self, name: &str, wait: Duration) -> Result<Option<i64>, LaunchError>;

    /// The runner's packed output and diagnostics, when the machine has them.
    fn collect_logs(&self, name: &str) -> Result<Option<Vec<u8>>, LaunchError>;

    /// Stops and deletes the machine. One that is gone is not an error.
    fn destroy(&self, name: &str) -> Result<(), LaunchError>;

    /// The pool's machines that exist now, whatever made them: at start, every one of them is left over.
    fn existing(&self) -> Result<Vec<String>, LaunchError>;
}
