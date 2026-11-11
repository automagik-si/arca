//! The MCP surface over the unix socket: the status of each refusal, the tool list per scope, the JSON-RPC framing, and the
//! results checked against the verb schemas. Every server here listens on a temporary unix socket.

mod common;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use arca_api::{Scope, Verb, VerbClass, conforms, verb, verbs};
use arca_mcp::{CallError, Caller, Server, TokenStore, ToolHandler, mint_with_tools, serve_unix};
use common::{
    Reply, Scratch, bearer, connect_unix, exchange, mint_into, post, rpc, wait_for_socket,
    write_hash_file,
};
use serde_json::{Value, json};
use tokio::runtime::Runtime;

/// The API table plus one write verb of minimum scope `doctor`, which the table itself does not yet carry.
fn table() -> &'static [Verb] {
    static TABLE: OnceLock<&'static [Verb]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut all = verbs().to_vec();
        all.push(Verb {
            name: "write_probe",
            summary: "A write verb for the scope tests.",
            class: VerbClass::L,
            min_scope: Scope::Doctor,
            writes: true,
            destructive: false,
            untrusted: false,
            output_key: None,
            summary_text: arca_api::verbs::summary_done,
            cut: arca_api::verbs::no_cut,
            params: write_probe_params,
            result: write_probe_result,
        });
        Box::leak(all.into_boxed_slice())
    })
}

fn write_probe_params() -> Value {
    json!({
        "type": "object",
        "properties": { "name": { "type": "string", "maxLength": 31 } },
        "required": ["name"],
        "additionalProperties": false
    })
}

fn write_probe_result() -> Value {
    json!({
        "type": "object",
        "properties": { "name": { "type": "string" } },
        "required": ["name"]
    })
}

fn status_result() -> Value {
    json!({
        "version": "0.1.0",
        "node": "a".repeat(64),
        "mode": "watch-only",
        "uptimeS": 12,
        "incusReachable": false,
        "appliedSeq": null,
        "metricsSource": "local",
        "metricsSourceReason": "the test names the local mode"
    })
}

/// Answers with a fixed value.
struct Canned(Value);

impl ToolHandler for Canned {
    fn call(&self, _args: Value, _caller: &Caller) -> Result<Value, CallError> {
        Ok(self.0.clone())
    }
}

/// Answers the write verb, and records every name it was asked to write, so a test can see whether a write ran.
struct WriteProbe {
    log: Arc<Mutex<Vec<String>>>,
}

impl ToolHandler for WriteProbe {
    fn call(&self, args: Value, caller: &Caller) -> Result<Value, CallError> {
        let name = args["name"].as_str().unwrap_or_default().to_string();
        self.log.lock().expect("the log lock").push(name.clone());
        match name.as_str() {
            "replayed" => Err(CallError::Refused {
                code: "Replayed".into(),
                message: "this approval was already spent".into(),
            }),
            "quoted" => Err(CallError::UntrustedRefused {
                code: "refused".into(),
                message: "a service answered 500: text it wrote".into(),
            }),
            "forbid" => Err(CallError::Forbidden {
                verb: "write_probe".into(),
            }),
            "boom" => Err(CallError::Internal(
                "a detail that must not leave the server".into(),
            )),
            "panic" => panic!("a handler fault"),
            _ => Ok(json!({ "name": name, "by": caller.token_id })),
        }
    }
}

struct Fixture {
    _rt: Runtime,
    _scratch: Scratch,
    socket: PathBuf,
    viewer: String,
    viewer_tools: String,
    doctor: String,
    operator: String,
    writes: Arc<Mutex<Vec<String>>>,
}

