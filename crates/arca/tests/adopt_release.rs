//! Resume, release and verify of an adoption, against the fake Incus, which models `deviceVolatileReset`. A kill is the run
//! halted where a process kill would stop it: after a step's intent line, after its request but before its outcome line (with
//! a torn line left behind), or after its outcome line. Every refusal is checked twice: the command refuses and names its
//! reason, and the fake records no write.

mod common;

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use arca::Failure;
use arca::adoptions::{AdoptionPlan, AdoptionRecord, Phase, release_steps, resume_steps};
use arca::cmd_adopt::{
    HttpUrl, Kill, adopt_halting, http_probe, url_probe, verify_until, verify_url_until,
    verify_url_with, verify_with,
};
use arca::cmd_serve::watch_handler;
use arca::config;
use arca::desired_store::lock_state_dir;
use arca::handlers::{now_ms, watcher_step};
use arca_api::Scope;
use arca_incus::adopt::{AdoptStep, AdoptTarget, Allowances};
use arca_incus::{FakeIncus, UnixIncus};
use arca_kernel::ownership::{IncusKind, ObjectRef};
use arca_mcp::{Caller, ToolHandler};
use common::{
    ADOPT_IP, ADOPT_MAC, ADOPT_TO, ADOPT_UUID, Scratch, adopt_config, adopt_digest, adopt_fake,
    adopt_instance, adopt_run, get,
};
use serde_json::{Value, json};

const KILLS: [Kill; 3] = [Kill::AfterIntent, Kill::BeforeOutcome, Kill::AfterOutcome];
const MARK: &str = "user.arcakernel.entry";

/// One adoption's fake, config and plan digest. The fake is dropped before its scratch directory. A rooted run's instance has
/// no root of its own: the default profile gives it, so the plan localizes it. A stopped run's instance is stopped when it is
/// planned, so its plan neither stops nor starts it, and its apply names the IPv4 with `--expect-ip`.
struct Run {
    fake: FakeIncus,
    config: PathBuf,
    digest: String,
    rooted: bool,
    stopped: bool,
    scratch: Scratch,
}

/// The root disk the instance has, of its own or from the default profile.
fn root() -> Value {
    json!({ "type": "disk", "path": "/", "pool": "pool1", "size": "60GiB" })
}

impl Run {
    fn new(tag: &str) -> Run {
        Run::of(tag, false)
    }

    /// A run whose root the default profile gives.
    fn rooted(tag: &str) -> Run {
        Run::of(tag, true)
    }

    fn of(tag: &str, rooted: bool) -> Run {
        Run::built(tag, rooted, false)
    }

    /// A run whose instance is stopped when it is planned.
    fn stopped(tag: &str, rooted: bool) -> Run {
        Run::built(tag, rooted, true)
    }

    fn built(tag: &str, rooted: bool, stopped: bool) -> Run {
        let scratch = Scratch::new(tag);
        let mut instance = adopt_instance("default", json!({}));
        if rooted {
            instance["devices"] = json!({});
        }
        if stopped {
            instance["status"] = json!("Stopped");
        }
        let fake = adopt_fake(&scratch, vec![instance]);
        if rooted {
            fake.add_profile(
                "default",
                json!({ "name": "default", "config": {}, "devices": {
                    "eth0": { "type": "nic", "name": "eth0", "network": "br1" }, "root": root() } }),
            );
        }
        let config = adopt_config(&scratch, &fake);
        let digest = adopt_digest(&config);
        Run {
            fake,
            config,
            digest,
            rooted,
            stopped,
            scratch,
        }
    }

    /// A run whose apply was killed at `kill` of `step`.
    fn killed(tag: &str, step: AdoptStep, kill: Kill) -> Run {
        Run::new(tag).halted(tag, step, kill)
    }

    /// This run with its apply killed at `kill` of `step`.
    fn halted(self, tag: &str, step: AdoptStep, kill: Kill) -> Run {
        let run = self;
        let halted = run.apply_halting(&mut |at, point| at == step && point == kill);
        assert!(
            matches!(&halted, Err(Failure::Runtime(why)) if why.contains("halted")),
            "{tag}: {halted:?}"
        );
        if kill == Kill::BeforeOutcome {
            // The process died while it wrote the outcome line: a torn line is left, which the next open drops.
            let mut file = OpenOptions::new()
                .append(true)
                .open(run.record_path())
                .expect("the record");
            file.write_all(b"{\"kind\":\"step\",\"instance\":\"svc1\",\"st")
                .expect("the torn line");
        }
        run
    }

    /// The status the instance has once an apply or a release completes: Running, or Stopped for a stopped run.
    fn status(&self) -> &'static str {
        if self.stopped { "Stopped" } else { "Running" }
    }

    fn record_path(&self) -> PathBuf {
        self.scratch.path("state/adoptions.jsonl")
    }

    fn record(&self) -> AdoptionRecord {
        AdoptionRecord::open(&self.scratch.path("state")).expect("the record")
    }

    fn adopt(
        &self,
        args: &[&str],
        halt: &mut dyn FnMut(AdoptStep, Kill) -> bool,
    ) -> Result<Option<Value>, Failure> {
        let mut argv: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
        argv.push("--config".to_string());
        argv.push(self.config.display().to_string());
        adopt_halting(&argv, halt)
    }

    /// The arguments of this run's apply: a stopped run names the IPv4, which a stopped instance does not show.
    fn apply_args(&self) -> Vec<&str> {
        let mut args = vec!["apply", "--instance", "svc1", "--plan-digest", &self.digest];
        if self.stopped {
            args.extend_from_slice(&["--expect-ip", ADOPT_IP]);
        }
        args
    }

    fn apply_halting(
        &self,
        halt: &mut dyn FnMut(AdoptStep, Kill) -> bool,
    ) -> Result<Option<Value>, Failure> {
        self.adopt(&self.apply_args(), halt)
    }

    fn apply(&self) -> Result<Option<Value>, Failure> {
        adopt_run(&self.config, &self.apply_args())
    }

    fn release(&self) -> Result<Option<Value>, Failure> {
        adopt_run(&self.config, &["release", "--instance", "svc1"])
    }

    fn writes(&self) -> Vec<(String, String)> {
        self.fake
            .writes()
            .into_iter()
            .map(|write| (write.method, write.path))
            .collect()
    }

    fn instance(&self, project: &str) -> Option<Value> {
        get(
            &self.fake,
            &format!("/1.0/instances/svc1?project={project}"),
        )
    }

    /// The live MAC and IPv4 of `eth0` in `project`.
    fn nic(&self, project: &str) -> (String, String) {
        let state = get(
            &self.fake,
            &format!("/1.0/instances/svc1/state?project={project}"),
        )
        .expect("the state");
        let eth0 = &state["network"]["eth0"];
        (
            eth0["hwaddr"].as_str().unwrap_or("").to_string(),
            eth0["addresses"][0]["address"]
                .as_str()
                .unwrap_or("")
                .to_string(),
        )
    }

    fn snapshot_in(&self, project: &str) -> bool {
        let name = format!("arca-adopt-{}", &self.digest[..12]);
        get(
            &self.fake,
            &format!("/1.0/instances/svc1/snapshots/{name}?project={project}"),
        )
        .is_some()
    }

    /// A request sent straight to the fake, past every check, for a test's own setup.
    fn raw(&self, method: &str, path: &str, body: &Value) -> u16 {
        raw_at(self.fake.socket(), method, path, body)
    }
}

/// The MAC the instance's config gives `eth0`: the pinned device's own `hwaddr`, else its volatile key. The pin sets `hwaddr`,
/// so Incus's `deviceVolatileReset`, which the fake models, drops the volatile key it replaces.
fn configured_mac(body: &Value) -> Value {
    body["expanded_devices"]["eth0"]
        .get("hwaddr")
        .cloned()
        .unwrap_or_else(|| body["config"]["volatile.eth0.hwaddr"].clone())
}

/// A request sent straight to the fake at `socket`, past every check, and the status it answered.
fn raw_at(socket: &Path, method: &str, path: &str, body: &Value) -> u16 {
    let text = body.to_string();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: incus\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        text.len()
    );
    let mut stream = UnixStream::connect(socket).expect("connect");
    stream.write_all(request.as_bytes()).expect("send");
    let mut reply = String::new();
    stream.read_to_string(&mut reply).expect("read");
    reply
        .get(9..12)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0)
}

fn pin() -> Value {
    json!({ "type": "nic", "name": "eth0", "network": "br1", "hwaddr": ADOPT_MAC, "ipv4.address": ADOPT_IP })
}

/// The seven writes of a whole adoption, each once, in order.
fn adoption_writes() -> Vec<(String, String)> {
    adoption_writes_of(false)
}

/// The writes of a whole adoption of a stopped run, each once, in order: those of [`adoption_writes_of`] but the stop and the
/// start.
fn stopped_adoption_writes_of(rooted: bool) -> Vec<(String, String)> {
    adoption_writes_of(rooted)
        .into_iter()
        .filter(|(method, path)| !(method == "PUT" && path.contains("/state")))
        .collect()
}

/// The writes of a whole adoption, each once, in order: seven, and LocalizeRoot's PATCH after the pin's when `rooted`.
fn adoption_writes_of(rooted: bool) -> Vec<(String, String)> {
    let at = |path: &str, project: &str| format!("{path}?project={project}");
    let mut writes = vec![
        ("POST", "/1.0/projects".to_string()),
        ("PUT", at("/1.0/instances/svc1/state", "default")),
        ("POST", at("/1.0/instances/svc1/snapshots", "default")),
        ("PATCH", at("/1.0/instances/svc1", "default")),
        ("POST", at("/1.0/instances/svc1", "default")),
        ("PATCH", at("/1.0/instances/svc1", ADOPT_TO)),
        ("PUT", at("/1.0/instances/svc1/state", ADOPT_TO)),
    ];
    if rooted {
        writes.insert(4, ("PATCH", at("/1.0/instances/svc1", "default")));
    }
    writes
        .into_iter()
        .map(|(method, path)| (method.to_string(), path))
        .collect()
}

/// Whether the request `step` sends was sent before the kill.
fn sent_before(step: AdoptStep, kill_step: AdoptStep, kill: Kill) -> bool {
    let at = |s: AdoptStep| {
        AdoptStep::ADOPT
            .iter()
            .position(|x| *x == s)
            .expect("an adopt step")
    };
    at(step) < at(kill_step) || (step == kill_step && kill != Kill::AfterIntent)
}

