//! Issue #307: a hung mark (Rule 82) ends when Arca sees its instance stopped. The lifecycle verbs' stop, start and
//! restart and the update path that stops an instance clear it; an instance nobody stopped keeps it. `service_logs` says
//! which source its lines came from and, when the journal was not read, why. Every value is synthetic.
//!
//! A mark also ages: after [`HUNG_PROBE_AFTER`] the next `service_logs` reads the journal once, one read at a time,
//! and an answer clears the mark. The node's clock is the test's own, so nothing waits for it.
//!
//! A stop and a restart through their handlers take an approved action proposal; `instance_lifecycle.rs` proves that
//! gate. Here they are run through the lifecycle the handlers call, `InstanceLifecycle::run`, so the test is about the
//! mark and not the approval. A start runs through its handler, as a doctor calls it.
use super::*;
use arca::app_formula::AppUpdate;
use arca::handlers::{ServiceHandlers, UpdateOptions, service_exec, service_handlers_updating};
use arca::lifecycle::InstanceLifecycle;
use arca::service_exec::{DeclaredCommand, ExecTarget, HUNG_PROBE_AFTER, ServiceExec};
use arca_incus::{
    StateAction,
    exec::{ExecCommand, JournalLines},
    fake::ExecScript,
};
use std::time::{Duration, Instant};

/// The node's service handlers and the lifecycle as a served node builds them: one exec handle, whose marks the
/// lifecycle clears.
struct Node {
    /// The node's clock in milliseconds: a test moves it on.
    clock: Arc<AtomicU64>,
    exec: Arc<ServiceExec>,
    handlers: HashMap<&'static str, Arc<dyn ToolHandler>>,
    lifecycle: Arc<InstanceLifecycle>,
}

fn node(rig: &Rig) -> Node {
    let clock = Arc::new(AtomicU64::new(NOW));
    let now = Arc::clone(&clock);
    let served: ServiceHandlers = service_handlers_updating(
        rig.kernel.clone(),
        Arc::new(move || now.load(Ordering::SeqCst)),
        rig.cfg.clone(),
        None,
        UpdateOptions {
            probe: Arc::new(|_, _, _, _, _| true),
            pace: Duration::from_millis(10),
        },
    );
    let exec = service_exec(&served);
    let lifecycle = lifecycle(rig, &exec, None);
    let mut handlers: HashMap<&'static str, Arc<dyn ToolHandler>> = served.into_iter().collect();
    handlers.extend(arca::lifecycle::handlers(
        Arc::clone(&rig.kernel),
        Arc::new(|| NOW),
        Some(Arc::clone(&lifecycle)),
    ));
    Node {
        clock,
        exec,
        handlers,
        lifecycle,
    }
}

