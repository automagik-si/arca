//! Connector cards against fake Incus. Names, links and credentials are synthetic.
use super::*;
use arca_incus::fake::ExecScript;

const LOGIN: &str = "https://login.tailscale.com/a/synthetic123";
const KEY: &str = "tskey-auth-synthetic-reach-example";

fn rig() -> (Rig, common::SkKey) {
    let key = common::SkKey::from_seed(71);
    let rig = Rig::with_key(
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![],
            networks: vec![],
            pools: vec![("pool1".into(), json!({}))],
        },
        vec![PROJECT.into()],
        Some("192.0.2.0/24"),
        Some(&key),
    );
    prepare_edge(&rig);
    (rig, key)
}

fn prepare_edge(rig: &Rig) {
    arca::cmd_templates::install_into(&rig.dir.join("templates"), false).unwrap();
    rig.template("edge", json!({"name":"edge","kind":"oci","image":"docker.io/library/example","digest":DIGEST,"ports":[443],"env":{},"lan":{"port":443},"edge":true,"provides":{"env":{"ARCA_EDGE_TARGET":"{{host}}:{{port}}"}}}));
    rig.call("service_create", json!({"name":"edge","template":"edge"}))
        .unwrap();
    rig.fake.script_exec(ExecScript::new(
        arca_incus::exec::ExecCommand::EdgeReload.argv(),
        0,
        "",
    ));
}

fn proposed(rig: &Rig, args: Value) -> Value {
    try_proposed(rig, args).unwrap()
}

fn try_proposed(rig: &Rig, args: Value) -> Result<Value, CallError> {
    proposed_action(rig, "reach_connect", args)
}

fn proposed_action(rig: &Rig, action: &str, args: Value) -> Result<Value, CallError> {
    let plans: HashMap<_, _> = arca::handlers::plan_handlers_served(
        rig.kernel.clone(),
        Arc::new(|| NOW),
        None,
        None,
        None,
        None,
        arca::handlers::ActionSlot::default(),
        Some(rig.cfg.clone()),
    )
    .into_iter()
    .collect();
    plans["action_propose"].call(json!({"action":action,"args":args}), &doctor())
}

fn approved_args(rig: &Rig, key: &common::SkKey, proposal: &Value) -> (Value, String) {
    static NONCE: AtomicU64 = AtomicU64::new(0x7400);
    let statement = arca::handlers::action_statement(
        "node1",
        proposal["id"].as_str().unwrap(),
        proposal["action"].as_str().unwrap(),
        proposal["argsDigest"].as_str().unwrap(),
        NOW,
        &format!("{:032x}", NONCE.fetch_add(1, Ordering::SeqCst)),
    )
    .unwrap();
    let signed = common::approve_statement(key, &statement);
    let approved = arca::handlers::approve_action(
        &mut rig.kernel.lock().unwrap(),
        &json!({"plan":proposal["id"],"approval":signed}),
        NOW,
    )
    .unwrap();
    (approved.args, approved.grant)
}

fn connect(rig: &Rig, key: &common::SkKey, name: Option<&str>) -> Result<Value, CallError> {
    let mut args = json!({"kind":"tailscale"});
    if let Some(name) = name {
        args["name"] = json!(name);
    }
    let proposal = proposed(rig, args);
    let (mut args, grant) = approved_args(rig, key, &proposal);
    args["approval"] = json!(grant);
    rig.call("reach_connect", args)
}

fn status(rig: &Rig, state: &str) {
    let argv = [
        "/usr/bin/timeout",
        "-k",
        "2",
        "30",
        "/usr/local/bin/tailscale",
        "--socket=/tmp/tailscaled.sock",
        "status",
        "--json",
        "--peers=false",
    ];
    rig.fake.script_exec(ExecScript::new(argv.into_iter().map(str::to_string).collect(), 0,
        &json!({"BackendState":state,"AuthURL":LOGIN,"TailscaleIPs":["100.64.0.7"],"Self":{"DNSName":"node.example.ts.net.","HostName":"node"}}).to_string()));
}

fn edge_reload(rig: &Rig) {
    rig.fake.script_exec(ExecScript::new(
        arca_incus::exec::ExecCommand::EdgeReload.argv(),
        0,
        "",
    ));
}

fn assert_private(rig: &Rig, needle: &str) {
    fn walk(path: &std::path::Path, needle: &str) {
        for entry in fs::read_dir(path).unwrap().flatten() {
            if entry.file_name() == "secrets" {
                continue;
            }
            if entry.path().is_dir() {
                walk(&entry.path(), needle);
            } else if entry.path().is_file() {
                let bytes = fs::read(entry.path()).unwrap();
                assert!(
                    !String::from_utf8_lossy(&bytes).contains(needle),
                    "{} leaked a private value",
                    entry.path().display()
                );
            }
        }
    }
    walk(&rig.dir, needle);
}

