//! A card the node has no room for never reaches the owner. Every cap a `service_create` is held to (the node's caps
//! on virtual machines, under `kernel.vmLimits` or `kernel.bridged.limits`, and the caps of the tenant it names) is
//! checked when the proposal is made, in the words the create uses, and again by the create, because the node can
//! fill up while a card waits. Every name and amount here is synthetic.
use super::*;

const GIB: u64 = 1 << 30;

/// A node whose caps on virtual machines are `limits`, stated under `key`: `vmLimits`, or `bridged` for a node that
/// also names a host bridge, whose caps are `kernel.bridged.limits`.
fn node(key: &str, limits: Value) -> Rig {
    let mut kernel = json!({
        "projects": [PROJECT],
        "droiddProjects": ["other"],
        "serviceCidr": CIDR
    });
    if key == "bridged" {
        kernel["bridged"] = json!({ "bridges": ["br0"], "pool": "vmpool", "limits": limits });
    } else {
        kernel[key] = limits;
    }
    let rig = Rig::with_kernel(kernel, empty_state());
    // 2 CPUs, 4 GiB of memory and a 20 GiB disk.
    rig.template("vm", vm_template());
    rig
}

fn propose(rig: &Rig, args: Value) -> Result<Value, CallError> {
    rig.call(
        "action_propose",
        json!({ "action": "service_create", "args": args }),
    )
}

/// The ids of the proposals that wait.
fn waiting(rig: &Rig) -> Vec<String> {
    rig.kernel
        .lock()
        .expect("kernel")
        .action_proposals()
        .iter()
        .map(|proposal| proposal.id.clone())
        .collect()
}

/// An owner's approval of a waiting proposal, signed once.
fn signed(rig: &Rig, proposal: &Value) -> Value {
    let id = proposal["id"].as_str().expect("the proposal's id");
    let digest = proposal["argsDigest"].as_str().expect("its digest");
    let nonce = format!("{:064x}", rig.nonce.fetch_add(1, Ordering::SeqCst) + 1);
    let statement =
        arca::handlers::action_statement("node1", id, "service_create", digest, rig.now(), &nonce)
            .expect("the statement");
    json!({ "plan": id, "approval": common::approve_statement(&rig.key, &statement) })
}

/// Submits an approval the way `action_approve` and the approval page do: what comes back is what the owner is told.
fn submit(rig: &Rig, approval: &Value) -> Result<Value, CallError> {
    let executor = ActionExecutor::new(
        rig.handlers
            .iter()
            .map(|(verb, handler)| (*verb, Arc::clone(handler))),
    );
    arca::handlers::run_approved_action(&rig.kernel, &executor, approval, rig.now())
}

fn room(limit: &str, have: u64, want: u64, max: u64) -> Value {
    json!({ "scope": "node", "limit": limit, "have": have, "want": want, "max": max })
}

fn has_entry(rig: &Rig, id: &str) -> bool {
    rig.entries().iter().any(|entry| entry.id.0 == id)
}

