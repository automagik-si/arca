//! `instance_move_preflight` against two fake Incus daemons: this node (the source) and a linked host (the target), each
//! on its own socket. The verb only reads: every test that calls it ends with neither fake holding a write. Nothing here
//! touches a live host. Every name and address is made up: node-a and node-b, documentation ranges only.

use arca::{config::NodeConfig, move_preflight::MovePreflightHandler};
use arca_api::Scope;
use arca_incus::{FakeIncus, FakeState};
use arca_mcp::{Caller, ToolHandler};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    net::IpAddr,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

static SEQ: AtomicU64 = AtomicU64::new(0);
const MIB: u64 = 1024 * 1024;
/// The CPU features of an x86-64-v3 host, the source's and, unless a test changes it, the target's.
const V3: &[&str] = &[
    "fpu", "sse", "sse2", "cx16", "lahf_lm", "popcnt", "sse4_1", "sse4_2", "ssse3", "avx", "avx2",
    "bmi1", "bmi2", "f16c", "fma", "abm", "movbe", "xsave",
];
/// The CPU features of an x86-64-v2 host: the v3 ones left out.
const V2: &[&str] = &[
    "fpu", "sse", "sse2", "cx16", "lahf_lm", "popcnt", "sse4_1", "sse4_2", "ssse3",
];

