//! The host sample: its shape, the sampler that reads the host and Incus, the ring that keeps the recent samples, the window
//! mean, and the `node_metrics` result row. Field names are the camel-case keys of the `node_metrics` result schema.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use arca_incus::IncusRead;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::gpu::{
    self, GpuCard, GpuCardSample, GpuSection, INVENTORY_REASON, InstanceKey, NO_READING_YET,
    NOT_MEASURABLE, Nvidia, NvidiaReading, NvidiaSmi, mean_gpu,
};
use crate::incus::{self, InstanceReading};
use crate::mode::{HostModeChoice, MODE_PROBE_TIMEOUT, ModeRetry, choose_host_mode_within};
use crate::osd::{self, OsdReading, UpdateMemory};
use crate::probe::{self, ProbeTarget};
use crate::procfs::{self, CpuTotals, NicTotals};
use crate::smart::{self, DrivesReading};

/// The window `node_metrics` reads when the caller names no minutes.
pub const DEFAULT_MINUTES: u32 = 1;

/// The reason a local `/proc` and `/sys` reading is absent: it would describe the machine Arca runs on, not the remote
/// machine the node's Incus serves.
pub const REMOTE_HOST_REASON: &str = "not available for a remote host";

/// The reason the host totals are absent: the node's Incus did not answer `GET /1.0/resources`.
pub const RESOURCES_REASON: &str = "the Incus resources read failed";

/// How long the socket-path fallback stands as provisional, in milliseconds: 10 minutes after the first provisional
/// sample. Past it the sampler settles on the fallback for alerts, so a remote reading is treated as never available and
/// its alerts can resolve with no data. The sampler keeps probing `/1.0` at [`MODE_PROBE_EVERY_MS`] until a real name
/// decides the mode.
pub const PROVISIONAL_WINDOW_MS: u64 = 600_000;

/// How often the sampler probes `/1.0` after the provisional window ends, in milliseconds. A late Incus can still correct
/// the fallback; the first real name switches the mode and ends the probing.
pub const MODE_PROBE_EVERY_MS: u64 = 60_000;

/// A monotonic millisecond clock. The sampler measures the provisional window and its slow retry cadence with it, so a
/// wall-clock step cannot settle the mode early. Tests inject a clock they advance by hand.
pub trait MonotonicClock: Send + Sync {
    /// Milliseconds since an arbitrary origin. The value never goes backwards.
    fn now_ms(&self) -> u64;
}

/// The process's monotonic clock, in milliseconds since the sampler was built.
struct SystemClock(Instant);

impl SystemClock {
    fn new() -> SystemClock {
        SystemClock(Instant::now())
    }
}

impl MonotonicClock for SystemClock {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.0.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

/// The window `starts_10m` counts, in milliseconds: a start counts while it is less than this old.
const START_WINDOW_MS: u64 = 600_000;

/// How often a latency probe round runs, in milliseconds.
const PROBE_EVERY_MS: u64 = 30_000;

/// How long one latency probe may take to connect before it reads `None`.
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// How often the incus-osd security and update documents are read, in milliseconds.
const OSD_EVERY_MS: u64 = 300_000;

/// How often the incus-osd storage document is read for SMART, in milliseconds.
const SMART_EVERY_MS: u64 = 600_000;

/// Where the sampler reads, which pools it reports, and how many samples it keeps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SamplerConfig {
    /// The root of the procfs tree (`/proc` on a host).
    pub proc_root: PathBuf,
    /// The root of the sysfs tree (`/sys` on a host).
    pub sys_root: PathBuf,
    /// The allow-filter over the pools Incus lists, by name. An empty list admits every pool Incus lists. A name Incus does
    /// not list has no row.
    pub pools: Vec<String>,
    /// How often the caller samples, in milliseconds. The ring holds `history_minutes` of samples at this period.
    pub interval_ms: u64,
    /// How many minutes of samples the ring keeps.
    pub history_minutes: u32,
    /// Whether the node's Incus is the local daemon. When true, the host totals and the CPU load are read from the local
    /// `proc_root` and `sys_root`. When false, the totals come from the node's Incus `GET /1.0/resources`, and every local
    /// reading is left out: it would describe the machine Arca runs on, not the remote machine Incus serves.
    pub incus_local: bool,
    /// The explicit config value and socket-path rule to retry a provisional mode. `None` keeps `incus_local` fixed.
    pub mode_retry: Option<ModeRetry>,
}

