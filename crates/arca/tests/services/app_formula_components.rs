//! A template's `appUpdate` with several ordered components, and the words it carries beside either form: what else
//! the update restarts (`alsoRestarts`) and what it deliberately leaves alone (`notUpdated`). Every limit at its edge
//! and one past it, the rules each component shares with the single form, the argument list a component's version
//! makes, and what `service_status` says of them. Nothing here runs a command. Every value is synthetic.
use super::*;
use arca::app_formula::{AppUpdate, AppVersion};

fn health() -> Value {
    json!({ "command": ["/usr/bin/curl", "--fail", "--silent", "http://localhost/ready"], "seconds": 300 })
}

fn version_reader() -> Value {
    json!({ "command": ["/usr/bin/cat", "/opt/app/version.txt"] })
}

fn update() -> Value {
    json!({ "command": ["/usr/local/sbin/app-update", "{version}"], "seconds": 3600 })
}

/// A component named `name` with the usual three parts.
fn component(name: &str) -> Value {
    json!({ "name": name, "update": update(), "health": health(), "version": version_reader() })
}

/// A formula of the components named `names`, in that order.
fn components(names: &[&str]) -> Value {
    json!({ "components": names.iter().map(|name| component(name)).collect::<Vec<_>>() })
}

fn system(formula: Value) -> Value {
    json!({
        "name": "app", "kind": "system", "image": "images:debian/13",
        "ports": [8080], "env": {}, "publishable": false, "appUpdate": formula
    })
}

fn oci(formula: Value) -> Value {
    json!({
        "name": "app", "kind": "oci", "image": "registry.example/app", "digest": DIGEST,
        "ports": [8080], "env": {}, "publishable": false, "appUpdate": formula
    })
}

fn load(rig: &Rig, template: Value) -> Result<service::Template, service::TemplateError> {
    rig.template("app", template);
    service::load_template(&rig.dir.join("templates"), "app")
}

fn formula(rig: &Rig, template: Value) -> AppUpdate {
    match load(rig, template) {
        Ok(template) => template.app_update.expect("the template has a formula"),
        Err(error) => panic!("the template is refused: {}", error.message),
    }
}

/// The refusal of `template`, which must be refused.
fn refusal(rig: &Rig, template: Value) -> String {
    match load(rig, template.clone()) {
        Ok(_) => panic!("accepted: {template}"),
        Err(error) => {
            assert!(!error.not_found, "{}", error.message);
            error.message
        }
    }
}

#[test]
fn a_system_template_takes_one_to_eight_components_in_their_order() {
    let rig = Rig::new();
    let read = formula(&rig, system(components(&["server"])));
    assert_eq!(read.component_names(), ["server"]);
    let eight = ["queue", "runtime", "plugins", "a", "b1", "c-2", "d", "e"];
    let read = formula(&rig, system(components(&eight)));
    assert_eq!(read.component_names(), eight);
    // The single parts stand at none of the top's places.
    assert!(read.update.is_none() && read.health.is_none() && read.version.is_none());
    // The order is the formula's, not sorted.
    let read = formula(&rig, system(components(&["zeta", "alpha"])));
    assert_eq!(read.component_names(), ["zeta", "alpha"]);
    let count = "template app: appUpdate.components is a list of 1 to 8 components, in the order they are updated";
    assert_eq!(refusal(&rig, system(components(&[]))), count);
    let nine = ["a", "b", "c", "d", "e", "f", "g", "h", "i"];
    assert_eq!(refusal(&rig, system(components(&nine))), count);
}

