//! Group 5's title tests and the exec seam's independent negative cases. All secrets and addresses are synthetic.
use super::*;
use arca::service_exec::{AUDIT_FILE, ExecError, ExecTarget, ServiceExec};
use arca_incus::{
    exec::{DashPort, ExecCommand as C, JournalLines, TailnetNode},
    fake::ExecScript,
};
use std::time::{Duration, Instant};

fn signed_rig() -> (Rig, common::SkKey) {
    let key = common::SkKey::from_seed(9);
    let mut rig = Rig::with_key(
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![],
            networks: vec![],
            pools: vec![],
        },
        vec![PROJECT.into()],
        Some("192.0.2.0/24"),
        Some(&key),
    );
    configure(&mut rig, Some("host.example.ts.net"), Some("example"));
    (rig, key)
}
fn configure(rig: &mut Rig, host: Option<&str>, tailnet: Option<&str>) {
    let cfg:NodeConfig=serde_json::from_value(json!({"stateDir":rig.dir,"incus":{"socket":rig.fake.socket()},"listen":{"unix":rig.dir.join("arca.sock")},"tokens":{"hashDir":rig.dir.join("tokens")},"kernel":{"projects":[PROJECT],"droiddProjects":["other"],"serviceCidr":"192.0.2.0/24"},"serveHost":host,"tailnet":tailnet})).unwrap();
    rig.handlers = service_handlers(rig.kernel.clone(), Arc::new(|| NOW), Arc::new(cfg))
        .into_iter()
        .collect();
}
fn op(rig: &Rig, verb: &str, args: Value) -> Result<Value, CallError> {
    rig.handlers[verb].call(
        args,
        &Caller {
            token_id: "operator".into(),
            scope: Scope::Operator,
            tools: None,
        },
    )
}
fn system(rig: &Rig, name: &str) {
    rig.template("dash",json!({"name":"dash","kind":"system","image":"images:debian/13","ports":[],"env":{},"secrets":["tailscale_authkey"],"publishable":true,"dashPort":3080}));
    rig.call("service_create", json!({"name":name,"template":"dash"}))
        .unwrap();
    op(
        rig,
        "service_secret_set",
        json!({"name":name,"key":"tailscale_authkey","value":"tskey-synthetic-example"}),
    )
    .unwrap();
}
fn approval(key: &common::SkKey, name: &str) -> String {
    use arca_kernel::canon::{canonical_json, params_digest};
    common::approve_statement(key,&canonical_json(&json!({"host":"node1","action":"publish","target":name,"paramsDigest":params_digest(&json!({"name":name,"reach":"tailnet"})).unwrap(),"issuedAt":"2026-10-30T12:00:00.000Z","expiresAt":"2026-10-30T12:05:00.000Z","nonce":"b".repeat(64)})).unwrap())
}
fn publish(rig: &Rig, key: &common::SkKey, name: &str) -> Result<Value, CallError> {
    op(
        rig,
        "service_publish",
        json!({"name":name,"reach":"tailnet","approval":approval(key,name)}),
    )
}
fn node_status(name: &str) -> String {
    json!({"BackendState":"Running","Self":{"DNSName":format!("arca-{name}.example.ts.net."),"Tags":["tag:arca-svc"]}}).to_string()
}
fn serving(name: &str) -> Value {
    json!({"TCP":{"8443":{"HTTPS":true}},"Web":{format!("arca-{name}.example.ts.net:8443"): {"Handlers":{"/":{"Proxy":"http://127.0.0.1:3080"}}}},"Foreground":{}})
}
fn script(rig: &Rig, cmd: C, exit: i64, text: &str) {
    rig.fake
        .script_exec(ExecScript::new(cmd.argv(), exit, text));
}
fn publish_scripts(rig: &Rig, name: &str, status: &str, serve: &Value) {
    for cmd in [
        C::TailscaleVersion,
        C::StartDaemon,
        C::ClearKeyDir,
        C::MakeKeyDir,
        C::Join(TailnetNode::new(&format!("arca-{name}")).unwrap()),
        C::ClearKeyDir,
    ] {
        script(rig, cmd, 0, "");
    }
    script(rig, C::TailnetStatus, 0, status);
    script(rig, C::ServeOn(DashPort::new(3080).unwrap()), 0, "");
    script(rig, C::ServeStatus, 0, &serve.to_string());
}
fn cleanup_scripts(rig: &Rig) {
    for cmd in [C::ServeOff, C::Logout, C::ClearKeyDir, C::StopDaemon] {
        script(rig, cmd, 0, "");
    }
}
fn unpublish_scripts(rig: &Rig, has_record: bool, logout_takes: bool) {
    script(rig, C::StartDaemon, 0, "");
    script(
        rig,
        C::ServeStatus,
        0,
        if has_record {
            r#"{"TCP":{"8443":{"HTTPS":true}}}"#
        } else {
            "{}"
        },
    );
    if has_record {
        script(rig, C::ServeOff, 0, "");
    }
    script(rig, C::TailnetStatus, 0, &node_status("dash"));
    script(rig, C::Logout, 0, "");
    script(
        rig,
        C::TailnetStatus,
        0,
        if logout_takes {
            r#"{"BackendState":"NeedsLogin"}"#
        } else {
            r#"{"BackendState":"Running"}"#
        },
    );
    if logout_takes {
        script(rig, C::ClearKeyDir, 0, "");
        script(rig, C::StopDaemon, 0, "");
    }
}
fn commands(rig: &Rig) -> Vec<Vec<String>> {
    rig.fake
        .service_requests()
        .into_iter()
        .filter(|r| r.path.contains("/exec?") && r.method == "POST")
        .map(|r| {
            serde_json::from_slice::<Value>(&r.body).unwrap()["command"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().into())
                .collect()
        })
        .collect()
}
fn create_pair(rig: &Rig) {
    rig.template("provider",json!({"name":"provider","kind":"oci","image":"docker.io/library/example","digest":DIGEST,"ports":[5432],"env":{"PASSWORD":"{{secret:PASSWORD}}"},"secrets":["PASSWORD"],"provides":{"env":{"DATABASE_URL":"postgresql://app:{{secret:PASSWORD}}@{{host}}:{{port}}/app"}}}));
    rig.template("consumer",json!({"name":"consumer","kind":"oci","image":"docker.io/library/example","digest":DIGEST,"ports":[],"env":{}}));
    for (name, t) in [("db", "provider"), ("app", "consumer")] {
        rig.call("service_create", json!({"name":name,"template":t}))
            .unwrap();
    }
    op(
        rig,
        "service_secret_set",
        json!({"name":"db","key":"PASSWORD","value":"synthetic-password"}),
    )
    .unwrap();
}
fn instance(rig: &Rig, name: &str) -> Value {
    rig.get(&format!("/1.0/instances/{name}?project={PROJECT}"))
        .unwrap()
}
fn seam(rig: &Rig, name: &str) -> (ServiceExec, ExecTarget) {
    let cfg:NodeConfig=serde_json::from_value(json!({"stateDir":rig.dir,"incus":{"socket":rig.fake.socket()},"listen":{"unix":rig.dir.join("arca.sock")},"tokens":{"hashDir":rig.dir.join("tokens")},"kernel":{"projects":[PROJECT],"droiddProjects":["other"],"serviceCidr":"192.0.2.0/24"}})).unwrap();
    let config = rig.service_config(name);
    let kernel = rig.kernel.lock().unwrap();
    let entry = EntryId(format!("service-{name}"));
    let target = ExecTarget {
        uuid: kernel.service_uuid(&entry, name),
        entry,
        service: config,
        retained: kernel.retained_entries().unwrap(),
    };
    (ServiceExec::new(Arc::new(cfg)), target)
}
fn run(seam: &ServiceExec, target: &ExecTarget, cmd: &C) -> Result<Vec<u8>, ExecError> {
    seam.run(target, "viewer", "service_logs", cmd, &[], None)
}
/// Whether the exec audit holds an exec that has started and not ended.
fn exec_in_flight(rig: &Rig) -> bool {
    let audit = fs::read_to_string(rig.dir.join(AUDIT_FILE)).unwrap_or_default();
    let rows: Vec<Value> = audit
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let ended: Vec<u64> = rows
        .iter()
        .filter(|row| row["kind"] == "end")
        .filter_map(|row| row["id"].as_u64())
        .collect();
    rows.iter().any(|row| {
        row["kind"] == "start" && row["id"].as_u64().is_some_and(|id| !ended.contains(&id))
    })
}
/// How many waits on an operation the fake has written down.
fn waits(rig: &Rig) -> usize {
    rig.fake
        .requests()
        .iter()
        .filter(|r| {
            r.method == "GET" && r.path.starts_with("/1.0/operations/") && r.path.contains("/wait?")
        })
        .count()
}

