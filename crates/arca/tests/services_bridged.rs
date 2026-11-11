//! Virtual-machine services on a bridged node, against a fake Incus. No live host is touched.
mod common;

use arca::{
    action_proposal::ActionKind,
    config::NodeConfig,
    handlers::{ActionExecutor, action_statement, approve_action, service_handlers},
    kernel::Kernel,
    service::{self, Service},
};
use arca_api::Scope;
use arca_approval::{ApprovalVerifier, FileNonces, OwnerKeys, PasskeyVerifier, SshSigVerifier};
use arca_incus::{FakeIncus, FakeState, IncusRead, UnixIncus};
use arca_kernel::canon::{canonical_json, params_digest};
use arca_kernel::identity::NodeKey;
use arca_kernel::ownership::KERNEL_ENTRY_KEY;
use arca_mcp::{CallError, Caller, ToolHandler};
use arca_netproject::{BRIDGE, BRIDGED_ENTRY, PROJECT};
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

struct Rig {
    dir: PathBuf,
    fake: FakeIncus,
    kernel: Arc<Mutex<Kernel>>,
    handlers: HashMap<&'static str, Arc<dyn ToolHandler>>,
    cfg: Arc<NodeConfig>,
}

impl Rig {
    /// A bridged node that allows `bridges` and keeps root disks on `vmpool`, with room for four of the test template.
    fn bridged(bridges: &[&str], key: Option<&common::SkKey>) -> Self {
        Self::with(bridged_kernel(bridges, roomy()), key, vec![])
    }

    /// A node with the managed service network, as every node before bridged mode.
    fn managed() -> Self {
        Self::with(managed_kernel(), None, vec![])
    }

