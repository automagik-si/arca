//! The verb table, the OpenRPC document and the released 0.1.0 schema. Each expected schema below is written out by hand from
//! the table, so a typo in the constructors fails here.

use arca_api::openrpc::{check_document, check_released_document, openrpc};
use arca_api::verbs::{KERNEL_FILLED, KERNEL_FILLS_IN, kernel_filled};
use arca_api::{API_VERSION, Scope, VerbClass, conforms, may_call, verb, verbs};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const CURRENT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../schema/arca.openrpc.json"
);
const RELEASED: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../schema/released/0.1.0.openrpc.json"
);
/// The SHA-256 of the released 0.1.0 file, byte for byte. A change to the file is a new release, not an edit.
const RELEASED_SHA256: &str = "2afb4621ced2dd06cd744b2c883671ba2e76dca52deee54162cf8f4779221ca7";

fn pretty(doc: &Value) -> String {
    let mut text = serde_json::to_string_pretty(doc).expect("the document serializes");
    text.push('\n');
    text
}

fn names() -> Vec<&'static str> {
    verbs().iter().map(|v| v.name).collect()
}

#[test]
fn the_table_holds_the_initial_plan_alerts_postgres_and_ontology_verbs_in_order() {
    assert_eq!(
        names(),
        [
            "arca_status",
            "node_metrics",
            "watch_report",
            "token_list",
            "journal_list",
            "plan_list",
            "action_propose",
            "plan_decline",
            "approval_submit",
            "action_approve",
            "peer_keys_sync",
            "alerts_list",
            "postgres_create",
            "postgres_list",
            "postgres_link",
            "postgres_unlink",
            "postgres_delete",
            "ontology_query",
            "service_create",
            "service_list",
            "service_status",
            "service_logs",
            "service_remove",
            "service_update",
            "service_import",
            "service_release",
            "service_connect",
            "service_disconnect",
            "service_secret_set",
            "service_publish",
            "service_unpublish",
            "external_add",
            "external_remove",
            "omni_instance_create",
            "omni_pair",
            "omni_status",
            "owner_approval_request",
            "owner_approval_status",
            "owner_approval_list",
            "enroll_confirm",
            "gpu_attach",
            "gpu_detach",
            "storage_list",
            "storage_add",
            "snapshot_create",
            "snapshot_list",
            "snapshot_delete",
            "instance_start",
            "instance_stop",
            "instance_restart",
            "tenant_create",
            "tenant_list",
            "tenant_status",
            "tenant_usage",
            "tenant_remove",
            "deploy_confirm",
            "deploy_rollback",
            "deploy_remove",
            "function_create",
            "function_remove",
            "function_secret_set",
            "function_list",
            "function_run",
            "function_runs",
            "registry_secret_set",
            "registry_list",
            "registry_secret_remove",
            "host_network_restore",
        ]
    );
    assert_eq!(API_VERSION, "0.1.0");
}

#[test]
fn each_verb_has_its_table_row_for_class_scope_and_writes() {
    let rows = [
        ("arca_status", VerbClass::L, Scope::Viewer, false, false),
        ("node_metrics", VerbClass::L, Scope::Viewer, false, false),
        ("watch_report", VerbClass::L, Scope::Viewer, false, false),
        ("token_list", VerbClass::L, Scope::Operator, false, false),
        ("plan_list", VerbClass::L, Scope::Viewer, false, false),
        ("action_propose", VerbClass::L, Scope::Viewer, false, false),
        ("plan_decline", VerbClass::L, Scope::Doctor, true, false),
        ("approval_submit", VerbClass::L, Scope::Doctor, true, true),
        ("action_approve", VerbClass::L, Scope::Doctor, true, true),
        ("peer_keys_sync", VerbClass::L, Scope::Doctor, true, false),
        ("alerts_list", VerbClass::L, Scope::Viewer, false, false),
        ("postgres_create", VerbClass::L, Scope::Doctor, true, false),
        ("postgres_list", VerbClass::L, Scope::Viewer, false, false),
        ("postgres_link", VerbClass::L, Scope::Doctor, true, false),
        ("postgres_unlink", VerbClass::L, Scope::Doctor, true, false),
        ("postgres_delete", VerbClass::L, Scope::Doctor, true, true),
        ("ontology_query", VerbClass::L, Scope::Viewer, false, false),
        ("service_create", VerbClass::L, Scope::Doctor, true, false),
        ("service_list", VerbClass::L, Scope::Viewer, false, false),
        ("service_status", VerbClass::L, Scope::Viewer, false, false),
        ("service_logs", VerbClass::L, Scope::Viewer, false, false),
        ("service_remove", VerbClass::L, Scope::Doctor, true, true),
        ("service_update", VerbClass::L, Scope::Doctor, true, true),
        ("service_import", VerbClass::L, Scope::Doctor, true, true),
        ("service_release", VerbClass::L, Scope::Doctor, true, true),
        ("gpu_attach", VerbClass::L, Scope::Doctor, true, false),
        ("gpu_detach", VerbClass::L, Scope::Doctor, true, true),
        ("storage_list", VerbClass::L, Scope::Viewer, false, false),
        ("storage_add", VerbClass::L, Scope::Doctor, true, true),
        ("snapshot_create", VerbClass::L, Scope::Doctor, true, false),
        ("snapshot_list", VerbClass::L, Scope::Viewer, false, false),
        ("snapshot_delete", VerbClass::L, Scope::Doctor, true, true),
        ("instance_start", VerbClass::L, Scope::Doctor, true, false),
        ("instance_stop", VerbClass::L, Scope::Doctor, true, true),
        ("instance_restart", VerbClass::L, Scope::Doctor, true, true),
        ("enroll_confirm", VerbClass::L, Scope::Doctor, true, false),
        ("tenant_create", VerbClass::L, Scope::Doctor, true, false),
        ("tenant_list", VerbClass::L, Scope::Viewer, false, false),
        ("tenant_status", VerbClass::L, Scope::Viewer, false, false),
        ("tenant_usage", VerbClass::L, Scope::Viewer, false, false),
        ("tenant_remove", VerbClass::L, Scope::Doctor, true, true),
        ("deploy_confirm", VerbClass::L, Scope::Doctor, true, false),
        ("deploy_rollback", VerbClass::L, Scope::Doctor, true, true),
        ("deploy_remove", VerbClass::L, Scope::Doctor, true, true),
        ("function_create", VerbClass::L, Scope::Doctor, true, false),
        ("function_remove", VerbClass::L, Scope::Doctor, true, true),
        (
            "function_secret_set",
            VerbClass::L,
            Scope::Doctor,
            true,
            false,
        ),
        ("function_list", VerbClass::L, Scope::Viewer, false, false),
        ("function_run", VerbClass::L, Scope::Doctor, true, false),
        ("function_runs", VerbClass::L, Scope::Viewer, false, false),
        (
            "registry_secret_set",
            VerbClass::L,
            Scope::Doctor,
            true,
            false,
        ),
        ("registry_list", VerbClass::L, Scope::Viewer, false, false),
        (
            "registry_secret_remove",
            VerbClass::L,
            Scope::Doctor,
            true,
            false,
        ),
        (
            "host_network_restore",
            VerbClass::L,
            Scope::Doctor,
            true,
            false,
        ),
    ];
    for (name, class, scope, writes, destructive) in rows {
        let v = verb(name).expect("the verb is in the table");
        assert_eq!(v.class, class, "{name} class");
        assert_eq!(v.min_scope, scope, "{name} minimum scope");
        assert_eq!(v.writes, writes, "{name} writes");
        assert_eq!(v.destructive, destructive, "{name} destructive");
    }
}

