//! `arca adopt plan` and `arca adopt apply` against the fake Incus. Every refusal is checked twice: the command refuses with
//! exit 2 and names its reason, and the fake records no write.

mod common;

use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Command;

use arca::Failure;
use arca::adoptions::{AdoptionRecord, Phase};
use arca::desired_store::lock_state_dir;
use arca_incus::FakeIncus;
use arca_incus::adopt::AdoptStep;
use arca_kernel::ownership::{IncusKind, ObjectRef};
use common::{
    ADOPT_IP, ADOPT_MAC, ADOPT_TO, ADOPT_UUID, Scratch, adopt_config, adopt_digest, adopt_fake,
    adopt_instance, adopt_run, get,
};
use serde_json::{Value, json};

/// Plans `svc1` against a fake serving `instances`, after `setup`, and expects a refusal naming `reason` with no write past
/// the setup's own. An apply with any digest is refused too.
fn refused_after(
    tag: &str,
    instances: Vec<Value>,
    setup: &dyn Fn(&FakeIncus, &Path),
    args: &[&str],
    reason: &str,
) {
    let scratch = Scratch::new(tag);
    let fake = adopt_fake(&scratch, instances);
    let config = adopt_config(&scratch, &fake);
    setup(&fake, &config);
    let before = fake.writes().len();
    let mut plan = vec!["plan", "--instance", "svc1", "--json"];
    if !args.contains(&"--expect-ip") {
        plan.extend_from_slice(&["--expect-ip", ADOPT_IP]);
    }
    plan.extend_from_slice(args);
    let planned = adopt_run(&config, &plan);
    match &planned {
        Err(failure @ Failure::Refused(why)) => {
            assert_eq!(failure.code(), 2);
            assert!(why.contains(reason), "{tag}: {why}");
        }
        other => panic!("{tag}: the plan is refused, not {other:?}"),
    }
    let applied = adopt_run(
        &config,
        &[
            "apply",
            "--instance",
            "svc1",
            "--plan-digest",
            &"0".repeat(64),
        ],
    );
    assert!(
        matches!(applied, Err(Failure::Refused(_))),
        "{tag}: {applied:?}"
    );
    assert_eq!(fake.writes().len(), before, "{tag}: {:?}", fake.writes());
}

fn refused_with_no_write(tag: &str, instances: Vec<Value>, reason: &str) {
    refused_after(tag, instances, &|_, _| {}, &[], reason);
}

/// A request sent straight to the fake, past every check, for a test's own setup.
fn raw(fake: &FakeIncus, method: &str, path: &str, body: &Value) -> u16 {
    let text = body.to_string();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: incus\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        text.len()
    );
    let mut stream = UnixStream::connect(fake.socket()).expect("connect");
    stream.write_all(request.as_bytes()).expect("send");
    let mut reply = String::new();
    stream.read_to_string(&mut reply).expect("read");
    reply
        .get(9..12)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0)
}

fn svc1_with(edit: impl Fn(&mut Value)) -> Vec<Value> {
    let mut instance = adopt_instance("default", json!({}));
    edit(&mut instance);
    vec![instance]
}

fn apply(config: &Path, digest: &str, more: &[&str]) -> Result<Option<Value>, Failure> {
    let mut args = vec!["apply", "--instance", "svc1", "--plan-digest", digest];
    args.extend_from_slice(more);
    adopt_run(config, &args)
}

#[test]
fn a_virtual_machine_is_refused_with_no_write() {
    let mut vm = adopt_instance("default", json!({}));
    vm["type"] = json!("virtual-machine");
    refused_with_no_write("vm", vec![vm], "not a container");
}

#[test]
fn an_instance_in_bench_is_refused_with_no_write() {
    refused_with_no_write(
        "bench",
        vec![adopt_instance("bench", json!({}))],
        "not in project default",
    );
}

#[test]
fn a_droidd_owned_instance_is_refused_with_no_write() {
    let owned = adopt_instance("default", json!({ "user.droid.lease": "1" }));
    refused_with_no_write("droidd", vec![owned], "owned by droidd");
    let tactu = adopt_instance("default", json!({ "user.tactu.desktop": "1" }));
    refused_with_no_write("tactu", vec![tactu], "owned by droidd");
}

#[test]
fn an_instance_already_marked_by_the_kernel_is_refused_with_no_write() {
    let marked = adopt_instance("default", json!({ "user.arcakernel.entry": "adopt-svc1" }));
    refused_with_no_write("marked", vec![marked], "already carries a kernel mark");
    let tombstoned = adopt_instance("default", json!({ "user.arcakernel.retiring": "x" }));
    refused_with_no_write(
        "tombstoned",
        vec![tombstoned],
        "already carries a kernel mark",
    );
}

