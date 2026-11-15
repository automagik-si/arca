//! The owner's own signed-in session is asked to approve: an issued token the node says may be asked
//! ([`TerminalApprover::may_ask`]). `owner_approval_pending` lists several plans and the session is asked about each, on
//! the call's stream (`2025-11-25`) or with one input request per plan (`2026-07-28`). A session whose passkey is too old
//! is sent to the step-up page first (URL mode). The node's side is a fake: it says whether the session may be asked, and
//! records every approval with the caller it was made for.

mod common;

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arca_api::{Scope, conforms, verb, verbs};
use arca_mcp::{
    Asking, CallError, Caller, IssuedTokens, PendingPlan, Server, TerminalApprover, TokenStore,
    ToolHandler, serve_unix,
};
use common::{Reply, Scratch, bearer, connect_unix, post, post_bytes, rpc, wait_for_socket};
use serde_json::{Value, json};
use tokio::runtime::Runtime;

const MODERN: &str = "2026-07-28";
const VERSION_META: &str = "io.modelcontextprotocol/protocolVersion";
const CAPABILITIES_META: &str = "io.modelcontextprotocol/clientCapabilities";
const SESSION: &str = "arca_at_session-token-of-the-owners-signed-in-app";
const SESSION_ID: &str = "oauth:mcpc-0123/g-0123";
const FIRST: &str = "act-000000000001";
const SECOND: &str = "act-000000000002";
const STEP_UP: &str = "https://box1.example.com/oauth/step-up?grant=g-0123";

/// The issued token of the owner's session: a viewer, as `approvals` alone grants.
struct Issued;

impl IssuedTokens for Issued {
    fn lookup(&self, bearer: &str) -> Option<Caller> {
        (bearer == SESSION).then(|| Caller {
            token_id: SESSION_ID.to_string(),
            scope: Scope::Viewer,
            tools: None,
        })
    }
}

/// `owner_approval_pending`, as a fake: two action plans the session may be asked about, and one it may not.
struct Pending;

fn listing() -> Value {
    let plan = |id: &str, created: u64, asked: bool| {
        json!({
            "id": id, "kind": "action", "action": "instance_stop",
            "summary": format!("Stop the instance {id}"),
            "details": [{ "label": "Instance", "value": id }],
            "createdAt": created, "expiresAt": created + 900_000, "askedHere": asked
        })
    };
    let mut kept = plan("act-000000000003", 3, false);
    kept["action"] = json!("service_remove");
    kept["why"] =
        json!("it deletes or erases: it waits for the owner's passkey on the approval page");
    json!({
        "plans": [plan(FIRST, 1, true), plan(SECOND, 2, true), kept],
        "session": "asks",
        "page": "https://box1.example.com/approve"
    })
}

impl ToolHandler for Pending {
    fn call(&self, _args: Value, _caller: &Caller) -> Result<Value, CallError> {
        Ok(listing())
    }
}

/// The node's approval, as a fake. `mode` is what `may_ask` answers: 0 never, 1 yes, 2 step up first.
#[derive(Default)]
struct Approver {
    mode: AtomicU8,
    approved: Mutex<Vec<(String, u64, String)>>,
    declined: Mutex<Vec<(String, u64, String)>>,
}

impl TerminalApprover for Approver {
    fn may_ask(&self, caller: &Caller) -> Asking {
        if caller.token_id != SESSION_ID {
            return Asking::No;
        }
        match self.mode.load(Ordering::SeqCst) {
            1 => Asking::Yes,
            2 => Asking::StepUp {
                url: STEP_UP.to_string(),
            },
            _ => Asking::No,
        }
    }

    fn pending(&self, _verb: &str, _result: &Value, _caller: &Caller) -> Option<PendingPlan> {
        None
    }

