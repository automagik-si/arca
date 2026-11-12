//! The five listeners: the unix socket at mode 0600, the tailnet address and its peer filter, the proxy address and its
//! exact peer, the loopback address and its `Host` rule, and the services socket. Only loopback and temporary unix sockets are used. The tailnet ranges are checked as addresses; no test binds
//! a tailnet address.

mod common;

use std::fs::{self, Permissions};
use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, SocketAddr};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt, symlink};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::str::FromStr;
use std::time::{Duration, Instant};

use arca_api::Scope;
use arca_mcp::{
    Server, TokenStore, loopback_addr_ok, peer_allowed, proxy_addr_ok, serve_filtered,
    serve_loopback, serve_loopback_with, serve_proxy, serve_services, serve_tailnet, serve_unix,
    services_path_ok, tailnet_addr_ok,
};
use common::{
    Scratch, bearer, connect_tcp, connect_unix, exchange, mint_into, own_uid, post, post_bytes,
    rpc, wait_for_socket, write_hash_file,
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
            "action_propose",
            "alerts_list",
            "postgres_list",
            "ontology_query",
            "service_list",
            "service_status",
            "service_logs",
            "owner_approval_list",
            "storage_list",
            "snapshot_list",
            "tenant_list",
            "tenant_status",
            "tenant_usage",
            "function_list",
            "function_runs",
            "registry_list"
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
    let deadline = Instant::now() + Duration::from_secs(120);
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

// The services socket: a second unix socket, for the node's own services, that serves the MCP router alone.

// Loopback address: a fixed, unreserved port on a loopback address, and a request that names the listener.

#[test]
fn loopback_addr_ok_takes_a_loopback_address_on_a_fixed_free_port_and_nothing_else() {
    for text in ["127.0.0.1:7471", "127.0.0.2:7471", "[::1]:7471"] {
        assert_eq!(loopback_addr_ok(&addr(text)), Ok(()), "accepted: {text}");
    }
    for text in [
        "0.0.0.0:7471",
        "[::]:7471",
        "192.0.2.1:7471",
        "100.64.0.1:7471",
        "[fd7a:115c:a1e0::1]:7471",
        "[2001:db8::1]:7471",
        "[::ffff:127.0.0.1]:7471",
        "127.0.0.1:0",
        "127.0.0.1:80",
        "127.0.0.1:443",
        "127.0.0.1:7443",
        "127.0.0.1:9443",
    ] {
        assert!(
            loopback_addr_ok(&addr(text)).is_err(),
            "a loopback address must be refused: {text}"
        );
    }
}

#[test]
fn serve_loopback_refuses_a_refused_address_before_it_binds() {
    let rt = Runtime::new().expect("runtime");
    for text in ["0.0.0.0:7471", "192.0.2.1:7471", "127.0.0.1:443"] {
        let err = rt
            .block_on(serve_loopback(
                empty_router("loopback-refused"),
                addr(text),
                |_| panic!("a refused address is never tried"),
            ))
            .expect_err("a refused address is never bound");
        assert_eq!(err.kind(), ErrorKind::InvalidInput, "{text}");
    }
}

#[test]
fn a_loopback_port_another_program_holds_is_reported_and_tried_again_and_never_ends_the_listener() {
    // Any program of the machine can hold the port first. The listener's future must not end for that: in `serve` the
    // first listener to end stops them all, the unix socket among them.
    let held = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a port");
    let at = held.local_addr().expect("its address");
    let rt = Runtime::new().expect("runtime");
    let (tried, tries) = std::sync::mpsc::channel::<Option<ErrorKind>>();
    let serving = rt.spawn(serve_loopback(
        empty_router("loopback-held"),
        at,
        move |error| {
            let _ = tried.send(error.map(std::io::Error::kind));
        },
    ));
    assert_eq!(
        tries.recv_timeout(Duration::from_secs(5)),
        Ok(Some(ErrorKind::AddrInUse)),
        "the first try finds the port held, and says so"
    );
    assert_eq!(
        tries.recv_timeout(Duration::from_secs(5)),
        Ok(Some(ErrorKind::AddrInUse)),
        "it is tried again while it is held"
    );
    assert!(
        !serving.is_finished(),
        "a held port does not end the listener"
    );

    // The other program lets go: a later try takes the port, says so, and the door answers.
    drop(held);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match tries.recv_timeout(Duration::from_secs(20)) {
            Ok(None) => break,
            Ok(Some(_)) => assert!(
                Instant::now() < deadline,
                "the port is taken once it is free"
            ),
            Err(error) => panic!("the bind was not tried again: {error}"),
        }
    }
    let reply = raw(
        at,
        &[
            "POST /mcp HTTP/1.1".to_string(),
            format!("Host: {at}"),
            "Content-Type: application/json".to_string(),
        ],
        &rpc(json!(1), "initialize", json!({})),
    );
    assert_eq!(reply.status, 401, "the door is open and wants its token");
    assert!(!serving.is_finished());
}

