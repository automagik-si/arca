//! Clean plans and their application.
//!
//! A plan lists every item with its path (or command), category, bytes, reason and origin, and the guards it must
//! pass. Applying a plan builds the protections again from scratch and checks every guard of every item again: an
//! item that no longer qualifies is skipped with the reason, never applied.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::categories;
use crate::config::Config;
use crate::jobs::{CleanApplyParams, CleanPlanParams, Ctx, JobError, Profile, TrashPurgeParams};
use crate::platform;
use crate::protect::{self, Protections};
use crate::util::{now, on_path, parse_date, run_cmd};
use crate::walk::{changed_since, remove_tree};

/// What applying an item does.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Action {
    /// Delete the file, link or folder.
    Remove { path: PathBuf },
    /// Move the folder into `<trash>/<date>/` (a rename: instant and undoable).
    Quarantine { path: PathBuf, trash: PathBuf },
    /// Delete what is inside the folder, keep the folder.
    EmptyDir { path: PathBuf },
    /// Run a tool's own clean command (no shell).
    Command { argv: Vec<String> },
}

impl Action {
    pub fn path(&self) -> Option<&Path> {
        match self {
            Action::Remove { path }
            | Action::Quarantine { path, .. }
            | Action::EmptyDir { path } => Some(path),
            Action::Command { .. } => None,
        }
    }

    pub fn label(&self) -> String {
        match self {
            Action::Command { argv } => argv.join(" "),
            other => other
                .path()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        }
    }
}

/// A check an item must pass when it is planned and again when it is applied.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "guard", rename_all = "kebab-case")]
pub enum Guard {
    /// Not in, inside or around anything a service, container, live process, PROTECT or .cleanup-keep marks.
    Protected,
    /// Not inside the home's tool dot-folders.
    NotToolHome,
    /// No live process holds a path inside it, and no PROTECT path is in, inside or around it.
    NotLive,
    /// None of these programs runs for this user.
    OwnersIdle { names: Vec<String> },
    /// No live process runs this file (or one inside this folder).
    NotRunning,
    /// The project and the output are idle: nothing changed for `days`.
    ProjectIdle { project: PathBuf, days: u64 },
    /// Nothing at or below the path changed for `days`.
    Untouched { days: u64 },
    /// The trash day folder's date is at least `days` old.
    TrashOlderThan { days: u64 },
    /// The folder's name is not in CACHE_KEEP.
    NotCacheKeep,
    /// The program is installed.
    Tool { program: String },
}

/// Where an item came from: the pattern that matched it, and whose it is.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Origin {
    /// tool-cache, build-output, scratch, trash, docker, old-build, log or app-cache.
    pub pattern: String,
    /// The repository or project folder holding it.
    pub project: Option<PathBuf>,
    /// The worktree's name, when the project is a git worktree.
    pub worktree: Option<String>,
    /// The repository a worktree belongs to.
    pub repo: Option<PathBuf>,
    /// The tool or process that made it, when known.
    pub creator: Option<String>,
}

