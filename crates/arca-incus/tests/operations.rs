//! The operations reads: the listing of operations Incus is still working on, which object each one names, and the bounded wait
//! on one. They are checked against canned answers and against the fake Incus, whose held operations run until the test ends
//! them. No test connects to a real Incus socket.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arca_incus::{
    Appears, FakeIncus, FakeState, IncusError, IncusRead, OPERATIONS_PATH, RunningOp, UnixIncus,
    operation_names, running_on, running_operations, wait_operation, writing_on,
};
use arca_kernel::ownership::{IncusKind, ObjectRef};
use serde_json::{Value, json};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/state.json");
const OP: &str = "00000000-0000-4000-8000-0000000000aa";

/// A fresh temporary directory, removed when the test ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .subsec_nanos();
        let dir = std::env::temp_dir().join(format!(
            "arca-operations-{}-{nanos}-{tag}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create the scratch directory");
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn start(tag: &str) -> (Scratch, FakeIncus) {
    let dir = Scratch::new(tag);
    let state = FakeState::load(Path::new(FIXTURE)).expect("load the fixture");
    let fake = FakeIncus::start(&dir.0, state).expect("start the fake");
    (dir, fake)
}

fn reader(fake: &FakeIncus) -> UnixIncus {
    UnixIncus::new(fake.socket().to_path_buf())
}

/// A reader that answers every GET with one canned result and records the paths it was asked for.
struct Canned {
    answer: Result<Value, IncusError>,
    asked: Mutex<Vec<String>>,
}

impl Canned {
    fn new(answer: Result<Value, IncusError>) -> Canned {
        Canned {
            answer,
            asked: Mutex::new(Vec::new()),
        }
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().expect("the log").clone()
    }
}

impl IncusRead for Canned {
    fn get(&self, path: &str) -> Result<Value, IncusError> {
        self.asked.lock().expect("the log").push(path.to_string());
        self.answer.clone()
    }
}

fn op(id: &str, code: u16, status: &str, resources: Value) -> Value {
    json!({ "id": id, "class": "task", "status": status, "status_code": code, "resources": resources, "err": "" })
}

fn running(resources: &[&str]) -> RunningOp {
    RunningOp {
        id: OP.to_string(),
        status_code: 103,
        resources: resources.iter().map(|url| url.to_string()).collect(),
        class: None,
        description: None,
    }
}

fn object(kind: IncusKind, project: &str, parent: Option<&str>, name: &str) -> ObjectRef {
    ObjectRef {
        kind,
        project: project.to_string(),
        parent: parent.map(str::to_string),
        name: name.to_string(),
        uuid: None,
    }
}

fn names(op: &RunningOp, target: &ObjectRef) -> bool {
    operation_names(
        op,
        target.kind,
        &target.project,
        target.parent.as_deref(),
        &target.name,
    )
}

/// Sends the create of instance `name` in `svc` to the fake, outside any client, and returns the status.
fn post(fake: &FakeIncus, name: &str) -> u16 {
    let mut stream = UnixStream::connect(fake.socket()).expect("the fake accepts");
    let body = json!({ "name": name, "config": {} }).to_string();
    let request = format!(
        "POST /1.0/instances?project=svc HTTP/1.1\r\nHost: incus\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).expect("written");
    let mut answer = String::new();
    stream.read_to_string(&mut answer).expect("read");
    answer
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .expect("a status")
}

fn ids(ops: &[RunningOp]) -> Vec<&str> {
    ops.iter().map(|op| op.id.as_str()).collect()
}

#[test]
fn the_listing_keeps_only_the_operations_that_have_not_ended() {
    let listing = json!({
        "running": [op("00000000-0000-4000-8000-000000000003", 103, "Running", json!({"instances": ["/1.0/instances/c1?project=p1"]}))],
        "pending": [op("00000000-0000-4000-8000-000000000005", 105, "Pending", json!(null))],
        "cancelling": [op("00000000-0000-4000-8000-000000000004", 104, "Cancelling", json!({}))],
        "success": [op("00000000-0000-4000-8000-000000000200", 200, "Success", json!({}))],
        "failure": [op("00000000-0000-4000-8000-000000000400", 400, "Failure", json!({}))],
        "cancelled": [op("00000000-0000-4000-8000-000000000401", 401, "Cancelled", json!({}))],
        "empty": null,
    });
    let canned = Canned::new(Ok(listing));
    let found = running_operations(&canned).expect("the listing reads");
    assert_eq!(
        ids(&found),
        vec![
            "00000000-0000-4000-8000-000000000003",
            "00000000-0000-4000-8000-000000000004",
            "00000000-0000-4000-8000-000000000005",
        ]
    );
    assert_eq!(found[0].status_code, 103);
    assert_eq!(found[0].resources, vec!["/1.0/instances/c1?project=p1"]);
    assert!(found[1].resources.is_empty() && found[2].resources.is_empty());
    assert_eq!(canned.asked(), vec![OPERATIONS_PATH.to_string()]);
    assert_eq!(
        OPERATIONS_PATH,
        "/1.0/operations?all-projects=true&recursion=1"
    );
}

#[test]
fn the_listing_reads_each_operations_class_and_description_when_they_are_text() {
    let listing = json!({ "running": [
        { "id": "00000000-0000-4000-8000-000000000001", "class": "websocket", "description": "Executing command", "status_code": 103 },
        { "id": "00000000-0000-4000-8000-000000000002", "status_code": 103 },
        { "id": "00000000-0000-4000-8000-000000000003", "class": 2, "description": null, "status_code": 103 },
    ]});
    let found = running_operations(&Canned::new(Ok(listing))).expect("the listing reads");
    let read: Vec<(Option<&str>, Option<&str>)> = found
        .iter()
        .map(|op| (op.class.as_deref(), op.description.as_deref()))
        .collect();
    assert_eq!(
        read,
        vec![
            (Some("websocket"), Some("Executing command")),
            (None, None),
            (None, None)
        ]
    );
}

#[test]
fn only_an_operation_that_can_still_create_change_or_delete_its_object_may_write_it() {
    let typed = |class: &str, description: &str| RunningOp {
        class: Some(class.to_string()),
        description: Some(description.to_string()),
        ..running(&[])
    };
    // Incus runs an exec as a websocket operation, or as a task one without websockets, a console as a websocket one, and a
    // token operation runs nothing at all. None of them writes the instance, and a session stays running while it is open.
    for (class, description) in [
        ("websocket", "Executing command"),
        ("task", "Executing command"),
        ("websocket", "Showing console"),
        ("token", "Image download token"),
    ] {
        assert!(
            !typed(class, description).may_write(),
            "{class} {description}"
        );
    }
    // A push migration's create is a websocket operation, so the class alone frees nothing; a description is matched exactly.
    for (class, description) in [
        ("websocket", "Creating instance"),
        ("websocket", "Migrating instance"),
        ("task", "Creating instance"),
        ("task", "Updating instance"),
        ("task", "Deleting instance"),
        ("task", "executing command"),
        ("task", "Held by the test"),
    ] {
        assert!(
            typed(class, description).may_write(),
            "{class} {description}"
        );
    }
    assert!(
        running(&[]).may_write(),
        "an operation with no class and no description may write"
    );
}

#[test]
fn writing_on_leaves_out_the_sessions_running_on_an_object() {
    let (_dir, fake) = start("writing-on");
    let target = object(IncusKind::Instance, "svc", None, "db1");
    fake.hold_operation_as(&target, "websocket", "Executing command");
    fake.hold_operation_as(&target, "task", "Executing command");
    fake.hold_operation_as(&target, "websocket", "Showing console");
    let create = fake.hold_operation_as(&target, "websocket", "Creating instance");
    let read = reader(&fake);
    let all = running_on(&read, IncusKind::Instance, "svc", None, "db1").expect("lists");
    assert_eq!(
        all.len(),
        4,
        "running_on holds every operation that names it"
    );
    let writing = writing_on(&read, IncusKind::Instance, "svc", None, "db1").expect("lists");
    assert_eq!(ids(&writing), vec![create.as_str()]);
}

#[test]
fn a_listing_that_is_not_a_map_of_operation_lists_is_a_decode_error() {
    let good = || op(OP, 103, "Running", json!({}));
    for listing in [
        json!([good()]),
        json!("running"),
        json!({ "running": good() }),
        json!({ "running": [json!("an id")] }),
        json!({ "running": [{ "status_code": 103 }] }),
        json!({ "running": [op("../../1.0/projects", 103, "Running", json!({}))] }),
        json!({ "running": [op("", 103, "Running", json!({}))] }),
        json!({ "running": [{ "id": OP, "status": "Running" }] }),
        json!({ "running": [{ "id": OP, "status_code": 70000 }] }),
        json!({ "running": [op(OP, 103, "Running", json!(["/1.0/instances/c1"]))] }),
        json!({ "running": [op(OP, 103, "Running", json!({ "instances": "/1.0/instances/c1" }))] }),
        json!({ "running": [op(OP, 103, "Running", json!({ "instances": [7] }))] }),
    ] {
        assert!(
            matches!(
                running_operations(&Canned::new(Ok(listing.clone()))),
                Err(IncusError::Decode(_))
            ),
            "{listing}"
        );
    }
    let refused = IncusError::Status {
        code: 500,
        message: "no".to_string(),
    };
    assert_eq!(
        running_operations(&Canned::new(Err(refused.clone()))),
        Err(refused)
    );
}

#[test]
fn an_operation_names_an_instance_by_its_path_and_its_project() {
    let in_p1 = object(IncusKind::Instance, "p1", None, "c1");
    let in_default = object(IncusKind::Instance, "default", None, "c1");
    for (url, target, named) in [
        ("/1.0/instances/c1?project=p1", &in_p1, true),
        ("/1.0/instances/c1?project=p2", &in_p1, false),
        ("/1.0/instances/c1", &in_p1, false),
        ("/1.0/instances/c1", &in_default, true),
        ("/1.0/instances/c1?project=p1", &in_default, false),
        ("/1.0/instances/c1/snapshots/s1?project=p1", &in_p1, true),
        ("/1.0/instances/c10?project=p1", &in_p1, false),
        ("/1.0/instances/c?project=p1", &in_p1, false),
        ("/1.0/containers/c1?project=p1", &in_p1, true),
        ("/1.0/virtual-machines/c1?project=p1", &in_p1, true),
        ("/1.0/instances/%63%31?project=p1", &in_p1, true),
        ("/1.0/instances/c1?project=p%31", &in_p1, true),
        ("/1.0//instances/c1/?project=p1", &in_p1, true),
        ("/1.0/instances/c1?x=1&project=p1", &in_p1, true),
        ("/1.0/profiles/c1?project=p1", &in_p1, false),
        ("/2.0/instances/c1?project=p1", &in_p1, false),
        ("/instances/c1?project=p1", &in_p1, false),
    ] {
        assert_eq!(names(&running(&[url]), target), named, "{url}");
    }
    assert!(
        !names(&running(&[]), &in_p1),
        "an operation without resources names nothing"
    );
    assert!(
        names(
            &running(&["/1.0/images/abc", "/1.0/instances/c1?project=p1"]),
            &in_p1
        ),
        "any one resource names the object"
    );
}

#[test]
fn an_operation_names_a_volume_a_bucket_and_a_forward_under_their_parent() {
    let volume = object(IncusKind::Volume, "p1", Some("f"), "v1");
    let any_pool = object(IncusKind::Volume, "p1", None, "v1");
    let bucket = object(IncusKind::Bucket, "p1", Some("f"), "b1");
    let forward = object(IncusKind::Forward, "default", Some("br1"), "192.0.2.5");
    let network = object(IncusKind::Network, "default", None, "br1");
    for (url, target, named) in [
        (
            "/1.0/storage-pools/f/volumes/custom/v1?project=p1",
            &volume,
            true,
        ),
        (
            "/1.0/storage-pools/g/volumes/custom/v1?project=p1",
            &volume,
            false,
        ),
        ("/1.0/storage-pools/f/volumes/custom/v1", &volume, false),
        (
            "/1.0/storage-pools/g/volumes/custom/v1?project=p1",
            &any_pool,
            true,
        ),
        ("/1.0/storage-pools/f/buckets/b1?project=p1", &bucket, true),
        (
            "/1.0/storage-pools/f/volumes/custom/b1?project=p1",
            &bucket,
            false,
        ),
        ("/1.0/networks/br1/forwards/192.0.2.5", &forward, true),
        ("/1.0/networks/br2/forwards/192.0.2.5", &forward, false),
        ("/1.0/networks/br1/forwards/192.0.2.5", &network, true),
        ("/1.0/networks/br1?project=p1", &network, true),
    ] {
        assert_eq!(names(&running(&[url]), target), named, "{url}");
    }
}

#[test]
fn a_network_an_acl_and_a_forward_compare_in_default_and_a_project_by_its_path() {
    let acl_in_kernel_project = object(IncusKind::Acl, "p1", None, "a1");
    let acl_in_default = object(IncusKind::Acl, "default", None, "a1");
    for url in ["/1.0/network-acls/a1", "/1.0/network-acls/a1?project=p1"] {
        let op = running(&[url]);
        assert!(names(&op, &acl_in_kernel_project), "{url}");
        assert!(names(&op, &acl_in_default), "{url}");
    }
    assert!(!names(&running(&["/1.0/network-acls/a2"]), &acl_in_default));
    let project = object(IncusKind::Project, "p1", None, "p1");
    assert!(names(&running(&["/1.0/projects/p1"]), &project));
    assert!(!names(&running(&["/1.0/projects/p2"]), &project));
    assert!(
        !names(&running(&["/1.0/instances/c1?project=p1"]), &project),
        "an operation on an object inside a project does not name the project"
    );
}

#[test]
fn a_wait_says_whether_the_operation_still_runs_and_names_only_that_operation() {
    for (code, status, still) in [
        (103, "Running", true),
        (105, "Pending", true),
        (104, "Cancelling", true),
        (200, "Success", false),
        (400, "Failure", false),
        (401, "Cancelled", false),
    ] {
        let canned = Canned::new(Ok(op(OP, code, status, json!({}))));
        assert_eq!(
            wait_operation(&canned, OP, Duration::from_secs(5)),
            Ok(still),
            "{status}"
        );
        assert_eq!(
            canned.asked(),
            vec![format!("/1.0/operations/{OP}/wait?timeout=5")]
        );
    }
    let gone = Canned::new(Err(IncusError::Status {
        code: 404,
        message: "not found".to_string(),
    }));
    assert_eq!(wait_operation(&gone, OP, Duration::from_secs(1)), Ok(false));
    let failed = IncusError::Status {
        code: 500,
        message: "no".to_string(),
    };
    assert_eq!(
        wait_operation(&Canned::new(Err(failed.clone())), OP, Duration::ZERO),
        Err(failed)
    );
    assert_eq!(
        wait_operation(&Canned::new(Err(IncusError::Timeout)), OP, Duration::ZERO),
        Err(IncusError::Timeout)
    );
    let other = op(&OP.replace("aa", "bb"), 200, "Success", json!({}));
    for answer in [other, json!(null), json!({ "id": OP })] {
        let read = Canned::new(Ok(answer.clone()));
        let decoded = wait_operation(&read, OP, Duration::ZERO);
        assert!(matches!(decoded, Err(IncusError::Decode(_))), "{answer}");
    }
}

#[test]
fn a_wait_on_an_id_that_is_not_plain_is_refused_before_any_request() {
    for id in ["", "../projects", "abc?x", "abc/wait", &"a".repeat(65)] {
        let canned = Canned::new(Ok(op(OP, 200, "Success", json!({}))));
        assert!(
            matches!(
                wait_operation(&canned, id, Duration::ZERO),
                Err(IncusError::Refused(_))
            ),
            "{id}"
        );
        assert!(canned.asked().is_empty(), "{id}");
    }
}

#[test]
fn a_held_operation_is_listed_and_waited_on_until_the_test_releases_it() {
    let (_dir, fake) = start("held");
    let read = reader(&fake);
    let held = fake.hold_operation(&object(IncusKind::Instance, "svc", None, "db1"));
    fake.hold_operation(&object(IncusKind::Instance, "default", None, "guest1"));
    let found = running_on(&read, IncusKind::Instance, "svc", None, "db1").expect("lists");
    assert_eq!(ids(&found), vec![held.as_str()]);
    assert_eq!(found[0].resources, vec!["/1.0/instances/db1?project=svc"]);
    let in_default = running_on(&read, IncusKind::Instance, "default", None, "guest1");
    assert_eq!(
        in_default.expect("lists")[0].resources,
        vec!["/1.0/instances/guest1"],
        "an object in default is named without a project query"
    );
    assert_eq!(
        wait_operation(&read, held.as_str(), Duration::ZERO),
        Ok(true)
    );

    fake.release_operation(&held);
    assert_eq!(
        wait_operation(&read, held.as_str(), Duration::ZERO),
        Ok(false)
    );
    let left = running_on(&read, IncusKind::Instance, "svc", None, "db1").expect("lists");
    assert!(left.is_empty());
    let listing = read.get(OPERATIONS_PATH).expect("lists");
    assert_eq!(listing["success"][0]["id"], json!(held.as_str()));
    assert!(fake.writes().is_empty(), "the reads sent no write");
}

#[test]
fn a_held_create_shows_its_object_on_completion_or_while_it_runs_and_a_failure_removes_it() {
    let (_dir, fake) = start("held-create");
    let read = reader(&fake);
    let instance = |name: &str| object(IncusKind::Instance, "svc", None, name);
    let path = |name: &str| format!("/1.0/instances/{name}?project=svc");
    let later = fake.hold_create(&instance("new1"), Appears::OnCompletion);
    let early = fake.hold_create(&instance("new2"), Appears::WhileRunning);
    let armed = running_operations(&read).expect("lists");
    assert!(
        armed.is_empty(),
        "an armed create is not listed before it arrives"
    );
    assert_eq!((post(&fake, "new1"), post(&fake, "new2")), (202, 202));
    assert!(matches!(
        read.get(&path("new1")),
        Err(IncusError::Status { code: 404, .. })
    ));
    assert!(read.get(&path("new2")).is_ok(), "new2 shows while it runs");
    assert_eq!(running_operations(&read).expect("lists").len(), 2);

    fake.release_operation(&later);
    fake.fail_operation(&early, "the create failed");
    assert!(read.get(&path("new1")).is_ok(), "new1 lands when it ends");
    assert!(
        matches!(
            read.get(&path("new2")),
            Err(IncusError::Status { code: 404, .. })
        ),
        "a failed create leaves nothing"
    );
    let failed = read.get(&format!("/1.0/operations/{}", early.as_str()));
    let failed = failed.expect("the ended operation is still held");
    assert_eq!(failed["status_code"], json!(400));
    assert_eq!(failed["err"], json!("the create failed"));
    assert!(running_operations(&read).expect("lists").is_empty());
}

#[test]
fn the_fake_holds_a_wait_until_the_operation_ends_and_never_past_a_quarter_second() {
    let (_dir, fake) = start("hold");
    let read = reader(&fake);
    let held = fake.hold_operation(&object(IncusKind::Instance, "svc", None, "db1"));
    let started = Instant::now();
    let still = wait_operation(&read, held.as_str(), Duration::from_secs(30));
    let held_for = started.elapsed();
    assert_eq!(still, Ok(true));
    assert!(
        held_for >= Duration::from_millis(200) && held_for < Duration::from_secs(5),
        "the wait was held about a quarter second: {held_for:?}"
    );

    let fake = Arc::new(fake);
    let releaser = {
        let (fake, held) = (Arc::clone(&fake), held.clone());
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            fake.release_operation(&held);
        })
    };
    let started = Instant::now();
    while wait_operation(&read, held.as_str(), Duration::from_secs(5)) == Ok(true) {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the wait saw the end"
        );
    }
    releaser.join().expect("the releaser ends");
    assert_eq!(
        wait_operation(&read, held.as_str(), Duration::ZERO),
        Ok(false)
    );
}

#[test]
fn an_operations_listing_the_fake_cannot_answer_is_a_status_error() {
    let (_dir, fake) = start("unreadable");
    let read = reader(&fake);
    fake.hold_operation(&object(IncusKind::Instance, "svc", None, "db1"));
    fake.set_operations_unreadable(true);
    assert!(matches!(
        running_on(&read, IncusKind::Instance, "svc", None, "db1"),
        Err(IncusError::Status { code: 500, .. })
    ));
    fake.set_operations_unreadable(false);
    let found = running_on(&read, IncusKind::Instance, "svc", None, "db1");
    assert_eq!(found.expect("the listing reads again").len(), 1);
}
