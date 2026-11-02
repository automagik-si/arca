//! The verbs the binary answers, and the host reads they share with the one-shot commands. The MCP handlers and the `watch`
//! and `metrics` commands call the same functions, so a command's JSON output equals the MCP result of its verb. The three
//! plan verbs run over the kernel, through one lock and one clock.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arca_alerts::checks::{self, CheckTargets};
use arca_alerts::eval::{Timed, WatchReading};
use arca_alerts::{AlertSnapshot, Engine, TickStore, export, health, inputs};
use arca_api::Scope;
use arca_approval::{ApprovalRefusal, parse_statement};
use arca_incus::{IncusRead, UnixIncus};
use arca_kernel::canon::{Digest, params_digest};
use arca_kernel::desired::EntryId;
use arca_kernel::journal::DeletePlan;
use arca_kernel::ownership::ObjectRef;
use arca_kernel::plugin::EffectRefusal;
use arca_mcp::{CallError, Caller, GetHandler, GetReply, ToolHandler};
use arca_metrics::{DEFAULT_MINUTES, HostSample, Sampler, SamplerConfig, mean};
use arca_providers::ReadOnlyIncusProvider;
use arca_reconcile::PLAN_TTL_MS;
use arca_store::Store;
use arca_watch::{WatchConfig, watch_once};
use serde_json::{Value, json};

use crate::config::NodeConfig;
use crate::kernel::Kernel;

/// How long an approval statement a plan listing carries is valid from its issue. Droidd's own window for an owner's
/// approval, and the most the verifier accepts.
pub const STATEMENT_TTL_MS: u64 = 10 * 60 * 1000;

/// The clock the plan verbs read, in epoch milliseconds. The server reads the system clock; a test sets its own.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// How often the server adds a host sample to its ring.
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

/// How many minutes of samples the ring keeps: an hour at the sample interval.
const HISTORY_MINUTES: u32 = 60;

const PROC_ROOT: &str = "/proc";
const SYS_ROOT: &str = "/sys";

/// The current time in epoch milliseconds.
pub fn now_ms() -> u64 {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    u64::try_from(since).unwrap_or(u64::MAX)
}

/// An instant written as `Date.prototype.toISOString` writes it: `YYYY-MM-DDTHH:MM:SS.sssZ`, in UTC.
pub fn iso_utc(ms: u64) -> String {
    let secs = ms / 1_000;
    let millis = ms % 1_000;
    let days = i64::try_from(secs / 86_400).unwrap_or(i64::MAX);
    let in_day = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        in_day / 3_600,
        (in_day % 3_600) / 60,
        in_day % 60
    )
}

/// The civil date of a day count since 1970-01-01, in the proleptic Gregorian calendar (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// A sampler over the host's procfs and sysfs, reading the configured pools through the Incus socket.
pub fn new_sampler(cfg: &NodeConfig) -> Sampler<UnixIncus> {
    Sampler::new(
        SamplerConfig {
            proc_root: PathBuf::from(PROC_ROOT),
            sys_root: PathBuf::from(SYS_ROOT),
            pools: cfg.incus.pools.clone(),
            interval_ms: u64::try_from(SAMPLE_INTERVAL.as_millis()).unwrap_or(u64::MAX),
            history_minutes: HISTORY_MINUTES,
        },
        UnixIncus::new(cfg.incus.socket.clone()),
    )
}

/// How many times a watch reads the project index around its listing. The listing is filtered to the projects read before it,
/// so a project created during the read has its rows dropped; the watch then runs again on the new set.
const WATCH_ATTEMPTS: usize = 3;

/// The watch report of the host now, with `live` as its live entries: the adopted instances' entries. Each attempt reads the
/// project index (the scope), then runs the watch over that scope, then reads the index again. `complete` is set for the run, and the report is returned only when every project the second
/// read lists is in the scope, so a report is never complete while a listed project's rows were dropped. Otherwise the
/// attempt is run again on the new scope.
///
/// A project created and removed again inside one attempt is seen by neither index read. Its rows are dropped, and the attempt
/// is still accepted. Closing that gap needs the provider to report the rows it drops.
pub fn watch_snapshot<R: IncusRead>(
    incus: R,
    phones: &BTreeSet<String>,
    live: &BTreeSet<EntryId>,
    now: u64,
) -> Result<Value, CallError> {
    let mut provider = ReadOnlyIncusProvider::new(incus);
    for _ in 0..WATCH_ATTEMPTS {
        let scope = listed_projects(&provider)?;
        let cfg = WatchConfig {
            identity_phones: phones.clone(),
            live_entries: live.clone(),
            projects: scope.clone(),
            complete: true,
            ..WatchConfig::default()
        };
        let report = watch_once(&mut provider, &cfg, now)
            .map_err(|e| CallError::Internal(format!("the watch failed: {e}")))?;
        if listed_projects(&provider)?
            .iter()
            .all(|name| scope.contains(name))
        {
            return serde_json::to_value(&report)
                .map_err(|_| CallError::Internal("the report does not serialize".to_string()));
        }
    }
    Err(CallError::Internal(
        "the projects changed on every attempt of the watch; read again".to_string(),
    ))
}

/// The names of the projects the host lists now.
fn listed_projects<R: IncusRead>(
    provider: &ReadOnlyIncusProvider<R>,
) -> Result<BTreeSet<String>, CallError> {
    provider
        .project_names()
        .map(|names| names.into_iter().collect())
        .map_err(|refusal| CallError::Internal(listing_reason(&refusal)))
}

fn listing_reason(refusal: &EffectRefusal) -> String {
    match refusal {
        EffectRefusal::Provider(reason) => format!("the projects could not be listed: {reason}"),
        _ => "the projects could not be listed".to_string(),
    }
}

