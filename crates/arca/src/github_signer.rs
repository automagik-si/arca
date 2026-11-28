//! The GitHub signer: the one part of Arca that holds a GitHub credential for runners, and the only thing it does with it
//! is make runner registrations for the runner group and labels a person chose.
//!
//! A person connects a GitHub organisation in one of three ways, each a *binding*:
//!
//! - **OAuth (device flow)**: the person signs in to GitHub with a code Arca shows (`github.com/login/device`), granting
//!   Arca's OAuth App the `admin:org` scope once. The signer uses that grant for one thing: a runner registration token,
//!   with which it registers the runners the person asked for, each with its own key; then it deletes the grant at once
//!   (`DELETE /applications/{client_id}/grant`). What stays is the runners' own credentials, nothing for the organisation.
//!   See `crate::github_oauth`.
//! - **App** (one click): a GitHub App created in the person's organisation through GitHub's manifest flow, with the
//!   organisation permission "Self-hosted runners: write". Its private key lives here and mints a just-in-time runner
//!   configuration per job.
//! - **Push**: the person's own service generates the just-in-time configurations and pushes them to Arca; the signer
//!   holds no GitHub credential at all.
//!
//! The store is `<stateDir>/github`, mode 0700, every file in it 0600 and written by rename. No verb, page, log or journal
//! returns a key, a grant or a GitHub token, with two exceptions: `github_runner_mint` returns a runner's just-in-time
//! configuration, which is that runner's credential, to any `operator` token, and the page shows a push connection's
//! intake token once, when it is made. The binding records hold none. Every registration and every
//! mint is a line of `<stateDir>/github/audit.jsonl`: who asked, when, for which binding, runner group and labels, and the
//! outcome.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{self, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::github_api::{Endpoints, GitHubHttp, Secret};
use crate::handlers::Clock;

/// The signer's directory under the state directory.
pub const STORE_DIR: &str = "github";
/// The audit log, one JSON object per line.
pub const AUDIT_FILE: &str = "audit.jsonl";
/// Past this size the audit log is set aside as `audit.jsonl.1`, replacing the one set aside before.
const AUDIT_MAX_BYTES: u64 = 8 * 1024 * 1024;
/// The OAuth App's public record (its client ID) and its secret, apart.
const OAUTH_APP_FILE: &str = "oauth-app.json";
/// How long a pool's sign-in waits for the runner group's sync before it goes on with the group as recorded.
pub const SIGN_IN_SYNC_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);
/// After a failed sync the node retries it by itself: first after a minute, then doubling, up to ten minutes.
pub const SYNC_RETRY: (std::time::Duration, std::time::Duration) = (
    std::time::Duration::from_secs(60),
    std::time::Duration::from_secs(600),
);
const OAUTH_SECRET_FILE: &str = "oauth-app.secret";

/// How a binding reaches GitHub.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Runners registered once through a one-time OAuth grant; each holds its own credential.
    Oauth,
    /// A GitHub App made in the person's organisation; the signer mints a just-in-time runner per request.
    App,
    /// The person's own service pushes just-in-time configurations; the signer holds no GitHub credential.
    Push,
}

/// Where a binding stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Status {
    /// An App was made and is not installed yet.
    AwaitingInstall,
    /// Usable.
    Active,
    /// GitHub suspended the App's installation; the next mint asks GitHub again.
    Suspended,
    /// Taken away: on GitHub (the App uninstalled or deleted) or in Arca (disconnected). Nothing is minted.
    Revoked,
}

/// The runner group a binding registers into.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunnerGroup {
    pub id: u64,
    pub name: String,
    /// Whether Arca made the group (and so may change its repositories).
    pub created_by_arca: bool,
    /// What the person should know about it, in words, such as that it is the organisation's default group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    /// The group's own repositories as read from GitHub, when they are not the installation's (an owner's own way
    /// around issue 328, kept as it is).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repositories: Option<Vec<String>>,
    /// Which repositories may send jobs to the group, as Arca last set or read it. A record from before Arca kept it
    /// has none; [`Binding::group_access`] then says what it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<GroupAccess>,
}