#[test]
fn a_component_name_is_1_to_32_lower_case_letters_digits_and_hyphens_first_a_letter_and_unique() {
    let rig = Rig::new();
    let longest = format!("a{}", "b".repeat(31));
    for name in ["a", "z9", "message-server", "a-", "a--1", longest.as_str()] {
        let read = formula(&rig, system(components(&["first", name])));
        assert_eq!(read.component_names(), ["first", name]);
    }
    let too_long = format!("a{}", "b".repeat(32));
    for name in [
        "",
        too_long.as_str(),
        "A",
        "Server",
        "1a",
        "-a",
        "a_b",
        "a.b",
        "a b",
        " a",
        "a\n",
        "é",
        "aé",
        "a/b",
    ] {
        assert_eq!(
            refusal(&rig, system(components(&["first", name]))),
            "template app: appUpdate.components[1].name is 1 to 32 bytes of lower-case letters, digits and -, the first a letter",
            "{name:?}"
        );
    }
    assert_eq!(
        refusal(&rig, system(components(&["runtime", "plugins", "runtime"]))),
        "template app: appUpdate.components[2].name runtime is the name of an earlier component: each name stands once"
    );
}

#[test]
fn the_single_parts_and_components_together_are_refused() {
    let rig = Rig::new();
    for part in ["update", "health", "version"] {
        let mut both = components(&["server"]);
        both[part] = match part {
            "update" => update(),
            "health" => health(),
            _ => version_reader(),
        };
        assert_eq!(
            refusal(&rig, system(both)),
            "template app: appUpdate has either update, health and version, or components, never both",
            "{part}"
        );
    }
    let mut all = components(&["server"]);
    all["update"] = update();
    all["health"] = health();
    all["version"] = version_reader();
    assert!(refusal(&rig, system(all)).ends_with("never both"));
}

#[test]
fn components_on_an_oci_template_are_refused_naming_the_field() {
    let rig = Rig::new();
    assert_eq!(
        refusal(&rig, oci(components(&["server"]))),
        "template app: appUpdate.components is only for a system container template: an OCI template's update is the move to another image digest"
    );
    // A component with no update part on an OCI template is refused the same way: the field, not its parts.
    assert!(
        refusal(
            &rig,
            oci(json!({ "components": [{ "name": "server", "health": health(), "version": version_reader() }] }))
        )
        .starts_with("template app: appUpdate.components is only for a system container template")
    );
    let machine = json!({
        "name": "app", "kind": "virtual-machine", "image": "images:debian/13/cloud",
        "limits": { "cpu": "2", "memory": "4GiB" }, "rootSize": "20GiB",
        "appUpdate": components(&["server"])
    });
    assert_eq!(
        refusal(&rig, machine),
        "template app: appUpdate is only for an OCI or a system container template, never a virtual machine"
    );
}

#[test]
fn every_new_object_is_strict() {
    let rig = Rig::new();
    let mut unknown = component("server");
    unknown["order"] = json!(1);
    let mut nameless = component("server");
    nameless.as_object_mut().unwrap().remove("name");
    let mut number_name = component("server");
    number_name["name"] = json!(7);
    let mut command_line = component("server");
    command_line["health"] = json!({ "command": "/usr/bin/true" });
    let mut extra_step_key = component("server");
    extra_step_key["version"] = json!({ "command": ["/usr/bin/true"], "every": 5 });
    for template in [
        system(json!({ "components": [unknown] })),
        system(json!({ "components": [nameless] })),
        system(json!({ "components": [number_name] })),
        system(json!({ "components": [command_line] })),
        system(json!({ "components": [extra_step_key] })),
        system(json!({ "components": component("server") })),
        system(json!({ "components": ["server"] })),
        system(json!({ "components": null, "other": true })),
        oci(
            json!({ "health": health(), "version": version_reader(), "alsoRestarts": "the web server" }),
        ),
        oci(json!({ "health": health(), "version": version_reader(), "alsoRestarts": [7] })),
        oci(
            json!({ "health": health(), "version": version_reader(), "notUpdated": { "name": "a", "reason": "b" } }),
        ),
        oci(json!({ "health": health(), "version": version_reader(), "notUpdated": ["a"] })),
        oci(
            json!({ "health": health(), "version": version_reader(), "notUpdated": [{ "name": "a" }] }),
        ),
        oci(
            json!({ "health": health(), "version": version_reader(), "notUpdated": [{ "reason": "b" }] }),
        ),
        oci(
            json!({ "health": health(), "version": version_reader(), "notUpdated": [{ "name": "a", "reason": "b", "since": 1 }] }),
        ),
        oci(
            json!({ "health": health(), "version": version_reader(), "notUpdated": [{ "name": "a", "reason": 2 }] }),
        ),
        oci(json!({ "health": health(), "version": version_reader(), "also_restarts": ["x"] })),
    ] {
        assert_eq!(
            refusal(&rig, template.clone()),
            "template app: cannot be read",
            "{template}"
        );
    }
}