/// The host samples the sampler last published, oldest first. The sampler swaps in a new list after each sample, and a reader
/// takes the current list under a short lock, so no reader waits on a read of Incus. Before the first sample it is empty.
#[derive(Clone, Default)]
pub struct Published(Arc<RwLock<Arc<Vec<HostSample>>>>);

impl Published {
    /// Replaces the published samples with `samples`.
    pub fn publish(&self, samples: Vec<HostSample>) {
        let next = Arc::new(samples);
        let mut slot = self.0.write().unwrap_or_else(PoisonError::into_inner);
        *slot = next;
    }

    /// The samples published last.
    pub fn current(&self) -> Arc<Vec<HostSample>> {
        let slot = self.0.read().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(&slot)
    }
}

/// The alert snapshot the sampler's tick last published, swapped in as [`Published`] swaps its samples. Before the first
/// tick it is empty.
#[derive(Clone, Default)]
pub struct AlertsPublished(Arc<RwLock<Arc<AlertSnapshot>>>);

impl AlertsPublished {
    pub fn publish(&self, snapshot: AlertSnapshot) {
        let next = Arc::new(snapshot);
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = next;
    }

    pub fn current(&self) -> Arc<AlertSnapshot> {
        Arc::clone(&self.0.read().unwrap_or_else(PoisonError::into_inner))
    }
}

/// The sampler thread's tick: one host sample, the health checks and the newest watch sweep, the alert engine over them, and
/// the alert snapshot published. The checks and the watch are optional, so a ticker with neither reads only the host.
pub struct Ticker<S: TickStore> {
    sampler: Sampler<UnixIncus>,
    engine: Engine<S>,
    published: Published,
    alerts: AlertsPublished,
    checks: Option<CheckTargets>,
    watch: Option<Arc<WatchHandler>>,
}

impl<S: TickStore> Ticker<S> {
    pub fn new(
        sampler: Sampler<UnixIncus>,
        engine: Engine<S>,
        published: Published,
        alerts: AlertsPublished,
    ) -> Self {
        Ticker {
            sampler,
            engine,
            published,
            alerts,
            checks: None,
            watch: None,
        }
    }

    /// The same ticker, which runs the health checks of `targets` on each tick and fills `Inputs.components` with them.
    pub fn with_checks(mut self, targets: CheckTargets) -> Self {
        self.checks = Some(targets);
        self
    }

    /// The same ticker, which reads the newest watch sweep of `handler` on each tick. The sweep fills `Inputs.watch`, and its end
    /// time is the `watch` component's reading.
    pub fn with_watch(mut self, handler: Arc<WatchHandler>) -> Self {
        self.watch = Some(handler);
        self
    }

    /// Takes one host sample and publishes the ring, every sample of the last hour, oldest first. Then reads the health checks
    /// and the newest watch sweep, evaluates the alerts, commits the tick to the store, and publishes the alert snapshot, whether
    /// the commit worked or not.
    pub fn tick(&mut self, now_ms: u64) {
        self.sampler.sample(now_ms);
        self.published
            .publish(self.sampler.history(HISTORY_MINUTES));
        let mut inputs = inputs::from_sampler(&self.sampler);
        let swept = self.watch.as_ref().and_then(|handler| handler.newest());
        if let Some(targets) = &self.checks {
            let watch_end = swept.as_ref().map(|(taken, _)| *taken);
            inputs.components = Some(checks::components(targets, watch_end, now_ms));
        }
        if let Some((taken, report)) = &swept {
            inputs.watch = Some(Timed {
                at: *taken,
                value: watch_reading(report),
            });
        }
        let points = export::points(&self.sampler, &inputs);
        let mut snapshot = self.engine.tick(&inputs, points, now_ms);
        snapshot.sample_at = self.sampler.latest().map(|sample| sample.at);
        snapshot.watch_at = swept.as_ref().map(|(taken, _)| *taken);
        self.alerts.publish(snapshot);
    }
}

/// The samples within `minutes` of the newest one, oldest first. The window is measured from the newest sample's time, as
/// [`Sampler::history`] measures it.
pub fn window_of(samples: &[HostSample], minutes: u32) -> Vec<HostSample> {
    let Some(latest) = samples.last() else {
        return Vec::new();
    };
    let since = latest.at.saturating_sub(u64::from(minutes) * 60_000);
    samples
        .iter()
        .filter(|sample| sample.at >= since)
        .cloned()
        .collect()
}

/// The `node_metrics` result: the newest sample, and the mean of `window`. Before any sample exists both are null.
pub fn metrics_row(
    latest: Option<&HostSample>,
    window: &[HostSample],
    minutes: u32,
    now: u64,
) -> Value {
    json!({
        "at": now,
        "minutes": minutes,
        "latest": latest,
        "mean": mean(window),
    })
}

/// The `metrics --json` result of a run: the newest sample, and the mean over every sample the run took, so the mean covers
/// the whole run even when the run is a minute long. The window it prints is one minute.
pub fn run_row<R: IncusRead>(sampler: &Sampler<R>, now: u64) -> Value {
    let samples = sampler.history(HISTORY_MINUTES);
    metrics_row(samples.last(), &samples, DEFAULT_MINUTES, now)
}

/// `arca_status`: the node, whether Incus answers now, and how long the server has run.
pub struct StatusHandler {
    pub node: String,
    pub started: Instant,
    pub socket: PathBuf,
}

impl ToolHandler for StatusHandler {
    fn call(&self, _args: Value, _caller: &Caller) -> Result<Value, CallError> {
        let incus_reachable = UnixIncus::new(self.socket.clone()).get("/1.0").is_ok();
        Ok(json!({
            "version": env!("CARGO_PKG_VERSION"),
            "node": self.node,
            "mode": "watch-only",
            "uptimeS": self.started.elapsed().as_secs(),
            "incusReachable": incus_reachable,
            "appliedSeq": Value::Null,
        }))
    }
}

/// `node_metrics`: the latest published sample and the mean over the requested minutes. It reads the snapshot the sampler
/// published, so it never waits on Incus.
pub struct MetricsHandler {
    pub published: Published,
}

