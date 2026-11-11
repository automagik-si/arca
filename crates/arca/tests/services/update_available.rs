//! What `service_status` and `service_list` say of the version a service runs beside the one its template, as
//! installed on the node, names now: for a virtual machine whose template names its version's setting, and for a
//! container made from an image by digest, against a fake Incus. Nothing is asked of a registry, and nothing is
//! written. Every value is synthetic.
use super::*;

const OLD: &str =
    "registry.example/app@sha256:1111111111111111111111111111111111111111111111111111111111111111";
const NEW: &str =
    "registry.example/app@sha256:2222222222222222222222222222222222222222222222222222222222222222";
const OTHER_DIGEST: &str =
    "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// A machine template that names the version its service runs, under the setting `appImage`.
fn box_template(pin: &str) -> Value {
    json!({
        "name": "box",
        "kind": "virtual-machine",
        "image": "images:debian/13/cloud",
        "limits": { "cpu": "2", "memory": "4GiB" },
        "rootSize": "20GiB",
        "settings": { "appImage": pin, "address": "https://box.example", "note": "plain text" },
        "createSettings": ["address", "note"],
        "update": { "pin": "appImage" }
    })
}

fn rig() -> Rig {
    let rig = Rig::with(mixed_kernel(&["br0"], roomy()), None, vec![]);
    rig.template("box", box_template(OLD));
    rig
}

fn status(rig: &Rig, name: &str) -> Value {
    rig.call("service_status", json!({ "name": name }))
        .expect("status")
}

/// The row of `name` in `service_list`.
fn listed(rig: &Rig, name: &str) -> Value {
    rig.call("service_list", json!({})).expect("list")["services"]
        .as_array()
        .expect("services")
        .iter()
        .find(|row| row["name"] == name)
        .unwrap_or_else(|| panic!("{name} is listed"))
        .clone()
}

#[test]
fn a_machine_says_whether_its_installed_template_names_the_version_it_was_created_with() {
    let rig = rig();
    rig.call("service_create", json!({ "name": "b1", "template": "box" }))
        .expect("create");
    let instance_before = instance(&rig, "b1");
    let record_before = entry(&rig, "service-b1").config;

    // The template is the one the machine was created from: the same version, and the listing says nothing.
    assert_eq!(
        status(&rig, "b1")["update"],
        json!({ "pin": OLD, "templatePin": OLD, "state": "same" })
    );
    assert!(listed(&rig, "b1").get("update").is_none());

    // The installed template names another version. The status names both; the listing says only that it differs.
    rig.template("box", box_template(NEW));
    assert_eq!(
        status(&rig, "b1")["update"],
        json!({ "pin": OLD, "templatePin": NEW, "state": "differs" })
    );
    assert_eq!(listed(&rig, "b1")["update"], json!({ "state": "differs" }));
    // A setting a create may give is not the version: a template that changes one of those says the same.
    let mut other_note = box_template(OLD);
    other_note["settings"]["note"] = json!("edited later");
    rig.template("box", other_note);
    assert_eq!(status(&rig, "b1")["update"]["state"], "same");

    // Reading it changed nothing: the instance and the record are the ones the create left.
    rig.template("box", box_template(NEW));
    status(&rig, "b1");
    listed(&rig, "b1");
    assert_eq!(instance(&rig, "b1"), instance_before);
    assert_eq!(entry(&rig, "service-b1").config, record_before);
}

