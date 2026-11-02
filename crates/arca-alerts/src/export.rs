//! The tick's points, the `/metrics` text in the Prometheus text exposition format 0.0.4, and the `/alerts` NDJSON.
//!
//! Every series carries `node`. A reading that is unknown is left out, never written as 0, and no series uses the label
//! `instance`, which Prometheus sets on every scrape target. Labels come in the contract's order: `node`, then the family's own
//! labels in their declared order, then an alert's own labels sorted by name. Families come in the contract's order.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use arca_incus::IncusRead;
use arca_metrics::{HostSample, Readings, Sampler};
use arca_store::{AlertEvent, Point, SeriesKind};

use crate::AlertSnapshot;
use crate::eval::{HEALTH_MAX_AGE_MS, Inputs, Timed, WATCH_MAX_AGE_MS};

pub const CPU_USED: &str = "arca_node_cpu_used_ratio";
pub const CPU_CORES: &str = "arca_node_cpu_cores";
pub const LOAD: &str = "arca_node_load";
pub const MEMORY_TOTAL: &str = "arca_node_memory_total_bytes";
pub const MEMORY_AVAILABLE: &str = "arca_node_memory_available_bytes";
pub const SWAP_TOTAL: &str = "arca_node_swap_total_bytes";
pub const SWAP_FREE: &str = "arca_node_swap_free_bytes";
pub const PSI: &str = "arca_node_psi_avg10_ratio";
pub const CPU_TEMPERATURE: &str = "arca_node_cpu_temperature_celsius";
pub const GPU_BUSY: &str = "arca_node_gpu_busy_ratio";
pub const NETWORK_RECEIVE: &str = "arca_node_network_receive_bytes_total";
pub const NETWORK_TRANSMIT: &str = "arca_node_network_transmit_bytes_total";
pub const POOL_SIZE: &str = "arca_pool_size_bytes";
pub const POOL_USED: &str = "arca_pool_used_bytes";
pub const PROJECT_INSTANCES: &str = "arca_project_instances";
pub const PROJECT_INSTANCES_RUNNING: &str = "arca_project_instances_running";
pub const COMPONENT_UP: &str = "arca_component_up";
pub const DRIFT_ENTRIES: &str = "arca_drift_entries";
pub const INSTANCE_RUNNING: &str = "arca_instance_running";
pub const INSTANCE_STARTS: &str = "arca_instance_starts_total";
pub const INSTANCE_CPU: &str = "arca_instance_cpu_seconds_total";
pub const INSTANCE_MEMORY: &str = "arca_instance_memory_used_bytes";
pub const ALERT_FIRING: &str = "arca_alert_firing";
pub const STORE_WRITE_FAILURES: &str = "arca_store_write_failures_total";
pub const SAMPLE_TIMESTAMP: &str = "arca_sample_timestamp_seconds";

/// The drift kinds `arca_drift_entries` exports. The watch report's kind that the closed set does not name is `other`, and it is
/// never a label.
pub const DRIFT_KINDS: [&str; 2] = ["marked_without_entry", "entry_without_object"];

/// One metric family: its name, its type, its help text, and its own labels in the order they are written.
struct Family {
    name: &'static str,
    kind: &'static str,
    help: &'static str,
    labels: &'static [&'static str],
}

