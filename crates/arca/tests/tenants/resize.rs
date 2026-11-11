//! `tenant_resize`: a tenant's caps grow and shrink in place, only with the owner's approval, and never past what the
//! tenant uses, the node's capacity or the tenant's address block (Rule 68).
use super::*;

/// The `limits.*` keys a tenant's project holds in Incus, in the order of [`CAP_KEYS`].
fn limits(rig: &Rig, id: &str) -> Vec<String> {
    let found = rig
        .get(&format!("/1.0/projects/{0}?project={0}", project(id)))
        .expect("the project");
    CAP_KEYS
        .iter()
        .map(|key| {
            found["config"][*key]
                .as_str()
                .unwrap_or_else(|| panic!("{key}"))
                .to_string()
        })
        .collect()
}

const CAP_KEYS: [&str; 6] = [
    "limits.instances",
    "limits.containers",
    "limits.virtual-machines",
    "limits.cpu",
    "limits.memory",
    "limits.disk",
];

/// The node's default caps, as each tenant of [`two_tenants`] holds them.
const DEFAULT: [&str; 6] = ["4", "4", "1", "8", "16GiB", "100GiB"];

fn resize(rig: &Rig, id: &str, caps: Value) -> Result<Value, CallError> {
    rig.approved("tenant_resize", json!({ "id": id, "caps": caps }))
}

/// The refusal of a resize when it is proposed. Nothing is recorded for the owner, and nothing reaches Incus.
fn refused(rig: &Rig, id: &str, caps: Value) -> String {
    let writes = rig.fake().writes().len();
    let (_, message) = refusal(rig.call(
        "action_propose",
        json!({ "action": "tenant_resize", "args": { "id": id, "caps": caps } }),
    ));
    assert_eq!(rig.fake().writes().len(), writes, "nothing reached Incus");
    assert_eq!(waiting(rig), 0, "nothing waits for the owner");
    message
}

fn waiting(rig: &Rig) -> usize {
    rig.call("plan_list", json!({})).expect("plan_list")["plans"]
        .as_array()
        .map_or(0, Vec::len)
}

/// Proposes a resize and answers the proposal, without approving it.
fn propose(rig: &Rig, id: &str, caps: Value) -> Value {
    rig.call(
        "action_propose",
        json!({ "action": "tenant_resize", "args": { "id": id, "caps": caps } }),
    )
    .expect("the proposal")
}

/// The owner's approval of `proposed`, spent: the one-shot grant its action runs with.
fn approve(rig: &Rig, proposed: &Value) -> arca::kernel::ApprovedAction {
    let id = proposed["id"].as_str().expect("the proposal's id");
    let digest = proposed["argsDigest"].as_str().expect("its digest");
    let nonce = format!("{:064x}", rig.nonce.fetch_add(1, Ordering::SeqCst) + 1);
    let statement =
        arca::handlers::action_statement("node1", id, "tenant_resize", digest, rig.now(), &nonce)
            .expect("the statement");
    let approval = common::approve_statement(&rig.key, &statement);
    arca::handlers::approve_action(
        &mut rig.kernel.lock().expect("kernel"),
        &json!({ "plan": id, "approval": approval }),
        rig.now(),
    )
    .expect("the approval")
}

/// Runs an approved action through its own handler, as the approval page does.
fn run(rig: &Rig, approved: &arca::kernel::ApprovedAction) -> Result<Value, CallError> {
    ActionExecutor::new(
        rig.handlers
            .iter()
            .map(|(verb, handler)| (*verb, Arc::clone(handler))),
    )
    .run(&approved.action, &approved.args, &approved.grant)
}

fn create(rig: &Rig, name: &str, template: &str, tenant: &str) -> Value {
    rig.call(
        "service_create",
        json!({ "name": name, "template": template, "tenant": tenant }),
    )
    .unwrap_or_else(|error| panic!("create {name} in {tenant}: {error:?}"))
}

fn instance(rig: &Rig, id: &str, name: &str) -> Value {
    rig.get(&format!(
        "/1.0/instances/{name}-{id}?project={}",
        project(id)
    ))
    .unwrap_or_else(|| panic!("the instance {name}"))
}

