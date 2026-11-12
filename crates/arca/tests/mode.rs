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
        approval: None,
        loopback: None,
        page: None,
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

#[test]
fn a_localhost_pages_door_is_open_only_while_both_loopback_addresses_are_this_nodes() {
    use arca::handlers::LoopbackDoor;
    use std::io::{Error, ErrorKind};

    // A door with no page on it has no twin, and says nothing of a page.
    let plain = LoopbackDoor::new("127.0.0.1:7471".parse().expect("an address"));
    plain.report(None);
    assert!(plain.twin().is_none());
    assert_eq!(plain.page_refusal(), None);
    assert_eq!(
        plain.json(),
        serde_json::json!({ "addr": "127.0.0.1:7471", "open": true, "reason": null })
    );

    // The door of a localhost page: its twin is the same port on the other loopback address, whichever one the
    // config names. The machine's IPv6 addresses are read from a file of this test's own.
    let scratch = Scratch::new("door-inet6");
    let table = scratch.path("if_inet6");
    let door = LoopbackDoor::new("127.0.0.1:7471".parse().expect("an address"))
        .with_inet6_table(table.clone())
        .with_twin();
    let twin = door.twin().expect("a twin");
    assert_eq!(twin.addr().to_string(), "[::1]:7471");
    assert_eq!(
        LoopbackDoor::new("[::1]:7471".parse().expect("an address"))
            .with_twin()
            .twin()
            .expect("a twin")
            .addr()
            .to_string(),
        "127.0.0.1:7471"
    );

    // Before either bind has succeeded the page is closed.
    assert!(door.page_refusal().is_some());
    // The door alone is not the page: another program on the twin would be the page, at the page's own origin.
    door.report(None);
    twin.report(Some(&Error::from(ErrorKind::AddrInUse)));
    let why = door.page_refusal().expect("the page is closed");
    assert!(why.contains("does not hold [::1]:7471"), "{why}");
    assert!(why.contains("address in use"), "{why}");
    assert!(why.contains("http://localhost:7471"), "{why}");
    let status = door.json();
    assert_eq!(
        status["open"],
        serde_json::json!(true),
        "the door itself is open"
    );
    assert_eq!(status["page"]["open"], serde_json::json!(false));
    assert_eq!(status["page"]["reason"], serde_json::json!(why));
    // Both held: the page is open.
    twin.report(None);
    assert_eq!(door.page_refusal(), None);
    assert_eq!(
        door.json()["page"],
        serde_json::json!({ "open": true, "reason": null })
    );
    // The twin held and the door not: closed again, naming the door.
    door.report(Some(&Error::from(ErrorKind::AddrInUse)));
    assert!(
        door.page_refusal()
            .expect("closed")
            .contains("does not hold 127.0.0.1:7471")
    );
    door.report(None);

    // A machine with no IPv6 loopback: the twin's address is not on the machine (EADDRNOTAVAIL, 99), or the kernel
    // has no such family (EAFNOSUPPORT, 97). No program can bind it and no browser can reach it, so it is nobody's
    // to hold and the page is open on the one address there is. The error alone is not believed: the kernel's list
    // of addresses must agree. It agrees when it is not there at all (a kernel with no IPv6), and when it lists
    // other addresses and no ::1.
    let link_local = "fe800000000000000000000000000001 02 40 20 80     eth0\n";
    let with_loopback =
        format!("00000000000000000000000000000001 01 80 10 80       lo\n{link_local}");
    for missing in [99, 97] {
        let _ = std::fs::remove_file(&table);
        twin.report(None);
        twin.report(Some(&Error::from_raw_os_error(missing)));
        assert_eq!(door.page_refusal(), None, "os error {missing}, no list");
        std::fs::write(&table, link_local).expect("the list");
        twin.report(None);
        twin.report(Some(&Error::from_raw_os_error(missing)));
        assert_eq!(
            door.page_refusal(),
            None,
            "os error {missing}, no ::1 listed"
        );

        // The same error on a machine that has ::1 is the node's own doing (restricted address families, a
        // network namespace of its own): the address is there for another program to take, so the page is closed.
        std::fs::write(&table, &with_loopback).expect("the list");
        twin.report(Some(&Error::from_raw_os_error(missing)));
        let why = door.page_refusal().expect("closed");
        assert!(why.contains("does not hold [::1]:7471"), "{why}");
        assert!(why.contains("the machine has the address"), "{why}");
        assert_eq!(door.json()["page"]["open"], serde_json::json!(false));

        // A list that is there and cannot be read says nothing either way: closed, and the reason names the file.
        std::fs::remove_file(&table).expect("remove the list");
        std::fs::create_dir(&table).expect("something that is not a readable file");
        twin.report(None);
        twin.report(Some(&Error::from_raw_os_error(missing)));
        let why = door.page_refusal().expect("closed");
        assert!(why.contains("cannot be read"), "{why}");
        assert!(why.contains("if_inet6"), "{why}");
        std::fs::remove_dir(&table).expect("remove it");
    }
    // Any other failure of the twin's bind is a door this node does not hold.
    for held_elsewhere in [98, 13] {
        twin.report(Some(&Error::from_raw_os_error(held_elsewhere)));
        assert!(door.page_refusal().is_some(), "os error {held_elsewhere}");
    }
    // The door's own address missing is never "nobody's": the door is closed, and so is the page.
    twin.report(None);
    door.report(Some(&Error::from_raw_os_error(99)));
    assert!(door.page_refusal().is_some());
    assert_eq!(door.json()["open"], serde_json::json!(false));
}
