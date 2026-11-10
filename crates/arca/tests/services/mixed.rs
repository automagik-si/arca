//! Mixed nodes: containers on the managed service network and virtual machines on that network or on a host bridge, in the
//! one service project, against a fake Incus. No live host is touched.
use super::*;

const NET_ENTRY: &str = "net-arcak-svc";

/// A node with the managed service network and host bridges: its caps are the bridged ones.
fn mixed_kernel(bridges: &[&str], limits: Value) -> Value {
    json!({
        "projects": [PROJECT],
        "droiddProjects": ["other"],
        "serviceCidr": "192.0.2.0/24",
        "bridged": { "bridges": bridges, "pool": "vmpool", "limits": limits }
    })
}

/// A node with the managed service network alone, that may create virtual machines on it.
fn managed_vm_kernel(limits: Value) -> Value {
    json!({
        "projects": [PROJECT],
        "droiddProjects": ["other"],
        "serviceCidr": "192.0.2.0/24",
        "vmLimits": limits
    })
}

/// A virtual-machine template that offers a port to the containers that link to it.
fn db_template() -> Value {
    json!({
        "name": "db",
        "kind": "virtual-machine",
        "image": "images:debian/13",
        "limits": { "cpu": "2", "memory": "4GiB" },
        "rootSize": "20GiB",
        "ports": [5432],
        "provides": { "env": { "DB_ADDR": "{{host}}:{{port}}" } }
    })
}

fn project(rig: &Rig) -> Value {
    rig.get(&format!("/1.0/projects/{PROJECT}?project={PROJECT}"))
        .expect("the service project")["config"]
        .clone()
}

fn instance(rig: &Rig, name: &str) -> Value {
    rig.get(&format!("/1.0/instances/{name}?project={PROJECT}"))
        .unwrap_or_else(|| panic!("instance {name}"))
}

fn entry(rig: &Rig, id: &str) -> arca_kernel::desired::Entry {
    rig.entries()
        .into_iter()
        .find(|entry| entry.id.0 == id)
        .unwrap_or_else(|| panic!("entry {id}"))
}

fn create(
    handlers: &HashMap<&'static str, Arc<dyn ToolHandler>>,
    args: Value,
) -> Result<Value, CallError> {
    handlers["service_create"].call(args, &doctor())
}

/// What `arca serve` answers at its start on this node's state with the config edited to `kernel_config`: the check it
/// runs before the kernel opens, over the desired state on disk and a host that has the bridge `br0`. The running rig
/// holds its state directory's lock, as a serving process does, so the check reads a copy of the files, as the restarted
/// process reads them once the old one has gone.
fn serve_start(rig: &Rig, kernel_config: Value) -> Result<(), String> {
    let copy = rig
        .dir
        .join(format!("restart-{}", SEQ.fetch_add(1, Ordering::SeqCst)));
    fs::create_dir(&copy).expect("a copy of the state directory");
    fs::set_permissions(&copy, fs::Permissions::from_mode(0o700)).expect("0700");
    for file in fs::read_dir(&rig.dir)
        .expect("the state directory")
        .flatten()
    {
        if file.file_type().is_ok_and(|kind| kind.is_file()) {
            fs::copy(file.path(), copy.join(file.file_name())).expect("a state file");
        }
    }
    let sys = copy.join("sys");
    fs::create_dir_all(sys.join("br0/bridge")).expect("a host bridge");
    let cfg = node_config(&copy, &rig.fake, kernel_config);
    arca::cmd_serve::check_bridged_start(&cfg, &sys).map_err(|failure| match failure {
        arca::Failure::Config(why) | arca::Failure::Runtime(why) => why,
        other => format!("{other:?}"),
    })
}

/// Every effect the journal holds has an outcome: nothing is left as an intent.
fn no_open_intent(rig: &Rig) {
    let (records, torn) =
        arca_kernel::journal::read_journal(&rig.dir.join(arca::kernel::JOURNAL_FILE))
            .expect("the journal");
    assert!(torn.is_none());
    let mut last = std::collections::BTreeMap::new();
    for record in &records {
        last.insert(record.id, record.state);
    }
    assert!(
        last.values()
            .all(|state| *state != arca_kernel::journal::EffectState::Intent),
        "an intent is left open"
    );
}