#[test]
fn a_resize_grows_and_shrinks_a_tenant_in_place_and_restarts_nothing() {
    let rig = two_tenants(policy());
    // A tenant that holds a container and a running virtual machine.
    create(&rig, "web", "app", A);
    create(&rig, "box", "vm", A);
    assert_eq!(limits(&rig, A), DEFAULT);
    let before = (instance(&rig, A, "web"), instance(&rig, A, "box"));
    let block = rig.entry("tenant-aaaaaaaa")["block"].clone();
    let writes = rig.fake().writes().len();

    // Grow: the caps the call names change, and one it leaves out keeps its value.
    let grown = resize(&rig, A, json!({ "cpu": 16, "memory": "32GiB" })).expect("the grow");
    assert_eq!(
        grown["caps"],
        json!({ "instances": 4, "containers": 4, "virtualMachines": 1, "cpu": 16,
            "memory": "32GiB", "disk": "100GiB" })
    );
    assert_eq!(grown["id"], A);
    assert_eq!(grown["block"], block, "the tenant keeps its block");
    assert_eq!(grown["instances"], 2);
    assert_eq!(grown["use"]["cpu"], 4);
    assert_eq!(
        limits(&rig, A),
        ["4", "4", "1", "16", "32GiB", "100GiB"],
        "Incus holds the new caps"
    );
    assert_eq!(rig.entry("tenant-aaaaaaaa")["caps"], grown["caps"]);
    assert_eq!(rig.entry("tenant-aaaaaaaa")["block"], block);
    // The step is one write, of the tenant's project. No instance is written, so none is restarted.
    let made: Vec<(String, String, u16)> = rig.fake().writes()[writes..]
        .iter()
        .map(|write| (write.method.clone(), write.path.clone(), write.status))
        .collect();
    assert_eq!(made.len(), 1, "{made:?}");
    assert!(
        made[0].1.starts_with("/1.0/projects/arcak-t-aaaaaaaa") && made[0].2 == 200,
        "{made:?}"
    );
    assert_eq!(
        (instance(&rig, A, "web"), instance(&rig, A, "box")),
        before,
        "each instance is as it was"
    );
    assert_eq!(instance(&rig, A, "box")["status"], "Running");
    // tenant_status and tenant_list show the new caps.
    let status = rig.call("tenant_status", json!({ "id": A })).unwrap();
    assert_eq!(status["caps"], grown["caps"]);
    let listed = rig.call("tenant_list", json!({})).unwrap();
    assert_eq!(listed["tenants"][0]["id"], A);
    assert_eq!(listed["tenants"][0]["caps"], grown["caps"]);

    // Shrink, down to exactly what the tenant uses: one container and one machine, 4 CPUs, 6 GiB, 36 GiB.
    let shrunk = resize(
        &rig,
        A,
        json!({ "instances": 2, "containers": 1, "virtualMachines": 1, "cpu": 4,
            "memory": "6GiB", "disk": "36GiB" }),
    )
    .expect("the shrink");
    assert_eq!(shrunk["caps"]["cpu"], 4);
    assert_eq!(limits(&rig, A), ["2", "1", "1", "4", "6GiB", "36GiB"]);
    assert_eq!((instance(&rig, A, "web"), instance(&rig, A, "box")), before);
    // The caps a create is checked against are the new ones, in both directions.
    let (_, message) = refusal(rig.call(
        "service_create",
        json!({ "name": "api", "template": "app", "tenant": A }),
    ));
    assert_eq!(
        message,
        "creating api would exceed tenant aaaaaaaa's cap on instances (limits.instances): 2 in use, 1 asked, 2 allowed"
    );
    resize(
        &rig,
        A,
        json!({ "instances": 3, "containers": 2, "cpu": 6, "memory": "8GiB", "disk": "52GiB" }),
    )
    .expect("the second grow");
    create(&rig, "api", "app", A);
    // A resized tenant is removed like any other: its project goes with everything in it.
    rig.approved("tenant_remove", json!({ "id": A }))
        .expect("the removal");
    assert!(rig.get(&format!("/1.0/projects/{}", project(A))).is_none());
}