impl Fixture {
    fn start(tag: &str) -> Fixture {
        let scratch = Scratch::new(tag);
        let viewer = mint_into(scratch.path(), "v-viewer", Scope::Viewer);
        let tools = vec!["arca_status".to_string(), "journal_list".to_string()];
        let (viewer_tools, tools_text) = mint_with_tools(Scope::Viewer, Some(&tools));
        write_hash_file(scratch.path(), "v-tools", &tools_text, 0o400);
        let doctor = mint_into(scratch.path(), "d-doctor", Scope::Doctor);
        let operator = mint_into(scratch.path(), "o-operator", Scope::Operator);
        let tokens = TokenStore::load(scratch.path()).expect("the store loads");
        let writes = Arc::new(Mutex::new(Vec::new()));
        let server = Server::new(table(), tokens)
            .handle("arca_status", Arc::new(Canned(status_result())))
            .handle(
                "token_list",
                Arc::new(Canned(
                    json!({ "tokens": [{ "id": "v-viewer", "scope": "viewer" }] }),
                )),
            )
            .handle("watch_report", Arc::new(Canned(json!({ "oops": true }))))
            .handle(
                "write_probe",
                Arc::new(WriteProbe {
                    log: writes.clone(),
                }),
            );
        let rt = Runtime::new().expect("runtime");
        let socket = scratch.at("arca.sock");
        let (router, served) = (server.router(), socket.clone());
        rt.spawn(async move { serve_unix(router, &served).await });
        wait_for_socket(&socket);
        Fixture {
            _rt: rt,
            _scratch: scratch,
            socket,
            viewer,
            viewer_tools,
            doctor,
            operator,
            writes,
        }
    }

    /// One request with an optional bearer and any extra headers, over a fresh connection.
    fn send(&self, token: Option<&str>, extra: &[(String, String)], body: &[u8]) -> Reply {
        let mut headers: Vec<(String, String)> = token.map(bearer).into_iter().collect();
        headers.extend_from_slice(extra);
        let mut stream = connect_unix(&self.socket);
        post(&mut stream, &headers, body)
    }

    fn call(&self, token: &str, method: &str, params: Value) -> Reply {
        self.send(Some(token), &[], &rpc(json!(1), method, params))
    }

