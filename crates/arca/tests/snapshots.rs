//! Snapshot storage and snapshots: disk roles, the gated storage_add, snapshot_create and snapshot_delete plans with their
//! wording and refusals, the copy rules (never started, sources only read, no dependency on the source project), and the
//! closed write family. Every disk is an `lsblk` fixture with made-up models and serials, and every Incus answer comes from
//! an in-memory fake; no real disk and no real Incus is touched.

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arca::action_proposal::ActionKind;
use arca::handlers::{ActionExecutor, ActionSlot, plan_handlers_full};
use arca::kernel::Kernel;
use arca::storage::{
    ERASE_SENTENCE, FixedBlockDevices, SnapshotListHandler, Storage, StorageListHandler,
    snapshot_name,
};
use arca_incus::fake_storage::FakeStorageIncus;
use arca_incus::snapshot::{
    SNAPSHOT_POOL_DESCRIPTION, SNAPSHOT_PROJECT, source_dependencies, write_allowed,
};
use arca_mcp::{CallError, ToolHandler};
use common::*;
use serde_json::{Value, json};

const GB: u64 = 1_000_000_000;

fn disk(name: &str, model: &str, tran: &str, rm: bool, size: u64, extra: Value) -> Value {
    let mut row = json!({
        "name": name, "path": format!("/dev/{name}"), "size": size, "model": model,
        "serial": format!("FAKE-{}", name.to_uppercase()), "tran": tran, "rm": rm, "type": "disk",
        "fstype": null, "label": null, "uuid": null, "mountpoints": [null],
    });
    if let (Some(row), Some(extra)) = (row.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            row.insert(key.clone(), value.clone());
        }
    }
    row
}

fn node(
    name: &str,
    kind: &str,
    fstype: Option<&str>,
    label: Option<&str>,
    mounts: &[&str],
    size: u64,
) -> Value {
    json!({
        "name": name, "path": format!("/dev/{name}"), "size": size, "type": kind,
        "fstype": fstype, "label": label, "uuid": format!("uuid-{name}"),
        "mountpoints": if mounts.is_empty() { json!([null]) } else { json!(mounts) },
    })
}

/// The host's disks: a system NVMe, a free USB disk, a mounted USB stick, a RAID member, an LVM member, a disk holding an
/// Incus pool, a disk holding swap, and a loop device that is never listed.
fn host_disks() -> Value {
    json!({ "blockdevices": [
        disk("nvme0n1", "Fakedisk NVMe 512G", "nvme", false, 512 * GB, json!({ "children": [
            node("nvme0n1p1", "part", Some("vfat"), None, &["/boot/efi"], GB),
            node("nvme0n1p2", "part", Some("ext4"), None, &["/", "/home"], 511 * GB),
        ]})),
        disk("sdb", "Acme Portable 1T", "usb", true, 1000 * GB, json!({ "children": [
            node("sdb1", "part", Some("ext4"), Some("PHOTOS"), &[], 1000 * GB),
        ]})),
        disk("sdc", "Acme Stick 64G", "usb", true, 64 * GB, json!({ "children": [
            node("sdc1", "part", Some("exfat"), Some("BACKUP"), &["/media/owner/BACKUP"], 64 * GB),
        ]})),
        disk("sdd", "Fakedisk SATA 2T", "sata", false, 2000 * GB, json!({
            "fstype": "linux_raid_member",
            "children": [node("md0", "raid1", Some("ext4"), None, &["/srv"], 2000 * GB)],
        })),
        disk("sde", "Fakedisk SATA 4T", "sata", false, 4000 * GB, json!({ "children": [
            node("sde1", "part", Some("LVM2_member"), None, &[], 4000 * GB),
        ]})),
        disk("sdf", "Fakedisk SSD 1T", "sata", false, 1000 * GB, json!({
            "fstype": "btrfs", "uuid": "pool-uuid-fast",
        })),
        disk("sdg", "Acme Portable 500G", "usb", true, 500 * GB, json!({ "children": [
            node("sdg1", "part", Some("swap"), None, &["[SWAP]"], 8 * GB),
        ]})),
        { "name": "loop0", "path": "/dev/loop0", "size": GB, "type": "loop", "mountpoints": ["/snap/core"] },
    ]})
}

