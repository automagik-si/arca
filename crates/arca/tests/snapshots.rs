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
    DeviceLinks, ERASE_SENTENCE, ExclusiveOpen, FixedBlockDevices, SYSTEM_LISTS_SENTENCE,
    SnapshotListHandler, Storage, StorageListHandler, SystemLists, snapshot_name,
};
use arca_incus::fake_storage::FakeStorageIncus;
use arca_incus::snapshot::{
    SNAPSHOT_POOL_DESCRIPTION, SNAPSHOT_PROJECT, SNAPSHOT_STORAGE_KEY, source_dependencies,
    write_allowed,
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
/// Incus pool, a disk holding swap, a disk another machine made snapshot storage (its btrfs label says so), a disk holding
/// this machine's snapshot pool, and a loop device that is never listed.
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
        disk("sdh", "Acme Portable 2T", "usb", true, 2000 * GB, json!({
            "fstype": "btrfs", "label": "arca-snapshots-office", "uuid": "uuid-office",
        })),
        disk("sdi", "Acme Portable 4T", "usb", true, 4000 * GB, json!({
            "fstype": "btrfs", "uuid": "uuid-vault",
        })),
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
            // The app's own (local) config and devices: live marks, a data volume, a proxy and a host folder. Incus carries
            // every one of them into a copy whose request leaves them unset.
            "config": {
                "volatile.uuid": "00000000-0000-4000-8000-0000000000a1",
                "volatile.idmap.base": "0",
                "boot.autostart": "true",
                "user.arca.app": "web",
                "user.arcakernel.entry": "service-web",
                "user.droid.slot": "7",
                "user.tactu.owner": "shop"
            },
            "devices": {
                "photos": { "type": "disk", "path": "/srv/photos", "pool": "fast", "source": "photos" },
                "http": { "type": "proxy", "listen": "tcp:0.0.0.0:8080", "connect": "tcp:127.0.0.1:80" },
                "share": { "type": "disk", "path": "/srv/share", "source": "/srv/share" }
            },
            "expanded_config": {
                "volatile.uuid": "00000000-0000-4000-8000-0000000000a1",
                "volatile.idmap.base": "0",
                "volatile.eth0.hwaddr": "00:16:3e:00:00:01",
                "boot.autostart": "true",
                "limits.memory": "2GiB",
                "user.arca.app": "web",
                "user.arcakernel.entry": "service-web",
                "user.droid.slot": "7",
                "user.tactu.owner": "shop"
            },
            "expanded_devices": {
                "root": { "type": "disk", "path": "/", "pool": "fast" },
                "eth0": { "type": "nic", "network": "incusbr0", "name": "eth0", "ipv4.address": "192.0.2.10" },
                "photos": { "type": "disk", "path": "/srv/photos", "pool": "fast", "source": "photos" },
                "http": { "type": "proxy", "listen": "tcp:0.0.0.0:8080", "connect": "tcp:127.0.0.1:80" },
                "share": { "type": "disk", "path": "/srv/share", "source": "/srv/share" }
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
                .with_wait(Duration::from_secs(1))
                .with_helper_pools(vec!["missing".to_string(), "default".to_string()]),
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
            json!({ "source": format!("uuid-{name}"), SNAPSHOT_STORAGE_KEY: name }),
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
/// created, an instance or volume created or deleted in the snapshot project, or a new copy rewritten there once.
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
        // Only a folder copy helper is ever started, stopped or given a command; a snapshot copy never is.
        if write.target.contains("/state") || write.target.contains("/exec") {
            assert!(
                write.target.starts_with("/1.0/instances/arca-copy-"),
                "{} {}: only a copy helper is started, stopped or run",
                write.method,
                write.target
            );
        }
        if write.target == "/1.0/projects" {
            assert_eq!(write.body["name"], json!(SNAPSHOT_PROJECT));
        }
    }
}

#[test]
fn every_disk_gets_its_plain_role() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("storage-roles", &key);
    rig.add_snapshot_pool("vault");
    // A pool that only says "Arca snapshot storage" in its editable description is not snapshot storage.
    rig.incus.add_pool(
        "lookalike",
        "btrfs",
        SNAPSHOT_POOL_DESCRIPTION,
        json!({ "source": "uuid-sdd-none" }),
        0,
        GB,
    );
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
        ("sdh", "snapshots"),
        ("sdi", "snapshots"),
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
    for name in ["sdh", "sdi"] {
        assert_eq!(disk_view(&list, name)["offer"], Value::Null, "{name}");
    }
    assert!(
        disk_view(&list, "sdh")["reason"]
            .as_str()
            .unwrap_or("")
            .contains("label arca-snapshots-office")
    );
    assert!(
        disk_view(&list, "sdi")["reason"]
            .as_str()
            .unwrap_or("")
            .contains("snapshot storage vault")
    );
    let pools = list["pools"].as_array().expect("pools");
    let snapshots_of = |name: &str| {
        pools
            .iter()
            .find(|pool| pool["name"] == json!(name))
            .map(|pool| pool["snapshots"].clone())
    };
    assert_eq!(snapshots_of("vault"), Some(json!(true)));
    assert_eq!(snapshots_of("lookalike"), Some(json!(false)));
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
    assert_eq!(
        row(details, "Storage name"),
        "arca-snapshots-usb1",
        "the disk's label will say it holds snapshots"
    );
    assert_eq!(proposed["args"]["name"], json!("arca-snapshots-usb1"));
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
        json!({ "pool": "arca-snapshots-usb1", "driver": "btrfs", "source": "/dev/sdb" })
    );
    let pool = rig
        .incus
        .pool("arca-snapshots-usb1")
        .expect("Incus created the pool");
    assert_eq!(pool["driver"], json!("btrfs"));
    assert_eq!(pool["description"], json!(SNAPSHOT_POOL_DESCRIPTION));
    assert_eq!(
        pool["config"][SNAPSHOT_STORAGE_KEY],
        json!("arca-snapshots-usb1")
    );
    assert_eq!(
        pool["config"]["source.wipe"],
        json!("true"),
        "Incus wipes the old partition table before it formats"
    );
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

fn folder_source(path: &str) -> arca_incus::PoolSource {
    arca_incus::PoolSource::Folder {
        path: path.to_string(),
        fs_uuid: None,
    }
}

/// The host's disks with the stick's partition (sdc1) formatted ext4 and mounted at `mount`.
fn disks_with_stick_at(mount: Option<&str>) -> Value {
    let mut disks = host_disks();
    disks["blockdevices"][2]["children"][0]["fstype"] = json!("ext4");
    disks["blockdevices"][2]["children"][0]["mountpoints"] = match mount {
        Some(mount) => json!([mount]),
        None => json!([null]),
    };
    disks
}

/// A real folder stands in for the stick's mount point (sdc1, ext4), so links and folder rights in it are real.
fn stick_mount(rig: &Rig) -> String {
    let path = rig._scratch.path("stick");
    std::fs::create_dir(&path).expect("the mount folder");
    let mount = std::fs::canonicalize(path).expect("a real path");
    let mount = mount.to_str().expect("utf-8").to_string();
    rig.devices.set(Ok(disks_with_stick_at(Some(&mount))));
    mount
}

fn mode_of(path: &str) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::symlink_metadata(path)
        .expect("there")
        .permissions()
        .mode()
        & 0o7777
}

fn running_as_root() -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").is_ok_and(|me| me.uid() == 0)
}

#[test]
fn a_folder_plan_keeps_the_rest_of_the_disk() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("storage-add-folder", &key);
    let mount = stick_mount(&rig);
    let proposed = rig
        .propose(
            "storage_add",
            json!({ "name": "stick", "path": format!("{mount}/") }),
        )
        .expect("a folder on a mounted disk may be proposed");
    let details = &proposed["details"];
    assert_eq!(row(details, "Disk"), "Acme Stick 64G (64.0 GB, USB)");
    assert_eq!(row(details, "On it now"), "sdc1 ext4 \"BACKUP\" 64.0 GB");
    assert_eq!(row(details, "Folder"), format!("{mount}/arca-pool"));
    assert_eq!(
        proposed["args"]["fsUuid"],
        json!("uuid-sdc1"),
        "the owner approves the file system"
    );
    assert_eq!(row(details, "Mounted at"), mount);
    assert!(
        !details.to_string().contains(ERASE_SENTENCE),
        "a folder target erases nothing"
    );
    assert!(
        std::fs::symlink_metadata(format!("{mount}/arca-pool")).is_err(),
        "a proposal writes nothing"
    );
    let result = rig
        .approve_and_run(&key, &proposed)
        .expect("the approved add runs");
    assert_eq!(result["driver"], json!("dir"));
    let pool = rig.incus.pool("stick").expect("Incus created the pool");
    assert_eq!(
        pool["config"]["source"],
        json!(format!("{mount}/arca-pool"))
    );
    assert_eq!(pool["config"][SNAPSHOT_STORAGE_KEY], json!("stick"));
    assert_eq!(pool["config"][arca_incus::FS_UUID_KEY], json!("uuid-sdc1"));
    assert!(pool["config"].get("source.wipe").is_none());
    // Incus makes a folder pool only in a folder that is there and empty: Arca made it, for Incus alone.
    assert_eq!(mode_of(&format!("{mount}/arca-pool")), 0o700);
    assert_write_family(&rig.incus);
}

#[test]
fn incus_refuses_a_folder_pool_whose_folder_is_missing() {
    // The fake answers as Incus's dir driver does, so a create without the folder fails as it does on a real host.
    let scratch = Scratch::new("storage-fake-dir-source");
    let dir = scratch.path("incus");
    std::fs::create_dir(&dir).expect("the fake's directory");
    let incus = FakeStorageIncus::start(&dir).expect("the storage fake serves");
    let writer = arca_incus::SnapshotWriter::new(incus.socket().to_path_buf());
    let folder = scratch.path("stick");
    std::fs::create_dir(&folder).expect("a folder");
    let folder = folder.to_str().expect("utf-8").to_string();
    let missing = writer
        .create_pool("a", &folder_source(&folder))
        .expect_err("no arca-pool folder");
    assert!(missing.to_string().contains("doesn't exist"), "{missing}");
    std::fs::create_dir(format!("{folder}/arca-pool")).expect("arca-pool");
    std::fs::write(format!("{folder}/arca-pool/old"), "x").expect("a file");
    let full = writer
        .create_pool("a", &folder_source(&folder))
        .expect_err("arca-pool has a file in it");
    assert!(full.to_string().contains("isn't empty"), "{full}");
    std::fs::remove_file(format!("{folder}/arca-pool/old")).expect("remove");
    writer
        .create_pool("a", &folder_source(&folder))
        .expect("an empty arca-pool folder is fine");
}

