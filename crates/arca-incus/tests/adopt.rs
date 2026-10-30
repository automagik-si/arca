#![cfg(feature = "write")]
//! The adoption writer's step table, step by step, against the fake Incus. The fake models the stop, the start, snapshots, the
//! cross-project move (a copy under a new uuid, as Incus 7.5 runs it), the DHCP lease table and Incus's `deviceVolatileReset`.
//! Every refusal is checked twice: the writer returns `IncusError::Refused`, and the fake records no write.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use arca_incus::{
    AdoptStep, AdoptTarget, AdoptWriter, ExpectClass, FakeIncus, FakeState, IncusError, IncusRead,
    PreImage, Side, StepOutcome, UnixIncus, adoption_project_body, pin_patch,
};
use arca_kernel::ownership::{IncusKind, ObjectRef};
use serde_json::{Value, json};

const MAC: &str = "10:66:6a:00:00:01";
const IP: &str = "192.0.2.131";
const UUID: &str = "00000000-0000-4000-8000-0000000000a1";
const TO: &str = "arcak-adopt-svc1";
const SNAPSHOT: &str = "arca-adopt-0123456789ab";
const MARK: &str = "user.arcakernel.entry";

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .subsec_nanos();
        let dir =
            std::env::temp_dir().join(format!("arca-adopt-{}-{nanos}-{tag}", std::process::id()));
        fs::create_dir_all(&dir).expect("create the scratch directory");
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The instance the tests adopt: a running container in `default`, whose NIC comes from the default profile and whose root is
/// its own.
fn svc1(config: Value) -> Value {
    let mut own =
        json!({ "volatile.uuid": UUID, "volatile.eth0.hwaddr": MAC, "image.os": "debian" });
    if let (Some(own), Value::Object(extra)) = (own.as_object_mut(), config) {
        own.extend(extra);
    }
    json!({
        "name": "svc1", "project": "default", "type": "container", "status": "Running",
        "created_at": "2026-01-01T00:00:00Z", "profiles": ["default"], "config": own,
        "devices": { "root": { "type": "disk", "path": "/", "pool": "pool1", "size": "60GiB" } },
    })
}

fn state(instance: Value) -> FakeState {
    FakeState {
        server: json!({ "api_version": "1.0" }),
        projects: vec![
            json!({ "name": "default", "description": "", "config": {} }),
            json!({ "name": "bench", "description": "", "config": {} }),
        ],
        instances: vec![instance],
        pools: vec![("pool1".to_string(), json!({}))],
        networks: vec![
            json!({ "name": "br1", "project": "default", "type": "bridge",
            "config": { "ipv4.address": "192.0.2.1/24" } }),
        ],
    }
}

fn start_with(tag: &str, instance: Value) -> (Scratch, FakeIncus) {
    let dir = Scratch::new(tag);
    let fake = FakeIncus::start(&dir.0, state(instance)).expect("start the fake");
    fake.add_profile(
        "default",
        json!({ "name": "default", "config": {},
            "devices": { "eth0": { "type": "nic", "name": "eth0", "network": "br1" } } }),
    );
    fake.set_lease("br1", MAC, IP);
    (dir, fake)
}

fn start(tag: &str) -> (Scratch, FakeIncus) {
    start_with(tag, svc1(json!({})))
}

fn target() -> AdoptTarget {
    AdoptTarget {
        instance: "svc1".to_string(),
        to: TO.to_string(),
    }
}

