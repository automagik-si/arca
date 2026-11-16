//! Approved, project-scoped image copies. Every image and property is synthetic.
use super::*;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

const VERB: &str = "tenant_image_import";

fn setup() -> (Rig, String) {
    let rig = Rig::new(Some(policy()));
    for tenant in [A, B] {
        rig.create_tenant(json!({ "id": tenant })).unwrap();
    }
    let fingerprint = rig.fake().add_image(PROJECT, "source");
    (rig, fingerprint)
}

fn args(fingerprint: &str) -> Value {
    json!({ "tenant": A, "fingerprint": fingerprint, "alias": "app" })
}

fn proposal(rig: &Rig, args: Value) -> Result<Value, CallError> {
    rig.call("action_propose", json!({ "action": VERB, "args": args }))
}

fn approve(rig: &Rig, proposal: &Value) -> Result<Value, CallError> {
    let statement = arca::handlers::action_statement(
        "node1",
        proposal["id"].as_str().unwrap(),
        VERB,
        proposal["argsDigest"].as_str().unwrap(),
        rig.now(),
        &"f".repeat(64),
    )
    .unwrap();
    let signed = common::approve_statement(&rig.key, &statement);
    let approved = arca::handlers::approve_action(
        &mut rig.kernel.lock().unwrap(),
        &json!({ "plan": proposal["id"], "approval": signed }),
        rig.now(),
    )?;
    ActionExecutor::new(
        rig.handlers
            .iter()
            .map(|(verb, handler)| (*verb, Arc::clone(handler))),
    )
    .run(&approved.action, &approved.args, &approved.grant)
}

fn set_properties(rig: &Rig, fingerprint: &str, properties: Value) {
    put_properties(rig, PROJECT, fingerprint, properties);
}

fn put_properties(rig: &Rig, project: &str, fingerprint: &str, properties: Value) {
    image_request(
        rig,
        "PUT",
        &format!("/1.0/images/{fingerprint}?project={project}"),
        json!({ "properties": properties, "public": false, "auto_update": false, "profiles": [] }),
    );
}

