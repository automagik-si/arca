//! The GET-only guarantee, tested against a fake Incus and the recording shim. Every server here is bound under a fresh
//! temporary directory. No test connects to a real Incus socket.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arca_incus::{
    FakeIncus, FakeState, IncusError, IncusRead, RecordedRequest, RecordingShim, ShimSummary,
    UnixIncus,
};
use serde_json::{Value, json};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/state.json");

/// A fresh temporary directory, removed when the test ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .subsec_nanos();
        let dir =
            std::env::temp_dir().join(format!("arca-incus-{}-{nanos}-{tag}", std::process::id()));
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

fn fixture() -> FakeState {
    FakeState::load(Path::new(FIXTURE)).expect("load the fixture")
}

/// Writes raw bytes to a unix socket and returns everything the server sends before it closes.
fn exchange(socket: &Path, request: &[u8]) -> String {
    let mut stream = UnixStream::connect(socket).expect("connect to the test server");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set a read timeout");
    stream.write_all(request).expect("write the request");
    let mut reply = Vec::new();
    let _ = stream.read_to_end(&mut reply);
    String::from_utf8_lossy(&reply).into_owned()
}

fn status_of(reply: &str) -> u16 {
    reply
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {reply:?}"))
}

fn log_lines(log: &Path) -> Vec<Value> {
    fs::read_to_string(log)
        .expect("read the shim log")
        .lines()
        .map(|line| serde_json::from_str(line).expect("each log line is JSON"))
        .collect()
}

#[test]
fn unix_incus_reads_each_fixture_path_and_the_fake_records_only_gets() {
    let dir = Scratch::new("reads");
    let state = fixture();
    let fake = FakeIncus::start(&dir.0, fixture()).expect("start the fake");
    let client = UnixIncus::new(fake.socket().to_path_buf());

    assert_eq!(client.get("/1.0").expect("server"), state.server);
    assert_eq!(
        client.get("/1.0/projects?recursion=1").expect("projects"),
        Value::Array(state.projects.clone())
    );
    assert_eq!(
        client
            .get("/1.0/instances?all-projects=true&recursion=1")
            .expect("instances"),
        Value::Array(state.instances.clone())
    );
    let pools = client
        .get("/1.0/storage-pools?recursion=1")
        .expect("pool list");
    assert_eq!(pools.as_array().map(Vec::len), Some(state.pools.len()));
    for (name, resources) in &state.pools {
        let path = format!("/1.0/storage-pools/{name}/resources");
        assert_eq!(&client.get(&path).expect("pool resources"), resources);
    }

    let seen = fake.requests();
    assert_eq!(seen.len(), 4 + state.pools.len(), "{seen:?}");
    assert!(
        seen.iter().all(|r| r.method == "GET" && r.status == 200),
        "{seen:?}"
    );
}

#[test]
fn an_unknown_route_or_pool_is_a_status_error_and_is_recorded() {
    let dir = Scratch::new("missing");
    let fake = FakeIncus::start(&dir.0, fixture()).expect("start the fake");
    let client = UnixIncus::new(fake.socket().to_path_buf());

    let pool = client
        .get("/1.0/storage-pools/nosuch/resources")
        .expect_err("an unknown pool");
    assert!(
        matches!(pool, IncusError::Status { code: 404, .. }),
        "{pool:?}"
    );
    let route = client
        .get("/1.0/projects")
        .expect_err("the list without recursion is not served");
    assert!(
        matches!(route, IncusError::Status { code: 404, .. }),
        "{route:?}"
    );
    let statuses: Vec<u16> = fake.requests().iter().map(|r| r.status).collect();
    assert_eq!(statuses, vec![404, 404]);
}

#[test]
fn a_path_that_is_not_a_plain_origin_target_is_refused_before_any_request() {
    let dir = Scratch::new("badpath");
    let fake = FakeIncus::start(&dir.0, fixture()).expect("start the fake");
    let client = UnixIncus::new(fake.socket().to_path_buf());

    let long = format!("/{}", "a".repeat(2048));
    let bad = [
        "1.0",
        "",
        "/1.0 HTTP/1.1",
        "/1.0\r\nX-Injected: 1",
        "/1.0#fragment",
        "/1.0/a\u{7f}b",
        "/1.0/\u{fc}",
        long.as_str(),
    ];
    for path in bad {
        assert!(
            matches!(client.get(path), Err(IncusError::Io(_))),
            "{path:?} was not refused"
        );
    }
    assert!(
        fake.requests().is_empty(),
        "a refused path still reached the fake"
    );
}