/// The live node's case: a bridged node that already runs a virtual machine gains `kernel.serviceCidr`. The start is not
/// refused, the running machine and its record are untouched, and a container is then created on the managed network in
/// the same project, which its owner widened by one journaled write.
#[test]
fn a_bridged_node_that_gains_the_service_network_becomes_mixed_in_place_and_its_vm_is_untouched() {
    let rig = Rig::bridged(&["br0"], None);
    rig.call("service_create", json!({"name":"ws1","template":"ws"}))
        .expect("the bridged node's virtual machine");
    let vm_entry = entry(&rig, "service-ws1");
    let vm_instance = instance(&rig, "ws1");
    let before = project(&rig);
    assert_eq!(before["limits.containers"], "0");
    assert_eq!(before["limits.cpu"], "8");
    assert_eq!(
        entry(&rig, BRIDGED_ENTRY).config,
        json!({"project":PROJECT,"virtualMachines":4,"cpu":8,"memory":"16GiB","disk":"80GiB"}),
        "the entry the bridged node wrote"
    );

    // The config gains the service network. It passes the config rules, and serve's start check finds no disagreement.
    let (cfg, handlers) = rig.switched(mixed_kernel(&["br0"], roomy()));
    assert_eq!(arca::handlers::mode_refusal(&cfg, &rig.entries()), None);
    // The check `arca serve` runs first, over the desired state on disk and the host's bridges, passes too.
    assert_eq!(serve_start(&rig, mixed_kernel(&["br0"], roomy())), Ok(()));
    assert!(
        serve_start(&rig, managed_vm_kernel(roomy()))
            .expect_err("dropping kernel.bridged under the machine is still refused")
            .ends_with(
                "from bridged mode. Put kernel.bridged back, or remove its virtual machines first"
            )
    );
    assert_eq!(
        rig.fake.writes().len(),
        2,
        "the project and the machine, and nothing since"
    );

    let nats = create(&handlers, json!({"name":"nats","template":"nats"}))
        .expect("a container on the managed network");
    assert_eq!(nats["state"], "running");
    assert_eq!(nats["ipv4"], "192.0.2.10");
    let container = instance(&rig, "nats");
    assert_eq!(container["type"], "container");
    assert_eq!(container["devices"]["eth0"]["network"], BRIDGE);

    // The machine: the same record, the same instance, and no request that writes it.
    assert_eq!(entry(&rig, "service-ws1"), vm_entry);
    assert_eq!(instance(&rig, "ws1"), vm_instance);
    assert_eq!(
        rig.fake
            .writes()
            .iter()
            .filter(|write| write.path.contains("ws1"))
            .count(),
        0,
        "no write names the machine after its create: {:?}",
        rig.fake
            .writes()
            .iter()
            .map(|write| format!("{} {}", write.method, write.path))
            .collect::<Vec<_>>()
    );

    // The project is still the bridged entry's, still restricted, and widened: the container cap and the caps that would
    // count containers are gone, proxies are allowed, and the virtual-machine cap stays.
    let after = project(&rig);
    assert_eq!(after[KERNEL_ENTRY_KEY], BRIDGED_ENTRY);
    assert_eq!(after["restricted"], "true");
    assert_eq!(after["restricted.devices.nic"], "allow");
    assert_eq!(after["restricted.devices.proxy"], "allow");
    assert_eq!(after["limits.virtual-machines"], "4");
    for gone in [
        "limits.containers",
        "limits.cpu",
        "limits.memory",
        "limits.disk",
    ] {
        assert!(after.get(gone).is_none(), "{gone}");
    }
    let project_writes: Vec<String> = rig
        .fake
        .writes()
        .iter()
        .filter(|write| write.path.starts_with("/1.0/projects"))
        .map(|write| write.method.clone())
        .collect();
    assert_eq!(
        project_writes,
        ["POST", "PUT"],
        "one create, one widening, no second project"
    );

    // The desired state records the step: the owner is widened, and the network entry shares its project.
    assert_eq!(
        entry(&rig, BRIDGED_ENTRY).config,
        json!({"project":PROJECT,"virtualMachines":4,"cpu":8,"memory":"16GiB","disk":"80GiB","managed":true})
    );
    assert_eq!(
        entry(&rig, NET_ENTRY).config,
        json!({"project":PROJECT,"bridge":BRIDGE,"cidr":"192.0.2.0/24","virtualMachines":0,"sharedProject":true})
    );
    assert!(
        rig.get(&format!("/1.0/networks/{BRIDGE}?project=default"))
            .is_some()
    );
    no_open_intent(&rig);

    // The node is now mixed for good: more containers, a machine on the managed network and one on the bridge, with no
    // further project write.
    create(&handlers, json!({"name":"nats2","template":"nats"})).expect("a second container");
    let ws2 = create(&handlers, json!({"name":"ws2","template":"ws"}))
        .expect("a machine on the managed network");
    assert_eq!(ws2["ipv4"], "192.0.2.12");
    create(
        &handlers,
        json!({"name":"ws3","template":"ws","bridge":"br0"}),
    )
    .expect("a machine on the host bridge");
    assert_eq!(instance(&rig, "ws3")["devices"]["eth0"]["parent"], "br0");
    assert_eq!(
        rig.fake
            .writes()
            .iter()
            .filter(|write| write.path.starts_with("/1.0/projects"))
            .count(),
        2
    );
    assert_eq!(entry(&rig, "service-ws1"), vm_entry);
    assert_eq!(instance(&rig, "ws1"), vm_instance);
    assert_eq!(arca::handlers::mode_refusal(&cfg, &rig.entries()), None);
}