#[test]
fn a_resize_below_what_the_tenant_uses_is_refused_and_names_the_cap() {
    let rig = two_tenants(policy());
    create(&rig, "web", "app", A);
    create(&rig, "box", "vm", A);
    // In use: one container and one machine, 4 CPUs, 6 GiB of memory, 36 GiB of disk.
    for (caps, cap) in [
        (
            json!({ "instances": 1, "containers": 1, "virtualMachines": 1 }),
            "instances (limits.instances) cannot go below what the tenant uses now: 2 in use, 1 asked",
        ),
        (
            json!({ "containers": 0 }),
            "containers (limits.containers) cannot go below what the tenant uses now: 1 in use, 0 asked",
        ),
        (
            json!({ "virtualMachines": 0 }),
            "virtual machines (limits.virtual-machines) cannot go below what the tenant uses now: 1 in use, 0 asked",
        ),
        (
            json!({ "cpu": 3 }),
            "CPUs (limits.cpu) cannot go below what the tenant uses now: 4 in use, 3 asked",
        ),
        (
            json!({ "memory": "4GiB" }),
            "memory (limits.memory) cannot go below what the tenant uses now: 6.0 GiB in use, 4.0 GiB asked",
        ),
        (
            json!({ "disk": "30GiB" }),
            "disk (limits.disk) cannot go below what the tenant uses now: 36.0 GiB in use, 30.0 GiB asked",
        ),
    ] {
        assert_eq!(
            refused(&rig, A, caps),
            format!("resize tenant aaaaaaaa: its cap on {cap}")
        );
    }
    assert_eq!(limits(&rig, A), DEFAULT);
    // The use that counts is the use when the resize runs. A shrink the owner approved is refused when the tenant has
    // grown past it since, and the approval is spent.
    let rig = two_tenants(policy());
    create(&rig, "web", "app", A);
    let approved = approve(&rig, &propose(&rig, A, json!({ "cpu": 2 })));
    create(&rig, "api", "app", A);
    let (_, message) = refusal(run(&rig, &approved));
    assert_eq!(
        message,
        "resize tenant aaaaaaaa: its cap on CPUs (limits.cpu) cannot go below what the tenant uses now: 4 in use, 2 asked"
    );
    assert_eq!(limits(&rig, A), DEFAULT);
    assert_eq!(rig.entry("tenant-aaaaaaaa")["caps"]["cpu"], 8);
    let (code, _) = refusal(run(&rig, &approved));
    assert_eq!(code, "NotApproved", "the grant ran once");
}

#[test]
fn a_resize_past_the_nodes_capacity_is_refused_and_names_the_capacity() {
    let rig = two_tenants(policy());
    // Two tenants of 8 CPUs each, and a capacity of 64: one of them may hold 56.
    assert_eq!(
        refused(&rig, A, json!({ "cpu": 57 })),
        "resize tenant aaaaaaaa: the tenants' caps together would exceed this node's capacity (kernel.tenants.capacity.cpu): 65 with this resize, 64 allowed"
    );
    assert_eq!(
        refused(&rig, A, json!({ "memory": "113GiB" })),
        "resize tenant aaaaaaaa: the tenants' caps together would exceed this node's capacity (kernel.tenants.capacity.memory): 129.0 GiB with this resize, 128.0 GiB allowed"
    );
    assert_eq!(
        refused(&rig, A, json!({ "disk": "2TiB" })),
        "resize tenant aaaaaaaa: the tenants' caps together would exceed this node's capacity (kernel.tenants.capacity.disk): 2148.0 GiB with this resize, 2048.0 GiB allowed"
    );
    // The node's own virtual-machine cap covers every tenant's machines: 4, of which B holds one.
    assert_eq!(
        refused(&rig, A, json!({ "virtualMachines": 4 })),
        "resize tenant aaaaaaaa: the tenants' virtual machines (virtualMachines) together with the node's own would exceed the node's virtual-machine cap: 5 with this resize, 4 allowed"
    );
    assert_eq!(limits(&rig, A), DEFAULT);
    // Exactly the room that is left is given.
    let grown = resize(
        &rig,
        A,
        json!({ "cpu": 56, "memory": "112GiB", "virtualMachines": 3 }),
    )
    .expect("the grow to the capacity");
    assert_eq!(grown["caps"]["cpu"], 56);
    assert_eq!(limits(&rig, A), ["4", "4", "3", "56", "112GiB", "100GiB"]);
    // The room one tenant took is not there for the other.
    assert_eq!(
        refused(&rig, B, json!({ "cpu": 9 })),
        "resize tenant bbbbbbbb: the tenants' caps together would exceed this node's capacity (kernel.tenants.capacity.cpu): 65 with this resize, 64 allowed"
    );
    // The instances count is bounded by the capacity too.
    let mut tight = policy();
    tight["capacity"]["instances"] = json!(9);
    let rig = two_tenants(tight);
    assert_eq!(
        refused(&rig, B, json!({ "instances": 6 })),
        "resize tenant bbbbbbbb: the tenants' caps together would exceed this node's capacity (kernel.tenants.capacity.instances): 10 with this resize, 9 allowed"
    );
    resize(&rig, B, json!({ "instances": 5 })).expect("the last instance of the capacity");
    // A tenant that asks for less is never refused for the room the others take.
    resize(&rig, B, json!({ "instances": 2, "containers": 2 })).expect("a shrink");
}

