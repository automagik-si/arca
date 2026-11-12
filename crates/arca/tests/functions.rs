//! Functions against a fake Incus: the owner's card, the one name space, secrets by name, a run and each of its
//! outcomes, the timer, the removal, and what a run does not write. No live host is touched.
mod common;

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arca::config::NodeConfig;
use arca::function_run::{FunctionRunner, RUNS_DIR};
use arca::handlers::{
    ActionExecutor, ActionSlot, Clock, plan_handlers_served, service_handlers_with,
};
use arca::kernel::Kernel;
use arca::lifecycle::InstanceLifecycle;
use arca::service::{FUNCTION_KEY, Service};
use arca_api::{Scope, conforms, verb};
use arca_incus::{FakeIncus, FakeState, IncusRead, ProjectFamily, UnixIncus};
use arca_kernel::canon::Digest;
use arca_kernel::identity::NodeKey;
use arca_mcp::{CallError, Caller, ToolHandler};
use arca_netproject::PROJECT;
use arca_providers::incus::{Family, IncusProvider};
use arca_reconcile::PluginSet;
use common::{HOST, Scratch, SkKey, approve_statement, verifier_for};
use serde_json::{Value, json};

const NOW: u64 = 1_793_361_600_000;
const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OK: &str = "published 3\n{\"ok\":true,\"published\":3}\n";
static SEQ: AtomicU64 = AtomicU64::new(0);

struct Rig {
    dir: PathBuf,
    _scratch: Scratch,
    fake: FakeIncus,
    kernel: Arc<Mutex<Kernel>>,
    key: SkKey,
    handlers: HashMap<&'static str, Arc<dyn ToolHandler>>,
    executor: ActionExecutor,
    runner: Arc<FunctionRunner>,
    cfg: Arc<NodeConfig>,
    now: Arc<AtomicU64>,
    nonce: AtomicU64,
}

impl Rig {
    fn new() -> Rig {
        Rig::with(json!([]))
    }