/// Which repositories of the organisation may send jobs to a binding's runner group.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GroupAccess {
    /// Every private repository of the organisation, public ones off: Arca's group when the installation lists no
    /// repository (an App with only the organisation permission is given none).
    AllPrivate,
    /// Exactly the repositories the installation lists.
    Selected,
    /// A group Arca did not make: Arca leaves it as it is and cannot tell what it is open to.
    Unknown,
    /// No repository at all, so no job ever reaches the runners: Arca's group as Arca before issue 328 left it for an
    /// installation that lists none. The next sync opens it.
    #[serde(rename = "none")]
    NoRepository,
    /// The last sync failed, so Arca has not checked what the group is open to; [`Binding::sync_error`] says why.
    Unchecked,
}

/// A runner registered for an OAuth binding. Its credential is in the store, never in this record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisteredRunner {
    pub name: String,
    pub id: u64,
    pub labels: Vec<String>,
}

/// One connection to one GitHub organisation. It holds no secret.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Binding {
    pub id: String,
    pub mode: Mode,
    /// A separate App for workspace repository credentials, never runner credentials.
    #[serde(default, skip_serializing_if = "is_false")]
    pub repository_access: bool,
    /// The organisation's login.
    pub account: String,
    pub status: Status,
    /// Why the status is what it is, in words, when it is not `active`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    /// The label sets a runner of this binding may carry, each exactly as the person chose it.
    pub label_sets: Vec<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner_group: Option<RunnerGroup>,
    /// The runners an OAuth binding registered.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runners: Vec<RegisteredRunner>,
    /// The GitHub App of an App binding. Its key is in the store, never here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<AppRecord>,
    /// The App's installation in the organisation, once installed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installation: Option<InstallationRecord>,
    /// Why the last sync of the runner group failed, in words, until one succeeds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_error: Option<String>,
    /// When a sync failed: when the node's own retry runs (ms since the epoch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_retry_at: Option<u64>,
    /// Who made it: a token id, or `page` for the owner's page.
    pub created_by: String,
    pub created_at: u64,
    pub updated_at: u64,
}

fn is_false(value: &bool) -> bool {
    !value
}

impl Binding {
    /// The organisation's address on GitHub's web host, as a runner's `gitHubUrl` names it.
    pub fn org_url(&self, endpoints: &Endpoints) -> String {
        format!("{}/{}", endpoints.web, self.account)
    }

    /// Which repositories may send jobs to the runner group: as recorded, or, for a record from before Arca kept it,
    /// what Arca's code then made (an App's own group open to exactly the installation's repositories, which is to
    /// none when it lists none; a signed-in connection's own group open to every private repository).
    pub fn group_access(&self) -> Option<GroupAccess> {
        let group = self.runner_group.as_ref()?;
        if self.sync_error.is_some() {
            return Some(GroupAccess::Unchecked);
        }
        if let Some(access) = group.access {
            return Some(access);
        }
        Some(match (group.created_by_arca, self.mode) {
            (false, _) => GroupAccess::Unknown,
            (true, Mode::Oauth) => GroupAccess::AllPrivate,
            (true, _) => match &self.installation {
                Some(installation) if installation.repositories.is_empty() => {
                    GroupAccess::NoRepository
                }
                _ => GroupAccess::Selected,
            },
        })
    }

    /// Whether the runner group is open to no repository, so no job reaches the runners: Arca's group as Arca before
    /// issue 328 left it, or a `selected` group whose installation now lists none.
    pub fn open_to_none(&self) -> bool {
        match self.group_access() {
            Some(GroupAccess::NoRepository) => true,
            Some(GroupAccess::Selected) => self.group_repositories().is_empty(),
            _ => false,
        }
    }

    /// The repositories a `selected` group is open to: its own as read when the owner kept them, else the
    /// installation's.
    fn group_repositories(&self) -> Vec<String> {
        self.runner_group
            .as_ref()
            .and_then(|group| group.repositories.clone())
            .or_else(|| {
                self.installation
                    .as_ref()
                    .map(|installation| installation.repositories.clone())
            })
            .unwrap_or_default()
    }

    /// Whether the page and `github_bindings` should say the group's access as the connection's `why`: when no job can
    /// reach it, or Arca could not check it.
    pub fn group_access_is_news(&self) -> bool {
        self.open_to_none() || self.group_access() == Some(GroupAccess::Unchecked)
    }

