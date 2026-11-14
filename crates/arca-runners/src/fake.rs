//! A fake GitHub for tests, and a fake launcher whose machines run the fake's jobs.
//!
//! [`FakeGitHub`] serves the calls of `docs/RUNNERS.md` over HTTP on a loopback port and plays GitHub's part with a
//! queue of jobs: a job whose labels a scale set has is offered to it (`JobAvailable`), assigned once acquired
//! (`JobAssigned`), started on an idle runner of the scale set that registered with a configuration the fake minted
//! (`JobStarted`), and completed when the machine running it says so (`JobCompleted`). It checks every credential, can
//! answer as an uninstalled App or a bad key, and records every call.
//!
//! [`FakeLauncher`] keeps machines in memory. Each has its own disk, a set of paths, empty when it is made. A job may
//! write paths there and may require paths to be absent: the state-leak test. [`FakeLauncher::reuse_disks`] makes a
//! new machine inherit the disk of the last one destroyed, as a fixed slot does, so the test can be seen to fail.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use serde_json::{Value, json};

use crate::launcher::{LaunchError, Launcher, MachineSpec, RunnerStart};
use crate::secret::Secret;
use crate::time::{format_rfc3339, now_ms};

/// One job to queue.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct JobSpec {
    pub name: String,
    pub repo: String,
    pub labels: Vec<String>,
    /// How long the job runs once started.
    pub seconds: f64,
    /// Paths the job writes on its machine's disk.
    pub writes: Vec<String>,
    /// Paths the job fails on if its machine's disk has them.
    pub absent: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobState {
    Queued,
    Offered,
    Assigned,
    Running,
    Done,
}

#[derive(Debug, Clone)]
struct FakeJob {
    id: i64,
    run_id: i64,
    spec: JobSpec,
    state: JobState,
    scale_set: Option<u64>,
    runner: Option<u64>,
    /// The runner's name, kept because GitHub removes an ephemeral runner once its job is done.
    runner_name: String,
    queued_ms: u64,
    assigned_ms: u64,
    started: Option<Instant>,
    finished_ms: u64,
    result: String,
}

#[derive(Debug, Clone)]
struct FakeSession {
    id: String,
    queue_token: String,
}

#[derive(Debug, Clone)]
struct FakeScaleSet {
    id: u64,
    name: String,
    group: u64,
    labels: Vec<String>,
    session: Option<FakeSession>,
    queue: VecDeque<(i64, Vec<Value>)>,
    next_message: i64,
}

#[derive(Debug, Clone)]
struct FakeRunner {
    id: u64,
    name: String,
    scale_set: u64,
    jit: String,
    online: bool,
    job: Option<i64>,
}

/// One call the fake took.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub method: String,
    pub path: String,
    pub query: String,
    /// The `X-ScaleSetMaxCapacity` header, on a message poll.
    pub capacity: Option<u32>,
    pub status: u16,
}

/// What a test can read back about one finished job.
#[derive(Debug, Clone, PartialEq)]
pub struct Finished {
    pub name: String,
    pub runner: String,
    pub result: String,
    pub queue_ms: u64,
}

#[derive(Debug, Default)]
struct World {
    base: String,
    token: String,
    installed: bool,
    registration_tokens: BTreeSet<String>,
    admin_tokens: BTreeSet<String>,
    scale_sets: BTreeMap<u64, FakeScaleSet>,
    jobs: BTreeMap<i64, FakeJob>,
    runners: BTreeMap<u64, FakeRunner>,
    removed_runners: Vec<String>,
    next_id: u64,
    calls: Vec<Call>,
    poll: Duration,
}

/// The fake GitHub. Clones share it.
#[derive(Clone)]
pub struct FakeGitHub {
    world: Arc<Mutex<World>>,
    base: String,
}

impl FakeGitHub {
    /// Starts the fake on a loopback port, accepting `token` as the installation token. Needs a tokio runtime.
    pub async fn start(token: &str) -> std::io::Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let world = Arc::new(Mutex::new(World {
            base: base.clone(),
            token: token.to_string(),
            installed: true,
            next_id: 1,
            poll: Duration::from_millis(300),
            ..World::default()
        }));
        let shared = Arc::clone(&world);
        let router = axum::Router::new().fallback(
            move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| {
                let world = Arc::clone(&shared);
                async move { handle(world, method, uri, headers, body).await }
            },
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Ok(Self { world, base })
    }

