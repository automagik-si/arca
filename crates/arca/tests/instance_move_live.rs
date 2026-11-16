//! A move between two real Incus daemons, run by hand: the same verbs, card and grant as `instance_move.rs`, but the
//! source and the target are live Incus sockets, not fakes. It is ignored by default; run it on the source host, as root
//! (it reads Incus's socket and walks its storage), with:
//!
//! ```text
//! ARCA_MOVE_SOURCE_SOCKET=/var/lib/incus/unix.socket \
//! ARCA_MOVE_TARGET_SOCKET=/run/node-b-incus.sock \
//! ARCA_MOVE_STORAGE_ROOT=/var/lib/incus \
//! ARCA_MOVE_INSTANCE=default/web \
//! ARCA_MOVE_EXPECT=moved \
//!   instance_move_live --ignored --nocapture
//! ```
//!
//! `ARCA_MOVE_TARGET_SOCKET` is the target's Incus socket as this host reaches it (the linked host's `incusSocket`).
//! Optional: `ARCA_MOVE_ROUNDS` (1 to 5), `ARCA_MOVE_SPOT_CHECK` (a file path in the container) and `ARCA_MOVE_EXPECT`
//! (`moved`, the default, or `rolled_back`). It prints the plan, the progress each second, the final status and the
//! move's journal lines, and asserts the instance's result is the expected one.

mod common;

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arca::config::NodeConfig;
use arca::handlers::{ActionExecutor, ActionSlot, Clock, plan_handlers_served};
use arca::instance_move::{Moves, handlers as move_handlers};
use arca::kernel::Kernel;
use arca_api::Scope;
use arca_incus::{FakeIncus, FakeState};
use arca_kernel::canon::Digest;
use arca_kernel::identity::NodeKey;
use arca_mcp::{Caller, ToolHandler};
use arca_providers::incus::{Family, IncusProvider};
use arca_reconcile::PluginSet;
use common::{HOST, NOW, Scratch, SkKey, approve_statement, verifier_for};
use serde_json::{Value, json};

fn env(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| panic!("{key} is not set"))
}

