//! `service_update` with `withTemplate`: the new image and what the service's CURRENT template says go in with one
//! owner card and one restart, and a rollback puts back both. Every value is synthetic.
use super::*;

#[test]
fn container_options_template_update_and_rollback_keep_the_creation_settings() {
    for healthy in [true, false] {
        let key = common::SkKey::from_seed(81);
        let up = Arc::new(AtomicBool::new(healthy));
        let rig = bare_node(&up, Some(&key));
        let mut template = first();
        template["portIsolation"] = json!(true);
        template["rootSize"] = json!("30GiB");
        rig.template("app", template);
        rig.call(
            "service_create",
            json!({ "name": "app", "template": "app" }),
        )
        .expect("created");
        let devices = instance(&rig)["devices"].clone();
        assert_eq!(devices["eth0"]["security.port_isolation"], "true");
        assert_eq!(devices["root"]["size"], "30GiB");
        rig.template("app", second());
        let proposed = propose_update(&rig, with_template()).expect("proposed");
        let result = approve(&rig, &key, &proposed);
        if healthy {
            result.expect("updated");
        } else {
            assert_eq!(refusal_words(result).0, "rolled_back");
        }
        assert_eq!(instance(&rig)["devices"], devices);
        let record = serde_json::to_value(rig.service_config("app")).unwrap();
        assert_eq!(record["portIsolation"], true);
        assert_eq!(record["rootSize"], "30GiB");
    }
}

/// The shell-form start command of the second template, quotes and `$HOME` in it, as an operator writes it.
const SHELL_SCRIPT: &str =
    "unset APP_PATH; cd \"$HOME\"; exec /opt/app/bin/app serve --no-active-app --port 8080";

/// The template `app` is made from first: one plain value and no entrypoint, so the image's own runs.
fn first() -> Value {
    let mut template = app_template();
    template["env"] = json!({ "MODE": "one" });
    template
}

/// The template as it says later: a shell-form entrypoint and a new environment value.
fn second() -> Value {
    let mut template = first();
    template["env"] = json!({ "MODE": "one", "APP_CONTROL_DATABASE": "app" });
    template["entrypoint"] = json!(["/bin/sh", "-ec", SHELL_SCRIPT]);
    template
}

/// A node whose owner holds `key`, with `app` made from [`first`] and running, the health check answering as `up` says.
/// `snapshots` lets the project take a snapshot, which no project Arca makes does.
fn node(up: &Arc<AtomicBool>, key: &common::SkKey, snapshots: bool) -> Rig {
    let rig = bare_node(up, Some(key));
    rig.template("app", first());
    rig.call(
        "service_create",
        json!({ "name": "app", "template": "app" }),
    )
    .expect("create");
    if snapshots {
        let path = format!("/1.0/projects/{PROJECT}");
        let mut config = rig.get(&path).expect("the project")["config"].clone();
        config["restricted.snapshots"] = json!("allow");
        rig.edit("PATCH", &path, json!({ "config": config }));
    }
    rig
}

/// Each approval in these tests is signed with a nonce of its own.
static NONCE: AtomicU64 = AtomicU64::new(0x7100);

/// The owner approves `proposed` and the approved action runs through the node's own handlers.
fn approve(rig: &Rig, key: &common::SkKey, proposed: &Value) -> Result<Value, CallError> {
    let id = proposed["id"].as_str().expect("an id");
    let statement = arca::handlers::action_statement(
        "node1",
        id,
        "service_update",
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

fn with_template() -> Value {
    json!({ "name": "app", "digest": NEW, "withTemplate": true, "timeoutSeconds": 1 })
}

fn config(rig: &Rig) -> Value {
    instance(rig)["config"].clone()
}

fn row<'a>(details: &'a Value, label: &str) -> Option<&'a str> {
    details
        .as_array()
        .expect("details")
        .iter()
        .find(|row| row["label"] == label)
        .and_then(|row| row["value"].as_str())
}

