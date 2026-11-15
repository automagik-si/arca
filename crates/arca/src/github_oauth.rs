//! Connecting a GitHub organisation through Arca's OAuth App and GitHub's device flow, with the grant used once.
//!
//! The person asks for runners: an organisation, a runner group, the labels and how many. Arca asks GitHub for a device
//! code (`POST /login/device/code`, scope `admin:org`) and shows the person the eight-character code and
//! `github.com/login/device`. The person types the code there and authorises. Arca polls for the grant
//! (`POST /login/oauth/access_token`) at the pace GitHub sets. With the grant, and only in this order:
//!
//! 1. a runner registration token for the organisation (`POST /orgs/{org}/actions/runners/registration-token`);
//! 2. the runner group: the one the person named, made when the organisation has none of that name, or the default
//!    group when no name was given;
//! 3. the Actions service for the organisation (`POST /actions/runner-registration`, with the registration token);
//! 4. each runner, registered with its own key pair made here, exactly as the runner's `config.sh` registers one: the
//!    runner-admin call (`POST /actions/runners/register`) when GitHub says to use it, otherwise the Actions service's
//!    agent call. Each runner's three files go into the signer's store, 0600;
//! 5. Arca asks GitHub to delete the grant (`DELETE /applications/{client_id}/grant`, with the OAuth App's client ID
//!    and secret), on success and on every failure after the grant arrived, and keeps no copy of it. Only GitHub's 204
//!    counts as deleted; any other answer is shown to the person as NOT deleted, with where to revoke Arca by hand.
//!
//! Every step is a line the person sees (`github_oauth_status`, the page) and a line of the audit log. The runners stay
//! registered and keep their own credentials; nothing that reaches the organisation is kept.

use std::time::Duration;

use reqwest::Method;
use serde::Serialize;
use serde_json::{Value, json};

use crate::github_api::{ACTIONS_API_VERSION, Answer, Auth, RunnerKey, Secret};
use crate::github_signer::{
    Binding, Mode, RegisteredRunner, RunnerGroup, Signer, SignerError, Status, check_account,
    check_labels, fresh_id, write_private,
};

/// The scope the grant asks for. GitHub has no narrower scope that can make a runner registration token for an
/// organisation.
pub const OAUTH_SCOPE: &str = "admin:org";
/// The runner version a registration names: the release the shipped pool pins.
pub const RUNNER_VERSION: &str = "2.338.2";
/// The most runners one connection registers.
pub const MAX_RUNNERS: u32 = 50;
/// Where a person revokes an OAuth App's grant on GitHub by hand.
pub const REVOKE_URL: &str = "https://github.com/settings/applications";
/// How long a finished flow stays listed.
const FLOW_KEEP_MS: u64 = 24 * 60 * 60 * 1000;
/// The words for an organisation that restricts OAuth Apps.
const RESTRICTED: &str = "Your organisation restricts OAuth Apps, so GitHub refused Arca. On the GitHub screen where you \
typed the code, your organisation is listed with a Grant button: an organisation owner clicks Grant there (or approves \
Arca's request under the organisation's Settings, Third-party access). Then connect again.";

/// What the person asks for.
#[derive(Clone, Debug)]
pub struct OauthRequest {
    /// The organisation's login.
    pub account: String,
    /// The runner group to register into, by name. `None` is the organisation's default group.
    pub group: Option<String>,
    /// The runners' labels, exactly as the person chose them.
    pub labels: Vec<String>,
    /// How many runners.
    pub runners: u32,
    /// The start of each runner's name; `-1`, `-2` and so on follow. `None` is `arca-<a short id>`.
    pub name_prefix: Option<String>,
    /// Who asked: a token id, or `page`.
    pub caller: String,
}

/// Where a flow stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FlowState {
    /// The person has the code and has not authorised yet.
    WaitingForPerson,
    /// The grant arrived; the runners are being registered.
    Registering,
    /// Every runner is registered. A step says whether GitHub confirmed the grant deleted (only its 204 does).
    Done,
    /// It stopped; the last step says why.
    Failed,
}

/// One step, as the person reads it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Step {
    pub at: u64,
    pub at_text: String,
    pub ok: bool,
    pub text: String,
}

