use arca_link::{Config, Link, Network, PAIR_ALPN, PairLink, State};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use iroh::EndpointAddr;
use iroh_tickets::endpoint::EndpointTicket;
use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt, path::PathBuf, time::Duration};
use tokio::{net::UnixListener, time::timeout};

const FORWARD: &[u8] = b"test/1";
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let mut bytes = [0; 16];
        getrandom::fill(&mut bytes).unwrap();
        let path =
            std::env::temp_dir().join(format!("arca-link-{}", URL_SAFE_NO_PAD.encode(bytes)));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn state(&self) -> State {
        State::new(&self.0).unwrap()
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

async fn node(temp: &Temp, name: &str, echo: bool) -> Link {
    let forward = if echo {
        BTreeMap::from([("test/1".to_string(), temp.0.join("echo.sock"))])
    } else {
        BTreeMap::new()
    };
    Link::start(
        temp.state(),
        Config {
            name: Some(name.to_string()),
            forward,
            ..Config::default()
        },
        Network::local(),
    )
    .await
    .unwrap()
}
fn addr(node: &Link) -> EndpointAddr {
    EndpointAddr::new(node.endpoint().id()).with_ip_addr(node.endpoint().bound_sockets()[0])
}
fn invite(temp: &Temp, node: &Link) -> String {
    let mut output = temp.state().pair(10).unwrap();
    // Production status uses endpoint.addr(); tests explicitly ticket the bound local address.
    let offset = output.link.find("&t=").unwrap();
    output.link.truncate(offset + 3);
    output
        .link
        .push_str(&EndpointTicket::new(addr(node)).to_string());
    output.link
}
async fn paired(temp: &Temp, host: &Link, guest: &Link) -> String {
    let text = invite(temp, host);
    guest.join(&PairLink::parse(&text).unwrap()).await.unwrap();
    text
}
async fn echo(listener: UnixListener) {
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            break;
        };
        tokio::spawn(async move {
            let (mut read, mut write) = socket.split();
            let _ = tokio::io::copy(&mut read, &mut write).await;
        });
    }
}

#[tokio::test]
async fn pair_forward_remove_and_keep_membership_local() {
    timeout(Duration::from_secs(20), async {
        let (a, b, c) = (Temp::new(), Temp::new(), Temp::new());
        let listener = UnixListener::bind(a.0.join("echo.sock")).unwrap();
        let echo_task = tokio::spawn(echo(listener));
        let host = node(&a, "first", true).await;
        let guest = node(&b, "second & unicode λ", true).await;
        let third = node(&c, "third", false).await;
        // The command's pair path creates status-backed state and the URI consumed by join.
        let text = paired(&a, &host, &guest).await;
        assert_eq!(
            b.state().peers().unwrap()[0].id,
            host.endpoint().id().to_string()
        );
        assert_eq!(a.state().peers().unwrap()[0].name, "second & unicode λ");
        assert!(!a.0.join("pairing.json").exists());
        assert!(
            guest.join(&PairLink::parse(&text).unwrap()).await.is_err(),
            "a consumed code cannot be reused"
        );

        let conn = guest
            .endpoint()
            .connect(addr(&host), FORWARD)
            .await
            .unwrap();
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        let bytes: Vec<_> = (0..=255).cycle().take(256 * 1024).collect();
        let send_task = tokio::spawn(async move {
            send.write_all(&bytes).await.unwrap();
            send.finish().unwrap();
            bytes
        });
        let echoed = recv.read_to_end(512 * 1024).await.unwrap();
        assert_eq!(echoed, send_task.await.unwrap());

        // A second stream stays live until membership is removed.
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        send.write_all(b"open").await.unwrap();
        let mut data = [0; 4];
        recv.read_exact(&mut data).await.unwrap();
        assert_eq!(&data, b"open");
        paired(&b, &guest, &third).await;
        assert!(
            b.state()
                .peers()
                .unwrap()
                .iter()
                .any(|p| p.id == third.endpoint().id().to_string())
        );
        assert!(
            !a.state()
                .peers()
                .unwrap()
                .iter()
                .any(|p| p.id == third.endpoint().id().to_string())
        );
        assert!(
            !c.state()
                .peers()
                .unwrap()
                .iter()
                .any(|p| p.id == host.endpoint().id().to_string())
        );
        let refused = third
            .endpoint()
            .connect(addr(&host), FORWARD)
            .await
            .unwrap();
        let closed = refused.closed().await;
        assert!(closed.to_string().contains("not paired"), "{closed}");

        assert_eq!(a.state().remove("second & unicode λ").unwrap(), 1);
        host.refresh_peers();
        let closed = conn.closed().await;
        assert!(closed.to_string().contains("not paired"), "{closed}");
        assert!(recv.read_exact(&mut data).await.is_err());
        host.shutdown().await;
        guest.shutdown().await;
        third.shutdown().await;
        assert!(!a.0.join("status.json").exists());
        echo_task.abort();
    })
    .await
    .expect("local link test deadline");
}

