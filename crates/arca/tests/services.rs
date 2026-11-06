//! Service verbs against a fake Incus: create, list and status are hermetic; no live host is touched.

use arca::{
    config::NodeConfig, handlers::service_handlers, kernel::Kernel, service, service::Service,
};
use arca_api::Scope;
use arca_approval::{ApprovalVerifier, FileNonces, OwnerKeys, PasskeyVerifier, SshSigVerifier};
use arca_incus::{FakeIncus, FakeState, IncusRead, UnixIncus};
use arca_kernel::identity::NodeKey;
use arca_mcp::{CallError, Caller, ToolHandler};
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

struct Rig {
    dir: PathBuf,
    fake: FakeIncus,
    kernel: Arc<Mutex<Kernel>>,
    handlers: HashMap<&'static str, Arc<dyn ToolHandler>>,
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
        let verifier = ApprovalVerifier::new(
            "node1".into(),
            OwnerKeys::new(
                SshSigVerifier::new(vec![]),
                PasskeyVerifier::refusing(arca_approval::ApprovalRefusal::NoVerifier),
            ),
            FileNonces::open(&dir.join("nonces")).expect("nonces"),
        );
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
                verifier,
                "node1".into(),
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
        let handlers = service_handlers(kernel.clone(), Arc::new(|| NOW), cfg)
            .into_iter()
            .collect();
        Self {
            dir,
            fake,
            kernel,
            handlers,
        }
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
    let listed = rig.call("service_list", json!({})).expect("list");
    assert_eq!(listed["services"], json!([created]));
    let status = rig
        .call("service_status", json!({"name":"nats"}))
        .expect("status");
    assert_eq!(status, created);
    let instance = rig
        .get(&format!("/1.0/instances/nats?project={PROJECT}"))
        .expect("instance");
    assert_eq!(
        instance["config"][format!("user.arcakernel.service.template")],
        "nats"
    );
    assert_eq!(
        instance["config"]["user.arcakernel.service.address"],
        "192.0.2.10"
    );
    let acl = rig
        .get(&format!(
            "/1.0/network-acls/{PROJECT}-svc-nats?project=default"
        ))
        .expect("acl");
    assert_eq!(acl["config"]["user.arcakernel.service.managed"], "true");
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
    }
}

#[test]
fn generate_and_droidd_port_the_key_is_made_at_create_in_its_own_form_before_the_first_start_the_acl_lets_the_bridge_gateway_reach_droidd_port_alone()
 {
    let rig = Rig::new();
    rig.template(
        "omni",
        json!({
            "name":"omni","kind":"oci","image":"ghcr.io/automagik-dev/omni-api",
            "digest":DIGEST,"ports":[8882],
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
    rig.template(
        "omni",
        json!({
            "name":"omni","kind":"oci","image":"ghcr.io/automagik-dev/omni-api",
            "digest":DIGEST,"ports":[8882],
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
            "ports":[8080],"env":{},"publishable":false,
            "entrypoint":["/bin/sh","-ec",{"file":"jet.sh"},"jet"]
        }),
    );
    fs::write(rig.dir.join("templates/jet.sh"), "echo ok\n").expect("script");
    let refused = rig.call(
        "service_create",
        json!({"name":"jet","template":"jet","entrypoint":["evil"]}),
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
    assert!(!entrypoint.contains('\n'));
}

#[test]
fn service_create_puts_each_service_in_the_project_with_desired_service_nic_its_own_nic_acl_and_its_user_arcakernel_keys_and_starts_it()
 {
    let rig = Rig::new();
    rig.template("nats", nats_template());
    let created = rig
        .call("service_create", json!({"name":"nats","template":"nats"}))
        .expect("create");
    assert_eq!(
        created,
        json!({"name":"nats","template":"nats","state":"running","ipv4":"192.0.2.10","ports":[4222],"links":[],"published":null})
    );
    let instance = rig
        .get(&format!("/1.0/instances/nats?project={PROJECT}"))
        .expect("instance");
    assert_eq!(
        instance["config"]["user.arcakernel.service.template"],
        "nats"
    );
    assert_eq!(
        instance["config"]["user.arcakernel.service.address"],
        "192.0.2.10"
    );
    assert_eq!(instance["config"]["user.arcakernel.service.ports"], "4222");
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
    assert_eq!(
        instance["config"]["user.arcakernel.service.envkeys"],
        "PORT,SELF"
    );
}

#[test]
fn a_failed_create_leaves_no_acl_behind_and_no_row() {
    let rig = Rig::new();
    rig.template("nats", nats_template());
    rig.fake.set_reject_config_on_post("/1.0/instances");
    let failed = rig.call("service_create", json!({"name":"nats","template":"nats"}));
    assert!(matches!(
        failed,
        Err(CallError::Refused { message, .. }) if message.contains("create service nats")
    ));
    assert!(
        rig.get(&format!(
            "/1.0/network-acls/{PROJECT}-svc-nats?project=default"
        ))
        .is_none(),
        "the ACL is gone"
    );
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
}

#[test]
fn the_tools_operator_scope_read_only_reads_service_remove_destructive_a_viewer_reads_and_changes_nothing()
 {
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
    let local = source(&source_template("system", "local:my-image", None)).expect("local");
    assert_eq!(local, json!({"type":"image","alias":"my-image"}));
    assert!(source(&source_template("oci", "docker.io/library/nats", None)).is_err());
    assert!(source(&source_template("system", "https://example.org/x", None)).is_err());
}

#[test]
fn lists_the_service_project_with_each_instances_own_config_and_devices_none_when_incus_has_no_such_project()
 {
    let rig = Rig::with(
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![
                hand_instance(
                    "hand",
                    PROJECT,
                    json!({"eth0":{"type":"nic","ipv4.address":"192.0.2.13"}}),
                ),
                hand_instance("other", "default", json!({})),
            ],
            networks: vec![],
            pools: vec![],
        },
        vec![PROJECT.to_string()],
        Some("192.0.2.0/24"),
    );
    let listed = rig.call("service_list", json!({})).expect("list");
    let rows = listed["services"].as_array().expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["name"], "hand");
    assert_eq!(rows[0]["state"], "unknown");
    assert_eq!(rows[0]["ipv4"], "192.0.2.13");
    let empty = Rig::new();
    assert_eq!(
        empty.call("service_list", json!({})).expect("empty"),
        json!({"services":[]})
    );
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
    let hand = Rig::with(
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![hand_instance("taken", PROJECT, json!({}))],
            networks: vec![],
            pools: vec![],
        },
        vec![PROJECT.to_string()],
        Some("192.0.2.0/24"),
    );
    hand.template("nats", nats_template());
    assert!(matches!(
        hand.call("service_create", json!({"name":"taken","template":"nats"})),
        Err(CallError::Refused { message, .. }) if message.contains("not a service")
    ));
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
    rig.template("nats", nats_template());
    rig.call("service_create", json!({"name":"nats","template":"nats"}))
        .expect("create");
    let acl = rig
        .get(&format!(
            "/1.0/network-acls/{PROJECT}-svc-nats?project=default"
        ))
        .expect("acl");
    assert_eq!(acl["name"], format!("{PROJECT}-svc-nats"));
    assert_eq!(acl["config"]["user.arcakernel.service.managed"], "true");
    assert_eq!(
        arca::service::acl_name(PROJECT, "nats"),
        format!("{PROJECT}-svc-nats")
    );
    assert_eq!(acl["egress"], json!([]));
}