/// A component's parts as the single form would hold them: each case of row 1's rules made once in the single form
/// and once in the second component of a formula, refused with the same words but for where the part stands.
#[test]
fn each_component_holds_the_single_forms_limits_and_version_rules_by_the_same_code() {
    let rig = Rig::new();
    let mut thirty_two = vec![json!("/usr/bin/true")];
    thirty_two.extend((1..32).map(|at| json!(format!("a{at}"))));
    let mut thirty_three = thirty_two.clone();
    thirty_three.push(json!("one-more"));
    let mut update_32 = thirty_two.clone();
    update_32[31] = json!("{version}");
    let long = |bytes: usize| format!("/{}", "a".repeat(bytes - 1));
    // (part, its value): every case is a change to one part of the usual three.
    let cases: Vec<(&str, Value)> = vec![
        ("health", json!({ "command": thirty_two.clone() })),
        ("health", json!({ "command": thirty_three.clone() })),
        ("health", json!({ "command": [] })),
        ("health", json!({ "command": [long(512)] })),
        ("health", json!({ "command": [long(513)] })),
        ("health", json!({ "command": ["/usr/bin/true", ""] })),
        (
            "health",
            json!({ "command": ["/usr/bin/true", "a\u{202E}b"] }),
        ),
        ("health", json!({ "command": ["true"] })),
        (
            "health",
            json!({ "command": ["/usr/bin/check", "{version}"] }),
        ),
        (
            "health",
            json!({ "command": ["/usr/bin/true"], "seconds": 1 }),
        ),
        (
            "health",
            json!({ "command": ["/usr/bin/true"], "seconds": 900 }),
        ),
        (
            "health",
            json!({ "command": ["/usr/bin/true"], "seconds": 0 }),
        ),
        (
            "health",
            json!({ "command": ["/usr/bin/true"], "seconds": 901 }),
        ),
        (
            "version",
            json!({ "command": ["/usr/bin/cat", "/v"], "seconds": 60 }),
        ),
        (
            "version",
            json!({ "command": ["/usr/bin/cat", "/v"], "seconds": 61 }),
        ),
        ("version", json!({ "command": ["/usr/bin/cat", "a\nb"] })),
        (
            "version",
            json!({ "command": ["/usr/bin/check", "{version}"] }),
        ),
        ("update", json!({ "command": update_32, "seconds": 60 })),
        (
            "update",
            json!({ "command": ["/usr/sbin/up", "{version}"], "seconds": 10 }),
        ),
        (
            "update",
            json!({ "command": ["/usr/sbin/up", "{version}"], "seconds": 7200 }),
        ),
        (
            "update",
            json!({ "command": ["/usr/sbin/up", "{version}"], "seconds": 9 }),
        ),
        (
            "update",
            json!({ "command": ["/usr/sbin/up", "{version}"], "seconds": 7201 }),
        ),
        (
            "update",
            json!({ "command": ["/usr/sbin/up", "{version}"] }),
        ),
        (
            "update",
            json!({ "command": ["/usr/sbin/up"], "seconds": 60 }),
        ),
        (
            "update",
            json!({ "command": ["/usr/sbin/up", "{version}", "{version}"], "seconds": 60 }),
        ),
        (
            "update",
            json!({ "command": ["/usr/sbin/up", "{version}-{version}"], "seconds": 60 }),
        ),
        (
            "update",
            json!({ "command": ["/usr/sbin/{version}", "a"], "seconds": 60 }),
        ),
        (
            "update",
            json!({ "command": ["usr/sbin/up", "{version}"], "seconds": 60 }),
        ),
        (
            "update",
            json!({ "command": ["/usr/bin/apt-get", "install", "app={version}-1"], "seconds": 60 }),
        ),
        (
            "update",
            json!({ "command": ["/bin/sh", "-c", "apt-get install --yes app={version}"], "seconds": 60 }),
        ),
    ];
    let (mut accepted, mut refused) = (0, 0);
    for (part, value) in cases {
        let mut single =
            json!({ "update": update(), "health": health(), "version": version_reader() });
        single[part] = value.clone();
        let mut second = component("runtime");
        second[part] = value.clone();
        let several = json!({ "components": [component("server"), second] });
        match (load(&rig, system(single)), load(&rig, system(several))) {
            (Ok(single), Ok(several)) => {
                let single = single.app_update.unwrap();
                let several = several.app_update.unwrap();
                let component = &several.components.as_ref().unwrap()[1];
                assert_eq!(component.update, single.update, "{part}: {value}");
                assert_eq!(component.health, single.health, "{part}: {value}");
                assert_eq!(component.version, single.version, "{part}: {value}");
                accepted += 1;
            }
            (Err(single), Err(several)) => {
                assert_eq!(
                    several.message,
                    single
                        .message
                        .replacen("appUpdate.", "appUpdate.components[1].", 1),
                    "{part}: {value}"
                );
                refused += 1;
            }
            (single, several) => panic!(
                "{part}: {value}: the single form is {:?} and the component {:?}",
                single.map(|_| "read").map_err(|e| e.message),
                several.map(|_| "read").map_err(|e| e.message)
            ),
        }
    }
    // Both ways are proved, not only one.
    assert_eq!((accepted, refused), (10, 20));
    // A part a component needs, missing: the component's own words, as the single form says it of the app.
    for (part, why) in [
        (
            "update",
            "update is required: the command that moves the component to a version",
        ),
        (
            "health",
            "health is required: the command whose exit code 0 says the component is healthy",
        ),
        (
            "version",
            "version is required: the command that prints the component's version",
        ),
    ] {
        let mut second = component("runtime");
        second.as_object_mut().unwrap().remove(part);
        assert_eq!(
            refusal(
                &rig,
                system(json!({ "components": [component("server"), second] }))
            ),
            format!("template app: appUpdate.components[1].{why}")
        );
    }
}