fn assert_adopted(run: &Run, tag: &str) {
    assert!(
        run.instance("default").is_none(),
        "{tag}: the instance left default"
    );
    let body = run
        .instance(ADOPT_TO)
        .expect("the instance is in its adoption project");
    assert_eq!(body["status"], json!(run.status()), "{tag}");
    assert_eq!(body["config"][MARK], json!("adopt-svc1"), "{tag}");
    let uuid = run.record().uuid("svc1").expect("the recorded uuid");
    assert_ne!(
        uuid, ADOPT_UUID,
        "{tag}: the move was a copy, under a new uuid"
    );
    assert_eq!(body["config"]["volatile.uuid"], json!(uuid), "{tag}");
    assert_eq!(
        body["devices"]["eth0"],
        pin(),
        "{tag}: the local eth0 is the pin"
    );
    assert_eq!(
        body["devices"]["root"],
        root(),
        "{tag}: the root is the instance's own"
    );
    if !run.stopped {
        assert_eq!(
            run.nic(ADOPT_TO),
            (ADOPT_MAC.to_string(), ADOPT_IP.to_string()),
            "{tag}"
        );
    }
    assert!(run.snapshot_in(ADOPT_TO), "{tag}: the snapshot");
    let last = if run.stopped {
        AdoptStep::Mark
    } else {
        AdoptStep::Start
    };
    assert_eq!(
        run.record().last("svc1"),
        Some((last, Phase::Done)),
        "{tag}"
    );
    assert!(run.record().complete("svc1"), "{tag}");
}

/// The instance is back in `default`, running and unmarked, with the pre-image MAC and IPv4, and its adoption project is gone.
/// The pin and the snapshot are there when their steps ran before the kill. A rooted run's root is the profile's again. A
/// stopped run's instance is stopped, as it was planned, and shows no live MAC: its configured MAC is the pre-image's.
fn assert_released(run: &Run, tag: &str, pinned: bool, snapshotted: bool) {
    assert!(
        run.instance(ADOPT_TO).is_none(),
        "{tag}: the instance left {ADOPT_TO}"
    );
    let body = run
        .instance("default")
        .expect("the instance is back in default");
    assert_eq!(body["status"], json!(run.status()), "{tag}");
    let config = body["config"].as_object().expect("config");
    assert!(
        !config.keys().any(|key| key.starts_with("user.arcakernel.")),
        "{tag}: the instance is Foreign again: {config:?}"
    );
    let uuid = run.record().uuid("svc1").expect("the recorded uuid");
    assert_eq!(body["config"]["volatile.uuid"], json!(uuid), "{tag}");
    assert_eq!(configured_mac(&body), json!(ADOPT_MAC), "{tag}");
    if !run.stopped {
        assert_eq!(
            run.nic("default"),
            (ADOPT_MAC.to_string(), ADOPT_IP.to_string()),
            "{tag}"
        );
    }
    if pinned {
        assert_eq!(body["devices"]["eth0"], pin(), "{tag}: the pin is kept");
    } else {
        assert!(
            body["devices"].get("eth0").is_none(),
            "{tag}: no pin was set"
        );
    }
    assert_eq!(
        run.snapshot_in("default"),
        snapshotted,
        "{tag}: the snapshot"
    );
    assert_eq!(body["expanded_devices"]["root"], root(), "{tag}: the root");
    if run.rooted {
        assert!(
            body["devices"].get("root").is_none(),
            "{tag}: the localized root is taken back off"
        );
    } else {
        assert_eq!(body["devices"]["root"], root(), "{tag}: the own root stays");
    }
    assert!(
        get(&run.fake, &format!("/1.0/projects/{ADOPT_TO}")).is_none(),
        "{tag}: the adoption project is deleted"
    );
    assert_eq!(
        run.fake.leases(),
        vec![(
            "br1".to_string(),
            ADOPT_MAC.to_string(),
            ADOPT_IP.to_string()
        )],
        "{tag}: the lease is unchanged"
    );
    let record = run.record();
    assert_eq!(
        record.last("svc1"),
        Some((AdoptStep::DeleteProject, Phase::Done)),
        "{tag}"
    );
    assert!(record.released("svc1"), "{tag}");
    assert!(record.incomplete().is_empty(), "{tag}");
    assert!(
        record.adopted().is_empty(),
        "{tag}: a released instance is no live entry"
    );
}

/// No write deleted anything but the adoption project, touched a network or a profile, or patched a project, and the only
/// instance PUT is a rooted run's RestoreRoot, in `default`.
fn assert_writes_in_table(run: &Run, tag: &str) {
    for (method, path) in run.writes() {
        if method == "DELETE" {
            assert_eq!(path, format!("/1.0/projects/{ADOPT_TO}"), "{tag}");
        }
        if method == "PUT" && !path.contains("/state") {
            assert!(
                run.rooted && path == "/1.0/instances/svc1?project=default",
                "{tag}: {path}"
            );
        }
        assert!(
            !path.starts_with("/1.0/networks") && !path.starts_with("/1.0/profiles"),
            "{tag}: {method} {path}"
        );
        assert!(
            !(method == "PATCH" && path.starts_with("/1.0/projects")),
            "{tag}: {path}"
        );
    }
}

/// The paths only the preflight reads. A resume reads none of them.
fn preflight_reads(run: &Run, from: usize) -> Vec<String> {
    run.fake.requests()[from..]
        .iter()
        .filter(|request| request.path.contains("/backups") || request.path.contains("/leases"))
        .map(|request| request.path.clone())
        .collect()
}

#[test]
fn the_steps_left_follow_the_recorded_last_step() {
    use AdoptStep as S;
    let target = AdoptTarget {
        instance: "svc1".to_string(),
        to: ADOPT_TO.to_string(),
    };
    let plan = AdoptionPlan::new(&target, pre_image(), Allowances::default()).expect("a plan");
    let all = S::adopt_steps(false);
    assert_eq!(plan.steps, all);
    assert_eq!(resume_steps(&plan, None), all);
    assert_eq!(
        resume_steps(&plan, Some((S::Move, Phase::Intent))),
        vec![S::Move, S::Mark, S::Start]
    );
    assert_eq!(
        resume_steps(&plan, Some((S::Move, Phase::Failed))),
        vec![S::Move, S::Mark, S::Start]
    );
    assert_eq!(
        resume_steps(&plan, Some((S::PinNic, Phase::Done))),
        vec![S::Move, S::Mark, S::Start],
        "no LocalizeRoot for a plan that localizes nothing"
    );
    assert_eq!(
        resume_steps(&plan, Some((S::Move, Phase::Done))),
        vec![S::Mark, S::Start]
    );
    assert_eq!(
        resume_steps(&plan, Some((S::Start, Phase::Done))),
        Vec::<S>::new()
    );
    assert_eq!(
        resume_steps(&plan, Some((S::Unmark, Phase::Done))),
        Vec::<S>::new()
    );

    let left = |last: Option<(S, Phase)>| release_steps(&plan, last);
    let tail = vec![S::ReleaseStart, S::DeleteProject];
    for step in [S::CreateProject, S::Stop, S::Snapshot, S::PinNic] {
        for phase in [Phase::Intent, Phase::Done, Phase::Failed] {
            assert_eq!(left(Some((step, phase))), tail, "{step:?} {phase:?}");
        }
    }
    assert_eq!(left(None), tail);
    for phase in [Phase::Intent, Phase::Done, Phase::Failed] {
        assert_eq!(
            left(Some((S::Move, phase))),
            vec![S::MoveBack, S::ReleaseStart, S::DeleteProject]
        );
        assert_eq!(
            left(Some((S::Mark, phase))),
            vec![S::Unmark, S::MoveBack, S::ReleaseStart, S::DeleteProject]
        );
        assert_eq!(left(Some((S::Start, phase))), S::release_steps(false));
    }
    assert_eq!(
        left(Some((S::ReleaseStop, Phase::Intent))),
        S::release_steps(false)
    );
    let from_unmark = vec![S::Unmark, S::MoveBack, S::ReleaseStart, S::DeleteProject];
    assert_eq!(left(Some((S::ReleaseStop, Phase::Done))), from_unmark);
    assert_eq!(left(Some((S::Unmark, Phase::Failed))), from_unmark);
    let from_move_back = vec![S::MoveBack, S::ReleaseStart, S::DeleteProject];
    assert_eq!(left(Some((S::Unmark, Phase::Done))), from_move_back);
    assert_eq!(left(Some((S::MoveBack, Phase::Intent))), from_move_back);
    assert_eq!(left(Some((S::MoveBack, Phase::Done))), tail);
    assert_eq!(left(Some((S::ReleaseStart, Phase::Intent))), tail);
    assert_eq!(
        left(Some((S::ReleaseStart, Phase::Done))),
        vec![S::DeleteProject],
        "a release recorded before releases deleted the project finishes with DeleteProject"
    );
    assert_eq!(
        left(Some((S::DeleteProject, Phase::Failed))),
        vec![S::DeleteProject]
    );
    assert_eq!(left(Some((S::DeleteProject, Phase::Done))), Vec::<S>::new());

    // A plan that localizes the root runs LocalizeRoot before the move and RestoreRoot after MoveBack, and a release from
    // before the move restores a root LocalizeRoot may have made the instance's own.
    let mut allowances = Allowances::default();
    allowances.localize.insert(
        "root".to_string(),
        serde_json::from_value(root()).expect("a device"),
    );
    let rooted = AdoptionPlan::new(&target, pre_image(), allowances).expect("a rooted plan");
    assert_eq!(rooted.steps, S::ADOPT.to_vec());
    assert_eq!(resume_steps(&rooted, None), S::ADOPT.to_vec());
    assert_eq!(
        resume_steps(&rooted, Some((S::PinNic, Phase::Done))),
        vec![S::LocalizeRoot, S::Move, S::Mark, S::Start]
    );
    assert_eq!(
        resume_steps(&rooted, Some((S::LocalizeRoot, Phase::Intent))),
        vec![S::LocalizeRoot, S::Move, S::Mark, S::Start]
    );
    let left = |last: Option<(S, Phase)>| release_steps(&rooted, last);
    let restore = vec![S::RestoreRoot, S::ReleaseStart, S::DeleteProject];
    for step in [
        S::CreateProject,
        S::Stop,
        S::Snapshot,
        S::PinNic,
        S::LocalizeRoot,
    ] {
        for phase in [Phase::Intent, Phase::Done, Phase::Failed] {
            assert_eq!(left(Some((step, phase))), restore, "{step:?} {phase:?}");
        }
    }
    assert_eq!(left(None), restore);
    assert_eq!(
        left(Some((S::Move, Phase::Done))),
        vec![
            S::MoveBack,
            S::RestoreRoot,
            S::ReleaseStart,
            S::DeleteProject
        ]
    );
    assert_eq!(left(Some((S::Start, Phase::Done))), S::RELEASE.to_vec());
    assert_eq!(left(Some((S::MoveBack, Phase::Done))), restore);
    assert_eq!(left(Some((S::RestoreRoot, Phase::Failed))), restore);
    assert_eq!(left(Some((S::RestoreRoot, Phase::Done))), tail);

    // A plan whose steps are not the ones its allowances give resumes nothing.
    let mut bent = rooted.clone();
    bent.steps = S::adopt_steps(false);
    assert_eq!(resume_steps(&bent, None), Vec::<S>::new());
}

