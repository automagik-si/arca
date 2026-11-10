//! The OpenRPC compatibility checker against the released 0.1.0 document. Each break fixture is the released document with
//! one edit and yields exactly one break. The additive fixture yields none. Two fixtures start from a disclosed base instead:
//! the minLength fixture and the shrunk-enum test add one optional string parameter, because no released parameter carries
//! a string bound, and the frozen `minutes` has no enum to shrink. A closed-result fixture closes the base's result object,
//! because every released result is open.

use arca_api::Scope;
use arca_api::compat::{Break, compatible};
use arca_api::openrpc::{check_document, check_released_document, openrpc};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const RELEASED_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../schema/released");
const RELEASED: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../schema/released/0.1.0.openrpc.json"
);

fn released() -> Value {
    let bytes = std::fs::read(RELEASED).expect("the released schema exists");
    serde_json::from_slice(&bytes).expect("the released schema is JSON")
}

fn method_mut<'a>(doc: &'a mut Value, name: &str) -> &'a mut Value {
    doc["methods"]
        .as_array_mut()
        .expect("methods is an array")
        .iter_mut()
        .find(|m| m["name"] == name)
        .expect("the method is in the document")
}

fn params_mut<'a>(doc: &'a mut Value, method: &str) -> &'a mut Vec<Value> {
    method_mut(doc, method)["params"]
        .as_array_mut()
        .expect("params is an array")
}

fn param_mut<'a>(doc: &'a mut Value, method: &str, param: &str) -> &'a mut Value {
    params_mut(doc, method)
        .iter_mut()
        .find(|p| p["name"] == param)
        .expect("the parameter is in the document")
}

fn result_mut<'a>(doc: &'a mut Value, method: &str) -> &'a mut Value {
    &mut method_mut(doc, method)["result"]["schema"]
}

/// The breaks of the released document after one edit.
fn breaks_after(edit: impl FnOnce(&mut Value)) -> Vec<Break> {
    let base = released();
    let mut current = base.clone();
    edit(&mut current);
    compatible(&base, &current)
}

/// The released document plus one optional `sample` parameter on `node_metrics`, with the given schema. It is an addition,
/// so the released document is compatible with it.
fn with_sample(schema: Value) -> Value {
    let mut doc = released();
    params_mut(&mut doc, "node_metrics").push(json!({
        "name": "sample",
        "required": false,
        "schema": schema
    }));
    assert!(
        compatible(&released(), &doc).is_empty(),
        "the base with one optional parameter is an addition"
    );
    doc
}

/// The base for the enum and minLength fixtures: an optional string parameter with an enum and a minimum length.
fn probed() -> Value {
    with_sample(json!({
        "type": "string",
        "enum": ["alpha", "beta", "gamma"],
        "minLength": 1
    }))
}

fn scratch_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the clock is after the epoch")
        .as_nanos();
    let dir =
        std::env::temp_dir().join(format!("arca-compat-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch directory can be created");
    dir
}

/// Checks every released document under `dir` against `current`. Returns how many it checked. An empty directory is an
/// error, so the check cannot pass by checking nothing. The current document must first pass `check_document`, the shape
/// the compatibility check reads; a released one `check_released_document`, which lets it predate the read-only hint.
fn check_released_dir(dir: &Path, current: &Value) -> Result<usize, String> {
    check_document(current).map_err(|e| format!("the current document: {e}"))?;
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    if files.is_empty() {
        return Err(format!("no released document under {}", dir.display()));
    }
    for file in &files {
        let bytes = std::fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
        let released: Value =
            serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", file.display()))?;
        check_released_document(&released).map_err(|e| format!("{}: {e}", file.display()))?;
        let breaks = compatible(&released, current);
        if !breaks.is_empty() {
            let named: Vec<String> = breaks.iter().map(ToString::to_string).collect();
            return Err(format!("{}: {}", file.display(), named.join("; ")));
        }
    }
    Ok(files.len())
}

// The failing fixtures: twelve break variants, with ParamNarrowed covered three times.

#[test]
fn a_removed_method_is_one_break() {
    let got = breaks_after(|doc| {
        doc["methods"]
            .as_array_mut()
            .expect("methods")
            .retain(|m| m["name"] != "node_metrics");
    });
    assert_eq!(
        got,
        [Break::MethodRemoved {
            method: "node_metrics".into()
        }]
    );
}