/// The families `/metrics` renders, in the export contract's order. A new family goes in its place in this order.
const FAMILIES: [Family; 25] = [
    Family {
        name: CPU_USED,
        kind: "gauge",
        help: "The share of all CPUs busy since the previous sample, from 0 to 1.",
        labels: &[],
    },
    Family {
        name: CPU_CORES,
        kind: "gauge",
        help: "The number of CPUs.",
        labels: &[],
    },
    Family {
        name: LOAD,
        kind: "gauge",
        help: "The load average over 1, 5 and 15 minutes.",
        labels: &["window"],
    },
    Family {
        name: MEMORY_TOTAL,
        kind: "gauge",
        help: "The memory of the node, in bytes.",
        labels: &[],
    },
    Family {
        name: MEMORY_AVAILABLE,
        kind: "gauge",
        help: "The memory available to start applications, in bytes.",
        labels: &[],
    },
    Family {
        name: SWAP_TOTAL,
        kind: "gauge",
        help: "The swap space of the node, in bytes.",
        labels: &[],
    },
    Family {
        name: SWAP_FREE,
        kind: "gauge",
        help: "The free swap space, in bytes.",
        labels: &[],
    },
    Family {
        name: PSI,
        kind: "gauge",
        help: "The share of time tasks stalled on a resource, averaged over 10 seconds, from 0 to 1.",
        labels: &["resource", "kind"],
    },
    Family {
        name: CPU_TEMPERATURE,
        kind: "gauge",
        help: "The CPU temperature, in degrees Celsius.",
        labels: &[],
    },
    Family {
        name: GPU_BUSY,
        kind: "gauge",
        help: "The share of time the GPU is busy, from 0 to 1.",
        labels: &[],
    },
    Family {
        name: NETWORK_RECEIVE,
        kind: "counter",
        help: "The bytes the physical NICs have received.",
        labels: &[],
    },
    Family {
        name: NETWORK_TRANSMIT,
        kind: "counter",
        help: "The bytes the physical NICs have sent.",
        labels: &[],
    },
    Family {
        name: POOL_SIZE,
        kind: "gauge",
        help: "The size of a storage pool, in bytes.",
        labels: &["pool"],
    },
    Family {
        name: POOL_USED,
        kind: "gauge",
        help: "The bytes a storage pool uses.",
        labels: &["pool"],
    },
    Family {
        name: PROJECT_INSTANCES,
        kind: "gauge",
        help: "The instances in a project.",
        labels: &["project"],
    },
    Family {
        name: PROJECT_INSTANCES_RUNNING,
        kind: "gauge",
        help: "The running instances in a project.",
        labels: &["project"],
    },
    Family {
        name: COMPONENT_UP,
        kind: "gauge",
        help: "1 when a health component is ok, 0 when it is not.",
        labels: &["component"],
    },
    Family {
        name: DRIFT_ENTRIES,
        kind: "gauge",
        help: "The drift entries of the newest watch sweep, by kind.",
        labels: &["kind"],
    },
    Family {
        name: INSTANCE_RUNNING,
        kind: "gauge",
        help: "1 when an instance runs, 0 when it does not.",
        labels: &["project", "incus_instance", "type"],
    },
    Family {
        name: INSTANCE_STARTS,
        kind: "counter",
        help: "The times an instance has started since the server started.",
        labels: &["project", "incus_instance"],
    },
    Family {
        name: INSTANCE_CPU,
        kind: "counter",
        help: "The CPU time an instance has used, in seconds.",
        labels: &["project", "incus_instance"],
    },
    Family {
        name: INSTANCE_MEMORY,
        kind: "gauge",
        help: "The memory an instance uses, in bytes.",
        labels: &["project", "incus_instance"],
    },
    Family {
        name: ALERT_FIRING,
        kind: "gauge",
        help: "1 for each firing alert.",
        labels: &["rule", "severity"],
    },
    Family {
        name: STORE_WRITE_FAILURES,
        kind: "counter",
        help: "The node store's failed writes since the server started.",
        labels: &[],
    },
    Family {
        name: SAMPLE_TIMESTAMP,
        kind: "gauge",
        help: "When the newest host sample was taken, in epoch seconds.",
        labels: &[],
    },
];

/// The points of the sampler's newest sample, read with what Incus and the NICs answered at it, against the tick's `inputs`.
pub fn points<R: IncusRead>(sampler: &Sampler<R>, inputs: &Inputs) -> Vec<Point> {
    match sampler.latest() {
        Some(sample) => points_of(sample, sampler.readings(), inputs),
        None => Vec::new(),
    }
}