#[test]
fn arca_makes_the_pool_folder_and_refuses_one_it_cannot_use() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("storage-add-pool-folder", &key);
    let mount = stick_mount(&rig);

    // An arca-pool that is not an empty plain folder is never used.
    std::fs::create_dir_all(format!("{mount}/full/arca-pool")).expect("a folder");
    std::fs::write(format!("{mount}/full/arca-pool/photo.jpg"), "x").expect("a file");
    std::fs::create_dir(format!("{mount}/file")).expect("a folder");
    std::fs::write(format!("{mount}/file/arca-pool"), "x").expect("a file");
    std::fs::create_dir(format!("{mount}/link")).expect("a folder");
    std::fs::create_dir(format!("{mount}/elsewhere")).expect("a folder");
    std::os::unix::fs::symlink(
        format!("{mount}/elsewhere"),
        format!("{mount}/link/arca-pool"),
    )
    .expect("a link");
    for folder in ["full", "file", "link"] {
        assert_eq!(
            refused_code(rig.propose(
                "storage_add",
                json!({ "name": folder, "path": format!("{mount}/{folder}") })
            )),
            "PoolFolderInUse",
            "{folder}"
        );
    }

    // A folder whose arca-pool becomes unusable after the proposal is refused when it runs, before Incus is asked.
    std::fs::create_dir(format!("{mount}/later")).expect("a folder");
    let proposed = rig
        .propose(
            "storage_add",
            json!({ "name": "later", "path": format!("{mount}/later") }),
        )
        .expect("fine when proposed");
    std::fs::create_dir(format!("{mount}/later/arca-pool")).expect("made by someone else");
    std::fs::write(format!("{mount}/later/arca-pool/x"), "x").expect("a file");
    let writes = rig.incus.writes().len();
    assert_eq!(
        refused_code(rig.approve_and_run(&key, &proposed)),
        "PoolFolderInUse"
    );
    assert_eq!(rig.incus.writes().len(), writes, "Incus was not asked");

    // A folder Arca may not write in is refused when it is proposed, with a plain reason.
    if !running_as_root() {
        use std::os::unix::fs::PermissionsExt;
        let locked = format!("{mount}/locked");
        std::fs::create_dir(&locked).expect("a folder");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).expect("lock");
        for path in [locked.clone(), format!("{locked}/inside")] {
            match rig.propose("storage_add", json!({ "name": "locked", "path": path })) {
                Err(CallError::Refused { code, message }) => {
                    assert_eq!(code, "FolderNotWritable");
                    assert!(message.contains(&locked), "{message}");
                }
                other => panic!("expected a refusal, got {other:?}"),
            }
        }
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("unlock");
    }
    assert!(rig.incus.writes().is_empty(), "nothing was asked of Incus");

    // A folder that is not there yet is made, with every missing part, when the approved add runs.
    let proposed = rig
        .propose(
            "storage_add",
            json!({ "name": "deep", "path": format!("{mount}/snaps/arca") }),
        )
        .expect("a new folder on the stick");
    assert!(std::fs::symlink_metadata(format!("{mount}/snaps")).is_err());
    rig.approve_and_run(&key, &proposed).expect("the add runs");
    assert_eq!(mode_of(&format!("{mount}/snaps/arca/arca-pool")), 0o700);
    assert!(rig.incus.pool("deep").is_some());
    assert_write_family(&rig.incus);

    // An empty arca-pool folder already there is used as it is (on a fresh stick: the one above now holds snapshots).
    let rig = Rig::new("storage-add-pool-folder-empty", &key);
    let mount = stick_mount(&rig);
    std::fs::create_dir_all(format!("{mount}/empty/arca-pool")).expect("an empty one");
    let proposed = rig
        .propose(
            "storage_add",
            json!({ "name": "empty", "path": format!("{mount}/empty") }),
        )
        .expect("an empty arca-pool");
    rig.approve_and_run(&key, &proposed).expect("the add runs");
    assert!(rig.incus.pool("empty").is_some());
}

#[test]
fn a_folder_is_judged_by_where_its_links_lead() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("storage-add-link", &key);
    let mount = stick_mount(&rig);
    std::fs::create_dir(format!("{mount}/photos")).expect("a folder on the stick");
    std::os::unix::fs::symlink("/etc", format!("{mount}/escape")).expect("a link");

    assert_eq!(
        refused_code(rig.propose(
            "storage_add",
            json!({ "name": "stick", "path": format!("{mount}/escape") })
        )),
        "SystemDisk",
        "a folder that leads onto the system disk is refused"
    );
    let proposed = rig
        .propose(
            "storage_add",
            json!({ "name": "stick", "path": format!("{mount}/photos") }),
        )
        .expect("a real folder on the stick");
    assert_eq!(proposed["args"]["path"], json!(format!("{mount}/photos")));
    // The folder is swapped for a link to the system disk after the proposal.
    std::fs::remove_dir(format!("{mount}/photos")).expect("remove");
    std::os::unix::fs::symlink("/etc", format!("{mount}/photos")).expect("a link");
    assert_eq!(
        refused_code(rig.approve_and_run(&key, &proposed)),
        "DiskChanged"
    );
    assert!(rig.incus.writes().is_empty());
}

#[test]
fn a_disk_that_holds_snapshots_is_never_erased() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("storage-add-snapshot-disk", &key);
    rig.add_snapshot_pool("vault");
    for device in ["sdh", "sdi"] {
        assert_eq!(
            refused_code(rig.propose("storage_add", json!({ "name": "again", "device": device }))),
            "SnapshotDisk",
            "{device}"
        );
    }
    assert!(rig.incus.writes().is_empty());
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
        (json!({ "name": "x", "device": "sdh" }), "SnapshotDisk"),
        (
            json!({ "name": "x", "path": "/media/owner/BACKUP/snaps" }),
            "FolderFileSystem",
        ),
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
        json!({ "name": "a-name-too-long-for-it", "device": "sdb" }),
    ] {
        let refused = rig
            .propose("storage_add", args.clone())
            .expect_err("refused");
        assert!(
            matches!(refused, CallError::Invalid(_)),
            "{args}: {refused:?}"
        );
    }
    // On a Linux file system, a folder pool still needs a name no pool has.
    rig.devices
        .set(Ok(disks_with_stick_at(Some("/media/owner/BACKUP"))));
    assert_eq!(
        refused_code(rig.propose(
            "storage_add",
            json!({ "name": "fast", "path": "/media/owner/BACKUP" })
        )),
        "PoolExists"
    );
    rig.devices.set(Ok(host_disks()));
    assert!(rig.incus.writes().is_empty(), "no refusal wrote anything");

    // A direct call without the owner's one-shot grant is refused before any write.
    let direct = call_verb(
        &rig.handlers,
        "storage_add",
        json!({ "name": "usb1", "device": "sdb", "disk": "sdb", "model": "Acme Portable 1T",
                "serial": "FAKE-SDB", "transport": "usb", "size": 1000 * GB,
                "contents": "sdb1 ext4 \"PHOTOS\" 1000.0 GB", "inUseCheck": "exclusive open",
                "approval": "not-a-grant" }),
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
        Some(9)
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
    for device in ["photos", "http", "share"] {
        for field in ["devices", "expanded_devices"] {
            assert!(
                copy[field].get(device).is_none(),
                "the source's {device} is left out of {field}: {copy}"
            );
        }
    }
    assert_eq!(
        copy["devices"].as_object().map(|devices| devices.len()),
        Some(2),
        "only the root disk and the NIC: {copy}"
    );
    for field in ["config", "expanded_config"] {
        for key in copy[field].as_object().expect("config").keys() {
            assert!(
                !arca_incus::snapshot::ownership_key(key),
                "the copy carries the live mark {key}"
            );
        }
    }
    for key in [
        "user.arca.app",
        "user.arcakernel.entry",
        "user.droid.slot",
        "user.tactu.owner",
    ] {
        assert!(
            copy["config"][format!("{}{key}", arca_incus::snapshot::MARK_PREFIX)].is_string(),
            "{key} is kept as a mark"
        );
    }
    assert_eq!(
        copy["config"][arca_incus::VERIFIED_KEY],
        json!("true"),
        "the clean-up PUT marks the copy verified"
    );
    assert_eq!(
        copy["config"]["user.arcasnapshot.dropped-devices"],
        json!("http,photos,share")
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
            (
                "PUT".to_string(),
                format!("/1.0/instances/{name}?project={SNAPSHOT_PROJECT}")
            ),
        ]
    );
    assert!(
        writes[1].body.get("start").is_none(),
        "the create never asks to start"
    );
    assert_eq!(
        writes[1].body["devices"]["photos"],
        json!({ "type": "none" }),
        "the create masks the source's volume, which the snapshot project does not have"
    );
    assert_eq!(writes[1].body["config"]["user.arca.app"], json!(""));
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
    let name = snapshot_name("shop", "web", NOW).expect("a name");
    let copying = rig
        .incus
        .instance(SNAPSHOT_PROJECT, &name)
        .expect("the copy being made");
    for key in copying["config"].as_object().expect("config").keys() {
        assert!(
            !arca_incus::snapshot::ownership_key(key),
            "even while copying, no live mark: {key}"
        );
    }
    assert_eq!(copying["devices"]["http"], json!({ "type": "none" }));
    // Once Incus finishes, the copy is rewritten clean in the background.
    rig.incus.release_operations();
    let clean = (0..200).find_map(|_| {
        std::thread::sleep(Duration::from_millis(25));
        rig.incus
            .instance(SNAPSHOT_PROJECT, &name)
            .filter(|copy| copy["devices"].get("http").is_none())
    });
    let clean = clean.expect("the copy is cleaned once it is made");
    assert_eq!(
        clean["devices"].as_object().map(|devices| devices.len()),
        Some(2)
    );
    assert_write_family(&rig.incus);
}

#[test]
fn a_copy_that_stays_tied_to_its_source_is_deleted_and_refused() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("snapshot-not-clean", &key);
    rig.add_snapshot_pool("usb1");
    rig.incus.ignore_puts();
    let proposed = rig
        .propose(
            "snapshot_create",
            json!({ "project": "shop", "instance": "web", "pool": "usb1" }),
        )
        .expect("a proposal");
    match rig.approve_and_run(&key, &proposed) {
        Err(CallError::Refused { code, message }) => {
            assert_eq!(code, "StorageWriteFailed");
            assert!(message.contains("device http"), "{message}");
            assert!(message.contains("the copy was deleted"), "{message}");
        }
        other => panic!("expected a refused copy, got {other:?}"),
    }
    let name = snapshot_name("shop", "web", NOW).expect("a name");
    assert!(
        rig.incus.instance(SNAPSHOT_PROJECT, &name).is_none(),
        "the half-made copy is gone"
    );
    let last = rig.incus.writes().pop().expect("a write");
    assert_eq!(
        (last.method.as_str(), last.target),
        (
            "DELETE",
            format!("/1.0/instances/{name}?project={SNAPSHOT_PROJECT}")
        )
    );
    assert!(
        rig.incus.instance("shop", "web").is_some(),
        "the source stays"
    );
    assert_write_family(&rig.incus);
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
    assert_eq!(app["droppedDevices"], json!(["http", "photos", "share"]));
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

// ---- Round 2: nothing that holds data is ever offered for erase, whatever `lsblk` shows. ----

