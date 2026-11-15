//! MCP authorization: the approval page is an OAuth 2.1 authorization server for this node's MCP server, as the MCP
//! specification's authorization chapter (revision 2025-11-25, which 2026-07-28 keeps) asks. An MCP client is given only
//! the MCP URL. Its first request is refused `401` with the protected resource metadata's URL; from there it finds this
//! server, registers itself, and sends the owner to the consent page, where one passkey tap signs the owner in and approves
//! the client with the scope it asked for. The client then holds short-lived tokens of that scope, and nobody copies a
//! token anywhere.
//!
//! Endpoints, all on the page origin (`approvalOrigin`), which is the issuer. The protected resource is `<origin>/mcp`.
//! - `GET /.well-known/oauth-protected-resource` and `.../mcp` (RFC 9728): the resource, this issuer, the scopes.
//! - `GET /.well-known/oauth-authorization-server` (RFC 8414): the endpoints, `code` with PKCE `S256` only, public
//!   clients only (`none`), and the RFC 9207 `iss` parameter on every redirect.
//! - `POST /oauth/register` (RFC 7591): a public client with its exact redirect URIs, each an `https` URL or an `http`
//!   URL on a loopback host (`127.0.0.1`, `[::1]`, `localhost`). Registration gives a client nothing but an id.
//! - `GET /oauth/authorize`: checks the request and renders the consent page, which shows the client's registered name,
//!   whether another client registered the same name, the host its answer goes to and the scope it asks for. The page's
//!   `POST /oauth/authorize/begin` mints the statement the owner's passkey signs (action `mcp_authorize`, target the
//!   client id, `paramsDigest` over the whole request), `.../approve` spends it with the kernel's own approval check
//!   (owner passkeys, sign count, one-use nonce) and answers the redirect with a one-use code, `.../deny` the redirect
//!   with `access_denied`.
//! - `POST /oauth/token`: `authorization_code` with the PKCE verifier, and `refresh_token`, which rotates: each refresh
//!   token works once, and a refresh token used again after its rotation revokes the grant. A token request may narrow
//!   the scope and never widen it.
//! - `POST /oauth/revoke` (RFC 7009): an access token alone, or a refresh token and its whole grant.
//! - `GET /oauth/step-up?grant=<id>`: the page where one passkey tap (action `mcp_authorize`, target the client id,
//!   `paramsDigest` over the grant) confirms the owner's signed-in session again for [`STEP_UP_MS`]; its
//!   `POST /oauth/step-up/begin` and `.../approve` work as the consent page's.
//! - `POST /oauth/device_authorization` (RFC 8628): for an agent away from the person's browser (in a container, on a
//!   server). Checked as an authorization request is, it answers a device code for the agent and a user code (eight
//!   letters, `XXXX-XXXX`) with the link `GET /oauth/device?user_code=...`, where the person approves on any device on
//!   the same consent page, with the same passkey statement, which binds the user code instead of a redirect. The agent
//!   polls `POST /oauth/token` with the device code grant: `authorization_pending`, `slow_down` (the interval grows by
//!   five seconds), `access_denied`, `expired_token` after ten minutes, then the tokens, once. A client may register for
//!   the device grant with no redirect at all.
//! - `GET /approve/oauth/clients` and `POST /approve/oauth/revoke`: the owner's list of connected clients and its revoke.
//! - On the unix socket alone, `POST /local/oauth/clients` and `POST /local/oauth/revoke`, behind the setup code's
//!   proof file: what `arca oauth list` and `arca oauth revoke` call.
//!
//! Scopes are `viewer`, `doctor` and `operator`, the levels a stored token has; the highest one asked wins, and none asked
//! means `viewer`. `tool:<verb>` narrows a grant to the verbs it names. `admin` is never granted here. `approvals` makes
//! the grant the owner's own signed-in session: its client is asked, by elicitation, to approve the plans that wait for
//! the owner (`owner_approval_pending`, and the plan an `action_propose` answers), while the passkey that approved it is
//! less than [`STEP_UP_MS`] old or was confirmed again on the step-up page; plans that delete or erase keep their passkey
//! card. While a grant with `approvals` is live, the waiting action plans stay open past their fifteen minutes. An access
//! token lives one hour and a refresh token thirty days; a code lives sixty seconds.
//!
//! Tokens and device codes are opaque: `arca_at_`, `arca_rt_` or `arca_dc_` and 43 base64url characters (256 random bits). The state directory keeps
//! only their SHA-256, in `mcp-oauth.json` (mode 0600, written whole and renamed). Codes, device codes and consent
//! requests live in memory only. `mcp-oauth-journal.jsonl` records each registration, grant, denial and revocation, and each refresh
//! token or code used twice. No token, code, verifier or proof reaches the journal, a log line or an error.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use arca_api::Scope;
use arca_approval::{ARCA_NAMESPACE, ArcaAction, TTL_MAX_MS, challenge_for, parse_proof};
use arca_kernel::canon::{canonical_json, params_digest};
use arca_mcp::{Caller, ISSUED_PREFIX, IssuedTokens};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Query, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::approve_page::PageGate;
use crate::handlers::{Clock, iso_utc};
use crate::kernel::Kernel;

/// The clients and grants, in the state directory.
pub const STORE_FILE: &str = "mcp-oauth.json";
/// One line per registration, grant, denial and revocation, in the state directory.
pub const JOURNAL_FILE: &str = "mcp-oauth-journal.jsonl";
/// How long an access token lives.
pub const ACCESS_TTL_MS: u64 = 60 * 60 * 1000;
/// How long a refresh token lives from the moment it is issued.
pub const REFRESH_TTL_MS: u64 = 30 * 24 * 60 * 60 * 1000;
/// How long an authorization code lives.
pub const CODE_TTL_MS: u64 = 60 * 1000;
/// How long a consent request waits for the owner.
pub const REQUEST_TTL_MS: u64 = 10 * 60 * 1000;
/// How long a begun statement waits for its passkey.
pub const BEGUN_MAX_MS: u64 = 120 * 1000;
/// How long the owner's passkey counts for a session with `approvals`: from the consent, then from each step-up.
pub const STEP_UP_MS: u64 = 12 * 60 * 60 * 1000;
/// The scope word that makes a grant the owner's signed-in session, which is asked to approve.
pub const APPROVALS: &str = "approvals";
pub const STEP_UP_PATH: &str = "/oauth/step-up";
/// The paths of the endpoints, under the issuer.
pub const AUTHORIZE_PATH: &str = "/oauth/authorize";
pub const TOKEN_PATH: &str = "/oauth/token";
pub const REGISTER_PATH: &str = "/oauth/register";
pub const REVOKE_PATH: &str = "/oauth/revoke";
pub const RESOURCE_METADATA_PATH: &str = "/.well-known/oauth-protected-resource";
/// Where an agent away from the person's browser asks for a device code (RFC 8628 3.1), and the page the person opens.
pub const DEVICE_AUTHORIZATION_PATH: &str = "/oauth/device_authorization";
pub const DEVICE_PATH: &str = "/oauth/device";
/// The grant type an agent polls the token endpoint with (RFC 8628 3.4).
pub const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
/// How long an agent waits between polls at first, and how much longer after each poll that came too soon (RFC 8628 3.5).
pub const DEVICE_INTERVAL_MS: u64 = 5 * 1000;
const SLOW_DOWN_MS: u64 = 5 * 1000;
pub const SERVER_METADATA_PATH: &str = "/.well-known/oauth-authorization-server";
/// The unix socket's routes for `arca oauth`.
pub const LOCAL_CLIENTS_PATH: &str = "/local/oauth/clients";
pub const LOCAL_REVOKE_PATH: &str = "/local/oauth/revoke";

const ACCESS_PREFIX: &str = "arca_at_";
const REFRESH_PREFIX: &str = "arca_rt_";
const CODE_PREFIX: &str = "arca_ac_";
const DEVICE_CODE_PREFIX: &str = "arca_dc_";
/// The letters of a user code: no vowels, and nothing that reads as another letter or a digit (RFC 8628 6.1). Eight of
/// them, shown as two groups of four, are 20^8 (about 2.6 * 10^10) codes.
const USER_CODE_LETTERS: &[u8; 20] = b"BCDFGHJKLMNPQRSTVWXZ";
const SECRET_CHARS: usize = 43;
/// The scopes a client may ask for, lowest first.
const LEVELS: [Scope; 3] = [Scope::Viewer, Scope::Doctor, Scope::Operator];
/// Registered clients that no owner approved yet: the most kept, and how long one is kept.
const UNAPPROVED_MAX: usize = 100;
const UNAPPROVED_TTL_MS: u64 = 24 * 60 * 60 * 1000;
/// Consent requests waiting at once.
const PENDING_MAX: usize = 50;
/// The longest client name kept, in characters, and the longest redirect URI, in bytes.
const NAME_MAX: usize = 80;
const REDIRECT_MAX: usize = 512;
const REDIRECTS_MAX: usize = 10;
/// The rotated refresh tokens a grant remembers, to know one used again.
const ROTATED_MAX: usize = 32;
const BODY_MAX: usize = 16 * 1024;
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; \
                   frame-ancestors 'none'; base-uri 'none'; object-src 'none'; form-action 'none'";
/// The device page's: the same, but its one form (type the code) may be sent to this page.
const DEVICE_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; \
                          frame-ancestors 'none'; base-uri 'none'; object-src 'none'; form-action 'self'";

/// A client that registered (RFC 7591).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Client {
    id: String,
    name: String,
    redirect_uris: Vec<String>,
    registered_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    revoked_at: Option<u64>,
}

/// One stored token: the SHA-256 of its secret in lowercase hex, and when it expires.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Hashed {
    sha256: String,
    expires: u64,
}

/// What the owner approved for one client: a scope, an optional tool list and the resource, with the tokens that carry it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Grant {
    id: String,
    client_id: String,
    scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<String>>,
    resource: String,
    approved_at: u64,
    /// A prefix of the fingerprint of the passkey that approved it.
    key: String,
    refresh: Option<Hashed>,
    #[serde(default)]
    rotated: Vec<String>,
    #[serde(default)]
    access: Vec<Hashed>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    revoked_at: Option<u64>,
    /// What the owner approved, which a refresh never loses. A refresh that asks for less narrows the whole grant, every
    /// live token of it included, until a refresh with no `scope` puts this back on it (RFC 6749 6). Absent in a store
    /// written before it was kept; `scope` and `tools` stand in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    approved: Option<Approved>,
    /// Whether the owner approved this client as their signed-in session, which is asked to approve plans.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    approvals: bool,
    /// Until when (unix ms) the owner's passkey counts for that session: the consent's, or the last step-up's.
    #[serde(default, skip_serializing_if = "is_zero")]
    stepped_up_until: u64,
    /// A prefix of the fingerprint of the passkey of the last step-up, when there was one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    step_key: Option<String>,
}

/// The scope and tool list the owner's passkey approved for a grant.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Approved {
    scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<String>>,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Stored {
    clients: Vec<Client>,
    grants: Vec<Grant>,
}

/// A consent request waiting for the owner.
#[derive(Debug, Clone)]
struct Pending {
    id: String,
    client_id: String,
    redirect_uri: String,
    redirect_given: bool,
    state: Option<String>,
    scope: Scope,
    tools: Option<Vec<String>>,
    approvals: bool,
    resource: String,
    challenge: String,
    created: u64,
    statement: Option<String>,
    begun_at: Option<u64>,
    /// For a request an agent made with the device grant: the SHA-256 of its device code, and the user code the person
    /// typed or followed. It has no redirect, state or PKCE challenge.
    device: Option<(String, String)>,
}

/// A device code (RFC 8628) until the agent's poll takes its answer.
#[derive(Debug, Clone)]
struct Device {
    client_id: String,
    user_code: String,
    /// The consent request the person approves or denies.
    request: String,
    expires: u64,
    /// How long the agent waits between polls now, and when it last polled in time.
    interval: u64,
    last_poll: Option<u64>,
    answer: DeviceAnswer,
}

#[derive(Debug, Clone)]
enum DeviceAnswer {
    Waiting,
    /// The owner's passkey approved: what the grant carries.
    Approved {
        scope: Scope,
        tools: Option<Vec<String>>,
        resource: String,
        key: String,
        /// Whether the owner made it their signed-in session for approvals, and when the passkey approved it.
        approvals: bool,
        approved_at: u64,
    },
    Denied,
}