/// Find the project (the nearest folder above holding `.git`) and, for a worktree, its repository.
pub fn origin_of(path: &Path, pattern: &str, creator: Option<&str>, home: &Path) -> Origin {
    let mut origin = Origin {
        pattern: pattern.to_string(),
        creator: creator.map(str::to_string),
        ..Origin::default()
    };
    let mut dir = path.parent();
    while let Some(d) = dir {
        if d == home || d.parent().is_none() {
            break;
        }
        let git = d.join(".git");
        if let Ok(md) = fs::symlink_metadata(&git) {
            origin.project = Some(d.to_path_buf());
            if md.is_file() {
                let text = fs::read_to_string(&git).unwrap_or_default();
                let gitdir = text.trim().strip_prefix("gitdir:").unwrap_or("").trim();
                if let Some(at) = gitdir.find("/.git/worktrees/") {
                    origin.repo = Some(PathBuf::from(&gitdir[..at]));
                    origin.worktree = d.file_name().map(|n| n.to_string_lossy().to_string());
                }
            } else {
                origin.repo = Some(d.to_path_buf());
            }
            break;
        }
        dir = d.parent();
    }
    origin
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Item {
    pub id: usize,
    pub category: String,
    pub action: Action,
    pub bytes: u64,
    pub reason: String,
    pub guards: Vec<Guard>,
    pub origin: Origin,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Skip {
    pub category: String,
    pub label: String,
    pub bytes: u64,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Plan {
    pub id: String,
    pub created: u64,
    pub host: String,
    pub os: String,
    pub profile: Profile,
    pub only: Vec<String>,
    pub items: Vec<Item>,
    pub skipped: Vec<Skip>,
    /// Bytes the removals and commands would free (measured at plan time; nothing is freed by a plan).
    pub would_free_bytes: u64,
    /// Bytes that would move to trash (freed after TRASH_DAYS).
    pub would_trash_bytes: u64,
    /// The job that applied it, once applied.
    pub applied_by: Option<String>,
}

/// What categories see while planning: the config, the protections and the job's context.
pub struct Env<'a> {
    pub config: &'a Config,
    pub prot: Protections,
    pub ctx: Option<&'a Ctx>,
    pub now: u64,
    pub profile: Profile,
}

impl Env<'_> {
    pub fn uid(&self) -> u32 {
        self.prot.uid
    }
    pub fn mounts(&self) -> &HashSet<PathBuf> {
        &self.prot.mounts
    }
    pub fn checkpoint(&self) -> Result<(), crate::jobs::Stop> {
        match self.ctx {
            Some(ctx) => ctx.checkpoint(),
            None => Ok(()),
        }
    }
}

/// Build the protections from the live system.
pub fn build_env<'a>(
    config: &'a Config,
    ctx: Option<&'a Ctx>,
    profile: Profile,
) -> Result<Env<'a>, JobError> {
    let uid = platform::euid();
    if uid == 0 {
        return Err(JobError::Failed(
            "refused: run as a user, never as root".to_string(),
        ));
    }
    let mounts = platform::mount_points();
    let sources = protect::gather(config, uid, &mounts, ctx)?;
    let prot = Protections::new(config, uid, mounts, sources);
    Ok(Env {
        config,
        prot,
        ctx,
        now: now(),
        profile,
    })
}

fn odd(path: &Path) -> bool {
    !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
}

/// The checks every path item passes: a plain absolute path, still there, this user's, not a link, not a mount.
pub fn basic_check(path: &Path, env: &Env) -> Result<(), String> {
    if odd(path) {
        return Err("refused: odd path".to_string());
    }
    let md = fs::symlink_metadata(path).map_err(|_| "gone".to_string())?;
    if md.uid() != env.uid() {
        return Err("refused: not owned by this user".to_string());
    }
    if md.file_type().is_symlink() {
        return Err("refused: a link".to_string());
    }
    if md.is_dir() && platform::is_mount_point(path, env.mounts()) {
        return Err("refused: a mount point".to_string());
    }
    Ok(())
}

