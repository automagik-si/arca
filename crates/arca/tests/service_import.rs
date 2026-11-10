//! Importing a service another manager created, in place: the real kernel, the real owner verifier with a software passkey,
//! the real service handlers and the MCP door, over a fake Incus. No live host is touched.
//!
//! The fixture is a Postgres service `db1` in the other manager's project `app1`, at an address in that project's subnet
//! `10.0.5.0/24`, outside this node's own service subnet `192.0.2.0/24`.

mod common;
#[path = "../../arca-approval/tests/common/webauthn.rs"]
mod webauthn;

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arca::adoptions::{ADOPTIONS_FILE, AdoptionRecord, Phase};
use arca::config::NodeConfig;
use arca::handlers::{
    ActionExecutor, ActionSlot, action_statement, approve_action, plan_handlers_served,
    run_approved_action, service_handlers, watcher_step,
};
use arca::kernel::Kernel;
use arca::service::Service;
use arca_api::Scope;
use arca_incus::{
    FakeIncus, FakeState, ImportTarget, ImportWriter, IncusError, IncusRead, UnixIncus,
    import_allowance,
};
use arca_kernel::desired::EntryId;
use arca_kernel::identity::NodeKey;
use arca_kernel::ownership::{IncusKind, KERNEL_ENTRY_KEY, ObjectRef, OwnerClass, classify};
use arca_kernel::plugin::{EffectOp, EffectRefusal, Provider};
use arca_mcp::{CallError, Caller, ToolHandler};
use arca_netproject::PROJECT;
use arca_providers::incus::{Family, IncusProvider, RetainedSet};
use arca_reconcile::PluginSet;
use common::{HOST, NAMESPACE, NOW, Scratch, verifier_with_passkeys};
use serde_json::{Value, json};
use webauthn::{Assert, SoftPasskey, passkey_approval};

const UUID: &str = "11111111-1111-4111-8111-111111111111";
const OTHER_UUID: &str = "22222222-2222-4222-8222-222222222222";
const MARK: &str = "adopt-db1";
const DB1: &str = "/1.0/instances/db1?project=app1";

/// A Postgres service as the other manager left it: its service keys, a secret in its environment, its pinned address.
fn foreign_service(name: &str, project: &str, ipv4: &str, uuid: &str) -> Value {
    json!({
        "name": name,
        "project": project,
        "type": "container",
        "status": "Running",
        "profiles": ["default"],
        "config": {
            "user.arca.template": "postgres-autopg",
            "user.arca.address": ipv4,
            "user.arca.ports": "5432",
            "user.arca.envkeys": "POSTGRES_PASSWORD",
            "environment.POSTGRES_PASSWORD": "redacted-value",
            "volatile.uuid": uuid
        },
        "devices": {
            "eth0": {
                "type": "nic", "name": "eth0", "network": "appbr0", "ipv4.address": ipv4,
                "security.acls": format!("{project}-svc-{name}")
            }
        }
    })
}

fn db1() -> Value {
    foreign_service("db1", "app1", "10.0.5.10", UUID)
}

fn listed(name: &str, project: &str, cidr: &str) -> Value {
    json!({ "name": name, "project": project, "cidr": cidr })
}

/// One opening of the kernel over the rig's state directory: the kernel, every handler, and the executor an approval runs
/// its action through, as `serve` wires them.
struct Node {
    cfg: Arc<NodeConfig>,
    kernel: Arc<Mutex<Kernel>>,
    handlers: HashMap<&'static str, Arc<dyn ToolHandler>>,
    executor: ActionExecutor,
}

struct Rig {
    scratch: Scratch,
    fake: FakeIncus,
    passkey: SoftPasskey,
    counter: AtomicU32,
    nonce: AtomicU64,
    node: Option<Node>,
}

impl Rig {
    /// A node over `instances` whose configuration lists `imports`.
    fn new(tag: &str, instances: Vec<Value>, imports: Value) -> Rig {
        let scratch = Scratch::new(tag);
        let fake = FakeIncus::start(
            &scratch.dir,
            FakeState {
                server: json!({}),
                projects: ["default", "app1", "app2"]
                    .iter()
                    .map(|name| json!({ "name": name, "config": {} }))
                    .collect(),
                instances,
                networks: vec![],
                pools: vec![],
            },
        )
        .expect("the fake starts");
        fake.add_profile(
            "default",
            json!({"name":"default","config":{},"devices":{"root":{"type":"disk","pool":"pool1","path":"/"}}}),
        );
        let mut rig = Rig {
            scratch,
            fake,
            passkey: SoftPasskey::from_seed(7),
            counter: AtomicU32::new(0),
            nonce: AtomicU64::new(0),
            node: None,
        };
        rig.open(imports);
        rig
    }

    fn state_dir(&self) -> PathBuf {
        self.scratch.path("state")
    }

    fn config_value(&self, imports: Value) -> Value {
        json!({
            "stateDir": self.state_dir(),
            "incus": { "socket": self.fake.socket() },
            "listen": { "unix": self.scratch.path("arca.sock") },
            "tokens": { "hashDir": self.scratch.path("tokens") },
            "kernel": {
                "projects": [PROJECT],
                "droiddProjects": ["app1", "app2"],
                "serviceCidr": "192.0.2.0/24",
                "importServices": imports
            }
        })
    }