#[test]
fn a_removed_parameter_is_one_break() {
    let got =
        breaks_after(|doc| params_mut(doc, "node_metrics").retain(|p| p["name"] != "minutes"));
    assert_eq!(
        got,
        [Break::ParamRemoved {
            method: "node_metrics".into(),
            param: "minutes".into()
        }]
    );
}

#[test]
fn a_parameter_made_required_is_one_break() {
    let got =
        breaks_after(|doc| param_mut(doc, "node_metrics", "minutes")["required"] = json!(true));
    assert_eq!(
        got,
        [Break::ParamNowRequired {
            method: "node_metrics".into(),
            param: "minutes".into()
        }]
    );
}

#[test]
fn a_new_required_parameter_is_one_break() {
    let got = breaks_after(|doc| {
        params_mut(doc, "node_metrics").push(json!({
            "name": "scope",
            "required": true,
            "schema": { "type": "string" }
        }));
    });
    assert_eq!(
        got,
        [Break::NewRequiredParam {
            method: "node_metrics".into(),
            param: "scope".into()
        }]
    );
}

#[test]
fn a_lowered_maximum_is_one_parameter_narrowing() {
    let got = breaks_after(|doc| {
        param_mut(doc, "node_metrics", "minutes")["schema"]["maximum"] = json!(30)
    });
    assert_eq!(
        got,
        [Break::ParamNarrowed {
            method: "node_metrics".into(),
            param: "minutes".into(),
            why: "maximum lowered".into()
        }]
    );
}

#[test]
fn an_enum_added_to_a_released_parameter_is_one_narrowing() {
    let got = breaks_after(|doc| {
        param_mut(doc, "node_metrics", "minutes")["schema"]["enum"] = json!([1, 5, 15, 60]);
    });
    assert_eq!(
        got,
        [Break::ParamNarrowed {
            method: "node_metrics".into(),
            param: "minutes".into(),
            why: "enum added".into()
        }]
    );
}

/// Disclosed base: the frozen 0.1.0 has no enum on a parameter, so a shrink is reached from the probed base.
#[test]
fn a_shrunk_enum_on_an_added_parameter_is_one_narrowing() {
    let base = probed();
    let mut current = base.clone();
    param_mut(&mut current, "node_metrics", "sample")["schema"]["enum"] = json!(["alpha", "beta"]);
    assert_eq!(
        compatible(&base, &current),
        [Break::ParamNarrowed {
            method: "node_metrics".into(),
            param: "sample".into(),
            why: "enum shrunk".into()
        }]
    );
}

#[test]
fn a_raised_min_length_is_one_parameter_narrowing() {
    let base = probed();
    let mut current = base.clone();
    param_mut(&mut current, "node_metrics", "sample")["schema"]["minLength"] = json!(2);
    assert_eq!(
        compatible(&base, &current),
        [Break::ParamNarrowed {
            method: "node_metrics".into(),
            param: "sample".into(),
            why: "minLength raised".into()
        }]
    );
}

#[test]
fn a_removed_result_field_is_one_break() {
    let got = breaks_after(|doc| {
        let schema = result_mut(doc, "arca_status");
        schema["properties"]
            .as_object_mut()
            .expect("properties")
            .remove("uptimeS");
        schema["required"]
            .as_array_mut()
            .expect("required")
            .retain(|v| v.as_str() != Some("uptimeS"));
    });
    assert_eq!(
        got,
        [Break::ResultFieldRemoved {
            method: "arca_status".into(),
            path: "$.uptimeS".into()
        }]
    );
}

#[test]
fn a_closed_result_object_is_one_break() {
    let got = breaks_after(|doc| {
        result_mut(doc, "arca_status")["additionalProperties"] = json!(false);
    });
    assert_eq!(
        got,
        [Break::ResultClosed {
            method: "arca_status".into(),
            path: "$".into()
        }]
    );
}

#[test]
fn a_changed_result_type_is_one_break() {
    let got = breaks_after(|doc| {
        result_mut(doc, "arca_status")["properties"]["uptimeS"]["type"] = json!("string");
    });
    assert_eq!(
        got,
        [Break::ResultTypeChanged {
            method: "arca_status".into(),
            path: "$.uptimeS".into()
        }]
    );
}

