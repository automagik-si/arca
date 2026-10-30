//! The verb table: every Arca API verb with its class, the minimum scope that may call it, whether it writes, and the exact
//! JSON Schemas of its arguments and its result. The OpenRPC document and the MCP tool list are both read from this table.
//!
//! Params objects are closed (`additionalProperties: false`). Result objects are open: a client must accept fields it does
//! not know, so a later release can add a field without a break.

use serde::Serialize;
use serde_json::{Map, Value, json};

/// The class of a verb. Every initial verb is `L`: it runs on one node and needs no second node or vote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum VerbClass {
    L,
    B,
    Q,
}

impl VerbClass {
    /// The class as the document and the table spell it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::L => "L",
            Self::B => "B",
            Self::Q => "Q",
        }
    }
}

/// The scope a bearer token carries. The order is the order of privilege: a higher scope may call every verb a lower one
/// may.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Viewer,
    Doctor,
    Operator,
}

impl Scope {
    /// The scope as a token file and the document spell it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Doctor => "doctor",
            Self::Operator => "operator",
        }
    }
}

/// One verb of the API. `params` and `result` return the verb's JSON Schemas; `params` is a closed object and `result` an
/// open one.
#[derive(Debug, Clone, Copy)]
pub struct Verb {
    pub name: &'static str,
    pub summary: &'static str,
    pub class: VerbClass,
    pub min_scope: Scope,
    pub writes: bool,
    pub destructive: bool,
    pub params: fn() -> Value,
    pub result: fn() -> Value,
}

static VERBS: [Verb; 7] = [
    Verb {
        name: "arca_status",
        summary: "Node status: the API version, the node id, the mode, the uptime, whether Incus answers and the last applied sequence number.",
        class: VerbClass::L,
        min_scope: Scope::Viewer,
        writes: false,
        destructive: false,
        params: arca_status_params,
        result: arca_status_result,
    },
    Verb {
        name: "node_metrics",
        summary: "Host metrics of the node: the latest sample and the mean over the requested minutes.",
        class: VerbClass::L,
        min_scope: Scope::Viewer,
        writes: false,
        destructive: false,
        params: node_metrics_params,
        result: node_metrics_result,
    },
    Verb {
        name: "watch_report",
        summary: "Every instance in every project with its ownership class, and the drift between marks and desired state.",
        class: VerbClass::L,
        min_scope: Scope::Viewer,
        writes: false,
        destructive: false,
        params: watch_report_params,
        result: watch_report_result,
    },
    Verb {
        name: "token_list",
        summary: "The interim API tokens by id and scope. Never a secret or a hash.",
        class: VerbClass::L,
        min_scope: Scope::Operator,
        writes: false,
        destructive: false,
        params: token_list_params,
        result: token_list_result,
    },
    Verb {
        name: "plan_list",
        summary: "The delete plans waiting for an owner's approval, each with the exact statement the owner signs.",
        class: VerbClass::L,
        min_scope: Scope::Viewer,
        writes: false,
        destructive: false,
        params: plan_list_params,
        result: plan_list_result,
    },
    Verb {
        name: "plan_decline",
        summary: "Declines a waiting delete plan. Its objects are retained, and the decline is final.",
        class: VerbClass::L,
        min_scope: Scope::Doctor,
        writes: true,
        destructive: false,
        params: plan_decline_params,
        result: plan_decline_result,
    },
    Verb {
        name: "approval_submit",
        summary: "Submits an owner's approval for a waiting delete plan, which deletes the plan's objects once. The approval is never echoed.",
        class: VerbClass::L,
        min_scope: Scope::Doctor,
        writes: true,
        destructive: true,
        params: approval_submit_params,
        result: approval_submit_result,
    },
];

/// Every verb of the table, in table order.
pub fn verbs() -> &'static [Verb] {
    &VERBS
}

/// The verb with this name, if the table has one.
pub fn verb(name: &str) -> Option<&'static Verb> {
    VERBS.iter().find(|v| v.name == name)
}

/// Whether a token of `scope` may call `v`. The rule is the scope order alone; the table keeps viewers off write verbs by
/// giving every write verb a minimum scope of `Doctor` or higher.
pub fn may_call(scope: Scope, v: &Verb) -> bool {
    scope >= v.min_scope
}