/// Check an item now: the basic checks for a path, then each of its guards.
pub fn check(item: &Item, env: &Env) -> Result<(), String> {
    if let Some(path) = item.action.path() {
        basic_check(path, env)?;
    }
    if let Action::Quarantine { trash, .. } = &item.action
        && fs::symlink_metadata(trash).is_ok_and(|m| m.file_type().is_symlink())
    {
        return Err("refused: the trash folder is a link".to_string());
    }
    let path = item.action.path().unwrap_or(Path::new(""));
    let mounts = env.mounts();
    for guard in &item.guards {
        match guard {
            Guard::Protected => {
                if let Some(why) = env.prot.protected_by(path) {
                    return Err(why);
                }
            }
            Guard::NotToolHome => {
                if env.prot.in_tool_home(path) {
                    return Err("tool folder of the home (global installs)".to_string());
                }
            }
            Guard::NotLive => {
                if let Some(live) = env.prot.live_inside(path) {
                    return Err(format!("in use by a live process ({})", live.display()));
                }
                if let Some(x) = env
                    .prot
                    .protect
                    .iter()
                    .find(|x| path == x.as_path() || path.starts_with(x) || x.starts_with(path))
                {
                    return Err(format!("protected (PROTECT: {})", x.display()));
                }
                if path.join(".cleanup-keep").exists() {
                    return Err("protected (.cleanup-keep)".to_string());
                }
            }
            Guard::OwnersIdle { names } => {
                let names: Vec<&str> = names.iter().map(String::as_str).collect();
                if let Some(who) = platform::running(env.uid(), &names) {
                    return Err(format!("skipped: {who} is running"));
                }
            }
            Guard::NotRunning => {
                if env.prot.exe_in_use(path) {
                    return Err("kept: a live process runs it".to_string());
                }
            }
            Guard::ProjectIdle { project, days } => {
                let cutoff = env.now.saturating_sub(days * 86_400);
                let recent = changed_since(
                    project,
                    cutoff,
                    Some(3),
                    &["target", "node_modules", ".git"],
                    mounts,
                ) || changed_since(&project.join(".git"), cutoff, None, &[], mounts)
                    || changed_since(path, cutoff, Some(2), &[], mounts);
                if recent {
                    return Err(format!("changed in the last {days} days"));
                }
            }
            Guard::Untouched { days } => {
                let cutoff = env.now.saturating_sub(days * 86_400);
                if changed_since(path, cutoff, None, &[], mounts) {
                    return Err(format!("changed in the last {days} days"));
                }
            }
            Guard::TrashOlderThan { days } => {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                let Some(day) = parse_date(name) else {
                    return Err("not a trash day folder".to_string());
                };
                if env.now.saturating_sub(day) / 86_400 < *days {
                    return Err(format!("trash younger than {days} days"));
                }
            }
            Guard::NotCacheKeep => {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if env.config.cache_keep.iter().any(|k| k == name) {
                    return Err("kept warm (CACHE_KEEP)".to_string());
                }
            }
            Guard::Tool { program } => {
                if !on_path(program) {
                    return Err(format!("{program} is not installed"));
                }
            }
        }
    }
    Ok(())
}

/// Make a plan: run the categories, check each candidate, and keep what passes.
pub fn make_plan(
    config: &Config,
    ctx: Option<&Ctx>,
    params: &CleanPlanParams,
) -> Result<Plan, JobError> {
    let all = categories::names();
    for c in &params.only {
        if !all.contains(&c.as_str()) {
            return Err(JobError::Failed(format!(
                "unknown category {c} (this system has: {})",
                all.join(", ")
            )));
        }
    }
    if let Some(ctx) = ctx {
        ctx.phase("working out what is protected");
    }
    let env = build_env(config, ctx, params.profile)?;
    let mut plan = Plan {
        id: crate::util::new_id("p"),
        created: env.now,
        host: config.host.clone(),
        os: std::env::consts::OS.to_string(),
        profile: params.profile,
        only: params.only.clone(),
        items: Vec::new(),
        skipped: Vec::new(),
        would_free_bytes: 0,
        would_trash_bytes: 0,
        applied_by: None,
    };
    for name in all {
        if !params.only.is_empty() && !params.only.iter().any(|c| c == name) {
            continue;
        }
        if let Some(ctx) = ctx {
            ctx.phase(&format!("planning {name}"));
        }
        let mut found = categories::run(name, &env)?;
        for skip in found.skipped.drain(..) {
            plan.skipped.push(skip);
        }
        for mut item in found.items {
            env.checkpoint()?;
            match check(&item, &env) {
                Ok(()) => {
                    item.id = plan.items.len();
                    if matches!(item.action, Action::Quarantine { .. }) {
                        plan.would_trash_bytes += item.bytes;
                    } else {
                        plan.would_free_bytes += item.bytes;
                    }
                    if let Some(ctx) = ctx {
                        ctx.tick(1, item.bytes);
                    }
                    plan.items.push(item);
                }
                Err(reason) => plan.skipped.push(Skip {
                    category: item.category,
                    label: item.action.label(),
                    bytes: item.bytes,
                    reason,
                }),
            }
        }
    }
    Ok(plan)
}

