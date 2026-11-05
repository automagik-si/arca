//! The three listeners: the unix socket at mode 0600, the tailnet address and its peer filter, and the proxy address and its
//! exact peer. Only loopback and temporary unix sockets are used. The tailnet ranges are checked as addresses; no test binds
//! a tailnet address.

mod common;

use std::fs::{self, Permissions};
use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, SocketAddr};
use std::os::unix::fs::{FileTypeExt, PermissionsExt, symlink};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::str::FromStr;
use std::time::{Duration, Instant};

use arca_api::Scope;
use arca_mcp::{
    Server, TokenStore, peer_allowed, proxy_addr_ok, serve_filtered, serve_proxy, serve_tailnet,
    serve_unix, tailnet_addr_ok,
};
use common::{
    Scratch, bearer, connect_tcp, connect_unix, mint_into, own_uid, post, post_bytes, rpc,
    wait_for_socket,
};
use serde_json::json;
use tokio::net::TcpListener;
use tokio::runtime::Runtime;

fn addr(text: &str) -> SocketAddr {
    SocketAddr::from_str(text).expect("a socket address literal")
}

fn ip(text: &str) -> IpAddr {
    IpAddr::from_str(text).expect("an IP address literal")
}

/// A router over an empty token store: every call is refused 401, which is what these tests look for.
fn empty_router(tag: &str) -> axum::Router {
    let scratch = Scratch::new(tag);
    let tokens = TokenStore::load(scratch.path()).expect("an empty store loads");
    Server::new(arca_api::verbs(), tokens).router()
}

// Tailnet address: the refusals and the acceptance, as the tailnet ranges and reserved ports define them.

#[test]
fn tailnet_addr_ok_refuses_the_wildcards() {
    for text in ["0.0.0.0:7450", "[::]:7450"] {
        assert!(
            tailnet_addr_ok(&addr(text)).is_err(),
            "a wildcard must be refused: {text}"
        );
    }
}

#[test]
fn tailnet_addr_ok_refuses_loopback_documentation_and_public_addresses() {
    for text in [
        "127.0.0.1:7450",
        "192.0.2.1:7450",
        "198.51.100.7:7450",
        "203.0.113.9:7450",
        "8.8.8.8:7450",
        "100.128.0.1:7450",
        "100.63.255.255:7450",
        "[fd7a:115c:a1e1::1]:7450",
        "[2001:db8::1]:7450",
        "[::1]:7450",
    ] {
        assert!(
            tailnet_addr_ok(&addr(text)).is_err(),
            "outside the tailnet ranges: {text}"
        );
    }
}

#[test]
fn tailnet_addr_ok_refuses_the_reserved_ports() {
    for port in [80, 443, 7443, 9443] {
        let text = format!("100.64.0.1:{port}");
        assert!(
            tailnet_addr_ok(&addr(&text)).is_err(),
            "reserved port refused: {text}"
        );
    }
}

#[test]
fn tailnet_addr_ok_refuses_port_zero() {
    assert!(
        tailnet_addr_ok(&addr("100.64.0.1:0")).is_err(),
        "an unnumbered port is no endpoint"
    );
}

#[test]
fn tailnet_addr_ok_refuses_an_ipv4_mapped_tailnet_address() {
    assert!(tailnet_addr_ok(&addr("[::ffff:100.64.0.1]:7450")).is_err());
}

#[test]
fn tailnet_addr_ok_accepts_a_tailnet_address_on_a_free_port() {
    for text in [
        "100.64.0.1:7450",
        "100.127.255.254:7450",
        "[fd7a:115c:a1e0::1]:7450",
    ] {
        assert_eq!(tailnet_addr_ok(&addr(text)), Ok(()), "accepted: {text}");
    }
}

// Peer filter: every TCP peer is checked against the same ranges.

#[test]
fn peer_allowed_refuses_addresses_outside_the_tailnet_ranges() {
    for text in [
        "198.51.100.7",
        "127.0.0.1",
        "192.168.1.1",
        "100.128.0.1",
        "100.63.255.255",
        "fd7a:115c:a1e1::1",
        "2001:db8::1",
        "::1",
    ] {
        assert!(
            !peer_allowed(&ip(text)),
            "a peer outside the ranges is refused: {text}"
        );
    }
}

