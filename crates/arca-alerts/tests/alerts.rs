//! The alert evaluator, the rules file, the store-failure queue and the read-only tick, on a synthetic clock. Every file is in a
//! temporary directory. Incus is a fake that records every request.
//!
//! Test names are module paths, so `cargo test -p arca-alerts <module path>` selects exactly the tests under it.

use std::collections::BTreeMap;
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use arca_alerts::eval::{
    Component, ComponentName, DriveUse, Evaluator, GpuGone, GpuUse, HEALTH_MAX_AGE_MS,
    HOST_NETWORK_MAX_AGE_MS, HostNetworkItem, Inputs, InstanceUse, OSD_MAX_AGE_MS, PoolUse, RuleId,
    Rules, SAMPLE_MAX_AGE_MS, SMART_MAX_AGE_MS, Severity, Timed, UpdateUse, WATCH_MAX_AGE_MS,
    WatchReading,
};
use arca_alerts::rules::{AlertsFile, Probes, RulesError};
use arca_alerts::{
    Engine, QUEUE_LIMIT, STORE_COMMIT_SLOW, STORE_WRITE_FAILED, TickStore, UnknownReason, export,
    inputs,
};
use arca_incus::{FakeIncus, FakeState, UnixIncus};
use arca_metrics::{Sampler, SamplerConfig};
use arca_store::{AlertEvent, EventState, Point, ResolveReason, RollupReport, Store, StoreError};
use serde_json::json;

/// The synthetic clock's start: 2026-11-01T00:00:00Z, in epoch milliseconds.
const T0: u64 = 1_793_491_200_000;

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A directory under the default temporary directory, removed when the test ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("arca-alerts-{}-{n}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("the scratch directory is created");
        Scratch(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The maximum age of a rule's source, in milliseconds.
fn max_age(id: RuleId) -> u64 {
    match id {
        RuleId::DiskPoolFull
        | RuleId::MemPsiHigh
        | RuleId::CpuHot
        | RuleId::CrashLoop
        | RuleId::GpuVramHigh
        | RuleId::GpuGone => SAMPLE_MAX_AGE_MS,
        RuleId::ServiceDown => HEALTH_MAX_AGE_MS,
        RuleId::Drift => WATCH_MAX_AGE_MS,
        RuleId::UpdateFailed => OSD_MAX_AGE_MS,
        RuleId::SmartWarning => SMART_MAX_AGE_MS,
        RuleId::HostNetwork => HOST_NETWORK_MAX_AGE_MS,
        RuleId::RunnerSignIn => arca_alerts::eval::RUNNER_SIGNIN_MAX_AGE_MS,
    }
}

fn pool(name: &str, used: u64) -> PoolUse {
    PoolUse {
        name: name.to_string(),
        used_bytes: Some(used),
        total_bytes: Some(100),
    }
}

fn instance(project: &str, name: &str, starts_10m: u64) -> InstanceUse {
    InstanceUse {
        project: project.to_string(),
        name: name.to_string(),
        starts_10m,
    }
}

/// The inputs of one rule at a reading of `value`, taken at `at`. Every other source is missing.
fn reading_of(id: RuleId, value: f64, at: u64) -> Inputs {
    let mut inputs = Inputs {
        at,
        ..Inputs::default()
    };
    match id {
        RuleId::DiskPoolFull => {
            inputs.pools = Some(Timed {
                at,
                value: vec![pool("default", value as u64)],
            });
        }
        RuleId::MemPsiHigh => inputs.memory_some_avg10 = Some(Timed { at, value }),
        RuleId::CpuHot => inputs.cpu_temp_c = Some(Timed { at, value }),
        RuleId::ServiceDown => {
            inputs.components = Some(Timed {
                at,
                value: vec![Component {
                    name: ComponentName::Incus,
                    ok: value == 0.0,
                }],
            });
        }
        RuleId::Drift => {
            inputs.watch = Some(Timed {
                at,
                value: WatchReading {
                    drift: BTreeMap::from([("marked_without_entry".to_string(), value as u64)]),
                },
            });
        }
        RuleId::UpdateFailed => {
            inputs.update = Some(Timed {
                at,
                value: UpdateUse {
                    failed: value != 0.0,
                },
            });
        }
        RuleId::CrashLoop => {
            inputs.instances = Some(Timed {
                at,
                value: vec![instance("proj1", "db1", value as u64)],
            });
        }
        RuleId::SmartWarning => {
            inputs.drives = Some(Timed {
                at,
                value: vec![DriveUse {
                    device: "disk1".to_string(),
                    passed: Some(value == 0.0),
                }],
            });
        }
        RuleId::GpuVramHigh => {
            inputs.gpus = Some(Timed {
                at,
                value: vec![GpuUse {
                    pci: "0000:01:00.0".to_string(),
                    vram_used_pct: Some(value),
                }],
            });
        }
        RuleId::GpuGone => {
            inputs.gpus_gone = Some(Timed {
                at,
                value: if value > 0.0 {
                    vec![GpuGone {
                        pci: "0000:01:00.0".to_string(),
                    }]
                } else {
                    Vec::new()
                },
            });
        }
        RuleId::HostNetwork => {
            inputs.host_network = Some(Timed {
                at,
                value: if value > 0.0 {
                    vec![HostNetworkItem {
                        item: "ipv4-forwarding".to_string(),
                    }]
                } else {
                    Vec::new()
                },
            });
        }
        RuleId::RunnerSignIn => {
            inputs.runner_signin = Some(Timed {
                at,
                value: vec![arca_alerts::eval::RunnerSignInUse {
                    pool: "acme".to_string(),
                    reason: (value > 0.0).then(|| "app-not-installed".to_string()),
                }],
            });
        }
    }
    inputs
}

/// The same reading as [`reading_of`], taken so old that the rule's source is stale.
fn stale_of(id: RuleId, value: f64, now: u64) -> Inputs {
    reading_of(id, value, now - max_age(id) - 1_000)
}

/// A rule's reading kinds for the table tests: a value over the threshold, one inside the clearing band (`None` when the rule
/// has none), and one at or below the clear margin.
struct Case {
    id: RuleId,
    high: f64,
    band: Option<f64>,
    low: f64,
}

const CASES: [Case; 12] = [
    Case {
        id: RuleId::DiskPoolFull,
        high: 95.0,
        band: Some(88.0),
        low: 84.0,
    },
    Case {
        id: RuleId::MemPsiHigh,
        high: 30.0,
        band: Some(22.0),
        low: 20.0,
    },
    Case {
        id: RuleId::CpuHot,
        high: 90.0,
        band: Some(82.0),
        low: 80.0,
    },
    Case {
        id: RuleId::ServiceDown,
        high: 1.0,
        band: None,
        low: 0.0,
    },
    Case {
        id: RuleId::Drift,
        high: 3.0,
        band: None,
        low: 0.0,
    },
    Case {
        id: RuleId::UpdateFailed,
        high: 1.0,
        band: None,
        low: 0.0,
    },
    Case {
        id: RuleId::CrashLoop,
        high: 3.0,
        band: Some(1.0),
        low: 0.0,
    },
    Case {
        id: RuleId::SmartWarning,
        high: 1.0,
        band: None,
        low: 0.0,
    },
    Case {
        id: RuleId::GpuVramHigh,
        high: 98.0,
        band: Some(91.0),
        low: 89.0,
    },
    Case {
        id: RuleId::GpuGone,
        high: 1.0,
        band: None,
        low: 0.0,
    },
    Case {
        id: RuleId::HostNetwork,
        high: 1.0,
        band: None,
        low: 0.0,
    },
    Case {
        id: RuleId::RunnerSignIn,
        high: 1.0,
        band: None,
        low: 0.0,
    },
];

/// Whether a case's rule uses the default 60 s fire window. The GPU VRAM rule holds for five minutes, so the default-timing
/// tests skip it and the GPU tests below drive its own clock. A rule whose alert ends by its subject leaving the reading
/// (`gpu-gone`, `host-network-drift`) has no clearing window to hold, so it is skipped too.
fn default_timing(id: RuleId) -> bool {
    !matches!(
        id,
        RuleId::GpuVramHigh | RuleId::GpuGone | RuleId::HostNetwork
    )
}

/// The reason the newest reading of `id` is unknown, if it is.
fn unknown_of(eval: &Evaluator, id: RuleId) -> Option<UnknownReason> {
    eval.unknown()
        .into_iter()
        .find(|alert| alert.rule == id.as_str())
        .map(|alert| alert.reason)
}

/// A stored firing event, as the store gives it back before any resolve.
fn firing_event(rule: &str, labels: &[(&str, &str)], at: u64) -> AlertEvent {
    AlertEvent {
        seq: None,
        alert_id: None,
        rule: rule.to_string(),
        severity: "critical".to_string(),
        state: EventState::Firing,
        labels: labels
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect(),
        value: Some(95.0),
        threshold: Some(90.0),
        started_at: at,
        resolved_at: None,
        reason: None,
        at,
    }
}

/// The default rules with one rule disabled.
fn rules_with_disabled(id: RuleId) -> Rules {
    let mut rules = Rules::default();
    for rule in rules.0.iter_mut().filter(|rule| rule.id == id) {
        rule.enabled = false;
    }
    rules
}

/// Every alert event of a store, oldest first, read on a read-only connection.
fn events_of(dir: &Path) -> Vec<AlertEvent> {
    Store::open_read_only(dir)
        .expect("a reader opens")
        .events_after(0, 10_000)
        .expect("the events are read")
}

/// A store that fails or takes too long on demand, and is otherwise the real store.
#[derive(Clone, Default)]
struct Switches {
    fail: std::sync::Arc<AtomicBool>,
    slow: std::sync::Arc<AtomicBool>,
}

fn set(flag: &AtomicBool, on: bool) {
    flag.store(on, Ordering::SeqCst);
}

struct Flaky {
    inner: Store,
    switches: Switches,
}

impl TickStore for Flaky {
    fn commit_tick(
        &mut self,
        at: u64,
        points: &[Point],
        events: &[AlertEvent],
    ) -> Result<Vec<u64>, StoreError> {
        if self.switches.fail.load(Ordering::SeqCst) {
            return Err(StoreError::Io("no space left on device".to_string()));
        }
        if self.switches.slow.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(1_100));
        }
        self.inner.commit_tick(at, points, events)
    }

    fn firing(&self) -> Result<Vec<AlertEvent>, StoreError> {
        self.inner.firing()
    }

    fn rollup(&mut self, now_ms: u64) -> Result<RollupReport, StoreError> {
        self.inner.rollup(now_ms)
    }
}

/// An engine over a new store in `dir`, started at [`T0`].
fn engine_on(dir: &Path, switches: &Switches, rules: Rules) -> Engine<Flaky> {
    let inner = Store::open(dir, T0).expect("the store opens").store;
    let flaky = Flaky {
        inner,
        switches: switches.clone(),
    };
    Engine::start(flaky, rules, false, T0).expect("the engine starts")
}