#[test]
fn a_changed_class_is_one_break() {
    let got = breaks_after(|doc| method_mut(doc, "node_metrics")["x-arca-class"] = json!("B"));
    assert_eq!(
        got,
        [Break::ClassChanged {
            method: "node_metrics".into()
        }]
    );
}

#[test]
fn a_raised_scope_is_one_break() {
    let got =
        breaks_after(|doc| method_mut(doc, "arca_status")["x-arca-scope"] = json!("operator"));
    assert_eq!(
        got,
        [Break::ScopeRaised {
            method: "arca_status".into()
        }]
    );
}

#[test]
fn writes_turned_on_is_one_break() {
    let got = breaks_after(|doc| method_mut(doc, "arca_status")["x-arca-writes"] = json!(true));
    assert_eq!(
        got,
        [Break::WritesTurnedOn {
            method: "arca_status".into()
        }]
    );
}

/// The released document with the current read-only hint added to every method it shares with the current document, as a
/// release made after the hint would carry it.
fn released_with_hints() -> Value {
    let current = openrpc();
    let mut doc = released();
    for m in doc["methods"].as_array_mut().expect("methods") {
        let hint = current["methods"]
            .as_array()
            .expect("methods")
            .iter()
            .find(|c| c["name"] == m["name"])
            .map(|c| c["x-arca-read-only-hint"].clone())
            .expect("every released method is current");
        m["x-arca-read-only-hint"] = hint;
    }
    assert_eq!(
        check_document(&doc),
        Ok(()),
        "the hinted release is well formed"
    );
    doc
}

#[test]
fn a_read_only_hint_flip_either_way_is_one_break() {
    for (from, to) in [(true, false), (false, true)] {
        let mut base = released_with_hints();
        method_mut(&mut base, "arca_status")["x-arca-read-only-hint"] = json!(from);
        let mut current = base.clone();
        method_mut(&mut current, "arca_status")["x-arca-read-only-hint"] = json!(to);
        assert_eq!(
            compatible(&base, &current),
            [Break::ReadOnlyHintChanged {
                method: "arca_status".into()
            }],
            "{from} to {to}"
        );
    }
}

#[test]
fn a_removed_read_only_hint_is_one_break() {
    let base = released_with_hints();
    let mut current = base.clone();
    method_mut(&mut current, "arca_status")
        .as_object_mut()
        .expect("a method is an object")
        .remove("x-arca-read-only-hint");
    assert_eq!(
        compatible(&base, &current),
        [Break::ReadOnlyHintChanged {
            method: "arca_status".into()
        }]
    );
}

#[test]
fn a_release_without_read_only_hints_compares_none() {
    let released = released();
    assert!(
        released["methods"]
            .as_array()
            .expect("methods")
            .iter()
            .all(|m| m.get("x-arca-read-only-hint").is_none()),
        "the 0.1.0 document predates the hint"
    );
    assert_eq!(check_released_document(&released), Ok(()));
    assert!(
        check_document(&released).is_err(),
        "a current document must carry the hint"
    );
    assert!(compatible(&released, &openrpc()).is_empty());
}

// Keywords outside the subset and schemas that are not objects. The module cannot relate them to the released schema, so
// any difference is a break, and the first differing keyword is named.

fn minutes_narrowed(why: &str) -> Break {
    Break::ParamNarrowed {
        method: "node_metrics".into(),
        param: "minutes".into(),
        why: why.into(),
    }
}

#[test]
fn a_parameter_schema_of_false_is_one_narrowing() {
    let got =
        breaks_after(|doc| param_mut(doc, "node_metrics", "minutes")["schema"] = json!(false));
    assert_eq!(got, [minutes_narrowed("schema is not an object")]);
}

#[test]
fn a_parameter_schema_that_is_a_string_is_one_narrowing() {
    let got =
        breaks_after(|doc| param_mut(doc, "node_metrics", "minutes")["schema"] = json!("integer"));
    assert_eq!(got, [minutes_narrowed("schema is not an object")]);
}

#[test]
fn an_exclusive_maximum_added_is_one_parameter_narrowing() {
    let got = breaks_after(|doc| {
        param_mut(doc, "node_metrics", "minutes")["schema"]["exclusiveMaximum"] = json!(30)
    });
    assert_eq!(got, [minutes_narrowed("exclusiveMaximum changed")]);
}