struct Rig {
    _scratch: Scratch,
    _fake: arca_incus::FakeIncus,
    incus: FakeStorageIncus,
    devices: Arc<FixedBlockDevices>,
    kernel: Arc<Mutex<Kernel>>,
    handlers: std::collections::HashMap<&'static str, Arc<dyn ToolHandler>>,
    executor: ActionExecutor,
    storage: Arc<Storage>,
    nonces: AtomicU64,
}

impl Rig {
    fn new(tag: &str, key: &SkKey) -> Rig {
        Self::with_writes(tag, key, true)
    }

    fn with_writes(tag: &str, key: &SkKey, writes: bool) -> Rig {
        let scratch = Scratch::new(tag);
        let fake = start_fake(&scratch);
        let storage_dir = scratch.path("storage");
        std::fs::create_dir(&storage_dir).expect("the storage fake's directory");
        let incus = FakeStorageIncus::start(&storage_dir).expect("the storage fake serves");
        incus.add_pool(
            "default",
            "dir",
            "",
            json!({ "source": "/var/lib/incus/storage-pools/default" }),
            40 * GB,
            511 * GB,
        );
        incus.add_pool(
            "fast",
            "btrfs",
            "",
            json!({ "source": "pool-uuid-fast", "volatile.initial_source": "/dev/sdf" }),
            300 * GB,
            1000 * GB,
        );
        incus.add_project("shop", json!({ "features.networks": "false" }));
        incus.add_instance(json!({
            "name": "web", "project": "shop", "type": "container", "status": "Running",
            "profiles": ["default"],
            "config": { "volatile.uuid": "00000000-0000-4000-8000-0000000000a1", "boot.autostart": "true" },
            "expanded_config": {
                "volatile.uuid": "00000000-0000-4000-8000-0000000000a1",
                "volatile.eth0.hwaddr": "00:16:3e:00:00:01",
                "boot.autostart": "true",
                "limits.memory": "2GiB"
            },
            "expanded_devices": {
                "root": { "type": "disk", "path": "/", "pool": "fast" },
                "eth0": { "type": "nic", "network": "incusbr0", "name": "eth0", "ipv4.address": "192.0.2.10" },
                "photos": { "type": "disk", "path": "/srv/photos", "pool": "fast", "source": "photos" }
            },
        }));
        incus.add_volume(
            "fast",
            json!({ "name": "photos", "project": "shop", "type": "custom", "content_type": "filesystem",
                    "config": { "size": "50GiB", "btrfs.mount_options": "x" } }),
        );
        let devices = Arc::new(FixedBlockDevices::new(host_disks()));
        let storage = Arc::new(
            Storage::new(incus.socket().to_path_buf(), devices.clone(), writes)
                .with_wait(Duration::from_secs(1)),
        );
        let verifier = verifier_for(&scratch, &[key], tag);
        let kernel = Arc::new(Mutex::new(
            kernel_with_image(&scratch, &fake, verifier, NOW, None).expect("the kernel opens"),
        ));
        let clock = Arc::new(AtomicU64::new(NOW));
        let handlers: std::collections::HashMap<&'static str, Arc<dyn ToolHandler>> =
            plan_handlers_full(
                Arc::clone(&kernel),
                Arc::new(move || clock.load(Ordering::SeqCst)),
                None,
                None,
                Some(Arc::clone(&storage)),
                ActionSlot::default(),
            )
            .into_iter()
            .collect();
        let executor = ActionExecutor::new(
            ["storage_add", "snapshot_create", "snapshot_delete"]
                .map(|name| (name, Arc::clone(&handlers[name]))),
        );
        Rig {
            _scratch: scratch,
            _fake: fake,
            incus,
            devices,
            kernel,
            handlers,
            executor,
            storage,
            nonces: AtomicU64::new(0),
        }
    }

    fn propose(&self, action: &str, args: Value) -> Result<Value, CallError> {
        call_verb(
            &self.handlers,
            "action_propose",
            json!({ "action": action, "args": args }),
        )
    }