fn device(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

fn pre() -> PreImage {
    PreImage {
        uuid: UUID.to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        mac: MAC.to_string(),
        ipv4: IP.parse().expect("an address"),
        profiles: vec!["default".to_string()],
        config: device(&[("image.os", "debian")]),
        devices: BTreeMap::from([
            (
                "eth0".to_string(),
                device(&[("type", "nic"), ("name", "eth0"), ("network", "br1")]),
            ),
            (
                "root".to_string(),
                device(&[
                    ("type", "disk"),
                    ("path", "/"),
                    ("pool", "pool1"),
                    ("size", "60GiB"),
                ]),
            ),
        ]),
        config_hash: "0".repeat(64),
    }
}

fn writer(fake: &FakeIncus) -> AdoptWriter {
    AdoptWriter::new(
        fake.socket().to_path_buf(),
        target(),
        vec!["br1".to_string()],
        pre(),
    )
    .expect("the writer")
    .with_snapshot(SNAPSHOT)
    .expect("the snapshot name")
}

fn read(fake: &FakeIncus, path: &str) -> Result<Value, IncusError> {
    UnixIncus::new(fake.socket().to_path_buf()).get(path)
}

fn instance_in(fake: &FakeIncus, project: &str) -> Option<Value> {
    read(fake, &format!("/1.0/instances/svc1?project={project}")).ok()
}

fn nic_state(fake: &FakeIncus, project: &str) -> (String, String) {
    let state = read(
        fake,
        &format!("/1.0/instances/svc1/state?project={project}"),
    )
    .expect("the state reads");
    let eth0 = &state["network"]["eth0"];
    (
        eth0["hwaddr"].as_str().unwrap_or("").to_string(),
        eth0["addresses"][0]["address"]
            .as_str()
            .unwrap_or("")
            .to_string(),
    )
}

/// A request sent straight to the fake, past every writer check: the status and the envelope.
fn raw(fake: &FakeIncus, method: &str, path: &str, body: &Value) -> (u16, Value) {
    let text = body.to_string();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: incus\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        text.len()
    );
    let mut stream = UnixStream::connect(fake.socket()).expect("connect to the fake");
    stream.write_all(request.as_bytes()).expect("send");
    let mut reply = String::new();
    stream.read_to_string(&mut reply).expect("read");
    let status = reply
        .get(9..12)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let body = reply
        .split_once("\r\n\r\n")
        .and_then(|(_, body)| serde_json::from_str(body).ok())
        .unwrap_or(Value::Null);
    (status, body)
}

fn refused(result: Result<StepOutcome, IncusError>, why: &str) {
    assert!(
        matches!(result, Err(IncusError::Refused(_))),
        "{why}: {result:?}"
    );
}

/// The method and path of every write the fake has seen.
fn writes(fake: &FakeIncus) -> Vec<(String, String)> {
    fake.writes()
        .into_iter()
        .map(|write| (write.method, write.path))
        .collect()
}

#[test]
fn the_expected_class_and_project_of_each_step_follow_the_table() {
    use AdoptStep as S;
    let table = [
        (
            S::CreateProject,
            Side::Default,
            ExpectClass::Foreign,
            "Running",
        ),
        (S::Stop, Side::Default, ExpectClass::Foreign, "Running"),
        (S::Snapshot, Side::Default, ExpectClass::Foreign, "Stopped"),
        (S::PinNic, Side::Default, ExpectClass::Foreign, "Stopped"),
        (S::Move, Side::Default, ExpectClass::Foreign, "Stopped"),
        (S::Mark, Side::To, ExpectClass::Foreign, "Stopped"),
        (S::Start, Side::To, ExpectClass::Adopted, "Stopped"),
        (S::ReleaseStop, Side::To, ExpectClass::Adopted, "Running"),
        (S::Unmark, Side::To, ExpectClass::Adopted, "Stopped"),
        (S::MoveBack, Side::To, ExpectClass::Foreign, "Stopped"),
        (
            S::ReleaseStart,
            Side::Default,
            ExpectClass::Foreign,
            "Stopped",
        ),
    ];
    for (step, project, class, status) in table {
        let expect = AdoptWriter::expect(step);
        assert_eq!(
            (expect.project, expect.class, expect.status),
            (project, class, status),
            "{step:?}"
        );
    }
}

