//! The Incus launcher against the repository's fake Incus: a job machine is a virtual machine of exactly its class's
//! size, carrying the pool's mark, with Arca's card, and no other, on the pool's own bridge (or the bridge the pool
//! names) naming the pool's ACL; only marked machines are listed and destroyed; the bridge and the ACL are made once
//! and reused, and a gone pool's are deleted.

use std::fs;
use std::net::{IpAddr, Ipv4Addr};
use std::os::unix::fs::PermissionsExt as _;

use arca_incus::runner_vm::AclState;
use arca_incus::{FakeIncus, FakeState};
use arca_runners::incus::{IncusLauncher, NetworkPlan, choose_subnet, remove_gone_pools};
use arca_runners::launcher::{Launcher, MachineSpec};
use serde_json::{Value, json};

const PROJECT: &str = "arcak-runners";
const IMAGE: &str = "arca-runner-image-ubuntu24-20261111-370";
/// The node's service bridge and its subnet.
const SERVICE_BRIDGE: &str = "arcak-svcbr";
const SERVICE_CIDR: &str = "192.0.2.0/24";

fn fake() -> (FakeIncus, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "arca-runners-incus-{}-{}-{:08x}",
        std::process::id(),
        arca_runners::time::now_ms(),
        rand_u32()
    ));
    fs::create_dir_all(&dir).expect("scratch");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("0700");
    let fake = FakeIncus::start(
        &dir,
        FakeState {
            server: json!({}),
            projects: vec![json!({ "name": PROJECT, "config": { "features.images": "false", "features.profiles": "false" } })],
            instances: vec![json!({
                "name": "someone-else",
                "project": PROJECT,
                "type": "virtual-machine",
                "status": "Running",
                "config": { "volatile.uuid": "00000000-0000-4000-8000-000000000001" },
                "devices": {},
            })],
            networks: vec![
                json!({ "name": SERVICE_BRIDGE, "type": "bridge", "managed": true,
                    "config": { "ipv4.address": "192.0.2.1/24", "ipv4.nat": "true", "ipv6.address": "none" } }),
                json!({ "name": "ci-br0", "type": "bridge", "managed": true,
                    "config": { "ipv4.address": "198.51.100.129/25", "ipv6.address": "none" } }),
            ],
            pools: vec![],
        },
    )
    .expect("fake");
    fake.add_profile(
        "default",
        json!({"name":"default","config":{},"devices":{
            "root":{"type":"disk","pool":"pool1","path":"/"},
            "eth0":{"type":"nic","network":"incusbr0","name":"eth0"}}}),
    );
    fake.set_host_interfaces(vec![
        (SERVICE_BRIDGE.to_string(), vec![ip([192, 0, 2, 1])]),
        ("ci-br0".to_string(), vec![ip([198, 51, 100, 129])]),
        ("uplink0".to_string(), vec![ip([203, 0, 113, 7])]),
    ]);
    fake.set_host_netmask(ip([192, 0, 2, 1]), 24);
    fake.set_host_netmask(ip([198, 51, 100, 129]), 25);
    fake.set_host_netmask(ip([203, 0, 113, 7]), 24);
    (fake, dir)
}

fn rand_u32() -> u32 {
    let mut bytes = [0u8; 4];
    getrandom::fill(&mut bytes).expect("random");
    u32::from_le_bytes(bytes)
}

fn ip(octets: [u8; 4]) -> IpAddr {
    IpAddr::V4(Ipv4Addr::from(octets))
}

fn get(fake: &FakeIncus, path: &str) -> Option<Value> {
    let read = arca_incus::UnixIncus::new(fake.socket().to_path_buf());
    arca_incus::IncusRead::get(&read, path).ok()
}

fn instance(fake: &FakeIncus, name: &str) -> Option<Value> {
    get(fake, &format!("/1.0/instances/{name}?project={PROJECT}"))
}

/// The ACL `name` where Incus keeps it: the project does not enable `features.networks`, so in `default`.
fn acl(fake: &FakeIncus, name: &str) -> Option<Value> {
    get(fake, &format!("/1.0/network-acls/{name}?project=default"))
}

