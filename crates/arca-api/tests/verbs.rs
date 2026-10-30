//! The verb table, the OpenRPC document and the released 0.1.0 schema. Each expected schema below is written out by hand from
//! the table, so a typo in the constructors fails here.

use arca_api::openrpc::{check_document, openrpc};
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
fn the_table_holds_the_four_initial_verbs_then_the_three_plan_verbs_in_order() {
    assert_eq!(
        names(),
        [
            "arca_status",
            "node_metrics",
            "watch_report",
            "token_list",
            "plan_list",
            "plan_decline",
            "approval_submit",
        ]
    );
    assert_eq!(API_VERSION, "0.1.0");
}

#[test]
fn each_verb_has_its_table_row_for_class_scope_and_writes() {
    let rows = [
        ("arca_status", Scope::Viewer, false, false),
        ("node_metrics", Scope::Viewer, false, false),
        ("watch_report", Scope::Viewer, false, false),
        ("token_list", Scope::Operator, false, false),
        ("plan_list", Scope::Viewer, false, false),
        ("plan_decline", Scope::Doctor, true, false),
        ("approval_submit", Scope::Doctor, true, true),
    ];
    for (name, scope, writes, destructive) in rows {
        let v = verb(name).expect("the verb is in the table");
        assert_eq!(v.class, VerbClass::L, "{name} is class L");
        assert_eq!(v.min_scope, scope, "{name} minimum scope");
        assert_eq!(v.writes, writes, "{name} writes");
        assert_eq!(v.destructive, destructive, "{name} destructive");
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

    let list = verb("plan_list").expect("present");
    assert_eq!((list.params)(), closed(json!({})));
    assert_eq!(
        (list.result)(),
        open(
            json!({ "plans": { "type": "array", "items": plan } }),
            &["plans"]
        )
    );

    let decline = verb("plan_decline").expect("present");
    assert_eq!(
        (decline.params)(),
        closed_with(json!({ "plan": hex64() }), &["plan"])
    );
    assert_eq!(
        (decline.result)(),
        open(
            json!({
                "plan": hex64(),
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
                "appliedSeq": { "type": ["integer", "null"] }
            }),
            &[
                "version",
                "node",
                "mode",
                "uptimeS",
                "incusReachable",
                "appliedSeq"
            ]
        )
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
                            "scope": { "type": "string", "enum": ["viewer", "doctor", "operator"] }
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
    assert_eq!(check_document(&doc), Ok(()));
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
        "addedLater": 1
    });
    assert!(conforms(&result, &value).is_ok(), "a result stays open");
}

#[test]
fn a_host_sample_with_nulls_conforms_to_the_result_schema() {
    let result = (verb("node_metrics").expect("present").result)();
    let sample = json!({
        "at": 1,
        "cpu": { "usedPct": null, "cores": 8 },
        "load": { "one": 0.5, "five": null, "fifteen": 0.1 },
        "memory": { "totalBytes": 1, "availableBytes": null, "swapTotalBytes": 0, "swapFreeBytes": null },
        "psi": { "memorySomeAvg10": 0.0, "memoryFullAvg10": null, "cpuSomeAvg10": 1.5 },
        "cpuTempC": null,
        "gpuBusyPct": null,
        "pools": [{ "name": "p", "usedBytes": null, "totalBytes": 9, "freeBytes": null }],
        "network": null,
        "projects": [{ "project": "p", "total": 2, "running": 1 }]
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

fn hex64() -> Value {
    json!({ "type": "string", "minLength": 64, "maxLength": 64 })
}

/// `T|null` for an object: the object schema with `null` added to its types.
fn nullable_object(mut schema: Value) -> Value {
    schema["type"] = json!(["object", "null"]);
    schema
}

fn host_sample() -> Value {
    json!({
        "type": "object",
        "properties": {
            "at": { "type": "integer" },
            "cpu": open(
                json!({ "usedPct": { "type": ["number", "null"] }, "cores": { "type": "integer" } }),
                &["usedPct", "cores"]
            ),
            "load": open(
                json!({
                    "one": { "type": ["number", "null"] },
                    "five": { "type": ["number", "null"] },
                    "fifteen": { "type": ["number", "null"] }
                }),
                &["one", "five", "fifteen"]
            ),
            "memory": open(
                json!({
                    "totalBytes": { "type": ["integer", "null"] },
                    "availableBytes": { "type": ["integer", "null"] },
                    "swapTotalBytes": { "type": ["integer", "null"] },
                    "swapFreeBytes": { "type": ["integer", "null"] }
                }),
                &["totalBytes", "availableBytes", "swapTotalBytes", "swapFreeBytes"]
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
            }
        },
        "required": [
            "at", "cpu", "load", "memory", "psi", "cpuTempC", "gpuBusyPct", "pools", "network", "projects"
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