#[test]
fn a_socket_that_is_not_there_is_an_io_error() {
    let dir = Scratch::new("absent");
    let client = UnixIncus::new(dir.at("absent.sock"));
    assert!(matches!(client.get("/1.0"), Err(IncusError::Io(_))));
}

#[test]
fn a_zero_deadline_is_a_timeout_before_any_request_is_sent() {
    let dir = Scratch::new("zero");
    let fake = FakeIncus::start(&dir.0, fixture()).expect("start the fake");
    let client = UnixIncus::new(fake.socket().to_path_buf()).with_timeout(Duration::ZERO);
    assert_eq!(client.get("/1.0"), Err(IncusError::Timeout));
    assert!(
        fake.requests().is_empty(),
        "a request was sent after the deadline had passed"
    );
}

#[test]
fn a_body_over_sixteen_mib_is_refused_as_too_large() {
    let dir = Scratch::new("large");
    let mut state = fixture();
    state.pools = vec![(
        "big".to_string(),
        json!({ "blob": "a".repeat(17 * 1024 * 1024) }),
    )];
    let fake = FakeIncus::start(&dir.0, state).expect("start the fake");
    let client = UnixIncus::new(fake.socket().to_path_buf());

    assert_eq!(
        client.get("/1.0/storage-pools/big/resources"),
        Err(IncusError::TooLarge)
    );
}

#[test]
fn a_socket_that_never_answers_yields_timeout_on_the_clients_deadline() {
    let dir = Scratch::new("stall");
    // A local listener under the scratch directory. It accepts and never writes, so the client must give up on its own.
    let listener = UnixListener::bind(dir.at("stall.sock")).expect("bind the stalled listener");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let holder = thread::spawn(move || {
        let mut held = Vec::new();
        while !flag.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => held.push(stream),
                Err(_) => thread::sleep(Duration::from_millis(5)),
            }
        }
        held.len()
    });

    let client = UnixIncus::new(dir.at("stall.sock")).with_timeout(Duration::from_millis(300));
    let started = Instant::now();
    let outcome = client.get("/1.0");
    let elapsed = started.elapsed();
    stop.store(true, Ordering::SeqCst);
    holder.join().expect("the holder thread");

    assert_eq!(outcome, Err(IncusError::Timeout));
    assert!(
        elapsed < Duration::from_secs(5),
        "the deadline was not honoured: {elapsed:?}"
    );
}

#[test]
fn the_shim_answers_post_put_patch_and_delete_with_405_and_never_forwards_them() {
    let dir = Scratch::new("refuse");
    let fake = FakeIncus::start(&dir.0, fixture()).expect("start the fake");
    let shim = RecordingShim::start(fake.socket(), &dir.at("shim.sock"), &dir.at("shim.log"))
        .expect("start the shim");

    for method in ["POST", "PUT", "PATCH", "DELETE"] {
        let request = format!(
            "{method} /1.0/instances/db1 HTTP/1.1\r\nHost: incus\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
        );
        let reply = exchange(shim.socket(), request.as_bytes());
        assert_eq!(
            status_of(&reply),
            405,
            "{method} was not refused: {reply:?}"
        );
    }
    assert!(
        fake.requests().is_empty(),
        "a refused method reached the fake"
    );

    let summary = shim.stop().expect("stop the shim");
    assert_eq!(summary, ShimSummary { get: 0, refused: 4 });
    let log = log_lines(&dir.at("shim.log"));
    assert_eq!(log.len(), 4);
    for (line, method) in log.iter().zip(["POST", "PUT", "PATCH", "DELETE"]) {
        assert_eq!(line["method"], method);
        assert_eq!(line["path"], "/1.0/instances/db1");
        assert_eq!(line["status"], 405);
        assert_eq!(line["forwarded"], false);
    }
}

