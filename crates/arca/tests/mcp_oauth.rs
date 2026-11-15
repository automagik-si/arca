//! MCP authorization: the approval page as the OAuth 2.1 authorization server of the MCP door, end to end over one unix
//! socket that carries the MCP routes, the page and the OAuth routes merged as `serve` merges them. The owner is the
//! software P-256 passkey of the page tests; the clock is the test's own.
//!
//! The security checks each have a test that fails when the check is taken out: PKCE, the resource, the redirect, the
//! scope ceiling, refresh rotation and reuse, code reuse, revocation, expiry and the owner's passkey.

mod common;
#[path = "../../arca-approval/tests/common/webauthn.rs"]
mod webauthn;

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arca::approve_page::{self, SetupCodeDisplay};
use arca::handlers::Clock;
use arca::mcp_oauth::OAuth;
use arca_api::Scope;
use arca_mcp::{IssuedTokens, Server, TokenStore, serve_unix};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use common::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::runtime::Runtime;
use webauthn::{Assert, ORIGIN, SoftPasskey};

const RP: &str = "box1.example.com";
const REDIRECT: &str = "http://127.0.0.1:33418/callback";
const VERIFIER: &str = "a-pkce-verifier-of-fifty-characters-0123456789abcd";
const HOUR: u64 = 60 * 60 * 1000;
const DAY: u64 = 24 * HOUR;

struct NoDisplay;

impl SetupCodeDisplay for NoDisplay {
    fn show(&self, _code: &str) -> Result<(), String> {
        Err("no screen".to_string())
    }
}

struct Rig {
    _scratch: Scratch,
    _fake: arca_incus::FakeIncus,
    socket: PathBuf,
    state_dir: PathBuf,
    clock: Arc<AtomicU64>,
    oauth: Arc<OAuth>,
    kernel: Arc<Mutex<arca::kernel::Kernel>>,
    viewer: String,
    _runtime: Runtime,
}

struct Reply {
    status: u16,
    head: String,
    body: Vec<u8>,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }

    fn header(&self, name: &str) -> Option<String> {
        self.head.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_string())
        })
    }
}

fn challenge_of(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
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

/// A query parameter of a URL, decoded.
fn param(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| {
            let bytes = v.as_bytes();
            let mut out = Vec::new();
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == b'%' && i + 2 < bytes.len() + 1 {
                    let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).expect("hex");
                    out.push(u8::from_str_radix(hex, 16).expect("a hex byte"));
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            String::from_utf8(out).expect("utf-8")
        })
    })
}

impl Rig {
    fn new(tag: &str) -> (Rig, SoftPasskey) {
        let scratch = Scratch::new(tag);
        let fake = start_fake(&scratch);
        let key = SoftPasskey::from_seed(1);
        let verifier =
            verifier_with_passkeys(&scratch, &[], &format!("{}\n", key.line(NAMESPACE)), tag);
        let kernel = Arc::new(Mutex::new(
            kernel(&scratch, &fake, verifier, NOW).expect("the kernel opens"),
        ));
        let tokens_dir = scratch.path("tokens");
        fs::create_dir(&tokens_dir).expect("the token directory");
        fs::set_permissions(&tokens_dir, fs::Permissions::from_mode(0o700)).expect("mode 0700");
        let (viewer, text) = arca_mcp::mint(Scope::Viewer);
        let file = tokens_dir.join("viewer.hash");
        fs::write(&file, text).expect("the hash file");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o400)).expect("mode 0400");
        let tokens = TokenStore::load(&tokens_dir).expect("the token store loads");
        let state_dir = scratch.path("state");
        fs::create_dir(&state_dir).expect("the state directory");
        fs::set_permissions(&state_dir, fs::Permissions::from_mode(0o700)).expect("mode 0700");
        let clock = Arc::new(AtomicU64::new(NOW));
        let ticks = Arc::clone(&clock);
        let clock_fn: Clock = Arc::new(move || ticks.load(Ordering::SeqCst));
        let page = approve_page::Page::new(
            ORIGIN.to_string(),
            Arc::clone(&kernel),
            clock_fn.clone(),
            state_dir.clone(),
            Arc::new(NoDisplay),
        )
        .with_log(Arc::new(|_| {}));
        let oauth = Arc::new(
            OAuth::open(ORIGIN, RP, Arc::clone(&kernel), clock_fn, &state_dir)
                .expect("the store opens")
                .with_log(Arc::new(|_| {})),
        );
        // As `serve` does: the kernel holds waiting plans open while the owner's signed-in session is connected.
        kernel
            .lock()
            .expect("kernel")
            .set_owner_session(oauth.owner_session());
        // The unix socket's router exactly as `serve` merges it.
        let router = Server::new(arca_api::verbs(), tokens)
            .oauth(
                Arc::clone(&oauth) as Arc<dyn IssuedTokens>,
                oauth.resource_metadata_url(),
            )
            .router()
            .merge(page.router(false))
            .merge(oauth.router())
            .merge(page.local_router())
            .merge(oauth.local_router());
        let socket = scratch.path("serve.sock");
        let runtime = Runtime::new().expect("a runtime");
        let served = socket.clone();
        runtime.spawn(async move {
            let _ = serve_unix(router, &served).await;
        });
        let until = Instant::now() + Duration::from_secs(5);
        while !socket.exists() {
            assert!(Instant::now() < until, "the test server started");
            std::thread::sleep(Duration::from_millis(10));
        }
        let rig = Rig {
            _scratch: scratch,
            _fake: fake,
            socket,
            state_dir,
            clock,
            oauth,
            kernel,
            viewer,
            _runtime: runtime,
        };
        (rig, key)
    }

    fn advance(&self, millis: u64) {
        self.clock.fetch_add(millis, Ordering::SeqCst);
    }

    fn send(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        content_type: &str,
        body: &[u8],
    ) -> Reply {
        let mut stream = UnixStream::connect(&self.socket).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("read timeout");
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: box1.example.com\r\nConnection: close\r\nContent-Length: {}\r\n",
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
        let head = String::from_utf8_lossy(&raw[..split]).to_string();
        let status = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|status| status.parse().ok())
            .expect("a status");
        Reply {
            status,
            head,
            body: raw[split + 4..].to_vec(),
        }
    }

    fn get(&self, path: &str) -> Reply {
        self.send("GET", path, &[], "", b"")
    }

    fn page_post(&self, path: &str, body: Value) -> Reply {
        self.send(
            "POST",
            path,
            &[("Origin", ORIGIN)],
            "application/json",
            body.to_string().as_bytes(),
        )
    }

    fn token(&self, pairs: &[(&str, &str)]) -> Reply {
        self.send(
            "POST",
            "/oauth/token",
            &[],
            "application/x-www-form-urlencoded",
            form(pairs).as_bytes(),
        )
    }

    /// One MCP request with `bearer`, or none.
    fn mcp(&self, bearer: Option<&str>, body: Value) -> Reply {
        let auth = bearer.map(|token| format!("Bearer {token}"));
        let headers: Vec<(&str, &str)> = auth
            .as_deref()
            .map(|value| vec![("Authorization", value)])
            .unwrap_or_default();
        self.send(
            "POST",
            "/mcp",
            &headers,
            "application/json",
            body.to_string().as_bytes(),
        )
    }

    fn tools(&self, bearer: &str) -> Vec<String> {
        let reply = self.mcp(
            Some(bearer),
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
        );
        assert_eq!(reply.status, 200, "tools/list: {}", reply.text());
        reply.json()["result"]["tools"]
            .as_array()
            .expect("a tool list")
            .iter()
            .map(|tool| tool["name"].as_str().expect("a name").to_string())
            .collect()
    }

    fn register(&self, name: &str, redirects: &[&str]) -> Reply {
        self.send(
            "POST",
            "/oauth/register",
            &[],
            "application/json",
            json!({ "client_name": name, "redirect_uris": redirects, "token_endpoint_auth_method": "none" })
                .to_string()
                .as_bytes(),
        )
    }

    fn client(&self, name: &str) -> String {
        let reply = self.register(name, &[REDIRECT]);
        assert_eq!(reply.status, 201, "register: {}", reply.text());
        reply.json()["client_id"]
            .as_str()
            .expect("a client id")
            .to_string()
    }

    /// `GET /oauth/authorize` with the usual parameters, `changes` replacing or (with `None`) removing some.
    fn authorize(&self, client: &str, changes: &[(&str, Option<&str>)]) -> Reply {
        let challenge = challenge_of(VERIFIER);
        let mut params: Vec<(&str, String)> = vec![
            ("response_type", "code".to_string()),
            ("client_id", client.to_string()),
            ("redirect_uri", REDIRECT.to_string()),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256".to_string()),
            ("state", "xyz-state".to_string()),
            ("scope", "viewer".to_string()),
            ("resource", format!("{ORIGIN}/mcp")),
        ];
        for (key, value) in changes {
            params.retain(|(k, _)| k != key);
            if let Some(value) = value {
                params.push((key, value.to_string()));
            }
        }
        let pairs: Vec<(&str, &str)> = params.iter().map(|(k, v)| (*k, v.as_str())).collect();
        self.get(&format!("/oauth/authorize?{}", form(&pairs)))
    }

    /// The consent request id a consent page carries.
    fn request_id(reply: &Reply) -> String {
        assert_eq!(reply.status, 200, "the consent page: {}", reply.text());
        let text = reply.text();
        let at = text.find("data-request=\"").expect("a request id") + 14;
        text[at..at + text[at..].find('"').expect("its end")].to_string()
    }

    /// Begins and approves the consent request `id` with `key`; the page's answer.
    fn approve_with(&self, key: &SoftPasskey, id: &str) -> Reply {
        let begun = self.page_post("/oauth/authorize/begin", json!({ "id": id }));
        assert_eq!(begun.status, 200, "begin: {}", begun.text());
        let statement = begun.json()["statement"]
            .as_str()
            .expect("a statement")
            .to_string();
        let proof = key.proof_value(&Assert::over(NAMESPACE, statement.as_bytes()));
        self.page_post(
            "/oauth/authorize/approve",
            json!({ "id": id, "statement": statement, "proof": proof }),
        )
    }

    /// The whole flow for `client` at `scope`: consent, passkey, code. The code.
    fn code(&self, key: &SoftPasskey, client: &str, scope: &str) -> String {
        let page = self.authorize(client, &[("scope", Some(scope))]);
        let id = Rig::request_id(&page);
        let approved = self.approve_with(key, &id);
        assert_eq!(approved.status, 200, "approve: {}", approved.text());
        let redirect = approved.json()["redirect"]
            .as_str()
            .expect("a redirect")
            .to_string();
        assert!(redirect.starts_with(REDIRECT), "{redirect}");
        assert_eq!(param(&redirect, "state").as_deref(), Some("xyz-state"));
        assert_eq!(param(&redirect, "iss").as_deref(), Some(ORIGIN));
        param(&redirect, "code").expect("a code")
    }

    fn exchange(&self, client: &str, code: &str) -> Reply {
        self.token(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("client_id", client),
            ("redirect_uri", REDIRECT),
            ("code_verifier", VERIFIER),
            ("resource", &format!("{ORIGIN}/mcp")),
        ])
    }

    /// Access and refresh tokens for a new client at `scope`.
    fn connected(&self, key: &SoftPasskey, name: &str, scope: &str) -> (String, String, String) {
        let client = self.client(name);
        let code = self.code(key, &client, scope);
        let reply = self.exchange(&client, &code);
        assert_eq!(reply.status, 200, "exchange: {}", reply.text());
        let body = reply.json();
        (
            client,
            body["access_token"].as_str().expect("access").to_string(),
            body["refresh_token"].as_str().expect("refresh").to_string(),
        )
    }

    fn refresh(&self, client: &str, refresh: &str, scope: Option<&str>) -> Reply {
        let mut pairs = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
            ("client_id", client),
        ];
        if let Some(scope) = scope {
            pairs.push(("scope", scope));
        }
        self.token(&pairs)
    }

    /// The store as a restart reads it from disk.
    fn reopened(&self) -> OAuth {
        let ticks = Arc::clone(&self.clock);
        OAuth::open(
            ORIGIN,
            RP,
            Arc::clone(&self.kernel),
            Arc::new(move || ticks.load(Ordering::SeqCst)),
            &self.state_dir,
        )
        .expect("the store reopens")
        .with_log(Arc::new(|_| {}))
    }

    /// `POST /oauth/revoke` of `token` for `client`.
    fn revoke(&self, client: &str, token: &str) -> Reply {
        self.send(
            "POST",
            "/oauth/revoke",
            &[],
            "application/x-www-form-urlencoded",
            form(&[("token", token), ("client_id", client)]).as_bytes(),
        )
    }

    fn journal(&self) -> String {
        fs::read_to_string(self.state_dir.join(arca::mcp_oauth::JOURNAL_FILE)).unwrap_or_default()
    }
}