/// A database through the card alone. On a node with the keys `arca init --enable-changes` writes, with the shipped
/// templates as init installs them, a token that only looks and proposes asks for the shipped `postgres-autopg`
/// service, the owner approves, and it STARTS: the template makes its own password, so no `service_secret_set`,
/// which cannot be proposed, is needed, and so no doctor token. A second approved card links a service to it, and
/// that service gets a `DATABASE_URL` with the same password.
#[test]
fn the_shipped_postgres_template_approved_from_a_card_starts_and_gives_a_linked_service_its_database_url()
 {
    let key = common::SkKey::from_seed(9);
    let rig = Rig::with_key(
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![],
            networks: vec![],
            pools: vec![],
        },
        vec![PROJECT.into()],
        Some("192.0.2.0/24"),
        Some(&key),
    );
    let templates = rig.dir.join("templates");
    assert!(
        arca::cmd_templates::install_into(&templates, false).is_ok(),
        "the shipped templates install"
    );
    rig.template("consumer",json!({"name":"consumer","kind":"oci","image":"docker.io/library/example","digest":DIGEST,"ports":[],"env":{}}));

    // The shipped template is not one that is created stopped: nothing it needs is missing before the create.
    let shipped =
        service::load_template(&templates, "postgres-autopg").expect("the shipped template");
    assert_eq!(
        service::template_stopped_reason(&shipped, "service-db1", &rig.dir.join("secrets")),
        None
    );

    let plans: HashMap<&'static str, Arc<dyn ToolHandler>> = arca::handlers::plan_handlers_served(
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
    .collect();
    let viewer = Caller {
        token_id: "viewer".into(),
        scope: Scope::Viewer,
        tools: None,
    };
    let executor = arca::handlers::ActionExecutor::new(
        rig.handlers
            .iter()
            .map(|(verb, handler)| (*verb, Arc::clone(handler))),
    );
    // One card: proposed with a token that only looks and proposes, signed by the owner's key, and run.
    let card = |action: &str, args: Value, nonce: &str| -> (Value, Value) {
        let proposed = plans["action_propose"]
            .call(json!({ "action": action, "args": args }), &viewer)
            .expect("the proposal is recorded");
        let statement = arca::handlers::action_statement(
            "node1",
            proposed["id"].as_str().expect("its id"),
            action,
            proposed["argsDigest"].as_str().expect("its digest"),
            NOW,
            &nonce.repeat(32),
        )
        .expect("the statement");
        let approval = common::approve_statement(&key, &statement);
        let approved = {
            let mut kernel = rig.kernel.lock().expect("kernel");
            arca::handlers::approve_action(
                &mut kernel,
                &json!({ "plan": proposed["id"], "approval": approval }),
                NOW,
            )
            .expect("the owner's approval verifies")
        };
        let result = executor
            .run(&approved.action, &approved.args, &approved.grant)
            .expect("the approved action runs");
        (proposed, result)
    };

    let (proposed, created) = card(
        "service_create",
        json!({ "name": "db1", "template": "postgres-autopg" }),
        "1",
    );
    // The card told the owner the truth before the signature. A template that is created stopped whatever the
    // call says gets a Start row that begins "No: it is created stopped because"; this one has no such row.
    assert!(
        proposed["details"]
            .as_array()
            .expect("the card's rows")
            .iter()
            .all(|row| row["label"] != "Start"),
        "{proposed}"
    );
    assert_eq!(created["state"], "running", "{created}");
    // Its password was made at create: letters and digits, which the template's own start script accepts and which
    // goes into a URL as it is. It is in the instance and in no answer.
    let password = instance(&rig, "db1")["config"]["environment.POSTGRES_PASSWORD"]
        .as_str()
        .expect("the database has its password")
        .to_string();
    assert_eq!(password.len(), 32);
    assert!(password.bytes().all(|byte| byte.is_ascii_alphanumeric()));
    assert!(!created.to_string().contains(&password) && !proposed.to_string().contains(&password));

    // A service linked to it by a second card gets the URL, with that password and the database's own address.
    card(
        "service_create",
        json!({ "name": "app", "template": "consumer" }),
        "2",
    );
    let (_, linked) = card(
        "service_connect",
        json!({ "from": "app", "to": "db1" }),
        "3",
    );
    assert_eq!(linked, json!({ "env": ["DATABASE_URL"] }));
    let address = instance(&rig, "db1")["config"][service::ADDRESS_KEY]
        .as_str()
        .expect("the database's address")
        .to_string();
    assert_eq!(
        instance(&rig, "app")["config"]["environment.DATABASE_URL"],
        json!(["postgresql://app:", &password, "@", &address, ":5432/app"].concat())
    );
    // Nothing in this test set a secret, and no handler was called with more than the owner's approval.
    assert!(
        rig.entries()
            .iter()
            .any(|entry| entry.id.0 == "service-db1"),
        "the database is the kernel's own"
    );
}

/// A card that waits across an upgrade. A database is proposed while the node still has the template of 0.12.0, the
/// card says it will be created stopped, and then `templates install` replaces that template with this release's.
/// The approval of the old card creates nothing: the card bound the content of the template it showed, and the
/// refusal says to propose it again. Proposed again, on the new template, it is created and runs. This is what
/// `arca init` and the installer tell the owner beside the names they replaced.
#[test]
fn a_card_that_waited_across_the_replacement_of_its_template_is_refused_and_proposed_again_it_runs()
{
    let key = common::SkKey::from_seed(9);
    let rig = Rig::with_key(
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![],
            networks: vec![],
            pools: vec![],
        },
        vec![PROJECT.into()],
        Some("192.0.2.0/24"),
        Some(&key),
    );
    let templates = rig.dir.join("templates");
    assert!(arca::cmd_templates::install_into(&templates, false).is_ok());
    for name in ["postgres-autopg.json", "postgres-autopg.sh"] {
        fs::write(templates.join(name), postgres_file_of_0_12_0(name)).expect("the file of 0.12.0");
    }
    let plans: HashMap<&'static str, Arc<dyn ToolHandler>> = arca::handlers::plan_handlers_served(
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
    .collect();
    let viewer = Caller {
        token_id: "viewer".into(),
        scope: Scope::Viewer,
        tools: None,
    };
    let executor = arca::handlers::ActionExecutor::new(
        rig.handlers
            .iter()
            .map(|(verb, handler)| (*verb, Arc::clone(handler))),
    );
    let propose = || {
        plans["action_propose"]
            .call(
                json!({ "action": "service_create", "args": { "name": "db1", "template": "postgres-autopg" } }),
                &viewer,
            )
            .expect("the proposal is recorded")
    };
    let approve_and_run = |proposed: &Value, nonce: &str| {
        let statement = arca::handlers::action_statement(
            "node1",
            proposed["id"].as_str().expect("its id"),
            "service_create",
            proposed["argsDigest"].as_str().expect("its digest"),
            NOW,
            &nonce.repeat(32),
        )
        .expect("the statement");
        let approval = common::approve_statement(&key, &statement);
        let approved = {
            let mut kernel = rig.kernel.lock().expect("kernel");
            arca::handlers::approve_action(
                &mut kernel,
                &json!({ "plan": proposed["id"], "approval": approval }),
                NOW,
            )
            .expect("the owner's approval verifies")
        };
        executor.run(&approved.action, &approved.args, &approved.grant)
    };

    // Proposed on the template of 0.12.0: the card says the truth of that template.
    let before = propose();
    assert!(
        before.to_string().contains("created stopped"),
        "the old template's card: {before}"
    );
    // The upgrade replaces the template while the card waits.
    let report = arca::cmd_templates::install_into(&templates, false).expect("install");
    assert_eq!(
        report["replaced"],
        json!(["postgres-autopg.json", "postgres-autopg.sh"])
    );
    // The old card's approval makes nothing, and says what to do.
    let refused = format!(
        "{:?}",
        approve_and_run(&before, "1").expect_err("the old card creates nothing")
    );
    assert!(
        refused.contains("was changed or installed again since the card was shown")
            && refused.contains("Nothing was created; propose it again"),
        "{refused}"
    );
    assert!(
        rig.get(&format!("/1.0/instances/db1?project={PROJECT}"))
            .is_none(),
        "nothing was created"
    );
    // Proposed again, the card is the new template's, and the database runs.
    let after = propose();
    assert_ne!(
        after["id"], before["id"],
        "another card: it binds another template"
    );
    assert!(!after.to_string().contains("created stopped"), "{after}");
    let created = approve_and_run(&after, "2").expect("the new card's create runs");
    assert_eq!(created["state"], "running", "{created}");
}

