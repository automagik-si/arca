//! nightly: plan the daily profile, judge the plan, apply it only when nothing in it looks wrong.
//!
//! The plan is refused as a whole when it would free more than NIGHTLY_MAX_GB, move more than NIGHTLY_MAX_TRASH_GB to
//! trash, or when any path in it holds a stop word (STOP_WORDS: `release`, `backup`, `archive`... plus whatever the host's
//! config adds). These limits are deliberate: a person looks at a refused plan, not a program.

use serde::{Deserialize, Serialize};

use crate::jobs::clean::{
    ApplyResult, Plan, PlanSummary, apply_stored, make_plan, reason_key, record_run,
};
use crate::jobs::{CleanPlanParams, Ctx, JobError, NightlyParams, Profile};
use crate::util::human;

const GIB: u64 = 1024 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct NightlyReport {
    pub plan: String,
    pub dry_run: bool,
    pub refused: Option<String>,
    pub planned: PlanSummary,
    pub applied: Option<ApplyResult>,
    /// The short report, a few lines.
    pub lines: Vec<String>,
}

/// Why a nightly plan must not be applied, if it must not.
pub fn judge(plan: &Plan, max_gb: u64, max_trash_gb: u64, stop_words: &[String]) -> Option<String> {
    if plan.would_free_bytes > max_gb * GIB {
        return Some(format!(
            "would free {}, above the {max_gb} GB limit",
            human(plan.would_free_bytes)
        ));
    }
    if plan.would_trash_bytes > max_trash_gb * GIB {
        return Some(format!(
            "would move {} to trash, above the {max_trash_gb} GB limit",
            human(plan.would_trash_bytes)
        ));
    }
    let hits: Vec<String> = plan
        .items
        .iter()
        .filter_map(|i| {
            let label = i.action.label().to_lowercase();
            stop_words
                .iter()
                .find(|w| !w.is_empty() && label.contains(&w.to_lowercase()))
                .map(|w| format!("{} ({w})", i.action.label()))
        })
        .take(5)
        .collect();
    if !hits.is_empty() {
        return Some(format!("stop word in the plan: {}", hits.join(", ")));
    }
    None
}

pub fn run(p: &NightlyParams, ctx: &Ctx) -> Result<NightlyReport, JobError> {
    let params = CleanPlanParams {
        profile: Profile::Daily,
        only: Vec::new(),
    };
    let mut plan = make_plan(&ctx.config, Some(ctx), &params)?;
    ctx.store
        .save_plan(&plan.id, &plan)
        .map_err(|e| JobError::Failed(format!("cannot store the plan: {e}")))?;
    let planned = PlanSummary::of(&plan);
    let c = &ctx.config;
    let refused = judge(
        &plan,
        c.nightly_max_gb,
        c.nightly_max_trash_gb,
        &c.stop_words,
    );
    let mut lines = vec![format!(
        "nightly on {}: plan {} has {} items; it would free {} and move {} to trash; {} skipped",
        c.host,
        plan.id,
        plan.items.len(),
        human(plan.would_free_bytes),
        human(plan.would_trash_bytes),
        plan.skipped.len()
    )];
    if let Some(why) = &refused {
        lines.push(format!("REFUSED, nothing applied: {why}"));
        let empty = ApplyResult {
            plan: plan.id.clone(),
            ..ApplyResult::default()
        };
        if let Err(e) = record_run(ctx, "nightly", &empty, p.dry_run, Some(why.clone())) {
            lines.push(format!("ERROR {e}"));
        }
        return Ok(NightlyReport {
            plan: plan.id.clone(),
            dry_run: p.dry_run,
            refused,
            planned,
            applied: None,
            lines,
        });
    }
    if p.dry_run {
        lines.push(
            "dry run: nothing applied; the numbers above are what an apply would do".to_string(),
        );
        return Ok(NightlyReport {
            plan: plan.id.clone(),
            dry_run: true,
            refused: None,
            planned,
            applied: None,
            lines,
        });
    }
    let result = apply_stored(&mut plan, ctx, "nightly")?;
    let mut reasons: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for s in &result.skipped {
        *reasons.entry(reason_key(&s.reason)).or_insert(0) += 1;
    }
    lines.push(format!(
        "done and verified: {} items, freed {}, moved {} to trash",
        result.done.len(),
        human(result.freed_bytes),
        human(result.trash_bytes)
    ));
    if !result.skipped.is_empty() {
        let why: Vec<String> = reasons.iter().map(|(r, n)| format!("{n} {r}")).collect();
        lines.push(format!("skipped at apply: {}", why.join("; ")));
    }
    if let Some(first) = result.failed.first() {
        lines.push(format!(
            "FAILED: {} items ({} not freed), first: {}: {}",
            result.failed.len(),
            human(result.failed_bytes),
            first.label,
            first.error
        ));
    }
    for e in &result.errors {
        lines.push(format!("ERROR {e}"));
    }
    if let Some(stopped) = &result.stopped {
        lines.push(format!("stopped early: {stopped}"));
    }
    Ok(NightlyReport {
        plan: plan.id.clone(),
        dry_run: false,
        refused: None,
        planned,
        applied: Some(result),
        lines,
    })
}
