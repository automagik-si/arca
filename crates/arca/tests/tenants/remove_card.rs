//! The owner's card for a tenant's removal says what goes, and not only the tenant's random id: its block, when it was
//! made, and each service in it, read from the node's records when the removal is proposed, and the same after a
//! restart. Every name and address here is synthetic.
use super::*;
use arca::service::DATABASE_TEMPLATE;

fn rows(details: &[arca::action_proposal::ActionDetail]) -> Vec<(&str, &str)> {
    details
        .iter()
        .map(|row| (row.label.as_str(), row.value.as_str()))
        .collect()
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
        format!(
            "Remove the tenant {B} ({block_b}, made 2026-10-30 12:00 UTC) and everything in it: no service"
        )
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
    let summary = format!(
        "Remove the tenant {A} ({block_a}, made 2026-10-30 12:00 UTC) and everything in it: 2 services, 1 of them a database"
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
        ("Services", "orders (database), web (container)"),
        (
            "Effect",
            "Deletes every service of this tenant, with its data, and then the tenant's project.",
        ),
    ];
    assert_eq!(card, expected);
    let empty_card: Vec<&str> = empty["details"]
        .as_array()
        .expect("details")
        .iter()
        .map(|row| row["value"].as_str().unwrap())
        .collect();
    assert_eq!(empty_card[3], "none");
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