/// The script inside a shell entrypoint as Incus will run it: the third argument Incus splits `oci.entrypoint` into,
/// decoded from the one word Arca makes of it.
fn script_of(entrypoint: &str) -> String {
    use base64::Engine as _;
    let args = arca_incus::oci_entrypoint_args(entrypoint).expect("Incus splits it");
    assert_eq!(args[..2], ["/bin/sh", "-ec"], "{args:?}");
    let word = &args[2];
    let encoded = word
        .split_once("echo${IFS}")
        .and_then(|(_, rest)| rest.split_once("|base64"))
        .map(|(encoded, _)| encoded)
        .unwrap_or_else(|| panic!("an encoded script: {word}"));
    String::from_utf8(
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("base64"),
    )
    .expect("utf-8")
}

#[test]
fn one_card_moves_the_image_and_the_template_from_no_entrypoint_to_a_shell_one_with_one_restart() {
    let key = common::SkKey::from_seed(81);
    let up = Arc::new(AtomicBool::new(true));
    let rig = node(&up, &key, false);
    assert!(
        config(&rig).get("oci.entrypoint").is_none(),
        "the image's own runs"
    );
    rig.template("app", second());

    let proposed = propose_update(&rig, with_template()).expect("proposed");
    // The card binds the digest and the template's difference, both shown.
    let changes = &proposed["args"]["changes"];
    assert_eq!(
        changes["environment"],
        json!([{ "key": "APP_CONTROL_DATABASE", "value": "app", "change": "added" }]),
        "{proposed}"
    );
    assert_eq!(changes["entrypoint"]["from"], "the image's own");
    let details = &proposed["details"];
    assert_eq!(row(details, "Digest"), Some(NEW));
    assert_eq!(row(details, "Env APP_CONTROL_DATABASE"), Some("added: app"));
    assert!(row(details, "Entrypoint").is_some(), "{details}");
    assert_eq!(
        row(details, "Template changes"),
        Some("environment value 1 added; its entrypoint")
    );
    let summary = proposed["summary"].as_str().expect("a summary");
    assert!(summary.contains("one restart"), "{summary}");

    let actions = rig.fake.state_actions(PROJECT, "app").len();
    let done = approve(&rig, &key, &proposed).expect("runs");
    assert_eq!(done["updated"], true, "{done}");
    assert_eq!(done["readBack"]["matches"], true, "{done}");
    assert_eq!(&done["changes"], changes);

    // One interruption: one stop before the rebuild and one start after it, and no restart.
    assert_eq!(
        rig.fake.state_actions(PROJECT, "app")[actions..],
        ["stop", "start"]
    );
    assert_eq!(rig.fake.rebuilds(PROJECT, "app"), vec!["registry"]);

    // The instance runs the new image with the template's environment and its shell-form entrypoint, byte for byte.
    let after = config(&rig);
    assert_eq!(instance(&rig)["status"], "Running");
    assert_eq!(after["environment.APP_CONTROL_DATABASE"], "app");
    assert_eq!(after["environment.MODE"], "one");
    let entrypoint = after["oci.entrypoint"].as_str().expect("an entrypoint");
    assert_eq!(script_of(entrypoint), SHELL_SCRIPT);
    let record = rig.service_config("app");
    assert_eq!(record.entrypoint.as_deref(), Some(entrypoint));
    assert_eq!(alias_of_record(&rig), format!("app@{NEW}"));

    let steps: Vec<String> = journal(&rig).into_iter().map(|(step, _)| step).collect();
    assert_eq!(
        steps,
        [
            "begin",
            "stop",
            "rebuild",
            "apply",
            "empty /run",
            "start",
            "health",
            "record",
            "read back",
            "end"
        ]
    );
    // Nothing is left to do: a reconcile writes nothing more.
    let writes = rig.fake.writes().len();
    rig.apply(&rig.service_config("app")).expect("reconcile");
    assert_eq!(rig.fake.writes().len(), writes);
}

