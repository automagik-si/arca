//! GitHub's runner scale set protocol, ported from `actions/scaleset` (client.go, session_client.go, types.go at
//! 716908e): scale sets, message sessions, the message long poll, job acquisition and runner removal.
//!
//! Every call but the message queue's is made with the Actions service connection the [`Access`] gives; the message
//! queue's calls take the session's own queue token. A 401 from the Actions service drops the connection and the call
//! is made once more with a fresh one; a 401 from the queue means the session's token expired, and the session is
//! refreshed.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::access::{Access, AccessError, ActionsConnection};
use crate::secret::Secret;
use crate::signin::SignInReason;

/// The Actions service API version every scale set call names.
pub const API_VERSION: &str = "6.0-preview";
/// The scale set collection, under the Actions service address.
pub const SCALE_SET_PATH: &str = "/_apis/runtime/runnerscalesets";
/// The runner collection, under the Actions service address.
pub const RUNNER_PATH: &str = "/_apis/distributedtask/pools/0/agents";
/// The header that tells GitHub how many runners the scale set can still take.
pub const MAX_CAPACITY_HEADER: &str = "X-ScaleSetMaxCapacity";
/// How long one message poll may last: GitHub holds it up to about 50 seconds.
pub const POLL_TIMEOUT: Duration = Duration::from_secs(120);
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// A request's credential.
pub enum Auth<'a> {
    Bearer(&'a Secret),
    RemoteAuth(&'a Secret),
}

/// One answer: the status and the body.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Reply {
    /// GitHub's own words about a failure, short, from `message` when the body has one.
    pub fn message(&self) -> String {
        serde_json::from_slice::<Value>(trim_bom(&self.body))
            .ok()
            .and_then(|value| {
                value
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| {
                String::from_utf8_lossy(&self.body)
                    .chars()
                    .take(200)
                    .collect()
            })
            .chars()
            .take(200)
            .collect()
    }
}

/// The HTTPS client every call goes through.
#[derive(Clone)]
pub struct Http {
    client: reqwest::Client,
}

impl Http {
    /// A client with TLS through rustls and ring and the Mozilla roots of webpki-roots, so a static binary needs no
    /// system certificate store. Redirects are not followed: a credential goes only where it was sent.
    pub fn new() -> Result<Self, String> {
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let tls = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|_| "TLS has no protocol version".to_string())?
        .with_root_certificates(roots)
        .with_no_client_auth();
        let client = reqwest::Client::builder()
            .tls_backend_preconfigured(tls)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("arca-runners/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|error| format!("the HTTPS client could not be built: {error}"))?;
        Ok(Self { client })
    }

    /// Sends one request and reads its whole answer. A request is bounded by [`CALL_TIMEOUT`] unless `headers` names
    /// the message poll's capacity, which is bounded by [`POLL_TIMEOUT`].
    pub async fn send(
        &self,
        method: &str,
        url: &str,
        auth: Auth<'_>,
        body: Option<&Value>,
        headers: &[(&str, String)],
    ) -> Result<Reply, String> {
        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|_| "a bad method".to_string())?;
        let poll = headers.iter().any(|(name, _)| *name == MAX_CAPACITY_HEADER);
        let mut request = self
            .client
            .request(method, url)
            .timeout(if poll { POLL_TIMEOUT } else { CALL_TIMEOUT })
            .header("Accept", "application/json; api-version=6.0-preview");
        request = match auth {
            Auth::Bearer(token) => {
                request.header("Authorization", format!("Bearer {}", token.expose()))
            }
            Auth::RemoteAuth(token) => {
                request.header("Authorization", format!("RemoteAuth {}", token.expose()))
            }
        };
        for (name, value) in headers {
            request = request.header(*name, value);
        }
        if let Some(body) = body {
            let bytes =
                serde_json::to_vec(body).map_err(|_| "the body does not serialize".to_string())?;
            request = request
                .header("Content-Type", "application/json")
                .body(bytes);
        }
        // reqwest's error text names the address and never a header, so it carries no credential.
        let response = request
            .send()
            .await
            .map_err(|error| error.without_url().to_string())?;
        let status = response.status().as_u16();
        let body = response
            .bytes()
            .await
            .map_err(|error| error.without_url().to_string())?
            .to_vec();
        Ok(Reply { status, body })
    }
}

/// The body without a UTF-8 byte order mark, which the Actions service sometimes sends.
pub fn trim_bom(body: &[u8]) -> &[u8] {
    body.strip_prefix(b"\xef\xbb\xbf").unwrap_or(body)
}

