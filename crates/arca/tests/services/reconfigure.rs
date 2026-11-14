//! `service_reconfigure` against a fake Incus: a service is made what its template says now, behind one owner card
//! whose rows are exactly what is applied, and a template's `rawLxc` lines survive every reconcile. Every value is
//! synthetic.
use super::*;

/// The template the service is made from first: a plain value, one to drop later, a generated secret, and its address.
fn first_template() -> Value {
    json!({
        "name": "app",
        "kind": "oci",
        "image": "registry.example/app",
        "digest": DIGEST,
        "ports": [8080],
        "env": {
            "MODE": "one",
            "OLD_FLAG": "1",
            "API_KEY": "{{secret:API_KEY}}",
            "URL": "http://{{host}}:{{port}}"
        },
        "secrets": ["API_KEY"],
        "generate": { "API_KEY": "ak_" },
        "publishable": false
    })
}

/// The same template as it says later: one value changed, one dropped, one added, a new secret whose value is set
/// separately, a second port and an extra LXC line.
fn second_template() -> Value {
    let mut template = first_template();
    template["env"] = json!({
        "MODE": "two",
        "HOSTS": "a.example,b.example",
        "API_KEY": "{{secret:API_KEY}}",
        "NEW_TOKEN": "{{secret:NEW_TOKEN}}",
        "URL": "http://{{host}}:{{port}}"
    });
    template["secrets"] = json!(["API_KEY", "NEW_TOKEN"]);
    template["ports"] = json!([8080, 9090]);
    template["rawLxc"] = json!(["lxc.sched.core=0"]);
    template
}

/// A node whose owner holds `key`, with low-level container config allowed, and `app` made from the first template.
fn node(key: &common::SkKey) -> Rig {
    let rig = Rig::with_key(
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![],
            networks: vec![],
            pools: vec![("pool1".into(), json!({}))],
        },
        vec![PROJECT.to_string()],
        Some("192.0.2.0/24"),
        Some(key),
    );
    rig.template("app", first_template());
    rig.call(
        "service_create",
        json!({ "name": "app", "template": "app" }),
    )
    .expect("create");
    rig.allow_low_level();
    rig
}

fn instance(rig: &Rig, name: &str) -> Value {
    rig.get(&format!("/1.0/instances/{name}?project={PROJECT}"))
        .expect("instance")
}

fn propose(rig: &Rig, args: Value) -> Result<Value, CallError> {
    propose_action(rig, "service_reconfigure", args)
}

fn propose_action(rig: &Rig, action: &str, args: Value) -> Result<Value, CallError> {
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
    .collect::<HashMap<_, _>>()["action_propose"]
        .call(json!({ "action": action, "args": args }), &doctor())
}

/// Each approval in these tests is signed with a nonce of its own.
static NONCE: AtomicU64 = AtomicU64::new(0x6000);

/// The owner approves the proposal and the approved action runs through the node's own handlers.
fn approve_and_run(rig: &Rig, key: &common::SkKey, proposed: &Value) -> Result<Value, CallError> {
    let id = proposed["id"].as_str().expect("an id");
    let statement = arca::handlers::action_statement(
        "node1",
        id,
        "service_reconfigure",
        proposed["argsDigest"].as_str().expect("a digest"),
        NOW,
        &format!("{:032x}", NONCE.fetch_add(1, Ordering::SeqCst)),
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

/// The generated secret the service holds, read from its file.
fn api_key(rig: &Rig) -> String {
    fs::read_to_string(rig.dir.join("secrets/service-app/api-key")).expect("the generated secret")
}

fn row<'a>(details: &'a Value, label: &str) -> Option<&'a str> {
    details
        .as_array()
        .expect("details")
        .iter()
        .find(|row| row["label"] == label)
        .and_then(|row| row["value"].as_str())
}

