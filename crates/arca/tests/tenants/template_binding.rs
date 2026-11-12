//! What the owner saw is what is created. A proposed create is given, by the node, the image its template names and
//! the fingerprint of the template's content (its file and each script it names). The card shows both, the approval
//! binds both, and the create is made from one read of the template that must still be exactly that one. Every image
//! and address here is synthetic, the shipped database template's own aside.
use super::*;
use arca::service::DATABASE_TEMPLATE;
use sha2::{Digest, Sha256};

const WORKLOAD: &str = "registry.example/automagik/server@sha256:5555555555555555555555555555555555555555555555555555555555555555";
const FINGERPRINT: &str = "sha256:6666666666666666666666666666666666666666666666666666666666666666";
const NO_DIGEST: &str =
    " (no digest: Incus takes what that name gives when the service is created)";

fn kernel() -> Value {
    json!({
        "projects": [PROJECT],
        "droiddProjects": ["other"],
        "serviceCidr": CIDR,
        "vmLimits": { "virtualMachines": 4, "cpu": 16, "memory": "64GiB", "disk": "1TiB" }
    })
}

fn templates(rig: &Rig) -> std::path::PathBuf {
    rig.dir.join("templates")
}

/// A container template whose entrypoint is a script beside it.
fn scripted() -> Value {
    json!({
        "name": "scripted", "kind": "oci", "image": "docker.io/library/app", "digest": DIGEST,
        "entrypoint": ["/bin/sh", "-ec", { "file": "scripted.sh" }, "scripted"],
        "ports": [8080], "env": { "MODE": "one" }, "publishable": false
    })
}

/// A virtual machine that names the version it runs and a first-boot script, on a system image with no digest.
fn machine() -> Value {
    json!({
        "name": "machine", "kind": "virtual-machine", "image": "images:debian/13/cloud",
        "limits": { "cpu": "2", "memory": "4GiB" }, "rootSize": "20GiB", "ports": [22],
        "firstBoot": { "file": "machine.sh" },
        "update": { "pin": "serverImage" },
        "settings": { "serverImage": WORKLOAD }
    })
}

/// A container from a local image the template pins by its fingerprint: an image somebody put in the service
/// project by hand, under the alias `built`.
fn built(fingerprint: &str) -> Value {
    json!({
        "name": "built", "kind": "system", "image": "local:built",
        "digest": format!("sha256:{fingerprint}"), "ports": [], "env": {}, "publishable": false
    })
}

/// A virtual machine on a system image the template pins by its fingerprint.
fn pinned_machine() -> Value {
    json!({
        "name": "pinned", "kind": "virtual-machine", "image": "images:debian/13",
        "digest": FINGERPRINT,
        "limits": { "cpu": "2", "memory": "4GiB" }, "rootSize": "20GiB", "ports": [22]
    })
}

fn script(rig: &Rig, name: &str, text: &str) {
    fs::write(templates(rig).join(name), text).expect("the script");
}

/// A node with a plain container template, one with a script, and three virtual-machine templates.
fn node() -> Rig {
    let rig = Rig::with_kernel(kernel(), empty_state());
    rig.template("app", app_template("app"));
    rig.template("scripted", scripted());
    script(&rig, "scripted.sh", "exec /usr/local/bin/app --serve\n");
    rig.template("vm", vm_template());
    rig.template("machine", machine());
    script(&rig, "machine.sh", "#!/bin/sh\necho first boot\n");
    rig.template("pinned", pinned_machine());
    rig
}

fn propose(rig: &Rig, action: &str, args: Value) -> Result<Value, CallError> {
    rig.call("action_propose", json!({ "action": action, "args": args }))
}

/// Approves a waiting proposal with the owner's passkey and runs it, as the approval page does.
fn approve(rig: &Rig, proposal: &Value) -> Result<Value, CallError> {
    let id = proposal["id"].as_str().expect("the proposal's id");
    let action = proposal["action"].as_str().expect("its action");
    let digest = proposal["argsDigest"].as_str().expect("its digest");
    let nonce = format!("{:064x}", rig.nonce.fetch_add(1, Ordering::SeqCst) + 1);
    let statement =
        arca::handlers::action_statement("node1", id, action, digest, rig.now(), &nonce)
            .expect("the statement");
    let approval = common::approve_statement(&rig.key, &statement);
    let approved = arca::handlers::approve_action(
        &mut rig.kernel.lock().expect("kernel"),
        &json!({ "plan": id, "approval": approval }),
        rig.now(),
    )?;
    ActionExecutor::new(
        rig.handlers
            .iter()
            .map(|(verb, handler)| (*verb, Arc::clone(handler))),
    )
    .run(&approved.action, &approved.args, &approved.grant)
}