/// A host with only the system NVMe and the given extra disks.
fn host_with(extra: Vec<Value>) -> Value {
    let mut disks = vec![host_disks()["blockdevices"][0].clone()];
    disks.extend(extra);
    json!({ "blockdevices": disks })
}

fn role_and_reason(rig: &Rig, name: &str) -> (String, String) {
    let list = rig.storage_list();
    let view = disk_view(&list, name);
    (
        view["role"].as_str().unwrap_or_default().to_string(),
        view["reason"].as_str().unwrap_or_default().to_string(),
    )
}

#[test]
fn every_member_of_a_mounted_multi_device_file_system_is_taken() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("storage-multi-device", &key);
    // The root file system is a btrfs over the NVMe partition and a USB disk; lsblk lists / on the NVMe partition only.
    let mut system = host_disks()["blockdevices"][0].clone();
    system["children"][1]["fstype"] = json!("btrfs");
    system["children"][1]["uuid"] = json!("uuid-rootfs");
    rig.devices.set(Ok(json!({ "blockdevices": [
        system,
        disk("sdb", "Acme Portable 1T", "usb", true, 1000 * GB, json!({ "fstype": "btrfs", "uuid": "uuid-rootfs" })),
        // A bcachefs over two disks, mounted at /srv/bulk; lsblk lists the mount on sdc only.
        disk("sdc", "Fakedisk SATA 2T", "sata", false, 2000 * GB, json!({
            "fstype": "bcachefs", "uuid": "uuid-bulk", "mountpoints": ["/srv/bulk"],
        })),
        disk("sdd", "Fakedisk SATA 2T", "sata", false, 2000 * GB, json!({ "fstype": "bcachefs", "uuid": "uuid-bulk" })),
        // A btrfs member lsblk knows nothing about; the kernel lists it under a mounted file system.
        disk("sde", "Acme Portable 500G", "usb", true, 500 * GB, json!({})),
    ]})));
    rig.devices.set_links(DeviceLinks {
        members: vec![("btrfs".into(), "uuid-elsewhere".into(), vec!["sde".into()])],
        ..DeviceLinks::default()
    });
    let (role, reason) = role_and_reason(&rig, "sdb");
    assert_eq!(role, "system");
    assert!(
        reason.contains("part of the btrfs file system that holds /"),
        "{reason}"
    );
    assert!(reason.contains("nvme0n1p2"), "{reason}");
    assert_eq!(
        role_and_reason(&rig, "sdd").0,
        "in use",
        "a bcachefs member is mounted with its set"
    );
    let (role, reason) = role_and_reason(&rig, "sde");
    assert_eq!(role, "system");
    assert!(
        reason.contains("part of a mounted btrfs file system"),
        "{reason}"
    );
    for (device, code) in [
        ("sdb", "SystemDisk"),
        ("sdd", "DiskMounted"),
        ("sde", "SystemDisk"),
    ] {
        assert_eq!(
            refused_code(rig.propose("storage_add", json!({ "name": "x", "device": device }))),
            code,
            "{device}"
        );
    }
    assert!(rig.incus.writes().is_empty());
}

#[test]
fn raid_lvm_zfs_luks_swap_and_held_disks_are_never_free_even_unmounted() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("storage-members", &key);
    let part = |disk_name: &str, fstype: Option<&str>| {
        node(
            &format!("{disk_name}1"),
            "part",
            fstype,
            None,
            &[],
            500 * GB,
        )
    };
    rig.devices.set(Ok(host_with(vec![
        // One half of a failing RAID1: the array is not assembled, so lsblk shows the member alone.
        disk(
            "sdb",
            "Fakedisk SATA 500G",
            "sata",
            false,
            500 * GB,
            json!({ "children": [part("sdb", Some("linux_raid_member"))] }),
        ),
        disk(
            "sdc",
            "Fakedisk SATA 500G",
            "sata",
            false,
            500 * GB,
            json!({ "children": [part("sdc", Some("LVM2_member"))] }),
        ),
        disk(
            "sdd",
            "Fakedisk SATA 500G",
            "sata",
            false,
            500 * GB,
            json!({ "fstype": "zfs_member" }),
        ),
        disk(
            "sde",
            "Acme Portable 500G",
            "usb",
            true,
            500 * GB,
            json!({ "children": [part("sde", Some("crypto_LUKS"))] }),
        ),
        disk(
            "sdf",
            "Acme Portable 500G",
            "usb",
            true,
            500 * GB,
            json!({ "children": [part("sdf", Some("swap"))] }),
        ),
        // Nothing on it says so, but the kernel lists holders: a partition held by an inactive array, a disk held by a
        // device-mapper target.
        disk(
            "sdg",
            "Fakedisk SATA 500G",
            "sata",
            false,
            500 * GB,
            json!({ "children": [part("sdg", None)] }),
        ),
        disk(
            "sdh",
            "Fakedisk SATA 500G",
            "sata",
            false,
            500 * GB,
            json!({}),
        ),
    ])));
    rig.devices.set_links(DeviceLinks {
        holders: [
            ("sdg1".to_string(), vec!["md127".to_string()]),
            ("sdh".to_string(), vec!["dm-3".to_string()]),
        ]
        .into_iter()
        .collect(),
        ..DeviceLinks::default()
    });
    for device in ["sdb", "sdc", "sdd", "sde", "sdf", "sdg", "sdh"] {
        let (role, reason) = role_and_reason(&rig, device);
        assert_eq!(role, "system", "{device}: {reason}");
        assert_eq!(
            refused_code(rig.propose("storage_add", json!({ "name": "x", "device": device }))),
            "SystemDisk",
            "{device}"
        );
    }
    assert!(role_and_reason(&rig, "sdg").1.contains("held by md127"));
    assert!(role_and_reason(&rig, "sdh").1.contains("held by dm-3"));
    assert!(rig.incus.writes().is_empty());
}

#[test]
fn the_kernel_must_let_arca_hold_the_disk_alone_before_it_is_erased() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("storage-exclusive", &key);
    // lsblk says sdb is free, but the kernel holds its partition (a mount in another namespace, say).
    rig.devices.set_open("/dev/sdb1", ExclusiveOpen::Busy);
    match rig.propose("storage_add", json!({ "name": "usb1", "device": "sdb" })) {
        Err(CallError::Refused { code, message }) => {
            assert_eq!(code, "DiskBusy");
            assert!(message.contains("sdb1 is in use right now"), "{message}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert_eq!(
        rig.devices.opened(),
        vec!["/dev/sdb", "/dev/sdb1"],
        "the disk and every partition"
    );
    // A device that fails to open for any reason but permission cannot be checked, and is not erased.
    rig.devices
        .set_open("/dev/sdb1", ExclusiveOpen::Failed("No such device".into()));
    assert_eq!(
        refused_code(rig.propose("storage_add", json!({ "name": "usb1", "device": "sdb" }))),
        "DiskUncheckable"
    );
    // Checked again when the approved add runs: something took the disk after the proposal.
    rig.devices.set_open("/dev/sdb1", ExclusiveOpen::Free);
    let proposed = rig
        .propose("storage_add", json!({ "name": "usb1", "device": "sdb" }))
        .expect("free when proposed");
    assert_eq!(proposed["args"]["inUseCheck"], json!("exclusive open"));
    assert!(
        !proposed["details"].to_string().contains("In-use check"),
        "the kernel's own answer needs no note"
    );
    rig.devices.set_open("/dev/sdb", ExclusiveOpen::Busy);
    assert_eq!(
        refused_code(rig.approve_and_run(&key, &proposed)),
        "DiskBusy"
    );
    assert!(rig.incus.writes().is_empty(), "nothing was wiped");
    // A folder target is mounted by nature and is never opened.
    let before = rig.devices.opened().len();
    let mount = stick_mount(&rig);
    rig.propose("storage_add", json!({ "name": "stick", "path": mount }))
        .expect("a folder");
    assert_eq!(rig.devices.opened().len(), before);
}

#[test]
fn a_disk_with_this_machines_folder_snapshots_is_known_while_unplugged() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("storage-folder-uuid", &key);
    let mount = stick_mount(&rig);
    let proposed = rig
        .propose("storage_add", json!({ "name": "stick", "path": mount }))
        .expect("a folder on the stick");
    rig.approve_and_run(&key, &proposed).expect("the add runs");
    assert_eq!(role_and_reason(&rig, "sdc").0, "snapshots");
    // The stick is unmounted: lsblk no longer shows where the folder is, and the pool's folder is gone with it.
    rig.devices.set(Ok(disks_with_stick_at(None)));
    std::fs::remove_dir(format!("{mount}/arca-pool")).expect("the folder goes with the disk");
    let (role, reason) = role_and_reason(&rig, "sdc");
    assert_eq!(role, "snapshots", "{reason}");
    assert!(reason.contains("snapshot storage stick"), "{reason}");
    assert_eq!(
        refused_code(rig.propose("storage_add", json!({ "name": "x", "device": "sdc" }))),
        "SnapshotDisk"
    );
    // And while that folder can't be reached, no disk is erased at all: it might be the one about to be.
    match rig.propose("storage_add", json!({ "name": "usb1", "device": "sdb" })) {
        Err(CallError::Refused { code, message }) => {
            assert_eq!(code, "SnapshotStorageUnreachable");
            assert!(
                message.contains("plug it in or remove it first"),
                "{message}"
            );
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    // A folder pool made before the file system was recorded, whose folder is gone, blocks erases the same way.
    let rig = Rig::new("storage-folder-old", &key);
    rig.incus.add_pool(
        "old",
        "dir",
        SNAPSHOT_POOL_DESCRIPTION,
        json!({ "source": "/media/owner/GONE/arca-pool", SNAPSHOT_STORAGE_KEY: "old" }),
        0,
        GB,
    );
    assert_eq!(
        refused_code(rig.propose("storage_add", json!({ "name": "usb1", "device": "sdb" }))),
        "SnapshotStorageUnreachable"
    );
    assert!(rig.incus.writes().is_empty());
}

#[test]
fn a_disk_given_to_an_app_is_never_offered() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("storage-passthrough", &key);
    rig.incus.add_project("lab", json!({}));
    rig.incus.add_instance(json!({
        "name": "vm1", "project": "lab", "type": "virtual-machine", "status": "Stopped", "profiles": [],
        "config": {}, "devices": {},
        "expanded_devices": { "usb": { "type": "unix-block", "source": "/dev/sdb" } },
    }));
    rig.incus.add_instance(json!({
        "name": "nas", "project": "default", "type": "container", "status": "Running", "profiles": [],
        "config": {}, "expanded_devices": {},
        "devices": { "raw": { "type": "disk", "source": "/dev/sdc1", "path": "/mnt/raw" } },
    }));
    let (role, reason) = role_and_reason(&rig, "sdb");
    assert_eq!(role, "system");
    assert!(reason.contains("given to the app lab/vm1"), "{reason}");
    let (role, reason) = role_and_reason(&rig, "sdc");
    assert_eq!(role, "system");
    assert!(reason.contains("given to the app default/nas"), "{reason}");
    assert_eq!(
        refused_code(rig.propose("storage_add", json!({ "name": "usb1", "device": "sdb" }))),
        "SystemDisk"
    );
}

#[test]
fn a_pool_with_a_name_arca_never_writes_still_takes_its_disk() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("storage-odd-pool", &key);
    rig.incus.add_pool(
        "Old Pool",
        "btrfs",
        "",
        json!({ "source": "/dev/sdb" }),
        0,
        GB,
    );
    let (role, reason) = role_and_reason(&rig, "sdb");
    assert_eq!(role, "system");
    assert!(reason.contains("Old Pool"), "{reason}");
}

