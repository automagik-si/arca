//! Memory hotplug is a recorded VM option; omitted options leave existing creates alone.
use super::*;

#[test]
fn vm_memory_hotplug_round_trips_and_recreates_without_drift_on_both_networks() {
    for bridged in [false, true] {
        for hotplug in [None, Some(false), Some(true)] {
            let rig = Rig::with(
                json!({"projects":[PROJECT], "droiddProjects":["other"], "serviceCidr":"192.0.2.0/24",
                "bridged":{"bridges":["br0"], "pool":"vmpool", "limits":roomy()}}),
                None,
                vec![],
            );
            let mut template = vm_template();
            if let Some(value) = hotplug {
                template["limits"]["memoryHotplug"] = json!(value);
            }
            rig.template("ws", template);
            let mut args = json!({"name":"ws1", "template":"ws"});
            if bridged {
                args["bridge"] = json!("br0");
            }
            rig.call("service_create", args).expect("a VM create");
            let held = rig
                .entries()
                .into_iter()
                .find(|entry| entry.id.0 == "service-ws1")
                .unwrap();
            let record: service::ServiceConfig =
                serde_json::from_value(held.config.clone()).unwrap();
            let bytes = serde_json::to_vec(&record).unwrap();
            let restored: service::ServiceConfig = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(serde_json::to_vec(&restored).unwrap(), bytes);
            assert_eq!(restored.limits.as_ref().unwrap().memory_hotplug, hotplug);
            let mut expected = json!({"agent.nic_config":"true", "boot.autostart":"true",
                "limits.cpu":"2", "limits.memory":"4GiB", "user.arca.template":"ws"});
            if !bridged {
                expected[service::ADDRESS_KEY] = json!(restored.ipv4);
            }
            if let Some(value) = hotplug {
                expected["limits.memory.hotplug"] = json!(value.to_string());
                assert_eq!(held.config["limits"]["memoryHotplug"], value);
            } else {
                assert_eq!(
                    serde_json::to_string(restored.limits.as_ref().unwrap()).unwrap(),
                    r#"{"cpu":"2","memory":"4GiB"}"#
                );
                assert!(held.config["limits"].get("memoryHotplug").is_none());
            }
            let mut sent: Value = rig
                .fake
                .service_requests()
                .into_iter()
                .find(|r| r.method == "POST" && r.path.starts_with("/1.0/instances?"))
                .map(|r| serde_json::from_slice(&r.body).unwrap())
                .expect("the create request");
            sent["config"]
                .as_object_mut()
                .unwrap()
                .remove(KERNEL_ENTRY_KEY);
            assert_eq!(
                serde_json::to_string(&sent["config"]).unwrap(),
                serde_json::to_string(&expected).unwrap()
            );
            if !bridged {
                // create_body is the managed-network renderer; bridged creates use their own internal renderer.
                assert_eq!(
                    service::create_body(&restored),
                    sent,
                    "the whole create comes from the round-tripped record"
                );
            }
            let instance = rig
                .get(&format!("/1.0/instances/ws1?project={PROJECT}"))
                .unwrap();
            for (key, value) in expected.as_object().unwrap() {
                assert_eq!(&instance["config"][key], value);
            }
            assert_eq!(
                instance["config"].get("limits.memory.hotplug"),
                expected.get("limits.memory.hotplug")
            );
            let own = serde_json::from_value(instance["config"].clone()).unwrap();
            let retained = rig.kernel.lock().unwrap().retained_entries().unwrap();
            assert_eq!(
                arca_kernel::ownership::classify_object(
                    &arca_kernel::ownership::ObjectRef {
                        kind: arca_kernel::ownership::IncusKind::Instance,
                        project: PROJECT.into(),
                        parent: None,
                        name: "ws1".into(),
                        uuid: instance["config"]["volatile.uuid"]
                            .as_str()
                            .map(str::to_owned),
                    },
                    &own,
                    &retained
                ),
                arca_kernel::ownership::OwnerClass::Kernel {
                    entry: held.id.clone()
                }
            );
            for row in [
                rig.call("service_status", json!({"name":"ws1"})).unwrap(),
                rig.call("service_list", json!({})).unwrap()["services"][0].clone(),
            ] {
                assert_eq!(row["limits"], held.config["limits"]);
                assert_eq!(
                    row["limits"].get("memoryHotplug"),
                    hotplug
                        .as_ref()
                        .map(|_| &held.config["limits"]["memoryHotplug"])
                );
            }
            let writes = rig.fake.writes().len();
            // Apply through a fresh plugin context too, so the converged-entry shortcut cannot hide drift.
            use arca_kernel::{
                journal::{Journal, MemJournal},
                plugin::{Ctx, Plugin},
            };
            let mut provider = IncusProvider::new(
                rig.fake.socket().into(),
                Family {
                    projects: vec![PROJECT.into()],
                    scratch: false,
                    droidd_projects: vec!["other".into()],
                    reserved_cidrs: vec![],
                },
                rig.dir.join("secrets"),
                arca_providers::incus::RetainedSet::empty(),
            )
            .unwrap();
            let mut journal = Journal::open(MemJournal::default()).unwrap();
            Service::apply(
                &mut Ctx::new(
                    held.id.clone(),
                    &Service::manifest(),
                    &mut provider,
                    &mut journal,
                    NOW + 1,
                ),
                &restored,
            )
            .unwrap();
            let outcomes = rig.kernel.lock().unwrap().propose(vec![], NOW + 1).unwrap();
            assert!(
                outcomes
                    .iter()
                    .all(|outcome| matches!(outcome, arca_reconcile::Outcome::Applied { .. })),
                "{outcomes:?}"
            );
            assert_eq!(rig.fake.writes().len(), writes, "reconcile sees no drift");
        }
    }
}