fn error_of(reply: &Reply) -> String {
    reply.json()["error"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[test]
fn an_mcp_client_given_only_the_url_is_told_where_to_ask_and_the_metadata_names_every_endpoint() {
    let (rig, _) = Rig::new("oauth-discovery");
    let reply = rig.mcp(
        None,
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
    );
    assert_eq!(reply.status, 401);
    let challenge = reply.header("www-authenticate").expect("a challenge");
    assert_eq!(
        challenge,
        format!(
            "Bearer resource_metadata=\"{ORIGIN}/.well-known/oauth-protected-resource/mcp\", scope=\"viewer\""
        )
    );
    // A token that is presented and not taken is told so (RFC 6750 3.1); with no token, the challenge stays bare.
    let dead = rig.mcp(
        Some("arca_at_0123456789012345678901234567890123456789012"),
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
    );
    assert_eq!(dead.status, 401);
    assert_eq!(
        dead.header("www-authenticate").expect("a challenge"),
        format!(
            "Bearer error=\"invalid_token\", resource_metadata=\"{ORIGIN}/.well-known/oauth-protected-resource/mcp\", scope=\"viewer\""
        )
    );
    for path in [
        "/.well-known/oauth-protected-resource/mcp",
        "/.well-known/oauth-protected-resource",
    ] {
        let metadata = rig.get(path);
        assert_eq!(metadata.status, 200, "{path}");
        let body = metadata.json();
        assert_eq!(body["resource"], json!(format!("{ORIGIN}/mcp")));
        assert_eq!(body["authorization_servers"], json!([ORIGIN]));
        assert_eq!(
            body["scopes_supported"],
            json!(["viewer", "doctor", "operator", "approvals"])
        );
    }
    let server = rig.get("/.well-known/oauth-authorization-server").json();
    assert_eq!(server["issuer"], json!(ORIGIN));
    assert_eq!(
        server["authorization_endpoint"],
        json!(format!("{ORIGIN}/oauth/authorize"))
    );
    assert_eq!(
        server["token_endpoint"],
        json!(format!("{ORIGIN}/oauth/token"))
    );
    assert_eq!(
        server["registration_endpoint"],
        json!(format!("{ORIGIN}/oauth/register"))
    );
    assert_eq!(server["code_challenge_methods_supported"], json!(["S256"]));
    assert_eq!(
        server["token_endpoint_auth_methods_supported"],
        json!(["none"])
    );
    assert_eq!(
        server["authorization_response_iss_parameter_supported"],
        json!(true)
    );
    // A browser client may read the metadata and register.
    let preflight = rig.send(
        "OPTIONS",
        "/oauth/token",
        &[("Origin", "https://inspector.example.com")],
        "",
        b"",
    );
    assert_eq!(preflight.status, 204);
    assert_eq!(
        preflight.header("access-control-allow-origin").as_deref(),
        Some("*")
    );
}

#[test]
fn the_full_flow_registers_asks_the_owner_and_the_issued_token_calls_mcp_with_its_scope() {
    let (rig, key) = Rig::new("oauth-flow");
    let client = rig.client("Claude Code");
    let page = rig.authorize(&client, &[]);
    let text = page.text();
    assert!(text.contains("Claude Code"), "the name is shown");
    assert!(
        text.contains("<strong id=\"redirect-host\">127.0.0.1</strong>"),
        "the redirect host is shown"
    );
    assert!(text.contains("<strong id=\"scope\">viewer</strong>"));
    assert!(!text.contains("name-shared"), "the name is unique");
    assert!(
        page.header("content-security-policy")
            .is_some_and(|csp| csp.contains("script-src 'self'"))
    );
    let id = Rig::request_id(&page);
    let approved = rig.approve_with(&key, &id);
    assert_eq!(approved.status, 200, "{}", approved.text());
    let redirect = approved.json()["redirect"]
        .as_str()
        .expect("a redirect")
        .to_string();
    let code = param(&redirect, "code").expect("a code");
    let reply = rig.exchange(&client, &code);
    assert_eq!(reply.status, 200, "{}", reply.text());
    assert_eq!(reply.header("cache-control").as_deref(), Some("no-store"));
    let body = reply.json();
    assert_eq!(body["token_type"], json!("Bearer"));
    assert_eq!(body["expires_in"], json!(3600));
    assert_eq!(body["scope"], json!("viewer"));
    let access = body["access_token"].as_str().expect("access").to_string();
    let refresh = body["refresh_token"].as_str().expect("refresh").to_string();
    assert!(access.starts_with("arca_at_") && refresh.starts_with("arca_rt_"));

    // The issued viewer token sees exactly what the stored viewer token sees.
    assert_eq!(rig.tools(&access), rig.tools(&rig.viewer));
    // A verb above its scope is refused with a step-up challenge naming the scope it needs.
    let refused = rig.mcp(
        Some(&access),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "service_remove", "arguments": { "name": "nats" } } }),
    );
    assert_eq!(refused.status, 403);
    let challenge = refused.header("www-authenticate").expect("a challenge");
    assert!(
        challenge.starts_with("Bearer error=\"insufficient_scope\", scope=\"doctor\""),
        "{challenge}"
    );

    // The store keeps digests only; the journal names the grant and no secret.
    let store =
        fs::read_to_string(rig.state_dir.join(arca::mcp_oauth::STORE_FILE)).expect("the store");
    for secret in [&access, &refresh, &code] {
        assert!(!store.contains(secret.as_str()));
        assert!(!rig.journal().contains(secret.as_str()));
    }
    let mode = fs::metadata(rig.state_dir.join(arca::mcp_oauth::STORE_FILE))
        .expect("the store")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    let journal = rig.journal();
    assert!(journal.contains("\"event\":\"registered\""));
    assert!(journal.contains("\"event\":\"granted\""));

    // The owner's list shows it connected.
    let list = rig.get("/approve/oauth/clients").json();
    let app = &list["clients"][0];
    assert_eq!(app["clientId"], json!(client));
    assert_eq!(app["connected"], json!(true));
    assert_eq!(app["redirectHosts"], json!(["127.0.0.1"]));
    assert_eq!(app["grants"][0]["scope"], json!("viewer"));
    assert!(app["grants"][0]["lastUsedAt"].is_string());
}

#[test]
fn an_operator_grant_sees_operator_verbs_and_a_tool_list_narrows_it() {
    let (rig, key) = Rig::new("oauth-operator");
    let (_, operator, _) = rig.connected(&key, "Desktop", "viewer operator");
    let tools = rig.tools(&operator);
    assert!(tools.iter().any(|tool| tool == "token_list"));
    assert!(tools.len() > rig.tools(&rig.viewer).len());
    let (_, narrow, _) = rig.connected(&key, "Narrow", "operator tool:service_list");
    assert_eq!(rig.tools(&narrow), vec!["service_list".to_string()]);
}

#[test]
fn stored_tokens_work_exactly_as_before() {
    let (rig, _) = Rig::new("oauth-static");
    let tools = rig.tools(&rig.viewer);
    assert!(tools.iter().any(|tool| tool == "arca_status"));
    let refused = rig.mcp(
        Some(&rig.viewer),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "service_remove", "arguments": { "name": "nats" } } }),
    );
    assert_eq!(refused.status, 403);
    assert_eq!(
        refused.header("www-authenticate"),
        None,
        "a stored token gets the bare 403"
    );
    // A stored token is not an issued one, and an issued-looking string is not a stored one.
    assert!(rig.oauth.lookup(&rig.viewer).is_none());
    let fake = format!("arca_at_{}", "A".repeat(43));
    assert_eq!(
        rig.mcp(
            Some(&fake),
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })
        )
        .status,
        401
    );
}

