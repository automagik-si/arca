//! A virtual-machine template's `settingRules`: what must hold between its settings. Each rule says one amount is at
//! least another, where an amount is a whole number, a whole-number setting, or the number of entries of a list
//! setting. The kernel compares and knows nothing of what the amounts mean. A create whose settings would not pass
//! is refused before any write, and so is its proposal, in the same words; a template whose rules cannot be read, or
//! whose own defaults do not pass them, is refused when it is read. Every value is synthetic, the two shipped GitLab
//! pool templates aside.
use super::*;

const EVERY_CLASS: &str = "every class is kept one place under the cap";

/// A machine whose settings are held to four rules: the cap is at least the number of classes, there is at least
/// one class, and a percentage stays between 100 and 200.
fn ruled_template() -> Value {
    json!({
        "name": "ruled",
        "kind": "virtual-machine",
        "image": "images:debian/13/cloud",
        "limits": { "cpu": "2", "memory": "4GiB" },
        "rootSize": "20GiB",
        "settings": {
            "maxSlots": 2,
            "percent": 100,
            "classes": [{ "name": "small", "slots": 1 }],
            "note": "plain text"
        },
        "createSettings": ["maxSlots", "percent", "classes", "note"],
        "settingRules": [
            { "need": "maxSlots", "atLeast": { "count": "classes" }, "why": EVERY_CLASS },
            { "need": { "count": "classes" }, "atLeast": 1, "why": "a pool has a class" },
            { "need": "percent", "atLeast": 100, "why": "100 is the least" },
            { "need": 200, "atLeast": "percent", "why": "200 is the most" }
        ]
    })
}

fn classes(count: usize) -> Value {
    json!(
        (0..count)
            .map(|n| json!({ "name": format!("c{n}"), "slots": 1 }))
            .collect::<Vec<_>>()
    )
}

fn ruled_rig() -> Rig {
    let key = common::SkKey::from_seed(9);
    let rig = Rig::with(mixed_kernel(&["br0"], roomy()), Some(&key), vec![]);
    rig.template("ruled", ruled_template());
    rig
}

fn propose(rig: &Rig, args: Value) -> Result<Value, CallError> {
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
        &Caller {
            token_id: "doctor".into(),
            scope: Scope::Doctor,
            tools: None,
        },
    )
}

/// Each rule in turn is the one a create's settings do not pass. The create is refused before any write with the
/// kernel's words about the two amounts and the template's own reason after them, and the proposal of the same
/// create is refused in exactly those words, so no card is recorded.
#[test]
fn a_create_whose_settings_fail_a_rule_is_refused_and_so_is_its_proposal_in_the_same_words() {
    let rig = ruled_rig();
    let before = rig.fake.writes().len();
    for (settings, why) in [
        // More classes than the default cap of 2.
        (
            json!({ "classes": classes(3) }),
            format!(
                "settings: settings.maxSlots is 2, below the number of entries in settings.classes (3): {EVERY_CLASS}"
            ),
        ),
        // A cap of 0 is below one class too.
        (
            json!({ "maxSlots": 0 }),
            format!(
                "settings: settings.maxSlots is 0, below the number of entries in settings.classes (1): {EVERY_CLASS}"
            ),
        ),
        // Both given: the rule reads the values this service would have, not the defaults.
        (
            json!({ "maxSlots": 3, "classes": classes(4) }),
            format!(
                "settings: settings.maxSlots is 3, below the number of entries in settings.classes (4): {EVERY_CLASS}"
            ),
        ),
        (
            json!({ "classes": [] }),
            "settings: the number of entries in settings.classes is 0, below 1: a pool has a class"
                .to_string(),
        ),
        (
            json!({ "percent": 99 }),
            "settings: settings.percent is 99, below 100: 100 is the least".to_string(),
        ),
        // A rule whose `need` is a number is a ceiling on the other amount, and is said so.
        (
            json!({ "percent": 201 }),
            "settings: settings.percent is 201, above 200: 200 is the most".to_string(),
        ),
    ] {
        let args = json!({ "name": "p1", "template": "ruled", "settings": settings });
        let create = refusal(rig.call("service_create", args.clone()));
        assert_eq!(create, why, "{settings}");
        assert_eq!(refusal(propose(&rig, args)), create, "{settings}");
    }
    assert_eq!(rig.fake.writes().len(), before, "nothing was written");
    assert!(rig.entry_ids().is_empty());
    assert!(
        rig.kernel
            .lock()
            .expect("kernel")
            .action_proposals()
            .is_empty(),
        "no card waits"
    );
}

/// Settings exactly at every rule pass: the cap equal to the number of classes, the percentage at each end. The
/// proposal is recorded and the create is made. A create that gives no settings is the template's defaults, which
/// passed when the template was read.
#[test]
fn settings_exactly_at_each_rule_pass_and_the_service_is_created() {
    let rig = ruled_rig();
    for (name, settings) in [
        (
            "p1",
            json!({ "maxSlots": 3, "classes": classes(3), "percent": 200 }),
        ),
        ("p2", json!({ "maxSlots": 1, "percent": 100 })),
    ] {
        let args = json!({ "name": name, "template": "ruled", "settings": settings });
        propose(&rig, args.clone()).unwrap_or_else(|error| panic!("{name} proposed: {error:?}"));
        rig.call("service_create", args)
            .unwrap_or_else(|error| panic!("{name} created: {error:?}"));
    }
    rig.call(
        "service_create",
        json!({ "name": "p3", "template": "ruled" }),
    )
    .expect("the defaults pass");
}

