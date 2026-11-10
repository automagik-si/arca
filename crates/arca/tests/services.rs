//! Service verbs against a fake Incus: create, list and status are hermetic; no live host is touched.
mod common;
#[path = "../../arca-approval/tests/common/webauthn.rs"]
mod webauthn;

use arca::{
    config::NodeConfig,
    handlers::{StatusHandler, service_handlers},
    kernel::Kernel,
    service,
    service::Service,
};
use arca_api::Scope;
use arca_approval::{ApprovalVerifier, FileNonces, OwnerKeys, PasskeyVerifier, SshSigVerifier};
use arca_incus::{FakeIncus, FakeState, IncusRead, UnixIncus};
use arca_kernel::canon::Digest;
use arca_kernel::desired::EntryId;
use arca_kernel::identity::NodeKey;
use arca_kernel::journal::{AclPatch, Inverse};
use arca_kernel::ownership::{IncusKind, KERNEL_ENTRY_KEY, ObjectRef};
use arca_kernel::plugin::{
    EffectOp, EffectRefusal, Observe, Planned, Provider, ReadBack, Retirement,
};
use arca_mcp::{CallError, Caller, ToolHandler};
use arca_metrics::{HostMode, HostModeChoice};
use arca_netproject::PROJECT;
use arca_providers::incus::{Family, IncusProvider};
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
static SEQ: AtomicU64 = AtomicU64::new(0);

// Pauses the instance retirement at the provider boundary, after its plan exists.
struct RemovalPause {
    entered: std::sync::mpsc::Sender<Digest>,
    release: std::sync::mpsc::Receiver<()>,
}

struct PausedProvider {
    inner: IncusProvider,
    pause: Option<RemovalPause>,
    fail_observe: Arc<Mutex<Option<String>>>,
    unknown_create: Arc<Mutex<bool>>,
}

impl Provider for PausedProvider {
    fn observe(
        &self,
        entry: &EntryId,
        projects: &[String],
        query: &Observe,
    ) -> Result<Value, EffectRefusal> {
        if self.fail_observe.lock().expect("failure switch").as_deref() == Some(entry.0.as_str()) {
            return Err(EffectRefusal::Provider(
                "Incus answered 503 for a transient read".into(),
            ));
        }
        self.inner.observe(entry, projects, query)
    }
    fn plan(&self, entry: &EntryId, op: &EffectOp) -> Result<Planned, EffectRefusal> {
        self.inner.plan(entry, op)
    }
    fn effect(
        &mut self,
        entry: &EntryId,
        op: &EffectOp,
        planned: &Planned,
    ) -> Result<(ObjectRef, Inverse), EffectRefusal> {
        // An instance create whose answer never came: the provider cannot tell whether it landed.
        if *self.unknown_create.lock().expect("unknown switch")
            && matches!(
                op,
                EffectOp::IncusCreate {
                    kind: IncusKind::Instance,
                    ..
                }
            )
        {
            return Err(EffectRefusal::Unresolved);
        }
        self.inner.effect(entry, op, planned)
    }
    fn read_back(
        &self,
        entry: &EntryId,
        target: &ObjectRef,
        pre_image: Option<&std::collections::BTreeMap<String, Option<String>>>,
    ) -> Result<ReadBack, EffectRefusal> {
        self.inner.read_back(entry, target, pre_image)
    }
    fn read_back_acl(
        &self,
        entry: &EntryId,
        target: &ObjectRef,
        patch: &AclPatch,
    ) -> Result<ReadBack, EffectRefusal> {
        self.inner.read_back_acl(entry, target, patch)
    }
    fn retire(
        &mut self,
        entry: &EntryId,
        object: &ObjectRef,
        plan: &Digest,
    ) -> Result<Retirement, EffectRefusal> {
        if object.kind == IncusKind::Instance
            && let Some(pause) = self.pause.take()
        {
            pause
                .entered
                .send(plan.clone())
                .expect("report retirement plan");
            pause
                .release
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("release retirement");
        }
        self.inner.retire(entry, object, plan)
    }
}

struct Rig {
    dir: PathBuf,
    fake: FakeIncus,
    kernel: Arc<Mutex<Kernel>>,
    handlers: HashMap<&'static str, Arc<dyn ToolHandler>>,
    fail_observe: Arc<Mutex<Option<String>>>,
    unknown_create: Arc<Mutex<bool>>,
}

impl Rig {
    fn new() -> Self {
        Self::with(
            FakeState {
                server: json!({}),
                projects: vec![],
                instances: vec![],
                networks: vec![],
                pools: vec![],
            },
            vec![PROJECT.to_string()],
            Some("192.0.2.0/24"),
        )
    }

    fn with(state: FakeState, projects: Vec<String>, cidr: Option<&str>) -> Self {
        Self::with_key(state, projects, cidr, None)
    }

    fn with_key(
        state: FakeState,
        projects: Vec<String>,
        cidr: Option<&str>,
        key: Option<&common::SkKey>,
    ) -> Self {
        Self::with_key_and_pause(state, projects, cidr, key, None)
    }

    fn with_key_and_pause(
        state: FakeState,
        projects: Vec<String>,
        cidr: Option<&str>,
        key: Option<&common::SkKey>,
        pause: Option<RemovalPause>,
    ) -> Self {
        let verifier = key.map(|key| {
            let scratch = common::Scratch::new("service-owner");
            common::verifier_for(&scratch, &[key], "service")
        });
        Self::with_verifier(state, projects, cidr, verifier, pause)
    }

    /// The rig over `verifier`, or over a verifier that trusts no key when it is `None`.
    fn with_verifier(
        state: FakeState,
        projects: Vec<String>,
        cidr: Option<&str>,
        verifier: Option<ApprovalVerifier<OwnerKeys, FileNonces>>,
        pause: Option<RemovalPause>,
    ) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "arca-service-{}-{}",
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
        let mut kernel_config = json!({
            "projects": projects,
            "droiddProjects": ["other"]
        });
        if let Some(cidr) = cidr {
            kernel_config["serviceCidr"] = json!(cidr);
        }
        let cfg: NodeConfig = serde_json::from_value(json!({
            "stateDir": dir,
            "incus": {"socket": fake.socket()},
            "listen": {"unix": dir.join("arca.sock")},
            "tokens": {"hashDir": dir.join("tokens")},
            "kernel": kernel_config
        }))
        .expect("config");
        let verifier = match verifier {
            Some(verifier) => verifier,
            None => ApprovalVerifier::new(
                "node1".into(),
                OwnerKeys::new(
                    SshSigVerifier::new(vec![]),
                    PasskeyVerifier::refusing(arca_approval::ApprovalRefusal::NoVerifier),
                ),
                FileNonces::open(&dir.join("nonces")).expect("nonces"),
            ),
        };
        let mut plugins = PluginSet::new();
        plugins.add::<arca_netproject::NetProject>().expect("net");
        plugins.add::<Service>().expect("service");
        let socket = fake.socket().to_path_buf();
        let secrets = dir.join("secrets");
        let fail_observe = Arc::new(Mutex::new(None));
        let provider_fail_observe = Arc::clone(&fail_observe);
        let unknown_create = Arc::new(Mutex::new(false));
        let provider_unknown_create = Arc::clone(&unknown_create);
        let kernel = Arc::new(Mutex::new(
            Kernel::open(
                &dir,
                NodeKey::load_or_create(&dir.join("node.key")).expect("key"),
                plugins,
                verifier,
                "node1".into(),
                None,
                move |retained| {
                    Ok(Box::new(PausedProvider {
                        inner: IncusProvider::new(
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
                        pause,
                        fail_observe: provider_fail_observe,
                        unknown_create: provider_unknown_create,
                    }))
                },
                NOW,
            )
            .expect("kernel"),
        ));
        let cfg = Arc::new(cfg);
        let handlers = service_handlers(kernel.clone(), Arc::new(|| NOW), cfg)
            .into_iter()
            .collect();
        Self {
            dir,
            fake,
            kernel,
            handlers,
            fail_observe,
            unknown_create,
        }
    }

    fn fail_observe(&self, entry: &str) {
        *self.fail_observe.lock().expect("failure switch") = Some(entry.into());
    }

    fn call(&self, verb: &str, args: Value) -> Result<Value, CallError> {
        self.handlers[verb].call(
            args,
            &Caller {
                token_id: "doctor".into(),
                scope: Scope::Doctor,
                tools: None,
            },
        )
    }

    fn template(&self, name: &str, value: Value) {
        let dir = self.dir.join("templates");
        fs::create_dir_all(&dir).expect("templates");
        fs::write(dir.join(format!("{name}.json")), value.to_string()).expect("template");
    }

    fn get(&self, path: &str) -> Option<Value> {
        UnixIncus::new(self.fake.socket().to_path_buf())
            .get(path)
            .ok()
    }

