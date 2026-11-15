//! The import on made-up source descriptions: what is read, what Incus is asked to make, the seed, the card, and the
//! run with a fake Incus and a fake disk copy, each failure with its undo.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::net::Ipv4Addr;

use arca_vmimport::source::{Bus, DiskKind, Firmware, Protocol};
use arca_vmimport::{
    FakeCopy, Held, IMPORT_KEY, ImportIncus, Place, Plan, PoolInfo, Probe, parse, plan_of, run,
    undo,
};
use serde_json::{Value, json};

const BIOS: &str = include_str!("fixtures/bios-one-disk.xml");
const UEFI: &str = include_str!("fixtures/uefi-two-disks-sg.xml");
const CLONE: &str = include_str!("fixtures/clone-of-base.xml");
const NO_NIC: &str = include_str!("fixtures/no-nic.xml");

fn args(source: &str) -> Value {
    json!({
        "name": "web-1",
        "project": "people",
        "pool": "remote",
        "bridge": "br-lan",
        "source": source,
    })
}

fn plan(source: &str) -> Plan {
    plan_of(&args(source)).expect("a plan")
}

#[test]
fn a_bios_vm_with_one_disk_reads_whole() {
    let vm = parse(BIOS).expect("parsed");
    assert_eq!((vm.id, vm.name.as_str()), (7, "web-1"));
    assert_eq!(vm.state, Some("powered off"));
    assert_eq!((vm.vcpu, vm.memory_mib, vm.memory_max_mib), (2, 4096, None));
    assert_eq!(vm.firmware, Firmware::Bios);
    assert_eq!(vm.machine.as_deref(), Some("pc-i440fx-8.2"));
    assert_eq!(vm.disks.len(), 1);
    let disk = &vm.disks[0];
    assert_eq!(
        (disk.pool.as_str(), disk.image.as_str(), disk.size_mib),
        ("src-pool", "web-1-root", 20480)
    );
    assert_eq!(disk.bus, Bus::VirtioBlk);
    assert!(
        matches!(&disk.kind, DiskKind::Persistent { image_name } if image_name == "web-1-root")
    );
    assert_eq!(vm.nics.len(), 1);
    assert_eq!(vm.nics[0].mac, "02:00:c0:00:02:0a");
    assert_eq!(vm.nics[0].bridge, "br-lan");
    let eth = &vm.context.eth[&0];
    assert_eq!(eth.ip, Some(Ipv4Addr::new(192, 0, 2, 10)));
    assert_eq!(eth.prefix, Some(24));
    assert_eq!(eth.dns.len(), 2);
    assert_eq!(eth.search, ["example.test"]);
    assert_eq!(vm.context.ssh_keys.len(), 2);
    assert_eq!(vm.context.hostname.as_deref(), Some("web-1"));
    assert!(
        vm.context
            .user_data
            .as_deref()
            .is_some_and(|data| data.starts_with("#cloud-config"))
    );
    // The password is named as not carried, and its value is kept nowhere.
    assert_eq!(vm.context.dropped, ["PASSWORD"]);
    assert_eq!(vm.digest.len(), 64);
}

#[test]
fn a_uefi_vm_with_two_disks_and_a_security_group_reads_whole() {
    let vm = parse(UEFI).expect("parsed");
    assert_eq!(vm.name, "build & test");
    assert_eq!(vm.state, Some("running"));
    assert_eq!(vm.firmware, Firmware::Uefi { secure: false });
    assert_eq!(vm.memory_max_mib, Some(65536));
    assert_eq!(vm.disks.len(), 2);
    assert_eq!(vm.disks[0].image, "build-root");
    assert_eq!(vm.disks[1].image, "build-data");
    assert_eq!(vm.disks[1].bus, Bus::Scsi);
    assert_eq!(vm.nics[0].mac, "02:00:c0:00:02:15");
    assert_eq!(vm.nics[0].security_groups, [0, 104]);
    assert_eq!(vm.rules.len(), 5);
    assert_eq!(vm.rules[2].protocol, Protocol::Icmp);
    assert_eq!(vm.context.eth[&0].mtu, Some(9000));
    // Base64 user data is decoded.
    assert_eq!(
        vm.context.user_data.as_deref(),
        Some("#cloud-config\ntimezone: UTC")
    );
    assert_eq!(vm.context.dropped, ["START_SCRIPT_BASE64"]);
}

