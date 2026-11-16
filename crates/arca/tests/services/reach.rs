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
    arca::cmd_templates::install_into(&rig.dir.join("templates"), false).unwrap();
    rig.template("edge", json!({"name":"edge","kind":"oci","image":"docker.io/library/example","digest":DIGEST,"ports":[443],"env":{},"lan":{"port":443},"edge":true,"provides":{"env":{"ARCA_EDGE_TARGET":"{{host}}:{{port}}"}}}));
    rig.call("service_create", json!({"name":"edge","template":"edge"}))
        .unwrap();
    rig.fake.script_exec(ExecScript::new(
        arca_incus::exec::ExecCommand::EdgeReload.argv(),
        0,
        "",
    ));
    (rig, key)
}

fn proposed(rig: &Rig, args: Value) -> Value {
    try_proposed(rig, args).unwrap()
}

fn try_proposed(rig: &Rig, args: Value) -> Result<Value, CallError> {
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
    plans["action_propose"].call(json!({"action":"reach_connect","args":args}), &doctor())
}

fn approved_args(rig: &Rig, key: &common::SkKey, proposal: &Value) -> (Value, String) {
    static NONCE: AtomicU64 = AtomicU64::new(0x7400);
    let statement = arca::handlers::action_statement(
        "node1",
        proposal["id"].as_str().unwrap(),
        "reach_connect",
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
    println!("privacy sweep rows={}", cases.len());
    privacy_cases(
        &cases
            .iter()
            .map(|(label, raw)| (label.as_str(), raw.clone()))
            .collect::<Vec<_>>(),
        id,
        false,
    );
}

#[test]
fn logs_privacy_two_inserts() {
    let id = "syntheticDouble1234";
    privacy_cases(
        &[
            (
                "host and identifier",
                "visit https://\u{1b}login.tailscale.com/a/synthetic\0Double1234\n".into(),
            ),
            (
                "mixed host sequences",
                "visit https://lo\u{1b}[1mgin.tails\u{1b}[cale.com/a/syntheticDouble1234\n".into(),
            ),
        ],
        id,
        false,
    );
}

#[test]
fn logs_privacy_ordinary_output_and_short_identifiers() {
    let (rig, key) = rig();
    connect(&rig, &key, Some("node-ts")).unwrap();
    let raw = "To authenticate, visit:\n\thttps://login.tailscale.com/a/syntheticPlain1234\nlogin.tailscale.com/admin/machines\ngo on and stay up\nhttps://login.tailscale.com/a/go\ngo on\n";
    assert_eq!(
        privacy_log_lines(&rig, "node-ts", raw),
        [
            "To authenticate, visit:",
            "[sign-in link]",
            "login.tailscale.com/admin/machines",
            "go on and stay up",
            "[sign-in link]",
            "go on"
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
