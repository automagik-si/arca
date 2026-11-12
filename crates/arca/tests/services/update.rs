//! `service_update` against a fake Incus: a running service moves to a new digest, and a failure of the health check
//! or of the rebuild puts the old version back. Every value is synthetic.
use super::*;
use arca::handlers::{UpdateOptions, service_handlers_updating};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

const NEW: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";

fn app_template() -> Value {
    json!({
        "name": "app",
        "kind": "oci",
        "image": "registry.example/app",
        "digest": DIGEST,
        "ports": [8080],
        "data": { "path": "/data", "uid": 1000, "gid": 1000 },
        "env": {},
        "publishable": false
    })
}

/// A rig with the service `app` running, a project that allows snapshots, and a health check that answers as `up` says.
fn rig(up: &Arc<AtomicBool>) -> Rig {
    let rig = bare_rig(up);
    let path = format!("/1.0/projects/{PROJECT}");
    let mut config = rig.get(&path).expect("the project")["config"].clone();
    config["restricted.snapshots"] = json!("allow");
    rig.edit("PATCH", &path, json!({ "config": config }));
    rig
}

/// The same, in a project as Arca makes it: restricted, so Incus refuses a snapshot there.
fn bare_rig(up: &Arc<AtomicBool>) -> Rig {
    let mut rig = Rig::with(
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![],
            networks: vec![],
            pools: vec![("pool1".into(), json!({}))],
        },
        vec![PROJECT.to_string()],
        Some("192.0.2.0/24"),
    );
    rig.template("app", app_template());
    rig.call(
        "service_create",
        json!({ "name": "app", "template": "app" }),
    )
    .expect("create");
    let answers = Arc::clone(up);
    rig.handlers = service_handlers_updating(
        rig.kernel.clone(),
        Arc::new(|| NOW),
        rig.cfg.clone(),
        None,
        UpdateOptions {
            probe: Arc::new(move |_, _| answers.load(Ordering::SeqCst)),
            pace: Duration::from_millis(10),
        },
    )
    .into_iter()
    .collect();
    rig
}

fn update(rig: &Rig, args: Value) -> Result<Value, CallError> {
    rig.call("service_update", args)
}

fn instance(rig: &Rig) -> Value {
    rig.get(&format!("/1.0/instances/app?project={PROJECT}"))
        .expect("instance")
}

fn alias_of_record(rig: &Rig) -> String {
    let record = rig
        .entries()
        .into_iter()
        .find(|entry| entry.id.0 == "service-app")
        .expect("the record");
    record.config["source"]["alias"]
        .as_str()
        .expect("alias")
        .to_string()
}

/// The steps of the journal, in order, with whether each went well.
fn journal(rig: &Rig) -> Vec<(String, bool)> {
    fs::read_to_string(rig.dir.join("service-updates.jsonl"))
        .expect("the journal")
        .lines()
        .map(|line| {
            let row: Value = serde_json::from_str(line).expect("a JSON line");
            (
                row["step"].as_str().expect("step").to_string(),
                row["ok"].as_bool().expect("ok"),
            )
        })
        .collect()
}

#[test]
fn an_update_rebuilds_from_the_new_digest_keeps_the_data_and_records_the_digest() {
    let up = Arc::new(AtomicBool::new(true));
    let rig = rig(&up);
    let before = instance(&rig);

    let done = update(&rig, json!({ "name": "app", "digest": NEW })).expect("update");
    assert_eq!(done["updated"], true);
    assert_eq!(done["digest"], NEW);
    let snapshot = done["snapshot"].as_str().expect("a snapshot").to_string();
    assert!(snapshot.starts_with("pre-update-"));

    // The root was rebuilt once from the registry, and the instance runs again with what the rebuild keeps.
    assert_eq!(rig.fake.rebuilds(PROJECT, "app"), vec!["registry"]);
    assert!(rig.fake.restores(PROJECT, "app").is_empty());
    let after = instance(&rig);
    assert_eq!(after["status"], "Running");
    assert_eq!(after["devices"]["data"], before["devices"]["data"]);
    assert_eq!(after["devices"]["eth0"], before["devices"]["eth0"]);
    assert_eq!(
        after["config"]["volatile.uuid"],
        before["config"]["volatile.uuid"]
    );
    assert!(
        rig.get(&format!(
            "/1.0/instances/app/snapshots/{snapshot}?project={PROJECT}"
        ))
        .is_some(),
        "the snapshot of the old version is kept"
    );

    // The record names the new digest, so a restart keeps it, and the status says the service runs it.
    assert_eq!(alias_of_record(&rig), format!("app@{NEW}"));
    let status = rig
        .call("service_status", json!({ "name": "app" }))
        .expect("status");
    assert_eq!(
        status["update"]["pin"],
        format!("registry.example/app@{NEW}")
    );
    assert_eq!(status["state"], "running");

    // Every step is in the journal, in order.
    let steps: Vec<String> = journal(&rig).into_iter().map(|(step, _)| step).collect();
    assert_eq!(
        steps,
        [
            "begin", "stop", "snapshot", "rebuild", "start", "health", "record", "end"
        ]
    );

    // Asking for the digest it runs now changes nothing.
    let again = update(&rig, json!({ "name": "app", "digest": NEW })).expect("again");
    assert_eq!(again["updated"], false);
    assert_eq!(rig.fake.rebuilds(PROJECT, "app").len(), 1);
}