#[test]
fn a_clone_of_a_base_image_names_its_own_image_and_its_base() {
    let vm = parse(CLONE).expect("parsed");
    assert_eq!(vm.state, Some("undeployed"));
    assert_eq!(vm.disks.len(), 2);
    assert_eq!(vm.disks[0].image, "developer-base-v1-16-0");
    assert!(
        matches!(&vm.disks[0].kind, DiskKind::Clone { base, .. } if base == "developer-base-v1")
    );
    assert_eq!(vm.disks[1].image, "one-sys-16-1");
    assert!(matches!(&vm.disks[1].kind, DiskKind::Volatile { format } if format == "swap"));
    assert_eq!(vm.skipped_disks.len(), 1);
    assert_eq!(vm.skipped_disks[0].why, "a CD-ROM");
    // HOSTNAME stands in for SET_HOSTNAME.
    assert_eq!(vm.context.hostname.as_deref(), Some("dev-ana"));
}

#[test]
fn a_vm_with_no_nic_needs_no_bridge() {
    let mut no_bridge = args(NO_NIC);
    no_bridge
        .as_object_mut()
        .expect("an object")
        .remove("bridge");
    let plan = plan_of(&no_bridge).expect("a plan");
    assert!(plan.source.nics.is_empty());
    assert!(plan.read_back.is_none());
    assert!(plan.seed.network_config.is_none());
    // The fallback when the description says no boot order: the lowest disk id; its bus defaults to virtio-blk.
    assert_eq!(plan.copies[0].source.bus, Bus::VirtioBlk);
    // A source with a NIC needs one.
    let mut bios = args(BIOS);
    bios.as_object_mut().expect("an object").remove("bridge");
    assert!(plan_of(&bios).expect_err("refused").contains("bridge"));
}

#[test]
fn descriptions_arca_cannot_read_are_refused_with_the_place() {
    let cases = [
        ("not xml", "not one <VM>"),
        ("<VM><ID>1</VM>", "not XML"),
        ("<VM><ID>1</ID></VM><VM/>", "not one <VM>"),
        ("<!DOCTYPE x [<!ENTITY a \"b\">]><VM/>", "document type"),
        (
            &BIOS.replace(
                "<MAC><![CDATA[02:00:c0:00:02:0a]]>",
                "<MAC><![CDATA[02:00:c0:00:02]]>",
            ),
            "MAC",
        ),
        (&BIOS.replace("br-lan", "br lan; rm"), "BRIDGE"),
        (
            &BIOS.replace("src-pool/web-1-root", "src-pool/web 1"),
            "Ceph",
        ),
        (&UEFI.replace("22,8000:8100", "22;8000"), "RANGE"),
        (
            &BIOS.replace("<MEMORY><![CDATA[4096]]></MEMORY>", ""),
            "MEMORY",
        ),
        (
            &BIOS
                .replace("<FIRMWARE>", "")
                .replace("<MACHINE>", "<FIRMWARE>coreboot</FIRMWARE><MACHINE>"),
            "FIRMWARE",
        ),
    ];
    for (xml, words) in cases {
        let error = parse(xml).expect_err(xml).to_string();
        assert!(error.contains(words), "{error} should name {words}");
    }
    let long = format!("<VM>{}</VM>", " ".repeat(60_000));
    assert!(parse(&long).is_err());
}

