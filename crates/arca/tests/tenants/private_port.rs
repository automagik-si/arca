//! A template that declares `privatePort`: its service, in the node's own project, offers that one port on ONE host
//! address of the node's config (`kernel.serviceListenAddresses`), through one proxy device that listens on that address
//! and never on every interface. The create runs only from a proposal the owner approved, whose card says the address and
//! the port; a tenant never gets it; the service's network rules are the ones of an ordinary service. Every address is
//! from RFC 5737.
use super::*;
use arca::handlers::{UpdateOptions, service_handlers_updating};
use std::net::IpAddr;
use std::time::Duration;

/// The port the template names, a NATS leafnode port.
const PORT: u16 = 7422;
/// The address of the private link on this node, and another address of the node that the config does not name.
const LINK: &str = "198.51.100.7";
const OTHER: &str = "203.0.113.9";

fn leaf_template() -> Value {
    json!({
        "name": "leaf", "kind": "oci", "image": "docker.io/library/nats", "digest": DIGEST,
        "ports": [PORT], "env": {}, "privatePort": { "port": PORT }
    })
}

/// What a create of the `leaf` template states once it is proposed.
fn stated() -> Value {
    json!({ "address": LINK, "port": PORT })
}

/// A node with tenants, the tenant `A`, the template `leaf` and the ordinary template `app`, whose config names
/// `addresses` and whose host has the interfaces `LINK` (the private link) and `OTHER`.
fn node(addresses: Value) -> Rig {
    let rig = Rig::with_kernel(
        json!({
            "projects": [PROJECT],
            "droiddProjects": ["other"],
            "serviceCidr": CIDR,
            "postgresImage": PG_IMAGE,
            "vmLimits": { "virtualMachines": 4, "cpu": 16, "memory": "64GiB", "disk": "1TiB" },
            "tenants": policy(),
            "serviceListenAddresses": addresses
        }),
        empty_state(),
    );
    rig.template("leaf", leaf_template());
    rig.template("app", app_template("app"));
    rig.fake().set_host_interfaces(vec![
        ("eth1".into(), vec![LINK.parse::<IpAddr>().unwrap()]),
        ("eth2".into(), vec![OTHER.parse::<IpAddr>().unwrap()]),
    ]);
    rig.create_tenant(json!({ "id": A })).expect("tenant");
    rig
}

fn instance(rig: &Rig, name: &str) -> Option<Value> {
    rig.get(&format!("/1.0/instances/{name}?project={PROJECT}"))
}

fn acl(rig: &Rig, name: &str) -> Option<Value> {
    rig.get(&format!(
        "/1.0/network-acls/{PROJECT}-svc-{name}?project=default"
    ))
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

/// An edit of the fake Incus, as an admin does by hand.
fn edit(rig: &Rig, method: &str, path: &str, body: Value) {
    use std::io::{Read, Write};
    let mut stream = std::os::unix::net::UnixStream::connect(rig.fake().socket()).expect("socket");
    let body = body.to_string();
    write!(stream, "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).expect("write");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read");
    assert!(
        response.starts_with("HTTP/1.1 200") || response.starts_with("HTTP/1.1 202"),
        "fixture edit failed: {}",
        response.lines().next().unwrap_or_default()
    );
}

fn row<'a>(proposal: &'a Value, label: &str) -> Option<&'a str> {
    proposal["details"]
        .as_array()
        .expect("details")
        .iter()
        .find(|row| row["label"] == label)
        .map(|row| row["value"].as_str().expect("words"))
}

