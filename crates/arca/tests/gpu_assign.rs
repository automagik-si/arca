//! The owner-approved instance device writer against the fake Incus. It adds and removes one GPU device by PCI address and
//! leaves every other device of the instance alone. Two identical cards are two devices.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use arca_incus::{DeviceAssigner, FakeIncus, FakeState, ProjectFamily};
use serde_json::{Value, json};

const FAMILY: &str = "arcak-gpu";
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
        projects: vec![
            json!({ "name": FAMILY }),
            json!({ "name": "default" }),
            json!({ "name": "droidd" }),
        ],
        instances: vec![json!({
            "name": "alpha",
            "project": FAMILY,
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

fn family() -> ProjectFamily {
    ProjectFamily::new([FAMILY], []).expect("the family is valid")
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
    let assigner = DeviceAssigner::new(fake.socket().to_path_buf(), family());

    let row = assigner
        .read(FAMILY, "alpha")
        .expect("the instance is read");
    assert_eq!(row.kind, "container");
    assert!(row.devices.contains_key("root"));
    assert!(row.arca_device(FIRST).is_none());

    let first = assigner
        .attach_gpu(&row, FIRST)
        .expect("the first card attaches");
    assert!(first.changed);
    assert!(first.device.contains("0000-01-00-0"), "{}", first.device);

    let row = assigner
        .read(FAMILY, "alpha")
        .expect("the instance is read");
    assert_eq!(row.arca_device(FIRST), Some(first.device.as_str()));
    assert!(row.devices.contains_key("root"), "the root disk stayed");

    let second = assigner
        .attach_gpu(&row, SECOND)
        .expect("the second card attaches");
    assert!(second.changed);
    assert_ne!(first.device, second.device, "two cards, two devices");

    let row = assigner
        .read(FAMILY, "alpha")
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
        .read(FAMILY, "alpha")
        .expect("the instance is read");
    let removed = assigner
        .detach_gpu(&row, FIRST)
        .expect("the first card detaches");
    assert!(removed.changed);
    let row = assigner
        .read(FAMILY, "alpha")
        .expect("the instance is read");
    assert!(row.arca_device(FIRST).is_none());
    assert_eq!(row.arca_device(SECOND), Some(second.device.as_str()));
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
    let kernel_family = arca_incus::ProjectFamily::new([FAMILY], []).expect("the family is valid");
    let backend = IncusGpu::new(fake.socket().to_path_buf(), kernel_family, false);
    let cards = backend.inventory().expect("the inventory is read");
    assert_eq!(cards.len(), 2);
    assert_eq!(cards[0].pci, FIRST);
    assert_eq!(cards[1].pci, SECOND);
    assert_eq!(cards[0].product, cards[1].product);
    let alpha = backend
        .resolve("alpha")
        .expect("the instance is found in the family");
    assert_eq!(alpha.project, FAMILY);
    assert!(alpha.is_container());
    assert!(alpha.uuid.is_some());
    let change = backend
        .attach(&alpha, SECOND)
        .expect("the second card attaches");
    assert!(change.changed);

    let row = DeviceAssigner::new(fake.socket().to_path_buf(), family())
        .read(FAMILY, "alpha")
        .expect("the instance is read");
    assert_eq!(row.arca_device(SECOND), Some(change.device.as_str()));
    assert!(
        row.arca_device(FIRST).is_none(),
        "the first card was not attached by the second card's proposal"
    );
}

#[test]
fn a_stale_etag_is_412_and_changes_nothing() {
    let scratch = Scratch::new("stale");
    let fake = FakeIncus::start(scratch.path(), state()).expect("the fake serves");
    let assigner = DeviceAssigner::new(fake.socket().to_path_buf(), family());
    let stale = assigner
        .read(FAMILY, "alpha")
        .expect("the instance is read");
    assigner
        .attach_gpu(&stale, FIRST)
        .expect("the first write lands");
    let refused = assigner
        .attach_gpu(&stale, SECOND)
        .expect_err("the stale ETag is refused");
    assert!(
        matches!(refused, arca_incus::IncusError::Status { code: 412, .. }),
        "{refused:?}"
    );
    let row = assigner
        .read(FAMILY, "alpha")
        .expect("the instance is read");
    assert!(row.arca_device(FIRST).is_some());
    assert!(
        row.arca_device(SECOND).is_none(),
        "the stale write sent nothing"
    );
}

#[test]
fn a_gpu_device_that_arca_did_not_add_is_refused_and_left_alone() {
    let scratch = Scratch::new("foreign");
    let mut fake_state = state();
    fake_state.instances[0]["devices"]["gpu-0000-01-00-0"] = json!({
        "type": "gpu",
        "gputype": "physical",
        "pci": FIRST,
        "extra": "not Arca's"
    });
    let fake = FakeIncus::start(scratch.path(), fake_state).expect("the fake serves");
    let assigner = DeviceAssigner::new(fake.socket().to_path_buf(), family());
    let row = assigner
        .read(FAMILY, "alpha")
        .expect("the instance is read");
    let refused = assigner
        .detach_gpu(&row, FIRST)
        .expect_err("the device is not Arca's");
    assert!(
        refused
            .to_string()
            .contains("this GPU device was not added by Arca"),
        "{refused}"
    );
    assert!(
        fake.writes().iter().all(|write| write.method == "GET"),
        "nothing was written"
    );
    let row = assigner
        .read(FAMILY, "alpha")
        .expect("the instance is read");
    assert!(row.devices.contains_key("gpu-0000-01-00-0"));
}

#[test]
fn two_devices_for_one_card_do_not_leave_a_false_unconfirmed_removal() {
    let scratch = Scratch::new("two-devices");
    let mut fake_state = state();
    fake_state.instances[0]["devices"]["gpu-0000-01-00-0"] = json!({
        "type": "gpu", "gputype": "physical", "pci": FIRST
    });
    fake_state.instances[0]["devices"]["another-gpu"] = json!({
        "type": "gpu", "gputype": "physical", "pci": FIRST
    });
    let fake = FakeIncus::start(scratch.path(), fake_state).expect("the fake serves");
    let assigner = DeviceAssigner::new(fake.socket().to_path_buf(), family());
    let row = assigner
        .read(FAMILY, "alpha")
        .expect("the instance is read");
    let removed = assigner
        .detach_gpu(&row, FIRST)
        .expect("Arca's device is removed even when another names the card");
    assert!(removed.changed);
    let row = assigner
        .read(FAMILY, "alpha")
        .expect("the instance is read");
    assert!(row.arca_device(FIRST).is_none());
    assert!(
        row.devices.contains_key("another-gpu"),
        "the other writer's device stayed"
    );
}

#[test]
fn pci_spelling_is_normalized_on_attach_and_detach() {
    let scratch = Scratch::new("pci-spelling");
    let fake = FakeIncus::start(scratch.path(), state()).expect("the fake serves");
    let assigner = DeviceAssigner::new(fake.socket().to_path_buf(), family());
    let row = assigner
        .read(FAMILY, "alpha")
        .expect("the instance is read");
    let attached = assigner
        .attach_gpu(&row, "01:00.0")
        .expect("the short spelling attaches");
    assert!(attached.changed);
    let row = assigner
        .read(FAMILY, "alpha")
        .expect("the instance is read");
    assert_eq!(row.arca_device(FIRST), Some(attached.device.as_str()));
    assert_eq!(
        row.devices[&attached.device]["pci"],
        json!(FIRST),
        "the stored address is canonical"
    );
    let removed = assigner
        .detach_gpu(&row, "01:00.0")
        .expect("the short spelling detaches");
    assert!(removed.changed);
    assert!(
        assigner
            .read(FAMILY, "alpha")
            .expect("the instance is read")
            .arca_device(FIRST)
            .is_none()
    );
}

#[test]
fn the_backend_refuses_droidd_and_bench_and_a_moved_instance() {
    use arca::gpu::{GpuBackend, GpuInstance, GpuRefusal, IncusGpu};

    let scratch = Scratch::new("family");
    let mut fake_state = state();
    fake_state.instances.push(json!({
        "name": "foreign", "project": "droidd", "type": "container",
        "status": "Running", "profiles": [], "config": { "volatile.uuid": "foreign" },
        "devices": {}
    }));
    fake_state.instances.push(json!({
        "name": "benchy", "project": "bench", "type": "container",
        "status": "Running", "profiles": [], "config": { "volatile.uuid": "benchy" },
        "devices": {}
    }));
    let fake = FakeIncus::start(scratch.path(), fake_state).expect("the fake serves");
    let kernel_family =
        arca_incus::ProjectFamily::new([FAMILY], ["droidd"]).expect("the family is valid");
    let backend = IncusGpu::new(fake.socket().to_path_buf(), kernel_family, false);

    assert!(matches!(
        backend.resolve("foreign").expect_err("droidd is refused"),
        GpuRefusal::Project(_)
    ));
    assert!(matches!(
        backend.resolve("benchy").expect_err("bench is refused"),
        GpuRefusal::Project(_)
    ));
    assert!(matches!(
        backend
            .instance("droidd", "foreign")
            .expect_err("a droidd read is refused"),
        GpuRefusal::Project(_)
    ));
    assert!(backend.resolve("alpha").is_ok());
    let droidd_instance = GpuInstance {
        project: "droidd".to_string(),
        name: "foreign".to_string(),
        kind: "container".to_string(),
        uuid: Some("foreign".to_string()),
    };
    assert!(
        backend.attach(&droidd_instance, FIRST).is_err(),
        "the backend itself refuses a write outside the family"
    );
    assert_eq!(
        backend.in_use(FIRST).expect("the local check is off"),
        None,
        "a remote or provisional node cannot read local consumers"
    );

    let alpha = backend.resolve("alpha").expect("alpha");
    let moved = GpuInstance {
        uuid: Some("00000000-0000-4000-8000-00000000ffff".to_string()),
        ..alpha
    };
    let refused = backend
        .attach(&moved, FIRST)
        .expect_err("a recreated instance is refused");
    assert!(refused.contains("recreated"), "{refused}");
}

#[test]
fn a_taken_exact_device_name_is_refused_rather_than_suffixed() {
    let scratch = Scratch::new("name-taken");
    let mut fake_state = state();
    fake_state.instances[0]["devices"]["gpu-0000-01-00-0"] = json!({
        "type": "disk", "source": "/tmp", "path": "/mnt"
    });
    let fake = FakeIncus::start(scratch.path(), fake_state).expect("the fake serves");
    let assigner = DeviceAssigner::new(fake.socket().to_path_buf(), family());
    let row = assigner
        .read(FAMILY, "alpha")
        .expect("the instance is read");
    let refused = assigner
        .attach_gpu(&row, FIRST)
        .expect_err("the exact name is taken");
    assert!(refused.to_string().contains("already taken"), "{refused}");
    let row = assigner
        .read(FAMILY, "alpha")
        .expect("the instance is read");
    assert!(row.arca_device(FIRST).is_none());
}

#[test]
fn the_writer_itself_refuses_a_project_outside_its_family() {
    let scratch = Scratch::new("writer-family");
    let fake = FakeIncus::start(scratch.path(), state()).expect("the fake serves");
    let assigner = DeviceAssigner::new(fake.socket().to_path_buf(), family());
    let refused = assigner
        .read("default", "alpha")
        .expect_err("default is outside the family");
    assert!(
        refused.to_string().contains("outside the kernel's family"),
        "{refused}"
    );
    assert!(fake.requests().is_empty(), "no request was sent");
}