    // Simulate edits made by an Incus operator, outside the kernel's writer.
    fn edit(&self, method: &str, path: &str, body: Value) {
        use std::io::{Read, Write};
        let mut stream = std::os::unix::net::UnixStream::connect(self.fake.socket()).unwrap();
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

    fn service_config(&self, name: &str) -> service::ServiceConfig {
        serde_json::from_value(
            self.entries()
                .into_iter()
                .find(|entry| entry.id.0 == format!("service-{name}"))
                .unwrap()
                .config,
        )
        .unwrap()
    }

    fn apply(
        &self,
        config: &service::ServiceConfig,
    ) -> Result<(), arca_kernel::plugin::PluginError> {
        use arca_kernel::{
            desired::EntryId,
            journal::{Journal, MemJournal},
            plugin::{Ctx, Plugin},
        };
        let mut provider = IncusProvider::new(
            self.fake.socket().into(),
            Family {
                projects: vec![PROJECT.into()],
                scratch: false,
                droidd_projects: vec!["other".into()],
                reserved_cidrs: vec![],
            },
            self.dir.join("secrets"),
            arca_providers::incus::RetainedSet::empty(),
        )
        .unwrap();
        let mut journal = Journal::open(MemJournal::default()).unwrap();
        Service::apply(
            &mut Ctx::new(
                EntryId(format!("service-{}", config.name)),
                &Service::manifest(),
                &mut provider,
                &mut journal,
                NOW,
            ),
            config,
        )
    }

    fn entries(&self) -> Vec<arca_kernel::desired::Entry> {
        self.kernel.lock().expect("kernel").entries().to_vec()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn nats_template() -> Value {
    json!({
        "name": "nats",
        "kind": "oci",
        "image": "docker.io/library/nats",
        "digest": DIGEST,
        "ports": [4222],
        "env": {},
        "publishable": false
    })
}

#[test]
fn service_create_list_status_round_trip() {
    let rig = Rig::new();
    rig.template("nats", nats_template());
    let created = rig
        .call("service_create", json!({"name":"nats","template":"nats"}))
        .expect("create");
    assert_eq!(created["name"], "nats");
    assert_eq!(created["template"], "nats");
    assert_eq!(created["state"], "running");
    assert_eq!(created["ipv4"], "192.0.2.10");
    assert_eq!(created["ports"], json!([4222]));
    assert_eq!(created["links"], json!([]));
    assert_eq!(created["published"], Value::Null);
    assert_eq!(created["lanPort"], Value::Null);
    let listed = rig.call("service_list", json!({})).expect("list");
    assert_eq!(listed["services"], json!([created]));
    let status = rig
        .call("service_status", json!({"name":"nats"}))
        .expect("status");
    assert_eq!(status, created);
    let instance = rig
        .get(&format!("/1.0/instances/nats?project={PROJECT}"))
        .expect("instance");
    assert_eq!(instance["config"]["user.arca.template"], "nats");
    assert_eq!(instance["config"]["user.arca.address"], "192.0.2.10");
    let acl = rig
        .get(&format!(
            "/1.0/network-acls/{PROJECT}-svc-nats?project=default"
        ))
        .expect("acl");
    assert_eq!(acl["config"]["user.arca.managed"], "true");
}

#[test]
fn service_list_reports_a_lan_port_only_when_the_template_has_a_lan_proxy() {
    let rig = Rig::new();
    rig.template("nats", nats_template());
    let nats = rig
        .call("service_create", json!({"name":"nats","template":"nats"}))
        .expect("nats create");
    assert_eq!(
        nats["lanPort"],
        Value::Null,
        "a container port is not a host listener"
    );

    let mut lan = nats_template();
    lan["name"] = json!("automagik");
    lan["ports"] = json!([3081]);
    lan["lan"] = json!({"port": 3081});
    rig.template("automagik", lan);
    let automagik = rig
        .call(
            "service_create",
            json!({"name":"automagik","template":"automagik"}),
        )
        .expect("automagik create");
    assert_eq!(automagik["lanPort"], json!(3081));

    let listed = rig.call("service_list", json!({})).expect("list");
    let rows = listed["services"].as_array().expect("services");
    assert_eq!(rows.len(), 2);
    for row in rows {
        let expected = match row["name"].as_str() {
            Some("automagik") => json!(3081),
            Some("nats") => Value::Null,
            other => panic!("unexpected service {other:?}"),
        };
        assert_eq!(row["lanPort"], expected);
    }
}

#[test]
fn node_status_answers_while_the_kernel_is_locked_and_after_it_is_poisoned() {
    let rig = Rig::new();
    rig.template("nats", nats_template());
    rig.call("service_create", json!({"name":"nats","template":"nats"}))
        .expect("create");
    let kernel = rig.kernel.lock().expect("kernel");
    let seq = kernel.applied_seq().expect("applied sequence");
    let status = Arc::new(StatusHandler {
        node: "node1".into(),
        mode: "kernel",
        applied_seq: kernel.applied_seq_reader(),
        started: std::time::Instant::now(),
        socket: rig.fake.socket().to_path_buf(),
        metrics_source: Arc::new(Mutex::new(HostModeChoice {
            mode: HostMode::Local,
            reason: "the test names the local mode".to_string(),
            provisional: false,
        })),
        approval: None,
    });
    let caller = Caller {
        token_id: "doctor".into(),
        scope: Scope::Doctor,
        tools: None,
    };
    let (send, receive) = std::sync::mpsc::channel();
    let reader = Arc::clone(&status);
    let worker = std::thread::spawn(move || {
        send.send(reader.call(json!({}), &caller)).expect("reply");
    });
    let result = receive.recv_timeout(std::time::Duration::from_secs(2));
    drop(kernel);
    worker.join().expect("status thread");
    let row = result
        .expect("status does not wait for the kernel lock")
        .expect("status");
    assert_eq!(row["appliedSeq"], seq);

    let kernel = Arc::clone(&rig.kernel);
    assert!(
        std::thread::spawn(move || {
            let _held = kernel.lock().expect("kernel");
            panic!("poison the kernel lock");
        })
        .join()
        .is_err()
    );
    let row = status
        .call(
            json!({}),
            &Caller {
                token_id: "doctor".into(),
                scope: Scope::Doctor,
                tools: None,
            },
        )
        .expect("status survives a poisoned kernel lock");
    assert_eq!(row["appliedSeq"], seq);
}

fn hand_instance(name: &str, project: &str, devices: Value) -> Value {
    json!({
        "name": name,
        "project": project,
        "type": "container",
        "status": "Running",
        "config": {},
        "devices": devices
    })
}

fn source_template(kind: &str, image: &str, digest: Option<&str>) -> service::Template {
    service::Template {
        name: "probe".into(),
        kind: kind.into(),
        image: image.into(),
        digest: digest.map(str::to_string),
        commit: None,
        ports: vec![],
        env: std::collections::BTreeMap::new(),
        provides: None,
        secrets: vec![],
        publishable: false,
        dash_port: None,
        description: None,
        entrypoint: None,
        generate: std::collections::BTreeMap::new(),
        droidd_port: None,
        limits: None,
        autostart: false,
        data: None,
        lan: None,
        root_size: None,
    }
}

#[test]
fn generate_and_droidd_port_the_key_is_made_at_create_in_its_own_form_before_the_first_start_the_acl_lets_the_bridge_gateway_reach_droidd_port_alone()
 {
    let rig = Rig::new();
    rig.template(
        "omni",
        json!({
            "name":"omni","kind":"oci","image":"ghcr.io/automagik-si/omni-api",
            "digest":DIGEST,"ports":[8080,8882],
            "env":{"OMNI_API_KEY":"{{secret:OMNI_API_KEY}}"},
            "secrets":["OMNI_API_KEY"],"generate":{"OMNI_API_KEY":"omni_sk_"},
            "droiddPort":8882,"publishable":false
        }),
    );
    let first = rig
        .call("service_create", json!({"name":"omni","template":"omni"}))
        .expect("create");
    assert_eq!(first["state"], "running");
    let instance = rig
        .get(&format!("/1.0/instances/omni?project={PROJECT}"))
        .expect("instance");
    let key = instance["config"]["environment.OMNI_API_KEY"]
        .as_str()
        .expect("generated key");
    assert!(key.starts_with("omni_sk_"));
    assert_eq!(key.len(), "omni_sk_".len() + 32);
    assert!(
        key["omni_sk_".len()..]
            .chars()
            .all(|c| c.is_ascii_alphanumeric())
    );
    let acl = rig
        .get(&format!(
            "/1.0/network-acls/{PROJECT}-svc-omni?project=default"
        ))
        .expect("acl");
    let ingress = acl["ingress"].as_array().expect("ingress");
    assert_eq!(ingress.len(), 1, "only droiddPort is open");
    assert_eq!(ingress[0]["source"], "192.0.2.1/32");
    assert_eq!(ingress[0]["destination_port"], "8882");
    let requests = rig.fake.requests();
    let acl_write = requests
        .iter()
        .position(|request| request.method == "POST" && request.path.contains("network-acls"))
        .unwrap();
    // start=true on the instance POST is Incus' first start.
    let first_start = requests
        .iter()
        .position(|request| request.method == "POST" && request.path.starts_with("/1.0/instances?"))
        .unwrap();
    assert!(acl_write < first_start);
    rig.template(
        "omni",
        json!({
            "name":"omni","kind":"oci","image":"ghcr.io/automagik-si/omni-api",
            "digest":DIGEST,"ports":[8080,8882],
            "env":{"OMNI_API_KEY":"{{secret:OMNI_API_KEY}}"},
            "secrets":["OMNI_API_KEY"],"generate":{"OMNI_API_KEY":"omni_sk_"},
            "droiddPort":8882,"publishable":false
        }),
    );
    rig.call("service_create", json!({"name":"omni2","template":"omni"}))
        .expect("second");
    let second = rig
        .get(&format!("/1.0/instances/omni2?project={PROJECT}"))
        .expect("instance");
    assert_ne!(
        key,
        second["config"]["environment.OMNI_API_KEY"]
            .as_str()
            .expect("second key")
    );
}

#[test]
fn service_create_sets_an_oci_templates_entrypoint_from_the_template_file_alone_no_calls_input_secret_or_link_reaches_it()
 {
    let rig = Rig::new();
    rig.template(
        "jet",
        json!({
            "name":"jet","kind":"oci","image":"docker.io/library/jet","digest":DIGEST,
            "ports":[8080],"env":{"TOKEN":"{{secret:TOKEN}}"},"secrets":["TOKEN"],"generate":{"TOKEN":"test_"},"publishable":false,
            "entrypoint":["/bin/sh","-ec",{"file":"jet.sh"},"jet"]
        }),
    );
    fs::write(
        rig.dir.join("templates/jet.sh"),
        "echo ok; exec jet --token \"$TOKEN\" --provider \"$PROVIDER\"\n",
    )
    .expect("script");
    let refused = rig.call(
        "service_create",
        json!({"name":"jet","template":"jet","entrypoint":["evil"],"links":[{"to":"evil","port":1234}]}),
    );
    assert!(matches!(refused, Err(CallError::Invalid(_))));
    assert!(
        rig.get(&format!("/1.0/instances/jet?project={PROJECT}"))
            .is_none(),
        "the extra input created nothing"
    );
    let created = rig
        .call("service_create", json!({"name":"jet","template":"jet"}))
        .expect("create");
    assert_eq!(created["ipv4"], "192.0.2.10");
    let instance = rig
        .get(&format!("/1.0/instances/jet?project={PROJECT}"))
        .expect("instance");
    let entrypoint = instance["config"]["oci.entrypoint"]
        .as_str()
        .expect("entrypoint");
    assert!(entrypoint.contains("echo ok"));
    assert!(!entrypoint.contains("evil"));
    assert!(entrypoint.contains("$TOKEN"));
    assert!(entrypoint.contains("$PROVIDER"));
    let token = fs::read_to_string(rig.dir.join("secrets/service-jet/token")).unwrap();
    assert!(!entrypoint.contains(&token));
    let stored = rig
        .entries()
        .into_iter()
        .find(|entry| entry.id.0 == "service-jet")
        .unwrap();
    assert_eq!(
        stored.config["environment"]["TOKEN"],
        "arca-secret:service-jet/token"
    );
    rig.template("nats", nats_template());
    rig.call(
        "service_create",
        json!({"name":"provider","template":"nats"}),
    )
    .unwrap();
    let provider = rig
        .get(&format!("/1.0/instances/provider?project={PROJECT}"))
        .unwrap();
    let mut config = rig.service_config("jet");
    config.links.push(service::ServiceLink {
        to: "provider".into(),
        port: 4222,
        env: vec!["PROVIDER".into()],
        uuid: provider["config"]["volatile.uuid"].as_str().unwrap().into(),
    });
    config
        .environment
        .insert("PROVIDER".into(), "192.0.2.11:4222".into());
    rig.edit(
        "PUT",
        &format!("/1.0/instances/jet/state?project={PROJECT}"),
        json!({"action":"stop"}),
    );
    rig.edit(
        "DELETE",
        &format!("/1.0/instances/jet?project={PROJECT}"),
        json!({}),
    );
    rig.apply(&config).unwrap();
    let linked = rig
        .get(&format!("/1.0/instances/jet?project={PROJECT}"))
        .unwrap();
    assert_eq!(linked["config"]["oci.entrypoint"], entrypoint);
    assert!(
        !linked["config"]["oci.entrypoint"]
            .as_str()
            .unwrap()
            .contains("192.0.2.11:4222")
    );
    assert_eq!(linked["config"]["environment.PROVIDER"], "192.0.2.11:4222");
    assert_eq!(
        linked["config"]["user.arca.links"],
        format!(
            "provider:4222:{}",
            provider["config"]["volatile.uuid"].as_str().unwrap()
        )
    );
    assert_eq!(linked["config"]["user.arca.envkeys"], "PROVIDER,TOKEN");
    assert!(!entrypoint.contains('\n'));
}

#[test]
fn service_create_puts_each_service_in_the_project_with_desired_service_nic_its_own_nic_acl_and_its_user_arca_keys_and_starts_it()
 {
    let rig = Rig::new();
    rig.template("nats", nats_template());
    let created = rig
        .call("service_create", json!({"name":"nats","template":"nats"}))
        .expect("create");
    assert_eq!(
        created,
        json!({"name":"nats","template":"nats","state":"running","ipv4":"192.0.2.10","ports":[4222],"links":[],"published":null,"lanPort":null})
    );
    let instance = rig
        .get(&format!("/1.0/instances/nats?project={PROJECT}"))
        .expect("instance");
    assert_eq!(instance["config"]["user.arca.template"], "nats");
    assert_eq!(instance["config"]["user.arca.address"], "192.0.2.10");
    assert_eq!(instance["config"]["user.arca.ports"], "4222");
    assert_eq!(
        instance["devices"]["eth0"]["security.acls"],
        format!("{PROJECT}-svc-nats")
    );
    assert_eq!(instance["devices"]["eth0"]["network"], "arcak-svcbr");
    assert_eq!(instance["profiles"], json!(["arca-service"]));
    let writes = rig.fake.writes();
    let acl = writes
        .iter()
        .position(|write| write.path.contains("network-acls"))
        .expect("ACL create");
    let create = writes
        .iter()
        .position(|write| write.path.contains("/instances") && write.method == "POST")
        .expect("instance create");
    assert!(acl < create, "the ACL is made before the instance");
    rig.template(
        "api",
        json!({
            "name":"api","kind":"oci","image":"docker.io/library/api","digest":DIGEST,
            "ports":[8080],"env":{"PORT":"{{port}}","SELF":"{{name}}@{{host}}"},
            "publishable":false
        }),
    );
    let api = rig
        .call("service_create", json!({"name":"api","template":"api"}))
        .expect("create api");
    assert_eq!(api["ipv4"], "192.0.2.11");
    let instance = rig
        .get(&format!("/1.0/instances/api?project={PROJECT}"))
        .expect("api instance");
    assert_eq!(instance["config"]["environment.PORT"], "8080");
    assert_eq!(instance["config"]["environment.SELF"], "api@192.0.2.11");
    assert_eq!(instance["config"]["user.arca.envkeys"], "PORT,SELF");
}

#[test]
fn a_failed_create_leaves_no_acl_behind_and_no_row() {
    let rig = Rig::new();
    let mut template = nats_template();
    template["secrets"] = json!(["TOKEN"]);
    template["generate"] = json!({"TOKEN":"test_"});
    template["env"] = json!({"TOKEN":"{{secret:TOKEN}}"});
    rig.template("nats", template);
    let secrets = rig.dir.join("secrets/service-nats");
    fs::create_dir_all(&secrets).unwrap();
    fs::write(secrets.join("owner-seeded"), "keep this").unwrap();
    fs::set_permissions(
        secrets.join("owner-seeded"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    rig.fake.set_reject_config_on_post("/1.0/instances");
    let failed = rig.call("service_create", json!({"name":"nats","template":"nats"}));
    let Err(CallError::Refused { message, .. }) = failed else {
        panic!("a failed create is refused");
    };
    assert_eq!(
        message,
        "create service nats from template nats: the create did not land: Incus said \"config is not accepted on create\"; nothing was left behind"
    );
    assert!(
        rig.get(&format!(
            "/1.0/network-acls/{PROJECT}-svc-nats?project=default"
        ))
        .is_none(),
        "the ACL is gone"
    );
    assert_eq!(
        fs::read_to_string(secrets.join("owner-seeded")).unwrap(),
        "keep this"
    );
    assert_eq!(
        fs::metadata(secrets.join("owner-seeded"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(!secrets.join("token").exists());
    assert!(
        !rig.entries()
            .iter()
            .any(|entry| entry.id.0 == "service-nats"),
        "no service row"
    );
    assert!(
        !rig.fake
            .requests()
            .iter()
            .any(|request| request.method == "POST"
                && request.path.contains("/instances")
                && request.path.contains("nats")),
        "no instance was made"
    );
    assert!(
        matches!(rig.call("service_create", json!({"name":"another","template":"nats"})),
        Err(CallError::Refused { message, .. }) if message == "create service another from template nats: the create did not land: Incus said \"config is not accepted on create\"; nothing was left behind")
    );
    assert!(
        !rig.dir.join("secrets/service-another").exists(),
        "an empty generated directory is removed"
    );
}

#[test]
fn the_tools_operator_scope_read_only_reads_a_viewer_reads_and_changes_nothing() {
    use arca_api::{Scope, may_call, verb};
    let create = verb("service_create").expect("create");
    let list = verb("service_list").expect("list");
    let status = verb("service_status").expect("status");
    assert!(create.writes);
    assert!(!create.destructive);
    assert!(list.writes == false && status.writes == false);
    assert!(list.min_scope == Scope::Viewer && status.min_scope == Scope::Viewer);
    assert!(!may_call(Scope::Viewer, create));
    assert!(may_call(Scope::Viewer, list));
    assert!(may_call(Scope::Viewer, status));
    let rig = Rig::new();
    let viewer = Caller {
        token_id: "viewer".into(),
        scope: Scope::Viewer,
        tools: None,
    };
    assert!(matches!(
        rig.handlers["service_create"].call(json!({"name":"nats","template":"nats"}), &viewer),
        Err(CallError::Forbidden { .. })
    ));
    assert!(rig.call("service_list", json!({})).is_ok());
}

#[test]
fn a_prefix_without_a_service_subnet_starts_and_every_service_tool_refuses() {
    let rig = Rig::with(
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![],
            networks: vec![],
            pools: vec![],
        },
        vec![PROJECT.to_string()],
        None,
    );
    for (verb, args) in [
        ("service_create", json!({"name":"nats","template":"nats"})),
        ("service_list", json!({})),
        ("service_status", json!({"name":"nats"})),
    ] {
        assert!(matches!(
            rig.call(verb, args),
            Err(CallError::Refused { message, .. }) if message.contains("no service project")
        ));
    }
    assert!(rig.fake.requests().is_empty(), "Incus was not contacted");
}

#[test]
fn a_new_services_address_is_none_an_instances_nic_any_of_them_or_a_lease_of_the_bridge_holds() {
    let rig = Rig::with(
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![hand_instance(
                "hand",
                PROJECT,
                json!({"eth0":{"type":"nic","network":"arcak-svcbr","ipv4.address":"192.0.2.13"},"eth1":{"type":"nic","network":"arcak-svcbr","ipv4.address":"192.0.2.11"}}),
            )],
            networks: vec![json!({
                "name":"arcak-svcbr","project":"default","type":"bridge",
                "config":{"user.arcakernel.entry":"net-arcak-svc","ipv4.address":"192.0.2.1/24","ipv6.address":"none"}
            })],
            pools: vec![],
        },
        vec![PROJECT.to_string()],
        Some("192.0.2.0/24"),
    );
    rig.fake
        .set_lease("arcak-svcbr", "00:11:22:33:44:55", "192.0.2.10");
    rig.fake
        .set_lease("arcak-svcbr", "00:11:22:33:44:66", "192.0.2.12");
    rig.template("nats", nats_template());
    let created = rig
        .call("service_create", json!({"name":"nats","template":"nats"}))
        .expect("create");
    assert_eq!(created["ipv4"], "192.0.2.14");
}

#[test]
fn makes_the_incus_source_of_a_service_an_oci_image_pulled_by_its_digest_a_system_image_by_alias_or_fingerprint_nothing_else()
 {
    use arca::service::source;
    let oci = source(&source_template(
        "oci",
        "docker.io/library/nats",
        Some("sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
    ))
    .expect("oci");
    assert_eq!(oci["protocol"], "oci");
    assert_eq!(oci["server"], "https://docker.io");
    assert_eq!(oci["mode"], "pull");
    assert_eq!(
        oci["alias"],
        "library/nats@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
    );
    let system = source(&source_template("system", "images:alpine/3.20", None)).expect("system");
    assert_eq!(system["protocol"], "simplestreams");
    assert_eq!(system["server"], "https://images.linuxcontainers.org");
    assert_eq!(system["alias"], "alpine/3.20");
    let fingerprint = source(&source_template(
        "system",
        "images:alpine/3.20",
        Some(DIGEST),
    ))
    .expect("fingerprint");
    assert_eq!(
        fingerprint["fingerprint"],
        DIGEST.strip_prefix("sha256:").unwrap()
    );
    assert_eq!(fingerprint["alias"], "alpine/3.20");
    let local = source(&source_template("system", "local:my-image", None)).expect("local");
    assert_eq!(local, json!({"type":"image","alias":"my-image"}));
    assert!(source(&source_template("oci", "docker.io/library/nats", None)).is_err());
    assert!(source(&source_template("system", "https://example.org/x", None)).is_err());
}

#[test]
fn lists_the_service_project_with_each_instances_own_config_and_devices_none_when_incus_has_no_such_project()
 {
    let mut hand = hand_instance(
        "hand",
        PROJECT,
        json!({"eth0":{"type":"nic","ipv4.address":"192.0.2.13"}}),
    );
    hand["config"] = json!({"environment.IMAGE_SETTING":"own-config"});
    hand["expanded_config"] =
        json!({"environment.IMAGE_SETTING":"profile-config", "user.arca.template":"profile-only"});
    let rig = Rig::with(
        FakeState {
            server: json!({}),
            projects: vec![json!({"name":PROJECT,"config":{}})],
            instances: vec![hand, hand_instance("other", "default", json!({}))],
            networks: vec![],
            pools: vec![],
        },
        vec![PROJECT.to_string()],
        Some("192.0.2.0/24"),
    );
    let listing = rig
        .get(&format!("/1.0/instances?project={PROJECT}&recursion=2"))
        .unwrap();
    let instances = service::project_instances(&listing, PROJECT);
    assert_eq!(instances.len(), 1);
    assert_eq!(
        instances[0]["config"],
        json!({"environment.IMAGE_SETTING":"own-config"})
    );
    assert_eq!(
        instances[0]["devices"],
        json!({"eth0":{"type":"nic","ipv4.address":"192.0.2.13"}})
    );
    assert_ne!(instances[0]["config"], instances[0]["expanded_config"]);
    let listed = rig.call("service_list", json!({})).expect("list");
    let rows = listed["services"].as_array().expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["name"], "hand");
    assert_eq!(rows[0]["state"], "unknown");
    assert_eq!(rows[0]["ipv4"], "192.0.2.13");
    assert_eq!(
        rows[0]["template"], "",
        "an unmarked instance config is not a service record"
    );
    assert!(rig.fake.requests().iter().any(|request| request.path
        == "/1.0/instances?all-projects=true&recursion=2"
        && request.status == 200));
    let empty = Rig::new();
    assert_eq!(
        empty.call("service_list", json!({})).expect("empty"),
        json!({"services":[]})
    );
    assert!(empty.fake.requests().iter().any(|request| request.path
        == "/1.0/instances?all-projects=true&recursion=2"
        && request.status == 200));
}

#[test]
fn refuses_every_service_call_without_a_service_project_asking_incus_nothing() {
    let rig = Rig::with(
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![],
            networks: vec![],
            pools: vec![],
        },
        vec![],
        Some("192.0.2.0/24"),
    );
    for (verb, args) in [
        ("service_create", json!({"name":"nats","template":"nats"})),
        ("service_list", json!({})),
        ("service_status", json!({"name":"nats"})),
    ] {
        assert!(matches!(
            rig.call(verb, args),
            Err(CallError::Refused { message, .. }) if message.contains("no service project")
        ));
    }
    assert!(rig.fake.requests().is_empty());
}

#[test]
fn creates_a_service_in_the_service_project_only_on_its_one_profile_and_nic_marked_refuses_an_unmarked_one_or_a_nic_without_its_acl()
 {
    let rig = Rig::new();
    rig.template("nats", nats_template());
    rig.call("service_create", json!({"name":"nats","template":"nats"}))
        .expect("create");
    let instance = rig
        .get(&format!("/1.0/instances/nats?project={PROJECT}"))
        .expect("instance");
    assert_eq!(instance["config"]["user.arcakernel.entry"], "service-nats");
    assert_eq!(
        instance["devices"]["eth0"]["security.acls"],
        format!("{PROJECT}-svc-nats")
    );
    assert_eq!(instance["profiles"], json!(["arca-service"]));
    let config = rig.service_config("nats");
    let path = format!("/1.0/instances/nats?project={PROJECT}");
    rig.edit("PATCH", &path, json!({"config":{"user.arca.template":""}}));
    assert!(
        rig.apply(&config).is_err(),
        "an unmarked instance is refused"
    );
    rig.edit("PATCH", &path, json!({"config":{"user.arca.template":"nats"},"devices":{"eth0":{"type":"nic","network":"arcak-svcbr","ipv4.address":"192.0.2.10"}}}));
    assert!(
        rig.apply(&config).is_err(),
        "a NIC without its ACL is refused"
    );
}

#[test]
fn reads_the_ipv4_leases_of_the_service_bridge_as_the_service_project_sees_them_none_without_the_network()
 {
    let with_lease = Rig::with(
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![],
            networks: vec![json!({
                "name":"arcak-svcbr","project":"default","type":"bridge",
                "config":{"user.arcakernel.entry":"net-arcak-svc","ipv4.address":"192.0.2.1/24","ipv6.address":"none"}
            })],
            pools: vec![],
        },
        vec![PROJECT.to_string()],
        Some("192.0.2.0/24"),
    );
    with_lease
        .fake
        .set_lease("arcak-svcbr", "00:11:22:33:44:55", "192.0.2.10");
    with_lease.template("nats", nats_template());
    assert_eq!(
        with_lease
            .call("service_create", json!({"name":"nats","template":"nats"}))
            .expect("create")["ipv4"],
        "192.0.2.11"
    );
    let no_network = Rig::new();
    no_network.template("nats", nats_template());
    assert_eq!(
        no_network
            .call("service_create", json!({"name":"nats","template":"nats"}))
            .expect("create")["ipv4"],
        "192.0.2.10"
    );
}

#[test]
fn keeps_a_service_acl_in_the_default_project_created_and_replaced_only_with_its_managed_key_and_only_under_a_service_acl_name()
 {
    let rig = Rig::new();
    rig.fake.set_host_interfaces(vec![(
        "uplink".into(),
        vec!["198.51.100.7".parse().unwrap()],
    )]);
    rig.template("nats", nats_template());
    rig.call("service_create", json!({"name":"nats","template":"nats"}))
        .expect("create");
    let acl = rig
        .get(&format!(
            "/1.0/network-acls/{PROJECT}-svc-nats?project=default"
        ))
        .expect("acl");
    assert_eq!(acl["name"], format!("{PROJECT}-svc-nats"));
    assert_eq!(acl["config"]["user.arca.managed"], "true");
    assert_eq!(
        arca::service::acl_name(PROJECT, "nats"),
        format!("{PROJECT}-svc-nats")
    );
    assert_eq!(acl["egress"][0]["action"], "reject");
    assert!(
        acl["egress"][0]["destination"]
            .as_str()
            .unwrap()
            .contains("10.0.0.0/8")
    );
    assert_eq!(acl["egress"][3]["action"], "reject");
    assert_eq!(
        acl["egress"][3]["destination"],
        "192.0.2.1/32,198.51.100.7/32"
    );
    assert_eq!(acl["egress"][4]["action"], "allow");
    assert_eq!(acl["egress"][4]["destination"], "");
    let config = rig.service_config("nats");
    let mut private = config.clone();
    private.gateway = "10.42.0.1".into();
    let rules = service::egress(
        &private,
        &json!(["198.51.100.7", "192.0.2.9", "198.51.100.7", "2001:db8::1"]),
    );
    assert_eq!(
        rules[3]["destination"],
        "10.42.0.1/32,192.0.2.9/32,198.51.100.7/32"
    );
    let rejected = rules[0]["destination"].as_str().unwrap();
    let rejects = |address: &str| {
        let address = u32::from(address.parse::<std::net::Ipv4Addr>().unwrap());
        rejected.split(',').any(|cidr| {
            let (base, prefix) = cidr.split_once('/').unwrap();
            let mask = u32::MAX << (32 - prefix.parse::<u32>().unwrap());
            address & mask == u32::from(base.parse::<std::net::Ipv4Addr>().unwrap())
        })
    };
    assert!(!rejects("10.42.0.10"));
    for address in [
        "10.41.255.255",
        "10.42.1.1",
        "172.16.0.1",
        "192.168.0.1",
        "100.64.0.1",
        "169.254.0.1",
        "127.0.0.1",
    ] {
        assert!(rejects(address));
    }
    let path = format!("/1.0/network-acls/{PROJECT}-svc-nats?project=default");
    rig.edit(
        "PATCH",
        &path,
        json!({"config":{},"ingress":[],"egress":[]}),
    );
    rig.apply(&config).expect("replace owned ACL");
    assert_eq!(rig.get(&path).unwrap()["egress"], acl["egress"]);
    let mut with_port = config.clone();
    with_port.droidd_port = Some(4222);
    rig.apply(&with_port).expect("replace ingress too");
    assert_eq!(
        rig.get(&path).unwrap()["ingress"][0]["destination_port"],
        "4222"
    );
    rig.edit(
        "PATCH",
        &path,
        json!({"config":{"user.arca.managed":"false"},"ingress":[],"egress":[]}),
    );
    let writes = rig.fake.writes().len();
    assert!(rig.apply(&config).is_err());
    assert_eq!(
        rig.fake.writes().len(),
        writes,
        "a mismatched managed key is refused before a write"
    );
    rig.edit("PATCH", &path, json!({"config":{"user.arca.managed":"true","user.arcakernel.entry":"foreign"},"ingress":[],"egress":[]}));
    let writes = rig.fake.writes().len();
    assert!(rig.apply(&config).is_err());
    assert_eq!(rig.fake.writes().len(), writes, "a foreign ACL is refused");
    let mut bad_name = config;
    bad_name.name = "../foreign".into();
    assert!(rig.apply(&bad_name).is_err());
}

#[test]
fn a_failed_start_keeps_the_created_instance_record_and_secrets() {
    let rig = Rig::new();
    let mut template = nats_template();
    template["secrets"] = json!(["TOKEN"]);
    template["generate"] = json!({"TOKEN":"test_"});
    template["env"] = json!({"TOKEN":"{{secret:TOKEN}}"});
    rig.template("nats", template);
    rig.fake.fail_instance_start_on_create();
    // The refusal carries Incus's own reason, says what is left on the node, and names the way forward both ways.
    match rig.call("service_create", json!({"name":"nats","template":"nats"})) {
        Err(CallError::Refused { message, .. }) => assert_eq!(
            message,
            "start service nats: the instance was created, and Incus did not start it: Incus said \"Failed starting instance\". Its instance exists and is stopped: instance_start starts it once the cause is fixed, and service_remove removes it"
        ),
        other => panic!("{other:?}"),
    }
    // The effect journal holds the create as applied, with the instance's delete as its inverse: no intent is left open
    // to block the next effect or to be guessed at after a restart.
    let (records, torn) =
        arca_kernel::journal::read_journal(&rig.dir.join(arca::kernel::JOURNAL_FILE)).unwrap();
    assert!(torn.is_none());
    let mut last = std::collections::BTreeMap::new();
    for record in &records {
        last.insert(record.id, record);
    }
    assert!(
        last.values()
            .all(|record| record.state != arca_kernel::journal::EffectState::Intent),
        "no dangling intent"
    );
    let created = last
        .values()
        .find(|record| {
            record.object.as_ref().is_some_and(|object| {
                object.kind == arca_kernel::ownership::IncusKind::Instance && object.name == "nats"
            })
        })
        .expect("the instance's create");
    assert_eq!(created.state, arca_kernel::journal::EffectState::Applied);
    assert!(matches!(
        created.inverse,
        Some(arca_kernel::journal::Inverse::IncusDelete { .. })
    ));
    assert!(
        rig.entries()
            .iter()
            .any(|entry| entry.id.0 == "service-nats")
    );
    assert_eq!(
        rig.get(&format!("/1.0/instances/nats?project={PROJECT}"))
            .unwrap()["status"],
        "Stopped"
    );
    assert!(
        fs::read_to_string(rig.dir.join("secrets/service-nats/token"))
            .unwrap()
            .starts_with("test_")
    );
    let listed = rig.call("service_list", json!({})).unwrap();
    assert_eq!(listed["services"].as_array().unwrap().len(), 1);
    assert_eq!(listed["services"][0]["name"], "nats");
    assert_eq!(listed["services"][0]["state"], "stopped");
    assert_eq!(
        rig.call("service_status", json!({"name":"nats"})).unwrap()["state"],
        "stopped"
    );
    assert!(
        matches!(rig.call("service_create", json!({"name":"nats","template":"nats"})), Err(CallError::Refused { message, .. }) if message == "service nats exists")
    );
}

#[test]
fn a_missing_instance_keeps_its_desired_service_address_reserved() {
    let rig = Rig::new();
    rig.template("nats", nats_template());
    rig.call("service_create", json!({"name":"first","template":"nats"}))
        .unwrap();
    rig.edit(
        "PUT",
        &format!("/1.0/instances/first/state?project={PROJECT}"),
        json!({"action":"stop"}),
    );
    rig.edit(
        "DELETE",
        &format!("/1.0/instances/first?project={PROJECT}"),
        json!({}),
    );
    assert!(
        rig.get(&format!("/1.0/instances/first?project={PROJECT}"))
            .is_none()
    );
    assert_eq!(
        rig.call("service_create", json!({"name":"second","template":"nats"}))
            .unwrap()["ipv4"],
        "192.0.2.11"
    );
}

#[test]
fn service_key_names_and_acl_home_match_the_public_contract() {
    let keys = json!({"template":"user.arca.template", "address":"user.arca.address", "ports":"user.arca.ports",
        "links":"user.arca.links", "published":"user.arca.published", "envkeys":"user.arca.envkeys", "managed":"user.arca.managed"});
    assert_eq!(service::TEMPLATE_KEY, keys["template"]);
    assert_eq!(service::ADDRESS_KEY, keys["address"]);
    assert_eq!(service::PORTS_KEY, keys["ports"]);
    assert_eq!(service::LINKS_KEY, keys["links"]);
    assert_eq!(service::PUBLISHED_KEY, keys["published"]);
    assert_eq!(service::ENVKEYS_KEY, keys["envkeys"]);
    assert_eq!(service::COOKIE, keys["managed"]);
    assert_eq!(service::acl_name("arcak-svc", "nats"), "arcak-svc-svc-nats");
    let rig = Rig::new();
    rig.template("nats", nats_template());
    rig.call("service_create", json!({"name":"nats","template":"nats"}))
        .unwrap();
    assert!(
        rig.get("/1.0/network-acls/arcak-svc-svc-nats?project=default")
            .is_some()
    );
    let instance = rig
        .get(&format!("/1.0/instances/nats?project={PROJECT}"))
        .unwrap();
    assert!(
        instance["config"]
            .as_object()
            .unwrap()
            .keys()
            .all(|key| !key.starts_with("user.arcakernel.service."))
    );
}

fn adopted_instance(
    name: &str,
    project: &str,
    template: &str,
    ipv4: &str,
    ports: &str,
    uuid: &str,
) -> Value {
    json!({
        "name": name,
        "project": project,
        "type": "container",
        "status": "Running",
        "config": {
            "user.arcakernel.entry": format!("adopt-{name}"),
            "user.arca.template": template,
            "user.arca.address": ipv4,
            "user.arca.ports": ports,
            "user.arca.envkeys": "POSTGRES_PASSWORD",
            "environment.POSTGRES_PASSWORD": "redacted-value",
            "volatile.uuid": uuid
        },
        "devices": {"eth0":{"type":"nic","name":"eth0","ipv4.address":ipv4}}
    })
}

#[test]
fn the_server_watch_covers_services_and_reports_a_missing_service_object() {
    use arca::handlers::watcher_step;
    let rig = Rig::with(
        FakeState {
            server: json!({}),
            projects: vec![json!({"name":"default","config":{}})],
            instances: vec![],
            networks: vec![],
            pools: vec![],
        },
        vec![PROJECT.into()],
        Some("192.0.2.0/24"),
    );
    rig.template("nats", nats_template());
    let cfg: NodeConfig = serde_json::from_value(json!({
        "stateDir":rig.dir, "incus":{"socket":rig.fake.socket()},
        "listen":{"unix":rig.dir.join("arca.sock")},
        "tokens":{"hashDir":rig.dir.join("tokens")},
        "kernel":{"projects":[PROJECT], "droiddProjects":["other"], "serviceCidr":"192.0.2.0/24"}
    }))
    .unwrap();
    let watch = arca::cmd_serve::watch_kernel_handler(&cfg, Arc::clone(&rig.kernel));
    rig.call("service_create", json!({"name":"nats","template":"nats"}))
        .unwrap();
    // The handler predates the record, so the sweep must read the current kernel entries.
    let mut last = None;
    watcher_step(&watch, NOW, &mut last);
    let report = watch.newest().unwrap().1;
    let project = report["projects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|project| project["project"] == PROJECT)
        .unwrap();
    assert_eq!(project["instances"][0]["owner"]["class"], "kernel");
    assert_eq!(project["instances"][0]["owner"]["entry"], "service-nats");
    assert_eq!(report["drift"], json!([]));
    assert!(
        report["projects"]
            .as_array()
            .unwrap()
            .iter()
            .any(|project| project["project"] == "default")
    );
    rig.edit(
        "PUT",
        &format!("/1.0/instances/nats/state?project={PROJECT}"),
        json!({"action":"stop"}),
    );
    rig.edit(
        "DELETE",
        &format!("/1.0/instances/nats?project={PROJECT}"),
        json!({}),
    );
    watcher_step(&watch, NOW + 30_000, &mut last);
    let report = watch.newest().unwrap().1;
    assert_eq!(
        report["drift"],
        json!([{"kind":"entryWithoutObject","entry":"service-nats"}])
    );
}

#[test]
fn a_droidd_service_before_import_stays_external_and_has_no_service_record() {
    let mut row = adopted_instance(
        "db1",
        "other",
        "postgres-autopg",
        "192.0.2.10",
        "5432",
        "uuid-db1",
    );
    row["config"]
        .as_object_mut()
        .unwrap()
        .remove("user.arcakernel.entry");
    let rig = Rig::with(
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![row],
            networks: vec![],
            pools: vec![],
        },
        vec![PROJECT.into(), "other".into()],
        Some("192.0.2.0/24"),
    );
    assert_eq!(
        rig.call("service_list", json!({})).unwrap()["services"],
        json!([])
    );
    assert!(
        rig.entries()
            .iter()
            .all(|entry| entry.id.0 != "service-db1")
    );
    let live = rig.get("/1.0/instances/db1?project=other").unwrap();
    let own = serde_json::from_value(live["config"].clone()).unwrap();
    assert_eq!(
        arca_kernel::ownership::classify(&own, &Default::default()),
        arca_kernel::ownership::OwnerClass::Droidd
    );
    let error = rig
        .call("service_create", json!({"name":"db1","template":"nats"}))
        .unwrap_err();
    assert!(
        matches!(error, CallError::Refused { message, .. } if message == "an instance named db1 already exists that is not a service of Arca's; it is left alone" && !message.contains("other") && !message.contains(PROJECT))
    );
    assert!(
        matches!(rig.call("service_status", json!({"name":"db1"})), Err(CallError::Refused { code, .. }) if code == "not_found")
    );
    // Invalid metadata outside the owned scope must not become a failed service either.
    rig.edit(
        "PATCH",
        "/1.0/instances/db1?project=other",
        json!({"config":{"user.arca.address":"invalid"}}),
    );
    assert_eq!(
        rig.call("service_list", json!({})).unwrap()["services"],
        json!([])
    );
    assert!(
        rig.entries()
            .iter()
            .all(|entry| entry.id.0 != "service-db1")
    );
    assert!(
        rig.fake
            .writes()
            .iter()
            .all(|write| write.method == "PATCH")
    );
}

#[test]
fn adoption_reads_the_exact_user_arca_keys_and_leaves_them_as_they_are() {
    let rig = Rig::with(
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![adopted_instance(
                "svc1",
                "arca",
                "postgres-autopg",
                "192.0.2.10",
                "5432",
                "uuid-svc1",
            )],
            networks: vec![],
            pools: vec![],
        },
        vec![PROJECT.to_string(), "arca".into()],
        Some("192.0.2.0/24"),
    );
    let listed = rig.call("service_list", json!({})).expect("list");
    let rows = listed["services"].as_array().expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["name"], "svc1");
    assert_eq!(rows[0]["template"], "postgres-autopg");
    assert_eq!(rows[0]["state"], "running");
    assert_eq!(rows[0]["ipv4"], "192.0.2.10");
    assert_eq!(rows[0]["ports"], json!([5432]));
    assert_eq!(rows[0]["links"], json!([]));
    assert!(
        rig.entries()
            .iter()
            .any(|entry| { entry.id.0 == "service-svc1" && entry.config["adopted"] == true })
    );
    let live = rig
        .get("/1.0/instances/svc1?project=arca")
        .expect("instance");
    assert_eq!(live["config"]["user.arca.template"], "postgres-autopg");
    assert_eq!(live["config"]["user.arca.address"], "192.0.2.10");
    assert_eq!(live["config"]["user.arca.ports"], "5432");
    assert_eq!(
        live["config"]["environment.POSTGRES_PASSWORD"],
        "redacted-value"
    );
    assert_eq!(
        rig.call("service_status", json!({"name":"svc1"})).unwrap()["state"],
        "running"
    );
    assert!(matches!(
        rig.call("service_create", json!({"name":"svc1","template":"nats"})),
        Err(CallError::Refused { message, .. }) if message.contains("svc1")
    ));
    assert!(matches!(
        rig.call("service_remove", json!({"name":"svc1","approval":"x"})),
        Err(CallError::Refused { code, .. }) if code == "adopted"
    ));
    // Deleting the record never touches the instance.
    let id = arca_kernel::desired::EntryId("service-svc1".into());
    rig.kernel
        .lock()
        .unwrap()
        .propose(vec![arca::desired_store::EntryChange::Remove(id)], NOW)
        .expect("record removal");
    assert!(rig.get("/1.0/instances/svc1?project=arca").is_some());
}

#[test]
fn a_marked_instance_with_unreadable_keys_is_listed_failed_and_left_alone() {
    let mut row = adopted_instance(
        "broken",
        "arca",
        "postgres-autopg",
        "192.0.2.10",
        "5432",
        "uuid-b",
    );
    row["config"]["user.arca.address"] = json!("192.0.2.99");
    let rig = Rig::with(
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![row],
            networks: vec![],
            pools: vec![],
        },
        vec![PROJECT.to_string(), "arca".into()],
        Some("192.0.2.0/24"),
    );
    let listed = rig.call("service_list", json!({})).expect("list");
    let rows = listed["services"].as_array().expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["state"], "failed");
    assert!(
        rig.entries()
            .iter()
            .all(|entry| entry.id.0 != "service-broken")
    );
    assert_eq!(
        rig.get("/1.0/instances/broken?project=arca").unwrap()["config"]["user.arca.address"],
        "192.0.2.99"
    );
}