#[test]
fn pkce_is_required_and_a_wrong_verifier_gets_no_token() {
    let (rig, key) = Rig::new("oauth-pkce");
    let client = rig.client("App");
    let bad_pkce = [
        vec![("code_challenge", None)],
        vec![("code_challenge_method", Some("plain"))],
        vec![("code_challenge_method", None)],
        vec![("code_challenge", Some("short"))],
    ];
    // Before the owner approved the client, its redirect is not trusted: the error stays on the page.
    for changes in &bad_pkce {
        let reply = rig.authorize(&client, changes);
        assert_eq!(reply.status, 400, "{changes:?}");
        assert_eq!(reply.header("location"), None);
        assert!(reply.text().contains("PKCE"), "{}", reply.text());
    }
    let first = rig.code(&key, &client, "viewer");
    assert_eq!(rig.exchange(&client, &first).status, 200);
    // Once the owner approved it, the error goes back to the client, with its state.
    for changes in &bad_pkce {
        let reply = rig.authorize(&client, changes);
        assert_eq!(reply.status, 302, "{changes:?}");
        let location = reply.header("location").expect("a redirect");
        assert!(location.starts_with(REDIRECT));
        assert_eq!(
            param(&location, "error").as_deref(),
            Some("invalid_request")
        );
        assert_eq!(param(&location, "state").as_deref(), Some("xyz-state"));
    }
    let code = rig.code(&key, &client, "viewer");
    let wrong = rig.token(&[
        ("grant_type", "authorization_code"),
        ("code", &code),
        ("client_id", &client),
        ("redirect_uri", REDIRECT),
        (
            "code_verifier",
            "another-verifier-of-fifty-characters-0123456789abc",
        ),
    ]);
    assert_eq!(wrong.status, 400);
    assert_eq!(error_of(&wrong), "invalid_grant");
    let missing = rig.token(&[
        ("grant_type", "authorization_code"),
        ("code", &rig.code(&key, &client, "viewer")),
        ("client_id", &client),
        ("redirect_uri", REDIRECT),
    ]);
    assert_eq!(error_of(&missing), "invalid_grant");
}

#[test]
fn a_token_for_another_resource_is_never_issued() {
    let (rig, key) = Rig::new("oauth-resource");
    let client = rig.client("App");
    let reply = rig.authorize(
        &client,
        &[("resource", Some("https://other.example.com/mcp"))],
    );
    assert_eq!(reply.status, 400);
    assert_eq!(reply.header("location"), None);
    assert!(
        reply.text().contains("issues tokens for"),
        "{}",
        reply.text()
    );
    // The canonical form of this resource is accepted.
    let fine = rig.authorize(
        &client,
        &[("resource", Some("HTTPS://BOX1.example.com:8443/mcp/"))],
    );
    assert_eq!(fine.status, 200, "{}", fine.text());
    let code = rig.code(&key, &client, "viewer");
    let wrong = rig.token(&[
        ("grant_type", "authorization_code"),
        ("code", &code),
        ("client_id", &client),
        ("redirect_uri", REDIRECT),
        ("code_verifier", VERIFIER),
        ("resource", "https://other.example.com/mcp"),
    ]);
    assert_eq!(error_of(&wrong), "invalid_target");
}

#[test]
fn redirects_are_exact_and_loopback_or_https() {
    let (rig, key) = Rig::new("oauth-redirect");
    for bad in [
        "http://app.example.com/callback",
        "javascript:alert(1)",
        "https://app.example.com/cb#frag",
        "http://127.0.0.1.example.com/cb",
        "http://user@127.0.0.1/cb",
    ] {
        let reply = rig.register("Bad", &[bad]);
        assert_eq!(reply.status, 400, "{bad}");
        assert_eq!(error_of(&reply), "invalid_redirect_uri");
    }
    for good in [
        "http://localhost:4567/cb",
        "http://[::1]:4567/cb",
        "https://app.example.com/oauth/cb",
    ] {
        assert_eq!(rig.register("Good", &[good]).status, 201, "{good}");
    }
    let client = rig.client("App");
    let reply = rig.authorize(
        &client,
        &[("redirect_uri", Some("http://127.0.0.1:33419/callback"))],
    );
    assert_eq!(
        reply.status, 400,
        "a redirect it did not register is answered on the page"
    );
    assert!(reply.header("location").is_none());
    let unknown = rig.authorize("mcpc-000000000000000000000000", &[]);
    assert_eq!(unknown.status, 400);
    assert!(unknown.header("location").is_none());
    // The token request must name the redirect the code went to.
    let code = rig.code(&key, &client, "viewer");
    let other = rig.token(&[
        ("grant_type", "authorization_code"),
        ("code", &code),
        ("client_id", &client),
        ("redirect_uri", "http://127.0.0.1:33419/callback"),
        ("code_verifier", VERIFIER),
    ]);
    assert_eq!(error_of(&other), "invalid_grant");
}

#[test]
fn only_the_owners_passkey_over_the_begun_statement_approves() {
    let (rig, key) = Rig::new("oauth-passkey");
    let client = rig.client("App");
    let id = Rig::request_id(&rig.authorize(&client, &[]));
    // Another passkey is refused, and nothing is approved.
    let stranger = SoftPasskey::from_seed(2);
    let refused = rig.approve_with(&stranger, &id);
    assert_eq!(refused.status, 403, "{}", refused.text());
    // A statement that is not the one begun is refused.
    let begun = rig.page_post("/oauth/authorize/begin", json!({ "id": id }));
    let statement = begun.json()["statement"]
        .as_str()
        .expect("a statement")
        .to_string();
    let other = statement.replace("mcp_authorize", "service_remove");
    let proof = key.proof_value(&Assert::over(NAMESPACE, other.as_bytes()));
    let stale = rig.page_post(
        "/oauth/authorize/approve",
        json!({ "id": id, "statement": other, "proof": proof }),
    );
    assert_eq!(stale.status, 409);
    // Without the page's Origin the consent POSTs are refused.
    let no_origin = rig.send(
        "POST",
        "/oauth/authorize/begin",
        &[],
        "application/json",
        json!({ "id": id }).to_string().as_bytes(),
    );
    assert_eq!(no_origin.status, 403);
    // The owner's passkey approves once; the same request cannot be approved again.
    let approved = rig.approve_with(&key, &id);
    assert_eq!(approved.status, 200, "{}", approved.text());
    let again = rig.page_post("/oauth/authorize/begin", json!({ "id": id }));
    assert_eq!(again.status, 404);
    // Deny sends access_denied back with the state.
    let id = Rig::request_id(&rig.authorize(&client, &[]));
    let denied = rig.page_post("/oauth/authorize/deny", json!({ "id": id }));
    let redirect = denied.json()["redirect"]
        .as_str()
        .expect("a redirect")
        .to_string();
    assert_eq!(param(&redirect, "error").as_deref(), Some("access_denied"));
    assert_eq!(param(&redirect, "state").as_deref(), Some("xyz-state"));
}

#[test]
fn admin_is_never_granted_and_a_refresh_never_widens_the_scope() {
    let (rig, key) = Rig::new("oauth-scope");
    let client = rig.client("App");
    let reply = rig.authorize(&client, &[("scope", Some("operator admin"))]);
    assert_eq!(reply.status, 400);
    assert!(
        reply.text().contains("admin is never granted"),
        "{}",
        reply.text()
    );

    let (client, _, refresh) = rig.connected(&key, "Viewer app", "viewer");
    let wider = rig.refresh(&client, &refresh, Some("doctor"));
    assert_eq!(wider.status, 400);
    assert_eq!(error_of(&wider), "invalid_scope");
    // The refused refresh token still works, once.
    let same = rig.refresh(&client, &refresh, Some("viewer"));
    assert_eq!(same.status, 200, "{}", same.text());

    let (client, _, refresh) = rig.connected(&key, "Tool app", "doctor tool:service_list");
    let more = rig.refresh(
        &client,
        &refresh,
        Some("doctor tool:service_list tool:node_metrics"),
    );
    assert_eq!(error_of(&more), "invalid_scope");
    let narrower = rig.refresh(&client, &refresh, Some("viewer"));
    assert_eq!(narrower.status, 200, "{}", narrower.text());
    assert_eq!(narrower.json()["scope"], json!("viewer tool:service_list"));
    // A narrowing refresh narrows that token pair only: a refresh with no scope gets what the owner approved back
    // (RFC 6749 6), and asking for it again is no widening.
    let next = narrower.json()["refresh_token"]
        .as_str()
        .expect("a refresh")
        .to_string();
    let back = rig.refresh(&client, &next, None);
    assert_eq!(back.status, 200, "{}", back.text());
    assert_eq!(back.json()["scope"], json!("doctor tool:service_list"));
    let next = back.json()["refresh_token"]
        .as_str()
        .expect("a refresh")
        .to_string();
    let again = rig.refresh(&client, &next, Some("doctor"));
    assert_eq!(again.status, 200, "{}", again.text());
    assert_eq!(again.json()["scope"], json!("doctor tool:service_list"));
}

#[test]
fn the_owners_list_shows_the_most_an_app_may_do_and_its_latest_use_after_a_step_up() {
    let (rig, key) = Rig::new("oauth-two-grants");
    // The documented path: the app connects as viewer, a doctor verb answers insufficient_scope, the owner approves doctor.
    let client = rig.client("App");
    let viewer = rig.exchange(&client, &rig.code(&key, &client, "viewer"));
    assert_eq!(viewer.status, 200);
    let viewer_access = viewer.json()["access_token"]
        .as_str()
        .expect("access")
        .to_string();
    assert_eq!(rig.tools(&viewer_access), rig.tools(&rig.viewer));
    rig.advance(60_000);
    let doctor = rig.exchange(&client, &rig.code(&key, &client, "doctor"));
    assert_eq!(doctor.status, 200);
    let doctor_access = doctor.json()["access_token"]
        .as_str()
        .expect("access")
        .to_string();
    rig.advance(60_000);
    rig.tools(&doctor_access);
    let list = rig.get("/approve/oauth/clients").json();
    let app = &list["clients"][0];
    assert_eq!(app["grants"].as_array().map(Vec::len), Some(2));
    assert_eq!(app["grants"][0]["scope"], json!("viewer"));
    assert_eq!(app["scope"], json!("doctor"));
    assert_eq!(app["lastUsedAt"], app["grants"][1]["lastUsedAt"]);
    assert_ne!(app["lastUsedAt"], app["grants"][0]["lastUsedAt"]);
    // The card reads the app's own fields, never the first grant's.
    let page = include_str!("../src/approve_page.html");
    assert!(page.contains("fact(\"Allowed\", String(app.scope))"));
    assert!(!page.contains("app.grants[0]"));
}

