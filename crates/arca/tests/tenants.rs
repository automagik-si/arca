//! Tenants against a fake Incus: two tenants beside the node's own project. Nothing here touches a live host.
mod common;

use arca::{
    config::NodeConfig,
    handlers::{
        ActionExecutor, ActionSlot, plan_handlers_served, postgres_handlers, service_handlers,
        tenant_handlers,
    },
    kernel::Kernel,
    service::Service,
};
use arca_api::Scope;
use arca_incus::{FakeIncus, FakeState, IncusRead, ProjectFamily, UnixIncus};
use arca_kernel::identity::NodeKey;
use arca_mcp::{CallError, Caller, ToolHandler};
use arca_netproject::PROJECT;
use arca_providers::incus::{Family, IncusProvider, TenantProjects};
use arca_reconcile::PluginSet;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

const NOW: u64 = 1_793_361_600_000;
const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const PG_IMAGE: &str = "docker.io/library/postgres@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const CIDR: &str = "192.0.2.0/24";
/// Two tenant ids, fixed so the tests can name them.
const A: &str = "aaaaaaaa";
const B: &str = "bbbbbbbb";
static SEQ: AtomicU64 = AtomicU64::new(0);

/// The `kernel.tenants` key the rigs use unless a test changes it.
fn policy() -> Value {
    json!({
        "capacity": { "instances": 40, "cpu": 64, "memory": "128GiB", "disk": "2TiB" },
        "maxTenants": 6,
        "defaultCaps": { "instances": 4, "containers": 4, "virtualMachines": 1, "cpu": 8,
            "memory": "16GiB", "disk": "100GiB" },
        "createRate": { "count": 20, "perSeconds": 3600 }
    })
}

struct Rig {
    dir: PathBuf,
    socket: PathBuf,
    fake: Option<FakeIncus>,
    kernel: Arc<Mutex<Kernel>>,
    handlers: HashMap<&'static str, Arc<dyn ToolHandler>>,
    key: common::SkKey,
    clock: Arc<AtomicU64>,
    kernel_config: Value,
    /// What the node's config holds beside the kernel key: `proxy` and `services` are its `listen.proxy` and
    /// `listen.services`, and `approvalOrigin` its own.
    node: Value,
    nonce: AtomicU64,
    cfg: Arc<NodeConfig>,
}

/// The config of a rig's node: the kernel key `kernel`, and what `node` adds ([`Rig::node`]).
fn node_config(
    dir: &std::path::Path,
    incus: &std::path::Path,
    kernel: &Value,
    node: &Value,
) -> NodeConfig {
    let mut config = json!({
        "stateDir": dir,
        "incus": {"socket": incus},
        "listen": {"unix": dir.join("arca.sock")},
        "tokens": {"hashDir": dir.join("tokens")},
        "kernel": kernel
    });
    for key in ["proxy", "services"] {
        if let Some(value) = node.get(key) {
            config["listen"][key] = value.clone();
        }
    }
    if let Some(origin) = node.get("approvalOrigin") {
        config["approvalOrigin"] = origin.clone();
    }
    serde_json::from_value(config).expect("config")
}

impl Rig {
    /// A node with the managed network, virtual-machine caps and the tenants policy `tenants` (`None`: no tenants).
    fn new(tenants: Option<Value>) -> Self {
        let mut kernel = json!({
            "projects": [PROJECT],
            "droiddProjects": ["other"],
            "serviceCidr": CIDR,
            "postgresImage": PG_IMAGE,
            "vmLimits": { "virtualMachines": 4, "cpu": 16, "memory": "64GiB", "disk": "1TiB" }
        });
        if let Some(tenants) = tenants {
            kernel["tenants"] = tenants;
        }
        Self::with_kernel(kernel, empty_state())
    }

    fn with_kernel(kernel_config: Value, state: FakeState) -> Self {
        Self::with_node(kernel_config, json!({}), state)
    }