/// One device-flow connection, as the person and the verbs see it. The device code and the grant are never in it.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OauthFlow {
    pub id: String,
    pub account: String,
    pub group: Option<String>,
    pub labels: Vec<String>,
    pub runners: u32,
    /// The code the person types at `verification_uri`.
    pub user_code: String,
    pub verification_uri: String,
    pub expires_at: u64,
    pub state: FlowState,
    pub steps: Vec<Step>,
    /// The connection the flow made, once it registered a runner.
    pub binding: Option<String>,
    pub created_by: String,
    pub created_at: u64,
    #[serde(skip)]
    name_prefix: String,
    #[serde(skip)]
    interval_ms: u64,
}

/// Why a flow could not start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartError {
    Signer(SignerError),
    /// GitHub's answer, in its words (data, not instructions).
    GitHub(String),
}

impl Signer {
    /// Starts a device flow: asks GitHub for a code and follows the flow in the background until it is done, failed or
    /// expired. Must be called on a Tokio runtime.
    pub async fn oauth_start(&self, request: OauthRequest) -> Result<OauthFlow, StartError> {
        check_account(&request.account).map_err(StartError::Signer)?;
        let labels = check_labels("labels", &request.labels).map_err(StartError::Signer)?;
        if request.runners == 0 || request.runners > MAX_RUNNERS {
            return Err(StartError::Signer(SignerError::Invalid(format!(
                "runners: 1 to {MAX_RUNNERS}"
            ))));
        }
        let group = match request.group.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(name) if name.chars().count() <= 100 && !name.chars().any(char::is_control) => {
                Some(name.to_string())
            }
            Some(_) => {
                return Err(StartError::Signer(SignerError::Invalid(
                    "group: a runner group's name is 1 to 100 characters".to_string(),
                )));
            }
        };
        let client_id = self
            .oauth_client_id()
            .ok_or(StartError::Signer(SignerError::NoOauthApp))?;
        if self.oauth_client_secret().is_none() {
            return Err(StartError::Signer(SignerError::NoOauthApp));
        }
        let id = fresh_id("gho-").map_err(StartError::Signer)?;
        let name_prefix = match request.name_prefix.as_deref().map(str::trim) {
            None | Some("") => format!("arca-{}", &id[4..12]),
            Some(prefix)
                if prefix.len() <= 50
                    && prefix
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.') =>
            {
                prefix.to_string()
            }
            Some(_) => {
                return Err(StartError::Signer(SignerError::Invalid(
                    "namePrefix: at most 50 letters, digits, dots, hyphens and underscores"
                        .to_string(),
                )));
            }
        };
        let answer = self
            .inner
            .http
            .send(
                Method::POST,
                &self.inner.http.web("/login/device/code"),
                Auth::None,
                Some(&json!({ "client_id": client_id, "scope": OAUTH_SCOPE })),
            )
            .await
            .map_err(StartError::GitHub)?;
        let body = answer.json();
        let (Some(device_code), Some(user_code), Some(uri)) = (
            body.get("device_code").and_then(Value::as_str),
            body.get("user_code").and_then(Value::as_str),
            body.get("verification_uri").and_then(Value::as_str),
        ) else {
            return Err(StartError::GitHub(oauth_why(&answer)));
        };
        let now = self.now();
        let expires_in = body
            .get("expires_in")
            .and_then(Value::as_u64)
            .unwrap_or(900);
        let interval = body.get("interval").and_then(Value::as_u64).unwrap_or(5);
        let flow = OauthFlow {
            id: id.clone(),
            account: request.account.clone(),
            group,
            labels,
            runners: request.runners,
            user_code: user_code.to_string(),
            verification_uri: uri.to_string(),
            expires_at: now + expires_in * 1000,
            state: FlowState::WaitingForPerson,
            steps: Vec::new(),
            binding: None,
            created_by: request.caller.clone(),
            created_at: now,
            name_prefix,
            interval_ms: interval * 1000,
        };
        {
            let mut state = self.lock();
            state
                .oauth_flows
                .retain(|_, old| now.saturating_sub(old.created_at) < FLOW_KEEP_MS);
            state.oauth_flows.insert(id.clone(), flow);
            state
                .device_codes
                .insert(id.clone(), Secret::new(device_code));
        }
        self.step(
            &id,
            true,
            format!(
                "Open {uri} and type the code {user_code}. GitHub will ask you to let Arca manage {} once \
                 (admin:org): Arca uses it to register {} runner(s), then asks GitHub to delete it and shows you here \
                 whether GitHub confirmed.",
                request.account, request.runners
            ),
        );
        self.audit(json!({
            "kind": "oauthStart",
            "flow": id,
            "caller": request.caller,
            "account": request.account,
            "group": self.oauth_flow(&id).and_then(|flow| flow.group),
            "labels": request.labels,
            "runners": request.runners,
        }));
        let signer = self.clone();
        let follow = id.clone();
        tokio::spawn(async move { signer.oauth_follow(&follow).await });
        Ok(self.oauth_flow(&id).expect("the flow was just stored"))
    }

    /// One flow, as the person sees it.
    pub fn oauth_flow(&self, id: &str) -> Option<OauthFlow> {
        self.lock().oauth_flows.get(id).cloned()
    }

    /// Every flow of the last day, newest first.
    pub fn oauth_flows(&self) -> Vec<OauthFlow> {
        let mut flows: Vec<OauthFlow> = self.lock().oauth_flows.values().cloned().collect();
        flows.sort_by_key(|flow| std::cmp::Reverse(flow.created_at));
        flows
    }

    fn step(&self, id: &str, ok: bool, text: String) {
        let now = self.now();
        let mut state = self.lock();
        if let Some(flow) = state.oauth_flows.get_mut(id) {
            flow.steps.push(Step {
                at: now,
                at_text: crate::handlers::iso_utc(now),
                ok,
                text,
            });
        }
    }

    fn set_flow(&self, id: &str, change: impl FnOnce(&mut OauthFlow)) {
        if let Some(flow) = self.lock().oauth_flows.get_mut(id) {
            change(flow);
        }
    }

    fn fail(&self, id: &str, text: String) {
        self.step(id, false, text.clone());
        self.set_flow(id, |flow| flow.state = FlowState::Failed);
        self.audit(json!({ "kind": "oauthFailed", "flow": id, "why": text }));
    }

    /// Polls for the grant until it comes, the person declines or the code expires; then registers.
    async fn oauth_follow(&self, id: &str) {
        let Some(client_id) = self.oauth_client_id() else {
            self.lock().device_codes.remove(id);
            self.fail(
                id,
                "This node's OAuth App was removed while waiting.".to_string(),
            );
            return;
        };
        loop {
            let Some(flow) = self.oauth_flow(id) else {
                return;
            };
            if flow.state != FlowState::WaitingForPerson {
                return;
            }
            tokio::time::sleep(Duration::from_millis(flow.interval_ms.max(50))).await;
            if self.now() >= flow.expires_at {
                self.lock().device_codes.remove(id);
                self.fail(
                    id,
                    "The code expired before it was used on GitHub. Nothing was registered. Connect again for a new code."
                        .to_string(),
                );
                return;
            }
            let Some(device_code) = self.lock().device_codes.get(id).cloned() else {
                return;
            };
            let answer = self
                .inner
                .http
                .send(
                    Method::POST,
                    &self.inner.http.web("/login/oauth/access_token"),
                    Auth::None,
                    Some(&json!({
                        "client_id": client_id,
                        "device_code": device_code.expose(),
                        "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
                    })),
                )
                .await;
            let answer = match answer {
                Ok(answer) => answer,
                // No answer: ask again at the next turn, as long as the code lives.
                Err(_) => continue,
            };
            let body = answer.json();
            if let Some(token) = body.get("access_token").and_then(Value::as_str) {
                self.lock().device_codes.remove(id);
                let granted_scopes = body
                    .get("scope")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                self.set_flow(id, |flow| flow.state = FlowState::Registering);
                self.step(
                    id,
                    true,
                    format!(
                        "You authorised Arca on GitHub (scope {}). Arca uses it once now.",
                        if granted_scopes.is_empty() {
                            OAUTH_SCOPE
                        } else {
                            &granted_scopes
                        }
                    ),
                );
                self.audit(json!({ "kind": "oauthGranted", "flow": id, "scope": granted_scopes }));
                let grant = Secret::new(token);
                let outcome = self.oauth_register(id, &grant).await;
                let deleted = self.delete_grant(id, &client_id, &grant).await;
                match outcome {
                    Ok(connect) => {
                        // The connection is logged once the grant is gone (or known not to be).
                        self.audit(connect);
                        self.set_flow(id, |flow| flow.state = FlowState::Done);
                        if deleted {
                            self.step(
                                id,
                                true,
                                "Done. Arca holds no access to your organisation.".to_string(),
                            );
                        } else {
                            self.step(
                                id,
                                true,
                                format!(
                                    "Done: the runners are registered. The grant is still in place until you revoke \
                                     it at {REVOKE_URL}."
                                ),
                            );
                        }
                    }
                    Err(text) => self.fail(id, text),
                }
                return;
            }
            match body.get("error").and_then(Value::as_str) {
                Some("authorization_pending") => {}
                Some("slow_down") => {
                    let interval = body.get("interval").and_then(Value::as_u64);
                    self.set_flow(id, |flow| {
                        flow.interval_ms = interval.map_or(flow.interval_ms + 5000, |s| s * 1000);
                    });
                }
                Some("access_denied") => {
                    self.lock().device_codes.remove(id);
                    self.fail(
                        id,
                        "You declined on GitHub. Nothing was registered and Arca has no access."
                            .to_string(),
                    );
                    return;
                }
                Some("expired_token") => {
                    self.lock().device_codes.remove(id);
                    self.fail(
                        id,
                        "The code expired before it was used on GitHub. Nothing was registered. Connect again for a new code."
                            .to_string(),
                    );
                    return;
                }
                // GitHub is busy or answered something that is not the flow's (a 5xx, a 429, a page that is not
                // JSON): ask again, more slowly, as long as the code lives.
                _ if answer.status >= 500
                    || answer.status == 429
                    || body.get("error").is_none() =>
                {
                    if answer.status == 429 {
                        self.set_flow(id, |flow| flow.interval_ms += 5000);
                    }
                }
                _ => {
                    self.lock().device_codes.remove(id);
                    self.fail(
                        id,
                        format!(
                            "GitHub stopped the sign-in: {}. Nothing was registered.",
                            oauth_why(&answer)
                        ),
                    );
                    return;
                }
            }
        }
    }

    /// Deletes the grant, and says so. `true` only when GitHub confirmed the deletion.
    async fn delete_grant(&self, id: &str, client_id: &str, grant: &Secret) -> bool {
        let Some(secret) = self.oauth_client_secret() else {
            self.step(
                id,
                false,
                format!(
                    "The grant is NOT deleted: this Arca's OAuth App secret is gone. Arca can still act on your \
                     organisation until you revoke it: open {REVOKE_URL}, select this Arca's app under Authorized OAuth \
                     Apps, and press Revoke."
                ),
            );
            self.audit(json!({ "kind": "grantDeleted", "flow": id, "ok": false, "why": "no client secret" }));
            return false;
        };
        let answer = self
            .inner
            .http
            .send(
                Method::DELETE,
                &self
                    .inner
                    .http
                    .api(&format!("/applications/{client_id}/grant")),
                Auth::Basic(client_id, &secret),
                Some(&json!({ "access_token": grant.expose() })),
            )
            .await;
        match answer {
            // Only GitHub's 204 says the grant is gone. A 404 is what GitHub answers to client credentials it does not
            // take (a mistyped or rotated OAuth App secret), so it is not a deletion.
            Ok(answer) if answer.status == 204 => {
                self.step(
                    id,
                    true,
                    "The grant is deleted on GitHub: Arca can no longer act on your organisation."
                        .to_string(),
                );
                self.audit(json!({ "kind": "grantDeleted", "flow": id, "ok": true, "status": answer.status }));
                true
            }
            Ok(answer) => {
                let why = answer.why();
                let hint = if answer.status == 404 {
                    " GitHub did not take this Arca's OAuth App credentials; whoever looks after Arca should set the \
                     App's current secret again."
                } else {
                    ""
                };
                self.step(
                    id,
                    false,
                    format!(
                        "The grant is NOT deleted: {why}. Arca can still act on your organisation until you revoke \
                         it: open {REVOKE_URL}, select this Arca's app under Authorized OAuth Apps, and press \
                         Revoke.{hint}"
                    ),
                );
                self.audit(json!({
                    "kind": "grantDeleted",
                    "flow": id,
                    "ok": false,
                    "status": answer.status,
                    "why": why,
                }));
                false
            }
            Err(why) => {
                self.step(
                    id,
                    false,
                    format!(
                        "The grant is NOT deleted: GitHub could not be reached ({why}). Arca can still act on your \
                         organisation until you revoke it: open {REVOKE_URL}, select this Arca's app under Authorized \
                         OAuth Apps, and press Revoke."
                    ),
                );
                self.audit(json!({ "kind": "grantDeleted", "flow": id, "ok": false, "why": why }));
                false
            }
        }
    }

    /// Uses the grant: registration token, runner group, service, runners. `Err` is the step that stopped it.
    async fn oauth_register(&self, id: &str, grant: &Secret) -> Result<Value, String> {
        let flow = self.oauth_flow(id).ok_or("the flow is gone")?;
        let http = &self.inner.http;
        let org = flow.account.clone();
        let org_url = format!("{}/{org}", http.endpoints().web);

        // 1. The registration token.
        let answer = http
            .send(
                Method::POST,
                &http.api(&format!("/orgs/{org}/actions/runners/registration-token")),
                Auth::Bearer(grant),
                None,
            )
            .await
            .map_err(|why| format!("Arca could not get a runner registration token: {why}."))?;
        if !answer.ok() {
            return Err(refused("get a runner registration token", &answer));
        }
        let registration = answer
            .json()
            .get("token")
            .and_then(Value::as_str)
            .map(Secret::new)
            .ok_or("GitHub's registration token answer had no token")?;
        self.step(
            id,
            true,
            format!("GitHub gave a runner registration token for {org}."),
        );

        // 2. The runner group.
        let group = self
            .oauth_group(id, &org, flow.group.as_deref(), grant)
            .await?;

        // 3. The Actions service for the organisation.
        let answer = http
            .send(
                Method::POST,
                &http.api("/actions/runner-registration"),
                Auth::RemoteAuth(&registration),
                Some(&json!({ "url": org_url, "runner_event": "register" })),
            )
            .await
            .map_err(|why| format!("Arca could not reach GitHub's runner service: {why}."))?;
        if !answer.ok() {
            return Err(refused("open the runner service", &answer));
        }
        let tenant = answer.json();
        let service_url = tenant
            .get("url")
            .and_then(Value::as_str)
            .ok_or("GitHub's runner service answer had no address")?
            .trim_end_matches('/')
            .to_string();
        let admin = tenant
            .get("token")
            .and_then(Value::as_str)
            .map(Secret::new)
            .ok_or("GitHub's runner service answer had no token")?;
        let admin_flow = tenant
            .get("use_v2_flow")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        // The legacy service names the group by its pool id, looked up by name.
        let pool_id = if admin_flow {
            group.id
        } else {
            self.legacy_pool(&service_url, &admin, &group).await?
        };

        // 4. The runners.
        let binding_id = fresh_id("gh-").map_err(|error| error.message())?;
        let now = self.now();
        let mut binding = Binding {
            id: binding_id.clone(),
            mode: Mode::Oauth,
            account: org.clone(),
            status: Status::Active,
            why: None,
            label_sets: vec![flow.labels.clone()],
            runner_group: Some(group.clone()),
            runners: Vec::new(),
            app: None,
            installation: None,
            created_by: flow.created_by.clone(),
            created_at: now,
            updated_at: now,
        };
        for index in 1..=flow.runners {
            let name = format!("{}-{index}", flow.name_prefix);
            let registered = self
                .register_one(
                    &binding_id,
                    &name,
                    &flow.labels,
                    &group,
                    pool_id,
                    &org_url,
                    &service_url,
                    &admin,
                    &registration,
                    admin_flow,
                )
                .await;
            match registered {
                Ok(runner_id) => {
                    binding.runners.push(RegisteredRunner {
                        name: name.clone(),
                        id: runner_id,
                        labels: flow.labels.clone(),
                    });
                    self.step(
                        id,
                        true,
                        format!(
                            "Runner {name} is registered in group \"{}\" with labels {} (GitHub runner id {runner_id}).",
                            group.name,
                            flow.labels.join(", ")
                        ),
                    );
                    self.audit(json!({
                        "kind": "register",
                        "flow": id,
                        "binding": binding_id,
                        "caller": flow.created_by,
                        "account": org,
                        "runnerGroupId": group.id,
                        "runnerName": name,
                        "runnerId": runner_id,
                        "labels": flow.labels,
                        "outcome": "registered",
                    }));
                }
                Err(text) => {
                    self.audit(json!({
                        "kind": "register",
                        "flow": id,
                        "binding": binding_id,
                        "caller": flow.created_by,
                        "account": org,
                        "runnerGroupId": group.id,
                        "runnerName": name,
                        "labels": flow.labels,
                        "outcome": "refused",
                        "why": text,
                    }));
                    if !binding.runners.is_empty() {
                        binding.why = Some(format!(
                            "{} of {} runners were registered before GitHub refused one",
                            binding.runners.len(),
                            flow.runners
                        ));
                        self.save(binding).map_err(|error| error.message())?;
                        self.set_flow(id, |flow| flow.binding = Some(binding_id.clone()));
                    }
                    return Err(text);
                }
            }
        }
        self.save(binding).map_err(|error| error.message())?;
        self.set_flow(id, |flow| flow.binding = Some(binding_id.clone()));
        Ok(json!({
            "kind": "connect",
            "flow": id,
            "binding": binding_id,
            "mode": "oauth",
            "caller": flow.created_by,
            "account": org,
            "runnerGroupId": group.id,
            "runners": flow.runners,
        }))
    }

    /// The runner group: the named one (made when missing), or the default group.
    async fn oauth_group(
        &self,
        id: &str,
        org: &str,
        wanted: Option<&str>,
        grant: &Secret,
    ) -> Result<RunnerGroup, String> {
        let http = &self.inner.http;
        let answer = http
            .send(
                Method::GET,
                &http.api(&format!("/orgs/{org}/actions/runner-groups?per_page=100")),
                Auth::Bearer(grant),
                None,
            )
            .await
            .map_err(|why| format!("Arca could not read the runner groups: {why}."))?;
        let groups: Vec<Value> = if answer.ok() {
            answer
                .json()
                .get("runner_groups")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        } else if wanted.is_none() {
            Vec::new()
        } else {
            return Err(refused("read the organisation's runner groups", &answer));
        };
        let group_of = |group: &Value, created_by_arca: bool, warning: Option<String>| {
            Some(RunnerGroup {
                id: group.get("id")?.as_u64()?,
                name: group.get("name")?.as_str()?.to_string(),
                created_by_arca,
                warning,
            })
        };
        let Some(name) = wanted else {
            let default_warning = "This is the organisation's default runner group: every repository the group admits \
                can send jobs to these runners. Name a group of selected repositories to narrow it."
                .to_string();
            let group = groups
                .iter()
                .find(|group| group.get("default").and_then(Value::as_bool) == Some(true))
                .and_then(|group| group_of(group, false, Some(default_warning.clone())))
                .unwrap_or(RunnerGroup {
                    id: 1,
                    name: "Default".to_string(),
                    created_by_arca: false,
                    warning: Some(default_warning),
                });
            self.step(
                id,
                true,
                format!(
                    "The runners go into the default runner group \"{}\".",
                    group.name
                ),
            );
            return Ok(group);
        };
        if let Some(group) = groups
            .iter()
            .find(|group| {
                group
                    .get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|known| known.eq_ignore_ascii_case(name))
            })
            .and_then(|group| group_of(group, false, None))
        {
            self.step(
                id,
                true,
                format!("The runners go into your runner group \"{}\".", group.name),
            );
            return Ok(group);
        }
        let answer = http
            .send(
                Method::POST,
                &http.api(&format!("/orgs/{org}/actions/runner-groups")),
                Auth::Bearer(grant),
                Some(&json!({
                    "name": name,
                    "visibility": "all",
                    "allows_public_repositories": false,
                })),
            )
            .await
            .map_err(|why| format!("Arca could not make the runner group: {why}."))?;
        if !answer.ok() {
            return Err(refused(
                &format!("make the runner group \"{name}\""),
                &answer,
            ));
        }
        let warning = "Arca made this group open to every private repository of the organisation, public ones off. \
            To narrow it, choose selected repositories in the group's settings on GitHub."
            .to_string();
        let group = group_of(&answer.json(), true, Some(warning.clone()))
            .ok_or("GitHub's answer for the new runner group had no id")?;
        self.step(
            id,
            true,
            format!("Arca made the runner group \"{}\". {warning}", group.name),
        );
        Ok(group)
    }

    /// The Actions service's pool id of `group`, by name, for the legacy registration.
    async fn legacy_pool(
        &self,
        service_url: &str,
        admin: &Secret,
        group: &RunnerGroup,
    ) -> Result<u64, String> {
        let answer = self
            .inner
            .http
            .send(
                Method::GET,
                &format!(
                    "{service_url}/_apis/distributedtask/pools?api-version={ACTIONS_API_VERSION}"
                ),
                Auth::Bearer(admin),
                None,
            )
            .await
            .map_err(|why| format!("Arca could not read the runner service's groups: {why}."))?;
        if !answer.ok() {
            return Err(refused("read the runner service's groups", &answer));
        }
        let pools = answer.json();
        pools
            .get("value")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|pool| pool.get("isHosted").and_then(Value::as_bool) != Some(true))
            .find(|pool| {
                pool.get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| name.eq_ignore_ascii_case(&group.name))
            })
            .and_then(|pool| pool.get("id").and_then(Value::as_u64))
            .ok_or_else(|| {
                format!(
                    "GitHub's runner service has no group named \"{}\".",
                    group.name
                )
            })
    }

    /// Registers one runner and writes its three files. `Ok` is GitHub's runner id.
    #[allow(clippy::too_many_arguments)]
    async fn register_one(
        &self,
        binding: &str,
        name: &str,
        labels: &[String],
        group: &RunnerGroup,
        pool_id: u64,
        org_url: &str,
        service_url: &str,
        admin: &Secret,
        registration: &Secret,
        admin_flow: bool,
    ) -> Result<u64, String> {
        let bits = self.inner.runner_key_bits;
        let key = tokio::task::spawn_blocking(move || RunnerKey::generate(bits))
            .await
            .map_err(|_| "a runner key could not be made".to_string())??;
        let label_json: Vec<Value> = labels
            .iter()
            .map(|label| json!({ "id": 0, "name": label, "type": "user" }))
            .collect();
        let http = &self.inner.http;
        let (runner_id, runner_file, credentials) = if admin_flow {
            let answer = http
                .send(
                    Method::POST,
                    &http.api("/actions/runners/register"),
                    Auth::RemoteAuth(registration),
                    Some(&json!({
                        "url": org_url,
                        "group_id": group.id,
                        "name": name,
                        "version": RUNNER_VERSION,
                        "updates_disabled": false,
                        "ephemeral": false,
                        "labels": label_json,
                        "public_key": key.public_xml(),
                    })),
                )
                .await
                .map_err(|why| format!("Arca could not register runner {name}: {why}."))?;
            if !answer.ok() {
                return Err(refused(&format!("register runner {name}"), &answer));
            }
            let runner = answer.json();
            let runner_id = runner
                .get("id")
                .and_then(Value::as_u64)
                .ok_or("GitHub's registration answer had no runner id")?;
            let authorization = runner.get("authorization").cloned().unwrap_or(Value::Null);
            let text = |field: &str| {
                authorization
                    .get(field)
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
            };
            let client_id =
                text("client_id").ok_or("GitHub's registration answer had no client id")?;
            let authorization_url = text("authorization_url")
                .ok_or("GitHub's registration answer had no token address")?;
            let server_url_v2 = text("server_url").unwrap_or_default();
            let mut data = json!({
                "clientId": client_id,
                "authorizationUrl": authorization_url,
                "requireFipsCryptography": "True",
            });
            if let Some(legacy) = text("legacy_authorization_url") {
                data["authorizationUrl"] = json!(legacy);
                data["enableAuthMigrationByDefault"] = json!("true");
                data["authorizationUrlV2"] = json!(authorization_url);
            }
            let runner_file = json!({
                "agentId": runner_id,
                "agentName": name,
                "poolId": group.id,
                "poolName": group.name,
                "serverUrl": service_url,
                "gitHubUrl": org_url,
                "workFolder": "_work",
                "useV2Flow": true,
                "useRunnerAdminFlow": true,
                "serverUrlV2": server_url_v2,
            });
            (
                runner_id,
                runner_file,
                json!({ "scheme": "OAuth", "data": data }),
            )
        } else {
            let answer = http
                .send(
                    Method::POST,
                    &format!(
                        "{service_url}/_apis/distributedtask/pools/{pool_id}/agents?api-version={ACTIONS_API_VERSION}"
                    ),
                    Auth::Bearer(admin),
                    Some(&json!({
                        "name": name,
                        "version": RUNNER_VERSION,
                        "osDescription": "Linux",
                        "enabled": true,
                        "maxParallelism": 1,
                        "ephemeral": false,
                        "disableUpdate": false,
                        "labels": label_json,
                        "authorization": { "publicKey": key.public_json() },
                    })),
                )
                .await
                .map_err(|why| format!("Arca could not register runner {name}: {why}."))?;
            if !answer.ok() {
                return Err(refused(&format!("register runner {name}"), &answer));
            }
            let agent = answer.json();
            let runner_id = agent
                .get("id")
                .and_then(Value::as_u64)
                .ok_or("GitHub's registration answer had no runner id")?;
            let authorization = agent.get("authorization").cloned().unwrap_or(Value::Null);
            let client_id = authorization
                .get("clientId")
                .and_then(Value::as_str)
                .ok_or("GitHub's registration answer had no client id")?;
            let authorization_url = authorization
                .get("authorizationUrl")
                .and_then(Value::as_str)
                .ok_or("GitHub's registration answer had no token address")?;
            let fips = agent
                .pointer("/properties/RequireFipsCryptography/$value")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let mut runner_file = json!({
                "agentId": runner_id,
                "agentName": name,
                "poolId": pool_id,
                "poolName": group.name,
                "serverUrl": service_url,
                "gitHubUrl": org_url,
                "workFolder": "_work",
            });
            if let Some(v2) = agent
                .pointer("/properties/ServerUrlV2/$value")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
            {
                runner_file["serverUrlV2"] = json!(v2);
            }
            let credentials = json!({
                "scheme": "OAuth",
                "data": {
                    "clientId": client_id,
                    "authorizationUrl": authorization_url,
                    "requireFipsCryptography": if fips { "True" } else { "False" },
                },
            });
            (runner_id, runner_file, credentials)
        };
        let folder = self.runner_dir(binding, name);
        let rsaparams = key.rsaparams()?;
        for (file, text) in [
            (".runner", runner_file.to_string()),
            (".credentials", credentials.to_string()),
            (".credentials_rsaparams", rsaparams.expose().to_string()),
        ] {
            write_private(&folder.join(file), text.as_bytes()).map_err(|error| error.message())?;
        }
        Ok(runner_id)
    }
}