/// A node that installed its templates with 0.12.0. Its template makes no password, so a database created from it
/// is stopped. A plain `templates install` of this release (which `arca init` also runs) replaces that template,
/// since nobody edited it, and the next database starts. The database made before is not touched: it gains no
/// password by itself, and a password the owner stores for it is the one it gets.
#[test]
fn a_node_from_0_12_0_gets_the_new_postgres_template_and_its_old_database_is_left_as_it_is() {
    let rig = Rig::new();
    let templates = rig.dir.join("templates");
    assert!(arca::cmd_templates::install_into(&templates, false).is_ok());
    for name in ["postgres-autopg.json", "postgres-autopg.sh"] {
        fs::write(templates.join(name), postgres_file_of_0_12_0(name)).expect("the file of 0.12.0");
    }

    // The template as 0.12.0 shipped it: created stopped.
    let old = rig
        .call(
            "service_create",
            json!({ "name": "old1", "template": "postgres-autopg" }),
        )
        .expect("create");
    assert_eq!(old["state"], "stopped", "{old}");

    // The upgrade: no flag, and the report names what it replaced.
    let report = arca::cmd_templates::install_into(&templates, false).expect("install");
    assert_eq!(
        report["replaced"],
        json!(["postgres-autopg.json", "postgres-autopg.sh"]),
        "{report}"
    );
    let new = rig
        .call(
            "service_create",
            json!({ "name": "new1", "template": "postgres-autopg" }),
        )
        .expect("create");
    assert_eq!(new["state"], "running", "{new}");

    // The database from before gains no password by itself.
    assert!(
        instance(&rig, "old1")["config"]
            .get("environment.POSTGRES_PASSWORD")
            .is_none(),
        "a service from before gains no password by itself"
    );
    // The owner's own password, once stored, is the one it gets: the template makes none over it.
    op(
        &rig,
        "service_secret_set",
        json!({ "name": "old1", "key": "POSTGRES_PASSWORD", "value": "synthetic-password" }),
    )
    .expect("the owner's password");
    assert_eq!(
        instance(&rig, "old1")["config"]["environment.POSTGRES_PASSWORD"],
        "synthetic-password"
    );
}