    /// Approves a proposal with the owner's key and runs it once.
    fn approve_and_run(&self, key: &SkKey, proposed: &Value) -> Result<Value, CallError> {
        let id = proposed["id"].as_str().expect("an id");
        let digest = proposed["argsDigest"].as_str().expect("a digest");
        let action = proposed["action"].as_str().expect("an action");
        let statement = arca::handlers::action_statement(
            HOST,
            id,
            action,
            digest,
            NOW,
            &format!("{:032x}", self.nonces.fetch_add(1, Ordering::SeqCst)),
        )
        .expect("a kernel-built statement");
        let approval = approve_statement(key, &statement);
        let approved = {
            let mut kernel = self.kernel.lock().expect("the kernel lock");
            arca::handlers::approve_action(
                &mut kernel,
                &json!({ "plan": id, "approval": approval }),
                NOW,
            )
            .expect("the approval verifies")
        };
        self.executor
            .run(&approved.action, &approved.args, &approved.grant)
    }

    fn storage_list(&self) -> Value {
        StorageListHandler {
            storage: Arc::clone(&self.storage),
        }
        .call(json!({}), &caller(arca_api::Scope::Viewer))
        .expect("storage_list answers")
    }

    fn snapshot_list(&self) -> Value {
        SnapshotListHandler {
            storage: Arc::clone(&self.storage),
        }
        .call(json!({}), &caller(arca_api::Scope::Viewer))
        .expect("snapshot_list answers")
    }

    fn add_snapshot_pool(&self, name: &str) {
        self.incus.add_pool(
            name,
            "btrfs",
            SNAPSHOT_POOL_DESCRIPTION,
            json!({ "source": format!("uuid-{name}") }),
            0,
            1000 * GB,
        );
    }
}

fn refused_code(result: Result<Value, CallError>) -> String {
    match result {
        Err(CallError::Refused { code, .. }) => code,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn row<'a>(details: &'a Value, label: &str) -> &'a str {
    details
        .as_array()
        .expect("details")
        .iter()
        .find(|row| row["label"] == json!(label))
        .and_then(|row| row["value"].as_str())
        .unwrap_or_else(|| panic!("no {label} row in {details}"))
}

fn disk_view<'a>(list: &'a Value, name: &str) -> &'a Value {
    list["disks"]
        .as_array()
        .expect("disks")
        .iter()
        .find(|disk| disk["name"] == json!(name))
        .unwrap_or_else(|| panic!("no disk {name}"))
}

/// Every request a write path sent that was not a GET must be in the snapshot family: a pool or the snapshot project
/// created, or an instance or volume created or deleted in the snapshot project.
fn assert_write_family(incus: &FakeStorageIncus) {
    for write in incus.writes() {
        assert!(
            write_allowed(&write.method, &write.target),
            "{} {} is outside the snapshot family",
            write.method,
            write.target
        );
        let snapshot_project = write
            .target
            .ends_with(&format!("?project={SNAPSHOT_PROJECT}"));
        let pool_or_project = write.method == "POST"
            && (write.target == "/1.0/storage-pools" || write.target == "/1.0/projects");
        assert!(
            snapshot_project || pool_or_project,
            "{} {}",
            write.method,
            write.target
        );
        assert!(
            !write.target.contains("/state"),
            "nothing is ever started or stopped"
        );
        if write.target == "/1.0/projects" {
            assert_eq!(write.body["name"], json!(SNAPSHOT_PROJECT));
        }
    }
}