fn pre_image() -> arca_incus::adopt::PreImage {
    serde_json::from_value(json!({
        "uuid": ADOPT_UUID, "createdAt": "", "mac": ADOPT_MAC, "ipv4": ADOPT_IP, "profiles": ["default"],
        "config": {}, "devices": { "eth0": { "type": "nic", "name": "eth0", "network": "br1" } }, "configHash": "",
    }))
    .expect("a pre-image")
}

#[test]
fn every_kill_point_of_an_apply_resumes_to_a_complete_adoption_with_no_step_sent_twice() {
    for (rooted, step) in kill_steps() {
        for kill in KILLS {
            let tag = format!("resume-{rooted}-{step:?}-{kill:?}");
            let run = Run::of(&tag, rooted).halted(&tag, step, kill);
            let from = run.fake.requests().len();
            let resumed = run
                .apply()
                .unwrap_or_else(|f| panic!("{tag}: {f:?}"))
                .expect("a result");
            assert_eq!(resumed["complete"], json!(true), "{tag}");
            assert_eq!(resumed["resumed"], json!(true), "{tag}");
            assert_adopted(&run, &tag);
            assert_eq!(
                run.writes(),
                adoption_writes_of(rooted),
                "{tag}: every step's request is sent once, in order"
            );
            assert!(
                preflight_reads(&run, from).is_empty(),
                "{tag}: the resume ran no preflight"
            );
            assert_writes_in_table(&run, &tag);
        }
    }
}

/// Each adoption step of each fixture's plan: the instance's own root, and a root the default profile gives.
fn kill_steps() -> Vec<(bool, AdoptStep)> {
    [false, true]
        .into_iter()
        .flat_map(|rooted| {
            AdoptStep::adopt_steps(rooted)
                .into_iter()
                .map(move |step| (rooted, step))
        })
        .collect()
}

#[test]
fn every_kill_point_of_an_apply_releases_to_the_pre_image_location_class_mac_and_ipv4() {
    for (rooted, step) in kill_steps() {
        for kill in KILLS {
            let tag = format!("release-{rooted}-{step:?}-{kill:?}");
            let run = Run::of(&tag, rooted).halted(&tag, step, kill);
            let before = run.writes().len();
            let released = run
                .release()
                .unwrap_or_else(|f| panic!("{tag}: {f:?}"))
                .expect("a result");
            assert_eq!(released["released"], json!(true), "{tag}");
            let pinned = sent_before(AdoptStep::PinNic, step, kill);
            let snapshotted = sent_before(AdoptStep::Snapshot, step, kill);
            assert_released(&run, &tag, pinned, snapshotted);
            let mine = run.writes()[before..].to_vec();
            let mut once = mine.clone();
            once.sort();
            once.dedup();
            assert_eq!(
                once.len(),
                mine.len(),
                "{tag}: no release step is sent twice: {mine:?}"
            );
            assert_writes_in_table(&run, &tag);
        }
    }
}

#[test]
fn the_move_mark_window_stopped_unmarked_in_to_is_resumed_and_released() {
    for (index, then_release) in [false, true].into_iter().enumerate() {
        let tag = format!("window-{index}");
        let run = Run::killed(&tag, AdoptStep::Move, Kill::AfterOutcome);
        // The window: the instance is in its adoption project, stopped and unmarked.
        let body = run.instance(ADOPT_TO).expect("in the adoption project");
        assert!(run.instance("default").is_none());
        assert_eq!(body["status"], json!("Stopped"));
        assert!(body["config"].get(MARK).is_none(), "unmarked");
        assert_eq!(
            run.record().last("svc1"),
            Some((AdoptStep::Move, Phase::Done))
        );
        let from = run.fake.requests().len();
        if then_release {
            run.release().expect("the release").expect("a result");
            assert_released(&run, &tag, true, true);
        } else {
            run.apply().expect("the resume").expect("a result");
            assert_adopted(&run, &tag);
            assert_eq!(run.writes(), adoption_writes());
        }
        assert!(
            preflight_reads(&run, from).is_empty(),
            "{tag}: no fresh preflight once Move is recorded"
        );
    }
}

#[test]
fn foreign_in_to_is_accepted_only_for_the_uuid_the_record_holds() {
    let run = Run::killed("uuid", AdoptStep::Move, Kill::AfterOutcome);
    let path = format!("/1.0/instances/svc1?project={ADOPT_TO}");
    assert_eq!(
        run.raw(
            "PATCH",
            &path,
            &json!({ "config": { "volatile.uuid": "00000000-0000-4000-8000-0000000000ff" } })
        ),
        200
    );
    let before = run.writes().len();
    let resumed = run.apply();
    assert!(
        matches!(&resumed, Err(Failure::Runtime(why)) if why.contains("Mark") && why.contains("recorded")),
        "{resumed:?}"
    );
    let released = run.release();
    assert!(
        matches!(&released, Err(Failure::Runtime(why)) if why.contains("recorded")),
        "{released:?}"
    );
    assert_eq!(run.writes().len(), before, "neither sent a write");
    assert_eq!(
        run.record().last("svc1").map(|(_, phase)| phase),
        Some(Phase::Failed)
    );
}

#[test]
fn a_resume_with_a_digest_other_than_the_recorded_plans_is_refused_with_no_write() {
    let run = Run::killed("digest", AdoptStep::PinNic, Kill::AfterOutcome);
    let before = run.writes().len();
    let other = "0".repeat(64);
    let resumed = run.adopt(
        &["apply", "--instance", "svc1", "--plan-digest", &other],
        &mut |_, _| false,
    );
    assert!(
        matches!(&resumed, Err(failure @ Failure::Refused(why)) if why.contains("recorded plan") && failure.code() == 2),
        "{resumed:?}"
    );
    assert_eq!(run.writes().len(), before);
    assert_eq!(
        run.record().last("svc1"),
        Some((AdoptStep::PinNic, Phase::Done)),
        "nothing was recorded"
    );
}

#[test]
fn a_resume_is_refused_once_a_release_has_begun_and_the_release_then_finishes() {
    let run = Run::new("released-first");
    run.apply().expect("the adoption").expect("a result");
    let halted = run.adopt(&["release", "--instance", "svc1"], &mut |step, kill| {
        step == AdoptStep::Unmark && kill == Kill::AfterOutcome
    });
    assert!(
        matches!(&halted, Err(Failure::Runtime(why)) if why.contains("halted")),
        "{halted:?}"
    );
    let before = run.writes().len();
    let resumed = run.apply();
    assert!(
        matches!(&resumed, Err(Failure::Refused(why)) if why.contains("being released")),
        "{resumed:?}"
    );
    assert_eq!(run.writes().len(), before);
    run.release()
        .expect("the release finishes")
        .expect("a result");
    assert_released(&run, "released-first", true, true);
}

#[test]
fn a_held_operation_blocks_the_next_step_of_a_resume_and_of_a_release() {
    for then_release in [false, true] {
        let run = Run::killed("held", AdoptStep::Move, Kill::AfterOutcome);
        let op = run.fake.hold_operation(&ObjectRef {
            kind: IncusKind::Instance,
            project: ADOPT_TO.to_string(),
            parent: None,
            name: "svc1".to_string(),
            uuid: None,
        });
        let before = run.writes().len();
        let blocked = if then_release {
            run.release()
        } else {
            run.apply()
        };
        assert!(
            matches!(&blocked, Err(Failure::Runtime(why)) if why.contains("operation")),
            "{blocked:?}"
        );
        assert_eq!(
            run.writes().len(),
            before,
            "the held operation blocked every write"
        );
        run.fake.release_operation(&op);
        if then_release {
            run.release().expect("the release").expect("a result");
            assert_released(&run, "held-release", true, true);
        } else {
            run.apply().expect("the resume").expect("a result");
            assert_adopted(&run, "held-resume");
        }
    }
}

#[test]
fn release_refuses_with_no_record_or_while_another_adoption_is_open_and_sends_no_write() {
    let run = Run::new("no-record");
    let released = run.release();
    assert!(
        matches!(&released, Err(failure @ Failure::Refused(why)) if why.contains("no adoption") && failure.code() == 2),
        "{released:?}"
    );
    assert!(run.writes().is_empty());

    // svc2's adoption is complete in the record and svc1's is open: releasing svc2 would leave two open.
    let mut value: Value =
        serde_json::from_slice(&fs::read(&run.config).expect("read")).expect("parse");
    value["kernel"]["projects"] = json!([ADOPT_TO, "arcak-adopt-svc2"]);
    value["kernel"]["adopt"] = json!([{ "instance": "svc1", "to": ADOPT_TO }, { "instance": "svc2", "to": "arcak-adopt-svc2" }]);
    fs::write(&run.config, value.to_string()).expect("write");
    let mut record = run.record();
    let svc2 = AdoptTarget {
        instance: "svc2".to_string(),
        to: "arcak-adopt-svc2".to_string(),
    };
    record
        .begin(&AdoptionPlan::new(&svc2, pre_image(), Allowances::default()).expect("a plan"))
        .expect("begin");
    for step in AdoptStep::adopt_steps(false) {
        record.intend(step).expect("intent");
        record.done(step).expect("done");
    }
    let svc1 = AdoptTarget {
        instance: "svc1".to_string(),
        to: ADOPT_TO.to_string(),
    };
    record
        .begin(&AdoptionPlan::new(&svc1, pre_image(), Allowances::default()).expect("a plan"))
        .expect("begin");
    record.intend(AdoptStep::CreateProject).expect("intent");
    let other = adopt_run(&run.config, &["release", "--instance", "svc2"]);
    assert!(
        matches!(&other, Err(Failure::Refused(why)) if why.contains("svc1")),
        "{other:?}"
    );
    assert!(run.writes().is_empty());
}

