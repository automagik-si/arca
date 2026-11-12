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
        return Err(not_allowed(at, options, value));
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

/// A text outside its schema's length. The words name the limit, so a caller who chose the text knows by how much to
/// change it without reading the schema; they never name the text or its own length.
fn check_length(map: &Map<String, Value>, chars: usize, at: &str) -> Result<(), String> {
    let count = |n: u64| if n == 1 { "character" } else { "characters" };
    if let Some(min) = map.get("minLength").and_then(Value::as_u64)
        && (chars as u64) < min
    {
        return Err(format!(
            "{}: shorter than the minimum length of {min} {}",
            here(at),
            count(min)
        ));
    }
    if let Some(max) = map.get("maxLength").and_then(Value::as_u64)
        && (chars as u64) > max
    {
        return Err(format!(
            "{}: longer than the maximum length of {max} {}",
            here(at),
            count(max)
        ));
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
    // An unknown key is named, so the caller sees which of its fields is wrong, but only when it is shaped like a field
    // name ([`name_shaped`]): a key that is long or holds other characters may be a secret a client put there, and is
    // never repeated. It is never put in a path either.
    match map.get("additionalProperties") {
        Some(Value::Bool(false)) => {
            for name in fields.keys() {
                if !props.is_some_and(|p| p.contains_key(name)) {
                    let known: Vec<&str> = props
                        .map(|p| p.keys().map(String::as_str).collect())
                        .unwrap_or_default();
                    return Err(unknown_field(at, name, &known));
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

/// The longest unknown key a refusal repeats. The longest field name a verb takes is 19 characters; common service tokens
/// that are all letters, digits and `_` start at 32. So 24 names every real field and repeats none of those.
const LONGEST_NAMED_KEY: usize = 24;

/// Whether an unknown key is shaped like a field name, and so may be repeated in a refusal: it starts with a letter,
/// holds only letters, digits, `_` and `-`, and is at most [`LONGEST_NAMED_KEY`] characters. A token, a key or a
/// sentence a client put in the place of a field name is longer or holds other characters, and is never repeated.
fn name_shaped(key: &str) -> bool {
    let mut chars = key.chars();
    key.len() <= LONGEST_NAMED_KEY
        && chars
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The refusal of a field an object does not take: the one sentence every verb's refusal of an unknown field is, whether
/// the schema check or a handler makes it. `at` is the object's place (empty for the arguments themselves), `key` the
/// field that was sent, and `known` the fields the object takes.
///
/// It names the field when it is [`name_shaped`], lists the fields that are taken, and offers the closest one when one
/// is close: `unknown field 'nmae'; this verb takes: instance, name, project; did you mean name?`.
pub fn unknown_field(at: &str, key: &str, known: &[&str]) -> String {
    let place = if at.is_empty() {
        String::new()
    } else {
        format!("{at}: ")
    };
    let subject = if at.is_empty() {
        "this verb takes"
    } else {
        "it takes"
    };
    let shaped = name_shaped(key);
    let named = if shaped {
        format!("unknown field '{key}'")
    } else {
        "a field the schema does not name".to_string()
    };
    let takes = if known.is_empty() {
        format!("{subject} no field")
    } else {
        format!("{subject}: {}", known.join(", "))
    };
    let hint = match closest(key, known.iter().copied()) {
        Some(near) if shaped => format!("; did you mean {near}?"),
        _ => String::new(),
    };
    format!("{place}{named}; {takes}{hint}")
}

/// The most allowed values a refusal lists by name.
const ALLOWED_LISTED: usize = 64;

/// The refusal of a value an `enum` does not hold. When every allowed value is a string it lists them, and offers the
/// closest one when one is close: `/action: not one of the allowed values; it is one of: postgres_create, ...; did you
/// mean postgres_create?`. The value sent is never repeated.
fn not_allowed(at: &str, options: &[Value], value: &Value) -> String {
    let refusal = format!("{}: not one of the allowed values", here(at));
    let names: Option<Vec<&str>> = options.iter().map(Value::as_str).collect();
    let Some(names) = names.filter(|names| !names.is_empty() && names.len() <= ALLOWED_LISTED)
    else {
        return refusal;
    };
    let hint = match value
        .as_str()
        .and_then(|sent| closest(sent, names.iter().copied()))
    {
        Some(near) => format!("; did you mean {near}?"),
        None => String::new(),
    };
    format!("{refusal}; it is one of: {}{hint}", names.join(", "))
}

/// The number of single-character edits (insert, remove, replace) that turn `a` into `b`.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, x) in a.chars().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, y) in b.iter().enumerate() {
            let replaced = previous + usize::from(x != *y);
            previous = row[j + 1];
            row[j + 1] = replaced.min(previous + 1).min(row[j] + 1);
        }
    }
    row[b.len()]
}

/// The known word closest to `word`, when one is close enough to be what was meant: at most two edits away, or a third of
/// the word's length when that is more. Two words equally close give the first in `known`'s order.
pub fn closest<'a>(word: &str, known: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let limit = (word.chars().count() / 3).max(2);
    known
        .into_iter()
        .map(|candidate| (edit_distance(word, candidate), candidate))
        .filter(|(distance, candidate)| *distance <= limit && *distance < candidate.chars().count())
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, candidate)| candidate)
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
        // A key that could be a secret (not shaped like a field name) is refused and never repeated.
        let err = conforms(&schema, &json!({ "a": 1, "extra key: s3cret/7": 2 }))
            .expect_err("extra refused");
        assert!(
            !err.contains("s3cret"),
            "a key that is not a field name is not echoed: {err}"
        );
        // A key shaped like no field name is never repeated: the refusal names the place and the fields that are taken.
        assert_eq!(err, "a field the schema does not name; this verb takes: a");
        let nested = json!({
            "type": "object",
            "properties": { "args": schema.clone() },
            "additionalProperties": false
        });
        assert_eq!(
            conforms(
                &nested,
                &json!({ "args": { "arca_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA": 2 } })
            )
            .expect_err("extra refused"),
            "/args: a field the schema does not name; it takes: a"
        );
        let empty = json!({ "type": "object", "additionalProperties": false });
        assert_eq!(
            conforms(&empty, &json!({ "not a name": 2 })).expect_err("extra refused"),
            "a field the schema does not name; this verb takes no field"
        );
    }

    #[test]
    fn a_value_outside_a_list_of_names_is_refused_with_the_list_and_the_nearest_name() {
        let schema = json!({ "enum": ["postgres_create", "postgres_delete", "service_create"] });
        assert_eq!(
            conforms(&schema, &json!("postgres_creat")).expect_err("refused"),
            "the value: not one of the allowed values; it is one of: postgres_create, postgres_delete, service_create; did you mean postgres_create?"
        );
        // Nothing close: the list, and no guess.
        assert_eq!(
            conforms(&schema, &json!("zzz")).expect_err("refused"),
            "the value: not one of the allowed values; it is one of: postgres_create, postgres_delete, service_create"
        );
        // A value that is not text is refused with the list, and what was sent is never repeated.
        assert_eq!(
            conforms(&schema, &json!(7)).expect_err("refused"),
            "the value: not one of the allowed values; it is one of: postgres_create, postgres_delete, service_create"
        );
        // A list that is not all names, or too long to say, is refused in the words it always had.
        assert_eq!(
            conforms(&json!({ "enum": [1, 2] }), &json!(3)).expect_err("refused"),
            "the value: not one of the allowed values"
        );
        let many: Vec<String> = (0..65).map(|n| format!("v{n}")).collect();
        assert_eq!(
            conforms(&json!({ "enum": many }), &json!("v")).expect_err("refused"),
            "the value: not one of the allowed values"
        );
    }

    #[test]
    fn an_unknown_field_is_named_with_the_fields_that_are_taken_and_the_closest_one() {
        let schema = json!({
            "type": "object",
            "properties": {
                "instance": { "type": "string" },
                "project": { "type": "string" },
                "args": {
                    "type": "object",
                    "properties": { "name": { "type": "string" } },
                    "additionalProperties": false
                }
            },
            "additionalProperties": false
        });
        // The field is named, and the fields the verb takes are listed.
        assert_eq!(
            conforms(&schema, &json!({ "project": "p", "name": "x" })).expect_err("refused"),
            "unknown field 'name'; this verb takes: args, instance, project"
        );
        // A field close to one that is taken gets the one that was meant.
        assert_eq!(
            conforms(&schema, &json!({ "instnace": "x" })).expect_err("refused"),
            "unknown field 'instnace'; this verb takes: args, instance, project; did you mean instance?"
        );
        // Inside an object, with its place.
        assert_eq!(
            conforms(&schema, &json!({ "args": { "nmae": "x" } })).expect_err("refused"),
            "/args: unknown field 'nmae'; it takes: name; did you mean name?"
        );
        // The value of the field is never repeated.
        let err = conforms(&schema, &json!({ "name": "secret-value-123" })).expect_err("refused");
        assert!(!err.contains("secret-value-123"), "{err}");
    }

    #[test]
    fn only_a_key_shaped_like_a_field_name_is_repeated() {
        // Every field name in the verb schemas is shorter than the limit; the longest is confirmSnapshotLoss.
        for key in [
            "name",
            "serviceCidr",
            "http-path",
            "plan_digest",
            "a",
            "confirmSnapshotLoss",
        ] {
            assert!(name_shaped(key), "{key}");
        }
        let long = "a".repeat(25);
        // The shapes of common service tokens are all letters, digits and `_`, so only their length tells them from a
        // field name. They are put together here, so that no line of this file holds something shaped like a token.
        let prefixed = |prefix: &str, rest: usize| format!("{prefix}{}", "A".repeat(rest));
        let (arca, github, stripe) = (
            prefixed("arca_", 43),
            prefixed("ghp_", 36),
            prefixed("sk_live_", 24),
        );
        for key in [
            "",
            "9lives",
            "has space",
            "semi;colon",
            "with/other+chars=",
            arca.as_str(),
            github.as_str(),
            stripe.as_str(),
            long.as_str(),
        ] {
            assert!(!name_shaped(key), "{key}");
            let refusal = unknown_field("", key, &["name"]);
            assert_eq!(
                refusal,
                "a field the schema does not name; this verb takes: name"
            );
        }
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
