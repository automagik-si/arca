//! Approving a plan from the terminal with MCP elicitation: who is asked, when, and what each answer does. The node's own
//! approval is a fake that records who it was asked to approve for, so each test sees whether a plan ran and under whom.
//!
//! `2025-11-25` is read as the specification gives it (Streamable HTTP, "Sending Messages to the Server"): the call's `POST`
//! answers `text/event-stream`, the first message after the priming event is the server's `elicitation/create` request, the
//! client answers with a second `POST` that gets `202`, and the stream ends with the call's own reply. `2026-07-28` uses
//! the multi round-trip pattern: an `input_required` result, answered by sending the call again.

mod common;

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arca_api::{Scope, verbs};
use arca_mcp::{
    CallError, Caller, PendingPlan, Server, TerminalApprover, TokenStore, ToolHandler, serve_unix,
};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use common::{
    Reply, Scratch, bearer, connect_unix, mint_into, post, post_bytes, rpc, wait_for_socket,
};
use serde_json::{Value, json};
use tokio::runtime::Runtime;

const MODERN: &str = "2026-07-28";
const VERSION_META: &str = "io.modelcontextprotocol/protocolVersion";
const CAPABILITIES_META: &str = "io.modelcontextprotocol/clientCapabilities";
const PLAN: &str = "act-0123456789ab";

/// What `action_propose` answers: one waiting proposal.
struct Propose;

impl ToolHandler for Propose {
    fn call(&self, _args: Value, _caller: &Caller) -> Result<Value, CallError> {
        Ok(json!({
            "kind": "action", "id": PLAN, "action": "service_remove",
            "summary": "Remove the service demo",
            "details": [{ "label": "Service", "value": "demo" }],
            "args": { "name": "demo" },
            "argsDigest": "a".repeat(64),
            "createdAt": 1, "expiresAt": 900001
        }))
    }
}

/// The node's approval, as a fake: it asks for the plan the proposal names and records every approval and who made it.
#[derive(Default)]
struct Approver {
    approved: Mutex<Vec<(String, String)>>,
}

impl TerminalApprover for Approver {
    fn pending(&self, verb: &str, result: &Value) -> Option<PendingPlan> {
        (verb == "action_propose" && result["kind"] == "action").then(|| PendingPlan {
            id: result["id"].as_str().unwrap_or_default().to_string(),
            message: format!(
                "Approve this plan? {}",
                result["summary"].as_str().unwrap_or("")
            ),
        })
    }

    fn approve(&self, plan: &str, caller: &Caller) -> Result<Value, CallError> {
        self.approved
            .lock()
            .expect("lock")
            .push((plan.to_string(), caller.token_id.clone()));
        Ok(
            json!({ "plan": plan, "id": plan, "action": "service_remove", "result": { "removed": true } }),
        )
    }
}

struct Fixture {
    _rt: Runtime,
    _scratch: Scratch,
    socket: PathBuf,
    admin: String,
    operator: String,
    approver: Arc<Approver>,
}

impl Fixture {
    /// A node with the approval wired (`wired`) and an elicitation wait of `wait`.
    fn start(tag: &str, wired: bool, wait: Duration) -> Fixture {
        let scratch = Scratch::new(tag);
        let admin = mint_into(scratch.path(), "admin-1", Scope::Admin);
        let operator = mint_into(scratch.path(), "op-1", Scope::Operator);
        let tokens = TokenStore::load(scratch.path()).expect("the store loads");
        let approver = Arc::new(Approver::default());
        let mut server = Server::new(verbs(), tokens).handle("action_propose", Arc::new(Propose));
        if wired {
            server = server
                .terminal_approval(approver.clone() as Arc<dyn TerminalApprover>)
                .terminal_timeout(wait);
        }
        let rt = Runtime::new().expect("runtime");
        let socket = scratch.at("arca.sock");
        let (router, served) = (server.router(), socket.clone());
        rt.spawn(async move { serve_unix(router, &served).await });
        wait_for_socket(&socket);
        Fixture {
            _rt: rt,
            _scratch: scratch,
            socket,
            admin,
            operator,
            approver,
        }
    }