#[test]
fn peer_allowed_accepts_addresses_inside_the_tailnet_ranges() {
    for text in [
        "100.100.1.1",
        "100.64.0.1",
        "100.127.255.255",
        "fd7a:115c:a1e0::1",
        "fd7a:115c:a1e0:ffff::1",
        "::ffff:100.100.1.1",
    ] {
        assert!(
            peer_allowed(&ip(text)),
            "a peer inside the ranges is accepted: {text}"
        );
    }
}

// The tailnet entry point never binds a refused address.

#[test]
fn serve_tailnet_refuses_a_loopback_address_before_it_binds() {
    let rt = Runtime::new().expect("runtime");
    let err = rt
        .block_on(serve_tailnet(empty_router("loopback"), addr("127.0.0.1:0")))
        .expect_err("a loopback address is not the tailnet");
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
}

#[test]
fn serve_tailnet_refuses_a_wildcard_before_it_binds() {
    let rt = Runtime::new().expect("runtime");
    let err = rt
        .block_on(serve_tailnet(
            empty_router("wildcard"),
            addr("0.0.0.0:7450"),
        ))
        .expect_err("a wildcard is never bound");
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
}

// The peer filter on loopback: a refused peer gets no bytes, an allowed one gets the server.

fn deny_all(_: &IpAddr) -> bool {
    false
}

fn allow_loopback(ip: &IpAddr) -> bool {
    ip.is_loopback()
}

#[test]
fn a_refused_peer_is_closed_with_no_bytes_sent() {
    let rt = Runtime::new().expect("runtime");
    let listener = rt
        .block_on(TcpListener::bind("127.0.0.1:0"))
        .expect("bind loopback");
    let at = listener.local_addr().expect("the bound address");
    rt.spawn(serve_filtered(empty_router("deny"), listener, deny_all));

    let mut stream = connect_tcp(at);
    let mut received = Vec::new();
    match stream.read_to_end(&mut received) {
        Ok(_) => {}
        Err(err) => assert!(
            !matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut),
            "the refused peer must be closed, not left open: {err}"
        ),
    }
    assert!(received.is_empty(), "a refused peer receives no bytes");
}

#[test]
fn an_allowed_peer_is_served_by_the_router() {
    let rt = Runtime::new().expect("runtime");
    let listener = rt
        .block_on(TcpListener::bind("127.0.0.1:0"))
        .expect("bind loopback");
    let at = listener.local_addr().expect("the bound address");
    rt.spawn(serve_filtered(
        empty_router("allow"),
        listener,
        allow_loopback,
    ));

    let mut stream = connect_tcp(at);
    let reply = post(&mut stream, &[], &rpc(json!(1), "tools/list", json!({})));
    assert_eq!(
        reply.status, 401,
        "an allowed peer reaches the router, which wants a bearer"
    );
}

#[test]
fn a_viewer_on_the_loopback_listener_lists_only_its_tools() {
    let scratch = Scratch::new("tcpview");
    let viewer = mint_into(scratch.path(), "tail-viewer", Scope::Viewer);
    let tokens = TokenStore::load(scratch.path()).expect("loads");
    let router = Server::new(arca_api::verbs(), tokens).router();
    let rt = Runtime::new().expect("runtime");
    let listener = rt
        .block_on(TcpListener::bind("127.0.0.1:0"))
        .expect("bind loopback");
    let at = listener.local_addr().expect("the bound address");
    rt.spawn(serve_filtered(router, listener, allow_loopback));

    let mut stream = connect_tcp(at);
    let reply = post(
        &mut stream,
        &[bearer(&viewer)],
        &rpc(json!(7), "tools/list", json!({})),
    );
    assert_eq!(reply.status, 200);
    let names: Vec<String> = reply.json()["result"]["tools"]
        .as_array()
        .expect("a tool list")
        .iter()
        .map(|t| t["name"].as_str().expect("a name").to_string())
        .collect();
    // The viewer's reads of the table include the ontology. No write verb is listed.
    assert_eq!(
        names,
        vec![
            "arca_status",
            "node_metrics",
            "watch_report",
            "journal_list",
            "plan_list",
            "alerts_list",
            "postgres_list",
            "ontology_query"
        ]
    );
}

// The unix socket: mode 0600, refuses what it must, replaces only a stale socket.

/// Waits until something exists at `path`, and returns at the first moment it does. Nothing is waited for after that.
fn wait_for_path(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while fs::symlink_metadata(path).is_err() {
        assert!(Instant::now() < deadline, "the socket path never appeared");
        std::thread::yield_now();
    }
}