#[test]
fn ontology_query_schemas_equal_its_table_row() {
    let v = verb("ontology_query").expect("present");
    assert_eq!(
        (v.params)(),
        closed_with(json!({"id": {"type": "string", "maxLength": 128}}), &["id"])
    );
    let nullable_enum =
        |values: &[&str]| json!({"anyOf": [{"type": "string", "enum": values}, {"type": "null"}]});
    let edge = open(
        json!({
            "id": {"type": "string"},
            "to": {"type": "string", "enum": ["arca", "droidd", "dsh", "omni", "arca-id", "automagik", "weunfold", "herdr"]},
            "contract": {"type": "string", "enum": ["mcp", "api", "file", "token", "socket"]},
            "status": {"type": "string", "enum": ["live", "planned", "deferred"]}
        }),
        &["id", "to", "contract", "status"],
    );
    assert_eq!(
        (v.result)(),
        open(
            json!({
                "id": {"type": "string", "maxLength": 128},
                "kind": nullable_enum(&["tool", "incus-project", "incus-mark", "data", "unit", "token-file", "passkey-name"]),
                "owner": nullable_enum(&["arca"]),
                "status": nullable_enum(&["live", "planned", "deferred"]),
                "edges": {"type": "array", "items": edge},
                "neverOwns": {"type": "boolean"}
            }),
            &["id", "kind", "owner", "status", "edges", "neverOwns"]
        )
    );
}

#[test]
fn ontology_query_nullable_enums_accept_null_and_reject_unknown_values() {
    let schema = (verb("ontology_query").expect("present").result)();
    let unknown = json!({"id": "unknown", "kind": null, "owner": null, "status": null, "edges": [], "neverOwns": false});
    assert_eq!(conforms(&schema, &unknown), Ok(()));
    for key in ["kind", "owner", "status"] {
        let mut invalid = unknown.clone();
        invalid[key] = json!("invalid");
        assert!(conforms(&schema, &invalid).is_err(), "{key}");
    }
    for key in ["id", "kind", "owner", "status", "edges", "neverOwns"] {
        let mut missing = unknown.clone();
        missing.as_object_mut().expect("object").remove(key);
        assert!(conforms(&schema, &missing).is_err(), "{key} required");
    }
}

#[test]
fn plan_verb_schemas_equal_their_table_rows() {
    let kinds = [
        "project", "instance", "network", "acl", "profile", "volume", "zone", "forward", "bucket",
    ];
    let obj = open(
        json!({
            "kind": { "type": "string", "enum": kinds },
            "project": { "type": "string" },
            "name": { "type": "string" },
            "uuid": { "type": ["string", "null"] },
        }),
        &["kind", "project", "name", "uuid"],
    );
    let plan = open(
        json!({
            "plan": hex64(),
            "entry": { "type": "string" },
            "createdAt": { "type": "integer" },
            "expiresAt": { "type": "integer" },
            "objects": { "type": "array", "items": obj.clone() },
            "paramsDigest": hex64(),
            "statement": { "type": "string" },
        }),
        &[
            "plan",
            "entry",
            "createdAt",
            "expiresAt",
            "objects",
            "paramsDigest",
            "statement",
        ],
    );

    let action_names = [
        "service_create",
        "service_remove",
        "service_update",
        "service_import",
        "service_release",
        "service_publish",
        "service_unpublish",
        "service_connect",
        "service_disconnect",
        "postgres_create",
        "gpu_attach",
        "gpu_detach",
        "storage_add",
        "snapshot_create",
        "snapshot_delete",
        "instance_start",
        "instance_stop",
        "instance_restart",
        "tenant_create",
        "tenant_remove",
        "deploy_remove",
        "function_create",
        "function_remove",
        "host_network_restore",
    ];
    let action_proposal = open(
        json!({
            "kind": { "const": "action", "type": "string" },
            "id": {
                "type": "string",
                "minLength": 16,
                "maxLength": 16,
                "pattern": "^act-[0-9a-f]{12}$",
            },
            "action": { "type": "string", "enum": action_names },
            "summary": { "type": "string" },
            "details": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "label": { "type": "string" },
                        "value": { "type": "string" },
                    },
                    "required": ["label", "value"],
                },
            },
            "args": { "type": "object" },
            "argsDigest": hex64(),
            "createdAt": { "type": "integer" },
            "expiresAt": { "type": "integer" },
            "statement": { "type": "string" },
            "host": { "type": "string" },
            "approvedIn": { "type": "string" },
            "approvedBy": { "type": "string" },
            "approval": { "type": "object" },
        }),
        &[
            "kind",
            "id",
            "action",
            "summary",
            "details",
            "args",
            "argsDigest",
            "createdAt",
            "expiresAt",
        ],
    );
    let plan_ref = json!({
        "type": "string",
        "minLength": 16,
        "maxLength": 68,
        "pattern": "^(?:[0-9a-f]{64}|act-[0-9a-f]{12})$",
    });

    let list = verb("plan_list").expect("present");
    assert_eq!((list.params)(), closed(json!({})));
    assert_eq!(
        (list.result)(),
        open(
            json!({ "plans": { "type": "array", "items": { "anyOf": [plan, action_proposal.clone()] } } }),
            &["plans"]
        )
    );

    let propose = verb("action_propose").expect("present");
    assert_eq!(
        (propose.params)(),
        closed_with(
            json!({
                "action": { "type": "string", "enum": action_names },
                // One branch per action, each pinned by the issue 113 test below.
                "args": (propose.params)()["properties"]["args"].clone(),
            }),
            &["action", "args"]
        )
    );
    // The proposal, and what the owner does next: the page, how many passkeys are trusted, and the step in words.
    let mut answer = (propose.result)();
    for (field, kind) in [
        ("approvalUrl", "string"),
        ("passkeysTrusted", "integer"),
        ("nextStep", "string"),
    ] {
        let property = answer["properties"]
            .as_object_mut()
            .and_then(|properties| properties.remove(field))
            .unwrap_or_else(|| panic!("action_propose answers {field}"));
        assert_eq!(property["type"], json!(kind), "{field}");
        assert!(property["description"].is_string(), "{field} is described");
    }
    let mut proposal_answer = action_proposal.clone();
    if let Some(required) = proposal_answer["required"].as_array_mut() {
        required.extend([json!("passkeysTrusted"), json!("nextStep")]);
    }
    assert_eq!(answer, proposal_answer);

    let decline = verb("plan_decline").expect("present");
    assert_eq!(
        (decline.params)(),
        closed_with(json!({ "plan": plan_ref.clone() }), &["plan"])
    );
    assert_eq!(
        (decline.result)(),
        open(
            json!({
                "plan": plan_ref.clone(),
                "retained": { "type": "array", "items": obj.clone() },
            }),
            &["plan", "retained"]
        )
    );

    let submit = verb("approval_submit").expect("present");
    assert_eq!(
        (submit.params)(),
        closed_with(
            json!({
                "plan": hex64(),
                "approval": { "type": "string", "maxLength": 16384 },
                "confirm": confirm(),
            }),
            &["plan", "approval"]
        )
    );
    assert_eq!(
        (submit.result)(),
        open(
            json!({
                "plan": hex64(),
                "deleted": { "type": "array", "items": obj.clone() },
                "drift": { "type": "array", "items": obj },
                "partial": { "type": "boolean" },
            }),
            &["plan", "deleted", "drift", "partial"]
        )
    );
}

