//! The node's side of the host's declared forwarding and firewall: the alert input read from the root part's status file, and
//! the `host_network_restore` action, which asks the root part's socket to put back what the declared file says.
//!
//! The node never changes the host itself. The root part (`arca host-network run`, a system unit) does, and it answers on its
//! socket to two verbs only. This module calls `read` when a restore is proposed, so the owner is never asked to approve
//! what cannot run or has nothing to do, and `apply-declared-state` when the owner has approved it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use arca_alerts::eval::{HostNetworkItem, Timed};
use arca_api::Scope;
use arca_hostnet::client::{self, ClientError};
use arca_hostnet::status::{self, Status};
use arca_mcp::{CallError, Caller, ToolHandler};
use serde_json::{Value, json};

use crate::action_proposal::ActionKind;
use crate::kernel::Kernel;

/// The alert input of the status file at `path`: the items that are not as declared, stamped with the time the root part read
/// the host. `None` when there is no status file, so the rule is unknown on a node that has none.
pub fn alert_input(path: &Path) -> Option<Timed<Vec<HostNetworkItem>>> {
    let status = status::read(path)?;
    Some(Timed {
        at: status.at,
        value: status
            .drift
            .iter()
            .map(|drift| HostNetworkItem {
                item: drift.item.clone(),
            })
            .collect(),
    })
}

fn refused(code: &str, message: impl Into<String>) -> CallError {
    CallError::Refused {
        code: code.to_string(),
        message: message.into(),
    }
}

/// Why the root part could not be asked, in the owner's words.
fn unreachable(error: &ClientError, socket: &Path) -> CallError {
    let why = match error {
        ClientError::Unreachable(_) => "nothing answers on its socket",
        ClientError::Refused(_) => "it refused the request",
        ClientError::Malformed => "its answer was not one it gives",
    };
    refused(
        "HostNetworkUnavailable",
        format!(
            "this host's forwarding and firewall are not tended: {why} ({}). Install the root part with arca host-network install; see \"Host forwarding and the firewall\" in docs/OPERATING.md",
            socket.display()
        ),
    )
}

/// The drift of a status in words, for the card and the answer.
pub fn drift_words(status: &Status) -> String {
    status
        .drift
        .iter()
        .map(|drift| drift.detail.as_str())
        .collect::<Vec<_>>()
        .join("; ")
}

/// What a restore proposal's card says the host is now, read live from the root part. A host that has no declared state, or
/// is as declared already, is refused: there is nothing to approve.
pub fn proposal_fact(socket: &Path) -> Result<String, CallError> {
    let status = client::read(socket).map_err(|error| unreachable(&error, socket))?;
    if !status.declared && status.drift.is_empty() {
        return Err(refused(
            "HostNetworkNotDeclared",
            "this host has no declared forwarding and firewall state, so there is nothing to put back: declare it with arca host-network install",
        ));
    }
    if status.in_sync {
        return Err(refused(
            "NothingToRestore",
            "this host's forwarding and firewall are already as declared, so there is nothing to put back",
        ));
    }
    Ok(drift_words(&status))
}

/// The answer of an approved restore: the read-back after the root part applied the declared state.
pub fn answer(status: &Status) -> Value {
    let message = if status.in_sync {
        match status.applied.len() {
            0 => "the host was already as declared".to_string(),
            1 => "put back 1 setting; the host is as declared".to_string(),
            many => format!("put back {many} settings; the host is as declared"),
        }
    } else {
        format!(
            "the host still differs from what is declared: {}",
            drift_words(status)
        )
    };
    json!({
        "inSync": status.in_sync,
        "applied": status.applied,
        "drift": status.drift,
        "errors": status.errors,
        "at": status.at,
        "message": message,
    })
}

/// `host_network_restore`: runs only from an approved action proposal, once. It asks the root part to apply the declared
/// state and answers with what it read back.
pub struct HostNetworkRestore {
    pub kernel: Arc<Mutex<Kernel>>,
    pub socket: PathBuf,
}

