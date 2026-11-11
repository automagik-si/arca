//! The service plugin and a record that names kernel access, against the fake Incus: the plugin makes the device from
//! the record alone, and refuses a record that gives the access to anything but a container of the node's own project.
use super::*;

/// The services socket of the node these records were made on.
const KERNEL: &str = "/run/arca/services.sock";

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
            "connect": "unix:/run/arca/services.sock"
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
    let refused = "kernel access is for a container of the node's own service project, on a port of 1024 or above that is not one of its own, to the kernel's services socket";
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
        // Only a plain socket path is a place a device may connect to: no address of any network, the host's
        // loopback included, and nothing a device value could read as more than a path.
        ("the host's loopback", 7461, "127.0.0.1:7460"),
        ("the bridge gateway", 7461, "192.0.2.1:7460"),
        ("a wildcard", 7461, "0.0.0.0:7460"),
        ("a host name", 7461, "localhost:7460"),
        ("a relative path", 7461, "run/arca/services.sock"),
        (
            "a path with a parent step",
            7461,
            "/run/arca/../incus/unix.socket",
        ),
        (
            "a path with a comma",
            7461,
            "/run/arca/services.sock,mode=0666",
        ),
        ("a path with a space", 7461, "/run/arca/services sock"),
        ("a directory", 7461, "/run/arca/"),
        ("nothing", 7461, ""),
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

/// A record that is lost and made again from the instance's own marks says what the instance has: the device stays on
/// the instance, so the record names the access. Only a device to this node's own services socket is read, and only on
/// an instance of the node's own project. A device somebody made by hand for another path is not an access Arca gave:
/// the record does not name it, and it never stops `serve` from starting.
#[test]
fn a_record_made_again_from_an_instance_reads_the_kernel_access_its_device_shows() {
    let device = service::kernel_device(&service::KernelLink {
        port: 7461,
        connect: KERNEL.into(),
    });
    let with_device = |name: &str, project: &str, ipv4: &str, uuid: &str, device: &Value| {
        let mut row = adopted_instance(name, project, "cloud", ipv4, "8080", uuid);
        row["devices"]["kernel"] = device.clone();
        row
    };
    let mut other = device.clone();
    other["connect"] = json!("tcp:127.0.0.1:7460");
    let elsewhere = service::kernel_device(&service::KernelLink {
        port: 7461,
        connect: "/run/other/hand-made.sock".into(),
    });
    let state = || FakeState {
        server: json!({}),
        projects: vec![],
        instances: vec![
            with_device("cloud", PROJECT, "192.0.2.10", "uuid-cloud", &device),
            // The same device in another manager's project: not the node's own service.
            with_device("far", "arca", "192.0.2.11", "uuid-far", &device),
            // A device of the same name and another shape is not the access Arca gives.
            with_device("odd", PROJECT, "192.0.2.12", "uuid-odd", &other),
            // A device of the right shape that somebody pointed at another socket by hand.
            with_device("hand", PROJECT, "192.0.2.13", "uuid-hand", &elsewhere),
        ],
        networks: vec![],
        pools: vec![],
    };
    let projects = || vec![PROJECT.to_string(), "arca".into()];
    let rig = Rig::with_services(
        state(),
        projects(),
        Some("192.0.2.0/24"),
        None,
        None,
        Some(KERNEL),
    );
    let listed = rig.call("service_list", json!({})).expect("list");
    let row = |name: &str| {
        listed["services"]
            .as_array()
            .expect("rows")
            .iter()
            .find(|row| row["name"] == name)
            .unwrap_or_else(|| panic!("the row of {name}"))
            .clone()
    };
    assert_eq!(
        row("cloud")["kernelAccess"],
        json!({ "port": 7461, "connect": KERNEL })
    );
    for name in ["far", "odd", "hand"] {
        assert!(row(name).get("kernelAccess").is_none(), "{name}");
    }
    assert_eq!(
        rig.service_config("cloud").kernel_access,
        Some(service::KernelLink {
            port: 7461,
            connect: KERNEL.into()
        })
    );
    assert_eq!(rig.service_config("hand").kernel_access, None);
    // So the records made here never stand between this node and its next start.
    assert_eq!(
        arca::handlers::kernel_access_refusal(&rig.cfg, &rig.entries()),
        None
    );
    // The record follows the device. Pointed somewhere else by hand, or taken off, it leaves the record at the next
    // call, and a start over a config with no services socket is then not refused.
    let without_socket = |rig: &Rig| {
        let mut cfg = (*rig.cfg).clone();
        cfg.listen.services = None;
        arca::handlers::kernel_access_refusal(&cfg, &rig.entries())
    };
    assert_eq!(
        without_socket(&rig).as_deref(),
        Some(
            "the service cloud has access to this node's kernel through the socket /run/arca/services.sock, and this config sets no listen.services: set listen.services to /run/arca/services.sock, or remove the service before taking it out"
        )
    );
    rig.edit(
        "PATCH",
        &format!("/1.0/instances/cloud?project={PROJECT}"),
        json!({"devices":{
            "eth0":{"type":"nic","name":"eth0","ipv4.address":"192.0.2.10"},
            "kernel": elsewhere
        }}),
    );
    rig.call("service_list", json!({})).expect("list");
    assert_eq!(rig.service_config("cloud").kernel_access, None);
    assert_eq!(without_socket(&rig), None);
    // Put back as Arca made it, the record names it again.
    rig.edit(
        "PATCH",
        &format!("/1.0/instances/cloud?project={PROJECT}"),
        json!({"devices":{
            "eth0":{"type":"nic","name":"eth0","ipv4.address":"192.0.2.10"},
            "kernel": device
        }}),
    );
    rig.call("service_list", json!({})).expect("list");
    assert!(rig.service_config("cloud").kernel_access.is_some());
    // On a node with no services socket, no device is read back at all.
    let bare = Rig::with(state(), projects(), Some("192.0.2.0/24"));
    bare.call("service_list", json!({})).expect("list");
    assert_eq!(bare.service_config("cloud").kernel_access, None);
    assert_eq!(
        arca::handlers::kernel_access_refusal(&bare.cfg, &bare.entries()),
        None
    );
    // The reading is exact: it gives back only what `kernel_device` makes.
    assert_eq!(service::kernel_link_of(&json!({ "devices": {} })), None);
    for (key, value) in [
        ("bind", "host"),
        ("type", "nic"),
        ("listen", "tcp:0.0.0.0:7461"),
        ("listen", "tcp:127.0.0.1:80"),
        ("connect", "unix:/run/arca/../incus/unix.socket"),
        ("connect", "unix:/var/lib/incus/unix.socket,x"),
    ] {
        let mut changed = device.clone();
        changed[key] = json!(value);
        assert_eq!(
            service::kernel_link_of(&json!({ "devices": { "kernel": changed } })),
            None,
            "{key}={value}"
        );
    }
    let mut extra = device.clone();
    extra["security.uid"] = json!("0");
    assert_eq!(
        service::kernel_link_of(&json!({ "devices": { "kernel": extra } })),
        None
    );
}
