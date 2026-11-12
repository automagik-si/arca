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
    AdoptStep, AdoptTarget, AdoptWriter, Allowances, Devices, ExpectClass, FakeIncus, FakeState,
    IncusError, IncusRead, PreImage, Side, StepOutcome, UnixIncus, adoption_project_body,
    needed_allowances, pin_patch,
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
        stopped: false,
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
        (
            S::LocalizeRoot,
            Side::Default,
            ExpectClass::Foreign,
            "Stopped",
        ),
        (S::Move, Side::Default, ExpectClass::Foreign, "Stopped"),
        (S::Mark, Side::To, ExpectClass::Foreign, "Stopped"),
        (S::Start, Side::To, ExpectClass::Adopted, "Stopped"),
        (S::ReleaseStop, Side::To, ExpectClass::Adopted, "Running"),
        (S::Unmark, Side::To, ExpectClass::Adopted, "Stopped"),
        (S::MoveBack, Side::To, ExpectClass::Foreign, "Stopped"),
        (
            S::RestoreRoot,
            Side::Default,
            ExpectClass::Foreign,
            "Stopped",
        ),
        (
            S::ReleaseStart,
            Side::Default,
            ExpectClass::Foreign,
            "Stopped",
        ),
        (
            S::DeleteProject,
            Side::Default,
            ExpectClass::Foreign,
            "Running",
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
    assert_eq!(
        writer.adopt_steps(),
        [
            AdoptStep::CreateProject,
            AdoptStep::Stop,
            AdoptStep::Snapshot,
            AdoptStep::PinNic,
            AdoptStep::Move,
            AdoptStep::Mark,
            AdoptStep::Start
        ],
        "a plan that localizes nothing has no LocalizeRoot"
    );
    for step in writer.adopt_steps() {
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
    for step in writer.adopt_steps() {
        writer.run(step).expect("the adoption runs");
    }
    assert_eq!(
        writer.release_steps(),
        [
            AdoptStep::ReleaseStop,
            AdoptStep::Unmark,
            AdoptStep::MoveBack,
            AdoptStep::ReleaseStart,
            AdoptStep::DeleteProject
        ],
        "a plan that localizes nothing has no RestoreRoot"
    );
    for step in writer.release_steps() {
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
    assert!(
        matches!(
            read(&fake, &format!("/1.0/projects/{TO}")),
            Err(IncusError::Status { code: 404, .. })
        ),
        "the emptied adoption project is deleted"
    );
    let deletes: Vec<(String, String)> = writes(&fake)
        .into_iter()
        .filter(|(method, _)| method == "DELETE")
        .collect();
    assert_eq!(
        deletes,
        [("DELETE".to_string(), format!("/1.0/projects/{TO}"))],
        "the one DELETE is the adoption project's"
    );
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
    for step in &writer.adopt_steps()[2..] {
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
    let body = adoption_project_body(&target(), "br1", &BTreeMap::new());
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
    for step in AdoptStep::ADOPT.into_iter().chain(AdoptStep::RELEASE) {
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
fn the_adoption_project_grants_only_eth0s_network_and_allows_snapshots_and_backups() {
    // The base config, key for key: no allowance, and the grant names eth0's network alone.
    let body = adoption_project_body(&target(), "br1", &BTreeMap::new());
    assert_eq!(body["name"], json!(TO));
    assert_eq!(body["config"], base_config(&[]), "the config, key for key");

    // Two attach networks: the writer's create still grants eth0's alone.
    let (_dir, fake) = start("grant");
    let writer = AdoptWriter::new(
        fake.socket().to_path_buf(),
        target(),
        vec!["br3".to_string(), "br1".to_string()],
        pre(),
    )
    .and_then(|writer| writer.with_snapshot(SNAPSHOT))
    .expect("the writer");
    assert_eq!(
        writer.requests(AdoptStep::CreateProject)[0].body["config"],
        base_config(&[])
    );

    // The fake enforces the restrictions: the adoption project takes a snapshot and a backup, and a restricted project that
    // does not allow them refuses both.
    for step in &writer.adopt_steps()[..6] {
        writer.run(*step).expect("the step");
    }
    assert_eq!(
        read(&fake, &format!("/1.0/projects/{TO}")).expect("the project")["config"],
        base_config(&[]),
        "the created project's config is the body's, key for key"
    );
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
        500
    );
    assert_eq!(
        raw(
            &fake,
            "POST",
            "/1.0/instances/c2/backups?project=closed",
            &json!({})
        )
        .0,
        500
    );
}

#[test]
fn a_restricted_project_refuses_a_moved_in_nic_its_grant_does_not_list() {
    let (_dir, fake) = start("ungranted");
    let narrow = json!({ "name": TO, "config": { MARK: "adopt-svc1", "restricted": "true",
        "restricted.networks.access": "br9", "features.profiles": "true" } });
    assert_eq!(raw(&fake, "POST", "/1.0/projects", &narrow).0, 201);
    let writer = writer(&fake);
    for step in &writer.adopt_steps()[1..4] {
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
    for step in later.release_steps() {
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
    let mut project = adoption_project_body(&target(), "br1", &BTreeMap::new());
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

#[test]
fn a_copy_under_a_new_uuid_is_refused_while_the_old_uuid_is_in_any_project() {
    // The old uuid in the adoption project itself, under another name: the project the instance left holds neither.
    let (_dir, fake) = start("copy-old-uuid-to");
    ran(&fake, &TO_MOVE);
    let other = json!({ "name": "svc9", "type": "container", "config": {} });
    let (status, _) = raw(
        &fake,
        "POST",
        &format!("/1.0/instances?project={TO}"),
        &other,
    );
    assert_eq!(status, 202);
    let old = json!({ "config": { "volatile.uuid": UUID } });
    let (status, _) = raw(
        &fake,
        "PATCH",
        &format!("/1.0/instances/svc9?project={TO}"),
        &old,
    );
    assert_eq!(status, 200);
    let before = fake.writes().len();
    refused_naming(self::writer(&fake).run(AdoptStep::Move), "old uuid");
    refused_naming(self::writer(&fake).run(AdoptStep::MoveBack), "old uuid");
    assert_eq!(fake.writes().len(), before);
}

#[test]
fn a_copy_whose_new_uuid_is_not_plain_is_refused_and_not_followed() {
    for (tag, uuid) in [("bad", "NOT-A-UUID"), ("empty", "")] {
        let (_dir, fake) = start(&format!("copy-uuid-{tag}"));
        ran(&fake, &TO_MOVE);
        let bent = json!({ "config": { "volatile.uuid": uuid } });
        let (status, _) = raw(
            &fake,
            "PATCH",
            &format!("/1.0/instances/svc1?project={TO}"),
            &bent,
        );
        assert_eq!(status, 200);
        let before = fake.writes().len();
        let resumed = self::writer(&fake);
        refused_naming(resumed.run(AdoptStep::Move), "plain uuid");
        assert_eq!(
            resumed.uuid(),
            UUID,
            "{tag}: the writer still expects the old uuid"
        );
        assert_eq!(fake.writes().len(), before);
    }
}

#[test]
fn a_copy_under_a_new_uuid_is_refused_while_the_adoption_project_is_not_this_adoptions() {
    let (_dir, fake) = start("copy-project-unmarked");
    ran(&fake, &TO_MOVE);
    let mut project = adoption_project_body(&target(), "br1", &BTreeMap::new());
    project["config"]
        .as_object_mut()
        .expect("a config")
        .remove(MARK);
    let (status, _) = raw(&fake, "PUT", &format!("/1.0/projects/{TO}"), &project);
    assert_eq!(status, 200, "the project's mark is gone");
    let before = fake.writes().len();
    let resumed = self::writer(&fake);
    refused_naming(resumed.run(AdoptStep::Move), "adoption project");
    assert_eq!(resumed.uuid(), UUID);
    assert_eq!(fake.writes().len(), before);
}

// Allowances and the localized root.

/// The adoption project's config with nothing allowed past eth0's network, snapshots and backups, then `extra`.
fn base_config(extra: &[(&str, &str)]) -> Value {
    let mut config = json!({
        MARK: "adopt-svc1", "restricted": "true", "restricted.networks.access": "br1",
        "restricted.snapshots": "allow", "restricted.backups": "allow",
        "features.networks": "false", "features.profiles": "true",
        "features.storage.volumes": "true", "features.images": "true",
    });
    for (key, value) in extra {
        config[*key] = json!(value);
    }
    config
}

fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    device(pairs)
}

fn root() -> Value {
    json!({ "type": "disk", "path": "/", "pool": "pool1", "size": "60GiB" })
}

/// The first fixture's `eth0`, the instance's own: on the default bridge, with its IPv4 set and two ACL keys.
fn acl_nic() -> Value {
    json!({ "type": "nic", "name": "eth0", "network": "br1", "ipv4.address": IP,
        "security.acls": "web", "security.acls.default.egress.action": "allow",
        "security.acls.default.ingress.action": "drop" })
}

/// The default profile, giving `eth0` and, here, the root.
fn rooted_profile() -> Value {
    json!({ "name": "default", "config": {},
        "devices": { "eth0": { "type": "nic", "name": "eth0", "network": "br1" }, "root": root() } })
}

/// The first fixture: `svc1` runs in default with its own `eth0` ([`acl_nic`]) and the root its default profile gives. The
/// ACL `web` is in default. The fake's writes start after this setup's own ACL create.
fn start_acl(tag: &str) -> (Scratch, FakeIncus) {
    let mut instance = svc1(json!({}));
    instance["devices"] = json!({ "eth0": acl_nic() });
    let (dir, fake) = start_with(tag, instance);
    fake.add_profile("default", rooted_profile());
    let acl = json!({ "name": "web", "config": {} });
    assert_eq!(raw(&fake, "POST", "/1.0/network-acls", &acl).0, 201);
    (dir, fake)
}

fn acl_pre() -> PreImage {
    let mut pre = pre();
    let eth0: BTreeMap<String, String> =
        serde_json::from_value(acl_nic()).expect("a device of text");
    pre.devices.insert("eth0".to_string(), eth0);
    pre
}

fn root_localized() -> Allowances {
    Allowances {
        project: BTreeMap::new(),
        localize: Devices::from([(
            "root".to_string(),
            serde_json::from_value(root()).expect("a device of text"),
        )]),
    }
}

fn allowing(fake: &FakeIncus, pre: PreImage, allowances: Allowances) -> AdoptWriter {
    AdoptWriter::new_allowing(
        fake.socket().to_path_buf(),
        target(),
        vec!["br1".to_string()],
        pre,
        allowances,
    )
    .and_then(|writer| writer.with_snapshot(SNAPSHOT))
    .expect("the writer")
}

/// An instance's own config without its `volatile.*` keys and the mark.
fn own_config(body: &Value) -> BTreeMap<String, Value> {
    body["config"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(key, _)| !key.starts_with("volatile.") && key.as_str() != MARK)
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

#[test]
fn an_instance_whose_nic_sets_its_ip_and_acls_and_whose_root_a_profile_gives_is_adopted_and_released_with_no_allowance()
 {
    // Its plan allows nothing past the base: the NIC's ACLs are read from default, which no restricted key governs, and
    // the pin carries its IPv4 and ACL keys. Its root is localized.
    let pre = acl_pre();
    let needed = needed_allowances(&pre.config, &pre.devices).expect("every key maps");
    assert!(needed.project.is_empty(), "{needed:?}");
    let (_dir, fake) = start_acl("acl");
    let writer = allowing(&fake, pre, root_localized());
    assert_eq!(writer.adopt_steps(), AdoptStep::ADOPT.to_vec());
    assert_eq!(writer.release_steps(), AdoptStep::RELEASE.to_vec());
    let before = instance_in(&fake, "default").expect("in default");
    assert!(
        before["devices"].get("root").is_none(),
        "the root is the profile's"
    );
    for step in writer.adopt_steps() {
        assert_eq!(writer.run(step), Ok(StepOutcome::Sent), "{step:?}");
        assert_eq!(
            writer.run(step),
            Ok(StepOutcome::AlreadyDone),
            "{step:?} again"
        );
    }
    let project = read(&fake, &format!("/1.0/projects/{TO}")).expect("the project");
    assert_eq!(
        project["config"],
        base_config(&[]),
        "the project sets the base and no allow key"
    );
    let after = instance_in(&fake, TO).expect("adopted");
    assert_eq!(after["status"], json!("Running"));
    let mut pinned = acl_nic();
    pinned["hwaddr"] = json!(MAC);
    assert_eq!(
        after["devices"]["eth0"], pinned,
        "the pin keeps the NIC's IPv4 and ACL keys and adds its MAC"
    );
    assert_eq!(
        after["devices"]["root"],
        root(),
        "the root is the instance's own"
    );
    assert_eq!(nic_state(&fake, TO), (MAC.to_string(), IP.to_string()));
    assert_eq!(own_config(&after), own_config(&before));

    // The release takes the localized root back off, in default, by one PUT of the instance as read; then the project goes.
    for step in &writer.release_steps()[..3] {
        assert_eq!(writer.run(*step), Ok(StepOutcome::Sent), "{step:?}");
    }
    let moved_back = instance_in(&fake, "default").expect("moved back");
    let sent = fake.writes().len();
    assert_eq!(writer.run(AdoptStep::RestoreRoot), Ok(StepOutcome::Sent));
    assert_eq!(
        writes(&fake)[sent..].to_vec(),
        [(
            "PUT".to_string(),
            "/1.0/instances/svc1?project=default".to_string()
        )]
    );
    let restored = instance_in(&fake, "default").expect("in default");
    assert!(
        restored["devices"].get("root").is_none(),
        "the root is the profile's again"
    );
    assert_eq!(restored["expanded_devices"]["root"], root());
    for field in ["config", "profiles"] {
        assert_eq!(
            restored[field], moved_back[field],
            "the PUT kept {field} as read"
        );
    }
    for step in &writer.release_steps()[4..] {
        assert_eq!(writer.run(*step), Ok(StepOutcome::Sent), "{step:?}");
    }
    let back = instance_in(&fake, "default").expect("released");
    assert_eq!(back["status"], json!("Running"));
    assert_eq!(back["devices"]["eth0"], pinned, "the pin stays");
    assert_eq!(own_config(&back), own_config(&before));
    assert_eq!(
        nic_state(&fake, "default"),
        (MAC.to_string(), IP.to_string())
    );
    assert!(matches!(
        read(&fake, &format!("/1.0/projects/{TO}")),
        Err(IncusError::Status { code: 404, .. })
    ));
    assert_eq!(
        writer.run(AdoptStep::DeleteProject),
        Ok(StepOutcome::AlreadyDone)
    );
}

#[test]
fn a_nic_acl_needs_no_allowance_but_must_be_in_default_where_the_adoption_project_reads_it() {
    // The control: without the ACL in default, the fake refuses the move as Incus refuses the NIC.
    let mut instance = svc1(json!({}));
    instance["devices"] = json!({ "eth0": acl_nic() });
    let (_dir, fake) = start_with("acl-missing", instance);
    fake.add_profile("default", rooted_profile());
    let writer = allowing(&fake, acl_pre(), root_localized());
    for step in &writer.adopt_steps()[..5] {
        writer.run(*step).expect("the step");
    }
    let moved = writer.run(AdoptStep::Move);
    assert!(
        matches!(&moved, Err(IncusError::Status { code: 403, message }) if message.contains("web")),
        "{moved:?}"
    );
    assert!(instance_in(&fake, "default").is_some());
}

fn data_disk() -> Value {
    json!({ "type": "disk", "source": "/srv/data", "path": "/data" })
}

/// The second fixture's allowances: a host-path disk and nesting.
fn nested_allowances() -> BTreeMap<String, String> {
    map(&[
        ("restricted.containers.nesting", "allow"),
        ("restricted.devices.disk", "allow"),
        ("restricted.devices.disk.paths", "/srv/data"),
    ])
}

fn nested_pre() -> PreImage {
    let mut pre = pre();
    pre.config
        .insert("security.nesting".to_string(), "true".to_string());
    pre.devices.insert(
        "data".to_string(),
        serde_json::from_value(data_disk()).expect("a device of text"),
    );
    pre
}

#[test]
fn a_host_path_disk_and_nesting_get_exactly_their_allowances_and_the_adoption_runs() {
    let pre = nested_pre();
    let needed = needed_allowances(&pre.config, &pre.devices).expect("every key maps");
    assert_eq!(needed.project, nested_allowances());
    assert_eq!(
        needed.why["restricted.containers.nesting"],
        ["security.nesting"]
    );
    assert_eq!(needed.why["restricted.devices.disk"], ["devices.data"]);
    assert_eq!(
        needed.why["restricted.devices.disk.paths"],
        ["devices.data"]
    );

    let mut instance = svc1(json!({ "security.nesting": "true" }));
    instance["devices"]["data"] = data_disk();
    let (_dir, fake) = start_with("nested", instance);
    let writer = allowing(
        &fake,
        pre,
        Allowances {
            project: nested_allowances(),
            localize: Devices::new(),
        },
    );
    for step in writer.adopt_steps() {
        assert_eq!(writer.run(step), Ok(StepOutcome::Sent), "{step:?}");
    }
    let project = read(&fake, &format!("/1.0/projects/{TO}")).expect("the project");
    assert_eq!(
        project["config"],
        base_config(&[
            ("restricted.containers.nesting", "allow"),
            ("restricted.devices.disk", "allow"),
            ("restricted.devices.disk.paths", "/srv/data"),
        ]),
        "the base and exactly the three allowances"
    );
    let after = instance_in(&fake, TO).expect("adopted");
    assert_eq!(after["status"], json!("Running"));
    assert_eq!(after["devices"]["data"], data_disk());
    assert_eq!(after["config"]["security.nesting"], json!("true"));
    for step in writer.release_steps() {
        assert_eq!(writer.run(step), Ok(StepOutcome::Sent), "{step:?}");
    }
    assert_eq!(
        instance_in(&fake, "default").expect("released")["status"],
        json!("Running")
    );
}

/// A stopped `svc1` in default with its own root and `eth0`, `config` and `devices` added.
fn movable(config: &Value, devices: &Value) -> Value {
    let mut instance = svc1(config.clone());
    instance["status"] = json!("Stopped");
    instance["devices"]["eth0"] = json!({ "type": "nic", "name": "eth0", "network": "br1" });
    if let (Some(own), Some(extra)) = (instance["devices"].as_object_mut(), devices.as_object()) {
        own.extend(extra.clone());
    }
    instance
}

/// Whether the fake moves `instance` into an adoption project created with `project` as its allowances.
fn moves_with(tag: &str, instance: &Value, project: &BTreeMap<String, String>) -> bool {
    let (_dir, fake) = start_with(tag, instance.clone());
    let acl = json!({ "name": "web", "config": {} });
    assert_eq!(raw(&fake, "POST", "/1.0/network-acls", &acl).0, 201);
    let body = adoption_project_body(&target(), "br1", project);
    assert_eq!(raw(&fake, "POST", "/1.0/projects", &body).0, 201);
    let to = json!({ "name": "svc1", "project": TO });
    let (status, _) = raw(&fake, "POST", "/1.0/instances/svc1?project=default", &to);
    assert!(status == 202 || status == 403, "{tag}: {status}");
    status == 202
}

/// A mapping case: its tag, the instance's config and extra devices, and the allowances they need.
type Case = (String, Value, Value, Vec<(String, String)>);

#[test]
fn each_allowance_the_table_derives_is_one_the_fakes_incus_model_needs_and_none_is_extra() {
    let low = [("restricted.containers.lowlevel", "allow")];
    let device_case = |kind: &str| -> (Value, Vec<(String, String)>) {
        (
            json!({ "dev0": { "type": kind } }),
            vec![(format!("restricted.devices.{kind}"), "allow".to_string())],
        )
    };
    let mut cases: Vec<Case> = vec![];
    let mut config_case = |tag: &str, config: Value, expected: &[(&str, &str)]| {
        cases.push((
            tag.to_string(),
            config,
            json!({}),
            expected
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        ));
    };
    config_case(
        "nesting",
        json!({ "security.nesting": "true" }),
        &[("restricted.containers.nesting", "allow")],
    );
    config_case("nesting-off", json!({ "security.nesting": "false" }), &[]);
    config_case(
        "privileged-off",
        json!({ "security.privileged": "false" }),
        &[],
    );
    config_case(
        "intercept",
        json!({ "security.syscalls.intercept.mknod": "true" }),
        &[("restricted.containers.interception", "allow")],
    );
    config_case(
        "intercept-off",
        json!({ "security.syscalls.intercept.mknod": "false" }),
        &[],
    );
    config_case(
        "intercept-low",
        json!({ "security.syscalls.intercept.mount.allowed": "ext4" }),
        &low,
    );
    config_case(
        "raw-lxc",
        json!({ "raw.lxc": "lxc.apparmor.profile=unconfined" }),
        &low,
    );
    config_case("raw-apparmor", json!({ "raw.apparmor": "x" }), &low);
    config_case("raw-seccomp", json!({ "raw.seccomp": "x" }), &low);
    config_case(
        "modules",
        json!({ "linux.kernel_modules": "overlay" }),
        &low,
    );
    config_case("idmap", json!({ "security.idmap.base": "100000" }), &low);
    config_case("swap", json!({ "limits.memory.swap": "false" }), &low);
    for kind in [
        "unix-char",
        "unix-block",
        "unix-hotplug",
        "gpu",
        "usb",
        "pci",
        "proxy",
        "infiniband",
    ] {
        let (devices, expected) = device_case(kind);
        cases.push((kind.to_string(), json!({}), devices, expected));
    }
    cases.push((
        "host-disk".to_string(),
        json!({}),
        json!({ "data": data_disk() }),
        nested_allowances()
            .into_iter()
            .filter(|(key, _)| key.starts_with("restricted.devices"))
            .collect(),
    ));
    cases.push((
        "acl".to_string(),
        json!({}),
        json!({ "eth0": { "type": "nic", "name": "eth0", "network": "br1", "security.acls": "web" } }),
        vec![],
    ));
    cases.push((
        "nested".to_string(),
        json!({ "security.nesting": "true" }),
        json!({ "data": data_disk() }),
        nested_allowances().into_iter().collect(),
    ));

    for (tag, config, devices, expected) in cases {
        let expected: BTreeMap<String, String> = expected.into_iter().collect();
        let instance = movable(&config, &devices);
        let own: BTreeMap<String, String> =
            serde_json::from_value(instance["config"].clone()).expect("a config of text");
        let all: Devices =
            serde_json::from_value(instance["devices"].clone()).expect("devices of text");
        let needed = needed_allowances(&own, &all).unwrap_or_else(|why| panic!("{tag}: {why}"));
        assert_eq!(needed.project, expected, "{tag}: the table's allowances");
        assert!(
            moves_with(&format!("{tag}-all"), &instance, &expected),
            "{tag}: the move lands with exactly these allowances"
        );
        // Each one is needed: without it, or with the disk paths naming another path, the fake refuses the move. A paths
        // key left out would allow every path, so it is narrowed instead.
        for key in expected.keys() {
            let mut fewer = expected.clone();
            if key == "restricted.devices.disk.paths" {
                fewer.insert(key.clone(), "/srv/other".to_string());
            } else {
                fewer.remove(key);
            }
            assert!(
                !moves_with(&format!("{tag}-without"), &instance, &fewer),
                "{tag}: the move is refused without {key}"
            );
        }
    }
}

#[test]
fn what_no_allowance_admits_is_refused_with_its_reason() {
    let eth0 = || map(&[("type", "nic"), ("name", "eth0"), ("network", "br1")]);
    let with_device = |name: &str, pairs: &[(&str, &str)]| -> Devices {
        let mut devices = Devices::from([("eth0".to_string(), eth0())]);
        devices.insert(name.to_string(), map(pairs));
        devices
    };
    let none = BTreeMap::new();
    let cases: Vec<(&str, BTreeMap<String, String>, Devices, &str)> = vec![
        (
            "privileged",
            map(&[("security.privileged", "true")]),
            with_device("root", &[("type", "disk"), ("path", "/"), ("pool", "p")]),
            "security.privileged",
        ),
        (
            "privileged-1",
            map(&[("security.privileged", "1")]),
            Devices::new(),
            "security.privileged",
        ),
        (
            "eth1",
            none.clone(),
            with_device("eth1", &[("type", "nic"), ("network", "br1")]),
            "other than eth0",
        ),
        (
            "eth0-unmanaged",
            none.clone(),
            Devices::from([(
                "eth0".to_string(),
                map(&[("type", "nic"), ("nictype", "bridged"), ("parent", "br0")]),
            )]),
            "no managed network",
        ),
        (
            "volume",
            none.clone(),
            with_device(
                "data",
                &[
                    ("type", "disk"),
                    ("pool", "pool1"),
                    ("source", "vol1"),
                    ("path", "/d"),
                ],
            ),
            "a disk other than its root",
        ),
        (
            "neither",
            none.clone(),
            with_device("data", &[("type", "disk"), ("path", "/d")]),
            "neither a pool nor a host path",
        ),
        (
            "tpm",
            none.clone(),
            with_device("tpm0", &[("type", "tpm"), ("path", "/dev/tpm0")]),
            "of type",
        ),
        // Incus refuses `shift` on a host-path disk once `restricted.devices.disk.paths` is set, which the adoption sets.
        (
            "shifted",
            none.clone(),
            with_device(
                "data",
                &[
                    ("type", "disk"),
                    ("source", "/srv/data"),
                    ("path", "/d"),
                    ("shift", "true"),
                ],
            ),
            "shift",
        ),
        (
            "shifted-on",
            none.clone(),
            with_device(
                "data",
                &[
                    ("type", "disk"),
                    ("source", "/srv/data"),
                    ("path", "/d"),
                    ("shift", "On"),
                ],
            ),
            "shift",
        ),
    ];
    for (tag, config, devices, needle) in cases {
        let why = needed_allowances(&config, &devices).expect_err(tag);
        assert!(why.contains(needle), "{tag}: {why}");
    }
    // A shift that is off is no shift.
    let unshifted = with_device(
        "data",
        &[
            ("type", "disk"),
            ("source", "/srv/data"),
            ("path", "/d"),
            ("shift", "false"),
        ],
    );
    assert_eq!(
        needed_allowances(&none, &unshifted)
            .expect("an unshifted host disk maps")
            .project,
        nested_allowances()
            .into_iter()
            .filter(|(key, _)| key.starts_with("restricted.devices"))
            .collect::<BTreeMap<_, _>>()
    );
    for source in [
        "/srv/a,b",
        "/srv/data/",
        "/srv/../etc",
        "srv/data",
        " /srv",
        "/srv//x",
    ] {
        let devices = with_device(
            "data",
            &[("type", "disk"), ("source", source), ("path", "/d")],
        );
        let why = needed_allowances(&none, &devices).expect_err(source);
        assert!(why.contains("cannot list"), "{source}: {why}");
    }
}

#[test]
fn a_writer_is_refused_for_allowances_that_are_not_exactly_the_pre_images_or_a_nic_the_pin_cannot_carry()
 {
    let socket = PathBuf::from("/nonexistent/incus.sock");
    let with =
        |pre: PreImage, project: BTreeMap<String, String>, localize: Devices, attach: &str| {
            AdoptWriter::new_allowing(
                socket.clone(),
                target(),
                vec![attach.to_string()],
                pre,
                Allowances { project, localize },
            )
        };
    let none = Devices::new;
    assert!(with(nested_pre(), nested_allowances(), none(), "br1").is_ok());
    let mut missing = nested_allowances();
    missing.remove("restricted.containers.nesting");
    let mut extra = nested_allowances();
    extra.insert("restricted.devices.gpu".to_string(), "allow".to_string());
    let mut wider = nested_allowances();
    wider.insert(
        "restricted.devices.disk.paths".to_string(),
        "/srv".to_string(),
    );
    for (tag, project) in [("missing", missing), ("extra", extra), ("wider", wider)] {
        assert!(with(nested_pre(), project, none(), "br1").is_err(), "{tag}");
    }
    let privileged = map(&[("restricted.containers.privilege", "allow")]);
    assert!(with(pre(), privileged, none(), "br1").is_err());
    let mut root_privileged = pre();
    root_privileged
        .config
        .insert("security.privileged".to_string(), "true".to_string());
    assert!(with(root_privileged, BTreeMap::new(), none(), "br1").is_err());

    // The grant: eth0's network off the attach list needs it, and it may name nothing else.
    let grant = |network: &str| map(&[("restricted.networks.access", network)]);
    assert!(with(pre(), BTreeMap::new(), none(), "br3").is_err());
    assert!(with(pre(), grant("br1"), none(), "br3").is_ok());
    assert!(with(pre(), grant("br3"), none(), "br3").is_err());
    let granted = with(pre(), grant("br1"), none(), "br3").expect("the writer");
    assert_eq!(
        granted.requests(AdoptStep::CreateProject)[0].body["config"],
        base_config(&[]),
        "the grant names eth0's network alone"
    );

    // Localize: the pre-image's root as recorded, and nothing else.
    assert!(with(pre(), BTreeMap::new(), root_localized().localize, "br1").is_ok());
    let mut resized = root_localized().localize;
    resized
        .get_mut("root")
        .expect("root")
        .insert("size".to_string(), "10GiB".to_string());
    assert!(with(pre(), BTreeMap::new(), resized, "br1").is_err());
    let nic = Devices::from([("eth0".to_string(), pre().devices["eth0"].clone())]);
    assert!(with(pre(), BTreeMap::new(), nic, "br1").is_err());

    // eth0: a key the pin does not carry, or a hwaddr, an ipv4.address or a name other than the pin's.
    for (key, value) in [
        ("mtu", "1400"),
        ("hwaddr", "10:66:6a:00:00:02"),
        ("ipv4.address", "192.0.2.132"),
        ("name", "enp5s0"),
    ] {
        let mut bent = pre();
        bent.devices
            .get_mut("eth0")
            .expect("eth0")
            .insert(key.to_string(), value.to_string());
        assert!(with(bent, BTreeMap::new(), none(), "br1").is_err(), "{key}");
    }
    let mut agreeing = pre();
    agreeing
        .devices
        .get_mut("eth0")
        .expect("eth0")
        .extend(map(&[
            ("hwaddr", MAC),
            ("ipv4.address", IP),
            ("security.acls", "web"),
        ]));
    let writer = with(agreeing, BTreeMap::new(), none(), "br1").expect("an agreeing NIC");
    assert_eq!(
        pin_patch(writer.pin())["devices"]["eth0"],
        json!({ "type": "nic", "name": "eth0", "network": "br1", "hwaddr": MAC,
            "ipv4.address": IP, "security.acls": "web" })
    );
}

#[test]
fn a_changed_root_is_neither_localized_nor_taken_off_and_the_move_waits_for_the_localized_root() {
    // Move refuses before the root is the instance's own, and LocalizeRoot refuses a root the profile changed since.
    let (_dir, fake) = start_acl("root-changed");
    let writer = allowing(&fake, acl_pre(), root_localized());
    for step in &writer.adopt_steps()[..4] {
        writer.run(*step).expect("the step");
    }
    let sent = fake.writes().len();
    refused_naming(writer.run(AdoptStep::Move), "root is not its own");
    let mut resized = rooted_profile();
    resized["devices"]["root"]["size"] = json!("10GiB");
    fake.add_profile("default", resized.clone());
    refused_naming(
        writer.run(AdoptStep::LocalizeRoot),
        "changed since the plan",
    );
    assert_eq!(fake.writes().len(), sent, "no write");

    // RestoreRoot keeps the instance's own root when the profiles no longer give the recorded one, or when its own changed.
    let (_dir, fake) = start_acl("restore-changed");
    let writer = allowing(&fake, acl_pre(), root_localized());
    for step in writer.adopt_steps() {
        writer.run(step).expect("the adoption");
    }
    for step in &writer.release_steps()[..3] {
        writer.run(*step).expect("the release up to RestoreRoot");
    }
    fake.add_profile("default", resized);
    let sent = fake.writes().len();
    refused_naming(writer.run(AdoptStep::RestoreRoot), "no longer give");
    assert!(
        matches!(writer.restore_ready(), Err(IncusError::Refused(why)) if why.contains("no longer give"))
    );
    assert_eq!(fake.writes().len(), sent, "no write");
    fake.add_profile("default", rooted_profile());
    assert_eq!(writer.restore_ready(), Ok(()));
    let mut bigger = root();
    bigger["size"] = json!("80GiB");
    let (status, _) = raw(
        &fake,
        "PATCH",
        "/1.0/instances/svc1?project=default",
        &json!({ "devices": { "root": bigger } }),
    );
    assert_eq!(status, 200);
    let sent = fake.writes().len();
    refused_naming(writer.run(AdoptStep::RestoreRoot), "so it is kept");
    assert_eq!(fake.writes().len(), sent, "no write");
    assert_eq!(
        instance_in(&fake, "default").expect("in default")["devices"]["root"],
        bigger
    );
}

#[test]
fn the_adoption_project_is_deleted_only_when_empty_and_this_adoptions_and_created_only_as_planned()
{
    // DeleteProject expects the instance back in default, and an empty project.
    let (_dir, fake) = start("delete");
    let adoption = writer(&fake);
    for step in adoption.adopt_steps() {
        adoption.run(step).expect("the adoption");
    }
    refused(
        adoption.run(AdoptStep::DeleteProject),
        "the instance is still in its adoption project",
    );
    for step in &adoption.release_steps()[..4] {
        adoption
            .run(*step)
            .expect("the release up to DeleteProject");
    }
    let other = json!({ "name": "other", "type": "container", "config": {} });
    let (status, _) = raw(
        &fake,
        "POST",
        &format!("/1.0/instances?project={TO}"),
        &other,
    );
    assert_eq!(status, 202);
    let sent = fake.writes().len();
    refused_naming(
        adoption.run(AdoptStep::DeleteProject),
        "still holds an instance",
    );
    assert_eq!(fake.writes().len(), sent, "no write");
    assert!(read(&fake, &format!("/1.0/projects/{TO}")).is_ok());

    // A project of that name that is not this adoption's is never deleted, and none at all is the step done.
    let (_dir, fake) = start("delete-foreign");
    let foreign = writer(&fake);
    assert_eq!(
        foreign.run(AdoptStep::DeleteProject),
        Ok(StepOutcome::AlreadyDone)
    );
    let unmarked = json!({ "name": TO, "config": {} });
    assert_eq!(raw(&fake, "POST", "/1.0/projects", &unmarked).0, 201);
    let sent = fake.writes().len();
    refused_naming(foreign.run(AdoptStep::DeleteProject), "not this adoption's");
    assert_eq!(fake.writes().len(), sent, "no write");

    // A marked project that sets restrictions other than the plan's is no CreateProject done, and no step loosens it.
    let (_dir, fake) = start("loose");
    let mut loose = adoption_project_body(&target(), "br1", &BTreeMap::new());
    loose["config"]["restricted.containers.nesting"] = json!("allow");
    assert_eq!(raw(&fake, "POST", "/1.0/projects", &loose).0, 201);
    let sent = fake.writes().len();
    refused_naming(
        writer(&fake).run(AdoptStep::CreateProject),
        "restrictions other than the plan's",
    );
    assert_eq!(fake.writes().len(), sent, "no write");
}

#[test]
fn restore_ready_refuses_while_adopted_when_the_instances_own_root_changed_since_it_was_localized()
{
    // A release judges this before ReleaseStop: RestoreRoot would keep a changed root, so the release would stop half way.
    let (_dir, fake) = start_acl("restore-ready");
    let writer = allowing(&fake, acl_pre(), root_localized());
    assert_eq!(writer.restore_ready(), Ok(()), "before any step");
    for step in writer.adopt_steps() {
        writer.run(step).expect("the adoption");
    }
    assert_eq!(writer.restore_ready(), Ok(()));
    let mut bigger = root();
    bigger["size"] = json!("80GiB");
    let path = format!("/1.0/instances/svc1?project={TO}");
    let (status, _) = raw(
        &fake,
        "PATCH",
        &path,
        &json!({ "devices": { "root": bigger } }),
    );
    assert_eq!(status, 200);
    let sent = fake.writes().len();
    let ready = writer.restore_ready();
    assert!(
        matches!(&ready, Err(IncusError::Refused(why)) if why.contains("changed since the adoption")),
        "{ready:?}"
    );
    assert_eq!(fake.writes().len(), sent, "GET alone");
    let (status, _) = raw(
        &fake,
        "PATCH",
        &path,
        &json!({ "devices": { "root": root() } }),
    );
    assert_eq!(status, 200);
    assert_eq!(writer.restore_ready(), Ok(()));
}

// A stopped instance. Incus starts it with eth0's own hwaddr, else its volatile.eth0.hwaddr, so the plan pins that MAC and the
// IPv4 the owner names. The adoption neither stops nor starts it, and the release leaves it stopped.

/// `svc1`, stopped: its MAC is its volatile key, and the fixture's lease of `IP` for it is dynamic.
fn stopped_svc1() -> Value {
    let mut instance = svc1(json!({}));
    instance["status"] = json!("Stopped");
    instance
}

/// The pre-image of `svc1` as a plan finds it stopped.
fn stopped_pre() -> PreImage {
    PreImage {
        stopped: true,
        ..pre()
    }
}

fn stopped_writer(fake: &FakeIncus) -> AdoptWriter {
    AdoptWriter::new(
        fake.socket().to_path_buf(),
        target(),
        vec!["br1".to_string()],
        stopped_pre(),
    )
    .expect("the writer")
    .with_snapshot(SNAPSHOT)
    .expect("the snapshot name")
}

/// Whether any write the fake has seen is a PUT of an instance's state: a stop or a start.
fn sent_a_state_change(fake: &FakeIncus) -> bool {
    writes(fake)
        .iter()
        .any(|(method, path)| method == "PUT" && path.contains("/state"))
}

#[test]
fn a_stopped_instance_is_adopted_and_released_with_no_stop_or_start_and_stays_stopped() {
    use AdoptStep as S;
    let (_dir, fake) = start_with("stopped", stopped_svc1());
    let writer = stopped_writer(&fake);
    let before = instance_in(&fake, "default").expect("the instance is in default");
    assert_eq!(
        writer.adopt_steps(),
        [S::CreateProject, S::Snapshot, S::PinNic, S::Move, S::Mark],
        "a plan that found the instance stopped has no Stop and no Start"
    );
    for step in writer.adopt_steps() {
        let sent = fake.writes().len();
        assert_eq!(writer.run(step), Ok(StepOutcome::Sent), "{step:?} runs");
        let table: Vec<(String, String)> = writer
            .requests(step)
            .into_iter()
            .map(|request| (request.method.to_string(), request.path))
            .collect();
        assert_eq!(writes(&fake)[sent..].to_vec(), table, "{step:?}");
        let sent = fake.writes().len();
        assert_eq!(
            writer.run(step),
            Ok(StepOutcome::AlreadyDone),
            "{step:?} again"
        );
        assert_eq!(fake.writes().len(), sent, "{step:?} again sends nothing");
    }
    assert!(instance_in(&fake, "default").is_none(), "it left default");
    let after = instance_in(&fake, TO).expect("the instance is in its adoption project");
    assert_eq!(
        after["status"],
        json!("Stopped"),
        "the adoption leaves it stopped"
    );
    assert_eq!(after["config"][MARK], json!("adopt-svc1"));
    let copied = after["config"]["volatile.uuid"].as_str().expect("a uuid");
    assert_ne!(copied, UUID, "the move is a copy, under a new uuid");
    assert_eq!(writer.uuid(), copied);
    assert_eq!(
        after["devices"]["eth0"],
        pin_patch(writer.pin())["devices"]["eth0"],
        "the local eth0 pins the MAC it starts with and the IPv4 the plan names"
    );
    assert_eq!(writer.pin().mac, MAC);
    assert_eq!(after["devices"]["root"], before["devices"]["root"]);
    assert!(
        read(
            &fake,
            &format!("/1.0/instances/svc1/snapshots/{SNAPSHOT}?project={TO}")
        )
        .is_ok(),
        "the snapshot moved with the instance"
    );

    assert_eq!(
        writer.release_steps(),
        [S::Unmark, S::MoveBack, S::DeleteProject],
        "the release neither stops nor starts it"
    );
    for step in writer.release_steps() {
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
    assert_eq!(
        back["status"],
        json!("Stopped"),
        "the release leaves it stopped"
    );
    assert!(back["config"].get(MARK).is_none(), "the mark is gone");
    assert_eq!(
        back["devices"]["eth0"],
        pin_patch(writer.pin())["devices"]["eth0"],
        "the pin stays"
    );
    assert!(
        matches!(
            read(&fake, &format!("/1.0/projects/{TO}")),
            Err(IncusError::Status { code: 404, .. })
        ),
        "the emptied adoption project is deleted"
    );
    assert!(
        !sent_a_state_change(&fake),
        "nothing stopped or started it: {:?}",
        writes(&fake)
    );
    assert_eq!(
        fake.leases(),
        vec![("br1".to_string(), MAC.to_string(), IP.to_string())],
        "no start leased anything"
    );
}

#[test]
fn a_stopped_plan_has_no_stop_or_start_request_and_expects_the_instance_stopped_at_every_step() {
    use AdoptStep as S;
    let (_dir, fake) = start_with("stopped-table", stopped_svc1());
    let writer = stopped_writer(&fake);
    for step in [S::Stop, S::Start, S::ReleaseStop, S::ReleaseStart] {
        assert!(writer.requests(step).is_empty(), "{step:?} has no request");
        refused_naming(writer.run(step), "stopped");
    }
    for step in S::ADOPT.iter().chain(S::RELEASE.iter()) {
        for request in writer.requests(*step) {
            assert!(
                !request.path.contains("/state"),
                "{step:?}: {}",
                request.path
            );
        }
    }
    for step in writer
        .adopt_steps()
        .into_iter()
        .chain(writer.release_steps())
    {
        let (running, stopped) = (AdoptWriter::expect(step), writer.expects(step));
        assert_eq!(
            (stopped.project, stopped.class),
            (running.project, running.class),
            "{step:?}: the project and the class are the running plan's"
        );
        assert_eq!(stopped.status, "Stopped", "{step:?}");
    }
    // A running plan's own table is unchanged.
    let running = self::writer(&fake);
    assert_eq!(running.expects(S::CreateProject).status, "Running");
    assert_eq!(running.expects(S::DeleteProject).status, "Running");
    assert_eq!(running.requests(S::Stop).len(), 2);
    assert!(fake.writes().is_empty(), "{:?}", fake.writes());
}

#[test]
fn a_stopped_plan_refuses_an_instance_started_since_and_a_running_plan_refuses_a_stopped_one() {
    use AdoptStep as S;
    // The plan found the instance stopped, and it runs now: no step of the stopped plan takes it.
    let (_dir, fake) = start("stopped-plan-running");
    let writer = stopped_writer(&fake);
    for step in [S::CreateProject, S::Snapshot, S::PinNic, S::Move] {
        refused_naming(writer.run(step), "status");
    }
    assert!(fake.writes().is_empty(), "{:?}", fake.writes());

    // The plan found it running, and it is stopped now: CreateProject, the first step, refuses it.
    let (_dir, fake) = start_with("running-plan-stopped", stopped_svc1());
    refused_naming(self::writer(&fake).run(S::CreateProject), "status");
    assert!(fake.writes().is_empty(), "{:?}", fake.writes());
}

#[test]
fn a_stopped_adoption_started_by_hand_is_released_only_once_it_is_stopped_again() {
    use AdoptStep as S;
    let (_dir, fake) = start_with("stopped-started", stopped_svc1());
    let writer = stopped_writer(&fake);
    for step in writer.adopt_steps() {
        writer.run(step).expect("the adoption");
    }
    let state = format!("/1.0/instances/svc1/state?project={TO}");
    assert_eq!(
        raw(&fake, "PUT", &state, &json!({ "action": "start" })).0,
        202
    );
    let sent = fake.writes().len();
    // The release has no stop: Unmark refuses a running instance, and nothing is sent.
    refused_naming(writer.run(S::Unmark), "status");
    assert_eq!(fake.writes().len(), sent, "no write");
    let stop = json!({ "action": "stop", "force": true });
    assert_eq!(raw(&fake, "PUT", &state, &stop).0, 202);
    for step in writer.release_steps() {
        assert_eq!(writer.run(step), Ok(StepOutcome::Sent), "{step:?}");
    }
    let back = instance_in(&fake, "default").expect("back in default");
    assert_eq!(back["status"], json!("Stopped"));
}

#[test]
fn a_stopped_plans_mark_refuses_an_instance_started_after_the_move_and_its_release_runs_once_it_is_stopped()
 {
    use AdoptStep as S;
    let (_dir, fake) = start_with("stopped-mark-running", stopped_svc1());
    let writer = stopped_writer(&fake);
    for step in [S::CreateProject, S::Snapshot, S::PinNic, S::Move] {
        assert_eq!(writer.run(step), Ok(StepOutcome::Sent), "{step:?}");
    }
    let state = format!("/1.0/instances/svc1/state?project={TO}");
    assert_eq!(
        raw(&fake, "PUT", &state, &json!({ "action": "start" })).0,
        202
    );
    let sent = fake.writes().len();
    // Mark is the stopped plan's last step: it does not complete the adoption of a running instance.
    refused_naming(writer.run(S::Mark), "stopped");
    assert_eq!(fake.writes().len(), sent, "no write");
    let seen = instance_in(&fake, TO).expect("still in its adoption project");
    assert!(seen["config"].get(MARK).is_none(), "not marked");
    assert_eq!(seen["status"], json!("Running"));

    // The release refuses it while it runs, and takes it back once the owner stops it.
    refused_naming(writer.run(S::MoveBack), "status");
    assert_eq!(fake.writes().len(), sent, "no write");
    let stop = json!({ "action": "stop", "force": true });
    assert_eq!(raw(&fake, "PUT", &state, &stop).0, 202);
    for step in writer.release_steps() {
        writer.run(step).expect("the release");
    }
    let back = instance_in(&fake, "default").expect("back in default");
    assert_eq!(back["status"], json!("Stopped"));
    assert!(back["config"].get(MARK).is_none(), "no mark");
    assert!(
        instance_in(&fake, TO).is_none(),
        "gone from its adoption project"
    );

    // A running plan's Mark still takes an instance started by hand after its move.
    let (_dir, fake) = start("running-mark-running");
    let writer = self::writer(&fake);
    for step in [
        S::CreateProject,
        S::Stop,
        S::Snapshot,
        S::PinNic,
        S::LocalizeRoot,
        S::Move,
    ] {
        writer.run(step).expect("the adoption up to Mark");
    }
    assert_eq!(
        raw(&fake, "PUT", &state, &json!({ "action": "start" })).0,
        202
    );
    assert_eq!(writer.run(S::Mark), Ok(StepOutcome::Sent));
    assert_eq!(writer.run(S::Start), Ok(StepOutcome::AlreadyDone));
}