#[test]
fn the_shim_log_carries_no_header_or_body_bytes() {
    const TOKEN: &str = "test-only-token-7a91";
    const BODY: &str = "body-marker-3c05";
    let dir = Scratch::new("secrets");
    let fake = FakeIncus::start(&dir.0, fixture()).expect("start the fake");
    let shim = RecordingShim::start(fake.socket(), &dir.at("shim.sock"), &dir.at("shim.log"))
        .expect("start the shim");

    let post = format!(
        "POST /1.0/instances/db1 HTTP/1.1\r\nHost: incus\r\nAuthorization: Bearer {TOKEN}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{BODY}",
        BODY.len()
    );
    let get = format!(
        "GET /1.0 HTTP/1.1\r\nHost: incus\r\nAuthorization: Bearer {TOKEN}\r\nConnection: close\r\n\r\n"
    );
    assert_eq!(status_of(&exchange(shim.socket(), post.as_bytes())), 405);
    assert_eq!(status_of(&exchange(shim.socket(), get.as_bytes())), 200);
    shim.stop().expect("stop the shim");

    let log = fs::read_to_string(dir.at("shim.log")).expect("read the log");
    assert_eq!(
        log_lines(&dir.at("shim.log")).len(),
        2,
        "both requests are logged"
    );
    assert!(!log.contains(TOKEN), "a header value reached the log");
    assert!(!log.contains(BODY), "a body byte reached the log");
    assert!(
        !log.to_ascii_lowercase().contains("authorization"),
        "a header name reached the log"
    );
    assert!(!log.contains("Bearer"), "a bearer scheme reached the log");
}

#[test]
fn a_get_through_the_shim_reaches_the_fake_and_is_logged_as_forwarded() {
    let dir = Scratch::new("forward");
    let state = fixture();
    let fake = FakeIncus::start(&dir.0, fixture()).expect("start the fake");
    let shim = RecordingShim::start(fake.socket(), &dir.at("shim.sock"), &dir.at("shim.log"))
        .expect("start the shim");
    let client = UnixIncus::new(shim.socket().to_path_buf());

    assert_eq!(
        client.get("/1.0").expect("server through the shim"),
        state.server
    );
    assert_eq!(
        client
            .get("/1.0/instances?all-projects=true&recursion=1")
            .expect("instances through the shim"),
        Value::Array(state.instances.clone())
    );
    let summary = shim.stop().expect("stop the shim");
    assert_eq!(summary, ShimSummary { get: 2, refused: 0 });

    let seen = fake.requests();
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert!(
        seen.iter().all(|r| r.method == "GET" && r.status == 200),
        "{seen:?}"
    );
    let log = log_lines(&dir.at("shim.log"));
    assert_eq!(log.len(), 2);
    assert_eq!(log[0]["method"], "GET");
    assert_eq!(log[0]["path"], "/1.0");
    assert_eq!(log[0]["status"], 200);
    assert_eq!(log[0]["forwarded"], true);
}

#[test]
fn a_get_that_carries_a_body_is_refused_and_bytes_after_a_get_head_are_never_forwarded() {
    let dir = Scratch::new("smuggle");
    let fake = FakeIncus::start(&dir.0, fixture()).expect("start the fake");
    let shim = RecordingShim::start(fake.socket(), &dir.at("shim.sock"), &dir.at("shim.log"))
        .expect("start the shim");

    let with_body =
        "GET /1.0 HTTP/1.1\r\nHost: incus\r\nContent-Length: 4\r\nConnection: close\r\n\r\nbody";
    let chunked = "GET /1.0 HTTP/1.1\r\nHost: incus\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n0\r\n\r\n";
    assert_eq!(
        status_of(&exchange(shim.socket(), with_body.as_bytes())),
        400
    );
    assert_eq!(status_of(&exchange(shim.socket(), chunked.as_bytes())), 400);

    let pipelined = "GET /1.0 HTTP/1.1\r\nHost: incus\r\nConnection: close\r\n\r\n\
                     POST /1.0/instances/db1 HTTP/1.1\r\nHost: incus\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    assert_eq!(
        status_of(&exchange(shim.socket(), pipelined.as_bytes())),
        200
    );

    let seen = fake.requests();
    assert_eq!(
        seen.len(),
        1,
        "only the GET head may reach the fake: {seen:?}"
    );
    assert_eq!(seen[0].method, "GET");
    let summary = shim.stop().expect("stop the shim");
    assert_eq!(summary, ShimSummary { get: 1, refused: 2 });
}