#[test]
fn every_disk_gets_its_plain_role() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("storage-roles", &key);
    let list = rig.storage_list();
    let roles: Vec<(String, String)> = list["disks"]
        .as_array()
        .expect("disks")
        .iter()
        .map(|disk| {
            (
                disk["name"].as_str().unwrap_or_default().to_string(),
                disk["role"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    let expected = [
        ("nvme0n1", "system"),
        ("sdb", "free"),
        ("sdc", "in use"),
        ("sdd", "system"),
        ("sde", "system"),
        ("sdf", "system"),
        ("sdg", "system"),
    ];
    assert_eq!(
        roles,
        expected.map(|(name, role)| (name.to_string(), role.to_string()))
    );
    assert!(
        !roles.iter().any(|(name, _)| name == "loop0"),
        "a loop device is not a disk"
    );

    let system = disk_view(&list, "nvme0n1");
    assert_eq!(system["offer"], Value::Null);
    assert!(
        system["reason"]
            .as_str()
            .unwrap_or("")
            .contains("holds /boot/efi")
    );
    assert!(
        disk_view(&list, "sdd")["reason"]
            .as_str()
            .unwrap_or("")
            .contains("RAID")
    );
    assert!(
        disk_view(&list, "sde")["reason"]
            .as_str()
            .unwrap_or("")
            .contains("LVM")
    );
    assert!(
        disk_view(&list, "sdf")["reason"]
            .as_str()
            .unwrap_or("")
            .contains("pool fast")
    );
    assert!(
        disk_view(&list, "sdg")["reason"]
            .as_str()
            .unwrap_or("")
            .contains("swap")
    );

    // A removable disk is classified like any other: free when nothing on it is mounted, in use when something is.
    let free = disk_view(&list, "sdb");
    assert_eq!(free["removable"], json!(true));
    assert_eq!(free["transport"], json!("usb"));
    assert_eq!(free["offer"], json!("whole disk"));
    assert_eq!(free["children"][0]["label"], json!("PHOTOS"));
    let mounted = disk_view(&list, "sdc");
    assert_eq!(mounted["removable"], json!(true));
    assert_eq!(mounted["offer"], json!("folder"));
    assert_eq!(mounted["folders"], json!(["/media/owner/BACKUP"]));

    let fast = list["pools"]
        .as_array()
        .expect("pools")
        .iter()
        .find(|pool| pool["name"] == json!("fast"))
        .expect("the fast pool");
    assert_eq!(fast["used"], json!(300 * GB));
    assert_eq!(fast["total"], json!(1000 * GB));
    assert_eq!(fast["snapshots"], json!(false));
    assert!(rig.incus.writes().is_empty(), "a listing writes nothing");
}

#[test]
fn a_whole_disk_plan_says_everything_will_be_erased_and_incus_formats_it() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("storage-add-device", &key);
    let proposed = rig
        .propose("storage_add", json!({ "name": "usb1", "device": "sdb" }))
        .expect("a free disk may be proposed");
    let details = &proposed["details"];
    assert_eq!(row(details, "Disk"), "Acme Portable 1T (1000.0 GB, USB)");
    assert_eq!(row(details, "Device"), "/dev/sdb");
    assert_eq!(row(details, "On it now"), "sdb1 ext4 \"PHOTOS\" 1000.0 GB");
    assert!(row(details, "Effect").starts_with(ERASE_SENTENCE));
    assert_eq!(ERASE_SENTENCE, "Everything on this disk will be erased.");
    assert!(
        proposed["summary"].as_str().is_some_and(
            |summary| summary.starts_with("Erase the disk Acme Portable 1T (/dev/sdb)")
        )
    );
    assert_eq!(
        proposed["args"]["serial"],
        json!("FAKE-SDB"),
        "the disk's identity is bound"
    );
    assert!(rig.incus.writes().is_empty(), "a proposal changes nothing");

    let result = rig
        .approve_and_run(&key, &proposed)
        .expect("the approved add runs");
    assert_eq!(
        result,
        json!({ "pool": "usb1", "driver": "btrfs", "source": "/dev/sdb" })
    );
    let pool = rig.incus.pool("usb1").expect("Incus created the pool");
    assert_eq!(pool["driver"], json!("btrfs"));
    assert_eq!(pool["description"], json!(SNAPSHOT_POOL_DESCRIPTION));
    assert!(
        pool["config"]["btrfs.mount_options"]
            .as_str()
            .is_some_and(|options| options.contains("compress=zstd"))
    );
    let writes = rig.incus.writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(
        (writes[0].method.as_str(), writes[0].target.as_str()),
        ("POST", "/1.0/storage-pools")
    );
    assert_write_family(&rig.incus);
}

#[test]
fn a_folder_plan_keeps_the_rest_of_the_disk() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("storage-add-folder", &key);
    let proposed = rig
        .propose(
            "storage_add",
            json!({ "name": "stick", "path": "/media/owner/BACKUP/" }),
        )
        .expect("a folder on a mounted disk may be proposed");
    let details = &proposed["details"];
    assert_eq!(row(details, "Disk"), "Acme Stick 64G (64.0 GB, USB)");
    assert_eq!(row(details, "Folder"), "/media/owner/BACKUP/arca-pool");
    assert_eq!(row(details, "Mounted at"), "/media/owner/BACKUP");
    assert!(
        !details.to_string().contains(ERASE_SENTENCE),
        "a folder target erases nothing"
    );
    let result = rig
        .approve_and_run(&key, &proposed)
        .expect("the approved add runs");
    assert_eq!(result["driver"], json!("dir"));
    let pool = rig.incus.pool("stick").expect("Incus created the pool");
    assert_eq!(
        pool["config"]["source"],
        json!("/media/owner/BACKUP/arca-pool")
    );
    assert_write_family(&rig.incus);
}

#[test]
fn system_member_mounted_and_unlisted_disks_are_refused() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("storage-add-refusals", &key);
    let cases = [
        (json!({ "name": "x", "device": "nvme0n1" }), "SystemDisk"),
        (json!({ "name": "x", "device": "/dev/sdd" }), "SystemDisk"),
        (json!({ "name": "x", "device": "sde" }), "SystemDisk"),
        (json!({ "name": "x", "device": "sdf" }), "SystemDisk"),
        (json!({ "name": "x", "device": "sdg" }), "SystemDisk"),
        (json!({ "name": "x", "device": "sdc" }), "DiskMounted"),
        (json!({ "name": "x", "device": "sdb1" }), "NotWholeDisk"),
        (json!({ "name": "x", "device": "sdz" }), "DiskNotListed"),
        (json!({ "name": "x", "device": "loop0" }), "DiskNotListed"),
        (json!({ "name": "x", "path": "/home/owner" }), "SystemDisk"),
        (json!({ "name": "x", "path": "/srv/data" }), "SystemDisk"),
        (json!({ "name": "fast", "device": "sdb" }), "PoolExists"),
    ];
    for (args, code) in cases {
        assert_eq!(
            refused_code(rig.propose("storage_add", args.clone())),
            code,
            "{args}"
        );
    }
    for args in [
        json!({ "name": "x", "device": "sdb", "path": "/media/owner/BACKUP" }),
        json!({ "name": "x" }),
        json!({ "name": "x", "path": "/media/owner/../owner" }),
        json!({ "name": "Bad Name", "device": "sdb" }),
    ] {
        let refused = rig
            .propose("storage_add", args.clone())
            .expect_err("refused");
        assert!(
            matches!(refused, CallError::Invalid(_)),
            "{args}: {refused:?}"
        );
    }
    assert!(rig.incus.writes().is_empty(), "no refusal wrote anything");

    // A direct call without the owner's one-shot grant is refused before any write.
    let direct = call_verb(
        &rig.handlers,
        "storage_add",
        json!({ "name": "usb1", "device": "sdb", "disk": "sdb", "model": "Acme Portable 1T",
                "serial": "FAKE-SDB", "transport": "usb", "size": 1000 * GB,
                "contents": "sdb1 ext4 \"PHOTOS\" 1000.0 GB", "approval": "not-a-grant" }),
    );
    assert_eq!(refused_code(direct), "NotApproved");
    assert!(rig.incus.writes().is_empty());
}

