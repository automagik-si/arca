//! A template that declares `kernelAccess`: its service, in the node's own project, gets one device that listens inside
//! the instance on loopback and connects to the kernel's `listen.proxy` on the host's loopback. A tenant never gets it,
//! a node without that listener gives none, and an ordinary template gets no such device. Every address is synthetic.
use super::*;
use arca_kernel::plugin::{EffectOp, op_digest};

/// The port the kernel's proxy listener has on the rig's node.
const KERNEL: &str = "127.0.0.1:7460";
/// The port the template names inside the instance.
const PORT: u16 = 7461;

/// The `listen.proxy` a node needs before it gives a service kernel access.
fn loopback() -> Value {
    json!({ "proxy": { "addr": KERNEL, "peer": "127.0.0.1" } })
}

fn cloud_template() -> Value {
    json!({
        "name": "cloud", "kind": "oci", "image": "docker.io/library/cloud", "digest": DIGEST,
        "ports": [8080], "env": {}, "kernelAccess": { "port": PORT }
    })
}

/// What a create of the `cloud` template states.
fn stated() -> Value {
    json!({ "port": PORT })
}

/// A node with tenants, the tenant `A`, the template `cloud` and the ordinary template `app`. `node` is what its config
/// holds beside the kernel key.
fn node(node: Value) -> Rig {
    let rig = Rig::with_node(
        json!({
            "projects": [PROJECT],
            "droiddProjects": ["other"],
            "serviceCidr": CIDR,
            "postgresImage": PG_IMAGE,
            "vmLimits": { "virtualMachines": 4, "cpu": 16, "memory": "64GiB", "disk": "1TiB" },
            "tenants": policy()
        }),
        node,
        empty_state(),
    );
    rig.template("cloud", cloud_template());
    rig.template("app", app_template("app"));
    rig.create_tenant(json!({ "id": A })).expect("tenant");
    rig
}

fn create_cloud(rig: &Rig) -> Value {
    rig.call(
        "service_create",
        json!({ "name": "cloud", "template": "cloud", "kernelAccess": stated() }),
    )
    .expect("the create")
}

fn instance(rig: &Rig, name: &str) -> Option<Value> {
    rig.get(&format!("/1.0/instances/{name}?project={PROJECT}"))
}

fn acl(rig: &Rig, name: &str) -> Option<Value> {
    rig.get(&format!(
        "/1.0/network-acls/{PROJECT}-svc-{name}?project=default"
    ))
}

/// The device every service with kernel access carries, and no other service does.
fn device() -> Value {
    json!({
        "type": "proxy",
        "bind": "instance",
        "listen": format!("tcp:127.0.0.1:{PORT}"),
        "connect": format!("tcp:{KERNEL}")
    })
}

/// The names of an instance's proxy devices.
fn proxies(row: &Value) -> Vec<String> {
    row["devices"]
        .as_object()
        .expect("devices")
        .iter()
        .filter(|(_, device)| device["type"] == "proxy")
        .map(|(name, _)| name.clone())
        .collect()
}

fn journal(rig: &Rig) -> Vec<Value> {
    fs::read_to_string(rig.dir.join("journal.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).expect("a journal row"))
        .collect()
}

fn row<'a>(proposal: &'a Value, label: &str) -> Option<&'a str> {
    proposal["details"]
        .as_array()
        .expect("details")
        .iter()
        .find(|row| row["label"] == label)
        .map(|row| row["value"].as_str().expect("words"))
}

/// Approves a waiting proposal with the owner's passkey and runs it, as the approval page does.
fn approve(rig: &Rig, proposal: &Value) -> Result<Value, CallError> {
    let id = proposal["id"].as_str().expect("the proposal's id");
    let action = proposal["action"].as_str().expect("its action");
    let digest = proposal["argsDigest"].as_str().expect("its digest");
    let nonce = format!("{:064x}", rig.nonce.fetch_add(1, Ordering::SeqCst) + 1);
    let statement =
        arca::handlers::action_statement("node1", id, action, digest, rig.now(), &nonce)
            .expect("the statement");
    let approval = common::approve_statement(&rig.key, &statement);
    let approved = arca::handlers::approve_action(
        &mut rig.kernel.lock().expect("kernel"),
        &json!({ "plan": id, "approval": approval }),
        rig.now(),
    )?;
    ActionExecutor::new(
        rig.handlers
            .iter()
            .map(|(verb, handler)| (*verb, Arc::clone(handler))),
    )
    .run(&approved.action, &approved.args, &approved.grant)
}

