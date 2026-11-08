//! The verbs the binary answers, and the host reads they share with the one-shot commands. The MCP handlers and the `watch`
//! and `metrics` commands call the same functions, so a command's JSON output equals the MCP result of its verb. The three
//! plan verbs run over the kernel, through one lock and one clock.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
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
use arca_kernel::journal::read_journal;
use arca_kernel::ownership::ObjectRef;
use arca_kernel::plugin::EffectRefusal;
use arca_mcp::{CallError, Caller, GetHandler, GetReply, TokenStore, ToolHandler};
use arca_metrics::{
    DEFAULT_MINUTES, HostModeChoice, HostSample, MODE_PROBE_TIMEOUT, ModeRetry, Sampler,
    SamplerConfig, choose_host_mode, mean,
};
use arca_providers::ReadOnlyIncusProvider;
use arca_reconcile::{Outcome, PLAN_TTL_MS};
use arca_store::Store;
use arca_watch::{WatchConfig, watch_once};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::NodeConfig;
use crate::kernel::{AppliedSeq, JOURNAL_FILE, Kernel};

/// Answers from the public ontology embedded in the binary, without reading node state.
pub struct OntologyQuery;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OntologySection {
    owns: Vec<OntologyOwned>,
    calls: Vec<OntologyCall>,
    never_owns: Vec<OntologyNeverOwn>,
}

#[derive(Deserialize)]
struct OntologyOwned {
    id: String,
    kind: String,
    status: String,
    #[serde(rename = "split")]
    _split: Option<OntologySplit>,
}

#[derive(Deserialize)]
struct OntologySplit {
    #[serde(rename = "by")]
    _by: String,
    #[serde(rename = "value")]
    _value: String,
}

#[derive(Deserialize)]
struct OntologyCall {
    id: String,
    to: String,
    contract: String,
    status: String,
    carries: Vec<String>,
    #[serde(rename = "requires")]
    _requires: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OntologyNeverOwn {
    Exact { kind: String, id: String },
    Prefix { kind: String, prefix: String },
}

impl OntologySection {
    fn query(&self, id: &str) -> Value {
        let owned = self.owns.iter().find(|entry| entry.id == id);
        let never = self.never_owns.iter().find_map(|entry| match entry {
            OntologyNeverOwn::Exact { kind, id: exact } if exact == id => Some(kind),
            OntologyNeverOwn::Prefix { kind, prefix } if id.starts_with(prefix) => Some(kind),
            _ => None,
        });
        let edges: Vec<Value> = self
            .calls
            .iter()
            .filter(|edge| edge.carries.iter().any(|carried| carried == id))
            .map(|edge| {
                json!({
                    "id": edge.id, "to": edge.to, "contract": edge.contract, "status": edge.status,
                })
            })
            .collect();
        json!({
            "id": id,
            "kind": owned.map(|entry| &entry.kind).or(never),
            "owner": owned.map(|_| "arca"),
            "status": owned.map(|entry| &entry.status),
            "edges": edges,
            "neverOwns": never.is_some(),
        })
    }
}

impl ToolHandler for OntologyQuery {
    fn call(&self, args: Value, _caller: &Caller) -> Result<Value, CallError> {
        let id = args
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| id.chars().count() <= 128)
            .ok_or_else(|| {
                CallError::Invalid("id: expected a string of at most 128 characters".to_string())
            })?;
        let section: OntologySection =
            serde_json::from_str(include_str!("../../../ontology/arca.json")).map_err(|_| {
                CallError::Internal("the embedded ontology could not be parsed".to_string())
            })?;
        Ok(section.query(id))
    }
}

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

/// A sampler over the host's procfs and sysfs, reading the configured pools through the Incus socket, and the chosen mode
/// with the reason it was chosen. The mode is resolved once, here, from the server the socket reaches: the explicit
/// `incus.local` wins, then the Incus server name against the local host name, then the socket path rule.
pub fn new_sampler(cfg: &NodeConfig) -> (Sampler<UnixIncus>, Arc<Mutex<HostModeChoice>>) {
    let probe = UnixIncus::new(cfg.incus.socket.clone()).with_timeout(MODE_PROBE_TIMEOUT);
    let choice = choose_host_mode(
        &probe,
        cfg.incus.local,
        cfg.incus.socket_is_local(),
        Path::new(PROC_ROOT),
    );
    eprintln!(
        "arca: metrics source {}: {}",
        choice.mode.as_str(),
        choice.reason
    );
    let shared = Arc::new(Mutex::new(choice.clone()));
    let retry = choice.provisional.then_some(ModeRetry {
        explicit: cfg.incus.local,
        path_is_local: cfg.incus.socket_is_local(),
    });
    let sampler = Sampler::new(
        SamplerConfig {
            proc_root: PathBuf::from(PROC_ROOT),
            sys_root: PathBuf::from(SYS_ROOT),
            pools: cfg.incus.pools.clone(),
            interval_ms: u64::try_from(SAMPLE_INTERVAL.as_millis()).unwrap_or(u64::MAX),
            history_minutes: HISTORY_MINUTES,
            incus_local: choice.is_local(),
            mode_retry: retry,
        },
        UnixIncus::new(cfg.incus.socket.clone()),
    )
    .with_mode_choice(Arc::clone(&shared));
    (sampler, shared)
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
    watch_snapshot_scoped(incus, phones, live, now, true)
}

