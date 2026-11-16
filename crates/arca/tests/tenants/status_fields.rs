//! Additive service observations and identity across the lifetime of a record.
use super::*;
use arca::desired_store::{DesiredStore, EntryChange, STATE_FILE};

fn setup() -> Rig {
    let rig = two_tenants(policy());
    rig.template(
        "data-app",
        json!({"name": "data-app", "kind": "oci", "image": "registry.example/app",
        "digest": DIGEST, "ports": [], "env": {}, "limits": {"cpu": "2", "memory": "1GiB"},
        "data": {"path": "/data", "size": "1GiB"}, "publishable": false}),
    );
    rig.call(
        "service_create",
        json!({"tenant": A, "name": "box", "template": "data-app"}),
    )
    .unwrap();
    rig
}

fn observed(rig: &Rig) -> Value {
    let row = rig
        .call("service_status", json!({"tenant": A, "name": "box"}))
        .unwrap();
    arca_api::conforms(&(arca_api::verb("service_status").unwrap().result)(), &row).unwrap();
    row
}

fn record(rig: &Rig) -> arca_kernel::desired::Entry {
    rig.kernel
        .lock()
        .unwrap()
        .entries()
        .iter()
        .find(|entry| entry.id.0 == format!("t-{A}-service-box"))
        .unwrap()
        .clone()
}

fn assert_id(id: &Value) {
    let id = id.as_str().expect("a recorded service has an id");
    assert_eq!(id.len(), 36);
    assert!(id.starts_with("svc_"));
    assert!(
        id.as_bytes()[4..]
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
    );
}

#[test]
fn status_fields_use_recorded_identity_limits_volume_and_live_start_without_writing() {
    let rig = setup();
    let entry = record(&rig);
    assert_id(&entry.config["id"]);
    let uuid = rig
        .kernel
        .lock()
        .unwrap()
        .service_uuid(&entry.id, &format!("box-{A}"));
    rig.fake().set_last_used(
        &project(A),
        &format!("box-{A}"),
        "2026-11-01T12:34:56.123456789Z",
    );
    let state = fs::read(rig.dir.join(STATE_FILE)).unwrap();
    let journal = fs::read(rig.dir.join(arca::kernel::JOURNAL_FILE)).unwrap();
    let row = observed(&rig);
    assert_eq!(row["id"], entry.config["id"]);
    assert_eq!(row["uuid"], json!(uuid));
    assert_eq!(row["createdAt"], arca::handlers::iso_utc(NOW));
    assert_eq!(row["startedAt"], "2026-11-01T12:34:56.123456789Z");
    assert_eq!(row["limits"], json!({"cpu": "2", "memory": "1GiB"}));
    assert_eq!(row["data"], json!({"volume": "box-data"}));
    assert!(row.get("health").is_none());
    assert!(row.get("labels").is_none());
    let list = rig.call("service_list", json!({"tenant": A})).unwrap();
    arca_api::conforms(&(arca_api::verb("service_list").unwrap().result)(), &list).unwrap();
    for key in ["id", "uuid", "createdAt", "startedAt", "limits", "data"] {
        assert_eq!(list["services"][0][key], row[key], "{key}");
    }
    assert_eq!(fs::read(rig.dir.join(STATE_FILE)).unwrap(), state);
    assert_eq!(
        fs::read(rig.dir.join(arca::kernel::JOURNAL_FILE)).unwrap(),
        journal
    );
    let rig = rig.restart();
    assert_eq!(observed(&rig)["id"], row["id"]);
    rig.fake().set_instance_config(
        &project(A),
        &format!("box-{A}"),
        "volatile.uuid",
        "unrecorded-uuid",
    );
    assert_eq!(
        observed(&rig)["uuid"],
        row["uuid"],
        "journal identity, never a fresh listing's uuid"
    );
    assert_eq!(observed(&rig)["id"], row["id"]);
}