#[test]
fn service_list_and_status_and_logs_work_on_an_adopted_record() {
    let rig = Rig::with(
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![adopted_instance(
                "svc1",
                "arca",
                "postgres-autopg",
                "192.0.2.10",
                "5432",
                "uuid-svc1",
            )],
            networks: vec![],
            pools: vec![],
        },
        vec![PROJECT.to_string(), "arca".into()],
        Some("192.0.2.0/24"),
    );
    rig.fake
        .set_console_log("arca", "svc1", "line one\nline two\n");
    rig.call("service_list", json!({})).expect("adopt");
    let logs = rig
        .call("service_logs", json!({"name":"svc1","lines":2}))
        .expect("logs");
    assert_eq!(logs["lines"], json!(["line one", "line two"]));
}

#[test]
fn service_logs_redacts_stored_secrets_rendered_environment_and_credentials() {
    let rig = Rig::new();
    rig.template(
        "omni",
        json!({
            "name":"omni","kind":"oci","image":"ghcr.io/automagik-si/omni-api",
            "digest":DIGEST,"ports":[8882],
            "env":{"OMNI_API_KEY":"{{secret:OMNI_API_KEY}}"},
            "secrets":["OMNI_API_KEY"],"generate":{"OMNI_API_KEY":"omni_sk_"},
            "publishable":false
        }),
    );
    rig.call("service_create", json!({"name":"omni","template":"omni"}))
        .expect("create");
    let key = rig
        .get(&format!("/1.0/instances/omni?project={PROJECT}"))
        .expect("instance")["config"]["environment.OMNI_API_KEY"]
        .as_str()
        .expect("key")
        .to_string();
    let log = format!(
        "{key}\nauthorization: Bearer abcdefghijklmnopqrstuvwxyz\ntskey-abc123\npostgresql://user:pw123456@db/app\napi_key=supersecret\nomni_sk_12***"
    );
    rig.fake.set_console_log(PROJECT, "omni", &log);
    let lines = rig
        .call("service_logs", json!({"name":"omni","lines":50}))
        .expect("logs")["lines"]
        .clone();
    let text = lines.to_string();
    assert!(!text.contains(&key));
    assert!(!text.contains("abcdefghijklmnopqrstuvwxyz"));
    assert!(!text.contains("tskey-abc123"));
    assert!(!text.contains("pw123456"));
    assert!(text.contains("[secret]"));
}