fn propose(rig: &Rig, args: Value) -> Result<Value, CallError> {
    rig.call(
        "action_propose",
        json!({ "action": "service_create", "args": args }),
    )
}

#[test]
fn a_service_with_kernel_access_carries_one_device_of_exactly_this_shape_and_no_acl_rule_for_it() {
    let rig = node(loopback());
    let made = create_cloud(&rig);
    assert_eq!(made["state"], "running");
    assert_eq!(
        made["kernelAccess"],
        json!({ "port": PORT, "connect": KERNEL })
    );

    // The device: Incus listens inside the instance on its loopback and connects from the host to the kernel's
    // listener on the host's loopback. It is the instance's only proxy device.
    let held = instance(&rig, "cloud").expect("the instance");
    assert_eq!(held["devices"]["kernel"], device());
    assert_eq!(proxies(&held), ["kernel"]);
    // It was in the create itself: no later write added it.
    let creates: Vec<Value> = rig
        .fake()
        .service_requests()
        .iter()
        .filter(|request| request.method == "POST" && request.path.starts_with("/1.0/instances?"))
        .filter_map(|request| serde_json::from_slice::<Value>(&request.body).ok())
        .filter(|body| body["name"] == "cloud")
        .collect();
    assert_eq!(creates.len(), 1, "{creates:?}");
    assert_eq!(creates[0]["devices"]["kernel"], device());

    // The record names both ends, in the node's own project.
    let record = rig.entry("service-cloud");
    assert_eq!(
        record["kernelAccess"],
        json!({ "port": PORT, "connect": KERNEL })
    );
    assert_eq!(record["project"], PROJECT);
    assert!(record.get("tenant").is_none());

    // Nothing of it is on the bridge: the service's NIC ACL is rule for rule the ACL of an ordinary service, which
    // admits nothing from the bridge gateway and rejects it on the way out.
    rig.call(
        "service_create",
        json!({ "name": "web", "template": "app" }),
    )
    .expect("an ordinary service");
    let (cloud, web) = (
        acl(&rig, "cloud").expect("its ACL"),
        acl(&rig, "web").expect("the other ACL"),
    );
    assert_eq!(cloud["ingress"], json!([]));
    assert_eq!(cloud["ingress"], web["ingress"]);
    assert_eq!(cloud["egress"], web["egress"]);
    let gateway = record["gateway"].as_str().expect("the gateway");
    let to_gateway: Vec<&Value> = cloud["egress"]
        .as_array()
        .expect("egress")
        .iter()
        .filter(|rule| {
            rule["destination"]
                .as_str()
                .is_some_and(|to| to.split(',').any(|one| one == format!("{gateway}/32")))
        })
        .collect();
    assert!(!to_gateway.is_empty());
    assert!(to_gateway.iter().all(|rule| rule["action"] == "reject"));

    // service_status says the access stands; the list names it without checking it.
    let status = rig
        .call("service_status", json!({ "name": "cloud" }))
        .expect("status");
    assert_eq!(
        status["kernelAccess"],
        json!({ "port": PORT, "connect": KERNEL, "ready": true })
    );
    let listed = rig.call("service_list", json!({})).expect("list");
    let listed = listed["services"]
        .as_array()
        .expect("services")
        .iter()
        .find(|row| row["name"] == "cloud")
        .expect("its row")
        .clone();
    assert_eq!(
        listed["kernelAccess"],
        json!({ "port": PORT, "connect": KERNEL })
    );

    // The effect journal holds the create, and the digest it records is the digest of the create that carries the
    // device: a create without it has another digest.
    let record: arca::service::ServiceConfig =
        serde_json::from_value(rig.entry("service-cloud")).expect("the record");
    let digest_of = |record: &arca::service::ServiceConfig| {
        serde_json::to_value(op_digest(&EffectOp::IncusCreate {
            kind: arca_kernel::ownership::IncusKind::Instance,
            project: PROJECT.into(),
            parent: None,
            name: "cloud".into(),
            body: arca::service::create_body(record),
        }))
        .expect("a digest")
    };
    let applied: Vec<Value> = journal(&rig)
        .into_iter()
        .filter(|row| {
            row["entry"] == "service-cloud"
                && row["target"]["kind"] == "instance"
                && row["state"] == "Applied"
        })
        .collect();
    assert_eq!(applied.len(), 1, "{applied:?}");
    assert_eq!(applied[0]["op_digest"], digest_of(&record));
    let mut without = record.clone();
    without.kernel_access = None;
    assert_ne!(applied[0]["op_digest"], digest_of(&without));
}

