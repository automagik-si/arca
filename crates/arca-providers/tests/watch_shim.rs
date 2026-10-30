//! The watch through the recording shim. The read-only provider answers the watch's one listing through a shim in front of a fake
//! Incus, and the shim's own log is the proof: every request it saw is a GET, and nothing was refused. No test here connects
//! to a real Incus socket, and every server is bound under a fresh temporary directory.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use arca_incus::{FakeIncus, FakeState, RecordingShim, UnixIncus};
use arca_kernel::desired::{EntryId, PluginId};
use arca_kernel::journal::{EffectClass, Journal, MemJournal};
use arca_kernel::ownership::IncusKind;
use arca_kernel::plugin::{
    Ctx, EffectOp, EffectRefusal, KeySpec, Manifest, Observe, Planned, Plugin, Provider,
};
use arca_providers::ReadOnlyIncusProvider;
use arca_watch::{INSTANCES_PATH, Watch, WatchConfig, watch_once};
use serde_json::{Value, json};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../arca-incus/tests/fixtures/state.json"
);

/// A fresh temporary directory, removed when the test ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .subsec_nanos();
        let dir = std::env::temp_dir().join(format!(
            "arca-providers-{}-{nanos}-{tag}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create the scratch directory");
        Scratch(dir)
    }

    fn at(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A fake Incus serving the fixture, and a recording shim in front of it, both under `scratch`.
struct Rig {
    fake: FakeIncus,
    shim: arca_incus::ShimHandle,
    log: PathBuf,
}

fn rig(scratch: &Scratch) -> Rig {
    let state = FakeState::load(Path::new(FIXTURE)).expect("load the fixture");
    let fake = FakeIncus::start(&scratch.0, state).expect("start the fake");
    let log = scratch.at("shim.log");
    let shim = RecordingShim::start(fake.socket(), &scratch.at("shim.sock"), &log)
        .expect("start the recording shim");
    Rig { fake, shim, log }
}

fn host(rig: &Rig) -> ReadOnlyIncusProvider<UnixIncus> {
    ReadOnlyIncusProvider::new(UnixIncus::new(rig.shim.socket().to_path_buf()))
}

fn entry() -> EntryId {
    EntryId("watch".to_string())
}

/// The shim's log, one parsed line per request.
fn log_lines(log: &Path) -> Vec<Value> {
    fs::read_to_string(log)
        .expect("read the shim log")
        .lines()
        .map(|line| serde_json::from_str(line).expect("a log line is JSON"))
        .collect()
}

fn fixture_projects() -> Vec<String> {
    ["default", "droid", "bench", "svc"]
        .iter()
        .map(|p| p.to_string())
        .collect()
}

#[test]
fn the_listing_path_the_provider_serves_is_the_watchs_path() {
    // The provider answers the one path the watch reads. A rename on either side would silently refuse the watch.
    assert_eq!(
        INSTANCES_PATH,
        "/1.0/instances?all-projects=true&recursion=1"
    );
}

#[test]
fn a_watch_through_the_shim_classes_the_fixture_and_the_shim_refuses_nothing() {
    let scratch = Scratch::new("classes");
    let rig = rig(&scratch);
    let mut provider = host(&rig);
    let cfg = WatchConfig {
        identity_phones: ["phone1".to_string()].into_iter().collect(),
        projects: fixture_projects().into_iter().collect(),
        ..WatchConfig::default()
    };
    let report = watch_once(&mut provider, &cfg, 1_000).expect("the watch runs through the shim");

    let total: usize = report.projects.iter().map(|p| p.instances.len()).sum();
    assert_eq!(total, 6, "every fixture instance is classed once");

    let summary = rig.shim.stop().expect("stop the shim");
    assert_eq!(summary.refused, 0, "the shim refused nothing");
    assert_eq!(
        summary.get, 1,
        "the watch makes exactly one read, its listing: {summary:?}"
    );

    let lines = log_lines(&rig.log);
    assert!(!lines.is_empty(), "the shim logged its requests");
    for line in &lines {
        assert_eq!(
            line["method"],
            json!("GET"),
            "a logged request is a GET: {line}"
        );
        assert_eq!(
            line["forwarded"],
            json!(true),
            "every request was forwarded: {line}"
        );
    }

    let sent = rig.fake.requests();
    assert!(!sent.is_empty());
    assert!(
        sent.iter().all(|r| r.method == "GET"),
        "the fake received only GETs: {sent:?}"
    );
}

#[test]
fn the_host_names_every_project_through_the_shim_and_plugins_cannot_ask_for_them() {
    // The project index is the host's own enumeration. It is an inherent method, so the only path to it is the provider value
    // the host holds, and the shim proves it is a GET.
    let scratch = Scratch::new("project-index");
    let rig = rig(&scratch);
    let provider = host(&rig);
    let mut names = provider.project_names().expect("the project index is read");
    names.sort();
    assert_eq!(names, vec!["bench", "default", "droid", "svc"]);
    let summary = rig.shim.stop().expect("stop the shim");
    assert_eq!(summary.get, 1);
    assert_eq!(summary.refused, 0);
}

#[test]
fn the_provider_drops_every_row_outside_the_manifests_projects_before_the_caller_sees_it() {
    // The plugin filters rows too, so the provider's own scoping is tested on its own: a caller that reads the provider directly
    // must get only the named projects.
    let scratch = Scratch::new("provider-scope");
    let rig = rig(&scratch);
    let provider = host(&rig);
    let answer = provider
        .observe(
            &entry(),
            &["droid".to_string()],
            &Observe::IncusGet {
                path: INSTANCES_PATH.to_string(),
            },
        )
        .expect("the listing is answered");
    let names: Vec<&str> = answer
        .as_array()
        .expect("the answer is a list")
        .iter()
        .map(|row| row["name"].as_str().expect("a name"))
        .collect();
    assert_eq!(
        names,
        vec!["phone1", "desk1"],
        "only the droid rows are answered"
    );
    let none = provider
        .observe(
            &entry(),
            &[],
            &Observe::IncusGet {
                path: INSTANCES_PATH.to_string(),
            },
        )
        .expect("an empty scope is answered");
    assert_eq!(none, json!([]), "an empty scope answers no rows");
    rig.shim.stop().expect("stop the shim");
}

#[test]
fn a_watch_scoped_to_one_project_reads_only_that_project_through_the_shim() {
    let scratch = Scratch::new("scoped");
    let rig = rig(&scratch);
    let mut provider = host(&rig);
    let cfg = WatchConfig {
        projects: ["droid".to_string()].into_iter().collect(),
        ..WatchConfig::default()
    };
    let report = watch_once(&mut provider, &cfg, 1).expect("the scoped watch runs");
    let projects: Vec<&str> = report.projects.iter().map(|p| p.project.as_str()).collect();
    assert_eq!(
        projects,
        vec!["droid"],
        "only the scoped project is reported"
    );
    let names: Vec<&str> = report.projects[0]
        .instances
        .iter()
        .map(|i| i.name.as_str())
        .collect();
    assert_eq!(
        names,
        vec!["desk1", "phone1"],
        "only the scoped project's instances are reported"
    );
    rig.shim.stop().expect("stop the shim");
}

#[test]
fn a_read_outside_the_listing_is_refused_and_never_reaches_incus() {
    let scratch = Scratch::new("outside");
    let rig = rig(&scratch);
    let provider = host(&rig);
    let refused = provider.observe(
        &entry(),
        &fixture_projects(),
        &Observe::IncusGet {
            path: "/1.0/instances/db1?project=svc".to_string(),
        },
    );
    assert_eq!(refused, Err(EffectRefusal::NotDeclared));
    let host_addresses = provider.observe(&entry(), &fixture_projects(), &Observe::HostAddresses);
    assert_eq!(
        host_addresses,
        Err(EffectRefusal::Provider("kernel provider only".to_string()))
    );
    let summary = rig.shim.stop().expect("stop the shim");
    assert_eq!(summary.get, 0, "the refused reads never reached the shim");
    assert!(
        rig.fake.requests().is_empty(),
        "the fake saw none of the refused reads"
    );
}

#[test]
fn the_read_only_provider_refuses_every_plan_and_every_effect() {
    let scratch = Scratch::new("refuse-writes");
    let rig = rig(&scratch);
    let mut provider = host(&rig);
    let op = EffectOp::IncusCreate {
        kind: IncusKind::Instance,
        project: "droid".to_string(),
        name: "probe1".to_string(),
        body: json!({ "type": "container" }),
    };
    assert_eq!(
        provider.plan(&entry(), &op).map(|_| ()),
        Err(EffectRefusal::WatchOnly)
    );
    let planned = Planned {
        class: EffectClass::A,
        op_digest: arca_kernel::plugin::op_digest(&op),
        pre_image: None,
        etag: None,
    };
    assert_eq!(
        provider.effect(&entry(), &op, &planned).map(|_| ()),
        Err(EffectRefusal::WatchOnly)
    );
    rig.shim.stop().expect("stop the shim");
    assert!(rig.fake.requests().is_empty(), "no write reached the fake");
}

/// A plugin manifest that declares an instance effect in `droid`, so a write passes every check `Ctx` makes before the
/// provider is asked.
fn writer_manifest() -> Manifest {
    Manifest {
        id: PluginId("arca.probe".to_string()),
        version: "0.1.0".to_string(),
        provides: vec![KeySpec {
            key: "watch.report".to_string(),
            version: "0.1.0".to_string(),
        }],
        effects: vec![(IncusKind::Instance, EffectClass::A)],
        projects: vec!["droid".to_string()],
        observes: false,
    }
}

#[test]
fn an_effect_from_the_watch_is_refused_and_journals_nothing() {
    let scratch = Scratch::new("watch-effect");
    let rig = rig(&scratch);
    let mut provider = host(&rig);
    let manifest = Watch::manifest();
    let mut journal = Journal::open(MemJournal::new()).expect("open the journal");
    let op = EffectOp::IncusCreate {
        kind: IncusKind::Instance,
        project: "droid".to_string(),
        name: "probe1".to_string(),
        body: json!({ "type": "container" }),
    };
    {
        let mut ctx = Ctx::new(entry(), &manifest, &mut provider, &mut journal, 1);
        assert_eq!(ctx.effect(op), Err(EffectRefusal::WatchOnly));
    }
    assert!(
        journal.live(&entry()).is_empty(),
        "no record for the refused effect"
    );
    assert!(journal.unresolved().is_empty(), "no intent is left open");
    let summary = rig.shim.stop().expect("stop the shim");
    assert_eq!(summary.refused, 0);
    assert!(rig.fake.requests().is_empty(), "no write reached the fake");
}

#[test]
fn a_planned_write_is_refused_by_the_provider_before_any_intent_is_journalled() {
    let scratch = Scratch::new("planned-write");
    let rig = rig(&scratch);
    let mut provider = host(&rig);
    let manifest = writer_manifest();
    let mut journal = Journal::open(MemJournal::new()).expect("open the journal");
    let op = EffectOp::IncusCreate {
        kind: IncusKind::Instance,
        project: "droid".to_string(),
        name: "probe1".to_string(),
        body: json!({ "type": "container" }),
    };
    {
        let mut ctx = Ctx::new(entry(), &manifest, &mut provider, &mut journal, 1);
        // Every check Ctx makes before the plan passes; the read-only provider's plan is what refuses, before any intent.
        assert_eq!(ctx.effect(op), Err(EffectRefusal::WatchOnly));
    }
    assert!(
        journal.live(&entry()).is_empty(),
        "the refused plan left no record"
    );
    assert!(journal.unresolved().is_empty());
    rig.shim.stop().expect("stop the shim");
    assert!(rig.fake.requests().is_empty(), "no write reached the fake");
}

#[test]
fn a_raw_post_is_refused_at_the_socket_even_if_a_caller_bypassed_ctx() {
    // The shim is the independent witness: a caller that skipped Ctx would still be refused at the socket.
    let scratch = Scratch::new("raw-post");
    let rig = rig(&scratch);
    let mut stream = UnixStream::connect(rig.shim.socket()).expect("connect to the shim");
    stream
        .write_all(b"POST /1.0/instances HTTP/1.1\r\nHost: incus\r\nContent-Length: 0\r\n\r\n")
        .expect("write the request");
    let mut reply = String::new();
    let _ = stream.read_to_string(&mut reply);
    assert!(
        reply.starts_with("HTTP/1.1 405"),
        "a POST is answered 405: {reply}"
    );
    drop(stream);
    let summary = rig.shim.stop().expect("stop the shim");
    assert_eq!(summary.refused, 1);
    assert!(
        rig.fake.requests().is_empty(),
        "the fake never saw the POST"
    );
    let lines = log_lines(&rig.log);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["forwarded"], json!(false));
}