#[test]
fn a_loopback_peer_that_sends_no_request_head_in_time_is_closed() {
    let rt = Runtime::new().expect("runtime");
    let at = free_loopback_port();
    rt.spawn(serve_loopback_with(
        empty_router("loopback-idle"),
        at,
        |_| {},
        Duration::from_millis(300),
    ));
    let deadline = Instant::now() + Duration::from_secs(120);
    while std::net::TcpStream::connect(at).is_err() {
        assert!(
            Instant::now() < deadline,
            "the loopback listener never came up"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // Whether the peer's end was closed: a read that ends, rather than one that is still waiting at its deadline.
    let closed = |stream: &mut std::net::TcpStream| {
        let mut rest = Vec::new();
        match stream.read_to_end(&mut rest) {
            Ok(_) => true,
            Err(err) => !matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut),
        }
    };

    // A peer that connects and says nothing, and one that starts a head and never ends it.
    let mut silent = connect_tcp(at);
    assert!(
        closed(&mut silent),
        "a silent peer is closed, not left holding a slot"
    );
    let mut slow = connect_tcp(at);
    slow.write_all(b"POST /mcp HTTP/1.1\r\nHost: ")
        .expect("send half a head");
    assert!(closed(&mut slow), "a head that never ends is closed");

    // A peer that was answered and then keeps the connection open and idle.
    let mut kept = connect_tcp(at);
    kept.write_all(format!("GET /no/such/route HTTP/1.1\r\nHost: {at}\r\n\r\n").as_bytes())
        .expect("send a request");
    let mut head = [0u8; 12];
    kept.read_exact(&mut head)
        .expect("the answer's first bytes");
    assert_eq!(&head, b"HTTP/1.1 404");
    assert!(closed(&mut kept), "an idle kept connection is closed too");

    // A peer that sends its head in time is served as before.
    let reply = raw(
        at,
        &[
            "POST /mcp HTTP/1.1".to_string(),
            format!("Host: {at}"),
            "Content-Type: application/json".to_string(),
        ],
        &rpc(json!(1), "initialize", json!({})),
    );
    assert_eq!(reply.status, 401);
}

/// Starts `serve_loopback` on a free loopback port and waits until it accepts a connection.
fn start_loopback(rt: &Runtime, router: axum::Router) -> SocketAddr {
    let at = free_loopback_port();
    rt.spawn(serve_loopback(router, at, |_| {}));
    let deadline = Instant::now() + Duration::from_secs(120);
    while std::net::TcpStream::connect(at).is_err() {
        assert!(
            Instant::now() < deadline,
            "the loopback listener never came up"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    at
}

/// One request, written as given, and the reply. `lines` are the request line and the headers; `body` follows them.
fn raw(at: SocketAddr, lines: &[String], body: &[u8]) -> common::Reply {
    let mut bytes = lines.join("\r\n").into_bytes();
    bytes.extend_from_slice(
        format!(
            "\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .as_bytes(),
    );
    bytes.extend_from_slice(body);
    exchange(&mut connect_tcp(at), &bytes)
}

/// The MCP router with one more route that needs no token, as an approval page's routes need none: what a browser page
/// that reached the listener could read without any credential.
fn router_with_an_open_route(tokens: TokenStore) -> axum::Router {
    Server::new(arca_api::verbs(), tokens).router().merge(
        axum::Router::new().route("/approve/plans", axum::routing::get(|| async { "cards" })),
    )
}

#[test]
fn the_loopback_listener_answers_its_own_two_names_as_the_unix_socket_does() {
    let scratch = Scratch::new("loopback-own");
    let viewer = mint_into(scratch.path(), "loopback-viewer", Scope::Viewer);
    let tokens = TokenStore::load(scratch.path()).expect("loads");
    let rt = Runtime::new().expect("runtime");
    let at = start_loopback(&rt, router_with_an_open_route(tokens));
    let port = at.port();
    let initialize = rpc(
        json!(1),
        "initialize",
        json!({ "protocolVersion": "2025-11-25" }),
    );
    let (name, value) = bearer(&viewer);
    let authorization = format!("{name}: {value}");

    for host in [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("LOCALHOST:{port}"),
    ] {
        let post = |more: &[String]| {
            let mut lines = vec![
                "POST /mcp HTTP/1.1".to_string(),
                format!("Host: {host}"),
                "Content-Type: application/json".to_string(),
            ];
            lines.extend_from_slice(more);
            raw(at, &lines, &initialize)
        };
        assert_eq!(post(&[]).status, 401, "{host}: no bearer is 401");
        assert_eq!(
            post(&[authorization.clone(), format!("Origin: http://{host}")]).status,
            403,
            "{host}: an Origin is 403 on an MCP route, its own name included"
        );
        let reply = post(std::slice::from_ref(&authorization));
        assert_eq!(reply.status, 200, "{host}: a viewer's initialize is 200");
        assert_eq!(reply.json()["result"]["protocolVersion"], "2025-11-25");

        let open = raw(
            at,
            &[
                "GET /approve/plans HTTP/1.1".to_string(),
                format!("Host: {host}"),
            ],
            b"",
        );
        assert_eq!(open.status, 200, "{host}: the open route answers");
        assert_eq!(open.body, "cards");
    }
}

#[test]
fn a_request_that_names_another_host_is_refused_on_every_route_of_the_loopback_listener() {
    let scratch = Scratch::new("loopback-rebound");
    let viewer = mint_into(scratch.path(), "loopback-viewer", Scope::Viewer);
    let tokens = TokenStore::load(scratch.path()).expect("loads");
    let rt = Runtime::new().expect("runtime");
    let at = start_loopback(&rt, router_with_an_open_route(tokens));
    let port = at.port();
    let initialize = rpc(json!(1), "initialize", json!({}));
    let (name, value) = bearer(&viewer);
    let authorization = format!("{name}: {value}");
    let own = format!("Host: 127.0.0.1:{port}");

    // DNS rebinding: a page a browser loaded from a foreign name, which was then pointed at 127.0.0.1, is same-origin
    // with this listener as far as the browser can tell. Its requests carry the foreign name as their Host. The POST
    // is refused although it carries a valid token and no Origin; the GET is refused although the route needs no token
    // and a same-origin GET carries no Origin.
    let rebound = format!("Host: rebound.example:{port}");
    let post = raw(
        at,
        &[
            "POST /mcp HTTP/1.1".to_string(),
            rebound.clone(),
            "Content-Type: application/json".to_string(),
            authorization.clone(),
        ],
        &initialize,
    );
    assert_eq!(post.status, 403, "a rebound POST is refused");
    assert_eq!(post.body, "", "and told nothing");
    let get = raw(
        at,
        &["GET /approve/plans HTTP/1.1".to_string(), rebound.clone()],
        b"",
    );
    assert_eq!(get.status, 403, "a rebound GET is refused");
    assert_eq!(get.body, "", "and reads nothing");
    let missing = raw(
        at,
        &["GET /no/such/route HTTP/1.1".to_string(), rebound.clone()],
        b"",
    );
    assert_eq!(
        missing.status, 403,
        "a route that is not there says so to nobody else"
    );

    // The refusal comes before the body is read: a foreign Host with a body announced and never sent, larger than any
    // body the door takes, is answered at once. A check that ran after the body was read would wait for it instead.
    for extra in ["", "Origin: http://rebound.example\r\n"] {
        let unsent = format!(
            "POST /mcp HTTP/1.1\r\nHost: rebound.example:{port}\r\nContent-Type: application/json\r\n{extra}Content-Length: 1048576\r\n\r\n"
        );
        let started = Instant::now();
        let reply = exchange(&mut connect_tcp(at), unsent.as_bytes());
        assert_eq!(
            reply.status, 403,
            "a foreign Host is refused with its body unread"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "the refusal did not wait for the body"
        );
    }

    // Every other way of not naming this listener.
    let refused: Vec<(&str, Vec<String>)> = vec![
        (
            "the right address on another port",
            vec![
                "GET /approve/plans HTTP/1.1".into(),
                format!("Host: 127.0.0.1:{}", port.wrapping_add(1).max(1)),
            ],
        ),
        (
            "localhost with no port",
            vec![
                "GET /approve/plans HTTP/1.1".into(),
                "Host: localhost".into(),
            ],
        ),
        (
            "a name that only starts like this listener's",
            vec![
                "GET /approve/plans HTTP/1.1".into(),
                format!("Host: localhost:{port}.rebound.example"),
            ],
        ),
        (
            "an empty Host",
            vec!["GET /approve/plans HTTP/1.1".into(), "Host:".into()],
        ),
        ("no Host at all", vec!["GET /approve/plans HTTP/1.0".into()]),
        (
            "a second Host beside the right one",
            vec![
                "GET /approve/plans HTTP/1.1".into(),
                own.clone(),
                rebound.clone(),
            ],
        ),
        (
            "a target that names another host beside the right Host",
            vec![
                format!("GET http://rebound.example:{port}/approve/plans HTTP/1.1"),
                own.clone(),
            ],
        ),
    ];
    for (what, lines) in refused {
        let reply = raw(at, &lines, b"");
        assert!(
            reply.status == 403 || reply.status == 400,
            "{what}: {} is not a refusal",
            reply.status
        );
        assert_ne!(reply.body, "cards", "{what}: the open route was read");
    }

    // The same listener still answers its own name afterwards.
    let reply = raw(at, &["GET /approve/plans HTTP/1.1".to_string(), own], b"");
    assert_eq!(reply.status, 200);
}

#[test]
fn services_path_ok_takes_a_plain_absolute_file_path_and_nothing_else() {
    for text in [
        "/run/arca/services.sock",
        "/run/user/1000/arca/services.sock",
        "/a",
    ] {
        assert_eq!(
            services_path_ok(Path::new(text)),
            Ok(()),
            "accepted: {text}"
        );
    }
    let long = format!("/run/{}", "a".repeat(100));
    for text in [
        "",
        "services.sock",
        "run/arca/services.sock",
        "/",
        "/run/arca/",
        "/run//services.sock",
        "/run/../services.sock",
        "/run/./services.sock",
        "/run/arca/services sock",
        "/run/arca/services.sock,mode=0666",
        "/run/arca/services.sock\n",
        "/run/arca/serviços.sock",
        "127.0.0.1:7460",
        "tcp:127.0.0.1:7460",
        long.as_str(),
    ] {
        assert!(
            services_path_ok(Path::new(text)).is_err(),
            "a services socket path must be refused: {text:?}"
        );
    }
}

#[test]
fn serve_services_refuses_a_path_or_a_directory_another_user_could_take() {
    let rt = Runtime::new().expect("runtime");
    // A path that fails the path rules is refused before anything is bound.
    let err = rt
        .block_on(serve_services(
            empty_router("services-path"),
            Path::new("services.sock"),
        ))
        .expect_err("a relative path is never bound");
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
    // A directory that a group or others may write to: another user could put a socket of its own at the path while
    // the server is down, and be sent a service's token.
    for mode in [0o770, 0o707, 0o777] {
        let scratch = Scratch::new("services-open");
        fs::set_permissions(scratch.path(), Permissions::from_mode(mode))
            .expect("open the directory");
        let path = scratch.at("services.sock");
        let err = rt
            .block_on(serve_services(empty_router("services-open-router"), &path))
            .expect_err("a directory others may write to is refused");
        assert_eq!(err.kind(), ErrorKind::PermissionDenied, "{mode:o}");
        assert!(fs::symlink_metadata(&path).is_err(), "nothing was bound");
        fs::set_permissions(scratch.path(), Permissions::from_mode(0o700)).expect("close it");
    }
    // A directory above the socket's own that a group or others may write to: whoever can rename the socket's
    // directory can put another in its place. Reaching the directory through a symlink changes nothing.
    let scratch = Scratch::new("services-above");
    let loose = scratch.at("loose");
    let inner = loose.join("inner");
    fs::create_dir_all(&inner).expect("the directories");
    fs::set_permissions(&inner, Permissions::from_mode(0o700)).expect("the socket's own directory");
    fs::set_permissions(&loose, Permissions::from_mode(0o777)).expect("open the one above");
    symlink(&inner, scratch.at("link")).expect("a symlink to the socket's directory");
    for path in [
        inner.join("services.sock"),
        scratch.at("link/services.sock"),
    ] {
        let err = rt
            .block_on(serve_services(empty_router("services-above-router"), &path))
            .expect_err("a directory above that others may write to is refused");
        assert_eq!(
            err.kind(),
            ErrorKind::PermissionDenied,
            "{}",
            path.display()
        );
        assert!(err.to_string().contains("loose"), "{err}");
        assert!(fs::symlink_metadata(inner.join("services.sock")).is_err());
    }
    // With the sticky bit, as /tmp has, nobody but its owner can rename what is in it: the socket is served.
    fs::set_permissions(&loose, Permissions::from_mode(0o1777)).expect("make it sticky");
    let path = inner.join("services.sock");
    let (router, served) = (empty_router("services-sticky-router"), path.clone());
    rt.spawn(async move { serve_services(router, &served).await });
    wait_for_path(&path);
    let reply = post(
        &mut connect_unix(&path),
        &[],
        &rpc(json!(1), "ping", json!({})),
    );
    assert_eq!(reply.status, 401);
    fs::set_permissions(&loose, Permissions::from_mode(0o700)).expect("close it");
}

/// The socket a service with kernel access reaches the kernel through, with the MCP router alone. It is mode 0600 in
/// the server's own directory, so no other user of the host connects to it. It gives its peer no authority: a call
/// without a token is 401, a token with a tool list reaches its tools and nothing else, and the approval page's paths
/// are not there.
#[test]
fn the_services_socket_gives_its_peer_only_what_the_token_gives_and_serves_no_approval_page() {
    let scratch = Scratch::new("services");
    let tools = vec!["plan_list".to_string(), "action_propose".to_string()];
    let (listed, text) = arca_mcp::mint_with_tools(Scope::Doctor, Some(&tools));
    let hashes = scratch.at("tokens");
    fs::create_dir(&hashes).expect("the token directory");
    fs::set_permissions(&hashes, Permissions::from_mode(0o700)).expect("make it private");
    write_hash_file(&hashes, "service", &text, 0o400);
    let tokens = TokenStore::load(&hashes).expect("loads");
    let router = Server::new(arca_api::verbs(), tokens).router();
    let rt = Runtime::new().expect("runtime");
    let path = scratch.at("services.sock");
    let served = path.clone();
    rt.spawn(async move { serve_services(router, &served).await });
    wait_for_path(&path);
    let meta = fs::symlink_metadata(&path).expect("stat the socket");
    assert!(meta.file_type().is_socket(), "the path is a socket");
    assert_eq!(
        meta.permissions().mode() & 0o777,
        0o600,
        "the socket is 0600"
    );
    assert_eq!(meta.uid(), own_uid(scratch.path()));
    // The staging directory is removed right after the rename, so the listing settles a moment after the path appears.
    let deadline = Instant::now() + Duration::from_secs(5);
    while names_in(scratch.path()) != ["services.sock", "tokens"] {
        assert!(
            Instant::now() < deadline,
            "a staging entry was left beside the socket: {:?}",
            names_in(scratch.path())
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let call = |name: &str| {
        rpc(
            json!(1),
            "tools/call",
            json!({ "name": name, "arguments": {} }),
        )
    };

    // No token: 401 on the one POST route, whatever the call. Nothing is answered about the node.
    for body in [
        rpc(
            json!(1),
            "initialize",
            json!({ "protocolVersion": "2025-11-25" }),
        ),
        rpc(json!(1), "tools/list", json!({})),
        call("plan_list"),
        call("service_remove"),
    ] {
        let reply = post(&mut connect_unix(&path), &[], &body);
        assert_eq!(reply.status, 401, "no bearer is 401");
        assert!(reply.body.is_empty(), "a 401 carries no body");
    }
    // A token that is not one of the node's is no token.
    let stranger = format!("arca_{}", "A".repeat(43));
    let reply = post(
        &mut connect_unix(&path),
        &[bearer(&stranger)],
        &call("plan_list"),
    );
    assert_eq!(reply.status, 401);

    // The approval page's paths are not on this socket, with a token or without one: 404, and no page. A request
    // that carries an Origin, as the page's own POSTs do, is 403 before its path is looked at.
    for (method, target) in [
        ("GET", "/approve"),
        ("GET", "/approve/plans"),
        ("GET", "/approve/passkeys"),
        ("GET", "/approve/canvas"),
        ("POST", "/approve/decline"),
        ("POST", "/approve/approve"),
        ("POST", "/approve/setup-code"),
    ] {
        for headers in [vec![], vec![bearer(&listed)]] {
            let mut request = format!(
                "{method} {target} HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\nContent-Type: application/json\r\nConnection: close\r\n"
            );
            for (name, value) in &headers {
                request.push_str(&format!("{name}: {value}\r\n"));
            }
            request.push_str("\r\n{}");
            let reply = exchange(&mut connect_unix(&path), request.as_bytes());
            assert_eq!(reply.status, 404, "{method} {target}");
            assert!(reply.body.is_empty(), "{method} {target} answers no page");
        }
        let request = format!(
            "{method} {target} HTTP/1.1\r\nHost: localhost\r\nOrigin: https://arca.example.com\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        let reply = exchange(&mut connect_unix(&path), request.as_bytes());
        assert_eq!(reply.status, 403, "{method} {target} with an Origin");
    }
    // So is a call to /mcp itself that carries an Origin, with a valid token.
    let reply = post(
        &mut connect_unix(&path),
        &[
            bearer(&listed),
            ("Origin".into(), "https://arca.example.com".into()),
        ],
        &call("plan_list"),
    );
    assert_eq!(reply.status, 403, "an Origin is 403");

    // The service's token: a doctor token with a tool list. It is offered its tools alone.
    let reply = post(
        &mut connect_unix(&path),
        &[bearer(&listed)],
        &rpc(json!(2), "tools/list", json!({})),
    );
    assert_eq!(reply.status, 200);
    let names: Vec<String> = reply.json()["result"]["tools"]
        .as_array()
        .expect("a tool list")
        .iter()
        .map(|tool| tool["name"].as_str().expect("a name").to_string())
        .collect();
    assert_eq!(names, vec!["plan_list", "action_propose"]);
    // A verb its scope would admit, outside its list, is refused before any handler runs.
    for outside in ["service_create", "service_remove", "plan_decline"] {
        let reply = post(&mut connect_unix(&path), &[bearer(&listed)], &call(outside));
        assert_eq!(reply.status, 403, "{outside} is outside the token's list");
    }
}