#[test]
fn the_vm_body_has_the_source_size_firmware_disks_and_mac() {
    let plan = plan(BIOS);
    let body = plan.instance_body(false);
    assert_eq!(body["type"], "virtual-machine");
    assert_eq!(body["source"], json!({ "type": "none" }));
    assert_eq!(body["start"], false);
    assert_eq!(body["profiles"], json!([]));
    let config = &body["config"];
    assert_eq!(config["limits.cpu"], "2");
    assert_eq!(config["limits.memory"], "4096MiB");
    assert_eq!(config["security.csm"], "true");
    assert_eq!(config["security.secureboot"], "false");
    assert_eq!(config[IMPORT_KEY], json!(plan.mark));
    assert_eq!(plan.mark, plan.source.digest[..16]);
    let devices = &body["devices"];
    assert_eq!(
        devices["root"],
        json!({ "type": "disk", "path": "/", "pool": "remote", "size": "20480MiB", "boot.priority": "1", "io.bus": "virtio-blk" })
    );
    assert_eq!(
        devices["eth0"],
        json!({ "type": "nic", "nictype": "bridged", "parent": "br-lan", "hwaddr": "02:00:c0:00:02:0a" })
    );
    assert_eq!(
        devices["cloud-init"],
        json!({ "type": "disk", "source": "cloud-init:config" })
    );
    // A size given replaces the source's.
    let mut resized = args(BIOS);
    resized["cpu"] = json!(1);
    resized["memoryMib"] = json!(2048);
    let body = plan_of(&resized).expect("a plan").instance_body(false);
    assert_eq!(body["config"]["limits.cpu"], "1");
    assert_eq!(body["config"]["limits.memory"], "2048MiB");
}

#[test]
fn a_second_disk_is_a_custom_block_volume_after_the_root() {
    let plan = plan(UEFI);
    assert_eq!(plan.volumes(), [("web-1-disk1".to_string(), 163_840)]);
    assert_eq!(
        plan.volume_body("web-1-disk1", 163_840),
        json!({
            "name": "web-1-disk1",
            "type": "custom",
            "content_type": "block",
            "description": "Disk of web-1 imported from an outside hypervisor",
            "config": { "size": "163840MiB", IMPORT_KEY: plan.mark },
        })
    );
    let body = plan.instance_body(true);
    assert_eq!(body["config"]["security.secureboot"], "false");
    assert!(body["config"].get("security.csm").is_none());
    let devices = &body["devices"];
    assert_eq!(devices["root"]["boot.priority"], "2");
    assert!(devices["root"].get("io.bus").is_none());
    assert_eq!(
        devices["disk1"],
        json!({ "type": "disk", "pool": "remote", "source": "web-1-disk1", "boot.priority": "1" })
    );
    // A managed bridge's NIC names the network and carries the ACL, dropping what it does not allow.
    assert_eq!(
        devices["eth0"],
        json!({
            "type": "nic", "network": "br-lan", "hwaddr": "02:00:c0:00:02:15",
            "security.acls": "arca-import-web-1",
            "security.acls.default.ingress.action": "drop",
            "security.acls.default.egress.action": "drop",
        })
    );
}

#[test]
fn the_security_groups_become_one_acl_and_what_cannot_be_carried_is_named() {
    let plan = plan(UEFI);
    assert_eq!(plan.acls.len(), 1);
    let body = plan.acl_body(&plan.acls[0]);
    assert_eq!(body["name"], "arca-import-web-1");
    assert_eq!(body["config"][IMPORT_KEY], json!(plan.mark));
    assert_eq!(
        body["ingress"],
        json!([
            {
                "action": "allow", "state": "enabled", "protocol": "tcp", "destination_port": "22,8000-8100",
                "source": "192.0.2.0-192.0.2.255", "description": "from the source's group 104 (build-private)",
            },
            {
                "action": "allow", "state": "enabled", "protocol": "icmp4", "icmp_type": "8",
                "description": "from the source's group 104 (build-private)",
            },
        ])
    );
    assert_eq!(
        body["egress"],
        json!([{ "action": "allow", "state": "enabled", "description": "from the source's group 0 (default)" }])
    );
    // The rule that names only a network is left out and named; the group the NIC does not name is not used.
    assert_eq!(plan.rules_left_out.len(), 1);
    assert!(plan.rules_left_out[0].contains("network 9"));
    assert!(!body.to_string().contains("3306"));
    // A source with no group has no ACL.
    assert!(super_plan(BIOS).acls.is_empty());
}

