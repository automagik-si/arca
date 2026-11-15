//! The rules, the inputs a tick reads, and the evaluator: one state machine per rule and label set.
//!
//! A rule fires when `value > threshold` has held for `for_s`, and clears when `value <= threshold - clear_margin` has held for
//! `clear_for_s`. A reading that is missing, not finite or older than its source's maximum age is unknown: a pending alert keeps
//! its state, a firing alert stays firing and marked unknown, and its timers restart. A reading this node can never provide
//! (such as PSI or CPU temperature on a remote source) resolves the firing alert once with `no data` after `clear_for_s`; a
//! transient gap never ends it. A fresh, known reading that no longer has an alert's label set resolves that alert with
//! `subject-gone`, once.

use std::collections::{BTreeMap, BTreeSet};

use arca_store::{AlertEvent, EventState, ResolveReason};

use crate::{AlertState, FiringAlert, UnknownAlert, UnknownReason};

/// The most recent host sample may be this old, in milliseconds, before its readings are unknown.
pub const SAMPLE_MAX_AGE_MS: u64 = 15_000;
/// The health checks may be this old, in milliseconds, before they are unknown.
pub const HEALTH_MAX_AGE_MS: u64 = 15_000;
/// The watch sweep may be this old, in milliseconds, before its drift is unknown.
pub const WATCH_MAX_AGE_MS: u64 = 90_000;
/// A latency probe may be this old, in milliseconds, before it is unknown.
pub const PROBE_MAX_AGE_MS: u64 = 90_000;
/// The incus-osd update status may be this old, in milliseconds, before it is unknown (15 minutes).
pub const OSD_MAX_AGE_MS: u64 = 900_000;
/// The newest the root part's status file may be, in milliseconds: three of its minute rereads.
pub const HOST_NETWORK_MAX_AGE_MS: u64 = 180_000;
/// The runner pools' sign-in board may be this old, in milliseconds, before it is unknown.
pub const RUNNER_SIGNIN_MAX_AGE_MS: u64 = 15_000;
/// A SMART reading may be this old, in milliseconds, before it is unknown (2 hours).
pub const SMART_MAX_AGE_MS: u64 = 7_200_000;

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
    GpuVramHigh,
    GpuGone,
    HostNetwork,
    RunnerSignIn,
}

impl RuleId {
    pub const ALL: [RuleId; 12] = [
        Self::DiskPoolFull,
        Self::MemPsiHigh,
        Self::CpuHot,
        Self::ServiceDown,
        Self::Drift,
        Self::UpdateFailed,
        Self::CrashLoop,
        Self::SmartWarning,
        Self::GpuVramHigh,
        Self::GpuGone,
        Self::HostNetwork,
        Self::RunnerSignIn,
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
            Self::GpuVramHigh => "gpu-vram-high",
            Self::GpuGone => "gpu-gone",
            Self::HostNetwork => "host-network-drift",
            Self::RunnerSignIn => "runner-signin",
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
    /// The twelve defaults. Disk is percent used, `mem-psi-high` the memory `some` avg10, `cpu-hot` °C. The yes-or-no rules
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
            // The owner's rule: a card above 95 percent VRAM for five minutes. A card that disappears from the inventory
            // fires at once and clears when it returns.
            Rule {
                id: R::GpuVramHigh,
                enabled: true,
                threshold: 95.0,
                for_s: 300,
                clear_for_s: 120,
                clear_margin: 5.0,
                severity: Warning,
            },
            Rule {
                id: R::GpuGone,
                enabled: true,
                threshold: 0.0,
                for_s: 15,
                clear_for_s: 120,
                clear_margin: 0.0,
                severity: Critical,
            },
            // An item of the host's declared forwarding and firewall that is not as declared: containers may have lost their
            // way out. The root part rereads each minute, so a minute of drift fires it.
            Rule::default_of(R::HostNetwork, 0.0, 0.0, Critical),
            // A runner pool whose GitHub credential is refused (401), whose App is not installed (404) or lacks the
            // permission (403): no job can reach the pool until someone acts.
            Rule::default_of(R::RunnerSignIn, 0.0, 0.0, Critical),
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
    /// Each card in the inventory, with its VRAM use in percent. A card without telemetry is present with `None`, so its
    /// alert is unknown rather than resolved; the whole source is `None` when the inventory read failed.
    pub gpus: Option<Timed<Vec<GpuUse>>>,
    /// The cards that were in the inventory and are gone from it now. `None` when the inventory read failed, so an outage
    /// never resolves a disappearance alert.
    pub gpus_gone: Option<Timed<Vec<GpuGone>>>,
    /// The items of the host's declared forwarding and firewall that are not as declared, from the root part's status file.
    /// `None` when there is no status file, so a node that has none is unknown and never resolves a firing alert.
    pub host_network: Option<Timed<Vec<HostNetworkItem>>>,
    /// Each runner pool's sign-in state. A node with no pools has a known, empty list.
    pub runner_signin: Option<Timed<Vec<RunnerSignInUse>>>,
    /// The rules whose reading this node can never provide, such as PSI and CPU temperature on a remote metrics source.
    /// A firing alert of one resolves with `no data` after its clear window; a transient outage never does.
    pub unavailable: BTreeSet<RuleId>,
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

impl ComponentName {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Incus => "incus",
            Self::Arca => "arca",
            Self::Watch => "watch",
            Self::Shim => "shim",
        }
    }
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

/// One GPU card's VRAM use. The PCI address is the card's identity, so two identical cards have two rows.
#[derive(Debug, Clone, PartialEq)]
pub struct GpuUse {
    pub pci: String,
    /// The share of the card's VRAM in use, in percent. `None` when the card has no telemetry yet.
    pub vram_used_pct: Option<f64>,
}

/// One card that was in the inventory and is gone from it now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuGone {
    pub pci: String,
}

