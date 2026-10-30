//! The OpenRPC compatibility checker. A released document is frozen, so this module lists every change in a current document
//! that would break a client built against the released one. An empty list is the only passing answer.
//!
//! The breaks are the eleven the API contract names. Methods, parameters and result fields are matched by name. A result
//! break names its path in the result schema: `$` is the root, `.field` a field and `[]` the items of an array, so
//! `$.projects[].instances[].name` is the name of one instance.
//!
//! A parameter is compared as the released schema admits it: every argument the released schema admits must still be
//! admitted. A result is compared as the current schema produces it: every result the current schema admits must already be
//! admitted by the released one. A result may therefore gain fields and admit less; it may not admit more.
//!
//! The schema subset is the one `check.rs` reads: type, enum, bounds, properties, required, items, additionalProperties and
//! anyOf. Where two schemas differ in a way this module cannot relate, it reports a break (fail closed). That covers an
//! `anyOf` that changes as a value, a keyword outside the subset (annotations and `x-` extensions aside) that differs, and
//! a schema that is not an object. The minimum scope is ranked in the order of [`crate::Scope`].
//!
//! The method's `x-arca-additional-properties` flag is not compared: the released document is closed by construction, and
//! opening a params object only admits more arguments.

use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::fmt;

use crate::check::outside_subset;

/// One change that breaks a client built against a released document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Break {
    /// A method of the released document is gone.
    MethodRemoved { method: String },
    /// A parameter of the released document is gone.
    ParamRemoved { method: String, param: String },
    /// A parameter the released document leaves optional is required now.
    ParamNowRequired { method: String, param: String },
    /// A required parameter the released document does not have.
    NewRequiredParam { method: String, param: String },
    /// A parameter admits fewer arguments than the released one. `why` names the keyword that narrowed.
    ParamNarrowed {
        method: String,
        param: String,
        why: String,
    },
    /// A result field the released document names is gone.
    ResultFieldRemoved { method: String, path: String },
    /// A result object the released document leaves open is closed now. Results are open so that a later release can add a
    /// field without a break; a closed object takes that away.
    ResultClosed { method: String, path: String },
    /// A result admits a value the released document does not: a type, an enum value, a bound, or a field that is no
    /// longer guaranteed. A result that admits less is not a break.
    ResultTypeChanged { method: String, path: String },
    /// The method's class changed.
    ClassChanged { method: String },
    /// The method's minimum scope is higher than before.
    ScopeRaised { method: String },
    /// The method writes now, and did not before.
    WritesTurnedOn { method: String },
}

impl fmt::Display for Break {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MethodRemoved { method } => write!(f, "{method}: method removed"),
            Self::ParamRemoved { method, param } => {
                write!(f, "{method}: parameter {param} removed")
            }
            Self::ParamNowRequired { method, param } => {
                write!(f, "{method}: parameter {param} is required now")
            }
            Self::NewRequiredParam { method, param } => {
                write!(f, "{method}: new required parameter {param}")
            }
            Self::ParamNarrowed { method, param, why } => {
                write!(f, "{method}: parameter {param} narrowed ({why})")
            }
            Self::ResultFieldRemoved { method, path } => {
                write!(f, "{method}: result field {path} removed")
            }
            Self::ResultClosed { method, path } => {
                write!(f, "{method}: result object closed at {path}")
            }
            Self::ResultTypeChanged { method, path } => {
                write!(f, "{method}: result type changed at {path}")
            }
            Self::ClassChanged { method } => write!(f, "{method}: class changed"),
            Self::ScopeRaised { method } => write!(f, "{method}: minimum scope raised"),
            Self::WritesTurnedOn { method } => write!(f, "{method}: writes turned on"),
        }
    }
}

const CLASS: &str = "x-arca-class";
const SCOPE: &str = "x-arca-scope";
const WRITES: &str = "x-arca-writes";