    fn tool_call(&self, token: &str, name: &str, arguments: Value) -> Reply {
        self.call(
            token,
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
    }
}

fn names_of(reply: &Reply) -> Vec<String> {
    reply.json()["result"]["tools"]
        .as_array()
        .expect("a tool list")
        .iter()
        .map(|t| t["name"].as_str().expect("a tool name").to_string())
        .collect()
}

// Authentication: 401 and 403, each on its own path.

#[test]
fn a_request_with_no_authorization_header_is_401_with_a_challenge() {
    let fx = Fixture::start("noauth");
    let reply = fx.send(None, &[], &rpc(json!(1), "ping", json!({})));
    assert_eq!(reply.status, 401);
    assert_eq!(reply.header("www-authenticate"), Some("Bearer"));
    assert!(reply.body.is_empty(), "a refusal carries no body");
}

#[test]
fn a_malformed_authorization_header_is_401() {
    let fx = Fixture::start("badauth");
    let body = rpc(json!(1), "ping", json!({}));
    let good = fx.viewer.clone();
    let cases = [
        format!("Basic {good}"),
        "Bearer".to_string(),
        "Bearer arca_short".to_string(),
        format!("Token {good}"),
        format!("Bearer  {good}"),
        good.clone(),
    ];
    for header in cases {
        let reply = fx.send(None, &[("Authorization".into(), header.clone())], &body);
        assert_eq!(reply.status, 401, "malformed authorization: {header:?}");
    }
}

#[test]
fn a_bearer_scheme_is_matched_without_regard_to_case() {
    let fx = Fixture::start("case");
    let reply = fx.send(
        None,
        &[("Authorization".into(), format!("bEARER {}", fx.viewer))],
        &rpc(json!(1), "ping", json!({})),
    );
    assert_eq!(reply.status, 200, "RFC 7235 schemes are case-insensitive");
}

#[test]
fn two_authorization_headers_are_401_even_when_one_is_good() {
    let fx = Fixture::start("twoauth");
    let reply = fx.send(
        None,
        &[bearer(&fx.operator), bearer(&fx.viewer)],
        &rpc(json!(1), "ping", json!({})),
    );
    assert_eq!(
        reply.status, 401,
        "an ambiguous credential is no credential"
    );
}

#[test]
fn an_unknown_token_of_the_right_shape_is_401() {
    let fx = Fixture::start("unknown");
    let fake = format!("arca_{}", "B".repeat(43));
    let reply = fx.call(&fake, "ping", json!({}));
    assert_eq!(reply.status, 401);
}

#[test]
fn any_origin_header_is_403_even_with_a_valid_operator_token() {
    let fx = Fixture::start("origin");
    for origin in ["https://example.com", "null", "http://localhost:7443"] {
        let reply = fx.send(
            Some(fx.operator.as_str()),
            &[("Origin".into(), origin.into())],
            &rpc(json!(1), "ping", json!({})),
        );
        assert_eq!(reply.status, 403, "an Origin of {origin} is refused");
    }
}

#[test]
fn an_origin_header_is_checked_before_the_token() {
    let fx = Fixture::start("origin-first");
    let reply = fx.send(
        None,
        &[("Origin".into(), "https://example.com".into())],
        &rpc(json!(1), "ping", json!({})),
    );
    assert_eq!(
        reply.status, 403,
        "a browser request is refused whatever it holds"
    );
}

/// A raw request on a fresh connection: `method` and `path`, with `headers` and `body`.
fn raw(fx: &Fixture, method: &str, path: &str, headers: &[(String, String)], body: &[u8]) -> Reply {
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let mut bytes = head.into_bytes();
    bytes.extend_from_slice(body);
    let mut stream = connect_unix(&fx.socket);
    exchange(&mut stream, &bytes)
}

#[test]
fn a_get_with_an_origin_is_403_not_405() {
    let fx = Fixture::start("get-origin");
    let headers = [
        bearer(&fx.viewer),
        ("Origin".into(), "https://example.com".into()),
    ];
    let reply = raw(&fx, "GET", "/mcp", &headers, b"");
    assert_eq!(
        reply.status, 403,
        "a browser GET is refused whatever its method"
    );
}

#[test]
fn an_origin_on_an_unknown_path_is_403_not_404() {
    let fx = Fixture::start("path-origin");
    let headers = [bearer(&fx.operator), ("Origin".into(), "null".into())];
    let reply = raw(&fx, "POST", "/elsewhere", &headers, b"{}");
    assert_eq!(
        reply.status, 403,
        "a browser request is refused on any path"
    );
}

#[test]
fn an_origin_is_refused_before_an_oversize_body_is_read() {
    let fx = Fixture::start("origin-body");
    let headers = [
        bearer(&fx.operator),
        ("Origin".into(), "https://example.com".into()),
    ];
    let big = vec![b'x'; 128 * 1024];
    let reply = raw(&fx, "POST", "/mcp", &headers, &big);
    assert_eq!(
        reply.status, 403,
        "the Origin refusal comes before the body limit"
    );
}

#[test]
fn a_viewer_calling_token_list_is_403() {
    let fx = Fixture::start("viewer-tl");
    let reply = fx.tool_call(&fx.viewer, "token_list", json!({}));
    assert_eq!(reply.status, 403);
}

#[test]
fn a_doctor_calling_token_list_is_403() {
    let fx = Fixture::start("doctor-tl");
    let reply = fx.tool_call(&fx.doctor, "token_list", json!({}));
    assert_eq!(reply.status, 403);
}

#[test]
fn an_operator_calling_token_list_is_200() {
    let fx = Fixture::start("operator-tl");
    let reply = fx.tool_call(&fx.operator, "token_list", json!({}));
    assert_eq!(reply.status, 200);
    assert!(reply.json()["result"]["structuredContent"]["tokens"].is_array());
}

#[test]
fn a_viewer_calling_a_write_verb_is_403_and_the_write_does_not_run() {
    let fx = Fixture::start("viewer-write");
    let reply = fx.tool_call(&fx.viewer, "write_probe", json!({ "name": "silent" }));
    assert_eq!(reply.status, 403);
    assert!(fx.writes.lock().expect("log").is_empty(), "no handler ran");
}

#[test]
fn a_doctor_calling_a_write_verb_is_200() {
    let fx = Fixture::start("doctor-write");
    let reply = fx.tool_call(&fx.doctor, "write_probe", json!({ "name": "ok" }));
    assert_eq!(reply.status, 200);
    assert_eq!(
        reply.json()["result"]["structuredContent"]["by"],
        "d-doctor"
    );
}

#[test]
fn a_viewer_calling_arca_status_gets_a_result_that_conforms() {
    let fx = Fixture::start("viewer-status");
    let reply = fx.tool_call(&fx.viewer, "arca_status", json!({}));
    assert_eq!(reply.status, 200);
    let body = reply.json();
    let result = &body["result"];
    assert_eq!(result["isError"], false);
    let structured = &result["structuredContent"];
    assert_eq!(structured["version"], "0.1.0");
    let schema = (verb("arca_status").expect("the verb").result)();
    assert_eq!(
        conforms(&schema, structured),
        Ok(()),
        "the result matches its schema"
    );
    assert!(
        result["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.starts_with("arca ")),
        "the summary line names the node"
    );
    assert_eq!(result["_meta"]["droidd/untrusted"], false);
    assert_eq!(result["_meta"].get("droidd/truncated"), None);
}

// The tool list follows the scope.

#[test]
fn a_viewer_tools_list_omits_token_list_and_every_write_verb() {
    let fx = Fixture::start("list-viewer");
    let reply = fx.call(&fx.viewer, "tools/list", json!({}));
    assert_eq!(reply.status, 200);
    // The viewer's reads of the table, in table order: the watch verbs, the read-only plan listing and the alerts.
    assert_eq!(
        names_of(&reply),
        vec![
            "arca_status",
            "node_metrics",
            "watch_report",
            "journal_list",
            "plan_list",
            "action_propose",
            "alerts_list",
            "postgres_list",
            "ontology_query",
            "service_list",
            "service_status",
            "service_logs",
            "owner_approval_list",
            "storage_list",
            "snapshot_list",
            "tenant_list",
            "tenant_status",
            "tenant_usage"
        ]
    );
}

#[test]
fn a_tools_allowlist_hides_and_refuses_every_verb_outside_it() {
    let fx = Fixture::start("tools-allow");
    let reply = fx.call(&fx.viewer_tools, "tools/list", json!({}));
    assert_eq!(names_of(&reply), vec!["arca_status", "journal_list"]);
    assert_eq!(
        fx.tool_call(&fx.viewer_tools, "arca_status", json!({}))
            .status,
        200
    );
    assert_eq!(
        fx.tool_call(&fx.viewer_tools, "node_metrics", json!({}))
            .status,
        403
    );
    assert_eq!(
        fx.tool_call(&fx.viewer_tools, "token_list", json!({}))
            .status,
        403
    );
}

#[test]
fn a_doctor_tools_list_adds_the_write_verb_but_not_token_list() {
    let fx = Fixture::start("list-doctor");
    let reply = fx.call(&fx.doctor, "tools/list", json!({}));
    let names = names_of(&reply);
    assert!(names.contains(&"write_probe".to_string()));
    assert!(names.contains(&"alerts_list".to_string()));
    assert!(!names.contains(&"token_list".to_string()));
}

#[test]
fn a_doctor_tools_list_shows_the_three_omni_tools_and_a_viewer_does_not() {
    let fx = Fixture::start("list-omni");
    let omni = ["omni_instance_create", "omni_pair", "omni_status"];
    let listed = |token: &str| -> Vec<String> {
        names_of(&fx.call(token, "tools/list", json!({})))
            .into_iter()
            .filter(|n| n.starts_with("omni_"))
            .collect()
    };
    assert_eq!(listed(&fx.doctor), omni);
    assert_eq!(listed(&fx.operator), omni);
    assert!(listed(&fx.viewer).is_empty(), "a viewer token");
    assert!(
        listed(&fx.viewer_tools).is_empty(),
        "a viewer token with a list"
    );
    // A viewer's call is refused before any handler runs; every advertised omni tool is untrusted.
    let reply = fx.tool_call(&fx.viewer, "omni_status", json!({ "name": "omni" }));
    assert_eq!(reply.status, 403);
    let tools = fx.call(&fx.doctor, "tools/list", json!({})).json()["result"]["tools"].clone();
    for tool in tools
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| omni.contains(&t["name"].as_str().unwrap()))
    {
        assert_eq!(
            tool["outputSchema"]["properties"]["untrusted"]["type"], "string",
            "{tool}"
        );
        assert_eq!(tool["annotations"]["destructiveHint"], false);
        assert_eq!(
            tool["annotations"]["readOnlyHint"],
            tool["name"] == "omni_status"
        );
    }
}

#[test]
fn owner_approval_request_and_status_are_a_doctors_and_the_list_is_a_viewers_too() {
    let fx = Fixture::start("list-owner-approval");
    let listed = |token: &str| -> Vec<String> {
        names_of(&fx.call(token, "tools/list", json!({})))
            .into_iter()
            .filter(|n| n.starts_with("owner_approval_"))
            .collect()
    };
    let all = [
        "owner_approval_request",
        "owner_approval_status",
        "owner_approval_list",
    ];
    assert_eq!(listed(&fx.doctor), all);
    assert_eq!(listed(&fx.operator), all);
    assert_eq!(listed(&fx.viewer), ["owner_approval_list"]);
    // A viewer's request or status is refused before any handler runs: a status may carry an approval string.
    for verb in ["owner_approval_request", "owner_approval_status"] {
        let reply = fx.tool_call(&fx.viewer, verb, json!({ "id": "ar_0123456789abcdef" }));
        assert_eq!(reply.status, 403, "{verb}");
    }
    let tools = fx.call(&fx.doctor, "tools/list", json!({})).json()["result"]["tools"].clone();
    for tool in tools
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| all.contains(&t["name"].as_str().unwrap()))
    {
        let name = tool["name"].as_str().unwrap();
        assert_eq!(
            tool["outputSchema"]["properties"]["untrusted"].is_object(),
            name == "owner_approval_list",
            "only the list is untrusted: {name}"
        );
        assert_eq!(
            tool["annotations"]["readOnlyHint"],
            name == "owner_approval_list",
            "{name}"
        );
        assert_eq!(tool["annotations"]["destructiveHint"], false, "{name}");
    }
}

