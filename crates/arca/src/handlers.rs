//! The verbs the binary answers, and the host reads they share with the one-shot commands. The MCP handlers and the `watch`
//! and `metrics` commands call the same functions, so a command's JSON output equals the MCP result of its verb. The three
//! plan verbs run over the kernel, through one lock and one clock.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arca_alerts::checks::{self, CheckTargets};
use arca_alerts::eval::{Timed, WatchReading};
use arca_alerts::{AlertSnapshot, Engine, TickStore, export, health, inputs};
use arca_api::Scope;
use arca_api::refusals::{
    Limit, PROPOSE_AGAIN, RATE_LIMITED, Room, TOO_MANY_PROPOSALS, Whose, retry_after, seconds_until,
};
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

use crate::action_proposal::{
    ActionKind, ActionProposal, ProposalRateLimiter, ProposeError, action_args_digest,
};
use crate::config::NodeConfig;
use crate::gpu::{GpuBackend, GpuDeviceState, GpuInstance, GpuRefusal, card_for};
use crate::kernel::{
    ActionApprovalError, AppliedSeq, ApprovedAction, JOURNAL_FILE, Kernel, PeerSync,
};

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
    /// The `approval` config, when the node has one: its linked hosts and the other kernels' passkeys files it trusts.
    pub approval: Option<ApprovalStatus>,
    /// The loopback listener, when the config names one: whether it holds its address, and why not while it does not.
    pub loopback: Option<LoopbackDoor>,
    /// The approval page, when the node serves one: its address, whether it is open, and how many passkeys approve.
    pub page: Option<PageStatus>,
}

/// What `arca_status` says of the node's approval page. The address is the config's; whether the page is open is the
/// loopback door's word for a page of this machine alone, and a page with another origin is open whenever it is
/// served; the passkeys are the kernel's own count of the owner passkeys it trusts.
#[derive(Clone)]
pub struct PageStatus {
    /// The page's address: the approval origin and `/approve`.
    pub url: String,
    /// The door of a page of this machine alone, which is closed while the node does not hold both its addresses.
    pub door: Option<LoopbackDoor>,
    pub kernel: Arc<Mutex<Kernel>>,
}

impl PageStatus {
    /// `url`, `open`, `reason` while it is closed, and `passkeys`: the number trusted, or null while the kernel is
    /// busy with a change and cannot be asked without waiting.
    fn json(&self) -> Value {
        let closed = self.door.as_ref().and_then(LoopbackDoor::page_refusal);
        let passkeys = self
            .kernel
            .try_lock()
            .ok()
            .map(|kernel| kernel.owner_passkey_count());
        json!({ "url": self.url, "open": closed.is_none(), "reason": closed, "passkeys": passkeys })
    }
}

/// The loopback listener's state: its address, and why it is closed while it is. `serve` shares one value between the
/// listener, which reports each bind it tries, and `arca_status`, which says what the last try found. A closed door
/// never stops the node: its other listeners serve, and the address is tried again.
///
/// The door of a node whose approval page is `http://localhost:<port>` has a twin: the same port on the other
/// loopback address. `localhost` names both `127.0.0.1` and `::1`, and a browser tries `::1` first, so a program that
/// held the address this node does not would be the page, at the page's exact origin. Such a node binds both, and its
/// page is open only while it holds both (see [`LoopbackDoor::page_refusal`]).
#[derive(Clone)]
pub struct LoopbackDoor {
    addr: std::net::SocketAddr,
    state: Arc<Mutex<DoorState>>,
    /// Whether this is the twin of a page's door: an address this machine does not have is then nobody's to hold.
    is_twin: bool,
    twin: Option<Arc<LoopbackDoor>>,
    /// The kernel's list of the machine's IPv6 addresses.
    inet6_table: PathBuf,
}

/// The kernel's list of the IPv6 addresses assigned to the machine's interfaces, one a line, the address first as 32
/// hexadecimal digits.
const INET6_TABLE: &str = "/proc/net/if_inet6";
/// `::1` as that list writes it.
const INET6_LOOPBACK: &str = "00000000000000000000000000000001";

/// Whether the machine has an IPv6 loopback address, read from the kernel's list at `table`.
#[derive(Debug, PartialEq, Eq)]
enum Inet6Loopback {
    /// The list is not there (a kernel with no IPv6), or lists no `::1`.
    No,
    /// The list has `::1`.
    Yes,
    /// The list is there and cannot be read.
    Unknown,
}