#[test]
fn an_ordinary_template_gets_no_such_device_and_cannot_ask_for_one() {
    let rig = node(loopback());
    let made = rig
        .call(
            "service_create",
            json!({ "name": "web", "template": "app" }),
        )
        .expect("the create");
    assert!(made.get("kernelAccess").is_none(), "{made}");
    let held = instance(&rig, "web").expect("the instance");
    assert!(held["devices"].get("kernel").is_none());
    assert!(proxies(&held).is_empty(), "{held}");
    assert!(rig.entry("service-web").get("kernelAccess").is_none());
    let status = rig
        .call("service_status", json!({ "name": "web" }))
        .expect("status");
    assert!(status.get("kernelAccess").is_none(), "{status}");

    // The access is the template's to declare, never the call's: a call that states one for such a template is
    // refused before anything is written, as a create and as a proposal.
    let before = rig.fake().writes().len();
    for result in [
        rig.call(
            "service_create",
            json!({ "name": "other", "template": "app", "kernelAccess": stated() }),
        ),
        propose(
            &rig,
            json!({ "name": "other", "template": "app", "kernelAccess": stated() }),
        ),
    ] {
        assert!(
            matches!(
                &result,
                Err(CallError::Invalid(message))
                    if message == "kernelAccess: template app gives its service no access to the kernel"
            ),
            "{result:?}"
        );
    }
    assert_eq!(rig.fake().writes().len(), before);
    assert!(instance(&rig, "other").is_none());

    // No template Arca ships declares it.
    let shipped = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../templates");
    for (file, _) in arca::cmd_templates::SHIPPED {
        if let Some(name) = file.strip_suffix(".json") {
            let template = arca::service::load_template(&shipped, name).expect("it loads");
            assert!(template.kernel_access.is_none(), "{name}");
        }
    }
}

#[test]
fn nothing_in_a_tenant_gets_kernel_access_and_no_verb_installs_a_template() {
    let rig = node(loopback());
    let before = rig.fake().writes().len();
    let entries = rig.entries().len();
    // A tenant names a template by its name, like any caller. One that declares kernelAccess is refused there,
    // whether or not the call states the access, as a create and as a proposal the owner would be shown.
    for args in [
        json!({ "name": "cloud", "template": "cloud", "tenant": A }),
        json!({ "name": "cloud", "template": "cloud", "tenant": A, "kernelAccess": stated() }),
    ] {
        for result in [
            rig.call("service_create", args.clone()),
            propose(&rig, args.clone()),
        ] {
            let (code, message) = refusal(result);
            assert_eq!(code, "refused");
            assert_eq!(
                message,
                "template cloud gives its service access to this node's kernel, and nothing in a tenant has that: only a service of the node's own project is created from it"
            );
        }
    }
    assert_eq!(rig.fake().writes().len(), before);
    assert_eq!(rig.entries().len(), entries);
    assert!(
        rig.get(&format!("/1.0/instances/cloud-{A}?project={}", project(A)))
            .is_none()
    );
    assert_eq!(
        rig.call("plan_list", json!({})).expect("plans")["plans"],
        json!([]),
        "no proposal was recorded"
    );
    // Beneath Arca's own refusal, Incus refuses every proxy device in a tenant's project.
    let held = rig
        .get(&format!("/1.0/projects/{0}?project={0}", project(A)))
        .expect("the tenant's project");
    assert_eq!(held["config"]["restricted.devices.proxy"], "block");
    // An ordinary template is still the tenant's to use.
    rig.call(
        "service_create",
        json!({ "name": "web", "template": "app", "tenant": A }),
    )
    .expect("an ordinary service in the tenant");

    // No verb installs or writes a template: none is named for one, and wherever a verb takes `template` it is a
    // name of at most 40 characters, never a template's content. Templates reach the node through
    // `arca templates install` and the owner's own files alone.
    for verb in arca_api::verbs() {
        assert!(!verb.name.contains("template"), "{}", verb.name);
        let params = (verb.params)();
        if let Some(template) = params["properties"].get("template") {
            assert_eq!(template["type"], "string", "{}", verb.name);
            assert_eq!(
                template["pattern"], "^[a-z](?:[a-z0-9-]{0,38}[a-z0-9])?$",
                "{}",
                verb.name
            );
        }
    }
}