impl ToolHandler for MetricsHandler {
    fn call(&self, args: Value, _caller: &Caller) -> Result<Value, CallError> {
        let minutes = args
            .get("minutes")
            .and_then(Value::as_u64)
            .and_then(|m| u32::try_from(m).ok())
            .unwrap_or(DEFAULT_MINUTES);
        let samples = self.published.current();
        Ok(metrics_row(
            samples.last(),
            &window_of(&samples, minutes),
            minutes,
            now_ms(),
        ))
    }
}

/// `alerts_list`: the firing alerts of the newest published tick, and the store's size, read on a read-only connection.
pub struct AlertsHandler {
    pub alerts: AlertsPublished,
    pub published: Published,
    pub store: Arc<Mutex<Store>>,
    pub node: String,
}

impl ToolHandler for AlertsHandler {
    fn call(&self, _args: Value, _caller: &Caller) -> Result<Value, CallError> {
        let snapshot = self.alerts.current();
        let now = now_ms();
        let firing: Vec<Value> = snapshot
            .firing
            .iter()
            .map(|alert| {
                let mut event = alert.event.to_json(&self.node);
                event["unknown"] = json!(alert.unknown);
                event
            })
            .collect();
        let sample_age = self
            .published
            .current()
            .last()
            .map(|sample| now.saturating_sub(sample.at));
        let unknown: Vec<Value> = snapshot
            .unknown
            .iter()
            .map(|alert| {
                json!({
                    "rule": alert.rule,
                    "labels": alert.labels,
                    "state": alert.state.as_str(),
                    "reason": alert.reason.as_str(),
                })
            })
            .collect();
        let store = self.store.lock().unwrap_or_else(PoisonError::into_inner);
        let resolved = store
            .resolved_since(now.saturating_sub(RESOLVED_WINDOW_MS), RESOLVED_LIMIT)
            .map_err(|error| {
                CallError::Internal(format!("the resolved alerts could not be read: {error}"))
            })?;
        let store_bytes = store.bytes();
        drop(store);
        let resolved: Vec<Value> = resolved
            .iter()
            .map(|event| event.to_json(&self.node))
            .collect();
        Ok(json!({
            "firing": firing,
            "unknown": unknown,
            "disabled": snapshot.disabled,
            "resolvedRecent": resolved,
            "sampleAgeMs": sample_age,
            "watchAgeMs": snapshot.watch_at.map(|taken| now.saturating_sub(taken)),
            "commitMs": snapshot.commit_ms,
            "storeError": snapshot.store_error.as_ref().map(|error| {
                json!({ "at": error.at, "message": error.message })
            }),
            "storeReset": snapshot.store_reset,
            "storeBytes": store_bytes,
            "healthLine": health::alerts_line(&snapshot, now),
        }))
    }
}

/// How far back `resolvedRecent` reaches, in milliseconds.
const RESOLVED_WINDOW_MS: u64 = 24 * 60 * 60 * 1_000;
/// The most resolves `resolvedRecent` lists.
const RESOLVED_LIMIT: usize = 20;
/// The most alert events one `GET /alerts` page carries.
pub const EXPORT_PAGE: usize = 500;

/// `GET /alerts?after=<seq>`: the alert events with a `seq` above `after`, oldest first, one page of at most [`EXPORT_PAGE`].
/// A missing `after` is 0. A value that is not a whole number is refused as invalid.
pub struct AlertsExport {
    pub node: String,
    pub store: Arc<Mutex<Store>>,
}

impl GetHandler for AlertsExport {
    fn get(&self, query: Option<&str>, _caller: &Caller) -> Result<GetReply, CallError> {
        let after = after_of(query)?;
        let events = self
            .store
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .events_after(after, EXPORT_PAGE)
            .map_err(|error| {
                CallError::Internal(format!("the alert events could not be read: {error}"))
            })?;
        Ok(GetReply {
            content_type: "application/x-ndjson",
            body: export::render_events(&self.node, &events).into_bytes(),
        })
    }
}

/// The `after` of a query string: the value of its first `after=` pair, or 0 when there is none.
fn after_of(query: Option<&str>) -> Result<u64, CallError> {
    let value = query
        .unwrap_or("")
        .split('&')
        .find_map(|pair| pair.strip_prefix("after="));
    match value {
        None => Ok(0),
        Some(text) => text
            .parse()
            .map_err(|_| CallError::Invalid("after must be a whole number".to_string())),
    }
}

/// `GET /metrics`: the newest published alert snapshot in the Prometheus text format.
pub struct MetricsExport {
    pub node: String,
    pub alerts: AlertsPublished,
}

impl GetHandler for MetricsExport {
    fn get(&self, _query: Option<&str>, _caller: &Caller) -> Result<GetReply, CallError> {
        let body = export::render_metrics(&self.node, &self.alerts.current());
        Ok(GetReply {
            content_type: "text/plain; version=0.0.4",
            body: body.into_bytes(),
        })
    }
}

/// The interval between sweeps, in milliseconds. A sweep starts once this long has passed since the last one started.
const WATCH_INTERVAL_MS: u64 = 30_000;
/// How often the `arca-watcher` thread asks whether a sweep is due. Far shorter than the interval, so a sweep starts within a
/// second of its due time.
const WATCH_POLL: Duration = Duration::from_secs(1);
/// The interval between watch sweeps.
pub const WATCH_INTERVAL: Duration = Duration::from_millis(WATCH_INTERVAL_MS);
/// How long a `watch_report` call waits for the first sweep to end before it answers that none has.
pub const WATCH_WAIT: Duration = Duration::from_secs(35);

/// What the watch thread last recorded: the newest successful sweep, and the most recent failure.
#[derive(Default)]
struct WatchState {
    /// The newest successful sweep: the epoch milliseconds it was taken at, and its report.
    newest: Option<(u64, Value)>,
    /// The error of the most recent sweep that failed.
    last_error: Option<CallError>,
}