    fn approvals(&self) -> Vec<(String, String)> {
        self.approver.approved.lock().expect("lock").clone()
    }

    fn post(&self, secret: &str, body: &[u8], extra: &[(String, String)]) -> Reply {
        let mut headers = vec![bearer(secret)];
        headers.extend_from_slice(extra);
        post(&mut connect_unix(&self.socket), &headers, body)
    }

    /// `initialize` as a `2025-11-25` client, declaring `capabilities`.
    fn initialize(&self, secret: &str, capabilities: Value) {
        let reply = self.post(
            secret,
            &rpc(
                json!(1),
                "initialize",
                json!({
                    "protocolVersion": "2025-11-25", "capabilities": capabilities,
                    "clientInfo": { "name": "a-test", "version": "1" }
                }),
            ),
            &[],
        );
        assert_eq!(reply.status, 200, "{}", reply.body);
    }

    /// The propose call as a legacy client sends it, read to the end.
    fn legacy_call(&self, secret: &str) -> Reply {
        self.post(secret, &rpc(json!(2), "tools/call", propose_call()), &[])
    }

    /// A modern request: its headers and its `_meta`, which declares `capabilities`.
    fn modern_call(
        &self,
        secret: &str,
        capabilities: Value,
        extra: Value,
        arguments: Value,
    ) -> Reply {
        let mut params = json!({ "name": "action_propose", "arguments": arguments });
        for (key, value) in extra.as_object().into_iter().flatten() {
            params[key] = value.clone();
        }
        params["_meta"] = json!({ VERSION_META: MODERN, CAPABILITIES_META: capabilities });
        let headers = [
            ("MCP-Protocol-Version".to_string(), MODERN.to_string()),
            ("Mcp-Method".to_string(), "tools/call".to_string()),
            ("Mcp-Name".to_string(), "action_propose".to_string()),
        ];
        self.post(secret, &rpc(json!(3), "tools/call", params), &headers)
    }
}

fn propose_args() -> Value {
    json!({ "action": "service_remove", "args": { "name": "demo" } })
}

fn propose_call() -> Value {
    json!({ "name": "action_propose", "arguments": propose_args() })
}

fn form() -> Value {
    json!({ "elicitation": {} })
}

/// The messages of an SSE body: every `data:` line that is not empty.
fn events(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str(data).ok())
        .collect()
}

/// An open `2025-11-25` call: the stream is read as it arrives, so the test can answer the request mid-call.
struct Stream {
    stream: std::os::unix::net::UnixStream,
    seen: String,
}

impl Stream {
    fn open(fixture: &Fixture, secret: &str) -> Stream {
        let mut stream = connect_unix(&fixture.socket);
        stream
            .write_all(&post_bytes(
                &[bearer(secret)],
                &rpc(json!(2), "tools/call", propose_call()),
            ))
            .expect("send the call");
        Stream {
            stream,
            seen: String::new(),
        }
    }

    /// Reads until `until` holds of what has arrived. `None` when the server closes first.
    fn read_until(&mut self, until: impl Fn(&str) -> bool) -> Option<()> {
        let mut buffer = [0u8; 4096];
        while !until(&self.seen) {
            let n = self.stream.read(&mut buffer).ok()?;
            if n == 0 {
                return None;
            }
            self.seen.push_str(&String::from_utf8_lossy(&buffer[..n]));
        }
        Some(())
    }

    /// The elicitation request the server sent, read once it has arrived.
    fn request(&mut self) -> Value {
        self.read_until(|seen| seen.contains("elicitation/create"))
            .expect("the request arrives");
        events(&self.seen)
            .into_iter()
            .find(|message| message["method"] == "elicitation/create")
            .expect("an elicitation request")
    }