    /// Opens the kernel over the state directory, as a start or a restart of `serve` does, with `imports` as its allowlist.
    fn open(&mut self, imports: Value) {
        assert!(self.node.is_none(), "the last kernel is closed first");
        let state = self.state_dir();
        if !state.exists() {
            fs::create_dir(&state).expect("the state directory");
            fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).expect("0700");
        }
        let cfg: NodeConfig =
            serde_json::from_value(self.config_value(imports)).expect("the config parses");
        arca::config::check(&cfg).expect("the config is one serve accepts");
        let cfg = Arc::new(cfg);
        let lines = format!("{}\n", self.passkey.line(NAMESPACE));
        let verifier = verifier_with_passkeys(&self.scratch, &[], &lines, "import");
        let mut plugins = PluginSet::new();
        plugins.add::<arca_netproject::NetProject>().expect("net");
        plugins.add::<Service>().expect("service");
        let socket = self.fake.socket().to_path_buf();
        let secrets = state.join("secrets");
        let family = family();
        let kernel = Arc::new(Mutex::new(
            Kernel::open(
                &state,
                NodeKey::load_or_create(&state.join("node.key")).expect("the node key"),
                plugins,
                verifier,
                HOST.into(),
                None,
                move |retained| {
                    Ok(Box::new(
                        IncusProvider::new(socket, family, secrets, retained).expect("provider"),
                    ) as Box<dyn Provider + Send>)
                },
                NOW,
            )
            .expect("the kernel opens"),
        ));
        let slot = ActionSlot::default();
        let handlers: HashMap<&'static str, Arc<dyn ToolHandler>> = plan_handlers_served(
            Arc::clone(&kernel),
            Arc::new(|| NOW),
            None,
            None,
            None,
            None,
            Arc::clone(&slot),
            Some(Arc::clone(&cfg)),
        )
        .into_iter()
        .chain(service_handlers(
            Arc::clone(&kernel),
            Arc::new(|| NOW),
            Arc::clone(&cfg),
        ))
        .collect();
        let executor = ActionExecutor::new(
            handlers
                .iter()
                .map(|(verb, handler)| (*verb, Arc::clone(handler))),
        );
        // The slot stays empty: an approval runs through `run_approved_action` with this executor, as `action_approve` does,
        // and a filled slot would keep the kernel alive past `close`.
        self.node = Some(Node {
            cfg,
            kernel,
            handlers,
            executor,
        });
    }

    /// Stops the kernel: every handle on it is dropped, so the state directory's lock is free again.
    fn close(&mut self) {
        self.node = None;
    }

    fn node(&self) -> &Node {
        self.node.as_ref().expect("the kernel is open")
    }

    fn call_as(&self, scope: Scope, verb: &str, args: Value) -> Result<Value, CallError> {
        self.node().handlers[verb].call(
            args,
            &Caller {
                token_id: "caller".into(),
                scope,
                tools: None,
            },
        )
    }

    fn call(&self, verb: &str, args: Value) -> Result<Value, CallError> {
        self.call_as(Scope::Doctor, verb, args)
    }

    /// An inert proposal of `action` for the service `name`, made by a viewer.
    fn propose(&self, action: &str, name: &str) -> Result<Value, CallError> {
        self.call_as(
            Scope::Viewer,
            "action_propose",
            json!({ "action": action, "args": { "name": name } }),
        )
    }

    /// The owner's passkey approval of `proposal`: the statement the kernel builds for it, signed.
    fn approval(&self, proposal: &Value) -> String {
        let nonce = format!("{:032x}", self.nonce.fetch_add(1, Ordering::SeqCst) + 1);
        let statement = action_statement(
            HOST,
            proposal["id"].as_str().expect("an id"),
            proposal["action"].as_str().expect("an action"),
            proposal["argsDigest"].as_str().expect("a digest"),
            NOW,
            &nonce,
        )
        .expect("the statement");
        let mut assert = Assert::over(NAMESPACE, statement.as_bytes());
        assert.sign_count = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
        passkey_approval(&statement, &self.passkey.assert(&assert))
    }

    /// Approves `proposal` with `approval` and runs its action once, exactly as `action_approve` and the approval page do.
    fn approve_with(&self, proposal: &Value, approval: &str) -> Result<Value, CallError> {
        let node = self.node();
        run_approved_action(
            &node.kernel,
            &node.executor,
            &json!({ "plan": proposal["id"], "approval": approval }),
            NOW,
        )
    }

    /// Proposes `action` for `name`, has the owner approve it, and runs it. The action's own result.
    fn approved(&self, action: &str, name: &str) -> Result<Value, CallError> {
        let proposal = self.propose(action, name)?;
        let approval = self.approval(&proposal);
        self.approve_with(&proposal, &approval)
            .map(|done| done["result"].clone())
    }

    fn get(&self, path: &str) -> Option<Value> {
        UnixIncus::new(self.fake.socket().to_path_buf())
            .get(path)
            .ok()
    }

    /// The instance as Incus holds it, less the fields a read derives.
    fn instance(&self, path: &str) -> Value {
        let mut body = self.get(path).expect("the instance");
        if let Some(object) = body.as_object_mut() {
            object.remove("expanded_devices");
            object.remove("expanded_config");
        }
        body
    }

    /// An edit another manager, or an operator, makes outside Arca.
    fn edit(&self, method: &str, path: &str, body: Value) {
        use std::io::{Read, Write};
        let mut stream =
            std::os::unix::net::UnixStream::connect(self.fake.socket()).expect("connect");
        let body = body.to_string();
        write!(stream, "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).expect("send");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("answer");
        assert!(
            response.starts_with("HTTP/1.1 200") || response.starts_with("HTTP/1.1 202"),
            "the fixture edit failed: {}",
            response.lines().next().unwrap_or("")
        );
    }

    fn entry(&self, id: &str) -> Option<Value> {
        self.node()
            .kernel
            .lock()
            .expect("kernel")
            .entries()
            .iter()
            .find(|entry| entry.id.0 == id)
            .map(|entry| entry.config.clone())
    }

    fn journal(&self) -> Vec<u8> {
        fs::read(self.state_dir().join(ADOPTIONS_FILE)).unwrap_or_default()
    }

    /// The writes the fake took that name `db1`'s instance, as (method, config keys).
    fn db1_writes(&self) -> Vec<(String, Vec<String>)> {
        self.fake
            .writes()
            .into_iter()
            .filter(|write| write.path.starts_with("/1.0/instances/db1"))
            .map(|write| (write.method, write.config_keys))
            .collect()
    }
}

/// A config change: each key with its new value, or `None` to remove it.
type Changes = BTreeMap<String, Option<String>>;

fn family() -> Family {
    Family {
        projects: vec![PROJECT.into()],
        scratch: false,
        droidd_projects: vec!["app1".into(), "app2".into()],
        reserved_cidrs: vec![],
    }
}

fn refusal(result: Result<Value, CallError>) -> (String, String) {
    match result {
        Err(CallError::Refused { code, message }) => (code, message),
        other => panic!("a refusal was expected, got {other:?}"),
    }
}

fn one_patch() -> Vec<(String, Vec<String>)> {
    vec![("PATCH".to_string(), vec![KERNEL_ENTRY_KEY.to_string()])]
}

fn standard() -> Rig {
    Rig::new(
        "import",
        vec![db1()],
        json!([listed("db1", "app1", "10.0.5.0/24")]),
    )
}

