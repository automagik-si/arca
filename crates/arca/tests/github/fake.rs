//! A fake GitHub for the signer's tests: the OAuth device flow on the web host, the REST calls the signer makes, and the
//! Actions service's registration calls, on one loopback HTTP server. It checks every credential it was given, records
//! every request (method, path and which credential came), and keeps what was registered.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Bytes, to_bytes};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

pub const CLIENT_ID: &str = "Ov23liFakeClientId";
pub const CLIENT_SECRET: &str = "fake-client-secret-0123456789";
pub const DEVICE_CODE: &str = "fake-device-code-abcdef";
pub const USER_CODE: &str = "WDJB-MJHT";
pub const GRANT: &str = "fake-grant-0123456789";
pub const REGISTRATION: &str = "fake-registration-token-AAAA";
pub const ADMIN: &str = "fake-admin-token-BBBB";
/// The App the manifest flow makes.
pub const APP_ID: u64 = 4242;
pub const APP_CLIENT_ID: &str = "Iv23liFakeAppClient";
pub const APP_SLUG: &str = "arca-runners-test";
pub const MANIFEST_CODE: &str = "fake-manifest-code_01";
pub const OWNER_ID: u64 = 9001;
pub const INSTALLATION: u64 = 555;
pub const INSTALL_TOKEN: &str = "fake-install-token-0123";
/// Every just-in-time configuration the fake mints starts with this.
pub const JIT: &str = "fake-jit-config-";
/// Every credential the fake hands out or takes: none may appear in anything the signer shows or logs.
pub const SECRETS: [&str; 8] = [
    CLIENT_SECRET,
    DEVICE_CODE,
    GRANT,
    REGISTRATION,
    ADMIN,
    "BEGIN RSA",
    INSTALL_TOKEN,
    JIT,
];

#[derive(Debug, Clone)]
pub struct Seen {
    pub method: String,
    pub path: String,
    /// `none`, `bearer:<value>`, `remote:<value>` or `basic:<user>:<password>`.
    pub auth: String,
    pub body: Value,
}

pub struct FakeState {
    pub org: String,
    /// The person has typed the code and authorised.
    pub authorized: bool,
    /// The person declined.
    pub declined: bool,
    /// The organisation restricts OAuth Apps.
    pub restricted: bool,
    /// What the runner-registration call answers for `use_v2_flow`.
    pub admin_flow: bool,
    pub grant_deleted: bool,
    pub groups: Vec<Value>,
    pub next_group: u64,
    /// The registered runners: `{id, name, group, labels, publicKey}`.
    pub runners: Vec<Value>,
    pub next_runner: u64,
    /// Refuse runner names that start with this.
    pub refuse_name: Option<String>,
    pub seen: Vec<Seen>,
    pub base: String,
    /// The App's key, made when GitHub "creates" the App; its public half checks every App JWT.
    pub app_key: rsa::RsaPrivateKey,
    pub app_pem: String,
    /// The manifest code was exchanged (GitHub answers a code once).
    pub app_made: bool,
    /// The App is installed on the organisation.
    pub installed: bool,
    pub suspended: bool,
    /// The App is deleted on GitHub.
    pub app_deleted: bool,
    /// The installation's repositories: `(id, full_name)`.
    pub repositories: Vec<(u64, String)>,
    /// What creating a runner group answers instead of making it, if anything.
    pub group_create_refusal: Option<u16>,
    /// The runner groups' repositories, by group id.
    pub group_repositories: Vec<(u64, Value)>,
    /// The organisation's scale sets: `(id, runner group id)`.
    pub scale_sets: Vec<(u64, u64)>,
    /// What the token poll answers before its usual answer, one entry a poll: `503` (an empty 503), `html` (a page
    /// that is not JSON) or `slow_down` (GitHub's slow_down with a one-second interval).
    pub poll_script: Vec<&'static str>,
}

pub struct Fake {
    pub base: String,
    pub state: Arc<Mutex<FakeState>>,
}

