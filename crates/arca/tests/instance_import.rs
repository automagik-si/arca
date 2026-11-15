//! `instance_import` and `instance_import_undo` through the proposal, the owner's approval and the run, against the fake
//! Incus with a fake disk copy: nothing runs without an approved card, the run makes what the card said, and the undo
//! takes it out again.

mod common;

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use arca::action_proposal::ActionKind;
use arca::config::NodeConfig;
use arca::handlers::{ActionExecutor, ActionSlot, plan_handlers_served};
use arca::kernel::Kernel;
use arca::vm_import::{ImportBackend, handlers as import_handlers};
use arca_mcp::{CallError, ToolHandler};
use arca_vmimport::{FakeCopy, Probe};
use common::*;
use serde_json::{Value, json};

const UEFI: &str = include_str!("../../arca-vmimport/tests/fixtures/uefi-two-disks-sg.xml");
const BIOS: &str = include_str!("../../arca-vmimport/tests/fixtures/bios-one-disk.xml");

struct Answers;

impl Probe for Answers {
    fn port_open(&self, ip: Ipv4Addr, port: u16) -> bool {
        ip == Ipv4Addr::new(192, 0, 2, 21) && port == 22
    }
    fn pause(&self) {}
}

struct Rig {
    _scratch: Scratch,
    fake: arca_incus::FakeIncus,
    kernel: Arc<Mutex<Kernel>>,
    handlers: HashMap<&'static str, Arc<dyn ToolHandler>>,
    executor: ActionExecutor,
    copy: Arc<FakeCopy>,
}

fn rig(tag: &str, key: &SkKey) -> Rig {
    let scratch = Scratch::new(tag);
    let mut state = fake_state();
    state.projects.push(json!({ "name": "people", "description": "", "config": { "features.storage.volumes": "true" } }));
    state.pools.push(("remote".to_string(), json!({})));
    state.networks.push(json!({ "name": "br-lan", "project": "default", "managed": true, "type": "bridge", "config": {} }));
    let fake = arca_incus::FakeIncus::start(&scratch.dir, state).expect("the fake serves");
    fake.set_pool_driver("remote", "ceph");
    let verifier = verifier_for(&scratch, &[key], tag);
    let kernel = Arc::new(Mutex::new(
        kernel_with_image(&scratch, &fake, verifier, NOW, None).expect("the kernel opens"),
    ));
    let clock = Arc::new(AtomicU64::new(NOW));
    // The node reads the source pool on a Ceph cluster of its own, `source-b`; the Incus pool is on `ceph`.
    let node: NodeConfig = serde_json::from_value(json!({
        "stateDir": scratch.dir.join("node-state"),
        "incus": { "socket": fake.socket() },
        "listen": { "unix": scratch.dir.join("arca.sock") },
        "tokens": { "hashDir": scratch.dir.join("tokens") },
        "kernel": {
            "projects": ["arcak-svc"],
            "droiddProjects": ["people"],
            "instanceImport": { "sourcePool": "src-pool", "cephCluster": "source-b" }
        }
    }))
    .expect("the node config");
    let handlers: HashMap<&'static str, Arc<dyn ToolHandler>> = plan_handlers_served(
        Arc::clone(&kernel),
        Arc::new(move || clock.load(Ordering::SeqCst)),
        None,
        None,
        None,
        None,
        ActionSlot::default(),
        Some(Arc::new(node)),
    )
    .into_iter()
    .collect();
    let copy = Arc::new(FakeCopy::on_cluster("source-b"));
    copy.target(
        "remote/virtual-machine_people_web-1.block",
        30_720 * 1024 * 1024,
    );
    copy.target(
        "remote/custom_people_web-1-disk1.block",
        163_840 * 1024 * 1024,
    );
    let backend = ImportBackend::new(
        fake.socket().to_path_buf(),
        "src-pool",
        Arc::clone(&copy) as Arc<dyn arca_vmimport::DiskCopy>,
        Arc::new(Answers),
    );
    let executor = ActionExecutor::new(import_handlers(
        Arc::clone(&kernel),
        Some(Arc::new(backend)),
    ));
    Rig {
        _scratch: scratch,
        fake,
        kernel,
        handlers,
        executor,
        copy,
    }
}