fn propose(rig: &Rig, args: Value) -> Result<Value, CallError> {
    rig.call(
        "action_propose",
        json!({ "action": "service_create", "args": args }),
    )
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

fn create_leaf(rig: &Rig) -> Value {
    rig.approved(
        "service_create",
        json!({ "name": "leaf", "template": "leaf" }),
    )
    .expect("the create")
}

#[test]
fn a_private_port_is_one_proxy_device_on_one_address_and_the_service_rules_are_an_ordinary_service_s()
 {
    let rig = node(json!([LINK]));
    let made = create_leaf(&rig);
    assert_eq!(made["state"], "running");
    assert_eq!(made["privatePort"], stated());

    // The device listens on the one address, and connects to the port on the instance's loopback. It is the instance's
    // only proxy device, and the wildcard is nowhere in it.
    let held = instance(&rig, "leaf").expect("the instance");
    assert_eq!(
        held["devices"]["private"],
        json!({
            "type": "proxy",
            "listen": format!("tcp:{LINK}:{PORT}"),
            "connect": format!("tcp:127.0.0.1:{PORT}")
        })
    );
    assert_eq!(proxies(&held), ["private"]);
    assert!(!held["devices"]["private"].to_string().contains("0.0.0.0"));
    // It was in the create itself: no later write added it.
    let creates: Vec<Value> = rig
        .fake()
        .service_requests()
        .iter()
        .filter(|request| request.method == "POST" && request.path.starts_with("/1.0/instances?"))
        .filter_map(|request| serde_json::from_slice::<Value>(&request.body).ok())
        .filter(|body| body["name"] == "leaf")
        .collect();
    assert_eq!(creates.len(), 1, "{creates:?}");
    assert_eq!(creates[0]["devices"]["private"], held["devices"]["private"]);

    // The record names the address and the port, in the node's own project.
    let record = rig.entry("service-leaf");
    assert_eq!(record["privatePort"], stated());
    assert_eq!(record["project"], PROJECT);
    assert!(record.get("tenant").is_none());
    assert!(record.get("lan").is_none_or(Value::is_null));

    // The service's network rules are those of an ordinary service, rule for rule: the proxy connects from the host
    // into the instance's own loopback, so it needs no rule and the rules open nothing for it.
    rig.call(
        "service_create",
        json!({ "name": "web", "template": "app" }),
    )
    .expect("an ordinary service");
    let (leaf, web) = (
        acl(&rig, "leaf").expect("its ACL"),
        acl(&rig, "web").expect("the other ACL"),
    );
    assert_eq!(leaf["ingress"], json!([]));
    assert_eq!(leaf["ingress"], web["ingress"]);
    assert_eq!(leaf["egress"], web["egress"]);

    // The status says it stands, the list names it, and an ordinary service has no such key.
    let status = rig
        .call("service_status", json!({ "name": "leaf" }))
        .expect("status");
    assert_eq!(
        status["privatePort"],
        json!({ "address": LINK, "port": PORT, "ready": true })
    );
    assert_eq!(status["lanPort"], Value::Null);
    let listed = rig.call("service_list", json!({})).expect("list");
    let find = |name: &str| {
        listed["services"]
            .as_array()
            .expect("services")
            .iter()
            .find(|row| row["name"] == name)
            .expect("its row")
            .clone()
    };
    assert_eq!(find("leaf")["privatePort"], stated());
    assert!(find("web").get("privatePort").is_none());
    assert!(
        rig.call("service_status", json!({ "name": "web" }))
            .expect("status")
            .get("privatePort")
            .is_none()
    );
    assert!(proxies(&instance(&rig, "web").expect("web")).is_empty());
}

#[test]
fn the_card_says_the_address_and_the_port_and_the_approval_binds_them() {
    let rig = node(json!([LINK, OTHER]));
    let proposal = propose(
        &rig,
        json!({ "name": "leaf", "template": "leaf", "privatePort": { "address": LINK } }),
    )
    .expect("the proposal");
    // The proposal names the address and the port, whatever the call left out.
    assert_eq!(proposal["args"]["privatePort"], stated());
    let words = row(&proposal, "Private port").expect("the card has a row for it");
    assert!(
        words.contains(LINK) && words.contains(&format!("tcp port {PORT}")),
        "{words}"
    );
    assert!(words.contains(&format!("{LINK} only")), "{words}");
    assert!(
        proposal["summary"]
            .as_str()
            .expect("a summary")
            .contains("private addresses"),
        "{proposal}"
    );
    // A proposal of an ordinary template has no such row.
    let plain = propose(&rig, json!({ "name": "web", "template": "app" })).expect("plain");
    assert!(row(&plain, "Private port").is_none());

    // Approved, it makes the service on that address.
    let made = approve(&rig, &proposal).expect("the approved create");
    assert_eq!(made["privatePort"], stated());
    assert_eq!(
        instance(&rig, "leaf").expect("instance")["devices"]["private"]["listen"],
        format!("tcp:{LINK}:{PORT}")
    );
}

#[test]
fn a_node_that_names_one_address_needs_none_in_the_call_and_one_that_names_more_needs_one() {
    let rig = node(json!([LINK]));
    let proposal = propose(&rig, json!({ "name": "leaf", "template": "leaf" })).expect("proposal");
    assert_eq!(proposal["args"]["privatePort"], stated());

    let rig = node(json!([LINK, OTHER]));
    let (code, message) = refusal(propose(&rig, json!({ "name": "leaf", "template": "leaf" })));
    assert_eq!(code, "refused");
    assert!(
        message.contains("more than one host address")
            && message.contains(LINK)
            && message.contains(OTHER),
        "{message}"
    );
    // The other address then serves.
    let other = propose(
        &rig,
        json!({ "name": "leaf", "template": "leaf", "privatePort": { "address": OTHER } }),
    )
    .expect("the other address");
    assert_eq!(
        other["args"]["privatePort"],
        json!({ "address": OTHER, "port": PORT })
    );
}

#[test]
fn an_address_the_config_does_not_name_or_the_host_does_not_hold_is_refused_before_anything_is_written()
 {
    // The config names the link only: a call cannot name the host's other address.
    let rig = node(json!([LINK]));
    let before = rig.fake().writes().len();
    let entries = rig.entries().len();
    for args in [
        json!({ "name": "leaf", "template": "leaf", "privatePort": { "address": OTHER } }),
        json!({ "name": "leaf", "template": "leaf", "privatePort": { "address": "0.0.0.0" } }),
        json!({ "name": "leaf", "template": "leaf", "privatePort": { "address": "198.51.100.0/24" } }),
    ] {
        for result in [
            rig.call("service_create", args.clone()),
            propose(&rig, args.clone()),
        ] {
            let (code, message) = refusal(result);
            assert_eq!(code, "refused", "{args}");
            assert!(
                message.contains("not one this node's config names"),
                "{message}"
            );
        }
    }
    for args in [
        json!({ "name": "leaf", "template": "leaf", "privatePort": { "address": 7 } }),
        json!({ "name": "leaf", "template": "leaf", "privatePort": "198.51.100.7" }),
        json!({ "name": "leaf", "template": "leaf", "privatePort": { "address": LINK, "port": 7423 } }),
        json!({ "name": "leaf", "template": "leaf", "privatePort": { "address": LINK, "other": 1 } }),
    ] {
        for result in [
            rig.call("service_create", args.clone()),
            propose(&rig, args.clone()),
        ] {
            assert!(
                matches!(result, Err(CallError::Invalid(_))),
                "{args}: {result:?}"
            );
        }
    }
    assert_eq!(rig.fake().writes().len(), before);
    assert_eq!(rig.entries().len(), entries);

    // A config that names an address no interface of the host holds: the create is refused and says so.
    let rig = node(json!(["198.51.100.99"]));
    let before = rig.fake().writes().len();
    for result in [
        rig.call(
            "service_create",
            json!({ "name": "leaf", "template": "leaf" }),
        ),
        propose(&rig, json!({ "name": "leaf", "template": "leaf" })),
    ] {
        let (code, message) = refusal(result);
        assert_eq!(code, "refused");
        assert!(
            message.contains("198.51.100.99")
                && message.contains("not an address of any interface"),
            "{message}"
        );
    }
    assert_eq!(rig.fake().writes().len(), before);
    assert!(instance(&rig, "leaf").is_none());
}

#[test]
fn a_node_that_names_no_address_gives_no_private_port() {
    let rig = node(json!([]));
    let before = rig.fake().writes().len();
    for result in [
        rig.call(
            "service_create",
            json!({ "name": "leaf", "template": "leaf" }),
        ),
        propose(&rig, json!({ "name": "leaf", "template": "leaf" })),
    ] {
        let (code, message) = refusal(result);
        assert_eq!(code, "refused");
        assert!(
            message.contains("kernel.serviceListenAddresses"),
            "{message}"
        );
    }
    assert_eq!(rig.fake().writes().len(), before);
}

#[test]
fn a_create_runs_only_from_an_approved_proposal_that_states_exactly_the_address_and_the_port() {
    let rig = node(json!([LINK]));
    let before = rig.fake().writes().len();
    // Called directly, with or without the statement: not approved.
    for args in [
        json!({ "name": "leaf", "template": "leaf" }),
        json!({ "name": "leaf", "template": "leaf", "privatePort": stated() }),
        json!({ "name": "leaf", "template": "leaf", "privatePort": stated(), "approval": "a-guess" }),
    ] {
        let (code, message) = refusal(rig.call("service_create", args.clone()));
        assert_eq!(code, "NotApproved", "{args}: {message}");
        assert!(
            message.contains(LINK) && message.contains("approved"),
            "{message}"
        );
    }
    assert_eq!(rig.fake().writes().len(), before);
    assert!(instance(&rig, "leaf").is_none());

    // A proposal made when the config named another address is no longer the one this node would make: the
    // statement the owner approved is compared with the config at the create.
    let rig = node(json!([LINK, OTHER]));
    let proposal = propose(
        &rig,
        json!({ "name": "leaf", "template": "leaf", "privatePort": { "address": OTHER } }),
    )
    .expect("the proposal");
    assert_eq!(proposal["args"]["privatePort"]["address"], OTHER);
    let made = approve(&rig, &proposal).expect("the create");
    assert_eq!(made["privatePort"]["address"], OTHER);
}

#[test]
fn an_ordinary_template_cannot_ask_for_a_private_port() {
    let rig = node(json!([LINK]));
    let before = rig.fake().writes().len();
    for result in [
        rig.call(
            "service_create",
            json!({ "name": "web", "template": "app", "privatePort": stated() }),
        ),
        propose(
            &rig,
            json!({ "name": "web", "template": "app", "privatePort": stated() }),
        ),
    ] {
        assert!(
            matches!(&result, Err(CallError::Invalid(message))
                if message == "privatePort: template app publishes no private port"),
            "{result:?}"
        );
    }
    assert_eq!(rig.fake().writes().len(), before);
    // The ordinary service made beside it has no such device or key.
    rig.call(
        "service_create",
        json!({ "name": "web", "template": "app" }),
    )
    .expect("an ordinary service");
    assert!(proxies(&instance(&rig, "web").expect("web")).is_empty());
    assert!(rig.entry("service-web").get("privatePort").is_none());

    // No template Arca ships declares it.
    let shipped = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../templates");
    for (file, _) in arca::cmd_templates::SHIPPED {
        if let Some(name) = file.strip_suffix(".json") {
            let template = arca::service::load_template(&shipped, name).expect("it loads");
            assert!(template.private_port.is_none(), "{name}");
        }
    }
}

#[test]
fn nothing_in_a_tenant_gets_a_private_port() {
    let rig = node(json!([LINK]));
    let before = rig.fake().writes().len();
    let entries = rig.entries().len();
    for args in [
        json!({ "name": "leaf", "template": "leaf", "tenant": A }),
        json!({ "name": "leaf", "template": "leaf", "tenant": A, "privatePort": stated() }),
    ] {
        for result in [
            rig.call("service_create", args.clone()),
            propose(&rig, args.clone()),
        ] {
            let (code, message) = refusal(result);
            assert_eq!(code, "refused");
            assert_eq!(
                message,
                "template leaf publishes a port on one of this node's host addresses, and nothing in a tenant has a host port: only a service of the node's own project is created from it"
            );
        }
    }
    assert_eq!(rig.fake().writes().len(), before);
    assert_eq!(rig.entries().len(), entries);
}

#[test]
fn a_service_update_keeps_the_device_and_checks_the_service_through_the_private_address() {
    let rig = node(json!([LINK]));
    create_leaf(&rig);
    let before = instance(&rig, "leaf").expect("instance");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let handlers: HashMap<&'static str, Arc<dyn ToolHandler>> = service_handlers_updating(
        rig.kernel.clone(),
        Arc::new(|| NOW),
        rig.cfg.clone(),
        None,
        UpdateOptions {
            probe: Arc::new(move |address, port, forwarded| {
                log.lock()
                    .unwrap()
                    .push((address.to_string(), port, forwarded));
                true
            }),
            pace: Duration::from_millis(10),
        },
    )
    .into_iter()
    .collect();
    let new = "sha256:2222222222222222222222222222222222222222222222222222222222222222";
    let done = handlers["service_update"]
        .call(
            json!({ "name": "leaf", "digest": new }),
            &Caller {
                token_id: "doctor".into(),
                scope: Scope::Doctor,
                tools: None,
            },
        )
        .expect("the update");
    assert_eq!(done["updated"], true);
    // The proxy listens on the private address alone, so the node tries that address, not its own loopback.
    assert_eq!(
        *seen.lock().unwrap(),
        vec![(LINK.to_string(), PORT, true)],
        "the one address the proxy listens on"
    );
    let after = instance(&rig, "leaf").expect("instance");
    assert_eq!(after["devices"]["private"], before["devices"]["private"]);
    assert_eq!(proxies(&after), ["private"]);
    assert_eq!(rig.entry("service-leaf")["privatePort"], stated());
}

