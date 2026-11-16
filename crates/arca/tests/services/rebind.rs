//! Rebinding uses observed identity and storage, and never edits the backup.
use super::*;
use arca::desired_store::EntryChange;

fn propose(rig: &Rig, args: Value) -> Result<Value, CallError> {
    let handlers: HashMap<_, _> = arca::handlers::plan_handlers_served(
        rig.kernel.clone(),
        Arc::new(|| NOW),
        None,
        None,
        None,
        None,
        Arc::default(),
        Some(rig.cfg.clone()),
    )
    .into_iter()
    .collect();
    handlers["action_propose"].call(json!({"action":"service_rebind", "args":args}), &doctor())
}
fn run(rig: &Rig, proposal: &Value) -> Result<Value, CallError> {
    let approved = rig
        .kernel
        .lock()
        .unwrap()
        .approve_action_in_terminal(proposal["id"].as_str().unwrap(), "test-admin", NOW, NOW)
        .unwrap();
    arca::handlers::ActionExecutor::new(rig.handlers.iter().map(|(k, v)| (*k, v.clone()))).run(
        &approved.action,
        &approved.args,
        &approved.grant,
    )
}
fn instance(rig: &Rig, name: &str) -> Value {
    rig.get(&format!("/1.0/instances/{name}?project={PROJECT}"))
        .unwrap()
}
fn volume_path(pool: &str) -> String {
    format!("/1.0/storage-pools/{pool}/volumes/custom/sample-data?project={PROJECT}")
}
fn journal_lines(rig: &Rig) -> usize {
    fs::read_to_string(rig.dir.join("journal.jsonl"))
        .unwrap()
        .lines()
        .count()
}
fn unchanged(rig: &Rig, journal: usize, writes: usize) {
    assert_eq!(journal_len(rig), journal);
    assert_eq!(rig.fake.writes().len(), writes);
}

fn reopen(rig: &Rig, snapshot: &std::path::Path) -> Result<Kernel, arca_kernel::KernelError> {
    let mut plugins = PluginSet::new();
    plugins.add::<arca_netproject::NetProject>().unwrap();
    plugins.add::<Service>().unwrap();
    let verifier = ApprovalVerifier::new(
        "node1".into(),
        OwnerKeys::new(
            SshSigVerifier::new(vec![]),
            PasskeyVerifier::refusing(arca_approval::ApprovalRefusal::NoVerifier),
        ),
        FileNonces::open(&snapshot.join("nonces")).unwrap(),
    );
    Kernel::open(
        snapshot,
        NodeKey::load_or_create(&rig.dir.join("node.key")).unwrap(),
        plugins,
        verifier,
        "node1".into(),
        None,
        |retained| {
            Ok(Box::new(
                IncusProvider::new(
                    rig.fake.socket().to_path_buf(),
                    Family {
                        projects: vec![PROJECT.into()],
                        scratch: false,
                        droidd_projects: vec!["other".into()],
                        reserved_cidrs: vec![],
                    },
                    rig.dir.join("secrets"),
                    retained,
                )
                .unwrap(),
            ))
        },
        NOW,
    )
}

fn remove_approved(rig: &Rig) -> Result<Value, CallError> {
    // Terminal approval yields the same one-shot grant the normal owner path uses.
    let proposal = rig
        .kernel
        .lock()
        .unwrap()
        .propose_action(
            arca::action_proposal::ActionKind::ServiceRemove,
            json!({"name":"sample"}),
            NOW,
            None,
        )
        .unwrap();
    let approved = rig
        .kernel
        .lock()
        .unwrap()
        .approve_action_in_terminal(&proposal.id, "test-admin", NOW, NOW)
        .unwrap();
    arca::handlers::ActionExecutor::new(rig.handlers.iter().map(|(k, v)| (*k, v.clone()))).run(
        &approved.action,
        &approved.args,
        &approved.grant,
    )
}

