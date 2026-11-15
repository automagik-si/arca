//! The import on made-up source descriptions: what is read, what Incus is asked to make, the seed, the card, and the
//! run with a fake Incus and a fake disk copy, each failure with its undo.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::sync::Mutex;

use arca_vmimport::source::{Bus, DiskKind, Firmware, Protocol};
use arca_vmimport::{
    FakeCopy, Held, IMPORT_KEY, ImportIncus, Place, Plan, PoolInfo, Probe, RbdCopy, Runner, parse,
    plan_of, reduce, reduce_shown, run, undo,
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
fn the_reduced_description_reads_the_same_holds_no_secret_and_reduces_to_itself() {
    // A template that also holds a VNC password, notes with markup and a carriage return, and history.
    let busy = BIOS.replace(
        "<TEMPLATE>",
        "<TEMPLATE><GRAPHICS><PASSWD>vnc-secret</PASSWD></GRAPHICS><DESCRIPTION>a &amp; b</DESCRIPTION>",
    )
    .replace(
        "<SET_HOSTNAME>",
        "<CRYPTED_PASSWORD>crypt-secret</CRYPTED_PASSWORD><START_SCRIPT>start-script-secret &lt;x&gt; &amp;&#13;</START_SCRIPT><SET_HOSTNAME>",
    )
    .replace("<USER_TEMPLATE/>", "<USER_TEMPLATE><NOTE>user-template-note</NOTE></USER_TEMPLATE><HISTORY_RECORDS><HISTORY><HOSTNAME>h</HOSTNAME></HISTORY></HISTORY_RECORDS>");
    let with_odd_user_data = UEFI.replace(
        "<USERDATA_ENCODING><![CDATA[base64]]></USERDATA_ENCODING>\n      <USER_DATA><![CDATA[I2Nsb3VkLWNvbmZpZwp0aW1lem9uZTogVVRDCg==]]></USER_DATA>",
        "<USER_DATA>#cloud-config&#13;\nruncmd: [\"a &amp;&amp; b &lt; c\"]</USER_DATA>",
    );
    assert_ne!(
        with_odd_user_data, UEFI,
        "the fixture still holds the user data the test replaces"
    );
    for xml in [
        BIOS,
        UEFI,
        CLONE,
        NO_NIC,
        busy.as_str(),
        with_odd_user_data.as_str(),
    ] {
        let reduced = reduce(xml).expect("reduced");
        assert_eq!(
            reduce(&reduced).expect("reduced again"),
            reduced,
            "a reduced text reduces to itself"
        );
        let (mut whole, mut cut) = (parse(xml).expect("read"), parse(&reduced).expect("read"));
        whole.digest.clear();
        cut.digest.clear();
        assert_eq!(cut, whole, "the reduced text reads as the whole did");
    }
    let bios = reduce(&busy).expect("reduced");
    for secret in [
        "not-a-real-password",
        "crypt-secret",
        "start-script-secret",
        "vnc-secret",
        "user-template-note",
        "a &amp; b",
    ] {
        assert!(!bios.contains(secret), "{secret} left in {bios}");
    }
    for name in ["<PASSWORD/>", "<CRYPTED_PASSWORD/>", "<START_SCRIPT/>"] {
        assert!(bios.contains(name), "{name} is kept by name");
    }
    let uefi = reduce(UEFI).expect("reduced");
    assert!(!uefi.contains("ZWNobyBoaQ=="));
    assert!(uefi.contains("<START_SCRIPT_BASE64/>"));
    assert!(
        uefi.contains("I2Nsb3VkLWNvbmZpZwp0aW1lem9uZTogVVRDCg=="),
        "the seed's user data stays"
    );
}

/// The BIOS fixture with its user data element replaced by `element`.
fn with_user_data(element: &str) -> String {
    let start = BIOS.find("<USER_DATA>").expect("the fixture's user data");
    let end = BIOS.find("</USER_DATA>").expect("its end") + "</USER_DATA>".len();
    format!("{}{element}{}", &BIOS[..start], &BIOS[end..])
}

#[test]
fn the_reduction_never_makes_a_well_formed_description_longer() {
    // Review 2's growth case: many `&` in one CDATA section beside a `]]>`, which references once made five times longer.
    let ampersands = with_user_data(&format!(
        "<USER_DATA><![CDATA[#cloud-config\n{}]]]]><![CDATA[>{}]]></USER_DATA>",
        "a&b<c>".repeat(1_800),
        "&".repeat(2_000)
    ));
    // Many carriage returns, each one a reference between short runs of text.
    let returns = with_user_data(&format!(
        "<USER_DATA>#cloud-config{}</USER_DATA>",
        "x&#13;&amp;".repeat(3_000)
    ));
    // Both kinds at once, references and CDATA mixed the way a hand-written description might.
    let mixed = with_user_data(&format!(
        "<USER_DATA>#cloud-config{}</USER_DATA>",
        "&#13;<![CDATA[&<&<&<]]>a&amp;&lt;]]&gt;".repeat(900)
    ));
    // Review 3's case: kept elements the source wrote self-closed, and one whose children are all left out. `parse`
    // reads the first of each name, so these change nothing it reads.
    let closed = BIOS.replacen(
        "</TEMPLATE>",
        &format!(
            "{}</TEMPLATE>",
            "<VCPU/><MEMORY_MAX/><OS><X>y</X></OS>".repeat(500)
        ),
        1,
    );
    for xml in [ampersands, returns, mixed, closed] {
        let reduced = reduce(&xml).expect("reduced");
        assert!(
            reduced.len() <= xml.len(),
            "{} bytes grew to {}",
            xml.len(),
            reduced.len()
        );
        assert_eq!(reduce(&reduced).expect("reduced again"), reduced);
        let (mut whole, mut cut) = (parse(&xml).expect("read"), parse(&reduced).expect("read"));
        whole.digest.clear();
        cut.digest.clear();
        assert_eq!(cut.context.user_data, whole.context.user_data);
        assert_eq!(cut, whole);
        let mut proposal = args(&xml);
        proposal["source"] = json!(reduced);
        assert!(
            plan_of(&proposal).is_ok(),
            "the reduced description still makes a plan"
        );
    }
    for xml in [BIOS, UEFI, CLONE, NO_NIC] {
        assert!(reduce(xml).expect("reduced").len() <= xml.len());
    }
}

#[test]
fn the_shown_description_leaves_out_the_user_data_and_nothing_else() {
    for xml in [BIOS, UEFI] {
        let kept = reduce(xml).expect("reduced");
        let shown = reduce_shown(xml).expect("shown");
        let user_data = parse(xml)
            .expect("read")
            .context
            .user_data
            .expect("the fixture's user data");
        assert!(
            !shown.contains(user_data.lines().last().expect("a line")),
            "{shown}"
        );
        assert!(shown.contains("<USER_DATA/>") && !shown.contains("I2Nsb3Vk"));
        let (mut from_kept, mut from_shown) =
            (parse(&kept).expect("read"), parse(&shown).expect("read"));
        assert!(from_shown.context.user_data.is_none());
        from_kept.context.user_data = None;
        from_kept.digest.clear();
        from_shown.digest.clear();
        assert_eq!(from_shown, from_kept);
    }
}

#[test]
fn the_user_data_spelled_userdata_is_kept_and_never_shown() {
    let xml =
        with_user_data("<USERDATA>#cloud-config\nruncmd: [userdata-spelling-secret]</USERDATA>");
    let kept = reduce(&xml).expect("reduced");
    let shown = reduce_shown(&xml).expect("shown");
    assert!(kept.contains("userdata-spelling-secret"), "{kept}");
    assert!(
        parse(&kept)
            .expect("read")
            .context
            .user_data
            .is_some_and(|data| data.contains("userdata-spelling-secret"))
    );
    assert!(!shown.contains("userdata-spelling-secret"), "{shown}");
    assert!(shown.contains("<USERDATA/>"), "{shown}");
    assert!(parse(&shown).expect("read").context.user_data.is_none());
}

/// The user data element of `reduce`'s text for a description whose user data is written `source`.
fn reduced_user_data(source: &str) -> String {
    let reduced =
        reduce(&with_user_data(&format!("<USER_DATA>{source}</USER_DATA>"))).expect("reduced");
    let start = reduced.find("<USER_DATA>").expect("the user data") + "<USER_DATA>".len();
    let end = reduced.find("</USER_DATA>").expect("its end");
    reduced[start..end].to_string()
}

#[test]
fn each_text_is_written_in_its_shortest_well_formed_form() {
    // A `>` after `]]` is a reference outside a section, or the text would end a section that is not open.
    assert_eq!(reduced_user_data("a]]&gt;b"), "a]]&gt;b");
    assert_eq!(
        reduced_user_data("<![CDATA[a]]]]><![CDATA[>b]]>"),
        "a]]&gt;b"
    );
    // A carriage return is a reference anywhere: a literal one reads back as a line feed.
    assert_eq!(reduced_user_data("a&#13;]]&gt;b"), "a&#13;]]&gt;b");
    // Two references are shorter than a section; twelve are longer than one.
    assert_eq!(reduced_user_data("a<![CDATA[&&]]>b"), "a&amp;&amp;b");
    assert_eq!(
        reduced_user_data(&"&amp;".repeat(12)),
        format!("<![CDATA[{}]]>", "&".repeat(12))
    );
    // Three at the end cost the same either way, counting the section's close; the plain form is kept.
    assert_eq!(reduced_user_data("a&amp;&amp;&amp;"), "a&amp;&amp;&amp;");
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
    /// The Ceph cluster of the Incus pool; `ceph` when `None`.
    cluster: Option<String>,
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
                cluster: self.cluster.clone().unwrap_or_else(|| "ceph".to_string()),
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

/// `rbd` as a recording: every command the copy runs, in order. `status` answers no watcher, `info` the size `sizes`
/// gives the image, and the pipe into `fail_into` fails.
#[derive(Default)]
struct RecordedRbd {
    sizes: BTreeMap<String, u64>,
    fail_into: Option<String>,
    calls: Mutex<Vec<Vec<String>>>,
}

impl RecordedRbd {
    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().expect("calls").clone()
    }
}

impl Runner for RecordedRbd {
    fn output(&self, argv: &[String]) -> Result<String, String> {
        self.calls.lock().expect("calls").push(argv.to_vec());
        match argv.get(6).map(String::as_str) {
            Some("status") => Ok(r#"{"watchers":[]}"#.to_string()),
            Some("info") => {
                let spec = argv.last().expect("a spec");
                let size = self.sizes.get(spec).ok_or("no such image")?;
                Ok(format!(r#"{{"size":{size}}}"#))
            }
            _ => Err(format!("unexpected rbd command {argv:?}")),
        }
    }

    fn pipe(&self, first: &[String], second: &[String]) -> Result<(), String> {
        let mut calls = self.calls.lock().expect("calls");
        calls.push(first.to_vec());
        calls.push(second.to_vec());
        match &self.fail_into {
            Some(spec) if second.last() == Some(spec) => Err("rbd import-diff failed".to_string()),
            _ => Ok(()),
        }
    }
}

fn copy_calls(copy: &RbdCopy<RecordedRbd>) -> Vec<Vec<String>> {
    copy.runner().calls()
}

/// The `rbd` verbs that only read an image: who watches it, its size, and its data to standard output.
const READ_VERBS: [&str; 3] = ["status", "info", "export-diff"];

/// Checks every recorded command: each names a cluster and a client; every command on the source cluster, or as the
/// source client, is a read of a source image to standard output; every write is `import-diff` on the target cluster
/// as Incus's client, into the target pool. Answers how many commands read the source.
fn source_only_read(calls: &[Vec<String>], source: (&str, &str), target: (&str, &str)) -> usize {
    let mut reads = 0;
    for argv in calls {
        assert_eq!(argv[0], "rbd", "{argv:?}");
        assert_eq!(
            (argv[1].as_str(), argv[3].as_str(), argv[5].as_str()),
            ("--cluster", "--id", "--no-progress"),
            "{argv:?}"
        );
        let (cluster, client, verb) = (argv[2].as_str(), argv[4].as_str(), argv[6].as_str());
        if cluster == source.0 || client == source.1 {
            assert_eq!(
                (cluster, client),
                source,
                "the source client stays on its cluster: {argv:?}"
            );
            assert!(
                READ_VERBS.contains(&verb),
                "the source is only read: {argv:?}"
            );
            // `export-diff <image> -` names the image first; `status` and `info` name it last.
            let image = if verb == "export-diff" {
                &argv[7]
            } else {
                argv.last().expect("an image")
            };
            assert!(
                image.starts_with("src-pool/"),
                "a read of a source image: {argv:?}"
            );
            if verb == "export-diff" {
                assert_eq!(argv.len(), 9, "{argv:?}");
                assert_eq!(argv[8], "-", "the data goes to the pipe: {argv:?}");
            }
            reads += 1;
        } else {
            assert_eq!((cluster, client), target, "{argv:?}");
            assert!(["info", "import-diff"].contains(&verb), "{argv:?}");
            assert!(
                argv.last().expect("a spec").starts_with("remote-osd/"),
                "{argv:?}"
            );
        }
    }
    reads
}

/// The UEFI import from the source cluster `from` into an Incus pool on the cluster `to`, with `rbd` recorded.
fn import_between(from: &str, to: &str) -> (Value, Vec<Vec<String>>) {
    let plan = plan(UEFI);
    let mut rbd = RecordedRbd::default();
    rbd.sizes.insert(
        "remote-osd/virtual-machine_people_web-1.block".to_string(),
        plan.copies[0].source.size_mib * 1024 * 1024,
    );
    for (name, size) in plan.volumes() {
        rbd.sizes.insert(
            format!("remote-osd/custom_people_{name}.block"),
            size * 1024 * 1024,
        );
    }
    let incus = FakeImport {
        managed: true,
        cluster: Some(to.to_string()),
        ..FakeImport::default()
    };
    let copy = RbdCopy::new(from, "arca-import", rbd);
    let answer =
        run(&plan, "src-pool", &incus, &copy, &probe(Some(1))).expect("imported across clusters");
    (answer, copy_calls(&copy))
}

#[test]
fn across_two_ceph_clusters_the_source_cluster_is_only_ever_read() {
    let (answer, calls) = import_between("source-b", "ceph");
    // Two disks: a status and an export-diff of each on the source; an info and an import-diff of each on the target.
    assert_eq!(calls.len(), 8);
    assert_eq!(
        source_only_read(&calls, ("source-b", "arca-import"), ("ceph", "incus")),
        4
    );
    let writes: Vec<&Vec<String>> = calls
        .iter()
        .filter(|argv| argv[6] == "import-diff")
        .collect();
    assert_eq!(writes.len(), 2);
    assert_eq!(answer["ceph"]["from"], "source-b");
    assert_eq!(answer["ceph"]["to"], "ceph");
    assert_eq!(
        answer["ceph"]["copied"],
        "Copied from the Ceph cluster source-b (the pool src-pool, read only) to the Ceph cluster ceph, into the Incus pool remote (written as client.incus)"
    );
    assert_eq!(answer["note"], "The VM runs and read back.");
}

#[test]
fn the_import_runs_the_same_in_either_direction_between_two_clusters() {
    // Neither cluster is special: the node's config names the source's, Incus names the target pool's.
    let (there, there_calls) = import_between("cluster-a", "cluster-b");
    let (back, back_calls) = import_between("cluster-b", "cluster-a");
    assert_eq!(
        source_only_read(
            &there_calls,
            ("cluster-a", "arca-import"),
            ("cluster-b", "incus")
        ),
        4
    );
    assert_eq!(
        source_only_read(
            &back_calls,
            ("cluster-b", "arca-import"),
            ("cluster-a", "incus")
        ),
        4
    );
    assert_eq!(
        (there["ceph"]["from"].as_str(), there["ceph"]["to"].as_str()),
        (Some("cluster-a"), Some("cluster-b"))
    );
    assert_eq!(
        (back["ceph"]["from"].as_str(), back["ceph"]["to"].as_str()),
        (Some("cluster-b"), Some("cluster-a"))
    );
    // The same commands, with the two names swapped.
    let swapped: Vec<Vec<String>> = there_calls
        .iter()
        .map(|argv| {
            argv.iter()
                .map(|part| match part.as_str() {
                    "cluster-a" => "cluster-b".to_string(),
                    "cluster-b" => "cluster-a".to_string(),
                    other => other.to_string(),
                })
                .collect()
        })
        .collect();
    assert_eq!(swapped, back_calls);
}

#[test]
fn a_failed_copy_across_two_clusters_still_only_reads_the_source() {
    let plan = plan(UEFI);
    let mut rbd = RecordedRbd::default();
    rbd.sizes.insert(
        "remote-osd/virtual-machine_people_web-1.block".to_string(),
        plan.copies[0].source.size_mib * 1024 * 1024,
    );
    for (name, size) in plan.volumes() {
        rbd.sizes.insert(
            format!("remote-osd/custom_people_{name}.block"),
            size * 1024 * 1024,
        );
    }
    rbd.fail_into = Some("remote-osd/custom_people_web-1-disk1.block".to_string());
    let incus = FakeImport {
        managed: true,
        ..FakeImport::default()
    };
    let copy = RbdCopy::new("source-b", "arca-import", rbd);
    let failure =
        run(&plan, "src-pool", &incus, &copy, &probe(Some(1))).expect_err("the copy fails");
    assert_eq!(failure.step, "Copying a disk");
    assert!(failure.words().ends_with("The source was not written."));
    let calls = copy_calls(&copy);
    assert_eq!(
        source_only_read(&calls, ("source-b", "arca-import"), ("ceph", "incus")),
        4
    );
    // The undo goes through Incus alone: no rbd command runs after the failed copy.
    assert_eq!(calls.last().expect("a call")[6], "import-diff");
    assert_eq!(
        incus.calls.borrow()[3..],
        [
            "remove instance people/web-1",
            "remove volume people/remote/web-1-disk1",
            "remove acl default/arca-import-web-1",
        ]
    );
}

#[test]
fn on_one_cluster_the_answer_says_so() {
    let plan = plan(BIOS);
    let copy = copy_for(&plan);
    let answer = run(
        &plan,
        "src-pool",
        &FakeImport::default(),
        &copy,
        &probe(Some(1)),
    )
    .expect("imported");
    assert_eq!(answer["ceph"]["from"], "ceph");
    assert_eq!(answer["ceph"]["to"], "ceph");
    assert_eq!(
        answer["ceph"]["copied"],
        "Copied from the Ceph cluster ceph (the pool src-pool, read only) to the same cluster, into the Incus pool remote (written as client.incus)"
    );
}

#[test]
fn a_source_on_another_cluster_is_copied_and_not_refused() {
    let plan = plan(BIOS);
    let copy = FakeCopy::on_cluster("source-b");
    copy.target(
        "remote-osd/virtual-machine_people_web-1.block",
        plan.copies[0].source.size_mib * 1024 * 1024,
    );
    let answer = run(
        &plan,
        "src-pool",
        &FakeImport::default(),
        &copy,
        &probe(Some(1)),
    )
    .expect("imported");
    assert_eq!(copy.copied().len(), 1);
    assert_eq!(answer["ceph"]["from"], "source-b");
}