/// One line of origins.jsonl.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct OriginEvent {
    pub at: u64,
    /// planned, applied or skipped.
    pub event: String,
    pub plan: String,
    pub category: String,
    pub label: String,
    pub bytes: u64,
    pub origin: Origin,
    pub reason: String,
}

fn record_origins(ctx: &Ctx, plan: &Plan, event: &str, items: &[(&Item, String)]) {
    let path = ctx.store.origins_path();
    for (item, reason) in items {
        let written = ctx.store.append(
            &path,
            &OriginEvent {
                at: now(),
                event: event.to_string(),
                plan: plan.id.clone(),
                category: item.category.clone(),
                label: item.action.label(),
                bytes: item.bytes,
                origin: item.origin.clone(),
                reason: reason.clone(),
            },
        );
        if let Err(e) = written {
            ctx.log(format!("ERROR cannot write {}: {e}", path.display()));
            return;
        }
    }
}

/// The clean-plan job: make a plan, store it, record its items' origins.
pub fn plan_job(params: &CleanPlanParams, ctx: &Ctx) -> Result<PlanSummary, JobError> {
    let plan = make_plan(&ctx.config, Some(ctx), params)?;
    ctx.store
        .save_plan(&plan.id, &plan)
        .map_err(|e| JobError::Failed(format!("cannot store the plan: {e}")))?;
    let planned: Vec<(&Item, String)> = plan.items.iter().map(|i| (i, i.reason.clone())).collect();
    record_origins(ctx, &plan, "planned", &planned);
    Ok(PlanSummary::of(&plan))
}

/// What a plan job returns: the plan id and its totals (the plan file holds every item).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PlanSummary {
    pub plan: String,
    pub profile: Profile,
    pub items: usize,
    pub skipped: usize,
    pub would_free_bytes: u64,
    pub would_trash_bytes: u64,
    pub by_category: BTreeMap<String, CategoryTotal>,
    pub sample: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct CategoryTotal {
    pub items: usize,
    pub bytes: u64,
}