/// A code the owner's approval made, until the client exchanges it.
#[derive(Debug, Clone)]
struct Code {
    client_id: String,
    redirect_uri: String,
    redirect_given: bool,
    scope: Scope,
    tools: Option<Vec<String>>,
    approvals: bool,
    resource: String,
    challenge: String,
    expires: u64,
    key: String,
    approved_at: u64,
}

#[derive(Default)]
struct Inner {
    stored: Stored,
    pending: Vec<Pending>,
    codes: HashMap<String, Code>,
    /// Codes exchanged once, with the grant they made and when they would have expired.
    used_codes: HashMap<String, (String, u64)>,
    /// Device codes by the SHA-256 of each, in memory only, kept a while past their expiry to answer `expired_token`.
    devices: HashMap<String, Device>,
    /// When each grant's access token was last used, in memory only.
    last_used: HashMap<String, u64>,
    /// Whether a revocation ended a grant in memory that the store on disk does not have yet: the next revocation
    /// request saves it before anything else.
    unsaved: bool,
    /// The step-up statements begun, by grant id: the statement and when it was begun.
    step_ups: HashMap<String, (String, u64)>,
}

/// The authorization server: the page origin it issues for, the resource it issues tokens to, the kernel whose owner
/// approves, and the store.
pub struct OAuth {
    origin: String,
    rp_id: String,
    resource: String,
    kernel: Arc<Mutex<Kernel>>,
    clock: Clock,
    state_dir: PathBuf,
    inner: Mutex<Inner>,
    log: Arc<dyn Fn(&str) + Send + Sync>,
    gate: Option<PageGate>,
    /// Until when (unix ms) a grant with `approvals` is live: the latest refresh expiry among them, zero with none. The
    /// kernel reads it to hold waiting proposals open.
    session_until: Arc<AtomicU64>,
}

/// What the owner's signed-in session may do for one caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Session {
    /// The caller is the owner's signed-in session and may be asked: `key` is the passkey that last confirmed it.
    Ready { key: String },
    /// The caller is the owner's signed-in session, but its passkey is too old: one tap at `url` confirms it again.
    StepUp { url: String },
    /// The caller is not the owner's signed-in session.
    None,
}

/// An OAuth error answer: the RFC 6749 code and a sentence.
#[derive(Debug)]
struct OAuthError {
    status: StatusCode,
    code: &'static str,
    description: String,
}

impl OAuthError {
    fn new(code: &'static str, description: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code,
            description: description.into(),
        }
    }

    fn status(mut self, status: StatusCode) -> Self {
        self.status = status;
        self
    }
}

impl IntoResponse for OAuthError {
    fn into_response(self) -> Response {
        (
            self.status,
            [
                (header::CACHE_CONTROL, "no-store"),
                (header::PRAGMA, "no-cache"),
            ],
            Json(json!({ "error": self.code, "error_description": self.description })),
        )
            .into_response()
    }
}

/// Why `GET /oauth/authorize` made no consent request: on the page, for a client or redirect that is not known, or back
/// to the client's redirect with the client's `state`.
enum AuthorizeFail {
    Page(OAuthError),
    Redirect {
        uri: String,
        state: Option<String>,
        error: OAuthError,
    },
}

/// A page refusal: a status, a code and the owner's words. A bare one is the status alone.
#[derive(Debug)]
pub struct Refusal {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl Refusal {
    fn bare(status: StatusCode) -> Self {
        Self {
            status,
            code: "",
            message: String::new(),
        }
    }
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        if self.code.is_empty() {
            return self.status.into_response();
        }
        (
            self.status,
            Json(json!({ "refusal": self.code, "message": self.message })),
        )
            .into_response()
    }
}

fn refused(status: StatusCode, code: &'static str, message: impl Into<String>) -> Refusal {
    Refusal {
        status,
        code,
        message: message.into(),
    }
}

impl OAuth {
    /// The authorization server of the page at `origin`, whose relying party is `rp_id`, over the state directory's store.
    /// A store file that is missing is empty; one that is not a private regular file, or does not parse, fails the open.
    pub fn open(
        origin: &str,
        rp_id: &str,
        kernel: Arc<Mutex<Kernel>>,
        clock: Clock,
        state_dir: &Path,
    ) -> io::Result<OAuth> {
        let stored = read_store(&state_dir.join(STORE_FILE))?;
        let session_until = Arc::new(AtomicU64::new(session_until(&stored, clock())));
        Ok(OAuth {
            origin: origin.to_string(),
            rp_id: rp_id.to_string(),
            resource: format!("{origin}/mcp"),
            kernel,
            clock,
            state_dir: state_dir.to_path_buf(),
            inner: Mutex::new(Inner {
                stored,
                ..Inner::default()
            }),
            log: Arc::new(|line| eprintln!("{line}")),
            gate: None,
            session_until,
        })
    }