/// The names in `dir`, sorted.
fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("list the directory")
        .map(|entry| {
            entry
                .expect("a directory entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

#[test]
fn serve_unix_binds_a_socket_of_mode_0600_that_answers_401_without_a_bearer() {
    let scratch = Scratch::new("unixok");
    let path = scratch.at("arca.sock");
    let rt = Runtime::new().expect("runtime");
    let (router, served) = (empty_router("unixok-router"), path.clone());
    rt.spawn(async move { serve_unix(router, &served).await });
    // The path appears only once the socket is at its final mode, so the first look at it sees 0600.
    wait_for_path(&path);

    let meta = fs::symlink_metadata(&path).expect("stat the socket");
    assert!(meta.file_type().is_socket(), "the path is a socket");
    assert_eq!(
        meta.permissions().mode() & 0o777,
        0o600,
        "the socket is 0600"
    );

    let mut stream = connect_unix(&path);
    let reply = post(&mut stream, &[], &rpc(json!(1), "ping", json!({})));
    assert_eq!(reply.status, 401);
    assert!(
        reply.header("www-authenticate").is_some(),
        "a 401 carries a bearer challenge"
    );
}

#[test]
fn serve_unix_refuses_an_existing_regular_file_and_leaves_it_alone() {
    let scratch = Scratch::new("unixfile");
    let path = scratch.at("arca.sock");
    fs::write(&path, "keep me").expect("write the file");
    let rt = Runtime::new().expect("runtime");
    let err = rt
        .block_on(serve_unix(empty_router("unixfile-r"), &path))
        .expect_err("a regular file is not replaced");
    assert_eq!(err.kind(), ErrorKind::AlreadyExists);
    assert_eq!(fs::read_to_string(&path).expect("still there"), "keep me");
}

#[test]
fn serve_unix_refuses_a_symlink_at_the_path_and_leaves_it_alone() {
    let scratch = Scratch::new("unixlink");
    let target = scratch.at("elsewhere");
    let path = scratch.at("arca.sock");
    fs::write(&target, "x").expect("write the target");
    symlink(&target, &path).expect("create the symlink");
    let rt = Runtime::new().expect("runtime");
    let err = rt
        .block_on(serve_unix(empty_router("unixlink-r"), &path))
        .expect_err("a symlink is never followed to a socket");
    assert_eq!(err.kind(), ErrorKind::AlreadyExists);
    assert!(
        fs::symlink_metadata(&path)
            .expect("still there")
            .file_type()
            .is_symlink()
    );
}

#[test]
fn serve_unix_refuses_a_socket_that_is_already_serving() {
    let scratch = Scratch::new("unixlive");
    let path = scratch.at("arca.sock");
    let live = UnixListener::bind(&path).expect("a live listener on the path");
    let rt = Runtime::new().expect("runtime");
    let err = rt
        .block_on(serve_unix(empty_router("unixlive-r"), &path))
        .expect_err("a live server keeps its socket");
    assert_eq!(err.kind(), ErrorKind::AddrInUse);
    drop(live);
}

#[test]
fn serve_unix_replaces_a_stale_socket_left_by_a_dead_server() {
    let scratch = Scratch::new("unixstale");
    let path = scratch.at("arca.sock");
    let dead = UnixListener::bind(&path).expect("bind, then let the listener die");
    drop(dead);
    assert!(path.exists(), "the stale socket file remains");
    let rt = Runtime::new().expect("runtime");
    let (router, served) = (empty_router("unixstale-r"), path.clone());
    rt.spawn(async move { serve_unix(router, &served).await });
    wait_for_socket(&path);
    let mut stream = connect_unix(&path);
    assert_eq!(
        post(&mut stream, &[], &rpc(json!(1), "ping", json!({}))).status,
        401
    );
}

#[test]
fn serve_unix_refuses_a_directory_at_the_path_and_leaves_it_alone() {
    let scratch = Scratch::new("unixdir");
    let path = scratch.at("arca.sock");
    fs::create_dir(&path).expect("a directory at the path");
    let rt = Runtime::new().expect("runtime");
    let err = rt
        .block_on(serve_unix(empty_router("unixdir-r"), &path))
        .expect_err("a directory is never removed or bound over");
    assert_eq!(err.kind(), ErrorKind::AlreadyExists);
    assert!(path.is_dir(), "the directory is still there");
}

#[test]
fn serve_unix_leaves_nothing_beside_the_socket_once_it_is_up() {
    let scratch = Scratch::new("unixstaged");
    let path = scratch.at("arca.sock");
    let rt = Runtime::new().expect("runtime");
    let (router, served) = (empty_router("unixstaged-r"), path.clone());
    rt.spawn(async move { serve_unix(router, &served).await });
    wait_for_path(&path);
    // The staging directory is removed right after the rename, so the listing settles a moment after the path appears.
    let deadline = Instant::now() + Duration::from_secs(5);
    while names_in(scratch.path()) != ["arca.sock"] {
        assert!(
            Instant::now() < deadline,
            "a staging entry was left beside the socket: {:?}",
            names_in(scratch.path())
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn serve_unix_fails_to_start_when_a_stale_socket_cannot_be_connected_to() {
    let scratch = Scratch::new("unixeacces");
    if own_uid(scratch.path()) == 0 {
        // Root connects to any socket, so there is no connect error to show; the other tests still cover the path.
        eprintln!("skipped: running as root, which connects to any socket");
        return;
    }
    let path = scratch.at("arca.sock");
    drop(UnixListener::bind(&path).expect("bind, then let the listener die"));
    fs::set_permissions(&path, Permissions::from_mode(0o000))
        .expect("make the stale socket unconnectable");
    let rt = Runtime::new().expect("runtime");
    let router = empty_router("unixeacces-r");
    let outcome = rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), serve_unix(router, &path)).await
    });
    match outcome {
        Ok(Err(err)) => {
            assert_eq!(
                err.kind(),
                ErrorKind::PermissionDenied,
                "a connect error other than refusal fails the start: {err}"
            );
        }
        Ok(Ok(())) => panic!("the server started over a socket it could not connect to"),
        Err(_) => panic!("the server did not fail to start within five seconds"),
    }
    assert!(
        fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_socket()),
        "a socket that is not known stale is left alone"
    );
}

/// The most connections one listener serves at once. Mirrors the server's cap.
const CAP: usize = 64;
/// How long a connection past the cap is given to answer before it counts as unanswered.
const QUIET: Duration = Duration::from_millis(500);

/// Holds `CAP` idle connections open, then sends one request on one more. While the cap is full that request gets no
/// reply within `QUIET`; once one held connection closes, it gets its reply.
fn cap_holds<S: Read + Write>(connect: impl Fn() -> S, set_read: impl Fn(&S, Option<Duration>)) {
    let request = post_bytes(&[], &rpc(json!(1), "ping", json!({})));
    let mut held: Vec<S> = (0..CAP).map(|_| connect()).collect();
    let mut extra = connect();
    set_read(&extra, Some(QUIET));
    extra
        .write_all(&request)
        .expect("write the request past the cap");
    let mut early = Vec::new();
    let _ = extra.read_to_end(&mut early);
    assert!(
        early.is_empty(),
        "a connection past the cap got a reply while the cap was full"
    );

    drop(held.pop());
    set_read(&extra, Some(Duration::from_secs(5)));
    let mut reply = Vec::new();
    extra
        .read_to_end(&mut reply)
        .expect("the queued connection is served once a slot frees");
    assert!(
        String::from_utf8_lossy(&reply).starts_with("HTTP/1.1 401"),
        "the queued request is answered"
    );
}

#[test]
fn a_tcp_connection_past_the_cap_waits_until_a_held_one_closes() {
    let rt = Runtime::new().expect("runtime");
    let listener = rt
        .block_on(TcpListener::bind("127.0.0.1:0"))
        .expect("bind loopback");
    let at = listener.local_addr().expect("the bound address");
    rt.spawn(serve_filtered(
        empty_router("tcpcap"),
        listener,
        allow_loopback,
    ));
    cap_holds(
        || connect_tcp(at),
        |s, d| s.set_read_timeout(d).expect("set a read timeout"),
    );
}

#[test]
fn a_unix_connection_past_the_cap_waits_until_a_held_one_closes() {
    let scratch = Scratch::new("unixcap");
    let path = scratch.at("arca.sock");
    let rt = Runtime::new().expect("runtime");
    let (router, served) = (empty_router("unixcap-r"), path.clone());
    rt.spawn(async move { serve_unix(router, &served).await });
    wait_for_path(&path);
    cap_holds(
        || connect_unix(&path),
        |s, d| s.set_read_timeout(d).expect("set a read timeout"),
    );
}

#[test]
fn the_real_peer_filter_gives_a_loopback_peer_no_bytes() {
    let rt = Runtime::new().expect("runtime");
    let listener = rt
        .block_on(TcpListener::bind("127.0.0.1:0"))
        .expect("bind loopback");
    let at = listener.local_addr().expect("the bound address");
    rt.spawn(serve_filtered(
        empty_router("real-filter"),
        listener,
        peer_allowed,
    ));

    let mut stream = connect_tcp(at);
    let _ = stream.write_all(&post_bytes(&[], &rpc(json!(1), "ping", json!({}))));
    let mut received = Vec::new();
    match stream.read_to_end(&mut received) {
        Ok(_) => {}
        Err(err) => assert!(
            !matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut),
            "the refused loopback peer must be closed, not left open: {err}"
        ),
    }
    assert!(
        received.is_empty(),
        "the real filter sends a loopback peer no bytes"
    );
}