/// A standalone reader without desired service entries excludes the service project and cannot claim completeness.
pub fn watch_snapshot_scoped<R: IncusRead>(
    incus: R,
    phones: &BTreeSet<String>,
    live: &BTreeSet<EntryId>,
    now: u64,
    include_services: bool,
) -> Result<Value, CallError> {
    let mut provider = ReadOnlyIncusProvider::new(incus);
    for _ in 0..WATCH_ATTEMPTS {
        let listed = listed_projects(&provider)?;
        let mut scope = listed.clone();
        let complete = include_services || !scope.remove(arca_netproject::PROJECT);
        let cfg = WatchConfig {
            identity_phones: phones.clone(),
            live_entries: live.clone(),
            projects: scope.clone(),
            complete,
            ..WatchConfig::default()
        };
        let report = watch_once(&mut provider, &cfg, now)
            .map_err(|e| CallError::Internal(format!("the watch failed: {e}")))?;
        if listed_projects(&provider)?.iter().all(|name| {
            scope.contains(name) || (!include_services && name == arca_netproject::PROJECT)
        }) {
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
    pub mode: &'static str,
    pub applied_seq: AppliedSeq,
    pub started: Instant,
    pub socket: PathBuf,
    /// The host-reading mode chosen at startup, and why. A provisional choice is replaced by the sampler when a later
    /// server-name probe succeeds.
    pub metrics_source: Arc<Mutex<HostModeChoice>>,
}

impl ToolHandler for StatusHandler {
    fn call(&self, _args: Value, _caller: &Caller) -> Result<Value, CallError> {
        let applied_seq = self.applied_seq.load();
        let incus_reachable = UnixIncus::new(self.socket.clone()).get("/1.0").is_ok();
        let metrics_source = self
            .metrics_source
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        Ok(json!({
            "version": env!("CARGO_PKG_VERSION"),
            "node": self.node,
            "mode": self.mode,
            "uptimeS": self.started.elapsed().as_secs(),
            "incusReachable": incus_reachable,
            "appliedSeq": applied_seq,
            "metricsSource": metrics_source.mode.as_str(),
            "metricsSourceReason": metrics_source.reason,
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
        let read = store.resolved_since(now.saturating_sub(RESOLVED_WINDOW_MS), RESOLVED_LIMIT);
        let store_bytes = store.bytes();
        drop(store);
        // A failed history read must not hide the firing alerts, so it empties `resolvedRecent` and reports itself.
        let (resolved, read_failure): (Vec<Value>, Option<String>) = match read {
            Ok(events) => (
                events
                    .iter()
                    .map(|event| event.to_json(&self.node))
                    .collect(),
                None,
            ),
            Err(error) => {
                let message = format!("the resolved alerts could not be read: {error}");
                eprintln!("arca: {message}");
                (Vec::new(), Some(message))
            }
        };
        let store_error = match (&snapshot.store_error, read_failure) {
            (Some(error), _) => Some(json!({ "at": error.at, "message": error.message })),
            (None, Some(message)) => Some(json!({ "at": now, "message": message })),
            (None, None) => None,
        };
        Ok(json!({
            "firing": firing,
            "unknown": unknown,
            "disabled": snapshot.disabled,
            "resolvedRecent": resolved,
            "sampleAgeMs": sample_age,
            "watchAgeMs": snapshot.watch_at.map(|taken| now.saturating_sub(taken)),
            "commitMs": snapshot.commit_ms,
            "storeError": store_error,
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

/// `journal_list`: a viewer read over the effect journal. It exposes only the intent and object fields, never a
/// `pre_image`, `inverse` or ACL patch, which can hold rendered secrets. Paging is by sequence with a limit.
pub struct JournalListHandler {
    pub state_dir: PathBuf,
}

impl ToolHandler for JournalListHandler {
    fn call(&self, args: Value, _caller: &Caller) -> Result<Value, CallError> {
        let after = args.get("after").and_then(Value::as_u64).unwrap_or(0);
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(100)
            .min(500);
        let (records, _) = read_journal(&self.state_dir.join(JOURNAL_FILE))
            .map_err(|_| CallError::Internal("the journal could not be read".to_string()))?;
        // The journal's own view is the newest record for each effect id. Each state change appends a full copy, so a
        // listing that returned every line would repeat an effect and could skip a later state when paging by id.
        let mut latest = BTreeMap::new();
        for record in records {
            latest.insert(record.id, record);
        }
        let records: Vec<Value> = latest
            .into_values()
            .filter(|record| record.id > after)
            .take(limit as usize)
            .map(|record| {
                let mut value = json!({
                    "id": record.id,
                    "entry": record.entry.0,
                    "class": record.class,
                    "target": record.target,
                    "state": record.state,
                    "at_ms": record.at_ms,
                });
                if let Some(plugin) = record.plugin {
                    value["plugin"] = json!(plugin.0);
                }
                if let Some(intent) = record.intent {
                    value["intent"] = json!(intent);
                }
                if let Some(object) = record.object {
                    value["object"] = json!(object);
                }
                value
            })
            .collect();
        Ok(json!({ "records": records }))
    }
}

/// `token_list`: the id, scope and optional tool list of each live token. No secret and no digest.
pub struct TokenListHandler {
    pub tokens: TokenStore,
}

impl ToolHandler for TokenListHandler {
    fn call(&self, _args: Value, _caller: &Caller) -> Result<Value, CallError> {
        let tokens: Vec<Value> = self
            .tokens
            .ids()
            .into_iter()
            .map(|(id, scope, tools)| {
                let mut token = json!({ "id": id, "scope": scope.as_str() });
                if let Some(tools) = tools {
                    token["tools"] = json!(tools);
                }
                token
            })
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
        list_plans(&kernel, now)
    }
}

/// The waiting plan list, shared by `plan_list` and the open approval page. Each listing draws a new nonce, so every
/// statement is built fresh.
pub(crate) fn list_plans(kernel: &Kernel, now: u64) -> Result<Value, CallError> {
    let plans = kernel
        .pending_plans()
        .iter()
        .filter(|plan| now.saturating_sub(plan.created_ms) <= PLAN_TTL_MS)
        .map(|plan| plan_json(kernel.host(), plan, now))
        .collect::<Result<Vec<Value>, CallError>>()?;
    Ok(json!({ "plans": plans }))
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
        let now = (self.clock)();
        let mut kernel = lock(&self.kernel)?;
        approve_plan(&mut kernel, &args, now)
    }
}

/// The approval path shared by `approval_submit` and the open approval page: check the proof for the named waiting plan and
/// run its delete once. The approval string is read and never stored or echoed.
pub(crate) fn approve_plan(
    kernel: &mut Kernel,
    args: &Value,
    now: u64,
) -> Result<Value, CallError> {
    let digest = plan_argument(args)?;
    let approval = args
        .get("approval")
        .and_then(Value::as_str)
        .ok_or_else(|| CallError::Invalid("approval is required".to_string()))?;
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
pub(crate) fn refusal_code(refusal: &ApprovalRefusal) -> &'static str {
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

    #[test]
    fn ontology_direct_calls_validate_id_by_character_count() {
        let caller = Caller {
            token_id: "viewer".to_string(),
            scope: Scope::Viewer,
            tools: None,
        };
        for args in [
            json!({}),
            json!({"id": null}),
            json!({"id": 1}),
            json!({"id": "é".repeat(129)}),
        ] {
            assert!(matches!(
                OntologyQuery.call(args, &caller),
                Err(CallError::Invalid(_))
            ));
        }
        assert!(
            OntologyQuery
                .call(json!({"id": "é".repeat(128)}), &caller)
                .is_ok()
        );
    }

    #[test]
    fn ontology_edges_follow_carries_for_owned_never_owned_and_unknown_ids() {
        let section: OntologySection = serde_json::from_value(json!({
            "owns": [{"id": "owned", "kind": "tool", "status": "live", "split": {"by": "kind", "value": "arca"}}],
            "neverOwns": [{"id": "owned", "kind": "data"}, {"prefix": "foreign-", "kind": "data"}],
            "calls": [
                {"id": "outgoing", "to": "droidd", "contract": "mcp", "status": "planned",
                 "endpoint": "arca-mcp", "carries": ["owned", "foreign-data", "unknown"], "requires": ["permission"]},
                {"id": "unrelated", "to": "omni", "contract": "api", "status": "live", "carries": [], "requires": ["unknown"]}
            ]
        })).expect("section");
        for id in ["owned", "foreign-data", "unknown"] {
            assert_eq!(
                section.query(id)["edges"],
                json!([{
                    "id": "outgoing", "to": "droidd", "contract": "mcp", "status": "planned"
                }])
            );
        }
        let owned = section.query("owned");
        assert_eq!(owned["owner"], "arca");
        assert_eq!(owned["kind"], "tool");
        assert_eq!(owned["status"], "live");
        assert_eq!(owned["neverOwns"], true);
        assert_eq!(section.query("foreign-data")["kind"], "data");
        assert_eq!(section.query("unknown")["kind"], Value::Null);
        assert_eq!(section.query("foreign")["neverOwns"], false);
    }

    const PROJECTS_PATH: &str = "/1.0/projects?recursion=1";
    /// The instance list the sampler reads each tick, with each row's state.
    const SAMPLED_INSTANCES: &str = "/1.0/instances?all-projects=true&recursion=2";

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
            // The watch reads the recursion-one listing, and the sampler reads the recursion-two listing in the same tick.
            if path == INSTANCES_ALL_PROJECTS || path == SAMPLED_INSTANCES {
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
    fn the_standalone_watch_excludes_services_and_does_not_claim_missing_objects() {
        let incus = Scripted {
            index: Mutex::new(VecDeque::from(vec![vec![
                "default",
                arca_netproject::PROJECT,
            ]])),
            instances: Mutex::new(VecDeque::from(vec![vec![
                instance("default", "guest"),
                instance(arca_netproject::PROJECT, "nats"),
            ]])),
        };
        let report = watch_snapshot_scoped(
            incus,
            &BTreeSet::new(),
            &BTreeSet::from([EntryId("adopt-missing".into())]),
            7,
            false,
        )
        .unwrap();
        assert_eq!(
            rows_by_project(&report),
            vec![("default".into(), vec!["guest".into()])]
        );
        assert_eq!(report["drift"], json!([]));
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
                incus_local: true,
                mode_retry: None,
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
                incus_local: true,
                mode_retry: None,
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
            tools: None,
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

/// Build one entry without secret values in its config.
pub fn service_entry(id: &str, plugin: &str, config: Value) -> arca_kernel::desired::Entry {
    arca_kernel::desired::Entry {
        id: EntryId(id.into()),
        plugin: arca_kernel::desired::PluginId(plugin.into()),
        version: "0.1.0".into(),
        digest: Digest::of(
            arca_kernel::canon::canonical_json(&config)
                .expect("config JSON")
                .as_bytes(),
        ),
        signer: "node".into(),
        config,
        disabled: false,
    }
}

/// Read the host's default profile once; the service project's own profile has no root disk.
pub fn service_pool(cfg: &NodeConfig) -> Result<String, CallError> {
    let profile = UnixIncus::new(cfg.incus.socket.clone())
        .get("/1.0/profiles/default?project=default")
        .map_err(|_| pg_invalid("default profile root pool is unavailable"))?;
    profile["devices"]["root"]["pool"]
        .as_str()
        .filter(|p| arca_postgres::pool_ok(p))
        .map(str::to_string)
        .ok_or_else(|| pg_invalid("default profile must name a valid devices.root.pool"))
}

/// The shared network is the first change when absent, in the caller's one document.
pub fn ensure_netproject(
    entries: &[arca_kernel::desired::Entry],
    cfg: &NodeConfig,
) -> Result<Vec<crate::desired_store::EntryChange>, CallError> {
    let cidr = cfg
        .kernel
        .service_cidr
        .as_deref()
        .ok_or_else(|| CallError::Invalid("kernel.serviceCidr is required".into()))?;
    arca_netproject::check_cidr(cidr, &cfg.kernel.reserved_cidrs)
        .map_err(|e| CallError::Invalid(e.into()))?;
    if !cfg
        .kernel
        .projects
        .iter()
        .any(|p| p == arca_netproject::PROJECT)
    {
        return Err(CallError::Invalid(
            "kernel.projects must include the service project".into(),
        ));
    }
    if cfg
        .kernel
        .droidd_projects
        .iter()
        .any(|p| p == arca_netproject::PROJECT)
        || cfg.kernel.scratch
    {
        return Err(CallError::Invalid(
            "the service project is excluded by kernel configuration".into(),
        ));
    }
    if entries.iter().any(|e| e.id.0 == "net-arcak-svc") {
        return Ok(vec![]);
    }
    Ok(vec![crate::desired_store::EntryChange::Put(service_entry(
        "net-arcak-svc",
        "arca.netproject",
        json!({"project":arca_netproject::PROJECT,"bridge":arca_netproject::BRIDGE,"cidr":cidr,"virtualMachines":0}),
    ))])
}

pub fn postgres_handlers(
    kernel: Arc<Mutex<Kernel>>,
    clock: Clock,
    cfg: Arc<NodeConfig>,
) -> Vec<(&'static str, Arc<dyn ToolHandler>)> {
    // Serialize secret/document transactions without blocking other kernel users during I/O.
    let writes = Arc::new(Mutex::new(()));
    [
        "postgres_create",
        "postgres_list",
        "postgres_link",
        "postgres_unlink",
        "postgres_delete",
    ]
    .into_iter()
    .map(|verb| {
        (
            verb,
            Arc::new(PostgresHandler {
                verb,
                kernel: kernel.clone(),
                clock: clock.clone(),
                cfg: cfg.clone(),
                writes: writes.clone(),
            }) as Arc<dyn ToolHandler>,
        )
    })
    .collect()
}
struct PostgresHandler {
    verb: &'static str,
    kernel: Arc<Mutex<Kernel>>,
    clock: Clock,
    cfg: Arc<NodeConfig>,
    writes: Arc<Mutex<()>>,
}
fn pg_invalid(message: &str) -> CallError {
    CallError::Invalid(message.into())
}
fn pg_reconciled(outcomes: &[Outcome], entries: &[&str]) -> Result<(), CallError> {
    if outcomes.iter().any(|outcome| {
        matches!(outcome, Outcome::Failed { entry, .. } if entries.contains(&entry.0.as_str()))
    }) {
        // Never expose plugin/provider diagnostics or roll back secrets for an
        // accepted desired document: reconciliation can be retried against it.
        return Err(CallError::Internal(
            "the database operation did not reconcile; desired state was saved".into(),
        ));
    }
    Ok(())
}
fn pg_name(args: &Value, key: &str, prefix: &str, max: usize) -> Result<String, CallError> {
    args[key]
        .as_str()
        .filter(|n| arca_netproject::name_ok(n, prefix, max))
        .map(str::to_string)
        .ok_or_else(|| pg_invalid("invalid database or consumer name"))
}
fn pg_entry(
    entries: &[arca_kernel::desired::Entry],
    id: &str,
    plugin: &str,
) -> Result<arca_kernel::desired::Entry, CallError> {
    entries
        .iter()
        .find(|e| e.id.0 == id && e.plugin.0 == plugin && !e.disabled)
        .cloned()
        .ok_or_else(|| pg_invalid("database or consumer entry is absent"))
}

/// Secret directories must be real, private and owned by this process.
fn secret_dir(cfg: &NodeConfig, owner: &str) -> Result<PathBuf, CallError> {
    use std::{
        fs,
        os::unix::fs::{DirBuilderExt, MetadataExt},
    };
    let uid = arca_approval::effective_uid().map_err(|_| internal())?;
    let mut path = cfg.state_dir.clone();
    for part in ["secrets", owner] {
        path.push(part);
        match fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(_) => return Err(internal()),
        }
        let meta = fs::symlink_metadata(&path).map_err(|_| internal())?;
        if !meta.is_dir()
            || meta.file_type().is_symlink()
            || meta.uid() != uid
            || meta.mode() & 0o077 != 0
        {
            return Err(internal());
        }
    }
    Ok(path)
}
fn read_pg_secret(path: &std::path::Path) -> Result<String, CallError> {
    use std::{fs, io::Read, os::unix::fs::MetadataExt};
    let meta = fs::symlink_metadata(path).map_err(|_| internal())?;
    if !meta.is_file()
        || meta.file_type().is_symlink()
        || meta.mode() & 0o777 != 0o600
        || meta.uid() != arca_approval::effective_uid().map_err(|_| internal())?
        || meta.len() > 65536
    {
        return Err(internal());
    }
    let file = fs::File::open(path).map_err(|_| internal())?;
    let opened = file.metadata().map_err(|_| internal())?;
    if (opened.dev(), opened.ino()) != (meta.dev(), meta.ino()) {
        return Err(internal());
    }
    let mut value = String::new();
    file.take(65537)
        .read_to_string(&mut value)
        .map_err(|_| internal())?;
    if value.len() > 65536 {
        return Err(internal());
    }
    Ok(value)
}
fn write_pg_secret(path: &std::path::Path, value: &str) -> Result<(), CallError> {
    use std::{
        fs::{self, File, OpenOptions},
        io::Write,
        os::unix::fs::OpenOptionsExt,
    };
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).map_err(|_| internal())?;
    let nonce: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
    let temp = path.with_file_name(format!(".secret-{nonce}.tmp"));
    let dir = path.parent().ok_or_else(internal)?;
    let result = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp)
        .and_then(|mut file| {
            file.write_all(value.as_bytes())?;
            file.sync_all()?;
            fs::rename(&temp, path)?;
            File::open(dir)?.sync_all()
        })
        .map_err(|_| internal());
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}
fn remove_pg_secret(path: &std::path::Path) -> Result<(), CallError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(internal()),
    }
}
fn previous_pg_secret(path: &std::path::Path) -> Result<Option<String>, CallError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => read_pg_secret(path).map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(internal()),
    }
}
impl ToolHandler for PostgresHandler {
    fn call(&self, args: Value, caller: &Caller) -> Result<Value, CallError> {
        if self.verb != "postgres_list" && caller.scope < Scope::Doctor {
            return Err(CallError::Forbidden {
                verb: self.verb.into(),
            });
        }
        let now = (self.clock)();
        let _write = if self.verb == "postgres_list" {
            None
        } else {
            Some(self.writes.lock().map_err(|_| internal())?)
        };
        let entries = {
            let kernel = lock(&self.kernel)?;
            kernel
                .entries()
                .iter()
                .filter(|e| self.verb != "postgres_list" || e.plugin.0 == "arca.postgres")
                .cloned()
                .collect::<Vec<_>>()
        };
        if self.verb == "postgres_list" {
            let read = UnixIncus::new(self.cfg.incus.socket.clone());
            let mut rows = Vec::new();
            for entry in &entries {
                let cfg: arca_postgres::PostgresConfig =
                    serde_json::from_value(entry.config.clone()).map_err(|_| internal())?;
                let state = match read.get(&format!(
                    "/1.0/instances/{}/state?project={}",
                    cfg.name, cfg.project
                )) {
                    Ok(s) => s,
                    Err(arca_incus::IncusError::Status { code: 404, .. }) => Value::Null,
                    Err(_) => return Err(internal()),
                };
                let address = arca_postgres::ipv4(&state).map(|ip| ip.to_string());
                let status = if address.is_none() {
                    "pending".into()
                } else {
                    state["status"].as_str().unwrap_or("pending").to_lowercase()
                };
                rows.push(json!({"name":cfg.name,"entry":entry.id.0,"status":status,"address":address,"port":5432,"links":cfg.allow}));
            }
            return Ok(json!({"databases":rows}));
        }
        let name = pg_name(&args, "name", "postgres", 31)?;
        let id = format!("postgres-{name}");
        use crate::desired_store::EntryChange::{Put, Remove};
        if self.verb == "postgres_create" {
            let mut changes = ensure_netproject(&entries, &self.cfg)?;
            if entries.iter().any(|e| e.id.0 == id) {
                return Err(pg_invalid("database entry already exists"));
            }
            let image = args["image"]
                .as_str()
                .filter(|i| i.len() <= 512)
                .ok_or_else(|| pg_invalid("image is required"))?;
            arca_postgres::fingerprint(image)
                .map_err(|_| pg_invalid("a digest-pinned image is required"))?;
            let pool = service_pool(&self.cfg)?;
            let path = secret_dir(&self.cfg, &id)?.join("password");
            let mut bytes = [0u8; 32];
            getrandom::fill(&mut bytes).map_err(|_| internal())?;
            let password: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            write_pg_secret(&path, &password)?;
            changes.push(Put(service_entry(&id,"arca.postgres",json!({"name":name,"image":image,"pool":pool,"project":arca_netproject::PROJECT,"bridge":arca_netproject::BRIDGE,"allow":[]}))));
            let proposed = lock(&self.kernel).and_then(|mut kernel| {
                if kernel.entries().iter().any(|e| e.id.0 == id) {
                    return Err(pg_invalid("database entry already exists"));
                }
                kernel.propose(changes, now).map_err(|_| internal())
            });
            let outcomes = match proposed {
                Ok(outcomes) => outcomes,
                Err(error) => {
                    remove_pg_secret(&path)?;
                    return Err(error);
                }
            };
            pg_reconciled(&outcomes, &[&id])?;
            return Ok(json!({"name":name,"entry":id,"status":"pending"}));
        }
        let mut database = pg_entry(&entries, &id, "arca.postgres")?;
        if self.verb == "postgres_delete" {
            let mut changes = vec![Remove(database.id)];
            let mut urls = Vec::new();
            for mut entry in entries
                .iter()
                .filter(|e| e.plugin.0 == "arca.consumer")
                .cloned()
            {
                let mut cfg: arca_postgres::consumer::ConsumerConfig =
                    serde_json::from_value(entry.config.clone()).map_err(|_| internal())?;
                if !cfg.links.iter().any(|link| link.database == name) {
                    continue;
                }
                cfg.links.retain(|link| link.database != name);
                urls.push(
                    self.cfg
                        .state_dir
                        .join("secrets")
                        .join(&entry.id.0)
                        .join(format!("{name}-url")),
                );
                let updated = service_entry(
                    &entry.id.0,
                    "arca.consumer",
                    serde_json::to_value(cfg).map_err(|_| internal())?,
                );
                entry.config = updated.config;
                entry.digest = updated.digest;
                changes.push(Put(entry));
            }
            let (plan, host) = {
                let mut kernel = lock(&self.kernel)?;
                kernel.propose(changes, now).map_err(|_| internal())?;
                (
                    kernel.pending_plans().into_iter().find(|p| p.entry.0 == id),
                    kernel.host().to_string(),
                )
            };
            for path in urls {
                remove_pg_secret(&path)?;
            }
            let dir = self.cfg.state_dir.join("secrets").join(&id);
            remove_pg_secret(&dir.join("password"))?;
            match std::fs::remove_dir(&dir) {
                Ok(()) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
                    ) => {}
                Err(_) => return Err(internal()),
            }
            return Ok(
                json!({"removed":plan.is_none(), "plan":plan.as_ref().map(|p| plan_json(&host,p,now)).transpose()?}),
            );
        }
        let consumer = pg_name(&args, "consumer", "consumer", 63)?;
        let mut target = pg_entry(&entries, &format!("consumer-{consumer}"), "arca.consumer")?;
        let mut cc: arca_postgres::consumer::ConsumerConfig =
            serde_json::from_value(target.config.clone()).map_err(|_| internal())?;
        let mut db: arca_postgres::PostgresConfig =
            serde_json::from_value(database.config.clone()).map_err(|_| internal())?;
        let mut env: Vec<String> = cc
            .links
            .iter()
            .filter(|l| l.database == name)
            .map(|l| l.env.clone())
            .collect();
        let linking = self.verb == "postgres_link";
        let url_path = self
            .cfg
            .state_dir
            .join("secrets")
            .join(&target.id.0)
            .join(format!("{name}-url"));
        let mut previous_url = None;
        if linking {
            if env.is_empty() {
                let key = if cc.links.is_empty() {
                    "DATABASE_URL".into()
                } else {
                    format!("DATABASE_URL_{}", name.to_uppercase().replace('-', "_"))
                };
                if cc.links.iter().any(|l| l.env == key) {
                    return Err(pg_invalid("link environment name is already in use"));
                }
                cc.links.push(arca_postgres::consumer::ConsumerLink {
                    database: name.clone(),
                    env: key.clone(),
                });
                env.push(key);
            }
            let password = read_pg_secret(&secret_dir(&self.cfg, &id)?.join("password"))?;
            let url = format!("postgresql://postgres:{password}@{name}:5432/postgres");
            secret_dir(&self.cfg, &target.id.0)?;
            previous_url = previous_pg_secret(&url_path)?;
            write_pg_secret(&url_path, &url)?;
            if !db.allow.contains(&consumer) {
                db.allow.push(consumer.clone());
            }
        } else {
            cc.links.retain(|l| l.database != name);
            db.allow.retain(|c| c != &consumer);
        }
        target = service_entry(
            &target.id.0,
            "arca.consumer",
            serde_json::to_value(cc).map_err(|_| internal())?,
        );
        database = service_entry(
            &id,
            "arca.postgres",
            serde_json::to_value(db).map_err(|_| internal())?,
        );
        let consumer_id = target.id.0.clone();
        let proposed = lock(&self.kernel).and_then(|mut kernel| {
            kernel
                .propose(vec![Put(target), Put(database)], now)
                .map_err(|_| internal())
        });
        let outcomes = match proposed {
            Ok(outcomes) => outcomes,
            Err(error) => {
                if linking {
                    match previous_url {
                        Some(value) => write_pg_secret(&url_path, &value)?,
                        None => remove_pg_secret(&url_path)?,
                    }
                }
                return Err(error);
            }
        };
        pg_reconciled(&outcomes, &[&id, &consumer_id])?;
        if !linking {
            remove_pg_secret(&url_path)?;
        }
        Ok(if linking {
            json!({"name":name,"consumer":consumer,"env":env})
        } else {
            json!({"name":name,"consumer":consumer,"removed":env})
        })
    }
}

#[path = "service_wiring.rs"]
mod service_wiring;

// Services: one record in the desired document, plus the live Incus row for state.

pub fn service_handlers(
    kernel: Arc<Mutex<Kernel>>,
    clock: Clock,
    cfg: Arc<NodeConfig>,
) -> Vec<(&'static str, Arc<dyn ToolHandler>)> {
    let writes = Arc::new(Mutex::new(()));
    let exec = Arc::new(crate::service_exec::ServiceExec::new(cfg.clone()));
    [
        "service_connect",
        "service_disconnect",
        "service_secret_set",
        "service_publish",
        "service_unpublish",
        "service_create",
        "service_list",
        "service_status",
        "service_logs",
        "service_remove",
    ]
    .into_iter()
    .map(|verb| {
        (
            verb,
            Arc::new(ServiceHandler {
                verb,
                kernel: kernel.clone(),
                clock: clock.clone(),
                cfg: cfg.clone(),
                writes: writes.clone(),
                exec: exec.clone(),
            }) as Arc<dyn ToolHandler>,
        )
    })
    .collect()
}

struct ServiceHandler {
    verb: &'static str,
    kernel: Arc<Mutex<Kernel>>,
    clock: Clock,
    cfg: Arc<NodeConfig>,
    writes: Arc<Mutex<()>>,
    exec: Arc<crate::service_exec::ServiceExec>,
}

fn service_refused(message: impl Into<String>) -> CallError {
    CallError::Refused {
        code: "refused".into(),
        message: message.into(),
    }
}

fn service_not_found(message: impl Into<String>) -> CallError {
    CallError::Refused {
        code: "not_found".into(),
        message: message.into(),
    }
}

fn service_template_error(error: crate::service::TemplateError) -> CallError {
    if error.not_found {
        service_not_found(error.message)
    } else {
        service_refused(error.message)
    }
}

/// The service project's CIDR and gateway, or a refusal before any Incus read.
fn service_project(cfg: &NodeConfig) -> Result<(String, String), CallError> {
    let cidr = cfg
        .kernel
        .service_cidr
        .as_deref()
        .ok_or_else(|| service_refused("no service project: kernel.serviceCidr is required"))?;
    let gateway = crate::service::gateway(cidr)
        .map_err(|_| service_refused("no service project: the service CIDR is invalid"))?;
    if !cfg
        .kernel
        .projects
        .iter()
        .any(|project| project == arca_netproject::PROJECT)
        || cfg
            .kernel
            .droidd_projects
            .iter()
            .any(|project| project == arca_netproject::PROJECT)
        || cfg.kernel.scratch
    {
        return Err(service_refused(
            "no service project: the service project is excluded by kernel configuration",
        ));
    }
    Ok((cidr.to_string(), gateway.to_string()))
}

fn service_listing(read: &UnixIncus, project: Option<&str>) -> Result<Vec<Value>, CallError> {
    let path = project
        .map(|project| format!("/1.0/instances?project={project}&recursion=2"))
        .unwrap_or_else(|| "/1.0/instances?all-projects=true&recursion=2".into());
    match read.get(&path) {
        Ok(rows) => Ok(rows.as_array().cloned().unwrap_or_default()),
        Err(arca_incus::IncusError::Status { code: 404, .. }) => Ok(Vec::new()),
        Err(_) => Err(internal()),
    }
}

fn service_leases(read: &UnixIncus) -> Result<Vec<String>, CallError> {
    let path = format!(
        "/1.0/networks/{}/leases?project={}",
        arca_netproject::BRIDGE,
        arca_netproject::PROJECT
    );
    match read.get(&path) {
        Ok(rows) => Ok(rows
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|lease| lease["address"].as_str().map(str::to_string))
            .collect()),
        Err(arca_incus::IncusError::Status { code: 404, .. }) => Ok(Vec::new()),
        Err(_) => Err(internal()),
    }
}

fn service_secret_names(dir: &std::path::Path) -> std::io::Result<BTreeSet<std::ffi::OsString>> {
    match std::fs::read_dir(dir) {
        Ok(files) => files
            .map(|file| file.map(|file| file.file_name()))
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeSet::new()),
        Err(error) => Err(error),
    }
}

fn cleanup_service_secrets(dir: &std::path::Path, seeded: &BTreeSet<std::ffi::OsString>) {
    if let Ok(names) = service_secret_names(dir) {
        for name in names.difference(seeded) {
            let _ = std::fs::remove_file(dir.join(name));
        }
        let _ = std::fs::remove_dir(dir); // Only succeeds when empty.
    }
}

fn service_used(
    rows: &[Value],
    leases: &[String],
    entries: &[arca_kernel::desired::Entry],
) -> BTreeSet<std::net::Ipv4Addr> {
    let mut used = BTreeSet::new();
    for row in rows {
        if let Some(devices) = row["devices"].as_object() {
            for device in devices.values() {
                if let Some(ip) = device["ipv4.address"]
                    .as_str()
                    .and_then(|ip| ip.parse().ok())
                {
                    used.insert(ip);
                }
            }
        }
        if let Some(network) = row["state"]["network"].as_object() {
            for nic in network.values() {
                for address in nic["addresses"].as_array().into_iter().flatten() {
                    if address["family"] == "inet"
                        && let Some(ip) = address["address"].as_str().and_then(|ip| ip.parse().ok())
                    {
                        used.insert(ip);
                    }
                }
            }
        }
    }
    for lease in leases {
        if let Ok(ip) = lease.parse() {
            used.insert(ip);
        }
    }
    for (_, config) in service_entries(entries) {
        if let Ok(ip) = config.ipv4.parse() {
            used.insert(ip);
        }
    }
    used
}

fn service_live_in<'a>(rows: &'a [Value], project: &str, name: &str) -> Option<&'a Value> {
    rows.iter()
        .find(|row| row["name"].as_str() == Some(name) && row["project"].as_str() == Some(project))
}