    /// [`Binding::group_access`] in words for the person.
    pub fn group_access_words(&self) -> Option<String> {
        let group = self.runner_group.as_ref()?;
        let org = &self.account;
        Some(match self.group_access()? {
            GroupAccess::AllPrivate => format!(
                "Every private and internal repository of {org} may send jobs here. Narrow it in the runner group on \
                 GitHub."
            ),
            GroupAccess::Selected if self.open_to_none() => format!(
                "The runner group \"{}\" is open to no repository: the App's installation on GitHub lists none, so no \
                 job reaches these runners. Give the installation the repositories of {org} that should send jobs here.",
                group.name
            ),
            GroupAccess::Selected => format!(
                "Only these repositories of {org} may send jobs here: {}.",
                self.group_repositories().join(", ")
            ),
            GroupAccess::Unknown => format!(
                "Arca leaves the runner group \"{}\" as GitHub has it, so it cannot tell which repositories of {org} may send \
                 jobs here. See the group's repository access on GitHub.",
                group.name
            ),
            GroupAccess::Unchecked => format!(
                "Arca has not checked yet which repositories of {org} may send jobs to the runner group \"{}\": {}.{}",
                group.name,
                self.sync_error
                    .as_deref()
                    .unwrap_or("the last check failed"),
                // Only an active connection is checked again.
                match (self.status, self.sync_retry_at) {
                    (Status::Active, Some(at)) =>
                        format!(" Arca checks again at {}.", utc_minute(at)),
                    (Status::Active, None) => {
                        " Arca checks again when a pool signs in or GitHub's page returns."
                            .to_string()
                    }
                    _ => String::new(),
                }
            ),
            GroupAccess::NoRepository => format!(
                "The runner group \"{}\" is open to no repository, so no job reaches these runners. Arca opens it to \
                 every private and internal repository of {org} at its next sync: when the pool starts or GitHub's page returns.",
                group.name
            ),
        })
    }
}

/// A GitHub App Arca made through the manifest flow. Public facts only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppRecord {
    pub id: u64,
    pub slug: String,
    pub client_id: String,
    /// The App's page on GitHub.
    pub html_url: String,
    /// The organisation's numeric id, which the install page is opened for.
    pub owner_id: u64,
    /// The bot user ID verified through GitHub, never inferred from the App ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_id: Option<u64>,
    /// The bot login returned with that ID by GitHub.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_login: Option<String>,
}

/// An App's installation in the organisation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallationRecord {
    pub id: u64,
    /// `selected` or `all`, as the person chose on GitHub.
    pub repository_selection: String,
    /// The repositories the installation reaches, `owner/name`, as last read.
    pub repositories: Vec<String>,
}

/// What the runner launcher reads of a binding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindingInfo {
    pub id: String,
    pub mode: Mode,
    pub status: Status,
    pub org_url: String,
    pub runner_group_id: Option<u64>,
    pub label_sets: Vec<Vec<String>>,
}

/// A time (ms since the epoch) as "HH:MM UTC".
fn utc_minute(at: u64) -> String {
    let minute = at / 60_000 % 1440;
    format!("{:02}:{:02} UTC", minute / 60, minute % 60)
}

/// A registered runner's three files, as the runner reads them from its own folder. They are its lasting credential.
#[derive(Debug)]
pub struct RunnerFiles {
    /// `.runner`: its id, name, runner group and the service it talks to. Not secret by itself.
    pub runner: String,
    /// `.credentials`: the OAuth client id and token address the runner signs in to.
    pub credentials: Secret,
    /// `.credentials_rsaparams`: the runner's private key.
    pub rsaparams: Secret,
}

/// Why the signer refused or failed. Every text is in words and holds no credential; a text that quotes GitHub says so.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignerError {
    UnknownBinding,
    UnknownRunner,
    /// The call's arguments, with the path and the reason.
    Invalid(String),
    /// The OAuth App is not set on this node.
    NoOauthApp,
    /// The store could not be read or written.
    Store(String),
}