#[test]
fn g5_connect_and_disconnect_open_only_the_provider_port_and_render_env() {
    let rig = Rig::new();
    create_pair(&rig);
    assert_eq!(
        op(&rig, "service_connect", json!({"from":"app","to":"db"})).unwrap(),
        json!({"env":["DATABASE_URL"]})
    );
    let acl = rig
        .get(&format!(
            "/1.0/network-acls/{PROJECT}-svc-db?project=default"
        ))
        .unwrap();
    let rules = acl["ingress"].as_array().unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(
        rules[0]["source"],
        format!(
            "{}/32",
            instance(&rig, "app")["config"][service::ADDRESS_KEY]
                .as_str()
                .unwrap()
        )
    );
    assert_eq!(rules[0]["destination_port"], "5432");
    assert!(
        instance(&rig, "app")["config"]["environment.DATABASE_URL"]
            .as_str()
            .unwrap()
            .contains("synthetic-password")
    );
    assert_eq!(
        op(&rig, "service_disconnect", json!({"from":"app","to":"db"})).unwrap(),
        json!({"env":["DATABASE_URL"]})
    );
    assert!(
        instance(&rig, "app")["config"]
            .get("environment.DATABASE_URL")
            .is_none()
    );
    assert!(
        rig.get(&format!(
            "/1.0/network-acls/{PROJECT}-svc-db?project=default"
        ))
        .unwrap()["ingress"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}
#[test]
fn g5_failed_acl_records_no_link_and_changes_no_environment() {
    let rig = Rig::new();
    create_pair(&rig);
    let before = instance(&rig, "app")["config"].clone();
    rig.fake
        .set_reject_patch(&format!("/1.0/network-acls/{PROJECT}-svc-db"));
    assert!(op(&rig, "service_connect", json!({"from":"app","to":"db"})).is_err());
    assert!(rig.service_config("app").links.is_empty());
    assert_eq!(instance(&rig, "app")["config"], before);
}
#[test]
fn g5_secrets_reach_no_answer_audit_journal_or_public_metadata() {
    let rig = Rig::new();
    create_pair(&rig);
    let secret = "synthetic-rotated-secret";
    let answer = op(
        &rig,
        "service_secret_set",
        json!({"name":"db","key":"PASSWORD","value":secret}),
    )
    .unwrap();
    assert!(!answer.to_string().contains(secret));
    for f in ["desired.json", "journal.jsonl", AUDIT_FILE] {
        if let Ok(text) = fs::read_to_string(rig.dir.join(f)) {
            assert!(!text.contains(secret), "{f}");
        }
    }
    let journal = arca::handlers::JournalListHandler {
        state_dir: rig.dir.clone(),
    }
    .call(
        json!({}),
        &Caller {
            token_id: "viewer".into(),
            scope: Scope::Viewer,
            tools: None,
        },
    )
    .unwrap();
    assert!(!journal.to_string().contains(secret));
    let values = instance(&rig, "db");
    for (k, v) in values["config"].as_object().unwrap() {
        if k.starts_with("user.arca.") {
            assert!(!v.to_string().contains(secret));
        }
    }
    rig.fake.set_console_log(PROJECT, "db", secret);
    assert!(
        !rig.call("service_logs", json!({"name":"db"}))
            .unwrap()
            .to_string()
            .contains(secret)
    );
}
#[test]
fn g5_image_environment_survives_secret_link_and_disconnect() {
    let rig = Rig::new();
    create_pair(&rig);
    rig.edit(
        "PATCH",
        &format!("/1.0/instances/app?project={PROJECT}"),
        json!({"config":{"environment.PATH":"/image/bin","environment.PGDATA":"/image/data"}}),
    );
    for verb in ["service_connect", "service_disconnect"] {
        op(&rig, verb, json!({"from":"app","to":"db"})).unwrap();
        let live = instance(&rig, "app");
        assert_eq!(live["config"]["environment.PATH"], "/image/bin");
        assert_eq!(live["config"]["environment.PGDATA"], "/image/data");
    }
    let before = instance(&rig, "db")["config"].clone();
    op(
        &rig,
        "service_secret_set",
        json!({"name":"db","key":"PASSWORD","value":"another-synthetic-password"}),
    )
    .unwrap();
    for (k, v) in before.as_object().unwrap() {
        if k != "environment.PASSWORD" {
            assert_eq!(&instance(&rig, "db")["config"][k], v);
        }
    }
}
#[test]
fn g5_restored_link_records_cannot_reopen_a_stale_grant() {
    for damage in ["gone", "address", "uuid", "undone"] {
        let rig = Rig::new();
        create_pair(&rig);
        op(&rig, "service_connect", json!({"from":"app","to":"db"})).unwrap();
        let stale = rig.service_config("db");
        match damage {
            "gone" => {
                rig.edit(
                    "PUT",
                    &format!("/1.0/instances/app/state?project={PROJECT}"),
                    json!({"action":"stop","force":true}),
                );
                rig.edit(
                    "DELETE",
                    &format!("/1.0/instances/app?project={PROJECT}"),
                    json!({}),
                );
                assert!(
                    rig.get(&format!("/1.0/instances/app?project={PROJECT}"))
                        .is_none()
                );
            }
            "address" => rig.edit(
                "PATCH",
                &format!("/1.0/instances/app?project={PROJECT}"),
                json!({"devices":{"eth0":{"type":"nic","ipv4.address":"192.0.2.99"}}}),
            ),
            "uuid" => rig.edit(
                "PATCH",
                &format!("/1.0/instances/app?project={PROJECT}"),
                json!({"config":{"volatile.uuid":"replacement"}}),
            ),
            _ => {
                op(&rig, "service_disconnect", json!({"from":"app","to":"db"})).unwrap();
            }
        }
        rig.apply(&stale).unwrap();
        assert!(
            rig.get(&format!(
                "/1.0/network-acls/{PROJECT}-svc-db?project=default"
            ))
            .unwrap()["ingress"]
                .as_array()
                .unwrap()
                .is_empty(),
            "{damage}: {:?}",
            rig.get(&format!(
                "/1.0/network-acls/{PROJECT}-svc-db?project=default"
            ))
            .unwrap()["ingress"]
        );
    }
}
#[test]
fn g5_publish_refuses_non_tailnet_reach_and_audits_without_exec() {
    let (rig, _) = signed_rig();
    system(&rig, "dash");
    let before = commands(&rig).len();
    assert!(
        matches!(op(&rig,"service_publish",json!({"name":"dash","reach":"public","approval":"bad"})),Err(CallError::Refused{message,..}) if message.contains("tailnet"))
    );
    assert_eq!(commands(&rig).len(), before);
    assert!(
        fs::read_to_string(rig.dir.join(AUDIT_FILE))
            .unwrap()
            .contains("refused")
    );
}
#[test]
fn g5_publish_refuses_own_or_missing_serve_name_before_exec() {
    for host in [None, Some("arca-dash.example.ts.net")] {
        let (mut rig, key) = signed_rig();
        system(&rig, "dash");
        configure(&mut rig, host, Some("example"));
        assert!(publish(&rig, &key, "dash").is_err());
        assert!(commands(&rig).is_empty());
    }
}
#[test]
fn g5_serve_host_is_process_config_and_never_looked_up() {
    let (mut rig, key) = signed_rig();
    system(&rig, "dash");
    configure(&mut rig, Some("arca-dash.example.ts.net"), Some("example"));
    let before = rig.fake.requests().len();
    assert!(publish(&rig, &key, "dash").is_err());
    assert!(
        rig.fake.requests()[before..]
            .iter()
            .all(|r| !r.path.contains("tailscale") && !r.path.ends_with("/1.0"))
    );
    assert!(commands(&rig).is_empty());
}
#[test]
fn g5_publish_preconditions_leave_approval_unspent() {
    for damage in ["template", "owner", "published", "key", "stopped"] {
        let (rig, key) = signed_rig();
        system(&rig, "dash");
        match damage {
            "template" => {
                let mut t: Value = serde_json::from_str(
                    &fs::read_to_string(rig.dir.join("templates/dash.json")).unwrap(),
                )
                .unwrap();
                t["publishable"] = json!(false);
                rig.template("dash", t);
            }
            "owner" => rig.edit(
                "PATCH",
                &format!("/1.0/instances/dash?project={PROJECT}"),
                json!({"config":{"user.arcakernel.entry":"another"}}),
            ),
            "published" => {
                let mut cfg = rig.service_config("dash");
                cfg.published = Some(service::Published {
                    node: "arca-dash".into(),
                    reach: "tailnet".into(),
                    port: 8443,
                });
                rig.kernel
                    .lock()
                    .unwrap()
                    .propose(
                        vec![arca::desired_store::EntryChange::Put(
                            arca::handlers::service_entry(
                                "service-dash",
                                "arca.service",
                                serde_json::to_value(cfg).unwrap(),
                            ),
                        )],
                        NOW,
                    )
                    .unwrap();
            }
            "key" => {
                fs::remove_dir_all(rig.dir.join("secrets/service-dash")).unwrap();
            }
            _ => rig.edit(
                "PUT",
                &format!("/1.0/instances/dash/state?project={PROJECT}"),
                json!({"action":"stop","force":true}),
            ),
        }
        assert!(publish(&rig, &key, "dash").is_err(), "{damage}");
        assert!(commands(&rig).is_empty(), "{damage}");
        assert!(
            rig.kernel
                .lock()
                .unwrap()
                .check_approval(
                    arca_approval::ArcaAction::Publish,
                    "dash",
                    &json!({"name":"dash","reach":"tailnet"}),
                    &approval(&key, "dash"),
                    NOW
                )
                .is_ok(),
            "{damage}"
        );
    }
}
#[test]
fn g5_valid_approval_joins_serves_8443_and_records_in_order() {
    {
        let (rig, _) = signed_rig();
        system(&rig, "dash");
        script(&rig, C::TailscaleVersion, 0, "");
        assert!(
            op(
                &rig,
                "service_publish",
                json!({"name":"dash","reach":"tailnet","approval":"bad"})
            )
            .is_err()
        );
        assert_eq!(commands(&rig), vec![C::TailscaleVersion.argv()]);
        assert!(rig.service_config("dash").published.is_none());
    }
    let (rig, key) = signed_rig();
    system(&rig, "dash");
    publish_scripts(&rig, "dash", &node_status("dash"), &serving("dash"));
    let answer = publish(&rig, &key, "dash").unwrap();
    assert_eq!(
        answer,
        json!({"node":"arca-dash","url":"https://arca-dash.example.ts.net:8443/"})
    );
    let cmds = commands(&rig);
    assert_eq!(cmds.len(), 9);
    assert_eq!(cmds[0], C::TailscaleVersion.argv());
    assert_eq!(
        cmds[4],
        C::Join(TailnetNode::new("arca-dash").unwrap()).argv()
    );
    assert_eq!(cmds[5], C::ClearKeyDir.argv());
    assert_eq!(cmds[8], C::ServeStatus.argv());
    assert_eq!(
        instance(&rig, "dash")["config"][service::PUBLISHED_KEY],
        "arca-dash"
    );
    assert!(rig.service_config("dash").published.is_some());
    assert_eq!(rig.fake.recordings(), 0);
    assert!(
        rig.kernel
            .lock()
            .unwrap()
            .check_approval(
                arca_approval::ArcaAction::Publish,
                "dash",
                &json!({"name":"dash","reach":"tailnet"}),
                &approval(&key, "dash"),
                NOW
            )
            .is_err()
    );
}
#[test]
fn g5_serve_commands_are_additive_and_cannot_express_reset_funnel_or_other_ports() {
    let (rig, key) = signed_rig();
    system(&rig, "dash");
    publish_scripts(&rig, "dash", &node_status("dash"), &serving("dash"));
    publish(&rig, &key, "dash").unwrap();
    unpublish_scripts(&rig, true, true);
    op(&rig, "service_unpublish", json!({"name":"dash"})).unwrap();
    let serves: Vec<_> = commands(&rig)
        .into_iter()
        .filter(|a| a.get(5).is_some_and(|v| v == "serve") && !a.contains(&"status".to_string()))
        .collect();
    assert_eq!(
        serves,
        vec![
            C::ServeOn(DashPort::new(3080).unwrap()).argv(),
            C::ServeOff.argv()
        ]
    );
}
#[test]
fn g5_wrong_node_tag_or_extra_serve_rolls_back_and_spends_approval() {
    for wrong in [
        "node",
        "tag",
        "port",
        "forward",
        "funnel",
        "foreground",
        "handler",
    ] {
        let (rig, key) = signed_rig();
        system(&rig, "dash");
        let mut status: Value = serde_json::from_str(&node_status("dash")).unwrap();
        let mut serve = serving("dash");
        match wrong {
            "node" => status["Self"]["DNSName"] = json!("arca-other.example.ts.net"),
            "tag" => status["Self"]["Tags"] = json!(["tag:other"]),
            "port" => serve["TCP"]["443"] = json!({"HTTPS":true}),
            "forward" => serve["TCP"]["8443"]["TCPForward"] = json!("localhost:1"),
            "funnel" => serve["AllowFunnel"] = json!({"x":true}),
            "foreground" => serve["Foreground"] = json!({"x":{}}),
            _ => {
                serve["Web"]["arca-dash.example.ts.net:8443"]["Handlers"]["/extra"] =
                    json!({"Proxy":"http://127.0.0.1:1"})
            }
        }
        if wrong == "node" || wrong == "tag" {
            for c in [
                C::TailscaleVersion,
                C::StartDaemon,
                C::ClearKeyDir,
                C::MakeKeyDir,
                C::Join(TailnetNode::new("arca-dash").unwrap()),
                C::ClearKeyDir,
            ] {
                script(&rig, c, 0, "");
            }
            script(&rig, C::TailnetStatus, 0, &status.to_string());
        } else {
            publish_scripts(&rig, "dash", &status.to_string(), &serve);
        }
        cleanup_scripts(&rig);
        let error = publish(&rig, &key, "dash").unwrap_err();
        assert!(
            matches!(error,CallError::Refused{message,..} if message.contains("approval is spent")),
            "{wrong}"
        );
        assert!(commands(&rig).ends_with(&[
            C::ServeOff.argv(),
            C::Logout.argv(),
            C::ClearKeyDir.argv(),
            C::StopDaemon.argv()
        ]));
        assert!(rig.service_config("dash").published.is_none());
    }
}
#[test]
fn g5_authkey_travels_only_in_files_body_and_is_redacted_from_failures() {
    let (rig, key) = signed_rig();
    system(&rig, "dash");
    for c in [
        C::TailscaleVersion,
        C::StartDaemon,
        C::ClearKeyDir,
        C::MakeKeyDir,
    ] {
        script(&rig, c, 0, "");
    }
    let mut join = ExecScript::new(
        C::Join(TailnetNode::new("arca-dash").unwrap()).argv(),
        1,
        "",
    );
    join.stderr = b"tskey-synthetic-example tskey-other-untrusted".to_vec();
    rig.fake.script_exec(join);
    script(&rig, C::ClearKeyDir, 0, "");
    cleanup_scripts(&rig);
    let error = publish(&rig, &key, "dash").unwrap_err();
    assert!(!format!("{error:?}").contains("tskey-"));
    assert!(format!("{error:?}").contains("untrusted"));
    for request in rig.fake.service_requests() {
        if !request.path.contains("/files?") {
            assert!(!String::from_utf8_lossy(&request.body).contains("tskey-"));
        }
    }
    for f in ["journal.jsonl", AUDIT_FILE] {
        assert!(
            !fs::read_to_string(rig.dir.join(f))
                .unwrap()
                .contains("tskey-")
        );
    }
    assert_eq!(rig.fake.recordings(), 0);
    assert!(rig.fake.service_files().is_empty());
}
#[test]
fn g5_unpublish_clears_only_after_confirmed_logout() {
    let (rig, key) = signed_rig();
    system(&rig, "dash");
    publish_scripts(&rig, "dash", &node_status("dash"), &serving("dash"));
    publish(&rig, &key, "dash").unwrap();
    unpublish_scripts(&rig, true, false);
    assert!(
        matches!(op(&rig,"service_unpublish",json!({"name":"dash"})),Err(CallError::Refused{message,..}) if message.contains("still published"))
    );
    assert!(rig.service_config("dash").published.is_some());
    unpublish_scripts(&rig, true, true);
    assert_eq!(
        op(&rig, "service_unpublish", json!({"name":"dash"})).unwrap(),
        json!({"unpublished":true})
    );
    assert!(rig.service_config("dash").published.is_none());
    assert!(
        instance(&rig, "dash")["config"]
            .get(service::PUBLISHED_KEY)
            .is_none()
    );
}
#[test]
fn g5_unpublish_finishes_a_partial_publish_without_a_record() {
    let (rig, _) = signed_rig();
    system(&rig, "dash");
    unpublish_scripts(&rig, false, true);
    op(&rig, "service_unpublish", json!({"name":"dash"})).unwrap();
    assert!(commands(&rig).contains(&C::Logout.argv()));
}
#[test]
fn g5_key_directory_is_private_and_removed_after_join_failure() {
    let (rig, key) = signed_rig();
    system(&rig, "dash");
    for c in [
        C::TailscaleVersion,
        C::StartDaemon,
        C::ClearKeyDir,
        C::MakeKeyDir,
    ] {
        script(&rig, c, 0, "");
    }
    script(&rig, C::Join(TailnetNode::new("arca-dash").unwrap()), 1, "");
    script(&rig, C::ClearKeyDir, 0, "");
    cleanup_scripts(&rig);
    assert!(publish(&rig, &key, "dash").is_err());
    let cmds = commands(&rig);
    let i = cmds
        .iter()
        .position(|c| c == &C::Join(TailnetNode::new("arca-dash").unwrap()).argv())
        .unwrap();
    assert_eq!(cmds[i + 1], C::ClearKeyDir.argv());
    assert_eq!(cmds[i - 1], C::MakeKeyDir.argv());
    assert!(rig.fake.service_files().is_empty());
    assert!(
        cmds.iter()
            .flatten()
            .all(|arg| arg != "sh" && arg != "bash")
    );
}
#[test]
fn g5_tailnet_is_pinned_and_invalid_serve_names_refuse_before_exec() {
    let (rig, key) = signed_rig();
    system(&rig, "dash");
    for c in [
        C::TailscaleVersion,
        C::StartDaemon,
        C::ClearKeyDir,
        C::MakeKeyDir,
        C::Join(TailnetNode::new("arca-dash").unwrap()),
        C::ClearKeyDir,
    ] {
        script(&rig, c, 0, "");
    }
    script(
        &rig,
        C::TailnetStatus,
        0,
        &node_status("dash").replace("example.ts.net", "other.ts.net"),
    );
    cleanup_scripts(&rig);
    assert!(publish(&rig, &key, "dash").is_err());
    assert!(commands(&rig).contains(&C::Logout.argv()));
    for (host, tailnet) in [
        (Some("not-a-tailnet.example"), Some("example")),
        (Some("host.example.ts.net"), None),
    ] {
        let (mut rig, key) = signed_rig();
        system(&rig, "dash");
        configure(&mut rig, host, tailnet);
        assert!(publish(&rig, &key, "dash").is_err());
        assert!(commands(&rig).is_empty());
    }
}
#[test]
fn service_remove_logs_a_running_services_node_out_before_it_deletes_it() {
    for failure in ["none", "exit", "post", "held"] {
        let (rig, key) = signed_rig();
        system(&rig, "dash");
        let before = rig.fake.requests().len();
        if failure == "none" {
            unpublish_scripts(&rig, false, true);
        } else {
            let mut s = ExecScript::new(
                C::StartDaemon.argv(),
                if failure == "exit" { 1 } else { 0 },
                "",
            );
            if failure == "post" {
                s.post_status = 500;
            }
            if failure == "held" {
                s.held = true;
            }
            rig.fake.script_exec(s);
        }
        // No clock is read here. A held logout never ends, and the removal gives it up at its own deadline: that the
        // removal answers at all while the logout's exec is still held is the bound. How long it took is that
        // deadline plus the machine's speed, on a slow disk seconds more, and says nothing else about the node.
        assert_eq!(
            op(
                &rig,
                "service_remove",
                json!({"name":"dash","approval":owner_approval(&key,"dash")})
            )
            .unwrap(),
            json!({"removed":true})
        );
        if failure == "held" {
            let operations = rig
                .get("/1.0/operations?all-projects=true&recursion=1")
                .unwrap();
            assert_eq!(
                operations["running"].as_array().map(Vec::len),
                Some(1),
                "the logout's exec was still held when the removal answered"
            );
        }
        let requests = &rig.fake.requests()[before..];
        let exec = requests
            .iter()
            .position(|r| r.path.contains("/exec?") && r.method == "POST")
            .unwrap();
        let write = requests
            .iter()
            .position(|r| {
                r.method != "GET"
                    && !r.path.contains("/exec?")
                    && !r.path.contains("/logs/exec-output/")
            })
            .unwrap();
        assert!(exec < write, "{failure}");
        assert!(matches!(
            rig.kernel.lock().unwrap().spend_service_removal(
                &EntryId("service-dash".into()),
                "dash",
                &json!({"name": "dash"}),
                &owner_approval(&key, "dash"),
                NOW
            ),
            Err(arca::kernel::ServiceRemovalError::Approval(
                arca_approval::ApprovalRefusal::Replayed
            ))
        ));
        assert!(
            op(
                &rig,
                "service_remove",
                json!({"name":"dash","approval":owner_approval(&key,"dash")})
            )
            .is_err()
        );
    }
}
#[test]
fn g5_publish_tools_match_golden_inputs_and_operator_registration() {
    for name in ["service_publish", "service_unpublish"] {
        let v = arca_api::verb(name).unwrap();
        assert!(arca_api::may_call(Scope::Operator, v));
        assert!(arca_api::may_call(Scope::Doctor, v));
        assert!(!arca_api::may_call(Scope::Viewer, v));
        assert!(!v.destructive);
        assert!(v.writes);
        let schema = (v.params)();
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(
            schema["properties"]["name"]["pattern"],
            "^[a-z](?:[a-z0-9-]{0,38}[a-z0-9])?$"
        );
    }
    let p = (arca_api::verb("service_publish").unwrap().params)();
    assert_eq!(p["required"], json!(["name", "reach", "approval"]));
    assert_eq!(p["properties"]["approval"]["maxLength"], 16384);
}
#[test]
fn g5_owner_slots_refuse_doctor_and_allow_operator() {
    let rig = Rig::new();
    rig.template("slots",json!({"name":"slots","kind":"oci","image":"docker.io/library/example","digest":DIGEST,"ports":[],"env":{},"secrets":["OMNI_API_KEY","telegram_bot_token","whatsapp_phone_number"]}));
    rig.call("service_create", json!({"name":"api","template":"slots"}))
        .unwrap();
    for key in [
        "OMNI_API_KEY",
        "telegram_bot_token",
        "whatsapp_phone_number",
    ] {
        assert!(
            matches!(rig.call("service_secret_set",json!({"name":"api","key":key,"value":"synthetic-owner-value"})),Err(CallError::Refused{message,..}) if message.contains(key)&&message.contains("owner's"))
        );
        assert_eq!(
            op(
                &rig,
                "service_secret_set",
                json!({"name":"api","key":key,"value":"synthetic-owner-value"})
            )
            .unwrap(),
            json!({"ok":true})
        );
    }
}
#[test]
fn g5_omni_wiring_gets_database_nats_and_own_api_key_only() {
    let rig = Rig::new();
    create_pair(&rig);
    let mut n = nats_template();
    n["provides"] = json!({"env":{"NATS_URL":"nats://{{host}}:{{port}}"}});
    rig.template("nats", n);
    rig.call("service_create", json!({"name":"bus","template":"nats"}))
        .unwrap();
    rig.template("omni",json!({"name":"omni","kind":"oci","image":"docker.io/library/example","digest":DIGEST,"ports":[8882],"env":{"OMNI_API_KEY":"{{secret:OMNI_API_KEY}}"},"secrets":["OMNI_API_KEY"],"generate":{"OMNI_API_KEY":"omni_sk_"}}));
    rig.call("service_create", json!({"name":"api","template":"omni"}))
        .unwrap();
    let own = instance(&rig, "api")["config"]["environment.OMNI_API_KEY"].clone();
    for to in ["db", "bus"] {
        op(&rig, "service_connect", json!({"from":"api","to":to})).unwrap();
    }
    let live = instance(&rig, "api");
    assert_eq!(live["config"]["environment.OMNI_API_KEY"], own);
    assert!(
        live["config"]["environment.DATABASE_URL"]
            .as_str()
            .unwrap()
            .starts_with("postgresql://")
    );
    assert!(
        live["config"]["environment.NATS_URL"]
            .as_str()
            .unwrap()
            .starts_with("nats://")
    );
    assert!(live["config"].get("environment.PASSWORD").is_none());
}
#[test]
fn g5_file_request_is_root_0600_and_exec_environment_adds_nothing() {
    let (rig, key) = signed_rig();
    system(&rig, "dash");
    publish_scripts(&rig, "dash", &node_status("dash"), &serving("dash"));
    publish(&rig, &key, "dash").unwrap();
    let calls = rig.fake.service_requests();
    let files: Vec<_> = calls
        .iter()
        .filter(|r| r.path.contains("/files?"))
        .collect();
    assert_eq!(files.len(), 1);
    let file = files[0];
    assert_eq!(file.body, b"tskey-synthetic-example");
    for (k, v) in [
        ("x-incus-mode", "0600"),
        ("x-incus-uid", "0"),
        ("x-incus-gid", "0"),
        ("x-incus-type", "file"),
        ("x-incus-write", "overwrite"),
    ] {
        assert!(file.headers.contains(&(k.into(), v.into())));
    }
    for request in calls.iter().filter(|r| r.path.contains("/exec?")) {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["environment"], json!({}));
        assert_eq!(body["record-output"], true);
        assert_eq!(body["wait-for-websocket"], false);
        assert_eq!(body["interactive"], false);
        assert_eq!(body["user"], 0);
        assert_eq!(body["group"], 0);
        assert_eq!(body["cwd"], "");
        assert_eq!(body["command"][0], "/usr/bin/timeout");
    }
}
#[test]
fn g5_config_patch_uses_etag_removes_managed_keys_and_keeps_the_mark() {
    let rig = Rig::new();
    create_pair(&rig);
    op(&rig, "service_connect", json!({"from":"app","to":"db"})).unwrap();
    let before = instance(&rig, "app");
    op(&rig, "service_disconnect", json!({"from":"app","to":"db"})).unwrap();
    let after = instance(&rig, "app");
    assert_eq!(
        before["config"]["user.arcakernel.entry"],
        after["config"]["user.arcakernel.entry"]
    );
    assert_eq!(after["config"][service::TEMPLATE_KEY], "consumer");
    assert!(after["config"].get("environment.DATABASE_URL").is_none());
    let patches = rig
        .fake
        .service_requests()
        .into_iter()
        .filter(|r| r.path.starts_with("/1.0/instances/app?") && r.method == "PATCH")
        .collect::<Vec<_>>();
    assert!(!patches.is_empty());
    assert!(patches.iter().all(|r| {
        r.headers
            .iter()
            .any(|(k, v)| k == "if-match" && !v.is_empty())
    }));
    let writes = rig.fake.writes().len();
    let cfg = rig.service_config("app");
    rig.apply(&cfg).unwrap();
    assert_eq!(rig.fake.writes().len(), writes);
}

#[test]
fn g5_adopted_secret_set_refuses_without_a_write_or_exec() {
    let rig = Rig::with(
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![adopted_instance(
                "legacy",
                "arca",
                "provider",
                "192.0.2.10",
                "5432",
                "uuid-legacy",
            )],
            networks: vec![],
            pools: vec![],
        },
        vec![PROJECT.into(), "arca".into()],
        Some("192.0.2.0/24"),
    );
    rig.template("provider",json!({"name":"provider","kind":"oci","image":"docker.io/library/example","digest":DIGEST,"ports":[5432],"env":{"PASSWORD":"{{secret:PASSWORD}}"},"secrets":["PASSWORD"]}));
    rig.call("service_list", json!({})).unwrap();
    let before = rig.fake.writes().len();
    assert_eq!(
        op(
            &rig,
            "service_secret_set",
            json!({"name":"legacy","key":"PASSWORD","value":"synthetic-password"})
        ),
        Err(CallError::Refused {
            code: "adopted".into(),
            message: "service legacy was adopted; this build does not write to it".into()
        })
    );
    assert_eq!(rig.fake.writes().len(), before);
    assert!(commands(&rig).is_empty());
}

