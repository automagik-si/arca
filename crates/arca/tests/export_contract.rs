//! The export contract, served. `GET /metrics` and `GET /alerts` run through a real server over a unix socket, and their bodies
//! are compared byte for byte with the golden files. The node id is 64 zeros. Host samples are built by hand, so no host file or
//! Incus is read. Tokens and stores are in temporary directories.
//!
//! The scenario runs an engine every 5 s for 185 s: a pool is full until 60 s and half full after 65 s, the CPU is hot throughout,
//! and two rules have no input, so they are unknown.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use arca::handlers::{
    AlertsExport, AlertsHandler, AlertsPublished, MetricsExport, Published, WatchHandler, now_ms,
    watcher_step,
};
use arca_alerts::eval::{
    Component, ComponentName, Inputs, InstanceUse, PoolUse, Rules, Timed, WatchReading,
};
use arca_alerts::health::alerts_line;
use arca_alerts::{AlertSnapshot, Engine, export};
use arca_api::{Scope, conforms, verb, verbs};
use arca_incus::{FakeIncus, FakeState};
use arca_mcp::{
    CallError, Caller, GetHandler, GetReply, Server, TokenStore, ToolHandler, mint, serve_unix,
};
use arca_metrics::{
    Cpu, HostSample, InstanceKind, InstanceReading, Load, Memory, NicCounters, Pool, ProjectCount,
    Psi, Readings,
};
use arca_store::{AlertEvent, DB_FILE, EventState, ResolveReason, Store};
use common::{NOW, Scratch, caller};
use serde_json::{Value, json};

/// The fixture node id: 64 zeros.
const NODE: &str = "0000000000000000000000000000000000000000000000000000000000000000";
/// One day, in milliseconds.
const DAY_MS: u64 = 24 * 60 * 60 * 1_000;

/// A `GET`, `POST` or other exchange on `socket`: the status, the head, and the body. `headers` are complete lines ending in CRLF.
fn send(
    socket: &Path,
    method: &str,
    path: &str,
    headers: &str,
    body: &str,
) -> (u16, String, String) {
    let mut stream = UnixStream::connect(socket).expect("connect to the server");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set a deadline");
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: arca\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .expect("send the request");
    let mut reply = String::new();
    stream.read_to_string(&mut reply).expect("read the reply");
    let (head, body) = reply.split_once("\r\n\r\n").unwrap_or((reply.as_str(), ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .expect("a status line");
    (status, head.to_string(), body.to_string())
}

/// The bearer header line for `token`.
fn auth(token: &str) -> String {
    format!("Authorization: Bearer {token}\r\n")
}

/// A `tools/call` of `name` on the server, with its structured result.
fn mcp(socket: &Path, token: &str, name: &str, arguments: Value) -> Value {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments },
    })
    .to_string();
    let headers = format!("{}Content-Type: application/json\r\n", auth(token));
    let (status, _, text) = send(socket, "POST", "/mcp", &headers, &body);
    assert_eq!(status, 200, "{text}");
    let reply: Value = serde_json::from_str(&text).expect("the reply is JSON");
    assert!(reply.get("error").is_none(), "the call succeeded: {reply}");
    reply["result"]["structuredContent"].clone()
}

/// Starts a server with the routes `build` adds to a viewer-token server, on a unix socket in `scratch`. The server runs on its own
/// thread and a current-thread runtime, so only a handler that blocks that runtime can stall another request. Returns the socket and
/// the viewer's token.
fn start(scratch: &Scratch, build: impl FnOnce(Server) -> Server) -> (PathBuf, String) {
    let tokens = scratch.path("tokens");
    fs::create_dir(&tokens).expect("the token directory is created");
    fs::set_permissions(&tokens, fs::Permissions::from_mode(0o700)).expect("mode 0700");
    let (secret, text) = mint(Scope::Viewer);
    let hash = tokens.join("viewer.hash");
    fs::write(&hash, text).expect("write the hash file");
    fs::set_permissions(&hash, fs::Permissions::from_mode(0o400)).expect("mode 0400");
    let server = build(Server::new(
        verbs(),
        TokenStore::load(&tokens).expect("the tokens load"),
    ));
    let router = server.router();
    let socket = scratch.path("arca.sock");
    let served = socket.clone();
    thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        let _ = runtime.block_on(serve_unix(router, &served));
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while UnixStream::connect(&socket).is_err() {
        assert!(Instant::now() < deadline, "the socket never came up");
        thread::sleep(Duration::from_millis(20));
    }
    (socket, secret)
}