#[test]
fn an_adoption_keeps_the_mac_and_ip_and_each_step_sends_only_its_own_request_once() {
    let (_dir, fake) = start("full");
    let writer = writer(&fake);
    let before = instance_in(&fake, "default").expect("the instance is in default");
    for step in AdoptStep::ADOPT {
        let sent = fake.writes().len();
        assert_eq!(writer.run(step), Ok(StepOutcome::Sent), "{step:?} runs");
        let mine: Vec<(String, String)> = writes(&fake)[sent..].to_vec();
        let table: Vec<(String, String)> = writer
            .requests(step)
            .into_iter()
            .filter(|request| request.only_if.is_none())
            .map(|request| (request.method.to_string(), request.path))
            .collect();
        assert_eq!(
            mine, table,
            "{step:?} sends its table requests and nothing else"
        );
        // Run again, the step's outcome holds, so it sends nothing.
        let sent = fake.writes().len();
        assert_eq!(
            writer.run(step),
            Ok(StepOutcome::AlreadyDone),
            "{step:?} again"
        );
        assert_eq!(fake.writes().len(), sent, "{step:?} again sends nothing");
    }

    assert!(
        instance_in(&fake, "default").is_none(),
        "the instance left default"
    );
    let after = instance_in(&fake, TO).expect("the instance is in its adoption project");
    assert_eq!(after["status"], json!("Running"));
    assert_eq!(after["config"][MARK], json!("adopt-svc1"));
    let copied = after["config"]["volatile.uuid"].as_str().expect("a uuid");
    assert_ne!(copied, UUID, "the move is a copy, under a new uuid");
    assert_eq!(writer.uuid(), copied, "the writer follows the copy");
    let kept = |body: &Value| -> BTreeMap<String, Value> {
        body["config"]
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(key, _)| !key.starts_with("volatile.") && key.as_str() != MARK)
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    };
    assert_eq!(
        kept(&after),
        kept(&before),
        "every other own config key is unchanged"
    );
    let pin = writer.pin().clone();
    assert_eq!(
        after["devices"]["eth0"],
        pin_patch(&pin)["devices"]["eth0"],
        "the local eth0 is the pin"
    );
    assert_eq!(
        after["devices"]["root"], before["devices"]["root"],
        "the root is unchanged"
    );
    assert_eq!(
        nic_state(&fake, TO),
        (MAC.to_string(), IP.to_string()),
        "the MAC and IPv4 are kept"
    );
    assert_eq!(
        fake.leases(),
        vec![("br1".to_string(), MAC.to_string(), IP.to_string())],
        "the lease is unchanged"
    );
    let snapshot = read(
        &fake,
        &format!("/1.0/instances/svc1/snapshots/{SNAPSHOT}?project={TO}"),
    );
    assert!(
        snapshot.is_ok(),
        "the snapshot moved with the instance: {snapshot:?}"
    );

    // The whole run wrote no network, profile or ACL, deleted nothing and sent no PATCH to a project.
    for (method, path) in writes(&fake) {
        assert_ne!(method, "DELETE", "{path}");
        assert!(
            !path.starts_with("/1.0/networks")
                && !path.starts_with("/1.0/profiles")
                && !path.starts_with("/1.0/network-acls"),
            "{method} {path}"
        );
        assert!(
            !(method == "PATCH" && path.starts_with("/1.0/projects")),
            "{path}"
        );
    }
}

#[test]
fn a_release_keeps_the_pin_the_snapshot_the_mac_and_the_ip_and_each_step_runs_once() {
    let (_dir, fake) = start("release");
    let writer = writer(&fake);
    for step in AdoptStep::ADOPT {
        writer.run(step).expect("the adoption runs");
    }
    for step in AdoptStep::RELEASE {
        assert_eq!(writer.run(step), Ok(StepOutcome::Sent), "{step:?} runs");
        let sent = fake.writes().len();
        assert_eq!(
            writer.run(step),
            Ok(StepOutcome::AlreadyDone),
            "{step:?} again"
        );
        assert_eq!(fake.writes().len(), sent, "{step:?} again sends nothing");
    }
    let back = instance_in(&fake, "default").expect("the instance is back in default");
    assert_eq!(back["status"], json!("Running"));
    assert!(back["config"].get(MARK).is_none(), "the mark is gone");
    assert_eq!(
        back["devices"]["eth0"],
        pin_patch(writer.pin())["devices"]["eth0"],
        "the pin stays"
    );
    assert_eq!(
        nic_state(&fake, "default"),
        (MAC.to_string(), IP.to_string())
    );
    let snapshot = read(&fake, &format!("/1.0/instances/svc1/snapshots/{SNAPSHOT}"));
    assert!(snapshot.is_ok(), "the snapshot stays: {snapshot:?}");
}

