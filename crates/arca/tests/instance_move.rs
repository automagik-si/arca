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
use arca_vmimport::{PipeFailure, Runner};
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
    third: FakeIncus,
    _kernel_fake: FakeIncus,
    _scratch: Scratch,
    kernel: Arc<Mutex<Kernel>>,
    key: SkKey,
    now: Arc<AtomicU64>,
    moves: Arc<Moves>,
    handlers: HashMap<&'static str, Arc<dyn ToolHandler>>,
    executor: ActionExecutor,
    nonce: AtomicU64,
    /// The guest probe's record: each address the move asked about, and the addresses that answer (none until a test
    /// names one). No test connects to a real address.
    probed: Arc<Mutex<Vec<String>>>,
    answering: Arc<Mutex<Vec<String>>>,
}

impl Rig {
    /// This node holds `instances`; node-b, its linked host, a pool of `target_mib` MiB.
    fn new(instances: Vec<Value>, target_mib: u64) -> Rig {
        Rig::waiting(instances, target_mib, Duration::from_millis(300))
    }

    /// The same, giving a copy `verify_wait` to come up and a failed copy's receiving operation as long to end.
    fn waiting(instances: Vec<Value>, target_mib: u64, verify_wait: Duration) -> Rig {
        Rig::build(instances, target_mib, verify_wait, None)
    }

    /// The same, with node-b's Ceph cluster named as this node reaches it (`approval.linkedHosts[].ceph`), and `rbd` run
    /// by `ceph`'s recorder.
    fn with_ceph(instances: Vec<Value>, linked: Value, ceph: Arc<CephFake>) -> Rig {
        Rig::build(
            instances,
            4096,
            Duration::from_millis(300),
            Some((linked, ceph)),
        )
    }