/// A label of a scale set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Label {
    #[serde(rename = "type", default)]
    pub kind: String,
    pub name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunnerSetting {
    #[serde(default)]
    pub disable_update: bool,
}

/// A runner scale set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScaleSet {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub runner_group_id: u64,
    #[serde(default)]
    pub labels: Vec<Label>,
    #[serde(rename = "RunnerSetting", default)]
    pub runner_setting: RunnerSetting,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub statistics: Option<Statistics>,
}

/// What GitHub counts for a scale set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Statistics {
    pub total_available_jobs: i64,
    pub total_acquired_jobs: i64,
    pub total_assigned_jobs: i64,
    pub total_running_jobs: i64,
    pub total_registered_runners: i64,
    pub total_busy_runners: i64,
    pub total_idle_runners: i64,
}

/// A message session on a scale set's queue.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub session_id: String,
    #[serde(default)]
    pub owner_name: String,
    pub message_queue_url: String,
    pub message_queue_access_token: Secret,
    #[serde(default)]
    pub statistics: Option<Statistics>,
}

/// One job message: what GitHub says about a job of the scale set. The fields depend on its type.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct JobMessage {
    pub message_type: String,
    pub runner_request_id: i64,
    pub repository_name: String,
    pub owner_name: String,
    pub job_id: String,
    pub job_workflow_ref: String,
    pub job_display_name: String,
    pub workflow_run_id: i64,
    pub event_name: String,
    pub request_labels: Vec<String>,
    pub queue_time: String,
    pub scale_set_assign_time: String,
    pub runner_assign_time: String,
    pub finish_time: String,
    pub result: String,
    pub runner_id: i64,
    pub runner_name: String,
    pub acquire_job_url: String,
}

/// One message of the queue, its job messages sorted by type.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Message {
    pub message_id: i64,
    pub statistics: Option<Statistics>,
    pub available: Vec<JobMessage>,
    pub assigned: Vec<JobMessage>,
    pub started: Vec<JobMessage>,
    pub completed: Vec<JobMessage>,
}

/// A runner as the Actions service lists it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunnerRef {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub runner_scale_set_id: u64,
}

/// Which call failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Call {
    GetScaleSet,
    CreateScaleSet,
    UpdateScaleSet,
    DeleteScaleSet,
    CreateSession,
    RefreshSession,
    DeleteSession,
    GetMessage,
    DeleteMessage,
    AcquireJobs,
    GetRunner,
    RemoveRunner,
}

/// Why a scale set call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// The credential's holder could not give a connection.
    Access(AccessError),
    /// The Actions service answered with a failure.
    Status {
        call: Call,
        status: u16,
        message: String,
    },
    /// The session's queue token expired: refresh the session.
    QueueTokenExpired,
    /// The Actions service could not be reached, or the answer was cut.
    Network(String),
    /// The answer is not what the protocol says.
    Decode(String),
}

impl ApiError {
    /// The sign-in failure this is, if it is one. A 401 from the Actions service that survived a fresh connection is a
    /// bad credential; a 403 a missing permission.
    pub fn signin(&self) -> Option<SignInReason> {
        match self {
            Self::Access(error) => error.signin(),
            Self::Status { status: 401, .. } => Some(SignInReason::BadCredentials),
            Self::Status { status: 403, .. } => Some(SignInReason::Forbidden),
            _ => None,
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Access(error) => error.fmt(f),
            Self::Status {
                call,
                status,
                message,
            } => {
                write!(f, "{call:?} answered {status}: {message}")
            }
            Self::QueueTokenExpired => f.write_str("the message queue token expired"),
            Self::Network(why) => write!(f, "the Actions service could not be reached: {why}"),
            Self::Decode(why) => write!(f, "the Actions service's answer is not readable: {why}"),
        }
    }
}

#[derive(Deserialize)]
struct List<T> {
    #[serde(default = "Vec::new")]
    value: Vec<T>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMessage {
    message_id: i64,
    message_type: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    statistics: Option<Statistics>,
}

/// The scale set client of one binding.
pub struct ScaleSetClient<A: Access> {
    http: Http,
    access: Arc<A>,
    binding: String,
    connection: Mutex<Option<ActionsConnection>>,
}

impl<A: Access> ScaleSetClient<A> {
    pub fn new(http: Http, access: Arc<A>, binding: &str) -> Self {
        Self {
            http,
            access,
            binding: binding.to_string(),
            connection: Mutex::new(None),
        }
    }