/// Each cap in turn is the one a second machine does not fit under, on a node that states its caps under
/// `kernel.vmLimits` and on one that states them under `kernel.bridged.limits`. The proposal is refused when it is
/// made, with the create's own sentence and fields: what is in use, what is asked, what is allowed, and the config
/// key. No card waits afterwards and nothing is written.
#[test]
fn a_proposal_over_a_cap_of_the_node_is_refused_when_it_is_made_and_no_card_waits() {
    for (key, source) in [
        ("vmLimits", "kernel.vmLimits"),
        ("bridged", "kernel.bridged.limits"),
    ] {
        for (limits, what, fields) in [
            (
                json!({ "virtualMachines": 1, "cpu": 8, "memory": "16GiB", "disk": "80GiB" }),
                format!(
                    "virtual machines ({source}.virtualMachines): 1 in use, 1 asked, 1 allowed"
                ),
                room("virtualMachines", 1, 1, 1),
            ),
            (
                json!({ "virtualMachines": 4, "cpu": 3, "memory": "16GiB", "disk": "80GiB" }),
                format!("CPUs ({source}.cpu): 2 in use, 2 asked, 3 allowed"),
                room("cpu", 2, 2, 3),
            ),
            (
                json!({ "virtualMachines": 4, "cpu": 8, "memory": "6GiB", "disk": "80GiB" }),
                format!("memory ({source}.memory): 4.0 GiB in use, 4.0 GiB asked, 6.0 GiB allowed"),
                room("memory", 4 * GIB, 4 * GIB, 6 * GIB),
            ),
            (
                json!({ "virtualMachines": 4, "cpu": 8, "memory": "16GiB", "disk": "30GiB" }),
                format!("disk ({source}.disk): 20.0 GiB in use, 20.0 GiB asked, 30.0 GiB allowed"),
                room("disk", 20 * GIB, 20 * GIB, 30 * GIB),
            ),
        ] {
            let rig = node(key, limits);
            rig.call("service_create", json!({ "name": "ws1", "template": "vm" }))
                .expect("the first machine fits");
            let writes = rig.fake().writes().len();
            let sentence = format!("creating ws2 would exceed this node's cap on {what}");
            assert_eq!(
                listed(propose(&rig, json!({ "name": "ws2", "template": "vm" }))),
                ("capacity".to_string(), sentence.clone(), fields.clone()),
            );
            assert_eq!(waiting(&rig), Vec::<String>::new(), "no card waits");
            assert_eq!(rig.fake().writes().len(), writes, "nothing was written");
            assert!(!has_entry(&rig, "service-ws2"));
            // The create itself says the same, word for word.
            assert_eq!(
                listed(rig.call("service_create", json!({ "name": "ws2", "template": "vm" }))),
                ("capacity".to_string(), sentence, fields),
            );
        }
    }
}

/// A second machine that brings every cap to exactly its amount is proposed, its card waits, and the approved create
/// makes it. A third is over every cap and is refused when proposed.
#[test]
fn a_proposal_exactly_at_the_caps_passes_and_is_created() {
    for key in ["vmLimits", "bridged"] {
        let rig = node(
            key,
            json!({ "virtualMachines": 2, "cpu": 4, "memory": "8GiB", "disk": "40GiB" }),
        );
        rig.call("service_create", json!({ "name": "ws1", "template": "vm" }))
            .expect("the first machine fits");
        let proposal = propose(&rig, json!({ "name": "ws2", "template": "vm" }))
            .expect("a machine exactly at the caps is proposed");
        assert_eq!(
            waiting(&rig),
            [proposal["id"].as_str().expect("its id").to_string()]
        );
        submit(&rig, &signed(&rig, &proposal)).expect("the approved create makes it");
        assert!(has_entry(&rig, "service-ws2"));
        let (code, message, _) = listed(propose(&rig, json!({ "name": "ws3", "template": "vm" })));
        assert_eq!(code, "capacity");
        assert!(
            message.contains("virtual machines")
                && message.contains("2 in use, 1 asked, 2 allowed"),
            "{message}"
        );
    }
}