#[test]
fn a_resize_past_the_tenants_address_block_is_refused_and_never_moves_the_tenant() {
    let rig = two_tenants(policy());
    // A holds the subnet's top /29, which lacks the broadcast address: 7 instances. B's /29 holds 8.
    assert_eq!(rig.entry("tenant-aaaaaaaa")["block"], "192.0.2.248/29");
    assert_eq!(rig.entry("tenant-bbbbbbbb")["block"], "192.0.2.240/29");
    assert_eq!(
        refused(&rig, A, json!({ "instances": 8 })),
        "resize tenant aaaaaaaa: its address block 192.0.2.248/29 holds 7 instances, and 8 were asked (limits.instances). More would need a bigger block at other addresses, and a tenant is never re-addressed: create a new tenant with the larger cap"
    );
    assert_eq!(
        refused(&rig, B, json!({ "instances": 9 })),
        "resize tenant bbbbbbbb: its address block 192.0.2.240/29 holds 8 instances, and 9 were asked (limits.instances). More would need a bigger block at other addresses, and a tenant is never re-addressed: create a new tenant with the larger cap"
    );
    let web = create(&rig, "web", "app", A);
    resize(&rig, A, json!({ "instances": 7, "containers": 7 })).expect("the whole block");
    resize(&rig, B, json!({ "instances": 8 })).expect("the whole block");
    for id in ["tenant-aaaaaaaa", "tenant-bbbbbbbb"] {
        assert!(rig.entry(id)["block"].as_str().unwrap().ends_with("/29"));
    }
    assert_eq!(rig.entry("tenant-aaaaaaaa")["block"], "192.0.2.248/29");
    // The service the tenant held keeps its address, and every address of the block is one an instance can take.
    assert_eq!(
        rig.call("service_status", json!({ "name": "web", "tenant": A }))
            .unwrap()["ipv4"],
        web["ipv4"]
    );
    // A tenant created with a /28 keeps it when it shrinks, and may grow back inside it.
    let large = json!({ "instances": 12, "containers": 12, "virtualMachines": 0, "cpu": 8,
        "memory": "16GiB", "disk": "100GiB" });
    let made = rig
        .approved("tenant_create", json!({ "id": "cccccccc", "caps": large }))
        .expect("a tenant of twelve");
    assert_eq!(made["block"], "192.0.2.224/28");
    let small = resize(&rig, "cccccccc", json!({ "instances": 4, "containers": 4 })).unwrap();
    assert_eq!(small["block"], "192.0.2.224/28");
    assert_eq!(
        resize(
            &rig,
            "cccccccc",
            json!({ "instances": 16, "containers": 16 })
        )
        .unwrap()["block"],
        "192.0.2.224/28"
    );
    // Seventeen is past every block.
    let error = rig
        .call(
            "action_propose",
            json!({ "action": "tenant_resize",
                "args": { "id": "cccccccc", "caps": { "instances": 17 } } }),
        )
        .expect_err("past a /28");
    assert!(
        matches!(&error, CallError::Invalid(why) if why.contains("instances of 1 to 16")),
        "{error:?}"
    );
}

