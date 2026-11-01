//! The alert tracer: one pool over 90 % becomes a firing `disk-pool-full` alert through the sampler thread's tick, on a
//! synthetic clock. The alert is committed to `arca.db`, listed by `alerts_list`, exported on `GET /metrics` behind the
//! bearer check, and loaded again as firing by a restarted engine. Incus is a fake, and every file is in a temporary
//! directory.

mod common;

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arca::handlers::{AlertsHandler, AlertsPublished, MetricsExport, Published, Ticker};
use arca_alerts::Engine;
use arca_alerts::eval::{Inputs, Rules};
use arca_api::{Scope, conforms, verb, verbs};
use arca_incus::{FakeIncus, FakeState, UnixIncus};
use arca_mcp::{Server, TokenStore, ToolHandler, mint, serve_unix};
use arca_metrics::{Sampler, SamplerConfig};
use arca_store::Store;
use common::{NOW, Scratch, caller};
use serde_json::json;

/// The fixture node id.
const NODE: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// A fake Incus with one pool, `default`, 95 of its 100 bytes used.
fn fake_state() -> FakeState {
    FakeState {
        server: json!({ "api_version": "1.0" }),
        projects: Vec::new(),
        instances: Vec::new(),
        networks: Vec::new(),
        pools: vec![(
            "default".to_string(),
            json!({ "space": { "used": 95, "total": 100 } }),
        )],
    }
}

/// A directory of mode 0700 inside the scratch directory.
fn private_dir(scratch: &Scratch, name: &str) -> PathBuf {
    let dir = scratch.path(name);
    fs::create_dir(&dir).expect("the directory is created");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("mode 0700");
    dir
}