#[test]
fn an_operator_tools_list_holds_every_verb() {
    let fx = Fixture::start("list-operator");
    let reply = fx.call(&fx.operator, "tools/list", json!({}));
    let names = names_of(&reply);
    assert_eq!(names.len(), table().len());
    assert!(names.contains(&"token_list".to_string()));
    assert!(names.contains(&"alerts_list".to_string()));
}

#[test]
fn every_verbs_openrpc_read_only_hint_is_the_served_read_only_hint() {
    let fx = Fixture::start("list-hint");
    let tools = fx.call(&fx.operator, "tools/list", json!({})).json()["result"]["tools"].clone();
    let tools = tools.as_array().expect("tools");
    let doc = arca_api::openrpc::openrpc();
    let methods = doc["methods"].as_array().expect("methods");
    assert_eq!(methods.len(), verbs().len());
    for v in verbs() {
        let method = methods
            .iter()
            .find(|m| m["name"] == v.name)
            .unwrap_or_else(|| panic!("{} is in the OpenRPC document", v.name));
        let tool = tools
            .iter()
            .find(|t| t["name"] == v.name)
            .unwrap_or_else(|| panic!("{} is in an operator's tools/list", v.name));
        assert!(
            method["x-arca-read-only-hint"].is_boolean(),
            "{} carries the hint",
            v.name
        );
        assert_eq!(
            method["x-arca-read-only-hint"], tool["annotations"]["readOnlyHint"],
            "{}: the OpenRPC hint is the served one",
            v.name
        );
    }
}

