//! Moving containers to a linked host: `instance_move_plan`, the owner's one `instance_move` card, the run in rounds with
//! its cutover, checks and rollback, `instance_move_status` and `instance_move_finish`. Two fake Incus daemons stand for
//! this node (the source) and a linked host (the target); the source's pushes land on the target as a daemon on the
//! network would take them. Nothing here touches a live host, and every name and address is made up: node-b, the
//! documentation ranges only.

mod common;

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arca::config::NodeConfig;
use arca::handlers::{ActionExecutor, ActionSlot, Clock, plan_handlers_served};
use arca::instance_move::{Moves, handlers as move_handlers};
use arca::kernel::Kernel;
use arca_api::{Scope, conforms, verb};
use arca_incus::{FakeIncus, FakeState, PushPlan};
use arca_kernel::canon::Digest;
use arca_kernel::identity::NodeKey;
use arca_mcp::{CallError, Caller, ToolHandler};
use arca_providers::incus::{Family, IncusProvider};
use arca_reconcile::PluginSet;
use common::{HOST, NOW, Scratch, SkKey, approve_statement, verifier_for};
use serde_json::{Value, json};

static SEQ: AtomicU64 = AtomicU64::new(0);
const MIB: u64 = 1024 * 1024;
const FLAGS: &[&str] = &[
    "fpu", "sse", "sse2", "cx16", "popcnt", "sse4_1", "sse4_2", "ssse3", "avx", "avx2",
];
const CERT: &str = "-----BEGIN CERTIFICATE-----\nZmFrZSB0YXJnZXQ=\n-----END CERTIFICATE-----\n";

/// A container of the project `shop` on pool `default`, with a fixed address on `arcabr0`.
fn container(name: &str, mac: &str, address: &str) -> Value {
    json!({
        "name": name, "project": "shop", "type": "container", "status": "Running", "architecture": "x86_64",
        "profiles": ["default"],
        "config": {
            "volatile.eth0.hwaddr": mac,
            "volatile.uuid": format!("00000000-0000-4000-8000-0000000000{}", &mac[mac.len() - 2..]),
        },
        "devices": {
            "root": { "type": "disk", "path": "/", "pool": "default" },
            "eth0": { "type": "nic", "network": "arcabr0", "ipv4.address": address }
        }
    })
}

/// A fake host: `instances` and a `default` pool of `pool_mib` MiB; the target listens on the network.
fn host(dir: &Path, instances: Vec<Value>, pool_mib: u64, target: bool) -> FakeIncus {
    fs::create_dir_all(dir).expect("dir");
    let mut environment =
        json!({ "architectures": ["x86_64"], "driver": "lxc | qemu", "server_name": "fake" });
    if target {
        environment["addresses"] = json!(["203.0.113.20:8443"]);
        environment["certificate"] = json!(CERT);
    }
    let fake = FakeIncus::start(
        dir,
        FakeState {
            server: json!({ "api_version": "1.0", "environment": environment }),
            projects: vec![
                json!({ "name": "default", "config": {} }),
                json!({ "name": "shop", "config": { "features.profiles": "true" } }),
            ],
            instances,
            pools: vec![(
                "default".to_string(),
                json!({ "space": { "used": 0, "total": pool_mib * MIB } }),
            )],
            networks: vec![json!({
                "name": "arcabr0", "project": "default", "type": "bridge", "managed": true,
                "config": { "ipv4.address": "192.0.2.1/24" }
            })],
        },
    )
    .expect("fake incus");
    fake.set_pool_driver("default", "btrfs");
    fake.set_resources(json!({
        "cpu": { "architecture": "x86_64", "sockets": [{ "socket": 0, "cores": [{ "core": 0, "flags": FLAGS }] }] },
        "gpu": { "cards": [] }, "usb": { "devices": [] }, "pci": { "devices": [] }
    }));
    let address: IpAddr = if target {
        "198.51.100.20"
    } else {
        "198.51.100.10"
    }
    .parse()
    .expect("ip");
    fake.set_host_interfaces(vec![("enp6s0".to_string(), vec![address])]);
    fake.add_profile(
        "shop",
        json!({ "name": "default", "description": "", "config": {}, "devices": {} }),
    );
    fake
}

struct Rig {
    dir: PathBuf,
    source: FakeIncus,
    target: FakeIncus,
    _kernel_fake: FakeIncus,
    _scratch: Scratch,
    kernel: Arc<Mutex<Kernel>>,
    key: SkKey,
    now: Arc<AtomicU64>,
    moves: Arc<Moves>,
    handlers: HashMap<&'static str, Arc<dyn ToolHandler>>,
    executor: ActionExecutor,
    nonce: AtomicU64,
}

