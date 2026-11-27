//! The Incus launcher: one virtual machine per job, cloned from the runner image, used once, deleted.
//!
//! Every write goes through [`arca_incus::runner_vm::RunnerVms`], which touches only instances carrying the pool's
//! mark in the pool's project, and the pool's network ACL and own bridge carrying the same mark.
//!
//! **The network.** By default a pool gets its own bridge, `arcarun-<hash>`, which the launcher makes at the pool's
//! start (or finds and reuses): a /24 with NAT and no IPv6, in `172.16.0.0/12`, the first one clear of the node's
//! service subnet, its reserved ranges, every Incus network and every host network, tried from a point the pool's name
//! fixes ([`choose_subnet`]). So a job cannot use up the service bridge's addresses, take one inside a tenant's block
//! or learn its neighbours' names from its DNS. A pool's `network` names an existing bridge instead.
//!
//! **The card.** Each machine's `eth0`, and its only card, is on that bridge, with MAC, IPv4 and IPv6 filtering and a
//! default of reject both ways, and names the pool's ACL `arca-runner-<pool>`: no ingress rule, so nothing reaches a job
//! machine but the answers to what it opened, and the egress rules of a tenant's service with the bridge's whole subnet
//! and the node's service subnet rejected ([`arca_netproject::tenant::job_egress_rules`]). The ACL is made at the pool's
//! start and its rules are made right again before each machine, from the host's addresses as they are then. A pool
//! that is gone from the config has its ACL and its own bridge deleted at the next start ([`remove_gone_pools`]).
//!
//! The runner configuration reaches the machine as a file on its `/run` (memory) through the
//! Incus agent; the guest script `arca-job` (shipped in this crate, written into the machine before each start) reads
//! and deletes it, runs the runner once and packs its logs.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use arca_incus::runner_vm::{
    AclState, ExecState, RUNNER_KEY, RunnerVms, VmSource, VmSpec, pool_hash,
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

/// Where a pool's machines go on the network, from the node's config and the pool's.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetworkPlan {
    /// The bridge the pool's `network` names; `None` gives the pool its own bridge.
    pub bridge: Option<String>,
    /// The node's service subnet (`kernel.serviceCidr`): closed to every job machine, and kept clear by the pool's own
    /// bridge.
    pub service_cidr: Option<String>,
    /// Ranges the pool's own bridge keeps clear of (`kernel.reservedCidrs`).
    pub reserved: Vec<String>,
}

/// The space a pool's own bridge is chosen in: the /24s of `172.16.0.0/12`.
const OWN_SPACE: u32 = 0xac10_0000;
const OWN_SLOTS: u32 = 4096;

/// The gateway address (`a.b.c.1/24`) of the pool's own bridge: the first /24 of `172.16.0.0/12`, from a point the
/// pool's name fixes, that overlaps none of `taken`. Each of `taken` is a CIDR range or a bare address, which counts as
/// its /24. `None` when every /24 is taken.
pub fn choose_subnet(pool: &str, taken: &[String]) -> Option<String> {
    let ranges: Vec<(u32, u32)> = taken.iter().filter_map(|range| range_of(range)).collect();
    let start = pool_hash(pool) % OWN_SLOTS;
    (0..OWN_SLOTS)
        .map(|step| OWN_SPACE | (((start + step) % OWN_SLOTS) << 8))
        .find(|base| {
            ranges.iter().all(|(other, mask)| {
                let common = mask & 0xffff_ff00;
                base & common != other & common
            })
        })
        .map(|base| format!("{}/24", Ipv4Addr::from(base | 1)))
}

/// A CIDR range as its network address and mask; a bare address is its /24.
fn range_of(text: &str) -> Option<(u32, u32)> {
    let (ip, prefix) = match text.split_once('/') {
        Some((ip, prefix)) => (ip, prefix.parse::<u32>().ok().filter(|prefix| *prefix <= 32)?),
        None => (text, 24),
    };
    let ip = u32::from(ip.parse::<Ipv4Addr>().ok()?);
    let mask = u32::MAX.checked_shl(32 - prefix).unwrap_or(0);
    Some((ip & mask, mask))
}