#[test]
fn a_listed_tool_carries_its_schemas_and_annotations() {
    let fx = Fixture::start("list-shape");
    let reply = fx.call(&fx.operator, "tools/list", json!({}));
    let tools = reply.json()["result"]["tools"].clone();
    let status = tools
        .as_array()
        .expect("tools")
        .iter()
        .find(|t| t["name"] == "arca_status")
        .expect("arca_status")
        .clone();
    assert_eq!(
        status["inputSchema"],
        (verb("arca_status").expect("verb").params)()
    );
    let output = &status["outputSchema"];
    assert_eq!(output["properties"]["version"], json!({ "type": "string" }));
    assert_eq!(
        output["properties"]["truncated"],
        json!({ "type": "boolean", "const": true, "description": "Present when something was cut to fit its cap." })
    );
    assert!(output["properties"].get("untrusted").is_none());
    assert_eq!(status["annotations"]["readOnlyHint"], true);
    let probe = tools
        .as_array()
        .expect("tools")
        .iter()
        .find(|t| t["name"] == "write_probe")
        .expect("write_probe")
        .clone();
    assert_eq!(probe["annotations"]["readOnlyHint"], false);
    let propose = tools
        .as_array()
        .expect("tools")
        .iter()
        .find(|t| t["name"] == "action_propose")
        .expect("action_propose")
        .clone();
    assert_eq!(
        propose["annotations"]["readOnlyHint"], false,
        "a proposal records a durable event, so it is not read-only"
    );
    for tool in tools.as_array().expect("tools") {
        assert_eq!(
            tool["outputSchema"]["properties"]["truncated"]["const"], true,
            "{} advertises the truncation marker",
            tool["name"]
        );
        assert_eq!(
            tool["outputSchema"]["additionalProperties"], true,
            "{} is advertised open",
            tool["name"]
        );
    }
}

