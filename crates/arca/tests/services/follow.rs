//! The Automagik follower against a fake Incus and a fake HTTP source: a service of Automagik's server image moves to a
//! newer release by itself, never to an older one, and every change is journalled. Every value is synthetic.
use super::*;
use arca::handlers::{FollowEdges, Follower, UpdateOptions, service_handlers_updating};
use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

const IMAGE: &str = "ghcr.io/automagik-si/automagik-server";
const LANE: &str =
    "https://github.com/automagik-si/automagik/releases/download/server-lane/server-image.json";
const RELEASES: &str = "https://api.github.com/repos/automagik-si/automagik/releases?per_page=10";
const D_NEW: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const D_NEWER: &str = "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

fn release_url(tag: &str) -> String {
    format!("https://github.com/automagik-si/automagik/releases/download/{tag}/server-image.json")
}

fn record(version: &str, digest: &str) -> String {
    json!({
        "image": IMAGE,
        "digest": digest,
        "ref": format!("{IMAGE}:{version}"),
        "version": version,
        "revision": "0123456789abcdef0123456789abcdef01234567",
        "tag": format!("v{version}"),
    })
    .to_string()
}

fn release(tag: &str, asset: bool) -> Value {
    json!({
        "tag_name": tag,
        "draft": false,
        "prerelease": false,
        "assets": if asset { json!([{ "name": "server-image.json" }]) } else { json!([]) },
    })
}

/// A fake source of HTTP bodies: an address with no answer fails as a 404 would.
#[derive(Clone)]
struct Source {
    answers: Arc<Mutex<BTreeMap<String, Result<String, String>>>>,
    asked: Arc<Mutex<Vec<String>>>,
    verify: Arc<Mutex<Result<String, String>>>,
}

impl Source {
    fn new() -> Self {
        Self {
            answers: Arc::default(),
            asked: Arc::default(),
            verify: Arc::new(Mutex::new(Ok("verified".into()))),
        }
    }

    fn set(&self, url: &str, body: &str) {
        self.answers
            .lock()
            .unwrap()
            .insert(url.to_string(), Ok(body.to_string()));
    }

