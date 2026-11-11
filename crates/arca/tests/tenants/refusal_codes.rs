//! The guard over the two refusal families the OpenRPC document lists under `x-arca-refusals`: out of room (`capacity`)
//! and asked too often (`rate`, and the two proposal refusals that keep their older names).
use super::*;
use std::collections::{BTreeMap, BTreeSet};

/// Walks the codes the OpenRPC document lists under `x-arca-refusals`. For each it makes real calls that a handler
/// refuses with that code, read through `common::listed_refusal`, which fails on the plain `refused`, on a code the
/// document does not list and on fields that do not fit the listed schema. Each call names the family it expects, and
/// the document's `family` for the code it got must be that one. At the end every listed code has been answered by a
/// call here, and no call here answered a code the document does not list.
///
/// How far it reaches. A site named below that goes back to `refused`, or changes family, fails here. So does a code
/// that is added to the document, or removed from it, without a call here that answers it. Every other tagged site is
/// held by its own test through the same helper (the node's virtual-machine caps, the address plans, the pending
/// approval requests, the Omni pairing hold-off). A refusal that is written new with the plain `refused` is caught
/// only as far as the source scan below reads its words.
#[test]
fn every_listed_refusal_code_is_answered_by_a_call_in_its_family_and_none_comes_back_as_refused() {
    let document = arca_api::openrpc();
    let codes: BTreeSet<String> = document["x-arca-refusals"]
        .as_array()
        .expect("the document lists the refusals of the two families")
        .iter()
        .map(|entry| entry["code"].as_str().expect("a code").to_string())
        .collect();
    let mut answered: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    let mut saw = |family: &str, site: &'static str, result: Result<Value, CallError>| {
        let (code, message, _) = listed(result);
        assert_ne!(code, "refused", "{site}: {message}");
        assert_eq!(
            common::listed_refusal_entry(&code).expect("listed")["family"],
            family,
            "{site}: {code}"
        );
        answered.entry(code).or_default().push(site);
    };

    // The node is out of room for one more tenant: by count, and by the caps it has set aside.
    let mut one = policy();
    one["maxTenants"] = json!(1);
    let rig = Rig::new(Some(one));
    rig.create_tenant(json!({ "id": A })).expect("the first");
    saw(
        "capacity",
        "tenant_create over kernel.tenants.maxTenants",
        rig.create_tenant(json!({ "id": B })),
    );
    let mut tight = policy();
    tight["capacity"]["instances"] = json!(7);
    let rig = Rig::new(Some(tight));
    rig.create_tenant(json!({ "id": A })).expect("the first");
    saw(
        "capacity",
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
        "capacity",
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
        "rate",
        "tenant_create over kernel.tenants.createRate",
        rig.create_tenant(json!({ "id": B })),
    );

    // Eight proposals wait at once, and the ninth is refused under the code it has always had.
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
    let waiting: Vec<Value> = (0..arca::action_proposal::MAX_WAITING)
        .map(|n| propose(n).expect("a proposal that waits"))
        .collect();
    saw(
        "rate",
        "action_propose while eight proposals wait",
        propose(arca::action_proposal::MAX_WAITING),
    );
    // One token records thirty new proposals in ten minutes, and the next one waits. Each is declined, so the eight
    // that may wait at once never fill again.
    for proposed in &waiting {
        rig.call("plan_decline", json!({ "plan": proposed["id"] }))
            .expect("the proposal is declined");
    }
    for n in arca::action_proposal::MAX_WAITING..arca::action_proposal::PROPOSAL_RATE_MAX {
        let proposed = propose(n).expect("a proposal inside the window");
        rig.call("plan_decline", json!({ "plan": proposed["id"] }))
            .expect("the proposal is declined");
    }
    saw(
        "rate",
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

/// A node whose virtual-machine caps allow 16 GiB of memory, where the node's own machines use 12 GiB of it, and one
/// tenant whose own cap is 16 GiB and who uses nothing.
fn a_full_node_with_an_empty_tenant() -> Rig {
    let mut kernel = json!({
        "projects": [PROJECT],
        "droiddProjects": ["other"],
        "serviceCidr": CIDR,
        "postgresImage": PG_IMAGE,
        "vmLimits": { "virtualMachines": 4, "cpu": 16, "memory": "16GiB", "disk": "1TiB" }
    });
    kernel["tenants"] = policy();
    let rig = Rig::with_kernel(kernel, empty_state());
    rig.template("vm", vm_template());
    rig.template(
        "big",
        json!({
            "name": "big", "kind": "virtual-machine", "image": "images:debian/13",
            "limits": { "cpu": "2", "memory": "8GiB" }, "rootSize": "20GiB", "ports": [22]
        }),
    );
    rig.create_tenant(json!({ "id": A })).expect("the tenant");
    for name in ["own1", "own2", "own3"] {
        rig.call("service_create", json!({ "name": name, "template": "vm" }))
            .unwrap_or_else(|error| panic!("the node's own machine {name}: {error:?}"));
    }
    rig
}

/// The scenario a billing client must read right. The tenant is far under its own caps: it may use 16 GiB of memory
/// and uses none. Its 8 GiB machine is refused because the NODE is full: the node's own machines use 12 GiB of the 16
/// the node allows. The refusal says so with `scope: "node"`, and its amounts are the node's. Without `scope` the
/// same fields would read as the tenant's plan being full.
#[test]
fn a_tenant_under_its_own_caps_refused_by_the_nodes_cap_is_told_the_cap_is_the_nodes() {
    const GIB: u64 = 1 << 30;
    let rig = a_full_node_with_an_empty_tenant();
    let tenant = rig.call("tenant_status", json!({ "id": A })).unwrap();
    assert_eq!(tenant["caps"]["memory"], "16GiB");
    assert_eq!(tenant["use"]["memoryBytes"], 0, "the tenant uses nothing");
    assert_eq!(tenant["use"]["instances"], 0);
    let writes = rig.fake().writes().len();
    assert_eq!(
        listed(rig.call(
            "service_create",
            json!({ "name": "big", "template": "big", "tenant": A }),
        )),
        (
            "capacity".to_string(),
            "creating big would exceed this node's cap on memory (kernel.vmLimits.memory): 12.0 GiB in use, 8.0 GiB asked, 16.0 GiB allowed".to_string(),
            json!({ "scope": "node", "limit": "memory", "have": 12 * GIB, "want": 8 * GIB, "max": 16 * GIB })
        )
    );
    assert_eq!(rig.fake().writes().len(), writes, "nothing reached Incus");

    // The same create on a node with room is refused by nothing: the tenant's own caps were never the reason.
    let roomy = Rig::new(Some(policy()));
    roomy.template(
        "big",
        json!({
            "name": "big", "kind": "virtual-machine", "image": "images:debian/13",
            "limits": { "cpu": "2", "memory": "8GiB" }, "rootSize": "20GiB", "ports": [22]
        }),
    );
    roomy.create_tenant(json!({ "id": A })).expect("the tenant");
    roomy
        .call(
            "service_create",
            json!({ "name": "big", "template": "big", "tenant": A }),
        )
        .expect("the tenant's machine fits its own caps");
    // And a second one is refused by the tenant's own cap, which says `scope: "tenant"`.
    let (code, message, fields) = listed(roomy.call(
        "service_create",
        json!({ "name": "big2", "template": "big", "tenant": A }),
    ));
    assert_eq!(code, "capacity");
    assert_eq!(
        message,
        "creating big2 would exceed tenant aaaaaaaa's cap on virtual machines (limits.virtual-machines): 1 in use, 1 asked, 1 allowed"
    );
    assert_eq!(
        fields,
        json!({ "scope": "tenant", "limit": "virtualMachines", "have": 1, "want": 1, "max": 1 })
    );
}

/// A tenant's block with every address in use, while the tenant is under its instance cap. The tenant's caps allow
/// four instances and it holds none, so its plan is not what is full: something else holds the addresses. The refusal
/// names the tenant's block in its sentence and says `scope: "node"`, never `tenant`.
#[test]
fn a_tenants_block_filled_by_something_else_is_a_capacity_refusal_of_the_node() {
    let rig = Rig::new(Some(policy()));
    rig.template("app", app_template("app"));
    let made = rig.create_tenant(json!({ "id": A })).expect("the tenant");
    assert_eq!(made["block"], "192.0.2.248/29");
    let tenant = rig.call("tenant_status", json!({ "id": A })).unwrap();
    assert_eq!(tenant["caps"]["instances"], 4);
    assert_eq!(tenant["use"]["instances"], 0, "the tenant holds nothing");
    // The top /29 holds seven addresses an instance may take. Something else on the bridge holds a lease on each.
    for last in 248..=254u8 {
        rig.fake().set_lease(
            "arcak-svcbr",
            &format!("10:66:6a:00:00:{last:02x}"),
            &format!("192.0.2.{last}"),
        );
    }
    assert_eq!(
        listed(rig.call(
            "service_create",
            json!({ "name": "web", "template": "app", "tenant": A }),
        )),
        (
            "capacity".to_string(),
            "tenant aaaaaaaa's address block 192.0.2.248/29 has no free address".to_string(),
            json!({ "scope": "node", "limit": "addresses", "have": 7, "want": 1, "max": 7 })
        )
    );
    // Once the stray leases are gone the same create passes: the tenant's own caps were never the reason.
    for last in 248..=254u8 {
        rig.fake().set_lease(
            "arcak-svcbr",
            &format!("10:66:6a:00:00:{last:02x}"),
            "192.0.2.9",
        );
    }
    rig.call(
        "service_create",
        json!({ "name": "web", "template": "app", "tenant": A }),
    )
    .expect("the create with the block free again");
}

/// An approved action that is refused when it runs has spent its approval. The caller of `action_approve`, and the
/// approval page, are told the refusal with `proposeAgain: true`: waiting does not make the same call pass, the action
/// is proposed again. The direct call of the same verb carries no such field: after the wait it is simply made again.
#[test]
fn an_approved_create_past_the_rate_keeps_its_code_and_says_to_propose_again() {
    let mut slow = policy();
    slow["createRate"] = json!({ "count": 1, "perSeconds": 3600 });
    let rig = Rig::new(Some(slow));
    rig.create_tenant(json!({ "id": A })).expect("the first");
    let sentence = "this node creates at most 1 tenants in 3600 seconds (kernel.tenants.createRate), and 1 were created in that time; the next create is allowed at 2026-10-30T13:00:00.000Z";
    assert_eq!(
        listed(rig.create_tenant(json!({ "id": B }))),
        (
            "rate".to_string(),
            sentence.to_string(),
            json!({ "retryAfterSeconds": 3600 })
        )
    );
    assert_eq!(
        listed(rig.approved_as_told("tenant_create", json!({ "id": B }))),
        (
            "rate".to_string(),
            format!("The approval was accepted, but tenant_create did not complete: {sentence}"),
            json!({ "retryAfterSeconds": 3600, "proposeAgain": true })
        )
    );
}

/// Every plain refusal the source builds whose words read like out of room or asked too often, as `file:line`. It
/// reads the arguments of each call of a plain-refusal helper (`service_refused`, `refused`, `service_refused_code`)
/// up to the call's closing parenthesis.
fn plain_refusals_that_read_like_the_two_families(file: &str, source: &str) -> Vec<String> {
    let call = regex::Regex::new(r"\b(service_refused|service_refused_code|refused)\(").unwrap();
    let words = regex::Regex::new(
        r"would exceed|no free|too many|\b429\b|in use, \{[^}]*\} asked|more second|more minute|s more:",
    )
    .unwrap();
    let mut found = Vec::new();
    for at in call.find_iter(source) {
        let mut depth = 0usize;
        let mut end = source.len();
        for (offset, character) in source[at.end() - 1..].char_indices() {
            match character {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = at.end() - 1 + offset;
                        break;
                    }
                }
                _ => {}
            }
        }
        if words.is_match(&source[at.start()..end]) {
            let line = source[..at.start()].matches('\n').count() + 1;
            found.push(format!("{file}:{line}"));
        }
    }
    found
}