impl SignerError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownBinding => "UnknownBinding",
            Self::UnknownRunner => "UnknownRunner",
            Self::Invalid(_) => "Invalid",
            Self::NoOauthApp => "NoOauthApp",
            Self::Store(_) => "StoreFailed",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::UnknownBinding => "no GitHub connection has that id on this node".to_string(),
            Self::UnknownRunner => "that connection registered no runner of that name".to_string(),
            Self::Invalid(why) | Self::Store(why) => why.clone(),
            Self::NoOauthApp => "this node has no GitHub OAuth App set: github_oauth_app_set sets its client ID and secret"
                .to_string(),
        }
    }
}

/// The signer. Cheap to clone; every clone shares one store and one state.
#[derive(Clone)]
pub struct Signer {
    pub(crate) inner: Arc<Inner>,
}

pub(crate) struct Inner {
    pub(crate) dir: PathBuf,
    pub(crate) http: GitHubHttp,
    pub(crate) clock: Clock,
    /// The bits of the runner keys the signer makes.
    pub(crate) runner_key_bits: usize,
    /// How long a pool's sign-in waits for the runner group's sync.
    pub(crate) sign_in_deadline: std::time::Duration,
    /// After a failed sync: the first wait before the node's own retry, and the most it doubles to.
    pub(crate) sync_retry: (std::time::Duration, std::time::Duration),
    pub(crate) state: Mutex<State>,
    /// Held while a line is appended to the audit log, so lines never interleave.
    audit: Mutex<()>,
}

#[derive(Default)]
pub(crate) struct State {
    pub(crate) bindings: BTreeMap<String, Binding>,
    pub(crate) oauth_flows: BTreeMap<String, crate::github_oauth::OauthFlow>,
    pub(crate) device_codes: HashMap<String, Secret>,
    /// Manifest flows in progress, by their `state` value.
    pub(crate) app_flows: HashMap<String, crate::github_app::AppFlow>,
    /// Installation tokens, by binding, with the time (ms) they are used until.
    pub(crate) installation_tokens: HashMap<String, (Secret, u64)>,
    /// Actions service connections, by binding.
    pub(crate) actions: HashMap<String, crate::github_app::ActionsConnection>,
    /// When a sign-in or retry sync of each binding's runner group last succeeded (ms), behind a lock that lets one run
    /// at a time.
    pub(crate) sign_in_syncs: HashMap<String, Arc<tokio::sync::Mutex<SyncGate>>>,
    /// The bindings whose failed sync the node is retrying by itself, with when the next try runs (ms).
    pub(crate) sync_retries: HashMap<String, u64>,
}

/// One binding's sign-in and retry syncs: when one last succeeded, and when the last one ended, with its outcome (ms).
#[derive(Default)]
pub(crate) struct SyncGate {
    pub(crate) succeeded: Option<u64>,
    pub(crate) last: Option<(u64, Result<(), String>)>,
}

