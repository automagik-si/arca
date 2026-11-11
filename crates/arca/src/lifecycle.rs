//! Starting, stopping and restarting an instance: `instance_start`, `instance_stop` and `instance_restart`.
//!
//! The three verbs name one instance by its project and its name. They act only on an instance Arca owns or has adopted:
//! one in the kernel's project family whose own config classes it as a kernel object, the check the GPU verbs make
//! (OWNERSHIP Rules 5 and 8). A foreign, droidd, contested or retained instance, and any instance outside the family, is
//! refused in plain words and no write is sent to it. A watch-only node builds no state writer at all.
//!
//! `instance_start` runs for a doctor's direct call: starting what Arca owns takes nothing away. `instance_stop` and
//! `instance_restart` interrupt whatever the instance serves, so they run only with the one-shot grant of an action
//! proposal the owner approved, and a direct call without one is refused. All three can be proposed with `action_propose`.
//!
//! The verbs are idempotent. A start of a running instance and a stop of a stopped one change nothing, succeed and say
//! so; a restart of a stopped instance starts it. A stop and a restart are graceful and nothing is ever forced.
//!
//! A power state is not a desired-state entry, so these changes are not effects in the effect journal: an open intent
//! there is settled at the next start as a create or a patch, which a state change is not. Every call that passes its
//! gate is journalled in [`INSTANCE_ACTIONS_FILE`] instead, an append-only file of JSON lines in the state directory: a
//! `begin` line before anything is sent to Incus, and an `end` line with what happened. A `begin` that cannot be made
//! durable refuses the call before any change.

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use arca_api::Scope;
use arca_incus::{
    GRACE_SECONDS, IncusError, InstanceState, ProjectFamily, StateAction, StateChanger,
};
use arca_kernel::ownership::{IncusKind, ObjectRef, OwnerClass, classify_object};
use arca_mcp::{CallError, Caller, ToolHandler};
use serde_json::{Value, json};

use crate::action_proposal::ActionKind;
use crate::gpu::RetainedEntries;
use crate::handlers::Clock;
use crate::kernel::Kernel;

/// The instance action journal's file name in the state directory.
pub const INSTANCE_ACTIONS_FILE: &str = "instance-actions.jsonl";
/// A journal past this size is set aside as `<name>.1`, replacing the one set aside before, and a new one is begun.
const MAX_JOURNAL_BYTES: u64 = 8 * 1024 * 1024;

/// The kernel's current retained set, read at each call.
pub type Retained = Arc<dyn Fn() -> Result<RetainedEntries, String> + Send + Sync>;

/// What follows a start or a restart that left the instance running, given its project and name: the secrets of a
/// virtual-machine service are written into its guest (`crate::guest_secrets`). `Ok(None)` when the instance has
/// none, the names written otherwise, or in plain words what was not done and whether asking again can help. No answer
/// holds a value.
pub type AfterStart = Arc<
    dyn Fn(&str, &str) -> Result<Option<Vec<String>>, crate::guest_secrets::NotWritten>
        + Send
        + Sync,
>;

/// The state writer of a kernel-mode node, the ownership check in front of it, and the journal behind it.
pub struct InstanceLifecycle {
    changer: StateChanger,
    /// The projects of the node's tenants, when the node may hold any.
    tenants: Option<arca_providers::incus::TenantProjects>,
    retained: Retained,
    journal: PathBuf,
    /// One action at a time. The number is the last id this process gave a journalled action.
    gate: Mutex<u64>,
    after_start: Option<AfterStart>,
}

impl std::fmt::Debug for InstanceLifecycle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstanceLifecycle").finish_non_exhaustive()
    }
}

/// What one action did.
struct Done {
    state: InstanceState,
    changed: bool,
    outcome: &'static str,
    message: String,
}

/// Why one action did nothing, or did not finish: the refusal, and the instance's uuid and status when they were read.
struct Stopped {
    refusal: CallError,
    uuid: Option<String>,
    was: Option<String>,
}

