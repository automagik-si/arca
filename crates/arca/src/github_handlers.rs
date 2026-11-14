//! The MCP verbs of the GitHub signer. They start, read and end connections, mint runners and read the audit log. None
//! returns a key, a grant or a token, but `github_runner_mint` returns a runner's just-in-time configuration, which is
//! that runner's credential, to any caller with an `operator` token (see `crate::github_signer`).

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