/// A node whose project the network entry made gains virtual machines on the managed network, and then on a host bridge,
/// by that entry's own writes. Its container is untouched.
#[test]
fn a_managed_node_that_gains_virtual_machines_widens_its_own_project() {
    let rig = Rig::managed();
    rig.call("service_create", json!({"name":"nats","template":"nats"}))
        .expect("a container");
    let container_entry = entry(&rig, "service-nats");
    let container = instance(&rig, "nats");
    assert_eq!(project(&rig)["limits.virtual-machines"], "0");

    // With the caps stated, a machine with no bridge joins the managed network like a container.
    let (cfg, handlers) = rig.switched(managed_vm_kernel(roomy()));
    assert_eq!(arca::handlers::mode_refusal(&cfg, &rig.entries()), None);
    assert_eq!(
        refusal(create(
            &handlers,
            json!({"name":"ws1","template":"ws","bridge":"br0"})
        )),
        "bridge: this node allows no host bridge (kernel.bridged is not set); leave bridge out to join the managed service network"
    );
    assert_eq!(
        refusal(create(
            &handlers,
            json!({"name":"ws1","template":"ws","mac":"02:aa:bb:00:00:01"})
        )),
        "mac: a virtual machine on the managed service network has its address reserved by Arca, so it takes no fixed MAC; mac is for a host bridge"
    );
    let created = create(&handlers, json!({"name":"ws1","template":"ws"})).expect("the machine");
    assert_eq!(
        created,
        json!({
            "name": "ws1", "template": "ws", "state": "running", "ipv4": "192.0.2.11",
            "ports": [], "links": [], "published": null, "lanPort": null
        })
    );
    let vm = instance(&rig, "ws1");
    assert_eq!(vm["type"], "virtual-machine");
    assert_eq!(
        vm["profiles"],
        json!([]),
        "the service profile holds container keys"
    );
    assert_eq!(
        vm["devices"],
        json!({
            "root": { "type": "disk", "path": "/", "pool": "pool1", "size": "20GiB" },
            "eth0": {
                "type": "nic", "name": "enp5s0", "network": BRIDGE, "ipv4.address": "192.0.2.11",
                "security.port_isolation": "false", "security.mac_filtering": "true",
                "security.ipv4_filtering": "true", "security.acls": "arcak-svc-svc-ws1",
                "security.acls.default.ingress.action": "reject",
                "security.acls.default.egress.action": "reject"
            },
            "agent": { "type": "disk", "source": "agent:config" }
        }),
        "a container's NIC, on the node's default pool, with the agent's config drive"
    );
    assert_eq!(vm["config"]["user.arca.address"], "192.0.2.11");
    assert_eq!(vm["config"]["limits.cpu"], "2");
    assert_eq!(vm["config"]["agent.nic_config"], "true");
    let acl = rig
        .get("/1.0/network-acls/arcak-svc-svc-ws1?project=default")
        .expect("the machine's own NIC ACL");
    assert_eq!(acl["ingress"], json!([]), "nothing reaches it until a link");
    assert_eq!(
        acl["egress"],
        rig.get("/1.0/network-acls/arcak-svc-svc-nats?project=default")
            .expect("the container's ACL")["egress"],
        "the same egress rules as a container"
    );
    let widened = project(&rig);
    assert_eq!(widened["limits.virtual-machines"], "4");
    assert_eq!(widened["restricted.networks.access"], BRIDGE);
    assert!(widened.get("restricted.devices.nic").is_none());
    assert_eq!(entry(&rig, NET_ENTRY).config["virtualMachines"], 4);

    // The node then allows a host bridge: the same entry lets NICs name one.
    let (cfg, handlers) = rig.switched(mixed_kernel(&["br0"], roomy()));
    assert_eq!(arca::handlers::mode_refusal(&cfg, &rig.entries()), None);
    create(
        &handlers,
        json!({"name":"ws2","template":"ws","bridge":"br0"}),
    )
    .expect("a machine on the host bridge");
    assert_eq!(
        instance(&rig, "ws2")["devices"]["eth0"],
        json!({ "type": "nic", "name": "enp5s0", "nictype": "bridged", "parent": "br0" })
    );
    let widened = project(&rig);
    assert_eq!(widened[KERNEL_ENTRY_KEY], NET_ENTRY);
    assert_eq!(widened["restricted"], "true");
    assert_eq!(widened["restricted.devices.nic"], "allow");
    assert!(widened.get("restricted.networks.access").is_none());
    assert_eq!(
        entry(&rig, NET_ENTRY).config,
        json!({"project":PROJECT,"bridge":BRIDGE,"cidr":"192.0.2.0/24","virtualMachines":4,"hostBridges":true})
    );
    assert!(
        rig.entry_ids().iter().all(|id| id != BRIDGED_ENTRY),
        "the project has one owner"
    );
    assert_eq!(entry(&rig, "service-nats"), container_entry);
    assert_eq!(instance(&rig, "nats"), container);
    no_open_intent(&rig);
}

