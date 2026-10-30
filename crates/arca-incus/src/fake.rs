//! A fake Incus for tests. It holds the objects of a [`FakeState`] in memory and serves them on a unix socket. It records every
//! request it receives.
//!
//! GET routes: `/1.0`, `/1.0/projects?recursion=1`, `/1.0/instances?all-projects=true&recursion=1`,
//! `/1.0/storage-pools?recursion=1`, `/1.0/storage-pools/<pool>/resources`, `/1.0/networks?recursion=1` and
//! `/1.0/networks/<name>/state` (the host's interfaces, set by [`FakeIncus::set_host_interfaces`]), every object path, and
//! `/1.0/operations/<id>` with its `/wait`. Any other GET is 404.
//!
//! Writes: a POST to a collection creates an object, a PATCH or DELETE changes or removes one. These cover projects and the
//! eight object kinds (instances, networks, ACLs, profiles, volumes, zones, forwards and buckets). An instance and a volume
//! get a uuid, in their `volatile.uuid` key; no other kind has one. Each object's ETag is a digest of its JSON. A PATCH whose
//! `If-Match` is not the current ETag is 412 and changes nothing. A DELETE ignores `If-Match`, as Incus does. An instance's or
//! a volume's create and delete are answered 202 with an operation, whose URL the caller waits on. Every other write, and
//! every PATCH, is answered sync: 201 for a create and 200 otherwise. A failed operation waits as a status error. A method a
//! route does not take is 405 with an `Allow` header.
//!
//! The fake records each request, and each write with the names of the config keys it carried. It never records a value.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::net::IpAddr;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use arca_kernel::canon::{Digest, canonical_json};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::shim::{IO_TIMEOUT, drain, head_end, parse_request_line, respond};

/// The project a request names when its query has none, as Incus does.
const DEFAULT_PROJECT: &str = "default";
/// The largest request body the fake reads. A larger one is answered 400.
const MAX_REQUEST_BODY: usize = 1024 * 1024;
/// The largest request head the fake reads.
const MAX_REQUEST_HEAD: usize = 64 * 1024;

/// The metadata the fake serves. The pools are `(name, resources)` pairs.
#[derive(Debug, Clone)]
pub struct FakeState {
    pub server: Value,
    pub projects: Vec<Value>,
    pub instances: Vec<Value>,
    pub pools: Vec<(String, Value)>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StateFile {
    server: Value,
    projects: Vec<Value>,
    instances: Vec<Value>,
    pools: Vec<PoolFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PoolFile {
    name: String,
    resources: Value,
}

impl FakeState {
    /// Reads a state file: `server`, `projects`, `instances`, and `pools` as `{name, resources}` objects.
    pub fn load(path: &Path) -> io::Result<FakeState> {
        let bytes = fs::read(path)?;
        let file: StateFile = serde_json::from_slice(&bytes).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "the fake state is not in the expected shape",
            )
        })?;
        Ok(FakeState {
            server: file.server,
            projects: file.projects,
            instances: file.instances,
            pools: file
                .pools
                .into_iter()
                .map(|pool| (pool.name, pool.resources))
                .collect(),
        })
    }
}

/// One request the fake received: its method and target as sent, and the status it answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    pub status: u16,
}

/// One write the fake received: its method, its target as sent, the status it answered and the names of the config keys it
/// carried. A value is never recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteRecord {
    pub method: String,
    pub path: String,
    pub status: u16,
    pub config_keys: Vec<String>,
}

/// One host interface: its name and the addresses it holds.
type HostInterface = (String, Vec<IpAddr>);

/// A running fake Incus. It serves until it is dropped.
pub struct FakeIncus {
    socket: PathBuf,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}

/// What every connection shares: the objects, the logs and the switches.
struct Shared {
    store: Mutex<Store>,
    requests: Mutex<Vec<RecordedRequest>>,
    writes: Mutex<Vec<WriteRecord>>,
    reject_config: Mutex<Vec<String>>,
    host_interfaces: Mutex<Vec<HostInterface>>,
    counter: AtomicU64,
}

/// The served state: the server document, the pools, every object and every operation.
struct Store {
    server: Value,
    pools: Vec<(String, Value)>,
    objects: Vec<Stored>,
    operations: BTreeMap<String, Value>,
}

