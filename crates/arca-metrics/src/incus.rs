//! The Incus readings a host sample carries: the storage pools the allow-filter admits, with each one's space, and the
//! instances per project. Both are GET requests through [`IncusRead`]. An answer that fails is `None`, so the caller can tell
//! an unreachable Incus from an empty one. The sample's own lists stay empty on a failure, because the `node_metrics` schema
//! has no nullable list; the alerts read the `None` through [`crate::Readings`].

use std::collections::BTreeMap;

use arca_incus::IncusRead;
use serde_json::Value;

use crate::sample::{Pool, ProjectCount};

/// The instance list the project counts come from: every project, recursed.
pub const INSTANCES_PATH: &str = "/1.0/instances?all-projects=true&recursion=1";

/// The storage pool list Incus answers, recursed so that each entry carries its name.
pub const POOLS_PATH: &str = "/1.0/storage-pools?recursion=1";

/// The storage pools Incus lists that the allow-filter admits, in Incus's order, each with its `resources` space. An empty
/// filter admits every pool, and a filter name Incus does not list has no row. `None` when the listing fails. A pool whose
/// `resources` read fails keeps its row with null fields.
pub fn read_pools<R: IncusRead>(incus: &R, filter: &[String]) -> Option<Vec<Pool>> {
    let Ok(Value::Array(listed)) = incus.get(POOLS_PATH) else {
        return None;
    };
    Some(
        listed
            .iter()
            .filter_map(|entry| entry.get("name").and_then(Value::as_str))
            .filter(|name| admits(filter, name))
            .map(|name| read_pool(incus, name))
            .collect(),
    )
}

/// The instances per project, sorted by project name. A project appears only when it holds an instance. `None` when the
/// instance list cannot be read.
pub fn read_projects<R: IncusRead>(incus: &R) -> Option<Vec<ProjectCount>> {
    let Ok(Value::Array(instances)) = incus.get(INSTANCES_PATH) else {
        return None;
    };
    let mut counts: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    for instance in &instances {
        let Some(project) = instance.get("project").and_then(Value::as_str) else {
            continue;
        };
        let entry = counts.entry(project.to_string()).or_default();
        entry.0 += 1;
        if instance.get("status").and_then(Value::as_str) == Some("Running") {
            entry.1 += 1;
        }
    }
    Some(
        counts
            .into_iter()
            .map(|(project, (total, running))| ProjectCount {
                project,
                total,
                running,
            })
            .collect(),
    )
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
