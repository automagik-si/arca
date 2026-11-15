//! The App and push modes of the GitHub signer, and the mint every per-job runner comes from.
//!
//! **App (one click).** The person names the organisation and the labels on Arca's GitHub page and is sent to GitHub's
//! "Create GitHub App" screen with a manifest Arca wrote: the organisation permission "Self-hosted runners: write" and
//! nothing else, no webhook, private to the organisation. They click Create. GitHub sends the browser back to
//! `/approve/github/created` with a one-hour code, which Arca exchanges (`POST /app-manifests/{code}/conversions`) for the
//! App's id and private key; the key goes straight into the signer's store. The browser is then sent to the App's install
//! screen, the person picks repositories and installs, and GitHub sends the browser to `/approve/github/installed`. Arca
//! checks the installation with the App's own JWT, reads the chosen repositories and makes a runner group of exactly
//! those repositories, public repositories off.
//!
//! **Push.** The person's own service makes the just-in-time configurations with its own credential and pushes them to
//! `POST /github/intake/{binding}` with the binding's intake token. A mint hands out the oldest one whose labels match.
//!
//! **The mint.** [`Signer::mint`] is the one call the runner launcher makes per runner: the binding, a runner name, the
//! labels (exactly one of the binding's label sets) and, for a scale set, its id. It answers one runner's
//! `encoded_jit_config`. Every call is a line of the audit log, refused ones too. [`Signer::actions_connection`] gives
//! the launcher the Actions service connection a scale-set listener needs; the App's key and its installation token never
//! leave this module.
//!
//! **Revocation.** Uninstalling or deleting the App on GitHub ends it: the next mint finds out, marks the binding
//! revoked and mints nothing more. [`Signer::revoke`] does the same from Arca's side, uninstalls the App where it can,
//! and deletes the key.

use std::fs;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use reqwest::Method;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::github_api::{ACTIONS_API_VERSION, Answer, AppKey, Auth, Secret};
use crate::github_signer::{
    AppRecord, Binding, InstallationRecord, Mode, RunnerGroup, Signer, SignerError, Status,
    check_account, check_labels, fresh_id, same_labels, write_private,
};

/// The name of the runner group Arca makes for an App binding.
pub const RUNNER_GROUP_NAME: &str = "Arca runners";
/// The one permission the App asks for.
pub const APP_PERMISSION: &str = "organization_self_hosted_runners";
/// A manifest flow lives an hour, as GitHub's code does.
const APP_FLOW_TTL_MS: u64 = 60 * 60 * 1000;
/// An installation token lasts an hour; the signer uses one for fifty minutes.
const TOKEN_USE_MS: u64 = 50 * 60 * 1000;
/// An Actions connection is renewed five minutes before it ends.
const ACTIONS_MARGIN_MS: u64 = 5 * 60 * 1000;
/// A pushed configuration older than a day is dropped: GitHub will have removed its runner.
const PUSHED_KEEP_MS: u64 = 24 * 60 * 60 * 1000;
/// The most configurations one push binding queues.
const PUSHED_MAX: usize = 1000;
/// The longest encoded configuration taken.
const PUSHED_BYTES: usize = 64 * 1024;

/// A manifest flow in progress.
#[derive(Clone, Debug)]
pub struct AppFlow {
    pub account: String,
    pub label_sets: Vec<Vec<String>>,
    pub caller: String,
    pub created_at: u64,
    /// The binding the flow made once GitHub handed over the App.
    pub binding: Option<String>,
}

/// Where the person's browser goes to create the App: a form posted to `action_url` with one field, `manifest`.
#[derive(Clone, Debug)]
pub struct AppStart {
    pub action_url: String,
    pub manifest: String,
    pub state: String,
}

/// The Actions service connection of an App binding, for a scale-set listener in the kernel. The admin token never
/// leaves the kernel process.
#[derive(Clone, Debug)]
pub struct ActionsConnection {
    /// The Actions service's address, with no trailing slash.
    pub service_url: String,
    pub admin_token: Secret,
    /// When (ms) the signer stops handing this connection out.
    pub expires_at: u64,
}

/// The job a runner took, for the audit log.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobRef {
    /// `owner/name`.
    pub repo: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow_ref: Option<String>,
}

/// One runner asked for.
#[derive(Clone, Debug)]
pub struct MintRequest {
    pub binding: String,
    pub runner_name: String,
    /// Exactly one of the binding's label sets, in any order.
    pub labels: Vec<String>,
    /// `_work` when absent.
    pub work_folder: Option<String>,
    /// A scale set of the binding's organisation: the configuration is then the scale set's.
    pub scale_set_id: Option<u64>,
    pub job: Option<JobRef>,
    /// Who asks, for the audit log.
    pub caller: String,
}

/// One runner's configuration.
#[derive(Clone, Debug)]
pub struct Minted {
    /// The audit log's id for this mint; [`Signer::attach_job`] names it.
    pub mint_id: u64,
    pub runner_id: u64,
    pub runner_name: String,
    pub runner_group_id: Option<u64>,
    pub labels: Vec<String>,
    pub encoded_jit_config: Secret,
}

/// Why no runner was minted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MintRefused {
    UnknownBinding,
    /// The labels are none of the binding's sets.
    LabelsNotAllowed {
        allowed: Vec<Vec<String>>,
    },
    /// The App is made and not installed yet.
    NotInstalled,
    /// The binding is revoked; nothing is minted until it is connected again.
    Revoked {
        why: String,
    },
    /// GitHub suspended the installation. The next mint asks again.
    Suspended {
        why: String,
    },
    /// A push binding has no configuration with those labels queued.
    Empty,
    /// An OAuth binding registered lasting runners and mints none.
    NotMintable,
    /// The scale set is not in the connection's runner group: Arca mints only into the group made for the
    /// repositories the person picked. `scale_set_group` is the group GitHub says the scale set is in.
    ScaleSetOutsideGroup {
        scale_set: u64,
        scale_set_group: Option<u64>,
        group: Option<u64>,
    },
    /// GitHub refused or did not answer. `retry` says whether the same call may succeed later as it is.
    GitHub {
        status: Option<u16>,
        why: String,
        retry: bool,
    },
    Store(String),
}