/// `count` entries of `bytes` bytes each.
fn entries(count: usize, bytes: usize) -> Vec<String> {
    (0..count)
        .map(|at| format!("{}{}", at % 10, "x".repeat(bytes - 1)))
        .collect()
}

#[test]
fn also_restarts_is_0_to_16_entries_of_1_to_80_bytes_of_plain_text() {
    let rig = Rig::new();
    let with = |list: Value| {
        oci(json!({ "health": health(), "version": version_reader(), "alsoRestarts": list }))
    };
    for list in [
        vec![],
        entries(1, 1),
        entries(16, 80),
        vec!["the message server and the agents in this instance".to_string()],
        vec!["é".repeat(40)],
    ] {
        assert_eq!(formula(&rig, with(json!(list))).also_restarts, list);
    }
    assert_eq!(
        refusal(&rig, with(json!(entries(17, 1)))),
        "template app: appUpdate.alsoRestarts is a list of at most 16 entries"
    );
    for entry in [
        String::new(),
        "x".repeat(81),
        "é".repeat(41),
        "a\nb".into(),
        "a\u{202E}b".into(),
        "a\u{200B}".into(),
    ] {
        assert_eq!(
            refusal(&rig, with(json!(["ok", entry]))),
            "template app: appUpdate.alsoRestarts: each entry is 1 to 80 bytes of plain text, with no control or hidden character",
            "{entry:?}"
        );
    }
}

