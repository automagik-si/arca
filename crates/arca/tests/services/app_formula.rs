//! A template's `appUpdate`, read strictly: which kind takes which part, every limit at its edge and one past it, where
//! `{version}` may stand, the plain version a call may name, the argument list a version makes, and what
//! `service_status` says of a formula. Nothing here runs a command. Every value is synthetic.
use super::*;
use arca::app_formula::{AppUpdate, AppVersion, version_ok};

fn health() -> Value {
    json!({ "command": ["/usr/bin/curl", "--fail", "--silent", "http://localhost/ready"], "seconds": 300 })
}

fn version_reader() -> Value {
    json!({ "command": ["/usr/bin/cat", "/opt/app/version.txt"] })
}

fn update() -> Value {
    json!({ "command": ["/usr/local/sbin/app-update", "{version}"], "seconds": 3600 })
}

fn oci(formula: Value) -> Value {
    json!({
        "name": "app", "kind": "oci", "image": "registry.example/app", "digest": DIGEST,
        "ports": [8080], "env": {}, "publishable": false, "appUpdate": formula
    })
}

fn system(formula: Value) -> Value {
    json!({
        "name": "app", "kind": "system", "image": "images:debian/13",
        "ports": [8080], "env": {}, "publishable": false, "appUpdate": formula
    })
}