#[test]
fn a_disk_swapped_or_mounted_after_the_proposal_is_refused_at_run() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("storage-add-changed", &key);
    let proposed = rig
        .propose("storage_add", json!({ "name": "usb1", "device": "sdb" }))
        .expect("a proposal");
    // Another disk now answers to sdb: same name, another serial.
    let mut swapped = host_disks();
    swapped["blockdevices"][1]["serial"] = json!("FAKE-OTHER");
    rig.devices.set(Ok(swapped));
    assert_eq!(
        refused_code(rig.approve_and_run(&key, &proposed)),
        "DiskChanged"
    );

    let again = rig
        .propose("storage_add", json!({ "name": "usb2", "device": "sdb" }))
        .expect("a fresh proposal");
    // The disk got mounted after the proposal: it is no longer free.
    let mut mounted = host_disks();
    mounted["blockdevices"][1]["children"][0]["mountpoints"] = json!(["/media/owner/PHOTOS"]);
    rig.devices.set(Ok(mounted));
    assert_eq!(
        refused_code(rig.approve_and_run(&key, &again)),
        "DiskMounted"
    );
    assert!(rig.incus.writes().is_empty(), "nothing was formatted");
}

#[test]
fn a_watch_only_node_lists_but_never_writes() {
    let key = SkKey::from_seed(1);
    let rig = Rig::with_writes("storage-watch-only", &key, false);
    assert_eq!(
        rig.storage_list()["disks"].as_array().map(Vec::len),
        Some(7)
    );
    assert_eq!(
        refused_code(rig.propose("storage_add", json!({ "name": "usb1", "device": "sdb" }))),
        "StorageDisabled"
    );
    assert_eq!(
        refused_code(rig.propose(
            "snapshot_create",
            json!({ "project": "shop", "instance": "web", "pool": "fast" })
        )),
        "StorageDisabled"
    );
    assert!(rig.incus.writes().is_empty());
}