/// Install a pre-upgrade record through the generic store while the kernel is stopped.
fn legacy(mut rig: Rig) -> Rig {
    let dir = rig.dir.clone();
    let fake = rig.fake.take().unwrap();
    let clock = Arc::clone(&rig.clock);
    let kernel_config = rig.kernel_config.clone();
    let node = rig.node.clone();
    let mut entry = record(&rig);
    entry.config.as_object_mut().unwrap().remove("id");
    rig.handlers.clear();
    let old = std::mem::replace(&mut rig.kernel, Arc::new(Mutex::new(placeholder_kernel())));
    drop(old);
    drop(rig);
    let key = NodeKey::load_or_create(&dir.join("node.key")).unwrap();
    let mut store = DesiredStore::open(&dir, key).unwrap();
    store.propose(vec![EntryChange::Put(entry)], NOW).unwrap();
    drop(store);
    let socket = fake.socket().to_path_buf();
    Rig::open(
        dir,
        socket,
        Some(fake),
        kernel_config,
        node,
        common::SkKey::from_seed(9),
        clock,
    )
}

#[test]
fn legacy_status_id_is_read_only_stable_and_persisted_only_on_its_next_normal_write() {
    let rig = legacy(setup());
    assert!(record(&rig).config.get("id").is_none());
    let state = fs::read(rig.dir.join(STATE_FILE)).unwrap();
    let journal = fs::read(rig.dir.join(arca::kernel::JOURNAL_FILE)).unwrap();
    let id = observed(&rig)["id"].clone();
    assert_id(&id);
    assert_eq!(observed(&rig)["id"], id);
    assert_eq!(fs::read(rig.dir.join(STATE_FILE)).unwrap(), state);
    assert_eq!(
        fs::read(rig.dir.join(arca::kernel::JOURNAL_FILE)).unwrap(),
        journal
    );
    let rig = rig.restart();
    assert_eq!(observed(&rig)["id"], id);
    assert!(record(&rig).config.get("id").is_none());
    let unrelated = rig
        .kernel
        .lock()
        .unwrap()
        .entries()
        .iter()
        .find(|e| e.plugin.0 == "arca.tenant")
        .unwrap()
        .clone();
    rig.kernel
        .lock()
        .unwrap()
        .propose(vec![EntryChange::Put(unrelated)], NOW)
        .unwrap();
    assert!(
        record(&rig).config.get("id").is_none(),
        "another record's write does not migrate this one"
    );
    let mut entry = record(&rig);
    entry.config["follow"] = json!("none");
    rig.kernel
        .lock()
        .unwrap()
        .propose(vec![EntryChange::Put(entry)], NOW)
        .unwrap();
    assert_eq!(record(&rig).config["id"], id);
    assert_eq!(observed(&rig)["id"], id);
    let rig = rig.restart();
    assert_eq!(observed(&rig)["id"], id);
    rig.approved("service_remove", json!({"tenant": A, "name": "box"}))
        .unwrap();
    rig.call(
        "service_create",
        json!({"tenant": A, "name": "box", "template": "data-app"}),
    )
    .unwrap();
    let new_id = observed(&rig)["id"].clone();
    assert_id(&new_id);
    assert_ne!(
        new_id, id,
        "a recreate at the same clock time has a fresh random identity"
    );
}

#[test]
fn status_fields_never_started_missing_and_unknown_instances_have_explicit_nulls() {
    let rig = setup();
    for time in ["0001-01-01T00:00:00Z", "", "not-a-time"] {
        rig.fake()
            .set_last_used(&project(A), &format!("box-{A}"), time);
        let row = observed(&rig);
        assert!(row.get("startedAt").unwrap().is_null(), "{time}");
    }
    let row = json!({"name": "unknown", "config": {}, "devices": {}});
    for view in [
        arca::service::unknown_view(&row).unwrap(),
        arca::service::failed_view(&row).unwrap(),
    ] {
        for key in ["id", "uuid", "createdAt", "startedAt", "limits", "data"] {
            assert!(view.get(key).unwrap().is_null(), "{key}");
        }
    }
}