#[test]
fn every_other_preflight_refusal_names_its_reason_and_sends_no_write() {
    let nic = |extra: Value| {
        move |instance: &mut Value| {
            instance["devices"]["eth0"] =
                json!({ "type": "nic", "name": "eth0", "network": "br1" });
            if let Value::Object(extra) = extra.clone() {
                instance["devices"]["eth0"]
                    .as_object_mut()
                    .expect("eth0")
                    .extend(extra);
            }
        }
    };
    let cases: Vec<(&str, Vec<Value>, &str)> = vec![
        (
            "two-nics",
            svc1_with(|i| i["devices"]["eth1"] = json!({ "type": "nic", "network": "br1" })),
            "exactly one NIC",
        ),
        (
            "other-network",
            svc1_with(nic(json!({ "network": "br2" }))),
            "kernel.attachNetworks",
        ),
        (
            "nic-key",
            svc1_with(nic(json!({ "mtu": "1400" }))),
            "a key the pin would replace",
        ),
        (
            "nic-name",
            svc1_with(nic(json!({ "name": "enp5s0" }))),
            "a key the pin would replace",
        ),
        (
            "no-mac",
            svc1_with(|i| {
                i["config"]
                    .as_object_mut()
                    .expect("config")
                    .remove("volatile.eth0.hwaddr");
            }),
            "hwaddr",
        ),
        (
            "stopped",
            svc1_with(|i| i["status"] = json!("Stopped")),
            "running MAC",
        ),
        (
            "profile",
            svc1_with(|i| i["profiles"] = json!(["default", "extra"])),
            "profile other than default",
        ),
        (
            "root",
            svc1_with(|i| i["devices"] = json!({})),
            "root is not its own device",
        ),
        (
            "privileged",
            svc1_with(|i| i["config"]["security.privileged"] = json!("true")),
            "restricted project refuses",
        ),
        (
            "nesting",
            svc1_with(|i| i["config"]["security.nesting"] = json!("true")),
            "restricted project refuses",
        ),
        (
            "raw",
            svc1_with(|i| i["config"]["raw.lxc"] = json!("lxc.x=1")),
            "restricted project refuses",
        ),
        (
            "no-uuid",
            svc1_with(|i| {
                i["config"]
                    .as_object_mut()
                    .expect("config")
                    .remove("volatile.uuid");
            }),
            "volatile.uuid",
        ),
    ];
    for (tag, instances, reason) in cases {
        refused_with_no_write(tag, instances, reason);
    }
    let mut pinned = vec![adopt_instance("default", json!({}))];
    pinned.push(json!({ "name": "other", "project": "default", "type": "container", "status": "Stopped",
        "config": {}, "devices": { "eth0": { "type": "nic", "network": "br1", "ipv4.address": ADOPT_IP } } }));
    refused_with_no_write("device-claim", pinned, "claimed by another lease or device");
    let svc1 = || vec![adopt_instance("default", json!({}))];
    refused_after(
        "lease-claim",
        svc1(),
        &|fake, _| fake.set_lease("br1", "10:66:6a:00:00:99", ADOPT_IP),
        &[],
        "claimed",
    );
    refused_after(
        "other-ip",
        svc1(),
        &|_, _| {},
        &["--expect-ip", "192.0.2.99"],
        "--expect-ip",
    );
    refused_after(
        "held",
        svc1(),
        &|fake, _| {
            fake.hold_operation(&ObjectRef {
                kind: IncusKind::Instance,
                project: "default".to_string(),
                parent: None,
                name: "svc1".to_string(),
                uuid: None,
            });
        },
        &[],
        "an operation is running",
    );
    refused_after(
        "phone",
        svc1(),
        &|_, config| {
            let mut value: Value =
                serde_json::from_slice(&fs::read(config).expect("read")).expect("parse");
            value["identityPhones"] = json!(["svc1"]);
            fs::write(config, value.to_string()).expect("write");
        },
        &[],
        "identityPhones",
    );
    refused_after(
        "unmarked-project",
        svc1(),
        &|fake, _| {
            assert_eq!(
                raw(
                    fake,
                    "POST",
                    "/1.0/projects",
                    &json!({ "name": ADOPT_TO, "config": {} })
                ),
                201
            );
        },
        &[],
        "adoption project exists",
    );
    refused_after(
        "busy-project",
        svc1(),
        &|fake, _| {
            let marked =
                json!({ "name": ADOPT_TO, "config": { "user.arcakernel.entry": "adopt-svc1" } });
            assert_eq!(raw(fake, "POST", "/1.0/projects", &marked), 201);
            let other = json!({ "name": "c2", "type": "container", "config": {} });
            assert_eq!(
                raw(
                    fake,
                    "POST",
                    &format!("/1.0/instances?project={ADOPT_TO}"),
                    &other
                ),
                202
            );
        },
        &[],
        "adoption project exists",
    );

    // An instance the config does not list is refused before any read.
    let scratch = Scratch::new("unlisted");
    let fake = adopt_fake(&scratch, vec![adopt_instance("default", json!({}))]);
    let config = adopt_config(&scratch, &fake);
    let unlisted = adopt_run(
        &config,
        &[
            "plan",
            "--instance",
            "svc2",
            "--expect-ip",
            ADOPT_IP,
            "--json",
        ],
    );
    assert!(
        matches!(&unlisted, Err(Failure::Refused(why)) if why.contains("kernel.adopt")),
        "{unlisted:?}"
    );
    assert!(
        fake.requests().is_empty(),
        "nothing was read: {:?}",
        fake.requests()
    );
}