#[test]
fn approved_rebind_recovers_secrets_and_storage_without_touching_the_backups() {
    let rig = copied_service_binding_fixture();
    let backup = instance(&rig, "sample-backup");
    let old_volume = rig.get(&volume_path("pool1")).unwrap();
    let writes = rig.fake.writes().len();
    let journal = journal_lines(&rig);
    let proposal = propose(&rig, json!({"name":"sample"})).unwrap();
    assert_eq!(journal_lines(&rig), journal);
    assert_eq!(rig.fake.writes().len(), writes);
    let shown = proposal["details"].to_string();
    assert!(shown.contains("pool1") && shown.contains("pool2"));
    assert!(shown.contains(backup["config"]["volatile.uuid"].as_str().unwrap()));
    let reads = rig.fake.requests().len();
    let result = run(&rig, &proposal).unwrap();
    let requests = rig.fake.requests();
    for path in [
        format!("/1.0/instances/sample?project={PROJECT}"),
        volume_path("pool2"),
    ] {
        assert!(
            requests[reads..]
                .iter()
                .filter(|r| r.method == "GET" && r.path == path)
                .count()
                >= 2,
            "execution rereads {path} before the append"
        );
    }
    arca_api::conforms(
        &(arca_api::verbs::verb("service_rebind").unwrap().result)(),
        &result,
    )
    .unwrap();
    assert_eq!(result["rebound"], true);
    assert_eq!(
        rig.fake.writes().len(),
        writes,
        "rebind writes only Arca state"
    );
    assert_eq!(
        journal_lines(&rig),
        journal + 1,
        "one durable binding event"
    );
    let cfg = rig.service_config("sample");
    let cfg = serde_json::to_value(cfg).unwrap();
    assert_eq!(cfg["pool"], "pool2");
    assert_eq!(cfg["dataVolume"]["pool"], "pool2");
    rig.call(
        "service_secret_set",
        json!({"name":"sample","key":"TOKEN","value":"after-bind"}),
    )
    .unwrap();
    assert_eq!(
        instance(&rig, "sample")["config"]["environment.TOKEN"],
        "after-bind"
    );
    assert_eq!(instance(&rig, "sample-backup"), backup);
    assert_eq!(rig.get(&volume_path("pool1")).unwrap(), old_volume);
    assert!(
        propose(&rig, json!({"name":"sample"})).is_err(),
        "same UUID is not a new bind"
    );
}

#[test]
fn rebind_requires_doctor_and_a_matching_one_shot_approved_proposal() {
    let rig = copied_service_binding_fixture();
    let handler = rig
        .handlers
        .get("service_rebind")
        .expect("explicit rebind handler");
    let journal = journal_len(&rig);
    let writes = rig.fake.writes().len();
    let mut viewer = doctor();
    viewer.scope = Scope::Viewer;
    assert!(matches!(
        handler.call(json!({"name":"sample"}), &viewer),
        Err(CallError::Forbidden { .. })
    ));
    assert!(
        handler
            .call(json!({"name":"sample","approval":"invented"}), &doctor())
            .is_err()
    );
    assert!(
        propose(
            &rig,
            json!({"name":"sample","binding":{"uuid":"caller-chosen"}})
        )
        .is_err()
    );
    unchanged(&rig, journal, writes);
    let proposal = propose(&rig, json!({"name":"sample"})).unwrap();
    let approved = rig
        .kernel
        .lock()
        .unwrap()
        .approve_action_in_terminal(proposal["id"].as_str().unwrap(), "test-admin", NOW, NOW)
        .unwrap();
    let exec =
        arca::handlers::ActionExecutor::new(rig.handlers.iter().map(|(k, v)| (*k, v.clone())));
    let mut different = approved.args.clone();
    different["name"] = json!("another");
    assert!(
        exec.run(&approved.action, &different, &approved.grant)
            .is_err()
    );
    unchanged(&rig, journal, writes);
    exec.run(&approved.action, &approved.args, &approved.grant)
        .unwrap();
    assert!(
        exec.run(&approved.action, &approved.args, &approved.grant)
            .is_err()
    );
}

