#![cfg(feature = "write")]
//! The Incus writer, tested against the fake Incus and against small canned servers that answer a scripted sequence. Every
//! server is bound under a fresh temporary directory, and no test connects to a real Incus socket. Each refusal is checked
//! twice: the writer returns `IncusError::Refused`, and the server sees no write.

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use arca_incus::{
    FakeIncus, FakeState, IncusError, Owned, ProjectFamily, Reading, Target, Unconfirmed,
    UnixIncusWriter,
};
use arca_kernel::canon::Digest;
use arca_kernel::desired::{EntryId, PluginId};
use arca_kernel::ownership::{IncusKind, ObjectRef, OwnerClass};
use serde_json::{Value, json};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/state.json");
const MARK: &str = "user.arcakernel.entry";
const RETIRING: &str = "user.arcakernel.retiring";
/// The projects the kernel's family names in these tests. `droid` is the other system's project and is never listed.
const FAMILY: [&str; 4] = ["svc", "kproj1", "kfull", "kcan"];
const NONE: [&str; 0] = [];
const UUID: &str = "00000000-0000-4000-8000-0000000000cc";
const OTHER_UUID: &str = "00000000-0000-4000-8000-00000000ffff";
const OP: &str = "00000000-0000-4000-8000-0000000000aa";

/// A fresh temporary directory, removed when the test ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .subsec_nanos();
        let dir =
            std::env::temp_dir().join(format!("arca-write-{}-{nanos}-{tag}", std::process::id()));
        fs::create_dir_all(&dir).expect("create the scratch directory");
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fixture() -> FakeState {
    FakeState::load(Path::new(FIXTURE)).expect("load the fixture")
}

fn start(tag: &str) -> (Scratch, FakeIncus) {
    let dir = Scratch::new(tag);
    let fake = FakeIncus::start(&dir.0, fixture()).expect("start the fake");
    (dir, fake)
}

fn family() -> ProjectFamily {
    ProjectFamily::new(FAMILY, ["droid"]).expect("the family")
}

fn writer(fake: &FakeIncus, entry: &str) -> UnixIncusWriter {
    UnixIncusWriter::new(
        fake.socket().to_path_buf(),
        EntryId(entry.to_string()),
        BTreeMap::new(),
        family(),
    )
}

fn target(kind: IncusKind, project: &str, name: &str, parent: Option<&str>) -> Target {
    ObjectRef {
        kind,
        project: project.to_string(),
        parent: parent.map(str::to_string),
        name: name.to_string(),
        uuid: None,
    }
}

fn project_target(name: &str) -> Target {
    target(IncusKind::Project, name, name, None)
}

/// Whether Incus gives an object of this kind a uuid: instances and volumes do, through their `volatile.uuid` key.
fn has_uuid(kind: IncusKind) -> bool {
    matches!(kind, IncusKind::Instance | IncusKind::Volume)
}

/// A create body that marks the object as the entry `toy`.
fn marked() -> Value {
    json!({ "config": { MARK: "toy" } })
}

fn changes(pairs: &[(&str, Option<&str>)]) -> BTreeMap<String, Option<String>> {
    pairs
        .iter()
        .map(|(key, value)| (key.to_string(), value.map(str::to_string)))
        .collect()
}

fn plan() -> Digest {
    Digest::of(b"a delete plan for the test")
}

fn status_of(error: &IncusError) -> Option<u16> {
    match error {
        IncusError::Status { code, .. } => Some(*code),
        _ => None,
    }
}

/// The owner class an unconfirmed write reports, or `None` when the result is not an unconfirmed write.
fn owner_of(result: &Result<Owned, IncusError>) -> Option<OwnerClass> {
    match result {
        Err(IncusError::AppliedUnconfirmed(unconfirmed)) => unconfirmed.owner.clone(),
        _ => None,
    }
}

/// Writes raw bytes to the fake and returns everything it sends back. Only the fake is ever addressed here.
fn raw(socket: &Path, request: &str) -> String {
    let mut stream = UnixStream::connect(socket).expect("connect to the fake");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    stream.write_all(request.as_bytes()).expect("write");
    let mut reply = Vec::new();
    let _ = stream.read_to_end(&mut reply);
    String::from_utf8_lossy(&reply).into_owned()
}

/// A raw PATCH to the fake, the way another writer's change would reach it.
fn raw_patch(socket: &Path, path: &str, if_match: Option<&str>, body: &Value) -> String {
    let body = body.to_string();
    let condition = if_match
        .map(|tag| format!("If-Match: {tag}\r\n"))
        .unwrap_or_default();
    raw(
        socket,
        &format!(
            "PATCH {path} HTTP/1.1\r\nHost: incus\r\n{condition}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    )
}

/// The fake's write log is checked for a count that does not move, so a refused call is seen as sending nothing.
fn write_count(fake: &FakeIncus) -> usize {
    fake.writes().len()
}

/// A server on its own socket that answers each connection with `answer(method, target)`. The requests it has seen are kept
/// in order, so a test can see what was sent.
struct Canned {
    _dir: Scratch,
    socket: PathBuf,
    seen: Arc<Mutex<Vec<(String, String)>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Canned {
    fn start(tag: &str, mut answer: impl FnMut(&str, &str) -> Vec<u8> + Send + 'static) -> Canned {
        let dir = Scratch::new(tag);
        let socket = dir.0.join("canned.sock");
        let listener = UnixListener::bind(&socket).expect("bind the canned socket");
        listener
            .set_nonblocking(true)
            .expect("a non-blocking accept");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let seen = Arc::clone(&seen);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => serve_once(stream, &mut answer, &seen),
                        Err(_) => thread::sleep(Duration::from_millis(2)),
                    }
                }
            })
        };
        Canned {
            _dir: dir,
            socket,
            seen,
            stop,
            thread: Some(thread),
        }
    }

    /// A writer for `toy` over this server, with a short operation deadline so a wait that never ends is quick to see.
    fn writer(&self) -> UnixIncusWriter {
        UnixIncusWriter::new(
            self.socket.clone(),
            EntryId("toy".to_string()),
            BTreeMap::new(),
            family(),
        )
        .with_operation_deadline(Duration::from_millis(300))
    }

    /// Every request seen so far, as (method, target).
    fn seen(&self) -> Vec<(String, String)> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn count(&self, method: &str) -> usize {
        self.seen().iter().filter(|(m, _)| m == method).count()
    }

    fn waits(&self) -> usize {
        self.seen()
            .iter()
            .filter(|(_, t)| t.contains("/wait"))
            .count()
    }
}

