//! Who may create a tenant, what it is made as, and what the node's policy refuses.
use super::*;

#[test]
fn a_node_without_the_key_has_no_tenants_whatever_the_verb() {
    let rig = Rig::new(None);
    rig.template("app", app_template("app"));
    for (verb, args) in [
        ("tenant_create", json!({})),
        ("tenant_list", json!({})),
        ("tenant_status", json!({ "id": A })),
        ("tenant_usage", json!({ "id": A })),
        (
            "tenant_resize",
            json!({ "id": A, "caps": { "cpu": 4 }, "approval": "x" }),
        ),
        ("tenant_remove", json!({ "id": A, "approval": "x" })),
        (
            "service_create",
            json!({ "name": "web", "template": "app", "tenant": A }),
        ),
        ("service_list", json!({ "tenant": A })),
        ("service_status", json!({ "name": "web", "tenant": A })),
        (
            "service_remove",
            json!({ "name": "web", "approval": "x", "tenant": A }),
        ),
        (
            "service_connect",
            json!({ "from": "web", "to": "db", "tenant": A }),
        ),
        (
            "postgres_create",
            json!({ "name": "db", "image": PG_IMAGE, "tenant": A }),
        ),
        ("postgres_list", json!({ "tenant": A })),
    ] {
        let (code, message) = refusal(rig.call_as(verb, args, Some(&["tenant_create"])));
        assert_eq!(
            (code.as_str(), message.as_str()),
            ("no_tenants", "this node has no tenants"),
            "{verb}"
        );
    }
    assert!(rig.fake().writes().is_empty(), "nothing was written");
    assert!(rig.entries().is_empty());
}

#[test]
fn a_tenant_is_one_restricted_project_on_the_managed_bridge_with_its_caps_and_profile() {
    let rig = Rig::new(Some(policy()));
    let created = rig.create_tenant(json!({ "id": A })).expect("tenant");
    assert_eq!(created["id"], A);
    // The highest /29 of the subnet: the node's defaults allow four instances.
    assert_eq!(created["block"], "192.0.2.248/29");
    assert_eq!(created["caps"]["instances"], 4);
    assert_eq!(created["use"]["instances"], 0);
    let found = rig
        .get(&format!("/1.0/projects/{0}?project={0}", project(A)))
        .expect("the project");
    let config = found["config"].as_object().expect("config");
    let expected = [
        ("user.arcakernel.entry", "tenant-aaaaaaaa"),
        ("restricted", "true"),
        ("restricted.networks.access", "arcak-svcbr"),
        ("restricted.devices.proxy", "block"),
        ("restricted.devices.gpu", "block"),
        ("restricted.devices.disk", "managed"),
        ("restricted.devices.nic", "managed"),
        ("restricted.containers.nesting", "block"),
        ("restricted.containers.privilege", "unprivileged"),
        ("restricted.containers.lowlevel", "block"),
        ("restricted.virtual-machines.lowlevel", "block"),
        ("limits.instances", "4"),
        ("limits.containers", "4"),
        ("limits.virtual-machines", "1"),
        ("limits.cpu", "8"),
        ("limits.memory", "16GiB"),
        ("limits.disk", "100GiB"),
        ("features.images", "true"),
        ("features.profiles", "true"),
        ("features.storage.volumes", "true"),
    ];
    for (key, value) in expected {
        assert_eq!(
            config.get(key).and_then(Value::as_str),
            Some(value),
            "{key}"
        );
    }
    assert!(
        config.get("features.networks").is_none(),
        "no network of its own"
    );
    let profile = rig
        .get(&format!(
            "/1.0/profiles/arca-service?project={}",
            project(A)
        ))
        .expect("the tenant's service profile");
    assert_eq!(
        profile["config"]["user.arcakernel.entry"],
        "tenant-aaaaaaaa"
    );
    assert_eq!(profile["devices"]["root"]["size"], "16GiB");
    // No bridge was made for the tenant: the only network is the node's managed one, made by its own entry.
    let networks: Vec<String> = rig
        .fake()
        .writes()
        .iter()
        .filter(|write| write.method == "POST" && write.path.starts_with("/1.0/networks"))
        .map(|write| write.path.clone())
        .collect();
    assert_eq!(networks, ["/1.0/networks?project=default"]);
    assert_eq!(
        rig.get("/1.0/networks/arcak-svcbr?project=default")
            .unwrap()["config"]["user.arcakernel.entry"],
        "net-arcak-svc"
    );
    // The entry records the block with its size.
    let entry = rig.entry("tenant-aaaaaaaa");
    assert_eq!(entry["block"], "192.0.2.248/29");
    assert_eq!(entry["project"], project(A));
    // An id is made when none is given: 8 base32 characters, never a name.
    let made = rig
        .create_tenant(json!({}))
        .expect("a tenant with a made id");
    let id = made["id"].as_str().unwrap();
    assert!(
        id.len() == 8
            && id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b))
    );
    assert_eq!(made["block"], "192.0.2.240/29");
    let listed = rig.call("tenant_list", json!({})).unwrap();
    assert_eq!(listed["tenants"].as_array().map(Vec::len), Some(2));
}

