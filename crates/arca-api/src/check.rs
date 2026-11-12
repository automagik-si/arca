//! A JSON Schema subset, enough to check the arguments and results of the verb table: `type` (a name or an array of names),
//! `properties`, `required`, `items`, `enum`, `minimum`, `maximum`, `minLength`, `maxLength`, `anyOf`,
//! `additionalProperties` (false, true or a schema for the fields `properties` does not name), `propertyNames` (a schema
//! every field name must meet) and `pattern`. The pattern is an ECMA-style regular expression; the schema is code, not a
//! caller value, so it is compiled when the schema is checked.
//!
//! The schema is checked before the value. A keyword outside the subset is an error, not a silent pass, so a schema that
//! asks for more than this module checks cannot accept a value it should refuse. Annotation keywords (`title`,
//! `description`, `$comment`, `default`, `examples`, `summary`, `$schema`) and `x-` extensions are ignored.
//!
//! An error names the JSON Pointer of the failing value and the reason. It never carries the value, so an argument that
//! holds a secret cannot reach a log through a validation error.

use serde_json::{Map, Value};

const ANNOTATIONS: &[&str] = &[
    "$schema",
    "$comment",
    "title",
    "description",
    "default",
    "examples",
    "summary",
];

const SUPPORTED: &[&str] = &[
    "type",
    "properties",
    "required",
    "items",
    "enum",
    "minimum",
    "maximum",
    "minLength",
    "maxLength",
    "maxItems",
    "const",
    "pattern",
    "anyOf",
    "additionalProperties",
    "propertyNames",
];

const TYPE_NAMES: &[&str] = &[
    "null", "boolean", "string", "array", "object", "number", "integer",
];

/// Whether a keyword is outside the subset: not supported, not an annotation and not an `x-` extension. The schema check
/// refuses such a keyword, and the compatibility checker treats a difference in it as a break.
pub(crate) fn outside_subset(key: &str) -> bool {
    !(key.starts_with("x-") || ANNOTATIONS.contains(&key) || SUPPORTED.contains(&key))
}

/// Whether `value` conforms to `schema`. The first failure is returned as `Err`, naming its path and reason.
pub fn conforms(schema: &Value, value: &Value) -> Result<(), String> {
    check_schema(schema, "")?;
    validate(schema, value, "")
}

/// The schema itself must be inside the subset. Every nested schema is visited, so an unsupported keyword is refused even
/// where the value never reaches it. Each supported keyword must also hold a value of its own type: `enum` an array,
/// `minimum` a number, `required` an array of names, and so on. The document check runs this over every schema of a
/// method, so a malformed one fails the compatibility gate.
pub(crate) fn check_schema(schema: &Value, at: &str) -> Result<(), String> {
    let Value::Object(map) = schema else {
        return Err(format!("{}: a schema must be an object", here(at)));
    };
    for key in map.keys() {
        if outside_subset(key) {
            return Err(format!("{}: keyword {key} is not supported", here(at)));
        }
    }
    if let Some(types) = map.get("type") {
        type_names(types, at)?;
    }
    if let Some(props) = map.get("properties") {
        let Value::Object(props) = props else {
            return Err(format!("{}: properties must be an object", here(at)));
        };
        for (name, sub) in props {
            check_schema(sub, &child(at, name))?;
        }
    }
    if let Some(items) = map.get("items") {
        check_schema(items, &format!("{at}/items"))?;
    }
    if let Some(options) = map.get("anyOf") {
        let Value::Array(options) = options else {
            return Err(format!("{}: anyOf must be an array", here(at)));
        };
        for option in options {
            check_schema(option, &format!("{at}/anyOf"))?;
        }
    }
    if let Some(pattern) = map.get("pattern") {
        let Some(text) = pattern.as_str() else {
            return Err(format!("{}: pattern must be a string", here(at)));
        };
        regex::Regex::new(text)
            .map_err(|_| format!("{}: pattern is not a regular expression", here(at)))?;
    }
    if let Some(max_items) = map.get("maxItems")
        && !max_items.is_u64()
    {
        return Err(format!(
            "{}: maxItems must be a non-negative integer",
            here(at)
        ));
    }
    if let Some(required) = map.get("required")
        && !required
            .as_array()
            .is_some_and(|names| names.iter().all(Value::is_string))
    {
        return Err(format!("{}: required must be an array of names", here(at)));
    }
    if map.get("enum").is_some_and(|e| !e.is_array()) {
        return Err(format!("{}: enum must be an array", here(at)));
    }
    for bound in ["minimum", "maximum"] {
        if map.get(bound).is_some_and(|b| !b.is_number()) {
            return Err(format!("{}: {bound} must be a number", here(at)));
        }
    }
    for bound in ["minLength", "maxLength"] {
        if map.get(bound).is_some_and(|b| b.as_u64().is_none()) {
            return Err(format!("{}: {bound} must be a whole number", here(at)));
        }
    }
    match map.get("additionalProperties") {
        None | Some(Value::Bool(_)) => {}
        Some(sub @ Value::Object(_)) => check_schema(sub, &format!("{at}/additionalProperties"))?,
        Some(_) => {
            return Err(format!(
                "{}: additionalProperties must be true, false or a schema",
                here(at)
            ));
        }
    }
    if let Some(names) = map.get("propertyNames") {
        check_schema(names, &format!("{at}/propertyNames"))?;
    }
    Ok(())
}