impl Drop for Canned {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Reads one request, records its method and target, and writes the reply `answer` gives.
fn serve_once(
    mut stream: UnixStream,
    answer: &mut impl FnMut(&str, &str) -> Vec<u8>,
    seen: &Mutex<Vec<(String, String)>>,
) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(1) => head.push(byte[0]),
            _ => return,
        }
    }
    let text = String::from_utf8_lossy(&head).into_owned();
    let mut parts = text.lines().next().unwrap_or("").split(' ');
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    let length = text
        .lines()
        .find_map(|line| line.strip_prefix("Content-Length: "))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    if stream.read_exact(&mut body).is_err() {
        return;
    }
    seen.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push((method.clone(), target.clone()));
    let reply = answer(&method, &target);
    let _ = stream.write_all(&reply);
}

/// A complete HTTP answer with a JSON body and any extra header lines.
fn reply(status: u16, extra: &str, body: &Value) -> Vec<u8> {
    let text = body.to_string();
    format!(
        "HTTP/1.1 {status} Reply\r\n{extra}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        text.len()
    )
    .into_bytes()
}

/// A sync answer whose metadata is `body`, with its ETag.
fn object_reply(body: &Value) -> Vec<u8> {
    reply(
        200,
        "ETag: \"e1\"\r\n",
        &json!({"type": "sync", "status": "Success", "status_code": 200, "operation": "", "error_code": 0, "error": "", "metadata": body}),
    )
}

fn not_found() -> Vec<u8> {
    reply(
        404,
        "",
        &json!({"type": "error", "status": "Not Found", "status_code": 404, "operation": "", "error_code": 404, "error": "not found", "metadata": null}),
    )
}

/// A sync answer with the given status and metadata.
fn sync_reply(status: u16, metadata: Value) -> Vec<u8> {
    reply(
        status,
        "",
        &json!({"type": "sync", "status": "Success", "status_code": 200, "operation": "", "error_code": 0, "error": "", "metadata": metadata}),
    )
}

/// An operation as Incus shows it: its id, its status code and status, and its error text.
fn operation(id: &str, code: u64, status: &str, err: &str) -> Value {
    json!({"id": id, "class": "task", "status": status, "status_code": code, "err": err, "resources": {}, "metadata": null})
}

/// The answer to a wait: the operation, as a sync answer.
fn wait_reply(op: Value) -> Vec<u8> {
    sync_reply(200, op)
}

/// The envelope of a write that starts operation [`OP`].
fn accepted_post() -> Value {
    json!({"type": "async", "status": "Operation created", "status_code": 100, "operation": format!("/1.0/operations/{OP}"), "error_code": 0, "error": "", "metadata": operation(OP, 100, "Running", "")})
}

/// An instance as the canned servers serve it.
fn instance(name: &str, config: Value) -> Value {
    json!({"name": name, "project": "svc", "status": "Stopped", "config": config})
}

/// The config of an instance the entry `toy` owns, with the `volatile.uuid` Incus gives an instance when given one.
fn owned_config(uuid: Option<&str>) -> Value {
    let mut config = json!({ MARK: "toy" });
    if let Some(uuid) = uuid {
        config["volatile.uuid"] = json!(uuid);
    }
    config
}

/// A create's server. The instance `cn*` is missing until its POST, which is answered `post`. Each wait pops the next
/// operation, and the last one repeats. The object reads back as `read_back`.
fn create_server(
    post: Value,
    waits: Vec<Value>,
    read_back: Value,
) -> impl FnMut(&str, &str) -> Vec<u8> + Send + 'static {
    let mut posted = false;
    let mut waits = VecDeque::from(waits);
    move |method, target| {
        if method == "POST" {
            posted = true;
            return reply(202, "", &post);
        }
        if target.contains("/wait") {
            let next = if waits.len() > 1 {
                waits.pop_front()
            } else {
                waits.front().cloned()
            };
            return wait_reply(next.unwrap_or_else(|| operation(OP, 200, "Success", "")));
        }
        if method == "GET" && target.starts_with("/1.0/instances/cn") {
            return if posted {
                object_reply(&read_back)
            } else {
                not_found()
            };
        }
        not_found()
    }
}

/// Creates `cn1` over a server whose waits answer `waits`, and returns the result with the number of waits seen.
fn create_with_waits(waits: Vec<Value>) -> (Result<Owned, IncusError>, usize) {
    let canned = Canned::start(
        "waits",
        create_server(accepted_post(), waits, instance("cn1", owned_config(None))),
    );
    let result = canned
        .writer()
        .create(&target(IncusKind::Instance, "svc", "cn1", None), &marked());
    (result, canned.waits())
}

#[test]
fn each_kind_round_trips_through_create_patch_tombstone_and_delete() {
    let (_dir, fake) = start("roundtrip");
    let writer = writer(&fake, "toy");
    // The forward's network is made first and kept, so the forward's parent exists when its turn comes.
    writer
        .create(&target(IncusKind::Network, "svc", "fnet1", None), &marked())
        .expect("the forward's network");
    let cases = [
        (IncusKind::Project, "kproj1", None),
        (IncusKind::Instance, "ki1", None),
        (IncusKind::Network, "knet1", None),
        (IncusKind::Acl, "kacl1", None),
        (IncusKind::Profile, "kprof1", None),
        (IncusKind::Zone, "kzone1", None),
        (IncusKind::Volume, "kvol1", Some("default")),
        (IncusKind::Bucket, "kbkt1", Some("default")),
        (IncusKind::Forward, "192.0.2.10", Some("fnet1")),
    ];
    for (kind, name, parent) in cases {
        let project = if kind == IncusKind::Project {
            name
        } else {
            "svc"
        };
        let target = target(kind, project, name, parent);
        let created = writer.create(&target, &marked()).expect("create");
        assert_eq!(created.entry(), &EntryId("toy".to_string()), "{kind:?}");
        assert_eq!(
            created.uuid().is_some(),
            has_uuid(kind),
            "{kind:?} has a uuid exactly when Incus gives one"
        );
        if has_uuid(kind) {
            assert!(
                created.config().contains_key("volatile.uuid"),
                "{kind:?} carries volatile.uuid"
            );
        }

        let set = writer
            .patch(&created, &changes(&[("user.note", Some("first"))]))
            .expect("patch");
        assert_eq!(
            set.config().get("user.note").map(String::as_str),
            Some("first")
        );
        let cleared = writer
            .patch(&set, &changes(&[("user.note", None)]))
            .expect("remove a key");
        assert!(!cleared.config().contains_key("user.note"), "{kind:?}");

        let retiring = writer.tombstone(&cleared, &plan()).expect("tombstone");
        assert_eq!(retiring.tombstone(), Some(plan().to_hex().as_str()));
        writer.delete(&retiring).expect("delete");
        let gone = writer.get_etag(&target).expect_err("the object is gone");
        assert_eq!(status_of(&gone), Some(404), "{kind:?}");
    }

    // Five writes per case, and the one create of the forward's network.
    let writes = fake.writes();
    assert_eq!(writes.len(), cases.len() * 5 + 1, "{writes:?}");
    assert!(
        writes
            .iter()
            .all(|w| matches!(w.method.as_str(), "POST" | "PATCH" | "DELETE")),
        "only POST, PATCH and DELETE are sent: {writes:?}"
    );
    // Instances and volumes create and delete through an operation (202). Every other write, and every PATCH, is sync.
    for write in &writes {
        let accepted_late = (write.path.starts_with("/1.0/instances")
            || write.path.contains("/volumes/custom"))
            && write.method != "PATCH";
        let expected = if accepted_late {
            202
        } else if write.method == "POST" {
            201
        } else {
            200
        };
        assert_eq!(write.status, expected, "{write:?}");
    }
    let accepted = writes.iter().filter(|w| w.status == 202).count();
    let waits = fake
        .requests()
        .iter()
        .filter(|r| {
            r.method == "GET" && r.path.starts_with("/1.0/operations/") && r.path.contains("/wait")
        })
        .count();
    assert_eq!(
        waits, accepted,
        "every accepted write, and only those, is waited on"
    );
}

