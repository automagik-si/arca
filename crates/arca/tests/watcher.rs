//! The watch thread's schedule and the health it feeds. A fake clock drives `watcher_step` and the ticker, and no `watch_report`
//! call is made. The sweeps start at 0, 30 and 60 s and not between. The tick reads the newest sweep as drift, and when the
//! sweeps stop succeeding the `watch` component turns not ok, so `service-down` goes pending. Incus is a fake. The arca socket
//! is a real server over a token minted in a temporary directory, so its tokenless `POST /mcp` is answered by arca itself.

mod common;

mod watcher_schedule {
    use std::collections::BTreeSet;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use arca::handlers::{
        AlertsPublished, Published, Ticker, WatchHandler, watch_reading, watcher_step,
    };
    use arca_alerts::checks::{CheckTargets, components};
    use arca_alerts::eval::{ComponentName, Rules};
    use arca_alerts::{AlertSnapshot, Engine, FiringAlert, UnknownReason};
    use arca_api::{Scope, verbs};
    use arca_incus::{FakeIncus, FakeState, UnixIncus};
    use arca_mcp::{Server, TokenStore, mint, serve_unix};
    use arca_metrics::{Sampler, SamplerConfig};
    use arca_store::Store;
    use serde_json::json;

    use super::common::{NOW, Scratch};

    /// One default project with one instance that carries a kernel mark for an entry no desired state holds. That is one
    /// `marked_without_entry` drift.
    fn fake_state() -> FakeState {
        FakeState {
            server: json!({ "api_version": "1.0" }),
            projects: vec![json!({ "name": "default", "description": "", "config": {} })],
            instances: vec![json!({
                "name": "k1",
                "project": "default",
                "type": "container",
                "status": "Running",
                "config": { "user.arcakernel.entry": "toy" },
                "expanded_config": { "user.arcakernel.entry": "toy" }
            })],
            networks: Vec::new(),
            pools: vec![(
                "default".to_string(),
                json!({ "space": { "used": 10, "total": 100 } }),
            )],
        }
    }

    /// A directory of mode 0700 inside the scratch directory.
    fn private_dir(scratch: &Scratch, name: &str) -> PathBuf {
        let dir = scratch.path(name);
        fs::create_dir(&dir).expect("the directory is created");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("mode 0700");
        dir
    }

    /// Serves arca's router on a unix socket in the scratch directory, with one viewer token. Returns the socket.
    fn arca_socket(scratch: &Scratch, runtime: &tokio::runtime::Runtime) -> PathBuf {
        let tokens = private_dir(scratch, "tokens");
        let (_secret, text) = mint(Scope::Viewer);
        let hash = tokens.join("viewer.hash");
        fs::write(&hash, text).expect("write the hash file");
        fs::set_permissions(&hash, fs::Permissions::from_mode(0o400)).expect("mode 0400");
        let router =
            Server::new(verbs(), TokenStore::load(&tokens).expect("the tokens load")).router();
        let socket = scratch.path("arca.sock");
        let served = socket.clone();
        runtime.spawn(async move { serve_unix(router, &served).await });
        let deadline = Instant::now() + Duration::from_secs(5);
        while UnixStream::connect(&socket).is_err() {
            assert!(Instant::now() < deadline, "the socket never came up");
            std::thread::sleep(Duration::from_millis(20));
        }
        socket
    }

    fn ticker(
        scratch: &Scratch,
        incus: &Path,
        arca: &Path,
        handler: &Arc<WatchHandler>,
        alerts: &AlertsPublished,
    ) -> Ticker<Store> {
        let state = private_dir(scratch, "state");
        let sampler = Sampler::new(
            SamplerConfig {
                proc_root: scratch.path("proc"),
                sys_root: scratch.path("sys"),
                pools: vec!["default".to_string()],
                interval_ms: 5_000,
                history_minutes: 60,
                incus_local: true,
                mode_retry: None,
            },
            UnixIncus::new(incus.to_path_buf()),
        );
        let store = Store::open(&state, NOW).expect("the store opens").store;
        let engine = Engine::start(store, Rules::default(), false, NOW).expect("the engine starts");
        let targets = CheckTargets {
            incus: incus.to_path_buf(),
            arca: arca.to_path_buf(),
            shim: None,
        };
        Ticker::new(sampler, engine, Published::default(), alerts.clone())
            .with_checks(targets)
            .with_watch(Arc::clone(handler))
    }