fn super_plan(source: &str) -> Plan {
    plan(source)
}

#[test]
fn the_seed_carries_the_network_keys_hostname_and_user_data() {
    let plan = plan(BIOS);
    assert_eq!(
        plan.seed.network_config.as_deref(),
        Some(
            "ethernets:\n  eth0:\n    addresses:\n      - \"192.0.2.10/24\"\n    match:\n      macaddress: \"02:00:c0:00:02:0a\"\n    nameservers:\n      addresses:\n        - \"192.0.2.53\"\n        - \"198.51.100.53\"\n      search:\n        - \"example.test\"\n    routes:\n      - to: \"0.0.0.0/0\"\n        via: \"192.0.2.1\"\nversion: 2\n"
        )
    );
    assert_eq!(
        plan.seed.vendor_data,
        "#cloud-config\nhostname: \"web-1\"\nssh_authorized_keys:\n  - \"ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExampleKeyOne person@example.test\"\n  - \"ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExampleKeyTwo other@example.test\"\nssh_deletekeys: false\n"
    );
    let user = plan.seed.user_data.as_deref().expect("user data");
    assert!(user.starts_with("#cloud-config\npackages:"));
    let body = plan.instance_body(false);
    assert_eq!(body["config"]["cloud-init.user-data"], json!(user));
    assert_eq!(
        body["config"]["cloud-init.vendor-data"],
        json!(plan.seed.vendor_data)
    );
    // userData: false leaves it out.
    let mut without = args(BIOS);
    without["userData"] = json!(false);
    let plan = plan_of(&without).expect("a plan");
    assert!(plan.seed.user_data.is_none());
    assert!(
        plan.instance_body(false)["config"]
            .get("cloud-init.user-data")
            .is_none()
    );
    // A NIC whose context has no address asks DHCP; an MTU is carried.
    let uefi = plan_of(&args(UEFI)).expect("a plan");
    assert!(
        uefi.seed
            .network_config
            .as_deref()
            .is_some_and(|text| text.contains("mtu: 9000"))
    );
}

#[test]
fn the_card_names_every_step_and_what_is_not_carried() {
    let plan = plan(CLONE);
    let card = plan.card();
    let row = |label: &str| {
        card.iter()
            .find(|(name, _)| name == label)
            .map(|(_, value)| value.clone())
            .unwrap_or_else(|| panic!("no row {label}"))
    };
    assert!(row("Source").contains("dev-ana (id 16)"));
    assert!(row("Source").contains("undeployed"));
    assert_eq!(row("Size"), "4 CPUs, 16 GiB memory, as the source");
    assert!(row("Boot").starts_with("BIOS"));
    assert!(row("Disk 1").contains("src-pool/developer-base-v1-16-0 (200 GiB)"));
    assert!(row("Disk 1").contains("base image developer-base-v1"));
    assert!(row("Disk 2").contains("volatile swap disk"));
    assert!(row("Disk 2").contains("web-1-disk1"));
    assert_eq!(row("Disk not copied"), "disk 2: a CD-ROM");
    assert!(
        row("NIC 1").contains("02:00:c0:00:02:10, address 192.0.2.16, on the host bridge br-lan")
    );
    assert!(row("Inbound and outbound rules").starts_with("None"));
    assert!(row("Before you approve").contains("Power the source off"));
    assert!(row("Read-back").contains("192.0.2.16"));
    assert!(row("Not carried").contains("snapshots"));
    assert_eq!(
        plan.summary(),
        "Move the VM dev-ana (id 16) from the outside hypervisor into the instance web-1 in project people"
    );
    // A resized, UEFI, i440fx and user-data card says each.
    let mut resized = args(UEFI);
    resized["memoryMib"] = json!(8192);
    let card = plan_of(&resized).expect("a plan").card();
    let words: String = card
        .iter()
        .map(|(label, value)| format!("{label}: {value}\n"))
        .collect();
    assert!(words.contains("Size: 4 CPUs, 8 GiB memory; the source has 4 CPUs and 24 GiB"));
    assert!(words.contains("UEFI, secure boot off"));
    assert!(words.contains("memory hot-plug up to 64 GiB"));
    assert!(words.contains("context keys START_SCRIPT_BASE64"));
    assert!(words.contains("runs again on the first boot"));
    assert!(
        words.contains("inbound only tcp 22,8000-8100 from 192.0.2.0-192.0.2.255, icmp4 type 8")
    );
    assert!(words.contains("Rules not carried"));
    let bios = plan_of(&args(BIOS)).expect("a plan").card();
    assert!(
        bios.iter()
            .any(|(_, value)| value.contains("pc-i440fx-8.2"))
    );
}

