//! The rules, the inputs a tick reads, and the evaluator: one state machine per rule and label set.
//!
//! A rule fires when `value > threshold` has held for `for_s`, and clears when `value <= threshold - clear_margin` has held
//! for `clear_for_s`. A reading that is missing is unknown: the alert keeps its state, its timer restarts, and no event is
//! written. Only `disk-pool-full` is evaluated so far; the other rules have no reading yet.

use std::collections::BTreeMap;

use arca_store::{AlertEvent, EventState, ResolveReason};

use crate::{AlertState, FiringAlert, UnknownAlert, UnknownReason};

/// An alert's own labels, such as `pool`.
pub type Labels = BTreeMap<String, String>;

/// The fixed rule ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RuleId {
    DiskPoolFull,
    MemPsiHigh,
    CpuHot,
    ServiceDown,
    Drift,
    UpdateFailed,
    CrashLoop,
    SmartWarning,
}

impl RuleId {
    pub const ALL: [RuleId; 8] = [
        Self::DiskPoolFull,
        Self::MemPsiHigh,
        Self::CpuHot,
        Self::ServiceDown,
        Self::Drift,
        Self::UpdateFailed,
        Self::CrashLoop,
        Self::SmartWarning,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::DiskPoolFull => "disk-pool-full",
            Self::MemPsiHigh => "mem-psi-high",
            Self::CpuHot => "cpu-hot",
            Self::ServiceDown => "service-down",
            Self::Drift => "drift",
            Self::UpdateFailed => "update-failed",
            Self::CrashLoop => "crash-loop",
            Self::SmartWarning => "smart-warning",
        }
    }

    /// The rule an id names, if it is one of the fixed ids.
    pub fn parse(text: &str) -> Option<RuleId> {
        Self::ALL.into_iter().find(|id| id.as_str() == text)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Critical,
    Warning,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::Warning => "warning",
        }
    }
}

/// One rule with its thresholds. The times are seconds.
#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    pub id: RuleId,
    pub enabled: bool,
    pub threshold: f64,
    pub for_s: u64,
    pub clear_for_s: u64,
    pub clear_margin: f64,
    pub severity: Severity,
}

impl Rule {
    /// A default rule: enabled, firing after 60 s and clearing after 120 s.
    fn default_of(id: RuleId, threshold: f64, clear_margin: f64, severity: Severity) -> Rule {
        Rule {
            id,
            enabled: true,
            threshold,
            for_s: 60,
            clear_for_s: 120,
            clear_margin,
            severity,
        }
    }
}

/// The rule set, one rule per id.
#[derive(Debug, Clone, PartialEq)]
pub struct Rules(pub Vec<Rule>);

impl Default for Rules {
    /// The eight defaults. Disk is percent used, `mem-psi-high` the memory `some` avg10, `cpu-hot` °C. The yes-or-no rules
    /// read 1 when bad, `drift` a count, and `crash-loop` the starts of the last 10 minutes.
    fn default() -> Rules {
        use RuleId as R;
        use Severity::{Critical, Warning};
        Rules(vec![
            Rule::default_of(R::DiskPoolFull, 90.0, 5.0, Critical),
            Rule::default_of(R::MemPsiHigh, 25.0, 5.0, Warning),
            Rule::default_of(R::CpuHot, 85.0, 5.0, Warning),
            Rule::default_of(R::ServiceDown, 0.0, 0.0, Critical),
            Rule::default_of(R::Drift, 0.0, 0.0, Warning),
            Rule::default_of(R::UpdateFailed, 0.0, 0.0, Warning),
            Rule::default_of(R::CrashLoop, 2.0, 2.0, Critical),
            Rule::default_of(R::SmartWarning, 0.0, 0.0, Critical),
        ])
    }
}

impl Rules {
    /// The ids of the disabled rules, in rule order.
    pub fn disabled(&self) -> Vec<String> {
        self.0
            .iter()
            .filter(|rule| !rule.enabled)
            .map(|rule| rule.id.as_str().to_string())
            .collect()
    }
}

/// A reading and the epoch milliseconds it was taken at.
#[derive(Debug, Clone, PartialEq)]
pub struct Timed<T> {
    pub at: u64,
    pub value: T,
}