/// The Incus launcher of one pool.
pub struct IncusLauncher {
    vms: RunnerVms,
    socket: PathBuf,
    source: VmSource,
    profiles: Vec<String>,
    storage_pool: Option<String>,
    network: String,
    gateway: Ipv4Addr,
    /// The subnets the rules close: the bridge's, then the service subnet.
    closed: Vec<String>,
    acl: AclState,
    /// The agent operation of each started runner.
    runs: Mutex<BTreeMap<String, String>>,
}

impl IncusLauncher {
    /// A launcher for `pool`'s machines in `project` on the Incus socket at `socket`, each with its card on the bridge
    /// `plan` gives. `image` is an image alias or fingerprint (`arca-runner-image-<tag>`, as the image build publishes
    /// it), or `<instance>/<snapshot>` (`arca-runner-image-<tag>/image`, the stopped machine the build keeps).
    ///
    /// It makes the pool's own bridge when the plan names none, or finds it and reuses it, and the pool's ACL the same
    /// way (its rules replaced when they changed). A named bridge must have an IPv4 address, its gateway.
    pub fn new(
        socket: PathBuf,
        project: &str,
        pool: &str,
        image: &str,
        profiles: Vec<String>,
        storage_pool: Option<String>,
        plan: &NetworkPlan,
    ) -> Result<Self, LaunchError> {
        let source = match image.split_once('/') {
            Some((instance, snapshot)) => VmSource::Snapshot {
                instance: instance.to_string(),
                snapshot: snapshot.to_string(),
            },
            None => VmSource::Image(image.to_string()),
        };
        let vms = RunnerVms::new(socket.clone(), project, pool).map_err(incus)?;
        let network = match &plan.bridge {
            Some(bridge) => bridge.clone(),
            None => {
                if vms.own_bridge().map_err(incus)?.is_none() {
                    let address = choose_subnet(pool, &taken(&socket, plan)?).ok_or_else(|| {
                        LaunchError("no /24 of 172.16.0.0/12 is free for the pool's own bridge".to_string())
                    })?;
                    vms.create_bridge(&address).map_err(incus)?;
                }
                vms.bridge_name()
            }
        };
        let (gateway, subnet) = bridge_address(&socket, project, &network)?;
        let mut closed = vec![subnet];
        if let Some(service) = plan.service_cidr.as_ref().filter(|cidr| **cidr != closed[0]) {
            closed.push(service.clone());
        }
        let egress = job_rules(&socket, gateway, &closed)?;
        // Nothing inbound: no ingress rule, and the card's default rejects the rest.
        let acl = vms.ensure_acl(&[], &egress).map_err(incus)?;
        Ok(Self {
            vms,
            socket,
            source,
            profiles,
            storage_pool,
            network,
            gateway,
            closed,
            acl,
            runs: Mutex::new(BTreeMap::new()),
        })
    }