#[test]
fn a_resize_names_a_tenant_the_node_holds_and_caps_that_change_something() {
    let rig = two_tenants(policy());
    let (code, message) = refusal(rig.call(
        "action_propose",
        json!({ "action": "tenant_resize", "args": { "id": "cccccccc", "caps": { "cpu": 4 } } }),
    ));
    assert_eq!(
        (code.as_str(), message.as_str()),
        ("not_found", "tenant cccccccc: not found")
    );
    for (args, why) in [
        (json!({ "id": "NOT-AN-ID", "caps": { "cpu": 4 } }), "id"),
        (json!({ "caps": { "cpu": 4 } }), "id"),
        (json!({ "id": A }), "caps"),
        (json!({ "id": A, "caps": { "cpus": 4 } }), "caps"),
        (json!({ "id": A, "caps": { "cpu": 0 } }), "caps"),
        (json!({ "id": A, "caps": { "memory": "lots" } }), "caps"),
        (json!({ "id": A, "caps": { "containers": 5 } }), "caps"),
        (
            json!({ "id": A, "caps": { "cpu": 4 }, "tenant": B }),
            "a field the schema does not name",
        ),
    ] {
        let error = rig
            .call(
                "action_propose",
                json!({ "action": "tenant_resize", "args": args }),
            )
            .expect_err("a malformed resize");
        assert!(
            matches!(&error, CallError::Invalid(text) if text.contains(why)),
            "{args}: {error:?}"
        );
    }
    // Caps that are the caps the tenant has change nothing, so the owner is not asked.
    for caps in [
        json!({}),
        json!({ "cpu": 8 }),
        json!({ "instances": 4, "memory": "16GiB" }),
    ] {
        assert_eq!(
            refused(&rig, A, caps),
            "resize tenant aaaaaaaa: the caps asked are the caps it has; nothing would change"
        );
    }
    assert_eq!(waiting(&rig), 0);
    assert_eq!(limits(&rig, A), DEFAULT);
    // A node without the key has no tenant to resize, proposed or called.
    let rig = Rig::new(None);
    for result in [
        rig.call(
            "action_propose",
            json!({ "action": "tenant_resize", "args": { "id": A, "caps": { "cpu": 4 } } }),
        ),
        rig.call(
            "tenant_resize",
            json!({ "id": A, "caps": { "cpu": 4 }, "approval": "x" }),
        ),
    ] {
        let (code, message) = refusal(result);
        assert_eq!(
            (code.as_str(), message.as_str()),
            ("no_tenants", "this node has no tenants")
        );
    }
    assert!(rig.fake().writes().is_empty());
}

#[test]
fn a_resize_runs_only_from_the_owners_approval_whatever_the_caps() {
    let rig = two_tenants(policy());
    let writes = rig.fake().writes().len();
    // The words of every verb that runs only from an approved proposal.
    let (_, approval_only) =
        refusal(rig.call("tenant_remove", json!({ "id": B, "approval": "x" })));
    // Under the node's defaults, at them and above them; with no approval, a forged one, and from a token whose tool
    // allowlist names the verb: a direct call is never a resize.
    for caps in [
        json!({ "cpu": 2 }),
        json!({ "cpu": 8, "memory": "8GiB" }),
        json!({ "cpu": 32 }),
    ] {
        for approval in [None, Some("0123456789abcdef0123456789abcdef")] {
            for tools in [None, Some(&["tenant_resize", "tenant_create"][..])] {
                let mut args = json!({ "id": A, "caps": caps });
                if let Some(approval) = approval {
                    args["approval"] = json!(approval);
                }
                let (code, message) = refusal(rig.call_as("tenant_resize", args, tools));
                assert_eq!(code, "NotApproved");
                assert_eq!(message, approval_only);
            }
        }
    }
    assert_eq!(
        approval_only,
        "this action runs only from an approved proposal: propose it with action_propose, and it runs once the owner approves it"
    );
    assert_eq!(limits(&rig, A), DEFAULT);
    assert_eq!(rig.fake().writes().len(), writes, "nothing reached Incus");
    // A token below doctor may not call it at all.
    let viewer = rig.handlers["tenant_resize"].call(
        json!({ "id": A, "caps": { "cpu": 2 }, "approval": "x" }),
        &Caller {
            token_id: "viewer".into(),
            scope: Scope::Viewer,
            tools: None,
        },
    );
    assert!(
        matches!(viewer, Err(CallError::Forbidden { .. })),
        "{viewer:?}"
    );
    // The same change, proposed and approved, runs: even one inside the node's defaults.
    let done = resize(&rig, A, json!({ "cpu": 2 })).expect("the approved resize");
    assert_eq!(done["caps"]["cpu"], 2);
}