impl PlanSummary {
    pub fn of(plan: &Plan) -> PlanSummary {
        let mut by_category: BTreeMap<String, CategoryTotal> = BTreeMap::new();
        for item in &plan.items {
            let t = by_category.entry(item.category.clone()).or_default();
            t.items += 1;
            t.bytes += item.bytes;
        }
        let mut biggest: Vec<&Item> = plan.items.iter().collect();
        biggest.sort_by_key(|i| std::cmp::Reverse(i.bytes));
        PlanSummary {
            plan: plan.id.clone(),
            profile: plan.profile,
            items: plan.items.len(),
            skipped: plan.skipped.len(),
            would_free_bytes: plan.would_free_bytes,
            would_trash_bytes: plan.would_trash_bytes,
            by_category,
            sample: biggest
                .iter()
                .take(12)
                .map(|i| {
                    format!(
                        "{} {} ({})",
                        i.category,
                        i.action.label(),
                        crate::util::human(i.bytes)
                    )
                })
                .collect(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Applied {
    pub id: usize,
    pub category: String,
    pub label: String,
    pub bytes: u64,
    /// Where a quarantined folder went (move it back to undo).
    pub moved_to: Option<PathBuf>,
}

/// An item whose action failed, or whose result could not be seen on disk afterwards.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Failed {
    pub id: usize,
    pub category: String,
    pub label: String,
    /// The bytes it was planned to free: not freed.
    pub bytes: u64,
    /// The OS error, exit status or failed verification.
    pub error: String,
}

/// What a clean run did, item by item. Every `done` item was verified on disk afterwards (a removed path is gone, a
/// moved folder is at its destination and not at its source, an emptied folder is empty, a command exited 0); only
/// those count as freed.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ApplyResult {
    pub plan: String,
    /// A dry run changes nothing: its items are in `would`, and nothing is counted as freed.
    pub dry_run: bool,
    pub done: Vec<Applied>,
    pub skipped: Vec<Skip>,
    pub failed: Vec<Failed>,
    pub would: Vec<Applied>,
    /// Verified bytes freed.
    pub freed_bytes: u64,
    /// Verified bytes moved to trash.
    pub trash_bytes: u64,
    /// Bytes of failed items, not freed.
    pub failed_bytes: u64,
    /// Dry run only.
    pub would_free_bytes: u64,
    /// Errors of the run itself (the plan or the history could not be written).
    pub errors: Vec<String>,
    /// Set when the run stopped before the end (cancelled, budget).
    pub stopped: Option<String>,
}

impl ApplyResult {
    /// Why this run must end as failed, if it must: any failed item or run error.
    pub fn failure(&self) -> Option<String> {
        if self.failed.is_empty() && self.errors.is_empty() {
            return None;
        }
        let first = self
            .failed
            .first()
            .map(|f| format!("{}: {}", f.label, f.error))
            .or_else(|| self.errors.first().cloned())
            .unwrap_or_default();
        Some(format!(
            "{} items failed ({} not freed), {} run errors; {} done and verified ({} freed or trashed); first: {first}",
            self.failed.len(),
            crate::util::human(self.failed_bytes),
            self.errors.len(),
            self.done.len(),
            crate::util::human(self.freed_bytes + self.trash_bytes),
        ))
    }
}

/// One line of history.jsonl: what one clean run did.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct RunRecord {
    pub at: u64,
    pub job: String,
    pub kind: String,
    pub host: String,
    pub plan: Option<String>,
    pub dry_run: bool,
    /// Items done and verified.
    pub done: usize,
    pub freed_bytes: u64,
    pub trash_bytes: u64,
    pub skipped: usize,
    pub skip_reasons: BTreeMap<String, usize>,
    pub failed: usize,
    pub failed_bytes: u64,
    /// The first few failures and run errors, word for word.
    pub errors: Vec<String>,
    pub refused: Option<String>,
    pub stopped: Option<String>,
}

/// A skip reason without its particulars, so the report can count them: `protected (live process: /x)` becomes
/// `protected (live process)`.
pub fn reason_key(reason: &str) -> String {
    match reason.split_once(':') {
        Some((head, _)) if head.contains('(') => format!("{head})"),
        Some((head, _)) => head.to_string(),
        None => reason.to_string(),
    }
}

pub fn record_run(
    ctx: &Ctx,
    kind: &str,
    result: &ApplyResult,
    dry_run: bool,
    refused: Option<String>,
) -> Result<(), String> {
    let mut reasons = BTreeMap::new();
    for s in &result.skipped {
        *reasons.entry(reason_key(&s.reason)).or_insert(0) += 1;
    }
    let mut errors: Vec<String> = result
        .failed
        .iter()
        .map(|f| format!("{}: {}", f.label, f.error))
        .collect();
    errors.extend(result.errors.iter().cloned());
    errors.truncate(5);
    let record = RunRecord {
        at: now(),
        job: ctx.with_job(|j| j.id.clone()),
        kind: kind.to_string(),
        host: ctx.config.host.clone(),
        plan: (!result.plan.is_empty()).then(|| result.plan.clone()),
        dry_run,
        done: result.done.len(),
        freed_bytes: result.freed_bytes,
        trash_bytes: result.trash_bytes,
        skipped: result.skipped.len(),
        skip_reasons: reasons,
        failed: result.failed.len(),
        failed_bytes: result.failed_bytes,
        errors,
        refused,
        stopped: result.stopped.clone(),
    };
    let path = ctx.store.history_path();
    ctx.store
        .append(&path, &record)
        .map_err(|e| format!("cannot write the history ({}): {e}", path.display()))
}

/// Apply one item. Returns where a quarantined folder went.
fn execute(item: &Item, env: &Env, ctx: Option<&Ctx>) -> Result<Option<PathBuf>, String> {
    match &item.action {
        Action::Remove { path } => remove_tree(path, ctx)
            .map(|_| None)
            .map_err(|e| e.to_string()),
        Action::EmptyDir { path } => {
            let entries = fs::read_dir(path).map_err(|e| e.to_string())?;
            for entry in entries.flatten() {
                let child = entry.path();
                remove_tree(&child, ctx).map_err(|e| format!("{}: {e}", child.display()))?;
            }
            Ok(None)
        }
        Action::Quarantine { path, trash } => {
            let day = trash.join(crate::util::date(env.now));
            fs::create_dir_all(&day).map_err(|e| format!("cannot make {}: {e}", day.display()))?;
            let name = path.file_name().ok_or("no name")?;
            let mut dest = day.join(name);
            if fs::symlink_metadata(&dest).is_ok() {
                let (_, _, _, h, m, s) = crate::util::civil(now());
                dest = day.join(format!("{}.{h:02}{m:02}{s:02}", name.to_string_lossy()));
            }
            fs::rename(path, &dest)
                .map_err(|e| format!("cannot move to {}: {e}", dest.display()))?;
            Ok(Some(dest))
        }
        Action::Command { argv } => {
            let args: Vec<&str> = argv.iter().map(String::as_str).collect();
            match run_cmd(&args, Duration::from_secs(30 * 60)) {
                Some(out) if out.success => Ok(None),
                Some(out) if out.timed_out => Err("timed out and was killed".to_string()),
                Some(out) => Err(format!(
                    "exit status {}: {}",
                    out.status
                        .map_or("none (killed by a signal)".to_string(), |c| c.to_string()),
                    out.stderr.lines().last().unwrap_or("")
                )),
                None => Err("cannot start the program".to_string()),
            }
        }
    }
}

/// Look at the disk after an action: did it really happen?
pub fn verify(action: &Action, moved_to: Option<&Path>) -> Result<(), String> {
    let gone = |p: &Path| matches!(fs::symlink_metadata(p), Err(e) if e.kind() == std::io::ErrorKind::NotFound);
    match action {
        Action::Remove { path } => {
            if gone(path) {
                Ok(())
            } else {
                Err("still there after the removal".to_string())
            }
        }
        Action::EmptyDir { path } => match fs::read_dir(path).map(|mut e| e.next().is_none()) {
            Ok(true) => Ok(()),
            Ok(false) => Err("not empty after emptying".to_string()),
            Err(e) => Err(format!("cannot verify the emptying: {e}")),
        },
        Action::Quarantine { path, .. } => {
            let dest_there = moved_to.is_some_and(|d| fs::symlink_metadata(d).is_ok());
            match (gone(path), dest_there) {
                (true, true) => Ok(()),
                (false, _) => Err("still at its place after the move".to_string()),
                (true, false) => Err("gone from its place but not found in the trash".to_string()),
            }
        }
        Action::Command { .. } => Ok(()),
    }
}

/// Apply a plan: every item is checked again against protections built anew, then applied or skipped.
pub fn apply_plan(
    plan: &Plan,
    config: &Config,
    ctx: Option<&Ctx>,
) -> Result<ApplyResult, JobError> {
    if plan.host != config.host {
        return Err(JobError::Failed(format!(
            "refused: plan {} was made on {}, this is {}",
            plan.id, plan.host, config.host
        )));
    }
    if let Some(ctx) = ctx {
        ctx.phase("working out what is protected, again");
    }
    let env = build_env(config, ctx, plan.profile)?;
    let mut result = ApplyResult {
        plan: plan.id.clone(),
        ..ApplyResult::default()
    };
    let stop_reason = |stop: crate::jobs::Stop| {
        match stop {
            crate::jobs::Stop::Cancelled => "cancelled",
            crate::jobs::Stop::Budget => "time budget spent",
        }
        .to_string()
    };
    // Every verdict is decided before anything is changed: removing one item touches its parent folder, and a
    // sibling checked after that would look freshly changed and be skipped. The run never judges recency on a
    // tree it has already started to change.
    if let Some(ctx) = ctx {
        ctx.phase("checking every item before changing anything");
    }
    let mut passed = Vec::new();
    for item in &plan.items {
        if let Err(stop) = env.checkpoint() {
            result.stopped = Some(stop_reason(stop));
            return Ok(result);
        }
        match check(item, &env) {
            Ok(()) => passed.push(item),
            Err(reason) => result.skipped.push(Skip {
                category: item.category.clone(),
                label: item.action.label(),
                bytes: item.bytes,
                reason,
            }),
        }
    }
    if let Some(ctx) = ctx {
        ctx.phase("applying");
    }
    for item in passed {
        if let Err(stop) = env.checkpoint() {
            result.stopped = Some(stop_reason(stop));
            break;
        }
        // Right before acting, only what the disk itself says is looked at again: still there, still ours, not a
        // link, not a mount. The protections were built once for this run and do not change within it.
        if let Some(path) = item.action.path()
            && let Err(reason) = basic_check(path, &env)
        {
            result.skipped.push(Skip {
                category: item.category.clone(),
                label: item.action.label(),
                bytes: item.bytes,
                reason,
            });
            continue;
        }
        let outcome = execute(item, &env, ctx)
            .and_then(|moved_to| verify(&item.action, moved_to.as_deref()).map(|()| moved_to));
        match outcome {
            Ok(moved_to) => {
                if moved_to.is_some() {
                    result.trash_bytes += item.bytes;
                } else {
                    result.freed_bytes += item.bytes;
                }
                if let Some(ctx) = ctx {
                    ctx.tick(1, item.bytes);
                    match &moved_to {
                        Some(dest) => ctx.log(format!(
                            "quarantined {} -> {} (verified)",
                            item.action.label(),
                            dest.display()
                        )),
                        None => ctx.log(format!(
                            "{} {} (verified)",
                            verb(&item.action),
                            item.action.label()
                        )),
                    }
                }
                result.done.push(Applied {
                    id: item.id,
                    category: item.category.clone(),
                    label: item.action.label(),
                    bytes: item.bytes,
                    moved_to,
                });
            }
            Err(error) => {
                if let Some(ctx) = ctx {
                    ctx.log(format!(
                        "FAILED {} {}: {error}",
                        verb_base(&item.action),
                        item.action.label()
                    ));
                }
                result.failed_bytes += item.bytes;
                result.failed.push(Failed {
                    id: item.id,
                    category: item.category.clone(),
                    label: item.action.label(),
                    bytes: item.bytes,
                    error,
                });
            }
        }
    }
    Ok(result)
}

fn verb_base(action: &Action) -> &'static str {
    match action {
        Action::Remove { .. } => "remove",
        Action::EmptyDir { .. } => "empty",
        Action::Quarantine { .. } => "quarantine",
        Action::Command { .. } => "run",
    }
}