#[test]
fn a_multiple_of_added_is_one_parameter_narrowing() {
    let got = breaks_after(|doc| {
        param_mut(doc, "node_metrics", "minutes")["schema"]["multipleOf"] = json!(5)
    });
    assert_eq!(got, [minutes_narrowed("multipleOf changed")]);
}

#[test]
fn a_pattern_added_is_one_parameter_narrowing() {
    let got = breaks_after(|doc| {
        param_mut(doc, "node_metrics", "minutes")["schema"]["pattern"] = json!("^[0-9]+$")
    });
    assert_eq!(got, [minutes_narrowed("pattern changed")]);
}

/// `propertyNames` and an `additionalProperties` schema are checked by `check.rs` but not modelled here: a change is a break.
#[test]
fn a_property_names_or_additional_properties_schema_added_is_one_parameter_narrowing() {
    let got = breaks_after(|doc| {
        param_mut(doc, "node_metrics", "minutes")["schema"]["propertyNames"] =
            json!({ "maxLength": 4 })
    });
    assert_eq!(got, [minutes_narrowed("propertyNames changed")]);
    let got = breaks_after(|doc| {
        param_mut(doc, "node_metrics", "minutes")["schema"]["additionalProperties"] =
            json!({ "type": "integer" })
    });
    assert_eq!(got, [minutes_narrowed("additionalProperties changed")]);
}

/// The reviewer's case: a released result object whose unnamed fields must be integers gains a named string field. The
/// released schema refused that field, so the current result is broader: one break, at the object.
#[test]
fn a_result_object_with_an_additional_properties_schema_that_names_a_new_field_is_one_break() {
    let doc = |schema: Value| json!({ "methods": [{ "name": "m", "params": [], "result": { "name": "result", "schema": schema } }] });
    let old = json!({ "type": "object", "additionalProperties": { "type": "integer" } });
    let mut new = old.clone();
    new["properties"] = json!({ "x": { "type": "string" } });
    assert_eq!(
        compatible(&doc(old.clone()), &doc(new)),
        [Break::ResultTypeChanged {
            method: "m".into(),
            path: "$".into()
        }]
    );
    let names = json!({ "type": "object", "propertyNames": { "maxLength": 2 } });
    let mut named = names.clone();
    named["properties"] = json!({ "longer": { "type": "string" } });
    assert_eq!(
        compatible(&doc(names), &doc(named)).len(),
        1,
        "propertyNames too"
    );
    assert!(
        compatible(&doc(old.clone()), &doc(old)).is_empty(),
        "unchanged is no break"
    );
}

/// The released result admits any array length. A `maxItems` the module cannot relate is refused, even though it narrows.
#[test]
fn a_result_keyword_outside_the_subset_is_one_type_break() {
    let got = breaks_after(|doc| {
        result_mut(doc, "watch_report")["properties"]["projects"]["minItems"] = json!(3);
    });
    assert_eq!(
        got,
        [Break::ResultTypeChanged {
            method: "watch_report".into(),
            path: "$.projects".into()
        }]
    );
}

#[test]
fn an_annotation_or_extension_added_to_a_parameter_is_no_break() {
    let got = breaks_after(|doc| {
        let schema = &mut param_mut(doc, "node_metrics", "minutes")["schema"];
        schema["title"] = json!("Minutes");
        schema["description"] = json!("The window in minutes");
        schema["x-note"] = json!("ignored by the checker");
    });
    assert!(
        got.is_empty(),
        "annotations and extensions are ignored: {got:?}"
    );
}

#[test]
fn a_parameter_schema_of_false_fails_the_released_gate() {
    let mut current = openrpc();
    param_mut(&mut current, "node_metrics", "minutes")["schema"] = json!(false);
    let got = check_released_dir(Path::new(RELEASED_DIR), &current);
    let message = got.expect_err("a parameter that admits nothing must fail the gate");
    // The document check refuses a schema that is not an object before the compatibility check reads it, so the refusal
    // names the parameter. The narrowing itself is still reported by `compatible`, in the test above this one.
    assert!(
        message.contains("parameter minutes has a malformed schema"),
        "the refused parameter is named: {message}"
    );
}

// A closed released result gaining a field, and anyOf compared as a value.

