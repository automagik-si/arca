#![cfg(feature = "write")]
//! The writer of one function run, against the fake Incus: it starts and stops a function's own instance and nothing
//! else, and it is the only path that forces a stop.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use arca_incus::function_run::{FUNCTION_KEY, FreshRoot, FunctionRun};
use arca_incus::{FakeIncus, FakeState, IncusError, ProjectFamily, StateAction, StateChanger};
use serde_json::{Value, json};

const PROJECT: &str = "arcak-svc";
static SEQ: AtomicU64 = AtomicU64::new(0);

fn instance(name: &str, status: &str, kind: &str, config: Value) -> Value {
    let mut config = config;
    config["volatile.uuid"] = json!(format!("00000000-0000-4000-8000-{name:0>12}"));
    json!({
        "name": name,
        "project": PROJECT,
        "type": kind,
        "status": status,
        "profiles": [],
        "config": config,
        "devices": {},
    })
}

struct Rig {
    dir: PathBuf,
    fake: FakeIncus,
    run: FunctionRun,
}

impl Rig {
    fn new() -> Rig {
        let dir = std::env::temp_dir().join(format!(
            "arca-function-run-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir(&dir).expect("scratch");
        let state = FakeState {
            server: json!({}),
            projects: vec![json!({ "name": PROJECT }), json!({ "name": "other" })],
            instances: vec![
                instance(
                    "relay",
                    "Stopped",
                    "container",
                    json!({ FUNCTION_KEY: "true", "volatile.base_image": "c".repeat(64) }),
                ),
                instance(
                    "busy",
                    "Running",
                    "container",
                    json!({ FUNCTION_KEY: "true" }),
                ),
                // A service: Arca's own, and not a function.
                instance(
                    "web",
                    "Running",
                    "container",
                    json!({ "user.arcakernel.entry": "service-web" }),
                ),
                // The mark on a virtual machine is not a function either: a function is a container.
                instance(
                    "vm",
                    "Running",
                    "virtual-machine",
                    json!({ FUNCTION_KEY: "true" }),
                ),
            ],
            networks: vec![],
            pools: vec![],
        };
        let fake = FakeIncus::start(&dir, state).expect("the fake serves");
        let family = ProjectFamily::new([PROJECT], std::iter::empty::<&str>()).expect("family");
        let run = FunctionRun::new(fake.socket().to_path_buf(), family);
        Rig { dir, fake, run }
    }

    fn status(&self, name: &str) -> String {
        let family = ProjectFamily::new([PROJECT], std::iter::empty::<&str>()).expect("family");
        StateChanger::new(self.fake.socket().to_path_buf(), family)
            .read(PROJECT, name)
            .expect("the instance reads")
            .status
    }

    /// How many rebuilds the fake was asked for, refused ones included.
    fn rebuild_requests(&self) -> usize {
        self.fake
            .requests()
            .iter()
            .filter(|request| request.method == "POST" && request.path.contains("/rebuild"))
            .count()
    }

    /// The state changes the fake received, as `(method, path)`.
    fn state_writes(&self) -> Vec<String> {
        self.fake
            .requests()
            .into_iter()
            .filter(|request| request.method != "GET" && request.path.contains("/state"))
            .map(|request| request.path)
            .collect()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn a_run_starts_its_function_and_reads_what_it_wrote() {
    let rig = Rig::new();
    rig.fake
        .exit_after_start(PROJECT, "relay", 1, "working\n{\"ok\":true}\n");
    let seen = rig.run.read(PROJECT, "relay").expect("a function");
    rig.fake.set_console_log(PROJECT, "relay", "an older run\n");
    rig.run.clear_output(&seen).expect("the log is cleared");
    assert_eq!(rig.run.output(&seen).expect("no log"), "");
    rig.run.start(&seen).expect("it starts");
    assert_eq!(rig.run.status(&seen).expect("status"), "Running");
    assert_eq!(rig.run.status(&seen).expect("status"), "Stopped");
    assert_eq!(
        rig.run.output(&seen).expect("the output"),
        "working\n{\"ok\":true}\n"
    );
    // A function with no log yet has nothing to clear, and that is not a failure.
    rig.run.clear_output(&seen).expect("cleared");
    rig.run.clear_output(&seen).expect("nothing to clear");
}

#[test]
fn a_function_that_will_not_stop_is_force_stopped() {
    let rig = Rig::new();
    // Its graceful stop outlasts the grace; only a forced stop ends it.
    rig.fake.hang_stop("busy", false);
    let seen = rig.run.read(PROJECT, "busy").expect("a function");
    let forced = rig
        .run
        .stop(&seen, Duration::from_secs(1))
        .expect("it is stopped");
    assert!(forced, "the graceful stop failed, so the stop was forced");
    assert_eq!(rig.status("busy"), "Stopped");
    assert_eq!(rig.state_writes().len(), 2, "one graceful try, one forced");
    // A stop of a stopped function changes nothing and sends nothing.
    assert!(
        !rig.run
            .stop(&seen, Duration::from_secs(1))
            .expect("already stopped")
    );
    assert_eq!(rig.state_writes().len(), 2);
}

#[test]
fn a_function_that_stops_in_its_grace_is_not_forced() {
    let rig = Rig::new();
    let seen = rig.run.read(PROJECT, "busy").expect("a function");
    let forced = rig
        .run
        .stop(&seen, Duration::from_secs(1))
        .expect("it is stopped");
    assert!(!forced);
    assert_eq!(rig.state_writes().len(), 1);
}

#[test]
fn a_service_is_never_read_started_or_force_stopped_from_here() {
    let rig = Rig::new();
    rig.fake.hang_stop("web", false);
    for name in ["web", "vm"] {
        let refused = rig.run.read(PROJECT, name).expect_err("not a function");
        assert!(
            matches!(&refused, IncusError::Refused(why) if why.contains("not a function's")),
            "{refused:?}"
        );
    }
    // A caller that holds a service's state from elsewhere gets no further: every write reads the mark again.
    let family = ProjectFamily::new([PROJECT], std::iter::empty::<&str>()).expect("family");
    let changer = StateChanger::new(rig.fake.socket().to_path_buf(), family)
        .with_deadline(Duration::from_secs(5));
    let service = changer.read(PROJECT, "web").expect("the service reads");
    for result in [
        rig.run.stop(&service, Duration::ZERO).map(|_| ()),
        rig.run.stop(&service, Duration::from_secs(1)).map(|_| ()),
        rig.run.start(&service),
        rig.run.clear_output(&service),
        rig.run.output(&service).map(|_| ()),
    ] {
        assert!(matches!(result, Err(IncusError::Refused(_))), "{result:?}");
    }
    assert!(
        rig.state_writes().is_empty(),
        "nothing was sent to the service"
    );
    // The service path itself still forces nothing: a hung service fails its one graceful stop and keeps running.
    changer
        .change(&service, StateAction::Stop)
        .expect_err("the graceful stop fails");
    assert_eq!(rig.status("web"), "Running");
    assert_eq!(rig.state_writes().len(), 1);
}

#[test]
fn a_root_is_rebuilt_from_the_stores_copy_and_from_the_registry_when_that_is_gone() {
    let rig = Rig::new();
    let source = json!({
        "type": "image", "mode": "pull", "protocol": "oci",
        "server": "https://registry.example",
        "alias": format!("acme/relay@sha256:{}", "a".repeat(64)),
    });
    let seen = rig.run.read(PROJECT, "relay").expect("a function");
    assert_eq!(
        rig.run.fresh_root(&seen, &source).expect("rebuilt"),
        FreshRoot::Store
    );
    rig.fake.lose_image(PROJECT, "relay");
    assert_eq!(
        rig.run.fresh_root(&seen, &source).expect("rebuilt"),
        FreshRoot::Registry
    );
    assert_eq!(rig.fake.rebuilds(PROJECT, "relay"), ["store", "registry"]);
    // With the copy gone, only an image by digest on a registry is asked for: a tag, a bare name, another protocol,
    // a server without TLS, another mode and a source with a field more are each refused before a second request.
    let sent = rig.rebuild_requests();
    for change in [
        json!({ "alias": "acme/relay:latest" }),
        json!({ "alias": "acme/relay" }),
        json!({ "protocol": "simplestreams" }),
        json!({ "server": "http://registry.example" }),
        json!({ "mode": "local" }),
        json!({ "secret": "x" }),
    ] {
        let mut odd = source.clone();
        for (key, value) in change.as_object().expect("an object") {
            odd[key] = value.clone();
        }
        let refused = rig
            .run
            .fresh_root(&seen, &odd)
            .expect_err("not a pinned image");
        assert!(
            matches!(&refused, IncusError::Refused(why) if why.contains("no image by digest")),
            "{change}: {refused:?}"
        );
    }
    assert_eq!(
        rig.rebuild_requests(),
        sent + 6,
        "one try from the store each, and no other"
    );
    // Neither the store nor the registry: the error says why each was refused.
    rig.fake.refuse_rebuild(PROJECT, "relay");
    let both = rig
        .run
        .fresh_root(&seen, &source)
        .expect_err("no image anywhere");
    assert!(
        matches!(&both, IncusError::Refused(why)
            if why.contains("from the node's own copy of the image") && why.contains("from the registry")),
        "{both:?}"
    );
    // A running function is not rebuilt, and neither is anything that is not a function.
    let busy = rig.run.read(PROJECT, "busy").expect("a function");
    let sent = rig.rebuild_requests();
    assert!(matches!(
        rig.run.fresh_root(&busy, &source),
        Err(IncusError::Refused(_))
    ));
    let family = ProjectFamily::new([PROJECT], std::iter::empty::<&str>()).expect("family");
    let service = StateChanger::new(rig.fake.socket().to_path_buf(), family)
        .read(PROJECT, "web")
        .expect("the service reads");
    assert!(matches!(
        rig.run.fresh_root(&service, &source),
        Err(IncusError::Refused(_))
    ));
    assert_eq!(rig.rebuild_requests(), sent, "nothing was sent for either");
}

#[test]
fn a_function_outside_the_family_or_recreated_is_refused() {
    let rig = Rig::new();
    let outside = rig
        .run
        .read("other", "relay")
        .expect_err("outside the family");
    assert!(matches!(outside, IncusError::Refused(_)), "{outside:?}");
    let mut seen = rig.run.read(PROJECT, "relay").expect("a function");
    seen.uuid = Some("00000000-0000-4000-8000-00000000dead".to_string());
    let recreated = rig
        .run
        .start(&seen)
        .expect_err("another instance of the name");
    assert!(
        matches!(&recreated, IncusError::Refused(why) if why.contains("recreated")),
        "{recreated:?}"
    );
    assert!(rig.state_writes().is_empty());
    // A running function is not started again: a run never joins a start it did not make.
    let busy = rig.run.read(PROJECT, "busy").expect("a function");
    assert!(matches!(rig.run.start(&busy), Err(IncusError::Refused(_))));
}