/// `count` pools named `pool0000`.., each at `used` of 100 bytes.
fn many_pools(count: u64, used: impl Fn(u64) -> u64, at: u64) -> Inputs {
    Inputs {
        at,
        pools: Some(Timed {
            at,
            value: (0..count)
                .map(|i| pool(&format!("pool{i:04}"), used(i)))
                .collect(),
        }),
        ..Inputs::default()
    }
}

mod eval {
    use super::*;

    mod hysteresis {
        use super::*;

        #[test]
        fn each_default_rule_fires_at_60_s_and_clears_at_120_s_with_hysteresis() {
            assert_eq!(CASES.len(), RuleId::ALL.len(), "every rule has a case");
            let rules = Rules::default();
            for case in CASES.iter().filter(|case| default_timing(case.id)) {
                let id = case.id;
                let mut eval = Evaluator::default();
                let feed = |eval: &mut Evaluator, t: u64, value: f64| {
                    eval.step(&rules, &reading_of(id, value, T0 + t), T0 + t)
                };
                // Over the threshold from 0 s: pending, not firing at 55 s, firing at 60 s.
                for t in (0..=55_000).step_by(5_000) {
                    assert!(feed(&mut eval, t, case.high).is_empty(), "{id:?} at {t} ms");
                }
                let fired = feed(&mut eval, 60_000, case.high);
                assert_eq!(fired.len(), 1, "{id:?} fires at 60 s");
                assert_eq!(fired[0].rule, id.as_str());
                assert_eq!(fired[0].state, EventState::Firing);
                // Inside the clearing band the alert keeps firing. A rule without a band stays over its threshold.
                for t in (65_000..=180_000).step_by(5_000) {
                    let value = case.band.unwrap_or(case.high);
                    assert!(feed(&mut eval, t, value).is_empty(), "{id:?} at {t} ms");
                }
                // At or below the margin from 185 s: not cleared at 115 s, cleared at 120 s, with one resolve.
                for t in (185_000..=300_000).step_by(5_000) {
                    assert!(
                        feed(&mut eval, t, case.low).is_empty(),
                        "{id:?} cleared early at {t} ms"
                    );
                }
                let cleared = feed(&mut eval, 305_000, case.low);
                assert_eq!(cleared.len(), 1, "{id:?} clears at 120 s");
                assert_eq!(cleared[0].state, EventState::Resolved);
                assert_eq!(cleared[0].reason, Some(ResolveReason::Cleared));
                assert!(eval.firing().is_empty(), "{id:?} is no longer firing");
            }
        }
    }

    mod unknown {
        use super::*;

        #[test]
        fn a_pending_alert_keeps_its_state_through_a_gap_and_its_timer_restarts() {
            let rules = Rules::default();
            for case in CASES.iter().filter(|case| default_timing(case.id)) {
                let id = case.id;
                for stale in [false, true] {
                    let mut eval = Evaluator::default();
                    assert!(
                        eval.step(&rules, &reading_of(id, case.high, T0), T0)
                            .is_empty()
                    );
                    // A gap at 20 s, missing or stale: no event, the alert is still pending, and the reason is shown.
                    let gap = if stale {
                        stale_of(id, case.high, T0 + 20_000)
                    } else {
                        Inputs::default()
                    };
                    assert!(eval.step(&rules, &gap, T0 + 20_000).is_empty(), "{id:?}");
                    let reason = if stale {
                        UnknownReason::Stale
                    } else {
                        UnknownReason::Missing
                    };
                    assert_eq!(unknown_of(&eval, id), Some(reason), "{id:?}");
                    assert!(eval.firing().is_empty(), "{id:?} is pending, not firing");
                    // The timer restarted at the gap: a known reading at 30 s fires at 90 s, not at 60 s.
                    for t in [30_000, 40_000, 50_000, 60_000] {
                        let reading = reading_of(id, case.high, T0 + t);
                        assert!(
                            eval.step(&rules, &reading, T0 + t).is_empty(),
                            "{id:?} at {t} ms"
                        );
                    }
                    let fired =
                        eval.step(&rules, &reading_of(id, case.high, T0 + 90_000), T0 + 90_000);
                    assert_eq!(
                        fired.len(),
                        1,
                        "{id:?} fires 60 s after the reading resumes"
                    );
                }
            }
        }

        #[test]
        fn a_firing_alert_stays_firing_and_shows_unknown_through_a_gap_and_its_clear_timer_restarts()
         {
            let rules = Rules::default();
            for case in CASES.iter().filter(|case| default_timing(case.id)) {
                let id = case.id;
                let mut eval = Evaluator::default();
                for t in (0..=60_000).step_by(5_000) {
                    eval.step(&rules, &reading_of(id, case.high, T0 + t), T0 + t);
                }
                assert_eq!(eval.firing().len(), 1, "{id:?} fires at 60 s");
                // Missing at 70 s, then stale at 80 s: firing, unknown, no event, the reason shown each time.
                assert!(
                    eval.step(&rules, &Inputs::default(), T0 + 70_000)
                        .is_empty()
                );
                assert!(eval.firing()[0].unknown);
                assert_eq!(unknown_of(&eval, id), Some(UnknownReason::Missing));
                assert!(
                    eval.step(&rules, &stale_of(id, case.high, T0 + 80_000), T0 + 80_000)
                        .is_empty()
                );
                assert_eq!(unknown_of(&eval, id), Some(UnknownReason::Stale));
                // A known reading over the threshold at 90 s clears the unknown, still no event.
                assert!(
                    eval.step(&rules, &reading_of(id, case.high, T0 + 90_000), T0 + 90_000)
                        .is_empty()
                );
                assert!(!eval.firing()[0].unknown);
                assert_eq!(unknown_of(&eval, id), None);
                // The clear timer starts at 100 s, and a gap at 145 s restarts it. Clearing at 220 s would be the early bug.
                for t in (100_000..=140_000).step_by(5_000) {
                    assert!(
                        eval.step(&rules, &reading_of(id, case.low, T0 + t), T0 + t)
                            .is_empty()
                    );
                }
                assert!(
                    eval.step(&rules, &Inputs::default(), T0 + 145_000)
                        .is_empty()
                );
                for t in (150_000..=260_000).step_by(5_000) {
                    assert!(
                        eval.step(&rules, &reading_of(id, case.low, T0 + t), T0 + t)
                            .is_empty(),
                        "{id:?} at {t} ms"
                    );
                }
                let cleared = eval.step(
                    &rules,
                    &reading_of(id, case.low, T0 + 270_000),
                    T0 + 270_000,
                );
                assert_eq!(cleared.len(), 1, "{id:?} clears 120 s after the restart");
                assert_eq!(cleared[0].reason, Some(ResolveReason::Cleared));
            }
        }

        #[test]
        fn after_a_restart_a_stored_firing_alert_is_firing_and_unknown_until_its_first_reading() {
            let rules = Rules::default();
            let stored = firing_event("disk-pool-full", &[("pool", "default")], T0);
            let (mut eval, resolves) = Evaluator::load(&rules, vec![stored], T0 + 500_000);
            assert!(resolves.is_empty(), "an enabled rule resolves nothing");
            assert_eq!(eval.firing().len(), 1);
            assert!(eval.firing()[0].unknown);
            assert_eq!(
                unknown_of(&eval, RuleId::DiskPoolFull),
                Some(UnknownReason::Missing)
            );
            let first = reading_of(RuleId::DiskPoolFull, 95.0, T0 + 500_000);
            assert!(eval.step(&rules, &first, T0 + 500_000).is_empty());
            assert!(
                !eval.firing()[0].unknown,
                "the first reading clears the unknown"
            );
        }
    }

    mod duplicate_firings {
        use super::*;

        #[test]
        fn two_stored_firings_of_one_key_keep_the_newer_and_resolve_the_older_once() {
            let rules = Rules::default();
            let mut older = firing_event("disk-pool-full", &[("pool", "default")], T0);
            older.seq = Some(4);
            let mut newer = firing_event("disk-pool-full", &[("pool", "default")], T0 + 300_000);
            newer.seq = Some(9);
            let now = T0 + 600_000;
            // The stored order is not fixed, so both orders must give the same result.
            for stored in [
                vec![older.clone(), newer.clone()],
                vec![newer.clone(), older.clone()],
            ] {
                let (eval, resolves) = Evaluator::load(&rules, stored, now);
                assert_eq!(resolves.len(), 1, "exactly one resolve for the older");
                assert_eq!(resolves[0].state, EventState::Resolved);
                assert_eq!(resolves[0].reason, Some(ResolveReason::Cleared));
                assert_eq!(resolves[0].started_at, T0, "the older firing's start");
                assert_eq!(resolves[0].resolved_at, Some(now));
                assert_eq!(eval.firing().len(), 1, "one open firing is kept");
                assert_eq!(
                    eval.firing()[0].event.started_at,
                    T0 + 300_000,
                    "the newer one"
                );
            }
        }
    }

    mod rule_disabled {
        use super::*;

        #[test]
        fn a_stored_firing_alert_of_a_disabled_rule_resolves_once_at_start() {
            let scratch = Scratch::new("rule-disabled");
            let mut store = Store::open(scratch.path(), T0)
                .expect("the store opens")
                .store;
            store
                .commit_tick(T0, &[], &[firing_event("cpu-hot", &[], T0)])
                .expect("the firing commits");
            drop(store);

            let store = Store::open(scratch.path(), T0 + 1_000)
                .expect("the store opens")
                .store;
            let mut engine = Engine::start(
                store,
                rules_with_disabled(RuleId::CpuHot),
                false,
                T0 + 1_000,
            )
            .expect("the engine starts");
            let events = events_of(scratch.path());
            let firing = events
                .iter()
                .find(|event| event.state == EventState::Firing)
                .expect("the firing is stored");
            let resolves: Vec<&AlertEvent> = events
                .iter()
                .filter(|event| event.state == EventState::Resolved)
                .collect();
            assert_eq!(resolves.len(), 1, "exactly one resolve");
            assert_eq!(resolves[0].reason, Some(ResolveReason::RuleDisabled));
            assert_eq!(resolves[0].rule, "cpu-hot");
            assert_eq!(
                resolves[0].alert_id, firing.alert_id,
                "it closes the stored firing"
            );

            let snapshot = engine.tick(&Inputs::default(), Vec::new(), T0 + 6_000);
            assert_eq!(snapshot.disabled, vec!["cpu-hot".to_string()]);
            assert!(snapshot.firing.is_empty());
            assert_eq!(events_of(scratch.path()).len(), 2, "no later event for it");
        }
    }

    mod rule_missing {
        use super::*;

        #[test]
        fn a_stored_firing_alert_of_a_rule_this_build_lacks_resolves_once_at_load() {
            let rules = Rules::default();
            let stored = firing_event("retired-rule", &[], T0);
            let (eval, resolves) = Evaluator::load(&rules, vec![stored], T0 + 1_000);
            assert_eq!(resolves.len(), 1, "it cannot stay firing forever");
            assert_eq!(resolves[0].reason, Some(ResolveReason::RuleDisabled));
            assert!(eval.firing().is_empty());
        }
    }