/// The type names of a `type` keyword: one name or an array of names, each a JSON type.
fn type_names(types: &Value, at: &str) -> Result<Vec<String>, String> {
    let names: Vec<&Value> = match types {
        Value::Array(items) if !items.is_empty() => items.iter().collect(),
        Value::String(_) => vec![types],
        _ => {
            return Err(format!(
                "{}: type must be a name or an array of names",
                here(at)
            ));
        }
    };
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        match name.as_str() {
            Some(text) if TYPE_NAMES.contains(&text) => out.push(text.to_string()),
            _ => {
                return Err(format!(
                    "{}: type names a JSON type this subset lacks",
                    here(at)
                ));
            }
        }
    }
    Ok(out)
}

fn validate(schema: &Value, value: &Value, at: &str) -> Result<(), String> {
    let Value::Object(map) = schema else {
        return Err(format!("{}: a schema must be an object", here(at)));
    };
    if let Some(types) = map.get("type") {
        let names = type_names(types, at)?;
        if !names.iter().any(|name| is_type(name, value)) {
            return Err(format!("{}: expected {}", here(at), names.join(" or ")));
        }
    }
    if let Some(constant) = map.get("const")
        && constant != value
    {
        return Err(format!("{}: not the required value", here(at)));
    }
    if let Some(Value::Array(options)) = map.get("enum")
        && !options.contains(value)
    {
        return Err(format!("{}: not one of the allowed values", here(at)));
    }
    if let Some(Value::Array(options)) = map.get("anyOf")
        && !options
            .iter()
            .any(|option| validate(option, value, at).is_ok())
    {
        return Err(format!("{}: matches none of the allowed schemas", here(at)));
    }
    match value {
        Value::Number(_) => check_bounds(map, value, at)?,
        Value::String(text) => {
            check_length(map, text.chars().count(), at)?;
            if let Some(pattern) = map.get("pattern").and_then(Value::as_str)
                && !regex::Regex::new(pattern)
                    .map(|pattern| pattern.is_match(text))
                    .unwrap_or(false)
            {
                return Err(format!("{}: does not match the pattern", here(at)));
            }
        }
        Value::Array(items) => {
            if map
                .get("maxItems")
                .and_then(Value::as_u64)
                .is_some_and(|max| items.len() as u64 > max)
            {
                return Err(format!("{}: longer than the maximum length", here(at)));
            }
            if let Some(items_schema) = map.get("items") {
                for (index, item) in items.iter().enumerate() {
                    validate(items_schema, item, &format!("{at}/{index}"))?;
                }
            }
        }
        Value::Object(fields) => check_object(map, fields, at)?,
        Value::Null | Value::Bool(_) => {}
    }
    Ok(())
}