fn refused_case(change: impl FnOnce(&Rig)) {
    let rig = copied_service_binding_fixture();
    propose(&rig, json!({"name":"sample"})).expect("control: valid copy can be proposed");
    change(&rig);
    let journal = journal_len(&rig);
    let writes = rig.fake.writes().len();
    assert!(propose(&rig, json!({"name":"sample"})).is_err());
    unchanged(&rig, journal, writes);
}
#[test]
fn rebind_refuses_another_owner() {
    refused_case(|r| {
        r.fake
            .set_instance_config(PROJECT, "sample", KERNEL_ENTRY_KEY, "service-other")
    });
}
#[test]
fn rebind_refuses_contested_marks() {
    refused_case(|r| {
        r.fake
            .set_instance_config(PROJECT, "sample", "user.droid.owner", "other")
    });
}
#[test]
fn rebind_refuses_a_retiring_instance() {
    refused_case(|r| {
        r.fake
            .set_instance_config(PROJECT, "sample", "user.arcakernel.retiring", "plan")
    });
}
#[test]
fn rebind_refuses_a_missing_uuid() {
    refused_case(|r| {
        r.fake
            .set_instance_config(PROJECT, "sample", "volatile.uuid", "")
    });
}
#[test]
fn rebind_refuses_a_retiring_volume() {
    refused_case(|r| {
        let mut v = r.get(&volume_path("pool2")).unwrap();
        v["config"]["user.arcakernel.retiring"] = json!("plan");
        r.edit(
            "PATCH",
            &volume_path("pool2"),
            json!({"config":v["config"]}),
        );
    });
}
#[test]
fn rebind_refuses_a_foreign_volume() {
    refused_case(|r| {
        let mut v = r.get(&volume_path("pool2")).unwrap();
        v["config"][KERNEL_ENTRY_KEY] = json!("service-other");
        r.edit(
            "PATCH",
            &volume_path("pool2"),
            json!({"config":v["config"]}),
        );
    });
}
#[test]
fn rebind_refuses_missing_or_cross_project_storage() {
    refused_case(|r| {
        r.fake.set_instance_device(PROJECT,"sample","data",json!({"type":"disk","path":"/data","pool":"pool2","source":"sample-data","project":"other"}))
    });
}
#[test]
fn rebind_refuses_unreadable_family_evidence() {
    refused_case(|r| {
        r.fake
            .fail_reads("/1.0/instances?all-projects=true", 8, 503)
    });
}
#[test]
fn rebind_refuses_a_duplicate_target_and_old_uuid_under_the_same_name() {
    refused_case(|r| {
        let mut old = instance(r, "sample-backup");
        old["name"] = json!("sample");
        old["project"] = json!(PROJECT);
        r.fake.add_instance(old);
    });
}

#[test]
fn rebind_refuses_an_unresolved_effect() {
    refused_case(|r| {
        r.kernel.lock().unwrap().propose(vec![], NOW).unwrap();
        *r.unknown_create.lock().unwrap() = true;
        let _ = r.call(
            "service_create",
            json!({"name":"pending","template":"nats"}),
        );
    });
}

#[test]
fn a_rebind_card_cannot_pick_a_later_uuid_config_or_device_layout() {
    for change in ["uuid", "config", "devices", "volume"] {
        let rig = copied_service_binding_fixture();
        let proposal = propose(&rig, json!({"name":"sample"})).unwrap();
        match change {
            "uuid" => rig
                .fake
                .set_instance_config(PROJECT, "sample", "volatile.uuid", "later"),
            "config" => rig
                .fake
                .set_instance_config(PROJECT, "sample", "user.note", "changed"),
            "devices" => rig.fake.set_instance_device(
                PROJECT,
                "sample",
                "root",
                json!({"type":"disk","path":"/","pool":"pool1"}),
            ),
            _ => {
                let mut v = rig.get(&volume_path("pool2")).unwrap();
                v["config"]["user.note"] = json!("changed");
                rig.edit(
                    "PATCH",
                    &volume_path("pool2"),
                    json!({"config":v["config"]}),
                );
            }
        }
        let journal = journal_len(&rig);
        let writes = rig.fake.writes().len();
        assert!(run(&rig, &proposal).is_err(), "stale {change}");
        unchanged(&rig, journal, writes);
    }
}