#[test]
fn not_updated_is_0_to_16_entries_each_a_name_of_1_to_64_bytes_and_a_reason_of_1_to_200() {
    let rig = Rig::new();
    let with = |list: Value| {
        oci(json!({ "health": health(), "version": version_reader(), "notUpdated": list }))
    };
    let entry = |name: String, reason: String| json!({ "name": name, "reason": reason });
    let sixteen: Vec<Value> = (0..16)
        .map(|at| entry(format!("part {at}"), "pinned".into()))
        .collect();
    for list in [
        json!([]),
        json!(sixteen),
        json!([entry("a".into(), "b".into())]),
        json!([entry("x".repeat(64), "y".repeat(200))]),
        json!([entry(
            "the runtime".into(),
            "pinned by the owner to a version the plugins need".into()
        )]),
    ] {
        let read = formula(&rig, with(list.clone()));
        assert_eq!(serde_json::to_value(&read.not_updated).unwrap(), list);
    }
    let mut seventeen = sixteen.clone();
    seventeen.push(entry("one more".into(), "pinned".into()));
    assert_eq!(
        refusal(&rig, with(json!(seventeen))),
        "template app: appUpdate.notUpdated is a list of at most 16 entries"
    );
    let ok = entry("ok".into(), "ok".into());
    for (bad, key, max) in [
        (entry(String::new(), "b".into()), "name", 64),
        (entry("x".repeat(65), "b".into()), "name", 64),
        (entry("a\tb".into(), "b".into()), "name", 64),
        (entry("a".into(), String::new()), "reason", 200),
        (entry("a".into(), "y".repeat(201)), "reason", 200),
        (entry("a".into(), "é".repeat(101)), "reason", 200),
        (entry("a".into(), "b\u{FEFF}".into()), "reason", 200),
    ] {
        assert_eq!(
            refusal(&rig, with(json!([ok.clone(), bad.clone()]))),
            format!(
                "template app: appUpdate.notUpdated[1].{key} is 1 to {max} bytes of plain text, with no control or hidden character"
            ),
            "{bad}"
        );
    }
}

#[test]
fn the_words_stand_beside_the_single_form_of_either_kind_and_beside_components() {
    let rig = Rig::new();
    let words = json!({
        "alsoRestarts": ["the message server"],
        "notUpdated": [{ "name": "the runtime", "reason": "pinned" }]
    });
    let mut on_oci = json!({ "health": health(), "version": version_reader() });
    let mut on_system =
        json!({ "update": update(), "health": health(), "version": version_reader() });
    let mut on_components = components(&["server", "plugins"]);
    for formula_value in [&mut on_oci, &mut on_system, &mut on_components] {
        for (key, value) in words.as_object().unwrap() {
            formula_value[key] = value.clone();
        }
    }
    for template in [oci(on_oci), system(on_system), system(on_components)] {
        let read = formula(&rig, template);
        assert_eq!(read.also_restarts, ["the message server"]);
        assert_eq!(read.not_updated[0].name, "the runtime");
        assert_eq!(read.not_updated[0].reason, "pinned");
    }
}

/// A system formula of two components of exactly `total` bytes as compact JSON, the second's health command padded.
fn components_of(total: usize) -> Value {
    let make = |command: &[String]| {
        let mut second = component("runtime");
        second["health"] = json!({ "command": command });
        json!({ "components": [component("server"), second], "alsoRestarts": ["the web server"] })
    };
    let mut command = vec!["/h".to_string()];
    let room = |command: &[String]| total - make(command).to_string().len();
    while room(&command) > 2 * (512 + 3) {
        command.push("a".repeat(512));
    }
    let left = room(&command);
    if left > 512 + 3 {
        let first = (left - 6) / 2;
        command.push("a".repeat(first));
        command.push("a".repeat(left - 6 - first));
    } else {
        command.push("a".repeat(left - 3));
    }
    let value = make(&command);
    assert_eq!(value.to_string().len(), total);
    value
}