impl InstanceLifecycle {
    /// The lifecycle of one kernel family. `retained` is read at each call, never captured once; the journal lives in
    /// `state_dir`.
    pub fn new(
        socket: PathBuf,
        family: ProjectFamily,
        retained: impl Fn() -> Result<RetainedEntries, String> + Send + Sync + 'static,
        state_dir: &Path,
    ) -> InstanceLifecycle {
        InstanceLifecycle {
            changer: StateChanger::new(socket, family),
            tenants: None,
            retained: Arc::new(retained),
            journal: state_dir.join(INSTANCE_ACTIONS_FILE),
            gate: Mutex::new(0),
            after_start: None,
        }
    }

    /// The same lifecycle, also over the projects of the tenants whose entries exist, as the kernel keeps them.
    pub fn with_tenants(
        mut self,
        tenants: arca_providers::incus::TenantProjects,
    ) -> InstanceLifecycle {
        self.tenants = Some(tenants);
        self
    }

    /// The state writer for this call: the configured family, and the tenant projects as they are now.
    fn changer(&self) -> StateChanger {
        self.tenants
            .as_ref()
            .and_then(|tenants| tenants.snapshot().ok())
            .and_then(|projects| {
                self.changer
                    .with_tenants(projects.iter().map(String::as_str))
                    .ok()
            })
            .unwrap_or_else(|| self.changer.clone())
    }

    /// The same lifecycle with what follows a start or a restart. Without one, nothing follows.
    pub fn with_after_start(mut self, after_start: AfterStart) -> InstanceLifecycle {
        self.after_start = Some(after_start);
        self
    }

    /// The same lifecycle over another state writer, so a test can shorten its deadline.
    pub fn with_changer(mut self, changer: StateChanger) -> InstanceLifecycle {
        self.changer = changer;
        self
    }

    /// Reads the instance and checks that it is Arca's: in the kernel's project family, and a kernel object by its own
    /// config at the current retained set. Anything else is refused in plain words.
    pub fn target(&self, project: &str, instance: &str) -> Result<InstanceState, CallError> {
        if !self.changer().allows(project) {
            return Err(not_arca(
                project,
                instance,
                "its project is outside the kernel's project family",
            ));
        }
        let state = self
            .changer()
            .read(project, instance)
            .map_err(|error| match error {
                IncusError::Status { code: 404, .. } => refused(
                    "InstanceUnknown",
                    format!("no instance named {instance} exists in project {project}"),
                ),
                IncusError::Refused(why) => refused(
                    "InstanceUnknown",
                    format!("the instance {instance} in project {project} was not read: {why}"),
                ),
                other => refused(
                    "InstanceUnknown",
                    format!(
                        "the instance {instance} in project {project} could not be read: {other}"
                    ),
                ),
            })?;
        let retained = (self.retained)().map_err(|_| {
            refused(
                "InstanceUnknown",
                "the kernel's retained set could not be read, so the instance's owner is unknown",
            )
        })?;
        let object = ObjectRef {
            kind: IncusKind::Instance,
            project: state.project.clone(),
            parent: None,
            name: state.name.clone(),
            uuid: state.uuid.clone(),
        };
        match classify_object(&object, &state.own_config, &retained) {
            OwnerClass::Kernel { .. } => Ok(state),
            OwnerClass::Droidd => Err(not_arca(project, instance, "it is droidd's")),
            OwnerClass::Foreign => Err(not_arca(project, instance, "it carries no mark of Arca's")),
            OwnerClass::Contested => Err(not_arca(
                project,
                instance,
                "both droidd and Arca mark it, so nobody writes it",
            )),
            OwnerClass::Retained { .. } => Err(not_arca(
                project,
                instance,
                "its delete plan was declined, so Arca leaves it alone for good",
            )),
        }
    }