/// A directory of mode 0700 for a store.
fn state_dir(scratch: &Scratch) -> PathBuf {
    let dir = scratch.path("state");
    fs::create_dir(&dir).expect("the state directory is created");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("mode 0700");
    dir
}

/// A read-only connection to the store in `state`, shared as the handlers share it.
fn reader(state: &Path) -> Arc<Mutex<Store>> {
    Arc::new(Mutex::new(
        Store::open_read_only(state).expect("a reader opens"),
    ))
}

/// The host sample and readings of the scenario at `at`, with the pool `pool_used` bytes of 100 used.
fn sample_at(at: u64, pool_used: u64) -> (HostSample, Readings) {
    let sample = HostSample {
        at,
        cpu: Cpu {
            used_pct: Some(12.5),
            cores: 4,
        },
        load: Load {
            one: Some(0.5),
            five: Some(0.25),
            fifteen: Some(0.125),
        },
        memory: Memory {
            total_bytes: Some(8_589_934_592),
            available_bytes: Some(4_294_967_296),
            swap_total_bytes: Some(2_147_483_648),
            swap_free_bytes: Some(2_147_483_648),
        },
        psi: Psi {
            memory_some_avg10: Some(1.5),
            memory_full_avg10: Some(0.5),
            cpu_some_avg10: Some(2.0),
        },
        cpu_temp_c: Some(90.0),
        gpu_busy_pct: Some(10.0),
        pools: vec![Pool {
            name: "default".to_string(),
            used_bytes: Some(pool_used),
            total_bytes: Some(100),
            free_bytes: Some(100 - pool_used),
        }],
        network: None,
        projects: Vec::new(),
    };
    let readings = Readings {
        pools: Some(sample.pools.clone()),
        projects: Some(vec![ProjectCount {
            project: "proj1".to_string(),
            total: 3,
            running: 2,
        }]),
        nic: Some(NicCounters {
            rx_bytes: 1_000,
            tx_bytes: 2_000,
        }),
        instances: Some(scenario_instances()),
    };
    (sample, readings)
}

/// The instances of the scenario: a running container that has started twice, a stopped container that has started once, and a
/// running virtual machine. The stopped one reads no usage, so its CPU and memory series are left out.
fn scenario_instances() -> Vec<InstanceReading> {
    vec![
        InstanceReading {
            project: "proj1".to_string(),
            name: "db1".to_string(),
            kind: InstanceKind::Container,
            running: true,
            starts_total: 2,
            starts_10m: 0,
            cpu_seconds: Some(4.5),
            memory_bytes: Some(1_073_741_824),
        },
        InstanceReading {
            project: "proj1".to_string(),
            name: "web1".to_string(),
            kind: InstanceKind::Container,
            running: false,
            starts_total: 1,
            starts_10m: 0,
            cpu_seconds: None,
            memory_bytes: None,
        },
        InstanceReading {
            project: "proj1".to_string(),
            name: "vm1".to_string(),
            kind: InstanceKind::Vm,
            running: true,
            starts_total: 0,
            starts_10m: 0,
            cpu_seconds: Some(120.0),
            memory_bytes: Some(2_147_483_648),
        },
    ]
}

/// The tick's inputs for the scenario at `at`. The watch sweep reports two `entry_without_object` drifts and one `other`, which is
/// never exported.
fn inputs_at(at: u64, sample: &HostSample) -> Inputs {
    let pools = sample
        .pools
        .iter()
        .map(|pool| PoolUse {
            name: pool.name.clone(),
            used_bytes: pool.used_bytes,
            total_bytes: pool.total_bytes,
        })
        .collect();
    let components = [
        ComponentName::Incus,
        ComponentName::Arca,
        ComponentName::Watch,
    ]
    .map(|name| Component { name, ok: true })
    .to_vec();
    Inputs {
        at,
        pools: Some(Timed { at, value: pools }),
        memory_some_avg10: sample
            .psi
            .memory_some_avg10
            .map(|value| Timed { at, value }),
        cpu_temp_c: sample.cpu_temp_c.map(|value| Timed { at, value }),
        components: Some(Timed {
            at,
            value: components,
        }),
        watch: Some(Timed {
            at,
            value: WatchReading {
                drift: BTreeMap::from([
                    ("entry_without_object".to_string(), 2),
                    ("marked_without_entry".to_string(), 0),
                    ("other".to_string(), 1),
                ]),
            },
        }),
        ..Inputs::default()
    }
}