    pub fn access(&self) -> &Arc<A> {
        &self.access
    }

    pub fn binding(&self) -> &str {
        &self.binding
    }

    async fn connection(&self) -> Result<ActionsConnection, ApiError> {
        let mut held = self.connection.lock().await;
        if let Some(connection) = held.as_ref()
            && connection.expires_at_ms > crate::time::now_ms() + 5 * 60 * 1000
        {
            return Ok(connection.clone());
        }
        let fresh = self
            .access
            .actions_connection(&self.binding)
            .await
            .map_err(ApiError::Access)?;
        *held = Some(fresh.clone());
        Ok(fresh)
    }

    async fn forget_connection(&self) {
        *self.connection.lock().await = None;
        self.access.forget_connection(&self.binding);
    }

    /// One Actions service call with the admin token, made once more with a fresh connection after a 401.
    async fn service(
        &self,
        method: &str,
        path_and_query: &str,
        body: Option<&Value>,
    ) -> Result<Reply, ApiError> {
        for attempt in 0..2 {
            let connection = self.connection().await?;
            let url = service_url(&connection.service_url, path_and_query);
            let reply = self
                .http
                .send(
                    method,
                    &url,
                    Auth::Bearer(&connection.admin_token),
                    body,
                    &[],
                )
                .await
                .map_err(ApiError::Network)?;
            if reply.status == 401 && attempt == 0 {
                self.forget_connection().await;
                continue;
            }
            return Ok(reply);
        }
        unreachable!("the loop returns on its second pass")
    }

    /// The scale set of that name in the runner group, if there is one.
    pub async fn get_scale_set(
        &self,
        group: u64,
        name: &str,
    ) -> Result<Option<ScaleSet>, ApiError> {
        let path = format!(
            "{SCALE_SET_PATH}?runnerGroupId={group}&name={}",
            query_escape(name)
        );
        let reply = self.service("GET", &path, None).await?;
        let list: List<ScaleSet> = expect(Call::GetScaleSet, &reply, 200)?;
        if list.value.len() > 1 {
            return Err(ApiError::Decode(
                "more than one scale set has that name".to_string(),
            ));
        }
        Ok(list.value.into_iter().next())
    }

    /// Creates a scale set with these labels in the runner group. Its runners never update themselves.
    pub async fn create_scale_set(
        &self,
        group: u64,
        name: &str,
        labels: &[String],
    ) -> Result<ScaleSet, ApiError> {
        let body = scale_set_body(group, name, labels);
        let reply = self.service("POST", SCALE_SET_PATH, Some(&body)).await?;
        expect(Call::CreateScaleSet, &reply, 200)
    }

    /// Replaces a scale set's labels.
    pub async fn update_scale_set(
        &self,
        id: u64,
        group: u64,
        name: &str,
        labels: &[String],
    ) -> Result<ScaleSet, ApiError> {
        let body = scale_set_body(group, name, labels);
        let path = format!("{SCALE_SET_PATH}/{id}");
        let reply = self.service("PATCH", &path, Some(&body)).await?;
        expect(Call::UpdateScaleSet, &reply, 200)
    }

    pub async fn delete_scale_set(&self, id: u64) -> Result<(), ApiError> {
        let path = format!("{SCALE_SET_PATH}/{id}");
        let reply = self.service("DELETE", &path, None).await?;
        expect_empty(Call::DeleteScaleSet, &reply, &[200, 204, 404])
    }

    /// Opens the scale set's message session. GitHub answers 409 while another session is open on it.
    pub async fn create_session(&self, scale_set: u64, owner: &str) -> Result<Session, ApiError> {
        let path = format!("{SCALE_SET_PATH}/{scale_set}/sessions");
        let body = json!({ "ownerName": owner });
        let reply = self.service("POST", &path, Some(&body)).await?;
        expect(Call::CreateSession, &reply, 200)
    }

    /// The same session with a new queue token.
    pub async fn refresh_session(
        &self,
        scale_set: u64,
        session: &Session,
    ) -> Result<Session, ApiError> {
        let path = format!(
            "{SCALE_SET_PATH}/{scale_set}/sessions/{}",
            session.session_id
        );
        let reply = self.service("PATCH", &path, None).await?;
        expect(Call::RefreshSession, &reply, 200)
    }