#[test]
fn a_failed_health_check_puts_back_the_old_root_and_no_entrypoint_exactly() {
    for snapshots in [false, true] {
        let key = common::SkKey::from_seed(82);
        let up = Arc::new(AtomicBool::new(false));
        let rig = node(&up, &key, snapshots);
        let before = rig.service_config("app");
        let old_alias = alias_of_record(&rig);
        let old_config = config(&rig);
        rig.template("app", second());
        let proposed = propose_update(&rig, with_template()).expect("proposed");

        let actions = rig.fake.state_actions(PROJECT, "app").len();
        let (code, message) = refusal_words(approve(&rig, &key, &proposed));
        assert_eq!(code, "rolled_back", "{message}");
        assert!(message.contains("health check"), "{message}");
        assert!(
            message.contains("its old settings are back in its record and its instance's config"),
            "{message}"
        );

        assert_eq!(
            rig.fake.rebuilds(PROJECT, "app"),
            vec!["registry", "registry"]
        );
        assert!(rig.fake.restores(PROJECT, "app").is_empty());
        // The old record, exactly: no entrypoint, the old environment, the old digest.
        let record = rig.service_config("app");
        assert_eq!(record.entrypoint, None);
        assert_eq!(record.environment, before.environment);
        assert_eq!(record.ports, before.ports);
        assert_eq!(alias_of_record(&rig), old_alias);
        // And the old config on the instance: the template's entrypoint and its new value are gone, so the image's own
        // command runs again.
        let after = config(&rig);
        assert!(after.get("oci.entrypoint").is_none(), "{after}");
        assert!(after.get(service::ENTRYPOINT_KEY).is_none(), "{after}");
        assert!(
            after.get("environment.APP_CONTROL_DATABASE").is_none(),
            "{after}"
        );
        assert_eq!(after["environment.MODE"], old_config["environment.MODE"]);
        assert_eq!(instance(&rig)["status"], "Running");
        // The new version was started once, then the old one: two starts, no restart.
        assert_eq!(
            rig.fake.state_actions(PROJECT, "app")[actions..],
            ["stop", "start", "stop", "start"]
        );
        let steps = journal(&rig);
        assert!(steps.contains(&("put back".to_string(), true)), "{steps:?}");
    }
}

#[test]
fn an_exec_form_entrypoint_moves_to_the_shell_form_and_a_rollback_puts_the_exec_form_back() {
    let key = common::SkKey::from_seed(83);
    let up = Arc::new(AtomicBool::new(true));
    let rig = bare_node(&up, Some(&key));
    let mut exec = first();
    exec["entrypoint"] = json!(["/opt/app/bin/app", "serve", "--port", "8080"]);
    rig.template("app", exec);
    rig.call(
        "service_create",
        json!({ "name": "app", "template": "app" }),
    )
    .expect("create");
    let old = config(&rig)["oci.entrypoint"]
        .as_str()
        .expect("the exec form")
        .to_string();
    assert_eq!(old, "/opt/app/bin/app serve --port 8080");
    let mut shell = second();
    shell["env"] = json!({ "MODE": "one" });
    rig.template("app", shell);

    // A failed health check first: the exec form comes back on the record and on the instance.
    up.store(false, Ordering::SeqCst);
    let proposed = propose_update(&rig, with_template()).expect("proposed");
    assert_eq!(
        proposed["args"]["changes"]["entrypoint"]["from"],
        old.as_str()
    );
    let (code, message) = refusal_words(approve(&rig, &key, &proposed));
    assert_eq!(code, "rolled_back", "{message}");
    assert_eq!(config(&rig)["oci.entrypoint"], old.as_str());
    assert_eq!(
        rig.service_config("app").entrypoint.as_deref(),
        Some(old.as_str())
    );

    // Then the same card made again, healthy: the shell form, byte for byte.
    up.store(true, Ordering::SeqCst);
    let proposed = propose_update(&rig, with_template()).expect("proposed again");
    let done = approve(&rig, &key, &proposed).expect("runs");
    assert_eq!(done["readBack"]["matches"], true, "{done}");
    let entrypoint = config(&rig)["oci.entrypoint"]
        .as_str()
        .expect("an entrypoint")
        .to_string();
    assert_eq!(script_of(&entrypoint), SHELL_SCRIPT);
}