    /// A kernel-mode node with the managed service network over a fake Incus, whose config names `destinations` as its
    /// `kernel.serviceDestinations`, and every handler a served node builds over it.
    fn with(destinations: Value) -> Rig {
        let dir = std::env::temp_dir().join(format!(
            "arca-function-{}-{}",
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
        fake.add_profile(
            "default",
            json!({"name":"default","config":{},"devices":{"root":{"type":"disk","pool":"pool1","path":"/"}}}),
        );
        let cfg: NodeConfig = serde_json::from_value(json!({
            "stateDir": dir,
            "incus": {"socket": fake.socket()},
            "listen": {"unix": dir.join("arca.sock")},
            "tokens": {"hashDir": dir.join("tokens")},
            "kernel": {
                "projects": [PROJECT],
                "droiddProjects": ["other"],
                "serviceCidr": "192.0.2.0/24",
                "serviceDestinations": destinations,
            }
        }))
        .expect("config");
        let cfg = Arc::new(cfg);
        let scratch = Scratch::new("function-owner");
        let key = SkKey::from_seed(7);
        let mut plugins = PluginSet::new();
        plugins.add::<arca_netproject::NetProject>().expect("net");
        plugins.add::<Service>().expect("service");
        let socket = fake.socket().to_path_buf();
        let secrets = dir.join("secrets");
        let kernel = Arc::new(Mutex::new(
            Kernel::open(
                &dir,
                NodeKey::load_or_create(&dir.join("node.key")).expect("key"),
                plugins,
                verifier_for(&scratch, &[&key], "function"),
                HOST.into(),
                None,
                move |retained| {
                    Ok(Box::new(
                        IncusProvider::new(
                            socket,
                            Family {
                                projects: vec![PROJECT.into()],
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
        let now = Arc::new(AtomicU64::new(NOW));
        let ticking = Arc::clone(&now);
        let clock: Clock = Arc::new(move || ticking.load(Ordering::SeqCst));
        let cgroup = dir.join("cgroup");
        let runner = Arc::new(
            FunctionRunner::new(Arc::clone(&kernel), Arc::clone(&cfg), clock.clone())
                .expect("the runner")
                .with_pace(
                    Duration::from_millis(5),
                    Duration::from_secs(1),
                    Duration::ZERO,
                )
                .with_cgroup(Arc::new(move || Some(cgroup.clone()))),
        );
        let retained_kernel = Arc::clone(&kernel);
        let lifecycle = Arc::new(InstanceLifecycle::new(
            fake.socket().to_path_buf(),
            ProjectFamily::new([PROJECT], ["other"]).expect("family"),
            move || {
                retained_kernel
                    .lock()
                    .map_err(|_| "poisoned".to_string())?
                    .retained_entries()
                    .map_err(|error| error.to_string())
            },
            &dir,
        ));
        let handlers: HashMap<&'static str, Arc<dyn ToolHandler>> = plan_handlers_served(
            Arc::clone(&kernel),
            clock.clone(),
            None,
            None,
            None,
            Some(lifecycle),
            ActionSlot::default(),
            Some(Arc::clone(&cfg)),
        )
        .into_iter()
        .chain(service_handlers_with(
            Arc::clone(&kernel),
            clock,
            Arc::clone(&cfg),
            Some(Arc::clone(&runner)),
        ))
        .collect();
        let executor = ActionExecutor::new(
            handlers
                .iter()
                .map(|(verb, handler)| (*verb, Arc::clone(handler))),
        );
        let rig = Rig {
            dir,
            _scratch: scratch,
            fake,
            kernel,
            key,
            handlers,
            executor,
            runner,
            cfg,
            now,
            nonce: AtomicU64::new(0),
        };
        let templates = rig.dir.join("templates");
        fs::create_dir_all(&templates).expect("templates");
        fs::write(
            templates.join("nats.json"),
            json!({
                "name": "nats", "kind": "oci", "image": "docker.io/library/nats", "digest": DIGEST,
                "ports": [4222], "env": {}, "publishable": false
            })
            .to_string(),
        )
        .expect("template");
        rig
    }

    /// A call as a token of `scope` makes it. A result is checked against its verb's schema.
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

    fn call(&self, name: &str, args: Value) -> Result<Value, CallError> {
        self.call_as(Scope::Doctor, name, args)
    }

    fn propose(&self, action: &str, args: Value) -> Result<Value, CallError> {
        self.call_as(
            Scope::Viewer,
            "action_propose",
            json!({ "action": action, "args": args }),
        )
    }

    /// Proposes `action` as a viewer, has the owner approve it, and runs it once through the executor.
    fn approved(&self, action: &str, args: Value) -> Result<Value, CallError> {
        let proposal = self.propose(action, args)?;
        self.approve(action, &proposal)
    }

    /// Has the owner approve a proposal that waits, and runs it once through the executor.
    fn approve(&self, action: &str, proposal: &Value) -> Result<Value, CallError> {
        let args = self.approval(action, proposal);
        let result = arca::handlers::run_approved_action(&self.kernel, &self.executor, &args, NOW)
            .map(|answer| answer["result"].clone());
        result
    }

    /// The owner's signed approval of a proposal that waits, as the arguments that run it.
    fn approval(&self, action: &str, proposal: &Value) -> Value {
        let id = proposal["id"].as_str().expect("an id").to_string();
        let nonce = self.nonce.fetch_add(1, Ordering::SeqCst);
        let statement = arca::handlers::action_statement(
            HOST,
            &id,
            action,
            proposal["argsDigest"].as_str().expect("a digest"),
            NOW,
            &Digest::of(format!("{id}{nonce}").as_bytes()).to_hex()[..32],
        )
        .expect("the statement");
        let approval = approve_statement(&self.key, &statement);
        json!({ "plan": id, "approval": approval })
    }

    /// The fingerprint a function's record took of its image when it was created.
    fn fingerprint(&self, name: &str) -> String {
        self.call_as(Scope::Viewer, "function_list", json!({}))
            .expect("list")["functions"]
            .as_array()
            .expect("functions")
            .iter()
            .find(|function| function["name"] == name)
            .expect("the function")["fingerprint"]
            .as_str()
            .expect("a fingerprint")
            .to_string()
    }

    fn get(&self, path: &str) -> Option<Value> {
        UnixIncus::new(self.fake.socket().to_path_buf())
            .get(path)
            .ok()
    }

    fn instance(&self, name: &str) -> Value {
        self.get(&format!("/1.0/instances/{name}?project={PROJECT}"))
            .unwrap_or_else(|| panic!("no instance {name}"))
    }

    fn acl(&self, name: &str) -> Value {
        self.get(&format!(
            "/1.0/network-acls/{PROJECT}-svc-{name}?project=default"
        ))
        .unwrap_or_else(|| panic!("no ACL of {name}"))
    }

    /// The state changes the fake received, refused ones included.
    fn state_writes(&self) -> usize {
        self.fake
            .requests()
            .iter()
            .filter(|request| request.method == "PUT" && request.path.contains("/state"))
            .count()
    }

    /// Runs the function once, to its end, and returns its line.
    fn run(&self, name: &str) -> Value {
        let ticket = self.runner.begin(name, "manual").expect("a run begins");
        self.runner.run(ticket)
    }

    /// Waits until no run of `name` is going.
    fn settle(&self, name: &str) {
        let until = Instant::now() + Duration::from_secs(10);
        while self.runner.running(name) {
            assert!(Instant::now() < until, "the run of {name} never ended");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn entries(&self) -> Vec<arca_kernel::desired::Entry> {
        self.kernel.lock().expect("kernel").entries().to_vec()
    }

    fn runs(&self, name: &str) -> Vec<Value> {
        self.call_as(Scope::Viewer, "function_runs", json!({ "name": name }))
            .expect("runs")["runs"]
            .as_array()
            .expect("a list")
            .clone()
    }

    /// Every file of the state directory but the run log's, with its bytes: the desired state and the effect journal
    /// among them.
    fn state_files(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        fn walk(dir: &Path, skip: &Path, into: &mut BTreeMap<PathBuf, Vec<u8>>) {
            for entry in fs::read_dir(dir).expect("a directory").flatten() {
                let path = entry.path();
                if path == skip {
                    continue;
                }
                let kind = entry.file_type().expect("a kind");
                if kind.is_dir() {
                    walk(&path, skip, into);
                } else if kind.is_file() {
                    into.insert(path.clone(), fs::read(&path).expect("a file"));
                }
            }
        }
        let mut files = BTreeMap::new();
        walk(&self.dir, &self.dir.join(RUNS_DIR), &mut files);
        files
    }

    /// The service `nats`, running, as a service_create makes it.
    fn nats(&self) -> Value {
        self.call(
            "service_create",
            json!({ "name": "nats", "template": "nats" }),
        )
        .expect("the service nats")
    }

    /// The function `relay`, linked to nats, created through an approved proposal.
    fn relay(&self) -> Value {
        self.nats();
        self.approved("function_create", relay_args())
            .expect("the function relay")
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn relay_args() -> Value {
    json!({
        "name": "relay",
        "image": "registry.example/acme/relay",
        "digest": DIGEST,
        "command": ["/relay", "--once"],
        "env": { "BATCH": "500" },
        "links": [{ "to": "nats", "port": 4222 }],
        "triggers": [{ "timer": { "everySeconds": 30 } }],
    })
}

fn refusal(result: Result<Value, CallError>) -> (String, String) {
    match result {
        Err(CallError::Refused { code, message }) => (code, message),
        Err(CallError::Invalid(message)) => ("Invalid".to_string(), message),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn rules(acl: &Value, direction: &str) -> Vec<Value> {
    acl[direction].as_array().cloned().unwrap_or_default()
}

#[test]
fn a_function_is_created_only_from_an_approved_proposal_whose_card_names_what_it_reaches() {
    let rig = Rig::new();
    let nats = rig.nats();
    // A direct call creates nothing, whatever the token.
    let (code, message) = refusal(rig.call_as(Scope::Operator, "function_create", relay_args()));
    assert_eq!(code, "NotApproved", "{message}");
    assert!(
        rig.get(&format!("/1.0/instances/relay?project={PROJECT}"))
            .is_none()
    );

    // The proposal names every default, and its card says what the function is, when it runs and what it reaches.
    let proposal = rig
        .propose("function_create", relay_args())
        .expect("proposed");
    assert_eq!(
        proposal["summary"],
        "Create the function relay from registry.example/acme/relay and run it every 30 seconds"
    );
    assert_eq!(
        proposal["args"]["limits"],
        json!({ "seconds": 60, "memory": "128MiB", "cpu": 1 })
    );
    assert_eq!(proposal["args"]["kind"], "container");
    let rows: BTreeMap<String, String> = proposal["details"]
        .as_array()
        .expect("details")
        .iter()
        .map(|row| {
            (
                row["label"].as_str().unwrap().to_string(),
                row["value"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(rows["Function"], "relay");
    assert_eq!(
        rows["Program"],
        format!("registry.example/acme/relay@{DIGEST}")
    );
    assert_eq!(rows["Command"], "/relay");
    assert!(rows["Runs"].starts_with("every 30 seconds, one run at a time"));
    assert!(rows["Limits"].contains("at most 60 seconds, 128MiB of memory and 1 CPU"));
    assert!(rows["Secrets"].starts_with("None"));
    assert_eq!(
        rows["Services on this node"],
        "It can reach nats on tcp port 4222, and no other service"
    );
    assert!(rows["Private network"].starts_with("No:"));
    // The row says the one way out that stays: names are looked up through the node, which asks the internet.
    assert!(rows["Internet"].starts_with("No: it can open no connection to a public address."));
    assert!(
        rows["Internet"].contains(
            "names are looked up through this node's DNS, which asks the internet's name servers"
        ) && rows["Internet"].contains("send a little data out in the names it asks for"),
        "{}",
        rows["Internet"]
    );

    let created = rig
        .approved("function_create", relay_args())
        .expect("created");
    assert_eq!(created["name"], "relay");
    assert_eq!(created["running"], false);
    assert_eq!(created["lastRun"], Value::Null);
    assert_eq!(created["links"], json!([{ "to": "nats", "port": 4222 }]));

    // Its instance is the function's own container: marked, stopped, limited, and never started by Incus at boot.
    let instance = rig.instance("relay");
    let config = &instance["config"];
    assert_eq!(instance["status"], "Stopped");
    assert_eq!(config[FUNCTION_KEY], "true");
    assert_eq!(config["limits.cpu"], "1");
    assert_eq!(config["limits.memory"], "128MiB");
    assert_eq!(config["limits.memory.swap"], "false");
    assert_eq!(config["oci.entrypoint"], "/relay --once");
    assert_eq!(config["environment.BATCH"], "500");
    assert_eq!(config["boot.autostart"], "false", "{config}");
    // Its root is not read-only: Incus refuses to start an OCI container whose root is (seen on Incus 7.5.1).
    assert!(instance["devices"]["root"].get("readonly").is_none());
    assert_eq!(
        instance["devices"]["eth0"]["security.acls.default.egress.action"],
        "reject"
    );

    // What it may reach is one rule: tcp to nats on 4222. No rule lets anything else out.
    let egress = rules(&rig.acl("relay"), "egress");
    let allows: Vec<&Value> = egress
        .iter()
        .filter(|rule| rule["action"] == "allow")
        .collect();
    assert_eq!(allows.len(), 1, "{egress:?}");
    assert_eq!(
        allows[0]["destination"],
        format!("{}/32", nats["ipv4"].as_str().unwrap())
    );
    assert_eq!(allows[0]["protocol"], "tcp");
    assert_eq!(allows[0]["destination_port"], "4222");
    assert!(
        egress.iter().all(|rule| rule["description"] != "internet"),
        "{egress:?}"
    );
    // And nats lets it in on that port, from its address alone.
    let ingress = rules(&rig.acl("nats"), "ingress");
    assert!(
        ingress.iter().any(|rule| rule["action"] == "allow"
            && rule["source"] == format!("{}/32", created["ipv4"].as_str().unwrap())
            && rule["destination_port"] == "4222"),
        "{ingress:?}"
    );

    // function_list shows it; service_list does not, and neither shows it as an instance nobody recorded.
    let listed = rig
        .call_as(Scope::Viewer, "function_list", json!({}))
        .expect("list");
    assert_eq!(listed["functions"].as_array().unwrap().len(), 1);
    assert_eq!(
        listed["functions"][0]["triggers"],
        json!([{ "timer": { "everySeconds": 30 } }])
    );
    let services = rig.call("service_list", json!({})).expect("services");
    let names: Vec<&str> = services["services"]
        .as_array()
        .unwrap()
        .iter()
        .map(|service| service["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["nats"]);
}

#[test]
fn a_proposal_the_node_would_refuse_is_refused_before_the_owner_sees_it() {
    let rig = Rig::new();
    rig.nats();
    let with = |change: &dyn Fn(&mut Value)| {
        let mut args = relay_args();
        change(&mut args);
        refusal(rig.propose("function_create", args)).1
    };
    assert!(
        with(&|args| args["links"] = json!([{ "to": "pg1", "port": 5432 }]))
            .contains("no service named pg1 exists on this node")
    );
    assert!(
        with(&|args| args["links"] = json!([{ "to": "nats", "port": 5432 }]))
            .contains("the service nats does not declare tcp port 5432; it declares 4222")
    );
    assert!(with(&|args| args["digest"] = json!("latest")).starts_with("args"));
    assert!(
        with(&|args| args["triggers"] = json!([{ "timer": { "everySeconds": 5 } }]))
            .starts_with("args")
    );
    assert!(with(&|args| args["internet"] = json!(true)).starts_with("args"));
    // A caller cannot state a private destination: only the node's config names one.
    assert!(
        with(&|args| args["destinations"] = json!([{ "address": "10.4.0.9", "port": 443 }]))
            .contains("kernel.serviceDestinations")
    );
    assert!(rig.kernel.lock().unwrap().action_proposals().is_empty());
}

#[test]
fn functions_and_services_share_one_name_space_and_each_verb_keeps_to_its_kind() {
    let rig = Rig::new();
    rig.relay();
    // A function cannot take a service's name, and a service cannot take a function's.
    let mut taken = relay_args();
    taken["name"] = json!("nats");
    taken["links"] = json!([]);
    let (_, message) = refusal(rig.propose("function_create", taken));
    assert!(
        message
            .contains("a service named nats exists: functions and services share one name space"),
        "{message}"
    );
    let (_, message) = refusal(rig.propose("function_create", relay_args()));
    assert!(message.contains("function relay exists"), "{message}");
    let (_, message) = refusal(rig.call(
        "service_create",
        json!({ "name": "relay", "template": "nats" }),
    ));
    assert!(
        message
            .contains("a function named relay exists: functions and services share one name space"),
        "{message}"
    );
    // No service verb acts on a function.
    for (name, args) in [
        ("service_status", json!({ "name": "relay" })),
        ("service_logs", json!({ "name": "relay" })),
        (
            "service_secret_set",
            json!({ "name": "relay", "key": "A", "value": "12345678" }),
        ),
        ("service_connect", json!({ "from": "relay", "to": "nats" })),
        (
            "service_disconnect",
            json!({ "from": "relay", "to": "nats" }),
        ),
        ("service_connect", json!({ "from": "nats", "to": "relay" })),
        (
            "service_remove",
            json!({ "name": "relay", "approval": "x" }),
        ),
    ] {
        let (_, message) = refusal(rig.call(name, args));
        assert!(
            message.contains("relay is a function, not a service"),
            "{name}: {message}"
        );
    }
    // No function verb acts on a service.
    for (name, args) in [
        ("function_run", json!({ "name": "nats" })),
        ("function_runs", json!({ "name": "nats" })),
        (
            "function_secret_set",
            json!({ "name": "nats", "key": "A", "value": "12345678" }),
        ),
    ] {
        let (_, message) = refusal(rig.call(name, args));
        assert!(
            message.contains("nats is a service, not a function"),
            "{name}: {message}"
        );
    }
    let (_, message) = refusal(rig.propose("function_remove", json!({ "name": "nats" })));
    assert!(
        message.contains("nats is a service, not a function"),
        "{message}"
    );
    // A link goes to a service, never to a function.
    let mut second = relay_args();
    second["name"] = json!("second");
    second["links"] = json!([{ "to": "relay", "port": 1 }]);
    let (_, message) = refusal(rig.propose("function_create", second));
    assert!(
        message.contains("relay is a function, and a function serves nothing"),
        "{message}"
    );
    // The instance verbs leave a function's instance alone: only a run starts it.
    let writes = rig.state_writes();
    for name in ["instance_start", "instance_stop", "instance_restart"] {
        let args = json!({ "project": PROJECT, "instance": "relay" });
        let (code, message) = refusal(rig.propose(name, args.clone()));
        assert_eq!(code, "InstanceIsFunction", "{name}: {message}");
    }
    let (code, _) = refusal(rig.call(
        "instance_start",
        json!({ "project": PROJECT, "instance": "relay" }),
    ));
    assert_eq!(code, "InstanceIsFunction");
    assert_eq!(
        rig.state_writes(),
        writes,
        "nothing was sent to the function's instance"
    );
    assert_eq!(rig.instance("relay")["status"], "Stopped");
}

#[test]
fn a_secret_is_set_by_name_and_a_run_waits_for_it() {
    let rig = Rig::new();
    rig.nats();
    let mut args = relay_args();
    args["secrets"] = json!(["DATABASE_URL", "NATS_URL"]);
    let proposal = rig
        .propose("function_create", args.clone())
        .expect("proposed");
    let secrets = proposal["details"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["label"] == "Secrets")
        .expect("the row")["value"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        secrets.starts_with("DATABASE_URL, NATS_URL: each run is given these by name and no other")
    );
    let created = rig.approved("function_create", args).expect("created");
    assert_eq!(
        created["secrets"],
        json!([{ "name": "DATABASE_URL", "set": false }, { "name": "NATS_URL", "set": false }])
    );

    // With a secret missing nothing is started, and the line names what is missing.
    rig.fake.exit_after_start(PROJECT, "relay", 0, OK);
    let writes = rig.state_writes();
    let line = rig.run("relay");
    assert_eq!(line["outcome"], "notStarted");
    assert_eq!(
        line["why"],
        "the secrets DATABASE_URL, NATS_URL are not set: function_secret_set sets each"
    );
    assert_eq!(rig.state_writes(), writes);

    // A name the definition does not list is refused, in words that say which it lists.
    let (_, message) = refusal(rig.call(
        "function_secret_set",
        json!({ "name": "relay", "key": "OTHER", "value": "12345678" }),
    ));
    assert_eq!(
        message,
        "the function relay does not name the secret OTHER in its definition: it names DATABASE_URL, NATS_URL"
    );
    let (_, message) = refusal(rig.call(
        "function_secret_set",
        json!({ "name": "relay", "key": "NATS_URL", "value": "two\nlines-of-it" }),
    ));
    assert!(
        message.contains("8 to 4096 characters without control characters"),
        "{message}"
    );

    // A database address with a word in it, put together here: no secret-shaped text stands in this file.
    let word = ["s3cr3t", "pass"].join("-");
    let mut url = String::from("postgresql");
    url.push_str("://relay:");
    url.push_str(&word);
    url.push('@');
    url.push_str("192.0.2.20:5432/control");
    let url = url.as_str();
    rig.call(
        "function_secret_set",
        json!({ "name": "relay", "key": "DATABASE_URL", "value": url }),
    )
    .expect("set");
    let line = rig.run("relay");
    assert_eq!(
        line["why"],
        "the secret NATS_URL is not set: function_secret_set sets it"
    );
    rig.call(
        "function_secret_set",
        json!({ "name": "relay", "key": "NATS_URL", "value": "nats://192.0.2.10:4222" }),
    )
    .expect("set");
    // The run is given exactly its two secrets and its plain value, as environment.
    let config = rig.instance("relay")["config"].clone();
    assert_eq!(config["environment.DATABASE_URL"], url);
    assert_eq!(config["environment.NATS_URL"], "nats://192.0.2.10:4222");
    assert_eq!(config["environment.BATCH"], "500");
    let listed = rig.call("function_list", json!({})).expect("list");
    assert_eq!(
        listed["functions"][0]["secrets"],
        json!([{ "name": "DATABASE_URL", "set": true }, { "name": "NATS_URL", "set": true }])
    );
    // No answer shows a value again, and a program that prints one does not put it in the run log.
    assert!(!listed.to_string().contains(word.as_str()));
    rig.fake.exit_after_start(
        PROJECT,
        "relay",
        0,
        &format!("connecting to {url}\n{{\"ok\":true,\"url\":\"{url}\"}}\n"),
    );
    let line = rig.run("relay");
    assert_eq!(line["outcome"], "ok");
    assert!(!line.to_string().contains(word.as_str()), "{line}");
    let shown = rig
        .call(
            "function_runs",
            json!({ "name": "relay", "run": line["run"] }),
        )
        .expect("the run");
    assert_eq!(shown["output"].as_array().unwrap().len(), 2);
    assert!(!shown.to_string().contains(word.as_str()), "{shown}");
    let kept = fs::read_to_string(
        rig.dir
            .join(RUNS_DIR)
            .join("output/relay")
            .join(format!("{}.log", line["run"].as_str().unwrap())),
    )
    .expect("the kept output");
    assert!(!kept.contains(word.as_str()));
}

#[test]
fn ten_runs_write_ten_lines_and_nothing_to_the_journal_or_the_desired_state() {
    let rig = Rig::new();
    rig.relay();
    rig.fake.exit_after_start(PROJECT, "relay", 2, OK);
    let cgroup = rig
        .dir
        .join("cgroup")
        .join(format!("lxc.payload.{PROJECT}_relay"));
    fs::create_dir_all(&cgroup).expect("cgroup");
    fs::write(cgroup.join("memory.peak"), "6291456\n").expect("peak");
    fs::write(cgroup.join("cpu.stat"), "usage_usec 42000\n").expect("cpu");
    fs::write(cgroup.join("memory.events"), "oom 0\noom_kill 0\n").expect("events");

    let before = rig.state_files();
    let applied = rig.kernel.lock().unwrap().applied_seq();
    let entries = rig.kernel.lock().unwrap().entries().to_vec();
    let journal = fs::read(rig.dir.join(arca::kernel::JOURNAL_FILE)).expect("the effect journal");
    assert!(!journal.is_empty(), "the create is in the effect journal");
    for _ in 0..10 {
        let line = rig.run("relay");
        assert_eq!(line["outcome"], "ok", "{line}");
    }
    // The effect journal has the length it had, the desired state is what it was, and no other file of the state
    // directory changed by a byte: the run log is all a run writes.
    assert_eq!(rig.state_files(), before);
    assert_eq!(rig.kernel.lock().unwrap().applied_seq(), applied);
    assert_eq!(rig.kernel.lock().unwrap().entries(), entries.as_slice());
    let after = fs::read(rig.dir.join(arca::kernel::JOURNAL_FILE)).expect("the effect journal");
    assert_eq!(
        after.len(),
        journal.len(),
        "the effect journal has the length it had"
    );
    assert_eq!(after, journal);
    // To Incus each run sent three writes, in this order: the rebuild of its root from its image, the clearing of its
    // console log, and its start. Nothing else.
    let writes: Vec<String> = rig
        .fake
        .requests()
        .into_iter()
        .filter(|request| request.method != "GET")
        .skip_while(|request| !request.path.contains("/instances/relay/"))
        .map(|request| {
            format!(
                "{} {}",
                request.method,
                request.path.split('?').next().unwrap()
            )
        })
        .collect();
    let one_run = [
        "POST /1.0/instances/relay/rebuild",
        "DELETE /1.0/instances/relay/console",
        "PUT /1.0/instances/relay/state",
    ];
    assert_eq!(writes.len(), 30, "{writes:?}");
    for run in writes.chunks(3) {
        assert_eq!(run, one_run, "{writes:?}");
    }
    let image = rig.fingerprint("relay");
    assert_eq!(
        rig.fake.rebuilds(PROJECT, "relay"),
        vec![format!("store:{image}"); 10]
    );

    // One line a run, in the month's file, each a meter.
    // NOW is in October 2026: a month has its own file.
    let month = rig.dir.join(RUNS_DIR).join("runs-2026-10.jsonl");
    let text = fs::read_to_string(&month).expect("the run log");
    let lines: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 10);
    assert_eq!(
        fs::metadata(&month).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let line = &lines[9];
    assert_eq!(line["function"], "relay");
    assert_eq!(line["digest"], DIGEST);
    assert_eq!(line["kind"], "container");
    assert_eq!(line["trigger"], "manual");
    assert_eq!(line["startedAt"], NOW);
    assert!(
        line["startMs"].is_u64() && line["durationMs"].is_u64(),
        "{line}"
    );
    assert_eq!(line["root"], "store");
    assert_eq!(line["rootImage"], image.as_str());
    assert!(line["rootMs"].is_u64(), "{line}");
    assert_eq!(line["cpuMs"], 42);
    assert_eq!(line["memoryPeakBytes"], 6_291_456);
    assert_eq!(line["memoryLimitBytes"], 128 * 1024 * 1024);
    assert_eq!(line["result"], json!({ "ok": true, "published": 3 }));
    assert!(line.get("why").is_none());

    // function_runs reads them, newest first, and a new runner over the same directory still has them.
    let runs = rig.runs("relay");
    assert_eq!(runs.len(), 10);
    assert_eq!(runs[0], lines[9]);
    let limited = rig
        .call("function_runs", json!({ "name": "relay", "limit": 3 }))
        .expect("three");
    assert_eq!(limited["runs"].as_array().unwrap().len(), 3);
    let again = FunctionRunner::new(
        Arc::clone(&rig.kernel),
        Arc::clone(&rig.cfg),
        Arc::new(|| NOW),
    )
    .expect("a second runner");
    assert_eq!(again.runs("relay", 100).len(), 10);
    assert_eq!(again.runs("relay", 1)[0], lines[9]);
    let (_, message) =
        refusal(rig.call("function_runs", json!({ "name": "relay", "run": "fr-1-1" })));
    assert!(message.contains("no run fr-1-1"), "{message}");
}

#[test]
fn each_way_a_run_ends_is_an_outcome_in_its_line() {
    let rig = Rig::new();
    let mut args = relay_args();
    args["limits"] = json!({ "seconds": 1 });
    rig.nats();
    rig.approved("function_create", args).expect("created");

    // The program's own word.
    rig.fake.exit_after_start(
        PROJECT,
        "relay",
        1,
        "{\"ok\":false,\"error\":\"stream full\"}\n",
    );
    let line = rig.run("relay");
    assert_eq!(line["outcome"], "failed");
    assert_eq!(line["result"]["error"], "stream full");

    // It ended and said nothing a result is.
    rig.fake
        .exit_after_start(PROJECT, "relay", 1, "panicked at relay.rs:10\n");
    let line = rig.run("relay");
    assert_eq!(line["outcome"], "died");
    assert_eq!(line["result"], Value::Null);
    assert!(
        line["why"]
            .as_str()
            .unwrap()
            .contains("ended without a result")
    );
    let shown = rig
        .call(
            "function_runs",
            json!({ "name": "relay", "run": line["run"] }),
        )
        .expect("the run");
    assert_eq!(shown["output"], json!(["panicked at relay.rs:10"]));

    // It was killed at its memory limit: the cgroup counted the kill.
    let cgroup = rig
        .dir
        .join("cgroup")
        .join(format!("lxc.payload.{PROJECT}_relay"));
    fs::create_dir_all(&cgroup).expect("cgroup");
    fs::write(cgroup.join("memory.events"), "oom 1\noom_kill 1\n").expect("events");
    fs::write(cgroup.join("memory.peak"), "134217728\n").expect("peak");
    rig.fake.exit_after_start(PROJECT, "relay", 1, "working\n");
    let line = rig.run("relay");
    assert_eq!(line["outcome"], "memory");
    assert_eq!(line["memoryPeakBytes"], 134_217_728u64);
    fs::remove_dir_all(&cgroup).expect("gone");

    // It passed its one second: the kernel stops it. This one ignores the stop, so it is killed.
    rig.fake.run_until_stopped(PROJECT, "relay");
    rig.fake.hang_stop("relay", false);
    let began = Instant::now();
    let line = rig.run("relay");
    assert!(began.elapsed() >= Duration::from_secs(1));
    assert_eq!(line["outcome"], "timeout");
    assert_eq!(
        line["why"],
        "it was still running after 1 second, its time limit, and was killed"
    );
    assert!(line["durationMs"].as_u64().unwrap() >= 1000, "{line}");
    assert_eq!(rig.instance("relay")["status"], "Stopped");
    // The meter has no reading where there was nothing to read.
    assert_eq!(line["memoryPeakBytes"], Value::Null);
    assert_eq!(line["cpuMs"], Value::Null);
}

#[test]
fn a_run_starts_only_the_instance_arca_created_for_its_function() {
    let rig = Rig::new();
    rig.relay();
    rig.fake.exit_after_start(PROJECT, "relay", 0, OK);
    assert_eq!(rig.run("relay")["outcome"], "ok");
    // Someone takes the mark off: it is no longer a function's instance, and no run touches it.
    let writes = rig.state_writes();
    let mut body = rig.instance("relay");
    body["config"].as_object_mut().unwrap().remove(FUNCTION_KEY);
    put(
        &rig,
        &format!("/1.0/instances/relay?project={PROJECT}"),
        &body,
    );
    let line = rig.run("relay");
    assert_eq!(line["outcome"], "notStarted");
    assert!(
        line["why"].as_str().unwrap().contains("not a function's"),
        "{line}"
    );
    assert_eq!(rig.state_writes(), writes);
    // The mark back, but on another instance of the same name: its uuid is not the one the journal holds.
    body["config"][FUNCTION_KEY] = json!("true");
    body["config"]["volatile.uuid"] = json!("00000000-0000-4000-8000-00000000dead");
    put(
        &rig,
        &format!("/1.0/instances/relay?project={PROJECT}"),
        &body,
    );
    let line = rig.run("relay");
    assert_eq!(line["outcome"], "notStarted");
    assert_eq!(
        line["why"],
        "its instance is not the one Arca created for it, so nothing was started"
    );
    assert_eq!(rig.instance("relay")["status"], "Stopped");
}

/// The word the owner of the linked database signs in with, in the tests that rotate it.
const OWNERS_WORD: &str = "owner-word-of-pg";

/// A database address with a user and a word in it, put together when the test runs.
fn database_url(user: &str, word: &str, database: &str) -> String {
    let mut url = String::from("postgresql");
    url.push_str("://");
    url.push_str(user);
    url.push(':');
    url.push_str(word);
    url.push('@');
    url.push_str("192.0.2.10:5432/");
    url.push_str(database);
    url
}

/// A Postgres-like template: a secret of its own, and a connection value it hands the services linked to it.
fn pg_template() -> Value {
    json!({
        "name": "pg", "kind": "oci", "image": "registry.example/acme/pg", "digest": DIGEST,
        "ports": [5432],
        "env": { "POSTGRES_PASSWORD": "{{secret:POSTGRES_PASSWORD}}" },
        "secrets": ["POSTGRES_PASSWORD"],
        "provides": { "env": { "DATABASE_URL": database_url("app", "{{secret:POSTGRES_PASSWORD}}", "app")
            .replace("192.0.2.10:5432", "{{host}}:{{port}}") } },
        "publishable": false
    })
}

#[test]
fn a_linked_services_secret_never_enters_a_functions_environment() {
    let rig = Rig::new();
    fs::write(rig.dir.join("templates/pg.json"), pg_template().to_string()).expect("template");
    rig.call("service_create", json!({ "name": "pg", "template": "pg" }))
        .expect("the service pg");
    // One function names DATABASE_URL as its own secret, set to a narrow role's. The other names no secret at all.
    let mut relay = relay_args();
    relay["links"] = json!([{ "to": "pg", "port": 5432 }]);
    relay["secrets"] = json!(["DATABASE_URL"]);
    rig.approved("function_create", relay).expect("relay");
    let mut tick = relay_args();
    tick["name"] = json!("tick");
    tick["links"] = json!([{ "to": "pg", "port": 5432 }]);
    rig.approved("function_create", tick).expect("tick");
    let narrow = database_url(
        "relay_only",
        &["narrow", "role", "word"].join("-"),
        "control",
    );
    rig.call(
        "function_secret_set",
        json!({ "name": "relay", "key": "DATABASE_URL", "value": narrow }),
    )
    .expect("set");
    let records = |rig: &Rig| -> Vec<Value> {
        rig.entries()
            .into_iter()
            .filter(|entry| ["service-relay", "service-tick"].contains(&entry.id.0.as_str()))
            .map(|entry| entry.config)
            .collect()
    };
    let before = records(&rig);
    let instances = (
        rig.instance("relay")["config"].clone(),
        rig.instance("tick")["config"].clone(),
    );

    // The linked service's own secret is set, as a routine rotation sets it. A service linked to pg would be handed
    // pg's DATABASE_URL, the owner of the database's. A function is handed nothing.
    rig.call(
        "service_secret_set",
        json!({ "name": "pg", "key": "POSTGRES_PASSWORD", "value": OWNERS_WORD }),
    )
    .expect("the provider's secret is set");
    assert_eq!(records(&rig), before, "no function's record changed");
    assert_eq!(rig.instance("relay")["config"], instances.0);
    assert_eq!(rig.instance("tick")["config"], instances.1);
    assert_eq!(
        rig.instance("relay")["config"]["environment.DATABASE_URL"],
        narrow
    );
    assert!(
        rig.instance("tick")["config"]
            .get("environment.DATABASE_URL")
            .is_none()
    );
    assert!(
        !rig.instance("relay")["config"]
            .to_string()
            .contains(OWNERS_WORD)
    );

    // And whatever else tried to put a name into a function's environment, the plugin refuses the record: a run is
    // given what the definition names and nothing else.
    let owners = database_url("app", OWNERS_WORD, "app");
    for (key, value) in [("DATABASE_URL", owners.as_str()), ("EXTRA", "anything")] {
        let mut entry = rig
            .entries()
            .into_iter()
            .find(|entry| entry.id.0 == "service-tick")
            .expect("tick's record");
        entry.config["environment"][key] = json!(value);
        let outcomes = rig
            .kernel
            .lock()
            .unwrap()
            .propose(vec![arca::desired_store::EntryChange::Put(entry)], NOW)
            .expect("the document is taken");
        let said = format!("{outcomes:?}");
        assert!(
            said.contains("Failed") && said.contains("a function is a container"),
            "{said}"
        );
        assert!(
            rig.instance("tick")["config"]
                .get(format!("environment.{key}"))
                .is_none()
        );
    }
}

#[test]
fn a_service_a_function_links_to_is_not_removed_until_the_function_is() {
    let rig = Rig::new();
    rig.relay();
    let said = "remove service nats: the function relay links to it, and what a function reaches is part of what its owner approved. Remove the function relay first (function_remove); nothing was removed, and no approval was spent";
    // Refused when it is proposed, so the owner is never shown the card.
    let (_, message) = refusal(rig.propose("service_remove", json!({ "name": "nats" })));
    assert_eq!(message, said);
    assert!(rig.kernel.lock().unwrap().action_proposals().is_empty());
    // And in the removal itself, before any approval is looked at.
    let (_, message) = refusal(rig.call(
        "service_remove",
        json!({ "name": "nats", "approval": "not-an-approval" }),
    ));
    assert_eq!(message, said);
    // Nothing of either changed.
    assert_eq!(rig.instance("nats")["status"], "Running");
    let record = rig
        .entries()
        .into_iter()
        .find(|entry| entry.id.0 == "service-relay")
        .expect("relay's record");
    assert_eq!(record.config["links"][0]["to"], "nats");
    rig.fake.exit_after_start(PROJECT, "relay", 0, OK);
    assert_eq!(rig.run("relay")["outcome"], "ok");
    // With the function gone the service is removed as any other.
    rig.approved("function_remove", json!({ "name": "relay" }))
        .expect("the function is removed");
    rig.approved("service_remove", json!({ "name": "nats" }))
        .expect("the service is removed");
    assert!(
        rig.get(&format!("/1.0/instances/nats?project={PROJECT}"))
            .is_none()
    );
}

#[test]
fn two_approved_actions_in_either_order_never_leave_a_function_linked_to_nothing() {
    let rig = Rig::new();
    rig.nats();
    // The owner is shown the removal of nats while no function links to it, and the card waits.
    let removal = rig
        .propose("service_remove", json!({ "name": "nats" }))
        .expect("the removal is proposed");
    // Meanwhile a function that links to nats is created.
    rig.approved("function_create", relay_args())
        .expect("the function relay");
    let before = rig.entries();
    // The kernel itself refuses the removal, whoever calls it and whatever was checked before: it is asked directly
    // here, as the removal's handler asks it once it holds the write gate.
    let refused = {
        let mut kernel = rig.kernel.lock().expect("kernel");
        let spent = kernel.spend_service_removal_authorized(&arca_kernel::desired::EntryId(
            "service-nats".to_string(),
        ));
        kernel.remove_service_spent(spent, NOW)
    };
    assert!(
        matches!(&refused, Err(arca::kernel::ServiceRemovalError::LinkedFunctions(names)) if names == &["relay".to_string()]),
        "the kernel refuses, and names the function"
    );
    assert_eq!(rig.entries(), before, "no record changed");
    // Now the owner approves the card that waited. The approval is used up, and the refusal says so.
    let (_, message) = refusal(rig.approve("service_remove", &removal));
    assert_eq!(
        message,
        "The approval was accepted, but service_remove did not complete: remove service nats: the function relay links to it, and what a function reaches is part of what its owner approved. Remove the function relay first (function_remove); nothing was removed, and the owner's approval is spent, so another try needs a new one"
    );
    assert_eq!(rig.entries(), before);
    assert_eq!(rig.instance("nats")["status"], "Running");
    // The function is whole: its record is applied as it stands, and it runs.
    rig.fake.exit_after_start(PROJECT, "relay", 0, OK);
    assert_eq!(rig.run("relay")["outcome"], "ok");
    let egress = rules(&rig.acl("relay"), "egress");
    assert!(
        egress
            .iter()
            .any(|rule| rule["action"] == "allow" && rule["destination_port"] == "4222")
    );
}

#[test]
fn a_removal_that_passed_its_own_check_is_refused_by_the_kernel_and_the_handler_says_so() {
    let rig = Rig::new();
    // A function linked to nats is made and removed again: its record is kept for later.
    rig.relay();
    let record = rig
        .entries()
        .into_iter()
        .find(|entry| entry.id.0 == "service-relay")
        .expect("relay's record");
    rig.approved("function_remove", json!({ "name": "relay" }))
        .expect("removed");
    // The owner approves the removal of nats while no function links to it. The removal reads the node, passes its
    // own check, and then waits for the write gate, which this test holds.
    let removal = rig
        .propose("service_remove", json!({ "name": "nats" }))
        .expect("proposed");
    let args = rig.approval("service_remove", &removal);
    let gate = rig.kernel.lock().expect("kernel").write_gate();
    let held = gate.lock().expect("the write gate");
    let listings = |rig: &Rig| {
        rig.fake
            .requests()
            .iter()
            .filter(|request| {
                request.method == "GET" && request.path.starts_with("/1.0/instances?")
            })
            .count()
    };
    let before = listings(&rig);
    let (kernel, executor) = (Arc::clone(&rig.kernel), rig.executor.clone());
    let removing = std::thread::spawn(move || {
        arca::handlers::run_approved_action(&kernel, &executor, &args, NOW)
    });
    // The removal has read the node's instances, which it does just before it reads the entries and checks them.
    let until = Instant::now() + Duration::from_secs(10);
    while listings(&rig) == before {
        assert!(Instant::now() < until, "the removal never read the node");
        std::thread::sleep(Duration::from_millis(5));
    }
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !removing.is_finished(),
        "the removal waits for the write gate"
    );
    // Meanwhile the function comes to be, linked to nats, as a create that held the gate would have left it.
    let outcomes = rig
        .kernel
        .lock()
        .expect("kernel")
        .propose(vec![arca::desired_store::EntryChange::Put(record)], NOW)
        .expect("the function's record is taken");
    assert!(!format!("{outcomes:?}").contains("Failed"), "{outcomes:?}");
    let entries = rig.entries();
    drop(held);
    // The kernel refuses under its own lock, and the handler says it in the owner's words, approval and all.
    let (_, message) = refusal(removing.join().expect("the removal answers"));
    assert_eq!(
        message,
        "The approval was accepted, but service_remove did not complete: remove service nats: the function relay links to it, and what a function reaches is part of what its owner approved. Remove the function relay first (function_remove); nothing was removed, and the owner's approval is spent, so another try needs a new one"
    );
    assert_eq!(rig.entries(), entries, "no record changed");
    assert_eq!(rig.instance("nats")["status"], "Running");
    assert_eq!(rig.instance("relay")["status"], "Stopped");
}

#[test]
fn a_create_that_fails_is_said_as_a_functions_with_the_verbs_that_act_on_one() {
    let rig = Rig::new();
    rig.nats();
    // Incus refuses the new instance.
    rig.fake.set_reject_config_on_post("/1.0/instances");
    let (_, message) = refusal(rig.approved("function_create", relay_args()));
    assert!(
        message.starts_with(
            "The approval was accepted, but function_create did not complete: create function relay: "
        ),
        "{message}"
    );
    for word in [
        "service relay",
        "template function",
        "instance_start",
        "service_remove",
        "service_status",
    ] {
        assert!(!message.contains(word), "{word} is in: {message}");
    }
    assert!(message.ends_with("nothing was left behind"), "{message}");
    assert!(
        rig.get(&format!("/1.0/instances/relay?project={PROJECT}"))
            .is_none()
    );
    assert!(
        rig.call("function_list", json!({})).expect("list")["functions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_secret_the_program_prints_escaped_or_spaced_is_in_no_line_result_or_output() {
    let rig = Rig::new();
    rig.nats();
    let mut args = relay_args();
    args["secrets"] = json!(["TOKEN", "PHRASE"]);
    rig.approved("function_create", args).expect("created");
    // One value a JSON encoder must escape, one with two spaces in a row.
    let token = r#"Tr0ub4dor&3"x\yz+a='b"#;
    let phrase = "correct  horse battery";
    for (key, value) in [("TOKEN", token), ("PHRASE", phrase)] {
        rig.call(
            "function_secret_set",
            json!({ "name": "relay", "key": key, "value": value }),
        )
        .expect("set");
    }
    // The program prints both plainly, then a result line as two encoders write it: one escapes the quote and the
    // backslash, the other writes the ampersand as \u0026 too. The value is a name of the object as well.
    let outputs = [
        format!(
            "auth failed for {token}\nphrase was {phrase}\n{}\n",
            json!({ "ok": false, "error": format!("auth failed for {token}"), token: 1, "nested": [{ "p": phrase }] })
        ),
        "{\"ok\":false,\"error\":\"auth failed for Tr0ub4dor\\u00263\\\"x\\\\yz\\u002Ba\\u003d\\u0027b\"}\n".to_string(),
    ];
    for output in outputs {
        rig.fake.exit_after_start(PROJECT, "relay", 0, &output);
        let line = rig.run("relay");
        assert_eq!(line["outcome"], "failed", "{line}");
        let shown = rig
            .call_as(
                Scope::Viewer,
                "function_runs",
                json!({ "name": "relay", "run": line["run"] }),
            )
            .expect("the run");
        let listed = rig
            .call_as(Scope::Viewer, "function_list", json!({}))
            .expect("list");
        let kept = fs::read_to_string(
            rig.dir
                .join(RUNS_DIR)
                .join("output/relay")
                .join(format!("{}.log", line["run"].as_str().unwrap())),
        )
        .expect("the kept output");
        let month =
            fs::read_to_string(rig.dir.join(RUNS_DIR).join("runs-2026-10.jsonl")).expect("log");
        for text in [
            line.to_string(),
            shown.to_string(),
            listed.to_string(),
            kept,
            month,
        ] {
            for part in ["Tr0ub4dor", "x\\yz", "horse", "a='b", "003d"] {
                assert!(!text.contains(part), "{part} is in: {text}");
            }
        }
        assert!(
            line["result"]["error"]
                .as_str()
                .unwrap()
                .starts_with("auth failed for ")
        );
    }
}

#[test]
fn a_run_through_the_verb_and_one_through_the_timer_write_only_the_run_log() {
    let rig = Rig::new();
    rig.relay();
    rig.fake.exit_after_start(PROJECT, "relay", 1, OK);
    let before = rig.state_files();
    let journal = fs::read(rig.dir.join(arca::kernel::JOURNAL_FILE)).expect("the effect journal");
    let entries = rig.entries();
    // The verb, as a doctor token calls it, and then the timer's own tick.
    rig.call("function_run", json!({ "name": "relay" }))
        .expect("started");
    rig.settle("relay");
    rig.now.store(NOW + 30_000, Ordering::SeqCst);
    assert_eq!(rig.runner.tick(NOW + 30_000).len(), 1);
    rig.settle("relay");
    // And the two reads a viewer makes of them.
    assert_eq!(rig.runs("relay").len(), 2);
    rig.call_as(Scope::Viewer, "function_list", json!({}))
        .expect("list");
    assert_eq!(
        rig.state_files(),
        before,
        "no file outside the run log changed"
    );
    assert_eq!(
        fs::read(rig.dir.join(arca::kernel::JOURNAL_FILE)).expect("the journal"),
        journal
    );
    assert_eq!(rig.entries(), entries);
    let triggers: Vec<String> = rig
        .runs("relay")
        .iter()
        .map(|run| {
            format!(
                "{}:{}",
                run["trigger"].as_str().unwrap(),
                run["outcome"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(triggers, ["timer:ok", "manual:ok"]);
}

#[test]
fn a_node_with_no_function_has_no_run_log_and_one_appears_with_the_first_run() {
    let rig = Rig::new();
    rig.nats();
    // Ticks, a sweep and the reads leave no directory behind.
    assert!(rig.runner.tick(NOW).is_empty());
    assert!(rig.runner.sweep().is_empty());
    rig.call_as(Scope::Viewer, "function_list", json!({}))
        .expect("list");
    assert!(!rig.dir.join(RUNS_DIR).exists());
    rig.approved("function_create", relay_args())
        .expect("created");
    assert!(
        !rig.dir.join(RUNS_DIR).exists(),
        "a create writes no run log either"
    );
    rig.fake.exit_after_start(PROJECT, "relay", 0, OK);
    rig.run("relay");
    let dir = rig.dir.join(RUNS_DIR);
    assert_eq!(
        fs::metadata(&dir)
            .expect("the run log's directory")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert!(dir.join("runs-2026-10.jsonl").is_file());
}

#[test]
fn every_run_starts_from_its_images_own_root_or_does_not_start() {
    let rig = Rig::new();
    rig.relay();
    rig.fake.exit_after_start(PROJECT, "relay", 0, OK);
    let image = rig.fingerprint("relay");
    let instance = format!("/1.0/instances/relay?project={PROJECT}");
    let clears = |rig: &Rig| {
        rig.fake
            .requests()
            .iter()
            .filter(|request| request.method == "DELETE" && request.path.contains("/console"))
            .count()
    };
    // The node holds the copy the record took at create: the root is rebuilt from it, and no registry is asked.
    let line = rig.run("relay");
    assert_eq!(
        (&line["outcome"], &line["root"], &line["rootImage"]),
        (&json!("ok"), &json!("store"), &json!(image))
    );
    // Someone who may write the instance points it at another image of the store. The run does not follow: it
    // rebuilds from the image its record names, and its line says which.
    let other = rig.fake.add_image(PROJECT, "another-image");
    let mut body = rig.instance("relay");
    body["config"]["volatile.base_image"] = json!(other);
    send(&rig, "PUT", &instance, &body);
    let line = rig.run("relay");
    assert_eq!(line["rootImage"], image.as_str());
    assert_eq!(
        rig.fake.rebuilds(PROJECT, "relay"),
        vec![format!("store:{image}"); 2]
    );
    // The copy is there and Incus cannot rebuild from it: the run does not start, and the registry is not asked.
    rig.fake.lose_image(PROJECT, "relay");
    let (writes, cleared) = (rig.state_writes(), clears(&rig));
    let line = rig.run("relay");
    assert_eq!(line["outcome"], "notStarted");
    assert!(
        line["why"].as_str().unwrap().starts_with(
            "its root could not be put back to its image's own, so nothing was started: "
        ),
        "{line}"
    );
    assert_eq!(rig.fake.rebuilds(PROJECT, "relay").len(), 2);
    assert_eq!((rig.state_writes(), clears(&rig)), (writes, cleared));
    rig.fake.allow_rebuild(PROJECT, "relay");
    // The copy is gone from the store: the rebuild names the registry and the digest the owner approved, and Incus
    // fetches the image again. The run goes on, and its line says the registry served it.
    send(
        &rig,
        "DELETE",
        &format!("/1.0/images/{image}?project={PROJECT}"),
        &json!({}),
    );
    let line = rig.run("relay");
    assert_eq!(
        (&line["outcome"], &line["root"], &line["rootImage"]),
        (&json!("ok"), &json!("registry"), &Value::Null)
    );
    assert_eq!(
        rig.fake.rebuilds(PROJECT, "relay").last().unwrap(),
        "registry"
    );
    // Neither the store nor the registry: the run does not start, and its line says why.
    rig.fake.refuse_rebuild(PROJECT, "relay");
    let (writes, cleared) = (rig.state_writes(), clears(&rig));
    let line = rig.run("relay");
    assert_eq!(line["outcome"], "notStarted");
    assert_eq!(line["root"], Value::Null);
    assert!(
        line["why"].as_str().unwrap().contains(
            "the node's own copy of the image is gone, and the registry did not give it again"
        ),
        "{line}"
    );
    assert_eq!((rig.state_writes(), clears(&rig)), (writes, cleared));
    rig.fake.allow_rebuild(PROJECT, "relay");
    assert_eq!(rig.run("relay")["root"], "registry");
    // A record whose image is not the one its definition names is refused by the plugin, and no run starts on it.
    let mut entry = rig
        .entries()
        .into_iter()
        .find(|entry| entry.id.0 == "service-relay")
        .expect("relay's record");
    entry.config["source"]["alias"] = json!(format!("acme/other@sha256:{}", "b".repeat(64)));
    let outcomes = rig
        .kernel
        .lock()
        .unwrap()
        .propose(vec![arca::desired_store::EntryChange::Put(entry)], NOW)
        .expect("the document is taken");
    assert!(format!("{outcomes:?}").contains("Failed"), "{outcomes:?}");
    let before = rig.fake.rebuilds(PROJECT, "relay").len();
    let line = rig.run("relay");
    assert_eq!(
        line["why"],
        "its record's image is not the one its definition names, so nothing was started"
    );
    assert_eq!(rig.fake.rebuilds(PROJECT, "relay").len(), before);
}

/// An edit made by an Incus operator, outside the kernel's writer.
fn put(rig: &Rig, path: &str, body: &Value) {
    send(rig, "PUT", path, body);
}

/// A request an Incus operator makes, outside the kernel's writer.
fn send(rig: &Rig, method: &str, path: &str, body: &Value) {
    use std::io::{Read, Write};
    let mut stream = std::os::unix::net::UnixStream::connect(rig.fake.socket()).unwrap();
    let body = body.to_string();
    write!(stream, "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(
        response.starts_with("HTTP/1.1 200") || response.starts_with("HTTP/1.1 202"),
        "fixture edit failed: {}",
        response.lines().next().unwrap()
    );
}

#[test]
fn the_timer_runs_a_function_when_it_is_due_and_never_two_runs_at_once() {
    let rig = Rig::new();
    rig.relay();
    // The first tick finds it due. It stays running for a while.
    rig.fake.exit_after_start(PROJECT, "relay", 60, OK);
    let begun = rig.runner.tick(NOW);
    assert_eq!(begun.len(), 1);
    assert!(rig.runner.running("relay"));
    // While it runs: a later tick starts nothing, even past its timer, and a manual run is refused in words.
    assert!(rig.runner.tick(NOW + 31_000).is_empty());
    let (code, message) = refusal(rig.call("function_run", json!({ "name": "relay" })));
    assert_eq!(code, "FunctionBusy");
    assert!(message.contains("one runs at a time"), "{message}");
    rig.settle("relay");
    let runs = rig.runs("relay");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["trigger"], "timer");
    assert_eq!(runs[0]["run"], begun[0].as_str());
    // Not due again until thirty seconds after its last start.
    rig.fake.exit_after_start(PROJECT, "relay", 0, OK);
    assert!(rig.runner.tick(NOW + 29_999).is_empty());
    rig.now.store(NOW + 30_000, Ordering::SeqCst);
    assert_eq!(rig.runner.tick(NOW + 30_000).len(), 1);
    rig.settle("relay");
    // A manual run counts as a start too, and answers at once with the run's id.
    rig.now.store(NOW + 40_000, Ordering::SeqCst);
    let started = rig
        .call("function_run", json!({ "name": "relay" }))
        .expect("started");
    assert_eq!(started["started"], true);
    rig.settle("relay");
    assert!(rig.runner.tick(NOW + 60_000).is_empty());
    assert_eq!(rig.runner.tick(NOW + 70_000).len(), 1);
    rig.settle("relay");
    let runs = rig.runs("relay");
    let triggers: Vec<&str> = runs
        .iter()
        .map(|run| run["trigger"].as_str().unwrap())
        .collect();
    assert_eq!(triggers, ["timer", "manual", "timer", "timer"]);
    assert!(runs.iter().all(|run| run["outcome"] == "ok"), "{runs:?}");
    let (_, message) = refusal(rig.call("function_run", json!({ "name": "nobody" })));
    assert!(message.contains("function nobody: not found"), "{message}");
}

#[test]
fn a_kernel_that_starts_stops_a_run_nobody_watches() {
    let rig = Rig::new();
    rig.relay();
    // A kernel that is gone started it, and it still runs.
    put(
        &rig,
        &format!("/1.0/instances/relay/state?project={PROJECT}"),
        &json!({ "action": "start" }),
    );
    assert_eq!(rig.instance("relay")["status"], "Running");
    assert_eq!(rig.runner.sweep(), ["relay"]);
    assert_eq!(rig.instance("relay")["status"], "Stopped");
    let runs = rig.runs("relay");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["outcome"], "interrupted");
    assert_eq!(runs[0]["trigger"], "kernel");
    // The service beside it runs on: a sweep is for functions.
    assert_eq!(rig.instance("nats")["status"], "Running");
    assert!(rig.runner.sweep().is_empty());
    // An instance of the function's name and mark that is not the one Arca created is left alone, running: the
    // sweep checks the uuid the journal holds, as a run does.
    put(
        &rig,
        &format!("/1.0/instances/relay/state?project={PROJECT}"),
        &json!({ "action": "start" }),
    );
    let mut body = rig.instance("relay");
    body["config"]["volatile.uuid"] = json!("00000000-0000-4000-8000-00000000dead");
    put(
        &rig,
        &format!("/1.0/instances/relay?project={PROJECT}"),
        &body,
    );
    assert!(rig.runner.sweep().is_empty());
    assert_eq!(rig.instance("relay")["status"], "Running");
    assert_eq!(rig.runs("relay").len(), 1);
}

#[test]
fn a_function_is_removed_only_from_an_approved_proposal_and_its_run_is_stopped_first() {
    let rig = Rig::new();
    let mut args = relay_args();
    args["secrets"] = json!(["NATS_URL"]);
    rig.nats();
    rig.approved("function_create", args).expect("created");
    rig.call(
        "function_secret_set",
        json!({ "name": "relay", "key": "NATS_URL", "value": "nats://192.0.2.10:4222" }),
    )
    .expect("set");
    let (code, _) = refusal(rig.call_as(
        Scope::Operator,
        "function_remove",
        json!({ "name": "relay", "approval": "not-a-grant" }),
    ));
    assert_eq!(code, "NotApproved");
    let (_, message) = refusal(rig.propose("function_remove", json!({ "name": "nobody" })));
    assert!(message.contains("function nobody: not found"), "{message}");

    // A run is going when the owner approves the removal.
    rig.fake.run_until_stopped(PROJECT, "relay");
    rig.runner.tick(NOW);
    let until = Instant::now() + Duration::from_secs(5);
    while rig.instance("relay")["status"] != "Running" {
        assert!(Instant::now() < until, "the run never started");
        std::thread::sleep(Duration::from_millis(5));
    }
    let removed = rig
        .approved("function_remove", json!({ "name": "relay" }))
        .expect("removed");
    assert_eq!(removed["removed"], true);
    assert!(
        rig.get(&format!("/1.0/instances/relay?project={PROJECT}"))
            .is_none()
    );
    assert!(
        rig.get(&format!(
            "/1.0/network-acls/{PROJECT}-svc-relay?project=default"
        ))
        .is_none()
    );
    assert!(!rig.dir.join("secrets/service-relay").exists());
    // nats no longer lets it in, and still runs.
    let ingress = rules(&rig.acl("nats"), "ingress");
    assert!(
        ingress
            .iter()
            .all(|rule| rule["destination_port"] != "4222"),
        "{ingress:?}"
    );
    assert_eq!(rig.instance("nats")["status"], "Running");
    assert!(
        rig.call("function_list", json!({})).expect("list")["functions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    // The stopped run is in the run log, which is kept: runs are billed from it.
    let text = fs::read_to_string(rig.dir.join(RUNS_DIR).join("runs-2026-10.jsonl")).expect("log");
    let lines: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["outcome"], "interrupted");
    assert!(
        lines[0]["why"]
            .as_str()
            .unwrap()
            .contains("is being removed")
    );
    assert!(rig.runner.tick(NOW + 60_000).is_empty());
    // The name is free again, for a function or a service.
    rig.approved("function_create", relay_args())
        .expect("created again");
}

#[test]
fn a_private_destination_comes_from_the_nodes_config_and_is_on_the_card() {
    let rig = Rig::with(json!([{ "service": "relay", "address": "10.4.0.9", "port": 5432 }]));
    rig.nats();
    let proposal = rig
        .propose("function_create", relay_args())
        .expect("proposed");
    assert_eq!(
        proposal["args"]["destinations"],
        json!([{ "address": "10.4.0.9", "port": 5432 }])
    );
    let private = proposal["details"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["label"] == "Private network")
        .expect("the row")["value"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        private.starts_with(
            "Yes: this function can reach 10.4.0.9 on tcp port 5432 through this node"
        ),
        "{private}"
    );
    let created = rig
        .approved("function_create", relay_args())
        .expect("created");
    assert_eq!(
        created["destinations"],
        json!([{ "address": "10.4.0.9", "port": 5432 }])
    );
    // Two things are open: the linked service and the destination, each on its one tcp port. Nothing else.
    let egress = rules(&rig.acl("relay"), "egress");
    let allows: Vec<(String, String)> = egress
        .iter()
        .filter(|rule| rule["action"] == "allow")
        .map(|rule| {
            (
                rule["destination"].as_str().unwrap().to_string(),
                rule["destination_port"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        allows,
        [
            ("10.4.0.9/32".to_string(), "5432".to_string()),
            ("192.0.2.10/32".to_string(), "4222".to_string()),
        ]
    );
}