#[test]
fn an_app_copy_whose_clean_up_never_finished_is_incomplete_and_journalled() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("snapshot-incomplete", &key);
    rig.add_snapshot_pool("usb1");
    rig.incus.hold_operations();
    rig.incus.ignore_puts();
    let proposed = rig
        .propose(
            "snapshot_create",
            json!({ "project": "shop", "instance": "web", "pool": "usb1" }),
        )
        .expect("a proposal");
    let id = proposed["id"].as_str().expect("an id").to_string();
    let result = rig
        .approve_and_run(&key, &proposed)
        .expect("a long copy keeps going");
    assert_eq!(result["state"], json!("copying"));
    let name = snapshot_name("shop", "web", NOW).expect("a name");
    // Incus finishes; the clean-up is lost, so the read-back refuses and deletes the copy, in the background.
    rig.incus.release_operations();
    let journal = (0..200).find_map(|_| {
        std::thread::sleep(Duration::from_millis(25));
        let text = find_file(rig._scratch.dir.as_path(), "action-proposals.jsonl")
            .and_then(|path| std::fs::read_to_string(path).ok())?;
        text.contains("SnapshotCleanupFailed").then_some(text)
    });
    let journal = journal.expect("the background failure is journalled");
    let line = journal
        .lines()
        .find(|line| line.contains("SnapshotCleanupFailed"))
        .expect("a line");
    let event: Value = serde_json::from_str(line).expect("json");
    assert_eq!(event["id"], json!(id));
    assert_eq!(event["ok"], json!(false));
    assert!(
        rig.incus.instance(SNAPSHOT_PROJECT, &name).is_none(),
        "the unclean copy was deleted"
    );

    // A copy left without its clean-up (the process stopped before it) is listed as incomplete.
    rig.incus.add_project(SNAPSHOT_PROJECT, json!({}));
    rig.incus.add_instance(json!({
        "name": "shop--web--20261108t0900", "project": SNAPSHOT_PROJECT, "type": "container", "status": "Stopped",
        "profiles": [], "config": { "user.arcasnapshot.source-project": "shop", "user.arcasnapshot.source-name": "web" },
        "devices": {},
    }));
    let listed = rig.snapshot_list();
    let row = listed["snapshots"]
        .as_array()
        .expect("a list")
        .iter()
        .find(|row| row["name"] == json!("shop--web--20261108t0900"))
        .expect("listed");
    assert_eq!(row["state"], json!("incomplete"));
}

fn find_file(dir: &std::path::Path, name: &str) -> Option<std::path::PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.file_name().and_then(|n| n.to_str()) == Some(name) {
            return Some(path);
        }
        if path.is_dir()
            && let Some(found) = find_file(&path, name)
        {
            return Some(found);
        }
    }
    None
}

fn denied_disk(rig: &Rig) {
    for path in ["/dev/sdb", "/dev/sdb1"] {
        rig.devices
            .set_open(path, ExclusiveOpen::Denied("Permission denied".into()));
    }
}

fn lists(mountinfo: &str, swaps: &str) -> SystemLists {
    SystemLists {
        mountinfo: mountinfo.to_string(),
        swaps: swaps.to_string(),
        numbers: [
            ("sdb".to_string(), "8:16".to_string()),
            ("sdb1".to_string(), "8:17".to_string()),
        ]
        .into_iter()
        .collect(),
    }
}

const ROOT_MOUNT: &str = "22 1 259:2 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p2 rw\n";
const NO_SWAP: &str = "Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n";

#[test]
fn a_disk_arca_may_not_open_is_checked_through_the_system_lists() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("storage-lists", &key);
    // The service user is not in the disk group: the exclusive open answers EACCES, so the system's lists decide.
    denied_disk(&rig);
    rig.devices.set_lists(Ok(lists(ROOT_MOUNT, NO_SWAP)));
    let proposed = rig
        .propose("storage_add", json!({ "name": "usb1", "device": "sdb" }))
        .expect("nothing in the lists uses sdb");
    assert_eq!(proposed["args"]["inUseCheck"], json!("system lists"));
    assert_eq!(
        row(&proposed["details"], "In-use check"),
        SYSTEM_LISTS_SENTENCE
    );
    assert_eq!(
        SYSTEM_LISTS_SENTENCE,
        "In-use check through system lists (Arca can't open the disk directly)."
    );
    rig.approve_and_run(&key, &proposed)
        .expect("the approved add runs");
    assert!(rig.incus.pool("arca-snapshots-usb1").is_some());

    // Each list that shows the disk in use refuses it.
    let cases = [
        (
            lists(
                &format!("{ROOT_MOUNT}40 22 8:17 / /mnt/old\\040photos rw - ext4 /dev/sdb1 rw\n"),
                NO_SWAP,
            ),
            "sdb1 is in use right now (it is mounted at /mnt/old photos)",
        ),
        (
            // A btrfs mount names another device; the device number still matches.
            lists(
                &format!("{ROOT_MOUNT}41 22 8:17 / /data rw - btrfs /dev/mapper/other rw\n"),
                NO_SWAP,
            ),
            "it is mounted at /data",
        ),
        (
            lists(
                ROOT_MOUNT,
                &format!(
                    "{NO_SWAP}/dev/sdb1                               partition\t8388604\t\t0\t\t-2\n"
                ),
            ),
            "it is used as swap",
        ),
    ];
    for (found, why) in cases {
        let rig = Rig::new("storage-lists-busy", &key);
        denied_disk(&rig);
        rig.devices.set_lists(Ok(found));
        match rig.propose("storage_add", json!({ "name": "usb1", "device": "sdb" })) {
            Err(CallError::Refused { code, message }) => {
                assert_eq!(code, "DiskBusy");
                assert!(message.contains(why), "{message}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(rig.incus.writes().is_empty());
    }

    // Holders and multi-device membership are in the lists too; the classifier already takes such a disk.
    let rig = Rig::new("storage-lists-held", &key);
    denied_disk(&rig);
    rig.devices.set_lists(Ok(lists(ROOT_MOUNT, NO_SWAP)));
    rig.devices.set_links(DeviceLinks {
        members: vec![("btrfs".into(), "uuid-x".into(), vec!["sdb1".into()])],
        ..DeviceLinks::default()
    });
    assert_eq!(
        refused_code(rig.propose("storage_add", json!({ "name": "usb1", "device": "sdb" }))),
        "SystemDisk"
    );

    // Lists that cannot be read are no check.
    let rig = Rig::new("storage-lists-unreadable", &key);
    denied_disk(&rig);
    rig.devices
        .set_lists(Err("the mount list could not be read: denied".into()));
    assert_eq!(
        refused_code(rig.propose("storage_add", json!({ "name": "usb1", "device": "sdb" }))),
        "DiskUncheckable"
    );

    // Checked again through the lists when the approved add runs.
    let rig = Rig::new("storage-lists-run", &key);
    denied_disk(&rig);
    rig.devices.set_lists(Ok(lists(ROOT_MOUNT, NO_SWAP)));
    let proposed = rig
        .propose("storage_add", json!({ "name": "usb1", "device": "sdb" }))
        .expect("free when proposed");
    rig.devices.set_lists(Ok(lists(
        &format!("{ROOT_MOUNT}42 22 8:17 / /media/owner/PHOTOS rw - ext4 /dev/sdb1 rw\n"),
        NO_SWAP,
    )));
    assert_eq!(
        refused_code(rig.approve_and_run(&key, &proposed)),
        "DiskBusy"
    );
    assert!(rig.incus.writes().is_empty(), "nothing was wiped");
}

#[test]
fn firmware_raid_and_other_member_disks_are_never_free() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("storage-firmware-raid", &key);
    let whole = |name: &str, fstype: &str| {
        disk(
            name,
            "Fakedisk SATA 1T",
            "sata",
            false,
            1000 * GB,
            json!({ "fstype": fstype }),
        )
    };
    // Members of an Intel firmware RAID that is not assembled: lsblk shows the signature and nothing on it.
    rig.devices.set(Ok(host_with(vec![
        whole("sdb", "isw_raid_member"),
        whole("sdc", "ddf_raid_member"),
        whole("sdd", "drbd"),
        whole("sde", "swsuspend"),
        whole("sdf", "ceph_bluestore"),
    ])));
    for device in ["sdb", "sdc", "sdd", "sde", "sdf"] {
        let (role, reason) = role_and_reason(&rig, device);
        assert_eq!(role, "system", "{device}: {reason}");
        assert_eq!(
            refused_code(rig.propose("storage_add", json!({ "name": "x", "device": device }))),
            "SystemDisk",
            "{device}"
        );
    }
    // Through the system-lists path as well: the disk is refused before anything is written.
    rig.devices.set_open(
        "/dev/sdb",
        ExclusiveOpen::Denied("Permission denied".into()),
    );
    assert_eq!(
        refused_code(rig.propose("storage_add", json!({ "name": "x", "device": "sdb" }))),
        "SystemDisk"
    );
    assert!(rig.incus.writes().is_empty());
}

#[test]
fn a_disk_whose_in_use_check_changes_after_the_proposal_is_refused() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("storage-check-changed", &key);
    rig.devices.set_lists(Ok(lists(ROOT_MOUNT, NO_SWAP)));
    let proposed = rig
        .propose("storage_add", json!({ "name": "usb1", "device": "sdb" }))
        .expect("checked by the exclusive open");
    assert_eq!(proposed["args"]["inUseCheck"], json!("exclusive open"));
    // By the time it runs, the open is refused: the lists would pass, but the owner approved another check.
    denied_disk(&rig);
    assert_eq!(
        refused_code(rig.approve_and_run(&key, &proposed)),
        "DiskChanged"
    );
    assert!(rig.incus.writes().is_empty());
}

#[test]
fn a_list_check_that_cannot_read_the_kernels_links_is_refused() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("storage-links-unreadable", &key);
    denied_disk(&rig);
    rig.devices.set_lists(Ok(lists(ROOT_MOUNT, NO_SWAP)));
    rig.devices
        .set_links_error("/sys/class/block could not be read");
    assert_eq!(
        refused_code(rig.propose("storage_add", json!({ "name": "usb1", "device": "sdb" }))),
        "DiskUncheckable"
    );
}

// ---- Folder snapshots ----

