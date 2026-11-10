//! The verb table: every Arca API verb with its class, the minimum scope that may call it, whether it writes, and the exact
//! JSON Schemas of its arguments and its result. The OpenRPC document and the MCP tool list are both read from this table.
//!
//! Params objects are closed (`additionalProperties: false`). Result objects are open: a client must accept fields it does
//! not know, so a later release can add a field without a break.

use serde::Serialize;
use serde_json::{Map, Value, json};

/// The class of a verb. Every initial verb is `L`: it runs on one node and needs no second node or vote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum VerbClass {
    L,
    B,
    Q,
}

impl VerbClass {
    /// The class as the document and the table spell it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::L => "L",
            Self::B => "B",
            Self::Q => "Q",
        }
    }
}

/// The scope a bearer token carries. The order is the order of privilege: a higher scope may call every verb a lower one
/// may.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Viewer,
    Doctor,
    Operator,
}

impl Scope {
    /// The scope as a token file and the document spell it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Doctor => "doctor",
            Self::Operator => "operator",
        }
    }
}

/// One verb of the API. `params` and `result` return the verb's JSON Schemas; `params` is a closed object and `result` an
/// open one.
#[derive(Debug, Clone, Copy)]
pub struct Verb {
    pub name: &'static str,
    pub summary: &'static str,
    pub class: VerbClass,
    pub min_scope: Scope,
    pub writes: bool,
    pub destructive: bool,
    pub params: fn() -> Value,
    pub result: fn() -> Value,
    /// The result may carry text produced outside Arca; the MCP door marks it.
    pub untrusted: bool,
    /// When the result is not an object, the MCP door puts it under this key.
    pub output_key: Option<&'static str>,
    /// The one-line text summary of a result in the MCP door.
    pub summary_text: fn(&Value) -> String,
    /// What the MCP door cuts for its cap, and then marks `truncated`.
    pub cut: fn(&Value) -> Option<Value>,
}

impl Verb {
    /// Whether the MCP door advertises `readOnlyHint`. A verb that records a durable proposal is not read-only even
    /// though it changes no desired state, so it keeps the viewer scope while saying that it writes a record.
    pub fn read_only_hint(&self) -> bool {
        !self.writes && self.name != "action_propose"
    }
}

