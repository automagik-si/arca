//! Jobs: their typed parameters, their record, and the context every job runs in.
//!
//! A job's context is how it stays polite: every walk loop calls [`Ctx::checkpoint`], which stops on a cancel or a
//! spent time budget, and pauses (with growing waits) while the machine's IO pressure is above the threshold.

pub mod bigfiles;
pub mod clean;
pub mod growth;
pub mod insights;
pub mod nightly;
pub mod space;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::Config;
use crate::platform::{Priority, pressure};
use crate::store::Store;
use crate::util::now;

// ------------------------------------------------------------------------------------------------ parameters

/// One job's kind and parameters. Each kind maps one-to-one to a verb (`arca-janitor schema` prints them).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", content = "params", rename_all = "kebab-case")]
pub enum JobParams {
    SpaceReport(SpaceReportParams),
    Growth(GrowthParams),
    BigFiles(BigFilesParams),
    CleanPlan(CleanPlanParams),
    CleanApply(CleanApplyParams),
    TrashPurge(TrashPurgeParams),
    Nightly(NightlyParams),
    Insights(InsightsParams),
    Probe(ProbeParams),
}

pub const KINDS: [&str; 9] = [
    "space-report",
    "growth",
    "big-files",
    "clean-plan",
    "clean-apply",
    "trash-purge",
    "nightly",
    "insights",
    "probe",
];