/// A scratch folder to snapshot, removed when dropped. It lives under the build's own scratch folder, since `/tmp` is a
/// system folder a folder snapshot never copies: `a.txt` (5 bytes), `docs/`, `docs/deep/`, `docs/b.txt` (1000 bytes)
/// and the link `link`, so 5 paths and 1005 bytes.
struct SourceFolder {
    path: std::path::PathBuf,
}

impl SourceFolder {
    fn new(tag: &str) -> SourceFolder {
        let base =
            std::fs::canonicalize(env!("CARGO_TARGET_TMPDIR")).expect("the build's scratch folder");
        let path = base.join(format!(
            "folder-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(path.join("docs/deep")).expect("the source folder");
        std::fs::write(path.join("a.txt"), b"hello").expect("a file");
        std::fs::write(path.join("docs/b.txt"), vec![1u8; 1000]).expect("a file");
        std::os::unix::fs::symlink("a.txt", path.join("link")).expect("a link");
        SourceFolder { path }
    }

    fn text(&self) -> String {
        self.path.to_str().expect("a UTF-8 path").to_string()
    }
}

impl Drop for SourceFolder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// The copy command's manifest line, as the helper prints it.
fn manifest(source: (u64, u64), copy: (u64, u64)) -> String {
    format!(
        "{{\"version\":1,\"source\":{{\"paths\":{},\"bytes\":{}}},\"copy\":{{\"paths\":{},\"bytes\":{}}},\"tar\":{{\"read\":0,\"write\":0}}}}\n",
        source.0, source.1, copy.0, copy.1
    )
}

fn folder_name(folder: &SourceFolder) -> String {
    arca::storage::folder_snapshot_name(&folder.text(), NOW).expect("a name")
}

fn helper_create(rig: &Rig) -> Vec<arca_incus::fake_storage::StorageRequest> {
    rig.incus
        .writes()
        .into_iter()
        .filter(|write| {
            write.method == "POST"
                && write.target == format!("/1.0/instances?project={SNAPSHOT_PROJECT}")
        })
        .collect()
}

fn listed<'a>(list: &'a Value, name: &str) -> &'a Value {
    list["snapshots"]
        .as_array()
        .expect("a list")
        .iter()
        .find(|row| row["name"] == json!(name))
        .unwrap_or_else(|| panic!("{name} is not listed in {list}"))
}

#[test]
fn a_folder_snapshot_copies_through_a_read_only_offline_helper_that_is_then_deleted() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("folder-snapshot", &key);
    rig.add_snapshot_pool("usb1");
    let folder = SourceFolder::new("copy");
    rig.incus.set_exec(0, &manifest((5, 1005), (5, 1005)));
    let proposed = rig
        .propose(
            "snapshot_create",
            json!({ "folder": format!("{}/", folder.text()), "pool": "usb1" }),
        )
        .expect("a folder snapshot may be proposed");
    let args = &proposed["args"];
    assert_eq!(
        args["folder"],
        json!(folder.text()),
        "the trailing slash is dropped"
    );
    assert_eq!(args["kind"], json!("folder"));
    assert_eq!(
        args["fsUuid"],
        json!("uuid-nvme0n1p2"),
        "the file system it is on"
    );
    assert_eq!(
        (args["size"].clone(), args["files"].clone()),
        (json!(1005), json!(5))
    );
    assert_eq!(args["unreadable"], json!(0));
    assert!(
        args["allocated"].as_u64().is_some(),
        "the blocks it takes are bound"
    );
    assert_eq!(
        (args["separate"].clone(), args["separateCount"].clone()),
        (json!([]), json!(0))
    );
    assert_eq!(args["poolFree"], json!(1000 * GB));
    let name = folder_name(&folder);
    assert_eq!(args["name"], json!(name));
    assert_eq!(
        proposed["summary"],
        json!(format!(
            "Copy the folder {} (1.0 kB, 5 files) into snapshot storage usb1",
            folder.text()
        ))
    );
    let details = &proposed["details"];
    assert_eq!(row(details, "Folder"), folder.text());
    assert_eq!(row(details, "Size"), "1.0 kB");
    assert_eq!(row(details, "Files"), "5 files and folders");
    assert_eq!(row(details, "Target"), "Snapshot storage usb1");
    assert!(row(details, "Space it takes").starts_with("about "));
    assert_eq!(row(details, "Free space after"), "about 1000 GB");
    assert_eq!(row(details, "Read-only"), "The folder is not changed");
    assert!(rig.incus.writes().is_empty(), "a proposal writes nothing");

    let result = rig
        .approve_and_run(&key, &proposed)
        .expect("the approved copy runs");
    assert_eq!(result["state"], json!("copied"));
    assert_eq!(result["kind"], json!("folder"));
    assert_eq!(result["files"], json!(5));

    // The volume: sized with room, shifted, with the marks a restore reads, and verified.
    let volume = rig
        .incus
        .volume("usb1", SNAPSHOT_PROJECT, &name)
        .expect("the folder's volume");
    let config = &volume["config"];
    assert_eq!(
        config["size"],
        json!(format!("{}B", 1u64 << 30)),
        "at least 1 GiB"
    );
    assert_eq!(config["security.shifted"], json!("true"));
    assert_eq!(config["user.arcasnapshot.source-kind"], json!("folder"));
    assert_eq!(
        config["user.arcasnapshot.source-path"],
        json!(folder.text())
    );
    assert_eq!(config["user.arcasnapshot.fs-uuid"], json!("uuid-nvme0n1p2"));
    assert_eq!(config["user.arcasnapshot.verified"], json!("true"));
    assert_eq!(config["user.arcasnapshot.paths"], json!("5"));
    assert_eq!(config["user.arcasnapshot.bytes"], json!("1005"));

    // The helper: from the cached image, the folder read-only and shifted, no NIC, no profile, unprivileged, capped.
    let creates = helper_create(&rig);
    assert_eq!(creates.len(), 1);
    assert_eq!(result["ioCaps"], json!(true));
    assert_eq!(result["io"], json!("incus-caps"));
    assert_eq!(result["skipped"], json!({ "sockets": 0, "devices": 0 }));
    assert!(result.get("notes").is_none(), "nothing to tell: {result}");
    let helper = &creates[0].body;
    let helper_name = arca::storage::helper_name(&name);
    assert_eq!(helper["name"], json!(helper_name));
    assert_eq!(helper["source"]["alias"], json!("arca-folder-copy"));
    assert_eq!(helper["profiles"], json!([]));
    assert!(helper.get("start").is_none());
    assert_eq!(helper["config"]["boot.autostart"], json!("false"));
    assert_eq!(helper["config"]["security.privileged"], json!("false"));
    assert_eq!(helper["config"]["limits.cpu"], json!("2"));
    assert_eq!(helper["config"]["limits.memory"], json!("1GiB"));
    let devices = helper["devices"].as_object().expect("devices");
    assert_eq!(
        devices["root"]["pool"],
        json!("default"),
        "the helper's root is on the machine's first existing regular pool, never on snapshot storage"
    );
    assert_eq!(
        devices.keys().map(String::as_str).collect::<Vec<_>>(),
        ["dst", "root", "src"]
    );
    assert!(
        devices
            .values()
            .all(|device| device["type"] != json!("nic"))
    );
    assert_eq!(
        devices["src"],
        json!({ "type": "disk", "source": folder.text(), "path": "/src", "readonly": "true", "shift": "true" })
    );
    assert_eq!(devices["dst"]["source"], json!(name));
    assert_eq!(devices["dst"]["limits.read"], json!("150MB"));
    assert_eq!(devices["dst"]["limits.write"], json!("80MB"));
    // Started, run once, stopped and deleted.
    assert!(
        rig.incus.instance(SNAPSHOT_PROJECT, &helper_name).is_none(),
        "the helper is gone"
    );
    let steps: Vec<String> = rig
        .incus
        .writes()
        .iter()
        .filter(|write| write.target.contains(&helper_name))
        .map(|write| {
            let what = write.body["action"].as_str().unwrap_or_default();
            format!(
                "{} {}",
                write.method,
                if what.is_empty() {
                    write
                        .target
                        .split('?')
                        .next()
                        .unwrap_or_default()
                        .rsplit('/')
                        .next()
                        .unwrap_or_default()
                } else {
                    what
                }
            )
        })
        .collect();
    let delete = format!("DELETE {helper_name}");
    assert_eq!(
        steps,
        ["PUT start", "POST exec", "PUT stop", delete.as_str()]
    );
    let exec = rig
        .incus
        .writes()
        .into_iter()
        .find(|write| write.target.contains("/exec"))
        .expect("the copy command");
    assert_eq!(
        exec.body["environment"]["ARCA_SOURCE"],
        json!(format!("\"{}\"", folder.text()))
    );
    assert_eq!(exec.body["environment"]["ARCA_THROTTLE"], json!("0"));
    assert_eq!(
        exec.body["environment"]["ARCA_SEPARATE"],
        json!(r#"{"count":0,"paths":[]}"#)
    );
    assert!(
        exec.body["command"][2]
            .as_str()
            .is_some_and(|script| script.contains("--xattrs") && script.contains("--numeric-owner"))
    );
    assert_write_family(&rig.incus);

    let list = rig.snapshot_list();
    let entry = listed(&list, &name);
    assert_eq!(entry["kind"], json!("folder"));
    assert_eq!(entry["state"], json!("ready"));
    assert_eq!(entry["sourcePath"], json!(folder.text()));
    assert!(
        list["snapshots"]
            .as_array()
            .expect("a list")
            .iter()
            .all(|row| !row["name"]
                .as_str()
                .unwrap_or_default()
                .starts_with("arca-copy-")),
        "a helper is never listed"
    );

    // The image is fetched once and kept.
    let other = SourceFolder::new("again");
    let again = rig
        .propose(
            "snapshot_create",
            json!({ "folder": other.text(), "pool": "usb1" }),
        )
        .expect("a second folder snapshot");
    rig.approve_and_run(&key, &again).expect("it runs");
    let pulls = rig
        .incus
        .writes()
        .into_iter()
        .filter(|write| write.target.starts_with("/1.0/images"))
        .count();
    assert_eq!(pulls, 1, "the helper image is cached");

    // Deleting it names the folder it came from.
    let delete = rig
        .propose("snapshot_delete", json!({ "name": name }))
        .expect("a delete may be proposed");
    assert_eq!(
        row(&delete["details"], "Copied from"),
        format!("folder {}", folder.text())
    );
    rig.approve_and_run(&key, &delete).expect("the delete runs");
    assert!(rig.incus.volume("usb1", SNAPSHOT_PROJECT, &name).is_none());
}