// The JSON Schema builders. A builder yields exactly the subset `check::conforms` reads.

/// One named field of an object: its name, whether it is required, and its schema.
type Field = (&'static str, bool, Value);

fn string() -> Value {
    json!({ "type": "string" })
}

fn int() -> Value {
    json!({ "type": "integer" })
}

fn num() -> Value {
    json!({ "type": "number" })
}

fn boolean() -> Value {
    json!({ "type": "boolean" })
}

/// A 64-character string. The format (lowercase hex) is checked by the handlers, not by the schema.
fn hex64() -> Value {
    json!({ "type": "string", "minLength": 64, "maxLength": 64 })
}

/// A string of at most `max` characters.
fn string_max(max: i64) -> Value {
    json!({ "type": "string", "maxLength": max })
}

fn one_of(values: &[&str]) -> Value {
    json!({ "type": "string", "enum": values })
}

fn bounded_int(min: i64, max: i64) -> Value {
    json!({ "type": "integer", "minimum": min, "maximum": max })
}

fn at_least(min: i64) -> Value {
    json!({ "type": "integer", "minimum": min })
}

fn array(items: Value) -> Value {
    json!({ "type": "array", "items": items })
}

/// The schema with `null` added to its types, as `T|null`.
fn nullable(mut schema: Value) -> Value {
    if let Some(obj) = schema.as_object_mut() {
        let base = obj.get("type").cloned().unwrap_or(Value::Null);
        obj.insert("type".to_string(), json!([base, "null"]));
    }
    schema
}

fn object_of(fields: Vec<Field>, closed: bool) -> Value {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for (name, is_required, schema) in fields {
        if is_required {
            required.push(Value::from(name));
        }
        properties.insert(name.to_string(), schema);
    }
    let mut obj = Map::new();
    obj.insert("type".to_string(), Value::from("object"));
    obj.insert("properties".to_string(), Value::Object(properties));
    if !required.is_empty() {
        obj.insert("required".to_string(), Value::Array(required));
    }
    if closed {
        obj.insert("additionalProperties".to_string(), Value::Bool(false));
    }
    Value::Object(obj)
}

/// A params object: closed, so an argument the schema does not name is refused.
fn params(fields: Vec<Field>) -> Value {
    object_of(fields, true)
}

/// A result object: open, so a client accepts a field it does not know.
fn result(fields: Vec<Field>) -> Value {
    object_of(fields, false)
}

fn arca_status_params() -> Value {
    params(vec![])
}

fn arca_status_result() -> Value {
    result(vec![
        ("version", true, string()),
        ("node", true, hex64()),
        ("mode", true, one_of(&["watch-only", "kernel"])),
        ("uptimeS", true, at_least(0)),
        ("incusReachable", true, boolean()),
        ("appliedSeq", true, nullable(int())),
    ])
}

fn node_metrics_params() -> Value {
    params(vec![("minutes", false, bounded_int(1, 60))])
}

fn node_metrics_result() -> Value {
    result(vec![
        ("at", true, int()),
        ("minutes", true, int()),
        ("latest", true, nullable(host_sample())),
        ("mean", true, nullable(host_sample())),
    ])
}

/// One host sample. Every key is required; a measurement the host cannot give is `null`, not absent.
fn host_sample() -> Value {
    result(vec![
        ("at", true, int()),
        (
            "cpu",
            true,
            result(vec![
                ("usedPct", true, nullable(num())),
                ("cores", true, int()),
            ]),
        ),
        (
            "load",
            true,
            result(vec![
                ("one", true, nullable(num())),
                ("five", true, nullable(num())),
                ("fifteen", true, nullable(num())),
            ]),
        ),
        (
            "memory",
            true,
            result(vec![
                ("totalBytes", true, nullable(int())),
                ("availableBytes", true, nullable(int())),
                ("swapTotalBytes", true, nullable(int())),
                ("swapFreeBytes", true, nullable(int())),
            ]),
        ),
        (
            "psi",
            true,
            result(vec![
                ("memorySomeAvg10", true, nullable(num())),
                ("memoryFullAvg10", true, nullable(num())),
                ("cpuSomeAvg10", true, nullable(num())),
            ]),
        ),
        ("cpuTempC", true, nullable(num())),
        ("gpuBusyPct", true, nullable(num())),
        (
            "pools",
            true,
            array(result(vec![
                ("name", true, string()),
                ("usedBytes", true, nullable(int())),
                ("totalBytes", true, nullable(int())),
                ("freeBytes", true, nullable(int())),
            ])),
        ),
        (
            "network",
            true,
            nullable(result(vec![
                ("rxBytesPerSec", true, num()),
                ("txBytesPerSec", true, num()),
            ])),
        ),
        (
            "projects",
            true,
            array(result(vec![
                ("project", true, string()),
                ("total", true, int()),
                ("running", true, int()),
            ])),
        ),
    ])
}

fn watch_report_params() -> Value {
    params(vec![])
}

fn watch_report_result() -> Value {
    result(vec![
        ("at", true, int()),
        (
            "projects",
            true,
            array(result(vec![
                ("project", true, string()),
                (
                    "instances",
                    true,
                    array(result(vec![
                        ("name", true, string()),
                        ("kind", true, one_of(&["container", "virtual-machine"])),
                        ("status", true, string()),
                        (
                            "owner",
                            true,
                            result(vec![
                                (
                                    "class",
                                    true,
                                    one_of(&[
                                        "droidd",
                                        "foreign",
                                        "kernel",
                                        "retained",
                                        "contested",
                                    ]),
                                ),
                                ("entry", false, string()),
                                ("plugin", false, string()),
                                ("plan", false, hex64()),
                            ]),
                        ),
                        ("privileged", true, boolean()),
                        ("identity", true, boolean()),
                        ("configHash", true, hex64()),
                    ])),
                ),
            ])),
        ),
        (
            "drift",
            true,
            array(result(vec![
                (
                    "kind",
                    true,
                    one_of(&["markedWithoutEntry", "entryWithoutObject"]),
                ),
                ("entry", true, string()),
                ("project", false, string()),
                ("name", false, string()),
            ])),
        ),
    ])
}

fn token_list_params() -> Value {
    params(vec![])
}

fn token_list_result() -> Value {
    result(vec![(
        "tokens",
        true,
        array(result(vec![
            ("id", true, string()),
            ("scope", true, one_of(&["viewer", "doctor", "operator"])),
        ])),
    )])
}

fn plan_list_params() -> Value {
    params(vec![])
}

fn plan_list_result() -> Value {
    result(vec![("plans", true, array(plan()))])
}

/// A delete plan: its digest, its entry, its times, the objects it deletes, the digest of its arguments and the exact
/// statement the owner signs.
fn plan() -> Value {
    result(vec![
        ("plan", true, hex64()),
        ("entry", true, string()),
        ("createdAt", true, int()),
        ("expiresAt", true, int()),
        ("objects", true, array(object_ref())),
        ("paramsDigest", true, hex64()),
        ("statement", true, string()),
    ])
}

/// One Incus object a plan names. `uuid` is null for a kind that has none.
fn object_ref() -> Value {
    result(vec![
        (
            "kind",
            true,
            one_of(&[
                "project", "instance", "network", "acl", "profile", "volume", "zone", "forward",
                "bucket",
            ]),
        ),
        ("project", true, string()),
        ("name", true, string()),
        ("uuid", true, nullable(string())),
    ])
}

fn plan_decline_params() -> Value {
    params(vec![("plan", true, hex64())])
}

fn plan_decline_result() -> Value {
    result(vec![
        ("plan", true, hex64()),
        ("retained", true, array(object_ref())),
    ])
}

fn approval_submit_params() -> Value {
    params(vec![
        ("plan", true, hex64()),
        ("approval", true, string_max(16_384)),
    ])
}

fn approval_submit_result() -> Value {
    result(vec![
        ("plan", true, hex64()),
        ("deleted", true, array(object_ref())),
        ("drift", true, array(object_ref())),
        ("partial", true, boolean()),
    ])
}
