//! What the create refuses for what the node's config and records hold is refused when the proposal is made, in the
//! create's own words, and no card is recorded: a name that is taken, a deploy the approved create could not add to,
//! a tenant's machine on a host bridge, a tenant's service from a template that opens a host port, a MAC another
//! record holds, a config with no service project, and a count of virtual machines the config sets below what the
//! node holds. What is read from Incus stays with the create. Every name and address here is synthetic.
use super::*;

const TOOLS: [&str; 3] = ["service_create", "deploy_confirm", "deploy_rollback"];

fn propose(rig: &Rig, args: Value) -> Result<Value, CallError> {
    rig.call(
        "action_propose",
        json!({ "action": "service_create", "args": args }),
    )
}

fn waiting(rig: &Rig) -> usize {
    rig.kernel.lock().expect("kernel").action_proposals().len()
}

/// Approves a waiting proposal and runs it, as the approval page does.
fn approve(rig: &Rig, proposal: &Value) -> Result<Value, CallError> {
    let id = proposal["id"].as_str().expect("the proposal's id");
    let digest = proposal["argsDigest"].as_str().expect("its digest");
    let nonce = format!("{:064x}", rig.nonce.fetch_add(1, Ordering::SeqCst) + 1);
    let statement =
        arca::handlers::action_statement("node1", id, "service_create", digest, rig.now(), &nonce)
            .expect("the statement");
    let approved = arca::handlers::approve_action(
        &mut rig.kernel.lock().expect("kernel"),
        &json!({ "plan": id, "approval": common::approve_statement(&rig.key, &statement) }),
        rig.now(),
    )?;
    ActionExecutor::new(
        rig.handlers
            .iter()
            .map(|(verb, handler)| (*verb, Arc::clone(handler))),
    )
    .run(&approved.action, &approved.args, &approved.grant)
}

/// The create as the approved action runs it: under the approval's own name, never under a token's.
fn as_the_approved_create(rig: &Rig, args: Value) -> Result<Value, CallError> {
    rig.call_token("owner-approval", "service_create", args, &TOOLS)
}

/// Runs a proposal that must be refused, and checks that it left no card, wrote nothing to Incus and changed no
/// file under the node's state directory (the proposals' journal, the deploy journal and the kernel's own among
/// them).
fn untouched(
    rig: &Rig,
    proposal: impl FnOnce() -> Result<Value, CallError>,
) -> Result<Value, CallError> {
    let cards = waiting(rig);
    let writes = rig.fake().writes().len();
    let before = room_at_propose::state(rig);
    let answer = proposal();
    assert!(answer.is_err(), "the proposal is refused: {answer:?}");
    assert_eq!(waiting(rig), cards, "no card waits");
    assert_eq!(rig.fake().writes().len(), writes, "nothing was written");
    assert!(
        room_at_propose::state(rig) == before,
        "a refused proposal changed a file"
    );
    answer
}

/// The proposal is refused with exactly what the create answers a token for the same arguments, no card waits and
/// nothing is written: not to Incus, and no file under the node's state directory changes.
fn refused_as_the_create(rig: &Rig, args: Value, words: &str) {
    let create = refusal(rig.call("service_create", args.clone()));
    assert_eq!(create.1, words, "the create's own words");
    assert_eq!(refusal(untouched(rig, || propose(rig, args))), create);
}

/// A node with the service network, one host bridge and room for virtual machines; with tenants when asked.
fn mixed(virtual_machines: u64, tenants: bool) -> Rig {
    let mut kernel = json!({
        "projects": [PROJECT],
        "droiddProjects": ["other"],
        "serviceCidr": CIDR,
        "bridged": { "bridges": ["br0"], "pool": "vmpool", "limits": {
            "virtualMachines": virtual_machines, "cpu": 16, "memory": "64GiB", "disk": "1TiB" } }
    });
    if tenants {
        kernel["tenants"] = policy();
    }
    let rig = Rig::with_kernel(kernel, empty_state());
    rig.template("app", app_template("app"));
    // A machine with no ports: the kind a host bridge takes.
    let mut bare = vm_template();
    bare["name"] = json!("bare");
    bare["ports"] = json!([]);
    rig.template("bare", bare);
    rig
}