#[test]
fn a_folder_copy_that_fails_or_miscounts_is_kept_incomplete_and_the_helper_is_always_deleted() {
    let key = SkKey::from_seed(1);
    type Arrange = fn(&FakeStorageIncus);
    let cases: [(&str, Arrange, &str); 4] = [
        (
            "copy-fails",
            |incus| incus.set_exec(21, &manifest((5, 1005), (3, 600))),
            "writing the copy failed",
        ),
        (
            "miscount",
            |incus| incus.set_exec(0, &manifest((5, 1005), (4, 1005))),
            "the counts differ",
        ),
        (
            "wait-error",
            |incus| incus.fail_next_exec("websocket: close 1006 (abnormal closure)"),
            "abnormal closure",
        ),
        (
            "start-fails",
            |incus| {
                // Refused with the caps and again without them.
                for _ in 0..2 {
                    incus.fail_next_start(
                        "Failed to setup device mount \"src\": idmapping abilities are required",
                    );
                }
            },
            "idmapping",
        ),
    ];
    for (tag, arrange, words) in cases {
        let rig = Rig::new(&format!("folder-{tag}"), &key);
        rig.add_snapshot_pool("usb1");
        let folder = SourceFolder::new(tag);
        let proposed = rig
            .propose(
                "snapshot_create",
                json!({ "folder": folder.text(), "pool": "usb1" }),
            )
            .expect("a proposal");
        arrange(&rig.incus);
        let message = match rig.approve_and_run(&key, &proposed) {
            Err(CallError::Refused { code, message }) => {
                assert_eq!(code, "SnapshotIncomplete", "{tag}: {message}");
                message
            }
            other => panic!("{tag}: expected an incomplete copy, got {other:?}"),
        };
        assert!(message.contains(words), "{tag}: {message}");
        let name = folder_name(&folder);
        let helper = arca::storage::helper_name(&name);
        assert!(
            rig.incus.instance(SNAPSHOT_PROJECT, &helper).is_none(),
            "{tag}: the helper is deleted"
        );
        let volume = rig
            .incus
            .volume("usb1", SNAPSHOT_PROJECT, &name)
            .expect("the volume is kept");
        assert!(
            volume["config"].get("user.arcasnapshot.verified").is_none(),
            "{tag}"
        );
        assert!(
            volume["config"]["user.arcasnapshot.incomplete"]
                .as_str()
                .is_some_and(|reason| reason.contains(words)),
            "{tag}: {volume}"
        );
        let list = rig.snapshot_list();
        let entry = listed(&list, &name);
        assert_eq!(entry["state"], json!("incomplete"), "{tag}");
        assert!(
            entry["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains(words)),
            "{tag}"
        );
        assert_write_family(&rig.incus);
    }
}

#[test]
fn a_helper_incus_refuses_the_io_caps_for_is_made_again_without_them() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("folder-caps", &key);
    rig.add_snapshot_pool("usb1");
    let folder = SourceFolder::new("caps");
    let proposed = rig
        .propose(
            "snapshot_create",
            json!({ "folder": folder.text(), "pool": "usb1" }),
        )
        .expect("a proposal");
    rig.incus.set_exec(0, &manifest((5, 1005), (5, 1005)));
    // Incus's refusal does not name the caps (the exact answer a real host gave): any refusal with the caps on is
    // retried once without them.
    rig.incus.fail_next_start("Invalid block device \"tmpfs\"");
    let result = rig.approve_and_run(&key, &proposed).expect("the copy runs");
    assert_eq!(result["state"], json!("copied"));
    assert_eq!(result["ioCaps"], json!(false));
    // Without Incus's caps the copy is not left unthrottled: tar pauses itself.
    assert_eq!(result["io"], json!("throttled-by-tar"));
    let exec = rig
        .incus
        .writes()
        .into_iter()
        .find(|write| write.target.contains("/exec"))
        .expect("the copy command");
    assert_eq!(exec.body["environment"]["ARCA_THROTTLE"], json!("1"));
    let volume = rig
        .incus
        .volume("usb1", SNAPSHOT_PROJECT, &folder_name(&folder))
        .expect("the volume");
    assert_eq!(
        volume["config"]["user.arcasnapshot.io-caps"],
        json!("false")
    );
    let creates = helper_create(&rig);
    assert_eq!(creates.len(), 2, "made again once");
    assert!(
        creates[1].body["devices"]["dst"]
            .get("limits.write")
            .is_none()
    );
    assert_write_family(&rig.incus);
}

/// The copy command's manifest line with files that changed while they were read, and sockets and device nodes left
/// out.
fn manifest_with_changes(paths: u64, bytes: u64) -> String {
    format!(
        "{{\"version\":2,\"source\":{{\"paths\":{paths},\"bytes\":{bytes}}},\"copy\":{{\"paths\":{paths},\"bytes\":{bytes}}},\"skipped\":{{\"sockets\":2,\"devices\":1}},\"changed\":{{\"files\":3,\"head\":[\"./a.txt: file changed as we read it\"]}},\"io\":\"incus-caps\",\"sparse\":false,\"tar\":{{\"list\":0,\"read\":1,\"write\":0}}}}\n"
    )
}

#[test]
fn a_copy_whose_files_changed_while_read_is_ready_and_says_so() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("folder-changed", &key);
    rig.add_snapshot_pool("usb1");
    let folder = SourceFolder::new("changed");
    let proposed = rig
        .propose(
            "snapshot_create",
            json!({ "folder": folder.text(), "pool": "usb1" }),
        )
        .expect("a proposal");
    rig.incus.set_exec(0, &manifest_with_changes(5, 1005));
    let result = rig.approve_and_run(&key, &proposed).expect("the copy runs");
    assert_eq!(result["state"], json!("copied"));
    assert_eq!(result["changedDuringCopy"], json!(3));
    assert_eq!(
        result["changedFiles"],
        json!(["./a.txt: file changed as we read it"])
    );
    assert_eq!(result["skipped"], json!({ "sockets": 2, "devices": 1 }));
    let notes: Vec<&str> = result["notes"]
        .as_array()
        .expect("notes")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        notes.contains(
            &"3 files changed while being copied; take another snapshot with the apps stopped"
        ),
        "{notes:?}"
    );
    assert!(
        notes
            .iter()
            .any(|note| note.contains("2 socket(s) and 1 device node(s)"))
    );
    assert_eq!(result["sparse"], json!(false));
    assert!(
        notes
            .iter()
            .any(|note| note.contains("without sparse-file handling"))
    );

    let name = folder_name(&folder);
    let volume = rig
        .incus
        .volume("usb1", SNAPSHOT_PROJECT, &name)
        .expect("the volume");
    assert_eq!(
        volume["config"]["user.arcasnapshot.verified"],
        json!("true")
    );
    assert_eq!(
        volume["config"]["user.arcasnapshot.changed-during-copy"],
        json!("3")
    );
    assert_eq!(
        volume["config"]["user.arcasnapshot.skipped-sockets"],
        json!("2")
    );
    assert_eq!(
        volume["config"]["user.arcasnapshot.skipped-devices"],
        json!("1")
    );
    let list = rig.snapshot_list();
    let entry = listed(&list, &name);
    assert_eq!(entry["state"], json!("ready"));
    assert_eq!(entry["changedDuringCopy"], json!(3));
    assert_eq!(
        entry["note"],
        json!("3 files changed while being copied; take another snapshot with the apps stopped")
    );
    assert_write_family(&rig.incus);
}

#[test]
fn a_verified_copy_whose_helper_could_not_be_deleted_says_why() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("folder-helper-left", &key);
    rig.add_snapshot_pool("usb1");
    let folder = SourceFolder::new("helper-left");
    let proposed = rig
        .propose(
            "snapshot_create",
            json!({ "folder": folder.text(), "pool": "usb1" }),
        )
        .expect("a proposal");
    rig.incus.set_exec(0, &manifest((5, 1005), (5, 1005)));
    rig.incus
        .fail_next_delete("Failed deleting instance: device or resource busy");
    let result = rig.approve_and_run(&key, &proposed).expect("the copy runs");
    assert_eq!(result["state"], json!("copied"));
    let notes = result["notes"].to_string();
    assert!(
        notes.contains("could not be deleted") && notes.contains("resource busy"),
        "{result}"
    );
    let helper = arca::storage::helper_name(&folder_name(&folder));
    assert!(
        rig.incus.instance(SNAPSHOT_PROJECT, &helper).is_some(),
        "the helper is left for the startup sweep"
    );
}

#[test]
fn a_long_folder_copy_answers_copying_and_journals_how_it_ended() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("folder-long", &key);
    rig.add_snapshot_pool("usb1");
    let folder = SourceFolder::new("long");
    let proposed = rig
        .propose(
            "snapshot_create",
            json!({ "folder": folder.text(), "pool": "usb1" }),
        )
        .expect("a proposal");
    let id = proposed["id"].as_str().expect("an id").to_string();
    rig.incus.set_exec(0, &manifest((5, 1005), (5, 1005)));
    rig.incus.hold_operations();
    let result = rig
        .approve_and_run(&key, &proposed)
        .expect("a long copy keeps going");
    assert_eq!(result["state"], json!("copying"));
    let name = folder_name(&folder);
    assert_eq!(
        listed(&rig.snapshot_list(), &name)["state"],
        json!("copying")
    );
    // A copy still running is not deleted from under its helper.
    assert_eq!(
        refused_code(rig.propose("snapshot_delete", json!({ "name": name }))),
        "SnapshotBusy"
    );

    rig.incus.release_operations();
    let journal = (0..400).find_map(|_| {
        std::thread::sleep(Duration::from_millis(25));
        let text = find_file(rig._scratch.dir.as_path(), "action-proposals.jsonl")
            .and_then(|path| std::fs::read_to_string(path).ok())?;
        text.contains("SnapshotCopied").then_some(text)
    });
    let journal = journal.expect("the end of the copy is journalled");
    let event: Value = serde_json::from_str(
        journal
            .lines()
            .find(|line| line.contains("SnapshotCopied"))
            .expect("a line"),
    )
    .expect("json");
    assert_eq!(
        (event["id"].clone(), event["ok"].clone()),
        (json!(id), json!(true))
    );
    let row = listed(&rig.snapshot_list(), &name).clone();
    assert_eq!(row["state"], json!("ready"));
    assert!(
        rig.incus
            .instance(SNAPSHOT_PROJECT, &arca::storage::helper_name(&name))
            .is_none(),
        "the helper is gone"
    );
}