#[test]
fn a_get_the_upstream_cannot_take_is_answered_502_and_counted_as_refused() {
    let dir = Scratch::new("nodown");
    let shim = RecordingShim::start(
        &dir.at("absent.sock"),
        &dir.at("shim.sock"),
        &dir.at("shim.log"),
    )
    .expect("start the shim");
    let reply = exchange(
        shim.socket(),
        b"GET /1.0 HTTP/1.1\r\nHost: incus\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status_of(&reply), 502, "{reply:?}");
    let summary = shim.stop().expect("stop the shim");
    assert_eq!(summary, ShimSummary { get: 0, refused: 1 });
    let log = log_lines(&dir.at("shim.log"));
    assert_eq!(log[0]["method"], "GET");
    assert_eq!(log[0]["status"], 502);
    assert_eq!(log[0]["forwarded"], false);
}

#[test]
fn a_malformed_request_is_refused_with_400_and_never_forwarded() {
    let dir = Scratch::new("malformed");
    let fake = FakeIncus::start(&dir.0, fixture()).expect("start the fake");
    let shim = RecordingShim::start(fake.socket(), &dir.at("shim.sock"), &dir.at("shim.log"))
        .expect("start the shim");

    let bad = [
        "GARBAGE\r\nHost: incus\r\n\r\n",
        "GET /1.0\r\nHost: incus\r\n\r\n",
        "GET /1.0 SPDY/3\r\nHost: incus\r\n\r\n",
        "GET /1.0 HTTP/1.1\r\nNoColonHeader\r\n\r\n",
    ];
    for request in bad {
        assert_eq!(
            status_of(&exchange(shim.socket(), request.as_bytes())),
            400,
            "{request:?}"
        );
    }
    assert!(
        fake.requests().is_empty(),
        "a malformed request reached the fake"
    );
    let summary = shim.stop().expect("stop the shim");
    assert_eq!(summary, ShimSummary { get: 0, refused: 4 });
}

#[test]
fn methods_are_matched_exactly_so_lowercase_get_and_head_are_refused() {
    let dir = Scratch::new("methods");
    let fake = FakeIncus::start(&dir.0, fixture()).expect("start the fake");
    let shim = RecordingShim::start(fake.socket(), &dir.at("shim.sock"), &dir.at("shim.log"))
        .expect("start the shim");

    for request in [
        "get /1.0 HTTP/1.1",
        "HEAD /1.0 HTTP/1.1",
        "CONNECT incus:80 HTTP/1.1",
    ] {
        let raw = format!("{request}\r\nHost: incus\r\nConnection: close\r\n\r\n");
        assert_eq!(
            status_of(&exchange(shim.socket(), raw.as_bytes())),
            405,
            "{request:?}"
        );
    }
    assert!(fake.requests().is_empty());
    let summary = shim.stop().expect("stop the shim");
    assert_eq!(summary, ShimSummary { get: 0, refused: 3 });
}

#[test]
fn the_fake_answers_any_non_get_with_405_and_records_it() {
    let dir = Scratch::new("fakepost");
    let fake = FakeIncus::start(&dir.0, fixture()).expect("start the fake");
    let reply = exchange(
        fake.socket(),
        b"POST /1.0 HTTP/1.1\r\nHost: incus\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status_of(&reply), 405);
    assert_eq!(
        fake.requests(),
        vec![RecordedRequest {
            method: "POST".to_string(),
            path: "/1.0".to_string(),
            status: 405
        }]
    );
}

#[test]
fn the_fixture_marks_each_object_as_its_case_needs() {
    let state = fixture();
    let find = |name: &str| -> Value {
        state
            .instances
            .iter()
            .find(|i| i["name"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("no instance {name}"))
    };
    let has = |object: &Value, key: &str| {
        object["config"]
            .as_object()
            .is_some_and(|c| c.contains_key(key))
    };

    assert!(has(&find("db1"), "user.arca.template"));
    assert!(has(&find("phone1"), "user.droid.lease"));
    assert_eq!(
        find("phone1")["expanded_config"]["security.privileged"],
        "true"
    );
    assert!(has(&find("desk1"), "user.tactu.desktop"));
    let guest = find("guest1");
    assert!(
        guest["config"]
            .as_object()
            .is_some_and(|c| c.keys().all(|k| !k.starts_with("user.")))
    );
    assert!(has(&find("k1"), "user.arcakernel.entry"));
    assert!(has(&find("x1"), "user.droid.lease") && has(&find("x1"), "user.arcakernel.entry"));

    let projects: Vec<&str> = state
        .projects
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    for name in ["default", "droid", "bench", "svc"] {
        assert!(projects.contains(&name), "missing project {name}");
    }
}

#[test]
fn the_client_is_usable_as_a_shared_trait_object() {
    fn takes(_: &dyn IncusRead) {}
    let client = UnixIncus::new(PathBuf::from("/nonexistent/incus.sock"));
    takes(&client);
    fn shared<T: Send + Sync>() {}
    shared::<UnixIncus>();
}