    /// The call's own reply, once the stream has ended.
    fn reply(&mut self) -> Value {
        let _ = self.read_until(|_| false);
        events(&self.seen)
            .into_iter()
            .find(|message| message.get("method").is_none())
            .expect("the call's reply")
    }
}

/// The client's answer to the server's request `id`.
fn answer(fixture: &Fixture, secret: &str, id: &Value, result: Value) -> Reply {
    fixture.post(
        secret,
        &serde_json::to_vec(&json!({ "jsonrpc": "2.0", "id": id, "result": result }))
            .expect("json"),
        &[],
    )
}

#[test]
fn an_admin_whose_client_declared_elicitation_is_asked_on_the_calls_own_stream() {
    let fixture = Fixture::start("asked", true, Duration::from_secs(5));
    fixture.initialize(&fixture.admin, form());
    let mut call = Stream::open(&fixture, &fixture.admin);
    call.read_until(|seen| seen.contains("elicitation/create"))
        .expect("a stream");
    assert!(call.seen.contains("200 OK"), "{}", call.seen);
    assert!(
        call.seen
            .to_ascii_lowercase()
            .contains("content-type: text/event-stream"),
        "the call answers as an event stream: {}",
        call.seen
    );
    let request = call.request();
    assert_eq!(request["jsonrpc"], "2.0");
    assert_eq!(request["params"]["mode"], "form");
    let message = request["params"]["message"].as_str().expect("a message");
    assert!(
        message.contains("Remove the service demo"),
        "the plan is stated: {message}"
    );
    let schema = &request["params"]["requestedSchema"];
    assert_eq!(schema["type"], "object");
    assert_eq!(
        schema["properties"]["approve"]["enum"],
        json!(["yes", "no"])
    );
    assert!(
        fixture.approvals().is_empty(),
        "nothing is approved before an answer"
    );
    // Declining ends the stream with the call's normal reply.
    let id = request["id"].clone();
    assert_eq!(
        answer(
            &fixture,
            &fixture.admin,
            &id,
            json!({ "action": "decline" })
        )
        .status,
        202
    );
    let reply = call.reply();
    assert_eq!(reply["id"], 2);
    assert_eq!(reply["result"]["structuredContent"]["id"], PLAN);
}

#[test]
fn an_accept_with_yes_approves_and_runs_the_plan_for_the_admin_token() {
    let fixture = Fixture::start("accept", true, Duration::from_secs(5));
    fixture.initialize(&fixture.admin, form());
    let mut call = Stream::open(&fixture, &fixture.admin);
    let id = call.request()["id"].clone();
    let sent = answer(
        &fixture,
        &fixture.admin,
        &id,
        json!({ "action": "accept", "content": { "approve": "yes" } }),
    );
    assert_eq!(sent.status, 202, "the client's answer is accepted with 202");
    assert!(sent.body.is_empty(), "with no body");
    let reply = call.reply();
    assert_eq!(
        fixture.approvals(),
        vec![(PLAN.to_string(), "admin-1".to_string())]
    );
    let structured = &reply["result"]["structuredContent"];
    assert_eq!(structured["approvedIn"], "terminal");
    assert_eq!(structured["approvedBy"], "admin-1");
    assert_eq!(structured["result"]["removed"], true);
    assert_eq!(reply["result"]["isError"], false);
}