#[test]
fn a_move_without_the_pin_clears_the_mac_the_instance_depends_on() {
    // The control: the fake models deviceVolatileReset, so a move that takes the profile's eth0 away clears its MAC.
    let (_dir, fake) = start("control");
    let writer = writer(&fake);
    for step in [
        AdoptStep::CreateProject,
        AdoptStep::Stop,
        AdoptStep::Snapshot,
    ] {
        writer.run(step).expect("the step runs");
    }
    let sent = fake.writes().len();
    refused(
        writer.run(AdoptStep::Move),
        "the writer never moves an unpinned NIC",
    );
    assert_eq!(fake.writes().len(), sent, "the refused move sent nothing");

    let before = instance_in(&fake, "default").expect("in default");
    assert_eq!(before["config"]["volatile.eth0.hwaddr"], json!(MAC));
    let (status, _) = raw(
        &fake,
        "POST",
        "/1.0/instances/svc1?project=default",
        &json!({ "name": "svc1", "project": TO }),
    );
    assert_eq!(status, 202, "the fake moves the instance");
    let moved = instance_in(&fake, TO).expect("in the adoption project");
    assert!(
        moved["config"].get("volatile.eth0.hwaddr").is_none(),
        "the move cleared the MAC: {}",
        moved["config"]
    );
    assert_ne!(
        moved["config"]["volatile.uuid"],
        json!(UUID),
        "a stopped project move is a copy, under a new uuid"
    );
}

#[test]
fn a_graceful_stop_that_outlasts_its_timeout_is_followed_by_one_forced_stop() {
    let (_dir, fake) = start("hang");
    fake.hang_stop("svc1", false);
    let writer = writer(&fake);
    writer.run(AdoptStep::CreateProject).expect("the project");
    let sent = fake.writes().len();
    assert_eq!(writer.run(AdoptStep::Stop), Ok(StepOutcome::Sent));
    let state = "/1.0/instances/svc1/state?project=default".to_string();
    assert_eq!(
        writes(&fake)[sent..].to_vec(),
        vec![
            ("PUT".to_string(), state.clone()),
            ("PUT".to_string(), state)
        ],
        "one graceful stop, then one forced stop"
    );
    for step in &AdoptStep::ADOPT[2..] {
        writer.run(*step).expect("the adoption continues");
    }
    assert_eq!(nic_state(&fake, TO), (MAC.to_string(), IP.to_string()));
}

#[test]
fn a_forced_stop_that_does_not_stop_fails_the_step_and_changes_nothing_else() {
    let (_dir, fake) = start("stuck");
    fake.hang_stop("svc1", true);
    let writer = writer(&fake);
    writer.run(AdoptStep::CreateProject).expect("the project");
    let sent = fake.writes().len();
    let stop = writer.run(AdoptStep::Stop);
    assert!(
        matches!(stop, Err(IncusError::AppliedUnconfirmed(_))),
        "{stop:?}"
    );
    assert_eq!(
        fake.writes().len(),
        sent + 2,
        "the two stops and nothing else"
    );
    let still = instance_in(&fake, "default").expect("still in default");
    assert_eq!(still["status"], json!("Running"));
    assert_eq!(still["config"]["volatile.eth0.hwaddr"], json!(MAC));
}

#[test]
fn every_step_out_of_its_expected_project_class_or_status_is_refused_with_no_write() {
    let (_dir, fake) = start("before");
    let writer = writer(&fake);
    // The instance runs in default, unmarked, and no adoption project exists.
    for step in [
        AdoptStep::Snapshot,
        AdoptStep::PinNic,
        AdoptStep::Move,
        AdoptStep::Mark,
        AdoptStep::Start,
        AdoptStep::ReleaseStop,
        AdoptStep::Unmark,
    ] {
        refused(
            writer.run(step),
            &format!("{step:?} on a running instance in default"),
        );
    }
    // The release's move and start already hold: the instance runs in default, unmarked.
    assert_eq!(
        writer.run(AdoptStep::MoveBack),
        Ok(StepOutcome::AlreadyDone)
    );
    assert_eq!(
        writer.run(AdoptStep::ReleaseStart),
        Ok(StepOutcome::AlreadyDone)
    );
    assert!(fake.writes().is_empty(), "{:?}", fake.writes());
}

