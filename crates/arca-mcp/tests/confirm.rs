//! A destructive verb's optional `confirm` at the MCP door, for every destructive verb in the table. `true` runs the call
//! and the handler sees the arguments without `confirm`; `false` is refused in plain words before the handler runs; a value
//! that is not a boolean is refused by the params schema. The result each handler answers is checked by the door against
//! the verb's result schema, so an accepted call is a whole round trip.

mod common;

use std::sync::{Arc, Mutex};

use arca_api::{Scope, conforms, verb, verbs};
use arca_mcp::{CallError, Caller, Server, TokenStore, ToolHandler, serve_unix};
use common::{Scratch, bearer, connect_unix, mint_into, post, rpc, wait_for_socket};
use serde_json::{Value, json};
use tokio::runtime::Runtime;

/// Every destructive verb, with arguments its params schema accepts, the result its handler answers, and the plain words
/// of its `confirm: false` refusal.
fn cases() -> Vec<(&'static str, Value, Value, &'static str)> {
    let plan = json!({
        "plan": "a".repeat(64),
        "entry": "postgres-orders",
        "createdAt": 1,
        "expiresAt": 2,
        "objects": [],
        "paramsDigest": "b".repeat(64),
        "statement": "s",
    });
    vec![
        (
            "approval_submit",
            json!({ "plan": "a".repeat(64), "approval": "x.y" }),
            json!({ "plan": "a".repeat(64), "deleted": [], "drift": [], "partial": false }),
            "confirm is false: nothing was deleted and no approval was spent",
        ),
        (
            "action_approve",
            json!({ "plan": "act-0123456789ab", "approval": "x.y" }),
            json!({ "plan": "act-0123456789ab", "id": "act-0123456789ab", "action": "service_remove", "result": {} }),
            "confirm is false: nothing was run and no approval was spent",
        ),
        (
            "postgres_delete",
            json!({ "name": "orders" }),
            json!({ "plan": plan, "removed": false }),
            "confirm is false: nothing was deleted",
        ),
        (
            "service_remove",
            json!({ "name": "pg", "approval": "x.y" }),
            json!({ "removed": true }),
            "confirm is false: nothing was removed and no approval was spent",
        ),
        (
            "service_import",
            json!({ "name": "db1", "approval": "0123456789abcdef" }),
            json!({
                "imported": true,
                "preImage": "c".repeat(64),
                "service": {
                    "name": "db1", "template": "postgres-autopg", "state": "running", "ipv4": "10.0.5.10",
                    "ports": [5432], "links": [], "published": null, "lanPort": null
                }
            }),
            "confirm is false: nothing was imported and no approval was spent",
        ),
        (
            "service_release",
            json!({ "name": "db1", "approval": "0123456789abcdef" }),
            json!({ "released": true, "restored": true, "preImage": "c".repeat(64) }),
            "confirm is false: nothing was released and no approval was spent",
        ),
        (
            "gpu_detach",
            json!({ "instance": "box", "pci": "0000:01:00.0", "approval": "x.y" }),
            json!({ "instance": "box", "project": "default", "pci": "0000:01:00.0", "device": "gpu0", "attached": false, "changed": true }),
            "confirm is false: nothing was detached and no approval was spent",
        ),
        (
            "storage_add",
            json!({ "name": "pool1", "path": "/mnt/disk", "approval": "x.y" }),
            json!({ "pool": "arca-snapshots-pool1", "driver": "dir", "source": "/mnt/disk/arca-pool" }),
            "confirm is false: nothing was added and no approval was spent",
        ),
        (
            "snapshot_delete",
            json!({ "name": "snap1", "approval": "x.y" }),
            json!({ "snapshot": "snap1", "pool": "arca-snapshots-pool1", "kind": "instance", "state": "deleted" }),
            "confirm is false: nothing was deleted and no approval was spent",
        ),
        (
            "instance_stop",
            json!({ "project": "arcak-svc", "instance": "web", "approval": "x.y" }),
            json!({ "project": "arcak-svc", "instance": "web", "action": "stop", "status": "Stopped", "changed": true, "message": "stopped the instance web in project arcak-svc" }),
            "confirm is false: nothing was stopped and no approval was spent",
        ),
        (
            "instance_restart",
            json!({ "project": "arcak-svc", "instance": "web", "approval": "x.y" }),
            json!({ "project": "arcak-svc", "instance": "web", "action": "restart", "status": "Running", "changed": true, "message": "restarted the instance web in project arcak-svc" }),
            "confirm is false: nothing was restarted and no approval was spent",
        ),
        (
            "tenant_remove",
            json!({ "id": "abcdefgh", "approval": "x.y" }),
            json!({ "removed": true }),
            "confirm is false: nothing was removed and no approval was spent",
        ),
        (
            "deploy_rollback",
            json!({ "tenant": "abcdefgh", "deploy": "d1" }),
            json!({ "rolledBack": true, "deploy": "d1", "removed": ["web"] }),
            "confirm is false: nothing was rolled back",
        ),
        (
            "deploy_remove",
            json!({ "tenant": "abcdefgh", "deploy": "d1", "approval": "x.y" }),
            json!({ "removed": true, "deploy": "d1", "parts": ["web"] }),
            "confirm is false: nothing was removed and no approval was spent",
        ),
    ]
}