/// The fingerprint of a template's content, worked out here from the files alone: the tag, then each part framed by
/// the length of its name, its name, the length of its bytes and its bytes; the template's file first, then its
/// scripts by name.
fn fingerprint(rig: &Rig, name: &str, scripts: &[&str]) -> String {
    let mut hash = Sha256::new();
    hash.update(b"arca-template-content-v1\0");
    let mut part = |name: &str, bytes: &[u8]| {
        hash.update((name.len() as u64).to_be_bytes());
        hash.update(name.as_bytes());
        hash.update((bytes.len() as u64).to_be_bytes());
        hash.update(bytes);
    };
    let file = format!("{name}.json");
    part(
        &file,
        &fs::read(templates(rig).join(&file)).expect("the template"),
    );
    let mut scripts = scripts.to_vec();
    scripts.sort_unstable();
    for script in scripts {
        part(
            script,
            &fs::read(templates(rig).join(script)).expect("the script"),
        );
    }
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn rows(proposal: &Value) -> Vec<(String, String)> {
    proposal["details"]
        .as_array()
        .expect("details")
        .iter()
        .map(|row| {
            (
                row["label"].as_str().expect("a label").to_string(),
                row["value"].as_str().expect("words").to_string(),
            )
        })
        .collect()
}

fn row(proposal: &Value, label: &str) -> Option<String> {
    rows(proposal)
        .into_iter()
        .find(|(name, _)| name == label)
        .map(|(_, value)| value)
}

fn content_row(digest: &str) -> String {
    format!(
        "{digest}. Arca creates this only if the template on the node is still exactly this one."
    )
}

fn has_entry(rig: &Rig, id: &str) -> bool {
    rig.entries().iter().any(|entry| entry.id.0 == id)
}

const NOT_THE_ONE: &str = "on this node is not the one this create names: it was changed or installed again since the card was shown, or the call states an image or a template content of its own. Nothing was created; propose it again";

fn not_the_one(template: &str) -> String {
    format!("the template {template} {NOT_THE_ONE}")
}

#[test]
fn a_proposed_create_is_given_its_templates_image_and_content_and_the_card_shows_both() {
    let rig = node();
    for (template, scripts, image, shown) in [
        // A registry image by its digest.
        (
            "app",
            &[][..],
            format!("docker.io/library/app@{DIGEST}"),
            format!("docker.io/library/app@{DIGEST}"),
        ),
        (
            "scripted",
            &["scripted.sh"][..],
            format!("docker.io/library/app@{DIGEST}"),
            format!("docker.io/library/app@{DIGEST}"),
        ),
        // A machine on a system image with no digest: the row says so, in words.
        (
            "vm",
            &[][..],
            "images:debian/13".to_string(),
            format!("images:debian/13{NO_DIGEST}"),
        ),
        // A machine that names the version it runs: what boots first, with the same words, then what it runs.
        (
            "machine",
            &["machine.sh"][..],
            format!("images:debian/13/cloud running {WORKLOAD}"),
            format!("images:debian/13/cloud{NO_DIGEST}, running {WORKLOAD}"),
        ),
        // A system image the template pins by its fingerprint.
        (
            "pinned",
            &[][..],
            format!("images:debian/13@{FINGERPRINT}"),
            format!("images:debian/13@{FINGERPRINT}"),
        ),
    ] {
        let proposal = propose(
            &rig,
            "service_create",
            json!({ "name": "one", "template": template }),
        )
        .unwrap_or_else(|error| panic!("{template}: {error:?}"));
        let digest = fingerprint(&rig, template, scripts);
        assert_eq!(proposal["args"]["image"], image, "{template}");
        assert_eq!(proposal["args"]["templateDigest"], digest, "{template}");
        // The Image row is the one a person reads; the template's content is the last row, whole.
        let rows = rows(&proposal);
        let last = rows.len() - 1;
        assert_eq!(rows[last - 1], ("Image".to_string(), shown), "{template}");
        assert_eq!(
            rows[last],
            ("Template content".to_string(), content_row(&digest)),
            "{template}"
        );
        assert_eq!(
            rows.iter().filter(|(label, _)| label == "Image").count(),
            1,
            "{template}: one Image row"
        );
    }
    assert!(
        rig.entries()
            .iter()
            .all(|entry| entry.plugin.0 != "arca.service")
    );
    // A caller states neither: one that does is compared with the node's own, and refused when it differs.
    let digest = fingerprint(&rig, "app", &[]);
    for args in [
        json!({ "name": "two", "template": "app", "image": format!("docker.io/library/other@{DIGEST}") }),
        json!({ "name": "two", "template": "app", "templateDigest": "0".repeat(64) }),
        json!({ "name": "two", "template": "vm", "image": format!("images:debian/13@{FINGERPRINT}") }),
    ] {
        let (code, message) = refusal(propose(&rig, "service_create", args));
        assert_eq!(code, "refused");
        assert!(message.ends_with(NOT_THE_ONE), "{message}");
    }
    let same = propose(
        &rig,
        "service_create",
        json!({
            "name": "two", "template": "app",
            "image": format!("docker.io/library/app@{DIGEST}"), "templateDigest": digest
        }),
    )
    .expect("the node's own, stated");
    assert_eq!(same["args"]["templateDigest"], digest);
}

#[test]
fn a_pinned_local_image_is_on_the_card_by_its_fingerprint_and_is_what_the_instance_is_made_from() {
    let rig = node();
    // The service project is made with the node's first service, and holds the image after that.
    rig.call(
        "service_create",
        json!({ "name": "first", "template": "app" }),
    )
    .expect("the first service makes the project");
    let held = rig.fake().add_image(PROJECT, "built");
    rig.template("built", built(&held));
    let proposal = propose(
        &rig,
        "service_create",
        json!({ "name": "dash", "template": "built" }),
    )
    .expect("the proposal");
    // One field and one row for it, as for every kind of template: the alias and the fingerprint it is pinned to.
    let image = format!("local:built@sha256:{held}");
    assert_eq!(proposal["args"]["image"], image);
    assert_eq!(row(&proposal, "Image"), Some(image));
    assert_eq!(
        rows(&proposal)
            .iter()
            .filter(|(label, _)| label == "Image")
            .count(),
        1
    );
    assert!(proposal["args"].get("imagePin").is_none());
    // A pin edited after the card is another template: the approval creates nothing.
    let other = "0".repeat(64);
    rig.template("built", built(&other));
    let (_, message) = refusal(approve(&rig, &proposal));
    assert_eq!(message, not_the_one("built"));
    assert!(!has_entry(&rig, "service-dash"));
    // Proposed again from the pin that fits the image the project holds, it is created, and Incus is told the
    // fingerprint: the instance is made from that image whatever the alias points at.
    rig.template("built", built(&held));
    let again = propose(
        &rig,
        "service_create",
        json!({ "name": "dash", "template": "built" }),
    )
    .expect("proposed again");
    approve(&rig, &again).expect("the approved create, from the pinned image");
    assert_eq!(
        rig.entry("service-dash")["source"],
        json!({ "type": "image", "alias": "built", "fingerprint": held })
    );
    // A local image with no pin is shown with the words that say what its name leaves open.
    let mut unpinned = built(&held);
    unpinned
        .as_object_mut()
        .expect("a template")
        .remove("digest");
    rig.template("built", unpinned);
    let open = propose(
        &rig,
        "service_create",
        json!({ "name": "other", "template": "built" }),
    )
    .expect("the proposal");
    assert_eq!(open["args"]["image"], "local:built");
    assert_eq!(row(&open, "Image"), Some(format!("local:built{NO_DIGEST}")));
}

#[test]
fn a_template_changed_after_the_card_was_shown_is_not_created() {
    // Each change is one a person could make between the card and the approval: the template's file, a space in
    // it, the script its entrypoint names, a machine's first-boot script, the version a machine runs.
    type Change = fn(&Rig);
    let changes: [(&str, &str, Change); 6] = [
        ("app", "its environment", |rig| {
            let mut template = app_template("app");
            template["env"] = json!({ "DEBUG": "1" });
            rig.template("app", template);
        }),
        ("app", "one space", |rig| {
            let path = templates(rig).join("app.json");
            let mut text = fs::read_to_string(&path).expect("the template");
            text.push('\n');
            fs::write(&path, text).expect("the template");
        }),
        ("app", "another image", |rig| {
            let mut template = app_template("app");
            template["digest"] = json!(format!("sha256:{}", "7".repeat(64)));
            rig.template("app", template);
        }),
        ("scripted", "its entrypoint script", |rig| {
            script(
                rig,
                "scripted.sh",
                "exec /usr/local/bin/app --serve --open\n",
            );
        }),
        ("machine", "its first-boot script", |rig| {
            script(rig, "machine.sh", "#!/bin/sh\necho another first boot\n");
        }),
        ("machine", "the version it runs", |rig| {
            let mut template = machine();
            template["settings"]["serverImage"] = json!(WORKLOAD.replace("5555", "8888"));
            rig.template("machine", template);
        }),
    ];
    for (template, what, change) in changes {
        let rig = node();
        let proposal = propose(
            &rig,
            "service_create",
            json!({ "name": "one", "template": template }),
        )
        .expect("the proposal");
        change(&rig);
        let writes = rig.fake().writes().len();
        let (code, message) = refusal(approve(&rig, &proposal));
        assert_eq!(code, "refused", "{template}, {what}");
        assert_eq!(message, not_the_one(template), "{template}, {what}");
        assert!(!has_entry(&rig, "service-one"), "{template}, {what}");
        assert!(
            !rig.dir.join("secrets").join("service-one").exists(),
            "{template}, {what}: no secret was made"
        );
        assert_eq!(
            rig.fake().writes().len(),
            writes,
            "{template}, {what}: Incus was not written"
        );
        // Proposed again, the card shows the template as it is now, and that one is created.
        let again = propose(
            &rig,
            "service_create",
            json!({ "name": "one", "template": template }),
        )
        .expect("a new proposal");
        assert_ne!(
            again["args"]["templateDigest"], proposal["args"]["templateDigest"],
            "{template}, {what}"
        );
        approve(&rig, &again).unwrap_or_else(|error| panic!("{template}, {what}: {error:?}"));
        assert!(has_entry(&rig, "service-one"), "{template}, {what}");
    }
    // The control: nothing changed, and the approved create is made from what the card showed.
    let rig = node();
    for template in ["app", "scripted", "machine"] {
        let name = format!("of-{template}");
        let proposal = propose(
            &rig,
            "service_create",
            json!({ "name": name, "template": template }),
        )
        .expect("the proposal");
        approve(&rig, &proposal).unwrap_or_else(|error| panic!("{template}: {error:?}"));
        assert!(has_entry(&rig, &format!("service-{name}")), "{template}");
    }
    // The service is made from the script that was read with the template: the one the fingerprint names.
    assert!(
        rig.entry("service-of-scripted")["entrypoint"]
            .as_str()
            .is_some_and(|entrypoint| entrypoint.contains("exec /usr/local/bin/app --serve")),
    );
}

#[test]
fn a_card_that_waits_across_an_upgrade_of_the_shipped_templates_is_refused_and_proposed_again() {
    // A node holds an earlier shipped database template; a card is shown from it; an upgrade then installs the
    // template this build ships over it. The approval of the old card creates nothing, through either verb.
    let earlier = |rig: &Rig| {
        arca::cmd_templates::install_into(&templates(rig), false)
            .unwrap_or_else(|_| panic!("the shipped templates install"));
        let path = templates(rig).join(format!("{DATABASE_TEMPLATE}.json"));
        let mut template: Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("the template")).expect("json");
        template["description"] = json!("PostgreSQL, as an earlier release shipped it.");
        fs::write(&path, template.to_string()).expect("the earlier template");
    };
    let upgrade = |rig: &Rig| {
        arca::cmd_templates::install_into(&templates(rig), true)
            .unwrap_or_else(|_| panic!("the upgrade installs the shipped templates"));
    };
    for (action, args) in [
        (
            "service_create",
            json!({ "name": "orders", "template": DATABASE_TEMPLATE }),
        ),
        ("postgres_create", json!({ "name": "orders" })),
    ] {
        let rig = Rig::with_kernel(kernel(), empty_state());
        earlier(&rig);
        let proposal = propose(&rig, action, args.clone()).expect("the proposal");
        // The image is the same in both templates: only the content's fingerprint tells them apart.
        upgrade(&rig);
        let writes = rig.fake().writes().len();
        let (code, message) = refusal(approve(&rig, &proposal));
        assert_eq!(code, "refused", "{action}");
        assert_eq!(message, not_the_one(DATABASE_TEMPLATE), "{action}");
        assert!(!has_entry(&rig, "service-orders"), "{action}");
        assert!(
            !rig.dir.join("secrets").join("service-orders").exists(),
            "{action}: no password was made"
        );
        assert_eq!(rig.fake().writes().len(), writes, "{action}");
        let again = propose(&rig, action, args).expect("proposed again");
        assert_eq!(
            again["args"]["image"], proposal["args"]["image"],
            "{action}"
        );
        assert_ne!(
            again["args"]["templateDigest"], proposal["args"]["templateDigest"],
            "{action}"
        );
        approve(&rig, &again).unwrap_or_else(|error| panic!("{action}: {error:?}"));
        assert!(has_entry(&rig, "service-orders"), "{action}");
    }
}

