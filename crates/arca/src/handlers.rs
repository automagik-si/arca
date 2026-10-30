//! The four verbs the binary answers, and the host reads they share with the one-shot commands. The MCP handlers and the
//! `watch` and `metrics` commands call the same functions, so a command's JSON output equals the MCP result of its verb.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arca_api::Scope;
use arca_incus::{IncusRead, UnixIncus};
use arca_kernel::plugin::EffectRefusal;
use arca_mcp::{CallError, Caller, ToolHandler};
use arca_metrics::{Sampler, SamplerConfig, node_metrics};
use arca_providers::ReadOnlyIncusProvider;
use arca_watch::{WatchConfig, watch_once};
use serde_json::{Value, json};

use crate::config::NodeConfig;

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

/// The watch report of the host now. The provider lists every project first; the watch covers all of them, so it is
/// complete: a live entry with no object in any project is drift. A later narrower cover must set `complete` to false.
pub fn watch_snapshot(
    incus: &UnixIncus,
    phones: &BTreeSet<String>,
    now: u64,
) -> Result<Value, CallError> {
    let mut provider = ReadOnlyIncusProvider::new(incus.clone());
    let listed = provider
        .project_names()
        .map_err(|refusal| CallError::Internal(listing_reason(&refusal)))?;
    let projects: BTreeSet<String> = listed.iter().cloned().collect();
    let complete = listed.iter().all(|name| projects.contains(name));
    let cfg = WatchConfig {
        identity_phones: phones.clone(),
        projects,
        complete,
        ..WatchConfig::default()
    };
    let report = watch_once(&mut provider, &cfg, now)
        .map_err(|e| CallError::Internal(format!("the watch failed: {e}")))?;
    serde_json::to_value(&report)
        .map_err(|_| CallError::Internal("the report does not serialize".to_string()))
}

fn listing_reason(refusal: &EffectRefusal) -> String {
    match refusal {
        EffectRefusal::Provider(reason) => format!("the projects could not be listed: {reason}"),
        _ => "the projects could not be listed".to_string(),
    }
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

/// `node_metrics`: the latest sample and the mean over the requested minutes.
pub struct MetricsHandler {
    pub sampler: Arc<Mutex<Sampler<UnixIncus>>>,
}

impl ToolHandler for MetricsHandler {
    fn call(&self, args: Value, _caller: &Caller) -> Result<Value, CallError> {
        let minutes = args
            .get("minutes")
            .and_then(Value::as_u64)
            .and_then(|m| u32::try_from(m).ok());
        let sampler = self
            .sampler
            .lock()
            .map_err(|_| CallError::Internal("the sampler stopped".to_string()))?;
        Ok(node_metrics(&sampler, minutes, now_ms()))
    }
}

/// `watch_report`: the watch of every project, with the identity flags of the configured phones.
pub struct WatchHandler {
    pub socket: PathBuf,
    pub phones: BTreeSet<String>,
}

impl ToolHandler for WatchHandler {
    fn call(&self, _args: Value, _caller: &Caller) -> Result<Value, CallError> {
        watch_snapshot(&UnixIncus::new(self.socket.clone()), &self.phones, now_ms())
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
