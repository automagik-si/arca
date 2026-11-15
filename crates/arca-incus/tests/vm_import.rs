#![cfg(feature = "write")]
//! The import writer against the fake Incus: it makes only marked objects of the shapes an import makes, and it starts,
//! stops and deletes only objects that carry the import mark.

use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use arca_incus::{FakeIncus, FakeState, IMPORT_KEY, IncusError, VmImportWriter};
use serde_json::{Value, json};

static SEQ: AtomicU64 = AtomicU64::new(0);

struct Rig {
    dir: PathBuf,
    fake: FakeIncus,
    writer: VmImportWriter,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn rig() -> Rig {
    let dir = std::env::temp_dir().join(format!(
        "arca-vm-import-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir(&dir).expect("scratch");
    let state = FakeState {
        server: json!({}),
        projects: vec![
            json!({ "name": "default", "config": {} }),
            json!({ "name": "people", "config": { "features.storage.volumes": "true" } }),
        ],
        instances: vec![json!({
            "name": "someone-else",
            "project": "people",
            "type": "virtual-machine",
            "status": "Running",
            "profiles": [],
            "config": {},
            "devices": {},
        })],
        pools: vec![("remote".to_string(), json!({}))],
        networks: vec![
            json!({ "name": "br-lan", "project": "default", "managed": true, "type": "bridge", "config": {} }),
        ],
    };
    let fake = FakeIncus::start(&dir, state).expect("the fake serves");
    fake.set_pool_driver("remote", "ceph");
    let writer = VmImportWriter::new(fake.socket().to_path_buf());
    Rig { dir, fake, writer }
}

fn vm_body() -> Value {
    json!({
        "name": "web-1",
        "type": "virtual-machine",
        "source": { "type": "none" },
        "start": false,
        "profiles": [],
        "config": { IMPORT_KEY: "0123456789abcdef", "limits.cpu": "2", "limits.memory": "4096MiB" },
        "devices": { "root": { "type": "disk", "path": "/", "pool": "remote", "size": "20480MiB" } },
    })
}

fn writes(rig: &Rig) -> Vec<String> {
    rig.fake
        .requests()
        .into_iter()
        .filter(|request| request.method != "GET")
        .map(|request| format!("{} {}", request.method, request.path))
        .collect()
}

#[test]
fn it_reads_the_pool_the_network_and_the_project() {
    let rig = rig();
    assert_eq!(
        rig.writer.pool("remote").expect("read").expect("a pool")["driver"],
        "ceph"
    );
    assert!(rig.writer.pool("nowhere").expect("read").is_none());
    assert!(rig.writer.project("people").expect("read").is_some());
    assert!(
        rig.writer
            .instance("people", "web-1")
            .expect("read")
            .is_none()
    );
    assert!(matches!(
        rig.writer.pool("../x"),
        Err(IncusError::Refused(_))
    ));
}

#[test]
fn it_makes_a_marked_vm_volume_and_acl_and_removes_them() {
    let rig = rig();
    let acl = json!({ "name": "arca-import-web-1", "config": { IMPORT_KEY: "0123456789abcdef" }, "ingress": [], "egress": [] });
    rig.writer.create_acl("default", &acl).expect("made");
    let volume = json!({ "name": "web-1-disk1", "type": "custom", "content_type": "block", "config": { "size": "1024MiB", IMPORT_KEY: "0123456789abcdef" } });
    rig.writer
        .create_volume("people", "remote", &volume)
        .expect("made");
    rig.writer
        .create_instance("people", &vm_body())
        .expect("made");
    rig.writer.start("people", "web-1").expect("started");
    let read = rig
        .writer
        .instance("people", "web-1")
        .expect("read")
        .expect("there");
    assert_eq!(read["status"], "Running");
    let removed = rig
        .writer
        .remove_instance("people", "web-1")
        .expect("removed")
        .expect("it was there");
    assert_eq!(removed["config"]["limits.cpu"], "2");
    assert!(
        rig.writer
            .instance("people", "web-1")
            .expect("read")
            .is_none()
    );
    assert!(
        rig.writer
            .remove_volume("people", "remote", "web-1-disk1")
            .expect("removed")
    );
    assert!(
        rig.writer
            .remove_acl("default", "arca-import-web-1")
            .expect("removed")
    );
    // Gone is not an error.
    assert!(
        !rig.writer
            .remove_volume("people", "remote", "web-1-disk1")
            .expect("read")
    );
    assert!(
        rig.writer
            .remove_instance("people", "web-1")
            .expect("read")
            .is_none()
    );
    let sent = writes(&rig);
    assert!(
        sent.iter()
            .any(|line| line.starts_with("POST /1.0/network-acls?project=default"))
    );
    assert!(sent.iter().any(|line| {
        line.starts_with("POST /1.0/storage-pools/remote/volumes/custom?project=people")
    }));
    assert!(
        sent.iter()
            .any(|line| line.starts_with("DELETE /1.0/instances/web-1?project=people"))
    );
}

#[test]
fn a_body_without_the_mark_or_of_another_shape_is_never_sent() {
    let rig = rig();
    let mut unmarked = vm_body();
    unmarked["config"]
        .as_object_mut()
        .expect("config")
        .remove(IMPORT_KEY);
    let mut started = vm_body();
    started["start"] = json!(true);
    let mut from_image = vm_body();
    from_image["source"] = json!({ "type": "image", "alias": "debian" });
    let mut container = vm_body();
    container["type"] = json!("container");
    for body in [unmarked, started, from_image, container] {
        assert!(matches!(
            rig.writer.create_instance("people", &body),
            Err(IncusError::Refused(_))
        ));
    }
    let foreign_acl = json!({ "name": "web-acl", "config": { IMPORT_KEY: "x" } });
    assert!(rig.writer.create_acl("default", &foreign_acl).is_err());
    let filesystem =
        json!({ "name": "v", "content_type": "filesystem", "config": { IMPORT_KEY: "x" } });
    assert!(
        rig.writer
            .create_volume("people", "remote", &filesystem)
            .is_err()
    );
    let taken = json!({ "name": "someone-else", "type": "virtual-machine", "source": { "type": "none" }, "start": false, "config": { IMPORT_KEY: "x" } });
    assert!(rig.writer.create_instance("people", &taken).is_err());
    assert!(writes(&rig).is_empty());
}

#[test]
fn an_instance_without_the_mark_is_never_started_or_removed() {
    let rig = rig();
    assert!(matches!(
        rig.writer.start("people", "someone-else"),
        Err(IncusError::Refused(_))
    ));
    assert!(matches!(
        rig.writer.remove_instance("people", "someone-else"),
        Err(IncusError::Refused(_))
    ));
    assert!(rig.writer.remove_acl("default", "not-an-import").is_err());
    assert!(writes(&rig).is_empty());
    assert!(!rig.writer.agent_answers("people", "someone-else"));
}

/// One POST straight to the fake, past the writer: how an object the import did not make comes to exist.
fn post_past_the_writer(socket: &Path, path: &str, body: &Value) {
    let mut stream = UnixStream::connect(socket).expect("the fake answers");
    let body = body.to_string();
    write!(
        stream,
        "POST {path} HTTP/1.1\r\nHost: incus\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .expect("sent");
    stream.shutdown(std::net::Shutdown::Write).expect("sent");
    let mut answer = String::new();
    stream.read_to_string(&mut answer).expect("answered");
    assert!(answer.starts_with("HTTP/1.1 2"), "{answer}");
}

#[test]
fn a_volume_or_acl_without_the_mark_is_never_removed_even_with_an_import_name() {
    let rig = rig();
    post_past_the_writer(
        rig.fake.socket(),
        "/1.0/storage-pools/remote/volumes/custom?project=people",
        &json!({ "name": "web-1-disk1", "type": "custom", "content_type": "block", "config": { "size": "1024MiB" } }),
    );
    rig.fake.add_acl(
        "default",
        json!({ "name": "arca-import-web-1", "config": {}, "ingress": [], "egress": [] }),
    );
    let before = writes(&rig);
    assert!(matches!(
        rig.writer.remove_volume("people", "remote", "web-1-disk1"),
        Err(IncusError::Refused(_))
    ));
    assert!(matches!(
        rig.writer.remove_acl("default", "arca-import-web-1"),
        Err(IncusError::Refused(_))
    ));
    assert_eq!(writes(&rig), before, "no DELETE was sent");
    assert!(
        rig.fake
            .requests()
            .iter()
            .all(|request| request.method != "DELETE")
    );
}