    /// Until when the owner's signed-in session is connected (unix ms; zero when it is not), kept current as grants change.
    pub fn owner_session(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.session_until)
    }

    /// The step-up page of grant `grant`.
    pub fn step_up_url(&self, grant: &str) -> String {
        format!(
            "{}{STEP_UP_PATH}?grant={}",
            self.origin,
            percent_encode(grant)
        )
    }

    /// What `caller` may do as the owner's signed-in session: an issued token of a live grant with `approvals`, of a
    /// client that is not revoked, is it; it may be asked while its passkey is less than [`STEP_UP_MS`] old.
    pub fn session(&self, caller: &Caller) -> Session {
        let Some(grant_id) = caller
            .token_id
            .strip_prefix(ISSUED_PREFIX)
            .and_then(|rest| rest.split_once('/'))
            .map(|(_, grant)| grant)
        else {
            return Session::None;
        };
        let now = self.now();
        let inner = self.lock();
        let Some(grant) = inner.stored.grants.iter().find(|grant| {
            grant.id == grant_id
                && grant.approvals
                && grant.revoked_at.is_none()
                && grant
                    .refresh
                    .as_ref()
                    .is_some_and(|token| now < token.expires)
        }) else {
            return Session::None;
        };
        if Self::client(&inner, &grant.client_id).is_none() {
            return Session::None;
        }
        if now < grant.stepped_up_until {
            Session::Ready {
                key: grant.step_key.clone().unwrap_or_else(|| grant.key.clone()),
            }
        } else {
            Session::StepUp {
                url: self.step_up_url(&grant.id),
            }
        }
    }

    /// Replaces the log sink.
    pub fn with_log(mut self, log: Arc<dyn Fn(&str) + Send + Sync>) -> Self {
        self.log = log;
        self
    }

    /// Closes every route while the page's gate says why, as the page itself is closed.
    pub fn with_gate(mut self, gate: Option<PageGate>) -> Self {
        self.gate = gate;
        self
    }

    /// The MCP resource the tokens are for: `<origin>/mcp`.
    pub fn resource(&self) -> &str {
        &self.resource
    }

    /// The URL of the protected resource metadata, which the MCP server's `401` names.
    pub fn resource_metadata_url(&self) -> String {
        format!("{}{RESOURCE_METADATA_PATH}/mcp", self.origin)
    }

    fn now(&self) -> u64 {
        (self.clock)()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn persist(&self, inner: &Inner) -> io::Result<()> {
        let path = self.state_dir.join(STORE_FILE);
        let temp = self
            .state_dir
            .join(format!("{STORE_FILE}.tmp-{}", std::process::id()));
        self.session_until
            .store(session_until(&inner.stored, self.now()), Ordering::SeqCst);
        let bytes = serde_json::to_vec(&inner.stored).map_err(io::Error::other)?;
        let _ = fs::remove_file(&temp);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, &path)?;
        File::open(&self.state_dir)?.sync_all()
    }

    /// Saves a revocation: once more if the first write fails, then a log line. A grant ended in memory and not on disk
    /// would come back at the next start, so the caller says so to whoever asked. Answers whether it was saved.
    /// Saves a revocation, once more if the first write fails. When both fail, the store is marked unsaved, so the
    /// client's retry of `/oauth/revoke` saves it, and a log line names the grant. Answers whether it was saved.
    fn persist_revoked(&self, inner: &mut Inner, grant: Option<&str>) -> bool {
        if self.persist(inner).is_ok() || self.persist(inner).is_ok() {
            inner.unsaved = false;
            return true;
        }
        inner.unsaved = true;
        (self.log)(&format!(
            "arca: the revocation of MCP grant {} was not saved: it ends now, but would work again after a restart until it is saved; revoke it again",
            grant.unwrap_or("(an access token)")
        ));
        false
    }

    /// The journal line of a revocation, saved or not: one that was not saved says so, and is still written.
    fn journal_revoked(&self, now: u64, event: &str, mut fields: Value, saved: bool) {
        if !saved {
            fields["saved"] = json!(false);
        }
        self.journal(now, event, fields);
    }

    fn journal(&self, now: u64, event: &str, fields: Value) {
        let mut line = json!({ "at": iso_utc(now), "event": event });
        if let (Some(line), Value::Object(fields)) = (line.as_object_mut(), fields) {
            line.extend(fields);
        }
        let written = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(self.state_dir.join(JOURNAL_FILE))
            .and_then(|mut file| writeln!(file, "{line}"));
        if let Err(error) = written {
            (self.log)(&format!(
                "arca: the MCP authorization journal was not written: {error}"
            ));
        }
    }

    /// Drops what has expired: consent requests, codes, access tokens, unapproved clients past a day.
    fn sweep(inner: &mut Inner, now: u64) {
        inner
            .pending
            .retain(|pending| now.saturating_sub(pending.created) < REQUEST_TTL_MS);
        inner.codes.retain(|_, code| now < code.expires);
        inner
            .devices
            .retain(|_, device| now < device.expires.saturating_add(REQUEST_TTL_MS));
        inner.used_codes.retain(|_, (_, expires)| now < *expires);
        for grant in &mut inner.stored.grants {
            grant.access.retain(|token| now < token.expires);
        }
        let approved: Vec<String> = inner
            .stored
            .grants
            .iter()
            .map(|grant| grant.client_id.clone())
            .collect();
        inner.stored.clients.retain(|client| {
            approved.contains(&client.id)
                || client.revoked_at.is_some()
                || now.saturating_sub(client.registered_at) < UNAPPROVED_TTL_MS
        });
    }

    fn client<'a>(inner: &'a Inner, id: &str) -> Option<&'a Client> {
        inner
            .stored
            .clients
            .iter()
            .find(|client| client.id == id && client.revoked_at.is_none())
    }

    /// Whether another live client registered the same name, compared without regard to case or surrounding space.
    fn name_shared(inner: &Inner, client: &Client) -> bool {
        let name = client.name.trim().to_lowercase();
        inner.stored.clients.iter().any(|other| {
            other.id != client.id
                && other.revoked_at.is_none()
                && other.name.trim().to_lowercase() == name
        })
    }

    // ---------------------------------------------------------------------------------------------------------------
    // Registration

    fn register(&self, body: &[u8]) -> Result<Value, OAuthError> {
        let metadata: Value = serde_json::from_slice(body).map_err(|_| {
            OAuthError::new(
                "invalid_client_metadata",
                "the registration is not a JSON object",
            )
        })?;
        let uris: Vec<String> = metadata
            .get("redirect_uris")
            .and_then(Value::as_array)
            .map(|uris| {
                uris.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        // A client that asks for the device grant (an agent away from the browser) may register no redirect at all.
        let device = metadata
            .get("grant_types")
            .and_then(Value::as_array)
            .is_some_and(|grants| grants.iter().any(|grant| grant == DEVICE_GRANT));
        if (uris.is_empty() && !device) || uris.len() > REDIRECTS_MAX {
            return Err(OAuthError::new(
                "invalid_redirect_uri",
                format!(
                    "a client registers 1 to {REDIRECTS_MAX} redirect_uris, or none with grant_types naming {DEVICE_GRANT}"
                ),
            ));
        }
        if let Some(bad) = uris.iter().find(|uri| !redirect_ok(uri)) {
            let shown: String = bad.chars().take(80).collect();
            return Err(OAuthError::new(
                "invalid_redirect_uri",
                format!(
                    "{shown:?} is not a redirect Arca sends to: an https URL, or an http URL on 127.0.0.1, [::1] or localhost, with no fragment"
                ),
            ));
        }
        let name = metadata
            .get("client_name")
            .and_then(Value::as_str)
            .map(clean_name)
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "An app with no name".to_string());
        let now = self.now();
        let mut inner = self.lock();
        Self::sweep(&mut inner, now);
        let approved: Vec<&str> = inner
            .stored
            .grants
            .iter()
            .map(|grant| grant.client_id.as_str())
            .collect();
        let unapproved = inner
            .stored
            .clients
            .iter()
            .filter(|client| client.revoked_at.is_none() && !approved.contains(&client.id.as_str()))
            .count();
        if unapproved >= UNAPPROVED_MAX {
            return Err(OAuthError::new(
                "temporarily_unavailable",
                "too many apps registered and are waiting for the owner; try again tomorrow",
            )
            .status(StatusCode::TOO_MANY_REQUESTS));
        }
        let client = Client {
            id: format!("mcpc-{}", random_hex(12)),
            name,
            redirect_uris: uris,
            registered_at: now,
            revoked_at: None,
        };
        inner.stored.clients.push(client.clone());
        if self.persist(&inner).is_err() {
            inner.stored.clients.pop();
            return Err(OAuthError::new("server_error", "the client was not saved")
                .status(StatusCode::INTERNAL_SERVER_ERROR));
        }
        drop(inner);
        self.journal(
            now,
            "registered",
            json!({ "client": client.id, "name": client.name, "redirects": client.redirect_uris.iter().map(|uri| redirect_host(uri)).collect::<Vec<_>>() }),
        );
        let (grant_types, response_types) = match (client.redirect_uris.is_empty(), device) {
            (true, _) => (json!([DEVICE_GRANT, "refresh_token"]), json!([])),
            (false, true) => (
                json!(["authorization_code", DEVICE_GRANT, "refresh_token"]),
                json!(["code"]),
            ),
            (false, false) => (
                json!(["authorization_code", "refresh_token"]),
                json!(["code"]),
            ),
        };
        Ok(json!({
            "client_id": client.id,
            "client_id_issued_at": now / 1000,
            "client_name": client.name,
            "redirect_uris": client.redirect_uris,
            "grant_types": grant_types,
            "response_types": response_types,
            "token_endpoint_auth_method": "none",
        }))
    }

    // ---------------------------------------------------------------------------------------------------------------
    // Authorization

    /// The consent request a `GET /oauth/authorize` makes, with its client and whether its name is shared, or why there is
    /// none. A request whose client or redirect is not known is answered on the page and never redirected.
    fn authorize(
        &self,
        query: &HashMap<String, String>,
    ) -> Result<(Pending, Client, bool), AuthorizeFail> {
        let now = self.now();
        let mut inner = self.lock();
        Self::sweep(&mut inner, now);
        let param = |key: &str| query.get(key).map(String::as_str);
        let client = param("client_id")
            .and_then(|id| Self::client(&inner, id))
            .cloned()
            .ok_or_else(|| {
                AuthorizeFail::Page(OAuthError::new(
                    "invalid_client",
                    "Arca does not know this app. Remove Arca from the app and add it again.",
                ))
            })?;
        let (redirect_uri, redirect_given) = match param("redirect_uri") {
            Some(uri) if client.redirect_uris.iter().any(|known| known == uri) => {
                (uri.to_string(), true)
            }
            None if client.redirect_uris.len() == 1 => (client.redirect_uris[0].clone(), false),
            _ => {
                return Err(AuthorizeFail::Page(OAuthError::new(
                    "invalid_request",
                    "The app asked to be answered at an address it did not register. Nothing was approved.",
                )));
            }
        };
        let state = param("state").map(str::to_string);
        // An error is sent back to the redirect only for a client the owner approved before: registration is open, so
        // for any other the redirect is not trusted yet, and the error stays on this page instead of making the page's
        // own address a link to anywhere (RFC 9700 4.11.2; MCP authorization, "Open Redirection").
        let trusted = inner.stored.grants.iter().any(|grant| {
            grant.client_id == client.id
                && grant.revoked_at.is_none()
                && grant
                    .refresh
                    .as_ref()
                    .is_some_and(|token| now < token.expires)
        });
        let back = |error: OAuthError| {
            if trusted {
                Err(AuthorizeFail::Redirect {
                    uri: redirect_uri.clone(),
                    state: state.clone(),
                    error,
                })
            } else {
                Err(AuthorizeFail::Page(error))
            }
        };
        if param("response_type") != Some("code") {
            return back(OAuthError::new(
                "unsupported_response_type",
                "Arca answers response_type=code only",
            ));
        }
        let challenge = match (param("code_challenge"), param("code_challenge_method")) {
            (Some(challenge), Some("S256")) if pkce_challenge_ok(challenge) => {
                challenge.to_string()
            }
            _ => {
                return back(OAuthError::new(
                    "invalid_request",
                    "Arca needs PKCE with code_challenge_method=S256 and a 43-character code_challenge",
                ));
            }
        };
        let resource = match param("resource") {
            None => self.resource.clone(),
            Some(asked) if canonical_resource(asked) == canonical_resource(&self.resource) => {
                self.resource.clone()
            }
            Some(_) => {
                return back(OAuthError::new(
                    "invalid_target",
                    format!("Arca issues tokens for {} only", self.resource),
                ));
            }
        };
        let (scope, tools, approvals) = match parse_scope(param("scope").unwrap_or_default()) {
            Ok(parsed) => parsed,
            Err(why) => {
                return back(OAuthError::new("invalid_scope", why));
            }
        };
        if inner.pending.len() >= PENDING_MAX {
            return back(OAuthError::new(
                "temporarily_unavailable",
                "too many requests are waiting for the owner",
            ));
        }
        let pending = Pending {
            id: format!("oar-{}", random_hex(16)),
            client_id: client.id.clone(),
            redirect_uri,
            redirect_given,
            state,
            scope,
            tools,
            approvals,
            resource,
            challenge,
            created: now,
            statement: None,
            begun_at: None,
            device: None,
        };
        inner.pending.push(pending.clone());
        let shared = Self::name_shared(&inner, &client);
        Ok((pending, client, shared))
    }

    /// The arguments the owner's statement binds for a consent request: every field the code will carry.
    fn bound(pending: &Pending, client: &Client) -> Value {
        let mut bound = json!({
            "request": pending.id,
            "client": client.id,
            "name": client.name,
            "redirectUri": pending.redirect_uri,
            "scope": scope_text(pending.scope, pending.tools.as_deref(), pending.approvals),
            "resource": pending.resource,
            "codeChallenge": pending.challenge,
        });
        // A device request binds the code the person saw instead of a redirect.
        if let Some((_, user_code)) = &pending.device {
            bound["userCode"] = json!(user_code);
        }
        bound
    }

    fn begin(&self, id: &str) -> Result<Value, Refusal> {
        let now = self.now();
        let (lines, host) = self.owner_lines()?;
        let mut inner = self.lock();
        Self::sweep(&mut inner, now);
        let client = {
            let pending = find_pending(&inner, id)?;
            Self::client(&inner, &pending.client_id)
                .cloned()
                .ok_or_else(gone)?
        };
        let nonce = random_hex(16);
        let pending = inner
            .pending
            .iter_mut()
            .find(|pending| pending.id == id)
            .ok_or_else(gone)?;
        let digest = params_digest(&Self::bound(pending, &client)).map_err(|_| internal())?;
        let statement = canonical_json(&json!({
            "host": host,
            "action": ArcaAction::McpAuthorize.as_str(),
            "target": client.id,
            "paramsDigest": digest,
            "issuedAt": iso_utc(now),
            "expiresAt": iso_utc(now.saturating_add(TTL_MAX_MS.min(BEGUN_MAX_MS * 2))),
            "nonce": nonce,
        }))
        .map_err(|_| internal())?;
        let challenge =
            challenge_for(ARCA_NAMESPACE, statement.as_bytes()).map_err(|_| internal())?;
        pending.statement = Some(statement.clone());
        pending.begun_at = Some(now);
        Ok(Self::begun(id, &statement, &challenge, &self.rp_id, &lines))
    }

    fn approve(&self, id: &str, statement: &str, proof: &Value) -> Result<Value, Refusal> {
        let now = self.now();
        let (pending, client) = {
            let mut inner = self.lock();
            Self::sweep(&mut inner, now);
            let pending = find_pending(&inner, id)?.clone();
            let client = Self::client(&inner, &pending.client_id)
                .cloned()
                .ok_or_else(gone)?;
            (pending, client)
        };
        let (Some(begun), Some(begun_at)) = (&pending.statement, pending.begun_at) else {
            return Err(refused(
                StatusCode::CONFLICT,
                "not_begun",
                "tap Approve again",
            ));
        };
        if now.saturating_sub(begun_at) > BEGUN_MAX_MS || begun != statement {
            return Err(refused(
                StatusCode::CONFLICT,
                "stale",
                "this approval is out of date: tap Approve again",
            ));
        }
        let key = self.spend(
            &client.id,
            &Self::bound(&pending, &client),
            statement,
            proof,
            now,
        )?;
        if let Some((device_hash, _)) = &pending.device {
            {
                let mut inner = self.lock();
                if !inner.pending.iter().any(|kept| kept.id == pending.id) {
                    return Err(gone());
                }
                inner.pending.retain(|kept| kept.id != pending.id);
                let device = inner.devices.get_mut(device_hash).ok_or_else(gone)?;
                device.answer = DeviceAnswer::Approved {
                    scope: pending.scope,
                    tools: pending.tools.clone(),
                    resource: pending.resource.clone(),
                    key: key.clone(),
                    approvals: pending.approvals,
                    approved_at: now,
                };
            }
            let scope = scope_text(pending.scope, pending.tools.as_deref(), pending.approvals);
            self.journal(
                now,
                "granted",
                json!({ "client": client.id, "name": client.name, "scope": scope, "device": true, "by": format!("passkey {key}") }),
            );
            (self.log)(&format!(
                "arca: the owner approved MCP client {} ({}) on a device code with scope {scope}",
                client.id, client.name
            ));
            return Ok(json!({
                "message": "Approved. Go back to the app: it connects within a few seconds.",
            }));
        }
        let code = secret(CODE_PREFIX);
        {
            let mut inner = self.lock();
            if !inner.pending.iter().any(|kept| kept.id == pending.id) {
                return Err(gone());
            }
            inner.pending.retain(|kept| kept.id != pending.id);
            inner.codes.insert(
                hash_hex(&code),
                Code {
                    client_id: client.id.clone(),
                    redirect_uri: pending.redirect_uri.clone(),
                    redirect_given: pending.redirect_given,
                    scope: pending.scope,
                    tools: pending.tools.clone(),
                    approvals: pending.approvals,
                    resource: pending.resource.clone(),
                    challenge: pending.challenge.clone(),
                    expires: now + CODE_TTL_MS,
                    key: key.clone(),
                    approved_at: now,
                },
            );
        }
        let scope = scope_text(pending.scope, pending.tools.as_deref(), pending.approvals);
        self.journal(
            now,
            "granted",
            json!({ "client": client.id, "name": client.name, "scope": scope, "redirect": redirect_host(&pending.redirect_uri), "by": format!("passkey {key}") }),
        );
        (self.log)(&format!(
            "arca: the owner approved MCP client {} ({}) with scope {scope}",
            client.id, client.name
        ));
        Ok(json!({
            "redirect": with_params(&pending.redirect_uri, &[
                ("code", Some(code.as_str())),
                ("state", pending.state.as_deref()),
                ("iss", Some(self.origin.as_str())),
            ]),
        }))
    }

    fn deny(&self, id: &str) -> Result<Value, Refusal> {
        let now = self.now();
        let mut inner = self.lock();
        Self::sweep(&mut inner, now);
        let pending = find_pending(&inner, id)?.clone();
        inner.pending.retain(|kept| kept.id != id);
        if let Some((device_hash, _)) = &pending.device
            && let Some(device) = inner.devices.get_mut(device_hash)
        {
            device.answer = DeviceAnswer::Denied;
        }
        drop(inner);
        self.journal(
            now,
            "denied",
            json!({ "client": pending.client_id, "scope": scope_text(pending.scope, pending.tools.as_deref(), pending.approvals), "device": pending.device.is_some() }),
        );
        if pending.device.is_some() {
            return Ok(json!({ "message": "Not allowed. The app is told no." }));
        }
        Ok(json!({
            "redirect": with_params(&pending.redirect_uri, &[
                ("error", Some("access_denied")),
                ("error_description", Some("the owner said no")),
                ("state", pending.state.as_deref()),
                ("iss", Some(self.origin.as_str())),
            ]),
        }))
    }

    // ---------------------------------------------------------------------------------------------------------------
    // Step-up: the owner's passkey confirms the signed-in session again

    /// The live grant with `approvals` named `id`, and its client.
    fn session_grant(inner: &Inner, id: &str, now: u64) -> Result<(Grant, Client), Refusal> {
        let grant = inner
            .stored
            .grants
            .iter()
            .find(|grant| {
                grant.id == id
                    && grant.approvals
                    && grant.revoked_at.is_none()
                    && grant
                        .refresh
                        .as_ref()
                        .is_some_and(|token| now < token.expires)
            })
            .cloned()
            .ok_or_else(|| {
                refused(
                    StatusCode::NOT_FOUND,
                    "gone",
                    "this session is no longer connected: connect the app again",
                )
            })?;
        let client = Self::client(inner, &grant.client_id)
            .cloned()
            .ok_or_else(gone)?;
        Ok((grant, client))
    }

    /// The arguments a step-up statement binds: the grant, its client and that it is a step-up.
    fn step_up_bound(grant: &Grant, client: &Client) -> Value {
        json!({ "stepUp": grant.id, "client": client.id, "name": client.name })
    }

    fn step_up_begin(&self, id: &str) -> Result<Value, Refusal> {
        let now = self.now();
        let (lines, host) = self.owner_lines()?;
        let mut inner = self.lock();
        let (grant, client) = Self::session_grant(&inner, id, now)?;
        let digest =
            params_digest(&Self::step_up_bound(&grant, &client)).map_err(|_| internal())?;
        let statement = canonical_json(&json!({
            "host": host,
            "action": ArcaAction::McpAuthorize.as_str(),
            "target": client.id,
            "paramsDigest": digest,
            "issuedAt": iso_utc(now),
            "expiresAt": iso_utc(now.saturating_add(TTL_MAX_MS.min(BEGUN_MAX_MS * 2))),
            "nonce": random_hex(16),
        }))
        .map_err(|_| internal())?;
        let challenge =
            challenge_for(ARCA_NAMESPACE, statement.as_bytes()).map_err(|_| internal())?;
        inner
            .step_ups
            .retain(|_, (_, begun)| now.saturating_sub(*begun) <= BEGUN_MAX_MS);
        inner
            .step_ups
            .insert(grant.id.clone(), (statement.clone(), now));
        Ok(Self::begun(id, &statement, &challenge, &self.rp_id, &lines))
    }

    fn step_up_approve(&self, id: &str, statement: &str, proof: &Value) -> Result<Value, Refusal> {
        let now = self.now();
        let (grant, client, begun) = {
            let inner = self.lock();
            let (grant, client) = Self::session_grant(&inner, id, now)?;
            (grant, client, inner.step_ups.get(id).cloned())
        };
        let Some((begun, begun_at)) = begun else {
            return Err(refused(
                StatusCode::CONFLICT,
                "not_begun",
                "tap Confirm again",
            ));
        };
        if now.saturating_sub(begun_at) > BEGUN_MAX_MS || begun != statement {
            return Err(refused(
                StatusCode::CONFLICT,
                "stale",
                "this confirmation is out of date: tap Confirm again",
            ));
        }
        let key = self.spend(
            &client.id,
            &Self::step_up_bound(&grant, &client),
            statement,
            proof,
            now,
        )?;
        let client_name = {
            let mut inner = self.lock();
            inner.step_ups.remove(id);
            let Some(kept) = inner
                .stored
                .grants
                .iter_mut()
                .find(|kept| kept.id == grant.id && kept.revoked_at.is_none())
            else {
                return Err(gone());
            };
            let before = kept.clone();
            kept.stepped_up_until = now.saturating_add(STEP_UP_MS);
            kept.step_key = Some(key.clone());
            if self.persist(&inner).is_err() {
                if let Some(kept) = inner
                    .stored
                    .grants
                    .iter_mut()
                    .find(|kept| kept.id == grant.id)
                {
                    *kept = before;
                }
                return Err(internal());
            }
            client.name.clone()
        };
        self.journal(
            now,
            "stepped_up",
            json!({ "client": client.id, "name": client_name, "grant": grant.id, "until": iso_utc(now.saturating_add(STEP_UP_MS)), "by": format!("passkey {key}") }),
        );
        Ok(
            json!({ "confirmedUntil": iso_utc(now.saturating_add(STEP_UP_MS)), "name": client_name }),
        )
    }

    /// The owner's passkey lines for this page's relying party and the kernel's host name, or why none can sign.
    fn owner_lines(&self) -> Result<(Vec<arca_approval::PasskeyLine>, String), Refusal> {
        let kernel = self.kernel.lock().map_err(|_| internal())?;
        if let Some(refusal) = kernel.owner_passkey_refusal() {
            return Err(refused(
                StatusCode::SERVICE_UNAVAILABLE,
                "passkeys_refused",
                refusal.to_string(),
            ));
        }
        let lines: Vec<_> = kernel
            .owner_passkey_lines()
            .into_iter()
            .filter(|line| line.rp_id == self.rp_id)
            .collect();
        if lines.is_empty() {
            return Err(refused(
                StatusCode::CONFLICT,
                "no_passkey",
                "no passkey is set up for this page: add one on the approval page first",
            ));
        }
        Ok((lines, kernel.host().to_string()))
    }

    /// What a begin answers: the statement, its WebAuthn challenge and the passkeys that may sign it.
    fn begun(
        id: &str,
        statement: &str,
        challenge: &[u8],
        rp_id: &str,
        lines: &[arca_approval::PasskeyLine],
    ) -> Value {
        json!({
            "id": id,
            "statement": statement,
            "challenge": URL_SAFE_NO_PAD.encode(challenge),
            "rpId": rp_id,
            "allowCredentials": lines
                .iter()
                .map(|line| json!({ "id": URL_SAFE_NO_PAD.encode(&line.credential_id) }))
                .collect::<Vec<_>>(),
            "timeoutMs": BEGUN_MAX_MS,
        })
    }

    /// Checks the owner's passkey proof over `statement` for an `mcp_authorize` of `client` binding `args`, and spends it
    /// (nonce and sign count) with the kernel's own check. Answers a prefix of the passkey's fingerprint.
    fn spend(
        &self,
        client: &str,
        args: &Value,
        statement: &str,
        proof: &Value,
        now: u64,
    ) -> Result<String, Refusal> {
        let proof_bytes = canonical_json(proof)
            .ok()
            .filter(|bytes| parse_proof(bytes.as_bytes()).is_ok())
            .ok_or_else(|| {
                refused(
                    StatusCode::BAD_REQUEST,
                    "bad_proof",
                    "the passkey's answer is not a proof",
                )
            })?;
        let approval = format!(
            "{}.{}",
            STANDARD.encode(statement),
            STANDARD.encode(&proof_bytes)
        );
        let mut kernel = self.kernel.lock().map_err(|_| internal())?;
        let check = kernel
            .preview_approval(ArcaAction::McpAuthorize, client, args, &approval, now)
            .and_then(|spent| {
                kernel
                    .check_approval(ArcaAction::McpAuthorize, client, args, &approval, now)
                    .map(|()| spent)
            });
        match check {
            Ok(spent) => Ok(spent.key.chars().take(24).collect::<String>()),
            Err(refusal) => {
                (self.log)(&format!(
                    "arca: MCP authorization of {client}: the passkey was refused: {}",
                    crate::handlers::refusal_code(&refusal)
                ));
                Err(refused(
                    StatusCode::FORBIDDEN,
                    "assertion_refused",
                    refusal.to_string(),
                ))
            }
        }
    }

    // ---------------------------------------------------------------------------------------------------------------
    // The device authorization grant (RFC 8628)

    /// `POST /oauth/device_authorization`: a consent request for an agent away from the person's browser, checked as a
    /// `GET /oauth/authorize` is, answered with the device code the agent polls with and the user code the person types.
    fn device_authorize(&self, form: &HashMap<String, String>) -> Result<Value, OAuthError> {
        let now = self.now();
        let param = |key: &str| form.get(key).map(String::as_str);
        let mut inner = self.lock();
        Self::sweep(&mut inner, now);
        let client = param("client_id")
            .and_then(|id| Self::client(&inner, id))
            .cloned()
            .ok_or_else(|| {
                OAuthError::new(
                    "invalid_client",
                    "Arca does not know this app: register it first",
                )
            })?;
        let resource = match param("resource") {
            None => self.resource.clone(),
            Some(asked) if canonical_resource(asked) == canonical_resource(&self.resource) => {
                self.resource.clone()
            }
            Some(_) => {
                return Err(OAuthError::new(
                    "invalid_target",
                    format!("Arca issues tokens for {} only", self.resource),
                ));
            }
        };
        let (scope, tools, approvals) = parse_scope(param("scope").unwrap_or_default())
            .map_err(|why| OAuthError::new("invalid_scope", why))?;
        if inner.pending.len() >= PENDING_MAX {
            return Err(OAuthError::new(
                "temporarily_unavailable",
                "too many requests are waiting for the owner",
            )
            .status(StatusCode::TOO_MANY_REQUESTS));
        }
        let user_code = loop {
            let code = user_code();
            if !inner
                .devices
                .values()
                .any(|device| device.user_code == code)
            {
                break code;
            }
        };
        let device_code = secret(DEVICE_CODE_PREFIX);
        let device_hash = hash_hex(&device_code);
        let request = format!("oar-{}", random_hex(16));
        inner.pending.push(Pending {
            id: request.clone(),
            client_id: client.id.clone(),
            redirect_uri: String::new(),
            redirect_given: false,
            state: None,
            scope,
            tools,
            resource,
            challenge: String::new(),
            created: now,
            statement: None,
            begun_at: None,
            device: Some((device_hash.clone(), user_code.clone())),
            approvals,
        });
        inner.devices.insert(
            device_hash,
            Device {
                client_id: client.id,
                user_code: user_code.clone(),
                request,
                expires: now.saturating_add(REQUEST_TTL_MS),
                interval: DEVICE_INTERVAL_MS,
                last_poll: None,
                answer: DeviceAnswer::Waiting,
            },
        );
        let page = format!("{}{DEVICE_PATH}", self.origin);
        Ok(json!({
            "device_code": device_code,
            "user_code": user_code,
            "verification_uri": page,
            "verification_uri_complete": format!("{page}?user_code={user_code}"),
            "expires_in": REQUEST_TTL_MS / 1000,
            "interval": DEVICE_INTERVAL_MS / 1000,
        }))
    }

    /// The consent request a user code opens, as the person typed it (any case, with or without its dash), with its client
    /// and whether its name is shared. `None` for a code nobody issued, one already decided, or one that expired.
    fn device_request(&self, typed: &str) -> Option<(Pending, Client, bool)> {
        let letters: String = typed
            .chars()
            .filter(char::is_ascii_alphabetic)
            .map(|c| c.to_ascii_uppercase())
            .collect();
        if letters.len() != 8 {
            return None;
        }
        let user_code = format!("{}-{}", &letters[..4], &letters[4..]);
        let now = self.now();
        let mut inner = self.lock();
        Self::sweep(&mut inner, now);
        let device = inner.devices.values().find(|device| {
            device.user_code == user_code
                && now < device.expires
                && matches!(device.answer, DeviceAnswer::Waiting)
        })?;
        let pending = inner
            .pending
            .iter()
            .find(|pending| pending.id == device.request)?
            .clone();
        let client = Self::client(&inner, &pending.client_id)?.clone();
        let shared = Self::name_shared(&inner, &client);
        Some((pending, client, shared))
    }

    /// The agent's poll of the token endpoint with its device code (RFC 8628 3.4, 3.5).
    fn device_token(&self, form: &HashMap<String, String>) -> Result<Value, OAuthError> {
        let now = self.now();
        let param = |key: &str| form.get(key).map(String::as_str);
        let device_hash = hash_hex(param("device_code").unwrap_or_default());
        let mut inner = self.lock();
        Self::sweep(&mut inner, now);
        let Some(device) = inner.devices.get_mut(&device_hash) else {
            return Err(OAuthError::new(
                "invalid_grant",
                "the device code is not known, was used already or expired long ago",
            ));
        };
        if param("client_id") != Some(device.client_id.as_str()) {
            return Err(OAuthError::new(
                "invalid_grant",
                "the device code was issued to another client",
            ));
        }
        if now >= device.expires {
            return Err(OAuthError::new(
                "expired_token",
                "the device code expired: ask for a new one",
            ));
        }
        match device.answer.clone() {
            DeviceAnswer::Waiting => {
                if device
                    .last_poll
                    .is_some_and(|last| now < last.saturating_add(device.interval))
                {
                    device.interval = device.interval.saturating_add(SLOW_DOWN_MS);
                    return Err(OAuthError::new(
                        "slow_down",
                        format!("poll every {} seconds at most", device.interval / 1000),
                    ));
                }
                device.last_poll = Some(now);
                Err(OAuthError::new(
                    "authorization_pending",
                    "the owner has not approved yet",
                ))
            }
            DeviceAnswer::Denied => {
                inner.devices.remove(&device_hash);
                Err(OAuthError::new("access_denied", "the owner said no"))
            }
            DeviceAnswer::Approved {
                scope,
                tools,
                resource,
                key,
                approvals,
                approved_at,
            } => {
                let Some(device) = inner.devices.remove(&device_hash) else {
                    return Err(OAuthError::new("invalid_grant", "the device code was used"));
                };
                if Self::client(&inner, &device.client_id).is_none() {
                    return Err(OAuthError::new("invalid_grant", "the client was revoked"));
                }
                match self.issue(
                    &mut inner,
                    &device.client_id,
                    scope,
                    tools,
                    &resource,
                    &key,
                    approvals,
                    approved_at,
                    now,
                ) {
                    Ok((access, refresh, grant)) => Ok(token_answer(&access, &refresh, &grant)),
                    Err(error) => {
                        // Not saved: the agent may poll again for the same answer.
                        inner.devices.insert(device_hash, device);
                        Err(error)
                    }
                }
            }
        }
    }

    // ---------------------------------------------------------------------------------------------------------------
    // Tokens

    fn token(&self, form: &HashMap<String, String>) -> Result<Value, OAuthError> {
        match form.get("grant_type").map(String::as_str) {
            Some("authorization_code") => self.exchange(form),
            Some("refresh_token") => self.refresh(form),
            Some(DEVICE_GRANT) => self.device_token(form),
            _ => Err(OAuthError::new(
                "unsupported_grant_type",
                format!("Arca takes authorization_code, refresh_token and {DEVICE_GRANT}"),
            )),
        }
    }

    /// Makes and saves a new grant of `scope` (and `tools`) for `client_id`, which the owner's passkey `key` approved, with
    /// its first access and refresh tokens.
    #[allow(clippy::too_many_arguments)]
    fn issue(
        &self,
        inner: &mut Inner,
        client_id: &str,
        scope: Scope,
        tools: Option<Vec<String>>,
        resource: &str,
        key: &str,
        approvals: bool,
        approved_at: u64,
        now: u64,
    ) -> Result<(String, String, Grant), OAuthError> {
        let access = secret(ACCESS_PREFIX);
        let refresh = secret(REFRESH_PREFIX);
        let grant = Grant {
            id: format!("g-{}", random_hex(12)),
            client_id: client_id.to_string(),
            scope: scope.as_str().to_string(),
            tools: tools.clone(),
            resource: resource.to_string(),
            approved_at: now,
            key: key.to_string(),
            refresh: Some(Hashed {
                sha256: hash_hex(&refresh),
                expires: now + REFRESH_TTL_MS,
            }),
            rotated: Vec::new(),
            access: vec![Hashed {
                sha256: hash_hex(&access),
                expires: now + ACCESS_TTL_MS,
            }],
            revoked_at: None,
            approved: Some(Approved {
                scope: scope.as_str().to_string(),
                tools,
            }),
            approvals,
            // The consent's passkey counts for the owner's session from the moment it approved.
            stepped_up_until: if approvals {
                approved_at.saturating_add(STEP_UP_MS)
            } else {
                0
            },
            step_key: None,
        };
        inner.stored.grants.push(grant.clone());
        if self.persist(inner).is_err() {
            inner.stored.grants.pop();
            return Err(OAuthError::new("server_error", "the grant was not saved")
                .status(StatusCode::INTERNAL_SERVER_ERROR));
        }
        Ok((access, refresh, grant))
    }

    fn exchange(&self, form: &HashMap<String, String>) -> Result<Value, OAuthError> {
        let now = self.now();
        let param = |key: &str| form.get(key).map(String::as_str);
        let invalid = |why: &str| OAuthError::new("invalid_grant", why);
        let code_hash = hash_hex(param("code").unwrap_or_default());
        let mut inner = self.lock();
        Self::sweep(&mut inner, now);
        let Some(code) = inner.codes.remove(&code_hash) else {
            // A code used a second time takes back what it made (OAuth 2.1, 4.1.3).
            if let Some((grant, _)) = inner.used_codes.remove(&code_hash) {
                let client = revoke_grant(&mut inner, &grant, now);
                let saved = self.persist_revoked(&mut inner, Some(&grant));
                drop(inner);
                self.journal_revoked(
                    now,
                    "code_reused",
                    json!({ "client": client, "grant": grant }),
                    saved,
                );
                if !saved {
                    return Err(not_saved());
                }
            }
            return Err(invalid(
                "the code is not known, was used already or expired",
            ));
        };
        let client_id = param("client_id").unwrap_or_default();
        if client_id != code.client_id {
            return Err(invalid("the code was issued to another client"));
        }
        match param("redirect_uri") {
            Some(uri) if uri != code.redirect_uri => {
                return Err(invalid("redirect_uri is not the one the code was sent to"));
            }
            None if code.redirect_given => {
                return Err(invalid(
                    "redirect_uri is needed: the authorization request named one",
                ));
            }
            _ => {}
        }
        if !pkce_verifier_ok(param("code_verifier").unwrap_or_default(), &code.challenge) {
            return Err(invalid(
                "code_verifier does not match the code_challenge (PKCE S256)",
            ));
        }
        if let Some(asked) = param("resource")
            && canonical_resource(asked) != canonical_resource(&code.resource)
        {
            return Err(OAuthError::new(
                "invalid_target",
                format!("the code is for {}", code.resource),
            ));
        }
        if Self::client(&inner, &code.client_id).is_none() {
            return Err(invalid("the client was revoked"));
        }
        let (access, refresh, grant) = self.issue(
            &mut inner,
            &code.client_id,
            code.scope,
            code.tools.clone(),
            &code.resource,
            &code.key,
            code.approvals,
            code.approved_at,
            now,
        )?;
        inner
            .used_codes
            .insert(code_hash, (grant.id.clone(), code.expires));
        Ok(token_answer(&access, &refresh, &grant))
    }

    fn refresh(&self, form: &HashMap<String, String>) -> Result<Value, OAuthError> {
        let now = self.now();
        let param = |key: &str| form.get(key).map(String::as_str);
        let invalid = |why: &str| OAuthError::new("invalid_grant", why);
        let presented = hash_hex(param("refresh_token").unwrap_or_default());
        let mut inner = self.lock();
        Self::sweep(&mut inner, now);
        let found = inner.stored.grants.iter().position(|grant| {
            grant.revoked_at.is_none()
                && grant
                    .refresh
                    .as_ref()
                    .is_some_and(|token| token.sha256 == presented)
        });
        let Some(index) = found else {
            // A rotated refresh token used again: someone holds a copy, so the grant ends (OAuth 2.1, 4.3.1).
            if let Some(grant) = inner
                .stored
                .grants
                .iter()
                .find(|grant| grant.revoked_at.is_none() && grant.rotated.contains(&presented))
                .map(|grant| grant.id.clone())
            {
                let client = revoke_grant(&mut inner, &grant, now);
                let saved = self.persist_revoked(&mut inner, Some(&grant));
                drop(inner);
                self.journal_revoked(
                    now,
                    "refresh_reused",
                    json!({ "client": client, "grant": grant }),
                    saved,
                );
                if !saved {
                    return Err(not_saved());
                }
            }
            return Err(invalid(
                "the refresh token is not known, was used already or was revoked",
            ));
        };
        let grant = inner.stored.grants[index].clone();
        if param("client_id").is_some_and(|id| id != grant.client_id) {
            return Err(invalid("the refresh token was issued to another client"));
        }
        if grant
            .refresh
            .as_ref()
            .is_none_or(|token| now >= token.expires)
        {
            return Err(invalid("the refresh token expired: ask the owner again"));
        }
        if Self::client(&inner, &grant.client_id).is_none() {
            return Err(invalid("the client was revoked"));
        }
        if let Some(asked) = param("resource")
            && canonical_resource(asked) != canonical_resource(&grant.resource)
        {
            return Err(OAuthError::new(
                "invalid_target",
                format!("the grant is for {}", grant.resource),
            ));
        }
        let (granted, granted_tools) = match &grant.approved {
            Some(approved) => (level(&approved.scope), approved.tools.clone()),
            None => (level(&grant.scope), grant.tools.clone()),
        };
        let granted = granted.ok_or_else(|| invalid("the grant is damaged"))?;
        let (scope, tools) = match param("scope") {
            None => (granted, granted_tools),
            Some(asked) => {
                let (scope, tools, approvals) =
                    parse_scope(asked).map_err(|why| OAuthError::new("invalid_scope", why))?;
                if approvals && !grant.approvals {
                    return Err(OAuthError::new(
                        "invalid_scope",
                        "the owner did not approve approvals; a refresh cannot ask for it",
                    ));
                }
                if scope > granted {
                    return Err(OAuthError::new(
                        "invalid_scope",
                        format!(
                            "the owner approved {}; a refresh cannot ask for {}",
                            granted.as_str(),
                            scope.as_str()
                        ),
                    ));
                }
                let tools = match (&granted_tools, tools) {
                    (None, tools) => tools,
                    (Some(held), None) => Some(held.clone()),
                    (Some(held), Some(asked)) => {
                        if let Some(extra) = asked.iter().find(|tool| !held.contains(tool)) {
                            return Err(OAuthError::new(
                                "invalid_scope",
                                format!("the owner did not approve the tool {extra}"),
                            ));
                        }
                        Some(asked)
                    }
                };
                (scope, tools)
            }
        };
        let access = secret(ACCESS_PREFIX);
        let refresh = secret(REFRESH_PREFIX);
        let before = inner.stored.grants[index].clone();
        let kept = &mut inner.stored.grants[index];
        kept.scope = scope.as_str().to_string();
        kept.tools = tools;
        kept.rotated.push(presented);
        if kept.rotated.len() > ROTATED_MAX {
            kept.rotated.remove(0);
        }
        kept.refresh = Some(Hashed {
            sha256: hash_hex(&refresh),
            expires: now + REFRESH_TTL_MS,
        });
        kept.access.push(Hashed {
            sha256: hash_hex(&access),
            expires: now + ACCESS_TTL_MS,
        });
        let answer = token_answer(&access, &refresh, kept);
        if self.persist(&inner).is_err() {
            inner.stored.grants[index] = before;
            return Err(OAuthError::new("server_error", "the grant was not saved")
                .status(StatusCode::INTERNAL_SERVER_ERROR));
        }
        Ok(answer)
    }

    /// RFC 7009: an access token is dropped alone; a refresh token ends its grant. An unknown token is no error. A
    /// revocation that could not be saved is `503` (RFC 7009 2.2.1), so the client tries again; the retry saves what
    /// the failed one ended, whichever token it presents, and answers `200` only once it is on disk.
    fn revoke_token(&self, form: &HashMap<String, String>) -> Result<(), OAuthError> {
        let now = self.now();
        let presented = hash_hex(form.get("token").map(String::as_str).unwrap_or_default());
        let mut inner = self.lock();
        let owed = inner.unsaved;
        let mut ended = None;
        let mut changed = false;
        for grant in &mut inner.stored.grants {
            let before = grant.access.len();
            grant.access.retain(|token| token.sha256 != presented);
            changed |= grant.access.len() != before;
            if grant
                .refresh
                .as_ref()
                .is_some_and(|token| token.sha256 == presented)
            {
                ended = Some(grant.id.clone());
            }
        }
        if let Some(grant) = &ended {
            let client = revoke_grant(&mut inner, grant, now);
            let saved = self.persist_revoked(&mut inner, Some(grant));
            drop(inner);
            self.journal_revoked(
                now,
                "revoked",
                json!({ "client": client, "grant": grant, "by": "client" }),
                saved,
            );
            if !saved {
                return Err(not_saved());
            }
        } else if (changed || owed) && !self.persist_revoked(&mut inner, None) {
            return Err(not_saved());
        } else if owed {
            drop(inner);
            self.journal(
                now,
                "saved",
                json!({ "what": "a revocation that was not saved before" }),
            );
        }
        Ok(())
    }

    // ---------------------------------------------------------------------------------------------------------------
    // The owner's list

    /// Every client that is not revoked: its name, whether the name is shared, where it is answered, and its live grants.
    pub fn clients(&self) -> Value {
        let now = self.now();
        let mut inner = self.lock();
        Self::sweep(&mut inner, now);
        let list: Vec<Value> = inner
            .stored
            .clients
            .iter()
            .filter(|client| client.revoked_at.is_none())
            .map(|client| {
                let live: Vec<&Grant> = inner
                    .stored
                    .grants
                    .iter()
                    .filter(|grant| grant.client_id == client.id && grant.revoked_at.is_none())
                    .filter(|grant| grant.refresh.as_ref().is_some_and(|token| now < token.expires))
                    .collect();
                // What the app may do is the most any of its live grants allows, and it was last used when any was: a
                // step-up adds a grant beside the first, and the card must not keep showing the first.
                let most = live
                    .iter()
                    .max_by_key(|grant| level(&approved_scope(grant).0).unwrap_or(Scope::Viewer));
                let last_used = live
                    .iter()
                    .filter_map(|grant| inner.last_used.get(&grant.id))
                    .max()
                    .map(|at| iso_utc(*at));
                let grants: Vec<Value> = live
                    .iter()
                    .map(|grant| {
                        let (scope, tools) = approved_scope(grant);
                        json!({
                            "scope": scope_text(level(&scope).unwrap_or(Scope::Viewer), tools.as_deref(), grant.approvals),
                            "approvals": grant.approvals,
                            "approvedAt": iso_utc(grant.approved_at),
                            "lastUsedAt": inner.last_used.get(&grant.id).map(|at| iso_utc(*at)),
                            "expiresAt": grant.refresh.as_ref().map(|token| iso_utc(token.expires)),
                        })
                    })
                    .collect();
                json!({
                    "clientId": client.id,
                    "name": client.name,
                    "scope": most.map(|grant| {
                        let (scope, tools) = approved_scope(grant);
                        scope_text(level(&scope).unwrap_or(Scope::Viewer), tools.as_deref(), false)
                    }),
                    "approvals": live.iter().any(|grant| grant.approvals),
                    "lastUsedAt": last_used,
                    "nameShared": Self::name_shared(&inner, client),
                    "redirectHosts": client.redirect_uris.iter().map(|uri| redirect_host(uri)).collect::<Vec<_>>(),
                    "registeredAt": iso_utc(client.registered_at),
                    "connected": !grants.is_empty(),
                    "grants": grants,
                })
            })
            .collect();
        json!({ "clients": list })
    }

    /// Revokes a client: every grant it holds ends, and it can be approved again only after it registers again.
    pub fn revoke_client(&self, id: &str, by: &str) -> Result<Value, Refusal> {
        let now = self.now();
        let mut inner = self.lock();
        let Some(index) = inner
            .stored
            .clients
            .iter()
            .position(|client| client.id == id && client.revoked_at.is_none())
        else {
            return Err(refused(
                StatusCode::NOT_FOUND,
                "not_found",
                "no such app is connected",
            ));
        };
        let before = inner.stored.clone();
        inner.stored.clients[index].revoked_at = Some(now);
        let grants: Vec<String> = inner
            .stored
            .grants
            .iter()
            .filter(|grant| grant.client_id == id)
            .map(|grant| grant.id.clone())
            .collect();
        for grant in &grants {
            revoke_grant(&mut inner, grant, now);
        }
        inner.pending.retain(|pending| pending.client_id != id);
        inner.codes.retain(|_, code| code.client_id != id);
        if self.persist(&inner).is_err() {
            inner.stored = before;
            return Err(internal());
        }
        let name = inner.stored.clients[index].name.clone();
        drop(inner);
        self.journal(
            now,
            "revoked",
            json!({ "client": id, "name": name, "by": by }),
        );
        (self.log)(&format!("arca: MCP client {id} ({name}) revoked by {by}"));
        Ok(json!({ "revoked": id }))
    }

    // ---------------------------------------------------------------------------------------------------------------
    // Routes

    /// The routes every listener that carries the page carries: metadata, registration, consent, token, revocation and
    /// the owner's list. They keep the page's gate. The consent and list POSTs need the page's exact `Origin`; the
    /// metadata, registration, token and revocation endpoints are a client's, take any `Origin` or none and answer CORS.
    pub fn router(self: &Arc<Self>) -> Router {
        Router::new()
            .route(RESOURCE_METADATA_PATH, get(resource_metadata))
            .route(
                &format!("{RESOURCE_METADATA_PATH}/mcp"),
                get(resource_metadata),
            )
            .route(SERVER_METADATA_PATH, get(server_metadata))
            .route(REGISTER_PATH, post(register))
            .route(TOKEN_PATH, post(token))
            .route(REVOKE_PATH, post(revoke))
            .route(AUTHORIZE_PATH, get(authorize))
            .route(DEVICE_AUTHORIZATION_PATH, post(device_authorization))
            .route(DEVICE_PATH, get(device_page))
            .route("/oauth/authorize/begin", post(consent_begin))
            .route("/oauth/authorize/approve", post(consent_approve))
            .route("/oauth/authorize/deny", post(consent_deny))
            .route(STEP_UP_PATH, get(step_up_page))
            .route("/oauth/step-up/begin", post(step_up_begin))
            .route("/oauth/step-up/approve", post(step_up_approve))
            .route("/oauth/consent.js", get(consent_js))
            .route("/oauth/consent.css", get(consent_css))
            .route("/approve/oauth/clients", get(owner_clients))
            .route("/approve/oauth/revoke", post(owner_revoke))
            .layer(DefaultBodyLimit::max(BODY_MAX))
            .layer(middleware::from_fn_with_state(Arc::clone(self), rules))
            .with_state(Arc::clone(self))
    }

    /// The unix socket's routes for `arca oauth list` and `arca oauth revoke`, behind the setup code's proof file.
    pub fn local_router(self: &Arc<Self>) -> Router {
        Router::new()
            .route(LOCAL_CLIENTS_PATH, post(local_clients))
            .route(LOCAL_REVOKE_PATH, post(local_revoke))
            .layer(DefaultBodyLimit::max(1024))
            .with_state(Arc::clone(self))
    }
}

