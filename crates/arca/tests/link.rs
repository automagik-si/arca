//! The CLI uses direct-only in-process endpoints; this test never contacts a relay.
use arca_link::{Config, Link, Network, State};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use iroh_tickets::endpoint::EndpointTicket;
use serde_json::{Value, json};
use std::{fs, path::PathBuf, time::Duration};
use tokio::time::timeout;

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let mut bytes = [0; 16];
        getrandom::fill(&mut bytes).unwrap();
        let path =
            std::env::temp_dir().join(format!("arca-cli-link-{}", URL_SAFE_NO_PAD.encode(bytes)));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn config(&self) -> String {
        let path = self.0.join("config.json");
        fs::write(
            &path,
            serde_json::to_vec(&json!({
                "stateDir": self.0.join("state"),
                "incus": {"socket": self.0.join("incus.sock")},
                "listen": {"unix": self.0.join("arca.sock")},
                "tokens": {"hashDir": self.0.join("tokens")},
                "kernel": {"droiddProjects": []},
                "link": {"name": "fixture", "forward": {}, "pairMinutes": 1, "localOnly": true}
            }))
            .unwrap(),
        )
        .unwrap();
        path.to_str().unwrap().to_string()
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn command(args: &[&str]) -> Value {
    arca::run(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn link_cli_pairs_joins_lists_and_removes_without_relays() {
    timeout(Duration::from_secs(20), async {
        let (host_dir, guest_dir) = (Temp::new(), Temp::new());
        let (host_config, guest_config) = (host_dir.config(), guest_dir.config());
        let host = Link::start(
            State::new(host_dir.0.join("state")).unwrap(),
            Config {
                name: Some("first λ &".to_string()),
                ..Config::default()
            },
            Network::local(),
        )
        .await
        .unwrap();
        let guest = Link::start(
            State::new(guest_dir.0.join("state")).unwrap(),
            Config {
                name: Some("second".to_string()),
                ..Config::default()
            },
            Network::local(),
        )
        .await
        .unwrap();
        let output = command(&["link", "pair", "--config", &host_config, "--minutes", "0"]);
        let expires = output["expires"].as_u64().unwrap();
        assert!(expires <= arca_link::now_ms() + 60_000);
        let mut text = output["link"].as_str().unwrap().to_string();
        let offset = text.find("&t=").unwrap();
        text.truncate(offset + 3);
        text.push_str(
            &EndpointTicket::new(host.addr().with_ip_addr(host.endpoint().bound_sockets()[0]))
                .to_string(),
        );
        let config = guest_config.clone();
        let joined = tokio::task::spawn_blocking(move || {
            command(&["link", "join", &text, "--config", &config])
        })
        .await
        .unwrap();
        assert_eq!(joined["id"], host.endpoint().id().to_string());
        assert_eq!(joined["name"], "first λ &");
        assert_eq!(
            command(&["link", "peers", "--config", &guest_config])[0]["id"],
            joined["id"]
        );
        let config = guest_config.clone();
        let removed = tokio::task::spawn_blocking(move || {
            command(&["link", "remove", "first λ &", "--config", &config])
        })
        .await
        .unwrap();
        assert_eq!(removed, json!({"removed": 1}));
        assert_eq!(
            command(&["link", "peers", "--config", &guest_config]),
            json!([])
        );
        let error = arca::run(
            &[
                "link",
                "join",
                "arca://pair?c=private-code",
                "--config",
                &guest_config,
            ]
            .map(String::from),
        )
        .unwrap_err();
        assert_eq!(error.code(), 2);
        assert_eq!(error.message(), "invalid pairing link");
        host.shutdown().await;
        guest.shutdown().await;
    })
    .await
    .expect("CLI link test deadline");
}

#[test]
fn link_remove_accepts_end_of_options_before_name() {
    let temp = Temp::new();
    let config = temp.config();
    let remove = || {
        std::process::Command::new(env!("CARGO_BIN_EXE_arca"))
            .args(["link", "remove", "--", "--probe", "--config", &config])
            .output()
            .unwrap()
    };
    let output = remove();
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({"removed": 0})
    );
    assert!(temp.0.join("state/removed.json").is_file());

    let state = State::new(temp.0.join("state")).unwrap();
    let peer = arca_link::Peer {
        id: state.secret().unwrap().public().to_string(),
        name: "--probe".to_string(),
        paired: arca_link::now_ms(),
        seen: arca_link::now_ms(),
        ticket: None,
    };
    fs::write(
        temp.0.join("state/peers.json"),
        serde_json::to_vec(&vec![peer.clone()]).unwrap(),
    )
    .unwrap();
    let unmarked = std::process::Command::new(env!("CARGO_BIN_EXE_arca"))
        .args(["link", "remove", "--probe", "--config", &config])
        .output()
        .unwrap();
    assert_eq!(unmarked.status.code(), Some(2));
    assert!(String::from_utf8(unmarked.stderr).unwrap().starts_with(
        "arca: link remove needs the name of the peer to remove; arca link peers lists them"
    ));
    assert_eq!(state.peers().unwrap(), vec![peer]);
    let output = remove();
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({"removed": 1})
    );
    assert!(state.peers().unwrap().is_empty());
}

#[test]
fn link_join_accepts_end_of_options_before_link() {
    let error = arca::run(
        &["link", "join", "--", "--invalid-link", "--config", "unused"].map(String::from),
    )
    .unwrap_err();
    assert_eq!(error.code(), 2);
    assert_eq!(error.message(), "invalid pairing link");
}

#[test]
fn link_serve_sigterm_exits_cleanly_and_removes_runtime_files_without_relays() {
    let temp = Temp::new();
    let config = temp.config();
    // Reserve a silent direct peer: startup sync cannot complete before the signal.
    let silent = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let id = State::new(temp.0.join("silent"))
        .unwrap()
        .secret()
        .unwrap()
        .public();
    let state = temp.0.join("state");
    State::new(&state).unwrap();
    let mut peer = arca_link::Peer {
        id: id.to_string(),
        name: "silent".to_string(),
        paired: 1,
        seen: 1,
        ticket: None,
    };
    peer.ticket = Some(
        EndpointTicket::new(
            peer.addr()
                .unwrap()
                .with_ip_addr(silent.local_addr().unwrap()),
        )
        .to_string(),
    );
    fs::write(
        state.join("peers.json"),
        serde_json::to_vec(&vec![peer]).unwrap(),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(state.join("peers.json"), fs::Permissions::from_mode(0o600)).unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_arca"))
        .args(["link", "serve", "--config", &config])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !state.join("status.json").exists() {
        if let Some(exit) = child.try_wait().unwrap() {
            panic!("serve exited before publishing status: {exit}");
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("serve did not publish status");
        }
        std::thread::yield_now();
    }
    assert!(state.join(".link-control.sock").exists());
    assert!(
        std::process::Command::new("/bin/kill")
            .args(["-TERM", &child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    // Allow QUIC's best-effort close drain, while staying below the mesh dial timeout.
    let deadline = std::time::Instant::now() + Duration::from_secs(6);
    loop {
        if let Some(exit) = child.try_wait().unwrap() {
            assert!(exit.success(), "{exit}");
            break;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("serve did not stop after SIGTERM");
        }
        std::thread::yield_now();
    }
    assert!(!state.join("status.json").exists());
    assert!(!state.join(".link-control.sock").exists());
}

#[test]
fn link_rejects_public_state_directory_with_exit_two() {
    use std::os::unix::fs::PermissionsExt;
    let temp = Temp::new();
    let config = temp.config();
    let dir = temp.0.join("state");
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o750)).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_arca"))
        .args(["link", "peers", "--config", &config])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("link state directory must have mode 0700")
    );
    assert!(!dir.join("secret.key").exists());
}

/// Whether `child` still runs.
fn alive(child: &mut std::process::Child) -> bool {
    child.try_wait().unwrap().is_none()
}

#[test]
fn a_running_link_survives_token_reload_and_token_revoke_on_its_own_config() {
    use std::os::unix::fs::PermissionsExt;
    let arca = || std::process::Command::new(env!("CARGO_BIN_EXE_arca"));
    let temp = Temp::new();
    let config = temp.config();
    let state = temp.0.join("state");
    State::new(&state).unwrap();
    // A token to revoke later, minted before anything runs.
    let tokens = temp.0.join("tokens");
    fs::create_dir(&tokens).unwrap();
    fs::set_permissions(&tokens, fs::Permissions::from_mode(0o700)).unwrap();
    let minted = arca()
        .args([
            "token",
            "mint",
            "--scope",
            "viewer",
            "--id",
            "link-test-1",
            "--out",
        ])
        .arg(temp.0.join("link-test-1.secret"))
        .args(["--config", &config])
        .output()
        .unwrap();
    assert!(minted.status.success(), "{minted:?}");

    // The node's own link, on the node's config. No node runs, so both commands look for the node by its command line,
    // and the link's command line names `serve` and this config too.
    let mut link = arca()
        .args(["link", "serve", "--config", &config])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    while !state.join("status.json").exists() {
        assert!(
            alive(&mut link),
            "the link exited before publishing its status"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "the link publishes its status"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    let reload = arca()
        .args(["token", "reload", "--config", &config])
        .output()
        .unwrap();
    let after_reload = alive(&mut link);
    let revoke = arca()
        .args(["token", "revoke", "link-test-1", "--config", &config])
        .output()
        .unwrap();
    let after_revoke = alive(&mut link);
    link.kill().unwrap();
    link.wait().unwrap();

    assert!(after_reload, "token reload did not signal the link");
    assert_eq!(reload.status.code(), Some(1), "{reload:?}");
    assert!(
        String::from_utf8_lossy(&reload.stderr)
            .starts_with("arca: no node runs on this config, so nothing was reloaded."),
        "{reload:?}"
    );
    assert!(after_revoke, "token revoke did not signal the link");
    assert!(revoke.status.success(), "{revoke:?}");
    let answer: Value = serde_json::from_slice(&revoke.stdout).unwrap();
    assert_eq!(answer["revoked"], json!(true));
    assert_eq!(
        answer["reloadSignalled"],
        json!(false),
        "no node was there to signal"
    );
}

#[test]
fn the_variable_alone_never_picks_the_node_a_peer_is_joined_or_paired_on() {
    let arca = || {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_arca"));
        command.env_remove("ARCA_TOKEN_FILE");
        command
    };
    // Two nodes: the one the installer's default path names, and one the variable was left set for.
    let installed = Temp::new();
    let other = Temp::new();
    let other_config = other.config();
    let home = installed.0.join("home");
    let default = home.join(".config/arca/node.json");
    fs::create_dir_all(default.parent().unwrap()).unwrap();
    fs::copy(installed.config(), &default).unwrap();
    // A pairing link that parses: a code of the right length and a ticket for an endpoint nobody runs.
    let silent = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let id = State::new(installed.0.join("silent"))
        .unwrap()
        .secret()
        .unwrap()
        .public();
    let peer = arca_link::Peer {
        id: id.to_string(),
        name: "silent".to_string(),
        paired: 1,
        seen: 1,
        ticket: None,
    };
    let ticket = EndpointTicket::new(
        peer.addr()
            .unwrap()
            .with_ip_addr(silent.local_addr().unwrap()),
    );
    let link = format!(
        "arca://pair?v=1&n=silent&c={}&t={ticket}",
        URL_SAFE_NO_PAD.encode([7u8; 16])
    );
    let refusal = format!(
        "arca: ARCA_CONFIG names {other_config}, not the default config ({}). This command hands out, removes or rewrites something on one node, so the variable alone does not choose that node, and nothing was done. To act on the node ARCA_CONFIG names, run the same command with: --config {other_config}\n",
        default.display()
    );
    for args in [vec!["link", "join", link.as_str()], vec!["link", "pair"]] {
        let out = arca()
            .args(&args)
            .env("HOME", &home)
            .env("ARCA_CONFIG", &other_config)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{:?}: {out:?}", &args[..2]);
        assert!(out.stdout.is_empty(), "{:?}", &args[..2]);
        assert_eq!(
            String::from_utf8_lossy(&out.stderr),
            refusal,
            "{:?}",
            &args[..2]
        );
    }
    assert!(
        !other.0.join("state").exists(),
        "nothing was made on the node the variable names"
    );
    // The variable names the default file itself: pairing goes on, says so, and then finds no link running.
    let out = arca()
        .args(["link", "pair"])
        .env("HOME", &home)
        .env("ARCA_CONFIG", &default)
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&out.stderr).starts_with(&format!(
            "arca: using the config {} (from ARCA_CONFIG)\n",
            default.display()
        )),
        "{out:?}"
    );
}

#[test]
fn link_peers_shows_the_path_in_use_only_while_a_fresh_record_exists() {
    let temp = Temp::new();
    let config = temp.config();
    let state = State::new(temp.0.join("state")).unwrap();
    let peer = arca_link::Peer {
        id: state.secret().unwrap().public().to_string(),
        name: "fiber".to_string(),
        paired: arca_link::now_ms(),
        seen: arca_link::now_ms(),
        ticket: None,
    };
    fs::write(
        temp.0.join("state/peers.json"),
        serde_json::to_vec(&vec![peer]).unwrap(),
    )
    .unwrap();
    let peers = || {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_arca"))
            .args(["link", "peers", "--config", &config])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    assert!(peers()[0].get("path").is_none(), "no running link, no path");
    let record = |at: u64| {
        json!({"fiber": {"via": "direct", "addr": "10.0.0.2:7000", "named": true, "at": at}})
    };
    fs::write(
        temp.0.join("state/paths.json"),
        serde_json::to_vec(&record(arca_link::now_ms())).unwrap(),
    )
    .unwrap();
    assert_eq!(
        peers()[0]["path"],
        json!({"via": "direct", "addr": "10.0.0.2:7000", "named": true})
    );
    fs::write(
        temp.0.join("state/paths.json"),
        serde_json::to_vec(&record(arca_link::now_ms() - 60_000)).unwrap(),
    )
    .unwrap();
    assert!(peers()[0].get("path").is_none(), "an old record is not shown");
}