/// One object: its kind, its project, its name, its parent (the storage pool for a volume or a bucket, the network for a
/// forward, empty otherwise) and its metadata. A project's project is its own name.
#[derive(Debug, Clone)]
struct Stored {
    kind: String,
    project: String,
    name: String,
    parent: String,
    body: Value,
}

/// An object address as a route names it.
#[derive(Debug, Clone)]
struct Address {
    kind: &'static str,
    project: String,
    parent: String,
    name: String,
}

enum Route {
    Server,
    ListProjects,
    ListInstances,
    ListPools,
    HostInterfaces,
    HostInterfaceState(String),
    PoolResources(String),
    Operation(String),
    Collection(Address),
    Object(Address),
}

impl Route {
    fn methods(&self) -> &'static [&'static str] {
        match self {
            Route::Collection(_) => &["POST"],
            Route::Object(_) => &["GET", "PATCH", "DELETE"],
            _ => &["GET"],
        }
    }
}

impl Shared {
    fn new(state: FakeState) -> Shared {
        let mut objects = Vec::new();
        for body in state.projects {
            let name = text_of(&body, "name");
            objects.push(Stored {
                kind: "project".to_string(),
                project: name.clone(),
                name,
                parent: String::new(),
                body,
            });
        }
        for body in state.instances {
            let project = text_of(&body, "project");
            let name = text_of(&body, "name");
            objects.push(Stored {
                kind: "instance".to_string(),
                project,
                name,
                parent: String::new(),
                body,
            });
        }
        Shared {
            store: Mutex::new(Store {
                server: state.server,
                pools: state.pools,
                objects,
                operations: BTreeMap::new(),
            }),
            requests: Mutex::new(Vec::new()),
            writes: Mutex::new(Vec::new()),
            reject_config: Mutex::new(Vec::new()),
            host_interfaces: Mutex::new(Vec::new()),
            counter: AtomicU64::new(1),
        }
    }

    /// A fresh id in the shape of a uuid, used for an operation and for an object's uuid.
    fn next_id(&self) -> String {
        let n = self.counter.fetch_add(1, Ordering::SeqCst);
        format!("00000000-0000-4000-8000-{n:012x}")
    }
}

impl FakeIncus {
    /// Binds `incus.sock` under `dir` and serves `state` there.
    pub fn start(dir: &Path, state: FakeState) -> io::Result<FakeIncus> {
        let socket = dir.join("incus.sock");
        let listener = UnixListener::bind(&socket)?;
        listener.set_nonblocking(true)?;
        let shared = Arc::new(Shared::new(state));
        let stop = Arc::new(AtomicBool::new(false));
        let accept = {
            let shared = Arc::clone(&shared);
            let stop = Arc::clone(&stop);
            thread::spawn(move || accept_loop(&listener, &shared, &stop))
        };
        Ok(FakeIncus {
            socket,
            shared,
            stop,
            accept: Some(accept),
        })
    }

    /// The socket the fake serves on.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Every request received so far, in arrival order.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        lock(&self.shared.requests).clone()
    }

    /// Every write received so far, in arrival order: each non-GET request, with the config keys it carried.
    pub fn writes(&self) -> Vec<WriteRecord> {
        lock(&self.shared.writes).clone()
    }

    /// Sets the host's own interfaces: each name with the addresses it holds. The fake answers the listing of
    /// `/1.0/networks?recursion=1` and each `/1.0/networks/<name>/state` from them, and nothing else reads them.
    pub fn set_host_interfaces(&self, interfaces: Vec<(String, Vec<IpAddr>)>) {
        *lock(&self.shared.host_interfaces) = interfaces;
    }

    /// Makes the fake answer 400 to a POST whose path starts with `kind_path_prefix` and whose body carries a `config`. The
    /// object is not created. A switch for the live probe's rejection case.
    pub fn set_reject_config_on_post(&self, kind_path_prefix: &str) {
        lock(&self.shared.reject_config).push(kind_path_prefix.to_string());
    }
}

impl Drop for FakeIncus {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn accept_loop(listener: &UnixListener, shared: &Arc<Shared>, stop: &AtomicBool) {
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let shared = Arc::clone(shared);
                thread::spawn(move || serve(stream, &shared));
            }
            Err(_) => thread::sleep(Duration::from_millis(5)),
        }
    }
}