#[test]
fn g5_exec_each_ownership_gate_refuses_before_post() {
    for damage in [
        "adopted",
        "adopted_other_project",
        "target",
        "oci",
        "no_uuid",
        "uuid",
        "tombstone",
        "retained",
        "foreign",
    ] {
        let (rig, _) = signed_rig();
        system(&rig, "dash");
        let (seam, mut target) = seam(&rig, "dash");
        let expected = match damage {
            "adopted" => {
                target.service.adopted = true;
                "adopted"
            }
            "adopted_other_project" => {
                target.service.adopted = true;
                target.service.project = "arca".into();
                "adopted"
            }
            "target" => {
                target.service.instance = "other".into();
                "target"
            }
            "oci" => {
                target.service.source["protocol"] = json!("oci");
                "oci"
            }
            "no_uuid" => {
                target.uuid = None;
                "uuid"
            }
            "uuid" => {
                target.uuid = Some("different".into());
                "owner"
            }
            "tombstone" => {
                rig.edit(
                    "PATCH",
                    &format!("/1.0/instances/dash?project={PROJECT}"),
                    json!({"config":{"user.arcakernel.retiring":"a".repeat(64)}}),
                );
                "tombstone"
            }
            "retained" => {
                target.retained.insert(
                    target.entry.clone(),
                    (
                        arca_kernel::desired::PluginId("arca.service".into()),
                        Digest::of(b"plan"),
                    ),
                );
                "owner"
            }
            _ => {
                rig.edit(
                    "PATCH",
                    &format!("/1.0/instances/dash?project={PROJECT}"),
                    json!({"config":{"user.arcakernel.entry":"other"}}),
                );
                "owner"
            }
        };
        assert!(
            matches!(run(&seam,&target,&C::Journal(JournalLines::new(1).unwrap())),Err(ExecError::Refused(code)) if code==expected),
            "{damage}"
        );
        assert!(commands(&rig).is_empty(), "{damage}");
    }
}
#[test]
fn g5_uuid_swap_drops_output_and_never_reads_the_console() {
    for damage in ["success", "exit", "over_cap"] {
        let (rig, _) = signed_rig();
        system(&rig, "dash");
        let mut s = ExecScript::new(
            C::Journal(JournalLines::new(1).unwrap()).argv(),
            0,
            "wrong instance output",
        );
        s.swap_uuid = true;
        if damage == "exit" {
            s.exit = 1;
        }
        if damage == "over_cap" {
            s.stdout = vec![b'x'; 65537];
        }
        rig.fake.script_exec(s);
        rig.fake
            .set_console_log(PROJECT, "dash", "wrong instance console");
        assert!(
            matches!(rig.call("service_logs",json!({"name":"dash","lines":1})),Err(CallError::Refused{code,..}) if code=="instance_changed")
        );
        assert!(
            rig.fake
                .requests()
                .iter()
                .all(|r| !r.path.contains("/console?"))
        );
        assert_eq!(rig.fake.recordings(), 0);
    }
}