impl Rig {
    fn propose(&self, action: &str, args: Value) -> Result<Value, CallError> {
        call_verb(
            &self.handlers,
            "action_propose",
            json!({ "action": action, "args": args }),
        )
    }

    /// The owner approves the proposal and the approved action runs once.
    fn approve_and_run(
        &self,
        key: &SkKey,
        proposal: &Value,
        nonce: char,
    ) -> Result<Value, CallError> {
        let id = proposal["id"].as_str().expect("an id");
        let digest = proposal["argsDigest"].as_str().expect("a digest");
        let action = proposal["action"].as_str().expect("an action");
        let statement = arca::handlers::action_statement(
            HOST,
            id,
            action,
            digest,
            NOW,
            &nonce.to_string().repeat(32),
        )
        .expect("a statement");
        let approval = approve_statement(key, &statement);
        let approved = {
            let mut kernel = self.kernel.lock().expect("the kernel lock");
            arca::handlers::approve_action(
                &mut kernel,
                &json!({ "plan": id, "approval": approval }),
                NOW,
            )
            .expect("the approval verifies")
        };
        self.executor
            .run(&approved.action, &approved.args, &approved.grant)
    }

    fn writes(&self) -> Vec<String> {
        self.fake
            .requests()
            .into_iter()
            .filter(|request| request.method != "GET")
            .map(|request| format!("{} {}", request.method, request.path))
            .collect()
    }
}

fn import_args() -> Value {
    json!({ "name": "web-1", "project": "people", "pool": "remote", "bridge": "br-lan", "source": UEFI })
}

#[test]
fn an_approved_import_makes_the_vm_copies_the_disks_and_reads_back_and_the_undo_removes_it() {
    let key = SkKey::from_seed(41);
    let rig = rig("import-run", &key);
    let proposal = rig
        .propose("instance_import", import_args())
        .expect("proposed");
    assert_eq!(
        proposal["summary"],
        "Move the VM build & test (id 21) from the outside hypervisor into the instance web-1 in project people"
    );
    assert!(rig.writes().is_empty(), "a proposal changes nothing");
    // The card names the cluster the disks are read from and the one they are written to, after the last disk.
    let rows: Vec<(&str, &str)> = proposal["details"]
        .as_array()
        .expect("the card's rows")
        .iter()
        .map(|row| {
            (
                row["label"].as_str().unwrap_or_default(),
                row["value"].as_str().unwrap_or_default(),
            )
        })
        .collect();
    let at = rows
        .iter()
        .position(|(label, _)| *label == "Ceph")
        .expect("a Ceph row");
    assert_eq!(rows[at - 1].0, "Disk 2");
    assert_eq!(
        rows[at].1,
        "Read from the Ceph cluster source-b (the pool src-pool, as client.arca-import, which may only read), and copied to the Ceph cluster ceph, into the Incus pool remote. The source cluster is never written"
    );

    // A direct call without the owner's approval runs nothing.
    let refused = rig
        .executor
        .run("instance_import", &import_args(), "not-a-grant")
        .expect_err("no approval");
    assert!(matches!(refused, CallError::Refused { ref code, .. } if code == "NotApproved"));
    assert!(rig.writes().is_empty());

    let result = rig
        .approve_and_run(&key, &proposal, 'a')
        .expect("the import runs");
    assert_eq!(result["instance"], "web-1");
    assert_eq!(result["acls"], json!(["arca-import-web-1"]));
    assert_eq!(result["volumes"], json!(["web-1-disk1"]));
    assert_eq!(result["readBack"]["answered"], true);
    assert_eq!(
        result["ceph"],
        json!({
            "from": "source-b",
            "to": "ceph",
            "copied": "Copied from the Ceph cluster source-b (the pool src-pool, read only) to the Ceph cluster ceph, into the Incus pool remote (written as client.admin)"
        })
    );
    assert_eq!(
        rig.copy.copied(),
        [
            (
                "src-pool/build-root".to_string(),
                "remote/virtual-machine_people_web-1.block".to_string()
            ),
            (
                "src-pool/build-data".to_string(),
                "remote/custom_people_web-1-disk1.block".to_string()
            ),
        ]
    );
    let vm = get(&rig.fake, "/1.0/instances/web-1?project=people").expect("the VM exists");
    assert_eq!(vm["status"], "Running");
    assert_eq!(vm["config"]["limits.cpu"], "4");
    assert_eq!(vm["devices"]["eth0"]["hwaddr"], "02:00:c0:00:02:15");
    assert_eq!(vm["devices"]["eth0"]["security.acls"], "arca-import-web-1");
    assert!(
        get(
            &rig.fake,
            "/1.0/network-acls/arca-import-web-1?project=default"
        )
        .is_some()
    );

    let undo = rig
        .propose(
            "instance_import_undo",
            json!({ "project": "people", "name": "web-1" }),
        )
        .expect("proposed");
    let removed = rig
        .approve_and_run(&key, &undo, 'b')
        .expect("the undo runs");
    assert_eq!(
        removed["removed"],
        json!([
            "the instance web-1",
            "the volume web-1-disk1",
            "the ACL arca-import-web-1"
        ])
    );
    assert!(get(&rig.fake, "/1.0/instances/web-1?project=people").is_none());
    assert!(
        get(
            &rig.fake,
            "/1.0/network-acls/arca-import-web-1?project=default"
        )
        .is_none()
    );
}

