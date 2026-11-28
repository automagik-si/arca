#![cfg(feature = "write")]
//! The job machine writer's create against the fake Incus: the create body's config carries the pool's mark and the
//! machine's limits and, for a copy of a kept machine's snapshot only, the key that has Incus rebuild the machine's UEFI
//! variables from the host it runs on. No other key is sent. Its devices are the root disk the spec names, Arca's card and
//! the agent drive, from an image and from a kept machine alike; a machine whose agent drive reads back as anything but
//! that drive is deleted before it starts.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use arca_incus::runner_vm::{RUNNER_KEY, RunnerVms, VmSource, VmSpec};
use arca_incus::{Appears, FakeIncus, FakeState, IncusRead as _, UnixIncus};
use arca_kernel::ownership::{IncusKind, ObjectRef};
use serde_json::{Value, json};

const PROJECT: &str = "arcak-runners";

static SEQ: AtomicU64 = AtomicU64::new(0);

struct Rig {
    dir: PathBuf,
    fake: FakeIncus,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn rig() -> Rig {
    let dir = std::env::temp_dir().join(format!(
        "arca-runner-vm-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir(&dir).expect("scratch");
    let state = FakeState {
        server: json!({}),
        projects: vec![json!({ "name": PROJECT, "config": { "features.profiles": "false" } })],
        instances: vec![json!({
            "name": "runner-image",
            "project": PROJECT,
            "type": "virtual-machine",
            "status": "Stopped",
            "profiles": [],
            "config": {},
            "devices": {},
        })],
        pools: vec![("pool1".to_string(), json!({}))],
        networks: vec![
            json!({ "name": "arcarun-0000001", "project": "default", "managed": true,
            "type": "bridge", "config": {} }),
        ],
    };
    let fake = FakeIncus::start(&dir, state).expect("the fake serves");
    Rig { dir, fake }
}

fn spec(name: &str, source: VmSource) -> VmSpec {
    VmSpec {
        name: name.to_string(),
        source,
        cpu: 2,
        memory_mib: 4096,
        root_gib: None,
        storage_pool: None,
        profiles: vec![],
        network: "arcarun-0000001".to_string(),
    }
}

/// The config of the machine `name` as the fake keeps it, without the keys the fake writes itself: the uuid of every
/// instance, and the card's MAC and the power state a start writes.
fn config(rig: &Rig, name: &str) -> Value {
    let read = UnixIncus::new(rig.fake.socket().to_path_buf());
    let made = read
        .get(&format!("/1.0/instances/{name}?project={PROJECT}"))
        .expect("the machine exists");
    let made = made.get("metadata").unwrap_or(&made).clone();
    let mut config = made["config"].clone();
    let keys = config.as_object_mut().expect("a config");
    for written in [
        "volatile.uuid",
        "volatile.eth0.hwaddr",
        "volatile.last_state.power",
    ] {
        keys.remove(written);
    }
    config
}

/// The config keys each instance create sent, in order, each list sorted.
fn create_keys(rig: &Rig) -> Vec<Vec<String>> {
    rig.fake
        .writes()
        .into_iter()
        .filter(|write| write.method == "POST" && write.path.starts_with("/1.0/instances?"))
        .map(|write| {
            let mut keys = write.config_keys;
            keys.sort();
            keys
        })
        .collect()
}

fn keys(names: &[&str]) -> Vec<String> {
    let mut keys: Vec<String> = names.iter().map(|name| name.to_string()).collect();
    keys.sort();
    keys
}

/// The key is checked in the create request only: real Incus clears it at the machine's first start, which the create
/// makes, while the fake keeps what was sent. So the config read back is compared without it.
fn kept_keys(rig: &Rig, name: &str) -> Value {
    let mut config = config(rig, name);
    config
        .as_object_mut()
        .expect("a config")
        .remove("volatile.apply_nvram");
    config
}

#[test]
fn a_copy_of_a_kept_machine_is_made_to_take_fresh_firmware_variables_from_the_host_it_runs_on() {
    let rig = rig();
    let vms = RunnerVms::new(rig.fake.socket().to_path_buf(), PROJECT, "acme").expect("writer");
    let source = VmSource::Snapshot {
        instance: "runner-image".to_string(),
        snapshot: "ready".to_string(),
    };
    vms.create(&spec("acme-0-0a1b2c3d", source))
        .expect("create");
    assert_eq!(
        create_keys(&rig),
        vec![keys(&[
            RUNNER_KEY,
            "limits.cpu",
            "limits.memory",
            "volatile.apply_nvram"
        ])]
    );
    // The pool's mark and the machine's limits keep their values.
    assert_eq!(
        kept_keys(&rig, "acme-0-0a1b2c3d"),
        json!({ RUNNER_KEY: "acme", "limits.cpu": "2", "limits.memory": "4096MiB" })
    );
}

#[test]
fn a_machine_made_from_an_image_is_sent_no_firmware_key() {
    // A machine made from an image has no firmware variables yet, so Incus builds them from the host's template anyway;
    // and a restricted project refuses the key on an image create.
    let rig = rig();
    let vms = RunnerVms::new(rig.fake.socket().to_path_buf(), PROJECT, "acme").expect("writer");
    let source = VmSource::Image("arca-runner-image".to_string());
    vms.create(&spec("acme-0-0a1b2c3e", source))
        .expect("create");
    assert_eq!(
        create_keys(&rig),
        vec![keys(&[RUNNER_KEY, "limits.cpu", "limits.memory"])]
    );
    assert_eq!(
        config(&rig, "acme-0-0a1b2c3e"),
        json!({ RUNNER_KEY: "acme", "limits.cpu": "2", "limits.memory": "4096MiB" })
    );
}

/// The machine `name` as the fake keeps it, or `None` when it is gone.
fn machine(rig: &Rig, name: &str) -> Option<Value> {
    let read = UnixIncus::new(rig.fake.socket().to_path_buf());
    let made = read
        .get(&format!("/1.0/instances/{name}?project={PROJECT}"))
        .ok()?;
    Some(made.get("metadata").unwrap_or(&made).clone())
}

/// The card every job machine is given, for the pool `acme` on the spec's bridge.
fn card() -> Value {
    json!({
        "type": "nic",
        "network": "arcarun-0000001",
        "security.mac_filtering": "true",
        "security.ipv4_filtering": "true",
        "security.ipv6_filtering": "true",
        "security.acls": "arca-runner-acme",
        "security.acls.default.ingress.action": "reject",
        "security.acls.default.egress.action": "reject",
    })
}

fn kept_machine() -> VmSource {
    VmSource::Snapshot {
        instance: "runner-image".to_string(),
        snapshot: "ready".to_string(),
    }
}

#[test]
fn a_job_machine_carries_the_agent_drive_whether_made_from_an_image_or_a_kept_machine() {
    // On a host whose QEMU has no 9p, the drive is the only way the guest's agent gets its config; it is built per machine.
    for (name, source) in [
        (
            "acme-0-0a1b2c40",
            VmSource::Image("arca-runner-image".to_string()),
        ),
        ("acme-0-0a1b2c41", kept_machine()),
    ] {
        let rig = rig();
        let vms = RunnerVms::new(rig.fake.socket().to_path_buf(), PROJECT, "acme").expect("writer");
        let mut spec = spec(name, source);
        spec.root_gib = Some(80);
        spec.storage_pool = Some("pool1".to_string());
        vms.create(&spec).expect("create");
        let made = machine(&rig, name).expect("the machine exists");
        assert_eq!(
            made["devices"],
            json!({
                "root": { "type": "disk", "path": "/", "size": "80GiB", "pool": "pool1" },
                "eth0": card(),
                "agent": { "type": "disk", "source": "agent:config" },
            }),
            "{name}"
        );
        // The check kept it: it was started.
        assert_eq!(made["status"], "Running", "{name}");
    }
}

#[test]
fn a_job_machine_without_a_root_disk_in_its_spec_carries_only_the_card_and_the_agent_drive() {
    let rig = rig();
    let vms = RunnerVms::new(rig.fake.socket().to_path_buf(), PROJECT, "acme").expect("writer");
    vms.create(&spec("acme-0-0a1b2c42", kept_machine()))
        .expect("create");
    let made = machine(&rig, "acme-0-0a1b2c42").expect("the machine exists");
    assert_eq!(
        made["devices"],
        json!({ "eth0": card(), "agent": { "type": "disk", "source": "agent:config" } })
    );
    assert_eq!(made["status"], "Running");
}

#[test]
fn a_job_machine_whose_agent_drive_reads_back_changed_is_deleted_before_it_starts() {
    for (case, device) in [
        (
            "a path",
            json!({ "type": "disk", "source": "agent:config", "path": "/mnt/agent" }),
        ),
        (
            "another source",
            json!({ "type": "disk", "source": "cloud-init:config" }),
        ),
        (
            "a host directory",
            json!({ "type": "disk", "source": "/srv/ci-cache" }),
        ),
        (
            "another key",
            json!({ "type": "disk", "source": "agent:config", "pool": "pool1" }),
        ),
        (
            "another type",
            json!({ "type": "unix-block", "source": "agent:config" }),
        ),
    ] {
        let rig = rig();
        let name = "acme-0-0a1b2c50";
        // The create is held while it runs, and the drive is changed under it, as a profile or a hand edit could.
        let held = rig.fake.hold_create(
            &ObjectRef {
                kind: IncusKind::Instance,
                project: PROJECT.to_string(),
                parent: None,
                name: name.to_string(),
                uuid: None,
            },
            Appears::WhileRunning,
        );
        let vms = RunnerVms::new(rig.fake.socket().to_path_buf(), PROJECT, "acme").expect("writer");
        let spec = spec(name, VmSource::Image("arca-runner-image".to_string()));
        let create = thread::spawn(move || vms.create(&spec));
        let until = Instant::now() + Duration::from_secs(20);
        while machine(&rig, name).is_none() {
            assert!(Instant::now() < until, "{case}: the create never arrived");
            thread::sleep(Duration::from_millis(10));
        }
        rig.fake
            .set_instance_device(PROJECT, name, "agent", device.clone());
        rig.fake.release_operation(&held);
        let error = create.join().expect("the create ends").expect_err(case);
        assert!(error.to_string().contains("agent ("), "{case}: {error}");
        assert!(machine(&rig, name).is_none(), "{case}: the machine is gone");
        assert!(
            !rig.fake
                .writes()
                .iter()
                .any(|write| write.path.contains("/state")),
            "{case}: never started"
        );
    }
}