#[test]
fn an_approved_import_writes_one_key_and_makes_the_adopted_record() {
    let rig = standard();
    rig.fake
        .set_console_log("app1", "db1", "database system is ready\nlistening\n");
    let before = rig.instance(DB1);
    assert_eq!(
        rig.call("service_list", json!({})).expect("list")["services"],
        json!([]),
        "before the import Arca has no service"
    );

    let proposal = rig.propose("service_import", "db1").expect("a proposal");
    assert_eq!(
        proposal["summary"],
        "Import the service db1, which another manager created, where it stands"
    );
    let details = proposal["details"].to_string();
    assert!(
        details.contains("user.arcakernel.entry = adopt-db1") && details.contains("not stopped"),
        "the owner is shown the one key: {details}"
    );
    assert!(rig.fake.writes().is_empty(), "a proposal writes nothing");
    assert!(rig.entry("service-db1").is_none());
    assert!(rig.journal().is_empty());

    let approval = rig.approval(&proposal);
    let done = rig
        .approve_with(&proposal, &approval)
        .expect("the approved import runs");
    assert_eq!(done["action"], "service_import");
    let result = &done["result"];
    assert_eq!(result["imported"], true);

    // Exactly one write, and it names one key.
    assert_eq!(rig.db1_writes(), one_patch());
    assert_eq!(rig.fake.writes().len(), 1, "nothing else was written");
    let after = rig.instance(DB1);
    let mut expected = before.clone();
    expected["config"][KERNEL_ENTRY_KEY] = json!(MARK);
    assert_eq!(after, expected, "the pre-image plus exactly the mark");
    assert_eq!(after["status"], "Running", "it was never stopped");

    // The record, in the instance's own project.
    let record = rig.entry("service-db1").expect("the adopted record");
    assert_eq!(record["adopted"], true);
    assert_eq!(record["project"], "app1");

    // The adoption record holds the intent and the outcome, with the pre-image digest the call answered.
    let journal = AdoptionRecord::open(&rig.state_dir()).expect("the adoption record");
    let events = journal.import_events();
    assert_eq!(events.len(), 2);
    assert!(events.iter().all(|(release, event)| {
        !release
            && event.name == "db1"
            && event.project == "app1"
            && event.uuid == UUID
            && result["preImage"] == event.pre_image.as_str()
    }));
    assert_eq!(
        [events[0].1.phase, events[1].1.phase],
        [Phase::Intent, Phase::Done]
    );
    assert_eq!(
        journal.imported("db1").map(|event| event.uuid),
        Some(UUID.to_string())
    );
    assert!(
        journal.adopted().is_empty(),
        "an import is not an adoption of the adopt command's"
    );
    let mode = fs::metadata(rig.state_dir().join(ADOPTIONS_FILE))
        .expect("the file")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    let text = String::from_utf8(rig.journal()).expect("text");
    assert!(
        !text.contains("redacted-value"),
        "no config value is recorded"
    );

    // The fields the canvas draws, for an address outside this node's own service subnet.
    let row = json!({
        "name": "db1", "template": "postgres-autopg", "state": "running", "ipv4": "10.0.5.10",
        "ports": [5432], "links": [], "published": null, "lanPort": null
    });
    assert_eq!(result["service"], row);
    assert_eq!(
        rig.call("service_list", json!({})).expect("list")["services"],
        json!([row])
    );
    assert_eq!(
        rig.call("service_status", json!({"name":"db1"}))
            .expect("status"),
        row
    );
    assert_eq!(
        rig.call("service_logs", json!({"name":"db1","lines":2}))
            .expect("logs")["lines"],
        json!(["database system is ready", "listening"])
    );

    // The watch reports no drift: the instance carries both managers' marks, which no entry's mark satisfies and no drift
    // kind names, and the import's record owns no object.
    let node = rig.node();
    let watch = arca::cmd_serve::watch_kernel_handler(&node.cfg, Arc::clone(&node.kernel));
    let mut last = None;
    watcher_step(&watch, NOW, &mut last);
    let report = watch.newest().expect("a sweep").1;
    assert_eq!(report["drift"], json!([]));
    let app1 = report["projects"]
        .as_array()
        .expect("projects")
        .iter()
        .find(|project| project["project"] == "app1")
        .expect("the other manager's project");
    assert_eq!(app1["instances"][0]["owner"]["class"], "contested");
    assert_eq!(
        rig.fake.writes().len(),
        1,
        "reads and a sweep write nothing"
    );
}

#[test]
fn an_approval_for_another_target_and_a_replay_are_refused_and_write_nothing() {
    let rig = Rig::new(
        "import-approval",
        vec![
            db1(),
            foreign_service("db2", "app1", "10.0.5.11", OTHER_UUID),
        ],
        json!([
            listed("db1", "app1", "10.0.5.0/24"),
            listed("db2", "app1", "10.0.5.0/24")
        ]),
    );
    let first = rig
        .propose("service_import", "db1")
        .expect("db1's proposal");
    let second = rig
        .propose("service_import", "db2")
        .expect("db2's proposal");
    assert_ne!(first["id"], second["id"]);

    // The owner approved db2's import. It does not run db1's.
    let for_second = rig.approval(&second);
    let (code, _) = refusal(rig.approve_with(&first, &for_second));
    assert_eq!(code, "WrongTarget");
    assert!(
        rig.fake.writes().is_empty(),
        "a refused approval writes nothing"
    );
    assert!(rig.entry("service-db1").is_none() && rig.entry("service-db2").is_none());

    // The verb itself runs only with the one-shot grant of an approved proposal: not bare, and not with the owner's approval.
    for approval in ["x".to_string(), for_second.clone()] {
        let (code, _) = refusal(rig.call(
            "service_import",
            json!({ "name": "db1", "approval": approval }),
        ));
        assert_eq!(code, "NotApproved");
    }
    assert!(matches!(
        rig.call("service_import", json!({ "name": "db1" })),
        Err(CallError::Refused { code, .. }) if code == "NotApproved"
    ));
    assert!(matches!(
        rig.call_as(
            Scope::Viewer,
            "service_import",
            json!({ "name": "db1", "approval": "x" })
        ),
        Err(CallError::Forbidden { .. })
    ));
    assert!(rig.fake.writes().is_empty());

    // The genuine approval runs db1's import once.
    let for_first = rig.approval(&first);
    rig.approve_with(&first, &for_first)
        .expect("db1's import runs");
    assert_eq!(rig.fake.writes().len(), 1);

    // A replay of that approval: its proposal is retired, and on a new proposal its nonce is spent and its target is another.
    let (code, _) = refusal(rig.approve_with(&first, &for_first));
    assert_eq!(code, "NoSuchPlan");
    let release = rig.propose("service_release", "db1").expect("a release");
    let (code, _) = refusal(rig.approve_with(&release, &for_first));
    assert!(
        [
            "WrongTarget",
            "WrongAction",
            "Replayed",
            "SignCountRegressed"
        ]
        .contains(&code.as_str()),
        "{code}"
    );
    // db2's own approval, signed now, runs db2's import once.
    let for_second = rig.approval(&second);
    rig.approve_with(&second, &for_second)
        .expect("db2's import runs");
    let (code, _) = refusal(rig.approve_with(&second, &for_second));
    assert_eq!(code, "NoSuchPlan");
    assert_eq!(
        rig.fake.writes().len(),
        2,
        "two imports, two writes, no replay"
    );
    assert!(rig.entry("service-db1").is_some() && rig.entry("service-db2").is_some());
}