#[test]
fn an_app_snapshot_is_a_stopped_self_contained_copy_and_the_source_is_only_read() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("snapshot-instance", &key);
    rig.add_snapshot_pool("usb1");
    let source_before = rig.incus.instance("shop", "web").expect("the source");
    let proposed = rig
        .propose(
            "snapshot_create",
            json!({ "project": "shop", "instance": "web", "pool": "usb1" }),
        )
        .expect("a snapshot may be proposed");
    let name = snapshot_name("shop", "web", NOW).expect("a name");
    assert_eq!(name, "shop--web--20261030T1200");
    assert_eq!(proposed["args"]["name"], json!(name));
    assert_eq!(proposed["args"]["kind"], json!("container"));
    let details = &proposed["details"];
    assert_eq!(
        row(details, "Source"),
        "App web (container) in project shop"
    );
    assert_eq!(row(details, "Copy to"), "Snapshot storage usb1");
    assert_eq!(row(details, "Snapshot name"), name);
    assert!(row(details, "Effect").contains("never starts on its own"));
    assert!(row(details, "Effect").contains("not stopped or changed"));
    assert_eq!(
        proposed["summary"],
        json!("Snapshot the app web from project shop onto usb1")
    );

    let result = rig
        .approve_and_run(&key, &proposed)
        .expect("the approved copy runs");
    assert_eq!(result["state"], json!("copied"));
    let project = rig
        .incus
        .project(SNAPSHOT_PROJECT)
        .expect("the snapshot project was created");
    assert_eq!(project["config"]["features.networks"], json!("false"));
    let copy = rig
        .incus
        .instance(SNAPSHOT_PROJECT, &name)
        .expect("the copy");
    assert_eq!(
        copy["status"],
        json!("Stopped"),
        "the copy was never started"
    );
    assert_eq!(copy["config"]["boot.autostart"], json!("false"));
    assert_eq!(copy["profiles"], json!([]));
    assert_eq!(copy["devices"]["root"]["pool"], json!("usb1"));
    assert!(
        copy["devices"].get("photos").is_none(),
        "the source project's volume is left out"
    );
    assert!(
        copy["devices"]["eth0"].get("ipv4.address").is_none(),
        "the source's address is not claimed"
    );
    assert!(
        source_dependencies(&copy, "shop").is_empty(),
        "the copy depends on nothing in the source project: {copy}"
    );
    assert_eq!(
        rig.incus.instance("shop", "web").expect("the source"),
        source_before,
        "the source is unchanged"
    );
    let writes = rig.incus.writes();
    let shapes: Vec<(String, String)> = writes
        .iter()
        .map(|write| (write.method.clone(), write.target.clone()))
        .collect();
    assert_eq!(
        shapes,
        vec![
            ("POST".to_string(), "/1.0/projects".to_string()),
            (
                "POST".to_string(),
                format!("/1.0/instances?project={SNAPSHOT_PROJECT}")
            ),
        ]
    );
    assert!(
        writes[1].body.get("start").is_none(),
        "the create never asks to start"
    );
    assert!(
        !rig.incus
            .requests()
            .iter()
            .any(|request| request.method != "GET" && request.target.contains("project=shop")),
        "no write ever named the source project"
    );
    assert_write_family(&rig.incus);

    // The same snapshot name in the same minute is refused rather than overwritten.
    assert_eq!(
        refused_code(rig.propose(
            "snapshot_create",
            json!({ "project": "shop", "instance": "web", "pool": "usb1" })
        )),
        "SnapshotExists"
    );
}