impl Rig {
    /// This node holds `instances`; node-b, its linked host, a pool of `target_mib` MiB.
    fn new(instances: Vec<Value>, target_mib: u64) -> Rig {
        let dir = std::env::temp_dir().join(format!(
            "arca-instance-move-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("state")).expect("state");
        let source = host(&dir.join("source"), instances, 100, false);
        let target = host(&dir.join("target"), Vec::new(), target_mib, true);
        source.link_migration(&target);
        fs::create_dir_all(dir.join("kernel")).expect("kernel dir");
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
            "incus": { "socket": source.socket() },
            "listen": { "unix": dir.join("arca.sock") },
            "tokens": { "hashDir": dir.join("tokens") },
            "kernel": { "projects": ["arcak-svc"], "droiddProjects": [] },
            "approval": { "linkedHosts": [{
                "name": "node-b",
                "socket": dir.join("node-b-arca.sock"),
                "tokenFile": dir.join("node-b.token"),
                "incusSocket": target.socket(),
            }] },
        }))
        .expect("config");
        let cfg = Arc::new(cfg);
        let scratch = Scratch::new("instance-move-owner");
        let key = SkKey::from_seed(11);
        let provider_socket = kernel_fake.socket().to_path_buf();
        let secrets = dir.join("secrets");
        let kernel = Arc::new(Mutex::new(
            Kernel::open(
                &dir.join("state"),
                NodeKey::load_or_create(&dir.join("node.key")).expect("key"),
                PluginSet::new(),
                verifier_for(&scratch, &[&key], "instance-move"),
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
        let now = Arc::new(AtomicU64::new(NOW));
        let ticking = Arc::clone(&now);
        let clock: Clock = Arc::new(move || ticking.load(Ordering::SeqCst));
        let moves = Arc::new(
            Moves::new(Arc::clone(&cfg), Arc::clone(&kernel), Arc::clone(&clock))
                .with_storage_root(dir.join("source"))
                .with_timing(Duration::from_millis(20), Duration::from_millis(300)),
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
        let rig = Rig {
            dir,
            source,
            target,
            _kernel_fake: kernel_fake,
            _scratch: scratch,
            kernel,
            key,
            now,
            moves,
            handlers,
            executor,
            nonce: AtomicU64::new(0),
        };
        rig.files("containers/shop_web/rootfs", 2);
        rig.files("containers/shop_api/rootfs", 1);
        rig
    }

    /// Writes `mib` MiB of files under the source's `storage-pools/default/<at>`.
    fn files(&self, at: &str, mib: u64) {
        let folder = self
            .dir
            .join("source/storage-pools/default")
            .join(at)
            .join("srv");
        fs::create_dir_all(&folder).expect("folder");
        for index in 0..mib {
            fs::write(
                folder.join(format!("blob-{index}")),
                vec![7u8; MIB as usize],
            )
            .expect("blob");
        }
    }

    fn call_as(&self, scope: Scope, name: &str, args: Value) -> Result<Value, CallError> {
        let result = self.handlers[name].call(
            args,
            &Caller {
                token_id: "doctor-1".to_string(),
                scope,
                tools: None,
            },
        );
        if let Ok(value) = &result {
            conforms(&(verb(name).expect("in the table").result)(), value).unwrap_or_else(|why| {
                panic!("{name} result does not meet its schema: {why}\n{value}")
            });
        }
        result
    }

    fn plan(&self, names: &[&str]) -> Value {
        self.plan_with(names, json!({}))
    }

    fn plan_with(&self, names: &[&str], extra: Value) -> Value {
        let mut args = json!({
            "instances": names.iter().map(|name| json!({ "project": "shop", "name": name })).collect::<Vec<_>>(),
            "target": "node-b",
        });
        for (key, value) in extra.as_object().into_iter().flatten() {
            args[key] = value.clone();
        }
        self.call_as(Scope::Viewer, "instance_move_plan", args)
            .expect("the plan answers")
    }

    fn propose_move(&self, plan: &Value) -> Result<Value, CallError> {
        let instances: Vec<Value> = plan["plan"]["instances"]
            .as_array()
            .expect("instances")
            .iter()
            .map(|one| json!({ "project": one["project"], "name": one["name"] }))
            .collect();
        self.call_as(
            Scope::Viewer,
            "action_propose",
            json!({ "action": "instance_move", "args": {
                "plan": plan["digest"], "instances": instances, "target": "node-b",
            } }),
        )
    }

    fn propose_finish(&self, id: &str) -> Result<Value, CallError> {
        self.call_as(
            Scope::Viewer,
            "action_propose",
            json!({ "action": "instance_move_finish", "args": { "move": id } }),
        )
    }

    fn approve(&self, proposal: &Value) -> Result<Value, CallError> {
        let id = proposal["id"].as_str().expect("an id").to_string();
        let nonce = self.nonce.fetch_add(1, Ordering::SeqCst);
        let statement = arca::handlers::action_statement(
            HOST,
            &id,
            proposal["action"].as_str().expect("an action"),
            proposal["argsDigest"].as_str().expect("a digest"),
            NOW,
            &Digest::of(format!("{id}{nonce}").as_bytes()).to_hex()[..32],
        )
        .expect("the statement");
        let args = json!({ "plan": id, "approval": approve_statement(&self.key, &statement) });
        arca::handlers::run_approved_action(&self.kernel, &self.executor, &args, NOW)
            .map(|answer| answer["result"].clone())
    }

    /// Plans, proposes and approves a move of `names`; answers the run's first answer.
    fn start(&self, names: &[&str]) -> Value {
        let plan = self.plan(names);
        assert_eq!(plan["go"], true, "{plan}");
        let proposal = self.propose_move(&plan).expect("proposed");
        self.approve(&proposal).expect("approved and started")
    }

    fn status(&self, id: &str) -> Value {
        let answer = self
            .call_as(Scope::Viewer, "instance_move_status", json!({ "move": id }))
            .expect("status");
        answer["moves"][0].clone()
    }

    /// Waits for the move to end, and answers its status.
    fn wait(&self, id: &str) -> Value {
        let until = Instant::now() + Duration::from_secs(30);
        loop {
            let status = self.status(id);
            if status["endedAt"].is_u64() {
                return status;
            }
            assert!(Instant::now() < until, "the move did not end: {status}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn journal(&self) -> Vec<Value> {
        self.moves.journal().lines()
    }

    fn events(&self, id: &str) -> Vec<String> {
        self.journal()
            .iter()
            .filter(|line| line["move"] == id)
            .map(|line| line["event"].as_str().unwrap_or_default().to_string())
            .collect()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn refusal_code(result: Result<Value, CallError>) -> String {
    match result {
        Err(CallError::Refused { code, .. }) => code,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn web() -> Value {
    container("web", "00:16:3e:00:00:01", "192.0.2.50")
}

fn api() -> Value {
    container("api", "00:16:3e:00:00:02", "192.0.2.51")
}

/// `web` with the custom volume `web-data` mounted at /srv/data.
fn web_with_volume() -> Value {
    let mut web = web();
    web["devices"]["data"] =
        json!({ "type": "disk", "pool": "default", "source": "web-data", "path": "/srv/data" });
    web
}

#[test]
fn a_passing_plan_is_frozen_with_its_digest_and_a_changed_source_gives_another() {
    let rig = Rig::new(vec![web_with_volume()], 100);
    rig.source
        .add_volume("default", "default", "web-data", json!({}));
    rig.files("custom/default_web-data", 1);
    let answer = rig.plan(&["web"]);
    assert_eq!(answer["go"], true, "{answer}");
    let digest = answer["digest"].as_str().expect("a digest");
    assert_eq!(digest.len(), 64);
    let plan = &answer["plan"];
    assert_eq!(plan["digest"], digest);
    assert_eq!(answer["expiresAt"], json!(NOW + 60 * 60 * 1000));
    assert_eq!(plan["target"]["name"], "node-b");
    assert_eq!(plan["transport"]["mode"], "push");
    assert_eq!(plan["transport"]["address"], "203.0.113.20:8443");
    assert_eq!(plan["transport"]["fallback"], "relay");
    assert_eq!(
        plan["transport"]["certificateFingerprint"],
        Digest::of(CERT.as_bytes()).to_hex()
    );
    assert!(plan["credential"].as_str().unwrap().contains("incusSocket"));
    let one = &plan["instances"][0];
    assert_eq!(one["project"], "shop");
    assert_eq!(one["name"], "web");
    assert!(one["uuid"].is_string(), "{one}");
    assert_eq!(one["bytes"]["instance"], json!(2 * MIB));
    assert_eq!(one["bytes"]["volumes"], json!(MIB));
    assert_eq!(one["bytes"]["measured"], json!(3 * MIB));
    assert!(one["bytes"]["needed"].as_u64().unwrap() > 3 * MIB);
    assert_eq!(one["rounds"]["preCopy"], 2);
    assert_eq!(one["volumes"][0]["name"], "web-data");
    assert_eq!(one["cutover"][0], "stop the source");
    assert!(one["verify"].as_array().unwrap().len() >= 4);
    // The plan is in the move journal; nothing was written on either host.
    assert!(
        rig.journal()
            .iter()
            .any(|line| line["event"] == "plan" && line["plan"]["digest"] == digest)
    );
    assert!(rig.source.writes().is_empty() && rig.target.writes().is_empty());

    // The same source plans to the same digest; a changed uuid, a grown size or a new device give another.
    assert_eq!(rig.plan(&["web"])["digest"], digest);
    rig.files("containers/shop_web/rootfs/more", 1);
    let grown = rig.plan(&["web"]);
    assert_ne!(grown["digest"], digest);
    rig.source.set_instance_config(
        "shop",
        "web",
        "volatile.uuid",
        "00000000-0000-4000-8000-00000000beef",
    );
    let other = rig.plan(&["web"]);
    assert_ne!(other["digest"], digest);
    assert_ne!(other["digest"], grown["digest"]);
}

#[test]
fn a_no_go_blocks_the_plan_with_the_preflight_reasons() {
    let rig = Rig::new(vec![web()], 1);
    let answer = rig.plan(&["web"]);
    assert_eq!(answer["go"], false);
    assert_eq!(answer["digest"], Value::Null);
    assert_eq!(answer["plan"], Value::Null);
    let reasons = answer["blocked"][0]["reasons"].to_string();
    assert!(reasons.contains("size:"), "{reasons}");
    assert_eq!(answer["preflight"]["rows"][0]["go"], false);
    assert!(!rig.journal().iter().any(|line| line["event"] == "plan"));
}

#[test]
fn a_virtual_machine_is_not_movable_yet() {
    let mut vm = web();
    vm["type"] = json!("virtual-machine");
    let rig = Rig::new(vec![vm], 100);
    let answer = rig.plan(&["web"]);
    assert_eq!(answer["go"], false);
    let first = answer["blocked"][0]["reasons"][0].as_str().unwrap();
    assert!(
        first.starts_with("virtual machines are not movable yet"),
        "{first}"
    );
}

#[test]
fn the_card_binds_the_digest_and_a_changed_or_altered_plan_moves_nothing() {
    let rig = Rig::new(vec![web()], 100);
    let plan = rig.plan(&["web"]);
    let proposal = rig.propose_move(&plan).expect("proposed");
    let rows: Vec<(String, String)> = proposal["details"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["label"].as_str().unwrap().to_string(),
                row["value"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        rows[0],
        (
            "Plan".to_string(),
            plan["digest"].as_str().unwrap().to_string()
        )
    );
    assert_eq!(rows[1], ("Target".to_string(), "node-b".to_string()));
    assert_eq!(rows[2].0, "Move shop/web");
    assert!(rows[2].1.contains("downtime about"), "{rows:?}");
    for label in ["Transport", "Credential", "Rollback", "Source"] {
        assert!(rows.iter().any(|row| row.0 == label), "{label}: {rows:?}");
    }
    // A card for instances that are not the plan's, or a digest the node never planned, is not proposed.
    let altered = rig.call_as(
        Scope::Viewer,
        "action_propose",
        json!({ "action": "instance_move", "args": {
            "plan": plan["digest"], "instances": [{ "project": "shop", "name": "api" }], "target": "node-b",
        } }),
    );
    assert_eq!(refusal_code(altered), "PlanMismatch");
    let unknown = rig.call_as(
        Scope::Viewer,
        "action_propose",
        json!({ "action": "instance_move", "args": {
            "plan": "0".repeat(64), "instances": [{ "project": "shop", "name": "web" }], "target": "node-b",
        } }),
    );
    assert_eq!(refusal_code(unknown), "NoSuchPlan");
    // The source changes after the card: the approved run refuses it, and nothing moves.
    rig.source.set_instance_config(
        "shop",
        "web",
        "volatile.uuid",
        "00000000-0000-4000-8000-00000000beef",
    );
    let refused = rig.approve(&proposal);
    match refused {
        Err(CallError::Refused { code, message }) => {
            assert_eq!(code, "PlanChanged");
            assert!(message.contains("its uuid"), "{message}");
        }
        other => panic!("expected PlanChanged, got {other:?}"),
    }
    assert!(rig.source.pushes().is_empty());
    assert!(!rig.target.holds("instance", "shop", "web"));
    assert_eq!(
        rig.source.instance_status("shop", "web").as_deref(),
        Some("Running")
    );
}

#[test]
fn an_expired_plan_is_not_run() {
    let rig = Rig::new(vec![web()], 100);
    let plan = rig.plan(&["web"]);
    let proposal = rig.propose_move(&plan).expect("proposed");
    rig.now.store(NOW + 2 * 60 * 60 * 1000, Ordering::SeqCst);
    assert_eq!(refusal_code(rig.approve(&proposal)), "PlanExpired");
    assert!(rig.source.pushes().is_empty());
}

#[test]
fn the_move_runs_only_from_an_approved_card_and_each_grant_once() {
    let rig = Rig::new(vec![web()], 100);
    let plan = rig.plan(&["web"]);
    let args = json!({
        "plan": plan["digest"], "instances": [{ "project": "shop", "name": "web" }], "target": "node-b",
    });
    let direct = rig.call_as(Scope::Doctor, "instance_move", args.clone());
    assert!(matches!(direct, Err(CallError::Invalid(_))), "{direct:?}");
    let mut forged = args.clone();
    forged["approval"] = json!("forged");
    assert_eq!(
        refusal_code(rig.call_as(Scope::Doctor, "instance_move", forged.clone())),
        "NotApproved"
    );
    assert!(matches!(
        rig.call_as(Scope::Viewer, "instance_move", forged),
        Err(CallError::Forbidden { .. })
    ));
    assert!(rig.source.pushes().is_empty());
    let proposal = rig.propose_move(&plan).expect("proposed");
    let started = rig.approve(&proposal).expect("runs");
    let id = started["move"].as_str().unwrap().to_string();
    assert!(
        rig.approve(&proposal).is_err(),
        "a replayed approval is refused"
    );
    rig.wait(&id);
    // The plan ran: a new card for it is not proposed either.
    assert_eq!(refusal_code(rig.propose_move(&plan)), "AlreadyRun");
    assert_eq!(rig.events(&id).iter().filter(|e| *e == "begin").count(), 1);
}

#[test]
fn a_move_copies_in_rounds_cuts_over_checks_and_measures_its_downtime() {
    let rig = Rig::new(vec![web_with_volume()], 100);
    rig.source
        .add_volume("default", "default", "web-data", json!({}));
    rig.files("custom/default_web-data", 1);
    rig.source.set_push_bytes("instance:shop/web", 40 * MIB);
    rig.source
        .write_guest_file("shop", "web", "/srv/data/check", b"orders 41\n");
    let plan = rig.plan_with(&["web"], json!({ "instances": [{ "project": "shop", "name": "web", "spotCheck": "/srv/data/check" }] }));
    assert_eq!(plan["go"], true, "{plan}");
    // The copy's state is read only after the stop, and answers 200 ms late: the measured downtime holds that wait.
    rig.target
        .slow_reads("/1.0/instances/web/state", 1, Duration::from_millis(200));
    let proposal = rig.propose_move(&plan).expect("proposed");
    let started = rig.approve(&proposal).expect("started");
    assert_eq!(started["phase"], "copying");
    let id = started["move"].as_str().unwrap().to_string();
    assert_eq!(
        id,
        format!("mv-{}", &plan["digest"].as_str().unwrap()[..12])
    );
    let status = rig.wait(&id);
    assert_eq!(status["phase"], "moved", "{status}");
    let one = &status["instances"][0];
    assert_eq!(one["result"], "moved", "{one}");
    assert_eq!(one["roundsDone"], 3);
    assert_eq!(one["roundsTotal"], 3);
    assert!(
        one["downtimeMs"]
            .as_u64()
            .is_some_and(|ms| (200..30_000).contains(&ms)),
        "the result carries the measured downtime, from the stop to the passed checks: {one}"
    );
    let checks: Vec<&str> = one["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["check"].as_str().unwrap())
        .collect();
    assert_eq!(
        checks,
        [
            "running",
            "network",
            "ports",
            "guestAgent",
            "volumes",
            "spotCheck"
        ]
    );
    assert!(
        one["checks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["ok"] == true),
        "{one}"
    );
    assert!(
        one["checks"][5]["detail"]
            .as_str()
            .unwrap()
            .contains("the same on both")
    );
    // Two rounds while it ran and the last delta after the stop, each the volume and then the container.
    let pushes = rig.source.pushes();
    assert_eq!(pushes.len(), 6, "{pushes:?}");
    assert!(pushes[0].starts_with(
        "volume:default/default/web-data -> https://203.0.113.20:8443/1.0/operations/"
    ));
    assert!(pushes[1].starts_with("instance:shop/web -> "));
    let rounds: Vec<Value> = rig
        .journal()
        .into_iter()
        .filter(|l| l["event"] == "round")
        .collect();
    assert_eq!(rounds.len(), 3);
    assert_eq!(rounds[0]["refresh"], false);
    assert_eq!(rounds[1]["refresh"], true);
    assert_eq!(rounds[2]["final"], true);
    assert_eq!(rounds[0]["bytes"], json!(41 * MIB));
    // The copy runs on the target with its volume; the source is kept, stopped, with its volume.
    assert_eq!(
        rig.target.instance_status("shop", "web").as_deref(),
        Some("Running")
    );
    assert!(rig.target.holds("volume", "default", "web-data"));
    assert_eq!(
        rig.source.instance_status("shop", "web").as_deref(),
        Some("Stopped")
    );
    assert!(rig.source.holds("volume", "default", "web-data"));
    let result = rig
        .journal()
        .into_iter()
        .find(|l| l["event"] == "result")
        .unwrap();
    assert_eq!(result["result"], "moved");
    assert_eq!(result["downtimeMs"], one["downtimeMs"]);
    // The status is as the journal tells it; the result was journaled on the proposal too.
    let events = rig.events(&id);
    assert_eq!(events.first().map(String::as_str), Some("begin"));
    assert_eq!(events.last().map(String::as_str), Some("end"));
    for event in ["round", "cutover", "verify", "result"] {
        assert!(events.iter().any(|e| e == event), "{event}: {events:?}");
    }
}

#[test]
fn progress_is_readable_during_a_slow_round_and_reading_it_writes_nothing() {
    let rig = Rig::new(vec![web()], 100);
    rig.source.set_push_bytes("instance:shop/web", 40 * MIB);
    rig.source.plan_push(PushPlan::Hold {
        processed: 10 * MIB,
        speed: 5 * MIB,
    });
    let started = rig.start(&["web"]);
    let id = started["move"].as_str().unwrap().to_string();
    let until = Instant::now() + Duration::from_secs(10);
    while rig.source.held_pushes().is_empty() {
        assert!(Instant::now() < until, "the first push never started");
        std::thread::sleep(Duration::from_millis(10));
    }
    // Incus's progress reaches the status while the round runs.
    let status = loop {
        let status = rig.status(&id);
        if status["instances"][0]["bytesMoved"] == json!(10 * MIB) {
            break status;
        }
        assert!(Instant::now() < until, "no progress: {status}");
        std::thread::sleep(Duration::from_millis(20));
    };
    let (writes_source, writes_target) = (rig.source.writes().len(), rig.target.writes().len());
    let one = &status["instances"][0];
    assert_eq!(status["phase"], "running");
    assert_eq!(one["phase"], "copying");
    assert_eq!(one["roundsDone"], 0);
    assert_eq!(one["rateBytesPerSecond"], json!(5 * MIB));
    assert!(one["bytesTotal"].as_u64().unwrap() >= 2 * MIB);
    assert_eq!(one["downtimeMs"], Value::Null);
    assert_eq!(status["finish"]["available"], false);
    rig.status(&id);
    assert_eq!(
        rig.source.writes().len(),
        writes_source,
        "reading the status writes nothing"
    );
    assert_eq!(rig.target.writes().len(), writes_target);
    // A finish is not offered while it runs.
    assert_eq!(refusal_code(rig.propose_finish(&id)), "NotVerified");
    for held in rig.source.held_pushes() {
        rig.source.release_operation(&held);
    }
    let status = rig.wait(&id);
    assert_eq!(status["instances"][0]["result"], "moved", "{status}");
}

#[test]
fn a_failed_push_rolls_back_and_the_source_keeps_running() {
    let rig = Rig::new(vec![web_with_volume()], 100);
    rig.source
        .add_volume("default", "default", "web-data", json!({}));
    rig.files("custom/default_web-data", 1);
    // The volume lands; the container's push fails.
    rig.source.plan_push(PushPlan::Pass);
    rig.source.plan_push(PushPlan::Fail(
        "connection reset by the network".to_string(),
    ));
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    let status = rig.wait(&id);
    let one = &status["instances"][0];
    assert_eq!(status["phase"], "rolled_back");
    assert_eq!(one["result"], "rolled_back");
    assert!(
        one["reason"].as_str().unwrap().contains("connection reset"),
        "{one}"
    );
    assert_eq!(
        rig.source.instance_status("shop", "web").as_deref(),
        Some("Running")
    );
    assert!(
        rig.source.holds("volume", "default", "web-data"),
        "the source's volume is never touched"
    );
    assert!(!rig.target.holds("instance", "shop", "web"));
    assert!(
        !rig.target.holds("volume", "default", "web-data"),
        "the copied volume is removed"
    );
    let rollback = rig
        .journal()
        .into_iter()
        .find(|l| l["event"] == "rollback")
        .unwrap();
    assert_eq!(rollback["sourceRunning"], true);
    assert_eq!(rollback["targetRemoved"], true);
    assert!(
        !rig.events(&id).contains(&"cutover".to_string()),
        "it never stopped the source"
    );
}

#[test]
fn a_copy_that_does_not_start_is_removed_and_the_source_started_again() {
    let rig = Rig::new(vec![web()], 100);
    rig.target.refuse_start("shop", "web");
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    let status = rig.wait(&id);
    let one = &status["instances"][0];
    assert_eq!(one["result"], "rolled_back", "{one}");
    assert!(
        one["reason"].as_str().unwrap().contains("did not start"),
        "{one}"
    );
    assert_eq!(one["downtimeMs"], Value::Null);
    assert_eq!(
        rig.source.instance_status("shop", "web").as_deref(),
        Some("Running")
    );
    assert!(!rig.target.holds("instance", "shop", "web"));
}

#[test]
fn a_copy_that_fails_its_checks_is_rolled_back() {
    // The copy starts, and its program exits at once on the target.
    let rig = Rig::new(vec![web()], 100);
    rig.target.exit_after_start("shop", "web", 0, "exited");
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    let status = rig.wait(&id);
    let one = &status["instances"][0];
    assert_eq!(one["result"], "rolled_back", "{one}");
    assert!(
        one["reason"]
            .as_str()
            .unwrap()
            .contains("running: the copy is Stopped"),
        "{one}"
    );
    let verify = rig
        .journal()
        .into_iter()
        .find(|l| l["event"] == "verify")
        .unwrap();
    assert_eq!(verify["ok"], false);
    assert_eq!(
        rig.source.instance_status("shop", "web").as_deref(),
        Some("Running")
    );
    assert!(!rig.target.holds("instance", "shop", "web"));
}

#[test]
fn a_target_that_fills_during_the_move_rolls_it_back() {
    let rig = Rig::new(vec![web()], 100);
    rig.source.plan_push(PushPlan::Hold {
        processed: MIB,
        speed: MIB,
    });
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    let until = Instant::now() + Duration::from_secs(10);
    while rig.source.held_pushes().is_empty() {
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(10));
    }
    // Something else fills the target's pool while the first round runs.
    rig.target.set_pool_space("default", 100 * MIB, 100 * MIB);
    for held in rig.source.held_pushes() {
        rig.source.release_operation(&held);
    }
    let status = rig.wait(&id);
    let one = &status["instances"][0];
    assert_eq!(one["result"], "rolled_back", "{one}");
    assert!(
        one["reason"]
            .as_str()
            .unwrap()
            .contains("the target filled up"),
        "{one}"
    );
    assert_eq!(one["roundsDone"], 1);
    assert!(
        !rig.target.holds("instance", "shop", "web"),
        "the first round's copy is removed"
    );
    assert_eq!(
        rig.source.instance_status("shop", "web").as_deref(),
        Some("Running")
    );
}

#[test]
fn a_batch_with_one_failing_instance_rolls_that_one_back_and_moves_the_others() {
    let rig = Rig::new(vec![web(), api()], 100);
    // The first instance of the batch fails; the one after it still moves.
    rig.target.refuse_start("shop", "web");
    let plan = rig.plan(&["web", "api"]);
    assert_eq!(plan["go"], true, "{plan}");
    let proposal = rig.propose_move(&plan).expect("one card for the batch");
    assert_eq!(proposal["summary"], "Move 2 containers to node-b");
    let id = rig.approve(&proposal).expect("started")["move"]
        .as_str()
        .unwrap()
        .to_string();
    let status = rig.wait(&id);
    assert_eq!(status["phase"], "partly_moved", "{status}");
    assert_eq!(status["instances"][0]["result"], "rolled_back");
    assert_eq!(status["instances"][1]["result"], "moved", "{status}");
    assert_eq!(
        rig.target.instance_status("shop", "api").as_deref(),
        Some("Running")
    );
    assert!(!rig.target.holds("instance", "shop", "web"));
    assert_eq!(
        rig.source.instance_status("shop", "web").as_deref(),
        Some("Running")
    );
    assert_eq!(
        rig.source.instance_status("shop", "api").as_deref(),
        Some("Stopped")
    );
    // The finish offers only the verified one.
    assert_eq!(status["finish"]["available"], true);
    let deletes = status["finish"]["deletes"].to_string();
    assert!(
        deletes.contains("shop/api") && !deletes.contains("shop/web"),
        "{deletes}"
    );
}

#[test]
fn a_finish_is_not_offered_while_the_move_runs_even_with_one_instance_verified() {
    let rig = Rig::new(vec![web(), api()], 100);
    // web's three pushes land; api's first push is held.
    for _ in 0..3 {
        rig.source.plan_push(PushPlan::Pass);
    }
    rig.source.plan_push(PushPlan::Hold {
        processed: MIB,
        speed: MIB,
    });
    let id = rig.start(&["web", "api"])["move"]
        .as_str()
        .unwrap()
        .to_string();
    let until = Instant::now() + Duration::from_secs(10);
    while rig.source.held_pushes().is_empty() {
        assert!(Instant::now() < until, "api's push never started");
        std::thread::sleep(Duration::from_millis(10));
    }
    let status = rig.status(&id);
    assert_eq!(status["instances"][0]["result"], "moved", "{status}");
    assert_eq!(status["finish"]["available"], false);
    assert_eq!(refusal_code(rig.propose_finish(&id)), "NotVerified");
    for held in rig.source.held_pushes() {
        rig.source.release_operation(&held);
    }
    assert_eq!(rig.wait(&id)["phase"], "moved");
    rig.propose_finish(&id)
        .expect("offered once the move ended");
}

#[test]
fn the_finish_is_offered_only_after_the_checks_and_deletes_only_on_the_owners_card() {
    let rig = Rig::new(vec![web_with_volume()], 100);
    rig.source
        .add_volume("default", "default", "web-data", json!({}));
    rig.files("custom/default_web-data", 1);
    // A move that rolled back has nothing to finish.
    rig.target.refuse_start("shop", "web");
    let failed = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    rig.wait(&failed);
    assert_eq!(refusal_code(rig.propose_finish(&failed)), "NotVerified");
    assert_eq!(
        refusal_code(rig.propose_finish("mv-000000000000")),
        "NoSuchMove"
    );
}

#[test]
fn a_verified_move_is_finished_only_by_the_owners_explicit_card() {
    let rig = Rig::new(vec![web_with_volume()], 100);
    rig.source
        .add_volume("default", "default", "web-data", json!({}));
    rig.files("custom/default_web-data", 1);
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    let status = rig.wait(&id);
    assert_eq!(status["phase"], "moved", "{status}");
    // Nothing deletes the source by itself: it waits, stopped, as long as the owner does.
    rig.now
        .store(NOW + 30 * 24 * 60 * 60 * 1000, Ordering::SeqCst);
    assert!(rig.source.holds("instance", "shop", "web"));
    assert!(!rig.events(&id).contains(&"finish".to_string()));
    // A direct call, or one with a made-up grant, deletes nothing.
    let direct = rig.call_as(Scope::Doctor, "instance_move_finish", json!({ "move": id }));
    assert!(matches!(direct, Err(CallError::Invalid(_))), "{direct:?}");
    let forged = rig.call_as(
        Scope::Doctor,
        "instance_move_finish",
        json!({ "move": id, "approval": "forged" }),
    );
    assert_eq!(refusal_code(forged), "NotApproved");
    assert!(rig.source.holds("instance", "shop", "web"));
    // The owner's card names what goes.
    rig.now.store(NOW, Ordering::SeqCst);
    let proposal = rig.propose_finish(&id).expect("proposed");
    let shown = proposal["details"].to_string();
    assert!(
        shown.contains("the source instance shop/web on this host"),
        "{shown}"
    );
    assert!(shown.contains("web-data"), "{shown}");
    let finished = rig.approve(&proposal).expect("finished");
    assert_eq!(
        finished["deleted"],
        json!(["shop/web", "volume default/web-data"])
    );
    assert_eq!(finished["kept"], json!([]));
    assert!(!rig.source.holds("instance", "shop", "web"));
    assert!(!rig.source.holds("volume", "default", "web-data"));
    assert_eq!(
        rig.target.instance_status("shop", "web").as_deref(),
        Some("Running")
    );
    assert!(rig.target.holds("volume", "default", "web-data"));
    let status = rig.status(&id);
    assert_eq!(status["phase"], "finished");
    assert_eq!(status["instances"][0]["result"], "finished");
    assert!(rig.approve(&proposal).is_err(), "a finish runs once");
    assert_eq!(refusal_code(rig.propose_finish(&id)), "NotVerified");
}

#[test]
fn the_journal_holds_each_step_without_secrets_and_tolerates_a_torn_tail() {
    let rig = Rig::new(vec![web(), api()], 100);
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    rig.wait(&id);
    let path = rig.dir.join("state").join(arca::instance_move::MOVES_FILE);
    let text = fs::read_to_string(&path).expect("journal");
    for event in [
        "plan", "begin", "round", "created", "cutover", "verify", "result", "end",
    ] {
        assert!(
            text.contains(&format!("\"event\":\"{event}\"")),
            "{event} missing"
        );
    }
    assert!(
        !text.contains("BEGIN CERTIFICATE"),
        "no certificate in the journal"
    );
    assert!(
        !text.contains("\"control\"") && !text.contains("\"fs\""),
        "no websocket secret"
    );
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    // A crash leaves half a line: it is skipped, and the next line starts on its own.
    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(br#"{"event":"round","move":"#).unwrap();
    drop(file);
    assert_eq!(rig.status(&id)["phase"], "moved");
    let again = rig.plan(&["api"]);
    assert_eq!(again["go"], true, "{again}");
    let lines = rig.journal();
    assert_eq!(lines.last().unwrap()["event"], "plan");
}

#[test]
fn a_move_a_stopped_kernel_left_unfinished_is_rolled_back_on_the_next_start() {
    let rig = Rig::new(vec![web()], 100);
    // The kernel stopped in the cutover: the source stopped, a copy on the target.
    arca_incus::MoveWriter::new(rig.source.socket().to_path_buf())
        .set_state("shop", "web", "stop", true, Duration::from_secs(5))
        .expect("stopped");
    let journal = rig.moves.journal();
    journal
        .append(&json!({
            "event": "begin", "at": NOW, "move": "mv-0123456789ab", "plan": "ab".repeat(32), "target": "node-b",
            "proposal": null, "instances": [{
                "project": "shop", "name": "web", "wasRunning": true, "roundsTotal": 3,
                "bytesTotal": 2 * MIB, "needed": 3 * MIB, "targetPool": "default", "volumes": [],
            }],
        }))
        .unwrap();
    journal
        .append(&json!({ "event": "cutover", "at": NOW, "move": "mv-0123456789ab", "project": "shop", "name": "web", "step": "stopped" }))
        .unwrap();
    assert_eq!(rig.status("mv-0123456789ab")["interrupted"], true);
    rig.moves.recover();
    let status = rig.wait("mv-0123456789ab");
    assert_eq!(status["phase"], "rolled_back", "{status}");
    assert_eq!(status["interrupted"], true);
    assert!(
        status["instances"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("the kernel stopped during the move")
    );
    assert_eq!(
        rig.source.instance_status("shop", "web").as_deref(),
        Some("Running")
    );
    let rollback = rig
        .journal()
        .into_iter()
        .find(|l| l["event"] == "rollback")
        .unwrap();
    assert_eq!(rollback["interrupted"], true);
}