#[test]
fn the_dry_run_prints_the_one_write_and_every_check_and_writes_nothing() {
    let rig = standard();
    let config = rig.scratch.path("node.json");
    fs::write(
        &config,
        rig.config_value(json!([
            listed("db1", "app1", "10.0.5.0/24"),
            listed("gone", "app1", "10.0.5.0/24")
        ]))
        .to_string(),
    )
    .expect("the config file");
    let before = rig.instance(DB1);
    let requests = rig.fake.requests().len();
    let run = |name: &str| {
        let argv: Vec<String> = [
            "service",
            "import",
            "plan",
            "--name",
            name,
            "--config",
            config.to_str().expect("utf-8"),
        ]
        .iter()
        .map(|part| part.to_string())
        .collect();
        arca::run(&argv)
            .expect("the plan prints")
            .expect("a document")
    };

    let plan = run("db1");
    assert_eq!(plan["ready"], true);
    assert_eq!(plan["wrote"], false);
    assert_eq!(plan["project"], "app1");
    assert_eq!(plan["uuid"], UUID);
    assert_eq!(
        plan["write"],
        json!({
            "method": "PATCH",
            "path": DB1,
            "ifMatch": true,
            "body": { "config": { "user.arcakernel.entry": "adopt-db1" } }
        })
    );
    let digest = plan["preImage"].as_str().expect("the pre-image digest");
    assert!(digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()));
    let checks: Vec<(String, Value)> = plan["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .map(|check| {
            assert!(
                check["detail"]
                    .as_str()
                    .is_some_and(|text| !text.is_empty()),
                "{check}"
            );
            (
                check["check"].as_str().expect("a name").to_string(),
                check["ok"].clone(),
            )
        })
        .collect();
    assert_eq!(
        checks,
        [
            ("listed", json!(true)),
            ("found", json!(true)),
            ("container", json!(true)),
            ("unmarked", json!(true)),
            ("serviceKeys", json!(true)),
            ("address", json!(true)),
            ("record", json!(true)),
            ("nameFree", Value::Null),
            ("identity", json!(true)),
        ]
        .map(|(name, ok)| (name.to_string(), ok))
    );
    assert!(
        !plan.to_string().contains("redacted-value"),
        "the plan prints no config value"
    );

    // A name the allowlist does not hold, and one whose instance is not there: not ready, and no write to print.
    let unlisted = run("other");
    assert_eq!(unlisted["ready"], false);
    assert_eq!(unlisted["write"], Value::Null);
    assert_eq!(unlisted["checks"][0]["check"], "listed");
    assert_eq!(unlisted["checks"][0]["ok"], false);
    let missing = run("gone");
    assert_eq!(missing["ready"], false);
    assert_eq!(missing["checks"][1]["check"], "found");
    assert_eq!(missing["checks"][1]["ok"], false);

    // Nothing was written: every request was a GET, the instance is as it was, and the state directory holds no record.
    assert!(rig.fake.writes().is_empty());
    assert!(
        rig.fake.requests()[requests..]
            .iter()
            .all(|request| request.method == "GET")
    );
    assert_eq!(rig.instance(DB1), before);
    assert!(rig.journal().is_empty());
    assert!(rig.entry("service-db1").is_none());

    // The digest the dry run printed is the one the import records.
    let imported = rig.approved("service_import", "db1").expect("the import");
    assert_eq!(imported["preImage"], digest);
    // And a second dry run says so, and still prints no write.
    let again = run("db1");
    assert_eq!(again["ready"], false);
    assert_eq!(again["write"], Value::Null);
    let unmarked = again["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .find(|check| check["check"] == "unmarked")
        .expect("the check");
    assert_eq!(unmarked["ok"], false);
    assert!(
        unmarked["detail"]
            .as_str()
            .expect("words")
            .contains("it is imported")
    );
    assert_eq!(rig.fake.writes().len(), 1);
}

#[test]
fn a_release_restores_the_exact_pre_image_and_removes_the_record() {
    let rig = standard();
    let before = rig.instance(DB1);
    let imported = rig.approved("service_import", "db1").expect("the import");
    assert_ne!(rig.instance(DB1), before);

    let proposal = rig.propose("service_release", "db1").expect("a proposal");
    assert_eq!(
        proposal["summary"],
        "Release the imported service db1 back to the manager that created it"
    );
    let approval = rig.approval(&proposal);
    let released = rig
        .approve_with(&proposal, &approval)
        .expect("the approved release runs")["result"]
        .clone();
    assert_eq!(
        released,
        json!({ "released": true, "restored": true, "preImage": imported["preImage"] })
    );

    // The instance is exactly as it was before the import, and it kept running.
    assert_eq!(rig.instance(DB1), before);
    let own = serde_json::from_value(before["config"].clone()).expect("config");
    assert_eq!(
        classify(&own, &Default::default()),
        OwnerClass::Droidd,
        "it is the other manager's alone again"
    );
    // Two writes in all, each the one key.
    assert_eq!(rig.db1_writes(), [one_patch(), one_patch()].concat());
    assert_eq!(rig.fake.writes().len(), 2);

    // The record is gone, no plan waits for it, and the next call does not make it again.
    assert!(rig.entry("service-db1").is_none());
    assert!(
        rig.node()
            .kernel
            .lock()
            .expect("kernel")
            .pending_plans()
            .is_empty()
    );
    assert_eq!(
        rig.call("service_list", json!({})).expect("list")["services"],
        json!([])
    );
    assert!(rig.entry("service-db1").is_none());
    let (code, _) = refusal(rig.call("service_status", json!({"name":"db1"})));
    assert_eq!(code, "not_found");

    let journal = AdoptionRecord::open(&rig.state_dir()).expect("the adoption record");
    let events = journal.import_events();
    assert_eq!(
        events
            .iter()
            .map(|(release, event)| (*release, event.phase))
            .collect::<Vec<_>>(),
        [
            (false, Phase::Intent),
            (false, Phase::Done),
            (true, Phase::Intent),
            (true, Phase::Done)
        ]
    );
    assert!(journal.imported("db1").is_none());

    // It can be imported again: the release left nothing behind that stands in the way.
    let again = rig
        .approved("service_import", "db1")
        .expect("a second import");
    assert_eq!(again["preImage"], imported["preImage"]);
    // A release of a service that is not there, and of one Arca created, is refused.
    rig.approved("service_release", "db1").expect("released");
    let (code, _) = refusal(rig.approved("service_release", "db1"));
    assert_eq!(code, "not_found");
}