/// What is sound for a virtual machine on the managed network is allowed, and the rest is refused in words.
#[test]
fn a_container_links_to_a_virtual_machine_and_the_other_verbs_are_refused_in_words() {
    let rig = Rig::with(mixed_kernel(&["br0"], roomy()), None, vec![]);
    rig.template("db", db_template());
    rig.call("service_create", json!({"name":"nats","template":"nats"}))
        .expect("the consumer");
    let db = rig
        .call("service_create", json!({"name":"db","template":"db"}))
        .expect("a machine that offers a port");
    assert_eq!(db["ports"], json!([5432]));
    assert_eq!(db["ipv4"], "192.0.2.11");

    // A template with ports needs the managed network's firewall rules, which a host bridge does not have.
    assert_eq!(
        refusal(rig.call(
            "service_create",
            json!({"name":"db2","template":"db","bridge":"br0"})
        )),
        "template db declares ports, which a link opens in the managed service network's firewall rules: a host bridge has none, so leave bridge out or use a template without ports"
    );

    let linked = rig
        .call("service_connect", json!({"from":"nats","to":"db"}))
        .expect("a container links to the machine");
    assert_eq!(linked, json!({"env":["DB_ADDR"]}));
    let acl = rig
        .get("/1.0/network-acls/arcak-svc-svc-db?project=default")
        .expect("the machine's ACL");
    let ingress = acl["ingress"].as_array().expect("ingress");
    assert_eq!(ingress.len(), 1);
    assert_eq!(ingress[0]["source"], "192.0.2.10/32");
    assert_eq!(ingress[0]["destination_port"], "5432");
    assert_eq!(ingress[0]["action"], "allow");
    assert!(
        instance(&rig, "nats")["config"]
            .get("environment.DB_ADDR")
            .is_some(),
        "the consumer is given the machine's address"
    );
    rig.call("service_disconnect", json!({"from":"nats","to":"db"}))
        .expect("the link is taken away");
    assert_eq!(
        rig.get("/1.0/network-acls/arcak-svc-svc-db?project=default")
            .expect("the machine's ACL")["ingress"],
        json!([])
    );

    assert_eq!(
        refusal(rig.call("service_connect", json!({"from":"db","to":"nats"}))),
        "service db is a virtual machine: a link hands its consumer the connection values as container environment, which a virtual machine's programs never read; link a container to it instead"
    );
    assert_eq!(
        refusal(rig.call(
            "service_publish",
            json!({"name":"db","reach":"tailnet","approval":"x"})
        )),
        "service db is a virtual machine: a dash is published by a tailnet node inside an Arca-built container image, and no virtual-machine image carries one"
    );
    assert_eq!(
        refusal(rig.call(
            "service_secret_set",
            json!({"name":"db","key":"TOKEN","value":"12345678"})
        )),
        "service db is a virtual machine: it has no template secrets or environment for Arca to set"
    );

    // A machine on the host bridge keeps the bridged node's refusals.
    rig.call(
        "service_create",
        json!({"name":"ws1","template":"ws","bridge":"br0"}),
    )
    .expect("a machine on the host bridge");
    assert_eq!(
        refusal(rig.call("service_connect", json!({"from":"nats","to":"ws1"}))),
        "service ws1 is a virtual machine on a host bridge that Arca does not manage: links need the service network and its per-service firewall rules, which it does not have"
    );
    no_open_intent(&rig);
}

