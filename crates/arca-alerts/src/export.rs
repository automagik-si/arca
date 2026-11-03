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
use crate::eval::{
    HEALTH_MAX_AGE_MS, Inputs, OSD_MAX_AGE_MS, PROBE_MAX_AGE_MS, SMART_MAX_AGE_MS, Timed,
    WATCH_MAX_AGE_MS,
};

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
pub const LATENCY: &str = "arca_latency_seconds";
pub const SECURE_BOOT: &str = "arca_secureboot_enabled";
pub const TPM_PRESENT: &str = "arca_tpm_present";
pub const UPDATE_LAST_SUCCESS: &str = "arca_update_last_success_timestamp_seconds";
pub const UPDATE_FAILED: &str = "arca_update_failed";
pub const SMART_PASSED: &str = "arca_smart_passed";
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
const FAMILIES: [Family; 31] = [
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
        name: LATENCY,
        kind: "gauge",
        help: "The TCP connect time to a latency target, in seconds.",
        labels: &["target"],
    },
    Family {
        name: SECURE_BOOT,
        kind: "gauge",
        help: "1 when Secure Boot is enabled, 0 when it is not.",
        labels: &[],
    },
    Family {
        name: TPM_PRESENT,
        kind: "gauge",
        help: "1 when the TPM reports its status as ok, 0 when it does not.",
        labels: &[],
    },
    Family {
        name: UPDATE_LAST_SUCCESS,
        kind: "gauge",
        help: "The start time of the last completed incus-osd update check, in epoch seconds.",
        labels: &[],
    },
    Family {
        name: UPDATE_FAILED,
        kind: "gauge",
        help: "1 when incus-osd reports an update failure, no update check has completed in 14 days, or a node up 24 hours has no completed check on record; 0 when not.",
        labels: &[],
    },
    Family {
        name: SMART_PASSED,
        kind: "gauge",
        help: "1 when the drive's SMART health check passed, 0 when it failed. A drive with no usable SMART result has no series.",
        labels: &["device"],
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
    // A probe round is read only while it is no older than its source's maximum age, and each target is a point only when its
    // connect time is known: a failed probe is left out, never written as 0.
    if let Some(latency) = readings
        .latency
        .as_ref()
        .filter(|latency| inputs.at.saturating_sub(latency.at) <= PROBE_MAX_AGE_MS)
    {
        for (target, seconds) in &latency.targets {
            let labels = [("target", target.as_str())];
            push(&mut out, LATENCY, SeriesKind::Gauge, &labels, *seconds);
        }
    }
    // The incus-osd reading is written only while it is no older than its maximum age at the tick. A failed read is no reading.
    if let Some(osd) = readings
        .osd
        .as_ref()
        .filter(|osd| inputs.at.saturating_sub(osd.at) <= OSD_MAX_AGE_MS)
    {
        let flag = |value: bool| Some(if value { 1.0 } else { 0.0 });
        push(
            &mut out,
            SECURE_BOOT,
            SeriesKind::Gauge,
            &[],
            flag(osd.secure_boot),
        );
        push(
            &mut out,
            TPM_PRESENT,
            SeriesKind::Gauge,
            &[],
            flag(osd.tpm_ok),
        );
        push(
            &mut out,
            UPDATE_LAST_SUCCESS,
            SeriesKind::Gauge,
            &[],
            osd.last_success_ms.map(|ms| ms as f64 / 1_000.0),
        );
        push(
            &mut out,
            UPDATE_FAILED,
            SeriesKind::Gauge,
            &[],
            osd.update_failed.and_then(flag),
        );
    }
    // The SMART state is written only while it is no older than its maximum age at the tick, one point per drive with SMART enabled.
    if let Some(drives) = readings
        .drives
        .as_ref()
        .filter(|drives| inputs.at.saturating_sub(drives.at) <= SMART_MAX_AGE_MS)
    {
        for drive in &drives.drives {
            let labels = [("device", drive.device.as_str())];
            let passed = drive.passed.map(|passed| if passed { 1.0 } else { 0.0 });
            push(&mut out, SMART_PASSED, SeriesKind::Gauge, &labels, passed);
        }
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use arca_incus::{IncusError, IncusRead};
    use arca_metrics::{DriveReading, DrivesReading, Sampler, SamplerConfig};
    use serde_json::{Value, json};

    use super::*;
    use crate::AlertSnapshot;
    use crate::eval::UpdateUse;
    use crate::inputs::from_sampler;

    /// A fake secret. It is obviously fake, and no `/metrics` line may carry it.
    const FAKE_KEY: &str = "FAKE-RECOVERY-KEY-NOT-REAL-0003";
    const AT: u64 = 1_793_527_200_000;

    /// An Incus that answers the two incus-osd documents, or 404 for each when `served` is `None`.
    struct Osd {
        served: Option<(Value, Value)>,
    }

    impl IncusRead for Osd {
        fn get(&self, path: &str) -> Result<Value, IncusError> {
            match (&self.served, path) {
                (Some((security, _)), "/os/1.0/system/security") => Ok(security.clone()),
                (Some((_, update)), "/os/1.0/system/update") => Ok(update.clone()),
                _ => Err(IncusError::Status {
                    code: 404,
                    message: "not found".to_string(),
                }),
            }
        }
    }

    /// An Incus that answers the storage document with one SMART drive whose model and serial are fake, or 404 for any other path.
    struct Storage;

    impl IncusRead for Storage {
        fn get(&self, path: &str) -> Result<Value, IncusError> {
            if path != "/os/1.0/system/storage" {
                return Err(IncusError::Status {
                    code: 404,
                    message: "not found".to_string(),
                });
            }
            Ok(json!({ "state": { "drives": [{
                "id": "/dev/disk/by-id/ata-FAKE-MODEL-NOT-REAL_FAKE-SERIAL-0001",
                "model_name": "FAKE-MODEL-NOT-REAL",
                "serial_number": "FAKE-SERIAL-NOT-REAL-0001",
                "smart": { "enabled": true, "passed": false },
            }] } }))
        }
    }

    /// A sampler that has read the documents once at `AT`, over roots that do not exist.
    fn sampled(served: Option<(Value, Value)>) -> Sampler<Osd> {
        let mut sampler = Sampler::new(
            SamplerConfig {
                proc_root: PathBuf::from("/nonexistent/arca-export-proc"),
                sys_root: PathBuf::from("/nonexistent/arca-export-sys"),
                pools: Vec::new(),
                interval_ms: 5_000,
                history_minutes: 60,
            },
            Osd { served },
        );
        sampler.sample(AT);
        sampler
    }

    /// The documents with Secure Boot on, a TPM that reports ok, a check that reports no failure, and fake recovery keys.
    fn documents() -> (Value, Value) {
        (
            json!({ "state": {
                "secure_boot_enabled": true,
                "tpm_status": "ok",
                "drive_recovery_keys": { "drive1": FAKE_KEY },
                "pool_recovery_keys": { "pool1": FAKE_KEY },
            } }),
            json!({ "state": { "status": "OS update check completed", "last_check": "2026-11-01T10:00:00Z" } }),
        )
    }

    #[test]
    fn the_osd_families_render_from_the_documents_and_no_secret_text_reaches_metrics() {
        let sampler = sampled(Some(documents()));
        let inputs = from_sampler(&sampler);
        assert_eq!(
            inputs.update,
            Some(Timed {
                at: AT,
                value: UpdateUse { failed: false },
            })
        );
        let snapshot = AlertSnapshot {
            at: AT,
            sample_at: Some(AT),
            points: points(&sampler, &inputs),
            ..AlertSnapshot::default()
        };
        let text = render_metrics("node1", &snapshot);
        // Checked first, so the messages below that show the text never show a key.
        assert!(!text.contains(FAKE_KEY), "a fake key reached /metrics");
        assert!(
            text.contains("arca_secureboot_enabled{node=\"node1\"} 1\n"),
            "{text}"
        );
        assert!(
            text.contains("arca_tpm_present{node=\"node1\"} 1\n"),
            "{text}"
        );
        assert!(
            text.contains("arca_update_failed{node=\"node1\"} 0\n"),
            "{text}"
        );
        assert!(
            text.contains(
                "arca_update_last_success_timestamp_seconds{node=\"node1\"} 1793527200\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn a_smart_label_is_a_hash_and_no_fake_model_or_serial_reaches_metrics() {
        let mut sampler = Sampler::new(
            SamplerConfig {
                proc_root: PathBuf::from("/nonexistent/arca-export-smart-proc"),
                sys_root: PathBuf::from("/nonexistent/arca-export-smart-sys"),
                pools: Vec::new(),
                interval_ms: 5_000,
                history_minutes: 60,
            },
            Storage,
        );
        sampler.sample(AT);
        let inputs = from_sampler(&sampler);
        let text = render_metrics(
            "node1",
            &AlertSnapshot {
                at: AT,
                sample_at: Some(AT),
                points: points(&sampler, &inputs),
                ..AlertSnapshot::default()
            },
        );
        // Checked first, so the messages below that show the text never show a fake value.
        for fake in ["FAKE", "SERIAL", "MODEL", "by-id", "disk/"] {
            assert!(!text.contains(fake), "{fake} reached /metrics");
        }
        assert!(
            text.lines().any(|line| {
                line.starts_with("arca_smart_passed{") && line.contains(",device=\"drive-")
            }),
            "the drive is written under a drive- label"
        );
    }

    #[test]
    fn an_unreadable_osd_writes_no_osd_series() {
        let sampler = sampled(None);
        let inputs = from_sampler(&sampler);
        assert_eq!(inputs.update, None, "a failed read is unknown");
        let text = render_metrics(
            "node1",
            &AlertSnapshot {
                points: points(&sampler, &inputs),
                ..AlertSnapshot::default()
            },
        );
        assert!(!text.contains("arca_secureboot_enabled"), "{text}");
        assert!(!text.contains("arca_update_failed"), "{text}");
    }

    #[test]
    fn a_drive_with_no_smart_result_writes_no_smart_passed_series() {
        let sampler = sampled(None);
        let sample = sampler.latest().expect("one sample was taken");
        let readings = Readings {
            drives: Some(DrivesReading {
                at: AT,
                drives: vec![
                    DriveReading {
                        device: "drive-000000000001".to_string(),
                        passed: None,
                    },
                    DriveReading {
                        device: "drive-000000000002".to_string(),
                        passed: Some(true),
                    },
                ],
            }),
            ..Readings::default()
        };
        let inputs = Inputs {
            at: AT,
            ..Inputs::default()
        };
        let smart: Vec<String> = points_of(sample, &readings, &inputs)
            .iter()
            .filter(|point| point.name == SMART_PASSED)
            .map(|point| point.labels["device"].clone())
            .collect();
        assert_eq!(smart, vec!["drive-000000000002".to_string()]);
    }

    #[test]
    fn a_smart_reading_is_written_per_drive_and_left_out_once_older_than_its_max_age() {
        let sampler = sampled(None);
        let sample = sampler.latest().expect("one sample was taken");
        let readings = Readings {
            drives: Some(DrivesReading {
                at: AT,
                drives: vec![
                    DriveReading {
                        device: "drive-000000000001".to_string(),
                        passed: Some(true),
                    },
                    DriveReading {
                        device: "drive-000000000002".to_string(),
                        passed: Some(false),
                    },
                ],
            }),
            ..Readings::default()
        };
        let at = |at: u64| Inputs {
            at,
            ..Inputs::default()
        };
        let smart = |points: &[Point]| -> Vec<(String, f64)> {
            points
                .iter()
                .filter(|point| point.name == SMART_PASSED)
                .map(|point| (point.labels["device"].clone(), point.value))
                .collect()
        };
        assert_eq!(
            smart(&points_of(sample, &readings, &at(AT + SMART_MAX_AGE_MS))),
            vec![
                ("drive-000000000001".to_string(), 1.0),
                ("drive-000000000002".to_string(), 0.0),
            ],
            "at the boundary each drive is a point, 1 when passed and 0 when not"
        );
        assert!(
            smart(&points_of(
                sample,
                &readings,
                &at(AT + SMART_MAX_AGE_MS + 1)
            ))
            .is_empty(),
            "one millisecond past it is not"
        );
    }

    #[test]
    fn an_osd_reading_older_than_its_max_age_is_left_out() {
        let sampler = sampled(Some(documents()));
        let sample = sampler.latest().expect("one sample was taken");
        let fresh = Inputs {
            at: AT + OSD_MAX_AGE_MS,
            ..Inputs::default()
        };
        let stale = Inputs {
            at: AT + OSD_MAX_AGE_MS + 1,
            ..Inputs::default()
        };
        let has_osd = |points: &[Point]| points.iter().any(|point| point.name == UPDATE_FAILED);
        assert!(
            has_osd(&points_of(sample, sampler.readings(), &fresh)),
            "at the boundary it is still read"
        );
        assert!(
            !has_osd(&points_of(sample, sampler.readings(), &stale)),
            "one millisecond past it is not"
        );
    }
}