/// The points of one sample. A reading the sample or the readings lack is left out. A component or a drift count is read only
/// while it is no older than its source's maximum age at the tick, the same boundary the evaluator uses.
pub fn points_of(sample: &HostSample, readings: &Readings, inputs: &Inputs) -> Vec<Point> {
    let mut out = Vec::new();
    push(
        &mut out,
        CPU_USED,
        SeriesKind::Gauge,
        &[],
        sample.cpu.used_pct.map(|pct| pct / 100.0),
    );
    // `cpu.cores` is 0 when `/proc/stat` cannot be read, which is no reading.
    let cores = (sample.cpu.cores > 0).then_some(sample.cpu.cores as f64);
    push(&mut out, CPU_CORES, SeriesKind::Gauge, &[], cores);
    for (window, value) in [
        ("1m", sample.load.one),
        ("5m", sample.load.five),
        ("15m", sample.load.fifteen),
    ] {
        push(
            &mut out,
            LOAD,
            SeriesKind::Gauge,
            &[("window", window)],
            value,
        );
    }
    let memory = &sample.memory;
    for (name, bytes) in [
        (MEMORY_TOTAL, memory.total_bytes),
        (MEMORY_AVAILABLE, memory.available_bytes),
        (SWAP_TOTAL, memory.swap_total_bytes),
        (SWAP_FREE, memory.swap_free_bytes),
    ] {
        push(
            &mut out,
            name,
            SeriesKind::Gauge,
            &[],
            bytes.map(|b| b as f64),
        );
    }
    let psi = &sample.psi;
    for (resource, kind, pct) in [
        ("memory", "some", psi.memory_some_avg10),
        ("memory", "full", psi.memory_full_avg10),
        ("cpu", "some", psi.cpu_some_avg10),
    ] {
        let labels = [("resource", resource), ("kind", kind)];
        push(
            &mut out,
            PSI,
            SeriesKind::Gauge,
            &labels,
            pct.map(|pct| pct / 100.0),
        );
    }
    push(
        &mut out,
        CPU_TEMPERATURE,
        SeriesKind::Gauge,
        &[],
        sample.cpu_temp_c,
    );
    push(
        &mut out,
        GPU_BUSY,
        SeriesKind::Gauge,
        &[],
        sample.gpu_busy_pct.map(|pct| pct / 100.0),
    );
    if let Some(nic) = readings.nic {
        push(
            &mut out,
            NETWORK_RECEIVE,
            SeriesKind::Counter,
            &[],
            Some(nic.rx_bytes as f64),
        );
        push(
            &mut out,
            NETWORK_TRANSMIT,
            SeriesKind::Counter,
            &[],
            Some(nic.tx_bytes as f64),
        );
    }
    for pool in &sample.pools {
        let labels = [("pool", pool.name.as_str())];
        push(
            &mut out,
            POOL_SIZE,
            SeriesKind::Gauge,
            &labels,
            pool.total_bytes.map(|b| b as f64),
        );
        push(
            &mut out,
            POOL_USED,
            SeriesKind::Gauge,
            &labels,
            pool.used_bytes.map(|b| b as f64),
        );
    }
    for project in readings.projects.iter().flatten() {
        let labels = [("project", project.project.as_str())];
        push(
            &mut out,
            PROJECT_INSTANCES,
            SeriesKind::Gauge,
            &labels,
            Some(project.total as f64),
        );
        push(
            &mut out,
            PROJECT_INSTANCES_RUNNING,
            SeriesKind::Gauge,
            &labels,
            Some(project.running as f64),
        );
    }
    for instance in readings.instances.iter().flatten() {
        let labels = [
            ("project", instance.project.as_str()),
            ("incus_instance", instance.name.as_str()),
        ];
        let running = if instance.running { 1.0 } else { 0.0 };
        let typed = [labels[0], labels[1], ("type", instance.kind.as_str())];
        push(
            &mut out,
            INSTANCE_RUNNING,
            SeriesKind::Gauge,
            &typed,
            Some(running),
        );
        push(
            &mut out,
            INSTANCE_STARTS,
            SeriesKind::Counter,
            &labels,
            Some(instance.starts_total as f64),
        );
        push(
            &mut out,
            INSTANCE_CPU,
            SeriesKind::Counter,
            &labels,
            instance.cpu_seconds,
        );
        push(
            &mut out,
            INSTANCE_MEMORY,
            SeriesKind::Gauge,
            &labels,
            instance.memory_bytes.map(|bytes| bytes as f64),
        );
    }
    if let Some(components) = fresh(&inputs.components, HEALTH_MAX_AGE_MS, inputs.at) {
        for component in &components.value {
            let ok = if component.ok { 1.0 } else { 0.0 };
            let labels = [("component", component.name.as_str())];
            push(&mut out, COMPONENT_UP, SeriesKind::Gauge, &labels, Some(ok));
        }
    }
    if let Some(watch) = fresh(&inputs.watch, WATCH_MAX_AGE_MS, inputs.at) {
        for kind in DRIFT_KINDS {
            let count = watch.value.drift.get(kind).copied().unwrap_or(0);
            push(
                &mut out,
                DRIFT_ENTRIES,
                SeriesKind::Gauge,
                &[("kind", kind)],
                Some(count as f64),
            );
        }
    }
    push(
        &mut out,
        SAMPLE_TIMESTAMP,
        SeriesKind::Gauge,
        &[],
        Some(sample.at as f64 / 1_000.0),
    );
    out
}

