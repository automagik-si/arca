//! `instance_start`, `instance_stop` and `instance_restart`, run against the fake Incus through the real handlers, the real
//! kernel and the real owner-approval verifier, and through the MCP door with the real verb table. A start runs for a
//! doctor; a stop and a restart run only from an approved action proposal; every one of them acts only on an instance of
//! Arca's, is idempotent, forces nothing and is journalled.

mod common;

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arca::handlers::{ActionExecutor, ActionSlot, Clock, plan_handlers_node};
use arca::kernel::Kernel;
use arca::lifecycle::{INSTANCE_ACTIONS_FILE, InstanceLifecycle};
use arca_api::{Scope, conforms, verb};
use arca_incus::{FakeIncus, ProjectFamily};
use arca_kernel::canon::Digest;
use arca_kernel::desired::{EntryId, PluginId};
use arca_mcp::{CallError, Caller, ToolHandler};
use common::*;
use serde_json::{Value, json};

/// A project outside the kernel's family.
const OUTSIDE: &str = "elsewhere";

/// One instance of the fake: its project, its name, its status and its own config beside its uuid.
fn instance(project: &str, name: &str, status: &str, config: Value) -> Value {
    let mut config = config;
    config["volatile.uuid"] = json!(format!("00000000-0000-4000-8000-{name:0>12}"));
    json!({
        "name": name,
        "project": project,
        "type": "container",
        "status": status,
        "profiles": [],
        "config": config,
        "devices": {},
    })
}

/// A kernel in kernel mode over a fake Incus that holds one instance of every ownership class, and the handlers a served
/// node builds over it.
struct Rig {
    scratch: Scratch,
    fake: FakeIncus,
    kernel: Arc<Mutex<Kernel>>,
    key: SkKey,
    handlers: HashMap<&'static str, Arc<dyn ToolHandler>>,
    executor: ActionExecutor,
}

impl Rig {
    fn new(tag: &str) -> Rig {
        Self::build(tag, true)
    }

