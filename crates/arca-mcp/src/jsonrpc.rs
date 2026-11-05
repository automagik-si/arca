//! JSON-RPC 2.0 framing for the MCP methods the server answers: `initialize`, `ping`, `tools/list` and `tools/call`.
//!
//! A request without an `id` is a notification and gets no body, but only a `notifications/` method may be one. Any other
//! method without an id is refused as an invalid request, so a write never runs with no answer to its caller. An `id` is
//! a string or a number; a `null` id is invalid.

use arca_api::{API_VERSION, conforms, may_call};
use serde_json::{Value, json};

use crate::PROTOCOL_VERSION;
use crate::auth::Caller;
use crate::http::Server;
use crate::tools::CallError;

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const REFUSED: i64 = -32000;

/// Answers one request body. `Ok(None)` is a notification, which gets no body. `Err` is a refusal that is not a JSON-RPC
/// error: the HTTP layer answers it with a status.
pub async fn dispatch(
    server: &Server,
    caller: &Caller,
    body: &[u8],
) -> Result<Option<Value>, CallError> {
    let value: Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(_) => {
            return Ok(Some(error_reply(
                Value::Null,
                PARSE_ERROR,
                "parse error",
                None,
            )));
        }
    };
    let Some(request) = value.as_object() else {
        return Ok(Some(error_reply(
            Value::Null,
            INVALID_REQUEST,
            "invalid request",
            None,
        )));
    };
    let id = request.get("id").cloned();
    let id_ok = id.as_ref().is_none_or(|v| v.is_string() || v.is_number());
    let method = request.get("method").and_then(Value::as_str);
    let version_ok = request.get("jsonrpc") == Some(&json!("2.0"));
    let (true, true, Some(method)) = (version_ok, id_ok, method) else {
        return Ok(Some(error_reply(
            valid_id_or_null(&id),
            INVALID_REQUEST,
            "invalid request",
            None,
        )));
    };
    let Some(id) = id else {
        if method.starts_with("notifications/") {
            return Ok(None);
        }
        return Ok(Some(error_reply(
            Value::Null,
            INVALID_REQUEST,
            "a request needs an id",
            None,
        )));
    };
    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
    match method {
        "initialize" => Ok(Some(result_reply(id, initialize_result()))),
        "ping" => Ok(Some(result_reply(id, json!({})))),
        "tools/list" => Ok(Some(result_reply(
            id,
            json!({ "tools": tools_for(server, caller) }),
        ))),
        "tools/call" => call_tool(server, caller, id, &params).await,
        _ => Ok(Some(error_reply(
            id,
            METHOD_NOT_FOUND,
            "method not found",
            None,
        ))),
    }
}

fn valid_id_or_null(id: &Option<Value>) -> Value {
    match id {
        Some(v) if v.is_string() || v.is_number() => v.clone(),
        _ => Value::Null,
    }
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "arca", "version": API_VERSION }
    })
}

/// The tools the caller may call, in table order. A write verb is absent for a viewer, and `token_list` for anything
/// below an operator.
fn tools_for(server: &Server, caller: &Caller) -> Vec<Value> {
    server
        .verbs
        .iter()
        .filter(|v| may_call(caller.scope, v) && caller.allows_tool(v.name))
        .map(|v| {
            json!({
                "name": v.name,
                "description": v.summary,
                "inputSchema": (v.params)(),
                "outputSchema": (v.result)(),
                "annotations": { "readOnlyHint": !v.writes, "destructiveHint": v.destructive }
            })
        })
        .collect()
}

async fn call_tool(
    server: &Server,
    caller: &Caller,
    id: Value,
    params: &Value,
) -> Result<Option<Value>, CallError> {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return Ok(Some(error_reply(
            id,
            INVALID_PARAMS,
            "a tool call names its tool",
            None,
        )));
    };
    let Some(verb) = server.verbs.iter().find(|v| v.name == name) else {
        return Ok(Some(error_reply(id, METHOD_NOT_FOUND, "not_found", None)));
    };
    if !may_call(caller.scope, verb) || !caller.allows_tool(verb.name) {
        return Err(CallError::Forbidden {
            verb: name.to_string(),
        });
    }
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if let Err(why) = conforms(&(verb.params)(), &arguments) {
        return Ok(Some(error_reply(id, INVALID_PARAMS, &why, None)));
    }
    let Some(handler) = server.handlers.get(name).cloned() else {
        return Ok(Some(error_reply(id, METHOD_NOT_FOUND, "not_found", None)));
    };
    let owned_caller = caller.clone();
    let outcome = tokio::task::spawn_blocking(move || handler.call(arguments, &owned_caller)).await;
    let result = outcome.map_err(|_| CallError::Internal("the handler did not finish".into()))?;
    match result {
        Ok(value) => {
            if conforms(&(verb.result)(), &value).is_err() {
                return Err(CallError::Internal("a result broke its schema".into()));
            }
            Ok(Some(result_reply(id, tool_result(&value))))
        }
        Err(CallError::NotFound) => Ok(Some(error_reply(id, METHOD_NOT_FOUND, "not_found", None))),
        Err(CallError::Invalid(why)) => Ok(Some(error_reply(id, INVALID_PARAMS, &why, None))),
        Err(CallError::Refused { code, message }) => Ok(Some(error_reply(
            id,
            REFUSED,
            &message,
            Some(json!({ "refusal": code })),
        ))),
        Err(other) => Err(other),
    }
}

/// An MCP tool result: the value as text, and as structured content, which the verb's output schema checks.
fn tool_result(value: &Value) -> Value {
    json!({
        "content": [{ "type": "text", "text": value.to_string() }],
        "structuredContent": value,
        "isError": false
    })
}

fn result_reply(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_reply(id: Value, code: i64, message: &str, data: Option<Value>) -> Value {
    let mut error = json!({ "code": code, "message": message });
    if let Some(data) = data {
        error["data"] = data;
    }
    json!({ "jsonrpc": "2.0", "id": id, "error": error })
}
