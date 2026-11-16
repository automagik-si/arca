//! Tenant-bound tokens resolve their tenant before a handler, approval or relay can run.
mod common;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use arca_api::{Scope, verbs};
use arca_mcp::{
    CallError, Caller, GetHandler, GetReply, Server, TokenStore, TokenStoreError, ToolHandler,
    mint, serve_unix,
};
use common::{
    Scratch, bearer, connect_unix, exchange, post, rpc, wait_for_socket, write_hash_file,
};
use serde_json::{Value, json};
use tokio::runtime::Runtime;

const TENANT: &str = "abcdefgh";
type Calls = Arc<Mutex<Vec<(String, Value)>>>;

fn token(scratch: &Scratch, id: &str, scope: Scope, lines: &str) -> String {
    let (secret, text) = mint(scope);
    write_hash_file(scratch.path(), id, &format!("{text}{lines}"), 0o400);
    secret
}

struct Recording(&'static str, Calls);

struct Inventory;

struct PostRecording(Calls);

impl arca_mcp::PostHandler for PostRecording {
    fn post(&self, path: &str, caller: &Caller) -> Result<GetReply, CallError> {
        self.0
            .lock()
            .unwrap()
            .push((caller.token_id.clone(), json!({"path": path})));
        Ok(GetReply {
            content_type: "application/json",
            body: b"{}".to_vec(),
        })
    }
}

impl GetHandler for Inventory {
    fn get(&self, _: Option<&str>, _: &Caller) -> Result<GetReply, CallError> {
        Ok(GetReply {
            content_type: "text/plain",
            body: b"inventory".to_vec(),
        })
    }
}

impl ToolHandler for Recording {
    fn call(&self, args: Value, _caller: &Caller) -> Result<Value, CallError> {
        self.1.lock().unwrap().push((self.0.to_string(), args));
        if self.0 == "tenant_list" {
            let row = |id| {
                json!({"id": id, "block": "192.0.2.0/28", "caps": {
                "instances": 1, "containers": 1, "virtualMachines": 0, "cpu": 1,
                "memory": "1GiB", "disk": "1GiB"}, "use": {"instances": 0,
                "containers": 0, "virtualMachines": 0, "cpu": 0, "memoryBytes": 0, "diskBytes": 0}})
            };
            return Ok(json!({"tenants": [row(TENANT), row("ijklmnop")]}));
        }
        Err(CallError::Refused {
            code: "Reached".into(),
            message: "handler reached".into(),
        })
    }
}

struct Door {
    _runtime: Runtime,
    _scratch: Scratch,
    socket: PathBuf,
    bound: String,
    limited: String,
    open: String,
    viewer: String,
    proposer_only: String,
    ack_viewer: String,
    calls: Calls,
}

impl Door {
    fn start() -> Self {
        let scratch = Scratch::new("tenant-token");
        let bound = token(&scratch, "bound", Scope::Operator, "tenant=abcdefgh\n");
        let limited = token(
            &scratch,
            "limited",
            Scope::Doctor,
            "tenant=abcdefgh\nservices=web\n",
        );
        let open = token(&scratch, "open", Scope::Operator, "");
        let viewer = token(&scratch, "viewer", Scope::Viewer, "tenant=abcdefgh\n");
        let proposer_only = token(
            &scratch,
            "proposer",
            Scope::Doctor,
            "tenant=abcdefgh\ntools=action_propose\n",
        );
        let ack_viewer = token(
            &scratch,
            "ack-viewer",
            Scope::Viewer,
            "tenant=abcdefgh\ntools=support_report_ack,arca_status\n",
        );
        let calls: Calls = Arc::default();
        let mut server = Server::new(verbs(), TokenStore::load(scratch.path()).unwrap())
            .log_to(Arc::new(|_| {}));
        for v in verbs() {
            server = server.handle(v.name, Arc::new(Recording(v.name, calls.clone())));
        }
        server = server.get_route("/inventory", "arca_status", Arc::new(Inventory));
        for (path, verb) in [
            ("/reports/{id}/ack", "support_report_ack"),
            ("/future", "arca_status"),
        ] {
            server = server.post_route(path, verb, Arc::new(PostRecording(calls.clone())));
        }
        let runtime = Runtime::new().unwrap();
        let socket = scratch.at("mcp.sock");
        let (router, served) = (server.router(), socket.clone());
        runtime.spawn(async move { serve_unix(router, &served).await });
        wait_for_socket(&socket);
        Self {
            _runtime: runtime,
            _scratch: scratch,
            socket,
            bound,
            limited,
            open,
            viewer,
            proposer_only,
            ack_viewer,
            calls,
        }
    }

    fn rpc(&self, token: &str, method: &str, args: Value) -> Value {
        let reply = post(
            &mut connect_unix(&self.socket),
            &[bearer(token)],
            &rpc(json!(1), method, args),
        );
        assert_eq!(reply.status, 200, "{}", reply.body);
        reply.json()
    }

    fn call(&self, token: &str, name: &str, args: Value) -> Value {
        self.rpc(
            token,
            "tools/call",
            json!({"name": name, "arguments": args}),
        )
    }
}

#[test]
fn a_proposals_inner_scope_and_tool_list_still_apply() {
    let door = Door::start();
    for token in [&door.viewer, &door.proposer_only] {
        let reply = door.call(
            token,
            "action_propose",
            json!({"action": "service_remove", "args": {"name": "web"}}),
        );
        assert_eq!(refusal(&reply), "OutsideTokenTenant", "{reply}");
    }
    assert!(door.calls.lock().unwrap().is_empty());
}

#[test]
fn a_bound_token_never_reads_a_get_route_even_when_its_verb_is_allowed() {
    let door = Door::start();
    for (token, expected) in [(&door.bound, 403), (&door.open, 200)] {
        let request = format!(
            "GET /inventory HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
        );
        let reply = exchange(&mut connect_unix(&door.socket), request.as_bytes());
        assert_eq!(reply.status, expected);
    }
}

#[test]
fn bound_operators_and_named_viewers_never_reach_post_routes() {
    let door = Door::start();
    for path in ["/reports/1/ack", "/future"] {
        door.calls.lock().unwrap().clear();
        let statuses: Vec<_> = [&door.bound, &door.ack_viewer, &door.open]
            .into_iter()
            .map(|token| {
                let request = format!(
                    "POST {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                exchange(&mut connect_unix(&door.socket), request.as_bytes()).status
            })
            .collect();
        assert_eq!(statuses, [403, 403, 200], "{path}");
        assert_eq!(
            *door.calls.lock().unwrap(),
            [("open".into(), json!({"path": path}))],
            "only the unbound request reaches a handler"
        );
    }
}

#[test]
fn tenant_status_and_usage_default_only_a_bound_tokens_missing_id() {
    let door = Door::start();
    for name in ["tenant_status", "tenant_usage"] {
        assert_eq!(refusal(&door.call(&door.bound, name, json!({}))), "Reached");
        assert_eq!(door.calls.lock().unwrap().last().unwrap().1["id"], TENANT);
        let before = door.calls.lock().unwrap().len();
        let reply = door.call(&door.open, name, json!({}));
        assert_eq!(reply["result"]["isError"], true, "{reply}");
        assert_eq!(door.calls.lock().unwrap().len(), before);
    }
}

#[test]
fn unbound_admin_enumeration_keeps_its_legacy_prefix_even_when_loading_refuses_the_file() {
    let scratch = Scratch::new("unbound-admin-prefix");
    let (_, header) = mint(Scope::Admin);
    let padding = "x".repeat(4096 - header.len() - "tools=\n".len());
    let mut bytes = format!("{header}tools={padding}\n").into_bytes();
    bytes.extend_from_slice(&[0xff; 5000]);
    let path = scratch.path().join("legacy.hash");
    std::fs::write(&path, bytes).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
    assert_eq!(arca_mcp::admin_token_ids(scratch.path()), ["legacy"]);
    assert!(matches!(
        TokenStore::load(scratch.path()),
        Err(TokenStoreError::Malformed { .. })
    ));
}

fn refusal(reply: &Value) -> &Value {
    &reply["error"]["data"]["refusal"]
}

#[test]
fn tenant_lines_load_and_local_service_names_are_normalized() {
    let scratch = Scratch::new("tenant-format");
    let secret = token(
        &scratch,
        "bound",
        Scope::Doctor,
        "services=web,abcdefgh/db\ntenant=abcdefgh\n",
    );
    let store = TokenStore::load(scratch.path()).unwrap();
    let caller = store
        .lookup(&secret)
        .expect("a tenant-bound token authenticates");
    assert_eq!(
        caller.limits.services.unwrap().as_line(),
        "abcdefgh/db,abcdefgh/web"
    );
}

#[test]
fn maximum_service_names_round_trip_with_tools_and_keep_the_unbound_file_limit() {
    let names = (0..64)
        .map(|n| format!("a{n:02}{}", "x".repeat(37)))
        .collect::<Vec<_>>()
        .join(",");
    let services = arca_api::limits::ServiceLimit::parse(&names).unwrap();
    let mut tools: Vec<String> = verbs()
        .iter()
        .filter(|v| arca_api::may_call(Scope::Operator, v))
        .map(|v| v.name.to_string())
        .collect();
    let plain_limits = arca_mcp::Limits {
        services: Some(services.clone()),
        ..Default::default()
    };
    let (plain, plain_text) = loop {
        let minted = arca_mcp::mint_limited(Scope::Operator, Some(&tools), &plain_limits);
        if minted.1.len() <= 4096 {
            break minted;
        }
        tools
            .pop()
            .expect("there are enough allowed tools to exercise the boundary");
    };
    let limits = arca_mcp::Limits {
        tenant: Some(TENANT.into()),
        services: services.in_tenant(TENANT),
        ..Default::default()
    };
    let (bound, text) = arca_mcp::mint_limited(Scope::Operator, Some(&tools), &limits);
    assert!(
        text.len() > 4096,
        "the tenant qualifiers cross the former file bound"
    );
    let scratch = Scratch::new("tenant-full-file");
    write_hash_file(scratch.path(), "plain", &plain_text, 0o400);
    write_hash_file(scratch.path(), "bound", &text, 0o400);
    let store = TokenStore::load(scratch.path())
        .expect("a valid bound mint never poisons the token directory");
    assert!(store.lookup(&plain).is_some());
    assert_eq!(store.lookup(&bound).unwrap().limits, limits);

    let scratch = Scratch::new("unbound-file-limit");
    let (_, text) = mint(Scope::Operator);
    let oversized = format!("{text}tools={}\n", "arca_status,".repeat(400));
    assert!(oversized.len() > 4096 && oversized.len() < 8192);
    write_hash_file(scratch.path(), "too-large", &oversized, 0o400);
    assert!(matches!(
        TokenStore::load(scratch.path()),
        Err(TokenStoreError::Malformed { .. })
    ));

    let scratch = Scratch::new("bound-file-limit");
    let (_, header) = mint(Scope::Operator);
    let oversized = format!(
        "{header}tenant={TENANT}\ntools={}\n",
        "arca_status,".repeat(700)
    );
    assert!(oversized.len() > 8192);
    write_hash_file(scratch.path(), "too-large", &oversized, 0o400);
    assert!(matches!(
        TokenStore::load(scratch.path()),
        Err(TokenStoreError::Malformed { .. })
    ));

    // A tenant line after the former read boundary still excludes a limited admin from the admin enumeration.
    let scratch = Scratch::new("large-bound-admin");
    let (secret, header) = mint(Scope::Admin);
    let padding = "x".repeat(4096 - header.len() - "tools=\n".len());
    let text = format!("{header}tools={padding}\ntenant={TENANT}\n");
    write_hash_file(scratch.path(), "admin", &text, 0o400);
    let store = TokenStore::load(scratch.path()).unwrap();
    assert!(store.lookup(&secret).is_none());
    assert!(arca_mcp::admin_token_ids(scratch.path()).is_empty());
}

#[test]
fn invalid_duplicate_or_other_tenant_limits_are_malformed() {
    for lines in [
        "tenant=\n",
        "tenant=bad\n",
        "tenant=ABCDEFGH\n",
        "tenant=abcdefgh\ntenant=abcdefgh\n",
        "tenant=abcdefgh\nservices=ijklmnop/web\n",
        "tenant=abcdefgh\nservices=web,abcdefgh/web\n",
    ] {
        let scratch = Scratch::new("tenant-invalid");
        token(&scratch, "bad", Scope::Doctor, lines);
        assert!(
            matches!(
                TokenStore::load(scratch.path()),
                Err(TokenStoreError::Malformed { .. })
            ),
            "{lines:?}"
        );
    }
}

#[test]
fn a_bound_parent_never_hands_down_an_unbound_or_different_tenant_token() {
    let scratch = Scratch::new("tenant-chain");
    token(
        &scratch,
        "parent",
        Scope::Doctor,
        "tenant=abcdefgh\nservices=web\n",
    );
    let same = token(
        &scratch,
        "same",
        Scope::Viewer,
        "tenant=abcdefgh\nservices=abcdefgh/web\nby=parent\n",
    );
    let open = token(
        &scratch,
        "open",
        Scope::Viewer,
        "services=abcdefgh/web\nby=parent\n",
    );
    let other = token(
        &scratch,
        "other",
        Scope::Viewer,
        "tenant=ijklmnop\nservices=web\nby=parent\n",
    );
    let store = TokenStore::load(scratch.path()).unwrap();
    assert!(store.lookup(&same).is_some());
    assert!(store.lookup(&open).is_none());
    assert!(store.lookup(&other).is_none());
}

#[test]
fn omitted_and_same_tenants_resolve_before_the_handler_and_other_tenants_never_run() {
    let door = Door::start();
    for args in [
        json!({"name": "web"}),
        json!({"name": "web", "tenant": TENANT}),
    ] {
        assert_eq!(
            refusal(&door.call(&door.bound, "service_status", args)),
            "Reached"
        );
        assert_eq!(
            door.calls.lock().unwrap().last().unwrap().1["tenant"],
            TENANT
        );
    }
    let before = door.calls.lock().unwrap().len();
    assert_eq!(
        refusal(&door.call(
            &door.bound,
            "service_status",
            json!({"name": "web", "tenant": "ijklmnop"})
        )),
        "OutsideTokenTenant"
    );
    assert_eq!(door.calls.lock().unwrap().len(), before);
    assert_eq!(
        refusal(&door.call(&door.open, "service_status", json!({"name": "web"}))),
        "Reached"
    );
    assert!(
        door.calls
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .1
            .get("tenant")
            .is_none()
    );
}

#[test]
fn services_are_checked_after_resolving_the_tenant() {
    let door = Door::start();
    assert_eq!(
        refusal(&door.call(&door.limited, "service_status", json!({"name": "web"}))),
        "Reached"
    );
    assert_eq!(
        refusal(&door.call(&door.limited, "service_status", json!({"name": "db"}))),
        "OutsideTokenServices"
    );
    assert_eq!(door.calls.lock().unwrap().len(), 1);
}

#[test]
fn required_deployment_tenants_default_before_validation_only_for_bound_tokens() {
    let door = Door::start();
    for (name, args) in [
        ("deploy_confirm", json!({"deploy": "draft"})),
        ("deploy_rollback", json!({"deploy": "draft"})),
        (
            "deploy_remove",
            json!({"deploy": "draft", "approval": "grant"}),
        ),
    ] {
        assert_eq!(
            refusal(&door.call(&door.bound, name, args.clone())),
            "Reached"
        );
        assert_eq!(
            door.calls.lock().unwrap().last().unwrap().1["tenant"],
            TENANT
        );
        let before = door.calls.lock().unwrap().len();
        let reply = door.call(&door.open, name, args);
        assert_eq!(reply["result"]["isError"], true, "{reply}");
        assert_eq!(door.calls.lock().unwrap().len(), before);
    }
    // Input schemas remain the same for both kinds of token.
    for token in [&door.bound, &door.open] {
        let reply = door.rpc(token, "tools/list", json!({}));
        let tool = reply["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "deploy_rollback")
            .unwrap();
        assert!(
            tool["inputSchema"]["required"]
                .as_array()
                .unwrap()
                .contains(&json!("tenant"))
        );
    }
}

#[test]
fn proposals_default_required_tenants_and_foreign_tenants_are_refused_before_validation() {
    let door = Door::start();
    let reply = door.call(
        &door.bound,
        "action_propose",
        json!({"action": "deploy_remove", "args": {"deploy": "draft"}}),
    );
    assert_eq!(refusal(&reply), "Reached", "{reply}");
    assert_eq!(
        door.calls.lock().unwrap().last().unwrap().1["args"]["tenant"],
        TENANT
    );
    let before = door.calls.lock().unwrap().len();
    for (name, args) in [
        ("service_create", json!({"tenant": "ijklmnop", "name": 42})),
        (
            "action_propose",
            json!({"action": "deploy_remove", "args": {"tenant": "ijklmnop", "deploy": 42}}),
        ),
    ] {
        assert_eq!(
            refusal(&door.call(&door.bound, name, args)),
            "OutsideTokenTenant"
        );
    }
    assert_eq!(door.calls.lock().unwrap().len(), before);
}

#[test]
fn node_verbs_and_linked_nodes_are_refused_before_any_handler() {
    let door = Door::start();
    for name in [
        "node_metrics",
        "watch_report",
        "ontology_query",
        "registry_list",
        "os_update",
        "journal_list",
        "plan_list",
        "postgres_list",
        "function_list",
        "storage_list",
        "route_list",
        "tenant_create",
    ] {
        let reply = door.call(&door.bound, name, json!({}));
        // Some write schemas require arguments; either way no handler may run.
        assert_ne!(refusal(&reply), "Reached", "{name}: {reply}");
    }
    for (name, args) in [
        ("arca_status", json!({"node": "elsewhere"})),
        (
            "service_status",
            json!({"name": "web", "node": "elsewhere"}),
        ),
    ] {
        assert_eq!(
            refusal(&door.call(&door.bound, name, args)),
            "OutsideTokenTenant"
        );
    }
    assert!(door.calls.lock().unwrap().is_empty());
}

#[test]
fn tenant_reads_only_return_the_bound_tenant() {
    let door = Door::start();
    let reply = door.call(&door.bound, "tenant_list", json!({}));
    let rows = reply["result"]["structuredContent"]["tenants"]
        .as_array()
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], TENANT);
    for name in ["tenant_status", "tenant_usage"] {
        assert_eq!(
            refusal(&door.call(&door.bound, name, json!({"id": TENANT}))),
            "Reached"
        );
        assert_eq!(
            refusal(&door.call(&door.bound, name, json!({"id": "ijklmnop"}))),
            "OutsideTokenTenant"
        );
    }
}

#[test]
fn proposals_resolve_inner_arguments_and_refuse_other_tenants_before_being_saved() {
    let door = Door::start();
    let proposal = |args| json!({"action": "service_remove", "args": args});
    assert_eq!(
        refusal(&door.call(
            &door.limited,
            "action_propose",
            proposal(json!({"name": "web"}))
        )),
        "Reached"
    );
    assert_eq!(
        door.calls.lock().unwrap().last().unwrap().1["args"]["tenant"],
        TENANT
    );
    let before = door.calls.lock().unwrap().len();
    for args in [
        json!({"name": "web", "tenant": "ijklmnop"}),
        json!({"name": "db"}),
    ] {
        assert_ne!(
            refusal(&door.call(&door.limited, "action_propose", proposal(args))),
            "Reached"
        );
    }
    assert_eq!(door.calls.lock().unwrap().len(), before);
}

#[test]
fn the_tool_list_contains_only_the_closed_tenant_allowlist() {
    let door = Door::start();
    let reply = door.rpc(&door.bound, "tools/list", json!({}));
    let names: Vec<_> = reply["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"service_list"));
    assert!(names.contains(&"tenant_usage"));
    for name in [
        "node_metrics",
        "watch_report",
        "ontology_query",
        "registry_list",
        "os_update",
        "service_import",
        "service_publish",
        "tenant_create",
    ] {
        assert!(!names.contains(&name), "{name}");
    }
}

#[test]
fn mint_round_trips_the_binding_and_unbound_parent_can_narrow_to_it() {
    let scratch = Scratch::new("tenant-mint");
    token(&scratch, "parent", Scope::Doctor, "services=abcdefgh/web\n");
    let limits = arca_mcp::Limits {
        tenant: Some(TENANT.into()),
        services: Some(arca_api::limits::ServiceLimit::parse("web").unwrap()),
        by: Some("parent".into()),
        ..Default::default()
    };
    let (secret, text) = arca_mcp::mint_limited(Scope::Viewer, None, &limits);
    assert!(text.contains("\ntenant=abcdefgh\n"));
    write_hash_file(scratch.path(), "child", &text, 0o400);
    let store = TokenStore::load(scratch.path()).unwrap();
    let caller = store.lookup(&secret).unwrap();
    assert_eq!(caller.limits.tenant.as_deref(), Some(TENANT));
    assert_eq!(caller.limits.services.unwrap().as_line(), "abcdefgh/web");
}

#[test]
fn a_stored_tenant_bound_admin_never_authenticates_or_counts_as_the_nodes_admin() {
    let scratch = Scratch::new("tenant-admin");
    let secret = token(&scratch, "admin", Scope::Admin, "tenant=abcdefgh\n");
    let store = TokenStore::load(scratch.path()).unwrap();
    assert!(store.lookup(&secret).is_none());
    assert_eq!(store.skipped().len(), 1);
    assert!(arca_mcp::admin_token_ids(scratch.path()).is_empty());
}

#[test]
fn label_writes_obey_bound_tenants_and_service_limits_before_the_handler() {
    let door = Door::start();
    for args in [
        json!({"name":"web","labels":{"hold":"true"}}),
        json!({"tenant":TENANT,"name":"web","labels":{}}),
    ] {
        assert_eq!(
            refusal(&door.call(&door.limited, "service_labels_set", args)),
            "Reached"
        );
        assert_eq!(
            door.calls.lock().unwrap().last().unwrap().1["tenant"],
            TENANT
        );
    }
    let before = door.calls.lock().unwrap().len();
    for (args, expected) in [
        (
            json!({"tenant":"ijklmnop","name":"web","labels":{}}),
            "OutsideTokenTenant",
        ),
        (
            json!({"tenant":"","name":"web","labels":{}}),
            "OutsideTokenTenant",
        ),
        (json!({"name":"db","labels":{}}), "OutsideTokenServices"),
    ] {
        assert_eq!(
            refusal(&door.call(&door.limited, "service_labels_set", args)),
            expected
        );
    }
    assert_eq!(door.calls.lock().unwrap().len(), before);
    let viewer = post(
        &mut connect_unix(&door.socket),
        &[bearer(&door.viewer)],
        &rpc(
            json!(1),
            "tools/call",
            json!({"name":"service_labels_set", "arguments":{"name":"web","labels":{}}}),
        ),
    );
    assert_eq!(viewer.status, 403);
    assert_eq!(door.calls.lock().unwrap().len(), before);
}