impl IssuedTokens for OAuth {
    /// A live access token of a live grant for this resource, of a client that is not revoked.
    fn lookup(&self, bearer: &str) -> Option<Caller> {
        let body = bearer.strip_prefix(ACCESS_PREFIX)?;
        if body.len() != SECRET_CHARS
            || !body
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return None;
        }
        let presented = hash_hex(bearer);
        let now = self.now();
        let mut inner = self.lock();
        let grant = inner
            .stored
            .grants
            .iter()
            .find(|grant| {
                grant
                    .access
                    .iter()
                    .any(|token| token.sha256 == presented && now < token.expires)
            })?
            .clone();
        if grant.revoked_at.is_some()
            || canonical_resource(&grant.resource) != canonical_resource(&self.resource)
            || Self::client(&inner, &grant.client_id).is_none()
        {
            return None;
        }
        let scope = level(&grant.scope)?;
        inner.last_used.insert(grant.id.clone(), now);
        Some(Caller {
            token_id: format!("{ISSUED_PREFIX}{}/{}", grant.client_id, grant.id),
            scope,
            tools: grant.tools.clone(),
        })
    }
}

/// The scope and tools the owner approved for `grant`: what it keeps across narrowing refreshes.
fn approved_scope(grant: &Grant) -> (String, Option<Vec<String>>) {
    match &grant.approved {
        Some(approved) => (approved.scope.clone(), approved.tools.clone()),
        None => (grant.scope.clone(), grant.tools.clone()),
    }
}