#[test]
fn a_decline_a_cancel_an_accept_of_no_and_a_wrong_answerer_leave_the_plan_waiting() {
    let fixture = Fixture::start("pending", true, Duration::from_secs(5));
    fixture.initialize(&fixture.admin, form());
    let cases = [
        json!({ "action": "decline" }),
        json!({ "action": "cancel" }),
        json!({ "action": "accept", "content": { "approve": "no" } }),
        json!({ "action": "accept", "content": {} }),
    ];
    for result in cases {
        let mut call = Stream::open(&fixture, &fixture.admin);
        let id = call.request()["id"].clone();
        answer(&fixture, &fixture.admin, &id, result.clone());
        let reply = call.reply();
        assert_eq!(
            reply["result"]["structuredContent"]["id"], PLAN,
            "{result}: the normal reply"
        );
        assert!(
            reply["result"]["structuredContent"]
                .get("approvedIn")
                .is_none(),
            "{result}"
        );
    }
    // Another token cannot answer the admin's question: its POST is no answer, and the plan is not approved.
    let mut call = Stream::open(&fixture, &fixture.admin);
    let id = call.request()["id"].clone();
    let stolen = answer(
        &fixture,
        &fixture.operator,
        &id,
        json!({ "action": "accept", "content": { "approve": "yes" } }),
    );
    assert_ne!(
        stolen.status, 202,
        "an answer from another token is not taken"
    );
    answer(&fixture, &fixture.admin, &id, json!({ "action": "cancel" }));
    call.reply();
    assert!(
        fixture.approvals().is_empty(),
        "no answer approved anything"
    );
}

#[test]
fn a_question_nobody_answers_times_out_and_leaves_the_plan_waiting() {
    let fixture = Fixture::start("timeout", true, Duration::from_millis(300));
    fixture.initialize(&fixture.admin, form());
    let mut call = Stream::open(&fixture, &fixture.admin);
    call.request();
    let reply = call.reply();
    assert_eq!(
        reply["result"]["structuredContent"]["id"], PLAN,
        "the normal reply"
    );
    assert!(fixture.approvals().is_empty());
}

/// What a node with no terminal approval answers: the baseline the other paths must equal, byte for byte.
fn baseline(secret_of: impl Fn(&Fixture) -> String, setup: impl Fn(&Fixture, &str)) -> String {
    let plain = Fixture::start("baseline", false, Duration::from_secs(1));
    let secret = secret_of(&plain);
    setup(&plain, &secret);
    plain.legacy_call(&secret).body
}

#[test]
fn a_client_without_elicitation_gets_the_json_response_it_always_got() {
    let plain = baseline(|f| f.admin.clone(), |f, s| f.initialize(s, json!({})));
    let fixture = Fixture::start("nocap", true, Duration::from_secs(5));
    // Declared nothing, and declared sampling only.
    for capabilities in [json!({}), json!({ "sampling": {} })] {
        fixture.initialize(&fixture.admin, capabilities);
        let reply = fixture.legacy_call(&fixture.admin);
        assert_eq!(reply.status, 200);
        assert_eq!(reply.header("content-type"), Some("application/json"));
        assert_eq!(
            reply.body, plain,
            "byte for byte the answer of a node without the feature"
        );
    }
    // Never initialized at all.
    let fresh = Fixture::start("noinit", true, Duration::from_secs(5));
    assert_eq!(fresh.legacy_call(&fresh.admin).body, plain);
    assert!(fixture.approvals().is_empty());
}

#[test]
fn a_token_below_admin_is_never_asked_even_when_its_client_declares_elicitation() {
    let plain = baseline(|f| f.operator.clone(), |f, s| f.initialize(s, form()));
    let fixture = Fixture::start("operator", true, Duration::from_secs(5));
    fixture.initialize(&fixture.operator, form());
    let reply = fixture.legacy_call(&fixture.operator);
    assert_eq!(reply.header("content-type"), Some("application/json"));
    assert_eq!(reply.body, plain);
    // Its modern request, declaring the capability in `_meta`, is answered as `complete` too.
    let modern = fixture.modern_call(&fixture.operator, form(), json!({}), propose_args());
    assert_eq!(modern.json()["result"]["resultType"], "complete");
    assert!(fixture.approvals().is_empty());
}