#[test]
fn the_same_arguments_give_the_same_plan() {
    assert_eq!(plan(UEFI), plan(UEFI));
    assert_ne!(plan(UEFI).mark, plan(BIOS).mark);
}

#[test]
fn a_disk_outside_the_node_source_pool_is_refused() {
    let plan = plan(BIOS);
    assert!(plan.check_source_pool("src-pool").is_ok());
    assert!(
        plan.check_source_pool("other")
            .expect_err("refused")
            .contains("src-pool")
    );
}

#[test]
fn nics_on_two_bridges_or_no_ceph_disk_are_refused() {
    let two = UEFI.replace(
        "</NIC>",
        "</NIC><NIC><BRIDGE>br-other</BRIDGE><MAC>02:00:c0:00:02:99</MAC><NIC_ID>1</NIC_ID></NIC>",
    );
    assert!(
        plan_of(&args(&two))
            .expect_err("refused")
            .contains("more than one bridge")
    );
    let local = BIOS.replace(
        "<TM_MAD><![CDATA[ceph]]></TM_MAD>",
        "<TM_MAD>qcow2</TM_MAD>",
    );
    assert!(
        plan_of(&args(&local))
            .expect_err("refused")
            .contains("no Ceph disk")
    );
    let bad = json!({ "name": "9web", "project": "people", "pool": "remote", "source": NO_NIC });
    assert!(plan_of(&bad).expect_err("refused").starts_with("args/name"));
}

/// An Incus in memory, recording every call.
#[derive(Default)]
struct FakeImport {
    managed: bool,
    driver: Option<String>,
    exists: bool,
    fail_on: Option<&'static str>,
    agent: bool,
    calls: RefCell<Vec<String>>,
    objects: RefCell<BTreeMap<String, Value>>,
}

impl FakeImport {
    fn call(&self, what: String) -> Result<(), String> {
        let refused = self.fail_on.is_some_and(|step| what.starts_with(step));
        self.calls.borrow_mut().push(what);
        if refused {
            Err("Incus refused it".to_string())
        } else {
            Ok(())
        }
    }
}