/// Ends a grant: no token of it works again. Answers the client it belonged to.
fn revoke_grant(inner: &mut Inner, id: &str, now: u64) -> String {
    let mut client = String::new();
    for grant in &mut inner.stored.grants {
        if grant.id == id {
            grant.revoked_at.get_or_insert(now);
            grant.refresh = None;
            grant.access.clear();
            client = grant.client_id.clone();
        }
    }
    client
}

fn token_answer(access: &str, refresh: &str, grant: &Grant) -> Value {
    json!({
        "access_token": access,
        "token_type": "Bearer",
        "expires_in": ACCESS_TTL_MS / 1000,
        "refresh_token": refresh,
        "scope": scope_text(level(&grant.scope).unwrap_or(Scope::Viewer), grant.tools.as_deref(), grant.approvals),
    })
}

/// A revocation that could not be saved: the client should try again.
fn not_saved() -> OAuthError {
    OAuthError::new(
        "temporarily_unavailable",
        "the revocation could not be saved: try again",
    )
    .status(StatusCode::SERVICE_UNAVAILABLE)
}

fn find_pending<'a>(inner: &'a Inner, id: &str) -> Result<&'a Pending, Refusal> {
    inner
        .pending
        .iter()
        .find(|pending| pending.id == id)
        .ok_or_else(gone)
}