#[test]
fn memory_hotplug_is_refused_when_read_on_containers_or_when_not_boolean() {
    let rig = Rig::managed();
    for kind in ["oci", "system"] {
        for value in [false, true] {
            let mut template = nats_template();
            template["kind"] = json!(kind);
            template["limits"] = json!({"cpu":"2", "memory":"4GiB", "memoryHotplug":value});
            rig.template("nats", template);
            let error = service::load_template(&rig.dir.join("templates"), "nats").unwrap_err();
            assert_eq!(
                error.message,
                "template nats: limits.memoryHotplug is only for a virtual-machine template"
            );
        }
    }
    for value in [json!("false"), json!(0), json!(null), json!([]), json!({})] {
        let mut template = vm_template();
        template["limits"]["memoryHotplug"] = value;
        rig.template("ws", template);
        assert!(service::load_template(&rig.dir.join("templates"), "ws").is_err());
    }
    assert!(rig.fake.writes().is_empty());
}

#[test]
fn a_pinned_vm_still_refuses_update_and_reconfigure_with_the_existing_words() {
    let key = common::SkKey::from_seed(9);
    let rig = Rig::bridged(&["br0"], Some(&key));
    let mut template = vm_template();
    template["limits"]["memoryHotplug"] = json!(false);
    rig.template("ws", template);
    rig.call("service_create", json!({"name":"ws1", "template":"ws"}))
        .unwrap();
    let before = rig.entries();
    let writes = rig.fake.writes().len();
    for (index, action, kind, args, message) in [
        (
            0,
            "service_update",
            ActionKind::ServiceUpdate,
            json!({"name":"ws1", "digest":DIGEST}),
            "update service ws1: only a container service made from an image by digest or a local image pinned by fingerprint can be updated. Nothing was changed",
        ),
        (
            1,
            "service_reconfigure",
            ActionKind::ServiceReconfigure,
            json!({"name":"ws1"}),
            "reconfigure service ws1: only a container service is reconfigured from its template. Nothing was changed",
        ),
    ] {
        let proposal = rig
            .kernel
            .lock()
            .unwrap()
            .propose_action(kind, args, NOW, None)
            .unwrap();
        let statement = action_statement(
            "node1",
            &proposal.id,
            action,
            &proposal.args_digest,
            NOW,
            &format!("{:032x}", 0x7000 + index),
        )
        .unwrap();
        let approval = common::approve_statement(&key, &statement);
        let approved = approve_action(
            &mut rig.kernel.lock().unwrap(),
            &json!({"plan":proposal.id,"approval":approval}),
            NOW,
        )
        .unwrap();
        let result = ActionExecutor::new(
            rig.handlers
                .iter()
                .map(|(verb, handler)| (*verb, Arc::clone(handler))),
        )
        .run(&approved.action, &approved.args, &approved.grant);
        assert_eq!(refusal(result), message);
    }
    assert_eq!(rig.entries(), before);
    assert_eq!(rig.fake.writes().len(), writes);
}
