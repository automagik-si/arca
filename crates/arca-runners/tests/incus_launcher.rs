//! The Incus launcher against the repository's fake Incus: a job machine is a virtual machine of exactly its class's
//! size, carrying the pool's mark, with Arca's card on the pool's bridge naming the pool's ACL; only marked machines are
//! listed and destroyed; the ACL is made once and reused, and a gone pool's ACL is deleted.

use std::fs;
use std::net::{IpAddr, Ipv4Addr};
use std::os::unix::fs::PermissionsExt as _;

use arca_incus::runner_vm::AclState;
use arca_incus::{FakeIncus, FakeState};
use arca_runners::incus::{IncusLauncher, remove_gone_pool_acls};
use arca_runners::launcher::{Launcher, MachineSpec};
use serde_json::{Value, json};

const PROJECT: &str = "arcak-runners";
const IMAGE: &str = "arca-runner-image-ubuntu24-20261111-370";
/// The node's service bridge, the default of a pool's `network`.
const BRIDGE: &str = "arcak-svcbr";

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
                json!({ "name": BRIDGE, "type": "bridge", "managed": true,
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
        (BRIDGE.to_string(), vec![ip([192, 0, 2, 1])]),
        ("ci-br0".to_string(), vec![ip([198, 51, 100, 129])]),
        ("uplink0".to_string(), vec![ip([198, 51, 100, 7])]),
    ]);
    fake.set_host_netmask(ip([192, 0, 2, 1]), 24);
    fake.set_host_netmask(ip([198, 51, 100, 129]), 25);
    fake.set_host_netmask(ip([198, 51, 100, 7]), 25);
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

fn launcher(fake: &FakeIncus, pool: &str, network: &str) -> Result<IncusLauncher, String> {
    IncusLauncher::new(
        fake.socket().to_path_buf(),
        PROJECT,
        pool,
        IMAGE,
        vec![],
        None,
        network,
    )
    .map_err(|error| error.to_string())
}

fn spec(name: &str) -> MachineSpec {
    MachineSpec {
        name: name.to_string(),
        cpu: 4,
        memory_mib: 16 * 1024,
        disk_gib: None,
    }
}

/// The ACL writes the fake received: creates, patches and deletes of a network ACL.
fn acl_writes(fake: &FakeIncus) -> Vec<String> {
    fake.writes()
        .into_iter()
        .filter(|write| write.path.starts_with("/1.0/network-acls"))
        .map(|write| format!("{} {}", write.method, write.path))
        .collect()
}