#[test]
fn the_card_binds_the_template_difference_and_an_edit_after_proposing_needs_a_new_card() {
    let key = common::SkKey::from_seed(84);
    let up = Arc::new(AtomicBool::new(true));
    let rig = node(&up, &key, false);
    rig.template("app", second());
    let proposed = propose_update(&rig, with_template()).expect("proposed");

    // What a caller sends as changes is never kept: the node works them out.
    let mut forged = with_template();
    forged["changes"] = json!({ "environment": [] });
    let again = propose_update(&rig, forged).expect("proposed with forged changes");
    assert_eq!(again["args"]["changes"], proposed["args"]["changes"]);
    assert_eq!(again["argsDigest"], proposed["argsDigest"]);

    // The template changes after the card was made: another difference is another digest, and the old card's run
    // changes nothing.
    let mut third = second();
    third["env"]["APP_CONTROL_DATABASE"] = json!("other");
    rig.template("app", third);
    let moved = propose_update(&rig, with_template()).expect("proposed after the edit");
    assert_ne!(moved["argsDigest"], proposed["argsDigest"]);
    let before = config(&rig);
    let actions = rig.fake.state_actions(PROJECT, "app").len();
    let (_, message) = refusal_words(approve(&rig, &key, &proposed));
    assert!(
        message.contains("changed since the owner's card was made")
            && message.ends_with("Nothing was changed"),
        "{message}"
    );
    assert!(rig.fake.rebuilds(PROJECT, "app").is_empty());
    assert_eq!(rig.fake.state_actions(PROJECT, "app").len(), actions);
    assert_eq!(config(&rig), before);

    // A different start command alone is a different card too.
    let mut fourth = second();
    fourth["entrypoint"] = json!(["/bin/sh", "-ec", format!("{SHELL_SCRIPT} --verbose")]);
    rig.template("app", fourth);
    let other = propose_update(&rig, with_template()).expect("proposed");
    assert_ne!(other["argsDigest"], moved["argsDigest"]);
}

#[test]
fn what_service_reconfigure_would_refuse_is_refused_in_words_and_changes_nothing() {
    let key = common::SkKey::from_seed(85);
    let up = Arc::new(AtomicBool::new(true));
    let rig = node(&up, &key, false);
    rig.template("app", second());
    let before = config(&rig);

    // Only an approved card runs it.
    let (_, message) = refusal_words(update(&rig, with_template()));
    assert!(
        message.contains("only from a card the owner approved")
            && message.ends_with("Nothing was changed"),
        "{message}"
    );
    // A template that is now a virtual machine's.
    let mut vm = second();
    vm["kind"] = json!("vm");
    vm["vm"] = json!({ "cpu": 1, "memory": "1GiB", "disk": "8GiB" });
    rig.template("app", vm);
    if let Ok(proposed) = propose_update(&rig, with_template()) {
        panic!("a card for a virtual machine's template: {proposed}");
    }
    rig.template("app", second());
    // Not with follow or noNewPrivileges, and not without a digest.
    for (args, word) in [
        (
            json!({ "name": "app", "follow": true, "withTemplate": true }),
            "withTemplate",
        ),
        (
            json!({ "name": "app", "digest": NEW, "withTemplate": true, "noNewPrivileges": true }),
            "noNewPrivileges",
        ),
        (
            json!({ "name": "app", "withTemplate": true }),
            "withTemplate",
        ),
        (
            json!({ "name": "app", "digest": NEW, "withTemplate": "yes" }),
            "withTemplate",
        ),
    ] {
        match propose_update(&rig, args.clone()) {
            Err(CallError::Invalid(why)) => assert!(why.contains(word), "{args}: {why}"),
            other => panic!("{args}: {other:?}"),
        }
    }
    // A direct call that states changes without the flag; a proposal drops what a caller sent there.
    match update(&rig, json!({ "name": "app", "digest": NEW, "changes": {} })) {
        Err(CallError::Invalid(why)) => assert!(why.starts_with("changes:"), "{why}"),
        other => panic!("{other:?}"),
    }
    // The digest the service runs: there is no image to go in with.
    let (_, message) = refusal_words(propose_update(
        &rig,
        json!({ "name": "app", "digest": DIGEST, "withTemplate": true }),
    ));
    assert!(message.contains("service_reconfigure"), "{message}");
    assert!(rig.fake.rebuilds(PROJECT, "app").is_empty());
    assert_eq!(config(&rig), before);
}