    /// Runs one action for the caller `token`, journalled before and after. `proposal` is the approved proposal whose
    /// grant the call consumed, when it consumed one. Returns the verb's result.
    pub fn run(
        &self,
        action: StateAction,
        project: &str,
        instance: &str,
        token: &str,
        proposal: Option<&str>,
        now_ms: u64,
    ) -> Result<Value, CallError> {
        let mut last = self.gate.lock().unwrap_or_else(PoisonError::into_inner);
        *last += 1;
        let id = format!("ia-{now_ms}-{}", *last);
        self.append(&json!({
            "kind": "begin",
            "id": id,
            "at_ms": now_ms,
            "token_id": token,
            "proposal": proposal,
            "action": action.as_str(),
            "project": project,
            "tenant": arca_kernel::ownership::tenant_of_project(project),
            "instance": instance,
        }))
        .map_err(|_| {
            refused(
                "InstanceJournalUnavailable",
                "the instance action journal could not be written, so nothing was changed",
            )
        })?;
        let outcome = self.act(action, project, instance);
        let end = match &outcome {
            Ok(done) => json!({
                "kind": "end",
                "id": id,
                "outcome": done.outcome,
                "code": null,
                "uuid": done.state.uuid,
                "status": done.state.status,
            }),
            Err(stopped) => json!({
                "kind": "end",
                "id": id,
                "outcome": match &stopped.refusal {
                    CallError::Refused { code, .. } if code.ends_with("Failed") => "failed",
                    _ => "refused",
                },
                "code": match &stopped.refusal {
                    CallError::Refused { code, .. } => code.as_str(),
                    CallError::Invalid(_) => "Invalid",
                    _ => "Internal",
                },
                "uuid": stopped.uuid,
                "status": stopped.was,
            }),
        };
        if self.append(&end).is_err() {
            eprintln!("arca: the end of instance action {id} was not journalled");
        }
        // The gate is not held while a guest is waited for: another instance's action need not wait for it.
        drop(last);
        let mut done = match outcome {
            Ok(done) => done,
            Err(stopped) => return Err(stopped.refusal),
        };
        if action != StateAction::Stop
            && done.state.status == "Running"
            && let Some(after_start) = &self.after_start
        {
            let followed = after_start(project, instance);
            // The journal names the secrets written, or that they were not. It never holds a value or a reason.
            if !matches!(followed, Ok(None)) {
                let row = json!({
                    "kind": "guest_secrets",
                    "id": id,
                    "outcome": if followed.is_ok() { "written" } else { "failed" },
                    "secrets": followed.as_ref().ok().and_then(Option::as_ref),
                });
                if self.append(&row).is_err() {
                    eprintln!("arca: the secrets of instance action {id} were not journalled");
                }
            }
            match followed {
                Ok(None) => {}
                Ok(Some(names)) => {
                    done.message = format!(
                        "{}. {}",
                        done.message,
                        crate::guest_secrets::written_words(&names)
                    );
                }
                Err(not_written) => {
                    // What to do next is said once, and only what is true: asking again helps when the guest was
                    // not ready, and changes nothing when something has to be put right first.
                    let next = if not_written.retry {
                        "Run instance_start for it again to write them; that never restarts it"
                    } else {
                        "Running instance_start again changes nothing until that is put right"
                    };
                    return Err(refused(
                        "InstanceSecretsNotWritten",
                        format!(
                            "{}. It is running, but its secrets were not written into the guest: {}. {next}",
                            done.message, not_written.why
                        ),
                    ));
                }
            }
        }
        Ok(json!({
            "project": done.state.project,
            "instance": done.state.name,
            "action": action.as_str(),
            "status": done.state.status,
            "changed": done.changed,
            "message": done.message,
        }))
    }

