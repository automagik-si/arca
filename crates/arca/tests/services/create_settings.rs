//! Settings given at `service_create` for one service of a virtual-machine template that lists `createSettings`,
//! against a fake Incus: what is taken, what is refused before any write, what the record keeps, what the machine is
//! given, what `service_status` shows and what the owner's card shows. Every value is synthetic.
use super::*;

fn doctor_caller() -> Caller {
    doctor()
}

/// A template whose create may set four of its settings, and may not set its pin or its API address.
fn pool_template() -> Value {
    json!({
        "name": "pool",
        "kind": "virtual-machine",
        "image": "images:debian/13/cloud",
        "limits": { "cpu": "2", "memory": "4GiB" },
        "rootSize": "20GiB",
        "settings": {
            "url": "https://git.example/example-org",
            "group": 1,
            "ratio": 1.5,
            "open": false,
            "classes": [{ "name": "small", "labels": ["a"], "slots": 1 }],
            "limits": { "slots": 2 },
            "pin": "registry.example/runner@sha256:1111",
            "note": "plain text"
        },
        "createSettings": ["url", "group", "ratio", "open", "classes", "limits", "note"]
    })
}

fn rig() -> Rig {
    let rig = Rig::with(mixed_kernel(&["br0"], roomy()), None, vec![]);
    rig.template("pool", pool_template());
    rig
}

fn create_with(rig: &Rig, name: &str, settings: Value) -> Result<Value, CallError> {
    rig.call(
        "service_create",
        json!({ "name": name, "template": "pool", "settings": settings }),
    )
}

/// The settings object a machine's create carried, decoded from its cloud-init data in Incus.
fn guest_settings(rig: &Rig, name: &str) -> Value {
    use base64::Engine as _;
    let config = instance(rig, name)["config"].clone();
    let vendor = config["cloud-init.vendor-data"]
        .as_str()
        .expect("vendor data");
    let content = vendor
        .lines()
        .find_map(|line| line.strip_prefix("  content: "))
        .expect("the content line");
    serde_json::from_slice(
        &base64::engine::general_purpose::STANDARD
            .decode(content)
            .expect("base64"),
    )
    .expect("json")
}

#[test]
fn a_create_replaces_the_settings_its_template_lists_and_the_machine_gets_the_merged_object() {
    let rig = rig();
    let given = json!({
        "url": "https://git.example/another-org",
        "group": 7,
        "classes": [{ "name": "large", "labels": ["a", "b"], "slots": 3 }]
    });
    create_with(&rig, "p1", given.clone()).expect("create");
    // The machine reads the template's settings with the three values in place of the defaults.
    let mut expected = pool_template()["settings"].clone();
    for (key, value) in given.as_object().unwrap() {
        expected[key] = value.clone();
    }
    assert_eq!(guest_settings(&rig, "p1"), expected);
    // The record keeps what was asked for, beside the data the machine was given.
    let record = entry(&rig, "service-p1").config;
    assert_eq!(record["vm"]["settings"], given);
    // service_status shows the settings the machine was created with.
    let status = rig
        .call("service_status", json!({ "name": "p1" }))
        .expect("status");
    assert_eq!(status["settings"], expected);
    // A template edited afterwards changes nothing for the machine that exists: its settings are its create's.
    let mut edited = pool_template();
    edited["settings"]["note"] = json!("edited later");
    rig.template("pool", edited);
    assert_eq!(
        rig.call("service_status", json!({ "name": "p1" }))
            .expect("status")["settings"],
        expected
    );
    rig.template("pool", pool_template());

    // With no settings, and with an empty object, the machine gets the template's own and the record has no key:
    // it is the record, and the request, of before.
    for (name, args) in [
        ("p2", json!({ "name": "p2", "template": "pool" })),
        (
            "p3",
            json!({ "name": "p3", "template": "pool", "settings": {} }),
        ),
    ] {
        rig.call("service_create", args).expect("create");
        assert_eq!(guest_settings(&rig, name), pool_template()["settings"]);
        assert!(
            entry(&rig, &format!("service-{name}")).config["vm"]
                .get("settings")
                .is_none()
        );
    }
    assert_eq!(
        instance(&rig, "p2")["config"]["cloud-init.vendor-data"],
        instance(&rig, "p3")["config"]["cloud-init.vendor-data"]
    );
    // A machine without settings shows none.
    rig.call("service_create", json!({ "name": "ws1", "template": "ws" }))
        .expect("a plain machine");
    assert!(
        rig.call("service_status", json!({ "name": "ws1" }))
            .expect("status")
            .get("settings")
            .is_none()
    );
    no_open_intent(&rig);
}

