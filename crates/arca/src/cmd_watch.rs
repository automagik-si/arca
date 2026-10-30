//! The one-shot host reads: `watch --once --json`, which prints the watch report, and `metrics --json`, which samples the host
//! for a while and prints the `node_metrics` result. Both print the same JSON as the MCP verb they answer.

use std::collections::BTreeSet;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use arca_incus::UnixIncus;
use arca_metrics::node_metrics;
use serde_json::Value;

use crate::config;
use crate::handlers::{self, SAMPLE_INTERVAL};
use crate::{Failure, Flags, call_failure};

/// The longest `metrics` may sample. The window of `node_metrics` is a minute, so the run stays inside it with room to spare.
const MAX_SAMPLE_SECONDS: u64 = 30;

/// How long `metrics` samples when no `--sample-for` is given: long enough that the mean has two samples to average.
const DEFAULT_SAMPLE_SECONDS: u64 = 5;

/// `watch --once --json --config <file>`.
pub fn watch(flags: &Flags) -> Result<Option<Value>, Failure> {
    if !flags.switch("once") {
        return Err(Failure::Usage(
            "watch runs only as a single read: pass --once".to_string(),
        ));
    }
    if !flags.switch("json") {
        return Err(Failure::Usage("watch prints JSON: pass --json".to_string()));
    }
    let cfg = config::load(Path::new(flags.required("config")?))?;
    let phones: BTreeSet<String> = cfg.identity_phones.iter().cloned().collect();
    let value = handlers::watch_snapshot(
        &UnixIncus::new(cfg.incus.socket.clone()),
        &phones,
        handlers::now_ms(),
    )
    .map_err(call_failure)?;
    Ok(Some(value))
}

/// `metrics --json [--sample-for <seconds>] --config <file>`. It samples at once and then every
/// [`SAMPLE_INTERVAL`] until the time runs out, and prints the result with a one-minute window.
pub fn metrics(flags: &Flags) -> Result<Option<Value>, Failure> {
    if !flags.switch("json") {
        return Err(Failure::Usage(
            "metrics prints JSON: pass --json".to_string(),
        ));
    }
    let seconds = match flags.value("sample-for") {
        None => DEFAULT_SAMPLE_SECONDS,
        Some(text) => text
            .parse::<u64>()
            .ok()
            .filter(|seconds| *seconds <= MAX_SAMPLE_SECONDS)
            .ok_or_else(|| {
                Failure::Usage(format!(
                    "--sample-for is a whole number of seconds from 0 to {MAX_SAMPLE_SECONDS}"
                ))
            })?,
    };
    let cfg = config::load(Path::new(flags.required("config")?))?;
    let mut sampler = handlers::new_sampler(&cfg);
    sampler.sample(handlers::now_ms());
    let span = Duration::from_secs(seconds);
    let start = Instant::now();
    while start.elapsed() < span {
        thread::sleep(SAMPLE_INTERVAL.min(span - start.elapsed()));
        sampler.sample(handlers::now_ms());
    }
    Ok(Some(node_metrics(&sampler, Some(1), handlers::now_ms())))
}
