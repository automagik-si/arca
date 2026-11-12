//! Ontology queries through authenticated JSON-RPC on a real Unix socket, without host or Incus state.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arca::handlers::OntologyQuery;
use arca_api::{Scope, conforms, verb, verbs};
use arca_mcp::{Server, TokenStore, mint, serve_unix};
use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(15);

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Node {
    // Stop the server before removing its scratch directory.
    _runtime: tokio::runtime::Runtime,
    _scratch: Scratch,
    socket: PathBuf,
    secret: String,
}

impl Node {
    fn start() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let scratch =
            Scratch(std::env::temp_dir().join(format!("arca-oq-{}-{nanos}", std::process::id())));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&scratch.0)
            .expect("private scratch state");
        let tokens = scratch.0.join("tokens");
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&tokens)
            .expect("private token directory");
        let (secret, hash) = mint(Scope::Viewer);
        let hash_file = tokens.join("viewer.hash");
        fs::write(&hash_file, hash).expect("write token hash");
        fs::set_permissions(&hash_file, fs::Permissions::from_mode(0o400)).expect("private hash");
        let server = Server::new(verbs(), TokenStore::load(&tokens).expect("load viewer"))
            .handle("ontology_query", Arc::new(OntologyQuery));
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        let socket = scratch.0.join("arca.sock");
        let path = socket.clone();
        let serving = runtime.spawn(async move { serve_unix(server.router(), &path).await });
        let deadline = Instant::now() + WAIT;
        while UnixStream::connect(&socket).is_err() {
            assert!(!serving.is_finished(), "server must keep listening");
            assert!(Instant::now() < deadline, "server listens within deadline");
            thread::sleep(Duration::from_millis(10));
        }
        Self {
            _runtime: runtime,
            _scratch: scratch,
            socket,
            secret,
        }
    }

    fn post(&self, args: Value, authenticated: bool) -> (u16, String) {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "ontology_query", "arguments": args}})
        .to_string();
        let mut stream = UnixStream::connect(&self.socket).expect("connect");
        stream.set_read_timeout(Some(WAIT)).expect("deadline");
        stream
            .set_write_timeout(Some(WAIT))
            .expect("write deadline");
        let mut request = format!(
            "POST /mcp HTTP/1.1\r\nHost: arca\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        if authenticated {
            request.push_str(&format!("Authorization: Bearer {}\r\n", self.secret));
        }
        request.push_str("\r\n");
        request.push_str(&body);
        stream.write_all(request.as_bytes()).expect("send");
        let mut reply = String::new();
        stream.read_to_string(&mut reply).expect("read reply");
        let status = reply
            .split_whitespace()
            .nth(1)
            .expect("status")
            .parse()
            .expect("HTTP status");
        let body = reply
            .split_once("\r\n\r\n")
            .expect("HTTP body")
            .1
            .to_string();
        (status, body)
    }

    fn query(&self, id: &str) -> Value {
        let (status, body) = self.post(json!({"id": id}), true);
        assert_eq!(status, 200);
        let reply: Value = serde_json::from_str(&body).expect("JSON-RPC response");
        assert!(reply.get("error").is_none(), "{reply}");
        let result = reply["result"]["structuredContent"].clone();
        assert_eq!(
            conforms(&(verb("ontology_query").expect("verb").result)(), &result),
            Ok(())
        );
        assert_eq!(result["id"], id);
        result
    }
}

#[test]
fn viewer_reads_the_live_omni_edge() {
    let node = Node::start();
    let edges = json!([{
        "id": "omni-api", "to": "omni", "contract": "api", "status": "live"
    }]);
    let result = node.query("omni_instance_create");
    assert_eq!(result["owner"], "arca");
    assert_eq!(result["kind"], "tool");
    assert_eq!(result["status"], "live");
    assert_eq!(result["edges"], edges);
    for id in ["omni_pair", "omni_status"] {
        assert_eq!(node.query(id)["edges"], edges, "{id}");
    }
    let unrelated = node.query("service_logs");
    assert_eq!(unrelated["edges"], json!([]));
    assert_eq!(unrelated["neverOwns"], false);
}

#[test]
fn viewer_reads_planned_node_metrics() {
    assert_eq!(
        Node::start().query("node_metrics"),
        json!({
            "id": "node_metrics", "owner": "arca", "kind": "tool", "status": "planned", "neverOwns": false, "edges": []
        })
    );
}

#[test]
fn viewer_reads_live_service_create() {
    let result = Node::start().query("service_create");
    assert_eq!(result["owner"], "arca");
    assert_eq!(result["kind"], "tool");
    assert_eq!(result["status"], "live");
    assert_eq!(result["neverOwns"], false);
    assert_eq!(result["edges"], json!([]));
}

#[test]
fn viewer_reads_exact_never_owned_data() {
    assert_eq!(
        Node::start().query("renter-gate"),
        json!({
            "id": "renter-gate", "owner": null, "kind": "data", "status": null, "neverOwns": true, "edges": []
        })
    );
}

#[test]
fn viewer_reads_never_owned_prefix() {
    let result = Node::start().query("phone_example");
    assert_eq!(result["owner"], Value::Null);
    assert_eq!(result["kind"], "tool");
    assert_eq!(result["status"], Value::Null);
    assert_eq!(result["neverOwns"], true);
}

#[test]
fn viewer_reads_unknown_id() {
    assert_eq!(
        Node::start().query("unknown-ontology-item"),
        json!({
            "id": "unknown-ontology-item", "owner": null, "kind": null, "status": null, "neverOwns": false, "edges": []
        })
    );
}

#[test]
fn missing_token_is_unauthorized() {
    assert_eq!(
        Node::start().post(json!({"id": "node_metrics"}), false).0,
        401
    );
}

#[test]
fn invalid_arguments_are_refused_by_mcp() {
    let node = Node::start();
    for args in [
        json!({}),
        json!({"id": 1}),
        json!({"id": "x".repeat(129)}),
        json!({"id": "node_metrics", "extra": true}),
    ] {
        let (status, body) = node.post(args, true);
        assert_eq!(status, 200);
        let reply: Value = serde_json::from_str(&body).expect("JSON-RPC");
        assert_eq!(reply["result"]["isError"], true, "{reply}");
        assert_eq!(
            reply["result"]["_meta"]["droidd/error"],
            "invalid_arguments"
        );
    }
    node.query(&"é".repeat(128));
}
