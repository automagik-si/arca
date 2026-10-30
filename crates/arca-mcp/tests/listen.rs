//! The two listeners: the unix socket at mode 0600, the tailnet address and its peer filter. Only loopback and temporary
//! unix sockets are used. The tailnet ranges are checked as addresses; no test binds a tailnet address.

mod common;

use std::fs;
use std::io::{ErrorKind, Read};
use std::net::{IpAddr, SocketAddr};
use std::os::unix::fs::{FileTypeExt, PermissionsExt, symlink};
use std::os::unix::net::UnixListener;
use std::str::FromStr;

use arca_api::Scope;
use arca_mcp::{
    Server, TokenStore, peer_allowed, serve_filtered, serve_tailnet, serve_unix, tailnet_addr_ok,
};
use common::{Scratch, bearer, connect_tcp, connect_unix, mint_into, post, rpc, wait_for_socket};
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
    assert_eq!(names, vec!["arca_status", "node_metrics", "watch_report"]);
}

// The unix socket: mode 0600, refuses what it must, replaces only a stale socket.

#[test]
fn serve_unix_binds_a_socket_of_mode_0600_that_answers_401_without_a_bearer() {
    let scratch = Scratch::new("unixok");
    let path = scratch.at("arca.sock");
    let rt = Runtime::new().expect("runtime");
    let (router, served) = (empty_router("unixok-router"), path.clone());
    rt.spawn(async move { serve_unix(router, &served).await });
    wait_for_socket(&path);

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
