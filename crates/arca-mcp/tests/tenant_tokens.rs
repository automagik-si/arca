//! Tenant-bound tokens resolve their tenant before a handler, approval or relay can run.
mod common;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use arca_api::{Scope, verbs};
use arca_mcp::{
    CallError, Caller, Server, TokenStore, TokenStoreError, ToolHandler, mint, serve_unix,
};
use common::{Scratch, bearer, connect_unix, post, rpc, wait_for_socket, write_hash_file};
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
        let calls: Calls = Arc::default();
        let mut server = Server::new(verbs(), TokenStore::load(scratch.path()).unwrap())
            .log_to(Arc::new(|_| {}));
        for v in verbs() {
            server = server.handle(v.name, Arc::new(Recording(v.name, calls.clone())));
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
        "deploy_rollback",
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
