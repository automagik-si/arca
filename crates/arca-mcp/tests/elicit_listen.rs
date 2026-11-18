//! The owner's signed-in session is asked about a plan when nobody called: its client opens the server's own stream with
//! `GET /mcp` (`2025-11-25`, Streamable HTTP "Listening for Messages from the Server"), and each plan that starts waiting,
//! whoever proposed it, is put to it there once as an `elicitation/create`. A yes approves and runs it, a no declines it,
//! a cancel leaves it on its card. A session whose passkey is too old is sent to the step-up page first. A caller the
//! server may ask is given a session id at `initialize` and offered `2025-11-25` alone at `server/discover`. The node's
//! side is a fake: a list of waiting plans another agent fills, and a record of every approval and decline.

mod common;

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arca_api::{Scope, verbs};
use arca_mcp::{
    Asking, CallError, Caller, IssuedTokens, PendingPlan, Server, TerminalApprover, TokenStore,
    ToolHandler, serve_unix,
};
use common::{
    Reply, Scratch, bearer, connect_unix, exchange, mint_into, post, rpc, wait_for_socket,
};
use serde_json::{Value, json};
use tokio::runtime::Runtime;

const MODERN: &str = "2026-07-28";
const VERSION_META: &str = "io.modelcontextprotocol/protocolVersion";
const CAPABILITIES_META: &str = "io.modelcontextprotocol/clientCapabilities";
const SESSION: &str = "arca_at_session-token-of-the-owners-signed-in-app";
const SESSION_ID: &str = "oauth:mcpc-0123/g-0123";
const STEP_UP: &str = "https://box1.example.com/oauth/step-up?grant=g-0123";

/// The issued token of the owner's session.
struct Issued;

impl IssuedTokens for Issued {
    fn lookup(&self, bearer: &str) -> Option<Caller> {
        (bearer == SESSION).then(|| Caller {
            token_id: SESSION_ID.to_string(),
            scope: Scope::Viewer,
            tools: None,
            limits: Default::default(),
        })
    }
}

/// The node, as a fake. `mode` is what `may_ask` answers for the session: 0 never, 1 yes, 2 step up first. `waiting` is
/// what another agent proposed and nobody decided yet.
#[derive(Default)]
struct Node {
    mode: AtomicU8,
    waiting: Mutex<Vec<PendingPlan>>,
    approved: Mutex<Vec<(String, u64, String)>>,
    declined: Mutex<Vec<(String, u64, String)>>,
}

impl Node {
    /// Another agent proposes `id`: it waits for the owner from now on.
    fn propose(&self, id: &str, created_at: u64) {
        self.waiting.lock().expect("lock").push(PendingPlan {
            id: id.to_string(),
            message: format!("Approve this plan? Stop the instance {id}"),
            created_at,
        });
    }

    fn decided(&self, id: &str) {
        self.waiting
            .lock()
            .expect("lock")
            .retain(|plan| plan.id != id);
    }
}

impl TerminalApprover for Node {
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
        let ids: Vec<&str> = result["plans"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|plan| plan["id"].as_str())
            .collect();
        self.waiting
            .lock()
            .expect("lock")
            .iter()
            .filter(|plan| ids.contains(&plan.id.as_str()))
            .cloned()
            .collect()
    }

    fn waiting_now(&self, caller: &Caller) -> Vec<PendingPlan> {
        if self.may_ask(caller) == Asking::No {
            return Vec::new();
        }
        self.waiting.lock().expect("lock").clone()
    }

    fn approve(&self, plan: &str, created_at: u64, caller: &Caller) -> Result<Value, CallError> {
        if self.may_ask(caller) != Asking::Yes {
            return Err(CallError::Forbidden {
                verb: "action_propose".to_string(),
            });
        }
        self.decided(plan);
        self.approved.lock().expect("lock").push((
            plan.to_string(),
            created_at,
            caller.token_id.clone(),
        ));
        Ok(json!({ "plan": plan }))
    }

    fn decline(&self, plan: &str, created_at: u64, caller: &Caller) -> bool {
        if self.may_ask(caller) != Asking::Yes {
            return false;
        }
        self.decided(plan);
        self.declined.lock().expect("lock").push((
            plan.to_string(),
            created_at,
            caller.token_id.clone(),
        ));
        true
    }
}

/// `owner_approval_pending`, as a fake: every plan the node has waiting, each asked here.
struct Pending(Arc<Node>);