#[test]
fn service_logs_is_untrusted_and_has_a_real_cut_that_names_the_truncation() {
    use arca_api::verb;
    let logs = verb("service_logs").expect("logs");
    assert!(logs.untrusted);
    let mut lines = (0..205).map(|i| format!("{i}")).collect::<Vec<_>>();
    lines[0] = "x".repeat(2100);
    let (cut, truncated) = arca_api::verbs::cut_service_logs(&json!({"lines": lines}))
        .map(|cut| (cut, true))
        .unwrap_or((json!({}), false));
    assert!(truncated);
    let out = cut["lines"].as_array().expect("lines");
    assert_eq!(out.len(), 200);
    assert!(out[0].as_str().unwrap().chars().count() <= 2000);
}

#[test]
fn refuses_to_read_or_remove_an_unmarked_instance_before_sending_a_log_or_delete() {
    let rig = Rig::with(
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![hand_instance(
                "hand",
                PROJECT,
                json!({"eth0":{"type":"nic","ipv4.address":"192.0.2.10"}}),
            )],
            networks: vec![],
            pools: vec![],
        },
        vec![PROJECT.to_string()],
        Some("192.0.2.0/24"),
    );
    assert!(matches!(
        rig.call("service_logs", json!({"name":"hand"})),
        Err(CallError::Refused { code, .. }) if code == "not_found"
    ));
    assert!(matches!(
        rig.call("service_remove", json!({"name":"hand","approval":"x"})),
        Err(CallError::Refused { code, .. }) if code == "not_found"
    ));
    assert!(
        !rig.fake
            .requests()
            .iter()
            .any(|r| r.path.contains("console"))
    );
    assert!(!rig.fake.requests().iter().any(|r| r.method == "DELETE"));
}