#[test]
fn a_node_gives_kernel_access_only_through_a_loopback_proxy_listener_served_to_loopback() {
    const SET: &str = "set \"listen\": { \"proxy\": { \"addr\": \"127.0.0.1:<a free port>\", \"peer\": \"127.0.0.1\" } } in the node's config and restart serve";
    let not_set = format!(
        "template cloud gives its service access to this node's kernel, and this node has no listener for it (listen.proxy is not set): {SET}"
    );
    let other = format!(
        "template cloud gives its service access to this node's kernel, and this node's listen.proxy is not 127.0.0.1 served to the peer 127.0.0.1: it serves one address to one peer, so while it serves a reverse proxy at another address it gives no service kernel access. To give it, {SET}"
    );
    let page = "template cloud gives its service access to this node's kernel, and this node serves its approval page on the same listener (approvalOrigin is set). The page's routes need no token, so the service would reach them as the reverse proxy does. Kernel access is for a node whose approval page is served by another machine".to_string();
    let proxy = |addr: &str, peer: &str| json!({ "proxy": { "addr": addr, "peer": peer } });
    for (config, expected) in [
        // No proxy listener at all.
        (json!({}), &not_set),
        // A reverse proxy in a container on the host's bridge: the one address and the one peer are taken.
        (proxy("192.0.2.1:7460", "192.0.2.20"), &other),
        // Loopback, served to another loopback peer: Incus connects from 127.0.0.1.
        (proxy("127.0.0.1:7460", "127.0.0.2"), &other),
        (proxy("127.0.0.2:7460", "127.0.0.1"), &other),
        (proxy("[::1]:7460", "::1"), &other),
        // The right listener on a node that serves its approval page on it too.
        (
            json!({
                "proxy": { "addr": KERNEL, "peer": "127.0.0.1" },
                "approvalOrigin": "https://arca.example.com"
            }),
            &page,
        ),
    ] {
        let rig = node(config.clone());
        let before = rig.fake().writes().len();
        for result in [
            rig.call(
                "service_create",
                json!({ "name": "cloud", "template": "cloud", "kernelAccess": stated() }),
            ),
            propose(&rig, json!({ "name": "cloud", "template": "cloud" })),
        ] {
            let (code, message) = refusal(result);
            assert_eq!(code, "refused", "{config}");
            assert_eq!(&message, expected, "{config}");
        }
        assert_eq!(rig.fake().writes().len(), before, "{config}");
        assert!(instance(&rig, "cloud").is_none(), "{config}");
        assert!(
            !rig.entries()
                .iter()
                .any(|entry| entry.id.0 == "service-cloud"),
            "{config}"
        );
        // An ordinary template is created on every one of these nodes.
        rig.call(
            "service_create",
            json!({ "name": "web", "template": "app" }),
        )
        .unwrap_or_else(|error| panic!("{config}: {error:?}"));
    }
}