#[test]
fn a_revocation_that_cannot_be_saved_is_refused_with_503_and_still_ends_the_grant_now() {
    let (rig, key) = Rig::new("oauth-unsaved");
    let (client, access, refresh) = rig.connected(&key, "App", "viewer");
    let mode = |bits| {
        fs::set_permissions(&rig.state_dir, fs::Permissions::from_mode(bits)).expect("chmod")
    };
    mode(0o500);
    let reply = rig.send(
        "POST",
        "/oauth/revoke",
        &[],
        "application/x-www-form-urlencoded",
        form(&[("token", &refresh), ("client_id", &client)]).as_bytes(),
    );
    mode(0o700);
    assert_eq!(reply.status, 503, "{}", reply.text());
    assert_eq!(error_of(&reply), "temporarily_unavailable");
    let call = rig.mcp(
        Some(&access),
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
    );
    assert_eq!(call.status, 401);
    // The journal says it, unsaved, and the store on disk still holds the grant.
    assert!(
        rig.journal().contains("\"event\":\"revoked\""),
        "{}",
        rig.journal()
    );
    assert!(
        rig.journal().contains("\"saved\":false"),
        "{}",
        rig.journal()
    );
    assert!(rig.reopened().lookup(&access).is_some(), "not saved yet");
    // The client's retry saves it, then answers 200: a restart no longer brings the grant back.
    let retry = rig.revoke(&client, &refresh);
    assert_eq!(retry.status, 200, "{}", retry.text());
    assert!(
        rig.reopened().lookup(&access).is_none(),
        "saved by the retry"
    );
    assert!(
        rig.journal().contains("\"event\":\"saved\""),
        "{}",
        rig.journal()
    );
}

#[test]
fn a_reused_code_or_refresh_token_that_cannot_be_saved_is_journalled_and_saved_by_the_next_revoke()
{
    let (rig, key) = Rig::new("oauth-unsaved-reuse");
    let mode = |bits| {
        fs::set_permissions(&rig.state_dir, fs::Permissions::from_mode(bits)).expect("chmod")
    };
    // A code used twice while the store cannot be written.
    let client = rig.client("App");
    let code = rig.code(&key, &client, "viewer");
    let first = rig.exchange(&client, &code);
    let access = first.json()["access_token"]
        .as_str()
        .expect("access")
        .to_string();
    mode(0o500);
    let reused = rig.exchange(&client, &code);
    mode(0o700);
    assert_eq!(reused.status, 503, "{}", reused.text());
    assert!(
        rig.journal().contains("\"event\":\"code_reused\""),
        "{}",
        rig.journal()
    );
    assert!(rig.reopened().lookup(&access).is_some(), "not saved yet");
    assert_eq!(rig.revoke(&client, "not-a-token").status, 200);
    assert!(
        rig.reopened().lookup(&access).is_none(),
        "saved by the next revoke"
    );
    // A rotated refresh token used again while the store cannot be written.
    let (client, access, first) = rig.connected(&key, "App 2", "viewer");
    let rotated = rig.refresh(&client, &first, None);
    assert_eq!(rotated.status, 200);
    mode(0o500);
    let reused = rig.refresh(&client, &first, None);
    mode(0o700);
    assert_eq!(reused.status, 503, "{}", reused.text());
    let lines = rig.journal();
    let line = lines
        .lines()
        .find(|line| line.contains("\"event\":\"refresh_reused\""))
        .expect("journalled");
    assert!(line.contains("\"saved\":false"), "{line}");
    assert!(rig.reopened().lookup(&access).is_some(), "not saved yet");
    assert_eq!(rig.revoke(&client, "not-a-token").status, 200);
    assert!(
        rig.reopened().lookup(&access).is_none(),
        "saved by the next revoke"
    );
    let current = rotated.json()["refresh_token"]
        .as_str()
        .expect("refresh")
        .to_string();
    assert_eq!(
        error_of(&rig.refresh(&client, &current, None)),
        "invalid_grant"
    );
}

#[test]
fn refresh_tokens_rotate_and_one_used_again_ends_the_grant() {
    let (rig, key) = Rig::new("oauth-rotate");
    let (client, access, first) = rig.connected(&key, "App", "viewer");
    let rotated = rig.refresh(&client, &first, None);
    assert_eq!(rotated.status, 200, "{}", rotated.text());
    let second = rotated.json()["refresh_token"]
        .as_str()
        .expect("a refresh")
        .to_string();
    let access2 = rotated.json()["access_token"]
        .as_str()
        .expect("an access")
        .to_string();
    assert_ne!(second, first);
    assert_eq!(rig.tools(&access2), rig.tools(&rig.viewer));
    // The first refresh token, used again: refused, and the whole grant ends.
    let reused = rig.refresh(&client, &first, None);
    assert_eq!(error_of(&reused), "invalid_grant");
    assert_eq!(
        error_of(&rig.refresh(&client, &second, None)),
        "invalid_grant"
    );
    for token in [&access, &access2] {
        let reply = rig.mcp(
            Some(token),
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
        );
        assert_eq!(reply.status, 401);
    }
    assert!(rig.journal().contains("\"event\":\"refresh_reused\""));
}

#[test]
fn access_tokens_expire_in_an_hour_and_refresh_tokens_in_thirty_days() {
    let (rig, key) = Rig::new("oauth-expiry");
    let (client, access, refresh) = rig.connected(&key, "App", "viewer");
    rig.advance(HOUR - 1000);
    assert_eq!(rig.tools(&access), rig.tools(&rig.viewer));
    rig.advance(2000);
    let expired = rig.mcp(
        Some(&access),
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
    );
    assert_eq!(expired.status, 401);
    let fresh = rig.refresh(&client, &refresh, None);
    assert_eq!(fresh.status, 200);
    let refresh = fresh.json()["refresh_token"]
        .as_str()
        .expect("a refresh")
        .to_string();
    rig.advance(30 * DAY + 1000);
    assert_eq!(
        error_of(&rig.refresh(&client, &refresh, None)),
        "invalid_grant"
    );
    // A code lives a minute.
    let client = rig.client("Late");
    let code = rig.code(&key, &client, "viewer");
    rig.advance(61_000);
    assert_eq!(error_of(&rig.exchange(&client, &code)), "invalid_grant");
}

#[test]
fn a_code_works_once_and_one_used_again_ends_what_it_made() {
    let (rig, key) = Rig::new("oauth-code");
    let client = rig.client("App");
    let code = rig.code(&key, &client, "viewer");
    let first = rig.exchange(&client, &code);
    assert_eq!(first.status, 200);
    let access = first.json()["access_token"]
        .as_str()
        .expect("access")
        .to_string();
    let second = rig.exchange(&client, &code);
    assert_eq!(error_of(&second), "invalid_grant");
    let reply = rig.mcp(
        Some(&access),
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
    );
    assert_eq!(reply.status, 401);
    // A code is bound to its client.
    let other = rig.client("Other");
    let code = rig.code(&key, &client, "viewer");
    let stolen = rig.token(&[
        ("grant_type", "authorization_code"),
        ("code", &code),
        ("client_id", &other),
        ("redirect_uri", REDIRECT),
        ("code_verifier", VERIFIER),
    ]);
    assert_eq!(error_of(&stolen), "invalid_grant");
}

#[test]
fn a_revoked_client_loses_its_tokens_its_refresh_and_its_consent() {
    let (rig, key) = Rig::new("oauth-revoke");
    let (client, access, refresh) = rig.connected(&key, "App", "viewer");
    // The page's revoke needs the page's Origin.
    let no_origin = rig.send(
        "POST",
        "/approve/oauth/revoke",
        &[],
        "application/json",
        json!({ "clientId": client }).to_string().as_bytes(),
    );
    assert_eq!(no_origin.status, 403);
    assert_eq!(rig.tools(&access), rig.tools(&rig.viewer));
    let revoked = rig.page_post("/approve/oauth/revoke", json!({ "clientId": client }));
    assert_eq!(revoked.status, 200, "{}", revoked.text());
    let reply = rig.mcp(
        Some(&access),
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
    );
    assert_eq!(reply.status, 401);
    assert_eq!(
        error_of(&rig.refresh(&client, &refresh, None)),
        "invalid_grant"
    );
    assert_eq!(rig.authorize(&client, &[]).status, 400);
    assert!(rig.journal().contains("\"event\":\"revoked\""));
    let list = rig.get("/approve/oauth/clients").json();
    assert_eq!(list["clients"], json!([]));
}

#[test]
fn the_client_revokes_its_own_tokens_with_rfc_7009() {
    let (rig, key) = Rig::new("oauth-7009");
    let (client, access, refresh) = rig.connected(&key, "App", "viewer");
    let revoke = |token: &str| {
        rig.send(
            "POST",
            "/oauth/revoke",
            &[],
            "application/x-www-form-urlencoded",
            form(&[("token", token), ("client_id", &client)]).as_bytes(),
        )
    };
    assert_eq!(revoke(&access).status, 200);
    let reply = rig.mcp(
        Some(&access),
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
    );
    assert_eq!(reply.status, 401);
    // The refresh token still works after its access token is revoked; revoking it ends the grant.
    let fresh = rig.refresh(&client, &refresh, None);
    assert_eq!(fresh.status, 200);
    let refresh = fresh.json()["refresh_token"]
        .as_str()
        .expect("a refresh")
        .to_string();
    assert_eq!(revoke(&refresh).status, 200);
    assert_eq!(
        error_of(&rig.refresh(&client, &refresh, None)),
        "invalid_grant"
    );
    assert_eq!(revoke("not-a-token").status, 200);
}

#[test]
fn two_clients_with_one_name_are_marked_on_the_consent_page() {
    let (rig, _) = Rig::new("oauth-names");
    let first = rig.client("Claude Code");
    let page = rig.authorize(&first, &[]);
    assert!(!page.text().contains("name-shared"));
    let _second = rig.client("claude code ");
    let page = rig.authorize(&first, &[]);
    assert!(
        page.text().contains("id=\"name-shared\""),
        "{}",
        page.text()
    );
    // A name is shown as text, never as markup.
    let tricky = rig.client("<img src=x>");
    let page = rig.authorize(&tricky, &[]);
    assert!(page.text().contains("&lt;img src=x&gt;"));
    assert!(!page.text().contains("<img src=x>"));
}