fn gone() -> Refusal {
    refused(
        StatusCode::NOT_FOUND,
        "gone",
        "this request is no longer waiting: start again from the app",
    )
}

fn internal() -> Refusal {
    refused(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal",
        "Arca could not do that",
    )
}

// -------------------------------------------------------------------------------------------------------------------
// Scopes, redirects, PKCE and secrets

/// The level of a stored scope word.
fn level(word: &str) -> Option<Scope> {
    LEVELS.into_iter().find(|scope| scope.as_str() == word)
}

/// A scope request: the highest level it names (viewer when none), the tools it names, if any, and whether it names
/// `approvals`. `admin` and an unknown tool are refused; other words are ignored, so a client that adds its own words is
/// not refused for them.
fn parse_scope(text: &str) -> Result<(Scope, Option<Vec<String>>, bool), String> {
    let mut scope = Scope::Viewer;
    let mut tools: Vec<String> = Vec::new();
    let mut approvals = false;
    for word in text.split_ascii_whitespace() {
        if word == APPROVALS {
            approvals = true;
        } else if word == Scope::Admin.as_str() {
            return Err("admin is never granted to an app: ask for operator or less".to_string());
        }
        if let Some(found) = level(word) {
            scope = scope.max(found);
        } else if let Some(tool) = word.strip_prefix("tool:") {
            if !arca_api::verbs().iter().any(|verb| verb.name == tool) {
                return Err(format!("{tool:?} is not an Arca tool"));
            }
            if !tools.iter().any(|kept| kept == tool) {
                tools.push(tool.to_string());
            }
        }
    }
    Ok((scope, (!tools.is_empty()).then_some(tools), approvals))
}

/// Until when a grant with `approvals` is live in `stored` at `now`: the latest refresh expiry among them, or zero.
fn session_until(stored: &Stored, now: u64) -> u64 {
    stored
        .grants
        .iter()
        .filter(|grant| grant.approvals && grant.revoked_at.is_none())
        .filter(|grant| {
            stored
                .clients
                .iter()
                .any(|client| client.id == grant.client_id && client.revoked_at.is_none())
        })
        .filter_map(|grant| grant.refresh.as_ref().map(|token| token.expires))
        .filter(|expires| now < *expires)
        .max()
        .unwrap_or(0)
}