impl ToolHandler for Pending {
    fn call(&self, _args: Value, _caller: &Caller) -> Result<Value, CallError> {
        let plans: Vec<Value> = self
            .0
            .waiting
            .lock()
            .expect("lock")
            .iter()
            .map(|plan| {
                json!({
                    "id": plan.id, "kind": "action", "action": "instance_stop",
                    "summary": plan.message, "details": [],
                    "createdAt": plan.created_at, "expiresAt": plan.created_at + 900_000, "askedHere": true
                })
            })
            .collect();
        Ok(json!({ "plans": plans, "session": "asks" }))
    }
}

struct Fixture {
    _rt: Runtime,
    scratch: Scratch,
    socket: PathBuf,
    node: Arc<Node>,
    viewer: String,
}

impl Fixture {
    fn start(tag: &str, mode: u8) -> Fixture {
        let scratch = Scratch::new(tag);
        let viewer = mint_into(scratch.path(), "viewer", Scope::Viewer);
        let tokens = TokenStore::load(scratch.path()).expect("the store loads");
        let node = Arc::new(Node::default());
        node.mode.store(mode, Ordering::SeqCst);
        let server = Server::new(verbs(), tokens)
            .handle("owner_approval_pending", Arc::new(Pending(node.clone())))
            .oauth(
                Arc::new(Issued),
                "https://box1.example.com/.well-known/oauth-protected-resource/mcp".to_string(),
            )
            .terminal_approval(node.clone() as Arc<dyn TerminalApprover>)
            .terminal_timeout(Duration::from_secs(5));
        let rt = Runtime::new().expect("runtime");
        let socket = scratch.at("arca.sock");
        let (router, served) = (server.router(), socket.clone());
        rt.spawn(async move { serve_unix(router, &served).await });
        wait_for_socket(&socket);
        Fixture {
            _rt: rt,
            scratch,
            socket,
            node,
            viewer,
        }
    }

    fn post_as(&self, secret: &str, body: &[u8], extra: &[(String, String)]) -> Reply {
        let mut headers = vec![bearer(secret)];
        headers.extend_from_slice(extra);
        post(&mut connect_unix(&self.socket), &headers, body)
    }

