#![cfg(feature = "write")]
//! The writer of one function run, against the fake Incus: it starts and stops a function's own instance and nothing
//! else, and it is the only path that forces a stop.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use arca_incus::function_run::{FUNCTION_KEY, FreshRoot, FunctionRun, STARTED_KEY};
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
                    // It has run before: Incus wrote the key that says so at its first start.
                    json!({ FUNCTION_KEY: "true", "volatile.base_image": "c".repeat(64),
                            "volatile.last_state.power": "STOPPED" }),
                ),
                // A function's container as its create leaves it: never started, with the id map it would start
                // with next.
                instance(
                    "fresh",
                    "Stopped",
                    "container",
                    json!({ FUNCTION_KEY: "true", "volatile.base_image": "e".repeat(64),
                            "volatile.idmap.base": "0", "volatile.idmap.next": "[{\"Hostid\":1000000}]",
                            "volatile.last_state.idmap": "[]" }),
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
fn a_root_is_rebuilt_from_the_recorded_copy_and_from_the_registry_only_when_that_is_gone() {
    let rig = Rig::new();
    let source = json!({
        "type": "image", "mode": "pull", "protocol": "oci",
        "server": "https://registry.example",
        "alias": format!("acme/relay@sha256:{}", "a".repeat(64)),
    });
    let never = || false;
    let held = rig.fake.add_image(PROJECT, "the-functions-image");
    let other = rig.fake.add_image(PROJECT, "another-image");
    let seen = rig.run.read(PROJECT, "relay").expect("a function");
    // The store holds the recorded copy: the root is rebuilt from it, and from no other image, whatever the
    // instance itself says it was made from (its volatile.base_image is another fingerprint here).
    assert_ne!(seen.own_config["volatile.base_image"], held);
    assert_eq!(
        rig.run
            .fresh_root(&seen, Some(&held), &source, None, &never)
            .expect("rebuilt"),
        FreshRoot::Store
    );
    assert_eq!(
        rig.fake.rebuilds(PROJECT, "relay"),
        [format!("store:{held}")]
    );
    // The recorded copy is gone from the store, or the record names none: the registry is asked.
    let gone = "d".repeat(64);
    for recorded in [Some(gone.as_str()), None] {
        assert_eq!(
            rig.run
                .fresh_root(&seen, recorded, &source, None, &never)
                .expect("rebuilt"),
            FreshRoot::Registry
        );
    }
    assert_eq!(rig.fake.rebuilds(PROJECT, "relay").len(), 3);
    // A copy that is there and cannot be rebuilt from is an error: the registry is not asked for it.
    rig.fake.lose_image(PROJECT, "relay");
    let sent = rig.rebuild_requests();
    let refused = rig
        .run
        .fresh_root(&seen, Some(&other), &source, None, &never)
        .expect_err("the rebuild from the copy fails");
    assert!(
        !matches!(&refused, IncusError::Refused(why) if why.contains("registry")),
        "{refused:?}"
    );
    assert_eq!(
        rig.rebuild_requests(),
        sent + 1,
        "one try from the store, none from the registry"
    );
    rig.fake.allow_rebuild(PROJECT, "relay");
    // With the copy gone, only an image by digest on a registry is asked for: a tag, a bare name, another protocol,
    // a server without TLS, another mode and a source with a field more are each refused before any request.
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
            .fresh_root(&seen, Some(&gone), &odd, None, &never)
            .expect_err("not a pinned image");
        assert!(
            matches!(&refused, IncusError::Refused(why) if why.contains("no image by digest")),
            "{change}: {refused:?}"
        );
    }
    // A record whose image is no fingerprint is refused, and a function that is being removed fetches nothing.
    assert!(matches!(
        rig.run
            .fresh_root(&seen, Some("../x"), &source, None, &never),
        Err(IncusError::Refused(_))
    ));
    let removing = || true;
    let cancelled = rig
        .run
        .fresh_root(&seen, Some(&gone), &source, None, &removing)
        .expect_err("cancelled");
    assert!(
        matches!(&cancelled, IncusError::Refused(why) if why.contains("being removed")),
        "{cancelled:?}"
    );
    assert_eq!(rig.rebuild_requests(), sent, "no rebuild was asked for");
    // The copy gone and the registry silent: the error says so.
    rig.fake.refuse_rebuild(PROJECT, "relay");
    let both = rig
        .run
        .fresh_root(&seen, Some(&gone), &source, None, &never)
        .expect_err("no image anywhere");
    assert!(
        matches!(&both, IncusError::Refused(why) if why.contains("the registry did not give it again")),
        "{both:?}"
    );
    // A running function is not rebuilt, and neither is anything that is not a function.
    let busy = rig.run.read(PROJECT, "busy").expect("a function");
    let sent = rig.rebuild_requests();
    assert!(matches!(
        rig.run
            .fresh_root(&busy, Some(&held), &source, None, &never),
        Err(IncusError::Refused(_))
    ));
    let family = ProjectFamily::new([PROJECT], std::iter::empty::<&str>()).expect("family");
    let service = StateChanger::new(rig.fake.socket().to_path_buf(), family)
        .read(PROJECT, "web")
        .expect("the service reads");
    assert!(matches!(
        rig.run
            .fresh_root(&service, Some(&held), &source, None, &never),
        Err(IncusError::Refused(_))
    ));
    assert_eq!(rig.rebuild_requests(), sent, "nothing was sent for either");
}