fn inet6_loopback(table: &Path) -> Inet6Loopback {
    match std::fs::read_to_string(table) {
        Ok(text) => {
            let listed = text
                .lines()
                .any(|line| line.split_whitespace().next() == Some(INET6_LOOPBACK));
            if listed {
                Inet6Loopback::Yes
            } else {
                Inet6Loopback::No
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Inet6Loopback::No,
        Err(_) => Inet6Loopback::Unknown,
    }
}

/// What the last try of a bind found.
#[derive(Clone, PartialEq, Eq)]
enum DoorState {
    /// This node holds the address.
    Held,
    /// The machine has no such address (no IPv6 loopback), so no program can hold it. Only a twin is ever this.
    Absent,
    /// This node does not hold the address, and why.
    Closed(String),
}

/// `EAFNOSUPPORT` on Linux: the machine's kernel has no such address family at all.
const ADDRESS_FAMILY_NOT_SUPPORTED: i32 = 97;

impl LoopbackDoor {
    /// The door of `addr`, closed until its first bind succeeds.
    pub fn new(addr: std::net::SocketAddr) -> Self {
        Self {
            addr,
            state: Arc::new(Mutex::new(DoorState::Closed("not opened yet".to_string()))),
            is_twin: false,
            twin: None,
            inet6_table: PathBuf::from(INET6_TABLE),
        }
    }

    /// Reads the machine's IPv6 addresses from `table` instead of the kernel's own list.
    ///
    /// Test seam: a test names a file of its own. Call it before [`LoopbackDoor::with_twin`], which hands it on.
    #[doc(hidden)]
    pub fn with_inet6_table(mut self, table: PathBuf) -> Self {
        self.inet6_table = table;
        self
    }

    /// This door with its twin: the same port on the other loopback address, for a node whose approval page is
    /// `http://localhost:<port>`.
    pub fn with_twin(mut self) -> Self {
        let other: std::net::IpAddr = if self.addr.is_ipv4() {
            std::net::Ipv6Addr::LOCALHOST.into()
        } else {
            std::net::Ipv4Addr::LOCALHOST.into()
        };
        let mut twin = Self::new(std::net::SocketAddr::new(other, self.addr.port()));
        twin.is_twin = true;
        twin.inet6_table = self.inet6_table.clone();
        self.twin = Some(Arc::new(twin));
        self
    }

    /// The twin's own door, for the listener that binds it.
    pub fn twin(&self) -> Option<LoopbackDoor> {
        self.twin.as_deref().cloned()
    }

    /// The address this door is of.
    pub fn addr(&self) -> std::net::SocketAddr {
        self.addr
    }

    /// Records one try of the bind: `Some(error)` for a try that failed, `None` for the one that succeeded. A change
    /// is said once on stderr, so a port that stays held does not fill the log.
    ///
    /// For a twin, an address the machine does not have at all (`EADDRNOTAVAIL`, as on a machine whose IPv6 is turned
    /// off, or `EAFNOSUPPORT`, as on a kernel built without it) is not a door another program holds: no program can
    /// bind it and no browser can reach it. It counts as held, and the bind is still tried again, so an address that
    /// appears later is taken. Every other failure, a port in use first of all, is a door this node does not hold.
    ///
    /// Those two errors can be true for this process alone: a unit that restricts address families, or a network
    /// namespace of the node's own, fails the bind while the machine has `::1` for any other program to take. So the
    /// error is believed only when the kernel's own list of addresses has no `::1` (or is not there at all). When the
    /// list has it, or cannot be read, the address is one this node does not hold, and the page stays closed.
    pub fn report(&self, error: Option<&std::io::Error>) {
        let no_such_address = |error: &std::io::Error| {
            error.kind() == std::io::ErrorKind::AddrNotAvailable
                || error.raw_os_error() == Some(ADDRESS_FAMILY_NOT_SUPPORTED)
        };
        let found = match error {
            None => DoorState::Held,
            Some(error) if self.is_twin && no_such_address(error) => {
                match inet6_loopback(&self.inet6_table) {
                    Inet6Loopback::No => DoorState::Absent,
                    Inet6Loopback::Yes => DoorState::Closed(format!(
                        "this node cannot bind it ({error}), and the machine has the address: something of the node's own, such as restricted address families or a network namespace, keeps it from the node, and another program could hold it"
                    )),
                    Inet6Loopback::Unknown => DoorState::Closed(format!(
                        "this node cannot bind it ({error}), and {} cannot be read, so it cannot tell whether the machine has the address",
                        self.inet6_table.display()
                    )),
                }
            }
            Some(error) => DoorState::Closed(match error.kind() {
                std::io::ErrorKind::AddrInUse => {
                    "address in use: another program holds this port".to_string()
                }
                std::io::ErrorKind::AddrNotAvailable => {
                    "the address is not assigned to this machine".to_string()
                }
                std::io::ErrorKind::PermissionDenied => {
                    "this user may not take this port".to_string()
                }
                other => other.to_string(),
            }),
        };
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if *state != found {
            let addr = self.addr;
            match (&found, self.is_twin) {
                (DoorState::Closed(why), false) => eprintln!(
                    "arca: the loopback door {addr} is closed: {why}. The node runs without it and tries the address again"
                ),
                (DoorState::Closed(why), true) => eprintln!(
                    "arca: the approval page's other address {addr} is not this node's: {why}. The page is closed until it is, and the address is tried again"
                ),
                (DoorState::Absent, _) => eprintln!(
                    "arca: the approval page's other address {addr} is not on this machine, so no program can hold it"
                ),
                (DoorState::Held, false) => eprintln!("arca: the loopback door {addr} is open"),
                (DoorState::Held, true) => {
                    eprintln!("arca: the approval page's other address {addr} is this node's")
                }
            }
        }
        *state = found;
    }

    /// Why this node does not hold the address, or `None` while it does, or while nobody can.
    fn not_held(&self) -> Option<String> {
        match &*self.state.lock().unwrap_or_else(PoisonError::into_inner) {
            DoorState::Closed(why) => Some(why.clone()),
            DoorState::Held | DoorState::Absent => None,
        }
    }

    /// Why the page of this machine alone is closed, in words for the owner, or `None` while it is open: this node
    /// holds its door and the twin (or the machine has no twin address). `None` too for a door with no twin, whose
    /// page is not on it. While this is `Some`, some other program may be answering at the page's origin: the page
    /// serves nothing, takes no approval and gives no setup code, so no code exists to be typed there. A signature
    /// the owner gives there is not undone by this: it stays a valid approval of the card that program chose until
    /// its statement expires.
    pub fn page_refusal(&self) -> Option<String> {
        let twin = self.twin.as_deref()?;
        [self, twin].into_iter().find_map(|door| {
            door.not_held().map(|why| {
                format!(
                    "The approval page is closed: this node does not hold {} ({why}). Whatever answers at http://localhost:{} now may be another program, so type no setup code and approve nothing there. `arca_status` says when the page is open",
                    door.addr,
                    self.addr.port()
                )
            })
        })
    }

    /// What `arca_status` says of the door: its address, whether it is open, and the reason while it is not. With a
    /// twin, also `page`: whether the page is open, which takes both addresses, and the reason while it is not.
    pub fn json(&self) -> Value {
        let closed = self.not_held();
        let mut status =
            json!({ "addr": self.addr.to_string(), "open": closed.is_none(), "reason": closed });
        if self.twin.is_some() {
            let refusal = self.page_refusal();
            status["page"] = json!({ "open": refusal.is_none(), "reason": refusal });
        }
        status
    }
}

/// How old the trusted files' last read may be for `arca_status` to report it rather than read the files again.
pub const STATUS_TRUST_WINDOW: std::time::Duration = std::time::Duration::from_secs(10);

/// What `arca_status` reports of a node's multi-host approvals. `foreign` is the verifier's own value (a clone shares its
/// last read), so a status call reports what the last approval check saw, and reads the files itself only when that read is
/// older than [`STATUS_TRUST_WINDOW`]. A file is named by its index and file name, never its path.
#[derive(Clone, Default)]
pub struct ApprovalStatus {
    pub linked_hosts: Vec<String>,
    pub foreign: arca_approval::ForeignPasskeys,
    /// The passkeys the first `approval.peerKeys` source's page host keeps in sync here: the verifier's own value.
    pub peer: Option<arca_approval::PeerPasskeys>,
    /// Every source, the first included, in the config's order, when `approval.peerKeys` is written as a list: the
    /// verifier's own values. `None` for the single-object form, whose status is `peerKeys` alone.
    pub peer_sources: Option<Vec<arca_approval::PeerPasskeys>>,
}

impl ApprovalStatus {
    fn json(&self) -> Value {
        // A file is named by its place in `approval.trustOwnerKeysFrom` and its file name, never its full path.
        let files: Vec<Value> = self
            .foreign
            .recent_states(STATUS_TRUST_WINDOW)
            .iter()
            .enumerate()
            .map(|(index, state)| {
                json!({
                    "index": index,
                    "name": state
                        .path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    "keys": state.keys,
                    "reason": state.reason,
                })
            })
            .collect();
        let mut status = json!({ "linkedHosts": self.linked_hosts, "trustedKeyFiles": files });
        if let Some(peer) = &self.peer {
            status["peerKeys"] = peer_status(peer);
        }
        if let Some(sources) = &self.peer_sources {
            status["peerKeySources"] = sources.iter().map(peer_status).collect();
        }
        status
    }
}

/// `arca_status`'s `peerKeys`: the page host, how many of its passkeys are trusted, since when, and a plain sentence. No key,
/// credential, line or path.
fn peer_status(peer: &arca_approval::PeerPasskeys) -> Value {
    let state = peer.recent_state(STATUS_TRUST_WINDOW);
    let from = peer.from();
    let since = state.since.map(iso_utc);
    let reason = match (&state.reason, &since) {
        (Some(why), _) => why.clone(),
        (None, Some(since)) => {
            let mut text = format!("trusting {from}'s passkeys since {since} (first sync)");
            if let Some(changed) = state.changed {
                text.push_str(&format!(
                    "; the list last changed with a passkey approval at {}",
                    iso_utc(changed)
                ));
            }
            text
        }
        (None, None) => format!("no passkey has been synced from {from} yet"),
    };
    json!({ "from": from, "keys": state.keys, "since": since, "reason": reason })
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
        let mut status = json!({
            "version": env!("CARGO_PKG_VERSION"),
            "node": self.node,
            "mode": self.mode,
            "uptimeS": self.started.elapsed().as_secs(),
            "incusReachable": incus_reachable,
            "appliedSeq": applied_seq,
            "metricsSource": metrics_source.mode.as_str(),
            "metricsSourceReason": metrics_source.reason,
        });
        if let Some(approval) = &self.approval {
            status["approval"] = approval.json();
        }
        if let Some(door) = &self.loopback {
            status["loopback"] = door.json();
        }
        if let Some(page) = &self.page {
            status["approvalPage"] = page.json();
        }
        Ok(status)
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

/// The plan, proposal, GPU and storage action handlers, keyed by verb name, over one kernel and one clock.
pub fn plan_handlers(
    kernel: Arc<Mutex<Kernel>>,
    clock: Clock,
    postgres_image: Option<String>,
    gpu: Option<Arc<dyn GpuBackend>>,
) -> Vec<(&'static str, Arc<dyn ToolHandler>)> {
    plan_handlers_with(kernel, clock, postgres_image, gpu, ActionSlot::default())
}

/// The plan handlers, with `actions` as the executor `action_approve` runs an approved action through. `cmd_serve` fills
/// the slot once the whole handler set exists. An empty slot refuses before the approval is read, so nothing is spent.
pub fn plan_handlers_with(
    kernel: Arc<Mutex<Kernel>>,
    clock: Clock,
    postgres_image: Option<String>,
    gpu: Option<Arc<dyn GpuBackend>>,
    actions: ActionSlot,
) -> Vec<(&'static str, Arc<dyn ToolHandler>)> {
    plan_handlers_full(kernel, clock, postgres_image, gpu, None, actions)
}

/// The plan handlers with the snapshot storage backend too. `storage` is `None` only where no Incus is wired; a watch-only
/// node passes a [`Storage`](crate::storage::Storage) with no writer, which refuses every storage action.
pub fn plan_handlers_full(
    kernel: Arc<Mutex<Kernel>>,
    clock: Clock,
    postgres_image: Option<String>,
    gpu: Option<Arc<dyn GpuBackend>>,
    storage: Option<Arc<crate::storage::Storage>>,
    actions: ActionSlot,
) -> Vec<(&'static str, Arc<dyn ToolHandler>)> {
    plan_handlers_node(kernel, clock, postgres_image, gpu, storage, None, actions)
}

/// The plan handlers with the instance state writer too, and the three instance verbs (`instance_start`,
/// `instance_stop`, `instance_restart`) beside the GPU and storage ones. `lifecycle` is `None` on a watch-only node,
/// where the three verbs and their proposals are refused.
pub fn plan_handlers_node(
    kernel: Arc<Mutex<Kernel>>,
    clock: Clock,
    postgres_image: Option<String>,
    gpu: Option<Arc<dyn GpuBackend>>,
    storage: Option<Arc<crate::storage::Storage>>,
    lifecycle: Option<Arc<crate::lifecycle::InstanceLifecycle>>,
    actions: ActionSlot,
) -> Vec<(&'static str, Arc<dyn ToolHandler>)> {
    plan_handlers_served(
        kernel,
        clock,
        postgres_image,
        gpu,
        storage,
        lifecycle,
        actions,
        None,
    )
}

/// The plan handlers of a served node: those of [`plan_handlers_node`], with the node's config. With `node`, a proposed
/// `service_create` is checked against the node's mode and bridges when it is proposed, so the owner is never asked to
/// approve a create this node is sure to refuse.
#[allow(clippy::too_many_arguments)]
pub fn plan_handlers_served(
    kernel: Arc<Mutex<Kernel>>,
    clock: Clock,
    postgres_image: Option<String>,
    gpu: Option<Arc<dyn GpuBackend>>,
    storage: Option<Arc<crate::storage::Storage>>,
    lifecycle: Option<Arc<crate::lifecycle::InstanceLifecycle>>,
    actions: ActionSlot,
    node: Option<Arc<NodeConfig>>,
) -> Vec<(&'static str, Arc<dyn ToolHandler>)> {
    let rate = Arc::new(ProposalRateLimiter::default());
    let instance_handlers =
        crate::lifecycle::handlers(Arc::clone(&kernel), Arc::clone(&clock), lifecycle.clone());
    let storage_handler = |action: ActionKind| -> Arc<dyn ToolHandler> {
        Arc::new(crate::storage::StorageActionHandler {
            action,
            kernel: Arc::clone(&kernel),
            storage: storage.clone(),
            clock: Arc::clone(&clock),
        })
    };
    let storage_add = storage_handler(ActionKind::StorageAdd);
    let snapshot_create = storage_handler(ActionKind::SnapshotCreate);
    let snapshot_delete = storage_handler(ActionKind::SnapshotDelete);
    vec![
        (
            "plan_list",
            Arc::new(PlanList {
                kernel: Arc::clone(&kernel),
                clock: Arc::clone(&clock),
            }) as Arc<dyn ToolHandler>,
        ),
        (
            "action_propose",
            Arc::new(ActionPropose {
                kernel: Arc::clone(&kernel),
                clock: Arc::clone(&clock),
                postgres_image,
                rate,
                gpu: gpu.clone(),
                storage: storage.clone(),
                node,
                lifecycle,
            }) as Arc<dyn ToolHandler>,
        ),
        ("storage_add", storage_add),
        ("snapshot_create", snapshot_create),
        ("snapshot_delete", snapshot_delete),
        (
            "gpu_attach",
            Arc::new(GpuActionHandler {
                action: ActionKind::GpuAttach,
                kernel: Arc::clone(&kernel),
                gpu: gpu.clone(),
            }) as Arc<dyn ToolHandler>,
        ),
        (
            "gpu_detach",
            Arc::new(GpuActionHandler {
                action: ActionKind::GpuDetach,
                kernel: Arc::clone(&kernel),
                gpu,
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
            Arc::new(ApprovalSubmit {
                kernel: Arc::clone(&kernel),
                clock: Arc::clone(&clock),
            }) as Arc<dyn ToolHandler>,
        ),
        (
            "action_approve",
            Arc::new(ActionApprove {
                kernel: Arc::clone(&kernel),
                clock: Arc::clone(&clock),
                actions,
            }) as Arc<dyn ToolHandler>,
        ),
        (
            "peer_keys_sync",
            Arc::new(PeerKeysSync { kernel, clock }) as Arc<dyn ToolHandler>,
        ),
    ]
    .into_iter()
    .chain(instance_handlers)
    .collect()
}

/// `peer_keys_sync`: keeps the page host's owner passkeys in this kernel's peer passkeys file (see
/// [`Kernel::peer_keys_sync_from`]). `from` names the source, and absent it is the first one. `statement` and `approval`
/// (the owner's passkey proof over it, as its JSON) come together or not at all. No line is echoed or logged.
struct PeerKeysSync {
    kernel: Arc<Mutex<Kernel>>,
    clock: Clock,
}

impl ToolHandler for PeerKeysSync {
    fn call(&self, args: Value, _caller: &Caller) -> Result<Value, CallError> {
        let lines: Vec<String> = args
            .get("lines")
            .and_then(Value::as_array)
            .ok_or_else(|| CallError::Invalid("lines is required".to_string()))?
            .iter()
            .map(|line| line.as_str().map(str::to_string))
            .collect::<Option<_>>()
            .ok_or_else(|| CallError::Invalid("each line is a string".to_string()))?;
        let statement = args.get("statement").and_then(Value::as_str);
        let approval = args.get("approval").and_then(Value::as_str);
        let signed = match (statement, approval) {
            (Some(statement), Some(proof)) => Some((statement, proof.as_bytes())),
            (None, None) => None,
            _ => {
                return Err(CallError::Invalid(
                    "statement and approval come together".to_string(),
                ));
            }
        };
        let from = match args.get("from") {
            None => None,
            Some(value) => Some(
                value
                    .as_str()
                    .ok_or_else(|| CallError::Invalid("from is a page host's name".to_string()))?,
            ),
        };
        let now = (self.clock)();
        let outcome = lock(&self.kernel)?
            .peer_keys_sync_from(from, &lines, signed, now)
            .map_err(refusal_failure)?;
        Ok(match outcome {
            PeerSync::Trusted { keys } => json!({ "status": "trusted", "keys": keys }),
            PeerSync::Unchanged { keys } => json!({ "status": "unchanged", "keys": keys }),
            PeerSync::Changed { keys } => json!({ "status": "changed", "keys": keys }),
            PeerSync::NeedsApproval { keys, statement } => {
                json!({ "status": "needs_approval", "keys": keys, "statement": statement })
            }
        })
    }
}

/// `plan_list`: each waiting delete plan and action proposal that has not lapsed. A delete plan carries the statement the
/// owner signs; an action proposal's statement is added only by the open approval page.
struct PlanList {
    kernel: Arc<Mutex<Kernel>>,
    clock: Clock,
}

impl ToolHandler for PlanList {
    fn call(&self, _args: Value, _caller: &Caller) -> Result<Value, CallError> {
        let now = (self.clock)();
        let kernel = lock(&self.kernel)?;
        list_plans(&kernel, now, false)
    }
}

/// The waiting plan list, shared by `plan_list` and the open approval page. Every statement is built fresh with a new
/// nonce. `action_statements` is true only for the page, whose cards sign an action proposal too.
pub(crate) fn list_plans(
    kernel: &Kernel,
    now: u64,
    action_statements: bool,
) -> Result<Value, CallError> {
    let mut plans = kernel
        .pending_plans()
        .iter()
        .filter(|plan| now.saturating_sub(plan.created_ms) <= PLAN_TTL_MS)
        .map(|plan| plan_json(kernel.host(), plan, now))
        .collect::<Result<Vec<Value>, CallError>>()?;
    for proposal in kernel
        .action_proposals()
        .iter()
        .filter(|proposal| !proposal.expired(now))
    {
        plans.push(action_json(
            kernel.host(),
            proposal,
            now,
            action_statements,
        )?);
    }
    Ok(json!({ "plans": plans }))
}

/// One action proposal in a listing. The page gets the exact statement the owner signs; `plan_list` does not.
fn action_json(
    host: &str,
    proposal: &ActionProposal,
    now: u64,
    statement: bool,
) -> Result<Value, CallError> {
    let mut value = json!({
        "kind": "action",
        "id": proposal.id,
        "action": proposal.action,
        "summary": proposal.summary,
        "details": proposal.details,
        "args": proposal.args,
        "argsDigest": proposal.args_digest,
        "createdAt": proposal.created_ms,
        "expiresAt": proposal.expires_at,
        "host": host,
    });
    if statement {
        value["statement"] = json!(action_statement(
            host,
            &proposal.id,
            &proposal.action,
            &proposal.args_digest,
            now,
            &fresh_nonce()?,
        )?);
    }
    Ok(value)
}

/// `action_propose`: validates one allowed action's arguments, resolves a GPU action's project, type and identity, checks
/// that an instance start, stop or restart names an instance of Arca's, and records an inert proposal. A disallowed action is an invalid call and leaves no record.
struct ActionPropose {
    kernel: Arc<Mutex<Kernel>>,
    clock: Clock,
    postgres_image: Option<String>,
    rate: Arc<ProposalRateLimiter>,
    gpu: Option<Arc<dyn GpuBackend>>,
    storage: Option<Arc<crate::storage::Storage>>,
    node: Option<Arc<NodeConfig>>,
    lifecycle: Option<Arc<crate::lifecycle::InstanceLifecycle>>,
}

impl ToolHandler for ActionPropose {
    fn call(&self, args: Value, caller: &Caller) -> Result<Value, CallError> {
        let name = args["action"]
            .as_str()
            .ok_or_else(|| CallError::Invalid("action is required".to_string()))?;
        let action = ActionKind::from_name(name).ok_or_else(|| {
            CallError::Invalid("action: not one of the actions this build allows".to_string())
        })?;
        let payload = args
            .get("args")
            .cloned()
            .ok_or_else(|| CallError::Invalid("args is required".to_string()))?;
        let is_gpu = matches!(action, ActionKind::GpuAttach | ActionKind::GpuDetach);
        let now = (self.clock)();
        let storage = if action.is_storage() {
            Some(self.storage.as_deref().ok_or_else(|| CallError::Refused {
                code: "StorageDisabled".to_string(),
                message: "snapshot storage is not available on this node".to_string(),
            })?)
        } else {
            None
        };
        // A database action that names a tenant is refused when it is proposed, in the words the verb itself uses: it
        // is never recorded, and the owner is never shown a card for it.
        if action == ActionKind::PostgresCreate && payload.get("tenant").is_some() {
            let Some(node) = &self.node else {
                return Err(tenants::no_tenants());
            };
            tenants::tenant_arg(node, &payload, lock(&self.kernel)?.entries())?;
            return Err(tenants::database_route(action.as_str()));
        }
        // Without a tenant a snapshot action reaches nothing of any tenant: a tenant's project is named by tenant, and
        // a snapshot taken from a tenant is that tenant's to delete.
        if matches!(
            action,
            ActionKind::SnapshotCreate | ActionKind::SnapshotDelete
        ) && payload.get("tenant").is_none()
        {
            let tenant_project =
                |project: &str| arca_kernel::ownership::tenant_of_project(project).is_some();
            if action == ActionKind::SnapshotCreate
                && payload["project"].as_str().is_some_and(tenant_project)
            {
                return Err(CallError::Invalid(
                    "args/project: a tenant's project is named by tenant, not by project"
                        .to_string(),
                ));
            }
            if action == ActionKind::SnapshotDelete
                && let Some(storage) = storage
            {
                let name = payload["name"].as_str().unwrap_or_default();
                if storage.snapshots()?.iter().any(|snapshot| {
                    snapshot.name == name && tenant_project(&snapshot.source_project)
                }) {
                    return Err(service_not_found(format!("snapshot {name}: not found")));
                }
            }
        }
        // A snapshot action that names a tenant acts on that tenant's project: a create is given the project, and a
        // delete must name a snapshot taken from it.
        let payload = match (action, payload.get("tenant"), &self.node) {
            (ActionKind::SnapshotCreate | ActionKind::SnapshotDelete, Some(_), Some(node)) => {
                let entries = lock(&self.kernel)?.entries().to_vec();
                let scope = tenants::tenant_arg(node, &payload, &entries)?
                    .ok_or_else(tenants::no_tenants)?;
                let mut payload = payload;
                if action == ActionKind::SnapshotCreate {
                    if payload
                        .get("project")
                        .is_some_and(|project| project.as_str() != Some(scope.project.as_str()))
                    {
                        return Err(CallError::Invalid(
                            "args/project: it is not the project of the tenant the call names"
                                .to_string(),
                        ));
                    }
                    payload["project"] = json!(scope.project);
                } else if let Some(storage) = storage {
                    let name = payload["name"].as_str().unwrap_or_default();
                    let own = storage.snapshots()?.iter().any(|snapshot| {
                        snapshot.name == name && snapshot.source_project == scope.project
                    });
                    if !own {
                        return Err(service_not_found(format!("snapshot {name}: not found")));
                    }
                }
                payload
            }
            (ActionKind::SnapshotCreate | ActionKind::SnapshotDelete, Some(_), None) => {
                return Err(tenants::no_tenants());
            }
            _ => payload,
        };
        // What a create's card says of its template beyond the arguments (why it is created stopped, or that a
        // database's password is generated), read from the very template the proposal's fingerprint is of.
        let mut card_fact: Option<String> = None;
        let payload = if is_gpu {
            let Some(gpu) = &self.gpu else {
                return Err(gpu_refusal(GpuRefusal::Disabled));
            };
            enrich_gpu(gpu.as_ref(), &payload)?
        } else if let Some(storage) = storage {
            crate::storage::enrich(storage, action, &payload, now)?
        } else if let (ActionKind::ServiceCreate, Some(node)) = (action, &self.node) {
            // The arguments are held to the verb's schema first, so a proposal with a bad name is told about the
            // name and not about what the node holds. What the node fills in below is checked again after it.
            action
                .prepare(&payload, self.postgres_image.as_deref())
                .map_err(CallError::Invalid)?;
            let entries = lock(&self.kernel)?.entries().to_vec();
            enrich_service_create(node, &entries, &payload, &mut card_fact, now)?
        } else if let (ActionKind::PostgresCreate, Some(node)) = (action, &self.node) {
            let entries = lock(&self.kernel)?.entries().to_vec();
            enrich_postgres_create(node, &entries, &payload, &mut card_fact)?
        } else if action == ActionKind::PostgresCreate
            && self.postgres_image.is_none()
            && payload.get("template").is_some()
        {
            // Only the node names the template and its image, from its own template (`enrich_postgres_create`). A
            // handler given no node cannot, so it records no proposal that names them: no card shows an image the
            // node did not read from its template.
            return Err(CallError::Invalid(
                "args/template: the kernel names the template and the image it pins when the proposal is made; a caller names neither"
                    .to_string(),
            ));
        } else if let (ActionKind::TenantCreate | ActionKind::TenantRemove, Some(node)) =
            (action, &self.node)
        {
            // A proposal the node would refuse is refused now, so the owner is never asked to approve it. Its
            // arguments are recorded exactly as proposed.
            let entries = lock(&self.kernel)?.entries().to_vec();
            if action == ActionKind::TenantCreate {
                tenants::check_create(node, &entries, &payload)?;
            } else {
                tenants::check_remove(node, &entries, &payload)?;
            }
            payload
        } else if let (ActionKind::DeployRemove, Some(node)) = (action, &self.node) {
            let entries = lock(&self.kernel)?.entries().to_vec();
            tenants::check_deploy_remove(node, &entries, &payload, now)?;
            payload
        } else if action == ActionKind::ServiceRemove && payload.get("tenant").is_none() {
            // A service a function links to is not removed, so the owner is never shown a card for it.
            let entries = lock(&self.kernel)?.entries().to_vec();
            if let Some(refusal) = function_handlers::linked_service_refusal(
                &entries,
                payload["name"].as_str().unwrap_or_default(),
                false,
            ) {
                return Err(refusal);
            }
            payload
        } else if let (ActionKind::FunctionCreate, Some(node)) = (action, &self.node) {
            // The proposal names every default and every private destination, so the card shows them and the
            // approval binds them. A create the node would refuse is refused now.
            let entries = lock(&self.kernel)?.entries().to_vec();
            function_handlers::enrich_function_create(node, &entries, &payload)?
        } else if let (ActionKind::FunctionRemove, Some(_)) = (action, &self.node) {
            let entries = lock(&self.kernel)?.entries().to_vec();
            function_handlers::check_function_remove(&entries, &payload)?;
            payload
        } else {
            payload
        };
        // A create whose image is on a registry this node holds a credential for names that registry, so the card
        // says where the credential will rest and the approval binds it.
        let payload = match (action, &self.node) {
            (ActionKind::ServiceCreate | ActionKind::FunctionCreate, Some(node)) => {
                function_handlers::with_registry(node, action, payload)?
            }
            _ => payload,
        };
        let payload = action
            .prepare(&payload, self.postgres_image.as_deref())
            .map_err(CallError::Invalid)?;
        if let Some(storage) = storage {
            crate::storage::check_proposal(storage, action, &payload, now)?;
        }
        if action.is_lifecycle() {
            // The kernel's lock is held for the lookup only: the instance read takes it again.
            let project = {
                let kernel = lock(&self.kernel)?;
                crate::lifecycle::project_of(&kernel, &payload)
            }?;
            crate::lifecycle::check_proposal(self.lifecycle.as_deref(), &project, &payload)?;
        }
        // An import that could not run is refused now, with the check that failed, so the owner is never asked to approve it.
        if let (ActionKind::ServiceImport, Some(node)) = (action, &self.node) {
            let name = payload["name"].as_str().unwrap_or_default();
            let entries = lock(&self.kernel)?.entries().to_vec();
            let plan =
                crate::service_import::plan(node, name, Some(&entries), now).map_err(|_| {
                    service_refused(format!("import service {name}: Incus could not be read"))
                })?;
            if let Some(failed) = plan.refusal() {
                return Err(service_refused(format!(
                    "import service {name}: {}; nothing was proposed",
                    failed.detail
                )));
            }
        }
        let gpu_product = match action {
            ActionKind::GpuAttach => prepare_attach(
                self.gpu.as_deref().expect("a GPU action has a backend"),
                &payload,
            )?
            .card
            .map(|card| card.product),
            ActionKind::GpuDetach => prepare_detach(
                self.gpu.as_deref().expect("a GPU action has a backend"),
                &payload,
            )?
            .card
            .map(|card| card.product),
            // The card of a create says when its template is created stopped whatever the call says, and why.
            // The card of a database made from the node's template says that the template generates its password
            // only when it does, and otherwise why the database is created stopped. Both were read when the
            // proposal was given its template's fingerprint, from that one read of the template.
            ActionKind::ServiceCreate | ActionKind::PostgresCreate => card_fact.take(),
            // The card of a deploy's removal lists the deploy's parts as they stand now.
            ActionKind::DeployRemove => {
                let entries = lock(&self.kernel)?.entries().to_vec();
                Some(tenants::deploy_parts(&entries, &payload))
            }
            _ => None,
        };
        let mut kernel = lock(&self.kernel)?;
        // A proposal whose digest already waits is a deduplicated request: it returns that record and records nothing, so it
        // never consumes the token's window and a full window does not refuse it.
        let duplicate = action_args_digest(action.as_str(), &payload).is_some_and(|digest| {
            kernel
                .action_proposals()
                .iter()
                .any(|proposal| proposal.args_digest == digest && !proposal.expired(now))
        });
        if !duplicate && !self.rate.allow(&caller.token_id, now) {
            // The code is the one this refusal has always had. The wait is the time until the token's oldest counted
            // proposal leaves the window, which the sentence rounds up to the whole window.
            let wait = self
                .rate
                .wait_ms(&caller.token_id, now)
                .map(|ms| seconds_until(ms, 0));
            return Err(CallError::RefusedWith {
                code: RATE_LIMITED.to_string(),
                message: "too many proposals from this token; try again in ten minutes".to_string(),
                fields: retry_after(wait),
            });
        }
        let proposal = match kernel.propose_action(action, payload, now, gpu_product.as_deref()) {
            Ok(proposal) => proposal,
            Err(error) => {
                // Only a newly recorded proposal counts. A refusal gives back the place reserved above.
                if !duplicate {
                    self.rate.refund(&caller.token_id, now);
                }
                return Err(propose_failure(error));
            }
        };
        Ok(json!({
            "kind": "action",
            "id": proposal.id,
            "action": proposal.action,
            "summary": proposal.summary,
            "details": proposal.details,
            "args": proposal.args,
            "argsDigest": proposal.args_digest,
            "createdAt": proposal.created_ms,
            "expiresAt": proposal.expires_at,
        }))
    }
}

/// A resolved GPU target: the instance the project-bound action names, and the card when the inventory knows it.
struct GpuTarget {
    instance: GpuInstance,
    card: Option<arca_metrics::GpuCard>,
}

/// Adds the resolved project, type and UUID to a GPU proposal's arguments before validation, so the argsDigest binds
/// them. The caller's `instance` and `pci` are kept; anything the caller sent for the resolved fields is replaced.
fn enrich_gpu(gpu: &dyn GpuBackend, args: &Value) -> Result<Value, CallError> {
    let name = args["instance"]
        .as_str()
        .ok_or_else(|| CallError::Invalid("args/instance is required".to_string()))?;
    let instance = gpu.resolve(name).map_err(gpu_refusal)?;
    let Some(mut object) = args.as_object().cloned() else {
        return Err(CallError::Invalid("args: expected an object".to_string()));
    };
    object.insert("project".to_string(), json!(instance.project));
    object.insert("kind".to_string(), json!(instance.kind));
    object.insert("uuid".to_string(), json!(instance.uuid));
    Ok(Value::Object(object))
}

/// Reads the instance the arguments bind and refuses a moved or recreated one.
fn bound_instance(gpu: &dyn GpuBackend, args: &Value) -> Result<GpuInstance, CallError> {
    let project = args["project"].as_str().unwrap_or_default();
    let name = args["instance"].as_str().unwrap_or_default();
    let instance = gpu.instance(project, name).map_err(gpu_refusal)?;
    let kind = args["kind"].as_str().unwrap_or_default();
    let uuid = args["uuid"].as_str();
    if instance.kind != kind || instance.uuid.as_deref() != uuid {
        return Err(gpu_refusal(GpuRefusal::Identity(
            "the instance moved, changed type or was recreated".to_string(),
        )));
    }
    Ok(instance)
}

/// Checks an attach proposal against the inventory and the project-bound instance. The card must be in the inventory; a
/// container may not take a vfio-pci card; a virtual machine may not take a driver-bound card another consumer uses.
fn prepare_attach(gpu: &dyn GpuBackend, args: &Value) -> Result<GpuTarget, CallError> {
    let pci = args["pci"].as_str().unwrap_or_default();
    let cards = gpu.inventory().map_err(gpu_refusal)?;
    let card = card_for(&cards, pci).map_err(gpu_refusal)?;
    let instance = bound_instance(gpu, args)?;
    if !gpu.is_kernel_object(&instance).map_err(gpu_refusal)? {
        return Err(gpu_refusal(GpuRefusal::NotKernelObject(instance.name)));
    }
    if card.usage == arca_metrics::GpuUse::VfioPci && instance.is_container() {
        return Err(gpu_refusal(GpuRefusal::Bound(vec![card.pci.clone()])));
    }
    if instance.is_vm() && card.driver.as_deref() == Some("nvidia") {
        match gpu.in_use(&card.pci) {
            Ok(Some(true)) => {
                return Err(gpu_refusal(GpuRefusal::InUse(card.pci.clone())));
            }
            Ok(Some(false)) => {}
            // Unknown is never free: a VM may not take a card whose users this node cannot list. The check runs again
            // when an approved action executes, so a card that becomes unreadable later is refused there too.
            Ok(None) | Err(_) => return Err(gpu_refusal(GpuRefusal::InUseUnknown)),
        }
    }
    Ok(GpuTarget {
        instance,
        card: Some(card),
    })
}

/// Checks a detach's project-bound instance and that it holds Arca's own device for the card. A card that has left the
/// inventory can still be detached by its address; the device is the part that must still be Arca's.
fn prepare_detach(gpu: &dyn GpuBackend, args: &Value) -> Result<GpuTarget, CallError> {
    let pci = args["pci"].as_str().unwrap_or_default();
    let instance = bound_instance(gpu, args)?;
    if !gpu.is_kernel_object(&instance).map_err(gpu_refusal)? {
        return Err(gpu_refusal(GpuRefusal::NotKernelObject(instance.name)));
    }
    let state = gpu.device_state(&instance, pci).map_err(gpu_refusal)?;
    match state {
        GpuDeviceState::Arca => {}
        GpuDeviceState::Absent => {
            return Err(gpu_refusal(GpuRefusal::DeviceAbsent(pci.to_string())));
        }
        GpuDeviceState::Foreign => {
            return Err(gpu_refusal(GpuRefusal::ForeignDevice(pci.to_string())));
        }
    }
    let card = gpu
        .inventory()
        .ok()
        .and_then(|cards| card_for(&cards, pci).ok());
    Ok(GpuTarget { instance, card })
}

fn gpu_refusal(refusal: GpuRefusal) -> CallError {
    let code = match &refusal {
        GpuRefusal::Disabled => "GpuDisabled",
        GpuRefusal::Inventory(_) => "GpuInventoryUnavailable",
        GpuRefusal::NotInInventory(_) => "GpuNotInInventory",
        GpuRefusal::Bound(_) => "GpuBoundToVfio",
        GpuRefusal::InUse(_) => "GpuInUse",
        GpuRefusal::InUseUnknown => "GpuInUseUnknown",
        GpuRefusal::Project(_) => "GpuProjectRefused",
        GpuRefusal::Instance(_) => "GpuInstanceUnknown",
        GpuRefusal::Identity(_) => "GpuInstanceChanged",
        GpuRefusal::DeviceAbsent(_) => "GpuDeviceAbsent",
        GpuRefusal::ForeignDevice(_) => "GpuDeviceNotArca",
        GpuRefusal::NotKernelObject(_) => "GpuInstanceNotKernel",
    };
    CallError::Refused {
        code: code.to_string(),
        message: refusal.to_string(),
    }
}

/// `gpu_attach` and `gpu_detach`: the gated verbs an approved action proposal runs. The random one-shot grant is passed in
/// `approval`; the handler consumes it before doing any work, so a direct call with no grant is refused and an approval is
/// spent once. The project, instance identity and card are checked again here, because any of them may have changed.
struct GpuActionHandler {
    action: ActionKind,
    kernel: Arc<Mutex<Kernel>>,
    gpu: Option<Arc<dyn GpuBackend>>,
}

impl ToolHandler for GpuActionHandler {
    fn call(&self, args: Value, _caller: &Caller) -> Result<Value, CallError> {
        let Some(gpu) = &self.gpu else {
            return Err(gpu_refusal(GpuRefusal::Disabled));
        };
        let mut bare = args;
        let grant = bare
            .as_object_mut()
            .and_then(|object| object.remove("approval"))
            .and_then(|value| value.as_str().map(str::to_string))
            .ok_or_else(|| CallError::Invalid("approval is required".to_string()))?;
        let args = self
            .action
            .prepare(&bare, None)
            .map_err(CallError::Invalid)?;
        {
            let mut kernel = lock(&self.kernel)?;
            if !kernel.consume_action_grant(self.action.as_str(), &args, &grant) {
                return Err(CallError::Refused {
                    code: "NotApproved".to_string(),
                    message: "this action runs only from an approved proposal".to_string(),
                });
            }
        }
        let pci = args["pci"].as_str().unwrap_or_default().to_string();
        let target = match self.action {
            ActionKind::GpuAttach => prepare_attach(gpu.as_ref(), &args)?,
            _ => prepare_detach(gpu.as_ref(), &args)?,
        };
        let change = match self.action {
            ActionKind::GpuAttach => {
                gpu.attach(&target.instance, &pci)
                    .map_err(|message| CallError::Refused {
                        code: "GpuWriteFailed".to_string(),
                        message,
                    })?
            }
            _ => gpu
                .detach(&target.instance, &pci)
                .map_err(|message| CallError::Refused {
                    code: "GpuWriteFailed".to_string(),
                    message,
                })?,
        };
        if self.action == ActionKind::GpuDetach && !change.changed {
            return Err(CallError::Refused {
                code: "NothingToRemove".to_string(),
                message:
                    "nothing to remove: the instance no longer holds Arca's device for this card"
                        .to_string(),
            });
        }
        Ok(json!({
            "instance": target.instance.name,
            "project": target.instance.project,
            "pci": pci,
            "device": change.device,
            "attached": self.action == ActionKind::GpuAttach,
            "changed": change.changed,
        }))
    }
}

fn propose_failure(error: ProposeError) -> CallError {
    match error {
        ProposeError::TooMany => CallError::Refused {
            code: TOO_MANY_PROPOSALS.to_string(),
            message: "eight action proposals already wait; approve or decline one first"
                .to_string(),
        },
        ProposeError::IdCollision => CallError::Refused {
            code: "IdCollision".to_string(),
            message: "another action proposal already uses this id".to_string(),
        },
        ProposeError::Store(_) => internal(),
    }
}

/// `plan_decline`: declines one waiting delete plan or action proposal. A delete plan's objects are retained, and the
/// decline is final.
struct PlanDecline {
    kernel: Arc<Mutex<Kernel>>,
}

impl ToolHandler for PlanDecline {
    fn call(&self, args: Value, _caller: &Caller) -> Result<Value, CallError> {
        let mut kernel = lock(&self.kernel)?;
        decline_plan(&mut kernel, &args)
    }
}

/// The decline path shared by the verb and the approval page.
pub(crate) fn decline_plan(kernel: &mut Kernel, args: &Value) -> Result<Value, CallError> {
    match plan_argument(args)? {
        PlanRef::Delete(digest) => match kernel.decline(&digest).map_err(|_| internal())? {
            None => Err(no_such_plan()),
            Some(objects) => Ok(json!({
                "plan": digest.to_hex(),
                "retained": objects_json(&objects)?,
            })),
        },
        PlanRef::Action(id) => match kernel.decline_action(&id).map_err(|_| internal())? {
            false => Err(no_such_plan()),
            true => Ok(json!({ "plan": id, "retained": [] })),
        },
    }
}

/// `approval_submit`: checks an owner's approval for one waiting delete plan and, when it holds, runs the plan's delete.
/// An action proposal is approved by the owner on the approval page, which can also run its verb; this door refuses it.
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

/// The delete-plan approval path shared by `approval_submit` and the open approval page: check the proof for the named
/// waiting plan and run its delete once. The approval string is read and never stored or echoed.
pub(crate) fn approve_plan(
    kernel: &mut Kernel,
    args: &Value,
    now: u64,
) -> Result<Value, CallError> {
    let digest = match plan_argument(args)? {
        PlanRef::Delete(digest) => digest,
        PlanRef::Action(_) => return Err(no_such_plan()),
    };
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

/// The executor `action_approve` runs approved actions through, filled once the whole handler set is built.
pub type ActionSlot = Arc<std::sync::OnceLock<ActionExecutor>>;

/// `action_approve`: checks an owner's passkey approval for one waiting action proposal with this kernel's own verifier,
/// and, when it holds, runs the action once, exactly as the approval page does. The approval page of another kernel on
/// this machine relays a linked host's approvals here; it never checks them itself.
struct ActionApprove {
    kernel: Arc<Mutex<Kernel>>,
    clock: Clock,
    actions: ActionSlot,
}

impl ToolHandler for ActionApprove {
    fn call(&self, args: Value, _caller: &Caller) -> Result<Value, CallError> {
        let Some(actions) = self.actions.get() else {
            return Err(CallError::Internal(
                "the approved action has no handler".to_string(),
            ));
        };
        let now = (self.clock)();
        run_approved_action(&self.kernel, actions, &args, now)
    }
}

/// The action-proposal approval path shared by `action_approve` and the open approval page: check and spend the approval
/// under the kernel lock, run the action once outside it, then revoke the grant and journal the result.
pub fn run_approved_action(
    kernel: &Mutex<Kernel>,
    actions: &ActionExecutor,
    args: &Value,
    now: u64,
) -> Result<Value, CallError> {
    let approved = {
        let mut kernel = lock(kernel)?;
        approve_action(&mut kernel, args, now)?
    };
    // From here the approval is spent. A failure is the action's own, and it is told as one: the caller, the approval page
    // and the journal all get the handler's refusal message, never a bare code.
    let outcome = actions
        .run(&approved.action, &approved.args, &approved.grant)
        .map_err(|error| {
            let told = |why: &str| {
                format!(
                    "{APPROVED_BUT_FAILED} {} did not complete: {why}",
                    approved.action
                )
            };
            let (code, why) = match error {
                // A refusal for want of room, or for asking too often, keeps its code and its fields, and gains one:
                // the approval is spent, so waiting or making room does not make this same call pass. The action is
                // proposed again.
                CallError::RefusedWith {
                    code,
                    message,
                    mut fields,
                } => {
                    fields.insert(PROPOSE_AGAIN.to_string(), json!(true));
                    return CallError::RefusedWith {
                        code,
                        message: told(&message),
                        fields,
                    };
                }
                CallError::Refused { code, message }
                | CallError::UntrustedRefused { code, message } => (code, message),
                CallError::Invalid(message) => ("Invalid".to_string(), message),
                _ => (
                    "Internal".to_string(),
                    "Arca hit an internal error while running it".to_string(),
                ),
            };
            CallError::Refused {
                code,
                message: told(&why),
            }
        });
    let (code, message) = match outcome.as_ref().err().and_then(CallError::refusal) {
        Some((code, message)) => (Some(code), Some(message)),
        None => (None, None),
    };
    if let Ok(mut kernel) = kernel.lock() {
        kernel.revoke_action_grant(&approved.grant);
        kernel.journal_action_result(&approved, outcome.is_ok(), code, message);
    }
    outcome.map(|result| {
        json!({
            "plan": approved.id,
            "id": approved.id,
            "action": approved.action,
            "result": result,
        })
    })
}

/// How the message of an approved action that failed begins. The approval page shows a message that begins so in full,
/// since the approval itself was accepted and spent.
pub const APPROVED_BUT_FAILED: &str = "The approval was accepted, but";

/// Checks and spends an owner's approval for one action proposal, retires it and arms exactly one execution grant. The
/// caller runs the action outside the kernel lock, then journals its result.
pub fn approve_action(
    kernel: &mut Kernel,
    args: &Value,
    now: u64,
) -> Result<ApprovedAction, CallError> {
    let id = match plan_argument(args)? {
        PlanRef::Action(id) => id,
        PlanRef::Delete(_) => return Err(no_such_plan()),
    };
    let approval = args
        .get("approval")
        .and_then(Value::as_str)
        .ok_or_else(|| CallError::Invalid("approval is required".to_string()))?;
    kernel
        .approve_action(&id, approval, now)
        .map_err(action_approval_failure)
}

fn action_approval_failure(error: ActionApprovalError) -> CallError {
    match error {
        ActionApprovalError::NoSuch => no_such_plan(),
        ActionApprovalError::Expired => CallError::Refused {
            code: "Expired".to_string(),
            message: "the action proposal has expired".to_string(),
        },
        ActionApprovalError::Approval(refusal) => refusal_failure(refusal),
        // Said before the approval is checked: the owner's approval is not spent on it.
        ActionApprovalError::Unbound(template) => service_refused(format!(
            "this card was shown before this node bound a create to the content of its template ({template}), so it cannot be approved. Nothing was created and the approval was not used; propose it again"
        )),
        ActionApprovalError::Store(_) => internal(),
    }
}

/// Runs one already approved action through the same handler its gated verb uses. `service_create` gets the random
/// grant too: a template with kernel access is created only from an approved proposal. The verbs that take an inline approval
/// (`service_remove`, `service_publish`, the GPU, storage and snapshot verbs, `instance_stop` and `instance_restart`) get
/// the random grant in that field, and the
/// handler consumes it, so the owner's real approval is never stored or replayed.
#[derive(Clone, Default)]
pub struct ActionExecutor {
    handlers: Arc<HashMap<&'static str, Arc<dyn ToolHandler>>>,
}

impl ActionExecutor {
    pub fn new(handlers: impl IntoIterator<Item = (&'static str, Arc<dyn ToolHandler>)>) -> Self {
        Self {
            handlers: Arc::new(handlers.into_iter().collect()),
        }
    }

    pub fn run(&self, action: &str, args: &Value, grant: &str) -> Result<Value, CallError> {
        let handler =
            self.handlers.get(action).cloned().ok_or_else(|| {
                CallError::Internal("the approved action has no handler".to_string())
            })?;
        let mut call = args.clone();
        if matches!(
            action,
            "service_create"
                | "service_remove"
                | "service_publish"
                | "service_import"
                | "service_release"
                | "gpu_attach"
                | "gpu_detach"
                | "storage_add"
                | "snapshot_create"
                | "snapshot_delete"
                | "instance_stop"
                | "instance_restart"
                | "tenant_create"
                | "tenant_remove"
                | "deploy_remove"
                | "function_create"
                | "function_remove"
        ) {
            call["approval"] = json!(grant);
        }
        handler.call(
            call,
            &Caller {
                token_id: APPROVED_CALLER.to_string(),
                scope: Scope::Operator,
                tools: None,
            },
        )
    }
}

/// The token an approved action's handler is called as: no token a person holds, since no token may be minted
/// under it.
const APPROVED_CALLER: &str = arca_mcp::auth::APPROVED_ACTION_ID;

/// The kernel's write gate, shared by the service, database and tenant handlers and by the front pass.
fn write_gate(kernel: &Mutex<Kernel>) -> Arc<Mutex<()>> {
    kernel
        .lock()
        .map(|kernel| kernel.write_gate())
        .unwrap_or_default()
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
        message: "no delete plan or action proposal waits under this id".to_string(),
    }
}

/// A delete plan digest or an action proposal id.
enum PlanRef {
    Delete(Digest),
    Action(String),
}

fn plan_argument(args: &Value) -> Result<PlanRef, CallError> {
    let text = args
        .get("plan")
        .or_else(|| args.get("id"))
        .and_then(Value::as_str)
        .ok_or_else(|| CallError::Invalid("plan is required".to_string()))?;
    if let Some(hex) = text.strip_prefix("act-") {
        if hex.len() == 12
            && hex
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Ok(PlanRef::Action(text.to_string()));
        }
        return Err(CallError::Invalid(
            "plan is not an action proposal id".to_string(),
        ));
    }
    Digest::from_hex(text)
        .map(PlanRef::Delete)
        .map_err(|_| CallError::Invalid("plan is not a digest".to_string()))
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
    statement_text(
        host,
        "delete",
        &format!("plan-{plan}"),
        &params,
        issued_ms,
        nonce,
    )
}

/// The statement an owner signs to approve an action proposal: the action is the gated verb, the target is the proposal's
/// id, and `paramsDigest` is the proposal's `argsDigest`. The same parser reads it back before a listing carries it.
pub fn action_statement(
    host: &str,
    id: &str,
    action: &str,
    args_digest: &str,
    issued_ms: u64,
    nonce: &str,
) -> Result<String, CallError> {
    statement_text(host, action, id, args_digest, issued_ms, nonce)
}

fn statement_text(
    host: &str,
    action: &str,
    target: &str,
    params: &str,
    issued_ms: u64,
    nonce: &str,
) -> Result<String, CallError> {
    let text = format!(
        r#"{{"action":"{action}","expiresAt":"{}","host":"{host}","issuedAt":"{}","nonce":"{nonce}","paramsDigest":"{params}","target":"{target}"}}"#,
        iso_utc(issued_ms.saturating_add(STATEMENT_TTL_MS)),
        iso_utc(issued_ms),
    );
    parse_statement(text.as_bytes()).map_err(|_| internal())?;
    Ok(text)
}

/// Sixteen random bytes as 32 lowercase hex digits: a statement's nonce.
pub(crate) fn fresh_nonce() -> Result<String, CallError> {
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
        fn get_within(
            &self,
            path: &str,
            _timeout: std::time::Duration,
        ) -> Result<Value, IncusError> {
            self.get(path)
        }

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
            // The watch and the sampler both read the recursion-one listing.
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

/// The entry that owns the service project when the node's first service used the managed network.
const NET_ENTRY: &str = "net-arcak-svc";

/// What a create needs of the service project and its networks.
#[derive(Clone, Copy, Default)]
pub struct ProjectNeeds {
    /// The managed service network: a container, a database, a consumer, or a virtual machine with no host bridge.
    pub managed: bool,
    /// Room for a virtual machine.
    pub vm: bool,
    /// A NIC on a host bridge.
    pub host_bridge: bool,
}

/// The shared network is the first change when absent, in the caller's one document.
pub fn ensure_netproject(
    entries: &[arca_kernel::desired::Entry],
    cfg: &NodeConfig,
) -> Result<Vec<crate::desired_store::EntryChange>, CallError> {
    project_changes(
        entries,
        cfg,
        ProjectNeeds {
            managed: true,
            ..ProjectNeeds::default()
        },
    )
}

/// The entry that owns the service project, or `None` while the node has none: the bridged entry on a node whose first
/// service was a virtual machine on a host bridge, the network entry otherwise. The owner never changes.
fn project_owner(entries: &[arca_kernel::desired::Entry]) -> Option<&'static str> {
    [arca_netproject::BRIDGED_ENTRY, NET_ENTRY]
        .into_iter()
        .find(|id| entries.iter().any(|entry| entry.id.0 == *id))
}

/// The changes that give the service project and its networks what a create needs, first in the caller's one document.
/// The project is made by the entry that first needs it and widened by that same entry when the node gains the other
/// network (Rules 54 and 55 of the ownership document). A virtual-machine create also brings the caps the owner's entry
/// holds, and so the project's `limits.*`, to the node's configured caps. An entry that already holds what is needed is
/// not rewritten.
pub fn project_changes(
    entries: &[arca_kernel::desired::Entry],
    cfg: &NodeConfig,
    needs: ProjectNeeds,
) -> Result<Vec<crate::desired_store::EntryChange>, CallError> {
    let cidr = if needs.managed {
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
        cidr
    } else {
        ""
    };
    let find = |id: &str| entries.iter().find(|entry| entry.id.0 == id);
    let net = find(NET_ENTRY);
    let bridged = find(arca_netproject::BRIDGED_ENTRY);
    let caps = cfg.kernel.vm_caps();
    let put = |id: &str, plugin: &str, config: Value| {
        crate::desired_store::EntryChange::Put(service_entry(id, plugin, config))
    };
    let mut changes = Vec::new();
    if bridged.is_some() || (net.is_none() && !needs.managed) {
        match bridged {
            None => {
                let caps = caps.ok_or_else(internal)?;
                changes.push(put(
                    arca_netproject::BRIDGED_ENTRY,
                    "arca.bridgedproject",
                    json!({
                        "project": arca_netproject::PROJECT,
                        "virtualMachines": caps.virtual_machines,
                        "cpu": caps.cpu,
                        "memory": caps.memory,
                        "disk": caps.disk
                    }),
                ));
            }
            Some(entry) => {
                let mut config = entry.config.clone();
                if needs.managed {
                    config["managed"] = json!(true);
                }
                // The caps Incus holds follow the node's config: whenever the owner's entry is written, for a virtual
                // machine or for the widening alike, it carries the configured caps, never the ones it held. The
                // plugin writes the count on every apply, so an entry whose caps were stale (one an earlier release
                // wrote, with the project raised by hand since) would otherwise set the project below what it holds.
                if (needs.vm || config != entry.config)
                    && let Some(caps) = caps
                {
                    config["virtualMachines"] = json!(caps.virtual_machines);
                    config["cpu"] = json!(caps.cpu);
                    config["memory"] = json!(caps.memory);
                    config["disk"] = json!(caps.disk);
                }
                if config != entry.config {
                    count_fits(cfg, entries)?;
                    changes.push(put(
                        arca_netproject::BRIDGED_ENTRY,
                        "arca.bridgedproject",
                        config,
                    ));
                }
            }
        }
        if needs.managed && net.is_none() {
            changes.push(put(
                NET_ENTRY,
                "arca.netproject",
                json!({"project":arca_netproject::PROJECT,"bridge":arca_netproject::BRIDGE,"cidr":cidr,"virtualMachines":0,"sharedProject":true}),
            ));
        }
        return Ok(changes);
    }
    let cap = match (needs.vm, caps) {
        (true, Some(caps)) => caps.virtual_machines,
        (true, None) => return Err(internal()),
        (false, _) => 0,
    };
    match net {
        None => {
            let mut config = json!({"project":arca_netproject::PROJECT,"bridge":arca_netproject::BRIDGE,"cidr":cidr,"virtualMachines":cap});
            if needs.host_bridge {
                config["hostBridges"] = json!(true);
            }
            changes.push(put(NET_ENTRY, "arca.netproject", config));
        }
        Some(entry) => {
            let mut config = entry.config.clone();
            // The cap Incus holds follows the node's config, as the bridged owner's caps do: a virtual-machine create
            // writes the configured cap into the entry when it differs, up or down.
            if needs.vm {
                config["virtualMachines"] = json!(cap);
            }
            if needs.host_bridge && config["hostBridges"] != json!(true) {
                config["hostBridges"] = json!(true);
            }
            if config != entry.config {
                if needs.vm {
                    count_fits(cfg, entries)?;
                }
                changes.push(put(NET_ENTRY, "arca.netproject", config));
            }
        }
    }
    Ok(changes)
}

/// Refuses, in words and before any write, a write of the project owner's entry whose configured virtual-machine count is
/// below the number of virtual machines the node holds: Incus would refuse that count for the project, and the entry would
/// then fail on every pass. It is a `capacity` refusal: the node holds more virtual machines than its cap allows.
fn count_fits(cfg: &NodeConfig, entries: &[arca_kernel::desired::Entry]) -> Result<(), CallError> {
    let Some(caps) = cfg.kernel.vm_caps() else {
        return Ok(());
    };
    let held = service_entries(entries)
        .iter()
        .filter(|(_, record)| record.vm.is_some())
        .count();
    if held as u64 <= u64::from(caps.virtual_machines) {
        return Ok(());
    }
    let key = if cfg.kernel.bridged.is_some() {
        "kernel.bridged.limits.virtualMachines"
    } else {
        "kernel.vmLimits.virtualMachines"
    };
    Err(CallError::capacity(
        format!(
            "this node holds {held} virtual machines, but {key} is {}: raise it to at least {held}, or remove virtual machines first. Nothing was written",
            caps.virtual_machines
        ),
        Room {
            scope: Whose::Node,
            limit: Limit::VirtualMachines,
            have: Some(held as u64),
            want: None,
            max: Some(u64::from(caps.virtual_machines)),
        },
    ))
}

/// Why a create has no service project to go into, in the words of the check that failed: only a missing
/// `kernel.serviceCidr` is called one.
fn no_service_project(error: CallError) -> CallError {
    match error {
        CallError::Invalid(why) => service_refused(format!("no service project: {why}")),
        // A refusal in words is the reason itself.
        refused @ (CallError::Refused { .. } | CallError::RefusedWith { .. }) => refused,
        _ => internal(),
    }
}

fn no_vm_caps(template: &str) -> String {
    format!(
        "template {template} is a virtual machine, and this node sets no virtual-machine caps (kernel.vmLimits, or kernel.bridged with its limits)"
    )
}

fn bridged_only(template: &str) -> String {
    format!(
        "this node is a bridged node: it runs virtual-machine templates only, and template {template} is not one"
    )
}

/// Why the node's config and its desired state disagree, in plain words, or `None` when they agree. Holding containers
/// and virtual machines together is never the reason: what is refused is a config that would leave a half-state (Rule 57
/// of the ownership document). A config without the managed network is refused while anything lives on that network, a
/// config without `kernel.bridged` while a virtual machine is on a host bridge or the project is still a bridged-only
/// one, and a config without caps while any virtual machine exists. `arca serve` refuses to start on a disagreement,
/// before the kernel reconciles anything, and a virtual-machine create checks it again before its first write.
pub fn mode_refusal(cfg: &NodeConfig, entries: &[arca_kernel::desired::Entry]) -> Option<String> {
    let is_vm = |entry: &arca_kernel::desired::Entry| {
        entry.plugin.0 == "arca.service" && entry.config.get("vm").is_some_and(|vm| !vm.is_null())
    };
    let on_host_bridge = |entry: &arca_kernel::desired::Entry| {
        is_vm(entry) && entry.config["bridge"].as_str() != Some(arca_netproject::BRIDGE)
    };
    let kernel = &cfg.kernel;
    if kernel.bridged.is_some() && kernel.service_cidr.is_none() {
        let managed = entries.iter().find(|entry| {
            matches!(
                entry.plugin.0.as_str(),
                "arca.netproject" | "arca.postgres" | "arca.consumer"
            ) || (entry.plugin.0 == "arca.service" && !on_host_bridge(entry))
        })?;
        return Some(format!(
            "kernel.bridged is set, but this node already holds {} from the managed service network. A node is bridged from its first service or not at all: remove kernel.bridged, or remove those services first",
            managed.id.0
        ));
    }
    if kernel.bridged.is_none() {
        let unwidened = entries.iter().find(|entry| {
            entry.plugin.0 == "arca.bridgedproject" && entry.config["managed"] != json!(true)
        });
        if let Some(vm) = entries.iter().find(|entry| on_host_bridge(entry)) {
            // Removing the machines helps only where the project is not a bridged-only one.
            return Some(match unwidened {
                Some(_) => format!(
                    "kernel.bridged is not set, but this node already holds {} from bridged mode, in a service project created in bridged mode. Put kernel.bridged back. To use the managed service network, set kernel.serviceCidr beside kernel.bridged: the node then becomes a mixed node, never a managed-only one",
                    vm.id.0
                ),
                None => format!(
                    "kernel.bridged is not set, but this node already holds {} from bridged mode. Put kernel.bridged back, or remove its virtual machines first",
                    vm.id.0
                ),
            });
        }
        // The project is still a bridged-only one, and nothing the owner can remove changes that: the way to the
        // managed network is to add it beside kernel.bridged, which widens the project.
        if let Some(project) = unwidened {
            return Some(format!(
                "kernel.bridged is not set, but this node's service project was created in bridged mode ({}). Put kernel.bridged back. To use the managed service network, set kernel.serviceCidr beside kernel.bridged: the node then becomes a mixed node, never a managed-only one",
                project.id.0
            ));
        }
    }
    if kernel.vm_caps().is_none()
        && let Some(vm) = entries.iter().find(|entry| is_vm(entry))
    {
        return Some(format!(
            "this node already holds the virtual machine {}, but its config sets no virtual-machine caps. Put kernel.vmLimits back, or remove its virtual machines first",
            vm.id.0
        ));
    }
    None
}

/// The configured bridges that are not bridges on this host: `<sys>/<name>/bridge` is a directory only for a bridge
/// interface. `sys` is `/sys/class/net`; nothing is asked of Incus.
pub fn missing_bridges(
    bridged: &crate::config::BridgedConfig,
    sys: &std::path::Path,
) -> Vec<String> {
    bridged
        .bridges
        .iter()
        .filter(|bridge| !sys.join(bridge).join("bridge").is_dir())
        .cloned()
        .collect()
}

/// The bridge a virtual-machine create joins: the one the request names, or the node's only one. It is always one of the
/// node's allowed bridges.
fn vm_bridge(bridged: &crate::config::BridgedConfig, args: &Value) -> Result<String, CallError> {
    let bridge = match args.get("bridge") {
        Some(value) => value
            .as_str()
            .filter(|bridge| crate::service::host_bridge_ok(bridge))
            .ok_or_else(|| CallError::Invalid("bridge: expected a host bridge name".into()))?,
        None => match bridged.bridges.as_slice() {
            [only] => only.as_str(),
            _ => {
                return Err(service_refused(
                    "this node allows more than one bridge: name one with bridge",
                ));
            }
        },
    };
    if !bridged.bridges.iter().any(|allowed| allowed == bridge) {
        return Err(service_refused(format!(
            "bridge {bridge} is not one of the bridges this node allows (kernel.bridged.bridges)"
        )));
    }
    Ok(bridge.to_string())
}

/// Where a virtual machine's NIC goes.
enum VmNetwork {
    /// The managed service network, with an address of Arca's plan.
    Managed,
    /// One of the node's allowed host bridges.
    Host(String),
}

/// The network a virtual-machine create joins. A named `bridge` is a host bridge and must be one the node allows. With no
/// `bridge`, a node that has the managed service network uses it, and a bridged node uses its only bridge.
fn vm_network(cfg: &NodeConfig, args: &Value) -> Result<VmNetwork, CallError> {
    match (&cfg.kernel.bridged, args.get("bridge")) {
        (_, None) if cfg.kernel.service_cidr.is_some() => Ok(VmNetwork::Managed),
        (Some(bridged), _) => vm_bridge(bridged, args).map(VmNetwork::Host),
        (None, _) => Err(service_refused(
            "bridge: this node allows no host bridge (kernel.bridged is not set); leave bridge out to join the managed service network",
        )),
    }
}

/// Why a virtual-machine template cannot go on a host bridge, or `None`.
fn host_bridge_refusal(template: &crate::service::Template) -> Option<String> {
    (!template.ports.is_empty() || template.provides.is_some()).then(|| {
        format!(
            "template {} declares ports, which a link opens in the managed service network's firewall rules: a host bridge has none, so leave bridge out or use a template without ports",
            template.name
        )
    })
}

/// Whether an Incus instance row holds `mac` (lowercase): on a device of its own, on a device a profile gives it, as the
/// address Incus assigned (`volatile.<device>.hwaddr`), or on a live interface. Devices on the bridge that Incus does not
/// run cannot be seen from here.
fn instance_holds_mac(row: &Value, mac: &str) -> bool {
    let same = |held: &Value| {
        held.as_str()
            .is_some_and(|held| held.eq_ignore_ascii_case(mac))
    };
    let on_device = |devices: &Value| {
        devices
            .as_object()
            .is_some_and(|devices| devices.values().any(|device| same(&device["hwaddr"])))
    };
    let assigned = |config: &Value| {
        config.as_object().is_some_and(|config| {
            config.iter().any(|(key, value)| {
                key.starts_with("volatile.") && key.ends_with(".hwaddr") && same(value)
            })
        })
    };
    on_device(&row["devices"])
        || on_device(&row["expanded_devices"])
        || assigned(&row["config"])
        || assigned(&row["expanded_config"])
        || row["state"]["network"]
            .as_object()
            .is_some_and(|nics| nics.values().any(|nic| same(&nic["hwaddr"])))
}

/// Why creating `name` from `template` would exceed one of the node's caps, or `None`: a `capacity` refusal that names
/// the cap, with `scope` `node`. A tenant's machine is checked here too, before its tenant's own caps, so a tenant
/// under its caps is refused with the node's amounts when the node is full. The use counted is that of the virtual-machine records Arca holds, on the managed network and on host
/// bridges alike, whether or not their machines run. No container counts.
fn caps_refusal(
    cfg: &NodeConfig,
    caps: &crate::config::BridgedLimits,
    entries: &[arca_kernel::desired::Entry],
    own: bool,
    name: &str,
    template: &crate::service::Template,
) -> Option<CallError> {
    let cpu = |limits: &Option<crate::service::Limits>| {
        limits
            .as_ref()
            .and_then(|limits| limits.cpu.as_deref()?.parse::<u128>().ok())
    };
    let memory = |limits: &Option<crate::service::Limits>| {
        limits
            .as_ref()
            .and_then(|limits| crate::service::size_bytes(limits.memory.as_deref()?))
    };
    let (mut count, mut cpus, mut ram, mut disk) = (0u128, 0u128, 0u128, 0u128);
    // Each tenant's virtual-machine cap is set aside for it: a machine of the node's own project counts them all as
    // taken, and a tenant's machine, which is inside its tenant's cap, counts the node's own and the caps.
    let reserved = tenants::reserved_vms(entries);
    for (_, record) in service_entries(entries) {
        let Some(vm) = &record.vm else { continue };
        if record.tenant.is_none() {
            count += 1;
        }
        cpus = cpus.saturating_add(cpu(&record.limits).unwrap_or(u128::MAX));
        ram = ram.saturating_add(memory(&record.limits).unwrap_or(u128::MAX));
        disk = disk.saturating_add(crate::service::size_bytes(&vm.root_size).unwrap_or(u128::MAX));
    }
    let source = if cfg.kernel.bridged.is_some() {
        "kernel.bridged.limits"
    } else {
        "kernel.vmLimits"
    };
    let gib = |bytes: u128| format!("{:.1} GiB", bytes as f64 / (1u64 << 30) as f64);
    let plain = |n: u128| n.to_string();
    // The cap, what is capped, its config key, the use, the amount asked, the cap's amount, and how an amount is shown.
    type Check<'a> = (
        Limit,
        &'a str,
        &'a str,
        u128,
        Option<u128>,
        Option<u128>,
        &'a dyn Fn(u128) -> String,
    );
    let checks: [Check<'_>; 4] = [
        (
            Limit::VirtualMachines,
            "virtual machines",
            "virtualMachines",
            count + reserved,
            Some(u128::from(own)),
            Some(u128::from(caps.virtual_machines)),
            &plain,
        ),
        (
            Limit::Cpu,
            "CPUs",
            "cpu",
            cpus,
            cpu(&template.limits),
            Some(u128::from(caps.cpu)),
            &plain,
        ),
        (
            Limit::Memory,
            "memory",
            "memory",
            ram,
            memory(&template.limits),
            crate::service::size_bytes(&caps.memory),
            &gib,
        ),
        (
            Limit::Disk,
            "disk",
            "disk",
            disk,
            template
                .root_size
                .as_deref()
                .and_then(crate::service::size_bytes),
            crate::service::size_bytes(&caps.disk),
            &gib,
        ),
    ];
    for (limit, what, key, used, asked, cap, show) in checks {
        // A size that cannot be read counts as over the cap: the create is refused, never let through.
        let asked = asked.unwrap_or(u128::MAX);
        let cap = cap.unwrap_or(0);
        if asked != 0 && used.saturating_add(asked) > cap {
            return Some(CallError::capacity(
                format!(
                    "creating {name} would exceed this node's cap on {what} ({source}.{key}): {} in use, {} asked, {} allowed",
                    show(used),
                    if asked == u128::MAX {
                        "an unreadable amount".to_string()
                    } else {
                        show(asked)
                    },
                    show(cap)
                ),
                Room::of(Whose::Node, limit, used, asked, cap),
            ));
        }
    }
    None
}

/// Why creating `name` from `template` would pass a cap, or `None`: the node's caps on virtual machines first
/// ([`caps_refusal`]), then the caps of the tenant the create names. `entries` are all the node's records. The
/// proposal and the create both ask here, so a proposal the node has no room for is refused when it is made, in
/// the words the create would use, and the create asks again because the node can fill up while a card waits.
fn create_room_refusal(
    cfg: &NodeConfig,
    entries: &[arca_kernel::desired::Entry],
    tenant: Option<&tenants::TenantScope>,
    name: &str,
    template: &crate::service::Template,
) -> Option<CallError> {
    // A node that sets no caps for virtual machines creates none: both callers refuse that first, in its own words.
    if template.is_vm()
        && let Some(caps) = cfg.kernel.vm_caps()
        && let Some(refusal) = caps_refusal(cfg, caps, entries, tenant.is_none(), name, template)
    {
        return Some(refusal);
    }
    tenant.and_then(|scope| tenants::service_caps_refusal(scope, entries, name, template))
}

/// The checks a proposed `service_create` must pass on this node before the owner is asked: the template's kind fits the
/// node's networks, the config fits the desired state, and a virtual machine's bridge is an allowed one. On a bridged
/// node an omitted bridge is filled in, so the owner's card names it; a virtual machine for the managed network names none.
/// Last, the create must fit under the node's caps and its tenant's ([`create_room_refusal`]).
fn enrich_service_create(
    cfg: &NodeConfig,
    entries: &[arca_kernel::desired::Entry],
    payload: &Value,
    card_fact: &mut Option<String>,
    now: u64,
) -> Result<Value, CallError> {
    // First, as in the create: the tenant the proposal names, and what the node's config and records refuse before
    // any template is read.
    let tenant = tenants::tenant_arg(cfg, payload, entries)?;
    if let Some(refusal) = proposed_before_template(cfg, entries, payload, tenant.as_ref()) {
        return Err(refusal);
    }
    // A private destination comes from the node's config alone, by the service's name. The proposal names it, so the
    // owner's card says it and the approval binds it; a caller that states anything else is refused now.
    let destinations = destinations_for(
        cfg,
        payload["name"].as_str().unwrap_or_default(),
        payload.get("tenant").is_some(),
        None,
    )?;
    if payload
        .get("destinations")
        .is_some_and(|given| destinations.is_empty() || *given != json!(destinations))
    {
        return Err(destinations_mismatch(
            payload["name"].as_str().unwrap_or_default(),
            &destinations,
            payload.get("tenant").is_some(),
        ));
    }
    let named = !destinations.is_empty();
    // An address of the node's own is refused now when Incus can be asked, so the owner is never shown a card for it;
    // the create asks again.
    if named
        && let Ok(hosts) = host_observation(&UnixIncus::new(cfg.incus.socket.clone()))
        && let Some(why) = own_address_refusal(
            payload["name"].as_str().unwrap_or_default(),
            &destinations,
            &hosts,
        )
    {
        return Err(service_refused(why));
    }
    let payload = &{
        let mut payload = payload.clone();
        if let (true, Some(object)) = (named, payload.as_object_mut()) {
            object.insert("destinations".to_string(), json!(destinations));
        }
        payload
    };
    // A proposal that names no template is refused by its schema; one whose template does not load, or names a
    // script that cannot be read, is refused now in the words the create would use, so the owner is never shown a
    // card for it.
    let Some(template_name) = payload["template"].as_str() else {
        return Ok(payload.clone());
    };
    let content =
        crate::service::load_template_content(&cfg.state_dir.join("templates"), template_name)
            .map_err(service_template_error)?;
    // What the owner is shown is what is created: the proposal is given the image the template names and the
    // fingerprint of the template's content as this node holds it now, so the card shows the image and the approval
    // binds both. A caller that states either is only compared with the node's own.
    let image = crate::service::template_image(&content.template);
    let digest = content.bound().map_err(service_template_error)?.to_string();
    if payload
        .get("image")
        .is_some_and(|given| given.as_str() != Some(image.as_str()))
        || payload
            .get("templateDigest")
            .is_some_and(|given| given.as_str() != Some(digest.as_str()))
    {
        return Err(template_not_the_one_stated(template_name));
    }
    // Why this template is created stopped whatever the call's `start` says, for the card's `Start` row: from the
    // template this fingerprint is of, and from the secrets stored for the service now.
    if let Some(name) = payload["name"].as_str() {
        *card_fact = crate::service::template_stopped_reason(
            &content.template,
            &crate::service::entry_id(payload["tenant"].as_str(), name),
            &cfg.state_dir.join("secrets"),
        );
    }
    let payload = &{
        let mut payload = payload.clone();
        if let Some(object) = payload.as_object_mut() {
            object.insert("image".to_string(), json!(image));
            object.insert("templateDigest".to_string(), json!(digest));
        }
        payload
    };
    let template = content.template;
    // A create that could never give the kernel access its template declares is refused now, so the owner is never asked
    // to approve it. The proposal then names the access, so the owner's card says it and the approval binds it.
    let stated = kernel_access_for(cfg, &template, payload.get("tenant").is_some(), None)?
        .map(|(port, _)| kernel_access_arg(port));
    if payload
        .get("kernelAccess")
        .is_some_and(|given| Some(given) != stated.as_ref())
    {
        return Err(kernel_access_mismatch(&template));
    }
    if !template.is_vm() {
        if cfg.kernel.bridged.is_some() && cfg.kernel.service_cidr.is_none() {
            return Err(service_refused(bridged_only(&template.name)));
        }
        if payload.get("bridge").is_some() || payload.get("mac").is_some() {
            return Err(CallError::Invalid(
                "bridge and mac: only a virtual-machine template takes them".into(),
            ));
        }
        crate::service::create_settings(&template, payload.get("settings"))
            .map_err(service_template_error)?;
        if tenant.is_some() && (template.lan.is_some() || template.publishable) {
            return Err(service_refused(tenant_host_port(&template.name)));
        }
        if let Some(refusal) = create_room_refusal(
            cfg,
            entries,
            tenant.as_ref(),
            payload["name"].as_str().unwrap_or_default(),
            &template,
        ) {
            return Err(refusal);
        }
        // The node's own project and network are written with the first service that needs them: a write the
        // create would refuse is refused now. Not on a node that only looks, whose proposal is recorded as before.
        if tenant.is_none() && !node_only_looks(cfg) {
            ensure_netproject(entries, cfg).map_err(no_service_project)?;
        }
        tenants::joinable_deploy(cfg, tenant.as_ref(), payload, APPROVED_CALLER, now)?;
        let mut payload = payload.clone();
        if let (Some(stated), Some(object)) = (stated, payload.as_object_mut()) {
            object.insert("kernelAccess".to_string(), stated);
        }
        return Ok(payload);
    }
    // The settings a create gives are checked now, so the owner's card never shows a value the create would refuse.
    crate::service::create_settings(&template, payload.get("settings"))
        .map_err(service_template_error)?;
    if cfg.kernel.vm_caps().is_none() {
        return Err(service_refused(no_vm_caps(&template.name)));
    }
    if let Some(why) = mode_refusal(cfg, entries) {
        return Err(service_refused(why));
    }
    let mut payload = payload.clone();
    // A tenant's machine joins the managed service network and nothing else.
    if tenant.is_some() && payload.get("bridge").is_some() {
        return Err(service_refused(TENANT_BRIDGE));
    }
    let network = if tenant.is_some() {
        VmNetwork::Managed
    } else {
        vm_network(cfg, &payload)?
    };
    match &network {
        VmNetwork::Managed => {
            if payload.get("mac").is_some() {
                return Err(service_refused(MANAGED_MAC));
            }
        }
        VmNetwork::Host(bridge) => {
            if let Some(why) = host_bridge_refusal(&template) {
                return Err(service_refused(why));
            }
            if named {
                return Err(service_refused(destinations_on_host_bridge(
                    payload["name"].as_str().unwrap_or_default(),
                )));
            }
            if let Some(object) = payload.as_object_mut() {
                object.insert("bridge".to_string(), Value::String(bridge.clone()));
            }
        }
    }
    // A MAC another of this node's records holds is refused now. Whether an instance Arca holds no record of has
    // it is read from Incus, and the create asks that.
    if let Some(mac) = payload
        .get("mac")
        .and_then(Value::as_str)
        .and_then(crate::service::normalize_mac)
        && record_holds_mac(&tenants::scoped_entries(entries, tenant.as_ref()), &mac)
    {
        return Err(service_refused(mac_held(&mac)));
    }
    // As in the create: a machine the node has no room for is refused now, so the owner is never asked to
    // approve it.
    if let Some(refusal) = create_room_refusal(
        cfg,
        entries,
        tenant.as_ref(),
        payload["name"].as_str().unwrap_or_default(),
        &template,
    ) {
        return Err(refusal);
    }
    // Last, the write of the node's own project that the create would refuse: its entry would carry a count of
    // virtual machines below what the node holds. Not on a node that only looks, whose proposal is recorded as
    // before.
    if tenant.is_none() && !node_only_looks(cfg) {
        project_changes(
            entries,
            cfg,
            ProjectNeeds {
                managed: matches!(network, VmNetwork::Managed),
                vm: true,
                host_bridge: matches!(network, VmNetwork::Host(_)),
            },
        )
        .map_err(no_service_project)?;
    }
    // A deploy the approved create could not add to: one another token opened, or one that is closed. The approved
    // create runs as the approval's own caller, and the deploy journal says who opened what.
    tenants::joinable_deploy(cfg, tenant.as_ref(), &payload, APPROVED_CALLER, now)?;
    Ok(payload)
}

/// What a proposed `service_create` is refused for before its template is read, in the words and the order of the
/// create: a deploy named outside a tenant, a config that sets up no service project for it, a name a function
/// holds, and a name a service holds. All of it is read from the node's config and its records. Whether an instance
/// Arca holds no record of has the name is read from Incus, and the create asks that. On a node that only looks
/// two things are not asked: its config's service project here, and the write of that project in
/// [`enrich_service_create`]. Its create answers that the node only looks, so a proposal those two alone would
/// refuse is recorded there as before. Every other check, here and there, is made on such a node as on any other.
fn proposed_before_template(
    cfg: &NodeConfig,
    entries: &[arca_kernel::desired::Entry],
    payload: &Value,
    tenant: Option<&tenants::TenantScope>,
) -> Option<CallError> {
    if tenant.is_none() && payload.get("deploy").is_some() {
        return Some(CallError::Invalid(DEPLOY_OUTSIDE_TENANT.into()));
    }
    if !node_only_looks(cfg) {
        let project = if tenant.is_some() {
            tenants::network(cfg).map(|_| ())
        } else if no_service_project_asked(cfg) && holds_service_records(entries) {
            Err(service_records_without_config())
        } else if cfg.kernel.bridged.is_some() && cfg.kernel.service_cidr.is_none() {
            bridged_project(cfg)
        } else {
            service_project(cfg).map(|_| ())
        };
        if let Err(refusal) = project {
            return Some(refusal);
        }
    }
    if let Some(refusal) = function_handlers::kind_refusal("service_create", payload, entries) {
        return Some(refusal);
    }
    let name = payload["name"].as_str()?;
    let tenant_id = tenant.map(|scope| scope.id.as_str());
    let id = crate::service::entry_id(tenant_id, name);
    if entries
        .iter()
        .any(|entry| entry.id.0 == id && entry.plugin.0 == "arca.service")
    {
        return Some(service_exists(name));
    }
    instance_name_taken(entries, tenant_id, name).then(|| name_taken(name))
}

/// Whether another service record on the node, of the node's own or of any tenant, already has the instance name a
/// create of `name` in `tenant` would take. A tenant's instance is `<name>-<tenant id>` and the node's own is its
/// name alone, so the node's service `x-<id>` and tenant `<id>`'s service `x` are one instance name; Incus takes a
/// name once on the service bridge, whatever the project. `entries` are all the node's records.
fn instance_name_taken(
    entries: &[arca_kernel::desired::Entry],
    tenant: Option<&str>,
    name: &str,
) -> bool {
    let id = crate::service::entry_id(tenant, name);
    let instance = crate::service::instance_name(tenant, name);
    service_entries(entries)
        .iter()
        .any(|(entry, other)| entry.id.0 != id && other.instance == instance)
}

/// The refusal of a create whose instance name another service on the node has. It says nothing of that service:
/// it may be another side's.
fn name_taken(name: &str) -> CallError {
    service_refused(format!(
        "the name {name} is taken on this node: the instance this service would get has the name of another service's instance. Choose another name"
    ))
}

/// The refusal of a create whose name a service already holds.
fn service_exists(name: &str) -> CallError {
    service_refused(format!("service {name} exists"))
}

/// Why a create outside a tenant that names a deploy is not made.
const DEPLOY_OUTSIDE_TENANT: &str = "deploy: only a call inside a tenant names a deploy";

/// Why a tenant's virtual machine that names a host bridge is not made.
const TENANT_BRIDGE: &str = "bridge: a virtual machine in a tenant joins the managed service network only, never a host bridge";

/// Why a tenant's service is not made from a template that opens a port on the host or publishes.
fn tenant_host_port(template: &str) -> String {
    format!(
        "template {template} opens a port on the host or publishes a dash, and nothing in a tenant does: a tenant's service is reached through the node's front service"
    )
}

/// Whether one of `entries`, the records on the call's own side, is a virtual machine that holds `mac`.
fn record_holds_mac(entries: &[arca_kernel::desired::Entry], mac: &str) -> bool {
    service_entries(entries)
        .iter()
        .any(|(_, other)| other.vm.as_ref().and_then(|vm| vm.mac.as_deref()) == Some(mac))
}

/// Why a create that states a MAC something else holds is not made.
fn mac_held(mac: &str) -> String {
    format!("the MAC {mac} is already held by another instance")
}

/// What a `service_create` call brings to a template that declares kernel access: what it stated as `kernelAccess`, and
/// whether it runs from a proposal the owner approved.
struct KernelAccessCall<'a> {
    stated: Option<&'a Value>,
    approved: bool,
}

/// The kernel access a create of `template` gives: the port inside the instance and this node's services socket, or
/// `None` for a template that declares none. A template that declares it is refused, before anything is written, in a
/// tenant and on a node with no `listen.services`. A create (`call`) of one runs only from a proposal the owner
/// approved, whose card says what the access is, and states exactly the template's `kernelAccess`; a proposal passes
/// `None`, since the access is filled in when it is made.
fn kernel_access_for(
    cfg: &NodeConfig,
    template: &crate::service::Template,
    in_tenant: bool,
    call: Option<KernelAccessCall<'_>>,
) -> Result<Option<(u16, String)>, CallError> {
    let name = &template.name;
    let Some(access) = &template.kernel_access else {
        return match call {
            Some(KernelAccessCall {
                stated: Some(_), ..
            }) => Err(kernel_access_mismatch(template)),
            _ => Ok(None),
        };
    };
    if in_tenant {
        return Err(service_refused(format!(
            "template {name} gives its service access to this node's kernel, and nothing in a tenant has that: only a service of the node's own project is created from it"
        )));
    }
    let Some(connect) = cfg.kernel_access() else {
        return Err(service_refused(format!(
            "template {name} gives its service access to this node's kernel, and this node has no socket for it (listen.services is not set): set \"listen\": {{ \"services\": \"<a path beside listen.unix, such as /run/arca/services.sock>\" }} in the node's config and restart serve. That socket serves the kernel's MCP alone, and a token is needed on every call"
        )));
    };
    if let Some(call) = call {
        if !call.approved {
            return Err(CallError::Refused {
                code: "NotApproved".into(),
                message: format!(
                    "template {name} gives its service access to this node's kernel, so its create runs only from a proposal the owner approved: propose it with action_propose, and the owner's card says what the access is"
                ),
            });
        }
        if call.stated != Some(&kernel_access_arg(access.port)) {
            return Err(service_refused(format!(
                "template {name} gives its service access to this node's kernel (\"kernelAccess\": {{ \"port\": {} }}), and the proposal the owner approved does not say that: the template changed after it was proposed. Propose the create again, so the owner's card says it",
                access.port
            )));
        }
    }
    Ok(Some((access.port, connect)))
}

/// What a `service_create` call brings to a service the node's config gives a private destination: what it stated as
/// `destinations`, and whether it runs from a proposal the owner approved.
struct DestinationsCall<'a> {
    stated: Option<&'a Value>,
    approved: bool,
}

/// The private destinations a create of the service `name` records: the ones this node's config names for it
/// (`kernel.serviceDestinations`), and none for any other service. A tenant's service never has one: the config is not
/// even read for it, so a tenant's service that bears the name of an entry gets nothing. A create (`call`) of a named
/// service runs only from a proposal the owner approved, whose card shows each address, and states exactly what the
/// config names now; a proposal passes `None`, since the destinations are filled in when it is made. No caller can
/// state one of its own: what a call states is only ever compared with the config.
fn destinations_for(
    cfg: &NodeConfig,
    name: &str,
    in_tenant: bool,
    call: Option<DestinationsCall<'_>>,
) -> Result<Vec<arca_netproject::destination::Destination>, CallError> {
    let named = if in_tenant {
        Vec::new()
    } else {
        cfg.kernel.destinations_of(name)
    };
    let Some(call) = call else {
        return Ok(named);
    };
    if named.is_empty() {
        return match call.stated {
            Some(_) => Err(destinations_mismatch(name, &named, in_tenant)),
            None => Ok(named),
        };
    }
    if !call.approved {
        return Err(CallError::Refused {
            code: "NotApproved".into(),
            message: format!(
                "this node's config lets the service {name} reach {} through this node (kernel.serviceDestinations), so its create runs only from a proposal the owner approved: propose it with action_propose, and the owner's card shows the address",
                crate::action_proposal::destinations_list(&json!(named))
            ),
        });
    }
    if call.stated != Some(&json!(named)) {
        return Err(service_refused(format!(
            "this node's config lets the service {name} reach {} through this node (kernel.serviceDestinations), and the proposal the owner approved does not say that: the config changed after it was proposed. Propose the create again, so the owner's card says it",
            crate::action_proposal::destinations_list(&json!(named))
        )));
    }
    Ok(named)
}

/// The refusal for a call whose `destinations` is not what this node's config names for the service.
fn destinations_mismatch(
    name: &str,
    named: &[arca_netproject::destination::Destination],
    in_tenant: bool,
) -> CallError {
    CallError::Invalid(if in_tenant {
        "destinations: nothing in a tenant reaches a private address through the node. Only the node's config names one, and only for a service of the node's own project".to_string()
    } else if named.is_empty() {
        format!(
            "destinations: this node's config (kernel.serviceDestinations) names none for the service {name}. Only the config names one, never a call"
        )
    } else {
        format!(
            "destinations: this node's config names {} for the service {name}, and the call must state exactly that",
            crate::action_proposal::destinations_list(&json!(named))
        )
    })
}

/// The host's addresses as the service plugin observes them (`Observe::HostAddresses`), so a handler judges a
/// destination over the very list the rules are built from.
fn host_observation(read: &dyn arca_incus::IncusRead) -> Result<Value, CallError> {
    arca_providers::incus::host_networks(read)
        .map(|networks| arca_providers::incus::host_addresses_value(&networks))
        .map_err(|_| internal())
}

/// Why a create of the service `name` is refused over `hosts`, the node's own addresses: the config names as a
/// destination one of them, or the network or broadcast address of one of the node's networks, which the node itself
/// answers on. The rules of a service never open such an address, so the entry would promise what nothing gives.
fn own_address_refusal(
    name: &str,
    destinations: &[arca_netproject::destination::Destination],
    hosts: &Value,
) -> Option<String> {
    let own = arca_netproject::destination::node_own(hosts);
    let named = destinations.iter().find(|one| own.contains(&one.address))?;
    Some(format!(
        "this node's config names {} as a private destination of the service {name}, and that address is this node's own (one of its addresses, or the network or broadcast address of one of its networks): a service's rules never open one. Take the entry out of kernel.serviceDestinations",
        named.address
    ))
}

/// Why a service the config gives a destination is not created on a host bridge.
fn destinations_on_host_bridge(name: &str) -> String {
    format!(
        "this node's config names a private destination for the service {name} (kernel.serviceDestinations), and this create puts it on a host bridge, where Arca writes no network rules: the machine reaches whatever that network reaches. Create it on the managed service network, or take the entry out of the config"
    )
}

/// Why each destination a service's record names does not stand as recorded, in the record's order: the address is
/// one of the node's own (the rules never open one), or the rules Incus holds for the service's NIC are not the ones
/// the record gives. `None` for one that stands.
fn destination_problems(
    config: &crate::service::ServiceConfig,
    read: &dyn arca_incus::IncusRead,
) -> Vec<Option<String>> {
    let hosts = host_observation(read).ok();
    let own = hosts
        .as_ref()
        .map(arca_netproject::destination::node_own)
        .unwrap_or_default();
    let rules = read
        .get(&format!(
            "/1.0/network-acls/{}?project=default",
            config.acl()
        ))
        .ok();
    config
        .destinations
        .iter()
        .map(|destination| {
            let Some(hosts) = &hosts else {
                return Some("the node's own addresses could not be read from Incus".to_string());
            };
            if own.contains(&destination.address) {
                return Some(format!(
                    "{} is this node's own (one of its addresses, or the network or broadcast address of one of its networks), and a service's rules never open one",
                    destination.address
                ));
            }
            let wanted = crate::service::egress(config, hosts);
            match rules.as_ref().and_then(|acl| acl["egress"].as_array()) {
                Some(held) if arca_incus::acl_rules_match(held, &wanted) => None,
                Some(_) => Some(
                    "the network rules Incus holds for this service are not the ones its record gives"
                        .to_string(),
                ),
                None => Some(
                    "the network rules of this service could not be read from Incus".to_string(),
                ),
            }
        })
        .collect()
}

/// Why `serve` does not start over this desired state: a service's record names a private destination this config no
/// longer names for it. The owner approved that address because the operator's config named it; a start that left
/// the service's rules open with the config saying otherwise is refused.
pub fn destinations_refusal(
    cfg: &NodeConfig,
    entries: &[arca_kernel::desired::Entry],
) -> Option<String> {
    entries
        .iter()
        .filter(|entry| entry.plugin.0 == "arca.service")
        .find_map(|entry| {
            let recorded = entry.config.get("destinations")?.as_array()?;
            let name = entry.config["name"].as_str().unwrap_or(entry.id.0.as_str());
            // A tenant's record never carries the key, and the config never names a tenant's service.
            let named = if entry.config.get("tenant").is_some() {
                Value::Array(Vec::new())
            } else {
                json!(cfg.kernel.destinations_of(name))
            };
            let lost = recorded
                .iter()
                .find(|one| !named.as_array().is_some_and(|named| named.contains(one)))?;
            Some(format!(
                "the service {name} may reach {} through this node, as its owner approved, and this config's kernel.serviceDestinations does not name that for it: put the entry back, or remove the service before taking it out",
                crate::action_proposal::destinations_list(&json!([lost]))
            ))
        })
}

/// The `kernelAccess` argument of a create whose template has that port.
fn kernel_access_arg(port: u16) -> Value {
    json!({ "port": port })
}

/// Why the kernel access a service's record names does not stand as recorded, in the order a reader would check it:
/// the device on its instance, then this server's listener. `None` when both are as the record says.
fn kernel_access_problem(
    cfg: &NodeConfig,
    link: &crate::service::KernelLink,
    live: Option<&Value>,
) -> Option<String> {
    let Some(live) = live else {
        return Some("its instance is gone".to_string());
    };
    if live["devices"][crate::service::KERNEL_DEVICE] != crate::service::kernel_device(link) {
        return Some(format!(
            "its instance does not carry the device {} as Arca made it",
            crate::service::KERNEL_DEVICE
        ));
    }
    match cfg.kernel_access() {
        Some(path) if path == link.connect => None,
        _ => Some(format!(
            "this node's listen.services is no longer {}, so the device connects to nothing of Arca's; remove the service and create it again",
            link.connect
        )),
    }
}

/// Why `serve` does not start over this desired state: a service's record names kernel access through a socket this
/// config does not serve. The device of such a service connects to the path its record holds, and only the services
/// socket is known never to carry the approval page; a start that left the path to anything else is refused.
pub fn kernel_access_refusal(
    cfg: &NodeConfig,
    entries: &[arca_kernel::desired::Entry],
) -> Option<String> {
    let served = cfg.kernel_access();
    entries
        .iter()
        .filter(|entry| entry.plugin.0 == "arca.service")
        .find_map(|entry| {
            let access = entry.config.get("kernelAccess")?;
            let recorded = access["connect"].as_str().unwrap_or("an unreadable path");
            let name = entry.config["name"].as_str().unwrap_or(entry.id.0.as_str());
            match &served {
                Some(path) if path == recorded => None,
                Some(path) => Some(format!(
                    "the service {name} has access to this node's kernel through the socket {recorded}, and this config's listen.services is {path}: set listen.services back to {recorded}, or remove the service before changing it"
                )),
                None => Some(format!(
                    "the service {name} has access to this node's kernel through the socket {recorded}, and this config sets no listen.services: set listen.services to {recorded}, or remove the service before taking it out"
                )),
            }
        })
}

/// The refusal for a call whose `kernelAccess` is not its template's.
fn kernel_access_mismatch(template: &crate::service::Template) -> CallError {
    CallError::Invalid(match &template.kernel_access {
        Some(access) => format!(
            "kernelAccess: template {} has {{ \"port\": {} }}, and the call must state exactly that",
            template.name, access.port
        ),
        None => format!(
            "kernelAccess: template {} gives its service no access to the kernel",
            template.name
        ),
    })
}

/// The refusal for a create whose stated image or template content is not what the template on this node is: a call
/// that stated its own, or a card shown before the template was edited or installed again.
fn template_not_the_one_stated(name: &str) -> CallError {
    service_refused(format!(
        "the template {name} on this node is not the one this create names: it was changed or installed again since the card was shown, or the call states an image or a template content of its own. Nothing was created; propose it again"
    ))
}

/// The refusal for an approved create whose proposal was made before this node bound a template's content.
fn template_not_bound(name: &str) -> CallError {
    service_refused(format!(
        "this approval was given on a card that did not bind the content of the template {name}. Nothing was created; propose it again"
    ))
}

/// The words of a create's answer for a service created stopped: the call's own `start: false`, or the template's
/// reason, which names a secret and never shows one.
fn stopped_because(start: bool, template: Option<String>) -> String {
    if !start {
        return "the call said start: false".to_string();
    }
    template.unwrap_or_else(|| "its template's environment cannot be filled in yet".to_string())
}

/// Why a fixed MAC is refused on the managed service network.
const MANAGED_MAC: &str = "mac: a virtual machine on the managed service network has its address reserved by Arca, so it takes no fixed MAC; mac is for a host bridge";

/// The `service_create` handler among `handlers`, as [`postgres_handlers`] takes it.
pub fn service_create_handler(
    handlers: &[(&'static str, Arc<dyn ToolHandler>)],
) -> Option<Arc<dyn ToolHandler>> {
    handlers
        .iter()
        .find(|(verb, _)| *verb == "service_create")
        .map(|(_, handler)| Arc::clone(handler))
}

/// The database verbs. `service_create` is the node's own `service_create` handler ([`service_handlers`]): on a node
/// whose config names no `kernel.postgresImage`, `postgres_create` makes the service that handler makes from the
/// node's database template, through that handler and nothing else.
pub fn postgres_handlers(
    kernel: Arc<Mutex<Kernel>>,
    clock: Clock,
    cfg: Arc<NodeConfig>,
    service_create: Arc<dyn ToolHandler>,
) -> Vec<(&'static str, Arc<dyn ToolHandler>)> {
    // Serialize secret/document transactions without blocking other kernel users during I/O.
    // One gate for every handler that picks an address or a block: the kernel's own.
    let writes = write_gate(&kernel);
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
                service_create: service_create.clone(),
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
    service_create: Arc<dyn ToolHandler>,
}

/// Where a new database's image comes from. It is never the call's to choose.
enum DatabaseSource {
    /// The image the node's config names (`kernel.postgresImage`): a database of the old kind, made from an image
    /// Incus already holds under that fingerprint.
    Configured(String),
    /// The node's database template, with the image by digest it names now and the fingerprint of its content as
    /// this node holds it now: a service, fetched as every template's image is.
    Template {
        image: String,
        digest: String,
        /// The template itself, from that same read: what a card says of it is said of this one.
        template: Box<crate::service::Template>,
    },
}

/// The source of a database this node creates now: the image its config names, or else its database template. A call
/// or a proposal that states an image must state exactly that one, so no caller runs an image of its own choosing,
/// and a proposal made before the template was changed is refused. The refusal never repeats what the caller stated.
fn database_source(cfg: &NodeConfig, args: &Value) -> Result<DatabaseSource, CallError> {
    let template_name = crate::service::DATABASE_TEMPLATE;
    let stated = match args.get("image") {
        None => None,
        Some(image) => {
            let image = image
                .as_str()
                .filter(|image| image.len() <= 512)
                .ok_or_else(|| pg_invalid("image is required"))?;
            arca_postgres::fingerprint(image)
                .map_err(|_| pg_invalid("a digest-pinned image is required"))?;
            Some(image)
        }
    };
    if let Some(configured) = cfg.kernel.postgres_image.as_deref() {
        if args.get("template").is_some() {
            return Err(service_refused(
                "template: this node's config names kernel.postgresImage, so a database is made from that image and from no template",
            ));
        }
        if stated.is_some_and(|image| image != configured) {
            return Err(service_refused(
                "image: a call cannot choose an image. This node makes a database from the image its config names (kernel.postgresImage), and from no other",
            ));
        }
        return Ok(DatabaseSource::Configured(configured.to_string()));
    }
    if args
        .get("template")
        .is_some_and(|template| template.as_str() != Some(template_name))
    {
        return Err(service_refused(format!(
            "template: a database is made from this node's {template_name} template, and from no other"
        )));
    }
    // A database is a container on the managed service network: a node that runs no services refuses in those words.
    service_project(cfg)?;
    let content = crate::service::load_template_content(
        &cfg.state_dir.join("templates"),
        template_name,
    )
    .map_err(|error| {
            if error.not_found {
                service_refused(format!(
                    "this node has no {template_name} template (templates/{template_name}.json in its state directory), and its config names no kernel.postgresImage: a database is made from one of the two. `arca templates install` puts the template Arca ships there"
                ))
            } else {
                service_template_error(error)
            }
        })?;
    let template = &content.template;
    let pin = crate::service::template_pin(template)
        .filter(|_| template.kind == "oci")
        .ok_or_else(|| {
            service_refused(format!(
                "the {template_name} template on this node names no image by digest, so no database is made from it"
            ))
        })?;
    if stated.is_some_and(|image| image != pin) {
        return Err(database_image_changed(&pin));
    }
    // The content a proposal's card showed is the content now, or nothing is created: the same image with another
    // script, environment or port is another template.
    let digest = content.bound().map_err(service_template_error)?.to_string();
    if args
        .get("templateDigest")
        .is_some_and(|stated| stated.as_str() != Some(digest.as_str()))
    {
        return Err(template_not_the_one_stated(template_name));
    }
    Ok(DatabaseSource::Template {
        image: pin,
        digest,
        template: Box::new(content.template),
    })
}

/// The refusal for an image that is not the one the node's database template names now, `pin`.
fn database_image_changed(pin: &str) -> CallError {
    service_refused(format!(
        "image: a call cannot choose an image. This node makes a database from its {} template, which names {pin} now. A proposal that names another was made before the template changed: nothing was created, propose it again",
        crate::service::DATABASE_TEMPLATE
    ))
}

/// What the owner's card is told about a proposed database `name` made from `template`, the node's database template
/// as it was read for the proposal's fingerprint, and from the node's stored secrets now. In this order:
/// - why the service is created stopped, when it is (`template_stopped_reason`: its environment names a secret that
///   is neither stored for this name nor generated by the template);
/// - nothing, when a `POSTGRES_PASSWORD` is already stored for this name: the create uses the stored one and
///   generates none, so the card does not say "generates";
/// - that the template generates the password, when it does.
///
/// `None` also for a template that neither generates the password nor leaves the service stopped.
fn database_card_fact(
    cfg: &NodeConfig,
    template: &crate::service::Template,
    name: &str,
) -> Option<String> {
    let owner = crate::service::entry_id(None, name);
    let secrets = cfg.state_dir.join("secrets");
    if let Some(stopped) = crate::service::template_stopped_reason(template, &owner, &secrets) {
        return Some(stopped);
    }
    let stored = crate::service::secret_file(&secrets, &owner, "POSTGRES_PASSWORD")
        .is_some_and(|file| file.is_file());
    (!stored && template.generate.contains_key("POSTGRES_PASSWORD"))
        .then(|| crate::action_proposal::DATABASE_PASSWORD_GENERATED.to_string())
}

/// Refuses a database made from the template under a name a database of either kind, or a service, holds already.
fn database_name_free(
    entries: &[arca_kernel::desired::Entry],
    name: &str,
) -> Result<(), CallError> {
    if entries
        .iter()
        .any(|entry| entry.id.0 == format!("postgres-{name}"))
    {
        return Err(pg_invalid("database entry already exists"));
    }
    if entries
        .iter()
        .any(|entry| entry.id.0 == crate::service::entry_id(None, name))
    {
        return Err(service_refused(format!(
            "a service named {name} exists already: a database made from this node's {} template is a service, and takes a name no service holds",
            crate::service::DATABASE_TEMPLATE
        )));
    }
    Ok(())
}

/// A proposed `postgres_create` on a node whose config names no image: the proposal is given the node's database
/// template and the image it pins now, so the owner's card shows the image and the approval binds it. A create this
/// node is sure to refuse (no template, a name that is taken, an image the caller chose) is refused now, so the owner
/// is never asked to approve it. On a node whose config names the image the proposal is left as it is: `prepare`
/// fills that image in, as it always did.
fn enrich_postgres_create(
    cfg: &NodeConfig,
    entries: &[arca_kernel::desired::Entry],
    payload: &Value,
    card_fact: &mut Option<String>,
) -> Result<Value, CallError> {
    if cfg.kernel.postgres_image.is_some() || !payload.is_object() {
        return Ok(payload.clone());
    }
    let DatabaseSource::Template {
        image,
        digest,
        template,
    } = database_source(cfg, payload)?
    else {
        return Ok(payload.clone());
    };
    if let Some(name) = payload["name"].as_str() {
        database_name_free(entries, name)?;
        // What the card says of the password, from the template this fingerprint is of.
        *card_fact = database_card_fact(cfg, &template, name);
    }
    let mut payload = payload.clone();
    payload["template"] = json!(crate::service::DATABASE_TEMPLATE);
    payload["image"] = json!(image);
    payload["templateDigest"] = json!(digest);
    Ok(payload)
}

/// The words for a database verb that names a service: a database made from the template is a service.
fn database_is_service(verb: &str, name: &str) -> CallError {
    let instead = match verb {
        "postgres_delete" => "remove it with service_remove",
        _ => {
            "connect a service to it with service_connect, and take the link away with service_disconnect"
        }
    };
    service_refused(format!(
        "{name} is a service: a database made from this node's {} template is one. {verb} acts on a database of the old kind only; {instead}",
        crate::service::DATABASE_TEMPLATE
    ))
}

/// The services of the node's own project made from the database template, by name.
fn template_databases(
    entries: &[arca_kernel::desired::Entry],
) -> Vec<(arca_kernel::desired::Entry, crate::service::ServiceConfig)> {
    service_entries(entries)
        .into_iter()
        .filter(|(_, config)| {
            config.template == crate::service::DATABASE_TEMPLATE
                && config.tenant.is_none()
                && !config.adopted
        })
        .collect()
}

impl PostgresHandler {
    /// `postgres_create` on a node whose config names no image: the service `service_create` makes from the node's
    /// database template, made by that handler and by nothing else. `image` and `digest` are what the template named
    /// and was when this call was checked: the handler compares both with the template it reads for the create, so
    /// what was checked and what is created from are one read. The password is the template's to generate; from a
    /// template that generates none the service is created stopped, and the answer says why. The name must be free
    /// among databases of both kinds and among services. A token whose tool list leaves out `service_create` makes
    /// no service this way either.
    fn create_from_template(
        &self,
        args: &Value,
        caller: &Caller,
        image: &str,
        digest: &str,
    ) -> Result<Value, CallError> {
        if !caller.allows_tool("service_create") {
            return Err(service_refused(format!(
                "on this node a database is a service made from the {} template, so postgres_create needs service_create too, and this token's tool list does not have it",
                crate::service::DATABASE_TEMPLATE
            )));
        }
        let name = pg_name(args, "name", "postgres", 31)?;
        if !crate::service::service_name_ok(&name) {
            return Err(pg_invalid("invalid database or consumer name"));
        }
        database_name_free(lock(&self.kernel)?.entries(), &name)?;
        let made = self.service_create.call(
            json!({
                "name": name,
                "template": crate::service::DATABASE_TEMPLATE,
                "image": image,
                "templateDigest": digest
            }),
            caller,
        )?;
        let mut answer = json!({
            "name": name,
            "entry": crate::service::entry_id(None, &name),
            "status": "pending",
            "service": true
        });
        if let Some(why) = made.get("stoppedBecause") {
            answer["stoppedBecause"] = why.clone();
        }
        Ok(answer)
    }
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
        // A node that only looks makes no database, link or removal: refused before the write lock, in its one sentence.
        if self.verb != "postgres_list" && node_only_looks(&self.cfg) {
            return Err(only_looks());
        }
        let now = (self.clock)();
        // Inside a tenant a database is a service: the postgres service template, linked with service_connect. The
        // database verbs act in the node's own project only, and write nothing for a call that names a tenant.
        if args.get("tenant").is_some() {
            // The tenant is answered for first, as by every verb: no tenants, a malformed id, an unknown tenant.
            tenants::tenant_arg(&self.cfg, &args, lock(&self.kernel)?.entries())?;
            return Err(tenants::database_route(self.verb));
        }
        // The image is the node's, never the call's: the one its config names, or else its database template, whose
        // create is the service handler's own and takes the write gate itself.
        let configured_image = if self.verb == "postgres_create" {
            match database_source(&self.cfg, &args)? {
                DatabaseSource::Template { image, digest, .. } => {
                    return self.create_from_template(&args, caller, &image, &digest);
                }
                DatabaseSource::Configured(image) => Some(image),
            }
        } else {
            None
        };
        let _write = if self.verb == "postgres_list" {
            None
        } else {
            Some(self.writes.lock().map_err(|_| internal())?)
        };
        let entries = lock(&self.kernel)?.entries().to_vec();
        if self.verb == "postgres_list" {
            let read = UnixIncus::new(self.cfg.incus.socket.clone());
            let mut rows = Vec::new();
            for entry in entries.iter().filter(|e| e.plugin.0 == "arca.postgres") {
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
            // A database made from the node's template is a service. Its row is the same, and it alone carries
            // `service`, so a database of the old kind is shown as it always was.
            for (entry, config) in template_databases(&entries) {
                let state = match read.get(&format!(
                    "/1.0/instances/{}/state?project={}",
                    config.instance, config.project
                )) {
                    Ok(state) => state,
                    Err(arca_incus::IncusError::Status { code: 404, .. }) => Value::Null,
                    Err(_) => return Err(internal()),
                };
                let address = arca_postgres::ipv4(&state).map(|ip| ip.to_string());
                let status = if address.is_none() {
                    "pending".into()
                } else {
                    state["status"].as_str().unwrap_or("pending").to_lowercase()
                };
                let links: Vec<&str> = config
                    .incoming
                    .iter()
                    .map(|grant| grant.consumer.as_str())
                    .collect();
                rows.push(json!({"name":config.name,"entry":entry.id.0,"status":status,"address":address,"port":5432,"links":links,"service":true}));
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
            let image = configured_image.as_deref().ok_or_else(internal)?;
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
        // A database made from the template is a service, and the service verbs act on it.
        if !entries.iter().any(|entry| entry.id.0 == id)
            && template_databases(&entries)
                .iter()
                .any(|(_, config)| config.name == name)
        {
            return Err(database_is_service(self.verb, &name));
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

#[path = "function_handlers.rs"]
mod function_handlers;
#[path = "service_omni.rs"]
mod service_omni;
#[path = "service_wiring.rs"]
mod service_wiring;
#[path = "tenants.rs"]
mod tenants;
pub use tenants::{SnapshotListInTenant, spawn_front_sync, sync_front, tenant_handlers};

// Services: one record in the desired document, plus the live Incus row for state.

pub fn service_handlers(
    kernel: Arc<Mutex<Kernel>>,
    clock: Clock,
    cfg: Arc<NodeConfig>,
) -> Vec<(&'static str, Arc<dyn ToolHandler>)> {
    service_handlers_with(kernel, clock, cfg, None)
}

/// The function verbs. They are served only by a node that runs functions: one with a runner.
pub const FUNCTION_VERBS: [&str; 6] = [
    "function_create",
    "function_remove",
    "function_secret_set",
    "function_list",
    "function_run",
    "function_runs",
];

/// The service verbs and, over `functions`, the function verbs: one handler, one write gate and one record kind, since
/// a function is a service record with one more key.
pub fn service_handlers_with(
    kernel: Arc<Mutex<Kernel>>,
    clock: Clock,
    cfg: Arc<NodeConfig>,
    functions: Option<Arc<crate::function_run::FunctionRunner>>,
) -> Vec<(&'static str, Arc<dyn ToolHandler>)> {
    // One gate for every handler that picks an address or a block: the kernel's own.
    let writes = write_gate(&kernel);
    let exec = Arc::new(crate::service_exec::ServiceExec::new(cfg.clone()));
    let function_verbs: &[&'static str] = if functions.is_some() {
        &FUNCTION_VERBS
    } else {
        &[]
    };
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
        "service_import",
        "service_release",
    ]
    .into_iter()
    .chain(function_verbs.iter().copied())
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
                functions: functions.clone(),
            }) as Arc<dyn ToolHandler>,
        )
    })
    .chain(omni_handlers(
        kernel.clone(),
        clock.clone(),
        cfg.clone(),
        crate::omni::OmniOptions::default(),
    ))
    .collect()
}

/// The three Omni verbs over the service model (`crate::omni`). `options` is the default in a served node; a test
/// points it at a fake Omni.
pub fn omni_handlers(
    kernel: Arc<Mutex<Kernel>>,
    clock: Clock,
    cfg: Arc<NodeConfig>,
    options: crate::omni::OmniOptions,
) -> Vec<(&'static str, Arc<dyn ToolHandler>)> {
    let pairing = Arc::new(Mutex::new(HashMap::new()));
    ["omni_instance_create", "omni_pair", "omni_status"]
        .into_iter()
        .map(|verb| {
            (
                verb,
                Arc::new(service_omni::OmniHandler {
                    verb,
                    kernel: kernel.clone(),
                    clock: clock.clone(),
                    cfg: cfg.clone(),
                    options: options.clone(),
                    pairing: pairing.clone(),
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
    /// The runner of this node's functions, on a node that runs any. The function verbs need it.
    functions: Option<Arc<crate::function_run::FunctionRunner>>,
}

/// A refusal in plain words, under the plain code `refused`. A refusal for want of room is never this one, and neither
/// is one for asking too often: those are [`CallError::capacity`] and [`CallError::rate`], whose codes a caller can act
/// on without reading the sentence (docs/OPERATING.md, "Refusals for want of room and for asking too often").
fn service_refused(message: impl Into<String>) -> CallError {
    CallError::Refused {
        code: "refused".into(),
        message: message.into(),
    }
}

/// The code of a refusal for a read of Incus that got no answer: Incus did not answer by the deadline, could not be
/// reached, or answered with a server error. Nothing is wrong with the request, and the same request may be made again.
pub const INCUS_UNAVAILABLE: &str = "incus_unavailable";
/// The code of a refusal for a read of Incus whose answer could not be used: a status that is no server error, an
/// answer over the size this node reads, or one that is not the JSON it expects. Asking again is likely to answer the
/// same.
pub const INCUS_ANSWER: &str = "incus_answer";

/// What follows a read of Incus that got no answer.
const ASK_AGAIN: &str =
    "The same request may be made again (an approved action is proposed again).";
/// What follows a read of Incus whose answer could not be used.
const ASKED_THE_SAME: &str =
    "The same request is likely to be answered the same until Incus answers that read.";

/// What went wrong with a read of Incus, in words, and the code of its refusal. The words hold the kind of failure
/// and Incus's status, and never Incus's own text, which may quote a value.
fn incus_read_cause(error: &arca_incus::IncusError) -> (&'static str, String) {
    use arca_incus::IncusError;
    match error {
        IncusError::Timeout => (
            INCUS_UNAVAILABLE,
            format!(
                "Incus did not answer within {} seconds",
                arca_incus::read::TIMEOUT.as_secs()
            ),
        ),
        IncusError::Io(_) => (
            INCUS_UNAVAILABLE,
            "Incus could not be reached on its socket".to_string(),
        ),
        IncusError::Status { code, .. } if *code >= 500 => (
            INCUS_UNAVAILABLE,
            format!("Incus answered with its own error (status {code})"),
        ),
        IncusError::Status { code, .. } => {
            (INCUS_ANSWER, format!("Incus answered with status {code}"))
        }
        IncusError::TooLarge => (
            INCUS_ANSWER,
            format!(
                "Incus's answer was over the {} MiB this node reads",
                arca_incus::read::MAX_BODY >> 20
            ),
        ),
        _ => (
            INCUS_ANSWER,
            "Incus's answer was not the JSON this node expects".to_string(),
        ),
    }
}

/// The refusal for a read of Incus, inside a service call, that failed: in words, never "internal". `what` is what was
/// read and `subject` what it was read for (the service the call names, or the list); `changed` says what the call had
/// done by then, such as "Nothing was changed.". The words hold the kind of failure and Incus's status, and never
/// Incus's own text, which may quote a value.
fn incus_read_refused(
    error: &arca_incus::IncusError,
    what: &str,
    subject: &str,
    changed: &str,
) -> CallError {
    let (code, _) = incus_read_cause(error);
    let after = if code == INCUS_UNAVAILABLE {
        ASK_AGAIN
    } else {
        ASKED_THE_SAME
    };
    let mut tail = String::from(changed);
    if !tail.is_empty() {
        tail.push(' ');
    }
    tail.push_str(after);
    incus_read_told(error, what, subject, &tail)
}

/// The same refusal with its last sentences given whole, for a read made after the call changed something: there the
/// caller is told what is there and what to look at, and never to make the same request again.
fn incus_read_told(
    error: &arca_incus::IncusError,
    what: &str,
    subject: &str,
    tail: &str,
) -> CallError {
    let (code, cause) = incus_read_cause(error);
    CallError::Refused {
        code: code.into(),
        message: format!("{cause} when this node read {what} for {subject}. {tail}"),
    }
}

/// What a service call reads Incus for, as a refusal names it: the service or the function the call names, or the list.
fn read_subject(verb: &str, args: &Value) -> String {
    let kind = if verb.starts_with("function_") {
        "function"
    } else {
        "service"
    };
    ["name", "from"]
        .into_iter()
        .find_map(|key| args.get(key).and_then(Value::as_str))
        .filter(|name| crate::service::service_name_ok(name))
        .map(|name| format!("{kind} {name}"))
        .unwrap_or_else(|| format!("the list of {kind}s"))
}

/// What a service call had changed when its first reads of Incus are made: nothing, which a call that only reads does
/// not need said. A call that runs from an owner's approval has used that approval by then, and says so.
fn nothing_changed(verb: &str, approved: bool) -> &'static str {
    if matches!(
        verb,
        "service_list" | "service_status" | "service_logs" | "function_list" | "function_runs"
    ) {
        ""
    } else if approved {
        "Nothing was changed, and the approval was used."
    } else {
        "Nothing was changed."
    }
}

fn service_not_found(message: impl Into<String>) -> CallError {
    CallError::Refused {
        code: "not_found".into(),
        message: message.into(),
    }
}

/// What a service's view says under `update` ([`crate::service::update_view`]): its own version beside the one the
/// template installed on this node names. `templates` keeps each template this call has read, so a listing reads a
/// template's file once whatever the number of services made from it. No network is used and no registry is asked.
fn service_update(
    cfg: &NodeConfig,
    templates: &mut BTreeMap<
        String,
        Result<crate::service::Template, crate::service::TemplateError>,
    >,
    service: &crate::service::ServiceConfig,
) -> Option<Value> {
    // An adopted instance has no version of Arca's making, so its template is not read at all.
    if service.adopted {
        return None;
    }
    let template = templates
        .entry(service.template.clone())
        .or_insert_with(|| {
            crate::service::load_template(&cfg.state_dir.join("templates"), &service.template)
        });
    crate::service::update_view(service, template.as_ref())
}

fn service_template_error(error: crate::service::TemplateError) -> CallError {
    if error.not_found {
        service_not_found(error.message)
    } else {
        service_refused(error.message)
    }
}

/// Refuses a template whose image is a local alias that no image in the service project's image store carries, before
/// anything is created: such an image is built or copied in by hand, and Incus's own answer to the create names no reason.
/// Only a listing that answers and lacks the alias refuses. A listing that cannot be read (the project is not made yet, or
/// the read fails) decides nothing, and the create goes on.
///
/// A template that pins its local image (a `digest` beside `local:<alias>`) is held to it, a virtual machine's too. The
/// fingerprint decides which image an instance is made from, and the alias is only the name it is looked for under: the
/// image under the alias must be the pinned one. Here an unreadable listing refuses: a pin that cannot be checked is
/// not met.
fn local_image_present(
    read: &UnixIncus,
    template: &crate::service::Template,
    project: &str,
) -> Result<(), CallError> {
    let pin = template.local_image_pin();
    let alias = match pin {
        Some((alias, _)) => alias,
        None => {
            let Some(alias) = (template.kind == "system")
                .then(|| template.image.strip_prefix("local:"))
                .flatten()
            else {
                return Ok(());
            };
            alias
        }
    };
    let listing = read
        .get(&format!("/1.0/images?project={project}&recursion=1"))
        .ok()
        .and_then(|images| images.as_array().cloned());
    let Some(images) = listing else {
        let Some((_, fingerprint)) = pin else {
            return Ok(());
        };
        // The service project is made with the node's first service. Before that it has no images to hold the pinned
        // one, and the refusal says so and what comes first.
        let no_project = matches!(
            read.get(&format!("/1.0/projects/{project}")),
            Err(arca_incus::IncusError::Status { code: 404, .. })
        );
        return Err(service_refused(if no_project {
            format!(
                "template {}: its image {alias} is pinned to sha256:{fingerprint}, and the service project {project} does not exist yet: it is made with this node's first service, so no image can be in it. Create another service first, put the pinned image in the project under the alias {alias}, then create this one",
                template.name
            )
        } else {
            format!(
                "template {}: its image {alias} is pinned to sha256:{fingerprint}, and the images of the service project {project} cannot be read to check it. Nothing was created",
                template.name
            )
        }));
    };
    let held = images.iter().find(|image| {
        image["aliases"]
            .as_array()
            .is_some_and(|aliases| aliases.iter().any(|a| a["name"].as_str() == Some(alias)))
    });
    let Some(held) = held else {
        return Err(service_refused(format!(
            "template {}: its image {alias} is not on this node: no image in the service project {} has that alias. \
             A local image is built or copied into that project before the service is created; the template's description says how",
            template.name, project
        )));
    };
    if let Some((_, fingerprint)) = pin {
        let found = held["fingerprint"].as_str().unwrap_or("unknown");
        if found != fingerprint {
            return Err(service_refused(format!(
                "template {}: its image {alias} is pinned to sha256:{fingerprint}, and the image under that alias in the service project {project} is sha256:{found}. Nothing was created. Put the pinned image under the alias, or change the template's digest",
                template.name
            )));
        }
    }
    Ok(())
}

/// Whether the node's config asks for no service project at all: no service subnet, no host bridges, and the service
/// project not among the kernel's projects. Such a node was never set up to run a service, so a list of its services is
/// the empty list. A config that names any of the three and not the rest is a fault, and every call still refuses it.
pub(crate) fn no_service_project_asked(cfg: &NodeConfig) -> bool {
    let kernel = &cfg.kernel;
    kernel.service_cidr.is_none()
        && kernel.bridged.is_none()
        && !kernel
            .projects
            .iter()
            .any(|project| project == arca_netproject::PROJECT)
}

/// What a log answer of a node that only looks says about its lines.
pub const CONSOLE_LOG_NOTE: &str = "these lines are the instance's console log, not the service's journal: the journal is read by running a command inside the instance, and this node only looks";

/// What marks an instance of the service project that this node holds no record of.
pub const NOT_RECORDED_NOTE: &str = "not recorded by this node";

/// `view`, marked as an instance this node holds no record of.
fn not_recorded(mut view: Value) -> Value {
    view["note"] = json!(NOT_RECORDED_NOTE);
    view
}

/// The refusal of every call that would change something on a node that only looks: the one sentence every answer of
/// such a node carries, so a write is refused in the words the reads are marked with. It is made before a grant is
/// spent and before anything is written.
pub(crate) fn only_looks() -> CallError {
    service_refused(arca_mcp::jsonrpc::ONLY_LOOKS_NOTE)
}

/// Whether the node only looks: it has no kernel project, so it makes no change.
pub(crate) fn node_only_looks(cfg: &NodeConfig) -> bool {
    cfg.kernel.projects.is_empty()
}

/// The command that turns changes on.
const ENABLE_CHANGES: &str = "arca init --enable-changes";

/// The refusal, on a node that only looks, of a verb that answers by running a command inside a service. `does` says
/// what the verb would have done.
pub(crate) fn only_looks_refusal(verb: &str, does: &str) -> CallError {
    service_refused(format!(
        "{}. {verb} {does}",
        arca_mcp::jsonrpc::ONLY_LOOKS_NOTE
    ))
}

/// Whether the node's desired state holds a record of anything in the service project: a service, a database, a consumer,
/// or the project and network themselves. A node whose config no longer asks for the project may still hold them.
fn holds_service_records(entries: &[arca_kernel::desired::Entry]) -> bool {
    entries.iter().any(|entry| {
        matches!(
            entry.plugin.0.as_str(),
            "arca.service" | "arca.netproject" | "arca.postgres" | "arca.consumer"
        )
    })
}

/// The config keys that let a node run services. A kernel project also needs `kernel.droiddProjects`, which the config
/// check asks for by name.
fn service_keys() -> String {
    format!(
        "its config must set kernel.serviceCidr to an IPv4 /24 and list {} in kernel.projects (with kernel.droiddProjects), and the node be restarted",
        arca_netproject::PROJECT
    )
}

/// The refusal of a call that needs the service subnet on a node whose config names none: what the node is, and the
/// config keys that would let it run services.
fn no_service_subnet(cfg: &NodeConfig) -> CallError {
    let fix = service_keys();
    service_refused(if !no_service_project_asked(cfg) {
        format!(
            "no service project: the node's config does not set kernel.serviceCidr. To run services, {fix}"
        )
    } else if cfg.kernel.projects.is_empty() {
        format!(
            "no service project: this node only looks, and its config sets up no services. To let it run them: {ENABLE_CHANGES}"
        )
    } else {
        format!(
            "no service project: this node's config sets up no services. To let it run them, {fix}"
        )
    })
}

/// The refusal of every service call on a node whose config asks for no service project while its desired state still
/// holds what it ran: the list is not empty, and the node cannot serve it, so it says so.
fn service_records_without_config() -> CallError {
    service_refused(format!(
        "no service project: this node's config sets up no services, but the node still holds records of services it ran. To reach them again, {}",
        service_keys()
    ))
}

/// The service project's CIDR and gateway, or a refusal before any Incus read.
fn service_project(cfg: &NodeConfig) -> Result<(String, String), CallError> {
    let cidr = cfg
        .kernel
        .service_cidr
        .as_deref()
        .ok_or_else(|| no_service_subnet(cfg))?;
    let gateway = crate::service::gateway(cidr)
        .map_err(|_| service_refused("no service project: the service CIDR is invalid"))?;
    // A node with no kernel project only looks. Its service config is whole and its services may well be there; what it
    // lacks is the leave to change them, and the refusal says that, not that a project is missing.
    if cfg.kernel.projects.is_empty() {
        return Err(only_looks());
    }
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

/// A bridged node's service project, or a refusal before any Incus read. It has no CIDR and no gateway.
fn bridged_project(cfg: &NodeConfig) -> Result<(), CallError> {
    if cfg.kernel.projects.is_empty() {
        return Err(only_looks());
    }
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
    Ok(())
}

/// Why a verb that needs the managed service network refuses a virtual machine on a host bridge.
fn bridged_refusal(name: &str, what: &str) -> CallError {
    service_refused(format!(
        "service {name} is a virtual machine on a host bridge that Arca does not manage: {what}"
    ))
}

/// Why a verb refuses a virtual machine on the managed service network.
fn vm_refusal(name: &str, what: &str) -> CallError {
    service_refused(format!("service {name} is a virtual machine: {what}"))
}

/// How long a read of Incus that failed fast waits before its one second try.
const READ_AGAIN_AFTER: Duration = Duration::from_millis(200);
/// A failure that came back within this long is a fast one: Incus was not there, or said so at once.
const FAST_FAILURE: Duration = Duration::from_secs(1);

/// One read of Incus, made a second time when the first failed fast: the socket was not reached, or Incus answered
/// with a server error, within a second. "Fast" is the time that passed, not the kind of failure: an error that took
/// most of the deadline to come is not tried again, and neither is a read that got no answer by its deadline. Both
/// tries share the one deadline a read is given, since a write holds its gate while it reads. Only the read is made
/// again; it changes nothing, so the second is safe. The node's log gets a line for it.
fn read_again_once(read: &UnixIncus, path: &str) -> Result<Value, arca_incus::IncusError> {
    use arca_incus::IncusError;
    let began = Instant::now();
    let first = read.get(path);
    let Err(error @ (IncusError::Io(_) | IncusError::Status { code: 500.., .. })) = &first else {
        return first;
    };
    if began.elapsed() >= FAST_FAILURE {
        return first;
    }
    eprintln!(
        "arca: a read of Incus failed at once and is made once more: {}",
        incus_read_cause(error).1
    );
    thread::sleep(READ_AGAIN_AFTER);
    // The second try has what is left of this client's own deadline, so both together take no longer than one read.
    let left = read.timeout().saturating_sub(began.elapsed());
    read.get_within(path, left)
}

/// The instances of `project`, or of every project. `with_state` asks Incus for each one's live state too, which for a
/// running virtual machine it takes from the guest's agent and can take seconds for one machine: only a call that
/// picks an address or a MAC for a new instance asks for it, since a live address or interface is in no record. Every
/// other call reads the listing without state, which holds each instance's status, config and devices and is answered
/// at once. `subject` and `changed` are for the refusal when Incus does not answer ([`incus_read_refused`]).
fn service_listing(
    read: &UnixIncus,
    project: Option<&str>,
    with_state: bool,
    subject: &str,
    changed: &str,
) -> Result<Vec<Value>, CallError> {
    let recursion = if with_state { 2 } else { 1 };
    let path = project
        .map(|project| format!("/1.0/instances?project={project}&recursion={recursion}"))
        .unwrap_or_else(|| format!("/1.0/instances?all-projects=true&recursion={recursion}"));
    match read_again_once(read, &path) {
        Ok(rows) => Ok(rows.as_array().cloned().unwrap_or_default()),
        Err(arca_incus::IncusError::Status { code: 404, .. }) => Ok(Vec::new()),
        Err(error) => Err(incus_read_refused(
            &error,
            if with_state {
                "the instances and their live state"
            } else {
                "the instances"
            },
            subject,
            changed,
        )),
    }
}

fn service_leases(read: &UnixIncus, subject: &str) -> Result<Vec<String>, CallError> {
    let path = format!(
        "/1.0/networks/{}/leases?project={}",
        arca_netproject::BRIDGE,
        arca_netproject::PROJECT
    );
    match read_again_once(read, &path) {
        Ok(rows) => Ok(rows
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|lease| lease["address"].as_str().map(str::to_string))
            .collect()),
        Err(arca_incus::IncusError::Status { code: 404, .. }) => Ok(Vec::new()),
        Err(error) => Err(incus_read_refused(
            &error,
            "the addresses the service network has leased",
            subject,
            "Nothing was created.",
        )),
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

pub(crate) fn ipv4_in_cidr(ip: std::net::Ipv4Addr, cidr: &str) -> bool {
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
pub(crate) fn adopted_config(
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
    // A virtual machine is never adopted from its marks: its record alone says what it is.
    if row["type"].as_str() == Some(crate::service::VM_KIND) {
        return None;
    }
    // Neither is a function's instance: a function is made by its create, from the definition its owner approved.
    if row["config"][crate::service::FUNCTION_KEY].is_string() {
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
        vm: None,
        tenant: None,
        // The device is the instance's own, so a record made again from the instance says it has the access. Only an
        // instance of the node's own project is read for it, and `adopt_services` keeps it only for this node's socket.
        kernel_access: (project == arca_netproject::PROJECT)
            .then(|| crate::service::kernel_link_of(row))
            .flatten(),
        // An instance says nothing of a destination: only a create the owner approved records one.
        destinations: Vec::new(),
        function: None,
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

/// The subnet a marked instance's address must be inside, and its gateway. An instance `kernel.importServices` lists, by name
/// and project, outside the service project, is checked against that entry's cidr: it stands in another manager's subnet.
/// Every other instance is checked against the node's own service subnet, as before.
fn adoption_subnet(cfg: &NodeConfig, row: &Value, cidr: &str, gateway: &str) -> (String, String) {
    let listed = match (row["name"].as_str(), row["project"].as_str()) {
        (Some(name), Some(project)) if project != arca_netproject::PROJECT => {
            cfg.kernel.import_service(name, project)
        }
        _ => None,
    };
    match listed {
        Some(listed) => (
            listed.cidr.clone(),
            crate::service::gateway(&listed.cidr)
                .map(|gateway| gateway.to_string())
                .unwrap_or_default(),
        ),
        None => (cidr.to_string(), gateway.to_string()),
    }
}

/// Adds a record for every kernel-owned marked instance without one, and keeps the kernel access of a record made this
/// way the same as the instance's device. Nothing on the instance is read into a secret or re-rendered.
fn adopt_services(
    kernel: &Mutex<Kernel>,
    cfg: &NodeConfig,
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
        let (cidr, gateway) = adoption_subnet(cfg, row, cidr, gateway);
        let Some(mut config) = adopted_config(row, rows, &cidr, &gateway, now) else {
            continue;
        };
        // A kernel device is read back only when it connects to this node's own services socket. One made by hand for
        // any other path is not an access Arca gave: the record does not name it, so it never stands between this node
        // and its next start.
        config.kernel_access = config
            .kernel_access
            .filter(|link| cfg.kernel_access().as_deref() == Some(link.connect.as_str()));
        let id = format!("service-{}", config.name);
        if let Some(held) = entries
            .iter()
            .find(|entry| entry.id.0 == id && entry.plugin.0 == "arca.service")
        {
            // A record made from an instance follows the instance's device: taken off by hand, it leaves the record
            // at the next call, so the record never keeps `serve` from a start over a config without the socket.
            // A record Arca itself created is never changed here.
            let read = config
                .kernel_access
                .as_ref()
                .and_then(|link| serde_json::to_value(link).ok());
            if held.config["adopted"] == true && held.config.get("kernelAccess") != read.as_ref() {
                let mut record = held.config.clone();
                if let Some(object) = record.as_object_mut() {
                    match read {
                        Some(read) => object.insert("kernelAccess".to_string(), read),
                        None => object.remove("kernelAccess"),
                    };
                }
                changes.push(crate::desired_store::EntryChange::Put(service_entry(
                    &id,
                    "arca.service",
                    record,
                )));
            }
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

pub(crate) fn service_secret_values(dir: &std::path::Path) -> Vec<String> {
    let mut values = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return values;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // A registry's directory also holds the address its credential is for, which is no secret.
        let registry = entry.file_name().to_string_lossy().starts_with("registry-");
        if let Ok(files) = std::fs::read_dir(&path) {
            for file in files.flatten() {
                if registry && file.file_name() == "server" {
                    continue;
                }
                // Which stored values are hidden is decided by the rule that says which values are stored
                // ([`crate::service::secret_to_hide`]), so no secret the node accepts is left out of this list.
                if let Ok(text) = std::fs::read_to_string(file.path())
                    && let Some(hidden) = crate::service::secret_to_hide(&text)
                {
                    values.push(hidden.to_string());
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
    // A virtual machine on a host bridge has no ACL. One on the managed network has its own, as a container does.
    if config.on_host_bridge() {
        return service_refused(format!(
            "remove service {}: partial removal ({instance}); the owner's approval is spent, so another try needs a new one",
            config.name
        ));
    }
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
    let entrypoint = parts.content.entrypoint().map_err(service_template_error)?;
    let (environment, start) = crate::service::environment(
        parts.template,
        parts.owner,
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
        vm: None,
        tenant: None,
        kernel_access: parts.kernel_access,
        destinations: Vec::new(),
        function: None,
    })
}

/// Where a new virtual machine goes, with what only that place records.
enum VmPlace {
    /// A host bridge, and the fixed MAC the request gave, if any.
    Host { bridge: String, mac: Option<String> },
    /// The managed service network, with the address Arca's plan gave it.
    Managed { ipv4: String, gateway: String },
}

/// The record of a virtual machine. On a host bridge it has no address, gateway or port; on the managed service network
/// it has those of a container. Neither has an environment. Its template's first-boot script and settings are recorded
/// as cloud-init reads them. One whose template declares secrets is created stopped: `service_secret_set` stores them
/// and `instance_start` starts it and writes them into the guest, so they are never in its config. `settings` is what
/// the create asked to replace of the template's settings (`service::create_settings` has checked it): the machine
/// gets the merged object, and the record keeps the replaced values beside it.
fn vm_service_config(
    name: &str,
    content: &crate::service::TemplateContent,
    settings: Option<&Value>,
    place: VmPlace,
    pool: String,
    now: u64,
) -> Result<crate::service::ServiceConfig, CallError> {
    let template = &content.template;
    let source = crate::service::source(template).map_err(service_template_error)?;
    let root_size = template.root_size.clone().ok_or_else(internal)?;
    let user_data = content.first_boot().map_err(service_template_error)?;
    let settings =
        crate::service::create_settings(template, settings).map_err(service_template_error)?;
    let vendor_data = crate::service::vendor_data_of(&template.name, settings.merged.as_ref())
        .map_err(service_template_error)?;
    let (bridge, mac, ipv4, gateway, ports) = match place {
        VmPlace::Host { bridge, mac } => (bridge, mac, String::new(), String::new(), Vec::new()),
        VmPlace::Managed { ipv4, gateway } => (
            arca_netproject::BRIDGE.to_string(),
            None,
            ipv4,
            gateway,
            template.ports.clone(),
        ),
    };
    Ok(crate::service::ServiceConfig {
        name: name.to_string(),
        template: template.name.clone(),
        instance: name.to_string(),
        ipv4,
        ports,
        links: Vec::new(),
        incoming: Vec::new(),
        secret_refs: BTreeMap::new(),
        environment_patch: None,
        published: None,
        created: now,
        source,
        pool,
        project: arca_netproject::PROJECT.into(),
        bridge,
        gateway,
        entrypoint: None,
        environment: BTreeMap::new(),
        droidd_port: None,
        limits: template.limits.clone(),
        autostart: template.autostart,
        data: None,
        lan: None,
        start: template.secrets.is_empty(),
        adopted: false,
        creating: false,
        vm: Some(crate::service::Vm {
            root_size,
            mac,
            user_data,
            vendor_data,
            settings: settings.replaced,
        }),
        tenant: None,
        kernel_access: None,
        destinations: Vec::new(),
        function: None,
    })
}

struct ServiceParts<'a> {
    /// The id of the entry that will record the service: its secret files are kept under it.
    owner: &'a str,
    name: &'a str,
    template: &'a crate::service::Template,
    ipv4: String,
    gateway: String,
    pool: String,
    now: u64,
    secrets_dir: &'a std::path::Path,
    /// The template as it was read for this create: its scripts are the ones read with it.
    content: &'a crate::service::TemplateContent,
    /// The kernel access of a template that declares it, with the listener this node gives it.
    kernel_access: Option<crate::service::KernelLink>,
}

impl ServiceHandler {
    /// The address a new service takes: in a tenant, the lowest free address of the tenant's block; in the node's own
    /// project, the lowest free address of the plan that no tenant's block holds. Every instance, lease and record on
    /// the node counts as in use.
    fn free_address(
        &self,
        read: &UnixIncus,
        cidr: &str,
        rows: &[Value],
        entries: &[arca_kernel::desired::Entry],
        tenant: Option<&tenants::TenantScope>,
        subject: &str,
    ) -> Result<String, CallError> {
        let candidates = crate::service::candidates(cidr).map_err(service_template_error)?;
        let leases = service_leases(read, subject)?;
        let used = service_used(rows, &leases, entries);
        if let Some(scope) = tenant {
            return tenants::free_address(scope, &used);
        }
        let blocks = tenants::held_blocks(&self.cfg, entries);
        let candidates = if blocks.is_empty() {
            candidates
        } else {
            tenants::outside_blocks(candidates, &blocks)
        };
        let all = candidates.len() as u128;
        candidates
            .iter()
            .find(|ip| !used.contains(ip))
            .map(|ip| ip.to_string())
            .ok_or_else(|| {
                CallError::capacity(
                    format!("the service subnet {cidr} has no free address (.10-.239)"),
                    Room::of(Whose::Node, Limit::Addresses, all, 1, all),
                )
            })
    }

    /// Makes a new record a tenant's: its project, its tenant, and the front address the node names, if any.
    fn place_in_tenant(
        &self,
        config: &mut crate::service::ServiceConfig,
        tenant: Option<&tenants::TenantScope>,
        entries: &[arca_kernel::desired::Entry],
        deploy: Option<String>,
    ) {
        if let Some(scope) = tenant {
            config.project = scope.project.clone();
            config.instance = crate::service::instance_name(Some(&scope.id), &config.name);
            config.tenant = Some(crate::service::TenantRef {
                id: scope.id.clone(),
                block: scope.config.block.clone(),
                // The front admits ordinary services only: one created in a deploy waits for its confirm.
                front: match deploy {
                    Some(_) => None,
                    None => tenants::front_address(&self.cfg, entries),
                },
                deploy,
            });
        }
    }

    /// Proposes a create's changes and answers the new service's view. A create that left no instance is undone: its
    /// record goes and the secret files it made (`made`) are removed.
    #[allow(clippy::too_many_arguments)]
    fn settle_create(
        &self,
        read: &UnixIncus,
        name: &str,
        template_name: &str,
        changes: Vec<crate::desired_store::EntryChange>,
        config: &crate::service::ServiceConfig,
        made: Option<(&std::path::Path, &BTreeSet<std::ffi::OsString>)>,
        now: u64,
    ) -> Result<Value, CallError> {
        let id = config.entry_id();
        let outcomes = {
            let mut kernel = lock(&self.kernel)?;
            kernel.propose(changes, now).map_err(|_| internal())?
        };
        let failed = service_wiring::failed_for(&outcomes, &[id.as_str()]);
        if failed {
            match read.get(&format!(
                "/1.0/instances/{}?project={}",
                config.instance, config.project
            )) {
                Ok(found) => {
                    // The instance exists, so its record stays: the effect journal holds the create as applied, never
                    // as an open intent, and the owner is told what is there and what to do next.
                    let why = service_wiring::failure_reason(&outcomes, &[id.as_str()])
                        .unwrap_or_else(|| "the kernel recorded no reason".to_string());
                    let why = crate::service::redact_log(
                        &why,
                        &service_secret_values(&self.cfg.state_dir.join("secrets")),
                    );
                    // Only a state that was read is stated.
                    let left = match found["status"].as_str() {
                        Some("Stopped") => {
                            "Its instance exists and is stopped: instance_start starts it once the cause is fixed, and service_remove removes it".to_string()
                        }
                        Some(status) if status.len() <= 32 && status.is_ascii() => format!(
                            "Its instance exists, and Incus reports it as {status}: service_status shows it, and service_remove removes it"
                        ),
                        _ => "Its instance exists, in a state Incus did not report: service_status shows it, and service_remove removes it".to_string(),
                    };
                    // A function is said as one, with the verbs that act on one.
                    if config.function.is_some() {
                        return Err(service_refused(format!(
                            "create function {name}: {why}. Its instance exists: function_list shows the function, function_remove removes it, and the create is proposed again once the cause is fixed"
                        )));
                    }
                    return Err(service_refused(format!(
                        "start service {name}: {why}. {left}"
                    )));
                }
                Err(arca_incus::IncusError::Status { code: 404, .. }) => {}
                Err(error) => {
                    // Said as what it is, with the verbs that act on one: a function is not a service.
                    let (subject, look) = if config.function.is_some() {
                        (
                            format!("function {name}"),
                            "function_list shows what is there, and function_remove removes it",
                        )
                    } else {
                        (
                            format!("service {name}"),
                            "service_status shows what is there, and service_remove removes it",
                        )
                    };
                    return Err(incus_read_told(
                        &error,
                        "the instance of a create that did not complete",
                        &subject,
                        &format!(
                            "Whether its instance exists is not known: {look}. Look before the create is made again."
                        ),
                    ));
                }
            }
            let removal = {
                let mut kernel = lock(&self.kernel)?;
                kernel.propose(
                    vec![crate::desired_store::EntryChange::Remove(EntryId(
                        id.clone(),
                    ))],
                    now,
                )
            };
            if let Some((service_secrets, seeded)) = made {
                cleanup_service_secrets(service_secrets, seeded);
            }
            // Incus's own reason for refusing the create is part of this text: it is redacted as a start failure's is.
            let why = service_wiring::failure_reason(&outcomes, &[id.as_str()])
                .unwrap_or_else(|| "the kernel recorded no reason".to_string());
            let why = crate::service::redact_log(
                &why,
                &service_secret_values(&self.cfg.state_dir.join("secrets")),
            );
            let left = service_wiring::left_behind(removal.ok().as_deref(), &id);
            if config.function.is_some() {
                return Err(service_refused(format!(
                    "create function {name}: {why}; {left}"
                )));
            }
            return Err(service_refused(format!(
                "create service {name} from template {template_name}: {why}; {left}"
            )));
        }
        let live = match read.get(&format!(
            "/1.0/instances/{}/state?project={}",
            config.instance, config.project
        )) {
            Ok(state) => Some(state),
            Err(arca_incus::IncusError::Status { code: 404, .. }) => None,
            Err(error) => {
                // The create went through: the caller is told to look, never to make it again.
                let (subject, look) = if config.function.is_some() {
                    (
                        format!("function {name}"),
                        "The function was created: function_list shows it",
                    )
                } else {
                    (
                        format!("service {name}"),
                        "The service was created: service_status shows it",
                    )
                };
                return Err(incus_read_told(
                    &error,
                    "the state of the new instance",
                    &subject,
                    &format!("{look}. Do not make the create again."),
                ));
            }
        };
        let live = live.map(|state| {
            let mut row = state;
            if row.get("status").is_none() {
                row["status"] = json!("Unknown");
            }
            row
        });
        Ok(crate::service::view(config, live.as_ref()))
    }
}

/// `service_import` and `service_release` run only with the one-shot grant of an approved proposal.
fn import_not_approved() -> CallError {
    CallError::Refused {
        code: "NotApproved".into(),
        message: "this action runs only from an approved proposal: propose it with action_propose, and it runs once the owner approves it".into(),
    }
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
            "service_create" => &[
                "name",
                "template",
                "registry",
                "bridge",
                "mac",
                "deploy",
                "start",
                "kernelAccess",
                "destinations",
                "settings",
                // Only an approved proposal carries it: the one-shot grant of its approval.
                "approval",
                // What a proposal is given when it is made: the image the template names and the fingerprint of
                // the template's content. A call that states either is compared with the template this create
                // reads, and refused when they differ: they can only refuse, never widen.
                "image",
                "templateDigest",
            ],
            "service_status" => &["name"],
            "service_logs" => &["name", "lines"],
            "service_remove" | "service_import" | "service_release" => &["name", "approval"],
            "service_connect" | "service_disconnect" => &["from", "to"],
            "service_secret_set" => &["name", "key", "value"],
            "service_publish" => &["name", "reach", "approval"],
            "service_unpublish" => &["name"],
            "function_create" => &[
                "name",
                "kind",
                "image",
                "digest",
                "command",
                "env",
                "secrets",
                "links",
                "destinations",
                "triggers",
                "limits",
                "concurrency",
                "registry",
                // Only an approved proposal carries it: the one-shot grant of its approval.
                "approval",
            ],
            "function_remove" => &["name", "approval"],
            "function_secret_set" => &["name", "key", "value"],
            "function_run" => &["name"],
            "function_runs" => &["name", "limit", "run"],
            _ => &[],
        };
        let object = args
            .as_object()
            .ok_or_else(|| CallError::Invalid("arguments: expected an object".into()))?;
        // A publication, an import and a release act on the node and take no tenant: the field is not theirs.
        let takes_tenant = !matches!(
            self.verb,
            "service_publish" | "service_unpublish" | "service_import" | "service_release"
        ) && !self.verb.starts_with("function_");
        if let Some(key) = object
            .keys()
            .find(|key| !(takes_tenant && *key == "tenant") && !allowed.contains(&key.as_str()))
        {
            let mut known = allowed.to_vec();
            if takes_tenant {
                known.push("tenant");
            }
            return Err(CallError::Invalid(arca_api::unknown_field("", key, &known)));
        }
        if matches!(
            self.verb,
            "service_create" | "service_import" | "service_release"
        ) && caller.scope < Scope::Doctor
        {
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
                | "function_create"
                | "function_remove"
                | "function_secret_set"
                | "function_run"
        ) && caller.scope < Scope::Doctor
        {
            return Err(CallError::Forbidden {
                verb: self.verb.into(),
            });
        }
        // A node that only looks refuses every service call that is not one of the three reads, a tenant's included,
        // before a grant is spent and before anything is written: a grant spent here could not be used once changes
        // are turned on, and a tenant's write would be saved now and applied then.
        if node_only_looks(&self.cfg)
            && !matches!(
                self.verb,
                "service_list" | "service_status" | "service_logs"
            )
        {
            return Err(only_looks());
        }
        let preapproved = matches!(
            self.verb,
            "service_create"
                | "service_remove"
                | "service_publish"
                | "service_import"
                | "service_release"
                | "function_create"
                | "function_remove"
        ) && args
            .get("approval")
            .and_then(Value::as_str)
            .is_some_and(|grant| {
                lock(&self.kernel)
                    .map(|mut kernel| kernel.consume_action_grant(self.verb, &args, grant))
                    .unwrap_or(false)
            });
        let _publish = matches!(
            self.verb,
            "service_publish" | "service_unpublish" | "service_remove"
        )
        .then(|| self.exec.publish_gate.blocking_lock());
        let _write = if matches!(
            self.verb,
            "service_create"
                | "service_connect"
                | "service_disconnect"
                | "service_secret_set"
                | "service_import"
                | "service_release"
                | "function_create"
                | "function_secret_set"
        ) {
            Some(self.writes.lock().map_err(|_| internal())?)
        } else {
            None
        };
        let now = (self.clock)();
        // A bridged node has no service /24: nothing on it is adopted from an address, and no gateway exists. A node with
        // the managed network has both, whether or not it also allows host bridges.
        let bridged = self.cfg.kernel.bridged.as_ref();
        let managed = self.cfg.kernel.service_cidr.is_some();
        // A call that names a tenant acts in that tenant's project and sees nothing outside it.
        let tenant = match args.get("tenant") {
            None => None,
            Some(_) => tenants::tenant_arg(&self.cfg, &args, lock(&self.kernel)?.entries())?,
        };
        if tenant.is_none() && args.get("deploy").is_some() {
            return Err(CallError::Invalid(DEPLOY_OUTSIDE_TENANT.into()));
        }
        // A node whose config asks for no service project, and whose desired state holds nothing of one, has no service:
        // the list is empty, and Incus is not asked. A node that still holds records of what it ran has not, so no call
        // answers for it as if it had none. Every other verb, and every half-made service config, still refuses below.
        //
        // A node that only looks (no kernel project: a fresh install, or one set back with `arca init --watch-only`) still
        // answers the three reads whenever there is something to read: a service config, or records of services it ran.
        // They read the node's own records and Incus, as they do on a node that makes changes, and nothing else: no
        // instance is adopted and no command is run inside one. So a person can always see what is there.
        let only_looks = node_only_looks(&self.cfg);
        let reading = matches!(
            self.verb,
            "service_list" | "service_status" | "service_logs"
        );
        let mut reads_as_it_is = only_looks && reading && tenant.is_none();
        if tenant.is_none() && no_service_project_asked(&self.cfg) {
            let records = holds_service_records(lock(&self.kernel)?.entries());
            if records && !reads_as_it_is {
                return Err(service_records_without_config());
            }
            if !records {
                if self.verb == "service_list" {
                    return Ok(json!({ "services": [] }));
                }
                // Nothing to read and nothing set up: the refusal below says what the config needs.
                reads_as_it_is = false;
            }
        }
        let (cidr, gateway) = match bridged {
            _ if tenant.is_some() => tenants::network(&self.cfg)?,
            // The reads use neither the subnet nor its gateway.
            _ if reads_as_it_is => (String::new(), String::new()),
            Some(_) if !managed => {
                bridged_project(&self.cfg)?;
                (String::new(), String::new())
            }
            _ => service_project(&self.cfg)?,
        };
        let read = UnixIncus::new(self.cfg.incus.socket.clone());
        let subject = read_subject(self.verb, &args);
        // Only a create needs an instance's live state: the addresses and the interfaces that are in no record.
        let picks_an_address = matches!(self.verb, "service_create" | "function_create");
        let rows = service_listing(
            &read,
            None,
            picks_an_address,
            &subject,
            nothing_changed(self.verb, preapproved),
        )?;
        let mut entries = {
            let kernel = lock(&self.kernel)?;
            kernel.entries().to_vec()
        };
        // Adopting writes a record into the desired state, which a node that only looks never does: an instance it holds
        // no record of is shown as it is, and marked.
        if managed && tenant.is_none() && !only_looks {
            adopt_services(
                &self.kernel,
                &self.cfg,
                &entries,
                &rows,
                &cidr,
                &gateway,
                now,
            )?;
            entries = {
                let kernel = lock(&self.kernel)?;
                kernel.entries().to_vec()
            };
        }
        // Every instance and every record on the node: the address plan and the node's caps count them all. The call
        // itself sees its own side only. On a node that holds no tenant the two are the same.
        let all_rows = rows;
        let all_entries = entries;
        let has_tenants = all_entries
            .iter()
            .any(|entry| entry.plugin.0 == "arca.tenant");
        let (rows, entries) = if has_tenants || tenant.is_some() {
            (
                tenants::scoped_rows(&all_rows, tenant.as_ref()),
                tenants::scoped_entries(&all_entries, tenant.as_ref()),
            )
        } else {
            (all_rows.clone(), all_entries.clone())
        };
        let project = tenant
            .as_ref()
            .map_or(arca_netproject::PROJECT, |scope| scope.project.as_str());
        let owned = |row: &Value| match &tenant {
            Some(scope) => row["project"].as_str() == Some(scope.project.as_str()),
            None => service_owned(row),
        };
        let tenant_id = tenant.as_ref().map(|scope| scope.id.as_str());
        // One name space, two kinds: a verb of one kind never acts on a record of the other.
        if let Some(refusal) = function_handlers::kind_refusal(self.verb, &args, &all_entries) {
            return Err(refusal);
        }
        if matches!(
            self.verb,
            "service_connect"
                | "service_disconnect"
                | "service_secret_set"
                | "service_publish"
                | "service_unpublish"
                | "function_secret_set"
        ) {
            return self.call_wiring(&args, caller, &entries, now, preapproved);
        }
        if self.verb.starts_with("function_") {
            // A create and a secret hold the write gate from above. A removal takes it itself, once the run that is
            // going has been stopped; a run and the two reads take none.
            return self.call_function(
                &args,
                &read,
                &all_entries,
                &all_rows,
                &cidr,
                &gateway,
                now,
                preapproved,
            );
        }
        match self.verb {
            "service_list" => {
                let mut views = Vec::new();
                let records = service_entries(&entries);
                let names: BTreeSet<String> = records
                    .iter()
                    .map(|(_, config)| config.instance.clone())
                    .collect();
                let mut templates = BTreeMap::new();
                for (_, config) in &records {
                    // A function is not a service: function_list shows it. Its instance's name stays known, so the
                    // rows below never show it as an instance nobody recorded.
                    if config.function.is_some() {
                        continue;
                    }
                    let live = service_live_in(&rows, &config.project, &config.instance);
                    let mut view = crate::service::view(config, live);
                    // A machine on a host bridge takes its address from that network: it is in no record, and the
                    // list reads no live state. Said so, so an empty address is not read as "has none".
                    if config.on_host_bridge() {
                        view["ipv4In"] = json!("state");
                    }
                    // Only a service whose installed template names another version carries the key, so every
                    // other row is the one it always was.
                    if service_update(&self.cfg, &mut templates, config)
                        .is_some_and(|update| update["state"] == "differs")
                    {
                        view["update"] = json!({ "state": "differs" });
                    }
                    views.push(view);
                }
                for row in &rows {
                    if let Some(name) = row["name"].as_str()
                        && !names.contains(name)
                        && owned(row)
                    {
                        let view = if reads_as_it_is {
                            // Not judged a failed create, and not adopted: only shown, and said to be unrecorded.
                            (row["project"].as_str() == Some(project))
                                .then(|| crate::service::unknown_view(row))
                                .flatten()
                                .map(not_recorded)
                        } else if row["config"][crate::service::TEMPLATE_KEY].is_string() {
                            crate::service::failed_view(row)
                        } else if row["project"].as_str() == Some(project) {
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
                    let live = service_live_in(&rows, &config.project, &config.instance);
                    let mut view = crate::service::view(&config, live);
                    // A machine on a host bridge has its address only in its live state, so this one call reads the
                    // state of this one instance, when it runs. A service on the service network has its address
                    // in its record, and nothing is read for it. A read that fails still answers: the address is
                    // left empty and the answer says why.
                    if config.on_host_bridge()
                        && let Some(row) = live
                        && row["status"].as_str() == Some("Running")
                    {
                        let path = format!(
                            "/1.0/instances/{}/state?project={}",
                            config.instance, config.project
                        );
                        match read_again_once(&read, &path) {
                            Ok(state) => {
                                if let Some(ipv4) = crate::service::bridged_ipv4(row, &state) {
                                    view["ipv4"] = json!(ipv4.to_string());
                                }
                            }
                            Err(error) => {
                                let (_, cause) = incus_read_cause(&error);
                                view["ipv4Problem"] = json!(format!(
                                    "{cause} when this node read the live state of service {name}, which is where the address of a machine on a host bridge is: the address was not read. Every other field is as usual; ask again for the address"
                                ));
                            }
                        }
                    }
                    // The image the instance was made from, as Incus recorded it: a template that names an image
                    // by an alias does not say which build that was.
                    if let Some(image) = live.and_then(crate::service::base_image) {
                        view["baseImage"] = json!(image);
                    }
                    // The settings the machine was created with, as its guest reads them. They never hold a secret:
                    // a template's secrets are named apart and are never part of this object.
                    if let Some(settings) = config.vm.as_ref().and_then(|vm| vm.guest_settings()) {
                        view["settings"] = settings;
                    }
                    // The version it was created with beside the one its installed template names now.
                    if let Some(update) = service_update(&self.cfg, &mut BTreeMap::new(), &config) {
                        view["update"] = update;
                    }
                    // Whether the access its record names is the one that stands: the device on the instance, and
                    // the listener of this server.
                    if let Some(link) = &config.kernel_access {
                        let problem = kernel_access_problem(&self.cfg, link, live);
                        view["kernelAccess"]["ready"] = json!(problem.is_none());
                        if let Some(problem) = problem {
                            view["kernelAccess"]["problem"] = json!(problem);
                        }
                    }
                    // Whether each private destination its record names is open as recorded.
                    if !config.destinations.is_empty() {
                        for (index, problem) in
                            destination_problems(&config, &read).into_iter().enumerate()
                        {
                            view["destinations"][index]["ready"] = json!(problem.is_none());
                            if let Some(problem) = problem {
                                view["destinations"][index]["problem"] = json!(problem);
                            }
                        }
                    }
                    return Ok(view);
                }
                if let Some(row) = rows.iter().find(|row| {
                    row["name"].as_str()
                        == Some(crate::service::instance_name(tenant_id, &name).as_str())
                        && owned(row)
                }) {
                    if reads_as_it_is {
                        if let Some(view) = crate::service::unknown_view(row) {
                            return Ok(not_recorded(view));
                        }
                    } else if let Some(view) = crate::service::failed_view(row) {
                        return Ok(view);
                    } else if let Some(view) = crate::service::unknown_view(row) {
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
                let row =
                    service_live_in(&rows, &config.project, &config.instance).ok_or_else(|| {
                        service_refused(format!("service {name}: its instance is gone"))
                    })?;
                let path = format!(
                    "/1.0/instances/{}/console?project={}&type=log",
                    config.instance, config.project
                );
                // A virtual machine's log is its console: an exec needs a guest agent the image may not run.
                // A node that only looks runs no command inside an instance, so it reads the console log, and says so.
                let journal = if !reads_as_it_is
                    && !config.adopted
                    && config.vm.is_none()
                    && config.tenant.is_none()
                    && config.source["protocol"] != "oci"
                {
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
                let mut answer = json!({ "lines": &lines_out[start..] });
                if reads_as_it_is {
                    answer["note"] = json!(CONSOLE_LOG_NOTE);
                }
                Ok(answer)
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
                // A service a function links to stays until the function is gone: said here, on the entries this call
                // read, before the write gate is waited for. The kernel checks again under its own lock.
                // A removal that runs from an approved card has used that approval up by now, and says so.
                if tenant.is_none()
                    && let Some(refusal) =
                        function_handlers::linked_service_refusal(&all_entries, &name, preapproved)
                {
                    return Err(refusal);
                }
                let id = entry.id.clone();
                let spent = if preapproved {
                    lock(&self.kernel)?.spend_service_removal_authorized(&id)
                } else {
                    // A removal in a tenant names the tenant in what the owner signs.
                    let (target, signed) = match tenant_id {
                        Some(tenant) => (
                            format!("tenant.{tenant}.{name}"),
                            json!({"name": name, "tenant": tenant}),
                        ),
                        None => (name.clone(), json!({"name": name})),
                    };
                    lock(&self.kernel)?
                        .spend_service_removal(&id, &target, &signed, approval, now)
                        .map_err(|error| match error {
                            crate::kernel::ServiceRemovalError::Approval(refusal) => {
                                CallError::Refused {
                                    code: "owner_approval".into(),
                                    message: refusal.to_string(),
                                }
                            }
                            _ => internal(),
                        })?
                };
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
                        // A function came to link to it after the check above: the kernel refused under its own
                        // lock, and the approval this removal ran on is spent.
                        crate::kernel::ServiceRemovalError::LinkedFunctions(functions) => {
                            function_handlers::linked_functions_refusal(&name, &functions, true)
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
                // An image this service was fetched from with a registry's credential goes with its last user.
                if tenant.is_none() {
                    self.drop_fetched_images(&read, &config);
                }
                Ok(json!({ "removed": true }))
            }
            "service_import" => {
                let name = service_name(&args)?;
                if !preapproved {
                    return Err(import_not_approved());
                }
                let done = crate::service_import::import(&self.cfg, &name, &entries, now)?;
                // The mark is on the instance now. Its record is made from it, exactly as a restart reads it.
                // The import landed and its approval is used: the caller is told to look, never to ask for it again.
                let rows = match read_again_once(
                    &read,
                    "/1.0/instances?all-projects=true&recursion=1",
                ) {
                    Ok(rows) => rows.as_array().cloned().unwrap_or_default(),
                    Err(arca_incus::IncusError::Status { code: 404, .. }) => Vec::new(),
                    Err(error) => {
                        return Err(incus_read_told(
                            &error,
                            "the instances",
                            &subject,
                            "The instance carries Arca's mark already, and its record is made by the next service call that Incus answers: service_list shows it. Do not propose the import again.",
                        ));
                    }
                };
                adopt_services(
                    &self.kernel,
                    &self.cfg,
                    &entries,
                    &rows,
                    &cidr,
                    &gateway,
                    now,
                )?;
                let entries = lock(&self.kernel)?.entries().to_vec();
                let Some((_, config)) = service_entries(&entries)
                    .into_iter()
                    .find(|(_, config)| config.name == name && config.adopted)
                else {
                    return Err(service_refused(format!(
                        "import service {name}: the instance is marked, but its keys no longer read as a service record; service_list shows it as failed, and service_release takes the mark off"
                    )));
                };
                Ok(json!({
                    "imported": true,
                    "preImage": done.pre_image,
                    "service": crate::service::view(
                        &config,
                        service_live_in(&rows, &config.project, &config.instance),
                    ),
                }))
            }
            "service_release" => {
                let name = service_name(&args)?;
                if !preapproved {
                    return Err(import_not_approved());
                }
                let record = service_entries(&entries)
                    .into_iter()
                    .find(|(_, config)| config.name == name);
                let Some((entry, config)) = record else {
                    return Err(service_not_found(format!("service {name}: not found")));
                };
                if !config.adopted {
                    return Err(service_refused(format!(
                        "release service {name}: Arca created this service, it did not import it; service_remove removes it. Nothing was written"
                    )));
                }
                // The key goes first: while the instance carries it, the next call would make the record again.
                let done = crate::service_import::release(&self.cfg, &name, &config.project)?;
                let outcomes = lock(&self.kernel)?
                    .propose(
                        vec![crate::desired_store::EntryChange::Remove(entry.id.clone())],
                        now,
                    )
                    .map_err(|_| internal())?;
                if service_wiring::failed_for(&outcomes, &[entry.id.0.as_str()]) {
                    return Err(service_refused(format!(
                        "release service {name}: the instance is released, but Arca's record of it could not be removed"
                    )));
                }
                Ok(json!({
                    "released": true,
                    "restored": done.restored,
                    "preImage": done.image,
                }))
            }
            "service_create" => {
                let name = service_name(&args)?;
                let template_name = args["template"]
                    .as_str()
                    .filter(|name| crate::service::template_name_ok(name))
                    .ok_or_else(|| {
                        CallError::Invalid("template: expected a template name".into())
                    })?;
                // `start: false` creates the service stopped: its secrets and links are set before its first start.
                let start = match args.get("start") {
                    None => true,
                    Some(value) => value.as_bool().ok_or_else(|| {
                        CallError::Invalid("start: expected true or false".into())
                    })?,
                };
                let id = crate::service::entry_id(tenant_id, &name);
                if entries
                    .iter()
                    .any(|entry| entry.id.0 == id && entry.plugin.0 == "arca.service")
                {
                    return Err(service_exists(&name));
                }
                // The instance name is one name on the node, whichever side holds it: asked of every record.
                if instance_name_taken(&all_entries, tenant_id, &name) {
                    return Err(name_taken(&name));
                }
                let instance = crate::service::instance_name(tenant_id, &name);
                if rows
                    .iter()
                    .any(|row| row["name"].as_str() == Some(instance.as_str()))
                {
                    return Err(service_refused(format!(
                        "an instance named {name} already exists that is not a service of Arca's; it is left alone"
                    )));
                }
                let templates_dir = self.cfg.state_dir.join("templates");
                // The template's file and the scripts it names are read once, here. What the call states of them
                // is compared with this read, and the service is made from this read and no other.
                let content = crate::service::load_template_content(&templates_dir, template_name)
                    .map_err(service_template_error)?;
                let template = content.template.clone();
                // Read before the create makes any secret: why this template is created stopped whatever `start` says.
                let forced = crate::service::template_stopped_reason(
                    &template,
                    &id,
                    &self.cfg.state_dir.join("secrets"),
                );
                // A template that gives its service access to the kernel: refused in a tenant, on a node with no
                // socket for it, and unless the create runs from an approved proposal that states it. All of it comes
                // before any write.
                let kernel_access = kernel_access_for(
                    &self.cfg,
                    &template,
                    tenant.is_some(),
                    Some(KernelAccessCall {
                        stated: args.get("kernelAccess"),
                        approved: preapproved,
                    }),
                )?
                .map(|(port, connect)| crate::service::KernelLink { port, connect });
                // What the call states of the template is compared with this read: the content its card showed
                // (an approved proposal always names it) and the image. A template that is no longer that one
                // creates nothing. It comes after the kernel access above, whose own refusal says more, and
                // before any write.
                match args.get("templateDigest") {
                    Some(stated) if stated.as_str() != content.digest.as_deref() => {
                        return Err(template_not_the_one_stated(template_name));
                    }
                    None if preapproved => return Err(template_not_bound(template_name)),
                    _ => {}
                }
                if args.get("image").is_some_and(|stated| {
                    stated.as_str() != Some(crate::service::template_image(&template).as_str())
                }) {
                    return Err(template_not_the_one_stated(template_name));
                }
                // A service the node's config gives a private destination: refused unless the create runs from an
                // approved proposal that states exactly what the config names now, and when an address is one of
                // the node's own. All of it comes before any write.
                let destinations = destinations_for(
                    &self.cfg,
                    &name,
                    tenant.is_some(),
                    Some(DestinationsCall {
                        stated: args.get("destinations"),
                        approved: preapproved,
                    }),
                )?;
                if !destinations.is_empty() {
                    let hosts = host_observation(&read)?;
                    if let Some(why) = own_address_refusal(&name, &destinations, &hosts) {
                        return Err(service_refused(why));
                    }
                }
                if template.is_vm() {
                    // A virtual machine's local image is checked only when the template pins it: an unpinned one is
                    // left to Incus, as before.
                    if template.local_image_pin().is_some() {
                        local_image_present(&read, &template, project)?;
                    }
                    if self.cfg.kernel.vm_caps().is_none() {
                        return Err(service_refused(no_vm_caps(template_name)));
                    }
                    // Every refusal below comes before any write, so a refused create leaves nothing behind.
                    if let Some(why) = mode_refusal(&self.cfg, &all_entries) {
                        return Err(service_refused(why));
                    }
                    crate::service::create_settings(&template, args.get("settings"))
                        .map_err(service_template_error)?;
                    // A tenant's machine joins the managed service network and nothing else: a host bridge would put
                    // it on the host's own network.
                    if tenant.is_some() && args.get("bridge").is_some() {
                        return Err(service_refused(TENANT_BRIDGE));
                    }
                    let network = if tenant.is_some() {
                        VmNetwork::Managed
                    } else {
                        vm_network(&self.cfg, &args)?
                    };
                    let mac = match args.get("mac") {
                        Some(value) => Some(
                            value
                                .as_str()
                                .and_then(crate::service::normalize_mac)
                                .ok_or_else(|| {
                                    CallError::Invalid(
                                        "mac: expected six hex octets joined by \":\", not a group or all-zero address"
                                            .into(),
                                    )
                                })?,
                        ),
                        None => None,
                    };
                    match &network {
                        VmNetwork::Managed if mac.is_some() => {
                            return Err(service_refused(MANAGED_MAC));
                        }
                        VmNetwork::Host(_) => {
                            if let Some(why) = host_bridge_refusal(&template) {
                                return Err(service_refused(why));
                            }
                            if !destinations.is_empty() {
                                return Err(service_refused(destinations_on_host_bridge(&name)));
                            }
                        }
                        VmNetwork::Managed => {}
                    }
                    if let Some(mac) = &mac
                        && (record_holds_mac(&entries, mac)
                            || rows.iter().any(|row| instance_holds_mac(row, mac)))
                    {
                        return Err(service_refused(mac_held(mac)));
                    }
                    if let Some(refusal) = create_room_refusal(
                        &self.cfg,
                        &all_entries,
                        tenant.as_ref(),
                        &name,
                        &template,
                    ) {
                        return Err(refusal);
                    }
                    // A project that exists must be the one its owning entry made: the bridged entry's on a node whose
                    // first service was a virtual machine on a host bridge, the network entry's otherwise. One made by
                    // hand, or one whose owner this node does not hold, is left alone.
                    let owner = project_owner(&entries).unwrap_or(match &network {
                        VmNetwork::Managed => NET_ENTRY,
                        VmNetwork::Host(_) => arca_netproject::BRIDGED_ENTRY,
                    });
                    let project_path =
                        format!("/1.0/projects/{0}?project={0}", arca_netproject::PROJECT);
                    // A tenant's project is its entry's own, made with the tenant; the node's project is not asked for.
                    match read.get(&project_path).and_then(|found| {
                        if tenant.is_some() {
                            Err(arca_incus::IncusError::Status {
                                code: 404,
                                message: String::new(),
                            })
                        } else {
                            Ok(found)
                        }
                    }) {
                        Ok(found)
                            if found["config"][arca_kernel::ownership::KERNEL_ENTRY_KEY]
                                .as_str()
                                == Some(owner) => {}
                        Ok(_) => {
                            return Err(service_refused(format!(
                                "the Incus project {} already exists and this {} did not create it; Arca leaves it alone, so no virtual machine can be created until it is removed",
                                arca_netproject::PROJECT,
                                if managed { "node" } else { "bridged node" }
                            )));
                        }
                        Err(arca_incus::IncusError::Status { code: 404, .. }) => {}
                        Err(error) => {
                            return Err(incus_read_refused(
                                &error,
                                "the service project",
                                &subject,
                                "Nothing was created.",
                            ));
                        }
                    }
                    let needs = ProjectNeeds {
                        managed: matches!(network, VmNetwork::Managed),
                        vm: true,
                        host_bridge: matches!(network, VmNetwork::Host(_)),
                    };
                    let pool = match bridged {
                        Some(bridged) => bridged.pool.clone(),
                        None => service_pool(&self.cfg).map_err(|_| {
                            service_refused("the default profile does not name a storage pool")
                        })?,
                    };
                    let place = match network {
                        VmNetwork::Host(bridge) => VmPlace::Host { bridge, mac },
                        VmNetwork::Managed => {
                            let ipv4 = self.free_address(
                                &read,
                                &cidr,
                                &all_rows,
                                &all_entries,
                                tenant.as_ref(),
                                &subject,
                            )?;
                            VmPlace::Managed {
                                ipv4,
                                gateway: gateway.clone(),
                            }
                        }
                    };
                    let mut changes = if tenant.is_some() {
                        Vec::new()
                    } else {
                        project_changes(&all_entries, &self.cfg, needs)
                            .map_err(no_service_project)?
                    };
                    let mut config =
                        vm_service_config(&name, &content, args.get("settings"), place, pool, now)?;
                    config.destinations = destinations.clone();
                    let deploy =
                        tenants::open_deploy(&self.cfg, tenant.as_ref(), &args, caller, &id, now)?;
                    self.place_in_tenant(&mut config, tenant.as_ref(), &all_entries, deploy);
                    // Created stopped when the call asked for that. A machine whose template declares secrets is
                    // created stopped whatever the call says, and the two rules give one `start: false`.
                    config.start = config.start && start;
                    // An explicit `start: false`, and nothing else, also leaves `boot.autostart` out, recorded as
                    // `autostart: false` so a create made again from the record is the same: Incus restores the last
                    // state when the key is unset, so a host restart never boots it before its first instance_start. A
                    // create the template alone stops keeps the template's autostart, in its record and in its body.
                    config.autostart = config.autostart && start;
                    changes.push(crate::desired_store::EntryChange::Put(service_entry(
                        &id,
                        "arca.service",
                        serde_json::to_value(&config).map_err(|_| internal())?,
                    )));
                    let mut made = self.settle_create(
                        &read,
                        &name,
                        template_name,
                        changes,
                        &config,
                        None,
                        now,
                    )?;
                    if !config.start {
                        made["stoppedBecause"] = json!(stopped_because(start, forced));
                    }
                    return Ok(made);
                }
                if bridged.is_some() && !managed {
                    return Err(service_refused(bridged_only(template_name)));
                }
                if args.get("bridge").is_some() || args.get("mac").is_some() {
                    return Err(CallError::Invalid(
                        "bridge and mac: only a virtual-machine template takes them".into(),
                    ));
                }
                // A container template has no settings a create may set: said before anything is written.
                crate::service::create_settings(&template, args.get("settings"))
                    .map_err(service_template_error)?;
                local_image_present(&read, &template, project)?;
                // An image fetched with this node's credential for its registry is said on the card: a create that
                // does not say it does not run. Nothing in a tenant is fetched with it.
                if tenant.is_none() && template.kind == "oci" {
                    crate::registry::stated(
                        &self.cfg,
                        crate::registry::server_of(&template.image).as_deref(),
                        args.get("registry"),
                        preapproved,
                    )?;
                } else if args.get("registry").is_some() {
                    return Err(CallError::Invalid(
                        "registry: only a create of the node's own project, from an image on a registry, names one".into(),
                    ));
                }
                if let Some(scope) = &tenant {
                    if template.lan.is_some() || template.publishable {
                        return Err(service_refused(tenant_host_port(template_name)));
                    }
                    if let Some(refusal) =
                        create_room_refusal(&self.cfg, &all_entries, Some(scope), &name, &template)
                    {
                        return Err(refusal);
                    }
                }
                let ipv4 = self.free_address(
                    &read,
                    &cidr,
                    &all_rows,
                    &all_entries,
                    tenant.as_ref(),
                    &subject,
                )?;
                let pool = service_pool(&self.cfg).map_err(|_| {
                    service_refused("the default profile does not name a storage pool")
                })?;
                let mut changes = if tenant.is_some() {
                    Vec::new()
                } else {
                    ensure_netproject(&all_entries, &self.cfg).map_err(no_service_project)?
                };
                let secrets_dir = self.cfg.state_dir.join("secrets");
                let service_secrets = secrets_dir.join(&id);
                let seeded = service_secret_names(&service_secrets).map_err(|_| internal())?;
                let mut config = service_config(ServiceParts {
                    owner: &id,
                    name: &name,
                    template: &template,
                    ipv4,
                    gateway,
                    pool,
                    now,
                    secrets_dir: &secrets_dir,
                    content: &content,
                    kernel_access,
                })
                .inspect_err(|_| cleanup_service_secrets(&service_secrets, &seeded))?;
                config.destinations = destinations;
                let deploy =
                    tenants::open_deploy(&self.cfg, tenant.as_ref(), &args, caller, &id, now)
                        .inspect_err(|_| cleanup_service_secrets(&service_secrets, &seeded))?;
                self.place_in_tenant(&mut config, tenant.as_ref(), &all_entries, deploy);
                // Created stopped when the call asked for that. A service whose template needs a secret that is not
                // set yet is created stopped either way.
                config.start = config.start && start;
                // An explicit `start: false`, and nothing else, also leaves `boot.autostart` out, recorded as
                // `autostart: false` so a create made again from the record is the same: Incus restores the last state
                // when the key is unset, so a host restart never boots it before its first instance_start. A create
                // the template alone stops keeps the template's autostart, in its record and in its body.
                config.autostart = config.autostart && start;
                changes.push(crate::desired_store::EntryChange::Put(service_entry(
                    &id,
                    "arca.service",
                    serde_json::to_value(&config).map_err(|_| internal())?,
                )));
                let mut made = self.settle_create(
                    &read,
                    &name,
                    template_name,
                    changes,
                    &config,
                    Some((service_secrets.as_path(), &seeded)),
                    now,
                )?;
                if !config.start {
                    made["stoppedBecause"] = json!(stopped_because(start, forced));
                }
                // The node's front was made, or made again at another address: every tenant service's ACL follows now.
                let front = self
                    .cfg
                    .kernel
                    .tenants
                    .as_ref()
                    .and_then(|t| t.front.as_deref());
                if tenant.is_none() && front == Some(name.as_str()) {
                    // This create holds the write gate already.
                    let _ = tenants::sync_front_held(&self.kernel, &self.cfg, now);
                }
                Ok(made)
            }
            _ => Err(service_not_found("unknown service verb")),
        }
    }
}

#[cfg(test)]
mod service_project_tests {
    use super::*;

    fn config(kernel: Value) -> NodeConfig {
        serde_json::from_value(json!({
            "stateDir": "/nonexistent/state",
            "incus": { "socket": "/nonexistent/incus.sock" },
            "listen": { "unix": "/nonexistent/arca.sock" },
            "tokens": { "hashDir": "/nonexistent/tokens" },
            "kernel": kernel
        }))
        .expect("the fixture config parses")
    }

    fn refused(cfg: &NodeConfig) -> String {
        match service_project(cfg) {
            Err(CallError::Refused { message, .. }) => message,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn only_a_config_that_names_nothing_of_the_service_project_asks_for_none() {
        let project = arca_netproject::PROJECT;
        assert!(no_service_project_asked(&config(
            json!({ "droiddProjects": [] })
        )));
        assert!(no_service_project_asked(&config(
            json!({ "projects": ["elsewhere"], "droiddProjects": ["droid"] })
        )));
        // Naming the service project as droidd's, or a scratch run, asks for no service either.
        assert!(no_service_project_asked(&config(
            json!({ "droiddProjects": [project] })
        )));
        assert!(no_service_project_asked(&config(
            json!({ "scratch": true, "droiddProjects": [] })
        )));
        // Any one of the three keys is a config that asked for services, whole or not.
        assert!(!no_service_project_asked(&config(
            json!({ "serviceCidr": "192.0.2.0/24", "droiddProjects": [] })
        )));
        assert!(!no_service_project_asked(&config(
            json!({ "projects": [project], "droiddProjects": ["droid"] })
        )));
    }

    #[test]
    fn a_node_that_only_looks_with_a_service_subnet_is_refused_changes_in_those_words() {
        // What `arca init --watch-only` leaves: the subnet, and no kernel project.
        let cfg = config(json!({ "serviceCidr": "192.0.2.0/24", "droiddProjects": [] }));
        assert!(!no_service_project_asked(&cfg));
        assert_eq!(refused(&cfg), arca_mcp::jsonrpc::ONLY_LOOKS_NOTE);
        // A node that makes changes elsewhere and left the service project out is told that, as before.
        assert_eq!(
            refused(&config(json!({
                "serviceCidr": "192.0.2.0/24",
                "projects": ["elsewhere"],
                "droiddProjects": ["droid"]
            }))),
            "no service project: the service project is excluded by kernel configuration"
        );
    }

    #[test]
    fn a_missing_subnet_is_refused_in_the_words_of_what_the_node_is() {
        let keys = "its config must set kernel.serviceCidr to an IPv4 /24 and list arcak-svc in kernel.projects (with kernel.droiddProjects), and the node be restarted";
        assert_eq!(
            refused(&config(json!({ "droiddProjects": [] }))),
            "no service project: this node only looks, and its config sets up no services. To let it run them: arca init --enable-changes"
        );
        assert_eq!(
            refused(&config(
                json!({ "projects": ["elsewhere"], "droiddProjects": ["droid"] })
            )),
            format!(
                "no service project: this node's config sets up no services. To let it run them, {keys}"
            )
        );
        assert_eq!(
            refused(&config(
                json!({ "projects": ["arcak-svc"], "droiddProjects": ["droid"] })
            )),
            format!(
                "no service project: the node's config does not set kernel.serviceCidr. To run services, {keys}"
            )
        );
    }
}