fn image_request(rig: &Rig, method: &str, path: &str, body: Value) {
    let mut socket = UnixStream::connect(&rig.socket).unwrap();
    let body = body.to_string();
    write!(socket, "{method} {path} HTTP/1.1\r\nHost: incus\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    let mut response = String::new();
    socket.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 2"), "{response}");
}

fn target(rig: &Rig, fingerprint: &str) -> Option<Value> {
    rig.get(&format!("/1.0/images/{fingerprint}?project={}", project(A)))
}

#[test]
fn image_import_requires_a_one_shot_approved_card_and_rejects_file() {
    let (rig, fingerprint) = setup();
    let before = rig.fake().writes().len();
    for approval in [None, Some("forged")] {
        let mut call = args(&fingerprint);
        if let Some(approval) = approval {
            call["approval"] = json!(approval);
        }
        assert_eq!(refusal(rig.call(VERB, call)).0, "NotApproved");
    }
    let mut call = args(&fingerprint);
    call["file"] = json!("image.tar.xz");
    assert!(proposal(&rig, call).is_err());
    assert_eq!(rig.fake().writes().len(), before);
    let card = proposal(&rig, args(&fingerprint)).unwrap();
    let statement = arca::handlers::action_statement(
        "node1",
        card["id"].as_str().unwrap(),
        VERB,
        card["argsDigest"].as_str().unwrap(),
        rig.now(),
        &"f".repeat(64),
    )
    .unwrap();
    let signed = common::approve_statement(&rig.key, &statement);
    let approved = arca::handlers::approve_action(
        &mut rig.kernel.lock().unwrap(),
        &json!({ "plan": card["id"], "approval": signed }),
        rig.now(),
    )
    .unwrap();
    let mut call = approved.args.clone();
    call["approval"] = json!(approved.grant);
    call["alias"] = json!("changed-after-approval");
    assert_eq!(refusal(rig.call(VERB, call.clone())).0, "NotApproved");
    assert_eq!(rig.fake().writes().len(), before);
    call["alias"] = json!("app");
    rig.call(VERB, call.clone())
        .expect("the unchanged approved arguments");
    let before = rig.fake().writes().len();
    assert_eq!(refusal(rig.call(VERB, call)).0, "NotApproved");
    assert_eq!(
        rig.fake().writes().len(),
        before,
        "a spent grant cannot write again"
    );
}

fn outcome(result: Result<Value, CallError>, code: &str) -> Value {
    let error = result.expect_err("closed refusal");
    let data = error.refusal_data().expect("refusal data");
    assert_eq!(data["refusal"], code, "{error:?}");
    let schema = (arca_api::verb(VERB).unwrap().result)();
    arca_api::conforms(&schema["x-arca-refusal-data"], &data).expect("closed refusal schema");
    data
}

fn no_deletes(rig: &Rig, before: usize) {
    assert!(
        rig.fake().writes()[before..]
            .iter()
            .all(|write| write.method != "DELETE")
    );
}

fn native_copy(rig: &Rig, project: &str, fingerprint: &str) {
    image_request(
        rig,
        "POST",
        &format!("/1.0/images?project={project}"),
        json!({ "source": { "type": "image", "fingerprint": fingerprint }, "profiles": [] }),
    );
}

fn default_properties(rig: &Rig, fingerprint: &str) -> Value {
    native_copy(rig, "default", fingerprint);
    let older = json!({ "extra": "from default", "type": "older" });
    put_properties(rig, "default", fingerprint, older.clone());
    older
}

#[test]
fn image_import_copies_post_publish_properties_and_repeats_without_any_write_or_record() {
    let (rig, fingerprint) = setup();
    let properties = json!({ "type": "app", "architecture": "x86_64", "custom": "after publish" });
    set_properties(&rig, &fingerprint, properties.clone());
    let source = rig
        .get(&format!("/1.0/images/{fingerprint}?project={PROJECT}"))
        .unwrap();
    let card = proposal(&rig, args(&fingerprint)).unwrap();
    assert_eq!(card["args"]["destination"]["kind"], "absent");
    let done = approve(&rig, &card).unwrap();
    assert_eq!(done["kind"], "imported");
    assert_eq!(done["imageWrite"], true);
    assert_eq!(done["aliasWrite"], true);
    arca_api::conforms(&(arca_api::verb(VERB).unwrap().result)(), &done).unwrap();
    assert_eq!(
        target(&rig, &fingerprint).unwrap()["properties"],
        properties
    );
    assert!(
        rig.get(&format!("/1.0/images/{fingerprint}?project={}", project(B)))
            .is_none()
    );
    assert_eq!(
        rig.get(&format!("/1.0/images/{fingerprint}?project={PROJECT}"))
            .unwrap(),
        source
    );
    assert!(!rig.dir.join("tenant-image-imports.jsonl").exists());
    let rig = rig.restart();
    let before = rig.fake().writes().len();
    let done = rig.approved(VERB, args(&fingerprint)).unwrap();
    assert_eq!(done["kind"], "alreadyPresent");
    assert_eq!(done["imageWrite"], false);
    assert_eq!(done["aliasWrite"], false);
    assert_eq!(rig.fake().writes().len(), before);
    rig.template(
        "app",
        json!({ "name": "app", "kind": "system", "image": "local:app",
        "digest": format!("sha256:{fingerprint}") }),
    );
    rig.call(
        "service_create",
        json!({ "name": "app", "template": "app", "tenant": A }),
    )
    .unwrap();
}

#[test]
fn image_import_completes_an_equal_preexisting_image_with_only_an_alias_write() {
    let (rig, fingerprint) = setup();
    native_copy(&rig, &project(A), &fingerprint);
    let card = proposal(&rig, args(&fingerprint)).unwrap();
    assert_eq!(card["args"]["destination"]["kind"], "sameProperties");
    let before = rig.fake().writes().len();
    let done = approve(&rig, &card).unwrap();
    assert_eq!(done["kind"], "completed");
    assert_eq!(done["imageWrite"], false);
    assert_eq!(done["aliasWrite"], true);
    assert_eq!(rig.fake().writes().len(), before + 1);
    no_deletes(&rig, before);
}

#[test]
fn image_import_replaces_different_properties_only_as_shown_on_the_card() {
    let (rig, fingerprint) = setup();
    set_properties(&rig, &fingerprint, json!({ "type": "approved" }));
    native_copy(&rig, &project(A), &fingerprint);
    let old = json!({ "type": "previous", "custom": "kept until approved" });
    put_properties(&rig, &project(A), &fingerprint, old.clone());
    let before = rig.fake().writes().len();
    let card = proposal(&rig, args(&fingerprint)).unwrap();
    assert_eq!(card["args"]["destination"]["kind"], "differentProperties");
    assert_eq!(card["args"]["destination"]["properties"], old);
    let words = card["details"].to_string().to_lowercase();
    assert!(words.contains("different properties") && words.contains("approving replaces"));
    assert_eq!(rig.fake().writes().len(), before);
    assert_eq!(target(&rig, &fingerprint).unwrap()["properties"], old);
    let done = approve(&rig, &card).unwrap();
    assert_eq!(done["kind"], "completed");
    assert_eq!(done["imageWrite"], true);
    assert_eq!(
        target(&rig, &fingerprint).unwrap()["properties"],
        json!({ "type": "approved" })
    );
    no_deletes(&rig, before);
}

#[test]
fn image_import_destination_changes_after_the_card_refuse_before_any_write() {
    for change in ["appeared", "properties", "alias"] {
        let (rig, fingerprint) = setup();
        if change != "appeared" {
            native_copy(&rig, &project(A), &fingerprint);
        }
        let card = proposal(&rig, args(&fingerprint)).unwrap();
        match change {
            "appeared" => native_copy(&rig, &project(A), &fingerprint),
            "properties" => put_properties(
                &rig,
                &project(A),
                &fingerprint,
                json!({ "changed": "later" }),
            ),
            _ => image_request(
                &rig,
                "POST",
                &format!("/1.0/images/aliases?project={}", project(A)),
                json!({ "name": "app", "target": fingerprint }),
            ),
        }
        let before = rig.fake().writes().len();
        outcome(approve(&rig, &card), "StaleCard");
        assert_eq!(rig.fake().writes().len(), before, "{change}");
    }
}

#[test]
fn image_import_a_default_record_supplies_properties_in_project_order() {
    let (rig, fingerprint) = setup();
    let older = default_properties(&rig, &fingerprint);
    native_copy(&rig, &project(B), &fingerprint);
    assert_eq!(
        rig.get(&format!("/1.0/images/{fingerprint}?project={}", project(B)))
            .unwrap()["properties"],
        older
    );
}

#[test]
fn image_import_a_failed_default_property_correction_completes_from_a_new_card() {
    let (rig, fingerprint) = setup();
    set_properties(&rig, &fingerprint, json!({ "type": "approved" }));
    let default = default_properties(&rig, &fingerprint);
    rig.fake().fail_image_import_at("properties");
    let before = rig.fake().writes().len();
    let data = outcome(rig.approved(VERB, args(&fingerprint)), "PartialImport");
    assert_eq!(data["imageState"], "present");
    assert_eq!(data["aliasState"], "absent");
    assert_eq!(data["fingerprint"], fingerprint);
    assert_eq!(
        data["properties"],
        json!({ "type": "approved", "extra": "from default" })
    );
    assert_eq!(data["proposeAgain"], true);
    no_deletes(&rig, before);
    let rig = rig.restart();
    let card = proposal(&rig, args(&fingerprint)).unwrap();
    assert_eq!(card["args"]["destination"]["kind"], "differentProperties");
    let before = rig.fake().writes().len();
    let done = approve(&rig, &card).unwrap();
    assert_eq!(done["kind"], "completed");
    assert_eq!(done["imageWrite"], true);
    assert_eq!(done["aliasWrite"], true);
    assert_eq!(
        rig.fake().writes().len(),
        before + 2,
        "PUT plus alias POST, no copy"
    );
    assert_eq!(
        target(&rig, &fingerprint).unwrap()["properties"],
        json!({ "type": "approved" })
    );
    assert_eq!(
        rig.get(&format!("/1.0/images/{fingerprint}?project=default"))
            .unwrap()["properties"],
        default
    );
    assert!(!rig.dir.join("tenant-image-imports.jsonl").exists());
    no_deletes(&rig, before);
}

#[test]
fn image_import_failures_leave_content_and_a_new_card_completes_it() {
    for step in ["copy", "copy_after", "alias", "alias_after"] {
        let (rig, fingerprint) = setup();
        rig.fake().fail_image_import_at(step);
        let before = rig.fake().writes().len();
        let data = outcome(
            rig.approved(VERB, args(&fingerprint)),
            if step == "copy" {
                "ImportFailed"
            } else {
                "PartialImport"
            },
        );
        assert_eq!(
            data["imageState"],
            if step == "copy" { "absent" } else { "present" }
        );
        no_deletes(&rig, before);
        if step != "copy" {
            assert!(target(&rig, &fingerprint).is_some());
        }
        let rig = rig.restart();
        let before = rig.fake().writes().len();
        let done = rig.approved(VERB, args(&fingerprint)).unwrap();
        assert_eq!(
            done["kind"],
            match step {
                "copy" => "imported",
                "alias_after" => "alreadyPresent",
                _ => "completed",
            }
        );
        assert_eq!(
            rig.fake().writes().len() - before,
            match step {
                "copy" => 2,
                "alias_after" => 0,
                _ => 1,
            }
        );
        no_deletes(&rig, before);
    }
}

#[test]
fn image_import_an_alias_taken_during_copy_keeps_the_image_and_names_the_state() {
    let (rig, fingerprint) = setup();
    let other = rig.fake().add_image(&project(A), "other");
    rig.fake().fail_image_import_at("alias_taken");
    let before = rig.fake().writes().len();
    let data = outcome(rig.approved(VERB, args(&fingerprint)), "AliasTaken");
    assert_eq!(data["imageState"], "present");
    assert_eq!(data["aliasState"], "taken");
    assert_eq!(data["aliasTarget"], other);
    assert!(target(&rig, &fingerprint).is_some());
    no_deletes(&rig, before);
}

#[test]
fn image_import_missing_source_and_a_known_taken_alias_have_closed_refusals() {
    let (rig, fingerprint) = setup();
    let before = rig.fake().writes().len();
    outcome(proposal(&rig, args(&"a".repeat(64))), "SourceMissing");
    rig.fake().add_image(&project(A), "app");
    outcome(proposal(&rig, args(&fingerprint)), "AliasTaken");
    assert_eq!(rig.fake().writes().len(), before);
    for invalid in [
        "a".repeat(12),
        "A".repeat(64),
        format!("sha256:{fingerprint}"),
    ] {
        assert!(proposal(&rig, args(&invalid)).is_err());
    }
}

#[test]
fn image_import_changed_source_or_tenant_is_a_stale_card_without_a_write() {
    for change in ["source", "tenant"] {
        let (rig, fingerprint) = setup();
        let card = proposal(&rig, args(&fingerprint)).unwrap();
        if change == "source" {
            set_properties(&rig, &fingerprint, json!({ "changed": "later" }));
        } else {
            rig.approved("tenant_remove", json!({ "id": A })).unwrap();
            rig.create_tenant(json!({ "id": A })).unwrap();
        }
        let before = rig.fake().writes().len();
        outcome(approve(&rig, &card), "StaleCard");
        assert_eq!(rig.fake().writes().len(), before);
    }
}

#[test]
fn image_import_does_not_read_or_change_an_obsolete_import_record() {
    let (rig, fingerprint) = setup();
    let path = rig.dir.join("tenant-image-imports.jsonl");
    fs::write(&path, "not an import record\n").unwrap();
    let before = rig.fake().writes().len();
    rig.approved(VERB, args(&fingerprint)).unwrap();
    assert_eq!(fs::read_to_string(path).unwrap(), "not an import record\n");
    no_deletes(&rig, before);
}
