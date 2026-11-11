//! The service plugin and a record that names kernel access, against the fake Incus: the plugin makes the device from
//! the record alone, and refuses a record that gives the access to anything but a container of the node's own project.
use super::*;

const KERNEL: &str = "127.0.0.1:7460";

/// The record of a service `cloud` that does not exist yet, made from an ordinary service's record, with kernel access.
fn cloud_record(rig: &Rig) -> service::ServiceConfig {
    rig.template("nats", nats_template());
    rig.call("service_create", json!({"name":"nats","template":"nats"}))
        .expect("an ordinary service");
    let mut record = rig.service_config("nats");
    record.name = "cloud".into();
    record.instance = "cloud".into();
    record.ipv4 = "192.0.2.11".into();
    record.kernel_access = Some(service::KernelLink {
        port: 7461,
        connect: KERNEL.into(),
    });
    record
}

#[test]
fn the_plugin_makes_the_kernel_access_device_from_the_record_alone() {
    let rig = Rig::new();
    let record = cloud_record(&rig);
    rig.apply(&record).expect("applied");
    let instance = rig
        .get(&format!("/1.0/instances/cloud?project={PROJECT}"))
        .expect("the instance");
    assert_eq!(
        instance["devices"]["kernel"],
        json!({
            "type": "proxy",
            "bind": "instance",
            "listen": "tcp:127.0.0.1:7461",
            "connect": "tcp:127.0.0.1:7460"
        })
    );
    assert_eq!(
        instance["devices"]["kernel"],
        service::kernel_device(record.kernel_access.as_ref().unwrap())
    );
    // The ordinary service beside it has no such device, and its record has no such key.
    let nats = rig
        .get(&format!("/1.0/instances/nats?project={PROJECT}"))
        .expect("the other instance");
    assert!(nats["devices"].get("kernel").is_none());
    assert!(
        serde_json::to_value(rig.service_config("nats"))
            .unwrap()
            .get("kernelAccess")
            .is_none()
    );
    // A second pass over the same record writes nothing.
    let writes = rig.fake.writes().len();
    rig.apply(&record).expect("applied again");
    assert_eq!(rig.fake.writes().len(), writes);
}

#[test]
fn the_plugin_refuses_a_record_that_gives_kernel_access_to_anything_else() {
    let rig = Rig::new();
    let record = cloud_record(&rig);
    let refused = "kernel access is for a container of the node's own service project, on a port of 1024 or above that is not one of its own, to the kernel's listener on 127.0.0.1";
    let link = |port: u16, connect: &str| {
        Some(service::KernelLink {
            port,
            connect: connect.into(),
        })
    };
    let mut cases: Vec<(&str, service::ServiceConfig)> = Vec::new();
    // In a tenant, with everything else a tenant's record holds.
    let mut tenant = record.clone();
    tenant.project = "arcak-t-aaaaaaaa".into();
    tenant.instance = "cloud-aaaaaaaa".into();
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
    });
    cases.push(("a virtual machine", vm));
    for (what, port, connect) in [
        ("a low port", 1023, KERNEL),
        ("one of its own ports", 4222, KERNEL),
        ("the bridge gateway", 7461, "192.0.2.1:7460"),
        ("a wildcard", 7461, "0.0.0.0:7460"),
        ("another loopback address", 7461, "127.0.0.2:7460"),
        ("IPv6 loopback", 7461, "[::1]:7460"),
        ("a host name", 7461, "localhost:7460"),
        ("no port", 7461, "127.0.0.1:0"),
        ("another spelling", 7461, "127.0.0.1:07460"),
        ("a unix socket", 7461, "/run/arca/arca.sock"),
    ] {
        let mut case = record.clone();
        case.kernel_access = link(port, connect);
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
        rig.get(&format!("/1.0/instances/cloud?project={PROJECT}"))
            .is_none()
    );
}