    /// The bridge the pool's machines are on: the one its `network` names, or its own.
    pub fn bridge(&self) -> &str {
        &self.network
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
        // The host's addresses as they are now: an address it gained since the last machine is rejected from this one.
        let egress = job_rules(&self.socket, self.gateway, &self.closed)?;
        self.vms.ensure_acl(&[], &egress).map_err(incus)?;
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

fn plain(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 63
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// The gateway and the subnet (`a.b.c.d/n`, at its real prefix) of the bridge `network`, read as `project` sees it.
fn bridge_address(socket: &Path, project: &str, network: &str) -> Result<(Ipv4Addr, String), LaunchError> {
    if !plain(network) || !plain(project) {
        return Err(LaunchError(
            "the pool's network or project is not a plain name".to_string(),
        ));
    }
    let read = UnixIncus::new(socket.to_path_buf());
    let bridge = read
        .get(&format!("/1.0/networks/{network}?project={project}"))
        .map_err(|error| LaunchError(format!("incus: the bridge {network}: {error}")))?;
    bridge
        .pointer("/config/ipv4.address")
        .and_then(Value::as_str)
        .and_then(gateway_and_subnet)
        .ok_or_else(|| {
            LaunchError(format!(
                "the bridge {network} has no IPv4 address: a job machine's rules are built from its gateway"
            ))
        })
}

/// The host's addresses, as the service plugin reads them.
fn host_addresses(read: &UnixIncus) -> Result<Value, LaunchError> {
    arca_providers::incus::host_networks(read)
        .map(|networks| arca_providers::incus::host_addresses_value(&networks))
        .map_err(|error| LaunchError(format!("incus: the host's addresses: {error:?}")))
}

/// The egress rules of the pool's job machines: the rules of a tenant's service with `closed` rejected, from the
/// bridge's gateway and the host's addresses as they are now.
fn job_rules(socket: &Path, gateway: Ipv4Addr, closed: &[String]) -> Result<Vec<Value>, LaunchError> {
    let hosts = host_addresses(&UnixIncus::new(socket.to_path_buf()))?;
    Ok(arca_netproject::tenant::job_egress_rules(gateway, closed, &hosts))
}

/// Every range the pool's own bridge must keep clear of: the service subnet, the reserved ranges, every Incus network's
/// IPv4 subnet in any project, and every host network.
fn taken(socket: &Path, plan: &NetworkPlan) -> Result<Vec<String>, LaunchError> {
    let read = UnixIncus::new(socket.to_path_buf());
    let mut taken: Vec<String> = plan.service_cidr.iter().cloned().collect();
    taken.extend(plan.reserved.iter().cloned());
    let networks = read
        .get("/1.0/networks?all-projects=true&recursion=1")
        .map_err(|error| LaunchError(format!("incus: the networks: {error}")))?;
    taken.extend(
        networks
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|network| network.pointer("/config/ipv4.address")?.as_str())
            .filter_map(|address| Some(gateway_and_subnet(address)?.1)),
    );
    taken.extend(
        host_addresses(&read)?
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|address| !address.contains(':'))
            .map(str::to_string),
    );
    Ok(taken)
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

/// Deletes the ACL and the own bridge of every pool that is gone from the config: an ACL named `arca-runner-<pool>`
/// or a network named that pool's bridge name, in any project, carrying the runner mark with that pool's name, for a
/// pool not among `pools`. Each delete goes through that pool's own writer, which reads the mark again first; the ACL
/// goes before the bridge. Returns one line for each object removed or left, and one when a listing failed.
pub fn remove_gone_pools(socket: &Path, pools: &[&str]) -> Vec<String> {
    let read = UnixIncus::new(socket.to_path_buf());
    let mut lines = Vec::new();
    let mut gone = |listing: &str, kind: &str, expected: &dyn Fn(&RunnerVms) -> String| {
        let rows = match read.get(listing) {
            Ok(Value::Array(rows)) => rows,
            Ok(_) => return lines.push(format!("the {kind} listing is not a list")),
            Err(error) => return lines.push(format!("the {kind}s could not be listed: {error}")),
        };
        for row in rows {
            let Some(pool) = row.pointer(&format!("/config/{RUNNER_KEY}")).and_then(Value::as_str) else {
                continue;
            };
            let name = row.get("name").and_then(Value::as_str).unwrap_or_default();
            let project = row.get("project").and_then(Value::as_str).unwrap_or("default");
            if pools.contains(&pool) {
                continue;
            }
            let Ok(vms) = RunnerVms::new(socket.to_path_buf(), project, pool) else {
                continue;
            };
            if name != expected(&vms) {
                continue;
            }
            let removed = if kind == "network ACL" { vms.remove_acl() } else { vms.remove_bridge() };
            lines.push(match removed {
                Ok(()) => format!("pool {pool} is gone: its {kind} {name} was deleted"),
                Err(error) => format!("pool {pool} is gone, and its {kind} {name} stays: {error}"),
            });
        }
    };
    gone("/1.0/network-acls?all-projects=true&recursion=1", "network ACL", &|vms| vms.acl_name());
    gone("/1.0/networks?all-projects=true&recursion=1", "bridge", &|vms| vms.bridge_name());
    lines
}

fn incus(error: IncusError) -> LaunchError {
    LaunchError(format!("incus: {error}"))
}