fn service_entries(
    entries: &[arca_kernel::desired::Entry],
) -> Vec<(arca_kernel::desired::Entry, crate::service::ServiceConfig)> {
    entries
        .iter()
        .filter(|entry| entry.plugin.0 == "arca.service")
        .filter_map(|entry| {
            serde_json::from_value(entry.config.clone())
                .ok()
                .map(|config| (entry.clone(), config))
        })
        .collect()
}

fn split_list(text: Option<&str>) -> Vec<&str> {
    text.unwrap_or("")
        .split(',')
        .filter(|part| !part.is_empty())
        .collect()
}

fn ipv4_in_cidr(ip: std::net::Ipv4Addr, cidr: &str) -> bool {
    let Some((network, prefix)) = cidr.split_once('/') else {
        return false;
    };
    let (Ok(network), Ok(prefix)) = (network.parse::<std::net::Ipv4Addr>(), prefix.parse::<u32>())
    else {
        return false;
    };
    if prefix > 32 {
        return false;
    }
    let mask = u32::MAX.checked_shl(32 - prefix).unwrap_or(0);
    u32::from(ip) & mask == u32::from(network) & mask
}

fn node_ok(node: &str) -> bool {
    !node.is_empty()
        && node.len() <= 63
        && node
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !node.starts_with('-')
        && !node.ends_with('-')
}