static VERBS: [Verb; 35] = [
    Verb {
        name: "arca_status",
        summary: "Node status: the API version, the node id, the mode, the uptime, whether Incus answers and the last applied sequence number.",
        class: VerbClass::L,
        min_scope: Scope::Viewer,
        writes: false,
        destructive: false,
        untrusted: false,
        output_key: None,
        summary_text: summary_status,
        cut: no_cut,
        params: arca_status_params,
        result: arca_status_result,
    },
    Verb {
        name: "node_metrics",
        summary: "Host metrics of the node: the latest sample and the mean over the requested minutes.",
        class: VerbClass::L,
        min_scope: Scope::Viewer,
        writes: false,
        destructive: false,
        untrusted: false,
        output_key: None,
        summary_text: summary_metrics,
        cut: no_cut,
        params: node_metrics_params,
        result: node_metrics_result,
    },
    Verb {
        name: "watch_report",
        summary: "Every instance in every project with its ownership class, and the drift between marks and desired state.",
        class: VerbClass::L,
        min_scope: Scope::Viewer,
        writes: false,
        destructive: false,
        untrusted: false,
        output_key: None,
        summary_text: summary_watch_report,
        cut: no_cut,
        params: watch_report_params,
        result: watch_report_result,
    },
    Verb {
        name: "token_list",
        summary: "The interim API tokens by id and scope. Never a secret or a hash.",
        class: VerbClass::L,
        min_scope: Scope::Operator,
        writes: false,
        destructive: false,
        untrusted: false,
        output_key: None,
        summary_text: summary_tokens,
        cut: no_cut,
        params: token_list_params,
        result: token_list_result,
    },
    Verb {
        name: "journal_list",
        summary: "The newest record for each effect in the journal, paged by effect id. It names only the intent and object fields, never a pre-image or inverse.",
        class: VerbClass::L,
        min_scope: Scope::Viewer,
        writes: false,
        destructive: false,
        untrusted: false,
        output_key: None,
        summary_text: summary_journal,
        cut: no_cut,
        params: journal_list_params,
        result: journal_list_result,
    },
    Verb {
        name: "plan_list",
        summary: "The delete plans and action proposals waiting for an owner's approval. Delete plans carry the exact statement the owner signs; action proposals carry their action, arguments and kernel-built summary.",
        class: VerbClass::L,
        min_scope: Scope::Viewer,
        writes: false,
        destructive: false,
        untrusted: false,
        output_key: None,
        summary_text: summary_plans,
        cut: no_cut,
        params: plan_list_params,
        result: plan_list_result,
    },
    Verb {
        name: "action_propose",
        summary: "Records an inert service, database, GPU assignment, storage or snapshot action proposal for the owner to approve with a passkey. Nothing changes until the owner approves it.",
        class: VerbClass::L,
        min_scope: Scope::Viewer,
        writes: false,
        destructive: false,
        untrusted: false,
        output_key: None,
        summary_text: summary_action_propose,
        cut: no_cut,
        params: action_propose_params,
        result: action_propose_result,
    },
    Verb {
        name: "plan_decline",
        summary: "Declines a waiting delete plan. Its objects are retained, and the decline is final.",
        class: VerbClass::L,
        min_scope: Scope::Doctor,
        writes: true,
        destructive: false,
        untrusted: false,
        output_key: None,
        summary_text: summary_decline,
        cut: no_cut,
        params: plan_decline_params,
        result: plan_decline_result,
    },
    Verb {
        name: "approval_submit",
        summary: "Submits an owner's approval for a waiting delete plan, which deletes the plan's objects once. The approval is never echoed.",
        class: VerbClass::L,
        min_scope: Scope::Doctor,
        writes: true,
        destructive: true,
        untrusted: false,
        output_key: None,
        summary_text: summary_approval,
        cut: no_cut,
        params: approval_submit_params,
        result: approval_submit_result,
    },
    Verb {
        name: "action_approve",
        summary: "Submits an owner's passkey approval for a waiting action proposal, which runs its action once. The approval is checked by this kernel's own verifier and never echoed; an approval page on the same machine relays a linked host's approvals here.",
        class: VerbClass::L,
        min_scope: Scope::Doctor,
        writes: true,
        destructive: true,
        untrusted: false,
        output_key: None,
        summary_text: summary_action_approve,
        cut: no_cut,
        params: action_approve_params,
        result: action_approve_result,
    },
    Verb {
        name: "peer_keys_sync",
        summary: "Keeps the owner passkeys of the page host `approval.peerKeys` names, for action approvals only. The first set is kept as is; a changed set needs a passkey approval by a key already kept, and without one the answer is the statement to sign. Lines are never echoed.",
        class: VerbClass::L,
        min_scope: Scope::Doctor,
        writes: true,
        destructive: false,
        untrusted: false,
        output_key: None,
        summary_text: summary_peer_keys_sync,
        cut: no_cut,
        params: peer_keys_sync_params,
        result: peer_keys_sync_result,
    },
    Verb {
        name: "alerts_list",
        summary: "The node's alerts: the firing ones, those whose data is unknown, the disabled rules, the recent resolves, and the store's health.",
        class: VerbClass::L,
        min_scope: Scope::Viewer,
        writes: false,
        destructive: false,
        untrusted: false,
        output_key: None,
        summary_text: summary_alerts,
        cut: no_cut,
        params: alerts_list_params,
        result: alerts_list_result,
    },
    Verb {
        name: "postgres_create",
        summary: "Create a service database.",
        class: VerbClass::L,
        min_scope: Scope::Doctor,
        writes: true,
        destructive: false,
        untrusted: false,
        output_key: None,
        summary_text: summary_pg_create,
        cut: no_cut,
        params: postgres_create_params,
        result: postgres_create_result,
    },
    Verb {
        name: "postgres_list",
        summary: "List a service database.",
        class: VerbClass::L,
        min_scope: Scope::Viewer,
        writes: false,
        destructive: false,
        untrusted: false,
        output_key: None,
        summary_text: summary_pg_list,
        cut: no_cut,
        params: postgres_list_params,
        result: postgres_list_result,
    },
    Verb {
        name: "postgres_link",
        summary: "Link a service database.",
        class: VerbClass::L,
        min_scope: Scope::Doctor,
        writes: true,
        destructive: false,
        untrusted: false,
        output_key: None,
        summary_text: summary_pg_link,
        cut: no_cut,
        params: postgres_link_params,
        result: postgres_link_result,
    },
    Verb {
        name: "postgres_unlink",
        summary: "Unlink a service database.",
        class: VerbClass::L,
        min_scope: Scope::Doctor,
        writes: true,
        destructive: false,
        untrusted: false,
        output_key: None,
        summary_text: summary_pg_unlink,
        cut: no_cut,
        params: postgres_unlink_params,
        result: postgres_unlink_result,
    },
    Verb {
        name: "postgres_delete",
        summary: "Delete a service database.",
        class: VerbClass::L,
        min_scope: Scope::Doctor,
        writes: true,
        destructive: true,
        untrusted: false,
        output_key: None,
        summary_text: summary_pg_delete,
        cut: no_cut,
        params: postgres_delete_params,
        result: postgres_delete_result,
    },
    Verb {
        name: "ontology_query",
        summary: "The ownership, status and outgoing calls for an item in Arca's public ontology.",
        class: VerbClass::L,
        min_scope: Scope::Viewer,
        writes: false,
        destructive: false,
        untrusted: false,
        output_key: None,
        summary_text: summary_ontology,
        cut: no_cut,
        params: ontology_query_params,
        result: ontology_query_result,
    },
    Verb {
        name: "service_create",
        summary: "Create a service from a template, in the service project and under the kernel's mark.",
        class: VerbClass::L,
        min_scope: Scope::Doctor,
        writes: true,
        destructive: false,
        untrusted: false,
        output_key: None,
        summary_text: summary_service_create,
        cut: no_cut,
        params: service_create_params,
        result: service_create_result,
    },
    Verb {
        name: "service_list",
        summary: "The services in the service project, each with its address, ports, links and state.",
        class: VerbClass::L,
        min_scope: Scope::Viewer,
        writes: false,
        destructive: false,
        untrusted: false,
        output_key: None,
        summary_text: summary_service_list,
        cut: cut_service_list,
        params: service_list_params,
        result: service_list_result,
    },
    Verb {
        name: "service_status",
        summary: "One service's address, ports, links and state.",
        class: VerbClass::L,
        min_scope: Scope::Viewer,
        writes: false,
        destructive: false,
        untrusted: false,
        output_key: None,
        summary_text: summary_service_status,
        cut: no_cut,
        params: service_status_params,
        result: service_status_result,
    },
    Verb {
        name: "service_logs",
        summary: "The last lines of a service's log, cleaned and with every stored secret redacted.",
        class: VerbClass::L,
        min_scope: Scope::Viewer,
        writes: false,
        destructive: false,
        untrusted: true,
        output_key: None,
        summary_text: summary_service_logs,
        cut: cut_service_logs,
        params: service_logs_params,
        result: service_logs_result,
    },
    Verb {
        name: "service_remove",
        summary: "Remove a service, its links, its ACL and its secrets, with the owner's signed approval.",
        class: VerbClass::L,
        min_scope: Scope::Doctor,
        writes: true,
        destructive: true,
        untrusted: false,
        output_key: None,
        summary_text: summary_service_remove,
        cut: no_cut,
        params: service_remove_params,
        result: service_remove_result,
    },
    Verb {
        name: "service_connect",
        summary: "Link a consumer to a provider: the provider's network ACL lets the consumer's address reach its first port, and nothing else, and the consumer's environment gets what the provider's template provides (DATABASE_URL, say); the environment applies on its next start. Returns the variable names only.",
        class: VerbClass::L,
        min_scope: Scope::Doctor,
        writes: true,
        destructive: false,
        untrusted: false,
        output_key: None,
        summary_text: summary_service_connect,
        cut: no_cut,
        params: service_connect_params,
        result: service_connect_result,
    },
    Verb {
        name: "service_disconnect",
        summary: "Remove a link: the provider's ACL rule for the consumer goes, and so do the variables it gave the consumer (applied on its next start). Returns their names.",
        class: VerbClass::L,
        min_scope: Scope::Doctor,
        writes: true,
        destructive: false,
        untrusted: false,
        output_key: None,
        summary_text: summary_service_disconnect,
        cut: no_cut,
        params: service_disconnect_params,
        result: service_disconnect_result,
    },
    Verb {
        name: "service_secret_set",
        summary: "Set one of a service's secrets (its template lists them). Write-only: no tool, log or audit row shows it again. The service's environment, and its consumers', is updated for their next start.",
        class: VerbClass::L,
        min_scope: Scope::Doctor,
        writes: true,
        destructive: false,
        untrusted: false,
        output_key: None,
        summary_text: summary_service_secret_set,
        cut: no_cut,
        params: service_secret_set_params,
        result: service_secret_set_result,
    },
    Verb {
        name: "service_publish",
        summary: "Publish a service's dash on the owner's tailnet (droidd's own) and nowhere else: the service joins it as its own node, arca-<name> (tag:arca-svc), and serves HTTPS on port 8443 only, to its dash. Needs the service's tailscale_authkey secret (the owner's tagged, pre-approved Tailscale auth key, service_secret_set) and the owner's signed approval (arca-approve, action publish, target the service, its arguments' digest), which an agent asks the owner for and relays. Returns the node and its https address.",
        class: VerbClass::L,
        min_scope: Scope::Doctor,
        writes: true,
        destructive: false,
        untrusted: false,
        output_key: None,
        summary_text: summary_service_publish,
        cut: no_cut,
        params: service_publish_params,
        result: service_publish_result,
    },
    Verb {
        name: "service_unpublish",
        summary: "Take a service's dash off the tailnet: it stops serving 8443, its node logs out and leaves the tailnet, and tailscaled stops. Runs whether or not a publish finished.",
        class: VerbClass::L,
        min_scope: Scope::Doctor,
        writes: true,
        destructive: false,
        untrusted: false,
        output_key: None,
        summary_text: summary_service_unpublish,
        cut: no_cut,
        params: service_unpublish_params,
        result: service_unpublish_result,
    },
    Verb {
        name: "gpu_attach",
        summary: "Attach a physical GPU card to an Incus instance, by PCI address, with the owner's signed approval. A card bound to vfio-pci is refused for a container.",
        class: VerbClass::L,
        min_scope: Scope::Doctor,
        writes: true,
        destructive: false,
        untrusted: false,
        output_key: None,
        summary_text: summary_gpu,
        cut: no_cut,
        params: gpu_attach_params,
        result: gpu_change_result,
    },
    Verb {
        name: "gpu_detach",
        summary: "Detach a physical GPU card from an Incus instance, by PCI address, with the owner's signed approval.",
        class: VerbClass::L,
        min_scope: Scope::Doctor,
        writes: true,
        destructive: true,
        untrusted: false,
        output_key: None,
        summary_text: summary_gpu,
        cut: no_cut,
        params: gpu_detach_params,
        result: gpu_change_result,
    },
    Verb {
        name: "storage_list",
        summary: "The host's disks, each with a plain role (system, snapshots, in use or free) and what it may be offered as, and the Incus storage pools with their used and total space.",
        class: VerbClass::L,
        min_scope: Scope::Viewer,
        writes: false,
        destructive: false,
        untrusted: false,
        output_key: None,
        summary_text: summary_storage_list,
        cut: no_cut,
        params: empty_params,
        result: storage_list_result,
    },
    Verb {
        name: "storage_add",
        summary: "Add snapshot storage, with the owner's signed approval: a folder on a mounted disk, or a whole free disk that Incus wipes and formats as the pool arca-snapshots-<name>. A system disk, a member disk, a disk that holds snapshots and a mounted disk are refused as a whole-disk target.",
        class: VerbClass::L,
        min_scope: Scope::Doctor,
        writes: true,
        destructive: true,
        untrusted: false,
        output_key: None,
        summary_text: summary_storage_add,
        cut: no_cut,
        params: storage_add_params,
        result: storage_add_result,
    },
    Verb {
        name: "snapshot_create",
        summary: "Copy one app or data volume onto snapshot storage, with the owner's signed approval. The copy lives in the arcak-snapshots project, never starts on its own, and the source is never changed.",
        class: VerbClass::L,
        min_scope: Scope::Doctor,
        writes: true,
        destructive: false,
        untrusted: false,
        output_key: None,
        summary_text: summary_snapshot_change,
        cut: no_cut,
        params: snapshot_create_params,
        result: snapshot_change_result,
    },
    Verb {
        name: "snapshot_list",
        summary: "Every snapshot copy in the arcak-snapshots project: its source, pool, size and when it was made. An app copy whose clean-up never finished is incomplete.",
        class: VerbClass::L,
        min_scope: Scope::Viewer,
        writes: false,
        destructive: false,
        untrusted: false,
        output_key: None,
        summary_text: summary_snapshot_list,
        cut: no_cut,
        params: empty_params,
        result: snapshot_list_result,
    },
    Verb {
        name: "snapshot_delete",
        summary: "Delete one snapshot copy from the arcak-snapshots project, with the owner's signed approval. Nothing outside that project is touched.",
        class: VerbClass::L,
        min_scope: Scope::Doctor,
        writes: true,
        destructive: true,
        untrusted: false,
        output_key: None,
        summary_text: summary_snapshot_change,
        cut: no_cut,
        params: snapshot_delete_params,
        result: snapshot_change_result,
    },
];