impl JobParams {
    pub fn kind(&self) -> &'static str {
        match self {
            JobParams::SpaceReport(_) => "space-report",
            JobParams::Growth(_) => "growth",
            JobParams::BigFiles(_) => "big-files",
            JobParams::CleanPlan(_) => "clean-plan",
            JobParams::CleanApply(_) => "clean-apply",
            JobParams::TrashPurge(_) => "trash-purge",
            JobParams::Nightly(_) => "nightly",
            JobParams::Insights(_) => "insights",
            JobParams::Probe(_) => "probe",
        }
    }

    /// A heavy job walks or changes the disk: it runs only in a daemon whose priority is lowered.
    pub fn heavy(&self) -> bool {
        !matches!(self, JobParams::Insights(_) | JobParams::Probe(_))
    }

    /// Build parameters from a kind and a JSON object of its fields.
    pub fn from_kind(kind: &str, params: Value) -> Result<JobParams, String> {
        let tagged = serde_json::json!({ "kind": kind, "params": params });
        serde_json::from_value(tagged).map_err(|e| format!("{kind}: {e}"))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct SpaceReportParams {
    /// The folder to measure; the home when left out.
    pub path: Option<PathBuf>,
    /// How many levels below it get a size of their own.
    pub depth: u32,
    /// How many of the biggest folders the result lists.
    pub top: usize,
}
impl Default for SpaceReportParams {
    fn default() -> Self {
        SpaceReportParams {
            path: None,
            depth: 3,
            top: 30,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct GrowthParams {
    pub path: Option<PathBuf>,
    /// Compare with the newest snapshot at least this many hours older than the newest one (else the oldest one).
    pub older_hours: u64,
    /// Take a fresh snapshot first when the newest one is older than this (or when fewer than two exist).
    pub max_age_hours: u64,
    pub depth: u32,
    pub top: usize,
}
impl Default for GrowthParams {
    fn default() -> Self {
        GrowthParams {
            path: None,
            older_hours: 24,
            max_age_hours: 24,
            depth: 3,
            top: 20,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct BigFilesParams {
    pub path: Option<PathBuf>,
    pub min_mb: u64,
    /// Only files changed in the last this many hours.
    pub changed_hours: Option<u64>,
    pub limit: usize,
}
impl Default for BigFilesParams {
    fn default() -> Self {
        BigFilesParams {
            path: None,
            min_mb: 500,
            changed_hours: None,
            limit: 50,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    /// Every category, cache thresholds off: what a person runs by hand.
    #[default]
    Full,
    /// What the nightly run does: caches only above their thresholds, runner caches on Sundays.
    Daily,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct CleanPlanParams {
    pub profile: Profile,
    /// Limit to these categories; all when empty.
    pub only: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct CleanApplyParams {
    /// The plan to apply, exactly as it was written.
    pub plan: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct TrashPurgeParams {
    /// List what would be deleted, delete nothing.
    pub dry_run: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct NightlyParams {
    /// Plan and judge, apply nothing.
    pub dry_run: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct InsightsParams {
    /// How many days of history to read.
    pub days: u64,
    pub top: usize,
}
impl Default for InsightsParams {
    fn default() -> Self {
        InsightsParams { days: 30, top: 20 }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ProbeParams {
    /// How long to stay running (checkpointed every 50 ms): a harmless job to see the queue, cancel and wait work.
    pub seconds: u64,
}

// ------------------------------------------------------------------------------------------------ the record

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl State {
    pub fn finished(self) -> bool {
        matches!(self, State::Done | State::Failed | State::Cancelled)
    }
    pub fn as_str(self) -> &'static str {
        match self {
            State::Queued => "queued",
            State::Running => "running",
            State::Done => "done",
            State::Failed => "failed",
            State::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Progress {
    pub phase: String,
    pub items: u64,
    pub bytes: u64,
    /// Set while the job waits for the machine to calm down: why, in words.
    pub paused: Option<String>,
    /// Seconds spent paused so far.
    pub paused_secs: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Job {
    pub id: String,
    pub kind: String,
    pub params: JobParams,
    pub state: State,
    pub progress: Progress,
    /// The daemon's priority, read back when the job started.
    pub priority: Option<Priority>,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub log: Vec<String>,
    pub created: u64,
    pub started: Option<u64>,
    pub finished: Option<u64>,
    pub budget_secs: u64,
}

const LOG_CAP: usize = 500;

impl Job {
    pub fn new(id: String, params: JobParams, budget_secs: u64) -> Job {
        Job {
            id,
            kind: params.kind().to_string(),
            params,
            state: State::Queued,
            progress: Progress::default(),
            priority: None,
            result: None,
            error: None,
            log: Vec::new(),
            created: now(),
            started: None,
            finished: None,
            budget_secs,
        }
    }

    pub fn push_log(&mut self, line: String) {
        if self.log.len() >= LOG_CAP {
            if self.log.len() == LOG_CAP {
                self.log.push("(log full; later lines dropped)".to_string());
            }
            return;
        }
        self.log.push(line);
    }
}

// ------------------------------------------------------------------------------------------------ context

/// Why a job stopped before its end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    Cancelled,
    Budget,
}

#[derive(Debug)]
pub enum JobError {
    Stopped(Stop),
    Failed(String),
}

impl From<Stop> for JobError {
    fn from(s: Stop) -> Self {
        JobError::Stopped(s)
    }
}

impl From<String> for JobError {
    fn from(s: String) -> Self {
        JobError::Failed(s)
    }
}

/// When a job pauses for pressure, and for how long.
#[derive(Clone, Debug)]
pub struct Throttle {
    pub psi_path: PathBuf,
    /// Percent; at or below it the job runs.
    pub threshold: f64,
    /// How often pressure is read at most.
    pub check_every: Duration,
    pub backoff_start: Duration,
    pub backoff_max: Duration,
}

impl Throttle {
    pub fn from_config(config: &Config) -> Throttle {
        Throttle {
            psi_path: config.psi_path.clone(),
            threshold: config.psi_threshold as f64,
            check_every: Duration::from_secs(1),
            backoff_start: Duration::from_secs(2),
            backoff_max: Duration::from_secs(60),
        }
    }
}

pub struct Ctx {
    pub config: Config,
    pub store: Store,
    pub uid: u32,
    pub cancel: Arc<AtomicBool>,
    pub deadline: Instant,
    pub throttle: Throttle,
    pub job: Arc<Mutex<Job>>,
    last_check: Mutex<Instant>,
    last_save: Mutex<Instant>,
}

impl Ctx {
    pub fn new(config: Config, store: Store, job: Arc<Mutex<Job>>, cancel: Arc<AtomicBool>) -> Ctx {
        let budget = job.lock().map(|j| j.budget_secs).unwrap_or(3600);
        let throttle = Throttle::from_config(&config);
        let long_ago = Instant::now()
            .checked_sub(Duration::from_secs(3600))
            .unwrap_or_else(Instant::now);
        Ctx {
            uid: crate::platform::euid(),
            config,
            store,
            cancel,
            deadline: Instant::now() + Duration::from_secs(budget),
            throttle,
            job,
            last_check: Mutex::new(long_ago),
            last_save: Mutex::new(Instant::now()),
        }
    }

    /// A context around a fresh job record, for running a job inline (tests, the CLI's local verbs).
    pub fn standalone(config: Config, store: Store, params: JobParams) -> Ctx {
        let budget = config.job_budget_min * 60;
        let job = Job::new(crate::util::new_id("j"), params, budget);
        Ctx::new(
            config,
            store,
            Arc::new(Mutex::new(job)),
            Arc::new(AtomicBool::new(false)),
        )
    }

    fn stopped(&self) -> Option<Stop> {
        if self.cancel.load(Ordering::Relaxed) {
            Some(Stop::Cancelled)
        } else if Instant::now() >= self.deadline {
            Some(Stop::Budget)
        } else {
            None
        }
    }

    /// Call often from loops: stops on cancel or budget, and pauses while pressure is above the threshold.
    pub fn checkpoint(&self) -> Result<(), Stop> {
        if let Some(stop) = self.stopped() {
            return Err(stop);
        }
        {
            let mut last = self.last_check.lock().unwrap_or_else(|e| e.into_inner());
            if last.elapsed() < self.throttle.check_every {
                return Ok(());
            }
            *last = Instant::now();
        }
        self.throttle_wait()?;
        self.save_if_due();
        Ok(())
    }

    /// Wait while pressure is above the threshold, with waits that double up to the maximum.
    pub fn throttle_wait(&self) -> Result<(), Stop> {
        let mut backoff = self.throttle.backoff_start;
        while let Some(p) = pressure(&self.throttle.psi_path) {
            if p.percent <= self.throttle.threshold {
                break;
            }
            let why = format!(
                "paused: {} {:.1}% is above {}%; next look in {}s",
                p.source,
                p.percent,
                self.throttle.threshold,
                backoff.as_secs_f64()
            );
            self.with_job(|j| j.progress.paused = Some(why));
            self.save_now();
            let start = Instant::now();
            while start.elapsed() < backoff {
                if let Some(stop) = self.stopped() {
                    self.with_job(|j| j.progress.paused = None);
                    return Err(stop);
                }
                std::thread::sleep(Duration::from_millis(20).min(backoff));
            }
            let waited = start.elapsed().as_secs();
            self.with_job(|j| j.progress.paused_secs += waited.max(1));
            backoff = (backoff * 2).min(self.throttle.backoff_max);
        }
        let was_paused = self.with_job(|j| j.progress.paused.take().is_some());
        if was_paused {
            self.save_now();
        }
        Ok(())
    }

    pub fn with_job<R>(&self, f: impl FnOnce(&mut Job) -> R) -> R {
        let mut job = self.job.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut job)
    }

    pub fn phase(&self, phase: &str) {
        self.with_job(|j| j.progress.phase = phase.to_string());
        self.save_now();
    }

    pub fn tick(&self, items: u64, bytes: u64) {
        self.with_job(|j| {
            j.progress.items += items;
            j.progress.bytes += bytes;
        });
    }

    pub fn log(&self, line: impl Into<String>) {
        let line = line.into();
        self.with_job(|j| j.push_log(line));
    }

    fn save_if_due(&self) {
        let due = {
            let mut last = self.last_save.lock().unwrap_or_else(|e| e.into_inner());
            if last.elapsed() >= Duration::from_secs(2) {
                *last = Instant::now();
                true
            } else {
                false
            }
        };
        if due {
            self.save_now();
        }
    }

    pub fn save_now(&self) {
        let job = self.with_job(|j| j.clone());
        let _ = self.store.save_job(&job.id, &job);
    }
}

/// What a job produced: its result, and why it must count as failed even so (an item failed, a write failed).
pub struct Outcome {
    pub value: Value,
    pub failure: Option<String>,
}

/// Run one job to its result.
pub fn run(params: &JobParams, ctx: &Ctx) -> Result<Outcome, JobError> {
    let ok = |value: Value| Outcome {
        value,
        failure: None,
    };
    let applied = |r: clean::ApplyResult| Outcome {
        failure: r.failure(),
        value: to_value(r),
    };
    Ok(match params {
        JobParams::SpaceReport(p) => ok(to_value(space::run(p, ctx)?)),
        JobParams::Growth(p) => ok(to_value(growth::run(p, ctx)?)),
        JobParams::BigFiles(p) => ok(to_value(bigfiles::run(p, ctx)?)),
        JobParams::CleanPlan(p) => ok(to_value(clean::plan_job(p, ctx)?)),
        JobParams::CleanApply(p) => applied(clean::apply_job(p, ctx)?),
        JobParams::TrashPurge(p) => applied(clean::trash_purge(p, ctx)?),
        JobParams::Nightly(p) => {
            let report = nightly::run(p, ctx)?;
            let failure = report.applied.as_ref().and_then(|r| r.failure());
            Outcome {
                failure,
                value: to_value(report),
            }
        }
        JobParams::Insights(p) => ok(to_value(insights::run(p, ctx)?)),
        JobParams::Probe(p) => ok(to_value(probe(p, ctx)?)),
    })
}

fn to_value<T: Serialize>(value: T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProbeResult {
    pub seconds: u64,
    pub priority: Option<Priority>,
    pub pressure: Option<f64>,
}

fn probe(p: &ProbeParams, ctx: &Ctx) -> Result<ProbeResult, JobError> {
    ctx.phase("probing");
    let end = Instant::now() + Duration::from_secs(p.seconds);
    while Instant::now() < end {
        ctx.checkpoint()?;
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(ProbeResult {
        seconds: p.seconds,
        priority: ctx.with_job(|j| j.priority.clone()),
        pressure: pressure(&ctx.throttle.psi_path).map(|p| p.percent),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_round_trip_and_refuse_unknown_fields() {
        let p = JobParams::from_kind("big-files", serde_json::json!({"min_mb": 10})).unwrap();
        assert_eq!(
            p,
            JobParams::BigFiles(BigFilesParams {
                min_mb: 10,
                ..Default::default()
            })
        );
        let e = JobParams::from_kind("big-files", serde_json::json!({"min_mbs": 10})).unwrap_err();
        assert!(e.contains("unknown field"), "{e}");
        assert!(JobParams::from_kind("nope", serde_json::json!({})).is_err());
        for kind in KINDS {
            let p = JobParams::from_kind(kind, serde_json::json!({})).unwrap();
            assert_eq!(p.kind(), kind);
        }
    }
}