#[test]
fn a_closed_result_that_gains_a_field_is_a_type_break() {
    // Disclosed base: the frozen 0.1.0 leaves every result open, so this base closes arca_status's result object.
    let mut base = released();
    result_mut(&mut base, "arca_status")["additionalProperties"] = json!(false);
    let mut current = base.clone();
    result_mut(&mut current, "arca_status")["properties"]["extra"] = json!({ "type": "string" });
    assert_eq!(
        compatible(&base, &current),
        [Break::ResultTypeChanged {
            method: "arca_status".into(),
            path: "$".into()
        }]
    );
}

#[test]
fn an_unchanged_any_of_with_a_narrowed_sibling_names_the_sibling() {
    // Disclosed base: an added optional parameter, as the probed base is.
    let base = with_sample(json!({
        "anyOf": [{ "type": "integer", "minimum": 1 }, { "type": "integer", "maximum": 60 }]
    }));
    let mut current = base.clone();
    param_mut(&mut current, "node_metrics", "sample")["schema"]["minimum"] = json!(2);
    assert_eq!(
        compatible(&base, &current),
        [Break::ParamNarrowed {
            method: "node_metrics".into(),
            param: "sample".into(),
            why: "minimum raised".into()
        }]
    );
}

// Duplicate names: the compared entry is the first one, so a second entry must fail the gate instead. The current document
// is the hinted release, so that the duplicate is its only fault.

#[test]
fn a_duplicate_method_name_fails_the_gate() {
    let mut current = released_with_hints();
    let mut dup = method_mut(&mut current, "arca_status").clone();
    dup["x-arca-scope"] = json!("operator");
    current["methods"]
        .as_array_mut()
        .expect("methods")
        .push(dup);
    assert!(
        check_document(&current).is_err(),
        "a duplicate method is refused"
    );
    let got = check_released_dir(Path::new(RELEASED_DIR), &current);
    let message = got.expect_err("a second arca_status must fail the gate");
    assert!(
        message.contains("arca_status is listed twice"),
        "the duplicate is named: {message}"
    );
}

#[test]
fn a_duplicate_parameter_name_fails_the_gate() {
    let mut current = released_with_hints();
    let mut dup = param_mut(&mut current, "node_metrics", "minutes").clone();
    dup["required"] = json!(true);
    params_mut(&mut current, "node_metrics").push(dup);
    assert!(
        check_document(&current).is_err(),
        "a duplicate parameter is refused"
    );
    let got = check_released_dir(Path::new(RELEASED_DIR), &current);
    let message = got.expect_err("a second minutes must fail the gate");
    assert!(
        message.contains("lists parameter minutes twice"),
        "the duplicate is named: {message}"
    );
}

#[test]
fn a_current_document_that_fails_its_own_check_fails_the_gate() {
    // The open params object is not compared by `compatible`, so only `check_document` refuses this current document.
    let mut current = openrpc();
    method_mut(&mut current, "node_metrics")["x-arca-additional-properties"] = json!(false);
    let got = check_released_dir(Path::new(RELEASED_DIR), &current);
    assert!(
        got.is_err(),
        "a current document that fails check_document must fail the gate"
    );
}

// A supported keyword with a malformed value on the current side. The checker reads a keyword only where both sides
// hold a value of its type, so a malformed value there passes unread. The document check refuses the schema first.

/// The gate refuses the current document, and the refusal names the malformed keyword.
fn assert_gate_refuses(current: &Value, expect: &str) {
    assert!(
        check_document(current).is_err(),
        "the document check refuses {expect}"
    );
    let message = check_released_dir(Path::new(RELEASED_DIR), current)
        .expect_err("a malformed schema must fail the gate");
    assert!(
        message.contains(expect),
        "the malformed keyword is named ({expect}): {message}"
    );
}

#[test]
fn a_malformed_keyword_on_a_current_parameter_fails_the_gate() {
    let cases: [(&str, Value); 6] = [
        ("enum", json!("x")),
        ("minimum", json!("1")),
        ("minLength", json!(-1)),
        ("required", json!("minutes")),
        ("properties", json!("x")),
        ("additionalProperties", json!("no")),
    ];
    for (keyword, value) in cases {
        let mut current = openrpc();
        param_mut(&mut current, "node_metrics", "minutes")["schema"][keyword] = value;
        assert_gate_refuses(&current, keyword);
    }
}