/// The node fills up while a card waits. The proposal fit when it was made; a token's own create then takes the room.
/// The approval is accepted and spent, the proposal is retired, and the create refuses in its own words with the
/// amounts as they stand now: the owner is told the approval was accepted and that the action must be proposed again.
/// Submitting the same approval a second time finds no proposal.
#[test]
fn a_proposal_that_no_longer_fits_at_approval_is_refused_by_the_create_and_the_approval_is_spent() {
    let rig = node(
        "bridged",
        json!({ "virtualMachines": 4, "cpu": 4, "memory": "16GiB", "disk": "80GiB" }),
    );
    rig.call("service_create", json!({ "name": "ws1", "template": "vm" }))
        .expect("the first machine fits");
    let proposal =
        propose(&rig, json!({ "name": "ws2", "template": "vm" })).expect("it fits when proposed");
    rig.call("service_create", json!({ "name": "ws3", "template": "vm" }))
        .expect("another create takes the room while the card waits");
    let writes = rig.fake().writes().len();
    let approval = signed(&rig, &proposal);
    assert_eq!(
        listed(submit(&rig, &approval)),
        (
            "capacity".to_string(),
            "The approval was accepted, but service_create did not complete: creating ws2 would exceed this node's cap on CPUs (kernel.bridged.limits.cpu): 4 in use, 2 asked, 4 allowed".to_string(),
            json!({ "scope": "node", "limit": "cpu", "have": 4, "want": 2, "max": 4, "proposeAgain": true }),
        )
    );
    assert_eq!(rig.fake().writes().len(), writes, "nothing was written");
    assert!(!has_entry(&rig, "service-ws2"));
    assert_eq!(
        waiting(&rig),
        Vec::<String>::new(),
        "the proposal is retired"
    );
    // The approval is spent with it: the same one again finds nothing to approve.
    let (code, _) = refusal(submit(&rig, &approval));
    assert_eq!(code, "NoSuchPlan");
    // Proposed again while the node is still full, it is refused when made.
    let (code, message, _) = listed(propose(&rig, json!({ "name": "ws2", "template": "vm" })));
    assert_eq!(code, "capacity");
    assert!(message.starts_with("creating ws2 would exceed this node's cap on CPUs"));
}

/// A create that names a tenant is held to the tenant's caps when it is proposed too: over one, it is refused in the
/// create's words, with `scope` `tenant`; at one, it is proposed. A tenant the node does not hold is refused when the
/// proposal is made, as the create refuses it.
#[test]
fn a_proposal_over_a_cap_of_its_tenant_is_refused_when_it_is_made() {
    // Each tenant may hold one virtual machine and four containers.
    let rig = two_tenants(policy());
    let first = propose(
        &rig,
        json!({ "name": "box", "template": "vm", "tenant": A }),
    )
    .expect("the tenant's one machine is proposed");
    submit(&rig, &signed(&rig, &first)).expect("and created");
    let writes = rig.fake().writes().len();
    let over = json!({ "name": "box2", "template": "vm", "tenant": A });
    let told = listed(propose(&rig, over.clone()));
    assert_eq!(
        told,
        (
            "capacity".to_string(),
            "creating box2 would exceed tenant aaaaaaaa's cap on virtual machines (limits.virtual-machines): 1 in use, 1 asked, 1 allowed".to_string(),
            json!({ "scope": "tenant", "limit": "virtualMachines", "have": 1, "want": 1, "max": 1 }),
        )
    );
    assert_eq!(listed(rig.call("service_create", over)), told);
    assert_eq!(waiting(&rig), Vec::<String>::new(), "no card waits");
    assert_eq!(rig.fake().writes().len(), writes, "nothing was written");
    // A container counts against the tenant's containers: the fourth fits, the fifth instance does not.
    for name in ["one", "two", "three"] {
        rig.call(
            "service_create",
            json!({ "name": name, "template": "app", "tenant": A }),
        )
        .unwrap_or_else(|error| panic!("{name}: {error:?}"));
    }
    let (code, message, fields) = listed(propose(
        &rig,
        json!({ "name": "four", "template": "app", "tenant": A }),
    ));
    assert_eq!(code, "capacity");
    assert_eq!(
        message,
        "creating four would exceed tenant aaaaaaaa's cap on instances (limits.instances): 4 in use, 1 asked, 4 allowed"
    );
    assert_eq!(fields["scope"], "tenant");
    // The other tenant has room for the same create.
    propose(
        &rig,
        json!({ "name": "four", "template": "app", "tenant": B }),
    )
    .expect("a tenant under its caps is proposed");
    // A tenant this node does not hold: the proposal answers what the create answers.
    let unknown = json!({ "name": "lost", "template": "app", "tenant": "zzzzzzzz" });
    let create = refusal(rig.call("service_create", unknown.clone()));
    assert_eq!(refusal(propose(&rig, unknown)), create);
}