impl ToolHandler for HostNetworkRestore {
    fn call(&self, args: Value, caller: &Caller) -> Result<Value, CallError> {
        let verb = ActionKind::HostNetworkRestore.as_str();
        if caller.scope < Scope::Doctor {
            return Err(CallError::Forbidden {
                verb: verb.to_string(),
            });
        }
        let mut bare = args;
        let grant = bare
            .as_object_mut()
            .and_then(|object| object.remove("approval"))
            .and_then(|value| value.as_str().map(str::to_string))
            .ok_or_else(|| CallError::Invalid("approval is required".to_string()))?;
        let args = ActionKind::HostNetworkRestore
            .prepare(&bare, None)
            .map_err(CallError::Invalid)?;
        // The root part is asked before the approval is spent, so an approval is never used up on a host that cannot act.
        client::read(&self.socket).map_err(|error| unreachable(&error, &self.socket))?;
        {
            let mut kernel = self.kernel.lock().map_err(|_| {
                CallError::Internal("the kernel could not complete the call".to_string())
            })?;
            if !kernel.consume_action_grant(verb, &args, &grant) {
                return Err(refused(
                    "NotApproved",
                    "this action runs only from an approved proposal: propose it with action_propose, and it runs once the owner approves it",
                ));
            }
        }
        let status = client::apply(&self.socket).map_err(|error| {
            refused(
                "HostNetworkRestoreFailed",
                format!(
                    "the approval was used, and the root part did not apply the declared state: {}. Propose the restore again once it answers",
                    match error {
                        ClientError::Unreachable(_) => "nothing answers on its socket",
                        ClientError::Refused(_) => "it refused the request",
                        ClientError::Malformed => "its answer was not one it gives",
                    }
                ),
            )
        })?;
        Ok(answer(&status))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_hostnet::status::DriftItem;

    fn status(in_sync: bool, applied: &[&str], drift: &[(&str, &str)]) -> Status {
        Status {
            at: 7,
            declared: true,
            in_sync,
            drift: drift
                .iter()
                .map(|(item, detail)| DriftItem {
                    item: (*item).to_string(),
                    detail: (*detail).to_string(),
                })
                .collect(),
            applied: applied.iter().map(|line| (*line).to_string()).collect(),
            errors: Vec::new(),
            version: "0.0.0".to_string(),
        }
    }

    #[test]
    fn the_alert_input_is_the_items_that_drifted_stamped_with_when_the_host_was_read() {
        let dir = std::env::temp_dir().join(format!("arca-host-input-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        let path = dir.join("status.json");
        assert_eq!(
            alert_input(&path),
            None,
            "no status file is an unknown reading"
        );
        status::write(
            &path,
            &status(
                false,
                &[],
                &[("ipv4-forwarding", "off"), ("rule-in:incusbr0", "gone")],
            ),
        )
        .expect("writes");
        let input = alert_input(&path).expect("a reading");
        assert_eq!(input.at, 7);
        let items: Vec<&str> = input.value.iter().map(|item| item.item.as_str()).collect();
        assert_eq!(items, ["ipv4-forwarding", "rule-in:incusbr0"]);
        status::write(&path, &status(true, &[], &[])).expect("writes");
        assert!(alert_input(&path).expect("a reading").value.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_answer_is_the_read_back_in_words() {
        let put_back = answer(&status(true, &["a", "b"], &[]));
        assert_eq!(put_back["inSync"], true);
        assert_eq!(
            put_back["message"],
            "put back 2 settings; the host is as declared"
        );
        assert_eq!(put_back["applied"], json!(["a", "b"]));
        assert_eq!(
            answer(&status(true, &["a"], &[]))["message"],
            "put back 1 setting; the host is as declared"
        );
        assert_eq!(
            answer(&status(true, &[], &[]))["message"],
            "the host was already as declared"
        );
        let still = answer(&status(
            false,
            &["a"],
            &[("forward-policy", "the FORWARD policy is ACCEPT")],
        ));
        assert_eq!(still["inSync"], false);
        assert_eq!(
            still["message"],
            "the host still differs from what is declared: the FORWARD policy is ACCEPT"
        );
    }
}