    fn edges(&self) -> FollowEdges {
        let answers = Arc::clone(&self.answers);
        let asked = Arc::clone(&self.asked);
        let verify = Arc::clone(&self.verify);
        FollowEdges {
            fetch: Arc::new(move |url| {
                asked.lock().unwrap().push(url.to_string());
                answers
                    .lock()
                    .unwrap()
                    .get(url)
                    .cloned()
                    .unwrap_or_else(|| Err("curl failed: HTTP 404".into()))
            }),
            verify: Arc::new(move |_| verify.lock().unwrap().clone()),
        }
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

fn templates(rig: &Rig) {
    rig.template(
        "automagik-server",
        json!({
            "name": "automagik-server",
            "kind": "oci",
            "image": IMAGE,
            "digest": DIGEST,
            "ports": [3081],
            "data": { "path": "/data", "uid": 1000, "gid": 1000 },
            "lan": { "port": 3081 },
            "env": {},
            "publishable": false
        }),
    );
    rig.template(
        "app",
        json!({
            "name": "app",
            "kind": "oci",
            "image": "registry.example/app",
            "digest": DIGEST,
            "ports": [8080],
            "data": { "path": "/data", "uid": 1000, "gid": 1000 },
            "env": {},
            "publishable": false
        }),
    );
}

/// A node with the services `automagik` (made from the automagik-server template, running DIGEST) and `app`.
fn rig(up: &Arc<AtomicBool>) -> Rig {
    let mut rig = Rig::with(
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![],
            networks: vec![],
            pools: vec![("pool1".into(), json!({}))],
        },
        vec![PROJECT.to_string()],
        Some("192.0.2.0/24"),
    );
    templates(&rig);
    for (name, template) in [("automagik", "automagik-server"), ("app", "app")] {
        rig.call(
            "service_create",
            json!({ "name": name, "template": template }),
        )
        .expect("create");
    }
    let path = format!("/1.0/projects/{PROJECT}");
    let mut config = rig.get(&path).expect("the project")["config"].clone();
    config["restricted.snapshots"] = json!("allow");
    rig.edit("PATCH", &path, json!({ "config": config }));
    let answers = Arc::clone(up);
    rig.handlers = service_handlers_updating(
        rig.kernel.clone(),
        Arc::new(|| NOW),
        rig.cfg.clone(),
        None,
        update_options(answers),
    )
    .into_iter()
    .collect();
    rig
}

fn update_options(up: Arc<AtomicBool>) -> UpdateOptions {
    UpdateOptions {
        probe: Arc::new(move |_, _, _| up.load(Ordering::SeqCst)),
        pace: Duration::from_millis(10),
    }
}

fn follower(rig: &Rig, up: &Arc<AtomicBool>, source: &Source) -> Follower {
    // A clock that moves, since a snapshot is named after the time.
    static TICKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(NOW);
    Follower::new(
        rig.kernel.clone(),
        Arc::new(|| TICKS.fetch_add(1, Ordering::SeqCst)),
        rig.cfg.clone(),
        update_options(Arc::clone(up)),
        source.edges(),
    )
    .with_health_seconds(1)
    .with_pins(vec![(DIGEST.to_string(), "3.136.4".to_string())])
}

fn alias(rig: &Rig, name: &str) -> String {
    rig.entries()
        .into_iter()
        .find(|entry| entry.id.0 == format!("service-{name}"))
        .expect("the record")
        .config["source"]["alias"]
        .as_str()
        .expect("alias")
        .to_string()
}

fn journal(rig: &Rig) -> Vec<Value> {
    fs::read_to_string(rig.dir.join("service-updates.jsonl"))
        .map(|text| {
            text.lines()
                .map(|line| serde_json::from_str(line).expect("a JSON line"))
                .collect()
        })
        .unwrap_or_default()
}

fn steps(rig: &Rig) -> Vec<String> {
    journal(rig)
        .iter()
        .map(|row| row["step"].as_str().expect("step").to_string())
        .collect()
}

/// The releases the node can see: v3.136.4 is what the template pinned (DIGEST).
fn history(source: &Source) {
    source.set(
        RELEASES,
        &json!([
            release("v3.136.4", true),
            release("v3.136.3", true),
            release("server-lane", true),
        ])
        .to_string(),
    );
    source.set(&release_url("v3.136.4"), &record("3.136.4", DIGEST));
    source.set(
        &release_url("v3.136.3"),
        &record(
            "3.136.3",
            "sha256:9999999999999999999999999999999999999999999999999999999999999999",
        ),
    );
}

#[test]
fn a_service_of_the_automagik_template_moves_to_a_newer_lane_record_and_the_change_is_journalled() {
    let up = Arc::new(AtomicBool::new(true));
    let rig = rig(&up);
    let source = Source::new();
    history(&source);
    source.set(LANE, &record("3.137.0", D_NEW));

    let done = follower(&rig, &up, &source).tick();
    assert_eq!(done.len(), 1, "{done:?}");
    assert_eq!(done[0].service, "automagik");
    assert_eq!(done[0].version, "3.137.0");
    assert_eq!(done[0].outcome, Ok(()));
    assert_eq!(
        alias(&rig, "automagik"),
        format!("automagik-si/automagik-server@{D_NEW}")
    );
    assert_eq!(
        alias(&rig, "app"),
        format!("app@{DIGEST}"),
        "another template is left alone"
    );

    let steps = steps(&rig);
    assert_eq!(
        steps,
        [
            "follow",
            "begin",
            "stop",
            "snapshot",
            "rebuild",
            "start",
            "health",
            "record",
            "end",
            "follow end"
        ]
    );
    let first = &journal(&rig)[0];
    assert_eq!(first["detail"]["from"], DIGEST);
    assert_eq!(first["detail"]["fromVersion"], "3.136.4");
    assert_eq!(first["detail"]["to"], D_NEW);
    assert_eq!(first["detail"]["version"], "3.137.0");
    assert_eq!(first["detail"]["tag"], "v3.137.0");
    assert_eq!(
        first["detail"]["revision"],
        "0123456789abcdef0123456789abcdef01234567"
    );
    assert_eq!(first["detail"]["attestation"]["result"], "verified");
    assert!(
        source.asked().iter().all(|url| !url.contains("latest")),
        "releases/latest is never read: {:?}",
        source.asked()
    );

    // The next look finds the service on the record's digest: nothing changes and nothing is journalled.
    let before = journal(&rig).len();
    assert!(follower(&rig, &up, &source).tick().is_empty());
    assert_eq!(journal(&rig).len(), before);
}

#[test]
fn a_record_whose_version_is_not_newer_never_moves_a_service() {
    let up = Arc::new(AtomicBool::new(true));
    let rig = rig(&up);
    let source = Source::new();
    history(&source);
    // The lane names an older version than the service runs, and another digest.
    source.set(LANE, &record("3.136.3", D_NEW));
    assert!(follower(&rig, &up, &source).tick().is_empty());
    // The same version under another digest does not move it either.
    source.set(LANE, &record("3.136.4", D_NEW));
    assert!(follower(&rig, &up, &source).tick().is_empty());
    assert_eq!(
        alias(&rig, "automagik"),
        format!("automagik-si/automagik-server@{DIGEST}")
    );
    assert!(
        journal(&rig).is_empty(),
        "a look that changes nothing is not journalled"
    );
}

#[test]
fn until_the_lane_exists_the_newest_release_with_the_asset_is_read() {
    let up = Arc::new(AtomicBool::new(true));
    let rig = rig(&up);
    let source = Source::new();
    source.set(
        RELEASES,
        &json!([
            release("v3.136.4", true),
            release("v3.138.0", false),
            release("v3.137.0", true),
            release("not-a-version", true),
        ])
        .to_string(),
    );
    source.set(&release_url("v3.136.4"), &record("3.136.4", DIGEST));
    source.set(&release_url("v3.137.0"), &record("3.137.0", D_NEW));
    // The lane answers 404, as it does until it exists.
    let done = follower(&rig, &up, &source).tick();
    assert_eq!(done.len(), 1, "{done:?}");
    assert_eq!(done[0].version, "3.137.0");
    let asked = source.asked();
    assert_eq!(asked[0], LANE);
    assert!(
        !asked.contains(&release_url("v3.138.0")),
        "a release without the asset is skipped"
    );
    assert!(asked.iter().all(|url| !url.contains("latest")));
}

#[test]
fn a_service_that_said_follow_false_stays_and_one_that_says_true_follows_again() {
    let up = Arc::new(AtomicBool::new(true));
    let rig = rig(&up);
    let source = Source::new();
    history(&source);
    source.set(LANE, &record("3.137.0", D_NEW));

    let said = rig
        .call(
            "service_update",
            json!({ "name": "automagik", "follow": false }),
        )
        .expect("opt out");
    assert_eq!(said["follow"], false);
    let status = rig
        .call("service_status", json!({ "name": "automagik" }))
        .expect("status");
    assert_eq!(status["follow"], false);
    assert!(follower(&rig, &up, &source).tick().is_empty());
    assert!(
        source.asked().is_empty(),
        "no service follows, so nothing is asked of the network"
    );
    assert_eq!(
        alias(&rig, "automagik"),
        format!("automagik-si/automagik-server@{DIGEST}")
    );
    assert_eq!(steps(&rig), ["follow set"]);

    rig.call(
        "service_update",
        json!({ "name": "automagik", "follow": true }),
    )
    .expect("opt in");
    assert_eq!(follower(&rig, &up, &source).tick().len(), 1);
    assert_eq!(
        alias(&rig, "automagik"),
        format!("automagik-si/automagik-server@{D_NEW}")
    );
}

#[test]
fn follow_is_refused_for_a_service_that_cannot_follow_and_never_with_a_digest() {
    let up = Arc::new(AtomicBool::new(true));
    let rig = rig(&up);
    let refusal = rig
        .call("service_update", json!({ "name": "app", "follow": true }))
        .expect_err("app is not Automagik's image");
    let CallError::Refused { message, .. } = refusal else {
        panic!("a refusal");
    };
    assert!(message.contains("Nothing was changed"), "{message}");
    assert!(matches!(
        rig.call(
            "service_update",
            json!({ "name": "automagik", "follow": true, "digest": D_NEW })
        ),
        Err(CallError::Invalid(_))
    ));
    // A service that is not Automagik's is never followed, whatever its record says.
    assert!(
        rig.call("service_status", json!({ "name": "app" }))
            .expect("status")
            .get("follow")
            .is_none()
    );
}

#[test]
fn a_failed_provenance_check_is_logged_in_the_journal_and_never_stops_the_update() {
    let up = Arc::new(AtomicBool::new(true));
    let rig = rig(&up);
    let source = Source::new();
    history(&source);
    source.set(LANE, &record("3.137.0", D_NEW));
    *source.verify.lock().unwrap() = Err("gh could not run".into());

    let done = follower(&rig, &up, &source).tick();
    assert_eq!(done[0].outcome, Ok(()));
    let first = &journal(&rig)[0];
    assert_eq!(first["detail"]["attestation"]["result"], "failed");
    assert!(
        first["detail"]["attestation"]["detail"]
            .as_str()
            .expect("words")
            .contains("gh could not run")
    );
}

#[test]
fn an_attestation_the_api_lists_is_journalled_as_found_when_gh_cannot_check() {
    let up = Arc::new(AtomicBool::new(true));
    let rig = rig(&up);
    let source = Source::new();
    history(&source);
    source.set(LANE, &record("3.137.0", D_NEW));
    *source.verify.lock().unwrap() = Err("gh is not installed".into());
    source.set(
        &format!("https://api.github.com/repos/automagik-si/automagik/attestations/{D_NEW}"),
        &json!({ "attestations": [{ "bundle": {} }] }).to_string(),
    );
    follower(&rig, &up, &source).tick();
    assert_eq!(
        journal(&rig)[0]["detail"]["attestation"]["result"],
        "found, not verified"
    );
}

#[test]
fn an_update_that_fails_is_rolled_back_journalled_and_not_tried_again_for_the_same_digest() {
    let up = Arc::new(AtomicBool::new(false));
    let rig = rig(&up);
    let source = Source::new();
    history(&source);
    source.set(LANE, &record("3.137.0", D_NEW));
    let follower = follower(&rig, &up, &source);

    let done = follower.tick();
    assert_eq!(done.len(), 1);
    let why = done[0]
        .outcome
        .clone()
        .expect_err("the port never answered");
    assert!(why.contains("rolled back"), "{why}");
    assert_eq!(
        alias(&rig, "automagik"),
        format!("automagik-si/automagik-server@{DIGEST}")
    );
    let steps = steps(&rig);
    assert_eq!(steps.last().map(String::as_str), Some("follow end"));
    assert_eq!(journal(&rig).last().expect("a line")["ok"], false);

    let lines = journal(&rig).len();
    assert!(
        follower.tick().is_empty(),
        "the same digest is not tried again"
    );
    assert_eq!(journal(&rig).len(), lines);

    // A newer record is tried.
    up.store(true, Ordering::SeqCst);
    source.set(LANE, &record("3.137.1", D_NEWER));
    let done = follower.tick();
    assert_eq!(done[0].outcome, Ok(()));
    assert_eq!(
        alias(&rig, "automagik"),
        format!("automagik-si/automagik-server@{D_NEWER}")
    );
}

#[test]
fn a_record_for_another_image_or_with_a_bad_digest_moves_nothing() {
    let up = Arc::new(AtomicBool::new(true));
    let rig = rig(&up);
    let source = Source::new();
    history(&source);
    for body in [
        json!({ "image": "ghcr.io/other/image", "digest": D_NEW, "ref": "r", "version": "9.0.0", "revision": "x", "tag": "t" }),
        json!({ "image": IMAGE, "digest": "sha256:xyz", "ref": "r", "version": "9.0.0", "revision": "x", "tag": "t" }),
        json!({ "image": IMAGE, "digest": D_NEW, "ref": "r", "version": "latest", "revision": "x", "tag": "t" }),
    ] {
        source.set(LANE, &body.to_string());
        assert!(follower(&rig, &up, &source).tick().is_empty());
    }
    assert!(journal(&rig).is_empty());
}

/// The steps of the journal that are the follower's own.
fn followed_lines(rig: &Rig) -> Vec<Value> {
    journal(rig)
        .into_iter()
        .filter(|row| {
            row["step"]
                .as_str()
                .is_some_and(|step| step.starts_with("follow"))
        })
        .collect()
}

#[test]
fn a_service_moves_however_many_releases_have_been_cut_since_its_image_was_pinned() {
    let up = Arc::new(AtomicBool::new(true));
    let rig = rig(&up);
    let source = Source::new();
    // Automagik cut dozens of releases since: the API's first page names none that carries a record for DIGEST.
    let many: Vec<Value> = (1..=40)
        .map(|n| release(&format!("v3.200.{n}"), false))
        .collect();
    source.set(RELEASES, &Value::Array(many).to_string());
    source.set(LANE, &record("3.137.0", D_NEW));
    let done = follower(&rig, &up, &source).tick();
    assert_eq!(done.len(), 1, "{done:?}");
    assert_eq!(done[0].outcome, Ok(()));
    assert_eq!(
        alias(&rig, "automagik"),
        format!("automagik-si/automagik-server@{D_NEW}")
    );
}

#[test]
fn the_version_a_service_runs_is_kept_in_its_record_and_survives_a_restart_of_the_follower() {
    let up = Arc::new(AtomicBool::new(true));
    let rig = rig(&up);
    let source = Source::new();
    source.set(LANE, &record("3.137.0", D_NEW));
    assert_eq!(follower(&rig, &up, &source).tick().len(), 1);

    // A new follower knows nothing from memory, and no pin names D_NEW: the record says 3.137.0.
    source.set(LANE, &record("3.137.1", D_NEWER));
    let restarted = follower(&rig, &up, &source).with_pins(Vec::new());
    let done = restarted.tick();
    assert_eq!(done.len(), 1, "{done:?}");
    assert_eq!(done[0].outcome, Ok(()));
    assert_eq!(
        alias(&rig, "automagik"),
        format!("automagik-si/automagik-server@{D_NEWER}")
    );
}

#[test]
fn a_service_on_the_lanes_own_digest_has_its_version_seeded_from_the_record() {
    let up = Arc::new(AtomicBool::new(true));
    let rig = rig(&up);
    let source = Source::new();
    // The lane names the digest the service runs, as 3.136.4. No pin is known and no release is listed.
    source.set(LANE, &record("3.136.4", DIGEST));
    assert!(
        follower(&rig, &up, &source)
            .with_pins(Vec::new())
            .tick()
            .is_empty()
    );
    // Later the lane moves on: the seeded version orders the service, on another follower with no memory.
    source.set(LANE, &record("3.137.0", D_NEW));
    let done = follower(&rig, &up, &source).with_pins(Vec::new()).tick();
    assert_eq!(done.len(), 1, "{done:?}");
    assert_eq!(done[0].outcome, Ok(()));
}

#[test]
fn a_version_nobody_knows_is_journalled_once_as_the_reason_nothing_moved() {
    let up = Arc::new(AtomicBool::new(true));
    let rig = rig(&up);
    let source = Source::new();
    source.set(LANE, &record("3.137.0", D_NEW));
    let follower = follower(&rig, &up, &source).with_pins(Vec::new());
    assert!(follower.tick().is_empty());
    assert!(follower.tick().is_empty());
    let lines = followed_lines(&rig);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["step"], "follow skipped");
    assert!(
        lines[0]["detail"]["why"]
            .as_str()
            .expect("words")
            .contains("version"),
        "{lines:?}"
    );
    assert_eq!(
        alias(&rig, "automagik"),
        format!("automagik-si/automagik-server@{DIGEST}")
    );
}