/// `watch_report`: the watch of every project, with the identity flags of the configured phones. The `arca-watcher` thread runs
/// the sweeps ([`watcher_step`]); `watch_report` never sweeps. It answers from the newest successful sweep, with `ageMs`, the
/// milliseconds since that sweep was taken. Before the first sweep has ended, a call waits up to [`WATCH_WAIT`] for it. If no
/// sweep has ever succeeded, a call answers the last error, or `no watch sweep has finished yet` when none has ended.
///
/// `adopted` gives the live entries of the adopted instances, `adopt-<instance>`; each sweep asks it again, so an adoption or a
/// release made while the server runs shows at the next sweep.
pub struct WatchHandler {
    pub socket: PathBuf,
    pub phones: BTreeSet<String>,
    pub adopted: Arc<dyn Fn() -> BTreeSet<EntryId> + Send + Sync>,
    state: Mutex<WatchState>,
    finished: Condvar,
}

impl WatchHandler {
    /// A handler over the Incus socket at `socket`, with no sweep yet and no adopted instance.
    pub fn new(socket: PathBuf, phones: BTreeSet<String>) -> Self {
        WatchHandler {
            socket,
            phones,
            adopted: Arc::new(BTreeSet::new),
            state: Mutex::default(),
            finished: Condvar::new(),
        }
    }

    /// The same handler, asking `adopted` for the adopted instances' entries on each sweep.
    pub fn with_adopted(
        mut self,
        adopted: impl Fn() -> BTreeSet<EntryId> + Send + Sync + 'static,
    ) -> Self {
        self.adopted = Arc::new(adopted);
        self
    }

    /// The newest successful sweep: the epoch milliseconds it was taken at, and its report. `None` before one has succeeded.
    pub fn newest(&self) -> Option<(u64, Value)> {
        self.lock().newest.clone()
    }

    /// Runs one sweep taken at `now_ms` and records its outcome, then wakes the calls that wait for a first sweep. The state lock
    /// is not held during the sweep, so a call never waits on Incus.
    fn sweep(&self, now_ms: u64) {
        let live = (self.adopted)();
        let outcome = watch_snapshot(
            UnixIncus::new(self.socket.clone()),
            &self.phones,
            &live,
            now_ms,
        );
        let mut state = self.lock();
        match outcome {
            Ok(report) => state.newest = Some((now_ms, report)),
            Err(error) => state.last_error = Some(error),
        }
        drop(state);
        self.finished.notify_all();
    }

    fn lock(&self) -> MutexGuard<'_, WatchState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The answer at `now_ms`, after waiting up to `wait` for the first sweep to end.
    fn answer(&self, wait: Duration, now_ms: u64) -> Result<Value, CallError> {
        let state = self.lock();
        let (state, _) = self
            .finished
            .wait_timeout_while(state, wait, |s| {
                s.newest.is_none() && s.last_error.is_none()
            })
            .unwrap_or_else(PoisonError::into_inner);
        if let Some((taken, report)) = &state.newest {
            let mut answer = report.clone();
            if let Some(object) = answer.as_object_mut() {
                object.insert("ageMs".to_string(), json!(now_ms.saturating_sub(*taken)));
            }
            return Ok(answer);
        }
        Err(state
            .last_error
            .clone()
            .unwrap_or_else(|| CallError::Internal("no watch sweep has finished yet".to_string())))
    }
}

impl ToolHandler for WatchHandler {
    fn call(&self, _args: Value, _caller: &Caller) -> Result<Value, CallError> {
        self.answer(WATCH_WAIT, now_ms())
    }
}

/// Runs a sweep of `h` at `now_ms` when [`WATCH_INTERVAL`] has passed since `last_start_ms`, or when no sweep has started yet.
/// Records the start in `last_start_ms`, whatever the sweep's outcome. Returns whether it swept.
pub fn watcher_step(h: &WatchHandler, now_ms: u64, last_start_ms: &mut Option<u64>) -> bool {
    let due = last_start_ms.is_none_or(|last| now_ms.saturating_sub(last) >= WATCH_INTERVAL_MS);
    if !due {
        return false;
    }
    *last_start_ms = Some(now_ms);
    h.sweep(now_ms);
    true
}

/// Starts the `arca-watcher` thread: a sweep at once, then one each [`WATCH_INTERVAL`], for the life of the process.
pub fn spawn_watcher(h: Arc<WatchHandler>, clock: Clock) -> io::Result<()> {
    thread::Builder::new()
        .name("arca-watcher".to_string())
        .spawn(move || {
            let mut last_start = None;
            loop {
                watcher_step(&h, clock(), &mut last_start);
                thread::sleep(WATCH_POLL);
            }
        })?;
    Ok(())
}

/// The drift of a watch report, by kind, for the evaluator's drift input. `marked_without_entry` and `entry_without_object` are
/// always present, at 0 when none is drift. A kind the report's closed set does not name counts as `other`.
pub fn watch_reading(report: &Value) -> WatchReading {
    let mut drift = BTreeMap::from([
        ("marked_without_entry".to_string(), 0u64),
        ("entry_without_object".to_string(), 0u64),
    ]);
    for entry in report["drift"].as_array().into_iter().flatten() {
        let kind = match entry["kind"].as_str() {
            Some("markedWithoutEntry") => "marked_without_entry",
            Some("entryWithoutObject") => "entry_without_object",
            _ => "other",
        };
        *drift.entry(kind.to_string()).or_insert(0) += 1;
    }
    WatchReading { drift }
}

/// `token_list`: the id and scope of each token, as they were when the server started. No secret and no digest.
pub struct TokenListHandler {
    pub ids: Vec<(String, Scope)>,
}

