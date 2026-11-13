//! The two halves of a cold NATS replica, made from the shipped templates against the fake Incus: a `nats-primary` whose
//! leaf node port listens on the node's address on a private link (`kernel.serviceListenAddresses`), and a
//! `nats-replica` on a second node whose config lets it reach that address and port (`kernel.serviceDestinations`).
//! Each node is a rig of its own: nothing here joins two nodes, runs a server or carries a message. Every address is
//! synthetic.
use super::*;
use std::net::IpAddr;

/// The primary's leaf node port.
const PORT: u16 = 7422;
/// The primary node's address on the private link, and the replica node's own address on it.
const LINK: &str = "10.20.30.7";
const REPLICA_LINK: &str = "10.20.30.8";
/// The leaf password the owner chose for the pair, set on both halves.
const PASSWORD: &str = "Synthetic0PairPassword0123456789";

/// A node with the shipped templates installed, whose config adds `extra` to the kernel key and whose host holds `host`
/// on its private link.
fn node(extra: Value, host: &str) -> Rig {
    let mut kernel = json!({
        "projects": [PROJECT],
        "droiddProjects": ["other"],
        "serviceCidr": CIDR,
        "postgresImage": PG_IMAGE,
        "vmLimits": { "virtualMachines": 4, "cpu": 16, "memory": "64GiB", "disk": "1TiB" }
    });
    for (key, value) in extra.as_object().expect("an object") {
        kernel[key] = value.clone();
    }
    let rig = Rig::with_kernel(kernel, empty_state());
    arca::cmd_templates::install_into(&rig.dir.join("templates"), false)
        .expect("the shipped templates");
    rig.fake().set_host_interfaces(vec![(
        "link0".into(),
        vec![host.parse::<IpAddr>().expect("an address")],
    )]);
    rig
}

/// The primary's node: its config names its address on the link as one a private port may listen on.
fn primary_node() -> Rig {
    node(json!({ "serviceListenAddresses": [LINK] }), LINK)
}

/// The replica's node: its config lets the service `copy` reach the primary's leaf node port.
fn replica_node() -> Rig {
    node(
        json!({ "serviceDestinations": [{ "service": "copy", "address": LINK, "port": PORT }] }),
        REPLICA_LINK,
    )
}