#[test]
fn a_sync_answer_is_done_at_once_and_an_async_one_is_waited_on() {
    let (_dir, fake) = start("sync-async");
    let writer = writer(&fake, "toy");
    writer
        .create(&project_target("kproj1"), &marked())
        .expect("a project is answered sync");
    assert!(
        fake.requests().iter().all(|r| !r.path.contains("/wait")),
        "a sync write has no operation to wait on"
    );
    writer
        .create(&target(IncusKind::Instance, "svc", "sa1", None), &marked())
        .expect("an instance is answered async");
    let waits: Vec<_> = fake
        .requests()
        .into_iter()
        .filter(|r| r.path.contains("/wait"))
        .collect();
    assert_eq!(waits.len(), 1, "the async create is waited on once");
    assert!(
        waits[0].path.contains("?timeout="),
        "the wait carries its timeout: {}",
        waits[0].path
    );
}

#[test]
fn an_answer_with_an_empty_or_absent_operation_is_done_without_a_wait() {
    let replies = [
        json!({"type": "async", "status_code": 100, "operation": "", "error_code": 0, "error": "", "metadata": null}),
        json!({"type": "async", "status_code": 100, "error_code": 0, "error": "", "metadata": null}),
    ];
    for (n, post) in replies.into_iter().enumerate() {
        let canned = Canned::start(
            "empty-operation",
            create_server(post, vec![], instance("cn1", owned_config(None))),
        );
        let created = canned
            .writer()
            .create(&target(IncusKind::Instance, "svc", "cn1", None), &marked())
            .expect("an answer that names no operation is done");
        assert_eq!(created.entry(), &EntryId("toy".to_string()), "case {n}");
        assert_eq!(canned.waits(), 0, "case {n}: nothing was waited on");
    }
}

#[test]
fn the_operation_wait_follows_running_and_accepts_only_status_200_success() {
    let running = operation(OP, 103, "Running", "");
    let success = operation(OP, 200, "Success", "");

    let (result, waits) = create_with_waits(vec![running.clone(), running.clone(), success]);
    assert!(result.is_ok(), "running, then success: {result:?}");
    assert_eq!(waits, 3, "each running answer is waited on again");

    let (result, _) = create_with_waits(vec![running]);
    assert!(
        matches!(result, Err(IncusError::Timeout)),
        "an operation that runs past the deadline times out: {result:?}"
    );

    let (result, _) = create_with_waits(vec![operation(OP, 400, "Failure", "boom")]);
    assert_eq!(
        result.err(),
        Some(IncusError::Status {
            code: 400,
            message: "boom".to_string()
        })
    );

    let (result, _) = create_with_waits(vec![operation(OP, 401, "Cancelled", "stopped")]);
    assert_eq!(
        result.err(),
        Some(IncusError::Status {
            code: 401,
            message: "stopped".to_string()
        })
    );

    for odd in [
        operation(OP, 200, "Running", ""),
        operation(OP, 201, "Created", ""),
        operation(OP, 300, "Multiple", ""),
    ] {
        let (result, _) = create_with_waits(vec![odd.clone()]);
        assert!(
            matches!(result, Err(IncusError::Decode(_))),
            "{odd}: {result:?}"
        );
    }

    let other = "00000000-0000-4000-8000-0000000000bb";
    let (result, _) = create_with_waits(vec![operation(other, 200, "Success", "")]);
    assert!(
        matches!(result, Err(IncusError::Decode(_))),
        "an answer for another operation is not this one's: {result:?}"
    );
}

#[test]
fn a_read_is_owned_only_for_this_entrys_own_kernel_object() {
    let (_dir, fake) = start("reads");
    let toy = writer(&fake, "toy");
    let other = writer(&fake, "other");
    let before = write_count(&fake);

    let k1 = target(IncusKind::Instance, "svc", "k1", None);
    let owned = toy.read_owned(&k1).expect("the entry reads its own object");
    assert_eq!(owned.config().get(MARK).map(String::as_str), Some("toy"));
    assert!(
        other.read_owned(&k1).is_err(),
        "another entry may not read it as its own"
    );

    for (project, name) in [
        ("svc", "db1"),
        ("droid", "phone1"),
        ("droid", "desk1"),
        ("default", "guest1"),
        ("svc", "x1"),
    ] {
        let refused = toy
            .read_owned(&target(IncusKind::Instance, project, name, None))
            .expect_err("a droidd, foreign or contested object is not owned");
        assert!(
            matches!(refused, IncusError::Refused(_)),
            "{name}: {refused:?}"
        );
    }
    assert_eq!(write_count(&fake), before, "a read sends no write");
}

#[test]
fn a_target_outside_the_kernel_family_is_refused_before_any_request() {
    let (_dir, fake) = start("family");
    let writer = writer(&fake, "toy");
    let requests = fake.requests().len();
    let before = write_count(&fake);

    for (project, name) in [
        ("default", "kd1"),
        ("bench", "kb1"),
        ("droid", "kd2"),
        ("svc-other", "kx1"),
    ] {
        let refused = writer
            .create(&target(IncusKind::Instance, project, name, None), &marked())
            .expect_err("a project outside the family");
        assert!(
            matches!(refused, IncusError::Refused(_)),
            "{project}: {refused:?}"
        );
    }
    for name in ["default", "bench"] {
        let refused = writer
            .create(&project_target(name), &marked())
            .expect_err("a reserved project");
        assert!(
            matches!(refused, IncusError::Refused(_)),
            "{name}: {refused:?}"
        );
    }
    let read = writer.read_owned(&target(IncusKind::Instance, "default", "guest1", None));
    assert!(matches!(read, Err(IncusError::Refused(_))), "{read:?}");

    assert_eq!(write_count(&fake), before, "no write reached the fake");
    assert_eq!(
        fake.requests().len(),
        requests,
        "not even a read was sent for a target outside the family"
    );
}

