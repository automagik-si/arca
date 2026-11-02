//! Arca's node alerts. The [`Engine`] runs on the sampler's tick: it evaluates the rules over the tick's [`eval::Inputs`],
//! commits the tick's points and alert events to the store in one transaction, and returns the [`AlertSnapshot`] the tick
//! publishes, whether the commit worked or not. Alerts have no write path: nothing here writes to Incus or to the desired
//! state.
//!
//! A failed commit keeps its events in a queue, at most [`QUEUE_LIMIT`] long. The next commit that works writes the queue and
//! the tick's events together, oldest first, so a resolve is always committed with the firing it closes. The in-memory
//! `store-write-failed` alert reports the failures, because the store cannot hold it.

pub mod checks;
pub mod eval;
pub mod export;
pub mod health;
pub mod inputs;
pub mod rules;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{Duration, Instant};

pub use arca_store::{AlertEvent, Point, RollupReport, Store, StoreError};

use arca_store::EventState;

use crate::eval::{Evaluator, Inputs, Labels, Rules};

/// The most events a failed commit keeps waiting. Past it the oldest are dropped.
pub const QUEUE_LIMIT: usize = 1_000;
/// A commit that succeeds but takes longer than this is committed, and it counts as a failure.
pub const SLOW_COMMIT: Duration = Duration::from_secs(1);
/// How long commits must keep succeeding, in milliseconds, before `store-write-failed` clears.
pub const WRITE_FAILED_CLEAR_MS: u64 = 120_000;
/// The rule of the in-memory alert that reports the store's failed writes.
pub const STORE_WRITE_FAILED: &str = "store-write-failed";

/// The store calls a tick makes. `arca_store::Store` is the store; a test may give its own.
pub trait TickStore {
    fn commit_tick(
        &mut self,
        at: u64,
        points: &[Point],
        events: &[AlertEvent],
    ) -> Result<Vec<u64>, StoreError>;
    fn firing(&self) -> Result<Vec<AlertEvent>, StoreError>;
    fn rollup(&mut self, now_ms: u64) -> Result<RollupReport, StoreError>;
}

impl TickStore for Store {
    fn commit_tick(
        &mut self,
        at: u64,
        points: &[Point],
        events: &[AlertEvent],
    ) -> Result<Vec<u64>, StoreError> {
        Store::commit_tick(self, at, points, events)
    }

    fn firing(&self) -> Result<Vec<AlertEvent>, StoreError> {
        Store::firing(self)
    }

    fn rollup(&mut self, now_ms: u64) -> Result<RollupReport, StoreError> {
        Store::rollup(self, now_ms)
    }
}

/// The state of an alert's rule and label set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertState {
    Ok,
    Pending,
    Firing,
}

impl AlertState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Pending => "pending",
            Self::Firing => "firing",
        }
    }
}

/// Why a reading is unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnknownReason {
    Missing,
    Stale,
}

impl UnknownReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Stale => "stale",
        }
    }
}

/// A firing alert, and whether its newest reading is unknown.
#[derive(Debug, Clone, PartialEq)]
pub struct FiringAlert {
    pub event: AlertEvent,
    pub unknown: bool,
}

/// An alert whose newest reading is unknown.
#[derive(Debug, Clone, PartialEq)]
pub struct UnknownAlert {
    pub rule: String,
    pub labels: BTreeMap<String, String>,
    pub state: AlertState,
    pub reason: UnknownReason,
}

/// A failed store call: when, and its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreErrorMark {
    pub at: u64,
    pub message: String,
}

/// What one tick published: the points it read, the alerts, and the store's health.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AlertSnapshot {
    pub at: u64,
    /// When the host sample the tick read was taken.
    pub sample_at: Option<u64>,
    /// When the newest watch sweep the tick read was taken, or `None` when no sweep has succeeded.
    pub watch_at: Option<u64>,
    pub points: Vec<Point>,
    pub firing: Vec<FiringAlert>,
    pub unknown: Vec<UnknownAlert>,
    pub disabled: Vec<String>,
    pub store_error: Option<StoreErrorMark>,
    pub store_reset: bool,
    pub store_write_failures: u64,
    /// How long the tick's commit took, in milliseconds.
    pub commit_ms: Option<u64>,
}

/// The in-memory `store-write-failed` alert: when it started firing, and since when commits have worked, while it fires.
struct WriteFailed {
    started_at: u64,
    ok_since: Option<u64>,
}

/// The evaluator and the store, on the sampler's tick.
pub struct Engine<S: TickStore> {
    store: S,
    rules: Rules,
    eval: Evaluator,
    store_reset: bool,
    store_write_failures: u64,
    /// The epoch minute the rollup last ran in.
    rolled_minute: u64,
    /// Events whose commit failed, oldest first. They are committed with the next batch that works.
    queue: VecDeque<AlertEvent>,
    /// Events dropped since start: the oldest past [`QUEUE_LIMIT`], and any resolve whose firing was dropped.
    dropped: u64,
    write_failed: Option<WriteFailed>,
}

impl<S: TickStore> Engine<S> {
    /// Loads the stored firing alerts, runs the rollup's catch-up, and resolves the stored firing alerts of disabled rules with
    /// `rule-disabled`. A failed commit of those resolves queues them for the first tick.
    pub fn start(
        mut store: S,
        rules: Rules,
        store_reset: bool,
        now_ms: u64,
    ) -> Result<Engine<S>, StoreError> {
        let firing = store.firing()?;
        store.rollup(now_ms)?;
        let (eval, disabled) = Evaluator::load(&rules, firing, now_ms);
        let mut engine = Engine {
            store,
            rules,
            eval,
            store_reset,
            store_write_failures: 0,
            rolled_minute: now_ms / 60_000,
            queue: VecDeque::new(),
            dropped: 0,
            write_failed: None,
        };
        if !disabled.is_empty() {
            engine.commit(now_ms, &[], disabled);
        }
        Ok(engine)
    }