// Routing and argument checks.

#[test]
fn an_unknown_tool_is_a_not_found_json_rpc_error() {
    let fx = Fixture::start("unknown-tool");
    let reply = fx.tool_call(&fx.operator, "no_such_verb", json!({}));
    assert_eq!(reply.status, 200);
    assert_eq!(reply.json()["error"]["message"], "not_found");
}

#[test]
fn a_verb_with_no_handler_is_a_not_found_json_rpc_error() {
    let fx = Fixture::start("no-handler");
    let reply = fx.tool_call(&fx.viewer, "node_metrics", json!({}));
    assert_eq!(reply.status, 200);
    assert_eq!(reply.json()["error"]["message"], "not_found");
}

#[test]
fn arguments_are_checked_against_the_closed_params_schema() {
    let fx = Fixture::start("args");
    let cases = [
        ("arca_status", json!({ "extra": 1 })),
        ("node_metrics", json!({ "minutes": 0 })),
        ("node_metrics", json!({ "minutes": 61 })),
        ("node_metrics", json!({ "minutes": "5" })),
        ("write_probe", json!({ "name": "x".repeat(32) })),
        ("write_probe", json!({})),
    ];
    for (name, args) in cases {
        let reply = fx.tool_call(&fx.operator, name, args.clone());
        assert_eq!(reply.status, 200, "{name} {args}");
        assert_eq!(
            reply.json()["error"]["code"],
            -32602,
            "{name} {args} is invalid params"
        );
    }
}

#[test]
fn a_result_that_breaks_its_schema_is_an_is_error_tool_result() {
    let fx = Fixture::start("bad-result");
    let reply = fx.tool_call(&fx.operator, "watch_report", json!({}));
    assert_eq!(reply.status, 200);
    let result = &reply.json()["result"];
    assert_eq!(result["isError"], true);
    assert_eq!(result["_meta"]["droidd/error"], "internal");
    assert!(
        !reply.body.contains("oops"),
        "the internal shape is not shown"
    );
}

#[test]
fn a_refusal_is_a_json_rpc_error_that_names_its_code() {
    let fx = Fixture::start("refusal");
    let reply = fx.tool_call(&fx.doctor, "write_probe", json!({ "name": "replayed" }));
    assert_eq!(reply.status, 200);
    let body = reply.json();
    assert_eq!(body["error"]["code"], -32000);
    assert_eq!(body["error"]["data"]["refusal"], "Replayed");
    assert_eq!(body["error"]["message"], "this approval was already spent");
}

#[test]
fn a_refusal_that_quotes_a_service_is_marked_untrusted_beside_its_code() {
    let fx = Fixture::start("refusal-untrusted");
    let reply = fx.tool_call(&fx.doctor, "write_probe", json!({ "name": "quoted" }));
    assert_eq!(reply.status, 200);
    let body = reply.json();
    assert_eq!(body["error"]["code"], -32000);
    assert_eq!(
        body["error"]["data"],
        json!({ "refusal": "refused", "untrusted": true })
    );
    assert_eq!(
        body["error"]["message"],
        "a service answered 500: text it wrote"
    );
    // A plain refusal carries no mark.
    let plain = fx.tool_call(&fx.doctor, "write_probe", json!({ "name": "replayed" }));
    assert_eq!(
        plain.json()["error"]["data"],
        json!({ "refusal": "Replayed" })
    );
}

