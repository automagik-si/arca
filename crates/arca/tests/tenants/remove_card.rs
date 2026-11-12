//! The owner's card for a tenant's removal says what goes, and not only the tenant's random id: its block, when it was
//! made, and each service in it, read from the node's records when the removal is proposed, and the same after a
//! restart. The removal deletes what the tenant holds when it runs, so the card says its list is as of the proposal and
//! that a service made after it goes too. Every name and address here is synthetic.
use super::*;
use arca::service::DATABASE_TEMPLATE;

fn rows(details: &[arca::action_proposal::ActionDetail]) -> Vec<(&str, &str)> {
    details
        .iter()
        .map(|row| (row.label.as_str(), row.value.as_str()))
        .collect()
}

/// The words every card of a tenant's removal ends with.
const EFFECT: &str = "Deletes every service of this tenant as it stands when the removal runs, with its data, services made after this proposal included, and then the tenant's project.";

fn summary(id: &str, block: &str, at: &str, held: &str) -> String {
    format!(
        "Remove the tenant {id} ({block}, made 2026-10-30 12:00 UTC) and everything in it when the removal runs, services made after this proposal included; at {at} it held {held}"
    )
}

fn card(proposed: &Value) -> Vec<(String, String)> {
    proposed["details"]
        .as_array()
        .expect("details")
        .iter()
        .map(|row| {
            (
                row["label"].as_str().unwrap().to_string(),
                row["value"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn propose_removal(rig: &Rig, id: &str) -> Value {
    rig.call(
        "action_propose",
        json!({ "action": "tenant_remove", "args": { "id": id } }),
    )
    .expect("the proposal")
}

/// Whether the node records a service `orders` in tenant B.
fn has_orders(rig: &Rig) -> bool {
    rig.kernel
        .lock()
        .expect("kernel")
        .entries()
        .iter()
        .any(|entry| entry.config["name"] == "orders" && entry.config["tenant"]["id"] == B)
}

fn create(rig: &Rig, name: &str, template: &str, tenant: &str) {
    rig.call(
        "service_create",
        json!({ "name": name, "template": template, "tenant": tenant }),
    )
    .unwrap_or_else(|error| panic!("create {name} in {tenant}: {error:?}"));
}

#[test]
fn a_tenants_removal_card_names_its_block_when_it_was_made_and_each_service_in_it_and_keeps_them_across_a_restart()
 {
    let rig = two_tenants(policy());
    rig.template(DATABASE_TEMPLATE, app_template(DATABASE_TEMPLATE));
    let block = |id: &str| {
        rig.call("tenant_status", json!({ "id": id }))
            .expect("the tenant")["block"]
            .as_str()
            .expect("its block")
            .to_string()
    };
    let (block_a, block_b) = (block(A), block(B));
    // B holds nothing yet: its card says so.
    let empty = rig
        .call(
            "action_propose",
            json!({ "action": "tenant_remove", "args": { "id": B } }),
        )
        .expect("the proposal");
    assert_eq!(
        empty["summary"],
        summary(B, &block_b, "2026-10-30 12:00 UTC", "no service")
    );
    create(&rig, "web", "app", A);
    create(&rig, "orders", DATABASE_TEMPLATE, A);
    // A service of the other tenant is never named on A's card.
    create(&rig, "other", "app", B);
    let proposed = rig
        .call(
            "action_propose",
            json!({ "action": "tenant_remove", "args": { "id": A } }),
        )
        .expect("the proposal");
    // The arguments are what was proposed: what the node read is on the card only.
    assert_eq!(proposed["args"], json!({ "id": A }));
    let summary = summary(
        A,
        &block_a,
        "2026-10-30 12:00 UTC",
        "2 services, 1 of them a database",
    );
    assert_eq!(proposed["summary"], summary);
    let card: Vec<(&str, &str)> = proposed["details"]
        .as_array()
        .expect("details")
        .iter()
        .map(|row| {
            (
                row["label"].as_str().unwrap(),
                row["value"].as_str().unwrap(),
            )
        })
        .collect();
    let expected = [
        ("Tenant", A),
        ("Address block", block_a.as_str()),
        ("Made", "2026-10-30 12:00 UTC"),
        (
            "Services",
            "As of 2026-10-30 12:00 UTC, when this was proposed: orders (database), web (container)",
        ),
        ("Effect", EFFECT),
    ];
    assert_eq!(card, expected);
    let empty_card: Vec<&str> = empty["details"]
        .as_array()
        .expect("details")
        .iter()
        .map(|row| row["value"].as_str().unwrap())
        .collect();
    assert_eq!(
        empty_card[3],
        "As of 2026-10-30 12:00 UTC, when this was proposed: none"
    );
    // After a restart the card is the one the owner was shown: what the tenant held was read when it was proposed.
    let rig = rig.restart();
    let waiting = rig.kernel.lock().unwrap().action_proposals().to_vec();
    let kept = waiting
        .iter()
        .find(|proposal| proposal.args == json!({ "id": A }))
        .expect("A's removal still waits");
    assert_eq!(kept.summary, summary);
    assert_eq!(rows(&kept.details), expected);
}

/// The card is read when the removal is proposed, and the removal deletes what the tenant holds when it runs. A
/// database made in an empty tenant after its removal was proposed is deleted with it; the card the owner approves,
/// the first one, says its list is as of its own time and that a service made after it goes too.
#[test]
fn a_service_made_after_a_tenants_removal_was_proposed_is_removed_with_it_and_the_card_said_so() {
    let rig = two_tenants(policy());
    rig.template(DATABASE_TEMPLATE, app_template(DATABASE_TEMPLATE));
    let block = rig
        .call("tenant_status", json!({ "id": B }))
        .expect("the tenant")["block"]
        .as_str()
        .expect("its block")
        .to_string();
    // 1. B is empty when its removal is proposed, at 12:00.
    let first = propose_removal(&rig, B);
    let first_summary = summary(B, &block, "2026-10-30 12:00 UTC", "no service");
    assert_eq!(first["summary"], first_summary);
    let first_card = card(&first);
    assert_eq!(
        first_card[3].1,
        "As of 2026-10-30 12:00 UTC, when this was proposed: none"
    );
    assert_eq!(first_card[4].1, EFFECT);
    // 2. Five minutes later a database is made in B.
    rig.clock.fetch_add(5 * 60_000, Ordering::SeqCst);
    create(&rig, "orders", DATABASE_TEMPLATE, B);
    assert!(has_orders(&rig), "the database is recorded in B");
    // 3. The same removal proposed again is the one that waits, with its card as it was, time included.
    let again = propose_removal(&rig, B);
    assert_eq!(again["id"], first["id"]);
    assert_eq!(again["summary"], first_summary);
    assert_eq!(card(&again), first_card);
    // 4. The owner approves it: the database goes with the tenant.
    let removed = rig
        .approved("tenant_remove", json!({ "id": B }))
        .expect("the removal");
    assert_eq!(removed["removed"], json!(true));
    let (code, _) = refusal(rig.call("tenant_status", json!({ "id": B })));
    assert_eq!(code, "not_found");
    assert!(!has_orders(&rig), "the database is gone with the tenant");
}