#[test]
fn a_malformed_keyword_on_a_current_result_field_fails_the_gate() {
    let mut current = openrpc();
    result_mut(&mut current, "arca_status")["properties"]["version"]["maxLength"] = json!("64");
    assert_gate_refuses(&current, "maxLength");
}

#[test]
fn a_parameter_without_a_schema_fails_the_gate() {
    let mut current = openrpc();
    param_mut(&mut current, "node_metrics", "minutes")
        .as_object_mut()
        .expect("a parameter is an object")
        .remove("schema");
    assert_gate_refuses(&current, "schema");
}

// The passing fixture and the additions and widenings that must stay silent.

#[test]
fn an_added_method_and_an_optional_parameter_are_no_break() {
    let got = breaks_after(|doc| {
        let mut added = method_mut(doc, "arca_status").clone();
        added["name"] = json!("arca_version");
        doc["methods"].as_array_mut().expect("methods").push(added);
        params_mut(doc, "node_metrics").push(json!({
            "name": "window",
            "required": false,
            "schema": { "type": "integer", "minimum": 1, "maximum": 60 }
        }));
    });
    assert!(got.is_empty(), "additions are not breaks: {got:?}");
}

#[test]
fn the_released_document_is_compatible_with_itself() {
    assert!(compatible(&released(), &released()).is_empty());
}

#[test]
fn a_new_result_field_required_or_optional_is_no_break() {
    let got = breaks_after(|doc| {
        let schema = result_mut(doc, "arca_status");
        schema["properties"]["extraOptional"] = json!({ "type": "string" });
        schema["properties"]["extraRequired"] = json!({ "type": "string" });
        schema["required"]
            .as_array_mut()
            .expect("required")
            .push(json!("extraRequired"));
    });
    assert!(got.is_empty(), "result additions are not breaks: {got:?}");
}

#[test]
fn a_widened_parameter_is_no_break() {
    let got = breaks_after(|doc| {
        let schema = &mut param_mut(doc, "node_metrics", "minutes")["schema"];
        schema["type"] = json!("number");
        schema["minimum"] = json!(0);
        schema["maximum"] = json!(120);
    });
    assert!(got.is_empty(), "a wider parameter is not a break: {got:?}");
}

#[test]
fn a_narrowed_result_type_is_no_break() {
    let got = breaks_after(|doc| {
        result_mut(doc, "arca_status")["properties"]["appliedSeq"]["type"] = json!("integer");
    });
    assert!(
        got.is_empty(),
        "a result that admits less is not a break: {got:?}"
    );
}

#[test]
fn a_method_whose_scope_is_lowered_is_no_break() {
    let got = breaks_after(|doc| method_mut(doc, "token_list")["x-arca-scope"] = json!("doctor"));
    assert!(got.is_empty(), "a lower scope is not a break: {got:?}");
}

#[test]
fn the_scope_rule_follows_the_scope_order_of_the_table() {
    let order = [Scope::Viewer, Scope::Doctor, Scope::Operator];
    for old in order {
        for new in order {
            let mut base = released();
            method_mut(&mut base, "arca_status")["x-arca-scope"] = json!(old.as_str());
            let mut current = base.clone();
            method_mut(&mut current, "arca_status")["x-arca-scope"] = json!(new.as_str());
            let expect = if new > old {
                vec![Break::ScopeRaised {
                    method: "arca_status".into(),
                }]
            } else {
                Vec::new()
            };
            assert_eq!(compatible(&base, &current), expect, "{old:?} to {new:?}");
        }
    }
}

// Breaks that the subset makes visible below the top level, and the paths that name them.

#[test]
fn a_nested_result_field_is_named_by_its_path() {
    let got = breaks_after(|doc| {
        let cpu = &mut result_mut(doc, "node_metrics")["properties"]["latest"]["properties"]["cpu"];
        cpu["properties"]
            .as_object_mut()
            .expect("properties")
            .remove("usedPct");
        cpu["required"]
            .as_array_mut()
            .expect("required")
            .retain(|v| v.as_str() != Some("usedPct"));
    });
    assert_eq!(
        got,
        [Break::ResultFieldRemoved {
            method: "node_metrics".into(),
            path: "$.latest.cpu.usedPct".into()
        }]
    );
}