#[test]
fn the_six_templates_and_nothing_else_load_and_are_pinned() {
    use std::path::Path;
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../templates");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name.ends_with(".json") && !name.starts_with("package"))
        .map(|name| name.trim_end_matches(".json").to_string())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "automagik",
            "automagik-server",
            "nats",
            "omni-api",
            "postgres-autopg",
            "workspace-vm"
        ]
    );
    for name in &names {
        let template = service::load_template(&dir, name).expect("the template loads");
        if template.kind == "oci" {
            assert!(
                template
                    .digest
                    .as_deref()
                    .is_some_and(|d| d.starts_with("sha256:"))
            );
        }
        assert!(!template.image.to_lowercase().contains("redis"));
    }
    let automagik = service::load_template(&dir, "automagik").unwrap();
    assert_eq!(
        automagik.commit.as_deref(),
        Some("bc0e4cf5465cae4fbeb454fbb089249bfeb3e189")
    );
    assert!(
        automagik
            .description
            .as_deref()
            .unwrap()
            .contains("templates/automagik.build.sh")
    );
    assert!(dir.join("automagik.build.sh").is_file());
    assert!(automagik.publishable);
    assert_eq!(automagik.dash_port, Some(3080));
    let nats = service::load_template(&dir, "nats").unwrap();
    assert!(
        nats.entrypoint
            .as_ref()
            .unwrap()
            .iter()
            .any(|v| v == "--jetstream")
    );
    assert!(!nats.publishable);
    let pg = service::load_template(&dir, "postgres-autopg").unwrap();
    let entry = pg.entrypoint.as_ref().unwrap();
    assert!(
        entry
            .iter()
            .any(|v| v.get("file").and_then(Value::as_str) == Some("postgres-autopg.sh"))
    );
    let script = std::fs::read_to_string(dir.join("postgres-autopg.sh")).unwrap();
    assert!(script.contains("postgres") && script.contains("exec"));
}

#[test]
fn the_build_lockfile_pins_every_package_by_registry_integrity_and_runs_no_install_script() {
    let dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../templates/automagik-dsh");
    let pkg: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("package.json")).unwrap()).unwrap();
    let lock: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("package-lock.json")).unwrap())
            .unwrap();
    assert_eq!(pkg["dependencies"]["@deepseek-ai/dsh"], "0.2.1-rc.3");
    assert_eq!(pkg["dependencies"]["pnpm"], "11.28.5");
    for (path, package) in lock["packages"].as_object().unwrap() {
        if path.is_empty() {
            continue;
        }
        assert!(
            package["resolved"]
                .as_str()
                .is_some_and(|r| r.starts_with("https://registry.npmjs.org/"))
        );
        assert!(
            package["integrity"]
                .as_str()
                .is_some_and(|i| i.starts_with("sha512-"))
        );
    }
    assert!(lock["packages"].get("node_modules/pnpm").is_some());
}