#[test]
fn action_approve_takes_an_action_id_and_an_approval_and_names_what_ran() {
    let v = verb("action_approve").expect("present");
    let id = json!({ "type": "string", "minLength": 16, "maxLength": 16, "pattern": "^act-[0-9a-f]{12}$" });
    assert_eq!(
        (v.params)(),
        closed_with(
            json!({
                "plan": id.clone(),
                "approval": { "type": "string", "maxLength": 16384 },
                "confirm": confirm(),
            }),
            &["plan", "approval"]
        )
    );
    let result = (v.result)();
    assert_eq!(
        result["required"],
        json!(["plan", "id", "action", "result"])
    );
    assert_eq!(result["properties"]["plan"], id);
    assert_eq!(result["properties"]["result"], json!({ "type": "object" }));
    assert!(arca_api::conforms(
        &result,
        &json!({ "plan": "act-0123456789ab", "id": "act-0123456789ab", "action": "service_create", "result": {} })
    )
    .is_ok());
}

#[test]
fn peer_keys_sync_takes_lines_and_an_optional_signed_statement_and_never_echoes_a_line() {
    let v = verb("peer_keys_sync").expect("present");
    assert_eq!(
        (v.params)(),
        closed_with(
            json!({
                "from": { "type": "string", "maxLength": 253, "pattern": "^[A-Za-z0-9][A-Za-z0-9.-]{0,252}$" },
                "lines": { "type": "array", "items": { "type": "string", "maxLength": 2048 }, "maxItems": 64 },
                "statement": { "type": "string", "maxLength": 4096 },
                "approval": { "type": "string", "maxLength": 8192 },
            }),
            &["lines"]
        )
    );
    let result = (v.result)();
    assert_eq!(result["required"], json!(["status", "keys"]));
    assert!(result["properties"].get("lines").is_none());
    assert!(
        arca_api::conforms(
            &result,
            &json!({ "status": "needs_approval", "keys": 2, "statement": "{}" })
        )
        .is_ok()
    );
}

#[test]
fn arca_status_schemas_equal_its_table_row() {
    let v = verb("arca_status").expect("present");
    assert_eq!((v.params)(), closed(json!({})));
    assert_eq!(
        (v.result)(),
        open(
            json!({
                "version": { "type": "string" },
                "node": hex64(),
                "mode": { "type": "string", "enum": ["watch-only", "kernel"] },
                "uptimeS": { "type": "integer", "minimum": 0 },
                "incusReachable": { "type": "boolean" },
                "appliedSeq": { "type": ["integer", "null"] },
                "metricsSource": { "type": "string", "enum": ["local", "remote"] },
                "metricsSourceReason": { "type": "string" },
                "approval": {
                    "type": "object",
                    "properties": {
                        "linkedHosts": { "type": "array", "items": { "type": "string" } },
                        "trustedKeyFiles": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "index": { "type": "integer", "minimum": 0 },
                                    "name": { "type": "string" },
                                    "keys": { "type": "integer", "minimum": 0 },
                                    "reason": { "type": ["string", "null"] },
                                },
                                "required": ["index", "name", "keys", "reason"],
                            },
                        },
                        "peerKeys": {
                            "type": "object",
                            "properties": {
                                "from": { "type": "string" },
                                "keys": { "type": "integer", "minimum": 0 },
                                "since": { "type": ["string", "null"] },
                                "reason": { "type": "string" },
                            },
                            "required": ["from", "keys", "since", "reason"],
                        },
                        "peerKeySources": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "from": { "type": "string" },
                                    "keys": { "type": "integer", "minimum": 0 },
                                    "since": { "type": ["string", "null"] },
                                    "reason": { "type": "string" },
                                },
                                "required": ["from", "keys", "since", "reason"],
                            },
                        },
                    },
                    "required": ["linkedHosts", "trustedKeyFiles"],
                },
                "loopback": {
                    "type": "object",
                    "properties": {
                        "addr": { "type": "string" },
                        "open": { "type": "boolean" },
                        "reason": { "type": ["string", "null"] },
                        "page": {
                            "type": "object",
                            "properties": {
                                "open": { "type": "boolean" },
                                "reason": { "type": ["string", "null"] },
                            },
                            "required": ["open", "reason"],
                        },
                    },
                    "required": ["addr", "open", "reason"],
                },
                "approvalPage": {
                    "type": "object",
                    "properties": {
                        "url": { "type": "string" },
                        "open": { "type": "boolean" },
                        "reason": { "type": ["string", "null"] },
                        "passkeys": { "type": ["integer", "null"], "minimum": 0 },
                    },
                    "required": ["url", "open", "reason", "passkeys"],
                }
            }),
            &[
                "version",
                "node",
                "mode",
                "uptimeS",
                "incusReachable",
                "appliedSeq",
                "metricsSource",
                "metricsSourceReason"
            ]
        )
    );
}

#[test]
fn alerts_list_takes_no_argument_and_requires_every_key_of_its_result_and_events() {
    let v = verb("alerts_list").expect("present");
    assert_eq!((v.params)(), closed(json!({})));
    let result = (v.result)();
    let event = &result["properties"]["resolvedRecent"]["items"];
    let firing = &result["properties"]["firing"]["items"];
    let fields = [
        "schema",
        "seq",
        "alertId",
        "node",
        "rule",
        "severity",
        "state",
        "labels",
        "value",
        "threshold",
        "startedAt",
        "resolvedAt",
        "at",
    ];
    assert_eq!(event["required"], json!(fields), "reason is optional");
    assert_eq!(
        event["properties"]["reason"]["enum"],
        json!(["cleared", "rule-disabled", "subject-gone", "no data"])
    );
    assert_eq!(
        event["properties"]["labels"],
        json!({ "type": "object", "properties": {} })
    );
    assert_eq!(
        firing["required"].as_array().map(Vec::len),
        Some(fields.len() + 1)
    );
    assert_eq!(
        firing["properties"]["unknown"],
        json!({ "type": "boolean" })
    );
    assert_eq!(
        result["required"],
        json!([
            "firing",
            "unknown",
            "disabled",
            "resolvedRecent",
            "sampleAgeMs",
            "watchAgeMs",
            "commitMs",
            "storeError",
            "storeReset",
            "storeBytes",
            "healthLine"
        ])
    );
    for key in ["sampleAgeMs", "watchAgeMs", "commitMs"] {
        assert_eq!(
            result["properties"][key],
            json!({ "type": ["integer", "null"] }),
            "{key}"
        );
    }
    assert_eq!(
        result["properties"]["storeError"]["type"],
        json!(["object", "null"])
    );
}

#[test]
fn node_metrics_params_and_result_equal_its_table_row() {
    let v = verb("node_metrics").expect("present");
    assert_eq!(
        (v.params)(),
        closed_with(
            json!({ "minutes": { "type": "integer", "minimum": 1, "maximum": 60 } }),
            &[]
        )
    );
    let got = (v.result)();
    assert_eq!(
        got,
        open(
            json!({
                "at": { "type": "integer" },
                "minutes": { "type": "integer" },
                "latest": nullable_object(host_sample()),
                "mean": nullable_object(host_sample())
            }),
            &["at", "minutes", "latest", "mean"]
        )
    );
}

