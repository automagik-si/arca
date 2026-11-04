use arca::{config::NodeConfig, handlers::postgres_handlers, kernel::Kernel};
use arca_api::Scope;
use arca_approval::{
    ApprovalRefusal, ApprovalVerifier, FileNonces, OwnerKeys, PasskeyVerifier, SshSigVerifier,
};
use arca_incus::{FakeIncus, FakeState, IncusRead, UnixIncus};
use arca_kernel::{
    desired::EntryId,
    identity::NodeKey,
    journal::{Journal, MemJournal},
    plugin::{Ctx, Plugin, PluginError},
};
use arca_mcp::{CallError, Caller, ToolHandler};
use arca_netproject::{BRIDGE, PROJECT};
use arca_postgres::{Postgres, PostgresConfig, consumer::Consumer};
use arca_providers::incus::{Family, IncusProvider, RetainedSet};
use arca_reconcile::PluginSet;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::{
        Arc, Mutex, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard,
        atomic::{AtomicU64, Ordering},
    },
};
const NOW: u64 = 1_793_361_600_000;
static SEQ: AtomicU64 = AtomicU64::new(0);
/// A store is open in some test while this is read-locked. The password-scan test takes the write
/// side while it spawns and waits for its child, so no sibling store is open in that window and
/// the child cannot race a sibling test's desired.lock.
static STORES: RwLock<()> = RwLock::new(());
fn store_guard() -> RwLockReadGuard<'static, ()> {
    STORES.read().unwrap_or_else(PoisonError::into_inner)
}
fn spawn_guard() -> RwLockWriteGuard<'static, ()> {
    STORES.write().unwrap_or_else(PoisonError::into_inner)
}
struct Rig {
    /// Held for the Rig's lifetime: the store is open while the guard lives.
    _store: RwLockReadGuard<'static, ()>,
    dir: PathBuf,
    fake: FakeIncus,
    kernel: Arc<Mutex<Kernel>>,
    handlers: HashMap<&'static str, Arc<dyn ToolHandler>>,
    cfg: NodeConfig,
}
fn scratch() -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "arca-postgres-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    fs::create_dir(&p).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o700)).unwrap();
    p
}
impl Rig {
    fn new(dir: PathBuf, running: bool) -> Self {
        let store = store_guard();
        let consumer = json!({"name":"consumer-a","project":PROJECT,"status":"Running","config":{"user.arcakernel.entry":"consumer-consumer-a","environment.UNRELATED":"keep"},"devices":{"eth0":{"type":"nic","name":"eth0","network":BRIDGE,"ipv4.address":"192.0.2.10"}}});
        let fake = FakeIncus::start(
            &dir,
            FakeState {
                server: json!({}),
                projects: vec![],
                instances: if running { vec![consumer] } else { vec![] },
                networks: vec![],
                pools: vec![],
            },
        )
        .unwrap();
        fake.add_profile("default",json!({"name":"default","config":{},"devices":{"root":{"type":"disk","pool":"pool1","path":"/"}}}));
        let cfg:NodeConfig = serde_json::from_value(json!({"stateDir":dir,"incus":{"socket":fake.socket()},"listen":{"unix":dir.join("arca.sock")},"tokens":{"hashDir":dir.join("tokens")},"approvalHost":"node1","kernel":{"projects":[PROJECT],"droiddProjects":["other"],"serviceCidr":"192.0.2.0/24","reservedCidrs":["198.51.100.0/24"]}})).unwrap();
        let cfg_json = json!({"stateDir":dir,"incus":{"socket":fake.socket()},"listen":{"unix":dir.join("arca.sock")},"tokens":{"hashDir":dir.join("tokens")},"kernel":{"projects":[PROJECT],"droiddProjects":["other"],"serviceCidr":"192.0.2.0/24"}});
        fs::write(dir.join("config.json"), cfg_json.to_string()).unwrap();
        let args: Vec<String> = [
            "probe",
            "consumer",
            "add",
            "--name",
            "consumer-a",
            "--image",
            &"a".repeat(64),
            "--config",
            dir.join("config.json").to_str().unwrap(),
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(arca::run(&args).unwrap().unwrap()["seq"], 1);
        let mut plugins = PluginSet::new();
        plugins.add::<arca_netproject::NetProject>().unwrap();
        plugins.add::<Postgres>().unwrap();
        plugins.add::<Consumer>().unwrap();
        let verifier = ApprovalVerifier::new(
            "node1".into(),
            OwnerKeys::new(
                SshSigVerifier::new(vec![]),
                PasskeyVerifier::refusing(ApprovalRefusal::NoVerifier),
            ),
            FileNonces::open(&dir.join("nonces")).unwrap(),
        );
        let socket = fake.socket().to_path_buf();
        let secrets = dir.join("secrets");
        let kernel = Arc::new(Mutex::new(
            Kernel::open(
                &dir,
                NodeKey::load_or_create(&dir.join("node.key")).unwrap(),
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
                        .unwrap(),
                    ))
                },
                NOW,
            )
            .unwrap(),
        ));
        let handlers = postgres_handlers(kernel.clone(), Arc::new(|| NOW), Arc::new(cfg.clone()))
            .into_iter()
            .collect();
        Self {
            _store: store,
            dir,
            fake,
            kernel,
            handlers,
            cfg,
        }
    }
    fn call(&self, verb: &str, args: Value) -> Result<Value, CallError> {
        self.handlers[verb].call(
            args,
            &Caller {
                token_id: "doctor".into(),
                scope: Scope::Doctor,
            },
        )
    }
    fn create(&self, name: &str) -> Value {
        self.call(
            "postgres_create",
            json!({"name":name,"image":format!("example/db@sha256:{}","b".repeat(64))}),
        )
        .unwrap()
    }
    fn get(&self, path: &str) -> Value {
        UnixIncus::new(self.fake.socket().into()).get(path).unwrap()
    }
}
impl Drop for Rig {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn postgres_skips_equal_acl_arrays_and_kernel_instances_are_running() {
    let r = Rig::new(scratch(), false);
    r.create("db1");
    for name in ["consumer-a", "db1"] {
        let state = r.get(&format!("/1.0/instances/{name}/state?project={PROJECT}"));
        assert_eq!(state["status"], "Running", "{name}");
        assert!(arca_postgres::ipv4(&state).is_some(), "{name}");
    }
    let records: Vec<Value> = fs::read_to_string(r.dir.join("journal.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        records
            .iter()
            .all(|record| record.get("acl_patch").is_none())
    );

    // The same empty array on an existing ACL also needs no patch. Then repeat
    // with nonempty ingress after linking a consumer.
    for linked in [false, true] {
        if linked {
            r.call(
                "postgres_link",
                json!({"name":"db1","consumer":"consumer-a"}),
            )
            .unwrap();
        }
        let cfg: PostgresConfig = serde_json::from_value(
            r.kernel
                .lock()
                .unwrap()
                .entries()
                .iter()
                .find(|e| e.id.0 == "postgres-db1")
                .unwrap()
                .config
                .clone(),
        )
        .unwrap();
        let mut provider = IncusProvider::new(
            r.fake.socket().into(),
            Family {
                projects: vec![PROJECT.into()],
                scratch: false,
                droidd_projects: vec![],
                reserved_cidrs: vec![],
            },
            r.dir.join("secrets"),
            RetainedSet::empty(),
        )
        .unwrap();
        let mut journal = Journal::open(MemJournal::new()).unwrap();
        let n = r.fake.writes().len();
        Postgres::apply(
            &mut Ctx::new(
                EntryId("postgres-db1".into()),
                &Postgres::manifest(),
                &mut provider,
                &mut journal,
                NOW,
            ),
            &cfg,
        )
        .unwrap();
        assert_eq!(r.fake.writes().len(), n);
        assert!(journal.live(&EntryId("postgres-db1".into())).is_empty());
        assert!(journal.unresolved().is_empty());
    }
    let list = r.call("postgres_list", json!({})).unwrap();
    assert_eq!(list["databases"][0]["status"], "running");
    assert!(list["databases"][0]["address"].is_string());
}

#[test]
fn link_and_unlink_write_only_the_consumers_instance_and_databases_acl() {
    let r = Rig::new(scratch(), true);
    r.create("db1");
    let n = r.fake.writes().len();
    assert_eq!(
        r.call(
            "postgres_link",
            json!({"name":"db1","consumer":"consumer-a"})
        )
        .unwrap()["env"],
        json!(["DATABASE_URL"])
    );
    let writes = r.fake.writes();
    let changed = &writes[n..];
    assert_eq!(changed.len(), 2, "{changed:?}");
    for w in changed {
        assert_eq!(w.method, "PATCH");
        assert!(
            [
                "/1.0/instances/consumer-a?project=arcak-svc",
                "/1.0/network-acls/postgres-db1?project=default"
            ]
            .contains(&w.path.as_str())
        );
    }
    let acl = r.get("/1.0/network-acls/postgres-db1?project=default");
    assert_eq!(acl["ingress"][0]["source"], "192.0.2.10/32");
    assert_eq!(acl["ingress"][0]["destination_port"], "5432");
    assert_eq!(acl["egress"], json!([]));
    let instance = r.get("/1.0/instances/db1?project=arcak-svc");
    assert_eq!(
        instance["devices"]["root"],
        json!({"type":"disk","pool":"pool1","path":"/"})
    );
    assert_eq!(
        instance["devices"]["eth0"]["security.acls.default.ingress.action"],
        "reject"
    );
    let entries = r.kernel.lock().unwrap().entries().to_vec();
    assert_eq!(entries[1].id.0, "consumer-consumer-a");
    assert_eq!(entries[1].config["links"][0]["database"], "db1");
    assert_eq!(entries[2].config["allow"], json!(["consumer-a"]));
    let desired: Value =
        serde_json::from_slice(&fs::read(r.dir.join("desired.json")).unwrap()).unwrap();
    assert_eq!(desired["current"]["document"]["seq"], 3);
    let n = r.fake.writes().len();
    assert_eq!(
        r.call(
            "postgres_unlink",
            json!({"name":"db1","consumer":"consumer-a"})
        )
        .unwrap()["removed"],
        json!(["DATABASE_URL"])
    );
    assert_eq!(r.fake.writes().len() - n, 2);
    let config = r.get("/1.0/instances/consumer-a?project=arcak-svc")["config"].clone();
    assert_eq!(config["environment.UNRELATED"], "keep");
    assert!(config.get("environment.DATABASE_URL").is_none());
    assert_eq!(
        r.get("/1.0/network-acls/postgres-db1?project=default")["ingress"],
        json!([])
    );
    assert!(!r.dir.join("secrets/consumer-consumer-a/db1-url").exists());
}
#[test]
fn digest_tag_refusal_never_echoes_the_image() {
    let dir = scratch();
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
    .unwrap();
    let mut provider = IncusProvider::new(
        fake.socket().into(),
        Family {
            projects: vec![PROJECT.into()],
            scratch: false,
            droidd_projects: vec![],
            reserved_cidrs: vec![],
        },
        dir.join("secrets"),
        RetainedSet::empty(),
    )
    .unwrap();
    let mut journal = Journal::open(MemJournal::default()).unwrap();
    let manifest = Postgres::manifest();
    for image in [
        "example/db:latest".to_string(),
        format!("example/db@sha256:{}", "A".repeat(64)),
        "example/db".into(),
    ] {
        let cfg = PostgresConfig {
            name: "db1".into(),
            image: image.clone(),
            pool: "pool1".into(),
            project: PROJECT.into(),
            bridge: BRIDGE.into(),
            allow: vec![],
        };
        let error = Postgres::apply(
            &mut Ctx::new(
                EntryId("postgres-db1".into()),
                &manifest,
                &mut provider,
                &mut journal,
                NOW,
            ),
            &cfg,
        )
        .unwrap_err();
        assert!(matches!(error, PluginError::Config(_)));
        assert!(!format!("{error:?} {error}").contains(&image));
    }
    assert!(fake.requests().is_empty());
    drop(fake);
    fs::remove_dir_all(dir).unwrap();
}
#[test]
fn viewer_is_refused_by_every_postgres_write_handler() {
    let r = Rig::new(scratch(), false);
    for (verb, handler) in &r.handlers {
        if *verb == "postgres_list" {
            continue;
        }
        let e = handler
            .call(
                json!({}),
                &Caller {
                    token_id: "viewer".into(),
                    scope: Scope::Viewer,
                },
            )
            .unwrap_err();
        assert!(matches!(e, CallError::Forbidden { .. }));
    }
    assert!(!r.dir.join("secrets").exists());
}
#[test]
fn missing_entries_names_images_service_cidr_and_locked_store_are_refused() {
    let r = Rig::new(scratch(), false);
    for args in [
        json!({"name":"../db1","image":"bad"}),
        json!({"name":"db1","image":"tag:latest"}),
    ] {
        assert!(r.call("postgres_create", args).is_err());
    }
    assert!(!r.dir.join("secrets").exists());
    assert!(
        r.call(
            "postgres_link",
            json!({"name":"db1","consumer":"consumer-a"})
        )
        .is_err()
    );
    r.create("db1");
    assert!(
        r.call("postgres_link", json!({"name":"db1","consumer":"missing"}))
            .is_err()
    );
    assert!(
        r.call("postgres_create", json!({"name":"db1","image":"bad"}))
            .is_err()
    );
    let args: Vec<_> = [
        "probe",
        "consumer",
        "remove",
        "--name",
        "consumer-a",
        "--config",
        r.dir.join("config.json").to_str().unwrap(),
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let error = arca::run(&args).unwrap_err();
    assert!(error.message().contains("held") || error.message().contains("lock"));
    let mut cfg = r.cfg.clone();
    cfg.kernel.service_cidr = None;
    assert!(
        format!(
            "{:?}",
            arca::handlers::ensure_netproject(&[], &cfg).unwrap_err()
        )
        .contains("serviceCidr")
    );
    cfg.kernel.service_cidr = Some("192.0.2.0/25".into());
    assert!(arca::config::check(&cfg).is_err());
    cfg.kernel.service_cidr = Some("198.51.100.0/24".into());
    assert!(arca::config::check(&cfg).is_err());
    cfg.kernel.service_cidr = Some("192.0.2.0/24".into());
    assert!(arca::config::check(&cfg).is_ok());
}
#[test]
fn second_link_uses_database_suffix_list_reads_only_state_and_delete_returns_plan() {
    let r = Rig::new(scratch(), true);
    r.create("db1");
    r.create("db2");
    r.call(
        "postgres_link",
        json!({"name":"db1","consumer":"consumer-a"}),
    )
    .unwrap();
    assert_eq!(
        r.call(
            "postgres_link",
            json!({"name":"db2","consumer":"consumer-a"})
        )
        .unwrap()["env"],
        json!(["DATABASE_URL_DB2"])
    );
    let n = r.fake.requests().len();
    let list = r.call("postgres_list", json!({})).unwrap();
    assert!(list["databases"][0]["address"].is_string());
    assert_eq!(list["databases"][0]["status"], "running");
    assert_eq!(list["databases"][0]["port"], 5432);
    assert_eq!(list["databases"][0]["links"], json!(["consumer-a"]));
    assert!(
        r.fake.requests()[n..]
            .iter()
            .all(|q| q.method == "GET" && q.path.contains("/state?"))
    );
    r.call(
        "postgres_unlink",
        json!({"name":"db1","consumer":"consumer-a"}),
    )
    .unwrap();
    assert!(r.get("/1.0/instances/consumer-a?project=arcak-svc")["config"]["environment.DATABASE_URL_DB2"].is_string());
    let deleted = r.call("postgres_delete", json!({"name":"db1"})).unwrap();
    assert_eq!(deleted["removed"], false);
    assert_eq!(deleted["plan"]["entry"], "postgres-db1");
    assert!(deleted["plan"]["statement"].is_string());
    assert!(
        r.kernel
            .lock()
            .unwrap()
            .entries()
            .iter()
            .any(|e| e.id.0 == "net-arcak-svc")
    );
    assert!(r.call("postgres_delete", json!({"name":"db1"})).is_err());
    assert!(!r.dir.join("secrets/postgres-db1/password").exists());
}

// Run the entire secret-bearing flow under a captured test subprocess, including all handler results and errors.
#[test]
#[ignore]
fn password_flow_child() {
    let dir = PathBuf::from(std::env::var_os("ARCA_PASSWORD_SCAN_DIR").unwrap());
    let r = Rig::new(dir.clone(), true);
    let mut transcript = String::new();
    let mut password = None;
    for (verb, args) in [
        (
            "postgres_create",
            json!({"name":"db1","image":format!("example/db@sha256:{}","b".repeat(64))}),
        ),
        (
            "postgres_link",
            json!({"name":"db1","consumer":"consumer-a"}),
        ),
        ("postgres_list", json!({})),
        (
            "postgres_unlink",
            json!({"name":"db1","consumer":"consumer-a"}),
        ),
        ("postgres_link", json!({"name":"db1","consumer":"missing"})),
        ("postgres_create", json!({"name":"db1","image":"bad"})),
        ("postgres_delete", json!({"name":"db1"})),
        ("postgres_delete", json!({"name":"db1"})),
    ] {
        let result = r.call(verb, args);
        if verb == "postgres_create" && result.is_ok() {
            password = Some(fs::read_to_string(dir.join("secrets/postgres-db1/password")).unwrap());
            for path in ["secrets", "secrets/postgres-db1"] {
                assert_eq!(
                    fs::metadata(dir.join(path)).unwrap().permissions().mode() & 0o777,
                    0o700
                );
            }
            assert_eq!(
                fs::metadata(dir.join("secrets/postgres-db1/password"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        if verb == "postgres_link" && result.is_ok() {
            assert_eq!(
                fs::metadata(dir.join("secrets/consumer-consumer-a"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(dir.join("secrets/consumer-consumer-a/db1-url"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        match &result {
            Ok(v) => transcript.push_str(&v.to_string()),
            Err(e) => transcript.push_str(&format!("{e:?}")),
        }
        transcript.push_str(&format!("{result:?}"));
    }
    let password = password.unwrap();
    assert_eq!(password.len(), 64);
    assert!(password.bytes().all(|b| b.is_ascii_hexdigit()));
    // Private test evidence survives the cleanup so the parent can scan captured output.
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let mut evidence = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dir.join("scan-password"))
        .unwrap();
    evidence.write_all(password.as_bytes()).unwrap();
    transcript.push_str(&fs::read_to_string(dir.join("journal.jsonl")).unwrap());
    transcript.push_str(&fs::read_to_string(dir.join("desired.json")).unwrap());
    transcript.push_str(&format!("{:?} {:?}", r.fake.requests(), r.fake.writes()));
    assert!(!transcript.contains(&password));
    fs::write(dir.join("scanned-transcript"), transcript).unwrap();
    // Keep the password and evidence until the parent scans captured stdout/stderr.
    std::mem::forget(r);
}
#[test]
fn password_free_results_errors_journal_desired_and_captured_stderr() {
    let _spawn = spawn_guard();
    let dir = scratch();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "password_flow_child", "--nocapture"])
        .env("ARCA_PASSWORD_SCAN_DIR", &dir)
        .output()
        .unwrap();
    let password = fs::read(dir.join("scan-password")).unwrap();
    for bytes in [
        &output.stdout,
        &output.stderr,
        &fs::read(dir.join("scanned-transcript")).unwrap(),
        &fs::read(dir.join("desired.json")).unwrap(),
        &fs::read(dir.join("journal.jsonl")).unwrap(),
    ] {
        assert!(!bytes.windows(password.len()).any(|w| w == password));
    }
    assert!(
        output.status.success(),
        "child failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn missing_consumer_address_never_broadens_the_acl() {
    let r = Rig::new(scratch(), false);
    r.create("db1");
    // Creates now start, so explicitly model a consumer without a usable lease.
    let state = r.get(&format!(
        "/1.0/instances/consumer-a/state?project={PROJECT}"
    ));
    r.fake.set_lease(
        BRIDGE,
        state["network"]["eth0"]["hwaddr"].as_str().unwrap(),
        "0.0.0.0",
    );
    r.call(
        "postgres_link",
        json!({"name":"db1","consumer":"consumer-a"}),
    )
    .unwrap();
    assert_eq!(
        r.get("/1.0/network-acls/postgres-db1?project=default")["ingress"],
        json!([])
    );
    let writes = r.fake.writes().len();
    let outcomes = r.kernel.lock().unwrap().propose(vec![], NOW).unwrap();
    assert!(outcomes.iter().any(|o| matches!(o,arca_reconcile::Outcome::Failed{entry,why} if entry.0 == "postgres-db1" && why.contains("no IPv4"))));
    assert_eq!(r.fake.writes().len(), writes);
}
#[test]
fn probe_add_and_remove_use_default_profile_pool_and_refuse_missing_configuration() {
    let _store = store_guard();
    let dir = scratch();
    let path = dir.join("config.json");
    let mut cfg = json!({"stateDir":dir,"incus":{"socket":dir.join("absent.sock")},"listen":{"unix":dir.join("arca.sock")},"tokens":{"hashDir":dir.join("tokens")},"kernel":{"projects":[PROJECT],"droiddProjects":["other"]}});
    let call = |action: &str| {
        let args: Vec<_> = [
            "probe",
            "consumer",
            action,
            "--name",
            "consumer-a",
            "--image",
            &"a".repeat(64),
            "--config",
            path.to_str().unwrap(),
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        arca::run(&args)
    };
    fs::write(&path, cfg.to_string()).unwrap();
    assert!(call("add").is_err());
    assert!(!dir.join("desired.json").exists());
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
    .unwrap();
    cfg["incus"]["socket"] = json!(fake.socket());
    cfg["kernel"]["serviceCidr"] = json!("203.0.113.0/24");
    fs::write(&path, cfg.to_string()).unwrap();
    let error = call("add").unwrap_err();
    assert!(error.message().contains("root"), "{error:?}");
    assert!(!dir.join("desired.json").exists());
    fake.add_profile("default",json!({"name":"default","config":{},"devices":{"root":{"type":"disk","pool":"pool1","path":"/"}}}));
    assert_eq!(
        call("add").unwrap().unwrap(),
        json!({"entry":"consumer-consumer-a","seq":1})
    );
    assert_eq!(call("remove").unwrap().unwrap()["seq"], 2);
    let before = fake.requests().len();
    assert_eq!(call("add").unwrap().unwrap()["seq"], 3);
    assert_eq!(fake.requests().len() - before, 1);
    assert!(fake.writes().is_empty());
    let store = arca::desired_store::DesiredStore::open(
        &dir,
        NodeKey::load_or_create(&dir.join("node.key")).unwrap(),
    )
    .unwrap();
    assert_eq!(store.current().unwrap().document.entries.len(), 2);
    assert_eq!(
        store.current().unwrap().document.entries[1].config["pool"],
        "pool1"
    );
    assert_eq!(
        store.current().unwrap().document.entries[0].id.0,
        "net-arcak-svc"
    );
    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn root_disks_use_default_profile_pool_and_missing_pool_refuses_create() {
    let r = Rig::new(scratch(), false);
    assert_eq!(
        r.get("/1.0/instances/consumer-a?project=arcak-svc")["devices"]["root"],
        json!({"type":"disk","pool":"pool1","path":"/"})
    );
    let before = r
        .fake
        .requests()
        .iter()
        .filter(|q| q.path == "/1.0/profiles/default?project=default")
        .count();
    r.create("db1");
    assert_eq!(
        r.fake
            .requests()
            .iter()
            .filter(|q| q.path == "/1.0/profiles/default?project=default")
            .count()
            - before,
        1
    );
    assert_eq!(
        r.get("/1.0/instances/db1?project=arcak-svc")["devices"]["root"],
        json!({"type":"disk","pool":"pool1","path":"/"})
    );
    r.fake.add_profile(
        "default",
        json!({"name":"default","config":{},"devices":{}}),
    );
    let writes = r.fake.writes().len();
    let error = r
        .call(
            "postgres_create",
            json!({"name":"db2","image":format!("example/db@sha256:{}","b".repeat(64))}),
        )
        .unwrap_err();
    assert!(matches!(error,CallError::Invalid(ref s) if s.contains("devices.root.pool")));
    assert_eq!(r.fake.writes().len(), writes);
    assert!(!r.dir.join("secrets/postgres-db2").exists());
    assert!(
        !r.kernel
            .lock()
            .unwrap()
            .entries()
            .iter()
            .any(|e| e.id.0 == "postgres-db2")
    );
    for bad in ["", "../pool", "pool/name", "pool name"] {
        assert!(!arca_postgres::pool_ok(bad));
    }
}

#[test]
fn create_proposal_failure_cleans_password_and_retry_replaces_a_stale_file() {
    let r = Rig::new(scratch(), false);
    let fault = r.dir.join("desired.json.tmp");
    fs::create_dir(&fault).unwrap();
    let result = r.call(
        "postgres_create",
        json!({"name":"db1","image":format!("example/db@sha256:{}","b".repeat(64))}),
    );
    assert!(matches!(result, Err(CallError::Internal(_))));
    let path = r.dir.join("secrets/postgres-db1/password");
    assert!(
        path.parent().unwrap().is_dir(),
        "secret creation preceded the refused proposal"
    );
    assert!(!path.exists());
    assert!(
        !r.kernel
            .lock()
            .unwrap()
            .entries()
            .iter()
            .any(|e| e.id.0 == "postgres-db1")
    );
    fs::remove_dir(fault).unwrap();
    fs::write(&path, "stale-password").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(r.create("db1")["status"], "pending");
    assert!(fs::read_to_string(&path).unwrap() != "stale-password");
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(r.get("/1.0/instances/db1?project=arcak-svc")["name"], "db1");
}

#[test]
fn partial_create_delete_settles_and_removes_password_url_and_consumer_link() {
    let r = Rig::new(scratch(), true);
    r.fake.set_reject_config_on_post("/1.0/instances");
    r.create("db1");
    assert!(
        UnixIncus::new(r.fake.socket().into())
            .get("/1.0/instances/db1?project=arcak-svc")
            .is_err()
    );
    r.call(
        "postgres_link",
        json!({"name":"db1","consumer":"consumer-a"}),
    )
    .unwrap();
    assert!(r.dir.join("secrets/consumer-consumer-a/db1-url").exists());
    let deleted = r.call("postgres_delete", json!({"name":"db1"})).unwrap();
    assert_eq!(deleted, json!({"removed":true,"plan":null}));
    assert!(!r.dir.join("secrets/postgres-db1").exists());
    assert!(!r.dir.join("secrets/consumer-consumer-a/db1-url").exists());
    let entries = r.kernel.lock().unwrap().entries().to_vec();
    assert_eq!(
        entries
            .iter()
            .find(|e| e.id.0 == "consumer-consumer-a")
            .unwrap()
            .config["links"],
        json!([])
    );
    assert!(!entries.iter().any(|e| e.id.0 == "postgres-db1"));
    assert!(r.kernel.lock().unwrap().pending_plans().is_empty());
    assert!(
        UnixIncus::new(r.fake.socket().into())
            .get("/1.0/network-acls/postgres-db1?project=default")
            .is_err()
    );
}

#[test]
fn live_delete_cleans_all_consumer_links_and_secrets_in_one_document() {
    let r = Rig::new(scratch(), true);
    r.create("db1");
    let other = arca::handlers::service_entry(
        "consumer-consumer-b",
        "arca.consumer",
        json!({"name":"consumer-b","image":"a".repeat(64),"pool":"pool1","project":PROJECT,"bridge":BRIDGE,"links":[]}),
    );
    r.kernel
        .lock()
        .unwrap()
        .propose(vec![arca::desired_store::EntryChange::Put(other)], NOW)
        .unwrap();
    for consumer in ["consumer-a", "consumer-b"] {
        r.call("postgres_link", json!({"name":"db1","consumer":consumer}))
            .unwrap();
    }
    let before: Value =
        serde_json::from_slice(&fs::read(r.dir.join("desired.json")).unwrap()).unwrap();
    let deleted = r.call("postgres_delete", json!({"name":"db1"})).unwrap();
    assert_eq!(deleted["removed"], false);
    assert_eq!(deleted["plan"]["entry"], "postgres-db1");
    assert!(deleted["plan"]["statement"].is_string());
    let after: Value =
        serde_json::from_slice(&fs::read(r.dir.join("desired.json")).unwrap()).unwrap();
    assert_eq!(
        after["current"]["document"]["seq"].as_u64().unwrap(),
        before["current"]["document"]["seq"].as_u64().unwrap() + 1
    );
    assert!(!r.dir.join("secrets/postgres-db1").exists());
    let kernel = r.kernel.lock().unwrap();
    for consumer in ["consumer-a", "consumer-b"] {
        let id = format!("consumer-{consumer}");
        assert_eq!(
            kernel
                .entries()
                .iter()
                .find(|e| e.id.0 == id)
                .unwrap()
                .config["links"],
            json!([])
        );
        assert!(!r.dir.join("secrets").join(id).join("db1-url").exists());
        assert!(
            r.get(&format!("/1.0/instances/{consumer}?project=arcak-svc"))["config"]
                .get("environment.DATABASE_URL")
                .is_none()
        );
    }
    drop(kernel);
    assert_eq!(r.get("/1.0/instances/db1?project=arcak-svc")["name"], "db1");
}

#[test]
fn recreate_with_new_password_replaces_a_stale_consumer_url() {
    let r = Rig::new(scratch(), true);
    r.fake.set_reject_config_on_post("/1.0/instances");
    r.create("db1");
    r.call(
        "postgres_link",
        json!({"name":"db1","consumer":"consumer-a"}),
    )
    .unwrap();
    let path = r.dir.join("secrets/consumer-consumer-a/db1-url");
    let old_url = fs::read_to_string(&path).unwrap();
    let old_password = fs::read_to_string(r.dir.join("secrets/postgres-db1/password")).unwrap();
    assert_eq!(
        r.call("postgres_delete", json!({"name":"db1"})).unwrap()["removed"],
        true
    );
    r.create("db1");
    let password = fs::read_to_string(r.dir.join("secrets/postgres-db1/password")).unwrap();
    assert!(old_password != password);
    fs::write(&path, old_url).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    r.call(
        "postgres_link",
        json!({"name":"db1","consumer":"consumer-a"}),
    )
    .unwrap();
    assert!(
        fs::read_to_string(&path).unwrap()
            == format!("postgresql://postgres:{password}@db1:5432/postgres")
    );
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn link_proposal_failure_restores_previous_url_or_removes_new_url() {
    let r = Rig::new(scratch(), true);
    r.create("db1");
    r.call(
        "postgres_link",
        json!({"name":"db1","consumer":"consumer-a"}),
    )
    .unwrap();
    let path = r.dir.join("secrets/consumer-consumer-a/db1-url");
    fs::write(&path, "previous-url").unwrap();
    let fault = r.dir.join("desired.json.tmp");
    fs::create_dir(&fault).unwrap();
    assert!(
        r.call(
            "postgres_link",
            json!({"name":"db1","consumer":"consumer-a"})
        )
        .is_err()
    );
    assert!(fs::read_to_string(&path).unwrap() == "previous-url");
    fs::remove_dir(&fault).unwrap();
    r.call(
        "postgres_link",
        json!({"name":"db1","consumer":"consumer-a"}),
    )
    .unwrap();
    r.call(
        "postgres_unlink",
        json!({"name":"db1","consumer":"consumer-a"}),
    )
    .unwrap();
    assert!(!path.exists());
    fs::create_dir(&fault).unwrap();
    assert!(
        r.call(
            "postgres_link",
            json!({"name":"db1","consumer":"consumer-a"})
        )
        .is_err()
    );
    assert!(!path.exists());
    fs::remove_dir(fault).unwrap();
}

#[test]
fn postgres_list_releases_kernel_lock_while_instance_state_get_is_stalled() {
    use std::{
        io::Read,
        net::Shutdown,
        os::unix::net::UnixListener,
        thread,
        time::{Duration, Instant},
    };
    let r = Rig::new(scratch(), false);
    r.create("db1");
    let path = r.dir.join("stalled.sock");
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut cfg = r.cfg.clone();
    cfg.incus.socket = path;
    let handler = postgres_handlers(r.kernel.clone(), Arc::new(|| NOW), Arc::new(cfg))
        .into_iter()
        .find(|(name, _)| *name == "postgres_list")
        .unwrap()
        .1;
    let call = thread::spawn(move || {
        handler.call(
            json!({}),
            &Caller {
                token_id: "viewer".into(),
                scope: Scope::Viewer,
            },
        )
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(5))
            }
            Err(error) => panic!("state GET did not connect: {error}"),
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let mut request = [0u8; 1024];
    assert!(stream.read(&mut request).unwrap() > 0);
    let stalled = !call.is_finished();
    let deadline = Instant::now() + Duration::from_millis(300);
    let available = loop {
        if r.kernel.try_lock().is_ok() {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        thread::sleep(Duration::from_millis(5));
    };
    stream.shutdown(Shutdown::Both).unwrap();
    drop(stream);
    drop(listener);
    let result = call.join().unwrap();
    assert!(
        stalled,
        "GET must still be waiting for the unanswered response"
    );
    assert!(
        available,
        "an unanswered state GET must not hold the kernel mutex"
    );
    assert!(matches!(result, Err(CallError::Internal(_))));
}
