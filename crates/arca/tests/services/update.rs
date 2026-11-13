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
        "lan": { "port": 8080 },
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
    let rig = bare_node(up, None);
    rig.template("app", app_template());
    rig.call(
        "service_create",
        json!({ "name": "app", "template": "app" }),
    )
    .expect("create");
    rig
}

/// A node with no service yet, whose update handler answers its health check as `up` says. `key` is the owner's.
fn bare_node(up: &Arc<AtomicBool>, key: Option<&common::SkKey>) -> Rig {
    let mut rig = Rig::with_key(
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![],
            networks: vec![],
            pools: vec![("pool1".into(), json!({}))],
        },
        vec![PROJECT.to_string()],
        Some("192.0.2.0/24"),
        key,
    );
    let answers = Arc::clone(up);
    rig.handlers = service_handlers_updating(
        rig.kernel.clone(),
        Arc::new(|| NOW),
        rig.cfg.clone(),
        None,
        UpdateOptions {
            probe: Arc::new(move |_, _, _| answers.load(Ordering::SeqCst)),
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
    alias_of(rig, "app")
}

fn alias_of(rig: &Rig, service: &str) -> String {
    let record = rig
        .entries()
        .into_iter()
        .find(|entry| entry.id.0 == format!("service-{service}"))
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

#[test]
fn the_health_check_goes_through_the_host_port_proxy_on_loopback() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut rig = bare_rig(&Arc::new(AtomicBool::new(true)));
    let log = Arc::clone(&seen);
    rig.handlers = service_handlers_updating(
        rig.kernel.clone(),
        Arc::new(|| NOW),
        rig.cfg.clone(),
        None,
        UpdateOptions {
            probe: Arc::new(move |address, port, forwarded| {
                log.lock()
                    .unwrap()
                    .push((address.to_string(), port, forwarded));
                true
            }),
            pace: Duration::from_millis(10),
        },
    )
    .into_iter()
    .collect();
    update(&rig, json!({ "name": "app", "digest": NEW })).expect("update");
    assert_eq!(
        *seen.lock().unwrap(),
        vec![("127.0.0.1".to_string(), 8080, true)],
        "the node's own loopback and the proxy's port, never the service's address"
    );
}

#[test]
fn a_service_never_started_is_refused_before_anything_is_changed() {
    let up = Arc::new(AtomicBool::new(true));
    let rig = bare_rig(&up);
    rig.call(
        "service_create",
        json!({ "name": "idle", "template": "app", "start": false }),
    )
    .expect("create stopped");
    let CallError::Refused { message, .. } =
        update(&rig, json!({ "name": "idle", "digest": NEW })).expect_err("never started")
    else {
        panic!("a refusal");
    };
    assert!(message.contains("never been started"), "{message}");
    assert!(rig.fake.rebuilds(PROJECT, "idle").is_empty());
}

#[test]
fn a_registry_that_is_down_leaves_the_old_root_and_the_old_version_runs_again() {
    let up = Arc::new(AtomicBool::new(true));
    let rig = bare_rig(&up);
    rig.fake.refuse_rebuild(PROJECT, "app");
    let CallError::Refused { code, message } =
        update(&rig, json!({ "name": "app", "digest": NEW })).expect_err("rebuild fails")
    else {
        panic!("a refusal");
    };
    assert_eq!(code, "rolled_back");
    assert!(message.contains("left as it was"), "{message}");
    assert_eq!(instance(&rig)["status"], "Running");
}

/// The three shipped templates that name a data volume, as synthetic data: the same ports, the same data paths.
fn shipped_shapes() -> Vec<Value> {
    let shape = |name: &str, ports: Value, path: &str, extra: Value| {
        let mut template = json!({
            "name": name,
            "kind": "oci",
            "image": format!("registry.example/{name}"),
            "digest": DIGEST,
            "ports": ports,
            "data": { "path": path, "uid": 1000, "gid": 1000, "size": "10GiB" },
            "env": {},
            "publishable": false
        });
        for (key, value) in extra.as_object().into_iter().flatten() {
            template[key] = value.clone();
        }
        template
    };
    vec![
        shape(
            "postgres-autopg",
            json!([5432]),
            "/var/lib/autopg/data",
            json!({}),
        ),
        shape("nats", json!([4222]), "/data", json!({})),
        shape(
            "omni-api",
            json!([8882]),
            "/app/data",
            json!({ "droiddPort": 8882 }),
        ),
    ]
}

fn without_data(template: &Value) -> Value {
    let mut template = template.clone();
    template.as_object_mut().expect("an object").remove("data");
    template
}

fn instance_named(rig: &Rig, service: &str) -> Value {
    rig.get(&format!("/1.0/instances/{service}?project={PROJECT}"))
        .expect("instance")
}

/// A node holding `service`, made from `template` before the template named a data volume: it keeps its data on its
/// root disk. The template installed now declares the volume.
fn old_style_rig(template: &Value, service: &str, key: Option<&common::SkKey>) -> Rig {
    let rig = bare_node(&Arc::new(AtomicBool::new(true)), key);
    let name = template["name"].as_str().expect("a name");
    rig.template(name, without_data(template));
    rig.call(
        "service_create",
        json!({ "name": service, "template": name }),
    )
    .expect("create");
    assert!(
        instance_named(&rig, service)["devices"]
            .get("data")
            .is_none(),
        "an old-style service has no data volume"
    );
    rig.template(name, template.clone());
    rig
}

/// A node holding `service` made from `template` as it stands.
fn rig_with(template: &Value, service: &str, key: Option<&common::SkKey>) -> Rig {
    let rig = bare_node(&Arc::new(AtomicBool::new(true)), key);
    rig.template(template["name"].as_str().expect("a name"), template.clone());
    rig.call(
        "service_create",
        json!({ "name": service, "template": template["name"] }),
    )
    .expect("create");
    rig
}

fn refusal_words(result: Result<Value, CallError>) -> (String, String) {
    match result.expect_err("a refusal") {
        CallError::Refused { code, message } => (code, message),
        other => panic!("a refusal was expected, got {other:?}"),
    }
}

fn plan_handlers(rig: &Rig) -> HashMap<&'static str, Arc<dyn ToolHandler>> {
    arca::handlers::plan_handlers_served(
        rig.kernel.clone(),
        Arc::new(|| NOW),
        None,
        None,
        None,
        None,
        arca::handlers::ActionSlot::default(),
        Some(rig.cfg.clone()),
    )
    .into_iter()
    .collect()
}