impl ImportIncus for FakeImport {
    fn place(&self, _project: &str, pool: &str, _bridge: Option<&str>) -> Result<Place, String> {
        Ok(Place {
            pool: PoolInfo {
                driver: self.driver.clone().unwrap_or_else(|| "ceph".to_string()),
                osd_pool: format!("{pool}-osd"),
                cluster: "ceph".to_string(),
                client: "incus".to_string(),
            },
            volume_project: "people".to_string(),
            acl_project: "default".to_string(),
            managed: Some(self.managed),
        })
    }
    fn homes(&self, _project: &str) -> Result<(String, String), String> {
        Ok(("people".to_string(), "default".to_string()))
    }
    fn instance_exists(&self, _project: &str, _name: &str) -> Result<bool, String> {
        Ok(self.exists)
    }
    fn create_acl(&self, project: &str, body: &Value) -> Result<(), String> {
        self.call(format!(
            "create acl {project}/{}",
            body["name"].as_str().unwrap_or_default()
        ))?;
        self.objects.borrow_mut().insert(
            format!("acl {}", body["name"].as_str().unwrap_or_default()),
            body.clone(),
        );
        Ok(())
    }
    fn create_volume(&self, project: &str, pool: &str, body: &Value) -> Result<(), String> {
        self.call(format!(
            "create volume {project}/{pool}/{}",
            body["name"].as_str().unwrap_or_default()
        ))
    }
    fn create_instance(&self, project: &str, body: &Value) -> Result<(), String> {
        self.call(format!(
            "create instance {project}/{}",
            body["name"].as_str().unwrap_or_default()
        ))?;
        self.objects
            .borrow_mut()
            .insert("instance".to_string(), body.clone());
        Ok(())
    }
    fn start(&self, project: &str, name: &str) -> Result<(), String> {
        self.call(format!("start {project}/{name}"))
    }
    fn agent_answers(&self, _project: &str, _name: &str) -> bool {
        self.agent
    }
    fn remove_instance(&self, project: &str, name: &str) -> Result<Option<Held>, String> {
        self.call(format!("remove instance {project}/{name}"))?;
        let Some(body) = self.objects.borrow_mut().remove("instance") else {
            return Ok(None);
        };
        let devices = body["devices"].as_object().cloned().unwrap_or_default();
        Ok(Some(Held {
            volumes: devices
                .values()
                .filter_map(|device| {
                    Some((
                        device["pool"].as_str()?.to_string(),
                        device["source"].as_str()?.to_string(),
                    ))
                })
                .collect(),
            acls: devices
                .values()
                .filter_map(|device| device["security.acls"].as_str().map(str::to_string))
                .collect(),
        }))
    }
    fn remove_volume(&self, project: &str, pool: &str, name: &str) -> Result<bool, String> {
        self.call(format!("remove volume {project}/{pool}/{name}"))
            .map(|()| true)
    }
    fn remove_acl(&self, project: &str, name: &str) -> Result<bool, String> {
        self.call(format!("remove acl {project}/{name}"))
            .map(|()| true)
    }
}

/// A port that answers from the `answer_on`th try.
struct FakeProbe {
    answer_on: Option<u32>,
    tries: RefCell<u32>,
}

impl Probe for FakeProbe {
    fn port_open(&self, ip: Ipv4Addr, port: u16) -> bool {
        assert_eq!(port, 22);
        assert!(ip.is_documentation());
        *self.tries.borrow_mut() += 1;
        self.answer_on.is_some_and(|on| *self.tries.borrow() >= on)
    }
    fn pause(&self) {}
}

fn probe(answer_on: Option<u32>) -> FakeProbe {
    FakeProbe {
        answer_on,
        tries: RefCell::new(0),
    }
}

/// A disk copy where Incus made the images the plan asks for, with the right sizes.
fn copy_for(plan: &Plan) -> FakeCopy {
    let copy = FakeCopy::default();
    copy.target(
        "remote-osd/virtual-machine_people_web-1.block",
        plan.copies[0].source.size_mib * 1024 * 1024,
    );
    for (name, size) in plan.volumes() {
        copy.target(
            &format!("remote-osd/custom_people_{name}.block"),
            size * 1024 * 1024,
        );
    }
    copy
}