/// One item of the host's forwarding and firewall that is not as declared, such as `ipv4-forwarding` or `rule-in:<bridge>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostNetworkItem {
    pub item: String,
}

/// One runner pool's sign-in state: `reason` names why it fails, and is `None` while it signs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerSignInUse {
    pub pool: String,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveUse {
    pub device: String,
    /// `None` when the drive has no usable SMART result.
    pub passed: Option<bool>,
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
    /// When a firing alert's input became permanently unavailable, for the no-data resolve. Separate from `timer`, which
    /// belongs to the fire and clear conditions.
    unknown_since: Option<u64>,
}

impl Track {
    fn ok() -> Track {
        Track {
            state: AlertState::Ok,
            timer: None,
            firing: None,
            unknown: None,
            unknown_since: None,
        }
    }
}

/// The alert states of every rule and label set.
#[derive(Debug, Default)]
pub struct Evaluator {
    tracks: BTreeMap<(RuleId, Labels), Track>,
    /// The rules whose source is unknown at the last tick, with why. A rule that has no alert shows here as one unknown entry.
    unknown_rules: BTreeMap<RuleId, UnknownReason>,
}

impl Evaluator {
    /// An evaluator holding the stored firing alerts as firing, with fresh timers, unknown until their first reading. A firing
    /// alert of a rule that `rules` disables, or that this build lacks, is not loaded: its resolve, `rule-disabled`, is returned
    /// at `now` instead. Two stored firings of one rule and label set (a dropped resolve left the older one open) keep the newer
    /// one; the older resolves once at `now` with `cleared`, not `rule-disabled` or `subject-gone`.
    pub fn load(rules: &Rules, firing: Vec<AlertEvent>, now: u64) -> (Evaluator, Vec<AlertEvent>) {
        let mut evaluator = Evaluator::default();
        let mut resolves = Vec::new();
        for event in firing {
            // A rule this build does not have, or a disabled one, can never evaluate its alert: it resolves once.
            let id = RuleId::parse(&event.rule)
                .filter(|id| rules.0.iter().any(|rule| rule.id == *id && rule.enabled));
            let Some(id) = id else {
                resolves.push(resolution(event, None, ResolveReason::RuleDisabled, now));
                continue;
            };
            let key = (id, event.labels.clone());
            // Two stored firings of one key exist when a dropped resolve left the older one open (a queue overflow). The newer is
            // kept and the older resolves once, now, so no open firing is lost without its resolve.
            let event = match evaluator.tracks.remove(&key).and_then(|track| track.firing) {
                None => event,
                Some(held) => {
                    let (keep, drop) =
                        if (held.seq, held.started_at) >= (event.seq, event.started_at) {
                            (held, event)
                        } else {
                            (event, held)
                        };
                    resolves.push(resolution(drop, None, ResolveReason::Cleared, now));
                    keep
                }
            };
            evaluator.tracks.insert(
                key,
                Track {
                    state: AlertState::Firing,
                    timer: None,
                    firing: Some(event),
                    unknown: Some(UnknownReason::Missing),
                    unknown_since: None,
                },
            );
        }
        (evaluator, resolves)
    }