#[test]
fn a_port_that_never_answers_rolls_the_update_back_to_the_snapshot() {
    let up = Arc::new(AtomicBool::new(false));
    let rig = rig(&up);
    let old = alias_of_record(&rig);

    let refusal = update(
        &rig,
        json!({ "name": "app", "digest": NEW, "timeoutSeconds": 1 }),
    )
    .expect_err("the port never answers");
    let CallError::Refused { code, message } = refusal else {
        panic!("a refusal");
    };
    assert_eq!(code, "rolled_back");
    assert!(message.contains("rolled back"), "{message}");
    assert!(message.contains("health check"), "{message}");

    // The new root was built and started, then the snapshot was restored and the old version started.
    assert_eq!(rig.fake.rebuilds(PROJECT, "app"), vec!["registry"]);
    let restored = rig.fake.restores(PROJECT, "app");
    assert_eq!(restored.len(), 1);
    assert!(restored[0].starts_with("pre-update-"));
    assert_eq!(instance(&rig)["status"], "Running");
    assert_eq!(
        alias_of_record(&rig),
        old,
        "the record keeps the old digest"
    );
    let steps = journal(&rig);
    assert!(steps.contains(&("health check".to_string(), false)));
    assert!(steps.contains(&("restore".to_string(), true)));
    assert_eq!(steps.last(), Some(&("end".to_string(), false)));
}

#[test]
fn a_rebuild_that_fails_rolls_the_update_back_to_the_snapshot() {
    let up = Arc::new(AtomicBool::new(true));
    let rig = rig(&up);
    let old = alias_of_record(&rig);
    rig.fake.refuse_rebuild(PROJECT, "app");

    let refusal =
        update(&rig, json!({ "name": "app", "digest": NEW })).expect_err("the rebuild fails");
    let CallError::Refused { code, message } = refusal else {
        panic!("a refusal");
    };
    assert_eq!(code, "rolled_back");
    assert!(message.contains("rolled back"), "{message}");
    assert!(message.contains("rebuild"), "{message}");

    assert!(rig.fake.rebuilds(PROJECT, "app").is_empty());
    assert_eq!(rig.fake.restores(PROJECT, "app").len(), 1);
    assert_eq!(instance(&rig)["status"], "Running");
    assert_eq!(alias_of_record(&rig), old);
}

#[test]
fn a_call_that_cannot_run_changes_nothing() {
    let up = Arc::new(AtomicBool::new(true));
    let rig = rig(&up);
    let before = instance(&rig);

    for (args, wanted) in [
        (json!({ "name": "app", "digest": "sha256:short" }), "digest"),
        (
            json!({ "name": "app", "digest": NEW, "timeoutSeconds": 0 }),
            "timeoutSeconds",
        ),
        (
            json!({ "name": "app", "digest": NEW, "force": true }),
            "force",
        ),
    ] {
        let CallError::Invalid(message) = update(&rig, args).expect_err("invalid") else {
            panic!("an invalid call");
        };
        assert!(message.contains(wanted), "{message}");
    }
    let CallError::Refused { code, .. } =
        update(&rig, json!({ "name": "nobody", "digest": NEW })).expect_err("not found")
    else {
        panic!("a refusal");
    };
    assert_eq!(code, "not_found");
    let viewer = rig.handlers["service_update"].call(
        json!({ "name": "app", "digest": NEW }),
        &Caller {
            token_id: "viewer".into(),
            scope: Scope::Viewer,
            tools: None,
        },
    );
    assert!(matches!(viewer, Err(CallError::Forbidden { .. })));

    assert!(rig.fake.rebuilds(PROJECT, "app").is_empty());
    assert_eq!(instance(&rig), before);
    assert!(!rig.dir.join("service-updates.jsonl").exists());
}

#[test]
fn a_project_that_forbids_snapshots_is_rolled_back_by_a_rebuild_from_the_old_digest() {
    let up = Arc::new(AtomicBool::new(false));
    let rig = bare_rig(&up);
    let old = alias_of_record(&rig);

    let refusal = update(
        &rig,
        json!({ "name": "app", "digest": NEW, "timeoutSeconds": 1 }),
    )
    .expect_err("the port never answers");
    let CallError::Refused { code, message } = refusal else {
        panic!("a refusal");
    };
    assert_eq!(code, "rolled_back");
    assert!(message.contains("rolled back"), "{message}");
    // No snapshot to restore: the root was rebuilt from the new digest and then from the old one.
    assert_eq!(
        rig.fake.rebuilds(PROJECT, "app"),
        vec!["registry", "registry"]
    );
    assert!(rig.fake.restores(PROJECT, "app").is_empty());
    assert_eq!(instance(&rig)["status"], "Running");
    assert_eq!(alias_of_record(&rig), old);

    up.store(true, Ordering::SeqCst);
    let done = update(&rig, json!({ "name": "app", "digest": NEW })).expect("update");
    assert_eq!(done["updated"], true);
    assert_eq!(done["snapshot"], Value::Null);
}