#[test]
fn the_family_is_refused_at_construction_when_it_names_a_reserved_or_droidd_project() {
    assert!(ProjectFamily::new(["bench"], NONE).is_err());
    assert!(ProjectFamily::new(["default"], NONE).is_err());
    assert!(ProjectFamily::new(["svc", "default"], NONE).is_err());
    assert!(ProjectFamily::new(["droid"], ["droid"]).is_err());
    assert!(
        ProjectFamily::new(["s vc"], NONE).is_err(),
        "a name that is not a plain segment"
    );
    let family = ProjectFamily::new(["svc"], ["droid"]).expect("a family");
    assert!(family.allows("svc"));
    assert!(!family.allows("droid"));
    assert!(!family.allows("default"));
    assert!(!family.allows("bench"));
}

#[test]
fn a_retained_object_is_never_owned_by_its_entry() {
    let (_dir, fake) = start("retained");
    let mut retained = BTreeMap::new();
    retained.insert(
        EntryId("toy".to_string()),
        (PluginId("arca.toy".to_string()), plan()),
    );
    let writer = UnixIncusWriter::new(
        fake.socket().to_path_buf(),
        EntryId("toy".to_string()),
        retained,
        family(),
    );
    let before = write_count(&fake);

    let k1 = target(IncusKind::Instance, "svc", "k1", None);
    assert!(matches!(
        writer.read_owned(&k1),
        Err(IncusError::Refused(_))
    ));
    let create = writer.create(&target(IncusKind::Instance, "svc", "kr1", None), &marked());
    assert!(matches!(create, Err(IncusError::Refused(_))));
    assert_eq!(write_count(&fake), before);
}

#[test]
fn a_create_is_refused_unless_its_config_classes_as_the_entrys_own() {
    let (_dir, fake) = start("create-refusals");
    let writer = writer(&fake, "toy");
    let before = write_count(&fake);
    let bodies = [
        json!({}),
        json!({ "config": { "user.note": "no mark" } }),
        json!({ "config": { MARK: "toy", "user.droid.lease": "1" } }),
        json!({ "config": { MARK: "toy", "user.tactu.desktop": "1" } }),
        json!({ "config": { MARK: "other" } }),
        json!({ "config": { MARK: "toy", RETIRING: "x" } }),
        json!({ "config": { MARK: 7 } }),
        json!(["not", "an", "object"]),
    ];
    for body in bodies {
        let refused = writer
            .create(&target(IncusKind::Instance, "svc", "bad1", None), &body)
            .expect_err("a create that would not class as the entry's own");
        assert!(
            matches!(refused, IncusError::Refused(_)),
            "{body}: {refused:?}"
        );
    }
    assert_eq!(
        write_count(&fake),
        before,
        "no refused create reached the fake"
    );
    assert!(matches!(
        writer.get_etag(&target(IncusKind::Instance, "svc", "bad1", None)),
        Err(IncusError::Status { code: 404, .. })
    ));
}

#[test]
fn a_create_takes_only_an_image_source_and_sends_nothing_else() {
    let (_dir, fake) = start("sources");
    let writer = writer(&fake, "toy");
    let before = write_count(&fake);
    let refused_sources = [
        json!({"type": "copy", "source": "svc/k1"}),
        json!({"type": "migration", "mode": "pull"}),
        json!({"type": "image", "alias": "base", "refresh": true}),
        json!({"type": "image", "alias": "base", "server": "https://192.0.2.1:8443", "protocol": "simplestreams"}),
        json!({"type": "image", "alias": "base", "remote": "elsewhere"}),
        json!({"type": "none"}),
        json!({"type": "image", "alias": 7}),
        json!("image"),
    ];
    for (n, source) in refused_sources.into_iter().enumerate() {
        let mut body = marked();
        body["source"] = source.clone();
        let name = format!("src{n}");
        let refused = writer
            .create(&target(IncusKind::Instance, "svc", &name, None), &body)
            .expect_err("a source that is not an image");
        assert!(
            matches!(refused, IncusError::Refused(_)),
            "{source}: {refused:?}"
        );
    }
    assert_eq!(
        write_count(&fake),
        before,
        "no refused source reached the fake"
    );

    let mut body = marked();
    body["source"] = json!({"type": "image", "alias": "base", "fingerprint": "abc123", "properties": {"os": "linux"}});
    writer
        .create(&target(IncusKind::Instance, "svc", "img1", None), &body)
        .expect("an image source is sent");
    assert_eq!(write_count(&fake), before + 1);
}

#[test]
fn a_create_key_that_is_not_plain_ascii_is_refused_before_any_request() {
    let (_dir, fake) = start("key-folding");
    let writer = writer(&fake, "toy");
    let before_writes = write_count(&fake);
    let before_requests = fake.requests().len();
    // Go's decoder binds a key whatever its case, and folds Unicode too, so each of these reaches Incus as `source`. The
    // exact-key check reads only `source`, so it sees none of them.
    for (n, key) in ["Source", "SOURCE", "\u{017f}ource"]
        .into_iter()
        .enumerate()
    {
        let mut body = marked();
        body[key] = json!({"type": "copy", "source": "svc/k1"});
        let refused = writer
            .create(
                &target(IncusKind::Instance, "svc", &format!("fold{n}"), None),
                &body,
            )
            .expect_err("a body key that is not plain ASCII");
        assert!(
            matches!(refused, IncusError::Refused(_)),
            "{key}: {refused:?}"
        );
    }
    assert_eq!(
        write_count(&fake),
        before_writes,
        "no refused create reached the fake"
    );
    assert_eq!(
        fake.requests().len(),
        before_requests,
        "no refused create sent even a read"
    );
}

#[test]
fn a_config_key_that_differs_only_in_case_is_refused_before_any_request() {
    let (_dir, fake) = start("config-folding");
    let writer = writer(&fake, "toy");
    let before_writes = write_count(&fake);
    let before_requests = fake.requests().len();
    // Both keys bind to the one config field, and the later one wins. The check reads only `config`, which is marked.
    for (n, key) in ["Config", "CONFIG"].into_iter().enumerate() {
        let mut body = marked();
        body[key] = json!({"user.droid.lease": "1"});
        let refused = writer
            .create(
                &target(IncusKind::Instance, "svc", &format!("cfg{n}"), None),
                &body,
            )
            .expect_err("a second config key that differs in case");
        assert!(
            matches!(refused, IncusError::Refused(_)),
            "{key}: {refused:?}"
        );
    }
    assert_eq!(write_count(&fake), before_writes);
    assert_eq!(fake.requests().len(), before_requests);
}