/// One marked instance's record, from its `user.arca.*` keys and NIC, without reading or changing its environment.
fn adopted_config(
    row: &Value,
    rows: &[Value],
    cidr: &str,
    gateway: &str,
    now: u64,
) -> Option<crate::service::ServiceConfig> {
    let name = row["name"].as_str()?;
    if !crate::service::service_name_ok(name) {
        return None;
    }
    let project = row["project"].as_str()?;
    let config = row["config"].as_object()?;
    let template = config.get(crate::service::TEMPLATE_KEY)?.as_str()?;
    if !crate::service::template_name_ok(template) {
        return None;
    }
    let ipv4_text = config.get(crate::service::ADDRESS_KEY)?.as_str()?;
    let ipv4 = ipv4_text.parse::<std::net::Ipv4Addr>().ok()?;
    if !ipv4_in_cidr(ipv4, cidr)
        || row["devices"]["eth0"]["ipv4.address"].as_str() != Some(ipv4_text)
    {
        return None;
    }
    let ports_text = config.get(crate::service::PORTS_KEY)?.as_str()?;
    let mut ports = Vec::new();
    for part in split_list(Some(ports_text)) {
        let port = part.parse::<u16>().ok().filter(|port| *port >= 1)?;
        if port.to_string() != part {
            return None;
        }
        ports.push(port);
    }
    let published = match config
        .get(crate::service::PUBLISHED_KEY)
        .and_then(Value::as_str)
    {
        Some(node) if node_ok(node) => Some(crate::service::Published {
            node: node.to_string(),
            port: 8443,
            reach: "tailnet".into(),
        }),
        Some(_) => return None,
        None => None,
    };
    let mut links = Vec::new();
    for entry in split_list(
        config
            .get(crate::service::LINKS_KEY)
            .and_then(Value::as_str),
    ) {
        let mut parts = entry.splitn(3, ':');
        let to = parts.next()?;
        let port = parts.next()?.parse::<u16>().ok()?;
        let uuid = parts.next()?;
        if !crate::service::service_name_ok(to) || port == 0 {
            return None;
        }
        let provider = rows.iter().find(|candidate| {
            candidate["name"].as_str() == Some(to)
                && candidate["config"]["volatile.uuid"].as_str() == Some(uuid)
        })?;
        if provider["name"].as_str() == Some(name) {
            continue;
        }
        links.push(crate::service::ServiceLink {
            to: to.to_string(),
            port,
            env: Vec::new(),
            uuid: uuid.to_string(),
        });
    }
    Some(crate::service::ServiceConfig {
        name: name.to_string(),
        template: template.to_string(),
        instance: name.to_string(),
        ipv4: ipv4_text.to_string(),
        ports,
        links,
        incoming: Vec::new(),
        secret_refs: BTreeMap::new(),
        environment_patch: None,
        published,
        created: now,
        source: json!({}),
        pool: String::new(),
        project: project.to_string(),
        bridge: String::new(),
        gateway: gateway.to_string(),
        entrypoint: None,
        environment: BTreeMap::new(),
        droidd_port: None,
        limits: None,
        autostart: false,
        data: None,
        lan: None,
        start: false,
        adopted: true,
        creating: false,
    })
}

