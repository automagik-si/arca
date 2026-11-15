//! `report`: the last clean runs in a few lines each, for a person or an agent. Read from history.jsonl and the job
//! records; the daemon need not run.

use crate::jobs::clean::RunRecord;
use crate::jobs::{Job, State};
use crate::store::Store;
use crate::util::{human, now, parse_date, stamp};

/// `24h`, `7d`, `90m` or a `YYYY-MM-DD` date, as an epoch.
pub fn parse_since(text: &str, now: u64) -> Option<u64> {
    if let Some(day) = parse_date(text) {
        return Some(day);
    }
    let unit = text.chars().last()?;
    let n: u64 = text[..text.len() - unit.len_utf8()].parse().ok()?;
    let secs = match unit {
        'm' => n * 60,
        'h' => n * 3600,
        'd' => n * 86_400,
        _ => return None,
    };
    Some(now.saturating_sub(secs))
}

pub fn line(r: &RunRecord) -> String {
    let mut s = format!("{} {} ", stamp(r.at), r.kind);
    if let Some(why) = &r.refused {
        s.push_str(&format!("REFUSED (nothing applied): {why}"));
        return s;
    }
    if r.dry_run {
        s.push_str("dry run: ");
    }
    s.push_str(&format!(
        "done {}: freed {}, moved {} to trash (verified)",
        r.done,
        human(r.freed_bytes),
        human(r.trash_bytes)
    ));
    if r.skipped > 0 {
        let why: Vec<String> = r
            .skip_reasons
            .iter()
            .map(|(k, n)| format!("{n} {k}"))
            .collect();
        s.push_str(&format!("; skipped {} ({})", r.skipped, why.join(", ")));
    }
    if r.failed > 0 {
        s.push_str(&format!(
            "; FAILED {} ({} not freed)",
            r.failed,
            human(r.failed_bytes)
        ));
    }
    if let Some(first) = r.errors.first() {
        s.push_str(&format!("; first error: {first}"));
    }
    if let Some(stopped) = &r.stopped {
        s.push_str(&format!("; stopped early: {stopped}"));
    }
    s
}

/// The runs since `since` (and failed or cancelled jobs, which leave no run line), oldest first.
pub fn lines(store: &Store, since: u64) -> Vec<String> {
    let mut out: Vec<(u64, String)> = store
        .read_lines::<RunRecord>(&store.history_path())
        .into_iter()
        .filter(|r| r.at >= since)
        .map(|r| (r.at, line(&r)))
        .collect();
    for job in store.list_jobs::<Job>() {
        let at = job.finished.unwrap_or(job.created);
        if at < since || !matches!(job.state, State::Failed | State::Cancelled) {
            continue;
        }
        out.push((
            at,
            format!(
                "{} {} job {} {}{}",
                stamp(at),
                job.kind,
                job.id,
                job.state.as_str(),
                job.error.map(|e| format!(": {e}")).unwrap_or_default()
            ),
        ));
    }
    out.sort();
    out.into_iter().map(|(_, l)| l).collect()
}

pub fn default_since() -> u64 {
    now().saturating_sub(7 * 86_400)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn since_forms() {
        assert_eq!(parse_since("24h", 100_000), Some(100_000 - 86_400));
        assert_eq!(parse_since("2d", 200_000), Some(200_000 - 172_800));
        assert_eq!(parse_since("1970-01-02", 0), Some(86_400));
        assert_eq!(parse_since("soon", 0), None);
        assert_eq!(parse_since("", 0), None);
    }

    #[test]
    fn a_run_reads_in_one_line() {
        let mut reasons = BTreeMap::new();
        reasons.insert("protected (live process)".to_string(), 2);
        let r = RunRecord {
            at: 0,
            kind: "nightly".into(),
            done: 3,
            freed_bytes: 2 << 30,
            skipped: 2,
            skip_reasons: reasons,
            ..RunRecord::default()
        };
        assert_eq!(
            line(&r),
            "1970-01-01 00:00 UTC nightly done 3: freed 2.0 GiB, moved 0 B to trash (verified); skipped 2 (2 protected (live process))"
        );
        let refused = RunRecord {
            refused: Some("stop word".into()),
            ..r
        };
        assert!(line(&refused).contains("REFUSED"));
    }
}