impl Fake {
    pub async fn start(org: &str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port");
        let base = format!("http://{}", listener.local_addr().expect("an address"));
        let app_key = rsa::RsaPrivateKey::new(&mut TestRandom, 1024).expect("an App key");
        let app_pem = {
            use rsa::pkcs1::EncodeRsaPrivateKey as _;
            let der = app_key.to_pkcs1_der().expect("the key's DER");
            let body = STANDARD.encode(der.as_bytes());
            let lines: Vec<&str> = body
                .as_bytes()
                .chunks(64)
                .map(|chunk| std::str::from_utf8(chunk).expect("base64 is ASCII"))
                .collect();
            format!(
                "-----BEGIN RSA PRIVATE KEY-----\n{}\n-----END RSA PRIVATE KEY-----\n",
                lines.join("\n")
            )
        };
        let state = Arc::new(Mutex::new(FakeState {
            org: org.to_string(),
            authorized: false,
            declined: false,
            restricted: false,
            admin_flow: true,
            grant_deleted: false,
            groups: vec![
                json!({ "id": 1, "name": "Default", "default": true, "visibility": "all" }),
                json!({ "id": 7, "name": "ci-pool", "default": false, "visibility": "selected" }),
            ],
            next_group: 20,
            runners: Vec::new(),
            next_runner: 100,
            refuse_name: None,
            seen: Vec::new(),
            base: base.clone(),
            app_key: app_key.clone(),
            app_pem,
            app_made: false,
            installed: false,
            suspended: false,
            app_deleted: false,
            repositories: vec![(11, format!("{org}/web")), (12, format!("{org}/api"))],
            group_create_refusal: None,
            group_repositories: Vec::new(),
            scale_sets: Vec::new(),
            poll_script: Vec::new(),
        }));
        let router = Router::new().fallback(serve).with_state(Arc::clone(&state));
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Self { base, state }
    }

    pub fn endpoints(&self) -> arca::github_api::Endpoints {
        arca::github_api::Endpoints {
            api: format!("{}/api", self.base),
            web: format!("{}/web", self.base),
        }
    }

    pub fn with<T>(&self, f: impl FnOnce(&mut FakeState) -> T) -> T {
        f(&mut self.state.lock().expect("the fake's lock"))
    }

    pub fn seen(&self, method: &str, path: &str) -> Vec<Seen> {
        self.with(|s| {
            s.seen
                .iter()
                .filter(|seen| seen.method == method && seen.path == path)
                .cloned()
                .collect()
        })
    }
}

/// Random numbers for the fake's App key.
struct TestRandom;

impl rsa::rand_core::RngCore for TestRandom {
    fn next_u32(&mut self) -> u32 {
        let mut bytes = [0u8; 4];
        self.fill_bytes(&mut bytes);
        u32::from_le_bytes(bytes)
    }
    fn next_u64(&mut self) -> u64 {
        let mut bytes = [0u8; 8];
        self.fill_bytes(&mut bytes);
        u64::from_le_bytes(bytes)
    }
    fn fill_bytes(&mut self, dest: &mut [u8]) {
        getrandom::fill(dest).expect("random bytes");
    }
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rsa::rand_core::Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}

impl rsa::rand_core::CryptoRng for TestRandom {}