#[test]
fn an_item_field_is_named_with_brackets() {
    let got = breaks_after(|doc| {
        let item = &mut result_mut(doc, "watch_report")["properties"]["projects"]["items"]["properties"]
            ["instances"]["items"];
        item["properties"]
            .as_object_mut()
            .expect("properties")
            .remove("name");
        item["required"]
            .as_array_mut()
            .expect("required")
            .retain(|v| v.as_str() != Some("name"));
    });
    assert_eq!(
        got,
        [Break::ResultFieldRemoved {
            method: "watch_report".into(),
            path: "$.projects[].instances[].name".into()
        }]
    );
}

#[test]
fn a_result_field_made_optional_is_a_type_break_at_its_path() {
    let got = breaks_after(|doc| {
        result_mut(doc, "arca_status")["required"]
            .as_array_mut()
            .expect("required")
            .retain(|v| v.as_str() != Some("uptimeS"));
    });
    assert_eq!(
        got,
        [Break::ResultTypeChanged {
            method: "arca_status".into(),
            path: "$.uptimeS".into()
        }]
    );
}

#[test]
fn a_result_type_widened_to_null_is_a_break() {
    let got = breaks_after(|doc| {
        result_mut(doc, "arca_status")["properties"]["uptimeS"]["type"] =
            json!(["integer", "null"]);
    });
    assert_eq!(
        got,
        [Break::ResultTypeChanged {
            method: "arca_status".into(),
            path: "$.uptimeS".into()
        }]
    );
}

#[test]
fn a_result_enum_that_gains_a_value_is_a_break() {
    let got = breaks_after(|doc| {
        result_mut(doc, "arca_status")["properties"]["mode"]["enum"]
            .as_array_mut()
            .expect("enum")
            .push(json!("fleet"));
    });
    assert_eq!(
        got,
        [Break::ResultTypeChanged {
            method: "arca_status".into(),
            path: "$.mode".into()
        }]
    );
}

#[test]
fn a_parameter_whose_any_of_changes_is_a_narrowing_fail_closed() {
    let base = probed();
    let mut current = base.clone();
    param_mut(&mut current, "node_metrics", "sample")["schema"] = json!({
        "anyOf": [{ "type": "string" }, { "type": "integer" }]
    });
    assert_eq!(
        compatible(&base, &current),
        [Break::ParamNarrowed {
            method: "node_metrics".into(),
            param: "sample".into(),
            why: "anyOf changed".into()
        }]
    );
}

#[test]
fn an_unchanged_any_of_is_no_break() {
    let base = with_sample(json!({ "anyOf": [{ "type": "string" }, { "type": "integer" }] }));
    assert!(compatible(&base, &base).is_empty());
}

#[test]
fn a_result_array_whose_items_are_dropped_is_a_type_break() {
    let got = breaks_after(|doc| {
        result_mut(doc, "watch_report")["properties"]["projects"]
            .as_object_mut()
            .expect("the projects schema is an object")
            .remove("items");
    });
    assert_eq!(
        got,
        [Break::ResultTypeChanged {
            method: "watch_report".into(),
            path: "$.projects[]".into()
        }]
    );
}

#[test]
fn a_result_that_admits_fields_the_released_one_refused_is_a_type_break() {
    let mut base = released();
    result_mut(&mut base, "arca_status")["additionalProperties"] = json!(false);
    let mut current = base.clone();
    result_mut(&mut current, "arca_status")["additionalProperties"] = json!(true);
    assert_eq!(
        compatible(&base, &current),
        [Break::ResultTypeChanged {
            method: "arca_status".into(),
            path: "$".into()
        }]
    );
}

#[test]
fn a_nested_parameter_property_made_required_is_a_narrowing() {
    let base = with_sample(json!({
        "type": "object",
        "properties": { "a": { "type": "integer" } }
    }));
    let mut current = base.clone();
    param_mut(&mut current, "node_metrics", "sample")["schema"]["required"] = json!(["a"]);
    assert_eq!(
        compatible(&base, &current),
        [Break::ParamNarrowed {
            method: "node_metrics".into(),
            param: "sample".into(),
            why: "property a made required".into()
        }]
    );
}

