//! The OpenRPC document, generated from the verb table. The file `schema/arca.openrpc.json` is this document, pretty and
//! newline-terminated.
//!
//! Each method lists its named parameters as Content Descriptors, required ones first, then the optional ones, each in name
//! order. The one result is a Content Descriptor named `result`. Every method carries four extensions: `x-arca-class`,
//! `x-arca-scope`, `x-arca-writes` and `x-arca-read-only-hint`. `x-arca-writes` says whether the verb changes desired
//! state; `x-arca-read-only-hint` is the `readOnlyHint` the MCP door serves for it ([`Verb::read_only_hint`]). The two are
//! separate because a verb can change no desired state and still not be read-only. docs/OPERATING.md ("The OpenRPC
//! method flags") says what each means to a client. `x-arca-additional-properties` records whether the params object is
//! closed, which the table makes true for every verb.
//!
//! The document's own `x-arca-refusals` lists the refusals that carry fields beside their code ([`crate::refusals`]): one
//! entry per code, with a summary and the schema of the error's `data`. It is not every code a verb answers.

use serde_json::{Value, json};
use std::collections::BTreeSet;

use crate::API_VERSION;
use crate::check::check_schema;
use crate::verbs::{Verb, verbs};

/// The OpenRPC specification version of the document.
pub const OPENRPC_VERSION: &str = "1.3.2";

/// The OpenRPC document of the verb table.
pub fn openrpc() -> Value {
    let methods: Vec<Value> = verbs().iter().map(method).collect();
    json!({
        "openrpc": OPENRPC_VERSION,
        "info": {
            "title": "Arca API",
            "version": API_VERSION,
            "license": { "name": "Apache-2.0" }
        },
        "methods": methods,
        "x-arca-refusals": crate::refusals::document()
    })
}

fn method(v: &Verb) -> Value {
    let params = (v.params)();
    let closed = params.get("additionalProperties") == Some(&Value::Bool(false));
    json!({
        "name": v.name,
        "summary": v.summary,
        "params": descriptors(&params),
        "result": { "name": "result", "schema": (v.result)() },
        "x-arca-class": v.class.as_str(),
        "x-arca-scope": v.min_scope.as_str(),
        "x-arca-writes": v.writes,
        "x-arca-read-only-hint": v.read_only_hint(),
        "x-arca-additional-properties": closed
    })
}

/// One Content Descriptor per property of a params object: required properties first, then the rest, each in name order.
fn descriptors(params: &Value) -> Vec<Value> {
    let Some(props) = params.get("properties").and_then(Value::as_object) else {
        return Vec::new();
    };
    let required: Vec<&str> = params
        .get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut out = Vec::with_capacity(props.len());
    for want_required in [true, false] {
        for (name, schema) in props {
            let is_required = required.contains(&name.as_str());
            if is_required == want_required {
                out.push(json!({ "name": name, "required": is_required, "schema": schema }));
            }
        }
    }
    out
}

/// Whether a document has the shape the verb table promises: the version, the info version, and for every method a name,
/// a params list, a result, and the four extensions with a legal value. A method without a class is refused, so a verb
/// cannot reach the document unclassed. A method that writes and also claims to be read-only is refused. Method names are unique, and so are the parameter names of a method, and every
/// parameter is named: the compatibility checker compares one entry per name, so a repeat would go unread. Every
/// parameter has a schema, and every parameter and result schema is well formed in the subset `check::conforms` reads.
/// The compatibility checker reads a keyword only where both sides hold a value of its type, so a malformed one would
/// pass unread.
pub fn check_document(doc: &Value) -> Result<(), String> {
    check(doc, true)
}

/// [`check_document`] for a released document. A document released before `x-arca-read-only-hint` existed does not
/// carry it and is frozen, so the key may be absent here. Where it is present it must be legal.
pub fn check_released_document(doc: &Value) -> Result<(), String> {
    check(doc, false)
}

fn check(doc: &Value, hint_required: bool) -> Result<(), String> {
    if doc.get("openrpc").and_then(Value::as_str) != Some(OPENRPC_VERSION) {
        return Err(format!("openrpc must be {OPENRPC_VERSION}"));
    }
    let version = doc.pointer("/info/version").and_then(Value::as_str);
    if version != Some(API_VERSION) {
        return Err(format!("info.version must be {API_VERSION}"));
    }
    let methods = doc
        .get("methods")
        .and_then(Value::as_array)
        .ok_or_else(|| "methods must be an array".to_string())?;
    let mut names = BTreeSet::new();
    for m in methods {
        let name = m
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| "a method has no name".to_string())?;
        if !names.insert(name) {
            return Err(format!("method {name} is listed twice"));
        }
        let class = m.get("x-arca-class").and_then(Value::as_str);
        if !matches!(class, Some("L" | "B" | "Q")) {
            return Err(format!("method {name} has no legal x-arca-class"));
        }
        let scope = m.get("x-arca-scope").and_then(Value::as_str);
        if !matches!(scope, Some("viewer" | "doctor" | "operator")) {
            return Err(format!("method {name} has no legal x-arca-scope"));
        }
        let Some(writes) = m.get("x-arca-writes").and_then(Value::as_bool) else {
            return Err(format!("method {name} has no x-arca-writes flag"));
        };
        match m.get("x-arca-read-only-hint") {
            None if !hint_required => {}
            Some(Value::Bool(true)) if writes => {
                return Err(format!(
                    "method {name} writes, so x-arca-read-only-hint must be false"
                ));
            }
            Some(Value::Bool(_)) => {}
            _ => return Err(format!("method {name} has no x-arca-read-only-hint flag")),
        }
        // The flag records that the params object is closed: `true` is the only legal value.
        if m.get("x-arca-additional-properties") != Some(&Value::Bool(true)) {
            return Err(format!(
                "method {name} is open: x-arca-additional-properties must be true"
            ));
        }
        let Some(params) = m.get("params").and_then(Value::as_array) else {
            return Err(format!("method {name} has no params list"));
        };
        let mut param_names = BTreeSet::new();
        for p in params {
            let param = p
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("method {name} has a parameter with no name"))?;
            if !param_names.insert(param) {
                return Err(format!("method {name} lists parameter {param} twice"));
            }
            let schema = p
                .get("schema")
                .ok_or_else(|| format!("method {name} parameter {param} has no schema"))?;
            check_schema(schema, "").map_err(|e| {
                format!("method {name} parameter {param} has a malformed schema: {e}")
            })?;
        }
        let result = m
            .pointer("/result/schema")
            .ok_or_else(|| format!("method {name} has no result schema"))?;
        check_schema(result, "")
            .map_err(|e| format!("method {name} result has a malformed schema: {e}"))?;
    }
    check_refusals(doc, hint_required)
}

