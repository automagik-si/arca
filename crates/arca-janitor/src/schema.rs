//! JSON Schemas of every job kind's parameters and result, so each kind maps one-to-one to a verb.

use serde_json::{Map, Value, json};

fn obj(props: &[(&str, Value)]) -> Value {
    let mut map = Map::new();
    for (k, v) in props {
        map.insert((*k).to_string(), v.clone());
    }
    json!({ "type": "object", "additionalProperties": false, "properties": Value::Object(map) })
}

fn int(desc: &str, default: Option<u64>) -> Value {
    match default {
        Some(d) => json!({ "type": "integer", "minimum": 0, "default": d, "description": desc }),
        None => json!({ "type": ["integer", "null"], "minimum": 0, "description": desc }),
    }
}

fn path(desc: &str) -> Value {
    json!({ "type": ["string", "null"], "description": desc })
}

fn text(desc: &str) -> Value {
    json!({ "type": "string", "description": desc })
}

fn flag(desc: &str) -> Value {
    json!({ "type": "boolean", "default": false, "description": desc })
}

fn list(desc: &str, item: Value) -> Value {
    json!({ "type": "array", "items": item, "description": desc })
}

fn loose(desc: &str) -> Value {
    json!({ "type": "object", "description": desc })
}

fn bytes(desc: &str) -> Value {
    json!({ "type": "integer", "description": desc })
}

fn dir_size() -> Value {
    loose("{path, bytes}")
}

/// The parameter schema of a kind.
pub fn params(kind: &str) -> Option<Value> {
    Some(match kind {
        "space-report" => obj(&[
            ("path", path("folder to measure; the home when null")),
            (
                "depth",
                int("levels below the folder that get their own size", Some(3)),
            ),
            (
                "top",
                int("how many of the biggest folders to list", Some(30)),
            ),
        ]),
        "growth" => obj(&[
            ("path", path("folder; the home when null")),
            (
                "older_hours",
                int(
                    "compare with the newest snapshot at least this much older",
                    Some(24),
                ),
            ),
            (
                "max_age_hours",
                int(
                    "rescan first when the newest snapshot is older than this",
                    Some(24),
                ),
            ),
            ("depth", int("depth of a fresh snapshot", Some(3))),
            ("top", int("how many folders to list", Some(20))),
        ]),
        "big-files" => obj(&[
            ("path", path("folder; the home when null")),
            ("min_mb", int("smallest size listed, in MiB", Some(500))),
            (
                "changed_hours",
                int("only files changed in the last this many hours", None),
            ),
            ("limit", int("how many files to list", Some(50))),
        ]),
        "clean-plan" => obj(&[
            (
                "profile",
                json!({ "enum": ["full", "daily"], "default": "full", "description": "daily: caches only above their caps, runner caches on Sundays" }),
            ),
            (
                "only",
                list(
                    "limit to these categories; all when empty",
                    json!({ "type": "string" }),
                ),
            ),
        ]),
        "clean-apply" => obj(&[("plan", text("the plan id clean-plan returned"))]),
        "trash-purge" => obj(&[("dry_run", flag("list, delete nothing"))]),
        "nightly" => obj(&[("dry_run", flag("plan and judge, apply nothing"))]),
        "insights" => obj(&[
            ("days", int("days of history to read", Some(30))),
            ("top", int("how many findings to list", Some(20))),
        ]),
        "probe" => obj(&[("seconds", int("how long to stay running", Some(0)))]),
        _ => return None,
    })
}

fn apply_result() -> Value {
    obj(&[
        ("plan", text("plan id")),
        (
            "dry_run",
            flag("a dry run: items are in would, nothing freed"),
        ),
        (
            "done",
            list(
                "items done and verified on disk: {id, category, label, bytes, moved_to}",
                loose("item"),
            ),
        ),
        (
            "skipped",
            list(
                "items skipped at apply: {category, label, bytes, reason}",
                loose("skip"),
            ),
        ),
        (
            "failed",
            list(
                "items that failed or could not be verified: {id, category, label, bytes, error}",
                loose("failure"),
            ),
        ),
        (
            "would",
            list("dry run only: what an apply would do", loose("item")),
        ),
        ("freed_bytes", bytes("verified bytes freed")),
        ("trash_bytes", bytes("verified bytes moved to trash")),
        ("failed_bytes", bytes("bytes of failed items, not freed")),
        ("would_free_bytes", bytes("dry run only")),
        (
            "errors",
            list("errors of the run itself", json!({ "type": "string" })),
        ),
        ("stopped", path("why the run stopped early, if it did")),
    ])
}