#[test]
fn a_service_whose_update_is_still_unsettled_in_incus_is_held_and_moves_once_incus_is_idle() {
    let up = Arc::new(AtomicBool::new(true));
    let rig = rig(&up);
    let source = Source::new();
    source.set(LANE, &record("3.137.0", D_NEW));
    let operation = rig.fake.hold_operation(&arca_kernel::ownership::ObjectRef {
        kind: arca_kernel::ownership::IncusKind::Instance,
        project: PROJECT.to_string(),
        parent: None,
        name: "automagik".to_string(),
        uuid: None,
    });
    let follower = follower(&rig, &up, &source);
    assert!(follower.tick().is_empty());
    assert!(follower.tick().is_empty());
    assert!(rig.fake.rebuilds(PROJECT, "automagik").is_empty());
    let lines = followed_lines(&rig);
    assert_eq!(lines.len(), 1, "the hold is journalled once: {lines:?}");
    assert_eq!(lines[0]["step"], "follow held");

    rig.fake.release_operation(&operation);
    let done = follower.tick();
    assert_eq!(done.len(), 1, "{done:?}");
    assert_eq!(done[0].outcome, Ok(()));
}

#[test]
fn a_stopped_service_is_not_started_by_the_follower_and_moves_once_it_runs() {
    let up = Arc::new(AtomicBool::new(true));
    let rig = rig(&up);
    let source = Source::new();
    source.set(LANE, &record("3.137.0", D_NEW));
    rig.edit(
        "PUT",
        &format!("/1.0/instances/automagik/state?project={PROJECT}"),
        json!({ "action": "stop", "force": true }),
    );
    assert_eq!(
        rig.get(&format!("/1.0/instances/automagik?project={PROJECT}"))
            .expect("instance")["status"],
        "Stopped"
    );
    let follower = follower(&rig, &up, &source);
    assert!(follower.tick().is_empty());
    assert!(follower.tick().is_empty());
    assert!(rig.fake.rebuilds(PROJECT, "automagik").is_empty());
    assert_eq!(
        rig.get(&format!("/1.0/instances/automagik?project={PROJECT}"))
            .expect("instance")["status"],
        "Stopped",
        "the owner's stop stands"
    );
    let lines = followed_lines(&rig);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["step"], "follow skipped");
    assert!(
        lines[0]["detail"]["why"]
            .as_str()
            .expect("words")
            .contains("stopped")
    );

    rig.edit(
        "PUT",
        &format!("/1.0/instances/automagik/state?project={PROJECT}"),
        json!({ "action": "start" }),
    );
    let done = follower.tick();
    assert_eq!(done.len(), 1, "{done:?}");
    assert_eq!(done[0].outcome, Ok(()));
}