    fn waiting(&self, verb: &str, result: &Value, _caller: &Caller) -> Vec<PendingPlan> {
        if verb != "owner_approval_pending" {
            return Vec::new();
        }
        result["plans"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|plan| plan["askedHere"] == true)
            .map(|plan| PendingPlan {
                id: plan["id"].as_str().unwrap_or_default().to_string(),
                message: format!(
                    "Approve this plan? {}",
                    plan["summary"].as_str().unwrap_or("")
                ),
                created_at: plan["createdAt"].as_u64().unwrap_or_default(),
            })
            .collect()
    }

    fn approve(&self, plan: &str, created_at: u64, caller: &Caller) -> Result<Value, CallError> {
        if self.may_ask(caller) != Asking::Yes {
            return Err(CallError::Forbidden {
                verb: "owner_approval_pending".to_string(),
            });
        }
        self.approved.lock().expect("lock").push((
            plan.to_string(),
            created_at,
            caller.token_id.clone(),
        ));
        Ok(json!({ "plan": plan, "result": { "stopped": true } }))
    }

    fn decline(&self, plan: &str, created_at: u64, caller: &Caller) -> bool {
        if self.may_ask(caller) != Asking::Yes {
            return false;
        }
        self.declined.lock().expect("lock").push((
            plan.to_string(),
            created_at,
            caller.token_id.clone(),
        ));
        true
    }
}

struct Fixture {
    _rt: Runtime,
    _scratch: Scratch,
    socket: PathBuf,
    approver: Arc<Approver>,
}

impl Fixture {
    fn start(tag: &str, mode: u8) -> Fixture {
        let scratch = Scratch::new(tag);
        let tokens = TokenStore::load(scratch.path()).expect("the store loads");
        let approver = Arc::new(Approver::default());
        approver.mode.store(mode, Ordering::SeqCst);
        let server = Server::new(verbs(), tokens)
            .handle("owner_approval_pending", Arc::new(Pending))
            .oauth(
                Arc::new(Issued),
                "https://box1.example.com/.well-known/oauth-protected-resource/mcp".to_string(),
            )
            .terminal_approval(approver.clone() as Arc<dyn TerminalApprover>)
            .terminal_timeout(Duration::from_secs(5));
        let rt = Runtime::new().expect("runtime");
        let socket = scratch.at("arca.sock");
        let (router, served) = (server.router(), socket.clone());
        rt.spawn(async move { serve_unix(router, &served).await });
        wait_for_socket(&socket);
        Fixture {
            _rt: rt,
            _scratch: scratch,
            socket,
            approver,
        }
    }

    fn approvals(&self) -> Vec<(String, u64, String)> {
        self.approver.approved.lock().expect("lock").clone()
    }

    fn declines(&self) -> Vec<(String, u64, String)> {
        self.approver.declined.lock().expect("lock").clone()
    }

    fn post(&self, body: &[u8], extra: &[(String, String)]) -> Reply {
        let mut headers = vec![bearer(SESSION)];
        headers.extend_from_slice(extra);
        post(&mut connect_unix(&self.socket), &headers, body)
    }

