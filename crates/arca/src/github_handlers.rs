//! The MCP verbs of the GitHub signer. They start, read and end connections, mint runners and read the audit log. None
//! returns a key, a grant or a token, but `github_runner_mint` returns a runner's just-in-time configuration, which is
//! that runner's credential, to any caller with an `operator` token (see `crate::github_signer`).
//!
//! `github_link` is the verb a linked node serves its approval host's GitHub page with (see `crate::github_page`). The
//! owner proves themself to this node, not to the page: begin (the organisation, the kind of App, the label sets and the
//! page GitHub returns to) and disconnect are a `github_link` action proposal, approved with the owner's passkey, whose
//! assertion this node checks itself against the owner keys it trusts for approvals (`approval.peerKeys`, or
//! `trustOwnerKeysFrom` with `approval.origins`), spending its nonce, as for every approved action. Only the one-shot grant
//! that approval arms runs them; any token without one is refused. The node's other owner approvals arm that grant too, as
//! for any action: the owner's signed-in session (begin only; a disconnect erases) and a static admin token's terminal.
//! GitHub's two returns (created, with GitHub's code, and
//! installed) are taken only for the flow that approved begin made: the same proposal id, page, organisation and kind,
//! within the hour, and installed once. refresh reads an installation of an App this node holds again from GitHub; claim
//! asks GitHub, writing nothing, whether one of this node's Apps holds an installation; flow and bindings read this node. The node converts GitHub's code into the App and its key itself, so the key is made into a
//! file here and never passes through the page's node.

use std::sync::{Arc, Mutex};

use arca_mcp::{CallError, Caller, ToolHandler};
use serde_json::{Value, json};

use crate::github_api::Secret;
use crate::github_app::{JobRef, MintRefused, MintRequest};
use crate::github_oauth::{OauthRequest, StartError};
use crate::github_signer::{GroupAccess, Signer, SignerError, check_account, check_labels};
use crate::kernel::Kernel;

/// The verbs, keyed by name.
pub const VERBS: [&str; 8] = [
    "github_oauth_app_set",
    "github_connect",
    "github_connect_status",
    "github_bindings",
    "github_audit",
    "github_runner_mint",
    "github_mint_job",
    "github_disconnect",
];

struct GithubHandler {
    verb: &'static str,
    signer: Signer,
}

/// The verb a linked node serves its approval host's GitHub page with.
pub const LINK_VERB: &str = "github_link";

/// `github_link` for `signer` on the kernel `kernel`: begin and disconnect run only with the one-shot grant `kernel` arms
/// when it has checked the owner's passkey for an approved `github_link` proposal, and every step names one of `origins`,
/// the page origins whose owner passkeys this node trusts for approvals.
pub fn link_handler(
    signer: &Signer,
    kernel: Arc<Mutex<Kernel>>,
    origins: Vec<String>,
) -> Arc<dyn ToolHandler> {
    Arc::new(LinkHandler {
        signer: signer.clone(),
        kernel,
        origins,
    })
}

struct LinkHandler {
    signer: Signer,
    kernel: Arc<Mutex<Kernel>>,
    origins: Vec<String>,
}

/// The refusal of a later step whose flow is not one an approved begin made for the page, organisation and kind it names.
fn unknown_flow() -> CallError {
    CallError::Refused {
        code: "UnknownFlow".to_string(),
        message: "This link is unknown or older than an hour. Start again from Arca's GitHub page."
            .to_string(),
    }
}

/// The refusal of a begin or a disconnect that comes with no grant of an approved proposal.
fn not_approved() -> CallError {
    CallError::Refused {
        code: "NotApproved".to_string(),
        message: "a GitHub connection begins or ends here only from a github_link proposal the owner approved with a \
                  passkey this node trusts: nothing was begun or disconnected"
            .to_string(),
    }
}

