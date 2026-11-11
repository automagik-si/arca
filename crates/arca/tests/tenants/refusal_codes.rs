//! The guard over the two refusal families that carry fields: out of room (`capacity`) and asked too often (`rate`,
//! and the proposal window's older `RateLimited`).
use super::*;
use std::collections::{BTreeMap, BTreeSet};

/// Walks the codes the OpenRPC document lists under `x-arca-refusals`. For each it makes real calls that a handler
/// refuses with that code, read through `common::listed_refusal`, which fails on the plain `refused`, on a code the
/// document does not list and on fields that do not fit the listed schema. At the end every listed code has been
/// answered by a call here, and no call here answered a code the document does not list.
///
/// How far it reaches. A site named below that goes back to `refused` fails here. So does a code that is added to the
/// document, or removed from it, without a call here that answers it. Every other tagged site is held by its own test
/// through the same helper (the node's virtual-machine caps, the address plans, the pending approval requests, the
/// Omni pairing hold-off). What no test holds is a refusal that is written new, for want of room or for asking too
/// often, with the plain `refused`: only its sentence says what it is, so that one is for the review to catch.
#[test]
fn every_listed_refusal_code_is_answered_by_a_call_and_none_of_them_comes_back_as_refused() {
    let document = arca_api::openrpc();
    let codes: BTreeSet<String> = document["x-arca-refusals"]
        .as_array()
        .expect("the document lists the refusals that carry fields")
        .iter()
        .map(|entry| entry["code"].as_str().expect("a code").to_string())
        .collect();
    let mut answered: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    let mut saw = |site: &'static str, result: Result<Value, CallError>| {
        let (code, message, _) = listed(result);
        assert_ne!(code, "refused", "{site}: {message}");
        answered.entry(code).or_default().push(site);
    };

    // The node is out of room for one more tenant: by count, and by the caps it has set aside.
    let mut one = policy();
    one["maxTenants"] = json!(1);
    let rig = Rig::new(Some(one));
    rig.create_tenant(json!({ "id": A })).expect("the first");
    saw(
        "tenant_create over kernel.tenants.maxTenants",
        rig.create_tenant(json!({ "id": B })),
    );
    let mut tight = policy();
    tight["capacity"]["instances"] = json!(7);
    let rig = Rig::new(Some(tight));
    rig.create_tenant(json!({ "id": A })).expect("the first");
    saw(
        "tenant_create over kernel.tenants.capacity",
        rig.create_tenant(json!({ "id": B })),
    );

    // A tenant is out of room: one instance is all its caps allow.
    let mut small = policy();
    small["defaultCaps"] = json!({ "instances": 1, "containers": 1, "virtualMachines": 0, "cpu": 6,
        "memory": "8GiB", "disk": "50GiB" });
    let rig = two_tenants(small);
    rig.call(
        "service_create",
        json!({ "name": "one", "template": "app", "tenant": A }),
    )
    .expect("the tenant's one instance");
    saw(
        "service_create over a tenant's caps",
        rig.call(
            "service_create",
            json!({ "name": "two", "template": "app", "tenant": A }),
        ),
    );

    // The node creates tenants no faster than its rate.
    let mut slow = policy();
    slow["createRate"] = json!({ "count": 1, "perSeconds": 3600 });
    let rig = Rig::new(Some(slow));
    rig.create_tenant(json!({ "id": A })).expect("the first");
    saw(
        "tenant_create over kernel.tenants.createRate",
        rig.create_tenant(json!({ "id": B })),
    );

    // One token records thirty new proposals in ten minutes, and the next one waits. Each is declined, so the eight
    // that may wait at once never fill.
    let rig = Rig::new(Some(policy()));
    let propose = |n: usize| {
        let id = format!(
            "aaaaaa{}{}",
            (b'a' + (n / 26) as u8) as char,
            (b'a' + (n % 26) as u8) as char
        );
        rig.call(
            "action_propose",
            json!({ "action": "tenant_create", "args": { "id": id } }),
        )
    };
    for n in 0..arca::action_proposal::PROPOSAL_RATE_MAX {
        let proposed = propose(n).expect("a proposal inside the window");
        rig.call("plan_decline", json!({ "plan": proposed["id"] }))
            .expect("the proposal is declined");
    }
    saw(
        "action_propose over the token's window",
        propose(arca::action_proposal::PROPOSAL_RATE_MAX),
    );

    for code in &codes {
        assert!(
            answered.contains_key(code),
            "the document lists {code}, and no call here is refused with it: {answered:?}"
        );
    }
    assert_eq!(
        answered.keys().cloned().collect::<BTreeSet<_>>(),
        codes,
        "{answered:?}"
    );
}