impl MintRefused {
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownBinding => "UnknownBinding",
            Self::LabelsNotAllowed { .. } => "LabelsNotAllowed",
            Self::NotInstalled => "NotInstalled",
            Self::Revoked { .. } => "Revoked",
            Self::Suspended { .. } => "Suspended",
            Self::Empty => "Empty",
            Self::NotMintable => "NotMintable",
            Self::ScaleSetOutsideGroup { .. } => "ScaleSetOutsideGroup",
            Self::GitHub { .. } => "GitHubRefused",
            Self::Store(_) => "StoreFailed",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::UnknownBinding => "no GitHub connection has that id on this node".to_string(),
            Self::LabelsNotAllowed { allowed } => format!(
                "the labels are none of the connection's label sets: {}",
                allowed
                    .iter()
                    .map(|set| format!("[{}]", set.join(", ")))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            Self::NotInstalled => {
                "the App is made and not installed in the organisation yet".to_string()
            }
            Self::Revoked { why } | Self::Suspended { why } => why.clone(),
            Self::Empty => {
                "no pushed runner configuration with those labels is waiting".to_string()
            }
            Self::NotMintable => {
                "this connection registered lasting runners and mints none: use its runners' files"
                    .to_string()
            }
            Self::ScaleSetOutsideGroup {
                scale_set,
                scale_set_group,
                group,
            } => format!(
                "scale set {scale_set} is in runner group {}, not this connection's runner group {}: Arca mints only \
                 into the group made for the repositories the person picked",
                scale_set_group.map_or_else(|| "unknown".to_string(), |id| id.to_string()),
                group.map_or_else(|| "(none yet)".to_string(), |id| id.to_string()),
            ),
            Self::GitHub { why, .. } | Self::Store(why) => why.clone(),
        }
    }
}

impl From<SignerError> for MintRefused {
    fn from(error: SignerError) -> Self {
        match error {
            SignerError::UnknownBinding => Self::UnknownBinding,
            other => Self::Store(other.message()),
        }
    }
}

/// Why a pushed configuration was not taken: an HTTP status and words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntakeRefused {
    pub status: u16,
    pub why: String,
}

fn transport(why: String) -> MintRefused {
    MintRefused::GitHub {
        status: None,
        why,
        retry: true,
    }
}

fn github(answer: &Answer) -> MintRefused {
    MintRefused::GitHub {
        status: Some(answer.status),
        why: answer.why(),
        retry: answer.status >= 500 || answer.status == 429,
    }
}

/// A random 32-hex value.
fn fresh_state() -> Result<String, SignerError> {
    Ok(format!("{}{}", fresh_id("")?, fresh_id("")?))
}

impl Signer {
    // ---- The manifest flow --------------------------------------------------------------------------------------

    /// Starts a manifest flow for `account`, whose runners may carry `label_sets`. `origin` is the page's own origin,
    /// to which GitHub sends the browser back.
    pub fn app_begin(
        &self,
        account: &str,
        label_sets: &[Vec<String>],
        origin: &str,
        caller: &str,
    ) -> Result<AppStart, SignerError> {
        check_account(account)?;
        if label_sets.is_empty() || label_sets.len() > 10 {
            return Err(SignerError::Invalid(
                "labels: one to ten label sets".to_string(),
            ));
        }
        let label_sets = label_sets
            .iter()
            .map(|set| check_labels("labels", set))
            .collect::<Result<Vec<_>, _>>()?;
        let state = fresh_state()?;
        let manifest = json!({
            "name": format!("Arca runners {}", &state[..6]),
            "url": origin,
            "description": "Runs this organisation's GitHub Actions jobs on Arca. It registers and removes self-hosted runners, and nothing else.",
            "hook_attributes": { "url": format!("{origin}/approve/github/hook"), "active": false },
            "redirect_url": format!("{origin}/approve/github/created"),
            "setup_url": format!("{origin}/approve/github/installed"),
            "setup_on_update": true,
            "public": false,
            "default_permissions": { APP_PERMISSION: "write" },
            "default_events": [],
        });
        let now = self.now();
        {
            let mut state_lock = self.lock();
            state_lock
                .app_flows
                .retain(|_, flow| now.saturating_sub(flow.created_at) < APP_FLOW_TTL_MS);
            state_lock.app_flows.insert(
                state.clone(),
                AppFlow {
                    account: account.to_string(),
                    label_sets: label_sets.clone(),
                    caller: caller.to_string(),
                    created_at: now,
                    binding: None,
                },
            );
        }
        self.audit(json!({
            "kind": "appStart",
            "caller": caller,
            "account": account,
            "labelSets": label_sets,
        }));
        Ok(AppStart {
            action_url: self.inner.http.web(&format!(
                "/organizations/{account}/settings/apps/new?state={state}"
            )),
            manifest: manifest.to_string(),
            state,
        })
    }

    fn app_flow(&self, state: &str) -> Option<AppFlow> {
        let now = self.now();
        self.lock()
            .app_flows
            .get(state)
            .filter(|flow| now.saturating_sub(flow.created_at) < APP_FLOW_TTL_MS)
            .cloned()
    }