#[test]
fn the_card_shows_exactly_the_difference_that_is_applied_and_never_a_secret() {
    let key = common::SkKey::from_seed(41);
    let rig = node(&key);
    let secret = api_key(&rig);
    let before = instance(&rig, "app");
    assert_eq!(before["config"]["environment.OLD_FLAG"], "1");
    assert!(
        before["config"]["environment.API_KEY"].as_str() == Some(secret.as_str()),
        "the instance holds the generated secret"
    );
    rig.template("app", second_template());

    let proposed = propose(&rig, json!({ "name": "app" })).expect("the proposal is recorded");
    let changes = proposed["args"]["changes"].clone();
    assert_eq!(
        changes,
        json!({
            "environment": [
                { "key": "OLD_FLAG", "change": "removed" },
                { "key": "MODE", "change": "changed", "value": "two", "was": "one" },
                { "key": "HOSTS", "change": "added", "value": "a.example,b.example" }
            ],
            "secrets": [{ "name": "NEW_TOKEN", "change": "declared" }],
            "ports": { "from": [8080], "to": [8080, 9090] },
            "rawLxc": { "from": [], "to": ["lxc.sched.core=0"] }
        }),
        "{proposed}"
    );
    let details = &proposed["details"];
    assert_eq!(row(details, "Env OLD_FLAG"), Some("removed"));
    assert_eq!(row(details, "Env MODE"), Some("changed: two (was: one)"));
    assert_eq!(
        row(details, "Env HOSTS"),
        Some("added: a.example,b.example")
    );
    assert_eq!(
        row(details, "Secret NEW_TOKEN"),
        Some("added: secret, value set separately")
    );
    assert_eq!(row(details, "Ports"), Some("8080, 9090 (was: 8080)"));
    assert_eq!(
        row(details, "Low-level container settings (raw.lxc)"),
        Some("lxc.sched.core=0 (was: none)")
    );
    assert_eq!(
        row(details, "Changes"),
        Some(
            "environment values 1 added, 1 changed, 1 removed; 1 secret; its ports; its low-level container settings"
        )
    );
    assert!(
        row(details, "Env API_KEY").is_none(),
        "a value that does not change has no row"
    );
    assert_eq!(
        instance(&rig, "app")["config"]["environment.MODE"],
        "one",
        "nothing runs until it is approved"
    );

    let done = approve_and_run(&rig, &key, &proposed).expect("the approved reconfigure runs");
    assert_eq!(done["reconfigured"], true, "{done}");
    assert_eq!(done["changes"], changes, "what ran is what the card showed");
    assert_eq!(done["restarted"], true);
    assert_eq!(done["readBack"], json!({ "matches": true, "differs": [] }));

    let after = instance(&rig, "app")["config"].clone();
    assert_eq!(after["environment.MODE"], "two");
    assert_eq!(after["environment.HOSTS"], "a.example,b.example");
    assert!(
        after.get("environment.OLD_FLAG").is_none(),
        "the key the template dropped is gone"
    );
    assert!(
        after.get("environment.NEW_TOKEN").is_none(),
        "a secret whose value is set separately gives no value yet"
    );
    assert!(
        after["environment.API_KEY"].as_str() == Some(secret.as_str()),
        "the secret it held is kept"
    );
    assert_eq!(after["raw.lxc"], "lxc.sched.core=0");
    assert_eq!(after[service::RAW_LXC_KEY], "lxc.sched.core=0");
    assert_eq!(after["user.arca.ports"], "8080,9090");
    let record = rig.service_config("app");
    assert_eq!(record.ports, vec![8080, 9090]);
    assert_eq!(record.raw_lxc, vec!["lxc.sched.core=0".to_string()]);

    // No secret's value reaches the card, the answer or the journal.
    let journal = fs::read_to_string(rig.dir.join("service-updates.jsonl")).expect("the journal");
    for (what, text) in [
        ("the proposal", proposed.to_string()),
        ("the answer", done.to_string()),
        ("the journal", journal.clone()),
    ] {
        assert!(!text.contains(&secret), "{what} holds a secret's value");
    }
    assert!(
        journal.contains("removed OLD_FLAG"),
        "the journal names keys"
    );

    // Asked again, nothing is left to change but the secret whose value is set separately.
    let again = propose(&rig, json!({ "name": "app" })).expect("a second proposal");
    assert_eq!(
        again["args"]["changes"],
        json!({ "secrets": [{ "name": "NEW_TOKEN", "change": "declared" }] })
    );
}

