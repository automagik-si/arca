//! Tokens limited to named services and to a time, and tokens handed down by another (`services=`, `expires=`, `by=`).
//!
//! The format: what a limited mint writes and a load reads back; a line this release does not know leaves only its own
//! token out; a malformed limit refuses the directory as any malformed line does; an admin token is never limited; an
//! expired token authenticates nobody; a token handed down loads only while its parent does and is no wider than it.
//!
//! The door: every refusal of a limited token (a service, a pair, an instance, a node-wide verb, a proposal, a linked node,
//! a GET route, an expired token), every cut of a read (service_list, service_status, snapshot_list, watch_report), and a
//! filtered verb with no cut, which answers nothing.

mod common;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use arca_api::limits::{OUTSIDE_SERVICES, ServiceLimit, Target};
use arca_api::{Scope, Verb, VerbClass, verbs};
use arca_mcp::auth::{
    SKIP_ADMIN_LIMITED, SKIP_NO_PARENT, SKIP_UNKNOWN_LINE, SKIP_WIDER, format_rfc3339,
    parse_rfc3339,
};
use arca_mcp::{
    CallError, Caller, GetHandler, GetReply, Limits, LinkedNodes, Server, TokenStore,
    TokenStoreError, ToolHandler, mint, mint_limited, serve_unix,
};
use common::{
    Scratch, bearer, connect_unix, exchange, post, rpc, wait_for_socket, write_hash_file,
};
use serde_json::{Value, json};
use tokio::runtime::Runtime;

const NOW: u64 = 1_763_200_000;

fn services(text: &str) -> Option<ServiceLimit> {
    Some(ServiceLimit::parse(text).expect("a list"))
}

fn limits(list: Option<&str>, expires: Option<u64>, by: Option<&str>) -> Limits {
    Limits {
        tenant: None,
        services: list.and_then(services),
        expires,
        by: by.map(str::to_string),
    }
}

/// Mints a token of `scope`, `tools` and `limits` into `<id>.hash` and returns its secret.
fn mint_limited_into(
    scratch: &Scratch,
    id: &str,
    scope: Scope,
    tools: Option<&[&str]>,
    limits: &Limits,
) -> String {
    let tools: Option<Vec<String>> = tools.map(|t| t.iter().map(|s| s.to_string()).collect());
    let (secret, text) = mint_limited(scope, tools.as_deref(), limits);
    write_hash_file(scratch.path(), id, &text, 0o400);
    secret
}

// The format.

#[test]
fn a_limited_mint_writes_its_lines_and_a_load_reads_them_back() {
    let scratch = Scratch::new("limited-mint");
    let want = limits(Some("gtm,dev"), Some(NOW + 3600), None);
    let (secret, text) = mint_limited(Scope::Doctor, None, &want);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "scope=doctor");
    assert!(lines[1].starts_with("sha256="));
    assert_eq!(lines[2], "services=dev,gtm");
    assert_eq!(lines[3], format!("expires={}", format_rfc3339(NOW + 3600)));
    assert_eq!(lines.len(), 4);
    write_hash_file(scratch.path(), "limited", &text, 0o400);
    let store = TokenStore::load(scratch.path()).expect("a limited token loads");
    let caller = store.lookup_at(&secret, NOW).expect("it authenticates");
    assert_eq!(caller.limits, want);
    assert!(store.skipped().is_empty());
}

#[test]
fn a_plain_mint_writes_no_limit_lines_and_loads_with_no_limits() {
    let (_, text) = mint(Scope::Viewer);
    assert_eq!(text.lines().count(), 2);
    let scratch = Scratch::new("plain-mint");
    let secret = mint_limited_into(&scratch, "plain", Scope::Viewer, None, &Limits::default());
    let store = TokenStore::load(scratch.path()).expect("loads");
    assert_eq!(
        store.lookup(&secret).expect("known").limits,
        Limits::default()
    );
}

#[test]
fn the_optional_lines_load_in_any_order() {
    let scratch = Scratch::new("any-order");
    let (secret, text) = mint(Scope::Doctor);
    let digest = text.lines().nth(1).expect("digest").to_string();
    let expires = format_rfc3339(NOW + 60);
    let body =
        format!("scope=doctor\n{digest}\nexpires={expires}\nservices=dev\ntools=service_status\n");
    write_hash_file(scratch.path(), "ordered", &body, 0o400);
    let store = TokenStore::load(scratch.path()).expect("loads");
    let caller = store.lookup_at(&secret, NOW).expect("known");
    assert_eq!(caller.tools, Some(vec!["service_status".to_string()]));
    assert_eq!(caller.limits, limits(Some("dev"), Some(NOW + 60), None));
}

