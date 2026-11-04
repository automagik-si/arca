//! Peer calls reuse a live, direct-only link and carry MCP HTTP bytes unchanged.

use arca_link::{Config, Link, Network, PairLink, State};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use iroh_tickets::endpoint::EndpointTicket;
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt, path::PathBuf, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixListener,
    time::timeout,
};

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let mut bytes = [0; 16];
        getrandom::fill(&mut bytes).unwrap();
        let path = std::env::temp_dir().join(format!("arca-mcp-{}", URL_SAFE_NO_PAD.encode(bytes)));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }

    fn state(&self) -> State {
        State::new(&self.0).unwrap()
    }

    fn call_args(&self, peer: &str) -> Vec<String> {
        vec![
            "call".into(),
            "arca_status".into(),
            "--peer".into(),
            peer.into(),
            "--config".into(),
            self.0.join("node.json").to_str().unwrap().into(),
            "--token-file".into(),
            self.0.join("token").to_str().unwrap().into(),
        ]
    }

    fn config(&self) -> String {
        let cfg = json!({
            "stateDir": self.0,
            "incus": {"socket": self.0.join("unused-incus.sock")},
            "listen": {"unix": self.0.join("unused-local.sock")},
            "tokens": {"hashDir": self.0.join("unused-tokens")},
            "kernel": {"droiddProjects": ["droid"]}
        });
        fs::write(self.0.join("node.json"), cfg.to_string()).unwrap();
        let mut bytes = [0; 32];
        getrandom::fill(&mut bytes).unwrap();
        let token = format!("arca_{}", URL_SAFE_NO_PAD.encode(bytes));
        let path = self.0.join("token");
        fs::write(&path, format!("{token}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        token
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

async fn fake_mcp(listener: UnixListener) -> (UnixListener, Vec<(String, Value)>) {
    let mut requests = Vec::new();
    for _ in 0..2 {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            assert!(head.len() < 8192, "bounded HTTP header");
            head.push(socket.read_u8().await.unwrap());
        }
        let head = String::from_utf8(head).unwrap();
        assert!(head.starts_with("POST /mcp HTTP/1.1\r\n"));
        assert!(head.contains("\r\nConnection: close\r\n"));
        let length: usize = head
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .unwrap()
            .parse()
            .unwrap();
        let authorization = head
            .lines()
            .find(|line| line.starts_with("Authorization: "))
            .unwrap()
            .to_string();
        let mut body = vec![0; length];
        socket.read_exact(&mut body).await.unwrap();
        // The client half-closes each POST, and the link must propagate that EOF.
        assert_eq!(socket.read(&mut [0]).await.unwrap(), 0);
        let request: Value = serde_json::from_slice(&body).unwrap();
        let result = if request["method"] == "initialize" {
            json!({})
        } else {
            json!({"structuredContent": {"owner": "arca"}})
        };
        let reply = json!({"jsonrpc": "2.0", "id": request["id"], "result": result}).to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
            reply.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.shutdown().await.unwrap();
        requests.push((authorization, request));
    }
    (listener, requests)
}

#[tokio::test]
async fn peer_call_carries_both_posts_and_bearer_without_opening_another_endpoint() {
    timeout(Duration::from_secs(30), async {
        let (host_dir, caller_dir) = (Temp::new(), Temp::new());
        let token = caller_dir.config();
        let listener = UnixListener::bind(host_dir.0.join("fake.sock")).unwrap();
        let host = Link::start(
            host_dir.state(),
            Config {
                name: Some("host".into()),
                forward: BTreeMap::from([("arca/mcp/1".into(), host_dir.0.join("fake.sock"))]),
                ..Config::default()
            },
            Network::local(),
        )
        .await
        .unwrap();
        let caller = Link::start(
            caller_dir.state(),
            Config {
                name: Some("caller".into()),
                ..Config::default()
            },
            Network::local(),
        )
        .await
        .unwrap();
        let ticket =
            EndpointTicket::new(host.addr().with_ip_addr(host.endpoint().bound_sockets()[0]))
                .to_string();
        let invitation = host_dir.state().pair(10).unwrap().link;
        let (invitation, _) = invitation.split_once("&t=").unwrap();
        caller
            .join(&PairLink::parse(&format!("{invitation}&t={ticket}")).unwrap())
            .await
            .unwrap();
        assert_eq!(
            host_dir.state().peers().unwrap()[0].id,
            caller.endpoint().id().to_string()
        );
        // Pairing writes no ticket yet; supply the host's direct-only address hint.
        let path = caller_dir.0.join("peers.json");
        let mut peers: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        peers[0]["ticket"] = json!(ticket);
        fs::write(path, peers.to_string()).unwrap();

        let unknown_args = caller_dir.call_args("missing");
        let error = tokio::task::spawn_blocking(move || arca::run(&unknown_args))
            .await
            .unwrap()
            .unwrap_err();
        assert!(matches!(error, arca::Failure::Runtime(_)));
        assert_eq!(error.code(), 1);
        assert_eq!(error.message(), "unknown peer name");
        assert!(
            timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "unknown peer must not reach MCP"
        );

        let fake = tokio::spawn(fake_mcp(listener));
        let args = caller_dir.call_args("host");
        let output = tokio::task::spawn_blocking(move || arca::run(&args))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(output, Some(json!({"owner": "arca"})));
        let (listener, requests) = fake.await.unwrap();
        assert_eq!(requests.len(), 2);
        for (header, _) in &requests {
            assert!(header == &format!("Authorization: Bearer {token}"));
        }
        assert_eq!(requests[0].1["method"], "initialize");
        assert_eq!(requests[1].1["method"], "tools/call");
        assert_eq!(requests[1].1["params"]["name"], "arca_status");
        assert_eq!(requests[1].1["params"]["arguments"], json!({}));
        assert!(
            timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "one stream per POST"
        );
        caller.shutdown().await;
        host.shutdown().await;
    })
    .await
    .expect("peer call deadline");
}

#[test]
fn peer_call_without_a_running_link_is_a_runtime_failure_and_creates_no_key() {
    let temp = Temp::new();
    temp.config();
    let mut args = temp.call_args("host");
    for prefix in [false, true] {
        if prefix {
            args[1..4].rotate_left(1);
        }
        let error = arca::run(&args).unwrap_err();
        assert!(matches!(error, arca::Failure::Runtime(_)));
        assert_eq!(error.code(), 1);
        assert_eq!(error.message(), "the link is not running");
        assert!(!temp.0.join("secret.key").exists());
        assert!(!temp.0.join(".link-control.sock").exists());
    }
}
