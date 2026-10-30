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

/// A raw upstream on `socket` that takes one connection, records every byte the client sends, and answers `200`. It keeps
/// reading until the client is idle for a moment, so bytes sent after a request head are recorded too. The bytes come back
/// through the join handle.
fn record_one_connection(socket: &Path) -> thread::JoinHandle<Vec<u8>> {
    let listener = UnixListener::bind(socket).expect("bind the recording upstream");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    thread::spawn(move || {
        let started = Instant::now();
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(_) if started.elapsed() < Duration::from_secs(10) => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("no connection reached the recording upstream: {error}"),
            }
        };
        stream.set_nonblocking(false).expect("blocking stream");
        stream
            .set_read_timeout(Some(Duration::from_millis(300)))
            .expect("read timeout");
        let mut seen = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => seen.extend_from_slice(&chunk[..n]),
            }
        }
        let _ = stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
        seen
    })
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
fn a_query_string_never_reaches_the_shim_log() {
    const SECRET: &str = "secret=x-query-9b2f";
    let dir = Scratch::new("query");
    let fake = FakeIncus::start(&dir.0, fixture()).expect("start the fake");
    let shim = RecordingShim::start(fake.socket(), &dir.at("shim.sock"), &dir.at("shim.log"))
        .expect("start the shim");

    let get = format!("GET /1.0?{SECRET} HTTP/1.1\r\nHost: incus\r\nConnection: close\r\n\r\n");
    let post = format!(
        "POST /1.0/instances/db1?{SECRET} HTTP/1.1\r\nHost: incus\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    exchange(shim.socket(), get.as_bytes());
    assert_eq!(status_of(&exchange(shim.socket(), post.as_bytes())), 405);
    shim.stop().expect("stop the shim");

    let raw = fs::read_to_string(dir.at("shim.log")).expect("read the log");
    assert!(!raw.contains(SECRET), "a query string reached the log");
    assert!(
        !raw.contains("secret"),
        "a query parameter name reached the log"
    );
    let log = log_lines(&dir.at("shim.log"));
    assert_eq!(log[0]["path"], "/1.0");
    assert_eq!(log[1]["path"], "/1.0/instances/db1");
}

#[test]
fn an_upgrade_or_a_header_name_that_is_not_a_token_is_refused_with_400_and_never_forwarded() {
    let dir = Scratch::new("tokens");
    let fake = FakeIncus::start(&dir.0, fixture()).expect("start the fake");
    let shim = RecordingShim::start(fake.socket(), &dir.at("shim.sock"), &dir.at("shim.log"))
        .expect("start the shim");

    let bad = [
        "GET /1.0 HTTP/1.1\r\nHost: incus\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
        "GET /1.0 HTTP/1.1\r\nHost: incus\r\nupgrade: h2c\r\n\r\n",
        "GET /1.0 HTTP/1.1\r\nHost: incus\r\nX(Bad): 1\r\n\r\n",
        "GET /1.0 HTTP/1.1\r\nHost: incus\r\nX@Name: 1\r\n\r\n",
        "GET /1.0 HTTP/1.1\r\nHost: incus\r\n\"quoted\": 1\r\n\r\n",
        "GET /1.0 HTTP/1.1\r\nHost: incus\r\nX/Slash: 1\r\n\r\n",
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
        "a refused head reached the fake"
    );

    // A header name made of token characters is still a plain GET and is forwarded.
    let token =
        "GET /1.0 HTTP/1.1\r\nHost: incus\r\nX-Probe.1_a!b~: y\r\nConnection: close\r\n\r\n";
    assert_eq!(status_of(&exchange(shim.socket(), token.as_bytes())), 200);
    assert_eq!(fake.requests().len(), 1);

    let summary = shim.stop().expect("stop the shim");
    assert_eq!(summary, ShimSummary { get: 1, refused: 6 });
}

#[test]
fn a_timeout_too_long_for_the_clock_is_an_unbounded_wait_not_a_panic() {
    let dir = Scratch::new("huge");
    let state = fixture();
    let fake = FakeIncus::start(&dir.0, fixture()).expect("start the fake");
    let client = UnixIncus::new(fake.socket().to_path_buf()).with_timeout(Duration::MAX);
    assert_eq!(
        client
            .get("/1.0")
            .expect("a server answer with an unbounded deadline"),
        state.server
    );
}

#[test]
fn the_shim_summary_serializes_as_its_two_counts() {
    let summary = ShimSummary { get: 2, refused: 1 };
    assert_eq!(
        serde_json::to_value(summary).expect("serialize the summary"),
        json!({ "get": 2, "refused": 1 })
    );
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
    let upstream = record_one_connection(&dir.at("upstream.sock"));
    let shim = RecordingShim::start(
        &dir.at("upstream.sock"),
        &dir.at("shim.sock"),
        &dir.at("shim.log"),
    )
    .expect("start the shim");

    let with_body =
        "GET /1.0 HTTP/1.1\r\nHost: incus\r\nContent-Length: 4\r\nConnection: close\r\n\r\nbody";
    let chunked = "GET /1.0 HTTP/1.1\r\nHost: incus\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n0\r\n\r\n";
    assert_eq!(
        status_of(&exchange(shim.socket(), with_body.as_bytes())),
        400
    );
    assert_eq!(status_of(&exchange(shim.socket(), chunked.as_bytes())), 400);

    let get_head = "GET /1.0 HTTP/1.1\r\nHost: incus\r\nConnection: close\r\n\r\n";
    let pipelined = format!(
        "{get_head}POST /1.0/instances/db1 HTTP/1.1\r\nHost: incus\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    assert_eq!(
        status_of(&exchange(shim.socket(), pipelined.as_bytes())),
        200
    );

    let summary = shim.stop().expect("stop the shim");
    assert_eq!(summary, ShimSummary { get: 1, refused: 2 });
    let relayed = upstream.join().expect("the recording upstream");
    assert_eq!(
        relayed,
        get_head.as_bytes(),
        "the upstream must receive exactly the GET head and nothing after it"
    );
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

// The capped log and the live summary.

/// The longest line the log holds in these tests: a forwarded GET of the longest path they send.
fn longest_line(log: &Path) -> u64 {
    let rotated = log.with_extension("log.1");
    [log, rotated.as_path()]
        .iter()
        .filter_map(|path| fs::read_to_string(path).ok())
        .flat_map(|text| {
            text.lines()
                .map(|line| line.len() as u64 + 1)
                .collect::<Vec<_>>()
        })
        .max()
        .unwrap_or(0)
}

#[test]
fn a_capped_log_never_exceeds_its_cap_plus_one_line_and_keeps_one_rotated_file() {
    let dir = Scratch::new("capped");
    let fake = FakeIncus::start(&dir.0, fixture()).expect("start the fake");
    let log = dir.at("shim.log");
    let cap = 200;
    let shim = RecordingShim::start_capped(fake.socket(), &dir.at("shim.sock"), &log, cap)
        .expect("start the capped shim");
    let client = UnixIncus::new(shim.socket().to_path_buf());
    for _ in 0..40 {
        client.get("/1.0").expect("a GET through the shim");
    }
    let summary = shim.stop().expect("stop the shim");
    assert_eq!(
        summary,
        ShimSummary {
            get: 40,
            refused: 0
        }
    );

    let line = longest_line(&log);
    assert!(line > 0, "the log holds lines");
    let rotated = dir.at("shim.log.1");
    for path in [&log, &rotated] {
        let len = fs::metadata(path).expect("the log file exists").len();
        assert!(
            len < cap + line,
            "{} holds {len} bytes, over the cap of {cap} plus one line of {line}",
            path.display()
        );
    }
    assert!(
        fs::metadata(&rotated).expect("the rotated log").len() >= cap,
        "the rotated log was full when it was rotated"
    );
    assert!(
        !dir.at("shim.log.2").exists(),
        "only one rotated file is kept"
    );
    for line in log_lines(&log).into_iter().chain(log_lines(&rotated)) {
        assert_eq!(line["method"], "GET");
        assert_eq!(line["forwarded"], true);
    }
}

#[test]
fn a_capped_shim_rotates_a_full_log_it_starts_on() {
    let dir = Scratch::new("capped-start");
    let fake = FakeIncus::start(&dir.0, fixture()).expect("start the fake");
    let log = dir.at("shim.log");
    fs::write(&log, "x".repeat(300)).expect("write a full log");
    let shim = RecordingShim::start_capped(fake.socket(), &dir.at("shim.sock"), &log, 200)
        .expect("start the capped shim");
    assert_eq!(
        fs::metadata(dir.at("shim.log.1"))
            .expect("rotated at start")
            .len(),
        300
    );
    assert_eq!(fs::metadata(&log).expect("a new log").len(), 0);
    shim.stop().expect("stop the shim");
}

#[test]
fn a_log_cap_of_zero_is_refused_before_the_shim_binds() {
    let dir = Scratch::new("capped-zero");
    let fake = FakeIncus::start(&dir.0, fixture()).expect("start the fake");
    let refused =
        RecordingShim::start_capped(fake.socket(), &dir.at("shim.sock"), &dir.at("shim.log"), 0);
    assert_eq!(
        refused.err().map(|e| e.kind()),
        Some(std::io::ErrorKind::InvalidInput)
    );
    assert!(!dir.at("shim.sock").exists(), "nothing was bound");
}

#[test]
fn the_live_summary_counts_while_the_shim_keeps_serving() {
    let dir = Scratch::new("live-summary");
    let fake = FakeIncus::start(&dir.0, fixture()).expect("start the fake");
    let shim = RecordingShim::start(fake.socket(), &dir.at("shim.sock"), &dir.at("shim.log"))
        .expect("start the shim");
    let client = UnixIncus::new(shim.socket().to_path_buf());
    client.get("/1.0").expect("a GET through the shim");
    let refused = exchange(
        shim.socket(),
        b"POST /1.0 HTTP/1.1\r\nHost: arca\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status_of(&refused), 405);
    assert_eq!(shim.summary(), ShimSummary { get: 1, refused: 1 });
    assert!(!shim.log_failed());

    client
        .get("/1.0")
        .expect("the shim still serves after a summary");
    assert_eq!(shim.summary(), ShimSummary { get: 2, refused: 1 });
    assert_eq!(
        shim.stop().expect("stop the shim"),
        ShimSummary { get: 2, refused: 1 }
    );
}