#[test]
fn node_metrics_result_has_no_renter_gate_at_any_depth() {
    let v = verb("node_metrics").expect("present");
    assert!(!mentions_key(&(v.result)(), "renterGate"));
}

#[test]
fn watch_report_schemas_equal_its_table_row() {
    let v = verb("watch_report").expect("present");
    assert_eq!((v.params)(), closed(json!({})));
    let instance = json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" },
            "kind": { "type": "string", "enum": ["container", "virtual-machine"] },
            "status": { "type": "string" },
            "owner": {
                "type": "object",
                "properties": {
                    "class": {
                        "type": "string",
                        "enum": ["droidd", "foreign", "kernel", "retained", "contested"]
                    },
                    "entry": { "type": "string" },
                    "plugin": { "type": "string" },
                    "plan": hex64()
                },
                "required": ["class"]
            },
            "privileged": { "type": "boolean" },
            "identity": { "type": "boolean" },
            "configHash": hex64()
        },
        "required": ["name", "kind", "status", "owner", "privileged", "identity", "configHash"]
    });
    let project = json!({
        "type": "object",
        "properties": {
            "project": { "type": "string" },
            "instances": { "type": "array", "items": instance }
        },
        "required": ["project", "instances"]
    });
    let drift = json!({
        "type": "object",
        "properties": {
            "kind": { "type": "string", "enum": ["markedWithoutEntry", "entryWithoutObject"] },
            "entry": { "type": "string" },
            "project": { "type": "string" },
            "name": { "type": "string" }
        },
        "required": ["kind", "entry"]
    });
    assert_eq!(
        (v.result)(),
        open(
            json!({
                "at": { "type": "integer" },
                "ageMs": { "type": "integer" },
                "projects": { "type": "array", "items": project },
                "drift": { "type": "array", "items": drift }
            }),
            &["at", "projects", "drift"]
        )
    );
}

#[test]
fn token_list_schemas_equal_its_table_row() {
    let v = verb("token_list").expect("present");
    assert_eq!((v.params)(), closed(json!({})));
    assert_eq!(
        (v.result)(),
        open(
            json!({
                "tokens": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "string" },
                            "scope": { "type": "string", "enum": ["viewer", "doctor", "operator"] },
                            "tools": { "type": "array", "items": { "type": "string" } },
                            "admin": { "type": "boolean" }
                        },
                        "required": ["id", "scope"]
                    }
                }
            }),
            &["tokens"]
        )
    );
}

#[test]
fn a_viewer_is_refused_every_write_verb_and_token_list() {
    for v in verbs() {
        if v.writes || v.name == "token_list" {
            assert!(
                !may_call(Scope::Viewer, v),
                "a viewer may not call {}",
                v.name
            );
        }
    }
}

#[test]
fn a_doctor_is_refused_token_list_and_an_operator_may_call_all() {
    assert!(!may_call(
        Scope::Doctor,
        verb("token_list").expect("present")
    ));
    for v in verbs() {
        assert!(
            may_call(Scope::Operator, v),
            "an operator may call {}",
            v.name
        );
        if v.min_scope <= Scope::Doctor {
            assert!(may_call(Scope::Doctor, v), "a doctor may call {}", v.name);
        }
    }
}

#[test]
fn every_writes_verb_needs_doctor_or_more() {
    for v in verbs() {
        if v.writes {
            assert!(
                v.min_scope >= Scope::Doctor,
                "{} writes at viewer scope",
                v.name
            );
        }
    }
}

#[test]
fn an_unknown_verb_is_not_in_the_table() {
    assert!(verb("arca_nope").is_none());
    assert!(verb("").is_none());
}

#[test]
fn the_document_passes_its_check_and_every_method_is_classed() {
    let doc = openrpc();
    assert_eq!(check_document(&doc), Ok(()));
    let methods = doc["methods"].as_array().expect("methods");
    assert_eq!(methods.len(), verbs().len());
    for m in methods {
        assert!(m["x-arca-class"].is_string(), "every method has a class");
        assert!(m["x-arca-scope"].is_string(), "every method has a scope");
        assert!(
            m["x-arca-writes"].is_boolean(),
            "every method has a writes flag"
        );
        let v = verbs()
            .iter()
            .find(|v| m["name"] == v.name)
            .expect("every method is a verb");
        assert_eq!(
            m["x-arca-writes"],
            json!(v.writes),
            "{}: writes is the table's",
            v.name
        );
        assert_eq!(
            m["x-arca-read-only-hint"],
            json!(v.read_only_hint()),
            "{}: the hint is the served one",
            v.name
        );
        assert_eq!(
            m["x-arca-additional-properties"],
            json!(true),
            "every params object is closed"
        );
    }
}

#[test]
fn a_document_with_an_unclassed_method_fails_the_check() {
    let mut doc = openrpc();
    doc["methods"][1]
        .as_object_mut()
        .expect("a method is an object")
        .remove("x-arca-class");
    assert!(
        check_document(&doc).is_err(),
        "a verb without a class is refused"
    );
}

#[test]
fn a_document_with_an_illegal_class_fails_the_check() {
    let mut doc = openrpc();
    doc["methods"][0]["x-arca-class"] = json!("X");
    assert!(check_document(&doc).is_err());
}

#[test]
fn a_method_whose_params_object_is_open_fails_the_check() {
    // The flag records that the params object is closed, so `true` is the only legal value. A bool of `false` is
    // a bool too, so a bare type check would let an open params object through.
    for open in [json!(false), json!("true"), json!(1)] {
        let mut doc = openrpc();
        doc["methods"][0]["x-arca-additional-properties"] = open.clone();
        assert!(
            check_document(&doc).is_err(),
            "x-arca-additional-properties {open} is refused"
        );
    }
}

#[test]
fn the_document_is_the_generated_schema_file() {
    let text = pretty(&openrpc());
    if std::env::var_os("ARCA_UPDATE_SCHEMA").is_some() {
        std::fs::write(CURRENT, &text).expect("the current schema file is writable");
    }
    let on_disk = std::fs::read_to_string(CURRENT).expect("the schema file exists");
    assert_eq!(
        on_disk, text,
        "schema/arca.openrpc.json is stale: run ARCA_UPDATE_SCHEMA=1 cargo test -p arca-api"
    );
}

#[test]
fn the_released_0_1_0_document_holds_exactly_the_four_initial_verbs() {
    let bytes = std::fs::read(RELEASED).expect("the released schema exists");
    let hex: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(
        hex, RELEASED_SHA256,
        "the released 0.1.0 file is byte-pinned"
    );
    let doc: Value = serde_json::from_slice(&bytes).expect("the released schema is JSON");
    assert_eq!(check_released_document(&doc), Ok(()));
    assert_eq!(doc["info"]["version"], json!("0.1.0"));
    let released: Vec<&str> = doc["methods"]
        .as_array()
        .expect("methods")
        .iter()
        .filter_map(|m| m["name"].as_str())
        .collect();
    assert_eq!(released, names()[..4]);
}

#[test]
fn an_arguments_object_is_checked_against_the_params_schema() {
    let params = (verb("node_metrics").expect("present").params)();
    assert!(conforms(&params, &json!({})).is_ok(), "minutes is optional");
    assert!(conforms(&params, &json!({ "minutes": 60 })).is_ok());
    assert!(conforms(&params, &json!({ "minutes": 61 })).is_err());
    assert!(conforms(&params, &json!({ "minutes": 0 })).is_err());
    assert!(conforms(&params, &json!({ "minutes": 5, "extra": true })).is_err());
}

