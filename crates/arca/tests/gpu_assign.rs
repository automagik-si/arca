//! The owner-approved instance device writer against the fake Incus. It adds and removes one GPU device by PCI address and
//! leaves every other device of the instance alone. Two identical cards are two devices.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use arca_incus::{DeviceAssigner, FakeIncus, FakeState};
use serde_json::{Value, json};

const FIRST: &str = "0000:01:00.0";
const SECOND: &str = "0000:02:00.0";

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("the clock")
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("arca-assign-{}-{nanos}-{tag}", std::process::id()));
        fs::create_dir_all(&dir).expect("the scratch directory");
        Scratch(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn state() -> FakeState {
    FakeState {
        server: json!({ "api_version": "1.0" }),
        projects: vec![json!({ "name": "default" })],
        instances: vec![json!({
            "name": "alpha",
            "project": "default",
            "type": "container",
            "status": "Running",
            "profiles": [],
            "config": { "volatile.uuid": "00000000-0000-4000-8000-0000000000a1" },
            "devices": {
                "root": { "type": "disk", "path": "/", "pool": "pool1" }
            }
        })],
        pools: Vec::new(),
        networks: Vec::new(),
    }
}

fn gpu_devices(row: &Value) -> Vec<(String, String)> {
    row["devices"]
        .as_object()
        .expect("devices")
        .iter()
        .filter(|(_, device)| device["type"] == json!("gpu"))
        .map(|(name, device)| {
            (
                name.clone(),
                device["pci"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

#[test]
fn two_identical_cards_are_two_devices_and_only_the_named_one_goes() {
    let scratch = Scratch::new("two-gpus");
    let fake = FakeIncus::start(scratch.path(), state()).expect("the fake serves");
    let assigner = DeviceAssigner::new(fake.socket().to_path_buf());

    let row = assigner
        .read("default", "alpha")
        .expect("the instance is read");
    assert_eq!(row.kind, "container");
    assert!(row.devices.contains_key("root"));
    assert!(row.gpu_device(FIRST).is_none());

    let first = assigner
        .attach_gpu(&row, FIRST)
        .expect("the first card attaches");
    assert!(first.changed);
    assert!(first.device.contains("0000-01-00-0"), "{}", first.device);

    let row = assigner
        .read("default", "alpha")
        .expect("the instance is read");
    assert_eq!(row.gpu_device(FIRST), Some(first.device.as_str()));
    assert!(row.devices.contains_key("root"), "the root disk stayed");

    let second = assigner
        .attach_gpu(&row, SECOND)
        .expect("the second card attaches");
    assert!(second.changed);
    assert_ne!(first.device, second.device, "two cards, two devices");

    let row = assigner
        .read("default", "alpha")
        .expect("the instance is read");
    assert_eq!(
        gpu_devices(&row.body),
        vec![
            (first.device.clone(), FIRST.to_string()),
            (second.device.clone(), SECOND.to_string()),
        ]
        .into_iter()
        .collect::<std::collections::BTreeMap<_, _>>()
        .into_iter()
        .collect::<Vec<_>>()
    );

    // The same card again is the same device, not a second one.
    let again = assigner
        .attach_gpu(&row, SECOND)
        .expect("a repeated attach is idempotent");
    assert!(!again.changed);
    assert_eq!(again.device, second.device);

    let row = assigner
        .read("default", "alpha")
        .expect("the instance is read");
    let removed = assigner
        .detach_gpu(&row, FIRST)
        .expect("the first card detaches");
    assert!(removed.changed);
    let row = assigner
        .read("default", "alpha")
        .expect("the instance is read");
    assert!(row.gpu_device(FIRST).is_none());
    assert_eq!(row.gpu_device(SECOND), Some(second.device.as_str()));
    assert!(row.devices.contains_key("root"), "the root disk stayed");

    let gone = assigner
        .detach_gpu(&row, FIRST)
        .expect("a repeated detach is idempotent");
    assert!(!gone.changed);
}

#[test]
fn the_backend_reads_the_inventory_and_attaches_the_named_card_only() {
    use arca::gpu::{GpuBackend, IncusGpu};

    let scratch = Scratch::new("backend");
    let fake = FakeIncus::start(scratch.path(), state()).expect("the fake serves");
    fake.set_resources(json!({
        "gpu": {
            "total": 2,
            "cards": [
                {
                    "pci_address": FIRST,
                    "vendor": "NVIDIA Corporation",
                    "product": "NVIDIA GeForce RTX 4060 Ti",
                    "driver": "nvidia",
                    "driver_version": "580.65.06",
                    "numa_node": 0,
                    "nvidia": { "uuid": "GPU-11111111-1111-1111-1111-111111111111" }
                },
                {
                    "pci_address": SECOND,
                    "vendor": "NVIDIA Corporation",
                    "product": "NVIDIA GeForce RTX 4060 Ti",
                    "driver": "nvidia",
                    "driver_version": "580.65.06",
                    "numa_node": 0,
                    "nvidia": { "uuid": "GPU-22222222-2222-2222-2222-222222222222" }
                }
            ]
        }
    }));
    let backend = IncusGpu::new(fake.socket().to_path_buf());
    let cards = backend.inventory().expect("the inventory is read");
    assert_eq!(cards.len(), 2);
    assert_eq!(cards[0].pci, FIRST);
    assert_eq!(cards[1].pci, SECOND);
    assert_eq!(cards[0].product, cards[1].product);
    let alpha = backend.instance("alpha").expect("the instance is found");
    assert_eq!(alpha.project, "default");
    assert!(alpha.is_container());
    let change = backend
        .attach(&alpha, SECOND)
        .expect("the second card attaches");
    assert!(change.changed);

    let row = DeviceAssigner::new(fake.socket().to_path_buf())
        .read("default", "alpha")
        .expect("the instance is read");
    assert_eq!(row.gpu_device(SECOND), Some(change.device.as_str()));
    assert!(
        row.gpu_device(FIRST).is_none(),
        "the first card was not attached by the second card's proposal"
    );
}