#[test]
fn a_data_volume_snapshot_copies_only_the_volume() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("snapshot-volume", &key);
    rig.add_snapshot_pool("usb1");
    let proposed = rig
        .propose(
            "snapshot_create",
            json!({ "project": "shop", "volume": "photos", "pool": "usb1" }),
        )
        .expect("a volume snapshot may be proposed");
    assert_eq!(proposed["args"]["sourcePool"], json!("fast"));
    assert_eq!(
        row(&proposed["details"], "Source"),
        "Data volume photos on pool fast in project shop"
    );
    let result = rig
        .approve_and_run(&key, &proposed)
        .expect("the approved copy runs");
    assert_eq!(result["kind"], json!("volume"));
    let copy = rig
        .incus
        .volume("usb1", SNAPSHOT_PROJECT, "shop--photos--20261030T1200")
        .expect("the copy");
    assert_eq!(copy["config"]["size"], json!("50GiB"));
    assert!(copy["config"].get("btrfs.mount_options").is_none());
    assert!(
        rig.incus.volume("fast", "shop", "photos").is_some(),
        "the source stays"
    );
    assert_write_family(&rig.incus);
}

#[test]
fn snapshots_go_only_to_added_storage_and_only_from_real_sources() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("snapshot-refusals", &key);
    rig.add_snapshot_pool("usb1");
    let cases = [
        (
            json!({ "project": "shop", "instance": "web", "pool": "fast" }),
            "NotSnapshotStorage",
        ),
        (
            json!({ "project": "shop", "instance": "web", "pool": "nope" }),
            "NotSnapshotStorage",
        ),
        (
            json!({ "project": "shop", "instance": "ghost", "pool": "usb1" }),
            "SourceUnknown",
        ),
        (
            json!({ "project": "shop", "volume": "ghost", "pool": "usb1" }),
            "SourceUnknown",
        ),
    ];
    for (args, code) in cases {
        assert_eq!(
            refused_code(rig.propose("snapshot_create", args.clone())),
            code,
            "{args}"
        );
    }
    for args in [
        json!({ "project": "shop", "instance": "web", "volume": "photos", "pool": "usb1" }),
        json!({ "project": "shop", "pool": "usb1" }),
    ] {
        assert!(matches!(
            rig.propose("snapshot_create", args),
            Err(CallError::Invalid(_))
        ));
    }
    assert!(rig.incus.writes().is_empty());
}

#[test]
fn a_failed_or_slow_copy_is_reported_plainly() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("snapshot-slow", &key);
    rig.add_snapshot_pool("usb1");
    let proposed = rig
        .propose(
            "snapshot_create",
            json!({ "project": "shop", "instance": "web", "pool": "usb1" }),
        )
        .expect("a proposal");
    rig.incus
        .fail_next_copy("Failed to copy: no space left on device");
    match rig.approve_and_run(&key, &proposed) {
        Err(CallError::Refused { code, message }) => {
            assert_eq!(code, "StorageWriteFailed");
            assert!(message.contains("no space left"), "{message}");
        }
        other => panic!("expected a failed copy, got {other:?}"),
    }

    let rig = Rig::new("snapshot-slow-held", &key);
    rig.add_snapshot_pool("usb1");
    rig.incus.hold_operations();
    let proposed = rig
        .propose(
            "snapshot_create",
            json!({ "project": "shop", "instance": "web", "pool": "usb1" }),
        )
        .expect("a proposal");
    let result = rig
        .approve_and_run(&key, &proposed)
        .expect("a long copy keeps going");
    assert_eq!(result["state"], json!("copying"));
}