#[test]
fn the_terminal_lists_and_revokes_with_its_proof_file_only() {
    let (rig, key) = Rig::new("oauth-terminal");
    let (client, access, _) = rig.connected(&key, "App", "viewer");
    let proof = "0123456789abcdef0123456789abcdef";
    let without = rig.send(
        "POST",
        "/local/oauth/clients",
        &[],
        "application/json",
        json!({ "proof": proof }).to_string().as_bytes(),
    );
    assert_eq!(without.status, 403);
    let write_proof = || {
        let path = rig
            .state_dir
            .join(format!("{}{proof}", approve_page::LOCAL_PROOF_PREFIX));
        fs::write(&path, b"").expect("the proof file");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("mode 0600");
    };
    write_proof();
    let list = rig.send(
        "POST",
        "/local/oauth/clients",
        &[],
        "application/json",
        json!({ "proof": proof }).to_string().as_bytes(),
    );
    assert_eq!(list.status, 200, "{}", list.text());
    assert_eq!(list.json()["clients"][0]["clientId"], json!(client));
    write_proof();
    let revoked = rig.send(
        "POST",
        "/local/oauth/revoke",
        &[],
        "application/json",
        json!({ "proof": proof, "clientId": client })
            .to_string()
            .as_bytes(),
    );
    assert_eq!(revoked.status, 200, "{}", revoked.text());
    let reply = rig.mcp(
        Some(&access),
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
    );
    assert_eq!(reply.status, 401);
}

#[test]
fn grants_survive_a_restart_of_the_store() {
    let (rig, key) = Rig::new("oauth-restart");
    let (_, access, _) = rig.connected(&key, "App", "viewer");
    let ticks = Arc::clone(&rig.clock);
    let reopened = OAuth::open(
        ORIGIN,
        RP,
        Arc::clone(&rig.kernel),
        Arc::new(move || ticks.load(Ordering::SeqCst)),
        &rig.state_dir,
    )
    .expect("the store opens again");
    let caller = reopened.lookup(&access).expect("the token still works");
    assert_eq!(caller.scope, Scope::Viewer);
    assert!(caller.token_id.starts_with("oauth:mcpc-"));
    // The audience: a server for another resource never accepts it (RFC 8707).
    let ticks = Arc::clone(&rig.clock);
    let elsewhere = OAuth::open(
        "https://other.example.com",
        "other.example.com",
        Arc::clone(&rig.kernel),
        Arc::new(move || ticks.load(Ordering::SeqCst)),
        &rig.state_dir,
    )
    .expect("the store opens for another origin");
    assert!(
        elsewhere.lookup(&access).is_none(),
        "a token is for its resource only"
    );
}

// ---------------------------------------------------------------------------------------------------------------------
// The owner's signed-in session: an app connected with `approvals`.

use arca::action_proposal::ActionKind;
use arca::handlers::{OwnerApprovalPending, TerminalApproval};
use arca::mcp_oauth::{STEP_UP_MS, Session};
use arca_mcp::{Asking, CallError, Caller, TerminalApprover, ToolHandler};

impl Rig {
    fn clock_fn(&self) -> Clock {
        let ticks = Arc::clone(&self.clock);
        Arc::new(move || ticks.load(Ordering::SeqCst))
    }

    fn now(&self) -> u64 {
        self.clock.load(Ordering::SeqCst)
    }

    /// The caller an access token is, as the MCP door sees it.
    fn caller(&self, access: &str) -> Caller {
        self.oauth.lookup(access).expect("a live access token")
    }

    /// Waits for the owner, as a worker's `action_propose` makes it: `action` with `args`, now.
    fn propose(&self, action: ActionKind, args: Value) -> String {
        let now = self.now();
        self.kernel
            .lock()
            .expect("kernel")
            .propose_action(action, args, now, None)
            .expect("proposed")
            .id
    }

    fn pending_handler(&self) -> OwnerApprovalPending {
        OwnerApprovalPending {
            kernel: Arc::clone(&self.kernel),
            clock: self.clock_fn(),
            requests: Arc::new(
                arca::approval_requests::ApprovalRequests::open(&self.state_dir, None, self.now())
                    .expect("the requests open"),
            ),
            oauth: Some(Arc::clone(&self.oauth)),
            page: Some(format!("{ORIGIN}/approve")),
        }
    }

    fn terminal(&self) -> TerminalApproval {
        TerminalApproval {
            kernel: Arc::clone(&self.kernel),
            clock: self.clock_fn(),
            actions: arca::handlers::ActionExecutor::new(Vec::new()),
            oauth: Some(Arc::clone(&self.oauth)),
        }
    }

    /// Confirms the session of `grant` again on the step-up page, with `key`.
    fn step_up(&self, key: &SoftPasskey, grant: &str) -> Reply {
        let begun = self.page_post("/oauth/step-up/begin", json!({ "id": grant }));
        assert_eq!(begun.status, 200, "begin: {}", begun.text());
        let statement = begun.json()["statement"]
            .as_str()
            .expect("a statement")
            .to_string();
        let proof = key.proof_value(&Assert::over(NAMESPACE, statement.as_bytes()));
        self.page_post(
            "/oauth/step-up/approve",
            json!({ "id": grant, "statement": statement, "proof": proof }),
        )
    }
}

fn step_up_url(session: Session) -> String {
    match session {
        Session::StepUp { url } => url,
        other => panic!("a step-up, not {other:?}"),
    }
}

#[test]
fn an_app_connected_with_approvals_is_the_owners_session_and_its_passkey_counts_for_12_hours() {
    let (rig, key) = Rig::new("oauth-session");
    let client = rig.client("Owner app");
    let page = rig.authorize(&client, &[("scope", Some("viewer approvals"))]);
    assert!(page.text().contains("id=\"approvals\""), "{}", page.text());
    assert!(
        page.text()
            .contains("<strong id=\"scope\">viewer approvals</strong>")
    );
    let code = rig.code(&key, &client, "viewer approvals");
    let tokens = rig.exchange(&client, &code).json();
    assert_eq!(tokens["scope"], json!("viewer approvals"));
    let session = rig.caller(tokens["access_token"].as_str().expect("access"));
    assert!(
        session.token_id.starts_with(&format!("oauth:{client}/g-")),
        "{}",
        session.token_id
    );
    assert_eq!(session.scope, Scope::Viewer);
    assert!(matches!(rig.oauth.session(&session), Session::Ready { .. }));
    // An app connected without approvals is never the owner's session, and cannot ask for it at a refresh.
    let (plain_client, plain_access, plain_refresh) = rig.connected(&key, "Plain app", "doctor");
    let plain = rig.caller(&plain_access);
    assert_eq!(rig.oauth.session(&plain), Session::None);
    let wider = rig.refresh(&plain_client, &plain_refresh, Some("doctor approvals"));
    assert_eq!(error_of(&wider), "invalid_scope");
    let list = rig.get("/approve/oauth/clients").json();
    let owner_app = list["clients"]
        .as_array()
        .expect("clients")
        .iter()
        .find(|app| app["clientId"] == json!(client))
        .expect("the owner's app")
        .clone();
    assert_eq!(owner_app["approvals"], json!(true));
    // Twelve hours on, the consent's passkey no longer counts: the session is sent to the step-up page.
    rig.advance(STEP_UP_MS + 1);
    let url = step_up_url(rig.oauth.session(&session));
    let grant = url.split("grant=").nth(1).expect("a grant").to_string();
    assert!(session.token_id.ends_with(&grant), "{url}");
    let path = url.strip_prefix(ORIGIN).expect("on the page origin");
    let step_page = rig.get(path);
    assert_eq!(step_page.status, 200);
    assert!(
        step_page
            .text()
            .contains("data-begin=\"/oauth/step-up/begin\"")
    );
    assert!(step_page.text().contains("Owner app"));
    // A passkey over another statement does not confirm it.
    rig.page_post("/oauth/step-up/begin", json!({ "id": grant }));
    let proof = key.proof_value(&Assert::over(NAMESPACE, b"{\"another\":\"statement\"}"));
    let stale = rig.page_post(
        "/oauth/step-up/approve",
        json!({ "id": grant, "statement": "{\"another\":\"statement\"}", "proof": proof }),
    );
    assert_eq!(stale.status, 409, "{}", stale.text());
    assert!(matches!(
        rig.oauth.session(&session),
        Session::StepUp { .. }
    ));
    // One tap on the page confirms it for another twelve hours.
    let confirmed = rig.step_up(&key, &grant);
    assert_eq!(confirmed.status, 200, "{}", confirmed.text());
    assert!(matches!(rig.oauth.session(&session), Session::Ready { .. }));
    assert!(rig.journal().contains("\"event\":\"stepped_up\""));
    rig.advance(STEP_UP_MS + 1);
    assert!(matches!(
        rig.oauth.session(&session),
        Session::StepUp { .. }
    ));
    // A plain app's grant has no step-up page.
    let plain_grant = plain.token_id;
    let plain_grant = plain_grant.rsplit('/').next().expect("a grant");
    assert_eq!(
        rig.get(&format!("/oauth/step-up?grant={plain_grant}"))
            .status,
        404
    );
    // Revoked, the session is gone.
    rig.oauth.revoke_client(&client, "test").expect("revoked");
    assert_eq!(rig.oauth.session(&session), Session::None);
}

#[test]
fn waiting_plans_stay_open_while_the_owners_session_is_connected_and_lapse_after() {
    let (rig, key) = Rig::new("oauth-hold");
    let minute = 60 * 1000;
    // Without a session, a plan lapses after its fifteen minutes as ever.
    let lapsing = rig.propose(
        ActionKind::InstanceStop,
        json!({ "project": "p1", "instance": "old" }),
    );
    rig.advance(16 * minute);
    rig.kernel.lock().expect("kernel").hold_proposals(rig.now());
    assert!(
        rig.pending_handler()
            .call(
                json!({}),
                &rig.caller(&rig.connected(&key, "Plain", "viewer").1)
            )
            .expect("listed")["plans"]
            .as_array()
            .expect("plans")
            .iter()
            .all(|plan| plan["id"] != json!(lapsing))
    );
    // With the owner's session connected, a plan is held forward while time passes.
    let (client, access, _) = rig.connected(&key, "Owner app", "viewer approvals");
    let session = rig.caller(&access);
    assert!(
        rig.kernel
            .lock()
            .expect("kernel")
            .owner_session_live(rig.now())
    );
    let held = rig.propose(
        ActionKind::InstanceStop,
        json!({ "project": "p1", "instance": "api" }),
    );
    // A plan that erases keeps its passkey card: the session is never asked about it, so nothing holds it.
    let erasing = rig.propose(ActionKind::ServiceRemove, json!({ "name": "demo" }));
    assert_eq!(
        rig.pending_handler()
            .call(json!({}), &session)
            .expect("listed")["plans"]
            .as_array()
            .expect("plans")
            .iter()
            .find(|plan| plan["id"] == json!(erasing))
            .expect("waiting")["heldForSession"],
        json!(false)
    );
    let made = rig.now();
    for _ in 0..6 {
        rig.advance(10 * minute);
        rig.kernel.lock().expect("kernel").hold_proposals(rig.now());
    }
    let listing = rig
        .pending_handler()
        .call(json!({}), &session)
        .expect("listed");
    let plan = listing["plans"]
        .as_array()
        .expect("plans")
        .iter()
        .find(|plan| plan["id"] == json!(held))
        .expect("still waiting an hour on")
        .clone();
    assert_eq!(plan["heldForSession"], json!(true));
    assert!(plan["expiresAt"].as_u64().expect("an expiry") > made + 60 * minute);
    assert!(
        listing["plans"]
            .as_array()
            .expect("plans")
            .iter()
            .all(|plan| plan["id"] != json!(erasing))
    );
    // The card says so: the plan listing marks it held.
    let card = rig.get("/approve/plans");
    assert_eq!(card.status, 200, "{}", card.text());
    assert!(
        card.text().contains("\"heldForSession\":true"),
        "{}",
        card.text()
    );
    // Once the session is gone, nothing holds it: it lapses after its last fifteen minutes.
    rig.oauth.revoke_client(&client, "test").expect("revoked");
    assert!(
        !rig.kernel
            .lock()
            .expect("kernel")
            .owner_session_live(rig.now())
    );
    rig.advance(16 * minute);
    rig.kernel.lock().expect("kernel").hold_proposals(rig.now());
    let after = rig
        .pending_handler()
        .call(
            json!({}),
            &rig.caller(&rig.connected(&key, "Plain 2", "viewer").1),
        )
        .expect("listed");
    assert!(
        after["plans"]
            .as_array()
            .expect("plans")
            .iter()
            .all(|plan| plan["id"] != json!(held))
    );
}