/// The fields each step of a proposal takes.
const BEGIN_FIELDS: [&str; 5] = ["step", "origin", "org", "purpose", "labelSets"];
const DISCONNECT_FIELDS: [&str; 3] = ["step", "origin", "binding"];

/// Checks a proposed `github_link` (already held to the verb's schema) and returns what the owner approves: a begin with
/// its organisation, its kind (`purpose`, written out) and, for runners, its label sets checked as the page's own form
/// checks them; or a disconnect of one connection. No other step is proposed, and no field of another step is carried.
pub fn prepare_link(mut args: Value) -> Result<Value, String> {
    let step = args["step"].as_str().unwrap_or_default().to_string();
    let fields: &[&str] = match step.as_str() {
        "begin" => &BEGIN_FIELDS,
        "disconnect" => &DISCONNECT_FIELDS,
        _ => {
            return Err(
                "args/step: a proposal begins a GitHub connection or ends one: begin or disconnect"
                    .to_string(),
            );
        }
    };
    let Some(object) = args.as_object_mut() else {
        return Err("args: expected an object".to_string());
    };
    if let Some(extra) = object.keys().find(|key| !fields.contains(&key.as_str())) {
        return Err(format!(
            "args/{extra}: a {step} takes only {}",
            fields.join(", ")
        ));
    }
    if step == "disconnect" {
        if !object.contains_key("binding") {
            return Err("args/binding: the connection to disconnect is required".to_string());
        }
        return Ok(args);
    }
    let org = object
        .get("org")
        .and_then(Value::as_str)
        .unwrap_or_default();
    check_account(org).map_err(|error| format!("args/{}", error.message()))?;
    let repositories = object.get("purpose").and_then(Value::as_str) == Some("repositories");
    object.insert(
        "purpose".to_string(),
        json!(if repositories {
            "repositories"
        } else {
            "runners"
        }),
    );
    if repositories {
        if object
            .get("labelSets")
            .and_then(Value::as_array)
            .is_some_and(|sets| !sets.is_empty())
        {
            return Err(
                "args/labelSets: the workspace repositories App has no runners, so no labels"
                    .to_string(),
            );
        }
        object.remove("labelSets");
        return Ok(args);
    }
    let sets = label_sets_of(object.get("labelSets"));
    if sets.is_empty() || sets.len() > 10 {
        return Err("args/labelSets: one to ten label sets".to_string());
    }
    let sets = sets
        .iter()
        .map(|set| check_labels("args/labelSets", set))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.message())?;
    object.insert("labelSets".to_string(), json!(sets));
    Ok(args)
}