#[test]
fn a_key_that_is_not_plain_ascii_is_refused_at_any_depth_and_inside_source() {
    let (_dir, fake) = start("key-depth");
    let writer = writer(&fake, "toy");
    let before_writes = write_count(&fake);
    let before_requests = fake.requests().len();
    let bodies = [
        json!({"config": {MARK: "toy"}, "devices": {"root": {"Pool": "default"}}}),
        json!({"config": {MARK: "toy"}, "tags": [{"Bad": 1}]}),
        json!({"config": {MARK: "toy"}, "source": {"type": "image", "alias": "base", "properties": {"OS": "linux"}}}),
        json!({"config": {MARK: "toy"}, "": 1}),
    ];
    for (n, body) in bodies.into_iter().enumerate() {
        let refused = writer
            .create(
                &target(IncusKind::Instance, "svc", &format!("deep{n}"), None),
                &body,
            )
            .expect_err("a key that is not plain ASCII at some depth");
        assert!(
            matches!(refused, IncusError::Refused(_)),
            "{body}: {refused:?}"
        );
    }
    assert_eq!(write_count(&fake), before_writes);
    assert_eq!(fake.requests().len(), before_requests);
}

#[test]
fn a_volatile_config_key_is_refused_in_a_create_and_in_a_patch() {
    let (_dir, fake) = start("volatile");
    let writer = writer(&fake, "toy");
    let before_writes = write_count(&fake);
    let before_requests = fake.requests().len();
    let mut body = marked();
    body["config"]["volatile.uuid"] = json!(OTHER_UUID);
    let refused = writer
        .create(&target(IncusKind::Instance, "svc", "vol1", None), &body)
        .expect_err("a create that sets volatile.uuid");
    assert!(matches!(refused, IncusError::Refused(_)), "{refused:?}");
    assert_eq!(write_count(&fake), before_writes);
    assert_eq!(fake.requests().len(), before_requests);

    let owned = writer
        .create(&target(IncusKind::Instance, "svc", "vol2", None), &marked())
        .expect("create");
    let before_writes = write_count(&fake);
    for change in [
        changes(&[("volatile.uuid", Some(OTHER_UUID))]),
        changes(&[("volatile.uuid", None)]),
        changes(&[("volatile.last_state.power", Some("RUNNING"))]),
        changes(&[("volatile", Some("x"))]),
    ] {
        let refused = writer
            .patch(&owned, &change)
            .expect_err("a patch that names a volatile key");
        assert!(
            matches!(refused, IncusError::Refused(_)),
            "{change:?}: {refused:?}"
        );
    }
    assert_eq!(
        write_count(&fake),
        before_writes,
        "no refused patch reached the fake"
    );
}

#[test]
fn a_patch_key_that_is_not_plain_ascii_is_refused_before_any_request() {
    let (_dir, fake) = start("patch-key");
    let writer = writer(&fake, "toy");
    let owned = writer
        .create(&target(IncusKind::Instance, "svc", "pk1", None), &marked())
        .expect("create");
    let before_writes = write_count(&fake);
    let before_requests = fake.requests().len();
    for change in [
        changes(&[("User.Note", Some("x"))]),
        changes(&[("user.n\u{00f6}te", Some("x"))]),
    ] {
        let refused = writer
            .patch(&owned, &change)
            .expect_err("a patch key that is not plain ASCII");
        assert!(
            matches!(refused, IncusError::Refused(_)),
            "{change:?}: {refused:?}"
        );
    }
    assert_eq!(write_count(&fake), before_writes);
    assert_eq!(fake.requests().len(), before_requests);
}

#[test]
fn a_create_whose_name_is_taken_is_refused_and_nothing_is_written() {
    let (_dir, fake) = start("taken");
    let writer = writer(&fake, "toy");
    let before = write_count(&fake);
    for (project, name) in [("svc", "k1"), ("svc", "db1")] {
        let refused = writer
            .create(&target(IncusKind::Instance, project, name, None), &marked())
            .expect_err("the name is taken");
        assert!(
            matches!(refused, IncusError::Refused(_)),
            "{name}: {refused:?}"
        );
    }
    assert_eq!(write_count(&fake), before, "a taken name sends no POST");
    assert!(
        fake.requests()
            .iter()
            .any(|r| r.method == "GET" && r.path.starts_with("/1.0/instances/k1")),
        "the name was read before the create"
    );
}

#[test]
fn a_patch_that_would_leave_the_object_owned_by_another_owner_is_refused() {
    let (_dir, fake) = start("patch-refusals");
    let writer = writer(&fake, "toy");
    let owned = writer
        .create(&target(IncusKind::Instance, "svc", "kp1", None), &marked())
        .expect("create");
    let before = write_count(&fake);
    for change in [
        changes(&[("user.droid.lease", Some("1"))]),
        changes(&[(MARK, None)]),
        changes(&[(MARK, Some("other"))]),
        changes(&[(RETIRING, Some("x"))]),
        changes(&[(RETIRING, None)]),
    ] {
        let refused = writer.patch(&owned, &change).expect_err("a refused change");
        assert!(
            matches!(refused, IncusError::Refused(_)),
            "{change:?}: {refused:?}"
        );
    }
    assert_eq!(
        write_count(&fake),
        before,
        "no refused patch reached the fake"
    );
}

#[test]
fn a_tombstone_needs_a_uuid_where_incus_gives_one_and_a_delete_needs_its_tombstone() {
    let (_dir, fake) = start("delete-refusals");
    let writer = writer(&fake, "toy");

    // The fixture's kernel instance has no volatile.uuid, so it can be neither retired nor deleted.
    let k1 = writer
        .read_owned(&target(IncusKind::Instance, "svc", "k1", None))
        .expect("read");
    let before = write_count(&fake);
    assert!(matches!(
        writer.tombstone(&k1, &plan()),
        Err(IncusError::Refused(_))
    ));
    assert!(matches!(writer.delete(&k1), Err(IncusError::Refused(_))));
    assert_eq!(
        write_count(&fake),
        before,
        "a refused tombstone or delete sent nothing"
    );

    // A created object has a uuid, but it has no tombstone yet, so a delete is still refused.
    let created = writer
        .create(&target(IncusKind::Instance, "svc", "kd1", None), &marked())
        .expect("create");
    let before = write_count(&fake);
    assert!(matches!(
        writer.delete(&created),
        Err(IncusError::Refused(_))
    ));
    assert_eq!(
        write_count(&fake),
        before,
        "a delete without its tombstone sent nothing"
    );
}