/// Every break a current document makes against a released one, in the released document's method order. Both documents
/// should pass `openrpc::check_document`. A released method without a name is skipped, since nothing can match it.
pub fn compatible(released: &Value, current: &Value) -> Vec<Break> {
    let mut out = Vec::new();
    let current_methods = methods(current);
    for old in methods(released) {
        let Some(name) = name_of(old) else { continue };
        let method = name.to_string();
        let Some(new) = find_named(&current_methods, name) else {
            out.push(Break::MethodRemoved { method });
            continue;
        };
        if old.get(CLASS) != new.get(CLASS) {
            out.push(Break::ClassChanged {
                method: method.clone(),
            });
        }
        if scope_raised(old.get(SCOPE), new.get(SCOPE)) {
            out.push(Break::ScopeRaised {
                method: method.clone(),
            });
        }
        if flag(new, WRITES) && !flag(old, WRITES) {
            out.push(Break::WritesTurnedOn {
                method: method.clone(),
            });
        }
        params_breaks(&method, old, new, &mut out);
        results_breaks(&method, old, new, &mut out);
    }
    out
}

fn methods(doc: &Value) -> Vec<&Value> {
    doc.get("methods")
        .and_then(Value::as_array)
        .map(|m| m.iter().collect())
        .unwrap_or_default()
}

fn params(method: &Value) -> Vec<&Value> {
    method
        .get("params")
        .and_then(Value::as_array)
        .map(|p| p.iter().collect())
        .unwrap_or_default()
}

fn name_of(v: &Value) -> Option<&str> {
    v.get("name")?.as_str()
}

fn find_named<'a>(list: &[&'a Value], name: &str) -> Option<&'a Value> {
    list.iter().copied().find(|v| name_of(v) == Some(name))
}

/// Whether a key holds `true`. An absent key is `false`.
fn flag(v: &Value, key: &str) -> bool {
    v.get(key) == Some(&Value::Bool(true))
}

/// The scope rank, in the order of [`crate::Scope`]. An unknown scope has no rank.
fn scope_rank(v: Option<&Value>) -> Option<u8> {
    match v?.as_str()? {
        "viewer" => Some(0),
        "doctor" => Some(1),
        "operator" => Some(2),
        _ => None,
    }
}

fn scope_raised(old: Option<&Value>, new: Option<&Value>) -> bool {
    if old == new {
        return false;
    }
    match (scope_rank(new), scope_rank(old)) {
        (Some(new), Some(old)) => new > old,
        // A scope this module cannot rank is treated as raised: fail closed.
        _ => true,
    }
}

fn params_breaks(method: &str, old: &Value, new: &Value, out: &mut Vec<Break>) {
    let old_params = params(old);
    let new_params = params(new);
    for op in &old_params {
        let Some(name) = name_of(op) else { continue };
        let Some(np) = find_named(&new_params, name) else {
            out.push(Break::ParamRemoved {
                method: method.to_string(),
                param: name.to_string(),
            });
            continue;
        };
        if flag(np, "required") && !flag(op, "required") {
            out.push(Break::ParamNowRequired {
                method: method.to_string(),
                param: name.to_string(),
            });
        }
        // The released schema's arguments must all still be admitted: the current schema is the wide side.
        if let Err(why) = covers(&schema_of(np), &schema_of(op)) {
            out.push(Break::ParamNarrowed {
                method: method.to_string(),
                param: name.to_string(),
                why,
            });
        }
    }
    for np in &new_params {
        let Some(name) = name_of(np) else { continue };
        if flag(np, "required") && find_named(&old_params, name).is_none() {
            out.push(Break::NewRequiredParam {
                method: method.to_string(),
                param: name.to_string(),
            });
        }
    }
}

fn schema_of(descriptor: &Value) -> Value {
    descriptor
        .get("schema")
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()))
}

fn results_breaks(method: &str, old: &Value, new: &Value, out: &mut Vec<Break>) {
    match (old.pointer("/result/schema"), new.pointer("/result/schema")) {
        (Some(old), Some(new)) => result_node(method, old, new, "$", out),
        (Some(_), None) => out.push(Break::ResultTypeChanged {
            method: method.to_string(),
            path: "$".to_string(),
        }),
        (None, _) => {}
    }
}