#[test]
fn a_droidd_marked_a_kernel_marked_and_a_different_instance_are_refused_with_no_write() {
    let cases = [
        ("droidd", svc1(json!({ "user.droid.lease": "1" }))),
        ("kernel", svc1(json!({ MARK: "toy" }))),
        (
            "tombstone",
            svc1(json!({ "user.arcakernel.retiring": "x" })),
        ),
        (
            "uuid",
            svc1(json!({ "volatile.uuid": "00000000-0000-4000-8000-0000000000ff" })),
        ),
        ("vm", {
            let mut vm = svc1(json!({}));
            vm["type"] = json!("virtual-machine");
            vm
        }),
    ];
    for (tag, instance) in cases {
        let (_dir, fake) = start_with(tag, instance);
        let writer = writer(&fake);
        for step in [AdoptStep::CreateProject, AdoptStep::Stop] {
            refused(writer.run(step), &format!("{tag}: {step:?}"));
        }
        assert!(fake.writes().is_empty(), "{tag}: {:?}", fake.writes());
    }
}

#[test]
fn an_adoption_project_that_is_not_this_adoptions_is_refused_with_no_write() {
    let (_dir, fake) = start("foreign-project");
    let (status, _) = raw(
        &fake,
        "POST",
        "/1.0/projects",
        &json!({ "name": TO, "config": {} }),
    );
    assert_eq!(status, 201);
    let writer = writer(&fake);
    refused(
        writer.run(AdoptStep::CreateProject),
        "an unmarked project of that name",
    );
    assert_eq!(fake.writes().len(), 1, "only the test's own create");

    let (_dir, fake) = start("busy-project");
    let body = adoption_project_body(&target(), &["br1".to_string()]);
    assert_eq!(raw(&fake, "POST", "/1.0/projects", &body).0, 201);
    let other = json!({ "name": "other", "type": "container", "config": {} });
    assert_eq!(
        raw(
            &fake,
            "POST",
            &format!("/1.0/instances?project={TO}"),
            &other
        )
        .0,
        202
    );
    let writer = AdoptWriter::new(
        fake.socket().to_path_buf(),
        target(),
        vec!["br1".to_string()],
        pre(),
    )
    .expect("the writer");
    let sent = fake.writes().len();
    refused(
        writer.run(AdoptStep::CreateProject),
        "a marked project that holds another instance",
    );
    assert_eq!(fake.writes().len(), sent);
}

#[test]
fn a_changed_nic_an_unpinned_move_and_a_missing_snapshot_name_are_refused_with_no_write() {
    let (_dir, fake) = start("changed");
    let writer = writer(&fake);
    writer.run(AdoptStep::CreateProject).expect("the project");
    writer.run(AdoptStep::Stop).expect("the stop");
    fake.add_profile(
        "default",
        json!({ "name": "default", "config": {},
            "devices": { "eth0": { "type": "nic", "name": "eth0", "network": "br2" } } }),
    );
    let sent = fake.writes().len();
    refused(
        writer.run(AdoptStep::PinNic),
        "the NIC changed since the plan",
    );
    refused(writer.run(AdoptStep::Move), "the NIC is not pinned");
    let unnamed = AdoptWriter::new(
        fake.socket().to_path_buf(),
        target(),
        vec!["br1".to_string()],
        pre(),
    )
    .expect("the writer");
    refused(unnamed.run(AdoptStep::Snapshot), "no snapshot name");
    assert_eq!(fake.writes().len(), sent);
}