#[test]
fn release_dry_run_lists_the_inverse_takes_no_lock_and_sends_nothing() {
    let run = Run::new("release-dry");
    run.apply().expect("the adoption").expect("a result");
    let held = lock_state_dir(&run.scratch.path("state")).expect("the lock");
    let before = run.writes().len();
    let dry = adopt_run(&run.config, &["release", "--instance", "svc1", "--dry-run"])
        .expect("the dry run")
        .expect("a result");
    assert_eq!(dry["dryRun"], json!(true));
    assert_eq!(
        dry["steps"],
        json!([
            "releaseStop",
            "unmark",
            "moveBack",
            "releaseStart",
            "deleteProject"
        ])
    );
    assert_eq!(
        dry["allowances"],
        json!({ "project": {}, "localize": {}, "nic": {}, "why": {} }),
        "the dry run shows that the plan allows nothing past the base"
    );
    let requests = dry["requests"].as_array().expect("requests");
    assert_eq!(
        requests.len(),
        6,
        "the stop, the forced stop under its condition, and one request per other step"
    );
    for request in requests {
        if request["method"] == "DELETE" {
            assert_eq!(request["path"], json!(format!("/1.0/projects/{ADOPT_TO}")));
            assert_eq!(request["step"], json!("deleteProject"));
        }
        assert!(
            request["body"].get("devices").is_none(),
            "no release request touches eth0: {request}"
        );
    }
    assert_eq!(run.writes().len(), before, "the dry run sent nothing");
    assert_eq!(
        run.record().last("svc1"),
        Some((AdoptStep::Start, Phase::Done)),
        "and recorded nothing"
    );
    let wet = run.release();
    assert!(
        matches!(&wet, Err(Failure::Runtime(why)) if why.contains("held")),
        "{wet:?}"
    );
    drop(held);

    let window = Run::killed("release-dry-window", AdoptStep::Move, Kill::AfterOutcome);
    let dry = adopt_run(
        &window.config,
        &["release", "--instance", "svc1", "--dry-run"],
    )
    .expect("the dry run")
    .expect("a result");
    assert_eq!(
        dry["steps"],
        json!(["moveBack", "releaseStart", "deleteProject"])
    );
}

#[test]
fn a_resume_dry_run_lists_the_steps_left_and_sends_nothing() {
    let run = Run::killed("resume-dry", AdoptStep::Move, Kill::AfterIntent);
    let held = lock_state_dir(&run.scratch.path("state")).expect("the lock");
    let before = run.writes().len();
    let dry = run
        .adopt(
            &[
                "apply",
                "--instance",
                "svc1",
                "--plan-digest",
                &run.digest,
                "--dry-run",
            ],
            &mut |_, _| false,
        )
        .expect("the dry run")
        .expect("a result");
    assert_eq!(dry["dryRun"], json!(true));
    assert_eq!(dry["steps"], json!(["move", "mark", "start"]));
    assert_eq!(run.writes().len(), before);
    drop(held);
}

#[test]
fn a_completed_release_is_released_again_with_no_write() {
    let run = Run::new("twice");
    run.apply().expect("the adoption").expect("a result");
    run.release().expect("the release").expect("a result");
    let before = run.writes().len();
    let again = run.release().expect("a second release").expect("a result");
    assert_eq!(again["released"], json!(true));
    assert_eq!(again["steps"], json!([]));
    assert_eq!(run.writes().len(), before);
}

#[test]
fn a_full_adopt_then_release_keeps_the_mac_with_volatile_reset_modelled() {
    for rooted in [false, true] {
        full_adopt_then_release(rooted);
    }
}

fn full_adopt_then_release(rooted: bool) {
    let run = Run::of(&format!("full-{rooted}"), rooted);
    let mut seen = Vec::new();
    let mut observe = |step: AdoptStep, kill: Kill| {
        if kill == Kill::AfterOutcome {
            // After every step the instance still has its eth0, whose config still gives the pre-image MAC.
            let body = run
                .instance("default")
                .or_else(|| run.instance(ADOPT_TO))
                .expect("the instance is in one of its projects");
            assert!(
                body["expanded_devices"].get("eth0").is_some(),
                "{step:?} removed eth0"
            );
            assert_eq!(
                configured_mac(&body),
                json!(ADOPT_MAC),
                "{step:?} lost the MAC"
            );
            assert_eq!(
                body["expanded_devices"]["root"],
                root(),
                "{step:?} changed the root"
            );
            seen.push(step);
        }
        false
    };
    run.apply_halting(&mut observe)
        .expect("the adoption")
        .expect("a result");
    run.adopt(&["release", "--instance", "svc1"], &mut observe)
        .expect("the release")
        .expect("a result");
    let mut all = AdoptStep::adopt_steps(rooted);
    all.extend(AdoptStep::release_steps(rooted));
    assert_eq!(seen, all);
    assert_released(&run, "full", true, true);
    assert_writes_in_table(&run, "full");
}

#[test]
fn a_release_whose_pin_was_removed_is_refused_before_any_write() {
    // The control for the pin: the pin's hwaddr replaced the volatile MAC key (deviceVolatileReset), so an eth0 unpinned in the
    // adoption project leaves nothing in the instance's config that gives its MAC. MoveBack would refuse it, so the release
    // refuses before ReleaseStop: it neither stops nor unmarks the instance, and the dry run refuses too.
    let run = Run::new("unpinned");
    run.apply().expect("the adoption").expect("a result");
    let path = format!("/1.0/instances/svc1?project={ADOPT_TO}");
    let unpinned =
        json!({ "devices": { "eth0": { "type": "nic", "name": "eth0", "network": "br1" } } });
    assert_eq!(run.raw("PATCH", &path, &unpinned), 200);
    let body = run.instance(ADOPT_TO).expect("the instance");
    assert_eq!(
        body["devices"]["eth0"], unpinned["devices"]["eth0"],
        "{body}"
    );
    assert_eq!(
        configured_mac(&body),
        Value::Null,
        "the MAC is gone with the pin"
    );
    let before = run.writes().len();
    for dry in [true, false] {
        let mut args = vec!["release", "--instance", "svc1"];
        if dry {
            args.push("--dry-run");
        }
        let released = adopt_run(&run.config, &args);
        assert!(
            matches!(&released, Err(failure @ Failure::Refused(why)) if why.contains("pin") && failure.code() == 2),
            "dry run {dry}: {released:?}"
        );
    }
    assert_eq!(run.writes().len(), before, "the release sent no write");
    let body = run
        .instance(ADOPT_TO)
        .expect("the instance stays in its adoption project");
    assert_eq!(body["status"], json!("Running"), "it was not stopped");
    assert_eq!(
        body["config"][MARK],
        json!("adopt-svc1"),
        "it was not unmarked"
    );
    assert_eq!(
        run.record().last("svc1"),
        Some((AdoptStep::Start, Phase::Done)),
        "nothing was recorded"
    );
}

/// A probe that records the address it was asked for and answers `answer`.
fn probe_answering(
    answer: Result<u16, String>,
) -> impl Fn(SocketAddrV4, &str, Duration) -> Result<u16, String> {
    move |addr, path, timeout| {
        assert_eq!(
            addr,
            SocketAddrV4::new(ADOPT_IP.parse().expect("an address"), 8080),
            "the pinned IP and the port"
        );
        assert_eq!(path, "/");
        assert_eq!(timeout, Duration::from_secs(10));
        answer.clone()
    }
}

#[test]
fn verify_passes_for_the_pinned_mac_and_ipv4_and_an_answer_below_500() {
    let run = Run::new("verify");
    run.apply().expect("the adoption").expect("a result");
    let plan = run.record().plan("svc1").expect("the plan");
    let uuid = run.record().uuid("svc1").expect("the uuid");
    let read = UnixIncus::new(run.fake.socket().to_path_buf());
    for code in [200, 302, 404, 499] {
        let verified = verify_with(
            &read,
            &plan,
            &uuid,
            ADOPT_TO,
            8080,
            "/",
            &probe_answering(Ok(code)),
        )
        .expect("verified");
        assert_eq!(verified.mac, ADOPT_MAC);
        assert_eq!(
            verified.ipv4,
            ADOPT_IP.parse::<Ipv4Addr>().expect("an address")
        );
        assert_eq!(verified.http_status, code);
    }
    // The pre-image's uuid is not the instance's any more: the move was a copy.
    let stale = verify_with(
        &read,
        &plan,
        ADOPT_UUID,
        ADOPT_TO,
        8080,
        "/",
        &probe_answering(Ok(200)),
    );
    assert!(
        matches!(&stale, Err(Failure::Runtime(why)) if why.contains("recorded")),
        "{stale:?}"
    );
    assert!(
        run.fake.writes().len() == adoption_writes().len(),
        "verify sends no write"
    );
}

#[test]
fn verify_fails_on_a_changed_mac_a_changed_ipv4_a_closed_port_or_a_5xx() {
    let run = Run::new("verify-fail");
    run.apply().expect("the adoption").expect("a result");
    let plan = run.record().plan("svc1").expect("the plan");
    let uuid = run.record().uuid("svc1").expect("the uuid");
    let read = UnixIncus::new(run.fake.socket().to_path_buf());
    let fails = |what: &str, project: &str, answer: Result<u16, String>| {
        let result = verify_with(
            &read,
            &plan,
            &uuid,
            project,
            8080,
            "/",
            &probe_answering(answer),
        );
        assert!(
            matches!(&result, Err(failure @ Failure::Runtime(why)) if why.contains(what) && failure.code() == 1),
            "{what}: {result:?}"
        );
    };
    fails("answered 503", ADOPT_TO, Ok(503));
    fails("answered 500", ADOPT_TO, Ok(500));
    fails(
        "did not answer",
        ADOPT_TO,
        Err("the TCP connect was refused".to_string()),
    );
    fails("not in project", "default", Ok(200));

    let path = format!("/1.0/instances/svc1?project={ADOPT_TO}");
    let devices = |eth0: Value| json!({ "devices": { "eth0": eth0, "root": { "type": "disk", "path": "/", "pool": "pool1", "size": "60GiB" } } });
    let mut moved_ip = pin();
    moved_ip["ipv4.address"] = json!("192.0.2.132");
    assert_eq!(run.raw("PATCH", &path, &devices(moved_ip)), 200);
    fails("IPv4", ADOPT_TO, Ok(200));
    let mut moved_mac = pin();
    moved_mac["hwaddr"] = json!("10:66:6a:00:00:02");
    assert_eq!(run.raw("PATCH", &path, &devices(moved_mac)), 200);
    fails("MAC", ADOPT_TO, Ok(200));
}

/// A one-connection HTTP server on the loopback that answers `status`, and its port.
fn answer_once(status: &'static str) -> (u16, thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("the address").port();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
            head.push(byte[0]);
        }
        let _ = stream.write_all(
            format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        );
        String::from_utf8_lossy(&head).to_string()
    });
    (port, server)
}

