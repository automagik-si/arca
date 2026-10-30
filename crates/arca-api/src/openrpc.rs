//! The OpenRPC document, generated from the verb table. The file `schema/arca.openrpc.json` is this document, pretty and
//! newline-terminated.
//!
//! Each method lists its named parameters as Content Descriptors, required ones first, then the optional ones, each in name
//! order. The one result is a Content Descriptor named `result`. Every method carries three extensions: `x-arca-class`,
//! `x-arca-scope` and `x-arca-writes`. `x-arca-additional-properties` records whether the params object is closed, which the
//! table makes true for every verb.

use serde_json::{Value, json};
use std::collections::BTreeSet;

use crate::API_VERSION;
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
        "methods": methods
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
/// a params list, a result, and the three extensions with a legal value. A method without a class is refused, so a verb
/// cannot reach the document unclassed. Method names are unique, and so are the parameter names of a method, and every
/// parameter is named: the compatibility checker compares one entry per name, so a repeat would go unread.
pub fn check_document(doc: &Value) -> Result<(), String> {
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
        if m.get("x-arca-writes").and_then(Value::as_bool).is_none() {
            return Err(format!("method {name} has no x-arca-writes flag"));
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
        }
        if m.pointer("/result/schema").is_none() {
            return Err(format!("method {name} has no result schema"));
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
}