// Proxy address: a fixed, unreserved port, never a wildcard and never a tailnet address.

#[test]
fn proxy_addr_ok_refuses_the_wildcards_tailnet_addresses_and_reserved_ports() {
    for text in [
        "0.0.0.0:7460",
        "[::]:7460",
        "[::ffff:0.0.0.0]:7460",
        "100.64.0.1:7460",
        "100.127.255.254:7460",
        "[::ffff:100.64.0.1]:7460",
        "[fd7a:115c:a1e0::1]:7460",
        "192.0.2.1:80",
        "192.0.2.1:443",
        "192.0.2.1:7443",
        "192.0.2.1:9443",
        "192.0.2.1:0",
    ] {
        assert!(
            proxy_addr_ok(&addr(text)).is_err(),
            "a proxy address must be refused: {text}"
        );
    }
}

#[test]
fn proxy_addr_ok_accepts_a_fixed_address_outside_the_tailnet() {
    for text in [
        "192.0.2.1:7460",
        "198.51.100.7:7460",
        "127.0.0.1:7460",
        "[2001:db8::1]:7460",
    ] {
        assert_eq!(proxy_addr_ok(&addr(text)), Ok(()), "accepted: {text}");
    }
}

#[test]
fn serve_proxy_refuses_a_refused_address_before_it_binds() {
    let rt = Runtime::new().expect("runtime");
    for text in [
        "0.0.0.0:7460",
        "[::]:7460",
        "100.64.0.1:7460",
        "127.0.0.1:0",
        "127.0.0.1:80",
        "127.0.0.1:443",
        "127.0.0.1:7443",
        "127.0.0.1:9443",
    ] {
        let err = rt
            .block_on(serve_proxy(
                empty_router("proxy-refuse"),
                addr(text),
                ip("127.0.0.2"),
            ))
            .expect_err("a refused address is never bound");
        assert_eq!(
            err.kind(),
            ErrorKind::InvalidInput,
            "refused before bind: {text}"
        );
    }
}