/// `GET <path>` on the unix socket, with a bearer when one is given: the status and the body.
fn get(socket: &Path, path: &str, bearer: Option<&str>) -> (u16, String) {
    let mut stream = UnixStream::connect(socket).expect("connect to the server");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set a deadline");
    let auth = bearer.map_or(String::new(), |token| {
        format!("Authorization: Bearer {token}\r\n")
    });
    let request = format!("GET {path} HTTP/1.1\r\nHost: arca\r\n{auth}Connection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .expect("send the request");
    let mut reply = String::new();
    stream.read_to_string(&mut reply).expect("read the reply");
    let status = reply
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .expect("a status line");
    let body = reply.split_once("\r\n\r\n").map_or("", |(_, body)| body);
    (status, body.to_string())
}

#[test]
fn a_full_pool_fires_at_60_s_and_reaches_the_store_alerts_list_and_metrics() {
    let scratch = Scratch::new("alerts-tracer");
    let state_dir = private_dir(&scratch, "state");
    let fake = FakeIncus::start(&scratch.dir, fake_state()).expect("start the fake Incus");
    let sampler = Sampler::new(
        SamplerConfig {
            proc_root: scratch.path("proc"),
            sys_root: scratch.path("sys"),
            pools: vec!["default".to_string()],
            interval_ms: 5_000,
            history_minutes: 60,
        },
        UnixIncus::new(fake.socket().to_path_buf()),
    );
    let store = Store::open(&state_dir, NOW).expect("the store opens").store;
    let engine = Engine::start(store, Rules::default(), false, NOW).expect("the engine starts");
    let (published, alerts) = (Published::default(), AlertsPublished::default());
    let mut ticker = Ticker::new(sampler, engine, published.clone(), alerts.clone());

    // The pool is over 90 % from t=0, so the alert is pending until it has held 60 s.
    for t in (0..=55).step_by(5) {
        ticker.tick(NOW + t * 1_000);
        assert!(alerts.current().firing.is_empty(), "not firing at {t} s");
    }
    ticker.tick(NOW + 60_000);
    let snapshot = alerts.current();
    assert_eq!(snapshot.firing.len(), 1, "firing at 60 s");
    let event = &snapshot.firing[0].event;
    assert_eq!(
        (event.rule.as_str(), event.severity.as_str()),
        ("disk-pool-full", "critical")
    );
    assert_eq!(
        event.labels.get("pool").map(String::as_str),
        Some("default")
    );
    assert_eq!((event.value, event.at), (Some(95.0), NOW + 60_000));
    let seq = event.seq.expect("the committed event has a seq");
    assert!(snapshot.store_error.is_none());

    // alerts_list lists it with its seq, in the verb's result schema.
    let reader = Store::open_read_only(&state_dir).expect("a reader opens");
    let handler = AlertsHandler {
        alerts: alerts.clone(),
        published: published.clone(),
        store: Arc::new(Mutex::new(reader)),
        node: NODE.to_string(),
    };
    let list = handler
        .call(json!({}), &caller(Scope::Viewer))
        .expect("alerts_list answers");
    let schema = (verb("alerts_list").expect("the verb exists").result)();
    assert_eq!(conforms(&schema, &list), Ok(()));
    let listed = &list["firing"][0];
    assert_eq!(listed["seq"], json!(seq));
    assert_eq!(listed["alertId"], json!(seq));
    assert_eq!(listed["rule"], "disk-pool-full");
    assert_eq!(listed["labels"], json!({ "pool": "default" }));
    assert_eq!(listed["unknown"], false);
    assert!(list["storeBytes"].as_u64().is_some_and(|bytes| bytes > 0));

    // A read-only connection finds it firing in arca.db.
    let stored = Store::open_read_only(&state_dir)
        .expect("a reader opens")
        .firing()
        .expect("the firing alerts are read");
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].seq, Some(seq));

    // GET /metrics answers a viewer with the alert and the pool, and refuses a request without a token.
    let tokens = private_dir(&scratch, "tokens");
    let (secret, text) = mint(Scope::Viewer);
    let hash = tokens.join("viewer.hash");
    fs::write(&hash, text).expect("write the hash file");
    fs::set_permissions(&hash, fs::Permissions::from_mode(0o400)).expect("mode 0400");
    let router = Server::new(verbs(), TokenStore::load(&tokens).expect("the tokens load"))
        .handle("alerts_list", Arc::new(handler))
        .get_route(
            "/metrics",
            Arc::new(MetricsExport {
                node: NODE.to_string(),
                alerts: alerts.clone(),
            }),
        )
        .router();
    let runtime = tokio::runtime::Runtime::new().expect("a runtime");
    let socket = scratch.path("arca.sock");
    let served = socket.clone();
    runtime.spawn(async move { serve_unix(router, &served).await });
    let deadline = Instant::now() + Duration::from_secs(5);
    while UnixStream::connect(&socket).is_err() {
        assert!(Instant::now() < deadline, "the socket never came up");
        std::thread::sleep(Duration::from_millis(20));
    }
    let (status, body) = get(&socket, "/metrics", Some(&secret));
    assert_eq!(status, 200, "{body}");
    let lines: Vec<&str> = body.lines().collect();
    for wanted in [
        format!(
            "arca_alert_firing{{node=\"{NODE}\",rule=\"disk-pool-full\",severity=\"critical\",pool=\"default\"}} 1"
        ),
        format!("arca_pool_used_bytes{{node=\"{NODE}\",pool=\"default\"}} 95"),
        format!("arca_pool_size_bytes{{node=\"{NODE}\",pool=\"default\"}} 100"),
        format!("arca_store_write_failures_total{{node=\"{NODE}\"}} 0"),
    ] {
        assert!(lines.contains(&wanted.as_str()), "{wanted} is in:\n{body}");
    }
    assert_eq!(get(&socket, "/metrics", None).0, 401);
    assert_eq!(get(&socket, "/metrics", Some("arca_unknown")).0, 401);

    // A restarted engine loads the alert as firing, and unknown until its first reading.
    drop(ticker);
    let later = NOW + 65_000;
    let store = Store::open(&state_dir, later)
        .expect("the store opens again")
        .store;
    let mut engine =
        Engine::start(store, Rules::default(), false, later).expect("the engine starts");
    let restarted = engine.tick(&Inputs::default(), Vec::new(), later);
    assert_eq!(restarted.firing.len(), 1);
    assert_eq!(restarted.firing[0].event.seq, Some(seq));
    assert!(
        restarted.firing[0].unknown,
        "firing and unknown after a restart"
    );
    drop(fake);
}

#[test]
fn store_check_prints_the_pragmas_of_a_scratch_store_and_leaves_no_file() {
    let scratch = Scratch::new("store-check");
    let state_dir = private_dir(&scratch, "state");
    let config = scratch.path("node.json");
    let text = json!({
        "stateDir": state_dir,
        "incus": { "socket": scratch.path("incus.sock") },
        "listen": { "unix": scratch.path("arca.sock") },
        "tokens": { "hashDir": scratch.path("tokens") },
        "kernel": { "droiddProjects": [] }
    });
    fs::write(&config, text.to_string()).expect("write the config");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_arca"))
        .args(["store", "check", "--config"])
        .arg(&config)
        .output()
        .expect("run arca");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let printed: serde_json::Value = serde_json::from_slice(&out.stdout).expect("one JSON object");
    assert_eq!(printed["journal_mode"], "wal");
    assert_eq!(printed["synchronous"], 2);
    assert_eq!(printed["auto_vacuum"], 2);
    assert!(
        printed["sqlite_version"]
            .as_str()
            .is_some_and(|v| v.starts_with("3."))
    );
    let left: Vec<_> = fs::read_dir(&state_dir)
        .expect("read the state directory")
        .collect();
    assert!(
        left.is_empty(),
        "no arca.db and no scratch file is left: {left:?}"
    );
}