#[test]
fn a_template_that_drops_an_environment_key_removes_it_from_the_running_service() {
    let key = common::SkKey::from_seed(42);
    let rig = node(&key);
    let mut dropped = first_template();
    dropped["env"]
        .as_object_mut()
        .expect("env")
        .remove("OLD_FLAG");
    rig.template("app", dropped);
    let proposed = propose(&rig, json!({ "name": "app" })).expect("proposed");
    assert_eq!(
        proposed["args"]["changes"],
        json!({ "environment": [{ "key": "OLD_FLAG", "change": "removed" }] })
    );
    approve_and_run(&rig, &key, &proposed).expect("runs");
    let config = instance(&rig, "app")["config"].clone();
    assert!(config.get("environment.OLD_FLAG").is_none(), "{config}");
    assert!(
        !config["user.arca.envkeys"]
            .as_str()
            .expect("envkeys")
            .split(',')
            .any(|key| key == "OLD_FLAG")
    );
    assert!(
        !rig.service_config("app")
            .environment
            .contains_key("OLD_FLAG")
    );
}

#[test]
fn a_card_made_before_the_template_changed_again_applies_nothing() {
    let key = common::SkKey::from_seed(43);
    let rig = node(&key);
    rig.template("app", second_template());
    let proposed = propose(&rig, json!({ "name": "app" })).expect("proposed");
    let mut third = second_template();
    third["env"]["MODE"] = json!("three");
    rig.template("app", third);
    let writes = rig.fake.writes().len();
    let Err(CallError::Refused { message, .. }) = approve_and_run(&rig, &key, &proposed) else {
        panic!("a stale card is refused");
    };
    assert!(
        message.contains("changed since the owner's card"),
        "{message}"
    );
    assert_eq!(rig.fake.writes().len(), writes, "nothing was written");
    assert_eq!(instance(&rig, "app")["config"]["environment.MODE"], "one");
}

#[test]
fn a_call_without_the_owners_approval_changes_nothing() {
    let key = common::SkKey::from_seed(44);
    let rig = node(&key);
    rig.template("app", second_template());
    let writes = rig.fake.writes().len();
    let Err(CallError::Refused { message, .. }) = rig.call(
        "service_reconfigure",
        json!({ "name": "app", "approval": "forged" }),
    ) else {
        panic!("refused");
    };
    assert!(message.contains("action_propose"), "{message}");
    assert_eq!(rig.fake.writes().len(), writes);
}

#[test]
fn a_templates_raw_lxc_lines_survive_every_reconcile_beside_no_new_privileges() {
    let rig = Rig::new();
    // The project exists once the first service is made; the owner then allows low-level container config.
    rig.template("nats", nats_template());
    rig.call(
        "service_create",
        json!({ "name": "nats", "template": "nats" }),
    )
    .expect("create");
    rig.allow_low_level();
    let mut strict = nats_template();
    strict["name"] = json!("strict");
    strict["noNewPrivileges"] = json!(true);
    strict["rawLxc"] = json!(["lxc.sched.core=0"]);
    rig.template("strict", strict);
    rig.call(
        "service_create",
        json!({ "name": "strict", "template": "strict" }),
    )
    .expect("create");
    let wanted = "lxc.no_new_privs=1\nlxc.sched.core=0";
    let path = format!("/1.0/instances/strict?project={PROJECT}");
    let raw = |rig: &Rig| rig.get(&path).expect("instance")["config"]["raw.lxc"].clone();
    assert_eq!(raw(&rig), wanted);
    let record = rig.service_config("strict");
    assert_eq!(record.raw_lxc, vec!["lxc.sched.core=0".to_string()]);

    // A reconcile of the same record writes nothing and keeps the line.
    let writes = rig.fake.writes().len();
    rig.apply(&record).expect("reconcile");
    assert_eq!(rig.fake.writes().len(), writes, "nothing to write");
    assert_eq!(raw(&rig), wanted);

    // A line nobody declared is not kept: Arca's raw.lxc is what the record says, as before.
    let mut config = rig.get(&path).expect("instance")["config"].clone();
    config["raw.lxc"] = json!("lxc.no_new_privs=1\nlxc.foreign=1");
    rig.edit("PATCH", &path, json!({ "config": config }));
    rig.apply(&record).expect("reconcile");
    assert_eq!(raw(&rig), wanted);

    // A record that declares nothing any more takes away exactly what Arca wrote, and its mark.
    let mut plain = record.clone();
    plain.no_new_privileges = false;
    plain.raw_lxc.clear();
    rig.apply(&plain).expect("reconcile");
    let config = rig.get(&path).expect("instance")["config"].clone();
    assert!(config.get("raw.lxc").is_none(), "{config}");
    assert!(config.get(service::RAW_LXC_KEY).is_none(), "{config}");

    // A service whose record names neither keeps a raw.lxc somebody else put there, as before.
    let nats = format!("/1.0/instances/nats?project={PROJECT}");
    let mut config = rig.get(&nats).expect("instance")["config"].clone();
    config["raw.lxc"] = json!("lxc.foreign=1");
    rig.edit("PATCH", &nats, json!({ "config": config }));
    rig.apply(&rig.service_config("nats")).expect("reconcile");
    assert_eq!(
        rig.get(&nats).expect("instance")["config"]["raw.lxc"],
        "lxc.foreign=1"
    );
}

