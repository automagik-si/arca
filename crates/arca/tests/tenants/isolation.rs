//! Two tenants beside the node's own project: what each sees, what each may reach, and what a removal touches.
use super::*;

fn create(rig: &Rig, name: &str, template: &str, tenant: Option<&str>) -> Value {
    let mut args = json!({ "name": name, "template": template });
    if let Some(tenant) = tenant {
        args["tenant"] = json!(tenant);
    }
    rig.call("service_create", args)
        .unwrap_or_else(|error| panic!("create {name} in {tenant:?}: {error:?}"))
}

/// The name of a service's Incus instance: its own in the node's project, and `<name>-<id>` in a tenant's.
fn incus_name(project: &str, name: &str) -> String {
    match project.strip_prefix("arcak-t-") {
        Some(id) => format!("{name}-{id}"),
        None => name.to_string(),
    }
}

fn instance_path(project: &str, name: &str) -> String {
    format!(
        "/1.0/instances/{}?project={project}",
        incus_name(project, name)
    )
}

fn names(listing: &Value) -> Vec<String> {
    listing["services"]
        .as_array()
        .expect("services")
        .iter()
        .map(|row| row["name"].as_str().unwrap().to_string())
        .collect()
}

fn acl(rig: &Rig, project: &str, name: &str) -> Value {
    rig.get(&format!(
        "/1.0/network-acls/{project}-svc-{name}?project=default"
    ))
    .unwrap_or_else(|| panic!("the ACL of {name} in {project}"))
}

/// Every object of one project and of its services' ACLs, as Incus holds it.
fn objects(rig: &Rig, project: &str, services: &[&str]) -> Vec<Value> {
    let mut found = vec![
        rig.get(&format!("/1.0/projects/{project}"))
            .expect("project"),
    ];
    for name in services {
        found.push(
            rig.get(instance_path(project, name).as_str())
                .unwrap_or_else(|| panic!("instance {name} in {project}")),
        );
        found.push(acl(rig, project, name));
    }
    found
}

#[test]
fn each_side_sees_and_changes_only_its_own_services() {
    let rig = two_tenants(policy());
    // The same name on all three sides: one record and one instance each.
    create(&rig, "web", "app", None);
    create(&rig, "web", "app", Some(A));
    create(&rig, "web", "app", Some(B));
    create(&rig, "only-b", "app", Some(B));
    create(&rig, "own", "app", None);
    for (project, tenant) in [
        (PROJECT.to_string(), None),
        (project(A), Some(A)),
        (project(B), Some(B)),
    ] {
        assert!(
            rig.get(instance_path(&project, "web").as_str()).is_some(),
            "{tenant:?}"
        );
    }
    let ids: Vec<String> = rig.entries().into_iter().map(|entry| entry.id.0).collect();
    for id in [
        "service-web",
        "t-aaaaaaaa-service-web",
        "t-bbbbbbbb-service-web",
    ] {
        assert!(ids.iter().any(|found| found == id), "{id} in {ids:?}");
    }
    // Every read shows its own side and nothing else.
    assert_eq!(
        names(&rig.call("service_list", json!({ "tenant": A })).unwrap()),
        ["web"]
    );
    assert_eq!(
        names(&rig.call("service_list", json!({ "tenant": B })).unwrap()),
        ["only-b", "web"]
    );
    assert_eq!(
        names(&rig.call("service_list", json!({})).unwrap()),
        ["own", "web"]
    );
    let status = |tenant: Option<&str>| {
        let mut args = json!({ "name": "web" });
        if let Some(tenant) = tenant {
            args["tenant"] = json!(tenant);
        }
        rig.call("service_status", args).unwrap()["ipv4"]
            .as_str()
            .unwrap()
            .to_string()
    };
    // Each web is its own: an address of its own side's range.
    assert_eq!(status(Some(A)), "192.0.2.248");
    assert_eq!(status(Some(B)), "192.0.2.240");
    assert_eq!(status(None), "192.0.2.10");
    // A name of another side answers exactly as a name that does not exist.
    for verb in ["service_status", "service_logs"] {
        let other = refusal(rig.call(verb, json!({ "name": "only-b", "tenant": A })));
        let missing = refusal(rig.call(verb, json!({ "name": "nothing", "tenant": A })));
        assert_eq!(other.0, "not_found", "{verb}");
        assert_eq!(
            other.1.replace("only-b", "X"),
            missing.1.replace("nothing", "X"),
            "{verb}"
        );
        let own = refusal(rig.call(verb, json!({ "name": "own", "tenant": A })));
        assert_eq!(
            own.1.replace("own", "X"),
            missing.1.replace("nothing", "X"),
            "{verb}"
        );
        // The node's own side does not see into a tenant either.
        let inward = refusal(rig.call(verb, json!({ "name": "only-b" })));
        assert_eq!(inward.0, "not_found", "{verb}");
    }
    // Every write on another side's name is "not found", and writes nothing.
    let before = rig.fake().writes().len();
    let entries = rig.entries();
    for (verb, args) in [
        (
            "service_remove",
            json!({ "name": "only-b", "approval": "x", "tenant": A }),
        ),
        (
            "service_remove",
            json!({ "name": "own", "approval": "x", "tenant": A }),
        ),
        (
            "service_connect",
            json!({ "from": "web", "to": "only-b", "tenant": A }),
        ),
        (
            "service_connect",
            json!({ "from": "only-b", "to": "web", "tenant": A }),
        ),
        (
            "service_connect",
            json!({ "from": "web", "to": "own", "tenant": A }),
        ),
        (
            "service_disconnect",
            json!({ "from": "web", "to": "only-b", "tenant": A }),
        ),
        (
            "service_secret_set",
            json!({ "name": "only-b", "key": "K", "value": "12345678", "tenant": A }),
        ),
        ("service_connect", json!({ "from": "own", "to": "only-b" })),
    ] {
        let (code, message) = refusal(rig.call(verb, args.clone()));
        assert_eq!(code, "not_found", "{verb} {args}: {message}");
    }
    // A tenant the node does not hold is not found either, and a malformed id is refused as an argument.
    let (code, message) = refusal(rig.call("service_list", json!({ "tenant": "cccccccc" })));
    assert_eq!(
        (code.as_str(), message.as_str()),
        ("not_found", "tenant cccccccc: not found")
    );
    assert!(matches!(
        rig.call("service_list", json!({ "tenant": "Not-An-Id" })),
        Err(CallError::Invalid(_))
    ));
    assert_eq!(
        rig.fake().writes().len(),
        before,
        "no refusal wrote anything"
    );
    assert_eq!(rig.entries(), entries);
    // What stays on the node only: nothing in a tenant is published, imported or released.
    for verb in [
        "service_publish",
        "service_unpublish",
        "service_import",
        "service_release",
    ] {
        let mut args = json!({ "name": "web", "tenant": A, "approval": "x" });
        if verb == "service_publish" {
            args["reach"] = json!("tailnet");
        }
        if verb == "service_unpublish" {
            args.as_object_mut().unwrap().remove("approval");
        }
        // The field is not theirs: the schema does not name it, and the call is refused as an argument.
        assert!(
            matches!(rig.call(verb, args), Err(CallError::Invalid(_))),
            "{verb}"
        );
    }
}