#[test]
fn a_line_this_release_does_not_know_leaves_out_its_token_and_only_it() {
    let scratch = Scratch::new("unknown-line");
    let (kept, kept_text) = mint(Scope::Viewer);
    write_hash_file(scratch.path(), "kept", &kept_text, 0o400);
    let (left, left_text) = mint(Scope::Doctor);
    write_hash_file(
        scratch.path(),
        "later",
        &format!("{left_text}projects=alpha\n"),
        0o400,
    );
    let store = TokenStore::load(scratch.path()).expect("the store loads without it");
    assert!(store.lookup(&kept).is_some(), "the other token is loaded");
    assert_eq!(
        store.lookup(&left),
        None,
        "its own secret authenticates nobody"
    );
    assert_eq!(
        store.skipped(),
        vec![("later.hash".to_string(), SKIP_UNKNOWN_LINE)]
    );
    assert_eq!(store.ids().len(), 1);
}

#[test]
fn a_malformed_limit_line_refuses_the_directory_as_any_malformed_line() {
    let (_, good) = mint(Scope::Viewer);
    let digest = good.lines().nth(1).expect("digest").to_string();
    let cases = [
        ("an empty list", "services=\n".to_string()),
        ("a service twice", "services=dev,dev\n".to_string()),
        ("an uppercase service", "services=Dev\n".to_string()),
        ("a bad tenant", "services=x/web\n".to_string()),
        (
            "a time with an offset",
            "expires=2026-12-01T00:00:00+00:00\n".to_string(),
        ),
        (
            "a time that is no date",
            "expires=2026-02-30T00:00:00Z\n".to_string(),
        ),
        ("a word for a time", "expires=tomorrow\n".to_string()),
        ("a parent that is no id", "by=Parent\n".to_string()),
        ("a line twice", "services=dev\nservices=gtm\n".to_string()),
        ("a tools line twice", "tools=a\ntools=b\n".to_string()),
        ("a line with no =", "services\n".to_string()),
        ("an empty key", "=x\n".to_string()),
        ("a key with an uppercase letter", "Projects=x\n".to_string()),
        ("an empty line", "\n".to_string()),
    ];
    for (why, extra) in cases {
        let scratch = Scratch::new("bad-limit");
        write_hash_file(
            scratch.path(),
            "bad",
            &format!("scope=viewer\n{digest}\n{extra}"),
            0o400,
        );
        assert_eq!(
            TokenStore::load(scratch.path()).expect_err(why),
            TokenStoreError::Malformed {
                file: "bad.hash".into()
            },
            "{why}"
        );
    }
}

#[test]
fn a_token_handed_down_by_itself_is_malformed() {
    let scratch = Scratch::new("self-parent");
    mint_limited_into(
        &scratch,
        "loop",
        Scope::Viewer,
        None,
        &limits(None, None, Some("loop")),
    );
    assert_eq!(
        TokenStore::load(scratch.path()).expect_err("refused"),
        TokenStoreError::Malformed {
            file: "loop.hash".into()
        }
    );
}

#[test]
fn an_admin_token_limited_to_services_is_left_out() {
    let scratch = Scratch::new("admin-limited");
    let secret = mint_limited_into(
        &scratch,
        "boss",
        Scope::Admin,
        None,
        &limits(Some("dev"), None, None),
    );
    let store = TokenStore::load(scratch.path()).expect("loads");
    assert_eq!(store.lookup(&secret), None);
    assert_eq!(
        store.skipped(),
        vec![("boss.hash".to_string(), SKIP_ADMIN_LIMITED)]
    );
    assert!(arca_mcp::admin_token_ids(scratch.path()).is_empty());
}

#[test]
fn an_expired_token_authenticates_nobody_from_its_expiry_on() {
    let scratch = Scratch::new("expiry");
    let secret = mint_limited_into(
        &scratch,
        "short",
        Scope::Viewer,
        None,
        &limits(None, Some(NOW), None),
    );
    let store = TokenStore::load(scratch.path()).expect("an expired token still loads");
    assert!(store.lookup_at(&secret, NOW - 1).is_some());
    assert_eq!(store.lookup_at(&secret, NOW), None);
    assert_eq!(store.lookup_at(&secret, NOW + 1), None);
    // The clock of the real lookup is long past this moment.
    assert_eq!(store.lookup(&secret), None);
}