/// `action_propose` for a `service_update`.
fn propose_update(rig: &Rig, args: Value) -> Result<Value, CallError> {
    plan_handlers(rig)["action_propose"].call(
        json!({ "action": "service_update", "args": args }),
        &doctor(),
    )
}

/// The owner approves the proposal `id`, and the approved action runs through the node's own handlers.
fn approve_and_run(
    rig: &Rig,
    key: &common::SkKey,
    id: &str,
    digest: &str,
) -> Result<Value, CallError> {
    let statement = arca::handlers::action_statement(
        "node1",
        id,
        "service_update",
        digest,
        NOW,
        &"5".repeat(32),
    )
    .expect("the statement");
    let approval = common::approve_statement(key, &statement);
    let approved = {
        let mut kernel = rig.kernel.lock().expect("kernel");
        arca::handlers::approve_action(
            &mut kernel,
            &json!({ "plan": id, "approval": approval }),
            NOW,
        )
        .expect("the owner's approval verifies")
    };
    arca::handlers::ActionExecutor::new(
        rig.handlers
            .iter()
            .map(|(verb, handler)| (*verb, Arc::clone(handler))),
    )
    .run(&approved.action, &approved.args, &approved.grant)
}

#[test]
fn a_service_that_keeps_its_data_inside_the_container_is_refused_before_any_write() {
    for template in shipped_shapes() {
        let name = template["name"].as_str().expect("a name").to_string();
        let service = format!("old-{name}");
        let rig = old_style_rig(&template, &service, None);
        let before = instance_named(&rig, &service);
        let record = alias_of(&rig, &service);
        let writes = rig.fake.writes().len();

        let (code, message) = refusal_words(update(
            &rig,
            json!({ "name": service, "digest": NEW, "timeoutSeconds": 1 }),
        ));
        assert_eq!(code, "refused", "{name}");
        for wanted in [
            "would erase the service's data",
            "keeps its data inside the container",
            service.as_str(),
            name.as_str(),
            "eraseData",
            "Nothing was changed",
        ] {
            assert!(message.contains(wanted), "{name}: {wanted}: {message}");
        }

        // Nothing was written: not to Incus, not to the journal, not to the record.
        assert_eq!(rig.fake.writes().len(), writes, "{name}");
        assert!(rig.fake.rebuilds(PROJECT, &service).is_empty(), "{name}");
        assert_eq!(instance_named(&rig, &service), before, "{name}");
        assert_eq!(alias_of(&rig, &service), record, "{name}");
        assert!(!rig.dir.join("service-updates.jsonl").exists(), "{name}");
    }
}

