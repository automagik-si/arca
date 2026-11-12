//! The service plugin and a record that names private destinations, against the fake Incus: the plugin builds the
//! NIC's rules from the record alone, writes them again when the record changes, and refuses a record that gives a
//! destination to anything but a service of the node's own project on the managed network. Every address is synthetic.
use super::*;
use arca_netproject::destination::Destination;
use std::net::IpAddr;

fn to(address: [u8; 4], port: u16) -> Destination {
    Destination {
        address: address.into(),
        port,
    }
}

/// The record of a service `runners` that does not exist yet, made from an ordinary service's record, on a node whose
/// own addresses are 10.20.30.1 (another bridge) and 100.99.1.2 (its tailnet).
fn runners_record(rig: &Rig) -> service::ServiceConfig {
    rig.fake.set_host_interfaces(vec![
        ("br1".into(), vec![IpAddr::from([10, 20, 30, 1])]),
        ("ts0".into(), vec![IpAddr::from([100, 99, 1, 2])]),
    ]);
    rig.template("nats", nats_template());
    rig.call("service_create", json!({"name":"nats","template":"nats"}))
        .expect("an ordinary service");
    let mut record = rig.service_config("nats");
    record.name = "runners".into();
    record.instance = "runners".into();
    record.ipv4 = "192.0.2.11".into();
    record
}

fn egress(rig: &Rig, name: &str) -> Vec<Value> {
    rig.get(&format!(
        "/1.0/network-acls/{PROJECT}-svc-{name}?project=default"
    ))
    .expect("its ACL")["egress"]
        .as_array()
        .expect("egress")
        .clone()
}

fn wanted(record: &service::ServiceConfig) -> Vec<Value> {
    arca_netproject::tenant::egress_rules(
        record.gateway.parse().expect("the gateway"),
        &json!(["10.20.30.1", "100.99.1.2"]),
        None,
        &record.destinations,
    )
}

#[test]
fn the_plugin_builds_the_rules_from_the_record_and_writes_them_again_when_it_changes() {
    let rig = Rig::new();
    let mut record = runners_record(&rig);
    record.destinations = vec![to([10, 20, 30, 40], 443)];
    rig.apply(&record).expect("applied");
    let ordinary = egress(&rig, "nats");
    assert_eq!(ordinary.len(), 5);
    let held = egress(&rig, "runners");
    assert_eq!(held, wanted(&record));
    // Four rules more than an ordinary service, all of them for the one address. Of the others, the reject of the
    // private ranges no longer holds that address, and neither does the last allow.
    assert_eq!(held.len(), 9);
    assert!(
        held[4..8]
            .iter()
            .all(|rule| rule["destination"] == "10.20.30.40/32")
    );
    assert_eq!(held[1..4], ordinary[1..4]);
    // The last allow, which is every address for an ordinary service, names every address but the opened one.
    assert_eq!(ordinary[4]["destination"], "");
    assert_eq!(held[8]["action"], "allow");
    assert_eq!(
        held[8]["destination"]
            .as_str()
            .expect("a list")
            .split(',')
            .count(),
        32
    );
    assert_ne!(held[0], ordinary[0]);
    // The ordinary service beside it was not written, and its record has no such key.
    assert!(
        serde_json::to_value(rig.service_config("nats"))
            .unwrap()
            .get("destinations")
            .is_none()
    );
    // A second pass over the same record writes nothing.
    let writes = rig.fake.writes().len();
    rig.apply(&record).expect("applied again");
    assert_eq!(rig.fake.writes().len(), writes);

    // The record gains a destination: the rules on the ACL that exists are written again, and nothing else is.
    record.destinations.push(to([100, 101, 102, 103], 443));
    rig.apply(&record).expect("applied with two");
    assert_eq!(egress(&rig, "runners"), wanted(&record));
    assert_eq!(egress(&rig, "runners").len(), 13);
    assert_eq!(rig.fake.writes().len(), writes + 1);
    assert_eq!(egress(&rig, "nats"), ordinary);

    // The record loses them: the rules are the ordinary ones again, to the word.
    record.destinations.clear();
    rig.apply(&record).expect("applied with none");
    assert_eq!(egress(&rig, "runners"), ordinary);
    assert_eq!(rig.fake.writes().len(), writes + 2);

    // A record that names an address of the node's own opens nothing: the rules are the ordinary ones.
    record.destinations = vec![to([10, 20, 30, 1], 443), to([100, 99, 1, 2], 443)];
    let writes = rig.fake.writes().len();
    rig.apply(&record)
        .expect("applied with the node's own addresses");
    assert_eq!(egress(&rig, "runners"), ordinary);
    assert_eq!(rig.fake.writes().len(), writes);
}