#[test]
fn a_template_that_cannot_say_leaves_the_version_unknown_with_the_reason() {
    let rig = rig();
    rig.call("service_create", json!({ "name": "b1", "template": "box" }))
        .expect("create");
    let file = rig.dir.join("templates").join("box.json");

    // A template that cannot be read any more: the machine's own version is not known without its key, so nothing
    // is said; a template that names no version says nothing either.
    fs::write(&file, "{").expect("a broken template");
    assert!(status(&rig, "b1").get("update").is_none());
    let mut plain = box_template(OLD);
    plain.as_object_mut().unwrap().remove("update");
    rig.template("box", plain);
    assert!(status(&rig, "b1").get("update").is_none());
    assert!(listed(&rig, "b1").get("update").is_none());
    fs::remove_file(&file).expect("remove the template");
    assert!(status(&rig, "b1").get("update").is_none());
    assert!(listed(&rig, "b1").get("update").is_none());

    // A container's version is in its own record, so its status still names it when the template is gone, cannot
    // be used, or is a machine's now.
    rig.call(
        "service_create",
        json!({ "name": "n1", "template": "nats" }),
    )
    .expect("create");
    let pin = format!("docker.io/library/nats@{DIGEST}");
    let nats = rig.dir.join("templates").join("nats.json");
    fs::remove_file(&nats).expect("remove the template");
    let update = status(&rig, "n1")["update"].clone();
    assert_eq!(update["pin"], pin.as_str());
    assert_eq!(update["state"], "unknown");
    assert!(update.get("templatePin").is_none());
    assert_eq!(
        update["problem"],
        "template nats is not installed on this node any more"
    );
    assert!(listed(&rig, "n1").get("update").is_none());
    fs::write(&nats, "{").expect("a broken template");
    assert_eq!(
        status(&rig, "n1")["update"],
        json!({ "pin": pin, "state": "unknown",
            "problem": "template nats on this node cannot be used as it is" })
    );
    let mut machine = box_template(NEW);
    machine["name"] = json!("nats");
    rig.template("nats", machine);
    assert_eq!(
        status(&rig, "n1")["update"],
        json!({ "pin": pin, "state": "unknown",
            "problem": "template nats on this node is of another kind now, or names no version" })
    );
    assert!(listed(&rig, "n1").get("update").is_none());
}

#[test]
fn a_container_says_whether_its_installed_template_names_the_digest_it_was_created_from() {
    let rig = rig();
    rig.call(
        "service_create",
        json!({ "name": "n1", "template": "nats" }),
    )
    .expect("create");
    let pin = format!("docker.io/library/nats@{DIGEST}");
    assert_eq!(
        status(&rig, "n1")["update"],
        json!({ "pin": pin, "templatePin": pin, "state": "same" })
    );
    assert!(listed(&rig, "n1").get("update").is_none());

    let mut newer = nats_template();
    newer["digest"] = json!(OTHER_DIGEST);
    rig.template("nats", newer);
    assert_eq!(
        status(&rig, "n1")["update"],
        json!({ "pin": pin, "state": "differs",
            "templatePin": format!("docker.io/library/nats@{OTHER_DIGEST}") })
    );
    assert_eq!(listed(&rig, "n1")["update"], json!({ "state": "differs" }));

    // A machine made from a system image, with no version of its own, says nothing at all.
    rig.call("service_create", json!({ "name": "ws1", "template": "ws" }))
        .expect("create");
    assert!(status(&rig, "ws1").get("update").is_none());
    assert!(listed(&rig, "ws1").get("update").is_none());
}

#[test]
fn update_is_a_machine_templates_key_and_its_pin_is_an_image_by_digest_no_create_may_set() {
    let rig = rig();
    let refused = |template: Value| {
        rig.template("box", template);
        refusal(rig.call("service_create", json!({ "name": "b9", "template": "box" })))
    };
    let pin_rule = "template box: update.pin names a key of settings that createSettings does not list, whose value is an image by digest: <registry>/<name>@sha256:<64 hex digits>";
    // A key the settings do not have, one a create may set, and values that are not an image by digest.
    let mut absent = box_template(OLD);
    absent["update"]["pin"] = json!("nothing");
    assert_eq!(refused(absent), pin_rule);
    let mut settable = box_template(OLD);
    settable["createSettings"] = json!(["address", "note", "appImage"]);
    assert_eq!(refused(settable), pin_rule);
    for value in [
        json!("registry.example/app:latest"),
        json!("registry.example/app@sha256:1111"),
        json!("app@sha256:1111111111111111111111111111111111111111111111111111111111111111"),
        json!(
            "Registry.example/app@sha256:1111111111111111111111111111111111111111111111111111111111111111"
        ),
        json!(7),
    ] {
        let mut template = box_template(OLD);
        template["settings"]["appImage"] = value;
        assert_eq!(refused(template), pin_rule);
    }
    // No key but the pin.
    let mut extra = box_template(OLD);
    extra["update"]["more"] = json!(true);
    assert_eq!(refused(extra), "template box: cannot be read");
    // A container's version is its digest: its template carries no update.
    let mut container = nats_template();
    container["update"] = json!({ "pin": "appImage" });
    rig.template("nats", container);
    assert_eq!(
        refusal(rig.call(
            "service_create",
            json!({ "name": "n9", "template": "nats" })
        )),
        "template nats: only a virtual-machine template has an update; a container's version is its digest"
    );
    assert!(rig.entry_ids().iter().all(|id| !id.starts_with("service-")));
}