    pub async fn delete_session(&self, scale_set: u64, session: &Session) -> Result<(), ApiError> {
        let path = format!(
            "{SCALE_SET_PATH}/{scale_set}/sessions/{}",
            session.session_id
        );
        let reply = self.service("DELETE", &path, None).await?;
        expect_empty(Call::DeleteSession, &reply, &[200, 204, 404])
    }

    /// The next message after `last`, waiting up to [`POLL_TIMEOUT`]. `None` when the poll ended with nothing new.
    /// `capacity` is how many more runners the scale set can take.
    pub async fn get_message(
        &self,
        session: &Session,
        last: i64,
        capacity: u32,
    ) -> Result<Option<Message>, ApiError> {
        let mut url = session.message_queue_url.clone();
        if last > 0 {
            url.push(if url.contains('?') { '&' } else { '?' });
            url.push_str(&format!("lastMessageId={last}"));
        }
        let reply = self
            .http
            .send(
                "GET",
                &url,
                Auth::Bearer(&session.message_queue_access_token),
                None,
                &[(MAX_CAPACITY_HEADER, capacity.to_string())],
            )
            .await
            .map_err(ApiError::Network)?;
        match reply.status {
            202 => Ok(None),
            200 => parse_message(&reply.body).map(Some),
            401 => Err(ApiError::QueueTokenExpired),
            status => Err(ApiError::Status {
                call: Call::GetMessage,
                status,
                message: reply.message(),
            }),
        }
    }

    /// Acknowledges a message, so it is not sent again.
    pub async fn delete_message(&self, session: &Session, message: i64) -> Result<(), ApiError> {
        let base = session.message_queue_url.split('?').collect::<Vec<_>>();
        let mut url = format!("{}/{message}", base[0].trim_end_matches('/'));
        if let Some(query) = base.get(1) {
            url.push('?');
            url.push_str(query);
        }
        let reply = self
            .http
            .send(
                "DELETE",
                &url,
                Auth::Bearer(&session.message_queue_access_token),
                None,
                &[],
            )
            .await
            .map_err(ApiError::Network)?;
        match reply.status {
            200 | 204 | 404 => Ok(()),
            401 => Err(ApiError::QueueTokenExpired),
            status => Err(ApiError::Status {
                call: Call::DeleteMessage,
                status,
                message: reply.message(),
            }),
        }
    }

    /// Takes the offered jobs for the scale set. Returns the request ids GitHub gave it. Taking a job again is harmless.
    pub async fn acquire_jobs(
        &self,
        scale_set: u64,
        session: &Session,
        ids: &[i64],
    ) -> Result<Vec<i64>, ApiError> {
        let connection = self.connection().await?;
        let url = service_url(
            &connection.service_url,
            &format!("{SCALE_SET_PATH}/{scale_set}/acquirejobs"),
        );
        let body = json!(ids);
        let reply = self
            .http
            .send(
                "POST",
                &url,
                Auth::Bearer(&session.message_queue_access_token),
                Some(&body),
                &[],
            )
            .await
            .map_err(ApiError::Network)?;
        if reply.status == 401 {
            return Err(ApiError::QueueTokenExpired);
        }
        let list: List<i64> = expect(Call::AcquireJobs, &reply, 200)?;
        Ok(list.value)
    }

    /// The runner of that name, if the Actions service has one.
    pub async fn get_runner_by_name(&self, name: &str) -> Result<Option<RunnerRef>, ApiError> {
        let path = format!("{RUNNER_PATH}?agentName={}", query_escape(name));
        let reply = self.service("GET", &path, None).await?;
        let list: List<RunnerRef> = expect(Call::GetRunner, &reply, 200)?;
        Ok(list.value.into_iter().next())
    }