    fn firing<'a>(snapshot: &'a AlertSnapshot, rule: &str) -> Option<&'a FiringAlert> {
        snapshot
            .firing
            .iter()
            .find(|alert| alert.event.rule == rule)
    }

    /// Whether `service-down` fires for `component`.
    fn firing_component(snapshot: &AlertSnapshot, component: &str) -> bool {
        snapshot.firing.iter().any(|alert| {
            alert.event.rule == "service-down"
                && alert.event.labels.get("component").map(String::as_str) == Some(component)
        })
    }

    #[test]
    fn watcher_step_sweeps_at_0_30_and_60_s_and_the_tick_reads_drift_from_the_first_sweep() {
        let scratch = Scratch::new("watch-schedule");
        let runtime = tokio::runtime::Runtime::new().expect("a runtime");
        let fake = FakeIncus::start(&scratch.dir, fake_state()).expect("start the fake Incus");
        let arca = arca_socket(&scratch, &runtime);
        let handler = Arc::new(WatchHandler::new(
            fake.socket().to_path_buf(),
            BTreeSet::new(),
        ));
        let alerts = AlertsPublished::default();
        let mut ticker = ticker(&scratch, fake.socket(), &arca, &handler, &alerts);

        // Every 5 s: the watcher's step, then the tick. The sweeps are the steps that return true.
        let mut last = None;
        let mut swept = Vec::new();
        for t in (0..=60).step_by(5) {
            let now = NOW + t * 1_000;
            if watcher_step(&handler, now, &mut last) {
                swept.push(t);
            }
            ticker.tick(now);
            if t < 60 {
                assert!(
                    firing(&alerts.current(), "drift").is_none(),
                    "drift is not firing at {t} s"
                );
            }
        }
        assert_eq!(
            swept,
            [0, 30, 60],
            "the sweeps start at 0, 30 and 60 s and not between"
        );
        let snapshot = alerts.current();
        let drift = firing(&snapshot, "drift").expect("drift fires 60 s after the first sweep");
        assert_eq!(drift.event.value, Some(1.0));
        let (_, report) = handler.newest().expect("a sweep succeeded");
        assert_eq!(watch_reading(&report).drift["marked_without_entry"], 1);

        // The fake's socket goes away. The sweeps at 90, 120 and 150 s fail, so the newest success is still the one at 60 s.
        fs::remove_file(fake.socket()).expect("remove the fake's socket");
        let mut failed = Vec::new();
        for t in (65..=155).step_by(5) {
            let now = NOW + t * 1_000;
            if watcher_step(&handler, now, &mut last) {
                failed.push(t);
            }
            ticker.tick(now);
            if t == 150 {
                let snapshot = alerts.current();
                assert!(
                    !firing_component(&snapshot, "watch"),
                    "the watch is ok at 150 s, 90 s after its sweep"
                );
                assert!(
                    firing(&snapshot, "drift").is_some(),
                    "drift is still known at 150 s"
                );
            }
        }
        assert_eq!(failed, [90, 120, 150], "the sweeps still start on schedule");
        let snapshot = alerts.current();
        assert!(
            snapshot
                .unknown
                .iter()
                .any(|alert| alert.rule == "drift" && alert.reason == UnknownReason::Stale),
            "the drift reading is stale at 155 s: {:?}",
            snapshot.unknown
        );
        assert!(
            firing(&snapshot, "drift").is_some(),
            "a stale reading keeps the alert's firing state"
        );

        // The watch component is not ok from 155 s, so service-down for it is pending. It fires 60 s later, at 215 s.
        for t in (160..=215).step_by(5) {
            let now = NOW + t * 1_000;
            watcher_step(&handler, now, &mut last);
            ticker.tick(now);
            assert_eq!(
                firing_component(&alerts.current(), "watch"),
                t == 215,
                "service-down for the watch is pending until 215 s, then firing: at {t} s"
            );
        }
        assert!(
            !firing_component(&alerts.current(), "arca"),
            "the arca check stays ok: the real server answers its tokenless POST with 401"
        );
    }

    #[test]
    fn the_arca_check_reads_401_from_a_real_server_without_a_token() {
        let scratch = Scratch::new("watch-arca");
        let runtime = tokio::runtime::Runtime::new().expect("a runtime");
        let arca = arca_socket(&scratch, &runtime);
        let targets = CheckTargets {
            incus: scratch.path("absent.sock"),
            arca,
            shim: None,
        };
        let reading = components(&targets, None, NOW);
        let arca_ok = reading
            .value
            .iter()
            .find(|component| component.name == ComponentName::Arca)
            .map(|component| component.ok);
        assert_eq!(
            arca_ok,
            Some(true),
            "the real server answers 401 to a tokenless POST"
        );
    }
}