#[test]
fn a_release_refuses_in_plain_words_when_the_inverse_is_not_safe() {
    // The mark names another entry now: Arca does not clear a mark it did not write.
    let rig = standard();
    rig.approved("service_import", "db1").expect("the import");
    let writes = rig.fake.writes().len();
    rig.edit(
        "PATCH",
        DB1,
        json!({"config":{"user.arcakernel.retiring":"0000"}}),
    );
    let (_, words) = refusal(rig.approved("service_release", "db1"));
    assert!(
        words.contains("another Arca key") && words.contains("Nothing was written"),
        "{words}"
    );
    rig.edit(
        "PATCH",
        DB1,
        json!({"config":{"user.arcakernel.retiring":"", "user.arcakernel.entry":"adopt-other"}}),
    );
    let (_, words) = refusal(rig.approved("service_release", "db1"));
    assert!(
        words.contains("no longer names this import") && words.contains("Nothing was written"),
        "{words}"
    );
    assert_eq!(
        rig.fake
            .writes()
            .iter()
            .filter(|write| write.status < 300)
            .count(),
        writes + 2,
        "only the two edits were written"
    );
    assert_eq!(
        rig.get(DB1).expect("db1")["config"][KERNEL_ENTRY_KEY],
        "adopt-other"
    );
    assert!(
        rig.entry("service-db1").is_some(),
        "the record stays while the release is refused"
    );

    // The instance of that name is another one now.
    let rig = standard();
    rig.approved("service_import", "db1").expect("the import");
    rig.edit(
        "PATCH",
        DB1,
        json!({"config":{"volatile.uuid": OTHER_UUID}}),
    );
    let (_, words) = refusal(rig.approved("service_release", "db1"));
    assert!(
        words.contains("is not the one that was imported") && words.contains("Nothing was written"),
        "{words}"
    );
    assert_eq!(rig.get(DB1).expect("db1")["config"][KERNEL_ENTRY_KEY], MARK);

    // The allowlist no longer lists it: the allowance to write is gone with the entry.
    let mut rig = standard();
    rig.approved("service_import", "db1").expect("the import");
    rig.close();
    rig.open(json!([]));
    let (_, words) = refusal(rig.approved("service_release", "db1"));
    assert!(
        words.contains("kernel.importServices no longer lists it")
            && words.contains("Nothing was written"),
        "{words}"
    );
    assert_eq!(rig.db1_writes(), one_patch());
    assert_eq!(rig.get(DB1).expect("db1")["config"][KERNEL_ENTRY_KEY], MARK);
    // Listed again, it is released.
    rig.close();
    rig.open(json!([listed("db1", "app1", "10.0.5.0/24")]));
    let released = rig.approved("service_release", "db1").expect("released");
    assert_eq!(released["restored"], true);

    // What the other manager changed since the import stays: only the one key is cleared, and the answer says the image is
    // no longer the recorded pre-image.
    let rig = standard();
    rig.approved("service_import", "db1").expect("the import");
    rig.edit(
        "PATCH",
        DB1,
        json!({"config":{"user.arca.links":"", "environment.EXTRA":"1"}}),
    );
    let released = rig.approved("service_release", "db1").expect("released");
    assert_eq!(released["restored"], false);
    let after = rig.get(DB1).expect("db1");
    assert_eq!(after["config"]["environment.EXTRA"], "1");
    assert!(after["config"].get(KERNEL_ENTRY_KEY).is_none());

    // An instance that is gone takes no write: the record goes, and that is all.
    let rig = standard();
    rig.approved("service_import", "db1").expect("the import");
    rig.edit(
        "PUT",
        "/1.0/instances/db1/state?project=app1",
        json!({"action":"stop"}),
    );
    rig.edit("DELETE", DB1, json!({}));
    let released = rig.approved("service_release", "db1").expect("released");
    assert_eq!(
        released,
        json!({ "released": true, "restored": false, "preImage": null })
    );
    assert!(rig.entry("service-db1").is_none());
    assert_eq!(rig.db1_writes().len(), 3, "the import and the two edits");
}

#[test]
fn a_restart_reads_the_imported_record_back_and_writes_nothing() {
    let mut rig = standard();
    rig.approved("service_import", "db1").expect("the import");
    let listed_before = rig.call("service_list", json!({})).expect("list");
    let record = rig.entry("service-db1").expect("the record");
    let journal = rig.journal();
    let instance = rig.instance(DB1);
    let writes = rig.fake.writes().len();
    let requests = rig.fake.requests().len();

    for _ in 0..2 {
        rig.close();
        rig.open(json!([listed("db1", "app1", "10.0.5.0/24")]));
        assert_eq!(
            rig.call("service_list", json!({})).expect("list"),
            listed_before
        );
        assert_eq!(
            rig.call("service_status", json!({"name":"db1"}))
                .expect("status")["state"],
            "running"
        );
        let node = rig.node();
        let watch = arca::cmd_serve::watch_kernel_handler(&node.cfg, Arc::clone(&node.kernel));
        let mut last = None;
        watcher_step(&watch, NOW, &mut last);
        assert_eq!(watch.newest().expect("a sweep").1["drift"], json!([]));
    }

    // Zero requests that are not a GET since the restart: nothing was re-imported, re-created or re-written.
    let since: Vec<String> = rig.fake.requests()[requests..]
        .iter()
        .filter(|request| request.method != "GET")
        .map(|request| format!("{} {}", request.method, request.path))
        .collect();
    assert_eq!(since, Vec::<String>::new());
    assert_eq!(rig.fake.writes().len(), writes);
    assert_eq!(rig.entry("service-db1").expect("the record"), record);
    assert_eq!(rig.journal(), journal, "the adoption record gained no line");
    assert_eq!(rig.instance(DB1), instance);
    assert!(
        rig.node()
            .kernel
            .lock()
            .expect("kernel")
            .pending_plans()
            .is_empty()
    );
}

#[test]
fn every_other_writing_verb_still_refuses_an_imported_service() {
    let rig = standard();
    fs::create_dir_all(rig.state_dir().join("templates")).expect("templates");
    fs::write(
        rig.state_dir().join("templates/nats.json"),
        json!({
            "name": "nats", "kind": "oci", "image": "docker.io/library/nats",
            "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "ports": [4222], "env": {}, "publishable": false
        })
        .to_string(),
    )
    .expect("a template");
    rig.call("service_create", json!({"name":"nats","template":"nats"}))
        .expect("one of Arca's own services");
    rig.approved("service_import", "db1").expect("the import");
    let instance = rig.instance(DB1);

    for (verb, args) in [
        ("service_remove", json!({"name":"db1","approval":"x"})),
        (
            "service_secret_set",
            json!({"name":"db1","key":"POSTGRES_PASSWORD","value":"another-secret-value"}),
        ),
        (
            "service_publish",
            json!({"name":"db1","reach":"tailnet","approval":"x"}),
        ),
        ("service_unpublish", json!({"name":"db1"})),
        ("service_connect", json!({"from":"db1","to":"nats"})),
        ("service_connect", json!({"from":"nats","to":"db1"})),
        ("service_disconnect", json!({"from":"db1","to":"nats"})),
        ("service_disconnect", json!({"from":"nats","to":"db1"})),
    ] {
        let (code, _) = refusal(rig.call(verb, args.clone()));
        assert_eq!(code, "adopted", "{verb} {args}");
    }
    // An approved removal is refused the same way: the owner's approval does not make Arca delete it.
    let (code, _) = refusal(rig.approved("service_remove", "db1"));
    assert_eq!(code, "adopted");
    let (_, words) = refusal(rig.call("service_create", json!({"name":"db1","template":"nats"})));
    assert!(words.contains("db1"), "{words}");
    // A second import is refused when it is proposed: nothing is written again.
    let (_, words) = refusal(rig.propose("service_import", "db1"));
    assert!(words.contains("it is imported"), "{words}");
    // And one of Arca's own services is not released: it was never imported.
    let (_, words) = refusal(rig.approved("service_release", "nats"));
    assert!(words.contains("service_remove removes it"), "{words}");

    assert_eq!(rig.instance(DB1), instance);
    assert_eq!(rig.db1_writes(), one_patch());
    assert!(rig.entry("service-db1").is_some());
}