#[test]
fn removing_the_service_removes_the_instance_with_its_device_and_the_record() {
    let rig = node(json!([LINK]));
    create_leaf(&rig);
    assert!(instance(&rig, "leaf").is_some());
    let proposal = rig
        .call(
            "action_propose",
            json!({ "action": "service_remove", "args": { "name": "leaf" } }),
        )
        .expect("the proposal");
    approve(&rig, &proposal).expect("the removal");
    assert!(instance(&rig, "leaf").is_none());
    assert!(acl(&rig, "leaf").is_none());
    assert!(
        !rig.entries()
            .iter()
            .any(|entry| entry.id.0 == "service-leaf")
    );
}

#[test]
fn a_status_names_what_does_not_stand() {
    let rig = node(json!([LINK]));
    create_leaf(&rig);
    // The device taken off the instance by hand.
    edit(
        &rig,
        "PATCH",
        &format!("/1.0/instances/leaf?project={PROJECT}"),
        json!({ "devices": { "private": { "type": "proxy", "listen": format!("tcp:0.0.0.0:{PORT}"), "connect": format!("tcp:127.0.0.1:{PORT}") } } }),
    );
    let status = rig
        .call("service_status", json!({ "name": "leaf" }))
        .expect("status");
    assert_eq!(status["privatePort"]["ready"], false);
    assert_eq!(
        status["privatePort"]["problem"],
        "its instance does not carry the device private as Arca made it"
    );
}