/// The label sets of a `labelSets` value, each label as given.
fn label_sets_of(value: Option<&Value>) -> Vec<Vec<String>> {
    value
        .and_then(Value::as_array)
        .map(|sets| {
            sets.iter()
                .map(|set| {
                    set.as_array()
                        .map(|labels| {
                            labels
                                .iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The card rows of a proposed `github_link`, in the owner's words.
pub fn link_rows(args: &Value) -> Vec<(&'static str, String)> {
    let text = |key: &str| args[key].as_str().unwrap_or_default().to_string();
    if args["step"] == "disconnect" {
        return vec![
            ("Connection", text("binding")),
            ("Page", text("origin")),
            (
                "Effect",
                "This node ends the GitHub connection: it deletes the connection's key, removes its runners from GitHub \
                 where it can, and takes no more jobs for it. The App stays on GitHub until you delete it there."
                    .to_string(),
            ),
        ];
    }
    let repositories = args["purpose"] == "repositories";
    let mut rows = vec![
        ("Organisation", text("org")),
        (
            "Kind",
            if repositories {
                "The workspace repositories App: Git access to the repositories you select"
                    .to_string()
            } else {
                "Arca's App for the organisation's Actions runners".to_string()
            },
        ),
    ];
    if !repositories {
        rows.push((
            "Labels",
            label_sets_of(args.get("labelSets"))
                .iter()
                .map(|set| set.join(", "))
                .collect::<Vec<_>>()
                .join(" | "),
        ));
    }
    rows.push(("Page", text("origin")));
    rows.push((
        "Effect",
        "This node begins making the App: GitHub opens with it ready to create in the organisation, and sends you back to \
         this page. This node takes GitHub's code itself and keeps the App's key; nothing is made until you click Create \
         on GitHub, and the flow ends after an hour."
            .to_string(),
    ));
    rows
}

/// The card's sentence for a proposed `github_link`.
pub fn link_summary(args: &Value) -> String {
    let text = |key: &str| args[key].as_str().unwrap_or_default();
    match (text("step"), text("purpose")) {
        ("disconnect", _) => format!(
            "Disconnect the GitHub connection {} on this node",
            text("binding")
        ),
        (_, "repositories") => format!(
            "Make the workspace repositories App in the GitHub organisation {} for this node",
            text("org")
        ),
        _ => format!(
            "Connect the GitHub organisation {}'s Actions runners to this node with Arca's App",
            text("org")
        ),
    }
}

impl LinkHandler {
    /// Spends the one-shot grant that the kernel armed for an approved `github_link` with exactly these arguments, and
    /// answers the approved proposal's id. Without one nothing runs.
    fn spend(&self, args: &Value, grant: Option<&str>) -> Result<String, CallError> {
        let grant = grant.ok_or_else(not_approved)?;
        let mut kernel = self
            .kernel
            .lock()
            .map_err(|_| CallError::Internal("the kernel lock is poisoned".to_string()))?;
        let id = kernel.action_grant_id(grant).ok_or_else(not_approved)?;
        if !kernel.consume_action_grant(LINK_VERB, args, grant) {
            return Err(not_approved());
        }
        Ok(id)
    }

    /// The manifest flow a later step names: one the approved begin `plan` made for the page `origin`, for the organisation
    /// and kind the step names, within its hour.
    fn approved_flow<'a>(&self, args: &'a Value, origin: &str) -> Result<&'a str, CallError> {
        let state = text(args, "state")?;
        let plan = text(args, "plan")?;
        let flow = self.signer.app_flow(state).ok_or_else(unknown_flow)?;
        let repositories = args.get("purpose").and_then(Value::as_str) == Some("repositories");
        let same = flow.approved.as_deref() == Some(plan)
            && flow.origin == origin
            && args.get("org").and_then(Value::as_str) == Some(flow.account.as_str())
            && repositories == flow.repository_access;
        if same { Ok(state) } else { Err(unknown_flow()) }
    }
}

impl ToolHandler for LinkHandler {
    fn call(&self, args: Value, _caller: &Caller) -> Result<Value, CallError> {
        let mut args = args;
        let grant = args
            .as_object_mut()
            .and_then(|object| object.remove("approval"))
            .and_then(|value| value.as_str().map(str::to_string));
        let origin = text(&args, "origin")?.to_string();
        if !self.origins.contains(&origin) {
            return Err(CallError::Refused {
                code: "NotApprovalHost".to_string(),
                message: "this node takes GitHub connections only from a page whose owner passkeys it trusts for \
                          approvals (approval.origins with trustOwnerKeysFrom, or an approval.peerKeys source's origins), \
                          and this page is not one: nothing was done"
                    .to_string(),
            });
        }
        match text(&args, "step")? {
            "begin" => {
                let plan = self.spend(&args, grant.as_deref())?;
                let org = text(&args, "org")?;
                let repositories = args["purpose"] == "repositories";
                let sets = label_sets_of(args.get("labelSets"));
                let by = format!("approved:{plan}");
                let start = self
                    .signer
                    .app_begin_approved(org, &sets, &origin, &by, repositories, &plan)
                    .map_err(|error| refusal(&error))?;
                let name = serde_json::from_str::<Value>(&start.manifest)
                    .ok()
                    .and_then(|manifest| manifest["name"].as_str().map(str::to_string))
                    .unwrap_or_default();
                Ok(json!({
                    "actionUrl": start.action_url,
                    "manifest": start.manifest,
                    "state": start.state,
                    "appName": name,
                    "account": org,
                    "purpose": if repositories { "repositories" } else { "runners" },
                }))
            }
            "disconnect" => {
                let plan = self.spend(&args, grant.as_deref())?;
                let binding = text(&args, "binding")?;
                let record = runtime()?
                    .block_on(self.signer.revoke(binding, &format!("approved:{plan}")))
                    .map_err(|error| refusal(&error))?;
                json_of(&record)
            }
            "created" => {
                let state = self.approved_flow(&args, &origin)?;
                let code = text(&args, "code")?;
                let install = runtime()?
                    .block_on(self.signer.app_created(code, state))
                    .map_err(|why| CallError::UntrustedRefused {
                        code: "NotTaken".to_string(),
                        message: why,
                    })?;
                let mut answer = self.flow_words(state);
                answer["installUrl"] = json!(install);
                Ok(answer)
            }
            "installed" => {
                let state = self.approved_flow(&args, &origin)?;
                let installation = installation_of(&args)?;
                let id = runtime()?
                    .block_on(self.signer.app_installed(installation, Some(state)))
                    .map_err(|why| CallError::UntrustedRefused {
                        code: "NotConnected".to_string(),
                        message: why,
                    })?;
                Ok(json!({ "binding": id }))
            }
            // GitHub's later return with no flow (repositories changed, or an install finished after the hour): the
            // installation is read again from GitHub with the key of an App this node holds, and its runner group made
            // to match. Nothing is taken that GitHub does not say.
            "refresh" => {
                let installation = installation_of(&args)?;
                let id = runtime()?
                    .block_on(self.signer.app_installed(installation, None))
                    .map_err(|why| CallError::UntrustedRefused {
                        code: "NotConnected".to_string(),
                        message: why,
                    })?;
                Ok(self.claim_words(&id))
            }
            // Whether one of this node's Apps holds the installation, asked of GitHub with the App's own key; nothing is
            // written here or on GitHub.
            "claim" => {
                let installation = installation_of(&args)?;
                let owner = runtime()?
                    .block_on(self.signer.app_owning(installation))
                    .map_err(|why| CallError::UntrustedRefused {
                        code: "GitHubUnreachable".to_string(),
                        message: why,
                    })?;
                Ok(owner.map_or_else(|| json!({}), |id| self.claim_words(&id)))
            }
            "flow" => match self.approved_flow(&args, &origin) {
                Ok(state) => Ok(self.flow_words(state)),
                Err(_) => Ok(json!({ "known": false })),
            },
            "bindings" => Ok(json!({ "bindings": bindings_view(&self.signer)? })),
            _ => Err(CallError::Invalid(
                "step: begin, created, installed, refresh, claim, flow, bindings or disconnect"
                    .to_string(),
            )),
        }
    }
}

impl LinkHandler {
    /// The connection `id` and its App, by its id and its name on GitHub.
    fn claim_words(&self, id: &str) -> Value {
        let app = self
            .signer
            .bindings()
            .into_iter()
            .find(|binding| binding.id == id)
            .and_then(|binding| binding.app)
            .map(|app| json!({ "id": app.id, "slug": app.slug }));
        let mut answer = json!({ "binding": id });
        if let Some(app) = app {
            answer["app"] = app;
        }
        answer
    }

    /// What this node holds of the live flow `state`: whether GitHub handed its App over, the connection it made, its
    /// install screen and the App's settings page.
    fn flow_words(&self, state: &str) -> Value {
        let Some(id) = self.signer.app_flow_binding(state) else {
            return json!({ "known": true });
        };
        let mut answer = json!({ "known": true, "binding": id });
        if let Some(install) = self.signer.app_flow_install(state) {
            answer["installUrl"] = json!(install);
        }
        if let Some(binding) = self.signer.bindings().into_iter().find(|b| b.id == id) {
            answer["account"] = json!(binding.account);
            answer["installed"] = json!(binding.installation.is_some());
            if let Some(app) = binding.app {
                let settings = self.signer.inner.http.web(&format!(
                    "/organizations/{}/settings/apps/{}",
                    binding.account, app.slug
                ));
                answer["app"] = json!({ "slug": app.slug, "settingsUrl": settings });
            }
        }
        answer
    }
}

fn installation_of(args: &Value) -> Result<u64, CallError> {
    args.get("installationId")
        .and_then(Value::as_u64)
        .ok_or_else(|| CallError::Invalid("installationId: a whole number is required".to_string()))
}

/// The handlers of the GitHub verbs for `signer`.
pub fn handlers(signer: &Signer) -> Vec<(&'static str, Arc<dyn ToolHandler>)> {
    VERBS
        .into_iter()
        .map(|verb| {
            (
                verb,
                Arc::new(GithubHandler {
                    verb,
                    signer: signer.clone(),
                }) as Arc<dyn ToolHandler>,
            )
        })
        .collect()
}

fn refusal(error: &SignerError) -> CallError {
    match error {
        SignerError::Invalid(why) => CallError::Invalid(why.clone()),
        other => CallError::Refused {
            code: other.code().to_string(),
            message: other.message(),
        },
    }
}

fn text<'a>(args: &'a Value, key: &str) -> Result<&'a str, CallError> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| CallError::Invalid(format!("{key}: a string is required")))
}

fn job_of(value: Option<&Value>) -> Option<JobRef> {
    let value = value?;
    Some(JobRef {
        repo: value.get("repo")?.as_str()?.to_string(),
        run_id: value.get("runId").and_then(Value::as_u64),
        job_id: value.get("jobId").and_then(Value::as_u64),
        job_name: value
            .get("jobName")
            .and_then(Value::as_str)
            .map(str::to_string),
        workflow_ref: value
            .get("workflowRef")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn strings(args: &Value, key: &str) -> Result<Vec<String>, CallError> {
    args.get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| CallError::Invalid(format!("{key}: an array is required")))?
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_string)
                .ok_or_else(|| CallError::Invalid(format!("{key}: strings only")))
        })
        .collect()
}

fn runtime() -> Result<tokio::runtime::Handle, CallError> {
    tokio::runtime::Handle::try_current()
        .map_err(|_| CallError::Internal("no runtime for the GitHub call".to_string()))
}

fn json_of<T: serde::Serialize>(value: &T) -> Result<Value, CallError> {
    serde_json::to_value(value).map_err(|_| {
        CallError::Internal("a GitHub record could not be written as JSON".to_string())
    })
}

/// The connections as the verbs show them: each record with what its runner group is open to (`access`, and the
/// same in words, `accessWords`). One whose own group is open to no repository carries those words as its `why`, so
/// it never reads as simply active.
fn bindings_view(signer: &Signer) -> Result<Value, CallError> {
    let mut list = Vec::new();
    for binding in signer.bindings() {
        let mut value = json_of(&binding)?;
        if let (Some(access), Some(words)) = (binding.group_access(), binding.group_access_words())
        {
            if let Some(group) = value.get_mut("runnerGroup").and_then(Value::as_object_mut) {
                group.insert("access".to_string(), json_of(&access)?);
                group.insert("accessWords".to_string(), json!(words));
            }
            if access == GroupAccess::NoRepository && binding.why.is_none() {
                value["why"] = json!(words);
            }
        }
        list.push(value);
    }
    Ok(Value::Array(list))
}

impl ToolHandler for GithubHandler {
    fn call(&self, args: Value, caller: &Caller) -> Result<Value, CallError> {
        match self.verb {
            "github_oauth_app_set" => {
                let client_id = text(&args, "clientId")?;
                let secret = Secret::new(text(&args, "clientSecret")?);
                self.signer
                    .set_oauth_app(client_id, &secret)
                    .map_err(|error| refusal(&error))?;
                Ok(json!({ "ok": true, "clientId": client_id }))
            }
            "github_connect" => {
                let labels = strings(&args, "labels")?;
                let runners = args
                    .get("runners")
                    .and_then(Value::as_u64)
                    .and_then(|n| u32::try_from(n).ok())
                    .ok_or_else(|| {
                        CallError::Invalid("runners: a whole number is required".to_string())
                    })?;
                let request = OauthRequest {
                    account: text(&args, "org")?.to_string(),
                    group: args
                        .get("group")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    labels,
                    runners,
                    name_prefix: args
                        .get("namePrefix")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    caller: caller.token_id.clone(),
                };
                let handle = runtime()?;
                match handle.block_on(self.signer.oauth_start(request)) {
                    Ok(flow) => json_of(&flow),
                    Err(StartError::Signer(error)) => Err(refusal(&error)),
                    Err(StartError::GitHub(why)) => Err(CallError::UntrustedRefused {
                        code: "GitHubRefused".to_string(),
                        message: format!("GitHub did not start the sign-in: {why}"),
                    }),
                }
            }
            "github_connect_status" => {
                let id = text(&args, "id")?;
                let flow = self
                    .signer
                    .oauth_flow(id)
                    .ok_or_else(|| CallError::Refused {
                        code: "UnknownFlow".to_string(),
                        message: "no device flow of the last day has that id on this node"
                            .to_string(),
                    })?;
                json_of(&flow)
            }
            "github_bindings" => Ok(json!({
                "bindings": bindings_view(&self.signer)?,
                "flows": json_of(&self.signer.oauth_flows())?,
            })),
            "github_audit" => {
                let limit = args
                    .get("limit")
                    .and_then(Value::as_u64)
                    .map_or(100, |n| n.clamp(1, 1000) as usize);
                let binding = args.get("binding").and_then(Value::as_str);
                Ok(json!({ "lines": self.signer.audit_log(binding, limit) }))
            }
            "github_runner_mint" => {
                let request = MintRequest {
                    binding: text(&args, "binding")?.to_string(),
                    runner_name: text(&args, "runnerName")?.to_string(),
                    labels: strings(&args, "labels")?,
                    work_folder: args
                        .get("workFolder")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    scale_set_id: args.get("scaleSetId").and_then(Value::as_u64),
                    job: job_of(args.get("job")),
                    caller: caller.token_id.clone(),
                };
                match runtime()?.block_on(self.signer.mint(request)) {
                    Ok(minted) => Ok(json!({
                        "mintId": minted.mint_id,
                        "runnerId": minted.runner_id,
                        "runnerName": minted.runner_name,
                        "runnerGroupId": minted.runner_group_id,
                        "labels": minted.labels,
                        "encodedJitConfig": minted.encoded_jit_config.expose(),
                    })),
                    Err(refused @ MintRefused::GitHub { .. }) => Err(CallError::UntrustedRefused {
                        code: refused.code().to_string(),
                        message: refused.message(),
                    }),
                    Err(refused) => Err(CallError::Refused {
                        code: refused.code().to_string(),
                        message: refused.message(),
                    }),
                }
            }
            "github_mint_job" => {
                let mint_id = args.get("mintId").and_then(Value::as_u64).ok_or_else(|| {
                    CallError::Invalid("mintId: a whole number is required".to_string())
                })?;
                let job = job_of(args.get("job")).ok_or_else(|| {
                    CallError::Invalid("job.repo: a string is required".to_string())
                })?;
                self.signer.attach_job(mint_id, &job);
                Ok(json!({ "ok": true }))
            }
            "github_disconnect" => {
                let binding = text(&args, "binding")?;
                let record = runtime()?
                    .block_on(self.signer.revoke(binding, &caller.token_id))
                    .map_err(|error| refusal(&error))?;
                json_of(&record)
            }
            _ => Err(CallError::NotFound),
        }
    }
}