#[test]
fn a_token_handed_down_loads_only_while_its_parent_does() {
    let scratch = Scratch::new("chain");
    let parent_limits = limits(Some("dev,gtm"), Some(NOW + 600), None);
    mint_limited_into(&scratch, "parent", Scope::Doctor, None, &parent_limits);
    let child = mint_limited_into(
        &scratch,
        "child",
        Scope::Viewer,
        None,
        &limits(Some("dev"), Some(NOW + 60), Some("parent")),
    );
    let grandchild = mint_limited_into(
        &scratch,
        "grandchild",
        Scope::Viewer,
        None,
        &limits(Some("dev"), Some(NOW + 30), Some("child")),
    );
    let store = TokenStore::load(scratch.path()).expect("loads");
    assert_eq!(
        store
            .lookup_at(&child, NOW)
            .expect("loaded")
            .limits
            .by
            .as_deref(),
        Some("parent")
    );
    assert!(store.lookup_at(&grandchild, NOW).is_some());

    // The parent is revoked: its whole chain goes at the next load.
    std::fs::remove_file(scratch.at("parent.hash")).expect("revoke the parent");
    store.reload(scratch.path()).expect("reloads");
    assert_eq!(store.lookup_at(&child, NOW), None);
    assert_eq!(store.lookup_at(&grandchild, NOW), None);
    assert_eq!(
        store.skipped(),
        vec![
            ("child.hash".to_string(), SKIP_NO_PARENT),
            ("grandchild.hash".to_string(), SKIP_NO_PARENT),
        ]
    );
}

#[test]
fn a_token_handed_down_that_is_wider_than_its_parent_is_left_out() {
    let parent = limits(Some("dev,gtm"), Some(NOW + 600), None);
    let narrow = limits(Some("dev"), Some(NOW + 60), Some("parent"));
    let wider: Vec<(&str, Scope, Option<&[&str]>, Limits)> = vec![
        (
            "a higher scope",
            Scope::Operator,
            Some(&["service_status"]),
            narrow.clone(),
        ),
        (
            "no tool list under a parent's",
            Scope::Viewer,
            None,
            narrow.clone(),
        ),
        (
            "a tool its parent lacks",
            Scope::Viewer,
            Some(&["service_logs"]),
            narrow.clone(),
        ),
        (
            "every service",
            Scope::Viewer,
            Some(&["service_status"]),
            limits(None, Some(NOW + 60), Some("parent")),
        ),
        (
            "a service its parent lacks",
            Scope::Viewer,
            Some(&["service_status"]),
            limits(Some("cx"), Some(NOW + 60), Some("parent")),
        ),
        (
            "no expiry under a parent's",
            Scope::Viewer,
            Some(&["service_status"]),
            limits(Some("dev"), None, Some("parent")),
        ),
        (
            "a later expiry",
            Scope::Viewer,
            Some(&["service_status"]),
            limits(Some("dev"), Some(NOW + 601), Some("parent")),
        ),
    ];
    for (why, scope, tools, child_limits) in wider {
        let scratch = Scratch::new("wider");
        mint_limited_into(
            &scratch,
            "parent",
            Scope::Doctor,
            Some(&["service_status", "service_list"]),
            &parent,
        );
        let child = mint_limited_into(&scratch, "child", scope, tools, &child_limits);
        let store = TokenStore::load(scratch.path()).expect("loads");
        assert_eq!(store.lookup_at(&child, NOW), None, "{why}");
        assert_eq!(
            store.skipped(),
            vec![("child.hash".to_string(), SKIP_WIDER)],
            "{why}"
        );
    }
    // The same child, narrower in every way, loads.
    let scratch = Scratch::new("narrow");
    mint_limited_into(
        &scratch,
        "parent",
        Scope::Doctor,
        Some(&["service_status", "service_list"]),
        &parent,
    );
    let child = mint_limited_into(
        &scratch,
        "child",
        Scope::Viewer,
        Some(&["service_status"]),
        &narrow,
    );
    let store = TokenStore::load(scratch.path()).expect("loads");
    assert!(store.lookup_at(&child, NOW).is_some());
    // An equal child is no wider either.
    let scratch = Scratch::new("equal");
    mint_limited_into(&scratch, "parent", Scope::Doctor, None, &parent);
    let equal = limits(Some("dev,gtm"), Some(NOW + 600), Some("parent"));
    let child = mint_limited_into(&scratch, "child", Scope::Doctor, None, &equal);
    let store = TokenStore::load(scratch.path()).expect("loads");
    assert!(store.lookup_at(&child, NOW).is_some());
}