#[test]
fn addresses_come_from_each_tenants_block_and_the_nodes_own_plan_skips_them() {
    let rig = two_tenants(policy());
    assert_eq!(rig.entry("tenant-aaaaaaaa")["block"], "192.0.2.248/29");
    assert_eq!(rig.entry("tenant-bbbbbbbb")["block"], "192.0.2.240/29");
    let a: Vec<String> = ["one", "two", "three"]
        .iter()
        .map(|name| {
            create(&rig, name, "app", Some(A))["ipv4"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(a, ["192.0.2.248", "192.0.2.249", "192.0.2.250"]);
    assert_eq!(create(&rig, "one", "app", Some(B))["ipv4"], "192.0.2.240");
    assert_eq!(create(&rig, "one", "app", None)["ipv4"], "192.0.2.10");
    // An address in use anywhere on the bridge is never handed out again, and a block is never laid over one.
    let nic = rig.get(instance_path(&project(A), "one").as_str()).unwrap();
    let nic = &nic["devices"]["eth0"];
    assert_eq!(nic["network"], "arcak-svcbr");
    assert_eq!(nic["ipv4.address"], "192.0.2.248");
    assert_eq!(nic["security.mac_filtering"], "true");
    assert_eq!(nic["security.ipv4_filtering"], "true");
    // A tenant's NIC filters IPv6 too; a NIC of the node's own project is what it always was.
    assert_eq!(nic["security.ipv6_filtering"], "true");
    let own = rig.get(instance_path(PROJECT, "one").as_str()).unwrap();
    assert_eq!(own["devices"]["eth0"]["security.ipv4_filtering"], "true");
    assert!(
        own["devices"]["eth0"]
            .get("security.ipv6_filtering")
            .is_none()
    );
    assert_eq!(nic["security.port_isolation"], "false");
    assert_eq!(nic["security.acls"], "arcak-t-aaaaaaaa-svc-one");
    assert_eq!(nic["security.acls.default.ingress.action"], "reject");
    assert_eq!(nic["security.acls.default.egress.action"], "reject");
    // A third tenant's block sits under the first two, and under no address in use.
    rig.create_tenant(json!({ "id": "cccccccc" })).unwrap();
    assert_eq!(rig.entry("tenant-cccccccc")["block"], "192.0.2.232/29");
}

#[test]
fn the_nodes_own_plan_never_hands_out_an_address_inside_a_tenants_block() {
    // A block low in the plan's range: the node's own services step over it.
    let mut wide = policy();
    wide["maxTenants"] = json!(30);
    wide["capacity"] = json!({ "instances": 200, "cpu": 512, "memory": "1TiB", "disk": "8TiB" });
    wide["defaultCaps"]["virtualMachines"] = json!(0);
    wide["createRate"] = json!({ "count": 100, "perSeconds": 3600 });
    let rig = Rig::new(Some(wide));
    rig.template("app", app_template("app"));
    // Twenty-nine tenants fill the subnet from the top down to .24; the last one's block is .24/29.
    let ids: Vec<String> = (0..29)
        .map(|n| {
            format!(
                "aaaaaa{}{}",
                (b'a' + n / 26) as char,
                (b'a' + n % 26) as char
            )
        })
        .collect();
    for id in &ids {
        rig.create_tenant(json!({ "id": id })).expect("tenant");
    }
    assert_eq!(
        rig.entry(&format!("tenant-{}", ids[28]))["block"],
        "192.0.2.24/29"
    );
    // The node's own plan is .10 to .239 without the blocks: .10 to .23 is what is left.
    let own: Vec<String> = (0..14)
        .map(|n| {
            create(&rig, &format!("own{n}"), "app", None)["ipv4"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(own[0], "192.0.2.10");
    assert_eq!(own[13], "192.0.2.23");
    let (_, message) = refusal(rig.call(
        "service_create",
        json!({ "name": "own14", "template": "app" }),
    ));
    assert!(message.contains("has no free address"), "{message}");
    // The next tenant finds one block left (.16/29 holds addresses in use), and then none.
    let (_, message) = refusal(rig.create_tenant(json!({ "id": "bbbbbbbb" })));
    assert!(
        message.contains("has no free /29 block for a tenant"),
        "{message}"
    );
}

#[test]
fn a_tenants_nic_reaches_its_own_block_and_the_internet_and_nothing_else_of_the_subnet() {
    let rig = two_tenants(policy());
    create(&rig, "web", "app", Some(A));
    create(&rig, "web", "app", None);
    let rules = acl(&rig, &project(A), "web");
    let egress = rules["egress"].as_array().expect("egress");
    // The service subnet outside the tenant's /29 is rejected: together the ranges are the /24 without the block.
    let outside = egress
        .iter()
        .find(|rule| rule["description"] == "the service subnet outside this tenant's block")
        .expect("the tenant's reject rule");
    assert_eq!(outside["action"], "reject");
    assert_eq!(
        outside["destination"],
        "192.0.2.0/25,192.0.2.128/26,192.0.2.192/27,192.0.2.224/28,192.0.2.240/29"
    );
    // DHCP and DNS on the gateway are the only things named as allowed inside the subnet.
    let allowed: Vec<(String, String, String)> = egress
        .iter()
        .filter(|rule| rule["action"] == "allow" && rule["destination"] != "")
        .map(|rule| {
            (
                rule["destination"].as_str().unwrap().to_string(),
                rule["protocol"].as_str().unwrap().to_string(),
                rule["destination_port"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let gateway = "192.0.2.1/32".to_string();
    assert_eq!(
        allowed,
        [
            (gateway.clone(), "udp".to_string(), "67".to_string()),
            (gateway.clone(), "udp".to_string(), "53".to_string()),
            (gateway, "tcp".to_string(), "53".to_string()),
        ]
    );
    // Everything else is as it is for a service of the node's own project: the same rules, after the tenant's own.
    let own = acl(&rig, PROJECT, "web");
    let own_egress = own["egress"].as_array().expect("egress");
    assert_eq!(&egress[4..], &own_egress[..]);
    assert_eq!(own_egress.len(), 5);
    // No ingress at all: nothing links to it, and the node names no front.
    assert_eq!(rules["ingress"], json!([]));
    assert_eq!(own["ingress"], json!([]));
}

#[test]
fn the_front_reaches_every_declared_port_of_a_tenants_service_and_nothing_else_does() {
    let mut fronted = policy();
    fronted["front"] = json!("edge");
    let rig = two_tenants(fronted);
    let edge = create(&rig, "edge", "app", None)["ipv4"]
        .as_str()
        .unwrap()
        .to_string();
    let other = create(&rig, "other", "app", None)["ipv4"]
        .as_str()
        .unwrap()
        .to_string();
    create(&rig, "web", "app", Some(A));
    rig.template("vm", vm_template());
    create(&rig, "box", "vm", Some(A));
    let ingress = acl(&rig, &project(A), "web")["ingress"].clone();
    let ingress = ingress.as_array().expect("ingress");
    assert_eq!(ingress.len(), 1, "{ingress:?}");
    // The one rule: the front's address, tcp, every port the template declares.
    assert_eq!(ingress[0]["action"], "allow");
    assert_eq!(ingress[0]["source"], format!("{edge}/32"));
    assert_eq!(ingress[0]["protocol"], "tcp");
    assert_eq!(ingress[0]["destination_port"], "8080,9090");
    // No other service of the node's project is named anywhere in the tenant's rules.
    assert!(!ingress[0].to_string().contains(&other));
    // A virtual machine's declared port is admitted the same way.
    let vm = acl(&rig, &project(A), "box")["ingress"].clone();
    assert_eq!(vm[0]["source"], format!("{edge}/32"));
    assert_eq!(vm[0]["destination_port"], "22");
    // The front itself is a service like any other: it gained no rule.
    assert_eq!(acl(&rig, PROJECT, "edge")["ingress"], json!([]));
    // A node that names no front admits nothing: see the test above. A service with no port admits nothing either.
    rig.template(
        "quiet",
        json!({
            "name": "quiet", "kind": "oci", "image": "docker.io/library/app", "digest": DIGEST,
            "ports": [], "env": {}, "publishable": false
        }),
    );
    create(&rig, "quiet", "quiet", Some(A));
    assert_eq!(acl(&rig, &project(A), "quiet")["ingress"], json!([]));
}

#[test]
fn a_link_joins_two_services_of_one_tenant_and_opens_one_port() {
    let rig = two_tenants(policy());
    create(&rig, "api", "app", Some(A));
    create(&rig, "db", "app", Some(A));
    create(&rig, "db", "app", Some(B));
    let linked = rig
        .call(
            "service_connect",
            json!({ "from": "api", "to": "db", "tenant": A }),
        )
        .expect("a link inside the tenant");
    assert_eq!(linked["env"], json!(["APP_ADDR"]));
    // The provider in A admits the consumer in A on its first port.
    let ingress = acl(&rig, &project(A), "db")["ingress"].clone();
    assert_eq!(ingress.as_array().map(Vec::len), Some(1));
    assert_eq!(ingress[0]["source"], "192.0.2.248/32");
    assert_eq!(ingress[0]["destination_port"], "8080");
    // The database of the same name in B is not touched, and the consumer was handed A's address.
    assert_eq!(acl(&rig, &project(B), "db")["ingress"], json!([]));
    let api = rig.entry("t-aaaaaaaa-service-api");
    assert_eq!(api["links"][0]["to"], "db");
    let reference = api["environment"]["APP_ADDR"].as_str().unwrap();
    assert!(
        reference.starts_with("arca-secret:t-aaaaaaaa-service-api/"),
        "{reference}"
    );
    let value = fs::read_to_string(
        rig.dir
            .join("secrets")
            .join(reference.strip_prefix("arca-secret:").unwrap()),
    )
    .unwrap();
    assert_eq!(value, "192.0.2.249:8080");
    // Removing A's db revokes the link in A and nothing in B.
    rig.approved("service_remove", json!({ "name": "db", "tenant": A }))
        .expect("the approved removal");
    assert_eq!(rig.entry("t-aaaaaaaa-service-api")["links"], json!([]));
    assert!(rig.get(instance_path(&project(B), "db").as_str()).is_some());
    assert!(rig.get(instance_path(&project(A), "db").as_str()).is_none());
    assert!(
        rig.get("/1.0/network-acls/arcak-t-aaaaaaaa-svc-db?project=default")
            .is_none()
    );
    rig.call(
        "service_disconnect",
        json!({ "from": "api", "to": "db", "tenant": A }),
    )
    .expect_err("the provider is gone");
}

#[test]
fn a_tenants_caps_are_checked_before_incus_and_the_refusal_names_the_cap() {
    let mut small = policy();
    small["defaultCaps"] = json!({ "instances": 3, "containers": 2, "virtualMachines": 1, "cpu": 6,
        "memory": "8GiB", "disk": "50GiB" });
    let rig = two_tenants(small);
    let refused = |name: &str, template: &str, tenant: &str| {
        let writes = rig.fake().writes().len();
        let (_, message) = refusal(rig.call(
            "service_create",
            json!({ "name": name, "template": template, "tenant": tenant }),
        ));
        assert_eq!(rig.fake().writes().len(), writes, "nothing reached Incus");
        message
    };
    create(&rig, "one", "app", Some(A));
    create(&rig, "two", "app", Some(A));
    assert_eq!(
        refused("three", "app", A),
        "creating three would exceed tenant aaaaaaaa's cap on containers (limits.containers): 2 in use, 1 asked, 2 allowed"
    );
    // Two containers hold 32 GiB of root disk: a machine of 20 GiB does not fit in 50.
    assert_eq!(
        refused("box", "vm", A),
        "creating box would exceed tenant aaaaaaaa's cap on disk (limits.disk): 32.0 GiB in use, 20.0 GiB asked, 50.0 GiB allowed"
    );
    // B is untouched by A's use: its own caps are whole.
    create(&rig, "box", "vm", Some(B));
    assert_eq!(
        refused("box2", "vm", B),
        "creating box2 would exceed tenant bbbbbbbb's cap on virtual machines (limits.virtual-machines): 1 in use, 1 asked, 1 allowed"
    );
    create(&rig, "one", "app", Some(B));
    // A tenant of one instance: the second is over its count, whatever else it has room for.
    let tiny = json!({ "instances": 1, "containers": 1, "virtualMachines": 0, "cpu": 6,
        "memory": "8GiB", "disk": "50GiB" });
    rig.create_tenant(json!({ "id": "cccccccc", "caps": tiny }))
        .unwrap();
    create(&rig, "one", "app", Some("cccccccc"));
    assert_eq!(
        refused("two", "app", "cccccccc"),
        "creating two would exceed tenant cccccccc's cap on instances (limits.instances): 1 in use, 1 asked, 1 allowed"
    );
    assert_eq!(
        refused("box", "vm", "cccccccc"),
        "creating box would exceed tenant cccccccc's cap on instances (limits.instances): 1 in use, 1 asked, 1 allowed"
    );
    // CPUs and memory are counted the same way: the machine's two CPUs and 4 GiB, the container's profile defaults.
    let status = rig.call("tenant_status", json!({ "id": B })).unwrap();
    assert_eq!(status["use"]["instances"], 2);
    assert_eq!(status["use"]["virtualMachines"], 1);
    assert_eq!(status["use"]["cpu"], 4);
    assert_eq!(status["use"]["memoryBytes"], 6u64 << 30);
    assert_eq!(status["use"]["diskBytes"], 36u64 << 30);
    assert_eq!(status["instances"], 2);
    assert_eq!(
        refused("two", "app", B),
        "creating two would exceed tenant bbbbbbbb's cap on disk (limits.disk): 36.0 GiB in use, 16.0 GiB asked, 50.0 GiB allowed"
    );
}

#[test]
fn a_tenants_machine_sits_on_the_managed_network_and_the_nodes_machine_cap_covers_every_tenant() {
    let rig = two_tenants(policy());
    // A host bridge would put a tenant on the host's own network: refused in words, before any write.
    let writes = rig.fake().writes().len();
    let (_, message) = refusal(rig.call(
        "service_create",
        json!({ "name": "box", "template": "vm", "tenant": A, "bridge": "br0" }),
    ));
    assert_eq!(
        message,
        "bridge: a virtual machine in a tenant joins the managed service network only, never a host bridge"
    );
    assert_eq!(rig.fake().writes().len(), writes);
    let made = create(&rig, "box", "vm", Some(A));
    assert_eq!(made["ipv4"], "192.0.2.248");
    let instance = rig.get(instance_path(&project(A), "box").as_str()).unwrap();
    assert_eq!(instance["type"], "virtual-machine");
    // Rule 59: the agent's config disk and the NIC name the images configure.
    assert_eq!(instance["devices"]["agent"]["source"], "agent:config");
    assert_eq!(instance["devices"]["eth0"]["name"], "enp5s0");
    assert_eq!(instance["devices"]["eth0"]["network"], "arcak-svcbr");
    assert_eq!(
        instance["devices"]["eth0"]["security.acls"],
        "arcak-t-aaaaaaaa-svc-box"
    );
    assert_eq!(
        instance["devices"]["eth0"]["security.ipv6_filtering"],
        "true"
    );
    // Rule 58: the node's cap of four machines holds the two tenants' one each, so the node's own project has two.
    create(&rig, "own1", "vm", None);
    create(&rig, "own2", "vm", None);
    let (_, message) = refusal(rig.call(
        "service_create",
        json!({ "name": "own3", "template": "vm" }),
    ));
    assert!(
        message.contains("would exceed this node's cap on virtual machines (kernel.vmLimits.virtualMachines): 4 in use, 1 asked, 4 allowed"),
        "{message}"
    );
    // A machine of the node's own project keeps the NIC it always had: no IPv6 filter.
    let own = rig.get(instance_path(PROJECT, "own1").as_str()).unwrap();
    assert_eq!(own["type"], "virtual-machine");
    assert!(
        own["devices"]["eth0"]
            .get("security.ipv6_filtering")
            .is_none()
    );
    // B's own machine is inside its cap, which was set aside for it.
    create(&rig, "box", "vm", Some(B));
}

#[test]
fn the_database_verbs_refuse_a_tenant_and_name_the_service_route() {
    let rig = two_tenants(policy());
    let writes = rig.fake().writes().len();
    let entries = rig.entries();
    for (verb, args) in [
        (
            "postgres_create",
            json!({ "name": "db", "image": PG_IMAGE, "tenant": A }),
        ),
        (
            "postgres_link",
            json!({ "name": "db", "consumer": "app", "tenant": A }),
        ),
        (
            "postgres_unlink",
            json!({ "name": "db", "consumer": "app", "tenant": A }),
        ),
        ("postgres_delete", json!({ "name": "db", "tenant": A })),
        ("postgres_list", json!({ "tenant": A })),
    ] {
        let (code, message) = refusal(rig.call(verb, args));
        assert_eq!(code, "refused", "{verb}");
        assert_eq!(
            message,
            format!(
                "{verb}: inside a tenant, a database is the postgres service template, created with service_create and linked with service_connect; the postgres verbs act in the node's own project only"
            )
        );
    }
    // The tenant is answered for first, as by every verb: an unknown tenant is not found, a malformed id is an
    // argument error.
    for verb in ["postgres_list", "postgres_create"] {
        let (code, message) = refusal(rig.call(
            verb,
            json!({ "name": "db", "image": PG_IMAGE, "tenant": "cccccccc" }),
        ));
        assert_eq!(
            (code.as_str(), message.as_str()),
            ("not_found", "tenant cccccccc: not found"),
            "{verb}"
        );
        assert!(matches!(
            rig.call(
                verb,
                json!({ "name": "db", "image": PG_IMAGE, "tenant": "Not-An-Id" })
            ),
            Err(CallError::Invalid(_))
        ));
    }
    // A proposal is refused the same way when it is made: it is never recorded, and no card is shown for it.
    let (code, message) = refusal(rig.call(
        "action_propose",
        json!({ "action": "postgres_create", "args": { "name": "db", "tenant": A } }),
    ));
    assert_eq!(code, "refused");
    assert_eq!(
        message,
        "postgres_create: inside a tenant, a database is the postgres service template, created with service_create and linked with service_connect; the postgres verbs act in the node's own project only"
    );
    let (code, _) = refusal(rig.call(
        "action_propose",
        json!({ "action": "postgres_create", "args": { "name": "db", "tenant": "cccccccc" } }),
    ));
    assert_eq!(code, "not_found");
    assert!(rig.kernel.lock().unwrap().action_proposals().is_empty());
    assert_eq!(
        rig.call("plan_list", json!({})).unwrap()["plans"],
        json!([])
    );
    // Nothing was written for any of them: no request, no record, no password file.
    assert_eq!(rig.fake().writes().len(), writes);
    assert_eq!(rig.entries(), entries);
    assert!(
        !rig.dir.join("secrets").exists(),
        "no secret was written before the refusal"
    );
    // The route that works: a database service of the tenant, linked to an app of the same tenant.
    create(&rig, "db", "app", Some(A));
    create(&rig, "api", "app", Some(A));
    rig.call(
        "service_connect",
        json!({ "from": "api", "to": "db", "tenant": A }),
    )
    .expect("the link to the database service");
    assert_eq!(
        acl(&rig, &project(A), "db")["ingress"][0]["source"],
        "192.0.2.249/32"
    );
    // Outside a tenant the database verbs are what they were.
    let created = rig
        .call(
            "postgres_create",
            json!({ "name": "db", "image": PG_IMAGE }),
        )
        .expect("a database in the node's own project");
    assert_eq!(created["entry"], "postgres-db");
}

#[test]
fn usage_reports_what_each_instance_is_allowed_and_the_instance_verbs_act_in_one_tenant() {
    let rig = two_tenants(policy());
    create(&rig, "web", "app", Some(A));
    create(&rig, "box", "vm", Some(A));
    create(&rig, "web", "app", Some(B));
    let usage = rig.call("tenant_usage", json!({ "id": A })).unwrap();
    assert_eq!(
        usage,
        json!({ "id": A, "instances": [
            { "name": "box-aaaaaaaa", "service": "box", "kind": "vm",
              "state": usage["instances"][0]["state"],
              "limitsCpu": "2", "limitsMemory": "4GiB", "rootDiskSize": "20GiB" },
            { "name": "web-aaaaaaaa", "service": "web", "kind": "container",
              "state": usage["instances"][1]["state"],
              "limitsCpu": "2", "limitsMemory": "2GiB", "rootDiskSize": "16GiB" }
        ] })
    );
    for row in usage["instances"].as_array().unwrap() {
        let keys: Vec<&String> = row.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            [
                "kind",
                "limitsCpu",
                "limitsMemory",
                "name",
                "rootDiskSize",
                "service",
                "state"
            ],
            "exactly these, and no time"
        );
    }
    // An instance in a tenant is named `<service>-<tenant id>`: one bridge holds one instance of a name. Inside a
    // tenant the three instance verbs need no approval, from a token whose tool allowlist names the verb.
    let verbs = ["instance_start", "instance_stop", "instance_restart"];
    let state = |project: &str| rig.get(&instance_path(project, "web")).unwrap()["status"].clone();
    let stopped = rig
        .call_token(
            "economy",
            "instance_stop",
            json!({ "tenant": A, "service": "web" }),
            &verbs,
        )
        .expect("a stop in the tenant, by its service's name");
    assert_eq!(stopped["project"], project(A));
    assert_eq!(stopped["instance"], "web-aaaaaaaa");
    assert_eq!(stopped["action"], "stop");
    assert_eq!(state(&project(A)), "Stopped");
    assert_ne!(
        state(&project(B)),
        "Stopped",
        "B's instance of the same name is not touched"
    );
    let started = rig
        .call_token(
            "cloud",
            "instance_start",
            json!({ "tenant": A, "instance": "web-aaaaaaaa" }),
            &verbs,
        )
        .expect("a start in the tenant, by its instance's name");
    assert_eq!(started["project"], project(A));
    assert_ne!(state(&project(A)), "Stopped");
    rig.call_token(
        "cloud",
        "instance_restart",
        json!({ "tenant": A, "service": "web" }),
        &verbs,
    )
    .expect("a restart in the tenant");
    // Each is journaled with the token and the tenant.
    let journal = fs::read_to_string(rig.dir.join(arca::lifecycle::INSTANCE_ACTIONS_FILE)).unwrap();
    let begun: Vec<Value> = journal
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["kind"] == "begin")
        .collect();
    let seen: Vec<(&str, &str, &str, &str)> = begun
        .iter()
        .map(|row| {
            (
                row["action"].as_str().unwrap(),
                row["token_id"].as_str().unwrap(),
                row["tenant"].as_str().unwrap(),
                row["instance"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        seen,
        [
            ("stop", "economy", A, "web-aaaaaaaa"),
            ("start", "cloud", A, "web-aaaaaaaa"),
            ("restart", "cloud", A, "web-aaaaaaaa"),
        ]
    );
    assert!(begun.iter().all(|row| row["proposal"].is_null()));
    // A token whose allowlist does not name the verb is refused, and so is one with no allowlist.
    for verb in verbs {
        let (code, message) = refusal(rig.call(verb, json!({ "tenant": A, "service": "web" })));
        assert_eq!(code, "NotAllowed", "{verb}: {message}");
        let (code, _) = refusal(rig.call_token(
            "cloud",
            verb,
            json!({ "tenant": A, "service": "web" }),
            &["service_list"],
        ));
        assert_eq!(code, "NotAllowed", "{verb}");
    }
    // A name that only another tenant holds is not this tenant's to act on.
    create(&rig, "only-b", "app", Some(B));
    for args in [
        json!({ "tenant": A, "instance": "only-b-bbbbbbbb" }),
        json!({ "tenant": A, "service": "only-b" }),
    ] {
        let (code, message) = refusal(rig.call_token("cloud", "instance_stop", args, &verbs));
        assert_eq!(code, "InstanceUnknown", "{message}");
        assert!(
            message.ends_with("exists in project arcak-t-aaaaaaaa"),
            "{message}"
        );
    }
    // Naming another project beside the tenant, both names, or neither, is refused as an argument.
    for args in [
        json!({ "tenant": A, "project": project(B), "instance": "web-aaaaaaaa" }),
        json!({ "tenant": A, "instance": "web-aaaaaaaa", "service": "web" }),
        json!({ "tenant": A }),
    ] {
        assert!(matches!(
            rig.call_token("cloud", "instance_stop", args, &verbs),
            Err(CallError::Invalid(_))
        ));
    }
    // A tenant's project is named by the tenant, never by project: the tenant's rules cannot be stepped around.
    for verb in verbs {
        assert!(
            matches!(
                rig.call_token(
                    "cloud",
                    verb,
                    json!({ "project": project(A), "instance": "web-aaaaaaaa", "approval": "x" }),
                    &verbs
                ),
                Err(CallError::Invalid(_))
            ),
            "{verb}"
        );
    }
    assert!(matches!(
        rig.call("instance_start", json!({ "project": project(A), "instance": "web-aaaaaaaa" })),
        Err(CallError::Invalid(message)) if message == "project: a tenant's project is named by tenant, not by project"
    ));
    // Outside a tenant nothing changed: a stop runs only from an approved proposal, and names its instance.
    create(&rig, "own", "app", None);
    for args in [
        json!({ "project": PROJECT, "instance": "own" }),
        json!({ "project": PROJECT, "service": "own", "approval": "x" }),
    ] {
        assert!(matches!(
            rig.call_token("cloud", "instance_stop", args, &verbs),
            Err(CallError::Invalid(_))
        ));
    }
    let (code, _) = refusal(rig.call_token(
        "cloud",
        "instance_stop",
        json!({ "project": PROJECT, "instance": "own", "approval": "0123456789abcdef0123456789abcdef" }),
        &verbs,
    ));
    assert_eq!(code, "NotApproved");
    assert_ne!(
        rig.get(&instance_path(PROJECT, "own")).unwrap()["status"],
        "Stopped"
    );
}

#[test]
fn removing_a_tenant_takes_everything_of_it_in_order_and_leaves_the_others_as_they_were() {
    let rig = two_tenants(policy());
    for tenant in [Some(A), Some(B), None] {
        create(&rig, "web", "app", tenant);
        create(&rig, "api", "app", tenant);
    }
    rig.call(
        "service_connect",
        json!({ "from": "api", "to": "web", "tenant": A }),
    )
    .unwrap();
    let b_before = objects(&rig, &project(B), &["web", "api"]);
    let own_before = objects(&rig, PROJECT, &["web", "api"]);
    let entries_before: Vec<_> = rig
        .entries()
        .into_iter()
        .filter(|entry| !entry.id.0.contains("aaaaaaaa"))
        .collect();
    let secrets = rig.dir.join("secrets");
    assert!(secrets.join("t-aaaaaaaa-service-api").is_dir());
    // A removal is the owner's: a direct call is refused and removes nothing.
    let (code, _) = refusal(rig.call("tenant_remove", json!({ "id": A, "approval": "x" })));
    assert_eq!(code, "NotApproved");
    let mark = rig.fake().requests().len();
    assert_eq!(
        rig.approved("tenant_remove", json!({ "id": A }))
            .expect("the approved removal"),
        json!({ "removed": true })
    );
    // Nothing of A is left: no instance, no ACL, no profile, no project, no record, no secret.
    assert!(rig.get(&format!("/1.0/projects/{}", project(A))).is_none());
    for name in ["web", "api"] {
        assert!(rig.get(instance_path(&project(A), name).as_str()).is_none());
    }
    for name in ["arcak-t-aaaaaaaa-svc-web", "arcak-t-aaaaaaaa-svc-api"] {
        assert!(
            rig.get(&format!("/1.0/network-acls/{name}?project=default"))
                .is_none(),
            "{name}"
        );
    }
    assert!(
        rig.entries()
            .iter()
            .all(|entry| !entry.id.0.contains("aaaaaaaa"))
    );
    assert!(!secrets.join("t-aaaaaaaa-service-api").exists());
    // The order: every instance is deleted and read back absent before its ACL, and the project goes last.
    let requests = &rig.fake().requests()[mark..];
    let deletes: Vec<&str> = requests
        .iter()
        .filter(|request| request.method == "DELETE")
        .map(|request| {
            assert!(request.status < 400, "{} {}", request.path, request.status);
            request.path.as_str()
        })
        .collect();
    let at = |path: &str| {
        deletes
            .iter()
            .position(|found| *found == path)
            .unwrap_or_else(|| panic!("{path} in {deletes:?}"))
    };
    for (instance, acl) in [("web", "svc-web"), ("api", "svc-api")] {
        let instance = instance_path(&project(A), instance);
        let acl = format!("/1.0/network-acls/arcak-t-aaaaaaaa-{acl}?project=default");
        assert!(at(&instance) < at(&acl), "{deletes:?}");
        let between = requests
            .iter()
            .skip_while(|request| !(request.method == "DELETE" && request.path == instance))
            .take_while(|request| !(request.method == "DELETE" && request.path == acl))
            .any(|request| {
                request.method == "GET" && request.path == instance && request.status == 404
            });
        assert!(
            between,
            "{instance} is read back absent before its ACL goes"
        );
    }
    let profile = format!("/1.0/profiles/arca-service?project={}", project(A));
    let gone = format!("/1.0/projects/{}", project(A));
    assert!(at(&profile) < at(&gone));
    assert_eq!(
        deletes.last().copied(),
        Some(gone.as_str()),
        "the project goes last"
    );
    // Every write of the removal names A's project or one of A's ACLs: none names B's or the node's own.
    for request in requests.iter().filter(|request| request.method != "GET") {
        assert!(
            request.path.contains("aaaaaaaa"),
            "{} {}",
            request.method,
            request.path
        );
    }
    // B and the node's own project are byte for byte what they were, in Incus and in the desired state.
    assert_eq!(objects(&rig, &project(B), &["web", "api"]), b_before);
    assert_eq!(objects(&rig, PROJECT, &["web", "api"]), own_before);
    assert_eq!(rig.entries(), entries_before);
    // The tenant is gone for every verb, and a new tenant's block is never B's.
    let (code, _) = refusal(rig.call("service_list", json!({ "tenant": A })));
    assert_eq!(code, "not_found");
    let next = rig.create_tenant(json!({ "id": "cccccccc" })).unwrap();
    assert_ne!(next["block"], "192.0.2.240/29");
}

#[test]
fn a_removal_that_cannot_finish_says_what_remains_and_keeps_the_project() {
    let rig = two_tenants(policy());
    create(&rig, "web", "app", Some(A));
    create(&rig, "api", "app", Some(A));
    rig.fake().set_reject_delete("api");
    let (_, message) = refusal(rig.approved("tenant_remove", json!({ "id": A })));
    assert_eq!(
        message,
        "remove tenant aaaaaaaa: partial removal (instances remain: api-aaaaaaaa; its project stands); the owner's approval is spent, so another try needs a new one"
    );
    assert!(rig.get(&format!("/1.0/projects/{}", project(A))).is_some());
    // The instance that could not be deleted keeps its ACL: it still names it.
    assert!(
        rig.get("/1.0/network-acls/arcak-t-aaaaaaaa-svc-api?project=default")
            .is_some()
    );
    assert!(
        rig.get("/1.0/network-acls/arcak-t-aaaaaaaa-svc-web?project=default")
            .is_none()
    );
    // The tenant is still there to be removed: a second approved removal finishes it.
    rig.fake().clear_reject_delete();
    let listed = rig.call("tenant_list", json!({})).unwrap();
    assert_eq!(listed["tenants"].as_array().map(Vec::len), Some(2));
    assert_eq!(
        rig.approved("tenant_remove", json!({ "id": A }))
            .expect("the second approved removal"),
        json!({ "removed": true })
    );
    assert!(rig.get(&format!("/1.0/projects/{}", project(A))).is_none());
    assert!(
        rig.get(instance_path(&project(A), "api").as_str())
            .is_none()
    );
    assert!(
        rig.get("/1.0/network-acls/arcak-t-aaaaaaaa-svc-api?project=default")
            .is_none()
    );
    let listed = rig.call("tenant_list", json!({})).unwrap();
    assert_eq!(listed["tenants"].as_array().map(Vec::len), Some(1));
}

#[test]
fn a_removal_that_was_cut_short_is_finished_by_the_next_approved_removal() {
    let rig = two_tenants(policy());
    create(&rig, "web", "app", Some(A));
    create(&rig, "api", "app", Some(A));
    // An earlier removal took the tenant's records out and stopped before it ran their plans: the instances stand,
    // and their delete plans wait.
    let inside: Vec<_> = rig
        .entries()
        .into_iter()
        .filter(|entry| entry.id.0.starts_with("t-aaaaaaaa-"))
        .map(|entry| arca::desired_store::EntryChange::Remove(entry.id))
        .collect();
    assert_eq!(inside.len(), 2);
    rig.kernel.lock().unwrap().propose(inside, NOW).unwrap();
    assert_eq!(rig.kernel.lock().unwrap().pending_plans().len(), 2);
    assert!(
        rig.get(instance_path(&project(A), "web").as_str())
            .is_some()
    );
    // The next approved removal runs those plans under its own approval and removes the tenant whole.
    assert_eq!(
        rig.approved("tenant_remove", json!({ "id": A }))
            .expect("the removal finishes"),
        json!({ "removed": true })
    );
    assert!(rig.get(&format!("/1.0/projects/{}", project(A))).is_none());
    for name in ["web", "api"] {
        assert!(rig.get(instance_path(&project(A), name).as_str()).is_none());
        assert!(
            rig.get(&format!(
                "/1.0/network-acls/arcak-t-aaaaaaaa-svc-{name}?project=default"
            ))
            .is_none()
        );
    }
    assert!(rig.kernel.lock().unwrap().pending_plans().is_empty());
    // And when an instance cannot be deleted, the refusal names it: it never says the instances are gone.
    create(&rig, "web", "app", Some(B));
    rig.fake().set_reject_delete("web-bbbbbbbb");
    let (_, message) = refusal(rig.approved("tenant_remove", json!({ "id": B })));
    assert!(
        message.contains("instances remain: web-bbbbbbbb"),
        "{message}"
    );
    let (_, message) = refusal(rig.approved("tenant_remove", json!({ "id": B })));
    assert!(
        message.contains("instances remain: web-bbbbbbbb"),
        "{message}"
    );
    assert!(!message.contains("every instance is removed"), "{message}");
}
