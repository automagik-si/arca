//! A private destination: one address and one tcp port a service of the node's own project may reach through the
//! node. Only the node's config names one (`kernel.serviceDestinations`), by the service's name; the create of such a
//! service runs only from a proposal the owner approved, whose card shows the address; the service's NIC rules are
//! then built from its record. A tenant never gets one. Every address here is synthetic.
use super::*;
use arca_netproject::destination::Destination;
use std::net::{IpAddr, Ipv4Addr};

/// An address on another bridge of the node, where the node itself is 10.20.30.1.
const BRIDGE_ADDRESS: [u8; 4] = [10, 20, 30, 40];
/// An address on the node's tailnet, where the node itself is 100.99.1.2.
const TAILNET_ADDRESS: [u8; 4] = [100, 101, 102, 103];

fn entry(service: &str, address: [u8; 4], port: u16) -> Value {
    json!({ "service": service, "address": Ipv4Addr::from(address).to_string(), "port": port })
}

/// What the config names for the service `runners`: both kinds of address a first user needs, on tcp 443.
fn named() -> Value {
    json!([
        entry("runners", TAILNET_ADDRESS, 443),
        entry("runners", BRIDGE_ADDRESS, 443)
    ])
}

/// What a create of `runners` states, and what its record and views carry: lowest address first.
fn stated() -> Value {
    json!([
        { "address": "10.20.30.40", "port": 443 },
        { "address": "100.101.102.103", "port": 443 }
    ])
}

fn destinations() -> Vec<Destination> {
    serde_json::from_value(stated()).expect("destinations")
}

fn kernel(destinations: Value) -> Value {
    json!({
        "projects": [PROJECT],
        "droiddProjects": ["other"],
        "serviceCidr": CIDR,
        "postgresImage": PG_IMAGE,
        "vmLimits": { "virtualMachines": 4, "cpu": 16, "memory": "64GiB", "disk": "1TiB" },
        "tenants": policy(),
        "serviceDestinations": destinations
    })
}

/// The node's own addresses on the other bridge and on the tailnet.
fn interfaces() -> Vec<(String, Vec<IpAddr>)> {
    vec![
        ("br1".into(), vec![IpAddr::from([10, 20, 30, 1])]),
        ("ts0".into(), vec![IpAddr::from([100, 99, 1, 2])]),
    ]
}

/// The same addresses, as the plugin is told them.
fn hosts() -> Value {
    json!(["10.20.30.1", "100.99.1.2"])
}

/// A node whose config names `destinations`, with the tenant `A`, a container template `app` and a virtual-machine
/// template `vm`: a runner pool is a virtual machine on the managed service network.
fn node(destinations: Value) -> Rig {
    let rig = Rig::with_kernel(kernel(destinations), empty_state());
    rig.template("app", app_template("app"));
    rig.template("vm", vm_template());
    rig.fake().set_host_interfaces(interfaces());
    rig.create_tenant(json!({ "id": A })).expect("tenant");
    rig
}

