//! The Incus readings a host sample carries: the host totals of the machine Incus serves, the storage pools the allow-filter
//! admits, with each one's space, and the instances, with each one's state, starts and usage. All are GET requests through
//! [`IncusRead`]. An answer that fails is `None`, so the caller can tell an unreachable Incus from an empty one. The sample's
//! own lists stay empty on a failure, because the `node_metrics` schema has no nullable list; the alerts read the `None`
//! through [`crate::Readings`].

use std::collections::BTreeMap;

use arca_incus::IncusRead;
use serde_json::Value;

use crate::sample::{Pool, ProjectCount};

/// The host resources of the machine the node's Incus serves. This is the machine Arca manages, which is not the machine Arca
/// runs on when the socket is a proxy to a remote Incus.
pub const RESOURCES_PATH: &str = "/1.0/resources";

/// The instance list every tick reads: every project, recursed twice, so that each row carries its `state`. One GET serves the
/// instance readings and the project counts.
pub const INSTANCES_PATH: &str = "/1.0/instances?all-projects=true&recursion=2";

/// The host totals Incus reports: its logical CPUs (threads), its memory total and its used memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostTotals {
    /// The logical CPUs the host has, from `/cpu/total`. Incus counts threads here.
    pub cpu_threads: u64,
    /// The host's memory, in bytes, from `/memory/total`.
    pub memory_total_bytes: u64,
    /// The memory in use, in bytes, from `/memory/used`.
    pub memory_used_bytes: u64,
}

/// Reads the host totals through the node's own Incus socket. `None` when the GET fails, or when the document lacks a
/// positive CPU count or the memory total and used figures: a malformed answer is unknown, never a zero host.
pub fn read_host_totals<R: IncusRead>(incus: &R) -> Option<HostTotals> {
    let document = incus.get(RESOURCES_PATH).ok()?;
    host_totals(&document)
}

/// The totals of a `/1.0/resources` document. `None` unless the three figures are whole numbers and the host has a CPU.
fn host_totals(document: &Value) -> Option<HostTotals> {
    let cpu_threads = document.pointer("/cpu/total")?.as_u64()?;
    let memory_total_bytes = document.pointer("/memory/total")?.as_u64()?;
    let memory_used_bytes = document.pointer("/memory/used")?.as_u64()?;
    (cpu_threads > 0 && memory_total_bytes > 0).then_some(HostTotals {
        cpu_threads,
        memory_total_bytes,
        memory_used_bytes,
    })
}

/// The kind of an instance. Incus has two: a container, and a virtual machine (`virtual-machine` on the wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstanceKind {
    Container,
    Vm,
}

impl InstanceKind {
    /// The `type` label the export writes: `container` or `vm`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Container => "container",
            Self::Vm => "vm",
        }
    }
}

/// One instance as the tick's list read it. `starts_total` and `starts_10m` are the sampler's counts, zero until it counts.
/// `cpu_seconds` and `memory_bytes` are `None` when the row carries no usage, as a stopped instance's `state` does.
#[derive(Debug, Clone, PartialEq)]
pub struct InstanceReading {
    pub project: String,
    pub name: String,
    pub kind: InstanceKind,
    pub running: bool,
    pub starts_total: u64,
    pub starts_10m: u64,
    pub cpu_seconds: Option<f64>,
    pub memory_bytes: Option<u64>,
}

/// One row of the instance list, with the `last_used_at` the sampler compares between ticks. A row without a project or a name
/// is not an instance and is dropped.
#[derive(Debug, Clone, PartialEq)]
pub struct Listed {
    pub reading: InstanceReading,
    pub last_used: Option<String>,
}

/// The storage pool list Incus answers, recursed so that each entry carries its name.
pub const POOLS_PATH: &str = "/1.0/storage-pools?recursion=1";

/// The storage pools Incus lists that the allow-filter admits, in Incus's order, each with its `resources` space. An empty
/// filter admits every pool, and a filter name Incus does not list has no row. `None` when the listing fails, or when any entry
/// has no string name: a malformed listing is unknown, never a shorter list that could read as known-empty. A pool whose
/// `resources` read fails keeps its row with null fields.
pub fn read_pools<R: IncusRead>(incus: &R, filter: &[String]) -> Option<Vec<Pool>> {
    let Ok(Value::Array(listed)) = incus.get(POOLS_PATH) else {
        return None;
    };
    let names = listed
        .iter()
        .map(|entry| entry.get("name").and_then(Value::as_str))
        .collect::<Option<Vec<&str>>>()?;
    Some(
        names
            .into_iter()
            .filter(|name| admits(filter, name))
            .map(|name| read_pool(incus, name))
            .collect(),
    )
}

/// The instance rows of the one list GET every tick makes. `None` when the list cannot be read, so the caller can tell an
/// unreachable Incus from an empty one. A row without a project or a name is dropped.
pub fn read_instances<R: IncusRead>(incus: &R) -> Option<Vec<Listed>> {
    let Ok(Value::Array(rows)) = incus.get(INSTANCES_PATH) else {
        return None;
    };
    Some(rows.iter().filter_map(listed).collect())
}