    mod subject_gone {
        use super::*;

        #[test]
        fn a_firing_smart_alert_resolves_once_when_its_drive_is_gone() {
            let rules = Rules::default();
            let mut eval = Evaluator::default();
            for t in (0..=60_000).step_by(5_000) {
                eval.step(
                    &rules,
                    &reading_of(RuleId::SmartWarning, 1.0, T0 + t),
                    T0 + t,
                );
            }
            assert_eq!(eval.firing().len(), 1, "the failed drive fires at 60 s");
            let gone = Inputs {
                at: T0 + 65_000,
                drives: Some(Timed {
                    at: T0 + 65_000,
                    value: Vec::new(),
                }),
                ..Inputs::default()
            };
            let resolved = eval.step(&rules, &gone, T0 + 65_000);
            assert_eq!(resolved.len(), 1);
            assert_eq!(resolved[0].state, EventState::Resolved);
            assert_eq!(resolved[0].reason, Some(ResolveReason::SubjectGone));
            assert_eq!(
                resolved[0].labels.get("device").map(String::as_str),
                Some("disk1")
            );
            assert!(eval.step(&rules, &gone, T0 + 70_000).is_empty(), "once");
            assert!(eval.firing().is_empty());
        }

        #[test]
        fn a_firing_smart_alert_stays_firing_and_unknown_while_its_drive_has_no_result() {
            let rules = Rules::default();
            let mut eval = Evaluator::default();
            for t in (0..=60_000).step_by(5_000) {
                eval.step(
                    &rules,
                    &reading_of(RuleId::SmartWarning, 1.0, T0 + t),
                    T0 + t,
                );
            }
            assert_eq!(eval.firing().len(), 1, "the failed drive fires at 60 s");
            let no_result = |t: u64| Inputs {
                at: T0 + t,
                drives: Some(Timed {
                    at: T0 + t,
                    value: vec![DriveUse {
                        device: "disk1".to_string(),
                        passed: None,
                    }],
                }),
                ..Inputs::default()
            };
            for t in (65_000..=180_000).step_by(5_000) {
                assert!(
                    eval.step(&rules, &no_result(t), T0 + t).is_empty(),
                    "no event while the drive has no result, at {t} ms"
                );
            }
            let firing = eval.firing();
            assert_eq!(firing.len(), 1, "still firing");
            assert!(firing[0].unknown, "held as unknown");
            assert_eq!(
                unknown_of(&eval, RuleId::SmartWarning),
                Some(UnknownReason::Missing)
            );
            let gone = Inputs {
                at: T0 + 185_000,
                drives: Some(Timed {
                    at: T0 + 185_000,
                    value: Vec::new(),
                }),
                ..Inputs::default()
            };
            let resolved = eval.step(&rules, &gone, T0 + 185_000);
            assert_eq!(resolved.len(), 1);
            assert_eq!(resolved[0].state, EventState::Resolved);
            assert_eq!(resolved[0].reason, Some(ResolveReason::SubjectGone));
            assert!(eval.firing().is_empty());
        }

        #[test]
        fn a_drive_with_no_smart_result_from_the_start_creates_no_track_and_no_event() {
            let rules = Rules::default();
            let mut eval = Evaluator::default();
            for t in (0..=180_000).step_by(5_000) {
                let inputs = Inputs {
                    at: T0 + t,
                    drives: Some(Timed {
                        at: T0 + t,
                        value: vec![DriveUse {
                            device: "disk1".to_string(),
                            passed: None,
                        }],
                    }),
                    ..Inputs::default()
                };
                assert!(eval.step(&rules, &inputs, T0 + t).is_empty(), "{t} ms");
            }
            assert!(eval.firing().is_empty(), "nothing fires");
            assert_eq!(unknown_of(&eval, RuleId::SmartWarning), None, "no track");
            assert!(
                eval.unknown()
                    .iter()
                    .all(|alert| alert.rule != "smart-warning"),
                "no smart-warning entry is shown"
            );
        }

        #[test]
        fn a_firing_pool_alert_resolves_once_when_a_fresh_reading_lacks_its_pool() {
            let rules = Rules::default();
            let mut eval = Evaluator::default();
            for t in (0..=60_000).step_by(5_000) {
                eval.step(
                    &rules,
                    &reading_of(RuleId::DiskPoolFull, 95.0, T0 + t),
                    T0 + t,
                );
            }
            let gone = Inputs {
                at: T0 + 65_000,
                pools: Some(Timed {
                    at: T0 + 65_000,
                    value: Vec::new(),
                }),
                ..Inputs::default()
            };
            let resolved = eval.step(&rules, &gone, T0 + 65_000);
            assert_eq!(resolved.len(), 1);
            assert_eq!(resolved[0].state, EventState::Resolved);
            assert_eq!(resolved[0].reason, Some(ResolveReason::SubjectGone));
            assert_eq!(
                resolved[0].labels.get("pool").map(String::as_str),
                Some("default")
            );
            assert!(eval.step(&rules, &gone, T0 + 70_000).is_empty(), "once");
            assert!(eval.firing().is_empty());
        }

        #[test]
        fn a_firing_crash_loop_alert_resolves_once_when_its_instance_is_gone() {
            let rules = Rules::default();
            let mut eval = Evaluator::default();
            for t in (0..=60_000).step_by(5_000) {
                eval.step(&rules, &reading_of(RuleId::CrashLoop, 3.0, T0 + t), T0 + t);
            }
            let gone = Inputs {
                at: T0 + 65_000,
                instances: Some(Timed {
                    at: T0 + 65_000,
                    value: Vec::new(),
                }),
                ..Inputs::default()
            };
            let resolved = eval.step(&rules, &gone, T0 + 65_000);
            assert_eq!(resolved.len(), 1);
            assert_eq!(resolved[0].reason, Some(ResolveReason::SubjectGone));
            assert_eq!(
                resolved[0].labels.get("project").map(String::as_str),
                Some("proj1")
            );
            assert_eq!(
                resolved[0].labels.get("incus_instance").map(String::as_str),
                Some("db1")
            );
            assert!(eval.step(&rules, &gone, T0 + 70_000).is_empty());
        }

        #[test]
        fn a_stale_or_missing_reading_that_lacks_the_subject_resolves_nothing() {
            let rules = Rules::default();
            let mut eval = Evaluator::default();
            for t in (0..=60_000).step_by(5_000) {
                eval.step(
                    &rules,
                    &reading_of(RuleId::DiskPoolFull, 95.0, T0 + t),
                    T0 + t,
                );
            }
            let stale = Inputs {
                at: T0 + 80_000,
                pools: Some(Timed {
                    at: T0 + 80_000 - SAMPLE_MAX_AGE_MS - 1_000,
                    value: Vec::new(),
                }),
                ..Inputs::default()
            };
            assert!(
                eval.step(&rules, &stale, T0 + 80_000).is_empty(),
                "a stale list is not a fresh one"
            );
            assert_eq!(eval.firing().len(), 1);
            assert!(
                eval.step(&rules, &Inputs::default(), T0 + 85_000)
                    .is_empty()
            );
            assert_eq!(eval.firing().len(), 1, "still firing");
        }

        #[test]
        fn a_pending_alert_whose_pool_is_gone_starts_over_without_an_event() {
            let rules = Rules::default();
            let mut eval = Evaluator::default();
            assert!(
                eval.step(&rules, &reading_of(RuleId::DiskPoolFull, 95.0, T0), T0)
                    .is_empty()
            );
            let gone = Inputs {
                at: T0 + 10_000,
                pools: Some(Timed {
                    at: T0 + 10_000,
                    value: Vec::new(),
                }),
                ..Inputs::default()
            };
            assert!(
                eval.step(&rules, &gone, T0 + 10_000).is_empty(),
                "no event for a pending alert"
            );
            assert!(
                eval.step(
                    &rules,
                    &reading_of(RuleId::DiskPoolFull, 95.0, T0 + 20_000),
                    T0 + 20_000
                )
                .is_empty()
            );
            assert!(
                eval.step(
                    &rules,
                    &reading_of(RuleId::DiskPoolFull, 95.0, T0 + 70_000),
                    T0 + 70_000
                )
                .is_empty(),
                "60 s not yet held from 20 s"
            );
            assert_eq!(
                eval.step(
                    &rules,
                    &reading_of(RuleId::DiskPoolFull, 95.0, T0 + 80_000),
                    T0 + 80_000
                )
                .len(),
                1
            );
        }
    }

    #[test]
    fn a_data_gap_does_not_shorten_the_clear_window() {
        let rules = Rules::default();
        let mut eval = Evaluator::default();
        for t in (0..=60_000).step_by(5_000) {
            eval.step(
                &rules,
                &reading_of(RuleId::MemPsiHigh, 30.0, T0 + t),
                T0 + t,
            );
        }
        assert_eq!(eval.firing().len(), 1, "the pressure alert fires at 60 s");

        // 100 s with no data. It is a transient gap, so the alert stays firing and no event is written.
        for t in (65_000..=165_000).step_by(5_000) {
            assert!(
                eval.step(
                    &rules,
                    &Inputs {
                        at: T0 + t,
                        ..Inputs::default()
                    },
                    T0 + t,
                )
                .is_empty(),
                "no event at {t} ms"
            );
        }

        // The first real low reading at 170 s starts the clear window. 15 s later it must not clear.
        for t in [170_000, 185_000] {
            assert!(
                eval.step(&rules, &reading_of(RuleId::MemPsiHigh, 0.0, T0 + t), T0 + t,)
                    .is_empty(),
                "a 15 s low reading does not clear at {t} ms"
            );
        }
        assert_eq!(eval.firing().len(), 1, "still firing after 15 s low");

        // 119 s after the first low reading is not enough; 120 s is.
        assert!(
            eval.step(
                &rules,
                &reading_of(RuleId::MemPsiHigh, 0.0, T0 + 289_000),
                T0 + 289_000,
            )
            .is_empty(),
            "119 s of low readings is not 120"
        );
        let cleared = eval.step(
            &rules,
            &reading_of(RuleId::MemPsiHigh, 0.0, T0 + 290_000),
            T0 + 290_000,
        );
        assert_eq!(cleared.len(), 1);
        assert_eq!(cleared[0].state, EventState::Resolved);
        assert_eq!(cleared[0].reason, Some(ResolveReason::Cleared));
    }

    mod incus_down {
        use super::*;