#[test]
fn a_running_source_stops_the_approved_import_before_anything_is_made() {
    let key = SkKey::from_seed(42);
    let rig = rig("import-watched", &key);
    rig.copy.watch("src-pool/build-root", "192.0.2.200:0/7");
    let proposal = rig
        .propose("instance_import", import_args())
        .expect("proposed");
    let failure = rig
        .approve_and_run(&key, &proposal, 'c')
        .expect_err("the source runs");
    let CallError::Refused { code, message } = failure else {
        panic!("a refusal");
    };
    assert_eq!(code, "InstanceImportFailed");
    assert!(message.contains("Power the source off"), "{message}");
    assert!(rig.writes().is_empty());
}

#[test]
fn a_description_arca_cannot_move_is_refused_when_it_is_proposed() {
    let key = SkKey::from_seed(43);
    let rig = rig("import-refused", &key);
    let mut bad = import_args();
    bad["source"] = json!(UEFI.replace(
        "<TM_MAD><![CDATA[ceph]]></TM_MAD>",
        "<TM_MAD>qcow2</TM_MAD>"
    ));
    assert!(
        matches!(rig.propose("instance_import", bad), Err(CallError::Invalid(why)) if why.contains("no Ceph disk"))
    );
    let mut no_bridge = import_args();
    no_bridge.as_object_mut().expect("args").remove("bridge");
    assert!(rig.propose("instance_import", no_bridge).is_err());
}

/// Every file under `dir`, read as text where it is text.
fn files_text(dir: &std::path::Path) -> String {
    let mut text = String::new();
    for entry in std::fs::read_dir(dir).expect("the scratch dir").flatten() {
        let path = entry.path();
        if path.is_dir() {
            text.push_str(&files_text(&path));
        } else if let Ok(read) = std::fs::read(&path) {
            text.push_str(&String::from_utf8_lossy(&read));
        }
    }
    text
}

