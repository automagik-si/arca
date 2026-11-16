//! Container disk sizes, NIC isolation and the front's access to declared ports.
use super::*;

fn create(rig: &Rig, name: &str) -> Result<Value, CallError> {
    rig.call(
        "service_create",
        json!({ "name": name, "template": "app", "tenant": A }),
    )
}

fn instance(rig: &Rig, name: &str) -> Value {
    rig.get(&format!("/1.0/instances/{name}-{A}?project={}", project(A)))
        .expect("instance")
}

#[test]
fn container_options_root_and_data_count_at_create_and_from_the_saved_record() {
    const GIB: u64 = 1 << 30;
    let mut small = policy();
    small["defaultCaps"]["disk"] = json!("50GiB");
    let rig = Rig::new(Some(small));
    rig.create_tenant(json!({ "id": A })).expect("tenant");
    let mut template = app_template("app");
    template["rootSize"] = json!("30GiB");
    template["data"] = json!({ "path": "/data", "size": "10GiB", "uid": 1000, "gid": 1000 });
    rig.template("app", template);
    create(&rig, "one").expect("custom root fits with data");
    assert_eq!(instance(&rig, "one")["devices"]["root"]["size"], "30GiB");
    assert_eq!(rig.entry("t-aaaaaaaa-service-one")["rootSize"], "30GiB");
    let usage = rig.call("tenant_usage", json!({ "id": A })).expect("usage");
    assert_eq!(usage["instances"][0]["rootDiskSize"], "30GiB");
    // Changing the template cannot change what an existing service is charged.
    rig.template("app", app_template("app"));
    let rig = rig.restart();
    let writes = rig.fake().writes().len();
    let (code, _, fields) = listed(create(&rig, "two"));
    assert_eq!(code, "capacity");
    assert_eq!(
        fields,
        json!({ "scope": "tenant", "limit": "disk", "have": 40 * GIB,
        "want": 16 * GIB, "max": 50 * GIB })
    );
    assert_eq!(
        rig.fake().writes().len(),
        writes,
        "refused before Incus writes"
    );
    assert!(
        rig.get(&format!("/1.0/instances/two-{A}?project={}", project(A)))
            .is_none()
    );
}

#[test]
fn container_options_an_over_cap_root_is_a_capacity_refusal_before_writes() {
    const GIB: u64 = 1 << 30;
    let rig = Rig::new(Some(policy()));
    rig.create_tenant(json!({ "id": A })).expect("tenant");
    let mut template = app_template("app");
    template["rootSize"] = json!("101GiB");
    rig.template("app", template);
    let writes = rig.fake().writes().len();
    let (code, _, fields) = listed(create(&rig, "big"));
    assert_eq!(code, "capacity");
    assert_eq!(
        fields,
        json!({ "scope": "tenant", "limit": "disk", "have": 0,
        "want": 101 * GIB, "max": 100 * GIB })
    );
    assert_eq!(rig.fake().writes().len(), writes);
}

#[test]
fn container_options_isolation_survives_a_restart_and_an_approved_reconfigure() {
    let rig = Rig::new(Some(policy()));
    rig.create_tenant(json!({ "id": A })).expect("tenant");
    let mut template = app_template("app");
    template["portIsolation"] = json!(true);
    template["rootSize"] = json!("30GiB");
    rig.template("app", template.clone());
    create(&rig, "one").expect("isolated container");
    assert_eq!(
        instance(&rig, "one")["devices"]["eth0"]["security.port_isolation"],
        "true"
    );
    let rig = rig.restart();
    template.as_object_mut().unwrap().remove("portIsolation");
    template.as_object_mut().unwrap().remove("rootSize");
    template["env"] = json!({ "MODE": "two" });
    rig.template("app", template);
    rig.approved("service_reconfigure", json!({ "name": "one", "tenant": A }))
        .expect("reconfigured");
    assert_eq!(
        instance(&rig, "one")["devices"]["eth0"]["security.port_isolation"],
        "true"
    );
    assert_eq!(rig.entry("t-aaaaaaaa-service-one")["portIsolation"], true);
    assert_eq!(instance(&rig, "one")["devices"]["root"]["size"], "30GiB");
}

#[test]
fn container_options_the_front_never_reaches_the_control_port() {
    let mut fronted = policy();
    fronted["front"] = json!("edge");
    let rig = Rig::new(Some(fronted));
    rig.template("app", app_template("app"));
    let edge = rig
        .call(
            "service_create",
            json!({ "name": "edge", "template": "app" }),
        )
        .expect("front")["ipv4"]
        .as_str()
        .unwrap()
        .to_string();
    rig.create_tenant(json!({ "id": A })).expect("tenant");
    for (name, ports, expected) in [
        ("mixed", vec![8080, 9090], Some("9090")),
        ("control", vec![8080], None),
    ] {
        let mut template = app_template("app");
        template["droiddPort"] = json!(8080);
        template["ports"] = json!(ports);
        rig.template("app", template);
        create(&rig, name).expect("container");
        let acl = rig
            .get(&format!(
                "/1.0/network-acls/{}-svc-{name}?project=default",
                project(A)
            ))
            .expect("ACL");
        let rules = acl["ingress"].as_array().expect("ingress");
        let front: Vec<_> = rules
            .iter()
            .filter(|rule| rule["source"] == format!("{edge}/32"))
            .collect();
        assert_eq!(front.len(), usize::from(expected.is_some()));
        if let Some(ports) = expected {
            assert_eq!(front[0]["destination_port"], ports);
        }
        assert!(
            rules
                .iter()
                .any(|rule| rule["source"] == "192.0.2.1/32" && rule["destination_port"] == "8080"),
            "the host retains its control access: {acl}"
        );
    }
}