/// Every verb of the table, in table order.
pub fn verbs() -> &'static [Verb] {
    &VERBS
}

/// The default one-line summary for a verb without a richer one.
pub fn summary_done(_: &Value) -> String {
    "done".to_string()
}

/// A short line for a successful `arca_status`.
pub fn summary_status(v: &Value) -> String {
    format!(
        "arca {} ({} mode)",
        v["version"].as_str().unwrap_or("?"),
        v["mode"].as_str().unwrap_or("?")
    )
}

/// A short line for a successful `node_metrics`.
pub fn summary_metrics(v: &Value) -> String {
    format!("host metrics for {} minute(s)", v["minutes"])
}

/// A short line for a successful `watch_report`.
pub fn summary_watch_report(v: &Value) -> String {
    let projects = v["projects"].as_array().map_or(0, Vec::len);
    let instances: usize = v["projects"].as_array().map_or(0, |list| {
        list.iter()
            .map(|project| project["instances"].as_array().map_or(0, Vec::len))
            .sum()
    });
    format!("{instances} instance(s) in {projects} project(s)")
}

/// A short line for a successful `token_list`.
pub fn summary_tokens(v: &Value) -> String {
    format!("{} token(s)", v["tokens"].as_array().map_or(0, Vec::len))
}

/// A short line for a successful `journal_list`.
pub fn summary_journal(v: &Value) -> String {
    format!(
        "{} journal record(s)",
        v["records"].as_array().map_or(0, Vec::len)
    )
}

/// A short line for a successful `plan_list`.
pub fn summary_plans(v: &Value) -> String {
    format!(
        "{} waiting plan(s)",
        v["plans"].as_array().map_or(0, Vec::len)
    )
}

/// A short line for a successful `action_propose`.
pub fn summary_action_propose(v: &Value) -> String {
    format!(
        "{} proposed: {}",
        v["id"].as_str().unwrap_or("?"),
        v["summary"].as_str().unwrap_or("?")
    )
}

/// A short line for a successful `plan_decline`.
pub fn summary_decline(v: &Value) -> String {
    format!(
        "plan {} declined; {} object(s) retained",
        v["plan"].as_str().unwrap_or("?"),
        v["retained"].as_array().map_or(0, Vec::len)
    )
}

/// A short line for a successful `approval_submit`.
pub fn summary_approval(v: &Value) -> String {
    format!(
        "plan {} submitted; {} deleted, {} drifted, partial={}",
        v["plan"].as_str().unwrap_or("?"),
        v["deleted"].as_array().map_or(0, Vec::len),
        v["drift"].as_array().map_or(0, Vec::len),
        v["partial"]
    )
}

/// A short line for a successful `action_approve`.
pub fn summary_action_approve(v: &Value) -> String {
    format!(
        "action proposal {} approved; {} ran",
        v["plan"].as_str().unwrap_or("?"),
        v["action"].as_str().unwrap_or("?")
    )
}

/// A short line for a successful `peer_keys_sync`.
pub fn summary_peer_keys_sync(v: &Value) -> String {
    format!(
        "peer passkeys {} ({} kept)",
        v["status"].as_str().unwrap_or("?"),
        v["keys"]
    )
}

/// A short line for a successful `alerts_list`.
pub fn summary_alerts(v: &Value) -> String {
    format!(
        "{} firing, {} unknown, {} disabled",
        v["firing"].as_array().map_or(0, Vec::len),
        v["unknown"].as_array().map_or(0, Vec::len),
        v["disabled"].as_array().map_or(0, Vec::len)
    )
}

/// A short line for a successful `postgres_create`.
pub fn summary_pg_create(v: &Value) -> String {
    format!(
        "database {} ({}) {}",
        v["name"].as_str().unwrap_or("?"),
        v["entry"].as_str().unwrap_or("?"),
        v["status"].as_str().unwrap_or("?")
    )
}

/// A short line for a successful `postgres_list`.
pub fn summary_pg_list(v: &Value) -> String {
    format!(
        "{} database(s)",
        v["databases"].as_array().map_or(0, Vec::len)
    )
}

/// A short line for a successful `postgres_link`.
pub fn summary_pg_link(v: &Value) -> String {
    format!(
        "database {} linked to {} ({} variable(s))",
        v["name"].as_str().unwrap_or("?"),
        v["consumer"].as_str().unwrap_or("?"),
        v["env"].as_array().map_or(0, Vec::len)
    )
}

/// A short line for a successful `postgres_unlink`.
pub fn summary_pg_unlink(v: &Value) -> String {
    format!(
        "database {} unlinked from {} ({} variable(s) removed)",
        v["name"].as_str().unwrap_or("?"),
        v["consumer"].as_str().unwrap_or("?"),
        v["removed"].as_array().map_or(0, Vec::len)
    )
}

/// A short line for a successful `postgres_delete`.
pub fn summary_pg_delete(v: &Value) -> String {
    if v["removed"].as_bool().unwrap_or(false) {
        "database removed".to_string()
    } else {
        "database delete planned".to_string()
    }
}

/// A short line for a successful `ontology_query`.
pub fn summary_ontology(v: &Value) -> String {
    format!(
        "{} is {} with {} edge(s)",
        v["id"].as_str().unwrap_or("?"),
        v["status"].as_str().unwrap_or("unknown"),
        v["edges"].as_array().map_or(0, Vec::len)
    )
}

/// The default cut: the result is already within its cap.
pub fn no_cut(_: &Value) -> Option<Value> {
    None
}

/// The verb with this name, if the table has one.
pub fn verb(name: &str) -> Option<&'static Verb> {
    VERBS.iter().find(|v| v.name == name)
}

/// Whether a token of `scope` may call `v`. The rule is the scope order alone; the table keeps viewers off write verbs by
/// giving every write verb a minimum scope of `Doctor` or higher.
pub fn may_call(scope: Scope, v: &Verb) -> bool {
    scope >= v.min_scope
}

// The JSON Schema builders. A builder yields exactly the subset `check::conforms` reads.