#[test]
fn an_approval_is_bound_to_its_tenant_and_its_caps() {
    let rig = two_tenants(policy());
    let for_a = propose(&rig, A, json!({ "cpu": 16 }));
    let for_b = propose(&rig, B, json!({ "cpu": 16 }));
    let other_caps = propose(&rig, A, json!({ "cpu": 32 }));
    // The digest covers the tenant and the caps: three proposals, three digests, and the same one twice is one record.
    let digests: std::collections::BTreeSet<&str> = [&for_a, &for_b, &other_caps]
        .iter()
        .map(|proposed| proposed["argsDigest"].as_str().unwrap())
        .collect();
    assert_eq!(digests.len(), 3);
    assert_eq!(
        for_a["args"],
        json!({ "id": A, "caps": { "cpu": 16 } }),
        "the arguments are bound exactly as proposed"
    );
    assert_eq!(
        for_a["argsDigest"].as_str().unwrap(),
        arca::action_proposal::action_args_digest("tenant_resize", &for_a["args"]).unwrap()
    );
    assert_eq!(propose(&rig, A, json!({ "cpu": 16 }))["id"], for_a["id"]);
    assert_eq!(waiting(&rig), 3);
    // The owner's statement over one proposal approves no other: not another tenant's, not other caps for the same.
    let nonce = format!("{:064x}", 900);
    let statement = arca::handlers::action_statement(
        "node1",
        for_a["id"].as_str().unwrap(),
        "tenant_resize",
        for_a["argsDigest"].as_str().unwrap(),
        rig.now(),
        &nonce,
    )
    .unwrap();
    let signed = common::approve_statement(&rig.key, &statement);
    for other in [&for_b, &other_caps] {
        let spent = arca::handlers::approve_action(
            &mut rig.kernel.lock().unwrap(),
            &json!({ "plan": other["id"], "approval": signed }),
            rig.now(),
        );
        assert!(spent.is_err(), "an approval for A at 16 CPUs ran {other}");
    }
    assert_eq!(waiting(&rig), 3, "no proposal was retired");
    // The grant an approval arms runs its own tenant and its own caps, once.
    let approved = approve(&rig, &for_a);
    assert_eq!(approved.args, json!({ "id": A, "caps": { "cpu": 16 } }));
    for forged in [
        json!({ "id": B, "caps": { "cpu": 16 } }),
        json!({ "id": A, "caps": { "cpu": 32 } }),
        json!({ "id": A, "caps": { "cpu": 16, "memory": "32GiB" } }),
        json!({ "id": A, "caps": { "cpu": 16, "memory": "16GiB" } }),
    ] {
        let mut call = forged.clone();
        call["approval"] = json!(approved.grant);
        let (code, _) = refusal(rig.call("tenant_resize", call));
        assert_eq!(code, "NotApproved", "{forged}");
    }
    assert_eq!(limits(&rig, A), DEFAULT);
    assert_eq!(limits(&rig, B), DEFAULT);
    let done = run(&rig, &approved).expect("the approved resize");
    assert_eq!(done["caps"]["cpu"], 16);
    assert_eq!(limits(&rig, A)[3], "16");
    assert_eq!(limits(&rig, B), DEFAULT);
    let (code, _) = refusal(run(&rig, &approved));
    assert_eq!(code, "NotApproved", "the grant is spent");
}