#[test]
fn a_raw_lxc_line_is_one_lxc_key_and_value_and_only_a_containers() {
    let rig = Rig::new();
    for (bad, why) in [
        (json!(["sched.core=0"]), "not an lxc. key"),
        (json!(["lxc.sched.core=0\nlxc.other=1"]), "two lines in one"),
        (json!(["lxc.sched.core"]), "no value"),
        (json!(["lxc.Bad=1"]), "an upper-case key"),
    ] {
        let mut template = nats_template();
        template["name"] = json!("bad");
        template["rawLxc"] = bad;
        rig.template("bad", template);
        let refused = rig.call(
            "service_create",
            json!({ "name": "bad", "template": "bad" }),
        );
        assert!(refused.is_err(), "{why}: {refused:?}");
    }
    assert!(service::raw_lxc_line_ok("lxc.sched.core=0"));
    assert!(service::raw_lxc_line_ok("lxc.cgroup2.cpuset.cpus = 0-3"));
}

#[test]
fn a_reconfigure_adds_sched_core_beside_no_new_privileges_and_the_card_shows_raw_lxc_whole() {
    let key = common::SkKey::from_seed(45);
    let rig = node(&key);
    let mut strict = first_template();
    strict["noNewPrivileges"] = json!(true);
    rig.template("app", strict.clone());
    let first = propose(&rig, json!({ "name": "app" })).expect("proposed");
    approve_and_run(&rig, &key, &first).expect("no-new-privileges applied");
    assert_eq!(
        instance(&rig, "app")["config"]["raw.lxc"],
        "lxc.no_new_privs=1"
    );

    strict["rawLxc"] = json!(["lxc.sched.core=0"]);
    rig.template("app", strict);
    let proposed = propose(&rig, json!({ "name": "app" })).expect("proposed");
    assert_eq!(
        proposed["args"]["changes"],
        json!({ "rawLxc": {
            "from": ["lxc.no_new_privs=1"],
            "to": ["lxc.no_new_privs=1", "lxc.sched.core=0"]
        } })
    );
    assert_eq!(
        row(
            &proposed["details"],
            "Low-level container settings (raw.lxc)"
        ),
        Some("lxc.no_new_privs=1, lxc.sched.core=0 (was: lxc.no_new_privs=1)")
    );
    let done = approve_and_run(&rig, &key, &proposed).expect("runs");
    assert_eq!(done["readBack"]["matches"], true, "{done}");
    assert_eq!(
        instance(&rig, "app")["config"]["raw.lxc"],
        "lxc.no_new_privs=1\nlxc.sched.core=0"
    );
    // The next reconcile keeps both lines.
    let writes = rig.fake.writes().len();
    rig.apply(&rig.service_config("app")).expect("reconcile");
    assert_eq!(rig.fake.writes().len(), writes);
}