#[test]
fn a_target_that_names_a_uuid_reads_only_the_object_with_that_uuid() {
    let (_dir, fake) = start("uuid-target");
    let writer = writer(&fake, "toy");
    let created = writer
        .create(&target(IncusKind::Instance, "svc", "m1", None), &marked())
        .expect("create");
    let uuid = created.uuid().expect("an instance has a uuid").to_string();

    let mut named = target(IncusKind::Instance, "svc", "m1", None);
    named.uuid = Some(uuid.clone());
    writer.read_owned(&named).expect("the named uuid reads");
    named.uuid = Some(OTHER_UUID.to_string());
    assert!(
        matches!(writer.read_owned(&named), Err(IncusError::Refused(_))),
        "another uuid is refused"
    );

    // A network has no uuid, so a target that names one never reads as that network.
    writer
        .create(&target(IncusKind::Network, "svc", "mn1", None), &marked())
        .expect("a network");
    let mut named_network = target(IncusKind::Network, "svc", "mn1", None);
    named_network.uuid = Some(uuid);
    assert!(matches!(
        writer.read_owned(&named_network),
        Err(IncusError::Refused(_))
    ));

    // A create names no uuid yet, so a target that does is refused before anything is sent.
    let before = write_count(&fake);
    let mut creating = target(IncusKind::Instance, "svc", "m2", None);
    creating.uuid = Some(OTHER_UUID.to_string());
    assert!(matches!(
        writer.create(&creating, &marked()),
        Err(IncusError::Refused(_))
    ));
    assert_eq!(write_count(&fake), before);
}

#[test]
fn a_tombstone_that_reads_back_without_the_tombstone_is_applied_unconfirmed() {
    let canned = Canned::start("tombstone-readback", |method, target| {
        if method == "GET" && target.starts_with("/1.0/instances/tb1") {
            return object_reply(&instance("tb1", owned_config(Some(UUID))));
        }
        if method == "PATCH" {
            return sync_reply(200, Value::Null);
        }
        not_found()
    });
    let writer = canned.writer();
    let owned = writer
        .read_owned(&target(IncusKind::Instance, "svc", "tb1", None))
        .expect("read");
    let result = writer.tombstone(&owned, &plan());
    assert!(
        matches!(owner_of(&result), Some(OwnerClass::Kernel { .. })),
        "the tombstone landed, and its read-back shows none: {result:?}"
    );
    assert_eq!(canned.count("PATCH"), 1, "the tombstone was sent once");
}

#[test]
fn a_tombstone_that_reads_back_on_another_uuid_is_applied_unconfirmed() {
    let mut patched = false;
    let canned = Canned::start("uuid-readback", move |method, target| {
        if method == "PATCH" {
            patched = true;
            return sync_reply(200, Value::Null);
        }
        if method == "GET" && target.starts_with("/1.0/instances/ub1") {
            let mut config = owned_config(Some(if patched { OTHER_UUID } else { UUID }));
            if patched {
                config[RETIRING] = json!(plan().to_hex());
            }
            return object_reply(&instance("ub1", config));
        }
        not_found()
    });
    let writer = canned.writer();
    let owned = writer
        .read_owned(&target(IncusKind::Instance, "svc", "ub1", None))
        .expect("read");
    let result = writer.tombstone(&owned, &plan());
    assert!(
        matches!(owner_of(&result), Some(OwnerClass::Kernel { .. })),
        "the read-back names another uuid: {result:?}"
    );
}

#[test]
fn a_patch_that_reads_back_without_its_change_is_applied_unconfirmed() {
    let canned = Canned::start("unconfirmed-patch", |method, target| {
        if method == "GET" && target.starts_with("/1.0/instances/pc1") {
            return object_reply(&instance("pc1", owned_config(None)));
        }
        if method == "PATCH" {
            return sync_reply(200, Value::Null);
        }
        not_found()
    });
    let writer = canned.writer();
    let owned = writer
        .read_owned(&target(IncusKind::Instance, "svc", "pc1", None))
        .expect("read");
    let result = writer.patch(&owned, &changes(&[("user.note", Some("first"))]));
    assert!(
        matches!(owner_of(&result), Some(OwnerClass::Kernel { .. })),
        "the change landed, and the read-back does not show it: {result:?}"
    );
}

#[test]
fn a_create_that_lands_and_reads_back_foreign_is_applied_unconfirmed() {
    let mut posted = false;
    let canned = Canned::start("unconfirmed-create", move |method, target| {
        if method == "POST" {
            posted = true;
            return sync_reply(201, Value::Null);
        }
        if method == "GET" && target.starts_with("/1.0/projects/kcan") {
            return if posted {
                object_reply(&json!({"name": "kcan", "config": {}}))
            } else {
                not_found()
            };
        }
        not_found()
    });
    let result = canned.writer().create(&project_target("kcan"), &marked());
    match result {
        Err(IncusError::AppliedUnconfirmed(unconfirmed)) => {
            assert_eq!(unconfirmed.target.name, "kcan");
            assert_eq!(unconfirmed.owner, Some(OwnerClass::Foreign));
            assert!(!unconfirmed.reason.is_empty());
        }
        other => panic!("expected AppliedUnconfirmed: {other:?}"),
    }
    assert_eq!(canned.count("POST"), 1, "the create was sent once");
}

#[test]
fn the_refused_and_unconfirmed_errors_say_whether_a_write_was_sent() {
    let refused = IncusError::Refused("the name is taken".to_string()).to_string();
    assert!(refused.contains("before any write was sent"), "{refused}");
    let landed = IncusError::AppliedUnconfirmed(Box::new(Unconfirmed {
        target: ObjectRef {
            kind: IncusKind::Instance,
            project: "svc".to_string(),
            parent: None,
            name: "x".to_string(),
            uuid: None,
        },
        uuid: None,
        owner: None,
        reason: "the object is not this entry's".to_string(),
    }))
    .to_string();
    assert!(landed.contains("landed"), "{landed}");
}

/// A complete HTTP answer with an error envelope of `status` and the message `text`.
fn error_reply(status: u16, text: &str) -> Vec<u8> {
    reply(
        status,
        "",
        &json!({"type": "error", "status": "Failure", "status_code": status, "operation": "", "error_code": status, "error": text, "metadata": null}),
    )
}

/// Runs a create against `canned`, which accepts its POST, and checks that the POST was sent and that the error is not
/// `Refused`. Refused means that no write was sent, so no error after a sent POST may be it.
fn error_after_sent_post(canned: &Canned, name: &str) -> IncusError {
    let error = canned
        .writer()
        .create(&target(IncusKind::Instance, "svc", name, None), &marked())
        .expect_err("a create that fails after its POST was sent");
    assert_eq!(canned.count("POST"), 1, "the POST was sent");
    assert!(!matches!(error, IncusError::Refused(_)), "{error:?}");
    error
}