/// The step's words for a refusal from GitHub: what Arca tried, GitHub's own words, and for an organisation that
/// restricts OAuth Apps, what the person does.
fn refused(what: &str, answer: &Answer) -> String {
    let why = answer.why();
    if answer.status == 403
        && why
            .to_ascii_lowercase()
            .contains("oauth app access restrictions")
    {
        return format!("Arca could not {what}. {RESTRICTED}");
    }
    format!("Arca could not {what}: {why}.")
}

/// GitHub's words for an OAuth answer that is not what was asked: its `error_description`, its `error`, or the status.
fn oauth_why(answer: &Answer) -> String {
    let body = answer.json();
    let text = body
        .get("error_description")
        .or_else(|| body.get("error"))
        .and_then(Value::as_str)
        .map(|text| text.chars().take(300).collect::<String>());
    match text {
        Some(text) => format!("GitHub answered {}: {text}", answer.status),
        None => answer.why(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_restricted_organisation_is_told_where_to_grant() {
        let answer = Answer {
            status: 403,
            body: br#"{"message":"Although you appear to have the correct authorization credentials, the `acme` organization has enabled OAuth App access restrictions, meaning that data access to third-parties is limited."}"#.to_vec(),
        };
        let text = refused("get a runner registration token", &answer);
        assert!(text.contains("Grant button"), "{text}");
        let answer = Answer {
            status: 422,
            body: br#"{"message":"Validation Failed"}"#.to_vec(),
        };
        assert_eq!(
            refused("register runner x", &answer),
            "Arca could not register runner x: GitHub answered 422: Validation Failed."
        );
    }
}