#[test]
fn removing_a_rebound_service_keeps_an_acl_the_backup_still_references() {
    let rig = copied_service_binding_fixture();
    let proposal = propose(&rig, json!({"name":"sample"})).unwrap();
    run(&rig, &proposal).unwrap();
    let backup = instance(&rig, "sample-backup");
    let acl_path = format!(
        "/1.0/network-acls/{}?project=default",
        rig.service_config("sample").acl()
    );
    let error = remove_approved(&rig).unwrap_err();
    let CallError::Refused { message: words, .. } = error else {
        panic!("unexpected removal result: {error:?}")
    };
    assert!(
        words.contains("instance removed")
            && words.contains("data volume removed")
            && words.contains("ACL remains")
            && words.contains("referenc"),
        "{words}"
    );
    assert_eq!(instance(&rig, "sample-backup"), backup);
    assert!(rig.get(&volume_path("pool2")).is_none());
    assert!(rig.get(&acl_path).is_some());
    rig.edit(
        "DELETE",
        &format!("/1.0/instances/sample-backup?project={PROJECT}"),
        json!({}),
    );
    assert!(
        matches!(rig.call("service_remove", json!({"name":"sample","approval":"unused"})),
        Err(CallError::Refused { code, message }) if code == "not_found" && message == "service sample: not found")
    );
    // The live ACL inverse remains available for the next normal reconciliation.
    rig.kernel.lock().unwrap().propose(vec![], NOW).unwrap();
    assert!(
        rig.get(&acl_path).is_none(),
        "the live ACL inverse can still finish on reconciliation"
    );
    assert!(
        rig.kernel
            .lock()
            .unwrap()
            .service_uuid(&EntryId("service-sample".into()), "sample")
            .is_none()
    );
}

#[test]
fn rebind_refuses_the_old_uuid_and_a_missing_target() {
    for missing in [false, true] {
        let rig = copied_service_binding_fixture();
        propose(&rig, json!({"name":"sample"})).unwrap();
        if missing {
            rig.fake.set_instance_status(PROJECT, "sample", "Stopped");
            rig.edit(
                "DELETE",
                &format!("/1.0/instances/sample?project={PROJECT}"),
                json!({}),
            );
        } else {
            let old = instance(&rig, "sample-backup")["config"]["volatile.uuid"]
                .as_str()
                .unwrap()
                .to_string();
            rig.fake
                .set_instance_config(PROJECT, "sample", "volatile.uuid", &old);
        }
        let journal = journal_len(&rig);
        let writes = rig.fake.writes().len();
        assert!(propose(&rig, json!({"name":"sample"})).is_err());
        unchanged(&rig, journal, writes);
    }
}
#[test]
fn rebind_refuses_a_retained_service() {
    let rig = copied_service_binding_fixture();
    propose(&rig, json!({"name":"sample"})).unwrap();
    let mut kernel = rig.kernel.lock().unwrap();
    let entry = kernel
        .entries()
        .iter()
        .find(|entry| entry.id.0 == "service-sample")
        .unwrap()
        .clone();
    kernel
        .propose(vec![EntryChange::Remove(entry.id.clone())], NOW)
        .unwrap();
    let plan = kernel
        .pending_plans()
        .into_iter()
        .find(|plan| plan.entry == entry.id)
        .unwrap();
    kernel.decline(&plan.digest).unwrap();
    kernel.propose(vec![EntryChange::Put(entry)], NOW).unwrap();
    drop(kernel);
    let journal = journal_len(&rig);
    let writes = rig.fake.writes().len();
    assert!(propose(&rig, json!({"name":"sample"})).is_err());
    unchanged(&rig, journal, writes);
}
#[test]
fn rebind_refuses_a_root_without_a_pool_and_an_unowned_or_retiring_volume() {
    for bad in ["root", "volume-owner", "volume-retiring"] {
        let rig = copied_service_binding_fixture();
        propose(&rig, json!({"name":"sample"})).unwrap();
        if bad == "root" {
            rig.fake.set_instance_device(
                PROJECT,
                "sample",
                "root",
                json!({"type":"disk","path":"/"}),
            );
        } else {
            let mut config = rig.get(&volume_path("pool2")).unwrap()["config"].clone();
            if bad == "volume-owner" {
                config[KERNEL_ENTRY_KEY] = json!("service-other");
            } else {
                config["user.arcakernel.retiring"] = json!("plan");
            }
            rig.edit("PATCH", &volume_path("pool2"), json!({"config":config}));
        }
        let journal = journal_len(&rig);
        let writes = rig.fake.writes().len();
        assert!(propose(&rig, json!({"name":"sample"})).is_err(), "{bad}");
        unchanged(&rig, journal, writes);
    }
}
#[test]
fn rebind_refuses_the_old_uuid_under_the_same_name_elsewhere_in_the_family() {
    let mut rig = copied_service_binding_fixture();
    let mut cfg = (*rig.cfg).clone();
    cfg.kernel.projects.push("family-other".into());
    rig.cfg = Arc::new(cfg);
    rig.handlers = service_handlers(rig.kernel.clone(), Arc::new(|| NOW), rig.cfg.clone())
        .into_iter()
        .collect();
    propose(&rig, json!({"name":"sample"})).unwrap();
    let mut old = instance(&rig, "sample-backup");
    let uuid = old["config"]["volatile.uuid"].as_str().unwrap().to_string();
    old["project"] = json!("family-other");
    old["name"] = json!("sample");
    rig.fake.add_instance(old);
    rig.fake
        .set_instance_config("family-other", "sample", "volatile.uuid", &uuid);
    let journal = journal_len(&rig);
    let writes = rig.fake.writes().len();
    assert!(propose(&rig, json!({"name":"sample"})).is_err());
    unchanged(&rig, journal, writes);
}