/// The cheap scan a new site meets. A refusal for want of room or for asking too often that is written with a plain
/// helper fails here when its sentence uses the words these refusals use today ("would exceed", "no free", "too many",
/// "429", "in use, .. asked", "more second", "more minute", "s more:"). It reads words, not meaning: a new sentence in
/// other words passes, and that one is still for the review.
#[test]
fn no_plain_refusal_in_the_source_reads_like_out_of_room_or_asked_too_often() {
    // The scan finds what it is for.
    let planted = r#"
        return Err(service_refused(format!(
            "creating {name} would exceed this node's cap on {what}: {} in use, {} asked",
            used, asked
        )));
        let fine = service_refused("template x: its data volume states no size");
        return Err(refused("refused", format!("too many pending requests (429): {} has 5", id)));
    "#;
    assert_eq!(
        plain_refusals_that_read_like_the_two_families("planted.rs", planted),
        ["planted.rs:2", "planted.rs:7"]
    );

    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut found = Vec::new();
    let mut read = 0;
    for entry in fs::read_dir(&src).expect("the source directory") {
        let path = entry.expect("an entry").path();
        if path.extension().is_none_or(|extension| extension != "rs") {
            continue;
        }
        let source = fs::read_to_string(&path).expect("a source file");
        let file = path.file_name().expect("a name").to_string_lossy();
        found.extend(plain_refusals_that_read_like_the_two_families(
            &file, &source,
        ));
        read += 1;
    }
    assert!(read > 20, "the scan read the kernel's source: {read} files");
    assert!(
        found.is_empty(),
        "a plain refusal reads like out of room or asked too often; use CallError::capacity or CallError::rate: {found:?}"
    );
}