    fn initialize(&self, capabilities: Value) {
        let reply = self.post(
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

    fn answer(&self, id: &Value, result: Value) -> Reply {
        self.post(
            &serde_json::to_vec(&json!({ "jsonrpc": "2.0", "id": id, "result": result }))
                .expect("json"),
            &[],
        )
    }

    /// A `2026-07-28` call of `owner_approval_pending`, declaring `capabilities`, with `extra` params.
    fn modern(&self, capabilities: Value, extra: Value) -> Value {
        let mut params = json!({ "name": "owner_approval_pending", "arguments": {} });
        for (key, value) in extra.as_object().into_iter().flatten() {
            params[key] = value.clone();
        }
        params["_meta"] = json!({ VERSION_META: MODERN, CAPABILITIES_META: capabilities });
        let headers = [
            ("MCP-Protocol-Version".to_string(), MODERN.to_string()),
            ("Mcp-Method".to_string(), "tools/call".to_string()),
            ("Mcp-Name".to_string(), "owner_approval_pending".to_string()),
        ];
        let reply = self.post(&rpc(json!(3), "tools/call", params), &headers);
        assert_eq!(reply.status, 200, "{}", reply.body);
        reply.json()
    }
}

/// An open `2025-11-25` call of `owner_approval_pending`, read as it arrives.
struct Stream {
    stream: std::os::unix::net::UnixStream,
    seen: String,
}

impl Stream {
    fn open(fixture: &Fixture) -> Stream {
        let mut stream = connect_unix(&fixture.socket);
        stream
            .write_all(&post_bytes(
                &[bearer(SESSION)],
                &rpc(
                    json!(2),
                    "tools/call",
                    json!({ "name": "owner_approval_pending", "arguments": {} }),
                ),
            ))
            .expect("send the call");
        Stream {
            stream,
            seen: String::new(),
        }
    }

    fn messages(&self) -> Vec<Value> {
        self.seen
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|data| serde_json::from_str(data).ok())
            .collect()
    }

    /// Reads until the `n`th message (counting from one) has arrived, and answers it.
    fn nth(&mut self, n: usize) -> Value {
        let mut buffer = [0u8; 4096];
        while self.messages().len() < n {
            let read = self.stream.read(&mut buffer).expect("read");
            assert!(read > 0, "the stream ended early: {}", self.seen);
            self.seen
                .push_str(&String::from_utf8_lossy(&buffer[..read]));
        }
        self.messages()[n - 1].clone()
    }
}

fn form() -> Value {
    json!({ "elicitation": {} })
}

fn yes() -> Value {
    json!({ "action": "accept", "content": { "approve": "yes" } })
}

fn assert_fits(structured: &Value) {
    let schema = (verb("owner_approval_pending").expect("in the table").result)();
    let mut bare = structured.clone();
    if let Some(object) = bare.as_object_mut() {
        object.remove("untrusted");
    }
    conforms(&schema, &bare).unwrap_or_else(|why| panic!("{why}: {structured}"));
}

#[test]
fn legacy_the_session_is_asked_about_each_plan_and_a_yes_runs_only_that_plan() {
    let fixture = Fixture::start("session-legacy", 1);
    fixture.initialize(form());
    let mut call = Stream::open(&fixture);
    let first = call.nth(1);
    assert_eq!(first["method"], "elicitation/create");
    assert!(
        first["params"]["message"]
            .as_str()
            .expect("a message")
            .contains(FIRST)
    );
    assert_eq!(fixture.answer(&first["id"], yes()).status, 202);
    let second = call.nth(2);
    assert_eq!(second["method"], "elicitation/create");
    assert!(
        second["params"]["message"]
            .as_str()
            .expect("a message")
            .contains(SECOND)
    );
    assert_eq!(
        fixture
            .answer(&second["id"], json!({ "action": "decline" }))
            .status,
        202
    );
    let reply = call.nth(3);
    assert_eq!(reply["id"], 2);
    let structured = &reply["result"]["structuredContent"];
    assert_eq!(
        structured["decisions"],
        json!([
            { "id": FIRST, "decision": "approved", "result": { "plan": FIRST, "result": { "stopped": true } } },
            { "id": SECOND, "decision": "left" }
        ])
    );
    assert_fits(structured);
    // Only the plan said yes to ran, for the session, bound to the creation time it was shown with. The plan that
    // keeps its passkey card was never asked.
    assert_eq!(
        fixture.approvals(),
        vec![(FIRST.to_string(), 1, SESSION_ID.to_string())]
    );
    let text = reply["result"]["content"].to_string();
    assert!(text.contains("1 approved and run"), "{text}");
    assert!(text.contains("1 left waiting"), "{text}");
}

#[test]
fn modern_the_session_gets_one_input_request_per_plan_and_the_state_is_spent_once() {
    let fixture = Fixture::start("session-modern", 1);
    let first = fixture.modern(form(), json!({}));
    let result = &first["result"];
    assert_eq!(result["resultType"], "input_required", "{first}");
    let requests = result["inputRequests"].as_object().expect("requests");
    assert_eq!(
        requests.keys().cloned().collect::<Vec<_>>(),
        vec![FIRST.to_string(), SECOND.to_string()]
    );
    assert_eq!(requests[FIRST]["method"], "elicitation/create");
    let state = result["requestState"]
        .as_str()
        .expect("a state")
        .to_string();
    assert!(fixture.approvals().is_empty(), "asking approves nothing");
    let answers = json!({ "requestState": state, "inputResponses": { FIRST: { "action": "cancel" }, SECOND: yes() } });
    let done = fixture.modern(form(), answers.clone());
    assert_eq!(done["result"]["resultType"], "complete", "{done}");
    let structured = &done["result"]["structuredContent"];
    assert_eq!(
        structured["decisions"][0],
        json!({ "id": FIRST, "decision": "left" })
    );
    assert_eq!(structured["decisions"][1]["decision"], "approved");
    assert_fits(structured);
    assert_eq!(
        fixture.approvals(),
        vec![(SECOND.to_string(), 2, SESSION_ID.to_string())]
    );
    // The same answers again are refused: a state that approved is spent.
    let replay = fixture.modern(form(), answers);
    assert_eq!(replay["error"]["code"], -32602, "{replay}");
    assert_eq!(fixture.approvals().len(), 1);
}

#[test]
fn a_session_that_may_not_be_asked_gets_the_listing_as_ever() {
    let fixture = Fixture::start("session-never", 0);
    let reply = fixture.modern(form(), json!({}));
    assert_eq!(reply["result"]["resultType"], "complete", "{reply}");
    assert!(reply["result"]["structuredContent"]["decisions"].is_null());
    fixture.initialize(form());
    let legacy = fixture.post(
        &rpc(
            json!(2),
            "tools/call",
            json!({ "name": "owner_approval_pending", "arguments": {} }),
        ),
        &[],
    );
    assert_eq!(legacy.status, 200);
    assert!(
        !legacy.body.contains("elicitation/create"),
        "{}",
        legacy.body
    );
    assert!(fixture.approvals().is_empty());
}

#[test]
fn modern_a_session_whose_passkey_is_old_is_sent_to_the_step_up_page_by_url_first() {
    let fixture = Fixture::start("session-step-modern", 2);
    let both = json!({ "elicitation": { "form": {}, "url": {} } });
    let first = fixture.modern(both.clone(), json!({}));
    let result = &first["result"];
    assert_eq!(result["resultType"], "input_required", "{first}");
    let request = &result["inputRequests"]["stepUp"];
    assert_eq!(request["params"]["mode"], "url");
    assert_eq!(request["params"]["url"], STEP_UP);
    let state = result["requestState"]
        .as_str()
        .expect("a state")
        .to_string();
    // Not confirmed yet: the retry answers the listing, and is never sent to the page twice.
    let retry =
        json!({ "requestState": state, "inputResponses": { "stepUp": { "action": "accept" } } });
    let unconfirmed = fixture.modern(both.clone(), retry.clone());
    assert_eq!(
        unconfirmed["result"]["resultType"], "complete",
        "{unconfirmed}"
    );
    // Confirmed on the page: the retry asks about each plan.
    fixture.approver.mode.store(1, Ordering::SeqCst);
    let asked = fixture.modern(both, retry);
    assert_eq!(asked["result"]["resultType"], "input_required", "{asked}");
    assert!(asked["result"]["inputRequests"][FIRST].is_object());
    // A client that takes forms only is not sent a URL: it gets the listing, which names the page.
    fixture.approver.mode.store(2, Ordering::SeqCst);
    let form_only = fixture.modern(form(), json!({}));
    assert_eq!(form_only["result"]["resultType"], "complete", "{form_only}");
    assert!(fixture.approvals().is_empty());
}

#[test]
fn legacy_the_step_up_completes_on_the_stream_and_the_plans_are_asked_after_it() {
    let fixture = Fixture::start("session-step-legacy", 2);
    fixture.initialize(json!({ "elicitation": { "form": {}, "url": {} } }));
    let mut call = Stream::open(&fixture);
    let step = call.nth(1);
    assert_eq!(step["params"]["mode"], "url");
    assert_eq!(step["params"]["url"], STEP_UP);
    assert_eq!(
        fixture
            .answer(&step["id"], json!({ "action": "accept" }))
            .status,
        202
    );
    // The owner taps the passkey on the page: the session may now be asked.
    fixture.approver.mode.store(1, Ordering::SeqCst);
    let complete = call.nth(2);
    assert_eq!(complete["method"], "notifications/elicitation/complete");
    assert_eq!(
        complete["params"]["elicitationId"],
        step["params"]["elicitationId"]
    );
    let first = call.nth(3);
    assert_eq!(first["params"]["mode"], "form");
    assert_eq!(fixture.answer(&first["id"], yes()).status, 202);
    let second = call.nth(4);
    assert_eq!(fixture.answer(&second["id"], yes()).status, 202);
    let reply = call.nth(5);
    assert_fits(&reply["result"]["structuredContent"]);
    assert_eq!(fixture.approvals().len(), 2);
}

fn no() -> Value {
    json!({ "action": "accept", "content": { "approve": "no" } })
}

#[test]
fn legacy_a_no_in_the_session_declines_that_plan() {
    let fixture = Fixture::start("session-legacy-no", 1);
    fixture.initialize(form());
    let mut call = Stream::open(&fixture);
    let first = call.nth(1);
    let description =
        first["params"]["requestedSchema"]["properties"]["approve"]["description"].to_string();
    assert!(description.contains("or no to decline it"), "{description}");
    assert_eq!(fixture.answer(&first["id"], no()).status, 202);
    let second = call.nth(2);
    assert_eq!(
        fixture
            .answer(&second["id"], json!({ "action": "cancel" }))
            .status,
        202
    );
    let reply = call.nth(3);
    let structured = &reply["result"]["structuredContent"];
    assert_eq!(
        structured["decisions"],
        json!([{ "id": FIRST, "decision": "declined" }, { "id": SECOND, "decision": "left" }])
    );
    assert_fits(structured);
    assert_eq!(
        fixture.declines(),
        vec![(FIRST.to_string(), 1, SESSION_ID.to_string())]
    );
    assert!(fixture.approvals().is_empty());
    let text = reply["result"]["content"].to_string();
    assert!(text.contains("1 declined"), "{text}");
}

#[test]
fn modern_a_no_in_the_session_declines_that_plan_and_spends_the_state() {
    let fixture = Fixture::start("session-modern-no", 1);
    let first = fixture.modern(form(), json!({}));
    let state = first["result"]["requestState"]
        .as_str()
        .expect("a state")
        .to_string();
    let answers = json!({ "requestState": state, "inputResponses": { FIRST: no(), SECOND: { "action": "decline" } } });
    let done = fixture.modern(form(), answers.clone());
    let structured = &done["result"]["structuredContent"];
    assert_eq!(
        structured["decisions"],
        json!([{ "id": FIRST, "decision": "declined" }, { "id": SECOND, "decision": "left" }])
    );
    assert_fits(structured);
    assert_eq!(
        fixture.declines(),
        vec![(FIRST.to_string(), 1, SESSION_ID.to_string())]
    );
    let replay = fixture.modern(form(), answers);
    assert_eq!(replay["error"]["code"], -32602, "{replay}");
    assert_eq!(fixture.declines().len(), 1);
}
