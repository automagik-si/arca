//! The MCP verbs of the GitHub signer. They start, read and end connections, mint runners and read the audit log. None
//! returns a key, a grant or a token, but `github_runner_mint` returns a runner's just-in-time configuration, which is
//! that runner's credential, to any caller with an `operator` token (see `crate::github_signer`).
//!
//! `github_link` is the verb a linked node serves its approval host's GitHub page with (see `crate::github_page`): the
//! page begins an App's manifest flow here, relays GitHub's two returns, lists the connections and disconnects one. The
//! node converts GitHub's code into the App and its key itself, so the key is made into a file here and never passes
//! through the page's node. Every step names the page origin it comes from, and the node takes it only from a page whose
//! owner passkeys it trusts: `approval.origins` (with `trustOwnerKeysFrom`) or an `approval.peerKeys` source's origins.

use std::sync::Arc;

use arca_mcp::{CallError, Caller, ToolHandler};
use serde_json::{Value, json};

use crate::github_api::Secret;
use crate::github_app::{JobRef, MintRefused, MintRequest};
use crate::github_oauth::{OauthRequest, StartError};
use crate::github_signer::{Signer, SignerError};

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

/// `github_link` for `signer`: it takes steps only from a page at one of `origins`, the page origins whose owner passkeys
/// this node trusts.
pub fn link_handler(signer: &Signer, origins: Vec<String>) -> Arc<dyn ToolHandler> {
    Arc::new(LinkHandler {
        signer: signer.clone(),
        origins,
    })
}

struct LinkHandler {
    signer: Signer,
    origins: Vec<String>,
}

/// The refusal of a step whose flow this node did not begin for the page that sends it.
fn unknown_flow() -> CallError {
    CallError::Refused {
        code: "UnknownFlow".to_string(),
        message: "This link is unknown or older than an hour. Start again from Arca's GitHub page."
            .to_string(),
    }
}

impl LinkHandler {
    /// The manifest flow `state`, which this node began for the page at `origin`.
    fn flow_of<'a>(&self, args: &'a Value, origin: &str) -> Result<&'a str, CallError> {
        let state = text(args, "state")?;
        match self.signer.app_flow_origin(state) {
            Some(begun) if begun == origin => Ok(state),
            _ => Err(unknown_flow()),
        }
    }
}

impl ToolHandler for LinkHandler {
    fn call(&self, args: Value, caller: &Caller) -> Result<Value, CallError> {
        // Reached by naming it: this node's operators, or a token whose list names the verb, which is the relay token
        // its approval host holds for it. A doctor token with no list (an agent's) does not connect GitHub.
        let named = caller
            .tools
            .as_ref()
            .is_some_and(|tools| tools.iter().any(|tool| tool == LINK_VERB));
        if !named && caller.scope < arca_api::Scope::Operator {
            return Err(CallError::Refused {
                code: "LinkToken".to_string(),
                message: format!(
                    "{LINK_VERB} is for this node's operators and for the relay token whose tool list names it"
                ),
            });
        }
        let origin = text(&args, "origin")?;
        if !self.origins.iter().any(|trusted| trusted == origin) {
            return Err(CallError::Refused {
                code: "NotApprovalHost".to_string(),
                message: "this node takes GitHub connections only from its approval host's page, and the page that \
                          asked is not one whose owner passkeys it trusts (approval.origins with trustOwnerKeysFrom, or \
                          an approval.peerKeys source's origins): nothing was begun"
                    .to_string(),
            });
        }
        let by: String = args
            .get("by")
            .and_then(Value::as_str)
            .unwrap_or("page")
            .chars()
            .filter(|c| !c.is_control())
            .take(64)
            .collect();
        let by = format!("link:{}:{by}", caller.token_id);
        match text(&args, "step")? {
            "begin" => {
                let org = text(&args, "org")?;
                let started = if args.get("purpose").and_then(Value::as_str) == Some("repositories")
                {
                    self.signer.repository_app_begin(org, origin, &by)
                } else {
                    let sets: Vec<Vec<String>> = args
                        .get("labelSets")
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
                        .unwrap_or_default();
                    self.signer.app_begin(org, &sets, origin, &by)
                };
                let start = started.map_err(|error| refusal(&error))?;
                let name = serde_json::from_str::<Value>(&start.manifest)
                    .ok()
                    .and_then(|manifest| manifest["name"].as_str().map(str::to_string))
                    .unwrap_or_default();
                Ok(json!({
                    "actionUrl": start.action_url,
                    "manifest": start.manifest,
                    "state": start.state,
                    "appName": name,
                }))
            }
            "created" => {
                let state = self.flow_of(&args, origin)?;
                let code = text(&args, "code")?;
                let install = runtime()?
                    .block_on(self.signer.app_created(code, state))
                    .map_err(|why| CallError::UntrustedRefused {
                        code: "NotTaken".to_string(),
                        message: why,
                    })?;
                let binding = self
                    .signer
                    .app_flow_binding(state)
                    .and_then(|id| self.signer.bindings().into_iter().find(|b| b.id == id))
                    .ok_or_else(unknown_flow)?;
                let app = binding.app.clone().ok_or_else(unknown_flow)?;
                let settings = self.signer.inner.http.web(&format!(
                    "/organizations/{}/settings/apps/{}",
                    binding.account, app.slug
                ));
                Ok(json!({
                    "installUrl": install,
                    "binding": binding.id,
                    "account": binding.account,
                    "app": { "slug": app.slug, "settingsUrl": settings },
                }))
            }
            "installed" => {
                let state = self.flow_of(&args, origin)?;
                let installation = args
                    .get("installationId")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| {
                        CallError::Invalid("installationId: a whole number is required".to_string())
                    })?;
                let id = runtime()?
                    .block_on(self.signer.app_installed(installation, Some(state)))
                    .map_err(|why| CallError::UntrustedRefused {
                        code: "NotConnected".to_string(),
                        message: why,
                    })?;
                Ok(json!({ "binding": id }))
            }
            "bindings" => Ok(json!({ "bindings": json_of(&self.signer.bindings())? })),
            "disconnect" => {
                let binding = text(&args, "binding")?;
                let record = runtime()?
                    .block_on(self.signer.revoke(binding, &by))
                    .map_err(|error| refusal(&error))?;
                json_of(&record)
            }
            _ => Err(CallError::Invalid(
                "step: begin, created, installed, bindings or disconnect".to_string(),
            )),
        }
    }
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
                "bindings": json_of(&self.signer.bindings())?,
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
