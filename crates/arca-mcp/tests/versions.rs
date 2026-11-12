//! The two protocol versions on the one route: a `2026-07-28` request that declares itself, a `2025-11-25` client that
//! opens with `initialize`, and a client that sends neither a version header nor `initialize`. The last two get from
//! `initialize`, `ping`, `tools/list` and `tools/call` what they got before the server knew `2026-07-28`: the same fields
//! and none added. What did change for them is pinned here too: `server/discover` answers, and a version the server
//! does not know is refused.

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use arca_api::{Scope, verbs};
use arca_mcp::{CallError, Caller, Server, TokenStore, ToolHandler, serve_unix};
use common::{Reply, Scratch, bearer, connect_unix, mint_into, post, rpc, wait_for_socket};
use serde_json::{Value, json};
use tokio::runtime::Runtime;

const MODERN: &str = "2026-07-28";
const LEGACY: &str = "2025-11-25";
const VERSION_META: &str = "io.modelcontextprotocol/protocolVersion";
const CAPABILITIES_META: &str = "io.modelcontextprotocol/clientCapabilities";
const SERVER_INFO_META: &str = "io.modelcontextprotocol/serverInfo";

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

/// Answers `arca_status`, and counts its calls, so a test can see that a refused request never reached the tool.
struct Counted(Arc<AtomicUsize>);

impl ToolHandler for Counted {
    fn call(&self, _args: Value, _caller: &Caller) -> Result<Value, CallError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(status_result())
    }
}

struct Fixture {
    _rt: Runtime,
    _scratch: Scratch,
    socket: PathBuf,
    viewer: String,
    calls: Arc<AtomicUsize>,
}

fn header(name: &str, value: &str) -> (String, String) {
    (name.to_string(), value.to_string())
}

/// The `_meta` every `2026-07-28` request carries.
fn meta() -> Value {
    json!({
        VERSION_META: MODERN,
        "io.modelcontextprotocol/clientInfo": { "name": "a-test", "version": "1" },
        CAPABILITIES_META: {}
    })
}