#[test]
fn an_import_that_fails_a_check_is_refused_before_any_write() {
    // Each of these is refused when it is proposed, so the owner is never asked.
    let vm = {
        let mut row = db1();
        row["type"] = json!("virtual-machine");
        row
    };
    let no_keys = {
        let mut row = db1();
        row["config"]
            .as_object_mut()
            .expect("config")
            .retain(|key, _| !key.starts_with("user.arca."));
        row
    };
    let marked = {
        let mut row = db1();
        row["config"]["user.arcakernel.entry"] = json!("service-db1");
        row
    };
    let unpinned = {
        let mut row = db1();
        row["devices"]["eth0"]["ipv4.address"] = json!("10.0.5.99");
        row
    };
    let bad_ports = {
        let mut row = db1();
        row["config"]["user.arca.ports"] = json!("5432,zero");
        row
    };
    for (instances, imports, name, words) in [
        (vec![db1()], json!([]), "db1", "does not list"),
        (
            vec![db1()],
            json!([listed("db1", "app1", "10.0.5.0/24")]),
            "db2",
            "does not list",
        ),
        // The same name in a project the allowlist does not name.
        (
            vec![foreign_service("db1", "app2", "10.0.5.10", UUID)],
            json!([listed("db1", "app1", "10.0.5.0/24")]),
            "db1",
            "no instance of this name is in the listed project",
        ),
        (
            vec![vm],
            json!([listed("db1", "app1", "10.0.5.0/24")]),
            "db1",
            "not a container",
        ),
        (
            vec![no_keys],
            json!([listed("db1", "app1", "10.0.5.0/24")]),
            "db1",
            "does not carry the other manager's service keys",
        ),
        (
            vec![marked],
            json!([listed("db1", "app1", "10.0.5.0/24")]),
            "db1",
            "already carries an Arca key",
        ),
        (
            vec![db1()],
            json!([listed("db1", "app1", "10.0.6.0/24")]),
            "db1",
            "not inside the listed cidr",
        ),
        (
            vec![unpinned],
            json!([listed("db1", "app1", "10.0.5.0/24")]),
            "db1",
            "not the one its eth0 device pins",
        ),
        (
            vec![bad_ports],
            json!([listed("db1", "app1", "10.0.5.0/24")]),
            "db1",
            "do not read as a service record",
        ),
    ] {
        let rig = Rig::new("import-check", instances, imports);
        let (code, message) = refusal(rig.propose("service_import", name));
        assert_eq!(code, "refused");
        assert!(
            message.contains(words) && message.contains("nothing was proposed"),
            "{message}"
        );
        assert!(rig.fake.writes().is_empty());
        assert!(rig.journal().is_empty());
        assert_eq!(
            rig.call("plan_list", json!({})).expect("plans")["plans"],
            json!([])
        );
    }

    // A check that fails after the owner approved: the import runs its plan again and refuses before it writes.
    let rig = standard();
    let proposal = rig.propose("service_import", "db1").expect("a proposal");
    let approval = rig.approval(&proposal);
    rig.edit(
        "PATCH",
        DB1,
        json!({"config":{"user.arca.address":"10.0.9.10"}}),
    );
    let (code, message) = refusal(rig.approve_with(&proposal, &approval));
    assert_eq!(code, "refused");
    assert!(
        message.contains("not inside the listed cidr") && message.contains("nothing was written"),
        "{message}"
    );
    assert_eq!(rig.db1_writes().len(), 1, "the edit alone");
    assert!(rig.journal().is_empty());
    assert!(rig.entry("service-db1").is_none());
    assert!(
        rig.get(DB1).expect("db1")["config"]
            .get(KERNEL_ENTRY_KEY)
            .is_none()
    );
}

#[test]
fn a_marked_instance_is_checked_against_its_allowlist_cidr_and_without_an_entry_as_before() {
    let marked = || {
        let mut row = db1();
        row["config"]["user.arcakernel.entry"] = json!(MARK);
        row
    };
    // Without an allowlist entry nothing changes: outside the node's service subnet it has no record and lists as failed.
    let rig = Rig::new("import-cidr", vec![marked()], json!([]));
    let rows = rig.call("service_list", json!({})).expect("list")["services"].clone();
    assert_eq!(rows[0]["state"], "failed");
    assert_eq!(rows.as_array().map(Vec::len), Some(1));
    assert!(rig.entry("service-db1").is_none());
    let (code, _) = refusal(rig.call("service_logs", json!({"name":"db1"})));
    assert_eq!(code, "not_found");

    // An entry for another project, or another name, is not this instance's.
    let rig = Rig::new(
        "import-cidr",
        vec![marked()],
        json!([
            listed("db1", "app2", "10.0.5.0/24"),
            listed("db2", "app1", "10.0.5.0/24")
        ]),
    );
    let rows = rig.call("service_list", json!({})).expect("list")["services"].clone();
    assert_eq!(rows[0]["state"], "failed");
    assert!(rig.entry("service-db1").is_none());

    // With its entry, its address is checked against the entry's cidr, and it has its record.
    let rig = Rig::new(
        "import-cidr",
        vec![marked()],
        json!([listed("db1", "app1", "10.0.5.0/24")]),
    );
    let rows = rig.call("service_list", json!({})).expect("list")["services"].clone();
    assert_eq!(rows[0]["state"], "running");
    assert_eq!(rows[0]["ipv4"], "10.0.5.10");
    assert_eq!(rows[0]["ports"], json!([5432]));
    assert_eq!(
        rig.entry("service-db1").expect("the record")["project"],
        "app1"
    );
    // An entry whose cidr does not hold the address gives no record either.
    let rig = Rig::new(
        "import-cidr",
        vec![marked()],
        json!([listed("db1", "app1", "10.0.6.0/24")]),
    );
    assert_eq!(
        rig.call("service_list", json!({})).expect("list")["services"][0]["state"],
        "failed"
    );
    assert!(rig.fake.writes().is_empty());
}

