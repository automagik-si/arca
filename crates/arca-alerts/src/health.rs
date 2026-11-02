//! The HEALTH line: the one line that states the node's alerts. `alerts_list` answers it, and so does the console footer, so both
//! read the same text from the same snapshot.

use std::fmt::Write as _;

use crate::AlertSnapshot;
use crate::eval::SAMPLE_MAX_AGE_MS;

/// The HEALTH line of `snapshot` at `now_ms`.
///
/// It is `alerts stale <s>s` when the newest sample is older than [`SAMPLE_MAX_AGE_MS`], and `alerts stale` before any sample
/// exists. Otherwise it is `alerts none`, or `alerts <n> firing: <rules, comma-separated, in firing order>`, and ` · <m> unknown`
/// is appended when `m` entries of the snapshot's unknown list exist.
pub fn alerts_line(snapshot: &AlertSnapshot, now_ms: u64) -> String {
    let Some(sample_at) = snapshot.sample_at else {
        return "alerts stale".to_string();
    };
    let age = now_ms.saturating_sub(sample_at);
    if age > SAMPLE_MAX_AGE_MS {
        return format!("alerts stale {}s", age / 1_000);
    }
    let mut line = if snapshot.firing.is_empty() {
        "alerts none".to_string()
    } else {
        let rules: Vec<&str> = snapshot
            .firing
            .iter()
            .map(|alert| alert.event.rule.as_str())
            .collect();
        format!(
            "alerts {} firing: {}",
            snapshot.firing.len(),
            rules.join(", ")
        )
    };
    if !snapshot.unknown.is_empty() {
        let _ = write!(line, " · {} unknown", snapshot.unknown.len());
    }
    line
}