#[test]
fn the_session_is_asked_only_about_plans_that_do_not_erase_and_others_list_only() {
    let (rig, key) = Rig::new("oauth-pending");
    let stop = rig.propose(
        ActionKind::InstanceStop,
        json!({ "project": "p1", "instance": "api" }),
    );
    let remove = rig.propose(ActionKind::ServiceRemove, json!({ "name": "demo" }));
    let (_, access, _) = rig.connected(&key, "Owner app", "viewer approvals");
    let session = rig.caller(&access);
    let listing = rig
        .pending_handler()
        .call(json!({}), &session)
        .expect("listed");
    assert_eq!(listing["session"], json!("asks"));
    assert_eq!(listing["page"], json!(format!("{ORIGIN}/approve")));
    let row = |listing: &Value, id: &str| {
        listing["plans"]
            .as_array()
            .expect("plans")
            .iter()
            .find(|plan| plan["id"] == json!(id))
            .expect("listed")
            .clone()
    };
    assert_eq!(row(&listing, &stop)["askedHere"], json!(true));
    assert_eq!(row(&listing, &remove)["askedHere"], json!(false));
    assert!(
        row(&listing, &remove)["why"]
            .as_str()
            .expect("why")
            .contains("passkey")
    );
    arca_api::conforms(
        &(arca_api::verb("owner_approval_pending")
            .expect("the verb")
            .result)(),
        &listing,
    )
    .expect("the listing fits its schema");
    // The door asks about the plan that does not erase, only.
    let terminal = rig.terminal();
    assert_eq!(terminal.may_ask(&session), Asking::Yes);
    let asked: Vec<String> = terminal
        .waiting("owner_approval_pending", &listing, &session)
        .into_iter()
        .map(|plan| plan.id)
        .collect();
    assert_eq!(asked, vec![stop.clone()]);
    // A plan that erases is refused to the session even when reached directly, and nothing runs.
    let created = row(&listing, &remove)["createdAt"].as_u64().expect("made");
    match terminal.approve(&remove, created, &session) {
        Err(CallError::Refused { code, .. }) => assert_eq!(code, "PasskeyOnly"),
        other => panic!("refused, not {other:?}"),
    }
    assert!(
        rig.kernel
            .lock()
            .expect("kernel")
            .action_proposals()
            .iter()
            .any(|p| p.id == remove)
    );
    // Any other app lists only, and is never asked or let approve.
    let (_, plain_access, _) = rig.connected(&key, "Plain app", "operator");
    let plain = rig.caller(&plain_access);
    let listed = rig
        .pending_handler()
        .call(json!({}), &plain)
        .expect("listed");
    assert_eq!(listed["session"], json!("none"));
    assert_eq!(row(&listed, &stop)["askedHere"], json!(false));
    assert_eq!(terminal.may_ask(&plain), Asking::No);
    assert!(matches!(
        terminal.approve(&stop, created, &plain),
        Err(CallError::Forbidden { .. })
    ));
    // Twelve hours on, the session lists with the step-up page and is not let approve until it confirms.
    rig.advance(STEP_UP_MS + 1);
    let stale = rig
        .pending_handler()
        .call(json!({}), &session)
        .expect("listed");
    assert_eq!(stale["session"], json!("step-up"));
    assert!(
        stale["stepUp"]
            .as_str()
            .expect("a page")
            .starts_with(&format!("{ORIGIN}/oauth/step-up?grant=g-"))
    );
    assert!(matches!(terminal.may_ask(&session), Asking::StepUp { .. }));
    assert!(matches!(
        terminal.approve(&stop, created, &session),
        Err(CallError::Forbidden { .. })
    ));
}

/// Whether a ninth plan is taken now: the kernel keeps at most eight waiting.
fn ninth_taken(rig: &Rig) -> bool {
    let now = rig.now();
    rig.kernel
        .lock()
        .expect("kernel")
        .propose_action(
            ActionKind::InstanceStop,
            json!({ "project": "p1", "instance": "ninth" }),
            now,
            None,
        )
        .is_ok()
}

#[test]
fn held_plans_lapse_a_day_after_they_were_made_so_undecided_plans_never_fill_the_queue_for_good() {
    let (rig, key) = Rig::new("oauth-hold-bound");
    let _session = rig.connected(&key, "Owner app", "viewer approvals");
    let minute = 60 * 1000;
    for n in 0..arca::action_proposal::MAX_WAITING {
        rig.propose(
            ActionKind::InstanceStop,
            json!({ "project": "p1", "instance": format!("i{n}") }),
        );
    }
    let tick = |times: u64| {
        for _ in 0..times {
            rig.advance(10 * minute);
            rig.kernel.lock().expect("kernel").hold_proposals(rig.now());
        }
    };
    // Held an hour on, with nobody deciding: the queue is still full.
    tick(6);
    assert!(
        rig.kernel
            .lock()
            .expect("kernel")
            .owner_session_live(rig.now())
    );
    assert!(!ninth_taken(&rig), "held plans still wait an hour on");
    // A day after they were made the hold ends; they lapse after their last fifteen minutes and the queue takes plans
    // again, the session still connected.
    tick(25 * 6 - 6);
    assert!(
        rig.kernel
            .lock()
            .expect("kernel")
            .owner_session_live(rig.now())
    );
    assert!(ninth_taken(&rig), "25 hours on, a ninth plan is taken");
}

/// When the waiting proposal `id` was made.
fn created(rig: &Rig, id: &str) -> u64 {
    rig.kernel
        .lock()
        .expect("kernel")
        .action_proposals()
        .iter()
        .find(|proposal| proposal.id == id)
        .expect("waiting")
        .created_ms
}

/// The waiting proposal `id` as `action_propose` answers with it.
fn proposed(rig: &Rig, id: &str) -> Value {
    let kernel = rig.kernel.lock().expect("kernel");
    let proposal = kernel
        .action_proposals()
        .iter()
        .find(|proposal| proposal.id == id)
        .expect("waiting");
    json!({
        "kind": "action", "id": proposal.id, "action": proposal.action, "args": proposal.args,
        "summary": proposal.summary, "details": [], "createdAt": proposal.created_ms, "expiresAt": proposal.expires_at
    })
}

#[test]
fn a_no_in_the_owners_session_declines_the_plan_and_the_admin_tokens_no_leaves_it_waiting() {
    let (rig, key) = Rig::new("oauth-decline");
    let (client, access, refresh) = rig.connected(&key, "Owner app", "viewer approvals");
    let session = rig.caller(&access);
    let stop = rig.propose(
        ActionKind::InstanceStop,
        json!({ "project": "p1", "instance": "api" }),
    );
    let remove = rig.propose(ActionKind::ServiceRemove, json!({ "name": "demo" }));
    let terminal = rig.terminal();
    let admin = Caller {
        token_id: "admin1".to_string(),
        scope: Scope::Admin,
        tools: None,
    };
    assert!(
        !terminal.decline(&stop, created(&rig, &stop), &admin),
        "the admin token's no leaves it waiting"
    );
    assert!(
        !terminal.decline(&stop, created(&rig, &stop) + 1, &session),
        "another record is not declined"
    );
    assert!(
        !terminal.decline(&remove, created(&rig, &remove), &session),
        "a plan that erases keeps its card"
    );
    assert!(terminal.decline(&stop, created(&rig, &stop), &session));
    let waiting: Vec<String> = rig
        .kernel
        .lock()
        .expect("kernel")
        .action_proposals()
        .iter()
        .map(|proposal| proposal.id.clone())
        .collect();
    assert_eq!(waiting, vec![remove]);
    let journal = fs::read_to_string(
        rig.state_dir
            .parent()
            .expect("the scratch directory, where the kernel journals")
            .join(arca::action_proposal::ACTION_PROPOSALS_FILE),
    )
    .expect("journal");
    let line = journal.lines().last().expect("a line");
    assert!(line.contains("\"declined\""), "{line}");
    assert!(
        line.contains(&format!(
            "declined in the owner's signed-in session {} (passkey ",
            session.token_id
        )),
        "{line}"
    );
    // Once its passkey no longer counts, the session's no declines nothing either.
    let again = rig.propose(
        ActionKind::InstanceStop,
        json!({ "project": "p1", "instance": "web" }),
    );
    rig.advance(STEP_UP_MS);
    let renewed = rig.refresh(&client, &refresh, None);
    assert_eq!(renewed.status, 200, "{}", renewed.text());
    let session = rig.caller(renewed.json()["access_token"].as_str().expect("access"));
    assert!(matches!(
        rig.oauth.session(&session),
        Session::StepUp { .. }
    ));
    assert!(!terminal.decline(&again, created(&rig, &again), &session));
}