#[test]
fn the_http_probe_reads_a_local_servers_status_and_fails_on_a_closed_port() {
    let loopback = Ipv4Addr::LOCALHOST;
    let (port, server) = answer_once("200 OK");
    assert_eq!(
        http_probe(
            SocketAddrV4::new(loopback, port),
            "/health",
            Duration::from_secs(10)
        ),
        Ok(200)
    );
    let head = server.join().expect("the server");
    assert!(head.starts_with("GET /health HTTP/1.1\r\n"), "{head}");
    let (port, server) = answer_once("503 Service Unavailable");
    assert_eq!(
        http_probe(
            SocketAddrV4::new(loopback, port),
            "/",
            Duration::from_secs(10)
        ),
        Ok(503)
    );
    server.join().expect("the server");

    let closed = TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port();
    assert!(
        http_probe(
            SocketAddrV4::new(loopback, closed),
            "/",
            Duration::from_secs(2)
        )
        .is_err(),
        "a closed port"
    );

    // A server that accepts and never answers fails at the deadline.
    let silent = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = silent.local_addr().expect("addr").port();
    let started = std::time::Instant::now();
    assert!(
        http_probe(
            SocketAddrV4::new(loopback, port),
            "/",
            Duration::from_millis(300)
        )
        .is_err()
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the deadline holds"
    );
    drop(silent);
}

#[test]
fn verify_refuses_a_path_or_port_that_is_not_plain_and_an_adoption_that_is_not_complete() {
    let run = Run::killed("verify-open", AdoptStep::Mark, Kill::AfterOutcome);
    let verify = |args: &[&str]| {
        let mut all = vec!["verify", "--instance", "svc1"];
        all.extend_from_slice(args);
        adopt_run(&run.config, &all)
    };
    let open = verify(&["--port", "8080"]);
    assert!(
        matches!(&open, Err(Failure::Refused(why)) if why.contains("not complete")),
        "{open:?}"
    );
    for (args, why) in [
        (vec!["--port", "0"], "--port"),
        (vec!["--port", "70000"], "--port"),
        (vec!["--port", "8080", "--http-path", "x"], "--http-path"),
        (vec!["--port", "8080", "--http-path", "/a b"], "--http-path"),
        (
            vec!["--port", "8080", "--http-path", "/a\r\nHost: x"],
            "--http-path",
        ),
    ] {
        let refused = verify(&args);
        assert!(
            matches!(&refused, Err(failure @ Failure::Usage(text)) if text.contains(why) && failure.code() == 2),
            "{args:?}: {refused:?}"
        );
    }
    let plan = run.record().plan("svc1").expect("the plan");
    let uuid = run.record().uuid("svc1").expect("the uuid");
    let read = UnixIncus::new(run.fake.socket().to_path_buf());
    let never = |_: SocketAddrV4, _: &str, _: Duration| -> Result<u16, String> {
        panic!("no probe for a refused path")
    };
    assert!(matches!(
        verify_with(&read, &plan, &uuid, ADOPT_TO, 8080, "nope", &never),
        Err(Failure::Usage(_))
    ));
    assert!(matches!(
        verify_with(&read, &plan, &uuid, ADOPT_TO, 0, "/", &never),
        Err(Failure::Usage(_))
    ));
    // A stopped instance is refused before the probe.
    let stopped = verify_with(&read, &plan, &uuid, ADOPT_TO, 8080, "/", &never);
    assert!(
        matches!(&stopped, Err(Failure::Runtime(why)) if why.contains("not running")),
        "{stopped:?}"
    );
    assert_eq!(run.writes().len(), 6, "verify sent no write");
}

fn viewer() -> Caller {
    Caller {
        token_id: "viewer-1".to_string(),
        scope: Scope::Viewer,
    }
}

/// The owner of `svc1` in `project` and the drift that names `adopt-svc1`, as a report shows them.
fn watched(report: &Value, project: &str) -> (Value, Vec<Value>) {
    let owner = report["projects"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|row| row["project"] == project)
        .flat_map(|row| row["instances"].as_array().cloned().unwrap_or_default())
        .find(|row| row["name"] == "svc1")
        .map(|row| row["owner"].clone())
        .unwrap_or(Value::Null);
    let drift = report["drift"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|drift| drift["entry"] == "adopt-svc1")
        .cloned()
        .collect();
    (owner, drift)
}

#[test]
fn after_adoption_the_watch_shows_the_instance_kernel_owned_with_no_drift() {
    let run = Run::new("watch");
    run.apply().expect("the adoption").expect("a result");
    let cfg = config::load(&run.config).expect("the config");
    let kernel = json!({ "class": "kernel", "entry": "adopt-svc1" });
    let once = || {
        arca::run(
            &[
                "watch",
                "--once",
                "--json",
                "--config",
                &run.config.display().to_string(),
            ]
            .map(str::to_string),
        )
        .expect("the watch")
        .expect("a report")
    };
    let served = || {
        // watch_report never sweeps: one sweep runs first, as the watcher thread's would.
        let handler = watch_handler(&cfg);
        let mut last = None;
        watcher_step(&handler, now_ms(), &mut last);
        handler.call(json!({}), &viewer()).expect("watch_report")
    };
    for report in [once(), served()] {
        assert_eq!(
            watched(&report, ADOPT_TO),
            (kernel.clone(), Vec::new()),
            "{report}"
        );
    }

    // The control: without the record the adopted instance is a mark with no entry.
    let saved = run.scratch.path("adoptions.saved");
    fs::rename(run.record_path(), &saved).expect("move the record away");
    let (_, drift) = watched(&served(), ADOPT_TO);
    assert_eq!(drift.len(), 1, "{drift:?}");
    assert_eq!(drift[0]["kind"], json!("markedWithoutEntry"));
    fs::rename(&saved, run.record_path()).expect("put it back");

    // A live adoption whose instance is gone is an entry without its object.
    let stop = json!({ "action": "stop", "force": true });
    assert_eq!(
        run.raw(
            "PUT",
            &format!("/1.0/instances/svc1/state?project={ADOPT_TO}"),
            &stop
        ),
        202
    );
    let path = format!("/1.0/instances/svc1?project={ADOPT_TO}");
    assert_eq!(run.raw("DELETE", &path, &Value::Null), 202);
    assert!(run.instance(ADOPT_TO).is_none(), "the instance is gone");
    for report in [once(), served()] {
        let (owner, drift) = watched(&report, ADOPT_TO);
        assert_eq!(owner, Value::Null);
        assert_eq!(
            drift,
            vec![json!({ "kind": "entryWithoutObject", "entry": "adopt-svc1" })],
            "{report}"
        );
    }
}