/// Runs the scenario on the store in `state` and returns the snapshot of its last tick, with `watch_at` set as the server sets it.
fn run(state: &Path) -> AlertSnapshot {
    let store = Store::open(state, NOW).expect("the store opens").store;
    let mut engine = Engine::start(store, Rules::default(), false, NOW).expect("the engine starts");
    let mut last = None;
    for at in (NOW..=NOW + 185_000).step_by(5_000) {
        let used = if at - NOW <= 60_000 { 95 } else { 50 };
        let (sample, readings) = sample_at(at, used);
        let mut inputs = inputs_at(at, &sample);
        inputs.instances = readings.instances.as_ref().map(|list| Timed {
            at,
            value: list
                .iter()
                .map(|instance| InstanceUse {
                    project: instance.project.clone(),
                    name: instance.name.clone(),
                    starts_10m: instance.starts_10m,
                })
                .collect(),
        });
        let points = export::points_of(&sample, &readings, &inputs);
        let mut snapshot = engine.tick(&inputs, points, at);
        snapshot.sample_at = Some(at);
        snapshot.watch_at = inputs.watch.as_ref().map(|watch| watch.at);
        last = Some(snapshot);
    }
    last.expect("the ticks ran")
}

/// A server over the scenario in a new store: `/metrics`, `/alerts` and `alerts_list` answer its last tick.
fn scenario_server(scratch: &Scratch) -> (PathBuf, String) {
    let state = state_dir(scratch);
    let alerts = AlertsPublished::default();
    alerts.publish(run(&state));
    let reader = reader(&state);
    start(scratch, |server| {
        server
            .handle(
                "alerts_list",
                Arc::new(AlertsHandler {
                    alerts: alerts.clone(),
                    published: Published::default(),
                    store: Arc::clone(&reader),
                    node: NODE.to_string(),
                }),
            )
            .get_route(
                "/alerts",
                Arc::new(AlertsExport {
                    node: NODE.to_string(),
                    store: reader,
                }),
            )
            .get_route(
                "/metrics",
                Arc::new(MetricsExport {
                    node: NODE.to_string(),
                    alerts: alerts.clone(),
                }),
            )
    })
}

fn golden(name: &str) -> String {
    fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/golden")
            .join(name),
    )
    .expect("the golden file is read")
}

/// Checks every line of `body` as text exposition 0.0.4, with the contract's rules: `node` is the first label of every series, no
/// label is `instance`, and no `null` is written.
fn assert_exposition(body: &str) {
    assert!(
        !body.contains("null"),
        "no null reading is written:\n{body}"
    );
    for line in body.lines() {
        if let Some(help) = line.strip_prefix("# HELP ") {
            assert!(help.split_once(' ').is_some(), "{line}");
            continue;
        }
        if let Some(kind) = line.strip_prefix("# TYPE ") {
            let kind = kind.rsplit_once(' ').map(|(_, kind)| kind);
            assert!(matches!(kind, Some("gauge" | "counter")), "{line}");
            continue;
        }
        let (series, value) = line.rsplit_once(' ').expect("a sample line has a value");
        value
            .parse::<f64>()
            .unwrap_or_else(|_| panic!("the value is a number: {line}"));
        let (name, labels) = series.split_once('{').expect("a series has labels");
        let labels = labels.strip_suffix('}').expect("the labels close");
        assert!(
            name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "{line}"
        );
        assert!(
            labels.starts_with("node=\""),
            "node is the first label: {line}"
        );
        for pair in labels.split(',') {
            let (key, value) = pair.split_once('=').expect("a label is key=value");
            assert!(!key.is_empty() && key != "instance", "{line}");
            assert!(value.starts_with('"') && value.ends_with('"'), "{line}");
        }
    }
}