fn owner_approval(key: &common::SkKey, name: &str) -> String {
    use arca_kernel::canon::{canonical_json, params_digest};
    let args = json!({"name": name});
    let statement = canonical_json(&json!({
        "host": "node1",
        "action": "delete",
        "target": name,
        "paramsDigest": params_digest(&args).expect("digest"),
        "issuedAt": "2026-10-30T12:00:00.000Z",
        "expiresAt": "2026-10-30T12:05:00.000Z",
        "nonce": "a".repeat(64)
    }))
    .expect("canonical");
    common::approve_statement(key, &statement)
}

fn stopped_service_template() -> Value {
    json!({
        "name":"pg","kind":"oci","image":"docker.io/library/pg","digest":DIGEST,
        "ports":[5432],
        "env":{"POSTGRES_PASSWORD":"{{secret:POSTGRES_PASSWORD}}"},
        "secrets":["POSTGRES_PASSWORD"],
        "publishable":false
    })
}

#[test]
fn an_approved_action_proposal_runs_the_real_service_remove_handler_once() {
    let key = common::SkKey::from_seed(9);
    let rig = Rig::with_key(
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![],
            networks: vec![],
            pools: vec![],
        },
        vec![PROJECT.to_string()],
        Some("192.0.2.0/24"),
        Some(&key),
    );
    rig.template("pg", stopped_service_template());
    rig.call("service_create", json!({"name":"pg","template":"pg"}))
        .expect("create");
    assert!(
        rig.get(&format!("/1.0/instances/pg?project={PROJECT}"))
            .is_some()
    );

    let proposal = rig
        .kernel
        .lock()
        .expect("kernel")
        .propose_action(
            arca::action_proposal::ActionKind::ServiceRemove,
            json!({ "name": "pg" }),
            NOW,
            None,
        )
        .expect("the proposal is inert");
    let statement = arca::handlers::action_statement(
        "node1",
        &proposal.id,
        "service_remove",
        &proposal.args_digest,
        NOW,
        &"7".repeat(32),
    )
    .expect("the statement");
    let approval = common::approve_statement(&key, &statement);
    let approved = {
        let mut kernel = rig.kernel.lock().expect("kernel");
        arca::handlers::approve_action(
            &mut kernel,
            &json!({ "plan": proposal.id, "approval": approval }),
            NOW,
        )
        .expect("the owner's approval verifies")
    };
    let executor = arca::handlers::ActionExecutor::new(
        rig.handlers
            .iter()
            .map(|(verb, handler)| (*verb, Arc::clone(handler))),
    );
    let result = executor
        .run(&approved.action, &approved.args, &approved.grant)
        .expect("the real service_remove handler runs");
    assert_eq!(result, json!({"removed": true}));
    assert!(
        rig.get(&format!("/1.0/instances/pg?project={PROJECT}"))
            .is_none(),
        "the approved action removed the instance"
    );
    assert!(
        rig.entries().iter().all(|entry| entry.id.0 != "service-pg"),
        "the desired entry is gone"
    );

    let replay = rig
        .kernel
        .lock()
        .expect("kernel")
        .approve_action(&proposal.id, &approval, NOW);
    assert!(matches!(
        replay,
        Err(arca::kernel::ActionApprovalError::NoSuch)
    ));
}

#[test]
fn service_remove_is_refused_without_a_valid_approval_and_removes_with_one() {
    let key = common::SkKey::from_seed(9);
    let rig = Rig::with_key(
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![],
            networks: vec![],
            pools: vec![],
        },
        vec![PROJECT.to_string()],
        Some("192.0.2.0/24"),
        Some(&key),
    );
    rig.template("pg", stopped_service_template());
    let created = rig
        .call("service_create", json!({"name":"pg","template":"pg"}))
        .unwrap();
    assert_eq!(created["state"], "stopped");
    assert!(matches!(
        rig.call(
            "service_remove",
            json!({"name":"pg","approval":"not-an-approval"})
        ),
        Err(CallError::Refused { .. })
    ));
    assert!(
        rig.get(&format!("/1.0/instances/pg?project={PROJECT}"))
            .is_some()
    );
    let secrets = rig.dir.join("secrets/service-pg");
    fs::create_dir_all(&secrets).unwrap();
    fs::write(secrets.join("password"), "removed-on-success").unwrap();
    let approval = owner_approval(&key, "pg");
    let removed = rig
        .call("service_remove", json!({"name":"pg","approval":approval}))
        .expect("remove");
    assert_eq!(removed, json!({"removed":true}));
    assert!(!secrets.exists());
    assert!(
        rig.get(&format!("/1.0/instances/pg?project={PROJECT}"))
            .is_none()
    );
    assert!(
        rig.get(&format!(
            "/1.0/network-acls/{PROJECT}-svc-pg?project=default"
        ))
        .is_none()
    );
    assert!(rig.entries().iter().all(|entry| entry.id.0 != "service-pg"));
}

#[test]
fn a_removal_that_fails_after_the_approval_was_spent_says_so() {
    let key = common::SkKey::from_seed(9);
    let rig = Rig::with_key(
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![],
            networks: vec![],
            pools: vec![],
        },
        vec![PROJECT.into()],
        Some("192.0.2.0/24"),
        Some(&key),
    );
    rig.template("pg", stopped_service_template());
    rig.call("service_create", json!({"name":"pg","template":"pg"}))
        .unwrap();
    let secrets = rig.dir.join("secrets/service-pg");
    fs::create_dir_all(&secrets).unwrap();
    fs::write(secrets.join("password"), "kept-secret").unwrap();
    let acl_path = format!("/1.0/network-acls/{PROJECT}-svc-pg?project=default");
    rig.fake.set_reject_delete(&format!("{PROJECT}-svc-pg"));
    let approval = owner_approval(&key, "pg");
    let error = rig
        .call("service_remove", json!({"name":"pg","approval":approval}))
        .unwrap_err();
    assert!(
        matches!(error, CallError::Refused { code, message } if code == "refused" && message == "remove service pg: partial removal (instance removed; ACL remains); the owner's approval is spent, so another try needs a new one")
    );
    assert!(
        rig.get(&format!("/1.0/instances/pg?project={PROJECT}"))
            .is_none()
    );
    assert!(rig.get(&acl_path).is_some());
    assert_eq!(
        fs::read_to_string(secrets.join("password")).unwrap(),
        "kept-secret"
    );
    assert!(
        rig.fake
            .requests()
            .iter()
            .any(|request| request.method == "DELETE"
                && request.path == acl_path
                && request.status == 400)
    );
    assert!(matches!(
        rig.kernel.lock().unwrap().check_approval(
            arca_approval::ArcaAction::Delete,
            "pg",
            &json!({"name":"pg"}),
            &approval,
            NOW
        ),
        Err(arca_approval::ApprovalRefusal::Replayed)
    ));
}

#[test]
fn an_instance_only_delete_rejection_is_a_partial_refusal_and_keeps_secrets() {
    let key = common::SkKey::from_seed(9);
    let rig = Rig::with_key(
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![],
            networks: vec![],
            pools: vec![],
        },
        vec![PROJECT.into()],
        Some("192.0.2.0/24"),
        Some(&key),
    );
    rig.template("pg", stopped_service_template());
    rig.call("service_create", json!({"name":"pg","template":"pg"}))
        .unwrap();
    let secrets = rig.dir.join("secrets/service-pg");
    fs::create_dir_all(&secrets).unwrap();
    fs::write(secrets.join("password"), "kept-secret").unwrap();
    rig.fake.set_reject_delete("pg");
    let approval = owner_approval(&key, "pg");
    let error = rig
        .call("service_remove", json!({"name":"pg","approval":approval}))
        .unwrap_err();
    assert!(
        matches!(error, CallError::Refused { code, message } if code == "refused" && message == "remove service pg: partial removal (instance remains; ACL removed); the owner's approval is spent, so another try needs a new one")
    );
    assert!(
        rig.get(&format!("/1.0/instances/pg?project={PROJECT}"))
            .is_some()
    );
    assert!(
        rig.get(&format!(
            "/1.0/network-acls/{PROJECT}-svc-pg?project=default"
        ))
        .is_none()
    );
    assert_eq!(
        fs::read_to_string(secrets.join("password")).unwrap(),
        "kept-secret"
    );
    assert!(
        rig.fake
            .requests()
            .iter()
            .any(|request| request.method == "DELETE"
                && request.path == format!("/1.0/instances/pg?project={PROJECT}")
                && request.status == 400)
    );
    assert!(matches!(
        rig.kernel.lock().unwrap().check_approval(
            arca_approval::ArcaAction::Delete,
            "pg",
            &json!({"name":"pg"}),
            &approval,
            NOW
        ),
        Err(arca_approval::ApprovalRefusal::Replayed)
    ));
}

#[test]
fn service_remove_holds_the_kernel_lock_until_its_plan_is_run_once() {
    use std::sync::mpsc;
    use std::time::Duration;
    let key = common::SkKey::from_seed(9);
    let (entered, at_retirement) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let rig = Rig::with_key_and_pause(
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![],
            networks: vec![],
            pools: vec![],
        },
        vec![PROJECT.into()],
        Some("192.0.2.0/24"),
        Some(&key),
        Some(RemovalPause {
            entered,
            release: released,
        }),
    );
    rig.template("pg", stopped_service_template());
    rig.call("service_create", json!({"name":"pg","template":"pg"}))
        .unwrap();
    let handler = Arc::clone(rig.handlers.get("service_remove").unwrap());
    let approval = owner_approval(&key, "pg");
    let remove = std::thread::spawn(move || {
        handler.call(
            json!({"name":"pg","approval":approval}),
            &Caller {
                token_id: "doctor".into(),
                scope: Scope::Doctor,
                tools: None,
            },
        )
    });
    let plan = at_retirement.recv_timeout(Duration::from_secs(5)).unwrap();
    // The pending plan exists, but the service call still owns the kernel lock.
    assert!(matches!(
        rig.kernel.try_lock(),
        Err(std::sync::TryLockError::WouldBlock)
    ));
    let statement =
        arca::handlers::plan_statement("node1", &plan.to_hex(), NOW, &"b".repeat(64)).unwrap();
    let proof = common::approve_statement(&key, &statement);
    let handler =
        arca::handlers::plan_handlers(Arc::clone(&rig.kernel), Arc::new(|| NOW), None, None)
            .into_iter()
            .find(|(name, _)| *name == "approval_submit")
            .unwrap()
            .1;
    let (started, waiting) = mpsc::channel();
    let (finished, done) = mpsc::channel();
    let approve = std::thread::spawn(move || {
        started.send(()).unwrap();
        let result = handler.call(
            json!({"plan":plan.to_hex(),"approval":proof}),
            &Caller {
                token_id: "doctor".into(),
                scope: Scope::Doctor,
                tools: None,
            },
        );
        finished.send(result).unwrap();
    });
    waiting.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(done.try_recv(), Err(mpsc::TryRecvError::Empty)));
    release.send(()).unwrap();
    assert_eq!(remove.join().unwrap().unwrap(), json!({"removed":true}));
    assert!(
        matches!(done.recv_timeout(Duration::from_secs(5)).unwrap(), Err(CallError::Refused { code, .. }) if code == "NoSuchPlan")
    );
    approve.join().unwrap();
    assert_eq!(
        rig.fake
            .writes()
            .iter()
            .filter(|write| write.method == "DELETE"
                && write.path == format!("/1.0/instances/pg?project={PROJECT}"))
            .count(),
        1
    );
    assert_eq!(
        rig.fake
            .writes()
            .iter()
            .filter(|write| write.method == "DELETE"
                && write.path == format!("/1.0/network-acls/{PROJECT}-svc-pg?project=default"))
            .count(),
        1
    );
    assert!(rig.kernel.lock().unwrap().pending_plans().is_empty());
}