/// Holding both kinds is never refused. A config that would drop a network from under what lives on it is.
#[test]
fn the_mode_refusals_protect_against_a_half_state_and_allow_both_kinds() {
    let rig = Rig::with(mixed_kernel(&["br0"], roomy()), None, vec![]);
    rig.call("service_create", json!({"name":"nats","template":"nats"}))
        .expect("a container");
    rig.call("service_create", json!({"name":"ws1","template":"ws"}))
        .expect("a machine on the managed network");
    rig.call(
        "service_create",
        json!({"name":"ws2","template":"ws","bridge":"br0"}),
    )
    .expect("a machine on the host bridge");
    let entries = rig.entries();
    let refuse = |kernel: Value, entries: &[arca_kernel::desired::Entry]| {
        let (cfg, _) = rig.switched(kernel);
        arca::handlers::mode_refusal(&cfg, entries)
    };
    assert_eq!(refuse(mixed_kernel(&["br0"], roomy()), &entries), None);

    // Dropping the managed network leaves the container, the managed machine and the network itself behind.
    assert_eq!(
        refuse(bridged_kernel(&["br0"], roomy()), &entries).as_deref(),
        Some(
            "kernel.bridged is set, but this node already holds net-arcak-svc from the managed service network. A node is bridged from its first service or not at all: remove kernel.bridged, or remove those services first"
        )
    );
    let without = |ids: &[&str]| -> Vec<arca_kernel::desired::Entry> {
        entries
            .iter()
            .filter(|entry| !ids.contains(&entry.id.0.as_str()))
            .cloned()
            .collect()
    };
    assert_eq!(
        refuse(
            bridged_kernel(&["br0"], roomy()),
            &without(&["net-arcak-svc", "service-nats"])
        )
        .as_deref(),
        Some(
            "kernel.bridged is set, but this node already holds service-ws1 from the managed service network. A node is bridged from its first service or not at all: remove kernel.bridged, or remove those services first"
        ),
        "a machine on the managed network needs it as a container does"
    );

    // Dropping kernel.bridged leaves the machine on the host bridge behind.
    assert_eq!(
        refuse(managed_vm_kernel(roomy()), &entries).as_deref(),
        Some(
            "kernel.bridged is not set, but this node already holds service-ws2 from bridged mode. Put kernel.bridged back, or remove its virtual machines first"
        )
    );
    // Once that machine is removed, the node is a managed one that holds a container and a machine: both stay.
    assert_eq!(
        refuse(managed_vm_kernel(roomy()), &without(&["service-ws2"])),
        None
    );
    // Dropping the caps leaves a machine nothing counts.
    assert_eq!(
        refuse(managed_kernel(), &without(&["service-ws2"])).as_deref(),
        Some(
            "this node already holds the virtual machine service-ws1, but its config sets no virtual-machine caps. Put kernel.vmLimits back, or remove its virtual machines first"
        )
    );
    assert_eq!(
        refuse(managed_kernel(), &without(&["service-ws1", "service-ws2"])),
        None,
        "a managed node with containers alone needs no caps"
    );

    // A create checks the same thing again before its first write.
    let (_, handlers) = rig.switched(managed_vm_kernel(roomy()));
    let writes = rig.fake.writes().len();
    assert_eq!(
        refusal(create(&handlers, json!({"name":"ws3","template":"ws"}))),
        "kernel.bridged is not set, but this node already holds service-ws2 from bridged mode. Put kernel.bridged back, or remove its virtual machines first"
    );
    assert_eq!(rig.fake.writes().len(), writes);
}

