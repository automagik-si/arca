//! insights: where the trash comes from, and what would stop it coming back. Rules only, no model.
//!
//! It reads the item history (origins.jsonl) and the snapshots, and ranks findings by what they cost: bytes cleaned,
//! times they came back, how fast they grow. Each finding names its evidence and one concrete change at the source.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::jobs::clean::OriginEvent;
use crate::jobs::growth::compare;
use crate::jobs::space::Snapshot;
use crate::jobs::{Ctx, InsightsParams, JobError};
use crate::util::{human, now, stamp};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Finding {
    pub rank: usize,
    /// Which rule found it: recurring, worktree-targets, worktree-node-modules, cache-cap, docker-build-cache, logs,
    /// test-temp-dirs, growth.
    pub rule: String,
    pub title: String,
    pub bytes: u64,
    pub occurrences: usize,
    pub evidence: Vec<String>,
    pub suggestion: String,
    pub score: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Insights {
    pub since: u64,
    pub events: usize,
    pub findings: Vec<Finding>,
}

fn finding(
    rule: &str,
    title: String,
    bytes: u64,
    occurrences: usize,
    evidence: Vec<String>,
    suggestion: String,
) -> Finding {
    Finding {
        rank: 0,
        rule: rule.to_string(),
        title,
        bytes,
        occurrences,
        evidence,
        suggestion,
        score: bytes as f64 * (1.0 + occurrences as f64),
    }
}

fn name_of(label: &str) -> &str {
    Path::new(label)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(label)
}

/// A folder name that looks like a test run's leftover temp dir.
pub fn looks_like_test_temp(name: &str) -> bool {
    let lower = name.to_lowercase();
    const PREFIXES: [&str; 12] = [
        "tmp",
        ".tmp",
        "temp",
        "test-",
        "test_",
        "pytest-",
        "rust-test",
        "cargo-test",
        "jest_",
        "vitest",
        "playwright",
        "go-build",
    ];
    if PREFIXES.iter().any(|p| lower.starts_with(p)) {
        return true;
    }
    // A random suffix: `something-a8Fk3x9Q` or `abc.XXXXXX`.
    lower.rsplit(['-', '.', '_']).next().is_some_and(|tail| {
        tail.len() >= 6
            && tail.chars().any(|c| c.is_ascii_digit())
            && tail.chars().any(|c| c.is_ascii_alphabetic())
            && tail != lower
    })
}

fn suggestion_for(pattern: &str, label: &str) -> String {
    let name = name_of(label);
    match (pattern, name) {
        ("build-output", "target") => "Share one CARGO_TARGET_DIR per repository across its worktrees (for example `[build] target-dir` in the repository's .cargo/config.toml), so each worktree stops building its own copy; or remove target/ in the worktree's teardown.".to_string(),
        ("build-output", "node_modules") => "Install with a content-addressed store (pnpm, or bun's global cache) so worktrees hard-link instead of copying, and remove node_modules in the worktree's teardown hook.".to_string(),
        ("build-output", _) => "Remove build caches (.next, .turbo, .parcel-cache) in the project's clean or teardown step.".to_string(),
        ("tool-cache", _) => cache_cap_hint(label),
        ("scratch", _) => "Whatever writes here should remove its folder when it ends (a TempDir dropped at the end, a trap on exit), or write under a per-run TMPDIR that is removed after the run.".to_string(),
        ("docker", _) => docker_hint(),
        ("log", _) => log_hint(),
        _ => "Find what writes this and make it clean up after itself.".to_string(),
    }
}

fn cache_cap_hint(label: &str) -> String {
    if label.contains("cargo") || label.contains(".cargo") {
        "Let cargo trim its own cache: `[cache] auto-clean-frequency = \"1 day\"` in ~/.cargo/config.toml (stable cargo garbage collection), instead of emptying it nightly.".to_string()
    } else if label.contains("npm") {
        "Cap npm's cache growth: run `npm cache verify` weekly (it garbage-collects), or raise NPM_MAX_MB if a warm cache matters more than the space.".to_string()
    } else if label.contains("go") {
        "Set GOCACHE to a dedicated folder and trim it with `go clean -cache` weekly, or raise GOBUILD_MAX_MB.".to_string()
    } else {
        "Give this cache a size cap in its tool's own config, so it trims itself instead of growing until the nightly run.".to_string()
    }
}

fn docker_hint() -> String {
    "Turn on the build cache's garbage collector: in /etc/docker/daemon.json, `\"builder\": {\"gc\": {\"enabled\": true, \"defaultKeepStorage\": \"20GB\"}}`; tag or remove images in the build script so they do not go dangling.".to_string()
}

fn log_hint() -> String {
    "Rotate it: logrotate or journald limits on Linux, newsyslog on macOS; or have the writer cap its own files.".to_string()
}

pub fn analyse(events: &[OriginEvent], snapshots: &[Snapshot], top: usize) -> Vec<Finding> {
    let mut findings = Vec::new();

    // 1. Recurring: cleaned, then back again.
    let mut by_label: BTreeMap<&str, Vec<&OriginEvent>> = BTreeMap::new();
    for e in events
        .iter()
        .filter(|e| e.event == "applied" || e.event == "planned")
    {
        by_label.entry(e.label.as_str()).or_default().push(e);
    }
    for (label, evs) in &by_label {
        let applied: Vec<&&OriginEvent> = evs.iter().filter(|e| e.event == "applied").collect();
        let Some(first) = applied.iter().map(|e| e.at).min() else {
            continue;
        };
        let back: Vec<&&OriginEvent> = evs
            .iter()
            .filter(|e| e.at > first && e.plan != applied[0].plan)
            .collect();
        let distinct_plans: BTreeSet<&str> = back.iter().map(|e| e.plan.as_str()).collect();
        if distinct_plans.is_empty() {
            continue;
        }
        let cleaned: u64 = applied.iter().map(|e| e.bytes).sum();
        let last = evs.iter().max_by_key(|e| e.at).copied();
        let span_days = last.map_or(1, |l| (l.at.saturating_sub(first) / 86_400).max(1));
        let regrow = last.map_or(0, |l| l.bytes) / span_days;
        let pattern = evs[0].origin.pattern.as_str();
        findings.push(finding(
            "recurring",
            format!(
                "{label} came back {} times after cleaning",
                distinct_plans.len()
            ),
            cleaned + last.map_or(0, |l| l.bytes),
            distinct_plans.len(),
            vec![
                format!(
                    "first cleaned {} ({})",
                    stamp(first),
                    human(applied[0].bytes)
                ),
                format!(
                    "seen again in {} later plans; regrows about {}/day",
                    distinct_plans.len(),
                    human(regrow)
                ),
                format!("category {}, pattern {}", evs[0].category, pattern),
            ],
            suggestion_for(pattern, label),
        ));
    }

    // 2 and 3. Worktrees of one repository each holding their own build output.
    let mut per_repo: BTreeMap<(PathBuf, &str), BTreeMap<String, u64>> = BTreeMap::new();
    for e in events {
        if e.origin.pattern != "build-output" {
            continue;
        }
        let (Some(repo), Some(wt)) = (&e.origin.repo, &e.origin.worktree) else {
            continue;
        };
        let name = match name_of(&e.label) {
            "target" => "target",
            "node_modules" => "node_modules",
            _ => continue,
        };
        let slot = per_repo
            .entry((repo.clone(), name))
            .or_default()
            .entry(wt.clone())
            .or_insert(0);
        *slot = (*slot).max(e.bytes);
    }
    for ((repo, name), wts) in per_repo {
        if wts.len() < 2 {
            continue;
        }
        let total: u64 = wts.values().sum();
        let mut evidence: Vec<(String, u64)> = wts.into_iter().collect();
        evidence.sort_by_key(|(_, b)| std::cmp::Reverse(*b));
        let count = evidence.len();
        let lines = evidence
            .iter()
            .take(8)
            .map(|(w, b)| format!("worktree {w}: {name} {}", human(*b)))
            .collect();
        let (rule, suggestion) = if name == "target" {
            (
                "worktree-targets",
                format!(
                    "Point every worktree of {} at one shared target folder (CARGO_TARGET_DIR, or `[build] target-dir` in the repository's .cargo/config.toml); builds then reuse each other's artifacts instead of {} separate copies.",
                    repo.display(),
                    count
                ),
            )
        } else {
            ("worktree-node-modules", "Add a teardown hook that removes node_modules (and target/) when an agent's worktree ends, and install with a content-addressed store (pnpm) so worktrees hard-link packages.".to_string())
        };
        findings.push(finding(
            rule,
            format!(
                "{count} worktrees of {} each keep their own {name}",
                repo.display()
            ),
            total,
            count,
            lines,
            suggestion,
        ));
    }

    // 4, 5 and 6. Caches, docker and logs cleaned again and again.
    let mut by_pattern: BTreeMap<(&str, String), (usize, u64)> = BTreeMap::new();
    for e in events.iter().filter(|e| e.event == "applied") {
        let key = match e.origin.pattern.as_str() {
            "tool-cache" => ("cache-cap", e.label.clone()),
            "docker" => ("docker-build-cache", "docker".to_string()),
            "log" => (
                "logs",
                e.label
                    .rsplit_once('/')
                    .map_or(e.label.clone(), |(d, _)| d.to_string()),
            ),
            _ => continue,
        };
        let slot = by_pattern.entry(key).or_insert((0, 0));
        slot.0 += 1;
        slot.1 += e.bytes;
    }
    for ((rule, label), (count, bytes)) in by_pattern {
        if count < 2 && rule != "logs" {
            continue;
        }
        let (title, suggestion) = match rule {
            "cache-cap" => (
                format!("{label} was emptied {count} times"),
                cache_cap_hint(&label),
            ),
            "docker-build-cache" => (
                format!("docker images and build cache were pruned {count} times"),
                docker_hint(),
            ),
            _ => (
                format!("{count} old log files removed under {label}"),
                log_hint(),
            ),
        };
        findings.push(finding(
            rule,
            title,
            bytes,
            count,
            vec![format!("{count} cleanings, {} in all", human(bytes))],
            suggestion,
        ));
    }

    // 7. Test runs leaving temp dirs in scratch roots.
    let mut temp_by_root: BTreeMap<String, (usize, u64, Vec<String>)> = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for e in events
        .iter()
        .filter(|e| e.origin.pattern == "scratch" && looks_like_test_temp(name_of(&e.label)))
    {
        if !seen.insert(e.label.as_str()) {
            continue;
        }
        let root = e
            .label
            .rsplit_once('/')
            .map_or(String::new(), |(d, _)| d.to_string());
        let slot = temp_by_root.entry(root).or_insert((0, 0, Vec::new()));
        slot.0 += 1;
        slot.1 += e.bytes;
        if slot.2.len() < 5 {
            slot.2.push(name_of(&e.label).to_string());
        }
    }
    for (root, (count, bytes, names)) in temp_by_root {
        if count < 3 {
            continue;
        }
        findings.push(finding(
            "test-temp-dirs",
            format!("{count} leftover temp folders in {root}"),
            bytes,
            count,
            vec![format!("for example: {}", names.join(", "))],
            "A test harness leaves its temp folders behind: create them with a guard that removes them on drop or exit (TempDir, `trap ... EXIT`), or give each run its own TMPDIR and remove it when the run ends.".to_string(),
        ));
    }

    // 8. Growth between the two newest snapshots of each root.
    let mut roots: BTreeMap<&Path, Vec<&Snapshot>> = BTreeMap::new();
    for s in snapshots {
        roots.entry(s.root.as_path()).or_default().push(s);
    }
    for (_, mut snaps) in roots {
        if snaps.len() < 2 {
            continue;
        }
        snaps.sort_by_key(|s| s.taken);
        let g = compare(snaps[snaps.len() - 2], snaps[snaps.len() - 1], 3);
        for grew in g.grew.iter().filter(|g| g.per_day > 512 * 1024 * 1024) {
            findings.push(finding(
                "growth",
                format!("{} grows about {}/day", grew.path.display(), human(grew.per_day as u64)),
                grew.grew as u64,
                1,
                vec![format!(
                    "{} -> {} over {} hours (snapshots {} and {})",
                    human(grew.before),
                    human(grew.after),
                    g.span_hours,
                    g.older,
                    g.newer
                )],
                "Run big-files on it with changed_hours to see what is writing, then cap or rotate it at the source.".to_string(),
            ));
        }
    }

    findings.sort_by(|a, b| b.score.total_cmp(&a.score));
    findings.truncate(top);
    for (i, f) in findings.iter_mut().enumerate() {
        f.rank = i + 1;
    }
    findings
}

pub fn run(p: &InsightsParams, ctx: &Ctx) -> Result<Insights, JobError> {
    ctx.phase("reading history");
    let since = now().saturating_sub(p.days * 86_400);
    let events: Vec<OriginEvent> = ctx
        .store
        .read_lines::<OriginEvent>(&ctx.store.origins_path())
        .into_iter()
        .filter(|e| e.at >= since)
        .collect();
    let snapshots: Vec<Snapshot> = ctx
        .store
        .list_snapshots::<Snapshot>()
        .into_iter()
        .filter(|s| s.taken >= since)
        .collect();
    Ok(Insights {
        since,
        events: events.len(),
        findings: analyse(&events, &snapshots, p.top),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::clean::Origin;

    fn ev(
        at: u64,
        event: &str,
        plan: &str,
        label: &str,
        bytes: u64,
        origin: Origin,
    ) -> OriginEvent {
        OriginEvent {
            at,
            event: event.into(),
            plan: plan.into(),
            category: "build-outputs".into(),
            label: label.into(),
            bytes,
            origin,
            reason: String::new(),
        }
    }

    fn wt(repo: &str, name: &str) -> Origin {
        Origin {
            pattern: "build-output".into(),
            repo: Some(repo.into()),
            worktree: Some(name.into()),
            ..Origin::default()
        }
    }

    #[test]
    fn worktree_targets_and_recurrence_rank_with_evidence() {
        let g = 1u64 << 30;
        let events = vec![
            ev(
                100,
                "applied",
                "p1",
                "/w/repo/.claude/worktrees/a/target",
                4 * g,
                wt("/w/repo", "a"),
            ),
            ev(
                200,
                "applied",
                "p1",
                "/w/repo/.claude/worktrees/b/target",
                3 * g,
                wt("/w/repo", "b"),
            ),
            ev(
                90_000,
                "planned",
                "p2",
                "/w/repo/.claude/worktrees/a/target",
                5 * g,
                wt("/w/repo", "a"),
            ),
        ];
        let f = analyse(&events, &[], 10);
        let rules: Vec<&str> = f.iter().map(|f| f.rule.as_str()).collect();
        assert!(rules.contains(&"worktree-targets"), "{rules:?}");
        assert!(rules.contains(&"recurring"), "{rules:?}");
        let wt = f.iter().find(|f| f.rule == "worktree-targets").unwrap();
        assert_eq!(wt.occurrences, 2);
        assert!(wt.suggestion.contains("CARGO_TARGET_DIR"));
        assert!(wt.evidence.iter().any(|e| e.contains("worktree a")));
        assert_eq!(f[0].rank, 1);
    }

    #[test]
    fn temp_dir_names() {
        assert!(looks_like_test_temp("tmp.Xa81kd"));
        assert!(looks_like_test_temp("pytest-of-acme"));
        assert!(looks_like_test_temp("build-a8fk3x9q"));
        assert!(!looks_like_test_temp("huggingface"));
        assert!(!looks_like_test_temp("my-project"));
    }
}