/// One request as the fake reads it. A head that does not parse has an empty method and is answered 400.
struct Request {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn bad() -> Request {
        Request {
            method: String::new(),
            target: String::new(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// The target without its query string.
    fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or("")
    }
}

/// One connection: read the request, route it, record it, answer it.
fn serve(mut stream: UnixStream, shared: &Shared) {
    if stream.set_nonblocking(false).is_err()
        || stream.set_read_timeout(Some(IO_TIMEOUT)).is_err()
        || stream.set_write_timeout(Some(IO_TIMEOUT)).is_err()
    {
        return;
    }
    let request = match read_request(&mut stream) {
        Ok(Some(request)) => request,
        Ok(None) => return,
        Err(_) => Request::bad(),
    };
    let (status, extra, envelope) = handle(shared, &request);
    record(shared, &request, status);
    let _ = respond(
        &mut stream,
        status,
        reason(status),
        &extra,
        &serde_json::to_vec(&envelope).unwrap_or_default(),
    );
    drain(&mut stream);
}

/// Reads one request: its head through the blank line, then a body of `Content-Length` bytes. A chunked request is refused.
fn read_request(stream: &mut UnixStream) -> io::Result<Option<Request>> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_len = loop {
        if let Some(end) = head_end(&buffer) {
            break end;
        }
        if buffer.len() >= MAX_REQUEST_HEAD {
            return Err(invalid("the request head is too large"));
        }
        match stream.read(&mut chunk) {
            Ok(0) if buffer.is_empty() => return Ok(None),
            Ok(0) => return Err(invalid("the request ended before its head")),
            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) && buffer.is_empty() =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        }
    };
    let head =
        std::str::from_utf8(&buffer[..head_len]).map_err(|_| invalid("the head is not text"))?;
    let mut lines = head
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line));
    let (method, target) =
        parse_request_line(lines.next().unwrap_or("")).ok_or_else(|| invalid("no request line"))?;
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| invalid("a header without a colon"))?;
        headers.push((name.to_ascii_lowercase(), value.trim().to_string()));
    }
    if headers.iter().any(|(name, _)| name == "transfer-encoding") {
        return Err(invalid("a chunked request is not read"));
    }
    let length = match headers.iter().find(|(name, _)| name == "content-length") {
        Some((_, value)) => value
            .parse::<usize>()
            .map_err(|_| invalid("a content length that is not a number"))?,
        None => 0,
    };
    if length > MAX_REQUEST_BODY {
        return Err(invalid("the request body is too large"));
    }
    let mut body = buffer[head_len..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Err(invalid("the request ended before its body"));
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(length);
    Ok(Some(Request {
        method: method.to_string(),
        target: target.to_string(),
        headers,
        body,
    }))
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

fn record(shared: &Shared, request: &Request, status: u16) {
    lock(&shared.requests).push(RecordedRequest {
        method: request.method.clone(),
        path: request.target.clone(),
        status,
    });
    if !request.method.is_empty() && request.method != "GET" {
        lock(&shared.writes).push(WriteRecord {
            method: request.method.clone(),
            path: request.target.clone(),
            status,
            config_keys: config_keys(&request.body),
        });
    }
}

/// The names of the keys in a body's `config` object. Values never leave this function.
fn config_keys(body: &[u8]) -> Vec<String> {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("config")
                .and_then(Value::as_object)
                .map(|config| config.keys().cloned().collect())
        })
        .unwrap_or_default()
}

/// The status, the extra header lines and the envelope for one request.
fn handle(shared: &Shared, request: &Request) -> (u16, String, Value) {
    if request.method.is_empty() {
        return (400, String::new(), error_envelope(400, "bad request"));
    }
    let route = parse(&request.target);
    let allowed: &[&str] = route.as_ref().map_or(&["GET"][..], Route::methods);
    let method = request.method.as_str();
    match route {
        Some(route) if allowed.contains(&method) => dispatch(shared, request, route),
        _ if method == "GET" => (404, String::new(), error_envelope(404, "not found")),
        _ => (
            405,
            format!("Allow: {}\r\n", allowed.join(", ")),
            error_envelope(405, "method not allowed"),
        ),
    }
}

