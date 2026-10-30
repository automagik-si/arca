//! The OpenRPC compatibility checker against the released 0.1.0 document. Each break fixture is the released document with
//! one edit and yields exactly one break. The additive fixture yields none. The enum and minLength fixtures start from a
//! base that adds one optional string parameter, because no released parameter carries an enum or a string bound.

use arca_api::Scope;
use arca_api::compat::{Break, compatible};
use arca_api::openrpc::{check_document, openrpc};
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
/// error, so the check cannot pass by checking nothing.
fn check_released_dir(dir: &Path, current: &Value) -> Result<usize, String> {
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
        check_document(&released).map_err(|e| format!("{}: {e}", file.display()))?;
        let breaks = compatible(&released, current);
        if !breaks.is_empty() {
            let named: Vec<String> = breaks.iter().map(ToString::to_string).collect();
            return Err(format!("{}: {}", file.display(), named.join("; ")));
        }
    }
    Ok(files.len())
}

// The thirteen failing fixtures: eleven break variants, with ParamNarrowed covered three times.

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
fn a_shrunk_enum_is_one_parameter_narrowing() {
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