#[test]
fn a_create_states_the_access_and_the_owners_card_says_it_in_words() {
    let rig = node(loopback());
    // A direct create that does not state the access, or states another port, is refused before any write.
    let before = rig.fake().writes().len();
    for args in [
        json!({ "name": "cloud", "template": "cloud" }),
        json!({ "name": "cloud", "template": "cloud", "kernelAccess": { "port": 7462 } }),
    ] {
        let (code, message) = refusal(rig.call("service_create", args));
        assert_eq!(code, "refused");
        assert_eq!(
            message,
            "template cloud gives its service access to this node's kernel, and a create of it says so: add \"kernelAccess\": { \"port\": 7461 } to the call, exactly as the template has it. A proposal made with action_propose names it by itself, on the owner's card"
        );
    }
    assert_eq!(rig.fake().writes().len(), before);
    assert!(instance(&rig, "cloud").is_none());
    // A proposal that states another port is refused too: the owner is never shown a card for it.
    assert!(matches!(
        propose(
            &rig,
            json!({ "name": "cloud", "template": "cloud", "kernelAccess": { "port": 7462 } })
        ),
        Err(CallError::Invalid(message))
            if message == "kernelAccess: template cloud has { \"port\": 7461 }, and the call must state exactly that"
    ));

    // A proposal names the access by itself: it is in the arguments the owner's approval binds, and the card says
    // in words what it means.
    let proposal =
        propose(&rig, json!({ "name": "cloud", "template": "cloud" })).expect("proposed");
    assert_eq!(proposal["args"]["kernelAccess"], stated());
    assert_eq!(
        proposal["summary"],
        "Create the service cloud from template cloud with access to this node's kernel"
    );
    let words = "Yes: this service can call this node's kernel from inside, at http://127.0.0.1:7461/mcp, with whatever token it is given. The token's scope and tool list decide what it may do, and a call without a token is refused.";
    assert_eq!(row(&proposal, "Kernel access"), Some(words));
    assert_eq!(arca::action_proposal::kernel_access_words(7461), words);
    // An ordinary create's card has no such row.
    let plain = propose(&rig, json!({ "name": "web", "template": "app" })).expect("proposed");
    assert_eq!(row(&plain, "Kernel access"), None);
    assert!(plain["args"].get("kernelAccess").is_none());
    assert_eq!(plain["summary"], "Create the service web from template app");

    // The card says the same after the kernel restarts: the row is made from the proposal's own arguments.
    let rig = rig.restart();
    let listed = rig.call("plan_list", json!({})).expect("plans");
    let waiting = listed["plans"]
        .as_array()
        .expect("plans")
        .iter()
        .find(|plan| plan["id"] == proposal["id"])
        .expect("the proposal still waits")
        .clone();
    assert_eq!(row(&waiting, "Kernel access"), Some(words));
    assert_eq!(waiting["summary"], proposal["summary"]);

    // Approved, it is created with the device.
    let made = approve(&rig, &proposal).expect("approved and created");
    assert_eq!(
        made["kernelAccess"],
        json!({ "port": PORT, "connect": KERNEL })
    );
    assert_eq!(
        instance(&rig, "cloud").expect("the instance")["devices"]["kernel"],
        device()
    );
}

#[test]
fn a_template_that_gains_kernel_access_after_its_card_was_shown_is_not_created_by_that_approval() {
    let rig = node(loopback());
    let mut late = app_template("late");
    late["ports"] = json!([8080]);
    rig.template("late", late.clone());
    // The owner is shown a card with no kernel access row.
    let proposal = propose(&rig, json!({ "name": "late", "template": "late" })).expect("proposed");
    assert_eq!(row(&proposal, "Kernel access"), None);
    // The template's file changes on the host before the owner approves.
    late["kernelAccess"] = json!({ "port": PORT });
    rig.template("late", late);
    let before = rig.fake().writes().len();
    let (_, message) = refusal(approve(&rig, &proposal));
    assert!(
        message.contains(
            "template late gives its service access to this node's kernel, and a create of it says so"
        ),
        "{message}"
    );
    assert_eq!(rig.fake().writes().len(), before);
    assert!(instance(&rig, "late").is_none());
    assert!(
        !rig.entries()
            .iter()
            .any(|entry| entry.id.0 == "service-late")
    );
}