/// Adds one point, unless its value is unknown or not finite.
fn push(
    out: &mut Vec<Point>,
    name: &str,
    kind: SeriesKind,
    labels: &[(&str, &str)],
    value: Option<f64>,
) {
    let Some(value) = value.filter(|value| value.is_finite()) else {
        return;
    };
    out.push(Point {
        name: name.to_string(),
        labels: labels
            .iter()
            .map(|(key, text)| (key.to_string(), text.to_string()))
            .collect(),
        kind,
        value,
    });
}

/// The reading when it is no older than `max_age` at `now`.
fn fresh<T>(reading: &Option<Timed<T>>, max_age: u64, now: u64) -> Option<&Timed<T>> {
    reading
        .as_ref()
        .filter(|timed| now.saturating_sub(timed.at) <= max_age)
}

/// The `/metrics` body of `snapshot` on `node`: each family that has a series, with its `# HELP` and `# TYPE` lines, its series
/// sorted by label values.
pub fn render_metrics(node: &str, snapshot: &AlertSnapshot) -> String {
    let mut out = String::new();
    for family in &FAMILIES {
        let mut series = series_of(family, snapshot);
        if series.is_empty() {
            continue;
        }
        series.sort_by(|a, b| {
            a.labels
                .iter()
                .map(|(_, value)| *value)
                .cmp(b.labels.iter().map(|(_, value)| *value))
        });
        let _ = writeln!(
            out,
            "# HELP {} {}\n# TYPE {} {}",
            family.name, family.help, family.name, family.kind
        );
        for series in series {
            let _ = write!(out, "{}{{node=\"{}\"", family.name, escape(node));
            for (key, value) in &series.labels {
                let _ = write!(out, ",{key}=\"{}\"", escape(value));
            }
            let _ = writeln!(out, "}} {}", series.value);
        }
    }
    out
}

/// One series of a family: its labels in the order they are written, and its value.
struct Series<'a> {
    labels: Vec<(&'a str, &'a str)>,
    value: f64,
}

/// The series of `family` in `snapshot`. Firing alerts and the store's failures come from the snapshot, every other family from its
/// points.
fn series_of<'a>(family: &Family, snapshot: &'a AlertSnapshot) -> Vec<Series<'a>> {
    match family.name {
        ALERT_FIRING => snapshot
            .firing
            .iter()
            .map(|alert| {
                let mut labels: BTreeMap<&str, &str> = alert
                    .event
                    .labels
                    .iter()
                    .map(|(key, value)| (key.as_str(), value.as_str()))
                    .collect();
                labels.insert("rule", alert.event.rule.as_str());
                labels.insert("severity", alert.event.severity.as_str());
                Series {
                    labels: ordered(family.labels, &labels),
                    value: 1.0,
                }
            })
            .collect(),
        STORE_WRITE_FAILURES => vec![Series {
            labels: Vec::new(),
            value: snapshot.store_write_failures as f64,
        }],
        _ => snapshot
            .points
            .iter()
            .filter(|point| point.name == family.name)
            .map(|point| {
                let labels: BTreeMap<&str, &str> = point
                    .labels
                    .iter()
                    .map(|(key, value)| (key.as_str(), value.as_str()))
                    .collect();
                Series {
                    labels: ordered(family.labels, &labels),
                    value: point.value,
                }
            })
            .collect(),
    }
}

/// The labels in the order a family writes them: its declared names first, in that order, then the others sorted by name.
fn ordered<'a>(declared: &[&str], labels: &BTreeMap<&'a str, &'a str>) -> Vec<(&'a str, &'a str)> {
    let mut out: Vec<(&'a str, &'a str)> = declared
        .iter()
        .filter_map(|name| {
            labels
                .get_key_value(*name)
                .map(|(key, value)| (*key, *value))
        })
        .collect();
    out.extend(
        labels
            .iter()
            .filter(|(key, _)| !declared.iter().any(|name| *name == **key))
            .map(|(key, value)| (*key, *value)),
    );
    out
}

/// The NDJSON body of `events` on `node`: one `arca.alert.v1` object per line, in the order given.
pub fn render_events(node: &str, events: &[AlertEvent]) -> String {
    let mut out = String::new();
    for event in events {
        out.push_str(&event.to_json(node).to_string());
        out.push('\n');
    }
    out
}

/// A label value with its backslashes, double quotes and newlines escaped.
fn escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}