#[test]
fn a_setting_the_template_does_not_list_a_wrong_type_or_a_loose_address_is_refused_before_any_write()
 {
    let rig = rig();
    let before = rig.fake.writes().len();
    let lets = "it lets a create set url, group, ratio, open, classes, limits, note";
    for (settings, why) in [
        // A key the template does not list: its pin, and a key it does not have at all.
        (
            json!({ "pin": "registry.example/other@sha256:2222" }),
            format!("settings.pin: template pool does not let a create set it; {lets}"),
        ),
        (
            json!({ "enterprise": true }),
            format!("settings.enterprise: template pool does not let a create set it; {lets}"),
        ),
        // The type is the template default's.
        (
            json!({ "group": "7" }),
            "settings.group: expected a whole number, the type of the template's default".into(),
        ),
        (
            json!({ "group": 7.5 }),
            "settings.group: expected a whole number, the type of the template's default".into(),
        ),
        (
            json!({ "ratio": 2 }),
            "settings.ratio: expected a number, the type of the template's default".into(),
        ),
        (
            json!({ "open": "yes" }),
            "settings.open: expected true or false, the type of the template's default".into(),
        ),
        (
            json!({ "classes": { "name": "small" } }),
            "settings.classes: expected a list, the type of the template's default".into(),
        ),
        (
            json!({ "limits": [2] }),
            "settings.limits: expected an object, the type of the template's default".into(),
        ),
        (
            json!({ "url": null }),
            "settings.url: expected a string, the type of the template's default".into(),
        ),
        // No control character, in a string or in a key, however deep.
        (
            json!({ "note": "two\nlines" }),
            "settings.note: a value holds no control character and nests at most eight deep".into(),
        ),
        (
            json!({ "classes": [{ "name": "a\u{1b}[31m" }] }),
            "settings.classes: a value holds no control character and nests at most eight deep"
                .into(),
        ),
        (
            json!({ "limits": { "a\tb": 1 } }),
            "settings.limits: a value holds no control character and nests at most eight deep"
                .into(),
        ),
        (
            json!({ "classes": [[[[[[[[[1]]]]]]]]] }),
            "settings.classes: a value holds no control character and nests at most eight deep"
                .into(),
        ),
    ] {
        assert_eq!(
            refusal(create_with(&rig, "p1", settings.clone())),
            why,
            "{settings}"
        );
    }
    // An address decides where the machine sends its credential: only a plain https address is taken.
    for url in [
        "http://git.example/example-org",
        "https://git.example@elsewhere.example/example-org",
        "https://user:word@git.example/example-org",
        "https://git.example#@elsewhere.example/example-org",
        "https://git.example/example-org?x=1",
        "https://git.example/example-org#frag",
        "https://git.example/%2e%2e/example-org",
        "https://git.example/../example-org",
        "https://git.example//example-org",
        "https://git.example\\@elsewhere.example/example-org",
        "https:///example-org",
        "https://-git.example/example-org",
        "https://git..example/example-org",
        "https://git.example:port/example-org",
        "https://git.example/example org",
        "git.example/example-org",
    ] {
        let why = refusal(create_with(&rig, "p1", json!({ "url": url })));
        assert!(
            why.starts_with("settings.url: expected an https address like the template's default, https://<host>/<path>, with a plain host name and path: no user or password (@)"),
            "{url}: {why}"
        );
    }
    // Not an object at all, and settings too large once merged.
    assert_eq!(
        refusal(create_with(&rig, "p1", json!(["url"]))),
        "settings: expected an object of setting names and values"
    );
    assert_eq!(
        refusal(create_with(
            &rig,
            "p1",
            json!({ "note": "x".repeat(17 * 1024) })
        )),
        "settings: with these values the settings of template pool pass 16KiB"
    );
    // A template that lists none takes none, a container template among them.
    for template in ["ws", "nats"] {
        assert_eq!(
            refusal(rig.call(
                "service_create",
                json!({ "name": "p1", "template": template, "settings": { "url": "https://git.example/x" } })
            )),
            format!("settings: template {template} lets a create set none (its createSettings lists none)")
        );
    }
    assert_eq!(rig.fake.writes().len(), before, "no refusal wrote anything");
    assert!(rig.entry_ids().iter().all(|id| id != "service-p1"));

    // What is taken: a port, a deeper path, and the address with nothing after the host.
    for (name, url) in [
        ("p1", "https://git.example:8443/example-org/example-repo"),
        ("p2", "https://git.example"),
    ] {
        create_with(&rig, name, json!({ "url": url })).expect("a plain https address");
        assert_eq!(guest_settings(&rig, name)["url"], url);
    }
}

