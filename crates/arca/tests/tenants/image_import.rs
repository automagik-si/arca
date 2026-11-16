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

#[test]
fn image_import_copies_the_exact_image_and_post_publish_properties_and_repeats_without_writes() {
    let (rig, fingerprint) = setup();
    let properties = json!({ "type": "app", "architecture": "x86_64", "custom": "after publish" });
    set_properties(&rig, &fingerprint, properties.clone());
    let source = rig
        .get(&format!("/1.0/images/{fingerprint}?project={PROJECT}"))
        .unwrap();
    let done = rig
        .approved(VERB, args(&fingerprint))
        .expect("approved import");
    assert_eq!(done["fingerprint"], fingerprint);
    let copied = target(&rig, &fingerprint).expect("the tenant's copy");
    assert_eq!(copied["properties"], properties);
    assert!(
        copied["aliases"]
            .as_array()
            .unwrap()
            .iter()
            .any(|alias| alias["name"] == "app")
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
    let rig = rig.restart();
    let before = rig.fake().writes().len();
    assert_eq!(
        rig.approved(VERB, args(&fingerprint)).unwrap()["imported"],
        false
    );
    assert_eq!(
        rig.fake().writes().len(),
        before,
        "repeat sends no Incus write"
    );
    let template = json!({ "name": "app", "kind": "system", "image": "local:app", "digest": format!("sha256:{fingerprint}") });
    rig.template("app", template);
    rig.call(
        "service_create",
        json!({ "name": "app", "template": "app", "tenant": A }),
    )
    .expect("pinned create from the imported image");
}

#[test]
fn image_import_missing_source_bad_fingerprint_and_taken_alias_write_nothing() {
    let (rig, fingerprint) = setup();
    proposal(&rig, args(&fingerprint)).expect("the valid source is available");
    for fingerprint in [
        "a".repeat(64),
        "a".repeat(12),
        "A".repeat(64),
        format!("sha256:{fingerprint}"),
    ] {
        let before = rig.fake().writes().len();
        assert!(proposal(&rig, args(&fingerprint)).is_err());
        assert_eq!(rig.fake().writes().len(), before);
    }
    let held = rig.fake().add_image(&project(A), "app");
    let before = rig.fake().writes().len();
    assert!(proposal(&rig, args(&fingerprint)).is_err());
    assert_eq!(rig.fake().writes().len(), before);
    assert!(target(&rig, &held).is_some());
    assert!(target(&rig, &fingerprint).is_none());
}

#[test]
fn image_import_rechecks_the_tenant_incarnation_even_at_the_same_timestamp() {
    let (rig, fingerprint) = setup();
    let proposed = proposal(&rig, args(&fingerprint)).unwrap();
    rig.approved("tenant_remove", json!({ "id": A })).unwrap();
    rig.create_tenant(json!({ "id": A })).unwrap();
    let before = rig.fake().writes().len();
    assert!(approve(&rig, &proposed).is_err());
    assert_eq!(rig.fake().writes().len(), before);
    assert!(target(&rig, &fingerprint).is_none());
}

#[test]
fn image_import_rechecks_source_properties_before_any_write() {
    let (rig, fingerprint) = setup();
    let proposed = proposal(&rig, args(&fingerprint)).unwrap();
    set_properties(&rig, &fingerprint, json!({ "custom": "changed" }));
    let before = rig.fake().writes().len();
    assert!(approve(&rig, &proposed).is_err());
    assert_eq!(rig.fake().writes().len(), before);
}

#[test]
fn image_import_copy_and_alias_failures_compensate_only_new_content() {
    for step in ["copy", "copy_after", "alias", "alias_after"] {
        let (rig, fingerprint) = setup();
        let source = rig.get(&format!("/1.0/images/{fingerprint}?project={PROJECT}"));
        rig.fake().fail_image_import_at(step);
        assert!(rig.approved(VERB, args(&fingerprint)).is_err(), "{step}");
        assert!(target(&rig, &fingerprint).is_none(), "{step}");
        assert_eq!(
            rig.get(&format!("/1.0/images/{fingerprint}?project={PROJECT}")),
            source
        );
        // A new approval can retry after a confirmed failure and compensation.
        rig.approved(VERB, args(&fingerprint)).expect("retry");
    }
}

#[test]
fn image_import_failure_keeps_a_preexisting_image_and_alias() {
    let (rig, fingerprint) = setup();
    let mut seed = args(&fingerprint);
    seed["alias"] = json!("seed");
    rig.approved(VERB, seed).unwrap();
    let before = target(&rig, &fingerprint).unwrap();
    rig.fake().fail_image_import_at("alias");
    assert!(rig.approved(VERB, args(&fingerprint)).is_err());
    assert_eq!(target(&rig, &fingerprint).unwrap(), before);
}

#[test]
fn image_import_rollback_pins_prevent_automatic_image_deletion() {
    let (rig, fingerprint) = setup();
    fs::write(
        rig.dir.join("service-updates.jsonl"),
        format!(
            "{}\n",
            json!({
                "step": "begin", "detail": { "tenant": A, "fromFingerprint": fingerprint }
            })
        ),
    )
    .unwrap();
    rig.fake().fail_image_import_at("alias");
    assert!(rig.approved(VERB, args(&fingerprint)).is_err());
    assert!(
        target(&rig, &fingerprint).is_some(),
        "a rollback's image is retained"
    );
    rig.approved(VERB, args(&fingerprint))
        .expect("retained image can be imported on a new card");
}

#[test]
fn image_import_a_tenant_with_a_live_service_retains_an_ambiguous_failed_copy() {
    let (rig, fingerprint) = setup();
    rig.template("app", app_template("app"));
    rig.call(
        "service_create",
        json!({ "name": "web", "template": "app", "tenant": A }),
    )
    .unwrap();
    rig.fake().fail_image_import_at("alias");
    assert!(rig.approved(VERB, args(&fingerprint)).is_err());
    assert!(target(&rig, &fingerprint).is_some());
}

#[test]
fn image_import_a_lost_completion_and_torn_tail_settle_by_readback_without_writes() {
    let (rig, fingerprint) = setup();
    rig.approved(VERB, args(&fingerprint)).unwrap();
    let path = rig.dir.join("tenant-image-imports.jsonl");
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let text = fs::read_to_string(&path).unwrap();
    let mut rows: Vec<&str> = text.lines().collect();
    assert_eq!(
        serde_json::from_str::<Value>(rows.pop().unwrap()).unwrap()["phase"],
        "Done"
    );
    fs::write(&path, format!("{}\n{{\"id\":", rows.join("\n"))).unwrap();
    let rig = rig.restart();
    let before = rig.fake().writes().len();
    assert_eq!(
        rig.approved(VERB, args(&fingerprint)).unwrap()["imported"],
        false
    );
    assert_eq!(rig.fake().writes().len(), before);
    let last: Value =
        serde_json::from_str(fs::read_to_string(&path).unwrap().lines().last().unwrap()).unwrap();
    assert_eq!(last["phase"], "Done");
}

#[test]
fn image_import_an_unknown_copy_outcome_is_not_deleted_until_its_operation_settles() {
    let (rig, fingerprint) = setup();
    rig.fake().fail_reads("/1.0/operations/", 2, 500);
    assert!(rig.approved(VERB, args(&fingerprint)).is_err());
    assert!(
        target(&rig, &fingerprint).is_some(),
        "an unreadable operation never triggers a delete"
    );
    rig.approved(VERB, args(&fingerprint))
        .expect("the settled operation can be recovered");
}

#[test]
fn image_import_a_nonprivate_record_refuses_before_any_incus_write() {
    let (rig, fingerprint) = setup();
    let path = rig.dir.join("tenant-image-imports.jsonl");
    fs::write(&path, "").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let before = rig.fake().writes().len();
    assert!(rig.approved(VERB, args(&fingerprint)).is_err());
    assert_eq!(rig.fake().writes().len(), before);
}

#[test]
fn image_import_recovers_an_unfinished_copy_before_its_properties_are_set() {
    let (rig, fingerprint) = setup();
    rig.fake().fail_reads("/1.0/operations/", 2, 500);
    assert!(rig.approved(VERB, args(&fingerprint)).is_err());
    // Model a crash before replacing extra database metadata on a copy reused from another project.
    put_properties(
        &rig,
        &project(A),
        &fingerprint,
        json!({ "extra": "other project" }),
    );
    let rig = rig.restart();
    rig.approved(VERB, args(&fingerprint))
        .expect("settle the recorded copy, then retry");
    assert_eq!(target(&rig, &fingerprint).unwrap()["properties"], json!({}));
}

#[test]
fn image_import_replaces_extra_database_properties_reused_from_another_project() {
    let (rig, fingerprint) = setup();
    let older = json!({ "extra": "only in the other project" });
    set_properties(&rig, &fingerprint, older.clone());
    let copy = json!({ "source": { "type": "image", "fingerprint": fingerprint }, "profiles": [] });
    image_request(
        &rig,
        "POST",
        &format!("/1.0/images?project={}", project(B)),
        copy.clone(),
    );
    let other = rig
        .get(&format!("/1.0/images/{fingerprint}?project={}", project(B)))
        .unwrap();
    assert_eq!(
        other["properties"], older,
        "native copy carries database properties without request overrides"
    );
    // Put the node's row after the other project's row, so native reuse selects that older metadata.
    image_request(
        &rig,
        "DELETE",
        &format!("/1.0/images/{fingerprint}?project={PROJECT}"),
        Value::Null,
    );
    image_request(
        &rig,
        "POST",
        &format!("/1.0/images?project={PROJECT}"),
        copy,
    );
    let approved = json!({ "type": "current" });
    set_properties(&rig, &fingerprint, approved.clone());
    rig.approved(VERB, args(&fingerprint)).unwrap();
    assert_eq!(target(&rig, &fingerprint).unwrap()["properties"], approved);
    let rows = fs::read_to_string(rig.dir.join("tenant-image-imports.jsonl")).unwrap();
    assert!(rows.contains("PropertiesIntent") && rows.contains("PropertiesSent"));
    assert_eq!(
        rig.get(&format!("/1.0/images/{fingerprint}?project={}", project(B)))
            .unwrap(),
        other
    );
}

#[test]
fn image_import_an_unrecorded_copy_response_retains_unproven_content() {
    let (rig, fingerprint) = setup();
    rig.fake().fail_reads("/1.0/operations/", 2, 500);
    assert!(rig.approved(VERB, args(&fingerprint)).is_err());
    let path = rig.dir.join("tenant-image-imports.jsonl");
    let text = fs::read_to_string(&path).unwrap();
    let intent = text.lines().next().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(intent).unwrap()["phase"],
        "CopyIntent"
    );
    fs::write(&path, format!("{intent}\n")).unwrap();
    let rig = rig.restart();
    let before = rig.fake().writes().len();
    assert!(rig.approved(VERB, args(&fingerprint)).is_err());
    assert_eq!(
        rig.fake().writes().len(),
        before,
        "an unrecorded response cannot prove ownership for a delete"
    );
    assert!(target(&rig, &fingerprint).is_some());
    rig.approved(VERB, args(&fingerprint))
        .expect("a new card can use the retained image");
}