        #[test]
        fn pools_and_instances_unreadable_for_10_min_write_no_event_and_keep_the_alerts_firing_and_unknown()
         {
            let rules = Rules::default();
            let both = |at: u64| Inputs {
                at,
                pools: Some(Timed {
                    at,
                    value: vec![pool("default", 95)],
                }),
                instances: Some(Timed {
                    at,
                    value: vec![instance("proj1", "db1", 3)],
                }),
                ..Inputs::default()
            };
            let mut eval = Evaluator::default();
            let mut fired = 0;
            for t in (0..=60_000).step_by(5_000) {
                fired += eval.step(&rules, &both(T0 + t), T0 + t).len();
            }
            assert_eq!(fired, 2, "the pool and the crash loop fire at 60 s");
            for t in (65_000..=665_000).step_by(5_000) {
                let down = Inputs {
                    at: T0 + t,
                    ..Inputs::default()
                };
                assert!(
                    eval.step(&rules, &down, T0 + t).is_empty(),
                    "no event at {t} ms"
                );
            }
            let firing = eval.firing();
            assert_eq!(firing.len(), 2, "both stay firing");
            assert!(firing.iter().all(|alert| alert.unknown), "both are unknown");
            assert_eq!(
                unknown_of(&eval, RuleId::DiskPoolFull),
                Some(UnknownReason::Missing)
            );
            assert_eq!(
                unknown_of(&eval, RuleId::CrashLoop),
                Some(UnknownReason::Missing)
            );
        }

        #[test]
        fn an_alert_whose_reading_can_never_exist_resolves_with_no_data_after_the_window() {
            let rules = Rules::default();
            let mut eval = Evaluator::default();
            for t in (0..=60_000).step_by(5_000) {
                eval.step(
                    &rules,
                    &reading_of(RuleId::MemPsiHigh, 30.0, T0 + t),
                    T0 + t,
                );
            }
            assert_eq!(eval.firing().len(), 1, "the pressure alert fires at 60 s");
            // The reading is known to be unavailable, not merely missing: after the clear window it resolves no data.
            let unavailable = |t: u64| Inputs {
                at: T0 + t,
                unavailable: std::collections::BTreeSet::from([RuleId::MemPsiHigh]),
                ..Inputs::default()
            };
            for t in (65_000..=180_000).step_by(5_000) {
                assert!(
                    eval.step(&rules, &unavailable(t), T0 + t).is_empty(),
                    "no event at {t} ms"
                );
                assert_eq!(eval.firing().len(), 1, "still firing at {t} ms");
                assert!(eval.firing()[0].unknown);
                assert_eq!(
                    unknown_of(&eval, RuleId::MemPsiHigh),
                    Some(UnknownReason::Unavailable)
                );
            }
            let resolved = eval.step(&rules, &unavailable(185_000), T0 + 185_000);
            assert_eq!(resolved.len(), 1);
            assert_eq!(resolved[0].state, EventState::Resolved);
            assert_eq!(resolved[0].reason, Some(ResolveReason::NoData));
            assert!(eval.firing().is_empty());
        }
    }

    mod crash_loop {
        use super::*;

        #[test]
        fn three_starts_held_60_s_fire_and_none_held_120_s_clear() {
            let rules = Rules::default();
            let mut eval = Evaluator::default();
            for t in (0..=55_000).step_by(5_000) {
                assert!(
                    eval.step(&rules, &reading_of(RuleId::CrashLoop, 3.0, T0 + t), T0 + t)
                        .is_empty()
                );
            }
            let fired = eval.step(
                &rules,
                &reading_of(RuleId::CrashLoop, 3.0, T0 + 60_000),
                T0 + 60_000,
            );
            assert_eq!(fired.len(), 1, "3 starts held 60 s fire");
            assert_eq!(
                fired[0].labels.get("project").map(String::as_str),
                Some("proj1")
            );
            assert_eq!(
                fired[0].labels.get("incus_instance").map(String::as_str),
                Some("db1")
            );
            assert_eq!(fired[0].value, Some(3.0));
            for t in (65_000..=180_000).step_by(5_000) {
                assert!(
                    eval.step(&rules, &reading_of(RuleId::CrashLoop, 0.0, T0 + t), T0 + t)
                        .is_empty()
                );
            }
            let cleared = eval.step(
                &rules,
                &reading_of(RuleId::CrashLoop, 0.0, T0 + 185_000),
                T0 + 185_000,
            );
            assert_eq!(cleared.len(), 1, "0 starts held 120 s clear");
            assert_eq!(cleared[0].reason, Some(ResolveReason::Cleared));
        }

        #[test]
        fn two_starts_is_not_over_the_threshold() {
            let rules = Rules::default();
            let mut eval = Evaluator::default();
            for t in (0..=300_000).step_by(5_000) {
                assert!(
                    eval.step(&rules, &reading_of(RuleId::CrashLoop, 2.0, T0 + t), T0 + t)
                        .is_empty()
                );
            }
        }
    }
}

mod rules {
    use super::*;

    fn parse(text: &str) -> Result<AlertsFile, RulesError> {
        arca_alerts::rules::parse(text.as_bytes())
    }

    fn rule(file: &AlertsFile, id: RuleId) -> arca_alerts::eval::Rule {
        file.rules
            .0
            .iter()
            .find(|rule| rule.id == id)
            .expect("every rule id has a rule")
            .clone()
    }

    mod disabled {
        use super::*;

        #[test]
        fn a_disabled_disk_pool_full_writes_no_event_and_exports_no_series() {
            let scratch = Scratch::new("disabled");
            let mut engine = engine_on(
                scratch.path(),
                &Switches::default(),
                rules_with_disabled(RuleId::DiskPoolFull),
            );
            let mut last = None;
            for t in (0..=600_000).step_by(5_000) {
                let snapshot = engine.tick(&many_pools(1, |_| 99, T0 + t), Vec::new(), T0 + t);
                assert!(snapshot.firing.is_empty(), "no firing at {t} ms");
                last = Some(snapshot);
            }
            assert!(events_of(scratch.path()).is_empty(), "no event is written");
            let snapshot = last.expect("ticks ran");
            assert_eq!(snapshot.disabled, vec!["disk-pool-full".to_string()]);
            let body = export::render_metrics("node-test", &snapshot);
            assert!(!body.contains("rule=\"disk-pool-full\""), "{body}");
        }
    }

    mod file {
        use super::*;

        #[test]
        fn a_missing_file_gives_the_defaults() {
            let scratch = Scratch::new("missing");
            let file =
                arca_alerts::rules::load(scratch.path()).expect("a missing file is not an error");
            assert_eq!(file, AlertsFile::default());
            assert_eq!(file.rules, Rules::default());
        }

        #[test]
        fn a_file_in_the_state_directory_is_read() {
            let scratch = Scratch::new("present");
            fs::write(
                scratch.join(arca_alerts::rules::ALERTS_FILE),
                r#"{"rules":[{"id":"disk-pool-full","threshold":85}]}"#,
            )
            .expect("write the file");
            let file = arca_alerts::rules::load(scratch.path()).expect("the file loads");
            assert_eq!(rule(&file, RuleId::DiskPoolFull).threshold, 85.0);
        }

        #[test]
        fn a_partial_rule_keeps_its_other_defaults() {
            let file = parse(r#"{"rules":[{"id":"disk-pool-full","threshold":85}]}"#)
                .expect("the file parses");
            let disk = rule(&file, RuleId::DiskPoolFull);
            assert_eq!(disk.threshold, 85.0);
            assert!(disk.enabled);
            assert_eq!((disk.for_s, disk.clear_for_s), (60, 120));
            assert_eq!(disk.clear_margin, 5.0);
            assert_eq!(disk.severity, Severity::Critical);
            assert_eq!(
                &file.rules.0[1..],
                &Rules::default().0[1..],
                "the other rules are unchanged"
            );
            assert_eq!(file.shim_socket, None);
            assert_eq!(file.probes, Probes::default());
        }

        #[test]
        fn every_field_of_a_rule_and_the_probes_and_shim_socket_are_read() {
            let file = parse(
                r#"{"rules":[{"id":"cpu-hot","enabled":false,"threshold":70,"forS":30,"clearForS":90,"clearMargin":2.5,"severity":"critical"}],
                    "shimSocket":"/run/example/shim.sock",
                    "probes":{"gateway":"192.0.2.1:9","relay":"198.51.100.7:443"}}"#,
            )
            .expect("the file parses");
            let cpu = rule(&file, RuleId::CpuHot);
            assert!(!cpu.enabled);
            assert_eq!(
                (cpu.threshold, cpu.for_s, cpu.clear_for_s, cpu.clear_margin),
                (70.0, 30, 90, 2.5)
            );
            assert_eq!(cpu.severity, Severity::Critical);
            assert_eq!(
                file.shim_socket,
                Some(PathBuf::from("/run/example/shim.sock"))
            );
            assert_eq!(
                file.probes.gateway,
                Some("192.0.2.1:9".parse().expect("an address"))
            );
            assert_eq!(file.probes.internet, None);
            assert_eq!(
                file.probes.relay,
                Some("198.51.100.7:443".parse().expect("an address"))
            );
        }

