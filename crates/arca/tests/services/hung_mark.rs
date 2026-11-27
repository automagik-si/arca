//! Issue #307: a hung mark (Rule 82) ends when Arca sees its instance stopped. The lifecycle verbs' stop, start and
//! restart and the update path that stops an instance clear it; an instance nobody stopped keeps it. `service_logs` says
//! which source its lines came from and, when the journal was not read, why. Every value is synthetic.
//!
//! A stop and a restart through their handlers take an approved action proposal; `instance_lifecycle.rs` proves that
//! gate. Here they are run through the lifecycle the handlers call, `InstanceLifecycle::run`, so the test is about the
//! mark and not the approval. A start runs through its handler, as a doctor calls it.
use super::*;
use arca::app_formula::AppUpdate;
use arca::handlers::{ServiceHandlers, UpdateOptions, service_exec, service_handlers_updating};
use arca::lifecycle::InstanceLifecycle;
use arca::service_exec::{DeclaredCommand, ExecTarget, ServiceExec};
use arca_incus::{
    StateAction,
    exec::{ExecCommand, JournalLines},
    fake::ExecScript,
};
use std::time::{Duration, Instant};

/// The longest one of these tests may take. A fake that never answers must fail the test, not hold the build.
const TEST_BOUND: Duration = Duration::from_secs(120);

/// Runs `test` on a thread of its own and fails when it has not ended within [`TEST_BOUND`].
fn bounded(test: impl FnOnce() + Send + 'static) {
    let (done, ended) = std::sync::mpsc::channel();
    let runner = std::thread::spawn(move || {
        test();
        let _ = done.send(());
    });
    match ended.recv_timeout(TEST_BOUND) {
        Ok(()) => runner.join().expect("the test"),
        // The test panicked: its own message is the failure.
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            if let Err(panic) = runner.join() {
                std::panic::resume_unwind(panic);
            }
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            panic!("the test did not end within {TEST_BOUND:?}")
        }
    }
}

/// The node's service handlers and the lifecycle as a served node builds them: one exec handle, whose marks the
/// lifecycle clears.
struct Node {
    exec: Arc<ServiceExec>,
    handlers: HashMap<&'static str, Arc<dyn ToolHandler>>,
    lifecycle: Arc<InstanceLifecycle>,
}

fn node(rig: &Rig) -> Node {
    let served: ServiceHandlers = service_handlers_updating(
        rig.kernel.clone(),
        Arc::new(|| NOW),
        rig.cfg.clone(),
        None,
        UpdateOptions {
            probe: Arc::new(|_, _, _, _, _| true),
            pace: Duration::from_millis(10),
        },
    );
    let exec = service_exec(&served);
    let kernel = Arc::clone(&rig.kernel);
    let marks = Arc::clone(&exec);
    let lifecycle = Arc::new(
        InstanceLifecycle::new(
            rig.fake.socket().to_path_buf(),
            arca_incus::ProjectFamily::new([PROJECT], ["other"]).expect("the family"),
            move || {
                kernel
                    .lock()
                    .map_err(|_| "kernel".to_string())?
                    .retained_entries()
                    .map_err(|error| error.to_string())
            },
            &rig.dir,
        )
        .with_after_stop(Arc::new(move |instance, uuid| {
            marks.clear_hung_seen_stopped(instance, uuid);
        })),
    );
    let mut handlers: HashMap<&'static str, Arc<dyn ToolHandler>> = served.into_iter().collect();
    handlers.extend(arca::lifecycle::handlers(
        Arc::clone(&rig.kernel),
        Arc::new(|| NOW),
        Some(Arc::clone(&lifecycle)),
    ));
    Node {
        exec,
        handlers,
        lifecycle,
    }
}

fn doctor() -> Caller {
    Caller {
        token_id: "doctor".into(),
        scope: Scope::Doctor,
        tools: None,
        limits: Default::default(),
    }
}

impl Node {
    fn call(&self, verb: &str, args: Value) -> Result<Value, CallError> {
        let answer = self.handlers[verb].call(args, &doctor());
        if let Ok(value) = &answer {
            arca_api::conforms(&(arca_api::verb(verb).expect("a verb").result)(), value)
                .unwrap_or_else(|why| panic!("{verb}: the answer does not meet its schema: {why}"));
        }
        answer
    }