/// The `x-arca-refusals` list: every entry has a code no other entry has, a summary, and a well-formed `data` schema
/// that requires `refusal` and pins it to the entry's code. A document released before the list existed does not carry
/// it and is frozen, so the list may be absent there.
fn check_refusals(doc: &Value, list_required: bool) -> Result<(), String> {
    let list = match doc.get("x-arca-refusals") {
        None if !list_required => return Ok(()),
        Some(Value::Array(list)) => list,
        _ => return Err("x-arca-refusals must be an array".to_string()),
    };
    let mut codes = BTreeSet::new();
    for entry in list {
        let code = entry
            .get("code")
            .and_then(Value::as_str)
            .filter(|code| !code.is_empty())
            .ok_or_else(|| "a refusal has no code".to_string())?;
        if !codes.insert(code) {
            return Err(format!("refusal {code} is listed twice"));
        }
        if entry.get("summary").and_then(Value::as_str).is_none() {
            return Err(format!("refusal {code} has no summary"));
        }
        let data = entry
            .get("data")
            .ok_or_else(|| format!("refusal {code} has no data schema"))?;
        check_schema(data, "")
            .map_err(|e| format!("refusal {code} has a malformed data schema: {e}"))?;
        let names_itself = data
            .pointer("/properties/refusal/const")
            .and_then(Value::as_str)
            == Some(code)
            && data
                .get("required")
                .and_then(Value::as_array)
                .is_some_and(|required| required.iter().any(|name| name == "refusal"));
        if !names_itself {
            return Err(format!(
                "refusal {code}: its data schema must require refusal and pin it to the code"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_generated_document_passes_its_own_check() {
        assert_eq!(check_document(&openrpc()), Ok(()));
    }

    #[test]
    fn the_refusal_list_is_required_and_every_entry_names_its_own_code() {
        let mut doc = openrpc();
        doc.as_object_mut()
            .expect("document")
            .remove("x-arca-refusals");
        assert!(
            check_document(&doc).is_err(),
            "a current document lists them"
        );
        assert_eq!(
            check_released_document(&doc),
            Ok(()),
            "a document released before the list has none"
        );

        let mut doc = openrpc();
        doc["x-arca-refusals"][1]["code"] = doc["x-arca-refusals"][0]["code"].clone();
        assert!(check_document(&doc).is_err(), "a code listed twice");

        let mut doc = openrpc();
        doc["x-arca-refusals"][0]["data"]["properties"]["refusal"] = json!({ "const": "refused" });
        assert!(
            check_document(&doc).is_err(),
            "a data schema for another code"
        );

        let mut doc = openrpc();
        doc["x-arca-refusals"][0]["data"]["format"] = json!("x");
        assert!(
            check_document(&doc).is_err(),
            "a keyword outside the subset"
        );
    }

    #[test]
    fn a_closed_params_object_has_one_descriptor_per_property_required_first() {
        let schema = json!({
            "type": "object",
            "properties": { "b": { "type": "integer" }, "a": { "type": "string" } },
            "required": ["a"],
            "additionalProperties": false
        });
        let got = descriptors(&schema);
        let names: Vec<&str> = got.iter().filter_map(|d| d["name"].as_str()).collect();
        assert_eq!(names, ["a", "b"], "the required one leads");
        assert_eq!(got[0]["required"], json!(true));
        assert_eq!(got[1]["required"], json!(false));
    }

    #[test]
    fn a_method_without_a_class_is_refused() {
        let mut doc = openrpc();
        doc["methods"][0]
            .as_object_mut()
            .expect("a method is an object")
            .remove("x-arca-class");
        assert!(check_document(&doc).is_err());
    }

    #[test]
    fn a_method_without_a_read_only_hint_is_refused_unless_it_was_released_before_the_hint() {
        let mut doc = openrpc();
        doc["methods"][0]
            .as_object_mut()
            .expect("a method is an object")
            .remove("x-arca-read-only-hint");
        assert!(check_document(&doc).is_err());
        assert_eq!(check_released_document(&doc), Ok(()));
        doc["methods"][0]["x-arca-read-only-hint"] = json!("yes");
        assert!(
            check_released_document(&doc).is_err(),
            "a present hint is a boolean"
        );
    }

    #[test]
    fn a_writing_method_that_claims_read_only_is_refused() {
        let mut doc = openrpc();
        doc["methods"][0]["x-arca-writes"] = json!(true);
        doc["methods"][0]["x-arca-read-only-hint"] = json!(true);
        assert!(check_document(&doc).is_err());
        assert!(check_released_document(&doc).is_err());
    }
}