#[test]
fn the_session_is_never_asked_about_an_erasing_plan_that_action_propose_answers_with() {
    let (rig, key) = Rig::new("oauth-pending-erase");
    let (_, access, _) = rig.connected(&key, "Owner app", "viewer approvals");
    let session = rig.caller(&access);
    let stop = rig.propose(
        ActionKind::InstanceStop,
        json!({ "project": "p1", "instance": "api" }),
    );
    let remove = rig.propose(ActionKind::ServiceRemove, json!({ "name": "demo" }));
    let terminal = rig.terminal();
    assert!(
        terminal
            .pending("action_propose", &proposed(&rig, &remove), &session)
            .is_none()
    );
    let asked = terminal
        .pending("action_propose", &proposed(&rig, &stop), &session)
        .expect("a plan that does not erase is asked");
    assert_eq!(asked.id, stop);
    assert!(
        asked.message.contains("No declines it"),
        "{}",
        asked.message
    );
}

// -------------------------------------------------------------------------------------------------------------------
// The device authorization grant (RFC 8628): an agent away from the person's browser (in a container or on a server)
// shows a short link and a code; the person approves on any device with the passkey; the agent polls for its token.

const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
/// The letters a user code is made of (RFC 8628 6.1: no vowels, nothing that reads as another letter or a digit).
const USER_CODE_LETTERS: &str = "BCDFGHJKLMNPQRSTVWXZ";

impl Rig {
    /// A client that registers for the device grant alone, with no redirect.
    fn device_client(&self, name: &str) -> String {
        let reply = self.send(
            "POST",
            "/oauth/register",
            &[],
            "application/json",
            json!({ "client_name": name, "grant_types": [DEVICE_GRANT, "refresh_token"], "token_endpoint_auth_method": "none" })
                .to_string()
                .as_bytes(),
        );
        assert_eq!(reply.status, 201, "register: {}", reply.text());
        let body = reply.json();
        assert_eq!(body["grant_types"], json!([DEVICE_GRANT, "refresh_token"]));
        body["client_id"].as_str().expect("a client id").to_string()
    }

    /// `POST /oauth/device_authorization` with `pairs`.
    fn device_authorization(&self, pairs: &[(&str, &str)]) -> Reply {
        self.send(
            "POST",
            "/oauth/device_authorization",
            &[],
            "application/x-www-form-urlencoded",
            form(pairs).as_bytes(),
        )
    }

    /// A device authorization for `client` at `scope` that must be taken: its answer.
    fn device_code(&self, client: &str, scope: &str) -> Value {
        let reply = self.device_authorization(&[
            ("client_id", client),
            ("scope", scope),
            ("resource", &format!("{ORIGIN}/mcp")),
        ]);
        assert_eq!(reply.status, 200, "device authorization: {}", reply.text());
        assert_eq!(reply.header("cache-control").as_deref(), Some("no-store"));
        reply.json()
    }

    /// The agent's poll of the token endpoint with `device_code`.
    fn poll(&self, client: &str, device_code: &str) -> Reply {
        self.token(&[
            ("grant_type", DEVICE_GRANT),
            ("device_code", device_code),
            ("client_id", client),
        ])
    }
}

/// The path and query of `url` on this origin.
fn on_origin(url: &str) -> String {
    url.strip_prefix(ORIGIN)
        .expect("a URL on the page origin")
        .to_string()
}

#[test]
fn an_agent_away_from_the_browser_shows_a_link_and_a_code_and_polls_for_its_token_after_the_passkey()
 {
    let (rig, key) = Rig::new("oauth-device");
    let client = rig.device_client("Codex in a container");
    let device = rig.device_code(&client, "doctor");
    let user_code = device["user_code"]
        .as_str()
        .expect("a user code")
        .to_string();
    let (left, right) = user_code.split_once('-').expect("two groups of four");
    assert!(
        left.len() == 4
            && right.len() == 4
            && left
                .chars()
                .chain(right.chars())
                .all(|c| USER_CODE_LETTERS.contains(c)),
        "{user_code}"
    );
    assert_eq!(
        device["verification_uri"],
        json!(format!("{ORIGIN}/oauth/device"))
    );
    assert_eq!(
        device["verification_uri_complete"],
        json!(format!("{ORIGIN}/oauth/device?user_code={user_code}"))
    );
    assert_eq!(device["expires_in"], json!(600));
    assert_eq!(device["interval"], json!(5));
    let device_code = device["device_code"]
        .as_str()
        .expect("a device code")
        .to_string();
    assert!(
        !rig.journal().contains(&device_code),
        "no device code in the journal"
    );

    // Nobody approved yet: the agent is told to keep waiting.
    let waiting = rig.poll(&client, &device_code);
    assert_eq!(waiting.status, 400, "{}", waiting.text());
    assert_eq!(error_of(&waiting), "authorization_pending");

    // The link alone opens a page where the person types the code.
    let entry = rig.get("/oauth/device");
    assert_eq!(entry.status, 200, "{}", entry.text());
    assert!(
        entry.text().contains("name=\"user_code\""),
        "{}",
        entry.text()
    );
    // The full link, or the code typed in lower case without its dash, opens the consent page for it.
    let typed = rig.get(&format!(
        "/oauth/device?user_code={}",
        user_code.replace('-', "").to_lowercase()
    ));
    assert_eq!(
        Rig::request_id(&typed),
        Rig::request_id(
            &rig.get(&on_origin(
                device["verification_uri_complete"]
                    .as_str()
                    .expect("a link")
            ))
        )
    );
    let page = rig.get(&on_origin(
        device["verification_uri_complete"]
            .as_str()
            .expect("a link"),
    ));
    let text = page.text();
    assert!(text.contains("Codex in a container"), "the name is shown");
    assert!(
        text.contains(&user_code),
        "the code is shown, to match with the agent's"
    );
    assert!(text.contains("<strong id=\"scope\">doctor</strong>"));
    assert!(text.contains("another device"), "{text}");
    assert!(
        text.contains("id=\"device-note\""),
        "the warning not to approve a code someone sent: {text}"
    );
    assert!(
        !text.contains("redirect-host"),
        "a device request has no redirect"
    );

    // The passkey approves; the page answers with words, not a redirect.
    rig.advance(5_000);
    let approved = rig.approve_with(&key, &Rig::request_id(&page));
    assert_eq!(approved.status, 200, "{}", approved.text());
    assert!(
        approved.json().get("redirect").is_none(),
        "{}",
        approved.text()
    );
    assert!(
        approved.json()["message"]
            .as_str()
            .is_some_and(|m| m.contains("app"))
    );

    // The next poll gets the tokens, at the scope approved, and they call MCP with it.
    let reply = rig.poll(&client, &device_code);
    assert_eq!(reply.status, 200, "{}", reply.text());
    assert_eq!(reply.header("cache-control").as_deref(), Some("no-store"));
    let body = reply.json();
    assert_eq!(body["token_type"], json!("Bearer"));
    assert_eq!(body["scope"], json!("doctor"));
    let access = body["access_token"].as_str().expect("access").to_string();
    let refresh = body["refresh_token"].as_str().expect("refresh").to_string();
    assert!(
        rig.tools(&access).contains(&"service_remove".to_string()),
        "a doctor verb"
    );
    // The device code works once.
    rig.advance(5_000);
    let again = rig.poll(&client, &device_code);
    assert_eq!(error_of(&again), "invalid_grant", "{}", again.text());
    // It refreshes like any grant.
    let renewed = rig.refresh(&client, &refresh, None);
    assert_eq!(renewed.status, 200, "{}", renewed.text());
    // The journal says it was granted to a device, by the passkey, with no secret.
    let journal = rig.journal();
    assert!(journal.contains("\"event\":\"granted\""), "{journal}");
    assert!(journal.contains("\"device\":true"), "{journal}");
    for secret in [&access, &refresh, &device_code] {
        assert!(!journal.contains(secret.as_str()));
    }
}

#[test]
fn a_device_viewer_is_stepped_up_by_asking_again_with_the_scope_the_door_names() {
    let (rig, key) = Rig::new("oauth-device-step-up");
    let client = rig.device_client("Claude on a server");
    let connect = |scope: &str| {
        let device = rig.device_code(&client, scope);
        let page = rig.get(&on_origin(
            device["verification_uri_complete"]
                .as_str()
                .expect("a link"),
        ));
        let approved = rig.approve_with(&key, &Rig::request_id(&page));
        assert_eq!(approved.status, 200, "{}", approved.text());
        let reply = rig.poll(
            &client,
            device["device_code"].as_str().expect("a device code"),
        );
        assert_eq!(reply.status, 200, "{}", reply.text());
        reply.json()["access_token"]
            .as_str()
            .expect("access")
            .to_string()
    };
    let viewer = connect("viewer");
    let call = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "service_remove", "arguments": { "name": "nats" } } });
    let refused = rig.mcp(Some(&viewer), call.clone());
    assert_eq!(refused.status, 403);
    assert!(
        refused
            .header("www-authenticate")
            .is_some_and(|c| c.starts_with("Bearer error=\"insufficient_scope\", scope=\"doctor\"")),
        "{:?}",
        refused.header("www-authenticate")
    );
    let doctor = connect("doctor");
    assert_ne!(rig.mcp(Some(&doctor), call).status, 403);
}

#[test]
fn a_device_code_polled_too_fast_slows_down_and_a_no_or_the_clock_ends_it() {
    let (rig, key) = Rig::new("oauth-device-poll");
    let _ = key;
    let client = rig.device_client("Codex");
    let device = rig.device_code(&client, "viewer");
    let code = device["device_code"]
        .as_str()
        .expect("a device code")
        .to_string();
    assert_eq!(error_of(&rig.poll(&client, &code)), "authorization_pending");
    // Again at once, under the interval: slow down, and the interval grows by five seconds (RFC 8628 3.5).
    assert_eq!(error_of(&rig.poll(&client, &code)), "slow_down");
    rig.advance(5_000);
    assert_eq!(
        error_of(&rig.poll(&client, &code)),
        "slow_down",
        "the interval is ten seconds now"
    );
    rig.advance(10_000);
    assert_eq!(error_of(&rig.poll(&client, &code)), "authorization_pending");
    // The person says no on the page: the agent is told access_denied, once.
    let page = rig.get(&on_origin(
        device["verification_uri_complete"]
            .as_str()
            .expect("a link"),
    ));
    let denied = rig.page_post(
        "/oauth/authorize/deny",
        json!({ "id": Rig::request_id(&page) }),
    );
    assert_eq!(denied.status, 200, "{}", denied.text());
    assert!(denied.json().get("redirect").is_none(), "{}", denied.text());
    // Decided, its link opens no consent page any more.
    let decided = rig.get(&on_origin(
        device["verification_uri_complete"]
            .as_str()
            .expect("a link"),
    ));
    assert_eq!(decided.status, 404, "{}", decided.text());
    rig.advance(10_000);
    assert_eq!(error_of(&rig.poll(&client, &code)), "access_denied");
    rig.advance(10_000);
    assert_eq!(error_of(&rig.poll(&client, &code)), "invalid_grant");
    assert!(rig.journal().contains("\"event\":\"denied\""));
    // Ten minutes on, a device code nobody approved has expired, and its link opens no consent page.
    let late = rig.device_code(&client, "viewer");
    rig.advance(600_000);
    let expired = rig.poll(
        &client,
        late["device_code"].as_str().expect("a device code"),
    );
    assert_eq!(error_of(&expired), "expired_token", "{}", expired.text());
    let gone = rig.get(&on_origin(
        late["verification_uri_complete"].as_str().expect("a link"),
    ));
    assert_eq!(gone.status, 404, "{}", gone.text());
    assert!(!gone.text().contains("data-request"));
}