fn verb(action: &Action) -> &'static str {
    match action {
        Action::Remove { .. } => "removed",
        Action::EmptyDir { .. } => "emptied",
        Action::Quarantine { .. } => "quarantined",
        Action::Command { .. } => "ran",
    }
}

/// The clean-apply job: apply exactly the stored plan, once.
pub fn apply_job(params: &CleanApplyParams, ctx: &Ctx) -> Result<ApplyResult, JobError> {
    let mut plan: Plan = ctx
        .store
        .load_plan(&params.plan)
        .map_err(|e| JobError::Failed(format!("no plan {}: {e}", params.plan)))?;
    apply_stored(&mut plan, ctx, "clean-apply")
}

/// Apply a stored plan, mark it applied, record history and origins.
pub fn apply_stored(plan: &mut Plan, ctx: &Ctx, kind: &str) -> Result<ApplyResult, JobError> {
    if let Some(by) = &plan.applied_by {
        return Err(JobError::Failed(format!(
            "refused: plan {} was already applied by job {by}; make a new plan",
            plan.id
        )));
    }
    let mut result = apply_plan(plan, &ctx.config, Some(ctx))?;
    plan.applied_by = Some(ctx.with_job(|j| j.id.clone()));
    if let Err(e) = ctx.store.save_plan(&plan.id, plan) {
        result
            .errors
            .push(format!("cannot mark plan {} applied: {e}", plan.id));
    }
    let by_id: BTreeMap<usize, &Item> = plan.items.iter().map(|i| (i.id, i)).collect();
    let applied: Vec<(&Item, String)> = result
        .done
        .iter()
        .filter_map(|a| by_id.get(&a.id).map(|i| (*i, i.reason.clone())))
        .collect();
    record_origins(ctx, plan, "applied", &applied);
    let skipped: Vec<(&Item, String)> = result
        .skipped
        .iter()
        .filter_map(|s| {
            plan.items
                .iter()
                .find(|i| i.action.label() == s.label)
                .map(|i| (i, s.reason.clone()))
        })
        .collect();
    record_origins(ctx, plan, "skipped", &skipped);
    let failed: Vec<(&Item, String)> = result
        .failed
        .iter()
        .filter_map(|f| by_id.get(&f.id).map(|i| (*i, f.error.clone())))
        .collect();
    record_origins(ctx, plan, "failed", &failed);
    if let Err(e) = record_run(ctx, kind, &result, false, None) {
        ctx.log(format!("ERROR {e}"));
        result.errors.push(e);
    }
    Ok(result)
}