impl ToolHandler for TokenListHandler {
    fn call(&self, _args: Value, _caller: &Caller) -> Result<Value, CallError> {
        let tokens: Vec<Value> = self
            .ids
            .iter()
            .map(|(id, scope)| json!({ "id": id, "scope": scope.as_str() }))
            .collect();
        Ok(json!({ "tokens": tokens }))
    }
}

/// The handlers of the three plan verbs, keyed by verb name, over one kernel and one clock.
pub fn plan_handlers(
    kernel: Arc<Mutex<Kernel>>,
    clock: Clock,
) -> Vec<(&'static str, Arc<dyn ToolHandler>)> {
    vec![
        (
            "plan_list",
            Arc::new(PlanList {
                kernel: Arc::clone(&kernel),
                clock: Arc::clone(&clock),
            }) as Arc<dyn ToolHandler>,
        ),
        (
            "plan_decline",
            Arc::new(PlanDecline {
                kernel: Arc::clone(&kernel),
            }) as Arc<dyn ToolHandler>,
        ),
        (
            "approval_submit",
            Arc::new(ApprovalSubmit { kernel, clock }) as Arc<dyn ToolHandler>,
        ),
    ]
}

/// `plan_list`: each waiting delete plan that has not lapsed, with the statement the owner signs for it. Every listing draws a
/// new nonce, so each statement is built fresh. A lapsed plan is left out, since its run is refused as expired.
struct PlanList {
    kernel: Arc<Mutex<Kernel>>,
    clock: Clock,
}

impl ToolHandler for PlanList {
    fn call(&self, _args: Value, _caller: &Caller) -> Result<Value, CallError> {
        let now = (self.clock)();
        let kernel = lock(&self.kernel)?;
        let plans = kernel
            .pending_plans()
            .iter()
            .filter(|plan| now.saturating_sub(plan.created_ms) <= PLAN_TTL_MS)
            .map(|plan| plan_json(kernel.host(), plan, now))
            .collect::<Result<Vec<Value>, CallError>>()?;
        Ok(json!({ "plans": plans }))
    }
}

/// `plan_decline`: declines one waiting plan. Its objects are retained, and the decline is final.
struct PlanDecline {
    kernel: Arc<Mutex<Kernel>>,
}

impl ToolHandler for PlanDecline {
    fn call(&self, args: Value, _caller: &Caller) -> Result<Value, CallError> {
        let digest = plan_argument(&args)?;
        let mut kernel = lock(&self.kernel)?;
        match kernel.decline(&digest).map_err(|_| internal())? {
            None => Err(no_such_plan()),
            Some(objects) => Ok(json!({
                "plan": digest.to_hex(),
                "retained": objects_json(&objects)?,
            })),
        }
    }
}

/// `approval_submit`: checks an owner's approval for one waiting plan and, when it holds, runs the plan's delete. The approval
/// is read here and never stored or echoed.
struct ApprovalSubmit {
    kernel: Arc<Mutex<Kernel>>,
    clock: Clock,
}

impl ToolHandler for ApprovalSubmit {
    fn call(&self, args: Value, _caller: &Caller) -> Result<Value, CallError> {
        let digest = plan_argument(&args)?;
        let approval = args
            .get("approval")
            .and_then(Value::as_str)
            .ok_or_else(|| CallError::Invalid("approval is required".to_string()))?;
        let now = (self.clock)();
        let mut kernel = lock(&self.kernel)?;
        match kernel.approve(&digest, approval, now) {
            Ok(None) => Err(no_such_plan()),
            Ok(Some(run)) => Ok(json!({
                "plan": digest.to_hex(),
                "deleted": objects_json(&run.deleted)?,
                "drift": objects_json(&run.drift)?,
                "partial": run.partial,
            })),
            Err(refusal) => Err(refusal_failure(refusal)),
        }
    }
}

fn lock(kernel: &Mutex<Kernel>) -> Result<MutexGuard<'_, Kernel>, CallError> {
    kernel.lock().map_err(|_| internal())
}

/// The failure a kernel error becomes. The text stays inside the server.
fn internal() -> CallError {
    CallError::Internal("the kernel could not complete the call".to_string())
}

fn no_such_plan() -> CallError {
    CallError::Refused {
        code: "NoSuchPlan".to_string(),
        message: "no delete plan waits under this digest".to_string(),
    }
}

/// The plan digest a call names. The schema has checked its length; this checks its digits.
fn plan_argument(args: &Value) -> Result<Digest, CallError> {
    let text = args
        .get("plan")
        .and_then(Value::as_str)
        .ok_or_else(|| CallError::Invalid("plan is required".to_string()))?;
    Digest::from_hex(text).map_err(|_| CallError::Invalid("plan is not a digest".to_string()))
}

/// A refused approval as a call's refusal. A storage failure is the server's own, and is not a refusal of the approval.
fn refusal_failure(refusal: ApprovalRefusal) -> CallError {
    if matches!(refusal, ApprovalRefusal::Storage(_)) {
        return CallError::Internal("the approval could not be recorded".to_string());
    }
    CallError::Refused {
        code: refusal_code(&refusal).to_string(),
        message: refusal.to_string(),
    }
}

/// The name of a refusal, as a caller's `refusal` field carries it.
fn refusal_code(refusal: &ApprovalRefusal) -> &'static str {
    match refusal {
        ApprovalRefusal::Malformed(_) => "Malformed",
        ApprovalRefusal::NotCanonical => "NotCanonical",
        ApprovalRefusal::NoVerifier => "NoVerifier",
        ApprovalRefusal::BadSignature => "BadSignature",
        ApprovalRefusal::NotHardwareKey => "NotHardwareKey",
        ApprovalRefusal::NoUserPresence => "NoUserPresence",
        ApprovalRefusal::NoUserVerification => "NoUserVerification",
        ApprovalRefusal::WrongOrigin => "WrongOrigin",
        ApprovalRefusal::WrongRp => "WrongRp",
        ApprovalRefusal::SignCountRegressed => "SignCountRegressed",
        ApprovalRefusal::WrongNamespace => "WrongNamespace",
        ApprovalRefusal::WrongHost => "WrongHost",
        ApprovalRefusal::WrongAction => "WrongAction",
        ApprovalRefusal::WrongTarget => "WrongTarget",
        ApprovalRefusal::WrongParams => "WrongParams",
        ApprovalRefusal::IssuedAhead => "IssuedAhead",
        ApprovalRefusal::Expired => "Expired",
        ApprovalRefusal::TtlTooLong => "TtlTooLong",
        ApprovalRefusal::Replayed => "Replayed",
        ApprovalRefusal::Untrusted(_) => "Untrusted",
        ApprovalRefusal::Storage(_) => "Storage",
        ApprovalRefusal::Unresolved => "Unresolved",
    }
}