impl Fixture {
    fn start(tag: &str) -> Fixture {
        let scratch = Scratch::new(tag);
        let viewer = mint_into(scratch.path(), "v-viewer", Scope::Viewer);
        let tokens = TokenStore::load(scratch.path()).expect("the store loads");
        let calls = Arc::new(AtomicUsize::new(0));
        let server =
            Server::new(verbs(), tokens).handle("arca_status", Arc::new(Counted(calls.clone())));
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
            calls,
        }
    }

    /// One request by the viewer with the headers `extra`, over a fresh connection.
    fn send(&self, extra: &[(String, String)], method: &str, params: Value) -> Reply {
        let mut headers = vec![bearer(&self.viewer)];
        headers.extend_from_slice(extra);
        let mut stream = connect_unix(&self.socket);
        post(&mut stream, &headers, &rpc(json!(7), method, params))
    }

    /// A request with no version header, as a client that never heard of one sends it.
    fn bare(&self, method: &str, params: Value) -> Reply {
        self.send(&[], method, params)
    }

    /// A complete `2026-07-28` request: the three headers, and the `_meta` in the body.
    fn modern(&self, method: &str, mut params: Value) -> Reply {
        let mut headers = vec![
            header("MCP-Protocol-Version", MODERN),
            header("Mcp-Method", method),
        ];
        if let Some(name) = params.get("name").and_then(Value::as_str) {
            headers.push(header("Mcp-Name", name));
        }
        params["_meta"] = meta();
        self.send(&headers, method, params)
    }

    fn ran(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

fn status_call() -> Value {
    json!({ "name": "arca_status", "arguments": {} })
}

fn keys(value: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = value
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

/// A refusal of a request before its method is looked at: `400`, the JSON-RPC error `code`, and the request's own id.
fn refused(reply: &Reply, code: i64) -> Value {
    assert_eq!(reply.status, 400, "{}", reply.body);
    let body = reply.json();
    assert_eq!(body["error"]["code"], code, "{}", reply.body);
    assert_eq!(body["id"], 7, "the refusal answers the request it refuses");
    body["error"].clone()
}

// Discovery.

fn assert_discovery(reply: &Reply) {
    assert_eq!(reply.status, 200, "{}", reply.body);
    let result = &reply.json()["result"];
    assert_eq!(result["supportedVersions"], json!([MODERN, LEGACY]));
    assert_eq!(result["capabilities"]["tools"]["listChanged"], false);
    assert_eq!(result["resultType"], "complete");
    // The release the binary is: the workspace's version, which `arca --version` prints too. Named on its own, and
    // beside every modern result's `_meta`.
    assert_eq!(
        result["serverInfo"],
        json!({ "name": "arca", "version": env!("CARGO_PKG_VERSION") })
    );
    assert_eq!(result["_meta"][SERVER_INFO_META], result["serverInfo"]);
    assert_eq!(result["ttlMs"], 300_000);
    assert_eq!(result["cacheScope"], "public");
}

#[test]
fn discover_answers_a_modern_request() {
    let fx = Fixture::start("discover-modern");
    assert_discovery(&fx.modern("server/discover", json!({})));
}

#[test]
fn discover_answers_a_request_that_declares_no_version() {
    let fx = Fixture::start("discover-bare");
    assert_discovery(&fx.bare("server/discover", json!({})));
    assert_discovery(&fx.send(
        &[header("MCP-Protocol-Version", LEGACY)],
        "server/discover",
        json!({}),
    ));
}

#[test]
fn discover_needs_a_token_like_every_request() {
    let fx = Fixture::start("discover-noauth");
    let mut stream = connect_unix(&fx.socket);
    let reply = post(
        &mut stream,
        &[],
        &rpc(json!(1), "server/discover", json!({})),
    );
    assert_eq!(reply.status, 401);
}

#[test]
fn a_request_with_no_token_is_401_whatever_version_it_declares() {
    let fx = Fixture::start("version-noauth");
    let mut call = status_call();
    call["_meta"] = meta();
    let cases = [
        // A complete 2026-07-28 call, its name encoded.
        (
            vec![
                header("MCP-Protocol-Version", MODERN),
                header("Mcp-Method", "tools/call"),
                header("Mcp-Name", "=?base64?YXJjYV9zdGF0dXM=?="),
            ],
            call,
        ),
        // Requests the server refuses with 400 once it knows the caller.
        (
            vec![header("MCP-Protocol-Version", "1900-01-01")],
            status_call(),
        ),
        (vec![header("MCP-Protocol-Version", MODERN)], status_call()),
    ];
    for (headers, params) in cases {
        let mut stream = connect_unix(&fx.socket);
        let reply = post(&mut stream, &headers, &rpc(json!(1), "tools/call", params));
        assert_eq!(reply.status, 401, "{headers:?}");
        assert_eq!(reply.header("www-authenticate"), Some("Bearer"));
        assert!(
            reply.body.is_empty(),
            "the version is not looked at before the token: {}",
            reply.body
        );
    }
    assert_eq!(fx.ran(), 0);
}

// A 2026-07-28 request.

#[test]
fn a_modern_tools_list_holds_the_same_tools_and_the_cache_hints() {
    let fx = Fixture::start("modern-list");
    let reply = fx.modern("tools/list", json!({}));
    assert_eq!(reply.status, 200, "{}", reply.body);
    let result = reply.json()["result"].clone();
    assert_eq!(result["resultType"], "complete");
    assert_eq!(result["ttlMs"], 300_000);
    assert_eq!(
        result["cacheScope"], "private",
        "the list depends on the token"
    );
    assert_eq!(result["_meta"][SERVER_INFO_META]["name"], "arca");
    let legacy = fx.bare("tools/list", json!({})).json()["result"]["tools"].clone();
    assert!(!legacy.as_array().expect("tools").is_empty());
    assert_eq!(
        result["tools"], legacy,
        "no tool differs between the versions"
    );
}

#[test]
fn a_modern_tool_call_returns_the_same_result_with_its_type_and_the_servers_name() {
    let fx = Fixture::start("modern-call");
    let reply = fx.modern("tools/call", status_call());
    assert_eq!(reply.status, 200, "{}", reply.body);
    let modern = reply.json()["result"].clone();
    let legacy = fx.bare("tools/call", status_call()).json()["result"].clone();
    assert_eq!(modern["resultType"], "complete");
    assert_eq!(modern["_meta"][SERVER_INFO_META]["name"], "arca");
    for field in ["content", "structuredContent", "isError"] {
        assert_eq!(modern[field], legacy[field], "{field}");
    }
    assert_eq!(
        modern["_meta"]["droidd/untrusted"],
        legacy["_meta"]["droidd/untrusted"]
    );
    assert_eq!(fx.ran(), 2);
}

#[test]
fn a_modern_call_is_held_to_the_tokens_scope() {
    let fx = Fixture::start("modern-scope");
    let reply = fx.modern(
        "tools/call",
        json!({ "name": "token_list", "arguments": {} }),
    );
    assert_eq!(reply.status, 403, "a viewer may not list tokens");
}

#[test]
fn a_request_with_only_the_modern_header_is_400_and_names_all_it_lacks() {
    let fx = Fixture::start("header-only");
    let reply = fx.send(
        &[header("MCP-Protocol-Version", MODERN)],
        "tools/list",
        json!({}),
    );
    let error = refused(&reply, -32020);
    let message = error["message"].as_str().expect("a message");
    for part in [
        "the Mcp-Method header is missing",
        VERSION_META,
        CAPABILITIES_META,
    ] {
        assert!(message.contains(part), "{message}");
    }
}

#[test]
fn a_modern_request_without_its_meta_is_400_invalid_params() {
    let fx = Fixture::start("no-meta");
    let headers = [
        header("MCP-Protocol-Version", MODERN),
        header("Mcp-Method", "tools/list"),
    ];
    let error = refused(&fx.send(&headers, "tools/list", json!({})), -32602);
    assert!(
        error["message"]
            .as_str()
            .expect("a message")
            .contains(VERSION_META)
    );
    // The version alone is not enough: the capabilities are required too.
    let half = json!({ "_meta": { VERSION_META: MODERN } });
    let error = refused(&fx.send(&headers, "tools/list", half), -32602);
    assert!(
        error["message"]
            .as_str()
            .expect("a message")
            .contains(CAPABILITIES_META)
    );
}

#[test]
fn a_modern_body_without_the_version_header_is_400_header_mismatch() {
    let fx = Fixture::start("no-header");
    let reply = fx.send(
        &[header("Mcp-Method", "tools/list")],
        "tools/list",
        json!({ "_meta": meta() }),
    );
    refused(&reply, -32020);
}

#[test]
fn a_header_and_a_body_that_name_different_versions_are_400_header_mismatch() {
    let fx = Fixture::start("disagree");
    let reply = fx.send(
        &[
            header("MCP-Protocol-Version", LEGACY),
            header("Mcp-Method", "tools/list"),
        ],
        "tools/list",
        json!({ "_meta": meta() }),
    );
    refused(&reply, -32020);
}

#[test]
fn a_version_header_sent_twice_is_400_header_mismatch() {
    let fx = Fixture::start("twice");
    let reply = fx.send(
        &[
            header("MCP-Protocol-Version", MODERN),
            header("MCP-Protocol-Version", LEGACY),
        ],
        "tools/list",
        json!({}),
    );
    refused(&reply, -32020);
}

#[test]
fn an_unknown_version_is_400_with_the_supported_list() {
    let fx = Fixture::start("unknown-version");
    for method in ["tools/list", "ping", "server/discover"] {
        let reply = fx.send(
            &[header("MCP-Protocol-Version", "1900-01-01")],
            method,
            json!({}),
        );
        let error = refused(&reply, -32022);
        assert_eq!(
            error["data"],
            json!({ "supported": [MODERN, LEGACY], "requested": "1900-01-01" }),
            "{method}"
        );
    }
}

#[test]
fn a_tool_call_whose_name_header_is_missing_or_wrong_is_400_and_never_runs() {
    let fx = Fixture::start("name-header");
    let mut params = status_call();
    params["_meta"] = meta();
    let base = [
        header("MCP-Protocol-Version", MODERN),
        header("Mcp-Method", "tools/call"),
    ];
    refused(&fx.send(&base, "tools/call", params.clone()), -32020);
    let mut wrong = base.to_vec();
    wrong.push(header("Mcp-Name", "token_list"));
    refused(&fx.send(&wrong, "tools/call", params.clone()), -32020);
    let mut other_method = vec![
        header("MCP-Protocol-Version", MODERN),
        header("Mcp-Method", "tools/list"),
        header("Mcp-Name", "arca_status"),
    ];
    refused(
        &fx.send(&other_method, "tools/call", params.clone()),
        -32020,
    );
    assert_eq!(fx.ran(), 0, "a refused call never reaches its tool");

    // The same call with the name encoded, as a client sends a name that is not plain ASCII.
    other_method[1] = header("Mcp-Method", "tools/call");
    other_method[2] = header("Mcp-Name", "=?base64?YXJjYV9zdGF0dXM=?=");
    let reply = fx.send(&other_method, "tools/call", params);
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(fx.ran(), 1);
}

#[test]
fn an_encoded_method_header_is_400_header_mismatch() {
    let fx = Fixture::start("encoded-method");
    // base64 of `tools/list`. Only a tool's name may be sent encoded.
    let headers = [
        header("MCP-Protocol-Version", MODERN),
        header("Mcp-Method", "=?base64?dG9vbHMvbGlzdA==?="),
    ];
    let reply = fx.send(&headers, "tools/list", json!({ "_meta": meta() }));
    refused(&reply, -32020);
}

#[test]
fn a_version_named_only_in_the_body_is_judged_too() {
    let fx = Fixture::start("body-version");
    let unknown = json!({ "_meta": { VERSION_META: "1900-01-01" } });
    let error = refused(&fx.bare("tools/list", unknown), -32022);
    assert_eq!(error["data"]["requested"], "1900-01-01");
    let not_text = json!({ "_meta": { VERSION_META: 20260728 } });
    refused(&fx.bare("tools/list", not_text), -32602);
}

#[test]
fn the_version_an_error_says_back_is_cut_short() {
    let fx = Fixture::start("long-version");
    let reply = fx.send(
        &[header("MCP-Protocol-Version", &"9".repeat(500))],
        "tools/list",
        json!({}),
    );
    let error = refused(&reply, -32022);
    assert_eq!(error["data"]["requested"], "9".repeat(64));
}

#[test]
fn a_modern_request_for_a_method_the_server_lacks_is_404_method_not_found() {
    let fx = Fixture::start("modern-404");
    // `ping` left the protocol with 2026-07-28, so there it is one more unknown method.
    for method in ["resources/list", "subscriptions/listen", "ping"] {
        let reply = fx.modern(method, json!({}));
        assert_eq!(reply.status, 404, "{method}");
        assert_eq!(reply.json()["error"]["code"], -32601, "{method}");
        assert_eq!(reply.json()["id"], 7);
    }
}

#[test]
fn an_unknown_tool_is_invalid_params_for_a_modern_request_and_unchanged_for_any_other() {
    let fx = Fixture::start("unknown-tool");
    let call = json!({ "name": "no_such_verb", "arguments": {} });
    let modern = fx.modern("tools/call", call.clone());
    assert_eq!(modern.status, 200);
    assert_eq!(modern.json()["error"]["code"], -32602);
    assert_eq!(modern.json()["error"]["message"], "not_found");
    let legacy = fx.bare("tools/call", call);
    assert_eq!(legacy.status, 200);
    assert_eq!(legacy.json()["error"]["code"], -32601);
    assert_eq!(legacy.json()["error"]["message"], "not_found");
}

// A 2025-11-25 client, and a client that declares nothing.

/// A tool list with the fields the server gave it before it knew `2026-07-28`, and none added.
fn assert_legacy_list(reply: &Reply) {
    assert_eq!(reply.status, 200, "{}", reply.body);
    let body = reply.json();
    assert_eq!(keys(&body), ["id", "jsonrpc", "result"]);
    assert_eq!(keys(&body["result"]), ["tools"]);
    let tool = &body["result"]["tools"][0];
    assert_eq!(
        keys(tool),
        [
            "annotations",
            "description",
            "inputSchema",
            "name",
            "outputSchema"
        ]
    );
}

fn assert_legacy_call(reply: &Reply) {
    assert_eq!(reply.status, 200, "{}", reply.body);
    let result = &reply.json()["result"];
    assert_eq!(
        keys(result),
        ["_meta", "content", "isError", "structuredContent"]
    );
    assert_eq!(keys(&result["_meta"]), ["droidd/untrusted"]);
    assert_eq!(result["structuredContent"]["mode"], "watch-only");
    assert_eq!(result["isError"], false);
}

#[test]
fn a_client_that_sends_neither_a_version_header_nor_initialize_is_served_as_before() {
    let fx = Fixture::start("neither");
    assert_legacy_list(&fx.bare("tools/list", json!({})));
    assert_legacy_call(&fx.bare("tools/call", status_call()));
    assert_eq!(fx.bare("ping", json!({})).json()["result"], json!({}));
    let unknown = fx.bare("resources/list", json!({}));
    assert_eq!(
        unknown.status, 200,
        "an unknown method stays inside a 200 for this client"
    );
    assert_eq!(unknown.json()["error"]["code"], -32601);
}

#[test]
fn a_2025_11_25_client_is_served_as_before_from_initialize_on() {
    let fx = Fixture::start("legacy");
    let version = [header("MCP-Protocol-Version", LEGACY)];
    let opened = fx.bare(
        "initialize",
        json!({
            "protocolVersion": LEGACY,
            "capabilities": {},
            "clientInfo": { "name": "a-test", "version": "1" }
        }),
    );
    assert_eq!(opened.status, 200);
    let result = &opened.json()["result"];
    assert_eq!(
        keys(result),
        ["capabilities", "protocolVersion", "serverInfo"]
    );
    assert_eq!(result["protocolVersion"], LEGACY);
    assert_eq!(result["capabilities"]["tools"]["listChanged"], false);
    assert_eq!(result["serverInfo"]["name"], "arca");
    assert_eq!(result["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));

    let mut headers = vec![bearer(&fx.viewer)];
    headers.extend_from_slice(&version);
    let notified =
        serde_json::to_vec(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .expect("json");
    let reply = post(&mut connect_unix(&fx.socket), &headers, &notified);
    assert_eq!(reply.status, 202);
    assert!(reply.body.is_empty());

    let list = fx.send(&version, "tools/list", json!({}));
    assert_legacy_list(&list);
    assert_eq!(
        list.body,
        fx.bare("tools/list", json!({})).body,
        "the header changes nothing"
    );
    let call = fx.send(&version, "tools/call", status_call());
    assert_legacy_call(&call);
    assert_eq!(call.body, fx.bare("tools/call", status_call()).body);
    assert_eq!(
        fx.send(&version, "ping", json!({})).json()["result"],
        json!({})
    );
}

#[test]
fn initialize_answers_2025_11_25_whatever_version_it_asks_for() {
    let fx = Fixture::start("init-modern");
    // `2026-07-28` has no handshake, so a client that opens with one is a legacy client and is told the legacy version.
    for headers in [vec![], vec![header("MCP-Protocol-Version", MODERN)]] {
        let reply = fx.send(&headers, "initialize", json!({ "protocolVersion": MODERN }));
        assert_eq!(reply.status, 200, "{}", reply.body);
        assert_eq!(reply.json()["result"]["protocolVersion"], LEGACY);
    }
}

#[test]
fn the_two_versions_before_2025_11_25_are_still_served_by_their_header() {
    let fx = Fixture::start("older");
    for version in ["2025-06-18", "2025-03-26"] {
        let reply = fx.send(
            &[header("MCP-Protocol-Version", version)],
            "tools/list",
            json!({}),
        );
        assert_legacy_list(&reply);
    }
}

#[test]
fn a_session_id_from_a_client_is_ignored_and_none_is_ever_issued() {
    let fx = Fixture::start("session");
    let session = header("Mcp-Session-Id", "0123456789abcdef");
    let legacy = fx.send(std::slice::from_ref(&session), "tools/list", json!({}));
    assert_legacy_list(&legacy);
    let opened = fx.bare("initialize", json!({ "protocolVersion": LEGACY }));
    let headers = [
        header("MCP-Protocol-Version", MODERN),
        header("Mcp-Method", "tools/list"),
        session,
    ];
    let modern = fx.send(&headers, "tools/list", json!({ "_meta": meta() }));
    assert_eq!(modern.status, 200, "{}", modern.body);
    for reply in [&legacy, &opened, &modern] {
        assert_eq!(reply.header("mcp-session-id"), None);
    }
}