#[test]
fn without_the_flag_an_update_keeps_the_record_and_answers_as_before() {
    let up = Arc::new(AtomicBool::new(true));
    let rig = rig(&up);
    let before = rig.service_config("app");
    let mut changed = app_template();
    changed["env"] = json!({ "NEW_KEY": "x" });
    changed["entrypoint"] = json!(["/bin/sh", "-ec", SHELL_SCRIPT]);
    rig.template("app", changed);

    let actions = rig.fake.state_actions(PROJECT, "app").len();
    let done = update(&rig, json!({ "name": "app", "digest": NEW })).expect("update");
    let mut keys: Vec<&str> = done
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    // As before, and `run`, which every service that keeps `/run` in memory answers.
    assert_eq!(
        keys,
        ["digest", "message", "run", "service", "snapshot", "updated"]
    );
    assert_eq!(done["run"], "memory", "{done}");
    let after = rig.service_config("app");
    assert_eq!(after.environment, before.environment);
    assert_eq!(after.entrypoint, before.entrypoint);
    assert!(config(&rig).get("oci.entrypoint").is_none());
    assert_eq!(
        rig.fake.state_actions(PROJECT, "app")[actions..],
        ["stop", "start"]
    );
    let steps: Vec<String> = journal(&rig).into_iter().map(|(step, _)| step).collect();
    assert_eq!(
        steps,
        [
            "begin",
            "stop",
            "rebuild",
            "empty /run",
            "start",
            "health",
            "record",
            "end"
        ]
    );
}

/// The node's handlers again, with `clock` and `probe` of the test's own.
fn handlers_with(
    rig: &mut Rig,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    probe: arca::handlers::UpdateProbe,
) {
    rig.handlers = service_handlers_updating(
        rig.kernel.clone(),
        clock,
        rig.cfg.clone(),
        None,
        UpdateOptions {
            probe,
            pace: Duration::from_millis(10),
        },
    )
    .into_iter()
    .collect();
}

/// The template of a service reached at its address on `droidd`, one of `ports`: no lan proxy.
fn direct(ports: [u16; 2], droidd: u16) -> Value {
    let mut template = first();
    template.as_object_mut().expect("an object").remove("lan");
    template["ports"] = json!(ports);
    template["droiddPort"] = json!(droidd);
    template
}

#[test]
fn the_health_check_looks_at_the_port_the_template_moves_the_service_to_never_the_old_one() {
    // The first port was the droidd port, so the old version was probed there. The template moves the first port to
    // one the node's firewall does not open to it: the new version is checked by staying running, never at 8080.
    let key = common::SkKey::from_seed(87);
    let up = Arc::new(AtomicBool::new(true));
    let mut rig = bare_node(&up, Some(&key));
    rig.template("app", direct([8080, 9090], 8080));
    rig.call(
        "service_create",
        json!({ "name": "app", "template": "app" }),
    )
    .expect("create");
    let probed = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&probed);
    handlers_with(
        &mut rig,
        Arc::new(|| NOW),
        Arc::new(move |_, port, _| {
            log.lock().unwrap().push(port);
            port == 9090
        }),
    );
    let mut moved = direct([9090, 8080], 8080);
    moved["entrypoint"] = json!(["/bin/sh", "-ec", SHELL_SCRIPT]);
    rig.template("app", moved);
    let proposed = propose_update(&rig, with_template()).expect("proposed");
    assert!(
        proposed["args"]["changes"].get("ports").is_some(),
        "{proposed}"
    );
    let done = approve(&rig, &key, &proposed).expect("the new version runs");
    assert_eq!(done["updated"], true, "{done}");
    assert!(probed.lock().unwrap().is_empty(), "{:?}", probed.lock());
    assert_eq!(rig.service_config("app").ports, vec![9090, 8080]);

    // The other way round: the first port the template moves to is the droidd port, so the new version is probed
    // there, at the service's own address, where it now listens.
    let key = common::SkKey::from_seed(88);
    let mut rig = bare_node(&up, Some(&key));
    rig.template("app", direct([8080, 9090], 9090));
    rig.call(
        "service_create",
        json!({ "name": "app", "template": "app" }),
    )
    .expect("create");
    let probed = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&probed);
    handlers_with(
        &mut rig,
        Arc::new(|| NOW),
        Arc::new(move |address, port, forwarded| {
            log.lock()
                .unwrap()
                .push((address.to_string(), port, forwarded));
            port == 9090
        }),
    );
    rig.template("app", direct([9090, 8080], 9090));
    let proposed = propose_update(&rig, with_template()).expect("proposed");
    approve(&rig, &key, &proposed).expect("the new version answers on 9090");
    let seen = probed.lock().unwrap().clone();
    assert!(!seen.is_empty(), "the new version was probed");
    let address = rig.service_config("app").ipv4;
    assert!(
        seen.iter()
            .all(|probe| *probe == (address.clone(), 9090, false)),
        "{seen:?}"
    );
}

