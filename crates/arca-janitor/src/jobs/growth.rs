//! growth: which folders grew between two snapshots of the same root. A fresh snapshot is taken only when the newest
//! one is older than `max_age_hours`, or when there are fewer than two.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::jobs::space::{Snapshot, take};
use crate::jobs::{Ctx, GrowthParams, JobError};
use crate::util::now;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Grew {
    pub path: PathBuf,
    pub before: u64,
    pub after: u64,
    pub grew: i64,
    /// Growth per day over the span between the two snapshots.
    pub per_day: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Growth {
    pub root: PathBuf,
    pub older: String,
    pub newer: String,
    pub span_hours: u64,
    pub total_grew: i64,
    pub fresh_scan: bool,
    pub grew: Vec<Grew>,
}

/// Compare two snapshots of one root: the folders that grew most, biggest first. A folder present in both and
/// inside a folder that is also listed is still listed on its own, so the deepest culprit shows.
pub fn compare(older: &Snapshot, newer: &Snapshot, top: usize) -> Growth {
    let span = newer.taken.saturating_sub(older.taken).max(1);
    let mut grew: Vec<Grew> = newer
        .dirs
        .iter()
        .filter(|(p, _)| p.as_path() != newer.root)
        .map(|(p, after)| {
            let before = older.dirs.get(p).copied().unwrap_or(0);
            let delta = *after as i64 - before as i64;
            Grew {
                path: p.clone(),
                before,
                after: *after,
                grew: delta,
                per_day: (delta as f64 * 86_400.0 / span as f64) as i64,
            }
        })
        .filter(|g| g.grew > 0)
        .collect();
    grew.sort_by_key(|g| std::cmp::Reverse(g.grew));
    grew.truncate(top);
    Growth {
        root: newer.root.clone(),
        older: older.id.clone(),
        newer: newer.id.clone(),
        span_hours: span / 3600,
        total_grew: newer.total as i64 - older.total as i64,
        fresh_scan: false,
        grew,
    }
}

pub fn run(p: &GrowthParams, ctx: &Ctx) -> Result<Growth, JobError> {
    let root = p.path.clone().unwrap_or_else(|| ctx.config.home.clone());
    let mut snaps: Vec<Snapshot> = ctx
        .store
        .list_snapshots::<Snapshot>()
        .into_iter()
        .filter(|s| s.root == root)
        .collect();
    snaps.sort_by_key(|s| s.taken);
    let stale = snaps
        .last()
        .is_none_or(|s| now().saturating_sub(s.taken) > p.max_age_hours * 3600);
    let mut fresh = false;
    if stale || snaps.len() < 2 {
        let (snap, _) = take(&root, p.depth, 0, ctx)?;
        snaps.push(snap);
        fresh = true;
    }
    if snaps.len() < 2 {
        return Err(JobError::Failed(format!(
            "only one snapshot of {} so far; run growth again later to compare",
            root.display()
        )));
    }
    let newer = snaps.last().cloned().ok_or("no snapshot".to_string())?;
    let cutoff = newer.taken.saturating_sub(p.older_hours * 3600);
    let older = snaps[..snaps.len() - 1]
        .iter()
        .rev()
        .find(|s| s.taken <= cutoff)
        .unwrap_or(&snaps[0])
        .clone();
    let mut g = compare(&older, &newer, p.top);
    g.fresh_scan = fresh;
    Ok(g)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn snap(id: &str, taken: u64, dirs: &[(&str, u64)]) -> Snapshot {
        Snapshot {
            id: id.into(),
            taken,
            root: "/r".into(),
            depth: 2,
            total: dirs.iter().map(|d| d.1).max().unwrap_or(0),
            dirs: dirs
                .iter()
                .map(|(p, b)| (PathBuf::from(p), *b))
                .collect::<BTreeMap<_, _>>(),
            unreadable: 0,
        }
    }

    #[test]
    fn ranks_what_grew() {
        let a = snap("a", 0, &[("/r", 100), ("/r/x", 50), ("/r/y", 50)]);
        let b = snap(
            "b",
            86_400,
            &[("/r", 400), ("/r/x", 60), ("/r/y", 40), ("/r/z", 300)],
        );
        let g = compare(&a, &b, 10);
        let order: Vec<&str> = g.grew.iter().map(|g| g.path.to_str().unwrap()).collect();
        assert_eq!(order, vec!["/r/z", "/r/x"]);
        assert_eq!(g.grew[0].per_day, 300);
        assert_eq!(g.total_grew, 300);
    }
}