    fn act(&self, action: StateAction) -> Value {
        self.lifecycle
            .run(action, PROJECT, "app", "operator", None, NOW)
            .expect("the lifecycle acts")
    }

    fn logs(&self) -> Value {
        self.call("service_logs", json!({"name":"app"}))
            .expect("the log")
    }
}

/// The running `system` service `app`.
fn system(rig: &Rig) {
    rig.template(
        "app",
        json!({"name":"app","kind":"system","image":"images:debian/13","ports":[],"env":{}}),
    );
    rig.call("service_create", json!({"name":"app","template":"app"}))
        .expect("the service");
}

fn target(rig: &Rig) -> ExecTarget {
    let service = rig.service_config("app");
    let kernel = rig.kernel.lock().unwrap();
    let entry = EntryId("service-app".into());
    ExecTarget {
        uuid: kernel.service_uuid(&entry, "app"),
        entry,
        service,
        retained: kernel.retained_entries().unwrap(),
    }
}

fn journal() -> ExecCommand {
    ExecCommand::Journal(JournalLines::new(50).unwrap())
}

/// A journal read Incus never answers: the instance is marked hung, as on the node of #307.
fn mark_hung(rig: &Rig, node: &Node, target: &ExecTarget) {
    let mut held = ExecScript::new(journal().argv(), 0, "");
    held.held = true;
    rig.fake.script_exec(held);
    // Past its deadline the identity re-read has no time left either, so the error itself can say the instance
    // changed; the mark is what counts.
    node.exec
        .run(
            target,
            "viewer",
            "service_logs",
            &journal(),
            &[],
            Some(Instant::now() + Duration::from_secs(2)),
        )
        .expect_err("Incus does not answer");
    assert!(node.exec.hung(target));
}

fn status(rig: &Rig) -> String {
    rig.get(&format!("/1.0/instances/app?project={PROJECT}"))
        .expect("the instance")["status"]
        .as_str()
        .expect("a status")
        .to_string()
}

/// The journal answers one line, the next time it is read.
fn journal_says(rig: &Rig, line: &str) {
    rig.fake
        .script_exec(ExecScript::new(journal().argv(), 0, &format!("{line}\n")));
}

fn reads_the_journal(rig: &Rig, node: &Node, line: &str) {
    journal_says(rig, line);
    let answer = node.logs();
    assert_eq!(answer["lines"], json!([line]), "{answer}");
    assert_eq!(answer["source"], "journal", "{answer}");
    assert!(answer.get("journalNotRead").is_none(), "{answer}");
}

#[test]
fn a_hung_instance_reads_the_console_and_says_why_until_it_is_stopped_and_started() {
    bounded(|| {
        let rig = Rig::new();
        system(&rig);
        let node = node(&rig);
        let target = target(&rig);
        reads_the_journal(&rig, &node, "journal before");
        mark_hung(&rig, &node, &target);
        rig.fake.set_console_log(PROJECT, "app", "console line");
        // The journal is refused while the mark stays: the console answers, and the answer says so and why.
        let answer = node.logs();
        assert_eq!(answer["lines"], json!(["console line"]), "{answer}");
        assert_eq!(answer["source"], "console", "{answer}");
        let why = answer["journalNotRead"].as_str().expect("why, in words");
        assert!(why.contains("did not answer"), "{why}");
        assert!(why.contains("instance_stop"), "{why}");
        // A start of a running instance stops nothing: the command that did not answer may still run, so the mark stays.
        let started = node
            .call(
                "instance_start",
                json!({"project":PROJECT,"instance":"app"}),
            )
            .expect("already running");
        assert_eq!(started["changed"], false);
        assert!(node.exec.hung(&target), "nothing stopped it");
        assert_eq!(node.logs()["source"], "console");
        // Stopped through the lifecycle, the mark is gone; started again, the journal answers.
        assert_eq!(node.act(StateAction::Stop)["status"], "Stopped");
        assert!(!node.exec.hung(&target), "seen stopped");
        node.call(
            "instance_start",
            json!({"project":PROJECT,"instance":"app"}),
        )
        .expect("the start");
        assert_eq!(status(&rig), "Running");
        reads_the_journal(&rig, &node, "journal after");
    });
}