#[test]
fn a_database_proposal_binds_the_content_of_its_template_too() {
    let rig = Rig::with_kernel(kernel(), empty_state());
    arca::cmd_templates::install_into(&templates(&rig), false)
        .unwrap_or_else(|_| panic!("the shipped templates install"));
    let proposal = propose(&rig, "postgres_create", json!({ "name": "orders" })).expect("it");
    let digest = fingerprint(&rig, DATABASE_TEMPLATE, &["postgres-autopg.sh"]);
    assert_eq!(proposal["args"]["templateDigest"], digest);
    let rows = rows(&proposal);
    assert_eq!(
        rows.last().expect("its last row"),
        &("Template content".to_string(), content_row(&digest))
    );
    assert_eq!(rows.iter().filter(|(label, _)| label == "Image").count(), 1);
    // The script that opens the database is part of what was shown: edited, the same image is another template.
    let path = templates(&rig).join("postgres-autopg.sh");
    let mut text = fs::read_to_string(&path).expect("the script");
    text.push_str("# an edit\n");
    fs::write(&path, text).expect("the edited script");
    let (code, message) = refusal(approve(&rig, &proposal));
    assert_eq!(code, "refused");
    assert_eq!(message, not_the_one(DATABASE_TEMPLATE));
    assert!(!has_entry(&rig, "service-orders"));
    // A caller states no content of its own, through either door.
    for result in [
        propose(
            &rig,
            "postgres_create",
            json!({ "name": "orders", "templateDigest": "0".repeat(64) }),
        ),
        rig.call(
            "postgres_create",
            json!({ "name": "orders", "templateDigest": "0".repeat(64) }),
        ),
    ] {
        let (code, message) = refusal(result);
        assert_eq!(code, "refused");
        assert_eq!(message, not_the_one(DATABASE_TEMPLATE));
    }
    assert!(!has_entry(&rig, "service-orders"));
}

