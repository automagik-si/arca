//! A tenant's container keeps `/run` in memory as a service of the node's own project does: its project refuses
//! low-level config and every disk not from a pool, and the memory `/run` needs neither, since Incus mounts it once the
//! image's `/run` is emptied on the root disk. Every value is synthetic.
use super::*;

#[test]
fn a_tenants_service_gets_a_memory_run_in_its_restricted_project() {
    let rig = Rig::new(Some(policy()));
    rig.template("app", app_template("app"));
    rig.create_tenant(json!({ "id": A })).expect("tenant");
    rig.fake().set_image_run(&["/run/lock/", "/run/lock/.keep"]);
    let made = rig
        .call(
            "service_create",
            json!({ "name": "web", "template": "app", "tenant": A }),
        )
        .expect("create");
    assert_eq!(made["run"], "memory", "{made}");
    let project = project(A);
    let tenant_project = rig
        .get(&format!("/1.0/projects/{project}"))
        .expect("the tenant's project");
    assert_eq!(tenant_project["config"]["restricted"], "true");
    assert_eq!(
        tenant_project["config"]["restricted.containers.lowlevel"],
        "block"
    );
    assert_eq!(
        tenant_project["config"]["restricted.devices.disk"],
        "managed"
    );
    let instance = format!("web-{A}");
    let found = rig
        .get(&format!("/1.0/instances/{instance}?project={project}"))
        .expect("the instance");
    assert_eq!(found["config"][arca::service::RUN_TMPFS_KEY], "true");
    assert!(found["config"].get("raw.lxc").is_none(), "{found}");
    assert!(
        found["devices"]
            .as_object()
            .expect("devices")
            .values()
            .all(|device| device["path"] != "/run"),
        "no device is mounted on /run: {found}"
    );
    assert!(rig.fake().run_on_disk(&project, &instance).is_empty());
    assert!(rig.fake().run_in_memory(&project, &instance));
}