#[test]
fn a_template_that_adds_a_generated_secret_is_reconfigured_on_the_first_approval() {
    let key = common::SkKey::from_seed(46);
    let rig = node(&key);
    let mut template = first_template();
    template["secrets"] = json!(["API_KEY", "SESSION_KEY"]);
    template["generate"] = json!({ "API_KEY": "ak_", "SESSION_KEY": "sk_" });
    template["env"]["SESSION"] = json!("{{secret:SESSION_KEY}}");
    rig.template("app", template);
    let proposed = propose(&rig, json!({ "name": "app" })).expect("proposed");
    assert_eq!(
        proposed["args"]["changes"],
        json!({
            "environment": [{ "key": "SESSION", "change": "added", "secrets": ["SESSION_KEY"] }],
            "secrets": [{ "name": "SESSION_KEY", "change": "generated" }]
        })
    );
    assert_eq!(
        row(&proposed["details"], "Secret SESSION_KEY"),
        Some("added: secret, Arca makes its value now (never shown)")
    );
    let done = approve_and_run(&rig, &key, &proposed).expect("the first approval applies it");
    assert_eq!(done["reconfigured"], true, "{done}");
    assert_eq!(done["readBack"]["matches"], true, "{done}");
    let made = fs::read_to_string(rig.dir.join("secrets/service-app/session-key"))
        .expect("the secret was made");
    assert!(made.starts_with("sk_"));
    assert!(
        instance(&rig, "app")["config"]["environment.SESSION"].as_str() == Some(made.as_str()),
        "the instance holds the secret it was given"
    );
    let journal = fs::read_to_string(rig.dir.join("service-updates.jsonl")).expect("journal");
    for text in [proposed.to_string(), done.to_string(), journal] {
        assert!(!text.contains(&made), "a secret's value was shown");
    }
}

#[test]
fn a_raw_lxc_line_that_may_loosen_isolation_is_said_in_words_on_both_cards() {
    let key = common::SkKey::from_seed(47);
    let rig = node(&key);
    let mut template = first_template();
    template["rawLxc"] = json!(["lxc.sched.core=0", "lxc.mount.entry=/srv srv none bind 0 0"]);
    rig.template("app", template.clone());
    let proposed = propose(&rig, json!({ "name": "app" })).expect("proposed");
    let isolation = |details: &Value| -> Vec<String> {
        details
            .as_array()
            .expect("details")
            .iter()
            .filter(|row| row["label"] == "Isolation")
            .map(|row| row["value"].as_str().expect("words").to_string())
            .collect()
    };
    assert_eq!(
        isolation(&proposed["details"]),
        ["This line may loosen the container's isolation: lxc.mount.entry=/srv srv none bind 0 0"]
    );

    // A create from such a template shows raw.lxc whole, with the same words; one with sched.core alone says nothing
    // more.
    let mut strict = template;
    strict["name"] = json!("strict");
    strict["noNewPrivileges"] = json!(true);
    rig.template("strict", strict.clone());
    let create = propose_action(
        &rig,
        "service_create",
        json!({ "name": "strict", "template": "strict" }),
    )
    .expect("a create card");
    assert_eq!(
        row(&create["details"], "Low-level container settings (raw.lxc)"),
        Some("lxc.no_new_privs=1, lxc.sched.core=0, lxc.mount.entry=/srv srv none bind 0 0")
    );
    assert_eq!(
        isolation(&create["details"]),
        ["This line may loosen the container's isolation: lxc.mount.entry=/srv srv none bind 0 0"]
    );
    strict["name"] = json!("calm");
    strict["rawLxc"] = json!(["lxc.sched.core=0"]);
    rig.template("calm", strict);
    let calm = propose_action(
        &rig,
        "service_create",
        json!({ "name": "calm", "template": "calm" }),
    )
    .expect("a create card");
    assert_eq!(
        row(&calm["details"], "Low-level container settings (raw.lxc)"),
        Some("lxc.no_new_privs=1, lxc.sched.core=0")
    );
    assert!(isolation(&calm["details"]).is_empty());
    // Every line may loosen it but the few Arca knows are harmless: a namespace kept from the host, the host's
    // network, another AppArmor profile, a host mount or a hook all get the row.
    for line in [
        "lxc.namespace.keep=net pid",
        "lxc.net.0.type=none",
        "lxc.apparmor.profile=unchanged",
        "lxc.apparmor.profile=unconfined",
        "lxc.rootfs.path=dir:/",
        "lxc.cgroup2.memory.max=max",
        "lxc.no_new_privs=0",
        "lxc.hook.mount=/bin/true",
    ] {
        assert!(service::raw_lxc_may_loosen(line), "{line}");
    }
    assert!(!service::raw_lxc_may_loosen("lxc.sched.core=0"));
    assert!(!service::raw_lxc_may_loosen("lxc.no_new_privs=1"));
}
