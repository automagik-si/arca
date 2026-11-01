//! The tick's points and the `/metrics` text, in the Prometheus text exposition format 0.0.4.
//!
//! Every series carries `node`. A reading that is unknown is left out, never written as 0, and no series uses the label
//! `instance`, which Prometheus sets on every scrape target.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use arca_incus::IncusRead;
use arca_metrics::Sampler;
use arca_store::{Point, SeriesKind};

use crate::AlertSnapshot;
use crate::eval::Inputs;

pub const POOL_SIZE: &str = "arca_pool_size_bytes";
pub const POOL_USED: &str = "arca_pool_used_bytes";
pub const ALERT_FIRING: &str = "arca_alert_firing";
pub const STORE_WRITE_FAILURES: &str = "arca_store_write_failures_total";
pub const SAMPLE_TIMESTAMP: &str = "arca_sample_timestamp_seconds";

/// The families `/metrics` renders, in the export contract's order, with their type and help text.
const FAMILIES: [(&str, &str, &str); 5] = [
    (POOL_SIZE, "gauge", "The size of a storage pool, in bytes."),
    (POOL_USED, "gauge", "The bytes a storage pool uses."),
    (ALERT_FIRING, "gauge", "1 for each firing alert."),
    (
        STORE_WRITE_FAILURES,
        "counter",
        "The node store's failed writes since the server started.",
    ),
    (
        SAMPLE_TIMESTAMP,
        "gauge",
        "When the newest host sample was taken, in epoch seconds.",
    ),
];

/// The points of the sampler's newest sample: each pool's size and used bytes, and the sample's time.
pub fn points<R: IncusRead>(sampler: &Sampler<R>, _inputs: &Inputs) -> Vec<Point> {
    let Some(sample) = sampler.latest() else {
        return Vec::new();
    };
    let mut points = Vec::new();
    for pool in &sample.pools {
        let labels = BTreeMap::from([("pool".to_string(), pool.name.clone())]);
        for (name, bytes) in [(POOL_SIZE, pool.total_bytes), (POOL_USED, pool.used_bytes)] {
            if let Some(bytes) = bytes {
                points.push(gauge(name, labels.clone(), bytes as f64));
            }
        }
    }
    points.push(gauge(
        SAMPLE_TIMESTAMP,
        BTreeMap::new(),
        sample.at as f64 / 1_000.0,
    ));
    points
}

fn gauge(name: &str, labels: BTreeMap<String, String>, value: f64) -> Point {
    Point {
        name: name.to_string(),
        labels,
        kind: SeriesKind::Gauge,
        value,
    }
}

/// The `/metrics` body of `snapshot` on `node`: each family that has a series, with its `# HELP` and `# TYPE` lines, its
/// series sorted by label values.
pub fn render_metrics(node: &str, snapshot: &AlertSnapshot) -> String {
    let mut out = String::new();
    for (name, kind, help) in FAMILIES {
        let mut series: Vec<(Vec<(&str, &str)>, f64)> = match name {
            ALERT_FIRING => snapshot
                .firing
                .iter()
                .map(|alert| {
                    let event = &alert.event;
                    let mut labels = vec![
                        ("rule", event.rule.as_str()),
                        ("severity", event.severity.as_str()),
                    ];
                    labels.extend(event.labels.iter().map(|(k, v)| (k.as_str(), v.as_str())));
                    (labels, 1.0)
                })
                .collect(),
            STORE_WRITE_FAILURES => vec![(Vec::new(), snapshot.store_write_failures as f64)],
            _ => snapshot
                .points
                .iter()
                .filter(|point| point.name == name)
                .map(|point| {
                    let labels = point.labels.iter().map(|(k, v)| (k.as_str(), v.as_str()));
                    (labels.collect(), point.value)
                })
                .collect(),
        };
        series.retain(|(_, value)| value.is_finite());
        if series.is_empty() {
            continue;
        }
        series.sort_by(|a, b| a.0.cmp(&b.0));
        let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
        for (labels, value) in series {
            let _ = write!(out, "{name}{{node=\"{}\"", escape(node));
            for (key, text) in labels {
                let _ = write!(out, ",{key}=\"{}\"", escape(text));
            }
            let _ = writeln!(out, "}} {value}");
        }
    }
    out
}

/// A label value with its backslashes, double quotes and newlines escaped.
fn escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}