#[test]
fn a_stale_digest_is_refused_with_no_write() {
    let scratch = Scratch::new("stale");
    let fake = adopt_fake(&scratch, vec![adopt_instance("default", json!({}))]);
    let config = adopt_config(&scratch, &fake);
    let digest = adopt_digest(&config);
    // The instance changes after the plan, so its fresh plan has another digest.
    let path = "/1.0/instances/svc1?project=default";
    assert_eq!(
        raw(
            &fake,
            "PATCH",
            path,
            &json!({ "config": { "image.os": "other" } })
        ),
        200
    );
    let before = fake.writes().len();
    let stale = apply(&config, &digest, &[]);
    assert!(
        matches!(&stale, Err(Failure::Refused(why)) if why.contains("stale")),
        "{stale:?}"
    );
    assert_eq!(fake.writes().len(), before);
    assert!(
        !scratch.path("state/adoptions.jsonl").exists(),
        "nothing was recorded"
    );
}

#[test]
fn apply_refuses_while_another_process_holds_the_state_lock_and_sends_no_write() {
    let scratch = Scratch::new("locked");
    let fake = adopt_fake(&scratch, vec![adopt_instance("default", json!({}))]);
    let config = adopt_config(&scratch, &fake);
    let digest = adopt_digest(&config);
    // A second open of the lock file holds the lock as another process's open would: flock is per open file.
    let held = lock_state_dir(&scratch.path("state")).expect("the lock");
    let refused = apply(&config, &digest, &[]);
    assert!(
        matches!(&refused, Err(failure @ Failure::Runtime(why)) if why.contains("held") && failure.code() == 1),
        "{refused:?}"
    );
    assert!(fake.writes().is_empty());

    // The dry run takes no lock: it runs while the lock is held, prints every request and sends none.
    let dry = apply(&config, &digest, &["--dry-run"])
        .expect("the dry run")
        .expect("a result");
    assert_eq!(dry["dryRun"], json!(true));
    let requests = dry["requests"].as_array().expect("the requests");
    let steps: Vec<&str> = requests.iter().filter_map(|r| r["step"].as_str()).collect();
    assert_eq!(
        steps,
        [
            "createProject",
            "stop",
            "stop",
            "snapshot",
            "pinNic",
            "move",
            "mark",
            "start"
        ]
    );
    assert!(requests.iter().all(|r| r["method"] != "DELETE"));
    assert!(
        fake.writes().is_empty(),
        "the dry run sent no write: {:?}",
        fake.writes()
    );
    assert!(
        !scratch.path("state/adoptions.jsonl").exists(),
        "the dry run recorded nothing"
    );
    drop(held);
}