#[test]
fn the_plugin_refuses_a_record_that_gives_a_destination_to_anything_else() {
    let rig = Rig::new();
    let mut record = runners_record(&rig);
    record.destinations = vec![to([10, 20, 30, 40], 443)];
    let refused = "a private destination is for a service of the node's own project on the managed service network: one private or tailnet address outside the service subnet and one tcp port each, at most 4";
    let mut cases: Vec<(&str, service::ServiceConfig)> = Vec::new();
    // In a tenant, with everything else a tenant's record holds.
    let mut tenant = record.clone();
    tenant.project = "arcak-t-aaaaaaaa".into();
    tenant.instance = "runners-aaaaaaaa".into();
    tenant.tenant = Some(service::TenantRef {
        id: "aaaaaaaa".into(),
        block: "192.0.2.16/29".into(),
        front: None,
        deploy: None,
    });
    cases.push(("a tenant's service", tenant));
    let mut elsewhere = record.clone();
    elsewhere.project = "other".into();
    cases.push(("another project", elsewhere));
    // A virtual machine on a host bridge has no rules of Arca's.
    let mut host = record.clone();
    host.bridge = "br0".into();
    host.vm = Some(service::Vm {
        root_size: "20GiB".into(),
        mac: None,
        user_data: None,
        vendor_data: None,
        settings: None,
    });
    cases.push(("a machine on a host bridge", host));
    for (what, list) in [
        ("a public address", vec![to([203, 0, 113, 9], 443)]),
        ("the host's loopback", vec![to([127, 0, 0, 1], 443)]),
        ("the metadata address", vec![to([169, 254, 169, 254], 80)]),
        ("multicast", vec![to([224, 0, 0, 1], 443)]),
        ("no port", vec![to([10, 20, 30, 40], 0)]),
        (
            "one beside a good one",
            vec![to([10, 20, 30, 40], 443), to([8, 8, 8, 8], 443)],
        ),
        (
            "the same one twice",
            vec![to([10, 20, 30, 40], 443), to([10, 20, 30, 40], 443)],
        ),
        (
            "five",
            (1..=5).map(|n| to([10, 20, 30, 40 + n], 443)).collect(),
        ),
    ] {
        let mut case = record.clone();
        case.destinations = list;
        cases.push((what, case));
    }
    // An address of the service subnet, on a node whose subnet is a private one.
    let mut inside = record.clone();
    inside.gateway = "10.90.0.1".into();
    inside.ipv4 = "10.90.0.11".into();
    inside.destinations = vec![to([10, 90, 0, 77], 443)];
    cases.push(("an address of the service subnet", inside));
    let writes = rig.fake.writes().len();
    for (what, case) in cases {
        match rig.apply(&case) {
            Err(arca_kernel::plugin::PluginError::Config(message)) => {
                assert_eq!(message, refused, "{what}")
            }
            other => panic!("{what}: expected the refusal, got {other:?}"),
        }
    }
    assert_eq!(rig.fake.writes().len(), writes, "nothing was written");
    assert!(
        rig.get(&format!("/1.0/instances/runners?project={PROJECT}"))
            .is_none()
    );
}