#[test]
fn a_template_lists_only_keys_of_its_own_settings_and_only_a_virtual_machine_lists_any() {
    let rig = rig();
    let dir = rig.dir.join("templates");
    let load = |value: Value| {
        let mut value = value;
        value["name"] = json!("bad");
        rig.template("bad", value);
        service::load_template(&dir, "bad")
    };
    for listed in [
        json!(["url", "absent"]),
        json!(["url", "url"]),
        json!((0..33).map(|n| format!("k{n}")).collect::<Vec<_>>()),
    ] {
        let mut template = pool_template();
        template["createSettings"] = listed.clone();
        let error = load(template).expect_err("refused");
        assert!(
            error.message.contains(
                "createSettings lists at most 32 distinct keys of settings, each with its default there"
            ),
            "{listed}: {}",
            error.message
        );
    }
    // Without settings there is nothing to list.
    let mut template = vm_template();
    template["createSettings"] = json!(["url"]);
    assert!(load(template).is_err());
    let mut container = nats_template();
    container["createSettings"] = json!(["url"]);
    assert!(
        load(container)
            .expect_err("refused")
            .message
            .contains("only a virtual-machine template has a firstBoot or settings")
    );
    // The shipped runner pool lets a create set where it registers and how it is sized, and nothing else: not its
    // pins, not its API address, not the switch for the default runner group.
    let shipped = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../templates");
    let pool =
        service::load_template(&shipped, "github-runner-pool").expect("the shipped template");
    assert_eq!(
        pool.create_settings,
        ["url", "runnerGroupId", "classes", "maxSlots"]
    );
}

#[test]
fn the_owners_card_shows_each_replaced_setting_and_a_proposal_with_a_refused_one_is_never_made() {
    let key = common::SkKey::from_seed(9);
    let rig = Rig::with(mixed_kernel(&["br0"], roomy()), Some(&key), vec![]);
    rig.template("pool", pool_template());
    let long = "x".repeat(400);
    let args = json!({
        "name": "p1", "template": "pool",
        "settings": {
            "url": "https://git.example/another-org",
            "group": 7,
            "classes": [{ "name": "large", "labels": ["a", "b"], "slots": 3 }],
            "note": long
        }
    });
    let prepared = ActionKind::ServiceCreate
        .prepare(&args, None)
        .expect("the arguments fit the verb's schema");
    let details: Vec<(String, String)> = ActionKind::ServiceCreate
        .details(&prepared)
        .into_iter()
        .map(|row| (row.label, row.value))
        .collect();
    // One row per replaced setting, with its value; a long value is cut and says so.
    assert_eq!(
        details,
        [
            ("Name".to_string(), "p1".to_string()),
            ("Template".to_string(), "pool".to_string()),
            (
                "Setting classes".to_string(),
                r#"[{"labels":["a","b"],"name":"large","slots":3}]"#.to_string()
            ),
            ("Setting group".to_string(), "7".to_string()),
            ("Setting note".to_string(), format!("{}…", "x".repeat(160))),
            (
                "Setting url".to_string(),
                "https://git.example/another-org".to_string()
            ),
        ]
    );
    // A card never carries a control character, whatever the arguments hold.
    let odd = ActionKind::ServiceCreate.details(&json!({
        "name": "p1", "template": "pool", "settings": { "no\nte": "a\u{1b}[2Jb" }
    }));
    assert_eq!(
        (odd[2].label.as_str(), odd[2].value.as_str()),
        ("Setting no te", "a [2Jb")
    );

    let propose = |args: Value| {
        let handlers: HashMap<&'static str, Arc<dyn ToolHandler>> =
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
            .collect();
        handlers["action_propose"].call(
            json!({ "action": "service_create", "args": args }),
            &doctor_caller(),
        )
    };
    // A setting the create would refuse is refused when the proposal is made: the owner is never shown it.
    for settings in [
        json!({ "pin": "registry.example/other@sha256:2222" }),
        json!({ "url": "https://git.example@elsewhere.example/example-org" }),
        json!({ "group": "7" }),
    ] {
        assert!(
            matches!(
                propose(json!({ "name": "p1", "template": "pool", "settings": settings })),
                Err(CallError::Refused { .. })
            ),
            "{settings}"
        );
    }
    assert!(rig.entry_ids().is_empty());
    // A good one is proposed with its rows, approved by the owner, and run by the same handler.
    let proposed = propose(args.clone()).expect("the proposal is recorded");
    let rows = proposed["details"].as_array().expect("details");
    assert!(
        rows.contains(
            &json!({ "label": "Setting url", "value": "https://git.example/another-org" })
        ),
        "{proposed}"
    );
    assert!(rows.contains(&json!({ "label": "Setting group", "value": "7" })));
    let proposal = rig
        .kernel
        .lock()
        .expect("kernel")
        .propose_action(ActionKind::ServiceCreate, args.clone(), NOW, None)
        .expect("the proposal");
    let statement = action_statement(
        "node1",
        &proposal.id,
        "service_create",
        &proposal.args_digest,
        NOW,
        &"7".repeat(32),
    )
    .expect("the statement");
    let approval = common::approve_statement(&key, &statement);
    let approved = {
        let mut kernel = rig.kernel.lock().expect("kernel");
        approve_action(
            &mut kernel,
            &json!({ "plan": proposal.id, "approval": approval }),
            NOW,
        )
        .expect("the owner's approval verifies")
    };
    let executor = ActionExecutor::new(
        rig.handlers
            .iter()
            .map(|(verb, handler)| (*verb, Arc::clone(handler))),
    );
    executor
        .run(&approved.action, &approved.args, &approved.grant)
        .expect("the approved create runs");
    // The machine got exactly the values the owner was shown, the long one whole.
    let got = guest_settings(&rig, "p1");
    assert_eq!(got["url"], "https://git.example/another-org");
    assert_eq!(got["group"], 7);
    assert_eq!(got["note"], json!(long));
    assert_eq!(got["pin"], pool_template()["settings"]["pin"]);
}