fn plan_json(host: &str, plan: &DeletePlan, now: u64) -> Result<Value, CallError> {
    let digest = plan.digest.to_hex();
    let params = params_digest(&json!({ "plan": digest.clone() })).map_err(|_| internal())?;
    let statement = plan_statement(host, &digest, now, &fresh_nonce()?)?;
    Ok(json!({
        "plan": digest,
        "entry": plan.entry.0,
        "createdAt": plan.created_ms,
        "expiresAt": plan.created_ms.saturating_add(PLAN_TTL_MS),
        "objects": objects_json(&plan.objects)?,
        "paramsDigest": params,
        "statement": statement,
    }))
}

/// The statement an owner signs to approve the delete of plan `plan`: the exact bytes, with the keys in sorted order, no spaces
/// and no trailing newline. The statement is read back by the kernel's own parser before it is returned, so a listing never
/// carries a statement the verifier would refuse for its form.
pub fn plan_statement(
    host: &str,
    plan: &str,
    issued_ms: u64,
    nonce: &str,
) -> Result<String, CallError> {
    let params = params_digest(&json!({ "plan": plan })).map_err(|_| internal())?;
    let text = format!(
        r#"{{"action":"delete","expiresAt":"{}","host":"{host}","issuedAt":"{}","nonce":"{nonce}","paramsDigest":"{params}","target":"plan-{plan}"}}"#,
        iso_utc(issued_ms.saturating_add(STATEMENT_TTL_MS)),
        iso_utc(issued_ms),
    );
    parse_statement(text.as_bytes()).map_err(|_| internal())?;
    Ok(text)
}

/// Sixteen random bytes as 32 lowercase hex digits: a statement's nonce.
fn fresh_nonce() -> Result<String, CallError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| internal())?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn objects_json(objects: &[ObjectRef]) -> Result<Vec<Value>, CallError> {
    objects.iter().map(object_json).collect()
}