#[test]
fn a_handler_forbidden_answer_is_403() {
    let fx = Fixture::start("handler-forbid");
    let reply = fx.tool_call(&fx.doctor, "write_probe", json!({ "name": "forbid" }));
    assert_eq!(reply.status, 403);
}

#[test]
fn a_handler_internal_failure_is_an_is_error_tool_result() {
    let fx = Fixture::start("handler-boom");
    let reply = fx.tool_call(&fx.doctor, "write_probe", json!({ "name": "boom" }));
    assert_eq!(reply.status, 200);
    let result = &reply.json()["result"];
    assert_eq!(result["isError"], true);
    assert_eq!(result["content"][0]["text"], "internal: the tool failed");
    assert_eq!(result["_meta"]["droidd/error"], "internal");
    assert!(
        !reply.body.contains("must not leave"),
        "the detail is not echoed"
    );
}

#[test]
fn no_reply_carries_a_secret_or_any_part_of_one() {
    let fx = Fixture::start("secrets");
    let replies = [
        fx.send(None, &[], &rpc(json!(1), "ping", json!({}))),
        fx.call(&fx.viewer, "tools/list", json!({})),
        fx.call(&fx.operator, "tools/list", json!({})),
        fx.tool_call(&fx.operator, "token_list", json!({})),
        fx.tool_call(&fx.viewer, "arca_status", json!({})),
        fx.tool_call(&fx.viewer, "token_list", json!({})),
        fx.tool_call(&fx.doctor, "write_probe", json!({ "name": "replayed" })),
        fx.tool_call(&fx.doctor, "write_probe", json!({ "name": "boom" })),
    ];
    for reply in &replies {
        for secret in [&fx.viewer, &fx.doctor, &fx.operator] {
            assert!(
                !reply.body.contains(secret.as_str()),
                "a reply carried a secret: {}",
                reply.body
            );
            assert!(
                !reply.body.contains(&secret[5..]),
                "a reply carried part of a secret"
            );
        }
    }
}

#[test]
fn a_panicking_handler_is_an_is_error_tool_result_and_the_server_still_answers() {
    let fx = Fixture::start("panic");
    let reply = fx.tool_call(&fx.doctor, "write_probe", json!({ "name": "panic" }));
    assert_eq!(reply.status, 200, "a handler fault is a tool failure");
    assert_eq!(reply.json()["result"]["isError"], true);
    let after = fx.call(&fx.viewer, "ping", json!({}));
    assert_eq!(after.status, 200, "the next request is still served");
}

#[test]
#[should_panic(expected = "the verb table has no verb named no_such_verb")]
fn a_handler_for_a_verb_the_table_lacks_is_a_wiring_error() {
    let scratch = Scratch::new("wiring");
    let tokens = TokenStore::load(scratch.path()).expect("loads");
    let _ = Server::new(table(), tokens).handle("no_such_verb", Arc::new(Canned(json!({}))));
}

#[test]
#[should_panic(expected = "is open to viewers")]
fn a_write_verb_open_to_viewers_is_a_wiring_error_at_start() {
    let scratch = Scratch::new("viewer-write-table");
    let tokens = TokenStore::load(scratch.path()).expect("loads");
    let mut all = verbs().to_vec();
    all.push(Verb {
        name: "write_for_viewers",
        summary: "A write verb that a viewer may call.",
        class: VerbClass::L,
        min_scope: Scope::Viewer,
        writes: true,
        destructive: false,
        untrusted: false,
        output_key: None,
        summary_text: arca_api::verbs::summary_done,
        cut: arca_api::verbs::no_cut,
        params: write_probe_params,
        result: write_probe_result,
    });
    let table: &'static [Verb] = Box::leak(all.into_boxed_slice());
    let _ = Server::new(table, tokens);
}

// Framing.

