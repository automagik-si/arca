//! Image copy uses the existing database image and merges request properties, as Incus imageDownload/imgPostRemoteInfo do.
use super::*;

type Reply = (u16, String, Value);
fn bad(code: u16, text: &str) -> Reply {
    (code, String::new(), error_envelope(code, text))
}
pub(super) fn failure(shared: &Shared, step: &str) -> bool {
    let mut held = lock(&shared.image_failure);
    if held.as_deref() != Some(step) {
        return false;
    }
    held.take();
    true
}

pub(super) fn copy(
    shared: &Shared,
    store: &mut Store,
    request: &Request,
    address: &Address,
) -> Reply {
    let Ok(body) = serde_json::from_slice::<Value>(&request.body) else {
        return bad(400, "invalid image copy");
    };
    let source = &body["source"];
    if source["type"] != "image"
        || source.get("server").is_some()
        || source.get("alias").is_some()
        || !source["fingerprint"].as_str().is_some_and(|s| {
            s.len() == 64
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
    {
        return bad(
            400,
            "the fake takes only same-daemon image copy by full fingerprint",
        );
    }
    if failure(shared, "copy") {
        return answered(
            shared,
            store,
            "image",
            "POST",
            Err((400, "copy failed".into())),
        );
    }
    let Some(home) = store.image_store(&address.project) else {
        return bad(404, "project missing");
    };
    let fingerprint = source["fingerprint"].as_str().unwrap().to_string();
    let target = Address {
        kind: "image",
        project: home.clone(),
        parent: String::new(),
        name: fingerprint.clone(),
    };
    let index = match store.find(&target) {
        Some(index) => index,
        None => {
            // Incus orders by projects.id: default first, then project creation order, not image insertion order.
            let Some(original) = store
                .objects
                .iter()
                .filter(|object| object.kind == "image" && object.name == fingerprint)
                .min_by_key(|object| {
                    if object.project == DEFAULT_PROJECT {
                        0
                    } else {
                        store
                            .objects
                            .iter()
                            .filter(|row| row.kind == "project")
                            .position(|project| project.name == object.project)
                            .map_or(usize::MAX, |index| index + 1)
                    }
                })
                .cloned()
            else {
                return bad(404, "source image missing");
            };
            let mut image = original.body.clone();
            image["project"] = json!(home);
            image["aliases"] = json!([]);
            image["cached"] = json!(false);
            image["public"] = json!(false);
            image["profiles"] = json!([]);
            image["update_source"] = Value::Null;
            store.objects.push(Stored {
                kind: "image".into(),
                project: home,
                parent: String::new(),
                name: fingerprint,
                body: image,
            });
            store.objects.len() - 1
        }
    };
    let mut properties = store.objects[index]
        .body
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if let Some(sent) = body["properties"].as_object() {
        properties.extend(sent.clone());
    }
    store.objects[index].body["properties"] = Value::Object(properties);
    // imgPostRemoteInfo updates metadata only when one of these request fields is nonempty.
    // Otherwise auto_update, filename and expires_at remain inherited from the reused record.
    if body["public"] == true
        || body["auto_update"] == true
        || body["filename"].as_str().is_some_and(|s| !s.is_empty())
        || body["properties"]
            .as_object()
            .is_some_and(|p| !p.is_empty())
        || body["profiles"].as_array().is_some_and(|p| !p.is_empty())
    {
        for key in ["public", "auto_update"] {
            store.objects[index].body[key] = body.get(key).cloned().unwrap_or(json!(false));
        }
        store.objects[index].body["filename"] = json!(body["filename"].as_str().unwrap_or(""));
        store.objects[index].body["profiles"] =
            body.get("profiles").cloned().unwrap_or(json!(["default"]));
    }
    let outcome = if failure(shared, "copy_after") {
        Err((400, "copy failed after creating its database entry".into()))
    } else {
        Ok(())
    };
    answered(shared, store, "image", "POST", outcome)
}

pub(super) fn aliases(
    shared: &Shared,
    store: &mut Store,
    request: &Request,
    project: &str,
    name: Option<&str>,
) -> Reply {
    let Some(home) = store.image_store(project) else {
        return bad(404, "project missing");
    };
    let parsed = serde_json::from_slice::<Value>(&request.body).unwrap_or(Value::Null);
    let Some(name) = name.or_else(|| parsed["name"].as_str()) else {
        return bad(400, "alias missing");
    };
    let held = store
        .objects
        .iter()
        .enumerate()
        .find_map(|(index, object)| {
            if object.kind != "image" || object.project != home {
                return None;
            }
            object.body["aliases"]
                .as_array()?
                .iter()
                .position(|alias| alias["name"] == name)
                .map(|slot| (index, slot))
        });
    match request.method.as_str() {
        "GET" => match held {
            Some((index, slot)) => {
                let alias = &store.objects[index].body["aliases"][slot];
                sync(json!({ "name": name, "target": store.objects[index].name,
                    "description": alias.get("description").and_then(Value::as_str).unwrap_or("") }))
            }
            None => bad(404, "alias missing"),
        },
        "POST" => {
            if held.is_some() {
                return bad(409, "alias already exists");
            }
            if failure(shared, "alias_taken") {
                let Some(index) = store.objects.iter().position(|object| {
                    object.kind == "image"
                        && object.project == home
                        && object.name != parsed["target"]
                }) else {
                    return bad(404, "conflicting image missing");
                };
                store.objects[index].body["aliases"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({ "name": name }));
                return bad(409, "alias was taken by another writer");
            }
            if failure(shared, "alias") {
                return bad(500, "alias creation failed");
            }
            let Some(index) = store.objects.iter().position(|object| {
                object.kind == "image" && object.project == home && object.name == parsed["target"]
            }) else {
                return bad(404, "target image missing");
            };
            store.objects[index].body["aliases"]
                .as_array_mut()
                .unwrap()
                .push(json!({ "name": name,
                "description": parsed["description"].as_str().unwrap_or("") }));
            if failure(shared, "alias_after") {
                return bad(500, "alias response failed after creation");
            }
            (201, String::new(), sync_envelope(Value::Null))
        }
        "DELETE" => match held {
            Some((index, slot)) => {
                store.objects[index].body["aliases"]
                    .as_array_mut()
                    .unwrap()
                    .remove(slot);
                sync(Value::Null)
            }
            None => bad(404, "alias missing"),
        },
        _ => bad(405, "method not allowed"),
    }
}