/// Whether an instance is in the service project or carries its kernel import mark.
fn service_owned(row: &Value) -> bool {
    row["project"].as_str() == Some(arca_netproject::PROJECT)
        || row["name"].as_str().is_some_and(|name| {
            row["config"][arca_kernel::ownership::KERNEL_ENTRY_KEY].as_str()
                == Some(format!("adopt-{name}").as_str())
        })
}

/// Adds a record for every kernel-owned marked instance without one. Nothing on the instance is read into a secret or re-rendered.
fn adopt_services(
    kernel: &Mutex<Kernel>,
    entries: &[arca_kernel::desired::Entry],
    rows: &[Value],
    cidr: &str,
    gateway: &str,
    now: u64,
) -> Result<(), CallError> {
    let mut changes = Vec::new();
    for row in rows {
        if !service_owned(row) {
            continue;
        }
        let Some(config) = adopted_config(row, rows, cidr, gateway, now) else {
            continue;
        };
        let id = format!("service-{}", config.name);
        if entries
            .iter()
            .any(|entry| entry.id.0 == id && entry.plugin.0 == "arca.service")
        {
            continue;
        }
        changes.push(crate::desired_store::EntryChange::Put(service_entry(
            &id,
            "arca.service",
            serde_json::to_value(&config).map_err(|_| internal())?,
        )));
    }
    if !changes.is_empty() {
        lock(kernel)?
            .propose(changes, now)
            .map_err(|_| internal())?;
    }
    Ok(())
}