    /// GitHub's return after the person clicked Create: exchanges `code` for the App and its key, stores the key and
    /// answers the address of the App's install screen. `Err` is in words for the person.
    pub async fn app_created(&self, code: &str, state: &str) -> Result<String, String> {
        let flow = self.app_flow(state).ok_or(
            "This link is unknown or older than an hour. Start again from Arca's GitHub page.",
        )?;
        if code.is_empty()
            || code.len() > 200
            || !code
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err("GitHub's code in the link is not one GitHub gives.".to_string());
        }
        let answer = self
            .inner
            .http
            .send(
                Method::POST,
                &self
                    .inner
                    .http
                    .api(&format!("/app-manifests/{code}/conversions")),
                Auth::None,
                None,
            )
            .await
            .map_err(|why| format!("Arca could not reach GitHub to take the new App: {why}."))?;
        if !answer.ok() {
            return Err(format!(
                "GitHub did not hand the new App over: {}.",
                answer.why()
            ));
        }
        let app = answer.json();
        let field = |key: &str| app.get(key).and_then(Value::as_str).map(str::to_string);
        let (Some(id), Some(slug), Some(client_id), Some(pem)) = (
            app.get("id").and_then(Value::as_u64),
            field("slug"),
            field("client_id"),
            field("pem"),
        ) else {
            return Err(
                "GitHub's answer for the new App was missing its id, name or key.".to_string(),
            );
        };
        let owner_login = app
            .pointer("/owner/login")
            .and_then(Value::as_str)
            .unwrap_or(&flow.account)
            .to_string();
        let owner_id = app
            .pointer("/owner/id")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        AppKey::from_pem(&pem).map_err(|why| format!("GitHub's key for the new App: {why}."))?;
        let binding_id = fresh_id("gh-").map_err(|error| error.message())?;
        write_private(&self.key_path(&binding_id), pem.as_bytes())
            .map_err(|error| error.message())?;
        let now = self.now();
        let binding = Binding {
            id: binding_id.clone(),
            mode: Mode::App,
            account: owner_login.clone(),
            status: Status::AwaitingInstall,
            why: Some("The App is made; install it in the organisation to finish.".to_string()),
            label_sets: flow.label_sets.clone(),
            runner_group: None,
            runners: Vec::new(),
            app: Some(AppRecord {
                id,
                slug: slug.clone(),
                client_id,
                html_url: field("html_url").unwrap_or_default(),
                owner_id,
            }),
            installation: None,
            created_by: flow.caller.clone(),
            created_at: now,
            updated_at: now,
        };
        self.save(binding).map_err(|error| error.message())?;
        if let Some(flow) = self.lock().app_flows.get_mut(state) {
            flow.binding = Some(binding_id.clone());
        }
        self.audit(json!({
            "kind": "appCreated",
            "binding": binding_id,
            "caller": flow.caller,
            "account": owner_login,
            "appId": id,
            "appSlug": slug,
        }));
        let target = if owner_id > 0 {
            format!("/permissions?target_id={owner_id}&state={state}")
        } else {
            format!("?state={state}")
        };
        Ok(self
            .inner
            .http
            .web(&format!("/apps/{slug}/installations/new{target}")))
    }

    /// GitHub's return after the person installed (or changed) the App: checks the installation with the App's own JWT,
    /// records it, and puts the runner group in place. Answers the binding's id; `Err` is in words for the person.
    pub async fn app_installed(
        &self,
        installation_id: u64,
        state: Option<&str>,
    ) -> Result<String, String> {
        let from_flow = state
            .and_then(|state| self.app_flow(state))
            .and_then(|flow| flow.binding);
        let candidates: Vec<String> = match from_flow {
            Some(id) => vec![id],
            None => self
                .bindings()
                .into_iter()
                .filter(|binding| binding.mode == Mode::App && binding.status != Status::Revoked)
                .map(|binding| binding.id)
                .collect(),
        };
        for id in candidates {
            let Ok(jwt) = self.app_jwt(&id) else { continue };
            let answer = self
                .inner
                .http
                .send(
                    Method::GET,
                    &self
                        .inner
                        .http
                        .api(&format!("/app/installations/{installation_id}")),
                    Auth::Bearer(&jwt),
                    None,
                )
                .await
                .map_err(|why| {
                    format!("Arca could not reach GitHub to check the installation: {why}.")
                })?;
            if !answer.ok() {
                continue;
            }
            let installation = answer.json();
            let mut binding = self
                .lock()
                .bindings
                .get(&id)
                .cloned()
                .ok_or("the connection is gone")?;
            if let Some(login) = installation
                .pointer("/account/login")
                .and_then(Value::as_str)
            {
                binding.account = login.to_string();
            }
            binding.installation = Some(InstallationRecord {
                id: installation_id,
                repository_selection: installation
                    .get("repository_selection")
                    .and_then(Value::as_str)
                    .unwrap_or("selected")
                    .to_string(),
                repositories: Vec::new(),
            });
            binding.status = Status::Active;
            binding.why = None;
            binding.updated_at = self.now();
            self.save(binding.clone())
                .map_err(|error| error.message())?;
            self.lock().installation_tokens.remove(&id);
            self.audit(json!({
                "kind": "appInstalled",
                "binding": id,
                "account": binding.account,
                "installationId": installation_id,
            }));
            if let Err(why) = self.sync(&id).await {
                let mut binding = self
                    .lock()
                    .bindings
                    .get(&id)
                    .cloned()
                    .ok_or("the connection is gone")?;
                binding.why = Some(format!(
                    "Installed; the runner group is not in place yet: {why}"
                ));
                self.save(binding).map_err(|error| error.message())?;
            }
            if let Some(state) = state {
                self.lock().app_flows.remove(state);
            }
            return Ok(id);
        }
        Err(
            "No App Arca made answers for that installation. Start again from Arca's GitHub page."
                .to_string(),
        )
    }

    /// Reads the installation's repositories and puts the runner group in place: Arca's own group made with exactly
    /// those repositories (public repositories off) or brought up to date, or, when GitHub lets Arca make no group, the
    /// organisation's default group with a warning.
    pub async fn sync(&self, id: &str) -> Result<(), String> {
        let binding = self
            .lock()
            .bindings
            .get(id)
            .cloned()
            .ok_or("no such connection")?;
        let token = self
            .installation_token(id)
            .await
            .map_err(|refused| refused.message())?;
        let http = &self.inner.http;
        let mut names = Vec::new();
        let mut ids = Vec::new();
        for page in 1..=10 {
            let answer = http
                .send(
                    Method::GET,
                    &http.api(&format!(
                        "/installation/repositories?per_page=100&page={page}"
                    )),
                    Auth::Bearer(&token),
                    None,
                )
                .await?;
            if !answer.ok() {
                return Err(answer.why());
            }
            let body = answer.json();
            let repositories = body
                .get("repositories")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for repository in &repositories {
                if let (Some(id), Some(name)) = (
                    repository.get("id").and_then(Value::as_u64),
                    repository.get("full_name").and_then(Value::as_str),
                ) {
                    ids.push(id);
                    names.push(name.to_string());
                }
            }
            if repositories.len() < 100 {
                break;
            }
        }
        let org = binding.account.clone();
        let group = match binding.runner_group.clone() {
            Some(group) if group.created_by_arca => {
                let answer = http
                    .send(
                        Method::PUT,
                        &http.api(&format!(
                            "/orgs/{org}/actions/runner-groups/{}/repositories",
                            group.id
                        )),
                        Auth::Bearer(&token),
                        Some(&json!({ "selected_repository_ids": ids })),
                    )
                    .await?;
                if !answer.ok() {
                    return Err(answer.why());
                }
                group
            }
            Some(group) => group,
            None => self.make_group(&org, &ids, &token).await?,
        };
        let mut binding = self
            .lock()
            .bindings
            .get(id)
            .cloned()
            .ok_or("no such connection")?;
        if let Some(installation) = binding.installation.as_mut() {
            installation.repositories = names.clone();
        }
        binding.runner_group = Some(group.clone());
        binding.updated_at = self.now();
        self.save(binding).map_err(|error| error.message())?;
        self.audit(json!({
            "kind": "sync",
            "binding": id,
            "account": org,
            "runnerGroupId": group.id,
            "repositories": names,
        }));
        Ok(())
    }

    async fn make_group(
        &self,
        org: &str,
        ids: &[u64],
        token: &Secret,
    ) -> Result<RunnerGroup, String> {
        let http = &self.inner.http;
        let answer = http
            .send(
                Method::POST,
                &http.api(&format!("/orgs/{org}/actions/runner-groups")),
                Auth::Bearer(token),
                Some(&json!({
                    "name": RUNNER_GROUP_NAME,
                    "visibility": "selected",
                    "selected_repository_ids": ids,
                    "allows_public_repositories": false,
                })),
            )
            .await?;
        if answer.ok() {
            let body = answer.json();
            return Ok(RunnerGroup {
                id: body
                    .get("id")
                    .and_then(Value::as_u64)
                    .ok_or("GitHub's new group had no id")?,
                name: RUNNER_GROUP_NAME.to_string(),
                created_by_arca: true,
                warning: None,
            });
        }
        if answer.status == 409 || answer.status == 422 {
            // A group of that name is there: take it, and give it exactly these repositories.
            let listed = http
                .send(
                    Method::GET,
                    &http.api(&format!("/orgs/{org}/actions/runner-groups?per_page=100")),
                    Auth::Bearer(token),
                    None,
                )
                .await?;
            let found = listed
                .json()
                .get("runner_groups")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .find(|group| group.get("name").and_then(Value::as_str) == Some(RUNNER_GROUP_NAME))
                .and_then(|group| group.get("id").and_then(Value::as_u64));
            if let Some(group_id) = found {
                for (method, path, body) in [
                    (
                        Method::PATCH,
                        format!("/orgs/{org}/actions/runner-groups/{group_id}"),
                        json!({ "visibility": "selected", "allows_public_repositories": false }),
                    ),
                    (
                        Method::PUT,
                        format!("/orgs/{org}/actions/runner-groups/{group_id}/repositories"),
                        json!({ "selected_repository_ids": ids }),
                    ),
                ] {
                    let answer = http
                        .send(method, &http.api(&path), Auth::Bearer(token), Some(&body))
                        .await?;
                    if !answer.ok() {
                        return Err(answer.why());
                    }
                }
                return Ok(RunnerGroup {
                    id: group_id,
                    name: RUNNER_GROUP_NAME.to_string(),
                    created_by_arca: true,
                    warning: None,
                });
            }
        }
        Ok(RunnerGroup {
            id: 1,
            name: "Default".to_string(),
            created_by_arca: false,
            warning: Some(format!(
                "GitHub did not let Arca make a runner group ({}): the runners go into the organisation's default \
                 group, which every repository it admits can send jobs to. GitHub offers more groups from its Team \
                 plan up.",
                answer.why()
            )),
        })
    }

    // ---- Credentials, inside this module alone ----------------------------------------------------------------------

    fn key_path(&self, binding: &str) -> std::path::PathBuf {
        self.inner.dir.join("keys").join(format!("{binding}.pem"))
    }

    fn app_jwt(&self, binding: &str) -> Result<Secret, MintRefused> {
        let issuer = self
            .lock()
            .bindings
            .get(binding)
            .and_then(|binding| binding.app.as_ref().map(|app| app.client_id.clone()))
            .ok_or(MintRefused::UnknownBinding)?;
        let pem = fs::read_to_string(self.key_path(binding)).map_err(|_| MintRefused::Revoked {
            why: "Arca no longer holds this App's key.".to_string(),
        })?;
        let key = AppKey::from_pem(&pem).map_err(MintRefused::Store)?;
        key.jwt(&issuer, self.now() / 1000)
            .map_err(MintRefused::Store)
    }

    fn mark(&self, binding: &str, status: Status, why: &str) {
        let mut state = self.lock();
        let Some(mut record) = state.bindings.get(binding).cloned() else {
            return;
        };
        if record.status == status && record.why.as_deref() == Some(why) {
            return;
        }
        record.status = status;
        record.why = Some(why.to_string());
        record.updated_at = (self.inner.clock)();
        state.installation_tokens.remove(binding);
        state.actions.remove(binding);
        drop(state);
        if self.save(record).is_ok() {
            self.audit(
                json!({ "kind": "status", "binding": binding, "status": status, "why": why }),
            );
        }
    }

    /// An installation token of an App binding, from the cache or new. A refusal that says the App is gone marks the
    /// binding revoked; one that says it is suspended marks it suspended.
    async fn installation_token(&self, binding: &str) -> Result<Secret, MintRefused> {
        let now = self.now();
        let (installation, account) = {
            let state = self.lock();
            if let Some((token, until)) = state.installation_tokens.get(binding)
                && *until > now
            {
                return Ok(token.clone());
            }
            let record = state
                .bindings
                .get(binding)
                .ok_or(MintRefused::UnknownBinding)?;
            let installation = record
                .installation
                .as_ref()
                .map(|i| i.id)
                .ok_or(MintRefused::NotInstalled)?;
            (installation, record.account.clone())
        };
        let jwt = self.app_jwt(binding)?;
        let http = &self.inner.http;
        let answer = http
            .send(
                Method::POST,
                &http.api(&format!("/app/installations/{installation}/access_tokens")),
                Auth::Bearer(&jwt),
                None,
            )
            .await
            .map_err(transport)?;
        if answer.ok() {
            let token = answer
                .json()
                .get("token")
                .and_then(Value::as_str)
                .map(Secret::new)
                .ok_or_else(|| {
                    MintRefused::Store("GitHub's token answer had no token".to_string())
                })?;
            self.lock()
                .installation_tokens
                .insert(binding.to_string(), (token.clone(), now + TOKEN_USE_MS));
            return Ok(token);
        }
        let why = answer.why();
        let lower = why.to_ascii_lowercase();
        if answer.status == 403 && lower.contains("suspend") {
            let text =
                format!("The App's installation in {account} is suspended on GitHub ({why}).");
            self.mark(binding, Status::Suspended, &text);
            return Err(MintRefused::Suspended { why: text });
        }
        if answer.status == 401
            && (lower.contains("exp") || lower.contains("iat") || lower.contains("time"))
        {
            return Err(MintRefused::GitHub {
                status: Some(401),
                why: format!("{why}. This machine's clock may be wrong."),
                retry: true,
            });
        }
        if answer.status == 404 || answer.status == 401 {
            // Is the App still there? Its own record answers to its JWT while it exists.
            let app = http
                .send(Method::GET, &http.api("/app"), Auth::Bearer(&jwt), None)
                .await
                .map_err(transport)?;
            let text = if app.ok() {
                format!(
                    "The App was uninstalled from {account} on GitHub, so it mints nothing. Install it again from its page, or connect again."
                )
            } else {
                format!(
                    "GitHub no longer knows the App for {account} or its key: it was deleted. Connect again to go on."
                )
            };
            self.mark(binding, Status::Revoked, &text);
            return Err(MintRefused::Revoked { why: text });
        }
        Err(github(&answer))
    }

    /// The Actions service connection of an App binding: a registration token from the installation token, exchanged at
    /// GitHub's runner-registration call for the service's address and admin token. Kept until five minutes before it
    /// ends. In-process only.
    pub async fn actions_connection(
        &self,
        binding: &str,
    ) -> Result<ActionsConnection, MintRefused> {
        let now = self.now();
        let account = {
            let state = self.lock();
            if let Some(connection) = state.actions.get(binding)
                && connection.expires_at > now
            {
                return Ok(connection.clone());
            }
            let record = state
                .bindings
                .get(binding)
                .ok_or(MintRefused::UnknownBinding)?;
            match record.mode {
                Mode::App => {}
                Mode::Oauth => return Err(MintRefused::NotMintable),
                Mode::Push => return Err(MintRefused::Empty),
            }
            if record.status == Status::Revoked {
                return Err(MintRefused::Revoked {
                    why: record.why.clone().unwrap_or_default(),
                });
            }
            record.account.clone()
        };
        let token = self.installation_token(binding).await?;
        let http = &self.inner.http;
        let answer = http
            .send(
                Method::POST,
                &http.api(&format!(
                    "/orgs/{account}/actions/runners/registration-token"
                )),
                Auth::Bearer(&token),
                None,
            )
            .await
            .map_err(transport)?;
        if !answer.ok() {
            return Err(github(&answer));
        }
        let registration = answer
            .json()
            .get("token")
            .and_then(Value::as_str)
            .map(Secret::new)
            .ok_or_else(|| {
                MintRefused::Store("GitHub's registration answer had no token".to_string())
            })?;
        let answer = http
            .send(
                Method::POST,
                &http.api("/actions/runner-registration"),
                Auth::RemoteAuth(&registration),
                Some(&json!({ "url": format!("{}/{account}", http.endpoints().web), "runner_event": "register" })),
            )
            .await
            .map_err(transport)?;
        if !answer.ok() {
            return Err(github(&answer));
        }
        let body = answer.json();
        let service_url = body
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                MintRefused::Store("GitHub's runner service answer had no address".to_string())
            })?
            .trim_end_matches('/')
            .to_string();
        let admin = body.get("token").and_then(Value::as_str).ok_or_else(|| {
            MintRefused::Store("GitHub's runner service answer had no token".to_string())
        })?;
        let ends = jwt_expiry_ms(admin).unwrap_or(now + 60 * 60 * 1000);
        let connection = ActionsConnection {
            service_url,
            admin_token: Secret::new(admin),
            expires_at: ends.saturating_sub(ACTIONS_MARGIN_MS),
        };
        self.lock()
            .actions
            .insert(binding.to_string(), connection.clone());
        self.audit(json!({ "kind": "actionsConnection", "binding": binding, "account": account }));
        Ok(connection)
    }

    // ---- The mint ---------------------------------------------------------------------------------------------------

    /// One runner's just-in-time configuration for `request`. Every call is a line of the audit log.
    pub async fn mint(&self, request: MintRequest) -> Result<Minted, MintRefused> {
        let outcome = self.mint_inner(&request).await;
        let mint_id = match &outcome {
            Ok(minted) => minted.mint_id,
            Err(_) => 0,
        };
        let group = self
            .lock()
            .bindings
            .get(&request.binding)
            .and_then(|binding| binding.runner_group.as_ref().map(|group| group.id));
        let mut line = json!({
            "kind": "mint",
            "binding": request.binding,
            "caller": request.caller,
            "runnerName": request.runner_name,
            "labels": request.labels,
            "runnerGroupId": group,
            "scaleSetId": request.scale_set_id,
            "job": request.job,
        });
        match &outcome {
            Ok(minted) => {
                line["outcome"] = json!("minted");
                line["mintId"] = json!(mint_id);
                line["runnerId"] = json!(minted.runner_id);
                // The group the runner really joined (a scale set's own group, checked against the binding's).
                line["runnerGroupId"] = json!(minted.runner_group_id);
            }
            Err(refused) => {
                line["outcome"] = json!("refused");
                line["refusal"] = json!(refused.code());
                line["why"] = json!(refused.message());
                if let MintRefused::ScaleSetOutsideGroup {
                    scale_set_group, ..
                } = refused
                {
                    line["scaleSetRunnerGroupId"] = json!(scale_set_group);
                }
            }
        }
        self.audit(line);
        outcome
    }

    async fn mint_inner(&self, request: &MintRequest) -> Result<Minted, MintRefused> {
        let binding = self
            .lock()
            .bindings
            .get(&request.binding)
            .cloned()
            .ok_or(MintRefused::UnknownBinding)?;
        let labels = binding
            .label_sets
            .iter()
            .find(|set| same_labels(set, &request.labels))
            .cloned()
            .ok_or_else(|| MintRefused::LabelsNotAllowed {
                allowed: binding.label_sets.clone(),
            })?;
        let mint_id = loop {
            let mut bytes = [0u8; 8];
            getrandom::fill(&mut bytes)
                .map_err(|_| MintRefused::Store("no random number".to_string()))?;
            let id = u64::from_le_bytes(bytes) >> 11;
            if id != 0 {
                break id;
            }
        };
        match binding.mode {
            Mode::Oauth => Err(MintRefused::NotMintable),
            Mode::Push => self.pop_pushed(&binding, &labels, mint_id),
            Mode::App => {
                match binding.status {
                    Status::Revoked => {
                        return Err(MintRefused::Revoked {
                            why: binding.why.clone().unwrap_or_default(),
                        });
                    }
                    Status::AwaitingInstall => return Err(MintRefused::NotInstalled),
                    Status::Active | Status::Suspended => {}
                }
                let work_folder = request
                    .work_folder
                    .clone()
                    .unwrap_or_else(|| "_work".to_string());
                let http = &self.inner.http;
                let group = binding.runner_group.as_ref().map(|group| group.id);
                let (answer, config_key) = match request.scale_set_id {
                    Some(scale_set) => {
                        let connection = self.actions_connection(&binding.id).await?;
                        // The admin token reaches every scale set of the organisation: mint only for one in this
                        // binding's own runner group.
                        let read = http
                            .send(
                                Method::GET,
                                &format!(
                                    "{}/_apis/runtime/runnerscalesets/{scale_set}?api-version={ACTIONS_API_VERSION}",
                                    connection.service_url
                                ),
                                Auth::Bearer(&connection.admin_token),
                                None,
                            )
                            .await
                            .map_err(transport)?;
                        if !read.ok() {
                            if read.status == 401 {
                                self.lock().actions.remove(&binding.id);
                            }
                            return Err(github(&read));
                        }
                        let scale_set_group =
                            read.json().get("runnerGroupId").and_then(Value::as_u64);
                        if group.is_none() || scale_set_group != group {
                            return Err(MintRefused::ScaleSetOutsideGroup {
                                scale_set,
                                scale_set_group,
                                group,
                            });
                        }
                        let answer = http
                            .send(
                                Method::POST,
                                &format!(
                                    "{}/_apis/runtime/runnerscalesets/{scale_set}/generatejitconfig?api-version={ACTIONS_API_VERSION}",
                                    connection.service_url
                                ),
                                Auth::Bearer(&connection.admin_token),
                                Some(&json!({ "name": request.runner_name, "workFolder": work_folder })),
                            )
                            .await
                            .map_err(transport)?;
                        (answer, "encodedJITConfig")
                    }
                    None => {
                        let group = binding
                            .runner_group
                            .as_ref()
                            .map(|group| group.id)
                            .ok_or_else(|| MintRefused::GitHub {
                                status: None,
                                why: "the connection has no runner group yet".to_string(),
                                retry: true,
                            })?;
                        let token = self.installation_token(&binding.id).await?;
                        let answer = http
                            .send(
                                Method::POST,
                                &http.api(&format!(
                                    "/orgs/{}/actions/runners/generate-jitconfig",
                                    binding.account
                                )),
                                Auth::Bearer(&token),
                                Some(&json!({
                                    "name": request.runner_name,
                                    "runner_group_id": group,
                                    "labels": labels,
                                    "work_folder": work_folder,
                                })),
                            )
                            .await
                            .map_err(transport)?;
                        (answer, "encoded_jit_config")
                    }
                };
                if !answer.ok() {
                    if answer.status == 401 {
                        self.lock().actions.remove(&binding.id);
                        self.lock().installation_tokens.remove(&binding.id);
                    }
                    return Err(github(&answer));
                }
                let body = answer.json();
                let config = body
                    .get(config_key)
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        MintRefused::Store(
                            "GitHub's answer had no runner configuration".to_string(),
                        )
                    })?;
                if binding.status == Status::Suspended {
                    let mut record = binding.clone();
                    record.status = Status::Active;
                    record.why = None;
                    record.updated_at = self.now();
                    let _ = self.save(record);
                }
                Ok(Minted {
                    mint_id,
                    runner_id: body
                        .pointer("/runner/id")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    runner_name: body
                        .pointer("/runner/name")
                        .and_then(Value::as_str)
                        .unwrap_or(&request.runner_name)
                        .to_string(),
                    runner_group_id: group,
                    labels,
                    encoded_jit_config: Secret::new(config),
                })
            }
        }
    }

    /// Records the job a minted runner took.
    pub fn attach_job(&self, mint_id: u64, job: &JobRef) {
        self.audit(json!({ "kind": "job", "mintId": mint_id, "job": job }));
    }

    // ---- Push -------------------------------------------------------------------------------------------------------

    fn intake_digest_path(&self, binding: &str) -> std::path::PathBuf {
        self.inner
            .dir
            .join("intake")
            .join(format!("{binding}.sha256"))
    }

    fn queue_dir(&self, binding: &str) -> std::path::PathBuf {
        self.inner.dir.join("intake").join(binding)
    }

    /// Makes a push binding for `account` and answers its intake token, shown this once.
    pub fn push_create(
        &self,
        account: &str,
        label_sets: &[Vec<String>],
        caller: &str,
    ) -> Result<(Binding, Secret), SignerError> {
        check_account(account)?;
        if label_sets.is_empty() || label_sets.len() > 10 {
            return Err(SignerError::Invalid(
                "labels: one to ten label sets".to_string(),
            ));
        }
        let label_sets = label_sets
            .iter()
            .map(|set| check_labels("labels", set))
            .collect::<Result<Vec<_>, _>>()?;
        let id = fresh_id("gh-")?;
        let token = Secret::new(format!(
            "arcaintake_{}{}{}{}",
            fresh_id("")?,
            fresh_id("")?,
            fresh_id("")?,
            fresh_id("")?
        ));
        write_private(
            &self.intake_digest_path(&id),
            hex_sha256(token.expose()).as_bytes(),
        )?;
        let now = self.now();
        let binding = Binding {
            id: id.clone(),
            mode: Mode::Push,
            account: account.to_string(),
            status: Status::Active,
            why: None,
            label_sets: label_sets.clone(),
            runner_group: None,
            runners: Vec::new(),
            app: None,
            installation: None,
            created_by: caller.to_string(),
            created_at: now,
            updated_at: now,
        };
        self.save(binding.clone())?;
        self.audit(json!({
            "kind": "connect",
            "binding": id,
            "mode": "push",
            "caller": caller,
            "account": account,
            "labelSets": label_sets,
        }));
        Ok((binding, token))
    }

    /// Takes one pushed configuration for a push binding. `presented` is the bearer the caller sent.
    pub fn intake(
        &self,
        binding: &str,
        presented: &str,
        body: &Value,
    ) -> Result<usize, IntakeRefused> {
        let not_found = || IntakeRefused {
            status: 404,
            why: "no push connection of that id".to_string(),
        };
        let record = self
            .lock()
            .bindings
            .get(binding)
            .cloned()
            .ok_or_else(not_found)?;
        if record.mode != Mode::Push {
            return Err(not_found());
        }
        let stored = fs::read_to_string(self.intake_digest_path(binding)).unwrap_or_default();
        let given = hex_sha256(presented);
        let same = stored.len() == given.len()
            && stored
                .bytes()
                .zip(given.bytes())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0;
        if stored.is_empty() || !same {
            return Err(IntakeRefused {
                status: 401,
                why: "the intake token is not this connection's".to_string(),
            });
        }
        if record.status == Status::Revoked {
            return Err(IntakeRefused {
                status: 410,
                why: "this connection is disconnected".to_string(),
            });
        }
        let config = body
            .get("encodedJitConfig")
            .and_then(Value::as_str)
            .filter(|config| !config.is_empty() && config.len() <= PUSHED_BYTES)
            .ok_or_else(|| IntakeRefused {
                status: 400,
                why: "encodedJitConfig: GitHub's encoded_jit_config, as a string of at most 64 KiB"
                    .to_string(),
            })?;
        let labels: Vec<String> = body
            .get("labels")
            .and_then(Value::as_array)
            .map(|labels| {
                labels
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let labels = record
            .label_sets
            .iter()
            .find(|set| same_labels(set, &labels))
            .cloned()
            .ok_or_else(|| IntakeRefused {
                status: 422,
                why: format!(
                    "labels: none of this connection's label sets: {}",
                    record
                        .label_sets
                        .iter()
                        .map(|set| format!("[{}]", set.join(", ")))
                        .collect::<Vec<_>>()
                        .join(" ")
                ),
            })?;
        let facts = jit_facts(config);
        let folder = self.queue_dir(binding);
        let queued = fs::read_dir(&folder)
            .map(|entries| entries.count())
            .unwrap_or(0);
        if queued >= PUSHED_MAX {
            return Err(IntakeRefused {
                status: 429,
                why: format!("{PUSHED_MAX} configurations are already waiting"),
            });
        }
        let now = self.now();
        let name = format!(
            "{now:015}-{}.json",
            fresh_id("").map_err(|error| IntakeRefused {
                status: 500,
                why: error.message()
            })?
        );
        let entry = json!({
            "receivedAt": now,
            "labels": labels,
            "encodedJitConfig": config,
            "runnerName": facts.get("name").cloned().or_else(|| body.get("runnerName").cloned()),
            "runnerId": facts.get("id").cloned(),
            "poolId": facts.get("pool").cloned(),
        });
        write_private(&folder.join(name), entry.to_string().as_bytes()).map_err(|error| {
            IntakeRefused {
                status: 500,
                why: error.message(),
            }
        })?;
        self.audit(json!({
            "kind": "intake",
            "binding": binding,
            "account": record.account,
            "labels": labels,
            "runnerName": entry["runnerName"],
            "runnerId": entry["runnerId"],
        }));
        Ok(queued + 1)
    }

    fn pop_pushed(
        &self,
        binding: &Binding,
        labels: &[String],
        mint_id: u64,
    ) -> Result<Minted, MintRefused> {
        if binding.status == Status::Revoked {
            return Err(MintRefused::Revoked {
                why: binding.why.clone().unwrap_or_default(),
            });
        }
        let folder = self.queue_dir(&binding.id);
        let mut names: Vec<String> = fs::read_dir(&folder)
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
                    .filter(|name| name.ends_with(".json") && !name.starts_with('.'))
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        let now = self.now();
        for name in names {
            let path = folder.join(&name);
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(entry) = serde_json::from_str::<Value>(&text) else {
                let _ = fs::remove_file(&path);
                continue;
            };
            let received = entry.get("receivedAt").and_then(Value::as_u64).unwrap_or(0);
            if now.saturating_sub(received) > PUSHED_KEEP_MS {
                let _ = fs::remove_file(&path);
                self.audit(json!({ "kind": "intakeExpired", "binding": binding.id, "runnerName": entry["runnerName"] }));
                continue;
            }
            let entry_labels: Vec<String> = entry
                .get("labels")
                .and_then(Value::as_array)
                .map(|labels| {
                    labels
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            if !same_labels(&entry_labels, labels) {
                continue;
            }
            if fs::remove_file(&path).is_err() {
                continue;
            }
            let Some(config) = entry.get("encodedJitConfig").and_then(Value::as_str) else {
                continue;
            };
            return Ok(Minted {
                mint_id,
                runner_id: entry.get("runnerId").and_then(Value::as_u64).unwrap_or(0),
                runner_name: entry
                    .get("runnerName")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                runner_group_id: entry.get("poolId").and_then(Value::as_u64),
                labels: entry_labels,
                encoded_jit_config: Secret::new(config),
            });
        }
        Err(MintRefused::Empty)
    }

    // ---- Revocation ---------------------------------------------------------------------------------------------------

    /// Disconnects a binding from Arca's side. An App is uninstalled from the organisation where GitHub lets it, and
    /// its key is deleted; a push binding's token and queue are deleted; an OAuth binding's runner files are deleted.
    /// Answers the binding as it now is.
    pub async fn revoke(&self, id: &str, caller: &str) -> Result<Binding, SignerError> {
        let mut binding = self
            .lock()
            .bindings
            .get(id)
            .cloned()
            .ok_or(SignerError::UnknownBinding)?;
        let mut said = format!("Disconnected in Arca by {caller}.");
        match binding.mode {
            Mode::App => {
                if let (Some(installation), Ok(jwt)) =
                    (binding.installation.as_ref(), self.app_jwt(id))
                {
                    let answer = self
                        .inner
                        .http
                        .send(
                            Method::DELETE,
                            &self
                                .inner
                                .http
                                .api(&format!("/app/installations/{}", installation.id)),
                            Auth::Bearer(&jwt),
                            None,
                        )
                        .await;
                    match answer {
                        Ok(answer) if answer.status == 204 || answer.status == 404 => {
                            said.push_str(" The App is uninstalled from the organisation.");
                        }
                        Ok(answer) => said.push_str(&format!(
                            " GitHub did not uninstall the App ({}); uninstall it under the organisation's Settings, GitHub Apps.",
                            answer.why()
                        )),
                        Err(why) => said.push_str(&format!(
                            " GitHub could not be asked to uninstall the App ({why}); uninstall it under the organisation's Settings, GitHub Apps."
                        )),
                    }
                }
                let _ = fs::remove_file(self.key_path(id));
                if let Some(app) = &binding.app {
                    said.push_str(&format!(
                        " Arca deleted the App's key. To delete the App itself: {}",
                        self.inner.http.web(&format!(
                            "/organizations/{}/settings/apps/{}/advanced",
                            binding.account, app.slug
                        ))
                    ));
                }
            }
            Mode::Push => {
                let _ = fs::remove_file(self.intake_digest_path(id));
                let _ = fs::remove_dir_all(self.queue_dir(id));
                said.push_str(" Its intake token and waiting configurations are deleted.");
            }
            Mode::Oauth => {
                let _ = fs::remove_dir_all(self.inner.dir.join("runners").join(id));
                said.push_str(
                    " Its runners' credentials are deleted here. The runners stay listed on GitHub, offline, until you \
                     remove them under the organisation's Settings, Actions, Runners.",
                );
            }
        }
        binding.status = Status::Revoked;
        binding.why = Some(said.clone());
        binding.updated_at = self.now();
        {
            let mut state = self.lock();
            state.installation_tokens.remove(id);
            state.actions.remove(id);
        }
        self.save(binding.clone())?;
        self.audit(json!({ "kind": "revoke", "binding": id, "caller": caller, "why": said }));
        Ok(binding)
    }
}

/// `exp` of a JWT, in milliseconds, without checking it: only to know when to renew.
fn jwt_expiry_ms(token: &str) -> Option<u64> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    claims.get("exp")?.as_u64().map(|exp| exp * 1000)
}

fn hex_sha256(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The runner's id, name and pool from an encoded configuration's `.runner` file, best effort and never the
/// credential files.
fn jit_facts(encoded: &str) -> serde_json::Map<String, Value> {
    let mut facts = serde_json::Map::new();
    let Some(outer) = STANDARD
        .decode(encoded.trim())
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
    else {
        return facts;
    };
    let Some(runner) = outer
        .get(".runner")
        .and_then(Value::as_str)
        .and_then(|text| STANDARD.decode(text).ok())
        .and_then(|bytes| {
            let bytes = bytes
                .strip_prefix(b"\xef\xbb\xbf".as_slice())
                .unwrap_or(&bytes)
                .to_vec();
            serde_json::from_slice::<Value>(&bytes).ok()
        })
    else {
        return facts;
    };
    let pick = |names: [&str; 2]| names.iter().find_map(|name| runner.get(*name).cloned());
    if let Some(id) = pick(["agentId", "AgentId"]) {
        facts.insert("id".to_string(), id);
    }
    if let Some(name) = pick(["agentName", "AgentName"]) {
        facts.insert("name".to_string(), name);
    }
    if let Some(pool) = pick(["poolId", "PoolId"]) {
        facts.insert("pool".to_string(), pool);
    }
    facts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pushed_configurations_runner_facts_are_read_without_its_credentials() {
        let runner = STANDARD.encode(br#"{"agentId":42,"agentName":"acme-1","poolId":7}"#);
        let outer = json!({ ".runner": runner, ".credentials": "c2VjcmV0" }).to_string();
        let facts = jit_facts(&STANDARD.encode(outer));
        assert_eq!(facts.get("id"), Some(&json!(42)));
        assert_eq!(facts.get("name"), Some(&json!("acme-1")));
        assert_eq!(facts.get("pool"), Some(&json!(7)));
        assert!(jit_facts("not base64 at all").is_empty());
    }

    #[test]
    fn a_jwts_expiry_is_read_for_renewal_only() {
        let payload = URL_SAFE_NO_PAD.encode(br#"{"exp":1700000000}"#);
        assert_eq!(
            jwt_expiry_ms(&format!("h.{payload}.s")),
            Some(1_700_000_000_000)
        );
        assert_eq!(jwt_expiry_ms("opaque"), None);
    }
}