/// A scope as the token answer and the consent page write it.
fn scope_text(scope: Scope, tools: Option<&[String]>, approvals: bool) -> String {
    let mut text = scope.as_str().to_string();
    if approvals {
        text.push(' ');
        text.push_str(APPROVALS);
    }
    for tool in tools.unwrap_or_default() {
        text.push_str(" tool:");
        text.push_str(tool);
    }
    text
}

/// A redirect URI Arca sends a code to: `https://` and a host, or `http://` on a loopback host, at most 512 bytes, no
/// fragment, no user info, no space or control character.
fn redirect_ok(uri: &str) -> bool {
    if uri.len() > REDIRECT_MAX
        || uri.contains('#')
        || uri.bytes().any(|b| b <= b' ' || b == 0x7f || b == b'\\')
    {
        return false;
    }
    let (https, rest) = if let Some(rest) = uri.strip_prefix("https://") {
        (true, rest)
    } else if let Some(rest) = uri.strip_prefix("http://") {
        (false, rest)
    } else {
        return false;
    };
    let authority = rest.split(['/', '?']).next().unwrap_or_default();
    if authority.is_empty() || authority.contains('@') {
        return false;
    }
    let host = host_of(authority);
    if host.is_empty() {
        return false;
    }
    https
        || matches!(
            host.to_ascii_lowercase().as_str(),
            "127.0.0.1" | "[::1]" | "localhost"
        )
}

/// The host of an authority, without its port.
fn host_of(authority: &str) -> &str {
    if authority.starts_with('[') {
        return authority.find(']').map_or("", |end| &authority[..=end]);
    }
    authority.split(':').next().unwrap_or_default()
}

/// The host a redirect URI sends to, as the owner is shown it.
fn redirect_host(uri: &str) -> String {
    let rest = uri
        .strip_prefix("https://")
        .or_else(|| uri.strip_prefix("http://"))
        .unwrap_or(uri);
    let authority = rest.split(['/', '?']).next().unwrap_or_default();
    host_of(authority).to_ascii_lowercase()
}

/// A resource as RFC 8707 and the MCP spec compare it: the scheme and host in lowercase, no trailing slash.
fn canonical_resource(uri: &str) -> String {
    let Some((scheme, rest)) = uri.split_once("://") else {
        return uri.to_string();
    };
    let (authority, path) = rest.find('/').map_or((rest, ""), |at| rest.split_at(at));
    format!(
        "{}://{}{}",
        scheme.to_ascii_lowercase(),
        authority.to_ascii_lowercase(),
        path.trim_end_matches('/')
    )
}

/// An S256 challenge: 43 base64url characters, the encoding of a SHA-256.
fn pkce_challenge_ok(challenge: &str) -> bool {
    challenge.len() == 43
        && challenge
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// RFC 7636: a verifier of 43 to 128 unreserved characters whose SHA-256, base64url, is the challenge. The comparison
/// does not stop at the first differing byte.
fn pkce_verifier_ok(verifier: &str, challenge: &str) -> bool {
    let shaped = (43..=128).contains(&verifier.len())
        && verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'));
    let computed = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let same = computed.len() == challenge.len()
        && computed
            .bytes()
            .zip(challenge.bytes())
            .fold(0u8, |diff, (a, b)| diff | (a ^ b))
            == 0;
    shaped && std::hint::black_box(same)
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).expect("the operating system's random source is available");
    bytes
}

fn random_hex(bytes: usize) -> String {
    random_bytes::<32>()[..bytes.min(32)]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A new secret: `prefix` and 43 base64url characters of 32 random bytes.
/// A new user code: eight of [`USER_CODE_LETTERS`], each from an unbiased random byte, shown as `XXXX-XXXX`.
fn user_code() -> String {
    let mut letters = Vec::with_capacity(8);
    while letters.len() < 8 {
        for byte in random_bytes::<16>() {
            // 240 is the largest multiple of 20 below 256: a byte at or above it would favour the first letters.
            if byte < 240 && letters.len() < 8 {
                letters.push(USER_CODE_LETTERS[usize::from(byte % 20)]);
            }
        }
    }
    let text = String::from_utf8(letters).unwrap_or_default();
    format!("{}-{}", &text[..4], &text[4..])
}

fn secret(prefix: &str) -> String {
    format!("{prefix}{}", URL_SAFE_NO_PAD.encode(random_bytes::<32>()))
}

fn hash_hex(secret: &str) -> String {
    Sha256::digest(secret.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A client name as the owner is shown it: no control or bidi character, at most 80 characters, trimmed.
fn clean_name(name: &str) -> String {
    name.chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(*c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .take(NAME_MAX)
        .collect::<String>()
        .trim()
        .to_string()
}

/// `uri` with query parameters added, each percent-encoded; `None` values are left out.
fn with_params(uri: &str, params: &[(&str, Option<&str>)]) -> String {
    let mut out = uri.to_string();
    let mut joiner = if uri.contains('?') { '&' } else { '?' };
    for (key, value) in params {
        let Some(value) = value else { continue };
        out.push(joiner);
        out.push_str(key);
        out.push('=');
        out.push_str(&percent_encode(value));
        joiner = '&';
    }
    out
}

fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// The store file, when it is a private regular file that parses; none at all is an empty store.
fn read_store(path: &Path) -> io::Result<Stored> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Stored::default()),
        Err(error) => return Err(error),
    };
    if !meta.is_file() || meta.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::other(format!(
            "{} is not a private regular file (mode 0600)",
            path.display()
        )));
    }
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(|_| {
        io::Error::other(format!(
            "{} does not parse as Arca's MCP authorization store",
            path.display()
        ))
    })
}

// -------------------------------------------------------------------------------------------------------------------
// HTTP