#[test]
fn snapshot_list_and_delete_stay_inside_the_snapshot_project() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("snapshot-delete", &key);
    rig.add_snapshot_pool("usb1");
    assert_eq!(
        rig.snapshot_list(),
        json!({ "snapshots": [] }),
        "no project, no snapshots"
    );
    for args in [
        json!({ "project": "shop", "instance": "web", "pool": "usb1" }),
        json!({ "project": "shop", "volume": "photos", "pool": "usb1" }),
    ] {
        let proposed = rig.propose("snapshot_create", args).expect("a proposal");
        rig.approve_and_run(&key, &proposed).expect("the copy runs");
    }
    rig.incus
        .set_usage(SNAPSHOT_PROJECT, "shop--web--20261030T1200", 3 * GB);
    let list = rig.snapshot_list();
    let snapshots = list["snapshots"].as_array().expect("snapshots");
    assert_eq!(snapshots.len(), 2);
    let app = &snapshots[0];
    assert_eq!(app["name"], json!("shop--web--20261030T1200"));
    assert_eq!(app["kind"], json!("container"));
    assert_eq!(app["sourceProject"], json!("shop"));
    assert_eq!(app["sourceName"], json!("web"));
    assert_eq!(app["pool"], json!("usb1"));
    assert_eq!(app["size"], json!(3 * GB));
    assert_eq!(app["created"], json!("2026-10-30T12:00:00.000Z"));
    assert_eq!(app["droppedDevices"], json!(["photos"]));
    assert_eq!(snapshots[1]["kind"], json!("volume"));
    assert_eq!(snapshots[1]["sourcePool"], json!("fast"));

    // Only a name in the snapshot project can be deleted: the source app's own name is unknown here.
    assert_eq!(
        refused_code(rig.propose("snapshot_delete", json!({ "name": "web" }))),
        "SnapshotUnknown"
    );
    let writes_before = rig.incus.writes().len();
    let proposed = rig
        .propose(
            "snapshot_delete",
            json!({ "name": "shop--web--20261030T1200" }),
        )
        .expect("a delete proposal");
    assert_eq!(proposed["args"]["kind"], json!("instance"));
    assert_eq!(proposed["args"]["pool"], json!("usb1"));
    assert_eq!(proposed["args"]["source"], json!("shop/web"));
    assert!(row(&proposed["details"], "Effect").contains("is not touched"));
    let result = rig
        .approve_and_run(&key, &proposed)
        .expect("the delete runs");
    assert_eq!(result["state"], json!("deleted"));
    assert!(
        rig.incus
            .instance(SNAPSHOT_PROJECT, "shop--web--20261030T1200")
            .is_none()
    );
    assert!(
        rig.incus.instance("shop", "web").is_some(),
        "the source app is untouched"
    );

    let proposed = rig
        .propose(
            "snapshot_delete",
            json!({ "name": "shop--photos--20261030T1200" }),
        )
        .expect("a delete proposal");
    rig.approve_and_run(&key, &proposed)
        .expect("the delete runs");
    assert!(
        rig.incus.volume("fast", "shop", "photos").is_some(),
        "the source volume is untouched"
    );
    let deletes: Vec<String> = rig.incus.writes()[writes_before..]
        .iter()
        .map(|write| format!("{} {}", write.method, write.target))
        .collect();
    assert_eq!(
        deletes,
        vec![
            format!("DELETE /1.0/instances/shop--web--20261030T1200?project={SNAPSHOT_PROJECT}"),
            format!(
                "DELETE /1.0/storage-pools/usb1/volumes/custom/shop--photos--20261030T1200?project={SNAPSHOT_PROJECT}"
            ),
        ]
    );
    assert_eq!(rig.snapshot_list(), json!({ "snapshots": [] }));
    assert_write_family(&rig.incus);
}

#[test]
fn the_doctor_may_propose_the_three_storage_actions() {
    for name in ["storage_add", "snapshot_create", "snapshot_delete"] {
        let action = ActionKind::from_name(name).expect("on the propose allow-list");
        assert!(action.is_storage());
        assert!(ActionKind::ALL.contains(&action));
        assert!(
            arca_approval::ArcaAction::from_name(name).is_some(),
            "a statement can name {name}"
        );
    }
    let propose = arca_api::verb("action_propose").expect("the verb");
    let allowed = (propose.params)()["properties"]["action"]["enum"].clone();
    for name in ["storage_add", "snapshot_create", "snapshot_delete"] {
        assert!(
            allowed
                .as_array()
                .is_some_and(|list| list.contains(&json!(name))),
            "{name}"
        );
    }
    for name in ["storage_list", "snapshot_list"] {
        let verb = arca_api::verb(name).expect("the verb");
        assert_eq!(verb.min_scope, arca_api::Scope::Viewer);
        assert!(!verb.writes);
    }
}