impl Signer {
    /// The signer of the node whose state directory is `state_dir`, talking to `endpoints`. Reads the bindings already
    /// in the store; makes the directory (0700) when it is missing.
    pub fn open(state_dir: &Path, endpoints: Endpoints, clock: Clock) -> Result<Self, SignerError> {
        let dir = state_dir.join(STORE_DIR);
        private_dir(&dir)?;
        let http = GitHubHttp::new(endpoints).map_err(SignerError::Store)?;
        let mut bindings = BTreeMap::new();
        let folder = dir.join("bindings");
        if folder.is_dir() {
            let entries = fs::read_dir(&folder).map_err(|_| {
                SignerError::Store("the GitHub connections cannot be read".to_string())
            })?;
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                    continue;
                }
                let Ok(text) = fs::read_to_string(&path) else {
                    continue;
                };
                if let Ok(binding) = serde_json::from_str::<Binding>(&text) {
                    bindings.insert(binding.id.clone(), binding);
                }
            }
        }
        Ok(Self {
            inner: Arc::new(Inner {
                dir,
                http,
                clock,
                runner_key_bits: crate::github_api::RUNNER_KEY_BITS,
                sign_in_deadline: SIGN_IN_SYNC_DEADLINE,
                sync_retry: SYNC_RETRY,
                state: Mutex::new(State {
                    bindings,
                    ..State::default()
                }),
                audit: Mutex::new(()),
            }),
        })
    }

    /// The same signer making runner keys of `bits` bits. Tests use small keys; production keeps 2048.
    pub fn with_runner_key_bits(self, bits: usize) -> Self {
        let inner = Arc::try_unwrap(self.inner).unwrap_or_else(|_| {
            panic!("with_runner_key_bits is called on a fresh signer before it is shared")
        });
        Self {
            inner: Arc::new(Inner {
                runner_key_bits: bits,
                ..inner
            }),
        }
    }

    /// The same signer, with a pool's sign-in waiting `deadline` for the runner group's sync. Tests use a short one.
    pub fn with_sign_in_deadline(self, deadline: std::time::Duration) -> Self {
        let inner = Arc::try_unwrap(self.inner).unwrap_or_else(|_| {
            panic!("with_sign_in_deadline is called on a fresh signer before it is shared")
        });
        Self {
            inner: Arc::new(Inner {
                sign_in_deadline: deadline,
                ..inner
            }),
        }
    }

    /// The same signer, retrying a failed sync first after `first`, doubling up to `cap`. Tests use short ones.
    pub fn with_sync_retry(self, first: std::time::Duration, cap: std::time::Duration) -> Self {
        let inner = Arc::try_unwrap(self.inner).unwrap_or_else(|_| {
            panic!("with_sync_retry is called on a fresh signer before it is shared")
        });
        Self {
            inner: Arc::new(Inner {
                sync_retry: (first, cap),
                ..inner
            }),
        }
    }

    pub fn endpoints(&self) -> &Endpoints {
        self.inner.http.endpoints()
    }

    pub(crate) fn now(&self) -> u64 {
        (self.inner.clock)()
    }

    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Every binding, by id.
    pub fn bindings(&self) -> Vec<Binding> {
        self.lock().bindings.values().cloned().collect()
    }

    /// What the runner launcher needs of one binding.
    pub fn binding(&self, id: &str) -> Result<BindingInfo, SignerError> {
        let state = self.lock();
        let binding = state.bindings.get(id).ok_or(SignerError::UnknownBinding)?;
        Ok(BindingInfo {
            id: binding.id.clone(),
            mode: binding.mode,
            status: binding.status,
            org_url: binding.org_url(self.endpoints()),
            runner_group_id: binding.runner_group.as_ref().map(|group| group.id),
            label_sets: binding.label_sets.clone(),
        })
    }

    /// A registered runner's files, for the machine that will run it. In-process only: no verb returns them.
    pub fn runner_files(&self, binding: &str, runner: &str) -> Result<RunnerFiles, SignerError> {
        {
            let state = self.lock();
            let record = state
                .bindings
                .get(binding)
                .ok_or(SignerError::UnknownBinding)?;
            if !record.runners.iter().any(|known| known.name == runner) {
                return Err(SignerError::UnknownRunner);
            }
        }
        let folder = self.runner_dir(binding, runner);
        let read = |name: &str| {
            fs::read_to_string(folder.join(name))
                .map_err(|_| SignerError::Store(format!("the runner's {name} file cannot be read")))
        };
        Ok(RunnerFiles {
            runner: read(".runner")?,
            credentials: Secret::new(read(".credentials")?),
            rsaparams: Secret::new(read(".credentials_rsaparams")?),
        })
    }

    pub(crate) fn runner_dir(&self, binding: &str, runner: &str) -> PathBuf {
        self.inner.dir.join("runners").join(binding).join(runner)
    }

    /// Writes `binding` to the store and to the state.
    pub(crate) fn save(&self, binding: Binding) -> Result<(), SignerError> {
        self.write(&binding)?;
        self.lock().bindings.insert(binding.id.clone(), binding);
        Ok(())
    }

    /// Changes binding `id` while it is active, under the state lock every write ends in, so a record a disconnect
    /// wrote meanwhile is never written back. `change` says whether there is anything to write; whether it was written.
    pub(crate) fn save_active(
        &self,
        id: &str,
        change: impl FnOnce(&mut Binding) -> bool,
    ) -> Result<bool, SignerError> {
        let mut state = self.lock();
        let Some(mut binding) = state
            .bindings
            .get(id)
            .filter(|binding| binding.status == Status::Active)
            .cloned()
        else {
            return Ok(false);
        };
        if !change(&mut binding) {
            return Ok(false);
        }
        self.write(&binding)?;
        state.bindings.insert(binding.id.clone(), binding);
        Ok(true)
    }

    fn write(&self, binding: &Binding) -> Result<(), SignerError> {
        let path = self
            .inner
            .dir
            .join("bindings")
            .join(format!("{}.json", binding.id));
        let text = serde_json::to_string_pretty(binding)
            .map_err(|_| SignerError::Store("a GitHub connection cannot be written".to_string()))?;
        write_private(&path, text.as_bytes())
    }

    /// Sets the node's OAuth App: its client ID, in its own file, and its secret, in another. The secret is never read
    /// back by a verb.
    pub fn set_oauth_app(
        &self,
        client_id: &str,
        client_secret: &Secret,
    ) -> Result<(), SignerError> {
        if client_id.is_empty()
            || client_id.len() > 64
            || !client_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_')
        {
            return Err(SignerError::Invalid(
                "clientId: a GitHub OAuth App's client ID is letters, digits, dots and underscores"
                    .to_string(),
            ));
        }
        if client_secret.expose().is_empty() || client_secret.expose().len() > 256 {
            return Err(SignerError::Invalid(
                "clientSecret: one line of at most 256 characters".to_string(),
            ));
        }
        write_private(
            &self.inner.dir.join(OAUTH_SECRET_FILE),
            client_secret.expose().trim().as_bytes(),
        )?;
        write_private(
            &self.inner.dir.join(OAUTH_APP_FILE),
            json!({ "clientId": client_id }).to_string().as_bytes(),
        )?;
        self.audit(json!({
            "kind": "oauthAppSet",
            "clientId": client_id,
        }));
        Ok(())
    }

    /// The OAuth App's client ID, when one is set.
    pub fn oauth_client_id(&self) -> Option<String> {
        let text = fs::read_to_string(self.inner.dir.join(OAUTH_APP_FILE)).ok()?;
        let value: Value = serde_json::from_str(&text).ok()?;
        value.get("clientId")?.as_str().map(str::to_string)
    }

    pub(crate) fn oauth_client_secret(&self) -> Option<Secret> {
        fs::read_to_string(self.inner.dir.join(OAUTH_SECRET_FILE))
            .ok()
            .map(|text| Secret::new(text.trim()))
            .filter(|secret| !secret.expose().is_empty())
    }

    /// Appends one line to the audit log, with `at` (milliseconds) and `atText` (UTC) first. A line that cannot be
    /// written is said on stderr, without its content's secrets (it never holds one).
    pub(crate) fn audit(&self, event: Value) {
        let now = self.now();
        let mut line = json!({ "at": now, "atText": crate::handlers::iso_utc(now) });
        if let (Some(line), Value::Object(fields)) = (line.as_object_mut(), event) {
            line.extend(fields);
        }
        let _held = self
            .inner
            .audit
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let path = self.inner.dir.join(AUDIT_FILE);
        if fs::metadata(&path).is_ok_and(|meta| meta.len() > AUDIT_MAX_BYTES) {
            let _ = fs::rename(&path, self.inner.dir.join(format!("{AUDIT_FILE}.1")));
        }
        let written = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&path)
            .and_then(|mut file| writeln!(file, "{line}"));
        if written.is_err() {
            eprintln!("arca: a line of the GitHub audit log could not be written");
        }
    }

    /// The last `limit` audit lines, oldest first, of one binding or of all. Lines that are not JSON are skipped.
    pub fn audit_log(&self, binding: Option<&str>, limit: usize) -> Vec<Value> {
        let text = fs::read_to_string(self.inner.dir.join(AUDIT_FILE)).unwrap_or_default();
        let mut lines: Vec<Value> = text
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|line| {
                binding.is_none_or(|id| line.get("binding").and_then(Value::as_str) == Some(id))
            })
            .collect();
        let skip = lines.len().saturating_sub(limit);
        lines.drain(..skip);
        lines
    }
}