#[test]
fn the_whole_field_with_components_is_at_most_8_kib_as_compact_json() {
    let rig = Rig::new();
    formula(&rig, system(components_of(8192)));
    assert_eq!(
        refusal(&rig, system(components_of(8193))),
        "template app: appUpdate is at most 8192 bytes as compact JSON, and this one is 8193"
    );
    // Eight components of the longest names with sixteen entries of each list stay under it as they are written.
    let names: Vec<String> = (0..8)
        .map(|at| format!("c{at}{}", "x".repeat(30)))
        .collect();
    let mut full = components(&names.iter().map(String::as_str).collect::<Vec<_>>());
    full["alsoRestarts"] = json!(entries(16, 80));
    full["notUpdated"] = json!(
        (0..16)
            .map(|at| json!({ "name": format!("part {at}"), "reason": "pinned" }))
            .collect::<Vec<_>>()
    );
    assert_eq!(formula(&rig, system(full)).component_names().len(), 8);
}

#[test]
fn a_components_arguments_keep_the_element_count_and_change_only_its_one_element() {
    let rig = Rig::new();
    let command = [
        "/usr/bin/apt-get",
        "install",
        "--yes",
        "app={version}-1",
        "--no-install-recommends",
    ];
    let mut runtime = component("runtime");
    runtime["update"]["command"] = json!(command);
    let mut plugins = component("plugins");
    plugins["update"]["command"] = json!(["/usr/local/sbin/plugins", "--to", "{version}"]);
    let read = formula(
        &rig,
        system(json!({ "components": [component("server"), runtime, plugins] })),
    );
    let version = AppVersion::parse("5:27.0.3-1~ubuntu.24.04").unwrap();
    let arguments = read
        .component_arguments("runtime", &version)
        .expect("a component named runtime");
    assert_eq!(arguments.len(), command.len());
    for (at, (made, declared)) in arguments.iter().zip(command).enumerate() {
        if at == 3 {
            assert_eq!(made, "app=5:27.0.3-1~ubuntu.24.04-1");
        } else {
            assert_eq!(made, declared, "{at}");
        }
    }
    // Each component's own command, and no other's.
    assert_eq!(
        read.component_arguments("plugins", &version),
        Some(vec![
            "/usr/local/sbin/plugins".to_string(),
            "--to".to_string(),
            "5:27.0.3-1~ubuntu.24.04".to_string()
        ])
    );
    assert_eq!(
        read.component_arguments("server", &AppVersion::parse("2.0").unwrap()),
        Some(vec![
            "/usr/local/sbin/app-update".to_string(),
            "2.0".to_string()
        ])
    );
    assert_eq!(read.component_arguments("absent", &version), None);
    assert_eq!(read.update_arguments(&version), None);
    // The same command in the single form makes the same list: one replacement for both.
    let single = formula(
        &rig,
        system(
            json!({ "update": { "command": command, "seconds": 60 }, "health": health(), "version": version_reader() }),
        ),
    );
    assert_eq!(single.update_arguments(&version), Some(arguments));
    assert_eq!(single.component_arguments("runtime", &version), None);
    assert!(single.component_names().is_empty());
}

#[test]
fn a_formula_of_the_single_form_reads_and_writes_back_as_before() {
    let rig = Rig::new();
    for value in [
        json!({ "health": health(), "version": version_reader() }),
        json!({ "health": { "command": ["/usr/bin/true"] }, "version": { "command": ["/v"], "seconds": 30 } }),
    ] {
        let read = formula(&rig, oci(value.clone()));
        assert!(read.components.is_none());
        assert!(read.also_restarts.is_empty() && read.not_updated.is_empty());
        assert_eq!(serde_json::to_value(&read).unwrap(), value);
    }
    let value = json!({ "update": update(), "health": health(), "version": version_reader() });
    let read = formula(&rig, system(value.clone()));
    assert_eq!(serde_json::to_value(&read).unwrap(), value);
}