#[test]
fn rebind_keeps_root_and_data_locators_independent_and_supports_no_data() {
    for data in [true, false] {
        let rig = if data {
            copied_service_binding_fixture()
        } else {
            let rig = Rig::new();
            let mut template = nats_template();
            template["secrets"] = json!(["TOKEN"]);
            template["generate"] = json!({"TOKEN":"test_"});
            template["env"] = json!({"TOKEN":"{{secret:TOKEN}}"});
            rig.template("nats", template);
            rig.call("service_create", json!({"name":"sample","template":"nats"}))
                .unwrap();
            rig.fake
                .set_instance_config(PROJECT, "sample", "volatile.uuid", "replacement");
            rig
        };
        if data {
            rig.fake.add_volume(
                PROJECT,
                "pool2",
                "copied-data",
                rig.get(&volume_path("pool2")).unwrap(),
            );
            let mut disk = instance(&rig, "sample")["devices"]["data"].clone();
            disk["source"] = json!("copied-data");
            rig.fake
                .set_instance_device(PROJECT, "sample", "data", disk);
        }
        rig.fake.set_instance_device(
            PROJECT,
            "sample",
            "root",
            json!({"type":"disk","path":"/","pool":"pool1"}),
        );
        let proposal = propose(&rig, json!({"name":"sample"})).unwrap();
        run(&rig, &proposal).unwrap();
        let config = serde_json::to_value(rig.service_config("sample")).unwrap();
        assert_eq!(config["pool"], "pool1");
        if data {
            assert_eq!(config["dataVolume"]["pool"], "pool2");
            assert_eq!(config["dataVolume"]["name"], "copied-data");
        } else {
            assert!(config["dataVolume"].is_null());
        }
        rig.call(
            "service_secret_set",
            json!({"name":"sample","key":"TOKEN","value":"new-value"}),
        )
        .unwrap();
        if data {
            assert_eq!(instance(&rig, "sample")["devices"]["data"]["pool"], "pool2");
            assert_eq!(
                instance(&rig, "sample")["devices"]["data"]["source"],
                "copied-data"
            );
        }
    }
}