#[test]
fn a_secret_set_while_the_update_runs_leaves_the_template_unwritten_and_the_rollback_keeps_that_record()
 {
    let key = common::SkKey::from_seed(89);
    let up = Arc::new(AtomicBool::new(true));
    let mut rig = bare_node(&up, Some(&key));
    let mut with_secret = first();
    with_secret["secrets"] = json!(["TOKEN"]);
    rig.template("app", with_secret);
    rig.call(
        "service_create",
        json!({ "name": "app", "template": "app" }),
    )
    .expect("create");
    let mut later = second();
    later["secrets"] = json!(["TOKEN"]);
    rig.template("app", later);

    // The node's clock is the test's hook: when the update journal's last line is its stop, the operator sets the
    // secret, once, while the instance is stopped and before the template's settings are written.
    let journal_file = rig.dir.join("service-updates.jsonl");
    let setter: Arc<std::sync::OnceLock<Arc<dyn ToolHandler>>> =
        Arc::new(std::sync::OnceLock::new());
    let fired = Arc::new(AtomicBool::new(false));
    let clock = {
        let setter = Arc::clone(&setter);
        let fired = Arc::clone(&fired);
        Arc::new(move || {
            let stopped = fs::read_to_string(&journal_file)
                .ok()
                .and_then(|text| text.lines().last().map(str::to_string))
                .and_then(|line| serde_json::from_str::<Value>(&line).ok())
                .is_some_and(|line| line["step"] == "stop");
            if stopped && !fired.swap(true, Ordering::SeqCst) {
                setter
                    .get()
                    .expect("the handlers")
                    .call(
                        json!({ "name": "app", "key": "TOKEN", "value": "set-meanwhile" }),
                        &Caller {
                            token_id: "operator".into(),
                            scope: Scope::Operator,
                            tools: None,
                            limits: Default::default(),
                        },
                    )
                    .expect("the secret is set meanwhile");
            }
            NOW
        })
    };
    handlers_with(&mut rig, clock, Arc::new(|_, _, _| true));
    assert!(
        setter
            .set(Arc::clone(&rig.handlers["service_secret_set"]))
            .is_ok()
    );
    let proposed = propose_update(&rig, with_template()).expect("proposed");
    let actions = rig.fake.state_actions(PROJECT, "app").len();
    let (code, message) = refusal_words(approve(&rig, &key, &proposed));
    assert!(fired.load(Ordering::SeqCst), "the hook ran");
    assert_eq!(code, "rolled_back", "{message}");
    assert!(
        message.contains("a link or a secret was set meanwhile"),
        "{message}"
    );
    assert!(
        !message.contains("its old settings are back"),
        "nothing was written, so nothing was put back: {message}"
    );

    // The record is the one the operator left: the secret stays, and none of the template's settings went in.
    let record = rig.service_config("app");
    assert!(
        record.secret_refs.contains_key("TOKEN"),
        "{:?}",
        record.secret_refs
    );
    assert_eq!(record.entrypoint, None);
    assert!(!record.environment.contains_key("APP_CONTROL_DATABASE"));
    let after = config(&rig);
    assert!(after.get("oci.entrypoint").is_none(), "{after}");
    assert!(after.get(service::ENTRYPOINT_KEY).is_none(), "{after}");
    // The old version runs again from its own digest, with one stop and one start: no second restart.
    assert_eq!(
        rig.fake.state_actions(PROJECT, "app")[actions..],
        ["stop", "start"]
    );
    let steps = journal(&rig);
    assert!(steps.contains(&("apply".to_string(), false)), "{steps:?}");
    assert!(
        !steps.iter().any(|(step, _)| step == "put back"),
        "{steps:?}"
    );
}

/// PATCHes the instance's config as an operator would with Incus, outside Arca.
fn patch_instance(rig: &Rig, add: Value) {
    let path = format!("/1.0/instances/app?project={PROJECT}");
    let mut config = rig.get(&path).expect("the instance")["config"].clone();
    for (key, value) in add.as_object().expect("an object") {
        config[key] = value.clone();
    }
    rig.edit("PATCH", &path, json!({ "config": config }));
}