/// The route for a target, or `None` when no route serves it.
fn parse(target: &str) -> Option<Route> {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let rest = path.strip_prefix("/1.0")?;
    let segments: Vec<&str> = if rest.is_empty() {
        Vec::new()
    } else {
        rest.strip_prefix('/')?.split('/').collect()
    };
    let project = query_param(query, "project").unwrap_or(DEFAULT_PROJECT);
    let address = |kind: &'static str, parent: &str, name: &str| Address {
        kind,
        project: project.to_string(),
        parent: parent.to_string(),
        name: name.to_string(),
    };
    let route = match segments.as_slice() {
        [] if query.is_empty() => Route::Server,
        ["projects"] if query == "recursion=1" => Route::ListProjects,
        ["projects"] => Route::Collection(Address {
            kind: "project",
            project: String::new(),
            parent: String::new(),
            name: String::new(),
        }),
        ["projects", name] => Route::Object(Address {
            kind: "project",
            project: (*name).to_string(),
            parent: String::new(),
            name: (*name).to_string(),
        }),
        ["instances"] if query == "all-projects=true&recursion=1" => Route::ListInstances,
        ["instances"] => Route::Collection(address("instance", "", "")),
        ["instances", name] => Route::Object(address("instance", "", name)),
        ["networks"] if query == "recursion=1" => Route::HostInterfaces,
        ["networks"] => Route::Collection(address("network", "", "")),
        ["networks", name, "state"] if query.is_empty() => {
            Route::HostInterfaceState((*name).to_string())
        }
        ["networks", name] => Route::Object(address("network", "", name)),
        ["networks", network, "forwards"] => Route::Collection(address("forward", network, "")),
        ["networks", network, "forwards", name] => Route::Object(address("forward", network, name)),
        ["network-acls"] => Route::Collection(address("acl", "", "")),
        ["network-acls", name] => Route::Object(address("acl", "", name)),
        ["profiles"] => Route::Collection(address("profile", "", "")),
        ["profiles", name] => Route::Object(address("profile", "", name)),
        ["network-zones"] => Route::Collection(address("zone", "", "")),
        ["network-zones", name] => Route::Object(address("zone", "", name)),
        ["storage-pools"] if query == "recursion=1" => Route::ListPools,
        ["storage-pools", pool, "resources"] if query.is_empty() => {
            Route::PoolResources((*pool).to_string())
        }
        ["storage-pools", pool, "volumes", "custom"] => {
            Route::Collection(address("volume", pool, ""))
        }
        ["storage-pools", pool, "volumes", "custom", name] => {
            Route::Object(address("volume", pool, name))
        }
        ["storage-pools", pool, "buckets"] => Route::Collection(address("bucket", pool, "")),
        ["storage-pools", pool, "buckets", name] => Route::Object(address("bucket", pool, name)),
        ["operations", id] | ["operations", id, "wait"] => Route::Operation((*id).to_string()),
        _ => return None,
    };
    Some(route)
}

/// The value of `key` in a query string, when it is present.
fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix(key)?.strip_prefix('='))
}