#[test]
fn the_card_shows_the_tenant_and_every_cap_as_old_to_new() {
    let rig = two_tenants(policy());
    create(&rig, "web", "app", A);
    let proposed = propose(
        &rig,
        A,
        json!({ "instances": 4, "cpu": 16, "memory": "32GiB", "disk": "60GiB" }),
    );
    let card = json!([
        { "label": "Tenant", "value": "aaaaaaaa" },
        { "label": "Instances", "value": "4 (unchanged)" },
        { "label": "Containers", "value": "4 (unchanged)" },
        { "label": "Virtual machines", "value": "1 (unchanged)" },
        { "label": "CPUs", "value": "8 -> 16" },
        { "label": "Memory", "value": "16GiB -> 32GiB" },
        { "label": "Disk", "value": "100GiB -> 60GiB" }
    ]);
    assert_eq!(proposed["details"], card);
    assert_eq!(
        proposed["summary"],
        "Change the caps of the tenant aaaaaaaa"
    );
    assert_eq!(proposed["action"], "tenant_resize");
    // The proposal changed nothing.
    assert_eq!(limits(&rig, A), DEFAULT);
    let listed = rig.call("plan_list", json!({})).unwrap();
    assert_eq!(listed["plans"][0]["details"], card);
    // A restart reads the card back from the proposals journal, old values and all.
    let rig = rig.restart();
    let listed = rig.call("plan_list", json!({})).unwrap();
    assert_eq!(listed["plans"][0]["id"], proposed["id"]);
    assert_eq!(listed["plans"][0]["details"], card);
    assert_eq!(
        listed["plans"][0]["summary"],
        "Change the caps of the tenant aaaaaaaa"
    );
    // And what the owner approves from that card is what runs.
    let done = run(&rig, &approve(&rig, &listed["plans"][0])).expect("the approved resize");
    assert_eq!(
        done["caps"],
        json!({ "instances": 4, "containers": 4, "virtualMachines": 1, "cpu": 16,
            "memory": "32GiB", "disk": "60GiB" })
    );
}

#[test]
fn resizing_one_tenant_changes_nothing_of_another() {
    let rig = two_tenants(policy());
    create(&rig, "web", "app", A);
    create(&rig, "web", "app", B);
    create(&rig, "box", "vm", B);
    let of_b = |rig: &Rig| {
        (
            rig.get(&format!("/1.0/projects/{0}?project={0}", project(B)))
                .expect("B's project"),
            rig.get(&format!(
                "/1.0/profiles/arca-service?project={}",
                project(B)
            ))
            .expect("B's profile"),
            instance(rig, B, "web"),
            instance(rig, B, "box"),
            rig.entry("tenant-bbbbbbbb"),
            rig.entry("t-bbbbbbbb-service-web"),
            rig.call("tenant_status", json!({ "id": B })).unwrap(),
            rig.call("tenant_usage", json!({ "id": B })).unwrap(),
            rig.call("service_list", json!({ "tenant": B })).unwrap(),
        )
    };
    let before = of_b(&rig);
    let own = rig.call("service_list", json!({})).unwrap();
    let writes = rig.fake().writes().len();
    resize(
        &rig,
        A,
        json!({ "instances": 6, "containers": 6, "cpu": 24, "memory": "48GiB", "disk": "300GiB" }),
    )
    .expect("the grow of A");
    resize(
        &rig,
        A,
        json!({ "cpu": 2, "memory": "2GiB", "disk": "16GiB" }),
    )
    .expect("the shrink");
    assert_eq!(of_b(&rig), before, "B is as it was");
    assert_eq!(limits(&rig, B), DEFAULT);
    assert_eq!(rig.call("service_list", json!({})).unwrap(), own);
    // Every write of the two resizes names A's project, and nothing else.
    let made = rig.fake().writes()[writes..].to_vec();
    assert_eq!(made.len(), 2);
    for write in made {
        assert!(
            write.path.starts_with("/1.0/projects/arcak-t-aaaaaaaa"),
            "{} {}",
            write.method,
            write.path
        );
    }
    // B's own room is whole: its caps, not A's, decide what it may create.
    create(&rig, "api", "app", B);
    let (_, message) = refusal(rig.call(
        "service_create",
        json!({ "name": "api", "template": "app", "tenant": A }),
    ));
    assert!(
        message
            .contains("tenant aaaaaaaa's cap on CPUs (limits.cpu): 2 in use, 2 asked, 2 allowed"),
        "{message}"
    );
}