/// One node of a result schema, compared as the current schema produces it against the released one.
fn result_node(method: &str, old: &Value, new: &Value, path: &str, out: &mut Vec<Break>) {
    if old == new {
        return;
    }
    let changed = |path: String| Break::ResultTypeChanged {
        method: method.to_string(),
        path,
    };
    let (Some(old_map), Some(new_map)) = (old.as_object(), new.as_object()) else {
        // A schema that is not an object is not modelled: fail closed.
        out.push(changed(path.to_string()));
        return;
    };
    if closed(new) && !closed(old) {
        out.push(Break::ResultClosed {
            method: method.to_string(),
            path: path.to_string(),
        });
    }
    // A closed released object refuses every field it does not name, so a field the current result adds is a broader result,
    // and so is an object the current result leaves open.
    let widened = closed(old) && (!closed(new) || names_new_property(old, new));
    if let Some(old_fields) = properties(old) {
        for (field, old_field) in old_fields {
            let child = format!("{path}.{field}");
            match properties(new).and_then(|fields| fields.get(field)) {
                None => out.push(Break::ResultFieldRemoved {
                    method: method.to_string(),
                    path: child,
                }),
                Some(new_field) => {
                    if required_names(old).contains(&field.as_str())
                        && !required_names(new).contains(&field.as_str())
                    {
                        // A field the client could rely on may be absent now.
                        out.push(changed(child.clone()));
                    }
                    result_node(method, old_field, new_field, &child, out);
                }
            }
        }
    }
    if let Some(old_items) = old.get("items") {
        let child = format!("{path}[]");
        match new.get("items") {
            None => out.push(changed(child)),
            Some(new_items) => result_node(method, old_items, new_items, &child, out),
        }
    }
    if widened
        || unmodelled_difference(old_map, new_map).is_some()
        || old.get("anyOf") != new.get("anyOf")
        || covers_domain(old, new).is_err()
    {
        out.push(changed(path.to_string()));
    }
}

/// Whether `wide` admits every value `narrow` admits. An error names the first keyword that does not. The result side
/// asks `covers(released, current)`; the parameter side asks `covers(current, released)`.
fn covers(wide: &Value, narrow: &Value) -> Result<(), String> {
    if wide == narrow {
        return Ok(());
    }
    let (Some(wide_map), Some(narrow_map)) = (wide.as_object(), narrow.as_object()) else {
        return Err("schema is not an object".to_string());
    };
    if let Some(key) = unmodelled_difference(wide_map, narrow_map) {
        return Err(format!("{key} changed"));
    }
    // anyOf is compared as a value: an unchanged one is no reason to refuse, and a changed one cannot be related.
    if wide.get("anyOf") != narrow.get("anyOf") {
        return Err("anyOf changed".to_string());
    }
    covers_domain(wide, narrow)?;
    if closed(wide) && !closed(narrow) {
        return Err("additional properties refused".to_string());
    }
    for name in required_names(wide) {
        if !required_names(narrow).contains(&name) {
            return Err(format!("property {name} made required"));
        }
    }
    let keys: BTreeSet<&str> = properties(wide)
        .into_iter()
        .chain(properties(narrow))
        .flat_map(|fields| fields.keys().map(String::as_str))
        .collect();
    for key in keys {
        match (slot(wide, key), slot(narrow, key)) {
            (_, Slot::Forbidden) => {}
            (Slot::Forbidden, _) => return Err(format!("property {key} refused")),
            (wide_slot, narrow_slot) => covers(&wide_slot.value(), &narrow_slot.value())
                .map_err(|e| format!("property {key}: {e}"))?,
        }
    }
    match (wide.get("items"), narrow.get("items")) {
        (None, _) => {}
        (Some(_), None) => return Err("items narrowed".to_string()),
        (Some(wide_items), Some(narrow_items)) => {
            covers(wide_items, narrow_items).map_err(|e| format!("items: {e}"))?;
        }
    }
    Ok(())
}

