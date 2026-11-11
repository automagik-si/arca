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
    // Fourteen addresses are outside every tenant's block, and the node's own project holds them all.
    assert_eq!(
        listed(rig.call(
            "service_create",
            json!({ "name": "own14", "template": "app" }),
        )),
        (
            "capacity".to_string(),
            "the service subnet 192.0.2.0/24 has no free address (.10-.239)".to_string(),
            json!({ "limit": "addresses", "have": 14, "want": 1, "max": 14 })
        )
    );
    // The next tenant finds one block left (.16/29 holds addresses in use), and then none.
    let (code, message, fields) = listed(rig.create_tenant(json!({ "id": "bbbbbbbb" })));
    assert_eq!(code, "capacity");
    assert!(
        message.contains("has no free /29 block for a tenant"),
        "{message}"
    );
    assert_eq!(fields, json!({ "limit": "addresses" }));
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
    // Every one is a `capacity` refusal: the sentence, and the same cap and amounts as fields (bytes for disk).
    let refused = |name: &str, template: &str, tenant: &str| {
        let writes = rig.fake().writes().len();
        let (code, message, fields) = listed(rig.call(
            "service_create",
            json!({ "name": name, "template": template, "tenant": tenant }),
        ));
        assert_eq!(code, "capacity", "{message}");
        assert_eq!(rig.fake().writes().len(), writes, "nothing reached Incus");
        (message, fields)
    };
    let said = |message: &str, fields: Value| (message.to_string(), fields);
    const GIB: u64 = 1 << 30;
    create(&rig, "one", "app", Some(A));
    create(&rig, "two", "app", Some(A));
    assert_eq!(
        refused("three", "app", A),
        said(
            "creating three would exceed tenant aaaaaaaa's cap on containers (limits.containers): 2 in use, 1 asked, 2 allowed",
            json!({ "limit": "containers", "have": 2, "want": 1, "max": 2 })
        )
    );
    // Two containers hold 32 GiB of root disk: a machine of 20 GiB does not fit in 50.
    assert_eq!(
        refused("box", "vm", A),
        said(
            "creating box would exceed tenant aaaaaaaa's cap on disk (limits.disk): 32.0 GiB in use, 20.0 GiB asked, 50.0 GiB allowed",
            json!({ "limit": "disk", "have": 32 * GIB, "want": 20 * GIB, "max": 50 * GIB })
        )
    );
    // B is untouched by A's use: its own caps are whole.
    create(&rig, "box", "vm", Some(B));
    assert_eq!(
        refused("box2", "vm", B),
        said(
            "creating box2 would exceed tenant bbbbbbbb's cap on virtual machines (limits.virtual-machines): 1 in use, 1 asked, 1 allowed",
            json!({ "limit": "virtualMachines", "have": 1, "want": 1, "max": 1 })
        )
    );
    create(&rig, "one", "app", Some(B));
    // A tenant of one instance: the second is over its count, whatever else it has room for.
    let tiny = json!({ "instances": 1, "containers": 1, "virtualMachines": 0, "cpu": 6,
        "memory": "8GiB", "disk": "50GiB" });
    rig.create_tenant(json!({ "id": "cccccccc", "caps": tiny }))
        .unwrap();
    create(&rig, "one", "app", Some("cccccccc"));
    let one_instance = json!({ "limit": "instances", "have": 1, "want": 1, "max": 1 });
    assert_eq!(
        refused("two", "app", "cccccccc"),
        said(
            "creating two would exceed tenant cccccccc's cap on instances (limits.instances): 1 in use, 1 asked, 1 allowed",
            one_instance.clone()
        )
    );
    assert_eq!(
        refused("box", "vm", "cccccccc"),
        said(
            "creating box would exceed tenant cccccccc's cap on instances (limits.instances): 1 in use, 1 asked, 1 allowed",
            one_instance
        )
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
        said(
            "creating two would exceed tenant bbbbbbbb's cap on disk (limits.disk): 36.0 GiB in use, 16.0 GiB asked, 50.0 GiB allowed",
            json!({ "limit": "disk", "have": 36 * GIB, "want": 16 * GIB, "max": 50 * GIB })
        )
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
    assert_eq!(
        listed(rig.call(
            "service_create",
            json!({ "name": "own3", "template": "vm" }),
        )),
        (
            "capacity".to_string(),
            "creating own3 would exceed this node's cap on virtual machines (kernel.vmLimits.virtualMachines): 4 in use, 1 asked, 4 allowed".to_string(),
            json!({ "limit": "virtualMachines", "have": 4, "want": 1, "max": 4 })
        )
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

/// A virtual-machine template that declares one secret.
fn keys_template() -> Value {
    json!({
        "name": "keys", "kind": "virtual-machine", "image": "images:debian/13/cloud",
        "limits": { "cpu": "2", "memory": "4GiB" }, "rootSize": "20GiB", "secrets": ["API_TOKEN"]
    })
}

const SECRET_A: &str = "synthetic-token-of-tenant-a-0123456789";
const SECRET_B: &str = "synthetic-token-of-tenant-b-9876543210";

/// Tenants `A` and `B`, each with a virtual machine `box` whose secret is set and that is still stopped.
fn two_boxes() -> Rig {
    let rig = two_tenants(policy());
    rig.template("keys", keys_template());
    for (tenant, value) in [(A, SECRET_A), (B, SECRET_B)] {
        let made = create(&rig, "box", "keys", Some(tenant));
        assert_eq!(made["state"], "stopped");
        rig.call(
            "service_secret_set",
            json!({ "name": "box", "key": "API_TOKEN", "value": value, "tenant": tenant }),
        )
        .expect("the secret is stored");
    }
    rig
}

/// Every guest file the fake holds, as `<project>/<instance>:<path>` with its content.
fn guest_files(rig: &Rig) -> Vec<(String, String)> {
    rig.fake()
        .service_files()
        .into_iter()
        .map(|(key, bytes)| (key, String::from_utf8(bytes).expect("text")))
        .collect()
}

/// Every file request the fake received, as `<method> <target>`.
fn file_requests(rig: &Rig) -> Vec<String> {
    rig.fake()
        .service_requests()
        .into_iter()
        .filter(|request| request.path.split('?').next().unwrap().ends_with("/files"))
        .map(|request| format!("{} {}", request.method, request.path))
        .collect()
}

/// A start of a tenant's virtual machine writes its stored secrets into its own guest, in its own tenant's project, and
/// into no other machine: not the other tenant's machine of the same name, and not one of the node's own.
#[test]
fn a_start_writes_a_tenants_machine_its_own_secrets_in_its_own_project_and_nowhere_else() {
    let rig = two_boxes();
    let started = rig
        .call_token(
            "cloud",
            "instance_start",
            json!({ "tenant": A, "service": "box" }),
            &["instance_start"],
        )
        .expect("the start");
    assert_eq!(started["project"], project(A));
    assert_eq!(started["instance"], "box-aaaaaaaa");
    assert_eq!(
        started["message"],
        "started the instance box-aaaaaaaa in project arcak-t-aaaaaaaa. Its secret (API_TOKEN) was written into the guest, in /etc/arca-secrets."
    );
    // A's guest holds A's secret, under the private directory. Nothing else holds anything.
    assert_eq!(
        guest_files(&rig),
        vec![
            (
                "arcak-t-aaaaaaaa/box-aaaaaaaa:/etc/arca-secrets".to_string(),
                String::new()
            ),
            (
                "arcak-t-aaaaaaaa/box-aaaaaaaa:/etc/arca-secrets/API_TOKEN".to_string(),
                SECRET_A.to_string()
            ),
        ]
    );
    // Every file request named A's project and A's instance: the read of the directory, the directory, the file.
    assert_eq!(
        file_requests(&rig),
        vec![
            "GET /1.0/instances/box-aaaaaaaa/files?project=arcak-t-aaaaaaaa&path=/etc/arca-secrets",
            "POST /1.0/instances/box-aaaaaaaa/files?project=arcak-t-aaaaaaaa&path=/etc/arca-secrets",
            "POST /1.0/instances/box-aaaaaaaa/files?project=arcak-t-aaaaaaaa&path=/etc/arca-secrets/API_TOKEN",
        ]
    );
    // B's machine was not started and holds nothing; B's secret went nowhere.
    assert_eq!(
        rig.get(&instance_path(&project(B), "box")).unwrap()["status"],
        "Stopped"
    );
    assert!(
        guest_files(&rig)
            .iter()
            .all(|(_, content)| content != SECRET_B)
    );
    // The journal names the secret and the tenant, and holds no value.
    let journal = fs::read_to_string(rig.dir.join(arca::lifecycle::INSTANCE_ACTIONS_FILE)).unwrap();
    let last: Value = serde_json::from_str(journal.lines().last().unwrap()).unwrap();
    assert_eq!(last["kind"], "guest_secrets");
    assert_eq!(last["outcome"], "written");
    assert_eq!(last["secrets"], json!(["API_TOKEN"]));
    assert!(!journal.contains(SECRET_A) && !journal.contains(SECRET_B));

    // A new value reaches the guest at the next start, which restarts nothing.
    let rotated = "synthetic-token-of-tenant-a-rotated-000";
    rig.call(
        "service_secret_set",
        json!({ "name": "box", "key": "API_TOKEN", "value": rotated, "tenant": A }),
    )
    .expect("set again");
    let again = rig
        .call_token(
            "cloud",
            "instance_start",
            json!({ "tenant": A, "instance": "box-aaaaaaaa" }),
            &["instance_start"],
        )
        .expect("a start of a running machine");
    assert_eq!(again["changed"], false);
    assert_eq!(
        guest_files(&rig)
            .into_iter()
            .find(|(key, _)| key.ends_with("/API_TOKEN"))
            .map(|(_, content)| content),
        Some(rotated.to_string())
    );
    // B's start writes B's own, in B's project.
    rig.call_token(
        "cloud",
        "instance_start",
        json!({ "tenant": B, "service": "box" }),
        &["instance_start"],
    )
    .expect("B's start");
    let files = guest_files(&rig);
    assert!(files.contains(&(
        "arcak-t-bbbbbbbb/box-bbbbbbbb:/etc/arca-secrets/API_TOKEN".to_string(),
        SECRET_B.to_string()
    )));
    assert!(
        files
            .iter()
            .all(
                |(key, content)| key.starts_with("arcak-t-bbbbbbbb/") == (content == SECRET_B)
                    || content.is_empty()
            )
    );
    // A service without stored secrets in a tenant starts as before, and nothing is asked of its guest.
    let before = file_requests(&rig).len();
    create(&rig, "web", "app", Some(B));
    rig.call_token(
        "cloud",
        "instance_start",
        json!({ "tenant": B, "service": "web" }),
        &["instance_start"],
    )
    .expect("a start of a service without secrets");
    assert_eq!(file_requests(&rig).len(), before);
}

/// A call inside a tenant cannot name another tenant's project or the node's own, and so cannot have a secret written
/// anywhere but into the tenant's own machine. Each such call is refused before the instance is touched, and no file
/// request is sent.
#[test]
fn a_call_in_a_tenant_cannot_send_secrets_to_another_tenants_or_the_nodes_project() {
    let rig = two_boxes();
    // A machine of the node's own, with a secret too.
    rig.call(
        "service_create",
        json!({ "name": "own", "template": "keys" }),
    )
    .expect("the node's own machine");
    rig.call(
        "service_secret_set",
        json!({ "name": "own", "key": "API_TOKEN", "value": "synthetic-token-of-the-node-0123" }),
    )
    .expect("stored");
    let start =
        |args: Value| rig.call_token("economy", "instance_start", args, &["instance_start"]);
    // Another tenant's project, or the node's, beside the tenant the call names.
    for args in [
        json!({ "tenant": A, "project": project(B), "instance": "box-bbbbbbbb" }),
        json!({ "tenant": A, "project": "arcak-svc", "instance": "own" }),
    ] {
        assert!(
            matches!(
                start(args.clone()),
                Err(CallError::Invalid(why)) if why == "project: it is not the project of the tenant the call names"
            ),
            "{args}"
        );
    }
    // A tenant's project named without its tenant: a tenant is reached by its id only.
    assert!(matches!(
        start(json!({ "project": project(B), "instance": "box-bbbbbbbb" })),
        Err(CallError::Invalid(why)) if why == "project: a tenant's project is named by tenant, not by project"
    ));
    // Another tenant's instance, or the node's, by name: there is no such instance in the tenant the call names.
    for instance in ["box-bbbbbbbb", "own"] {
        let (code, _) = refusal(start(json!({ "tenant": A, "instance": instance })));
        assert_eq!(code, "InstanceUnknown", "{instance}");
    }
    // Nothing was started and no file request was sent, to anyone.
    for (place, name) in [(project(A), "box"), (project(B), "box")] {
        assert_eq!(
            rig.get(&instance_path(&place, name)).unwrap()["status"],
            "Stopped"
        );
    }
    assert_eq!(file_requests(&rig), Vec::<String>::new());

    // Below the verbs: the delivery finds a record only where the project and the instance are that record's own. A
    // crossed pair is no record at all, so nothing is read and nothing is sent.
    let secrets = arca::guest_secrets::GuestSecrets::new(rig.cfg.clone(), rig.kernel.clone())
        .with_wait(
            std::time::Duration::from_millis(100),
            std::time::Duration::from_millis(10),
        );
    for (place, instance) in [
        (project(A), "box-bbbbbbbb"),
        (project(B), "box-aaaaaaaa"),
        ("arcak-svc".to_string(), "box-aaaaaaaa"),
        (project(A), "own"),
        ("arcak-t-cccccccc".to_string(), "box-aaaaaaaa"),
        ("default".to_string(), "box-aaaaaaaa"),
    ] {
        assert_eq!(
            secrets.deliver(&place, instance),
            Ok(None),
            "{place} {instance}"
        );
    }
    assert_eq!(file_requests(&rig), Vec::<String>::new());
}

/// The seam itself. A writer is bound to one entry and reaches the node's family and that entry's own tenant project.
/// Through it the two guest-secret destinations go to the owned instance's project; a command and every other file stay
/// in the node's own project and are refused for a tenant's instance; and another tenant's instance cannot be read as
/// owned at all.
#[test]
fn the_guest_file_seam_follows_the_owned_instance_and_a_command_never_does() {
    use arca::service_exec::{ExecTarget, writer_for};
    use arca_incus::exec::{ExecCommand, JournalLines, SecretName, ServiceFile};
    use arca_kernel::ownership::{IncusKind, ObjectRef};
    let rig = two_boxes();
    for tenant in [A, B] {
        rig.call_token(
            "cloud",
            "instance_start",
            json!({ "tenant": tenant, "service": "box" }),
            &["instance_start"],
        )
        .expect("start");
    }
    let target = |tenant: &str| {
        let kernel = rig.kernel.lock().unwrap();
        let entry = kernel
            .entries()
            .iter()
            .find(|entry| entry.id.0 == format!("t-{tenant}-service-box"))
            .expect("the entry")
            .clone();
        let service: arca::service::ServiceConfig =
            serde_json::from_value(entry.config.clone()).expect("the record");
        let object = kernel
            .service_object(&entry.id, &service.instance)
            .expect("the journal's instance");
        (
            ExecTarget {
                entry: entry.id.clone(),
                uuid: object.uuid.clone(),
                retained: kernel.retained_entries().expect("retained"),
                service,
            },
            object,
        )
    };
    let (target_a, object_a) = target(A);
    let (_, object_b) = target(B);
    // The journal holds each instance in its own tenant's project.
    assert_eq!(object_a.project, project(A));
    assert_eq!(object_b.project, project(B));
    let budget = std::time::Duration::from_secs(5);
    let writer = writer_for(&rig.cfg, &target_a, budget).expect("A's writer");
    let owned = writer.read_owned(&object_a).expect("A's own instance");
    let before = file_requests(&rig).len();
    // The guest-secret file goes to A's project.
    writer
        .write_service_file(
            &owned,
            &ServiceFile::GuestSecret(SecretName::new("API_TOKEN").unwrap()),
            b"synthetic-seam-value",
        )
        .expect("a guest secret in the tenant's project");
    assert_eq!(
        file_requests(&rig)[before..],
        [
            "POST /1.0/instances/box-aaaaaaaa/files?project=arcak-t-aaaaaaaa&path=/etc/arca-secrets/API_TOKEN"
        ]
    );
    // Every other file, and a command, are refused for a tenant's instance, and nothing is sent.
    let sent = rig.fake().service_requests().len();
    let refused = arca_incus::IncusError::Refused("invalid service exec target or argument".into());
    for file in [
        ServiceFile::TailnetKey,
        ServiceFile::Secret(SecretName::new("API_TOKEN").unwrap()),
    ] {
        assert_eq!(
            writer.write_service_file(&owned, &file, b"synthetic-seam-value"),
            Err(refused.clone())
        );
    }
    assert_eq!(
        writer.start_service_exec(
            &owned,
            &ExecCommand::Journal(JournalLines::new(5).unwrap()),
            budget
        ),
        Err(refused.clone())
    );
    assert!(
        rig.fake().service_requests()[sent..]
            .iter()
            .all(|request| request.method == "GET" && !request.path.contains("/files")),
        "nothing but the writer's own ownership reads was sent"
    );
    // A's writer does not reach B's project: B's instance is not read as owned, by its own uuid or by A's.
    for uuid in [object_b.uuid.clone(), object_a.uuid.clone()] {
        let crossed = ObjectRef {
            kind: IncusKind::Instance,
            project: project(B),
            parent: None,
            name: "box-bbbbbbbb".into(),
            uuid,
        };
        assert!(writer.read_owned(&crossed).is_err());
    }
    // A record that names another tenant's project than its own gets no writer at all.
    let mut crossed = target_a.clone();
    crossed.service.project = project(B);
    assert!(writer_for(&rig.cfg, &crossed, budget).is_err());
    // B's guest still holds B's secret and nothing of this.
    assert!(guest_files(&rig).contains(&(
        "arcak-t-bbbbbbbb/box-bbbbbbbb:/etc/arca-secrets/API_TOKEN".to_string(),
        SECRET_B.to_string()
    )));
}

/// What to do next is said once and is true: a guest that was not ready is asked again, and a refusal that needs
/// something put right says that asking again changes nothing.
#[test]
fn a_secret_that_was_not_written_says_whether_asking_again_can_help() {
    let rig = two_boxes();
    let start = || {
        rig.call_token(
            "cloud",
            "instance_start",
            json!({ "tenant": A, "service": "box" }),
            &["instance_start"],
        )
    };
    // The guest's agent never answers: asking again is the way.
    rig.fake().set_guest_agent_unready(u32::MAX);
    let (code, message) = refusal(start());
    assert_eq!(code, "InstanceSecretsNotWritten");
    assert!(
        message.ends_with("its agent is not running yet, or its image has none. Run instance_start for it again to write them; that never restarts it"),
        "{message}"
    );
    assert!(!message.contains("changes nothing"));
    // Something else sits where the directory goes: asking again changes nothing, and the answer does not say to.
    rig.fake().set_guest_agent_unready(0);
    rig.fake().put_guest_file(
        &project(A),
        "box-aaaaaaaa",
        "/etc/arca-secrets",
        "directory",
        "0755",
        0,
    );
    let (code, message) = refusal(start());
    assert_eq!(code, "InstanceSecretsNotWritten");
    assert!(
        message.ends_with("and Arca did not make it so; nothing was written. Running instance_start again changes nothing until that is put right"),
        "{message}"
    );
    assert!(!message.contains("Run instance_start for it again"));
    assert!(!message.contains(SECRET_A));
}