fn object_json(object: &ObjectRef) -> Result<Value, CallError> {
    let kind = serde_json::to_value(object.kind).map_err(|_| internal())?;
    Ok(json!({
        "kind": kind,
        "project": object.project,
        "name": object.name,
        "uuid": object.uuid,
    }))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use arca_incus::IncusError;
    use arca_metrics::node_metrics;
    use arca_providers::INSTANCES_ALL_PROJECTS;

    use super::*;

    const PROJECTS_PATH: &str = "/1.0/projects?recursion=1";

    /// An Incus that answers the project index and the instance listing from two scripts, one answer per read, the last
    /// answer repeating.
    struct Scripted {
        index: Mutex<VecDeque<Vec<&'static str>>>,
        instances: Mutex<VecDeque<Vec<Value>>>,
    }

    /// The next answer of a script: the front one, unless it is the last, which then repeats.
    fn next_answer<T: Clone>(script: &Mutex<VecDeque<T>>) -> T {
        let mut queue = script.lock().expect("the script lock");
        if queue.len() > 1 {
            queue.pop_front().expect("a queued answer")
        } else {
            queue.front().cloned().expect("a scripted answer")
        }
    }

    impl IncusRead for Scripted {
        fn get(&self, path: &str) -> Result<Value, IncusError> {
            if path == PROJECTS_PATH {
                let names = next_answer(&self.index);
                return Ok(Value::Array(
                    names
                        .into_iter()
                        .map(|name| json!({ "name": name }))
                        .collect(),
                ));
            }
            if path == INSTANCES_ALL_PROJECTS {
                return Ok(Value::Array(next_answer(&self.instances)));
            }
            Err(IncusError::Decode(format!("no script answer for {path}")))
        }
    }

    fn instance(project: &str, name: &str) -> Value {
        json!({
            "name": name,
            "project": project,
            "type": "container",
            "status": "Running",
            "config": {},
            "expanded_config": {},
        })
    }

    /// The projects of a report and the instance names each one carries.
    fn rows_by_project(report: &Value) -> Vec<(String, Vec<String>)> {
        report["projects"]
            .as_array()
            .expect("the report lists projects")
            .iter()
            .map(|project| {
                let name = project["project"]
                    .as_str()
                    .expect("a project name")
                    .to_string();
                let rows = project["instances"]
                    .as_array()
                    .expect("instances")
                    .iter()
                    .map(|row| row["name"].as_str().expect("a name").to_string())
                    .collect();
                (name, rows)
            })
            .collect()
    }

    #[test]
    fn a_project_created_between_the_index_read_and_the_listing_keeps_its_rows() {
        // Project `b` appears after the first index read and before the listing. Its instance is in the listing, and the
        // index read after the listing names `b`, so the report must carry `b`'s row rather than drop it.
        let incus = Scripted {
            index: Mutex::new(VecDeque::from(vec![vec!["a"], vec!["a", "b"]])),
            instances: Mutex::new(VecDeque::from(vec![vec![
                instance("a", "x1"),
                instance("b", "y1"),
            ]])),
        };
        let report = watch_snapshot(incus, &BTreeSet::new(), &BTreeSet::new(), 7)
            .expect("the watch answers");
        assert_eq!(
            rows_by_project(&report),
            vec![
                ("a".to_string(), vec!["x1".to_string()]),
                ("b".to_string(), vec!["y1".to_string()]),
            ]
        );
    }

    #[test]
    fn a_stable_project_index_reports_every_project_once() {
        let incus = Scripted {
            index: Mutex::new(VecDeque::from(vec![vec!["a", "b"]])),
            instances: Mutex::new(VecDeque::from(vec![vec![
                instance("a", "x1"),
                instance("b", "y1"),
                instance("b", "y2"),
            ]])),
        };
        let report = watch_snapshot(incus, &BTreeSet::new(), &BTreeSet::new(), 7)
            .expect("the watch answers");
        assert_eq!(
            rows_by_project(&report),
            vec![
                ("a".to_string(), vec!["x1".to_string()]),
                ("b".to_string(), vec!["y1".to_string(), "y2".to_string()]),
            ]
        );
    }

    #[test]
    fn the_metrics_mean_of_a_minute_long_run_covers_its_first_sample() {
        // A 60-second `metrics` run takes samples at 0, 5, ..., 55 seconds and once more just past 60 seconds, 13 in all. The
        // first sees no instance and the rest see thirteen, so the mean over the whole run is (0 + 12 * 13) / 13 = 12. A
        // one-minute window measured from the last sample drops the first sample and reads 13, which the `metrics` command
        // must not print for its own run.
        let thirteen: Vec<Value> = (0..13).map(|n| instance("p", &format!("n{n}"))).collect();
        let incus = Scripted {
            index: Mutex::new(VecDeque::from(vec![Vec::new()])),
            instances: Mutex::new(VecDeque::from(vec![Vec::new(), thirteen])),
        };
        let mut sampler = Sampler::new(
            SamplerConfig {
                proc_root: PathBuf::from("/nonexistent/arca-test-proc"),
                sys_root: PathBuf::from("/nonexistent/arca-test-sys"),
                pools: Vec::new(),
                interval_ms: 5_000,
                history_minutes: HISTORY_MINUTES,
            },
            incus,
        );
        for at in (0..12u64)
            .map(|step| 1_000_000 + step * 5_000)
            .chain(std::iter::once(1_000_000 + 60_001))
        {
            sampler.sample(at);
        }
        let row = run_row(&sampler, 0);
        assert_eq!(row["mean"]["projects"][0]["total"], json!(12));
        assert_eq!(row["latest"]["projects"][0]["total"], json!(13));
    }

    #[test]
    fn before_any_sample_the_metrics_row_has_no_latest_and_no_mean() {
        let row = metrics_row(None, &[], DEFAULT_MINUTES, 5);
        assert_eq!(row["at"], json!(5));
        assert_eq!(row["minutes"], json!(1));
        assert_eq!(row["latest"], Value::Null);
        assert_eq!(row["mean"], Value::Null);
    }

    #[test]
    fn a_published_snapshot_answers_the_same_row_as_the_sampler() {
        let incus = Scripted {
            index: Mutex::new(VecDeque::from(vec![Vec::new()])),
            instances: Mutex::new(VecDeque::from(vec![vec![instance("p", "n0")]])),
        };
        let mut sampler = Sampler::new(
            SamplerConfig {
                proc_root: PathBuf::from("/nonexistent/arca-test-proc"),
                sys_root: PathBuf::from("/nonexistent/arca-test-sys"),
                pools: Vec::new(),
                interval_ms: 5_000,
                history_minutes: HISTORY_MINUTES,
            },
            incus,
        );
        for step in 0..=3u64 {
            sampler.sample(1_000_000 + step * 5_000);
        }
        let published = Published::default();
        published.publish(sampler.history(HISTORY_MINUTES));
        let snapshot = published.current();
        for minutes in [1, 60] {
            let from_snapshot =
                metrics_row(snapshot.last(), &window_of(&snapshot, minutes), minutes, 9);
            assert_eq!(from_snapshot, node_metrics(&sampler, Some(minutes), 9));
        }
    }
}

#[cfg(test)]
mod watch_newest_tests {
    use std::path::Path;
    use std::thread;

    use arca_incus::{FakeIncus, FakeState};

    use super::*;