#[tokio::test]
async fn codes_expire_and_wrong_guesses_are_counted() {
    timeout(Duration::from_secs(20), async {
        let (a, b) = (Temp::new(), Temp::new());
        let host = node(&a, "first", false).await;
        let guest = node(&b, "second", false).await;
        let text = invite(&a, &host);
        let mut wrong = text.clone();
        let start = wrong.find("&c=").unwrap() + 3;
        let end = wrong[start..].find('&').unwrap() + start;
        let mut random = [0; 16];
        getrandom::fill(&mut random).unwrap();
        let random = URL_SAFE_NO_PAD.encode(random);
        wrong.replace_range(start..end, &random);
        for _ in 0..3 {
            assert!(guest.join(&PairLink::parse(&wrong).unwrap()).await.is_err());
        }
        let pending: serde_json::Value =
            serde_json::from_slice(&fs::read(a.0.join("pairing.json")).unwrap()).unwrap();
        assert_eq!(pending["failures"], 3);
        assert_eq!(pending["tries"][guest.endpoint().id().to_string()], 3);
        assert!(
            guest.join(&PairLink::parse(&text).unwrap()).await.is_err(),
            "peer is exhausted even with the correct code"
        );
        let mut pending = pending;
        pending["tries"] = serde_json::json!({});
        pending["failures"] = serde_json::json!(1000);
        fs::write(
            a.0.join("pairing.json"),
            serde_json::to_vec(&pending).unwrap(),
        )
        .unwrap();
        assert!(
            guest.join(&PairLink::parse(&text).unwrap()).await.is_err(),
            "code-wide limit"
        );
        a.state().pair_at(1, 0).unwrap();
        let pending: serde_json::Value =
            serde_json::from_slice(&fs::read(a.0.join("pairing.json")).unwrap()).unwrap();
        let expired = format!(
            "arca://pair?v=1&n=first&c={}&t={}",
            pending["code"].as_str().unwrap(),
            EndpointTicket::new(addr(&host))
        );
        assert!(
            guest
                .join(&PairLink::parse(&expired).unwrap())
                .await
                .is_err()
        );
        assert!(a.state().peers().unwrap().is_empty());
        assert!(b.state().peers().unwrap().is_empty());
        let fresh = paired(&a, &host, &guest).await;
        assert!(PairLink::parse(&fresh).is_ok());
        host.shutdown().await;
        guest.shutdown().await;
    })
    .await
    .expect("pair test deadline");
}