#[test]
fn a_result_object_is_open_to_a_field_it_does_not_name() {
    let result = (verb("arca_status").expect("present").result)();
    let value = json!({
        "version": "0.1.0",
        "node": "a".repeat(64),
        "mode": "kernel",
        "uptimeS": 12,
        "incusReachable": false,
        "appliedSeq": null,
        "metricsSource": "local",
        "metricsSourceReason": "the test names the local mode",
        "addedLater": 1
    });
    assert!(conforms(&result, &value).is_ok(), "a result stays open");
}

#[test]
fn a_host_sample_with_nulls_conforms_to_the_result_schema() {
    let result = (verb("node_metrics").expect("present").result)();
    let sample = json!({
        "at": 1,
        "cpu": { "usedPct": null, "cores": 8, "coresKnown": true, "reason": null },
        "load": { "one": 0.5, "five": null, "fifteen": 0.1, "reason": null },
        "memory": { "totalBytes": 1, "availableBytes": null, "usedBytes": null, "swapTotalBytes": 0, "swapFreeBytes": null, "reason": null },
        "psi": { "memorySomeAvg10": 0.0, "memoryFullAvg10": null, "cpuSomeAvg10": 1.5 },
        "cpuTempC": null,
        "gpuBusyPct": null,
        "gpu": { "reason": null, "skipped": null, "cards": [] },
        "pools": [{ "name": "p", "usedBytes": null, "totalBytes": 9, "freeBytes": null }],
        "network": null,
        "projects": [{ "project": "p", "total": 2, "running": 1 }],
        "instancesNotRead": [{ "project": "p", "name": "vm1", "why": "Incus did not answer within 2 seconds" }]
    });
    let value = json!({ "at": 1, "minutes": 1, "latest": sample, "mean": null });
    assert!(conforms(&result, &value).is_ok());
}

#[test]
fn a_host_sample_with_a_missing_key_fails_the_result_schema() {
    let result = (verb("node_metrics").expect("present").result)();
    let value = json!({ "at": 1, "minutes": 1, "latest": null });
    assert!(conforms(&result, &value).is_err(), "mean is required");
}

#[test]
fn class_and_scope_spell_the_document_values() {
    assert_eq!(VerbClass::L.as_str(), "L");
    assert_eq!(Scope::Doctor.as_str(), "doctor");
    assert_eq!(
        serde_json::to_value(Scope::Operator).expect("serializes"),
        json!("operator")
    );
    assert_eq!(
        serde_json::to_value(VerbClass::Q).expect("serializes"),
        json!("Q")
    );
}

// Expected-schema helpers. They build the same shape the table row names: a closed params object or an open result object.

fn closed(props: Value) -> Value {
    closed_with(props, &[])
}

fn closed_with(props: Value, required: &[&str]) -> Value {
    let mut obj = json!({ "type": "object", "properties": props, "additionalProperties": false });
    if !required.is_empty() {
        obj["required"] = json!(required);
    }
    obj
}

fn open(props: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": props, "required": required })
}

/// The optional `confirm` every destructive verb's params carry.
fn confirm() -> Value {
    json!({
        "description": "Optional. Absent or true: the call runs as without it. False: the call is refused, nothing changes and no approval is spent. It is never part of what an approval binds.",
        "type": "boolean"
    })
}

fn hex64() -> Value {
    json!({ "type": "string", "minLength": 64, "maxLength": 64 })
}

/// `T|null` for an object: the object schema with `null` added to its types.
fn nullable_object(mut schema: Value) -> Value {
    schema["type"] = json!(["object", "null"]);
    schema
}

fn gpu_section() -> Value {
    let telemetry = {
        let mut schema = open(
            json!({
                "utilizationPct": { "type": ["number", "null"] },
                "memoryUsedBytes": { "type": ["integer", "null"] },
                "memoryTotalBytes": { "type": ["integer", "null"] },
                "powerDrawW": { "type": ["number", "null"] },
                "powerLimitW": { "type": ["number", "null"] },
                "temperatureC": { "type": ["number", "null"] },
                "smClockMhz": { "type": ["integer", "null"] },
                "eventReasons": { "type": ["string", "null"] },
            }),
            &[
                "utilizationPct",
                "memoryUsedBytes",
                "memoryTotalBytes",
                "powerDrawW",
                "powerLimitW",
                "temperatureC",
                "smClockMhz",
                "eventReasons",
            ],
        );
        schema["type"] = json!(["object", "null"]);
        schema
    };
    let owner = open(
        json!({
            "kind": { "type": "string", "enum": ["instance", "container", "host"] },
            "project": { "type": "string" },
            "name": { "type": "string" },
            "container": { "type": "string" },
        }),
        &["kind"],
    );
    let card = open(
        json!({
            "pci": { "type": "string" },
            "uuid": { "type": ["string", "null"] },
            "vendor": { "type": "string" },
            "product": { "type": "string" },
            "driver": { "type": ["string", "null"] },
            "driverVersion": { "type": ["string", "null"] },
            "numaNode": { "type": "integer" },
            "vramTotalBytes": { "type": ["integer", "null"] },
            "use": { "type": "string", "enum": ["driver-bound", "vfio-pci", "no-driver"] },
            "telemetry": telemetry,
            "reason": { "type": ["string", "null"] },
            "processes": {
                "type": "array",
                "items": open(
                    json!({
                        "pci": { "type": "string" },
                        "pid": { "type": "integer" },
                        "vramBytes": { "type": ["integer", "null"] },
                        "owner": owner,
                    }),
                    &["pci", "pid", "vramBytes", "owner"],
                ),
            },
            "processReason": { "type": ["string", "null"] },
        }),
        &[
            "pci",
            "uuid",
            "vendor",
            "product",
            "driver",
            "driverVersion",
            "numaNode",
            "vramTotalBytes",
            "use",
            "telemetry",
            "reason",
            "processes",
            "processReason",
        ],
    );
    open(
        json!({
            "reason": { "type": ["string", "null"] },
            "skipped": { "type": ["string", "null"] },
            "cards": { "type": "array", "items": card },
        }),
        &["reason", "skipped", "cards"],
    )
}