/// A rule the kernel cannot read against the template's own settings, a rule without a reason, and a template
/// whose own defaults do not pass its rule are refused when the template is read, in words that name the rule.
/// Only a virtual machine states any.
#[test]
fn a_template_whose_rules_cannot_be_read_or_whose_defaults_fail_them_is_refused_when_it_is_read() {
    let rig = ruled_rig();
    let dir = rig.dir.join("templates");
    let load = |rules: Value| {
        let mut value = ruled_template();
        value["name"] = json!("bad");
        value["settingRules"] = rules;
        rig.template("bad", value);
        service::load_template(&dir, "bad")
    };
    let good = json!({ "need": "maxSlots", "atLeast": { "count": "classes" }, "why": EVERY_CLASS });
    let forms = "need and atLeast are each a whole number, the name of a setting, or { \"count\": \"<the name of a setting that is a list>\" }";
    for (rule, why) in [
        // A setting the template does not have, on either side.
        (
            json!({ "need": "slots", "atLeast": 1, "why": "x" }),
            "need names slots, which is not a setting of the template".to_string(),
        ),
        (
            json!({ "need": "maxSlots", "atLeast": { "count": "pools" }, "why": "x" }),
            "atLeast names pools, which is not a setting of the template".to_string(),
        ),
        // A setting of the wrong type for the amount.
        (
            json!({ "need": "note", "atLeast": 1, "why": "x" }),
            "need names note, whose value is not a whole number".to_string(),
        ),
        (
            json!({ "need": { "count": "maxSlots" }, "atLeast": 1, "why": "x" }),
            "need counts maxSlots, whose value is not a list".to_string(),
        ),
        (
            json!({ "need": "classes", "atLeast": 1, "why": "x" }),
            "need names classes, whose value is not a whole number".to_string(),
        ),
        // An amount of no form the kernel reads.
        (json!({ "need": { "sum": "classes" }, "atLeast": 1, "why": "x" }), forms.to_string()),
        (json!({ "need": "maxSlots", "atLeast": -1, "why": "x" }), forms.to_string()),
        (json!({ "need": "maxSlots", "atLeast": 1.5, "why": "x" }), forms.to_string()),
        (json!({ "need": true, "atLeast": 1, "why": "x" }), forms.to_string()),
        (
            json!({ "need": { "count": "classes", "of": "x" }, "atLeast": 1, "why": "x" }),
            forms.to_string(),
        ),
        (
            json!({ "need": 2, "atLeast": 1, "why": "x" }),
            "need and atLeast are both numbers, so it says nothing about a setting".to_string(),
        ),
        // The reason is the template's own words to the owner.
        (
            json!({ "need": "maxSlots", "atLeast": 1, "why": " " }),
            "why is its reason in plain words, 1 to 200 characters".to_string(),
        ),
        (
            json!({ "need": "maxSlots", "atLeast": 1, "why": "x".repeat(201) }),
            "why is its reason in plain words, 1 to 200 characters".to_string(),
        ),
        (
            json!({ "need": "maxSlots", "atLeast": 1, "why": "pay \u{202e}eno ot" }),
            "why is its reason in plain words, 1 to 200 characters".to_string(),
        ),
        // The template's own defaults (a cap of 2, one class) do not pass it.
        (
            json!({ "need": "maxSlots", "atLeast": 3, "why": "x" }),
            "the template's own settings do not pass it: settings.maxSlots is 2, below 3".to_string(),
        ),
        (
            json!({ "need": { "count": "classes" }, "atLeast": "maxSlots", "why": "x" }),
            "the template's own settings do not pass it: the number of entries in settings.classes is 1, below settings.maxSlots (2)".to_string(),
        ),
    ] {
        // The bad rule is the second of two, and the refusal names it by its place.
        let error = load(json!([good, rule])).expect_err("refused");
        assert_eq!(
            error.message,
            format!("template bad: settingRules[1]: {why}"),
            "{rule}"
        );
        assert!(!error.not_found);
    }
    // A key a rule does not have, and a rule without its reason, are refused by the template's shape.
    for rule in [
        json!({ "need": "maxSlots", "atLeast": 1, "why": "x", "atMost": 9 }),
        json!({ "need": "maxSlots", "atLeast": 1 }),
    ] {
        assert!(load(json!([rule])).is_err(), "{rule}");
    }
    let many: Vec<Value> = (0..17).map(|_| good.clone()).collect();
    assert_eq!(
        load(json!(many)).expect_err("refused").message,
        "template bad: settingRules lists at most 16 rules"
    );
    load(json!([good])).expect("one good rule loads");
    // Only a virtual machine has settings, so only one has rules about them.
    let mut container = nats_template();
    container["name"] = json!("bad");
    container["settingRules"] = json!([good]);
    rig.template("bad", container);
    assert_eq!(
        service::load_template(&dir, "bad")
            .expect_err("refused")
            .message,
        "template bad: only a virtual-machine template has settingRules"
    );
}