    fn with(kernel_config: Value, key: Option<&common::SkKey>, instances: Vec<Value>) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "arca-bridged-{}-{}",
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
                instances,
                networks: vec![],
                pools: vec![],
            },
        )
        .expect("fake");
        fake.add_profile(
            "default",
            json!({"name":"default","config":{},"devices":{"root":{"type":"disk","pool":"pool1","path":"/"}}}),
        );
        let cfg = node_config(&dir, &fake, kernel_config);
        let verifier = match key {
            Some(key) => {
                let scratch = common::Scratch::new("bridged-owner");
                common::verifier_for(&scratch, &[key], "service")
            }
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
        plugins
            .add::<arca_netproject::BridgedProject>()
            .expect("bridged");
        plugins.add::<Service>().expect("service");
        let socket = fake.socket().to_path_buf();
        let secrets = dir.join("secrets");
        let kernel = Arc::new(Mutex::new(
            Kernel::open(
                &dir,
                NodeKey::load_or_create(&dir.join("node.key")).expect("key"),
                plugins,
                verifier,
                "node1".into(),
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
        let cfg = Arc::new(cfg);
        let handlers = service_handlers(kernel.clone(), Arc::new(|| NOW), cfg.clone())
            .into_iter()
            .collect();
        let rig = Self {
            dir,
            fake,
            kernel,
            handlers,
            cfg,
        };
        rig.template("ws", vm_template());
        rig.template("nats", nats_template());
        rig
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

    /// The same node after its config was edited to `kernel_config`: the service handlers over the same kernel and Incus.
    fn switched(
        &self,
        kernel_config: Value,
    ) -> (Arc<NodeConfig>, HashMap<&'static str, Arc<dyn ToolHandler>>) {
        let cfg = Arc::new(node_config(&self.dir, &self.fake, kernel_config));
        let handlers = service_handlers(self.kernel.clone(), Arc::new(|| NOW), cfg.clone())
            .into_iter()
            .collect();
        (cfg, handlers)
    }

    /// A write made by hand on the host, outside Arca.
    fn edit(&self, method: &str, path: &str, body: Value) {
        use std::io::{Read, Write};
        let mut stream =
            std::os::unix::net::UnixStream::connect(self.fake.socket()).expect("the fake");
        let body = body.to_string();
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .expect("the request");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("the answer");
        assert!(
            response.starts_with("HTTP/1.1 200") || response.starts_with("HTTP/1.1 202"),
            "an edit by hand failed: {}",
            response.lines().next().unwrap_or("")
        );
    }

    fn entries(&self) -> Vec<arca_kernel::desired::Entry> {
        self.kernel.lock().expect("kernel").entries().to_vec()
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

    fn entry_ids(&self) -> Vec<String> {
        let kernel = self.kernel.lock().expect("kernel");
        kernel.entries().iter().map(|e| e.id.0.clone()).collect()
    }

    /// Every request the fake has seen that touches a network, a network ACL or a profile, reads included.
    fn network_requests(&self) -> Vec<String> {
        self.fake
            .requests()
            .into_iter()
            .filter(|request| {
                request.path.contains("/1.0/networks")
                    || request.path.contains("/1.0/network-acls")
                    || request.path.contains(BRIDGE)
            })
            .map(|request| format!("{} {}", request.method, request.path))
            .collect()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn node_config(dir: &std::path::Path, fake: &FakeIncus, kernel_config: Value) -> NodeConfig {
    let cfg: NodeConfig = serde_json::from_value(json!({
        "stateDir": dir,
        "incus": {"socket": fake.socket()},
        "listen": {"unix": dir.join("arca.sock")},
        "tokens": {"hashDir": dir.join("tokens")},
        "kernel": kernel_config
    }))
    .expect("config");
    arca::config::check(&cfg).expect("the config passes its rules");
    cfg
}

fn roomy() -> Value {
    json!({ "virtualMachines": 4, "cpu": 8, "memory": "16GiB", "disk": "80GiB" })
}

fn bridged_kernel(bridges: &[&str], limits: Value) -> Value {
    json!({
        "projects": [PROJECT],
        "droiddProjects": ["other"],
        "bridged": { "bridges": bridges, "pool": "vmpool", "limits": limits }
    })
}

fn managed_kernel() -> Value {
    json!({
        "projects": [PROJECT],
        "droiddProjects": ["other"],
        "serviceCidr": "192.0.2.0/24"
    })
}

fn doctor() -> Caller {
    Caller {
        token_id: "doctor".into(),
        scope: Scope::Doctor,
        tools: None,
    }
}

fn vm_template() -> Value {
    json!({
        "name": "ws",
        "kind": "virtual-machine",
        "image": "images:debian/13",
        "limits": { "cpu": "2", "memory": "4GiB" },
        "rootSize": "20GiB",
        "autostart": true
    })
}

fn nats_template() -> Value {
    json!({
        "name": "nats", "kind": "oci", "image": "docker.io/library/nats", "digest": DIGEST,
        "ports": [4222], "env": {}, "publishable": false
    })
}

fn refusal(result: Result<Value, CallError>) -> String {
    match result {
        Err(CallError::Refused { code, message }) if code == "refused" => message,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn owner_approval(key: &common::SkKey, name: &str) -> String {
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

#[test]
fn a_vm_is_created_with_its_type_devices_and_limits_and_no_network_is_created_or_read() {
    let rig = Rig::bridged(&["br0", "br1"], None);
    let created = rig
        .call(
            "service_create",
            json!({"name":"ws1","template":"ws","bridge":"br1","mac":"02:AA:bb:00:00:01"}),
        )
        .expect("create");
    assert_eq!(
        created,
        json!({
            "name": "ws1", "template": "ws", "state": "running", "ipv4": "",
            "ports": [], "links": [], "published": null, "lanPort": null
        })
    );

    let instance = rig
        .get(&format!("/1.0/instances/ws1?project={PROJECT}"))
        .expect("the instance exists in Arca's own project");
    assert_eq!(instance["type"], "virtual-machine");
    assert_eq!(instance["profiles"], json!([]));
    assert_eq!(
        instance["source"],
        json!({
            "type": "image", "mode": "pull", "protocol": "simplestreams",
            "server": "https://images.linuxcontainers.org", "alias": "debian/13"
        })
    );
    assert_eq!(
        instance["devices"],
        json!({
            "root": { "type": "disk", "path": "/", "pool": "vmpool", "size": "20GiB" },
            "eth0": { "type": "nic", "name": "enp5s0", "nictype": "bridged", "parent": "br1", "hwaddr": "02:aa:bb:00:00:01" },
            "agent": { "type": "disk", "source": "agent:config" }
        }),
        "one root disk on the configured pool, one NIC on the host bridge and the agent's config drive: no network, address, ACL or filter key"
    );
    let config = instance["config"].as_object().expect("config");
    assert_eq!(config["limits.cpu"], "2");
    assert_eq!(config["limits.memory"], "4GiB");
    assert_eq!(config["boot.autostart"], "true");
    assert_eq!(config["user.arca.template"], "ws");
    assert_eq!(config[KERNEL_ENTRY_KEY], "service-ws1");
    let mut keys: Vec<&str> = config.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "agent.nic_config",
            "boot.autostart",
            "limits.cpu",
            "limits.memory",
            "user.arca.template",
            KERNEL_ENTRY_KEY,
            "volatile.uuid"
        ]
    );

    // The project is the bridged entry's: restricted, with the one key a NIC on a host bridge needs, and no bridge grant.
    let project = rig
        .get(&format!("/1.0/projects/{PROJECT}?project={PROJECT}"))
        .expect("project");
    assert_eq!(project["config"][KERNEL_ENTRY_KEY], BRIDGED_ENTRY);
    assert_eq!(project["config"]["restricted"], "true");
    assert_eq!(project["config"]["restricted.devices.nic"], "allow");
    assert_eq!(project["config"]["limits.containers"], "0");
    assert_eq!(project["config"]["limits.virtual-machines"], "4");
    assert_eq!(project["config"]["limits.cpu"], "8");
    assert_eq!(project["config"]["limits.memory"], "16GiB");
    assert_eq!(project["config"]["limits.disk"], "80GiB");
    assert!(
        project["config"]
            .get("restricted.networks.access")
            .is_none()
    );
    assert!(project["config"].get("restricted.devices.proxy").is_none());

    // Nothing asked Incus for a network, a network ACL or the managed bridge, and nothing wrote a profile.
    assert_eq!(rig.network_requests(), Vec::<String>::new());
    let writes: Vec<String> = rig
        .fake
        .writes()
        .into_iter()
        .map(|write| format!("{} {}", write.method, write.path))
        .collect();
    assert_eq!(writes.len(), 2, "one project and one instance: {writes:?}");
    assert!(writes[0].starts_with("POST /1.0/projects"), "{writes:?}");
    assert!(writes[1].starts_with("POST /1.0/instances"), "{writes:?}");
    let mut ids = rig.entry_ids();
    ids.sort();
    assert_eq!(ids, [BRIDGED_ENTRY, "service-ws1"]);

    // The record is a service record like a container's, with the VM's own part.
    let record: service::ServiceConfig = {
        let kernel = rig.kernel.lock().expect("kernel");
        let entry = kernel
            .entries()
            .iter()
            .find(|entry| entry.id.0 == "service-ws1")
            .expect("record");
        assert_eq!(entry.plugin.0, "arca.service");
        serde_json::from_value(entry.config.clone()).expect("record")
    };
    assert_eq!(record.bridge, "br1");
    assert_eq!(record.pool, "vmpool");
    let vm = record.vm.expect("a virtual machine");
    assert_eq!(vm.root_size, "20GiB");
    assert_eq!(vm.mac.as_deref(), Some("02:aa:bb:00:00:01"));

    // List, status and logs work.
    let listed = rig.call("service_list", json!({})).expect("list");
    assert_eq!(listed["services"], json!([created]));
    assert_eq!(
        rig.call("service_status", json!({"name":"ws1"}))
            .expect("status"),
        created
    );
    rig.fake
        .set_console_log(PROJECT, "ws1", "booted\nlogin: \n");
    assert_eq!(
        rig.call("service_logs", json!({"name":"ws1"}))
            .expect("logs"),
        json!({"lines": ["booted", "login:"]})
    );
    assert_eq!(rig.network_requests(), Vec::<String>::new());

    // A second create on the same node adds no second project and still no network.
    rig.call(
        "service_create",
        json!({"name":"ws2","template":"ws","bridge":"br0"}),
    )
    .expect("second create");
    let second = rig
        .get(&format!("/1.0/instances/ws2?project={PROJECT}"))
        .expect("second instance");
    assert_eq!(
        second["devices"]["eth0"],
        json!({ "type": "nic", "name": "enp5s0", "nictype": "bridged", "parent": "br0" }),
        "without a mac, Incus picks one"
    );
    assert_eq!(rig.fake.writes().len(), 3);
    assert_eq!(rig.network_requests(), Vec::<String>::new());
}

#[test]
fn a_bridge_outside_the_allowlist_and_every_other_bad_request_is_refused_before_any_write() {
    let rig = Rig::bridged(&["br0", "br1"], None);
    assert_eq!(
        refusal(rig.call(
            "service_create",
            json!({"name":"ws1","template":"ws","bridge":"br9"})
        )),
        "bridge br9 is not one of the bridges this node allows (kernel.bridged.bridges)"
    );
    // The managed service bridge is not a host bridge a request may name either.
    assert_eq!(
        refusal(rig.call(
            "service_create",
            json!({"name":"ws1","template":"ws","bridge":BRIDGE})
        )),
        format!(
            "bridge {BRIDGE} is not one of the bridges this node allows (kernel.bridged.bridges)"
        )
    );
    assert_eq!(
        refusal(rig.call("service_create", json!({"name":"ws1","template":"ws"}))),
        "this node allows more than one bridge: name one with bridge"
    );
    for bridge in [json!("br0,br1"), json!("../br0"), json!(""), json!(7)] {
        assert!(
            matches!(
                rig.call(
                    "service_create",
                    json!({"name":"ws1","template":"ws","bridge":bridge})
                ),
                Err(CallError::Invalid(_))
            ),
            "{bridge}"
        );
    }
    for mac in [
        json!("02:aa:bb:00:00"),
        json!("02-aa-bb-00-00-01"),
        json!("01:aa:bb:00:00:01"),
        json!("ff:ff:ff:ff:ff:ff"),
        json!("00:00:00:00:00:00"),
        json!("02:aa:bb:00:00:zz"),
        json!(1),
    ] {
        assert!(
            matches!(
                rig.call(
                    "service_create",
                    json!({"name":"ws1","template":"ws","bridge":"br0","mac":mac})
                ),
                Err(CallError::Invalid(_))
            ),
            "{mac}"
        );
    }
    assert_eq!(
        refusal(rig.call("service_create", json!({"name":"nats","template":"nats"}))),
        "this node is a bridged node: it runs virtual-machine templates only, and template nats is not one"
    );
    assert!(rig.fake.writes().is_empty(), "no refusal wrote anything");
    assert!(rig.entry_ids().is_empty(), "no refusal recorded anything");
    assert_eq!(rig.network_requests(), Vec::<String>::new());

    // A fixed MAC is held by one service.
    rig.call(
        "service_create",
        json!({"name":"ws1","template":"ws","bridge":"br0","mac":"02:aa:bb:00:00:01"}),
    )
    .expect("create");
    assert_eq!(
        refusal(rig.call(
            "service_create",
            json!({"name":"ws2","template":"ws","bridge":"br1","mac":"02:AA:BB:00:00:01"})
        )),
        "the MAC 02:aa:bb:00:00:01 is already held by another instance"
    );
    assert!(
        rig.get(&format!("/1.0/instances/ws2?project={PROJECT}"))
            .is_none()
    );
}

#[test]
fn a_node_that_allows_one_bridge_uses_it_when_the_request_names_none() {
    let rig = Rig::bridged(&["vmbr0"], None);
    rig.call("service_create", json!({"name":"ws1","template":"ws"}))
        .expect("create");
    let instance = rig
        .get(&format!("/1.0/instances/ws1?project={PROJECT}"))
        .expect("instance");
    assert_eq!(instance["devices"]["eth0"]["parent"], "vmbr0");
}

#[test]
fn the_verbs_that_need_the_service_network_refuse_a_bridged_vm_in_plain_words() {
    let rig = Rig::bridged(&["br0"], None);
    for name in ["ws1", "ws2"] {
        rig.call("service_create", json!({"name":name,"template":"ws"}))
            .expect("create");
    }
    let before = rig.fake.writes().len();
    for verb in ["service_connect", "service_disconnect"] {
        assert_eq!(
            refusal(rig.call(verb, json!({"from":"ws1","to":"ws2"}))),
            "service ws1 is a virtual machine on a host bridge that Arca does not manage: links need the service network and its per-service firewall rules, which it does not have",
            "{verb}"
        );
    }
    for (verb, args) in [
        (
            "service_publish",
            json!({"name":"ws1","reach":"tailnet","approval":"x"}),
        ),
        ("service_unpublish", json!({"name":"ws1"})),
    ] {
        assert_eq!(
            refusal(rig.call(verb, args)),
            "service ws1 is a virtual machine on a host bridge that Arca does not manage: it cannot be published from here; reach it on its bridge's own network",
            "{verb}"
        );
    }
    assert_eq!(
        refusal(rig.call(
            "service_secret_set",
            json!({"name":"ws1","key":"TOKEN","value":"a-long-enough-value"})
        )),
        "template does not declare that secret: this virtual machine's template declares none"
    );
    assert_eq!(rig.fake.writes().len(), before, "no refusal wrote anything");
    assert_eq!(rig.network_requests(), Vec::<String>::new());
}

#[test]
fn a_vm_template_is_refused_on_a_node_that_is_not_bridged_and_a_container_takes_no_bridge_or_mac() {
    let rig = Rig::managed();
    assert_eq!(
        refusal(rig.call(
            "service_create",
            json!({"name":"ws1","template":"ws","bridge":"br0"})
        )),
        "template ws is a virtual machine, and this node sets no virtual-machine caps (kernel.vmLimits, or kernel.bridged with its limits)"
    );
    for extra in [json!({"bridge":"br0"}), json!({"mac":"02:aa:bb:00:00:01"})] {
        let mut args = json!({"name":"nats","template":"nats"});
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert!(matches!(
            rig.call("service_create", args),
            Err(CallError::Invalid(_))
        ));
    }
    assert!(rig.fake.writes().is_empty());
    // The container path is as it was.
    let created = rig
        .call("service_create", json!({"name":"nats","template":"nats"}))
        .expect("create");
    assert_eq!(created["ipv4"], "192.0.2.10");
    let instance = rig
        .get(&format!("/1.0/instances/nats?project={PROJECT}"))
        .expect("instance");
    assert_eq!(instance["type"], "container");
    assert_eq!(instance["devices"]["eth0"]["network"], BRIDGE);
    let record = {
        let kernel = rig.kernel.lock().expect("kernel");
        kernel
            .entries()
            .iter()
            .find(|entry| entry.id.0 == "service-nats")
            .expect("record")
            .config
            .clone()
    };
    assert!(
        record.get("vm").is_none(),
        "a container's record carries no vm key"
    );
}

#[test]
fn service_remove_needs_the_owners_approval_and_retires_the_vm_and_its_record() {
    let key = common::SkKey::from_seed(9);
    let rig = Rig::bridged(&["br0"], Some(&key));
    rig.call("service_create", json!({"name":"ws1","template":"ws"}))
        .expect("create");
    let path = format!("/1.0/instances/ws1?project={PROJECT}");
    assert!(matches!(
        rig.call(
            "service_remove",
            json!({"name":"ws1","approval":"not-an-approval"})
        ),
        Err(CallError::Refused { code, .. }) if code == "owner_approval"
    ));
    assert!(rig.get(&path).is_some(), "a refused removal leaves the VM");
    let removed = rig
        .call(
            "service_remove",
            json!({"name":"ws1","approval":owner_approval(&key, "ws1")}),
        )
        .expect("remove");
    assert_eq!(removed, json!({"removed": true}));
    assert!(rig.get(&path).is_none(), "the VM is gone");
    assert_eq!(rig.entry_ids(), [BRIDGED_ENTRY], "the record is gone");
    assert_eq!(
        rig.call("service_list", json!({})).expect("list"),
        json!({"services": []})
    );
    assert_eq!(rig.network_requests(), Vec::<String>::new());
    // The name is free again.
    rig.call("service_create", json!({"name":"ws1","template":"ws"}))
        .expect("create again");
}

#[test]
fn a_vm_create_is_proposed_approved_by_the_owner_and_run_by_the_same_handler() {
    let key = common::SkKey::from_seed(9);
    let rig = Rig::bridged(&["br0", "br1"], Some(&key));
    let args = json!({"name":"ws1","template":"ws","bridge":"br1","mac":"02:AA:BB:00:00:01"});

    // The owner is shown the bridge and the MAC, and approves the MAC in the form the record keeps.
    let prepared = ActionKind::ServiceCreate
        .prepare(&args, None)
        .expect("the arguments fit the verb's schema");
    assert_eq!(prepared["mac"], "02:aa:bb:00:00:01");
    let details: Vec<(String, String)> = ActionKind::ServiceCreate
        .details(&prepared)
        .into_iter()
        .map(|row| (row.label, row.value))
        .collect();
    assert_eq!(
        details,
        [
            ("Name".to_string(), "ws1".to_string()),
            ("Template".to_string(), "ws".to_string()),
            ("Host bridge".to_string(), "br1".to_string()),
            ("MAC address".to_string(), "02:aa:bb:00:00:01".to_string()),
        ]
    );
    assert!(
        ActionKind::ServiceCreate
            .prepare(
                &json!({"name":"ws1","template":"ws","mac":"01:00:5e:00:00:01"}),
                None
            )
            .is_err(),
        "a group address is refused when the proposal is made"
    );

    let proposal = rig
        .kernel
        .lock()
        .expect("kernel")
        .propose_action(ActionKind::ServiceCreate, args, NOW, None)
        .expect("the proposal is inert");
    assert!(
        rig.fake.writes().is_empty() && rig.entry_ids().is_empty(),
        "a proposal creates nothing"
    );
    let statement = action_statement(
        "node1",
        &proposal.id,
        "service_create",
        &proposal.args_digest,
        NOW,
        &"7".repeat(32),
    )
    .expect("the statement");
    let approval = common::approve_statement(&key, &statement);
    let approved = {
        let mut kernel = rig.kernel.lock().expect("kernel");
        approve_action(
            &mut kernel,
            &json!({ "plan": proposal.id, "approval": approval }),
            NOW,
        )
        .expect("the owner's approval verifies")
    };
    let executor = ActionExecutor::new(
        rig.handlers
            .iter()
            .map(|(verb, handler)| (*verb, Arc::clone(handler))),
    );
    let result = executor
        .run(&approved.action, &approved.args, &approved.grant)
        .expect("the real service_create handler runs");
    assert_eq!(result["name"], "ws1");
    let instance = rig
        .get(&format!("/1.0/instances/ws1?project={PROJECT}"))
        .expect("the approved action created the VM");
    assert_eq!(instance["type"], "virtual-machine");
    assert_eq!(
        instance["devices"]["eth0"],
        json!({ "type": "nic", "name": "enp5s0", "nictype": "bridged", "parent": "br1", "hwaddr": "02:aa:bb:00:00:01" })
    );
}

#[test]
fn a_vm_template_needs_both_limits_and_a_root_size_and_nothing_of_the_service_network() {
    let dir = std::env::temp_dir().join(format!(
        "arca-bridged-templates-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    fs::create_dir(&dir).expect("scratch");
    let load = |value: Value| {
        fs::write(dir.join("ws.json"), value.to_string()).expect("template");
        service::load_template(&dir, "ws")
    };
    let template = load(vm_template()).expect("the template loads");
    assert!(template.is_vm());
    assert_eq!(template.root_size.as_deref(), Some("20GiB"));

    let without = |key: &str| {
        let mut value = vm_template();
        value.as_object_mut().unwrap().remove(key);
        value
    };
    let with = |key: &str, extra: Value| {
        let mut value = vm_template();
        value[key] = extra;
        value
    };
    for (bad, why) in [
        (without("limits"), "limits with both cpu and memory"),
        (
            with("limits", json!({"cpu":"2"})),
            "limits with both cpu and memory",
        ),
        (
            with("limits", json!({"memory":"4GiB"})),
            "limits with both cpu and memory",
        ),
        (
            with("limits", json!({"cpu":"0","memory":"4GiB"})),
            "limits with both cpu and memory",
        ),
        (without("rootSize"), "needs a rootSize"),
        (with("rootSize", json!("20")), "needs a rootSize"),
        (with("rootSize", json!("0GiB")), "needs a rootSize"),
        (with("ports", json!([22, 22])), "ports are distinct"),
        (with("ports", json!([0])), "ports are distinct"),
        (
            with("provides", json!({"env":{"URL":"x"}})),
            "provides needs a port",
        ),
        (with("lan", json!({"port": 22})), "has no env, generate"),
        (with("publishable", json!(true)), "has no env, generate"),
        (with("env", json!({"A":"b"})), "has no env, generate"),
        (
            with("generate", json!({"TOKEN":"k_"})),
            "has no env, generate",
        ),
        (
            with("data", json!({"path":"/data"})),
            "has no env, generate",
        ),
        (
            with("image", json!("docker.io/library/debian")),
            "invalid system image",
        ),
    ] {
        let error = load(bad.clone()).expect_err("the template is refused");
        assert!(
            !error.not_found && error.message.contains(why),
            "{bad}: {}",
            error.message
        );
    }
    // Only a virtual machine has a root size.
    let mut container = nats_template();
    container["name"] = json!("ws");
    container["rootSize"] = json!("20GiB");
    let error = load(container).expect_err("refused");
    assert!(
        error
            .message
            .contains("only a virtual-machine template has a rootSize")
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn the_shipped_workspace_vm_template_loads_as_a_virtual_machine() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../templates");
    let template = service::load_template(&dir, "workspace-vm").expect("the template loads");
    assert!(template.is_vm());
    assert_eq!(template.image, "images:debian/13");
    assert_eq!(template.root_size.as_deref(), Some("20GiB"));
    let limits = template.limits.expect("limits");
    assert_eq!(limits.cpu.as_deref(), Some("2"));
    assert_eq!(limits.memory.as_deref(), Some("4GiB"));
}

#[test]
fn a_create_that_would_exceed_a_cap_is_refused_in_plain_words_before_any_write() {
    // Each cap in turn is the one the second machine of the test template (2 CPUs, 4GiB, 20GiB) does not fit under.
    for (limits, refused) in [
        (
            json!({ "virtualMachines": 1, "cpu": 8, "memory": "16GiB", "disk": "80GiB" }),
            "creating ws2 would exceed this node's cap on virtual machines (kernel.bridged.limits.virtualMachines): 1 in use, 1 asked, 1 allowed",
        ),
        (
            json!({ "virtualMachines": 4, "cpu": 3, "memory": "16GiB", "disk": "80GiB" }),
            "creating ws2 would exceed this node's cap on CPUs (kernel.bridged.limits.cpu): 2 in use, 2 asked, 3 allowed",
        ),
        (
            json!({ "virtualMachines": 4, "cpu": 8, "memory": "6GiB", "disk": "80GiB" }),
            "creating ws2 would exceed this node's cap on memory (kernel.bridged.limits.memory): 4.0 GiB in use, 4.0 GiB asked, 6.0 GiB allowed",
        ),
        (
            json!({ "virtualMachines": 4, "cpu": 8, "memory": "16GiB", "disk": "30GiB" }),
            "creating ws2 would exceed this node's cap on disk (kernel.bridged.limits.disk): 20.0 GiB in use, 20.0 GiB asked, 30.0 GiB allowed",
        ),
    ] {
        let rig = Rig::with(bridged_kernel(&["br0"], limits), None, vec![]);
        rig.call("service_create", json!({"name":"ws1","template":"ws"}))
            .expect("the first machine fits");
        let writes = rig.fake.writes().len();
        assert_eq!(
            refusal(rig.call("service_create", json!({"name":"ws2","template":"ws"}))),
            refused
        );
        assert_eq!(rig.fake.writes().len(), writes, "nothing was written");
        let mut ids = rig.entry_ids();
        ids.sort();
        assert_eq!(ids, [BRIDGED_ENTRY, "service-ws1"], "nothing was recorded");
    }
    // A template larger than the whole node is refused on an empty node too.
    let rig = Rig::with(
        bridged_kernel(
            &["br0"],
            json!({ "virtualMachines": 4, "cpu": 8, "memory": "2GiB", "disk": "80GiB" }),
        ),
        None,
        vec![],
    );
    assert_eq!(
        refusal(rig.call("service_create", json!({"name":"ws1","template":"ws"}))),
        "creating ws1 would exceed this node's cap on memory (kernel.bridged.limits.memory): 0.0 GiB in use, 4.0 GiB asked, 2.0 GiB allowed"
    );
    assert!(rig.fake.writes().is_empty() && rig.entry_ids().is_empty());
}

fn other_instance(name: &str, config: Value, extra: Value) -> Value {
    let mut row = json!({
        "name": name, "project": "elsewhere", "type": "virtual-machine", "status": "Running",
        "config": config, "devices": {}
    });
    row.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    row
}

#[test]
fn a_mac_incus_assigned_or_a_profile_gave_another_instance_is_refused() {
    let rig = Rig::with(
        bridged_kernel(&["br0"], roomy()),
        None,
        vec![
            other_instance(
                "assigned",
                json!({"volatile.eth0.hwaddr": "10:66:6A:00:00:01"}),
                json!({}),
            ),
            other_instance(
                "profiled",
                json!({}),
                json!({"expanded_devices": {"eth0": {"type": "nic", "hwaddr": "10:66:6a:00:00:02"}}}),
            ),
            other_instance(
                "own",
                json!({}),
                json!({"devices": {"eth0": {"type": "nic", "hwaddr": "10:66:6a:00:00:03"}}}),
            ),
        ],
    );
    for mac in [
        "10:66:6a:00:00:01",
        "10:66:6A:00:00:02",
        "10:66:6a:00:00:03",
    ] {
        assert_eq!(
            refusal(rig.call(
                "service_create",
                json!({"name":"ws1","template":"ws","mac":mac})
            )),
            format!(
                "the MAC {} is already held by another instance",
                mac.to_ascii_lowercase()
            )
        );
    }
    assert!(rig.fake.writes().is_empty() && rig.entry_ids().is_empty());
    rig.call(
        "service_create",
        json!({"name":"ws1","template":"ws","mac":"10:66:6a:00:00:04"}),
    )
    .expect("a free MAC is taken");
}

#[test]
fn a_managed_node_switched_to_bridged_is_refused_at_start_and_at_create_and_nothing_is_left_behind()
{
    let rig = Rig::managed();
    rig.call("service_create", json!({"name":"nats","template":"nats"}))
        .expect("a container on the managed network");
    let before = rig.entries();
    let writes = rig.fake.writes().len();
    let (cfg, handlers) = rig.switched(bridged_kernel(&["br0"], roomy()));

    let why = "kernel.bridged is set, but this node already holds net-arcak-svc from the managed service network. A node is bridged from its first service or not at all: remove kernel.bridged, or remove those services first";
    assert_eq!(
        arca::handlers::mode_refusal(&cfg, &before).as_deref(),
        Some(why),
        "serve refuses to start"
    );
    assert_eq!(
        refusal(handlers["service_create"].call(json!({"name":"ws1","template":"ws"}), &doctor())),
        why,
        "the create names the reason"
    );
    assert_eq!(rig.fake.writes().len(), writes, "nothing was written");
    let after: Vec<String> = rig.entries().iter().map(|e| e.id.0.clone()).collect();
    let was: Vec<String> = before.iter().map(|e| e.id.0.clone()).collect();
    assert_eq!(after, was, "no bridged entry is left to fail every pass");
    // The node's own config still agrees with its state.
    assert_eq!(arca::handlers::mode_refusal(&rig.cfg, &before), None);
}

#[test]
fn a_bridged_node_switched_to_managed_is_refused_at_start() {
    let rig = Rig::bridged(&["br0"], None);
    rig.call("service_create", json!({"name":"ws1","template":"ws"}))
        .expect("create");
    let entries = rig.entries();
    assert_eq!(arca::handlers::mode_refusal(&rig.cfg, &entries), None);
    let (cfg, _) = rig.switched(managed_kernel());
    let why = arca::handlers::mode_refusal(&cfg, &entries).expect("serve refuses to start");
    // Removing the machine would not help: the project stays a bridged-only one. The advice is one that can be followed.
    assert_eq!(
        why,
        "kernel.bridged is not set, but this node already holds service-ws1 from bridged mode, in a service project created in bridged mode. Put kernel.bridged back. To use the managed service network, set kernel.serviceCidr beside kernel.bridged: the node then becomes a mixed node, never a managed-only one"
    );
    // The project entry alone, with every machine removed, still marks the node as bridged.
    let project_only: Vec<_> = entries
        .iter()
        .filter(|entry| entry.id.0 == BRIDGED_ENTRY)
        .cloned()
        .collect();
    // No machine is left to remove, so the advice names what can be done: the node becomes mixed, never managed-only.
    assert_eq!(
        arca::handlers::mode_refusal(&cfg, &project_only).as_deref(),
        Some(
            "kernel.bridged is not set, but this node's service project was created in bridged mode (bridged-arcak-svc). Put kernel.bridged back. To use the managed service network, set kernel.serviceCidr beside kernel.bridged: the node then becomes a mixed node, never a managed-only one"
        )
    );
    assert_eq!(arca::handlers::mode_refusal(&cfg, &[]), None);
}

#[test]
fn an_existing_project_this_node_did_not_create_refuses_the_create_and_leaves_no_entry() {
    for config in [json!({}), json!({KERNEL_ENTRY_KEY: "net-arcak-svc"})] {
        let rig = Rig::bridged(&["br0"], None);
        // A project made by hand, or left by the managed service network, outside the kernel's writer.
        use std::io::{Read, Write};
        let mut stream = std::os::unix::net::UnixStream::connect(rig.fake.socket()).unwrap();
        let body = json!({"name": PROJECT, "config": config}).to_string();
        write!(stream, "POST /1.0/projects HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 20"), "{response}");
        let writes = rig.fake.writes().len();

        assert_eq!(
            refusal(rig.call("service_create", json!({"name":"ws1","template":"ws"}))),
            format!(
                "the Incus project {PROJECT} already exists and this bridged node did not create it; Arca leaves it alone, so no virtual machine can be created until it is removed"
            )
        );
        assert_eq!(rig.fake.writes().len(), writes, "nothing was written");
        assert!(
            rig.entry_ids().is_empty(),
            "no entry is left to fail every pass"
        );
    }
}

#[test]
fn a_bridge_this_host_does_not_have_is_named_at_start_without_asking_incus() {
    let rig = Rig::bridged(&["br0", "br1"], None);
    let sys = rig.dir.join("sys-class-net");
    fs::create_dir_all(sys.join("br0/bridge")).unwrap();
    // An interface that exists but is not a bridge has no `bridge` directory.
    fs::create_dir_all(sys.join("br1")).unwrap();
    let bridged = rig.cfg.kernel.bridged.as_ref().expect("bridged");
    let requests = rig.fake.requests().len();
    assert_eq!(arca::handlers::missing_bridges(bridged, &sys), ["br1"]);
    fs::create_dir_all(sys.join("br1/bridge")).unwrap();
    assert!(arca::handlers::missing_bridges(bridged, &sys).is_empty());
    assert_eq!(rig.fake.requests().len(), requests, "Incus is not asked");
}

#[test]
fn a_proposal_this_node_is_sure_to_refuse_is_refused_when_proposed_and_an_omitted_bridge_is_shown()
{
    let propose = |rig: &Rig, args: Value| {
        let handlers: HashMap<&'static str, Arc<dyn ToolHandler>> =
            arca::handlers::plan_handlers_served(
                rig.kernel.clone(),
                Arc::new(|| NOW),
                None,
                None,
                None,
                None,
                arca::handlers::ActionSlot::default(),
                Some(rig.cfg.clone()),
            )
            .into_iter()
            .collect();
        handlers["action_propose"].call(json!({"action":"service_create","args":args}), &doctor())
    };
    let rig = Rig::bridged(&["br0"], None);
    let proposed =
        propose(&rig, json!({"name":"ws1","template":"ws"})).expect("the proposal is recorded");
    assert_eq!(
        proposed["summary"],
        "Create the virtual machine ws1 from template ws on the host bridge br0"
    );
    assert!(
        proposed["details"]
            .as_array()
            .expect("details")
            .contains(&json!({"label":"Host bridge","value":"br0"})),
        "{proposed}"
    );
    assert_eq!(
        refusal(propose(
            &rig,
            json!({"name":"ws2","template":"ws","bridge":"br9"})
        )),
        "bridge br9 is not one of the bridges this node allows (kernel.bridged.bridges)"
    );
    assert_eq!(
        refusal(propose(&rig, json!({"name":"nats","template":"nats"}))),
        "this node is a bridged node: it runs virtual-machine templates only, and template nats is not one"
    );
    let managed = Rig::managed();
    assert_eq!(
        refusal(propose(&managed, json!({"name":"ws1","template":"ws"}))),
        "template ws is a virtual machine, and this node sets no virtual-machine caps (kernel.vmLimits, or kernel.bridged with its limits)"
    );
    let container = propose(&managed, json!({"name":"nats","template":"nats"}))
        .expect("a container proposal is as before");
    assert_eq!(
        container["summary"],
        "Create the service nats from template nats"
    );
    assert!(rig.fake.writes().is_empty() && managed.fake.writes().is_empty());
}

#[path = "services/mixed.rs"]
mod mixed;