#[test]
fn serve_proxy_keeps_trying_an_address_that_is_not_assigned_yet() {
    // No host carries the documentation address 192.0.2.1, so its bind fails as a bridge address's does before the bridge is
    // up at boot. A host that binds addresses it does not carry cannot show this.
    let at = addr("192.0.2.1:7461");
    match std::net::TcpListener::bind(at) {
        Err(err) if err.kind() == ErrorKind::AddrNotAvailable => {}
        other => {
            eprintln!("skipped: this host binds an address it does not carry: {other:?}");
            return;
        }
    }
    let rt = Runtime::new().expect("runtime");
    let router = empty_router("proxy-unassigned");
    let outcome = rt.block_on(async {
        tokio::time::timeout(
            Duration::from_millis(1500),
            serve_proxy(router, at, ip("192.0.2.2")),
        )
        .await
    });
    assert!(
        outcome.is_err(),
        "the proxy listener is still waiting for its address: {outcome:?}"
    );
}

#[test]
fn serve_proxy_fails_at_once_on_an_address_already_in_use() {
    let held = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a port");
    let at = held.local_addr().expect("its address");
    let rt = Runtime::new().expect("runtime");
    let router = empty_router("proxy-in-use");
    let outcome = rt.block_on(async {
        tokio::time::timeout(
            Duration::from_secs(5),
            serve_proxy(router, at, ip("127.0.0.2")),
        )
        .await
    });
    let err = outcome
        .expect("the bind is not tried again")
        .expect_err("a port in use is never bound");
    assert_eq!(err.kind(), ErrorKind::AddrInUse);
}

