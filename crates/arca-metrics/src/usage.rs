//! Each running instance's CPU in threads in use: the change of the CPU seconds the sampler reads from the instance's own
//! state, over the time between this read and the instance's previous one.
//!
//! A figure is only given across two reads of the same run of the same instance. An instance that was replaced (its
//! `created_at` changed), restarted (its `last_used_at` changed), was not seen running at its previous read, or whose
//! counter went backwards has no figure for that sample, never a negative or a huge one; a figure above the host's
//! threads is dropped as well. The list is bounded: at most [`INSTANCE_CPU_MAX`] rows, the largest figures first, and a
//! count and a sum of the rest.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// The most rows `instanceCpu.instances` holds. The rest are counted in `omitted` and summed in `omittedBusyThreads`.
pub const INSTANCE_CPU_MAX: usize = 50;

/// No figure: the instance was read once in this run, and a rate needs two reads.
pub const FIRST_READING: &str = "only one reading so far; a rate needs two";

/// No figure: the instance started or restarted between its previous read and this one.
pub const RESTARTED: &str = "it started or restarted since its previous reading";

/// No figure: the instance was deleted and made again under the same name since its previous read.
pub const REPLACED: &str = "it was replaced since its previous reading";

/// No figure: the CPU seconds are lower than at the previous read, with no restart seen.
pub const CPU_WENT_BACK: &str = "its CPU time went backwards";

/// No figure: this sample did not read the instance's state; `instancesNotRead` says why.
pub const NOT_READ_NOW: &str = "its CPU time was not read in this sample";

/// No figure: the two reads were taken at the same time.
pub const NO_TIME: &str = "no time passed since its previous reading";

/// No figure: the figure came out above the host's threads, which a real instance cannot use.
pub const ABOVE_HOST: &str = "the figure was more than the host's threads";

/// Why the list is empty: the instance list could not be read.
pub const LIST_FAILED: &str = "the instance list could not be read";

/// How far above the host's threads a figure may come out before it is dropped. Two reads are timed when each answers, so a
/// busy instance on every thread can read a little over the count.
const ABOVE_HOST_MARGIN: f64 = 1.05;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceCpu {
    /// The running instances, those with a figure first, largest first, then those without one by project and name. At
    /// most [`INSTANCE_CPU_MAX`] rows.
    pub instances: Vec<InstanceThreads>,
    /// The running instances left out past the cap.
    pub omitted: u64,
    /// The sum of the left-out instances' figures, in threads.
    pub omitted_busy_threads: f64,
    /// Why the list is empty: the instance list could not be read.
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceThreads {
    pub project: String,
    pub name: String,
    /// The threads in use between the instance's previous read and this one: 1.0 is one thread busy the whole time.
    pub busy_threads: Option<f64>,
    pub reason: Option<String>,
}

/// What the sampler keeps of one running instance between reads.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CpuBefore {
    at_ms: u64,
    seconds: f64,
    created: Option<String>,
    last_used: Option<String>,
}

/// One instance as this sample saw it.
pub(crate) struct Seen {
    pub project: String,
    pub name: String,
    pub running: bool,
    pub cpu_seconds: Option<f64>,
    /// When its state read answered, in epoch milliseconds.
    pub read_at_ms: u64,
    pub created: Option<String>,
    pub last_used: Option<String>,
}

