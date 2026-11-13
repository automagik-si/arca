//! `host_network_restore`: proposed as a card that says what the host drifted from, and, once the owner approves it, run by
//! asking the root part's socket to apply the declared state. The root part here is a stand-in that answers on a real socket.

mod common;

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use arca::config::NodeConfig;
use arca::handlers::{ActionExecutor, ActionSlot, Clock, plan_handlers_served};
use arca::kernel::Kernel;
use arca_api::{Scope, conforms, verb};
use arca_hostnet::root::Ops;
use arca_hostnet::socket;
use arca_hostnet::status::{DriftItem, Status};
use arca_incus::{FakeIncus, FakeState};
use arca_kernel::canon::Digest;
use arca_kernel::identity::NodeKey;
use arca_mcp::{CallError, Caller, ToolHandler};
use arca_providers::incus::{Family, IncusProvider};
use arca_reconcile::PluginSet;
use common::{HOST, Scratch, SkKey, approve_statement, verifier_for};
use serde_json::{Value, json};

const NOW: u64 = 1_793_361_600_000;
static SEQ: AtomicU64 = AtomicU64::new(0);

/// A root part that holds a host with one drifted item until it is asked to apply.
struct StandIn {
    drifted: AtomicBool,
    applies: Mutex<u32>,
}

impl StandIn {
    fn status(&self) -> Status {
        let drifted = self.drifted.load(Ordering::SeqCst);
        Status {
            at: NOW,
            declared: true,
            in_sync: !drifted,
            drift: if drifted {
                vec![DriftItem {
                    item: "rule-in:incusbr0".to_string(),
                    detail: "DOCKER-USER does not accept what comes in from the bridge incusbr0"
                        .to_string(),
                }]
            } else {
                Vec::new()
            },
            applied: Vec::new(),
            errors: Vec::new(),
            version: "test".to_string(),
        }
    }
}

impl Ops for StandIn {
    fn read(&self) -> Status {
        self.status()
    }

    fn apply(&self) -> Status {
        *self.applies.lock().expect("applies") += 1;
        let before = self.status();
        self.drifted.store(false, Ordering::SeqCst);
        let mut after = self.status();
        after.applied = before
            .drift
            .iter()
            .map(|_| "iptables -w 5 -I DOCKER-USER -i incusbr0 -j ACCEPT".to_string())
            .collect();
        after
    }
}

struct Rig {
    dir: PathBuf,
    _scratch: Scratch,
    _fake: FakeIncus,
    kernel: Arc<Mutex<Kernel>>,
    key: SkKey,
    handlers: HashMap<&'static str, Arc<dyn ToolHandler>>,
    executor: ActionExecutor,
    root: Arc<StandIn>,
    stop: Arc<AtomicBool>,
    nonce: AtomicU64,
}