#[test]
fn approved_connect_links_starts_and_lists_without_the_sign_in_link() {
    let (rig, key) = rig();
    status(&rig, "NeedsLogin");
    let made = connect(&rig, &key, None).unwrap();
    assert_eq!(made["name"], "node1-ts");
    assert_eq!(made["state"], "waiting-sign-in");
    assert!(!made.to_string().contains(LOGIN));
    let cfg = rig.service_config("node1-ts");
    assert_eq!(cfg.links[0].to, "edge");
    assert!(cfg.start && cfg.autostart);
    let requests = rig.fake.requests();
    let first_start = requests
        .iter()
        .position(|request| {
            request.method == "PUT" && request.path.starts_with("/1.0/instances/node1-ts/state?")
        })
        .expect("one explicit first start after linking");
    let connector_config = requests
        .iter()
        .rposition(|request| {
            request.method == "PATCH" && request.path.starts_with("/1.0/instances/node1-ts?")
        })
        .expect("the linked environment is written");
    let edge_link = requests
        .iter()
        .rposition(|request| {
            request.method == "PATCH"
                && request
                    .path
                    .starts_with("/1.0/network-acls/arcak-svc-svc-edge?")
        })
        .expect("the edge allows the connector");
    assert!(
        connector_config < first_start && edge_link < first_start,
        "the first start must already have the linked target and edge ACL"
    );
    assert_eq!(rig.fake.state_actions(PROJECT, "node1-ts"), ["start"]);

    assert_eq!(
        rig.get(&format!("/1.0/instances/node1-ts?project={PROJECT}"))
            .unwrap()["config"]["environment.ARCA_EDGE_TARGET"],
        format!("{}:443", rig.service_config("edge").ipv4)
    );
    assert_eq!(
        rig.get(&format!("/1.0/instances/node1-ts?project={PROJECT}"))
            .unwrap()["status"],
        "Running"
    );
    let files = rig.fake.service_files();
    let edge = String::from_utf8_lossy(
        &files[&format!("{PROJECT}/edge:{}", arca_incus::exec::EDGE_CONFIG)],
    );
    assert!(edge.contains(&format!("allow {}/32", cfg.ipv4)));
    status(&rig, "NeedsLogin");
    let waiting = rig.call("reach_list", json!({})).unwrap();
    assert_eq!(waiting["connectors"][0]["state"], "waiting-sign-in");
    status(&rig, "Running");
    let running = rig.call("reach_list", json!({})).unwrap();
    assert_eq!(running["connectors"][0]["state"], "running");
    assert_eq!(running["connectors"][0]["tailnetAddress"], "100.64.0.7");
    assert_eq!(
        running["connectors"][0]["tailnetName"],
        "node.example.ts.net"
    );
    for answer in [waiting, running] {
        assert!(!answer.to_string().contains(LOGIN));
    }
    assert_private(&rig, LOGIN);
}

#[test]
fn doctor_without_approval_creates_nothing_and_arguments_take_no_key() {
    let (rig, _) = rig();
    assert!(
        matches!(rig.call("reach_connect", json!({"kind":"tailscale"})), Err(CallError::Refused { code, .. }) if code == "NotApproved")
    );
    assert_eq!(
        rig.entries()
            .iter()
            .filter(|e| e.plugin.0 == "arca.service")
            .count(),
        1
    );
    assert!(matches!(
        rig.call(
            "reach_connect",
            json!({"kind":"tailscale","TS_AUTHKEY":KEY})
        ),
        Err(CallError::Invalid(_))
    ));
}

#[test]
fn a_later_key_is_used_on_restart_but_is_never_returned_or_journaled() {
    let (rig, key) = rig();
    connect(&rig, &key, Some("node-ts")).unwrap();
    let answer = rig
        .call(
            "service_secret_set",
            json!({"name":"node-ts","key":"TS_AUTHKEY","value":KEY}),
        )
        .unwrap();
    assert!(!answer.to_string().contains(KEY));
    // The lifecycle path uses the updated environment, as the shipped-script tests verify by file.
    let kernel = rig.kernel.clone();
    let life = arca::lifecycle::InstanceLifecycle::new(
        rig.fake.socket().into(),
        arca_incus::ProjectFamily::new([PROJECT], []).unwrap(),
        move || {
            kernel
                .lock()
                .unwrap()
                .retained_entries()
                .map_err(|e| e.to_string())
        },
        &rig.dir,
    );
    life.run(
        arca_incus::StateAction::Restart,
        PROJECT,
        "node-ts",
        "doctor",
        None,
        NOW,
    )
    .unwrap();
    assert_eq!(
        rig.get(&format!("/1.0/instances/node-ts?project={PROJECT}"))
            .unwrap()["config"]["environment.TS_AUTHKEY"],
        KEY
    );
    rig.fake.set_console_log(
        PROJECT,
        "node-ts",
        &format!("key={KEY}\n{LOGIN}\nordinary line\n"),
    );
    let logs = rig.call("service_logs", json!({"name":"node-ts"})).unwrap();
    assert!(!logs.to_string().contains(KEY));
    assert!(!logs.to_string().contains(LOGIN));
    assert_private(&rig, KEY);
    assert_private(&rig, LOGIN);
}

#[test]
fn logs_hide_only_connector_login_links_and_keep_other_lines() {
    let (rig, key) = rig();
    connect(&rig, &key, Some("node-ts")).unwrap();
    let log = format!("before\nvisit {LOGIN}\nhttps://login.tailscale.com/admin/machines\nafter\n");
    rig.fake.set_console_log(PROJECT, "node-ts", &log);
    let logs = rig.call("service_logs", json!({"name":"node-ts"})).unwrap();
    assert_eq!(
        logs["lines"],
        json!([
            "before",
            "visit [sign-in link]",
            "https://login.tailscale.com/admin/machines",
            "after"
        ])
    );
    rig.fake.set_console_log(PROJECT, "edge", &log);
    assert_eq!(
        rig.call("service_logs", json!({"name":"edge"})).unwrap()["lines"],
        json!([
            "before",
            format!("visit {LOGIN}"),
            "https://login.tailscale.com/admin/machines",
            "after"
        ])
    );
}