fn service_secret_values(dir: &std::path::Path) -> Vec<String> {
    let mut values = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return values;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if let Ok(files) = std::fs::read_dir(&path) {
            for file in files.flatten() {
                if let Ok(text) = std::fs::read_to_string(file.path())
                    && text.len() >= 8
                    && text.len() <= 4096
                {
                    values.push(text.trim_end_matches('\n').to_string());
                }
            }
        }
    }
    values
}

/// Reports each service object's observed state after an approved removal could not finish.
fn service_removal_refused(
    read: &impl IncusRead,
    config: &crate::service::ServiceConfig,
) -> CallError {
    let state = |path: String, object: &str| match read.get(&path) {
        Ok(_) => format!("{object} remains"),
        Err(arca_incus::IncusError::Status { code: 404, .. }) => format!("{object} removed"),
        Err(_) => format!("{object} state unknown"),
    };
    let instance = state(
        format!(
            "/1.0/instances/{}?project={}",
            config.instance, config.project
        ),
        "instance",
    );
    let acl = state(
        format!("/1.0/network-acls/{}?project=default", config.acl()),
        "ACL",
    );
    service_refused(format!(
        "remove service {}: partial removal ({instance}; {acl}); the owner's approval is spent, so another try needs a new one",
        config.name
    ))
}