#[test]
fn the_binding_holds_across_a_restart_of_the_node() {
    let rig = node();
    let kept = propose(
        &rig,
        "service_create",
        json!({ "name": "kept", "template": "scripted" }),
    )
    .expect("a proposal");
    let changed = propose(
        &rig,
        "service_create",
        json!({ "name": "changed", "template": "machine" }),
    )
    .expect("a proposal");
    let rig = rig.restart();
    let waiting = rig.call("plan_list", json!({})).expect("plans")["plans"]
        .as_array()
        .expect("plans")
        .clone();
    for shown in [&kept, &changed] {
        let now = waiting
            .iter()
            .find(|plan| plan["id"] == shown["id"])
            .expect("it still waits");
        assert_eq!(now["args"], shown["args"]);
        assert_eq!(now["details"], shown["details"]);
        assert_eq!(now["argsDigest"], shown["argsDigest"]);
    }
    script(
        &rig,
        "machine.sh",
        "#!/bin/sh\necho changed while it waited\n",
    );
    let (_, message) = refusal(approve(&rig, &changed));
    assert_eq!(message, not_the_one("machine"));
    approve(&rig, &kept).expect("the unchanged template is created");
    assert!(has_entry(&rig, "service-kept"));
    assert!(!has_entry(&rig, "service-changed"));
}