    /// Removes a runner from GitHub. One that is gone already is not an error.
    pub async fn remove_runner(&self, id: u64) -> Result<(), ApiError> {
        let path = format!("{RUNNER_PATH}/{id}");
        let reply = self.service("DELETE", &path, None).await?;
        expect_empty(Call::RemoveRunner, &reply, &[200, 204, 404])
    }
}

fn scale_set_body(group: u64, name: &str, labels: &[String]) -> Value {
    json!({
        "name": name,
        "runnerGroupId": group,
        "labels": labels.iter().map(|label| json!({ "type": "System", "name": label })).collect::<Vec<_>>(),
        "RunnerSetting": { "disableUpdate": true },
    })
}

/// The Actions service address of `path_and_query`, with the API version added.
fn service_url(base: &str, path_and_query: &str) -> String {
    let (path, query) = path_and_query
        .split_once('?')
        .map_or((path_and_query, ""), |(path, query)| (path, query));
    let base = base.trim_end_matches('/');
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    let mut url = format!("{base}{path}?");
    if !query.is_empty() {
        url.push_str(query);
        url.push('&');
    }
    url.push_str("api-version=");
    url.push_str(API_VERSION);
    url
}

fn query_escape(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn expect<T: serde::de::DeserializeOwned>(
    call: Call,
    reply: &Reply,
    want: u16,
) -> Result<T, ApiError> {
    if reply.status != want && !(want == 200 && reply.status == 201) {
        return Err(ApiError::Status {
            call,
            status: reply.status,
            message: reply.message(),
        });
    }
    serde_json::from_slice(trim_bom(&reply.body))
        .map_err(|error| ApiError::Decode(format!("{call:?}: {error}")))
}

fn expect_empty(call: Call, reply: &Reply, ok: &[u16]) -> Result<(), ApiError> {
    if ok.contains(&reply.status) {
        Ok(())
    } else {
        Err(ApiError::Status {
            call,
            status: reply.status,
            message: reply.message(),
        })
    }
}

/// A message of the queue: its job messages, which come as one JSON string holding an array, sorted by type.
pub fn parse_message(body: &[u8]) -> Result<Message, ApiError> {
    let raw: RawMessage = serde_json::from_slice(trim_bom(body))
        .map_err(|error| ApiError::Decode(format!("message: {error}")))?;
    if raw.message_type != "RunnerScaleSetJobMessages" {
        return Err(ApiError::Decode(format!(
            "an unknown message type: {}",
            raw.message_type.chars().take(64).collect::<String>()
        )));
    }
    let mut message = Message {
        message_id: raw.message_id,
        statistics: raw.statistics,
        ..Message::default()
    };
    if raw.body.is_empty() {
        return Ok(message);
    }
    let jobs: Vec<JobMessage> = serde_json::from_str(&raw.body)
        .map_err(|error| ApiError::Decode(format!("job messages: {error}")))?;
    for job in jobs {
        match job.message_type.as_str() {
            "JobAvailable" => message.available.push(job),
            "JobAssigned" => message.assigned.push(job),
            "JobStarted" => message.started.push(job),
            "JobCompleted" => message.completed.push(job),
            _ => {}
        }
    }
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_sorts_its_jobs_by_type() {
        let jobs = json!([
            { "messageType": "JobAvailable", "runnerRequestId": 7, "repositoryName": "omni", "requestLabels": ["acme-4"] },
            { "messageType": "JobStarted", "runnerRequestId": 6, "runnerName": "r-1", "runnerId": 11 },
            { "messageType": "JobCompleted", "runnerRequestId": 5, "result": "succeeded" },
            { "messageType": "Unknown" }
        ]);
        let body = json!({
            "messageId": 4,
            "messageType": "RunnerScaleSetJobMessages",
            "body": jobs.to_string(),
            "statistics": { "totalAssignedJobs": 2 }
        });
        let mut bytes = b"\xef\xbb\xbf".to_vec();
        bytes.extend(body.to_string().into_bytes());
        let message = parse_message(&bytes).expect("a message");
        assert_eq!(message.message_id, 4);
        assert_eq!(message.available[0].runner_request_id, 7);
        assert_eq!(message.started[0].runner_name, "r-1");
        assert_eq!(message.completed[0].result, "succeeded");
        assert_eq!(
            message.statistics.expect("statistics").total_assigned_jobs,
            2
        );
    }

    #[test]
    fn another_message_type_is_refused() {
        let body = json!({ "messageId": 1, "messageType": "Other", "body": "" });
        assert!(parse_message(body.to_string().as_bytes()).is_err());
    }

    #[test]
    fn service_urls_carry_the_api_version() {
        assert_eq!(
            service_url("https://pipelines.example.com/abc/", "/_apis/x?name=a"),
            "https://pipelines.example.com/abc/_apis/x?name=a&api-version=6.0-preview"
        );
        assert_eq!(
            service_url("https://pipelines.example.com/abc", "_apis/x"),
            "https://pipelines.example.com/abc/_apis/x?api-version=6.0-preview"
        );
        assert_eq!(query_escape("a b/c"), "a%20b%2Fc");
    }
}