fn service_config(parts: ServiceParts<'_>) -> Result<crate::service::ServiceConfig, CallError> {
    let source = crate::service::source(parts.template).map_err(service_template_error)?;
    let entrypoint = crate::service::entrypoint(parts.template, parts.templates_dir)
        .map_err(service_template_error)?;
    let (environment, start) = crate::service::environment(
        parts.template,
        parts.name,
        &parts.ipv4,
        &parts.gateway,
        parts.secrets_dir,
    )
    .map_err(service_template_error)?;
    Ok(crate::service::ServiceConfig {
        name: parts.name.to_string(),
        template: parts.template.name.clone(),
        instance: parts.name.to_string(),
        ipv4: parts.ipv4,
        ports: parts.template.ports.clone(),
        links: Vec::new(),
        incoming: Vec::new(),
        secret_refs: BTreeMap::new(),
        environment_patch: None,
        published: None,
        created: parts.now,
        source,
        pool: parts.pool,
        project: arca_netproject::PROJECT.into(),
        bridge: arca_netproject::BRIDGE.into(),
        gateway: parts.gateway,
        entrypoint,
        environment,
        droidd_port: parts.template.droidd_port,
        limits: parts.template.limits.clone(),
        autostart: parts.template.autostart,
        data: parts.template.data.clone(),
        lan: parts.template.lan.clone(),
        start,
        adopted: false,
        creating: false,
    })
}

struct ServiceParts<'a> {
    name: &'a str,
    template: &'a crate::service::Template,
    ipv4: String,
    gateway: String,
    pool: String,
    now: u64,
    secrets_dir: &'a std::path::Path,
    templates_dir: &'a std::path::Path,
}

fn service_name(args: &Value) -> Result<String, CallError> {
    args["name"]
        .as_str()
        .filter(|name| crate::service::service_name_ok(name))
        .map(str::to_string)
        .ok_or_else(|| CallError::Invalid("name: expected a service name".into()))
}

