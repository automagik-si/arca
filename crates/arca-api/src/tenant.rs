//! The closed boundary of a tenant-bound token. Resolution precedes handlers, proposals and relays.

use serde_json::Value;

use crate::Verb;

/// A call would reach outside the token's tenant.
pub const OUTSIDE_TENANT: &str = "OutsideTokenTenant";

/// The reviewed verbs that accept a tenant. A new verb is refused until it is reviewed here too.
const TENANT_VERBS: [&str; 18] = [
    "service_create",
    "service_list",
    "service_status",
    "service_logs",
    "service_remove",
    "service_update",
    "service_reconfigure",
    "service_connect",
    "service_disconnect",
    "service_secret_set",
    "snapshot_create",
    "snapshot_list",
    "snapshot_delete",
    "instance_start",
    "instance_stop",
    "instance_restart",
    "deploy_confirm",
    "deploy_remove",
];

/// Whether this verb is offered to a tenant-bound token, before scope, tools and services further narrow it.
pub fn offered(verb: &Verb) -> bool {
    TENANT_VERBS.contains(&verb.name)
        || matches!(
            verb.name,
            "arca_status" | "tenant_list" | "tenant_status" | "tenant_usage" | "action_propose"
        )
}

/// Resolve the tenant without broadening a call. Returns a proposal's inner verb so the door can check its scope and
/// tools too. Arguments have already met the public schema. Errors contain no name supplied by a caller.
pub fn resolve(
    verb: &Verb,
    tenant: &str,
    args: &mut Value,
) -> Result<Option<&'static Verb>, &'static str> {
    if !crate::limits::tenant_id_ok(tenant) {
        return Err("the token names no valid tenant");
    }
    if args.get("node").is_some() {
        return Err("a tenant-bound token cannot call a linked node");
    }
    if !offered(verb) {
        return Err("this verb is not available to a tenant-bound token");
    }
    match verb.name {
        "action_propose" => {
            let action = args
                .get("action")
                .and_then(Value::as_str)
                .and_then(crate::verb)
                .ok_or("the proposal names no permitted action")?;
            if !action.writes || !TENANT_VERBS.contains(&action.name) {
                return Err("the proposed action is not available to a tenant-bound token");
            }
            let inner = args
                .get_mut("args")
                .ok_or("the proposal has no arguments")?;
            resolve(action, tenant, inner)?;
            Ok(Some(action))
        }
        "arca_status" | "tenant_list" => Ok(None),
        "tenant_status" | "tenant_usage" => {
            if args.get("id").and_then(Value::as_str) != Some(tenant) {
                return Err("the call names another tenant");
            }
            Ok(None)
        }
        _ => {
            match args.get("tenant") {
                Some(value) if value.as_str() != Some(tenant) => {
                    return Err("the call names another tenant");
                }
                Some(_) => {}
                None => {
                    args.as_object_mut()
                        .ok_or("the call has no arguments")?
                        .insert("tenant".into(), tenant.into());
                }
            }
            Ok(None)
        }
    }
}

/// Cut the tenant inventory before summaries, truncation or schema validation can expose a different tenant's row.
pub fn cut(verb: &Verb, tenant: &str, value: &mut Value) {
    if verb.name == "tenant_list"
        && let Some(rows) = value.get_mut("tenants").and_then(Value::as_array_mut)
    {
        rows.retain(|row| row.get("id").and_then(Value::as_str) == Some(tenant));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{conforms, verb, verbs};
    use serde_json::json;

    #[test]
    fn every_reviewed_tenant_verb_still_accepts_the_tenant_argument() {
        for name in TENANT_VERBS {
            let v = verb(name).unwrap();
            let schema = (v.params)();
            assert!(schema["properties"].get("tenant").is_some(), "{name}");
            assert!(conforms(&schema["properties"]["tenant"], &json!("abcdefgh")).is_ok());
        }
        assert_eq!(
            verbs().iter().filter(|v| offered(v)).count(),
            TENANT_VERBS.len() + 5
        );
    }
}
