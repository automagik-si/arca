//! The host sample: its shape, the sampler that reads the host and Incus, the ring that keeps the recent samples, the window
//! mean, and the `node_metrics` result row. Field names are the camel-case keys of the `node_metrics` result schema.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use arca_incus::IncusRead;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::incus::{self, InstanceReading};
use crate::osd::{self, OsdReading, UpdateMemory};
use crate::probe::{self, ProbeTarget};
use crate::procfs::{self, CpuTotals, NicTotals};
use crate::smart::{self, DrivesReading};

/// The window `node_metrics` reads when the caller names no minutes.
pub const DEFAULT_MINUTES: u32 = 1;

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
}

/// One host sample. A reading the host cannot give is `null`, never a guessed zero, except `cpu.cores` (an integer in the
/// schema), which is 0 when `/proc/stat` cannot be read.
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
    pub pools: Vec<Pool>,
    /// Bytes per second over the physical NICs since the previous sample. `None` on the first sample.
    pub network: Option<Network>,
    /// The instances per project. A project with no instance is absent, which the mean reads as zero.
    pub projects: Vec<ProjectCount>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cpu {
    /// The share of all CPUs busy since the previous sample, in percent.
    pub used_pct: Option<f64>,
    pub cores: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Load {
    pub one: Option<f64>,
    pub five: Option<f64>,
    pub fifteen: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Memory {
    pub total_bytes: Option<u64>,
    pub available_bytes: Option<u64>,
    pub swap_total_bytes: Option<u64>,
    pub swap_free_bytes: Option<u64>,
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
}

/// What the sampler keeps of one instance between ticks: its `last_used_at` on the previous listing, its starts since the
/// process began, and the times of the starts inside [`START_WINDOW_MS`].
#[derive(Debug, Clone)]
struct StartTrack {
    last_used: Option<String>,
    starts_total: u64,
    starts: VecDeque<u64>,
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
}

impl<R: IncusRead> Sampler<R> {
    pub fn new(cfg: SamplerConfig, incus: R) -> Self {
        let capacity = ring_capacity(&cfg);
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
        }
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
    pub fn sample(&mut self, now_ms: u64) -> &HostSample {
        let previous = self.previous;
        let cpu = procfs::read_cpu(&self.cfg.proc_root);
        let nic = procfs::read_nic_totals(&self.cfg.proc_root, &self.cfg.sys_root);
        let pools = incus::read_pools(&self.incus, &self.cfg.pools);
        let instances =
            incus::read_instances(&self.incus).map(|listed| self.count_starts(listed, now_ms));
        let projects = instances.as_deref().map(incus::project_counts);
        let latency = if self.probe_due(now_ms) {
            Some(self.probe_round(now_ms))
        } else {
            self.readings.latency.clone()
        };
        let osd = if self.osd_due(now_ms) {
            self.osd_read_at = Some(now_ms);
            let uptime_ms = procfs::read_uptime_ms(&self.cfg.proc_root);
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
            cpu: Cpu {
                used_pct: previous
                    .and_then(|before| before.cpu)
                    .zip(cpu.totals)
                    .and_then(|(before, now)| used_pct(before, now)),
                cores: cpu.cores,
            },
            load: procfs::read_load(&self.cfg.proc_root),
            memory: procfs::read_memory(&self.cfg.proc_root),
            psi: procfs::read_psi(&self.cfg.proc_root),
            cpu_temp_c: procfs::read_cpu_temp_c(&self.cfg.sys_root),
            gpu_busy_pct: procfs::read_gpu_busy_pct(&self.cfg.sys_root),
            pools: rows,
            network: previous.and_then(|before| network_rates(before, nic, now_ms)),
            projects: projects.clone().unwrap_or_default(),
        };
        self.readings = Readings {
            pools,
            projects,
            nic: nic.map(|totals| NicCounters {
                rx_bytes: totals.rx_bytes,
                tx_bytes: totals.tx_bytes,
            }),
            instances,
            latency,
            osd,
            drives,
        };
        self.previous = Some(Previous {
            at: now_ms,
            cpu: cpu.totals,
            net: nic,
        });
        self.ring.push_back(sample);
        while self.ring.len() > self.capacity {
            self.ring.pop_front();
        }
        self.ring
            .back()
            .expect("the sample just pushed is in the ring")
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
/// as zero there. `at` and `cpu.cores` come from the last sample.
pub fn mean(samples: &[HostSample]) -> Option<HostSample> {
    let last = samples.last()?;
    Some(HostSample {
        at: last.at,
        cpu: Cpu {
            used_pct: average(samples.iter().filter_map(|s| s.cpu.used_pct)),
            cores: last.cpu.cores,
        },
        load: Load {
            one: average(samples.iter().filter_map(|s| s.load.one)),
            five: average(samples.iter().filter_map(|s| s.load.five)),
            fifteen: average(samples.iter().filter_map(|s| s.load.fifteen)),
        },
        memory: Memory {
            total_bytes: average_whole(samples.iter().filter_map(|s| s.memory.total_bytes)),
            available_bytes: average_whole(samples.iter().filter_map(|s| s.memory.available_bytes)),
            swap_total_bytes: average_whole(
                samples.iter().filter_map(|s| s.memory.swap_total_bytes),
            ),
            swap_free_bytes: average_whole(samples.iter().filter_map(|s| s.memory.swap_free_bytes)),
        },
        psi: Psi {
            memory_some_avg10: average(samples.iter().filter_map(|s| s.psi.memory_some_avg10)),
            memory_full_avg10: average(samples.iter().filter_map(|s| s.psi.memory_full_avg10)),
            cpu_some_avg10: average(samples.iter().filter_map(|s| s.psi.cpu_some_avg10)),
        },
        cpu_temp_c: average(samples.iter().filter_map(|s| s.cpu_temp_c)),
        gpu_busy_pct: average(samples.iter().filter_map(|s| s.gpu_busy_pct)),
        pools: mean_pools(samples),
        network: mean_network(samples),
        projects: mean_projects(samples),
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