fn host_sample() -> Value {
    json!({
        "type": "object",
        "properties": {
            "at": { "type": "integer" },
            "cpu": open(
                json!({
                    "usedPct": { "type": ["number", "null"] },
                    "cores": { "type": "integer" },
                    "coresKnown": { "type": "boolean" },
                    "reason": { "type": ["string", "null"] }
                }),
                &["usedPct", "cores", "coresKnown", "reason"]
            ),
            "load": open(
                json!({
                    "one": { "type": ["number", "null"] },
                    "five": { "type": ["number", "null"] },
                    "fifteen": { "type": ["number", "null"] },
                    "reason": { "type": ["string", "null"] }
                }),
                &["one", "five", "fifteen", "reason"]
            ),
            "memory": open(
                json!({
                    "totalBytes": { "type": ["integer", "null"] },
                    "availableBytes": { "type": ["integer", "null"] },
                    "usedBytes": { "type": ["integer", "null"] },
                    "swapTotalBytes": { "type": ["integer", "null"] },
                    "swapFreeBytes": { "type": ["integer", "null"] },
                    "reason": { "type": ["string", "null"] }
                }),
                &["totalBytes", "availableBytes", "usedBytes", "swapTotalBytes", "swapFreeBytes", "reason"]
            ),
            "psi": open(
                json!({
                    "memorySomeAvg10": { "type": ["number", "null"] },
                    "memoryFullAvg10": { "type": ["number", "null"] },
                    "cpuSomeAvg10": { "type": ["number", "null"] }
                }),
                &["memorySomeAvg10", "memoryFullAvg10", "cpuSomeAvg10"]
            ),
            "cpuTempC": { "type": ["number", "null"] },
            "gpuBusyPct": { "type": ["number", "null"] },
            "gpu": gpu_section(),
            "pools": {
                "type": "array",
                "items": open(
                    json!({
                        "name": { "type": "string" },
                        "usedBytes": { "type": ["integer", "null"] },
                        "totalBytes": { "type": ["integer", "null"] },
                        "freeBytes": { "type": ["integer", "null"] }
                    }),
                    &["name", "usedBytes", "totalBytes", "freeBytes"]
                )
            },
            "network": {
                "type": ["object", "null"],
                "properties": {
                    "rxBytesPerSec": { "type": "number" },
                    "txBytesPerSec": { "type": "number" }
                },
                "required": ["rxBytesPerSec", "txBytesPerSec"]
            },
            "projects": {
                "type": "array",
                "items": open(
                    json!({
                        "project": { "type": "string" },
                        "total": { "type": "integer" },
                        "running": { "type": "integer" }
                    }),
                    &["project", "total", "running"]
                )
            },
            "instancesNotRead": {
                "type": "array",
                "items": open(
                    json!({
                        "project": { "type": "string", "maxLength": 64 },
                        "name": { "type": "string", "maxLength": 64 },
                        "why": {
                            "type": "string",
                            "enum": [
                                "Incus did not answer within 2 seconds",
                                "Incus answered with an error",
                                "Incus answered without the CPU or the memory figure",
                                "the tick's time for state reads was spent"
                            ]
                        }
                    }),
                    &["project", "name", "why"]
                ),
                "description": "The running instances whose CPU and memory this sample's tick did not read, and why; in the mean, the newest sample's. Each tick reads every running instance's own live state, one after another, 2 seconds each and 4 seconds for all; the ones asked longest ago go first, so each is read within a few ticks. An instance named here is still counted, with its CPU and memory left out of that tick's export. Empty when every one was read, or when the instance list itself could not be read."
            }
        },
        "required": [
            "at", "cpu", "load", "memory", "psi", "cpuTempC", "gpuBusyPct", "gpu", "pools", "network", "projects",
            "instancesNotRead"
        ]
    })
}

/// Whether `key` appears as an object key anywhere in `v`.
fn mentions_key(v: &Value, key: &str) -> bool {
    match v {
        Value::Object(map) => map.contains_key(key) || map.values().any(|x| mentions_key(x, key)),
        Value::Array(items) => items.iter().any(|x| mentions_key(x, key)),
        _ => false,
    }
}

#[test]
fn viewer_cannot_call_postgres_write_verbs() {
    for verb in verbs().iter().filter(|v| v.name.starts_with("postgres_")) {
        assert_eq!(may_call(Scope::Viewer, verb), !verb.writes);
        assert!(may_call(Scope::Doctor, verb));
    }
}

#[test]
fn postgres_delete_accepts_a_waiting_plan_or_a_settled_removal() {
    let schema = (verb("postgres_delete").unwrap().result)();
    assert_eq!(schema["required"], json!(["plan", "removed"]));
    assert_eq!(
        schema["properties"]["plan"]["type"],
        json!(["object", "null"])
    );
    assert!(conforms(&schema, &json!({"plan":null,"removed":true})).is_ok());
    assert!(conforms(&schema, &json!({"plan":null})).is_err());
    assert!(conforms(&schema, &json!({"plan":null,"removed":"true"})).is_err());
}

#[test]
fn service_wiring_schemas_equal_the_frozen_contract() {
    let rows: Value = serde_json::from_str(include_str!("fixtures/service-wiring.json")).unwrap();
    for row in rows.as_array().unwrap() {
        let v = verb(row["name"].as_str().unwrap()).unwrap();
        assert_eq!((v.params)(), row["inputSchema"]);
        assert_eq!((v.result)(), row["outputSchema"]);
    }
}

/// A schema as the parity check compares it: annotations dropped, an open `additionalProperties` dropped, and `required`
/// sorted, since its order never changes a schema.
fn parity_shape(schema: &Value) -> Value {
    match schema {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, sub) in map {
                if ["description", "title", "examples"].contains(&key.as_str())
                    || (key == "additionalProperties" && sub == &Value::Bool(true))
                {
                    continue;
                }
                let mut sub = parity_shape(sub);
                if key == "required"
                    && let Some(list) = sub.as_array_mut()
                {
                    list.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
                }
                out.insert(key.clone(), sub);
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(parity_shape).collect()),
        other => other.clone(),
    }
}

#[test]
fn omni_schemas_equal_the_golden_and_every_omni_verb_is_untrusted() {
    let rows: Value = serde_json::from_str(include_str!("fixtures/omni.json")).unwrap();
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 3);
    for row in rows {
        let v = verb(row["name"].as_str().unwrap()).unwrap();
        assert_eq!(
            parity_shape(&(v.params)()),
            parity_shape(&row["inputSchema"]),
            "{}",
            v.name
        );
        assert_eq!(
            parity_shape(&(v.result)()),
            parity_shape(&row["outputSchema"]),
            "{}",
            v.name
        );
        assert!(v.untrusted, "{} is untrusted", v.name);
        assert!(!v.destructive, "{}", v.name);
        assert_eq!(v.min_scope, Scope::Doctor, "{}", v.name);
        assert_eq!(v.read_only_hint(), v.name == "omni_status", "{}", v.name);
    }
}

#[test]
fn owner_approval_schemas_equal_the_golden_with_list_untrusted_and_status_not_read_only() {
    let rows: Value = serde_json::from_str(include_str!("fixtures/owner-approval.json")).unwrap();
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 3);
    for row in rows {
        let v = verb(row["name"].as_str().unwrap()).unwrap();
        // Arca's own action beside the golden's three: `enroll`, spent with `enroll_confirm`.
        let mut params = (v.params)();
        if v.name == "owner_approval_request" {
            let actions = params["properties"]["action"]["enum"]
                .as_array_mut()
                .unwrap();
            assert_eq!(actions.pop(), Some(json!("enroll")));
        }
        assert_eq!(
            parity_shape(&params),
            parity_shape(&row["inputSchema"]),
            "{}",
            v.name
        );
        assert_eq!(
            parity_shape(&(v.result)()),
            parity_shape(&row["outputSchema"]),
            "{}",
            v.name
        );
        assert!(!v.destructive, "{}", v.name);
        // The golden's flags: only the list is readOnly and untrusted.
        assert_eq!(
            v.read_only_hint(),
            v.name == "owner_approval_list",
            "{}",
            v.name
        );
        assert_eq!(v.untrusted, v.name == "owner_approval_list", "{}", v.name);
    }
    // The request and the status are a doctor's at least (the status hands out an approval); the list is a viewer read.
    assert_eq!(
        verb("owner_approval_request").unwrap().min_scope,
        Scope::Doctor
    );
    assert_eq!(
        verb("owner_approval_status").unwrap().min_scope,
        Scope::Doctor
    );
    assert_eq!(
        verb("owner_approval_list").unwrap().min_scope,
        Scope::Viewer
    );
}