/// Answers a fixed result and records the arguments of every call, so a test sees whether the handler ran and with what.
struct Recording {
    result: Value,
    calls: Arc<Mutex<Vec<(String, Value)>>>,
    verb: &'static str,
}

impl ToolHandler for Recording {
    fn call(&self, args: Value, _caller: &Caller) -> Result<Value, CallError> {
        self.calls
            .lock()
            .expect("the call log")
            .push((self.verb.to_string(), args));
        Ok(self.result.clone())
    }
}

struct Fixture {
    _rt: Runtime,
    _scratch: Scratch,
    socket: std::path::PathBuf,
    operator: String,
    calls: Arc<Mutex<Vec<(String, Value)>>>,
}

impl Fixture {
    fn start(tag: &str) -> Fixture {
        let scratch = Scratch::new(tag);
        let operator = mint_into(scratch.path(), "o-operator", Scope::Operator);
        let tokens = TokenStore::load(scratch.path()).expect("the store loads");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut server = Server::new(verbs(), tokens);
        for (name, _, result, _) in cases() {
            server = server.handle(
                name,
                Arc::new(Recording {
                    result,
                    calls: calls.clone(),
                    verb: name,
                }),
            );
        }
        let rt = Runtime::new().expect("runtime");
        let socket = scratch.at("arca.sock");
        let (router, served) = (server.router(), socket.clone());
        rt.spawn(async move { serve_unix(router, &served).await });
        wait_for_socket(&socket);
        Fixture {
            _rt: rt,
            _scratch: scratch,
            socket,
            operator,
            calls,
        }
    }

    fn tool_call(&self, name: &str, arguments: Value) -> Value {
        let mut stream = connect_unix(&self.socket);
        let reply = post(
            &mut stream,
            &[bearer(&self.operator)],
            &rpc(
                json!(1),
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
            ),
        );
        assert_eq!(reply.status, 200, "{name}: {}", reply.body);
        reply.json()
    }

    fn take_calls(&self) -> Vec<(String, Value)> {
        std::mem::take(&mut *self.calls.lock().expect("the call log"))
    }
}

fn with(args: &Value, confirm: Value) -> Value {
    let mut args = args.clone();
    args["confirm"] = confirm;
    args
}

#[test]
fn the_cases_are_every_destructive_verb_and_their_samples_meet_the_schemas() {
    let mut destructive: Vec<&str> = verbs()
        .iter()
        .filter(|v| v.destructive)
        .map(|v| v.name)
        .collect();
    destructive.sort_unstable();
    let mut named: Vec<&str> = cases().iter().map(|(name, ..)| *name).collect();
    named.sort_unstable();
    assert_eq!(named, destructive);
    for (name, args, result, _) in cases() {
        let v = verb(name).expect("in the table");
        assert_eq!(conforms(&(v.params)(), &args), Ok(()), "{name} params");
        assert_eq!(conforms(&(v.result)(), &result), Ok(()), "{name} result");
    }
}