#[test]
#[ignore = "needs two live Incus daemons; see the module documentation"]
fn a_move_between_two_live_incus_daemons() {
    let source = PathBuf::from(env("ARCA_MOVE_SOURCE_SOCKET"));
    let target = PathBuf::from(env("ARCA_MOVE_TARGET_SOCKET"));
    let storage = PathBuf::from(env("ARCA_MOVE_STORAGE_ROOT"));
    let instance = env("ARCA_MOVE_INSTANCE");
    let (project, name) = instance
        .split_once('/')
        .expect("ARCA_MOVE_INSTANCE is project/name");
    let expect = std::env::var("ARCA_MOVE_EXPECT").unwrap_or_else(|_| "moved".to_string());
    let rounds: u64 = std::env::var("ARCA_MOVE_ROUNDS")
        .ok()
        .and_then(|rounds| rounds.parse().ok())
        .unwrap_or(2);
    let mut wanted = json!({ "project": project, "name": name });
    if let Ok(path) = std::env::var("ARCA_MOVE_SPOT_CHECK") {
        wanted["spotCheck"] = json!(path);
    }

    let dir = std::env::temp_dir().join(format!("arca-move-live-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("state")).expect("state");
    fs::create_dir_all(dir.join("kernel")).expect("kernel");
    // The kernel's own provider is a fake: this run moves instances, it reconciles nothing.
    let kernel_fake = FakeIncus::start(
        &dir.join("kernel"),
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![],
            networks: vec![],
            pools: vec![],
        },
    )
    .expect("kernel fake");
    let cfg: NodeConfig = serde_json::from_value(json!({
        "stateDir": dir.join("state"),
        "incus": { "socket": source },
        "listen": { "unix": dir.join("arca.sock") },
        "tokens": { "hashDir": dir.join("tokens") },
        "kernel": { "projects": ["arcak-svc"], "droiddProjects": [] },
        "approval": { "linkedHosts": [{
            "name": "node-b",
            "socket": dir.join("node-b-arca.sock"),
            "tokenFile": dir.join("node-b.token"),
            "incusSocket": target,
        }] },
    }))
    .expect("config");
    let cfg = Arc::new(cfg);
    let scratch = Scratch::new("instance-move-live");
    let key = SkKey::from_seed(11);
    let provider_socket = kernel_fake.socket().to_path_buf();
    let secrets = dir.join("secrets");
    let kernel = Arc::new(Mutex::new(
        Kernel::open(
            &dir.join("state"),
            NodeKey::load_or_create(&dir.join("node.key")).expect("key"),
            PluginSet::new(),
            verifier_for(&scratch, &[&key], "instance-move-live"),
            HOST.into(),
            None,
            move |retained| {
                Ok(Box::new(
                    IncusProvider::new(
                        provider_socket,
                        Family {
                            projects: vec!["arcak-svc".into()],
                            scratch: false,
                            droidd_projects: vec![],
                            reserved_cidrs: vec![],
                        },
                        secrets,
                        retained,
                    )
                    .expect("provider"),
                ))
            },
            NOW,
        )
        .expect("kernel"),
    ));
    let clock: Clock = Arc::new(|| NOW);
    let moves = Arc::new(
        Moves::new(Arc::clone(&cfg), Arc::clone(&kernel), Arc::clone(&clock))
            .with_storage_root(storage)
            .with_timing(Duration::from_millis(500), Duration::from_secs(60)),
    );
    let handlers: HashMap<&'static str, Arc<dyn ToolHandler>> = plan_handlers_served(
        Arc::clone(&kernel),
        clock,
        None,
        None,
        None,
        None,
        ActionSlot::default(),
        Some(cfg),
    )
    .into_iter()
    .chain(move_handlers(Arc::clone(&moves)))
    .collect();
    let executor = ActionExecutor::new(
        handlers
            .iter()
            .map(|(verb, handler)| (*verb, Arc::clone(handler))),
    );
    let call = |verb: &str, args: Value| {
        handlers[verb].call(
            args,
            &Caller {
                token_id: "live".to_string(),
                scope: Scope::Viewer,
                tools: None,
                limits: Default::default(),
            },
        )
    };

    let plan = call(
        "instance_move_plan",
        json!({ "instances": [wanted], "target": "node-b", "rounds": rounds }),
    )
    .expect("the plan answers");
    println!(
        "PLAN {}",
        serde_json::to_string_pretty(&json!({
            "go": plan["go"], "digest": plan["digest"], "blocked": plan["blocked"],
            "transport": plan["plan"]["transport"], "instances": plan["plan"]["instances"],
        }))
        .unwrap()
    );
    assert_eq!(plan["go"], true, "the plan is not a go");
    let proposal = call(
        "action_propose",
        json!({ "action": "instance_move", "args": {
            "plan": plan["digest"], "instances": [{ "project": project, "name": name }], "target": "node-b",
        } }),
    )
    .expect("proposed");
    println!(
        "CARD {}",
        serde_json::to_string_pretty(&proposal["details"]).unwrap()
    );
    let id = proposal["id"].as_str().expect("an id").to_string();
    let statement = arca::handlers::action_statement(
        HOST,
        &id,
        "instance_move",
        proposal["argsDigest"].as_str().expect("a digest"),
        NOW,
        &Digest::of(id.as_bytes()).to_hex()[..32],
    )
    .expect("the statement");
    let started = arca::handlers::run_approved_action(
        &kernel,
        &executor,
        &json!({ "plan": id, "approval": approve_statement(&key, &statement) }),
        NOW,
    )
    .expect("approved and started");
    let id = started["result"]["move"]
        .as_str()
        .expect("a move")
        .to_string();
    println!("STARTED {id}");

    let until = Instant::now() + Duration::from_secs(60 * 60);
    let status = loop {
        let status = call("instance_move_status", json!({ "move": id })).expect("status")["moves"]
            [0]
        .clone();
        let one = &status["instances"][0];
        println!(
            "PROGRESS phase={} round={}/{} bytesMoved={} rate={} timeLeftMs={}",
            one["phase"],
            one["roundsDone"],
            one["roundsTotal"],
            one["bytesMoved"],
            one["rateBytesPerSecond"],
            one["timeLeftMs"]
        );
        if status["endedAt"].is_u64() {
            break status;
        }
        assert!(Instant::now() < until, "the move did not end in an hour");
        std::thread::sleep(Duration::from_secs(1));
    };
    println!("STATUS {}", serde_json::to_string_pretty(&status).unwrap());
    for line in moves.journal().lines() {
        if line["move"] == id && line["event"] != "plan" {
            println!("JOURNAL {line}");
        }
    }
    assert_eq!(
        status["instances"][0]["result"],
        expect.as_str(),
        "{status}"
    );
    drop(kernel_fake);
    let _ = fs::remove_dir_all(&dir);
}
