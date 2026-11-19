//! The second kind of service command (Rule 82): a command a template's formula declares, run in a `system` or an OCI
//! container of the node's own service project on the fake Incus. Its exit code and its in-instance limit, the hung mark
//! it keeps and what clears it, the tail of its output and how that is masked, an instance that changed meanwhile, and
//! the refusals that stay. No verb calls it yet. Every value is synthetic.
use super::*;
use arca::app_formula::{AppUpdate, AppVersion};
use arca::service_exec::{
    AUDIT_FILE, DeclaredCommand, ExecError, ExecTarget, FormulaPart, FormulaRun, OutputCut,
    ServiceExec, masked_lines,
};
use arca_incus::{
    exec::{ExecCommand, FormulaCommand},
    fake::ExecScript,
};
use std::time::{Duration, Instant};

const STORED: &str = "synthetic-stored-value-4b1c9e";
const ENV_VALUE: &str = "synthetic-environment-value-7d2a";
/// A stored value with a line break written out in it (a backslash and an `n`), its first 16 characters a key's line.
const ESCAPED: &str = r"ABCDEFGHIJKLMNOP\ndemo-secret-suffix";
/// A stored value, and one that holds the marker that hides a value followed by the first letters of the other.
const SHORT: &str = "abcdefghij";
const MARKED: &str = "[secret]abc";

fn formula(kind: &str) -> AppUpdate {
    let mut value = json!({
        "health": { "command": ["/usr/bin/curl", "--fail", "http://localhost/ready"], "seconds": 300 },
        "version": { "command": ["/usr/bin/cat", "/opt/app/version.txt"] }
    });
    if kind == "system" {
        value["update"] =
            json!({ "command": ["/usr/local/sbin/app-update", "--to={version}"], "seconds": 3600 });
    }
    serde_json::from_value(value).expect("a formula")
}

fn update(version: &str) -> DeclaredCommand {
    DeclaredCommand::update(
        &formula("system"),
        "system",
        &AppVersion::parse(version).unwrap(),
    )
    .expect("the update command")
}

fn health(kind: &str) -> DeclaredCommand {
    DeclaredCommand::health(&formula(kind), kind).expect("the health check")
}

fn version(kind: &str) -> DeclaredCommand {
    DeclaredCommand::version(&formula(kind), kind).expect("the version reader")
}

fn operator(rig: &Rig, verb: &str, args: Value) -> Result<Value, CallError> {
    rig.handlers[verb].call(
        args,
        &Caller {
            token_id: "operator".into(),
            scope: Scope::Operator,
            tools: None,
            limits: Default::default(),
        },
    )
}

/// A running service `app` of `kind`, with one stored secret and one environment value of its own.
fn service(kind: &str) -> Rig {
    service_storing(kind, &[STORED])
}

/// A running service `app` of `kind` with one environment value of its own, and each of `values` stored as a secret.
fn service_storing(kind: &str, values: &[&str]) -> Rig {
    let rig = Rig::new();
    let keys: Vec<String> = (0..values.len()).map(|n| format!("SECRET_{n}")).collect();
    let mut template = json!({
        "name": "app", "kind": kind, "ports": [], "env": { "APP_URL": ENV_VALUE },
        "secrets": keys
    });
    if kind == "oci" {
        template["image"] = json!("registry.example/app");
        template["digest"] = json!(DIGEST);
    } else {
        template["image"] = json!("images:debian/13");
    }
    rig.template("app", template);
    rig.call("service_create", json!({"name":"app","template":"app"}))
        .expect("the service");
    for (key, value) in keys.iter().zip(values) {
        let secret = json!({"name":"app","key":key,"value":value});
        operator(&rig, "service_secret_set", secret).expect("the secret");
    }
    rig
}

fn seam(rig: &Rig) -> (ServiceExec, ExecTarget) {
    let cfg: NodeConfig = serde_json::from_value(json!({"stateDir":rig.dir,"incus":{"socket":rig.fake.socket()},"listen":{"unix":rig.dir.join("arca.sock")},"tokens":{"hashDir":rig.dir.join("tokens")},"kernel":{"projects":[PROJECT],"droiddProjects":["other"],"serviceCidr":"192.0.2.0/24"}})).unwrap();
    let config = rig.service_config("app");
    let kernel = rig.kernel.lock().unwrap();
    let entry = EntryId("service-app".into());
    let target = ExecTarget {
        uuid: kernel.service_uuid(&entry, "app"),
        entry,
        service: config,
        retained: kernel.retained_entries().unwrap(),
    };
    (ServiceExec::new(Arc::new(cfg)), target)
}