/// `service_status` of `name`, which must meet the verb's result schema: the door checks every answer against it, and
/// a keyword outside its subset fails every answer.
fn status(rig: &Rig, name: &str) -> Value {
    let answer = rig
        .call("service_status", json!({ "name": name }))
        .expect("status");
    let verb = arca_api::verb("service_status").expect("verb");
    if let Err(why) = arca_api::conforms(&(verb.result)(), &answer) {
        panic!("the answer does not meet its schema: {why}: {answer}");
    }
    answer
}

#[test]
fn service_status_names_the_components_and_the_words_only_when_the_formula_has_them() {
    let rig = Rig::new();
    let mut plain = oci(json!({}));
    plain.as_object_mut().unwrap().remove("appUpdate");
    rig.template("app", plain.clone());
    rig.call(
        "service_create",
        json!({ "name": "app", "template": "app" }),
    )
    .expect("create");
    let without = status(&rig, "app");
    assert!(without.get("appUpdate").is_none(), "{without}");
    let other = |mut with: Value| {
        with.as_object_mut().unwrap().remove("appUpdate");
        with.to_string()
    };

    // The single form: exactly the block of before, and nothing else changes.
    rig.template(
        "app",
        oci(json!({ "health": health(), "version": version_reader() })),
    );
    let single = status(&rig, "app");
    assert_eq!(single["appUpdate"].to_string(), r#"{"formula":"template"}"#);
    assert_eq!(other(single), without.to_string());
    // Empty lists are no words: the block is the single form's.
    rig.template(
        "app",
        oci(
            json!({ "health": health(), "version": version_reader(), "alsoRestarts": [], "notUpdated": [] }),
        ),
    );
    assert_eq!(
        status(&rig, "app")["appUpdate"].to_string(),
        r#"{"formula":"template"}"#
    );

    // Beside the single form, each list on its own.
    rig.template(
        "app",
        oci(
            json!({ "health": health(), "version": version_reader(), "alsoRestarts": ["the web server", "the agents"] }),
        ),
    );
    let restarts = status(&rig, "app");
    assert_eq!(
        restarts["appUpdate"],
        json!({ "formula": "template", "alsoRestarts": ["the web server", "the agents"] })
    );
    assert_eq!(other(restarts), without.to_string());
    rig.template(
        "app",
        oci(
            json!({ "health": health(), "version": version_reader(), "notUpdated": [{ "name": "the runtime", "reason": "pinned" }] }),
        ),
    );
    assert_eq!(
        status(&rig, "app")["appUpdate"],
        json!({ "formula": "template", "notUpdated": [{ "name": "the runtime", "reason": "pinned" }] })
    );

    // A system template with components: their names in order, beside both lists. The template's kind differs from
    // the service's image, so only the block is compared; the formula is read from the template alone.
    let mut several = components(&["queue", "runtime", "plugins"]);
    several["alsoRestarts"] = json!(["the message server"]);
    several["notUpdated"] = json!([{ "name": "the policy bundle", "reason": "kept local" }]);
    rig.template("app", system(several));
    assert_eq!(
        status(&rig, "app")["appUpdate"],
        json!({
            "formula": "template",
            "components": ["queue", "runtime", "plugins"],
            "alsoRestarts": ["the message server"],
            "notUpdated": [{ "name": "the policy bundle", "reason": "kept local" }]
        })
    );
    rig.template("app", system(components(&["server"])));
    assert_eq!(
        status(&rig, "app")["appUpdate"],
        json!({ "formula": "template", "components": ["server"] })
    );

    // Back to the template without the field: byte for byte the first answer.
    rig.template("app", plain);
    assert_eq!(status(&rig, "app").to_string(), without.to_string());
}