/// Whether `auth` carries a JWT for the App, signed by its key, issued to its client ID and current.
fn app_jwt_ok(s: &FakeState, auth: &str) -> bool {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use sha2::Digest as _;
    let Some(jwt) = auth.strip_prefix("bearer:") else {
        return false;
    };
    let parts: Vec<&str> = jwt.split('.').collect();
    let [header, payload, signature] = parts[..] else {
        return false;
    };
    let (Ok(header_json), Ok(claims), Ok(signature)) = (
        URL_SAFE_NO_PAD.decode(header),
        URL_SAFE_NO_PAD.decode(payload),
        URL_SAFE_NO_PAD.decode(signature),
    ) else {
        return false;
    };
    let header_json: Value = serde_json::from_slice(&header_json).unwrap_or(Value::Null);
    let claims: Value = serde_json::from_slice(&claims).unwrap_or(Value::Null);
    if header_json["alg"] != "RS256" || claims["iss"] != APP_CLIENT_ID {
        return false;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after the epoch")
        .as_secs();
    let (Some(iat), Some(exp)) = (claims["iat"].as_u64(), claims["exp"].as_u64()) else {
        return false;
    };
    if iat > now || exp <= now || exp - iat > 600 {
        return false;
    }
    // SHA-256's DigestInfo prefix, then the digest: what PKCS#1 v1.5 signs.
    let mut hashed = vec![
        0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01,
        0x05, 0x00, 0x04, 0x20,
    ];
    hashed.extend_from_slice(&sha2::Sha256::digest(
        format!("{header}.{payload}").as_bytes(),
    ));
    rsa::RsaPublicKey::from(&s.app_key)
        .verify(rsa::Pkcs1v15Sign::new_unprefixed(), &hashed, &signature)
        .is_ok()
}

fn auth_of(request: &Request) -> String {
    let Some(value) = request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
    else {
        return "none".to_string();
    };
    if let Some(token) = value.strip_prefix("Bearer ") {
        return format!("bearer:{token}");
    }
    if let Some(token) = value.strip_prefix("RemoteAuth ") {
        return format!("remote:{token}");
    }
    if let Some(basic) = value.strip_prefix("Basic ") {
        let decoded = STANDARD.decode(basic).unwrap_or_default();
        return format!("basic:{}", String::from_utf8_lossy(&decoded));
    }
    format!("other:{value}")
}

async fn serve(State(state): State<Arc<Mutex<FakeState>>>, request: Request) -> Response {
    let method = request.method().to_string();
    let path = request.uri().path().to_string();
    let query = request.uri().query().unwrap_or("").to_string();
    let auth = auth_of(&request);
    let accept = request
        .headers()
        .get("accept")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body: Bytes = to_bytes(request.into_body(), 1 << 20)
        .await
        .unwrap_or_default();
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let mut state = state.lock().expect("the fake's lock");
    state.seen.push(Seen {
        method: method.clone(),
        path: path.clone(),
        auth: auth.clone(),
        body: body.clone(),
    });
    let (status, answer) = answer(&mut state, &method, &path, &query, &auth, &accept, &body);
    let status = StatusCode::from_u16(status).expect("a status");
    if answer.is_null() {
        status.into_response()
    } else if let Some(text) = answer.as_str() {
        (status, text.to_string()).into_response()
    } else {
        (status, axum::Json(answer)).into_response()
    }
}

fn message(text: &str) -> Value {
    json!({ "message": text })
}

fn answer(
    s: &mut FakeState,
    method: &str,
    path: &str,
    query: &str,
    auth: &str,
    accept: &str,
    body: &Value,
) -> (u16, Value) {
    let org = s.org.clone();
    let grant_ok = (auth == format!("bearer:{GRANT}") && !s.grant_deleted && s.authorized)
        || (auth == format!("bearer:{INSTALL_TOKEN}")
            && s.installed
            && !s.suspended
            && !s.app_deleted);
    let installation_path = format!("/api/app/installations/{INSTALLATION}");
    match (method, path) {
        ("POST", p) if p == format!("/api/app-manifests/{MANIFEST_CODE}/conversions") => {
            if auth != "none" || s.app_made {
                return (404, message("Not Found"));
            }
            s.app_made = true;
            (
                201,
                json!({
                    "id": APP_ID,
                    "slug": APP_SLUG,
                    "client_id": APP_CLIENT_ID,
                    "client_secret": "fake-app-client-secret",
                    "webhook_secret": null,
                    "pem": s.app_pem,
                    "html_url": format!("{}/web/apps/{APP_SLUG}", s.base),
                    "owner": { "login": org, "id": OWNER_ID, "type": "Organization" },
                }),
            )
        }
        ("GET", "/api/app") => {
            if s.app_deleted || !app_jwt_ok(s, auth) {
                return (401, message("A JSON web token could not be decoded"));
            }
            (
                200,
                json!({ "id": APP_ID, "slug": APP_SLUG, "client_id": APP_CLIENT_ID }),
            )
        }
        ("GET", p) if p == installation_path => {
            if s.app_deleted || !app_jwt_ok(s, auth) {
                return (401, message("A JSON web token could not be decoded"));
            }
            if !s.installed {
                return (404, message("Not Found"));
            }
            (
                200,
                json!({
                    "id": INSTALLATION,
                    "account": { "login": org, "id": OWNER_ID },
                    "repository_selection": "selected",
                    "permissions": { "organization_self_hosted_runners": "write" },
                }),
            )
        }
        ("DELETE", p) if p == installation_path => {
            if s.app_deleted || !app_jwt_ok(s, auth) {
                return (401, message("A JSON web token could not be decoded"));
            }
            if !s.installed {
                return (404, message("Not Found"));
            }
            s.installed = false;
            (204, Value::Null)
        }
        ("POST", p) if p == format!("{installation_path}/access_tokens") => {
            if s.app_deleted || !app_jwt_ok(s, auth) {
                return (401, message("A JSON web token could not be decoded"));
            }
            if !s.installed {
                return (404, message("Not Found"));
            }
            if s.suspended {
                return (403, message("This installation has been suspended"));
            }
            (
                201,
                json!({ "token": INSTALL_TOKEN, "expires_at": "2026-11-14T20:00:00Z" }),
            )
        }
        ("GET", "/api/installation/repositories") => {
            if !grant_ok {
                return (401, message("Bad credentials"));
            }
            let repositories: Vec<Value> = s
                .repositories
                .iter()
                .map(|(id, name)| json!({ "id": id, "full_name": name }))
                .collect();
            (
                200,
                json!({ "total_count": repositories.len(), "repositories": repositories }),
            )
        }
        ("PUT", p)
            if p.starts_with(&format!("/api/orgs/{org}/actions/runner-groups/"))
                && p.ends_with("/repositories") =>
        {
            if !grant_ok {
                return (401, message("Bad credentials"));
            }
            let id: u64 = p
                .trim_start_matches(&format!("/api/orgs/{org}/actions/runner-groups/"))
                .trim_end_matches("/repositories")
                .parse()
                .unwrap_or(0);
            s.group_repositories.retain(|(group, _)| *group != id);
            s.group_repositories
                .push((id, body["selected_repository_ids"].clone()));
            (204, Value::Null)
        }
        ("PATCH", p) if p.starts_with(&format!("/api/orgs/{org}/actions/runner-groups/")) => {
            if !grant_ok {
                return (401, message("Bad credentials"));
            }
            (200, json!({ "ok": true }))
        }
        ("POST", p) if p == format!("/api/orgs/{org}/actions/runners/generate-jitconfig") => {
            if !grant_ok || !auth.contains(INSTALL_TOKEN) {
                return (401, message("Bad credentials"));
            }
            let id = s.next_runner;
            s.next_runner += 1;
            s.runners.push(json!({
                "id": id,
                "name": body["name"],
                "group": body["runner_group_id"],
                "labels": body["labels"],
                "workFolder": body["work_folder"],
                "jit": true,
            }));
            (
                201,
                json!({
                    "runner": { "id": id, "name": body["name"], "labels": body["labels"] },
                    "encoded_jit_config": format!("{JIT}{id}"),
                }),
            )
        }
        ("GET", p) if p.starts_with("/tenant/_apis/runtime/runnerscalesets/") => {
            if auth != format!("bearer:{ADMIN}") || !query.contains("api-version=") {
                return (401, message("Bad credentials"));
            }
            let id: u64 = p
                .trim_start_matches("/tenant/_apis/runtime/runnerscalesets/")
                .parse()
                .unwrap_or(0);
            match s.scale_sets.iter().find(|(scale_set, _)| *scale_set == id) {
                Some((id, group)) => (
                    200,
                    json!({ "id": id, "name": format!("set-{id}"), "runnerGroupId": group, "runnerGroupName": "g" }),
                ),
                None => (404, message("Not Found")),
            }
        }
        ("POST", p)
            if p.starts_with("/tenant/_apis/runtime/runnerscalesets/")
                && p.ends_with("/generatejitconfig") =>
        {
            if auth != format!("bearer:{ADMIN}") || !query.contains("api-version=") {
                return (401, message("Bad credentials"));
            }
            let scale_set: u64 = p
                .trim_start_matches("/tenant/_apis/runtime/runnerscalesets/")
                .trim_end_matches("/generatejitconfig")
                .parse()
                .unwrap_or(0);
            let id = s.next_runner;
            s.next_runner += 1;
            s.runners.push(json!({
                "id": id,
                "name": body["name"],
                "scaleSet": scale_set,
                "workFolder": body["workFolder"],
                "jit": true,
            }));
            (
                200,
                json!({
                    "runner": { "id": id, "name": body["name"], "runnerScaleSetId": scale_set },
                    "encodedJITConfig": format!("{JIT}{id}"),
                }),
            )
        }
        ("POST", "/web/login/device/code") => {
            if accept != "application/json" {
                return (406, Value::Null);
            }
            if body["client_id"] != CLIENT_ID || body["scope"] != "admin:org" {
                return (200, json!({ "error": "incorrect_client_credentials" }));
            }
            (
                200,
                json!({
                    "device_code": DEVICE_CODE,
                    "user_code": USER_CODE,
                    "verification_uri": "https://github.com/login/device",
                    "expires_in": 900,
                    "interval": 0,
                }),
            )
        }
        ("POST", "/web/login/oauth/access_token") => {
            if body["client_id"] != CLIENT_ID
                || body["device_code"] != DEVICE_CODE
                || body["grant_type"] != "urn:ietf:params:oauth:grant-type:device_code"
            {
                return (200, json!({ "error": "incorrect_device_code" }));
            }
            if s.declined {
                return (200, json!({ "error": "access_denied" }));
            }
            if !s.poll_script.is_empty() {
                return match s.poll_script.remove(0) {
                    "503" => (503, Value::Null),
                    "html" => (200, Value::String("<html>Unicorn!</html>".to_string())),
                    _ => (200, json!({ "error": "slow_down", "interval": 1 })),
                };
            }
            if !s.authorized {
                return (200, json!({ "error": "authorization_pending" }));
            }
            // A new sign-in is a new grant (the fake reuses the one value).
            s.grant_deleted = false;
            (
                200,
                json!({ "access_token": GRANT, "token_type": "bearer", "scope": "admin:org" }),
            )
        }
        ("DELETE", p) if p == format!("/api/applications/{CLIENT_ID}/grant") => {
            // GitHub answers 404, not 401, to client credentials it does not take.
            if auth != format!("basic:{CLIENT_ID}:{CLIENT_SECRET}") {
                return (404, message("Not Found"));
            }
            if body["access_token"] != GRANT {
                return (422, message("Invalid access token"));
            }
            s.grant_deleted = true;
            (204, Value::Null)
        }
        ("POST", p) if p == format!("/api/orgs/{org}/actions/runners/registration-token") => {
            if !grant_ok {
                return (401, message("Bad credentials"));
            }
            if s.restricted {
                return (
                    403,
                    message(&format!(
                        "Although you appear to have the correct authorization credentials, the `{org}` organization has enabled OAuth App access restrictions, meaning that data access to third-parties is limited."
                    )),
                );
            }
            (
                201,
                json!({ "token": REGISTRATION, "expires_at": "2026-11-14T20:00:00Z" }),
            )
        }
        ("GET", p) if p == format!("/api/orgs/{org}/actions/runner-groups") => {
            if !grant_ok {
                return (401, message("Bad credentials"));
            }
            (
                200,
                json!({ "total_count": s.groups.len(), "runner_groups": s.groups }),
            )
        }
        ("POST", p) if p == format!("/api/orgs/{org}/actions/runner-groups") => {
            if !grant_ok {
                return (401, message("Bad credentials"));
            }
            if let Some(status) = s.group_create_refusal {
                return (
                    status,
                    message("Runner groups are not available on this plan"),
                );
            }
            if s.groups.iter().any(|group| group["name"] == body["name"]) {
                return (409, message("A runner group with this name already exists"));
            }
            let id = s.next_group;
            s.next_group += 1;
            let group = json!({
                "id": id,
                "name": body["name"],
                "default": false,
                "visibility": body["visibility"],
                "allows_public_repositories": body["allows_public_repositories"],
            });
            s.groups.push(group.clone());
            (201, group)
        }
        ("POST", "/api/actions/runner-registration") => {
            if auth != format!("remote:{REGISTRATION}") {
                return (404, message("Not Found"));
            }
            if body["url"] != format!("{}/web/{org}", s.base) || body["runner_event"] != "register"
            {
                return (400, message("bad url"));
            }
            (
                200,
                json!({
                    "url": format!("{}/tenant/", s.base),
                    "token": ADMIN,
                    "token_schema": "OAuthAccessToken",
                    "use_v2_flow": s.admin_flow,
                }),
            )
        }
        ("POST", "/api/actions/runners/register") => {
            if auth != format!("remote:{REGISTRATION}") {
                return (401, message("Bad credentials"));
            }
            let name = body["name"].as_str().unwrap_or_default().to_string();
            if s.refuse_name
                .as_deref()
                .is_some_and(|start| name.starts_with(start))
            {
                return (409, message("A runner exists with the same name"));
            }
            let key = body["public_key"].as_str().unwrap_or_default();
            if !key.starts_with("<RSAKeyValue><Modulus>") {
                return (400, message("bad key"));
            }
            let id = s.next_runner;
            s.next_runner += 1;
            s.runners.push(json!({
                "id": id,
                "name": name,
                "group": body["group_id"],
                "labels": body["labels"],
                "publicKey": key,
                "version": body["version"],
                "ephemeral": body["ephemeral"],
                "url": body["url"],
            }));
            (
                201,
                json!({
                    "id": id,
                    "name": name,
                    "authorization": {
                        "authorization_url": format!("{}/token/{id}", s.base),
                        "server_url": format!("{}/broker", s.base),
                        "client_id": format!("00000000-0000-0000-0000-{id:012}"),
                    },
                }),
            )
        }
        ("GET", "/tenant/_apis/distributedtask/pools") => {
            if auth != format!("bearer:{ADMIN}") || !query.contains("api-version=") {
                return (401, message("Bad credentials"));
            }
            let pools: Vec<Value> = s
                .groups
                .iter()
                .map(|group| {
                    json!({
                        "id": group["id"].as_u64().unwrap_or(0) + 1000,
                        "name": group["name"],
                        "isHosted": false,
                        "isInternal": group["default"],
                    })
                })
                .chain(std::iter::once(
                    json!({ "id": 2, "name": "GitHub Actions", "isHosted": true }),
                ))
                .collect();
            (200, json!({ "count": pools.len(), "value": pools }))
        }
        ("POST", p)
            if p.starts_with("/tenant/_apis/distributedtask/pools/") && p.ends_with("/agents") =>
        {
            if auth != format!("bearer:{ADMIN}") {
                return (401, message("Bad credentials"));
            }
            let pool: u64 = p
                .trim_start_matches("/tenant/_apis/distributedtask/pools/")
                .trim_end_matches("/agents")
                .parse()
                .unwrap_or(0);
            let id = s.next_runner;
            s.next_runner += 1;
            s.runners.push(json!({
                "id": id,
                "name": body["name"],
                "pool": pool,
                "labels": body["labels"],
                "publicKey": body["authorization"]["publicKey"],
            }));
            (
                200,
                json!({
                    "id": id,
                    "name": body["name"],
                    "authorization": {
                        "clientId": format!("00000000-0000-0000-0000-{id:012}"),
                        "authorizationUrl": format!("{}/token/{id}", s.base),
                    },
                    "properties": {},
                }),
            )
        }
        _ => (404, message("Not Found")),
    }
}