#[test]
fn a_running_operation_on_the_instance_holds_every_step_with_no_write() {
    let (_dir, fake) = start("held");
    let held = fake.hold_operation(&ObjectRef {
        kind: IncusKind::Instance,
        project: "default".to_string(),
        parent: None,
        name: "svc1".to_string(),
        uuid: None,
    });
    let writer = writer(&fake);
    for step in AdoptStep::ADOPT {
        refused(
            writer.run(step),
            &format!("{step:?} while an operation runs"),
        );
    }
    assert!(fake.writes().is_empty());
    fake.release_operation(&held);
    assert_eq!(writer.run(AdoptStep::CreateProject), Ok(StepOutcome::Sent));
}

#[test]
fn a_writer_is_refused_for_a_target_or_pre_image_it_cannot_pin() {
    let socket = PathBuf::from("/nonexistent/incus.sock");
    let attach = || vec!["br1".to_string()];
    let bad_targets = [
        ("Svc1", TO),
        ("svc1", "arcak-scratch-x"),
        ("svc1", "arcak-adopt-"),
        ("-x", TO),
    ];
    for (instance, to) in bad_targets {
        let target = AdoptTarget {
            instance: instance.to_string(),
            to: to.to_string(),
        };
        assert!(
            AdoptWriter::new(socket.clone(), target, attach(), pre()).is_err(),
            "{instance} {to}"
        );
    }
    assert!(AdoptWriter::new(socket.clone(), target(), vec!["br2".to_string()], pre()).is_err());
    let mut no_nic = pre();
    no_nic.devices.remove("eth0");
    assert!(AdoptWriter::new(socket.clone(), target(), attach(), no_nic).is_err());
    let mut upper = pre();
    upper.mac = MAC.to_uppercase();
    assert!(AdoptWriter::new(socket.clone(), target(), attach(), upper).is_err());
    let writer = AdoptWriter::new(socket, target(), attach(), pre()).expect("the writer");
    assert!(writer.clone().with_snapshot("arca-adopt-XYZ").is_err());
    assert!(writer.with_snapshot("other-0123456789ab").is_err());
}

#[test]
fn the_adoption_project_grants_only_the_attach_networks_and_allows_snapshots_and_backups() {
    let body = adoption_project_body(&target(), &["br1".to_string(), "br3".to_string()]);
    let config = &body["config"];
    assert_eq!(body["name"], json!(TO));
    assert_eq!(config[MARK], json!("adopt-svc1"));
    assert_eq!(config["restricted"], json!("true"));
    assert_eq!(config["restricted.networks.access"], json!("br1,br3"));
    assert_eq!(config["restricted.snapshots"], json!("allow"));
    assert_eq!(config["restricted.backups"], json!("allow"));
    assert_eq!(config["features.networks"], json!("false"));
    for feature in [
        "features.profiles",
        "features.storage.volumes",
        "features.images",
    ] {
        assert_eq!(config[feature], json!("true"), "{feature}");
    }

    // The fake enforces the restrictions: the adoption project takes a snapshot and a backup, and a restricted project that
    // does not allow them refuses both.
    let (_dir, fake) = start("grant");
    let writer = writer(&fake);
    for step in &AdoptStep::ADOPT[..6] {
        writer.run(*step).expect("the step");
    }
    let snap = json!({ "name": "s2", "stateful": false });
    assert_eq!(
        raw(
            &fake,
            "POST",
            &format!("/1.0/instances/svc1/snapshots?project={TO}"),
            &snap
        )
        .0,
        202
    );
    assert_eq!(
        raw(
            &fake,
            "POST",
            &format!("/1.0/instances/svc1/backups?project={TO}"),
            &json!({})
        )
        .0,
        202
    );
    let closed = json!({ "name": "closed", "config": { "restricted": "true", "restricted.networks.access": "br1" } });
    assert_eq!(raw(&fake, "POST", "/1.0/projects", &closed).0, 201);
    let other = json!({ "name": "c2", "type": "container", "config": {} });
    assert_eq!(
        raw(&fake, "POST", "/1.0/instances?project=closed", &other).0,
        202
    );
    assert_eq!(
        raw(
            &fake,
            "POST",
            "/1.0/instances/c2/snapshots?project=closed",
            &snap
        )
        .0,
        403
    );
    assert_eq!(
        raw(
            &fake,
            "POST",
            "/1.0/instances/c2/backups?project=closed",
            &json!({})
        )
        .0,
        403
    );
}

