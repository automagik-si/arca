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
}

impl Rig {
    /// A bridged node that allows `bridges` and keeps root disks on `vmpool`.
    fn bridged(bridges: &[&str], key: Option<&common::SkKey>) -> Self {
        Self::with(
            json!({
                "projects": [PROJECT],
                "droiddProjects": ["other"],
                "bridged": { "bridges": bridges, "pool": "vmpool" }
            }),
            key,
        )
    }

    /// A node with the managed service network, as every node before bridged mode.
    fn managed() -> Self {
        Self::with(
            json!({
                "projects": [PROJECT],
                "droiddProjects": ["other"],
                "serviceCidr": "192.0.2.0/24"
            }),
            None,
        )
    }

    fn with(kernel_config: Value, key: Option<&common::SkKey>) -> Self {
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
            "kernel": kernel_config
        }))
        .expect("config");
        arca::config::check(&cfg).expect("the config passes its rules");
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
        let handlers = service_handlers(kernel.clone(), Arc::new(|| NOW), Arc::new(cfg))
            .into_iter()
            .collect();
        let rig = Self {
            dir,
            fake,
            kernel,
            handlers,
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
            "eth0": { "type": "nic", "nictype": "bridged", "parent": "br1", "hwaddr": "02:aa:bb:00:00:01" }
        }),
        "one root disk on the configured pool and one NIC on the host bridge: no network, address, ACL or filter key"
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
        json!({ "type": "nic", "nictype": "bridged", "parent": "br0" }),
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
        "service ws1 is a virtual machine on a host bridge that Arca does not manage: it has no template secrets or environment for Arca to set"
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
        "template ws is a virtual machine, and this node is not a bridged node (kernel.bridged is not set)"
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
        json!({ "type": "nic", "nictype": "bridged", "parent": "br1", "hwaddr": "02:aa:bb:00:00:01" })
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
        (with("ports", json!([22])), "it has no ports"),
        (with("lan", json!({"port": 22})), "it has no ports"),
        (with("publishable", json!(true)), "it has no ports"),
        (with("env", json!({"A":"b"})), "it has no ports"),
        (with("secrets", json!(["TOKEN"])), "it has no ports"),
        (with("data", json!({"path":"/data"})), "it has no ports"),
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