fn instance(rig: &Rig, name: &str) -> Value {
    rig.get(&format!("/1.0/instances/{name}?project={PROJECT}"))
        .expect("the instance")
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

fn secret(rig: &Rig, name: &str, key: &str, value: &str) {
    rig.call(
        "service_secret_set",
        json!({ "name": name, "key": key, "value": value }),
    )
    .unwrap_or_else(|error| panic!("{key}: {error:?}"));
}

fn start(rig: &Rig, name: &str) -> Value {
    rig.call(
        "instance_start",
        json!({ "project": PROJECT, "instance": name }),
    )
    .expect("the first start")
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

#[test]
fn a_nats_primary_is_made_from_the_owner_s_card_alone_and_its_leaf_port_listens_on_the_link_address_only()
 {
    let rig = primary_node();
    let before = rig.fake().writes().len();
    let (code, message) = refusal(rig.call(
        "service_create",
        json!({ "name": "bus", "template": "nats-primary" }),
    ));
    assert_eq!(code, "NotApproved", "{message}");
    assert_eq!(rig.fake().writes().len(), before);

    // Proposed with start false, so the password the owner chose for the pair is in place before its first start.
    let proposal = propose(
        &rig,
        json!({ "name": "bus", "template": "nats-primary", "start": false }),
    )
    .expect("the proposal");
    assert_eq!(
        proposal["args"]["privatePort"],
        json!({ "address": LINK, "port": PORT })
    );
    let words = row(&proposal, "Private port").expect("the card says the address and the port");
    assert!(
        words.contains(&format!("tcp port {PORT}")) && words.contains(&format!("{LINK} only")),
        "{words}"
    );
    let made = approve(&rig, &proposal).expect("the approved create");
    assert_eq!(made["state"], "stopped", "{made}");
    assert_eq!(
        made["privatePort"],
        json!({ "address": LINK, "port": PORT })
    );

    // One proxy device, on the link address and the leaf node port, into the container's own loopback.
    let held = instance(&rig, "bus");
    assert_eq!(
        held["devices"]["private"],
        json!({
            "type": "proxy",
            "listen": format!("tcp:{LINK}:{PORT}"),
            "connect": format!("tcp:127.0.0.1:{PORT}")
        })
    );
    assert_eq!(proxies(&held), ["private"]);
    assert_eq!(held["config"][arca::service::PORTS_KEY], "4222,7422,8222");

    // The owner sets the pair's password, which replaces the one made at create, and starts it.
    secret(&rig, "bus", "NATS_LEAF_PASSWORD", PASSWORD);
    assert_eq!(start(&rig, "bus")["status"], "Running");
    let live = instance(&rig, "bus");
    assert_eq!(live["config"]["environment.NATS_LEAF_PASSWORD"], PASSWORD);
    assert_eq!(live["config"]["environment.NATS_DOMAIN"], "bus");
    let status = rig
        .call("service_status", json!({ "name": "bus" }))
        .expect("status");
    assert_eq!(
        status["privatePort"],
        json!({ "address": LINK, "port": PORT, "ready": true })
    );

    // A nats beside it on the same node runs the same script line, with no private port.
    rig.call(
        "service_create",
        json!({ "name": "plain", "template": "nats" }),
    )
    .expect("an ordinary nats");
    let plain = instance(&rig, "plain");
    assert_eq!(
        plain["config"]["oci.entrypoint"],
        live["config"]["oci.entrypoint"]
    );
    assert!(proxies(&plain).is_empty());
}

#[test]
fn a_node_whose_config_names_no_listen_address_makes_nats_as_it_did_and_refuses_nats_primary_in_words()
 {
    let rig = node(json!({}), LINK);
    let made = rig
        .call(
            "service_create",
            json!({ "name": "bus", "template": "nats" }),
        )
        .expect("nats, created directly as before");
    assert_eq!(made["state"], "running", "{made}");
    assert!(made.get("privatePort").is_none());
    assert!(proxies(&instance(&rig, "bus")).is_empty());

    let before = rig.fake().writes().len();
    for result in [
        rig.call(
            "service_create",
            json!({ "name": "hub", "template": "nats-primary" }),
        ),
        propose(&rig, json!({ "name": "hub", "template": "nats-primary" })),
    ] {
        let (code, message) = refusal(result);
        assert_eq!(code, "refused");
        assert!(
            message.contains("kernel.serviceListenAddresses"),
            "{message}"
        );
    }
    assert_eq!(rig.fake().writes().len(), before, "nothing was written");
}

#[test]
fn a_nats_replica_is_made_stopped_from_a_card_that_names_the_primary_s_port_and_starts_once_both_secrets_are_set()
 {
    let rig = replica_node();
    let before = rig.fake().writes().len();
    let (code, message) = refusal(rig.call(
        "service_create",
        json!({ "name": "copy", "template": "nats-replica" }),
    ));
    assert_eq!(code, "NotApproved", "{message}");
    assert_eq!(rig.fake().writes().len(), before);

    // The card says the one way out it opens, and that the service is created stopped and why.
    let proposal =
        propose(&rig, json!({ "name": "copy", "template": "nats-replica" })).expect("the proposal");
    assert_eq!(
        proposal["args"]["destinations"],
        json!([{ "address": LINK, "port": PORT }])
    );
    let way = row(&proposal, "Private network").expect("the card names the way");
    assert!(way.contains(&format!("{LINK} on tcp port {PORT}")), "{way}");
    let stopped = row(&proposal, "Start").expect("the card says it starts stopped");
    assert!(
        stopped.contains("NATS_LEAF_PASSWORD") && stopped.contains("NATS_LEAF_REMOTE"),
        "{stopped}"
    );
    let made = approve(&rig, &proposal).expect("the approved create");
    assert_eq!(made["state"], "stopped", "{made}");
    let reason = made["stoppedBecause"].as_str().expect("why it is stopped");
    assert!(
        reason.contains("NATS_LEAF_PASSWORD") && reason.contains("NATS_LEAF_REMOTE"),
        "{reason}"
    );
    assert_eq!(
        rig.entry("service-copy")["destinations"],
        json!([{ "address": LINK, "port": PORT }])
    );

    // Its rules open tcp to the primary's leaf node port, and every other port and protocol of that address stays shut.
    let egress = rig
        .get(&format!(
            "/1.0/network-acls/{PROJECT}-svc-copy?project=default"
        ))
        .expect("its ACL")["egress"]
        .as_array()
        .expect("egress")
        .clone();
    let to_link: Vec<(String, String, String)> = egress
        .iter()
        .filter(|rule| rule["destination"] == format!("{LINK}/32"))
        .map(|rule| {
            let text = |key: &str| rule[key].as_str().unwrap_or_default().to_owned();
            (text("action"), text("protocol"), text("destination_port"))
        })
        .collect();
    assert_eq!(
        to_link,
        [
            ("reject", "tcp", "1-7421,7423-65535"),
            ("reject", "udp", ""),
            ("reject", "icmp4", ""),
            ("allow", "tcp", "7422"),
        ]
        .map(|(action, protocol, ports)| (action.into(), protocol.into(), ports.into()))
    );
    // Nothing reaches it from outside the node: no proxy, and its own ports are the client port and monitoring.
    let held = instance(&rig, "copy");
    assert!(proxies(&held).is_empty());
    assert_eq!(held["config"][arca::service::PORTS_KEY], "4222,8222");

    // The owner sets the primary's port and password, and starts it.
    secret(&rig, "copy", "NATS_LEAF_REMOTE", &format!("{LINK}:{PORT}"));
    secret(&rig, "copy", "NATS_LEAF_PASSWORD", PASSWORD);
    assert_eq!(start(&rig, "copy")["status"], "Running");
    let live = instance(&rig, "copy");
    assert_eq!(
        live["config"]["environment.NATS_LEAF_REMOTE"],
        format!("{LINK}:{PORT}")
    );
    assert_eq!(live["config"]["environment.NATS_LEAF_PASSWORD"], PASSWORD);
    assert_eq!(live["config"]["environment.NATS_DOMAIN"], "copy");
    assert!(
        live["config"]["oci.entrypoint"]
            .as_str()
            .expect("the entrypoint")
            .ends_with("' nats-replica")
    );
    assert_eq!(
        rig.call("service_status", json!({ "name": "copy" }))
            .expect("status")["destinations"],
        json!([{ "address": LINK, "port": PORT, "ready": true }])
    );

    // At promotion a consumer on this node is linked to it, and gets the URL a link to nats gives.
    rig.template("app", app_template("app"));
    rig.call(
        "service_create",
        json!({ "name": "api", "template": "app" }),
    )
    .expect("a consumer");
    assert_eq!(
        rig.call("service_connect", json!({ "from": "api", "to": "copy" }))
            .expect("the link"),
        json!({ "env": ["NATS_URL"] })
    );
    let address = live["config"][arca::service::ADDRESS_KEY]
        .as_str()
        .expect("its address");
    assert_eq!(
        instance(&rig, "api")["config"]["environment.NATS_URL"],
        format!("nats://{address}:4222")
    );
}