fn assert_split_login_hidden(log: &str) {
    let (rig, key) = rig();
    connect(&rig, &key, Some("node-ts")).unwrap();
    rig.fake.set_console_log(PROJECT, "node-ts", log);
    let logs = rig.call("service_logs", json!({"name":"node-ts"})).unwrap();
    assert_eq!(logs["lines"], json!(["visit [sign-in link]"]));
    rig.fake.set_console_log(PROJECT, "edge", log);
    let unchanged = rig.call("service_logs", json!({"name":"edge"})).unwrap();
    assert_eq!(
        unchanged["lines"],
        if log.contains('\r') {
            json!(["visit https://login.tailscale.com/a/synthetic", "123"])
        } else if log.starts_with("visit http:") {
            json!(["visit http://LOGIN.TAILSCALE.COM/a/synthetic123"])
        } else {
            json!([format!("visit {LOGIN}")])
        }
    );
    assert_private(&rig, LOGIN);
}

#[test]
fn connector_logs_preserve_other_carriage_return_lines() {
    let (rig, key) = rig();
    connect(&rig, &key, Some("node-ts")).unwrap();
    rig.fake
        .set_console_log(PROJECT, "node-ts", &format!("before\rafter\n{LOGIN}\n"));
    assert_eq!(
        rig.call("service_logs", json!({"name":"node-ts"})).unwrap()["lines"],
        json!(["before", "after", "[sign-in link]"])
    );
}

#[test]
fn connector_logs_hide_http_and_uppercase_login_links() {
    assert_split_login_hidden("visit http://LOGIN.TAILSCALE.COM/a/synthetic123\n");
}

#[test]
fn logs_hide_login_split_by_escape_sequences() {
    assert_split_login_hidden(
        "visit https://login.tailscale.com/a/\u{1b}[1msynthetic123\u{1b}[0m\n",
    );
}

#[test]
fn logs_hide_login_split_by_nul() {
    assert_split_login_hidden("visit https://login.tailscale\0.com/a/synthetic123\n");
}

#[test]
fn logs_hide_login_split_by_escape_sequences_and_nul() {
    assert_split_login_hidden(
        "visit https://login.tailscale\0.com/a/\u{1b}[1msynthetic123\u{1b}[0m\n",
    );
}

#[test]
fn logs_hide_login_split_by_carriage_return() {
    assert_split_login_hidden("visit https://login.tailscale.com/a/synthetic\r123\n");
}

