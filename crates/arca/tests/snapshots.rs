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
    DeviceLinks, ERASE_SENTENCE, ExclusiveOpen, FixedBlockDevices, SnapshotListHandler, Storage,
    StorageListHandler, snapshot_name,
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
    // A device Arca may not open is not erased either.
    rig.devices.set_open(
        "/dev/sdb1",
        ExclusiveOpen::Denied("Permission denied".into()),
    );
    assert_eq!(
        refused_code(rig.propose("storage_add", json!({ "name": "usb1", "device": "sdb" }))),
        "DiskUncheckable"
    );
    // Checked again when the approved add runs: something took the disk after the proposal.
    rig.devices.set_open("/dev/sdb1", ExclusiveOpen::Free);
    let proposed = rig
        .propose("storage_add", json!({ "name": "usb1", "device": "sdb" }))
        .expect("free when proposed");
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
