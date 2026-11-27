//! The Incus launcher: one virtual machine per job, cloned from the runner image, used once, deleted.
//!
//! Every write goes through [`arca_incus::runner_vm::RunnerVms`], which touches only instances carrying the pool's
//! mark in the pool's project, and the pool's one network ACL carrying the same mark.
//!
//! **The card.** Each machine's `eth0` is on the pool's bridge ([`crate::PoolConfig::bridge`]), with MAC, IPv4 and IPv6
//! filtering and a default of reject both ways, and names the pool's ACL `arca-runner-<pool>`. The ACL is made (or
//! found and reused) when the launcher is made, at the pool's start, from the bridge's own gateway and subnet and the
//! host's addresses: no ingress rule, so nothing reaches a job machine but the answers to what it opened, and the egress
//! rules of a tenant's service with the whole subnet rejected ([`arca_netproject::tenant::job_egress_rules`]). A pool
//! that is gone from the config has its ACL deleted at the next start ([`remove_gone_pool_acls`]). The runner configuration reaches the machine as a file on its `/run` (memory) through the
//! Incus agent; the guest script `arca-job` (shipped in this crate, written into the machine before each start) reads
//! and deletes it, runs the runner once and packs its logs.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use arca_incus::runner_vm::{
    AclState, ExecState, RUNNER_ACL_PREFIX, RUNNER_KEY, RunnerVms, VmSource, VmSpec,
};
use arca_incus::{IncusError, IncusRead as _, UnixIncus};
use serde_json::Value;

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
    network: String,
    acl: AclState,
    /// The agent operation of each started runner.
    runs: Mutex<BTreeMap<String, String>>,
}

impl IncusLauncher {
    /// A launcher for `pool`'s machines in `project` on the Incus socket at `socket`, each with its card on the bridge
    /// `network`. `image` is an image alias or fingerprint (`arca-runner-image-<tag>`, as the image build publishes it),
    /// or `<instance>/<snapshot>` (`arca-runner-image-<tag>/image`, the stopped machine the build keeps).
    ///
    /// It makes the pool's ACL, or finds it and reuses it (its rules replaced when the bridge or the host's addresses
    /// changed): the bridge must have an IPv4 address, its gateway.
    pub fn new(
        socket: PathBuf,
        project: &str,
        pool: &str,
        image: &str,
        profiles: Vec<String>,
        storage_pool: Option<String>,
        network: &str,
    ) -> Result<Self, LaunchError> {
        let source = match image.split_once('/') {
            Some((instance, snapshot)) => VmSource::Snapshot {
                instance: instance.to_string(),
                snapshot: snapshot.to_string(),
            },
            None => VmSource::Image(image.to_string()),
        };
        let vms = RunnerVms::new(socket.clone(), project, pool).map_err(incus)?;
        let egress = job_rules(&socket, project, network)?;
        // Nothing inbound: no ingress rule, and the card's default rejects the rest.
        let acl = vms.ensure_acl(&[], &egress).map_err(incus)?;
        Ok(Self {
            vms,
            source,
            profiles,
            storage_pool,
            network: network.to_string(),
            acl,
            runs: Mutex::new(BTreeMap::new()),
        })
    }

    /// What became of the pool's ACL when this launcher was made.
    pub fn acl(&self) -> AclState {
        self.acl
    }

    /// The pool's ACL's name: `arca-runner-<pool>`.
    pub fn acl_name(&self) -> String {
        self.vms.acl_name()
    }