#[test]
fn g5_system_journal_and_capped_fallback_delete_both_recordings() {
    let (rig, _) = signed_rig();
    system(&rig, "dash");
    script(
        &rig,
        C::Journal(JournalLines::new(2).unwrap()),
        0,
        "one\ntwo\n",
    );
    assert_eq!(
        rig.call("service_logs", json!({"name":"dash","lines":2}))
            .unwrap(),
        json!({"lines":["one","two"]})
    );
    rig.fake
        .set_console_log(PROJECT, "dash", "console fallback");
    let mut oversized = ExecScript::new(C::Journal(JournalLines::new(2).unwrap()).argv(), 0, "");
    oversized.stdout = vec![b'x'; 65537];
    rig.fake.script_exec(oversized);
    assert_eq!(
        rig.call("service_logs", json!({"name":"dash","lines":2}))
            .unwrap(),
        json!({"lines":["console fallback"]})
    );
    assert_eq!(rig.fake.recordings(), 0);
    let deletes = rig
        .fake
        .service_requests()
        .iter()
        .filter(|r| r.method == "DELETE")
        .count();
    assert_eq!(deletes, 4);
}
#[test]
fn g5_audit_failure_and_bad_lines_send_no_exec() {
    let (rig, _) = signed_rig();
    system(&rig, "dash");
    let (seam, target) = seam(&rig, "dash");
    let path = rig.dir.join(AUDIT_FILE);
    let _ = fs::remove_file(&path);
    fs::create_dir(&path).unwrap();
    assert!(matches!(
        run(&seam, &target, &C::TailscaleVersion),
        Err(ExecError::Audit)
    ));
    assert!(commands(&rig).is_empty());
    for lines in [json!(0), json!(201), json!(-1), json!("2"), json!(2.5)] {
        assert!(matches!(
            rig.call("service_logs", json!({"name":"dash","lines":lines})),
            Err(CallError::Invalid(_))
        ));
    }
    assert!(JournalLines::new(0).is_err());
    assert!(JournalLines::new(201).is_err());
    assert!(commands(&rig).is_empty());
}
#[test]
fn g5_hung_mark_allows_key_cleanup_and_stop_and_clears_on_uuid_change() {
    let (rig, _) = signed_rig();
    system(&rig, "dash");
    let (seam, mut target) = seam(&rig, "dash");
    let join = C::Join(TailnetNode::new("arca-dash").unwrap());
    let mut held = ExecScript::new(join.argv(), 0, "");
    held.held = true;
    rig.fake.script_exec(held);
    let waited = waits(&rig);
    // The deadline covers every request the node sends before it waits on the join, so it is seconds and not a tenth
    // of one: on a slow machine a tenth passed before the wait was sent, and the node then sent fewer requests. While
    // it waits, the fake holds each wait for a quarter of a second, answers that the join still runs, and the node asks
    // again. How often is not the node's choice, so this rig's golden log leaves the waits out (see `WITHOUT_WAITS`).
    assert!(
        seam.run(
            &target,
            "operator",
            "service_publish",
            &join,
            &[],
            Some(Instant::now() + Duration::from_secs(3))
        )
        .is_err()
    );
    assert!(
        commands(&rig).contains(&join.argv()),
        "the join was started"
    );
    // The fake writes a wait down when it answers it, a quarter of a second after it arrived. A wait the node sent
    // just before its deadline is therefore not in the log yet when the node has given up.
    let written = Instant::now() + Duration::from_secs(1);
    while waits(&rig) == waited && Instant::now() < written {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        waits(&rig) > waited,
        "the join was waited on before its deadline passed"
    );
    assert!(matches!(
        run(&seam, &target, &C::TailscaleVersion),
        Err(ExecError::Refused("hung"))
    ));
    for c in [C::ClearKeyDir, C::StopDaemon] {
        script(&rig, c.clone(), 0, "");
        assert!(run(&seam, &target, &c).is_ok());
    }
    rig.edit(
        "PATCH",
        &format!("/1.0/instances/dash?project={PROJECT}"),
        json!({"config":{"volatile.uuid":"new-identity"}}),
    );
    target.uuid = Some("new-identity".into());
    script(&rig, C::TailscaleVersion, 0, "");
    assert!(run(&seam, &target, &C::TailscaleVersion).is_ok());
}
#[test]
fn g5_journal_reads_have_exactly_four_nonblocking_permits() {
    let (rig, _) = signed_rig();
    system(&rig, "dash");
    let (seam, _) = seam(&rig, "dash");
    let permits: Vec<_> = (0..4).map(|_| seam.journal_permit().unwrap()).collect();
    assert!(seam.journal_permit().is_none());
    drop(permits);
    assert!(seam.journal_permit().is_some());
}
#[test]
fn g5_startup_sweep_names_only_valid_unfinished_audit_operations() {
    let (rig, _) = signed_rig();
    system(&rig, "dash");
    let (engine, target) = seam(&rig, "dash");
    let writer = engine.writer(&target, Duration::from_secs(3)).unwrap();
    let owned = engine.check(&target, &writer).unwrap();
    script(&rig, C::TailscaleVersion, 0, "leftover");
    let id = writer
        .start_service_exec(&owned, &C::TailscaleVersion, Duration::from_secs(3))
        .unwrap();
    assert_eq!(rig.fake.recordings(), 2);
    fs::write(
        rig.dir.join(AUDIT_FILE),
        format!(
            "{}\n{}\n{}\n",
            json!({"kind":"op","id":1,"op":id,"instance":"dash","project":PROJECT}),
            json!({"kind":"op","id":2,"op":"../bad","instance":"dash","project":PROJECT}),
            json!({"kind":"op","id":3,"op":id,"instance":"dash","project":"arca"})
        ),
    )
    .unwrap();
    fs::set_permissions(rig.dir.join(AUDIT_FILE), fs::Permissions::from_mode(0o600)).unwrap();
    drop(engine);
    let (_engine, _) = seam(&rig, "dash");
    assert_eq!(rig.fake.recordings(), 0);
    let audit = fs::read_to_string(rig.dir.join(AUDIT_FILE)).unwrap();
    assert!(audit.contains("\"outcome\":\"swept\""));
    let before = rig.fake.requests().len();
    let (_engine, _) = seam(&rig, "dash");
    assert_eq!(rig.fake.requests().len(), before);
}
#[test]
fn g5_bad_removal_approval_runs_no_exec_and_other_verbs_answer_during_logout() {
    let (rig, key) = signed_rig();
    system(&rig, "dash");
    assert!(
        op(
            &rig,
            "service_remove",
            json!({"name":"dash","approval":"bad"})
        )
        .is_err()
    );
    assert!(commands(&rig).is_empty());
    let mut held = ExecScript::new(C::StartDaemon.argv(), 0, "");
    held.held = true;
    rig.fake.script_exec(held);
    let handler = rig.handlers["service_remove"].clone();
    let proof = owner_approval(&key, "dash");
    let thread = std::thread::spawn(move || {
        handler.call(
            json!({"name":"dash","approval":proof}),
            &Caller {
                token_id: "operator".into(),
                scope: Scope::Operator,
                tools: None,
            },
        )
    });
    let until = Instant::now() + Duration::from_secs(3);
    while commands(&rig).is_empty() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!commands(&rig).is_empty());
    assert_eq!(
        rig.call("service_status", json!({"name":"dash"})).unwrap()["state"],
        "running"
    );
    // The status answered while the node was still being logged out: the audit holds the logout's exec as started and
    // not as ended. A status that had waited for the logout would find its end written, whatever the clock says.
    assert!(
        exec_in_flight(&rig),
        "the logout's exec was still running when the status answered"
    );
    let op = rig
        .fake
        .service_requests()
        .into_iter()
        .find(|r| r.path.contains("/exec?"))
        .unwrap();
    drop(op);
    // Let the pending task finish without waiting out the production 60-second remove budget.
    let operations = rig
        .get("/1.0/operations?all-projects=true&recursion=1")
        .unwrap();
    let id = operations["running"].as_array().unwrap()[0]["id"]
        .as_str()
        .unwrap();
    rig.fake.release_operation(&arca_incus::OpId(id.into()));
    assert_eq!(thread.join().unwrap().unwrap(), json!({"removed":true}));
    assert!(!exec_in_flight(&rig), "every exec of the removal has ended");
}

