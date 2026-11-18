//! A throwaway kernel on loopback for checking a real MCP client by hand: is the owner's signed-in session asked about a
//! plan another token proposed, with nobody typing in it? Ignored, so `cargo test` never runs it; run it with
//!
//! ```text
//! ARCA_LIVE_DIR=<an empty private directory> cargo test -p arca --test owner_session_live -- --ignored --nocapture
//! ```
//!
//! It serves the MCP door, the approval page and the OAuth routes as `serve` merges them, on `127.0.0.1` (port
//! `ARCA_LIVE_PORT`, 47811 by default), over a fake Incus with the software passkey of the page tests. At start the
//! passkey connects an app with `operator approvals`, as the owner's tap on the consent page does, and the harness
//! writes into the directory:
//!
//! - `url`: the MCP URL;
//! - `owner.token`: that app's access token (an hour), which a client sends as its bearer to be the owner's session;
//! - `agent.token`: a stored operator token, for another agent that proposes plans with `action_propose`;
//! - `requests.log`: one line per request the door was sent, with the headers that matter to MCP and the JSON-RPC
//!   method, id or answer of its body;
//! - `ran.log`: one line per approved plan the harness ran.
//!
//! It runs until a file named `stop` appears there, or for an hour. Run again on the same directory, it is a restart: the
//! grant, both tokens and the logs are kept, and every session id the first run gave is forgotten.

mod common;
#[path = "../../arca-approval/tests/common/webauthn.rs"]
mod webauthn;

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arca::approve_page::{self, SetupCodeDisplay};
use arca::handlers::{ActionExecutor, Clock, OwnerApprovalPending, TerminalApproval};
use arca::mcp_oauth::OAuth;
use arca_api::Scope;
use arca_mcp::{CallError, Caller, IssuedTokens, Server, TokenStore, ToolHandler, serve_unix};
use axum::body::Body;
use axum::extract::Request;
use axum::middleware::{self, Next};
use axum::response::Response;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use common::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use webauthn::{Assert, ORIGIN, SoftPasskey};

const RP: &str = "box1.example.com";
const REDIRECT: &str = "http://127.0.0.1:33418/callback";
const VERIFIER: &str = "a-pkce-verifier-of-fifty-characters-0123456789abcd";
const PG_IMAGE: &str = "docker.io/library/postgres@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

struct NoDisplay;

impl SetupCodeDisplay for NoDisplay {
    fn show(&self, _code: &str) -> Result<(), String> {
        Err("no screen".to_string())
    }
}

/// Stands in for the approved verb: it writes one line to `ran.log` and answers that it ran.
struct Ran {
    log: PathBuf,
}

impl ToolHandler for Ran {
    fn call(&self, args: Value, caller: &Caller) -> Result<Value, CallError> {
        append(
            &self.log,
            &format!("{} ran {args} as {}", now_ms(), caller.token_id),
        );
        Ok(json!({ "created": true }))
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

fn append(path: &Path, line: &str) {
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{line}");
    }
}

/// One line per request: its method and path, the MCP headers, and what its JSON-RPC body is.
async fn log_request(log: Arc<PathBuf>, request: Request, next: Next) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, 1 << 20)
        .await
        .unwrap_or_default();
    let header = |name: &str| {
        parts
            .headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(|value| format!(" {name}={value}"))
            .unwrap_or_default()
    };
    let what = serde_json::from_slice::<Value>(&bytes)
        .map(|body| {
            let mut said = json!({});
            for key in ["method", "id", "params", "result", "error"] {
                if let Some(value) = body.get(key) {
                    said[key] = value.clone();
                }
            }
            if let Some(protocol) = body["params"].get("protocolVersion") {
                said["protocolVersion"] = protocol.clone();
            }
            said.to_string()
        })
        .unwrap_or_default();
    let line = format!(
        "{} {} {}{}{}{}{}{} {}",
        now_ms(),
        parts.method,
        parts.uri.path(),
        header("accept"),
        header("mcp-protocol-version"),
        header("mcp-session-id"),
        header("mcp-method"),
        header("user-agent"),
        what.chars().take(1500).collect::<String>(),
    );
    let response = next
        .run(Request::from_parts(parts, Body::from(bytes)))
        .await;
    append(
        &log,
        &format!(
            "{line} -> {}{}",
            response.status(),
            response
                .headers()
                .get("mcp-session-id")
                .map(|_| " (session id given)")
                .unwrap_or_default()
        ),
    );
    response
}