/// The trash-purge job: delete quarantine day folders older than TRASH_DAYS in every scratch root.
pub fn trash_purge(params: &TrashPurgeParams, ctx: &Ctx) -> Result<ApplyResult, JobError> {
    ctx.phase("finding old trash");
    let env = build_env(&ctx.config, Some(ctx), Profile::Full)?;
    let mut plan = Plan {
        id: crate::util::new_id("p"),
        created: env.now,
        host: ctx.config.host.clone(),
        os: std::env::consts::OS.to_string(),
        profile: Profile::Full,
        only: vec!["trash".to_string()],
        items: categories::old_trash(&env)?,
        skipped: Vec::new(),
        would_free_bytes: 0,
        would_trash_bytes: 0,
        applied_by: None,
    };
    for (i, item) in plan.items.iter_mut().enumerate() {
        item.id = i;
        plan.would_free_bytes += item.bytes;
    }
    if params.dry_run {
        let result = ApplyResult {
            plan: plan.id.clone(),
            dry_run: true,
            would_free_bytes: plan.would_free_bytes,
            would: plan
                .items
                .iter()
                .map(|i| Applied {
                    id: i.id,
                    category: i.category.clone(),
                    label: i.action.label(),
                    bytes: i.bytes,
                    moved_to: None,
                })
                .collect(),
            ..ApplyResult::default()
        };
        return Ok(result);
    }
    ctx.store
        .save_plan(&plan.id, &plan)
        .map_err(|e| JobError::Failed(format!("cannot store the plan: {e}")))?;
    apply_stored(&mut plan, ctx, "trash-purge")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reason_keys_group_like_reasons() {
        assert_eq!(
            reason_key("protected (live process: /a/b)"),
            "protected (live process)"
        );
        assert_eq!(
            reason_key("changed in the last 14 days"),
            "changed in the last 14 days"
        );
        assert_eq!(reason_key("skipped: npm is running"), "skipped");
    }

    #[test]
    fn worktree_origins_name_the_repo() {
        let t = crate::testutil::TempDir::new("origin");
        let repo = t.path().join("repo");
        let wt = repo.join(".claude/worktrees/feat-x");
        fs::create_dir_all(repo.join(".git/worktrees/feat-x")).unwrap();
        fs::create_dir_all(wt.join("target")).unwrap();
        fs::write(
            wt.join(".git"),
            format!("gitdir: {}/.git/worktrees/feat-x\n", repo.display()),
        )
        .unwrap();
        let o = origin_of(&wt.join("target"), "build-output", None, t.path());
        assert_eq!(o.project.as_deref(), Some(wt.as_path()));
        assert_eq!(o.worktree.as_deref(), Some("feat-x"));
        assert_eq!(o.repo.as_deref(), Some(repo.as_path()));
        let o = origin_of(&repo.join("target"), "build-output", None, t.path());
        assert_eq!(o.repo.as_deref(), Some(repo.as_path()));
        assert_eq!(o.worktree, None);
    }
}