    /// `kernel_mode: false` is a watch-only node: it builds no state writer.
    fn build(tag: &str, kernel_mode: bool) -> Rig {
        let scratch = Scratch::new(tag);
        let mut state = fake_state();
        state.projects.push(json!({ "name": OUTSIDE }));
        state.instances.extend([
            // Arca's own: a kernel entry's mark.
            instance(PROJECT, "web", "Stopped", json!({ MARK: "app-web" })),
            instance(PROJECT, "api", "Running", json!({ MARK: "app-api" })),
            // Adopted: the mark an adoption leaves.
            instance(
                PROJECT,
                "legacy",
                "Stopped",
                json!({ MARK: "adopt-legacy" }),
            ),
            // Not Arca's, each for its own reason.
            instance(PROJECT, "stray", "Stopped", json!({})),
            instance(
                PROJECT,
                "phone",
                "Stopped",
                json!({ "user.droid.id": "d1" }),
            ),
            instance(
                PROJECT,
                "both",
                "Stopped",
                json!({ MARK: "app-both", "user.droid.id": "d2" }),
            ),
            instance(PROJECT, "kept", "Stopped", json!({ MARK: "app-kept" })),
            instance(OUTSIDE, "far", "Stopped", json!({ MARK: "app-far" })),
            instance(PROJECT, "paused", "Frozen", json!({ MARK: "app-paused" })),
        ]);
        let fake = FakeIncus::start(&scratch.dir, state).expect("the fake serves");
        let key = SkKey::from_seed(7);
        let kernel = Arc::new(Mutex::new(
            kernel(
                &scratch,
                &fake,
                verifier_for(&scratch, &[&key], "life"),
                NOW,
            )
            .expect("the kernel opens"),
        ));
        let lifecycle = kernel_mode.then(|| {
            Arc::new(InstanceLifecycle::new(
                fake.socket().to_path_buf(),
                ProjectFamily::new([PROJECT], ["droid"]).expect("the family is valid"),
                // The entry of `kept` is retained: its delete plan was declined.
                || {
                    Ok(BTreeMap::from([(
                        EntryId("app-kept".to_string()),
                        (PluginId("toy.scratch".to_string()), Digest::of(b"plan")),
                    )]))
                },
                &scratch.dir,
            ))
        });
        let clock: Clock = Arc::new(|| NOW);
        let handlers: HashMap<&'static str, Arc<dyn ToolHandler>> = plan_handlers_node(
            Arc::clone(&kernel),
            clock,
            None,
            None,
            None,
            lifecycle,
            ActionSlot::default(),
        )
        .into_iter()
        .collect();
        let executor = ActionExecutor::new(
            handlers
                .iter()
                .map(|(verb, handler)| (*verb, Arc::clone(handler))),
        );
        Rig {
            scratch,
            fake,
            kernel,
            key,
            handlers,
            executor,
        }
    }

    /// A direct call, as a token of `scope` makes it.
    fn call(&self, scope: Scope, name: &str, args: Value) -> Result<Value, CallError> {
        let result = self.handlers[name].call(
            args,
            &Caller {
                token_id: "doctor-1".to_string(),
                scope,
                tools: None,
            },
        );
        if let Ok(value) = &result {
            conforms(&(verb(name).expect("in the table").result)(), value)
                .unwrap_or_else(|why| panic!("{name} result does not meet its schema: {why}"));
        }
        result
    }

    fn status(&self, project: &str, name: &str) -> String {
        get(
            &self.fake,
            &format!("/1.0/instances/{name}?project={project}"),
        )
        .expect("the instance is there")["status"]
            .as_str()
            .expect("a status")
            .to_string()
    }

    /// How many state changes the fake received, refused ones included.
    fn state_writes(&self) -> usize {
        self.fake
            .requests()
            .iter()
            .filter(|request| request.method == "PUT" && request.path.contains("/state"))
            .count()
    }

    fn journal(&self) -> Vec<Value> {
        match fs::read_to_string(self.scratch.path(INSTANCE_ACTIONS_FILE)) {
            Ok(text) => text
                .lines()
                .map(|line| serde_json::from_str(line).expect("a JSON line"))
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Proposes `action` as a viewer, has the owner approve it, and runs it once through the executor.
    fn approved(&self, action: &str, args: Value) -> Result<Value, CallError> {
        let proposal = self.call(
            Scope::Viewer,
            "action_propose",
            json!({ "action": action, "args": args }),
        )?;
        let id = proposal["id"].as_str().expect("an id").to_string();
        let statement = arca::handlers::action_statement(
            HOST,
            &id,
            action,
            proposal["argsDigest"].as_str().expect("a digest"),
            NOW,
            &Digest::of(format!("{id}{}", self.state_writes()).as_bytes()).to_hex()[..32],
        )
        .expect("the statement");
        let approval = approve_statement(&self.key, &statement);
        arca::handlers::run_approved_action(
            &self.kernel,
            &self.executor,
            &json!({ "plan": id, "approval": approval }),
            NOW,
        )
        .map(|answer| answer["result"].clone())
    }
}

fn refusal(result: Result<Value, CallError>) -> (String, String) {
    match result {
        Err(CallError::Refused { code, message }) => (code, message),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn at(project: &str, name: &str) -> Value {
    json!({ "project": project, "instance": name })
}

#[test]
fn a_doctor_starts_a_stopped_instance_of_arcas_and_a_second_start_says_it_already_runs() {
    let rig = Rig::new("life-start");
    let started = rig
        .call(Scope::Doctor, "instance_start", at(PROJECT, "web"))
        .expect("a doctor starts Arca's instance with no approval");
    assert_eq!(
        started,
        json!({
            "project": PROJECT,
            "instance": "web",
            "action": "start",
            "status": "Running",
            "changed": true,
            "message": format!("started the instance web in project {PROJECT}"),
        })
    );
    assert_eq!(rig.status(PROJECT, "web"), "Running");

    let again = rig
        .call(Scope::Doctor, "instance_start", at(PROJECT, "web"))
        .expect("a start of a running instance succeeds");
    assert_eq!(again["changed"], json!(false));
    assert_eq!(again["status"], json!("Running"));
    assert_eq!(
        again["message"],
        json!(format!(
            "the instance web in project {PROJECT} is already running; nothing was started"
        ))
    );
    assert_eq!(rig.state_writes(), 1, "the second start sent nothing");

    // Each action is journalled: a begin before anything is sent, and an end with what happened.
    let journal = rig.journal();
    assert_eq!(journal.len(), 4, "{journal:?}");
    assert_eq!(journal[0]["kind"], json!("begin"));
    assert_eq!(journal[0]["action"], json!("start"));
    assert_eq!(journal[0]["project"], json!(PROJECT));
    assert_eq!(journal[0]["instance"], json!("web"));
    assert_eq!(journal[0]["token_id"], json!("doctor-1"));
    assert_eq!(journal[0]["proposal"], Value::Null, "a direct start");
    assert_eq!(journal[0]["at_ms"], json!(NOW));
    assert_eq!(journal[1]["kind"], json!("end"));
    assert_eq!(journal[1]["id"], journal[0]["id"]);
    assert_eq!(journal[1]["outcome"], json!("started"));
    assert_eq!(journal[1]["status"], json!("Running"));
    assert!(journal[1]["uuid"].is_string());
    assert_eq!(journal[3]["outcome"], json!("already_running"));
    assert_ne!(journal[2]["id"], journal[0]["id"]);
    let mode = fs::metadata(rig.scratch.path(INSTANCE_ACTIONS_FILE))
        .expect("the journal")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "the journal is private");
}

#[test]
fn an_adopted_instance_is_arcas_too() {
    let rig = Rig::new("life-adopted");
    let started = rig
        .call(Scope::Doctor, "instance_start", at(PROJECT, "legacy"))
        .expect("an adopted instance starts");
    assert_eq!(started["changed"], json!(true));
    assert_eq!(rig.status(PROJECT, "legacy"), "Running");
}

#[test]
fn an_instance_that_is_not_arcas_is_refused_in_plain_words_and_nothing_is_sent_to_it() {
    let rig = Rig::new("life-refused");
    let cases = [
        (
            PROJECT,
            "stray",
            "InstanceNotArca",
            "it carries no mark of Arca's",
        ),
        (PROJECT, "phone", "InstanceNotArca", "it is droidd's"),
        (
            PROJECT,
            "both",
            "InstanceNotArca",
            "both droidd and Arca mark it",
        ),
        (
            PROJECT,
            "kept",
            "InstanceNotArca",
            "its delete plan was declined",
        ),
        (
            OUTSIDE,
            "far",
            "InstanceNotArca",
            "its project is outside the kernel's project family",
        ),
        (
            PROJECT,
            "ghost",
            "InstanceUnknown",
            "no instance named ghost exists",
        ),
    ];
    for (project, name, code, words) in cases {
        let (got, message) = refusal(rig.call(Scope::Doctor, "instance_start", at(project, name)));
        assert_eq!(got, code, "{name}: {message}");
        assert!(message.contains(words), "{name}: {message}");
        if code == "InstanceNotArca" {
            assert!(
                message.contains(&format!(
                    "the instance {name} in project {project} is not Arca's"
                )) && message.contains("only an instance it owns or has adopted"),
                "{name}: {message}"
            );
            assert_eq!(rig.status(project, name), "Stopped", "{name} is left alone");
        }
        // The same instance cannot even be proposed, so the owner is never shown a card for it.
        for action in ["instance_start", "instance_stop", "instance_restart"] {
            let (got, _) = refusal(rig.call(
                Scope::Viewer,
                "action_propose",
                json!({ "action": action, "args": at(project, name) }),
            ));
            assert_eq!(got, code, "{action} {name}");
        }
    }
    assert_eq!(rig.state_writes(), 0, "no state change was sent");
    assert_eq!(
        rig.call(Scope::Viewer, "plan_list", json!({}))
            .expect("plan_list")["plans"],
        json!([]),
        "no refused proposal was recorded"
    );
    // Each refused call is journalled with its code.
    let ends: Vec<Value> = rig
        .journal()
        .into_iter()
        .filter(|row| row["kind"] == "end")
        .collect();
    assert_eq!(ends.len(), 6);
    assert!(ends.iter().all(|row| row["outcome"] == "refused"));
    assert_eq!(ends[0]["code"], json!("InstanceNotArca"));
    assert_eq!(ends[5]["code"], json!("InstanceUnknown"));

    // A frozen instance of Arca's is neither running nor stopped: it is left as it is.
    let (code, message) = refusal(rig.call(Scope::Doctor, "instance_start", at(PROJECT, "paused")));
    assert_eq!(code, "InstanceStateUnsupported");
    assert!(message.contains("is frozen"), "{message}");
    assert_eq!(rig.state_writes(), 0);
}

#[test]
fn a_viewer_may_not_start_and_a_malformed_call_is_invalid() {
    let rig = Rig::new("life-scope");
    assert!(matches!(
        rig.call(Scope::Viewer, "instance_start", at(PROJECT, "web")),
        Err(CallError::Forbidden { .. })
    ));
    for args in [
        json!({ "project": PROJECT }),
        json!({ "project": PROJECT, "instance": "web", "force": true }),
        json!({ "project": "../x", "instance": "web" }),
        json!({ "project": PROJECT, "instance": "web/state" }),
        // A start takes no approval: the field is not in its schema.
        json!({ "project": PROJECT, "instance": "web", "approval": "x" }),
    ] {
        assert!(
            matches!(
                rig.call(Scope::Doctor, "instance_start", args.clone()),
                Err(CallError::Invalid(_))
            ),
            "{args}"
        );
    }
    assert_eq!(rig.status(PROJECT, "web"), "Stopped");
    assert_eq!(rig.state_writes(), 0);
    assert!(rig.journal().is_empty(), "nothing passed the gate");
}

#[test]
fn a_stop_and_a_restart_run_only_from_an_approved_proposal() {
    let rig = Rig::new("life-gate");
    for name in ["instance_stop", "instance_restart"] {
        // No approval at all is an invalid call; one that no approved proposal armed is refused.
        assert!(matches!(
            rig.call(Scope::Doctor, name, at(PROJECT, "api")),
            Err(CallError::Invalid(_))
        ));
        for scope in [Scope::Doctor, Scope::Operator] {
            let mut args = at(PROJECT, "api");
            args["approval"] = json!("not-a-grant");
            let (code, message) = refusal(rig.call(scope, name, args));
            assert_eq!(code, "NotApproved");
            assert!(
                message.contains("runs only from an approved proposal"),
                "{message}"
            );
        }
    }
    assert_eq!(rig.status(PROJECT, "api"), "Running");
    assert_eq!(rig.state_writes(), 0);
    assert!(
        rig.journal().is_empty(),
        "a call without a grant did nothing"
    );

    // A viewer proposes the stop: the card says what it does, and nothing changes yet.
    let proposal = rig
        .call(
            Scope::Viewer,
            "action_propose",
            json!({ "action": "instance_stop", "args": at(PROJECT, "api") }),
        )
        .expect("a viewer may propose a stop");
    assert_eq!(
        proposal["summary"],
        json!(format!("Stop the instance api in project {PROJECT}"))
    );
    assert_eq!(proposal["args"], at(PROJECT, "api"));
    let labels: Vec<&str> = proposal["details"]
        .as_array()
        .expect("details")
        .iter()
        .map(|row| row["label"].as_str().expect("a label"))
        .collect();
    assert_eq!(labels, ["Project", "Instance", "Effect"]);
    assert_eq!(rig.status(PROJECT, "api"), "Running", "a proposal is inert");

    // The owner approves it, and it runs once.
    let stopped = rig
        .approved("instance_stop", at(PROJECT, "api"))
        .expect("the approved stop runs");
    assert_eq!(
        stopped,
        json!({
            "project": PROJECT,
            "instance": "api",
            "action": "stop",
            "status": "Stopped",
            "changed": true,
            "message": format!("stopped the instance api in project {PROJECT}"),
        })
    );
    assert_eq!(rig.status(PROJECT, "api"), "Stopped");
    let journal = rig.journal();
    assert_eq!(journal[0]["token_id"], json!("owner-approval"));
    assert_eq!(
        journal[0]["proposal"], proposal["id"],
        "the journal names the approved proposal"
    );
    assert_eq!(journal[1]["outcome"], json!("stopped"));

    // A stop of a stopped instance is approved like any other, changes nothing, succeeds and says so.
    let again = rig
        .approved("instance_stop", at(PROJECT, "api"))
        .expect("a stop of a stopped instance succeeds");
    assert_eq!(again["changed"], json!(false));
    assert_eq!(
        again["message"],
        json!(format!(
            "the instance api in project {PROJECT} is already stopped; nothing was stopped"
        ))
    );
    assert_eq!(rig.state_writes(), 1);
    assert_eq!(rig.journal()[3]["outcome"], json!("already_stopped"));
}

#[test]
fn an_approved_grant_runs_its_own_action_on_its_own_instance_once() {
    let rig = Rig::new("life-grant");
    let proposal = rig
        .call(
            Scope::Viewer,
            "action_propose",
            json!({ "action": "instance_stop", "args": at(PROJECT, "api") }),
        )
        .expect("the proposal");
    let id = proposal["id"].as_str().expect("an id");
    let statement = arca::handlers::action_statement(
        HOST,
        id,
        "instance_stop",
        proposal["argsDigest"].as_str().expect("a digest"),
        NOW,
        &"5".repeat(32),
    )
    .expect("the statement");
    let approval = approve_statement(&rig.key, &statement);
    let approved = {
        let mut kernel = rig.kernel.lock().expect("the kernel");
        arca::handlers::approve_action(
            &mut kernel,
            &json!({ "plan": id, "approval": approval }),
            NOW,
        )
        .expect("the owner's approval verifies")
    };
    let with_grant = |project: &str, name: &str| {
        let mut args = at(project, name);
        args["approval"] = json!(approved.grant);
        args
    };
    // The grant is bound to the verb and to the instance the owner saw.
    let (code, _) = refusal(rig.call(
        Scope::Doctor,
        "instance_restart",
        with_grant(PROJECT, "api"),
    ));
    assert_eq!(code, "NotApproved");
    let (code, _) = refusal(rig.call(Scope::Doctor, "instance_stop", with_grant(PROJECT, "web")));
    assert_eq!(code, "NotApproved");
    assert_eq!(rig.status(PROJECT, "api"), "Running");

    let stopped = rig
        .call(Scope::Doctor, "instance_stop", with_grant(PROJECT, "api"))
        .expect("the grant runs the stop it was armed for");
    assert_eq!(stopped["changed"], json!(true));
    let (code, _) = refusal(rig.call(Scope::Doctor, "instance_stop", with_grant(PROJECT, "api")));
    assert_eq!(code, "NotApproved", "the grant is spent");
    // The owner's approval itself is spent too.
    let replay = rig
        .kernel
        .lock()
        .expect("the kernel")
        .approve_action(id, &approval, NOW);
    assert!(matches!(
        replay,
        Err(arca::kernel::ActionApprovalError::NoSuch)
    ));
}

#[test]
fn an_approved_restart_restarts_a_running_instance_and_starts_a_stopped_one() {
    let rig = Rig::new("life-restart");
    let restarted = rig
        .approved("instance_restart", at(PROJECT, "api"))
        .expect("the approved restart runs");
    assert_eq!(
        restarted,
        json!({
            "project": PROJECT,
            "instance": "api",
            "action": "restart",
            "status": "Running",
            "changed": true,
            "message": format!("restarted the instance api in project {PROJECT}"),
        })
    );
    let started = rig
        .approved("instance_restart", at(PROJECT, "web"))
        .expect("a restart of a stopped instance starts it");
    assert_eq!(started["action"], json!("restart"));
    assert_eq!(started["changed"], json!(true));
    assert_eq!(
        started["message"],
        json!(format!(
            "the instance web in project {PROJECT} was stopped, so it was started"
        ))
    );
    assert_eq!(rig.status(PROJECT, "web"), "Running");
    let outcomes: Vec<Value> = rig
        .journal()
        .into_iter()
        .filter(|row| row["kind"] == "end")
        .map(|row| row["outcome"].clone())
        .collect();
    assert_eq!(
        outcomes,
        [json!("restarted"), json!("started_from_stopped")]
    );

    // A start may be proposed and approved too, for a requester that holds no doctor token.
    let proposal_start = rig
        .approved("instance_start", at(PROJECT, "legacy"))
        .expect("an approved start runs");
    assert_eq!(proposal_start["changed"], json!(true));
    assert_eq!(rig.status(PROJECT, "legacy"), "Running");
}

#[test]
fn an_instance_that_will_not_shut_down_stays_running_and_nothing_is_forced() {
    let rig = Rig::new("life-hung");
    // Only a forced stop would end it, and Arca sends none.
    rig.fake.hang_stop("api", false);
    for (action, code, word) in [
        ("instance_stop", "InstanceStopFailed", "was not stopped"),
        (
            "instance_restart",
            "InstanceRestartFailed",
            "was not restarted",
        ),
    ] {
        let (got, message) = refusal(rig.approved(action, at(PROJECT, "api")));
        assert_eq!(got, code, "{message}");
        assert!(message.contains(word), "{message}");
        assert!(message.contains("It is running now."), "{message}");
        assert!(message.contains("forces nothing"), "{message}");
        assert_eq!(rig.status(PROJECT, "api"), "Running");
    }
    assert_eq!(
        rig.state_writes(),
        2,
        "one graceful try each, and no second"
    );
    let ends: Vec<Value> = rig
        .journal()
        .into_iter()
        .filter(|row| row["kind"] == "end")
        .collect();
    assert_eq!(ends[0]["outcome"], json!("failed"));
    assert_eq!(ends[0]["code"], json!("InstanceStopFailed"));
    assert_eq!(ends[1]["code"], json!("InstanceRestartFailed"));
}

#[test]
fn an_instance_recreated_after_it_was_read_is_refused_before_any_write() {
    let rig = Rig::new("life-recreated");
    let changer = arca_incus::StateChanger::new(
        rig.fake.socket().to_path_buf(),
        ProjectFamily::new([PROJECT], ["droid"]).expect("the family is valid"),
    );
    let mut seen = changer.read(PROJECT, "api").expect("the instance reads");
    // The instance the caller checked had another uuid: this one was made under its name since.
    seen.uuid = Some("00000000-0000-4000-8000-00000000dead".to_string());
    let refused = changer.change(&seen, arca_incus::StateAction::Stop);
    assert!(
        matches!(refused, Err(arca_incus::IncusError::Refused(_))),
        "{refused:?}"
    );
    assert_eq!(rig.state_writes(), 0, "nothing was sent");
    assert_eq!(rig.status(PROJECT, "api"), "Running");
}

#[test]
fn a_proposal_binds_the_call_without_confirm() {
    let rig = Rig::new("life-confirm");
    let mut args = at(PROJECT, "api");
    args["confirm"] = json!(false);
    assert!(matches!(
        rig.call(
            Scope::Viewer,
            "action_propose",
            json!({ "action": "instance_stop", "args": args }),
        ),
        Err(CallError::Invalid(message)) if message.contains("nothing was proposed")
    ));
    let mut args = at(PROJECT, "api");
    args["confirm"] = json!(true);
    let proposal = rig
        .call(
            Scope::Viewer,
            "action_propose",
            json!({ "action": "instance_stop", "args": args }),
        )
        .expect("confirm: true proposes");
    assert_eq!(proposal["args"], at(PROJECT, "api"), "confirm is not bound");
    // A start is not destructive and takes no confirm.
    let mut args = at(PROJECT, "web");
    args["confirm"] = json!(true);
    assert!(matches!(
        rig.call(
            Scope::Viewer,
            "action_propose",
            json!({ "action": "instance_start", "args": args }),
        ),
        Err(CallError::Invalid(_))
    ));
}

#[test]
fn a_watch_only_node_refuses_all_three_and_their_proposals() {
    let rig = Rig::build("life-watch", false);
    for (name, args) in [
        ("instance_start", at(PROJECT, "web")),
        (
            "instance_stop",
            json!({ "project": PROJECT, "instance": "api", "approval": "x" }),
        ),
        (
            "instance_restart",
            json!({ "project": PROJECT, "instance": "api", "approval": "x" }),
        ),
    ] {
        let (code, message) = refusal(rig.call(Scope::Operator, name, args));
        assert_eq!(code, "InstanceLifecycleDisabled");
        assert!(message.contains("watch-only"), "{message}");
        let (code, _) = refusal(rig.call(
            Scope::Viewer,
            "action_propose",
            json!({ "action": name, "args": at(PROJECT, "web") }),
        ));
        assert_eq!(code, "InstanceLifecycleDisabled");
    }
    assert_eq!(rig.state_writes(), 0);
    assert!(rig.journal().is_empty());
}

#[test]
fn a_journal_that_cannot_be_written_refuses_the_action_before_any_change() {
    let rig = Rig::new("life-journal");
    // A journal another user could read is not written to, and is not replaced.
    let path = rig.scratch.path(INSTANCE_ACTIONS_FILE);
    fs::write(&path, "").expect("a journal");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("mode 0644");
    let (code, message) = refusal(rig.call(Scope::Doctor, "instance_start", at(PROJECT, "web")));
    assert_eq!(code, "InstanceJournalUnavailable");
    assert!(message.contains("nothing was changed"), "{message}");
    assert_eq!(rig.status(PROJECT, "web"), "Stopped");
    assert_eq!(rig.state_writes(), 0);
}

/// The MCP door over the rig's handlers: `arca_mcp::Server` with the real verb table, so every argument is checked against
/// its verb's params schema and every result against its result schema, as in production.
struct Door {
    socket: std::path::PathBuf,
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
        for (verb, handler) in &rig.handlers {
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

    /// One JSON-RPC request through the door: the HTTP status, and the reply when it has a body.
    fn rpc(&self, secret: &str, method: &str, params: Value) -> (u16, Value) {
        use std::io::{Read as _, Write as _};
        let body = serde_json::to_vec(&json!({
            "jsonrpc": "2.0", "id": 1, "method": method, "params": params
        }))
        .expect("the call");
        let head = format!(
            "POST /mcp HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {secret}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let mut stream =
            std::os::unix::net::UnixStream::connect(&self.socket).expect("connect to the door");
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .expect("a read timeout");
        stream.write_all(head.as_bytes()).expect("the head");
        stream.write_all(&body).expect("the body");
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).expect("the answer");
        let split = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("a response head");
        let status = std::str::from_utf8(&raw[..split])
            .expect("a text head")
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .expect("a status");
        (
            status,
            serde_json::from_slice(&raw[split + 4..]).unwrap_or(Value::Null),
        )
    }

    fn call(&self, secret: &str, name: &str, arguments: Value) -> (u16, Value) {
        self.rpc(
            secret,
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
    }
}

#[test]
fn through_the_mcp_door_a_doctor_starts_and_a_stop_needs_the_owners_approval() {
    let rig = Rig::new("life-door");
    let door = Door::over(&rig);

    // The tool list carries each verb's hints: a start is a plain write, a stop and a restart are destructive.
    let (_, listed) = door.rpc(&door.doctor, "tools/list", json!({}));
    let tools = listed["result"]["tools"].as_array().expect("tools");
    let tool = |name: &str| {
        tools
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap_or_else(|| panic!("{name} is listed"))
    };
    assert_eq!(
        tool("instance_start")["annotations"],
        json!({ "readOnlyHint": false, "destructiveHint": false })
    );
    for name in ["instance_stop", "instance_restart"] {
        assert_eq!(
            tool(name)["annotations"],
            json!({ "readOnlyHint": false, "destructiveHint": true })
        );
        let schema = &tool(name)["inputSchema"];
        // The project is named by `project` or, in a tenant, by `tenant`: the handler requires one of the two.
        // Nothing is required by the schema: the handler asks for the instance, and outside a tenant for the approval.
        assert!(schema["required"].as_array().is_none_or(|r| r.is_empty()));
        assert_eq!(schema["properties"]["confirm"]["type"], json!("boolean"));
    }
    assert!(
        tool("instance_start")["inputSchema"]["required"]
            .as_array()
            .is_none_or(|r| r.is_empty())
    );
    // A viewer is not offered any of the three, and a call is forbidden.
    let (_, listed) = door.rpc(&door.viewer, "tools/list", json!({}));
    let viewer_tools = listed["result"]["tools"].as_array().expect("tools");
    assert!(
        viewer_tools
            .iter()
            .all(|tool| !tool["name"].as_str().unwrap_or("").starts_with("instance_"))
    );
    let (status, _) = door.call(&door.viewer, "instance_start", at(PROJECT, "web"));
    assert_eq!(status, 403);
    assert_eq!(rig.status(PROJECT, "web"), "Stopped");

    // A doctor's start runs with no approval, and its result meets the verb's schema at the door.
    let (_, reply) = door.call(&door.doctor, "instance_start", at(PROJECT, "web"));
    assert_eq!(reply["result"]["isError"], json!(false), "{reply}");
    assert_eq!(
        reply["result"]["structuredContent"],
        json!({
            "project": PROJECT,
            "instance": "web",
            "action": "start",
            "status": "Running",
            "changed": true,
            "message": format!("started the instance web in project {PROJECT}"),
        })
    );
    assert_eq!(
        reply["result"]["content"][0]["text"],
        json!(format!("started the instance web in project {PROJECT}"))
    );
    assert_eq!(rig.status(PROJECT, "web"), "Running");
    assert_eq!(rig.journal()[0]["token_id"], json!("doctor"));

    // An instance that is not Arca's is refused in plain words.
    let (_, reply) = door.call(&door.doctor, "instance_start", at(PROJECT, "stray"));
    assert_eq!(
        reply["error"]["data"]["refusal"],
        json!("InstanceNotArca"),
        "{reply}"
    );
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("is not Arca's")
    );

    // A stop: confirm false is refused before the handler, a call with no grant is not approved, a schema miss is invalid.
    let mut args = at(PROJECT, "web");
    args["approval"] = json!("x.y");
    let mut unconfirmed = args.clone();
    unconfirmed["confirm"] = json!(false);
    let (_, reply) = door.call(&door.doctor, "instance_stop", unconfirmed);
    assert_eq!(
        reply["error"]["message"],
        json!("confirm is false: nothing was stopped and no approval was spent"),
        "{reply}"
    );
    assert_eq!(reply["error"]["data"]["refusal"], json!("NotConfirmed"));
    let mut confirmed = args.clone();
    confirmed["confirm"] = json!(true);
    for sent in [args.clone(), confirmed] {
        let (_, reply) = door.call(&door.doctor, "instance_stop", sent);
        assert_eq!(
            reply["error"]["data"]["refusal"],
            json!("NotApproved"),
            "{reply}"
        );
    }
    let (_, reply) = door.call(&door.doctor, "instance_restart", at(PROJECT, "web"));
    assert_eq!(reply["error"]["code"], json!(-32602), "{reply}");
    assert_eq!(rig.status(PROJECT, "web"), "Running");

    // The viewer proposes the stop through the door; the owner approves it; it runs once.
    let (_, reply) = door.call(
        &door.viewer,
        "action_propose",
        json!({ "action": "instance_stop", "args": at(PROJECT, "web") }),
    );
    assert_eq!(reply["result"]["isError"], json!(false), "{reply}");
    let proposal = &reply["result"]["structuredContent"];
    assert_eq!(proposal["action"], json!("instance_stop"));
    let (_, reply) = door.call(&door.viewer, "plan_list", json!({}));
    assert_eq!(
        reply["result"]["structuredContent"]["plans"][0]["id"],
        proposal["id"]
    );
    let stopped = rig
        .approved("instance_stop", at(PROJECT, "web"))
        .expect("the approved stop runs");
    assert_eq!(stopped["status"], json!("Stopped"));
    assert_eq!(rig.status(PROJECT, "web"), "Stopped");
}

#[test]
fn the_three_verbs_are_proposable_and_gated_as_the_table_says() {
    for (name, destructive) in [
        ("instance_start", false),
        ("instance_stop", true),
        ("instance_restart", true),
    ] {
        let v = verb(name).expect("in the table");
        assert_eq!(v.min_scope, Scope::Doctor, "{name}");
        assert!(v.writes, "{name}");
        assert_eq!(v.destructive, destructive, "{name}");
        assert!(
            arca::action_proposal::ActionKind::from_name(name).is_some(),
            "{name} is proposable"
        );
        assert!(
            arca_approval::ArcaAction::from_name(name).is_some(),
            "{name} can be approved"
        );
    }
}