#[test]
fn an_operator_entrypoint_on_a_service_whose_record_names_none_is_never_taken_off() {
    let up = Arc::new(AtomicBool::new(true));
    let rig = rig(&up);
    assert_eq!(rig.service_config("app").entrypoint, None);
    patch_instance(&rig, json!({ "oci.entrypoint": "/opt/own serve" }));
    rig.apply(&rig.service_config("app")).expect("reconcile");
    let after = config(&rig);
    assert_eq!(after["oci.entrypoint"], "/opt/own serve", "{after}");
    assert!(after.get(service::ENTRYPOINT_KEY).is_none(), "{after}");
}

#[test]
fn a_marked_entrypoint_an_operator_changed_since_is_left_and_only_the_mark_goes() {
    let up = Arc::new(AtomicBool::new(true));
    let rig = rig(&up);
    patch_instance(
        &rig,
        json!({ "oci.entrypoint": "/opt/changed serve", service::ENTRYPOINT_KEY: "/opt/arca wrote" }),
    );
    rig.apply(&rig.service_config("app")).expect("reconcile");
    let after = config(&rig);
    assert_eq!(after["oci.entrypoint"], "/opt/changed serve", "{after}");
    assert!(after.get(service::ENTRYPOINT_KEY).is_none(), "{after}");
}

#[test]
fn a_marked_entrypoint_still_as_written_goes_when_the_record_names_none() {
    let up = Arc::new(AtomicBool::new(true));
    let rig = rig(&up);
    patch_instance(
        &rig,
        json!({ "oci.entrypoint": "/opt/arca wrote", service::ENTRYPOINT_KEY: "/opt/arca wrote" }),
    );
    rig.apply(&rig.service_config("app")).expect("reconcile");
    let after = config(&rig);
    assert!(after.get("oci.entrypoint").is_none(), "{after}");
    assert!(after.get(service::ENTRYPOINT_KEY).is_none(), "{after}");
}

/// A service made before Arca kept `/run` in memory (its template then said `runTmpfs: false`, so its image's `/run` is
/// on its disk), whose template now says nothing of it: the withTemplate card carries the `/run` row, and the one
/// restart of the update empties the new root's `/run` after the template's record goes in, so it starts with a memory
/// `/run`, as a reconfigure gives it.
#[test]
fn an_older_service_gets_a_memory_run_with_its_new_image_in_the_one_restart() {
    let key = common::SkKey::from_seed(88);
    let up = Arc::new(AtomicBool::new(true));
    let rig = bare_node(&up, Some(&key));
    rig.fake
        .set_image_run(&["/run/lock/", "/run/lock/.keep", "/run/motd"]);
    let mut before = first();
    before["runTmpfs"] = json!(false);
    rig.template("app", before);
    rig.call(
        "service_create",
        json!({ "name": "app", "template": "app" }),
    )
    .expect("create");
    assert!(!rig.service_config("app").run_tmpfs);
    assert!(!rig.fake.run_in_memory(PROJECT, "app"));
    assert!(config(&rig).get(service::RUN_TMPFS_KEY).is_none());
    rig.template("app", first());

    let proposed = propose_update(&rig, with_template()).expect("proposed");
    assert_eq!(
        proposed["args"]["changes"],
        json!({ "runTmpfs": { "from": false, "to": true } }),
        "{proposed}"
    );
    let details = &proposed["details"];
    assert_eq!(
        row(details, arca::action_proposal::RUN_TMPFS_LABEL),
        Some(arca::action_proposal::RUN_TO_MEMORY)
    );
    assert_eq!(row(details, "Template changes"), Some("its /run"));

    let actions = rig.fake.state_actions(PROJECT, "app").len();
    let done = approve(&rig, &key, &proposed).expect("runs");
    assert_eq!(done["updated"], true, "{done}");
    assert_eq!(done["run"], "memory", "{done}");
    assert!(done.get("runNote").is_none(), "{done}");
    assert_eq!(done["readBack"]["matches"], true, "{done}");
    // One stop and one start: the new root's /run was emptied between them, so the start mounted a memory /run.
    assert_eq!(
        rig.fake.state_actions(PROJECT, "app")[actions..],
        ["stop", "start"]
    );
    assert_eq!(rig.fake.rebuilds(PROJECT, "app"), vec!["registry"]);
    assert!(rig.fake.run_on_disk(PROJECT, "app").is_empty());
    assert!(rig.fake.run_in_memory(PROJECT, "app"));
    assert_eq!(config(&rig)[service::RUN_TMPFS_KEY], "true");
    assert!(rig.service_config("app").run_tmpfs);
    assert_eq!(alias_of_record(&rig), format!("app@{NEW}"));

    let steps: Vec<String> = journal(&rig).into_iter().map(|(step, _)| step).collect();
    let at = |name: &str| {
        steps
            .iter()
            .position(|step| step == name)
            .unwrap_or_else(|| panic!("{name} in {steps:?}"))
    };
    assert!(at("rebuild") < at("apply"), "{steps:?}");
    assert!(at("apply") < at("empty /run"), "{steps:?}");
    assert!(at("empty /run") < at("start"), "{steps:?}");
    assert!(journal(&rig).contains(&("empty /run".to_string(), true)));

    // Nothing is left to do: a reconcile writes nothing more.
    let writes = rig.fake.writes().len();
    rig.apply(&rig.service_config("app")).expect("reconcile");
    assert_eq!(rig.fake.writes().len(), writes);
}