/// One set of caps covers the machines on both networks, and no container counts against it.
#[test]
fn the_caps_count_virtual_machines_on_both_networks_and_no_container() {
    let two = json!({ "virtualMachines": 2, "cpu": 8, "memory": "16GiB", "disk": "80GiB" });
    let rig = Rig::with(mixed_kernel(&["br0"], two.clone()), None, vec![]);
    for name in ["n1", "n2", "n3"] {
        rig.call("service_create", json!({"name":name,"template":"nats"}))
            .expect("a container");
    }
    rig.call("service_create", json!({"name":"ws1","template":"ws"}))
        .expect("a machine on the managed network");
    rig.call(
        "service_create",
        json!({"name":"ws2","template":"ws","bridge":"br0"}),
    )
    .expect("a machine on the host bridge");
    let entries = rig.entry_ids();
    for args in [
        json!({"name":"ws3","template":"ws"}),
        json!({"name":"ws3","template":"ws","bridge":"br0"}),
    ] {
        assert_eq!(
            refusal(rig.call("service_create", args)),
            "creating ws3 would exceed this node's cap on virtual machines (kernel.bridged.limits.virtualMachines): 2 in use, 1 asked, 2 allowed"
        );
    }
    assert_eq!(rig.entry_ids(), entries, "a refused create leaves nothing");
    rig.call("service_create", json!({"name":"n4","template":"nats"}))
        .expect("a container is outside the caps");
    // Incus holds the count too, and nothing else: its other limits would count the containers.
    let config = project(&rig);
    assert_eq!(config["limits.virtual-machines"], "2");
    for absent in [
        "limits.cpu",
        "limits.memory",
        "limits.disk",
        "limits.containers",
    ] {
        assert!(config.get(absent).is_none(), "{absent}");
    }

    // A managed node names its own key in the refusal, and counts memory the same way.
    let tight = json!({ "virtualMachines": 4, "cpu": 8, "memory": "6GiB", "disk": "80GiB" });
    let managed = Rig::with(managed_vm_kernel(tight), None, vec![]);
    managed
        .call("service_create", json!({"name":"ws1","template":"ws"}))
        .expect("the first machine");
    assert_eq!(
        refusal(managed.call("service_create", json!({"name":"ws2","template":"ws"}))),
        "creating ws2 would exceed this node's cap on memory (kernel.vmLimits.memory): 4.0 GiB in use, 4.0 GiB asked, 6.0 GiB allowed"
    );
}

/// F1 and F2: on either network the machine gets the agent's config share as a drive, and a NIC the agent renames to
/// the one name the public images configure.
#[test]
fn every_virtual_machine_carries_the_agent_disk_and_a_nic_name_the_image_configures() {
    let rig = Rig::with(mixed_kernel(&["br0"], roomy()), None, vec![]);
    rig.call("service_create", json!({"name":"ws1","template":"ws"}))
        .expect("managed");
    rig.call(
        "service_create",
        json!({"name":"ws2","template":"ws","bridge":"br0"}),
    )
    .expect("host bridge");
    for name in ["ws1", "ws2"] {
        let vm = instance(&rig, name);
        assert_eq!(
            vm["devices"]["agent"],
            json!({ "type": "disk", "source": "agent:config" }),
            "{name}"
        );
        assert_eq!(vm["devices"]["eth0"]["name"], "enp5s0", "{name}");
        assert_eq!(vm["config"]["agent.nic_config"], "true", "{name}");
    }
    // The service project is restricted, and the fake refuses a disk with no pool as Incus does, except this drive.
    assert_eq!(project(&rig)["restricted"], "true");
}

