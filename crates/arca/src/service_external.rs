//! `external_add` and `external_remove`: a node records a program that runs outside Incus, so `service_list` shows it.
//! Both take a doctor token and run at once, with no approval: they write one line in the node's own record
//! ([`crate::external`]) and change nothing on the machine. The two do not need Incus, so a node that only looks takes
//! them too.
use super::*;
use crate::external::{self, External};

/// How long a name of a tool may be, and the characters it may hold: a tool's name, kept as a label.
const HEALTH_MAX: usize = 64;

pub(super) struct ExternalHandler {
    pub(super) verb: &'static str,
    pub(super) kernel: Arc<Mutex<Kernel>>,
    pub(super) clock: Clock,
    pub(super) cfg: Arc<NodeConfig>,
    pub(super) writes: Arc<Mutex<()>>,
}

fn health_ok(health: &str) -> bool {
    (1..=HEALTH_MAX).contains(&health.len())
        && health
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

fn external_failed(why: String) -> CallError {
    CallError::Refused {
        code: "external_record".into(),
        message: format!("{why}. Nothing was changed"),
    }
}

/// What a call that names a recorded program is refused with, when the verb is one that acts on a service and the call
/// names no tenant: the record is the node's, and a tenant's service of the same name is another thing.
pub(super) fn recorded_refusal(cfg: &NodeConfig, verb: &str, args: &Value) -> Option<CallError> {
    if args.get("tenant").is_some()
        || !matches!(
            verb,
            "service_create"
                | "service_update"
                | "service_reconfigure"
                | "service_remove"
                | "service_import"
                | "service_release"
                | "service_publish"
                | "service_unpublish"
                | "service_connect"
                | "service_disconnect"
                | "service_secret_set"
                | "service_logs"
                | "function_create"
        )
    {
        return None;
    }
    let held = external::read(&cfg.state_dir).ok()?;
    ["name", "from", "to"].into_iter().find_map(|key| {
        let name = args.get(key)?.as_str()?;
        held.iter()
            .any(|record| record.name == name)
            .then(|| service_refused(external::refusal(name, verb)))
    })
}

impl ToolHandler for ExternalHandler {
    fn call(&self, args: Value, caller: &Caller) -> Result<Value, CallError> {
        let object = args
            .as_object()
            .ok_or_else(|| CallError::Invalid("arguments: expected an object".into()))?;
        let known: &[&str] = if self.verb == "external_add" {
            &["name", "kind", "address", "health"]
        } else {
            &["name"]
        };
        if let Some(key) = object.keys().find(|key| !known.contains(&key.as_str())) {
            return Err(CallError::Invalid(arca_api::unknown_field("", key, known)));
        }
        if caller.scope < Scope::Doctor {
            return Err(CallError::Forbidden {
                verb: self.verb.into(),
            });
        }
        let name = service_name(&args)?;
        // The write gate is the one a service create holds, so a name is never taken by both at once.
        let _write = self.writes.lock().map_err(|_| internal())?;
        let now = (self.clock)();
        let dir = &self.cfg.state_dir;
        if self.verb == "external_remove" {
            let removed = external::remove(dir, &name, now).map_err(external_failed)?;
            if !removed {
                return Err(service_not_found(format!(
                    "{name} is not a recorded program: nothing was changed"
                )));
            }
            return Ok(json!({ "removed": true, "name": name }));
        }
        if args["kind"].as_str() != Some(external::HOST_PROGRAM) {
            return Err(CallError::Invalid(format!(
                "kind: expected {}",
                external::HOST_PROGRAM
            )));
        }
        let address = args["address"]
            .as_str()
            .ok_or_else(|| CallError::Invalid("address: expected an http or https URL".into()))?;
        external::host_port(address).map_err(CallError::Invalid)?;
        let health = match args.get("health") {
            None => None,
            Some(health) => Some(
                health
                    .as_str()
                    .filter(|health| health_ok(health))
                    .ok_or_else(|| {
                        CallError::Invalid(
                            "health: expected the name of a tool, 1 to 64 letters, digits, _, - or .".into(),
                        )
                    })?
                    .to_string(),
            ),
        };
        // One name space: a service or a function that holds the name keeps it.
        let entries = lock(&self.kernel)?.entries().to_vec();
        // The id a service create checks, whatever project the service is in: an imported service holds its name too.
        let id = crate::service::entry_id(None, &name);
        if entries
            .iter()
            .any(|entry| entry.id.0 == id && entry.plugin.0 == "arca.service")
        {
            return Err(service_refused(format!(
                "a service or function named {name} exists: services, functions and recorded programs share one name space. Nothing was changed"
            )));
        }
        let record = External {
            name,
            kind: external::HOST_PROGRAM.to_string(),
            address: address.to_string(),
            health,
        };
        external::add(dir, record.clone(), now).map_err(external_failed)?;
        let mut done = json!({
            "recorded": true,
            "name": record.name,
            "kind": record.kind,
            "address": record.address
        });
        if let Some(health) = &record.health {
            done["health"] = json!(health);
        }
        Ok(done)
    }
}