/// The instance CPU block of one sample, and the reads the next sample measures against. An instance that is not running
/// is forgotten, so its next run starts with no figure.
pub(crate) fn measure(
    before: &mut BTreeMap<(String, String), CpuBefore>,
    seen: Vec<Seen>,
    host_threads: Option<u64>,
) -> InstanceCpu {
    let running: BTreeSet<(String, String)> = seen
        .iter()
        .filter(|instance| instance.running)
        .map(|instance| (instance.project.clone(), instance.name.clone()))
        .collect();
    before.retain(|key, _| running.contains(key));
    let mut rows = Vec::new();
    for instance in seen.into_iter().filter(|instance| instance.running) {
        let key = (instance.project.clone(), instance.name.clone());
        let changed = before.get(&key).and_then(|old| {
            if old.created != instance.created {
                Some(REPLACED)
            } else if old.last_used != instance.last_used {
                Some(RESTARTED)
            } else {
                None
            }
        });
        if changed.is_some() {
            before.remove(&key);
        }
        let (busy_threads, reason) = match instance.cpu_seconds {
            None => (None, Some(changed.unwrap_or(NOT_READ_NOW))),
            Some(seconds) => {
                let outcome = match (changed, before.get(&key)) {
                    (Some(why), _) => Err(why),
                    (None, None) => Err(FIRST_READING),
                    (None, Some(old)) => threads(old, seconds, instance.read_at_ms, host_threads),
                };
                before.insert(
                    key,
                    CpuBefore {
                        at_ms: instance.read_at_ms,
                        seconds,
                        created: instance.created,
                        last_used: instance.last_used,
                    },
                );
                match outcome {
                    Ok(figure) => (Some(figure), None),
                    Err(why) => (None, Some(why)),
                }
            }
        };
        rows.push(InstanceThreads {
            project: instance.project,
            name: instance.name,
            busy_threads,
            reason: reason.map(str::to_string),
        });
    }
    capped(rows)
}

/// The threads in use from the previous read to `seconds` read at `at_ms`.
fn threads(
    old: &CpuBefore,
    seconds: f64,
    at_ms: u64,
    host_threads: Option<u64>,
) -> Result<f64, &'static str> {
    let elapsed_ms = at_ms
        .checked_sub(old.at_ms)
        .filter(|ms| *ms > 0)
        .ok_or(NO_TIME)?;
    if seconds < old.seconds {
        return Err(CPU_WENT_BACK);
    }
    let figure = (seconds - old.seconds) / (elapsed_ms as f64 / 1000.0);
    if host_threads.is_some_and(|host| figure > host as f64 * ABOVE_HOST_MARGIN) {
        return Err(ABOVE_HOST);
    }
    Ok(figure)
}

/// The rows in their order, cut at [`INSTANCE_CPU_MAX`], with the rest counted and summed.
fn capped(mut rows: Vec<InstanceThreads>) -> InstanceCpu {
    sort_rows(&mut rows);
    let rest = rows.split_off(rows.len().min(INSTANCE_CPU_MAX));
    InstanceCpu {
        instances: rows,
        omitted: u64::try_from(rest.len()).unwrap_or(u64::MAX),
        omitted_busy_threads: rest.iter().filter_map(|row| row.busy_threads).sum(),
        reason: None,
    }
}

/// Those with a figure first, largest first, then those without one; ties by project and name.
fn sort_rows(rows: &mut [InstanceThreads]) {
    rows.sort_by(|a, b| match (a.busy_threads, b.busy_threads) {
        (Some(x), Some(y)) => y
            .total_cmp(&x)
            .then_with(|| (&a.project, &a.name).cmp(&(&b.project, &b.name))),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => (&a.project, &a.name).cmp(&(&b.project, &b.name)),
    });
}

/// The block of a sample whose instance list could not be read.
pub(crate) fn list_failed() -> InstanceCpu {
    InstanceCpu {
        reason: Some(LIST_FAILED.to_string()),
        ..InstanceCpu::default()
    }
}

/// The mean over a window: the newest sample's rows, each averaged over the samples that hold a figure for it. A row with
/// no figure in the window keeps the newest reason. The count, the sum of the rest and the reason are the newest sample's.
pub(crate) fn mean_instance_cpu(samples: &[&InstanceCpu]) -> Option<InstanceCpu> {
    let last = *samples.last()?;
    let mut instances: Vec<InstanceThreads> = last
        .instances
        .iter()
        .map(|row| {
            let held: Vec<f64> = samples
                .iter()
                .flat_map(|block| block.instances.iter())
                .filter(|other| other.project == row.project && other.name == row.name)
                .filter_map(|other| other.busy_threads)
                .collect();
            let figure = (!held.is_empty()).then(|| held.iter().sum::<f64>() / held.len() as f64);
            InstanceThreads {
                project: row.project.clone(),
                name: row.name.clone(),
                busy_threads: figure,
                reason: if figure.is_some() {
                    None
                } else {
                    row.reason.clone()
                },
            }
        })
        .collect();
    sort_rows(&mut instances);
    Some(InstanceCpu {
        instances,
        ..last.clone()
    })
}
