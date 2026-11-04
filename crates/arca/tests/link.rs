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
fn link_serve_sigterm_exits_cleanly_and_removes_runtime_files_without_relays() {
    let temp = Temp::new();
    let config = temp.config();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_arca"))
        .args(["link", "serve", "--config", &config])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let state = temp.0.join("state");
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