#[test]
fn a_create_inside_the_policy_needs_an_allowlisted_token_and_caps_above_default_need_the_owner() {
    let rig = Rig::new(Some(policy()));
    // A token with no allowlist, or one that does not name the verb, is refused.
    for tools in [None, Some(&["service_create"][..])] {
        let (code, message) = refusal(rig.call_as("tenant_create", json!({ "id": A }), tools));
        assert_eq!(code, "NotAllowed");
        assert!(
            message.contains("tool allowlist names tenant_create"),
            "{message}"
        );
    }
    // Caps at or under the defaults are inside the policy.
    let small = json!({ "instances": 2, "containers": 2, "virtualMachines": 0, "cpu": 2,
        "memory": "4GiB", "disk": "40GiB" });
    assert_eq!(
        rig.create_tenant(json!({ "id": A, "caps": small }))
            .expect("small caps")["caps"]["cpu"],
        2
    );
    // One cap above its default needs the owner, whatever the token.
    let large = json!({ "instances": 12, "containers": 12, "virtualMachines": 1, "cpu": 8,
        "memory": "16GiB", "disk": "100GiB" });
    let (code, message) = refusal(rig.create_tenant(json!({ "id": B, "caps": large })));
    assert_eq!(code, "NotApproved");
    assert!(message.contains("kernel.tenants.defaultCaps"), "{message}");
    assert!(rig.get(&format!("/1.0/projects/{}", project(B))).is_none());
    // A forged grant is not an approval.
    let (code, _) = refusal(rig.create_tenant(
        json!({ "id": B, "caps": large, "approval": "0123456789abcdef0123456789abcdef" }),
    ));
    assert_eq!(code, "NotApproved");
    // The owner's passkey over the proposal runs it; the proposal binds the exact arguments.
    let created = rig
        .approved("tenant_create", json!({ "id": B, "caps": large }))
        .expect("the approved create");
    assert_eq!(created["caps"]["instances"], 12);
    // More than eight instances take a /28: the highest one that overlaps no block in use.
    assert_eq!(created["block"], "192.0.2.224/28");
    assert_eq!(rig.entry("tenant-bbbbbbbb")["block"], "192.0.2.224/28");
}