fn script(rig: &Rig, command: &DeclaredCommand, exit: i64, stdout: &[u8], stderr: &[u8]) {
    let mut script = ExecScript::new(command.argv(), exit, "");
    script.stdout = stdout.to_vec();
    script.stderr = stderr.to_vec();
    rig.fake.script_exec(script);
}

fn run(
    seam: &ServiceExec,
    target: &ExecTarget,
    command: &DeclaredCommand,
) -> Result<FormulaRun, ExecError> {
    seam.run_formula(target, "doctor", "app_update", command, None)
}

/// The exec requests the fake received, each as its body.
fn execs(rig: &Rig) -> Vec<Value> {
    rig.fake
        .service_requests()
        .into_iter()
        .filter(|r| r.path.contains("/exec?") && r.method == "POST")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

fn argv(body: &Value) -> Vec<String> {
    body["command"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn a_declared_command_runs_in_a_system_and_an_oci_container_and_answers_its_exit_code() {
    for kind in ["system", "oci"] {
        let rig = service(kind);
        let (seam, target) = seam(&rig);
        script(&rig, &health(kind), 0, b"ok\n", b"");
        let passed = run(&seam, &target, &health(kind)).expect("a health check that passes");
        assert_eq!((passed.exit, passed.past_limit), (0, false), "{kind}");
        assert!(
            passed.lines.is_empty(),
            "a passed check keeps no line: {kind}"
        );
        script(&rig, &health(kind), 7, b"", b"connection refused\n");
        let failed = run(&seam, &target, &health(kind)).expect("a failed check is an answer");
        assert_eq!(failed.exit, 7, "{kind}");
        assert_eq!(failed.lines, ["connection refused"], "{kind}");
        script(
            &rig,
            &version(kind),
            0,
            b"19.4.8\nbuilt by a machine\n",
            b"",
        );
        let read = run(&seam, &target, &version(kind)).expect("the version");
        assert_eq!(read.version.as_deref(), Some("19.4.8"), "{kind}");
        assert!(
            read.lines.is_empty(),
            "the reader's output is never shown: {kind}"
        );
        // Each was started as the formula declares it, under the in-instance limit, detached, as root, with no
        // environment added; and both recordings of every one are gone.
        let bodies = execs(&rig);
        assert_eq!(bodies.len(), 3, "{kind}");
        for body in &bodies {
            assert_eq!(body["environment"], json!({}), "{kind}");
            assert_eq!((&body["user"], &body["group"]), (&json!(0), &json!(0)));
            assert_eq!(body["wait-for-websocket"], json!(false));
            assert_eq!(body["record-output"], json!(true));
            assert_eq!(body["interactive"], json!(false));
        }
        assert_eq!(
            argv(&bodies[0]),
            [
                "/usr/bin/timeout",
                "-k",
                "2",
                "30",
                "/usr/bin/curl",
                "--fail",
                "http://localhost/ready"
            ]
        );
        assert_eq!(
            &argv(&bodies[2])[3..],
            ["15", "/usr/bin/cat", "/opt/app/version.txt"]
        );
        assert_eq!(rig.fake.recordings(), 0, "{kind}");
        // It was waited on through its operation in the node's own project.
        assert!(rig.fake.requests().iter().any(|r| r.method == "GET"
            && r.path.starts_with("/1.0/operations/")
            && r.path.contains("/wait?timeout=")
            && r.path.contains("project=arcak-svc")));
        // The audit holds the steps and their exit codes, and no argument and no output.
        let audit = fs::read_to_string(rig.dir.join(AUDIT_FILE)).unwrap();
        for step in ["formula_health", "formula_version"] {
            assert!(audit.contains(step), "{kind}: {step}");
        }
        for word in [
            "/usr/bin/curl",
            "localhost/ready",
            "connection refused",
            "19.4.8",
        ] {
            assert!(!audit.contains(word), "{kind}: the audit holds {word:?}");
        }
    }
}

#[test]
fn the_update_command_takes_its_version_as_one_argument_and_its_own_limit() {
    let rig = service("system");
    let (seam, target) = seam(&rig);
    let command = update("5:27.0.3-1~ubuntu.24.04");
    assert_eq!(command.part(), FormulaPart::Update);
    script(&rig, &command, 0, b"Setting up app\nDone\n", b"");
    let answer = run(&seam, &target, &command).expect("the update");
    assert_eq!(
        (answer.exit, answer.past_limit, answer.output_cut.is_cut()),
        (0, false, false)
    );
    assert_eq!(answer.lines, ["Setting up app", "Done"]);
    assert_eq!(
        argv(&execs(&rig)[0]),
        [
            "/usr/bin/timeout",
            "-k",
            "2",
            "3600",
            "/usr/local/sbin/app-update",
            "--to=5:27.0.3-1~ubuntu.24.04"
        ]
    );
    // Only a formula that passes its checks, for its kind, makes a command.
    let oci = formula("oci");
    assert!(DeclaredCommand::update(&oci, "oci", &AppVersion::parse("1.0").unwrap()).is_err());
    assert!(DeclaredCommand::health(&formula("system"), "oci").is_err());
    assert!(DeclaredCommand::version(&formula("system"), "virtual-machine").is_err());
    let mut unchecked = formula("system");
    unchecked.update.as_mut().unwrap().command[0] = "app-update".into();
    assert!(
        DeclaredCommand::update(&unchecked, "system", &AppVersion::parse("1.0").unwrap()).is_err()
    );
    // A limit is only ever lowered.
    assert_eq!(health("oci").within(4).seconds(), 4);
    assert_eq!(health("oci").within(900).seconds(), 30);
    assert_eq!(health("oci").within(0).seconds(), 1);
    // The command type of `arca-incus` checks its list again, whoever built it.
    for (list, seconds) in [
        (vec![], 10),
        (vec!["relative/path".to_string()], 10),
        (vec!["/bin/true".to_string(), String::new()], 10),
        (vec!["/bin/true".to_string(), "a\u{7}b".to_string()], 10),
        (vec!["/bin/true".to_string(), "x".repeat(568)], 10),
        (vec!["/bin/true".to_string(); 33], 10),
        (vec!["/bin/true".to_string()], 0),
        (vec!["/bin/true".to_string()], 7201),
    ] {
        assert!(FormulaCommand::new(list, seconds).is_err());
    }
    assert!(FormulaCommand::new(vec!["/bin/true".into(); 32], 7200).is_ok());
    assert!(FormulaCommand::new(vec!["/bin/true".into(), "x".repeat(567)], 10).is_ok());
}

#[test]
fn a_command_past_its_in_instance_limit_is_told_from_one_that_failed() {
    let rig = service("system");
    let (seam, target) = seam(&rig);
    for (exit, past) in [(124, true), (137, true), (1, false), (0, false)] {
        script(&rig, &update("2.0"), exit, b"unpacking\n", b"");
        let answer = run(&seam, &target, &update("2.0")).expect("an answer");
        assert_eq!((answer.exit, answer.past_limit), (exit, past), "{exit}");
        assert_eq!(answer.lines, ["unpacking"], "{exit}");
    }
    assert_eq!(rig.fake.recordings(), 0);
}

#[test]
fn a_command_incus_does_not_answer_for_marks_the_instance_until_it_is_seen_stopped() {
    let rig = service("system");
    let (seam, target) = seam(&rig);
    let mut held = ExecScript::new(version("system").argv(), 0, "");
    held.held = true;
    rig.fake.script_exec(held);
    let error = seam
        .run_formula(
            &target,
            "doctor",
            "app_update",
            &version("system"),
            Some(Instant::now() + Duration::from_secs(3)),
        )
        .expect_err("Incus does not answer");
    assert!(
        matches!(&error, ExecError::Failed(text) if text.contains("hung")),
        "{error:?}"
    );
    assert!(seam.hung(&target));
    assert_eq!(execs(&rig).len(), 1, "the command was started");
    // No declared command runs in a marked instance, and no closed command but a cleanup.
    for command in [health("system"), update("2.0"), version("system")] {
        assert!(matches!(
            run(&seam, &target, &command),
            Err(ExecError::Refused("hung"))
        ));
    }
    assert!(matches!(
        seam.run(
            &target,
            "viewer",
            "service_logs",
            &ExecCommand::TailscaleVersion,
            &[],
            None
        ),
        Err(ExecError::Refused("hung"))
    ));
    assert_eq!(execs(&rig).len(), 1, "nothing more was sent");
    // While the instance runs the mark stays: nothing has ended the command.
    assert!(!seam.clear_hung_after_stop(&target).unwrap());
    assert!(seam.hung(&target));
    assert!(matches!(
        run(&seam, &target, &health("system")),
        Err(ExecError::Refused("hung"))
    ));
    // Seen stopped, it is cleared; started again, a declared command runs.
    rig.fake.set_instance_status(PROJECT, "app", "Stopped");
    assert!(matches!(
        run(&seam, &target, &health("system")),
        Err(ExecError::Refused("stopped"))
    ));
    assert!(seam.clear_hung_after_stop(&target).unwrap());
    assert!(!seam.hung(&target));
    rig.fake.set_instance_status(PROJECT, "app", "Running");
    script(&rig, &health("system"), 0, b"", b"");
    assert_eq!(run(&seam, &target, &health("system")).unwrap().exit, 0);
    assert_eq!(execs(&rig).len(), 2);
}

#[test]
fn an_output_over_the_cap_keeps_its_tail_and_each_part_keeps_its_own_lines() {
    let rig = service("system");
    let (seam, target) = seam(&rig);
    // 200 KiB of numbered lines on the output and on the error output: the last 64 KiB of each is kept, which is no
    // error, and the error output's lines come last.
    let lines = |tag: &str| -> Vec<u8> {
        (0..20_000)
            .map(|n| format!("{tag} line {n:05}\n"))
            .collect::<String>()
            .into_bytes()
    };
    let (out, err) = (lines("out"), lines("err"));
    assert!(out.len() > 3 * 64 * 1024);
    script(&rig, &update("2.0"), 100, &out, &err);
    let answer = run(&seam, &target, &update("2.0")).expect("a long output is no error");
    assert_eq!(answer.output_cut, OutputCut::FirstLineDropped);
    assert_eq!(answer.exit, 100);
    assert_eq!(answer.lines.len(), 60);
    assert_eq!(answer.lines.first().unwrap(), "err line 19940");
    assert_eq!(answer.lines.last().unwrap(), "err line 19999");
    // A short output is not cut; a long line is cut at 300 characters.
    let long = format!("{}\nend\n", "y".repeat(1000));
    script(&rig, &update("2.0"), 0, long.as_bytes(), b"");
    let answer = run(&seam, &target, &update("2.0")).unwrap();
    assert_eq!(answer.output_cut, OutputCut::Whole);
    assert_eq!(answer.lines, ["y".repeat(300), "end".to_string()]);
    // A failed health check keeps its last 5 lines.
    let many: String = (0..20).map(|n| format!("try {n}\n")).collect();
    script(&rig, &health("system"), 22, many.as_bytes(), b"");
    let answer = run(&seam, &target, &health("system")).unwrap();
    assert_eq!(
        answer.lines,
        ["try 15", "try 16", "try 17", "try 18", "try 19"]
    );
    // The version reader: its first line only, and only when it is a plain version.
    for (stdout, expected) in [
        (b"19.4.8\n".to_vec(), Some("19.4.8")),
        (b"  19.4.8  \nmore\n".to_vec(), Some("19.4.8")),
        (b"gitlab-ce 19.4.8\n".to_vec(), None),
        (b"\n19.4.8\n".to_vec(), None),
        (b"$(id)\n".to_vec(), None),
        (b"".to_vec(), None),
        ([b"19.4.8\n".to_vec(), vec![b'z'; 70 * 1024]].concat(), None),
    ] {
        script(&rig, &version("system"), 0, &stdout, b"");
        let answer = run(&seam, &target, &version("system")).unwrap();
        assert_eq!(
            answer.version.as_deref(),
            expected,
            "{:?}",
            String::from_utf8_lossy(&stdout[..stdout.len().min(30)])
        );
        assert!(answer.lines.is_empty());
    }
    assert_eq!(rig.fake.recordings(), 0);
}

#[test]
fn a_stored_secret_an_environment_value_a_credential_and_a_cut_key_are_masked() {
    for kind in ["system", "oci"] {
        let rig = service(kind);
        let (seam, target) = seam(&rig);
        let word = ["PRI", "VATE"].concat();
        let body: Vec<String> = (0..4)
            .map(|n| format!("{n}AbCdEfGhIjKlMnOpQrStUvWxYz0123456789+/AbCdEfGhIjKlMnOpQrStUv{n}"))
            .collect();
        // The output's tail begins inside a key: its BEGIN line was cut away. A whole key follows on the error output.
        let stdout = format!(
            "{}\n-----END EC {word} KEY-----\nusing {STORED} now\nurl is {ENV_VALUE}\napi_key=synthetic0credential\n\u{1b}[31mred\u{1b}[0m\tdone\n",
            body[..2].join("\n")
        );
        let stderr = format!(
            "-----BEGIN EC {word} KEY-----\n{}\n-----END EC {word} KEY-----\nfailed\n",
            body[2..].join("\n")
        );
        let command = if kind == "system" {
            update("2.0")
        } else {
            health(kind)
        };
        script(&rig, &command, 1, stdout.as_bytes(), stderr.as_bytes());
        let answer = run(&seam, &target, &command).expect("an answer");
        let shown = answer.lines.join("\n");
        for hidden in [STORED, ENV_VALUE, "synthetic0credential"]
            .into_iter()
            .chain(body.iter().map(|line| &line[..20]))
        {
            assert!(
                !shown.contains(hidden),
                "{kind}: {hidden:?} is shown in {shown:?}"
            );
        }
        assert!(
            !shown.contains('\u{1b}'),
            "{kind}: control bytes are cleaned"
        );
        if kind == "system" {
            assert_eq!(
                answer.lines,
                [
                    "[secret]",
                    "using [secret] now",
                    "url is [secret]",
                    "api_key=[secret]",
                    "red done",
                    "[secret]",
                    "failed"
                ]
            );
        } else {
            // A failed health check keeps its last five lines, masked the same way.
            assert_eq!(answer.lines.len(), 5);
            assert_eq!(answer.lines.last().unwrap(), "failed");
        }
    }
}

#[test]
fn an_instance_that_changed_meanwhile_has_its_output_discarded() {
    for exit in [0, 1] {
        let rig = service("system");
        let (seam, target) = seam(&rig);
        let mut swapped = ExecScript::new(update("2.0").argv(), exit, "the wrong instance's words");
        swapped.swap_uuid = true;
        rig.fake.script_exec(swapped);
        assert!(matches!(
            run(&seam, &target, &update("2.0")),
            Err(ExecError::Refused("instance_changed"))
        ));
        assert_eq!(rig.fake.recordings(), 0);
    }
}

#[test]
fn the_refusals_that_stay_send_no_exec_request() {
    for damage in [
        "adopted",
        "tenant",
        "tenant_elsewhere",
        "vm",
        "project",
        "target",
        "stopped",
        "no_uuid",
        "uuid",
        "tombstone",
    ] {
        let rig = service("system");
        let (seam, mut target) = seam(&rig);
        let expected = match damage {
            "adopted" => {
                target.service.adopted = true;
                "adopted"
            }
            "tenant" => {
                target.service.tenant = Some(
                    serde_json::from_value(json!({"id":"t1","block":"192.0.2.64/29"})).unwrap(),
                );
                target.service.project =
                    format!("{}t1", arca_kernel::ownership::TENANT_PROJECT_PREFIX);
                "tenant"
            }
            "tenant_elsewhere" => {
                target.service.tenant = Some(
                    serde_json::from_value(json!({"id":"t1","block":"192.0.2.64/29"})).unwrap(),
                );
                "tenant"
            }
            "vm" => {
                target.service.vm =
                    Some(serde_json::from_value(json!({"rootSize":"20GiB"})).unwrap());
                "vm"
            }
            "project" => {
                target.service.project = "other".into();
                "target"
            }
            "target" => {
                target.service.instance = "another".into();
                "target"
            }
            "stopped" => {
                rig.fake.set_instance_status(PROJECT, "app", "Stopped");
                "stopped"
            }
            "no_uuid" => {
                target.uuid = None;
                "uuid"
            }
            "uuid" => {
                target.uuid = Some("different".into());
                "owner"
            }
            _ => {
                rig.edit(
                    "PATCH",
                    &format!("/1.0/instances/app?project={PROJECT}"),
                    json!({"config":{"user.arcakernel.retiring":"a".repeat(64)}}),
                );
                "tombstone"
            }
        };
        for command in [update("2.0"), health("system"), version("system")] {
            assert!(
                matches!(run(&seam, &target, &command), Err(ExecError::Refused(code)) if code == expected),
                "{damage}"
            );
        }
        assert!(execs(&rig).is_empty(), "{damage}");
        assert!(!seam.hung(&target), "{damage}: a refusal marks nothing");
    }
}

/// A stored secret cut by the 64 KiB boundary: the kept tail begins inside it, where a value is no longer whole and
/// cannot be matched. The first partial line of a cut output is dropped, so no byte of the secret's end is answered.
#[test]
fn a_secret_cut_by_the_kept_tail_shows_no_byte_of_its_end() {
    let rig = service("system");
    let (seam, target) = seam(&rig);
    for cut_at in [1, STORED.len() / 2, STORED.len() - 1] {
        let end = &STORED[cut_at..];
        // What follows the secret: a line break, then lines, so that the last 64 KiB starts at `cut_at`.
        let need = 64 * 1024 - end.len();
        let mut rest = String::from("\n");
        while rest.len() + 2001 < need - 10 {
            rest.push_str(&"y".repeat(2000));
            rest.push('\n');
        }
        rest.push_str(&"w".repeat(need - rest.len() - 1));
        rest.push('\n');
        assert_eq!(rest.len(), need);
        let stdout = format!("{}\nlead {STORED}{rest}", "z".repeat(5000));
        script(&rig, &update("2.0"), 1, stdout.as_bytes(), b"");
        let answer = run(&seam, &target, &update("2.0")).expect("an answer");
        assert_eq!(answer.output_cut, OutputCut::FirstLineDropped, "{cut_at}");
        for line in &answer.lines {
            assert!(
                !line.contains(end),
                "{cut_at}: the secret's end is shown in {line:?}"
            );
        }
        assert!(answer.lines.first().unwrap().starts_with('y'), "{cut_at}");
    }
    // A cut output with no line break in what is kept keeps nothing, and says so; the other output is kept whole.
    let stdout = format!("{STORED}{}", "q".repeat(70 * 1024));
    script(
        &rig,
        &update("2.0"),
        1,
        stdout.as_bytes(),
        b"the other output\n",
    );
    let answer = run(&seam, &target, &update("2.0")).expect("an answer");
    assert_eq!(answer.output_cut, OutputCut::NothingKept);
    assert_eq!(answer.lines, ["the other output"]);
}

/// An argument of a formula that row 1 takes, at its 512 bytes with `{version}` in it, still makes a command once a
/// version of 64 bytes is filled in: 512 - 9 + 64 = 567 bytes.
#[test]
fn the_longest_argument_a_formula_takes_still_runs_with_the_longest_version() {
    let element = format!("--to={}{{version}}", "a".repeat(512 - 5 - 9));
    assert_eq!(element.len(), 512);
    let formula: AppUpdate = serde_json::from_value(json!({
        "update": { "command": ["/usr/local/sbin/app-update", element], "seconds": 3600 },
        "health": { "command": ["/usr/bin/true"] },
        "version": { "command": ["/usr/bin/true"] }
    }))
    .unwrap();
    formula.check("system").expect("row 1 takes it");
    let version = AppVersion::parse(&"9".repeat(64)).unwrap();
    let command = DeclaredCommand::update(&formula, "system", &version).expect("a command");
    assert_eq!(command.argv().last().unwrap().len(), 567);
}

/// An output whose kept 64 KiB tail begins exactly at `tail`: `head` comes before the cut, and lines of words that are
/// not a key's follow `tail`, few enough that every line of `tail` is among an update's last 60.
fn cut_before(head: &str, tail: &str) -> Vec<u8> {
    let mut kept = String::from(tail);
    while kept.len() + 2001 < 64 * 1024 - 10 {
        kept.push_str(&"done. ".repeat(333));
        kept.push('\n');
    }
    kept.push_str(&"w.".repeat((64 * 1024 - kept.len() - 1) / 2));
    while kept.len() < 64 * 1024 - 1 {
        kept.push('.');
    }
    kept.push('\n');
    assert_eq!(kept.len(), 64 * 1024);
    format!("{}\n{head}{kept}", "z".repeat(5000)).into_bytes()
}

/// How a test arms one fault on the fake Incus.
type Arm<'a> = &'a dyn Fn(&Rig);

/// Once the request to start a declared command went out, any outcome that does not show the command ended marks the
/// instance hung, as Incus not answering by the deadline does: the command may still run. One fault of each kind.
#[test]
fn a_declared_command_whose_end_is_not_shown_marks_the_instance_hung() {
    let foreign = json!({
        "id": "00000000-0000-4000-8000-0000000fffff", "class": "task", "status": "Success",
        "status_code": 200, "metadata": { "return": 0 }
    });
    let faults: [(&str, Arm); 6] = [
        ("a dropped wait connection", &|rig| {
            rig.fake.drop_reads("/1.0/operations/", 1)
        }),
        ("a cut wait answer", &|rig| {
            rig.fake.cut_reads("/1.0/operations/", 1)
        }),
        ("a 404 for the operation", &|rig| {
            rig.fake.fail_reads("/1.0/operations/", 1, 404)
        }),
        ("a malformed operation", &|rig| {
            rig.fake
                .answer_reads("/1.0/operations/", 1, json!("not an operation"))
        }),
        ("a foreign operation", &|rig| {
            rig.fake
                .answer_reads("/1.0/operations/", 1, foreign.clone())
        }),
        ("a cut start answer", &|_| {}),
    ];
    for (fault, arm) in faults {
        let rig = service("system");
        let (seam, target) = seam(&rig);
        let mut started = ExecScript::new(health("system").argv(), 0, "");
        started.cut_answer = fault == "a cut start answer";
        rig.fake.script_exec(started);
        arm(&rig);
        let error = run(&seam, &target, &health("system")).expect_err(fault);
        assert!(
            matches!(&error, ExecError::Failed(text) if text.contains("marked hung")),
            "{fault}: {error:?}"
        );
        assert!(seam.hung(&target), "{fault}");
        assert!(
            matches!(
                run(&seam, &target, &version("system")),
                Err(ExecError::Refused("hung"))
            ),
            "{fault}: the next declared command is refused"
        );
        assert_eq!(execs(&rig).len(), 1, "{fault}: nothing more was sent");
    }
}

/// An operation Incus reports failed is an answer: the command did not run, or ended. It marks nothing.
#[test]
fn a_declared_command_whose_operation_failed_does_not_mark_the_instance() {
    let rig = service("system");
    let (seam, target) = seam(&rig);
    let mut failed = ExecScript::new(health("system").argv(), 0, "");
    failed.op_failed = true;
    rig.fake.script_exec(failed);
    let error = run(&seam, &target, &health("system")).expect_err("the operation failed");
    assert!(
        matches!(&error, ExecError::Failed(text) if text == "service exec failed"),
        "{error:?}"
    );
    assert!(!seam.hung(&target));
    script(&rig, &health("system"), 0, b"", b"");
    assert_eq!(run(&seam, &target, &health("system")).unwrap().exit, 0);
    assert_eq!(execs(&rig).len(), 2);
    assert_eq!(rig.fake.recordings(), 0);
}

/// A restore or a rebuild keeps the instance's uuid, and the instance runs again after it. Arca did not see it
/// stopped, so the mark stays: only a stop Arca has seen clears it.
#[test]
fn a_restore_or_a_rebuild_alone_does_not_clear_the_mark() {
    let rig = service("system");
    let (seam, target) = seam(&rig);
    script(&rig, &health("system"), 0, b"", b"");
    rig.fake.drop_reads("/1.0/operations/", 1);
    run(&seam, &target, &health("system")).expect_err("no answer");
    assert!(seam.hung(&target));
    let uuid = rig
        .get(&format!("/1.0/instances/app?project={PROJECT}"))
        .unwrap()["config"]["volatile.uuid"]
        .clone();
    // What a restore or a rebuild shows from outside: stopped, then running again, with the same uuid.
    rig.fake.set_instance_status(PROJECT, "app", "Stopped");
    rig.fake.set_instance_status(PROJECT, "app", "Running");
    assert_eq!(
        rig.get(&format!("/1.0/instances/app?project={PROJECT}"))
            .unwrap()["config"]["volatile.uuid"],
        uuid
    );
    assert!(!seam.clear_hung_after_stop(&target).unwrap());
    assert!(seam.hung(&target));
    assert!(matches!(
        run(&seam, &target, &health("system")),
        Err(ExecError::Refused("hung"))
    ));
    assert_eq!(execs(&rig).len(), 1);
}

/// Whatever a cut tail holds, its answer is the one the log masking alone gives once the first partial line is dropped
/// (as at c739db95), with whole lines of it hidden more: line for line, each answer line is that answer's line or
/// `[secret]` where that line is a key's, so an update's last 60 and a failed health check's last 5 are the same lines
/// whatever the other output holds, and no seven bytes of a hidden value or of a key's body are shown. The rows: a key
/// cut in five places, a key on one long line, CRLF, a value with a line break written out, a value after a key-like
/// line, overlapping values on one line, and a cut error output that begins with key-like lines.
#[test]
fn a_cut_tail_shows_no_line_the_log_masking_hides_and_no_part_of_a_hidden_value() {
    let rig = service_storing("system", &[STORED, ESCAPED, SHORT, MARKED]);
    let (seam, target) = seam(&rig);
    let secrets = [STORED, ESCAPED, SHORT, MARKED, ENV_VALUE].map(String::from);
    let word = ["PRI", "VATE"].concat();
    let begin = format!("-----BEGIN RSA {word} KEY-----");
    let end = format!("-----END RSA {word} KEY-----");
    let body: Vec<String> = (0..4)
        .map(|n| format!("{n}QkVHSU5ib2R5bGluZXRoYXRsb29rc2xpa2VhcmVhbGtleWJ1dGlzbm90MDEy{n}x"))
        .collect();
    let (lines, crlf) = (body.join("\n"), body.join("\r\n"));
    let long = "S2V5T25PbmVMaW5l".repeat(12);
    let hex = format!("{}\n", "0123456789abcdef".repeat(4));
    let cut_err = |count: usize, last: &str| {
        format!("{}\n{}{last}\n", "x".repeat(70_000), hex.repeat(count)).into_bytes()
    };
    let numbered = |count: usize| -> Vec<u8> {
        (1..=count)
            .flat_map(|n| format!("line {n}\n").into_bytes())
            .collect()
    };
    let cut = |shape, head: &str, tail: String| {
        (
            shape,
            update("2.0"),
            60,
            cut_before(head, &tail),
            Vec::new(),
        )
    };
    let open = format!("{begin}\n0QkVHSU5i");
    let open_crlf = format!("{begin}\r\n0QkVHSU5i");
    let rows = [
        cut("standalone BEGIN", "", format!("{begin}\n{lines}\n")),
        cut("inline BEGIN", "Gene", format!("rated: {begin}\n{lines}\n")),
        cut(
            "inside BEGIN",
            "-----BEG",
            format!("IN RSA {word} KEY-----\n{lines}\n"),
        ),
        cut("body, no END", &open, format!("{}\n", &lines[9..])),
        cut(
            "body, END later",
            &open,
            format!("{}\n{end}\n", &lines[9..]),
        ),
        cut("one long line", "", format!("{begin}\n{long}\n")),
        cut(
            "CRLF",
            &open_crlf,
            format!("{}\r\nusing {STORED} now\r\n", &crlf[9..]),
        ),
        cut("written-out break", "par", format!("tial\n{ESCAPED}\n")),
        cut(
            "overlapping values",
            "par",
            format!("tial\nKLMNOPQRSTUVWXYZ\n{SHORT}\n"),
        ),
        cut(
            "one line",
            "par",
            format!("tial\nlead {MARKED}{SHORT} and {SHORT}xyz\n"),
        ),
        (
            "health",
            health("system"),
            5,
            numbered(6),
            cut_err(4, "health: refused"),
        ),
        (
            "update",
            update("2.0"),
            60,
            numbered(100),
            cut_err(30, "one more line"),
        ),
    ];
    // The answer at c739db95: each output's last 64 KiB, a cut one with its first partial line dropped, masked as a
    // log on its own, the error output's lines after the others, then the part's last lines.
    let before = |stdout: &[u8], stderr: &[u8], keep: usize| {
        let mut lines = Vec::new();
        for output in [stdout, stderr] {
            let kept = &output[output.len().saturating_sub(64 * 1024)..];
            let whole = match kept.iter().position(|b| matches!(b, b'\n' | b'\r')) {
                _ if kept.len() == output.len() => kept,
                Some(at) => &kept[at + 1..],
                None => &[],
            };
            lines.extend(masked_lines(whole, &secrets));
        }
        lines.drain(..lines.len().saturating_sub(keep));
        lines
    };
    let mut faults = Vec::new();
    for (shape, command, keep, stdout, stderr) in rows {
        script(&rig, &command, 1, &stdout, &stderr);
        let answer = run(&seam, &target, &command).expect("an answer");
        assert_eq!(answer.output_cut, OutputCut::FirstLineDropped, "{shape}");
        let before = before(&stdout, &stderr, keep);
        let key = |old: &str| {
            old.len() >= 16
                && old
                    .bytes()
                    .all(|b| b"+/=".contains(&b) || b.is_ascii_alphanumeric())
        };
        let same = answer.lines.len() == before.len()
            && (answer.lines.iter().zip(&before))
                .all(|(line, old)| line == old || line == "[secret]" && key(old));
        if !same {
            faults.push(format!("{shape}: not c739db95's answer line for line"));
        }
        let shown = answer.lines.join("\n");
        let piece = (secrets.iter().chain(&body).chain([&long]))
            .flat_map(|value| value.as_bytes().windows(7))
            .map(|piece| std::str::from_utf8(piece).unwrap())
            .find(|piece| !"[secret]".contains(piece) && shown.contains(piece));
        if let Some(piece) = piece {
            faults.push(format!("{shape}: {piece:?} of a hidden value is shown"));
        }
    }
    assert!(faults.is_empty(), "{faults:#?}");
    assert_eq!(rig.fake.recordings(), 0);
}