/// The rules in front of every route: the page's gate, the `Origin` rule, CORS for a client's endpoints, and the headers
/// that keep a response out of caches, frames and referrers.
async fn rules(State(oauth): State<Arc<OAuth>>, request: Request, next: Next) -> Response {
    if let Some(why) = oauth.gate.as_ref().and_then(|gate| gate()) {
        return refused(
            StatusCode::SERVICE_UNAVAILABLE,
            crate::approve_page::PAGE_CLOSED,
            why,
        )
        .into_response();
    }
    let path = request.uri().path().to_string();
    let for_clients = path.starts_with("/.well-known/")
        || [
            REGISTER_PATH,
            TOKEN_PATH,
            REVOKE_PATH,
            DEVICE_AUTHORIZATION_PATH,
        ]
        .contains(&path.as_str());
    if for_clients && request.method() == Method::OPTIONS {
        let mut response = StatusCode::NO_CONTENT.into_response();
        cors(response.headers_mut());
        return response;
    }
    if !for_clients {
        let origin = request
            .headers()
            .get(header::ORIGIN)
            .map(|value| value.to_str().ok());
        let exact = origin.is_some_and(|value| value == Some(oauth.origin.as_str()));
        let allowed = if request.method() == Method::POST {
            exact
        } else {
            exact || origin.is_none()
        };
        if !allowed {
            return StatusCode::FORBIDDEN.into_response();
        }
    }
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    if for_clients {
        cors(headers);
    }
    headers.insert(
        HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static(if path == DEVICE_PATH { DEVICE_CSP } else { CSP }),
    );
    headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        HeaderName::from_static("permissions-policy"),
        HeaderValue::from_static("publickey-credentials-get=(self)"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn cors(headers: &mut HeaderMap) {
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("authorization, content-type, mcp-protocol-version"),
    );
}

async fn resource_metadata(State(oauth): State<Arc<OAuth>>) -> Response {
    Json(json!({
        "resource": oauth.resource,
        "authorization_servers": [oauth.origin],
        "scopes_supported": LEVELS.iter().map(|scope| scope.as_str()).chain([APPROVALS]).collect::<Vec<_>>(),
        "bearer_methods_supported": ["header"],
        "resource_name": "Arca",
    }))
    .into_response()
}

async fn server_metadata(State(oauth): State<Arc<OAuth>>) -> Response {
    let origin = &oauth.origin;
    Json(json!({
        "issuer": origin,
        "authorization_endpoint": format!("{origin}{AUTHORIZE_PATH}"),
        "token_endpoint": format!("{origin}{TOKEN_PATH}"),
        "registration_endpoint": format!("{origin}{REGISTER_PATH}"),
        "revocation_endpoint": format!("{origin}{REVOKE_PATH}"),
        "device_authorization_endpoint": format!("{origin}{DEVICE_AUTHORIZATION_PATH}"),
        "response_types_supported": ["code"],
        "response_modes_supported": ["query"],
        "grant_types_supported": ["authorization_code", "refresh_token", DEVICE_GRANT],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "revocation_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": LEVELS.iter().map(|scope| scope.as_str()).chain([APPROVALS]).collect::<Vec<_>>(),
        "authorization_response_iss_parameter_supported": true,
    }))
    .into_response()
}

async fn register(State(oauth): State<Arc<OAuth>>, body: Bytes) -> Response {
    let outcome = tokio::task::spawn_blocking(move || oauth.register(&body)).await;
    match outcome {
        Ok(Ok(answer)) => (StatusCode::CREATED, Json(answer)).into_response(),
        Ok(Err(error)) => error.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Reads an `application/x-www-form-urlencoded` body. A key given twice is refused, as RFC 6749 asks.
fn form(body: &[u8]) -> Result<HashMap<String, String>, OAuthError> {
    let pairs: Vec<(String, String)> = serde_urlencoded::from_bytes(body)
        .map_err(|_| OAuthError::new("invalid_request", "the body is not a form"))?;
    let mut map = HashMap::new();
    for (key, value) in pairs {
        if map.insert(key, value).is_some() {
            return Err(OAuthError::new(
                "invalid_request",
                "a parameter is given twice",
            ));
        }
    }
    Ok(map)
}

async fn token(State(oauth): State<Arc<OAuth>>, body: Bytes) -> Response {
    let outcome =
        tokio::task::spawn_blocking(move || form(&body).and_then(|form| oauth.token(&form))).await;
    match outcome {
        Ok(Ok(answer)) => (
            StatusCode::OK,
            [
                (header::CACHE_CONTROL, "no-store"),
                (header::PRAGMA, "no-cache"),
            ],
            Json(answer),
        )
            .into_response(),
        Ok(Err(error)) => error.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn device_authorization(State(oauth): State<Arc<OAuth>>, body: Bytes) -> Response {
    let outcome = tokio::task::spawn_blocking(move || {
        form(&body).and_then(|form| oauth.device_authorize(&form))
    })
    .await;
    match outcome {
        Ok(Ok(answer)) => (
            StatusCode::OK,
            [
                (header::CACHE_CONTROL, "no-store"),
                (header::PRAGMA, "no-cache"),
            ],
            Json(answer),
        )
            .into_response(),
        Ok(Err(error)) => error.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// `GET /oauth/device`: with no code, the page where the person types the one the app shows; with `user_code`, the
/// consent page for it.
async fn device_page(
    State(oauth): State<Arc<OAuth>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let Some(typed) = query
        .get("user_code")
        .filter(|typed| !typed.trim().is_empty())
    else {
        return html(StatusCode::OK, device_entry_page(None));
    };
    match oauth.device_request(typed) {
        Some((pending, client, shared)) => html(
            StatusCode::OK,
            consent_page(&oauth, &pending, &client, shared),
        ),
        None => html(
            StatusCode::NOT_FOUND,
            device_entry_page(Some(
                "Arca does not know this code, or it expired. Check it, or ask the app for a new one.",
            )),
        ),
    }
}

async fn revoke(State(oauth): State<Arc<OAuth>>, body: Bytes) -> Response {
    let outcome =
        tokio::task::spawn_blocking(move || form(&body).and_then(|form| oauth.revoke_token(&form)))
            .await;
    match outcome {
        Ok(Ok(())) => StatusCode::OK.into_response(),
        Ok(Err(error)) => error.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn authorize(
    State(oauth): State<Arc<OAuth>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    match oauth.authorize(&query) {
        Ok((pending, client, shared)) => html(
            StatusCode::OK,
            consent_page(&oauth, &pending, &client, shared),
        ),
        Err(AuthorizeFail::Redirect { uri, state, error }) => {
            let target = with_params(
                &uri,
                &[
                    ("error", Some(error.code)),
                    ("error_description", Some(&error.description)),
                    ("state", state.as_deref()),
                    ("iss", Some(oauth.origin.as_str())),
                ],
            );
            match HeaderValue::from_str(&target) {
                Ok(location) => (StatusCode::FOUND, [(header::LOCATION, location)]).into_response(),
                Err(_) => StatusCode::BAD_REQUEST.into_response(),
            }
        }
        Err(AuthorizeFail::Page(error)) => html(
            StatusCode::BAD_REQUEST,
            notice_page("This app can't connect", &error.description),
        ),
    }
}

/// The page's JSON body, `{id, ...}`.
fn page_body(body: &[u8]) -> Result<Value, Refusal> {
    serde_json::from_slice::<Value>(body)
        .ok()
        .filter(|value| value.get("id").and_then(Value::as_str).is_some())
        .ok_or_else(|| Refusal::bare(StatusCode::BAD_REQUEST))
}

async fn consent_begin(State(oauth): State<Arc<OAuth>>, body: Bytes) -> Response {
    let outcome = tokio::task::spawn_blocking(move || {
        let args = page_body(&body)?;
        oauth.begin(args["id"].as_str().unwrap_or_default())
    })
    .await;
    answer(outcome)
}

async fn consent_approve(State(oauth): State<Arc<OAuth>>, body: Bytes) -> Response {
    let outcome = tokio::task::spawn_blocking(move || {
        let args = page_body(&body)?;
        let (Some(statement), Some(proof)) = (args["statement"].as_str(), args.get("proof")) else {
            return Err(Refusal::bare(StatusCode::BAD_REQUEST));
        };
        oauth.approve(args["id"].as_str().unwrap_or_default(), statement, proof)
    })
    .await;
    answer(outcome)
}

async fn consent_deny(State(oauth): State<Arc<OAuth>>, body: Bytes) -> Response {
    let outcome = tokio::task::spawn_blocking(move || {
        let args = page_body(&body)?;
        oauth.deny(args["id"].as_str().unwrap_or_default())
    })
    .await;
    answer(outcome)
}

async fn step_up_page(
    State(oauth): State<Arc<OAuth>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let now = oauth.now();
    let id = query.get("grant").map(String::as_str).unwrap_or_default();
    let found = {
        let inner = oauth.lock();
        OAuth::session_grant(&inner, id, now)
    };
    match found {
        Ok((grant, client)) => html(StatusCode::OK, step_up_page_html(&grant, &client)),
        Err(refusal) => html(
            StatusCode::NOT_FOUND,
            notice_page("Nothing to confirm", &refusal.message),
        ),
    }
}

async fn step_up_begin(State(oauth): State<Arc<OAuth>>, body: Bytes) -> Response {
    let outcome = tokio::task::spawn_blocking(move || {
        let args = page_body(&body)?;
        oauth.step_up_begin(args["id"].as_str().unwrap_or_default())
    })
    .await;
    answer(outcome)
}

async fn step_up_approve(State(oauth): State<Arc<OAuth>>, body: Bytes) -> Response {
    let outcome = tokio::task::spawn_blocking(move || {
        let args = page_body(&body)?;
        let (Some(statement), Some(proof)) = (args["statement"].as_str(), args.get("proof")) else {
            return Err(Refusal::bare(StatusCode::BAD_REQUEST));
        };
        oauth.step_up_approve(args["id"].as_str().unwrap_or_default(), statement, proof)
    })
    .await;
    answer(outcome)
}

fn answer(outcome: Result<Result<Value, Refusal>, tokio::task::JoinError>) -> Response {
    match outcome {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(refusal)) => refusal.into_response(),
        Err(_) => internal().into_response(),
    }
}

async fn owner_clients(State(oauth): State<Arc<OAuth>>) -> Response {
    let outcome = tokio::task::spawn_blocking(move || Ok(oauth.clients())).await;
    answer(outcome)
}

async fn owner_revoke(State(oauth): State<Arc<OAuth>>, body: Bytes) -> Response {
    let outcome = tokio::task::spawn_blocking(move || {
        let args: Value =
            serde_json::from_slice(&body).map_err(|_| Refusal::bare(StatusCode::BAD_REQUEST))?;
        let id = args["clientId"]
            .as_str()
            .ok_or_else(|| Refusal::bare(StatusCode::BAD_REQUEST))?;
        oauth.revoke_client(id, "page")
    })
    .await;
    answer(outcome)
}

/// The proof a terminal's request carries: the random part of the file it made in the state directory.
fn local_proof(oauth: &OAuth, args: &Value) -> Result<(), Refusal> {
    let proof = args["proof"].as_str().unwrap_or_default();
    let shaped = proof.len() == crate::approve_page::LOCAL_PROOF_LEN
        && proof
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if shaped && crate::approve_page::take_local_proof(&oauth.state_dir, proof) {
        Ok(())
    } else {
        Err(refused(
            StatusCode::FORBIDDEN,
            "no_proof",
            "run this as the user who runs the node",
        ))
    }
}

async fn local_clients(State(oauth): State<Arc<OAuth>>, body: Bytes) -> Response {
    let outcome = tokio::task::spawn_blocking(move || {
        let args: Value =
            serde_json::from_slice(&body).map_err(|_| Refusal::bare(StatusCode::BAD_REQUEST))?;
        local_proof(&oauth, &args)?;
        Ok(oauth.clients())
    })
    .await;
    answer(outcome)
}

async fn local_revoke(State(oauth): State<Arc<OAuth>>, body: Bytes) -> Response {
    let outcome = tokio::task::spawn_blocking(move || {
        let args: Value =
            serde_json::from_slice(&body).map_err(|_| Refusal::bare(StatusCode::BAD_REQUEST))?;
        local_proof(&oauth, &args)?;
        let id = args["clientId"]
            .as_str()
            .ok_or_else(|| Refusal::bare(StatusCode::BAD_REQUEST))?;
        oauth.revoke_client(id, "terminal")
    })
    .await;
    answer(outcome)
}

async fn consent_js() -> Response {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        CONSENT_JS,
    )
        .into_response()
}

async fn consent_css() -> Response {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        CONSENT_CSS,
    )
        .into_response()
}

fn html(status: StatusCode, body: String) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        body,
    )
        .into_response()
}

/// What each scope lets an app do, in the owner's words.
fn scope_words(scope: Scope) -> &'static str {
    match scope {
        Scope::Viewer => {
            "Look: read what runs on this machine, its health and its history. It changes nothing."
        }
        Scope::Doctor => {
            "Look and act: also create, change, start and stop services, data and machines. Anything that erases still waits for your passkey."
        }
        Scope::Operator | Scope::Admin => {
            "Everything a doctor does, and also list this machine's tokens and settle waiting plans."
        }
    }
}

fn consent_page(oauth: &OAuth, pending: &Pending, client: &Client, shared: bool) -> String {
    let answers = match &pending.device {
        // An agent on another device polls for its answer: the person matches the code it shows.
        Some((_, user_code)) => format!(
            "<dt>Code</dt><dd><strong id=\"user-code\">{code}</strong>: an app on another device, which shows this same code</dd>",
            code = escape_html(user_code),
        ),
        None => {
            let host = redirect_host(&pending.redirect_uri);
            let local = matches!(host.as_str(), "127.0.0.1" | "[::1]" | "localhost");
            let where_text = if local {
                "an app on the computer you are using".to_string()
            } else {
                host.clone()
            };
            format!(
                "<dt>Answers go to</dt><dd><strong id=\"redirect-host\">{host}</strong> ({where_text})</dd>",
                host = escape_html(&host),
                where_text = escape_html(&where_text),
            )
        }
    };
    let device_note = if pending.device.is_some() {
        "<p class=\"warn\" id=\"device-note\">Approve only if you started this yourself, just now, and the app shows this same code. Someone who sent you a code is asking for your access.</p>"
    } else {
        ""
    };
    let shared_note = if shared {
        "<p class=\"warn\" id=\"name-shared\">Another app registered with this same name. Approve only if you just added Arca to this app yourself.</p>"
    } else {
        ""
    };
    let approvals = if pending.approvals {
        "<div class=\"approvals\" id=\"approvals\"><p><strong>It also asks to be your signed-in session for approvals.</strong> \
When a plan waits for you, whoever made it, this app asks you yes or no in its own window, and yes runs it. \
Your passkey here counts for 12 hours; after that the app sends you to Arca's page to confirm it is you with one tap. \
Plans that delete or erase still wait for your passkey on the approval page. While this app is connected, waiting plans \
stay open for it instead of lapsing after 15 minutes.</p></div>"
    } else {
        ""
    };
    let tools = pending
        .tools
        .as_ref()
        .map(|tools| {
            format!(
                "<p class=\"tools\">Only these tools: <code>{}</code></p>",
                escape_html(&tools.join(", "))
            )
        })
        .unwrap_or_default();
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<title>Connect an app to Arca</title><link rel=\"stylesheet\" href=\"/oauth/consent.css\"></head>\
<body><main data-request=\"{id}\" data-begin=\"/oauth/authorize/begin\" data-approve=\"/oauth/authorize/approve\">\
<h1>Connect <span id=\"client-name\">{name}</span> to Arca?</h1>{shared_note}{device_note}\
<dl><dt>App</dt><dd><strong>{name}</strong></dd>{answers}\
<dt>It asks to</dt><dd><strong id=\"scope\">{scope}</strong>: {words}</dd>\
<dt>For</dt><dd>{resource}</dd></dl>{tools}{approvals}\
<p>Your passkey signs you in and approves this in one step. You can disconnect the app any time on the approval page.</p>\
<div class=\"actions\"><button id=\"approve\" type=\"button\">Approve with passkey</button>\
<button id=\"deny\" type=\"button\" class=\"secondary\">Don't allow</button></div>\
<p id=\"status\" role=\"status\"></p></main><script src=\"/oauth/consent.js\"></script></body></html>\n",
        id = escape_html(&pending.id),
        name = escape_html(&client.name),
        scope = scope_text(pending.scope, None, pending.approvals),
        words = scope_words(pending.scope),
        resource = escape_html(&oauth.resource),
    )
}

/// The step-up page: one passkey tap confirms the owner's signed-in session in `client` for the next 12 hours.
fn step_up_page_html(grant: &Grant, client: &Client) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<title>Confirm it is you</title><link rel=\"stylesheet\" href=\"/oauth/consent.css\"></head>\
<body><main data-request=\"{id}\" data-begin=\"/oauth/step-up/begin\" data-approve=\"/oauth/step-up/approve\">\
<h1>Confirm it is you in <span id=\"client-name\">{name}</span></h1>\
<p>Plans are waiting for your approval. One tap of your passkey lets <strong>{name}</strong> ask you about them, \
yes or no, for the next 12 hours. Plans that delete or erase still wait for your passkey on the approval page.</p>\
<div class=\"actions\"><button id=\"approve\" type=\"button\">Confirm with passkey</button></div>\
<p id=\"status\" role=\"status\"></p></main><script src=\"/oauth/consent.js\"></script></body></html>\n",
        id = escape_html(&grant.id),
        name = escape_html(&client.name),
    )
}

/// The page where the person types the code an app on another device shows, with `problem` above it when the last one
/// was not known.
fn device_entry_page(problem: Option<&str>) -> String {
    let problem = problem
        .map(|text| format!("<p class=\"warn\" id=\"problem\">{}</p>", escape_html(text)))
        .unwrap_or_default();
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<title>Connect an app to Arca</title><link rel=\"stylesheet\" href=\"/oauth/consent.css\"></head>\
<body><main><h1>Connect an app to Arca</h1>{problem}\
<p>Type the code the app shows. It looks like <code>BCDF-GHJK</code>.</p>\
<form method=\"get\" action=\"{DEVICE_PATH}\"><label for=\"user_code\">Code</label>\
<input id=\"user_code\" name=\"user_code\" autocomplete=\"off\" autocapitalize=\"characters\" spellcheck=\"false\" required>\
<div class=\"actions\"><button type=\"submit\">Continue</button></div></form></main></body></html>\n"
    )
}

fn notice_page(title: &str, message: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<title>{title}</title><link rel=\"stylesheet\" href=\"/oauth/consent.css\"></head><body><main><h1>{title}</h1><p>{message}</p></main></body></html>\n",
        title = escape_html(title),
        message = escape_html(message),
    )
}

const CONSENT_CSS: &str = include_str!("mcp_oauth_consent.css");
const CONSENT_JS: &str = include_str!("mcp_oauth_consent.js");