#[test]
fn a_name_a_service_already_holds_is_refused_when_it_is_proposed() {
    let rig = two_tenants(policy());
    rig.call(
        "service_create",
        json!({ "name": "one", "template": "app" }),
    )
    .expect("the node's own service");
    rig.call(
        "service_create",
        json!({ "name": "one", "template": "app", "tenant": A }),
    )
    .expect("a tenant's service of the same name");
    refused_as_the_create(
        &rig,
        json!({ "name": "one", "template": "app" }),
        "service one exists",
    );
    refused_as_the_create(
        &rig,
        json!({ "name": "one", "template": "app", "tenant": A }),
        "service one exists",
    );
    // The name is free in the other tenant.
    propose(
        &rig,
        json!({ "name": "one", "template": "app", "tenant": B }),
    )
    .expect("another tenant's name space is its own");
}

#[test]
fn a_deploy_named_outside_a_tenant_is_refused_when_it_is_proposed() {
    let rig = two_tenants(policy());
    let args = json!({ "name": "two", "template": "app", "deploy": "d1" });
    let create = rig
        .call("service_create", args.clone())
        .expect_err("refused");
    assert!(
        matches!(&create, CallError::Invalid(why) if why == "deploy: only a call inside a tenant names a deploy"),
        "{create:?}"
    );
    let proposed = untouched(&rig, || propose(&rig, args)).expect_err("refused");
    assert_eq!(format!("{proposed:?}"), format!("{create:?}"));
}

/// The approved create runs as the approval's own caller, never as a token. So a card may open a deploy, and a
/// second card may add to it while it is provisional; a card that names a deploy a token opened is refused when it
/// is proposed, and so is one that names a deploy that is closed. Each refusal is compared with what the approved
/// create itself answers for the same arguments.
#[test]
fn a_deploy_the_approved_create_could_not_add_to_is_refused_when_it_is_proposed() {
    let rig = two_tenants(policy());
    rig.call_token(
        "cloud",
        "service_create",
        json!({ "name": "db", "template": "app", "tenant": A, "deploy": "d1" }),
        &TOOLS,
    )
    .expect("a token opens the deploy d1");
    let args = json!({ "name": "web", "template": "app", "tenant": A, "deploy": "d1" });
    let create = refusal(as_the_approved_create(&rig, args.clone()));
    assert_eq!(
        create,
        (
            "refused".to_string(),
            "deploy d1 was opened by another caller: only the caller that opened a deploy adds to it, and an approved action runs as the owner's approval, never as the token that proposed it".to_string()
        )
    );
    assert_eq!(
        refusal(untouched(&rig, || propose(&rig, args.clone()))),
        create
    );
    // A proposal by the very token that opened the deploy gets the same sentence, and it is true for that token too.
    let by_its_opener = untouched(&rig, || {
        rig.call_token(
            "cloud",
            "action_propose",
            json!({ "action": "service_create", "args": args }),
            &["action_propose"],
        )
    });
    assert_eq!(refusal(by_its_opener), create);

    // A card opens a deploy of its own, and a second card adds to it.
    let first = propose(
        &rig,
        json!({ "name": "web", "template": "app", "tenant": A, "deploy": "d2" }),
    )
    .expect("a new deploy is proposed");
    approve(&rig, &first).expect("the approved create opens d2");
    let second = propose(
        &rig,
        json!({ "name": "side", "template": "app", "tenant": A, "deploy": "d2" }),
    )
    .expect("a card adds to the deploy a card opened");
    approve(&rig, &second).expect("and its create joins d2");
    // A token does not add to the deploy a card opened, and is told the same sentence.
    let (_, message) = refusal(rig.call_token(
        "cloud",
        "service_create",
        json!({ "name": "more", "template": "app", "tenant": A, "deploy": "d2" }),
        &TOOLS,
    ));
    assert!(
        message.starts_with("deploy d2 was opened by another caller:"),
        "{message}"
    );

    // Fifteen minutes later d2 is closed: a third card for it is refused when proposed, as its create would be.
    rig.clock.fetch_add(16 * 60 * 1000, Ordering::SeqCst);
    let args = json!({ "name": "late", "template": "app", "tenant": A, "deploy": "d2" });
    let create = refusal(as_the_approved_create(&rig, args.clone()));
    assert_eq!(
        create,
        (
            "refused".to_string(),
            "deploy d2 is closed: it was confirmed, rolled back, or opened more than 15 minutes ago. Use a new deploy id".to_string()
        )
    );
    assert_eq!(refusal(untouched(&rig, || propose(&rig, args))), create);
}

