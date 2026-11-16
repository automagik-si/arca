//! Owner-approved starts retain the tenant boundary and the approval's one-run lifetime.
use super::*;

fn stopped_service(rig: &Rig) {
    rig.call(
        "service_create",
        json!({ "name": "web", "template": "app", "tenant": A, "start": false }),
    )
    .expect("a stopped service");
}

fn state(rig: &Rig) -> Value {
    rig.get(&format!("/1.0/instances/web-{A}?project={}", project(A)))
        .expect("the instance")["status"]
        .clone()
}

#[test]
fn an_owner_approved_tenant_start_runs_once_without_the_proposers_start_tool() {
    let rig = two_tenants(policy());
    stopped_service(&rig);
    rig.call(
        "service_create",
        json!({ "name": "web", "template": "app", "tenant": B, "start": false }),
    )
    .expect("another tenant's stopped service");
    assert_eq!(state(&rig), "Stopped");
    let propose = |tenant| {
        rig.call_token(
            "proposer",
            "action_propose",
            json!({ "action": "instance_start", "args": { "tenant": tenant, "service": "web" } }),
            &["action_propose"],
        )
        .expect("a token without instance_start may propose it")
    };
    let proposed = propose(A);
    let id = proposed["id"].as_str().unwrap();
    let statement = arca::handlers::action_statement(
        "node1",
        id,
        "instance_start",
        proposed["argsDigest"].as_str().unwrap(),
        rig.now(),
        &format!("{:064x}", 1),
    )
    .unwrap();
    let approval = common::approve_statement(&rig.key, &statement);
    let executor = ActionExecutor::new(
        rig.handlers
            .iter()
            .map(|(verb, handler)| (*verb, Arc::clone(handler))),
    );
    let run =
        |args: Value| arca::handlers::run_approved_action(&rig.kernel, &executor, &args, rig.now());
    let writes = rig.fake().writes().len();
    // The same signature cannot authorize a different card (and therefore a different tenant).
    let other = propose(B);
    assert!(run(json!({ "plan": other["id"], "approval": approval })).is_err());
    assert_eq!(rig.fake().writes().len(), writes);
    assert_eq!(state(&rig), "Stopped");

    let args = json!({ "plan": id, "approval": approval });
    let started = run(args.clone()).expect("the owner's approved tenant start runs");
    assert_eq!(started["result"]["action"], "start");
    assert_eq!(state(&rig), "Running");
    let journal = fs::read_to_string(rig.dir.join(arca::lifecycle::INSTANCE_ACTIONS_FILE)).unwrap();
    let begun: Vec<Value> = journal
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["kind"] == "begin")
        .collect();
    assert_eq!(begun.len(), 1);
    assert_eq!(begun[0]["token_id"], arca_mcp::auth::APPROVED_ACTION_ID);
    assert_eq!(begun[0]["tenant"], A);
    assert_eq!(begun[0]["instance"], format!("web-{A}"));
    let writes = rig.fake().writes().len();
    assert!(run(args).is_err(), "an approval is spent once");
    assert_eq!(rig.fake().writes().len(), writes);
}

#[test]
fn a_direct_tenant_start_still_requires_its_tool_and_no_card() {
    let rig = two_tenants(policy());
    stopped_service(&rig);
    let args = json!({ "tenant": A, "service": "web" });
    let writes = rig.fake().writes().len();
    for tools in [None, Some(&["action_propose"][..])] {
        assert_eq!(
            refusal(rig.call_as("instance_start", args.clone(), tools)),
            ("NotAllowed".into(), "instance_start inside a tenant is for a token whose tool allowlist names instance_start".into()),
        );
    }
    assert_eq!(rig.fake().writes().len(), writes);
    assert_eq!(state(&rig), "Stopped");
    rig.call_as("instance_start", args, Some(&["instance_start"]))
        .expect("the listed tool starts without a card");
    assert_eq!(state(&rig), "Running");
    assert!(rig.kernel.lock().unwrap().action_proposals().is_empty());
}

#[test]
fn a_limited_proposer_cannot_make_a_start_card_outside_its_named_services() {
    let rig = two_tenants(policy());
    stopped_service(&rig);
    let tokens = rig.dir.join("proposal-tokens");
    fs::create_dir(&tokens).unwrap();
    fs::set_permissions(&tokens, fs::Permissions::from_mode(0o700)).unwrap();
    let server = Arc::new(
        arca_mcp::Server::new(
            arca_api::verbs(),
            arca_mcp::TokenStore::load(&tokens).unwrap(),
        )
        .handle(
            "action_propose",
            Arc::clone(&rig.handlers["action_propose"]),
        ),
    );
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let writes = rig.fake().writes().len();
    for services in [format!("{A}/web"), "own".to_string()] {
        let caller = Caller {
            token_id: "limited-proposer".into(),
            scope: Scope::Doctor,
            tools: Some(vec!["action_propose".into()]),
            limits: arca_mcp::Limits {
                services: arca_api::limits::ServiceLimit::parse(&services),
                ..Default::default()
            },
        };
        for args in [
            json!({ "tenant": B, "service": "web" }),
            json!({ "tenant": A, "service": "other" }),
            json!({ "project": PROJECT, "instance": "other" }),
        ] {
            let request = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": "action_propose",
                    "arguments": { "action": "instance_start", "args": args } } });
            let reply = runtime
                .block_on(arca_mcp::jsonrpc::dispatch(
                    &server,
                    &caller,
                    &Default::default(),
                    &serde_json::to_vec(&request).unwrap(),
                ))
                .unwrap()
                .unwrap();
            assert_eq!(
                reply.body["error"]["data"]["refusal"], "OutsideTokenServices",
                "{reply:?}"
            );
        }
    }
    assert!(rig.kernel.lock().unwrap().action_proposals().is_empty());
    assert_eq!(rig.fake().writes().len(), writes);
    assert_eq!(state(&rig), "Stopped");
}