#[test]
fn serve_does_not_start_over_an_address_the_config_dropped_and_never_over_one_the_host_lacks() {
    let rig = node(json!([LINK]));
    create_leaf(&rig);
    let entries = rig.entries();
    // The config still names it, held or not: the start is never refused for the host's interfaces, so one link
    // without carrier does not stop the node and every other service on it.
    assert_eq!(
        arca::handlers::private_port_refusal(&rig.cfg, &entries),
        None
    );
    // A config that no longer names it is the owner's own change, and is refused.
    let other = node(json!([OTHER]));
    let dropped = arca::handlers::private_port_refusal(&other.cfg, &entries)
        .expect("the config dropped the address");
    assert!(
        dropped.contains("leaf") && dropped.contains("does not name it"),
        "{dropped}"
    );
    // A node with no such service starts as before.
    assert_eq!(
        arca::handlers::private_port_refusal(&other.cfg, &other.entries()),
        None
    );
    // The host loses the address: the service says so, and the others of the node are untouched.
    rig.fake().set_host_interfaces(vec![]);
    let status = rig
        .call("service_status", json!({ "name": "leaf" }))
        .expect("status");
    assert_eq!(status["privatePort"]["ready"], false);
    assert!(
        status["privatePort"]["problem"]
            .as_str()
            .expect("a problem")
            .contains("not an address of any interface of this host now"),
        "{status}"
    );
}