fn check_bounds(map: &Map<String, Value>, value: &Value, at: &str) -> Result<(), String> {
    let Some(x) = value.as_f64() else {
        return Ok(());
    };
    if map
        .get("minimum")
        .and_then(Value::as_f64)
        .is_some_and(|min| x < min)
    {
        return Err(format!("{}: below the minimum", here(at)));
    }
    if map
        .get("maximum")
        .and_then(Value::as_f64)
        .is_some_and(|max| x > max)
    {
        return Err(format!("{}: above the maximum", here(at)));
    }
    Ok(())
}

fn check_length(map: &Map<String, Value>, chars: usize, at: &str) -> Result<(), String> {
    if map
        .get("minLength")
        .and_then(Value::as_u64)
        .is_some_and(|min| (chars as u64) < min)
    {
        return Err(format!("{}: shorter than the minimum length", here(at)));
    }
    if map
        .get("maxLength")
        .and_then(Value::as_u64)
        .is_some_and(|max| (chars as u64) > max)
    {
        return Err(format!("{}: longer than the maximum length", here(at)));
    }
    Ok(())
}

fn check_object(
    map: &Map<String, Value>,
    fields: &Map<String, Value>,
    at: &str,
) -> Result<(), String> {
    if let Some(Value::Array(required)) = map.get("required") {
        for name in required.iter().filter_map(Value::as_str) {
            if !fields.contains_key(name) {
                return Err(format!("{}: missing required field {name}", here(at)));
            }
        }
    }
    let props = map.get("properties").and_then(Value::as_object);
    if let Some(props) = props {
        for (name, sub) in props {
            if let Some(field) = fields.get(name) {
                validate(sub, field, &child(at, name))?;
            }
        }
    }
    // An unknown key is never echoed, here or in a path: a client may have put a secret in the key. The refusal names
    // the place and the fields the schema does take, which are the schema's own words.
    match map.get("additionalProperties") {
        Some(Value::Bool(false)) => {
            for name in fields.keys() {
                if !props.is_some_and(|p| p.contains_key(name)) {
                    let named: Vec<&str> = props
                        .map(|p| p.keys().map(String::as_str).collect())
                        .unwrap_or_default();
                    return Err(if named.is_empty() {
                        format!(
                            "{}: a field the schema does not name; it takes no field",
                            here(at)
                        )
                    } else {
                        format!(
                            "{}: a field the schema does not name; the fields it takes are {}",
                            here(at),
                            named.join(", ")
                        )
                    });
                }
            }
        }
        Some(sub @ Value::Object(_)) => {
            for (name, field) in fields {
                if !props.is_some_and(|p| p.contains_key(name)) && validate(sub, field, at).is_err()
                {
                    return Err(format!(
                        "{}: a field the schema does not name holds a value it refuses",
                        here(at)
                    ));
                }
            }
        }
        _ => {}
    }
    if let Some(names) = map.get("propertyNames") {
        for name in fields.keys() {
            if validate(names, &Value::String(name.clone()), at).is_err() {
                return Err(format!("{}: a field name the schema refuses", here(at)));
            }
        }
    }
    Ok(())
}