fn network(fake: &FakeIncus, name: &str) -> Option<Value> {
    get(fake, &format!("/1.0/networks/{name}?project=default"))
}

/// The node's plan: the pool's own bridge, the service subnet closed, one reserved range.
fn own() -> NetworkPlan {
    NetworkPlan {
        bridge: None,
        service_cidr: Some(SERVICE_CIDR.to_string()),
        reserved: vec!["100.70.0.0/16".to_string()],
        routes: vec![],
    }
}

/// `own()` with the host's routing table holding `routes` (destination, interface).
fn routed(routes: &[(&str, &str)]) -> NetworkPlan {
    NetworkPlan {
        routes: routes
            .iter()
            .map(|(destination, interface)| (destination.to_string(), interface.to_string()))
            .collect(),
        ..own()
    }
}

fn named(bridge: &str) -> NetworkPlan {
    NetworkPlan {
        bridge: Some(bridge.to_string()),
        ..own()
    }
}

fn launcher_with(
    fake: &FakeIncus,
    pool: &str,
    profiles: Vec<String>,
    plan: &NetworkPlan,
) -> Result<IncusLauncher, String> {
    IncusLauncher::new(
        fake.socket().to_path_buf(),
        PROJECT,
        pool,
        IMAGE,
        profiles,
        None,
        plan,
    )
    .map_err(|error| error.to_string())
}

fn launcher(fake: &FakeIncus, pool: &str, plan: &NetworkPlan) -> Result<IncusLauncher, String> {
    launcher_with(fake, pool, vec![], plan)
}

fn spec(name: &str) -> MachineSpec {
    MachineSpec {
        name: name.to_string(),
        cpu: 4,
        memory_mib: 16 * 1024,
        disk_gib: None,
    }
}

/// The writes the fake received under `prefix`, as `<method> <path>`.
fn writes(fake: &FakeIncus, prefix: &str) -> Vec<String> {
    fake.writes()
        .into_iter()
        .filter(|write| write.path.starts_with(prefix))
        .map(|write| format!("{} {}", write.method, write.path))
        .collect()
}

/// Whether two CIDR ranges share an address.
fn overlaps(left: &str, right: &str) -> bool {
    let parse = |cidr: &str| {
        let (ip, prefix) = cidr.split_once('/').unwrap_or((cidr, "32"));
        let ip = u32::from(ip.parse::<Ipv4Addr>().expect("an address"));
        let prefix: u32 = prefix.parse().expect("a prefix");
        let mask = u32::MAX.checked_shl(32 - prefix).unwrap_or(0);
        (ip & mask, mask)
    };
    let ((a, am), (b, bm)) = (parse(left), parse(right));
    let mask = am & bm;
    a & mask == b & mask
}

/// The subnet of a bridge whose `ipv4.address` is `address` (`a.b.c.1/24`).
fn subnet_of(address: &str) -> String {
    let (ip, prefix) = address.split_once('/').expect("a CIDR");
    let ip: Ipv4Addr = ip.parse().expect("an address");
    let prefix: u32 = prefix.parse().expect("a prefix");
    let mask = u32::MAX.checked_shl(32 - prefix).unwrap_or(0);
    format!("{}/{prefix}", Ipv4Addr::from(u32::from(ip) & mask))
}