#[test]
fn g5_publish_clears_key_directory_after_a_timed_out_join() {
    let (rig, key) = signed_rig();
    system(&rig, "dash");
    for cmd in [
        C::TailscaleVersion,
        C::StartDaemon,
        C::ClearKeyDir,
        C::MakeKeyDir,
    ] {
        script(&rig, cmd, 0, "");
    }
    let join = C::Join(TailnetNode::new("arca-dash").unwrap());
    let mut held = ExecScript::new(join.argv(), 0, "");
    held.held = true;
    rig.fake.script_exec(held);
    script(&rig, C::ClearKeyDir, 0, ""); // The hung mark must still permit this post-join cleanup.
    script(&rig, C::ClearKeyDir, 0, "");
    script(&rig, C::StopDaemon, 0, "");
    // The join's own limit ends it. The time that takes is the limit plus the machine's speed, so no clock is read.
    let error = publish(&rig, &key, "dash").unwrap_err();
    assert!(
        matches!(error,CallError::Refused{message,..} if message.contains("approval is spent"))
    );
    let commands = commands(&rig);
    let index = commands.iter().position(|c| c == &join.argv()).unwrap();
    assert_eq!(commands[index + 1], C::ClearKeyDir.argv());
    assert!(rig.fake.service_files().is_empty());
    assert_eq!(rig.fake.recordings(), 0);
    assert!(rig.service_config("dash").published.is_none());
}
#[test]
fn g5_startup_sweep_is_bounded_newest_first_and_resumes_the_rest() {
    let (rig, _) = signed_rig();
    system(&rig, "dash");
    let rows=(1..=257).map(|id|json!({"kind":"op","id":id,"op":format!("00000000-0000-4000-8000-{id:012x}"),"project":PROJECT,"instance":"dash"}).to_string()).collect::<Vec<_>>().join("\n")+"\n";
    fs::write(rig.dir.join(AUDIT_FILE), rows).unwrap();
    fs::set_permissions(rig.dir.join(AUDIT_FILE), fs::Permissions::from_mode(0o600)).unwrap();
    let (_engine, _) = seam(&rig, "dash");
    let requests = rig
        .fake
        .service_requests()
        .into_iter()
        .filter(|r| r.method == "DELETE" && r.path.contains("/exec-output/"))
        .collect::<Vec<_>>();
    assert_eq!(requests.len(), 512);
    assert!(requests[0].path.contains("000000000101"));
    assert!(requests.iter().all(|r| !r.path.contains("000000000001.")));
    let (_engine, _) = seam(&rig, "dash");
    assert_eq!(
        rig.fake
            .service_requests()
            .iter()
            .filter(|r| r.method == "DELETE" && r.path.contains("/exec-output/"))
            .count(),
        514
    );
}