/// A free loopback port: bound once, read, and released for the listener under test.
fn free_loopback_port() -> SocketAddr {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a probe");
    probe.local_addr().expect("the probe's address")
}

/// A TCP connection to `to` whose source address is `from`, with read and write deadlines. Loopback takes every address in
/// 127.0.0.0/8 as a source, so a test can stand for two peers on one host.
fn connect_from(rt: &Runtime, from: &str, to: SocketAddr) -> std::io::Result<std::net::TcpStream> {
    let stream = rt.block_on(async {
        let socket = tokio::net::TcpSocket::new_v4()?;
        socket.bind(addr(&format!("{from}:0")))?;
        socket.connect(to).await
    })?;
    let stream = stream.into_std()?;
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    Ok(stream)
}

/// Starts `serve_proxy` on a free loopback port for `peer`, and waits until it accepts a connection from `peer`.
fn start_proxy(rt: &Runtime, router: axum::Router, peer: &str) -> SocketAddr {
    let at = free_loopback_port();
    rt.spawn(serve_proxy(router, at, ip(peer)));
    let deadline = Instant::now() + Duration::from_secs(5);
    while connect_from(rt, peer, at).is_err() {
        assert!(
            Instant::now() < deadline,
            "the proxy listener never came up"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    at
}

#[test]
fn the_proxy_peer_gets_the_same_answers_as_the_unix_socket() {
    let scratch = Scratch::new("proxy-peer");
    let viewer = mint_into(scratch.path(), "proxy-viewer", Scope::Viewer);
    let tokens = TokenStore::load(scratch.path()).expect("loads");
    let router = Server::new(arca_api::verbs(), tokens).router();
    let rt = Runtime::new().expect("runtime");
    let at = start_proxy(&rt, router, "127.0.0.2");
    let initialize = rpc(
        json!(1),
        "initialize",
        json!({ "protocolVersion": "2025-11-25" }),
    );

    let mut stream = connect_from(&rt, "127.0.0.2", at).expect("the peer connects");
    let reply = post(&mut stream, &[], &initialize);
    assert_eq!(reply.status, 401, "no bearer is 401");

    let mut stream = connect_from(&rt, "127.0.0.2", at).expect("the peer connects");
    let reply = post(
        &mut stream,
        &[
            bearer(&viewer),
            ("Origin".into(), "https://example.com".into()),
        ],
        &initialize,
    );
    assert_eq!(reply.status, 403, "an Origin is 403");

    let mut stream = connect_from(&rt, "127.0.0.2", at).expect("the peer connects");
    let reply = post(&mut stream, &[bearer(&viewer)], &initialize);
    assert_eq!(reply.status, 200, "a viewer's initialize is 200");
    assert_eq!(reply.json()["result"]["protocolVersion"], "2025-11-25");
}

#[test]
fn a_proxy_peer_other_than_the_configured_one_is_closed_with_no_bytes() {
    let scratch = Scratch::new("proxy-other");
    let viewer = mint_into(scratch.path(), "proxy-viewer", Scope::Viewer);
    let tokens = TokenStore::load(scratch.path()).expect("loads");
    let router = Server::new(arca_api::verbs(), tokens).router();
    let rt = Runtime::new().expect("runtime");
    let at = start_proxy(&rt, router, "127.0.0.2");

    for other in ["127.0.0.1", "127.0.0.3"] {
        let mut stream = connect_from(&rt, other, at).expect("the TCP connect itself succeeds");
        let _ = stream.write_all(&post_bytes(
            &[bearer(&viewer)],
            &rpc(json!(1), "tools/list", json!({})),
        ));
        let mut received = Vec::new();
        match stream.read_to_end(&mut received) {
            Ok(_) => {}
            Err(err) => assert!(
                !matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut),
                "a peer other than the configured one must be closed, not left open: {err}"
            ),
        }
        assert!(
            received.is_empty(),
            "a peer other than the configured one receives no bytes: {other}"
        );
    }
}