/// A container that has never been started is not rebuilt before its first run: its root is the one its create made
/// from the image, and Incus cannot start a container that was rebuilt before it ever ran. Once it has run, every
/// run's root is rebuilt, and it still starts.
#[test]
fn a_container_that_never_started_is_not_rebuilt_and_one_that_has_run_is() {
    let rig = Rig::new();
    let source = json!({
        "type": "image", "mode": "pull", "protocol": "oci",
        "server": "https://registry.example",
        "alias": format!("acme/relay@sha256:{}", "a".repeat(64)),
    });
    let never = || false;
    let made_from = "e".repeat(64);
    let seen = rig.run.read(PROJECT, "fresh").expect("a function");
    assert!(!seen.own_config.contains_key(STARTED_KEY));
    // What can be checked of a root nothing was rebuilt into is checked: the instance says it was made from the
    // image the function's record took. Another image, and it is refused.
    let refused = rig
        .run
        .fresh_root(&seen, Some(&"f".repeat(64)), &source, &never)
        .expect_err("another image than the record's");
    assert!(
        matches!(&refused, IncusError::Refused(why) if why.contains("never been started")),
        "{refused:?}"
    );
    // The record's own image, or a record that names none: nothing is rebuilt, and Incus is asked for no rebuild.
    for recorded in [Some(made_from.as_str()), None] {
        assert_eq!(
            rig.run
                .fresh_root(&seen, recorded, &source, &never)
                .expect("a new root"),
            FreshRoot::New
        );
    }
    assert_eq!(rig.rebuild_requests(), 0);
    // So its first start is taken. Had it been rebuilt, Incus would have refused it: a rebuild takes away the id
    // map a container starts with next, and one that never ran has no other.
    rig.fake.run_until_stopped(PROJECT, "fresh");
    rig.run.start(&seen).expect("the first start");
    rig.run
        .stop(&seen, Duration::from_secs(1))
        .expect("it stops");
    let seen = rig.run.read(PROJECT, "fresh").expect("a function");
    assert!(seen.own_config.contains_key(STARTED_KEY));
    // It has run: now its root is rebuilt before a run, and the start after the rebuild is taken too.
    assert_eq!(
        rig.run
            .fresh_root(&seen, None, &source, &never)
            .expect("rebuilt"),
        FreshRoot::Registry
    );
    assert_eq!(rig.fake.rebuilds(PROJECT, "fresh"), ["registry"]);
    rig.run.start(&seen).expect("a start after a rebuild");
}

#[test]
fn a_registry_rebuild_carries_the_credential_and_only_an_unused_cached_image_is_deleted() {
    let rig = Rig::new();
    let source = json!({
        "type": "image", "mode": "pull", "protocol": "oci",
        "server": "https://registry.example",
        "alias": format!("acme/relay@sha256:{}", "a".repeat(64)),
    });
    let never = || false;
    let credential = arca_incus::RegistryCredential {
        username: "pull+only".to_string(),
        password: ["a", "word", "for", "pulls"].join("-"),
    };
    // The address the credential stands in: the user name is written as an address writes it.
    let address = credential
        .address("https://registry.example")
        .expect("an address");
    assert!(address.starts_with("https://pull%2Bonly:") && address.ends_with("@registry.example"));
    assert!(
        credential
            .address("https://registry.example/path")
            .is_none()
    );
    assert!(
        !format!("{credential:?}").contains("word"),
        "a credential does not print itself"
    );
    let seen = rig.run.read(PROJECT, "relay").expect("a function");
    assert_eq!(
        rig.run
            .fresh_root(&seen, None, &source, Some(&credential), &never)
            .expect("rebuilt"),
        FreshRoot::Registry
    );
    assert_eq!(
        rig.run
            .fresh_root(&seen, None, &source, None, &never)
            .expect("rebuilt"),
        FreshRoot::Registry
    );
    assert_eq!(
        rig.fake.rebuilds(PROJECT, "relay"),
        ["registry+credential", "registry"]
    );
    // An image that was put in the store by hand is not a cached one and stays; so does nothing at all.
    let by_hand = rig.fake.add_image(PROJECT, "kept-by-hand");
    assert!(!rig.run.drop_unused_image(PROJECT, &by_hand).expect("asked"));
    assert!(
        !rig.run
            .drop_unused_image(PROJECT, &"e".repeat(64))
            .expect("asked")
    );
    assert!(rig.run.drop_unused_image(PROJECT, "../x").is_err());
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