#[test]
fn every_destructive_verb_takes_an_optional_boolean_confirm_and_no_other_verb_does() {
    let destructive: Vec<&str> = verbs()
        .iter()
        .filter(|v| v.destructive)
        .map(|v| v.name)
        .collect();
    assert_eq!(
        destructive,
        [
            "approval_submit",
            "action_approve",
            "postgres_delete",
            "service_remove",
            "service_update",
            "service_import",
            "service_release",
            "gpu_detach",
            "storage_add",
            "snapshot_delete",
            "instance_stop",
            "instance_restart",
            "tenant_remove",
            "deploy_rollback",
            "deploy_remove",
            "function_remove",
        ]
    );
    for v in verbs() {
        let params = (v.params)();
        if !v.destructive {
            assert!(params["properties"].get("confirm").is_none(), "{}", v.name);
            continue;
        }
        assert_eq!(params["properties"]["confirm"], confirm(), "{}", v.name);
        assert!(
            !params["required"]
                .as_array()
                .is_some_and(|required| required.contains(&json!("confirm"))),
            "{}: confirm is optional",
            v.name
        );
        assert!(
            conforms(&params["properties"]["confirm"], &json!(true)).is_ok()
                && conforms(&params["properties"]["confirm"], &json!(false)).is_ok()
        );
        for wrong in [json!("yes"), json!(1), json!(null)] {
            assert!(
                conforms(&params["properties"]["confirm"], &wrong).is_err(),
                "{}: confirm {wrong} is refused",
                v.name
            );
        }
    }
}

#[test]
fn take_confirm_drops_true_refuses_false_and_leaves_other_verbs_alone() {
    use arca_api::verbs::take_confirm;
    let remove = verb("service_remove").expect("present");
    let mut absent = json!({ "name": "pg", "approval": "x" });
    assert_eq!(take_confirm(remove, &mut absent), Ok(()));
    assert_eq!(absent, json!({ "name": "pg", "approval": "x" }));
    let mut yes = json!({ "name": "pg", "approval": "x", "confirm": true });
    assert_eq!(take_confirm(remove, &mut yes), Ok(()));
    assert_eq!(
        yes,
        json!({ "name": "pg", "approval": "x" }),
        "confirm is gone"
    );
    let mut no = json!({ "name": "pg", "approval": "x", "confirm": false });
    assert_eq!(
        take_confirm(remove, &mut no),
        Err("confirm is false: nothing was removed and no approval was spent".to_string())
    );
    let pg = verb("postgres_delete").expect("present");
    assert_eq!(
        take_confirm(pg, &mut json!({ "name": "orders", "confirm": false })),
        Err("confirm is false: nothing was deleted".to_string()),
        "postgres_delete takes no approval, so it names none"
    );
    let create = verb("service_create").expect("present");
    let mut other = json!({ "name": "pg", "template": "pg", "confirm": false });
    assert_eq!(take_confirm(create, &mut other), Ok(()));
    assert_eq!(
        other["confirm"],
        json!(false),
        "a verb that is not destructive is untouched"
    );
}

/// droidd's golden row for `service_remove` (droid `packages/droidd/test/fixtures/arca-moved-tools.json`, source droid dev
/// 91f5a95): `confirm` is an optional boolean.
#[test]
fn service_remove_params_equal_droidds_golden_row() {
    let golden = json!({
        "additionalProperties": false,
        "properties": {
            "approval": { "maxLength": 16384, "minLength": 1, "type": "string" },
            "confirm": { "type": "boolean" },
            "name": { "pattern": "^[a-z](?:[a-z0-9-]{0,38}[a-z0-9])?$", "type": "string" },
            // Arca's own addition since the tools moved: the optional tenant.
            "tenant": { "pattern": "^[a-z2-7]{8}$", "type": "string" }
        },
        "required": ["approval", "name"],
        "type": "object"
    });
    let v = verb("service_remove").expect("present");
    assert_eq!(parity_shape(&(v.params)()), golden);
    assert!(v.destructive);
}

/// An assistant chooses a tool by its description, so none is a bare label and none is a page: at least
/// [`SHORTEST_SUMMARY`] characters, at most [`LONGEST_SUMMARY`].
const SHORTEST_SUMMARY: usize = 60;
const LONGEST_SUMMARY: usize = 700;

#[test]
fn every_verb_description_says_what_the_verb_does_in_60_to_700_characters() {
    for verb in arca_api::verbs() {
        let length = verb.summary.chars().count();
        assert!(
            (SHORTEST_SUMMARY..=LONGEST_SUMMARY).contains(&length),
            "{}: its description is {length} characters, outside {SHORTEST_SUMMARY} to {LONGEST_SUMMARY}: {}",
            verb.name,
            verb.summary
        );
    }
}

#[test]
fn the_postgres_descriptions_say_what_each_verb_returns() {
    let summary = |name: &str| verb(name).expect("present").summary;
    for (name, words) in [
        (
            "postgres_create",
            vec!["digest-pinned", "postgres_list", "postgres_link"],
        ),
        (
            "postgres_list",
            vec!["name", "status", "address", "port", "empty list"],
        ),
        ("postgres_link", vec!["DATABASE_URL", "never the URL"]),
        ("postgres_unlink", vec!["variables removed", "data stay"]),
        (
            "postgres_delete",
            vec!["at once", "removed: false", "delete plan"],
        ),
    ] {
        for word in words {
            assert!(
                summary(name).contains(word),
                "{name}: its description names {word}: {}",
                summary(name)
            );
        }
    }
}

/// The text a client shows of a read is the answer itself, not only a count: a client that does not show the
/// structured content to its model still gets each instance, each service and the node's state. Every value is
/// synthetic.
#[test]
fn the_text_of_a_watch_report_a_service_list_and_a_status_carries_the_answer() {
    let summary = |name: &str, value: Value| (verb(name).expect(name).summary_text)(&value);
    let report = json!({ "at": 1, "projects": [
        { "project": "default", "instances": [
            { "name": "web", "kind": "container", "status": "Running" },
            { "name": "db", "kind": "virtual-machine", "status": "Stopped" },
        ] },
        { "project": "empty", "instances": [] },
    ] });
    assert_eq!(
        summary("watch_report", report),
        "2 instance(s) in 2 project(s)\n- web in default: container, Running\n- db in default: virtual-machine, Stopped"
    );
    assert_eq!(
        summary(
            "service_list",
            json!({ "services": [
            { "name": "nats", "template": "nats", "state": "running", "ipv4": "192.0.2.20" },
            // A machine on a host bridge: its address is in no record, only in the live state.
            { "name": "wiki", "template": "automagik-workspace", "state": "running", "ipv4": "", "ipv4In": "state" },
            // An instance the node holds no record of.
            { "name": "stray", "template": "", "state": "unknown", "ipv4": "" },
        ] })
        ),
        "3 service(s)\n- nats (nats): running at 192.0.2.20\n- wiki (automagik-workspace): running, address in its live state (service_status reads it)\n- stray (no template): unknown, no address"
    );
    assert_eq!(
        summary("service_list", json!({ "services": [] })),
        "0 service(s)"
    );
    assert_eq!(
        summary(
            "arca_status",
            json!({ "version": "0.15.0", "mode": "watch-only", "incusReachable": true, "uptimeS": 3_725 })
        ),
        "arca 0.15.0 (watch-only mode)\nIncus answers; up 1 h 2 min"
    );
    assert_eq!(
        summary(
            "arca_status",
            json!({ "version": "0.15.0", "mode": "kernel", "incusReachable": false, "uptimeS": 42 })
        ),
        "arca 0.15.0 (kernel mode)\nIncus does not answer; up 42 s"
    );
    // A long list names the first fifty and counts the rest; the structured answer keeps them all.
    let many: Vec<Value> = (0..53)
        .map(|n| json!({ "name": format!("i{n}"), "kind": "container", "status": "Running" }))
        .collect();
    let text = summary(
        "watch_report",
        json!({ "at": 1, "projects": [{ "project": "p", "instances": many }] }),
    );
    assert_eq!(text.lines().count(), 1 + 50 + 1, "{text}");
    assert!(
        text.starts_with("53 instance(s) in 1 project(s)\n- i0 in p: container, Running\n"),
        "{text}"
    );
    assert!(
        text.ends_with("\n- i49 in p: container, Running\n- and 3 more"),
        "{text}"
    );
}