fn is_type(name: &str, value: &Value) -> bool {
    match name {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "string" => value.is_string(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "number" => value.is_number(),
        "integer" => value.as_number().is_some_and(|n| {
            n.is_i64()
                || n.is_u64()
                || n.as_f64()
                    .is_some_and(|x| x.is_finite() && x.fract() == 0.0)
        }),
        _ => false,
    }
}

/// The JSON Pointer segment for a property name (RFC 6901: `~` and `/` are escaped).
fn child(at: &str, name: &str) -> String {
    format!("{at}/{}", name.replace('~', "~0").replace('/', "~1"))
}

/// How an error names its location: the pointer, or the whole value at the root.
fn here(at: &str) -> String {
    if at.is_empty() {
        "the value".to_string()
    } else {
        at.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn additional_properties_as_a_schema_and_property_names_check_every_unnamed_field_and_name() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "id": { "type": "string" } },
            "additionalProperties": { "type": "integer" },
            "propertyNames": { "type": "string", "maxLength": 4 }
        });
        assert_eq!(
            conforms(&schema, &serde_json::json!({ "id": "x", "n": 1 })),
            Ok(())
        );
        let refused = conforms(&schema, &serde_json::json!({ "n": "secret-value" })).unwrap_err();
        assert!(!refused.contains("secret-value"));
        assert!(refused.contains("holds a value it refuses"), "{refused}");
        let refused = conforms(&schema, &serde_json::json!({ "secret-key": 1 })).unwrap_err();
        assert!(!refused.contains("secret-key"), "the key is never echoed");
        assert!(
            refused.contains("a field name the schema refuses"),
            "{refused}"
        );
        let bad = serde_json::json!({ "propertyNames": { "type": "text" } });
        assert!(
            conforms(&bad, &serde_json::json!({})).is_err(),
            "a nested schema is checked"
        );
        let bad = serde_json::json!({ "additionalProperties": 3 });
        assert!(conforms(&bad, &serde_json::json!({})).is_err());
    }
    use serde_json::json;

    #[test]
    fn integer_accepts_whole_numbers_only() {
        let schema = json!({ "type": "integer" });
        assert!(conforms(&schema, &json!(1)).is_ok());
        assert!(conforms(&schema, &json!(-7)).is_ok());
        assert!(
            conforms(&schema, &json!(1.0)).is_ok(),
            "1.0 is a whole number"
        );
        assert!(conforms(&schema, &json!(1.5)).is_err());
        assert!(conforms(&schema, &json!("1")).is_err());
    }

    #[test]
    fn a_type_array_accepts_each_listed_type() {
        let schema = json!({ "type": ["object", "null"], "required": ["a"] });
        assert!(
            conforms(&schema, &Value::Null).is_ok(),
            "null passes the null type"
        );
        assert!(conforms(&schema, &json!({ "a": 1 })).is_ok());
        assert!(
            conforms(&schema, &json!({})).is_err(),
            "the object rule still applies"
        );
        assert!(conforms(&schema, &json!("x")).is_err());
    }

    #[test]
    fn required_and_properties_apply_to_objects() {
        let schema = json!({
            "type": "object",
            "properties": { "a": { "type": "integer" } },
            "required": ["a"]
        });
        assert!(conforms(&schema, &json!({})).is_err(), "a is required");
        assert!(conforms(&schema, &json!({ "a": "x" })).is_err());
        assert!(conforms(&schema, &json!({ "a": 1 })).is_ok());
        assert!(
            conforms(&schema, &json!([1])).is_err(),
            "an array is not an object"
        );
    }

    #[test]
    fn additional_properties_false_refuses_a_field_the_schema_does_not_name() {
        let schema = json!({
            "type": "object",
            "properties": { "a": { "type": "integer" } },
            "additionalProperties": false
        });
        assert!(conforms(&schema, &json!({ "a": 1 })).is_ok());
        let err =
            conforms(&schema, &json!({ "a": 1, "extra-key-7": 2 })).expect_err("extra refused");
        assert!(
            !err.contains("extra-key-7"),
            "the unknown key is not echoed: {err}"
        );
        // The refusal names the place and the fields the schema does take.
        assert_eq!(
            err,
            "the value: a field the schema does not name; the fields it takes are a"
        );
        let nested = json!({
            "type": "object",
            "properties": { "args": schema.clone() },
            "additionalProperties": false
        });
        assert_eq!(
            conforms(&nested, &json!({ "args": { "extra-key-7": 2 } })).expect_err("extra refused"),
            "/args: a field the schema does not name; the fields it takes are a"
        );
        let empty = json!({ "type": "object", "additionalProperties": false });
        assert_eq!(
            conforms(&empty, &json!({ "extra-key-7": 2 })).expect_err("extra refused"),
            "the value: a field the schema does not name; it takes no field"
        );
    }

    #[test]
    fn errors_never_echo_the_value() {
        let schema = json!({ "type": "string", "maxLength": 4 });
        let err = conforms(&schema, &json!("secret-value-123")).expect_err("too long");
        assert!(!err.contains("secret"), "the value is not echoed: {err}");
        let schema = json!({ "type": "integer", "maximum": 60 });
        let err = conforms(&schema, &json!(9_999_999)).expect_err("too big");
        assert!(!err.contains("9999999"), "the number is not echoed: {err}");
    }

    #[test]
    fn bounds_are_inclusive() {
        let schema = json!({ "type": "integer", "minimum": 1, "maximum": 60 });
        assert!(conforms(&schema, &json!(1)).is_ok());
        assert!(conforms(&schema, &json!(60)).is_ok());
        assert!(conforms(&schema, &json!(0)).is_err());
        assert!(conforms(&schema, &json!(61)).is_err());
    }

    #[test]
    fn lengths_count_characters() {
        let schema = json!({ "type": "string", "minLength": 2, "maxLength": 3 });
        assert!(conforms(&schema, &json!("ab")).is_ok());
        assert!(
            conforms(&schema, &json!("é☕x")).is_ok(),
            "three characters, not bytes"
        );
        assert!(conforms(&schema, &json!("a")).is_err());
        assert!(conforms(&schema, &json!("abcd")).is_err());
    }

    #[test]
    fn enum_needs_an_exact_member() {
        let schema = json!({ "type": "string", "enum": ["watch-only", "kernel"] });
        assert!(conforms(&schema, &json!("kernel")).is_ok());
        assert!(conforms(&schema, &json!("Kernel")).is_err());
    }

    #[test]
    fn any_of_needs_one_matching_schema() {
        let schema = json!({ "anyOf": [{ "type": "integer" }, { "type": "string" }] });
        assert!(conforms(&schema, &json!(3)).is_ok());
        assert!(conforms(&schema, &json!("x")).is_ok());
        assert!(conforms(&schema, &json!(true)).is_err());
    }

    #[test]
    fn items_check_each_element_and_name_its_index() {
        let schema = json!({ "type": "array", "items": { "type": "integer" } });
        assert!(conforms(&schema, &json!([1, 2])).is_ok());
        let err = conforms(&schema, &json!([1, "x"])).expect_err("second item refused");
        assert!(err.starts_with("/1"), "the path names the index: {err}");
    }

    #[test]
    fn a_property_name_is_escaped_in_the_path() {
        let schema = json!({
            "type": "object",
            "properties": { "a/b": { "type": "integer" } }
        });
        let err = conforms(&schema, &json!({ "a/b": "x" })).expect_err("refused");
        assert!(err.starts_with("/a~1b"), "RFC 6901 escaping: {err}");
    }

    #[test]
    fn an_unsupported_keyword_fails_closed_even_where_no_value_reaches_it() {
        let schema = json!({ "type": "array", "minItems": 1 });
        assert!(
            conforms(&schema, &json!([])).is_err(),
            "minItems is not checked, so it is refused"
        );
        let nested = json!({
            "type": "object",
            "properties": { "x": { "type": "array", "items": { "minItems": 1 } } }
        });
        assert!(
            conforms(&nested, &json!({ "x": [] })).is_err(),
            "a nested schema is checked too"
        );
    }

    #[test]
    fn a_pattern_matches_the_whole_string_and_checks_arrays() {
        let schema = json!({
            "type": "array",
            "maxItems": 2,
            "items": {
                "type": "string",
                "pattern": "^[a-z](?:[a-z0-9-]{0,3}[a-z0-9])?$"
            }
        });
        assert!(conforms(&schema, &json!(["a", "ab-1"])).is_ok());
        assert!(conforms(&schema, &json!(["A"])).is_err());
        assert!(conforms(&schema, &json!(["a", "b", "c"])).is_err());
        let constant = json!({ "const": 8443 });
        assert!(conforms(&constant, &json!(8443)).is_ok());
        assert!(conforms(&constant, &json!(443)).is_err());
    }

    #[test]
    fn annotations_and_extensions_are_ignored() {
        let schema = json!({
            "title": "t",
            "description": "d",
            "x-arca-class": "L",
            "type": "string"
        });
        assert!(conforms(&schema, &json!("a")).is_ok());
    }

    #[test]
    fn a_schema_must_be_an_object() {
        assert!(conforms(&json!(true), &json!(1)).is_err());
    }
}