fn privacy_log_lines(rig: &Rig, name: &str, raw: &str) -> Vec<String> {
    rig.fake.set_console_log(PROJECT, name, raw);
    rig.call("service_logs", json!({"name":name})).unwrap()["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|line| line.as_str().unwrap().to_string())
        .collect()
}

fn privacy_non_connector_lines(raw: &str) -> Vec<String> {
    let hidden = arca::service::values_to_hide(&[]);
    arca::service::redact_blocks(raw)
        .split(['\n', '\r'])
        .map(arca::service::clean_log)
        .filter(|line| !line.is_empty())
        .map(|line| arca::service::redact_hidden(&line, &hidden))
        .collect()
}

fn privacy_cases(cases: &[(&str, String)], id: &str, placeholder: bool) {
    let (rig, key) = rig();
    connect(&rig, &key, Some("node-ts")).unwrap();
    let mut failures = Vec::new();
    for (label, raw) in cases {
        let lines = privacy_log_lines(&rig, "node-ts", raw);
        if cases.len() < 5 {
            println!("{label}: {lines:?}");
        }
        if lines.concat().contains(id)
            || (placeholder && !lines.concat().contains("[sign-in link]"))
        {
            failures.push(format!("{label}: {lines:?}"));
        }
        assert_eq!(
            privacy_log_lines(&rig, "edge", raw),
            privacy_non_connector_lines(raw),
            "{label}"
        );
    }
    assert_private(&rig, id);
    assert!(
        failures.is_empty(),
        "{} of {} rows failed: {}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}

#[test]
fn logs_privacy_terminal_wrappers() {
    let id = "syntheticTerminal1234";
    let url = format!("https://login.tailscale.com/a/{id}");
    privacy_cases(
        &[
            (
                "osc",
                format!("visit \u{1b}]8;;{url}\u{1b}\\{url}\u{1b}]8;;\u{1b}\\\n"),
            ),
            ("bare", format!("visit \u{1b}{url}\n")),
            ("csi", format!("visit \u{1b}[1;32{url}\u{1b}[0m\n")),
        ],
        id,
        true,
    );
}

#[test]
fn logs_privacy_host_and_scheme_views() {
    let id = "syntheticHost1234";
    privacy_cases(
        &[
            (
                "host",
                format!("visit https://\u{1b}login.tailscale.com/a/{id}\n"),
            ),
            ("scheme-less", format!("visit login.tailscale.com/a/{id}\n")),
        ],
        id,
        false,
    );
}

#[test]
fn logs_privacy_whitespace_after_path() {
    let id = "syntheticSpace1234";
    privacy_cases(
        &["\n", " ", "\t", "\r"].map(|space| {
            (
                space,
                format!("visit https://login.tailscale.com/a/{space}{id}\n"),
            )
        }),
        id,
        false,
    );
}

fn longest_visible_piece(id: &str, text: &str) -> usize {
    assert!(id.is_ascii());
    (1..=id.len())
        .rev()
        .find(|&len| (0..=id.len() - len).any(|start| text.contains(&id[start..start + len])))
        .unwrap_or(0)
}

#[test]
fn logs_privacy_identifier_fragments() {
    let (rig, key) = rig();
    connect(&rig, &key, Some("node-ts")).unwrap();
    for raw in [
        "visit https://login.tailscale.com/a/synth\u{1b}[1mQ03aa\u{1b}[0m\n".to_string(),
        "visit https://login.tailscale.com/a/synth\u{7}Q11aa\n".into(),
        "visit https://login.tailscale.com/a/syn\u{b}th\u{c}Q12\u{8}aa\n".into(),
        format!(
            "visit https://login.tailscale.com/a/syn{}thQ13aa\n",
            (1u8..=31)
                .filter(|b| ![9u8, 10, 13, 27].contains(b))
                .map(char::from)
                .collect::<String>()
        ),
        "visit https://login.tailscale.com/a/synth\u{7f}Q14aa\n".into(),
        "visit https://login.tailscale.com/a/synth\u{85}Q15\u{9b}aa\n".into(),
        "visit https://login.tailscale.com/ayntheticSweep1234\n".into(),
    ] {
        assert_eq!(
            privacy_log_lines(&rig, "node-ts", &raw),
            ["visit [sign-in link]"],
            "{raw:?}"
        );
        assert_eq!(
            privacy_log_lines(&rig, "edge", &raw),
            privacy_non_connector_lines(&raw)
        );
    }
}

#[test]
fn logs_privacy_every_link_offset() {
    let id = "syntheticSweep1234";
    let url = format!("https://login.tailscale.com/a/{id}");
    let inserts = [
        "\u{1b}",
        "\u{1b}[",
        "\u{1b}]8;;",
        "\u{1b}[1;32",
        "\0",
        "\r",
        "\u{7}",
    ];
    let mut cases = Vec::new();
    for offset in 0..=url.len() {
        for insert in inserts {
            cases.push((
                format!("offset={offset} insert={insert:?}"),
                format!("visit {}{insert}{}\n", &url[..offset], &url[offset..]),
            ));
        }
    }
    let rest = format!(
        "visit https://login.tailscale.com/a/[sign-in link][secret]{}",
        inserts.join("")
    );
    assert!(
        longest_visible_piece(id, &rest) < 4,
        "identifier must not overlap ordinary text"
    );
    assert_eq!(cases.len(), 343);
    let (rig, key) = rig();
    connect(&rig, &key, Some("node-ts")).unwrap();
    let mut histogram = std::collections::BTreeMap::new();
    let mut failures = Vec::new();
    for (label, raw) in &cases {
        let lines = privacy_log_lines(&rig, "node-ts", raw);
        let joined = lines.concat();
        let longest = longest_visible_piece(id, &joined);
        *histogram.entry(longest).or_insert(0) += 1;
        if joined.contains(id) || longest >= 4 {
            failures.push(format!("{label}: longest={longest} {lines:?}"));
        }
        assert_eq!(
            privacy_log_lines(&rig, "edge", raw),
            privacy_non_connector_lines(raw),
            "{label}"
        );
    }
    println!(
        "privacy sweep rows={} longest-piece histogram={histogram:?}",
        cases.len()
    );
    assert_private(&rig, id);
    assert!(
        failures.is_empty(),
        "{} rows failed: {}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn logs_privacy_two_inserts() {
    let id = "syntheticDouble1234";
    let (rig, key) = rig();
    connect(&rig, &key, Some("node-ts")).unwrap();
    let raw = "visit https://\u{1b}login.tailscale.com/a/synthetic\0Double1234\n";
    assert_eq!(
        privacy_log_lines(&rig, "node-ts", raw),
        ["visit https://[sign-in link]"]
    );
    assert_eq!(
        privacy_log_lines(&rig, "edge", raw),
        privacy_non_connector_lines(raw)
    );
    assert_private(&rig, id);
    privacy_cases(
        &[(
            "mixed host sequences",
            "visit https://lo\u{1b}[1mgin.tails\u{1b}[cale.com/a/syntheticDouble1234\n".into(),
        )],
        id,
        false,
    );
}

#[test]
fn logs_privacy_ordinary_output_and_short_identifiers() {
    let (rig, key) = rig();
    connect(&rig, &key, Some("node-ts")).unwrap();
    let raw = "To authenticate, visit:\n\thttps://login.tailscale.com/a/syntheticPlain1234\nlogin.tailscale.com/admin/machines\ngo on and stay up\nhttps://login.tailscale.com/a/go\ngo on\n[sign-in link] followingword\n";
    assert_eq!(
        privacy_log_lines(&rig, "node-ts", raw),
        [
            "To authenticate, visit:",
            "[sign-in link]",
            "login.tailscale.com/admin/machines",
            "go on and stay up",
            "[sign-in link]",
            "go on",
            "[sign-in link] followingword"
        ]
    );
    assert_eq!(
        privacy_log_lines(&rig, "edge", raw),
        privacy_non_connector_lines(raw)
    );
}

#[test]
fn unreadable_running_connector_status_is_unknown() {
    let (rig, key) = rig();
    assert_eq!(
        connect(&rig, &key, Some("node-ts")).unwrap()["state"],
        "unknown"
    );
    let command = arca_incus::exec::ExecCommand::ConnectorStatus;
    for (label, exit, output) in [
        ("malformed", 0, format!("{{{LOGIN}")),
        ("too large", 0, "x".repeat(65537)),
        ("failed command", 1, LOGIN.to_string()),
        (
            "missing backend state",
            0,
            json!({"AuthURL":LOGIN}).to_string(),
        ),
    ] {
        rig.fake
            .script_exec(ExecScript::new(command.argv(), exit, &output));
        let listed = rig.call("reach_list", json!({})).unwrap();
        assert_eq!(listed["connectors"][0]["state"], "unknown", "{label}");
        assert!(listed["connectors"][0].get("tailnetName").is_none());
        assert!(listed["connectors"][0].get("tailnetAddress").is_none());
        assert!(!listed.to_string().contains(LOGIN));
    }
    assert_private(&rig, LOGIN);
}

#[test]
fn connect_failure_names_the_created_service() {
    let (rig, key) = rig();
    // Creation is allowed; writing the edge's incoming link is refused.
    rig.fake
        .set_reject_patch("/1.0/network-acls/arcak-svc-svc-edge");
    let result = connect(&rig, &key, Some("node-ts"));
    let message = match result {
        Err(CallError::Refused { message, .. }) => message,
        other => panic!("expected partial failure: {other:?}"),
    };
    assert!(
        message.contains("node-ts") && message.contains("created"),
        "{message}"
    );
    assert!(
        rig.get(&format!("/1.0/instances/node-ts?project={PROJECT}"))
            .is_some()
    );
}

#[test]
fn both_verbs_run_through_the_mcp_door() {
    let (rig, key) = rig();
    let door = external::Door::over(&rig);
    let proposal = proposed(&rig, json!({"kind":"tailscale","name":"node-ts"}));
    let (mut args, grant) = approved_args(&rig, &key, &proposal);
    args["approval"] = json!(grant);
    assert_eq!(door.call("reach_connect", args), "ok");
    assert_eq!(door.call("reach_list", json!({})), "ok");
}

#[test]
fn stopped_connectors_only_gave_up_on_the_authentication_timeout_line() {
    let (rig, key) = rig();
    connect(&rig, &key, Some("node-ts")).unwrap();
    let path = format!("/1.0/instances/node-ts/state?project={PROJECT}");
    rig.edit("PUT", &path, json!({"action":"stop","force":true}));
    let state = || rig.call("reach_list", json!({})).unwrap()["connectors"][0]["state"].clone();
    assert_eq!(state(), "stopped");
    rig.fake
        .set_console_log(PROJECT, "node-ts", "unrelated context deadline exceeded\n");
    assert_eq!(state(), "stopped");
    rig.fake.set_console_log(PROJECT, "node-ts", "boot: 2026/01/01 12:00:00 failed to auth tailscale: tailscale up failed: context deadline exceeded\n");
    assert_eq!(state(), "gave-up");
    rig.fake.set_console_log(PROJECT, "node-ts", "boot: 2026/01/01 12:00:00 failed to auth tailscale: tailscale up failed: context deadline exceeded\nboot: 2026/01/01 12:01:00 Starting tailscaled\nboot: 2026/01/01 12:01:01 Startup complete, waiting for shutdown signal\n");
    assert_eq!(
        state(),
        "stopped",
        "an old timeout cannot describe a later stop"
    );
}

#[test]
fn connector_status_is_the_only_new_exec_power_and_only_connectors_receive_it() {
    use arca::service_exec::{ExecTarget, ServiceExec};
    use arca_incus::exec::ExecCommand;
    let (rig, key) = rig();
    connect(&rig, &key, Some("node-ts")).unwrap();
    let seam = ServiceExec::new(rig.cfg.clone());
    let target = |name: &str| {
        let cfg = rig.service_config(name);
        let kernel = rig.kernel.lock().unwrap();
        ExecTarget {
            entry: EntryId(cfg.entry_id()),
            uuid: kernel.service_uuid(&EntryId(cfg.entry_id()), name),
            retained: kernel.retained_entries().unwrap(),
            service: cfg,
        }
    };
    status(&rig, "NeedsLogin");
    assert_eq!(
        rig.call("reach_list", json!({})).unwrap()["connectors"][0]["state"],
        "waiting-sign-in"
    );
    assert!(
        rig.fake
            .service_requests()
            .iter()
            .any(|request| request.path.contains("/instances/node-ts/exec"))
    );
    assert!(
        seam.run(
            &target("node-ts"),
            "doctor",
            "service_logs",
            &ExecCommand::TailnetStatus,
            &[],
            None
        )
        .is_err()
    );
    rig.template("plain", json!({"name":"plain","kind":"oci","image":"docker.io/library/example","digest":DIGEST,"env":{}}));
    rig.call("service_create", json!({"name":"plain","template":"plain"}))
        .unwrap();
    assert!(
        seam.run(
            &target("plain"),
            "doctor",
            "service_logs",
            &ExecCommand::TailnetStatus,
            &[],
            None
        )
        .is_err()
    );
    for name in ["edge", "plain"] {
        assert!(
            seam.run(
                &target(name),
                "doctor",
                "reach_list",
                &ExecCommand::ConnectorStatus,
                &[],
                None
            )
            .is_err()
        );
    }
    assert_private(&rig, LOGIN);
}

#[test]
fn a_node_without_an_edge_explains_it_and_creates_nothing() {
    let rig = Rig::new();
    arca::cmd_templates::install_into(&rig.dir.join("templates"), false).unwrap();
    let plans: HashMap<_, _> = arca::handlers::plan_handlers_served(
        rig.kernel.clone(),
        Arc::new(|| NOW),
        None,
        None,
        None,
        None,
        arca::handlers::ActionSlot::default(),
        Some(rig.cfg.clone()),
    )
    .into_iter()
    .collect();
    let error = plans["action_propose"].call(
        json!({"action":"reach_connect","args":{"kind":"tailscale"}}),
        &doctor(),
    );
    assert!(
        matches!(error, Err(CallError::Refused { message, .. }) if message.contains("no edge"))
    );
    assert!(rig.entries().is_empty());
}

#[test]
fn legacy_connector_records_use_the_template_and_other_kinds_have_no_tailnet_fields() {
    let (rig, _) = rig();
    let mut template = json!({"name":"legacy","kind":"oci","image":"docker.io/library/example","digest":DIGEST,"env":{}});
    rig.template("legacy", template.clone());
    rig.call(
        "service_create",
        json!({"name":"legacy","template":"legacy"}),
    )
    .unwrap();
    assert!(rig.service_config("legacy").connector.is_none());
    template["connector"] = json!("cloudflare");
    rig.template("legacy", template);
    let listed = rig.call("reach_list", json!({})).unwrap();
    assert_eq!(
        listed["connectors"],
        json!([{"name":"legacy","kind":"cloudflare","edge":null,"state":"running"}])
    );
    assert!(
        !rig.fake
            .service_requests()
            .iter()
            .any(|request| request.path.contains("/instances/legacy/exec"))
    );
}

#[test]
fn an_approval_binds_the_template_and_cannot_be_replayed() {
    let (rig, key) = rig();
    let proposal = proposed(&rig, json!({"kind":"tailscale","name":"node-ts"}));
    assert!(
        proposal["details"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["label"] == "Image")
    );
    assert!(
        proposal["details"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["label"] == "Template content")
    );
    let (mut args, grant) = approved_args(&rig, &key, &proposal);
    args["approval"] = json!(grant);
    rig.call("reach_connect", args.clone()).unwrap();
    assert!(
        matches!(rig.call("reach_connect", args), Err(CallError::Refused { code, .. }) if code == "NotApproved")
    );
    let proposal = proposed(&rig, json!({"kind":"tailscale","name":"changed-ts"}));
    let (mut args, grant) = approved_args(&rig, &key, &proposal);
    args["approval"] = json!(grant);
    let template = rig.dir.join("templates/tailscale.json");
    let mut value: Value = serde_json::from_slice(&fs::read(&template).unwrap()).unwrap();
    value["digest"] = json!(DIGEST);
    fs::write(template, value.to_string()).unwrap();
    assert!(rig.call("reach_connect", args).is_err());
    assert!(
        rig.get(&format!("/1.0/instances/changed-ts?project={PROJECT}"))
            .is_none()
    );
}

#[test]
fn connect_keeps_the_create_namespace_rule_for_recorded_programs() {
    let (rig, key) = rig();
    let add = || {
        rig.call(
            "external_add",
            json!({"name":"node1-ts", "kind":"host-program", "address":"http://127.0.0.1:1/mcp"}),
        )
        .unwrap()
    };
    add();
    assert!(
        matches!(try_proposed(&rig, json!({"kind":"tailscale"})), Err(CallError::Refused { message, .. }) if message.contains("external_remove"))
    );
    rig.call("external_remove", json!({"name":"node1-ts"}))
        .unwrap();
    let proposal = proposed(&rig, json!({"kind":"tailscale"}));
    let (mut args, grant) = approved_args(&rig, &key, &proposal);
    args["approval"] = json!(grant);
    add();
    assert!(
        matches!(rig.call("reach_connect", args), Err(CallError::Refused { message, .. }) if message.contains("external_remove"))
    );
    assert!(
        rig.get(&format!("/1.0/instances/node1-ts?project={PROJECT}"))
            .is_none()
    );
}

#[test]
fn disconnect_without_approval_preserves_the_connector() {
    let (rig, key) = rig();
    connect(&rig, &key, Some("node-ts")).unwrap();
    assert!(matches!(
        rig.call("reach_disconnect", json!({"name":"node-ts"})),
        Err(CallError::Refused { code, .. }) if code == "NotApproved"
    ));
    assert!(
        rig.get(&format!("/1.0/instances/node-ts?project={PROJECT}"))
            .is_some()
    );
}

fn disconnect_args(rig: &Rig, key: &common::SkKey, name: &str) -> Value {
    let proposal = proposed_action(rig, "reach_disconnect", json!({"name":name})).unwrap();
    let (mut args, grant) = approved_args(rig, key, &proposal);
    args["approval"] = json!(grant);
    args
}

#[test]
fn approved_disconnect_restores_the_edge_erases_state_and_allows_the_same_name_again() {
    let (rig, key) = rig();
    let file = format!("{PROJECT}/edge:{}", arca_incus::exec::EDGE_CONFIG);
    rig.template("web", json!({"name":"web","kind":"oci","image":"docker.io/library/example","digest":DIGEST,"ports":[8080],"env":{},"provides":{"env":{"WEB":"{{host}}:{{port}}"}}}));
    rig.call("service_create", json!({"name":"web","template":"web"}))
        .unwrap();
    rig.call("service_connect", json!({"from":"edge","to":"web"}))
        .unwrap();
    rig.call(
        "route_add",
        json!({"hostname":"app.example.com","instance":"web","port":8080}),
    )
    .unwrap();
    let before = rig.fake.service_files()[&file].clone();
    let routes = rig.service_config("edge").edge.unwrap().routes;
    edge_reload(&rig);
    connect(&rig, &key, Some("node-ts")).unwrap();
    let volume = format!("/1.0/storage-pools/pool1/volumes/custom/node-ts-data?project={PROJECT}");
    assert!(rig.get(&volume).is_some());
    let args = disconnect_args(&rig, &key, "node-ts");
    edge_reload(&rig);
    let answer = rig.call("reach_disconnect", args.clone()).unwrap();
    assert_eq!(answer["removed"], true);
    assert_eq!(answer["edge"]["name"], "edge");
    assert_eq!(rig.fake.service_files()[&file], before);
    let edge = rig.service_config("edge");
    assert!(edge.incoming.is_empty());
    assert_eq!(edge.edge.unwrap().routes, routes);
    assert!(
        rig.get(&format!("/1.0/instances/node-ts?project={PROJECT}"))
            .is_none()
    );
    assert!(rig.get(&volume).is_none());
    assert!(
        matches!(rig.call("reach_disconnect", args.clone()), Err(CallError::Refused {code, ..}) if code == "NotApproved")
    );
    edge_reload(&rig);
    connect(&rig, &key, Some("node-ts")).unwrap();
    assert!(
        matches!(rig.call("reach_disconnect", args), Err(CallError::Refused {code, ..}) if code == "NotApproved")
    );
    assert!(
        rig.get(&format!("/1.0/instances/node-ts?project={PROJECT}"))
            .is_some()
    );
}

#[test]
fn disconnect_rejects_changed_arguments_other_verb_grants_and_ordinary_services() {
    let (rig, key) = rig();
    connect(&rig, &key, Some("node-ts")).unwrap();
    let mut args = disconnect_args(&rig, &key, "node-ts");
    args["name"] = json!("edge");
    assert!(
        matches!(rig.call("reach_disconnect", args), Err(CallError::Refused {code, ..}) if code == "NotApproved")
    );
    let proposal = proposed(&rig, json!({"kind":"tailscale","name":"another"}));
    let (_, grant) = approved_args(&rig, &key, &proposal);
    assert!(
        matches!(rig.call("reach_disconnect", json!({"name":"node-ts","approval":grant})), Err(CallError::Refused {code, ..}) if code == "NotApproved")
    );
    rig.template("plain", json!({"name":"plain","kind":"oci","image":"docker.io/library/example","digest":DIGEST,"env":{}}));
    rig.call("service_create", json!({"name":"plain","template":"plain"}))
        .unwrap();
    for name in ["edge", "plain"] {
        let args = disconnect_args(&rig, &key, name);
        assert!(
            matches!(rig.call("reach_disconnect", args), Err(CallError::Refused {message, ..}) if message.contains("not a connector"))
        );
        assert!(
            rig.get(&format!("/1.0/instances/{name}?project={PROJECT}"))
                .is_some()
        );
    }
    assert!(
        rig.get(&format!("/1.0/instances/edge?project={PROJECT}"))
            .is_some()
    );
    assert!(
        rig.get(&format!("/1.0/instances/node-ts?project={PROJECT}"))
            .is_some()
    );
}

#[test]
fn disconnect_removes_a_connector_left_by_a_failed_link() {
    let (rig, key) = rig();
    rig.fake.fail_reads("/1.0/instances/edge?", 1, 500);
    assert!(connect(&rig, &key, Some("node-ts")).is_err());
    assert!(rig.service_config("node-ts").links.is_empty());
    let args = disconnect_args(&rig, &key, "node-ts");
    assert_eq!(
        rig.call("reach_disconnect", args).unwrap(),
        json!({"removed":true,"edge":null})
    );
    assert!(
        rig.get(&format!("/1.0/instances/node-ts?project={PROJECT}"))
            .is_none()
    );
    assert!(
        rig.get(&format!(
            "/1.0/storage-pools/pool1/volumes/custom/node-ts-data?project={PROJECT}"
        ))
        .is_none()
    );
}

fn page_request(
    socket: &std::path::Path,
    method: &str,
    path: &str,
    body: Value,
) -> (u16, String, Value) {
    use std::io::{Read, Write};
    let body = body.to_string();
    let mut stream = std::os::unix::net::UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(20)))
        .unwrap();
    write!(stream, "{method} {path} HTTP/1.1\r\nHost: localhost\r\nOrigin: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", webauthn::ORIGIN, body.len()).unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap();
    (
        head.split_whitespace().nth(1).unwrap().parse().unwrap(),
        head.to_string(),
        serde_json::from_str(body).unwrap_or(Value::Null),
    )
}

#[test]
fn page_reads_real_connector_status_only_after_passkey_and_never_records_the_link() {
    use arca::approve_page::{DesktopDisplay, Page};
    use webauthn::{Assert, SoftPasskey};
    let ssh = common::SkKey::from_seed(71);
    let key = SoftPasskey::from_seed(5);
    let owner = common::Scratch::new("reach-page-owner");
    let verifier = common::verifier_with_passkeys(
        &owner,
        &[&ssh],
        &key.line(arca_approval::ARCA_NAMESPACE),
        "reach-page",
    );
    let mut rig = Rig::with_verifier(
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![],
            networks: vec![],
            pools: vec![("pool1".into(), json!({}))],
        },
        vec![PROJECT.into()],
        Some("192.0.2.0/24"),
        Some(verifier),
        None,
    );
    prepare_edge(&rig);
    let (handlers, reads) = arca::handlers::service_handlers_with_reach(
        rig.kernel.clone(),
        Arc::new(|| NOW),
        rig.cfg.clone(),
        None,
        Default::default(),
    );
    rig.handlers = handlers.into_iter().collect();
    status(&rig, "NeedsLogin");
    connect(&rig, &ssh, Some("node-ts")).unwrap();
    let logs = Arc::new(Mutex::new(Vec::new()));
    let captured = logs.clone();
    let page = Page::new(
        webauthn::ORIGIN.into(),
        rig.kernel.clone(),
        Arc::new(|| NOW),
        rig.dir.clone(),
        Arc::new(DesktopDisplay),
    )
    .with_reach(reads)
    .with_log(Arc::new(move |line| {
        captured.lock().unwrap().push(line.to_string())
    }));
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let socket = rig.dir.join("reach.sock");
    let served = socket.clone();
    let router = page.router(false);
    runtime.spawn(async move {
        arca_mcp::serve_unix(router, &served).await.unwrap();
    });
    let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !socket.exists() {
        assert!(std::time::Instant::now() < until);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    rig.fake.script_exec(ExecScript::new(
        arca_incus::exec::ExecCommand::ConnectorStatus.argv(),
        0,
        &json!({"BackendState":"NeedsLogin","AuthURL":LOGIN,"AuthKey":KEY}).to_string(),
    ));
    let request = |method, path, body| page_request(&socket, method, path, body);
    let (code, _, rows) = request("GET", "/approve/reach", json!({}));
    assert_eq!(code, 200);
    assert_eq!(rows["connectors"][0]["state"], "waiting-sign-in");
    assert!(!rows.to_string().contains(LOGIN));
    assert!(!rows.to_string().contains(KEY));
    let (_, _, mut pending) = request("POST", "/approve/reach/sign-in", json!({"name":"node-ts"}));
    assert!(pending.get("signInUrl").is_none());
    let proof = key.proof_value(&Assert::over(
        arca_approval::ARCA_NAMESPACE,
        pending["statement"].as_str().unwrap().as_bytes(),
    ));
    pending["name"] = json!("node-ts");
    pending["proof"] = proof;
    status(&rig, "NeedsLogin");
    let (code, head, answer) = request("POST", "/approve/reach/sign-in", pending.clone());
    assert_eq!(code, 200);
    assert!(
        head.to_ascii_lowercase()
            .contains("cache-control: no-store")
    );
    assert_eq!(answer, json!({"signInUrl":LOGIN}));
    assert_eq!(request("POST", "/approve/reach/sign-in", pending).0, 403);
    status(&rig, "Running");
    let (_, _, mut pending) = request("POST", "/approve/reach/sign-in", json!({"name":"node-ts"}));
    pending["proof"] = key.proof_value(&Assert::over(
        arca_approval::ARCA_NAMESPACE,
        pending["statement"].as_str().unwrap().as_bytes(),
    ));
    pending["name"] = json!("node-ts");
    let (code, _, answer) = request("POST", "/approve/reach/sign-in", pending);
    assert_ne!(code, 200);
    assert!(!answer.to_string().contains(LOGIN));
    let audit = fs::read_to_string(rig.dir.join(arca::service_exec::AUDIT_FILE)).unwrap();
    assert!(
        audit
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .any(|row| row["token_id"] == arca_mcp::auth::APPROVED_ACTION_ID
                && row["verb"] == "reach_sign_in")
    );
    assert_private(&rig, LOGIN);
    assert_private(&rig, KEY);
    assert!(
        logs.lock()
            .unwrap()
            .iter()
            .all(|line| !line.contains(LOGIN))
    );
}

#[test]
fn approved_disconnect_runs_through_mcp_inside_its_closed_schema() {
    let (rig, key) = rig();
    status(&rig, "NeedsLogin");
    connect(&rig, &key, Some("node-ts")).unwrap();
    let args = disconnect_args(&rig, &key, "node-ts");
    let door = external::Door::over(&rig);
    edge_reload(&rig);
    assert_eq!(door.call("reach_disconnect", args), "ok");
    assert_eq!(door.call("reach_list", json!({})), "ok");
    assert_private(&rig, LOGIN);
}

#[path = "reach_reads.rs"]
mod page_reads;

#[test]
fn disconnect_restores_every_linked_edges_file_and_keeps_both_route_sets() {
    let (rig, key) = rig();
    rig.template("web", json!({"name":"web","kind":"oci","image":"docker.io/library/example","digest":DIGEST,"ports":[8080],"env":{},"provides":{"env":{"WEB":"{{host}}:{{port}}"}}}));
    rig.call("service_create", json!({"name":"web","template":"web"}))
        .unwrap();
    rig.call("service_connect", json!({"from":"edge","to":"web"}))
        .unwrap();
    rig.call(
        "route_add",
        json!({"hostname":"first.example.com","instance":"web","port":8080}),
    )
    .unwrap();
    let first_routes = rig.service_config("edge").edge.unwrap().routes;
    assert!(!first_routes.is_empty());
    let first_file = format!("{PROJECT}/edge:{}", arca_incus::exec::EDGE_CONFIG);
    let first_before = rig.fake.service_files()[&first_file].clone();
    rig.call("service_create", json!({"name":"edge2","template":"edge"}))
        .unwrap();
    rig.call("service_connect", json!({"from":"edge2","to":"web"}))
        .unwrap();
    let mut second = rig
        .entries()
        .into_iter()
        .find(|entry| entry.id.0 == "service-edge2")
        .unwrap();
    let mut second_routes = first_routes.clone();
    second_routes[0].hostname = "second.example.com".into();
    second.config["edge"]["routes"] = serde_json::to_value(&second_routes).unwrap();
    rig.kernel
        .lock()
        .unwrap()
        .propose(vec![arca::desired_store::EntryChange::Put(second)], NOW)
        .unwrap();
    let second_cfg = rig.service_config("edge2");
    let initial = arca::route::caddyfile(second_cfg.edge.as_ref().unwrap(), &[]);
    rig.fake.set_service_file(
        PROJECT,
        "edge2",
        arca_incus::exec::EDGE_CONFIG,
        initial.as_bytes(),
    );
    let second_file = format!("{PROJECT}/edge2:{}", arca_incus::exec::EDGE_CONFIG);
    let second_before = rig.fake.service_files()[&second_file].clone();
    edge_reload(&rig);
    connect(&rig, &key, Some("node-ts")).unwrap();
    edge_reload(&rig);
    rig.call("service_connect", json!({"from":"node-ts","to":"edge2"}))
        .unwrap();
    let args = disconnect_args(&rig, &key, "node-ts");
    edge_reload(&rig);
    edge_reload(&rig);
    assert_eq!(rig.call("reach_disconnect", args).unwrap()["removed"], true);
    assert_eq!(
        rig.fake.service_files()[&first_file],
        first_before,
        "first edge file restored"
    );
    assert_eq!(
        rig.fake.service_files()[&second_file],
        second_before,
        "second edge file restored"
    );
    for (name, routes) in [("edge", first_routes), ("edge2", second_routes)] {
        let record = rig.service_config(name);
        assert!(
            record.incoming.is_empty(),
            "{name} has no incoming connector grant"
        );
        assert_eq!(
            record.edge.unwrap().routes,
            routes,
            "{name} routes unchanged"
        );
    }
}