#[test]
fn the_access_survives_a_restart_and_goes_with_the_service() {
    let mut rig = node(loopback());
    create_cloud(&rig);
    let ready = json!({ "port": PORT, "connect": KERNEL, "ready": true });
    let status = |rig: &Rig| {
        rig.call("service_status", json!({ "name": "cloud" }))
            .expect("status")["kernelAccess"]
            .clone()
    };

    // The device is the instance's own: a restart of the instance leaves it as it was.
    let restarted = rig
        .approved(
            "instance_restart",
            json!({ "project": PROJECT, "instance": "cloud" }),
        )
        .expect("the restart");
    assert_eq!(restarted["status"], "Running");
    assert_eq!(
        instance(&rig, "cloud").expect("the instance")["devices"]["kernel"],
        device()
    );
    assert_eq!(status(&rig), ready);

    // So does a restart of the kernel, which reads the record again and writes nothing to the instance.
    let writes = rig.fake().writes().len();
    rig = rig.restart();
    assert_eq!(status(&rig), ready);
    assert_eq!(rig.fake().writes().len(), writes);

    // The device names the listener as it was at the create. A node whose listener has since moved says so, and so
    // does one that has since begun to serve its approval page on it.
    rig.node = json!({ "proxy": { "addr": "127.0.0.1:7470", "peer": "127.0.0.1" } });
    rig = rig.restart();
    assert_eq!(
        status(&rig),
        json!({
            "port": PORT, "connect": KERNEL, "ready": false,
            "problem": "this node's listen.proxy is no longer 127.0.0.1:7460 served to the peer 127.0.0.1, so the device connects to nothing of Arca's; remove the service and create it again"
        })
    );
    rig.node = json!({
        "proxy": { "addr": KERNEL, "peer": "127.0.0.1" },
        "approvalOrigin": "https://arca.example.com"
    });
    rig = rig.restart();
    assert_eq!(
        status(&rig),
        json!({
            "port": PORT, "connect": KERNEL, "ready": false,
            "problem": "this node now serves its approval page on the same listener (approvalOrigin is set), so the service also reaches the page's routes, which need no token"
        })
    );
    rig.node = loopback();
    rig = rig.restart();
    assert_eq!(status(&rig), ready);

    // Removing the service removes its instance, and the device with it: nothing of the access is left.
    let removed = rig
        .approved("service_remove", json!({ "name": "cloud" }))
        .expect("the removal");
    assert_eq!(removed["removed"], true);
    assert!(instance(&rig, "cloud").is_none());
    assert!(acl(&rig, "cloud").is_none());
    assert!(
        !rig.entries()
            .iter()
            .any(|entry| entry.id.0 == "service-cloud")
    );
    let left = rig
        .get("/1.0/instances?all-projects=true&recursion=2")
        .expect("every instance on the node");
    assert!(
        left.as_array()
            .expect("instances")
            .iter()
            .all(|row| proxies(row).is_empty()),
        "{left}"
    );
}

#[test]
fn a_template_names_a_kernel_access_port_of_its_own_and_only_a_container_template_may() {
    let rig = node(loopback());
    let dir = rig.dir.join("templates");
    let load = |name: &str, change: &dyn Fn(&mut Value)| {
        let mut template = cloud_template();
        template["name"] = json!(name);
        change(&mut template);
        rig.template(name, template);
        arca::service::load_template(&dir, name).map_err(|error| error.message)
    };
    assert_eq!(
        load("ok", &|_| {}).expect("it loads").kernel_access,
        Some(arca::service::KernelAccess { port: PORT })
    );
    let port = "kernelAccess port is 1024..65535 and not one of the template's own ports";
    for (name, change) in [
        ("low", json!({ "kernelAccess": { "port": 1023 } })),
        ("zero", json!({ "kernelAccess": { "port": 0 } })),
        ("own", json!({ "kernelAccess": { "port": 8080 } })),
        (
            "dash",
            json!({ "kind": "system", "image": "local:cloud", "publishable": true, "dashPort": 9000,
                "kernelAccess": { "port": 9000 } }),
        ),
    ] {
        let refused = load(name, &|template| {
            for (key, value) in change.as_object().expect("changes") {
                template[key] = value.clone();
            }
        });
        assert_eq!(
            refused.err(),
            Some(format!("template {name}: {port}")),
            "{name}"
        );
    }
    // Any other shape of the key does not read.
    for (name, value) in [
        ("extra", json!({ "port": PORT, "peer": "192.0.2.9" })),
        ("bare", json!(true)),
        ("big", json!({ "port": 65536 })),
        ("none", json!({})),
    ] {
        let refused = load(name, &|template| template["kernelAccess"] = value.clone());
        assert_eq!(
            refused.err(),
            Some(format!("template {name}: cannot be read")),
            "{name}"
        );
    }
    // A virtual machine has no device that listens inside it.
    let mut vm = vm_template();
    vm["name"] = json!("vmk");
    vm["kernelAccess"] = stated();
    rig.template("vmk", vm);
    let refused = arca::service::load_template(&dir, "vmk")
        .map_err(|error| error.message)
        .expect_err("refused");
    assert!(
        refused.starts_with(
            "template vmk: a virtual machine has no env, generate, entrypoint, publishable, dashPort, droiddPort, data, lan or kernelAccess"
        ),
        "{refused}"
    );
}