#[test]
fn the_image_a_move_leaves_unused_is_deleted_from_the_image_store() {
    let up = Arc::new(AtomicBool::new(true));
    let rig = rig(&up);
    let source = Source::new();
    source.set(LANE, &record("3.137.0", D_NEW));
    // Incus records on a container the image it was made from; the fake leaves that to a rebuild, so say it here.
    let old = arca_kernel::canon::Digest::of(
        format!("https://ghcr.io|automagik-si/automagik-server@{DIGEST}").as_bytes(),
    )
    .to_hex();
    rig.fake
        .set_instance_config(PROJECT, "automagik", "volatile.base_image", &old);
    let before = rig.fake.images();
    assert!(
        before.iter().any(|(_, fingerprint)| *fingerprint == old),
        "the old image is in the store: {before:?} {old}"
    );
    assert!(
        !before.is_empty(),
        "the create fetched the image: {before:?}"
    );
    assert_eq!(follower(&rig, &up, &source).tick().len(), 1);
    let after = rig.fake.images();
    let gone: Vec<_> = before.iter().filter(|old| !after.contains(old)).collect();
    let added: Vec<_> = after.iter().filter(|new| !before.contains(new)).collect();
    assert_eq!(
        gone.len(),
        1,
        "the image the service left is gone: {before:?} then {after:?}; steps {:?} config {:?}",
        steps(&rig),
        rig.get(&format!("/1.0/instances/automagik?project={PROJECT}"))
            .map(|v| v["config"].clone())
    );
    assert_eq!(
        added.len(),
        1,
        "the image it runs now is kept: {before:?} then {after:?}"
    );
    assert!(steps(&rig).contains(&"follow image".to_string()));
}