#[test]
fn a_restricted_project_refuses_a_moved_in_nic_its_grant_does_not_list() {
    let (_dir, fake) = start("ungranted");
    let narrow = json!({ "name": TO, "config": { MARK: "adopt-svc1", "restricted": "true",
        "restricted.networks.access": "br9", "features.profiles": "true" } });
    assert_eq!(raw(&fake, "POST", "/1.0/projects", &narrow).0, 201);
    let writer = writer(&fake);
    for step in &AdoptStep::ADOPT[1..4] {
        writer.run(*step).expect("the step");
    }
    let moved = writer.run(AdoptStep::Move);
    assert!(
        matches!(moved, Err(IncusError::Status { code: 403, .. })),
        "{moved:?}"
    );
    assert!(
        instance_in(&fake, "default").is_some(),
        "the instance stayed in default"
    );
}

/// The step's refusal names `needle`.
fn refused_naming(result: Result<StepOutcome, IncusError>, needle: &str) {
    assert!(
        matches!(&result, Err(IncusError::Refused(why)) if why.contains(needle)),
        "{needle}: {result:?}"
    );
}

/// Runs `steps` on a writer of the recorded pre-image, as a run that then stops would.
fn ran(fake: &FakeIncus, steps: &[AdoptStep]) {
    let writer = writer(fake);
    for step in steps {
        let _ = writer.run(*step);
    }
}

const TO_MOVE: [AdoptStep; 5] = [
    AdoptStep::CreateProject,
    AdoptStep::Stop,
    AdoptStep::Snapshot,
    AdoptStep::PinNic,
    AdoptStep::Move,
];

#[test]
fn a_move_that_copies_the_instance_under_a_new_uuid_is_confirmed_and_the_new_uuid_is_followed() {
    let (_dir, fake) = start("copy");
    let writer = writer(&fake);
    for step in &TO_MOVE[..4] {
        writer.run(*step).expect("the step runs");
    }
    assert_eq!(writer.uuid(), UUID);
    assert_eq!(writer.run(AdoptStep::Move), Ok(StepOutcome::Sent));
    let copy = instance_in(&fake, TO).expect("the copy is in its adoption project");
    let new = copy["config"]["volatile.uuid"]
        .as_str()
        .expect("a uuid")
        .to_string();
    assert_ne!(new, UUID, "the fake copies under a new uuid, as Incus does");
    assert_eq!(writer.uuid(), new, "the writer follows the copy");

    // A writer that resumes without the new uuid, from a record whose Move has no outcome, finds the move done by the same
    // rule and sends nothing; one given the new uuid goes on.
    let sent = fake.writes().len();
    let resumed = self::writer(&fake);
    assert_eq!(resumed.run(AdoptStep::Move), Ok(StepOutcome::AlreadyDone));
    assert_eq!(resumed.uuid(), new);
    assert_eq!(fake.writes().len(), sent, "the move was not sent again");
    let later = self::writer(&fake).with_uuid(&new).expect("a plain uuid");
    assert_eq!(later.run(AdoptStep::Mark), Ok(StepOutcome::Sent));
    assert_eq!(later.run(AdoptStep::Start), Ok(StepOutcome::Sent));

    // The release's MoveBack is a copy too, which the writer follows the same way.
    for step in AdoptStep::RELEASE {
        assert_eq!(later.run(step), Ok(StepOutcome::Sent), "{step:?}");
    }
    let back = instance_in(&fake, "default").expect("back in default");
    let newer = back["config"]["volatile.uuid"].as_str().expect("a uuid");
    assert_ne!(newer, new);
    assert_eq!(later.uuid(), newer);
    assert_eq!(back["status"], json!("Running"));
    assert!(self::writer(&fake).with_uuid("not a uuid!").is_err());
}

