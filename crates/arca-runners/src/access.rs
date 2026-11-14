//! The seam between the runner pools and whoever holds the GitHub credential.
//!
//! In the kernel the signer implements [`Access`]: it holds the GitHub App's key and its installation tokens, derives
//! the Actions service connection, and mints every just-in-time runner configuration with an audit record. Nothing in
//! this crate ever sees the key or an installation token: the listener gets the one-hour Actions service connection,
//! and a launcher gets one runner configuration.
//!
//! [`RegistrationAccess`] is the same seam over a token handed to it: the tests drive it against the fake GitHub, and
//! it is the reference for the two calls that turn an installation token into an Actions service connection.

use std::future::Future;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Deserialize;
use serde_json::json;

use crate::scaleset::{API_VERSION, Auth, Http, Reply, SCALE_SET_PATH};
use crate::secret::Secret;
use crate::signin::SignInReason;

/// The Actions service address and the admin token of about one hour that the scale set calls take.
#[derive(Debug, Clone)]
pub struct ActionsConnection {
    pub service_url: String,
    pub admin_token: Secret,
    /// When the admin token expires, in epoch milliseconds.
    pub expires_at_ms: u64,
}

/// Why the access could not give what was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessError {
    /// The binding has no credential in the kernel.
    NoCredential,
    /// GitHub refused the credential.
    SignIn { reason: SignInReason, status: u16 },
    /// The signer refused the request itself: an unknown binding, labels it does not allow, a revoked or suspended
    /// binding.
    Refused(String),
    /// A push-mode binding has no runner configuration queued.
    Empty,
    /// GitHub answered with another failure; `retry` when trying again later may work.
    GitHub { status: u16, message: String },
    /// GitHub could not be reached.
    Network(String),
}

impl AccessError {
    /// The sign-in failure this is, if it is one.
    pub fn signin(&self) -> Option<SignInReason> {
        match self {
            Self::NoCredential => Some(SignInReason::NoCredential),
            Self::SignIn { reason, .. } => Some(*reason),
            _ => None,
        }
    }
}

impl std::fmt::Display for AccessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoCredential => f.write_str("the pool's binding has no GitHub credential"),
            Self::SignIn { reason, status } => {
                write!(
                    f,
                    "GitHub refused the credential ({}, HTTP {status})",
                    reason.as_str()
                )
            }
            Self::Refused(why) => write!(f, "the signer refused: {why}"),
            Self::Empty => f.write_str("no runner configuration is queued"),
            Self::GitHub { status, message } => write!(f, "GitHub answered {status}: {message}"),
            Self::Network(why) => write!(f, "GitHub could not be reached: {why}"),
        }
    }
}

/// One runner configuration to mint, in a scale set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MintRequest {
    pub binding: String,
    pub scale_set_id: u64,
    pub runner_name: String,
    /// The scale set's labels: a signer checks them against the labels the binding allows.
    pub labels: Vec<String>,
    pub work_folder: String,
    /// Who asks, for the audit record.
    pub caller: String,
}

/// A minted runner configuration.
#[derive(Debug, Clone)]
pub struct Minted {
    pub mint_id: u64,
    pub runner_id: u64,
    pub runner_name: String,
    pub encoded_jit_config: Secret,
}

/// The job a minted runner took, for the audit record.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct JobRef {
    pub repo: String,
    pub run_id: Option<u64>,
    pub job_id: Option<String>,
    pub job_name: Option<String>,
}

/// Whoever holds the GitHub credential of a pool's binding.
pub trait Access: Send + Sync + 'static {
    /// The Actions service connection of the binding. A caller keeps it until shortly before it expires.
    fn actions_connection(
        &self,
        binding: &str,
    ) -> impl Future<Output = Result<ActionsConnection, AccessError>> + Send;

    /// One just-in-time runner configuration in a scale set.
    fn mint(
        &self,
        request: MintRequest,
    ) -> impl Future<Output = Result<Minted, AccessError>> + Send;

    /// Records the job a minted runner took.
    fn attach_job(&self, mint_id: u64, job: JobRef) -> impl Future<Output = ()> + Send;
}

/// [`Access`] over an installation token handed to it: it makes the registration token and the Actions service
/// connection itself, and mints with that connection.
pub struct RegistrationAccess {
    http: Http,
    api_url: String,
    org_url: String,
    org: String,
    token: Box<dyn Fn() -> Option<Secret> + Send + Sync>,
    cached: Mutex<Option<ActionsConnection>>,
    mints: AtomicU64,
}

#[derive(Deserialize)]
struct RegistrationToken {
    token: Option<Secret>,
}

#[derive(Deserialize)]
struct AdminConnection {
    url: Option<String>,
    token: Option<Secret>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JitAnswer {
    runner: Option<JitRunner>,
    #[serde(rename = "encodedJITConfig")]
    encoded_jit_config: Option<Secret>,
}

#[derive(Deserialize)]
struct JitRunner {
    id: u64,
    name: String,
}

impl RegistrationAccess {
    /// `api_url` is GitHub's REST address (`https://api.github.com`), `org_url` the organisation's
    /// (`https://github.com/acme`), `token` an installation token (or a token with the same permission).
    pub fn new(http: Http, api_url: &str, org_url: &str, token: Secret) -> Result<Self, String> {
        Self::with_source(http, api_url, org_url, move || Some(token.clone()))
    }