#[test]
fn every_error_after_a_sent_write_is_something_other_than_refused() {
    let failed_post = Canned::start("sent-failed", |method, _| {
        if method == "POST" {
            return error_reply(500, "the backend failed");
        }
        not_found()
    });
    let stuck_wait = Canned::start("sent-stuck", |method, target| {
        if method == "POST" {
            return reply(202, "", &accepted_post());
        }
        if target.contains("/wait") {
            return wait_reply(operation(OP, 103, "Running", ""));
        }
        not_found()
    });
    let unreadable_wait = Canned::start("sent-decode", |method, target| {
        if method == "POST" {
            return reply(202, "", &accepted_post());
        }
        if target.contains("/wait") {
            return sync_reply(200, Value::Null);
        }
        not_found()
    });
    let mut posted = false;
    let unread_back = Canned::start("sent-readback", move |method, target| {
        if method == "POST" {
            posted = true;
            return reply(202, "", &accepted_post());
        }
        if target.contains("/wait") {
            return wait_reply(operation(OP, 200, "Success", ""));
        }
        if method == "GET" && posted && target.starts_with("/1.0/instances/rb1") {
            return error_reply(500, "the read-back failed");
        }
        not_found()
    });

    let status = error_after_sent_post(&failed_post, "pf1");
    assert_eq!(status_of(&status), Some(500), "{status:?}");
    assert_eq!(
        error_after_sent_post(&stuck_wait, "sw1"),
        IncusError::Timeout
    );
    let decode = error_after_sent_post(&unreadable_wait, "uw1");
    assert!(matches!(decode, IncusError::Decode(_)), "{decode:?}");
    let landed = error_after_sent_post(&unread_back, "rb1");
    assert!(
        matches!(landed, IncusError::AppliedUnconfirmed(_)),
        "{landed:?}"
    );
}

#[test]
fn a_delete_is_refused_when_the_tombstone_was_removed_after_it_was_set() {
    let (_dir, fake) = start("tombstone-gone");
    let writer = writer(&fake, "toy");
    let created = writer
        .create(&target(IncusKind::Instance, "svc", "chg1", None), &marked())
        .expect("create");
    let retiring = writer.tombstone(&created, &plan()).expect("tombstone");

    let path = "/1.0/instances/chg1?project=svc";
    let reply = raw_patch(
        fake.socket(),
        path,
        None,
        &json!({ "config": { RETIRING: "" } }),
    );
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");

    let refused = writer.delete(&retiring).expect_err("the tombstone is gone");
    assert!(matches!(refused, IncusError::Refused(_)), "{refused:?}");
    assert!(
        !fake.requests().iter().any(|r| r.method == "DELETE"),
        "no delete was sent"
    );
}

#[test]
fn a_stale_etag_is_412_and_changes_nothing() {
    let (_dir, fake) = start("stale");
    let writer = writer(&fake, "toy");
    let target = target(IncusKind::Instance, "svc", "st1", None);
    let first = writer.create(&target, &marked()).expect("create");
    let second = writer
        .patch(&first, &changes(&[("user.note", Some("first"))]))
        .expect("the current read patches");
    assert_ne!(first.etag(), second.etag(), "a change moves the ETag");

    let stale = writer
        .patch(&first, &changes(&[("user.note", Some("second"))]))
        .expect_err("the old ETag is stale");
    assert_eq!(status_of(&stale), Some(412), "{stale:?}");

    let (metadata, _) = writer.get_etag(&target).expect("read");
    assert_eq!(
        metadata["config"]["user.note"], "first",
        "the 412 changed nothing"
    );
}

#[test]
fn a_tombstone_with_a_stale_etag_is_412_and_sets_nothing() {
    let (_dir, fake) = start("stale-tombstone");
    let writer = writer(&fake, "toy");
    let target = target(IncusKind::Instance, "svc", "st2", None);
    let first = writer.create(&target, &marked()).expect("create");
    let raced = raw_patch(
        fake.socket(),
        "/1.0/instances/st2?project=svc",
        None,
        &json!({ "config": { "user.other": "1" } }),
    );
    assert!(raced.starts_with("HTTP/1.1 200"), "{raced}");

    let stale = writer
        .tombstone(&first, &plan())
        .expect_err("the ETag moved");
    assert_eq!(status_of(&stale), Some(412), "{stale:?}");
    let (metadata, _) = writer.get_etag(&target).expect("read");
    assert!(
        metadata["config"].get(RETIRING).is_none(),
        "no tombstone was set"
    );
}

#[test]
fn a_failed_operation_is_a_status_error_and_the_object_stays() {
    let (_dir, fake) = start("operation");
    let writer = writer(&fake, "toy");
    let mut body = marked();
    body["status"] = json!("Running");
    let running = target(IncusKind::Instance, "svc", "run1", None);
    let owned = writer.create(&running, &body).expect("create");
    let retiring = writer.tombstone(&owned, &plan()).expect("tombstone");

    let failed = writer
        .delete(&retiring)
        .expect_err("a running instance does not delete");
    assert_eq!(
        failed,
        IncusError::Status {
            code: 400,
            message: "The instance is running".to_string()
        }
    );
    assert!(writer.get_etag(&running).is_ok(), "the instance stays");
}

#[test]
fn a_non_empty_project_is_not_deleted() {
    let (_dir, fake) = start("project");
    let writer = writer(&fake, "toy");
    let project = project_target("kfull");
    let owned = writer
        .create(&project, &marked())
        .expect("create the project");
    writer
        .create(
            &target(IncusKind::Instance, "kfull", "inside", None),
            &marked(),
        )
        .expect("create an instance inside it");
    let retiring = writer
        .tombstone(&owned, &plan())
        .expect("tombstone the project");

    let refused = writer
        .delete(&retiring)
        .expect_err("the project holds an instance");
    assert_eq!(status_of(&refused), Some(400), "{refused:?}");
    assert!(writer.get_etag(&project).is_ok(), "the project stays");
}

#[test]
fn an_object_name_that_is_not_a_plain_path_segment_is_refused_before_any_write() {
    let (_dir, fake) = start("names");
    let writer = writer(&fake, "toy");
    let before = write_count(&fake);
    for bad in [
        target(IncusKind::Instance, "svc", "a/b", None),
        target(IncusKind::Instance, "svc", "a?b", None),
        target(IncusKind::Instance, "svc", "..", None),
        target(IncusKind::Instance, "svc", "", None),
        target(IncusKind::Instance, "s vc", "ok", None),
        target(IncusKind::Volume, "svc", "v1", None),
        target(IncusKind::Instance, "svc", "ok", Some("default")),
        target(IncusKind::Project, "kone", "ktwo", None),
    ] {
        let refused = writer.create(&bad, &marked()).expect_err("a bad target");
        assert!(
            matches!(refused, IncusError::Refused(_)),
            "{bad:?}: {refused:?}"
        );
    }
    assert_eq!(write_count(&fake), before, "no bad target reached the fake");
}

