//! Provisional creates: a deploy inside a tenant may be rolled back by the token that opened it, until it is confirmed or
//! 15 minutes old. And the front rule, which follows the node's front service and each deploy's state.
use super::*;

const WINDOW: u64 = 15 * 60 * 1000;
const TOOLS: [&str; 5] = [
    "service_create",
    "service_connect",
    "deploy_confirm",
    "deploy_rollback",
    "tenant_usage",
];

fn by(rig: &Rig, token: &str, verb: &str, args: Value) -> Result<Value, CallError> {
    rig.call_token(token, verb, args, &TOOLS)
}

/// The usual policy with room for six instances in a tenant.
fn roomy() -> Value {
    let mut policy = policy();
    policy["defaultCaps"]["instances"] = json!(6);
    policy["defaultCaps"]["containers"] = json!(6);
    policy["defaultCaps"]["cpu"] = json!(16);
    policy
}

fn ingress(rig: &Rig, tenant: &str, name: &str) -> Vec<Value> {
    rig.get(&format!(
        "/1.0/network-acls/arcak-t-{tenant}-svc-{name}?project=default"
    ))
    .unwrap_or_else(|| panic!("the ACL of {name}"))["ingress"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn instance(rig: &Rig, tenant: &str, name: &str) -> Option<Value> {
    rig.get(&format!(
        "/1.0/instances/{name}-{tenant}?project=arcak-t-{tenant}"
    ))
}

fn usage(rig: &Rig, tenant: &str) -> Vec<Value> {
    rig.call("tenant_usage", json!({ "id": tenant })).unwrap()["instances"]
        .as_array()
        .cloned()
        .unwrap()
}

/// Tenant A with an ordinary service `old`, a deploy `d1` of a database service, a service and a link (token `cloud`), and a
/// deploy `d2` of one service (token `other`).
fn deployed() -> Rig {
    let rig = two_tenants(roomy());
    rig.call(
        "service_create",
        json!({ "name": "old", "template": "app", "tenant": A }),
    )
    .expect("an ordinary service");
    by(
        &rig,
        "cloud",
        "service_create",
        json!({ "name": "db", "template": "app", "tenant": A, "deploy": "d1" }),
    )
    .expect("a database service in the deploy");
    by(
        &rig,
        "cloud",
        "service_create",
        json!({ "name": "web", "template": "app", "tenant": A, "deploy": "d1" }),
    )
    .expect("a service in the deploy");
    by(
        &rig,
        "cloud",
        "service_connect",
        json!({ "from": "old", "to": "web", "tenant": A }),
    )
    .expect("a link the deploy needs");
    by(
        &rig,
        "other",
        "service_create",
        json!({ "name": "side", "template": "app", "tenant": A, "deploy": "d2" }),
    )
    .expect("another deploy");
    rig
}

#[test]
fn a_rollback_inside_the_window_removes_exactly_the_deploys_own_things() {
    let rig = deployed();
    // The deploy is recorded on each entry, and journaled with the token that made it.
    assert_eq!(
        rig.entry("t-aaaaaaaa-service-web")["tenant"]["deploy"],
        "d1"
    );
    assert_eq!(rig.entry("t-aaaaaaaa-service-db")["tenant"]["deploy"], "d1");
    assert!(
        rig.entry("t-aaaaaaaa-service-old")["tenant"]
            .get("deploy")
            .is_none()
    );
    let journal = fs::read_to_string(rig.dir.join("tenant-deploys.jsonl")).unwrap();
    let rows: Vec<Value> = journal
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        rows.len(),
        3,
        "one line for each create that named a deploy"
    );
    assert_eq!(rows[0]["kind"], "create");
    assert_eq!(rows[0]["token_id"], "cloud");
    assert_eq!(rows[0]["tenant"], A);
    assert_eq!(rows[0]["deploy"], "d1");
    assert_eq!(rows[0]["entry"], "t-aaaaaaaa-service-db");
    // The database runs: a rollback takes a started instance too.
    assert_eq!(
        instance(&rig, A, "db").expect("the database")["status"],
        "Running"
    );
    // Usage flags what is provisional, so nothing is billed for a deploy that is rolled back.
    let flagged: Vec<(String, Value, Value)> = usage(&rig, A)
        .iter()
        .map(|row| {
            (
                row["service"].as_str().unwrap().to_string(),
                row["provisional"].clone(),
                row["deploy"].clone(),
            )
        })
        .collect();
    assert_eq!(
        flagged,
        [
            ("db".to_string(), json!(true), json!("d1")),
            ("old".to_string(), Value::Null, Value::Null),
            ("side".to_string(), json!(true), json!("d2")),
            ("web".to_string(), json!(true), json!("d1")),
        ]
    );
    // Another token may neither roll it back nor confirm it nor add to it.
    let before = rig.fake().writes().len();
    for (verb, args) in [
        ("deploy_rollback", json!({ "tenant": A, "deploy": "d1" })),
        ("deploy_confirm", json!({ "tenant": A, "deploy": "d1" })),
        (
            "service_create",
            json!({ "name": "more", "template": "app", "tenant": A, "deploy": "d1" }),
        ),
    ] {
        let (code, message) = refusal(by(&rig, "other", verb, args));
        assert_eq!(code, "refused", "{verb}");
        assert!(
            message.contains("only the token that opened"),
            "{verb}: {message}"
        );
    }
    // A deploy is one tenant's: the same id in B, or an id nobody opened, is not found.
    for args in [
        json!({ "tenant": B, "deploy": "d1" }),
        json!({ "tenant": A, "deploy": "nothing" }),
    ] {
        let (code, _) = refusal(by(&rig, "cloud", "deploy_rollback", args));
        assert_eq!(code, "not_found");
    }
    assert_eq!(
        rig.fake().writes().len(),
        before,
        "no refusal wrote anything"
    );
    // Its own token rolls it back, with no approval.
    let mark = rig.fake().requests().len();
    let rolled = by(
        &rig,
        "cloud",
        "deploy_rollback",
        json!({ "tenant": A, "deploy": "d1" }),
    )
    .expect("the rollback");
    assert_eq!(rolled["rolledBack"], true);
    let mut removed: Vec<&str> = rolled["removed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    removed.sort_unstable();
    assert_eq!(removed, ["db", "web"]);
    // Exactly the deploy's things are gone: instances, ACLs, records and secrets.
    assert!(instance(&rig, A, "db").is_none() && instance(&rig, A, "web").is_none());
    for name in ["arcak-t-aaaaaaaa-svc-db", "arcak-t-aaaaaaaa-svc-web"] {
        assert!(
            rig.get(&format!("/1.0/network-acls/{name}?project=default"))
                .is_none(),
            "{name}"
        );
    }
    let ids: Vec<String> = rig.entries().into_iter().map(|entry| entry.id.0).collect();
    assert!(
        !ids.iter()
            .any(|id| id.ends_with("-web") || id.ends_with("-db")),
        "{ids:?}"
    );
    // Everything else of the tenant stands: the ordinary service, the other deploy, the tenant.
    assert!(instance(&rig, A, "old").is_some() && instance(&rig, A, "side").is_some());
    assert!(ids.iter().any(|id| id == "t-aaaaaaaa-service-side"));
    // The link the deploy needed went with it: the ordinary service no longer names the removed one.
    assert_eq!(rig.entry("t-aaaaaaaa-service-old")["links"], json!([]));
    // The order: each instance is deleted and read back absent before its ACL, and nothing was refused.
    let requests = &rig.fake().requests()[mark..];
    for (name, acl) in [("web", "svc-web"), ("db", "svc-db")] {
        let path = format!("/1.0/instances/{name}-aaaaaaaa?project=arcak-t-aaaaaaaa");
        let acl = format!("/1.0/network-acls/arcak-t-aaaaaaaa-{acl}?project=default");
        let deleted = requests
            .iter()
            .position(|r| r.method == "DELETE" && r.path == path)
            .expect("instance delete");
        let rules = requests
            .iter()
            .position(|r| r.method == "DELETE" && r.path == acl)
            .expect("ACL delete");
        assert!(deleted < rules);
        assert!(
            requests[deleted..rules]
                .iter()
                .any(|r| r.method == "GET" && r.path == path && r.status == 404)
        );
    }
    assert!(
        requests
            .iter()
            .filter(|r| r.method == "DELETE")
            .all(|r| r.status < 400)
    );
    // It is journaled as a rollback of that deploy, by that token; and it is done once.
    let journal = fs::read_to_string(rig.dir.join("tenant-deploys.jsonl")).unwrap();
    let last: Value = serde_json::from_str(journal.lines().last().unwrap()).unwrap();
    assert_eq!(last["kind"], "rollback");
    assert_eq!(last["deploy"], "d1");
    assert_eq!(last["token_id"], "cloud");
    assert_eq!(last["ok"], true);
    let (_, message) = refusal(by(
        &rig,
        "cloud",
        "deploy_rollback",
        json!({ "tenant": A, "deploy": "d1" }),
    ));
    assert!(message.contains("it was rolled back"), "{message}");
    let (_, message) = refusal(by(
        &rig,
        "cloud",
        "service_create",
        json!({ "name": "late", "template": "app", "tenant": A, "deploy": "d1" }),
    ));
    assert!(message.contains("deploy d1 is closed"), "{message}");
}

#[test]
fn after_a_confirm_a_rollback_is_refused_and_the_services_are_ordinary() {
    let rig = deployed();
    assert_eq!(
        by(
            &rig,
            "cloud",
            "deploy_confirm",
            json!({ "tenant": A, "deploy": "d1" })
        )
        .expect("the confirm"),
        json!({ "confirmed": true, "deploy": "d1" })
    );
    let (code, message) = refusal(by(
        &rig,
        "cloud",
        "deploy_rollback",
        json!({ "tenant": A, "deploy": "d1" }),
    ));
    assert_eq!(code, "refused");
    assert_eq!(
        message,
        "roll back deploy d1: it was confirmed, so its services are ordinary now and a removal needs the owner's approval"
    );
    assert!(instance(&rig, A, "db").is_some() && instance(&rig, A, "web").is_some());
    // Usage no longer flags them; the other deploy is still provisional.
    let flagged: Vec<String> = usage(&rig, A)
        .iter()
        .filter(|row| row["provisional"] == json!(true))
        .map(|row| row["service"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(flagged, ["side"]);
    // A removal needs the owner now, as for every service: a direct call is refused, an approved one runs.
    let (code, _) = refusal(rig.call(
        "service_remove",
        json!({ "name": "web", "tenant": A, "approval": "x" }),
    ));
    assert_eq!(code, "owner_approval");
    rig.approved("service_remove", json!({ "name": "web", "tenant": A }))
        .expect("the approved removal");
    // A confirm is said again without harm, and nothing is added to a confirmed deploy.
    by(
        &rig,
        "cloud",
        "deploy_confirm",
        json!({ "tenant": A, "deploy": "d1" }),
    )
    .expect("again");
    let (_, message) = refusal(by(
        &rig,
        "cloud",
        "service_create",
        json!({ "name": "late", "template": "app", "tenant": A, "deploy": "d1" }),
    ));
    assert!(message.contains("deploy d1 is closed"), "{message}");
}

#[test]
fn the_fifteen_minutes_run_from_the_journal_and_a_restart_does_not_reset_them() {
    let rig = deployed();
    // One second before the end, after a restart, the other deploy can still be rolled back.
    rig.clock.store(NOW + WINDOW - 1_000, Ordering::SeqCst);
    let rig = rig.restart();
    by(
        &rig,
        "other",
        "deploy_rollback",
        json!({ "tenant": A, "deploy": "d2" }),
    )
    .expect("a rollback inside the window, after a restart");
    assert!(instance(&rig, A, "side").is_none());
    // At fifteen minutes the first deploy is ordinary, restart or not.
    rig.clock.store(NOW + WINDOW, Ordering::SeqCst);
    let rig = rig.restart();
    let refused = "roll back deploy d1: its 15 minutes passed at 2026-10-30T12:15:00.000Z, so its services are ordinary now and a removal needs the owner's approval";
    let (code, message) = refusal(by(
        &rig,
        "cloud",
        "deploy_rollback",
        json!({ "tenant": A, "deploy": "d1" }),
    ));
    assert_eq!((code.as_str(), message.as_str()), ("refused", refused));
    assert!(instance(&rig, A, "db").is_some() && instance(&rig, A, "web").is_some());
    assert!(
        usage(&rig, A)
            .iter()
            .all(|row| row.get("provisional").is_none())
    );
    let (_, message) = refusal(by(
        &rig,
        "cloud",
        "deploy_confirm",
        json!({ "tenant": A, "deploy": "d1" }),
    ));
    assert!(
        message.starts_with("confirm deploy d1: its 15 minutes passed"),
        "{message}"
    );
}

#[test]
fn a_deploy_is_for_a_create_inside_a_tenant_only() {
    let rig = two_tenants(policy());
    for (verb, args) in [
        (
            "service_create",
            json!({ "name": "web", "template": "app", "deploy": "d1" }),
        ),
        (
            "service_connect",
            json!({ "from": "a", "to": "b", "deploy": "d1" }),
        ),
        // A link is not part of a deploy by itself: the field is refused inside a tenant too.
        (
            "service_connect",
            json!({ "from": "a", "to": "b", "tenant": A, "deploy": "d1" }),
        ),
    ] {
        assert!(
            matches!(by(&rig, "cloud", verb, args), Err(CallError::Invalid(_))),
            "{verb}"
        );
    }
    assert!(matches!(
        by(
            &rig,
            "cloud",
            "service_create",
            json!({ "name": "web", "template": "app", "tenant": A, "deploy": "Not An Id" })
        ),
        Err(CallError::Invalid(_))
    ));
    assert!(
        !rig.dir.join("tenant-deploys.jsonl").exists(),
        "nothing was journaled"
    );
    assert!(
        rig.entries()
            .iter()
            .all(|entry| entry.plugin.0 != "arca.service")
    );
}

#[test]
fn the_front_reaches_only_ordinary_services_and_follows_a_front_that_is_made_again() {
    let mut fronted = policy();
    fronted["front"] = json!("edge");
    let rig = two_tenants(fronted);
    let create = |name: &str| {
        rig.call("service_create", json!({ "name": name, "template": "app" }))
            .unwrap_or_else(|error| panic!("{name}: {error:?}"))["ipv4"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let front_rule = |name: &str| -> Option<String> {
        ingress(&rig, A, name)
            .iter()
            .find(|rule| rule["description"] == "the node's front service to the declared ports")
            .map(|rule| rule["source"].as_str().unwrap().to_string())
    };
    let first = create("edge");
    rig.call(
        "service_create",
        json!({ "name": "old", "template": "app", "tenant": A }),
    )
    .unwrap();
    by(
        &rig,
        "cloud",
        "service_create",
        json!({ "name": "web", "template": "app", "tenant": A, "deploy": "d1" }),
    )
    .unwrap();
    // The front reaches the ordinary service, and not the provisional one: a pass changes nothing while it is provisional.
    assert_eq!(front_rule("old"), Some(format!("{first}/32")));
    assert_eq!(front_rule("web"), None);
    assert_eq!(
        arca::handlers::sync_front(&rig.kernel, &rig_config(&rig), rig.now()).unwrap(),
        0
    );
    assert_eq!(front_rule("web"), None);
    // Confirmed, it is ordinary, and the front reaches its declared ports.
    by(
        &rig,
        "cloud",
        "deploy_confirm",
        json!({ "tenant": A, "deploy": "d1" }),
    )
    .unwrap();
    assert_eq!(front_rule("web"), Some(format!("{first}/32")));
    assert_eq!(ingress(&rig, A, "web")[0]["destination_port"], "8080,9090");
    // A deploy nobody confirms becomes ordinary after 15 minutes, and the next pass opens it to the front.
    by(
        &rig,
        "cloud",
        "service_create",
        json!({ "name": "slow", "template": "app", "tenant": A, "deploy": "d2" }),
    )
    .unwrap();
    assert_eq!(front_rule("slow"), None);
    rig.clock.store(NOW + WINDOW, Ordering::SeqCst);
    assert_eq!(
        arca::handlers::sync_front(&rig.kernel, &rig_config(&rig), rig.now()).unwrap(),
        1
    );
    assert_eq!(front_rule("slow"), Some(format!("{first}/32")));
    // The front is removed and made again at another address: every tenant service's ACL follows.
    rig.approved("service_remove", json!({ "name": "edge" }))
        .expect("the front is removed");
    assert_eq!(
        arca::handlers::sync_front(&rig.kernel, &rig_config(&rig), rig.now()).unwrap(),
        3
    );
    for name in ["old", "web", "slow"] {
        assert_eq!(front_rule(name), None, "no front, no rule: {name}");
    }
    let second = create("edge");
    assert_ne!(second, first);
    for name in ["old", "web", "slow"] {
        assert_eq!(
            front_rule(name),
            Some(format!("{second}/32")),
            "{name} follows the new front"
        );
        assert!(
            !serde_json::to_string(&ingress(&rig, A, name))
                .unwrap()
                .contains(&format!("{first}/32"))
        );
    }
    // A pass over a node where nothing changed writes nothing.
    let writes = rig.fake().writes().len();
    assert_eq!(
        arca::handlers::sync_front(&rig.kernel, &rig_config(&rig), rig.now()).unwrap(),
        0
    );
    assert_eq!(rig.fake().writes().len(), writes);
}

/// The node config the rig serves with, for a call that takes it.
fn rig_config(rig: &Rig) -> NodeConfig {
    serde_json::from_value(json!({
        "stateDir": rig.dir,
        "incus": {"socket": rig.socket},
        "listen": {"unix": rig.dir.join("arca.sock")},
        "tokens": {"hashDir": rig.dir.join("tokens")},
        "kernel": rig.kernel_config
    }))
    .expect("config")
}

#[test]
fn one_owner_approval_removes_everything_a_confirmed_deploy_created_and_nothing_else() {
    let rig = deployed();
    let propose = |args: Value| {
        rig.call(
            "action_propose",
            json!({ "action": "deploy_remove", "args": args }),
        )
    };
    // A deploy that is still provisional is its own token's to roll back: the proposal is refused, nothing is recorded.
    let (code, message) = refusal(propose(json!({ "tenant": A, "deploy": "d1" })));
    assert_eq!(code, "refused");
    assert_eq!(
        message,
        "remove deploy d1: it is still provisional; the token that opened it rolls it back with deploy_rollback, or confirms it first"
    );
    // An id nobody opened, and the same id in another tenant, are not found.
    for args in [
        json!({ "tenant": A, "deploy": "nothing" }),
        json!({ "tenant": B, "deploy": "d1" }),
    ] {
        let (code, _) = refusal(propose(args));
        assert_eq!(code, "not_found");
    }
    assert!(rig.kernel.lock().unwrap().action_proposals().is_empty());
    by(
        &rig,
        "cloud",
        "deploy_confirm",
        json!({ "tenant": A, "deploy": "d1" }),
    )
    .unwrap();
    // A direct call is not an approval.
    let (code, _) = refusal(rig.call(
        "deploy_remove",
        json!({ "tenant": A, "deploy": "d1", "approval": "0123456789abcdef0123456789abcdef" }),
    ));
    assert_eq!(code, "NotApproved");
    // The proposal's digest covers the tenant and the deploy: another deploy, or another tenant, is another digest.
    let digest = |args: Value| {
        arca::action_proposal::action_args_digest("deploy_remove", &args).expect("a digest")
    };
    let proposed = propose(json!({ "tenant": A, "deploy": "d1" })).expect("the proposal");
    assert_eq!(proposed["args"], json!({ "tenant": A, "deploy": "d1" }));
    // The owner's card names every part of the deploy, with its kind, and the tenant.
    let card: Vec<(&str, &str)> = proposed["details"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["label"].as_str().unwrap(),
                row["value"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(card[0], ("Deploy", "d1"));
    assert_eq!(card[1], ("Parts", "db (container), web (container)"));
    assert_eq!(card[3], ("Tenant", A));
    // The card is the same after a restart: the parts were read when the removal was proposed, and are kept.
    let rig = rig.restart();
    let waiting = rig.kernel.lock().unwrap().action_proposals().to_vec();
    assert_eq!(waiting.len(), 1);
    let kept: Vec<(&str, &str)> = waiting[0]
        .details
        .iter()
        .map(|row| (row.label.as_str(), row.value.as_str()))
        .collect();
    assert_eq!(kept[1], ("Parts", "db (container), web (container)"));
    assert_eq!(kept.len(), 4);
    let propose = |args: Value| {
        rig.call(
            "action_propose",
            json!({ "action": "deploy_remove", "args": args }),
        )
    };
    assert_eq!(
        proposed["argsDigest"],
        json!(digest(json!({ "tenant": A, "deploy": "d1" })))
    );
    assert_ne!(
        digest(json!({ "tenant": A, "deploy": "d1" })),
        digest(json!({ "tenant": A, "deploy": "d2" }))
    );
    assert_ne!(
        digest(json!({ "tenant": A, "deploy": "d1" })),
        digest(json!({ "tenant": B, "deploy": "d1" }))
    );
    // One approval removes every part of the deploy: the database, the service, their ACLs, records and secrets.
    let mark = rig.fake().requests().len();
    let removed = rig
        .approved("deploy_remove", json!({ "tenant": A, "deploy": "d1" }))
        .expect("the approved removal");
    assert_eq!(removed["removed"], true);
    let mut parts: Vec<&str> = removed["parts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    parts.sort_unstable();
    assert_eq!(parts, ["db", "web"]);
    assert!(instance(&rig, A, "db").is_none() && instance(&rig, A, "web").is_none());
    for name in ["arcak-t-aaaaaaaa-svc-db", "arcak-t-aaaaaaaa-svc-web"] {
        assert!(
            rig.get(&format!("/1.0/network-acls/{name}?project=default"))
                .is_none(),
            "{name}"
        );
    }
    let ids: Vec<String> = rig.entries().into_iter().map(|entry| entry.id.0).collect();
    assert!(
        !ids.iter()
            .any(|id| id.ends_with("-web") || id.ends_with("-db")),
        "{ids:?}"
    );
    // Nothing else: the ordinary service, the other deploy and the tenant stand, and every delete named a part.
    assert!(instance(&rig, A, "old").is_some() && instance(&rig, A, "side").is_some());
    assert_eq!(rig.entry("t-aaaaaaaa-service-old")["links"], json!([]));
    let requests = &rig.fake().requests()[mark..];
    for request in requests.iter().filter(|request| request.method == "DELETE") {
        assert!(request.status < 400, "{}", request.path);
        assert!(
            request.path.contains("web") || request.path.contains("db"),
            "{}",
            request.path
        );
    }
    for (name, acl) in [("web", "svc-web"), ("db", "svc-db")] {
        let path = format!("/1.0/instances/{name}-aaaaaaaa?project=arcak-t-aaaaaaaa");
        let acl = format!("/1.0/network-acls/arcak-t-aaaaaaaa-{acl}?project=default");
        let deleted = requests
            .iter()
            .position(|r| r.method == "DELETE" && r.path == path)
            .expect("instance delete");
        let rules = requests
            .iter()
            .position(|r| r.method == "DELETE" && r.path == acl)
            .expect("ACL delete");
        assert!(deleted < rules, "the instance goes before its ACL");
    }
    // It is journaled, and there is nothing left of the deploy to remove again.
    let journal = fs::read_to_string(rig.dir.join("tenant-deploys.jsonl")).unwrap();
    let last: Value = serde_json::from_str(journal.lines().last().unwrap()).unwrap();
    assert_eq!(last["kind"], "remove");
    assert_eq!(last["deploy"], "d1");
    assert_eq!(last["ok"], true);
    let (_, message) = refusal(propose(json!({ "tenant": A, "deploy": "d1" })));
    assert_eq!(
        message,
        "remove deploy d1: nothing of it is left in tenant aaaaaaaa"
    );
}