/// The provider and ownership layer: the import's allowance, and every other write to another manager's project.
#[test]
fn the_import_writer_sends_one_key_under_if_match_and_refuses_every_other_write() {
    let rig = Rig::new(
        "import-writer",
        vec![
            db1(),
            foreign_service("db2", "app1", "10.0.5.11", OTHER_UUID),
            foreign_service("db1", "app2", "10.0.7.10", OTHER_UUID),
        ],
        json!([listed("db1", "app1", "10.0.5.0/24")]),
    );
    let allow = vec![ImportTarget {
        project: "app1".into(),
        instance: "db1".into(),
    }];
    let writer = ImportWriter::new(rig.fake.socket().to_path_buf(), allow.clone());
    let entry = EntryId(MARK.into());
    let object = |project: &str, name: &str| ObjectRef {
        kind: IncusKind::Instance,
        project: project.into(),
        parent: None,
        name: name.into(),
        uuid: None,
    };
    let set = |pairs: &[(&str, Option<&str>)]| -> BTreeMap<String, Option<String>> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.map(str::to_string)))
            .collect()
    };
    let the_mark = set(&[(KERNEL_ENTRY_KEY, Some(MARK))]);
    let read = writer
        .read(&allow[0])
        .expect("the read")
        .expect("the instance");
    let requests = rig.fake.requests().len();

    let other_entry = EntryId("adopt-db2".into());
    let service_entry = EntryId("service-db1".into());
    let unlisted_entry = EntryId("adopt-db9".into());
    let profile = ObjectRef {
        kind: IncusKind::Profile,
        ..object("app1", "db1")
    };
    let parented = ObjectRef {
        parent: Some("pool1".into()),
        ..object("app1", "db1")
    };
    let refused: Vec<(&str, &EntryId, ObjectRef, Changes)> = vec![
        (
            "another key in the same patch",
            &entry,
            object("app1", "db1"),
            set(&[
                (KERNEL_ENTRY_KEY, Some(MARK)),
                ("user.arca.template", Some("x")),
            ]),
        ),
        (
            "another key alone",
            &entry,
            object("app1", "db1"),
            set(&[("user.arca.template", Some("x"))]),
        ),
        (
            "the retiring tombstone",
            &entry,
            object("app1", "db1"),
            set(&[("user.arcakernel.retiring", Some("00"))]),
        ),
        (
            "no key at all",
            &entry,
            object("app1", "db1"),
            BTreeMap::new(),
        ),
        (
            "another value for the mark",
            &entry,
            object("app1", "db1"),
            set(&[(KERNEL_ENTRY_KEY, Some("service-db1"))]),
        ),
        (
            "another instance",
            &other_entry,
            object("app1", "db2"),
            set(&[(KERNEL_ENTRY_KEY, Some("adopt-db2"))]),
        ),
        (
            "a project that is not allowlisted",
            &entry,
            object("app2", "db1"),
            the_mark.clone(),
        ),
        (
            "a name that is not allowlisted",
            &unlisted_entry,
            object("app1", "db9"),
            set(&[(KERNEL_ENTRY_KEY, Some("adopt-db9"))]),
        ),
        (
            "another instance's entry",
            &other_entry,
            object("app1", "db1"),
            set(&[(KERNEL_ENTRY_KEY, Some("adopt-db2"))]),
        ),
        (
            "an entry that is not an import's",
            &service_entry,
            object("app1", "db1"),
            set(&[(KERNEL_ENTRY_KEY, Some("service-db1"))]),
        ),
        (
            "a kind that is not an instance",
            &entry,
            profile,
            the_mark.clone(),
        ),
        (
            "an object under a parent",
            &entry,
            parented,
            the_mark.clone(),
        ),
    ];
    for (what, entry, object, changes) in &refused {
        assert!(!import_allowance(&allow, entry, object, changes), "{what}");
        assert!(
            matches!(
                writer.patch(entry, object, changes, &read.etag),
                Err(IncusError::Refused(_))
            ),
            "{what}"
        );
    }
    assert!(matches!(
        writer.read(&ImportTarget {
            project: "app1".into(),
            instance: "db2".into()
        }),
        Err(IncusError::Refused(_))
    ));
    assert!(matches!(
        writer.patch(&entry, &object("app1", "db1"), &the_mark, ""),
        Err(IncusError::Refused(_))
    ));
    assert_eq!(
        rig.fake.requests().len(),
        requests,
        "a refused write sends no request at all, not even a read"
    );

    // A changed ETag: the other manager wrote the instance after the read. Incus refuses the PATCH and nothing changes.
    rig.edit("PATCH", DB1, json!({"config":{"environment.EXTRA":"1"}}));
    let stale = writer.patch(&entry, &object("app1", "db1"), &the_mark, &read.etag);
    assert!(
        matches!(stale, Err(IncusError::Status { code: 412, .. })),
        "{stale:?}"
    );
    assert!(
        rig.get(DB1).expect("db1")["config"]
            .get(KERNEL_ENTRY_KEY)
            .is_none()
    );

    // The one allowed write, under the fresh ETag, and its inverse.
    assert!(import_allowance(
        &allow,
        &entry,
        &object("app1", "db1"),
        &the_mark
    ));
    let fresh = writer.read(&allow[0]).expect("read").expect("there");
    writer
        .patch(&entry, &object("app1", "db1"), &the_mark, &fresh.etag)
        .expect("the import's one write");
    assert_eq!(rig.get(DB1).expect("db1")["config"][KERNEL_ENTRY_KEY], MARK);
    let marked = writer.read(&allow[0]).expect("read").expect("there");
    let clear = set(&[(KERNEL_ENTRY_KEY, None)]);
    assert!(import_allowance(
        &allow,
        &entry,
        &object("app1", "db1"),
        &clear
    ));
    assert!(matches!(
        writer.patch(&entry, &object("app1", "db1"), &clear, &fresh.etag),
        Err(IncusError::Status { code: 412, .. })
    ));
    writer
        .patch(&entry, &object("app1", "db1"), &clear, &marked.etag)
        .expect("the release's one write");
    assert!(
        rig.get(DB1).expect("db1")["config"]
            .get(KERNEL_ENTRY_KEY)
            .is_none()
    );
    let sent: Vec<(String, Vec<String>, u16)> = rig
        .fake
        .writes()
        .into_iter()
        .skip(1)
        .map(|write| (write.method, write.config_keys, write.status))
        .collect();
    assert!(
        sent.iter()
            .all(|(method, keys, _)| method == "PATCH" && keys == &[KERNEL_ENTRY_KEY.to_string()]),
        "{sent:?}"
    );
    // The stale import, the import, the stale release, the release: a refused one is 412, a taken one is not an error.
    assert_eq!(
        sent.iter()
            .map(|(_, _, status)| if *status < 300 { 200 } else { *status })
            .collect::<Vec<_>>(),
        [412, 200, 412, 200]
    );

    // The kernel's own provider is as it was: another manager's project is outside its family, for the import's entry too,
    // before the mark and with it.
    let provider = IncusProvider::new(
        rig.fake.socket().to_path_buf(),
        family(),
        rig.state_dir().join("secrets"),
        RetainedSet::empty(),
    )
    .expect("the provider");
    for marked in [false, true] {
        if marked {
            let fresh = writer.read(&allow[0]).expect("read").expect("there");
            writer
                .patch(&entry, &object("app1", "db1"), &the_mark, &fresh.etag)
                .expect("marked");
        }
        for changes in [
            the_mark.clone(),
            set(&[("user.arca.template", Some("x"))]),
            set(&[(KERNEL_ENTRY_KEY, None)]),
        ] {
            let op = EffectOp::IncusPatchConfig {
                object: object("app1", "db1"),
                config: changes,
            };
            assert!(matches!(
                provider.plan(&entry, &op),
                Err(EffectRefusal::ProjectNotAllowed)
            ));
        }
        assert!(matches!(
            provider.plan(
                &entry,
                &EffectOp::IncusDelete {
                    object: object("app1", "db1"),
                    plan: arca_kernel::canon::Digest::of(b"no plan")
                }
            ),
            Err(EffectRefusal::ProjectNotAllowed)
        ));
    }
    // And a family that named the other manager's project is refused outright.
    assert!(
        IncusProvider::new(
            rig.fake.socket().to_path_buf(),
            Family {
                projects: vec![PROJECT.into(), "app1".into()],
                ..family()
            },
            rig.state_dir().join("secrets"),
            RetainedSet::empty(),
        )
        .is_err()
    );
}

/// The MCP door over the real verb table: every result is checked against its verb's result schema, as in production.
struct Door {
    socket: PathBuf,
    doctor: String,
    viewer: String,
    _runtime: tokio::runtime::Runtime,
}