#[test]
fn metrics_and_alerts_refuse_no_token_an_unknown_token_and_an_origin() {
    let scratch = Scratch::new("refusals");
    let (socket, viewer) = scenario_server(&scratch);
    for path in ["/metrics", "/alerts"] {
        assert_eq!(
            send(&socket, "GET", path, "", "").0,
            401,
            "{path} without a token"
        );
        assert_eq!(
            send(&socket, "GET", path, &auth("arca_unknown"), "").0,
            401,
            "{path} with an unknown token"
        );
        let origin = format!("{}Origin: https://192.0.2.1\r\n", auth(&viewer));
        assert_eq!(
            send(&socket, "GET", path, &origin, "").0,
            403,
            "{path} with an Origin"
        );
    }
    let no_token_origin = "Origin: https://192.0.2.1\r\n";
    assert_eq!(send(&socket, "GET", "/metrics", no_token_origin, "").0, 403);
}

#[test]
fn metrics_is_the_golden_text_and_parses_as_exposition() {
    let scratch = Scratch::new("metrics");
    let (socket, viewer) = scenario_server(&scratch);
    let (status, head, body) = send(&socket, "GET", "/metrics", &auth(&viewer), "");
    assert_eq!(status, 200, "{body}");
    assert!(
        head.to_ascii_lowercase()
            .contains("content-type: text/plain; version=0.0.4"),
        "{head}"
    );
    assert_exposition(&body);
    assert!(
        !body.contains("{instance=") && !body.contains(",instance="),
        "no series is labelled instance; incus_instance is a different label"
    );
    assert!(
        !body.contains("kind=\"other\""),
        "an unnamed drift kind is never a label"
    );
    assert_eq!(body, golden("metrics.txt"));
}

#[test]
fn the_scenario_health_line_shows_its_unknown_rules_as_a_count() {
    let scratch = Scratch::new("scenario-line");
    let state = state_dir(&scratch);
    let snapshot = run(&state);
    // The two rules with no input (update-failed, smart-warning) stay unknown, and the line counts them.
    assert_eq!(
        alerts_line(&snapshot, NOW + 185_000),
        "alerts 2 firing: cpu-hot, drift · 2 unknown"
    );
}

#[test]
fn alerts_pages_are_the_golden_file_and_a_bad_cursor_is_400() {
    let scratch = Scratch::new("alerts-page");
    let (socket, viewer) = scenario_server(&scratch);
    let (status, head, body) = send(&socket, "GET", "/alerts?after=0", &auth(&viewer), "");
    assert_eq!(status, 200, "{body}");
    assert!(
        head.to_ascii_lowercase()
            .contains("content-type: application/x-ndjson"),
        "{head}"
    );
    assert_eq!(body, golden("alerts.ndjson"));
    assert_eq!(
        send(&socket, "GET", "/alerts", &auth(&viewer), "").2,
        body,
        "a missing after is 0"
    );

    let last: Value = serde_json::from_str(body.lines().last().expect("events were written"))
        .expect("a JSON event");
    let last_seq = last["seq"].as_u64().expect("an event has a seq");
    let (status, _, after_last) = send(
        &socket,
        "GET",
        &format!("/alerts?after={last_seq}"),
        &auth(&viewer),
        "",
    );
    assert_eq!(
        (status, after_last.as_str()),
        (200, ""),
        "nothing after the last seq"
    );

    let (status, _, _) = send(&socket, "GET", "/alerts?after=x", &auth(&viewer), "");
    assert_eq!(status, 400, "a non-integer after is refused");
}