    /// Evaluates every enabled rule at `now`, and returns the transitions as events, in order.
    pub fn step(&mut self, rules: &Rules, inputs: &Inputs, now: u64) -> Vec<AlertEvent> {
        let mut events = Vec::new();
        for rule in rules.0.iter().filter(|rule| rule.enabled) {
            match source(rule.id, inputs, now) {
                Source::Unknown(reason) => {
                    self.unknown_rules.insert(rule.id, reason);
                    for ((id, _), track) in self.tracks.iter_mut() {
                        if *id == rule.id
                            && let Some(event) = unknown(rule, track, reason, now)
                        {
                            events.push(event);
                        }
                    }
                }
                Source::Known(readings) => {
                    self.unknown_rules.remove(&rule.id);
                    // A subject the fresh reading no longer has. A firing alert resolves once, `subject-gone`; any other
                    // track is dropped, so a pending alert simply starts over if its subject returns.
                    let gone: Vec<Labels> = self
                        .tracks
                        .keys()
                        .filter(|(id, labels)| *id == rule.id && !readings.contains_key(labels))
                        .map(|(_, labels)| labels.clone())
                        .collect();
                    for labels in gone {
                        if let Some(firing) = self
                            .tracks
                            .remove(&(rule.id, labels))
                            .and_then(|track| track.firing)
                        {
                            events.push(resolution(firing, None, ResolveReason::SubjectGone, now));
                        }
                    }
                    for (labels, value) in readings {
                        // A drive that never had a SMART result is not tracked. One that already is keeps its track, which
                        // holds it as unknown.
                        if rule.id == RuleId::SmartWarning
                            && value.is_none()
                            && !self.tracks.contains_key(&(rule.id, labels.clone()))
                        {
                            continue;
                        }
                        let track = self
                            .tracks
                            .entry((rule.id, labels.clone()))
                            .or_insert_with(Track::ok);
                        events.extend(apply(rule, &labels, track, value, now));
                    }
                }
            }
        }
        events
    }