impl Door {
    fn over(rig: &Rig) -> Door {
        use arca_mcp::{Server, TokenStore, mint, serve_unix};
        let tokens = rig.scratch.path("door-tokens");
        fs::create_dir(&tokens).expect("the token directory");
        fs::set_permissions(&tokens, fs::Permissions::from_mode(0o700)).expect("0700");
        let mut secrets = Vec::new();
        for (id, scope) in [("doctor", Scope::Doctor), ("phone", Scope::Viewer)] {
            let (secret, hash) = mint(scope);
            let path = tokens.join(format!("{id}.hash"));
            fs::write(&path, hash).expect("the hash file");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).expect("0400");
            secrets.push(secret);
        }
        let mut server = Server::new(
            arca_api::verbs(),
            TokenStore::load(&tokens).expect("the tokens load"),
        );
        for (verb, handler) in &rig.node().handlers {
            server = server.handle(verb, Arc::clone(handler));
        }
        let socket = rig.scratch.path("door.sock");
        let runtime = tokio::runtime::Runtime::new().expect("a runtime");
        let served = socket.clone();
        let router = server.router();
        runtime.spawn(async move {
            let _ = serve_unix(router, &served).await;
        });
        let until = Instant::now() + Duration::from_secs(5);
        while !socket.exists() {
            assert!(Instant::now() < until, "the door started");
            std::thread::sleep(Duration::from_millis(10));
        }
        Door {
            socket,
            viewer: secrets.pop().expect("the viewer's"),
            doctor: secrets.pop().expect("the doctor's"),
            _runtime: runtime,
        }
    }

    /// A `tools/call` through the door; the JSON-RPC reply.
    fn call(&self, secret: &str, name: &str, arguments: Value) -> Value {
        use std::io::{Read as _, Write as _};
        let body = serde_json::to_vec(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        }))
        .expect("the call");
        let head = format!(
            "POST /mcp HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {secret}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let mut stream =
            std::os::unix::net::UnixStream::connect(&self.socket).expect("connect to the door");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("a read timeout");
        stream.write_all(head.as_bytes()).expect("the head");
        stream.write_all(&body).expect("the body");
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).expect("the answer");
        let split = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("a response head");
        // A call the door forbids outright answers with a status and no body.
        serde_json::from_slice(&raw[split + 4..]).unwrap_or_else(|_| {
            json!({ "error": { "http": String::from_utf8_lossy(&raw[..split]).lines().next() } })
        })
    }

    fn ok(&self, secret: &str, name: &str, arguments: Value) -> Value {
        let reply = self.call(secret, name, arguments);
        assert_eq!(reply["result"]["isError"], json!(false), "{name}: {reply}");
        reply["result"]["structuredContent"].clone()
    }

    /// The reply of a call the door did not run to a result, as text to search.
    fn refused(&self, secret: &str, name: &str, arguments: Value) -> String {
        let reply = self.call(secret, name, arguments);
        assert!(
            reply.get("error").is_some() || reply["result"]["isError"] == json!(true),
            "{name} was not refused: {reply}"
        );
        reply.to_string()
    }
}

#[test]
fn through_the_door_the_import_and_the_release_run_once_with_their_grant_and_meet_their_schemas() {
    let rig = standard();
    let door = Door::over(&rig);
    let before = rig.instance(DB1);

    // The verbs are listed as destructive writes of the doctor scope.
    for name in ["service_import", "service_release"] {
        let verb = arca_api::verb(name).expect("in the table");
        assert!(verb.writes && verb.destructive && verb.min_scope == Scope::Doctor);
        assert!(!verb.read_only_hint());
    }

    // A viewer may propose; it may not call the verb. A doctor's bare call, and one with `confirm: false`, write nothing.
    let proposal = door.ok(
        &door.viewer,
        "action_propose",
        json!({ "action": "service_import", "args": { "name": "db1", "confirm": true } }),
    );
    assert_eq!(proposal["action"], "service_import");
    assert_eq!(proposal["args"], json!({ "name": "db1" }));
    let viewer = door.refused(
        &door.viewer,
        "service_import",
        json!({ "name": "db1", "approval": "x" }),
    );
    assert!(viewer.contains("403 Forbidden"), "{viewer}");
    let unconfirmed = door.refused(
        &door.doctor,
        "service_import",
        json!({ "name": "db1", "approval": "x", "confirm": false }),
    );
    assert!(
        unconfirmed.contains("nothing was imported"),
        "{unconfirmed}"
    );
    let bare = door.refused(
        &door.doctor,
        "service_import",
        json!({ "name": "db1", "approval": "x" }),
    );
    assert!(bare.contains("NotApproved"), "{bare}");
    let extra = door.refused(
        &door.doctor,
        "service_import",
        json!({ "name": "db1", "approval": "x", "project": "app2" }),
    );
    assert!(!extra.contains("\"imported\""), "{extra}");
    assert!(rig.fake.writes().is_empty());

    // The owner approves; the grant runs the verb once through the door, and the door checks its result's schema.
    let grant = |proposal: &Value| {
        let approval = rig.approval(proposal);
        let mut kernel = rig.node().kernel.lock().expect("kernel");
        approve_action(
            &mut kernel,
            &json!({ "plan": proposal["id"], "approval": approval }),
            NOW,
        )
        .expect("the owner's approval verifies")
        .grant
    };
    let token = grant(&proposal);
    let imported = door.ok(
        &door.doctor,
        "service_import",
        json!({ "name": "db1", "approval": token, "confirm": true }),
    );
    assert_eq!(imported["imported"], true);
    assert_eq!(imported["service"]["ipv4"], "10.0.5.10");
    assert_eq!(imported["service"]["state"], "running");
    assert_eq!(rig.db1_writes(), one_patch());
    let again = door.refused(
        &door.doctor,
        "service_import",
        json!({ "name": "db1", "approval": token }),
    );
    assert!(again.contains("NotApproved"), "a grant runs once: {again}");

    let listed = door.ok(&door.viewer, "service_list", json!({}));
    assert_eq!(listed["services"][0]["name"], "db1");
    assert_eq!(listed["services"][0]["ports"], json!([5432]));
    let status = door.ok(&door.doctor, "service_status", json!({ "name": "db1" }));
    assert_eq!(status["state"], "running");

    let proposal = door.ok(
        &door.viewer,
        "action_propose",
        json!({ "action": "service_release", "args": { "name": "db1" } }),
    );
    let unconfirmed = door.refused(
        &door.doctor,
        "service_release",
        json!({ "name": "db1", "approval": "x", "confirm": false }),
    );
    assert!(
        unconfirmed.contains("nothing was released"),
        "{unconfirmed}"
    );
    let token = grant(&proposal);
    let released = door.ok(
        &door.doctor,
        "service_release",
        json!({ "name": "db1", "approval": token }),
    );
    assert_eq!(
        released,
        json!({ "released": true, "restored": true, "preImage": imported["preImage"] })
    );
    assert_eq!(rig.instance(DB1), before);
    assert_eq!(
        door.ok(&door.viewer, "service_list", json!({}))["services"],
        json!([])
    );
}