    /// Deletes the pool's ACL: the pool is gone. Incus refuses it while a machine's card still names it.
    pub fn remove_acl(&self) -> Result<(), LaunchError> {
        self.vms.remove_acl().map_err(incus)
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
                network: self.network.clone(),
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
                &[
                    ("ARCA_RUNNER_DIR", start.runner_dir.as_str()),
                    ("ARCA_WORK_FOLDER", start.work_folder.as_str()),
                ],
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

/// The egress rules of the pool's job machines on the bridge `network`, read as `project` sees it: the rules of a
/// tenant's service with the bridge's whole subnet rejected, from the bridge's gateway and the host's addresses.
fn job_rules(socket: &Path, project: &str, network: &str) -> Result<Vec<Value>, LaunchError> {
    let plain = |text: &str| {
        !text.is_empty()
            && text.len() <= 63
            && text
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    };
    if !plain(network) || !plain(project) {
        return Err(LaunchError(
            "the pool's network or project is not a plain name".to_string(),
        ));
    }
    let read = UnixIncus::new(socket.to_path_buf());
    let bridge = read
        .get(&format!("/1.0/networks/{network}?project={project}"))
        .map_err(|error| LaunchError(format!("incus: the bridge {network}: {error}")))?;
    let (gateway, subnet) = bridge
        .pointer("/config/ipv4.address")
        .and_then(Value::as_str)
        .and_then(gateway_and_subnet)
        .ok_or_else(|| {
            LaunchError(format!(
                "the bridge {network} has no IPv4 address: a job machine's rules are built from its gateway"
            ))
        })?;
    let hosts = arca_providers::incus::host_networks(&read)
        .map(|networks| arca_providers::incus::host_addresses_value(&networks))
        .map_err(|error| LaunchError(format!("incus: the host's addresses: {error:?}")))?;
    Ok(arca_netproject::tenant::job_egress_rules(
        gateway, &subnet, &hosts,
    ))
}

/// The gateway and the subnet (`a.b.c.d/n`, its network address) of a bridge whose `ipv4.address` is `address`.
fn gateway_and_subnet(address: &str) -> Option<(Ipv4Addr, String)> {
    let (ip, prefix) = address.split_once('/')?;
    let gateway: Ipv4Addr = ip.parse().ok()?;
    let prefix: u32 = prefix.parse().ok().filter(|prefix| *prefix <= 32)?;
    let mask = u32::MAX.checked_shl(32 - prefix).unwrap_or(0);
    let network = Ipv4Addr::from(u32::from(gateway) & mask);
    Some((gateway, format!("{network}/{prefix}")))
}

/// Deletes the ACL of every pool that is gone from the config: an ACL of any project named `arca-runner-<pool>` and
/// carrying the runner mark with that pool's name, for a pool not among `pools`. Each delete goes through that pool's
/// own writer, which reads the mark again first. Returns one line for each ACL removed or left, and one when the
/// listing failed.
pub fn remove_gone_pool_acls(socket: &Path, pools: &[&str]) -> Vec<String> {
    let read = UnixIncus::new(socket.to_path_buf());
    let rows = match read.get("/1.0/network-acls?all-projects=true&recursion=1") {
        Ok(Value::Array(rows)) => rows,
        Ok(_) => return vec!["the network ACL listing is not a list".to_string()],
        Err(error) => return vec![format!("the network ACLs could not be listed: {error}")],
    };
    let mut lines = Vec::new();
    for row in rows {
        let Some(pool) = row.pointer(&format!("/config/{RUNNER_KEY}")).and_then(Value::as_str) else {
            continue;
        };
        let name = row.get("name").and_then(Value::as_str).unwrap_or_default();
        let project = row.get("project").and_then(Value::as_str).unwrap_or("default");
        if pools.contains(&pool) || name != format!("{RUNNER_ACL_PREFIX}{pool}") {
            continue;
        }
        let removed = RunnerVms::new(socket.to_path_buf(), project, pool)
            .and_then(|vms| vms.remove_acl());
        lines.push(match removed {
            Ok(()) => format!("pool {pool} is gone: its network ACL {name} was deleted"),
            Err(error) => format!("pool {pool} is gone, and its network ACL {name} stays: {error}"),
        });
    }
    lines
}

fn incus(error: IncusError) -> LaunchError {
    LaunchError(format!("incus: {error}"))
}