    fn act(&self, action: StateAction, project: &str, instance: &str) -> Result<Done, Stopped> {
        let seen = self.target(project, instance).map_err(|refusal| Stopped {
            refusal,
            uuid: None,
            was: None,
        })?;
        let named = format!("the instance {instance} in project {project}");
        let (send, changed_words, outcome) = match (action, seen.status.as_str()) {
            (StateAction::Start, "Running") => {
                return Ok(Done {
                    message: format!("{named} is already running; nothing was started"),
                    state: seen,
                    changed: false,
                    outcome: "already_running",
                });
            }
            (StateAction::Stop, "Stopped") => {
                return Ok(Done {
                    message: format!("{named} is already stopped; nothing was stopped"),
                    state: seen,
                    changed: false,
                    outcome: "already_stopped",
                });
            }
            (StateAction::Start, "Stopped") => {
                (StateAction::Start, format!("started {named}"), "started")
            }
            (StateAction::Stop, "Running") => {
                (StateAction::Stop, format!("stopped {named}"), "stopped")
            }
            (StateAction::Restart, "Running") => (
                StateAction::Restart,
                format!("restarted {named}"),
                "restarted",
            ),
            (StateAction::Restart, "Stopped") => (
                StateAction::Start,
                format!("{named} was stopped, so it was started"),
                "started_from_stopped",
            ),
            (_, status) => {
                return Err(Stopped {
                    refusal: refused(
                        "InstanceStateUnsupported",
                        format!(
                            "{named} is {}; Arca starts, stops and restarts only a running or a stopped instance",
                            shown_status(status)
                        ),
                    ),
                    uuid: seen.uuid.clone(),
                    was: Some(seen.status.clone()),
                });
            }
        };
        match self.changer().change(&seen, send) {
            Ok(state) => Ok(Done {
                state,
                changed: true,
                outcome,
                message: changed_words,
            }),
            // The writer refused before it sent the change: the instance was recreated after the owner check. Nothing was
            // sent, so this is a refusal and not a failed change.
            Err(IncusError::Refused(why)) => Err(Stopped {
                refusal: changed_refusal(&named, &why),
                uuid: seen.uuid.clone(),
                was: Some(seen.status.clone()),
            }),
            Err(error) => {
                // What the instance is now is read again, so the refusal says what is true and not what was asked.
                let now = self
                    .changer()
                    .read(project, instance)
                    .map(|state| shown_status(&state.status))
                    .unwrap_or_else(|_| "in an unknown state".to_string());
                let verb = match action {
                    StateAction::Start => "started",
                    StateAction::Stop => "stopped",
                    StateAction::Restart => "restarted",
                };
                let grace = if send == StateAction::Start {
                    String::new()
                } else {
                    format!(
                        " Arca gives it {GRACE_SECONDS} seconds to shut down and forces nothing."
                    )
                };
                Err(Stopped {
                    refusal: refused(
                        match action {
                            StateAction::Start => "InstanceStartFailed",
                            StateAction::Stop => "InstanceStopFailed",
                            StateAction::Restart => "InstanceRestartFailed",
                        },
                        format!("{named} was not {verb}: {error}. It is {now} now.{grace}"),
                    ),
                    uuid: seen.uuid.clone(),
                    was: Some(seen.status.clone()),
                })
            }
        }
    }