/// One row as an instance. The starts are zero here: the sampler counts them against the previous tick.
fn listed(row: &Value) -> Option<Listed> {
    let project = row.get("project")?.as_str()?;
    let name = row.get("name")?.as_str()?;
    let kind = match row.get("type").and_then(Value::as_str) {
        Some("virtual-machine") => InstanceKind::Vm,
        _ => InstanceKind::Container,
    };
    // A stopped instance's `state` is an object of zeros on the wire, so it is ignored here: it has no usage to read.
    let stopped = row.get("status").and_then(Value::as_str) == Some("Stopped");
    let state = row
        .get("state")
        .filter(|state| !state.is_null() && !stopped);
    Some(Listed {
        reading: InstanceReading {
            project: project.to_string(),
            name: name.to_string(),
            kind,
            running: row.get("status").and_then(Value::as_str) == Some("Running"),
            starts_total: 0,
            starts_10m: 0,
            cpu_seconds: state
                .and_then(|state| state.pointer("/cpu/usage"))
                .and_then(Value::as_u64)
                .map(|nanoseconds| nanoseconds as f64 / 1e9),
            memory_bytes: state
                .and_then(|state| state.pointer("/memory/usage"))
                .and_then(Value::as_u64),
        },
        last_used: row
            .get("last_used_at")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// The instances per project, sorted by project name, from the instance readings of one tick. A project appears only when it
/// holds an instance.
pub fn project_counts(instances: &[InstanceReading]) -> Vec<ProjectCount> {
    let mut counts: BTreeMap<&str, (u64, u64)> = BTreeMap::new();
    for instance in instances {
        let entry = counts.entry(instance.project.as_str()).or_default();
        entry.0 += 1;
        if instance.running {
            entry.1 += 1;
        }
    }
    counts
        .into_iter()
        .map(|(project, (total, running))| ProjectCount {
            project: project.to_string(),
            total,
            running,
        })
        .collect()
}

/// Whether the allow-filter admits a pool. An empty filter admits every pool.
fn admits(filter: &[String], name: &str) -> bool {
    filter.is_empty() || filter.iter().any(|allowed| allowed == name)
}

fn read_pool<R: IncusRead>(incus: &R, name: &str) -> Pool {
    let space = if is_plain_name(name) {
        incus
            .get(&format!("/1.0/storage-pools/{name}/resources"))
            .ok()
            .and_then(|resources| space_of(&resources))
    } else {
        None
    };
    let (used_bytes, total_bytes) = space.unzip();
    Pool {
        name: name.to_string(),
        used_bytes,
        total_bytes,
        free_bytes: used_bytes
            .zip(total_bytes)
            .map(|(used, total)| total.saturating_sub(used)),
    }
}

/// `space.used` and `space.total` of a `resources` document, in bytes.
fn space_of(resources: &Value) -> Option<(u64, u64)> {
    let space = resources.get("space")?;
    Some((space.get("used")?.as_u64()?, space.get("total")?.as_u64()?))
}

/// A pool name that is only letters, digits, `-`, `_` and `.`, and is not a dot segment. It goes into a request path, so a
/// name with a slash or a query mark is never sent.
fn is_plain_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_pool_entry_without_a_name_makes_the_whole_listing_unknown() {
        struct Listing(Value);
        impl IncusRead for Listing {
            fn get(&self, path: &str) -> Result<Value, arca_incus::IncusError> {
                if path == POOLS_PATH {
                    return Ok(self.0.clone());
                }
                Err(arca_incus::IncusError::Decode(format!(
                    "no answer for {path}"
                )))
            }
        }
        let filter: Vec<String> = Vec::new();
        let named = Listing(json!([{ "name": "default" }, { "name": "pool1" }]));
        assert_eq!(
            read_pools(&named, &filter).map(|pools| pools.len()),
            Some(2)
        );
        let nameless = Listing(json!([{ "name": "default" }, { "driver": "zfs" }]));
        assert_eq!(read_pools(&nameless, &filter), None);
        let not_text = Listing(json!([{ "name": 7 }]));
        assert_eq!(read_pools(&not_text, &filter), None);
    }

    #[test]
    fn host_totals_read_the_three_resources_figures_and_refuse_a_partial_document() {
        let full = json!({
            "cpu": { "total": 16 },
            "memory": { "total": 17_045_651_456u64, "used": 3_417_186_304u64 },
        });
        assert_eq!(
            host_totals(&full),
            Some(HostTotals {
                cpu_threads: 16,
                memory_total_bytes: 17_045_651_456,
                memory_used_bytes: 3_417_186_304,
            })
        );
        for partial in [
            json!({ "cpu": { "total": 16 }, "memory": { "total": 17_045_651_456u64 } }),
            json!({ "cpu": { "total": 16 }, "memory": { "used": 3u64 } }),
            json!({ "cpu": { "total": 0 }, "memory": { "total": 1u64, "used": 0u64 } }),
            json!({ "cpu": { "total": "16" }, "memory": { "total": 1u64, "used": 0u64 } }),
            json!({}),
        ] {
            assert_eq!(host_totals(&partial), None, "{partial}");
        }
    }

    #[test]
    fn stopped_instance_reports_no_usage_even_with_zero_state_object() {
        let row = json!({
            "project": "default",
            "name": "web1",
            "type": "container",
            "status": "Stopped",
            "state": { "cpu": { "usage": 0 }, "memory": { "usage": 0 } },
        });
        let reading = listed(&row).expect("a named instance is listed").reading;
        assert_eq!(reading.cpu_seconds, None);
        assert_eq!(reading.memory_bytes, None);
    }
}