#[test]
fn confirm_true_runs_the_call_and_the_handler_never_sees_confirm() {
    let fx = Fixture::start("confirm-true");
    for (name, args, result, _) in cases() {
        for arguments in [args.clone(), with(&args, json!(true))] {
            let reply = fx.tool_call(name, arguments);
            assert_eq!(reply["result"]["isError"], json!(false), "{name}: {reply}");
            assert_eq!(reply["result"]["structuredContent"], result, "{name}");
            assert_eq!(
                fx.take_calls(),
                vec![(name.to_string(), args.clone())],
                "{name}: the handler ran once with the arguments as they are without confirm"
            );
        }
    }
}

#[test]
fn confirm_false_is_refused_in_plain_words_and_the_handler_never_runs() {
    let fx = Fixture::start("confirm-false");
    for (name, args, _, words) in cases() {
        let reply = fx.tool_call(name, with(&args, json!(false)));
        assert!(reply.get("result").is_none(), "{name}: {reply}");
        assert_eq!(reply["error"]["code"], json!(-32000), "{name}");
        assert_eq!(reply["error"]["message"], json!(words), "{name}");
        assert_eq!(
            reply["error"]["data"],
            json!({ "refusal": "NotConfirmed" }),
            "{name}"
        );
        assert!(
            fx.take_calls().is_empty(),
            "{name}: nothing ran, so nothing changed and no approval was spent"
        );
    }
}

#[test]
fn a_confirm_that_is_not_a_boolean_is_refused_by_the_schema() {
    let fx = Fixture::start("confirm-yes");
    for (name, args, _, _) in cases() {
        for confirm in [json!("yes"), json!(1), json!(null), json!({})] {
            let reply = fx.tool_call(name, with(&args, confirm.clone()));
            assert_eq!(
                reply["error"]["code"],
                json!(-32602),
                "{name} confirm {confirm}: {reply}"
            );
            assert!(
                fx.take_calls().is_empty(),
                "{name} confirm {confirm}: the handler never ran"
            );
        }
    }
}

#[test]
fn a_verb_that_is_not_destructive_still_refuses_confirm() {
    for v in verbs().iter().filter(|v| !v.destructive) {
        let params = (v.params)();
        assert!(
            params["properties"].get("confirm").is_none(),
            "{} takes no confirm",
            v.name
        );
    }
    let fx = Fixture::start("confirm-not-destructive");
    let reply = fx.tool_call(
        "service_create",
        json!({ "name": "pg", "template": "pg", "confirm": true }),
    );
    assert_eq!(reply["error"]["code"], json!(-32602), "{reply}");
}

#[test]
fn the_tool_list_advertises_confirm_as_an_optional_boolean_on_every_destructive_tool() {
    let fx = Fixture::start("confirm-list");
    let mut stream = connect_unix(&fx.socket);
    let reply = post(
        &mut stream,
        &[bearer(&fx.operator)],
        &rpc(json!(1), "tools/list", json!({})),
    );
    let tools = reply.json()["result"]["tools"].clone();
    for tool in tools.as_array().expect("a tool list") {
        let schema = &tool["inputSchema"];
        let destructive = tool["annotations"]["destructiveHint"] == json!(true);
        assert_eq!(
            schema["properties"].get("confirm").is_some(),
            destructive,
            "{}",
            tool["name"]
        );
        if destructive {
            assert_eq!(schema["properties"]["confirm"]["type"], json!("boolean"));
            assert!(
                !schema["required"]
                    .as_array()
                    .is_some_and(|required| required.contains(&json!("confirm"))),
                "{}: confirm is optional",
                tool["name"]
            );
        }
    }
}