#[test]
fn plan_then_apply_adopts_the_instance_and_keeps_its_mac_ip_lease_and_config() {
    let scratch = Scratch::new("adopt");
    let fake = adopt_fake(&scratch, vec![adopt_instance("default", json!({}))]);
    let config = adopt_config(&scratch, &fake);
    let before = get(&fake, "/1.0/instances/svc1?project=default").expect("the instance");
    let digest = adopt_digest(&config);
    let applied = apply(&config, &digest, &[])
        .expect("the adoption")
        .expect("a result");
    assert_eq!(applied["complete"], json!(true));
    let snapshot = format!("arca-adopt-{}", &digest[..12]);
    assert_eq!(applied["snapshot"], json!(snapshot));

    let at = |path: &str| format!("{path}?project={ADOPT_TO}");
    assert!(
        get(&fake, "/1.0/instances/svc1?project=default").is_none(),
        "the instance left default"
    );
    let after =
        get(&fake, &at("/1.0/instances/svc1")).expect("the instance is in its adoption project");
    assert_eq!(after["status"], json!("Running"));
    assert_eq!(
        after["config"]["user.arcakernel.entry"],
        json!("adopt-svc1")
    );
    assert_eq!(after["config"]["volatile.uuid"], json!(ADOPT_UUID));
    let own = |body: &Value| -> Vec<(String, Value)> {
        body["config"]
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(key, _)| {
                !key.starts_with("volatile.") && key.as_str() != "user.arcakernel.entry"
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    };
    assert_eq!(
        own(&after),
        own(&before),
        "every pre-image config key but volatile.* is unchanged"
    );
    assert_eq!(
        after["devices"]["eth0"],
        json!({ "type": "nic", "name": "eth0", "network": "br1",
        "hwaddr": ADOPT_MAC, "ipv4.address": ADOPT_IP }),
        "the local eth0 is the pin"
    );
    let state = get(&fake, &at("/1.0/instances/svc1/state")).expect("the state");
    assert_eq!(state["network"]["eth0"]["hwaddr"], json!(ADOPT_MAC));
    assert_eq!(
        state["network"]["eth0"]["addresses"][0]["address"],
        json!(ADOPT_IP)
    );
    assert_eq!(
        fake.leases(),
        vec![(
            "br1".to_string(),
            ADOPT_MAC.to_string(),
            ADOPT_IP.to_string()
        )]
    );
    assert!(
        get(
            &fake,
            &at(&format!("/1.0/instances/svc1/snapshots/{snapshot}"))
        )
        .is_some(),
        "the snapshot"
    );
    for write in fake.writes() {
        assert_ne!(write.method, "DELETE", "{}", write.path);
        assert!(
            !write.path.starts_with("/1.0/networks") && !write.path.starts_with("/1.0/profiles"),
            "{}",
            write.path
        );
        assert!(
            !(write.method == "PATCH" && write.path.starts_with("/1.0/projects")),
            "{}",
            write.path
        );
    }

    let record = AdoptionRecord::open(&scratch.path("state")).expect("the record");
    assert_eq!(
        record.plan("svc1").map(|plan| plan.digest),
        Some(digest.clone())
    );
    assert_eq!(record.last("svc1"), Some((AdoptStep::Start, Phase::Done)));
    let status = adopt_run(&config, &["status", "--instance", "svc1", "--json"])
        .expect("status")
        .expect("a result");
    assert_eq!(status["complete"], json!(true));
    assert_eq!(status["live"]["project"], json!(ADOPT_TO));
    assert_eq!(status["digest"], json!(digest));
}

#[test]
fn a_graceful_stop_that_outlasts_its_timeout_is_forced_once_and_the_adoption_continues() {
    let scratch = Scratch::new("forced");
    let fake = adopt_fake(&scratch, vec![adopt_instance("default", json!({}))]);
    fake.hang_stop("svc1", false);
    let config = adopt_config(&scratch, &fake);
    let digest = adopt_digest(&config);
    let applied = apply(&config, &digest, &[])
        .expect("the adoption")
        .expect("a result");
    assert_eq!(applied["complete"], json!(true));
    let stops = fake
        .writes()
        .iter()
        .filter(|w| w.path == "/1.0/instances/svc1/state?project=default")
        .count();
    assert_eq!(stops, 2, "one graceful stop and one forced stop");
}

#[test]
fn a_step_that_fails_is_recorded_and_the_adoption_is_not_planned_again() {
    let scratch = Scratch::new("stuck");
    let fake = adopt_fake(&scratch, vec![adopt_instance("default", json!({}))]);
    fake.hang_stop("svc1", true);
    let config = adopt_config(&scratch, &fake);
    let digest = adopt_digest(&config);
    let failed = apply(&config, &digest, &[]);
    assert!(
        matches!(&failed, Err(failure @ Failure::Runtime(why)) if why.contains("Stop") && failure.code() == 1),
        "{failed:?}"
    );
    let record = AdoptionRecord::open(&scratch.path("state")).expect("the record");
    assert_eq!(record.last("svc1"), Some((AdoptStep::Stop, Phase::Failed)));
    let before = fake.writes().len();
    let again = apply(&config, &digest, &[]);
    assert!(
        matches!(&again, Err(Failure::Refused(why)) if why.contains("not completed")),
        "{again:?}"
    );
    assert_eq!(fake.writes().len(), before);
}

#[test]
fn serve_exits_2_for_a_config_that_lists_an_adoption() {
    let scratch = Scratch::new("serve");
    let fake = adopt_fake(&scratch, vec![adopt_instance("default", json!({}))]);
    let config = adopt_config(&scratch, &fake);
    let out = Command::new(env!("CARGO_BIN_EXE_arca"))
        .args(["serve", "--config"])
        .arg(&config)
        .output()
        .expect("run arca");
    assert_eq!(
        out.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("kernel.adopt"));
    assert!(fake.requests().is_empty(), "serve read nothing");
}