#[test]
fn a_job_machine_is_a_marked_vm_of_its_size_and_only_marked_machines_go() {
    let (fake, dir) = fake();
    let launcher = launcher(&fake, "acme", &own()).expect("launcher");
    let spec = spec("acme-0-0a1b2c3d");
    launcher.create(&spec).expect("create");
    let made = instance(&fake, &spec.name).expect("the machine exists");
    let made = made.get("metadata").unwrap_or(&made).clone();
    assert_eq!(made["type"], "virtual-machine");
    assert_eq!(made["config"]["user.arcakernel.runner"], "acme");
    assert_eq!(made["config"]["limits.cpu"], "4");
    assert_eq!(made["config"]["limits.memory"], "16384MiB");
    // A second machine of that name is refused.
    assert!(launcher.create(&spec).is_err());
    // Only the pool's own machine is listed; another instance of the project is not touched.
    assert_eq!(launcher.existing().expect("list"), vec![spec.name.clone()]);
    assert!(launcher.destroy("someone-else").is_err());
    assert!(instance(&fake, "someone-else").is_some());
    // Another pool's launcher sees none of this pool's machines.
    let other = self::launcher(&fake, "other", &own()).expect("launcher");
    assert!(other.existing().expect("list").is_empty());
    assert!(other.destroy(&spec.name).is_err());
    launcher.destroy(&spec.name).expect("destroy");
    assert!(instance(&fake, &spec.name).is_none());
    assert!(launcher.existing().expect("list").is_empty());
    // A machine that is gone is not an error.
    launcher.destroy(&spec.name).expect("destroy again");
    let writes = fake.writes();
    assert!(writes.iter().any(|write| {
        write.method == "POST"
            && write
                .config_keys
                .contains(&"user.arcakernel.runner".to_string())
    }));
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn a_pool_gets_its_own_bridge_and_its_machines_arcas_card_on_it() {
    let (fake, dir) = fake();
    let launcher = launcher(&fake, "acme", &own()).expect("launcher");
    assert_eq!(launcher.acl(), AclState::Created);
    assert_eq!(launcher.acl_name(), "arca-runner-acme");
    let bridge = launcher.bridge().to_string();
    assert!(
        bridge.starts_with("arcarun-") && bridge.len() <= 15,
        "{bridge}"
    );
    // The bridge is Arca's: a /24 with NAT and no IPv6, in private space clear of the service subnet, the reserved
    // ranges and the host's networks, marked with the pool.
    let made = network(&fake, &bridge).expect("the pool's bridge");
    assert_eq!(made["type"], "bridge");
    assert_eq!(made["config"]["user.arcakernel.runner"], "acme");
    assert_eq!(made["config"]["ipv4.nat"], "true");
    assert_eq!(made["config"]["ipv6.address"], "none");
    // Its DNS does not hand the host's /etc/hosts to a job.
    assert_eq!(made["config"]["dns.include_hosts"], "false");
    let address = made["config"]["ipv4.address"]
        .as_str()
        .expect("an address")
        .to_string();
    assert!(address.ends_with(".1/24"), "{address}");
    let subnet = subnet_of(&address);
    assert!(overlaps(&subnet, "172.16.0.0/12"), "{subnet}");
    for taken in [
        SERVICE_CIDR,
        "100.70.0.0/16",
        "198.51.100.128/25",
        "203.0.113.0/24",
    ] {
        assert!(!overlaps(&subnet, taken), "{subnet} overlaps {taken}");
    }
    assert_eq!(
        subnet,
        subnet_of(&choose_subnet("acme", &[]).expect("a subnet"))
    );

    let spec = spec("acme-0-11223344");
    launcher.create(&spec).expect("create");
    let machine = instance(&fake, &spec.name).expect("the machine exists");
    // Made stopped, its devices checked, then started: one start after the create.
    let made_then_started: Vec<String> = writes(&fake, "/1.0/instances");
    assert_eq!(
        made_then_started,
        vec![
            format!("POST /1.0/instances?project={PROJECT}"),
            format!("PUT /1.0/instances/{}/state?project={PROJECT}", spec.name),
        ]
    );
    assert_eq!(machine["status"], "Running");
    // The card replaces the profile's eth0: on the pool's bridge, filtered, nothing passes that no rule allows.
    assert_eq!(
        machine["devices"]["eth0"],
        json!({
            "type": "nic",
            "network": bridge,
            "security.mac_filtering": "true",
            "security.ipv4_filtering": "true",
            "security.ipv6_filtering": "true",
            "security.acls": "arca-runner-acme",
            "security.acls.default.ingress.action": "reject",
            "security.acls.default.egress.action": "reject",
        })
    );
    // The ACL carries the pool's mark; nothing inbound, and the egress rules of a tenant's service with the pool's
    // subnet and the service subnet rejected, from the bridge's gateway and the host's addresses.
    let list = acl(&fake, "arca-runner-acme").expect("the ACL");
    assert_eq!(list["config"]["user.arcakernel.runner"], "acme");
    assert_eq!(list["ingress"], json!([]));
    let gateway: Ipv4Addr = address.split('/').next().unwrap().parse().unwrap();
    let hosts = json!(["192.0.2.1/24", "198.51.100.129/25", "203.0.113.7/24"]);
    let wanted = arca_netproject::tenant::job_egress_rules(
        gateway,
        &[subnet.clone(), SERVICE_CIDR.to_string()],
        &hosts,
    );
    assert!(arca_incus::acl_rules_match(
        list["egress"].as_array().expect("rules"),
        &wanted
    ));
    assert_eq!(
        list["egress"][0]["destination"],
        format!("{subnet},{SERVICE_CIDR}")
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn the_pools_bridge_is_reused_and_its_subnet_follows_from_the_pool() {
    let (fake, dir) = fake();
    let first = launcher(&fake, "acme", &own()).expect("launcher");
    let bridge = first.bridge().to_string();
    let address = network(&fake, &bridge).unwrap()["config"]["ipv4.address"].clone();
    drop(first);
    // A restart finds the bridge and the ACL and writes neither.
    let again = launcher(&fake, "acme", &own()).expect("launcher");
    assert_eq!(again.bridge(), bridge);
    assert_eq!(again.acl(), AclState::Reused);
    assert_eq!(
        writes(&fake, "/1.0/networks").len(),
        1,
        "{:?}",
        writes(&fake, "/1.0/networks")
    );
    assert_eq!(writes(&fake, "/1.0/network-acls").len(), 1);
    assert_eq!(
        network(&fake, &bridge).unwrap()["config"]["ipv4.address"],
        address
    );
    // Another pool gets another bridge and another subnet.
    let other = launcher(&fake, "other", &own()).expect("launcher");
    assert_ne!(other.bridge(), bridge);
    let other_address = network(&fake, other.bridge()).unwrap()["config"]["ipv4.address"].clone();
    assert!(!overlaps(
        &subnet_of(address.as_str().unwrap()),
        &subnet_of(other_address.as_str().unwrap())
    ));
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn a_subnet_is_chosen_clear_of_every_taken_range_and_the_same_each_time() {
    let first = choose_subnet("acme", &[]).expect("a subnet");
    assert_eq!(
        choose_subnet("acme", &[]),
        Some(first.clone()),
        "the same each time"
    );
    assert!(first.ends_with(".1/24"));
    let taken = vec![subnet_of(&first), "172.16.0.0/16".to_string()];
    let next = choose_subnet("acme", &taken).expect("another subnet");
    for range in &taken {
        assert!(
            !overlaps(&subnet_of(&next), range),
            "{next} overlaps {range}"
        );
    }
    assert!(
        choose_subnet("acme", &["172.16.0.0/12".to_string()]).is_none(),
        "no room"
    );
    // A host address alone, without its length, takes its /24.
    let host = subnet_of(&first).replace(".0/24", ".9");
    assert_ne!(choose_subnet("acme", &[host]), Some(first));
}

#[test]
fn a_taken_subnet_moves_the_pools_bridge_and_a_hand_made_bridge_of_its_name_is_left_alone() {
    let (fake, dir) = fake();
    // The pool's first choice is already some network's: the bridge goes elsewhere.
    let first = choose_subnet("acme", &[]).expect("a subnet");
    fake.set_host_interfaces(vec![
        (SERVICE_BRIDGE.to_string(), vec![ip([192, 0, 2, 1])]),
        (
            "lan9".to_string(),
            vec![first.split('/').next().unwrap().parse().unwrap()],
        ),
    ]);
    let launcher = launcher(&fake, "acme", &own()).expect("launcher");
    let address = network(&fake, launcher.bridge()).unwrap()["config"]["ipv4.address"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(!overlaps(&subnet_of(&address), &subnet_of(&first)));
    drop(launcher);

    // A network of the pool's bridge name without its mark stops the pool and is never written.
    let (fake, dir2) = self::fake();
    let bridge =
        arca_incus::runner_vm::RunnerVms::new(fake.socket().to_path_buf(), PROJECT, "acme")
            .expect("writer")
            .bridge_name();
    fake.add_network(
        "default",
        json!({"name": bridge, "type": "bridge", "description": "by hand",
            "config": {"ipv4.address": "198.51.100.1/24"}}),
    );
    assert!(self::launcher(&fake, "acme", &own()).is_err());
    assert_eq!(network(&fake, &bridge).unwrap()["description"], "by hand");
    assert!(
        fake.writes()
            .iter()
            .all(|write| !write.path.contains(&bridge))
    );
    let _ = fs::remove_dir_all(dir);
    let _ = fs::remove_dir_all(dir2);
}

#[test]
fn a_pools_network_key_puts_the_card_on_that_bridge_with_its_own_subnet_rejected() {
    let (fake, dir) = fake();
    let launcher = launcher(&fake, "acme", &named("ci-br0")).expect("launcher");
    assert_eq!(launcher.bridge(), "ci-br0");
    assert!(
        writes(&fake, "/1.0/networks").is_empty(),
        "a named bridge is never made"
    );
    launcher.create(&spec("acme-0-55667788")).expect("create");
    let made = instance(&fake, "acme-0-55667788").expect("the machine exists");
    assert_eq!(made["devices"]["eth0"]["network"], "ci-br0");
    let list = acl(&fake, "arca-runner-acme").expect("the ACL");
    // The bridge's real prefix, not a /24.
    assert_eq!(
        list["egress"][0]["destination"],
        format!("198.51.100.128/25,{SERVICE_CIDR}")
    );
    // A bridge Incus does not have gives no gateway to build the rules from: the pool does not start.
    let error = self::launcher(&fake, "bare", &named("no-such-bridge"))
        .err()
        .expect("the pool does not start");
    assert_eq!(
        error,
        format!(
            "the pool's network no-such-bridge is not an Incus network the project {PROJECT} sees: check its spelling"
        )
    );
    assert!(acl(&fake, "arca-runner-bare").is_none());
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn a_machine_with_any_card_but_arcas_is_deleted_and_its_create_fails() {
    let (fake, dir) = fake();
    fake.add_profile(
        "default",
        json!({"name":"extra","config":{},"devices":{
            "root":{"type":"disk","pool":"pool1","path":"/"},
            "eth1":{"type":"nic","network":"ci-br0"}}}),
    );
    let launcher =
        launcher_with(&fake, "acme", vec!["extra".to_string()], &own()).expect("launcher");
    let error = launcher
        .create(&spec("acme-0-99aabbcc"))
        .expect_err("a second card is refused");
    assert!(error.to_string().contains("eth1"), "{error}");
    assert!(
        instance(&fake, "acme-0-99aabbcc").is_none(),
        "the machine is gone"
    );
    assert!(launcher.existing().expect("list").is_empty());
    // It was never started: the create, then the delete, and no state change between.
    assert_eq!(
        writes(&fake, "/1.0/instances"),
        vec![
            format!("POST /1.0/instances?project={PROJECT}"),
            format!("DELETE /1.0/instances/acme-0-99aabbcc?project={PROJECT}"),
        ]
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn a_machine_with_any_device_but_its_root_disk_and_arcas_card_is_deleted_before_it_starts() {
    for (name, device) in [
        (
            "hostport",
            json!({"type":"proxy","listen":"tcp:0.0.0.0:2222","connect":"tcp:127.0.0.1:22"}),
        ),
        (
            "ib0",
            json!({"type":"infiniband","nictype":"physical","parent":"ib0"}),
        ),
        ("passed", json!({"type":"pci","address":"0000:03:00.0"})),
        // Only the root disk passes: a host directory, a host block device or a shared volume is a way in too.
        (
            "cache",
            json!({"type":"disk","source":"/srv/ci-cache","path":"/cache"}),
        ),
        ("hostblock", json!({"type":"disk","source":"/dev/sdb"})),
        (
            "shared",
            json!({"type":"disk","pool":"pool1","source":"shared-vol","path":"/shared"}),
        ),
    ] {
        let (fake, dir) = fake();
        let mut devices = json!({"root":{"type":"disk","pool":"pool1","path":"/"}});
        devices[name] = device;
        fake.add_profile(
            "default",
            json!({"name":"extra","config":{},"devices":devices}),
        );
        let launcher =
            launcher_with(&fake, "acme", vec!["extra".to_string()], &own()).expect("launcher");
        let error = launcher
            .create(&spec("acme-0-0d0d0d0d"))
            .expect_err("the device is refused");
        assert!(error.to_string().contains(name), "{error}");
        assert!(instance(&fake, "acme-0-0d0d0d0d").is_none(), "{name}");
        assert!(
            !writes(&fake, "/1.0/instances")
                .iter()
                .any(|write| write.contains("/state")),
            "{name}: never started"
        );
        let _ = fs::remove_dir_all(dir);
    }
}

#[test]
fn a_routed_range_moves_the_pools_bridge_and_one_that_now_meets_it_stops_the_pool() {
    let (fake, dir) = fake();
    // A range the host reaches through a route (a VPN, a peering) with no address of its own in it.
    let first = subnet_of(&choose_subnet("acme", &[]).expect("a subnet"));
    let wide = format!("{}/16", first.split('/').next().unwrap());
    let launcher = launcher(&fake, "acme", &routed(&[(&wide, "wg0")])).expect("launcher");
    let bridge = launcher.bridge().to_string();
    let address = network(&fake, &bridge).unwrap()["config"]["ipv4.address"]
        .as_str()
        .unwrap()
        .to_string();
    let subnet = subnet_of(&address);
    assert!(
        !overlaps(&subnet, &wide),
        "{subnet} is inside the route {wide}"
    );
    drop(launcher);
    let made = writes(&fake, "/1.0/networks").len();

    // The bridge's own connected route is not a conflict: a restart reuses it.
    let again = self::launcher(
        &fake,
        "acme",
        &routed(&[(&wide, "wg0"), (&subnet, &bridge)]),
    )
    .expect("launcher");
    assert_eq!(again.bridge(), bridge);
    drop(again);

    // A route that came since now meets the bridge: the pool stops, says so, and nothing is renumbered.
    let error = self::launcher(&fake, "acme", &routed(&[(&subnet, "tun7")]))
        .err()
        .expect("the pool does not start");
    assert!(
        error.contains(&bridge) && error.contains(&subnet),
        "{error}"
    );
    assert!(error.contains("tun7"), "{error}");
    assert_eq!(
        writes(&fake, "/1.0/networks").len(),
        made,
        "nothing is written"
    );
    assert_eq!(
        network(&fake, &bridge).unwrap()["config"]["ipv4.address"],
        address.as_str()
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn the_pools_acl_follows_the_hosts_addresses_at_each_create() {
    let (fake, dir) = fake();
    let launcher = launcher(&fake, "acme", &own()).expect("launcher");
    assert_eq!(writes(&fake, "/1.0/network-acls").len(), 1);
    // The same host: a create writes no rule.
    launcher.create(&spec("acme-0-00000001")).expect("create");
    assert_eq!(writes(&fake, "/1.0/network-acls").len(), 1);
    // The host gained a public address while the pool runs: the next create replaces the rules first.
    fake.set_host_interfaces(vec![
        (SERVICE_BRIDGE.to_string(), vec![ip([192, 0, 2, 1])]),
        (
            "uplink0".to_string(),
            vec![ip([203, 0, 113, 7]), ip([203, 0, 113, 99])],
        ),
    ]);
    launcher.create(&spec("acme-0-00000002")).expect("create");
    let acl_writes = writes(&fake, "/1.0/network-acls");
    assert_eq!(acl_writes.len(), 2, "{acl_writes:?}");
    assert_eq!(
        acl_writes[1],
        "PATCH /1.0/network-acls/arca-runner-acme?project=arcak-runners"
    );
    let list = acl(&fake, "arca-runner-acme").expect("the ACL");
    assert!(list["egress"].to_string().contains("203.0.113.99/32"));
    assert_eq!(list["config"]["user.arcakernel.runner"], "acme");
    // The rules were replaced before the machine was made.
    let all: Vec<String> = fake
        .writes()
        .iter()
        .map(|w| format!("{} {}", w.method, w.path))
        .collect();
    let patch = all
        .iter()
        .position(|w| w.starts_with("PATCH /1.0/network-acls"))
        .unwrap();
    let second = all
        .iter()
        .rposition(|w| w.starts_with("POST /1.0/instances"))
        .unwrap();
    assert!(patch < second, "{all:?}");
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn an_acl_of_the_pools_name_without_its_mark_is_left_alone() {
    let (fake, dir) = fake();
    let foreign = json!({ "name": "arca-runner-acme", "description": "by hand", "config": {},
        "ingress": [], "egress": [] });
    fake.add_acl("default", foreign);
    assert!(launcher(&fake, "acme", &own()).is_err());
    assert!(writes(&fake, "/1.0/network-acls").is_empty());
    let list = acl(&fake, "arca-runner-acme").expect("the ACL");
    assert_eq!(list["description"], "by hand");
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn a_gone_pools_acl_and_bridge_are_deleted_and_no_other() {
    let (fake, dir) = fake();
    let kept = launcher(&fake, "kept", &own()).expect("launcher");
    let gone = launcher(&fake, "gone", &own()).expect("launcher");
    // A pool still configured, moved to a named bridge: its own bridge has no pool left.
    let moved = launcher(&fake, "moved", &own()).expect("launcher");
    let (kept_bridge, gone_bridge, moved_bridge) = (
        kept.bridge().to_string(),
        gone.bridge().to_string(),
        moved.bridge().to_string(),
    );
    drop((kept, gone, moved));
    // An ACL without the mark, and a marked one whose name is not its pool's: neither is a pool's. The same for
    // networks.
    fake.add_acl(
        "default",
        json!({ "name": "arca-runner-byhand", "config": {}, "ingress": [], "egress": [] }),
    );
    fake.add_acl(
        "default",
        json!({ "name": "web-in", "config": { "user.arcakernel.runner": "web" }, "ingress": [], "egress": [] }),
    );
    fake.add_network(
        "default",
        json!({ "name": "arcarun-hand", "type": "bridge", "config": { "ipv4.address": "198.51.100.1/24" } }),
    );
    let lines = remove_gone_pools(fake.socket(), &["kept", "moved"], &["kept"]);
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert!(
        lines
            .iter()
            .all(|line| line.contains("gone") || line.contains("no longer uses")),
        "{lines:?}"
    );
    assert!(network(&fake, &moved_bridge).is_none());
    assert!(
        acl(&fake, "arca-runner-moved").is_some(),
        "a configured pool keeps its ACL"
    );
    assert!(acl(&fake, "arca-runner-gone").is_none());
    assert!(network(&fake, &gone_bridge).is_none());
    assert!(acl(&fake, "arca-runner-kept").is_some());
    assert!(network(&fake, &kept_bridge).is_some());
    assert!(acl(&fake, "arca-runner-byhand").is_some());
    assert!(acl(&fake, "web-in").is_some());
    assert!(network(&fake, "arcarun-hand").is_some());
    assert!(network(&fake, SERVICE_BRIDGE).is_some());
    let deletes: Vec<String> = fake
        .writes()
        .iter()
        .filter(|write| write.method == "DELETE")
        .map(|write| write.path.clone())
        .collect();
    assert_eq!(
        deletes,
        vec![
            "/1.0/network-acls/arca-runner-gone?project=default".to_string(),
            format!("/1.0/networks/{gone_bridge}?project=default"),
            format!("/1.0/networks/{moved_bridge}?project=default"),
        ]
    );
    // Run again, nothing is left to do.
    assert!(remove_gone_pools(fake.socket(), &["kept", "moved"], &["kept"]).is_empty());
    let _ = fs::remove_dir_all(dir);
}