/// One fake host: what its Incus serves.
struct Host {
    projects: Vec<Value>,
    instances: Vec<Value>,
    networks: Vec<Value>,
    /// Each pool: its name, its driver, its used and total bytes.
    pools: Vec<(&'static str, &'static str, u64, u64)>,
    flags: Vec<&'static str>,
    cards: Vec<Value>,
    interfaces: Vec<(&'static str, Vec<&'static str>)>,
    profiles: Vec<(&'static str, Value)>,
    acls: Vec<(&'static str, Value)>,
    /// Each lease: its network, the MAC holding it and its address.
    leases: Vec<(&'static str, &'static str, &'static str)>,
}

fn card() -> Value {
    json!({
        "pci_address": "0000:01:00.0", "vendor_id": "10de", "product_id": "2204",
        "drm": { "id": 0, "card_name": "card0", "render_name": "renderD128" }
    })
}

fn acl() -> Value {
    json!({
        "name": "web-in", "description": "",
        "ingress": [{ "action": "allow", "protocol": "tcp", "destination_port": "80", "state": "enabled" }],
        "egress": [], "config": {}
    })
}

fn bridge(v4: &str) -> Value {
    json!({
        "name": "arcabr0", "project": "default", "type": "bridge", "managed": true,
        "config": { "ipv4.address": v4, "ipv6.address": "2001:db8:1::1/64" }
    })
}

/// The instance `name` in the project `shop`: a container on pool `default` with a fixed address on `arcabr0` behind the
/// ACL `web-in`, the GPU and its render node, a proxy on every address, and a macvlan card on `enp6s0`.
fn instance(name: &str, mac: &str) -> Value {
    json!({
        "name": name, "project": "shop", "type": "container", "status": "Running", "architecture": "x86_64",
        "profiles": ["default"],
        "config": { "volatile.eth0.hwaddr": mac },
        "devices": {
            "root": { "type": "disk", "path": "/", "pool": "default" },
            "eth0": { "type": "nic", "network": "arcabr0", "ipv4.address": "192.0.2.50", "security.acls": "web-in" },
            "gpu0": { "type": "gpu", "pci": "0000:01:00.0" },
            "render": { "type": "unix-char", "source": "/dev/dri/renderD128", "path": "/dev/dri/renderD128" },
            "http": { "type": "proxy", "listen": "tcp:0.0.0.0:8080", "connect": "tcp:127.0.0.1:80" },
            "lan": { "type": "nic", "nictype": "macvlan", "parent": "enp6s0" }
        }
    })
}

impl Host {
    /// A host that holds what [`instance`] needs, and the instance `web` itself when `with_web`.
    fn new(with_web: bool, address: &'static str) -> Host {
        Host {
            projects: vec![
                json!({ "name": "default", "config": {} }),
                json!({ "name": "shop", "config": { "features.profiles": "true", "features.networks": "false" } }),
            ],
            instances: if with_web {
                vec![instance("web", "00:16:3e:00:00:01")]
            } else {
                Vec::new()
            },
            networks: vec![bridge("192.0.2.1/24")],
            pools: vec![("default", "btrfs", 0, 100 * MIB)],
            flags: V3.to_vec(),
            cards: vec![card()],
            interfaces: vec![("enp6s0", vec![address])],
            profiles: vec![(
                "shop",
                json!({ "name": "default", "description": "", "config": {}, "devices": {} }),
            )],
            acls: vec![("default", acl())],
            leases: Vec::new(),
        }
    }

    fn start(self, dir: &Path) -> FakeIncus {
        fs::create_dir_all(dir).expect("dir");
        let state = FakeState {
            server: json!({
                "api_version": "1.0",
                "environment": { "architectures": ["x86_64", "i686"], "driver": "lxc | qemu", "server_name": "fake" }
            }),
            projects: self.projects,
            instances: self.instances,
            pools: self
                .pools
                .iter()
                .map(|(name, _, used, total)| {
                    (
                        name.to_string(),
                        json!({ "space": { "used": used, "total": total } }),
                    )
                })
                .collect(),
            networks: self.networks,
        };
        let fake = FakeIncus::start(dir, state).expect("fake incus");
        for (name, driver, _, _) in &self.pools {
            fake.set_pool_driver(name, driver);
        }
        fake.set_resources(json!({
            "cpu": { "architecture": "x86_64", "sockets": [{ "socket": 0, "cores": [
                { "core": 0, "flags": self.flags }, { "core": 1, "flags": self.flags }
            ]}]},
            "gpu": { "cards": self.cards },
            "usb": { "devices": [] },
            "pci": { "devices": [] }
        }));
        fake.set_host_interfaces(
            self.interfaces
                .iter()
                .map(|(name, addresses)| {
                    (
                        name.to_string(),
                        addresses
                            .iter()
                            .map(|a| a.parse::<IpAddr>().expect("ip"))
                            .collect(),
                    )
                })
                .collect(),
        );
        for (project, body) in self.profiles {
            fake.add_profile(project, body);
        }
        for (project, body) in self.acls {
            fake.add_acl(project, body);
        }
        for (network, mac, address) in self.leases {
            fake.set_lease(network, mac, address);
        }
        fake
    }
}

struct Rig {
    dir: PathBuf,
    source: FakeIncus,
    target: Option<FakeIncus>,
    handler: MovePreflightHandler,
}

impl Rig {
    /// The source `source`, with the linked host node-b reading the target `target` through its Incus socket.
    fn new(source: Host, target: Host) -> Rig {
        Self::with(source, Some(target), true)
    }

    fn with(source: Host, target: Option<Host>, incus_socket: bool) -> Rig {
        let dir = std::env::temp_dir().join(format!(
            "arca-move-preflight-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("state")).expect("state dir");
        fs::write(dir.join("state").join("journal"), b"one record\n").expect("journal");
        let source = source.start(&dir.join("source"));
        let target_socket = dir.join("target").join("incus.sock");
        let target = target.map(|host| host.start(&dir.join("target")));
        let mut linked = json!({
            "name": "node-b",
            "socket": dir.join("node-b-arca.sock"),
            "tokenFile": dir.join("node-b.token"),
        });
        if incus_socket {
            linked["incusSocket"] = json!(target_socket);
        }
        let cfg: NodeConfig = serde_json::from_value(json!({
            "stateDir": dir.join("state"),
            "incus": { "socket": source.socket() },
            "listen": { "unix": dir.join("arca.sock") },
            "tokens": { "hashDir": dir.join("tokens") },
            "kernel": { "projects": [], "droiddProjects": [] },
            "approval": { "linkedHosts": [linked] }
        }))
        .expect("config");
        let handler =
            MovePreflightHandler::new(Arc::new(cfg)).with_storage_root(dir.join("source"));
        Rig {
            dir,
            source,
            target,
            handler,
        }
    }

    /// Writes `mib` MiB of files into the storage folder of the container `name` of `shop` on pool `default`.
    fn files(&self, name: &str, mib: u64) {
        let root = self
            .dir
            .join("source/storage-pools/default/containers")
            .join(format!("shop_{name}"))
            .join("rootfs");
        fs::create_dir_all(root.join("srv")).expect("rootfs");
        for index in 0..mib {
            fs::write(
                root.join("srv").join(format!("blob-{index}")),
                vec![7u8; MIB as usize],
            )
            .expect("blob");
        }
    }

    fn call(&self, args: Value) -> Value {
        self.handler
            .call(
                args,
                &Caller {
                    token_id: "viewer".into(),
                    scope: Scope::Viewer,
                    tools: None,
                },
            )
            .expect("the preflight answers")
    }

    fn preflight(&self, names: &[&str]) -> Value {
        let instances: Vec<Value> = names
            .iter()
            .map(|name| json!({ "project": "shop", "name": name }))
            .collect();
        self.call(json!({ "instances": instances, "target": "node-b" }))
    }

    /// Neither fake was sent anything but a GET.
    fn assert_only_reads(&self) {
        assert_eq!(self.source.writes(), Vec::new(), "the source saw no write");
        assert!(self.source.requests().iter().all(|r| r.method == "GET"));
        if let Some(target) = &self.target {
            assert_eq!(target.writes(), Vec::new(), "the target saw no write");
            assert!(target.requests().iter().all(|r| r.method == "GET"));
        }
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn check<'a>(row: &'a Value, name: &str) -> &'a Value {
    row["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .find(|check| check["check"] == name)
        .unwrap_or_else(|| panic!("the row has the check {name}: {row:#}"))
}

fn finding<'a>(check: &'a Value, subject: &str) -> &'a Value {
    check["findings"]
        .as_array()
        .expect("findings")
        .iter()
        .find(|finding| finding["subject"] == subject)
        .unwrap_or_else(|| panic!("the check has a finding on {subject}: {check:#}"))
}

const CHECKS: [&str; 8] = [
    "size",
    "storageDriver",
    "cpu",
    "devices",
    "networks",
    "projectsProfilesAcls",
    "proxyListen",
    "macvlanParents",
];

fn row_checks(row: &Value) -> Vec<String> {
    row["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .map(|check| check["check"].as_str().expect("name").to_string())
        .collect()
}

#[test]
fn a_ready_instance_on_a_target_with_room_is_go_with_every_check_ok() {
    let rig = Rig::new(
        Host::new(true, "203.0.113.10"),
        Host::new(false, "203.0.113.20"),
    );
    rig.files("web", 4);
    let answer = rig.preflight(&["web"]);
    assert_eq!(answer["target"]["name"], "node-b");
    assert_eq!(answer["target"]["reachable"], true);
    let rows = answer["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(
        (row["project"].as_str(), row["name"].as_str()),
        (Some("shop"), Some("web"))
    );
    assert_eq!(row["target"], "node-b");
    assert_eq!(row_checks(row), CHECKS);
    for name in CHECKS {
        assert_eq!(
            check(row, name)["ok"],
            true,
            "{name} is ok: {:#}",
            check(row, name)
        );
    }
    assert_eq!(row["go"], true, "{row:#}");
    let size = check(row, "size");
    let measured = size["measure"]["measuredBytes"].as_u64().expect("measured");
    assert!(measured >= 4 * MIB, "the walk counts the files: {measured}");
    assert_eq!(size["measure"]["method"], "walked");
    assert_eq!(size["measure"]["neededBytes"], measured + measured / 10);
    assert_eq!(size["measure"]["targetFreeBytes"], 100 * MIB);
    assert_eq!(size["measure"]["pendingBytes"], 0);
    assert_eq!(check(row, "storageDriver")["crossesDriver"], false);
    rig.assert_only_reads();
}

#[test]
fn a_size_that_does_not_fit_beside_a_pending_move_to_the_same_target_is_no_go_with_the_arithmetic()
{
    let mut source = Host::new(true, "203.0.113.10");
    source
        .instances
        .insert(0, instance("api", "00:16:3e:00:00:02"));
    let mut target = Host::new(false, "203.0.113.20");
    target.pools = vec![("default", "btrfs", 92 * MIB, 100 * MIB)];
    let rig = Rig::new(source, target);
    rig.files("api", 4);
    rig.files("web", 4);
    // Alone, web fits in the 8 MiB the target has free.
    let alone = rig.preflight(&["web"]);
    assert_eq!(check(&alone["rows"][0], "size")["ok"], true, "{alone:#}");
    // After api, which is already destined for the same target, it does not.
    let answer = rig.preflight(&["api", "web"]);
    let rows = answer["rows"].as_array().expect("rows");
    assert_eq!(rows[0]["name"], "api", "rows come in input order");
    assert_eq!(rows[1]["name"], "web");
    let first = check(&rows[0], "size");
    assert_eq!(first["ok"], true, "{first:#}");
    let size = check(&rows[1], "size");
    assert_eq!(size["ok"], false, "{size:#}");
    assert_eq!(rows[1]["go"], false);
    let measure = &size["measure"];
    assert_eq!(measure["targetFreeBytes"], 8 * MIB);
    assert_eq!(measure["pendingBytes"], first["measure"]["neededBytes"]);
    let available = measure["availableBytes"].as_u64().expect("available");
    assert_eq!(
        available,
        8 * MIB - first["measure"]["neededBytes"].as_u64().expect("needed")
    );
    assert!(measure["neededBytes"].as_u64().expect("needed") > available);
    let detail = size["detail"].as_str().expect("detail");
    for words in ["free", "pending", "shop/api", "10%", "does not fit"] {
        assert!(detail.contains(words), "the detail says {words}: {detail}");
    }
    rig.assert_only_reads();
}

#[test]
fn a_move_across_storage_drivers_is_said_in_the_row() {
    let mut target = Host::new(false, "203.0.113.20");
    target.pools = vec![("default", "zfs", 0, 100 * MIB)];
    let rig = Rig::new(Host::new(true, "203.0.113.10"), target);
    rig.files("web", 1);
    let answer = rig.preflight(&["web"]);
    let driver = check(&answer["rows"][0], "storageDriver");
    assert_eq!(driver["sourceDriver"], "btrfs");
    assert_eq!(driver["targetDriver"], "zfs");
    assert_eq!(driver["crossesDriver"], true);
    let said = format!("{} {}", driver["detail"], driver["risks"]);
    assert!(said.contains("btrfs") && said.contains("zfs"), "{driver:#}");
    rig.assert_only_reads();
}

#[test]
fn a_live_move_to_a_cpu_without_the_source_features_is_no_go_naming_them_and_the_baseline_risk() {
    let mut target = Host::new(false, "203.0.113.20");
    target.flags = V2.to_vec();
    let rig = Rig::new(Host::new(true, "203.0.113.10"), target);
    rig.files("web", 1);
    let live = rig.call(json!({
        "instances": [{ "project": "shop", "name": "web" }], "target": "node-b", "live": true
    }));
    let row = &live["rows"][0];
    let cpu = check(row, "cpu");
    assert_eq!(cpu["ok"], false, "{cpu:#}");
    assert_eq!(row["go"], false);
    let missing: Vec<&str> = cpu["missingFeatures"]
        .as_array()
        .expect("missing")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    for flag in ["avx", "avx2", "bmi2", "fma"] {
        assert!(missing.contains(&flag), "{flag} is named: {missing:?}");
    }
    assert!(cpu["detail"].as_str().expect("detail").contains("avx2"));
    assert_eq!(cpu["sourceBaseline"], "x86-64-v3");
    assert_eq!(cpu["targetBaseline"], "x86-64-v2");
    let risks = cpu["risks"].to_string();
    assert!(
        risks.contains("x86-64-v3") && risks.contains("x86-64-v2"),
        "{risks}"
    );
    // A cold move is not stopped by the CPU alone: the risk stays.
    let cold = rig.preflight(&["web"]);
    let cpu = check(&cold["rows"][0], "cpu");
    assert_eq!(cpu["ok"], true, "{cpu:#}");
    assert!(cpu["risks"].to_string().contains("x86-64-v3"));
    rig.assert_only_reads();
}

#[test]
fn devices_the_target_lacks_are_no_go_each_and_a_host_path_disk_proposes_a_custom_volume() {
    let mut source = Host::new(true, "203.0.113.10");
    let devices = &mut source.instances[0]["devices"];
    devices["serial"] =
        json!({ "type": "unix-char", "source": "/dev/ttyUSB0", "path": "/dev/ttyUSB0" });
    devices["data"] = json!({ "type": "disk", "source": "/srv/web-data", "path": "/data" });
    let mut target = Host::new(false, "203.0.113.20");
    target.cards = Vec::new();
    let rig = Rig::new(source, target);
    rig.files("web", 1);
    let answer = rig.preflight(&["web"]);
    let row = &answer["rows"][0];
    let devices = check(row, "devices");
    assert_eq!(devices["ok"], false, "{devices:#}");
    assert_eq!(row["go"], false);
    for subject in ["gpu0", "render", "serial", "data"] {
        assert_eq!(
            finding(devices, subject)["ok"],
            false,
            "{subject}: {devices:#}"
        );
    }
    assert!(
        finding(devices, "render")["detail"]
            .as_str()
            .expect("detail")
            .contains("renderD128")
    );
    let data = finding(devices, "data");
    let proposal = data["proposal"].as_str().expect("a proposal");
    assert!(proposal.contains("custom volume"), "{proposal}");
    assert!(proposal.contains("/srv/web-data"), "{proposal}");
    assert!(proposal.contains("/data"), "{proposal}");
    rig.assert_only_reads();
}

#[test]
fn a_network_the_target_lacks_or_a_fixed_address_it_cannot_take_is_no_go_proposing_the_same_subnet()
{
    // The target has no arcabr0 at all.
    let mut target = Host::new(false, "203.0.113.20");
    target.networks = Vec::new();
    let rig = Rig::new(Host::new(true, "203.0.113.10"), target);
    rig.files("web", 1);
    let answer = rig.preflight(&["web"]);
    let networks = check(&answer["rows"][0], "networks");
    assert_eq!(networks["ok"], false, "{networks:#}");
    let eth0 = finding(networks, "eth0");
    assert!(eth0["detail"].as_str().expect("detail").contains("arcabr0"));
    assert!(
        eth0["proposal"]
            .as_str()
            .expect("proposal")
            .contains("192.0.2.1/24"),
        "{eth0:#}"
    );
    rig.assert_only_reads();
    drop(rig);

    // The target's arcabr0 is another subnet, so the fixed address does not belong there.
    let mut target = Host::new(false, "203.0.113.20");
    target.networks = vec![bridge("198.51.100.1/24")];
    let rig = Rig::new(Host::new(true, "203.0.113.10"), target);
    rig.files("web", 1);
    let answer = rig.preflight(&["web"]);
    let networks = check(&answer["rows"][0], "networks");
    assert_eq!(networks["ok"], false, "{networks:#}");
    let eth0 = finding(networks, "eth0");
    let detail = eth0["detail"].as_str().expect("detail");
    assert!(
        detail.contains("192.0.2.50") && detail.contains("198.51.100.1/24"),
        "{detail}"
    );
    assert!(
        eth0["proposal"]
            .as_str()
            .expect("proposal")
            .contains("192.0.2.1/24")
    );
    assert_eq!(answer["rows"][0]["go"], false);
    rig.assert_only_reads();
    drop(rig);

    // The same subnet, but another card already holds the fixed address there.
    let mut target = Host::new(false, "203.0.113.20");
    target.leases = vec![("arcabr0", "00:16:3e:99:99:99", "192.0.2.50")];
    let rig = Rig::new(Host::new(true, "203.0.113.10"), target);
    rig.files("web", 1);
    let answer = rig.preflight(&["web"]);
    let networks = check(&answer["rows"][0], "networks");
    assert_eq!(networks["ok"], false, "{networks:#}");
    let detail = finding(networks, "eth0")["detail"]
        .as_str()
        .expect("detail")
        .to_string();
    assert!(
        detail.contains("192.0.2.50") && detail.contains("00:16:3e:99:99:99"),
        "{detail}"
    );
    rig.assert_only_reads();
}

#[test]
fn a_missing_project_profile_or_acl_and_a_differing_profile_are_no_go_naming_each() {
    let mut source = Host::new(true, "203.0.113.10");
    source.instances[0]["profiles"] = json!(["default", "gpu-host"]);
    source.profiles.push((
        "shop",
        json!({ "name": "gpu-host", "description": "", "config": {}, "devices": {} }),
    ));
    let mut target = Host::new(false, "203.0.113.20");
    target.acls = Vec::new();
    target.profiles = vec![(
        "shop",
        json!({ "name": "default", "description": "", "config": { "limits.cpu": "2" }, "devices": {} }),
    )];
    let rig = Rig::new(source, target);
    rig.files("web", 1);
    let answer = rig.preflight(&["web"]);
    let check6 = check(&answer["rows"][0], "projectsProfilesAcls");
    assert_eq!(check6["ok"], false, "{check6:#}");
    assert_eq!(finding(check6, "project shop")["ok"], true);
    assert_eq!(finding(check6, "profile gpu-host")["ok"], false);
    let default = finding(check6, "profile default");
    assert_eq!(default["ok"], false);
    assert!(
        default["detail"]
            .as_str()
            .expect("detail")
            .contains("limits.cpu"),
        "{default:#}"
    );
    assert_eq!(finding(check6, "acl web-in")["ok"], false);
    rig.assert_only_reads();
    drop(rig);

    // The project itself is missing on the target.
    let mut target = Host::new(false, "203.0.113.20");
    target.projects = vec![json!({ "name": "default", "config": {} })];
    target.profiles = Vec::new();
    let rig = Rig::new(Host::new(true, "203.0.113.10"), target);
    rig.files("web", 1);
    let answer = rig.preflight(&["web"]);
    let check6 = check(&answer["rows"][0], "projectsProfilesAcls");
    assert_eq!(check6["ok"], false);
    let project = finding(check6, "project shop");
    assert_eq!(project["ok"], false);
    assert!(
        project["proposal"]
            .as_str()
            .expect("proposal")
            .contains("features.networks")
    );
    rig.assert_only_reads();
}

#[test]
fn a_proxy_on_the_source_address_and_a_macvlan_parent_the_target_lacks_are_no_go_naming_them() {
    let mut source = Host::new(true, "203.0.113.10");
    source.instances[0]["devices"]["https"] = json!({ "type": "proxy", "listen": "tcp:203.0.113.10:443", "connect": "tcp:127.0.0.1:443" });
    let mut target = Host::new(false, "203.0.113.20");
    target.interfaces = vec![("br-lan", vec!["203.0.113.20"]), ("_p0a1b2c3d4e5f", vec![])];
    let rig = Rig::new(source, target);
    rig.files("web", 1);
    let answer = rig.preflight(&["web"]);
    let row = &answer["rows"][0];
    assert_eq!(row["go"], false);
    let proxy = check(row, "proxyListen");
    assert_eq!(proxy["ok"], false, "{proxy:#}");
    let https = finding(proxy, "https");
    assert_eq!(https["ok"], false);
    let detail = https["detail"].as_str().expect("detail");
    assert!(
        detail.contains("203.0.113.10") && detail.contains("443"),
        "{detail}"
    );
    assert!(
        https["proposal"]
            .as_str()
            .expect("proposal")
            .contains("203.0.113.20")
    );
    assert_eq!(
        finding(proxy, "http")["ok"],
        true,
        "a proxy on every address moves as it is"
    );
    let macvlan = check(row, "macvlanParents");
    assert_eq!(macvlan["ok"], false, "{macvlan:#}");
    let lan = finding(macvlan, "lan");
    let detail = lan["detail"].as_str().expect("detail");
    assert!(
        detail.contains("enp6s0") && detail.contains("br-lan"),
        "{detail}"
    );
    rig.assert_only_reads();
}

/// Every file under `dir`, with its bytes and its modification time.
fn tree(dir: &Path) -> BTreeMap<PathBuf, (Vec<u8>, std::time::SystemTime)> {
    let mut files = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).expect("read dir").flatten() {
            let meta = fs::symlink_metadata(entry.path()).expect("meta");
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                files.insert(
                    entry.path(),
                    (
                        fs::read(entry.path()).expect("read"),
                        meta.modified().expect("mtime"),
                    ),
                );
            }
        }
    }
    files
}

#[test]
fn the_preflight_only_reads_no_write_reaches_either_incus_and_no_state_file_changes() {
    let mut source = Host::new(true, "203.0.113.10");
    source.instances[0]["devices"]["data"] =
        json!({ "type": "disk", "source": "/srv/web-data", "path": "/data" });
    let mut target = Host::new(false, "203.0.113.20");
    target.networks = Vec::new();
    target.cards = Vec::new();
    target.flags = V2.to_vec();
    let rig = Rig::new(source, target);
    rig.files("web", 2);
    let before_state = tree(&rig.dir.join("state"));
    let before_storage = tree(&rig.dir.join("source/storage-pools"));
    for args in [
        json!({ "instances": [{ "project": "shop", "name": "web" }], "target": "node-b" }),
        json!({ "instances": [{ "project": "shop", "name": "web" }, { "project": "shop", "name": "gone" }],
            "target": "node-b", "live": true }),
    ] {
        let answer = rig.call(args);
        assert!(!answer["rows"].as_array().expect("rows").is_empty());
    }
    assert_eq!(
        rig.source.writes(),
        Vec::new(),
        "the source fake's write log is empty"
    );
    let target = rig.target.as_ref().expect("target");
    assert_eq!(
        target.writes(),
        Vec::new(),
        "the target fake's write log is empty"
    );
    let methods: Vec<String> = rig
        .source
        .requests()
        .into_iter()
        .chain(target.requests())
        .map(|request| request.method)
        .collect();
    assert!(!methods.is_empty());
    assert!(methods.iter().all(|method| method == "GET"), "{methods:?}");
    assert_eq!(
        tree(&rig.dir.join("state")),
        before_state,
        "no journal or state file changed"
    );
    assert_eq!(
        tree(&rig.dir.join("source/storage-pools")),
        before_storage,
        "the measured files are as they were"
    );
}

#[test]
fn a_target_that_cannot_be_read_is_a_why_not_row_and_the_verb_still_answers() {
    // The linked host names an Incus socket nobody listens on.
    let rig = Rig::with(Host::new(true, "203.0.113.10"), None, true);
    rig.files("web", 1);
    let answer = rig.preflight(&["web", "gone"]);
    assert_eq!(answer["target"]["reachable"], false);
    let reason = answer["target"]["reason"].as_str().expect("reason");
    assert!(reason.contains("node-b"), "{reason}");
    let rows = answer["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), 2);
    for row in rows {
        assert_eq!(row["go"], false);
        assert_eq!(row_checks(row), CHECKS);
        assert!(
            row["reason"].as_str().is_some_and(|r| !r.is_empty()),
            "{row:#}"
        );
        assert_eq!(check(row, "networks")["ok"], false);
    }
    assert!(rows[1]["reason"].as_str().expect("reason").contains("gone"));
    rig.assert_only_reads();
    drop(rig);

    // A linked host without an Incus socket, and a target no linked host names.
    let rig = Rig::with(Host::new(true, "203.0.113.10"), None, false);
    rig.files("web", 1);
    let answer = rig.preflight(&["web"]);
    assert_eq!(answer["target"]["reachable"], false);
    assert!(
        answer["target"]["reason"]
            .as_str()
            .expect("reason")
            .contains("incusSocket")
    );
    assert_eq!(answer["rows"][0]["go"], false);
    let answer = rig
        .call(json!({ "instances": [{ "project": "shop", "name": "web" }], "target": "node-z" }));
    assert_eq!(answer["target"]["reachable"], false);
    assert!(
        answer["target"]["reason"]
            .as_str()
            .expect("reason")
            .contains("node-z")
    );
    assert_eq!(answer["rows"][0]["go"], false);
    rig.assert_only_reads();
}

#[test]
fn malformed_arguments_are_invalid_not_a_finding() {
    let rig = Rig::new(
        Host::new(true, "203.0.113.10"),
        Host::new(false, "203.0.113.20"),
    );
    for args in [
        json!({ "instances": [], "target": "node-b" }),
        json!({ "instances": [{ "project": "shop" }], "target": "node-b" }),
        json!({ "instances": [{ "project": "shop", "name": "a/b" }], "target": "node-b" }),
        json!({ "instances": [{ "project": "shop", "name": "web" }] }),
        json!({ "instances": [{ "project": "shop", "name": "web" }], "target": "node-b", "extra": 1 }),
    ] {
        let error = rig.handler.call(
            args.clone(),
            &Caller {
                token_id: "viewer".into(),
                scope: Scope::Viewer,
                tools: None,
            },
        );
        assert!(
            matches!(error, Err(arca_mcp::CallError::Invalid(_))),
            "{args} is invalid: {error:?}"
        );
    }
    rig.assert_only_reads();
}

#[test]
fn every_answer_conforms_to_the_verb_tables_result_schema() {
    let verb = arca_api::verb("instance_move_preflight").expect("the verb is in the table");
    let schema = (verb.result)();
    let mut source = Host::new(true, "203.0.113.10");
    source.instances[0]["devices"]["data"] =
        json!({ "type": "disk", "source": "/srv/web-data", "path": "/data" });
    let mut target = Host::new(false, "203.0.113.20");
    target.pools = vec![("default", "zfs", 0, 100 * MIB)];
    let rig = Rig::new(source, target);
    rig.files("web", 1);
    for args in [
        json!({ "instances": [{ "project": "shop", "name": "web" }], "target": "node-b" }),
        json!({ "instances": [{ "project": "shop", "name": "web" }, { "project": "shop", "name": "gone" }],
            "target": "node-b", "live": true }),
        json!({ "instances": [{ "project": "shop", "name": "web" }], "target": "node-z" }),
    ] {
        let answer = rig.call(args);
        assert_eq!(arca_api::conforms(&schema, &answer), Ok(()), "{answer:#}");
        assert!(
            (verb.summary_text)(&answer).contains("node-"),
            "{}",
            (verb.summary_text)(&answer)
        );
    }
    rig.assert_only_reads();
}

impl Rig {
    /// Writes `mib` MiB of files under `relative`, a folder under the source's own folder.
    fn write(&self, relative: &str, mib: u64) {
        write_mib(&self.dir.join("source").join(relative), mib);
    }
}

fn write_mib(folder: &Path, mib: u64) {
    fs::create_dir_all(folder).expect("folder");
    for index in 0..mib {
        fs::write(
            folder.join(format!("blob-{index}")),
            vec![7u8; MIB as usize],
        )
        .expect("blob");
    }
}

#[test]
fn a_zfs_instance_is_never_sized_from_incus_usage_and_an_unmounted_dataset_is_not_measured() {
    // Incus says 1 MiB (the data the dataset holds apart from its image's clone); the files are 4 MiB, and the folder
    // is not where a mounted dataset is.
    let mut source = Host::new(true, "203.0.113.10");
    source.pools = vec![("default", "zfs", 0, 100 * MIB)];
    let mut target = Host::new(false, "203.0.113.20");
    target.pools = vec![("default", "zfs", 92 * MIB, 100 * MIB)];
    let rig = Rig::new(source, target);
    rig.files("web", 4);
    rig.source.set_root_disk_usage("shop", "web", MIB);
    let answer = rig.preflight(&["web"]);
    let row = &answer["rows"][0];
    let size = check(row, "size");
    assert_eq!(
        size["ok"], false,
        "a zfs size is not taken from Incus: {size:#}"
    );
    assert_eq!(row["go"], false);
    assert_eq!(size["measure"]["measuredBytes"], Value::Null, "{size:#}");
    let detail = size["detail"].as_str().expect("detail");
    assert!(
        detail.contains("not measured") && detail.contains("not mounted"),
        "{detail}"
    );
    assert!(
        size["measure"]["notMeasured"]
            .as_str()
            .is_some_and(|why| why.contains("not mounted")),
        "{size:#}"
    );
    rig.assert_only_reads();
    drop(rig);

    // A driver the verb does not know is never sized from Incus either.
    let mut source = Host::new(true, "203.0.113.10");
    source.pools = vec![("default", "fancyfs", 0, 100 * MIB)];
    let rig = Rig::new(source, Host::new(false, "203.0.113.20"));
    rig.files("web", 1);
    rig.source.set_root_disk_usage("shop", "web", MIB);
    let answer = rig.preflight(&["web"]);
    let size = check(&answer["rows"][0], "size");
    assert_eq!(size["ok"], false, "{size:#}");
    let detail = size["detail"].as_str().expect("detail");
    assert!(
        detail.contains("not measured") && detail.contains("fancyfs"),
        "{detail}"
    );
    rig.assert_only_reads();
}

#[test]
fn snapshots_custom_volumes_and_host_folders_count_in_the_size_and_in_the_pending_sum() {
    let host_folder = std::env::temp_dir().join(format!(
        "arca-move-preflight-hostdata-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    write_mib(&host_folder, 1);
    let mut source = Host::new(true, "203.0.113.10");
    source.instances[0]["devices"]["data"] =
        json!({ "type": "disk", "source": host_folder, "path": "/data" });
    source.instances[0]["devices"]["vol"] =
        json!({ "type": "disk", "pool": "default", "source": "vol1", "path": "/vol" });
    source.instances.push(instance("api", "00:16:3e:00:00:02"));
    let rig = Rig::new(source, Host::new(false, "203.0.113.20"));
    rig.files("web", 4);
    rig.files("api", 1);
    rig.write(
        "storage-pools/default/containers-snapshots/shop_web/snap0",
        2,
    );
    // shop does not set features.storage.volumes, so Incus keeps the volume in default: custom/default_vol1.
    rig.write("storage-pools/default/custom/default_vol1", 1);
    let answer = rig.preflight(&["web", "api"]);
    let web = check(&answer["rows"][0], "size");
    let measure = &web["measure"];
    let part = |key: &str| {
        measure[key]
            .as_u64()
            .unwrap_or_else(|| panic!("{key}: {measure:#}"))
    };
    assert!(part("instanceBytes") >= 4 * MIB, "{measure:#}");
    assert!(part("snapshotBytes") >= 2 * MIB, "{measure:#}");
    assert!(part("volumeBytes") >= MIB, "{measure:#}");
    assert!(part("hostFolderBytes") >= MIB, "{measure:#}");
    let measured = part("measuredBytes");
    assert_eq!(
        measured,
        part("instanceBytes")
            + part("snapshotBytes")
            + part("volumeBytes")
            + part("hostFolderBytes")
    );
    assert_eq!(part("neededBytes"), measured + measured / 10);
    assert_eq!(web["ok"], true, "{web:#}");
    let detail = web["detail"].as_str().expect("detail");
    assert!(
        detail.contains("snapshot") && detail.contains("vol1") && detail.contains("/data"),
        "{detail}"
    );
    // The later row counts all of it as pending.
    let api = check(&answer["rows"][1], "size");
    assert_eq!(
        api["measure"]["pendingBytes"],
        part("neededBytes"),
        "{api:#}"
    );
    rig.assert_only_reads();
    let _ = fs::remove_dir_all(&host_folder);
}

#[test]
fn a_storage_folder_the_kernels_user_cannot_list_is_not_measured_with_the_reason() {
    use std::os::unix::fs::PermissionsExt;
    let rig = Rig::new(
        Host::new(true, "203.0.113.10"),
        Host::new(false, "203.0.113.20"),
    );
    rig.files("web", 1);
    let folder = rig
        .dir
        .join("source/storage-pools/default/containers/shop_web");
    fs::set_permissions(&folder, fs::Permissions::from_mode(0o711)).expect("chmod");
    fs::set_permissions(folder.join("rootfs"), fs::Permissions::from_mode(0o000)).expect("chmod");
    let listable = fs::read_dir(folder.join("rootfs")).is_ok();
    let answer = rig.preflight(&["web"]);
    fs::set_permissions(folder.join("rootfs"), fs::Permissions::from_mode(0o755))
        .expect("chmod back");
    if listable {
        eprintln!(
            "skipped: this user lists a mode 0000 folder (root), so the refusal cannot be seen here"
        );
        return;
    }
    let size = check(&answer["rows"][0], "size");
    assert_eq!(size["ok"], false, "{size:#}");
    let detail = size["detail"].as_str().expect("detail");
    assert!(detail.contains("not measured"), "{detail}");
    assert!(
        detail.to_lowercase().contains("permission denied"),
        "{detail}"
    );
    assert!(detail.contains("read access"), "{detail}");
    assert!(
        size["measure"]["notMeasured"].as_str().is_some(),
        "{size:#}"
    );
    rig.assert_only_reads();
}

#[test]
fn an_optional_usb_or_hotplug_device_the_target_lacks_is_a_risk_and_a_required_one_is_no_go() {
    let mut source = Host::new(true, "203.0.113.10");
    let devices = &mut source.instances[0]["devices"];
    devices["stick"] = json!({ "type": "usb", "vendorid": "0403", "productid": "6001" });
    devices["dongle"] = json!({ "type": "unix-hotplug", "vendorid": "0403", "productid": "6001" });
    devices["key"] =
        json!({ "type": "usb", "vendorid": "1050", "productid": "0407", "required": "true" });
    let rig = Rig::new(source, Host::new(false, "203.0.113.20"));
    rig.files("web", 1);
    let answer = rig.preflight(&["web"]);
    let devices = check(&answer["rows"][0], "devices");
    assert_eq!(finding(devices, "stick")["ok"], true, "{devices:#}");
    let dongle = finding(devices, "dongle");
    assert_eq!(dongle["ok"], true, "{devices:#}");
    assert!(
        dongle["detail"].as_str().expect("detail").contains("0403"),
        "{dongle:#}"
    );
    assert_eq!(finding(devices, "key")["ok"], false, "{devices:#}");
    assert_eq!(devices["ok"], false);
    let risks = devices["risks"].to_string();
    assert!(
        risks.contains("stick") && risks.contains("dongle"),
        "{risks}"
    );
    rig.assert_only_reads();
}

/// The size check of `web` in `project`, mounting the custom volume `vol1` of pool `default`, whose 2 MiB are written at
/// `custom/<folder>`, while `features` is the config of the project shop.
fn volume_size(project: &str, features: Value, folder: &str) -> Value {
    let mut source = Host::new(false, "203.0.113.10");
    source.projects[1]["config"] = features;
    let mut web = instance("web", "00:16:3e:00:00:01");
    web["project"] = json!(project);
    web["devices"]["vol"] =
        json!({ "type": "disk", "pool": "default", "source": "vol1", "path": "/vol" });
    source.instances.push(web);
    source.profiles.push((
        "default",
        json!({ "name": "default", "description": "", "config": {}, "devices": {} }),
    ));
    let rig = Rig::new(source, Host::new(false, "203.0.113.20"));
    let own = if project == "default" {
        "web".to_string()
    } else {
        format!("{project}_web")
    };
    rig.write(&format!("storage-pools/default/containers/{own}/rootfs"), 1);
    rig.write(&format!("storage-pools/default/custom/{folder}"), 2);
    let answer = rig
        .call(json!({ "instances": [{ "project": project, "name": "web" }], "target": "node-b" }));
    rig.assert_only_reads();
    check(&answer["rows"][0], "size").clone()
}

#[test]
fn a_custom_volume_is_measured_where_incus_keeps_it_project_prefix_always_and_default_without_the_feature()
 {
    for (project, features, folder) in [
        // The default project: Incus still prefixes, default_<name>.
        ("default", json!({}), "default_vol1"),
        // A project without features.storage.volumes keeps its volumes in default.
        (
            "shop",
            json!({ "features.profiles": "true", "features.networks": "false" }),
            "default_vol1",
        ),
        // A project with the feature keeps its own.
        (
            "shop",
            json!({ "features.profiles": "true", "features.storage.volumes": "true" }),
            "shop_vol1",
        ),
    ] {
        let size = volume_size(project, features, folder);
        assert_eq!(size["ok"], true, "{project} {folder}: {size:#}");
        assert!(
            size["measure"]["volumeBytes"]
                .as_u64()
                .is_some_and(|bytes| bytes >= 2 * MIB),
            "{project} {folder}: {size:#}"
        );
        let detail = size["detail"].as_str().expect("detail");
        assert!(
            detail.contains(folder),
            "the detail names the folder walked: {detail}"
        );
    }
}

#[test]
fn snapshots_that_do_not_fit_leave_a_second_figure_that_says_it_fits_without_them() {
    let mut target = Host::new(false, "203.0.113.20");
    target.pools = vec![("default", "btrfs", 92 * MIB, 100 * MIB)];
    let rig = Rig::new(Host::new(true, "203.0.113.10"), target);
    rig.files("web", 4);
    rig.write(
        "storage-pools/default/containers-snapshots/shop_web/snap0",
        5,
    );
    rig.write(
        "storage-pools/default/containers-snapshots/shop_web/snap1",
        5,
    );
    let answer = rig.preflight(&["web"]);
    let size = check(&answer["rows"][0], "size");
    let measure = &size["measure"];
    assert_eq!(
        size["ok"], false,
        "the upper bound stays the answer: {size:#}"
    );
    let available = measure["availableBytes"].as_u64().expect("available");
    assert!(measure["neededBytes"].as_u64().expect("needed") > available);
    let without = measure["neededWithoutSnapshotsBytes"]
        .as_u64()
        .expect("without");
    let own = measure["instanceBytes"].as_u64().expect("instance");
    assert_eq!(without, own + own / 10);
    assert!(without <= available, "{measure:#}");
    assert_eq!(measure["fitsWithoutSnapshots"], true);
    let detail = size["detail"].as_str().expect("detail");
    assert!(detail.contains("fits without snapshots"), "{detail}");
    rig.assert_only_reads();
}

/// A stopped virtual machine `db` on pool `default` with a root disk of `size` (none when `None`).
fn virtual_machine(size: Option<&str>) -> Value {
    let mut root = json!({ "type": "disk", "path": "/", "pool": "default" });
    if let Some(size) = size {
        root["size"] = json!(size);
    }
    json!({
        "name": "db", "project": "shop", "type": "virtual-machine", "status": "Stopped", "architecture": "x86_64",
        "profiles": ["default"],
        "config": { "volatile.eth0.hwaddr": "00:16:3e:00:00:09" },
        "devices": { "root": root, "eth0": { "type": "nic", "network": "arcabr0" } }
    })
}

/// A virtual machine's block disk cannot be walked: its size is the one Incus gives it (its root device's, else the
/// pool's `volume.size`, else the volume state's total, which Incus answers from its own 10 GiB default), counted whole.
/// Nothing is written.
#[test]
fn a_virtual_machines_block_disk_is_sized_from_incus_and_counted_whole() {
    for (driver, size, pool_size, bytes, how) in [
        (
            "ceph",
            Some("3GiB"),
            None,
            3 * 1024 * MIB,
            "its root disk device's size",
        ),
        (
            "zfs",
            Some("2147483648"),
            None,
            2 * 1024 * MIB,
            "its root disk device's size",
        ),
        (
            "lvm",
            None,
            Some("512MiB"),
            512 * MIB,
            "the pool's volume.size",
        ),
        // The pool's volume.size, not the state's total: Incus made the disk at the pool's size, and answers the state's
        // total from its own default.
        (
            "ceph",
            None,
            Some("50GiB"),
            50 * 1024 * MIB,
            "the pool's volume.size",
        ),
        (
            "ceph",
            None,
            None,
            10 * 1024 * MIB,
            "the size its volume's state reports",
        ),
    ] {
        let mut source = Host::new(false, "203.0.113.10");
        source.instances = vec![virtual_machine(size)];
        source.pools = vec![("default", driver, 0, 100 * 1024 * MIB)];
        let mut target = Host::new(false, "203.0.113.20");
        target.pools = vec![("default", driver, 0, 100 * 1024 * MIB)];
        let rig = Rig::new(source, target);
        if let Some(pool_size) = pool_size {
            rig.source
                .set_pool_config("default", json!({ "volume.size": pool_size }));
        }
        let answer = rig.preflight(&["db"]);
        let size_check = check(&answer["rows"][0], "size");
        assert_eq!(size_check["ok"], true, "{driver}: {size_check:#}");
        assert_eq!(size_check["measure"]["method"], "incus");
        assert_eq!(
            size_check["measure"]["instanceBytes"], bytes,
            "{driver} {pool_size:?}"
        );
        assert_eq!(size_check["measure"]["neededBytes"], bytes + bytes / 10);
        let detail = size_check["detail"].as_str().unwrap();
        assert!(
            detail.contains(&format!(
                "from the sizes Incus gives: the virtual machine's {driver} disk"
            )),
            "{detail}"
        );
        assert!(detail.contains(how), "{how}: {detail}");
        assert!(
            size_check["risks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|risk| risk.as_str().unwrap().contains("counted at its whole size")),
            "{size_check:#}"
        );
        rig.assert_only_reads();
    }
}

/// A stopped virtual machine's custom volume on a block pool that neither the volume nor the pool sizes is counted at
/// Incus's own default, the size Incus made it, instead of blocking the plan.
#[test]
fn a_virtual_machines_unsized_custom_volume_counts_at_incus_default() {
    let mut db = virtual_machine(Some("1GiB"));
    db["devices"]["data"] = json!({ "type": "disk", "pool": "default", "source": "db-data" });
    let mut source = Host::new(false, "203.0.113.10");
    source.instances = vec![db];
    source.pools = vec![("default", "ceph", 0, 100 * 1024 * MIB)];
    let mut target = Host::new(false, "203.0.113.20");
    target.pools = vec![("default", "ceph", 0, 100 * 1024 * MIB)];
    let rig = Rig::new(source, target);
    rig.source.add_volume(
        "default",
        "default",
        "db-data",
        json!({ "content_type": "block", "config": {} }),
    );
    let answer = rig.preflight(&["db"]);
    let size_check = check(&answer["rows"][0], "size");
    assert_eq!(size_check["ok"], true, "{size_check:#}");
    assert_eq!(
        size_check["measure"]["volumeBytes"],
        10 * 1024 * MIB,
        "{size_check:#}"
    );
    rig.assert_only_reads();
}