fn plan_summary() -> Value {
    obj(&[
        ("plan", text("plan id, for clean-apply")),
        ("profile", text("full or daily")),
        ("items", bytes("items planned")),
        (
            "skipped",
            bytes("candidates skipped, with reasons in the plan file"),
        ),
        (
            "would_free_bytes",
            bytes("bytes an apply would free; a plan frees nothing"),
        ),
        (
            "would_trash_bytes",
            bytes("bytes an apply would move to trash"),
        ),
        ("by_category", loose("category -> {items, bytes}")),
        (
            "sample",
            list("the biggest items", json!({ "type": "string" })),
        ),
    ])
}

/// The result schema of a kind.
pub fn result(kind: &str) -> Option<Value> {
    Some(match kind {
        "space-report" => obj(&[
            ("snapshot", text("snapshot id")),
            ("root", text("measured folder")),
            ("total", bytes("bytes in all")),
            ("unreadable", bytes("folders that could not be read")),
            ("biggest", list("biggest folders", dir_size())),
        ]),
        "growth" => obj(&[
            ("root", text("folder")),
            ("older", text("older snapshot id")),
            ("newer", text("newer snapshot id")),
            ("span_hours", bytes("hours between them")),
            ("total_grew", bytes("growth of the whole folder")),
            ("fresh_scan", flag("whether a new snapshot was taken")),
            (
                "grew",
                list("{path, before, after, grew, per_day}", loose("growth row")),
            ),
        ]),
        "big-files" => obj(&[
            ("root", text("folder")),
            ("min_mb", bytes("threshold")),
            ("changed_hours", int("window", None)),
            ("count", bytes("files found")),
            ("total", bytes("their bytes")),
            ("files", list("{path, bytes, modified}", loose("file"))),
        ]),
        "clean-plan" => plan_summary(),
        "clean-apply" | "trash-purge" => apply_result(),
        "nightly" => obj(&[
            ("plan", text("plan id")),
            ("dry_run", flag("nothing applied")),
            ("refused", path("why the plan was refused, if it was")),
            ("planned", plan_summary()),
            ("applied", apply_result()),
            (
                "lines",
                list("the short report", json!({ "type": "string" })),
            ),
        ]),
        "insights" => obj(&[
            ("since", bytes("epoch the history starts at")),
            ("events", bytes("history lines read")),
            (
                "findings",
                list(
                    "ranked findings",
                    obj(&[
                        ("rank", bytes("1 is the costliest")),
                        ("rule", text("the rule that found it")),
                        ("title", text("what")),
                        ("bytes", bytes("what it cost")),
                        ("occurrences", bytes("how often")),
                        (
                            "evidence",
                            list("why we think so", json!({ "type": "string" })),
                        ),
                        ("suggestion", text("the change at the source")),
                        ("score", json!({ "type": "number" })),
                    ]),
                ),
            ),
        ]),
        "probe" => obj(&[
            ("seconds", bytes("how long it ran")),
            ("priority", loose("{nice, io, lowered, detail}")),
            ("pressure", json!({ "type": ["number", "null"] })),
        ]),
        _ => return None,
    })
}

/// Every kind's schemas: `{kind: {params, result}}`.
pub fn all() -> Value {
    let mut map = Map::new();
    for kind in crate::jobs::KINDS {
        map.insert(
            kind.to_string(),
            json!({ "params": params(kind), "result": result(kind) }),
        );
    }
    Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::JobParams;

    #[test]
    fn every_kind_has_schemas_covering_its_fields() {
        for kind in crate::jobs::KINDS {
            let schema = params(kind).unwrap_or_else(|| panic!("{kind} params"));
            assert!(result(kind).is_some(), "{kind} result");
            let defaults = JobParams::from_kind(kind, json!({})).unwrap();
            let fields = serde_json::to_value(&defaults).unwrap()["params"].clone();
            let props = schema["properties"].as_object().unwrap();
            for key in fields.as_object().unwrap().keys() {
                assert!(
                    props.contains_key(key),
                    "{kind}: {key} missing from the schema"
                );
            }
            for key in props.keys() {
                assert!(
                    fields.get(key).is_some(),
                    "{kind}: schema names {key}, params lack it"
                );
            }
        }
    }
}