#[test]
fn modern_asks_with_input_required_and_a_signed_state_and_an_accept_approves() {
    let fixture = Fixture::start("modern", true, Duration::from_secs(5));
    let first = fixture.modern_call(&fixture.admin, form(), json!({}), propose_args());
    assert_eq!(first.status, 200, "{}", first.body);
    let result = &first.json()["result"];
    assert_eq!(result["resultType"], "input_required");
    let request = &result["inputRequests"]["approve"];
    assert_eq!(request["method"], "elicitation/create");
    assert!(
        request["params"]["message"]
            .as_str()
            .expect("message")
            .contains("Remove the service demo")
    );
    let state = result["requestState"]
        .as_str()
        .expect("a state")
        .to_string();
    assert!(fixture.approvals().is_empty(), "asking approves nothing");
    let retry = |state: &str, arguments: Value, response: Value| {
        fixture.modern_call(
            &fixture.admin,
            form(),
            json!({ "requestState": state, "inputResponses": { "approve": response } }),
            arguments,
        )
    };
    // A state altered in any byte is refused, and so is one carried to another call.
    let mut altered = state.clone();
    altered.insert(3, 'x');
    let yes = json!({ "action": "accept", "content": { "approve": "yes" } });
    let refused = retry(&altered, propose_args(), yes.clone()).json();
    assert_eq!(refused["error"]["code"], -32602, "{refused}");
    // A well-formed payload naming another plan, under the MAC of the real one, is refused too: the signature is what binds it.
    let (body, mac) = state.split_once('.').expect("body.mac");
    let mut payload: Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(body).expect("base64")).expect("json");
    payload["p"] = json!("act-ffffffffffff");
    let forged = format!("{}.{mac}", URL_SAFE_NO_PAD.encode(payload.to_string()));
    assert_eq!(
        retry(&forged, propose_args(), yes.clone()).json()["error"]["code"],
        -32602
    );
    let other = json!({ "action": "service_remove", "args": { "name": "other" } });
    assert_eq!(
        retry(&state, other, yes.clone()).json()["error"]["code"],
        -32602
    );
    assert!(
        fixture.approvals().is_empty(),
        "no refused state approved anything"
    );
    // A decline and a cancel give the normal result, complete, and approve nothing.
    for response in [
        json!({ "action": "decline" }),
        json!({ "action": "cancel" }),
    ] {
        let done = retry(&state, propose_args(), response).json();
        assert_eq!(done["result"]["resultType"], "complete");
        assert_eq!(done["result"]["structuredContent"]["id"], PLAN);
    }
    assert!(fixture.approvals().is_empty());
    // The accept approves.
    let done = retry(&state, propose_args(), yes).json();
    assert_eq!(done["result"]["resultType"], "complete", "{done}");
    assert_eq!(
        done["result"]["structuredContent"]["approvedIn"],
        "terminal"
    );
    assert_eq!(
        fixture.approvals(),
        vec![(PLAN.to_string(), "admin-1".to_string())]
    );
}

#[test]
fn modern_without_the_capability_in_its_meta_is_answered_as_ever() {
    let plain = {
        let f = Fixture::start("modern-plain", false, Duration::from_secs(1));
        f.modern_call(&f.admin, json!({}), json!({}), propose_args())
            .body
    };
    let fixture = Fixture::start("modern-nocap", true, Duration::from_secs(5));
    let reply = fixture.modern_call(&fixture.admin, json!({}), json!({}), propose_args());
    assert_eq!(reply.body, plain);
}

#[test]
fn an_expired_modern_state_leaves_the_plan_waiting() {
    let fixture = Fixture::start("modern-expired", true, Duration::from_secs(1));
    let first = fixture.modern_call(&fixture.admin, form(), json!({}), propose_args());
    let state = first.json()["result"]["requestState"]
        .as_str()
        .expect("state")
        .to_string();
    std::thread::sleep(Duration::from_secs(3));
    let done = fixture
        .modern_call(
            &fixture.admin,
            form(),
            json!({ "requestState": state, "inputResponses": { "approve": { "action": "accept", "content": { "approve": "yes" } } } }),
            propose_args(),
        )
        .json();
    assert_eq!(done["result"]["resultType"], "complete");
    assert_eq!(done["result"]["structuredContent"]["id"], PLAN);
    assert!(
        fixture.approvals().is_empty(),
        "an expired state approves nothing"
    );
}
