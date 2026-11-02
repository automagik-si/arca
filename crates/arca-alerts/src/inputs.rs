//! The tick's [`Inputs`], built from the sampler's newest sample and its [`Readings`].

use arca_incus::IncusRead;
use arca_metrics::Sampler;

use crate::eval::{DriveUse, Inputs, InstanceUse, PoolUse, Timed, UpdateUse};

/// The inputs of the sampler's newest sample, stamped with its time. A read that failed in the sampler is `None` here, so
/// the rule reads it as unknown. Before the first sample every source is unknown.
pub fn from_sampler<R: IncusRead>(sampler: &Sampler<R>) -> Inputs {
    let Some(sample) = sampler.latest() else {
        return Inputs::default();
    };
    let at = sample.at;
    Inputs {
        at,
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
    use arca_metrics::{Sampler, SamplerConfig};
    use serde_json::{Value, json};

    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use arca_store::{EventState, ResolveReason};

    use super::from_sampler;
    use crate::UnknownReason;
    use crate::eval::{Evaluator, Inputs, InstanceUse, PoolUse, Rules, Timed};

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
}
