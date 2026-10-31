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
            "unmanaged-nic",
            svc1_with(|i| {
                i["devices"]["eth0"] =
                    json!({ "type": "nic", "name": "eth0", "nictype": "bridged", "parent": "br0" })
            }),
            "no managed network",
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
            "frozen",
            svc1_with(|i| i["status"] = json!("Frozen")),
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
            "has no root disk",
        ),
        (
            "root-off-pool",
            svc1_with(|i| {
                i["devices"]["root"] = json!({ "type": "disk", "path": "/", "source": "/srv/root" })
            }),
            "not on a storage pool",
        ),
        (
            "privileged",
            svc1_with(|i| i["config"]["security.privileged"] = json!("true")),
            "security.privileged",
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
    // eth0 sets a hwaddr that runs, and is leased, but is not the volatile MAC the instance had: the two disagree.
    refused_after(
        "nic-hwaddr",
        svc1_with(nic(json!({ "hwaddr": "10:66:6a:00:00:02" }))),
        &|fake, _| fake.set_lease("br1", "10:66:6a:00:00:02", ADOPT_IP),
        &[],
        "other than its running MAC",
    );
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

/// The default profile with `eth0` and `extra`, which the fake expands into `svc1`.
fn default_profile(fake: &FakeIncus, devices: Value, config: Value) {
    let mut all = json!({ "eth0": { "type": "nic", "name": "eth0", "network": "br1" } });
    all.as_object_mut()
        .expect("devices")
        .extend(devices.as_object().cloned().unwrap_or_default());
    fake.add_profile(
        "default",
        json!({ "name": "default", "config": config, "devices": all }),
    );
}

#[test]
fn what_a_move_would_drop_or_no_allowance_admits_is_refused_before_the_stop() {
    let device =
        |name: &'static str, body: Value| svc1_with(move |i| i["devices"][name] = body.clone());
    let key = |name: &'static str, value: &'static str| {
        svc1_with(move |i| i["config"][name] = json!(value))
    };
    let cases: Vec<(&str, Vec<Value>, &str)> = vec![
        (
            "volume-disk",
            device(
                "data",
                json!({ "type": "disk", "pool": "pool1", "source": "vol1", "path": "/data" }),
            ),
            "a disk other than its root",
        ),
        (
            "host-path-comma",
            device(
                "data",
                json!({ "type": "disk", "source": "/srv/a,b", "path": "/data" }),
            ),
            "cannot list exactly",
        ),
        (
            "host-path-shift",
            device(
                "data",
                json!({ "type": "disk", "source": "/srv/data", "path": "/data", "shift": "true" }),
            ),
            "shift",
        ),
        (
            "tpm",
            device("tpm0", json!({ "type": "tpm", "path": "/dev/tpm0" })),
            "of type",
        ),
        (
            "privileged-1",
            key("security.privileged", "1"),
            "security.privileged",
        ),
        ("expiry", key("snapshots.expiry", "1d"), "snapshots.expiry"),
    ];
    for (tag, instances, reason) in cases {
        refused_with_no_write(tag, instances, reason);
    }
    let svc1 = || vec![adopt_instance("default", json!({}))];
    refused_after(
        "profile-device",
        svc1(),
        &|fake, _| {
            default_profile(
                fake,
                json!({ "data": { "type": "disk", "pool": "pool1", "path": "/data" } }),
                json!({}),
            );
        },
        &[],
        "comes from a profile",
    );
    refused_after(
        "profile-config",
        svc1(),
        &|fake, _| default_profile(fake, json!({}), json!({ "limits.cpu": "2" })),
        &[],
        "comes from a profile",
    );
    refused_after(
        "profile-expiry",
        svc1(),
        &|fake, _| default_profile(fake, json!({}), json!({ "snapshots.expiry": "1w" })),
        &[],
        "snapshots.expiry",
    );
    refused_after(
        "backup",
        svc1(),
        &|fake, _| {
            assert_eq!(
                raw(
                    fake,
                    "POST",
                    "/1.0/instances/svc1/backups?project=default",
                    &json!({})
                ),
                202
            );
        },
        &[],
        "backups",
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
    assert_eq!(
        dry["allowances"],
        json!({ "project": {}, "localize": {}, "nic": {}, "why": {} }),
        "the dry run shows the plan allows nothing past the base"
    );
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
    // The move was a copy, under a new uuid, which the record holds.
    let recorded = AdoptionRecord::open(&scratch.path("state"))
        .expect("the record")
        .uuid("svc1")
        .expect("the recorded uuid");
    assert_ne!(recorded, ADOPT_UUID);
    assert_eq!(after["config"]["volatile.uuid"], json!(recorded));
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
    // A second apply resumes from the record: it runs no preflight, and runs the failed Stop again, which fails again.
    let (before, read_from) = (fake.writes().len(), fake.requests().len());
    let again = apply(&config, &digest, &[]);
    assert!(
        matches!(&again, Err(Failure::Runtime(why)) if why.contains("Stop")),
        "{again:?}"
    );
    assert_eq!(
        fake.writes().len(),
        before + 2,
        "the graceful stop and the forced stop"
    );
    assert!(
        fake.requests()[read_from..]
            .iter()
            .all(|r| !r.path.contains("/backups") && !r.path.contains("/leases")),
        "no preflight read"
    );
    let record = AdoptionRecord::open(&scratch.path("state")).expect("the record");
    assert_eq!(record.last("svc1"), Some((AdoptStep::Stop, Phase::Failed)));
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

/// The adoption project's config: eth0's network granted, snapshots and backups allowed, nothing else allowed, then `extra`.
fn base_config(network: &str, extra: &Value) -> Value {
    let mut config = json!({
        "user.arcakernel.entry": "adopt-svc1", "restricted": "true",
        "restricted.networks.access": network, "restricted.snapshots": "allow",
        "restricted.backups": "allow", "features.networks": "false", "features.profiles": "true",
        "features.storage.volumes": "true", "features.images": "true",
    });
    if let (Some(config), Some(extra)) = (config.as_object_mut(), extra.as_object()) {
        config.extend(extra.clone());
    }
    config
}

/// `arca adopt plan` for `svc1`.
fn plan_of(config: &Path) -> Value {
    adopt_run(
        config,
        &[
            "plan",
            "--instance",
            "svc1",
            "--expect-ip",
            ADOPT_IP,
            "--json",
        ],
    )
    .expect("the plan")
    .expect("a result")
}

/// The body of the request `step` sends, from a plan's or a dry run's request list.
fn request_body(planned: &Value, step: &str) -> Value {
    planned["requests"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|request| request["step"] == step)
        .map(|request| request["body"].clone())
        .unwrap_or(Value::Null)
}

/// An edit of the fixture instance.
type Edit = Box<dyn Fn(&mut Value)>;

#[test]
fn each_thing_the_instance_already_uses_is_planned_as_exactly_its_allowance_and_the_adoption_runs()
{
    let device = |name: &'static str, body: Value| {
        Box::new(move |i: &mut Value| i["devices"][name] = body.clone()) as Edit
    };
    let key = |name: &'static str, value: &'static str| {
        Box::new(move |i: &mut Value| i["config"][name] = json!(value)) as Edit
    };
    let data = json!({ "type": "disk", "source": "/srv/data", "path": "/data" });
    let low = |key: &str| {
        (
            json!({ "restricted.containers.lowlevel": "allow" }),
            json!({ "restricted.containers.lowlevel": [key] }),
        )
    };
    let one = |key: &str, because: &str| (json!({ key: "allow" }), json!({ key: [because] }));
    let cases: Vec<(&str, Edit, (Value, Value))> = vec![
        (
            "nothing",
            Box::new(|_: &mut Value| {}),
            (json!({}), json!({})),
        ),
        (
            "nesting",
            key("security.nesting", "true"),
            one("restricted.containers.nesting", "security.nesting"),
        ),
        ("raw", key("raw.lxc", "lxc.x=1"), low("raw.lxc")),
        (
            "kernel-modules",
            key("linux.kernel_modules", "overlay"),
            low("linux.kernel_modules"),
        ),
        (
            "idmap",
            key("security.idmap.base", "100000"),
            low("security.idmap.base"),
        ),
        (
            "intercept",
            key("security.syscalls.intercept.mknod", "true"),
            one(
                "restricted.containers.interception",
                "security.syscalls.intercept.mknod",
            ),
        ),
        (
            "proxy",
            device(
                "web",
                json!({ "type": "proxy", "listen": "tcp:0.0.0.0:80", "connect": "tcp:127.0.0.1:80" }),
            ),
            one("restricted.devices.proxy", "devices.web"),
        ),
        (
            "unix-char",
            device("tty", json!({ "type": "unix-char", "path": "/dev/ttyS0" })),
            one("restricted.devices.unix-char", "devices.tty"),
        ),
        (
            "unix-block",
            device("blk", json!({ "type": "unix-block", "path": "/dev/sdb" })),
            one("restricted.devices.unix-block", "devices.blk"),
        ),
        (
            "gpu",
            device("gpu0", json!({ "type": "gpu" })),
            one("restricted.devices.gpu", "devices.gpu0"),
        ),
        (
            "host-disk",
            device("data", data.clone()),
            (
                json!({ "restricted.devices.disk": "allow", "restricted.devices.disk.paths": "/srv/data" }),
                json!({ "restricted.devices.disk": ["devices.data"], "restricted.devices.disk.paths": ["devices.data"] }),
            ),
        ),
        (
            "nested-host-disk",
            Box::new({
                let data = data.clone();
                move |i: &mut Value| {
                    i["config"]["security.nesting"] = json!("true");
                    i["devices"]["data"] = data.clone();
                    i["devices"]["logs"] =
                        json!({ "type": "disk", "source": "/var/log/svc", "path": "/logs" });
                }
            }),
            (
                json!({ "restricted.containers.nesting": "allow", "restricted.devices.disk": "allow",
                    "restricted.devices.disk.paths": "/srv/data,/var/log/svc" }),
                json!({ "restricted.containers.nesting": ["security.nesting"],
                    "restricted.devices.disk": ["devices.data", "devices.logs"],
                    "restricted.devices.disk.paths": ["devices.data", "devices.logs"] }),
            ),
        ),
    ];
    for (tag, edit, (project, why)) in cases {
        let scratch = Scratch::new(&format!("allow-{tag}"));
        let fake = adopt_fake(&scratch, svc1_with(&*edit));
        let config = adopt_config(&scratch, &fake);
        let planned = plan_of(&config);
        assert_eq!(planned["allowances"]["project"], project, "{tag}");
        assert_eq!(planned["allowances"]["why"], why, "{tag}");
        assert_eq!(planned["allowances"]["localize"], json!({}), "{tag}");
        let recorded = planned["plan"]
            .get("allowances")
            .map_or(json!({}), |allowances| allowances["project"].clone());
        assert_eq!(
            recorded, project,
            "{tag}: the plan, which the digest covers"
        );
        assert_eq!(
            request_body(&planned, "createProject")["config"],
            base_config("br1", &project),
            "{tag}: the project sets exactly these allowances"
        );
        let digest = planned["digest"].as_str().expect("a digest").to_string();
        let dry = apply(&config, &digest, &["--dry-run"])
            .expect("the dry run")
            .expect("a result");
        assert_eq!(dry["allowances"], planned["allowances"], "{tag}");
        apply(&config, &digest, &[])
            .unwrap_or_else(|failure| panic!("{tag}: {failure:?}"))
            .expect("a result");
        let made = get(&fake, &format!("/1.0/projects/{ADOPT_TO}")).expect("the project");
        assert_eq!(made["config"], base_config("br1", &project), "{tag}");
        let after =
            get(&fake, &format!("/1.0/instances/svc1?project={ADOPT_TO}")).expect("adopted");
        assert_eq!(after["status"], json!("Running"), "{tag}");
    }
}

#[test]
fn a_nic_on_a_network_attach_networks_does_not_list_is_planned_as_its_grant() {
    let scratch = Scratch::new("allow-network");
    let instances = svc1_with(|i| {
        i["devices"]["eth0"] = json!({ "type": "nic", "name": "eth0", "network": "br2" })
    });
    let fake = adopt_fake(&scratch, instances);
    fake.set_lease("br2", ADOPT_MAC, ADOPT_IP);
    let config = adopt_config(&scratch, &fake);
    let planned = plan_of(&config);
    assert_eq!(
        planned["allowances"]["project"],
        json!({ "restricted.networks.access": "br2" })
    );
    assert_eq!(
        planned["allowances"]["why"],
        json!({ "restricted.networks.access": ["devices.eth0"] })
    );
    assert_eq!(
        request_body(&planned, "createProject")["config"],
        base_config("br2", &json!({})),
        "the grant names eth0's network alone, not the attach networks"
    );
    let digest = planned["digest"].as_str().expect("a digest").to_string();
    apply(&config, &digest, &[])
        .expect("the adoption")
        .expect("a result");
    let made = get(&fake, &format!("/1.0/projects/{ADOPT_TO}")).expect("the project");
    assert_eq!(made["config"], base_config("br2", &json!({})));
}

/// The first fixture's own `eth0`: on the attach network, with its IPv4 and two ACL keys set.
fn acl_nic() -> Value {
    json!({ "type": "nic", "name": "eth0", "network": "br1", "ipv4.address": ADOPT_IP,
        "security.acls": "web", "security.acls.default.egress.action": "allow",
        "security.acls.default.ingress.action": "drop" })
}

fn root() -> Value {
    json!({ "type": "disk", "path": "/", "pool": "pool1", "size": "60GiB" })
}

#[test]
fn an_instance_whose_nic_sets_its_ip_and_acls_and_whose_root_a_profile_gives_is_adopted_verified_and_released()
 {
    let scratch = Scratch::new("acl-root");
    let instances = svc1_with(|i| i["devices"] = json!({ "eth0": acl_nic() }));
    let fake = adopt_fake(&scratch, instances);
    fake.add_profile(
        "default",
        json!({ "name": "default", "config": {}, "devices": {
            "eth0": { "type": "nic", "name": "eth0", "network": "br1" }, "root": root() } }),
    );
    let acl = json!({ "name": "web", "config": {} });
    assert_eq!(raw(&fake, "POST", "/1.0/network-acls", &acl), 201);
    let config = adopt_config(&scratch, &fake);
    let before = get(&fake, "/1.0/instances/svc1?project=default").expect("the instance");
    let writes_before = fake.writes().len();

    // The plan allows nothing past the base, localizes the root, and shows the eth0 keys the pin keeps.
    let planned = plan_of(&config);
    // Every snapshot taken before LocalizeRoot, the adoption's own among them, holds no root of its own, so the plan says that
    // restoring one after the release gives a root without the profile's size.
    let note = &planned["allowances"]["why"]["localize.root"];
    assert_eq!(note.as_array().map(Vec::len), Some(1), "{note}");
    let note = note[0].as_str().expect("a note");
    for part in ["snapshot", "arca-adopt-", "size", "60GiB", "restor"] {
        assert!(note.contains(part), "{part}: {note}");
    }
    assert_eq!(
        planned["allowances"],
        json!({
            "project": {}, "localize": { "root": root() },
            "why": { "localize.root": [note] },
            "nic": { "ipv4.address": ADOPT_IP, "security.acls": "web",
                "security.acls.default.egress.action": "allow",
                "security.acls.default.ingress.action": "drop" },
        })
    );
    assert_eq!(
        planned["plan"]["allowances"],
        json!({ "localize": { "root": root() } }),
        "the localized root is in the plan the digest covers"
    );
    assert_eq!(
        planned["plan"]["steps"],
        json!([
            "createProject",
            "stop",
            "snapshot",
            "pinNic",
            "localizeRoot",
            "move",
            "mark",
            "start"
        ])
    );
    assert_eq!(
        request_body(&planned, "localizeRoot"),
        json!({ "devices": { "root": root() } })
    );
    assert_eq!(
        request_body(&planned, "createProject")["config"],
        base_config("br1", &json!({}))
    );
    let mut pinned = acl_nic();
    pinned["hwaddr"] = json!(ADOPT_MAC);
    assert_eq!(
        request_body(&planned, "pinNic"),
        json!({ "devices": { "eth0": pinned } }),
        "the pin keeps the ACL keys"
    );
    let digest = planned["digest"].as_str().expect("a digest").to_string();
    let dry = apply(&config, &digest, &["--dry-run"])
        .expect("the dry run")
        .expect("a result");
    assert_eq!(dry["allowances"], planned["allowances"]);
    assert_eq!(
        fake.writes().len(),
        writes_before,
        "plan and dry run send nothing"
    );

    apply(&config, &digest, &[])
        .expect("the adoption")
        .expect("a result");
    let made = get(&fake, &format!("/1.0/projects/{ADOPT_TO}")).expect("the project");
    assert_eq!(made["config"], base_config("br1", &json!({})));
    let after = get(&fake, &format!("/1.0/instances/svc1?project={ADOPT_TO}")).expect("adopted");
    assert_eq!(after["status"], json!("Running"));
    assert_eq!(after["devices"]["eth0"], pinned);
    assert_eq!(after["devices"]["root"], root());
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
        "the instance's config is unchanged"
    );
    let verified = adopt_run(
        &config,
        &[
            "verify",
            "--instance",
            "svc1",
            "--port",
            "8080",
            "--wait",
            "0",
        ],
    );
    // Nothing listens on the documentation address, so the port check fails after the MAC and IPv4 checks passed.
    assert!(
        matches!(&verified, Err(Failure::Runtime(why)) if why.contains("port 8080")),
        "{verified:?}"
    );

    let released = adopt_run(&config, &["release", "--instance", "svc1"])
        .expect("the release")
        .expect("a result");
    assert_eq!(released["released"], json!(true));
    assert!(
        get(&fake, &format!("/1.0/projects/{ADOPT_TO}")).is_none(),
        "the project is deleted"
    );
    let back = get(&fake, "/1.0/instances/svc1?project=default").expect("released");
    assert_eq!(back["status"], json!("Running"));
    assert!(
        back["devices"].get("root").is_none(),
        "the root is the profile's again"
    );
    assert_eq!(back["expanded_devices"]["root"], root());
    assert_eq!(back["devices"]["eth0"], pinned, "the pin stays");
    assert_eq!(own(&back), own(&before));
    let deletes: Vec<String> = fake
        .writes()
        .into_iter()
        .filter(|write| write.method == "DELETE")
        .map(|write| write.path)
        .collect();
    assert_eq!(deletes, [format!("/1.0/projects/{ADOPT_TO}")]);
    let verified = adopt_run(
        &config,
        &[
            "verify",
            "--instance",
            "svc1",
            "--port",
            "8080",
            "--wait",
            "0",
        ],
    );
    assert!(
        matches!(&verified, Err(Failure::Runtime(why)) if why.contains("port 8080")),
        "verify reads the released instance in default: {verified:?}"
    );
}

#[test]
fn an_eth0_hwaddr_that_agrees_with_the_running_mac_is_kept_and_the_adoption_runs() {
    // Incus keeps no volatile key for a NIC whose hwaddr is set, so the MAC is the device's own.
    let scratch = Scratch::new("hwaddr");
    let instances = svc1_with(|i| {
        i["devices"]["eth0"] =
            json!({ "type": "nic", "name": "eth0", "network": "br1", "hwaddr": ADOPT_MAC });
        i["config"]
            .as_object_mut()
            .expect("config")
            .remove("volatile.eth0.hwaddr");
    });
    let fake = adopt_fake(&scratch, instances);
    let config = adopt_config(&scratch, &fake);
    let planned = plan_of(&config);
    assert_eq!(planned["plan"]["preImage"]["mac"], json!(ADOPT_MAC));
    assert_eq!(planned["allowances"]["nic"], json!({ "hwaddr": ADOPT_MAC }));
    let digest = planned["digest"].as_str().expect("a digest").to_string();
    apply(&config, &digest, &[])
        .expect("the adoption")
        .expect("a result");
    let state = get(
        &fake,
        &format!("/1.0/instances/svc1/state?project={ADOPT_TO}"),
    )
    .expect("the state");
    assert_eq!(state["network"]["eth0"]["hwaddr"], json!(ADOPT_MAC));
}

// A stopped instance. It shows no running MAC or IPv4: the plan pins the MAC Incus starts it with, eth0's own hwaddr or else its
// volatile.eth0.hwaddr, and the IPv4 --expect-ip names. The adoption neither stops nor starts it.

/// A MAC no lease names.
const NO_LEASE_MAC: &str = "10:66:6a:00:00:07";

/// `svc1`, stopped, with `edit` applied.
fn stopped_svc1(edit: impl Fn(&mut Value)) -> Vec<Value> {
    svc1_with(move |i| {
        i["status"] = json!("Stopped");
        edit(i);
    })
}

/// The pin of a stopped fixture whose MAC is `mac`.
fn stopped_pin(mac: &str) -> Value {
    json!({ "type": "nic", "name": "eth0", "network": "br1", "hwaddr": mac, "ipv4.address": ADOPT_IP })
}

/// Whether the fake saw a stop or a start.
fn state_changed(fake: &FakeIncus) -> bool {
    fake.writes()
        .iter()
        .any(|write| write.method == "PUT" && write.path.contains("/state"))
}

#[test]
fn a_stopped_instance_whose_mac_or_ipv4_cannot_be_settled_is_refused_with_no_write() {
    let own_nic = |extra: Value| {
        move |i: &mut Value| {
            let mut eth0 = json!({ "type": "nic", "name": "eth0", "network": "br1" });
            if let (Some(eth0), Value::Object(extra)) = (eth0.as_object_mut(), extra.clone()) {
                eth0.extend(extra);
            }
            i["devices"]["eth0"] = eth0;
        }
    };
    let cases: Vec<(&str, Vec<Value>, &str)> = vec![
        (
            "stopped-no-mac",
            stopped_svc1(|i| {
                i["config"]
                    .as_object_mut()
                    .expect("config")
                    .remove("volatile.eth0.hwaddr");
            }),
            "has no MAC",
        ),
        (
            "stopped-bad-mac",
            stopped_svc1(|i| i["config"]["volatile.eth0.hwaddr"] = json!("10:66:6A:00:00:01")),
            "has no MAC",
        ),
        (
            "stopped-hwaddr",
            stopped_svc1(own_nic(json!({ "hwaddr": "10:66:6a:00:00:02" }))),
            "a hwaddr other than its volatile.eth0.hwaddr",
        ),
        (
            "stopped-nic-ip",
            stopped_svc1(own_nic(json!({ "ipv4.address": "192.0.2.132" }))),
            "an ipv4.address other than the one --expect-ip names",
        ),
    ];
    for (tag, instances, reason) in cases {
        refused_with_no_write(tag, instances, reason);
    }
    refused_after(
        "stopped-static-lease",
        stopped_svc1(|_| {}),
        &|fake, _| fake.set_static_lease("br1", ADOPT_MAC, "192.0.2.132"),
        &[],
        "static lease",
    );
    refused_after(
        "stopped-claimed",
        stopped_svc1(|_| {}),
        &|fake, _| fake.set_lease("br1", "10:66:6a:00:00:99", ADOPT_IP),
        &[],
        "claimed",
    );
    refused_after(
        "stopped-device-claim",
        stopped_svc1(|_| {}),
        &|fake, _| {
            let other = json!({ "name": "other", "type": "container", "config": {},
                "devices": { "eth0": { "type": "nic", "network": "br1", "ipv4.address": ADOPT_IP } } });
            assert_eq!(
                raw(fake, "POST", "/1.0/instances?project=default", &other),
                202
            );
        },
        &[],
        "claimed",
    );

    // apply plans afresh, and a stopped instance shows no IPv4 for it to take: --expect-ip is required, and it is in the
    // digest.
    let scratch = Scratch::new("stopped-apply-ip");
    let fake = adopt_fake(&scratch, stopped_svc1(|_| {}));
    let config = adopt_config(&scratch, &fake);
    let digest = adopt_digest(&config);
    let missing = apply(&config, &digest, &[]);
    assert!(
        matches!(&missing, Err(failure @ Failure::Refused(why)) if why.contains("--expect-ip") && failure.code() == 2),
        "{missing:?}"
    );
    let other = apply(&config, &digest, &["--expect-ip", "192.0.2.132"]);
    assert!(
        matches!(&other, Err(Failure::Refused(why)) if why.contains("stale")),
        "another IPv4 is another plan: {other:?}"
    );
    let bad = apply(&config, &digest, &["--expect-ip", "x"]);
    assert!(matches!(&bad, Err(Failure::Usage(_))), "{bad:?}");
    assert!(fake.writes().is_empty(), "{:?}", fake.writes());
    assert!(
        !scratch.path("state/adoptions.jsonl").exists(),
        "nothing was recorded"
    );
}

/// One stopped fixture: its tag, the MAC Incus starts it with, its edit, and the lease setup after the fake's own.
type StoppedCase = (&'static str, &'static str, Edit, Box<dyn Fn(&FakeIncus)>);

#[test]
fn a_stopped_instance_is_adopted_verified_and_released_stopped_with_its_mac_and_the_named_ipv4_pinned()
 {
    let own_ip =
        json!({ "type": "nic", "name": "eth0", "network": "br1", "ipv4.address": ADOPT_IP });
    let cases: Vec<StoppedCase> = vec![
        // eth0 from the profile, and a dynamic lease of another address for its MAC, which the pin's IPv4 replaces at start.
        (
            "stopped-dynamic",
            ADOPT_MAC,
            Box::new(|_: &mut Value| {}),
            Box::new(|fake: &FakeIncus| fake.set_lease("br1", ADOPT_MAC, "192.0.2.140")),
        ),
        // eth0 from the profile, and no lease for its MAC.
        (
            "stopped-no-lease",
            NO_LEASE_MAC,
            Box::new(|i: &mut Value| i["config"]["volatile.eth0.hwaddr"] = json!(NO_LEASE_MAC)),
            Box::new(|fake: &FakeIncus| fake.set_lease("br1", ADOPT_MAC, "192.0.2.150")),
        ),
        // eth0 of its own that sets the IPv4, and the static lease Incus lists for it.
        (
            "stopped-nic-ip",
            ADOPT_MAC,
            Box::new(move |i: &mut Value| i["devices"]["eth0"] = own_ip.clone()),
            Box::new(|fake: &FakeIncus| fake.set_static_lease("br1", ADOPT_MAC, ADOPT_IP)),
        ),
    ];
    for (tag, mac, edit, leases) in cases {
        let scratch = Scratch::new(tag);
        let fake = adopt_fake(&scratch, stopped_svc1(&*edit));
        leases(&fake);
        let config = adopt_config(&scratch, &fake);
        let leased = fake.leases();
        let before = get(&fake, "/1.0/instances/svc1?project=default").expect("the instance");

        let planned = plan_of(&config);
        let pre = &planned["plan"]["preImage"];
        assert_eq!(pre["stopped"], json!(true), "{tag}");
        assert_eq!(
            (&pre["mac"], &pre["ipv4"]),
            (&json!(mac), &json!(ADOPT_IP)),
            "{tag}"
        );
        let steps = json!(["createProject", "snapshot", "pinNic", "move", "mark"]);
        assert_eq!(
            planned["plan"]["steps"], steps,
            "{tag}: no stop and no start"
        );
        let requests = planned["requests"].as_array().expect("the requests");
        let listed: Vec<&Value> = requests.iter().map(|request| &request["step"]).collect();
        assert_eq!(json!(listed), steps, "{tag}: one request a step");
        assert!(
            requests
                .iter()
                .all(|request| !request["path"].as_str().unwrap_or("").contains("/state")),
            "{tag}: {requests:?}"
        );
        assert_eq!(
            request_body(&planned, "pinNic"),
            json!({ "devices": { "eth0": stopped_pin(mac) } }),
            "{tag}"
        );
        let digest = planned["digest"].as_str().expect("a digest").to_string();

        let missing = apply(&config, &digest, &[]);
        assert!(
            matches!(&missing, Err(Failure::Refused(why)) if why.contains("--expect-ip")),
            "{tag}: {missing:?}"
        );
        assert!(fake.writes().is_empty(), "{tag}");
        let applied = apply(&config, &digest, &["--expect-ip", ADOPT_IP])
            .unwrap_or_else(|failure| panic!("{tag}: {failure:?}"))
            .expect("a result");
        assert_eq!(applied["complete"], json!(true), "{tag}");
        assert_eq!(applied["steps"].as_array().map(Vec::len), Some(5), "{tag}");
        assert!(
            get(&fake, "/1.0/instances/svc1?project=default").is_none(),
            "{tag}"
        );
        let after =
            get(&fake, &format!("/1.0/instances/svc1?project={ADOPT_TO}")).expect("adopted");
        assert_eq!(
            after["status"],
            json!("Stopped"),
            "{tag}: the adoption leaves it stopped"
        );
        assert_eq!(
            after["config"]["user.arcakernel.entry"],
            json!("adopt-svc1"),
            "{tag}"
        );
        assert_eq!(after["devices"]["eth0"], stopped_pin(mac), "{tag}");
        assert_eq!(after["devices"]["root"], before["devices"]["root"], "{tag}");
        let record = AdoptionRecord::open(&scratch.path("state")).expect("the record");
        assert_eq!(
            record.last("svc1"),
            Some((AdoptStep::Mark, Phase::Done)),
            "{tag}"
        );
        assert!(
            record.complete("svc1"),
            "{tag}: Mark is a stopped plan's last step"
        );
        assert!(record.incomplete().is_empty(), "{tag}");
        let status = adopt_run(&config, &["status", "--instance", "svc1", "--json"])
            .expect("status")
            .expect("a result");
        assert_eq!(status["complete"], json!(true), "{tag}");
        assert_eq!(status["live"]["status"], json!("Stopped"), "{tag}");

        // A complete adoption applied again sends nothing.
        let writes = fake.writes().len();
        let again = apply(&config, &digest, &[])
            .expect("again")
            .expect("a result");
        assert_eq!(again["resumed"], json!(true), "{tag}");
        assert_eq!(fake.writes().len(), writes, "{tag}");

        // verify checks the move, the mark and the pin, and probes nothing: the instance is stopped.
        let verify = |more: &[&str]| {
            let mut args = vec!["verify", "--instance", "svc1"];
            args.extend_from_slice(more);
            adopt_run(&config, &args)
        };
        let verified = verify(&[]).expect("verified").expect("a result");
        assert_eq!(
            verified,
            json!({ "instance": "svc1", "project": ADOPT_TO, "mac": mac, "ipv4": ADOPT_IP,
                "stopped": true, "probed": false, "verified": true }),
            "{tag}"
        );
        for probe in [
            vec!["--port", "8080"],
            vec!["--url", "http://proxy.example/health"],
        ] {
            let refused = verify(&probe);
            assert!(
                matches!(&refused, Err(failure @ Failure::Usage(why)) if why.contains("stopped") && failure.code() == 2),
                "{tag} {probe:?}: {refused:?}"
            );
        }

        let released = adopt_run(&config, &["release", "--instance", "svc1"])
            .unwrap_or_else(|failure| panic!("{tag}: {failure:?}"))
            .expect("a result");
        assert_eq!(released["released"], json!(true), "{tag}");
        assert_eq!(released["steps"].as_array().map(Vec::len), Some(3), "{tag}");
        let back = get(&fake, "/1.0/instances/svc1?project=default").expect("released");
        assert_eq!(
            back["status"],
            json!("Stopped"),
            "{tag}: the release leaves it stopped"
        );
        assert!(
            !back["config"]
                .as_object()
                .expect("config")
                .keys()
                .any(|key| key.starts_with("user.arcakernel.")),
            "{tag}: Foreign again"
        );
        assert_eq!(
            back["devices"]["eth0"],
            stopped_pin(mac),
            "{tag}: the pin stays"
        );
        assert!(
            get(&fake, &format!("/1.0/projects/{ADOPT_TO}")).is_none(),
            "{tag}"
        );
        let verified = verify(&[]).expect("verified").expect("a result");
        assert_eq!(verified["project"], json!("default"), "{tag}");
        assert_eq!(verified["stopped"], json!(true), "{tag}");

        assert!(
            !state_changed(&fake),
            "{tag}: nothing stopped or started it"
        );
        for write in fake.writes() {
            assert!(
                write.method != "DELETE" || write.path == format!("/1.0/projects/{ADOPT_TO}"),
                "{tag}: {}",
                write.path
            );
            assert!(
                !write.path.starts_with("/1.0/networks")
                    && !write.path.starts_with("/1.0/profiles"),
                "{tag}: {}",
                write.path
            );
        }
        assert_eq!(fake.leases(), leased, "{tag}: no lease changed");
    }
}

#[test]
fn a_stopped_adoption_verify_fails_on_a_lost_pin_or_a_running_instance() {
    let scratch = Scratch::new("stopped-verify");
    let fake = adopt_fake(&scratch, stopped_svc1(|_| {}));
    let config = adopt_config(&scratch, &fake);
    let digest = adopt_digest(&config);
    apply(&config, &digest, &["--expect-ip", ADOPT_IP])
        .expect("the adoption")
        .expect("a result");
    let verify = || adopt_run(&config, &["verify", "--instance", "svc1"]);
    let path = format!("/1.0/instances/svc1?project={ADOPT_TO}");
    let mut moved = stopped_pin(ADOPT_MAC);
    moved["ipv4.address"] = json!("192.0.2.133");
    assert_eq!(
        raw(
            &fake,
            "PATCH",
            &path,
            &json!({ "devices": { "eth0": moved } })
        ),
        200
    );
    let lost = verify();
    assert!(
        matches!(&lost, Err(failure @ Failure::Runtime(why)) if why.contains("pin") && failure.code() == 1),
        "{lost:?}"
    );
    let pinned = json!({ "devices": { "eth0": stopped_pin(ADOPT_MAC) } });
    assert_eq!(raw(&fake, "PATCH", &path, &pinned), 200);
    verify().expect("pinned again").expect("a result");
    let unmarked = json!({ "config": { "user.arcakernel.entry": "" } });
    assert_eq!(raw(&fake, "PATCH", &path, &unmarked), 200);
    let foreign = verify();
    assert!(
        matches!(&foreign, Err(Failure::Runtime(why)) if why.contains("mark")),
        "{foreign:?}"
    );
    let marked = json!({ "config": { "user.arcakernel.entry": "adopt-svc1" } });
    assert_eq!(raw(&fake, "PATCH", &path, &marked), 200);
    let state = format!("/1.0/instances/svc1/state?project={ADOPT_TO}");
    assert_eq!(
        raw(&fake, "PUT", &state, &json!({ "action": "start" })),
        202
    );
    let running = verify();
    assert!(
        matches!(&running, Err(Failure::Runtime(why)) if why.contains("stopped")),
        "{running:?}"
    );

    // Started by hand, it is not released: the release neither stops nor starts it, so it refuses before any step.
    let writes = fake.writes().len();
    for args in [
        vec!["release", "--instance", "svc1"],
        vec!["release", "--instance", "svc1", "--dry-run"],
    ] {
        let refused = adopt_run(&config, &args);
        assert!(
            matches!(&refused, Err(failure @ Failure::Refused(why)) if why.contains("not stopped") && failure.code() == 2),
            "{args:?}: {refused:?}"
        );
    }
    assert_eq!(fake.writes().len(), writes, "no write");
    let record = AdoptionRecord::open(&scratch.path("state")).expect("the record");
    assert_eq!(
        record.last("svc1"),
        Some((AdoptStep::Mark, Phase::Done)),
        "nothing recorded"
    );
    let stop = json!({ "action": "stop", "force": true });
    assert_eq!(raw(&fake, "PUT", &state, &stop), 202);
    adopt_run(&config, &["release", "--instance", "svc1"])
        .expect("the release")
        .expect("a result");
    let back = get(&fake, "/1.0/instances/svc1?project=default").expect("released");
    assert_eq!(back["status"], json!("Stopped"));
}

#[test]
fn a_stopped_adoption_killed_before_its_move_and_started_by_hand_is_not_released_until_stopped() {
    let scratch = Scratch::new("stopped-early-start");
    let fake = adopt_fake(&scratch, stopped_svc1(|_| {}));
    let config = adopt_config(&scratch, &fake);
    let digest = adopt_digest(&config);
    let halted = arca::cmd_adopt::adopt_halting(
        &[
            "apply",
            "--instance",
            "svc1",
            "--plan-digest",
            &digest,
            "--expect-ip",
            ADOPT_IP,
            "--config",
            &config.display().to_string(),
        ]
        .map(str::to_string),
        &mut |step, kill| step == AdoptStep::PinNic && kill == arca::cmd_adopt::Kill::AfterOutcome,
    );
    assert!(
        matches!(&halted, Err(Failure::Runtime(why)) if why.contains("halted")),
        "{halted:?}"
    );
    let state = "/1.0/instances/svc1/state?project=default";
    assert_eq!(raw(&fake, "PUT", state, &json!({ "action": "start" })), 202);
    let writes = fake.writes().len();
    let refused = adopt_run(&config, &["release", "--instance", "svc1"]);
    assert!(
        matches!(&refused, Err(Failure::Refused(why)) if why.contains("not stopped in default")),
        "{refused:?}"
    );
    assert_eq!(fake.writes().len(), writes, "no write");
    assert_eq!(
        raw(
            &fake,
            "PUT",
            state,
            &json!({ "action": "stop", "force": true })
        ),
        202
    );
    let released = adopt_run(&config, &["release", "--instance", "svc1"])
        .expect("the release")
        .expect("a result");
    assert_eq!(released["released"], json!(true));
    assert!(
        get(&fake, &format!("/1.0/projects/{ADOPT_TO}")).is_none(),
        "the project is deleted"
    );
    let back = get(&fake, "/1.0/instances/svc1?project=default").expect("in default");
    assert_eq!(back["status"], json!("Stopped"));
    assert_eq!(
        back["devices"]["eth0"],
        stopped_pin(ADOPT_MAC),
        "the pin stays"
    );
}

#[test]
fn apply_takes_an_expect_ip_for_a_running_instance_only_as_its_running_ipv4_and_the_digest_is_the_same()
 {
    let scratch = Scratch::new("running-apply-ip");
    let fake = adopt_fake(&scratch, vec![adopt_instance("default", json!({}))]);
    let config = adopt_config(&scratch, &fake);
    let planned = plan_of(&config);
    assert!(
        planned["plan"]["preImage"].get("stopped").is_none(),
        "a running instance's pre-image is the one it had before stopped instances were planned"
    );
    let digest = planned["digest"].as_str().expect("a digest").to_string();
    let other = apply(&config, &digest, &["--expect-ip", "192.0.2.99"]);
    assert!(
        matches!(&other, Err(Failure::Refused(why)) if why.contains("--expect-ip")),
        "{other:?}"
    );
    assert!(fake.writes().is_empty());
    let applied = apply(&config, &digest, &["--expect-ip", ADOPT_IP])
        .expect("the adoption")
        .expect("a result");
    assert_eq!(applied["complete"], json!(true));
    let after = get(&fake, &format!("/1.0/instances/svc1?project={ADOPT_TO}")).expect("adopted");
    assert_eq!(after["status"], json!("Running"));
    // A resume takes the recorded plan: an --expect-ip that is not its IPv4 is refused.
    let resumed = apply(&config, &digest, &["--expect-ip", "192.0.2.99"]);
    assert!(
        matches!(&resumed, Err(Failure::Refused(why)) if why.contains("--expect-ip")),
        "{resumed:?}"
    );
}