fn acl(rig: &Rig, name: &str) -> Option<Value> {
    rig.get(&format!(
        "/1.0/network-acls/{PROJECT}-svc-{name}?project=default"
    ))
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

/// The rules the plugin gives a service of the node's own project at `gateway` with these destinations.
fn rules(gateway: &str, destinations: &[Destination]) -> Vec<Value> {
    arca_netproject::tenant::egress_rules(
        gateway.parse().expect("the gateway"),
        &hosts(),
        None,
        destinations,
    )
}

fn egress(rig: &Rig, name: &str) -> Vec<Value> {
    acl(rig, name).expect("its ACL")["egress"]
        .as_array()
        .expect("egress")
        .clone()
}

/// The rules of `held` that name `address` and nothing else, as (action, protocol, ports).
fn rules_for(held: &[Value], address: &str) -> Vec<(String, String, String)> {
    held.iter()
        .filter(|rule| rule["destination"] == format!("{address}/32"))
        .map(|rule| {
            (
                rule["action"].as_str().unwrap().to_owned(),
                rule["protocol"].as_str().unwrap().to_owned(),
                rule["destination_port"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

const WORDS: &str = "Yes: this service, and everything that runs in it, can reach 10.20.30.40 on tcp port 443 and 100.101.102.103 on tcp port 443 through this node, and is seen there as this node itself: whatever trusts this node's address there trusts everything in this service. Every other private address stays closed.";
const NOT_APPROVED: &str = "this node's config lets the service runners reach 10.20.30.40 on tcp port 443 and 100.101.102.103 on tcp port 443 through this node (kernel.serviceDestinations), so its create runs only from a proposal the owner approved: propose it with action_propose, and the owner's card shows the address";

#[test]
fn a_named_service_is_created_only_from_a_proposal_whose_card_shows_the_address() {
    let rig = node(named());
    // A direct create is refused before any write, whether or not it states the destinations: such a service is
    // created only from a proposal, so the owner always sees the card.
    let before = rig.fake().writes().len();
    for args in [
        json!({ "name": "runners", "template": "vm" }),
        json!({ "name": "runners", "template": "vm", "destinations": stated() }),
        json!({ "name": "runners", "template": "vm", "destinations": stated(), "approval": "a-guess" }),
        // The name is what the config binds, whatever the template.
        json!({ "name": "runners", "template": "app" }),
    ] {
        let (code, message) = refusal(rig.call("service_create", args));
        assert_eq!(code, "NotApproved");
        assert_eq!(message, NOT_APPROVED);
    }
    assert_eq!(rig.fake().writes().len(), before);
    assert!(acl(&rig, "runners").is_none());

    // No caller states a destination of its own: for a named service anything but the config's list is refused, and
    // for any other service anything at all is, as a create and as a proposal the owner would be shown.
    for other in [
        json!([{ "address": "10.20.30.41", "port": 443 }]),
        json!([{ "address": "10.20.30.40", "port": 22 }]),
        json!([{ "address": "10.20.30.40", "port": 443 }]),
        json!([]),
    ] {
        let result = propose(
            &rig,
            json!({ "name": "runners", "template": "vm", "destinations": other }),
        );
        assert!(
            matches!(
                &result,
                Err(CallError::Invalid(message))
                    if message == "destinations: this node's config names 10.20.30.40 on tcp port 443 and 100.101.102.103 on tcp port 443 for the service runners, and the call must state exactly that"
            ),
            "{other}: {result:?}"
        );
    }
    for given in [stated(), json!([])] {
        let args = json!({ "name": "web", "template": "app", "destinations": given });
        for result in [
            rig.call("service_create", args.clone()),
            propose(&rig, args.clone()),
        ] {
            assert!(
                matches!(
                    &result,
                    Err(CallError::Invalid(message))
                        if message == "destinations: this node's config (kernel.serviceDestinations) names none for the service web. Only the config names one, never a call"
                ),
                "{given}: {result:?}"
            );
        }
    }
    assert_eq!(rig.fake().writes().len(), before);
    assert_eq!(
        rig.call("plan_list", json!({})).expect("plans")["plans"],
        json!([]),
        "no proposal was recorded"
    );

    // A proposal names the destinations by itself, from the config: they are in the arguments the owner's approval
    // binds, and the card says in words what they mean.
    let proposal = propose(&rig, json!({ "name": "runners", "template": "vm" })).expect("proposed");
    assert_eq!(proposal["args"]["destinations"], stated());
    assert_eq!(
        proposal["summary"],
        "Create the service runners from template vm, with a way through this node to a private address"
    );
    assert_eq!(row(&proposal, "Private network"), Some(WORDS));
    // The digest the owner signs is over the arguments with the destinations: the same create without them, or with
    // another port, has another digest.
    let digest_of =
        |args: Value| propose(&node(json!([])), args).expect("proposed")["argsDigest"].clone();
    assert_ne!(
        proposal["argsDigest"],
        digest_of(json!({ "name": "runners", "template": "vm" }))
    );
    // An ordinary create's card has no such row.
    let plain = propose(&rig, json!({ "name": "web", "template": "app" })).expect("proposed");
    assert_eq!(row(&plain, "Private network"), None);
    assert!(plain["args"].get("destinations").is_none());
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
    assert_eq!(row(&waiting, "Private network"), Some(WORDS));

    // Approved, it is created, and its record carries the destinations and nothing of a tenant.
    let made = approve(&rig, &proposal).expect("approved and created");
    assert_eq!(made["destinations"], stated());
    let record = rig.entry("service-runners");
    assert_eq!(record["destinations"], stated());
    assert_eq!(record["project"], PROJECT);
    assert!(record.get("tenant").is_none());
    assert!(record.get("vm").is_some(), "a virtual machine");

    // Its NIC rules are the ones built from the record: each address is out of the reject of the private ranges, has
    // its own rejects for everything but tcp 443, and the node's own addresses stay rejected whole.
    let gateway = record["gateway"].as_str().expect("the gateway");
    let held = egress(&rig, "runners");
    assert_eq!(held, rules(gateway, &destinations()));
    for address in ["10.20.30.40", "100.101.102.103"] {
        assert_eq!(
            rules_for(&held, address),
            [
                ("reject", "tcp", "1-442,444-65535"),
                ("reject", "udp", ""),
                ("reject", "icmp4", ""),
                ("allow", "tcp", "443")
            ]
            .map(|(action, protocol, ports)| (
                action.to_owned(),
                protocol.to_owned(),
                ports.to_owned()
            )),
            "{address}"
        );
    }
    let ranges = held[0]["destination"].as_str().expect("the reject");
    for whole in ["10.0.0.0/8", "100.64.0.0/10"] {
        assert!(!ranges.split(',').any(|range| range == whole), "{whole}");
    }
    for kept in [
        "172.16.0.0/12",
        "169.254.0.0/16",
        "127.0.0.0/8",
        "10.20.30.41/32",
        "100.101.102.102/32",
    ] {
        assert!(ranges.split(',').any(|range| range == kept), "{kept}");
    }
    assert_eq!(held[3]["action"], "reject");
    assert_eq!(
        held[3]["destination"],
        format!("10.20.30.1/32,100.99.1.2/32,{gateway}/32")
    );

    // An ordinary service beside it has the five rules it always had, and no key in its record or its views.
    rig.call(
        "service_create",
        json!({ "name": "web", "template": "app" }),
    )
    .expect("an ordinary service");
    let ordinary = egress(&rig, "web");
    assert_eq!(ordinary, rules(gateway, &[]));
    assert_eq!(ordinary.len(), 5);
    assert_eq!(held[1..4], ordinary[1..4]);
    assert!(rig.entry("service-web").get("destinations").is_none());

    // service_status says whether each one stands; the list names them without checking.
    let status = |rig: &Rig, name: &str| {
        rig.call("service_status", json!({ "name": name }))
            .expect("status")
    };
    assert_eq!(
        status(&rig, "runners")["destinations"],
        json!([
            { "address": "10.20.30.40", "port": 443, "ready": true },
            { "address": "100.101.102.103", "port": 443, "ready": true }
        ])
    );
    assert!(status(&rig, "web").get("destinations").is_none());
    let listed = rig.call("service_list", json!({})).expect("list");
    let of = |name: &str| {
        listed["services"]
            .as_array()
            .expect("services")
            .iter()
            .find(|row| row["name"] == name)
            .expect("its row")
            .clone()
    };
    assert_eq!(of("runners")["destinations"], stated());
    assert!(of("web").get("destinations").is_none());

    // The node gains an address: until the service's rules are written again, the rules Incus holds are not the ones
    // the record gives, and status says so. An address of the node's own is never one a rule opens.
    let mut more = interfaces();
    more.push(("br2".into(), vec![IpAddr::from([10, 77, 0, 1])]));
    rig.fake().set_host_interfaces(more);
    let differs =
        "the network rules Incus holds for this service are not the ones its record gives";
    assert_eq!(
        status(&rig, "runners")["destinations"],
        json!([
            { "address": "10.20.30.40", "port": 443, "ready": false, "problem": differs },
            { "address": "100.101.102.103", "port": 443, "ready": false, "problem": differs }
        ])
    );
    let mut taken = interfaces();
    taken[0].1.push(IpAddr::from(BRIDGE_ADDRESS));
    rig.fake().set_host_interfaces(taken);
    assert_eq!(
        status(&rig, "runners")["destinations"][0],
        json!({
            "address": "10.20.30.40", "port": 443, "ready": false,
            "problem": "10.20.30.40 is this node's own (one of its addresses, or the network or broadcast address of one of its networks), and a service's rules never open one"
        })
    );

    // Removing the service removes its rules with it.
    rig.fake().set_host_interfaces(interfaces());
    rig.approved("service_remove", json!({ "name": "runners" }))
        .expect("the removal");
    assert!(acl(&rig, "runners").is_none());
}

#[test]
fn a_config_that_changes_after_the_card_was_shown_is_not_created_by_that_approval() {
    let changed = "this node's config lets the service runners reach 10.20.30.40 on tcp port 8443 through this node (kernel.serviceDestinations), and the proposal the owner approved does not say that: the config changed after it was proposed. Propose the create again, so the owner's card says it";
    // The owner is shown a card for tcp 443 on two addresses; the operator's config then names another port.
    let mut rig = node(named());
    let proposal =
        propose(&rig, json!({ "name": "runners", "template": "app" })).expect("proposed");
    rig.kernel_config = kernel(json!([entry("runners", BRIDGE_ADDRESS, 8443)]));
    rig = rig.restart();
    let before = rig.fake().writes().len();
    let (code, message) = refusal(approve(&rig, &proposal));
    assert_eq!((code.as_str(), message.as_str()), ("refused", changed));

    // The owner is shown a card with no private network row; the config then names the service.
    rig.kernel_config = kernel(json!([]));
    rig = rig.restart();
    let plain = propose(&rig, json!({ "name": "runners", "template": "app" })).expect("proposed");
    assert_eq!(row(&plain, "Private network"), None);
    rig.kernel_config = kernel(json!([entry("runners", BRIDGE_ADDRESS, 8443)]));
    rig = rig.restart();
    let (code, message) = refusal(approve(&rig, &plain));
    assert_eq!((code.as_str(), message.as_str()), ("refused", changed));

    // The owner is shown a card with the row; the config then loses the entry. Nothing is opened that the config
    // does not name now.
    let shown = propose(&rig, json!({ "name": "runners", "template": "app" })).expect("proposed");
    assert_eq!(
        shown["args"]["destinations"],
        json!([{ "address": "10.20.30.40", "port": 8443 }])
    );
    rig.kernel_config = kernel(json!([]));
    rig = rig.restart();
    let result = approve(&rig, &shown);
    assert!(
        matches!(
            &result,
            Err(CallError::Invalid(message))
                if message == "destinations: this node's config (kernel.serviceDestinations) names none for the service runners. Only the config names one, never a call"
        ),
        "{result:?}"
    );
    assert_eq!(rig.fake().writes().len(), before);
    assert!(acl(&rig, "runners").is_none());
    assert!(
        !rig.entries()
            .iter()
            .any(|entry| entry.id.0 == "service-runners")
    );
}

#[test]
fn nothing_in_a_tenant_gets_a_destination() {
    let rig = node(named());
    // A tenant's service that bears the very name the config names is an ordinary tenant service: the list is not
    // read for it, it needs no approval, and its record and rules carry nothing of a destination.
    let made = rig
        .call(
            "service_create",
            json!({ "name": "runners", "template": "app", "tenant": A }),
        )
        .expect("a tenant's service");
    assert!(made.get("destinations").is_none(), "{made}");
    let id = format!("service-{A}-runners");
    let record: arca::service::ServiceConfig = serde_json::from_value(
        rig.entries()
            .into_iter()
            .find(|entry| entry.plugin.0 == "arca.service" && entry.config["name"] == "runners")
            .unwrap_or_else(|| panic!("the record {id}"))
            .config,
    )
    .expect("the record");
    assert!(record.tenant.is_some() && record.destinations.is_empty());
    let block = arca_netproject::tenant::parse_block(
        &record.tenant.as_ref().expect("a tenant").block,
        CIDR,
    )
    .expect("the tenant's block");
    let held = rig
        .get(&format!(
            "/1.0/network-acls/{}?project=default",
            record.acl()
        ))
        .expect("its ACL");
    let gateway: Ipv4Addr = record.gateway.parse().expect("the gateway");
    assert_eq!(
        held["egress"].as_array().expect("egress"),
        &arca_netproject::tenant::egress_rules(gateway, &hosts(), Some(block), &[])
    );
    // Whatever a record said, a tenant's rules take no destination.
    assert_eq!(
        arca_netproject::tenant::egress_rules(gateway, &hosts(), Some(block), &destinations()),
        arca_netproject::tenant::egress_rules(gateway, &hosts(), Some(block), &[])
    );
    for address in ["10.20.30.40", "100.101.102.103"] {
        assert!(rules_for(held["egress"].as_array().unwrap(), address).is_empty());
    }

    // A call in a tenant that states one is refused before any write, as a create and as a proposal.
    let before = rig.fake().writes().len();
    for name in ["second", "other"] {
        let args =
            json!({ "name": name, "template": "app", "tenant": A, "destinations": stated() });
        for result in [
            rig.call("service_create", args.clone()),
            propose(&rig, args.clone()),
        ] {
            assert!(
                matches!(
                    &result,
                    Err(CallError::Invalid(message))
                        if message == "destinations: nothing in a tenant reaches a private address through the node. Only the node's config names one, and only for a service of the node's own project"
                ),
                "{name}: {result:?}"
            );
        }
    }
    assert_eq!(rig.fake().writes().len(), before);
    // The node's own service of that name is still the one the config means.
    let (code, _) = refusal(rig.call(
        "service_create",
        json!({ "name": "runners", "template": "app" }),
    ));
    assert_eq!(code, "NotApproved");
}

/// What `serve` answers when it is started over the rig's desired state with a config that names `destinations`. The
/// state directory is copied, since the rig's own kernel holds its lock.
fn serve_start(rig: &Rig, destinations: Value) -> Result<(), String> {
    let copy = rig
        .dir
        .join(format!("start-{}", SEQ.fetch_add(1, Ordering::SeqCst)));
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
    let cfg = node_config(&copy, &rig.socket, &kernel(destinations), &json!({}));
    arca::config::check(&cfg).expect("the config itself is one serve accepts");
    arca::cmd_serve::check_bridged_start(&cfg, &copy.join("sys")).map_err(|failure| match failure {
        arca::Failure::Config(why) => why,
        other => panic!("not a config refusal: {other:?}"),
    })
}

#[test]
fn serve_refuses_to_start_when_the_config_loses_a_destination_a_record_carries() {
    let rig = node(named());
    rig.call(
        "service_create",
        json!({ "name": "web", "template": "app" }),
    )
    .expect("an ordinary service");
    // A node on which no service has a destination starts with any of these configs: an entry opens nothing by
    // itself, and an existing service is not changed by one.
    let both = named();
    let bridge_only = json!([entry("runners", BRIDGE_ADDRESS, 443)]);
    let other_port = json!([
        entry("runners", TAILNET_ADDRESS, 443),
        entry("runners", BRIDGE_ADDRESS, 8443)
    ]);
    let more = json!([
        entry("runners", TAILNET_ADDRESS, 443),
        entry("runners", BRIDGE_ADDRESS, 443),
        entry("runners", BRIDGE_ADDRESS, 22),
        entry("web", BRIDGE_ADDRESS, 443)
    ]);
    let another = json!([
        entry("web", BRIDGE_ADDRESS, 443),
        entry("web", TAILNET_ADDRESS, 443)
    ]);
    for config in [
        &both,
        &bridge_only,
        &other_port,
        &more,
        &another,
        &json!([]),
    ] {
        assert_eq!(serve_start(&rig, config.clone()), Ok(()), "{config}");
    }
    assert!(rig.entry("service-web").get("destinations").is_none());

    rig.approved(
        "service_create",
        json!({ "name": "runners", "template": "app" }),
    )
    .expect("the create");
    // With one, the start is refused unless the config still names everything the record carries, for that service.
    let lost = |what: &str| {
        Err(format!(
            "the service runners may reach {what} through this node, as its owner approved, and this config's kernel.serviceDestinations does not name that for it: put the entry back, or remove the service before taking it out"
        ))
    };
    assert_eq!(
        serve_start(&rig, json!([])),
        lost("10.20.30.40 on tcp port 443")
    );
    assert_eq!(
        serve_start(&rig, another),
        lost("10.20.30.40 on tcp port 443")
    );
    assert_eq!(
        serve_start(&rig, bridge_only),
        lost("100.101.102.103 on tcp port 443")
    );
    assert_eq!(
        serve_start(&rig, other_port),
        lost("10.20.30.40 on tcp port 443")
    );
    assert_eq!(serve_start(&rig, both), Ok(()));
    // More entries than the record carries start too: they open nothing for a service that exists.
    assert_eq!(serve_start(&rig, more), Ok(()));
    assert_eq!(
        arca::handlers::destinations_refusal(&rig.cfg, &rig.entries()),
        None
    );
    // Once the service is removed, the entries may go.
    rig.approved("service_remove", json!({ "name": "runners" }))
        .expect("the removal");
    assert_eq!(serve_start(&rig, json!([])), Ok(()));
}

#[test]
fn an_address_of_the_nodes_own_is_never_a_destination() {
    let own = "this node's config names 10.20.30.1 as a private destination of the service runners, and that address is this node's own (one of its addresses, or the network or broadcast address of one of its networks): a service's rules never open one. Take the entry out of kernel.serviceDestinations";
    // The config cannot know the node's addresses, so the entry loads; the proposal is refused, and no card is shown.
    let rig = node(json!([entry("runners", [10, 20, 30, 1], 443)]));
    let before = rig.fake().writes().len();
    let (code, message) = refusal(propose(
        &rig,
        json!({ "name": "runners", "template": "app" }),
    ));
    assert_eq!((code.as_str(), message.as_str()), ("refused", own));
    assert_eq!(
        rig.call("plan_list", json!({})).expect("plans")["plans"],
        json!([])
    );
    assert_eq!(rig.fake().writes().len(), before);

    // An address the node takes after the card was shown: the create asks again and refuses before any write.
    let rig = node(json!([entry("runners", [10, 20, 30, 1], 443)]));
    rig.fake().set_host_interfaces(vec![]);
    let proposal =
        propose(&rig, json!({ "name": "runners", "template": "app" })).expect("proposed");
    rig.fake().set_host_interfaces(interfaces());
    let before_create = rig.fake().writes().len();
    let (code, message) = refusal(approve(&rig, &proposal));
    assert_eq!((code.as_str(), message.as_str()), ("refused", own));
    assert_eq!(rig.fake().writes().len(), before_create);
    assert!(acl(&rig, "runners").is_none());
}

#[test]
fn the_ends_of_the_nodes_own_networks_are_the_nodes_own_too() {
    // The node is 10.20.30.1/24 on its other bridge. A packet to that network's broadcast or network address is
    // answered by the node itself, so neither is a destination, while an ordinary address of the network is.
    for (address, own) in [
        ([10, 20, 30, 255], true),
        ([10, 20, 30, 0], true),
        ([10, 20, 30, 254], false),
    ] {
        let rig = node(json!([entry("runners", address, 443)]));
        rig.fake()
            .set_host_netmask(IpAddr::from([10, 20, 30, 1]), 24);
        let result = propose(&rig, json!({ "name": "runners", "template": "app" }));
        if own {
            let (code, message) = refusal(result);
            assert_eq!(code, "refused");
            assert_eq!(
                message,
                format!(
                    "this node's config names {} as a private destination of the service runners, and that address is this node's own (one of its addresses, or the network or broadcast address of one of its networks): a service's rules never open one. Take the entry out of kernel.serviceDestinations",
                    Ipv4Addr::from(address)
                )
            );
        } else {
            let proposal = result.expect("proposed");
            approve(&rig, &proposal).expect("approved and created");
            // The rules Incus holds are the ones built over the addresses with their netmask, and status agrees.
            let status = rig
                .call("service_status", json!({ "name": "runners" }))
                .expect("status");
            assert_eq!(
                status["destinations"],
                json!([{ "address": "10.20.30.254", "port": 443, "ready": true }])
            );
        }
    }
}

#[test]
fn a_machine_on_a_host_bridge_is_not_created_with_a_destination() {
    // A mixed node: its own service network, and a host bridge a virtual machine may name. On the host bridge Arca
    // writes no rules, so an entry there would promise what nothing enforces.
    let mut config = kernel(named());
    config["bridged"] = json!({
        "bridges": ["br0"], "pool": "vmpool",
        "limits": { "virtualMachines": 4, "cpu": 16, "memory": "64GiB", "disk": "1TiB" }
    });
    config.as_object_mut().expect("kernel").remove("vmLimits");
    let rig = Rig::with_kernel(config, empty_state());
    let mut bare = vm_template();
    bare["name"] = json!("bare");
    bare["ports"] = json!([]);
    rig.template("bare", bare);
    let before = rig.fake().writes().len();
    let (code, message) = refusal(propose(
        &rig,
        json!({ "name": "runners", "template": "bare", "bridge": "br0" }),
    ));
    assert_eq!(code, "refused");
    assert_eq!(
        message,
        "this node's config names a private destination for the service runners (kernel.serviceDestinations), and this create puts it on a host bridge, where Arca writes no network rules: the machine reaches whatever that network reaches. Create it on the managed service network, or take the entry out of the config"
    );
    assert_eq!(rig.fake().writes().len(), before);
    // The same machine on the managed service network is proposed with its row.
    let proposal =
        propose(&rig, json!({ "name": "runners", "template": "bare" })).expect("proposed");
    assert_eq!(row(&proposal, "Private network"), Some(WORDS));
    // And a machine the config does not name goes on the host bridge as before.
    let other = propose(
        &rig,
        json!({ "name": "other", "template": "bare", "bridge": "br0" }),
    )
    .expect("proposed");
    assert_eq!(row(&other, "Private network"), None);
}