fn machine(formula: Value) -> Value {
    json!({
        "name": "app", "kind": "virtual-machine", "image": "images:debian/13/cloud",
        "limits": { "cpu": "2", "memory": "4GiB" }, "rootSize": "20GiB", "appUpdate": formula
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

/// A system template whose update command is `command`, with the usual health and version parts.
fn system_command(command: Value) -> Value {
    system(json!({
        "update": { "command": command, "seconds": 60 },
        "health": health(),
        "version": version_reader()
    }))
}

#[test]
fn an_oci_template_takes_health_and_version_with_their_defaults() {
    let rig = Rig::new();
    let read = formula(
        &rig,
        oci(json!({ "health": { "command": ["/usr/bin/true"] }, "version": version_reader() })),
    );
    assert!(read.update.is_none());
    assert_eq!(read.health_seconds(), 120);
    assert_eq!(read.version_seconds(), 15);
    assert_eq!(read.update_seconds(), None);
    assert_eq!(
        read.update_arguments(&AppVersion::parse("1.2.3").unwrap()),
        None
    );
    let read = formula(
        &rig,
        oci(
            json!({ "health": health(), "version": { "command": ["/usr/bin/cat", "/v"], "seconds": 30 } }),
        ),
    );
    assert_eq!((read.health_seconds(), read.version_seconds()), (300, 30));
}

#[test]
fn a_system_template_takes_all_three_parts() {
    let rig = Rig::new();
    let read = formula(
        &rig,
        system(json!({ "update": update(), "health": health(), "version": version_reader() })),
    );
    assert_eq!(read.update_seconds(), Some(3600));
    assert_eq!(read.health_seconds(), 300);
    assert_eq!(read.version_seconds(), 15);
}

#[test]
fn each_shape_on_the_wrong_kind_is_refused_with_a_message_naming_the_field() {
    let rig = Rig::new();
    let three = json!({ "update": update(), "health": health(), "version": version_reader() });
    let two = json!({ "health": health(), "version": version_reader() });
    assert_eq!(
        refusal(&rig, oci(three.clone())),
        "template app: appUpdate.update is only for a system container template: an OCI template's update is the move to another image digest"
    );
    assert_eq!(
        refusal(&rig, system(two.clone())),
        "template app: appUpdate.update is required on a system container template: the command that moves the app to a version"
    );
    for formula in [two, three] {
        assert_eq!(
            refusal(&rig, machine(formula)),
            "template app: appUpdate is only for an OCI or a system container template, never a virtual machine"
        );
    }
    // The same virtual-machine template without the field is read, so the refusal above is the field's.
    let mut plain = machine(json!({}));
    plain.as_object_mut().unwrap().remove("appUpdate");
    load(&rig, plain).expect("the machine template without appUpdate");
    // A part a kind needs, missing.
    for template in [
        oci(json!({ "version": version_reader() })),
        system(json!({ "update": update(), "version": version_reader() })),
    ] {
        assert!(refusal(&rig, template).starts_with("template app: appUpdate.health is required"),);
    }
    for template in [
        oci(json!({ "health": health() })),
        system(json!({ "update": update(), "health": health() })),
    ] {
        assert!(refusal(&rig, template).starts_with("template app: appUpdate.version is required"),);
    }
}

#[test]
fn the_field_and_each_part_are_strict() {
    let rig = Rig::new();
    for template in [
        oci(json!({ "health": health(), "version": version_reader(), "other": true })),
        oci(
            json!({ "health": { "command": ["/usr/bin/true"], "every": 5 }, "version": version_reader() }),
        ),
        oci(json!({ "health": { "command": "/usr/bin/true" }, "version": version_reader() })),
        oci(
            json!({ "health": { "command": ["/usr/bin/true"], "seconds": -1 }, "version": version_reader() }),
        ),
        oci(
            json!({ "health": { "command": ["/usr/bin/true"], "seconds": "10" }, "version": version_reader() }),
        ),
        oci(json!({ "health": { "seconds": 10 }, "version": version_reader() })),
        oci(json!("/usr/bin/true")),
    ] {
        assert_eq!(refusal(&rig, template), "template app: cannot be read");
    }
}

#[test]
fn a_command_is_1_to_32_elements_of_1_to_512_bytes_of_plain_text_the_first_an_absolute_path() {
    let rig = Rig::new();
    let long = |bytes: usize| format!("/{}", "a".repeat(bytes - 1));
    let mut thirty_two = vec![json!("/usr/bin/true")];
    thirty_two.extend((1..32).map(|at| json!(format!("a{at}"))));
    let mut thirty_three = thirty_two.clone();
    thirty_three.push(json!("one-more"));
    for part in ["health", "version"] {
        let with = |command: Value| {
            let mut parts = json!({ "health": health(), "version": version_reader() });
            parts[part] = json!({ "command": command });
            oci(parts)
        };
        for command in [
            json!(thirty_two.clone()),
            json!(["/"]),
            json!([long(512)]),
            json!(["/usr/bin/true", "a"]),
            json!(["/usr/bin/true", "a".repeat(512)]),
        ] {
            formula(&rig, with(command));
        }
        let prefix = format!("template app: appUpdate.{part}.command");
        for command in [
            json!([]),
            json!(thirty_three.clone()),
            json!([long(513)]),
            json!(["/usr/bin/true", "a".repeat(513)]),
            json!(["/usr/bin/true", ""]),
            json!([""]),
            json!(["/usr/bin/true", "a\nb"]),
            json!(["/usr/bin/true", "a\u{202E}b"]),
            json!(["usr/bin/true"]),
            json!(["true"]),
            json!(["./true"]),
        ] {
            let why = refusal(&rig, with(command.clone()));
            assert!(why.starts_with(&prefix), "{command}: {why}");
        }
    }
    // The update command holds to the same list limits.
    let mut update_32 = thirty_two.clone();
    update_32[31] = json!("{version}");
    formula(&rig, system_command(json!(update_32)));
    let mut update_33 = thirty_three.clone();
    update_33[32] = json!("{version}");
    for command in [
        json!(update_33),
        json!(["/usr/sbin/up", "{version}", ""]),
        json!(["/usr/sbin/up", "{version}", "a".repeat(513)]),
        json!(["usr/sbin/up", "{version}"]),
    ] {
        let why = refusal(&rig, system_command(command.clone()));
        assert!(
            why.starts_with("template app: appUpdate.update.command"),
            "{command}: {why}"
        );
    }
}

#[test]
fn seconds_hold_to_the_table_at_each_edge_and_one_past() {
    let rig = Rig::new();
    let with_update = |seconds: Option<u64>| {
        let mut part = json!({ "command": ["/usr/sbin/up", "{version}"] });
        if let Some(seconds) = seconds {
            part["seconds"] = json!(seconds);
        }
        system(json!({ "update": part, "health": health(), "version": version_reader() }))
    };
    for seconds in [10, 7200] {
        assert_eq!(
            formula(&rig, with_update(Some(seconds))).update_seconds(),
            Some(seconds)
        );
    }
    for seconds in [None, Some(0), Some(9), Some(7201)] {
        assert_eq!(
            refusal(&rig, with_update(seconds)),
            "template app: appUpdate.update.seconds is required, from 10 to 7200"
        );
    }
    let with = |part: &str, seconds: u64| {
        let mut parts = json!({ "health": health(), "version": version_reader() });
        parts[part]["seconds"] = json!(seconds);
        oci(parts)
    };
    for seconds in [1, 900] {
        assert_eq!(
            formula(&rig, with("health", seconds)).health_seconds(),
            seconds
        );
    }
    for seconds in [0, 901] {
        assert_eq!(
            refusal(&rig, with("health", seconds)),
            "template app: appUpdate.health.seconds is from 1 to 900"
        );
    }
    for seconds in [1, 60] {
        assert_eq!(
            formula(&rig, with("version", seconds)).version_seconds(),
            seconds
        );
    }
    for seconds in [0, 61] {
        assert_eq!(
            refusal(&rig, with("version", seconds)),
            "template app: appUpdate.version.seconds is from 1 to 60"
        );
    }
}

/// An OCI formula of exactly `total` bytes as compact JSON: its health command padded with elements of at most 512 bytes.
fn formula_of(total: usize) -> Value {
    let make = |command: &[String]| json!({ "health": { "command": command }, "version": { "command": ["/v"] } });
    let mut command = vec!["/h".to_string()];
    let room = |command: &[String]| total - make(command).to_string().len();
    // Each element adds its bytes, its two quotes and a comma.
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
fn the_whole_field_is_at_most_8_kib_as_compact_json() {
    let rig = Rig::new();
    formula(&rig, oci(formula_of(8192)));
    assert_eq!(
        refusal(&rig, oci(formula_of(8193))),
        "template app: appUpdate is at most 8192 bytes as compact JSON, and this one is 8193"
    );
    // Spaces in the file do not count: the size is of the field as compact JSON.
    let rig_dir = rig.dir.join("templates");
    std::fs::create_dir_all(&rig_dir).unwrap();
    std::fs::write(
        rig_dir.join("app.json"),
        serde_json::to_string_pretty(&oci(formula_of(8192))).unwrap(),
    )
    .unwrap();
    service::load_template(&rig_dir, "app").expect("the pretty file of an 8192-byte field");
}

#[test]
fn version_stands_once_in_one_element_of_the_update_command_never_the_first_and_nowhere_else() {
    let rig = Rig::new();
    for command in [
        json!(["/usr/sbin/up", "{version}"]),
        json!(["/usr/bin/apt-get", "install", "--yes", "app={version}-1"]),
        json!(["/usr/sbin/up", "--to", "{version}", "--quiet"]),
    ] {
        formula(&rig, system_command(command));
    }
    let placement = "template app: appUpdate.update.command holds {version} exactly once, in one argument that is not the first";
    for command in [
        json!(["/usr/sbin/up"]),
        json!(["/usr/sbin/up", "latest"]),
        json!(["/usr/sbin/up", "{version}", "{version}"]),
        json!(["/usr/sbin/up", "{version}-{version}"]),
        json!(["/usr/sbin/{version}"]),
        json!(["/{version}", "{version}"]),
        json!(["/usr/sbin/{version}", "a"]),
    ] {
        assert_eq!(
            refusal(&rig, system_command(command.clone())),
            placement,
            "{command}"
        );
    }
    for part in ["health", "version"] {
        let mut parts =
            json!({ "update": update(), "health": health(), "version": version_reader() });
        parts[part]["command"] = json!(["/usr/bin/check", "{version}"]);
        assert_eq!(
            refusal(&rig, system(parts.clone())),
            format!(
                "template app: appUpdate.{part}.command does not hold {{version}}: only update.command does"
            )
        );
        parts.as_object_mut().unwrap().remove("update");
        assert!(refusal(&rig, oci(parts)).starts_with(&format!(
            "template app: appUpdate.{part}.command does not hold"
        )));
    }
}

#[test]
fn hostile_versions_are_refused_and_plain_ones_pass() {
    let longest = format!("1{}", "a".repeat(63));
    let too_long = format!("1{}", "a".repeat(64));
    for good in [
        "1",
        "a",
        "19.4.8",
        "19.4.8-ce.0",
        "5:27.0.3-1~ubuntu.24.04",
        "1:2.3_4+5~6",
        "v1.2.3",
        "2026.11.18",
        longest.as_str(),
    ] {
        assert!(version_ok(good), "{good}");
        assert_eq!(AppVersion::parse(good).unwrap().as_str(), good);
    }
    for hostile in [
        "",
        too_long.as_str(),
        "1 2",
        " 1",
        "1'",
        "1\"",
        "$(id)",
        "1$(id)",
        "1;id",
        "-1",
        "--force",
        ".1",
        "~1",
        ":1",
        "1\n2",
        "1\n",
        "1\t2",
        "é1",
        "1é",
        "1/2",
        "1\\2",
        "1`id`",
        "1|id",
        "1&id",
        "1>x",
        "1*",
        "1{version}",
        "1\u{0}",
    ] {
        assert!(!version_ok(hostile), "{hostile:?}");
        assert!(AppVersion::parse(hostile).is_err(), "{hostile:?}");
    }
}

#[test]
fn the_replacement_keeps_the_element_count_changes_only_its_one_element_and_leaves_the_first_alone()
{
    let rig = Rig::new();
    let command = [
        "/usr/bin/apt-get",
        "install",
        "--yes",
        "app={version}-1",
        "--no-install-recommends",
    ];
    let read = formula(&rig, system_command(json!(command)));
    let version = AppVersion::parse("5:27.0.3-1~ubuntu.24.04").unwrap();
    let arguments = read.update_arguments(&version).expect("an update part");
    assert_eq!(arguments.len(), command.len());
    assert_eq!(arguments[0], command[0]);
    for (at, (made, declared)) in arguments.iter().zip(command).enumerate() {
        if at == 3 {
            assert_eq!(made, "app=5:27.0.3-1~ubuntu.24.04-1");
        } else {
            assert_eq!(made, declared, "{at}");
        }
    }
    // A version that fills its whole argument stays one argument, and the program is the formula's.
    let read = formula(&rig, system_command(json!(["/usr/sbin/up", "{version}"])));
    assert_eq!(
        read.update_arguments(&AppVersion::parse("19.4.8").unwrap()),
        Some(vec!["/usr/sbin/up".to_string(), "19.4.8".to_string()])
    );
}

/// What the rules do not refuse, by decision: a formula that hands its version to a program which runs it. Such a
/// template has given the version that power; the version is still one plain argument with no space, quote or `$`.
#[test]
fn a_formula_that_hands_its_version_to_a_shell_or_to_env_is_accepted() {
    let rig = Rig::new();
    for command in [
        json!(["/bin/sh", "-c", "apt-get install --yes app={version}"]),
        json!(["/usr/bin/env", "{version}"]),
        json!(["/usr/bin/xargs", "-I", "x", "{version}"]),
    ] {
        let read = formula(&rig, system_command(command.clone()));
        let arguments = read
            .update_arguments(&AppVersion::parse("1.2.3").unwrap())
            .unwrap();
        assert_eq!(arguments.len(), command.as_array().unwrap().len());
    }
}

#[test]
fn every_shipped_template_reads_as_before_with_no_formula() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../templates");
    let mut read = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        let Some(name) = name.strip_suffix(".json") else {
            continue;
        };
        if name.starts_with("package") {
            continue;
        }
        let template = service::load_template(&dir, name).expect("the shipped template loads");
        assert!(template.app_update.is_none(), "{name}");
        let text = std::fs::read_to_string(dir.join(format!("{name}.json"))).unwrap();
        assert!(!text.contains("appUpdate"), "{name}");
        read += 1;
    }
    assert_eq!(read, 15);
}

/// `service_status` of `name`.
fn status(rig: &Rig, name: &str) -> Value {
    rig.call("service_status", json!({ "name": name }))
        .expect("status")
}

#[test]
fn service_status_says_a_formula_comes_from_the_template_and_is_unchanged_without_one() {
    let rig = Rig::new();
    let mut plain = oci(json!({}));
    plain.as_object_mut().unwrap().remove("appUpdate");
    rig.template("app", plain.clone());
    rig.call(
        "service_create",
        json!({ "name": "app", "template": "app" }),
    )
    .expect("create");
    let record = rig.service_config("app");
    let listed = rig.call("service_list", json!({})).expect("list");
    let without = status(&rig, "app");
    assert!(without.get("appUpdate").is_none(), "{without}");
    // The same read again is the same bytes, so the comparison below compares only the formula.
    assert_eq!(status(&rig, "app").to_string(), without.to_string());

    // The installed template gains a formula: the status gains the block and nothing else changes.
    rig.template(
        "app",
        oci(json!({ "health": health(), "version": version_reader() })),
    );
    let mut with = status(&rig, "app");
    assert_eq!(with["appUpdate"], json!({ "formula": "template" }));
    with.as_object_mut().unwrap().remove("appUpdate");
    assert_eq!(with.to_string(), without.to_string());
    // The list, the record and the service are as they were: the formula is the template's.
    assert_eq!(rig.call("service_list", json!({})).expect("list"), listed);
    assert_eq!(
        serde_json::to_value(rig.service_config("app")).unwrap(),
        serde_json::to_value(record).unwrap()
    );

    // A template whose formula is refused cannot be used as it is: no block, and `update` says why.
    rig.template(
        "app",
        oci(json!({ "update": update(), "health": health(), "version": version_reader() })),
    );
    let refused = status(&rig, "app");
    assert!(refused.get("appUpdate").is_none(), "{refused}");
    assert_eq!(refused["update"]["state"], "unknown", "{refused}");

    // Back to the template without the field: byte for byte the first answer.
    rig.template("app", plain);
    assert_eq!(status(&rig, "app").to_string(), without.to_string());
}
