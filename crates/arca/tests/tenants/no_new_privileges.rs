//! A template that sets `noNewPrivileges` needs `raw.lxc`, which a tenant's project refuses: nothing in a tenant is
//! created from it, as a create or as a proposal, and nothing is written. A template without the key is still a
//! tenant's to use.
use super::*;

#[test]
fn nothing_in_a_tenant_is_created_from_a_template_that_sets_no_new_privileges() {
    let rig = Rig::new(Some(policy()));
    let mut strict = app_template("strict");
    strict["noNewPrivileges"] = json!(true);
    rig.template("strict", strict);
    rig.template("app", app_template("app"));
    rig.create_tenant(json!({ "id": A })).expect("tenant");
    let before = rig.fake().writes().len();
    let args = json!({ "name": "strict", "template": "strict", "tenant": A });
    let (code, message) = refusal(rig.call("service_create", args.clone()));
    assert_eq!(code, "refused");
    assert_eq!(
        message,
        "template strict: noNewPrivileges is for a service of the node's own project, never one in a tenant, whose project refuses the LXC config it needs"
    );
    assert_eq!(rig.fake().writes().len(), before, "nothing was written");
    assert!(
        rig.get(&format!("/1.0/instances/strict-{A}?project={}", project(A)))
            .is_none()
    );
    rig.call(
        "service_create",
        json!({ "name": "web", "template": "app", "tenant": A }),
    )
    .expect("a template without the key is still the tenant's");
}