/// F4: an approved create whose start fails tells the owner why, in the answer and in the proposals journal, and leaves a
/// state the message names.
#[test]
fn an_approved_create_that_fails_at_start_reports_its_message_and_leaves_a_defined_state() {
    let key = common::SkKey::from_seed(9);
    let rig = Rig::with(mixed_kernel(&["br0"], roomy()), Some(&key), vec![]);
    rig.fake.fail_instance_start_on_create();
    let proposal = rig
        .kernel
        .lock()
        .expect("kernel")
        .propose_action(
            ActionKind::ServiceCreate,
            json!({"name":"ws1","template":"ws"}),
            NOW,
            None,
        )
        .expect("the proposal");
    let statement = action_statement(
        "node1",
        &proposal.id,
        "service_create",
        &proposal.args_digest,
        NOW,
        &"7".repeat(32),
    )
    .expect("the statement");
    let approval = common::approve_statement(&key, &statement);
    let executor = ActionExecutor::new(
        rig.handlers
            .iter()
            .map(|(verb, handler)| (*verb, Arc::clone(handler))),
    );
    let message = "The approval was accepted, but service_create did not complete: start service ws1: the instance was created, and Incus did not start it: Failed starting instance. Its instance exists and is stopped: instance_start starts it once the cause is fixed, and service_remove removes it";
    match arca::handlers::run_approved_action(
        &rig.kernel,
        &executor,
        &json!({ "plan": proposal.id, "approval": approval }),
        NOW,
    ) {
        Err(CallError::Refused { code, message: got }) => {
            assert_eq!(code, "refused");
            assert_eq!(got, message);
        }
        other => panic!("{other:?}"),
    }
    assert!(message.starts_with(arca::handlers::APPROVED_BUT_FAILED));

    // The proposals journal keeps the message beside the code.
    let journal =
        fs::read_to_string(rig.dir.join("action-proposals.jsonl")).expect("the proposals journal");
    let result: Value = journal
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|event| event["event"] == "result")
        .expect("a result event");
    assert_eq!(result["ok"], false);
    assert_eq!(result["code"], "refused");
    assert_eq!(result["message"], message);

    // The defined state: the record stays, the instance exists stopped, and the effect journal holds its create as
    // applied.
    assert_eq!(instance(&rig, "ws1")["status"], "Stopped");
    assert_eq!(
        rig.call("service_status", json!({"name":"ws1"}))
            .expect("status")["state"],
        "stopped"
    );
    no_open_intent(&rig);
}

/// A machine on the managed network is removed as a container is: on the owner's approval, with its NIC ACL, and its
/// name is free again.
#[test]
fn a_virtual_machine_on_the_managed_network_is_removed_with_its_acl_on_the_owners_approval() {
    let key = common::SkKey::from_seed(9);
    let rig = Rig::with(mixed_kernel(&["br0"], roomy()), Some(&key), vec![]);
    let created = rig
        .call("service_create", json!({"name":"ws1","template":"ws"}))
        .expect("create");
    assert_eq!(created["ipv4"], "192.0.2.10");
    let acl = "/1.0/network-acls/arcak-svc-svc-ws1?project=default";
    assert!(rig.get(acl).is_some());
    assert!(matches!(
        rig.call(
            "service_remove",
            json!({"name":"ws1","approval":"not-an-approval"})
        ),
        Err(CallError::Refused { code, .. }) if code == "owner_approval"
    ));
    assert_eq!(instance(&rig, "ws1")["status"], "Running");
    let removed = rig
        .call(
            "service_remove",
            json!({"name":"ws1","approval":owner_approval(&key, "ws1")}),
        )
        .expect("remove");
    assert_eq!(removed, json!({"removed": true}));
    assert!(
        rig.get(&format!("/1.0/instances/ws1?project={PROJECT}"))
            .is_none(),
        "the machine is gone"
    );
    assert!(rig.get(acl).is_none(), "its ACL is gone");
    assert_eq!(rig.entry_ids(), [NET_ENTRY], "its record is gone");
    let again = rig
        .call("service_create", json!({"name":"ws1","template":"ws"}))
        .expect("the name is free again");
    // The address is the plan's next one: a lease Incus still holds for the removed machine keeps its old address out.
    assert!(again["ipv4"] == "192.0.2.10" || again["ipv4"] == "192.0.2.11");
    no_open_intent(&rig);
}
