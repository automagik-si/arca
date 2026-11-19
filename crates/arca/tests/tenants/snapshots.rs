use super::*;
use arca::desired_store::EntryChange;
use arca::handlers::service_entry;
use arca_incus::{InstanceState, StateChanger};
use arca_reconcile::Outcome;

const SNAPSHOTS: &str = "restricted.snapshots";
const VOLUME_PATH: &str = "/1.0/storage-pools/pool1/volumes/custom/box-data/snapshots/before";

fn put_tenant(rig: &Rig, id: &str, config: Value) {
    let entry_id = format!("tenant-{id}");
    let entry = service_entry(&entry_id, "arca.tenant", config);
    let outcomes = rig
        .kernel
        .lock()
        .expect("kernel")
        .propose(vec![EntryChange::Put(entry)], rig.now())
        .expect("the tenant owner entry was saved");
    assert!(
        outcomes.iter().any(|outcome| matches!(
            outcome,
            Outcome::Applied { entry } if entry.0 == entry_id
        )),
        "{outcomes:?}"
    );
}

fn project_config(rig: &Rig, id: &str) -> Value {
    let project = project(id);
    rig.get(&format!("/1.0/projects/{project}?project={project}"))
        .expect("the tenant project")["config"]
        .clone()
}

fn data_service(rig: &Rig, id: &str) -> (StateChanger, InstanceState) {
    let mut template = app_template("app");
    template["data"] = json!({ "path": "/data", "size": "1GiB" });
    rig.template("app", template);
    rig.call(
        "service_create",
        json!({ "tenant": id, "name": "box", "template": "app" }),
    )
    .expect("a service with its own data volume");
    let project = project(id);
    let changer = StateChanger::new(
        rig.socket.clone(),
        ProjectFamily::new([PROJECT], std::iter::empty::<&str>()).expect("family"),
    )
    .with_tenants([project.as_str()])
    .expect("tenant project");
    let seen = changer
        .read(&project, &format!("box-{id}"))
        .expect("read the instance");
    (changer, seen)
}

#[test]
fn tenant_snapshot_step_on_create_allows_a_volume_snapshot_in_its_project() {
    let rig = Rig::new(Some(policy()));
    rig.create_tenant(json!({ "id": B }))
        .expect("default tenant");
    let other = project_config(&rig, B);
    put_tenant(
        &rig,
        A,
        json!({
            "id": A, "project": project(A), "bridge": arca_netproject::BRIDGE,
            "cidr": CIDR, "block": "192.0.2.16/29", "pool": "pool1",
            "caps": policy()["defaultCaps"], "created": rig.now(), "snapshots": true
        }),
    );
    let config = project_config(&rig, A);
    assert_eq!(config[SNAPSHOTS], "allow");
    assert_eq!(config["restricted"], "true");
    assert_eq!(project_config(&rig, B), other);

    let (changer, seen) = data_service(&rig, A);
    let mut foreign = seen.clone();
    foreign.own_config.insert(
        arca_kernel::ownership::KERNEL_ENTRY_KEY.into(),
        format!("t-{B}-service-box"),
    );
    let requests = rig.fake().requests().len();
    assert!(
        changer
            .snapshot_volume(&foreign, "pool1", "box-data", "before")
            .is_err()
    );
    assert!(changer.snapshot_with_volumes(&seen, "before").is_err());
    assert_eq!(rig.fake().requests().len(), requests);
    changer
        .snapshot_volume(&seen, "pool1", "box-data", "before")
        .expect("snapshot the tenant's data volume");
    assert!(
        rig.get(&format!("{VOLUME_PATH}?project={}", project(A)))
            .is_some()
    );
}

#[test]
fn tenant_snapshot_step_on_patch_allows_a_volume_snapshot_that_was_blocked() {
    let rig = Rig::new(Some(policy()));
    rig.create_tenant(json!({ "id": A }))
        .expect("default tenant");
    let mut config = rig.entry(&format!("tenant-{A}"));
    assert!(
        config.get("snapshots").is_none(),
        "legacy owner entry shape"
    );
    let mut project_before = project_config(&rig, A);
    assert!(project_before.get(SNAPSHOTS).is_none());

    config["snapshots"] = json!(false);
    put_tenant(&rig, A, config.clone());
    assert_eq!(project_config(&rig, A), project_before);

    let (changer, seen) = data_service(&rig, A);
    let blocked = changer
        .snapshot_volume(&seen, "pool1", "box-data", "before")
        .expect_err("Incus refuses snapshots by default");
    assert!(
        blocked
            .to_string()
            .contains("doesn't allow for snapshot creation"),
        "{blocked}"
    );
    let url = format!("{VOLUME_PATH}?project={}", project(A));
    assert!(rig.get(&url).is_none());

    config["snapshots"] = json!(true);
    put_tenant(&rig, A, config);
    project_before[SNAPSHOTS] = json!("allow");
    assert_eq!(project_config(&rig, A), project_before);
    changer
        .snapshot_volume(&seen, "pool1", "box-data", "before")
        .expect("snapshot the existing tenant's data volume");
    assert!(rig.get(&url).is_some());
}