    /// A node whose config also holds what `node` names: its `listen.proxy` and `listen.services`, and its
    /// `approvalOrigin`.
    fn with_node(kernel_config: Value, node: Value, state: FakeState) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "arca-tenants-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir(&dir).expect("scratch");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("0700");
        let fake = FakeIncus::start(&dir, state).expect("fake");
        fake.add_profile(
            "default",
            json!({"name":"default","config":{},"devices":{"root":{"type":"disk","pool":"pool1","path":"/"}}}),
        );
        Self::open(
            dir,
            fake.socket().to_path_buf(),
            Some(fake),
            kernel_config,
            node,
            common::SkKey::from_seed(9),
            Arc::new(AtomicU64::new(NOW)),
        )
    }

    /// A kernel over a real Incus at `socket`, with its state in a fresh directory. Only the scratch probe uses it.
    fn real(socket: PathBuf, kernel_config: Value, now: u64) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "arca-tenant-probe-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir(&dir).expect("scratch");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("0700");
        Self::open(
            dir,
            socket,
            None,
            kernel_config,
            json!({}),
            common::SkKey::from_seed(9),
            Arc::new(AtomicU64::new(now)),
        )
    }

    /// Opens the kernel over `dir` and `fake`: the first start of a rig, or a restart of one.
    fn open(
        dir: PathBuf,
        incus: PathBuf,
        fake: Option<FakeIncus>,
        kernel_config: Value,
        node: Value,
        key: common::SkKey,
        clock: Arc<AtomicU64>,
    ) -> Self {
        let cfg = node_config(&dir, &incus, &kernel_config, &node);
        arca::config::check(&cfg).expect("the config is one serve accepts");
        let scratch = common::Scratch::new("tenant-owner");
        let verifier = common::verifier_for(&scratch, &[&key], "tenants");
        let mut plugins = PluginSet::new();
        plugins.add::<arca_netproject::NetProject>().expect("net");
        plugins.add::<Service>().expect("service");
        plugins.add::<arca_postgres::Postgres>().expect("postgres");
        plugins
            .add::<arca_postgres::consumer::Consumer>()
            .expect("consumer");
        plugins
            .add::<arca_netproject::tenant::Tenant>()
            .expect("tenant");
        let scratch_run = cfg.kernel.scratch;
        let family = Family {
            projects: cfg.kernel.projects.clone(),
            scratch: scratch_run,
            droidd_projects: cfg.kernel.droidd_projects.clone(),
            reserved_cidrs: vec![],
        };
        let tenants = TenantProjects::new(scratch_run);
        let socket = incus.clone();
        let secrets = dir.join("secrets");
        let provider_tenants = tenants.clone();
        let kernel = Arc::new(Mutex::new(
            Kernel::open_with_tenants(
                &dir,
                NodeKey::load_or_create(&dir.join("node.key")).expect("key"),
                plugins,
                verifier,
                "node1".into(),
                Some(PG_IMAGE),
                move |retained| {
                    Ok(Box::new(
                        IncusProvider::new(socket, family, secrets, retained)
                            .expect("provider")
                            .with_tenants(provider_tenants),
                    ))
                },
                tenants.clone(),
                clock.load(Ordering::SeqCst),
            )
            .expect("kernel"),
        ));
        let cfg = Arc::new(cfg);
        let now = Arc::clone(&clock);
        let time: arca::handlers::Clock = Arc::new(move || now.load(Ordering::SeqCst));
        let lifecycle_kernel = Arc::clone(&kernel);
        let lifecycle = Arc::new(
            arca::lifecycle::InstanceLifecycle::new(
                incus.clone(),
                ProjectFamily::new(
                    cfg.kernel.projects.iter().map(String::as_str),
                    cfg.kernel.droidd_projects.iter().map(String::as_str),
                )
                .expect("family"),
                move || {
                    lifecycle_kernel
                        .lock()
                        .map_err(|_| "poisoned".to_string())?
                        .retained_entries()
                        .map_err(|error| error.to_string())
                },
                &dir,
            )
            .with_tenants(tenants)
            .with_after_start({
                // As `arca serve` wires it: a start writes a virtual machine's stored secrets into its guest.
                let secrets =
                    arca::guest_secrets::GuestSecrets::new(Arc::clone(&cfg), Arc::clone(&kernel))
                        .with_wait(
                            std::time::Duration::from_millis(200),
                            std::time::Duration::from_millis(10),
                        );
                Arc::new(move |project, instance| secrets.deliver(project, instance))
            }),
        );
        let handlers = plan_handlers_served(
            kernel.clone(),
            time.clone(),
            Some(PG_IMAGE.into()),
            None,
            None,
            Some(lifecycle),
            ActionSlot::default(),
            Some(cfg.clone()),
        )
        .into_iter()
        .chain(service_handlers(kernel.clone(), time.clone(), cfg.clone()))
        .chain(postgres_handlers(kernel.clone(), time.clone(), cfg.clone()))
        .chain(tenant_handlers(kernel.clone(), time, cfg.clone()))
        .collect();
        Self {
            dir,
            socket: incus,
            fake,
            kernel,
            handlers,
            key,
            clock,
            kernel_config,
            node,
            nonce: AtomicU64::new(0),
            cfg,
        }
    }

    /// Stops the kernel and starts it again over the same state directory and the same Incus.
    fn restart(mut self) -> Self {
        let dir = self.dir.clone();
        let fake = self.fake.take().expect("the fake");
        let key = common::SkKey::from_seed(9);
        let clock = Arc::clone(&self.clock);
        let kernel_config = self.kernel_config.clone();
        let node = self.node.clone();
        self.handlers.clear();
        // The state directory's lock is the old kernel's until it is dropped.
        let old = std::mem::replace(&mut self.kernel, Arc::new(Mutex::new(placeholder_kernel())));
        drop(old);
        drop(self);
        let socket = fake.socket().to_path_buf();
        Self::open(dir, socket, Some(fake), kernel_config, node, key, clock)
    }

    fn fake(&self) -> &FakeIncus {
        self.fake.as_ref().expect("the fake")
    }

    fn now(&self) -> u64 {
        self.clock.load(Ordering::SeqCst)
    }

    /// A call from a doctor token with no tool allowlist.
    fn call(&self, verb: &str, args: Value) -> Result<Value, CallError> {
        self.call_as(verb, args, None)
    }

    /// A call from a doctor token whose tool allowlist is `tools`.
    fn call_as(&self, verb: &str, args: Value, tools: Option<&[&str]>) -> Result<Value, CallError> {
        self.handlers[verb].call(
            args,
            &Caller {
                token_id: "doctor".into(),
                scope: Scope::Doctor,
                tools: tools.map(|tools| tools.iter().map(|tool| tool.to_string()).collect()),
            },
        )
    }

    /// A call from the doctor token `token`, whose tool allowlist is `tools`.
    fn call_token(
        &self,
        token: &str,
        verb: &str,
        args: Value,
        tools: &[&str],
    ) -> Result<Value, CallError> {
        self.handlers[verb].call(
            args,
            &Caller {
                token_id: token.into(),
                scope: Scope::Doctor,
                tools: Some(tools.iter().map(|tool| tool.to_string()).collect()),
            },
        )
    }

    /// `tenant_create` from the token the node allows it for.
    fn create_tenant(&self, args: Value) -> Result<Value, CallError> {
        self.call_as("tenant_create", args, Some(&["tenant_create"]))
    }

    /// Proposes `action` through `action_propose`, approves it with the owner's passkey and runs it, as the approval
    /// page does.
    fn approved(&self, action: &str, args: Value) -> Result<Value, CallError> {
        let proposed = self.call("action_propose", json!({ "action": action, "args": args }))?;
        let id = proposed["id"].as_str().expect("the proposal's id");
        let digest = proposed["argsDigest"].as_str().expect("its digest");
        let nonce = format!("{:064x}", self.nonce.fetch_add(1, Ordering::SeqCst) + 1);
        let statement =
            arca::handlers::action_statement("node1", id, action, digest, self.now(), &nonce)
                .expect("the statement");
        let approval = common::approve_statement(&self.key, &statement);
        let approved = arca::handlers::approve_action(
            &mut self.kernel.lock().expect("kernel"),
            &json!({ "plan": id, "approval": approval }),
            self.now(),
        )?;
        ActionExecutor::new(
            self.handlers
                .iter()
                .map(|(verb, handler)| (*verb, Arc::clone(handler))),
        )
        .run(&approved.action, &approved.args, &approved.grant)
    }

    /// Proposes `action`, approves it with the owner's passkey and runs it through `run_approved_action`, the path
    /// `action_approve` and the approval page share: what comes back is what the caller of either is told.
    fn approved_as_told(&self, action: &str, args: Value) -> Result<Value, CallError> {
        let proposed = self.call("action_propose", json!({ "action": action, "args": args }))?;
        let id = proposed["id"].as_str().expect("the proposal's id");
        let digest = proposed["argsDigest"].as_str().expect("its digest");
        let nonce = format!("{:064x}", self.nonce.fetch_add(1, Ordering::SeqCst) + 1);
        let statement =
            arca::handlers::action_statement("node1", id, action, digest, self.now(), &nonce)
                .expect("the statement");
        let approval = common::approve_statement(&self.key, &statement);
        let executor = ActionExecutor::new(
            self.handlers
                .iter()
                .map(|(verb, handler)| (*verb, Arc::clone(handler))),
        );
        arca::handlers::run_approved_action(
            &self.kernel,
            &executor,
            &json!({ "plan": id, "approval": approval }),
            self.now(),
        )
    }

    /// The node config the rig serves with.
    fn config(&self) -> NodeConfig {
        node_config(&self.dir, &self.socket, &self.kernel_config, &self.node)
    }

    fn template(&self, name: &str, value: Value) {
        let dir = self.dir.join("templates");
        fs::create_dir_all(&dir).expect("templates");
        fs::write(dir.join(format!("{name}.json")), value.to_string()).expect("template");
    }

    fn get(&self, path: &str) -> Option<Value> {
        UnixIncus::new(self.socket.clone()).get(path).ok()
    }

    fn entries(&self) -> Vec<arca_kernel::desired::Entry> {
        self.kernel.lock().expect("kernel").entries().to_vec()
    }

    fn entry(&self, id: &str) -> Value {
        self.entries()
            .into_iter()
            .find(|entry| entry.id.0 == id)
            .unwrap_or_else(|| panic!("entry {id}"))
            .config
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        if self.fake.is_some() || self.socket.starts_with("/var/lib/incus") {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}

/// A kernel that only fills the rig's field while the real one is dropped for a restart.
fn placeholder_kernel() -> Kernel {
    let scratch = common::Scratch::new("tenant-placeholder");
    let fake = FakeIncus::start(&scratch.dir, empty_state()).expect("fake");
    let verifier = common::verifier_for(&scratch, &[], "placeholder");
    common::kernel(&scratch, &fake, verifier, NOW).expect("placeholder")
}

fn empty_state() -> FakeState {
    FakeState {
        server: json!({}),
        projects: vec![],
        instances: vec![],
        networks: vec![],
        pools: vec![],
    }
}

/// A plain refusal: its code and its sentence. A refusal for want of room or for asking too often is not one, and is
/// read with [`listed`].
fn refusal(result: Result<Value, CallError>) -> (String, String) {
    match result {
        Err(CallError::Refused { code, message }) => (code, message),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// A `capacity` or `rate` refusal: its code, its sentence and its fields, checked against the document's list.
fn listed(result: Result<Value, CallError>) -> (String, String, Value) {
    common::listed_refusal(result)
}

/// A container template with two ports, the first of which a link opens.
fn app_template(name: &str) -> Value {
    json!({
        "name": name, "kind": "oci", "image": "docker.io/library/app", "digest": DIGEST,
        "ports": [8080, 9090], "env": {},
        "provides": { "env": { "APP_ADDR": "{{host}}:{{port}}" } },
        "publishable": false
    })
}

fn vm_template() -> Value {
    json!({
        "name": "vm", "kind": "virtual-machine", "image": "images:debian/13",
        "limits": { "cpu": "2", "memory": "4GiB" }, "rootSize": "20GiB", "ports": [22]
    })
}

/// A node with tenants `A` and `B`, each at the node's default caps, and the templates `app` and `vm`.
fn two_tenants(tenants: Value) -> Rig {
    let rig = Rig::new(Some(tenants));
    rig.template("app", app_template("app"));
    rig.template("vm", vm_template());
    for id in [A, B] {
        rig.create_tenant(json!({ "id": id })).expect("tenant");
    }
    rig
}

fn project(id: &str) -> String {
    format!("arcak-t-{id}")
}

#[path = "tenants/admission.rs"]
mod admission;
#[path = "tenants/create_stopped.rs"]
mod create_stopped;
#[path = "tenants/deploys.rs"]
mod deploys;
#[path = "tenants/gate.rs"]
mod gate;
#[path = "tenants/isolation.rs"]
mod isolation;
#[path = "tenants/kernel_access.rs"]
mod kernel_access;
#[path = "tenants/real_incus.rs"]
mod real_incus;
#[path = "tenants/refusal_codes.rs"]
mod refusal_codes;