#[test]
fn a_restarted_kernel_deletes_leftover_helpers_and_marks_their_copies_incomplete() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("folder-sweep", &key);
    rig.add_snapshot_pool("usb1");
    rig.incus.add_project(SNAPSHOT_PROJECT, json!({}));
    let folder_volume = |name: &str, extra: Value| {
        let mut config = json!({
            "user.arcasnapshot.source-kind": "folder",
            "user.arcasnapshot.source-path": "/home",
            "user.arcasnapshot.source-name": "/home",
        });
        for (key, value) in extra.as_object().into_iter().flatten() {
            config[key] = value.clone();
        }
        json!({ "name": name, "project": SNAPSHOT_PROJECT, "type": "custom", "config": config })
    };
    rig.incus.add_volume(
        "usb1",
        folder_volume("folder--home--20261109T0900", json!({})),
    );
    rig.incus.add_volume(
        "usb1",
        folder_volume(
            "folder--home--20261108T0900",
            json!({ "user.arcasnapshot.verified": "true" }),
        ),
    );
    // A crash before the helper was made leaves a volume and no helper.
    rig.incus.add_volume(
        "usb1",
        folder_volume("folder--srv--20261109T0800", json!({})),
    );
    rig.incus.add_instance(json!({
        "name": "arca-copy-00112233aabbccdd", "project": SNAPSHOT_PROJECT, "type": "container",
        "status": "Running", "profiles": [],
        "config": {
            "user.arcasnapshot.copy-helper-for": "folder--home--20261109T0900",
            "user.arcasnapshot.copy-helper-pool": "usb1",
        },
        "devices": {},
    }));
    rig.incus.add_instance(json!({
        "name": "shop--web--20261108T0900", "project": SNAPSHOT_PROJECT, "type": "container",
        "status": "Stopped", "profiles": [], "config": {}, "devices": {},
    }));
    let removed = rig.storage.sweep_copy_helpers().expect("the sweep runs");
    assert_eq!(removed, ["arca-copy-00112233aabbccdd"]);
    assert!(
        rig.incus
            .instance(SNAPSHOT_PROJECT, "arca-copy-00112233aabbccdd")
            .is_none()
    );
    assert!(
        rig.incus
            .instance(SNAPSHOT_PROJECT, "shop--web--20261108T0900")
            .is_some(),
        "a snapshot copy is never touched"
    );
    let list = rig.snapshot_list();
    for name in ["folder--home--20261109T0900", "folder--srv--20261109T0800"] {
        let row = listed(&list, name);
        assert_eq!(row["state"], json!("incomplete"), "{name}");
        assert!(
            row["reason"]
                .as_str()
                .is_some_and(|r| r.contains("restarted")),
            "{name}"
        );
    }
    assert_eq!(
        listed(&list, "folder--home--20261108T0900")["state"],
        json!("ready")
    );
    assert_write_family(&rig.incus);
    // A second sweep finds nothing to do.
    let before = rig.incus.writes().len();
    assert!(rig.storage.sweep_copy_helpers().expect("again").is_empty());
    assert_eq!(rig.incus.writes().len(), before);
}

#[test]
fn folders_that_are_not_plain_existing_user_folders_are_refused_at_proposal() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("folder-refusals", &key);
    rig.add_snapshot_pool("usb1");
    let folder = SourceFolder::new("refusals");
    let base = folder.text();
    std::os::unix::fs::symlink(folder.path.join("docs"), folder.path.join("via")).expect("a link");
    // A folder snapshot pool inside the source folder.
    std::fs::create_dir_all(folder.path.join("store/arca-pool/inside")).expect("a pool folder");
    rig.incus.add_pool(
        "usbdir",
        "dir",
        SNAPSHOT_POOL_DESCRIPTION,
        json!({ "source": format!("{base}/store/arca-pool"), SNAPSHOT_STORAGE_KEY: "usbdir" }),
        0,
        1000 * GB,
    );
    rig.incus.add_pool(
        "tiny",
        "btrfs",
        SNAPSHOT_POOL_DESCRIPTION,
        json!({ "source": "uuid-tiny", SNAPSHOT_STORAGE_KEY: "tiny" }),
        0,
        1100,
    );
    let cases = [
        ("/".to_string(), "usb1", "FolderRefused"),
        ("/proc".to_string(), "usb1", "FolderRefused"),
        ("/sys/fs".to_string(), "usb1", "FolderRefused"),
        ("/dev".to_string(), "usb1", "FolderRefused"),
        ("/run/user".to_string(), "usb1", "FolderRefused"),
        ("/boot".to_string(), "usb1", "FolderRefused"),
        ("/tmp/work".to_string(), "usb1", "FolderRefused"),
        (
            "/var/lib/incus/storage-pools".to_string(),
            "usb1",
            "FolderRefused",
        ),
        (format!("{base}/missing"), "usb1", "FolderNotFound"),
        (format!("{base}/a.txt"), "usb1", "FolderNotFound"),
        (format!("{base}/via"), "usb1", "FolderLink"),
        (format!("{base}/via/deep"), "usb1", "FolderLink"),
        (
            format!("{base}/store/arca-pool/inside"),
            "usb1",
            "FolderInSnapshotStorage",
        ),
        (base.clone(), "usbdir", "FolderInSnapshotStorage"),
        // 1005 bytes and 10% more do not fit in 1100.
        (base.clone(), "tiny", "FolderTooBig"),
        (base.clone(), "fast", "NotSnapshotStorage"),
    ];
    for (path, pool, code) in cases {
        assert_eq!(
            refused_code(rig.propose("snapshot_create", json!({ "folder": path, "pool": pool }))),
            code,
            "{path} onto {pool}"
        );
    }
    // A folder on no listed disk.
    let mut unlisted = host_disks();
    unlisted["blockdevices"][0]["children"][1]["mountpoints"] = json!(["/srv-nothing"]);
    rig.devices.set(Ok(unlisted));
    assert_eq!(
        refused_code(rig.propose("snapshot_create", json!({ "folder": base, "pool": "usb1" }))),
        "FolderNotOnDisk"
    );
    rig.devices.set(Ok(host_disks()));
    for args in [
        json!({ "folder": "home", "pool": "usb1" }),
        json!({ "folder": format!("{base}/../x"), "pool": "usb1" }),
        json!({ "folder": base, "project": "shop", "instance": "web", "pool": "usb1" }),
        json!({ "folder": base, "project": "shop", "pool": "usb1" }),
    ] {
        assert!(
            matches!(
                rig.propose("snapshot_create", args.clone()),
                Err(CallError::Invalid(_))
            ),
            "{args}"
        );
    }
    assert!(rig.incus.writes().is_empty(), "no refusal wrote anything");
}

#[test]
fn the_owners_digest_binds_the_folder_and_a_changed_folder_is_refused_at_run() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("folder-binding", &key);
    rig.add_snapshot_pool("usb1");
    let folder = SourceFolder::new("binding");
    let proposed = rig
        .propose(
            "snapshot_create",
            json!({ "folder": folder.text(), "pool": "usb1" }),
        )
        .expect("a proposal");
    let args = proposed["args"].clone();
    let digest = |args: &Value| {
        arca::action_proposal::action_args_digest("snapshot_create", args).expect("a digest")
    };
    assert_eq!(
        digest(&args),
        proposed["argsDigest"].as_str().expect("a digest")
    );
    for (key, value) in [
        ("folder", json!("/srv")),
        ("fsUuid", json!("uuid-other")),
        ("size", json!(1)),
        ("allocated", json!(1)),
        ("files", json!(1)),
        ("unreadable", json!(1)),
        ("separate", json!([format!("{}/mnt", folder.text())])),
        ("separateCount", json!(3)),
        ("pool", json!("usb2")),
    ] {
        let mut changed = args.clone();
        changed[key] = value;
        assert_ne!(digest(&changed), digest(&args), "{key} is bound");
    }

    // The approved grant runs only the exact approved arguments.
    let id = proposed["id"].as_str().expect("an id");
    let statement = arca::handlers::action_statement(
        HOST,
        id,
        "snapshot_create",
        proposed["argsDigest"].as_str().expect("a digest"),
        NOW,
        &format!("{:032x}", 99),
    )
    .expect("a statement");
    let approved = {
        let mut kernel = rig.kernel.lock().expect("the kernel lock");
        arca::handlers::approve_action(
            &mut kernel,
            &json!({ "plan": id, "approval": approve_statement(&key, &statement) }),
            NOW,
        )
        .expect("the approval verifies")
    };
    let mut bigger = approved.args.clone();
    bigger["size"] = json!(10);
    assert_eq!(
        refused_code(
            rig.executor
                .run("snapshot_create", &bigger, &approved.grant)
        ),
        "NotApproved"
    );
    assert!(rig.incus.writes().is_empty());

    // The folder's file system changed between the proposal and the run.
    let mut moved = host_disks();
    moved["blockdevices"][0]["children"][1]["uuid"] = json!("uuid-new-system");
    rig.devices.set(Ok(moved));
    assert_eq!(
        refused_code(
            rig.executor
                .run("snapshot_create", &approved.args, &approved.grant)
        ),
        "FolderChanged"
    );
    rig.devices.set(Ok(host_disks()));

    // The pool filled up between the proposal and the run. 5000 bytes would hold the folder's 1005 bytes and 10% more,
    // but not the space its copy takes: its blocks and 4 KiB for each of its 5 paths.
    let again = rig
        .propose(
            "snapshot_create",
            json!({ "folder": folder.text(), "pool": "usb1" }),
        )
        .expect("a fresh proposal");
    rig.incus.add_pool(
        "usb1",
        "btrfs",
        SNAPSHOT_POOL_DESCRIPTION,
        json!({ "source": "uuid-usb1", SNAPSHOT_STORAGE_KEY: "usb1" }),
        1000 * GB - 5000,
        1000 * GB,
    );
    assert_eq!(
        refused_code(rig.approve_and_run(&key, &again)),
        "FolderTooBig"
    );
    assert!(rig.incus.writes().is_empty(), "nothing was written");
}

#[test]
fn a_helper_that_fails_to_start_is_still_stopped_and_deleted() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("folder-start-delete", &key);
    rig.add_snapshot_pool("usb1");
    let folder = SourceFolder::new("start-delete");
    let proposed = rig
        .propose(
            "snapshot_create",
            json!({ "folder": folder.text(), "pool": "usb1" }),
        )
        .expect("a proposal");
    // As on a real host: the failed start leaves the helper reading back as stopped while Incus still holds it, so a
    // delete without a stop first is refused ("Instance is running").
    for _ in 0..2 {
        rig.incus
            .fail_next_start("Failed to run: Permission denied - Failed to mount \"credentials\"");
    }
    let message = match rig.approve_and_run(&key, &proposed) {
        Err(CallError::Refused { code, message }) => {
            assert_eq!(code, "SnapshotIncomplete");
            message
        }
        other => panic!("expected an incomplete copy, got {other:?}"),
    };
    assert!(message.contains("did not start"), "{message}");
    assert!(!message.contains("could not be deleted"), "{message}");
    let helper = arca::storage::helper_name(&folder_name(&folder));
    assert!(
        rig.incus.instance(SNAPSHOT_PROJECT, &helper).is_none(),
        "the helper is gone"
    );
    let steps: Vec<(String, String)> = rig
        .incus
        .writes()
        .into_iter()
        .filter(|write| write.target.contains(&helper))
        .map(|write| {
            (
                write.method,
                write.body["action"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            )
        })
        .collect();
    assert_eq!(
        steps,
        [
            ("PUT".to_string(), "start".to_string()),
            ("PUT".to_string(), "stop".to_string()),
            ("DELETE".to_string(), String::new()),
            ("PUT".to_string(), "start".to_string()),
            ("PUT".to_string(), "stop".to_string()),
            ("DELETE".to_string(), String::new()),
        ],
        "stopped before it is deleted, with and without the caps"
    );
    assert_write_family(&rig.incus);
}

