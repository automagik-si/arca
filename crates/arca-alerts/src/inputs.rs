//! The tick's [`Inputs`], built from the sampler's newest sample and its [`Readings`].

use arca_incus::IncusRead;
use arca_metrics::{NOT_MEASURABLE, NVIDIA_MISSING, REMOTE_HOST_REASON, Sampler};

use crate::eval::{
    DriveUse, GpuGone, GpuUse, Inputs, InstanceUse, PoolUse, RuleId, Timed, UpdateUse,
};

/// The inputs of the sampler's newest sample, stamped with its time. A read that failed in the sampler is `None` here, so
/// the rule reads it as unknown. Before the first sample every source is unknown.
pub fn from_sampler<R: IncusRead>(sampler: &Sampler<R>) -> Inputs {
    let Some(sample) = sampler.latest() else {
        return Inputs::default();
    };
    let at = sample.at;
    // A remote metrics source never gives its own PSI or CPU temperature. A transient outage gives neither, so it is not
    // marked here: the evaluator keeps those alerts firing through it. A provisional mode is not marked either: it may
    // still resolve local, and a no-data resolve now would let the same alert fire again after the switch.
    let mut unavailable = std::collections::BTreeSet::new();
    if sample.load.reason.as_deref() == Some(REMOTE_HOST_REASON) && !sampler.mode_provisional() {
        unavailable.insert(RuleId::MemPsiHigh);
        unavailable.insert(RuleId::CpuHot);
    }
    // The inventory is the same read for a local and a remote host. Telemetry and attribution are local only, so a remote
    // host (or one without nvidia-smi) has every card present with no telemetry. Only that permanently unmeasurable case is
    // `unavailable`: a failed or timed-out read, or one the tool has not answered yet, is an ordinary unknown and keeps a
    // firing alert. A provisional mode is not marked either; it may still resolve local.
    let gpu_known = sample.gpu.reason.is_none();
    let not_measurable = !sample.gpu.cards.is_empty()
        && sample.gpu.cards.iter().all(|card| {
            card.telemetry.is_none()
                && matches!(
                    card.reason.as_deref(),
                    Some(NOT_MEASURABLE) | Some(NVIDIA_MISSING)
                )
        });
    if gpu_known && not_measurable && !sampler.mode_provisional() {
        unavailable.insert(RuleId::GpuVramHigh);
    }
    let gpus = gpu_known.then(|| Timed {
        at,
        value: sample
            .gpu
            .cards
            .iter()
            .map(|card| GpuUse {
                pci: card.pci.clone(),
                vram_used_pct: card.telemetry.as_ref().and_then(|telemetry| {
                    let used = telemetry.memory_used_bytes?;
                    let total = telemetry.memory_total_bytes?;
                    (total > 0).then(|| used as f64 / total as f64 * 100.0)
                }),
            })
            .collect(),
    });
    let gpus_gone = gpu_known.then(|| Timed {
        at,
        value: sampler
            .readings()
            .gpus_gone
            .iter()
            .map(|card| GpuGone {
                pci: card.pci.clone(),
            })
            .collect(),
    });
    Inputs {
        at,
        unavailable,
        gpus,
        gpus_gone,
        pools: sampler.readings().pools.as_ref().map(|pools| Timed {
            at,
            value: pools
                .iter()
                .map(|pool| PoolUse {
                    name: pool.name.clone(),
                    used_bytes: pool.used_bytes,
                    total_bytes: pool.total_bytes,
                })
                .collect(),
        }),
        memory_some_avg10: sample
            .psi
            .memory_some_avg10
            .map(|value| Timed { at, value }),
        cpu_temp_c: sample.cpu_temp_c.map(|value| Timed { at, value }),
        instances: sampler
            .readings()
            .instances
            .as_ref()
            .map(|instances| Timed {
                at,
                value: instances
                    .iter()
                    .map(|instance| InstanceUse {
                        project: instance.project.clone(),
                        name: instance.name.clone(),
                        starts_10m: instance.starts_10m,
                    })
                    .collect(),
            }),
        update: sampler.readings().osd.as_ref().and_then(|osd| {
            osd.update_failed.map(|failed| Timed {
                at: osd.at,
                value: UpdateUse { failed },
            })
        }),
        drives: sampler.readings().drives.as_ref().map(|drives| Timed {
            at: drives.at,
            value: drives
                .drives
                .iter()
                .map(|drive| DriveUse {
                    device: drive.device.clone(),
                    passed: drive.passed,
                })
                .collect(),
        }),
        ..Inputs::default()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use arca_incus::{IncusError, IncusRead};
    use arca_metrics::{
        MODE_PROBE_EVERY_MS, ModeRetry, MonotonicClock, PROVISIONAL_WINDOW_MS, Sampler,
        SamplerConfig,
    };
    use serde_json::{Value, json};

    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use arca_store::{EventState, ResolveReason};

    use super::from_sampler;
    use crate::UnknownReason;
    use crate::eval::{Evaluator, Inputs, InstanceUse, PoolUse, RuleId, Rules, Timed};

    /// The pools an Incus lists: each name with its used and total bytes.
    type PoolList = Vec<(&'static str, u64, u64)>;

    /// An Incus that answers nothing, as an absent socket does.
    struct Offline;

    impl IncusRead for Offline {
        fn get(&self, path: &str) -> Result<Value, IncusError> {
            Err(IncusError::Decode(format!("offline: {path}")))
        }
    }

    /// An Incus that lists the given pools, each with its `(used, total)` space, and answers no instance list.
    struct Pools(PoolList);

    impl IncusRead for Pools {
        fn get(&self, path: &str) -> Result<Value, IncusError> {
            if path == "/1.0/storage-pools?recursion=1" {
                return Ok(Value::Array(
                    self.0
                        .iter()
                        .map(|(name, _, _)| json!({ "name": name }))
                        .collect(),
                ));
            }
            if let Some((_, used, total)) = self
                .0
                .iter()
                .find(|(name, _, _)| path == format!("/1.0/storage-pools/{name}/resources"))
            {
                return Ok(json!({ "space": { "used": used, "total": total } }));
            }
            Err(IncusError::Decode(format!("no answer for {path}")))
        }
    }

    /// A sampler over roots that do not exist, so every host reading is `None`, with the given allow-filter.
    fn sampler<R: IncusRead>(incus: R, pools: &[&str]) -> Sampler<R> {
        Sampler::new(
            SamplerConfig {
                proc_root: PathBuf::from("/nonexistent/arca-inputs-proc"),
                sys_root: PathBuf::from("/nonexistent/arca-inputs-sys"),
                pools: pools.iter().map(|pool| pool.to_string()).collect(),
                interval_ms: 5_000,
                history_minutes: 60,
                incus_local: true,
                mode_retry: None,
            },
            incus,
        )
    }

    /// A temporary procfs and sysfs tree for the two host readings the inputs take from the sample.
    struct Host(PathBuf);

    impl Host {
        fn new(tag: &str) -> Host {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .subsec_nanos();
            let dir = std::env::temp_dir().join(format!(
                "arca-alerts-inputs-{}-{nanos}-{tag}",
                std::process::id()
            ));
            fs::create_dir_all(&dir).expect("create the scratch directory");
            Host(dir)
        }

        fn write(&self, rel: &str, text: &str) {
            let path = self.0.join(rel);
            fs::create_dir_all(path.parent().expect("a fixture has a parent"))
                .expect("create a fixture directory");
            fs::write(path, text).expect("write a fixture file");
        }

        fn proc_root(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Host {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn before_any_sample_every_source_is_unknown() {
        let sampler = sampler(Offline, &["default"]);
        assert_eq!(from_sampler(&sampler), Inputs::default());
    }

    #[test]
    fn a_failed_pool_listing_maps_to_no_pools() {
        let mut sampler = sampler(Offline, &["default"]);
        sampler.sample(1_000);

        let inputs = from_sampler(&sampler);
        assert_eq!(inputs.at, 1_000);
        assert_eq!(
            inputs.pools, None,
            "a listing that failed is unknown, not empty"
        );
    }

    #[test]
    fn a_listed_pool_maps_to_its_space_stamped_with_the_sample_time() {
        let mut sampler = sampler(Pools(vec![("default", 95, 100)]), &[]);
        sampler.sample(2_000);

        assert_eq!(
            from_sampler(&sampler).pools,
            Some(Timed {
                at: 2_000,
                value: vec![PoolUse {
                    name: "default".to_string(),
                    used_bytes: Some(95),
                    total_bytes: Some(100),
                }],
            })
        );
    }

    #[test]
    fn a_filter_that_admits_no_listed_pool_maps_to_an_empty_list_not_to_unknown() {
        let mut sampler = sampler(Pools(vec![("default", 95, 100)]), &["pool9"]);
        sampler.sample(3_000);

        assert_eq!(
            from_sampler(&sampler).pools,
            Some(Timed {
                at: 3_000,
                value: Vec::new(),
            }),
            "a listed pool the filter drops is known to be absent, so its alert can end"
        );
    }

    #[test]
    fn memory_pressure_and_cpu_temperature_come_from_the_sample() {
        let host = Host::new("host-readings");
        host.write(
            "proc/pressure/memory",
            "some avg10=25.50 avg60=0.00 avg300=0.00 total=1\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n",
        );
        host.write("sys/class/hwmon/hwmon0/name", "k10temp\n");
        host.write("sys/class/hwmon/hwmon0/temp1_input", "85500\n");
        let mut sampler = Sampler::new(
            SamplerConfig {
                proc_root: host.proc_root().join("proc"),
                sys_root: host.proc_root().join("sys"),
                pools: Vec::new(),
                interval_ms: 5_000,
                history_minutes: 60,
                incus_local: true,
                mode_retry: None,
            },
            Offline,
        );
        sampler.sample(4_000);

        let inputs = from_sampler(&sampler);
        assert_eq!(
            inputs.memory_some_avg10,
            Some(Timed {
                at: 4_000,
                value: 25.5,
            })
        );
        assert_eq!(
            inputs.cpu_temp_c,
            Some(Timed {
                at: 4_000,
                value: 85.5,
            })
        );
    }

    /// An Incus that lists the instance rows the test sets, at recursion two.
    #[derive(Clone, Default)]
    struct Rows(Arc<Mutex<Vec<Value>>>);

    impl IncusRead for Rows {
        fn get(&self, path: &str) -> Result<Value, IncusError> {
            if path == "/1.0/instances?all-projects=true&recursion=2" {
                let rows = self.0.lock().expect("the rows are not poisoned").clone();
                return Ok(Value::Array(rows));
            }
            Err(IncusError::Decode(format!("no answer for {path}")))
        }
    }

    #[test]
    fn instances_map_to_their_starts_in_ten_minutes_and_a_failed_list_to_unknown() {
        let rows = Rows::default();
        *rows.0.lock().expect("the rows are not poisoned") = vec![json!({
            "name": "db1", "project": "proj1", "status": "Running", "type": "container",
            "last_used_at": "2026-11-01T10:00:00Z"
        })];
        let mut tracked = sampler(rows.clone(), &[]);
        tracked.sample(1_000);
        assert_eq!(
            from_sampler(&tracked).instances,
            Some(Timed {
                at: 1_000,
                value: vec![InstanceUse {
                    project: "proj1".to_string(),
                    name: "db1".to_string(),
                    starts_10m: 0,
                }],
            }),
            "the first tick counts no start"
        );

        rows.0.lock().expect("the rows are not poisoned")[0]["last_used_at"] =
            json!("2026-11-01T10:00:06Z");
        tracked.sample(6_000);
        assert_eq!(
            from_sampler(&tracked).instances.map(|timed| timed.value),
            Some(vec![InstanceUse {
                project: "proj1".to_string(),
                name: "db1".to_string(),
                starts_10m: 1,
            }])
        );

        let mut offline = sampler(Offline, &[]);
        offline.sample(7_000);
        assert_eq!(
            from_sampler(&offline).instances,
            None,
            "an unreachable Incus is unknown, never no instances"
        );
    }

    #[test]
    fn host_readings_the_host_does_not_give_map_to_unknown() {
        let mut sampler = sampler(Offline, &[]);
        sampler.sample(5_000);

        let inputs = from_sampler(&sampler);
        assert_eq!(inputs.memory_some_avg10, None);
        assert_eq!(inputs.cpu_temp_c, None);
    }

    /// An Incus whose pool list the test changes, and which can be switched off as an absent socket is.
    #[derive(Clone)]
    struct Live {
        down: Arc<AtomicBool>,
        pools: Arc<Mutex<PoolList>>,
    }

    impl IncusRead for Live {
        fn get(&self, path: &str) -> Result<Value, IncusError> {
            if self.down.load(Ordering::SeqCst) {
                return Err(IncusError::Decode(format!("offline: {path}")));
            }
            let listed = self
                .pools
                .lock()
                .expect("the pool list is not poisoned")
                .clone();
            Pools(listed).get(path)
        }
    }

    #[test]
    fn an_outage_stays_unknown_to_the_evaluator_and_a_pool_removed_after_it_resolves() {
        const T0: u64 = 1_000_000;
        let live = Live {
            down: Arc::new(AtomicBool::new(false)),
            pools: Arc::new(Mutex::new(vec![("default", 95, 100)])),
        };
        let mut sampler = sampler(live.clone(), &[]);
        let rules = Rules::default();
        let mut eval = Evaluator::default();
        let tick = |sampler: &mut Sampler<Live>, eval: &mut Evaluator, at: u64| {
            sampler.sample(at);
            eval.step(&rules, &from_sampler(sampler), at)
        };

        // The pool is over 90 % from the first tick, so it fires once it has held 60 s.
        let mut fired = 0;
        for t in (0..=60_000).step_by(5_000) {
            fired += tick(&mut sampler, &mut eval, T0 + t).len();
        }
        assert_eq!(fired, 1, "the pool alert fires at 60 s");

        // Incus is unreachable for ten minutes: no event, the alert stays firing, and its data is missing, not zero.
        live.down.store(true, Ordering::SeqCst);
        for t in (65_000..=665_000).step_by(5_000) {
            assert!(
                tick(&mut sampler, &mut eval, T0 + t).is_empty(),
                "no event at {t} ms"
            );
        }
        assert_eq!(eval.firing().len(), 1, "the alert stays firing");
        assert!(eval.firing()[0].unknown, "and is marked unknown");
        assert_eq!(
            eval.unknown().first().map(|alert| alert.reason),
            Some(UnknownReason::Missing),
            "an unreachable Incus is missing data"
        );

        // Incus answers again without the pool: the removed pool resolves its alert once, as subject-gone.
        live.down.store(false, Ordering::SeqCst);
        live.pools
            .lock()
            .expect("the pool list is not poisoned")
            .clear();
        let events = tick(&mut sampler, &mut eval, T0 + 670_000);
        assert_eq!(events.len(), 1, "exactly one resolve");
        assert_eq!(events[0].state, EventState::Resolved);
        assert_eq!(events[0].reason, Some(ResolveReason::SubjectGone));
        assert!(eval.firing().is_empty());
    }

    #[test]
    fn a_remote_source_marks_psi_and_cpu_temperature_never_available() {
        let mut remote = Sampler::new(
            SamplerConfig {
                proc_root: PathBuf::from("/nonexistent/arca-inputs-remote-proc"),
                sys_root: PathBuf::from("/nonexistent/arca-inputs-remote-sys"),
                pools: Vec::new(),
                interval_ms: 5_000,
                history_minutes: 60,
                incus_local: false,
                mode_retry: None,
            },
            Offline,
        );
        remote.sample(1_000);
        let inputs = from_sampler(&remote);
        assert!(inputs.unavailable.contains(&RuleId::MemPsiHigh));
        assert!(inputs.unavailable.contains(&RuleId::CpuHot));

        // A local source can give both, so it marks neither unavailable; a missing reading there is a transient gap.
        let mut local = sampler(Offline, &[]);
        local.sample(1_000);
        assert!(from_sampler(&local).unavailable.is_empty());
    }

    /// An Incus whose `/1.0` is unreadable until `up` is set, then names `hostname`; every other path answers 404.
    struct SlowName {
        up: Arc<AtomicBool>,
        hostname: String,
    }

    impl IncusRead for SlowName {
        fn get(&self, path: &str) -> Result<Value, IncusError> {
            if path == "/1.0" {
                return if self.up.load(Ordering::SeqCst) {
                    Ok(json!({ "environment": { "server_name": self.hostname } }))
                } else {
                    Err(IncusError::Timeout)
                };
            }
            Err(IncusError::Status {
                code: 404,
                message: "not found".to_string(),
            })
        }
    }

    /// A monotonic clock the test advances by hand, so window and cadence tests never sleep.
    #[derive(Clone)]
    struct FakeClock(Arc<AtomicU64>);

    impl FakeClock {
        fn new() -> FakeClock {
            FakeClock(Arc::new(AtomicU64::new(0)))
        }

        fn advance(&self, ms: u64) {
            self.0.fetch_add(ms, Ordering::SeqCst);
        }
    }

    impl MonotonicClock for FakeClock {
        fn now_ms(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    #[test]
    fn a_provisional_remote_mode_does_not_resolve_a_firing_alert_and_then_refire_it() {
        const T0: u64 = 1_000_000;
        let host = Host::new("provisional-alert");
        host.write("proc/sys/kernel/hostname", "node1\n");
        host.write(
            "proc/pressure/memory",
            "some avg10=30.00 avg60=0.00 avg300=0.00 total=1\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n",
        );
        let up = Arc::new(AtomicBool::new(false));
        let mut sampler = Sampler::new(
            SamplerConfig {
                proc_root: host.proc_root().join("proc"),
                sys_root: host.proc_root().join("sys"),
                pools: Vec::new(),
                interval_ms: 5_000,
                history_minutes: 60,
                incus_local: false,
                mode_retry: Some(ModeRetry {
                    explicit: None,
                    path_is_local: false,
                }),
            },
            SlowName {
                up: Arc::clone(&up),
                hostname: "node1".to_string(),
            },
        );
        let rules = Rules::default();
        let mut eval = Evaluator::default();

        // A local reading fired the pressure alert before Incus answered.
        for t in (0..=60_000).step_by(5_000) {
            let inputs = Inputs {
                at: T0 + t,
                memory_some_avg10: Some(Timed {
                    at: T0 + t,
                    value: 30.0,
                }),
                ..Inputs::default()
            };
            eval.step(&rules, &inputs, T0 + t);
        }
        assert_eq!(eval.firing().len(), 1, "the pressure alert fires at 60 s");

        // Incus is slow, so the mode is still provisional remote and the sampler cannot read PSI. That must not count as
        // permanence: the alert stays firing through the wait instead of resolving no data and firing again later.
        for t in (65_000..=200_000).step_by(5_000) {
            sampler.sample(T0 + t);
            let inputs = from_sampler(&sampler);
            assert!(
                inputs.unavailable.is_empty(),
                "a provisional mode marks nothing never-available at {t} ms"
            );
            assert!(
                eval.step(&rules, &inputs, T0 + t).is_empty(),
                "no event at {t} ms"
            );
            assert_eq!(eval.firing().len(), 1, "still firing at {t} ms");
            assert!(
                eval.unknown().iter().any(|alert| {
                    alert.rule == RuleId::MemPsiHigh.as_str()
                        && alert.reason == UnknownReason::Missing
                }),
                "a data gap, never a no-data resolve, at {t} ms"
            );
        }

        // Incus answers and names this host: the mode settles local, PSI is readable again, and the alert keeps firing
        // without a second notification.
        up.store(true, Ordering::SeqCst);
        sampler.sample(T0 + 205_000);
        assert!(!sampler.mode_provisional(), "the retry settled the mode");
        let inputs = from_sampler(&sampler);
        assert_eq!(
            inputs.memory_some_avg10.as_ref().map(|timed| timed.value),
            Some(30.0)
        );
        assert!(
            eval.step(&rules, &inputs, T0 + 205_000).is_empty(),
            "the recovered reading does not fire the alert again"
        );
        assert_eq!(eval.firing().len(), 1);
        assert!(!eval.firing()[0].unknown, "a fresh reading is known again");

        // A resolved alert would have re-fired 60 s after the recovery. The still-firing alert must not.
        for t in (210_000..=265_000).step_by(5_000) {
            sampler.sample(T0 + t);
            assert!(
                eval.step(&rules, &from_sampler(&sampler), T0 + t)
                    .is_empty(),
                "no re-fire at {t} ms"
            );
        }
        assert_eq!(eval.firing().len(), 1);
    }

    #[test]
    fn a_late_name_after_twenty_minutes_restores_the_mode_and_the_alert() {
        const T0: u64 = 3_000_000;
        let host = Host::new("late-name-alert");
        host.write("proc/sys/kernel/hostname", "node1\n");
        host.write(
            "proc/pressure/memory",
            "some avg10=30.00 avg60=0.00 avg300=0.00 total=1\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n",
        );
        let up = Arc::new(AtomicBool::new(false));
        let clock = FakeClock::new();
        let mut sampler = Sampler::new(
            SamplerConfig {
                proc_root: host.proc_root().join("proc"),
                sys_root: host.proc_root().join("sys"),
                pools: Vec::new(),
                interval_ms: 5_000,
                history_minutes: 60,
                incus_local: false,
                mode_retry: Some(ModeRetry {
                    explicit: None,
                    path_is_local: false,
                }),
            },
            SlowName {
                up: Arc::clone(&up),
                hostname: "node1".to_string(),
            },
        )
        .with_clock(Arc::new(clock.clone()));
        let rules = Rules::default();
        let mut eval = Evaluator::default();

        // The pressure alert is already firing when the sampler's provisional samples begin.
        for t in (0..=60_000).step_by(5_000) {
            let inputs = Inputs {
                at: T0 + t,
                memory_some_avg10: Some(Timed {
                    at: T0 + t,
                    value: 30.0,
                }),
                ..Inputs::default()
            };
            eval.step(&rules, &inputs, T0 + t);
        }
        assert_eq!(eval.firing().len(), 1);

        // The first sample starts the window; the alert keeps firing with a data gap.
        let started = 65_000;
        sampler.sample(T0 + started);
        assert!(sampler.mode_provisional());
        let inputs = from_sampler(&sampler);
        assert!(inputs.unavailable.is_empty());
        assert!(eval.step(&rules, &inputs, T0 + started).is_empty());
        assert_eq!(eval.firing().len(), 1);
        assert!(eval.unknown().iter().any(|alert| {
            alert.rule == RuleId::MemPsiHigh.as_str() && alert.reason == UnknownReason::Missing
        }));

        // At 10 minutes it settles on the remote fallback for alerts and starts the no-data clock.
        clock.advance(PROVISIONAL_WINDOW_MS);
        let settled_epoch = T0 + started + PROVISIONAL_WINDOW_MS;
        sampler.sample(settled_epoch);
        assert!(!sampler.mode_provisional(), "the window ended");
        let inputs = from_sampler(&sampler);
        assert!(inputs.unavailable.contains(&RuleId::MemPsiHigh));
        assert!(eval.step(&rules, &inputs, settled_epoch).is_empty());
        assert_eq!(eval.firing().len(), 1);
        assert!(eval.unknown().iter().any(|alert| {
            alert.rule == RuleId::MemPsiHigh.as_str() && alert.reason == UnknownReason::Unavailable
        }));

        // It stays unreadable to 20 minutes. The slow probe runs, and after the clear window the restored alert
        // resolves no data exactly once.
        let mut fake = PROVISIONAL_WINDOW_MS;
        let mut epoch = settled_epoch;
        let mut events = Vec::new();
        while fake < 1_200_000 {
            clock.advance(5_000);
            fake += 5_000;
            epoch += 5_000;
            sampler.sample(epoch);
            events.extend(eval.step(&rules, &from_sampler(&sampler), epoch));
        }
        assert_eq!(events.len(), 1, "exactly one no-data resolve");
        assert_eq!(events[0].state, EventState::Resolved);
        assert_eq!(events[0].reason, Some(ResolveReason::NoData));
        assert!(eval.firing().is_empty());
        assert!(!sampler.mode_provisional());

        // After 20 minutes the server names this host behind the shim: the next slow probe switches to local.
        up.store(true, Ordering::SeqCst);
        clock.advance(MODE_PROBE_EVERY_MS);
        epoch += MODE_PROBE_EVERY_MS;
        sampler.sample(epoch);
        assert!(!sampler.mode_provisional(), "the name decided the mode");
        let inputs = from_sampler(&sampler);
        assert!(inputs.unavailable.is_empty());
        assert_eq!(
            inputs.memory_some_avg10.as_ref().map(|timed| timed.value),
            Some(30.0)
        );
        assert!(
            eval.step(&rules, &inputs, epoch).is_empty(),
            "the restored reading starts pending, not firing"
        );
        assert!(eval.firing().is_empty());

        // The restored high PSI held for 60 s fires the alert again.
        let mut fired = 0;
        for _ in 0..12 {
            clock.advance(5_000);
            epoch += 5_000;
            sampler.sample(epoch);
            fired += eval.step(&rules, &from_sampler(&sampler), epoch).len();
        }
        assert_eq!(fired, 1, "the restored alert fires again");
        assert_eq!(eval.firing().len(), 1);
    }
}