#[test]
fn an_approved_import_makes_copies_starts_and_reads_back() {
    let plan = plan(UEFI);
    let incus = FakeImport {
        managed: true,
        ..FakeImport::default()
    };
    let copy = copy_for(&plan);
    let answer = run(&plan, "src-pool", &incus, &copy, &probe(Some(3))).expect("imported");
    assert_eq!(
        *incus.calls.borrow(),
        [
            "create acl default/arca-import-web-1",
            "create volume people/remote/web-1-disk1",
            "create instance people/web-1",
            "start people/web-1",
        ]
    );
    assert_eq!(
        copy.copied(),
        [
            (
                "src-pool/build-root".to_string(),
                "remote-osd/virtual-machine_people_web-1.block".to_string()
            ),
            (
                "src-pool/build-data".to_string(),
                "remote-osd/custom_people_web-1-disk1.block".to_string()
            ),
        ]
    );
    assert_eq!(
        answer["readBack"],
        json!({ "agent": false, "address": "192.0.2.21:22", "answered": true })
    );
    assert_eq!(answer["acls"], json!(["arca-import-web-1"]));
    assert_eq!(answer["note"], "The VM runs and read back.");
}

#[test]
fn a_failed_read_back_leaves_the_vm_running_and_says_so() {
    let plan = plan(BIOS);
    let incus = FakeImport {
        agent: true,
        ..FakeImport::default()
    };
    let answer = run(&plan, "src-pool", &incus, &copy_for(&plan), &probe(None)).expect("imported");
    assert_eq!(answer["readBack"]["answered"], false);
    assert_eq!(answer["readBack"]["agent"], true);
    assert!(
        answer["note"]
            .as_str()
            .expect("words")
            .contains("instance_import_undo")
    );
    assert!(
        !incus
            .calls
            .borrow()
            .iter()
            .any(|call| call.starts_with("remove"))
    );
}

#[test]
fn start_false_copies_and_leaves_it_stopped() {
    let mut stopped = args(BIOS);
    stopped["start"] = json!(false);
    let plan = plan_of(&stopped).expect("a plan");
    let incus = FakeImport::default();
    let answer = run(&plan, "src-pool", &incus, &copy_for(&plan), &probe(None)).expect("imported");
    assert_eq!(answer["started"], false);
    assert_eq!(
        answer["readBack"],
        json!({ "agent": null, "address": null, "answered": null })
    );
    assert!(
        !incus
            .calls
            .borrow()
            .iter()
            .any(|call| call.starts_with("start"))
    );
}

#[test]
fn a_watched_source_disk_stops_the_import_before_anything_is_made() {
    let plan = plan(BIOS);
    let incus = FakeImport::default();
    let copy = copy_for(&plan);
    copy.watch("src-pool/web-1-root", "192.0.2.200:0/123");
    let failure = run(&plan, "src-pool", &incus, &copy, &probe(Some(1))).expect_err("refused");
    assert!(failure.message.contains("the source still runs"));
    assert!(failure.words().contains("Nothing was made"));
    assert!(incus.calls.borrow().is_empty());
    assert!(copy.copied().is_empty());
}

#[test]
fn rules_on_a_bridge_incus_does_not_manage_stop_the_import() {
    let plan = plan(UEFI);
    let incus = FakeImport {
        managed: false,
        ..FakeImport::default()
    };
    let failure =
        run(&plan, "src-pool", &incus, &copy_for(&plan), &probe(Some(1))).expect_err("refused");
    assert_eq!(failure.step, "Checking the bridge");
    assert!(incus.calls.borrow().is_empty());
}

#[test]
fn a_pool_that_is_not_ceph_a_taken_name_or_another_source_pool_are_refused() {
    let plan = plan(BIOS);
    let copy = copy_for(&plan);
    let lvm = FakeImport {
        driver: Some("lvm".into()),
        ..FakeImport::default()
    };
    assert!(
        run(&plan, "src-pool", &lvm, &copy, &probe(Some(1)))
            .expect_err("refused")
            .message
            .contains("lvm")
    );
    let taken = FakeImport {
        exists: true,
        ..FakeImport::default()
    };
    assert!(
        run(&plan, "src-pool", &taken, &copy, &probe(Some(1)))
            .expect_err("refused")
            .message
            .contains("already exists")
    );
    let fine = FakeImport::default();
    assert!(run(&plan, "elsewhere", &fine, &copy, &probe(Some(1))).is_err());
    assert!(fine.calls.borrow().is_empty());
}