#[tokio::test]
async fn unknown_peer_never_opens_local_socket_and_disk_admission_is_live() {
    timeout(Duration::from_secs(20), async {
        let (a, b) = (Temp::new(), Temp::new());
        let listener = UnixListener::bind(a.0.join("echo.sock")).unwrap();
        let host = node(&a, "first", true).await;
        let guest = node(&b, "second", false).await;
        let connection = guest
            .endpoint()
            .connect(addr(&host), FORWARD)
            .await
            .unwrap();
        assert!(connection.closed().await.to_string().contains("not paired"));
        assert!(
            timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
        paired(&a, &host, &guest).await;
        // No sweep is needed for admission on the next connection.
        a.state().remove("second").unwrap();
        let connection = guest
            .endpoint()
            .connect(addr(&host), FORWARD)
            .await
            .unwrap();
        assert!(connection.closed().await.to_string().contains("not paired"));
        assert!(
            timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
        host.shutdown().await;
        guest.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn malformed_pair_lines_and_running_join_are_bounded() {
    timeout(Duration::from_secs(20), async {
        let (a, b) = (Temp::new(), Temp::new());
        let host = node(&a, "first λ &", false).await;
        let guest = node(&b, "second", false).await;
        let text = invite(&a, &host);
        assert_eq!(PairLink::parse(&text).unwrap().name, "first λ &");
        let peer = arca_link::join_running(&b.state(), &text).await.unwrap();
        assert_eq!(peer.id, host.endpoint().id().to_string());
        // The same endpoint key is used through local control.
        assert_eq!(
            a.state().peers().unwrap()[0].id,
            guest.endpoint().id().to_string()
        );
        let conn = guest
            .endpoint()
            .connect(addr(&host), PAIR_ALPN)
            .await
            .unwrap();
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        send.write_all(&vec![b'x'; 4096]).await.unwrap();
        send.finish().unwrap();
        assert!(recv.read_to_end(4096).await.is_err());
        // A successful pairing connection is also revoked if its caller leaves it open.
        let fresh = invite(&a, &host);
        let code = fresh
            .split("&c=")
            .nth(1)
            .unwrap()
            .split('&')
            .next()
            .unwrap();
        let paired_conn = guest
            .endpoint()
            .connect(addr(&host), PAIR_ALPN)
            .await
            .unwrap();
        let (mut send, mut recv) = paired_conn.open_bi().await.unwrap();
        let mut request =
            serde_json::to_vec(&serde_json::json!({"code": code, "device": "second"})).unwrap();
        request.push(b'\n');
        send.write_all(&request).await.unwrap();
        send.finish().unwrap();
        let reply: serde_json::Value =
            serde_json::from_slice(&recv.read_to_end(4096).await.unwrap()).unwrap();
        assert_eq!(reply["ok"], true);
        a.state().remove("second").unwrap();
        host.refresh_peers();
        assert!(
            paired_conn
                .closed()
                .await
                .to_string()
                .contains("not paired")
        );
        host.shutdown().await;
        guest.shutdown().await;
    })
    .await
    .unwrap();
}

#[test]
fn state_is_private_keys_are_stable_and_stale_status_is_refused() {
    let temp = Temp::new();
    let state = temp.state();
    let first = state.secret().unwrap();
    assert_eq!(first.public(), state.secret().unwrap().public());
    assert_eq!(fs::read(temp.0.join("secret.key")).unwrap().len(), 32);
    assert_eq!(
        fs::metadata(temp.0.join("secret.key"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(state.pair(10).is_err());
    fs::write(temp.0.join("status.json"), b"{}").unwrap();
    assert!(state.status().is_err());
    for text in [
        "",
        "arca://pair?",
        "arca://pair?v=2&n=x&c=secret&t=bad",
        "arca://pair?v=1&v=1",
        "arca://pair?n=%zz",
    ] {
        let error = PairLink::parse(text).err().unwrap();
        assert_eq!(error.to_string(), "invalid pairing link");
    }
}

#[tokio::test]
async fn first_stream_and_first_pair_line_have_an_accept_deadline() {
    timeout(Duration::from_secs(14), async {
        let (a, b) = (Temp::new(), Temp::new());
        let host = node(&a, "first", false).await;
        let guest = node(&b, "second", false).await;
        let no_stream = guest
            .endpoint()
            .connect(addr(&host), PAIR_ALPN)
            .await
            .unwrap();
        let partial_line = guest
            .endpoint()
            .connect(addr(&host), PAIR_ALPN)
            .await
            .unwrap();
        let (mut send, _recv) = partial_line.open_bi().await.unwrap();
        send.write_all(b"{").await.unwrap();
        let (first, second) = tokio::join!(no_stream.closed(), partial_line.closed());
        assert!(first.to_string().contains("too slow"), "{first}");
        assert!(second.to_string().contains("too slow"), "{second}");
        host.shutdown().await;
        guest.shutdown().await;
    })
    .await
    .expect("accept deadline must close both idle connections");
}

#[tokio::test]
async fn pairing_sanitizes_device_names_before_storing() {
    timeout(Duration::from_secs(10), async {
        let (a, b) = (Temp::new(), Temp::new());
        let host = node(&a, "first", false).await;
        let guest = node(&b, "second", false).await;
        let controls: String = (0..=31).map(char::from).collect();
        for (device, expected) in [
            (
                format!("{controls}{}\u{001f}", "λ".repeat(61)),
                "λ".repeat(60),
            ),
            (controls, "device".to_string()),
            (String::new(), "device".to_string()),
        ] {
            let text = invite(&a, &host);
            let code = text.split("&c=").nth(1).unwrap().split('&').next().unwrap();
            let connection = guest
                .endpoint()
                .connect(addr(&host), PAIR_ALPN)
                .await
                .unwrap();
            let (mut send, mut recv) = connection.open_bi().await.unwrap();
            let mut bytes =
                serde_json::to_vec(&serde_json::json!({"code": code, "device": device})).unwrap();
            bytes.push(b'\n');
            send.write_all(&bytes).await.unwrap();
            send.finish().unwrap();
            let reply: serde_json::Value =
                serde_json::from_slice(&recv.read_to_end(4096).await.unwrap()).unwrap();
            assert_eq!(reply["ok"], true);
            assert_eq!(a.state().peers().unwrap()[0].name, expected);
            connection.close(0u32.into(), b"done");
        }
        host.shutdown().await;
        guest.shutdown().await;
    })
    .await
    .expect("name sanitation test deadline");
}
