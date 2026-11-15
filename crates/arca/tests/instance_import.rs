//! `instance_import` and `instance_import_undo` through the proposal, the owner's approval and the run, against the fake
//! Incus with a fake disk copy: nothing runs without an approved card, the run makes what the card said, and the undo
//! takes it out again.

mod common;

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use arca::handlers::{ActionExecutor, plan_handlers};
use arca::kernel::Kernel;
use arca::vm_import::{ImportBackend, handlers as import_handlers};
use arca_mcp::{CallError, ToolHandler};
use arca_vmimport::{FakeCopy, Probe};
use common::*;
use serde_json::{Value, json};

const UEFI: &str = include_str!("../../arca-vmimport/tests/fixtures/uefi-two-disks-sg.xml");

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
    let handlers: HashMap<&'static str, Arc<dyn ToolHandler>> = plan_handlers(
        Arc::clone(&kernel),
        Arc::new(move || clock.load(Ordering::SeqCst)),
        None,
        None,
    )
    .into_iter()
    .collect();
    let copy = Arc::new(FakeCopy::default());
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