/// Everything one tick reads. A `None` source is unknown.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Inputs {
    pub at: u64,
    pub pools: Option<Timed<Vec<PoolUse>>>,
    pub memory_some_avg10: Option<Timed<f64>>,
    pub cpu_temp_c: Option<Timed<f64>>,
    pub components: Option<Timed<Vec<Component>>>,
    pub watch: Option<Timed<WatchReading>>,
    pub instances: Option<Timed<Vec<InstanceUse>>>,
    pub update: Option<Timed<UpdateUse>>,
    pub drives: Option<Timed<Vec<DriveUse>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolUse {
    pub name: String,
    pub used_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentName {
    Incus,
    Arca,
    Watch,
    Shim,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Component {
    pub name: ComponentName,
    pub ok: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchReading {
    /// The drift entries, by kind.
    pub drift: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceUse {
    pub project: String,
    pub name: String,
    pub starts_10m: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateUse {
    pub failed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveUse {
    pub device: String,
    pub passed: bool,
}

/// The state of one rule and label set.
#[derive(Debug, Clone)]
struct Track {
    state: AlertState,
    /// When the current fire or clear condition started holding; `None` while it does not hold.
    timer: Option<u64>,
    /// The firing event, while the alert fires.
    firing: Option<AlertEvent>,
    /// Why the newest reading was unknown, while it is.
    unknown: Option<UnknownReason>,
}

/// The alert states of every rule and label set.
#[derive(Debug, Default)]
pub struct Evaluator {
    tracks: BTreeMap<(RuleId, Labels), Track>,
}

impl Evaluator {
    /// An evaluator holding the stored firing alerts as firing, with fresh timers, unknown until their first reading.
    pub fn load(firing: Vec<AlertEvent>) -> Evaluator {
        let mut tracks = BTreeMap::new();
        for event in firing {
            let Some(id) = RuleId::parse(&event.rule) else {
                continue;
            };
            let key = (id, event.labels.clone());
            let track = Track {
                state: AlertState::Firing,
                timer: None,
                firing: Some(event),
                unknown: Some(UnknownReason::Missing),
            };
            tracks.insert(key, track);
        }
        Evaluator { tracks }
    }

    /// Evaluates every enabled rule that has a reading at `now`, and returns the transitions as events, in order.
    pub fn step(&mut self, rules: &Rules, inputs: &Inputs, now: u64) -> Vec<AlertEvent> {
        let mut events = Vec::new();
        for rule in rules.0.iter().filter(|rule| rule.enabled) {
            let readings = match source(rule.id, inputs) {
                Source::NotRead => continue,
                Source::Unknown => BTreeMap::new(),
                Source::Known(readings) => {
                    // A subject that is no longer read drops out while it is ok.
                    self.tracks.retain(|(id, labels), track| {
                        *id != rule.id
                            || readings.contains_key(labels)
                            || track.state != AlertState::Ok
                    });
                    readings
                }
            };
            // Every alert of the rule without a reading keeps its state, unknown.
            for ((id, labels), track) in self.tracks.iter_mut() {
                if *id == rule.id && !readings.contains_key(labels) {
                    unknown(track);
                }
            }
            for (labels, value) in readings {
                let track = self
                    .tracks
                    .entry((rule.id, labels.clone()))
                    .or_insert(Track {
                        state: AlertState::Ok,
                        timer: None,
                        firing: None,
                        unknown: None,
                    });
                events.extend(apply(rule, &labels, track, value, now));
            }
        }
        events
    }

    /// Gives the firing events among `events` the sequence numbers they were committed with.
    pub fn committed(&mut self, events: &[AlertEvent], seqs: &[u64]) {
        for (event, seq) in events.iter().zip(seqs) {
            let Some(id) = RuleId::parse(&event.rule) else {
                continue;
            };
            if event.state != EventState::Firing {
                continue;
            }
            if let Some(firing) = self
                .tracks
                .get_mut(&(id, event.labels.clone()))
                .and_then(|track| track.firing.as_mut())
                .filter(|firing| firing.seq.is_none())
            {
                firing.seq = Some(*seq);
                firing.alert_id = Some(*seq);
            }
        }
    }

    /// The firing alerts, in the order they started firing.
    pub fn firing(&self) -> Vec<FiringAlert> {
        let mut firing: Vec<FiringAlert> = self
            .tracks
            .values()
            .filter(|track| track.state == AlertState::Firing)
            .filter_map(|track| {
                track.firing.clone().map(|event| FiringAlert {
                    event,
                    unknown: track.unknown.is_some(),
                })
            })
            .collect();
        firing.sort_by_key(|alert| (alert.event.started_at, alert.event.seq));
        firing
    }

    /// The alerts whose newest reading is unknown.
    pub fn unknown(&self) -> Vec<UnknownAlert> {
        self.tracks
            .iter()
            .filter_map(|((id, labels), track)| {
                track.unknown.map(|reason| UnknownAlert {
                    rule: id.as_str().to_string(),
                    labels: labels.clone(),
                    state: track.state,
                    reason,
                })
            })
            .collect()
    }
}

/// What a rule reads this tick.
enum Source {
    /// The rule has no reading in this build.
    NotRead,
    /// The whole source is unknown.
    Unknown,
    /// Each label set read, with its value, which is `None` when unknown.
    Known(BTreeMap<Labels, Option<f64>>),
}

fn source(id: RuleId, inputs: &Inputs) -> Source {
    match (id, &inputs.pools) {
        (RuleId::DiskPoolFull, None) => Source::Unknown,
        (RuleId::DiskPoolFull, Some(pools)) => Source::Known(
            pools
                .value
                .iter()
                .map(|pool| {
                    let labels = Labels::from([("pool".to_string(), pool.name.clone())]);
                    (labels, percent_used(pool))
                })
                .collect(),
        ),
        _ => Source::NotRead,
    }
}

/// Used over total, in percent. `None` when either is unknown or the pool has no size.
fn percent_used(pool: &PoolUse) -> Option<f64> {
    let (used, total) = (pool.used_bytes?, pool.total_bytes?);
    (total > 0).then(|| used as f64 / total as f64 * 100.0)
}

/// Unknown data: the state is kept, the timer restarts and no event is written.
fn unknown(track: &mut Track) {
    track.timer = None;
    track.unknown = Some(UnknownReason::Missing);
}

/// One reading of one alert at `now`, and the event it causes, if any.
fn apply(
    rule: &Rule,
    labels: &Labels,
    track: &mut Track,
    value: Option<f64>,
    now: u64,
) -> Option<AlertEvent> {
    let Some(value) = value else {
        unknown(track);
        return None;
    };
    track.unknown = None;
    let held =
        |since: u64, seconds: u64| now.saturating_sub(since) >= seconds.saturating_mul(1_000);
    match track.state {
        AlertState::Ok | AlertState::Pending if value > rule.threshold => {
            track.state = AlertState::Pending;
            let since = *track.timer.get_or_insert(now);
            if !held(since, rule.for_s) {
                return None;
            }
            let event = AlertEvent {
                seq: None,
                alert_id: None,
                rule: rule.id.as_str().to_string(),
                severity: rule.severity.as_str().to_string(),
                state: EventState::Firing,
                labels: labels.clone(),
                value: Some(value),
                threshold: Some(rule.threshold),
                started_at: now,
                resolved_at: None,
                reason: None,
                at: now,
            };
            track.state = AlertState::Firing;
            track.timer = None;
            track.firing = Some(event.clone());
            Some(event)
        }
        AlertState::Ok | AlertState::Pending => {
            track.state = AlertState::Ok;
            track.timer = None;
            None
        }
        AlertState::Firing if value <= rule.threshold - rule.clear_margin => {
            let since = *track.timer.get_or_insert(now);
            if !held(since, rule.clear_for_s) {
                return None;
            }
            let firing = track.firing.take()?;
            track.state = AlertState::Ok;
            track.timer = None;
            Some(AlertEvent {
                seq: None,
                state: EventState::Resolved,
                value: Some(value),
                threshold: Some(rule.threshold),
                resolved_at: Some(now),
                reason: Some(ResolveReason::Cleared),
                at: now,
                ..firing
            })
        }
        AlertState::Firing => {
            track.timer = None;
            None
        }
    }
}
