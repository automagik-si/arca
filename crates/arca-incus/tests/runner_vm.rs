#![cfg(feature = "write")]
//! The job machine writer's create against the fake Incus: the create body's config carries the pool's mark and the
//! machine's limits and, for a copy of a kept machine's snapshot only, the key that has Incus rebuild the machine's UEFI
//! variables from the host it runs on. No other key is sent.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use arca_incus::runner_vm::{RUNNER_KEY, RunnerVms, VmSource, VmSpec};
use arca_incus::{FakeIncus, FakeState, IncusRead as _, UnixIncus};
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
        networks: vec![json!({ "name": "arcarun-0000001", "project": "default", "managed": true,
            "type": "bridge", "config": {} })],
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
    config.as_object_mut().expect("a config").remove("volatile.apply_nvram");
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
    vms.create(&spec("acme-0-0a1b2c3d", source)).expect("create");
    assert_eq!(
        create_keys(&rig),
        vec![keys(&[RUNNER_KEY, "limits.cpu", "limits.memory", "volatile.apply_nvram"])]
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
    vms.create(&spec("acme-0-0a1b2c3e", source)).expect("create");
    assert_eq!(
        create_keys(&rig),
        vec![keys(&[RUNNER_KEY, "limits.cpu", "limits.memory"])]
    );
    assert_eq!(
        config(&rig, "acme-0-0a1b2c3e"),
        json!({ RUNNER_KEY: "acme", "limits.cpu": "2", "limits.memory": "4096MiB" })
    );
}