    /// Gives the firing events among `events` the sequence numbers they were committed with. A firing event matches the track
    /// whose firing has the same rule, labels and start, and that has no sequence number yet.
    pub fn committed(&mut self, events: &[AlertEvent], seqs: &[u64]) {
        for (event, seq) in events.iter().zip(seqs) {
            if event.state != EventState::Firing {
                continue;
            }
            let Some(id) = RuleId::parse(&event.rule) else {
                continue;
            };
            let Some(track) = self.tracks.get_mut(&(id, event.labels.clone())) else {
                continue;
            };
            if let Some(firing) = track
                .firing
                .as_mut()
                .filter(|firing| firing.seq.is_none() && firing.started_at == event.started_at)
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

    /// The alerts whose newest reading is unknown, and each unknown rule that has no alert of its own.
    pub fn unknown(&self) -> Vec<UnknownAlert> {
        let mut unknown: Vec<UnknownAlert> = self
            .tracks
            .iter()
            .filter_map(|((id, labels), track)| {
                track.unknown.map(|reason| UnknownAlert {
                    rule: id.as_str().to_string(),
                    labels: labels.clone(),
                    state: track.state,
                    reason,
                })
            })
            .collect();
        for (id, reason) in &self.unknown_rules {
            if !self.tracks.keys().any(|(track_id, _)| track_id == id) {
                unknown.push(UnknownAlert {
                    rule: id.as_str().to_string(),
                    labels: Labels::new(),
                    state: AlertState::Ok,
                    reason: *reason,
                });
            }
        }
        unknown
    }
}

/// What a rule reads at one tick.
type Readings = BTreeMap<Labels, Option<f64>>;

enum Source {
    /// The source is missing or too old, so every alert of the rule is unknown.
    Unknown(UnknownReason),
    /// Each label set the fresh reading has, with its value, which is `None` when that one value is unknown.
    Known(Readings),
}

fn source(id: RuleId, inputs: &Inputs, now: u64) -> Source {
    let source = source_of(id, inputs, now);
    if matches!(source, Source::Unknown(_)) && inputs.unavailable.contains(&id) {
        Source::Unknown(UnknownReason::Unavailable)
    } else {
        source
    }
}

fn source_of(id: RuleId, inputs: &Inputs, now: u64) -> Source {
    match id {
        RuleId::DiskPoolFull => read(&inputs.pools, SAMPLE_MAX_AGE_MS, now, |pools| {
            pools
                .iter()
                .map(|pool| {
                    (
                        labels_of([("pool", pool.name.as_str())]),
                        percent_used(pool),
                    )
                })
                .collect()
        }),
        RuleId::MemPsiHigh => read(&inputs.memory_some_avg10, SAMPLE_MAX_AGE_MS, now, |value| {
            single(Some(*value))
        }),
        RuleId::CpuHot => read(&inputs.cpu_temp_c, SAMPLE_MAX_AGE_MS, now, |value| {
            single(Some(*value))
        }),
        RuleId::ServiceDown => read(&inputs.components, HEALTH_MAX_AGE_MS, now, |components| {
            components
                .iter()
                .map(|c| {
                    (
                        labels_of([("component", c.name.as_str())]),
                        Some(bad_as(!c.ok)),
                    )
                })
                .collect()
        }),
        RuleId::Drift => read(&inputs.watch, WATCH_MAX_AGE_MS, now, |watch| {
            let total = watch
                .drift
                .values()
                .fold(0u64, |sum, count| sum.saturating_add(*count));
            single(Some(total as f64))
        }),
        RuleId::UpdateFailed => read(&inputs.update, OSD_MAX_AGE_MS, now, |update| {
            single(Some(bad_as(update.failed)))
        }),
        RuleId::CrashLoop => read(&inputs.instances, SAMPLE_MAX_AGE_MS, now, |instances| {
            instances
                .iter()
                .map(|instance| {
                    (
                        labels_of([
                            ("project", instance.project.as_str()),
                            ("incus_instance", instance.name.as_str()),
                        ]),
                        Some(instance.starts_10m as f64),
                    )
                })
                .collect()
        }),
        RuleId::SmartWarning => read(&inputs.drives, SMART_MAX_AGE_MS, now, |drives| {
            drives
                .iter()
                .map(|drive| {
                    (
                        labels_of([("device", drive.device.as_str())]),
                        drive.passed.map(|passed| bad_as(!passed)),
                    )
                })
                .collect()
        }),
        RuleId::GpuVramHigh => read(&inputs.gpus, SAMPLE_MAX_AGE_MS, now, |gpus| {
            gpus.iter()
                .map(|gpu| (labels_of([("gpu", gpu.pci.as_str())]), gpu.vram_used_pct))
                .collect()
        }),
        RuleId::HostNetwork => read(
            &inputs.host_network,
            HOST_NETWORK_MAX_AGE_MS,
            now,
            |items| {
                items
                    .iter()
                    .map(|drift| (labels_of([("item", drift.item.as_str())]), Some(1.0)))
                    .collect()
            },
        ),
        RuleId::RunnerSignIn => read(
            &inputs.runner_signin,
            RUNNER_SIGNIN_MAX_AGE_MS,
            now,
            |pools| {
                pools
                    .iter()
                    .map(|pool| {
                        (
                            labels_of([("pool", pool.pool.as_str())]),
                            Some(bad_as(pool.reason.is_some())),
                        )
                    })
                    .collect()
            },
        ),
        RuleId::GpuGone => read(&inputs.gpus_gone, SAMPLE_MAX_AGE_MS, now, |gone| {
            gone.iter()
                .map(|gpu| (labels_of([("gpu", gpu.pci.as_str())]), Some(1.0)))
                .collect()
        }),
    }
}

/// A source's readings when it is present and not older than `max_age`, else why it is unknown.
fn read<T>(
    reading: &Option<Timed<T>>,
    max_age: u64,
    now: u64,
    known: impl FnOnce(&T) -> Readings,
) -> Source {
    match reading {
        None => Source::Unknown(UnknownReason::Missing),
        Some(timed) if now.saturating_sub(timed.at) > max_age => {
            Source::Unknown(UnknownReason::Stale)
        }
        Some(timed) => Source::Known(known(&timed.value)),
    }
}

/// The readings of a rule with one subject, which has no labels of its own.
fn single(value: Option<f64>) -> Readings {
    BTreeMap::from([(Labels::new(), value)])
}

fn labels_of<const N: usize>(pairs: [(&str, &str); N]) -> Labels {
    pairs
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

/// 1 when `bad`, else 0: the reading of a yes-or-no rule.
fn bad_as(bad: bool) -> f64 {
    if bad { 1.0 } else { 0.0 }
}

/// Used over total, in percent. `None` when either is unknown or the pool has no size.
fn percent_used(pool: &PoolUse) -> Option<f64> {
    let (used, total) = (pool.used_bytes?, pool.total_bytes?);
    (total > 0).then(|| used as f64 / total as f64 * 100.0)
}

/// Unknown data. A non-firing track keeps its state and its timer restarts. A firing track stays firing and unknown; only
/// when the reason is a reading the node can never provide, and after `clear_for_s` without a fresh reading, it resolves
/// once with `no data`. A transient gap never ends the alert. While it stays unknown, the reason is shown with the firing
/// alert.
fn unknown(rule: &Rule, track: &mut Track, reason: UnknownReason, now: u64) -> Option<AlertEvent> {
    track.unknown = Some(reason);
    if track.state != AlertState::Firing {
        track.timer = None;
        track.unknown_since = None;
        return None;
    }
    // A transient gap never ends a firing alert. Only a reading this node can never provide does, and only after the
    // clear window, measured from its own clock so a later clear timer keeps its full length.
    if reason != UnknownReason::Unavailable {
        track.timer = None;
        track.unknown_since = None;
        return None;
    }
    track.timer = None;
    let since = *track.unknown_since.get_or_insert(now);
    if !held(now, since, rule.clear_for_s) {
        return None;
    }
    let firing = track.firing.take()?;
    track.state = AlertState::Ok;
    track.timer = None;
    track.unknown = None;
    track.unknown_since = None;
    Some(resolution(firing, None, ResolveReason::NoData, now))
}

fn held(now: u64, since: u64, seconds: u64) -> bool {
    now.saturating_sub(since) >= seconds.saturating_mul(1_000)
}

/// One reading of one alert at `now`, and the event it causes, if any.
fn apply(
    rule: &Rule,
    labels: &Labels,
    track: &mut Track,
    value: Option<f64>,
    now: u64,
) -> Option<AlertEvent> {
    let Some(value) = value.filter(|value| value.is_finite()) else {
        return unknown(rule, track, UnknownReason::Missing, now);
    };
    track.unknown = None;
    track.unknown_since = None;
    match track.state {
        AlertState::Ok | AlertState::Pending if value > rule.threshold => {
            track.state = AlertState::Pending;
            let since = *track.timer.get_or_insert(now);
            if !held(now, since, rule.for_s) {
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
            if !held(now, since, rule.clear_for_s) {
                return None;
            }
            let firing = track.firing.take()?;
            track.state = AlertState::Ok;
            track.timer = None;
            Some(resolution(firing, Some(value), ResolveReason::Cleared, now))
        }
        AlertState::Firing => {
            track.timer = None;
            None
        }
    }
}

/// The resolve of `firing`: the same alert with its own time and reason. It has no `seq` until it is committed.
fn resolution(
    firing: AlertEvent,
    value: Option<f64>,
    reason: ResolveReason,
    now: u64,
) -> AlertEvent {
    AlertEvent {
        seq: None,
        state: EventState::Resolved,
        value,
        resolved_at: Some(now),
        reason: Some(reason),
        at: now,
        ..firing
    }
}