    /// Evaluates the rules, commits `points` and the tick's events in one transaction, runs the rollup on the first tick of
    /// each minute, and returns the snapshot to publish. A failed commit still returns a snapshot, marked with the error.
    pub fn tick(&mut self, inputs: &Inputs, points: Vec<Point>, now_ms: u64) -> AlertSnapshot {
        let events = self.eval.step(&self.rules, inputs, now_ms);
        let (mut failed, commit_ms) = self.commit(now_ms, &points, events);
        if now_ms / 60_000 != self.rolled_minute {
            self.rolled_minute = now_ms / 60_000;
            if let Err(error) = self.store.rollup(now_ms) {
                self.record_failure(now_ms);
                failed.get_or_insert(error);
            }
        }
        let store_error = failed.map(|error| self.store_error_mark(&error, now_ms));
        AlertSnapshot {
            at: now_ms,
            sample_at: None,
            watch_at: None,
            points,
            firing: self.firing(),
            unknown: self.eval.unknown(),
            disabled: self.rules.disabled(),
            store_error,
            store_reset: self.store_reset,
            store_write_failures: self.store_write_failures,
            commit_ms: Some(commit_ms),
        }
    }

    /// Commits `points` and `events` with the queued events, oldest first, in one transaction. A failed commit queues the whole
    /// batch. A commit that succeeds but takes over [`SLOW_COMMIT`] keeps its events, which are never queued again, and counts
    /// as a failure. Returns the error of a failed commit, and the commit's time in milliseconds.
    fn commit(
        &mut self,
        now_ms: u64,
        points: &[Point],
        events: Vec<AlertEvent>,
    ) -> (Option<StoreError>, u64) {
        let mut batch: Vec<AlertEvent> = self.queue.drain(..).collect();
        batch.extend(events);
        self.dropped += prune_orphans(&mut batch);
        let started = Instant::now();
        let result = self.store.commit_tick(now_ms, points, &batch);
        let elapsed = started.elapsed();
        let commit_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        match result {
            Ok(seqs) => {
                self.eval.committed(&batch, &seqs);
                if elapsed > SLOW_COMMIT {
                    self.record_failure(now_ms);
                } else {
                    self.record_success(now_ms);
                }
                (None, commit_ms)
            }
            Err(error) => {
                let excess = batch.len().saturating_sub(QUEUE_LIMIT);
                batch.drain(..excess);
                self.dropped += u64::try_from(excess).unwrap_or(u64::MAX);
                self.dropped += prune_orphans(&mut batch);
                self.queue = batch.into();
                self.record_failure(now_ms);
                (Some(error), commit_ms)
            }
        }
    }

    /// A commit worked. After [`WRITE_FAILED_CLEAR_MS`] of such commits, `store-write-failed` clears.
    fn record_success(&mut self, now_ms: u64) {
        let Some(failed) = self.write_failed.as_mut() else {
            return;
        };
        let since = *failed.ok_since.get_or_insert(now_ms);
        if now_ms.saturating_sub(since) >= WRITE_FAILED_CLEAR_MS {
            self.write_failed = None;
        }
    }

    /// A write failed, or was too slow. `store-write-failed` fires at once, and its clock starts again.
    fn record_failure(&mut self, now_ms: u64) {
        self.store_write_failures += 1;
        let failed = self.write_failed.get_or_insert(WriteFailed {
            started_at: now_ms,
            ok_since: None,
        });
        failed.ok_since = None;
    }

    fn store_error_mark(&self, error: &StoreError, now_ms: u64) -> StoreErrorMark {
        let message = if self.dropped == 0 {
            error.to_string()
        } else {
            format!("{error}; {} alert events dropped", self.dropped)
        };
        StoreErrorMark {
            at: now_ms,
            message,
        }
    }

    /// The firing alerts of the evaluator, and the in-memory `store-write-failed` alert while it fires.
    fn firing(&self) -> Vec<FiringAlert> {
        let mut firing = self.eval.firing();
        if let Some(failed) = &self.write_failed {
            firing.push(FiringAlert {
                event: AlertEvent {
                    seq: None,
                    alert_id: None,
                    rule: STORE_WRITE_FAILED.to_string(),
                    severity: "critical".to_string(),
                    state: EventState::Firing,
                    labels: BTreeMap::new(),
                    value: None,
                    threshold: None,
                    started_at: failed.started_at,
                    resolved_at: None,
                    reason: None,
                    at: failed.started_at,
                },
                unknown: false,
            });
            firing.sort_by_key(|alert| alert.event.started_at);
        }
        firing
    }
}

/// Drops each resolve that no firing in `batch` can link to, and returns how many it dropped. A resolve with an `alert_id` links
/// to a committed firing. One without takes the newest firing of its rule and labels earlier in the batch. A firing dropped from
/// the queue leaves its resolve with nothing to link to, and the store would refuse the whole batch for it.
fn prune_orphans(batch: &mut Vec<AlertEvent>) -> u64 {
    let mut open: BTreeSet<(String, Labels)> = BTreeSet::new();
    let before = batch.len();
    batch.retain(|event| match (event.state, event.alert_id) {
        (_, Some(_)) => true,
        (EventState::Firing, None) => {
            open.insert((event.rule.clone(), event.labels.clone()));
            true
        }
        (EventState::Resolved, None) => open.remove(&(event.rule.clone(), event.labels.clone())),
    });
    u64::try_from(before - batch.len()).unwrap_or(u64::MAX)
}