#[test]
fn a_tenant_that_may_fill_its_block_is_never_given_the_block_without_the_broadcast_address() {
    let mut full = policy();
    full["defaultCaps"] = json!({ "instances": 8, "containers": 8, "virtualMachines": 0, "cpu": 16,
        "memory": "16GiB", "disk": "200GiB" });
    full["capacity"]["disk"] = json!("4TiB");
    let rig = Rig::new(Some(full));
    // The top /29 holds 7 usable addresses, so 8 instances start one block lower; 16 instances skip the top /28 too.
    assert_eq!(
        rig.create_tenant(json!({ "id": A })).unwrap()["block"],
        "192.0.2.240/29"
    );
    let small = json!({ "instances": 7, "containers": 7, "virtualMachines": 0, "cpu": 8,
        "memory": "16GiB", "disk": "200GiB" });
    assert_eq!(
        rig.create_tenant(json!({ "id": B, "caps": small }))
            .unwrap()["block"],
        "192.0.2.248/29"
    );
    let large = json!({ "instances": 16, "containers": 16, "virtualMachines": 0, "cpu": 8,
        "memory": "16GiB", "disk": "200GiB" });
    assert_eq!(
        rig.approved("tenant_create", json!({ "id": "cccccccc", "caps": large }))
            .unwrap()["block"],
        "192.0.2.224/28"
    );
    // Every address of the block is one an instance can take.
    rig.template("app", app_template("app"));
    for n in 0..8 {
        let made = rig
            .call(
                "service_create",
                json!({ "name": format!("s{n}"), "template": "app", "tenant": A }),
            )
            .unwrap_or_else(|error| panic!("the instance {n} of 8: {error:?}"));
        assert_eq!(made["ipv4"], format!("192.0.2.{}", 240 + n));
    }
}

#[test]
fn the_policys_limits_refuse_in_plain_words_that_name_the_limit() {
    let mut tight = policy();
    tight["maxTenants"] = json!(2);
    tight["capacity"] = json!({ "instances": 9, "cpu": 64, "memory": "128GiB", "disk": "2TiB" });
    let rig = Rig::new(Some(tight));
    rig.create_tenant(json!({ "id": A })).expect("the first");
    rig.create_tenant(json!({ "id": B })).expect("the second");
    let (_, message) = refusal(rig.create_tenant(json!({ "id": "cccccccc" })));
    assert_eq!(
        message,
        "this node holds 2 tenants, and kernel.tenants.maxTenants is 2"
    );
    let (_, message) = refusal(rig.create_tenant(json!({ "id": A })));
    assert_eq!(message, "tenant aaaaaaaa exists");

    // Capacity: the sum of every tenant's caps never passes it.
    let mut tight = policy();
    tight["capacity"] = json!({ "instances": 7, "cpu": 64, "memory": "128GiB", "disk": "2TiB" });
    let rig = Rig::new(Some(tight));
    rig.create_tenant(json!({ "id": A })).expect("the first");
    let (_, message) = refusal(rig.create_tenant(json!({ "id": B })));
    assert_eq!(
        message,
        "the tenants' caps together would exceed this node's capacity (kernel.tenants.capacity.instances): 8 with this tenant, 7 allowed"
    );
    // The node's own virtual-machine cap covers the tenants' machines: four tenants of one machine each fill it.
    let rig = Rig::new(Some(policy()));
    for id in ["aaaaaaaa", "bbbbbbbb", "cccccccc", "dddddddd"] {
        rig.create_tenant(json!({ "id": id })).expect("a tenant");
    }
    let (_, message) = refusal(rig.create_tenant(json!({ "id": "eeeeeeee" })));
    assert!(
        message
            .contains("would exceed the node's virtual-machine cap: 5 with this tenant, 4 allowed"),
        "{message}"
    );
    // Nothing of a refused create is left behind.
    assert!(rig.get("/1.0/projects/arcak-t-eeeeeeee").is_none());
    assert!(
        rig.entries()
            .iter()
            .all(|entry| entry.id.0 != "tenant-eeeeeeee")
    );
}