#[test]
fn a_failed_copy_removes_what_the_run_made_newest_first() {
    let plan = plan(UEFI);
    let incus = FakeImport {
        managed: true,
        ..FakeImport::default()
    };
    let copy = copy_for(&plan);
    copy.fail_copy_into("remote-osd/custom_people_web-1-disk1.block");
    let failure = run(&plan, "src-pool", &incus, &copy, &probe(Some(1))).expect_err("failed");
    assert_eq!(failure.step, "Copying a disk");
    assert_eq!(
        failure.removed,
        [
            "the instance web-1",
            "the volume web-1-disk1",
            "the ACL arca-import-web-1"
        ]
    );
    assert!(failure.left.is_empty());
    let calls = incus.calls.borrow();
    assert_eq!(
        calls[3..],
        [
            "remove instance people/web-1",
            "remove volume people/remote/web-1-disk1",
            "remove acl default/arca-import-web-1",
        ]
    );
    assert!(failure.words().ends_with("The source was not written."));
}

#[test]
fn a_target_image_of_the_wrong_size_is_never_written() {
    let plan = plan(BIOS);
    let incus = FakeImport::default();
    let copy = FakeCopy::default();
    copy.target("remote-osd/virtual-machine_people_web-1.block", 1024);
    let failure = run(&plan, "src-pool", &incus, &copy, &probe(Some(1))).expect_err("failed");
    assert_eq!(failure.step, "Finding the disk Incus made");
    assert!(copy.copied().is_empty());
    assert_eq!(failure.removed, ["the instance web-1"]);
    // An image Incus did not make is the same refusal.
    let none = FakeCopy::default();
    let incus = FakeImport::default();
    assert!(run(&plan, "src-pool", &incus, &none, &probe(Some(1))).is_err());
}

#[test]
fn a_refused_start_removes_everything_and_a_refused_removal_is_named() {
    let plan = plan(BIOS);
    let incus = FakeImport {
        fail_on: Some("start"),
        ..FakeImport::default()
    };
    let failure =
        run(&plan, "src-pool", &incus, &copy_for(&plan), &probe(Some(1))).expect_err("failed");
    assert_eq!(failure.step, "Starting the VM");
    assert_eq!(failure.removed, ["the instance web-1"]);
    let stuck = FakeImport {
        fail_on: Some("remove"),
        managed: true,
        ..FakeImport::default()
    };
    let copy = copy_for(&plan);
    copy.fail_copy_into("remote-osd/virtual-machine_people_web-1.block");
    let failure = run(&plan, "src-pool", &stuck, &copy, &probe(Some(1))).expect_err("failed");
    assert_eq!(failure.left.len(), 1);
    assert!(
        failure
            .words()
            .contains("Could not remove: the instance web-1 (Incus refused it)")
    );
}

#[test]
fn undo_removes_the_instance_its_volumes_and_its_acl() {
    let plan = plan(UEFI);
    let incus = FakeImport {
        managed: true,
        ..FakeImport::default()
    };
    run(&plan, "src-pool", &incus, &copy_for(&plan), &probe(Some(1))).expect("imported");
    let answer = undo(&incus, "people", "web-1").expect("undone");
    assert_eq!(
        answer["removed"],
        json!([
            "the instance web-1",
            "the volume web-1-disk1",
            "the ACL arca-import-web-1"
        ])
    );
    assert!(
        undo(&incus, "people", "web-1")
            .expect_err("gone")
            .contains("no instance")
    );
}