#[test]
fn a_page_holds_at_most_500_events_and_the_next_page_takes_the_rest() {
    let scratch = Scratch::new("paging");
    let state = state_dir(&scratch);
    let mut store = Store::open(&state, NOW).expect("the store opens").store;
    let events: Vec<AlertEvent> = (0..600)
        .map(|index| AlertEvent {
            seq: None,
            alert_id: None,
            rule: "disk-pool-full".to_string(),
            severity: "critical".to_string(),
            state: EventState::Firing,
            labels: BTreeMap::from([("pool".to_string(), format!("p{index:03}"))]),
            value: Some(95.0),
            threshold: Some(90.0),
            started_at: NOW,
            resolved_at: None,
            reason: None,
            at: NOW,
        })
        .collect();
    store
        .commit_tick(NOW, &[], &events)
        .expect("the events commit");
    drop(store);
    let (socket, viewer) = start(&scratch, |server| {
        server.get_route(
            "/alerts",
            Arc::new(AlertsExport {
                node: NODE.to_string(),
                store: reader(&state),
            }),
        )
    });

    let (_, _, first) = send(&socket, "GET", "/alerts?after=0", &auth(&viewer), "");
    let first: Vec<Value> = first
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSON"))
        .collect();
    assert_eq!(first.len(), 500, "the first page holds 500");
    let cursor = first.last().expect("a last event")["seq"]
        .as_u64()
        .expect("a seq");

    let (_, _, second) = send(
        &socket,
        "GET",
        &format!("/alerts?after={cursor}"),
        &auth(&viewer),
        "",
    );
    let second: Vec<Value> = second
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSON"))
        .collect();
    assert_eq!(second.len(), 100, "the second page holds the other 100");
    assert_eq!(
        second[0]["seq"].as_u64(),
        Some(cursor + 1),
        "the second page starts after the first"
    );
    let end = second.last().expect("a last event")["seq"]
        .as_u64()
        .expect("a seq");
    let (_, _, third) = send(
        &socket,
        "GET",
        &format!("/alerts?after={end}"),
        &auth(&viewer),
        "",
    );
    assert!(third.is_empty(), "no event after the last");
}

/// A GET route that holds its reply until the test releases it.
struct Held {
    started: Sender<()>,
    release: Mutex<Receiver<()>>,
}

impl GetHandler for Held {
    fn get(&self, _query: Option<&str>, _caller: &Caller) -> Result<GetReply, CallError> {
        let _ = self.started.send(());
        let release = self.release.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = release.recv_timeout(Duration::from_secs(10));
        Ok(GetReply {
            content_type: "text/plain",
            body: b"held\n".to_vec(),
        })
    }
}

#[test]
fn a_held_get_route_does_not_stop_another_request() {
    let scratch = Scratch::new("held");
    let (started, started_rx) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    let held = Held {
        started,
        release: Mutex::new(release_rx),
    };
    let (socket, viewer) = start(&scratch, |server| {
        server.get_route("/held", Arc::new(held)).get_route(
            "/metrics",
            Arc::new(MetricsExport {
                node: NODE.to_string(),
                alerts: AlertsPublished::default(),
            }),
        )
    });
    let held_socket = socket.clone();
    let held_token = viewer.clone();
    let slow = thread::spawn(move || send(&held_socket, "GET", "/held", &auth(&held_token), ""));
    started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the held route started");

    let (status, _, _) = send(&socket, "GET", "/metrics", &auth(&viewer), "");
    let _ = release.send(());
    assert_eq!(status, 200, "/metrics answers while /held is held");
    let (held_status, _, held_body) = slow.join().expect("the held request ends");
    assert_eq!((held_status, held_body.as_str()), (200, "held\n"));
}

