//! The Incus readings a host sample carries: each configured pool's space, and the instances per project. Both are GET
//! requests through [`IncusRead`]. An answer that fails gives null pool fields and no project rows: the sample's schema has
//! no nullable list, so an unreachable Incus reads as no instances, and the `arca_status` verb's `incusReachable` says so.

use std::collections::BTreeMap;

use arca_incus::IncusRead;
use serde_json::Value;

use crate::sample::{Pool, ProjectCount};

/// The instance list the project counts come from: every project, recursed.
pub const INSTANCES_PATH: &str = "/1.0/instances?all-projects=true&recursion=1";

/// The space of each configured pool, from its `resources` document, in configuration order. A name that is not a plain
/// name is never sent to Incus, so its fields stay null.
pub fn read_pools<R: IncusRead>(incus: &R, pools: &[String]) -> Vec<Pool> {
    pools.iter().map(|name| read_pool(incus, name)).collect()
}

/// The instances per project, sorted by project name. A project appears only when it holds an instance.
pub fn read_projects<R: IncusRead>(incus: &R) -> Vec<ProjectCount> {
    let Ok(Value::Array(instances)) = incus.get(INSTANCES_PATH) else {
        return Vec::new();
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
    counts
        .into_iter()
        .map(|(project, (total, running))| ProjectCount {
            project,
            total,
            running,
        })
        .collect()
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