fn dispatch(shared: &Shared, request: &Request, route: Route) -> (u16, String, Value) {
    let mut guard = lock(&shared.store);
    let store: &mut Store = &mut guard;
    match (request.method.as_str(), route) {
        ("GET", Route::Server) => sync(store.server.clone()),
        ("GET", Route::ListProjects) => sync(Value::Array(store.bodies("project"))),
        ("GET", Route::ListInstances) => sync(Value::Array(store.bodies("instance"))),
        ("GET", Route::HostInterfaces) => {
            let interfaces = lock(&shared.host_interfaces);
            sync(Value::Array(
                interfaces
                    .iter()
                    .map(|(name, _)| json!({ "name": name, "managed": false, "type": "broadcast" }))
                    .collect(),
            ))
        }
        ("GET", Route::HostInterfaceState(name)) => {
            let interfaces = lock(&shared.host_interfaces);
            match interfaces.iter().find(|(listed, _)| *listed == name) {
                Some((_, addresses)) => sync(json!({
                    "addresses": addresses
                        .iter()
                        .map(|address| json!({
                            "family": if address.is_ipv4() { "inet" } else { "inet6" },
                            "address": address.to_string(),
                        }))
                        .collect::<Vec<Value>>(),
                })),
                None => (404, String::new(), error_envelope(404, "not found")),
            }
        }
        ("GET", Route::ListPools) => sync(Value::Array(
            store
                .pools
                .iter()
                .map(|(name, _)| json!({ "name": name }))
                .collect(),
        )),
        ("GET", Route::PoolResources(pool)) => {
            match store.pools.iter().find(|(name, _)| *name == pool) {
                Some((_, resources)) => sync(resources.clone()),
                None => (404, String::new(), error_envelope(404, "not found")),
            }
        }
        ("GET", Route::Operation(id)) => match store.operations.get(&id) {
            Some(operation) => sync(operation.clone()),
            None => (404, String::new(), error_envelope(404, "not found")),
        },
        ("GET", Route::Object(address)) => match store.find(&address) {
            Some(index) => {
                let body = store.objects[index].body.clone();
                let extra = format!("ETag: {}\r\n", etag_of(&body));
                (200, extra, sync_envelope(body))
            }
            None => (404, String::new(), error_envelope(404, "not found")),
        },
        ("POST", Route::Collection(address)) => create(shared, store, request, &address),
        ("PATCH", Route::Object(address)) => patch(shared, store, request, &address),
        ("DELETE", Route::Object(address)) => delete(shared, store, &address),
        _ => (
            405,
            String::new(),
            error_envelope(405, "method not allowed"),
        ),
    }
}

impl Store {
    fn find(&self, address: &Address) -> Option<usize> {
        self.objects.iter().position(|object| {
            object.kind == address.kind
                && object.project == address.project
                && object.parent == address.parent
                && object.name == address.name
        })
    }

    fn bodies(&self, kind: &str) -> Vec<Value> {
        self.objects
            .iter()
            .filter(|object| object.kind == kind)
            .map(|object| object.body.clone())
            .collect()
    }
}

/// Creates an object from a POST. A `config` is refused when the switch names this path.
fn create(
    shared: &Shared,
    store: &mut Store,
    request: &Request,
    address: &Address,
) -> (u16, String, Value) {
    let Ok(Value::Object(mut body)) = serde_json::from_slice::<Value>(&request.body) else {
        return (
            400,
            String::new(),
            error_envelope(400, "the body is not a JSON object"),
        );
    };
    let path = request.path();
    let rejected = body.contains_key("config")
        && lock(&shared.reject_config)
            .iter()
            .any(|prefix| path.starts_with(prefix.as_str()));
    if rejected {
        return (
            400,
            String::new(),
            error_envelope(400, "config is not accepted on create"),
        );
    }
    if body.get("config").is_some_and(|config| !config.is_object()) {
        return (
            400,
            String::new(),
            error_envelope(400, "config must be an object"),
        );
    }
    let kind = address.kind;
    let name_key = if kind == "forward" {
        "listen_address"
    } else {
        "name"
    };
    let name = text_in(&body, name_key);
    if name.is_empty() || name.contains('/') {
        return (
            400,
            String::new(),
            error_envelope(400, "the object has no usable name"),
        );
    }
    let project = if kind == "project" {
        name.clone()
    } else {
        address.project.clone()
    };
    let parent = address.parent.clone();
    if matches!(kind, "volume" | "bucket") && !store.pools.iter().any(|(pool, _)| *pool == parent) {
        return (
            404,
            String::new(),
            error_envelope(404, "the storage pool is not found"),
        );
    }
    if kind == "forward"
        && store
            .find(&Address {
                kind: "network",
                project: project.clone(),
                parent: String::new(),
                name: parent.clone(),
            })
            .is_none()
    {
        return (
            404,
            String::new(),
            error_envelope(404, "the network is not found"),
        );
    }
    if store.objects.iter().any(|object| {
        object.kind == kind
            && object.project == project
            && object.parent == parent
            && object.name == name
    }) {
        return (
            409,
            String::new(),
            error_envelope(409, "the object already exists"),
        );
    }
    body.insert("name".to_string(), Value::String(name.clone()));
    if kind != "project" {
        body.insert("project".to_string(), Value::String(project.clone()));
    }
    body.entry("config").or_insert_with(|| json!({}));
    if matches!(kind, "instance" | "volume") {
        // Incus gives an instance and a volume a uuid, in its `volatile.uuid` config key. No other kind has one.
        if let Some(config) = body.get_mut("config").and_then(Value::as_object_mut) {
            config.insert("volatile.uuid".to_string(), Value::String(shared.next_id()));
        }
    }
    match kind {
        "instance" => {
            body.entry("status")
                .or_insert_with(|| Value::String("Stopped".to_string()));
        }
        "volume" => {
            body.insert("pool".to_string(), Value::String(parent.clone()));
            body.entry("type")
                .or_insert_with(|| Value::String("custom".to_string()));
        }
        _ => {}
    }
    store.objects.push(Stored {
        kind: kind.to_string(),
        project,
        name,
        parent,
        body: Value::Object(body),
    });
    answered(shared, store, kind, "POST", Ok(()))
}