    fn build(
        instances: Vec<Value>,
        target_mib: u64,
        verify_wait: Duration,
        ceph: Option<(Value, Arc<CephFake>)>,
    ) -> Rig {
        let dir = std::env::temp_dir().join(format!(
            "arca-instance-move-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("state")).expect("state");
        let vms: Vec<(String, String)> = instances
            .iter()
            .filter(|one| one["type"] == "virtual-machine")
            .map(|one| {
                (
                    one["project"].as_str().unwrap_or("shop").to_string(),
                    one["name"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        let names: Vec<(String, String)> = instances
            .iter()
            .map(|one| {
                (
                    one["project"].as_str().unwrap_or("shop").to_string(),
                    one["name"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        let source = host(&dir.join("source"), instances, 100, false);
        // Each container holds the default spot-check file, as a real one does.
        for (project, name) in &names {
            source.write_guest_file(
                project,
                name,
                "/etc/hostname",
                format!("{name}\n").as_bytes(),
            );
        }
        let target = host(&dir.join("target"), Vec::new(), target_mib, true);
        // A virtual machine's copy boots and its Incus agent answers, unless a test says otherwise.
        for (project, name) in &vms {
            target.agent(project, name);
        }
        source.link_migration(&target);
        let third = host(&dir.join("third"), Vec::new(), target_mib, true);
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
        let mut cfg = json!({
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
            }, {
                "name": "node-c",
                "socket": dir.join("node-c-arca.sock"),
                "tokenFile": dir.join("node-c.token"),
                "incusSocket": third.socket(),
            }] },
        });
        if let Some((linked, _)) = &ceph
            && !linked.is_null()
        {
            cfg["approval"]["linkedHosts"][0]["ceph"] = linked.clone();
        }
        let cfg: NodeConfig = serde_json::from_value(cfg).expect("config");
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
        let mut moves = Moves::new(Arc::clone(&cfg), Arc::clone(&kernel), Arc::clone(&clock))
            .with_storage_root(dir.join("source"))
            .with_timing(Duration::from_millis(20), verify_wait);
        if let Some((_, runner)) = ceph {
            moves = moves.with_ceph_runner(runner);
        }
        let probed = Arc::new(Mutex::new(Vec::new()));
        let answering: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let (asked, answers) = (Arc::clone(&probed), Arc::clone(&answering));
        moves = moves.with_guest_probe(Arc::new(move |address: &str| {
            asked.lock().unwrap().push(address.to_string());
            answers.lock().unwrap().iter().any(|one| one == address)
        }));
        let moves = Arc::new(moves);
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
            third,
            _kernel_fake: kernel_fake,
            _scratch: scratch,
            kernel,
            key,
            now,
            moves,
            probed,
            answering,
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
                limits: Default::default(),
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
    // The files' bytes, plus each folder's own blocks: none on tmpfs, 4 KiB a folder on ext4.
    let near = |value: &Value, bytes: u64| {
        value
            .as_u64()
            .is_some_and(|got| got >= bytes && got < bytes + 64 * 1024)
    };
    assert!(near(&one["bytes"]["instance"], 2 * MIB), "{one}");
    assert!(near(&one["bytes"]["volumes"], MIB), "{one}");
    assert!(near(&one["bytes"]["measured"], 3 * MIB), "{one}");
    assert_eq!(
        one["bytes"]["measured"].as_u64(),
        one["bytes"]["instance"]
            .as_u64()
            .zip(one["bytes"]["volumes"].as_u64())
            .map(|(instance, volumes)| instance + volumes),
        "{one}"
    );
    assert!(one["bytes"]["needed"].as_u64().unwrap() > 3 * MIB);
    assert_eq!(one["rounds"]["preCopy"], 2);
    assert_eq!(one["volumes"][0]["name"], "web-data");
    assert_eq!(one["cutover"][0], "stop the source");
    assert!(one["verify"].as_array().unwrap().len() >= 4);
    // The plan is kept in its own private file, not in the move journal; nothing was written on either host.
    let kept = rig
        .dir
        .join("state")
        .join(arca::instance_move::PLANS_DIR)
        .join(format!("{}.json", digest));
    let frozen: Value = serde_json::from_slice(&fs::read(&kept).expect("the plan file")).unwrap();
    assert_eq!(frozen["digest"], digest);
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&kept).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    assert!(rig.journal().is_empty(), "planning writes no journal line");
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

type Hook = Box<dyn Fn() + Send + Sync>;

/// `rbd` as this node would run it, recorded: each image's size and watchers by `<cluster>:<pool>/<image>`, every
/// command line in order, a copy that may be told to fail, and what happens elsewhere during the copy.
#[derive(Default)]
struct CephFake {
    sizes: Mutex<HashMap<String, u64>>,
    watchers: Mutex<HashMap<String, Vec<String>>>,
    calls: Mutex<Vec<Vec<String>>>,
    fail_copy: Mutex<Option<String>>,
    /// Runs once, at the first copy, before it.
    during_copy: Mutex<Option<Hook>>,
}

impl CephFake {
    fn image(&self, cluster: &str, spec: &str, bytes: u64) {
        self.sizes
            .lock()
            .unwrap()
            .insert(format!("{cluster}:{spec}"), bytes);
    }

    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }

    /// The `rbd` subcommands run on `cluster`, in order.
    fn on(&self, cluster: &str) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter(|argv| argv[2] == cluster)
            .map(|argv| argv[6].clone())
            .collect()
    }

    fn key(argv: &[String]) -> String {
        format!("{}:{}", argv[2], argv.last().cloned().unwrap_or_default())
    }
}

impl Runner for CephFake {
    fn output(&self, argv: &[String]) -> Result<String, String> {
        self.calls.lock().unwrap().push(argv.to_vec());
        let key = CephFake::key(argv);
        match argv[6].as_str() {
            "status" => Ok(json!({
                "watchers": self.watchers.lock().unwrap().get(&key).cloned().unwrap_or_default()
                    .iter().map(|address| json!({ "address": address })).collect::<Vec<_>>(),
            })
            .to_string()),
            "info" => match self.sizes.lock().unwrap().get(&key) {
                Some(bytes) => Ok(json!({ "size": bytes }).to_string()),
                None => Err(format!(
                    "rbd failed: error opening image {key}: No such file or directory"
                )),
            },
            other => Err(format!("rbd {other} is not one the copy runs")),
        }
    }

    fn pipe(&self, first: &[String], second: &[String]) -> Result<(), PipeFailure> {
        if let Some(hook) = self.during_copy.lock().unwrap().take() {
            hook();
        }
        let mut calls = self.calls.lock().unwrap();
        calls.push(first.to_vec());
        calls.push(second.to_vec());
        match self.fail_copy.lock().unwrap().clone() {
            Some(message) => Err(PipeFailure::writer(message)),
            None => Ok(()),
        }
    }
}

const GIB: u64 = 1024 * MIB;
/// The size of a virtual machine's config image (its UEFI variables and TPM state) in these tests.
const CONFIG: u64 = 100 * MIB;

/// Gives `db`'s disk (1 GiB) and its config image on `cluster`, in the OSD pool `osd`, as Incus names them.
fn vm_images(ceph: &CephFake, cluster: &str, osd: &str) {
    ceph.image(
        cluster,
        &format!("{osd}/virtual-machine_shop_db.block"),
        GIB,
    );
    ceph.image(cluster, &format!("{osd}/virtual-machine_shop_db"), CONFIG);
}

/// A stopped virtual machine `db` of the project `shop` on pool `default`, its root disk 1 GiB.
fn vm() -> Value {
    json!({
        "name": "db", "project": "shop", "type": "virtual-machine", "status": "Stopped", "architecture": "x86_64",
        "profiles": ["default"],
        "config": {
            "limits.cpu": "2",
            "volatile.eth0.hwaddr": "00:16:3e:00:00:09",
            "volatile.uuid": "00000000-0000-4000-8000-000000000009",
        },
        "devices": {
            "root": { "type": "disk", "path": "/", "pool": "default", "size": "1GiB" },
            "eth0": { "type": "nic", "network": "arcabr0", "ipv4.address": "192.0.2.59" }
        }
    })
}

/// `db` with the block custom volume `db-data` of 512 MiB.
fn vm_with_volume() -> Value {
    let mut db = vm();
    db["devices"]["data"] = json!({ "type": "disk", "pool": "default", "source": "db-data" });
    db
}

/// Makes `default` a Ceph pool on both hosts: this node's on the cluster `here`, node-b's on `there`, each with its own
/// OSD pool and Incus client.
fn ceph_pools(rig: &Rig, here: &str, there: &str) {
    for (fake, cluster) in [(&rig.source, here), (&rig.target, there)] {
        fake.set_pool_driver("default", "ceph");
        fake.set_pool_config("default", "ceph.cluster_name", "ceph");
        fake.set_pool_config("default", "ceph.osd.pool_name", &format!("incus-{cluster}"));
        fake.set_pool_config("default", "ceph.user.name", &format!("incus-{cluster}"));
    }
}

/// Plans, proposes and approves a move of `db`; waits for its end and answers its status and the plan.
fn move_db(rig: &Rig) -> (Value, Value) {
    let plan = rig.plan(&["db"]);
    assert_eq!(plan["go"], true, "{plan}");
    let proposal = rig.propose_move(&plan).expect("proposed");
    let started = rig.approve(&proposal).expect("approved and started");
    let status = rig.wait(started["move"].as_str().unwrap());
    (status, plan)
}

fn card(plan: &Value) -> Vec<(String, String)> {
    Moves::card_rows(&plan["plan"])
}

#[test]
fn a_running_virtual_machine_is_not_moved_until_it_is_stopped() {
    let mut running = vm();
    running["status"] = json!("Running");
    let rig = Rig::new(vec![running], 100);
    rig.files("virtual-machines/shop_db", 1);
    let answer = rig.plan(&["db"]);
    assert_eq!(answer["go"], false);
    let first = answer["blocked"][0]["reasons"][0].as_str().unwrap();
    assert!(
        first.starts_with("the virtual machine is Running: this build moves a virtual machine only while it is stopped"),
        "{first}"
    );
}

/// One Ceph cluster, or none: a stopped virtual machine is pushed by Incus, once, consistent, with no rbd at all. Its
/// block disk is sized from Incus, its copy starts on the target and must answer, and the source stays stopped.
#[test]
fn a_stopped_virtual_machine_on_one_cluster_moves_by_one_incus_push() {
    let ceph = Arc::new(CephFake::default());
    // node-b names no Ceph cluster of its own.
    let rig = Rig::with_ceph(vec![vm()], Value::Null, Arc::clone(&ceph));
    ceph_pools(&rig, "one", "two");
    let (status, plan) = move_db(&rig);
    let instance = &plan["plan"]["instances"][0];
    assert_eq!(instance["copy"], "incus");
    assert_eq!(instance["ceph"], Value::Null);
    assert_eq!(instance["rounds"]["preCopy"], 0);
    assert_eq!(instance["bytes"]["instance"], GIB, "{instance}");
    let one = &status["instances"][0];
    assert_eq!(one["result"], "moved", "{one}");
    assert_eq!(one["roundsTotal"], 1);
    assert!(
        one["checks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["ok"] == true),
        "{one}"
    );
    let agent = one["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["check"] == "guestAgent")
        .unwrap();
    assert_eq!(
        agent["detail"],
        "the guest answers: its Incus agent answers"
    );
    // One push, after nothing stopped it, consistent; no rbd command ran.
    let pushes = rig.source.pushes();
    assert_eq!(pushes.len(), 1, "{pushes:?}");
    assert!(pushes[0].starts_with("instance:shop/db -> https://203.0.113.20:8443/1.0/operations/"));
    assert!(!pushes[0].ends_with(" (inconsistent)"), "{pushes:?}");
    assert!(ceph.calls().is_empty());
    assert!(
        !rig.source
            .writes()
            .iter()
            .any(|w| w.path.contains("/state")),
        "nothing started or stopped the source: {:?}",
        rig.source.writes()
    );
    assert_eq!(
        rig.target.instance_status("shop", "db").as_deref(),
        Some("Running")
    );
    assert_eq!(
        rig.source.instance_status("shop", "db").as_deref(),
        Some("Stopped")
    );
    // The card names both nodes, the disk's size and that the machine stays stopped.
    let rows = card(&plan);
    let (label, row) = &rows[0];
    assert_eq!(label, "Move shop/db (virtual machine, stopped)");
    assert!(row.starts_with("from "), "{row}");
    assert!(row.contains(" to node-b; its disk is 1.0 GiB;"), "{row}");
    assert!(
        row.contains("it stays stopped for the move and is copied by an Incus push"),
        "{row}"
    );
    assert!(
        rows.iter()
            .any(|(label, _)| label == "Stopped virtual machines")
    );
    // Its rollback promise is a stopped machine's: nothing starts the source.
    let rollback = &rows
        .iter()
        .find(|(label, _)| label == "Rollback")
        .unwrap()
        .1;
    assert!(
        rollback.contains("The source stays stopped throughout: the move never starts it"),
        "{rollback}"
    );
    assert!(!rollback.contains("starts the source again"), "{rollback}");
    assert!(!rollback.contains("keeps running"), "{rollback}");
}

/// Two Ceph clusters, either way: node-b names its cluster, so the machine and its block volume are made empty there by
/// Incus and copied by rbd from this node. The source's cluster sees only status, info and export-diff; the target's only
/// info and import-diff; each side names its own cluster, client and OSD pool. No Incus push runs.
#[test]
fn a_stopped_virtual_machine_moves_between_two_ceph_clusters_either_way() {
    for (here, there) in [("east", "west"), ("west", "east")] {
        let ceph = Arc::new(CephFake::default());
        let linked = json!({ "cluster": there });
        let rig = Rig::with_ceph(vec![vm_with_volume()], linked, Arc::clone(&ceph));
        ceph_pools(&rig, here, there);
        rig.source.add_volume(
            "default",
            "default",
            "db-data",
            json!({ "content_type": "block", "config": { "size": "512MiB" } }),
        );
        // This node reads its own cluster by the name Incus gives it (`ceph`); node-b's by the name the link gives.
        let source_osd = format!("incus-{here}");
        let target_osd = format!("incus-{there}");
        for (cluster, osd) in [("ceph", &source_osd), (there, &target_osd)] {
            vm_images(&ceph, cluster, osd);
            ceph.image(cluster, &format!("{osd}/custom_db-data.block"), 512 * MIB);
        }
        let (status, plan) = move_db(&rig);
        let instance = &plan["plan"]["instances"][0];
        assert_eq!(instance["copy"], "ceph", "{instance}");
        assert_eq!(
            instance["ceph"]["default"],
            json!({
                "targetPool": "default",
                "from": { "cluster": "ceph", "client": source_osd, "pool": source_osd },
                "to": { "cluster": there, "client": target_osd, "pool": target_osd },
            })
        );
        let one = &status["instances"][0];
        assert_eq!(one["result"], "moved", "{here} -> {there}: {one}");
        // The source cluster is only read; the target's only sized and written.
        assert_eq!(
            ceph.on("ceph"),
            [
                "status",
                "info",
                "status",
                "info",
                "status",
                "info",
                "export-diff",
                "export-diff",
                "export-diff"
            ],
            "{here} -> {there}"
        );
        assert_eq!(
            ceph.on(there),
            [
                "info",
                "import-diff",
                "info",
                "import-diff",
                "info",
                "import-diff"
            ],
            "{here} -> {there}"
        );
        // The volume, then the machine's config image (UEFI variables, TPM state), then its disk.
        let exported: Vec<String> = ceph
            .calls()
            .into_iter()
            .filter(|argv| argv[6] == "export-diff")
            .map(|argv| argv[7].clone())
            .collect();
        assert_eq!(
            exported,
            [
                format!("{source_osd}/custom_db-data.block"),
                format!("{source_osd}/virtual-machine_shop_db"),
                format!("{source_osd}/virtual-machine_shop_db.block"),
            ]
        );
        let calls = ceph.calls();
        let export = calls.iter().find(|argv| argv[6] == "export-diff").unwrap();
        assert_eq!(
            export[1..],
            [
                "--cluster",
                "ceph",
                "--id",
                source_osd.as_str(),
                "--no-progress",
                "export-diff",
                &format!("{source_osd}/custom_db-data.block"),
                "-"
            ]
        );
        let import = calls
            .iter()
            .rev()
            .find(|argv| argv[6] == "import-diff")
            .unwrap();
        assert_eq!(
            import[1..],
            [
                "--cluster",
                there,
                "--id",
                target_osd.as_str(),
                "--no-progress",
                "import-diff",
                "-",
                &format!("{target_osd}/virtual-machine_shop_db.block")
            ]
        );
        assert!(
            rig.source.pushes().is_empty(),
            "no Incus push: {:?}",
            rig.source.pushes()
        );
        // Incus on node-b made the machine of the source's exact size, and its volume; the copy runs there.
        let copy = writer(&rig.target)
            .get("/1.0/instances/db?project=shop")
            .expect("the copy");
        assert_eq!(copy["type"], "virtual-machine");
        assert_eq!(copy["devices"]["root"]["size"], format!("{GIB}B"));
        assert_eq!(copy["config"]["volatile.eth0.hwaddr"], "00:16:3e:00:00:09");
        assert_ne!(
            copy["config"]["volatile.uuid"],
            "00000000-0000-4000-8000-000000000009"
        );
        assert_eq!(
            rig.target.instance_status("shop", "db").as_deref(),
            Some("Running")
        );
        let volume = writer(&rig.target)
            .get("/1.0/storage-pools/default/volumes/custom/db-data?project=default")
            .expect("the volume's copy");
        assert_eq!(volume["content_type"], "block");
        assert_eq!(volume["config"]["size"], format!("{}B", 512 * MIB));
        // The source is kept, stopped, with its volume; the only write to it turns its autostart off.
        assert_eq!(
            rig.source.instance_status("shop", "db").as_deref(),
            Some("Stopped")
        );
        assert!(rig.source.holds("volume", "default", "db-data"));
        let writes: Vec<String> = rig
            .source
            .writes()
            .iter()
            .map(|w| format!("{} {}", w.method, w.path))
            .collect();
        assert_eq!(
            writes,
            ["PATCH /1.0/instances/db?project=shop"],
            "{here} -> {there}"
        );
        // The card names the two clusters and says no Incus push runs.
        let rows = card(&plan);
        let ceph_row = &rows.iter().find(|(label, _)| label == "Ceph").unwrap().1;
        assert!(
            ceph_row.contains(&format!(
                "read only from the cluster ceph, pool {source_osd}, as client.{source_osd} here, written into the cluster {there}, pool {target_osd}, as client.{target_osd} for node-b"
            )),
            "{ceph_row}"
        );
        let transport = &rows
            .iter()
            .find(|(label, _)| label == "Transport")
            .unwrap()
            .1;
        assert!(transport.starts_with("no Incus push"), "{transport}");
        assert!(rows[0].1.contains("copied by Ceph (rbd)"), "{}", rows[0].1);
        assert!(
            rows[0]
                .1
                .contains("with its config image (its UEFI variables and TPM state)"),
            "{}",
            rows[0].1
        );
        assert!(
            transport.contains("its empty custom volumes, to start it, to set its boot.autostart"),
            "{transport}"
        );
        // The finish card says the source's snapshots, which a copy by Ceph left behind, go with it.
        let deletes = status["finish"]["deletes"].to_string();
        assert!(
            deletes.contains("with its snapshots: they were not copied"),
            "{deletes}"
        );
    }
}

/// A copy that fails rolls back: what the move made on node-b is deleted, and the source is as it was, never written
/// and never started; its cluster saw only reads.
#[test]
fn a_failed_ceph_copy_rolls_back_and_leaves_the_source_untouched() {
    let ceph = Arc::new(CephFake::default());
    let rig = Rig::with_ceph(
        vec![vm()],
        json!({ "cluster": "west", "client": "arca-move" }),
        Arc::clone(&ceph),
    );
    ceph_pools(&rig, "east", "west");
    vm_images(&ceph, "ceph", "incus-east");
    vm_images(&ceph, "west", "incus-west");
    *ceph.fail_copy.lock().unwrap() = Some("rbd failed: import-diff: connection reset".to_string());
    let (status, _) = move_db(&rig);
    let one = &status["instances"][0];
    assert_eq!(one["result"], "rolled_back", "{one}");
    assert!(
        one["reason"]
            .as_str()
            .unwrap()
            .contains("rbd failed: import-diff: connection reset"),
        "{one}"
    );
    let rolled = rollback_line(&rig, "db");
    assert_eq!(rolled["targetRemoved"], true, "{rolled}");
    assert_eq!(rolled["sourceIntact"], true, "{rolled}");
    assert!(!rig.target.holds("instance", "shop", "db"));
    assert_eq!(
        rig.source.instance_status("shop", "db").as_deref(),
        Some("Stopped")
    );
    assert!(rig.source.writes().is_empty(), "{:?}", rig.source.writes());
    assert_eq!(
        ceph.on("ceph"),
        ["status", "info", "status", "info", "export-diff"]
    );
    // The link's own client writes node-b's pool.
    let import = ceph
        .calls()
        .into_iter()
        .find(|argv| argv[6] == "import-diff")
        .unwrap();
    assert_eq!(import[2..5], ["west", "--id", "arca-move"]);
}

/// A disk someone holds open may belong to a machine that runs: nothing is made or copied.
#[test]
fn a_virtual_machine_whose_disk_is_held_open_is_not_copied() {
    let ceph = Arc::new(CephFake::default());
    let rig = Rig::with_ceph(vec![vm()], json!({ "cluster": "west" }), Arc::clone(&ceph));
    ceph_pools(&rig, "east", "west");
    vm_images(&ceph, "ceph", "incus-east");
    ceph.watchers.lock().unwrap().insert(
        "ceph:incus-east/virtual-machine_shop_db.block".to_string(),
        vec!["192.0.2.77:0/3".to_string()],
    );
    let (status, _) = move_db(&rig);
    let one = &status["instances"][0];
    assert_eq!(one["result"], "rolled_back", "{one}");
    assert!(
        one["reason"]
            .as_str()
            .unwrap()
            .contains("is held open by 192.0.2.77:0/3"),
        "{one}"
    );
    assert_eq!(ceph.on("ceph"), ["status", "info", "status"]);
    assert!(ceph.on("west").is_empty());
    assert!(!rig.target.holds("instance", "shop", "db"));
    assert!(rig.source.writes().is_empty(), "{:?}", rig.source.writes());
}

/// The move counts only when the copy answers: a guest with no agent and no network card to take an address is rolled
/// back, and the source stays as it was.
#[test]
fn a_virtual_machine_copy_whose_guest_does_not_answer_is_rolled_back() {
    let mut quiet = vm();
    quiet["devices"].as_object_mut().unwrap().remove("eth0");
    let ceph = Arc::new(CephFake::default());
    let rig = Rig::with_ceph(vec![quiet], json!({ "cluster": "west" }), Arc::clone(&ceph));
    ceph_pools(&rig, "east", "west");
    vm_images(&ceph, "ceph", "incus-east");
    vm_images(&ceph, "west", "incus-west");
    rig.target.agentless("shop", "db");
    let (status, _) = move_db(&rig);
    let one = &status["instances"][0];
    assert_eq!(one["result"], "rolled_back", "{one}");
    assert!(
        one["reason"]
            .as_str()
            .unwrap()
            .contains("guestAgent: the guest did not answer"),
        "{one}"
    );
    assert!(!rig.target.holds("instance", "shop", "db"));
    assert_eq!(
        rig.source.instance_status("shop", "db").as_deref(),
        Some("Stopped")
    );
    assert!(rig.source.writes().is_empty(), "{:?}", rig.source.writes());
}

/// `db` as `vm()`, its card taking its address from a lease, not from its device.
fn vm_on_lease() -> Value {
    let mut db = vm();
    db["devices"]["eth0"] = json!({ "type": "nic", "network": "arcabr0" });
    db
}

/// A ready rig moving `db` (or `machine`) by Ceph from the cluster east to west, every image of the source's size.
fn ceph_rig(machine: Value) -> (Rig, Arc<CephFake>) {
    let ceph = Arc::new(CephFake::default());
    let rig = Rig::with_ceph(
        vec![machine],
        json!({ "cluster": "west" }),
        Arc::clone(&ceph),
    );
    ceph_pools(&rig, "east", "west");
    vm_images(&ceph, "ceph", "incus-east");
    vm_images(&ceph, "west", "incus-west");
    (rig, ceph)
}

/// Makes `count` snapshots of `db` in `shop` on `fake`, as `incus snapshot create` does, by Incus's own API.
fn snapshot_db(fake: &FakeIncus, count: usize) {
    use std::io::Read;
    for index in 0..count {
        let body = json!({ "name": format!("snap{index}") }).to_string();
        let mut stream =
            std::os::unix::net::UnixStream::connect(fake.socket()).expect("the fake listens");
        write!(
            stream,
            "POST /1.0/instances/db/snapshots?project=shop HTTP/1.1\r\nHost: incus\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .expect("sent");
        let mut answer = String::new();
        stream.read_to_string(&mut answer).expect("answered");
        assert!(answer.starts_with("HTTP/1.1 20"), "{answer}");
    }
    assert_eq!(
        writer(fake)
            .get("/1.0/instances/db/snapshots?project=shop")
            .expect("listed")
            .as_array()
            .map(Vec::len),
        Some(count)
    );
}

/// N1 (review 2): a copy by Ceph takes no snapshot, so it is sized without them. A 1 GiB machine with three snapshots fits
/// node-b's 4 GiB pool: it needs its disk and its config image (Incus's 500 MiB default) with 10% headroom, not the 4.4
/// GiB its snapshots counted in full would make, and it moves.
#[test]
fn a_ceph_copy_is_sized_without_the_snapshots_it_does_not_take() {
    let (rig, _ceph) = ceph_rig(vm());
    snapshot_db(&rig.source, 3);
    let (status, plan) = move_db(&rig);
    let instance = &plan["plan"]["instances"][0];
    assert_eq!(instance["copy"], "ceph", "{plan}");
    let copied = GIB + 500 * MIB;
    assert_eq!(instance["bytes"]["measured"], copied, "{instance}");
    assert_eq!(
        instance["bytes"]["needed"],
        copied + copied / 10,
        "{instance}"
    );
    assert_eq!(instance["bytes"]["snapshots"], 3 * GIB, "{instance}");
    let rows = card(&plan);
    assert!(
        rows.iter()
            .any(|(_, words)| words.contains("its snapshots are not counted, as none is copied")),
        "{rows:?}"
    );
    assert_eq!(status["instances"][0]["result"], "moved", "{status}");
}

/// The same machine on the Incus push, which sends its snapshots, still counts them in full: 4.4 GiB do not fit 4 GiB.
#[test]
fn an_incus_push_still_counts_the_snapshots_it_sends() {
    let ceph = Arc::new(CephFake::default());
    let rig = Rig::with_ceph(vec![vm()], Value::Null, Arc::clone(&ceph));
    ceph_pools(&rig, "one", "two");
    snapshot_db(&rig.source, 3);
    let plan = rig.plan(&["db"]);
    assert_eq!(plan["go"], false, "{plan}");
    let reasons = plan.to_string();
    assert!(reasons.contains("counted in full"), "{reasons}");
}

/// H1: without its agent, Incus lists for a guest that never booted the address it derives from the card's MAC (and its
/// leases): those are not an answer. Only an address that answers a connection from this node is.
#[test]
fn a_guest_that_never_booted_is_not_counted_by_the_addresses_incus_lists_for_it() {
    // The fake lists them as Incus does: the device's address and the EUI-64 one, with no agent.
    let mut running = vm();
    running["status"] = json!("Running");
    let probe = Rig::new(vec![running], 100);
    probe.source.agentless("shop", "db");
    let state = writer(&probe.source)
        .get("/1.0/instances/db/state?project=shop")
        .expect("state");
    assert_eq!(state["processes"], -1);
    let listed = state["network"]["eth0"]["addresses"].to_string();
    assert!(listed.contains("192.0.2.59"), "{listed}");
    assert!(
        listed.contains("fd42:4242:4242:4242:216:3eff:fe00:9"),
        "{listed}"
    );
    // The copy lists both and answers on neither: it is rolled back.
    let (rig, _ceph) = ceph_rig(vm());
    rig.target.agentless("shop", "db");
    let (status, _) = move_db(&rig);
    let one = &status["instances"][0];
    assert_eq!(one["result"], "rolled_back", "{one}");
    let reason = one["reason"].as_str().unwrap();
    assert!(
        reason.contains("none of its addresses answered a connection from this node"),
        "{reason}"
    );
    let probed = rig.probed.lock().unwrap().clone();
    assert!(
        probed.iter().any(|address| address == "192.0.2.59"),
        "{probed:?}"
    );
    assert!(
        probed
            .iter()
            .any(|address| address == "fd42:4242:4242:4242:216:3eff:fe00:9"),
        "{probed:?}"
    );
    assert!(!rig.target.holds("instance", "shop", "db"));
    assert_eq!(
        rig.source.instance_status("shop", "db").as_deref(),
        Some("Stopped")
    );
}

/// A guest without the agent answers when one of its addresses takes a connection from this node, accepted or refused:
/// here the probe answers at its lease's address.
#[test]
fn a_guest_without_the_agent_answers_by_an_address_that_takes_a_connection() {
    let (rig, _ceph) = ceph_rig(vm_on_lease());
    rig.target.agentless("shop", "db");
    rig.target
        .set_lease("arcabr0", "00:16:3e:00:00:09", "192.0.2.77");
    rig.answering.lock().unwrap().push("192.0.2.77".to_string());
    let (status, _) = move_db(&rig);
    let one = &status["instances"][0];
    assert_eq!(one["result"], "moved", "{one}");
    let agent = one["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["check"] == "guestAgent")
        .unwrap();
    assert_eq!(
        agent["detail"],
        "the guest answers: eth0's address 192.0.2.77 answers a connection from this node (port 22)"
    );
}

/// M3: a source started and stopped again while its images were read leaves a torn copy, though it reads stopped at
/// the end: its `last_used_at` moved, and the copy is rolled back.
#[test]
fn a_source_started_and_stopped_during_a_ceph_copy_rolls_the_copy_back() {
    let (rig, ceph) = ceph_rig(vm());
    let socket = rig.source.socket().to_path_buf();
    *ceph.during_copy.lock().unwrap() = Some(Box::new(move || {
        let source = arca_incus::MoveWriter::new(socket.clone());
        source
            .set_state("shop", "db", "start", false, Duration::from_secs(5))
            .expect("started");
        source
            .set_state("shop", "db", "stop", true, Duration::from_secs(5))
            .expect("stopped");
    }));
    let (status, _) = move_db(&rig);
    let one = &status["instances"][0];
    assert_eq!(one["result"], "rolled_back", "{one}");
    assert!(
        one["reason"]
            .as_str()
            .unwrap()
            .contains("was started here while it was copied"),
        "{one}"
    );
    assert!(!rig.target.holds("instance", "shop", "db"));
    assert_eq!(
        rig.source.instance_status("shop", "db").as_deref(),
        Some("Stopped")
    );
}

/// Two sides that name one OSD pool of one cluster: Incus would make the copy's images beside the source's own.
#[test]
fn a_ceph_copy_into_the_sources_own_osd_pool_is_refused() {
    let ceph = Arc::new(CephFake::default());
    let rig = Rig::with_ceph(vec![vm()], json!({ "cluster": "ceph" }), Arc::clone(&ceph));
    ceph_pools(&rig, "east", "east");
    let answer = rig.plan(&["db"]);
    assert_eq!(answer["go"], false, "{answer}");
    let reasons = answer["blocked"][0]["reasons"].to_string();
    assert!(
        reasons.contains("name one OSD pool, incus-east, of one Ceph cluster, ceph"),
        "{reasons}"
    );
    assert!(ceph.calls().is_empty());
}

/// An ISO volume cannot be made empty by Incus: such a machine goes by the Incus push, not refused.
#[test]
fn a_virtual_machine_with_an_iso_volume_goes_by_the_incus_push() {
    let ceph = Arc::new(CephFake::default());
    let rig = Rig::with_ceph(
        vec![vm_with_volume()],
        json!({ "cluster": "west" }),
        Arc::clone(&ceph),
    );
    ceph_pools(&rig, "east", "west");
    rig.source.add_volume(
        "default",
        "default",
        "db-data",
        json!({ "content_type": "iso", "config": { "size": "512MiB" } }),
    );
    let answer = rig.plan(&["db"]);
    assert_eq!(answer["go"], true, "{answer}");
    assert_eq!(answer["plan"]["instances"][0]["copy"], "incus");
}

/// An image Incus made at another size than the source's is not written: the copy stops there and is rolled back.
#[test]
fn a_made_image_of_another_size_is_not_copied_into() {
    let (rig, ceph) = ceph_rig(vm());
    ceph.image("west", "incus-west/virtual-machine_shop_db.block", 2 * GIB);
    let (status, _) = move_db(&rig);
    let one = &status["instances"][0];
    assert_eq!(one["result"], "rolled_back", "{one}");
    assert!(
        one["reason"]
            .as_str()
            .unwrap()
            .contains("nothing was copied into it"),
        "{one}"
    );
    let imported: Vec<String> = ceph
        .calls()
        .into_iter()
        .filter(|argv| argv[6] == "import-diff")
        .map(|argv| argv[8].clone())
        .collect();
    assert_eq!(
        imported,
        ["incus-west/virtual-machine_shop_db"],
        "only the config image"
    );
    assert!(!rig.target.holds("instance", "shop", "db"));
}

/// A machine started after its plan is not copied: the run reads it stopped first.
#[test]
fn a_virtual_machine_started_after_its_plan_is_not_moved() {
    let (rig, ceph) = ceph_rig(vm());
    let plan = rig.plan(&["db"]);
    assert_eq!(plan["go"], true, "{plan}");
    let proposal = rig.propose_move(&plan).expect("proposed");
    rig.source.set_instance_status("shop", "db", "Running");
    if let Ok(started) = rig.approve(&proposal) {
        let status = rig.wait(started["move"].as_str().unwrap());
        assert_ne!(status["instances"][0]["result"], "moved", "{status}");
    }
    assert!(ceph.on("west").is_empty(), "{:?}", ceph.calls());
    assert!(!rig.target.holds("instance", "shop", "db"));
}

/// A virtual machine started after the owner approved (the approval's own checks read it Stopped) fails at the run's
/// first read of it: nothing is copied or made, and the source is not touched. The approval's checks read the machine
/// a number of times this test does not fix, so it starts the machine at each read in turn, on a fresh rig, until the
/// approval passes and the run is the first to see it Running.
#[test]
fn a_virtual_machine_started_after_its_approval_is_not_moved() {
    let reads = |rig: &Rig| {
        rig.source
            .requests()
            .iter()
            .filter(|request| {
                request.method == "GET" && request.path.starts_with("/1.0/instances/db")
            })
            .count() as u32
    };
    let (probe, _) = ceph_rig(vm());
    let before = reads(&probe);
    assert_eq!(probe.plan(&["db"])["go"], true);
    let by_plan = reads(&probe) - before;
    for at in 0..=by_plan + 4 {
        let (rig, ceph) = ceph_rig(vm());
        let plan = rig.plan(&["db"]);
        let proposal = rig.propose_move(&plan).expect("proposed");
        rig.source.started_after_reads("shop", "db", at);
        let Ok(started) = rig.approve(&proposal) else {
            continue; // the approval's own checks saw it Running
        };
        let status = rig.wait(started["move"].as_str().unwrap());
        let one = &status["instances"][0];
        assert_ne!(one["result"], "moved", "{status}");
        let reason = one["reason"].as_str().unwrap_or_default();
        assert!(
            reason.contains("the virtual machine reads Running, not Stopped"),
            "the run's first read, not a later check, must see it Running: {status}"
        );
        assert!(ceph.calls().is_empty(), "{:?}", ceph.calls());
        assert!(!rig.target.holds("instance", "shop", "db"));
        assert!(rig.source.writes().is_empty(), "{:?}", rig.source.writes());
        return;
    }
    panic!("no read of the machine let the approval pass and the run see it Running");
}

/// A failed copy of a machine with a custom volume deletes the volume the move made on node-b; the source's volume is
/// kept and only read.
#[test]
fn a_failed_ceph_copy_deletes_the_volume_it_made_and_keeps_the_sources() {
    let (rig, ceph) = ceph_rig(vm_with_volume());
    rig.source.add_volume(
        "default",
        "default",
        "db-data",
        json!({ "content_type": "block", "config": { "size": "512MiB" } }),
    );
    ceph.image("ceph", "incus-east/custom_db-data.block", 512 * MIB);
    ceph.image("west", "incus-west/custom_db-data.block", 512 * MIB);
    *ceph.fail_copy.lock().unwrap() = Some("rbd failed: import-diff: no space left".to_string());
    let (status, _) = move_db(&rig);
    let one = &status["instances"][0];
    assert_eq!(one["result"], "rolled_back", "{one}");
    assert!(!rig.target.holds("volume", "default", "db-data"));
    assert!(rig.source.holds("volume", "default", "db-data"));
    assert!(rig.source.writes().is_empty(), "{:?}", rig.source.writes());
    assert_eq!(
        ceph.on("ceph").last().map(String::as_str),
        Some("export-diff")
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
    // Incus reports its progress in decimal units, as text: "web: 40.00MB (...)".
    rig.source.set_push_bytes("instance:shop/web", 40_000_000);
    rig.source
        .set_push_bytes("volume:default/default/web-data", 1_000_000);
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
    // The rounds while it runs do not freeze it; the last one, after the stop, is consistent.
    assert!(pushes[1].ends_with(" (inconsistent)"), "{pushes:?}");
    assert!(pushes[3].ends_with(" (inconsistent)"), "{pushes:?}");
    assert!(pushes[5].starts_with("instance:shop/web -> "));
    assert!(!pushes[5].ends_with(" (inconsistent)"), "{pushes:?}");
    let rounds: Vec<Value> = rig
        .journal()
        .into_iter()
        .filter(|l| l["event"] == "round")
        .collect();
    assert_eq!(rounds.len(), 3);
    assert_eq!(rounds[0]["refresh"], false);
    assert_eq!(rounds[1]["refresh"], true);
    assert_eq!(rounds[2]["final"], true);
    assert_eq!(rounds[0]["bytes"], json!(41_000_000));
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
    // The plan is journaled, whole, with the begin of the move that runs it.
    assert_eq!(&events[..2], ["plan", "begin"]);
    assert_eq!(events.last().map(String::as_str), Some("end"));
    for event in ["round", "cutover", "verify", "result"] {
        assert!(events.iter().any(|e| e == event), "{event}: {events:?}");
    }
}

#[test]
fn progress_is_readable_during_a_slow_round_and_reading_it_writes_nothing() {
    let rig = Rig::new(vec![web()], 100);
    rig.source.set_push_bytes("instance:shop/web", 40_000_000);
    rig.source.plan_push(PushPlan::Hold {
        processed: 10_000_000,
        speed: 5_000_000,
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
        // Read from the text Incus writes in the operation's fs_progress: "web: 10.00MB (5.00MB/s)".
        if status["instances"][0]["bytesMoved"] == json!(10_000_000) {
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
    assert_eq!(one["rateBytesPerSecond"], json!(5_000_000));
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
    let before = rig.journal().len();
    let again = rig.plan(&["api"]);
    assert_eq!(again["go"], true, "{again}");
    assert_eq!(rig.journal().len(), before, "a plan adds no journal line");
    // The next line starts on its own after the torn tail.
    let id = rig.start(&["api"])["move"].as_str().unwrap().to_string();
    assert_eq!(rig.wait(&id)["phase"], "moved");
}

/// The uuid `container` gives the source's `web`.
const WEB_UUID: &str = "00000000-0000-4000-8000-000000000001";

/// Puts a copy of `name` on the target as a move's first round would leave it; answers its uuid.
fn copy_on_target(rig: &Rig, name: &str, status: &str) -> String {
    rig.target.add_instance(json!({
        "name": name, "project": "shop", "type": "container", "status": status,
        "devices": { "root": { "type": "disk", "path": "/", "pool": "default" } },
    }));
    arca_incus::MoveWriter::new(rig.target.socket().to_path_buf())
        .get(&format!("/1.0/instances/{name}?project=shop"))
        .expect("the copy")["config"]["volatile.uuid"]
        .as_str()
        .expect("a uuid")
        .to_string()
}

/// Journals a move of `web` that a stopped kernel left after the lines `steps` (each without move, project and name).
fn interrupted(rig: &Rig, id: &str, steps: Vec<Value>) {
    interrupted_to(rig, id, "node-b", steps);
}

/// [`interrupted`], to the linked host `target` (or to a host that is no longer linked).
fn interrupted_to(rig: &Rig, id: &str, target: &str, steps: Vec<Value>) {
    let journal = rig.moves.journal();
    journal
        .append(&json!({
            "event": "begin", "at": NOW, "move": id, "plan": "ab".repeat(32), "target": target,
            "proposal": null, "instances": [{
                "project": "shop", "name": "web", "uuid": WEB_UUID, "roundsTotal": 3,
                "bytesTotal": 2 * MIB, "needed": 3 * MIB, "targetPool": "default", "volumes": [],
            }],
        }))
        .unwrap();
    for mut step in steps {
        step["at"] = json!(NOW);
        step["move"] = json!(id);
        step["project"] = json!("shop");
        step["name"] = json!("web");
        journal.append(&step).unwrap();
    }
}

#[test]
fn a_move_a_stopped_kernel_left_unfinished_is_rolled_back_on_the_next_start() {
    let rig = Rig::new(vec![web()], 100);
    // The kernel stopped in the cutover: the source stopped, the copy it recorded on the target, stopped.
    arca_incus::MoveWriter::new(rig.source.socket().to_path_buf())
        .set_state("shop", "web", "stop", true, Duration::from_secs(5))
        .expect("stopped");
    let uuid = copy_on_target(&rig, "web", "Stopped");
    interrupted(
        &rig,
        "mv-0123456789ab",
        vec![
            json!({ "event": "created", "targetUuid": uuid }),
            json!({ "event": "cutover", "step": "stopped", "wasRunning": true }),
        ],
    );
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
    assert!(
        !rig.target.holds("instance", "shop", "web"),
        "the recorded copy is removed"
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

// ---- The review of ab7427f: each test below fails without the fix it names. ----

fn writer(fake: &FakeIncus) -> arca_incus::MoveWriter {
    arca_incus::MoveWriter::new(fake.socket().to_path_buf())
}

fn uuid_on(fake: &FakeIncus, name: &str) -> Option<String> {
    writer(fake)
        .get(&format!("/1.0/instances/{name}?project=shop"))
        .ok()
        .and_then(|doc| doc["config"]["volatile.uuid"].as_str().map(str::to_string))
}

/// Points the target's server document at `addresses`, with `certificate`.
fn target_listens(rig: &Rig, addresses: &[&str], certificate: &str) {
    rig.target.set_server(json!({
        "api_version": "1.0",
        "environment": {
            "architectures": ["x86_64"], "driver": "lxc | qemu", "server_name": "fake",
            "addresses": addresses, "certificate": certificate,
        },
    }));
}

/// Waits until the source holds a push the test planned to hold; answers it.
fn held(rig: &Rig, count: usize) -> arca_incus::OpId {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        let held = rig.source.held_pushes();
        if held.len() >= count {
            return held[count - 1].clone();
        }
        assert!(Instant::now() < until, "the held push never started");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn hold() -> PushPlan {
    PushPlan::Hold {
        processed: 1_000_000,
        speed: 1_000_000,
    }
}

fn result_line(rig: &Rig, name: &str) -> Value {
    rig.journal()
        .into_iter()
        .rev()
        .find(|l| l["event"] == "result" && l["name"] == name)
        .unwrap_or_else(|| panic!("no result for {name}"))
}

fn rollback_line(rig: &Rig, name: &str) -> Value {
    rig.journal()
        .into_iter()
        .rev()
        .find(|l| l["event"] == "rollback" && l["name"] == name)
        .unwrap_or_else(|| panic!("no rollback for {name}"))
}

/// H2: an ephemeral container is deleted by Incus when it stops, so the cutover would delete the source. M5: a spot-check
/// that cannot be read on the source would pass by never comparing.
#[test]
fn a_plan_blocks_an_ephemeral_container_and_an_unreadable_spot_check() {
    let mut ephemeral = web();
    ephemeral["ephemeral"] = json!(true);
    let rig = Rig::new(vec![ephemeral, api()], 100);
    let plan = rig.plan(&["web"]);
    assert_eq!(plan["go"], false, "{plan}");
    assert!(
        plan["blocked"][0]["reasons"]
            .to_string()
            .contains("ephemeral"),
        "{plan}"
    );
    let plan = rig.plan_with(
        &["api"],
        json!({ "instances": [{ "project": "shop", "name": "api", "spotCheck": "/srv/missing" }] }),
    );
    assert_eq!(plan["go"], false, "{plan}");
    assert!(
        plan["blocked"][0]["reasons"]
            .to_string()
            .contains("spot-check file /srv/missing"),
        "{plan}"
    );
}

/// H2: the source is gone when the last round fails: the copy on the target is the only one, and the rollback keeps it.
#[test]
fn a_rollback_keeps_the_copy_when_the_source_is_not_read_back() {
    let rig = Rig::new(vec![web()], 100);
    rig.source.plan_push(PushPlan::Pass);
    rig.source.plan_push(PushPlan::Pass);
    rig.source.plan_push(hold());
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    let last = held(&rig, 1);
    // The source is stopped by the cutover; something deletes it while the last delta is in flight.
    writer(&rig.source)
        .delete_instance("shop", "web")
        .expect("deleted");
    rig.source.fail_operation(&last, "the link dropped");
    let status = rig.wait(&id);
    assert_eq!(status["instances"][0]["result"], "failed", "{status}");
    assert_eq!(status["phase"], "needs_owner");
    assert!(
        rig.target.holds("instance", "shop", "web"),
        "the only copy is kept"
    );
    let rollback = rollback_line(&rig, "web");
    assert_eq!(rollback["sourceIntact"], false);
    assert_eq!(rollback["targetRemoved"], false);
}

/// H5: an instance on the target that this move did not record making is never deleted by its rollback.
#[test]
fn a_rollback_leaves_an_object_this_move_did_not_make() {
    let rig = Rig::new(vec![web()], 100);
    rig.source.plan_push(hold());
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    let first = held(&rig, 1);
    // Someone creates their own shop/web on the target before the first round lands; the round then fails.
    rig.target
        .add_instance(json!({ "name": "web", "project": "shop", "type": "container" }));
    let theirs = uuid_on(&rig.target, "web").expect("their uuid");
    rig.source.fail_operation(&first, "the link dropped");
    let status = rig.wait(&id);
    assert_eq!(
        uuid_on(&rig.target, "web").as_deref(),
        Some(theirs.as_str()),
        "theirs is kept"
    );
    assert_eq!(status["instances"][0]["result"], "failed", "{status}");
    assert_eq!(
        rig.source.instance_status("shop", "web").as_deref(),
        Some("Running"),
        "the source was never stopped"
    );
    assert!(
        rollback_line(&rig, "web")["detail"]
            .to_string()
            .contains("not a copy this move recorded making")
    );
}

/// H4: the target cannot be read at the rollback, so a copy may run there: the source is not started next to it.
#[test]
fn a_rollback_does_not_start_the_source_while_the_target_cannot_be_read() {
    let rig = Rig::new(vec![web()], 100);
    rig.source.plan_push(PushPlan::Pass);
    rig.source.plan_push(PushPlan::Pass);
    rig.source.plan_push(hold());
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    let last = held(&rig, 1);
    rig.target.refuse_start("shop", "web");
    rig.target.fail_reads("/1.0/instances/web", 1000, 500);
    rig.source.release_operation(&last);
    let status = rig.wait(&id);
    assert_eq!(status["instances"][0]["result"], "failed", "{status}");
    assert_eq!(
        rig.source.instance_status("shop", "web").as_deref(),
        Some("Stopped"),
        "never started next to a copy that may run"
    );
    assert_eq!(rollback_line(&rig, "web")["sourceRunning"], false);
}

/// H4: whether the source runs is read at the cutover. api was stopped when the batch began and started during web's
/// move; its cutover stops it, so it never runs next to its copy.
#[test]
fn a_source_started_during_the_batch_is_stopped_at_its_cutover() {
    let rig = Rig::new(vec![web(), api()], 100);
    writer(&rig.source)
        .set_state("shop", "api", "stop", true, Duration::from_secs(5))
        .expect("stopped");
    rig.source.plan_push(hold());
    let id = rig.start(&["web", "api"])["move"]
        .as_str()
        .unwrap()
        .to_string();
    let first = held(&rig, 1);
    writer(&rig.source)
        .set_state("shop", "api", "start", false, Duration::from_secs(5))
        .expect("started");
    rig.source.release_operation(&first);
    let status = rig.wait(&id);
    assert_eq!(status["phase"], "moved", "{status}");
    assert_eq!(
        rig.target.instance_status("shop", "api").as_deref(),
        Some("Running")
    );
    assert_eq!(
        rig.source.instance_status("shop", "api").as_deref(),
        Some("Stopped"),
        "never both running"
    );
    let stopped = rig
        .journal()
        .into_iter()
        .find(|l| l["event"] == "cutover" && l["name"] == "api" && l["step"] == "stopped")
        .unwrap();
    assert_eq!(stopped["wasRunning"], true);
}

/// H4: a second plan of an instance a move holds does not start, and a plan that ran is not proposed again.
#[test]
fn an_instance_another_move_holds_does_not_move_twice() {
    let rig = Rig::new(vec![web()], 100);
    let one = rig.plan_with(&["web"], json!({ "rounds": 1 }));
    let two = rig.plan_with(&["web"], json!({ "rounds": 2 }));
    assert_ne!(one["digest"], two["digest"]);
    let first = rig.propose_move(&one).expect("proposed");
    let second = rig.propose_move(&two).expect("proposed");
    rig.source.plan_push(hold());
    let id = rig.approve(&first).expect("started")["move"]
        .as_str()
        .unwrap()
        .to_string();
    let pushed = held(&rig, 1);
    assert_eq!(refusal_code(rig.approve(&second)), "InstanceMoving");
    assert_eq!(refusal_code(rig.propose_move(&two)), "InstanceMoving");
    rig.source.release_operation(&pushed);
    assert_eq!(rig.wait(&id)["phase"], "moved");
    assert_eq!(refusal_code(rig.propose_move(&one)), "AlreadyRun");
    assert_eq!(
        rig.source.pushes().len(),
        2,
        "one move ran: {:?}",
        rig.source.pushes()
    );
}

/// H3: the finish reads the copy back on the target as the one this move made before it deletes a source.
#[test]
fn a_finish_keeps_the_source_when_its_copy_is_not_the_one_this_move_made() {
    let rig = Rig::new(vec![web()], 100);
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    assert_eq!(rig.wait(&id)["phase"], "moved");
    // The copy is replaced on the target by another shop/web.
    let target = writer(&rig.target);
    target
        .set_state("shop", "web", "stop", true, Duration::from_secs(5))
        .expect("stopped");
    target.delete_instance("shop", "web").expect("deleted");
    rig.target
        .add_instance(json!({ "name": "web", "project": "shop", "type": "container" }));
    let proposal = rig.propose_finish(&id).expect("proposed");
    let finished = rig.approve(&proposal).expect("finished");
    assert_eq!(finished["deleted"], json!([]), "{finished}");
    assert!(
        finished["kept"][0]
            .as_str()
            .unwrap()
            .contains("not read back"),
        "{finished}"
    );
    assert!(
        rig.source.holds("instance", "shop", "web"),
        "the source is kept"
    );
}

/// H3: a source that runs again, and a volume another instance uses, are kept by the finish.
#[test]
fn a_finish_keeps_a_running_source_and_a_volume_in_use() {
    let rig = Rig::new(vec![web()], 100);
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    assert_eq!(rig.wait(&id)["phase"], "moved");
    writer(&rig.source)
        .set_state("shop", "web", "start", false, Duration::from_secs(5))
        .expect("started");
    let finished = rig
        .approve(&rig.propose_finish(&id).expect("proposed"))
        .expect("finished");
    assert!(
        finished["kept"][0].as_str().unwrap().contains("runs again"),
        "{finished}"
    );
    assert!(rig.source.holds("instance", "shop", "web"));

    let rig = Rig::new(vec![web_with_volume()], 100);
    rig.source
        .add_volume("default", "default", "web-data", json!({}));
    rig.files("custom/default_web-data", 1);
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    assert_eq!(rig.wait(&id)["phase"], "moved");
    // Another instance on this host mounts the same volume.
    rig.source.add_instance(json!({
        "name": "batch", "project": "shop", "type": "container",
        "devices": { "data": { "type": "disk", "pool": "default", "source": "web-data", "path": "/data" } },
    }));
    let finished = rig
        .approve(&rig.propose_finish(&id).expect("proposed"))
        .expect("finished");
    assert_eq!(finished["deleted"], json!(["shop/web"]), "{finished}");
    assert!(
        finished["kept"][0]
            .as_str()
            .unwrap()
            .contains("another instance uses it"),
        "{finished}"
    );
    assert!(rig.source.holds("volume", "default", "web-data"));
}

/// M1: a copy whose checks passed is recorded as moved on the next start; nothing is deleted or started.
#[test]
fn a_verified_copy_a_stopped_kernel_left_is_recorded_as_moved() {
    let rig = Rig::new(vec![web()], 100);
    writer(&rig.source)
        .set_state("shop", "web", "stop", true, Duration::from_secs(5))
        .expect("stopped");
    let uuid = copy_on_target(&rig, "web", "Running");
    interrupted(
        &rig,
        "mv-0123456789ab",
        vec![
            json!({ "event": "created", "targetUuid": uuid }),
            json!({ "event": "cutover", "step": "stopped", "wasRunning": true }),
            json!({ "event": "cutover", "step": "starting" }),
            json!({ "event": "cutover", "step": "started" }),
            json!({ "event": "verify", "ok": true, "checks": [], "downtimeMs": 1234 }),
        ],
    );
    rig.moves.recover();
    let status = rig.wait("mv-0123456789ab");
    assert_eq!(status["phase"], "moved", "{status}");
    assert_eq!(status["instances"][0]["downtimeMs"], 1234);
    assert_eq!(
        uuid_on(&rig.target, "web").as_deref(),
        Some(uuid.as_str()),
        "the copy is kept"
    );
    assert_eq!(
        rig.source.instance_status("shop", "web").as_deref(),
        Some("Stopped")
    );
    assert!(rig.journal().iter().all(|l| l["event"] != "rollback"));
}

/// M1: a copy that was started may have served writes: the next start keeps it, leaves the source stopped and names it
/// for the owner.
#[test]
fn a_started_copy_a_stopped_kernel_left_is_kept_for_the_owner() {
    let rig = Rig::new(vec![web()], 100);
    writer(&rig.source)
        .set_state("shop", "web", "stop", true, Duration::from_secs(5))
        .expect("stopped");
    let uuid = copy_on_target(&rig, "web", "Running");
    interrupted(
        &rig,
        "mv-0123456789ab",
        vec![
            json!({ "event": "created", "targetUuid": uuid }),
            json!({ "event": "cutover", "step": "stopped", "wasRunning": true }),
            json!({ "event": "cutover", "step": "starting" }),
        ],
    );
    rig.moves.recover();
    let status = rig.wait("mv-0123456789ab");
    assert_eq!(status["phase"], "needs_owner", "{status}");
    assert_eq!(status["instances"][0]["result"], "failed");
    assert!(
        status["instances"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("may have served writes")
    );
    assert_eq!(
        rig.target.instance_status("shop", "web").as_deref(),
        Some("Running")
    );
    assert_eq!(
        rig.source.instance_status("shop", "web").as_deref(),
        Some("Stopped")
    );
}

/// M1: an instance whose result is journaled is settled as it is: a moved one is not rolled back.
#[test]
fn the_next_start_does_not_roll_back_an_instance_that_moved() {
    let rig = Rig::new(vec![web()], 100);
    writer(&rig.source)
        .set_state("shop", "web", "stop", true, Duration::from_secs(5))
        .expect("stopped");
    let uuid = copy_on_target(&rig, "web", "Running");
    interrupted(
        &rig,
        "mv-0123456789ab",
        vec![
            json!({ "event": "created", "targetUuid": uuid }),
            json!({ "event": "cutover", "step": "stopped", "wasRunning": true }),
            json!({ "event": "result", "result": "moved", "downtimeMs": 900, "targetUuid": uuid }),
        ],
    );
    rig.moves.recover();
    let status = rig.wait("mv-0123456789ab");
    assert_eq!(status["phase"], "moved", "{status}");
    assert!(rig.target.holds("instance", "shop", "web"));
    assert_eq!(
        rig.source.instance_status("shop", "web").as_deref(),
        Some("Stopped")
    );
}

/// M4: a proxy on every address that answered here must answer at the target's address; one that answers only on
/// 127.0.0.1 there does not.
#[test]
fn the_ports_check_probes_the_target_address_for_proxies_that_answered_here() {
    for (bind, moves) in [("127.0.0.1:0", false), ("0.0.0.0:0", true)] {
        let listener = std::net::TcpListener::bind(bind).expect("listens");
        let port = listener.local_addr().unwrap().port();
        let mut served = web();
        served["devices"]["site"] = json!({
            "type": "proxy", "listen": format!("tcp:0.0.0.0:{port}"), "connect": "tcp:127.0.0.1:80",
        });
        let rig = Rig::new(vec![served], 100);
        target_listens(&rig, &["127.0.0.2:8443"], CERT);
        let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
        let status = rig.wait(&id);
        let ports = rig
            .journal()
            .into_iter()
            .find(|l| l["event"] == "verify")
            .unwrap()["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["check"] == "ports")
            .cloned()
            .unwrap();
        assert_eq!(ports["ok"], moves, "{bind}: {ports}");
        assert_eq!(
            status["instances"][0]["result"],
            if moves { "moved" } else { "rolled_back" },
            "{bind}: {status}"
        );
        drop(listener);
    }
}

/// M5: a copy without the source's spot-check file fails its checks, and the rollback stops the running copy to delete it.
#[test]
fn a_copy_without_the_spot_check_file_is_stopped_and_removed() {
    let rig = Rig::new(vec![web()], 100);
    rig.source.push_without_files();
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    let status = rig.wait(&id);
    let one = &status["instances"][0];
    assert_eq!(one["result"], "rolled_back", "{status}");
    assert!(
        one["reason"].as_str().unwrap().contains("spotCheck"),
        "{one}"
    );
    assert!(
        !rig.target.holds("instance", "shop", "web"),
        "the running copy was stopped, then deleted"
    );
    assert_eq!(
        rig.source.instance_status("shop", "web").as_deref(),
        Some("Running")
    );
}

/// M4: a card that had an address on the source and has none on the copy fails the network check.
#[test]
fn a_copy_without_its_address_fails_the_network_check() {
    let rig = Rig::new(vec![web()], 100);
    rig.source.set_live_network(
        "shop",
        "web",
        &[("eth0", "00:16:3e:00:00:01", true, &["192.0.2.50"])],
    );
    rig.source.plan_push(PushPlan::Pass);
    rig.source.plan_push(PushPlan::Pass);
    rig.source.plan_push(hold());
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    let last = held(&rig, 1);
    rig.target
        .set_live_network("shop", "web", &[("eth0", "00:16:3e:00:00:01", true, &[])]);
    rig.source.release_operation(&last);
    let status = rig.wait(&id);
    let one = &status["instances"][0];
    assert_eq!(one["result"], "rolled_back", "{status}");
    assert!(
        one["reason"]
            .as_str()
            .unwrap()
            .contains("network: no address on eth0"),
        "{one}"
    );
}

/// M6: a move whose plan and begin cannot be journaled does not begin.
#[test]
fn a_move_that_cannot_be_journaled_does_not_begin() {
    let rig = Rig::new(vec![web()], 100);
    let plan = rig.plan(&["web"]);
    let proposal = rig.propose_move(&plan).expect("proposed");
    fs::create_dir(rig.dir.join("state").join(arca::instance_move::MOVES_FILE))
        .expect("a folder in its place");
    assert_eq!(refusal_code(rig.approve(&proposal)), "JournalUnwritable");
    assert!(rig.source.pushes().is_empty());
    assert!(rig.source.writes().is_empty() && rig.target.writes().is_empty());
}

/// M7: a failed round's receiving operation still holds the copy for a while; the rollback waits for it to end before
/// it deletes, so Incus never answers the delete as busy.
#[test]
fn a_rollback_waits_for_the_receiving_operation_before_it_deletes() {
    // The wait for the operation is bounded by verify_wait: a loaded machine needs more than the rig's 300 ms for the
    // three reads, and the wait ends as soon as the operation does.
    let rig = Rig::waiting(vec![web()], 100, Duration::from_secs(10));
    rig.target.receive_lingers(3);
    rig.source.plan_push(PushPlan::Pass);
    rig.source
        .plan_push(PushPlan::Fail("the link dropped".to_string()));
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    let status = rig.wait(&id);
    assert_eq!(status["instances"][0]["result"], "rolled_back", "{status}");
    assert!(!rig.target.holds("instance", "shop", "web"));
    let deletes: Vec<u16> = rig
        .target
        .writes()
        .iter()
        .filter(|w| w.method == "DELETE")
        .map(|w| w.status)
        .collect();
    assert_eq!(
        deletes,
        [202],
        "one delete, after the operation ended, never answered busy: {deletes:?}"
    );
}

/// M8: the target's addresses are tried in order with the same receiving operation; the one that answered goes first.
#[test]
fn each_target_address_is_tried_in_order() {
    let rig = Rig::new(vec![web()], 100);
    target_listens(&rig, &["192.0.2.99:8443", "203.0.113.20:8443"], CERT);
    rig.target.unreachable_at("192.0.2.99:8443");
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    assert_eq!(rig.wait(&id)["phase"], "moved");
    let pushes = rig.source.pushes();
    assert!(pushes[0].contains("https://192.0.2.99:8443/"), "{pushes:?}");
    assert!(
        pushes[1].contains("https://203.0.113.20:8443/"),
        "{pushes:?}"
    );
    assert_eq!(
        pushes.len(),
        4,
        "the later rounds go to the address that answered: {pushes:?}"
    );
    assert!(
        pushes[2..].iter().all(|p| p.contains("203.0.113.20")),
        "{pushes:?}"
    );
}

/// The node reads the instance again at the run: changed devices or config, or a grown size, refuse it, and nothing moves.
#[test]
fn changed_devices_or_config_or_a_grown_instance_refuse_the_approved_plan() {
    for change in ["devices", "config", "growth"] {
        let rig = Rig::new(vec![web()], 100);
        let plan = rig.plan(&["web"]);
        let proposal = rig.propose_move(&plan).expect("proposed");
        if change == "devices" {
            rig.source.set_instance_device(
                "shop",
                "web",
                "eth0",
                json!({ "type": "nic", "network": "arcabr0", "ipv4.address": "192.0.2.60" }),
            );
        } else if change == "config" {
            rig.source
                .set_instance_config("shop", "web", "limits.cpu", "2");
        } else {
            rig.files("containers/shop_web/rootfs/more", 1);
        }
        let refused = rig.approve(&proposal);
        let Err(CallError::Refused { code, message }) = refused else {
            panic!("{change}: {refused:?}");
        };
        assert_eq!(code, "PlanChanged", "{change}: {message}");
        assert!(
            message.contains(match change {
                "devices" => "its devices",
                "config" => "its config",
                _ => "it grew",
            }),
            "{change}: {message}"
        );
        assert!(rig.source.pushes().is_empty());
    }
}

/// The target's certificate is read again for each instance: a changed one rolls the next instance back.
#[test]
fn a_certificate_changed_during_a_batch_stops_the_next_instance() {
    let rig = Rig::new(vec![web(), api()], 100);
    rig.source.plan_push(hold());
    let id = rig.start(&["web", "api"])["move"]
        .as_str()
        .unwrap()
        .to_string();
    let first = held(&rig, 1);
    target_listens(
        &rig,
        &["203.0.113.20:8443"],
        "-----BEGIN CERTIFICATE-----\nb3RoZXI=\n-----END CERTIFICATE-----\n",
    );
    rig.source.release_operation(&first);
    let status = rig.wait(&id);
    assert_eq!(status["phase"], "partly_moved", "{status}");
    assert_eq!(result_line(&rig, "web")["result"], "moved");
    let api = result_line(&rig, "api");
    assert_eq!(api["result"], "rolled_back");
    assert!(
        api["reason"].as_str().unwrap().contains("certificate"),
        "{api}"
    );
    assert!(!rig.target.holds("instance", "shop", "api"));
}

/// A move to a target counts what the moves running to it may still land there.
#[test]
fn a_second_move_that_would_overfill_the_target_beside_a_running_one_is_refused() {
    let rig = Rig::new(vec![web(), api()], 7);
    rig.files("containers/shop_web/rootfs/more", 2);
    rig.files("containers/shop_api/rootfs/more", 2);
    let web_plan = rig.plan(&["web"]);
    let api_plan = rig.plan(&["api"]);
    assert_eq!(web_plan["go"], true, "{web_plan}");
    assert_eq!(api_plan["go"], true, "{api_plan}");
    let web_card = rig.propose_move(&web_plan).expect("proposed");
    let api_card = rig.propose_move(&api_plan).expect("proposed");
    rig.source.plan_push(hold());
    let id = rig.approve(&web_card).expect("started")["move"]
        .as_str()
        .unwrap()
        .to_string();
    let first = held(&rig, 1);
    assert_eq!(refusal_code(rig.approve(&api_card)), "TargetFull");
    rig.source.release_operation(&first);
    assert_eq!(rig.wait(&id)["phase"], "moved");
}

/// LOW: the kept source does not start by itself on a reboot of this host next to its copy.
#[test]
fn a_moved_source_is_kept_with_autostart_off() {
    let rig = Rig::new(vec![web()], 100);
    rig.source
        .set_instance_config("shop", "web", "boot.autostart", "true");
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    assert_eq!(rig.wait(&id)["phase"], "moved");
    let doc = writer(&rig.source)
        .get("/1.0/instances/web?project=shop")
        .expect("the source");
    assert_eq!(doc["config"]["boot.autostart"], "false");
    assert_eq!(result_line(&rig, "web")["autostartOff"], true);
}

/// M7: a receiving operation that outlasts the wait for it still holds the copy; the rollback's delete is tried again
/// while Incus answers it as busy, and the copy is removed once the operation ends.
#[test]
fn a_busy_delete_is_tried_again_until_the_operation_ends() {
    let rig = Rig::waiting(vec![web()], 100, Duration::from_millis(1));
    rig.target.receive_lingers(4);
    rig.source.plan_push(PushPlan::Pass);
    rig.source
        .plan_push(PushPlan::Fail("the link dropped".to_string()));
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    let status = rig.wait(&id);
    assert_eq!(status["instances"][0]["result"], "rolled_back", "{status}");
    assert!(!rig.target.holds("instance", "shop", "web"));
    // Incus answers a busy delete with an operation that fails; the last attempt is the one that removed it.
    let deletes = rig
        .target
        .writes()
        .iter()
        .filter(|w| w.method == "DELETE" && w.path.starts_with("/1.0/instances/web"))
        .count();
    assert!(
        deletes >= 2,
        "it was answered busy and tried again: {deletes} delete(s)"
    );
}

// ---- The review of 5596780: each test below fails without the fix it names. ----

/// N1: a moved source that is not finished waits here, stopped. Planning it to another host is refused: its copy on
/// node-b runs, and a second copy started on node-c would run beside it with the same address.
#[test]
fn a_moved_source_that_is_not_finished_is_not_moved_again() {
    let rig = Rig::new(vec![web()], 100);
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    assert_eq!(rig.wait(&id)["phase"], "moved");
    rig.source.link_migration(&rig.third);
    let plan = rig
        .call_as(
            Scope::Viewer,
            "instance_move_plan",
            json!({ "instances": [{ "project": "shop", "name": "web" }], "target": "node-c" }),
        )
        .expect("the plan answers");
    assert_eq!(plan["go"], false, "{plan}");
    let reasons = plan["blocked"][0]["reasons"].to_string();
    assert!(reasons.contains(&format!("finish move {id}")), "{reasons}");
    assert!(!rig.third.holds("instance", "shop", "web"));
    assert_eq!(
        rig.source.instance_status("shop", "web").as_deref(),
        Some("Stopped")
    );
}

/// N1: a plan made before the first move ended is refused at its start too.
#[test]
fn a_plan_made_before_its_source_moved_does_not_start_after() {
    let rig = Rig::new(vec![web()], 100);
    let first = rig.plan_with(&["web"], json!({ "rounds": 1 }));
    let second = rig.plan_with(&["web"], json!({ "rounds": 2 }));
    let first_card = rig.propose_move(&first).expect("proposed");
    let second_card = rig.propose_move(&second).expect("proposed");
    let id = rig.approve(&first_card).expect("started")["move"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(rig.wait(&id)["phase"], "moved");
    let Err(CallError::Refused { code, message }) = rig.approve(&second_card) else {
        panic!("the second plan started");
    };
    assert_eq!(code, "AlreadyMoved", "{message}");
    assert!(message.contains(&format!("finish move {id}")), "{message}");
    assert_eq!(rig.source.pushes().len(), 2, "{:?}", rig.source.pushes());
}

/// N2: a source Incus reports Frozen at the cutover (a snapshot freezes it on dir, lvm and ceph) is neither stopped nor
/// left beside a started copy: the move fails safely, deletes only its own copy, and Incus thaws the source.
#[test]
fn a_source_frozen_at_the_cutover_fails_the_move_safely() {
    let mut frozen = web();
    frozen["status"] = json!("Frozen");
    let rig = Rig::new(vec![frozen], 100);
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    let status = rig.wait(&id);
    let one = &status["instances"][0];
    assert_eq!(one["result"], "rolled_back", "{status}");
    let reason = one["reason"].as_str().unwrap();
    assert!(
        reason.contains("Frozen at the cutover"),
        "refused before the stop and the last round: {one}"
    );
    assert!(
        !rig.target.holds("instance", "shop", "web"),
        "its own copy is removed"
    );
    assert_eq!(
        rig.source.instance_status("shop", "web").as_deref(),
        Some("Frozen")
    );
    assert!(
        rig.journal()
            .iter()
            .all(|l| !(l["event"] == "cutover" && l["step"] == "starting")),
        "the copy was never started"
    );
}

/// N2: a source started again during the last round is read back before the copy starts, and the copy is not started.
#[test]
fn a_source_started_during_the_last_round_stops_the_copy_from_starting() {
    let rig = Rig::new(vec![web()], 100);
    rig.source.plan_push(PushPlan::Pass);
    rig.source.plan_push(PushPlan::Pass);
    rig.source.plan_push(hold());
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    let last = held(&rig, 1);
    writer(&rig.source)
        .set_state("shop", "web", "start", false, Duration::from_secs(5))
        .expect("started again");
    rig.source.release_operation(&last);
    let status = rig.wait(&id);
    let one = &status["instances"][0];
    assert_eq!(one["result"], "rolled_back", "{status}");
    assert!(one["reason"].as_str().unwrap().contains("Running"), "{one}");
    assert!(!rig.target.holds("instance", "shop", "web"));
    assert_eq!(
        rig.source.instance_status("shop", "web").as_deref(),
        Some("Running")
    );
}

/// N3: the card estimates the downtime from the whole size, since the last round resends the whole volume today.
#[test]
fn the_card_estimates_the_downtime_from_the_whole_size() {
    let rig = Rig::new(vec![web()], 100);
    let plan = rig.plan(&["web"]);
    let one = &plan["plan"]["instances"][0];
    let measured = one["bytes"]["measured"].as_u64().unwrap();
    assert_eq!(
        one["estimatedDowntimeSeconds"].as_u64().unwrap(),
        30 + measured.div_ceil(50 * MIB)
    );
    assert!(
        !one["rounds"]["order"]
            .to_string()
            .contains("only what changed"),
        "{one}"
    );
    let card = rig.propose_move(&plan).expect("proposed")["details"].to_string();
    assert!(card.contains("resends the whole"), "{card}");
}

/// N5: a kept source whose boot.autostart could not be turned off is named in the result for the owner.
#[test]
fn a_source_whose_autostart_stays_on_is_named_in_the_result() {
    let rig = Rig::new(vec![web()], 100);
    rig.source.set_reject_patch("/1.0/instances/web");
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    let status = rig.wait(&id);
    let one = &status["instances"][0];
    assert_eq!(one["result"], "moved", "{status}");
    assert!(
        one["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("boot.autostart"),
        "{one}"
    );
    assert_eq!(result_line(&rig, "web")["autostartOff"], false);
    // The proposal's late result asks for the owner.
    let late = (0..400).find_map(|_| {
        std::thread::sleep(Duration::from_millis(25));
        let path = find_file(&rig.dir, "action-proposals.jsonl")?;
        let text = fs::read_to_string(path).ok()?;
        text.lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|line| {
                line["code"]
                    .as_str()
                    .is_some_and(|code| code.starts_with("Move"))
            })
    });
    assert_eq!(late.expect("a late result")["code"], "MoveNeedsOwner");
}

fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.file_name().and_then(|n| n.to_str()) == Some(name) {
            return Some(path);
        }
        if path.is_dir()
            && let Some(found) = find_file(&path, name)
        {
            return Some(found);
        }
    }
    None
}

/// N1: once the copy on the target is gone, a finish keeps the source, and the source may be moved again.
#[test]
fn a_finish_that_finds_the_copy_gone_lets_the_source_move_again() {
    let rig = Rig::new(vec![web()], 100);
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    assert_eq!(rig.wait(&id)["phase"], "moved");
    writer(&rig.target)
        .set_state("shop", "web", "stop", true, Duration::from_secs(5))
        .expect("stopped");
    writer(&rig.target)
        .delete_instance("shop", "web")
        .expect("the copy is removed on the target");
    let finished = rig
        .approve(&rig.propose_finish(&id).expect("proposed"))
        .expect("finished");
    assert_eq!(finished["deleted"], json!([]), "{finished}");
    assert!(rig.source.holds("instance", "shop", "web"));
    let plan = rig
        .call_as(
            Scope::Viewer,
            "instance_move_plan",
            json!({ "instances": [{ "project": "shop", "name": "web" }], "target": "node-b" }),
        )
        .expect("the plan answers");
    assert_eq!(plan["go"], true, "{plan}");
}

/// N2: a finish keeps a moved source that reads Frozen, not Stopped: it may run again.
#[test]
fn a_finish_keeps_a_source_that_is_frozen() {
    let rig = Rig::new(vec![web()], 100);
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    assert_eq!(rig.wait(&id)["phase"], "moved");
    rig.source.set_instance_status("shop", "web", "Frozen");
    let finished = rig
        .approve(&rig.propose_finish(&id).expect("proposed"))
        .expect("finished");
    assert!(
        finished["kept"].to_string().contains("shop/web"),
        "{finished}"
    );
    assert!(rig.source.holds("instance", "shop", "web"));
}

/// N7: when the source's spot-check file can be read neither before nor after the stop, the check fails.
#[test]
fn a_spot_check_with_no_reference_on_the_source_fails() {
    let rig = Rig::new(vec![web()], 100);
    rig.source.plan_push(hold());
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    let first = held(&rig, 1);
    // The file goes from the source once the plan was checked: no read of it works, before or after the stop.
    rig.source.remove_guest_file("shop", "web", "/etc/hostname");
    rig.source.release_operation(&first);
    let status = rig.wait(&id);
    let one = &status["instances"][0];
    assert_eq!(one["result"], "rolled_back", "{status}");
    assert!(
        one["reason"]
            .as_str()
            .unwrap()
            .contains("could not be read on the source"),
        "{one}"
    );
}

/// N7: a copy whose spot-check file differs from the source's fails the check.
#[test]
fn a_spot_check_that_differs_fails() {
    let rig = Rig::new(vec![web()], 100);
    rig.source.plan_push(PushPlan::Pass);
    rig.source.plan_push(PushPlan::Pass);
    rig.source.plan_push(hold());
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    let last = held(&rig, 1);
    // The last delta was taken; the source's file changes after it.
    rig.source
        .write_guest_file("shop", "web", "/etc/hostname", b"changed\n");
    rig.source.release_operation(&last);
    let status = rig.wait(&id);
    let one = &status["instances"][0];
    assert_eq!(one["result"], "rolled_back", "{status}");
    assert!(one["reason"].as_str().unwrap().contains("differs"), "{one}");
}

/// Plans `web` to node-c, as a Doctor retrying elsewhere would.
fn plan_to_third(rig: &Rig) -> Value {
    rig.source.link_migration(&rig.third);
    rig.call_as(
        Scope::Viewer,
        "instance_move_plan",
        json!({ "instances": [{ "project": "shop", "name": "web" }], "target": "node-c" }),
    )
    .expect("the plan answers")
}

/// R3-1: a move the kernel left with its copy started is `failed`, its copy running on node-b and its source stopped
/// here. The instance is held while that copy runs: a move to node-c is not a go. Once the copy is stopped, it is.
#[test]
fn a_failed_move_whose_copy_runs_holds_the_instance() {
    let rig = Rig::new(vec![web()], 100);
    writer(&rig.source)
        .set_state("shop", "web", "stop", true, Duration::from_secs(5))
        .expect("stopped");
    let uuid = copy_on_target(&rig, "web", "Running");
    interrupted(
        &rig,
        "mv-0123456789ab",
        vec![
            json!({ "event": "created", "targetUuid": uuid }),
            json!({ "event": "cutover", "step": "stopped", "wasRunning": true }),
            json!({ "event": "cutover", "step": "starting" }),
        ],
    );
    rig.moves.recover();
    let status = rig.wait("mv-0123456789ab");
    assert_eq!(status["instances"][0]["result"], "failed", "{status}");
    let plan = plan_to_third(&rig);
    assert_eq!(plan["go"], false, "{plan}");
    let reasons = plan["blocked"][0]["reasons"].to_string();
    assert!(
        reasons.contains("mv-0123456789ab")
            && reasons.contains("node-b")
            && reasons.contains("Running"),
        "{reasons}"
    );
    writer(&rig.target)
        .set_state("shop", "web", "stop", true, Duration::from_secs(5))
        .expect("the copy is stopped");
    let plan = plan_to_third(&rig);
    assert_eq!(
        plan["go"], true,
        "a stopped copy releases it: {}",
        plan["blocked"]
    );
}

/// R3-2: the way back (stop the copy, start the source), a finish that keeps the source, then the copy deleted on
/// node-b: the instance may move again.
#[test]
fn after_going_back_and_finishing_the_instance_can_move_again() {
    let rig = Rig::new(vec![web()], 100);
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    assert_eq!(rig.wait(&id)["phase"], "moved");
    writer(&rig.target)
        .set_state("shop", "web", "stop", true, Duration::from_secs(5))
        .expect("copy stopped");
    writer(&rig.source)
        .set_state("shop", "web", "start", false, Duration::from_secs(5))
        .expect("source started");
    let finished = rig
        .approve(&rig.propose_finish(&id).expect("proposed"))
        .expect("finished");
    assert!(
        finished["kept"].to_string().contains("shop/web"),
        "{finished}"
    );
    writer(&rig.target)
        .delete_instance("shop", "web")
        .expect("the copy is removed on the target");
    let plan = rig.plan(&["web"]);
    assert_eq!(plan["go"], true, "{}", plan["blocked"]);
}

/// R3-1: a target that cannot be read holds the instance: its copy there may run.
#[test]
fn a_copy_on_a_target_that_cannot_be_read_holds_the_instance() {
    let rig = Rig::new(vec![web()], 100);
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    assert_eq!(rig.wait(&id)["phase"], "moved");
    writer(&rig.target)
        .set_state("shop", "web", "stop", true, Duration::from_secs(5))
        .expect("copy stopped");
    rig.target.fail_reads("/1.0/instances/web", 1000, 500);
    let plan = plan_to_third(&rig);
    assert_eq!(plan["go"], false, "{plan}");
    let reasons = plan["blocked"][0]["reasons"].to_string();
    assert!(
        reasons.contains(&id) && reasons.contains("could not be read"),
        "{reasons}"
    );
}

/// R3-1, at the start: a plan made while the copy was stopped does not start once the copy runs again.
#[test]
fn a_copy_started_again_after_the_plan_refuses_the_start() {
    let rig = Rig::new(vec![web()], 100);
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    assert_eq!(rig.wait(&id)["phase"], "moved");
    writer(&rig.target)
        .set_state("shop", "web", "stop", true, Duration::from_secs(5))
        .expect("copy stopped");
    let plan = plan_to_third(&rig);
    assert_eq!(plan["go"], true, "{}", plan["blocked"]);
    let proposal = rig
        .call_as(
            Scope::Viewer,
            "action_propose",
            json!({ "action": "instance_move", "args": {
                "plan": plan["digest"], "instances": [{ "project": "shop", "name": "web" }], "target": "node-c",
            } }),
        )
        .expect("proposed");
    writer(&rig.target)
        .set_state("shop", "web", "start", false, Duration::from_secs(5))
        .expect("copy started again");
    let Err(CallError::Refused { code, message }) = rig.approve(&proposal) else {
        panic!("the move started beside a running copy");
    };
    assert_eq!(code, "AlreadyMoved", "{message}");
    assert!(message.contains(&id), "{message}");
    assert!(!rig.third.holds("instance", "shop", "web"));
}

/// R3-3: the reason of a failed move with its copy started names what the owner can do, not a finish it is not offered.
#[test]
fn a_failed_move_with_its_copy_started_does_not_offer_a_finish() {
    let rig = Rig::new(vec![web()], 100);
    writer(&rig.source)
        .set_state("shop", "web", "stop", true, Duration::from_secs(5))
        .expect("stopped");
    let uuid = copy_on_target(&rig, "web", "Running");
    interrupted(
        &rig,
        "mv-0123456789ab",
        vec![
            json!({ "event": "created", "targetUuid": uuid }),
            json!({ "event": "cutover", "step": "stopped", "wasRunning": true }),
            json!({ "event": "cutover", "step": "starting" }),
        ],
    );
    rig.moves.recover();
    let status = rig.wait("mv-0123456789ab");
    let reason = status["instances"][0]["reason"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(!reason.contains("finish the move"), "{reason}");
    assert!(reason.contains("stop the copy"), "{reason}");
}

/// R4-1: a move whose rollback removed its copy, and journaled it, holds nothing, even once its target cannot be read.
#[test]
fn a_cleanly_rolled_back_move_to_an_unreadable_target_does_not_hold() {
    let rig = Rig::new(vec![web()], 100);
    rig.source.plan_push(PushPlan::Pass);
    rig.source
        .plan_push(PushPlan::Fail("the link dropped".to_string()));
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    let status = rig.wait(&id);
    assert_eq!(status["instances"][0]["result"], "rolled_back", "{status}");
    assert!(!rig.target.holds("instance", "shop", "web"));
    let lines = rig.journal();
    assert!(
        lines
            .iter()
            .any(|l| l["event"] == "created" && l["targetUuid"].is_string())
    );
    let rollback = lines.iter().find(|l| l["event"] == "rollback").unwrap();
    assert_eq!(rollback["targetRemoved"], true, "{rollback}");
    let plan = plan_to_third(&rig);
    assert_eq!(plan["go"], true, "node-b readable: {}", plan["blocked"]);
    rig.target.fail_reads("/1.0/instances/web", 1000, 500);
    let plan = plan_to_third(&rig);
    assert_eq!(
        plan["go"], true,
        "node-b unreadable, the copy journaled deleted: {}",
        plan["blocked"]
    );
}

/// R4-4: another instance of the name on the target, not the copy the move recorded, holds nothing.
#[test]
fn another_instance_of_the_name_on_the_target_does_not_hold() {
    let rig = Rig::new(vec![web()], 100);
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    assert_eq!(rig.wait(&id)["phase"], "moved");
    writer(&rig.target)
        .set_state("shop", "web", "stop", true, Duration::from_secs(5))
        .expect("copy stopped");
    writer(&rig.target)
        .delete_instance("shop", "web")
        .expect("the copy is removed on the target");
    copy_on_target(&rig, "web", "Running");
    let plan = plan_to_third(&rig);
    assert_eq!(plan["go"], true, "{}", plan["blocked"]);
}

/// R4-4: a card for a plan that was a go is refused when the instance is held by the time the card is proposed.
#[test]
fn a_card_for_an_instance_held_now_is_refused() {
    let rig = Rig::new(vec![web()], 100);
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    assert_eq!(rig.wait(&id)["phase"], "moved");
    writer(&rig.target)
        .set_state("shop", "web", "stop", true, Duration::from_secs(5))
        .expect("copy stopped");
    let plan = plan_to_third(&rig);
    assert_eq!(plan["go"], true, "{}", plan["blocked"]);
    writer(&rig.target)
        .set_state("shop", "web", "start", false, Duration::from_secs(5))
        .expect("copy started again");
    let card = rig.call_as(
        Scope::Viewer,
        "action_propose",
        json!({ "action": "instance_move", "args": {
            "plan": plan["digest"], "instances": [{ "project": "shop", "name": "web" }], "target": "node-c",
        } }),
    );
    assert_eq!(refusal_code(card), "AlreadyMoved");
}

/// R4-2: the AlreadyMoved reason warns that the copy on the target is the one that served since the cutover.
#[test]
fn the_already_moved_reason_says_to_keep_what_the_copy_holds() {
    let rig = Rig::new(vec![web()], 100);
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    assert_eq!(rig.wait(&id)["phase"], "moved");
    let reasons = plan_to_third(&rig)["blocked"][0]["reasons"].to_string();
    assert!(reasons.contains(&format!("finish move {id}")), "{reasons}");
    assert!(
        reasons.contains("once you have kept what it holds"),
        "{reasons}"
    );
}

fn source_autostart(rig: &Rig) -> Value {
    writer(&rig.source)
        .get("/1.0/instances/web?project=shop")
        .expect("the source")["config"]["boot.autostart"]
        .clone()
}

/// R5-1 (A): the next start leaves a stopped source beside a copy that was started: a restart of this host must not
/// start it, so its boot.autostart is turned off.
#[test]
fn a_source_left_beside_a_started_copy_by_recovery_has_autostart_off() {
    let rig = Rig::new(vec![web()], 100);
    rig.source
        .set_instance_config("shop", "web", "boot.autostart", "true");
    writer(&rig.source)
        .set_state("shop", "web", "stop", true, Duration::from_secs(5))
        .expect("stopped");
    let uuid = copy_on_target(&rig, "web", "Running");
    interrupted(
        &rig,
        "mv-0123456789ab",
        vec![
            json!({ "event": "created", "targetUuid": uuid }),
            json!({ "event": "cutover", "step": "stopped", "wasRunning": true }),
            json!({ "event": "cutover", "step": "starting" }),
        ],
    );
    rig.moves.recover();
    let status = rig.wait("mv-0123456789ab");
    assert_eq!(status["instances"][0]["result"], "failed", "{status}");
    assert_eq!(source_autostart(&rig), "false");
    assert_eq!(result_line(&rig, "web")["autostartOff"], true);
    // R6-2: the reason says it was turned off, and how to come back.
    let reason = status["instances"][0]["reason"]
        .as_str()
        .unwrap_or_default();
    assert!(reason.contains("boot.autostart was true"), "{reason}");
}

/// R5-1 (B): a rollback that cannot read the target leaves the source stopped, and turns its boot.autostart off.
#[test]
fn a_source_left_stopped_by_a_rollback_that_could_not_read_the_target_has_autostart_off() {
    let rig = Rig::new(vec![web()], 100);
    rig.source
        .set_instance_config("shop", "web", "boot.autostart", "true");
    rig.source.plan_push(PushPlan::Pass);
    rig.source.plan_push(PushPlan::Pass);
    rig.source.plan_push(hold());
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    let last = held(&rig, 1);
    rig.target.refuse_start("shop", "web");
    rig.target.fail_reads("/1.0/instances/web", 1000, 500);
    rig.source.release_operation(&last);
    let status = rig.wait(&id);
    assert_eq!(status["instances"][0]["result"], "failed", "{status}");
    assert_eq!(
        rig.source.instance_status("shop", "web").as_deref(),
        Some("Stopped")
    );
    assert_eq!(source_autostart(&rig), "false");
    assert_eq!(result_line(&rig, "web")["autostartOff"], true);
}

/// R5-1 (B, at the next start): the recovery's rollback cannot read the target either; the same holds.
#[test]
fn a_source_left_stopped_by_a_recovery_rollback_has_autostart_off() {
    let rig = Rig::new(vec![web()], 100);
    rig.source
        .set_instance_config("shop", "web", "boot.autostart", "true");
    writer(&rig.source)
        .set_state("shop", "web", "stop", true, Duration::from_secs(5))
        .expect("stopped");
    let uuid = copy_on_target(&rig, "web", "Stopped");
    interrupted(
        &rig,
        "mv-0123456789ab",
        vec![
            json!({ "event": "created", "targetUuid": uuid }),
            json!({ "event": "cutover", "step": "stopped", "wasRunning": true }),
        ],
    );
    rig.target.fail_reads("/1.0/instances/web", 1000, 500);
    rig.moves.recover();
    let status = rig.wait("mv-0123456789ab");
    assert_eq!(status["instances"][0]["result"], "failed", "{status}");
    assert_eq!(
        rig.source.instance_status("shop", "web").as_deref(),
        Some("Stopped")
    );
    assert_eq!(source_autostart(&rig), "false");
    assert_eq!(result_line(&rig, "web")["autostartOff"], true);
}

/// R5-1: when turning it off fails, the result says so for the owner.
#[test]
fn a_failed_move_whose_source_keeps_autostart_says_so() {
    let rig = Rig::new(vec![web()], 100);
    rig.source
        .set_instance_config("shop", "web", "boot.autostart", "true");
    rig.source.plan_push(PushPlan::Pass);
    rig.source.plan_push(PushPlan::Pass);
    rig.source.plan_push(hold());
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    let last = held(&rig, 1);
    rig.target.refuse_start("shop", "web");
    rig.target.fail_reads("/1.0/instances/web", 1000, 500);
    rig.source.set_reject_patch("/1.0/instances/web");
    rig.source.release_operation(&last);
    let status = rig.wait(&id);
    let one = &status["instances"][0];
    assert_eq!(one["result"], "failed", "{status}");
    assert!(
        one["reason"]
            .as_str()
            .unwrap()
            .contains("boot.autostart could not be turned off"),
        "{one}"
    );
    assert_eq!(result_line(&rig, "web")["autostartOff"], false);
}

/// The writes the source received that carried `boot.autostart` to the instance `web`.
fn autostart_writes(rig: &Rig) -> usize {
    rig.source
        .writes()
        .into_iter()
        .filter(|w| w.path.starts_with("/1.0/instances/web"))
        .filter(|w| w.config_keys.iter().any(|k| k == "boot.autostart"))
        .count()
}

const OTHER_UUID: &str = "11111111-1111-4111-8111-111111111111";

/// The recovery fixture whose copy was started: the copy runs on node-b.
fn copy_started(rig: &Rig) {
    let uuid = copy_on_target(rig, "web", "Running");
    interrupted(
        rig,
        "mv-0123456789ab",
        vec![
            json!({ "event": "created", "targetUuid": uuid }),
            json!({ "event": "cutover", "step": "stopped", "wasRunning": true }),
            json!({ "event": "cutover", "step": "starting" }),
        ],
    );
}

fn copy_autostart(rig: &Rig) -> Value {
    writer(&rig.target)
        .get("/1.0/instances/web?project=shop")
        .expect("the copy")["config"]["boot.autostart"]
        .clone()
}

/// R6-1: a copy a failed move leaves on a target it could not read is created with boot.autostart false, so a restart
/// of the target's Incus does not start it beside the running source.
#[test]
fn a_copy_left_on_an_unreadable_target_does_not_start_by_itself() {
    let rig = Rig::new(vec![web()], 100);
    rig.source
        .set_instance_config("shop", "web", "boot.autostart", "true");
    rig.source.plan_push(PushPlan::Pass);
    rig.source.plan_push(hold());
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    let second = held(&rig, 1);
    rig.target.fail_reads("/1.0/instances/web", 50, 500);
    rig.source
        .fail_operation(&second, "the link to the target dropped");
    let status = rig.wait(&id);
    assert_eq!(status["instances"][0]["result"], "failed", "{status}");
    assert_eq!(rollback_line(&rig, "web")["targetRemoved"], false);
    assert_eq!(
        rig.source.instance_status("shop", "web").as_deref(),
        Some("Running")
    );
    let copy = (0..100)
        .find_map(|_| {
            writer(&rig.target)
                .get("/1.0/instances/web?project=shop")
                .ok()
        })
        .expect("the copy is still on the target");
    assert_eq!(copy["status"], "Stopped", "{copy}");
    assert_eq!(copy["config"]["boot.autostart"], "false", "{copy}");
}

/// R6-1: once verified, the copy takes the source's own boot.autostart: true stays true, unset stays unset.
#[test]
fn a_verified_copy_takes_the_sources_autostart() {
    let rig = Rig::new(vec![web()], 100);
    rig.source
        .set_instance_config("shop", "web", "boot.autostart", "true");
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    assert_eq!(rig.wait(&id)["phase"], "moved");
    assert_eq!(copy_autostart(&rig), "true");
    let rig = Rig::new(vec![web()], 100);
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    assert_eq!(rig.wait(&id)["phase"], "moved");
    assert_eq!(copy_autostart(&rig), Value::Null);
}

/// R6-1 and R6-6: the next start that records a verified copy as moved gives the copy the source's boot.autostart,
/// turns the source's off, and says so when that fails.
#[test]
fn a_verified_copy_the_next_start_records_takes_the_sources_autostart() {
    let rig = Rig::new(vec![web()], 100);
    rig.source
        .set_instance_config("shop", "web", "boot.autostart", "true");
    writer(&rig.source)
        .set_state("shop", "web", "stop", true, Duration::from_secs(5))
        .expect("stopped");
    let uuid = copy_on_target(&rig, "web", "Running");
    interrupted(
        &rig,
        "mv-0123456789ab",
        vec![
            json!({ "event": "created", "targetUuid": uuid }),
            json!({ "event": "cutover", "step": "stopped", "wasRunning": true }),
            json!({ "event": "cutover", "step": "starting" }),
            json!({ "event": "cutover", "step": "started" }),
            json!({ "event": "verify", "ok": true, "checks": [], "downtimeMs": 1234 }),
        ],
    );
    rig.source.set_reject_patch("/1.0/instances/web");
    rig.moves.recover();
    let status = rig.wait("mv-0123456789ab");
    assert_eq!(status["instances"][0]["result"], "moved", "{status}");
    assert_eq!(copy_autostart(&rig), "true");
    let line = result_line(&rig, "web");
    assert_eq!(line["autostartOff"], false, "{line}");
    assert!(
        line["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("boot.autostart"),
        "{line}"
    );
}

/// R6-2: a failed ending leaves an unset boot.autostart unset: Incus then restores the last state, stopped.
#[test]
fn a_failed_ending_leaves_an_unset_autostart_unset() {
    let rig = Rig::new(vec![web()], 100);
    writer(&rig.source)
        .set_state("shop", "web", "stop", true, Duration::from_secs(5))
        .expect("stopped");
    copy_started(&rig);
    rig.moves.recover();
    let status = rig.wait("mv-0123456789ab");
    assert_eq!(status["instances"][0]["result"], "failed", "{status}");
    assert_eq!(source_autostart(&rig), Value::Null);
    assert_eq!(autostart_writes(&rig), 0);
    assert_eq!(result_line(&rig, "web")["autostartOff"], true);
}

/// R6-3: a recovery whose target is no longer linked leaves a running source's boot.autostart alone: the move never
/// stopped it.
#[test]
fn a_recovery_to_an_unlinked_target_leaves_a_running_source_alone() {
    let rig = Rig::new(vec![web()], 100);
    rig.source
        .set_instance_config("shop", "web", "boot.autostart", "true");
    interrupted_to(
        &rig,
        "mv-0123456789ab",
        "node-gone",
        vec![json!({ "event": "created", "targetUuid": OTHER_UUID })],
    );
    rig.moves.recover();
    let status = rig.wait("mv-0123456789ab");
    assert_eq!(status["instances"][0]["result"], "failed", "{status}");
    assert_eq!(
        rig.source.instance_status("shop", "web").as_deref(),
        Some("Running")
    );
    assert_eq!(source_autostart(&rig), "true");
    assert_eq!(autostart_writes(&rig), 0);
}

/// R6-4: the recovery's copy-started ending leaves another instance of the name (another uuid) alone.
#[test]
fn the_recovery_copy_started_ending_leaves_another_uuid_alone() {
    let rig = Rig::new(vec![web()], 100);
    rig.source
        .set_instance_config("shop", "web", "boot.autostart", "true");
    rig.source
        .set_instance_config("shop", "web", "volatile.uuid", OTHER_UUID);
    writer(&rig.source)
        .set_state("shop", "web", "stop", true, Duration::from_secs(5))
        .expect("stopped");
    copy_started(&rig);
    rig.moves.recover();
    let status = rig.wait("mv-0123456789ab");
    assert_eq!(status["instances"][0]["result"], "failed", "{status}");
    assert_eq!(source_autostart(&rig), "true");
    assert_eq!(autostart_writes(&rig), 0);
    assert_eq!(result_line(&rig, "web")["autostartOff"], Value::Null);
}

/// R6-4: the recovery's copy-started ending sends nothing to a source that is gone.
#[test]
fn the_recovery_copy_started_ending_leaves_a_gone_source_alone() {
    let rig = Rig::new(vec![api()], 100);
    copy_started(&rig);
    rig.moves.recover();
    let status = rig.wait("mv-0123456789ab");
    assert_eq!(status["instances"][0]["result"], "failed", "{status}");
    assert_eq!(autostart_writes(&rig), 0);
    let line = result_line(&rig, "web");
    assert_eq!(line["autostartOff"], Value::Null, "{line}");
    assert!(
        !line["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("boot.autostart could not be turned off"),
        "{line}"
    );
}

/// R6-4: the recovery's rollback that cannot read the target sends nothing to a gone source.
#[test]
fn the_recovery_rollback_leaves_a_gone_source_alone() {
    let rig = Rig::new(vec![api()], 100);
    let uuid = copy_on_target(&rig, "web", "Stopped");
    interrupted(
        &rig,
        "mv-0123456789ab",
        vec![
            json!({ "event": "created", "targetUuid": uuid }),
            json!({ "event": "cutover", "step": "stopped", "wasRunning": true }),
        ],
    );
    rig.target.fail_reads("/1.0/instances/web", 1000, 500);
    rig.moves.recover();
    let status = rig.wait("mv-0123456789ab");
    assert_eq!(status["instances"][0]["result"], "failed", "{status}");
    assert_eq!(autostart_writes(&rig), 0);
    assert_eq!(result_line(&rig, "web")["autostartOff"], Value::Null);
}

/// R6-4: a rollback that cannot read the target leaves alone a source swapped for another uuid during the last round.
#[test]
fn a_rollback_leaves_a_source_of_another_uuid_alone() {
    let rig = Rig::new(vec![web()], 100);
    rig.source
        .set_instance_config("shop", "web", "boot.autostart", "true");
    rig.source.plan_push(PushPlan::Pass);
    rig.source.plan_push(PushPlan::Pass);
    rig.source.plan_push(hold());
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    let last = held(&rig, 1);
    rig.source
        .set_instance_config("shop", "web", "volatile.uuid", OTHER_UUID);
    rig.target.refuse_start("shop", "web");
    rig.target.fail_reads("/1.0/instances/web", 1000, 500);
    rig.source.release_operation(&last);
    let status = rig.wait(&id);
    assert_eq!(status["instances"][0]["result"], "failed", "{status}");
    assert_eq!(rollback_line(&rig, "web")["sourceIntact"], false);
    assert_eq!(source_autostart(&rig), "true");
    assert_eq!(autostart_writes(&rig), 0);
    assert_eq!(result_line(&rig, "web")["autostartOff"], Value::Null);
}

/// R6-4: when turning it off fails at the next start, both recovery endings say so.
#[test]
fn the_recovery_endings_say_when_autostart_stays_on() {
    let rig = Rig::new(vec![web()], 100);
    rig.source
        .set_instance_config("shop", "web", "boot.autostart", "true");
    writer(&rig.source)
        .set_state("shop", "web", "stop", true, Duration::from_secs(5))
        .expect("stopped");
    copy_started(&rig);
    rig.source.set_reject_patch("/1.0/instances/web");
    rig.moves.recover();
    rig.wait("mv-0123456789ab");
    let line = result_line(&rig, "web");
    assert_eq!(line["autostartOff"], false, "{line}");
    assert!(
        line["reason"]
            .as_str()
            .unwrap()
            .contains("boot.autostart could not be turned off"),
        "{line}"
    );
    let rig = Rig::new(vec![web()], 100);
    rig.source
        .set_instance_config("shop", "web", "boot.autostart", "true");
    writer(&rig.source)
        .set_state("shop", "web", "stop", true, Duration::from_secs(5))
        .expect("stopped");
    let uuid = copy_on_target(&rig, "web", "Stopped");
    interrupted(
        &rig,
        "mv-0123456789ab",
        vec![
            json!({ "event": "created", "targetUuid": uuid }),
            json!({ "event": "cutover", "step": "stopped", "wasRunning": true }),
        ],
    );
    rig.target.fail_reads("/1.0/instances/web", 1000, 500);
    rig.source.set_reject_patch("/1.0/instances/web");
    rig.moves.recover();
    rig.wait("mv-0123456789ab");
    let line = result_line(&rig, "web");
    assert_eq!(line["autostartOff"], false, "{line}");
    assert!(
        line["reason"]
            .as_str()
            .unwrap()
            .contains("boot.autostart could not be turned off"),
        "{line}"
    );
}

/// R6-4: a failed rollback that removed the copy leaves the source's boot.autostart alone: no copy can run beside it.
#[test]
fn a_failed_rollback_that_removed_the_copy_leaves_autostart_alone() {
    let rig = Rig::new(vec![web()], 100);
    rig.source
        .set_instance_config("shop", "web", "boot.autostart", "true");
    rig.target.refuse_start("shop", "web");
    rig.source.refuse_start("shop", "web");
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    let status = rig.wait(&id);
    assert_eq!(status["instances"][0]["result"], "failed", "{status}");
    assert_eq!(rollback_line(&rig, "web")["targetRemoved"], true);
    assert_eq!(rollback_line(&rig, "web")["sourceRunning"], false);
    assert_eq!(source_autostart(&rig), "true");
    assert_eq!(autostart_writes(&rig), 0);
    assert_eq!(result_line(&rig, "web")["autostartOff"], Value::Null);
}

/// R6-1: a verified copy whose boot.autostart cannot be set to the source's is still moved, and the result says so for
/// the owner.
#[test]
fn a_moved_copy_whose_autostart_cannot_be_set_is_named_in_the_result() {
    let rig = Rig::new(vec![web()], 100);
    rig.source
        .set_instance_config("shop", "web", "boot.autostart", "true");
    rig.target.set_reject_patch("/1.0/instances/web");
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    let status = rig.wait(&id);
    assert_eq!(status["instances"][0]["result"], "moved", "{status}");
    assert_eq!(copy_autostart(&rig), "false");
    let line = result_line(&rig, "web");
    assert_eq!(line["copyAutostartSet"], false, "{line}");
    assert_eq!(line["autostartOff"], true, "{line}");
    assert!(
        line["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("could not be set back on the copy on node-b"),
        "{line}"
    );
    let late = (0..400).find_map(|_| {
        std::thread::sleep(Duration::from_millis(25));
        let path = find_file(&rig.dir, "action-proposals.jsonl")?;
        let text = fs::read_to_string(path).ok()?;
        text.lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|line| {
                line["code"]
                    .as_str()
                    .is_some_and(|code| code.starts_with("Move"))
            })
    });
    assert_eq!(late.expect("a late result")["code"], "MoveNeedsOwner");
}

/// R6-1: the next start gives a verified copy the source's boot.autostart as the copy's creation recorded it, not as
/// the source reads after the run turned it off.
#[test]
fn a_verified_copy_the_next_start_records_takes_the_recorded_autostart() {
    let rig = Rig::new(vec![web()], 100);
    rig.source
        .set_instance_config("shop", "web", "boot.autostart", "false");
    writer(&rig.source)
        .set_state("shop", "web", "stop", true, Duration::from_secs(5))
        .expect("stopped");
    let uuid = copy_on_target(&rig, "web", "Running");
    interrupted(
        &rig,
        "mv-0123456789ab",
        vec![
            json!({ "event": "created", "targetUuid": uuid, "sourceAutostart": "true" }),
            json!({ "event": "cutover", "step": "stopped", "wasRunning": true }),
            json!({ "event": "cutover", "step": "starting" }),
            json!({ "event": "cutover", "step": "started" }),
            json!({ "event": "verify", "ok": true, "checks": [], "downtimeMs": 1234 }),
        ],
    );
    rig.moves.recover();
    let status = rig.wait("mv-0123456789ab");
    assert_eq!(status["instances"][0]["result"], "moved", "{status}");
    assert_eq!(copy_autostart(&rig), "true");
    assert_eq!(source_autostart(&rig), "false");
    assert_eq!(result_line(&rig, "web")["copyAutostartSet"], true);
}

/// R6-1: the copy's creation records the source's own boot.autostart in the journal, so a next start can give it to a
/// verified copy after the run turned the source's off.
#[test]
fn the_copys_creation_records_the_sources_autostart() {
    let rig = Rig::new(vec![web()], 100);
    rig.source
        .set_instance_config("shop", "web", "boot.autostart", "true");
    let id = rig.start(&["web"])["move"].as_str().unwrap().to_string();
    assert_eq!(rig.wait(&id)["phase"], "moved");
    let created = rig
        .journal()
        .into_iter()
        .find(|line| line["event"] == "created" && line["targetUuid"].is_string())
        .expect("the created line");
    assert_eq!(created["sourceAutostart"], "true", "{created}");
}

/// R7-1: the next start that records a verified copy as moved sets boot.autostart only on the copy the move recorded:
/// another instance of the name on the target (another uuid) is left alone, and the result says the copy's was not set.
#[test]
fn the_recovery_moved_path_leaves_a_target_instance_of_another_uuid_alone() {
    let rig = Rig::new(vec![web()], 100);
    writer(&rig.source)
        .set_state("shop", "web", "stop", true, Duration::from_secs(5))
        .expect("stopped");
    let theirs = copy_on_target(&rig, "web", "Running");
    assert_ne!(theirs, OTHER_UUID);
    interrupted(
        &rig,
        "mv-0123456789ab",
        vec![
            json!({ "event": "created", "targetUuid": OTHER_UUID, "sourceAutostart": "true" }),
            json!({ "event": "cutover", "step": "stopped", "wasRunning": true }),
            json!({ "event": "cutover", "step": "starting" }),
            json!({ "event": "cutover", "step": "started" }),
            json!({ "event": "verify", "ok": true, "checks": [], "downtimeMs": 1234 }),
        ],
    );
    rig.moves.recover();
    let status = rig.wait("mv-0123456789ab");
    assert_eq!(status["instances"][0]["result"], "moved", "{status}");
    let on_target = writer(&rig.target)
        .get("/1.0/instances/web?project=shop")
        .expect("the target's web");
    assert_eq!(on_target["config"]["volatile.uuid"], theirs.as_str());
    assert_eq!(
        on_target["config"]["boot.autostart"],
        Value::Null,
        "{on_target}"
    );
    let line = result_line(&rig, "web");
    assert_eq!(line["copyAutostartSet"], false, "{line}");
    // R8-2: the reason says the instance there is not the recorded copy, and does not send the owner to set it.
    let reason = line["reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("on node-b is not read as the copy this move recorded"),
        "{line}"
    );
    assert!(!reason.contains("set it as it was on the source"), "{line}");
}

/// R6-2: a profile's boot.autostart true on a stopped source counts: the copy-started recovery ending turns it off.
#[test]
fn a_profiles_autostart_on_a_stopped_source_is_turned_off() {
    let rig = Rig::new(vec![web()], 100);
    rig.source.add_profile(
        "shop",
        json!({ "name": "default", "description": "", "config": { "boot.autostart": "true" }, "devices": {} }),
    );
    writer(&rig.source)
        .set_state("shop", "web", "stop", true, Duration::from_secs(5))
        .expect("stopped");
    let before = writer(&rig.source)
        .get("/1.0/instances/web?project=shop")
        .expect("the source");
    assert_eq!(
        before["expanded_config"]["boot.autostart"], "true",
        "{before}"
    );
    assert_eq!(before["config"]["boot.autostart"], Value::Null, "{before}");
    copy_started(&rig);
    rig.moves.recover();
    let status = rig.wait("mv-0123456789ab");
    assert_eq!(status["instances"][0]["result"], "failed", "{status}");
    assert_eq!(source_autostart(&rig), "false");
    assert_eq!(result_line(&rig, "web")["autostartOff"], true);
}

/// R6-2: a source whose boot.autostart is last-state or false is left alone by a failed ending.
#[test]
fn a_last_state_or_false_autostart_is_left_alone() {
    for value in ["last-state", "false"] {
        let rig = Rig::new(vec![web()], 100);
        rig.source
            .set_instance_config("shop", "web", "boot.autostart", value);
        writer(&rig.source)
            .set_state("shop", "web", "stop", true, Duration::from_secs(5))
            .expect("stopped");
        copy_started(&rig);
        rig.moves.recover();
        let status = rig.wait("mv-0123456789ab");
        assert_eq!(status["instances"][0]["result"], "failed", "{status}");
        assert_eq!(source_autostart(&rig), value);
        assert_eq!(autostart_writes(&rig), 0, "{value}");
        assert_eq!(result_line(&rig, "web")["autostartOff"], true);
    }
}