#[test]
fn a_nested_parameter_closed_to_its_named_properties_is_a_narrowing() {
    let base = with_sample(json!({
        "type": "object",
        "properties": { "a": { "type": "integer" } }
    }));
    let mut current = base.clone();
    param_mut(&mut current, "node_metrics", "sample")["schema"]["additionalProperties"] =
        json!(false);
    assert_eq!(
        compatible(&base, &current),
        [Break::ParamNarrowed {
            method: "node_metrics".into(),
            param: "sample".into(),
            why: "additional properties refused".into()
        }]
    );
}

#[test]
fn a_nested_parameter_property_dropped_from_a_closed_object_is_a_narrowing() {
    let base = with_sample(json!({
        "type": "object",
        "properties": { "a": { "type": "integer" }, "b": { "type": "string" } },
        "additionalProperties": false
    }));
    let mut current = base.clone();
    param_mut(&mut current, "node_metrics", "sample")["schema"]["properties"]
        .as_object_mut()
        .expect("properties")
        .remove("b");
    assert_eq!(
        compatible(&base, &current),
        [Break::ParamNarrowed {
            method: "node_metrics".into(),
            param: "sample".into(),
            why: "property b refused".into()
        }]
    );
}

#[test]
fn a_nested_parameter_that_opens_its_object_is_no_break() {
    let base = with_sample(json!({
        "type": "object",
        "properties": { "a": { "type": "integer" } },
        "additionalProperties": false
    }));
    let mut current = base.clone();
    param_mut(&mut current, "node_metrics", "sample")["schema"]["additionalProperties"] =
        json!(true);
    assert!(compatible(&base, &current).is_empty());
}

#[test]
fn a_parameter_array_item_bound_lowered_is_a_narrowing_at_its_items() {
    let base =
        with_sample(json!({ "type": "array", "items": { "type": "integer", "maximum": 10 } }));
    let mut current = base.clone();
    param_mut(&mut current, "node_metrics", "sample")["schema"]["items"]["maximum"] = json!(5);
    assert_eq!(
        compatible(&base, &current),
        [Break::ParamNarrowed {
            method: "node_metrics".into(),
            param: "sample".into(),
            why: "items: maximum lowered".into()
        }]
    );
}

#[test]
fn a_parameter_array_that_gains_item_constraints_is_a_narrowing() {
    let base = with_sample(json!({ "type": "array" }));
    let mut current = base.clone();
    param_mut(&mut current, "node_metrics", "sample")["schema"]["items"] =
        json!({ "type": "integer" });
    assert_eq!(
        compatible(&base, &current),
        [Break::ParamNarrowed {
            method: "node_metrics".into(),
            param: "sample".into(),
            why: "items narrowed".into()
        }]
    );
}

#[test]
fn a_parameter_array_that_drops_item_constraints_is_no_break() {
    let base = with_sample(json!({ "type": "array", "items": { "type": "integer" } }));
    let mut current = base.clone();
    param_mut(&mut current, "node_metrics", "sample")["schema"]
        .as_object_mut()
        .expect("the schema is an object")
        .remove("items");
    assert!(compatible(&base, &current).is_empty());
}

// The released files on disk.

#[test]
fn released_schemas_hold() {
    let current = openrpc();
    let checked = check_released_dir(Path::new(RELEASED_DIR), &current)
        .unwrap_or_else(|e| panic!("a released document is broken: {e}"));
    assert!(checked >= 1, "at least the 0.1.0 document is checked");
}

#[test]
fn an_empty_released_directory_fails_the_check() {
    let dir = scratch_dir("empty");
    let got = check_released_dir(&dir, &openrpc());
    std::fs::remove_dir_all(&dir).expect("the scratch directory is removed");
    assert!(
        got.is_err(),
        "an empty directory must not pass by checking nothing"
    );
}

#[test]
fn a_current_document_that_breaks_a_released_file_fails_the_check() {
    let dir = scratch_dir("broken");
    std::fs::write(
        dir.join("0.1.0.openrpc.json"),
        serde_json::to_vec(&released()).expect("the document serializes"),
    )
    .expect("the scratch file is written");
    let mut current = openrpc();
    current["methods"]
        .as_array_mut()
        .expect("methods")
        .retain(|m| m["name"] != "token_list");
    let got = check_released_dir(&dir, &current);
    std::fs::remove_dir_all(&dir).expect("the scratch directory is removed");
    let message = got.expect_err("a released method missing now must fail the check");
    assert!(
        message.contains("token_list"),
        "the break is named: {message}"
    );
}