        #[test]
        fn an_unknown_rule_id_is_refused_by_its_key_and_its_value_is_never_echoed() {
            let err = parse(r#"{"rules":[{"id":"zz-secret-rule"}]}"#)
                .expect_err("an unknown id is refused");
            assert!(matches!(err, RulesError::UnknownRule(_)));
            let text = err.to_string();
            assert!(text.contains("rules[0].id"), "{text}");
            assert!(!text.contains("zz-secret-rule"), "{text}");
        }

        #[test]
        fn an_unknown_key_is_refused_by_name_and_its_value_is_never_echoed() {
            let err = parse(r#"{"rules":[{"id":"cpu-hot","bogusKey":"VALUE-qq-1"}]}"#)
                .expect_err("an unknown key is refused");
            let text = err.to_string();
            assert!(text.contains("bogusKey"), "{text}");
            assert!(!text.contains("VALUE-qq-1"), "{text}");
            assert!(parse(r#"{"surprise":"VALUE-qq-2"}"#).is_err());
        }

        #[test]
        fn invalid_json_wrong_types_and_bad_values_are_refused_without_an_echo() {
            let cases = [
                (r#"{"rules": [ VALUE-zz"#, "VALUE-zz"),
                (
                    r#"{"rules":[{"id":"cpu-hot","threshold":"VALUE-ww"}]}"#,
                    "VALUE-ww",
                ),
                (
                    r#"{"rules":[{"id":"cpu-hot","severity":"VALUE-vv"}]}"#,
                    "VALUE-vv",
                ),
                (r#"{"probes":{"gateway":"VALUE-pp"}}"#, "VALUE-pp"),
                (r#"{"rules":[{"id":"cpu-hot","clearMargin":-1}]}"#, ""),
                (r#"{"rules":[{"id":"cpu-hot"},{"id":"cpu-hot"}]}"#, ""),
                (r#"{"rules":[{"threshold":1}]}"#, ""),
            ];
            for (text, secret) in cases {
                let err = parse(text).expect_err(text);
                let shown = err.to_string();
                assert!(matches!(err, RulesError::Parse(_)), "{shown}");
                if !secret.is_empty() {
                    assert!(!shown.contains(secret), "{shown}");
                }
            }
            let repeated =
                parse(r#"{"rules":[{"id":"cpu-hot"},{"id":"cpu-hot"}]}"#).expect_err("repeats");
            assert!(repeated.to_string().contains("rules[1].id"));
            let missing = parse(r#"{"rules":[{"threshold":1}]}"#).expect_err("no id");
            assert!(missing.to_string().contains("`id`"), "{missing}");
        }
    }
}

mod store_failure {
    use super::*;

    #[test]
    fn a_failed_commit_still_publishes_and_fires_store_write_failed_at_once() {
        let scratch = Scratch::new("fails-on-demand");
        let switches = Switches::default();
        set(&switches.fail, true);
        let mut engine = engine_on(scratch.path(), &switches, Rules::default());
        let snapshot = engine.tick(&Inputs::default(), Vec::new(), T0);
        let error = snapshot.store_error.expect("the failed commit is marked");
        assert_eq!(error.at, T0);
        assert!(
            error.message.contains("no space left on device"),
            "{}",
            error.message
        );
        let failed: Vec<_> = snapshot
            .firing
            .iter()
            .filter(|alert| alert.event.rule == STORE_WRITE_FAILED)
            .collect();
        assert_eq!(failed.len(), 1, "store-write-failed fires at once");
        assert_eq!(failed[0].event.started_at, T0);
        assert!(
            failed[0].event.seq.is_none(),
            "the in-memory alert has no seq"
        );
        assert_eq!(snapshot.store_write_failures, 1);
    }

    #[test]
    fn queued_events_commit_on_the_next_success_with_their_original_time() {
        let scratch = Scratch::new("queued-commit");
        let switches = Switches::default();
        set(&switches.fail, true);
        let mut engine = engine_on(scratch.path(), &switches, Rules::default());
        for t in (0..=60_000).step_by(5_000) {
            engine.tick(&many_pools(1, |_| 95, T0 + t), Vec::new(), T0 + t);
        }
        assert!(
            events_of(scratch.path()).is_empty(),
            "nothing is committed while the store fails"
        );
        set(&switches.fail, false);
        let snapshot = engine.tick(&many_pools(1, |_| 95, T0 + 65_000), Vec::new(), T0 + 65_000);
        assert!(snapshot.store_error.is_none(), "the next commit works");
        let events = events_of(scratch.path());
        assert_eq!(events.len(), 1, "the queued firing is written once");
        assert_eq!(events[0].at, T0 + 60_000, "with its original time");
        assert_eq!(events[0].started_at, T0 + 60_000);
        assert_eq!(events[0].alert_id, events[0].seq);
    }

    #[test]
    fn a_resolve_is_committed_with_its_firing_when_the_firing_was_queued() {
        let scratch = Scratch::new("resolve-queued");
        let switches = Switches::default();
        set(&switches.fail, true);
        let mut engine = engine_on(scratch.path(), &switches, Rules::default());
        for t in (0..=60_000).step_by(5_000) {
            engine.tick(&many_pools(1, |_| 95, T0 + t), Vec::new(), T0 + t);
        }
        // The pool drops to 50 %, and the resolve is due at 185 s. Both stay queued while the store fails.
        for t in (65_000..=185_000).step_by(5_000) {
            engine.tick(&many_pools(1, |_| 50, T0 + t), Vec::new(), T0 + t);
        }
        set(&switches.fail, false);
        let snapshot = engine.tick(
            &many_pools(1, |_| 50, T0 + 190_000),
            Vec::new(),
            T0 + 190_000,
        );
        assert!(
            snapshot.store_error.is_none(),
            "the resolve did not fail the batch: {:?}",
            snapshot.store_error
        );
        let events = events_of(scratch.path());
        let firing = events
            .iter()
            .find(|event| event.state == EventState::Firing)
            .expect("the firing is written");
        let resolve = events
            .iter()
            .find(|event| event.state == EventState::Resolved)
            .expect("the resolve is written");
        assert_eq!(events.len(), 2);
        assert_eq!(
            resolve.alert_id, firing.seq,
            "the resolve closes its firing"
        );
        assert_eq!(resolve.reason, Some(ResolveReason::Cleared));
        // store-write-failed is still firing: its 120 s clock only starts at this success.
        assert!(
            snapshot
                .firing
                .iter()
                .all(|alert| alert.event.rule != "disk-pool-full"),
            "the disk alert is closed in memory too"
        );
    }

    #[test]
    fn a_queue_past_the_limit_drops_the_oldest_and_the_failure_counts_them() {
        let scratch = Scratch::new("queue-limit");
        let switches = Switches::default();
        set(&switches.fail, true);
        let mut engine = engine_on(scratch.path(), &switches, Rules::default());
        let mut at_60 = None;
        for t in (0..=60_000).step_by(5_000) {
            at_60 = Some(engine.tick(&many_pools(1_100, |_| 95, T0 + t), Vec::new(), T0 + t));
        }
        let message = at_60
            .and_then(|snapshot| snapshot.store_error)
            .expect("the failure is marked")
            .message;
        assert!(message.contains("100 alert events dropped"), "{message}");
        set(&switches.fail, false);
        let snapshot = engine.tick(
            &many_pools(1_100, |_| 95, T0 + 65_000),
            Vec::new(),
            T0 + 65_000,
        );
        assert!(
            snapshot.store_error.is_none(),
            "the queue commits: {:?}",
            snapshot.store_error
        );
        let stored = events_of(scratch.path());
        assert_eq!(
            stored.len(),
            QUEUE_LIMIT,
            "the queue keeps the newest {QUEUE_LIMIT}"
        );
        assert_eq!(
            stored[0].labels.get("pool").map(String::as_str),
            Some("pool0100")
        );
    }

    #[test]
    fn a_resolve_whose_firing_was_dropped_is_pruned_and_the_batch_still_commits() {
        let scratch = Scratch::new("orphan-resolve");
        let switches = Switches::default();
        set(&switches.fail, true);
        let mut engine = engine_on(scratch.path(), &switches, Rules::default());
        // 1,100 pools fire at 60 s while the store fails. The 100 oldest firings are dropped from the queue.
        let mut at_60 = None;
        for t in (0..=60_000).step_by(5_000) {
            at_60 = Some(engine.tick(&many_pools(1_100, |_| 95, T0 + t), Vec::new(), T0 + t));
        }
        let dropped = at_60
            .and_then(|snapshot| snapshot.store_error)
            .expect("the failure is marked")
            .message;
        assert!(dropped.contains("100 alert events dropped"), "{dropped}");
        // pool0000, whose firing was dropped, clears: its resolve has nothing to link to, so it is pruned.
        for t in (65_000..=185_000).step_by(5_000) {
            engine.tick(
                &many_pools(1_100, |i| if i == 0 { 50 } else { 95 }, T0 + t),
                Vec::new(),
                T0 + t,
            );
        }
        set(&switches.fail, false);
        let snapshot = engine.tick(
            &many_pools(1_100, |i| if i == 0 { 50 } else { 95 }, T0 + 190_000),
            Vec::new(),
            T0 + 190_000,
        );
        assert!(
            snapshot.store_error.is_none(),
            "the batch commits: {:?}",
            snapshot.store_error
        );
        let events = events_of(scratch.path());
        assert_eq!(events.len(), 1000, "the 1,000 queued firings are written");
        assert!(events.iter().all(|event| event.state == EventState::Firing));
        let pools: Vec<&str> = events
            .iter()
            .filter_map(|event| event.labels.get("pool").map(String::as_str))
            .collect();
        assert_eq!(
            pools.first().copied(),
            Some("pool0100"),
            "the oldest 100 were dropped"
        );
    }

    #[test]
    fn store_write_failed_clears_after_120_s_of_successful_commits() {
        let scratch = Scratch::new("clears");
        let switches = Switches::default();
        set(&switches.fail, true);
        let mut engine = engine_on(scratch.path(), &switches, Rules::default());
        engine.tick(&Inputs::default(), Vec::new(), T0);
        set(&switches.fail, false);
        let failed = |snapshot: &arca_alerts::AlertSnapshot| {
            snapshot
                .firing
                .iter()
                .any(|alert| alert.event.rule == STORE_WRITE_FAILED)
        };
        // The first success at 5 s starts the clock: firing at 120 s, cleared at 125 s.
        for t in (5_000..=120_000).step_by(5_000) {
            assert!(
                failed(&engine.tick(&Inputs::default(), Vec::new(), T0 + t)),
                "still firing at {t} ms"
            );
        }
        assert!(
            !failed(&engine.tick(&Inputs::default(), Vec::new(), T0 + 125_000)),
            "cleared at 125 s"
        );
    }

    #[test]
    fn store_write_failures_counts_each_failed_commit() {
        let scratch = Scratch::new("counts");
        let switches = Switches::default();
        set(&switches.fail, true);
        let mut engine = engine_on(scratch.path(), &switches, Rules::default());
        for t in [0, 5_000, 10_000] {
            engine.tick(&Inputs::default(), Vec::new(), T0 + t);
        }
        set(&switches.fail, false);
        let snapshot = engine.tick(&Inputs::default(), Vec::new(), T0 + 15_000);
        assert_eq!(
            snapshot.store_write_failures, 3,
            "three failures, and the success does not count"
        );
        assert_eq!(
            snapshot.store_slow_commits, 0,
            "a commit that failed is not a slow one"
        );
        assert!(
            snapshot
                .firing
                .iter()
                .all(|alert| alert.event.rule != STORE_COMMIT_SLOW),
            "store-commit-slow does not fire for failures"
        );
    }

    #[test]
    fn store_commit_slow_clears_after_120_s_of_commits_in_time_and_a_slow_one_starts_the_wait_again()
     {
        let scratch = Scratch::new("slow-clears");
        let switches = Switches::default();
        let mut engine = engine_on(scratch.path(), &switches, Rules::default());
        let slow_fires = |snapshot: &arca_alerts::AlertSnapshot| {
            snapshot
                .firing
                .iter()
                .any(|alert| alert.event.rule == STORE_COMMIT_SLOW)
        };
        set(&switches.slow, true);
        let first = engine.tick(&Inputs::default(), Vec::new(), T0);
        set(&switches.slow, false);
        assert!(slow_fires(&first));
        // In time from 5 s on. A second slow commit at 60 s starts the 120 s again.
        for t in (5_000..=55_000).step_by(5_000) {
            assert!(slow_fires(&engine.tick(
                &Inputs::default(),
                Vec::new(),
                T0 + t
            )));
        }
        set(&switches.slow, true);
        let second = engine.tick(&Inputs::default(), Vec::new(), T0 + 60_000);
        set(&switches.slow, false);
        assert_eq!(second.store_slow_commits, 2);
        assert!(slow_fires(&second));
        // The first commit in time after it is at 65 s: the alert holds until 120 s of them have passed, at 185 s.
        for t in (65_000..=180_000).step_by(5_000) {
            assert!(
                slow_fires(&engine.tick(&Inputs::default(), Vec::new(), T0 + t)),
                "still firing at {t}"
            );
        }
        let cleared = engine.tick(&Inputs::default(), Vec::new(), T0 + 185_000);
        assert!(
            !slow_fires(&cleared),
            "cleared after 120 s of commits in time"
        );
        assert_eq!(cleared.store_slow_commits, 2, "the count stays");
        assert_eq!(cleared.store_write_failures, 0);
    }

    #[test]
    fn a_failed_commit_while_store_commit_slow_fires_starts_its_120_s_again() {
        let scratch = Scratch::new("slow-then-failed");
        let switches = Switches::default();
        let mut engine = engine_on(scratch.path(), &switches, Rules::default());
        let slow_fires = |snapshot: &arca_alerts::AlertSnapshot| {
            snapshot
                .firing
                .iter()
                .any(|alert| alert.event.rule == STORE_COMMIT_SLOW)
        };
        // Slow at 0 s, in time from 5 s: left alone, the warning would clear at 125 s.
        set(&switches.slow, true);
        engine.tick(&Inputs::default(), Vec::new(), T0);
        set(&switches.slow, false);
        for t in (5_000..=60_000).step_by(5_000) {
            engine.tick(&Inputs::default(), Vec::new(), T0 + t);
        }
        // A commit fails at 65 s. It was not a commit in time, so the 120 s start again with the next one that is,
        // at 70 s. It is a failed write and no slow commit.
        set(&switches.fail, true);
        let failed = engine.tick(&Inputs::default(), Vec::new(), T0 + 65_000);
        set(&switches.fail, false);
        assert_eq!(
            (failed.store_write_failures, failed.store_slow_commits),
            (1, 1)
        );
        assert!(slow_fires(&failed));
        for t in (70_000..=185_000).step_by(5_000) {
            assert!(
                slow_fires(&engine.tick(&Inputs::default(), Vec::new(), T0 + t)),
                "still firing at {t}: the failed commit started the wait again"
            );
        }
        let cleared = engine.tick(&Inputs::default(), Vec::new(), T0 + 190_000);
        assert!(
            !slow_fires(&cleared),
            "cleared 120 s after the first commit in time that followed the failure"
        );
    }

    #[test]
    fn a_failed_write_and_a_slow_commit_are_two_alerts_and_each_clears_by_its_own_clock() {
        let scratch = Scratch::new("failed-and-slow");
        let switches = Switches::default();
        let mut engine = engine_on(scratch.path(), &switches, Rules::default());
        let rules = |snapshot: &arca_alerts::AlertSnapshot| -> Vec<(String, String)> {
            snapshot
                .firing
                .iter()
                .map(|alert| (alert.event.rule.clone(), alert.event.severity.clone()))
                .collect()
        };
        // A commit fails at 0 s, and one that works is slow at 5 s.
        set(&switches.fail, true);
        engine.tick(&Inputs::default(), Vec::new(), T0);
        set(&switches.fail, false);
        set(&switches.slow, true);
        let both = engine.tick(&Inputs::default(), Vec::new(), T0 + 5_000);
        set(&switches.slow, false);
        assert_eq!((both.store_write_failures, both.store_slow_commits), (1, 1));
        // Both fire, the older first, each with its own severity.
        assert_eq!(
            rules(&both),
            [
                (STORE_WRITE_FAILED.to_string(), "critical".to_string()),
                (STORE_COMMIT_SLOW.to_string(), "warning".to_string())
            ]
        );
        // The slow commit worked, so the failed write's 120 s began with it, at 5 s: it clears at 125 s. The slow one's
        // began with the first commit in time, at 10 s: it clears at 130 s.
        for t in (10_000..=120_000).step_by(5_000) {
            engine.tick(&Inputs::default(), Vec::new(), T0 + t);
        }
        let at_125 = engine.tick(&Inputs::default(), Vec::new(), T0 + 125_000);
        assert_eq!(
            rules(&at_125),
            [(STORE_COMMIT_SLOW.to_string(), "warning".to_string())]
        );
        let at_130 = engine.tick(&Inputs::default(), Vec::new(), T0 + 130_000);
        assert!(rules(&at_130).is_empty(), "both cleared");
    }

    #[test]
    fn a_commit_over_1_s_counts_as_slow_and_its_events_are_never_queued_again() {
        let scratch = Scratch::new("slow");
        let switches = Switches::default();
        let mut engine = engine_on(scratch.path(), &switches, Rules::default());
        for t in (0..=55_000).step_by(5_000) {
            engine.tick(&many_pools(1, |_| 95, T0 + t), Vec::new(), T0 + t);
        }
        // The commit at 60 s takes 1.1 s. It is committed: a slow commit, and no failed write.
        set(&switches.slow, true);
        let slow = engine.tick(&many_pools(1, |_| 95, T0 + 60_000), Vec::new(), T0 + 60_000);
        set(&switches.slow, false);
        assert_eq!(slow.store_slow_commits, 1, "the slow commit counts as slow");
        assert_eq!(slow.store_write_failures, 0, "and not as a failed write");
        assert!(
            slow.store_error.is_none(),
            "its events are committed, so no error is marked"
        );
        let warned: Vec<_> = slow
            .firing
            .iter()
            .filter(|alert| alert.event.rule == STORE_COMMIT_SLOW)
            .collect();
        assert_eq!(warned.len(), 1, "store-commit-slow fires at once");
        assert_eq!(warned[0].event.severity, "warning");
        assert_eq!(warned[0].event.started_at, T0 + 60_000);
        assert!(
            warned[0].event.seq.is_none(),
            "the in-memory alert has no seq"
        );
        assert!(
            slow.firing
                .iter()
                .all(|alert| alert.event.rule != STORE_WRITE_FAILED),
            "store-write-failed does not fire: nothing failed"
        );
        for t in (65_000..=120_000).step_by(5_000) {
            engine.tick(&many_pools(1, |_| 95, T0 + t), Vec::new(), T0 + t);
        }
        let events = events_of(scratch.path());
        let firings: Vec<&AlertEvent> = events
            .iter()
            .filter(|event| event.state == EventState::Firing)
            .collect();
        assert_eq!(firings.len(), 1, "one copy of the firing");
        let copies = events
            .iter()
            .filter(|event| event.alert_id == firings[0].alert_id)
            .count();
        assert_eq!(copies, 1, "the store holds exactly one event for the alert");
    }
}

/// What a stub socket answers each connection with.
#[derive(Clone, Copy)]
enum Answer {
    /// A status line and no body.
    Status(&'static str),
    /// Incus's answer to `GET /1.0`: a sync envelope with status 200.
    Incus,
    /// No answer for longer than a check waits.
    Hang,
}

/// A unix socket that answers every connection the same way and records each request line. Dropping it stops the listener.
struct Stub {
    socket: PathBuf,
    lines: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Stub {
    fn start(socket: PathBuf, answer: Answer) -> Stub {
        let listener = UnixListener::bind(&socket).expect("bind the stub socket");
        listener
            .set_nonblocking(true)
            .expect("the listener is nonblocking");
        let lines = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (log, halt) = (Arc::clone(&lines), Arc::clone(&stop));
        let thread = thread::spawn(move || {
            while !halt.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => answer_once(stream, answer, &log),
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Stub {
            socket,
            lines,
            stop,
            thread: Some(thread),
        }
    }

    fn socket(&self) -> &Path {
        &self.socket
    }

    /// The request line of each connection so far, in order.
    fn requests(&self) -> Vec<String> {
        self.lines.lock().expect("the log").clone()
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Reads one request's head, answers it, and logs its request line.
fn answer_once(mut stream: UnixStream, answer: Answer, log: &Mutex<Vec<String>>) {
    stream
        .set_nonblocking(false)
        .expect("the connection is blocking");
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("a read deadline");
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(1) => head.push(byte[0]),
            _ => break,
        }
    }
    let line = String::from_utf8_lossy(&head)
        .lines()
        .next()
        .unwrap_or("")
        .to_string();
    log.lock().expect("the log").push(line);
    let reply = match answer {
        Answer::Status(status) => {
            format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        }
        Answer::Incus => {
            let body = r#"{"type":"sync","status":"Success","status_code":200,"metadata":{}}"#;
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
        }
        Answer::Hang => {
            thread::sleep(Duration::from_secs(3));
            return;
        }
    };
    let _ = stream.write_all(reply.as_bytes());
}

mod checks {
    use super::*;
    use arca_alerts::checks::{CHECK_TIMEOUT, CheckTargets, components};
    use std::time::Instant;

    /// The reading of one component, or `None` when the reading has no such component.
    fn ok_of(reading: &Timed<Vec<Component>>, name: ComponentName) -> Option<bool> {
        reading
            .value
            .iter()
            .find(|component| component.name == name)
            .map(|component| component.ok)
    }

    fn targets(incus: PathBuf, arca: PathBuf, shim: Option<PathBuf>) -> CheckTargets {
        CheckTargets { incus, arca, shim }
    }

    #[test]
    fn a_socket_answering_401_to_a_tokenless_post_is_arca_ok() {
        let scratch = Scratch::new("checks-arca-401");
        let arca = Stub::start(
            scratch.join("arca.sock"),
            Answer::Status("401 Unauthorized"),
        );
        let t = targets(
            scratch.join("incus.sock"),
            arca.socket().to_path_buf(),
            None,
        );
        let reading = components(&t, None, T0);
        assert_eq!(ok_of(&reading, ComponentName::Arca), Some(true));
        assert_eq!(arca.requests(), ["POST /mcp HTTP/1.1"]);
    }

    #[test]
    fn a_socket_answering_200_to_the_tokenless_post_is_not_arca_ok() {
        let scratch = Scratch::new("checks-arca-200");
        let arca = Stub::start(scratch.join("arca.sock"), Answer::Status("200 OK"));
        let t = targets(
            scratch.join("incus.sock"),
            arca.socket().to_path_buf(),
            None,
        );
        assert_eq!(
            ok_of(&components(&t, None, T0), ComponentName::Arca),
            Some(false)
        );
    }

    #[test]
    fn an_absent_socket_is_not_ok_for_incus_or_arca() {
        let scratch = Scratch::new("checks-absent");
        let t = targets(scratch.join("incus.sock"), scratch.join("arca.sock"), None);
        let reading = components(&t, None, T0);
        assert_eq!(ok_of(&reading, ComponentName::Incus), Some(false));
        assert_eq!(ok_of(&reading, ComponentName::Arca), Some(false));
    }

    #[test]
    fn a_socket_that_never_answers_is_not_arca_ok_within_the_check_timeout() {
        let scratch = Scratch::new("checks-hang");
        let arca = Stub::start(scratch.join("arca.sock"), Answer::Hang);
        let t = targets(
            scratch.join("incus.sock"),
            arca.socket().to_path_buf(),
            None,
        );
        let started = Instant::now();
        let reading = components(&t, None, T0);
        let took = started.elapsed();
        assert_eq!(ok_of(&reading, ComponentName::Arca), Some(false));
        assert!(
            took < CHECK_TIMEOUT + Duration::from_secs(1),
            "the check gives up within its timeout, not after {took:?}"
        );
    }

    #[test]
    fn incus_is_ok_only_when_get_1_0_answers_200() {
        let scratch = Scratch::new("checks-incus");
        let incus = Stub::start(scratch.join("incus.sock"), Answer::Incus);
        let broken = Stub::start(
            scratch.join("broken.sock"),
            Answer::Status("500 Internal Server Error"),
        );
        let arca = scratch.join("arca.sock");
        let good = components(
            &targets(incus.socket().to_path_buf(), arca.clone(), None),
            None,
            T0,
        );
        assert_eq!(ok_of(&good, ComponentName::Incus), Some(true));
        assert_eq!(incus.requests(), ["GET /1.0 HTTP/1.1"]);
        let bad = components(
            &targets(broken.socket().to_path_buf(), arca, None),
            None,
            T0,
        );
        assert_eq!(ok_of(&bad, ComponentName::Incus), Some(false));
    }

    #[test]
    fn the_shim_component_exists_only_when_a_shim_socket_is_named() {
        let scratch = Scratch::new("checks-shim");
        let shim = Stub::start(scratch.join("shim.sock"), Answer::Incus);
        let arca = scratch.join("arca.sock");
        let unnamed = components(
            &targets(scratch.join("incus.sock"), arca.clone(), None),
            None,
            T0,
        );
        assert_eq!(
            ok_of(&unnamed, ComponentName::Shim),
            None,
            "no shim without shimSocket"
        );
        assert_eq!(unnamed.value.len(), 3);

        let named = components(
            &targets(
                scratch.join("incus.sock"),
                arca.clone(),
                Some(shim.socket().to_path_buf()),
            ),
            None,
            T0,
        );
        assert_eq!(ok_of(&named, ComponentName::Shim), Some(true));
        assert_eq!(shim.requests(), ["GET /1.0 HTTP/1.1"]);

        let missing = components(
            &targets(
                scratch.join("incus.sock"),
                arca,
                Some(scratch.join("no-shim.sock")),
            ),
            None,
            T0,
        );
        assert_eq!(ok_of(&missing, ComponentName::Shim), Some(false));
    }

    #[test]
    fn the_watch_component_is_ok_while_the_newest_sweep_is_within_90_s() {
        let scratch = Scratch::new("checks-watch");
        let t = targets(scratch.join("incus.sock"), scratch.join("arca.sock"), None);
        assert_eq!(
            ok_of(&components(&t, None, T0), ComponentName::Watch),
            Some(false),
            "no sweep has succeeded"
        );
        assert_eq!(
            ok_of(&components(&t, Some(T0), T0), ComponentName::Watch),
            Some(true)
        );
        assert_eq!(
            ok_of(
                &components(&t, Some(T0), T0 + WATCH_MAX_AGE_MS),
                ComponentName::Watch
            ),
            Some(true),
            "a sweep 90 s old is still ok, the boundary the evaluator reads the drift with"
        );
        assert_eq!(
            ok_of(
                &components(&t, Some(T0), T0 + WATCH_MAX_AGE_MS + 1),
                ComponentName::Watch
            ),
            Some(false)
        );
    }

    #[test]
    fn the_components_are_stamped_with_the_tick_time_in_name_order() {
        let scratch = Scratch::new("checks-order");
        let t = targets(
            scratch.join("incus.sock"),
            scratch.join("arca.sock"),
            Some(scratch.join("shim.sock")),
        );
        let reading = components(&t, Some(T0), T0 + 5);
        assert_eq!(reading.at, T0 + 5);
        let names: Vec<ComponentName> = reading
            .value
            .iter()
            .map(|component| component.name)
            .collect();
        assert_eq!(
            names,
            [
                ComponentName::Incus,
                ComponentName::Arca,
                ComponentName::Watch,
                ComponentName::Shim
            ]
        );
    }
}

mod get_only {
    use super::*;

    fn fake_state() -> FakeState {
        FakeState {
            server: json!({ "api_version": "1.0" }),
            projects: Vec::new(),
            instances: Vec::new(),
            networks: Vec::new(),
            pools: vec![(
                "default".to_string(),
                json!({ "space": { "used": 95, "total": 100 } }),
            )],
        }
    }

    #[test]
    fn a_sampler_and_an_engine_run_30_ticks_on_get_requests_only() {
        let scratch = Scratch::new("get-only");
        let state = scratch.join("state");
        fs::create_dir(&state).expect("the state directory is created");
        let fake = FakeIncus::start(scratch.path(), fake_state()).expect("the fake Incus starts");
        let mut sampler = Sampler::new(
            SamplerConfig {
                proc_root: scratch.join("proc"),
                sys_root: scratch.join("sys"),
                pools: vec!["default".to_string()],
                interval_ms: 5_000,
                history_minutes: 60,
                incus_local: true,
                mode_retry: None,
            },
            UnixIncus::new(fake.socket().to_path_buf()),
        );
        let store = Store::open(&state, T0).expect("the store opens").store;
        let mut engine =
            Engine::start(store, Rules::default(), false, T0).expect("the engine starts");
        let arca = Stub::start(
            scratch.join("arca.sock"),
            Answer::Status("401 Unauthorized"),
        );
        let targets = arca_alerts::checks::CheckTargets {
            incus: fake.socket().to_path_buf(),
            arca: arca.socket().to_path_buf(),
            shim: None,
        };
        for tick in 0..30u64 {
            let now = T0 + tick * 5_000;
            sampler.sample(now);
            let mut readings = inputs::from_sampler(&sampler);
            readings.components = Some(arca_alerts::checks::components(&targets, None, now));
            let points = export::points(&sampler, &readings);
            engine.tick(&readings, points, now);
        }
        let requests = fake.requests();
        assert!(!requests.is_empty(), "the sampler read Incus");
        assert!(
            requests.iter().all(|request| request.method == "GET"),
            "{requests:?}"
        );
        assert!(
            requests
                .iter()
                .any(|request| request.path == "/os/1.0/system/security")
                && requests
                    .iter()
                    .any(|request| request.path == "/os/1.0/system/update"),
            "the incus-osd documents are read, by GET"
        );
        // The health check's one non-GET goes to arca's own socket, once per tick, and nowhere else.
        assert_eq!(arca.requests(), vec!["POST /mcp HTTP/1.1".to_string(); 30]);
        assert!(fake.writes().is_empty(), "no write reached Incus");
        assert!(
            !state.join("desired.json").exists(),
            "no desired state was written"
        );
    }
}

mod health {
    use super::*;
    use arca_alerts::health::alerts_line;
    use arca_alerts::{AlertSnapshot, AlertState, FiringAlert, UnknownAlert};

    /// The instant the snapshots are read at.
    const NOW: u64 = T0 + 600_000;

    fn firing(rule: &str) -> FiringAlert {
        FiringAlert {
            event: firing_event(rule, &[], NOW - 60_000),
            unknown: false,
        }
    }

    fn unknown(rule: &str) -> UnknownAlert {
        UnknownAlert {
            rule: rule.to_string(),
            labels: BTreeMap::new(),
            state: AlertState::Ok,
            reason: UnknownReason::Missing,
        }
    }

    /// A snapshot whose newest sample was taken `age_ms` before [`NOW`].
    fn snapshot(
        age_ms: u64,
        firing: Vec<FiringAlert>,
        unknown: Vec<UnknownAlert>,
    ) -> AlertSnapshot {
        AlertSnapshot {
            at: NOW,
            sample_at: Some(NOW - age_ms),
            firing,
            unknown,
            ..AlertSnapshot::default()
        }
    }

    #[test]
    fn the_four_snapshots_give_the_four_lines() {
        assert_eq!(
            alerts_line(&snapshot(0, vec![], vec![]), NOW),
            "alerts none"
        );
        assert_eq!(
            alerts_line(
                &snapshot(0, vec![firing("disk-pool-full"), firing("cpu-hot")], vec![]),
                NOW
            ),
            "alerts 2 firing: disk-pool-full, cpu-hot"
        );
        assert_eq!(
            alerts_line(
                &snapshot(
                    0,
                    vec![firing("disk-pool-full")],
                    vec![unknown("update-failed")]
                ),
                NOW
            ),
            "alerts 1 firing: disk-pool-full · 1 unknown"
        );
        assert_eq!(
            alerts_line(
                &snapshot(42_000, vec![firing("disk-pool-full")], vec![]),
                NOW
            ),
            "alerts stale 42s"
        );
    }

    #[test]
    fn unknown_entries_show_as_a_count_when_nothing_fires() {
        let unknowns = vec![
            unknown("crash-loop"),
            unknown("update-failed"),
            unknown("smart-warning"),
        ];
        assert_eq!(
            alerts_line(&snapshot(0, vec![], unknowns), NOW),
            "alerts none · 3 unknown"
        );
    }

    #[test]
    fn the_sample_is_stale_only_past_fifteen_seconds_and_before_any_sample() {
        assert_eq!(
            alerts_line(&snapshot(SAMPLE_MAX_AGE_MS, vec![], vec![]), NOW),
            "alerts none"
        );
        assert_eq!(
            alerts_line(&snapshot(SAMPLE_MAX_AGE_MS + 1, vec![], vec![]), NOW),
            "alerts stale 15s"
        );
        let unsampled = AlertSnapshot {
            at: NOW,
            ..AlertSnapshot::default()
        };
        assert_eq!(alerts_line(&unsampled, NOW), "alerts stale");
    }
}

mod gpu_alerts {
    use super::*;

    #[test]
    fn vram_above_95_percent_fires_after_five_minutes_and_95_is_not_above() {
        let rules = Rules::default();
        let mut eval = Evaluator::default();
        for t in (0..300_000).step_by(5_000) {
            assert!(
                eval.step(
                    &rules,
                    &reading_of(RuleId::GpuVramHigh, 96.0, T0 + t),
                    T0 + t
                )
                .is_empty(),
                "pending at {t} ms"
            );
        }
        let fired = eval.step(
            &rules,
            &reading_of(RuleId::GpuVramHigh, 96.0, T0 + 300_000),
            T0 + 300_000,
        );
        assert_eq!(fired.len(), 1, "fires at five minutes");
        assert_eq!(fired[0].rule, "gpu-vram-high");
        assert_eq!(fired[0].labels["gpu"], "0000:01:00.0");
        assert_eq!(fired[0].value, Some(96.0));
        assert_eq!(fired[0].threshold, Some(95.0));

        // At exactly the threshold the comparison is not `>`, so the card never fires however long it holds.
        let mut at_limit = Evaluator::default();
        for t in (0..600_000).step_by(5_000) {
            assert!(
                at_limit
                    .step(
                        &rules,
                        &reading_of(RuleId::GpuVramHigh, 95.0, T0 + t),
                        T0 + t
                    )
                    .is_empty()
            );
        }
    }

    #[test]
    fn vram_clears_after_the_clear_window_below_the_margin() {
        let rules = Rules::default();
        let mut eval = Evaluator::default();
        for t in (0..=300_000).step_by(5_000) {
            eval.step(
                &rules,
                &reading_of(RuleId::GpuVramHigh, 98.0, T0 + t),
                T0 + t,
            );
        }
        assert_eq!(eval.firing().len(), 1);
        for t in (305_000..425_000).step_by(5_000) {
            assert!(
                eval.step(
                    &rules,
                    &reading_of(RuleId::GpuVramHigh, 90.0, T0 + t),
                    T0 + t
                )
                .is_empty(),
                "not cleared before 120 s at {t} ms"
            );
        }
        let cleared = eval.step(
            &rules,
            &reading_of(RuleId::GpuVramHigh, 90.0, T0 + 425_000),
            T0 + 425_000,
        );
        assert_eq!(cleared.len(), 1);
        assert_eq!(cleared[0].state, EventState::Resolved);
        assert_eq!(cleared[0].reason, Some(ResolveReason::Cleared));
    }

    #[test]
    fn a_card_gone_from_the_inventory_fires_and_its_return_resolves_with_subject_gone() {
        let rules = Rules::default();
        let mut eval = Evaluator::default();
        // The disappearance holds for 15 s before it fires.
        for t in [0, 5_000, 10_000] {
            assert!(
                eval.step(&rules, &reading_of(RuleId::GpuGone, 1.0, T0 + t), T0 + t)
                    .is_empty()
            );
        }
        let fired = eval.step(
            &rules,
            &reading_of(RuleId::GpuGone, 1.0, T0 + 15_000),
            T0 + 15_000,
        );
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].rule, "gpu-gone");
        assert_eq!(fired[0].labels["gpu"], "0000:01:00.0");

        // A failed inventory read is unknown, never an empty one: the alert is held, still firing, and marked unknown.
        assert!(
            eval.step(&rules, &Inputs::default(), T0 + 20_000)
                .is_empty()
        );
        assert_eq!(eval.firing().len(), 1);
        assert!(eval.firing()[0].unknown);
        assert_eq!(
            unknown_of(&eval, RuleId::GpuGone),
            Some(UnknownReason::Missing)
        );

        // The card is back: the fresh reading no longer holds its label set, so the alert resolves once as subject-gone.
        let resolved = eval.step(
            &rules,
            &reading_of(RuleId::GpuGone, 0.0, T0 + 25_000),
            T0 + 25_000,
        );
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].state, EventState::Resolved);
        assert_eq!(resolved[0].reason, Some(ResolveReason::SubjectGone));
        assert!(eval.firing().is_empty());
    }

    #[test]
    fn each_host_network_item_that_drifts_has_its_own_alert_that_resolves_when_it_is_put_back() {
        let rules = Rules::default();
        let mut eval = Evaluator::default();
        let drifted = |items: &[&str], at: u64| Inputs {
            at,
            host_network: Some(Timed {
                at,
                value: items
                    .iter()
                    .map(|item| HostNetworkItem {
                        item: (*item).to_string(),
                    })
                    .collect(),
            }),
            ..Inputs::default()
        };
        let both = ["ipv4-forwarding", "rule-in:incusbr0"];
        for t in [0, 30_000] {
            assert!(
                eval.step(&rules, &drifted(&both, T0 + t), T0 + t)
                    .is_empty()
            );
        }
        let fired = eval.step(&rules, &drifted(&both, T0 + 60_000), T0 + 60_000);
        let mut named: Vec<String> = fired
            .iter()
            .map(|event| event.labels["item"].clone())
            .collect();
        named.sort();
        assert_eq!(named, ["ipv4-forwarding", "rule-in:incusbr0"]);
        assert!(fired.iter().all(|event| event.rule == "host-network-drift"));
        assert!(fired.iter().all(|event| event.severity == "critical"));
        // The rule is put back: the item is gone from the fresh reading, and its alert resolves once.
        let resolved = eval.step(
            &rules,
            &drifted(&["rule-in:incusbr0"], T0 + 65_000),
            T0 + 65_000,
        );
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].labels["item"], "ipv4-forwarding");
        assert_eq!(resolved[0].reason, Some(ResolveReason::SubjectGone));
        // No status file, or a stale one, is unknown and never a resolve.
        assert!(
            eval.step(&rules, &Inputs::default(), T0 + 70_000)
                .is_empty()
        );
        assert_eq!(
            unknown_of(&eval, RuleId::HostNetwork),
            Some(UnknownReason::Missing)
        );
        assert_eq!(eval.firing().len(), 1);
        let late = T0 + 65_000 + HOST_NETWORK_MAX_AGE_MS + 1_000;
        assert!(
            eval.step(&rules, &drifted(&["rule-in:incusbr0"], T0 + 65_000), late)
                .is_empty()
        );
        assert_eq!(
            unknown_of(&eval, RuleId::HostNetwork),
            Some(UnknownReason::Stale)
        );
        assert_eq!(eval.firing().len(), 1);
    }

    #[test]
    fn a_card_with_no_telemetry_is_unknown_not_a_zero_alert() {
        let rules = Rules::default();
        let mut eval = Evaluator::default();
        let inputs = Inputs {
            at: T0,
            gpus: Some(Timed {
                at: T0,
                value: vec![GpuUse {
                    pci: "0000:01:00.0".to_string(),
                    vram_used_pct: None,
                }],
            }),
            ..Inputs::default()
        };
        assert!(eval.step(&rules, &inputs, T0).is_empty());
        assert_eq!(
            unknown_of(&eval, RuleId::GpuVramHigh),
            Some(UnknownReason::Missing),
            "a card the tool cannot measure is unknown, never a zero that never fires"
        );
    }
}

mod gpu_inputs {
    use super::*;
    use arca_metrics::gpu::{FakeNvidia, GpuSection, NOT_MEASURABLE, NVIDIA_MISSING, NvidiaSmi};
    use arca_metrics::{ModeRetry, Sampler, SamplerConfig};
    use std::sync::Arc;

    fn fake(tag: &str) -> (Scratch, FakeIncus) {
        let scratch = Scratch::new(tag);
        let fake = FakeIncus::start(
            scratch.path(),
            FakeState {
                server: json!({ "api_version": "1.0" }),
                projects: Vec::new(),
                instances: Vec::new(),
                pools: Vec::new(),
                networks: Vec::new(),
            },
        )
        .expect("the fake serves");
        fake.set_resources(json!({
            "cpu": { "total": 4 },
            "memory": { "total": 8_589_934_592u64, "used": 1_073_741_824u64 },
            "gpu": {
                "total": 1,
                "cards": [{
                    "pci_address": "0000:01:00.0",
                    "vendor": "NVIDIA Corporation",
                    "product": "NVIDIA GeForce RTX 4060 Ti",
                    "driver": "nvidia",
                    "numa_node": 0
                }]
            }
        }));
        (scratch, fake)
    }

    fn sampler(
        scratch: &Scratch,
        fake: &FakeIncus,
        local: bool,
        retry: Option<ModeRetry>,
        nvidia: impl arca_metrics::Nvidia + 'static,
    ) -> Sampler<UnixIncus> {
        Sampler::new(
            SamplerConfig {
                proc_root: scratch.path().join("proc"),
                sys_root: scratch.path().join("sys"),
                pools: Vec::new(),
                interval_ms: 5_000,
                history_minutes: 60,
                incus_local: local,
                mode_retry: retry,
            },
            UnixIncus::new(fake.socket().to_path_buf()),
        )
        .with_nvidia(Arc::new(nvidia))
    }

    #[test]
    fn not_measurable_and_missing_tool_are_unavailable_but_a_failed_or_pending_read_is_not() {
        let (remote_scratch, remote_fake) = fake("gpu-remote");
        let mut remote = sampler(
            &remote_scratch,
            &remote_fake,
            false,
            None,
            FakeNvidia::missing(),
        );
        remote.sample(T0);
        let inputs = inputs::from_sampler(&remote);
        assert!(inputs.unavailable.contains(&RuleId::GpuVramHigh));
        assert_eq!(
            remote.latest().expect("a sample").gpu.cards[0]
                .reason
                .as_deref(),
            Some(NOT_MEASURABLE)
        );

        let (missing_scratch, missing_fake) = fake("gpu-missing");
        let mut missing = sampler(
            &missing_scratch,
            &missing_fake,
            true,
            None,
            FakeNvidia::missing(),
        );
        missing.sample(T0);
        let inputs = inputs::from_sampler(&missing);
        assert!(inputs.unavailable.contains(&RuleId::GpuVramHigh));
        assert_eq!(
            missing.latest().expect("a sample").gpu.cards[0]
                .reason
                .as_deref(),
            Some(NVIDIA_MISSING)
        );

        let (failed_scratch, failed_fake) = fake("gpu-failed");
        let mut failed = sampler(
            &failed_scratch,
            &failed_fake,
            true,
            None,
            FakeNvidia::failing(),
        );
        failed.sample(T0);
        let inputs = inputs::from_sampler(&failed);
        assert!(
            !inputs.unavailable.contains(&RuleId::GpuVramHigh),
            "a failed read is an ordinary unknown, not a permanent one"
        );
        assert!(inputs.gpus.is_some(), "the card still has an unknown value");

        let (empty_scratch, empty_fake) = fake("gpu-empty");
        let mut empty = sampler(&empty_scratch, &empty_fake, true, None, FakeNvidia::empty());
        empty.sample(T0);
        let inputs = inputs::from_sampler(&empty);
        assert!(
            !inputs.unavailable.contains(&RuleId::GpuVramHigh),
            "no reading yet is unknown, never permanently unavailable"
        );
        assert_eq!(
            empty.latest().expect("a sample").gpu.cards[0]
                .reason
                .as_deref(),
            Some(arca_metrics::NO_READING_YET)
        );
    }

    #[test]
    fn a_provisional_mode_is_not_unavailable_even_when_the_card_cannot_be_measured() {
        let (scratch, fake) = fake("gpu-provisional");
        let retry = ModeRetry {
            explicit: None,
            path_is_local: true,
        };
        let mut sampler = sampler(&scratch, &fake, true, Some(retry), FakeNvidia::missing());
        sampler.sample(T0);
        assert!(
            sampler.mode_provisional(),
            "the socket-path choice is provisional"
        );
        let inputs = inputs::from_sampler(&sampler);
        assert!(
            !inputs.unavailable.contains(&RuleId::GpuVramHigh),
            "a provisional mode may still resolve local"
        );
    }

    #[test]
    fn the_metric_read_does_not_wait_for_a_hanging_tool_on_the_tick() {
        // The tool itself is covered by the metrics tests; here the sampler must retain the card and a plain reason.
        let (scratch, fake) = fake("gpu-reason");
        let mut sampler = sampler(
            &scratch,
            &fake,
            true,
            None,
            NvidiaSmi::at(scratch.path().join("missing-nvidia-smi")),
        );
        sampler.sample(T0);
        let section: GpuSection = sampler.latest().expect("a sample").gpu.clone();
        assert_eq!(section.cards.len(), 1);
        assert!(section.cards[0].telemetry.is_none());
        assert_eq!(section.cards[0].reason.as_deref(), Some(NVIDIA_MISSING));
    }
}