    /// The REST address: `RegistrationAccess`'s `api_url`.
    pub fn api_url(&self) -> &str {
        &self.base
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, World> {
        self.world
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The App is installed on the organisation, or not (a 404 for the registration token).
    pub fn set_installed(&self, installed: bool) {
        self.lock().installed = installed;
    }

    /// Accepts another installation token, as after a key change; the old one answers 401. Every derived token is
    /// revoked too.
    pub fn set_token(&self, token: &str) {
        let mut world = self.lock();
        world.token = token.to_string();
        world.registration_tokens.clear();
        world.admin_tokens.clear();
    }

    /// Queues a job. It is offered at once to the scale set with its labels, or when one is made.
    pub fn queue_job(&self, spec: JobSpec) -> i64 {
        let mut world = self.lock();
        let id = i64::try_from(world.next_id).unwrap_or(i64::MAX);
        world.next_id += 1;
        world.jobs.insert(
            id,
            FakeJob {
                id,
                run_id: 4_000_000 + id,
                spec,
                state: JobState::Queued,
                scale_set: None,
                runner: None,
                runner_name: String::new(),
                queued_ms: now_ms(),
                assigned_ms: 0,
                started: None,
                finished_ms: 0,
                result: String::new(),
            },
        );
        offer(&mut world);
        id
    }

    /// Every call taken so far.
    pub fn calls(&self) -> Vec<Call> {
        self.lock().calls.clone()
    }

    /// The finished jobs, in the order they were queued.
    pub fn finished(&self) -> Vec<Finished> {
        let world = self.lock();
        world
            .jobs
            .values()
            .filter(|job| job.state == JobState::Done)
            .map(|job| Finished {
                name: job.spec.name.clone(),
                runner: job.runner_name.clone(),
                result: job.result.clone(),
                queue_ms: job.assigned_ms.saturating_sub(job.queued_ms),
            })
            .collect()
    }

    /// The runners removed through the API, by name.
    pub fn removed_runners(&self) -> Vec<String> {
        self.lock().removed_runners.clone()
    }

    /// The scale sets: name and labels.
    pub fn scale_sets(&self) -> Vec<(String, Vec<String>)> {
        self.lock()
            .scale_sets
            .values()
            .map(|set| (set.name.clone(), set.labels.clone()))
            .collect()
    }

    /// Whether a session is open on the scale set of that name.
    pub fn session_open(&self, name: &str) -> bool {
        self.lock()
            .scale_sets
            .values()
            .any(|set| set.name == name && set.session.is_some())
    }

    /// A runner came online with this configuration. Returns its name; an unknown configuration is refused.
    fn runner_online(&self, jit: &str) -> Option<String> {
        let mut world = self.lock();
        let runner = world
            .runners
            .values_mut()
            .find(|runner| runner.jit == jit)?;
        if runner.online {
            // A configuration is single-use.
            return None;
        }
        runner.online = true;
        let name = runner.name.clone();
        assign(&mut world);
        Some(name)
    }

    /// The job the runner of that name runs: its spec and how long it has run.
    fn runner_job(&self, name: &str) -> Option<(i64, JobSpec, Duration)> {
        let world = self.lock();
        let runner = world.runners.values().find(|runner| runner.name == name)?;
        let job = world.jobs.get(&runner.job?)?;
        Some((job.id, job.spec.clone(), job.started?.elapsed()))
    }

    /// The runner of that name finished its job with `result`. The runner is gone after it, as a JIT runner is.
    fn finish(&self, name: &str, result: &str) {
        let mut world = self.lock();
        let Some(runner_id) = world
            .runners
            .values()
            .find(|runner| runner.name == name)
            .map(|runner| runner.id)
        else {
            return;
        };
        let Some(job_id) = world.runners.get(&runner_id).and_then(|runner| runner.job) else {
            return;
        };
        let now = now_ms();
        let Some(job) = world.jobs.get_mut(&job_id) else {
            return;
        };
        job.state = JobState::Done;
        job.result = result.to_string();
        job.finished_ms = now;
        let set = job.scale_set;
        let message = job_message("JobCompleted", job, Some((runner_id, name)));
        world.runners.remove(&runner_id);
        if let Some(set) = set {
            push(&mut world, set, vec![message]);
        }
    }
}

/// Offers each queued job to the scale set whose labels hold all of the job's.
fn offer(world: &mut World) {
    let sets: Vec<(u64, Vec<String>, bool)> = world
        .scale_sets
        .values()
        .map(|set| {
            (
                set.id,
                set.labels
                    .iter()
                    .map(|label| label.to_ascii_lowercase())
                    .collect(),
                set.session.is_some(),
            )
        })
        .collect();
    let mut pushes = Vec::new();
    for job in world
        .jobs
        .values_mut()
        .filter(|job| job.state == JobState::Queued)
    {
        let wanted: Vec<String> = job
            .spec
            .labels
            .iter()
            .map(|label| label.to_ascii_lowercase())
            .collect();
        if let Some((set, _, _)) = sets
            .iter()
            .find(|(_, labels, open)| *open && wanted.iter().all(|label| labels.contains(label)))
        {
            job.state = JobState::Offered;
            job.scale_set = Some(*set);
            pushes.push((*set, job_message("JobAvailable", job, None)));
        }
    }
    for (set, message) in pushes {
        push(world, set, vec![message]);
    }
}

/// Starts each assigned job on an idle online runner of its scale set.
fn assign(world: &mut World) {
    let waiting: Vec<(i64, u64)> = world
        .jobs
        .values()
        .filter(|job| job.state == JobState::Assigned && job.runner.is_none())
        .filter_map(|job| Some((job.id, job.scale_set?)))
        .collect();
    for (job_id, set) in waiting {
        let Some(runner) = world
            .runners
            .values_mut()
            .find(|runner| runner.scale_set == set && runner.online && runner.job.is_none())
        else {
            continue;
        };
        runner.job = Some(job_id);
        let (runner_id, name) = (runner.id, runner.name.clone());
        let Some(job) = world.jobs.get_mut(&job_id) else {
            continue;
        };
        job.state = JobState::Running;
        job.runner = Some(runner_id);
        job.runner_name.clone_from(&name);
        job.assigned_ms = now_ms();
        job.started = Some(Instant::now());
        let message = job_message("JobStarted", job, Some((runner_id, &name)));
        push(world, set, vec![message]);
    }
}

fn job_message(kind: &str, job: &FakeJob, runner: Option<(u64, &str)>) -> Value {
    let mut message = json!({
        "messageType": kind,
        "runnerRequestId": job.id,
        "repositoryName": job.spec.repo,
        "ownerName": "acme",
        "jobId": format!("job-{}", job.id),
        "jobWorkflowRef": format!("acme/{}/.github/workflows/ci.yml@refs/heads/main", job.spec.repo),
        "jobDisplayName": job.spec.name,
        "workflowRunId": job.run_id,
        "eventName": "pull_request",
        "requestLabels": job.spec.labels,
        "queueTime": format_rfc3339(job.queued_ms),
    });
    if job.assigned_ms > 0 {
        message["runnerAssignTime"] = json!(format_rfc3339(job.assigned_ms));
    }
    if job.finished_ms > 0 {
        message["finishTime"] = json!(format_rfc3339(job.finished_ms));
        message["result"] = json!(job.result);
    }
    if let Some((id, name)) = runner {
        message["runnerId"] = json!(id);
        message["runnerName"] = json!(name);
    }
    message
}

fn push(world: &mut World, set: u64, jobs: Vec<Value>) {
    if let Some(set) = world.scale_sets.get_mut(&set) {
        set.next_message += 1;
        let id = set.next_message;
        set.queue.push_back((id, jobs));
    }
}

fn statistics(world: &World, set: u64) -> Value {
    let count = |state: JobState| {
        world
            .jobs
            .values()
            .filter(|job| job.scale_set == Some(set) && job.state == state)
            .count()
    };
    let assigned = count(JobState::Assigned) + count(JobState::Running);
    let runners: Vec<&FakeRunner> = world
        .runners
        .values()
        .filter(|runner| runner.scale_set == set)
        .collect();
    json!({
        "totalAvailableJobs": count(JobState::Offered),
        "totalAcquiredJobs": count(JobState::Assigned),
        "totalAssignedJobs": assigned,
        "totalRunningJobs": count(JobState::Running),
        "totalRegisteredRunners": runners.len(),
        "totalBusyRunners": runners.iter().filter(|runner| runner.job.is_some()).count(),
        "totalIdleRunners": runners.iter().filter(|runner| runner.job.is_none()).count(),
    })
}

fn random_hex() -> String {
    let mut bytes = [0u8; 8];
    let _ = getrandom::fill(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn admin_jwt() -> String {
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let claims = json!({ "exp": now_ms() / 1000 + 3600, "nonce": random_hex() });
    format!(
        "{}.{}.fake",
        engine.encode(br#"{"alg":"none"}"#),
        engine.encode(claims.to_string())
    )
}

fn reply(status: StatusCode, body: Value) -> Response {
    (
        status,
        [("content-type", "application/json")],
        body.to_string(),
    )
        .into_response()
}

fn empty(status: StatusCode) -> Response {
    status.into_response()
}

fn bearer(headers: &HeaderMap, scheme: &str) -> Option<String> {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix(scheme))
        .map(|value| value.trim().to_string())
}

fn query_value(query: &str, key: &str) -> Option<String> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(name, _)| *name == key)
        .map(|(_, value)| value.replace("%20", " ").replace("%2F", "/"))
}

async fn handle(
    world: Arc<Mutex<World>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = uri.path().to_string();
    let query = uri.query().unwrap_or("").to_string();
    let capacity = headers
        .get(crate::scaleset::MAX_CAPACITY_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok());
    // A message poll is held until a message is there or the poll time passes, as GitHub's long poll is.
    if method == Method::GET && path.starts_with("/queue/") {
        let poll = world.lock().map(|world| world.poll).unwrap_or_default();
        let until = Instant::now() + poll;
        loop {
            let ready = {
                let world = world
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                queue_ready(&world, &path, &query)
            };
            if ready || Instant::now() >= until {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    let mut world = world
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let response = route(&mut world, &method, &path, &query, &headers, &body);
    let status = response.status().as_u16();
    world.calls.push(Call {
        method: method.to_string(),
        path,
        query,
        capacity,
        status,
    });
    response
}

fn queue_ready(world: &World, path: &str, query: &str) -> bool {
    let Some(set) = path
        .strip_prefix("/queue/")
        .and_then(|rest| rest.parse::<u64>().ok())
    else {
        return true;
    };
    let last: i64 = query_value(query, "lastMessageId")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    world
        .scale_sets
        .get(&set)
        .is_none_or(|set| set.queue.iter().any(|(id, _)| *id > last))
}

fn route(
    world: &mut World,
    method: &Method,
    path: &str,
    query: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Response {
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    match (method.as_str(), parts.as_slice()) {
        ("POST", ["orgs", _org, "actions", "runners", "registration-token"]) => {
            if bearer(headers, "Bearer ") != Some(world.token.clone()) {
                return reply(
                    StatusCode::UNAUTHORIZED,
                    json!({ "message": "Bad credentials" }),
                );
            }
            if !world.installed {
                return reply(StatusCode::NOT_FOUND, json!({ "message": "Not Found" }));
            }
            let token = format!("reg-{}", random_hex());
            world.registration_tokens.insert(token.clone());
            reply(
                StatusCode::CREATED,
                json!({ "token": token, "expires_at": "2099-01-01T00:00:00Z" }),
            )
        }
        ("POST", ["actions", "runner-registration"]) => {
            let Some(token) = bearer(headers, "RemoteAuth ") else {
                return reply(StatusCode::UNAUTHORIZED, json!({ "message": "no token" }));
            };
            if !world.registration_tokens.remove(&token) {
                return reply(
                    StatusCode::UNAUTHORIZED,
                    json!({ "message": "Bad credentials" }),
                );
            }
            let admin = admin_jwt();
            world.admin_tokens.insert(admin.clone());
            reply(
                StatusCode::OK,
                json!({ "url": format!("{}/svc/", world.base), "token": admin }),
            )
        }
        (_, ["svc", "_apis", ..]) => service(world, method, &parts[2..], query, headers, body),
        ("GET", ["queue", set]) => {
            let Some(set) = set
                .parse::<u64>()
                .ok()
                .and_then(|id| world.scale_sets.get(&id))
            else {
                return empty(StatusCode::NOT_FOUND);
            };
            if !queue_token_ok(set, headers) {
                return empty(StatusCode::UNAUTHORIZED);
            }
            let last: i64 = query_value(query, "lastMessageId")
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
            let Some((id, jobs)) = set.queue.iter().find(|(id, _)| *id > last).cloned() else {
                return empty(StatusCode::ACCEPTED);
            };
            let id_of = set.id;
            reply(
                StatusCode::OK,
                json!({
                    "messageId": id,
                    "messageType": "RunnerScaleSetJobMessages",
                    "body": Value::Array(jobs).to_string(),
                    "statistics": statistics(world, id_of),
                }),
            )
        }
        ("DELETE", ["queue", set, message]) => {
            let (Ok(set), Ok(message)) = (set.parse::<u64>(), message.parse::<i64>()) else {
                return empty(StatusCode::NOT_FOUND);
            };
            let Some(set) = world.scale_sets.get_mut(&set) else {
                return empty(StatusCode::NOT_FOUND);
            };
            if !queue_token_ok(set, headers) {
                return empty(StatusCode::UNAUTHORIZED);
            }
            set.queue.retain(|(id, _)| *id != message);
            empty(StatusCode::NO_CONTENT)
        }
        _ => reply(StatusCode::NOT_FOUND, json!({ "message": "no such route" })),
    }
}

fn queue_token_ok(set: &FakeScaleSet, headers: &HeaderMap) -> bool {
    set.session.as_ref().is_some_and(|session| {
        bearer(headers, "Bearer ").as_deref() == Some(session.queue_token.as_str())
    })
}

fn service(
    world: &mut World,
    method: &Method,
    parts: &[&str],
    query: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Response {
    if query_value(query, "api-version").as_deref() != Some(crate::scaleset::API_VERSION) {
        return reply(
            StatusCode::BAD_REQUEST,
            json!({ "message": "no api-version" }),
        );
    }
    let token = bearer(headers, "Bearer ").unwrap_or_default();
    let admin = world.admin_tokens.contains(&token);
    let body: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    match (method.as_str(), parts) {
        // acquirejobs takes the session's queue token.
        ("POST", ["runtime", "runnerscalesets", id, "acquirejobs"]) => {
            let Some(set) = id
                .parse::<u64>()
                .ok()
                .and_then(|id| world.scale_sets.get(&id))
            else {
                return empty(StatusCode::NOT_FOUND);
            };
            if !queue_token_ok(set, headers) {
                return empty(StatusCode::UNAUTHORIZED);
            }
            let set = set.id;
            let ids: Vec<i64> = serde_json::from_value(body).unwrap_or_default();
            let mut taken = Vec::new();
            let mut messages = Vec::new();
            for id in ids {
                if let Some(job) = world.jobs.get_mut(&id)
                    && job.scale_set == Some(set)
                    && job.state == JobState::Offered
                {
                    job.state = JobState::Assigned;
                    taken.push(id);
                    messages.push(job_message("JobAssigned", job, None));
                }
            }
            if !messages.is_empty() {
                push(world, set, messages);
            }
            assign(world);
            reply(
                StatusCode::OK,
                json!({ "count": taken.len(), "value": taken }),
            )
        }
        _ if !admin => reply(StatusCode::UNAUTHORIZED, json!({ "message": "Bad token" })),
        ("GET", ["runtime", "runnerscalesets"]) => {
            let group: u64 = query_value(query, "runnerGroupId")
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
            let name = query_value(query, "name").unwrap_or_default();
            let found: Vec<Value> = world
                .scale_sets
                .values()
                .filter(|set| set.group == group && set.name == name)
                .map(scale_set_json)
                .collect();
            reply(
                StatusCode::OK,
                json!({ "count": found.len(), "value": found }),
            )
        }
        ("POST", ["runtime", "runnerscalesets"]) => {
            let id = world.next_id;
            world.next_id += 1;
            let set = FakeScaleSet {
                id,
                name: body["name"].as_str().unwrap_or("").to_string(),
                group: body["runnerGroupId"].as_u64().unwrap_or(0),
                labels: labels_of(&body),
                session: None,
                queue: VecDeque::new(),
                next_message: 0,
            };
            let answer = scale_set_json(&set);
            world.scale_sets.insert(id, set);
            reply(StatusCode::OK, answer)
        }
        ("PATCH", ["runtime", "runnerscalesets", id]) => {
            let Some(set) = id
                .parse::<u64>()
                .ok()
                .and_then(|id| world.scale_sets.get_mut(&id))
            else {
                return empty(StatusCode::NOT_FOUND);
            };
            set.labels = labels_of(&body);
            let answer = scale_set_json(set);
            reply(StatusCode::OK, answer)
        }
        ("DELETE", ["runtime", "runnerscalesets", id]) => {
            if let Ok(id) = id.parse::<u64>() {
                world.scale_sets.remove(&id);
            }
            empty(StatusCode::NO_CONTENT)
        }
        ("POST", ["runtime", "runnerscalesets", id, "sessions"]) => {
            let Some(set_id) = id
                .parse::<u64>()
                .ok()
                .filter(|id| world.scale_sets.contains_key(id))
            else {
                return empty(StatusCode::NOT_FOUND);
            };
            let base = world.base.clone();
            let set = world.scale_sets.get_mut(&set_id).expect("checked above");
            if set.session.is_some() {
                return reply(
                    StatusCode::CONFLICT,
                    json!({ "message": "a session is open" }),
                );
            }
            let session = FakeSession {
                id: format!("{}-{}", random_hex(), random_hex()),
                queue_token: format!("queue-{}", random_hex()),
            };
            set.session = Some(session.clone());
            offer(world);
            reply(
                StatusCode::OK,
                session_json(&base, set_id, &session, statistics(world, set_id)),
            )
        }
        ("PATCH", ["runtime", "runnerscalesets", id, "sessions", sid]) => {
            let base = world.base.clone();
            let Some(set) = id
                .parse::<u64>()
                .ok()
                .and_then(|id| world.scale_sets.get_mut(&id))
            else {
                return empty(StatusCode::NOT_FOUND);
            };
            let Some(session) = set.session.as_mut().filter(|session| session.id == *sid) else {
                return empty(StatusCode::NOT_FOUND);
            };
            session.queue_token = format!("queue-{}", random_hex());
            let (set_id, session) = (set.id, session.clone());
            reply(
                StatusCode::OK,
                session_json(&base, set_id, &session, statistics(world, set_id)),
            )
        }
        ("DELETE", ["runtime", "runnerscalesets", id, "sessions", sid]) => {
            if let Some(set) = id
                .parse::<u64>()
                .ok()
                .and_then(|id| world.scale_sets.get_mut(&id))
                && set
                    .session
                    .as_ref()
                    .is_some_and(|session| session.id == *sid)
            {
                set.session = None;
            }
            empty(StatusCode::NO_CONTENT)
        }
        ("POST", ["runtime", "runnerscalesets", id, "generatejitconfig"]) => {
            let Some(set) = id
                .parse::<u64>()
                .ok()
                .filter(|id| world.scale_sets.contains_key(id))
            else {
                return empty(StatusCode::NOT_FOUND);
            };
            let name = body["name"].as_str().unwrap_or("").to_string();
            if name.is_empty() || world.runners.values().any(|runner| runner.name == name) {
                return reply(
                    StatusCode::CONFLICT,
                    json!({ "message": "a runner of that name exists" }),
                );
            }
            if body["workFolder"].as_str() != Some(crate::config::WORK_FOLDER) {
                return reply(
                    StatusCode::BAD_REQUEST,
                    json!({ "message": "the work folder is not GitHub's" }),
                );
            }
            let id = world.next_id;
            world.next_id += 1;
            let jit = format!("jit-{id}-{}", random_hex());
            world.runners.insert(
                id,
                FakeRunner {
                    id,
                    name: name.clone(),
                    scale_set: set,
                    jit: jit.clone(),
                    online: false,
                    job: None,
                },
            );
            reply(
                StatusCode::OK,
                json!({ "runner": { "id": id, "name": name, "runnerScaleSetId": set }, "encodedJITConfig": jit }),
            )
        }
        ("GET", ["distributedtask", "pools", "0", "agents"]) => {
            let name = query_value(query, "agentName").unwrap_or_default();
            let found: Vec<Value> = world
                .runners
                .values()
                .filter(|runner| runner.name == name)
                .map(|runner| json!({ "id": runner.id, "name": runner.name, "runnerScaleSetId": runner.scale_set }))
                .collect();
            reply(
                StatusCode::OK,
                json!({ "count": found.len(), "value": found }),
            )
        }
        ("DELETE", ["distributedtask", "pools", "0", "agents", id]) => {
            if let Some(runner) = id
                .parse::<u64>()
                .ok()
                .and_then(|id| world.runners.remove(&id))
            {
                world.removed_runners.push(runner.name);
            }
            empty(StatusCode::NO_CONTENT)
        }
        _ => reply(StatusCode::NOT_FOUND, json!({ "message": "no such route" })),
    }
}

fn labels_of(body: &Value) -> Vec<String> {
    body["labels"]
        .as_array()
        .map(|labels| {
            labels
                .iter()
                .filter_map(|label| label["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn scale_set_json(set: &FakeScaleSet) -> Value {
    json!({
        "id": set.id,
        "name": set.name,
        "runnerGroupId": set.group,
        "labels": set.labels.iter().map(|label| json!({ "type": "System", "name": label })).collect::<Vec<_>>(),
        "RunnerSetting": { "disableUpdate": true },
    })
}

fn session_json(base: &str, set: u64, session: &FakeSession, statistics: Value) -> Value {
    json!({
        "sessionId": session.id,
        "ownerName": "arca",
        "messageQueueUrl": format!("{base}/queue/{set}"),
        "messageQueueAccessToken": session.queue_token,
        "statistics": statistics,
    })
}

#[derive(Debug, Clone, Default)]
struct FakeMachine {
    disk: BTreeSet<String>,
    runner: Option<String>,
    ready_reads: u32,
    exit: Option<i64>,
}

#[derive(Debug, Default)]
struct Machines {
    live: BTreeMap<String, FakeMachine>,
    created: Vec<MachineSpec>,
    destroyed: Vec<String>,
    peak: usize,
    boot_reads: u32,
    reuse: bool,
    last_disk: BTreeSet<String>,
    fail_creates: u32,
}

/// The fake launcher. Its machines run the fake GitHub's jobs.
pub struct FakeLauncher {
    github: FakeGitHub,
    machines: Mutex<Machines>,
}

impl FakeLauncher {
    pub fn new(github: FakeGitHub) -> Self {
        Self {
            github,
            machines: Mutex::new(Machines {
                boot_reads: 1,
                ..Machines::default()
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Machines> {
        self.machines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A machine is ready after this many readiness reads.
    pub fn boot_reads(&self, reads: u32) {
        self.lock().boot_reads = reads;
    }

    /// The bug a fixed slot has: a new machine starts with the disk of the last one destroyed.
    pub fn reuse_disks(&self, reuse: bool) {
        self.lock().reuse = reuse;
    }

    /// The next `count` creates fail.
    pub fn fail_creates(&self, count: u32) {
        self.lock().fail_creates = count;
    }

    /// A machine an earlier run left behind.
    pub fn leave_machine(&self, name: &str) {
        self.lock()
            .live
            .insert(name.to_string(), FakeMachine::default());
    }

    /// Every machine made, in order.
    pub fn created(&self) -> Vec<MachineSpec> {
        self.lock().created.clone()
    }

    /// Every machine destroyed, in order.
    pub fn destroyed(&self) -> Vec<String> {
        self.lock().destroyed.clone()
    }

    /// The machines that exist now.
    pub fn live(&self) -> Vec<String> {
        self.lock().live.keys().cloned().collect()
    }

    /// The most machines that existed at once.
    pub fn peak(&self) -> usize {
        self.lock().peak
    }

    /// Runs the job a machine's runner holds, when it has run long enough. Returns the runner's exit code once done.
    fn step(&self, name: &str) -> Option<i64> {
        let runner = {
            let machines = self.lock();
            let machine = machines.live.get(name)?;
            if let Some(code) = machine.exit {
                return Some(code);
            }
            machine.runner.clone()?
        };
        let (_, spec, ran) = self.github.runner_job(&runner)?;
        if ran.as_secs_f64() < spec.seconds {
            return None;
        }
        let result = {
            let mut machines = self.lock();
            let machine = machines.live.get_mut(name)?;
            let leaked = spec.absent.iter().any(|path| machine.disk.contains(path));
            machine.disk.extend(spec.writes.iter().cloned());
            machine.exit = Some(0);
            if leaked { "failed" } else { "succeeded" }
        };
        self.github.finish(&runner, result);
        Some(0)
    }
}

impl Launcher for FakeLauncher {
    fn create(&self, spec: &MachineSpec) -> Result<(), LaunchError> {
        let mut machines = self.lock();
        if machines.fail_creates > 0 {
            machines.fail_creates -= 1;
            return Err(LaunchError("the fake create failed".to_string()));
        }
        if machines.live.contains_key(&spec.name) {
            return Err(LaunchError("a machine of that name exists".to_string()));
        }
        let disk = if machines.reuse {
            machines.last_disk.clone()
        } else {
            BTreeSet::new()
        };
        machines.live.insert(
            spec.name.clone(),
            FakeMachine {
                disk,
                ..FakeMachine::default()
            },
        );
        machines.created.push(spec.clone());
        machines.peak = machines.peak.max(machines.live.len());
        Ok(())
    }

    fn ready(&self, name: &str) -> Result<bool, LaunchError> {
        let mut machines = self.lock();
        let boot = machines.boot_reads;
        let machine = machines
            .live
            .get_mut(name)
            .ok_or_else(|| LaunchError("no such machine".to_string()))?;
        machine.ready_reads += 1;
        Ok(machine.ready_reads >= boot)
    }

    fn start_runner(
        &self,
        name: &str,
        jit: &Secret,
        _start: &RunnerStart,
    ) -> Result<(), LaunchError> {
        if !self.lock().live.contains_key(name) {
            return Err(LaunchError("no such machine".to_string()));
        }
        let runner = self
            .github
            .runner_online(jit.expose())
            .ok_or_else(|| LaunchError("GitHub refused the runner configuration".to_string()))?;
        if let Some(machine) = self.lock().live.get_mut(name) {
            machine.runner = Some(runner);
        }
        Ok(())
    }

    fn runner_exited(&self, name: &str, wait: Duration) -> Result<Option<i64>, LaunchError> {
        let until = Instant::now() + wait.min(Duration::from_millis(200));
        loop {
            if !self.lock().live.contains_key(name) {
                return Err(LaunchError("no such machine".to_string()));
            }
            if let Some(code) = self.step(name) {
                return Ok(Some(code));
            }
            if Instant::now() >= until {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn collect_logs(&self, name: &str) -> Result<Option<Vec<u8>>, LaunchError> {
        Ok(self
            .lock()
            .live
            .get(name)
            .filter(|machine| machine.exit.is_some())
            .map(|_| format!("logs of {name}").into_bytes()))
    }

    fn destroy(&self, name: &str) -> Result<(), LaunchError> {
        let mut machines = self.lock();
        if let Some(machine) = machines.live.remove(name) {
            machines.last_disk = machine.disk;
            machines.destroyed.push(name.to_string());
        }
        Ok(())
    }

    fn existing(&self) -> Result<Vec<String>, LaunchError> {
        Ok(self.lock().live.keys().cloned().collect())
    }
}