#[test]
fn verify_waits_for_a_slow_start_and_reports_the_last_failure_when_time_runs_out() {
    // The instance is still stopped when verify starts, and starts a moment later, as a service does after Start.
    let run = Run::killed("verify-wait", AdoptStep::Mark, Kill::AfterOutcome);
    let plan = run.record().plan("svc1").expect("the plan");
    let uuid = run.record().uuid("svc1").expect("the uuid");
    let read = UnixIncus::new(run.fake.socket().to_path_buf());
    let socket = run.fake.socket().to_path_buf();
    let starter = thread::spawn(move || {
        thread::sleep(Duration::from_millis(150));
        let start = json!({ "action": "start" });
        raw_at(
            &socket,
            "PUT",
            &format!("/1.0/instances/svc1/state?project={ADOPT_TO}"),
            &start,
        )
    });
    // The probe fails twice once the instance runs, as a service still starting does.
    let calls = AtomicUsize::new(0);
    let slow = |_: SocketAddrV4, _: &str, _: Duration| -> Result<u16, String> {
        match calls.fetch_add(1, Ordering::SeqCst) {
            0 | 1 => Err("the TCP connect failed (connection refused)".to_string()),
            _ => Ok(200),
        }
    };
    let waited = verify_until(
        &read,
        &plan,
        &uuid,
        ADOPT_TO,
        8080,
        "/",
        Duration::from_secs(10),
        Duration::from_millis(10),
        &slow,
    );
    assert_eq!(starter.join().expect("the starter"), 202);
    let verified = waited.expect("verified once the instance runs and answers");
    assert_eq!(verified.http_status, 200);
    assert_eq!(calls.load(Ordering::SeqCst), 3);

    // With no wait, one attempt: its failure is the report.
    let calls = AtomicUsize::new(0);
    let failing = |_: SocketAddrV4, _: &str, _: Duration| -> Result<u16, String> {
        Err(format!(
            "attempt {}",
            calls.fetch_add(1, Ordering::SeqCst) + 1
        ))
    };
    let once = verify_until(
        &read,
        &plan,
        &uuid,
        ADOPT_TO,
        8080,
        "/",
        Duration::ZERO,
        Duration::from_millis(10),
        &failing,
    );
    assert!(
        matches!(&once, Err(Failure::Runtime(why)) if why.contains("attempt 1")),
        "{once:?}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // Out of time, the last failure is reported.
    let calls = AtomicUsize::new(0);
    let failing = |_: SocketAddrV4, _: &str, _: Duration| -> Result<u16, String> {
        Err(format!(
            "attempt {}",
            calls.fetch_add(1, Ordering::SeqCst) + 1
        ))
    };
    let started = Instant::now();
    let late = verify_until(
        &read,
        &plan,
        &uuid,
        ADOPT_TO,
        8080,
        "/",
        Duration::from_millis(200),
        Duration::from_millis(20),
        &failing,
    );
    let tries = calls.load(Ordering::SeqCst);
    assert!(tries >= 2, "it tried again: {tries}");
    assert!(
        matches!(&late, Err(Failure::Runtime(why)) if why.contains(&format!("attempt {tries}"))),
        "{late:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(5));

    // A usage failure is not retried.
    let never =
        |_: SocketAddrV4, _: &str, _: Duration| -> Result<u16, String> { panic!("no probe") };
    let started = Instant::now();
    let bad = verify_until(
        &read,
        &plan,
        &uuid,
        ADOPT_TO,
        8080,
        "nope",
        Duration::from_secs(10),
        Duration::from_millis(10),
        &never,
    );
    assert!(matches!(bad, Err(Failure::Usage(_))));
    assert!(started.elapsed() < Duration::from_secs(1), "at once");
}

#[test]
fn verify_takes_a_wait_in_whole_seconds() {
    let run = Run::killed("verify-wait-flag", AdoptStep::Mark, Kill::AfterOutcome);
    let verify = |wait: &str| {
        adopt_run(
            &run.config,
            &[
                "verify",
                "--instance",
                "svc1",
                "--port",
                "8080",
                "--wait",
                wait,
            ],
        )
    };
    let open = verify("5");
    assert!(
        matches!(&open, Err(Failure::Refused(why)) if why.contains("not complete")),
        "{open:?}"
    );
    for bad in ["x", "-1", "1.5", "3601"] {
        let refused = verify(bad);
        assert!(
            matches!(&refused, Err(failure @ Failure::Usage(text)) if text.contains("--wait") && failure.code() == 2),
            "{bad}: {refused:?}"
        );
    }
}

/// The record a move left when Incus ran it as a copy and the writer of that run could not confirm it: the plan, the facts,
/// four steps done, and the move's intent and failure, twelve lines. The copy is in the adoption project under a new uuid,
/// unmarked, and was started by hand.
fn copied_unconfirmed(tag: &str) -> (Run, String) {
    let run = Run::killed(tag, AdoptStep::Move, Kill::AfterIntent);
    let path = "/1.0/instances/svc1?project=default";
    let moved = json!({ "name": "svc1", "project": ADOPT_TO });
    assert_eq!(run.raw("POST", path, &moved), 202, "{tag}: the copy lands");
    let mut record = run.record();
    record
        .failed(
            AdoptStep::Move,
            "the write landed, but its read-back is not confirmed: the instance does not read back in its new project",
        )
        .expect("the failure line");
    let text = fs::read_to_string(run.record_path()).expect("the record");
    assert_eq!(text.lines().count(), 12, "{tag}: {text}");
    let start = json!({ "action": "start" });
    let state = format!("/1.0/instances/svc1/state?project={ADOPT_TO}");
    assert_eq!(
        run.raw("PUT", &state, &start),
        202,
        "{tag}: started by hand"
    );
    let body = run.instance(ADOPT_TO).expect("the copy");
    assert_eq!(body["status"], json!("Running"));
    assert!(body["config"].get(MARK).is_none(), "{tag}: unmarked");
    let copy = body["config"]["volatile.uuid"]
        .as_str()
        .expect("a uuid")
        .to_string();
    assert_ne!(copy, ADOPT_UUID, "{tag}: the copy has a new uuid");
    (run, copy)
}

#[test]
fn a_resume_of_a_move_incus_ran_as_a_copy_records_the_new_uuid_marks_it_and_sends_no_second_move() {
    let (run, copy) = copied_unconfirmed("copy-resume");
    assert_eq!(
        run.record().last("svc1"),
        Some((AdoptStep::Move, Phase::Failed))
    );
    assert_eq!(run.record().uuid("svc1").as_deref(), Some(ADOPT_UUID));
    let before = run.writes().len();
    let from = run.fake.requests().len();
    let resumed = run.apply().expect("the resume").expect("a result");
    assert_eq!(resumed["resumed"], json!(true));
    assert_eq!(
        run.writes()[before..],
        [(
            "PATCH".to_string(),
            format!("/1.0/instances/svc1?project={ADOPT_TO}")
        )],
        "Mark alone: the move is not sent again and the running copy is not started again"
    );
    let outcomes: Vec<(Value, Value)> = resumed["steps"]
        .as_array()
        .expect("the steps")
        .iter()
        .map(|step| (step["step"].clone(), step["outcome"].clone()))
        .collect();
    assert_eq!(
        outcomes,
        [
            (json!("move"), json!("alreadyDone")),
            (json!("mark"), json!("sent")),
            (json!("start"), json!("alreadyDone")),
        ]
    );
    assert!(preflight_reads(&run, from).is_empty(), "no fresh preflight");
    assert_eq!(run.record().uuid("svc1"), Some(copy.clone()));
    let text = fs::read_to_string(run.record_path()).expect("the record");
    let kinds: Vec<Value> = text
        .lines()
        .skip(12)
        .map(|line| {
            let line: Value = serde_json::from_str(line).expect("a line");
            json!([line["kind"], line["step"], line["phase"]])
        })
        .collect();
    assert_eq!(
        kinds,
        [
            json!(["step", "move", "intent"]),
            json!(["uuid", "move", null]),
            json!(["step", "move", "done"]),
            json!(["step", "mark", "intent"]),
            json!(["step", "mark", "done"]),
            json!(["step", "start", "intent"]),
            json!(["step", "start", "done"]),
        ],
        "the new uuid is recorded before the move's outcome"
    );
    assert_adopted(&run, "copy-resume");
    assert_writes_in_table(&run, "copy-resume");

    // verify, the watch's view and the release all follow the recorded uuid. MoveBack is a copy too, and its uuid is recorded.
    let plan = run.record().plan("svc1").expect("the plan");
    let read = UnixIncus::new(run.fake.socket().to_path_buf());
    verify_with(
        &read,
        &plan,
        &copy,
        ADOPT_TO,
        8080,
        "/",
        &probe_answering(Ok(200)),
    )
    .expect("verified under the recorded uuid");
    let status = adopt_run(&run.config, &["status", "--instance", "svc1", "--json"])
        .expect("the status")
        .expect("a result");
    assert_eq!(status["uuid"], json!(copy));
    run.release().expect("the release").expect("a result");
    assert_released(&run, "copy-resume", true, true);
    let back = run.record().uuid("svc1").expect("the uuid");
    assert_ne!(back, copy, "the move back was a copy too");
}

#[test]
fn a_resume_of_an_unconfirmed_copy_whose_pin_differs_is_refused_with_no_write() {
    let (run, _copy) = copied_unconfirmed("copy-unpinned");
    let path = format!("/1.0/instances/svc1?project={ADOPT_TO}");
    let unpinned =
        json!({ "devices": { "eth0": { "type": "nic", "name": "eth0", "network": "br1" } } });
    assert_eq!(run.raw("PATCH", &path, &unpinned), 200);
    let before = run.writes().len();
    let resumed = run.apply();
    assert!(
        matches!(&resumed, Err(Failure::Runtime(why)) if why.contains("Move") && why.contains("pin")),
        "{resumed:?}"
    );
    assert_eq!(run.writes().len(), before, "no write");
    assert_eq!(run.record().uuid("svc1").as_deref(), Some(ADOPT_UUID));
    assert_eq!(
        run.record().last("svc1"),
        Some((AdoptStep::Move, Phase::Failed))
    );
}

#[test]
fn a_rooted_release_whose_profile_root_or_own_root_changed_is_refused_before_any_write() {
    let run = Run::rooted("rooted-changed");
    run.apply().expect("the adoption").expect("a result");
    assert_eq!(run.writes(), adoption_writes_of(true));
    let refused_with = |run: &Run, needle: &str| {
        let before = run.writes().len();
        for dry in [true, false] {
            let mut args = vec!["release", "--instance", "svc1"];
            if dry {
                args.push("--dry-run");
            }
            let released = adopt_run(&run.config, &args);
            assert!(
                matches!(&released, Err(failure @ Failure::Refused(why)) if why.contains(needle) && why.contains("before ReleaseStop") && failure.code() == 2),
                "dry run {dry}: {released:?}"
            );
        }
        assert_eq!(run.writes().len(), before, "the release sent no write");
        let body = run.instance(ADOPT_TO).expect("still adopted");
        assert_eq!(body["status"], json!("Running"), "it was not stopped");
        assert_eq!(body["config"][MARK], json!("adopt-svc1"), "nor unmarked");
        assert_eq!(
            run.record().last("svc1"),
            Some((AdoptStep::Start, Phase::Done)),
            "nothing was recorded"
        );
    };

    // The default profile no longer gives the root the adoption localized: taking the instance's own off would change it.
    let mut smaller = root();
    smaller["size"] = json!("10GiB");
    let profile = |root: Value| {
        json!({ "name": "default", "config": {}, "devices": {
            "eth0": { "type": "nic", "name": "eth0", "network": "br1" }, "root": root } })
    };
    run.fake.add_profile("default", profile(smaller));
    refused_with(&run, "no longer give");
    run.fake.add_profile("default", profile(root()));

    // The instance's own root changed while adopted: the release would keep it, so it does not begin.
    let path = format!("/1.0/instances/svc1?project={ADOPT_TO}");
    let mut bigger = root();
    bigger["size"] = json!("80GiB");
    assert_eq!(
        run.raw("PATCH", &path, &json!({ "devices": { "root": bigger } })),
        200
    );
    refused_with(&run, "changed since the adoption");
    assert_eq!(
        run.raw("PATCH", &path, &json!({ "devices": { "root": root() } })),
        200
    );

    // Put back, the release runs and takes the localized root off with one PUT of the instance in default.
    let before = run.writes().len();
    let released = run.release().expect("the release").expect("a result");
    let steps: Vec<Value> = released["steps"]
        .as_array()
        .expect("the steps")
        .iter()
        .map(|step| step["step"].clone())
        .collect();
    assert_eq!(
        steps,
        [
            json!("releaseStop"),
            json!("unmark"),
            json!("moveBack"),
            json!("restoreRoot"),
            json!("releaseStart"),
            json!("deleteProject")
        ]
    );
    let puts: Vec<(String, String)> = run.writes()[before..]
        .iter()
        .filter(|(method, path)| method == "PUT" && !path.contains("/state"))
        .cloned()
        .collect();
    assert_eq!(
        puts,
        [(
            "PUT".to_string(),
            "/1.0/instances/svc1?project=default".to_string()
        )]
    );
    assert_released(&run, "rooted-changed", true, true);
    assert_writes_in_table(&run, "rooted-changed");
}

/// The URL the URL tests check through: a reverse proxy in front of the adopted service.
const PROXY_URL: &str = "http://proxy.example:8443/health";

fn proxy_url() -> HttpUrl {
    HttpUrl::parse(PROXY_URL).expect("a plain http URL")
}

/// A URL probe that checks it was asked for `PROXY_URL` and answers `answer`.
fn url_answering(
    answer: Result<u16, String>,
) -> impl Fn(&HttpUrl, Duration) -> Result<u16, String> {
    move |url, timeout| {
        assert_eq!(url.to_string(), PROXY_URL);
        assert_eq!(timeout, Duration::from_secs(10));
        answer.clone()
    }
}

#[test]
fn a_url_is_plain_http_with_a_host_an_optional_port_and_a_plain_path() {
    let url = HttpUrl::parse("HTTP://Proxy.example:8443/x?y=1").expect("parsed");
    assert_eq!(
        (url.host.as_str(), url.port, url.path.as_str()),
        ("Proxy.example", 8443, "/x?y=1")
    );
    assert_eq!(
        url.to_string(),
        "http://Proxy.example:8443/x?…",
        "the query, which may carry a secret, is never shown"
    );
    let url = HttpUrl::parse("http://192.0.2.10").expect("parsed");
    assert_eq!(
        (url.host.as_str(), url.port, url.path.as_str()),
        ("192.0.2.10", 80, "/")
    );
    assert_eq!(url.to_string(), "http://192.0.2.10/");
    let url = HttpUrl::parse("http://proxy.example?q=1").expect("parsed");
    assert_eq!(url.path, "/?q=1");
    let url = HttpUrl::parse("http://[2001:db8::1]:8080/h").expect("parsed");
    assert_eq!((url.host.as_str(), url.port), ("[2001:db8::1]", 8080));

    let refused = |text: &str, why: &str| {
        let result = HttpUrl::parse(text);
        assert!(
            matches!(&result, Err(failure @ Failure::Usage(message)) if message.contains(why) && failure.code() == 2),
            "{text:?}: {result:?}"
        );
        if let Err(Failure::Usage(message)) = &result {
            assert!(
                !message.contains("secret"),
                "the URL is not echoed: {message}"
            );
        }
    };
    refused("https://proxy.example/", "https");
    for text in [
        "ftp://proxy.example/",
        "proxy.example/",
        "http://",
        "http:///x",
        "http://user:secret@proxy.example/",
        "http://proxy.example:0/",
        "http://proxy.example:70000/",
        "http://proxy.example:/",
        "http://proxy.example:8x/",
        "http://pro xy.example/",
        "http://proxy_example/",
        "http://proxy.example/a b",
        "http://proxy.example/a#secret",
        "http://proxy.example/a\r\nHost: x",
        "http://[2001:db8::1/",
        "http://[secret]/",
    ] {
        refused(text, "--url");
    }
    let long = format!("http://proxy.example/{}", "a".repeat(1100));
    refused(&long, "--url");
}

#[test]
fn verify_through_a_url_passes_on_2xx_or_3xx_and_fails_on_any_other_answer() {
    let run = Run::new("verify-url");
    run.apply().expect("the adoption").expect("a result");
    let plan = run.record().plan("svc1").expect("the plan");
    let uuid = run.record().uuid("svc1").expect("the uuid");
    let read = UnixIncus::new(run.fake.socket().to_path_buf());
    let url = proxy_url();
    for code in [200, 204, 301, 302, 399] {
        let verified = verify_url_with(
            &read,
            &plan,
            &uuid,
            ADOPT_TO,
            &url,
            &url_answering(Ok(code)),
        )
        .expect("verified");
        assert_eq!(verified.mac, ADOPT_MAC);
        assert_eq!(
            verified.ipv4,
            ADOPT_IP.parse::<Ipv4Addr>().expect("an address")
        );
        assert_eq!(verified.http_status, code);
    }
    for code in [100, 400, 401, 404, 500, 503] {
        let failed = verify_url_with(
            &read,
            &plan,
            &uuid,
            ADOPT_TO,
            &url,
            &url_answering(Ok(code)),
        );
        assert!(
            matches!(&failed, Err(failure @ Failure::Runtime(why)) if why.contains(&format!("answered {code}")) && failure.code() == 1),
            "{code}: {failed:?}"
        );
    }
    let silent = verify_url_with(
        &read,
        &plan,
        &uuid,
        ADOPT_TO,
        &url,
        &url_answering(Err(
            "the TCP connect failed (connection refused)".to_string()
        )),
    );
    assert!(
        matches!(&silent, Err(Failure::Runtime(why)) if why.contains("did not answer") && why.contains(PROXY_URL)),
        "{silent:?}"
    );

    // The instance checks still come first: a stale uuid, the wrong project or a moved MAC fails before any probe.
    let never = |_: &HttpUrl, _: Duration| -> Result<u16, String> { panic!("no probe") };
    let stale = verify_url_with(&read, &plan, ADOPT_UUID, ADOPT_TO, &url, &never);
    assert!(
        matches!(&stale, Err(Failure::Runtime(why)) if why.contains("recorded")),
        "{stale:?}"
    );
    let elsewhere = verify_url_with(&read, &plan, &uuid, "default", &url, &never);
    assert!(
        matches!(&elsewhere, Err(Failure::Runtime(why)) if why.contains("not in project")),
        "{elsewhere:?}"
    );
    let path = format!("/1.0/instances/svc1?project={ADOPT_TO}");
    let mut moved_mac = pin();
    moved_mac["hwaddr"] = json!("10:66:6a:00:00:02");
    let devices = json!({ "devices": { "eth0": moved_mac, "root": { "type": "disk", "path": "/", "pool": "pool1", "size": "60GiB" } } });
    let writes = run.fake.writes().len();
    assert_eq!(run.raw("PATCH", &path, &devices), 200);
    let moved = verify_url_with(&read, &plan, &uuid, ADOPT_TO, &url, &never);
    assert!(
        matches!(&moved, Err(Failure::Runtime(why)) if why.contains("MAC")),
        "{moved:?}"
    );
    assert_eq!(run.fake.writes().len(), writes + 1, "verify sends no write");
}

#[test]
fn verify_through_a_url_waits_for_a_slow_start_and_retries_a_refused_status() {
    let run = Run::killed("verify-url-wait", AdoptStep::Mark, Kill::AfterOutcome);
    let plan = run.record().plan("svc1").expect("the plan");
    let uuid = run.record().uuid("svc1").expect("the uuid");
    let read = UnixIncus::new(run.fake.socket().to_path_buf());
    let socket = run.fake.socket().to_path_buf();
    let starter = thread::spawn(move || {
        thread::sleep(Duration::from_millis(150));
        let start = json!({ "action": "start" });
        raw_at(
            &socket,
            "PUT",
            &format!("/1.0/instances/svc1/state?project={ADOPT_TO}"),
            &start,
        )
    });
    // Once the instance runs, the proxy first cannot reach it and then answers.
    let calls = AtomicUsize::new(0);
    let slow = |_: &HttpUrl, _: Duration| -> Result<u16, String> {
        match calls.fetch_add(1, Ordering::SeqCst) {
            0 => Err("no answer (timed out)".to_string()),
            1 => Ok(502),
            _ => Ok(200),
        }
    };
    let url = proxy_url();
    let waited = verify_url_until(
        &read,
        &plan,
        &uuid,
        ADOPT_TO,
        &url,
        Duration::from_secs(10),
        Duration::from_millis(10),
        &slow,
    );
    assert_eq!(starter.join().expect("the starter"), 202);
    assert_eq!(waited.expect("verified").http_status, 200);
    assert_eq!(calls.load(Ordering::SeqCst), 3);

    // With no wait, one attempt, and its failure is the report.
    let calls = AtomicUsize::new(0);
    let refusing = |_: &HttpUrl, _: Duration| -> Result<u16, String> {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(403)
    };
    let once = verify_url_until(
        &read,
        &plan,
        &uuid,
        ADOPT_TO,
        &url,
        Duration::ZERO,
        Duration::from_millis(10),
        &refusing,
    );
    assert!(
        matches!(&once, Err(Failure::Runtime(why)) if why.contains("answered 403")),
        "{once:?}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn the_url_probe_gets_the_urls_path_with_its_host_and_fails_on_a_closed_port() {
    let (port, server) = answer_once("204 No Content");
    let url = HttpUrl::parse(&format!("http://127.0.0.1:{port}/health?deep=1")).expect("a URL");
    assert_eq!(url_probe(&url, Duration::from_secs(10)), Ok(204));
    let head = server.join().expect("the server");
    assert!(
        head.starts_with("GET /health?deep=1 HTTP/1.1\r\n"),
        "{head}"
    );
    assert!(
        head.contains(&format!("\r\nHost: 127.0.0.1:{port}\r\n")),
        "{head}"
    );

    // A host name is resolved, and each of its addresses is tried in turn.
    let (port, server) = answer_once("302 Found");
    let url = HttpUrl::parse(&format!("http://localhost:{port}")).expect("a URL");
    assert_eq!(url_probe(&url, Duration::from_secs(10)), Ok(302));
    let head = server.join().expect("the server");
    assert!(head.starts_with("GET / HTTP/1.1\r\n"), "{head}");
    assert!(
        head.contains(&format!("\r\nHost: localhost:{port}\r\n")),
        "{head}"
    );

    let closed = TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port();
    let url = HttpUrl::parse(&format!("http://127.0.0.1:{closed}/")).expect("a URL");
    assert!(
        url_probe(&url, Duration::from_secs(2)).is_err(),
        "a closed port"
    );
}

#[test]
fn verify_takes_exactly_one_of_port_and_url() {
    let run = Run::killed("verify-url-flags", AdoptStep::Mark, Kill::AfterOutcome);
    let verify = |args: &[&str]| {
        let mut all = vec!["verify", "--instance", "svc1"];
        all.extend_from_slice(args);
        adopt_run(&run.config, &all)
    };
    // A URL is taken in place of the port: the incomplete adoption is what refuses.
    let open = verify(&["--url", PROXY_URL]);
    assert!(
        matches!(&open, Err(Failure::Refused(why)) if why.contains("not complete")),
        "{open:?}"
    );
    for (args, why) in [
        (vec![], "--url"),
        (vec!["--port", "8080", "--url", PROXY_URL], "--url"),
        (vec!["--url", "https://proxy.example/"], "https"),
        (vec!["--url", "proxy.example"], "--url"),
        (vec!["--url", PROXY_URL, "--http-path", "/"], "--http-path"),
        (vec!["--url", PROXY_URL, "--wait", "x"], "--wait"),
    ] {
        let refused = verify(&args);
        assert!(
            matches!(&refused, Err(failure @ Failure::Usage(text)) if text.contains(why) && failure.code() == 2),
            "{args:?}: {refused:?}"
        );
    }
    assert_eq!(run.writes().len(), 6, "verify sent no write");
}

#[test]
fn verify_through_a_url_reports_the_url_and_the_status_it_answered() {
    let run = Run::new("verify-url-run");
    run.apply().expect("the adoption").expect("a result");
    let (port, server) = answer_once("200 OK");
    let url = format!("http://127.0.0.1:{port}/health");
    let verified = adopt_run(
        &run.config,
        &["verify", "--instance", "svc1", "--url", &url, "--wait", "0"],
    )
    .expect("verified")
    .expect("a result");
    server.join().expect("the server");
    assert_eq!(
        verified,
        json!({
            "instance": "svc1", "project": ADOPT_TO, "mac": ADOPT_MAC, "ipv4": ADOPT_IP,
            "url": url, "httpStatus": 200, "verified": true,
        })
    );
    let (port, server) = answer_once("404 Not Found");
    let url = format!("http://127.0.0.1:{port}/health");
    let failed = adopt_run(
        &run.config,
        &["verify", "--instance", "svc1", "--url", &url, "--wait", "0"],
    );
    server.join().expect("the server");
    assert!(
        matches!(&failed, Err(failure @ Failure::Runtime(why)) if why.contains("answered 404") && failure.code() == 1),
        "{failed:?}"
    );
}

// A stopped run: its plan has no Stop and no Start, Mark completes it, and its release leaves it stopped.

#[test]
fn the_steps_left_of_a_stopped_plan_follow_the_recorded_last_step() {
    use AdoptStep as S;
    let target = AdoptTarget {
        instance: "svc1".to_string(),
        to: ADOPT_TO.to_string(),
    };
    let mut pre = pre_image();
    pre.stopped = true;
    let plan = AdoptionPlan::new(&target, pre.clone(), Allowances::default()).expect("a plan");
    let running =
        AdoptionPlan::new(&target, pre_image(), Allowances::default()).expect("a running plan");
    assert_ne!(
        plan.digest, running.digest,
        "the digest covers that the instance was stopped"
    );
    assert!(plan.verify());
    let all = vec![S::CreateProject, S::Snapshot, S::PinNic, S::Move, S::Mark];
    assert_eq!(plan.steps, all);
    assert_eq!(plan.adopt_steps(), all);
    assert_eq!(
        plan.release_steps(),
        vec![S::Unmark, S::MoveBack, S::DeleteProject]
    );
    assert_eq!(resume_steps(&plan, None), all);
    assert_eq!(
        resume_steps(&plan, Some((S::CreateProject, Phase::Done))),
        vec![S::Snapshot, S::PinNic, S::Move, S::Mark]
    );
    assert_eq!(
        resume_steps(&plan, Some((S::Move, Phase::Intent))),
        vec![S::Move, S::Mark]
    );
    assert_eq!(
        resume_steps(&plan, Some((S::Mark, Phase::Failed))),
        vec![S::Mark]
    );
    assert_eq!(
        resume_steps(&plan, Some((S::Mark, Phase::Done))),
        Vec::<S>::new()
    );
    assert_eq!(
        resume_steps(&plan, Some((S::Stop, Phase::Done))),
        Vec::<S>::new(),
        "a step the plan does not have leaves nothing to resume"
    );

    let left = |last: Option<(S, Phase)>| release_steps(&plan, last);
    for step in [S::CreateProject, S::Snapshot, S::PinNic] {
        for phase in [Phase::Intent, Phase::Done, Phase::Failed] {
            assert_eq!(
                left(Some((step, phase))),
                vec![S::DeleteProject],
                "{step:?} {phase:?}"
            );
        }
    }
    assert_eq!(left(None), vec![S::DeleteProject]);
    for phase in [Phase::Intent, Phase::Done, Phase::Failed] {
        assert_eq!(
            left(Some((S::Move, phase))),
            vec![S::MoveBack, S::DeleteProject]
        );
        assert_eq!(
            left(Some((S::Mark, phase))),
            vec![S::Unmark, S::MoveBack, S::DeleteProject]
        );
    }
    assert_eq!(
        left(Some((S::Unmark, Phase::Failed))),
        vec![S::Unmark, S::MoveBack, S::DeleteProject]
    );
    assert_eq!(
        left(Some((S::Unmark, Phase::Done))),
        vec![S::MoveBack, S::DeleteProject]
    );
    assert_eq!(
        left(Some((S::MoveBack, Phase::Done))),
        vec![S::DeleteProject]
    );
    assert_eq!(left(Some((S::DeleteProject, Phase::Done))), Vec::<S>::new());

    let mut allowances = Allowances::default();
    allowances.localize.insert(
        "root".to_string(),
        serde_json::from_value(root()).expect("a device"),
    );
    let rooted = AdoptionPlan::new(&target, pre, allowances).expect("a rooted plan");
    assert_eq!(
        rooted.steps,
        vec![
            S::CreateProject,
            S::Snapshot,
            S::PinNic,
            S::LocalizeRoot,
            S::Move,
            S::Mark
        ]
    );
    let left = |last: Option<(S, Phase)>| release_steps(&rooted, last);
    assert_eq!(left(None), vec![S::RestoreRoot, S::DeleteProject]);
    assert_eq!(
        left(Some((S::Mark, Phase::Done))),
        vec![S::Unmark, S::MoveBack, S::RestoreRoot, S::DeleteProject]
    );

    // A stopped plan whose steps are a running plan's resumes nothing.
    let mut bent = plan.clone();
    bent.steps = S::adopt_steps(false);
    assert_eq!(resume_steps(&bent, None), Vec::<S>::new());
}

/// Each adoption step of each stopped fixture's plan: the instance's own root, and a root the default profile gives.
fn stopped_kill_steps() -> Vec<(bool, AdoptStep)> {
    [false, true]
        .into_iter()
        .flat_map(|rooted| {
            AdoptStep::adopt_steps_of(rooted, true)
                .into_iter()
                .map(move |step| (rooted, step))
        })
        .collect()
}

#[test]
fn every_kill_point_of_a_stopped_apply_resumes_to_a_stopped_adoption_with_no_step_sent_twice() {
    for (rooted, step) in stopped_kill_steps() {
        for kill in KILLS {
            let tag = format!("stopped-resume-{rooted}-{step:?}-{kill:?}");
            let run = Run::stopped(&tag, rooted).halted(&tag, step, kill);
            let from = run.fake.requests().len();
            let resumed = run
                .apply()
                .unwrap_or_else(|f| panic!("{tag}: {f:?}"))
                .expect("a result");
            assert_eq!(resumed["complete"], json!(true), "{tag}");
            assert_eq!(resumed["resumed"], json!(true), "{tag}");
            assert_adopted(&run, &tag);
            assert_eq!(
                run.writes(),
                stopped_adoption_writes_of(rooted),
                "{tag}: every step's request is sent once, in order, and none stops or starts it"
            );
            assert!(
                preflight_reads(&run, from).is_empty(),
                "{tag}: the resume ran no preflight"
            );
            assert_writes_in_table(&run, &tag);
        }
    }
}

#[test]
fn every_kill_point_of_a_stopped_apply_releases_to_default_still_stopped() {
    for (rooted, step) in stopped_kill_steps() {
        for kill in KILLS {
            let tag = format!("stopped-release-{rooted}-{step:?}-{kill:?}");
            let run = Run::stopped(&tag, rooted).halted(&tag, step, kill);
            let before = run.writes().len();
            let released = run
                .release()
                .unwrap_or_else(|f| panic!("{tag}: {f:?}"))
                .expect("a result");
            assert_eq!(released["released"], json!(true), "{tag}");
            let pinned = sent_before(AdoptStep::PinNic, step, kill);
            let snapshotted = sent_before(AdoptStep::Snapshot, step, kill);
            assert_released(&run, &tag, pinned, snapshotted);
            let mine = run.writes()[before..].to_vec();
            let mut once = mine.clone();
            once.sort();
            once.dedup();
            assert_eq!(
                once.len(),
                mine.len(),
                "{tag}: no release step is sent twice: {mine:?}"
            );
            assert!(
                run.writes()
                    .iter()
                    .all(|(method, path)| !(method == "PUT" && path.contains("/state"))),
                "{tag}: nothing stopped or started it: {:?}",
                run.writes()
            );
            assert_writes_in_table(&run, &tag);
        }
    }
}

#[test]
fn verify_through_a_url_never_shows_its_query_which_may_carry_a_secret() {
    const SECRET: &str = "s3cret";
    let url = HttpUrl::parse(&format!("http://proxy.example:8443/health?token={SECRET}"))
        .expect("a plain http URL");
    assert_eq!(
        url.path,
        format!("/health?token={SECRET}"),
        "the request still carries the query"
    );
    assert_eq!(url.to_string(), "http://proxy.example:8443/health?…");
    let bare = HttpUrl::parse(&format!("http://proxy.example?{SECRET}")).expect("parsed");
    assert_eq!(bare.to_string(), "http://proxy.example/?…");
    let plain = HttpUrl::parse("http://proxy.example/health").expect("parsed");
    assert_eq!(
        plain.to_string(),
        "http://proxy.example/health",
        "no query, nothing hidden"
    );

    let run = Run::new("verify-url-secret");
    run.apply().expect("the adoption").expect("a result");
    let plan = run.record().plan("svc1").expect("the plan");
    let uuid = run.record().uuid("svc1").expect("the uuid");
    let read = UnixIncus::new(run.fake.socket().to_path_buf());
    let silent = |_: &HttpUrl, _: Duration| -> Result<u16, String> { Err("no answer".to_string()) };
    let failing = |_: &HttpUrl, _: Duration| -> Result<u16, String> { Ok(503) };
    for probe in [&silent as UrlProbeRef<'_>, &failing] {
        let failed = verify_url_with(&read, &plan, &uuid, ADOPT_TO, &url, probe);
        match &failed {
            Err(Failure::Runtime(why)) => {
                assert!(!why.contains(SECRET), "the query is not shown: {why}");
                assert!(why.contains("/health?…"), "{why}");
            }
            other => panic!("the verify fails: {other:?}"),
        }
    }

    // Through the command: the request carries the query, and neither the answer nor a failure shows it.
    let (port, server) = answer_once("200 OK");
    let text = format!("http://127.0.0.1:{port}/health?token={SECRET}");
    let verified = adopt_run(
        &run.config,
        &[
            "verify",
            "--instance",
            "svc1",
            "--url",
            &text,
            "--wait",
            "0",
        ],
    )
    .expect("verified")
    .expect("a result");
    let request = server.join().expect("the server");
    assert!(
        request.starts_with(&format!("GET /health?token={SECRET} HTTP/1.1\r\n")),
        "{request}"
    );
    assert_eq!(
        verified["url"],
        json!(format!("http://127.0.0.1:{port}/health?…"))
    );
    assert!(!verified.to_string().contains(SECRET), "{verified}");
    let (port, server) = answer_once("503 Service Unavailable");
    let text = format!("http://127.0.0.1:{port}/health?token={SECRET}");
    let failed = adopt_run(
        &run.config,
        &[
            "verify",
            "--instance",
            "svc1",
            "--url",
            &text,
            "--wait",
            "0",
        ],
    );
    server.join().expect("the server");
    assert!(
        matches!(&failed, Err(Failure::Runtime(why)) if why.contains("answered 503") && !why.contains(SECRET)),
        "{failed:?}"
    );
}

/// A URL probe, as a reference.
type UrlProbeRef<'a> = &'a dyn Fn(&HttpUrl, Duration) -> Result<u16, String>;