#[test]
fn a_proposal_made_before_the_template_changed_is_refused_at_the_create() {
    let rig = node(json!([LINK]));
    let proposal = propose(&rig, json!({ "name": "leaf", "template": "leaf" })).expect("proposal");
    assert_eq!(proposal["args"]["privatePort"], stated());
    // The template now offers another port: what the owner approved does not say that.
    let mut changed = leaf_template();
    changed["ports"] = json!([7423]);
    changed["privatePort"] = json!({ "port": 7423 });
    rig.template("leaf", changed);
    let before = rig.fake().writes().len();
    // The approved arguments state the old port, and the call may state only the template's own: refused, whichever of
    // the two checks says it, before anything is written.
    let result = approve(&rig, &proposal);
    assert!(
        matches!(&result, Err(CallError::Invalid(message))
            if message == "privatePort: template leaf publishes port 7423, and the call may state only that"),
        "{result:?}"
    );
    assert_eq!(rig.fake().writes().len(), before);
    assert!(instance(&rig, "leaf").is_none());
}

#[test]
fn a_failed_health_check_names_the_private_port_it_probed_and_not_the_first_port() {
    // The template's ports are [7422] only, so give it a first port that is not the private one.
    let rig = node(json!([LINK]));
    let mut template = leaf_template();
    template["ports"] = json!([4222, PORT]);
    rig.template("leaf", template);
    create_leaf(&rig);
    let handlers: HashMap<&'static str, Arc<dyn ToolHandler>> = service_handlers_updating(
        rig.kernel.clone(),
        Arc::new(|| NOW),
        rig.cfg.clone(),
        None,
        UpdateOptions {
            probe: Arc::new(|_, _, _| false),
            pace: Duration::from_millis(10),
        },
    )
    .into_iter()
    .collect();
    let new = "sha256:2222222222222222222222222222222222222222222222222222222222222222";
    let failed = handlers["service_update"]
        .call(
            json!({ "name": "leaf", "digest": new, "timeoutSeconds": 1 }),
            &Caller {
                token_id: "doctor".into(),
                scope: Scope::Doctor,
                tools: None,
            },
        )
        .expect_err("the check fails");
    let message = format!("{failed:?}");
    assert!(
        message.contains(&format!("port {PORT} did not answer")) && !message.contains("port 4222"),
        "{message}"
    );
}