#[test]
fn a_card_that_bound_no_content_is_refused_before_the_approval_is_spent() {
    // Proposals recorded before this node bound a template's content, still inside their fifteen minutes: a create,
    // and a database made from the node's template. Neither card can be held to what it showed.
    let rig = node();
    arca::cmd_templates::install_into(&templates(&rig), false)
        .unwrap_or_else(|_| panic!("the shipped templates install"));
    let pin = arca::service::template_image(
        &arca::service::load_template(&templates(&rig), DATABASE_TEMPLATE)
            .unwrap_or_else(|_| panic!("the database template loads")),
    );
    for (action, args, template) in [
        (
            arca::action_proposal::ActionKind::ServiceCreate,
            json!({ "name": "old", "template": "app" }),
            "app",
        ),
        (
            arca::action_proposal::ActionKind::PostgresCreate,
            json!({ "name": "old", "template": DATABASE_TEMPLATE, "image": pin }),
            DATABASE_TEMPLATE,
        ),
    ] {
        let recorded = rig
            .kernel
            .lock()
            .expect("kernel")
            .propose_action(action, args, rig.now(), None)
            .unwrap_or_else(|_| panic!("the proposal is recorded"));
        let proposal = serde_json::to_value(&recorded).expect("json");
        assert!(proposal["args"].get("templateDigest").is_none());
        let writes = rig.fake().writes().len();
        let (code, message) = refusal(approve(&rig, &proposal));
        assert_eq!(code, "refused", "{template}");
        assert_eq!(
            message,
            format!(
                "this card was shown before this node bound a create to the content of its template ({template}), so it cannot be approved. Nothing was created and the approval was not used; propose it again"
            )
        );
        // The approval was not spent: the proposal still waits, unapproved, and no grant was armed for it.
        let waiting = rig.call("plan_list", json!({})).expect("plans")["plans"]
            .as_array()
            .expect("plans")
            .clone();
        assert!(
            waiting.iter().any(|plan| plan["id"] == proposal["id"]),
            "{template}: it still waits"
        );
        assert!(
            rig.kernel
                .lock()
                .expect("kernel")
                .action_grant_id("anything")
                .is_none()
        );
        assert!(!has_entry(&rig, "service-old"), "{template}");
        assert_eq!(rig.fake().writes().len(), writes, "{template}");
        rig.call("plan_decline", json!({ "plan": proposal["id"] }))
            .expect("the card is declined");
    }
    // A database of the old kind is made from no template: its card binds its image, and is approved as before.
    assert_eq!(
        arca::action_proposal::ActionKind::PostgresCreate
            .unbound_template(&json!({ "name": "old", "image": pin })),
        None
    );
}

