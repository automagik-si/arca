#![cfg(feature = "write")]
//! The job machine writer's create against the fake Incus: the create body's config carries the pool's mark, the
//! machine's limits and the key that has Incus rebuild the machine's UEFI variables from the host it runs on, and no
//! other key.

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

#[test]
fn a_job_machine_is_made_to_take_fresh_firmware_variables_from_the_host_it_runs_on() {
    let rig = rig();
    let vms = RunnerVms::new(rig.fake.socket().to_path_buf(), PROJECT, "acme").expect("writer");
    let copies = [
        (
            "acme-0-0a1b2c3d",
            VmSource::Snapshot {
                instance: "runner-image".to_string(),
                snapshot: "ready".to_string(),
            },
        ),
        ("acme-0-0a1b2c3e", VmSource::Image("arca-runner-image".to_string())),
    ];
    for (name, source) in copies {
        vms.create(&spec(name, source)).expect("create");
        // The key is sent with the pool's mark and the machine's limits, which keep their values.
        assert_eq!(
            config(&rig, name),
            json!({
                RUNNER_KEY: "acme",
                "limits.cpu": "2",
                "limits.memory": "4096MiB",
                "volatile.apply_nvram": "true",
            }),
            "{name}"
        );
    }
    // Each create's body carried exactly these keys, and no other.
    let posts: Vec<Vec<String>> = rig
        .fake
        .writes()
        .into_iter()
        .filter(|write| write.method == "POST" && write.path.starts_with("/1.0/instances?"))
        .map(|write| {
            let mut keys = write.config_keys;
            keys.sort();
            keys
        })
        .collect();
    let mut expected = vec![
        RUNNER_KEY.to_string(),
        "limits.cpu".to_string(),
        "limits.memory".to_string(),
        "volatile.apply_nvram".to_string(),
    ];
    expected.sort();
    assert_eq!(posts, vec![expected.clone(), expected]);
}