#[test]
fn a_restart_through_the_lifecycle_clears_the_mark() {
    bounded(|| {
        let rig = Rig::new();
        system(&rig);
        let node = node(&rig);
        let target = target(&rig);
        mark_hung(&rig, &node, &target);
        let restarted = node.act(StateAction::Restart);
        assert_eq!(restarted["changed"], true);
        assert_eq!(restarted["status"], "Running");
        assert!(!node.exec.hung(&target), "a restart stops the instance");
        reads_the_journal(&rig, &node, "journal after restart");
    });
}

#[test]
fn a_stop_found_already_done_and_a_start_of_a_stopped_instance_clear_the_mark() {
    bounded(|| {
        let rig = Rig::new();
        system(&rig);
        let node = node(&rig);
        let target = target(&rig);
        // Stopped by someone else: the lifecycle's stop finds it stopped, and that is seeing it stopped.
        mark_hung(&rig, &node, &target);
        rig.fake.set_instance_status(PROJECT, "app", "Stopped");
        assert!(node.exec.hung(&target), "Arca has not looked yet");
        assert_eq!(node.act(StateAction::Stop)["changed"], false);
        assert!(!node.exec.hung(&target), "found stopped");
        // Marked again and stopped by someone else: the start sees it stopped before it starts it.
        rig.fake.set_instance_status(PROJECT, "app", "Running");
        mark_hung(&rig, &node, &target);
        rig.fake.set_instance_status(PROJECT, "app", "Stopped");
        node.call(
            "instance_start",
            json!({"project":PROJECT,"instance":"app"}),
        )
        .expect("the start");
        assert!(!node.exec.hung(&target), "started from stopped");
        reads_the_journal(&rig, &node, "journal after start");
    });
}

#[test]
fn the_mark_of_another_instance_is_not_touched() {
    bounded(|| {
        let rig = Rig::new();
        system(&rig);
        rig.call("service_create", json!({"name":"other","template":"app"}))
            .expect("a second service");
        let node = node(&rig);
        let target = target(&rig);
        mark_hung(&rig, &node, &target);
        node.lifecycle
            .run(
                StateAction::Restart,
                PROJECT,
                "other",
                "operator",
                None,
                NOW,
            )
            .expect("the other restarts");
        assert!(
            node.exec.hung(&target),
            "only its own instance clears a mark"
        );
    });
}

fn formula() -> AppUpdate {
    serde_json::from_value(json!({
        "health": { "command": ["/usr/bin/curl", "--fail", "http://localhost/ready"], "seconds": 300 },
        "version": { "command": ["/usr/bin/cat", "/opt/app/version.txt"] }
    }))
    .expect("a formula")
}

#[test]
fn the_update_that_stops_an_instance_clears_its_mark() {
    bounded(|| {
        let rig = Rig::with(
            FakeState {
                server: json!({}),
                projects: vec![],
                instances: vec![],
                networks: vec![],
                pools: vec![("pool1".into(), json!({}))],
            },
            vec![PROJECT.to_string()],
            Some("192.0.2.0/24"),
        );
        rig.template(
        "app",
        json!({"name":"app","kind":"oci","image":"registry.example/app","digest":DIGEST,"ports":[8080],"lan":{"port":8080},"env":{}}),
    );
        rig.call("service_create", json!({"name":"app","template":"app"}))
            .expect("the service");
        let node = node(&rig);
        let target = target(&rig);
        // A declared command Incus never answers marks an OCI container (Rule 82).
        let version = DeclaredCommand::version(&formula(), "oci").expect("the version reader");
        let mut held = ExecScript::new(version.argv(), 0, "");
        held.held = true;
        rig.fake.script_exec(held);
        node.exec
            .run_formula(
                &target,
                "doctor",
                "app_update",
                &version,
                Some(Instant::now() + Duration::from_secs(2)),
            )
            .expect_err("Incus does not answer");
        assert!(node.exec.hung(&target));
        let uuid = target.uuid.clone();
        let done = node
        .call(
            "service_update",
            json!({"name":"app","digest":"sha256:2222222222222222222222222222222222222222222222222222222222222222"}),
        )
        .expect("the update");
        assert_eq!(done["updated"], true, "{done}");
        assert_eq!(
            rig.kernel
                .lock()
                .unwrap()
                .service_uuid(&EntryId("service-app".into()), "app"),
            uuid,
            "the same instance: only a stop can have cleared the mark"
        );
        assert!(!node.exec.hung(&target), "the update stopped it");
    });
}
