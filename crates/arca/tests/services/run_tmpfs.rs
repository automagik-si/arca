//! A container service's `/run` is memory: the image's `/run` is emptied on the root disk before the first start, so
//! Incus mounts a tmpfs there, in a project as Arca makes it (restricted, low-level config refused). The fake models
//! Incus's rule: a container gets a memory `/run` at a start only when its root holds nothing under `/run`. Every value is
//! synthetic.
use super::*;

/// What a Debian-based image keeps under `/run`: a directory with a file in it, and a file.
const IMAGE_RUN: [&str; 3] = ["/run/lock/", "/run/lock/.keep", "/run/motd"];

fn template(name: &str) -> Value {
    json!({
        "name": name,
        "kind": "oci",
        "image": "registry.example/app",
        "digest": DIGEST,
        "ports": [8080],
        "env": { "MODE": "one" },
        "publishable": false
    })
}

fn instance(rig: &Rig, name: &str) -> Value {
    rig.get(&format!("/1.0/instances/{name}?project={PROJECT}"))
        .expect("instance")
}

/// The create of `name`'s instance as it was sent to Incus, and every state change sent to it, in order.
fn create_and_states(rig: &Rig, name: &str) -> (Value, Vec<Value>) {
    let requests = rig.fake.service_requests();
    let create = requests
        .iter()
        .filter(|request| request.method == "POST" && request.path.starts_with("/1.0/instances?"))
        .filter_map(|request| serde_json::from_slice::<Value>(&request.body).ok())
        .find(|body| body["name"] == name)
        .expect("the create");
    let states = requests
        .iter()
        .filter(|request| {
            request.method == "PUT"
                && request
                    .path
                    .starts_with(&format!("/1.0/instances/{name}/state?"))
        })
        .filter_map(|request| serde_json::from_slice::<Value>(&request.body).ok())
        .collect();
    (create, states)
}

#[test]
fn a_new_service_gets_a_memory_run_even_when_its_image_keeps_files_there() {
    let rig = Rig::new();
    rig.fake.set_image_run(&IMAGE_RUN);
    rig.template("app", template("app"));
    let made = rig
        .call(
            "service_create",
            json!({ "name": "app", "template": "app" }),
        )
        .expect("create");
    assert_eq!(made["run"], "memory", "{made}");
    assert!(made.get("runNote").is_none(), "{made}");

    // The generated config carries the mark, and the record says it.
    let config = instance(&rig, "app")["config"].clone();
    assert_eq!(config[service::RUN_TMPFS_KEY], "true", "{config}");
    assert!(
        config.get("raw.lxc").is_none(),
        "no low-level config is needed for it"
    );
    assert!(rig.service_config("app").run_tmpfs);

    // Created stopped, the image's /run emptied, then started as the create asked: Incus mounted a memory /run.
    let (create, states) = create_and_states(&rig, "app");
    assert_eq!(create["start"], false, "{create}");
    assert_eq!(create["config"][service::RUN_TMPFS_KEY], "true");
    assert_eq!(states, vec![json!({ "action": "start" })]);
    assert!(rig.fake.run_on_disk(PROJECT, "app").is_empty());
    assert!(rig.fake.run_in_memory(PROJECT, "app"));
    assert_eq!(instance(&rig, "app")["status"], "Running");

    // A reconcile of the same record writes nothing and keeps the mark.
    let writes = rig.fake.writes().len();
    rig.apply(&rig.service_config("app")).expect("reconcile");
    assert_eq!(rig.fake.writes().len(), writes, "nothing to write");
    assert_eq!(
        instance(&rig, "app")["config"][service::RUN_TMPFS_KEY],
        "true"
    );
}

#[test]
fn a_service_created_stopped_has_an_empty_run_on_its_disk_for_its_first_start() {
    let rig = Rig::new();
    rig.fake.set_image_run(&IMAGE_RUN);
    rig.template("app", template("app"));
    let made = rig
        .call(
            "service_create",
            json!({ "name": "app", "template": "app", "start": false }),
        )
        .expect("create");
    assert_eq!(made["run"], "memory", "{made}");
    let (create, states) = create_and_states(&rig, "app");
    assert_eq!(create["start"], false);
    assert!(states.is_empty(), "nothing started it: {states:?}");
    assert!(rig.fake.run_on_disk(PROJECT, "app").is_empty());
    rig.edit(
        "PUT",
        &format!("/1.0/instances/app/state?project={PROJECT}"),
        json!({ "action": "start" }),
    );
    assert!(rig.fake.run_in_memory(PROJECT, "app"));
}