#[test]
fn service_logs_reads_a_system_services_console_without_a_template_mark() {
    let rig = Rig::new();
    rig.template(
        "dash",
        json!({
            "name":"dash","kind":"system","image":"local:dash-image",
            "ports":[],"env":{},"publishable":false
        }),
    );
    rig.fake.add_image(PROJECT, "dash-image");
    rig.call("service_create", json!({"name":"dash","template":"dash"}))
        .expect("system service");
    rig.edit(
        "PATCH",
        &format!("/1.0/instances/dash?project={PROJECT}"),
        json!({"config":{"user.arca.template":null}}),
    );
    rig.fake.set_console_log(PROJECT, "dash", "system line\n");
    let logs = rig
        .call("service_logs", json!({"name":"dash"}))
        .expect("logs");
    assert_eq!(logs["lines"], json!(["system line"]));
    assert!(
        rig.fake
            .requests()
            .iter()
            .any(|request| request.method == "GET"
                && request.path
                    == format!("/1.0/instances/dash/console?project={PROJECT}&type=log"))
    );
}

#[test]
fn after_a_rollback_that_lost_the_secrets_service_logs_still_redacts_the_environment_values_rendered_from_them()
 {
    let rig = Rig::with(
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![adopted_instance(
                "svc1",
                "arca",
                "postgres-autopg",
                "192.0.2.10",
                "5432",
                "uuid-svc1",
            )],
            networks: vec![],
            pools: vec![],
        },
        vec![PROJECT.to_string(), "arca".into()],
        Some("192.0.2.0/24"),
    );
    rig.fake
        .set_console_log("arca", "svc1", "password redacted-value in the log");
    let logs = rig
        .call("service_logs", json!({"name":"svc1"}))
        .expect("logs");
    let text = logs["lines"].to_string();
    assert!(!text.contains("redacted-value"));
    assert!(text.contains("[secret]"));
}

#[test]
fn template_images_are_pinned_by_digest_and_the_automagik_build_by_commit() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../templates");
    for name in ["nats", "postgres-autopg", "omni-api"] {
        let template = service::load_template(&dir, name).unwrap();
        assert!(
            template
                .digest
                .as_deref()
                .is_some_and(|d| d.starts_with("sha256:"))
        );
    }
    let automagik = service::load_template(&dir, "automagik").unwrap();
    assert_eq!(
        automagik.commit.as_deref(),
        Some("bc0e4cf5465cae4fbeb454fbb089249bfeb3e189")
    );
}

#[test]
fn template_entrypoints_nats_jetstream_and_autopg_postgres() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../templates");
    let nats = service::load_template(&dir, "nats").unwrap();
    assert!(
        nats.entrypoint
            .as_ref()
            .unwrap()
            .iter()
            .any(|v| v == "--jetstream")
    );
    let pg = service::load_template(&dir, "postgres-autopg").unwrap();
    assert!(
        pg.entrypoint
            .as_ref()
            .unwrap()
            .iter()
            .any(|v| v.get("file").and_then(Value::as_str) == Some("postgres-autopg.sh"))
    );
    let script = std::fs::read_to_string(dir.join("postgres-autopg.sh")).unwrap();
    assert!(script.contains("postgres") && script.contains("exec"));
}

#[test]
fn template_publishability_automagik_alone_with_its_dash_port() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../templates");
    let automagik = service::load_template(&dir, "automagik").unwrap();
    assert!(automagik.publishable);
    assert_eq!(automagik.dash_port, Some(3080));
    for name in ["nats", "postgres-autopg", "omni-api"] {
        assert!(!service::load_template(&dir, name).unwrap().publishable);
    }
}

#[test]
fn service_remove_deletes_a_running_service_with_owner_approval() {
    let key = common::SkKey::from_seed(9);
    let rig = Rig::with_key(
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![],
            networks: vec![],
            pools: vec![],
        },
        vec![PROJECT.to_string()],
        Some("192.0.2.0/24"),
        Some(&key),
    );
    rig.template("nats", nats_template());
    rig.call("service_create", json!({"name":"nats","template":"nats"}))
        .expect("running service");
    let approval = owner_approval(&key, "nats");
    assert_eq!(
        rig.call("service_remove", json!({"name":"nats","approval":approval}))
            .expect("remove"),
        json!({"removed":true})
    );
    assert!(
        rig.get(&format!("/1.0/instances/nats?project={PROJECT}"))
            .is_none()
    );
}

#[path = "services/approval_requests.rs"]
mod approval_requests;
#[path = "services/omni.rs"]
mod omni;
#[path = "services/wiring.rs"]
mod wiring;

#[test]
fn runtime_template_fields_are_strict_and_old_documents_keep_their_defaults() {
    let rig = Rig::new();
    for fields in [
        json!({"limits":{}}),
        json!({"limits":{"cpu":"1,2"}}),
        json!({"limits":{"cpu":" 4"}}),
        json!({"limits":{"cpu":"{4}"}}),
        json!({"limits":{"cpu":4}}),
        json!({"limits":{"cpu":"0"}}),
        json!({"limits":{"memory":"4 GiB"}}),
        json!({"limits":{"memory":"4GiB,8GiB"}}),
        json!({"limits":{"memory":"{{memory}}"}}),
        json!({"limits":{"memory":""}}),
        json!({"limits":{"memory":"0GiB"}}),
        json!({"limits":{"other":"4"}}),
        json!({"data":{"path":"data"}}),
        json!({"data":{"path":"/"}}),
        json!({"data":{"path":"//"}}),
        json!({"data":{"path":"/."}}),
        json!({"data":{"path":"/./"}}),
        json!({"data":{"path": format!("/{}", "a".repeat(255))}}),
        json!({"data":{"path":"/data/../etc"}}),
        json!({"data":{"path":"/data\n"}}),
        json!({"data":{"path":"/data","uid":-1}}),
        json!({"data":{"path":"/data","uid":4294967296_u64}}),
        json!({"data":{"path":"/data","uid":"1000"}}),
        json!({"data":{"path":"/data","gid":-1}}),
        json!({"data":{"path":"/data","size":"{{size}}"}}),
        json!({"data":{"path":"/data","size":"0GiB"}}),
        json!({"data":{"path":"/data","other":true}}),
        json!({"lan":{"port":3081}}),
        json!({"lan":{"port":0}}),
        json!({"lan":{"port":4222,"other":true}}),
    ] {
        let mut template = nats_template();
        template
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        rig.template("nats", template);
        assert!(
            service::load_template(&rig.dir.join("templates"), "nats").is_err(),
            "{fields}"
        );
    }
    for fields in [
        json!({"limits":{"cpu":"4"}}),
        json!({"limits":{"memory":"4GiB"}}),
        json!({"data":{"path":"/data/file..name","size":"8GiB","uid":0,"gid":4294967295_u64}}),
        json!({"data":{"path": format!("/{}", "a".repeat(254))}}),
        json!({"lan":{"port":4222}}),
    ] {
        let mut template = nats_template();
        template
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        rig.template("nats", template);
        service::load_template(&rig.dir.join("templates"), "nats").unwrap();
    }
    rig.template("nats", nats_template());
    let old = service::load_template(&rig.dir.join("templates"), "nats").unwrap();
    assert!(old.limits.is_none() && old.data.is_none() && old.lan.is_none() && !old.autostart);
    rig.call("service_create", json!({"name":"nats","template":"nats"}))
        .unwrap();
    let mut document = serde_json::to_value(rig.service_config("nats")).unwrap();
    for key in ["limits", "autostart", "data", "lan"] {
        document.as_object_mut().unwrap().remove(key);
    }
    let old: service::ServiceConfig = serde_json::from_value(document).unwrap();
    assert!(old.limits.is_none() && old.data.is_none() && old.lan.is_none() && !old.autostart);
    let instance = rig
        .get(&format!("/1.0/instances/nats?project={PROJECT}"))
        .unwrap();
    assert_eq!(instance["devices"].as_object().unwrap().len(), 2);
    for key in ["limits.cpu", "limits.memory", "boot.autostart"] {
        assert!(instance["config"].get(key).is_none());
    }
}

#[test]
fn automagik_server_runtime_shape_and_owned_volume_survive_retry_and_are_removed_together() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../templates");
    let template = service::load_template(&dir, "automagik-server").unwrap();
    assert_eq!(template.kind, "oci");
    assert_eq!(template.image, "ghcr.io/automagik-si/automagik-server");
    assert_eq!(
        template.digest.as_deref(),
        Some("sha256:671cdd101bc48863974c22b8331c4e390bbfaca3de4d2cd7f51491068105e450")
    );
    assert_eq!(
        template.entrypoint,
        Some(vec![
            json!("/nodejs/bin/node"),
            json!("/opt/automagik/entrypoint.mjs")
        ])
    );
    assert_eq!(template.ports, vec![3081]);
    let data = template.data.unwrap();
    assert_eq!(data.path, "/data");
    assert_eq!(
        (data.uid, data.gid, data.size),
        (Some(1000), Some(1000), None)
    );
    let limits = template.limits.unwrap();
    assert_eq!(limits.cpu.as_deref(), Some("4"));
    assert_eq!(limits.memory.as_deref(), Some("4GiB"));
    assert!(template.autostart);
    assert_eq!(template.lan.unwrap().port, 3081);
    assert!(!template.publishable);

    for size in [None, Some("8GiB")] {
        let key = common::SkKey::from_seed(9);
        let rig = Rig::with_key(
            FakeState {
                server: json!({}),
                projects: vec![],
                instances: vec![],
                networks: vec![],
                pools: vec![("pool1".into(), json!({}))],
            },
            vec![PROJECT.into()],
            Some("192.0.2.0/24"),
            Some(&key),
        );
        let mut value: Value =
            serde_json::from_str(&fs::read_to_string(dir.join("automagik-server.json")).unwrap())
                .unwrap();
        if let Some(size) = size {
            value["data"]["size"] = json!(size);
        }
        rig.template("automagik-server", value);
        rig.call(
            "service_create",
            json!({"name":"server","template":"automagik-server"}),
        )
        .unwrap();
        let instance_path = format!("/1.0/instances/server?project={PROJECT}");
        let volume_path =
            format!("/1.0/storage-pools/pool1/volumes/custom/server-data?project={PROJECT}");
        let acl_path = format!("/1.0/network-acls/{PROJECT}-svc-server?project=default");
        let instance = rig.get(&instance_path).unwrap();
        assert_eq!(instance["config"]["limits.cpu"], "4");
        assert_eq!(instance["config"]["limits.memory"], "4GiB");
        assert_eq!(instance["config"]["boot.autostart"], "true");
        assert_eq!(
            instance["config"]["oci.entrypoint"],
            "/nodejs/bin/node /opt/automagik/entrypoint.mjs"
        );
        let disk = json!({"type":"disk", "path":"/data", "pool":"pool1", "source":"server-data"});
        assert_eq!(instance["devices"]["data"], disk);
        assert!(
            instance["devices"]["data"].get("size").is_none(),
            "the data disk carries no size"
        );
        assert_eq!(
            instance["devices"]["web"],
            json!({"type":"proxy", "listen":"tcp:0.0.0.0:3081", "connect":"tcp:127.0.0.1:3081"})
        );
        let volume = rig.get(&volume_path).unwrap();
        let mut config = json!({"initial.uid":"1000", "initial.gid":"1000", "user.arcakernel.entry":"service-server"});
        if let Some(size) = size {
            config["size"] = json!(size);
        }
        assert_eq!(volume["config"], config);
        let writes = rig.fake.writes().len();
        rig.apply(&rig.service_config("server")).unwrap();
        assert_eq!(
            rig.fake.writes().len(),
            writes,
            "second apply writes nothing"
        );
        let approval = owner_approval(&key, "server");
        assert_eq!(
            rig.call(
                "service_remove",
                json!({"name":"server", "approval":approval})
            )
            .unwrap(),
            json!({"removed":true})
        );
        for path in [&instance_path, &volume_path, &acl_path] {
            assert!(rig.get(path).is_none(), "{path} was removed");
        }
    }
}