/// The two GitLab pool templates as they ship state six rules, each one the pool's own script refuses on the
/// machine's console (`settings: ...` in `templates/gitlab-runner-pool.sh`). Their defaults pass, each rule refuses
/// the first value outside it and takes the last value inside it.
#[test]
fn the_shipped_gitlab_pool_templates_state_what_their_script_refuses() {
    let shipped = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../templates");
    let runners = |count: usize| {
        json!(
            (0..count)
                .map(
                    |n| json!({ "name": format!("r{n}"), "limit": 0, "jobCpu": 0, "jobMemory": "" })
                )
                .collect::<Vec<_>>()
        )
    };
    for name in ["gitlab-runner-pool", "gitlab-runner-pool-large"] {
        let template = service::load_template(&shipped, name)
            .unwrap_or_else(|error| panic!("{name} loads with its rules: {}", error.message));
        assert_eq!(template.setting_rules.len(), 6, "{name}");
        let refused = |settings: Value| {
            service::create_settings(&template, Some(&settings))
                .expect_err("refused")
                .message
        };
        for (settings, starts) in [
            (
                json!({ "concurrent": 0 }),
                "settings: settings.concurrent is 0, below 1: ",
            ),
            (
                json!({ "jobCpu": 0 }),
                "settings: settings.jobCpu is 0, below 1: ",
            ),
            (
                json!({ "memoryOvercommitPercent": 99 }),
                "settings: settings.memoryOvercommitPercent is 99, below 100: ",
            ),
            (
                json!({ "memoryOvercommitPercent": 201 }),
                "settings: settings.memoryOvercommitPercent is 201, above 200: ",
            ),
            (
                json!({ "runners": [] }),
                "settings: the number of entries in settings.runners is 0, below 1: ",
            ),
            (
                json!({ "runners": runners(13) }),
                "settings: the number of entries in settings.runners is 13, above 12: ",
            ),
        ] {
            let said = refused(settings.clone());
            assert!(said.starts_with(starts), "{name} {settings}: {said}");
            assert!(said.len() > starts.len(), "the template's reason follows");
        }
        for settings in [
            json!({ "concurrent": 1, "jobCpu": 1 }),
            json!({ "memoryOvercommitPercent": 100 }),
            json!({ "memoryOvercommitPercent": 200 }),
            json!({ "runners": runners(1) }),
            json!({ "runners": runners(12) }),
        ] {
            service::create_settings(&template, Some(&settings))
                .unwrap_or_else(|error| panic!("{name} {settings}: {}", error.message));
        }
    }
}

/// The GitHub pool template as it ships states one rule, which its own script refuses on the machine's console
/// (`settings: maxSlots is ...` in `templates/github-runner-pool.sh`): the cap is at least the number of classes. Its
/// defaults pass, and the rule refuses the first value outside it and takes the last value inside it. It is the
/// plain rule: one class with a cap of nothing is refused too, though the script runs such a pool with one place.
#[test]
fn the_shipped_github_pool_template_states_that_the_cap_is_at_least_the_number_of_classes() {
    let shipped = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../templates");
    let template = service::load_template(&shipped, "github-runner-pool")
        .unwrap_or_else(|error| panic!("it loads with its rule: {}", error.message));
    assert_eq!(template.setting_rules.len(), 1);
    let classes = |count: usize| {
        json!(
            (0..count)
                .map(|n| json!({ "name": format!("c{n}"), "labels": ["self-hosted", format!("arca-c{n}")],
                    "cpu": 2, "memory": "4GiB", "slots": 1 }))
                .collect::<Vec<_>>()
        )
    };
    for (settings, starts) in [
        // The template's own two classes.
        (
            json!({ "maxSlots": 1 }),
            "settings: settings.maxSlots is 1, below the number of entries in settings.classes (2): ",
        ),
        // The template's own cap of four.
        (
            json!({ "classes": classes(5) }),
            "settings: settings.maxSlots is 4, below the number of entries in settings.classes (5): ",
        ),
        (
            json!({ "classes": classes(1), "maxSlots": 0 }),
            "settings: settings.maxSlots is 0, below the number of entries in settings.classes (1): ",
        ),
    ] {
        let said = service::create_settings(&template, Some(&settings))
            .expect_err("refused")
            .message;
        assert!(said.starts_with(starts), "{settings}: {said}");
        assert!(
            said.ends_with("set maxSlots to the number of classes or more, or name fewer classes"),
            "the template's reason follows: {said}"
        );
    }
    for settings in [
        json!({ "maxSlots": 2 }),
        json!({ "classes": classes(4) }),
        json!({ "classes": classes(1), "maxSlots": 1 }),
        json!({ "classes": classes(6), "maxSlots": 6 }),
    ] {
        service::create_settings(&template, Some(&settings))
            .unwrap_or_else(|error| panic!("{settings}: {}", error.message));
    }
}
