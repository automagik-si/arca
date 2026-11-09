//! The runtime host mode: the explicit config wins, otherwise the Incus server name decides, and the sampler follows the
//! choice. A fake Incus carries the server name; the local host name is the one `/proc` reports.

mod common;

use std::path::Path;

use std::sync::{Arc, Mutex};
use std::time::Instant;

use arca::config::NodeConfig;
use arca::handlers::{StatusHandler, new_sampler};
use arca::kernel::AppliedSeq;
use arca_api::Scope;
use arca_incus::{FakeIncus, FakeState};
use arca_mcp::{Caller, ToolHandler};
use arca_metrics::{HostMode, HostModeChoice};
use common::Scratch;
use serde_json::json;

/// A node config whose Incus socket is `socket`, with an optional explicit `local`.
fn config(socket: &Path, local: Option<bool>) -> NodeConfig {
    let mut incus = json!({ "socket": socket.display().to_string(), "pools": [] });
    if let Some(local) = local {
        incus["local"] = json!(local);
    }
    serde_json::from_value(json!({
        "stateDir": "/nonexistent/state",
        "incus": incus,
        "listen": { "unix": "/nonexistent/arca.sock" },
        "tokens": { "hashDir": "/nonexistent/tokens" },
        "kernel": { "droiddProjects": ["droid"] }
    }))
    .expect("the fixture config parses")
}

/// A fake Incus whose `/1.0` names `server_name` and holds no object.
fn fake(scratch: &Scratch, server_name: &str) -> FakeIncus {
    FakeIncus::start(
        &scratch.dir,
        FakeState {
            server: json!({
                "api_version": "1.0",
                "environment": { "server_name": server_name },
            }),
            projects: Vec::new(),
            instances: Vec::new(),
            networks: Vec::new(),
            pools: Vec::new(),
        },
    )
    .expect("the fake serves")
}

/// The host name `/proc` reports, which the mode probe compares with the Incus server name.
fn local_hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .expect("the local host name")
        .trim()
        .to_string()
}

/// The current shared mode choice.
fn chosen(shared: &Arc<Mutex<HostModeChoice>>) -> HostModeChoice {
    shared.lock().expect("the choice is not poisoned").clone()
}

fn resource_reads(fake: &FakeIncus) -> usize {
    fake.requests()
        .into_iter()
        .filter(|request| request.path == "/1.0/resources")
        .count()
}

#[test]
fn a_shim_style_socket_to_the_local_daemon_is_local_and_keeps_proc_readings() {
    let scratch = Scratch::new("mode-local");
    let fake = fake(&scratch, &local_hostname());
    let (mut sampler, choice) = new_sampler(&config(fake.socket(), None));

    let choice = chosen(&choice);
    assert_eq!(choice.mode, HostMode::Local);
    assert_eq!(
        choice.reason,
        "the Incus server name matches the local host name"
    );
    let sample = sampler.sample(1_000).clone();
    assert!(sample.cpu.cores_known, "the local /proc still counts cores");
    assert_eq!(sample.cpu.reason, None);
    assert!(sample.load.one.is_some(), "the local load is still read");
    assert_eq!(
        resource_reads(&fake),
        1,
        "a local node asks Incus once, for the GPU inventory, and never for host totals"
    );
}

#[test]
fn a_proxy_to_another_host_is_remote_and_reports_no_local_reading() {
    let scratch = Scratch::new("mode-remote");
    let fake = fake(&scratch, "another-host");
    let (mut sampler, choice) = new_sampler(&config(fake.socket(), None));

    let choice = chosen(&choice);
    assert_eq!(choice.mode, HostMode::Remote);
    assert_eq!(
        choice.reason,
        "the Incus server name does not match the local host name"
    );
    let sample = sampler.sample(1_000).clone();
    assert!(!sample.cpu.cores_known);
    assert_eq!(
        sample.cpu.reason.as_deref(),
        Some("the Incus resources read failed")
    );
    assert_eq!(sample.load.one, None);
    assert_eq!(
        resource_reads(&fake),
        2,
        "one resources read for the remote host totals and one for the GPU inventory"
    );
}

#[test]
fn an_explicit_local_or_remote_wins_over_the_server_name() {
    let scratch = Scratch::new("mode-explicit");
    let fake = fake(&scratch, &local_hostname());

    let (_, forced_remote) = new_sampler(&config(fake.socket(), Some(false)));
    assert_eq!(chosen(&forced_remote).mode, HostMode::Remote);
    assert_eq!(chosen(&forced_remote).reason, "incus.local is false");

    let (_, forced_local) = new_sampler(&config(fake.socket(), Some(true)));
    assert_eq!(chosen(&forced_local).mode, HostMode::Local);
    assert_eq!(chosen(&forced_local).reason, "incus.local is true");
}

#[test]
fn the_status_exposes_the_chosen_mode_and_reason() {
    let scratch = Scratch::new("mode-status");
    let fake = fake(&scratch, "another-host");
    let (_, choice) = new_sampler(&config(fake.socket(), None));
    let status = StatusHandler {
        node: "node1".to_string(),
        mode: "watch-only",
        applied_seq: AppliedSeq::default(),
        started: Instant::now(),
        socket: fake.socket().to_path_buf(),
        metrics_source: choice,
    };
    let caller = Caller {
        token_id: "viewer".to_string(),
        scope: Scope::Viewer,
        tools: None,
    };
    let row = status.call(json!({}), &caller).expect("status answers");
    assert_eq!(row["metricsSource"], json!("remote"));
    assert_eq!(
        row["metricsSourceReason"],
        json!("the Incus server name does not match the local host name")
    );
}
