//! Helpers shared by the integration tests: a scratch directory under the system temporary directory, token files, and a
//! plain HTTP/1.1 client that speaks to a unix socket or a loopback address. No test opens any other address.
#![allow(dead_code)]

use std::fs::{self, Permissions};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arca_api::Scope;
use arca_mcp::mint;
use serde_json::{Value, json};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A fresh directory, removed when the test ends.
pub struct Scratch(PathBuf);

impl Scratch {
    pub fn new(tag: &str) -> Scratch {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("the clock is after 1970")
            .subsec_nanos();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("arca-mcp-{}-{nanos}-{n}-{tag}", std::process::id()));
        fs::create_dir_all(&dir).expect("create the scratch directory");
        Scratch(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn at(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The uid that owns a directory this test created, which is the uid the server's own files must carry.
pub fn own_uid(dir: &Path) -> u32 {
    fs::metadata(dir).expect("stat the scratch directory").uid()
}

/// Writes the file `name` in `dir` with `text` and the given mode, and returns its path.
pub fn write_named(dir: &Path, name: &str, text: &str, mode: u32) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, text).expect("write the file");
    fs::set_permissions(&path, Permissions::from_mode(mode)).expect("set the file's mode");
    path
}

/// Writes `<id>.hash` with `text` and the given mode, and returns its path.
pub fn write_hash_file(dir: &Path, id: &str, text: &str, mode: u32) -> PathBuf {
    write_named(dir, &format!("{id}.hash"), text, mode)
}

/// Mints a secret of `scope`, writes its hash file as `<id>.hash` with mode 0400, and returns the secret.
pub fn mint_into(dir: &Path, id: &str, scope: Scope) -> String {
    let (secret, text) = mint(scope);
    write_hash_file(dir, id, &text, 0o400);
    secret
}

/// An `Authorization` header carrying `secret` as a bearer token.
pub fn bearer(secret: &str) -> (String, String) {
    ("Authorization".to_string(), format!("Bearer {secret}"))
}

/// The bytes of a JSON-RPC request.
pub fn rpc(id: Value, method: &str, params: Value) -> Vec<u8> {
    serde_json::to_vec(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
        .expect("serialize the request")
}

/// A connected unix socket with a read deadline, so a server that never closes fails the test instead of hanging it.
pub fn connect_unix(path: &Path) -> UnixStream {
    let stream = UnixStream::connect(path).expect("connect to the test socket");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set a read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .expect("set a write timeout");
    stream
}

/// A connected loopback TCP stream with a read deadline.
pub fn connect_tcp(addr: SocketAddr) -> TcpStream {
    let stream = TcpStream::connect(addr).expect("connect to the test listener");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set a read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .expect("set a write timeout");
    stream
}

/// Waits until a server answers on `path`, which means it is bound and listening.
pub fn wait_for_socket(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while UnixStream::connect(path).is_err() {
        assert!(Instant::now() < deadline, "the test socket never came up");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// One HTTP response: the status, the headers in order, and the body as text.
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Reply {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).expect("a JSON body")
    }
}

/// Sends `POST /mcp` with `headers` and `body`, then reads the reply until the server closes.
pub fn post<S: Read + Write>(stream: &mut S, headers: &[(String, String)], body: &[u8]) -> Reply {
    let mut head = format!(
        "POST /mcp HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    stream
        .write_all(head.as_bytes())
        .expect("write the request head");
    // A server may answer and close before it reads a large body, so a failed body write is not a test failure here.
    let _ = stream.write_all(body);
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    parse(&raw)
}

fn parse(raw: &[u8]) -> Reply {
    let text = String::from_utf8_lossy(raw).into_owned();
    let split = text.find("\r\n\r\n").expect("a response head");
    let head = &text[..split];
    let body = text[split + 4..].to_string();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().expect("a status line");
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .expect("a status code");
    let headers = lines
        .filter_map(|l| {
            l.split_once(':')
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        })
        .collect();
    Reply {
        status,
        headers,
        body,
    }
}