/// Changes an object from a PATCH. A stale `If-Match` is 412 and changes nothing. An empty string in `config` removes its key.
fn patch(
    shared: &Shared,
    store: &mut Store,
    request: &Request,
    address: &Address,
) -> (u16, String, Value) {
    let Some(index) = store.find(address) else {
        return (404, String::new(), error_envelope(404, "not found"));
    };
    if let Some(sent) = request.header("if-match")
        && sent != etag_of(&store.objects[index].body)
    {
        return (
            412,
            String::new(),
            error_envelope(412, "the object changed after it was read"),
        );
    }
    let Ok(Value::Object(changes)) = serde_json::from_slice::<Value>(&request.body) else {
        return (
            400,
            String::new(),
            error_envelope(400, "the body is not a JSON object"),
        );
    };
    let Some(object) = store.objects[index].body.as_object_mut() else {
        return (
            500,
            String::new(),
            error_envelope(500, "the object is not an object"),
        );
    };
    if let Err(message) = merge(object, &changes) {
        return (400, String::new(), error_envelope(400, message));
    }
    let kind = store.objects[index].kind.clone();
    answered(shared, store, &kind, "PATCH", Ok(()))
}

/// Applies a PATCH body to an object. Top-level keys replace; `config` keys merge, and an empty or null value removes a key.
fn merge(
    object: &mut Map<String, Value>,
    changes: &Map<String, Value>,
) -> Result<(), &'static str> {
    for (key, value) in changes {
        if key != "config" {
            object.insert(key.clone(), value.clone());
            continue;
        }
        let Some(edits) = value.as_object() else {
            return Err("config must be an object");
        };
        let config = object
            .entry("config")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or("config must be an object")?;
        for (name, edit) in edits {
            match edit {
                Value::Null => {
                    config.remove(name);
                }
                Value::String(text) if text.is_empty() => {
                    config.remove(name);
                }
                Value::String(_) => {
                    config.insert(name.clone(), edit.clone());
                }
                _ => return Err("a config value must be text"),
            }
        }
    }
    Ok(())
}

/// Removes an object. A non-empty project is refused at once. A running instance's delete fails inside its operation.
fn delete(shared: &Shared, store: &mut Store, address: &Address) -> (u16, String, Value) {
    let Some(index) = store.find(address) else {
        return (404, String::new(), error_envelope(404, "not found"));
    };
    let kind = store.objects[index].kind.clone();
    if kind == "project"
        && store
            .objects
            .iter()
            .any(|object| object.kind != "project" && object.project == address.name)
    {
        return (
            400,
            String::new(),
            error_envelope(400, "the project is not empty"),
        );
    }
    if kind == "instance" && store.objects[index].body.get("status") == Some(&json!("Running")) {
        return answered(
            shared,
            store,
            &kind,
            "DELETE",
            Err((400, "The instance is running".to_string())),
        );
    }
    store.objects.remove(index);
    answered(shared, store, &kind, "DELETE", Ok(()))
}

/// Whether Incus answers a write to this kind and method with an operation. An instance's and a volume's create and delete
/// are. Their config update is sync, as every other write is.
fn is_async(kind: &str, method: &str) -> bool {
    matches!(kind, "instance" | "volume") && method != "PATCH"
}