#[test]
fn apply_refuses_a_pre_existing_unmarked_data_volume_and_leaves_it() {
    let rig = Rig::with(
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![],
            networks: vec![],
            pools: vec![("pool1".into(), json!({}))],
        },
        vec![PROJECT.into()],
        Some("192.0.2.0/24"),
    );
    rig.template("nats", nats_template());
    rig.call("service_create", json!({"name":"nats","template":"nats"}))
        .unwrap();
    let mut config = rig.service_config("nats");
    let volume_path = format!(
        "/1.0/storage-pools/{}/volumes/custom/nats-data?project={PROJECT}",
        config.pool
    );
    rig.edit(
        "POST",
        &format!(
            "/1.0/storage-pools/{}/volumes/custom?project={PROJECT}",
            config.pool
        ),
        json!({"name":"nats-data","config":{"user.note":"created by hand"}}),
    );
    config.data = Some(service::Data {
        path: "/data".into(),
        uid: None,
        gid: None,
        size: None,
    });
    let error = rig.apply(&config).unwrap_err();
    assert!(
        matches!(error, arca_kernel::plugin::PluginError::Refused(_)),
        "the unmarked volume is refused, got {error}"
    );
    let volume = rig.get(&volume_path).expect("the volume still exists");
    assert_eq!(volume["config"]["user.note"], "created by hand");
    assert!(
        volume["config"].get(KERNEL_ENTRY_KEY).is_none(),
        "the volume was never adopted"
    );
}

#[test]
fn a_netproject_read_failure_does_not_refuse_a_service_create() {
    let rig = Rig::new();
    rig.template("nats", nats_template());
    rig.call("service_create", json!({"name":"nats","template":"nats"}))
        .expect("the first service installs the project entry");
    rig.fail_observe("net-arcak-svc");
    rig.call("service_create", json!({"name":"other","template":"nats"}))
        .expect("the service applies although the project entry cannot be read");
    assert!(
        rig.get(&format!("/1.0/instances/other?project={PROJECT}"))
            .is_some()
    );
}

#[test]
fn a_netproject_read_failure_does_not_refuse_a_service_remove_or_skip_secret_cleanup() {
    let key = common::SkKey::from_seed(9);
    let rig = Rig::with_key(
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![],
            networks: vec![],
            pools: vec![],
        },
        vec![PROJECT.into()],
        Some("192.0.2.0/24"),
        Some(&key),
    );
    rig.template("nats", nats_template());
    rig.call("service_create", json!({"name":"nats","template":"nats"}))
        .expect("the service exists");
    let secrets = rig.dir.join("secrets").join("service-nats");
    fs::create_dir_all(&secrets).expect("secret directory");
    fs::write(secrets.join("marker"), b"kept").expect("secret file");
    rig.fail_observe("net-arcak-svc");

    let approval = owner_approval(&key, "nats");
    assert_eq!(
        rig.call("service_remove", json!({"name":"nats","approval":approval}))
            .expect("removal ignores the project entry's read failure"),
        json!({"removed":true})
    );
    assert!(!secrets.exists(), "the secret directory is cleaned up");
    assert!(
        rig.get(&format!("/1.0/instances/nats?project={PROJECT}"))
            .is_none(),
        "the instance was deleted"
    );
}

#[test]
fn service_create_names_the_missing_entrypoint_script_and_how_it_gets_there() {
    let rig = Rig::new();
    rig.template(
        "jet",
        json!({
            "name":"jet","kind":"oci","image":"docker.io/library/jet","digest":DIGEST,
            "ports":[8080],"env":{},"publishable":false,
            "entrypoint":["/bin/sh","-ec",{"file":"jet.sh"},"jet"]
        }),
    );
    let Err(CallError::Refused { code, message }) =
        rig.call("service_create", json!({"name":"jet","template":"jet"}))
    else {
        panic!("a template without its script is refused");
    };
    assert_eq!(code, "refused");
    let path = rig.dir.join("templates/jet.sh");
    assert!(
        message.starts_with(
            "template jet: its entrypoint script is not installed (templates/jet.sh does not exist). \
             The script belongs beside templates/jet.json in the state directory"
        ),
        "the refusal names the file it looked for and where it belongs: {message}"
    );
    assert!(
        message.contains("for a template Arca ships, `arca templates install --config <file>`"),
        "the one-step install is offered for shipped templates only: {message}"
    );
    assert!(
        !message.contains(&rig.dir.display().to_string()) && !message.contains("ships beside"),
        "no absolute state path, and no claim that this template is shipped: {message}"
    );
    assert!(
        rig.get(&format!("/1.0/instances/jet?project={PROJECT}"))
            .is_none()
            && rig
                .get(&format!(
                    "/1.0/network-acls/{PROJECT}-svc-jet?project=default"
                ))
                .is_none()
            && !rig
                .entries()
                .iter()
                .any(|entry| entry.id.0 == "service-jet"),
        "nothing was created"
    );
    // The script in place, the same call creates the service.
    fs::write(&path, "exec jet\n").expect("script");
    rig.call("service_create", json!({"name":"jet","template":"jet"}))
        .expect("create");
}

#[test]
fn an_entrypoint_script_is_a_plain_file_name_in_the_template_directory() {
    let rig = Rig::new();
    fs::write(rig.dir.join("outside.sh"), "exec evil\n").expect("script");
    for file in ["../outside.sh", "/etc/hostname", ".hidden", ""] {
        rig.template(
            "jet",
            json!({
                "name":"jet","kind":"oci","image":"docker.io/library/jet","digest":DIGEST,
                "ports":[8080],"env":{},"publishable":false,
                "entrypoint":["/bin/sh","-ec",{"file":file}]
            }),
        );
        assert!(
            matches!(
                rig.call("service_create", json!({"name":"jet","template":"jet"})),
                Err(CallError::Refused { message, .. })
                    if message.starts_with("entrypoint: invalid file (a plain file name in the templates directory")
            ),
            "{file:?} is refused"
        );
    }
}

#[test]
fn templates_install_puts_every_shipped_template_and_its_script_in_place_in_one_step() {
    let rig = Rig::new();
    let dir = rig.dir.join("templates");
    let shipped = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../templates");
    let report = arca::cmd_templates::install_into(&dir, false).expect("install");
    assert_eq!(report["dir"], json!(dir.display().to_string()));
    assert_eq!(report["unchanged"], json!([]));
    assert_eq!(report["kept"], json!([]));
    let installed: Vec<&str> = report["installed"]
        .as_array()
        .expect("installed")
        .iter()
        .map(|name| name.as_str().expect("a name"))
        .collect();
    // Every template the repository ships is installed, and each loads with its entrypoint script readable.
    let mut templates = 0;
    for entry in fs::read_dir(&shipped).expect("templates/") {
        let name = entry
            .expect("entry")
            .file_name()
            .into_string()
            .expect("name");
        let Some(template) = name.strip_suffix(".json") else {
            continue;
        };
        templates += 1;
        assert!(installed.contains(&name.as_str()), "{name} is installed");
        assert_eq!(
            fs::read(dir.join(&name)).expect("installed"),
            fs::read(shipped.join(&name)).expect("shipped"),
            "{name} is the shipped file"
        );
        let loaded = service::load_template(&dir, template).expect("the installed template loads");
        service::entrypoint(&loaded, &dir).expect("its entrypoint script is installed with it");
    }
    assert_eq!(templates, 6);
    assert!(installed.contains(&"postgres-autopg.sh"));
    assert_eq!(
        fs::metadata(&dir).expect("dir").permissions().mode() & 0o777,
        0o700
    );

    // A second run changes nothing. A file that differs is kept, and replaced only when asked.
    let again = arca::cmd_templates::install_into(&dir, false).expect("again");
    assert_eq!(again["installed"], json!([]));
    assert_eq!(
        again["unchanged"].as_array().expect("unchanged").len(),
        installed.len()
    );
    fs::write(dir.join("nats.json"), "{}").expect("an edit");
    fs::remove_file(dir.join("postgres-autopg.sh")).expect("remove");
    fs::write(dir.join("mine.json"), "{}").expect("the owner's own template");
    let partial = arca::cmd_templates::install_into(&dir, false).expect("partial");
    assert_eq!(partial["installed"], json!(["postgres-autopg.sh"]));
    assert_eq!(partial["kept"], json!(["nats.json"]));
    assert_eq!(
        fs::read_to_string(dir.join("nats.json")).expect("kept"),
        "{}"
    );
    let replaced = arca::cmd_templates::install_into(&dir, true).expect("replace");
    assert_eq!(replaced["installed"], json!(["nats.json"]));
    assert_eq!(replaced["kept"], json!([]));
    service::load_template(&dir, "nats").expect("the shipped nats again");
    assert_eq!(
        fs::read_to_string(dir.join("mine.json")).expect("mine"),
        "{}"
    );
}

#[test]
fn service_create_refuses_a_local_image_the_service_project_does_not_hold_and_names_it() {
    let rig = Rig::new();
    rig.template(
        "dash",
        json!({
            "name":"dash","kind":"system","image":"local:dash-image",
            "ports":[],"env":{},"secrets":["TOKEN"],"generate":{"TOKEN":"test_"},"publishable":false
        }),
    );
    // The service project exists once a service was made in it. Another alias there does not stand in for the template's.
    rig.template("nats", nats_template());
    rig.call("service_create", json!({"name":"nats","template":"nats"}))
        .expect("nats");
    rig.fake.add_image(PROJECT, "other-image");
    let writes = |rig: &Rig| {
        rig.fake
            .requests()
            .iter()
            .filter(|request| request.method != "GET")
            .count()
    };
    let before = writes(&rig);
    let Err(CallError::Refused { code, message }) =
        rig.call("service_create", json!({"name":"dash","template":"dash"}))
    else {
        panic!("a missing local image is refused");
    };
    assert_eq!(code, "refused");
    assert!(
        message.starts_with(&format!(
            "template dash: its image dash-image is not on this node: no image in the service project {PROJECT} has that alias"
        )),
        "{message}"
    );
    assert!(
        rig.get(&format!(
            "/1.0/network-acls/{PROJECT}-svc-dash?project=default"
        ))
        .is_none()
            && !rig.dir.join("secrets/service-dash").exists()
            && !rig
                .entries()
                .iter()
                .any(|entry| entry.id.0 == "service-dash")
            && writes(&rig) == before,
        "nothing was written"
    );
    rig.fake.add_image(PROJECT, "dash-image");
    rig.call("service_create", json!({"name":"dash","template":"dash"}))
        .expect("the image in place, the service is created");
}

#[test]
fn a_failed_create_of_a_data_template_names_the_volume_left_for_the_owners_approval() {
    let rig = Rig::with(
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![],
            networks: vec![],
            pools: vec![("pool1".into(), json!({}))],
        },
        vec![PROJECT.into()],
        Some("192.0.2.0/24"),
    );
    let mut template = nats_template();
    template["data"] = json!({"path":"/data"});
    rig.template("nats", template);
    rig.fake.set_reject_config_on_post("/1.0/instances");
    let Err(CallError::Refused { message, .. }) =
        rig.call("service_create", json!({"name":"nats","template":"nats"}))
    else {
        panic!("a failed create is refused");
    };
    assert_eq!(
        message,
        "create service nats from template nats: the create did not land: Incus said \"config is not accepted on create\"; its data volume nats-data still exists, \
         waiting for the owner's approval to be deleted (plan_list shows the plan)"
    );
    assert!(
        rig.get(&format!(
            "/1.0/storage-pools/pool1/volumes/custom/nats-data?project={PROJECT}"
        ))
        .is_some(),
        "the volume the refusal names is there"
    );
    let plans = rig.kernel.lock().unwrap().pending_plans();
    assert_eq!(plans.len(), 1, "one delete plan waits");
    assert!(
        plans[0]
            .objects
            .iter()
            .any(|object| object.kind == IncusKind::Volume && object.name == "nats-data"),
        "and it holds the volume"
    );
}

#[test]
fn a_create_whose_outcome_is_not_known_does_not_claim_that_nothing_was_left_behind() {
    let rig = Rig::new();
    rig.template("nats", nats_template());
    *rig.unknown_create.lock().unwrap() = true;
    let Err(CallError::Refused { message, .. }) =
        rig.call("service_create", json!({"name":"nats","template":"nats"}))
    else {
        panic!("an unresolved create is refused");
    };
    assert_eq!(
        message,
        "create service nats from template nats: the outcome is not known; the intent stays open; \
         the cleanup did not complete (an earlier effect is unresolved; the step does not run), \
         so objects of this service may remain"
    );
    assert!(
        rig.get(&format!(
            "/1.0/network-acls/{PROJECT}-svc-nats?project=default"
        ))
        .is_some(),
        "nothing was reverted: the ACL the refusal warns about is still there"
    );
}