#[test]
fn the_handler_health_line_agrees_with_alerts_line_on_four_snapshots() {
    let scratch = Scratch::new("health-agree");
    let state = state_dir(&scratch);
    let _ = Store::open(&state, now_ms()).expect("the store opens");
    let now = now_ms();
    let firing = |rule: &str| arca_alerts::FiringAlert {
        event: AlertEvent {
            seq: Some(1),
            alert_id: Some(1),
            rule: rule.to_string(),
            severity: "critical".to_string(),
            state: EventState::Firing,
            labels: BTreeMap::new(),
            value: Some(95.0),
            threshold: Some(90.0),
            started_at: now,
            resolved_at: None,
            reason: None,
            at: now,
        },
        unknown: false,
    };
    let unknown = |rule: &str| arca_alerts::UnknownAlert {
        rule: rule.to_string(),
        labels: BTreeMap::new(),
        state: arca_alerts::AlertState::Ok,
        reason: arca_alerts::UnknownReason::Missing,
    };
    let snapshot = |age: u64,
                    firing: Vec<arca_alerts::FiringAlert>,
                    unknown: Vec<arca_alerts::UnknownAlert>| AlertSnapshot {
        at: now,
        sample_at: Some(now - age),
        firing,
        unknown,
        ..AlertSnapshot::default()
    };
    let cases = [
        (snapshot(0, vec![], vec![]), "alerts none", 0, 0),
        (
            snapshot(0, vec![firing("disk-pool-full"), firing("cpu-hot")], vec![]),
            "alerts 2 firing: disk-pool-full, cpu-hot",
            2,
            0,
        ),
        (
            snapshot(
                0,
                vec![firing("disk-pool-full")],
                vec![unknown("update-failed")],
            ),
            "alerts 1 firing: disk-pool-full · 1 unknown",
            1,
            1,
        ),
        (
            snapshot(42_000, vec![firing("disk-pool-full")], vec![]),
            "alerts stale 42s",
            1,
            0,
        ),
    ];
    let alerts = AlertsPublished::default();
    let handler = AlertsHandler {
        alerts: alerts.clone(),
        published: Published::default(),
        store: reader(&state),
        node: NODE.to_string(),
    };
    for (snapshot, line, firing_count, unknown_count) in cases {
        assert_eq!(
            alerts_line(&snapshot, now),
            line,
            "the line of the snapshot"
        );
        alerts.publish(snapshot);
        let list = handler
            .call(json!({}), &caller(Scope::Viewer))
            .expect("alerts_list answers");
        assert_eq!(list["healthLine"], json!(line));
        assert_eq!(list["firing"].as_array().map(Vec::len), Some(firing_count));
        assert_eq!(
            list["unknown"].as_array().map(Vec::len),
            Some(unknown_count)
        );
    }
}

#[test]
fn alerts_list_reads_the_resolves_of_the_last_day_newest_first() {
    let scratch = Scratch::new("resolves");
    let state = state_dir(&scratch);
    let mut store = Store::open(&state, now_ms())
        .expect("the store opens")
        .store;
    let now = now_ms();
    // Three firing events, each resolved: one a day and a half ago, and two in the last hour.
    let mut resolved_at = Vec::new();
    for (index, age) in [DAY_MS + DAY_MS / 2, 10_000, 5_000].into_iter().enumerate() {
        let started = now - age - 1_000;
        let firing = AlertEvent {
            seq: None,
            alert_id: None,
            rule: "cpu-hot".to_string(),
            severity: "warning".to_string(),
            state: EventState::Firing,
            labels: BTreeMap::from([("n".to_string(), index.to_string())]),
            value: Some(90.0),
            threshold: Some(85.0),
            started_at: started,
            resolved_at: None,
            reason: None,
            at: started,
        };
        let seq = store.commit_tick(started, &[], &[firing]).expect("commit")[0];
        let resolve = AlertEvent {
            seq: None,
            alert_id: Some(seq),
            rule: "cpu-hot".to_string(),
            severity: "warning".to_string(),
            state: EventState::Resolved,
            labels: BTreeMap::from([("n".to_string(), index.to_string())]),
            value: Some(70.0),
            threshold: Some(85.0),
            started_at: started,
            resolved_at: Some(now - age),
            reason: Some(ResolveReason::Cleared),
            at: now - age,
        };
        store
            .commit_tick(now - age, &[], &[resolve])
            .expect("commit");
        resolved_at.push(now - age);
    }
    drop(store);
    let alerts = AlertsPublished::default();
    let handler = AlertsHandler {
        alerts,
        published: Published::default(),
        store: reader(&state),
        node: NODE.to_string(),
    };
    let list = handler
        .call(json!({}), &caller(Scope::Viewer))
        .expect("alerts_list answers");
    let resolved = list["resolvedRecent"].as_array().expect("a list");
    assert_eq!(
        resolved.len(),
        2,
        "the resolve of a day and a half ago is out of the window"
    );
    assert_eq!(resolved[0]["at"], json!(resolved_at[2]), "newest first");
    assert_eq!(resolved[1]["at"], json!(resolved_at[1]));
    assert_eq!(resolved[0]["reason"], json!("cleared"));
    assert_eq!(resolved[0]["state"], json!("resolved"));
    assert_eq!(
        list["healthLine"],
        json!("alerts stale"),
        "no tick has published yet"
    );
    assert_eq!(list["watchAgeMs"], Value::Null, "no sweep is published yet");
    assert_eq!(list["storeError"], Value::Null);
    assert_eq!(list["commitMs"], Value::Null);
    assert!(list["storeBytes"].as_u64().is_some_and(|bytes| bytes > 0));
    let schema = (verb("alerts_list").expect("the verb exists").result)();
    assert_eq!(conforms(&schema, &list), Ok(()));
}