/// One host sample of the machine the node's Incus serves. A reading the host cannot give is `null`, never a guessed zero,
/// except `cpu.cores` (an integer in the schema), which is 0 when the count is unknown. A remote host reports no local
/// `/proc` or `/sys` reading; the reason fields name why the figures are absent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostSample {
    /// Epoch milliseconds when the sample was taken.
    pub at: u64,
    pub cpu: Cpu,
    pub load: Load,
    pub memory: Memory,
    pub psi: Psi,
    pub cpu_temp_c: Option<f64>,
    pub gpu_busy_pct: Option<f64>,
    /// The GPU inventory and telemetry, keyed by PCI address. A remote host reads the inventory and leaves the telemetry
    /// absent with a reason.
    pub gpu: GpuSection,
    pub pools: Vec<Pool>,
    /// Bytes per second over the physical NICs since the previous sample. `None` on the first sample.
    pub network: Option<Network>,
    /// The instances per project. A project with no instance is absent, which the mean reads as zero.
    pub projects: Vec<ProjectCount>,
    /// The running instances whose CPU and memory this sample's tick did not read, and why. In the mean, the newest
    /// sample's.
    #[serde(default)]
    pub instances_not_read: Vec<NotRead>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cpu {
    /// The share of all CPUs busy since the previous sample, in percent.
    pub used_pct: Option<f64>,
    /// The logical CPUs of the managed host. It is 0 only when `cores_known` is false: the schema keeps it an integer, so
    /// a consumer must read `cores_known` before showing a core count.
    pub cores: u64,
    /// Whether `cores` is a real count. False when the host total could not be read; `cores` is then 0 as a placeholder.
    pub cores_known: bool,
    /// Why `used_pct`, and sometimes `cores`, is not known. `None` when a local host reading is simply not available yet.
    /// `Some("not available for a remote host")` on a remote host, and `Some("the Incus resources read failed")` when the
    /// host totals could not be read.
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Load {
    pub one: Option<f64>,
    pub five: Option<f64>,
    pub fifteen: Option<f64>,
    /// Why the load averages are null. `Some("not available for a remote host")` on a remote host, `None` otherwise.
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Memory {
    pub total_bytes: Option<u64>,
    /// The memory available to start applications. On a remote host this is `total_bytes - used_bytes`, the only figure
    /// the Incus host resources document supports.
    pub available_bytes: Option<u64>,
    /// The memory in use, in bytes. On a local host this is `total_bytes - available_bytes`; on a remote host it is the
    /// Incus resources `memory.used` figure.
    pub used_bytes: Option<u64>,
    pub swap_total_bytes: Option<u64>,
    pub swap_free_bytes: Option<u64>,
    /// Why a memory figure is not known. `Some("the Incus resources read failed")` when the host totals could not be read,
    /// `None` otherwise. Swap is never read from a remote host, so its figures stay null without a separate reason.
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Psi {
    pub memory_some_avg10: Option<f64>,
    pub memory_full_avg10: Option<f64>,
    pub cpu_some_avg10: Option<f64>,
}

/// A storage pool's space. `free_bytes` is `total_bytes - used_bytes`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pool {
    pub name: String,
    pub used_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
}

impl Pool {
    /// A pool row whose space is not known: the name, and null fields.
    fn unread(name: String) -> Pool {
        Pool {
            name,
            used_bytes: None,
            total_bytes: None,
            free_bytes: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Network {
    pub rx_bytes_per_sec: f64,
    pub tx_bytes_per_sec: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectCount {
    pub project: String,
    pub total: u64,
    pub running: u64,
}

/// The NIC byte counters of the physical NICs, as the sampler read them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NicCounters {
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

/// One latency probe round: the time it ran, and each named target's TCP connect time in seconds, or `None` when its probe
/// failed. A target that is not named has no entry.
#[derive(Debug, Clone, PartialEq)]
pub struct LatencyReading {
    /// Epoch milliseconds when the round ran.
    pub at: u64,
    pub targets: Vec<(ProbeTarget, Option<f64>)>,
}

/// What Incus, the NICs and the latency probes answered at the latest sample. A `None` is a read that failed: the sampler never
/// turns a failure into an empty answer here. The sample's own `pools` and `projects` lists are the `node_metrics` view of the
/// same reads.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Readings {
    /// The pools the allow-filter admits, from Incus's own pool list, with each one's space. `None` when the listing failed.
    pub pools: Option<Vec<Pool>>,
    /// The instances per project, counted from `instances`. `None` when the instance list cannot be read.
    pub projects: Option<Vec<ProjectCount>>,
    /// The NIC byte counters, `None` when `/proc/net/dev` cannot be read.
    pub nic: Option<NicCounters>,
    /// The instances, from the same list GET as `projects`. `None` when the list cannot be read, which is unknown, never no
    /// instances.
    pub instances: Option<Vec<InstanceReading>>,
    /// The newest latency probe round. `None` before the first round, and always when no target is named.
    pub latency: Option<LatencyReading>,
    /// The incus-osd reading, taken every 5 min. `None` when either document cannot be read, as on a plain Incus that answers
    /// 404, and before the first read.
    pub osd: Option<OsdReading>,
    /// The SMART state of the drives with SMART enabled, taken every 10 min. `None` when the storage document cannot be read, as
    /// on a plain Incus that answers 404, and before the first read.
    pub drives: Option<DrivesReading>,
    /// The cards that were in a successful inventory and are gone from the newest one, keyed by PCI address. Empty when the
    /// newest inventory still holds every card. It is not cleared by a failed inventory read: the alert source is unknown
    /// then, so a firing disappearance alert is held, never resolved by an outage.
    pub gpus_gone: Vec<GpuCard>,
}

/// What the sampler keeps of one instance between ticks: its `last_used_at` on the previous listing, its starts since the
/// process began, and the times of the starts inside [`START_WINDOW_MS`].
#[derive(Debug, Clone)]
struct StartTrack {
    last_used: Option<String>,
    starts_total: u64,
    starts: VecDeque<u64>,
}

/// A running instance whose usage a tick did not read, and why, in fixed words ([`incus::NOT_ANSWERED`],
/// [`incus::ANSWERED_AN_ERROR`], [`incus::NO_FIGURES`] or [`incus::BUDGET_SPENT`]). Its row is still counted, with its CPU and memory `null`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotRead {
    pub project: String,
    pub name: String,
    pub why: String,
}

/// The counters a sample is measured against: when they were read, and the CPU and NIC totals.
#[derive(Debug, Clone, Copy)]
struct Previous {
    at: u64,
    cpu: Option<CpuTotals>,
    net: Option<NicTotals>,
}

/// Samples the host and Incus, and keeps a ring of the most recent samples.
pub struct Sampler<R: IncusRead> {
    cfg: SamplerConfig,
    incus: R,
    capacity: usize,
    ring: VecDeque<HostSample>,
    previous: Option<Previous>,
    readings: Readings,
    /// The pool names the last successful listing admitted, in its order. `None` until a listing has succeeded.
    listed_pools: Option<Vec<String>>,
    /// Each instance the last successful instance listing held, by project and name.
    starts: BTreeMap<(String, String), StartTrack>,
    /// The latency targets, each with its TCP address. Empty until [`Sampler::with_probes`] names one.
    probes: Vec<(ProbeTarget, SocketAddr)>,
    /// When the last probe round ran, in epoch milliseconds. `None` until the first round.
    probed_at: Option<u64>,
    /// When the incus-osd documents were last read, in epoch milliseconds. `None` until the first read.
    osd_read_at: Option<u64>,
    /// The update result and last success time carried between incus-osd reads, so an in-progress or skipped status keeps the
    /// previous result. A failed read leaves it as it was.
    osd_memory: UpdateMemory,
    /// When the incus-osd storage document was last read for SMART, in epoch milliseconds. `None` until the first read.
    smart_read_at: Option<u64>,
    /// How to retry a provisional mode. Cleared once a server-name answer decides it.
    mode_retry: Option<ModeRetry>,
    /// When the first provisional sample started the window, on the monotonic clock. `None` before the first sample and
    /// after the window ends.
    provisional_since: Option<u64>,
    /// Whether the provisional window has ended and the fallback is settled for alerts. A slow `/1.0` probe still runs
    /// until a real name decides the mode.
    provisional_settled: bool,
    /// When the last `/1.0` mode probe ran, on the monotonic clock. `None` until the first probe.
    last_mode_probe: Option<u64>,
    /// The monotonic clock that measures the window and the probe cadence.
    clock: Arc<dyn MonotonicClock>,
    /// The choice `arca_status` reads, when the server shares it. A provisional choice is replaced here when a retry
    /// succeeds or the window ends. `None` for a sampler a test builds directly.
    mode_choice: Option<Arc<Mutex<HostModeChoice>>>,
    /// The tool the GPU telemetry and attribution read. The real `nvidia-smi` unless a test supplies a fake.
    nvidia: Arc<dyn Nvidia>,
    /// Every card a successful inventory has held, by PCI address. A card that disappears stays here, so the disappearance
    /// is reported; two identical cards are told apart by this address alone.
    gpu_known: BTreeMap<String, GpuCard>,
    /// The cards gone from the newest successful inventory, by PCI address.
    gpu_gone: Vec<GpuCard>,
    /// The ticks counted since the process began; the first is 1.
    ticks: u64,
    /// The tick at which each running instance's state was last asked, by project and name: a read that answered, or
    /// that ran out its own deadline, never one the tick's budget cut short. A tick starts with those asked longest ago
    /// (never first), so a node with more slow machines than one tick's budget still reads each of them within a few
    /// ticks.
    state_tried: BTreeMap<(String, String), u64>,
    /// The running instances the newest tick did not read the usage of.
    not_read: Vec<NotRead>,
    /// The deadline of one state read, and the time a tick gives them all.
    state_deadline: Duration,
    state_budget: Duration,
}

impl<R: IncusRead> Sampler<R> {
    pub fn new(cfg: SamplerConfig, incus: R) -> Self {
        let capacity = ring_capacity(&cfg);
        let mode_retry = cfg.mode_retry.clone();
        Self {
            cfg,
            incus,
            capacity,
            ring: VecDeque::new(),
            previous: None,
            readings: Readings::default(),
            listed_pools: None,
            starts: BTreeMap::new(),
            probes: Vec::new(),
            probed_at: None,
            osd_read_at: None,
            osd_memory: UpdateMemory::default(),
            smart_read_at: None,
            mode_retry,
            provisional_since: None,
            provisional_settled: false,
            last_mode_probe: None,
            clock: Arc::new(SystemClock::new()),
            mode_choice: None,
            nvidia: Arc::new(NvidiaSmi::new()),
            gpu_known: BTreeMap::new(),
            gpu_gone: Vec::new(),
            ticks: 0,
            state_tried: BTreeMap::new(),
            not_read: Vec::new(),
            state_deadline: incus::STATE_DEADLINE,
            state_budget: incus::STATE_BUDGET,
        }
    }

    /// Uses `deadline` for each instance's state read and `budget` for a tick's reads together, instead of
    /// [`incus::STATE_DEADLINE`] and [`incus::STATE_BUDGET`]. A test shortens them; a build never needs to.
    pub fn with_state_reads(mut self, deadline: Duration, budget: Duration) -> Self {
        self.state_deadline = deadline;
        self.state_budget = budget;
        self
    }

    /// The running instances the newest tick did not read the usage of, and why.
    pub fn instances_not_read(&self) -> &[NotRead] {
        &self.not_read
    }

    /// Reads the usage of each running instance from its own state, one after another, the ones asked longest ago first,
    /// each under the state deadline and all under the tick's budget. One that does not answer, or that the budget does
    /// not reach, keeps its CPU and memory `None` for this tick and is named in [`Sampler::instances_not_read`].
    fn read_usages(&mut self, instances: &mut [InstanceReading]) {
        self.ticks += 1;
        let tick = self.ticks;
        let key = |instance: &InstanceReading| (instance.project.clone(), instance.name.clone());
        let held: BTreeSet<(String, String)> = instances.iter().map(key).collect();
        self.state_tried.retain(|known, _| held.contains(known));
        let mut order: Vec<usize> = (0..instances.len())
            .filter(|&index| instances[index].running)
            .collect();
        order.sort_by_key(|&index| {
            let instance = &instances[index];
            (
                self.state_tried.get(&key(instance)).copied().unwrap_or(0),
                key(instance),
            )
        });
        let began = Instant::now();
        let mut not_read = Vec::new();
        for index in order {
            let instance = &mut instances[index];
            let left = self.state_budget.saturating_sub(began.elapsed());
            let outcome = if left.is_zero() {
                Err(incus::BUDGET_SPENT)
            } else {
                incus::read_usage(
                    &self.incus,
                    &instance.project,
                    &instance.name,
                    left.min(self.state_deadline),
                    self.state_deadline,
                )
            };
            // A read the budget cut short does not count as asked: the instance keeps its older turn and goes first
            // next tick, with the whole deadline. A read that ran its own deadline out does count, so a machine whose
            // agent never answers does not hold the first place on every tick.
            if outcome != Err(incus::BUDGET_SPENT) {
                self.state_tried.insert(key(instance), tick);
            }
            match outcome {
                Ok((Some(cpu_seconds), Some(memory_bytes))) => {
                    instance.cpu_seconds = Some(cpu_seconds);
                    instance.memory_bytes = Some(memory_bytes);
                }
                Ok((cpu_seconds, memory_bytes)) => {
                    // An answer without one of its figures: what it has is kept, and the instance is named.
                    instance.cpu_seconds = cpu_seconds;
                    instance.memory_bytes = memory_bytes;
                    not_read.push(NotRead {
                        project: instance.project.clone(),
                        name: instance.name.clone(),
                        why: incus::NO_FIGURES.to_string(),
                    });
                }
                Err(why) => {
                    instance.cpu_seconds = None;
                    instance.memory_bytes = None;
                    not_read.push(NotRead {
                        project: instance.project.clone(),
                        name: instance.name.clone(),
                        why: why.to_string(),
                    });
                }
            }
        }
        not_read.sort_by(|a, b| (&a.project, &a.name).cmp(&(&b.project, &b.name)));
        self.not_read = not_read;
    }

    /// Shares the startup mode choice with `arca_status`, so a retry that succeeds replaces the provisional answer there too.
    pub fn with_mode_choice(mut self, choice: Arc<Mutex<HostModeChoice>>) -> Self {
        self.mode_choice = Some(choice);
        self
    }

    /// Uses `nvidia` for the local GPU telemetry and attribution, instead of the host's `nvidia-smi`. A test supplies a
    /// fake; a build never needs to.
    pub fn with_nvidia(mut self, nvidia: Arc<dyn Nvidia>) -> Self {
        self.nvidia = nvidia;
        self
    }

    /// Uses `clock` for the provisional window and the slow retry cadence, instead of the process's monotonic clock.
    pub fn with_clock(mut self, clock: Arc<dyn MonotonicClock>) -> Self {
        self.clock = clock;
        self
    }

    /// Names the latency targets to probe, each with its TCP address. A sampler with no target never probes, and a target that
    /// is not listed has no entry in [`Readings::latency`].
    pub fn with_probes(mut self, probes: Vec<(ProbeTarget, SocketAddr)>) -> Self {
        self.probes = probes;
        self
    }

    /// Reads the host and Incus, appends the sample to the ring, and returns it. The ring drops its oldest sample past its
    /// capacity. A latency probe round runs in the same call when one is due, so a round is at most 30 s old at its sample.
    ///
    /// When the pool listing fails, the sample's pool rows are the last listing's names with null fields, or, before any
    /// listing has succeeded, the allow-filter's names with null fields. When the instance list fails, the sample has no project
    /// rows. [`Sampler::readings`] keeps the `None`s, which the alerts read as unknown.
    ///
    /// A remote host's totals are read from its Incus every tick; a failed read is null with [`RESOURCES_REASON`], never the
    /// local machine's numbers. A local host keeps reading its own procfs and sysfs.
    pub fn sample(&mut self, now_ms: u64) -> &HostSample {
        self.retry_mode();
        let previous = self.previous;
        let host = read_host(&self.cfg, &self.incus, previous, now_ms);
        let pools = incus::read_pools(&self.incus, &self.cfg.pools);
        let instances = incus::read_instances(&self.incus).map(|listed| {
            let mut instances = self.count_starts(listed, now_ms);
            self.read_usages(&mut instances);
            instances
        });
        if instances.is_none() {
            self.not_read.clear();
        }
        let projects = instances.as_deref().map(incus::project_counts);
        let gpu = self.read_gpu(instances.as_deref());
        let latency = if self.probe_due(now_ms) {
            Some(self.probe_round(now_ms))
        } else {
            self.readings.latency.clone()
        };
        let osd = if self.osd_due(now_ms) {
            self.osd_read_at = Some(now_ms);
            // The uptime is a local reading, so it says nothing about a remote incus-osd's update age.
            let uptime_ms = self
                .cfg
                .incus_local
                .then(|| procfs::read_uptime_ms(&self.cfg.proc_root))
                .flatten();
            osd::read(&self.incus, now_ms, &mut self.osd_memory, uptime_ms)
        } else {
            self.readings.osd.clone()
        };
        let drives = if self.smart_due(now_ms) {
            self.smart_read_at = Some(now_ms);
            smart::read(&self.incus, now_ms)
        } else {
            self.readings.drives.clone()
        };
        let rows = match &pools {
            Some(listed) => {
                self.listed_pools = Some(listed.iter().map(|pool| pool.name.clone()).collect());
                listed.clone()
            }
            None => self
                .listed_pools
                .clone()
                .unwrap_or_else(|| self.cfg.pools.clone())
                .into_iter()
                .map(Pool::unread)
                .collect(),
        };
        let sample = HostSample {
            at: now_ms,
            cpu: host.cpu,
            load: host.load,
            memory: host.memory,
            psi: host.psi,
            cpu_temp_c: host.cpu_temp_c,
            gpu_busy_pct: host.gpu_busy_pct,
            gpu,
            pools: rows,
            network: host.network,
            projects: projects.clone().unwrap_or_default(),
            instances_not_read: self.not_read.clone(),
        };
        self.readings = Readings {
            pools,
            projects,
            nic: host.nic.map(|totals| NicCounters {
                rx_bytes: totals.rx_bytes,
                tx_bytes: totals.tx_bytes,
            }),
            instances,
            latency,
            osd,
            drives,
            gpus_gone: self.gpu_gone.clone(),
        };
        self.previous = Some(Previous {
            at: now_ms,
            cpu: host.cpu_totals,
            net: host.nic,
        });
        self.ring.push_back(sample);
        while self.ring.len() > self.capacity {
            self.ring.pop_front();
        }
        self.ring
            .back()
            .expect("the sample just pushed is in the ring")
    }

    /// Reads the GPU inventory through Incus and, on a local host only, the newest telemetry and process attribution.
    ///
    /// The inventory is read every tick, for a local and a remote host alike, and is keyed by the PCI address. A card that
    /// was in an earlier successful inventory and is gone from this one is kept in `gpu_gone` until it returns, so the
    /// disappearance is a reading rather than a gap. A failed inventory read leaves the known cards alone and reports the
    /// reason; a card is never invented and a missing read is never an empty inventory.
    ///
    /// A remote host's telemetry and attribution are absent with [`NOT_MEASURABLE`]: a local `nvidia-smi` would describe the
    /// wrong machine. A local host without the tool reports [`gpu::NVIDIA_MISSING`], and a failed read [`gpu::NVIDIA_FAILED`];
    /// a card the tool does not list reads [`NO_READING_YET`]. None of them is a zero.
    fn read_gpu(&mut self, instances: Option<&[InstanceReading]>) -> GpuSection {
        let Some(inventory) = gpu::read_inventory(&self.incus) else {
            return GpuSection {
                reason: Some(INVENTORY_REASON.to_string()),
                skipped: None,
                cards: Vec::new(),
            };
        };
        let cards = inventory.cards;
        let current: BTreeMap<String, GpuCard> = cards
            .iter()
            .cloned()
            .map(|card| (card.pci.clone(), card))
            .collect();
        for card in current.values() {
            self.gpu_known.insert(card.pci.clone(), card.clone());
        }
        self.gpu_gone = self
            .gpu_known
            .values()
            .filter(|card| !current.contains_key(&card.pci))
            .cloned()
            .collect();
        let (readings, reading_reason) = if self.cfg.incus_local {
            match self.nvidia.readings() {
                Ok(readings) => (Some(readings), None),
                Err(error) => (None, Some(nvidia_reason(&error))),
            }
        } else {
            (None, Some(NOT_MEASURABLE.to_string()))
        };
        let readings: Option<BTreeMap<String, NvidiaReading>> = readings.map(|readings| {
            readings
                .into_iter()
                .map(|reading| (reading.pci.clone(), reading))
                .collect()
        });
        let (processes, process_reason) = if self.cfg.incus_local {
            match self.nvidia.processes() {
                Ok(processes) => {
                    let keys: Vec<InstanceKey> = instances
                        .unwrap_or_default()
                        .iter()
                        .map(|instance| InstanceKey {
                            project: instance.project.clone(),
                            name: instance.name.clone(),
                        })
                        .collect();
                    (
                        Some(gpu::attribute(&self.cfg.proc_root, &processes, &keys)),
                        None,
                    )
                }
                Err(error) => (None, Some(nvidia_reason(&error))),
            }
        } else {
            (None, Some(NOT_MEASURABLE.to_string()))
        };
        let cards = current
            .values()
            .map(|card| {
                let mut sample: GpuCardSample = card.clone().into();
                match (
                    &reading_reason,
                    readings.as_ref().and_then(|map| map.get(&card.pci)),
                ) {
                    (Some(reason), _) => sample.reason = Some(reason.clone()),
                    (None, Some(reading)) => {
                        sample.reason = None;
                        sample.uuid = sample.uuid.clone().or_else(|| reading.uuid.clone());
                        sample.vram_total_bytes = sample
                            .vram_total_bytes
                            .or(reading.telemetry.memory_total_bytes);
                        sample.telemetry = Some(reading.telemetry.clone());
                    }
                    (None, None) => sample.reason = Some(NO_READING_YET.to_string()),
                }
                match &process_reason {
                    Some(reason) => sample.process_reason = Some(reason.clone()),
                    None => {
                        sample.process_reason = None;
                        sample.processes = processes
                            .as_ref()
                            .map(|processes| {
                                processes
                                    .iter()
                                    .filter(|process| process.pci == card.pci)
                                    .cloned()
                                    .collect()
                            })
                            .unwrap_or_default();
                    }
                }
                sample
            })
            .collect();
        GpuSection {
            reason: None,
            skipped: inventory.skipped,
            cards,
        }
    }

    /// Retries a provisional mode's `/1.0` name check, under [`MODE_PROBE_TIMEOUT`]. Before [`PROVISIONAL_WINDOW_MS`] the
    /// check runs on every sample. Once the window ends the fallback is settled for alerts, but the check continues at
    /// [`MODE_PROBE_EVERY_MS`] until a real name decides the mode. A mode from `incus.local` or from a real name comparison
    /// is never provisional, so this does nothing for it. A switch between the local and remote modes clears the ring and
    /// the rate counters, so a mean never mixes the two sources and no rate spans them. The window and the cadence use the
    /// monotonic [`MonotonicClock`], so a wall-clock step cannot settle the mode early.
    fn retry_mode(&mut self) {
        let Some(retry) = self.mode_retry.clone() else {
            return;
        };
        let now = self.clock.now_ms();
        let mut window_ended = false;
        if let Some(since) = self.provisional_since {
            if now.saturating_sub(since) >= PROVISIONAL_WINDOW_MS {
                self.provisional_since = None;
                self.provisional_settled = true;
                window_ended = true;
            }
        } else if self.provisional_settled
            && self
                .last_mode_probe
                .is_some_and(|last| now.saturating_sub(last) < MODE_PROBE_EVERY_MS)
        {
            return;
        }
        let choice = choose_host_mode_within(
            &self.incus,
            retry.explicit,
            retry.path_is_local,
            &self.cfg.proc_root,
            MODE_PROBE_TIMEOUT,
        );
        self.last_mode_probe = Some(now);
        if choice.provisional {
            if self.provisional_since.is_none() && !self.provisional_settled {
                self.provisional_since = Some(now);
            } else if window_ended {
                // The fallback is settled for alerts, but it is not final: the slow probe below can still replace it
                // with the mode a real name decides.
                let mut settled = choice.clone();
                settled.provisional = false;
                if let Some(shared) = &self.mode_choice {
                    *shared.lock().unwrap_or_else(PoisonError::into_inner) = settled;
                }
                eprintln!(
                    "arca: metrics source {}: {}; the provisional window ended, still probing every {MODE_PROBE_EVERY_MS} ms",
                    choice.mode.as_str(),
                    choice.reason
                );
            }
            return;
        }
        let mode = choice.mode;
        let reason = choice.reason.clone();
        self.settle_mode(choice);
        eprintln!("arca: metrics source {}: {reason}", mode.as_str());
    }

    /// Keeps a real mode answer, clears the retry, and drops samples from the mode it replaced.
    fn settle_mode(&mut self, choice: HostModeChoice) {
        self.provisional_since = None;
        self.provisional_settled = false;
        self.last_mode_probe = None;
        self.mode_retry = None;
        let was_local = self.cfg.incus_local;
        self.cfg.incus_local = choice.is_local();
        if was_local != self.cfg.incus_local {
            self.ring.clear();
            self.previous = None;
        }
        if let Some(shared) = &self.mode_choice {
            *shared.lock().unwrap_or_else(PoisonError::into_inner) = choice;
        }
    }

    /// Counts each instance's starts against the previous listing and returns the readings with the counts filled in. A start is
    /// a change of `last_used_at` since the previous successful listing. An instance seen for the first time counts none. An
    /// instance this listing does not hold is forgotten, so an instance that returns is seen for the first time again, and a
    /// tick whose listing failed never reaches this method, so the outage keeps each count.
    fn count_starts(&mut self, listed: Vec<incus::Listed>, now_ms: u64) -> Vec<InstanceReading> {
        let mut earlier = std::mem::take(&mut self.starts);
        listed
            .into_iter()
            .map(|row| {
                let key = (row.reading.project.clone(), row.reading.name.clone());
                let mut track = match earlier.remove(&key) {
                    Some(mut track) => {
                        if track.last_used != row.last_used {
                            track.starts_total += 1;
                            track.starts.push_back(now_ms);
                        }
                        track
                    }
                    None => StartTrack {
                        last_used: row.last_used.clone(),
                        starts_total: 0,
                        starts: VecDeque::new(),
                    },
                };
                track.last_used = row.last_used;
                track
                    .starts
                    .retain(|at| now_ms.saturating_sub(*at) < START_WINDOW_MS);
                let reading = InstanceReading {
                    starts_total: track.starts_total,
                    starts_10m: track.starts.len() as u64,
                    ..row.reading
                };
                self.starts.insert(key, track);
                reading
            })
            .collect()
    }

    /// Whether a probe round is due: none has run yet, or 30 s have passed since the last one in either direction, so a clock
    /// that steps back does not stop the probes. A sampler with no target never probes.
    fn probe_due(&self, now_ms: u64) -> bool {
        if self.probes.is_empty() {
            return false;
        }
        self.probed_at
            .is_none_or(|at| now_ms.abs_diff(at) >= PROBE_EVERY_MS)
    }

    /// Whether the incus-osd documents are due: none has been read yet, or 5 min have passed since the last read in either
    /// direction, so a clock that steps back does not stop the reads.
    fn osd_due(&self, now_ms: u64) -> bool {
        self.osd_read_at
            .is_none_or(|at| now_ms.abs_diff(at) >= OSD_EVERY_MS)
    }

    /// Whether the incus-osd storage document is due for SMART: none has been read yet, or 10 min have passed since the last read
    /// in either direction, so a clock that steps back does not stop the reads.
    fn smart_due(&self, now_ms: u64) -> bool {
        self.smart_read_at
            .is_none_or(|at| now_ms.abs_diff(at) >= SMART_EVERY_MS)
    }

    /// Probes every named target at once, each with [`PROBE_TIMEOUT`], and returns the round. A probe whose thread cannot start,
    /// or panics, reads `None`, as a refused connect does.
    fn probe_round(&mut self, now_ms: u64) -> LatencyReading {
        self.probed_at = Some(now_ms);
        let probes = &self.probes;
        let targets = thread::scope(|scope| {
            let handles: Vec<_> = probes
                .iter()
                .map(|&(target, addr)| (target, spawn_probe(scope, addr)))
                .collect();
            handles
                .into_iter()
                .map(|(target, handle)| {
                    let seconds = handle.and_then(|handle| handle.join().ok()).flatten();
                    (target, seconds)
                })
                .collect()
        });
        LatencyReading {
            at: now_ms,
            targets,
        }
    }

    /// The most recent sample, or `None` before the first one.
    pub fn latest(&self) -> Option<&HostSample> {
        self.ring.back()
    }

    /// Whether the host mode is still the provisional socket-path fallback, which each sample retries for at most
    /// [`PROVISIONAL_WINDOW_MS`]. While it is provisional, a remote-shaped sample does not establish that this machine can
    /// never provide its own PSI or CPU temperature: the mode may still resolve local. Once the window ends, this is false
    /// and a remote reading is treated as never available, though a slow `/1.0` probe can still correct the mode.
    pub fn mode_provisional(&self) -> bool {
        self.mode_retry.is_some() && !self.provisional_settled
    }

    /// What Incus and the NICs answered at the latest sample. Before the first sample every field is `None`.
    pub fn readings(&self) -> &Readings {
        &self.readings
    }

    /// The samples from the last `minutes` before the latest sample, oldest first. The window is measured from the latest
    /// sample's time, not the wall clock, so a caller reads the same window whenever it asks.
    pub fn history(&self, minutes: u32) -> Vec<HostSample> {
        let Some(latest) = self.ring.back() else {
            return Vec::new();
        };
        let since = latest.at.saturating_sub(u64::from(minutes) * 60_000);
        self.ring
            .iter()
            .filter(|sample| sample.at >= since)
            .cloned()
            .collect()
    }
}

/// The host readings of one sample, and the local counters the next sample measures its rates against. On a remote host the
/// counters are absent, because no local reading is taken.
struct HostReadings {
    cpu: Cpu,
    load: Load,
    memory: Memory,
    psi: Psi,
    cpu_temp_c: Option<f64>,
    gpu_busy_pct: Option<f64>,
    network: Option<Network>,
    cpu_totals: Option<CpuTotals>,
    nic: Option<NicTotals>,
}

/// The host readings of one sample. A local Incus daemon's host is this machine, so the readings come from the local `/proc`
/// and `/sys` trees, as they always have. A remote daemon's host is another machine: its totals come from the daemon's
/// `GET /1.0/resources`, and every local reading is left null with its reason. A failed resources read is null too, never the
/// local machine's numbers.
fn read_host<R: IncusRead>(
    cfg: &SamplerConfig,
    incus: &R,
    previous: Option<Previous>,
    now_ms: u64,
) -> HostReadings {
    if cfg.incus_local {
        let cpu = procfs::read_cpu(&cfg.proc_root);
        let nic = procfs::read_nic_totals(&cfg.proc_root, &cfg.sys_root);
        return HostReadings {
            cpu: Cpu {
                used_pct: previous
                    .and_then(|before| before.cpu)
                    .zip(cpu.totals)
                    .and_then(|(before, now)| used_pct(before, now)),
                cores: cpu.cores,
                cores_known: cpu.cores > 0,
                reason: None,
            },
            load: procfs::read_load(&cfg.proc_root),
            memory: procfs::read_memory(&cfg.proc_root),
            psi: procfs::read_psi(&cfg.proc_root),
            cpu_temp_c: procfs::read_cpu_temp_c(&cfg.sys_root),
            gpu_busy_pct: procfs::read_gpu_busy_pct(&cfg.sys_root),
            network: previous.and_then(|before| network_rates(before, nic, now_ms)),
            cpu_totals: cpu.totals,
            nic,
        };
    }
    let load_reason = Some(REMOTE_HOST_REASON.to_string());
    match incus::read_host_totals(incus) {
        Some(totals) => HostReadings {
            cpu: Cpu {
                used_pct: None,
                cores: totals.cpu_threads,
                cores_known: true,
                reason: load_reason.clone(),
            },
            load: Load {
                one: None,
                five: None,
                fifteen: None,
                reason: load_reason,
            },
            memory: Memory {
                total_bytes: Some(totals.memory_total_bytes),
                available_bytes: Some(
                    totals
                        .memory_total_bytes
                        .saturating_sub(totals.memory_used_bytes),
                ),
                used_bytes: Some(totals.memory_used_bytes),
                swap_total_bytes: None,
                swap_free_bytes: None,
                reason: None,
            },
            psi: Psi {
                memory_some_avg10: None,
                memory_full_avg10: None,
                cpu_some_avg10: None,
            },
            cpu_temp_c: None,
            gpu_busy_pct: None,
            network: None,
            cpu_totals: None,
            nic: None,
        },
        None => HostReadings {
            cpu: Cpu {
                used_pct: None,
                cores: 0,
                cores_known: false,
                reason: Some(RESOURCES_REASON.to_string()),
            },
            load: Load {
                one: None,
                five: None,
                fifteen: None,
                reason: load_reason,
            },
            memory: Memory {
                total_bytes: None,
                available_bytes: None,
                used_bytes: None,
                swap_total_bytes: None,
                swap_free_bytes: None,
                reason: Some(RESOURCES_REASON.to_string()),
            },
            psi: Psi {
                memory_some_avg10: None,
                memory_full_avg10: None,
                cpu_some_avg10: None,
            },
            cpu_temp_c: None,
            gpu_busy_pct: None,
            network: None,
            cpu_totals: None,
            nic: None,
        },
    }
}

/// Why a local `nvidia-smi` read is absent, in the plain words a card carries.
fn nvidia_reason(error: &gpu::NvidiaError) -> String {
    match error {
        gpu::NvidiaError::Missing => gpu::NVIDIA_MISSING.to_string(),
        gpu::NvidiaError::TimedOut => gpu::NVIDIA_TIMEOUT_REASON.to_string(),
        gpu::NvidiaError::Busy => gpu::NVIDIA_BUSY_REASON.to_string(),
        gpu::NvidiaError::Failed(_) => gpu::NVIDIA_FAILED.to_string(),
    }
}

/// The `node_metrics` result row: the sample time, the window in minutes, the latest sample and the mean over the window.
/// Without minutes the window is [`DEFAULT_MINUTES`].
pub fn node_metrics(s: &Sampler<impl IncusRead>, minutes: Option<u32>, now_ms: u64) -> Value {
    let minutes = minutes.unwrap_or(DEFAULT_MINUTES);
    json!({
        "at": now_ms,
        "minutes": minutes,
        "latest": s.latest(),
        "mean": mean(&s.history(minutes)),
    })
}

/// The mean of a window of samples. `None` for an empty window. Each measurement averages the samples that hold it, and is
/// `None` when none does. Byte counts and project counts are rounded to whole numbers. A project absent from a sample counts
/// as zero there. `at` is the last sample's, `cpu.cores` the last known sample's, and a reason is the newest known sample's:
/// a failed sample never labels figures averaged from earlier successes.
pub fn mean(samples: &[HostSample]) -> Option<HostSample> {
    let last = samples.last()?;
    let known_cores = samples.iter().rev().find(|sample| sample.cpu.cores_known);
    Some(HostSample {
        at: last.at,
        cpu: Cpu {
            used_pct: average(samples.iter().filter_map(|s| s.cpu.used_pct)),
            cores: known_cores.map_or(last.cpu.cores, |sample| sample.cpu.cores),
            cores_known: known_cores.is_some(),
            reason: mean_reason(
                samples,
                |sample| sample.cpu.cores_known || sample.cpu.used_pct.is_some(),
                |sample| sample.cpu.reason.clone(),
            ),
        },
        load: Load {
            one: average(samples.iter().filter_map(|s| s.load.one)),
            five: average(samples.iter().filter_map(|s| s.load.five)),
            fifteen: average(samples.iter().filter_map(|s| s.load.fifteen)),
            reason: mean_reason(
                samples,
                |sample| {
                    sample.load.one.is_some()
                        || sample.load.five.is_some()
                        || sample.load.fifteen.is_some()
                },
                |sample| sample.load.reason.clone(),
            ),
        },
        memory: Memory {
            total_bytes: average_whole(samples.iter().filter_map(|s| s.memory.total_bytes)),
            available_bytes: average_whole(samples.iter().filter_map(|s| s.memory.available_bytes)),
            used_bytes: average_whole(samples.iter().filter_map(|s| s.memory.used_bytes)),
            swap_total_bytes: average_whole(
                samples.iter().filter_map(|s| s.memory.swap_total_bytes),
            ),
            swap_free_bytes: average_whole(samples.iter().filter_map(|s| s.memory.swap_free_bytes)),
            reason: mean_reason(
                samples,
                |sample| {
                    sample.memory.total_bytes.is_some()
                        || sample.memory.available_bytes.is_some()
                        || sample.memory.used_bytes.is_some()
                        || sample.memory.swap_total_bytes.is_some()
                        || sample.memory.swap_free_bytes.is_some()
                },
                |sample| sample.memory.reason.clone(),
            ),
        },
        psi: Psi {
            memory_some_avg10: average(samples.iter().filter_map(|s| s.psi.memory_some_avg10)),
            memory_full_avg10: average(samples.iter().filter_map(|s| s.psi.memory_full_avg10)),
            cpu_some_avg10: average(samples.iter().filter_map(|s| s.psi.cpu_some_avg10)),
        },
        cpu_temp_c: average(samples.iter().filter_map(|s| s.cpu_temp_c)),
        gpu_busy_pct: average(samples.iter().filter_map(|s| s.gpu_busy_pct)),
        gpu: mean_gpu(
            &samples.iter().map(|sample| &sample.gpu).collect::<Vec<_>>(),
            &last.gpu,
        ),
        pools: mean_pools(samples),
        network: mean_network(samples),
        projects: mean_projects(samples),
        instances_not_read: last.instances_not_read.clone(),
    })
}

/// The pools of the last sample, each averaged over the samples that name it.
fn mean_pools(samples: &[HostSample]) -> Vec<Pool> {
    let Some(last) = samples.last() else {
        return Vec::new();
    };
    last.pools
        .iter()
        .map(|pool| {
            let same: Vec<&Pool> = samples
                .iter()
                .flat_map(|s| s.pools.iter())
                .filter(|other| other.name == pool.name)
                .collect();
            Pool {
                name: pool.name.clone(),
                used_bytes: average_whole(same.iter().filter_map(|p| p.used_bytes)),
                total_bytes: average_whole(same.iter().filter_map(|p| p.total_bytes)),
                free_bytes: average_whole(same.iter().filter_map(|p| p.free_bytes)),
            }
        })
        .collect()
}

fn mean_network(samples: &[HostSample]) -> Option<Network> {
    let held: Vec<Network> = samples.iter().filter_map(|s| s.network).collect();
    Some(Network {
        rx_bytes_per_sec: average(held.iter().map(|n| n.rx_bytes_per_sec))?,
        tx_bytes_per_sec: average(held.iter().map(|n| n.tx_bytes_per_sec))?,
    })
}

/// Every project any sample names, sorted by name. Each count is averaged over all the samples, a sample without the
/// project counting zero.
fn mean_projects(samples: &[HostSample]) -> Vec<ProjectCount> {
    let names: BTreeSet<&str> = samples
        .iter()
        .flat_map(|s| s.projects.iter().map(|p| p.project.as_str()))
        .collect();
    let count = samples.len() as f64;
    names
        .into_iter()
        .map(|name| {
            let (total, running) =
                samples.iter().fold((0u64, 0u64), |(total, running), s| {
                    match s.projects.iter().find(|p| p.project == name) {
                        Some(p) => (total + p.total, running + p.running),
                        None => (total, running),
                    }
                });
            ProjectCount {
                project: name.to_string(),
                total: whole(total as f64 / count),
                running: whole(running as f64 / count),
            }
        })
        .collect()
}

/// The ring's capacity: `history_minutes` of samples at `interval_ms`, and at least one.
fn ring_capacity(cfg: &SamplerConfig) -> usize {
    let slots = u64::from(cfg.history_minutes) * 60_000 / cfg.interval_ms.max(1);
    usize::try_from(slots.max(1)).unwrap_or(usize::MAX)
}

/// The share of CPU time busy between two readings, in percent. `None` unless the total moved forward.
fn used_pct(before: CpuTotals, now: CpuTotals) -> Option<f64> {
    if now.total <= before.total {
        return None;
    }
    let busy = now.busy.saturating_sub(before.busy) as f64;
    let total = (now.total - before.total) as f64;
    Some(100.0 * busy / total)
}

/// The byte rates since the previous sample. A counter that moved backwards reads as zero. `None` unless both readings
/// exist and time moved forward.
fn network_rates(before: Previous, now: Option<NicTotals>, now_ms: u64) -> Option<Network> {
    let earlier = before.net?;
    let later = now?;
    let elapsed_ms = now_ms
        .checked_sub(before.at)
        .filter(|elapsed| *elapsed > 0)?;
    let seconds = elapsed_ms as f64 / 1000.0;
    Some(Network {
        rx_bytes_per_sec: later.rx_bytes.saturating_sub(earlier.rx_bytes) as f64 / seconds,
        tx_bytes_per_sec: later.tx_bytes.saturating_sub(earlier.tx_bytes) as f64 / seconds,
    })
}

/// Starts one probe on its own thread, or `None` when the thread cannot start.
fn spawn_probe<'scope, 'env>(
    scope: &'scope thread::Scope<'scope, 'env>,
    addr: SocketAddr,
) -> Option<thread::ScopedJoinHandle<'scope, Option<f64>>> {
    thread::Builder::new()
        .spawn_scoped(scope, move || {
            probe::tcp_connect_seconds(addr, PROBE_TIMEOUT)
        })
        .ok()
}

/// The reason of the newest sample whose reading was known, or the last sample's reason when none was. A failed sample's
/// reason therefore never labels figures averaged from earlier successes.
fn mean_reason(
    samples: &[HostSample],
    known: impl Fn(&HostSample) -> bool,
    reason: impl Fn(&HostSample) -> Option<String>,
) -> Option<String> {
    match samples.iter().rev().find(|sample| known(sample)) {
        Some(sample) => reason(sample),
        None => samples.last().and_then(reason),
    }
}

/// The mean of the finite values, or `None` when there are none.
fn average(values: impl Iterator<Item = f64>) -> Option<f64> {
    let (sum, count) = values.fold((0.0, 0usize), |(sum, count), value| {
        (sum + value, count + 1)
    });
    (count > 0).then(|| sum / count as f64)
}

/// The mean of whole numbers, rounded to the nearest whole number, or `None` when there are none.
fn average_whole(values: impl Iterator<Item = u64>) -> Option<u64> {
    average(values.map(|value| value as f64)).map(whole)
}

fn whole(value: f64) -> u64 {
    value.round() as u64
}