/// A fresh random id: `prefix` and 16 hexadecimal digits.
pub(crate) fn fresh_id(prefix: &str) -> Result<String, SignerError> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes)
        .map_err(|_| SignerError::Store("no random number for an id".to_string()))?;
    Ok(format!(
        "{prefix}{}",
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
}

/// Makes `dir` and its parents, mode 0700, and sets 0700 on `dir`.
pub(crate) fn private_dir(dir: &Path) -> Result<(), SignerError> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .and_then(|()| fs::set_permissions(dir, fs::Permissions::from_mode(0o700)))
        .map_err(|_| {
            SignerError::Store(format!(
                "the folder {} cannot be made private",
                dir.display()
            ))
        })
}

/// Writes `bytes` to `path` through a 0600 file beside it and a rename, making the parent folder (0700) first.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<(), SignerError> {
    let parent = path
        .parent()
        .ok_or_else(|| SignerError::Store("a store file has no folder".to_string()))?;
    private_dir(parent)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| SignerError::Store("a store file has no name".to_string()))?;
    let temporary = parent.join(format!(".{name}.tmp"));
    let written: io::Result<()> = (|| {
        let _ = fs::remove_file(&temporary);
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)
    })();
    written.map_err(|_| {
        let _ = fs::remove_file(&temporary);
        SignerError::Store(format!("{} cannot be written", path.display()))
    })
}