/// Answers a write. An async write records its operation and answers 202 with the operation's URL; the operation's own status
/// carries the outcome. A sync write answers with its result: 201 for a create, 200 for a change or a delete.
fn answered(
    shared: &Shared,
    store: &mut Store,
    kind: &str,
    method: &str,
    outcome: Result<(), (u16, String)>,
) -> (u16, String, Value) {
    if !is_async(kind, method) {
        return match outcome {
            Ok(()) if method == "POST" => (201, String::new(), sync_envelope(Value::Null)),
            Ok(()) => (200, String::new(), sync_envelope(Value::Null)),
            Err((code, message)) => (code, String::new(), error_envelope(code, &message)),
        };
    }
    let id = shared.next_id();
    let operation = match outcome {
        Ok(()) => json!({
            "id": id, "class": "task", "status": "Success", "status_code": 200,
            "err": "", "resources": {}, "metadata": null,
        }),
        Err((code, message)) => json!({
            "id": id, "class": "task", "status": "Failure", "status_code": code,
            "err": message, "resources": {}, "metadata": null,
        }),
    };
    let url = format!("/1.0/operations/{id}");
    let envelope = json!({
        "type": "async", "status": "Operation created", "status_code": 100,
        "operation": url, "error_code": 0, "error": "", "metadata": operation,
    });
    store.operations.insert(id, operation);
    (202, String::new(), envelope)
}

/// The ETag of an object: a digest of its canonical JSON, quoted as HTTP has it.
fn etag_of(body: &Value) -> String {
    let text = canonical_json(body).unwrap_or_default();
    format!("\"{}\"", Digest::of(text.as_bytes()).to_hex())
}

fn text_of(body: &Value, key: &str) -> String {
    body.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn text_in(body: &Map<String, Value>, key: &str) -> String {
    body.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn sync(metadata: Value) -> (u16, String, Value) {
    (200, String::new(), sync_envelope(metadata))
}

fn sync_envelope(metadata: Value) -> Value {
    json!({
        "type": "sync",
        "status": "Success",
        "status_code": 200,
        "operation": "",
        "error_code": 0,
        "error": "",
        "metadata": metadata,
    })
}

fn error_envelope(status: u16, message: &str) -> Value {
    json!({
        "type": "error",
        "status": reason(status),
        "status_code": status,
        "operation": "",
        "error_code": status,
        "error": message,
        "metadata": null,
    })
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        412 => "Precondition Failed",
        _ => "Bad Request",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_query_parameter_is_found_by_its_exact_key() {
        assert_eq!(query_param("project=svc&x=1", "project"), Some("svc"));
        assert_eq!(query_param("xproject=svc", "project"), None);
        assert_eq!(query_param("", "project"), None);
    }

    #[test]
    fn an_unknown_path_has_no_route_and_a_known_collection_takes_only_post() {
        assert!(parse("/1.0/nope").is_none());
        assert!(matches!(parse("/1.0/projects"), Some(Route::Collection(_))));
        assert!(matches!(
            parse("/1.0/projects?recursion=1"),
            Some(Route::ListProjects)
        ));
        assert_eq!(
            parse("/1.0/instances").map(|r| r.methods()),
            Some(&["POST"][..])
        );
        assert_eq!(
            parse("/1.0/instances/a").map(|r| r.methods()),
            Some(&["GET", "PATCH", "DELETE"][..])
        );
    }

    #[test]
    fn a_merge_removes_a_key_set_to_empty_and_replaces_top_level_keys() {
        let mut object = json!({"config": {"a": "1", "b": "2"}, "description": "x"})
            .as_object()
            .cloned()
            .unwrap_or_default();
        let changes = json!({"config": {"a": "", "c": "3"}, "description": "y"})
            .as_object()
            .cloned()
            .unwrap_or_default();
        merge(&mut object, &changes).expect("a valid merge");
        assert_eq!(object["config"], json!({"b": "2", "c": "3"}));
        assert_eq!(object["description"], json!("y"));
    }

    #[test]
    fn a_config_value_that_is_not_text_is_refused_by_the_merge() {
        let mut object = Map::new();
        let changes = json!({"config": {"a": 1}})
            .as_object()
            .cloned()
            .unwrap_or_default();
        assert!(merge(&mut object, &changes).is_err());
    }

    #[test]
    fn the_etag_changes_with_the_object_and_is_quoted() {
        let first = etag_of(&json!({"config": {"a": "1"}}));
        let second = etag_of(&json!({"config": {"a": "2"}}));
        assert_ne!(first, second);
        assert!(first.starts_with('"') && first.ends_with('"'));
    }
}
