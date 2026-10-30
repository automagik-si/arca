//! Passkey approvals through `approval_submit`, in-process against the kernel: the toy plugin on the fake Incus, and the
//! owner keys the kernel holds, with a software P-256 passkey from a fixed seed enrolled through a scratch passkeys file. Each
//! refusal names its variant in the call's refusal code and deletes nothing.

mod common;
#[path = "../../arca-approval/tests/common/webauthn.rs"]
mod webauthn;

use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use arca::desired_store::EntryChange;
use arca::kernel::Kernel;
use arca_kernel::desired::EntryId;
use common::*;
use serde_json::{Value, json};
use webauthn::{Assert, SoftPasskey, UP, passkey_approval};

/// A kernel with one passkey enrolled, and its plan verbs, over a fresh fake.
struct Rig {
    _scratch: Scratch,
    fake: arca_incus::FakeIncus,
    kernel: Arc<Mutex<Kernel>>,
    handlers: std::collections::HashMap<&'static str, Arc<dyn arca_mcp::ToolHandler>>,
}

impl Rig {
    fn new(tag: &str, passkey: &SoftPasskey) -> Rig {
        let scratch = Scratch::new(tag);
        let fake = start_fake(&scratch);
        let lines = format!("{}\n", passkey.line(NAMESPACE));
        let verifier = verifier_with_passkeys(&scratch, &[], &lines, tag);
        let kernel = Arc::new(Mutex::new(
            kernel(&scratch, &fake, verifier, NOW).expect("the kernel opens"),
        ));
        let handlers = plan_verbs(Arc::clone(&kernel), Arc::new(AtomicU64::new(NOW)));
        Rig {
            _scratch: scratch,
            fake,
            kernel,
            handlers,
        }
    }

    /// The toy entry is applied, then removed, so its instance waits for an approval. Returns the plan and its statement.
    fn waiting_plan(&self) -> (String, String) {
        let mut kernel = self.kernel.lock().expect("the kernel lock");
        kernel
            .propose(vec![EntryChange::Put(toy_entry("t1"))], NOW)
            .expect("applied");
        kernel
            .propose(
                vec![EntryChange::Remove(EntryId("toy-scratch".to_string()))],
                NOW + 1_000,
            )
            .expect("removed");
        drop(kernel);
        let list = call_verb(&self.handlers, "plan_list", json!({})).expect("plan_list answers");
        let plan = &list["plans"][0];
        (
            plan["plan"].as_str().expect("a digest").to_string(),
            plan["statement"].as_str().expect("a statement").to_string(),
        )
    }

    fn submit(&self, plan: &str, approval: &str) -> Result<Value, arca_mcp::CallError> {
        call_verb(
            &self.handlers,
            "approval_submit",
            json!({ "plan": plan, "approval": approval }),
        )
    }
}

fn approve(key: &SoftPasskey, statement: &str, bend: impl FnOnce(&mut Assert)) -> String {
    let mut a = Assert::over(NAMESPACE, statement.as_bytes());
    bend(&mut a);
    passkey_approval(statement, &key.assert(&a))
}

/// How a test bends a genuine assertion.
type Bend = Box<dyn FnOnce(&mut Assert)>;

fn refusal_code(error: &arca_mcp::CallError) -> &str {
    match error {
        arca_mcp::CallError::Refused { code, .. } => code.as_str(),
        other => panic!("a refusal was expected, got {other:?}"),
    }
}

#[test]
fn a_passkey_approval_over_the_listed_statement_deletes_the_instance_once() {
    let key = SoftPasskey::from_seed(1);
    let rig = Rig::new("pk-approve", &key);
    let (plan, statement) = rig.waiting_plan();
    let approval = approve(&key, &statement, |a| a.sign_count = 5);

    let result = rig
        .submit(&plan, &approval)
        .expect("the passkey approval deletes");
    assert_eq!(result["plan"], json!(plan));
    assert_eq!(
        deletes_for(&rig.fake, "toy-box-t1"),
        1,
        "one DELETE was sent"
    );
    assert!(instances_marked(&rig.fake, MARK, "toy-scratch").is_empty());

    let replay = rig
        .submit(&plan, &approval)
        .expect_err("the replay is refused");
    assert_eq!(refusal_code(&replay), "SignCountRegressed");
    assert_eq!(deletes_for(&rig.fake, "toy-box-t1"), 1, "still one DELETE");
}

#[test]
fn each_passkey_refusal_is_named_in_the_refusal_code_and_deletes_nothing() {
    let key = SoftPasskey::from_seed(1);
    let cases: Vec<(&str, Bend)> = vec![
        (
            "NoUserVerification",
            Box::new(|a: &mut Assert| a.flags = UP),
        ),
        (
            "WrongOrigin",
            Box::new(|a: &mut Assert| a.origin = Some("https://box1.example.com:9443".to_string())),
        ),
        (
            "WrongRp",
            Box::new(|a: &mut Assert| a.rp_id = Some("box2.example.com".to_string())),
        ),
    ];
    for (code, bend) in cases {
        let rig = Rig::new(&format!("pk-code-{code}"), &key);
        let (plan, statement) = rig.waiting_plan();
        let mut a = Assert::over(NAMESPACE, statement.as_bytes());
        bend(&mut a);
        let error = rig
            .submit(&plan, &passkey_approval(&statement, &key.assert(&a)))
            .expect_err("the assertion is refused");
        assert_eq!(refusal_code(&error), code);
        assert_eq!(
            deletes_for(&rig.fake, "toy-box-t1"),
            0,
            "{code}: nothing deleted"
        );
        rig.submit(&plan, &approve(&key, &statement, |_| {}))
            .unwrap_or_else(|e| {
                panic!("{code}: the nonce is unspent, so the genuine approval deletes: {e:?}")
            });
    }
}