#[test]
fn a_job_machine_is_a_marked_vm_of_its_size_and_only_marked_machines_go() {
    let (fake, dir) = fake();
    let launcher = launcher(&fake, "acme", BRIDGE).expect("launcher");
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
    let other = self::launcher(&fake, "other", BRIDGE).expect("launcher");
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
fn a_job_machines_card_is_arcas_on_the_service_bridge_and_names_the_pools_acl() {
    let (fake, dir) = fake();
    let launcher = launcher(&fake, "acme", BRIDGE).expect("launcher");
    assert_eq!(launcher.acl(), AclState::Created);
    assert_eq!(launcher.acl_name(), "arca-runner-acme");
    let spec = spec("acme-0-11223344");
    launcher.create(&spec).expect("create");
    let made = instance(&fake, &spec.name).expect("the machine exists");
    // The card replaces the profile's eth0: on the bridge, filtered, nothing passes that no rule allows.
    assert_eq!(
        made["devices"]["eth0"],
        json!({
            "type": "nic",
            "network": BRIDGE,
            "security.mac_filtering": "true",
            "security.ipv4_filtering": "true",
            "security.ipv6_filtering": "true",
            "security.acls": "arca-runner-acme",
            "security.acls.default.ingress.action": "reject",
            "security.acls.default.egress.action": "reject",
        })
    );
    // The ACL, in the project that holds the pool's project's networks, carries the pool's mark; nothing inbound, and
    // the egress rules of a tenant's service with the whole subnet rejected, from the bridge's gateway and the host's
    // addresses.
    let list = acl(&fake, "arca-runner-acme").expect("the ACL");
    assert_eq!(list["config"]["user.arcakernel.runner"], "acme");
    assert_eq!(list["ingress"], json!([]));
    let hosts = json!(["192.0.2.1/24", "198.51.100.129/25", "198.51.100.7/25"]);
    let wanted = arca_netproject::tenant::job_egress_rules(
        Ipv4Addr::new(192, 0, 2, 1),
        "192.0.2.0/24",
        &hosts,
    );
    assert!(arca_incus::acl_rules_match(
        list["egress"].as_array().expect("rules"),
        &wanted
    ));
    assert_eq!(list["egress"][0]["destination"], "192.0.2.0/24");
    assert_eq!(list["egress"][0]["action"], "reject");
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn a_pools_network_key_puts_the_card_on_that_bridge_with_its_own_subnet_rejected() {
    let (fake, dir) = fake();
    let launcher = launcher(&fake, "acme", "ci-br0").expect("launcher");
    launcher.create(&spec("acme-0-55667788")).expect("create");
    let made = instance(&fake, "acme-0-55667788").expect("the machine exists");
    assert_eq!(made["devices"]["eth0"]["network"], "ci-br0");
    let list = acl(&fake, "arca-runner-acme").expect("the ACL");
    assert_eq!(list["egress"][0]["destination"], "198.51.100.128/25");
    // A bridge Incus does not have gives no gateway to build the rules from: the pool does not start.
    assert!(self::launcher(&fake, "bare", "no-such-bridge").is_err());
    assert!(acl(&fake, "arca-runner-bare").is_none());
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn the_pools_acl_is_reused_at_the_next_start_and_follows_the_hosts_addresses() {
    let (fake, dir) = fake();
    let first = launcher(&fake, "acme", BRIDGE).expect("launcher");
    assert_eq!(first.acl(), AclState::Created);
    assert_eq!(
        acl_writes(&fake),
        vec!["POST /1.0/network-acls?project=arcak-runners".to_string()]
    );
    drop(first);
    // A restart: the ACL is there with the same rules, and nothing is written.
    let again = launcher(&fake, "acme", BRIDGE).expect("launcher");
    assert_eq!(again.acl(), AclState::Reused);
    assert_eq!(acl_writes(&fake).len(), 1, "{:?}", acl_writes(&fake));
    // The host gained an address: the next start replaces the rules, and the mark stays.
    fake.set_host_interfaces(vec![
        (BRIDGE.to_string(), vec![ip([192, 0, 2, 1])]),
        ("uplink0".to_string(), vec![ip([198, 51, 100, 7]), ip([203, 0, 113, 5])]),
    ]);
    let later = launcher(&fake, "acme", BRIDGE).expect("launcher");
    assert_eq!(later.acl(), AclState::Updated);
    let writes = acl_writes(&fake);
    assert_eq!(writes.len(), 2, "{writes:?}");
    assert_eq!(
        writes[1],
        "PATCH /1.0/network-acls/arca-runner-acme?project=arcak-runners"
    );
    let list = acl(&fake, "arca-runner-acme").expect("the ACL");
    assert_eq!(list["config"]["user.arcakernel.runner"], "acme");
    assert!(list["egress"].to_string().contains("203.0.113.5/32"));
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn an_acl_of_the_pools_name_without_its_mark_is_left_alone() {
    let (fake, dir) = fake();
    let foreign = json!({ "name": "arca-runner-acme", "description": "by hand", "config": {},
        "ingress": [], "egress": [] });
    fake.add_acl("default", foreign.clone());
    assert!(launcher(&fake, "acme", BRIDGE).is_err());
    assert!(acl_writes(&fake).is_empty());
    let list = acl(&fake, "arca-runner-acme").expect("the ACL");
    assert_eq!(list["description"], "by hand");
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn a_gone_pools_acl_is_deleted_and_no_other() {
    let (fake, dir) = fake();
    launcher(&fake, "kept", BRIDGE).expect("launcher");
    launcher(&fake, "gone", BRIDGE).expect("launcher");
    // An ACL without the mark, and a marked one whose name is not its pool's: neither is a pool's.
    fake.add_acl(
        "default",
        json!({ "name": "arca-runner-byhand", "config": {}, "ingress": [], "egress": [] }),
    );
    fake.add_acl(
        "default",
        json!({ "name": "web-in", "config": { "user.arcakernel.runner": "web" }, "ingress": [], "egress": [] }),
    );
    let lines = remove_gone_pool_acls(fake.socket(), &["kept"]);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("gone"), "{lines:?}");
    assert!(acl(&fake, "arca-runner-gone").is_none());
    assert!(acl(&fake, "arca-runner-kept").is_some());
    assert!(acl(&fake, "arca-runner-byhand").is_some());
    assert!(acl(&fake, "web-in").is_some());
    assert_eq!(
        acl_writes(&fake)
            .iter()
            .filter(|write| write.starts_with("DELETE"))
            .cloned()
            .collect::<Vec<_>>(),
        vec!["DELETE /1.0/network-acls/arca-runner-gone?project=default".to_string()]
    );
    // Run again, nothing is left to do.
    assert!(remove_gone_pool_acls(fake.socket(), &["kept"]).is_empty());
    let _ = fs::remove_dir_all(dir);
}