#[test]
fn a_proposed_update_of_such_a_service_is_refused_when_it_is_made_and_leaves_no_card() {
    let template = shipped_shapes().remove(0);
    let rig = old_style_rig(&template, "old-db", None);
    let writes = rig.fake.writes().len();

    let (_, message) = refusal_words(propose_update(
        &rig,
        json!({ "name": "old-db", "digest": NEW }),
    ));
    assert!(
        message.contains("would erase the service's data"),
        "{message}"
    );
    assert!(message.contains("postgres-autopg"), "{message}");
    assert_eq!(
        rig.kernel
            .lock()
            .expect("kernel")
            .action_proposals()
            .iter()
            .count(),
        0,
        "no card was recorded"
    );
    assert_eq!(rig.fake.writes().len(), writes);
}

#[test]
fn an_approved_update_of_such_a_service_is_refused_when_it_runs_and_writes_nothing() {
    let key = common::SkKey::from_seed(11);
    let template = shipped_shapes().remove(1);
    let rig = old_style_rig(&template, "old-nats", Some(&key));
    // A card made before the node refused it when proposed.
    let proposal = rig
        .kernel
        .lock()
        .expect("kernel")
        .propose_action(
            arca::action_proposal::ActionKind::ServiceUpdate,
            json!({ "name": "old-nats", "digest": NEW }),
            NOW,
            None,
        )
        .expect("the proposal");
    let writes = rig.fake.writes().len();

    let (_, message) = refusal_words(approve_and_run(
        &rig,
        &key,
        &proposal.id,
        &proposal.args_digest,
    ));
    assert!(
        message.contains("would erase the service's data"),
        "{message}"
    );
    assert_eq!(rig.fake.writes().len(), writes);
    assert!(rig.fake.rebuilds(PROJECT, "old-nats").is_empty());
}

#[test]
fn erase_data_is_refused_without_the_owners_approval_before_any_write() {
    let template = shipped_shapes().remove(2);
    let rig = old_style_rig(&template, "old-omni", None);
    let writes = rig.fake.writes().len();

    let (_, message) = refusal_words(update(
        &rig,
        json!({ "name": "old-omni", "digest": NEW, "eraseData": true }),
    ));
    assert!(message.contains("owner's approval"), "{message}");
    assert!(message.contains("Nothing was changed"), "{message}");
    assert_eq!(rig.fake.writes().len(), writes);
    assert!(rig.fake.rebuilds(PROJECT, "old-omni").is_empty());
    assert!(!rig.dir.join("service-updates.jsonl").exists());

    // A grant that is not one is no approval either.
    let (_, message) = refusal_words(update(
        &rig,
        json!({ "name": "old-omni", "digest": NEW, "eraseData": true, "approval": "forged" }),
    ));
    assert!(message.contains("owner's approval"), "{message}");
    assert_eq!(rig.fake.writes().len(), writes);
}