#[test]
fn alerts_list_keeps_the_firing_alerts_when_the_history_read_fails() {
    let scratch = Scratch::new("history-read");
    let state = state_dir(&scratch);
    let snapshot = run(&state);
    let firing_count = snapshot.firing.len();
    assert!(firing_count > 0, "the scenario has firing alerts");
    let alerts = AlertsPublished::default();
    alerts.publish(snapshot);
    let handler = AlertsHandler {
        alerts,
        published: Published::default(),
        store: reader(&state),
        node: NODE.to_string(),
    };
    // Dropping the table under the open reader makes its history read fail.
    rusqlite::Connection::open(state.join(DB_FILE))
        .expect("the database opens")
        .execute("DROP TABLE alert_events", [])
        .expect("the table drops");
    let list = handler
        .call(json!({}), &caller(Scope::Viewer))
        .expect("alerts_list still answers");
    assert_eq!(list["firing"].as_array().map(Vec::len), Some(firing_count));
    assert_eq!(list["resolvedRecent"], json!([]));
    assert!(
        list["storeError"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("could not be read")),
        "the read failure is reported: {}",
        list["storeError"]
    );
    assert!(list["healthLine"].is_string());
}

#[test]
fn watch_report_ages_from_its_sweep_and_alerts_list_conforms_over_the_server() {
    let scratch = Scratch::new("ages");
    let fake = FakeIncus::start(
        &scratch.dir,
        FakeState {
            server: json!({ "api_version": "1.0" }),
            projects: vec![json!({ "name": "default", "description": "", "config": {} })],
            instances: Vec::new(),
            networks: Vec::new(),
            pools: Vec::new(),
        },
    )
    .expect("start the fake Incus");
    let watch = Arc::new(WatchHandler::new(
        fake.socket().to_path_buf(),
        BTreeSet::new(),
    ));
    let mut last_start = None;
    assert!(
        watcher_step(&watch, NOW, &mut last_start),
        "the first sweep runs"
    );

    let state = state_dir(&scratch);
    let alerts = AlertsPublished::default();
    let snapshot = run(&state);
    let taken = snapshot.watch_at.expect("the scenario read a sweep");
    alerts.publish(snapshot);
    let reader = reader(&state);
    let (socket, viewer) = start(&scratch, |server| {
        server
            .handle("watch_report", watch.clone() as Arc<dyn ToolHandler>)
            .handle(
                "alerts_list",
                Arc::new(AlertsHandler {
                    alerts: alerts.clone(),
                    published: Published::default(),
                    store: Arc::clone(&reader),
                    node: NODE.to_string(),
                }),
            )
    });

    let before = now_ms();
    let report = mcp(&socket, &viewer, "watch_report", json!({}));
    let after = now_ms();
    let age = report["ageMs"].as_u64().expect("the age is a whole number");
    assert!(
        (before.saturating_sub(NOW)..=after.saturating_sub(NOW)).contains(&age),
        "the age counts from the sweep at NOW: {age}"
    );

    let before = now_ms();
    let list = mcp(&socket, &viewer, "alerts_list", json!({}));
    let after = now_ms();
    let watch_age = list["watchAgeMs"].as_u64().expect("the watch age is set");
    assert!(
        (before.saturating_sub(taken)..=after.saturating_sub(taken)).contains(&watch_age),
        "the watch age counts from the tick's sweep: {watch_age}"
    );
    let schema = (verb("alerts_list").expect("the verb exists").result)();
    assert_eq!(conforms(&schema, &list), Ok(()));
    assert_eq!(
        list["firing"].as_array().map(Vec::len),
        Some(2),
        "cpu-hot and drift fire"
    );
    assert_eq!(
        list["unknown"].as_array().map(Vec::len),
        Some(2),
        "two rules have no input"
    );
}