    const FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../arca-incus/tests/fixtures/state.json"
    );

    /// A fresh directory under the default temporary directory, removed on drop. The default keeps the socket path short.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos());
            let dir = std::env::temp_dir().join(format!(
                "arca-watch-newest-{}-{nanos}-{tag}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).expect("create the scratch directory");
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fake(scratch: &Scratch) -> FakeIncus {
        let state = FakeState::load(Path::new(FIXTURE)).expect("load the fixture");
        FakeIncus::start(&scratch.0, state).expect("start the fake")
    }

    fn gets(fake: &FakeIncus) -> usize {
        fake.requests().iter().filter(|r| r.method == "GET").count()
    }

    fn viewer() -> Caller {
        Caller {
            token_id: "viewer-1".to_string(),
            scope: Scope::Viewer,
        }
    }

    fn without_age(mut report: Value) -> Value {
        report.as_object_mut().expect("an object").remove("ageMs");
        report
    }

    #[test]
    fn watch_report_never_sweeps_and_answers_the_newest_sweep_with_its_age() {
        let scratch = Scratch::new("newest");
        let fake = fake(&scratch);
        let handler = WatchHandler::new(fake.socket().to_path_buf(), BTreeSet::new());
        let mut last = None;
        assert!(
            watcher_step(&handler, 1_000, &mut last),
            "the first step sweeps"
        );
        let after_sweep = gets(&fake);
        assert!(after_sweep > 0, "the sweep read the host");

        let early = handler.answer(Duration::ZERO, 1_500).expect("the answer");
        let late = handler.answer(Duration::ZERO, 3_000).expect("the answer");
        assert_eq!(gets(&fake), after_sweep, "answering sends no GET");
        assert_eq!(early["ageMs"], json!(500));
        assert_eq!(late["ageMs"], json!(2_000));
        assert_eq!(
            without_age(early),
            without_age(late),
            "both report the same sweep"
        );

        let through_the_verb = handler
            .call(json!({}), &viewer())
            .expect("the verb answers");
        assert!(
            through_the_verb["ageMs"].is_u64(),
            "the verb's answer has ageMs"
        );
        assert_eq!(gets(&fake), after_sweep, "the verb sends no GET");
    }

    #[test]
    fn a_step_inside_the_interval_does_not_sweep_and_one_at_the_interval_does() {
        let scratch = Scratch::new("interval");
        let fake = fake(&scratch);
        let handler = WatchHandler::new(fake.socket().to_path_buf(), BTreeSet::new());
        let mut last = None;
        assert!(watcher_step(&handler, 0, &mut last));
        let after_first = gets(&fake);
        assert!(!watcher_step(&handler, WATCH_INTERVAL_MS - 1, &mut last));
        assert_eq!(gets(&fake), after_first, "no sweep inside the interval");
        assert!(watcher_step(&handler, WATCH_INTERVAL_MS, &mut last));
        assert!(gets(&fake) > after_first, "a sweep at the interval");
    }

    #[test]
    fn a_call_before_the_first_sweep_waits_for_it_and_then_answers() {
        let scratch = Scratch::new("waits");
        let fake = fake(&scratch);
        let handler = Arc::new(WatchHandler::new(
            fake.socket().to_path_buf(),
            BTreeSet::new(),
        ));
        let waiter = Arc::clone(&handler);
        let caller = thread::spawn(move || waiter.answer(Duration::from_secs(10), 2_000));
        thread::sleep(Duration::from_millis(50));
        let mut last = None;
        assert!(watcher_step(&handler, 1_000, &mut last));
        let answer = caller
            .join()
            .expect("the caller thread")
            .expect("the call answers once the sweep has ended");
        assert_eq!(answer["ageMs"], json!(1_000));
    }

    #[test]
    fn before_any_sweep_ends_a_call_answers_that_none_has_after_its_wait() {
        let scratch = Scratch::new("none");
        let handler = WatchHandler::new(scratch.0.join("absent.sock"), BTreeSet::new());
        let error = handler
            .answer(Duration::from_millis(20), 0)
            .expect_err("no sweep has ended");
        assert_eq!(
            error,
            CallError::Internal("no watch sweep has finished yet".to_string())
        );
    }

    #[test]
    fn a_failed_first_sweep_answers_its_error_at_once() {
        let scratch = Scratch::new("failed");
        let handler = WatchHandler::new(scratch.0.join("absent.sock"), BTreeSet::new());
        let mut last = None;
        assert!(watcher_step(&handler, 0, &mut last));
        assert!(
            handler.newest().is_none(),
            "a failed sweep is not an answer"
        );
        let started = Instant::now();
        let error = handler
            .answer(WATCH_WAIT, 0)
            .expect_err("the failed sweep is the answer");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a call does not wait out its window once a sweep has failed"
        );
        assert!(
            matches!(&error, CallError::Internal(text) if text.starts_with("the projects could not be listed")),
            "{error:?}"
        );
    }

    #[test]
    fn a_failed_later_sweep_leaves_the_newest_success_in_place() {
        let scratch = Scratch::new("later");
        let fake = fake(&scratch);
        let handler = WatchHandler::new(fake.socket().to_path_buf(), BTreeSet::new());
        let mut last = None;
        assert!(watcher_step(&handler, 1_000, &mut last));
        std::fs::remove_file(fake.socket()).expect("remove the socket");
        assert!(watcher_step(&handler, WATCH_INTERVAL_MS + 1_000, &mut last));

        assert_eq!(handler.newest().map(|(taken, _)| taken), Some(1_000));
        let answer = handler
            .answer(Duration::ZERO, 3_000)
            .expect("the newest success still answers");
        assert_eq!(answer["ageMs"], json!(2_000));
    }

    #[test]
    fn the_watch_reading_counts_each_drift_kind_and_keeps_both_kinds() {
        let report = json!({ "drift": [
            { "kind": "markedWithoutEntry", "project": "svc", "name": "k1", "entry": "" },
            { "kind": "markedWithoutEntry", "project": "svc", "name": "x1", "entry": "toy" },
            { "kind": "entryWithoutObject", "entry": "toy" },
            { "kind": "somethingNew" }
        ]});
        assert_eq!(
            watch_reading(&report).drift,
            BTreeMap::from([
                ("entry_without_object".to_string(), 1),
                ("marked_without_entry".to_string(), 2),
                ("other".to_string(), 1),
            ])
        );
        let clean = BTreeMap::from([
            ("entry_without_object".to_string(), 0),
            ("marked_without_entry".to_string(), 0),
        ]);
        assert_eq!(watch_reading(&json!({ "drift": [] })).drift, clean);
        assert_eq!(watch_reading(&json!({})).drift, clean);
    }
}

#[cfg(test)]
mod iso_tests {
    use super::*;
    use arca_kernel::desired::instant_ms;

    #[test]
    fn the_epoch_and_the_test_clock_are_written_as_to_iso_string_writes_them() {
        assert_eq!(iso_utc(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_utc(1_793_361_600_000), "2026-10-30T12:00:00.000Z");
    }

    #[test]
    fn every_instant_written_reads_back_to_the_same_milliseconds() {
        // Leap days, a century year, a year before the epoch's last leap year and the end of a day.
        for ms in [
            951_782_400_000u64,
            4_107_542_399_999,
            1_709_251_199_999,
            1_234_567_890_123,
            86_399_999,
        ] {
            let text = iso_utc(ms);
            assert_eq!(text.len(), 24, "{text}");
            assert_eq!(instant_ms(&text), Some(ms), "{text}");
        }
    }
}