struct Reply {
    status: u16,
    body: Vec<u8>,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

fn send(
    socket: &Path,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    content_type: &str,
    body: &[u8],
) -> Reply {
    let mut stream = UnixStream::connect(socket).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {RP}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    if !body.is_empty() {
        head.push_str(&format!("Content-Type: {content_type}\r\n"));
    }
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let mut bytes = head.into_bytes();
    bytes.extend_from_slice(body);
    stream.write_all(&bytes).expect("write");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read");
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("a response head");
    let status = String::from_utf8_lossy(&raw[..split])
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|status| status.parse().ok())
        .expect("a status");
    Reply {
        status,
        body: raw[split + 4..].to_vec(),
    }
}

fn form(pairs: &[(&str, &str)]) -> String {
    let encode = |text: &str| -> String {
        text.bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
                    (b as char).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect()
    };
    pairs
        .iter()
        .map(|(key, value)| format!("{key}={}", encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// The owner connects an app with `scope` on the consent page and taps `key`: the app's access token.
fn connect_owner_app(socket: &Path, key: &SoftPasskey, scope: &str) -> String {
    let registered = send(
        socket,
        "POST",
        "/oauth/register",
        &[],
        "application/json",
        json!({ "client_name": "A real client", "redirect_uris": [REDIRECT], "token_endpoint_auth_method": "none" })
            .to_string()
            .as_bytes(),
    );
    assert_eq!(registered.status, 201, "register");
    let client = registered.json()["client_id"]
        .as_str()
        .expect("a client id")
        .to_string();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(VERIFIER.as_bytes()));
    let resource = format!("{ORIGIN}/mcp");
    let query = form(&[
        ("response_type", "code"),
        ("client_id", &client),
        ("redirect_uri", REDIRECT),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("state", "live"),
        ("scope", scope),
        ("resource", &resource),
    ]);
    let page = send(
        socket,
        "GET",
        &format!("/oauth/authorize?{query}"),
        &[],
        "",
        b"",
    );
    assert_eq!(page.status, 200, "the consent page");
    let text = String::from_utf8_lossy(&page.body).to_string();
    let at = text.find("data-request=\"").expect("a request id") + 14;
    let id = text[at..at + text[at..].find('"').expect("its end")].to_string();
    let page_post = |path: &str, body: Value| {
        send(
            socket,
            "POST",
            path,
            &[("Origin", ORIGIN)],
            "application/json",
            body.to_string().as_bytes(),
        )
    };
    // The owner picks the level on the page; `approvals` rides along when the client asked for it.
    let level = scope.split(' ').next().unwrap_or("viewer");
    let begun = page_post(
        "/oauth/authorize/begin",
        json!({ "id": id, "scope": level }),
    );
    assert_eq!(begun.status, 200, "begin");
    let statement = begun.json()["statement"]
        .as_str()
        .expect("a statement")
        .to_string();
    let proof = key.proof_value(&Assert::over(NAMESPACE, statement.as_bytes()));
    let approved = page_post(
        "/oauth/authorize/approve",
        json!({ "id": id, "statement": statement, "proof": proof }),
    );
    assert_eq!(approved.status, 200, "approve");
    let redirect = approved.json()["redirect"]
        .as_str()
        .expect("a redirect")
        .to_string();
    let code = redirect
        .split(['?', '&'])
        .find_map(|pair| pair.strip_prefix("code="))
        .expect("a code")
        .to_string();
    let exchanged = send(
        socket,
        "POST",
        "/oauth/token",
        &[],
        "application/x-www-form-urlencoded",
        form(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("client_id", &client),
            ("redirect_uri", REDIRECT),
            ("code_verifier", VERIFIER),
            ("resource", &resource),
        ])
        .as_bytes(),
    );
    assert_eq!(exchanged.status, 200, "exchange");
    let tokens = exchanged.json();
    assert_eq!(tokens["scope"], json!(scope));
    tokens["access_token"]
        .as_str()
        .expect("an access token")
        .to_string()
}

#[test]
#[ignore = "a harness for checking a real client by hand; it runs until told to stop"]
fn a_throwaway_kernel_on_loopback_for_a_real_client() {
    let dir =
        PathBuf::from(std::env::var("ARCA_LIVE_DIR").expect("ARCA_LIVE_DIR names a directory"));
    let port: u16 = std::env::var("ARCA_LIVE_PORT")
        .ok()
        .and_then(|port| port.parse().ok())
        .unwrap_or(47811);
    fs::create_dir_all(&dir).expect("the directory");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("mode 0700");
    let scratch = Scratch::new("owner-session-live");
    let fake = start_fake(&scratch);
    let key = SoftPasskey::from_seed(1);
    let verifier = verifier_with_passkeys(
        &scratch,
        &[],
        &format!("{}\n", key.line(NAMESPACE)),
        "owner-session-live",
    );
    let kernel = Arc::new(Mutex::new(
        kernel_with_image(&scratch, &fake, verifier, now_ms(), Some(PG_IMAGE))
            .expect("the kernel opens"),
    ));
    let clock: Clock = Arc::new(now_ms);
    // The tokens and the authorization store live in the directory, so a second run is a restart: the same grant and
    // the same agent token, and every session id the first run gave forgotten.
    let restarted = dir.join("owner.token").exists();
    let tokens_dir = dir.join("tokens");
    let state_dir = dir.join("state");
    for made in [&tokens_dir, &state_dir] {
        fs::create_dir_all(made).expect("a directory");
        fs::set_permissions(made, fs::Permissions::from_mode(0o700)).expect("mode 0700");
    }
    let agent = if restarted {
        fs::read_to_string(dir.join("agent.token")).expect("the agent token")
    } else {
        let (agent, text) = arca_mcp::mint(Scope::Operator);
        let file = tokens_dir.join("agent.hash");
        fs::write(&file, text).expect("the hash file");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o400)).expect("mode 0400");
        agent
    };
    let tokens = TokenStore::load(&tokens_dir).expect("the token store loads");
    let page = approve_page::Page::new(
        ORIGIN.to_string(),
        Arc::clone(&kernel),
        clock.clone(),
        state_dir.clone(),
        Arc::new(NoDisplay),
    )
    .with_log(Arc::new(|_| {}));
    let oauth = Arc::new(
        OAuth::open(ORIGIN, RP, Arc::clone(&kernel), clock.clone(), &state_dir)
            .expect("the store opens")
            .with_log(Arc::new(|_| {})),
    );
    kernel
        .lock()
        .expect("kernel")
        .set_owner_session(oauth.owner_session());
    let handlers: std::collections::HashMap<&'static str, Arc<dyn ToolHandler>> =
        arca::handlers::plan_handlers(
            Arc::clone(&kernel),
            clock.clone(),
            Some(PG_IMAGE.to_string()),
            None,
        )
        .into_iter()
        .collect();
    let actions = ActionExecutor::new([(
        "postgres_create",
        Arc::new(Ran {
            log: dir.join("ran.log"),
        }) as Arc<dyn ToolHandler>,
    )]);
    let requests = arca::approval_requests::ApprovalRequests::open(&state_dir, None, now_ms())
        .expect("the requests open");
    let mut server = Server::new(arca_api::verbs(), tokens)
        .oauth(
            Arc::clone(&oauth) as Arc<dyn IssuedTokens>,
            oauth.resource_metadata_url(),
        )
        .terminal_approval(Arc::new(TerminalApproval {
            kernel: Arc::clone(&kernel),
            clock: clock.clone(),
            actions,
            oauth: Some(Arc::clone(&oauth)),
        }))
        .handle(
            arca::handlers::OWNER_APPROVAL_PENDING,
            Arc::new(OwnerApprovalPending {
                kernel: Arc::clone(&kernel),
                clock: clock.clone(),
                requests: Arc::new(requests),
                oauth: Some(Arc::clone(&oauth)),
                page: Some(format!("{ORIGIN}/approve")),
            }),
        );
    for verb in ["action_propose", "plan_list"] {
        server = server.handle(verb, Arc::clone(&handlers[verb]));
    }
    let log = Arc::new(dir.join("requests.log"));
    let router = server
        .router()
        .merge(page.router(false))
        .merge(oauth.router())
        .merge(page.local_router())
        .merge(oauth.local_router())
        .layer(middleware::from_fn(move |request: Request, next: Next| {
            log_request(Arc::clone(&log), request, next)
        }));
    let runtime = tokio::runtime::Runtime::new().expect("a runtime");
    let socket = scratch.path("serve.sock");
    let (unix_router, served) = (router.clone(), socket.clone());
    runtime.spawn(async move {
        let _ = serve_unix(unix_router, &served).await;
    });
    let addr: std::net::SocketAddr = ([127, 0, 0, 1], port).into();
    runtime.spawn(async move {
        let _ = arca_mcp::serve_loopback(router, addr, |_| {}).await;
    });
    let until = Instant::now() + Duration::from_secs(5);
    while !socket.exists() {
        assert!(Instant::now() < until, "the server started");
        std::thread::sleep(Duration::from_millis(10));
    }
    let owner = if restarted {
        fs::read_to_string(dir.join("owner.token")).expect("the owner token")
    } else {
        connect_owner_app(&socket, &key, "operator approvals")
    };
    let write_private = |name: &str, text: &str| {
        let path = dir.join(name);
        fs::write(&path, text).expect("written");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("mode 0600");
    };
    write_private("owner.token", &owner);
    write_private("agent.token", &agent);
    write_private("url", &format!("http://127.0.0.1:{port}/mcp"));
    println!(
        "ready: http://127.0.0.1:{port}/mcp, files in {}",
        dir.display()
    );
    let stop = dir.join("stop");
    let _ = fs::remove_file(&stop);
    let deadline = Instant::now() + Duration::from_secs(60 * 60);
    while !stop.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
    }
    drop(runtime);
}