#[test]
fn tokens_that_hand_each_other_down_in_a_loop_are_left_out() {
    let scratch = Scratch::new("loop");
    let a = mint_limited_into(
        &scratch,
        "a",
        Scope::Viewer,
        None,
        &limits(None, None, Some("b")),
    );
    let b = mint_limited_into(
        &scratch,
        "b",
        Scope::Viewer,
        None,
        &limits(None, None, Some("a")),
    );
    let root = mint_limited_into(&scratch, "root", Scope::Viewer, None, &Limits::default());
    let store = TokenStore::load(scratch.path()).expect("loads");
    assert_eq!(store.lookup(&a), None);
    assert_eq!(store.lookup(&b), None);
    assert!(store.lookup(&root).is_some());
    assert_eq!(store.skipped().len(), 2);
}

#[test]
fn a_moment_is_written_and_read_in_rfc3339_utc_to_the_second() {
    assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
    assert_eq!(format_rfc3339(951_782_400), "2000-02-29T00:00:00Z");
    assert_eq!(format_rfc3339(NOW), "2025-11-15T09:46:40Z");
    for secs in [
        0,
        59,
        86_399,
        951_782_400,
        NOW,
        4_102_444_799,
        253_402_300_799,
    ] {
        assert_eq!(parse_rfc3339(&format_rfc3339(secs)), Some(secs), "{secs}");
    }
    for bad in [
        "",
        "2025-11-15T09:46:40",
        "2025-11-15 09:46:40Z",
        "2025-11-15T09:46:40.5Z",
        "2025-11-15t09:46:40z",
        "1969-12-31T23:59:59Z",
        "2025-13-01T00:00:00Z",
        "2025-00-01T00:00:00Z",
        "2025-02-29T00:00:00Z",
        "2025-04-31T00:00:00Z",
        "2025-11-15T24:00:00Z",
        "2025-11-15T23:60:00Z",
        "2025-11-15T23:59:60Z",
        "+025-11-15T23:59:59Z",
        "2025-11-15T09:46:40+01:00",
    ] {
        assert_eq!(parse_rfc3339(bad), None, "{bad:?}");
    }
    assert_eq!(parse_rfc3339("2024-02-29T00:00:00Z"), Some(1_709_164_800));
    assert_eq!(parse_rfc3339("2100-02-29T00:00:00Z"), None);
    assert!(parse_rfc3339("2000-02-29T00:00:00Z").is_some());
}

// The door.

/// The verb table plus one read that is cut to a token's services and has no cut: the door must answer it with nothing.
fn table() -> &'static [Verb] {
    static TABLE: OnceLock<&'static [Verb]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut all = verbs().to_vec();
        all.push(Verb {
            name: "uncut_probe",
            summary: "A filtered read with no cut.",
            class: VerbClass::L,
            min_scope: Scope::Viewer,
            writes: false,
            destructive: false,
            untrusted: false,
            output_key: None,
            summary_text: arca_api::verbs::summary_done,
            cut: arca_api::verbs::no_cut,
            params: || json!({"type": "object", "properties": {}, "additionalProperties": false}),
            result: || json!({"type": "object"}),
            target: Target::Filtered,
        });
        Box::leak(all.into_boxed_slice())
    })
}

/// Every call a handler was reached with: its verb and its arguments.
type Calls = Arc<Mutex<Vec<(String, Value)>>>;

/// Records its call and answers with a fixed value.
struct Recording {
    verb: &'static str,
    answer: Result<Value, (&'static str, &'static str)>,
    calls: Calls,
}

impl ToolHandler for Recording {
    fn call(&self, args: Value, _caller: &Caller) -> Result<Value, CallError> {
        self.calls
            .lock()
            .expect("calls")
            .push((self.verb.to_string(), args));
        self.answer
            .clone()
            .map_err(|(code, message)| CallError::Refused {
                code: code.into(),
                message: message.into(),
            })
    }
}

/// A linked node that records each relay and answers an empty list.
struct Linked(Calls);

impl LinkedNodes for Linked {
    fn relay(&self, node: &str, verb: &str, _args: Value) -> Result<Value, CallError> {
        self.0
            .lock()
            .expect("calls")
            .push((format!("relay:{node}"), json!(verb)));
        Ok(
            json!({"jsonrpc": "2.0", "id": 1, "result": {"structuredContent": {"services": []}, "content": [], "isError": false}}),
        )
    }
}

/// A GET route's reading.
struct Page;