#[test]
fn initialize_answers_the_protocol_version_and_the_tools_capability() {
    let fx = Fixture::start("init");
    let reply = fx.call(
        &fx.viewer,
        "initialize",
        json!({ "protocolVersion": "2025-11-25" }),
    );
    assert_eq!(reply.status, 200);
    let result = &reply.json()["result"];
    assert_eq!(result["protocolVersion"], "2025-11-25");
    assert!(result["capabilities"]["tools"].is_object());
    assert_eq!(result["serverInfo"]["version"], "0.1.0");
}

#[test]
fn ping_answers_an_empty_result() {
    let fx = Fixture::start("ping");
    let reply = fx.call(&fx.viewer, "ping", json!({}));
    assert_eq!(reply.json()["result"], json!({}));
}

#[test]
fn a_notification_is_202_with_no_body() {
    let fx = Fixture::start("notify");
    let body =
        serde_json::to_vec(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .expect("json");
    let reply = fx.send(Some(fx.viewer.as_str()), &[], &body);
    assert_eq!(reply.status, 202);
    assert!(reply.body.is_empty());
}

#[test]
fn a_request_without_an_id_that_is_not_a_notification_writes_nothing() {
    let fx = Fixture::start("noid");
    let body = serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "params": { "name": "write_probe", "arguments": { "name": "silent" } }
    }))
    .expect("json");
    let reply = fx.send(Some(fx.doctor.as_str()), &[], &body);
    assert_eq!(reply.json()["error"]["code"], -32600, "an invalid request");
    assert!(
        fx.writes.lock().expect("log").is_empty(),
        "the call never ran"
    );
}

#[test]
fn a_request_without_the_jsonrpc_version_is_an_invalid_request() {
    let fx = Fixture::start("nover");
    let body = serde_json::to_vec(&json!({ "id": 1, "method": "ping" })).expect("json");
    let reply = fx.send(Some(fx.viewer.as_str()), &[], &body);
    assert_eq!(reply.json()["error"]["code"], -32600);
}

#[test]
fn a_null_id_is_an_invalid_request() {
    let fx = Fixture::start("null-id");
    let body = serde_json::to_vec(&json!({ "jsonrpc": "2.0", "id": null, "method": "ping" }))
        .expect("json");
    let reply = fx.send(Some(fx.viewer.as_str()), &[], &body);
    assert_eq!(
        reply.json()["error"]["code"],
        -32600,
        "a null id is not a valid request id"
    );
}

#[test]
fn a_body_that_is_not_json_is_a_parse_error() {
    let fx = Fixture::start("parse");
    let reply = fx.send(Some(fx.viewer.as_str()), &[], b"{not json");
    assert_eq!(reply.status, 200);
    assert_eq!(reply.json()["error"]["code"], -32700);
}

#[test]
fn a_batch_array_is_an_invalid_request() {
    let fx = Fixture::start("batch");
    let body = serde_json::to_vec(&json!([{ "jsonrpc": "2.0", "id": 1, "method": "ping" }]))
        .expect("json");
    let reply = fx.send(Some(fx.viewer.as_str()), &[], &body);
    assert_eq!(reply.json()["error"]["code"], -32600);
}

#[test]
fn an_unknown_method_is_method_not_found() {
    let fx = Fixture::start("method");
    let reply = fx.call(&fx.viewer, "resources/list", json!({}));
    assert_eq!(reply.json()["error"]["code"], -32601);
}

#[test]
fn a_body_over_the_limit_is_refused() {
    let fx = Fixture::start("oversize");
    let big = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\",\"pad\":\"{}\"}}",
        "x".repeat(128 * 1024)
    );
    let reply = fx.send(Some(fx.viewer.as_str()), &[], big.as_bytes());
    assert_eq!(reply.status, 413);
}

#[test]
fn a_get_on_the_mcp_path_is_not_served() {
    let fx = Fixture::start("get");
    let mut stream = connect_unix(&fx.socket);
    std::io::Write::write_all(
        &mut stream,
        format!("GET /mcp HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n", fx.viewer).as_bytes(),
    )
    .expect("write");
    let mut raw = Vec::new();
    let _ = std::io::Read::read_to_end(&mut stream, &mut raw);
    let text = String::from_utf8_lossy(&raw).into_owned();
    assert!(
        text.starts_with("HTTP/1.1 405"),
        "only POST is served: {}",
        text.lines().next().unwrap_or("")
    );
}