#[test]
fn a_kill_in_the_middle_of_a_resize_leaves_one_set_of_caps_and_the_next_start_reconciles_them() {
    let new = ["4", "4", "1", "16", "32GiB", "100GiB"];
    for kill in [Kill::BeforeWrite, Kill::AfterWrite] {
        let rig = two_tenants(policy());
        create(&rig, "web", "app", A);
        let before = instance(&rig, A, "web");
        *rig.kill.lock().unwrap() = Some(kill);
        let killed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            resize(&rig, A, json!({ "cpu": 16, "memory": "32GiB" }))
        }));
        assert!(killed.is_err(), "{kill:?}: the process died in the write");
        // What the dead process left: the record of the new caps and the write's intent are on disk, and Incus holds
        // the old caps or the new, whole, depending on whether its request was sent.
        let record: Value = serde_json::from_slice(
            &fs::read(rig.dir.join("desired.json")).expect("the desired state"),
        )
        .expect("JSON");
        assert!(
            record.to_string().replace('\\', "").contains(r#""cpu":16"#),
            "{kill:?}: the record was durable before Incus was asked"
        );
        let journal = |rig: &Rig| {
            arca_kernel::journal::read_journal(&rig.dir.join(arca::kernel::JOURNAL_FILE))
                .expect("the journal")
                .0
        };
        let open = |rig: &Rig| {
            let mut state = std::collections::BTreeMap::new();
            for record in journal(rig) {
                state.insert(record.id, record.state);
            }
            state
                .values()
                .filter(|state| **state == arca_kernel::journal::EffectState::Intent)
                .count()
        };
        assert_eq!(open(&rig), 1, "{kill:?}: the write's intent has no outcome");
        assert_eq!(
            limits(&rig, A),
            if kill == Kill::AfterWrite {
                new.to_vec()
            } else {
                DEFAULT.to_vec()
            },
            "{kill:?}"
        );
        // The next start settles the intent and brings the record and Incus together, at the caps the owner approved.
        let rig = rig.restart();
        assert_eq!(open(&rig), 0, "{kill:?}: the intent is settled");
        assert_eq!(limits(&rig, A), new, "{kill:?}: Incus");
        assert_eq!(
            rig.entry("tenant-aaaaaaaa")["caps"],
            json!({ "instances": 4, "containers": 4, "virtualMachines": 1, "cpu": 16,
                "memory": "32GiB", "disk": "100GiB" }),
            "{kill:?}: the record"
        );
        let status = rig.call("tenant_status", json!({ "id": A })).unwrap();
        assert_eq!(status["caps"]["cpu"], 16);
        assert_eq!(status["caps"]["memory"], "32GiB");
        assert_eq!(limits(&rig, B), DEFAULT, "{kill:?}: B");
        assert_eq!(instance(&rig, A, "web"), before, "{kill:?}: the instance");
        // The kernel is whole again: it creates, and it resizes.
        create(&rig, "api", "app", A);
        resize(&rig, A, json!({ "cpu": 12 })).expect("a resize after the restart");
        assert_eq!(limits(&rig, A)[3], "12");
        // A second start changes nothing more.
        let writes = rig.fake().writes().len();
        let rig = rig.restart();
        assert_eq!(rig.fake().writes().len(), writes, "{kill:?}");
        assert_eq!(limits(&rig, A)[3], "12");
    }
}

#[test]
fn a_write_incus_refuses_leaves_the_old_caps_in_the_record_and_in_incus() {
    let rig = two_tenants(policy());
    create(&rig, "web", "app", A);
    rig.fake().set_reject_put("/1.0/projects/arcak-t-aaaaaaaa");
    let (_, message) = refusal(resize(&rig, A, json!({ "cpu": 16, "memory": "32GiB" })));
    assert!(
        message.starts_with("resize tenant aaaaaaaa: ")
            && message.ends_with("; its caps are unchanged"),
        "{message}"
    );
    assert_eq!(limits(&rig, A), DEFAULT);
    assert_eq!(rig.entry("tenant-aaaaaaaa")["caps"]["cpu"], 8);
    let status = rig.call("tenant_status", json!({ "id": A })).unwrap();
    assert_eq!(status["caps"]["cpu"], 8);
    assert_eq!(status["caps"]["memory"], "16GiB");
    // Nothing is left half done: the kernel creates in the tenant, B resizes, and a restart writes nothing.
    create(&rig, "api", "app", A);
    resize(&rig, B, json!({ "cpu": 16 })).expect("B's resize");
    let writes = rig.fake().writes().len();
    let rig = rig.restart();
    assert_eq!(rig.fake().writes().len(), writes);
    assert_eq!(limits(&rig, A), DEFAULT);
    assert_eq!(rig.entry("tenant-aaaaaaaa")["caps"]["cpu"], 8);
}