#[test]
fn github_down_or_answering_with_something_that_is_not_json_moves_nothing_and_writes_nothing() {
    let up = Arc::new(AtomicBool::new(true));
    let rig = rig(&up);
    let source = Source::new();
    // Nothing answers: the lane and the releases both fail.
    assert!(follower(&rig, &up, &source).tick().is_empty());
    // Both answer with a page that is not JSON.
    source.set(LANE, "<html>unicorn</html>");
    source.set(RELEASES, "<html>unicorn</html>");
    assert!(follower(&rig, &up, &source).tick().is_empty());
    assert!(journal(&rig).is_empty());
    assert_eq!(
        alias(&rig, "automagik"),
        format!("automagik-si/automagik-server@{DIGEST}")
    );
}

#[test]
fn an_opt_out_survives_a_restart_of_the_follower() {
    let up = Arc::new(AtomicBool::new(true));
    let rig = rig(&up);
    let source = Source::new();
    source.set(LANE, &record("3.137.0", D_NEW));
    rig.call(
        "service_update",
        json!({ "name": "automagik", "follow": false }),
    )
    .expect("opt out");
    assert!(follower(&rig, &up, &source).tick().is_empty());
    assert!(follower(&rig, &up, &source).tick().is_empty());
    assert_eq!(
        alias(&rig, "automagik"),
        format!("automagik-si/automagik-server@{DIGEST}")
    );
}