/// Checks one label set: one to twenty labels, each 1 to 100 printable characters with no comma or space, no two the
/// same. The person chooses the names: none is added and none is required.
pub fn check_labels(path: &str, labels: &[String]) -> Result<Vec<String>, SignerError> {
    if labels.is_empty() || labels.len() > 20 {
        return Err(SignerError::Invalid(format!(
            "{path}: one to twenty labels"
        )));
    }
    let mut seen = Vec::new();
    for label in labels {
        let label = label.trim();
        if label.is_empty()
            || label.chars().count() > 100
            || label
                .chars()
                .any(|c| c.is_control() || c.is_whitespace() || c == ',')
        {
            return Err(SignerError::Invalid(format!(
                "{path}: a label is 1 to 100 characters with no space or comma"
            )));
        }
        if seen
            .iter()
            .any(|known: &String| known.eq_ignore_ascii_case(label))
        {
            return Err(SignerError::Invalid(format!(
                "{path}: a label is named twice"
            )));
        }
        seen.push(label.to_string());
    }
    Ok(seen)
}

/// Checks an organisation login: GitHub's rule, 1 to 39 letters, digits and single hyphens, not at either end.
pub fn check_account(account: &str) -> Result<(), SignerError> {
    let ok = !account.is_empty()
        && account.len() <= 39
        && account
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !account.starts_with('-')
        && !account.ends_with('-')
        && !account.contains("--");
    if ok {
        Ok(())
    } else {
        Err(SignerError::Invalid(
            "org: a GitHub organisation's login is 1 to 39 letters, digits and single hyphens"
                .to_string(),
        ))
    }
}

/// Whether two label sets are the same labels, in any order, letter case aside (GitHub matches labels so).
pub fn same_labels(a: &[String], b: &[String]) -> bool {
    a.len() == b.len()
        && a.iter()
            .all(|label| b.iter().any(|other| other.eq_ignore_ascii_case(label)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_the_persons_own_and_checked_for_shape_only() {
        let labels = vec!["linux".to_string(), "big-box".to_string()];
        assert_eq!(check_labels("labels", &labels).unwrap(), labels);
        assert!(check_labels("labels", &[]).is_err());
        assert!(check_labels("labels", &["a b".to_string()]).is_err());
        assert!(check_labels("labels", &["a,b".to_string()]).is_err());
        assert!(check_labels("labels", &["x".to_string(), "X".to_string()]).is_err());
        assert!(same_labels(
            &["Linux".to_string(), "x64".to_string()],
            &["x64".to_string(), "linux".to_string()]
        ));
        assert!(!same_labels(
            &["linux".to_string()],
            &["linux".to_string(), "x64".to_string()]
        ));
    }

    #[test]
    fn an_account_is_a_github_login() {
        assert!(check_account("acme").is_ok());
        assert!(check_account("acme-labs").is_ok());
        for bad in ["", "-acme", "acme-", "ac--me", "acme/x", &"a".repeat(40)] {
            assert!(check_account(bad).is_err(), "{bad}");
        }
    }
}