#[test]
fn a_tenants_machine_that_names_a_host_bridge_is_refused_when_it_is_proposed() {
    let rig = mixed(4, true);
    rig.create_tenant(json!({ "id": A })).expect("the tenant");
    refused_as_the_create(
        &rig,
        json!({ "name": "box", "template": "bare", "tenant": A, "bridge": "br0" }),
        "bridge: a virtual machine in a tenant joins the managed service network only, never a host bridge",
    );
    // Without the bridge the same machine is proposed, and the node's own machine may name the bridge.
    let card = propose(
        &rig,
        json!({ "name": "box", "template": "bare", "tenant": A }),
    )
    .expect("a tenant's machine on the managed network");
    assert!(card["args"].get("bridge").is_none(), "{card}");
    propose(
        &rig,
        json!({ "name": "own", "template": "bare", "bridge": "br0" }),
    )
    .expect("the node's own machine on the host bridge");
}

#[test]
fn a_tenants_service_from_a_template_that_opens_a_host_port_is_refused_when_it_is_proposed() {
    let rig = two_tenants(policy());
    let mut lan = app_template("lan");
    lan["lan"] = json!({ "port": 8080 });
    rig.template("lan", lan);
    rig.template(
        "dash",
        json!({
            "name": "dash", "kind": "system", "image": "images:debian/13", "ports": [], "env": {},
            "publishable": true, "dashPort": 3080
        }),
    );
    for template in ["lan", "dash"] {
        refused_as_the_create(
            &rig,
            json!({ "name": "open", "template": template, "tenant": A }),
            &format!(
                "template {template} opens a port on the host or publishes a dash, and nothing in a tenant does: a tenant's service is reached through the node's front service"
            ),
        );
        // The node's own service may be made from it.
        propose(&rig, json!({ "name": template, "template": template }))
            .expect("the node's own service is proposed");
    }
}

/// The half of the MAC check that needs no Incus: a MAC one of the node's own records holds. Whether an instance
/// Arca holds no record of has it is still asked of Incus by the create.
#[test]
fn a_mac_another_record_holds_is_refused_when_it_is_proposed() {
    let rig = mixed(4, false);
    rig.call(
        "service_create",
        json!({ "name": "m1", "template": "bare", "bridge": "br0", "mac": "02:00:00:00:00:01" }),
    )
    .expect("the machine that holds the MAC");
    refused_as_the_create(
        &rig,
        json!({ "name": "m2", "template": "bare", "bridge": "br0", "mac": "02:00:00:00:00:01" }),
        "the MAC 02:00:00:00:00:01 is already held by another instance",
    );
    // Stated in upper case: it is the same address.
    let (_, message) = refusal(untouched(&rig, || {
        propose(
            &rig,
            json!({ "name": "m2", "template": "bare", "bridge": "br0", "mac": "02:00:00:00:00:01".to_uppercase() }),
        )
    }));
    assert_eq!(
        message,
        "the MAC 02:00:00:00:00:01 is already held by another instance"
    );
    propose(
        &rig,
        json!({ "name": "m2", "template": "bare", "bridge": "br0", "mac": "02:00:00:00:00:02" }),
    )
    .expect("a free MAC is proposed");
}

/// A node whose config makes changes but leaves the service project out, or sets up no service network.
#[test]
fn a_config_with_no_service_project_is_refused_when_a_create_is_proposed() {
    let rig = Rig::with_kernel(
        json!({ "projects": ["elsewhere"], "droiddProjects": ["other"], "serviceCidr": CIDR }),
        empty_state(),
    );
    rig.template("app", app_template("app"));
    refused_as_the_create(
        &rig,
        json!({ "name": "one", "template": "app" }),
        "no service project: the service project is excluded by kernel configuration",
    );
    // No service network at all: the words name the keys the config needs.
    let rig = Rig::with_kernel(
        json!({ "projects": ["elsewhere"], "droiddProjects": ["other"] }),
        empty_state(),
    );
    rig.template("app", app_template("app"));
    let args = json!({ "name": "one", "template": "app" });
    let create = refusal(rig.call("service_create", args.clone()));
    assert!(
        create
            .1
            .starts_with("no service project: this node's config sets up no services."),
        "{}",
        create.1
    );
    assert_eq!(refusal(untouched(&rig, || propose(&rig, args))), create);
}

/// A node that ran services, restarted with a config that sets up none: it still holds their records, and it can
/// serve none of them. The create says so, and now the proposal does.
#[test]
fn records_of_services_without_a_config_for_them_refuse_a_proposed_create() {
    let mut rig = Rig::with_kernel(
        json!({ "projects": [PROJECT], "droiddProjects": ["other"], "serviceCidr": CIDR }),
        empty_state(),
    );
    rig.template("app", app_template("app"));
    rig.call(
        "service_create",
        json!({ "name": "one", "template": "app" }),
    )
    .expect("a service while the config sets services up");
    rig.kernel_config = json!({ "projects": ["elsewhere"], "droiddProjects": ["other"] });
    let rig = rig.restart();
    let args = json!({ "name": "two", "template": "app" });
    let create = refusal(rig.call("service_create", args.clone()));
    assert!(
        create.1.starts_with(
            "no service project: this node's config sets up no services, but the node still holds records of services it ran."
        ),
        "{}",
        create.1
    );
    assert_eq!(refusal(untouched(&rig, || propose(&rig, args))), create);
}