impl GetHandler for Page {
    fn get(&self, _query: Option<&str>, _caller: &Caller) -> Result<GetReply, CallError> {
        Ok(GetReply {
            content_type: "text/plain",
            body: b"every service of the node".to_vec(),
        })
    }
}

fn view(name: &str, links: &[&str]) -> Value {
    json!({
        "name": name,
        "template": "nats",
        "state": "running",
        "ipv4": "10.0.0.2",
        "ports": [],
        "links": links.iter().map(|to| json!({"to": to, "port": 4222})).collect::<Vec<_>>(),
        "published": null,
        "lanPort": null
    })
}

fn snapshot(project: &str, name: &str, kind: &str) -> Value {
    json!({
        "name": format!("snap-{name}"),
        "kind": kind,
        "project": "arcak-snapshots",
        "sourceProject": project,
        "sourceName": name,
        "sourcePool": null,
        "pool": "default",
        "size": null,
        "created": "2026-11-15T00:00:00Z",
        "state": "ready",
        "droppedDevices": []
    })
}

fn instance(name: &str) -> Value {
    json!({
        "name": name,
        "kind": "container",
        "status": "Running",
        "owner": {"class": "kernel"},
        "privileged": false,
        "identity": false,
        "configHash": "0".repeat(64)
    })
}

struct Door {
    _rt: Runtime,
    _scratch: Scratch,
    socket: PathBuf,
    /// Viewer, limited to dev and gtm.
    viewer: String,
    /// Doctor, limited to dev.
    doctor: String,
    /// Viewer, no limit.
    open: String,
    /// Viewer, limited to dev, expired.
    expired: String,
    calls: Calls,
}

impl Door {
    fn start(tag: &str) -> Door {
        let scratch = Scratch::new(tag);
        let now = arca_mcp::auth::unix_now();
        let viewer = mint_limited_into(
            &scratch,
            "limited-viewer",
            Scope::Viewer,
            None,
            &limits(Some("dev,gtm"), Some(now + 3600), None),
        );
        let doctor = mint_limited_into(
            &scratch,
            "limited-doctor",
            Scope::Doctor,
            None,
            &limits(Some("dev"), None, None),
        );
        let open = mint_limited_into(
            &scratch,
            "open-viewer",
            Scope::Viewer,
            None,
            &Limits::default(),
        );
        let expired = mint_limited_into(
            &scratch,
            "old-viewer",
            Scope::Viewer,
            None,
            &limits(Some("dev"), Some(now - 1), None),
        );
        let tokens = TokenStore::load(scratch.path()).expect("loads");
        let calls: Calls = Arc::new(Mutex::new(Vec::new()));
        let recording = |verb: &'static str, answer: Value| -> Arc<dyn ToolHandler> {
            Arc::new(Recording {
                verb,
                answer: Ok(answer),
                calls: calls.clone(),
            })
        };
        let server = Server::new(table(), tokens)
            .log_to(Arc::new(|_| {}))
            .handle("arca_status", recording("arca_status", json!({})))
            .handle(
                "service_list",
                recording(
                    "service_list",
                    json!({"services": [view("dev", &["gtm", "pg-secret"]), view("pg-secret", &[]), view("gtm", &["dev"])]}),
                ),
            )
            .handle(
                "service_status",
                recording("service_status", view("dev", &["gtm", "pg-secret"])),
            )
            .handle(
                "service_logs",
                recording("service_logs", json!({"lines": ["dev started"]})),
            )
            .handle(
                "snapshot_list",
                recording(
                    "snapshot_list",
                    json!({"snapshots": [
                        snapshot("arcak-svc", "dev", "container"),
                        snapshot("arcak-svc", "pg-secret", "container"),
                        snapshot("arcak-svc", "dev", "volume"),
                        snapshot("droid", "dev", "container")
                    ]}),
                ),
            )
            .handle(
                "watch_report",
                recording(
                    "watch_report",
                    json!({
                        "at": 1,
                        "projects": [
                            {"project": "arcak-svc", "instances": [instance("dev"), instance("pg-secret")]},
                            {"project": "droid-secret", "instances": [instance("phone1")]}
                        ],
                        "drift": [{"kind": "entryWithoutObject", "entry": "service-pg-secret"}]
                    }),
                ),
            )
            .handle("uncut_probe", recording("uncut_probe", json!({"everything": true})))
            .handle(
                "service_connect",
                recording("service_connect", json!({})),
            )
            .handle(
                "instance_start",
                recording("instance_start", json!({})),
            )
            .handle(
                "action_propose",
                Arc::new(Recording {
                    verb: "action_propose",
                    answer: Err(("Reached", "the proposal reached its handler")),
                    calls: calls.clone(),
                }),
            )
            .linked_nodes(Arc::new(Linked(calls.clone())))
            .get_route("/everything", "arca_status", Arc::new(Page));
        let rt = Runtime::new().expect("runtime");
        let socket = scratch.at("arca.sock");
        let (router, served) = (server.router(), socket.clone());
        rt.spawn(async move { serve_unix(router, &served).await });
        wait_for_socket(&socket);
        Door {
            _rt: rt,
            _scratch: scratch,
            socket,
            viewer,
            doctor,
            open,
            expired,
            calls,
        }
    }

    fn call(&self, token: &str, name: &str, arguments: Value) -> (u16, Value) {
        let body = rpc(
            json!(1),
            "tools/call",
            json!({"name": name, "arguments": arguments}),
        );
        let reply = post(&mut connect_unix(&self.socket), &[bearer(token)], &body);
        let json = if reply.body.is_empty() {
            Value::Null
        } else {
            reply.json()
        };
        (reply.status, json)
    }

    fn reached(&self) -> Vec<String> {
        self.calls
            .lock()
            .expect("calls")
            .iter()
            .map(|(verb, _)| verb.clone())
            .collect()
    }
}