/// Kernel access is a container template's and settings at create are a virtual machine's: no template has both, and
/// each argument is refused in words for the other kind of template, before anything is written. A node holds a
/// service of each kind's template side by side.
#[test]
fn kernel_access_and_create_settings_belong_to_two_kinds_of_template_and_neither_takes_the_others_argument()
 {
    let rig = rig();
    let dir = rig.dir.join("templates");
    let load = |name: &str, value: Value| {
        let mut value = value;
        value["name"] = json!(name);
        rig.template(name, value);
        service::load_template(&dir, name)
    };
    // A virtual-machine template with kernel access, and a container template that lists settings for a create.
    let mut machine = pool_template();
    machine["kernelAccess"] = json!({ "port": 7461 });
    assert!(
        load("both1", machine)
            .expect_err("refused")
            .message
            .contains("or kernelAccess: they need a container's environment, image or devices")
    );
    let mut container = nats_template();
    container["kernelAccess"] = json!({ "port": 7461 });
    container["settings"] = json!({ "url": "https://git.example/example-org" });
    container["createSettings"] = json!(["url"]);
    assert!(
        load("both2", container)
            .expect_err("refused")
            .message
            .contains("only a virtual-machine template has a firstBoot or settings")
    );
    // Each kind alone loads, and keeps what is its own.
    let mut kern = nats_template();
    kern["kernelAccess"] = json!({ "port": 7461 });
    let kern = load("kern", kern).expect("a container template with kernel access");
    assert!(kern.kernel_access.is_some() && kern.create_settings.is_empty());
    let pool = service::load_template(&dir, "pool").expect("the settings template");
    assert!(pool.kernel_access.is_none() && !pool.create_settings.is_empty());

    let before = rig.fake.writes().len();
    // The machine's create does not take the container's argument, with or without its own.
    for args in [
        json!({ "name": "p1", "template": "pool", "kernelAccess": { "port": 7461 } }),
        json!({ "name": "p1", "template": "pool", "kernelAccess": { "port": 7461 },
            "settings": { "url": "https://git.example/another-org" } }),
    ] {
        assert!(matches!(
            rig.call("service_create", args),
            Err(CallError::Invalid(why))
                if why == "kernelAccess: template pool gives its service no access to the kernel"
        ));
    }
    // The container's template does not take the machine's argument: its check says so, and a create with it is
    // refused before anything is written (on this node already for the kernel access it has no socket for).
    assert_eq!(
        service::create_settings(&kern, Some(&json!({ "url": "https://git.example/x" })))
            .expect_err("refused")
            .message,
        "settings: template kern lets a create set none (its createSettings lists none)"
    );
    assert!(
        refusal(rig.call(
            "service_create",
            json!({ "name": "k1", "template": "kern", "settings": { "url": "https://git.example/x" } })
        ))
        .starts_with("template kern gives its service access to this node's kernel, and this node has no socket for it")
    );
    assert_eq!(rig.fake.writes().len(), before, "no refusal wrote anything");

    // Side by side on one node: the machine with its settings, and beside it a container of a template with
    // kernel access, which this node (it sets no services socket) refuses for that reason and no other.
    create_with(
        &rig,
        "p1",
        json!({ "url": "https://git.example/another-org" }),
    )
    .expect("the machine");
    assert_eq!(
        guest_settings(&rig, "p1")["url"],
        "https://git.example/another-org"
    );
    let why = refusal(rig.call(
        "service_create",
        json!({ "name": "k1", "template": "kern" }),
    ));
    assert!(
        why.starts_with("template kern gives its service access to this node's kernel, and this node has no socket for it"),
        "{why}"
    );
    // The machine's status shows its settings and no kernel access.
    let status = rig
        .call("service_status", json!({ "name": "p1" }))
        .expect("status");
    assert!(status.get("settings").is_some() && status.get("kernelAccess").is_none());
}