#[test]
fn a_template_that_opts_out_keeps_the_files_its_image_ships_in_run() {
    let rig = Rig::new();
    rig.fake.set_image_run(&IMAGE_RUN);
    let mut own = template("own");
    own["runTmpfs"] = json!(false);
    rig.template("own", own);
    let made = rig
        .call(
            "service_create",
            json!({ "name": "own", "template": "own" }),
        )
        .expect("create");
    assert!(made.get("run").is_none(), "{made}");
    let config = instance(&rig, "own")["config"].clone();
    assert!(config.get(service::RUN_TMPFS_KEY).is_none(), "{config}");
    assert!(!rig.service_config("own").run_tmpfs);
    let (create, states) = create_and_states(&rig, "own");
    assert_eq!(create["start"], true, "created as it always was");
    assert!(states.is_empty());
    assert_eq!(rig.fake.run_on_disk(PROJECT, "own"), IMAGE_RUN.to_vec());
    assert!(!rig.fake.run_in_memory(PROJECT, "own"));
}

#[test]
fn a_run_that_cannot_be_emptied_leaves_the_service_created_as_before_and_says_so() {
    let rig = Rig::new();
    // More entries than one emptying removes.
    let many: Vec<String> = (0..300).map(|n| format!("/run/f{n:03}")).collect();
    rig.fake
        .set_image_run(&many.iter().map(String::as_str).collect::<Vec<_>>());
    rig.template("app", template("app"));
    let made = rig
        .call(
            "service_create",
            json!({ "name": "app", "template": "app" }),
        )
        .expect("the create goes on");
    assert_eq!(made["run"], "disk", "{made}");
    assert!(
        made["runNote"]
            .as_str()
            .is_some_and(|note| note.starts_with("/run stays on the container's disk")),
        "{made}"
    );
    assert_eq!(instance(&rig, "app")["status"], "Running");
    assert!(!rig.fake.run_in_memory(PROJECT, "app"));
}

#[test]
fn a_templates_own_raw_lxc_lines_are_kept_beside_the_memory_run() {
    let rig = Rig::new();
    rig.template("app", template("app"));
    rig.call(
        "service_create",
        json!({ "name": "app", "template": "app" }),
    )
    .expect("the first service makes the project");
    rig.allow_low_level();
    rig.fake.set_image_run(&IMAGE_RUN);
    let mut strict = template("strict");
    strict["noNewPrivileges"] = json!(true);
    strict["rawLxc"] = json!(["lxc.sched.core=0"]);
    rig.template("strict", strict);
    let made = rig
        .call(
            "service_create",
            json!({ "name": "strict", "template": "strict" }),
        )
        .expect("create");
    assert_eq!(made["run"], "memory", "{made}");
    let config = instance(&rig, "strict")["config"].clone();
    assert_eq!(config["raw.lxc"], "lxc.no_new_privs=1\nlxc.sched.core=0");
    assert_eq!(
        config[service::RAW_LXC_KEY],
        "lxc.no_new_privs=1\nlxc.sched.core=0"
    );
    assert_eq!(config[service::RUN_TMPFS_KEY], "true");
    let record = rig.service_config("strict");
    assert_eq!(record.raw_lxc, vec!["lxc.sched.core=0".to_string()]);
    assert!(record.no_new_privileges && record.run_tmpfs);
    assert!(rig.fake.run_in_memory(PROJECT, "strict"));
    rig.apply(&rig.service_config("strict")).expect("reconcile");
    let config = instance(&rig, "strict")["config"].clone();
    assert_eq!(config["raw.lxc"], "lxc.no_new_privs=1\nlxc.sched.core=0");
    assert_eq!(config[service::RUN_TMPFS_KEY], "true");
}