/// The type, enum and bound keywords only. The result side uses this on its own node, because its fields and items are
/// walked by `result_node`.
fn covers_domain(wide: &Value, narrow: &Value) -> Result<(), String> {
    if let Some(wide_type) = wide.get("type") {
        let allowed = type_names(wide_type);
        let offered = narrow.get("type").and_then(type_names);
        match (allowed, offered) {
            (Some(allowed), Some(offered)) if offered.iter().all(|t| type_covered(t, &allowed)) => {
            }
            _ => return Err("type set shrunk".to_string()),
        }
    }
    if let Some(wide_enum) = wide.get("enum").and_then(Value::as_array) {
        let Some(narrow_enum) = narrow.get("enum").and_then(Value::as_array) else {
            return Err("enum added".to_string());
        };
        if narrow_enum.iter().any(|v| !wide_enum.contains(v)) {
            return Err("enum shrunk".to_string());
        }
    }
    if let Some(wide_min) = wide.get("minimum").and_then(Value::as_f64)
        && !narrow
            .get("minimum")
            .and_then(Value::as_f64)
            .is_some_and(|min| min >= wide_min)
    {
        return Err("minimum raised".to_string());
    }
    if let Some(wide_max) = wide.get("maximum").and_then(Value::as_f64)
        && !narrow
            .get("maximum")
            .and_then(Value::as_f64)
            .is_some_and(|max| max <= wide_max)
    {
        return Err("maximum lowered".to_string());
    }
    if let Some(wide_min) = wide.get("minLength").and_then(Value::as_u64)
        && !narrow
            .get("minLength")
            .and_then(Value::as_u64)
            .is_some_and(|min| min >= wide_min)
    {
        return Err("minLength raised".to_string());
    }
    if let Some(wide_max) = wide.get("maxLength").and_then(Value::as_u64)
        && !narrow
            .get("maxLength")
            .and_then(Value::as_u64)
            .is_some_and(|max| max <= wide_max)
    {
        return Err("maxLength lowered".to_string());
    }
    Ok(())
}

/// The type names of a `type` keyword, or `None` when the keyword is neither a name nor an array of names.
fn type_names(v: &Value) -> Option<Vec<&str>> {
    match v {
        Value::String(name) => Some(vec![name.as_str()]),
        Value::Array(items) => items.iter().map(Value::as_str).collect(),
        _ => None,
    }
}

/// Whether `name` is one of `allowed`. Every integer is a number.
fn type_covered(name: &str, allowed: &[&str]) -> bool {
    allowed.contains(&name) || (name == "integer" && allowed.contains(&"number"))
}

/// What a schema says about one property name.
enum Slot<'a> {
    /// The schema names the property.
    Schema(&'a Value),
    /// An open object names no schema for it, so it admits any value.
    Any,
    /// A closed object names no schema for it, so it refuses the property.
    Forbidden,
}

impl Slot<'_> {
    /// The schema of a slot that is not forbidden. `Any` is the empty schema.
    fn value(&self) -> Value {
        match self {
            Self::Schema(schema) => (*schema).clone(),
            Self::Any | Self::Forbidden => Value::Object(Map::new()),
        }
    }
}

fn slot<'a>(schema: &'a Value, key: &str) -> Slot<'a> {
    if let Some(sub) = properties(schema).and_then(|fields| fields.get(key)) {
        Slot::Schema(sub)
    } else if closed(schema) {
        Slot::Forbidden
    } else {
        Slot::Any
    }
}

fn properties(schema: &Value) -> Option<&Map<String, Value>> {
    schema.get("properties")?.as_object()
}

fn required_names(schema: &Value) -> Vec<&str> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

fn closed(schema: &Value) -> bool {
    schema.get("additionalProperties") == Some(&Value::Bool(false))
}

/// Whether `new` names a property that `old` does not name.
fn names_new_property(old: &Value, new: &Value) -> bool {
    let old_fields = properties(old);
    properties(new).is_some_and(|fields| {
        fields
            .keys()
            .any(|name| !old_fields.is_some_and(|known| known.contains_key(name)))
    })
}

/// The first keyword outside the subset, in name order, on which two schema objects differ. The checker does not model
/// such a keyword, so a difference in it cannot be related and is a break.
fn unmodelled_difference<'a>(
    wide: &'a Map<String, Value>,
    narrow: &'a Map<String, Value>,
) -> Option<&'a str> {
    let keys: BTreeSet<&str> = wide
        .keys()
        .chain(narrow.keys())
        .map(String::as_str)
        .filter(|key| outside_subset(key))
        .collect();
    keys.into_iter()
        .find(|key| wide.get(*key) != narrow.get(*key))
}