#[test]
fn a_fsynced_binding_repairs_old_current_and_converged_storage_before_restart_applies() {
    for cut in ["before-record", "after-record"] {
        let rig = copied_service_binding_fixture();
        let proposal = propose(&rig, json!({"name":"sample"})).unwrap();
        let before: Value =
            serde_json::from_slice(&fs::read(rig.dir.join("desired.json")).unwrap()).unwrap();
        if cut == "after-record" {
            run(&rig, &proposal).unwrap();
        }
        let snapshot = rig.dir.join(cut);
        fs::create_dir(&snapshot).unwrap();
        let mut state: Value =
            serde_json::from_slice(&fs::read(rig.dir.join("desired.json")).unwrap()).unwrap();
        if cut == "after-record" {
            state["converged"] = before["converged"].clone();
        }
        fs::write(
            snapshot.join("desired.json"),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();
        let mut journal = fs::read(rig.dir.join("journal.jsonl")).unwrap();
        if cut == "before-record" {
            let binding = &proposal["args"]["binding"];
            journal.extend(
                serde_json::to_vec(&json!({"kind":"binding","binding":{
                    "entry":"service-sample","at_ms":NOW,"from":binding["from"],"to":binding["to"]
                }}))
                .unwrap(),
            );
            journal.push(b'\n');
        }
        fs::write(snapshot.join("journal.jsonl"), journal).unwrap();
        fs::set_permissions(&snapshot, fs::Permissions::from_mode(0o700)).unwrap();
        for file in ["desired.json", "journal.jsonl"] {
            fs::set_permissions(snapshot.join(file), fs::Permissions::from_mode(0o600)).unwrap();
        }
        let writes = rig.fake.writes().len();
        let reopened = reopen(&rig, &snapshot).unwrap();
        let service = reopened
            .entries()
            .iter()
            .find(|entry| entry.id.0 == "service-sample")
            .unwrap();
        assert_eq!(service.config["pool"], "pool2", "{cut}");
        assert_eq!(service.config["dataVolume"]["pool"], "pool2", "{cut}");
        assert_eq!(
            rig.fake.writes().len(),
            writes,
            "replay and startup send no host write: {cut}"
        );
        assert!(rig.get(&volume_path("pool1")).is_some());
    }
}

#[test]
fn rebind_refuses_a_destination_uuid_present_under_two_names() {
    refused_case(|r| {
        let mut copy = instance(r, "sample");
        let uuid = copy["config"]["volatile.uuid"]
            .as_str()
            .unwrap()
            .to_string();
        copy["name"] = json!("collision");
        copy["project"] = json!(PROJECT);
        r.fake.add_instance(copy);
        r.fake
            .set_instance_config(PROJECT, "collision", "volatile.uuid", &uuid);
    });
}
#[test]
fn rebind_refuses_missing_or_ambiguous_data_devices() {
    for ambiguous in [false, true] {
        let r = copied_service_binding_fixture();
        propose(&r, json!({"name":"sample"})).unwrap();
        if ambiguous {
            r.fake.set_instance_device(
                PROJECT,
                "sample",
                "another-data",
                instance(&r, "sample")["devices"]["data"].clone(),
            );
        } else {
            r.fake.set_instance_device(
                PROJECT,
                "sample",
                "data",
                json!({"type":"disk","path":"/data","pool":"pool2"}),
            );
        }
        let journal = journal_len(&r);
        let writes = r.fake.writes().len();
        assert!(propose(&r, json!({"name":"sample"})).is_err());
        unchanged(&r, journal, writes);
    }
}

#[test]
fn a_durable_binding_keeps_storage_current_when_the_record_cache_cannot_be_saved() {
    let rig = copied_service_binding_fixture();
    let proposal = propose(&rig, json!({"name":"sample"})).unwrap();
    let journal = journal_lines(&rig);
    let writes = rig.fake.writes().len();
    let temporary = rig.dir.join("desired.json.tmp");
    fs::create_dir(&temporary).unwrap();
    assert!(run(&rig, &proposal).is_err());
    assert_eq!(
        journal_lines(&rig),
        journal + 1,
        "the event is already durable"
    );
    assert_eq!(
        rig.service_config("sample").pool,
        "pool2",
        "memory follows the durable event even on a cache error"
    );
    assert_eq!(rig.fake.writes().len(), writes);
    fs::remove_dir(&temporary).unwrap();
    rig.kernel.lock().unwrap().propose(vec![], NOW).unwrap();
    assert_eq!(
        rig.fake.writes().len(),
        writes,
        "the next proposal must not reapply the old storage"
    );
    assert!(rig.get(&volume_path("pool1")).is_some());
    next_copy(&rig);
    let second = propose(&rig, json!({"name":"sample"})).unwrap();
    run(&rig, &second).unwrap();
    assert_eq!(rig.service_config("sample").data_locator().name, "copied-c");
}

fn next_copy(rig: &Rig) {
    rig.fake.add_volume(
        PROJECT,
        "pool2",
        "copied-c",
        rig.get(&volume_path("pool2")).unwrap(),
    );
    rig.fake
        .set_instance_config(PROJECT, "sample", "volatile.uuid", "replacement-c");
    rig.fake.set_instance_device(
        PROJECT,
        "sample",
        "data",
        json!({"type":"disk","path":"/data","pool":"pool2","source":"copied-c"}),
    );
}
fn copy_saved_state(rig: &Rig) -> PathBuf {
    let snapshot = rig.dir.join("reopen");
    fs::create_dir(&snapshot).unwrap();
    fs::set_permissions(&snapshot, fs::Permissions::from_mode(0o700)).unwrap();
    for file in ["desired.json", "journal.jsonl"] {
        fs::copy(rig.dir.join(file), snapshot.join(file)).unwrap();
        fs::set_permissions(snapshot.join(file), fs::Permissions::from_mode(0o600)).unwrap();
    }
    snapshot
}
#[test]
fn rebind_refuses_a_second_binding_until_the_first_storage_record_is_saved() {
    let rig = copied_service_binding_fixture();
    let first = propose(&rig, json!({"name":"sample"})).unwrap();
    fs::create_dir(rig.dir.join("desired.json.tmp")).unwrap();
    assert!(run(&rig, &first).is_err());
    next_copy(&rig);
    let journal = journal_lines(&rig);
    let writes = rig.fake.writes().len();
    let second = propose(&rig, json!({"name":"sample"}));
    if let Ok(proposal) = &second {
        assert!(run(&rig, proposal).is_err());
    }
    let reopened = reopen(&rig, &copy_saved_state(&rig)).unwrap();
    let config = &reopened
        .entries()
        .iter()
        .find(|e| e.id.0 == "service-sample")
        .unwrap()
        .config;
    assert!(
        matches!(second, Err(CallError::Refused { message, .. }) if message.contains("not saved")),
        "a second binding must wait for saved storage; reopened record: {config}"
    );
    assert_eq!(journal_lines(&rig), journal);
    assert_eq!(config["pool"], "pool2");
    assert_eq!(config["dataVolume"]["name"], "sample-data");
    assert_eq!(rig.fake.writes().len(), writes);
}
#[test]
fn rebind_reopen_refuses_storage_outside_the_latest_binding_without_host_writes() {
    let rig = copied_service_binding_fixture();
    let first = propose(&rig, json!({"name":"sample"})).unwrap();
    fs::create_dir(rig.dir.join("desired.json.tmp")).unwrap();
    assert!(run(&rig, &first).is_err());
    next_copy(&rig);
    let snapshot = copy_saved_state(&rig);
    let from = first["args"]["binding"]["to"].clone();
    let mut to = from.clone();
    to["instance"]["uuid"] = json!("replacement-c");
    to["data_volume"]["name"] = json!("copied-c");
    let mut journal = fs::read(snapshot.join("journal.jsonl")).unwrap();
    journal.extend(
        serde_json::to_vec(&json!({"kind":"binding","binding":{
            "entry":"service-sample","at_ms":NOW,"from":from,"to":to
        }}))
        .unwrap(),
    );
    journal.push(b'\n');
    fs::write(snapshot.join("journal.jsonl"), journal).unwrap();
    let writes = rig.fake.writes().len();
    let error = reopen(&rig, &snapshot)
        .err()
        .expect("mixed storage must refuse to open");
    assert!(error.to_string().contains("service-sample"), "{error}");
    assert_eq!(rig.fake.writes().len(), writes);
}
#[test]
fn rebind_removal_does_not_blame_another_instance_for_its_own_remaining_nic() {
    let rig = copied_service_binding_fixture();
    let proposal = propose(&rig, json!({"name":"sample"})).unwrap();
    run(&rig, &proposal).unwrap();
    rig.edit(
        "DELETE",
        &format!("/1.0/instances/sample-backup?project={PROJECT}"),
        json!({}),
    );
    rig.fake.set_reject_delete("sample");
    let error = remove_approved(&rig).unwrap_err();
    assert!(matches!(error, CallError::Refused { message, .. }
        if message.contains("instance remains") && !message.contains("another instance")));
}