/// The lifecycle over the node's family that clears `exec`'s marks, as the served node wires it. `deadline` shortens how
/// long a state change is waited on.
fn lifecycle(
    rig: &Rig,
    exec: &Arc<ServiceExec>,
    deadline: Option<Duration>,
) -> Arc<InstanceLifecycle> {
    let kernel = Arc::clone(&rig.kernel);
    let marks = Arc::clone(exec);
    let family = arca_incus::ProjectFamily::new([PROJECT], ["other"]).expect("the family");
    let mut lifecycle = InstanceLifecycle::new(
        rig.fake.socket().to_path_buf(),
        family.clone(),
        move || {
            kernel
                .lock()
                .map_err(|_| "kernel".to_string())?
                .retained_entries()
                .map_err(|error| error.to_string())
        },
        &rig.dir,
    );
    if let Some(deadline) = deadline {
        lifecycle = lifecycle.with_changer(
            arca_incus::StateChanger::new(rig.fake.socket().to_path_buf(), family)
                .with_deadline(deadline),
        );
    }
    Arc::new(lifecycle.with_after_stop(Arc::new(move |instance, uuid| {
        marks.clear_hung_seen_stopped(instance, uuid);
    })))
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

    /// Moves the node's clock on by `by`.
    fn wait(&self, by: Duration) {
        self.clock
            .fetch_add(u64::try_from(by.as_millis()).unwrap(), Ordering::SeqCst);
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

/// The exec requests the fake was sent.
fn execs(rig: &Rig) -> usize {
    rig.fake
        .service_requests()
        .iter()
        .filter(|r| r.path.contains("/exec?") && r.method == "POST")
        .count()
}

/// Starts a journal read Incus never answers on a thread of its own, and returns once Incus was sent it. The read
/// ends past its two-second deadline.
fn read_in_flight(rig: &Rig, node: &Node, target: &ExecTarget) -> std::thread::JoinHandle<()> {
    let mut held = ExecScript::new(journal().argv(), 0, "");
    held.held = true;
    rig.fake.script_exec(held);
    let sent = execs(rig);
    let exec = Arc::clone(&node.exec);
    let target = target.clone();
    let read = std::thread::spawn(move || {
        let _ = exec.run(
            &target,
            "viewer",
            "service_logs",
            &journal(),
            &[],
            Some(Instant::now() + Duration::from_secs(2)),
        );
    });
    let started = Instant::now() + Duration::from_secs(2);
    while execs(rig) == sent && Instant::now() < started {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(execs(rig) > sent, "the read was sent");
    read
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

#[test]
fn after_five_minutes_one_journal_read_tries_again_and_its_answer_clears_the_mark() {
    bounded(|| {
        let rig = Rig::new();
        system(&rig);
        let node = node(&rig);
        let target = target(&rig);
        mark_hung(&rig, &node, &target);
        rig.fake.set_console_log(PROJECT, "app", "console line");
        // Before its time the mark refuses the read: nothing is sent to the instance.
        node.wait(HUNG_PROBE_AFTER - Duration::from_secs(1));
        let sent = execs(&rig);
        assert_eq!(node.logs()["source"], "console");
        assert_eq!(execs(&rig), sent, "no read was sent");
        assert!(node.exec.hung(&target));
        // Once it has come, one read goes; it answers, so the mark is gone and the journal answers again.
        node.wait(Duration::from_secs(1));
        reads_the_journal(&rig, &node, "journal again");
        assert!(!node.exec.hung(&target), "the answer cleared the mark");
        reads_the_journal(&rig, &node, "journal still");
    });
}

#[test]
fn a_try_that_fails_keeps_the_mark_for_five_more_minutes() {
    bounded(|| {
        let rig = Rig::new();
        system(&rig);
        let node = node(&rig);
        let target = target(&rig);
        mark_hung(&rig, &node, &target);
        rig.fake.set_console_log(PROJECT, "app", "console line");
        node.wait(HUNG_PROBE_AFTER);
        // The read runs and exits with an error: the answer says so in those words, and the mark stays.
        rig.fake
            .script_exec(ExecScript::new(journal().argv(), 1, ""));
        let sent = execs(&rig);
        let answer = node.logs();
        assert_eq!(execs(&rig), sent + 1, "one read was sent");
        assert_eq!(answer["source"], "console", "{answer}");
        let why = answer["journalNotRead"].as_str().expect("why");
        assert!(why.starts_with("the journal read ran and failed"), "{why}");
        assert!(node.exec.hung(&target));
        // The mark is renewed: the next call sends nothing, until five more minutes have passed.
        node.wait(HUNG_PROBE_AFTER - Duration::from_secs(1));
        let why = node.logs()["journalNotRead"].clone();
        assert!(why.as_str().unwrap().contains("did not answer"), "{why}");
        assert_eq!(execs(&rig), sent + 1, "no second read");
        node.wait(Duration::from_secs(1));
        reads_the_journal(&rig, &node, "journal after the second try");
        assert!(!node.exec.hung(&target));
    });
}

#[test]
fn only_one_try_runs_at_a_time() {
    bounded(|| {
        let rig = Rig::new();
        system(&rig);
        let node = node(&rig);
        let target = target(&rig);
        mark_hung(&rig, &node, &target);
        rig.fake.set_console_log(PROJECT, "app", "console line");
        node.wait(HUNG_PROBE_AFTER);
        // The try Incus does not answer either: while it runs, every other read is refused before it is sent.
        let probe = read_in_flight(&rig, &node, &target);
        let sent = execs(&rig);
        for _ in 0..3 {
            let answer = node.logs();
            assert_eq!(answer["source"], "console", "{answer}");
            assert!(
                answer["journalNotRead"]
                    .as_str()
                    .unwrap()
                    .contains("did not answer")
            );
        }
        assert_eq!(execs(&rig), sent, "no read piled up behind the try");
        probe.join().expect("the try ends");
        // It did not answer: the mark is renewed, so the next read waits five more minutes.
        assert!(node.exec.hung(&target));
        assert_eq!(node.logs()["source"], "console");
        assert_eq!(execs(&rig), sent);
    });
}

#[test]
fn a_read_that_was_running_when_the_instance_stopped_does_not_mark_it_again() {
    bounded(|| {
        let rig = Rig::new();
        system(&rig);
        let node = node(&rig);
        let target = target(&rig);
        let read = read_in_flight(&rig, &node, &target);
        // The stop ends the read's command; its deadline passes after the clear, and it marks nothing.
        assert_eq!(node.act(StateAction::Restart)["status"], "Running");
        read.join().expect("the read ends");
        assert!(
            !node.exec.hung(&target),
            "the restart came after the read began"
        );
        reads_the_journal(&rig, &node, "journal after restart");
        // A read that begins after the clear still marks the instance when Incus does not answer it.
        mark_hung(&rig, &node, &target);
    });
}

#[test]
fn the_cut_answer_keeps_where_its_lines_came_from() {
    let lines: Vec<String> = (0..205).map(|i| format!("{i}")).collect();
    let why = "the journal read failed: service exec failed";
    let cut = arca_api::verbs::cut_service_logs(&json!({
        "lines": lines, "source": "console", "journalNotRead": why, "note": "a note"
    }))
    .expect("the answer is cut");
    assert_eq!(cut["lines"].as_array().unwrap().len(), 200);
    assert_eq!(cut["source"], "console");
    assert_eq!(cut["journalNotRead"], why);
    assert_eq!(cut["note"], "a note");
    let journal = arca_api::verbs::cut_service_logs(&json!({"lines": lines, "source": "journal"}))
        .expect("the answer is cut");
    assert_eq!(journal["source"], "journal");
    assert!(journal.get("journalNotRead").is_none());
}

#[test]
fn a_restart_that_left_the_instance_stopped_and_was_followed_by_a_start_clears_the_mark() {
    bounded(|| {
        let rig = Rig::new();
        system(&rig);
        let node = node(&rig);
        let target = target(&rig);
        mark_hung(&rig, &node, &target);
        // Incus does the stop half and fails the start half; Arca sees it stopped and sends one plain start (#306).
        rig.fake.refuse_start_once(PROJECT, "app");
        let restarted = node.act(StateAction::Restart);
        assert_eq!(restarted["status"], "Running", "{restarted}");
        assert!(
            restarted["message"]
                .as_str()
                .unwrap()
                .contains("one plain start"),
            "{restarted}"
        );
        assert!(!node.exec.hung(&target), "seen stopped after the restart");
        reads_the_journal(&rig, &node, "journal after the separate start");
    });
}

/// Review 2's N1: the clear count is per instance, by name and uuid. Another instance named `app` (a tenant's, in
/// another project) seen stopped while this one's read is in flight does not keep this one from being marked.
#[test]
fn a_clear_of_another_instance_with_the_same_name_does_not_stop_this_one_being_marked() {
    bounded(|| {
        let rig = Rig::new();
        system(&rig);
        let node = node(&rig);
        let target = target(&rig);
        let read = read_in_flight(&rig, &node, &target);
        assert!(
            node.exec
                .clear_hung_seen_stopped("app", Some("a-tenants-own-uuid"))
        );
        read.join().expect("the read ends");
        assert!(
            node.exec.hung(&target),
            "its own instance was never stopped"
        );
    });
}

/// Review 2's N2: the answer gives the wait that is left, which is longer than five minutes after a long declared
/// command, and says so while a try runs.
#[test]
fn the_reason_gives_the_wait_that_is_left() {
    bounded(|| {
        let rig = Rig::new();
        system(&rig);
        let node = node(&rig);
        let target = target(&rig);
        rig.fake.set_console_log(PROJECT, "app", "console line");
        let why = |node: &Node| {
            node.logs()["journalNotRead"]
                .as_str()
                .expect("why")
                .to_string()
        };
        // A journal read's mark: five minutes, then less as the clock moves on.
        mark_hung(&rig, &node, &target);
        assert!(why(&node).contains("for 5 minutes more"), "{}", why(&node));
        node.wait(Duration::from_secs(250));
        assert!(why(&node).contains("for 50 seconds more"), "{}", why(&node));
        // While the try runs, the others are told so.
        node.wait(Duration::from_secs(50));
        let probe = read_in_flight(&rig, &node, &target);
        assert!(
            why(&node).contains("one try of the journal is running now"),
            "{}",
            why(&node)
        );
        probe.join().expect("the try ends");
        // A declared update of an hour marks for its own limit and margin: an hour and five seconds, said in minutes.
        let formula: AppUpdate = serde_json::from_value(json!({
            "update": { "command": ["/usr/local/sbin/app-update", "--to={version}"], "seconds": 3600 },
            "health": { "command": ["/usr/bin/curl", "--fail", "http://localhost/ready"], "seconds": 300 },
            "version": { "command": ["/usr/bin/cat", "/opt/app/version.txt"] }
        }))
        .expect("a formula");
        let update = DeclaredCommand::update(
            &formula,
            "system",
            &arca::app_formula::AppVersion::parse("2.0").unwrap(),
        )
        .expect("the update");
        node.act(StateAction::Restart);
        let mut held = ExecScript::new(update.argv(), 0, "");
        held.held = true;
        rig.fake.script_exec(held);
        let _ = node.exec.run_formula(
            &target,
            "doctor",
            "app_update",
            &update,
            Some(Instant::now() + Duration::from_secs(2)),
        );
        assert!(node.exec.hung(&target));
        assert!(why(&node).contains("for 61 minutes more"), "{}", why(&node));
    });
}

/// Review 2's N3: a restart whose start half Incus failed, whose separate start failed too, but that is running anyway
/// (started from elsewhere) was seen stopped, so its mark goes.
#[test]
fn a_restart_found_running_after_its_separate_start_clears_the_mark() {
    bounded(|| {
        let rig = Rig::new();
        system(&rig);
        let node = node(&rig);
        let target = target(&rig);
        mark_hung(&rig, &node, &target);
        rig.fake.refuse_start(PROJECT, "app");
        rig.fake
            .started_after_reads(PROJECT, "app", OUTSIDE_START_READ);
        let restarted = node.act(StateAction::Restart);
        assert!(
            restarted["message"]
                .as_str()
                .unwrap()
                .contains("but it is running now"),
            "{restarted}"
        );
        let end: Value = fs::read_to_string(rig.dir.join(arca::lifecycle::INSTANCE_ACTIONS_FILE))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .filter(|row: &Value| row["kind"] == "end")
            .last()
            .expect("an end row");
        assert_eq!(end["outcome"], "running_after_restart", "{end}");
        assert!(!node.exec.hung(&target), "seen stopped before it ran again");
    });
}

/// The read of the instance on which the start from elsewhere lands: after the restart's own reads, and after the
/// read that finds it stopped.
const OUTSIDE_START_READ: u32 = 3;

/// Review 2's N3: a restart Incus has not finished when Arca stops waiting has an end Arca does not know (`unknown`,
/// InstanceRestartUnknown). Nothing says the instance stopped, so the mark stays.
#[test]
fn a_restart_whose_end_is_not_known_keeps_the_mark() {
    bounded(|| {
        let rig = Rig::new();
        system(&rig);
        let node = node(&rig);
        let target = target(&rig);
        mark_hung(&rig, &node, &target);
        let short = lifecycle(&rig, &node.exec, Some(Duration::from_millis(300)));
        let slow = rig.fake.hold_state(PROJECT, "app");
        let refusal = short
            .run(StateAction::Restart, PROJECT, "app", "operator", None, NOW)
            .expect_err("the restart is not finished");
        assert!(
            matches!(&refusal, CallError::Refused { code, .. } if code == "InstanceRestartUnknown"),
            "{refusal:?}"
        );
        assert!(node.exec.hung(&target), "nothing showed it stopped");
        rig.fake.release_operation(&slow);
    });
}