/// `action_propose` describes each action's `args`: one branch per action, titled with its name, holding the fields
/// of the verb it proposes (less `approval`) and an example that the branch takes. An assistant reads
/// what each action takes from the tool's own schema, not from refusals. Issue 113.
#[test]
fn action_propose_describes_the_args_of_every_action_with_an_example_it_takes() {
    let params = (verb("action_propose").expect("the verb").params)();
    let actions: Vec<&str> = params["properties"]["action"]["enum"]
        .as_array()
        .expect("the actions")
        .iter()
        .map(|action| action.as_str().expect("a name"))
        .collect();
    assert_eq!(actions.len(), 23);
    let args = &params["properties"]["args"];
    assert_eq!(args["type"], json!("object"));
    assert_eq!(args["x-arca-by"], json!("action"));
    let branches = args["anyOf"].as_array().expect("one branch per action");
    let titles: Vec<&str> = branches
        .iter()
        .map(|branch| branch["title"].as_str().expect("a title"))
        .collect();
    assert_eq!(titles, actions, "one branch per action, in the same order");
    for (action, branch) in actions.iter().zip(branches) {
        let mut own = (verb(action).expect("a verb").params)();
        if let Some(required) = own["required"].as_array_mut() {
            required.retain(|field| field != "approval");
        }
        let fields = own["properties"].as_object_mut().expect("properties");
        fields.remove("approval");
        for field in kernel_filled(action) {
            let said = fields[*field]["description"]
                .as_str()
                .map(|said| format!("{said} "))
                .unwrap_or_default();
            fields.insert(
                (*field).to_string(),
                json!({ "description": format!("{said}{KERNEL_FILLS_IN}") }),
            );
        }
        assert_eq!(branch["properties"], own["properties"], "{action}");
        assert_eq!(branch["required"], own["required"], "{action}");
        assert_eq!(branch["additionalProperties"], json!(false), "{action}");
        let example = &branch["examples"][0];
        for field in kernel_filled(action) {
            assert!(
                example.get(*field).is_none(),
                "{action}: the example holds {field}, which the kernel fills in"
            );
        }
        let description = branch["description"].as_str().expect("a description");
        assert!(
            description.starts_with(&format!("The args of {action}: "))
                && description.ends_with(&format!("Example: {example}")),
            "{action}: {description}"
        );
        // The example is one the branch, and the whole call, take.
        assert_eq!(conforms(branch, example), Ok(()), "{action}");
        assert_eq!(
            conforms(&params, &json!({ "action": action, "args": example })),
            Ok(()),
            "{action}"
        );
    }
}

/// The door holds `args` to the branch its `action` names, so a refusal is in that branch's own words; an action that
/// is not one is refused by its own list, with the nearest name.
#[test]
fn action_propose_args_are_refused_in_the_words_of_the_actions_own_branch() {
    let params = (verb("action_propose").expect("the verb").params)();
    let refused = |call: Value| conforms(&params, &call).expect_err("refused");
    assert_eq!(
        refused(json!({ "action": "function_create", "args": {} })),
        "/args: missing required field name"
    );
    assert_eq!(
        refused(json!({ "action": "function_create", "args": { "name": "relay", "imgae": "x" } })),
        "/args: missing required field image"
    );
    assert_eq!(
        refused(json!({ "action": "tenant_remove", "args": { "id": "abcdefgh", "nmae": 1 } })),
        "/args: unknown field 'nmae'; it takes: confirm, id"
    );
    assert_eq!(
        refused(json!({ "action": "service_publish", "args": { "name": "dash", "reach": 7 } })),
        "/args/reach: expected string"
    );
    // Fields another action takes are not this one's.
    assert_eq!(
        refused(json!({ "action": "tenant_remove", "args": { "name": "nats" } })),
        "/args: missing required field id"
    );
    assert!(
        refused(json!({ "action": "postgres_creat", "args": { "name": "orders" } }))
            .ends_with("; did you mean postgres_create?")
    );
}

/// No tool's input schema has `anyOf`, `oneOf` or `allOf` at its top level: a client that sends the schema to a
/// model's API is refused for one there, so each union stays inside a field.
#[test]
fn no_input_schema_has_a_union_at_its_top_level() {
    for v in verbs() {
        let params = (v.params)();
        for union in ["anyOf", "oneOf", "allOf"] {
            assert!(
                params.get(union).is_none(),
                "{} has {union} at its top",
                v.name
            );
        }
        assert_eq!(params["type"], json!("object"), "{}", v.name);
    }
}

/// A field the kernel fills in when the proposal is made takes any value at the door, as it did before `args` was held
/// to a branch: the kernel removes or replaces it before the action's own check runs. Each one is sent with a value
/// its verb's own rule refuses, and the call conforms. Review M1 of PR 136.
#[test]
fn a_field_the_kernel_fills_in_is_taken_at_the_door_with_any_value() {
    let params = (verb("action_propose").expect("the verb").params)();
    let branches = params["properties"]["args"]["anyOf"]
        .as_array()
        .expect("one branch per action");
    let off_rule = json!({ "off": "rule" });
    for (action, fields) in KERNEL_FILLED {
        let own = (verb(action).expect("a verb").params)();
        let branch = branches
            .iter()
            .find(|branch| branch["title"] == json!(action))
            .expect("a branch");
        for field in fields {
            let rule = &own["properties"][*field];
            assert!(rule.is_object(), "{action} takes {field}");
            assert!(
                conforms(rule, &off_rule).is_err(),
                "{action}/{field}: off its rule"
            );
            let said = branch["properties"][*field]["description"]
                .as_str()
                .unwrap_or_default();
            assert!(said.ends_with(KERNEL_FILLS_IN), "{action}/{field}: {said}");
            let mut args = branch["examples"][0].clone();
            args[*field] = off_rule.clone();
            assert_eq!(
                conforms(&params, &json!({ "action": action, "args": args })),
                Ok(()),
                "{action}/{field}"
            );
        }
    }
    // A snapshot_list row's kind, copied into a delete.
    let row = json!({ "name": "shop--web--20261030T1200", "pool": "usb1", "kind": "container" });
    assert_eq!(
        conforms(
            &params,
            &json!({ "action": "snapshot_delete", "args": row })
        ),
        Ok(())
    );
}