/// The refusal of a limited token: a JSON-RPC error with its code, that never names a service of the call.
fn assert_outside(reply: &Value, says: &str) {
    let error = &reply["error"];
    assert_eq!(error["data"]["refusal"], json!(OUTSIDE_SERVICES), "{reply}");
    let message = error["message"].as_str().expect("a message");
    assert!(message.contains(says), "{message}");
    for name in ["pg-secret", "cx"] {
        assert!(!message.contains(name), "{message}");
    }
}

#[test]
fn a_limited_token_is_refused_a_service_outside_its_list_before_any_handler_runs() {
    let door = Door::start("door-service");
    let (status, reply) = door.call(&door.viewer, "service_status", json!({"name": "pg-secret"}));
    assert_eq!(status, 200);
    assert_outside(&reply, "not on the token's list");
    let (_, reply) = door.call(&door.viewer, "service_logs", json!({"name": "cx"}));
    assert_outside(&reply, "not on the token's list");
    let (_, reply) = door.call(
        &door.viewer,
        "service_status",
        json!({"name": "dev", "tenant": "abcdefgh"}),
    );
    assert_outside(&reply, "not on the token's list");
    assert!(
        door.reached().is_empty(),
        "no handler ran: {:?}",
        door.reached()
    );
}

#[test]
fn a_limited_token_reads_a_listed_service_with_its_links_cut_to_the_list() {
    let door = Door::start("door-status");
    let (_, reply) = door.call(&door.viewer, "service_status", json!({"name": "dev"}));
    let view = &reply["result"]["structuredContent"];
    assert_eq!(view["name"], json!("dev"));
    assert_eq!(view["links"], json!([{"to": "gtm", "port": 4222}]));
    assert!(!reply.to_string().contains("pg-secret"), "{reply}");
    let (_, reply) = door.call(&door.viewer, "service_logs", json!({"name": "dev"}));
    assert_eq!(
        reply["result"]["structuredContent"]["lines"],
        json!(["dev started"])
    );
    // A token with no limit sees the links whole.
    let (_, reply) = door.call(&door.open, "service_status", json!({"name": "dev"}));
    assert!(reply.to_string().contains("pg-secret"));
}

#[test]
fn a_limited_token_lists_only_its_services_snapshots_and_instances() {
    let door = Door::start("door-lists");
    let (_, reply) = door.call(&door.viewer, "service_list", json!({}));
    let rows = &reply["result"]["structuredContent"]["services"];
    let names: Vec<&str> = rows
        .as_array()
        .expect("rows")
        .iter()
        .map(|row| row["name"].as_str().expect("a name"))
        .collect();
    assert_eq!(names, ["dev", "gtm"]);
    assert_eq!(rows[0]["links"], json!([{"to": "gtm", "port": 4222}]));

    let (_, reply) = door.call(&door.viewer, "snapshot_list", json!({}));
    assert_eq!(
        reply["result"]["structuredContent"]["snapshots"],
        json!([snapshot("arcak-svc", "dev", "container")])
    );

    let (_, reply) = door.call(&door.viewer, "watch_report", json!({}));
    let report = &reply["result"]["structuredContent"];
    assert_eq!(
        report["projects"],
        json!([{"project": "arcak-svc", "instances": [instance("dev")]}])
    );
    assert_eq!(report["drift"], json!([]));

    for verb in ["service_list", "snapshot_list", "watch_report"] {
        let (_, reply) = door.call(&door.viewer, verb, json!({}));
        // The answer itself and its text: the door's own meta keys name droidd, which is no project.
        let text = format!(
            "{} {}",
            reply["result"]["structuredContent"], reply["result"]["content"]
        );
        for leak in ["pg-secret", "\"droid\"", "droid-secret", "phone1"] {
            assert!(!text.contains(leak), "{verb} leaks {leak}: {text}");
        }
    }
    // A token with no limit reads every row.
    let (_, reply) = door.call(&door.open, "service_list", json!({}));
    assert_eq!(
        reply["result"]["structuredContent"]["services"]
            .as_array()
            .map(Vec::len),
        Some(3)
    );
}