impl Rig {
    /// A node whose config names a socket in its own directory. With `listening` the stand-in root part answers on it.
    fn new(listening: bool) -> Rig {
        let dir = std::env::temp_dir().join(format!(
            "arca-hostnet-rig-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir(&dir).expect("scratch");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("0700");
        let fake = FakeIncus::start(
            &dir,
            FakeState {
                server: json!({}),
                projects: vec![],
                instances: vec![],
                networks: vec![],
                pools: vec![],
            },
        )
        .expect("fake");
        let host_socket = dir.join("host.sock");
        let cfg: NodeConfig = serde_json::from_value(json!({
            "stateDir": dir,
            "incus": {"socket": fake.socket()},
            "listen": {"unix": dir.join("arca.sock")},
            "tokens": {"hashDir": dir.join("tokens")},
            "kernel": {"projects": ["arcak-svc"], "droiddProjects": ["other"]},
            "hostNetwork": {"socket": host_socket, "status": dir.join("status.json")},
        }))
        .expect("config");
        let cfg = Arc::new(cfg);
        let root = Arc::new(StandIn {
            drifted: AtomicBool::new(true),
            applies: Mutex::new(0),
        });
        let stop = Arc::new(AtomicBool::new(false));
        if listening {
            let listener = socket::bind(&host_socket).expect("binds");
            let (ops, stop) = (Arc::clone(&root), Arc::clone(&stop));
            thread::spawn(move || socket::serve(&listener, ops, &stop));
        }
        let scratch = Scratch::new("hostnet-owner");
        let key = SkKey::from_seed(7);
        let plugins = PluginSet::new();
        let provider_socket = fake.socket().to_path_buf();
        let secrets = dir.join("secrets");
        let kernel = Arc::new(Mutex::new(
            Kernel::open(
                &dir,
                NodeKey::load_or_create(&dir.join("node.key")).expect("key"),
                plugins,
                verifier_for(&scratch, &[&key], "hostnet"),
                HOST.into(),
                None,
                move |retained| {
                    Ok(Box::new(
                        IncusProvider::new(
                            provider_socket,
                            Family {
                                projects: vec!["arcak-svc".into()],
                                scratch: false,
                                droidd_projects: vec!["other".into()],
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
        .collect();
        let executor = ActionExecutor::new(
            handlers
                .iter()
                .map(|(verb, handler)| (*verb, Arc::clone(handler))),
        );
        Rig {
            dir,
            _scratch: scratch,
            _fake: fake,
            kernel,
            key,
            handlers,
            executor,
            root,
            stop,
            nonce: AtomicU64::new(0),
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

    fn propose(&self) -> Result<Value, CallError> {
        self.call_as(
            Scope::Viewer,
            "action_propose",
            json!({ "action": "host_network_restore", "args": {} }),
        )
    }

    fn approve(&self, proposal: &Value) -> Result<Value, CallError> {
        let id = proposal["id"].as_str().expect("an id").to_string();
        let nonce = self.nonce.fetch_add(1, Ordering::SeqCst);
        let statement = arca::handlers::action_statement(
            HOST,
            &id,
            "host_network_restore",
            proposal["argsDigest"].as_str().expect("a digest"),
            NOW,
            &Digest::of(format!("{id}{nonce}").as_bytes()).to_hex()[..32],
        )
        .expect("the statement");
        let args = json!({ "plan": id, "approval": approve_statement(&self.key, &statement) });
        arca::handlers::run_approved_action(&self.kernel, &self.executor, &args, NOW)
            .map(|answer| answer["result"].clone())
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn refusal_code(result: Result<Value, CallError>) -> String {
    match result {
        Err(CallError::Refused { code, .. }) => code,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn the_card_says_what_drifted_and_the_approved_action_applies_and_reports_the_read_back() {
    let rig = Rig::new(true);
    let proposal = rig.propose().expect("proposed");
    assert_eq!(proposal["action"], "host_network_restore");
    let rows: Vec<(String, String)> = proposal["details"]
        .as_array()
        .expect("details")
        .iter()
        .map(|row| {
            (
                row["label"].as_str().unwrap_or_default().to_string(),
                row["value"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    assert_eq!(rows[0].0, "Host");
    assert_eq!(rows[1].0, "Now");
    assert!(rows[1].1.contains("the bridge incusbr0"), "{rows:?}");
    // Proposing changed nothing on the host.
    assert_eq!(*rig.root.applies.lock().expect("applies"), 0);

    let result = rig.approve(&proposal).expect("approved and run");
    assert_eq!(result["inSync"], true);
    assert_eq!(
        result["applied"],
        json!(["iptables -w 5 -I DOCKER-USER -i incusbr0 -j ACCEPT"])
    );
    assert_eq!(result["drift"], json!([]));
    assert_eq!(
        result["message"],
        "put back 1 setting; the host is as declared"
    );
    assert_eq!(*rig.root.applies.lock().expect("applies"), 1);
}

#[test]
fn a_host_already_as_declared_is_not_proposed_and_nothing_is_applied() {
    let rig = Rig::new(true);
    rig.root.drifted.store(false, Ordering::SeqCst);
    assert_eq!(refusal_code(rig.propose()), "NothingToRestore");
    assert_eq!(*rig.root.applies.lock().expect("applies"), 0);
}

#[test]
fn a_host_with_no_root_part_is_not_proposed_either() {
    let rig = Rig::new(false);
    assert_eq!(refusal_code(rig.propose()), "HostNetworkUnavailable");
}

#[test]
fn the_verb_runs_only_from_an_approved_proposal_and_each_approval_once() {
    let rig = Rig::new(true);
    // A call with no approval, and one with a made-up grant, apply nothing.
    let direct = rig.call_as(Scope::Doctor, "host_network_restore", json!({}));
    assert!(matches!(direct, Err(CallError::Invalid(_))), "{direct:?}");
    let forged = rig.call_as(
        Scope::Doctor,
        "host_network_restore",
        json!({ "approval": "forged" }),
    );
    assert_eq!(refusal_code(forged), "NotApproved");
    let viewer = rig.call_as(
        Scope::Viewer,
        "host_network_restore",
        json!({ "approval": "x" }),
    );
    assert!(matches!(viewer, Err(CallError::Forbidden { .. })));
    assert_eq!(*rig.root.applies.lock().expect("applies"), 0);
    // An approval is spent by the run it approved.
    let proposal = rig.propose().expect("proposed");
    rig.approve(&proposal).expect("runs");
    assert!(rig.approve(&proposal).is_err(), "a second use is refused");
    assert_eq!(*rig.root.applies.lock().expect("applies"), 1);
}