#[test]
fn a_proposal_keeps_no_context_secret_in_the_journal_the_answer_or_plan_list() {
    let key = SkKey::from_seed(44);
    let rig = rig("import-secrets", &key);
    let secrets = ["ZWNobyBoaQ==", "not-a-real-password"];
    assert!(
        UEFI.contains(secrets[0]) && BIOS.contains(secrets[1]),
        "the fixtures hold the secrets"
    );
    let mut bios = import_args();
    bios["source"] = json!(BIOS);
    bios["name"] = json!("web-2");
    let answers = [
        rig.propose("instance_import", import_args())
            .expect("proposed"),
        rig.propose("instance_import", bios).expect("proposed"),
    ];
    let listed = call_verb(&rig.handlers, "plan_list", json!({})).expect("listed");
    let journal = files_text(&rig._scratch.dir);
    assert!(
        journal.contains("START_SCRIPT_BASE64"),
        "the journal names the key the seed does not carry"
    );
    for secret in secrets {
        for answer in &answers {
            assert!(
                !answer.to_string().contains(secret),
                "{secret} in the propose answer"
            );
        }
        assert!(
            !listed.to_string().contains(secret),
            "{secret} in plan_list"
        );
        assert!(
            !journal.contains(secret),
            "{secret} in a file the kernel wrote"
        );
    }
    // The run prepares the stored args again: the same args, so the approval binds the same digest.
    let stored = rig
        .kernel
        .lock()
        .expect("the kernel lock")
        .action_proposals()[0]
        .args
        .clone();
    let again = ActionKind::InstanceImport
        .prepare(&stored, None)
        .expect("prepared");
    assert_eq!(again, stored);
    assert_eq!(
        ActionKind::InstanceImport
            .prepare(&again, None)
            .expect("prepared"),
        again
    );
}

/// One GET to the approval page's router, served on a unix socket as `serve` serves it, with no bearer and no origin.
fn page_get(rig: &Rig, path: &str) -> String {
    use std::io::{Read as _, Write as _};
    // A short directory of its own: the server binds through a staging directory beside the socket, and a unix socket
    // path is at most 107 bytes.
    let short = std::env::temp_dir().join(format!("ii{}", std::process::id()));
    std::fs::create_dir_all(&short).expect("a socket directory");
    let socket = short.join("p");
    let router = arca::approve_page::router(
        "http://localhost:7443".to_string(),
        Arc::clone(&rig.kernel),
        Arc::new(|| NOW),
        rig._scratch.dir.clone(),
    );
    let runtime = tokio::runtime::Runtime::new().expect("a runtime");
    let served = socket.clone();
    runtime.spawn(async move {
        let _ = arca_mcp::serve_unix(router, &served).await;
    });
    let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !socket.exists() {
        assert!(std::time::Instant::now() < until, "the page started");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let mut stream = std::os::unix::net::UnixStream::connect(&socket).expect("the page answers");
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .expect("sent");
    let mut answer = String::new();
    stream.read_to_string(&mut answer).expect("answered");
    let _ = std::fs::remove_dir_all(&short);
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    answer
}

#[test]
fn the_user_data_is_shown_nowhere_and_still_seeds_the_vm() {
    let key = SkKey::from_seed(45);
    let rig = rig("import-userdata", &key);
    let user_data = "I2Nsb3VkLWNvbmZpZwp0aW1lem9uZTogVVRDCg==";
    assert!(UEFI.contains(user_data), "the fixture holds the user data");
    let proposal = rig
        .propose("instance_import", import_args())
        .expect("proposed");
    let listed = call_verb(&rig.handlers, "plan_list", json!({})).expect("listed");
    let page = page_get(&rig, "/approve/plans");
    for (place, text) in [
        ("the propose answer", proposal.to_string()),
        ("plan_list", listed.to_string()),
        ("/approve/plans", page.clone()),
    ] {
        assert!(!text.contains(user_data), "the user data in {place}");
        assert!(
            text.contains("<USER_DATA/>"),
            "{place} names the user data key"
        );
    }
    // What is shown is not what is bound: the digest is the stored arguments', and the approval of it runs.
    let stored = rig
        .kernel
        .lock()
        .expect("the kernel lock")
        .action_proposals()[0]
        .args
        .clone();
    assert!(
        stored["source"]
            .as_str()
            .expect("a source")
            .contains(user_data)
    );
    assert_eq!(listed["plans"][0]["argsDigest"], proposal["argsDigest"]);
    assert!(page.contains(proposal["argsDigest"].as_str().expect("a digest")));
    rig.approve_and_run(&key, &proposal, 'd')
        .expect("the import runs");
    let vm = get(&rig.fake, "/1.0/instances/web-1?project=people").expect("the VM exists");
    let seeded = vm["config"]["cloud-init.user-data"]
        .as_str()
        .expect("the seed's user data");
    assert!(seeded.contains("timezone: UTC"), "{seeded}");
}