    /// Appends one line and syncs it. A journal that is not a private regular file is refused, never replaced.
    fn append(&self, row: &Value) -> std::io::Result<()> {
        match fs::symlink_metadata(&self.journal) {
            Ok(meta) if !meta.is_file() || meta.permissions().mode() & 0o777 != 0o600 => {
                return Err(std::io::Error::other(
                    "the instance action journal is not a private file",
                ));
            }
            Ok(meta) if meta.len() > MAX_JOURNAL_BYTES => {
                let mut aside = self.journal.clone().into_os_string();
                aside.push(".1");
                fs::rename(&self.journal, aside)?;
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.journal)?;
        let mut bytes = serde_json::to_vec(row).map_err(std::io::Error::other)?;
        bytes.push(b'\n');
        file.write_all(&bytes)?;
        file.sync_all()
    }
}

/// A status as a sentence shows it: Incus's own word in lower case, or a plain word when it is not one.
fn shown_status(status: &str) -> String {
    if !status.is_empty() && status.len() <= 32 && status.bytes().all(|b| b.is_ascii_alphabetic()) {
        status.to_ascii_lowercase()
    } else {
        "in an unknown state".to_string()
    }
}

fn refused(code: &str, message: impl Into<String>) -> CallError {
    CallError::Refused {
        code: code.to_string(),
        message: message.into(),
    }
}

/// The refusal of a change the writer did not send, because the instance is no longer the one that was checked.
fn changed_refusal(named: &str, why: &str) -> CallError {
    refused(
        "InstanceChanged",
        format!("{named} changed after it was checked ({why}); nothing was sent to it"),
    )
}

fn not_arca(project: &str, instance: &str, why: &str) -> CallError {
    refused(
        "InstanceNotArca",
        format!(
            "the instance {instance} in project {project} is not Arca's: {why}. Arca starts, stops and restarts only an instance it owns or has adopted"
        ),
    )
}

/// The refusal of a node with no state writer.
pub fn disabled() -> CallError {
    refused(
        "InstanceLifecycleDisabled",
        "starting, stopping and restarting instances is disabled: this node is watch-only",
    )
}

/// Checks a start, stop or restart proposal's instance when the proposal is made, so the owner is never shown a card for
/// an instance Arca would refuse. The action checks it again when it runs.
pub fn check_proposal(
    lifecycle: Option<&InstanceLifecycle>,
    project: &str,
    args: &Value,
) -> Result<(), CallError> {
    let lifecycle = lifecycle.ok_or_else(disabled)?;
    lifecycle.target(project, &instance_of(args)?).map(|_| ())
}

/// The Incus instance a call names: `instance`, or, inside a tenant, the instance of the service `service`
/// (`<service>-<tenant id>`). Exactly one of the two is given.
pub fn instance_of(args: &Value) -> Result<String, CallError> {
    match (
        args.get("instance").and_then(Value::as_str),
        args.get("service").and_then(Value::as_str),
        args.get("tenant").and_then(Value::as_str),
    ) {
        (Some(instance), None, _) => Ok(instance.to_string()),
        (None, Some(service), Some(tenant)) => Ok(format!("{service}-{tenant}")),
        (None, Some(_), None) => Err(CallError::Invalid(
            "service: only a call inside a tenant names a service; name the instance".to_string(),
        )),
        _ => Err(CallError::Invalid(
            "instance: name the instance, or inside a tenant its service, and not both".to_string(),
        )),
    }
}

/// The project a call's instance is in: the one `project` names, or the project of the tenant `tenant` names. With a
/// tenant the call acts in that tenant's project and in no other.
pub fn project_of(kernel: &Kernel, args: &Value) -> Result<String, CallError> {
    let Some(tenant) = args.get("tenant").and_then(Value::as_str) else {
        let project = args["project"].as_str().ok_or_else(|| {
            CallError::Invalid("project: required unless tenant names a tenant".to_string())
        })?;
        // A tenant's project is reached by naming the tenant, so the tenant's rules always apply to it: the tool
        // allowlist, and the journal line that names the tenant.
        if arca_kernel::ownership::tenant_of_project(project).is_some() {
            return Err(CallError::Invalid(
                "project: a tenant's project is named by tenant, not by project".to_string(),
            ));
        }
        return Ok(project.to_string());
    };
    let tenants: Vec<_> = kernel
        .entries()
        .iter()
        .filter(|entry| entry.plugin.0 == "arca.tenant")
        .collect();
    if tenants.is_empty() {
        return Err(refused("no_tenants", "this node has no tenants"));
    }
    let project = tenants
        .iter()
        .find(|entry| entry.id.0 == arca_kernel::ownership::tenant_entry(tenant))
        .and_then(|entry| entry.config["project"].as_str())
        .ok_or_else(|| refused("not_found", format!("tenant {tenant}: not found")))?;
    if args
        .get("project")
        .is_some_and(|named| named.as_str() != Some(project))
    {
        return Err(CallError::Invalid(
            "project: it is not the project of the tenant the call names".to_string(),
        ));
    }
    Ok(project.to_string())
}

/// `instance_start`, `instance_stop` and `instance_restart`. A start runs for a doctor. A stop and a restart take the
/// one-shot grant of an approved action proposal in `approval` and consume it before any work, so a direct call without
/// one is refused and an approval is spent once. The instance's owner is checked again here, at the moment of the write.
pub struct LifecycleHandler {
    pub action: ActionKind,
    pub kernel: Arc<Mutex<Kernel>>,
    pub clock: Clock,
    pub lifecycle: Option<Arc<InstanceLifecycle>>,
}

impl ToolHandler for LifecycleHandler {
    fn call(&self, args: Value, caller: &Caller) -> Result<Value, CallError> {
        if caller.scope < Scope::Doctor {
            return Err(CallError::Forbidden {
                verb: self.action.as_str().to_string(),
            });
        }
        let Some(lifecycle) = &self.lifecycle else {
            return Err(disabled());
        };
        let state_action = match self.action {
            ActionKind::InstanceStart => StateAction::Start,
            ActionKind::InstanceStop => StateAction::Stop,
            ActionKind::InstanceRestart => StateAction::Restart,
            _ => {
                return Err(CallError::Internal(
                    "the handler is not an instance action".to_string(),
                ));
            }
        };
        let mut bare = args;
        // Inside a tenant the three verbs need no approval, from a token whose tool allowlist names the verb: a stop
        // is reversible, and each action is journaled with the token and the tenant. Outside a tenant nothing changes:
        // a stop and a restart run only from an approved proposal.
        let in_tenant = bare.get("tenant").is_some();
        // A start takes no approval: the field is not in its schema, and stays in the call for the schema to refuse.
        let approval = if state_action == StateAction::Start {
            None
        } else {
            bare.as_object_mut()
                .and_then(|object| object.remove("approval"))
                .and_then(|value| value.as_str().map(str::to_string))
        };
        let grant = match (state_action, approval) {
            (StateAction::Start, _) if !in_tenant => None,
            (_, Some(grant)) if state_action != StateAction::Start => Some(grant),
            _ if in_tenant => {
                let verb = self.action.as_str();
                if !caller
                    .tools
                    .as_ref()
                    .is_some_and(|tools| tools.iter().any(|tool| tool == verb))
                {
                    return Err(refused(
                        "NotAllowed",
                        format!(
                            "{verb} inside a tenant is for a token whose tool allowlist names {verb}"
                        ),
                    ));
                }
                None
            }
            _ => return Err(CallError::Invalid("approval is required".to_string())),
        };
        let args = self
            .action
            .prepare(&bare, None)
            .map_err(CallError::Invalid)?;
        let mut proposal = None;
        // The kernel's lock is held for the lookup only: the instance read takes it again.
        let project = {
            let kernel = self.kernel.lock().map_err(|_| {
                CallError::Internal("the kernel could not complete the call".into())
            })?;
            project_of(&kernel, &args)
        }?;
        let instance = instance_of(&args)?;
        if let Some(grant) = grant {
            let mut kernel = self.kernel.lock().map_err(|_| {
                CallError::Internal("the kernel could not complete the call".into())
            })?;
            proposal = kernel.action_grant_id(&grant);
            if !kernel.consume_action_grant(self.action.as_str(), &args, &grant) {
                return Err(refused(
                    "NotApproved",
                    "this action runs only from an approved proposal: propose it with action_propose, and it runs once the owner approves it",
                ));
            }
        }
        lifecycle.run(
            state_action,
            &project,
            &instance,
            &caller.token_id,
            proposal.as_deref(),
            (self.clock)(),
        )
    }
}

/// The three instance verbs' handlers, keyed by verb name. `lifecycle` is `None` on a watch-only node, where every call is
/// refused.
pub fn handlers(
    kernel: Arc<Mutex<Kernel>>,
    clock: Clock,
    lifecycle: Option<Arc<InstanceLifecycle>>,
) -> Vec<(&'static str, Arc<dyn ToolHandler>)> {
    [
        ActionKind::InstanceStart,
        ActionKind::InstanceStop,
        ActionKind::InstanceRestart,
    ]
    .into_iter()
    .map(|action| {
        (
            action.as_str(),
            Arc::new(LifecycleHandler {
                action,
                kernel: Arc::clone(&kernel),
                clock: Arc::clone(&clock),
                lifecycle: lifecycle.clone(),
            }) as Arc<dyn ToolHandler>,
        )
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_change_the_writer_never_sent_is_a_refusal_and_not_a_failed_change() {
        let CallError::Refused { code, message } = changed_refusal(
            "the instance web in project p",
            "the instance was recreated since it was read",
        ) else {
            panic!("a refusal");
        };
        assert_eq!(code, "InstanceChanged");
        assert!(message.contains("nothing was sent"), "{message}");
        // The journal calls an outcome failed only for the codes of a change that was sent.
        assert!(!code.ends_with("Failed"));
    }
}