/// One named field of an object: its name, whether it is required, and its schema.
type Field = (&'static str, bool, Value);

fn string() -> Value {
    json!({ "type": "string" })
}

fn int() -> Value {
    json!({ "type": "integer" })
}

fn num() -> Value {
    json!({ "type": "number" })
}

fn boolean() -> Value {
    json!({ "type": "boolean" })
}

/// A 64-character string. The format (lowercase hex) is checked by the handlers, not by the schema.
fn hex64() -> Value {
    json!({ "type": "string", "minLength": 64, "maxLength": 64 })
}

/// A string of at most `max` characters.
fn string_max(max: i64) -> Value {
    json!({ "type": "string", "maxLength": max })
}

fn one_of(values: &[&str]) -> Value {
    json!({ "type": "string", "enum": values })
}

fn bounded_int(min: i64, max: i64) -> Value {
    json!({ "type": "integer", "minimum": min, "maximum": max })
}

fn at_least(min: i64) -> Value {
    json!({ "type": "integer", "minimum": min })
}

fn array(items: Value) -> Value {
    json!({ "type": "array", "items": items })
}

/// The schema with `null` added to its types, as `T|null`.
fn nullable(mut schema: Value) -> Value {
    if let Some(obj) = schema.as_object_mut() {
        let base = obj.get("type").cloned().unwrap_or(Value::Null);
        obj.insert("type".to_string(), json!([base, "null"]));
    }
    schema
}

fn object_of(fields: Vec<Field>, closed: bool) -> Value {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for (name, is_required, schema) in fields {
        if is_required {
            required.push(Value::from(name));
        }
        properties.insert(name.to_string(), schema);
    }
    let mut obj = Map::new();
    obj.insert("type".to_string(), Value::from("object"));
    obj.insert("properties".to_string(), Value::Object(properties));
    if !required.is_empty() {
        obj.insert("required".to_string(), Value::Array(required));
    }
    if closed {
        obj.insert("additionalProperties".to_string(), Value::Bool(false));
    }
    Value::Object(obj)
}

/// A params object: closed, so an argument the schema does not name is refused.
fn params(fields: Vec<Field>) -> Value {
    object_of(fields, true)
}

/// A result object: open, so a client accepts a field it does not know.
fn result(fields: Vec<Field>) -> Value {
    object_of(fields, false)
}

fn arca_status_params() -> Value {
    params(vec![])
}

fn arca_status_result() -> Value {
    result(vec![
        ("version", true, string()),
        ("node", true, hex64()),
        ("mode", true, one_of(&["watch-only", "kernel"])),
        ("uptimeS", true, at_least(0)),
        ("incusReachable", true, boolean()),
        ("appliedSeq", true, nullable(int())),
        ("metricsSource", true, one_of(&["local", "remote"])),
        ("metricsSourceReason", true, string()),
        (
            "approval",
            false,
            result(vec![
                ("linkedHosts", true, array(string())),
                (
                    "trustedKeyFiles",
                    true,
                    array(result(vec![
                        ("index", true, at_least(0)),
                        ("name", true, string()),
                        ("keys", true, at_least(0)),
                        ("reason", true, nullable(string())),
                    ])),
                ),
                (
                    "peerKeys",
                    false,
                    result(vec![
                        ("from", true, string()),
                        ("keys", true, at_least(0)),
                        ("since", true, nullable(string())),
                        ("reason", true, string()),
                    ]),
                ),
            ]),
        ),
    ])
}

fn node_metrics_params() -> Value {
    params(vec![("minutes", false, bounded_int(1, 60))])
}

fn node_metrics_result() -> Value {
    result(vec![
        ("at", true, int()),
        ("minutes", true, int()),
        ("latest", true, nullable(host_sample())),
        ("mean", true, nullable(host_sample())),
    ])
}

/// One host sample. Every key is required; a measurement the host cannot give is `null`, not absent.
fn host_sample() -> Value {
    result(vec![
        ("at", true, int()),
        (
            "cpu",
            true,
            result(vec![
                ("usedPct", true, nullable(num())),
                ("cores", true, int()),
                ("coresKnown", true, boolean()),
                ("reason", true, nullable(string())),
            ]),
        ),
        (
            "load",
            true,
            result(vec![
                ("one", true, nullable(num())),
                ("five", true, nullable(num())),
                ("fifteen", true, nullable(num())),
                ("reason", true, nullable(string())),
            ]),
        ),
        (
            "memory",
            true,
            result(vec![
                ("totalBytes", true, nullable(int())),
                ("availableBytes", true, nullable(int())),
                ("usedBytes", true, nullable(int())),
                ("swapTotalBytes", true, nullable(int())),
                ("swapFreeBytes", true, nullable(int())),
                ("reason", true, nullable(string())),
            ]),
        ),
        (
            "psi",
            true,
            result(vec![
                ("memorySomeAvg10", true, nullable(num())),
                ("memoryFullAvg10", true, nullable(num())),
                ("cpuSomeAvg10", true, nullable(num())),
            ]),
        ),
        ("cpuTempC", true, nullable(num())),
        ("gpuBusyPct", true, nullable(num())),
        ("gpu", true, gpu_section()),
        (
            "pools",
            true,
            array(result(vec![
                ("name", true, string()),
                ("usedBytes", true, nullable(int())),
                ("totalBytes", true, nullable(int())),
                ("freeBytes", true, nullable(int())),
            ])),
        ),
        (
            "network",
            true,
            nullable(result(vec![
                ("rxBytesPerSec", true, num()),
                ("txBytesPerSec", true, num()),
            ])),
        ),
        (
            "projects",
            true,
            array(result(vec![
                ("project", true, string()),
                ("total", true, int()),
                ("running", true, int()),
            ])),
        ),
    ])
}

/// The GPU part of a host sample: the inventory, per-card telemetry and the attributed compute processes, all keyed by the
/// PCI address. A card that is in the inventory but has no telemetry carries a reason, never a zero.
fn gpu_section() -> Value {
    result(vec![
        ("reason", true, nullable(string())),
        ("skipped", true, nullable(string())),
        (
            "cards",
            true,
            array(result(vec![
                ("pci", true, string()),
                ("uuid", true, nullable(string())),
                ("vendor", true, string()),
                ("product", true, string()),
                ("driver", true, nullable(string())),
                ("driverVersion", true, nullable(string())),
                ("numaNode", true, int()),
                ("vramTotalBytes", true, nullable(int())),
                (
                    "use",
                    true,
                    one_of(&["driver-bound", "vfio-pci", "no-driver"]),
                ),
                ("telemetry", true, nullable(gpu_telemetry())),
                ("reason", true, nullable(string())),
                (
                    "processes",
                    true,
                    array(result(vec![
                        ("pci", true, string()),
                        ("pid", true, int()),
                        ("vramBytes", true, nullable(int())),
                        ("owner", true, gpu_owner()),
                    ])),
                ),
                ("processReason", true, nullable(string())),
            ])),
        ),
    ])
}

fn gpu_telemetry() -> Value {
    result(vec![
        ("utilizationPct", true, nullable(num())),
        ("memoryUsedBytes", true, nullable(int())),
        ("memoryTotalBytes", true, nullable(int())),
        ("powerDrawW", true, nullable(num())),
        ("powerLimitW", true, nullable(num())),
        ("temperatureC", true, nullable(num())),
        ("smClockMhz", true, nullable(int())),
        ("eventReasons", true, nullable(string())),
    ])
}

fn gpu_owner() -> Value {
    result(vec![
        ("kind", true, one_of(&["instance", "container", "host"])),
        ("project", false, string()),
        ("name", false, string()),
        ("container", false, string()),
    ])
}

fn watch_report_params() -> Value {
    params(vec![])
}

fn watch_report_result() -> Value {
    result(vec![
        ("at", true, int()),
        ("ageMs", false, int()),
        (
            "projects",
            true,
            array(result(vec![
                ("project", true, string()),
                (
                    "instances",
                    true,
                    array(result(vec![
                        ("name", true, string()),
                        ("kind", true, one_of(&["container", "virtual-machine"])),
                        ("status", true, string()),
                        (
                            "owner",
                            true,
                            result(vec![
                                (
                                    "class",
                                    true,
                                    one_of(&[
                                        "droidd",
                                        "foreign",
                                        "kernel",
                                        "retained",
                                        "contested",
                                    ]),
                                ),
                                ("entry", false, string()),
                                ("plugin", false, string()),
                                ("plan", false, hex64()),
                            ]),
                        ),
                        ("privileged", true, boolean()),
                        ("identity", true, boolean()),
                        ("configHash", true, hex64()),
                    ])),
                ),
            ])),
        ),
        (
            "drift",
            true,
            array(result(vec![
                (
                    "kind",
                    true,
                    one_of(&["markedWithoutEntry", "entryWithoutObject"]),
                ),
                ("entry", true, string()),
                ("project", false, string()),
                ("name", false, string()),
            ])),
        ),
    ])
}

fn token_list_params() -> Value {
    params(vec![])
}

fn token_list_result() -> Value {
    result(vec![(
        "tokens",
        true,
        array(result(vec![
            ("id", true, string()),
            ("scope", true, one_of(&["viewer", "doctor", "operator"])),
            ("tools", false, array(string())),
        ])),
    )])
}

fn journal_list_params() -> Value {
    params(vec![("after", false, int()), ("limit", false, int())])
}

fn journal_list_result() -> Value {
    result(vec![(
        "records",
        true,
        array(result(vec![
            ("id", true, int()),
            ("entry", true, string()),
            ("class", true, one_of(&["A", "B", "C"])),
            ("target", true, object_ref()),
            ("plugin", false, string()),
            (
                "state",
                true,
                one_of(&["Intent", "Applied", "Confirmed", "Reverted", "Failed"]),
            ),
            ("at_ms", true, int()),
            ("intent", false, one_of(&["Delete"])),
            ("object", false, object_ref()),
        ])),
    )])
}

fn plan_list_params() -> Value {
    params(vec![])
}

fn plan_list_result() -> Value {
    result(vec![(
        "plans",
        true,
        json!({ "type": "array", "items": json!({ "anyOf": [plan(), action_proposal()] }) }),
    )])
}

/// The twelve verbs an action proposal may name. The list is closed here and checked by the handler again.
const ACTION_NAMES: [&str; 12] = [
    "service_create",
    "service_remove",
    "service_publish",
    "service_unpublish",
    "service_connect",
    "service_disconnect",
    "postgres_create",
    "gpu_attach",
    "gpu_detach",
    "storage_add",
    "snapshot_create",
    "snapshot_delete",
];

/// A plan or action proposal id: a delete plan's 64 hex digits, or an action proposal's `act-` and twelve.
fn plan_ref() -> Value {
    json!({
        "maxLength": 68,
        "minLength": 16,
        "pattern": "^(?:[0-9a-f]{64}|act-[0-9a-f]{12})$",
        "type": "string"
    })
}

fn action_propose_params() -> Value {
    params(vec![
        ("action", true, one_of(&ACTION_NAMES)),
        ("args", true, json!({ "type": "object" })),
    ])
}

fn action_propose_result() -> Value {
    action_proposal()
}

/// An action proposal. `statement` is optional because `plan_list` omits it; the open approval page adds it.
fn action_proposal() -> Value {
    result(vec![
        ("kind", true, json!({ "const": "action", "type": "string" })),
        (
            "id",
            true,
            json!({
                "maxLength": 16,
                "minLength": 16,
                "pattern": "^act-[0-9a-f]{12}$",
                "type": "string"
            }),
        ),
        ("action", true, one_of(&ACTION_NAMES)),
        ("summary", true, string()),
        ("details", true, array(detail())),
        ("args", true, json!({ "type": "object" })),
        ("argsDigest", true, hex64()),
        ("createdAt", true, int()),
        ("expiresAt", true, int()),
        ("statement", false, string()),
        ("host", false, string()),
    ])
}

/// One argument the owner is shown for an action proposal.
fn detail() -> Value {
    result(vec![("label", true, string()), ("value", true, string())])
}

/// A delete plan: its digest, its entry, its times, the objects it deletes, the digest of its arguments and the exact
/// statement the owner signs.
fn plan() -> Value {
    result(vec![
        ("plan", true, hex64()),
        ("entry", true, string()),
        ("createdAt", true, int()),
        ("expiresAt", true, int()),
        ("objects", true, array(object_ref())),
        ("paramsDigest", true, hex64()),
        ("statement", true, string()),
    ])
}

/// One Incus object a plan names. `uuid` is null for a kind that has none.
fn object_ref() -> Value {
    result(vec![
        (
            "kind",
            true,
            one_of(&[
                "project", "instance", "network", "acl", "profile", "volume", "zone", "forward",
                "bucket",
            ]),
        ),
        ("project", true, string()),
        ("name", true, string()),
        ("uuid", true, nullable(string())),
    ])
}

fn plan_decline_params() -> Value {
    params(vec![("plan", true, plan_ref())])
}

fn plan_decline_result() -> Value {
    result(vec![
        ("plan", true, plan_ref()),
        ("retained", true, array(object_ref())),
    ])
}

fn approval_submit_params() -> Value {
    params(vec![
        ("plan", true, hex64()),
        ("approval", true, string_max(16_384)),
    ])
}

fn approval_submit_result() -> Value {
    result(vec![
        ("plan", true, hex64()),
        ("deleted", true, array(object_ref())),
        ("drift", true, array(object_ref())),
        ("partial", true, boolean()),
    ])
}

fn action_approve_params() -> Value {
    params(vec![
        ("plan", true, action_id()),
        ("approval", true, string_max(16_384)),
    ])
}

fn action_approve_result() -> Value {
    result(vec![
        ("plan", true, action_id()),
        ("id", true, action_id()),
        ("action", true, one_of(&ACTION_NAMES)),
        ("result", true, json!({ "type": "object" })),
    ])
}

fn peer_keys_sync_params() -> Value {
    params(vec![
        (
            "lines",
            true,
            json!({ "type": "array", "items": string_max(2048), "maxItems": 64 }),
        ),
        ("statement", false, string_max(4096)),
        ("approval", false, string_max(8192)),
    ])
}

fn peer_keys_sync_result() -> Value {
    result(vec![
        (
            "status",
            true,
            one_of(&["trusted", "unchanged", "needs_approval", "changed"]),
        ),
        ("keys", true, at_least(0)),
        ("statement", false, string()),
    ])
}

/// An action proposal id: `act-` and twelve lowercase hex digits.
fn action_id() -> Value {
    json!({
        "maxLength": 16,
        "minLength": 16,
        "pattern": "^act-[0-9a-f]{12}$",
        "type": "string"
    })
}

fn alerts_list_params() -> Value {
    params(vec![])
}

fn alerts_list_result() -> Value {
    result(vec![
        (
            "firing",
            true,
            array(alert_event(vec![("unknown", true, boolean())])),
        ),
        (
            "unknown",
            true,
            array(result(vec![
                ("rule", true, string()),
                ("labels", true, labels()),
                ("state", true, one_of(&["ok", "pending", "firing"])),
                ("reason", true, one_of(&["missing", "stale", "unavailable"])),
            ])),
        ),
        ("disabled", true, array(string())),
        ("resolvedRecent", true, array(alert_event(vec![]))),
        ("sampleAgeMs", true, nullable(int())),
        ("watchAgeMs", true, nullable(int())),
        ("commitMs", true, nullable(int())),
        (
            "storeError",
            true,
            nullable(result(vec![
                ("at", true, int()),
                ("message", true, string()),
            ])),
        ),
        ("storeReset", true, boolean()),
        ("storeBytes", true, int()),
        ("healthLine", true, string()),
    ])
}

/// An `arca.alert.v1` event, with `extra` fields. Only a resolve carries `reason`.
fn alert_event(extra: Vec<Field>) -> Value {
    let mut fields = vec![
        ("schema", true, string()),
        ("seq", true, nullable(int())),
        ("alertId", true, nullable(int())),
        ("node", true, string()),
        ("rule", true, string()),
        ("severity", true, one_of(&["critical", "warning"])),
        ("state", true, one_of(&["firing", "resolved"])),
        ("labels", true, labels()),
        ("value", true, nullable(num())),
        ("threshold", true, nullable(num())),
        ("startedAt", true, int()),
        ("resolvedAt", true, nullable(int())),
        ("at", true, int()),
        (
            "reason",
            false,
            one_of(&["cleared", "rule-disabled", "subject-gone", "no data"]),
        ),
    ];
    fields.extend(extra);
    result(fields)
}

/// An alert's own labels: an open object of names to strings, which the subset checks only as an object.
fn labels() -> Value {
    result(vec![])
}

fn postgres_create_params() -> Value {
    params(vec![
        ("name", true, string_max(31)),
        ("image", true, string_max(512)),
    ])
}
fn postgres_create_result() -> Value {
    result(vec![
        ("name", true, string()),
        ("entry", true, string()),
        ("status", true, one_of(&["pending", "running"])),
    ])
}
fn postgres_list_params() -> Value {
    params(vec![])
}
fn postgres_list_result() -> Value {
    result(vec![(
        "databases",
        true,
        array(result(vec![
            ("name", true, string()),
            ("entry", true, string()),
            ("status", true, string()),
            ("address", true, nullable(string())),
            ("port", true, int()),
            ("links", true, array(string())),
        ])),
    )])
}
fn postgres_link_params() -> Value {
    params(vec![
        ("name", true, string_max(31)),
        ("consumer", true, string_max(63)),
    ])
}
fn postgres_link_result() -> Value {
    result(vec![
        ("name", true, string()),
        ("consumer", true, string()),
        ("env", true, array(string())),
    ])
}
fn postgres_unlink_params() -> Value {
    postgres_link_params()
}
fn postgres_unlink_result() -> Value {
    result(vec![
        ("name", true, string()),
        ("consumer", true, string()),
        ("removed", true, array(string())),
    ])
}
fn postgres_delete_params() -> Value {
    params(vec![("name", true, string_max(31))])
}
fn postgres_delete_result() -> Value {
    result(vec![
        ("plan", true, nullable(plan())),
        ("removed", true, boolean()),
    ])
}

fn ontology_query_params() -> Value {
    params(vec![("id", true, string_max(128))])
}

fn nullable_enum(values: &[&str]) -> Value {
    json!({ "anyOf": [one_of(values), { "type": "null" }] })
}

fn ontology_query_result() -> Value {
    let statuses = &["live", "planned", "deferred"];
    let edge = result(vec![
        ("id", true, string()),
        (
            "to",
            true,
            one_of(&[
                "arca",
                "droidd",
                "dsh",
                "omni",
                "arca-id",
                "automagik",
                "weunfold",
                "herdr",
            ]),
        ),
        (
            "contract",
            true,
            one_of(&["mcp", "api", "file", "token", "socket"]),
        ),
        ("status", true, one_of(statuses)),
    ]);
    result(vec![
        ("id", true, string_max(128)),
        (
            "kind",
            true,
            nullable_enum(&[
                "tool",
                "incus-project",
                "incus-mark",
                "data",
                "unit",
                "token-file",
                "passkey-name",
            ]),
        ),
        ("owner", true, nullable_enum(&["arca"])),
        ("status", true, nullable_enum(statuses)),
        ("edges", true, array(edge)),
        ("neverOwns", true, boolean()),
    ])
}

/// A short line for a successful `service_create`.
pub fn summary_service_create(v: &Value) -> String {
    format!(
        "service {} ({}) {} at {}",
        v["name"].as_str().unwrap_or("?"),
        v["template"].as_str().unwrap_or("?"),
        v["state"].as_str().unwrap_or("?"),
        v["ipv4"].as_str().unwrap_or("?")
    )
}

/// A short line for a successful `service_list`.
pub fn summary_service_list(v: &Value) -> String {
    format!(
        "{} service(s)",
        v["services"].as_array().map_or(0, Vec::len)
    )
}

/// A short line for a successful `service_status`.
pub fn summary_service_status(v: &Value) -> String {
    format!(
        "service {} ({}) {} at {}",
        v["name"].as_str().unwrap_or("?"),
        v["template"].as_str().unwrap_or("?"),
        v["state"].as_str().unwrap_or("?"),
        v["ipv4"].as_str().unwrap_or("?")
    )
}

/// The shared natural service view: the golden's `ServiceViewOut`, closed at every object.
pub fn service_view_schema() -> Value {
    json!({
        "additionalProperties": false,
        "properties": {
            "ipv4": {
                "description": "Its fixed address on the service bridge.",
                "maxLength": 15,
                "type": "string"
            },
            "links": {
                "description": "The services it is linked to, as a consumer, and the port each link opens.",
                "items": {
                    "additionalProperties": false,
                    "properties": {
                        "port": { "maximum": 65535, "minimum": 1, "type": "integer" },
                        "to": { "maxLength": 40, "type": "string" }
                    },
                    "required": ["to", "port"],
                    "type": "object"
                },
                "maxItems": 64,
                "type": "array"
            },
            "name": { "maxLength": 40, "type": "string" },
            "ports": {
                "description": "The ports it listens on; a link opens the first.",
                "items": { "maximum": 65535, "minimum": 1, "type": "integer" },
                "maxItems": 16,
                "type": "array"
            },
            "lanPort": {
                "anyOf": [
                    { "maximum": 65535, "minimum": 1, "type": "integer" },
                    { "type": "null" }
                ],
                "description": "Its host port when the template has a lan proxy, or null."
            },
            "published": {
                "anyOf": [
                    {
                        "additionalProperties": false,
                        "properties": {
                            "node": { "maxLength": 63, "type": "string" },
                            "port": { "const": 8443, "type": "number" },
                            "reach": { "const": "tailnet", "type": "string" }
                        },
                        "required": ["reach", "node", "port"],
                        "type": "object"
                    },
                    { "type": "null" }
                ],
                "description": "Its dash on the tailnet (service_publish), or null."
            },
            "state": {
                "description": "failed: Incus reports an error, or its instance is gone; unknown: an instance droidd did not create, which it leaves alone.",
                "enum": ["creating", "running", "stopped", "failed", "unknown"],
                "type": "string"
            },
            "template": {
                "description": "Its template (empty for an instance droidd did not create).",
                "maxLength": 40,
                "type": "string"
            }
        },
        "required": ["name", "template", "state", "ipv4", "ports", "links", "published", "lanPort"],
        "type": "object"
    })
}

/// A short line for a successful `gpu_attach` or `gpu_detach`.
pub fn summary_gpu(v: &Value) -> String {
    let state = if v["attached"].as_bool() == Some(true) {
        "attached"
    } else {
        "detached"
    };
    format!(
        "GPU {} {} on the instance {} in project {}",
        v["pci"].as_str().unwrap_or("?"),
        state,
        v["instance"].as_str().unwrap_or("?"),
        v["project"].as_str().unwrap_or("?")
    )
}

/// An Incus instance name, as Incus accepts one: a hostname.
fn instance_name_schema() -> Value {
    json!({
        "description": "The Incus instance name.",
        "maxLength": 63,
        "minLength": 1,
        "pattern": "^[a-zA-Z0-9](?:[a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?$",
        "type": "string"
    })
}

/// A PCI address in Incus's spelling: an optional four lowercase domain hex digits, two bus, two slot and one function.
/// The kernel normalizes it before the proposal is recorded.
fn pci_schema() -> Value {
    json!({
        "description": "The card's PCI address, such as 0000:01:00.0 or 01:00.0. It is the card's identity.",
        "pattern": "^(?:[0-9a-f]{4}:)?[0-9a-f]{2}:[0-9a-f]{2}\\.[0-7]$",
        "type": "string"
    })
}

fn approval_schema() -> Value {
    json!({
        "description": "The one-shot execution grant an approved action proposal arms. A direct call carries none that works.",
        "maxLength": 16384,
        "minLength": 1,
        "type": "string"
    })
}

/// A GPU action's params. `project`, `kind` and `uuid` are resolved by the kernel when the proposal is made and are part
/// of the approved arguments; a direct caller names only the instance and the card.
fn gpu_params() -> Value {
    params(vec![
        ("instance", true, instance_name_schema()),
        ("pci", true, pci_schema()),
        ("project", false, instance_name_schema()),
        ("kind", false, one_of(&["container", "virtual-machine"])),
        ("uuid", false, nullable(string())),
        ("approval", true, approval_schema()),
    ])
}

fn gpu_attach_params() -> Value {
    gpu_params()
}

fn gpu_detach_params() -> Value {
    gpu_params()
}

fn gpu_change_result() -> Value {
    result(vec![
        ("instance", true, string()),
        ("project", true, string()),
        ("pci", true, string()),
        ("device", true, string()),
        ("attached", true, boolean()),
        ("changed", true, boolean()),
    ])
}

fn service_name_schema(description: &'static str) -> Value {
    json!({
        "description": description,
        "pattern": "^[a-z](?:[a-z0-9-]{0,38}[a-z0-9])?$",
        "type": "string"
    })
}

fn service_create_params() -> Value {
    json!({
        "additionalProperties": false,
        "properties": {
            "name": service_name_schema("The service name (1-40 of a-z, 0-9 and inner \"-\", starting with a letter)."),
            "template": {
                "description": "The template's name.",
                "pattern": "^[a-z](?:[a-z0-9-]{0,38}[a-z0-9])?$",
                "type": "string"
            }
        },
        "required": ["template", "name"],
        "type": "object"
    })
}

fn service_create_result() -> Value {
    service_view_schema()
}

fn service_list_params() -> Value {
    json!({ "additionalProperties": false, "properties": {}, "required": [], "type": "object" })
}

fn service_list_result() -> Value {
    let mut view = service_view_schema();
    if let Some(object) = view.as_object_mut() {
        object.remove("description");
    }
    json!({
        "additionalProperties": false,
        "properties": {
            "services": {
                "items": view,
                "maxItems": 256,
                "type": "array"
            }
        },
        "required": ["services"],
        "type": "object"
    })
}

fn service_status_params() -> Value {
    json!({
        "additionalProperties": false,
        "properties": {
            "name": service_name_schema("The service name (1-40 of a-z, 0-9 and inner \"-\", starting with a letter).")
        },
        "required": ["name"],
        "type": "object"
    })
}

fn service_status_result() -> Value {
    service_view_schema()
}

/// A short line for a successful `service_logs`.
pub fn summary_service_logs(v: &Value) -> String {
    let count = v["lines"].as_array().map_or(0, Vec::len);
    format!("{count} log line(s), untrusted (the service wrote them).")
}

/// The logs' cut: the oldest lines and the characters beyond each line's cap.
pub fn cut_service_logs(v: &Value) -> Option<Value> {
    let lines = v["lines"].as_array()?;
    let mut out = Vec::new();
    let start = lines.len().saturating_sub(200);
    for line in &lines[start..] {
        let text = line.as_str()?;
        let mut cut: String = text.chars().take(2000).collect();
        if cut.is_empty() && !text.is_empty() {
            cut = "…".to_string();
        }
        out.push(Value::String(cut));
    }
    if start > 0 || out.iter().zip(&lines[start..]).any(|(a, b)| a != b) {
        Some(json!({ "lines": out }))
    } else {
        None
    }
}

/// Caps the service rows while leaving their natural result schema unchanged.
pub fn cut_service_list(v: &Value) -> Option<Value> {
    let rows = v["services"].as_array()?;
    (rows.len() > 256).then(|| json!({ "services": &rows[..256] }))
}

/// Describes the service data omitted by the MCP cap, without adding natural result fields.
pub fn service_cut_description(name: &str, original: &Value) -> Option<String> {
    match name {
        "service_logs" => {
            let lines = original["lines"].as_array()?;
            let dropped = lines.len().saturating_sub(200);
            let shortened = lines[dropped..]
                .iter()
                .filter(|line| {
                    line.as_str()
                        .is_some_and(|text| text.chars().count() > 2000)
                })
                .count();
            Some(format!(
                "{dropped} line(s) dropped, {shortened} line(s) truncated of {} original line(s)",
                lines.len()
            ))
        }
        "service_list" => {
            let rows = original["services"].as_array()?;
            Some(format!(
                "{} service row(s) omitted of {} original row(s)",
                rows.len().saturating_sub(256),
                rows.len()
            ))
        }
        _ => None,
    }
}

/// A short line for a successful `service_remove`.
pub fn summary_service_remove(_: &Value) -> String {
    "Service removed.".to_string()
}

fn service_logs_params() -> Value {
    json!({
        "additionalProperties": false,
        "properties": {
            "lines": {
                "default": 50,
                "description": "How many lines, 1-200 (default 50).",
                "maximum": 200,
                "minimum": 1,
                "type": "integer"
            },
            "name": service_name_schema("The service name (1-40 of a-z, 0-9 and inner \"-\", starting with a letter).")
        },
        "required": ["name"],
        "type": "object"
    })
}

fn service_logs_result() -> Value {
    json!({
        "additionalProperties": false,
        "properties": {
            "lines": {
                "items": { "maxLength": 2000, "type": "string" },
                "maxItems": 200,
                "type": "array"
            }
        },
        "required": ["lines"],
        "type": "object"
    })
}

fn service_remove_params() -> Value {
    json!({
        "additionalProperties": false,
        "properties": {
            "approval": {
                "description": "The owner's approval: <base64 statement>.<base64 signature>.",
                "maxLength": 16384,
                "minLength": 1,
                "type": "string"
            },
            "name": service_name_schema("The service name (1-40 of a-z, 0-9 and inner \"-\", starting with a letter).")
        },
        "required": ["name", "approval"],
        "type": "object"
    })
}

fn service_remove_result() -> Value {
    json!({
        "additionalProperties": false,
        "properties": { "removed": { "const": true, "type": "boolean" } },
        "required": ["removed"],
        "type": "object"
    })
}

fn service_connect_params() -> Value {
    serde_json::from_str(r##"{"additionalProperties":false,"properties":{"from":{"description":"The consumer: the service whose environment gets the provider's variables.","pattern":"^[a-z](?:[a-z0-9-]{0,38}[a-z0-9])?$","type":"string"},"to":{"description":"The provider: the service whose first port the link opens.","pattern":"^[a-z](?:[a-z0-9-]{0,38}[a-z0-9])?$","type":"string"}},"required":["from","to"],"type":"object"}"##).expect("fixed service schema")
}
fn service_connect_result() -> Value {
    serde_json::from_str(r##"{"additionalProperties":false,"properties":{"env":{"description":"The environment variable names, never their values.","items":{"maxLength":64,"type":"string"},"maxItems":64,"type":"array"}},"required":["env"],"type":"object"}"##).expect("fixed service schema")
}
fn summary_service_connect(_: &Value) -> String {
    "service_connect: complete".into()
}
fn service_disconnect_params() -> Value {
    serde_json::from_str(r##"{"additionalProperties":false,"properties":{"from":{"description":"The consumer: the service whose environment gets the provider's variables.","pattern":"^[a-z](?:[a-z0-9-]{0,38}[a-z0-9])?$","type":"string"},"to":{"description":"The provider: the service whose first port the link opens.","pattern":"^[a-z](?:[a-z0-9-]{0,38}[a-z0-9])?$","type":"string"}},"required":["from","to"],"type":"object"}"##).expect("fixed service schema")
}
fn service_disconnect_result() -> Value {
    serde_json::from_str(r##"{"additionalProperties":false,"properties":{"env":{"description":"The environment variable names, never their values.","items":{"maxLength":64,"type":"string"},"maxItems":64,"type":"array"}},"required":["env"],"type":"object"}"##).expect("fixed service schema")
}
fn summary_service_disconnect(_: &Value) -> String {
    "service_disconnect: complete".into()
}
fn service_secret_set_params() -> Value {
    serde_json::from_str(r##"{"additionalProperties":false,"properties":{"key":{"description":"The secret's name, one of the template's secrets.","pattern":"^[A-Za-z_][A-Za-z0-9_]{0,63}$","type":"string"},"name":{"description":"The service name (1-40 of a-z, 0-9 and inner \"-\", starting with a letter).","pattern":"^[a-z](?:[a-z0-9-]{0,38}[a-z0-9])?$","type":"string"},"value":{"description":"The secret, 8 to 4096 characters.","maxLength":4096,"minLength":8,"type":"string"}},"required":["name","key","value"],"type":"object"}"##).expect("fixed service schema")
}
fn service_secret_set_result() -> Value {
    serde_json::from_str(r##"{"additionalProperties":false,"properties":{"ok":{"const":true,"type":"boolean"}},"required":["ok"],"type":"object"}"##).expect("fixed service schema")
}
fn summary_service_secret_set(_: &Value) -> String {
    "service_secret_set: complete".into()
}
fn service_publish_params() -> Value {
    serde_json::from_str(r##"{"additionalProperties":false,"properties":{"approval":{"description":"The owner's approval: <base64 statement>.<base64 signature>.","maxLength":16384,"minLength":1,"type":"string"},"name":{"description":"The service name.","pattern":"^[a-z](?:[a-z0-9-]{0,38}[a-z0-9])?$","type":"string"},"reach":{"description":"Where the dash is published: \"tailnet\", the only reach there is.","maxLength":32,"type":"string"}},"required":["name","reach","approval"],"type":"object"}"##).expect("fixed service schema")
}
fn service_publish_result() -> Value {
    serde_json::from_str(r##"{"additionalProperties":false,"properties":{"node":{"description":"The tailnet node the dash is served from: arca-<name>.","maxLength":63,"type":"string"},"url":{"description":"Its address on the tailnet: https://<node>.<tailnet>.ts.net:8443/ (no dash token: the dash asks for its own).","maxLength":300,"type":"string"}},"required":["node","url"],"type":"object"}"##).expect("fixed service schema")
}
fn summary_service_publish(_: &Value) -> String {
    "service_publish: complete".into()
}
fn service_unpublish_params() -> Value {
    serde_json::from_str(r##"{"additionalProperties":false,"properties":{"name":{"description":"The service name.","pattern":"^[a-z](?:[a-z0-9-]{0,38}[a-z0-9])?$","type":"string"}},"required":["name"],"type":"object"}"##).expect("fixed service schema")
}
fn service_unpublish_result() -> Value {
    serde_json::from_str(r##"{"additionalProperties":false,"properties":{"unpublished":{"const":true,"type":"boolean"}},"required":["unpublished"],"type":"object"}"##).expect("fixed service schema")
}
fn summary_service_unpublish(_: &Value) -> String {
    "service_unpublish: complete".into()
}

/// A params object with no fields.
fn empty_params() -> Value {
    params(vec![])
}

/// A short line for a successful `storage_list`.
pub fn summary_storage_list(v: &Value) -> String {
    let disks = v["disks"].as_array().map_or(0, Vec::len);
    let free = v["disks"].as_array().map_or(0, |disks| {
        disks
            .iter()
            .filter(|disk| disk["offer"].as_str().is_some())
            .count()
    });
    format!(
        "{disks} disk(s), {free} offered for snapshots, {} pool(s)",
        v["pools"].as_array().map_or(0, Vec::len)
    )
}

/// A short line for a successful `storage_add`.
pub fn summary_storage_add(v: &Value) -> String {
    format!(
        "snapshot storage {} added ({})",
        v["pool"].as_str().unwrap_or("?"),
        v["driver"].as_str().unwrap_or("?")
    )
}

/// A short line for a successful `snapshot_list`.
pub fn summary_snapshot_list(v: &Value) -> String {
    format!(
        "{} snapshot(s)",
        v["snapshots"].as_array().map_or(0, Vec::len)
    )
}

/// A short line for a successful `snapshot_create` or `snapshot_delete`.
pub fn summary_snapshot_change(v: &Value) -> String {
    format!(
        "snapshot {} {}",
        v["snapshot"].as_str().unwrap_or("?"),
        v["state"].as_str().unwrap_or("?")
    )
}

/// An Incus storage pool name, as `storage_add` creates one.
fn pool_name_schema() -> Value {
    json!({
        "description": "The storage pool name (1-32 of a-z, 0-9 and inner \"-\", starting with a letter).",
        "pattern": "^[a-z](?:[a-z0-9-]{0,30}[a-z0-9])?$",
        "type": "string"
    })
}

/// A plain Incus name: letters, digits, `-`, `_` and `.`.
fn plain_name_schema(description: &'static str) -> Value {
    json!({
        "description": description,
        "maxLength": 128,
        "minLength": 1,
        "pattern": "^[A-Za-z0-9][A-Za-z0-9_.-]*$",
        "type": "string"
    })
}

/// `storage_add`: exactly one of `path` and `device`. The disk's facts are read by the kernel when the proposal is made and
/// are part of the approved arguments, so a disk swapped in afterwards is refused.
fn storage_add_params() -> Value {
    params(vec![
        ("name", true, pool_name_schema()),
        (
            "path",
            false,
            json!({
                "description": "A folder on a mounted disk that is not a system disk. The pool lives in <path>/arca-pool, which Arca makes (empty, 0700) when the approved add runs; Arca must be able to write in the folder.",
                "maxLength": 1024,
                "minLength": 1,
                "pattern": "^/",
                "type": "string"
            }),
        ),
        (
            "device",
            false,
            json!({
                "description": "A whole free disk, such as sdb or /dev/sdb. Incus wipes and formats it as btrfs; the pool is named arca-snapshots-<name> (at most 32 characters).",
                "pattern": "^(?:/dev/)?[a-zA-Z0-9][a-zA-Z0-9_.-]{0,63}$",
                "type": "string"
            }),
        ),
        ("disk", false, string_max(64)),
        ("fsUuid", false, string_max(128)),
        ("model", false, string_max(256)),
        ("serial", false, string_max(256)),
        ("transport", false, string_max(32)),
        ("size", false, int()),
        ("contents", false, string_max(4096)),
        ("mount", false, string_max(1024)),
        ("approval", true, approval_schema()),
    ])
}

fn storage_add_result() -> Value {
    result(vec![
        ("pool", true, string()),
        ("driver", true, one_of(&["dir", "btrfs"])),
        ("source", true, string()),
    ])
}

/// `snapshot_create`: exactly one of `instance` and `volume`. The snapshot's name, the source's kind and identity, and a
/// volume's pool are resolved by the kernel when the proposal is made.
fn snapshot_create_params() -> Value {
    params(vec![
        (
            "project",
            true,
            plain_name_schema("The source's Incus project."),
        ),
        ("instance", false, instance_name_schema()),
        (
            "volume",
            false,
            plain_name_schema("The source custom volume."),
        ),
        ("pool", true, pool_name_schema()),
        (
            "name",
            false,
            plain_name_schema("The snapshot's name, set by the kernel."),
        ),
        (
            "kind",
            false,
            one_of(&["container", "virtual-machine", "volume"]),
        ),
        (
            "sourcePool",
            false,
            plain_name_schema("A source volume's pool."),
        ),
        ("uuid", false, nullable(string())),
        ("approval", true, approval_schema()),
    ])
}

/// `snapshot_delete`: the snapshot's name. Its kind, pool and source are resolved by the kernel when the proposal is made.
fn snapshot_delete_params() -> Value {
    params(vec![
        ("name", true, plain_name_schema("The snapshot's name.")),
        ("kind", false, one_of(&["instance", "volume"])),
        ("pool", false, plain_name_schema("The snapshot's pool.")),
        ("source", false, string_max(512)),
        ("approval", true, approval_schema()),
    ])
}

fn snapshot_change_result() -> Value {
    result(vec![
        ("snapshot", true, string()),
        ("pool", true, string()),
        ("kind", true, string()),
        (
            "state",
            true,
            one_of(&["copied", "copying", "deleted", "deleting"]),
        ),
    ])
}

fn storage_list_result() -> Value {
    let partition = result(vec![
        ("name", true, string()),
        ("path", true, string()),
        ("size", true, int()),
        ("type", true, string()),
        ("fstype", true, nullable(string())),
        ("label", true, nullable(string())),
        ("mountpoints", true, array(string())),
    ]);
    let disk = result(vec![
        ("name", true, string()),
        ("path", true, string()),
        ("size", true, int()),
        ("model", true, string()),
        ("serial", true, string()),
        ("transport", true, string()),
        ("removable", true, boolean()),
        ("fstype", true, nullable(string())),
        ("label", true, nullable(string())),
        ("mountpoints", true, array(string())),
        ("children", true, array(partition)),
        (
            "role",
            true,
            one_of(&["system", "snapshots", "in use", "free"]),
        ),
        ("reason", true, string()),
        ("offer", true, nullable_enum(&["folder", "whole disk"])),
        ("folders", true, array(string())),
    ]);
    let pool = result(vec![
        ("name", true, string()),
        ("driver", true, string()),
        ("source", true, string()),
        ("used", true, nullable(int())),
        ("total", true, nullable(int())),
        ("snapshots", true, boolean()),
    ]);
    result(vec![
        ("disks", true, array(disk)),
        ("pools", true, array(pool)),
    ])
}

fn snapshot_list_result() -> Value {
    let snapshot = result(vec![
        ("name", true, string()),
        (
            "kind",
            true,
            one_of(&["container", "virtual-machine", "volume"]),
        ),
        ("project", true, string()),
        ("sourceProject", true, string()),
        ("sourceName", true, string()),
        ("sourcePool", true, nullable(string())),
        ("pool", true, string()),
        ("size", true, nullable(int())),
        ("created", true, string()),
        ("state", true, one_of(&["ready", "copying", "incomplete"])),
        ("droppedDevices", true, array(string())),
    ]);
    result(vec![("snapshots", true, array(snapshot))])
}
