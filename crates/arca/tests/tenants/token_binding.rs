//! The MCP door resolves a token binding before real service and approval handlers run.
use super::*;
use arca_mcp::version::Mirrors;
use arca_mcp::{Server, TokenStore, mint};

#[test]
fn bound_tokens_use_only_their_tenants_services_and_run_approved_proposals_in_that_tenant() {
    let rig = two_tenants(policy());
    for tenant in [None, Some(A), Some(B)] {
        let mut args = json!({"name": "web", "template": "app"});
        if let Some(tenant) = tenant {
            args["tenant"] = json!(tenant);
        }
        rig.call("service_create", args).unwrap();
    }
    rig.call(
        "service_create",
        json!({"name": "only-b", "template": "app", "tenant": B}),
    )
    .unwrap();
    let own_ip = rig
        .call("service_status", json!({"name": "web", "tenant": A}))
        .unwrap()["ipv4"]
        .clone();
    let hashes = rig.dir.join("tokens");
    fs::create_dir(&hashes).unwrap();
    fs::set_permissions(&hashes, fs::Permissions::from_mode(0o700)).unwrap();
    let tools = "tools=service_list,service_status,service_remove,instance_start,instance_stop,action_propose,tenant_list,tenant_status,tenant_usage\n";
    let (bound, text) = mint(Scope::Doctor);
    fs::write(
        hashes.join("bound.hash"),
        format!("{text}{tools}tenant={A}\n"),
    )
    .unwrap();
    let (open, text) = mint(Scope::Doctor);
    fs::write(hashes.join("open.hash"), format!("{text}{tools}")).unwrap();
    for name in ["bound.hash", "open.hash"] {
        fs::set_permissions(hashes.join(name), fs::Permissions::from_mode(0o400)).unwrap();
    }
    let tokens = TokenStore::load(&hashes).unwrap();
    let bound = tokens.lookup(&bound).unwrap();
    let open = tokens.lookup(&open).unwrap();
    let mut server = Server::new(arca_api::verbs(), tokens);
    for (name, handler) in &rig.handlers {
        server = server.handle(name, handler.clone());
    }
    let server = Arc::new(server);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let call = |caller: &Caller, name: &str, args: Value| {
        let body = serde_json::to_vec(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": name, "arguments": args}}))
        .unwrap();
        runtime
            .block_on(arca_mcp::jsonrpc::dispatch(
                &server,
                caller,
                &Mirrors::default(),
                &body,
            ))
            .unwrap()
            .unwrap()
            .body
    };
    let answer = |reply: Value| {
        assert_eq!(reply["result"]["isError"], false, "{reply}");
        reply["result"]["structuredContent"].clone()
    };
    let rows = answer(call(&bound, "service_list", json!({})));
    assert_eq!(rows["services"].as_array().unwrap().len(), 1);
    assert_eq!(rows["services"][0]["ipv4"], own_ip);
    for args in [json!({"name": "web"}), json!({"name": "web", "tenant": A})] {
        assert_eq!(answer(call(&bound, "service_status", args))["ipv4"], own_ip);
    }
    assert_ne!(
        answer(call(&open, "service_status", json!({"name": "web"})))["ipv4"],
        own_ip
    );
    let before = rig.kernel.lock().unwrap().action_proposals().len();
    for args in [
        json!({"name": "web", "tenant": B}),
        json!({"name": "web", "tenant": "cccccccc"}),
    ] {
        let reply = call(&bound, "service_status", args);
        assert_eq!(reply["error"]["data"]["refusal"], "OutsideTokenTenant");
    }
    let other = call(&bound, "service_status", json!({"name": "only-b"}));
    assert_eq!(other["error"]["data"]["refusal"], "not_found");
    let refused = call(
        &bound,
        "action_propose",
        json!({"action": "service_remove", "args": {"name": "web", "tenant": B}}),
    );
    assert_eq!(refused["error"]["data"]["refusal"], "OutsideTokenTenant");
    assert_eq!(rig.kernel.lock().unwrap().action_proposals().len(), before);
    let proposed = answer(call(
        &bound,
        "action_propose",
        json!({"action": "service_remove", "args": {"name": "web"}}),
    ));
    assert_eq!(
        rig.kernel
            .lock()
            .unwrap()
            .action_proposals()
            .last()
            .unwrap()
            .args["tenant"],
        A
    );
    let stopped = answer(call(&bound, "instance_stop", json!({"service": "web"})));
    assert_eq!(stopped["project"], project(A));
    assert_eq!(
        rig.get(&format!("/1.0/instances/web-{B}?project={}", project(B)))
            .unwrap()["status"],
        "Running"
    );
    assert_eq!(
        rig.get(&format!("/1.0/instances/web?project={PROJECT}"))
            .unwrap()["status"],
        "Running"
    );
    let id = proposed["id"].as_str().unwrap();
    let digest = proposed["argsDigest"].as_str().unwrap();
    let nonce = format!("{:064x}", rig.nonce.fetch_add(1, Ordering::SeqCst) + 1);
    let statement =
        arca::handlers::action_statement("node1", id, "service_remove", digest, rig.now(), &nonce)
            .unwrap();
    let approval = common::approve_statement(&rig.key, &statement);
    let executor = ActionExecutor::new(
        rig.handlers
            .iter()
            .map(|(verb, handler)| (*verb, Arc::clone(handler))),
    );
    arca::handlers::run_approved_action(
        &rig.kernel,
        &executor,
        &json!({"plan": id, "approval": approval}),
        rig.now(),
    )
    .expect("the owner's approved bound proposal runs");
    assert!(
        !rig.entries()
            .iter()
            .any(|entry| entry.id.0 == format!("t-{A}-service-web"))
    );
    assert!(
        rig.get(&format!("/1.0/instances/web-{A}?project={}", project(A)))
            .is_none()
    );
    for (project, instance, entry) in [
        (project(B), format!("web-{B}"), format!("t-{B}-service-web")),
        (
            PROJECT.to_string(),
            "web".to_string(),
            "service-web".to_string(),
        ),
    ] {
        assert!(rig.entries().iter().any(|row| row.id.0 == entry));
        assert_eq!(
            rig.get(&format!("/1.0/instances/{instance}?project={project}"))
                .unwrap()["status"],
            "Running"
        );
    }
}