    /// The session's client initializes, declaring `capabilities`: the session id it was given, if any.
    fn initialize(&self, capabilities: Value) -> Option<String> {
        let reply = self.post_as(
            SESSION,
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
        reply.header("mcp-session-id").map(str::to_string)
    }

    /// The client's answer to the server's request `id`, sent in its session.
    fn answer(&self, session: &str, id: &Value, result: Value) -> Reply {
        self.post_as(
            SESSION,
            &serde_json::to_vec(&json!({ "jsonrpc": "2.0", "id": id, "result": result }))
                .expect("json"),
            &[session_header(session)],
        )
    }

    fn request(&self, method: &str, secret: &str, extra: &[(String, String)]) -> Reply {
        let mut head = format!(
            "{method} /mcp HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {secret}\r\nConnection: close\r\n"
        );
        for (name, value) in extra {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        exchange(&mut connect_unix(&self.socket), head.as_bytes())
    }

    fn approvals(&self) -> Vec<(String, u64, String)> {
        self.node.approved.lock().expect("lock").clone()
    }

    fn declines(&self) -> Vec<(String, u64, String)> {
        self.node.declined.lock().expect("lock").clone()
    }
}

fn session_header(id: &str) -> (String, String) {
    ("Mcp-Session-Id".to_string(), id.to_string())
}

fn accept_stream() -> (String, String) {
    ("Accept".to_string(), "text/event-stream".to_string())
}

fn form() -> Value {
    json!({ "elicitation": {} })
}

fn yes() -> Value {
    json!({ "action": "accept", "content": { "approve": "yes" } })
}

fn no() -> Value {
    json!({ "action": "accept", "content": { "approve": "no" } })
}

/// The server's own stream, opened with `GET /mcp` and read as it arrives.
struct Listen {
    stream: UnixStream,
    seen: String,
}

impl Listen {
    fn open(fixture: &Fixture, session: &str) -> Listen {
        let mut stream = connect_unix(&fixture.socket);
        stream
            .write_all(
                format!(
                    "GET /mcp HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {SESSION}\r\nAccept: text/event-stream\r\nMcp-Session-Id: {session}\r\nMcp-Protocol-Version: 2025-11-25\r\n\r\n"
                )
                .as_bytes(),
            )
            .expect("open the stream");
        let mut listen = Listen {
            stream,
            seen: String::new(),
        };
        listen.read_until(|seen| seen.contains("\r\n\r\n"));
        assert!(
            listen.seen.starts_with("HTTP/1.1 200"),
            "the stream opens: {}",
            listen.seen
        );
        assert!(
            listen
                .seen
                .to_ascii_lowercase()
                .contains("content-type: text/event-stream"),
            "{}",
            listen.seen
        );
        listen
    }

    fn read_until(&mut self, done: impl Fn(&str) -> bool) {
        let mut buffer = [0u8; 4096];
        while !done(&self.seen) {
            let read = self.stream.read(&mut buffer).expect("read the stream");
            assert!(read > 0, "the stream ended early: {}", self.seen);
            self.seen
                .push_str(&String::from_utf8_lossy(&buffer[..read]));
        }
    }

    fn messages(&self) -> Vec<Value> {
        self.seen
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|data| serde_json::from_str(data).ok())
            .collect()
    }

    /// Reads until the `n`th message (counting from one) has arrived.
    fn nth(&mut self, n: usize) -> Value {
        let mut buffer = [0u8; 4096];
        while self.messages().len() < n {
            let read = self.stream.read(&mut buffer).expect("read the stream");
            assert!(read > 0, "the stream ended early: {}", self.seen);
            self.seen
                .push_str(&String::from_utf8_lossy(&buffer[..read]));
        }
        self.messages()[n - 1].clone()
    }

    /// Whether another message arrives within `wait`.
    fn quiet_for(&mut self, wait: Duration) -> bool {
        let before = self.messages().len();
        self.stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .expect("a short timeout");
        let until = Instant::now() + wait;
        let mut buffer = [0u8; 4096];
        while Instant::now() < until {
            if let Ok(read) = self.stream.read(&mut buffer) {
                if read == 0 {
                    break;
                }
                self.seen
                    .push_str(&String::from_utf8_lossy(&buffer[..read]));
            }
        }
        self.stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("the timeout back");
        self.messages().len() == before
    }
}

/// Waits until `check` holds, up to two seconds: the decision is made on the server's side after the answer is posted.
fn eventually(check: impl Fn() -> bool) -> bool {
    let until = Instant::now() + Duration::from_secs(2);
    while Instant::now() < until {
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    check()
}

#[test]
fn a_plan_another_agent_proposes_is_asked_on_the_idle_sessions_stream_and_a_yes_runs_it() {
    let fixture = Fixture::start("listen-yes", 1);
    let session = fixture.initialize(form()).expect("a session id");
    let mut listen = Listen::open(&fixture, &session);
    assert!(
        listen.quiet_for(Duration::from_millis(1500)),
        "nothing is asked while nothing waits: {}",
        listen.seen
    );
    // Another agent proposes, and nobody calls anything: the question arrives on the stream.
    fixture.node.propose("act-000000000001", 11);
    let asked = listen.nth(1);
    assert_eq!(asked["method"], "elicitation/create");
    assert_eq!(asked["params"]["mode"], "form");
    assert!(
        asked["params"]["message"]
            .as_str()
            .expect("a message")
            .contains("act-000000000001")
    );
    assert_eq!(
        asked["params"]["requestedSchema"]["properties"]["approve"]["enum"],
        json!(["yes", "no"])
    );
    assert_eq!(fixture.answer(&session, &asked["id"], yes()).status, 202);
    assert!(eventually(|| fixture.approvals()
        == vec![(
            "act-000000000001".to_string(),
            11,
            SESSION_ID.to_string()
        )]));
}

#[test]
fn a_no_declines_it_a_cancel_leaves_it_and_no_plan_is_asked_twice() {
    let fixture = Fixture::start("listen-no", 1);
    let session = fixture.initialize(form()).expect("a session id");
    let mut listen = Listen::open(&fixture, &session);
    fixture.node.propose("act-000000000001", 1);
    fixture.node.propose("act-000000000002", 2);
    let first = listen.nth(1);
    let second = listen.nth(2);
    let asked: Vec<&str> = [&first, &second]
        .iter()
        .map(|message| message["params"]["message"].as_str().expect("a message"))
        .collect();
    assert!(asked[0].contains("act-000000000001") && asked[1].contains("act-000000000002"));
    assert_eq!(fixture.answer(&session, &first["id"], no()).status, 202);
    assert_eq!(
        fixture
            .answer(&session, &second["id"], json!({ "action": "cancel" }))
            .status,
        202
    );
    assert!(eventually(|| fixture.declines()
        == vec![(
            "act-000000000001".to_string(),
            1,
            SESSION_ID.to_string()
        )]));
    assert!(fixture.approvals().is_empty());
    // The cancelled plan still waits on its card, and is not asked again, on this stream or on a new one.
    assert_eq!(fixture.node.waiting.lock().expect("lock").len(), 1);
    assert!(
        listen.quiet_for(Duration::from_millis(1500)),
        "{}",
        listen.seen
    );
    let mut again = Listen::open(&fixture, &session);
    assert!(
        again.quiet_for(Duration::from_millis(1500)),
        "{}",
        again.seen
    );
    // A plan proposed again after a decision is another record, and is asked.
    fixture.node.propose("act-000000000001", 3);
    let anew = again.nth(1);
    assert!(
        anew["params"]["message"]
            .as_str()
            .expect("a message")
            .contains("act-000000000001")
    );
}

#[test]
fn an_answer_to_the_stream_from_another_token_is_not_taken() {
    let fixture = Fixture::start("listen-other", 1);
    let session = fixture.initialize(form()).expect("a session id");
    let mut listen = Listen::open(&fixture, &session);
    fixture.node.propose("act-000000000001", 1);
    let asked = listen.nth(1);
    let foreign = fixture.post_as(
        &fixture.viewer,
        &serde_json::to_vec(&json!({ "jsonrpc": "2.0", "id": asked["id"], "result": yes() }))
            .expect("json"),
        &[],
    );
    assert_ne!(foreign.status, 202, "{}", foreign.body);
    std::thread::sleep(Duration::from_millis(300));
    assert!(fixture.approvals().is_empty());
}

#[test]
fn a_session_whose_passkey_is_too_old_is_sent_to_the_step_up_page_then_asked() {
    let fixture = Fixture::start("listen-step-up", 2);
    let session = fixture
        .initialize(json!({ "elicitation": { "form": {}, "url": {} } }))
        .expect("a session id");
    let mut listen = Listen::open(&fixture, &session);
    fixture.node.propose("act-000000000001", 1);
    let step_up = listen.nth(1);
    assert_eq!(step_up["method"], "elicitation/create");
    assert_eq!(step_up["params"]["mode"], "url");
    assert_eq!(step_up["params"]["url"], STEP_UP);
    assert_eq!(
        fixture
            .answer(&session, &step_up["id"], json!({ "action": "accept" }))
            .status,
        202
    );
    // The same plans do not send the owner to the page twice while the tap is awaited.
    assert!(
        listen.quiet_for(Duration::from_millis(1500)),
        "{}",
        listen.seen
    );
    // The owner taps the passkey on the page: the URL request is complete, then the plan is asked.
    fixture.node.mode.store(1, Ordering::SeqCst);
    let complete = listen.nth(2);
    assert_eq!(complete["method"], "notifications/elicitation/complete");
    assert_eq!(
        complete["params"]["elicitationId"],
        step_up["params"]["elicitationId"]
    );
    let asked = listen.nth(3);
    assert_eq!(asked["params"]["mode"], "form");
    assert_eq!(fixture.answer(&session, &asked["id"], yes()).status, 202);
    assert!(eventually(|| fixture.approvals().len() == 1));
}

#[test]
fn only_a_caller_that_may_be_asked_and_declared_elicitation_gets_a_stream() {
    let fixture = Fixture::start("listen-who", 1);
    // A stored viewer token is never asked: no stream, as before there was one.
    assert_eq!(
        fixture
            .request("GET", &fixture.viewer, &[accept_stream()])
            .status,
        405
    );
    // The session, before its client declared elicitation, and a client that does not accept a stream.
    assert_eq!(
        fixture.request("GET", SESSION, &[accept_stream()]).status,
        405
    );
    let session = fixture.initialize(form()).expect("a session id");
    assert_eq!(
        fixture
            .request("GET", SESSION, &[session_header(&session)])
            .status,
        405
    );
    // Without a bearer: 401, as every route.
    let bare = exchange(
        &mut connect_unix(&fixture.socket),
        b"GET /mcp HTTP/1.1\r\nHost: localhost\r\nAccept: text/event-stream\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(bare.status, 401);
    // A session the node no longer asks loses its stream: its body ends with the last chunk.
    let mut listen = Listen::open(&fixture, &session);
    fixture.node.mode.store(0, Ordering::SeqCst);
    listen.read_until(|seen| seen.ends_with("\r\n0\r\n\r\n"));
}

#[test]
fn a_session_id_is_given_to_a_caller_that_may_be_asked_and_an_unknown_one_is_404() {
    let fixture = Fixture::start("listen-session", 1);
    let initialize = rpc(
        json!(1),
        "initialize",
        json!({ "protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": { "name": "t", "version": "1" } }),
    );
    // A caller the server never asks is given no session, and a session id it sends is never read.
    let viewer = fixture.post_as(&fixture.viewer, &initialize, &[]);
    assert_eq!(viewer.status, 200);
    assert_eq!(viewer.header("mcp-session-id"), None);
    let list = rpc(json!(2), "tools/list", json!({}));
    assert_eq!(
        fixture
            .post_as(&fixture.viewer, &list, &[session_header("made-up")])
            .status,
        200
    );
    // The owner's session is given one: visible ASCII, used on every later request.
    let session = fixture.initialize(form()).expect("a session id");
    assert!(session.len() >= 32 && session.bytes().all(|b| (0x21..=0x7e).contains(&b)));
    assert_eq!(
        fixture
            .post_as(SESSION, &list, &[session_header(&session)])
            .status,
        200
    );
    // One the server did not give this token, or has forgotten, is 404, so the client initializes again.
    assert_eq!(
        fixture
            .post_as(SESSION, &list, &[session_header("made-up")])
            .status,
        404
    );
    assert_eq!(
        fixture
            .request(
                "GET",
                SESSION,
                &[accept_stream(), session_header("made-up")]
            )
            .status,
        404
    );
    // A request without one is served, as the server never required it.
    assert_eq!(fixture.post_as(SESSION, &list, &[]).status, 200);
    // An initialize carrying an old id starts a new session.
    let again = fixture.post_as(SESSION, &initialize, &[session_header("made-up")]);
    assert_eq!(again.status, 200);
    assert!(again.header("mcp-session-id").is_some());
    // DELETE ends it: 204, then the id is unknown.
    assert_eq!(
        fixture
            .request("DELETE", SESSION, &[session_header(&session)])
            .status,
        204
    );
    assert_eq!(
        fixture
            .post_as(SESSION, &list, &[session_header(&session)])
            .status,
        404
    );
    assert_eq!(
        fixture
            .request("DELETE", SESSION, &[session_header(&session)])
            .status,
        404
    );
    assert_eq!(fixture.request("DELETE", SESSION, &[]).status, 405);
    drop(fixture.scratch);
}

#[test]
fn discovery_offers_the_version_that_can_be_asked_to_a_caller_that_may_be_asked() {
    let fixture = Fixture::start("listen-discover", 1);
    let discover = |secret: &str| {
        let headers = [
            ("MCP-Protocol-Version".to_string(), MODERN.to_string()),
            ("Mcp-Method".to_string(), "server/discover".to_string()),
        ];
        let reply = fixture.post_as(
            secret,
            &rpc(
                json!(1),
                "server/discover",
                json!({ "_meta": { VERSION_META: MODERN, CAPABILITIES_META: form() } }),
            ),
            &headers,
        );
        assert_eq!(reply.status, 200, "{}", reply.body);
        reply.json()["result"].clone()
    };
    let owner = discover(SESSION);
    assert_eq!(owner["supportedVersions"], json!(["2025-11-25"]));
    assert_eq!(owner["cacheScope"], "private");
    let viewer = discover(&fixture.viewer);
    assert_eq!(viewer["supportedVersions"], json!([MODERN, "2025-11-25"]));
    assert_eq!(viewer["cacheScope"], "public");
    // A session the node does not ask now is offered both, as anyone.
    fixture.node.mode.store(0, Ordering::SeqCst);
    assert_eq!(
        discover(SESSION)["supportedVersions"],
        json!([MODERN, "2025-11-25"])
    );
}

#[test]
fn a_listing_call_does_not_ask_a_plan_whose_question_is_open_on_the_stream() {
    let fixture = Fixture::start("listen-and-list", 1);
    let session = fixture.initialize(form()).expect("a session id");
    let mut listen = Listen::open(&fixture, &session);
    fixture.node.propose("act-000000000001", 1);
    let asked = listen.nth(1);
    assert_eq!(asked["method"], "elicitation/create");
    // The owner also asks what waits: the plan is listed, but its open question is not asked a second time.
    let call = rpc(
        json!(2),
        "tools/call",
        json!({ "name": "owner_approval_pending", "arguments": {} }),
    );
    let listed = fixture.post_as(SESSION, &call, &[session_header(&session)]);
    assert_eq!(listed.status, 200, "{}", listed.body);
    assert_eq!(
        listed.header("content-type"),
        Some("application/json"),
        "answered at once, not asked on the call's stream"
    );
    assert_eq!(
        listed.json()["result"]["structuredContent"]["plans"][0]["id"],
        "act-000000000001"
    );
    assert_eq!(fixture.answer(&session, &asked["id"], yes()).status, 202);
    assert!(eventually(|| fixture.approvals().len() == 1));
}
