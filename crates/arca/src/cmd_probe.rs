//! Offline consumer proposals. The desired-state lock excludes a running server.
use crate::{
    Failure, config,
    desired_store::{DesiredStore, EntryChange},
    handlers::{ensure_netproject, now_ms, service_entry},
    parse_flags, usage_failure,
};
use arca_kernel::{desired::EntryId, identity::NodeKey};
use serde_json::{Value, json};
use std::path::Path;
pub fn probe(args: &[String]) -> Result<Option<Value>, Failure> {
    let [kind, action, rest @ ..] = args else {
        return Err(usage_failure());
    };
    if kind != "consumer" || !["add", "remove"].contains(&action.as_str()) {
        return Err(usage_failure());
    }
    let flags = parse_flags(rest, &["name", "image", "config"], &[])?;
    let name = flags.required("name")?;
    if !arca_netproject::name_ok(name, "consumer", 63) {
        return Err(Failure::Config("invalid consumer name".into()));
    }
    let cfg = config::load(Path::new(flags.required("config")?))?;
    let key = NodeKey::load_or_create(&cfg.state_dir.join("node.key"))
        .map_err(|_| Failure::Runtime("node key unavailable".into()))?;
    let mut store =
        DesiredStore::open(&cfg.state_dir, key).map_err(|e| Failure::Runtime(e.to_string()))?;
    let id = format!("consumer-{name}");
    let mut changes = vec![];
    if action == "add" {
        let image = flags.required("image")?;
        if !arca_postgres::hex64(image) {
            return Err(Failure::Config(
                "image must be a lowercase fingerprint".into(),
            ));
        }
        let entries = store
            .current()
            .map_or(&[][..], |s| s.document.entries.as_slice());
        changes = ensure_netproject(entries, &cfg).map_err(probe_failure)?;
        if entries.iter().any(|e| e.id.0 == id) {
            return Err(Failure::Config("consumer entry already exists".into()));
        }
        let pool = crate::handlers::service_pool(&cfg).map_err(probe_failure)?;
        changes.push(EntryChange::Put(service_entry(&id,"arca.consumer",json!({"name":name,"image":image,"pool":pool,"project":arca_netproject::PROJECT,"bridge":arca_netproject::BRIDGE,"links":[]}))));
    } else {
        changes.push(EntryChange::Remove(EntryId(id.clone())));
    }
    let signed = store
        .propose(changes, now_ms())
        .map_err(|e| Failure::Runtime(e.to_string()))?;
    Ok(Some(json!({"entry":id,"seq":signed.document.seq})))
}

fn probe_failure(error: arca_mcp::CallError) -> Failure {
    match error {
        arca_mcp::CallError::Invalid(text) => Failure::Config(text),
        other => crate::call_failure(other),
    }
}