#[test]
fn g5_publication_preserves_secret_updates_made_during_its_exec() {
    let (rig, key) = signed_rig();
    system(&rig, "dash");
    let mut held = ExecScript::new(C::TailscaleVersion.argv(), 0, "");
    held.held = true;
    rig.fake.script_exec(held);
    for c in [
        C::StartDaemon,
        C::ClearKeyDir,
        C::MakeKeyDir,
        C::Join(TailnetNode::new("arca-dash").unwrap()),
        C::ClearKeyDir,
    ] {
        script(&rig, c, 0, "");
    }
    script(&rig, C::TailnetStatus, 0, &node_status("dash"));
    script(&rig, C::ServeOn(DashPort::new(3080).unwrap()), 0, "");
    script(&rig, C::ServeStatus, 0, &serving("dash").to_string());
    let handler = rig.handlers["service_publish"].clone();
    let proof = approval(&key, "dash");
    let thread = std::thread::spawn(move || {
        handler.call(
            json!({"name":"dash","reach":"tailnet","approval":proof}),
            &Caller {
                token_id: "operator".into(),
                scope: Scope::Operator,
                tools: None,
            },
        )
    });
    let until = Instant::now() + Duration::from_secs(3);
    while commands(&rig).is_empty() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!commands(&rig).is_empty());
    op(
        &rig,
        "service_secret_set",
        json!({"name":"dash","key":"tailscale_authkey","value":"tskey-new-synthetic"}),
    )
    .unwrap();
    let latest = rig.service_config("dash").secret_refs;
    let ops = rig
        .get("/1.0/operations?all-projects=true&recursion=1")
        .unwrap();
    let id = ops["running"].as_array().unwrap()[0]["id"]
        .as_str()
        .unwrap();
    rig.fake.release_operation(&arca_incus::OpId(id.into()));
    assert!(thread.join().unwrap().is_ok());
    assert_eq!(rig.service_config("dash").secret_refs, latest);
}

#[test]
fn g5_secret_set_changes_only_its_key_with_other_rendered_values_unavailable() {
    let rig = Rig::new();
    rig.template("two",json!({"name":"two","kind":"oci","image":"docker.io/library/example","digest":DIGEST,"ports":[],"secrets":["FIRST","SECOND"],"env":{"FIRST":"{{secret:FIRST}}","SECOND":"{{secret:SECOND}}"}}));
    rig.call("service_create", json!({"name":"two","template":"two"}))
        .unwrap();
    for key in ["FIRST", "SECOND"] {
        op(
            &rig,
            "service_secret_set",
            json!({"name":"two","key":key,"value":format!("synthetic-{key}")}),
        )
        .unwrap();
    }
    let cfg = rig.service_config("two");
    let path = cfg.secret_refs["SECOND"]
        .strip_prefix("arca-secret:")
        .unwrap();
    fs::remove_file(rig.dir.join("secrets").join(path)).unwrap();
    let before = instance(&rig, "two")["config"].clone();
    op(
        &rig,
        "service_secret_set",
        json!({"name":"two","key":"FIRST","value":"synthetic-replacement"}),
    )
    .unwrap();
    let after = instance(&rig, "two")["config"].clone();
    assert_eq!(after["environment.FIRST"], "synthetic-replacement");
    for (key, value) in before.as_object().unwrap() {
        if key != "environment.FIRST" {
            assert_eq!(&after[key], value, "{key}");
        }
    }
}

#[test]
fn g5_failed_link_revocation_does_not_release_the_consumers_address() {
    let (rig, key) = signed_rig();
    create_pair(&rig);
    op(&rig, "service_connect", json!({"from":"app","to":"db"})).unwrap();
    rig.fake
        .set_reject_patch(&format!("/1.0/network-acls/{PROJECT}-svc-db"));
    let before = rig.fake.requests().len();
    assert!(
        op(
            &rig,
            "service_remove",
            json!({"name":"app","approval":owner_approval(&key,"app")})
        )
        .is_err()
    );
    assert!(
        rig.get(&format!("/1.0/instances/app?project={PROJECT}"))
            .is_some()
    );
    assert!(
        rig.fake.requests()[before..]
            .iter()
            .all(|r| r.method != "DELETE"
                || r.path != format!("/1.0/instances/app?project={PROJECT}"))
    );
}

#[test]
fn a_netproject_read_failure_does_not_refuse_a_service_connect() {
    let rig = Rig::new();
    create_pair(&rig);
    rig.fail_observe("net-arcak-svc");
    assert_eq!(
        op(&rig, "service_connect", json!({"from":"app","to":"db"}))
            .expect("the service update ignores the project entry's read failure"),
        json!({"env":["DATABASE_URL"]})
    );
}
