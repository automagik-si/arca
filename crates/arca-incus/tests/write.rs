//! The Incus writer, tested against the fake Incus only. Every server is bound under a fresh temporary directory, and no test
//! connects to a real Incus socket. Each refusal is checked twice: the writer returns `IncusError::Refused`, and the fake
//! receives no write.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use arca_incus::{FakeIncus, FakeState, IncusError, Owned, Target, UnixIncusWriter};
use arca_kernel::canon::Digest;
use arca_kernel::desired::{EntryId, PluginId};
use arca_kernel::ownership::{IncusKind, ObjectRef};
use serde_json::{Value, json};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/state.json");
const MARK: &str = "user.arcakernel.entry";
const RETIRING: &str = "user.arcakernel.retiring";

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

fn writer(fake: &FakeIncus, entry: &str) -> UnixIncusWriter {
    UnixIncusWriter::new(
        fake.socket().to_path_buf(),
        EntryId(entry.to_string()),
        BTreeMap::new(),
    )
}

fn target(kind: IncusKind, project: &str, name: &str, parent: Option<&str>) -> Target {
    Target {
        object: ObjectRef {
            kind,
            project: project.to_string(),
            name: name.to_string(),
            uuid: None,
        },
        parent: parent.map(str::to_string),
    }
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
        assert!(created.uuid().is_some(), "{kind:?} has a uuid after create");

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
    assert!(writes.iter().all(|w| w.status == 202), "{writes:?}");
    let waits = fake
        .requests()
        .iter()
        .filter(|r| {
            r.method == "GET" && r.path.contains("/operations/") && r.path.ends_with("/wait")
        })
        .count();
    assert_eq!(waits, writes.len(), "every write waits on its operation");
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
fn a_tombstone_needs_a_uuid_and_a_delete_needs_its_tombstone() {
    let (_dir, fake) = start("delete-refusals");
    let writer = writer(&fake, "toy");

    // The fixture's kernel object has no uuid, so it can be neither retired nor deleted.
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
    assert!(reply.starts_with("HTTP/1.1 202"), "{reply}");

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
    assert!(raced.starts_with("HTTP/1.1 202"), "{raced}");

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
    let project = target(IncusKind::Project, "kfull", "kfull", None);
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