#[test]
fn a_device_request_is_checked_like_a_consent_request_and_its_code_is_bound_to_its_client() {
    let (rig, key) = Rig::new("oauth-device-checks");
    let client = rig.device_client("Codex");
    let other = rig.device_client("Another agent");
    let resource = format!("{ORIGIN}/mcp");
    let refused = |pairs: &[(&str, &str)]| error_of(&rig.device_authorization(pairs));
    assert_eq!(
        refused(&[("client_id", "mcpc-unknown"), ("scope", "viewer")]),
        "invalid_client"
    );
    assert_eq!(
        refused(&[("client_id", &client), ("scope", "admin")]),
        "invalid_scope"
    );
    assert_eq!(
        refused(&[
            ("client_id", &client),
            ("scope", "viewer"),
            ("resource", "https://elsewhere.example.com/mcp")
        ]),
        "invalid_target"
    );
    // No scope means viewer, as on the consent page; no resource means this node's.
    let plain = rig.device_authorization(&[("client_id", &client)]);
    assert_eq!(plain.status, 200, "{}", plain.text());
    // A device code polled by another client is refused, and stays its client's.
    let device = rig.device_code(&client, "viewer");
    let code = device["device_code"]
        .as_str()
        .expect("a device code")
        .to_string();
    let page = rig.get(&on_origin(
        device["verification_uri_complete"]
            .as_str()
            .expect("a link"),
    ));
    let approved = rig.approve_with(&key, &Rig::request_id(&page));
    assert_eq!(approved.status, 200, "{}", approved.text());
    assert_eq!(error_of(&rig.poll(&other, &code)), "invalid_grant");
    assert_eq!(rig.poll(&client, &code).status, 200);
    // A code nobody issued opens no consent page.
    let unknown = rig.get("/oauth/device?user_code=BCDF-GHJK");
    assert_eq!(unknown.status, 404, "{}", unknown.text());
    // A client with neither a redirect nor the device grant is still refused at registration.
    let bare = rig.send(
        "POST",
        "/oauth/register",
        &[],
        "application/json",
        json!({ "client_name": "Nothing", "token_endpoint_auth_method": "none" })
            .to_string()
            .as_bytes(),
    );
    assert_eq!(bare.status, 400, "{}", bare.text());
    // A device-only client has no redirect, so the code flow never sends anything anywhere for it.
    let code_flow = rig.authorize(&client, &[("redirect_uri", None)]);
    assert!(
        !code_flow.text().contains("data-request"),
        "{}",
        code_flow.text()
    );
    let _ = resource;
}

#[test]
fn the_metadata_names_the_device_endpoint_and_grant() {
    let (rig, _key) = Rig::new("oauth-device-metadata");
    let metadata = rig.get("/.well-known/oauth-authorization-server").json();
    assert_eq!(
        metadata["device_authorization_endpoint"],
        json!(format!("{ORIGIN}/oauth/device_authorization"))
    );
    let grants = metadata["grant_types_supported"]
        .as_array()
        .expect("grant types");
    assert!(grants.contains(&json!(DEVICE_GRANT)), "{grants:?}");
    assert!(
        grants.contains(&json!("authorization_code")),
        "the code flow stays"
    );
}

#[test]
fn device_codes_are_bounded_even_when_each_request_is_denied_at_once() {
    let (rig, _key) = Rig::new("oauth-device-bound");
    let client = rig.device_client("Churn");
    for _ in 0..50 {
        rig.device_code(&client, "viewer");
    }
    let refused = rig.device_authorization(&[("client_id", &client)]);
    assert_eq!(refused.status, 429, "{}", refused.text());
    assert_eq!(error_of(&refused), "temporarily_unavailable");
    // The requests lapse; a loop that denies each new one at once frees its request, and still meets a bound on the
    // device codes it leaves behind.
    rig.advance(600_001);
    let mut taken = 0;
    let mut bounded = None;
    for _ in 0..200 {
        let reply = rig.device_authorization(&[("client_id", &client)]);
        if reply.status == 429 {
            bounded = Some(error_of(&reply));
            break;
        }
        assert_eq!(reply.status, 200, "{}", reply.text());
        taken += 1;
        let page = rig.get(&on_origin(
            reply.json()["verification_uri_complete"]
                .as_str()
                .expect("a link"),
        ));
        let denied = rig.page_post(
            "/oauth/authorize/deny",
            json!({ "id": Rig::request_id(&page) }),
        );
        assert_eq!(denied.status, 200, "{}", denied.text());
    }
    assert_eq!(
        bounded.as_deref(),
        Some("temporarily_unavailable"),
        "refused after {taken}"
    );
    assert!(taken <= 50, "{taken} device codes past the first fifty");
    // Once the held ones are swept, device codes are given again.
    rig.advance(20 * 60 * 1000);
    assert_eq!(
        rig.device_authorization(&[("client_id", &client)]).status,
        200
    );
}

#[test]
fn a_client_registered_for_its_redirect_alone_cannot_start_a_device_flow() {
    let (rig, _key) = Rig::new("oauth-device-code-only");
    let desktop = rig.client("Claude Desktop");
    let refused = rig.device_authorization(&[("client_id", &desktop), ("scope", "operator")]);
    assert_eq!(refused.status, 400, "{}", refused.text());
    assert_eq!(error_of(&refused), "unauthorized_client");
    // One that registered both may use both.
    let both = rig.send(
        "POST",
        "/oauth/register",
        &[],
        "application/json",
        json!({ "client_name": "Both", "redirect_uris": [REDIRECT], "grant_types": ["authorization_code", DEVICE_GRANT, "refresh_token"], "token_endpoint_auth_method": "none" })
            .to_string()
            .as_bytes(),
    );
    assert_eq!(both.status, 201, "{}", both.text());
    let body = both.json();
    assert_eq!(
        body["grant_types"],
        json!(["authorization_code", DEVICE_GRANT, "refresh_token"])
    );
    let id = body["client_id"].as_str().expect("a client id").to_string();
    assert_eq!(rig.device_authorization(&[("client_id", &id)]).status, 200);
    assert!(rig.authorize(&id, &[]).text().contains("data-request"));
    // The grant types survive a restart of the store.
    let reopened = rig.reopened();
    drop(reopened);
    let store =
        fs::read_to_string(rig.state_dir.join(arca::mcp_oauth::STORE_FILE)).expect("the store");
    assert!(store.contains(DEVICE_GRANT), "{store}");
}

#[test]
fn the_passkey_statement_of_a_device_request_binds_its_user_code() {
    let (rig, _key) = Rig::new("oauth-device-binds");
    let client = rig.device_client("Bound");
    let device = rig.device_code(&client, "doctor");
    let page = rig.get(&on_origin(
        device["verification_uri_complete"]
            .as_str()
            .expect("a link"),
    ));
    let id = Rig::request_id(&page);
    let begun = rig.page_post("/oauth/authorize/begin", json!({ "id": id }));
    assert_eq!(begun.status, 200, "{}", begun.text());
    let statement: Value =
        serde_json::from_str(begun.json()["statement"].as_str().expect("a statement"))
            .expect("json");
    let bound = json!({
        "request": id, "client": client, "name": "Bound", "redirectUri": "", "scope": "doctor",
        "resource": format!("{ORIGIN}/mcp"), "codeChallenge": "", "userCode": device["user_code"],
    });
    assert_eq!(statement["action"], json!("mcp_authorize"));
    assert_eq!(statement["target"], json!(client));
    assert_eq!(
        statement["paramsDigest"],
        json!(arca_kernel::canon::params_digest(&bound).expect("digest"))
    );
}

#[test]
fn a_revoked_clients_waiting_device_code_ends_at_once() {
    let (rig, _key) = Rig::new("oauth-device-revoked");
    let client = rig.device_client("Gone");
    let device = rig.device_code(&client, "viewer");
    let code = device["device_code"]
        .as_str()
        .expect("a device code")
        .to_string();
    assert_eq!(error_of(&rig.poll(&client, &code)), "authorization_pending");
    rig.oauth.revoke_client(&client, "test").expect("revoked");
    rig.advance(5_000);
    assert_eq!(error_of(&rig.poll(&client, &code)), "invalid_grant");
}

#[test]
fn a_device_grant_with_approvals_is_the_owners_session_as_the_code_flows_is() {
    let (rig, key) = Rig::new("oauth-device-approvals");
    let client = rig.device_client("Codex on a server");
    let device = rig.device_code(&client, "viewer approvals");
    let page = rig.get(&on_origin(
        device["verification_uri_complete"]
            .as_str()
            .expect("a link"),
    ));
    assert!(page.text().contains("id=\"approvals\""), "{}", page.text());
    let approved = rig.approve_with(&key, &Rig::request_id(&page));
    assert_eq!(approved.status, 200, "{}", approved.text());
    // The agent polls a little later: the passkey still counts from the moment it approved.
    rig.advance(10_000);
    let reply = rig.poll(
        &client,
        device["device_code"].as_str().expect("a device code"),
    );
    assert_eq!(reply.status, 200, "{}", reply.text());
    assert_eq!(reply.json()["scope"], json!("viewer approvals"));
    let session = rig.caller(reply.json()["access_token"].as_str().expect("access"));
    assert!(matches!(rig.oauth.session(&session), Session::Ready { .. }));
    assert!(
        rig.kernel
            .lock()
            .expect("kernel")
            .owner_session_live(rig.now())
    );
    // Twelve hours after the passkey (not after the poll), it is asked to step up, as the code flow's is.
    rig.advance(STEP_UP_MS - 10_000);
    assert!(matches!(
        rig.oauth.session(&session),
        Session::StepUp { .. }
    ));
}
