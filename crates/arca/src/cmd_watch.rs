//! The one-shot host reads: `watch --once --json`, which prints the watch report, and `metrics --json`, which samples the host
//! for a while and prints the `node_metrics` result. Both print the same JSON as the MCP verb they answer.

use std::collections::BTreeSet;
use std::thread;
use std::time::{Duration, Instant};

use arca_incus::UnixIncus;
use serde_json::Value;

use crate::adoptions::AdoptionRecord;
use crate::handlers::{self, SAMPLE_INTERVAL};
use crate::{Failure, Flags, call_failure};

/// The longest `metrics` may sample. A run of a minute is the most its one-minute result window can be read against, so the
/// mean covers the whole run.
const MAX_SAMPLE_SECONDS: u64 = 60;

/// How long `metrics` samples when no `--sample-for` is given: long enough that the mean has two samples to average.
const DEFAULT_SAMPLE_SECONDS: u64 = 5;

/// `watch --once --json [--config <file>]`. The adoption record in the state directory names the adopted instances, which the
/// watch counts as live entries, as the server's `watch_report` does.
pub fn watch(flags: &Flags) -> Result<Option<Value>, Failure> {
    if !flags.switch("once") {
        return Err(Failure::Usage(
            "watch runs only as a single read: pass --once".to_string(),
        ));
    }
    if !flags.switch("json") {
        return Err(Failure::Usage("watch prints JSON: pass --json".to_string()));
    }
    let cfg = crate::load_config(flags)?;
    let phones: BTreeSet<String> = cfg.identity_phones.iter().cloned().collect();
    // The adopted instances are live entries, so each shows kernel-owned. A record that cannot be read fails the command.
    let adopted = AdoptionRecord::open(&cfg.state_dir)?.adopted();
    let value = handlers::watch_snapshot_scoped(
        UnixIncus::new(cfg.incus.socket.clone()),
        &phones,
        &adopted,
        handlers::now_ms(),
        false,
    )
    .map_err(call_failure)?;
    Ok(Some(value))
}

/// `metrics --json [--sample-for <seconds>] [--config <file>]`. It samples at once and then every [`SAMPLE_INTERVAL`] until the
/// time runs out, and prints the result. The mean covers every sample of the run.
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
    let cfg = crate::load_config(flags)?;
    let (mut sampler, _) = handlers::new_sampler(&cfg);
    sampler.sample(handlers::now_ms());
    let span = Duration::from_secs(seconds);
    let start = Instant::now();
    while start.elapsed() < span {
        thread::sleep(SAMPLE_INTERVAL.min(span - start.elapsed()));
        sampler.sample(handlers::now_ms());
    }
    Ok(Some(handlers::run_row(&sampler, handlers::now_ms())))
}