#[test]
fn a_limited_token_is_refused_every_node_wide_verb_and_is_not_offered_one() {
    let door = Door::start("door-node");
    let (_, reply) = door.call(&door.viewer, "arca_status", json!({}));
    assert_outside(&reply, "whole node");
    let (_, reply) = door.call(&door.doctor, "tenant_list", json!({}));
    assert_outside(&reply, "whole node");
    assert!(door.reached().is_empty());

    let body = rpc(json!(1), "tools/list", json!({}));
    let names = |token: &str| -> Vec<String> {
        let reply = post(&mut connect_unix(&door.socket), &[bearer(token)], &body);
        reply.json()["result"]["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .map(|tool| tool["name"].as_str().expect("name").to_string())
            .collect()
    };
    let limited = names(&door.viewer);
    for offered in [
        "service_list",
        "service_status",
        "service_logs",
        "snapshot_list",
        "watch_report",
        "action_propose",
    ] {
        assert!(
            limited.iter().any(|n| n == offered),
            "{offered}: {limited:?}"
        );
    }
    for hidden in [
        "arca_status",
        "journal_list",
        "tenant_list",
        "plan_list",
        "node_metrics",
    ] {
        assert!(
            !limited.iter().any(|n| n == hidden),
            "{hidden}: {limited:?}"
        );
    }
    assert!(names(&door.open).iter().any(|n| n == "arca_status"));
}

#[test]
fn a_filtered_read_with_no_cut_answers_a_limited_token_nothing() {
    let door = Door::start("door-uncut");
    let (_, reply) = door.call(&door.viewer, "uncut_probe", json!({}));
    assert_eq!(reply["result"]["isError"], json!(true), "{reply}");
    assert!(!reply.to_string().contains("everything"), "{reply}");
    let (_, reply) = door.call(&door.open, "uncut_probe", json!({}));
    assert_eq!(
        reply["result"]["structuredContent"]["everything"],
        json!(true)
    );
}

#[test]
fn a_limited_doctor_acts_on_listed_services_and_instances_only() {
    let door = Door::start("door-acts");
    let (_, reply) = door.call(
        &door.doctor,
        "service_connect",
        json!({"from": "dev", "to": "pg-secret"}),
    );
    assert_outside(&reply, "not on the token's list");
    let (_, reply) = door.call(
        &door.doctor,
        "instance_start",
        json!({"project": "arcak-svc", "instance": "pg-secret"}),
    );
    assert_outside(&reply, "not a listed service's");
    let (_, reply) = door.call(
        &door.doctor,
        "instance_start",
        json!({"project": "droid", "instance": "dev"}),
    );
    assert_outside(&reply, "not a listed service's");
    assert!(door.reached().is_empty());
    let (_, reply) = door.call(
        &door.doctor,
        "instance_start",
        json!({"project": "arcak-svc", "instance": "dev"}),
    );
    assert!(reply.get("error").is_none(), "{reply}");
    assert_eq!(door.reached(), ["instance_start"]);
}

#[test]
fn a_proposal_is_checked_against_the_proposers_list_when_it_is_made() {
    let door = Door::start("door-propose");
    let propose = |action: &str, args: Value| {
        door.call(
            &door.viewer,
            "action_propose",
            json!({"action": action, "args": args}),
        )
    };
    let (_, reply) = propose("service_remove", json!({"name": "pg-secret"}));
    assert_outside(&reply, "action_propose: service_remove");
    let (_, reply) = propose(
        "instance_stop",
        json!({"project": "arcak-svc", "instance": "pg-secret"}),
    );
    assert_outside(&reply, "not a listed service's");
    let (_, reply) = propose("host_network_restore", json!({}));
    assert_outside(&reply, "whole node");
    assert!(door.reached().is_empty(), "{:?}", door.reached());
    let (_, reply) = propose("service_remove", json!({"name": "dev"}));
    assert_eq!(
        reply["error"]["data"]["refusal"],
        json!("Reached"),
        "{reply}"
    );
    assert_eq!(door.reached(), ["action_propose"]);
}

#[test]
fn a_limited_token_is_never_relayed_to_a_linked_node() {
    let door = Door::start("door-relay");
    let (_, reply) = door.call(&door.viewer, "service_list", json!({"node": "peer"}));
    assert_outside(&reply, "linked node");
    let (_, reply) = door.call(
        &door.viewer,
        "service_status",
        json!({"name": "dev", "node": "peer"}),
    );
    assert_outside(&reply, "linked node");
    assert!(door.reached().is_empty(), "{:?}", door.reached());
    // A token with no limit is relayed as ever.
    let (_, reply) = door.call(&door.open, "service_list", json!({"node": "peer"}));
    assert!(reply.get("error").is_none(), "{reply}");
    assert_eq!(door.reached(), ["relay:peer"]);
}

#[test]
fn a_limited_token_is_refused_every_get_route() {
    let door = Door::start("door-get");
    let get = |token: &str| {
        let request = format!(
            "GET /everything HTTP/1.1\r\nHost: arca\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
        );
        exchange(&mut connect_unix(&door.socket), request.as_bytes())
    };
    let refused = get(&door.viewer);
    assert_eq!(refused.status, 403);
    assert!(!refused.body.contains("every service"));
    assert_eq!(get(&door.open).status, 200);
}

/// A POST route that records each path it is asked for, so a request that reaches it shows.
struct Ack(Arc<Mutex<Vec<String>>>);

impl arca_mcp::PostHandler for Ack {
    fn post(&self, path: &str, _caller: &Caller) -> Result<GetReply, CallError> {
        self.0.lock().expect("the paths").push(path.to_string());
        Ok(GetReply {
            content_type: "application/json",
            body: b"{}".to_vec(),
        })
    }
}

/// A POST route's verb reaches the node's own feed, never one service, so a token limited to named services is refused
/// it however its scope or list admits the verb: an operator, and a viewer whose list names it. An unlimited operator still
/// reaches it.
#[test]
fn a_limited_token_is_refused_every_post_route() {
    let scratch = Scratch::new("door-post");
    let dev_only = limits(Some("dev"), None, None);
    let operator = mint_limited_into(&scratch, "limited-op", Scope::Operator, None, &dev_only);
    let named = mint_limited_into(
        &scratch,
        "limited-named",
        Scope::Viewer,
        Some(&["support_report_ack"]),
        &dev_only,
    );
    let open = mint_limited_into(
        &scratch,
        "open-op",
        Scope::Operator,
        None,
        &Limits::default(),
    );
    let tokens = TokenStore::load(scratch.path()).expect("loads");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let server = Server::new(verbs(), tokens)
        .log_to(Arc::new(|_| {}))
        .post_route(
            "/reports/{id}/ack",
            "support_report_ack",
            Arc::new(Ack(Arc::clone(&seen))),
        );
    let rt = Runtime::new().expect("runtime");
    let socket = scratch.at("arca.sock");
    let (router, served) = (server.router(), socket.clone());
    rt.spawn(async move { serve_unix(router, &served).await });
    wait_for_socket(&socket);
    let ack = |token: &str| {
        let request = format!(
            "POST /reports/1/ack HTTP/1.1\r\nHost: arca\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        exchange(&mut connect_unix(&socket), request.as_bytes())
    };
    for (id, token) in [("limited-op", &operator), ("limited-named", &named)] {
        let reply = ack(token);
        assert_eq!(reply.status, 403, "{id}");
        assert!(reply.body.is_empty(), "{id}: the refusal says nothing");
    }
    assert!(
        seen.lock().expect("the paths").is_empty(),
        "no refused request reached the handler"
    );
    assert_eq!(ack(&open).status, 200);
    assert_eq!(seen.lock().expect("the paths").len(), 1);
    drop(rt);
}

#[test]
fn an_expired_token_is_refused_at_the_door() {
    let door = Door::start("door-expired");
    let (status, _) = door.call(&door.expired, "service_status", json!({"name": "dev"}));
    assert_eq!(status, 401);
    assert!(door.reached().is_empty());
}