#[test]
fn the_helpers_root_disk_is_on_a_regular_pool_and_never_on_snapshot_storage() {
    let key = SkKey::from_seed(1);
    let folder = SourceFolder::new("root-pool");
    // No configured pool is regular and the default profile has no root disk: refused before anything is written.
    let rig = Rig::new("folder-no-root-pool", &key);
    rig.add_snapshot_pool("usb1");
    let storage = Arc::new(
        Storage::new(rig.incus.socket().to_path_buf(), rig.devices.clone(), true)
            .with_wait(Duration::from_secs(1))
            .with_helper_pools(vec!["usb1".to_string()]),
    );
    let handler = |storage: &Arc<Storage>| {
        let clock = Arc::new(|| NOW);
        plan_handlers_full(
            Arc::clone(&rig.kernel),
            clock,
            None,
            None,
            Some(Arc::clone(storage)),
            ActionSlot::default(),
        )
        .into_iter()
        .find(|(name, _)| *name == "action_propose")
        .map(|(_, handler)| handler)
        .expect("action_propose")
    };
    let propose = |storage: &Arc<Storage>| {
        handler(storage).call(
            json!({ "action": "snapshot_create", "args": { "folder": folder.text(), "pool": "usb1" } }),
            &caller(arca_api::Scope::Doctor),
        )
    };
    assert_eq!(refused_code(propose(&storage)), "HelperPoolMissing");
    // The default profile's root pool is used when no configured pool is regular.
    rig.incus.add_profile(
        "default",
        json!({ "name": "default", "devices": { "root": { "type": "disk", "path": "/", "pool": "fast" } } }),
    );
    assert!(propose(&storage).is_ok(), "the default profile's root pool");
    // A default profile whose root is on snapshot storage is never used.
    rig.incus.add_profile(
        "default",
        json!({ "name": "default", "devices": { "root": { "type": "disk", "path": "/", "pool": "usb1" } } }),
    );
    assert_eq!(refused_code(propose(&storage)), "HelperPoolMissing");
    assert!(rig.incus.writes().is_empty());
}

/// A snapshot verb that names a tenant acts on that tenant's project and on nothing of another tenant's.
#[test]
fn snapshot_verbs_with_a_tenant_stay_inside_that_tenant() {
    use arca::handlers::plan_handlers_served;
    let key = SkKey::from_seed(1);
    let rig = Rig::new("snapshot-tenant", &key);
    rig.add_snapshot_pool("usb1");
    let (a, b) = ("aaaaaaaa", "bbbbbbbb");
    let project = |id: &str| format!("arcak-t-{id}");
    // Two tenants, each with an instance of its `web` service, and the node's own `web` in `shop`.
    let source = rig.incus.instance("shop", "web").expect("the fixture app");
    for (n, id) in [a, b].into_iter().enumerate() {
        rig.incus
            .add_project(&project(id), json!({ "features.networks": "false" }));
        let mut instance = source.clone();
        instance["name"] = json!(format!("web-{id}"));
        instance["project"] = json!(project(id));
        let uuid = format!("00000000-0000-4000-8000-0000000000b{n}");
        instance["config"]["volatile.uuid"] = json!(uuid);
        instance["expanded_config"]["volatile.uuid"] = json!(uuid);
        rig.incus.add_instance(instance);
        let block = format!("192.0.2.{}/29", 248 - 8 * n);
        rig.kernel
            .lock()
            .expect("the kernel lock")
            .propose(
                vec![arca::desired_store::EntryChange::Put(
                    arca::handlers::service_entry(
                        &format!("tenant-{id}"),
                        "arca.tenant",
                        json!({ "id": id, "project": project(id), "bridge": "arcak-svcbr",
                            "cidr": "192.0.2.0/24", "block": block, "pool": "fast",
                            "caps": { "instances": 4, "containers": 4, "virtualMachines": 0, "cpu": 8,
                                "memory": "16GiB", "disk": "100GiB" },
                            "created": NOW }),
                    ),
                )],
                NOW,
            )
            .expect("the tenant's entry is recorded");
    }
    let node: arca::config::NodeConfig = serde_json::from_value(json!({
        "stateDir": rig._scratch.dir,
        "incus": { "socket": rig.incus.socket() },
        "listen": { "unix": rig._scratch.path("arca.sock") },
        "tokens": { "hashDir": rig._scratch.path("tokens") },
        "kernel": {
            "projects": ["arcak-svc"], "droiddProjects": ["droid"], "serviceCidr": "192.0.2.0/24",
            "tenants": {
                "capacity": { "instances": 40, "cpu": 64, "memory": "128GiB", "disk": "2TiB" },
                "maxTenants": 6,
                "defaultCaps": { "instances": 4, "containers": 4, "virtualMachines": 0, "cpu": 8,
                    "memory": "16GiB", "disk": "100GiB" },
                "createRate": { "count": 5, "perSeconds": 3600 }
            }
        }
    }))
    .expect("the node config");
    let node = Arc::new(node);
    let handlers: std::collections::HashMap<&'static str, Arc<dyn ToolHandler>> =
        plan_handlers_served(
            Arc::clone(&rig.kernel),
            Arc::new(|| NOW),
            None,
            None,
            Some(Arc::clone(&rig.storage)),
            None,
            ActionSlot::default(),
            Some(Arc::clone(&node)),
        )
        .into_iter()
        .collect();
    let propose = |action: &str, args: Value| {
        call_verb(
            &handlers,
            "action_propose",
            json!({ "action": action, "args": args }),
        )
    };
    // The listing as a node with tenants serves it: a named tenant must be one the node holds.
    let served = |node: Arc<arca::config::NodeConfig>| arca::handlers::SnapshotListInTenant {
        inner: Arc::new(SnapshotListHandler {
            storage: Arc::clone(&rig.storage),
        }),
        kernel: Arc::clone(&rig.kernel),
        cfg: node,
    };
    let listing = served(Arc::clone(&node));
    let list = |args: Value| -> Vec<String> {
        listing
            .call(args, &caller(arca_api::Scope::Viewer))
            .expect("snapshot_list answers")["snapshots"]
            .as_array()
            .expect("snapshots")
            .iter()
            .map(|row| row["name"].as_str().expect("a name").to_string())
            .collect()
    };
    // A create that names a tenant is given that tenant's project, and the owner's card says so.
    let mut names = Vec::new();
    for id in [a, b] {
        let proposed = propose(
            "snapshot_create",
            json!({ "tenant": id, "instance": format!("web-{id}"), "pool": "usb1" }),
        )
        .expect("a snapshot in the tenant may be proposed");
        assert_eq!(proposed["args"]["project"], json!(project(id)));
        assert_eq!(proposed["args"]["tenant"], json!(id));
        assert_eq!(row(&proposed["details"], "Tenant"), id);
        rig.approve_and_run(&key, &proposed).expect("the copy runs");
        names.push(
            proposed["args"]["name"]
                .as_str()
                .expect("a name")
                .to_string(),
        );
    }
    let own = propose(
        "snapshot_create",
        json!({ "project": "shop", "instance": "web", "pool": "usb1" }),
    )
    .expect("the node's own snapshot");
    rig.approve_and_run(&key, &own).expect("the copy runs");
    let own = own["args"]["name"].as_str().expect("a name").to_string();
    // A tenant's instance is not reached under another tenant's name, and no other project is named beside a tenant.
    assert_eq!(
        refused_code(propose(
            "snapshot_create",
            json!({ "tenant": a, "instance": format!("web-{b}"), "pool": "usb1" }),
        )),
        "SourceUnknown"
    );
    assert!(matches!(
        propose(
            "snapshot_create",
            json!({ "tenant": a, "project": project(b), "instance": format!("web-{b}"), "pool": "usb1" }),
        ),
        Err(CallError::Invalid(_))
    ));
    assert_eq!(
        refused_code(propose(
            "snapshot_create",
            json!({ "tenant": "cccccccc", "instance": "web", "pool": "usb1" }),
        )),
        "not_found"
    );
    // The listing with a tenant holds that tenant's snapshots only.
    assert_eq!(list(json!({ "tenant": a })), vec![names[0].clone()]);
    assert_eq!(list(json!({ "tenant": b })), vec![names[1].clone()]);
    // A tenant the node does not hold is not found, and a node without the key has no tenants: neither lists anything.
    assert_eq!(
        refused_code(listing.call(
            json!({ "tenant": "cccccccc" }),
            &caller(arca_api::Scope::Viewer)
        )),
        "not_found"
    );
    let mut plain = (*node).clone();
    plain.kernel.tenants = None;
    assert_eq!(
        refused_code(
            served(Arc::new(plain)).call(json!({ "tenant": a }), &caller(arca_api::Scope::Viewer))
        ),
        "no_tenants"
    );
    // Without a tenant the verbs see nothing of any tenant: the listing holds the node's own snapshot only, a
    // tenant's project is not named by project, and a tenant's snapshot is not found.
    assert_eq!(list(json!({})), vec![own.clone()]);
    assert!(matches!(
        propose(
            "snapshot_create",
            json!({ "project": project(b), "instance": format!("web-{b}"), "pool": "usb1" }),
        ),
        Err(CallError::Invalid(_))
    ));
    let waiting_before = rig.kernel.lock().expect("lock").action_proposals().len();
    for name in &names {
        assert_eq!(
            refused_code(propose("snapshot_delete", json!({ "name": name }))),
            "not_found"
        );
    }
    assert_eq!(
        rig.kernel.lock().expect("lock").action_proposals().len(),
        waiting_before
    );
    // A delete under a tenant reaches that tenant's snapshots only: another tenant's, and the node's own, are not
    // found, and nothing is proposed for them.
    let waiting = |kernel: &Mutex<Kernel>| kernel.lock().expect("lock").action_proposals().len();
    let before = waiting(&rig.kernel);
    for name in [&names[1], &own] {
        assert_eq!(
            refused_code(propose(
                "snapshot_delete",
                json!({ "tenant": a, "name": name })
            )),
            "not_found"
        );
    }
    assert_eq!(waiting(&rig.kernel), before);
    let proposed = propose("snapshot_delete", json!({ "tenant": a, "name": names[0] }))
        .expect("the tenant's own snapshot may be deleted");
    rig.approve_and_run(&key, &proposed)
        .expect("the delete runs");
    assert_eq!(list(json!({ "tenant": a })), Vec::<String>::new());
    assert_eq!(list(json!({ "tenant": b })), vec![names[1].clone()]);
    assert_eq!(list(json!({})), vec![own.clone()]);
    // The node's own snapshot is the node's to delete, without a tenant.
    let proposed = propose("snapshot_delete", json!({ "name": own })).expect("the node's own");
    rig.approve_and_run(&key, &proposed)
        .expect("the delete runs");
    assert_eq!(list(json!({})), Vec::<String>::new());
    assert_eq!(list(json!({ "tenant": b })), vec![names[1].clone()]);
    assert_write_family(&rig.incus);
}