impl ToolHandler for ServiceHandler {
    fn call(&self, args: Value, caller: &Caller) -> Result<Value, CallError> {
        let allowed: &[&str] = match self.verb {
            "service_create" => &["name", "template"],
            "service_status" => &["name"],
            "service_logs" => &["name", "lines"],
            "service_remove" => &["name", "approval"],
            "service_connect" | "service_disconnect" => &["from", "to"],
            "service_secret_set" => &["name", "key", "value"],
            "service_publish" => &["name", "reach", "approval"],
            "service_unpublish" => &["name"],
            _ => &[],
        };
        let object = args
            .as_object()
            .ok_or_else(|| CallError::Invalid("arguments: expected an object".into()))?;
        if object.keys().any(|key| !allowed.contains(&key.as_str())) {
            return Err(CallError::Invalid(
                "arguments: a field the schema does not name".into(),
            ));
        }
        if self.verb == "service_create" && caller.scope < Scope::Doctor {
            return Err(CallError::Forbidden {
                verb: self.verb.into(),
            });
        }
        if matches!(
            self.verb,
            "service_connect"
                | "service_disconnect"
                | "service_secret_set"
                | "service_publish"
                | "service_unpublish"
        ) && caller.scope < Scope::Doctor
        {
            return Err(CallError::Forbidden {
                verb: self.verb.into(),
            });
        }
        let _publish = matches!(
            self.verb,
            "service_publish" | "service_unpublish" | "service_remove"
        )
        .then(|| self.exec.publish_gate.blocking_lock());
        let _write = if matches!(
            self.verb,
            "service_create" | "service_connect" | "service_disconnect" | "service_secret_set"
        ) {
            Some(self.writes.lock().map_err(|_| internal())?)
        } else {
            None
        };
        let now = (self.clock)();
        let (cidr, gateway) = service_project(&self.cfg)?;
        let read = UnixIncus::new(self.cfg.incus.socket.clone());
        let rows = service_listing(&read, None)?;
        let mut entries = {
            let kernel = lock(&self.kernel)?;
            kernel.entries().to_vec()
        };
        adopt_services(&self.kernel, &entries, &rows, &cidr, &gateway, now)?;
        entries = {
            let kernel = lock(&self.kernel)?;
            kernel.entries().to_vec()
        };
        if matches!(
            self.verb,
            "service_connect"
                | "service_disconnect"
                | "service_secret_set"
                | "service_publish"
                | "service_unpublish"
        ) {
            return self.call_wiring(&args, caller, &entries, now);
        }
        match self.verb {
            "service_list" => {
                let mut views = Vec::new();
                let records = service_entries(&entries);
                let names: BTreeSet<String> = records
                    .iter()
                    .map(|(_, config)| config.name.clone())
                    .collect();
                for (_, config) in &records {
                    let live = service_live_in(&rows, &config.project, &config.name);
                    views.push(crate::service::view(config, live));
                }
                for row in &rows {
                    if let Some(name) = row["name"].as_str()
                        && !names.contains(name)
                        && service_owned(row)
                    {
                        let view = if row["config"][crate::service::TEMPLATE_KEY].is_string() {
                            crate::service::failed_view(row)
                        } else if row["project"].as_str() == Some(arca_netproject::PROJECT) {
                            crate::service::unknown_view(row)
                        } else {
                            None
                        };
                        if let Some(view) = view {
                            views.push(view);
                        }
                    }
                }
                views.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
                Ok(json!({ "services": views }))
            }
            "service_status" => {
                let name = service_name(&args)?;
                let record = service_entries(&entries)
                    .into_iter()
                    .find(|(_, config)| config.name == name);
                if let Some((_, config)) = record {
                    return Ok(crate::service::view(
                        &config,
                        service_live_in(&rows, &config.project, &config.name),
                    ));
                }
                if let Some(row) = rows
                    .iter()
                    .find(|row| row["name"].as_str() == Some(name.as_str()) && service_owned(row))
                {
                    if let Some(view) = crate::service::failed_view(row) {
                        return Ok(view);
                    }
                    if let Some(view) = crate::service::unknown_view(row) {
                        return Ok(view);
                    }
                }
                Err(service_not_found(format!("service {name}: not found")))
            }
            "service_logs" => {
                let name = service_name(&args)?;
                let lines = match args.get("lines") {
                    None => 50,
                    Some(value) => value
                        .as_u64()
                        .ok_or_else(|| CallError::Invalid("lines: expected 1 to 200".into()))?,
                };
                if !(1..=200).contains(&lines) {
                    return Err(CallError::Invalid("lines: expected 1 to 200".into()));
                }
                let record = service_entries(&entries)
                    .into_iter()
                    .find(|(_, config)| config.name == name);
                let Some((entry, config)) = record else {
                    return Err(service_not_found(format!("service {name}: not found")));
                };
                let row = service_live_in(&rows, &config.project, &name).ok_or_else(|| {
                    service_refused(format!("service {name}: its instance is gone"))
                })?;
                let path = format!(
                    "/1.0/instances/{name}/console?project={}&type=log",
                    config.project
                );
                let journal = if !config.adopted && config.source["protocol"] != "oci" {
                    if let Some(_permit) = self.exec.journal_permit() {
                        let target = self.exec_target(&entry, &config)?;
                        let cmd = arca_incus::exec::ExecCommand::Journal(
                            arca_incus::exec::JournalLines::new(lines).map_err(|_| {
                                CallError::Invalid("lines: expected 1 to 200".into())
                            })?,
                        );
                        match self.exec.run(
                            &target,
                            &caller.token_id,
                            self.verb,
                            &cmd,
                            &service_secret_values(&self.cfg.state_dir.join("secrets")),
                            None,
                        ) {
                            Ok(text) => String::from_utf8(text).ok(),
                            Err(error) if error.ownership() => return Err(error.call_error()),
                            Err(_) => None,
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };
                let text = match journal {
                    Some(text) => text,
                    None => read.get_text(&path).map_err(|error| match error {
                        arca_incus::IncusError::Status { code: 404, .. } => {
                            service_not_found(format!("service {name}: no log"))
                        }
                        _ => service_refused(format!("read service {name}'s log")),
                    })?,
                };
                let mut secrets = service_secret_values(&self.cfg.state_dir.join("secrets"));
                let envkeys = row["config"][crate::service::ENVKEYS_KEY]
                    .as_str()
                    .unwrap_or("");
                for key in split_list(Some(envkeys)) {
                    if let Some(value) = row["config"][format!("environment.{key}")].as_str()
                        && value.len() <= 4096
                    {
                        secrets.push(value.to_string());
                    }
                }
                let mut lines_out = Vec::new();
                for line in text.split(['\n', '\r']) {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let clean = crate::service::clean_log(line);
                    if !clean.is_empty() {
                        lines_out.push(json!(crate::service::redact_log(&clean, &secrets)));
                    }
                }
                let start = lines_out.len().saturating_sub(lines as usize);
                Ok(json!({ "lines": &lines_out[start..] }))
            }
            "service_remove" => {
                let name = service_name(&args)?;
                let approval = args["approval"]
                    .as_str()
                    .ok_or_else(|| CallError::Invalid("approval: expected a string".into()))?;
                let record = service_entries(&entries)
                    .into_iter()
                    .find(|(_, config)| config.name == name);
                let Some((entry, config)) = record else {
                    return Err(service_not_found(format!("service {name}: not found")));
                };
                if config.adopted {
                    return Err(service_wiring::adopted(&name));
                }
                let id = entry.id.clone();
                let spent = lock(&self.kernel)?
                    .spend_service_removal(&id, &name, approval, now)
                    .map_err(|error| match error {
                        crate::kernel::ServiceRemovalError::Approval(refusal) => {
                            CallError::Refused {
                                code: "owner_approval".into(),
                                message: refusal.to_string(),
                            }
                        }
                        _ => internal(),
                    })?;
                self.logout_before_remove(&entry, &config, caller);
                let _write = self.writes.lock().map_err(|_| internal())?;
                let removal = lock(&self.kernel)?
                    .remove_service_spent(spent, now)
                    .map_err(|error| match error {
                        crate::kernel::ServiceRemovalError::Approval(refusal) => {
                            CallError::Refused {
                                code: "owner_approval".into(),
                                message: refusal.to_string(),
                            }
                        }
                        crate::kernel::ServiceRemovalError::Proposal(_) => {
                            service_removal_refused(&read, &config)
                        }
                    })?;
                let changed = removal
                    .changed
                    .iter()
                    .map(|entry| entry.0.as_str())
                    .collect::<Vec<_>>();
                if service_wiring::failed_for(&removal.outcomes, &changed)
                    || removal
                        .run
                        .as_ref()
                        .map_or(true, |run| run.as_ref().is_some_and(|run| run.partial))
                {
                    return Err(service_removal_refused(&read, &config));
                }
                let secrets = self.cfg.state_dir.join("secrets").join(&id.0);
                let _ = std::fs::remove_dir_all(secrets);
                Ok(json!({ "removed": true }))
            }
            "service_create" => {
                let name = service_name(&args)?;
                let template_name = args["template"]
                    .as_str()
                    .filter(|name| crate::service::template_name_ok(name))
                    .ok_or_else(|| {
                        CallError::Invalid("template: expected a template name".into())
                    })?;
                let id = format!("service-{name}");
                if entries
                    .iter()
                    .any(|entry| entry.id.0 == id && entry.plugin.0 == "arca.service")
                {
                    return Err(service_refused(format!("service {name} exists")));
                }
                if rows
                    .iter()
                    .any(|row| row["name"].as_str() == Some(name.as_str()))
                {
                    return Err(service_refused(format!(
                        "an instance named {name} already exists that is not a service of Arca's; it is left alone"
                    )));
                }
                let templates_dir = self.cfg.state_dir.join("templates");
                let template = crate::service::load_template(&templates_dir, template_name)
                    .map_err(service_template_error)?;
                let candidates =
                    crate::service::candidates(&cidr).map_err(service_template_error)?;
                let leases = service_leases(&read)?;
                let used = service_used(&rows, &leases, &entries);
                let ipv4 = candidates
                    .iter()
                    .find(|ip| !used.contains(ip))
                    .ok_or_else(|| {
                        service_refused(format!(
                            "the service subnet {cidr} has no free address (.10-.239)"
                        ))
                    })?
                    .to_string();
                let pool = service_pool(&self.cfg).map_err(|_| {
                    service_refused("the default profile does not name a storage pool")
                })?;
                let mut changes = ensure_netproject(&entries, &self.cfg).map_err(|_| {
                    service_refused("no service project: kernel.serviceCidr is required")
                })?;
                let secrets_dir = self.cfg.state_dir.join("secrets");
                let service_secrets = secrets_dir.join(format!("service-{name}"));
                let seeded = service_secret_names(&service_secrets).map_err(|_| internal())?;
                let config = service_config(ServiceParts {
                    name: &name,
                    template: &template,
                    ipv4,
                    gateway,
                    pool,
                    now,
                    secrets_dir: &secrets_dir,
                    templates_dir: &templates_dir,
                })
                .inspect_err(|_| cleanup_service_secrets(&service_secrets, &seeded))?;
                changes.push(crate::desired_store::EntryChange::Put(service_entry(
                    &id,
                    "arca.service",
                    serde_json::to_value(&config).map_err(|_| internal())?,
                )));
                let outcomes = {
                    let mut kernel = lock(&self.kernel)?;
                    kernel.propose(changes, now).map_err(|_| internal())?
                };
                let failed = service_wiring::failed_for(&outcomes, &[id.as_str()]);
                if failed {
                    match read.get(&format!(
                        "/1.0/instances/{name}?project={}",
                        arca_netproject::PROJECT
                    )) {
                        Ok(_) => {
                            return Err(service_refused(format!(
                                "start service {name} (created and stopped; service_remove removes it)"
                            )));
                        }
                        Err(arca_incus::IncusError::Status { code: 404, .. }) => {}
                        Err(_) => return Err(internal()),
                    }
                    let _ = {
                        let mut kernel = lock(&self.kernel)?;
                        kernel.propose(
                            vec![crate::desired_store::EntryChange::Remove(EntryId(
                                id.clone(),
                            ))],
                            now,
                        )
                    };
                    cleanup_service_secrets(&service_secrets, &seeded);
                    return Err(service_refused(format!(
                        "create service {name} from template {template_name}"
                    )));
                }
                let live = match read.get(&format!(
                    "/1.0/instances/{name}/state?project={}",
                    arca_netproject::PROJECT
                )) {
                    Ok(state) => Some(state),
                    Err(arca_incus::IncusError::Status { code: 404, .. }) => None,
                    Err(_) => return Err(internal()),
                };
                let live = live.map(|state| {
                    let mut row = state;
                    if row.get("status").is_none() {
                        row["status"] = json!("Unknown");
                    }
                    row
                });
                Ok(crate::service::view(&config, live.as_ref()))
            }
            _ => Err(service_not_found("unknown service verb")),
        }
    }
}