    /// The same, with the token read from `token` at each sign-in: `None` when there is none.
    pub fn with_source(
        http: Http,
        api_url: &str,
        org_url: &str,
        token: impl Fn() -> Option<Secret> + Send + Sync + 'static,
    ) -> Result<Self, String> {
        let org = org_url
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .filter(|org| !org.is_empty() && !org.contains(':'))
            .ok_or("the organisation's address names no organisation")?
            .to_string();
        Ok(Self {
            http,
            api_url: api_url.trim_end_matches('/').to_string(),
            org_url: org_url.trim_end_matches('/').to_string(),
            org,
            token: Box::new(token),
            cached: Mutex::new(None),
            mints: AtomicU64::new(0),
        })
    }

    async fn connect(&self) -> Result<ActionsConnection, AccessError> {
        if let Some(connection) = self.cached.lock().ok().and_then(|cached| cached.clone())
            && connection.expires_at_ms > crate::time::now_ms() + 5 * 60 * 1000
        {
            return Ok(connection);
        }
        let token = (self.token)()
            .filter(|token| !token.is_empty())
            .ok_or(AccessError::NoCredential)?;
        let url = format!(
            "{}/orgs/{}/actions/runners/registration-token",
            self.api_url, self.org
        );
        let reply = self
            .http
            .send("POST", &url, Auth::Bearer(&token), None, &[])
            .await
            .map_err(|error| AccessError::Network(error.to_string()))?;
        let registration: RegistrationToken = signin_checked(&reply, 201, true)?;
        let registration = registration
            .token
            .filter(|token| !token.is_empty())
            .ok_or_else(|| decode("the registration token answer has no token"))?;
        let url = format!("{}/actions/runner-registration", self.api_url);
        let body = json!({ "url": self.org_url, "runner_event": "register" });
        let reply = self
            .http
            .send(
                "POST",
                &url,
                Auth::RemoteAuth(&registration),
                Some(&body),
                &[],
            )
            .await
            .map_err(|error| AccessError::Network(error.to_string()))?;
        let admin: AdminConnection = signin_checked(&reply, 200, false)?;
        let (Some(service_url), Some(admin_token)) = (admin.url, admin.token) else {
            return Err(decode("the Actions service connection has no url or token"));
        };
        let expires_at_ms = jwt_expiry_ms(admin_token.expose())
            .ok_or_else(|| decode("the Actions service token carries no expiry"))?;
        let connection = ActionsConnection {
            service_url,
            admin_token,
            expires_at_ms,
        };
        if let Ok(mut cached) = self.cached.lock() {
            *cached = Some(connection.clone());
        }
        Ok(connection)
    }
}

impl Access for RegistrationAccess {
    async fn actions_connection(&self, _binding: &str) -> Result<ActionsConnection, AccessError> {
        self.connect().await
    }

    async fn mint(&self, request: MintRequest) -> Result<Minted, AccessError> {
        let connection = self.connect().await?;
        let url = format!(
            "{}{SCALE_SET_PATH}/{}/generatejitconfig?api-version={API_VERSION}",
            connection.service_url.trim_end_matches('/'),
            request.scale_set_id
        );
        let body = json!({ "name": request.runner_name, "workFolder": request.work_folder });
        let reply = self
            .http
            .send(
                "POST",
                &url,
                Auth::Bearer(&connection.admin_token),
                Some(&body),
                &[],
            )
            .await
            .map_err(|error| AccessError::Network(error.to_string()))?;
        if reply.status == 401
            && let Ok(mut cached) = self.cached.lock()
        {
            *cached = None;
        }
        let answer: JitAnswer = signin_checked(&reply, 200, false)?;
        let (Some(runner), Some(config)) = (answer.runner, answer.encoded_jit_config) else {
            return Err(decode("the runner configuration answer is incomplete"));
        };
        Ok(Minted {
            mint_id: self.mints.fetch_add(1, Ordering::Relaxed) + 1,
            runner_id: runner.id,
            runner_name: runner.name,
            encoded_jit_config: config,
        })
    }

    async fn attach_job(&self, _mint_id: u64, _job: JobRef) {}
}

/// The answer's body as `T` when its status is `want`. A credential failure is a sign-in error: 401 is a bad
/// credential, 403 a missing permission, and a 404 from the organisation's registration token means the App is not
/// installed there (`installation_call`).
fn signin_checked<T: serde::de::DeserializeOwned>(
    reply: &Reply,
    want: u16,
    installation_call: bool,
) -> Result<T, AccessError> {
    match reply.status {
        status if status == want || (200..300).contains(&status) => {
            serde_json::from_slice(crate::scaleset::trim_bom(&reply.body))
                .map_err(|_| decode("GitHub's answer is not the JSON expected"))
        }
        401 => Err(AccessError::SignIn {
            reason: SignInReason::BadCredentials,
            status: 401,
        }),
        403 => Err(AccessError::SignIn {
            reason: SignInReason::Forbidden,
            status: 403,
        }),
        404 if installation_call => Err(AccessError::SignIn {
            reason: SignInReason::AppNotInstalled,
            status: 404,
        }),
        status => Err(AccessError::GitHub {
            status,
            message: reply.message(),
        }),
    }
}

fn decode(why: &str) -> AccessError {
    AccessError::GitHub {
        status: 0,
        message: why.to_string(),
    }
}

/// The `exp` claim of a JWT, in epoch milliseconds. The token is read, never checked: it is GitHub's own, used only to
/// know when to ask for the next one.
pub fn jwt_expiry_ms(token: &str) -> Option<u64> {
    use base64::Engine as _;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    claims.get("exp")?.as_u64()?.checked_mul(1000)
}
