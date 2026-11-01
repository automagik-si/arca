//! Arca's node alerts. The [`Engine`] runs on the sampler's tick: it evaluates the rules over the tick's [`eval::Inputs`],
//! commits the tick's points and alert events to the store in one transaction, and returns the [`AlertSnapshot`] the tick
//! publishes, whether the commit worked or not. Alerts have no write path: nothing here writes to Incus or to the desired
//! state.

pub mod eval;
pub mod export;
pub mod inputs;

use std::collections::BTreeMap;
use std::time::Instant;

pub use arca_store::{AlertEvent, Point, RollupReport, Store, StoreError};

use crate::eval::{Evaluator, Inputs, Rules};

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

/// The evaluator and the store, on the sampler's tick.
pub struct Engine<S: TickStore> {
    store: S,
    rules: Rules,
    eval: Evaluator,
    store_reset: bool,
    store_write_failures: u64,
    /// The epoch minute the rollup last ran in.
    rolled_minute: u64,
}

impl<S: TickStore> Engine<S> {
    /// Loads the stored firing alerts and runs the rollup's catch-up.
    pub fn start(
        mut store: S,
        rules: Rules,
        store_reset: bool,
        now_ms: u64,
    ) -> Result<Engine<S>, StoreError> {
        let firing = store.firing()?;
        store.rollup(now_ms)?;
        Ok(Engine {
            store,
            rules,
            eval: Evaluator::load(firing),
            store_reset,
            store_write_failures: 0,
            rolled_minute: now_ms / 60_000,
        })
    }

    /// Evaluates the rules, commits `points` and the tick's events in one transaction, runs the rollup on the first tick of
    /// each minute, and returns the snapshot to publish. A failed commit still returns a snapshot, marked with the error.
    pub fn tick(&mut self, inputs: &Inputs, points: Vec<Point>, now_ms: u64) -> AlertSnapshot {
        let events = self.eval.step(&self.rules, inputs, now_ms);
        let started = Instant::now();
        let mut store_error = None;
        match self.store.commit_tick(now_ms, &points, &events) {
            Ok(seqs) => self.eval.committed(&events, &seqs),
            Err(error) => store_error = Some(error),
        }
        let commit_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        if now_ms / 60_000 != self.rolled_minute {
            self.rolled_minute = now_ms / 60_000;
            if let Err(error) = self.store.rollup(now_ms) {
                store_error.get_or_insert(error);
            }
        }
        if store_error.is_some() {
            self.store_write_failures += 1;
        }
        AlertSnapshot {
            at: now_ms,
            sample_at: None,
            points,
            firing: self.eval.firing(),
            unknown: self.eval.unknown(),
            disabled: self.rules.disabled(),
            store_error: store_error.map(|error| StoreErrorMark {
                at: now_ms,
                message: error.to_string(),
            }),
            store_reset: self.store_reset,
            store_write_failures: self.store_write_failures,
            commit_ms: Some(commit_ms),
        }
    }
}
