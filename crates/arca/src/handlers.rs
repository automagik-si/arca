//! The verbs the binary answers, and the host reads they share with the one-shot commands. The MCP handlers and the `watch`
//! and `metrics` commands call the same functions, so a command's JSON output equals the MCP result of its verb. The three
//! plan verbs run over the kernel, through one lock and one clock.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arca_api::Scope;
use arca_approval::{ApprovalRefusal, parse_statement};
use arca_incus::{IncusRead, UnixIncus};
use arca_kernel::canon::{Digest, params_digest};
use arca_kernel::journal::DeletePlan;
use arca_kernel::ownership::ObjectRef;
use arca_kernel::plugin::EffectRefusal;
use arca_mcp::{CallError, Caller, ToolHandler};
use arca_metrics::{DEFAULT_MINUTES, HostSample, Sampler, SamplerConfig, mean};
use arca_providers::ReadOnlyIncusProvider;
use arca_reconcile::PLAN_TTL_MS;
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

/// The watch report of the host now. Each attempt reads the project index (the scope), then runs the watch over that scope,
/// then reads the index again. `complete` is set for the run, and the report is returned only when every project the second
/// read lists is in the scope, so a report is never complete while a listed project's rows were dropped. Otherwise the
/// attempt is run again on the new scope.
///
/// A project created and removed again inside one attempt is seen by neither index read. Its rows are dropped, and the attempt
/// is still accepted. Closing that gap needs the provider to report the rows it drops.
pub fn watch_snapshot<R: IncusRead>(
    incus: R,
    phones: &BTreeSet<String>,
    now: u64,
) -> Result<Value, CallError> {
    let mut provider = ReadOnlyIncusProvider::new(incus);
    for _ in 0..WATCH_ATTEMPTS {
        let scope = listed_projects(&provider)?;
        let cfg = WatchConfig {
            identity_phones: phones.clone(),
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

/// Takes one host sample and publishes the ring: every sample of the last hour, oldest first.
pub fn sample_and_publish(sampler: &mut Sampler<UnixIncus>, published: &Published) {
    sampler.sample(now_ms());
    published.publish(sampler.history(HISTORY_MINUTES));
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

/// `watch_report`: the watch of every project, with the identity flags of the configured phones.
pub struct WatchHandler {
    pub socket: PathBuf,
    pub phones: BTreeSet<String>,
}

impl ToolHandler for WatchHandler {
    fn call(&self, _args: Value, _caller: &Caller) -> Result<Value, CallError> {
        watch_snapshot(UnixIncus::new(self.socket.clone()), &self.phones, now_ms())
    }
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
        let report = watch_snapshot(incus, &BTreeSet::new(), 7).expect("the watch answers");
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
        let report = watch_snapshot(incus, &BTreeSet::new(), 7).expect("the watch answers");
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