/// The same older service, whose withTemplate update fails its health check: the rollback puts back the old record
/// (`runTmpfs: false`) and the old root, and leaves that root's `/run` as the image has it. Only the new version's root
/// was emptied, so the old version starts with `/run` on its disk, as it ran before the update.
#[test]
fn a_rollback_of_an_older_service_leaves_the_old_roots_run_on_its_disk() {
    for snapshots in [false, true] {
        let key = common::SkKey::from_seed(89);
        let up = Arc::new(AtomicBool::new(false));
        let rig = bare_node(&up, Some(&key));
        rig.fake
            .set_image_run(&["/run/lock/", "/run/lock/.keep", "/run/motd"]);
        let mut before = first();
        before["runTmpfs"] = json!(false);
        rig.template("app", before);
        rig.call(
            "service_create",
            json!({ "name": "app", "template": "app" }),
        )
        .expect("create");
        if snapshots {
            let path = format!("/1.0/projects/{PROJECT}");
            let mut config = rig.get(&path).expect("the project")["config"].clone();
            config["restricted.snapshots"] = json!("allow");
            rig.edit("PATCH", &path, json!({ "config": config }));
        }
        rig.template("app", first());
        let proposed = propose_update(&rig, with_template()).expect("proposed");
        assert_eq!(
            proposed["args"]["changes"],
            json!({ "runTmpfs": { "from": false, "to": true } }),
            "{proposed}"
        );

        let (code, message) = refusal_words(approve(&rig, &key, &proposed));
        assert_eq!(code, "rolled_back", "{message}");
        assert!(message.contains("health check"), "{message}");
        assert_eq!(
            rig.fake.rebuilds(PROJECT, "app"),
            vec!["registry", "registry"]
        );
        assert!(rig.fake.restores(PROJECT, "app").is_empty());

        // The old record and config: no memory /run, and its root keeps what the image ships in /run.
        assert!(!rig.service_config("app").run_tmpfs);
        assert!(config(&rig).get(service::RUN_TMPFS_KEY).is_none());
        assert_eq!(instance(&rig)["status"], "Running");
        assert!(
            !rig.fake.run_on_disk(PROJECT, "app").is_empty(),
            "snapshots {snapshots}"
        );
        assert!(!rig.fake.run_in_memory(PROJECT, "app"));

        // The new root was emptied before its start; nothing was emptied after the old root came back.
        let steps: Vec<String> = journal(&rig).into_iter().map(|(step, _)| step).collect();
        let back = steps
            .iter()
            .position(|step| step == "rebuild back")
            .unwrap_or_else(|| panic!("the old root in {steps:?}"));
        let emptied: Vec<usize> = steps
            .iter()
            .enumerate()
            .filter(|(_, step)| *step == "empty /run")
            .map(|(at, _)| at)
            .collect();
        assert_eq!(emptied.len(), 1, "{steps:?}");
        assert!(emptied[0] < back, "{steps:?}");
    }
}
