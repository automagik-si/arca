//! The service plugin and a record that names a private port, against the fake Incus: the plugin makes the one proxy
//! device from the record alone, listening on the record's address and on no other, and refuses a record that gives the
//! port to anything but a container of the node's own project. Every address is from RFC 5737.
use super::*;

/// The record of a service `leaf` that does not exist yet, made from an ordinary service's record, with a private port.
fn leaf_record(rig: &Rig) -> service::ServiceConfig {
    rig.template("nats", nats_template());
    rig.call("service_create", json!({"name":"nats","template":"nats"}))
        .expect("an ordinary service");
    let mut record = rig.service_config("nats");
    record.name = "leaf".into();
    record.instance = "leaf".into();
    record.ipv4 = "192.0.2.11".into();
    record.private_port = Some(service::PrivateListen {
        address: "198.51.100.7".into(),
        port: 4222,
    });
    record
}

#[test]
fn the_plugin_makes_the_private_device_from_the_record_alone() {
    let rig = Rig::new();
    let record = leaf_record(&rig);
    rig.apply(&record).expect("applied");
    let instance = rig
        .get(&format!("/1.0/instances/leaf?project={PROJECT}"))
        .expect("the instance");
    assert_eq!(
        instance["devices"]["private"],
        json!({
            "type": "proxy",
            "listen": "tcp:198.51.100.7:4222",
            "connect": "tcp:127.0.0.1:4222"
        })
    );
    assert_eq!(
        instance["devices"]["private"],
        service::private_device(record.private_port.as_ref().unwrap())
    );
    // The ordinary service beside it has no such device, and its record has no such key.
    let nats = rig
        .get(&format!("/1.0/instances/nats?project={PROJECT}"))
        .expect("the other instance");
    assert!(nats["devices"].get("private").is_none());
    assert!(
        serde_json::to_value(rig.service_config("nats"))
            .unwrap()
            .get("privatePort")
            .is_none()
    );
    // The view names the address and the port, and an ordinary one has no key.
    assert_eq!(
        service::view(&record, None)["privatePort"],
        json!({"address":"198.51.100.7","port":4222})
    );
    assert!(
        service::view(&rig.service_config("nats"), None)
            .get("privatePort")
            .is_none()
    );
    // A second pass over the same record writes nothing.
    let writes = rig.fake.writes().len();
    rig.apply(&record).expect("applied again");
    assert_eq!(rig.fake.writes().len(), writes);
}

#[test]
fn the_plugin_refuses_a_record_that_gives_a_private_port_to_anything_else() {
    let rig = Rig::new();
    let record = leaf_record(&rig);
    let refused = "a private port is for a container of the node's own service project, on one of its own ports and one unicast host address, and never beside a lan port";
    let at = |address: &str, port: u16| {
        Some(service::PrivateListen {
            address: address.into(),
            port,
        })
    };
    let mut cases: Vec<(&str, service::ServiceConfig)> = Vec::new();
    let mut tenant = record.clone();
    tenant.project = "arcak-t-aaaaaaaa".into();
    tenant.instance = "leaf-aaaaaaaa".into();
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
    let mut vm = record.clone();
    vm.vm = Some(service::Vm {
        root_size: "20GiB".into(),
        mac: None,
        user_data: None,
        vendor_data: None,
        settings: None,
    });
    cases.push(("a virtual machine", vm));
    let mut beside = record.clone();
    beside.lan = Some(service::Lan { port: 4222 });
    cases.push(("beside a lan port", beside));
    for (what, address, port) in [
        ("a port that is not its own", "198.51.100.7", 5000),
        ("port zero", "198.51.100.7", 0),
        ("the wildcard", "0.0.0.0", 4222),
        ("the loopback", "127.0.0.1", 4222),
        ("a multicast address", "224.0.0.1", 4222),
        ("the broadcast address", "255.255.255.255", 4222),
        ("a range", "198.51.100.0/24", 4222),
        ("a name", "link.example", 4222),
        ("a padded address", "198.051.100.7", 4222),
        ("nothing", "", 4222),
        ("a listen string", "198.51.100.7:4222,bind=host", 4222),
    ] {
        let mut case = record.clone();
        case.private_port = at(address, port);
        cases.push((what, case));
    }
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
        rig.get(&format!("/1.0/instances/leaf?project={PROJECT}"))
            .is_none()
    );
}