/// A node that only looks is left as it was: a proposed create is recorded there, for a container and for a machine
/// on the service network alike, and it is the create that answers that the node only looks. Nothing about the
/// node's project is asked when the proposal is made.
#[test]
fn a_node_that_only_looks_still_records_a_proposed_create() {
    let rig = Rig::with_kernel(
        json!({
            "droiddProjects": [],
            "serviceCidr": CIDR,
            "vmLimits": { "virtualMachines": 4, "cpu": 16, "memory": "64GiB", "disk": "1TiB" }
        }),
        empty_state(),
    );
    rig.template("app", app_template("app"));
    rig.template("vm", vm_template());
    for (name, template) in [("one", "app"), ("box", "vm")] {
        let args = json!({ "name": name, "template": template });
        let card = propose(&rig, args.clone())
            .unwrap_or_else(|error| panic!("{template}: a card is recorded as before: {error:?}"));
        assert_eq!(card["args"]["name"], name);
        let (code, message) = refusal(rig.call("service_create", args));
        assert_eq!(code, "refused");
        assert!(
            message.starts_with("this node only looks: nothing here can be changed"),
            "{template}: {message}"
        );
    }
    assert_eq!(waiting(&rig), 2);
}

/// The node's config is changed to allow fewer virtual machines than the node holds, and the node is restarted. The
/// first container needs the node's project widened to the service network, and that write would carry the lower
/// count: the create refuses it for want of room, and now the proposal does, in the same words.
#[test]
fn a_count_of_virtual_machines_below_what_the_node_holds_is_refused_when_a_create_is_proposed() {
    let mut rig = mixed(2, false);
    for name in ["m1", "m2"] {
        rig.call(
            "service_create",
            json!({ "name": name, "template": "bare", "bridge": "br0" }),
        )
        .unwrap_or_else(|error| panic!("{name}: {error:?}"));
    }
    rig.kernel_config["bridged"]["limits"]["virtualMachines"] = json!(1);
    let rig = rig.restart();
    rig.template("app", app_template("app"));
    let args = json!({ "name": "one", "template": "app" });
    let create = listed(rig.call("service_create", args.clone()));
    assert_eq!(create.0, "capacity");
    assert_eq!(
        create.1,
        "this node holds 2 virtual machines, but kernel.bridged.limits.virtualMachines is 1: raise it to at least 2, or remove virtual machines first. Nothing was written"
    );
    assert_eq!(listed(untouched(&rig, || propose(&rig, args))), create);
}

/// A tenant's instance is named `<service>-<tenant id>` and the node's own is named as its service, so the node's
/// service `x-<id>` and tenant `<id>`'s service `x` would be one instance name. Whichever comes second is refused,
/// by the create and when it is proposed, from the node's records alone. The words say the name is taken on this
/// node and nothing of the service that has it, which may be another side's.
#[test]
fn an_instance_name_a_service_of_another_side_has_is_refused_in_either_order() {
    let taken = |name: &str| {
        format!(
            "the name {name} is taken on this node: the instance this service would get has the name of another service's instance. Choose another name"
        )
    };
    let on_the_node = format!("x-{A}");
    // The tenant's service first, then the node's own under the tenant's instance name.
    let rig = two_tenants(policy());
    rig.call(
        "service_create",
        json!({ "name": "x", "template": "app", "tenant": A }),
    )
    .expect("the tenant's service x");
    refused_as_the_create(
        &rig,
        json!({ "name": on_the_node, "template": "app" }),
        &taken(&on_the_node),
    );
    // Neither the other tenant's x nor the node's own x is that instance.
    propose(&rig, json!({ "name": "x", "template": "app", "tenant": B }))
        .expect("another tenant's x is its own instance");
    propose(&rig, json!({ "name": "x", "template": "app" })).expect("the node's own x");

    // The node's service first, then the tenant's service whose instance would have its name.
    let rig = two_tenants(policy());
    rig.call(
        "service_create",
        json!({ "name": on_the_node, "template": "app" }),
    )
    .expect("the node's own service");
    refused_as_the_create(
        &rig,
        json!({ "name": "x", "template": "app", "tenant": A }),
        &taken("x"),
    );
    propose(&rig, json!({ "name": "x", "template": "app", "tenant": B }))
        .expect("the other tenant's x is free");
}