#[test]
fn the_create_rate_is_counted_from_the_journal_and_holds_across_a_restart() {
    let mut limited = policy();
    limited["createRate"] = json!({ "count": 2, "perSeconds": 3600 });
    let rig = Rig::new(Some(limited));
    rig.create_tenant(json!({ "id": A })).expect("the first");
    rig.clock.store(NOW + 60_000, Ordering::SeqCst);
    rig.create_tenant(json!({ "id": B })).expect("the second");
    // A tenant that is removed still counts: the rate is about creates, not about what is left.
    rig.approved("tenant_remove", json!({ "id": B }))
        .expect("the removal");
    let refused = "this node creates at most 2 tenants in 3600 seconds (kernel.tenants.createRate), and 2 were created in that time; the next create is allowed at 2026-10-30T13:00:00.000Z";
    let (_, message) = refusal(rig.create_tenant(json!({ "id": "cccccccc" })));
    assert_eq!(message, refused);
    // A restart reads the same journal, so the count is the same.
    let rig = rig.restart();
    let (_, message) = refusal(rig.create_tenant(json!({ "id": "cccccccc" })));
    assert_eq!(message, refused);
    // The owner's approval does not lift the rate either.
    let large = json!({ "instances": 12, "containers": 12, "virtualMachines": 0, "cpu": 8,
        "memory": "16GiB", "disk": "100GiB" });
    let (_, message) =
        refusal(rig.approved("tenant_create", json!({ "id": "cccccccc", "caps": large })));
    assert_eq!(message, refused);
    // At the stated time the oldest create has left the span.
    rig.clock.store(NOW + 3_600_000, Ordering::SeqCst);
    rig.create_tenant(json!({ "id": "cccccccc" }))
        .expect("the create at the stated time");
    assert!(rig.get(&format!("/1.0/projects/{}", project(A))).is_some());
}

#[test]
fn a_scratch_run_names_its_tenant_projects_under_the_scratch_prefix_and_joins_the_bridge_it_finds()
{
    // The node's managed bridge is there already, made by the node's own kernel; a scratch kernel never makes one.
    let mut state = empty_state();
    state.networks.push(json!({
        "name": "arcak-svcbr", "type": "bridge", "project": "default",
        "config": { "ipv4.address": "192.0.2.1/24", "ipv4.nat": "true", "ipv6.address": "none",
            "user.arcakernel.entry": "net-arcak-svc" }
    }));
    let scratch = |state: FakeState| {
        Rig::with_kernel(
            json!({
                "projects": ["arcak-scratch-probe"],
                "droiddProjects": ["other"],
                "scratch": true,
                "serviceCidr": CIDR,
                "vmLimits": { "virtualMachines": 4, "cpu": 16, "memory": "64GiB", "disk": "1TiB" },
                "tenants": policy()
            }),
            state,
        )
    };
    let rig = scratch(state);
    rig.create_tenant(json!({ "id": A })).expect("tenant");
    assert!(rig.get("/1.0/projects/arcak-scratch-t-aaaaaaaa").is_some());
    assert!(rig.get("/1.0/projects/arcak-t-aaaaaaaa").is_none());
    assert_eq!(
        rig.entry("tenant-aaaaaaaa")["project"],
        "arcak-scratch-t-aaaaaaaa"
    );
    // It wrote the tenant's project and profile and nothing else: no network, and nothing in the node's own project.
    for write in rig.fake().writes() {
        assert!(
            write.path == "/1.0/projects" || write.path.contains("arcak-scratch-t-aaaaaaaa"),
            "{} {}",
            write.method,
            write.path
        );
    }
    rig.template("app", app_template("app"));
    let web = rig
        .call(
            "service_create",
            json!({ "name": "web", "template": "app", "tenant": A }),
        )
        .expect("a service in the scratch tenant");
    assert_eq!(web["ipv4"], "192.0.2.248");
    assert!(
        rig.get("/1.0/instances/web-aaaaaaaa?project=arcak-scratch-t-aaaaaaaa")
            .is_some()
    );
    // The node's own project stays out of a scratch run, as before.
    let (_, message) = refusal(rig.call(
        "service_create",
        json!({ "name": "web", "template": "app" }),
    ));
    assert!(
        message.contains("excluded by kernel configuration"),
        "{message}"
    );
    // Without the managed bridge there is nothing to join, and nothing is made.
    let rig = scratch(empty_state());
    let (_, message) = refusal(rig.create_tenant(json!({ "id": A })));
    assert!(message.contains("create tenant aaaaaaaa"), "{message}");
    assert!(rig.get("/1.0/projects/arcak-scratch-t-aaaaaaaa").is_none());
    assert!(rig.fake().writes().iter().all(|write| write.status >= 400));
}