#[test]
fn an_approved_erase_data_update_runs_and_its_card_says_in_words_that_it_erases_the_data() {
    let key = common::SkKey::from_seed(12);
    for template in shipped_shapes() {
        let name = template["name"].as_str().expect("a name").to_string();
        let service = format!("old-{name}");
        let rig = old_style_rig(&template, &service, Some(&key));
        let args = json!({
            "name": service, "digest": NEW, "timeoutSeconds": 1, "eraseData": true
        });

        let proposed = propose_update(&rig, args.clone()).expect("the proposal is recorded");
        assert!(
            proposed["summary"]
                .as_str()
                .expect("a summary")
                .contains("ERASE ALL OF ITS DATA"),
            "{name}: {proposed}"
        );
        assert!(
            proposed["details"]
                .as_array()
                .expect("details")
                .contains(&json!({
                    "label": "Erase data",
                    "value": arca::action_proposal::ERASE_DATA_WORDS
                })),
            "{name}: {proposed}"
        );
        assert!(
            proposed["undo"]
                .as_str()
                .expect("undo")
                .contains("cannot be undone"),
            "{name}: {proposed}"
        );
        assert!(
            rig.fake.rebuilds(PROJECT, &service).is_empty(),
            "nothing runs until it is approved"
        );

        let id = proposed["id"].as_str().expect("an id");
        let digest = proposed["argsDigest"].as_str().expect("a digest");
        let done = approve_and_run(&rig, &key, id, digest).expect("the approved update runs");
        assert_eq!(done["updated"], true, "{name}: {done}");
        assert_eq!(
            rig.fake.rebuilds(PROJECT, &service),
            vec!["registry"],
            "{name}"
        );
        assert_eq!(alias_of(&rig, &service), format!("{name}@{NEW}"));
    }
}

#[test]
fn a_service_that_has_its_data_volume_updates_as_before() {
    for template in shipped_shapes() {
        let name = template["name"].as_str().expect("a name").to_string();
        let service = format!("kept-{name}");
        let rig = rig_with(&template, &service, None);
        let before = instance_named(&rig, &service);
        assert!(before["devices"].get("data").is_some(), "{name}");

        let done = update(
            &rig,
            json!({ "name": service, "digest": NEW, "timeoutSeconds": 1 }),
        )
        .expect("the update runs");
        assert_eq!(done["updated"], true, "{name}: {done}");
        assert_eq!(
            rig.fake.rebuilds(PROJECT, &service),
            vec!["registry"],
            "{name}"
        );
        assert_eq!(
            instance_named(&rig, &service)["devices"]["data"],
            before["devices"]["data"],
            "{name}: the volume is kept"
        );

        // Its proposal is made as before, with no word of erasing.
        let proposed = propose_update(
            &rig,
            json!({ "name": service, "digest": format!("sha256:{}", "3".repeat(64)) }),
        )
        .expect("the proposal is recorded");
        assert!(
            !proposed["summary"]
                .as_str()
                .expect("summary")
                .contains("ERASE")
        );
        assert!(
            !proposed["details"]
                .as_array()
                .expect("details")
                .iter()
                .any(|row| row["label"] == "Erase data")
        );
    }
}

#[test]
fn a_template_that_declares_no_data_volume_updates_as_before() {
    for template in shipped_shapes() {
        let template = without_data(&template);
        let name = template["name"].as_str().expect("a name").to_string();
        let service = format!("plain-{name}");
        let rig = rig_with(&template, &service, None);

        let done = update(
            &rig,
            json!({ "name": service, "digest": NEW, "timeoutSeconds": 1 }),
        )
        .expect("the update runs");
        assert_eq!(done["updated"], true, "{name}: {done}");
        assert_eq!(
            rig.fake.rebuilds(PROJECT, &service),
            vec!["registry"],
            "{name}"
        );
        propose_update(
            &rig,
            json!({ "name": service, "digest": format!("sha256:{}", "3".repeat(64)) }),
        )
        .expect("the proposal is recorded");
    }
}