#[test]
fn a_copy_under_a_new_uuid_is_refused_when_the_snapshot_is_missing_or_the_pin_differs() {
    // A Move sent without the snapshot lands, but its copy is not confirmed, and a resume refuses it with no write.
    let (_dir, fake) = start("copy-no-snapshot");
    let writer = writer(&fake);
    for step in [AdoptStep::CreateProject, AdoptStep::Stop, AdoptStep::PinNic] {
        writer.run(step).expect("the step runs");
    }
    assert!(
        matches!(
            writer.run(AdoptStep::Move),
            Err(IncusError::AppliedUnconfirmed(_))
        ),
        "the move landed, unconfirmed"
    );
    let before = fake.writes().len();
    refused_naming(self::writer(&fake).run(AdoptStep::Move), "snapshot");
    refused_naming(self::writer(&fake).run(AdoptStep::MoveBack), "snapshot");
    assert_eq!(fake.writes().len(), before);

    // A copy whose eth0 is not the pin.
    let (_dir, fake) = start("copy-unpinned");
    ran(&fake, &TO_MOVE);
    let (status, _) = raw(
        &fake,
        "PATCH",
        &format!("/1.0/instances/svc1?project={TO}"),
        &json!({ "devices": { "eth0": { "type": "nic", "name": "eth0", "network": "br1" } } }),
    );
    assert_eq!(status, 200);
    let before = fake.writes().len();
    refused_naming(self::writer(&fake).run(AdoptStep::Move), "pin");
    refused_naming(self::writer(&fake).run(AdoptStep::Mark), "recorded");
    assert_eq!(fake.writes().len(), before);
}

#[test]
fn a_copy_under_a_new_uuid_is_refused_while_the_old_uuid_is_in_default_or_two_of_the_name_are_in_to()
 {
    // An instance in default that still carries the old uuid.
    let (_dir, fake) = start("copy-old-uuid");
    ran(&fake, &TO_MOVE);
    let other = json!({ "name": "svc9", "type": "container", "config": {} });
    let (status, _) = raw(&fake, "POST", "/1.0/instances?project=default", &other);
    assert_eq!(status, 202);
    let old = json!({ "config": { "volatile.uuid": UUID } });
    let (status, _) = raw(&fake, "PATCH", "/1.0/instances/svc9?project=default", &old);
    assert_eq!(status, 200);
    let before = fake.writes().len();
    refused_naming(self::writer(&fake).run(AdoptStep::Move), "default");
    assert_eq!(fake.writes().len(), before);

    // Two instances of the name in the adoption project.
    let dir = Scratch::new("copy-two");
    let pinned = json!({ "type": "nic", "name": "eth0", "network": "br1", "hwaddr": MAC, "ipv4.address": IP });
    let copy = |uuid: &str| {
        json!({
            "name": "svc1", "project": TO, "type": "container", "status": "Stopped",
            "created_at": "2026-01-01T00:00:00Z", "profiles": ["default"],
            "config": { "volatile.uuid": uuid, "image.os": "debian" },
            "devices": { "eth0": pinned, "root": { "type": "disk", "path": "/", "pool": "pool1", "size": "60GiB" } },
        })
    };
    let mut two = state(copy("00000000-0000-4000-8000-0000000000b1"));
    two.instances
        .push(copy("00000000-0000-4000-8000-0000000000b2"));
    let mut project = adoption_project_body(&target(), &["br1".to_string()]);
    project["description"] = json!("");
    two.projects.push(project);
    let fake = FakeIncus::start(&dir.0, two).expect("start the fake");
    let (status, _) = raw(
        &fake,
        "POST",
        &format!("/1.0/instances/svc1/snapshots?project={TO}"),
        &json!({ "name": SNAPSHOT, "stateful": false }),
    );
    assert_eq!(status, 202);
    let listed = read(&fake, "/1.0/instances?all-projects=true&recursion=1").expect("the listing");
    let named = listed
        .as_array()
        .expect("a list")
        .iter()
        .filter(|row| row["project"] == TO && row["name"] == "svc1")
        .count();
    assert_eq!(named, 2, "the fixture holds two of the name");
    let before = fake.writes().len();
    refused_naming(writer(&fake).run(AdoptStep::Move), "more than one");
    assert_eq!(fake.writes().len(), before);
}
