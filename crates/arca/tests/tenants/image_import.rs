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
    let mut socket = UnixStream::connect(&rig.socket).unwrap();
    let body =
        json!({ "properties": properties, "public": false, "auto_update": false, "profiles": [] })
            .to_string();
    write!(socket, "PUT /1.0/images/{fingerprint}?project={PROJECT} HTTP/1.1\r\nHost: incus\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    let mut response = String::new();
    socket.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
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
