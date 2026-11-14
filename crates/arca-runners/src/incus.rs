//! The Incus launcher: one virtual machine per job, cloned from the runner image, used once, deleted.
//!
//! Every write goes through [`arca_incus::runner_vm::RunnerVms`], which touches only instances carrying the pool's
//! mark in the pool's project. The runner configuration reaches the machine as a file on its `/run` (memory) through the
//! Incus agent; the guest script `arca-job` (shipped in this crate, written into the machine before each start) reads
//! and deletes it, runs the runner once and packs its logs.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use arca_incus::IncusError;
use arca_incus::runner_vm::{ExecState, RunnerVms, VmSource, VmSpec};

use crate::launcher::{LaunchError, Launcher, MachineSpec, RunnerStart};
use crate::secret::Secret;

/// The guest script, written into each machine before its runner starts.
pub const GUEST_SCRIPT: &str = include_str!("../guest/arca-job.sh");
/// Where the guest script is written.
pub const GUEST_SCRIPT_PATH: &str = "/usr/local/sbin/arca-job";
/// Where the runner configuration is written: `/run` is memory.
pub const GUEST_JIT_PATH: &str = "/run/arca-job-jit";
/// Where the guest script leaves the packed logs and the exit code.
pub const GUEST_LOGS_PATH: &str = "/var/log/arca-job/logs.tar.gz";
pub const GUEST_EXIT_PATH: &str = "/var/log/arca-job/exit";

/// The Incus launcher of one pool.
pub struct IncusLauncher {
    vms: RunnerVms,
    source: VmSource,
    profiles: Vec<String>,
    storage_pool: Option<String>,
    /// The agent operation of each started runner.
    runs: Mutex<BTreeMap<String, String>>,
}

impl IncusLauncher {
    /// A launcher for `pool`'s machines in `project` on the Incus socket at `socket`. `image` is an image alias or
    /// fingerprint, or `<instance>/<snapshot>`.
    pub fn new(
        socket: PathBuf,
        project: &str,
        pool: &str,
        image: &str,
        profiles: Vec<String>,
        storage_pool: Option<String>,
    ) -> Result<Self, LaunchError> {
        let source = match image.split_once('/') {
            Some((instance, snapshot)) => VmSource::Snapshot {
                instance: instance.to_string(),
                snapshot: snapshot.to_string(),
            },
            None => VmSource::Image(image.to_string()),
        };
        Ok(Self {
            vms: RunnerVms::new(socket, project, pool).map_err(incus)?,
            source,
            profiles,
            storage_pool,
            runs: Mutex::new(BTreeMap::new()),
        })
    }

    fn operation(&self, name: &str) -> Option<String> {
        self.runs
            .lock()
            .ok()
            .and_then(|runs| runs.get(name).cloned())
    }

    /// The exit code the guest script wrote, when it has written one.
    fn guest_exit(&self, name: &str) -> Result<Option<i64>, LaunchError> {
        Ok(self
            .vms
            .pull_file(name, GUEST_EXIT_PATH)
            .map_err(incus)?
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .and_then(|text| text.trim().parse().ok()))
    }
}

impl Launcher for IncusLauncher {
    fn create(&self, spec: &MachineSpec) -> Result<(), LaunchError> {
        self.vms
            .create(&VmSpec {
                name: spec.name.clone(),
                source: self.source.clone(),
                cpu: spec.cpu,
                memory_mib: spec.memory_mib,
                root_gib: spec.disk_gib,
                storage_pool: self.storage_pool.clone(),
                profiles: self.profiles.clone(),
            })
            .map_err(incus)
    }

    fn ready(&self, name: &str) -> Result<bool, LaunchError> {
        match self.vms.agent_ready(name) {
            Ok(ready) => Ok(ready),
            // A VM whose agent is not up yet answers the file read with an error status.
            Err(IncusError::Status { .. }) => Ok(false),
            Err(error) => Err(incus(error)),
        }
    }

    fn start_runner(
        &self,
        name: &str,
        jit: &Secret,
        start: &RunnerStart,
    ) -> Result<(), LaunchError> {
        self.vms
            .push_file(
                name,
                GUEST_SCRIPT_PATH,
                GUEST_SCRIPT.as_bytes(),
                0,
                0,
                0o755,
            )
            .map_err(incus)?;
        self.vms
            .push_file(name, GUEST_JIT_PATH, jit.expose().as_bytes(), 0, 0, 0o600)
            .map_err(incus)?;
        let operation = self
            .vms
            .exec(
                name,
                &[GUEST_SCRIPT_PATH],
                &[("ARCA_RUNNER_DIR", start.runner_dir.as_str())],
            )
            .map_err(incus)?;
        if let Ok(mut runs) = self.runs.lock() {
            runs.insert(name.to_string(), operation);
        }
        Ok(())
    }

    fn runner_exited(&self, name: &str, wait: Duration) -> Result<Option<i64>, LaunchError> {
        let Some(operation) = self.operation(name) else {
            // No run started here (a kernel restart): only the guest's own record can say.
            return self.guest_exit(name);
        };
        match self.vms.exec_state(&operation, wait).map_err(incus)? {
            ExecState::Running => Ok(None),
            ExecState::Exited(code) => Ok(Some(code)),
            ExecState::Gone => {
                let code = self.guest_exit(name)?;
                if code.is_none() {
                    // Incus forgot the operation (it restarted) while the runner may still run; look again later.
                    std::thread::sleep(wait.min(Duration::from_secs(5)));
                }
                Ok(code)
            }
        }
    }

    fn collect_logs(&self, name: &str) -> Result<Option<Vec<u8>>, LaunchError> {
        self.vms.pull_file(name, GUEST_LOGS_PATH).map_err(incus)
    }

    fn destroy(&self, name: &str) -> Result<(), LaunchError> {
        let done = self.vms.destroy(name).map_err(incus);
        if let Ok(mut runs) = self.runs.lock() {
            runs.remove(name);
        }
        done
    }

    fn existing(&self) -> Result<Vec<String>, LaunchError> {
        self.vms.list().map_err(incus)
    }
}

fn incus(error: IncusError) -> LaunchError {
    LaunchError(format!("incus: {error}"))
}