#[test]
fn a_write_that_names_another_entrys_read_is_refused() {
    let (_dir, fake) = start("cross-entry");
    let toy = writer(&fake, "toy");
    let other = writer(&fake, "other");
    let owned = toy
        .create(&target(IncusKind::Instance, "svc", "ce1", None), &marked())
        .expect("create");
    let before = write_count(&fake);
    let crossed = other
        .patch(&owned, &changes(&[("user.note", Some("x"))]))
        .expect_err("another entry may not use this read");
    assert!(matches!(crossed, IncusError::Refused(_)));
    assert!(matches!(other.delete(&owned), Err(IncusError::Refused(_))));
    assert_eq!(write_count(&fake), before);
}

#[test]
fn the_write_log_keeps_config_key_names_and_never_their_values() {
    let (_dir, fake) = start("log");
    let writer = writer(&fake, "toy");
    let mut body = marked();
    body["config"]["user.note"] = json!("SECRET-7f3c-value");
    writer
        .create(&target(IncusKind::Instance, "svc", "log1", None), &body)
        .expect("create");

    let writes = fake.writes();
    let post = writes.iter().find(|w| w.method == "POST").expect("a POST");
    assert!(post.config_keys.contains(&"user.note".to_string()));
    assert!(post.config_keys.contains(&MARK.to_string()));
    assert!(!format!("{writes:?}").contains("SECRET-7f3c-value"));
    assert!(!format!("{:?}", fake.requests()).contains("SECRET-7f3c-value"));
}

#[test]
fn a_create_that_the_switch_rejects_is_refused_by_the_fake_and_nothing_is_stored() {
    let (_dir, fake) = start("reject");
    fake.set_reject_config_on_post("/1.0/networks");
    let writer = writer(&fake, "toy");
    let target = target(IncusKind::Network, "svc", "rn1", None);
    let rejected = writer
        .create(&target, &marked())
        .expect_err("the fake rejects config");
    assert_eq!(
        rejected,
        IncusError::Status {
            code: 400,
            message: "config is not accepted on create".to_string()
        }
    );
    assert!(matches!(
        writer.get_etag(&target),
        Err(IncusError::Status { code: 404, .. })
    ));
    let post = fake
        .writes()
        .into_iter()
        .find(|w| w.method == "POST")
        .expect("the POST is still recorded");
    assert_eq!(post.status, 400);
    assert_eq!(post.config_keys, vec![MARK.to_string()]);
}

#[test]
fn a_patch_takes_the_etag_from_the_read_it_was_checked_against() {
    let (_dir, fake) = start("etag-source");
    let writer = writer(&fake, "toy");
    let created: Owned = writer
        .create(&target(IncusKind::Instance, "svc", "es1", None), &marked())
        .expect("create");
    let (_, etag) = writer
        .get_etag(created.target())
        .expect("the same object's ETag");
    assert_eq!(etag, created.etag(), "the read and the owned value agree");
    let patched = writer
        .patch(&created, &changes(&[("user.note", Some("ok"))]))
        .expect("patch");
    assert_eq!(
        patched.config().get("user.note").map(String::as_str),
        Some("ok")
    );
}

#[test]
fn the_ownership_document_exists_and_every_rule_cites_its_source() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/OWNERSHIP.md");
    let text = fs::read_to_string(path).expect("docs/OWNERSHIP.md exists");
    let rules = text.matches("**Rule ").count();
    let sources = text
        .lines()
        .filter(|line| line.trim_start().starts_with("Source:"))
        .count();
    assert!(rules >= 12, "the document states its rules: {rules}");
    assert_eq!(rules, sources, "every rule is followed by its Source line");
}

#[test]
fn the_ownership_document_carries_no_plan_cross_reference() {
    // A plan's cross-references (a letter and a number, such as a review item) live in the plan's own record, not here.
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/OWNERSHIP.md");
    let text = fs::read_to_string(path).expect("docs/OWNERSHIP.md exists");
    for word in text.split(|c: char| !c.is_ascii_alphanumeric()) {
        let bytes = word.as_bytes();
        let plan_ref = bytes.len() >= 2
            && bytes.len() <= 3
            && matches!(bytes[0], b'P' | b'C')
            && bytes[1..].iter().all(u8::is_ascii_digit);
        assert!(
            !plan_ref,
            "the public document carries the reference {word:?}"
        );
    }
}

#[test]
fn a_landed_write_names_its_full_target_and_the_uuid_it_read_back() {
    // A volume is named by its pool, so the unconfirmed error must carry the pool and the uuid the read-back showed.
    let mut posted = false;
    let canned = Canned::start("unconfirmed-volume", move |method, target| {
        if method == "POST" {
            posted = true;
            return sync_reply(201, Value::Null);
        }
        if method == "GET" && target.starts_with("/1.0/storage-pools/fast/volumes/custom/vol1") {
            return if posted {
                object_reply(&json!({"name": "vol1", "config": {"volatile.uuid": OTHER_UUID}}))
            } else {
                not_found()
            };
        }
        not_found()
    });
    let wanted = target(IncusKind::Volume, "svc", "vol1", Some("fast"));
    match canned.writer().create(&wanted, &marked()) {
        Err(IncusError::AppliedUnconfirmed(unconfirmed)) => {
            assert_eq!(unconfirmed.target, wanted, "the full target, pool included");
            assert_eq!(
                unconfirmed.uuid.as_deref(),
                Some(OTHER_UUID),
                "the uuid read back"
            );
            assert_eq!(unconfirmed.owner, Some(OwnerClass::Foreign));
        }
        other => panic!("expected AppliedUnconfirmed: {other:?}"),
    }
}

#[test]
fn read_class_names_the_owner_of_an_object_with_a_get_and_no_write() {
    // A read names the class of every owner. Only a GET is sent, so the fake records no write.
    let (_dir, fake) = start("read-class");
    let writer = writer(&fake, "toy");
    let droidd: Reading = writer
        .read_class(&target(IncusKind::Instance, "svc", "db1", None))
        .expect("the read names the owner");
    assert_eq!(droidd.class, OwnerClass::Droidd);
    assert!(droidd.etag.starts_with('"'), "{}", droidd.etag);
    let kernel = writer
        .read_class(&target(IncusKind::Instance, "svc", "k1", None))
        .expect("the read names the owner");
    assert_eq!(
        kernel.class,
        OwnerClass::Kernel {
            entry: EntryId("toy".to_string())
        }
    );
    assert_eq!(write_count(&fake), 0, "a read sends no write");
}

#[test]
fn read_class_refuses_a_target_outside_the_family_before_any_request() {
    // The family is checked before the read, so a target outside it sends no request at all.
    let (_dir, fake) = start("read-class-family");
    let writer = writer(&fake, "toy");
    let result = writer.read_class(&target(IncusKind::Instance, "default", "guest1", None));
    assert!(matches!(result, Err(IncusError::Refused(_))), "{result:?}");
    assert!(fake.requests().is_empty(), "no request was made");
}