#[test]
fn a_tokens_own_create_has_no_card_and_one_that_states_the_content_is_held_to_it() {
    let rig = node();
    // No card, nothing to bind: as before.
    rig.call(
        "service_create",
        json!({ "name": "plain", "template": "app" }),
    )
    .expect("a direct create");
    // A caller that read the template may state what it read, and is then held to it.
    let digest = fingerprint(&rig, "scripted", &["scripted.sh"]);
    let image = format!("docker.io/library/app@{DIGEST}");
    let entries = rig.entries();
    for args in [
        json!({ "name": "held", "template": "scripted", "templateDigest": "1".repeat(64) }),
        json!({ "name": "held", "template": "scripted", "image": format!("docker.io/library/other@{DIGEST}") }),
        json!({ "name": "held", "template": "scripted", "templateDigest": digest, "image": "docker.io/library/app" }),
    ] {
        let (code, message) = refusal(rig.call("service_create", args));
        assert_eq!(code, "refused");
        assert_eq!(message, not_the_one("scripted"));
    }
    assert_eq!(rig.entries(), entries);
    script(&rig, "scripted.sh", "exec /usr/local/bin/app --other\n");
    let (_, message) = refusal(rig.call(
        "service_create",
        json!({ "name": "held", "template": "scripted", "templateDigest": digest, "image": image }),
    ));
    assert_eq!(message, not_the_one("scripted"));
    script(&rig, "scripted.sh", "exec /usr/local/bin/app --serve\n");
    rig.call(
        "service_create",
        json!({ "name": "held", "template": "scripted", "templateDigest": digest, "image": image }),
    )
    .expect("the template is the one stated again");
    assert!(has_entry(&rig, "service-held"));
}

#[test]
fn a_create_that_could_not_be_made_from_its_template_is_refused_when_it_is_proposed() {
    let rig = node();
    // No such template, and a template whose script is not there: no card is shown for either.
    let (code, message) = refusal(propose(
        &rig,
        "service_create",
        json!({ "name": "one", "template": "absent" }),
    ));
    assert_eq!(code, "not_found");
    assert_eq!(message, "no template absent (templates/absent.json)");
    fs::remove_file(templates(&rig).join("scripted.sh")).expect("the script is removed");
    let (code, message) = refusal(propose(
        &rig,
        "service_create",
        json!({ "name": "one", "template": "scripted" }),
    ));
    assert_eq!(code, "refused");
    assert!(
        message.contains("scripted.sh"),
        "the refusal names the script: {message}"
    );
    assert_eq!(
        rig.call("plan_list", json!({})).expect("plans")["plans"],
        json!([])
    );
}
