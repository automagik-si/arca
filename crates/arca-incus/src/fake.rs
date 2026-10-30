//! A fake Incus for tests. It serves a fixed set of GET routes from a [`FakeState`] on a unix socket, and it records every
//! request it receives.
//!
//! Routes: `GET /1.0`, `/1.0/projects?recursion=1`, `/1.0/instances?all-projects=true&recursion=1`,
//! `/1.0/storage-pools?recursion=1` and `/1.0/storage-pools/<pool>/resources`. The pool list holds one `{"name"}` object per
//! pool. The resources route answers with the pool's `resources` document. Any other GET is 404. Any other method is 405
//! with `Allow: GET`, and it is never routed.

use std::fs;
use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::shim::{IO_TIMEOUT, drain, parse_request_line, read_head, respond};

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

/// A running fake Incus. It serves until it is dropped.
pub struct FakeIncus {
    socket: PathBuf,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}

impl FakeIncus {
    /// Binds `incus.sock` under `dir` and serves `state` there.
    pub fn start(dir: &Path, state: FakeState) -> io::Result<FakeIncus> {
        let socket = dir.join("incus.sock");
        let listener = UnixListener::bind(&socket)?;
        listener.set_nonblocking(true)?;
        let state = Arc::new(state);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let accept = {
            let state = Arc::clone(&state);
            let requests = Arc::clone(&requests);
            let stop = Arc::clone(&stop);
            thread::spawn(move || accept_loop(&listener, &state, &requests, &stop))
        };
        Ok(FakeIncus {
            socket,
            requests,
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
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
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

fn accept_loop(
    listener: &UnixListener,
    state: &Arc<FakeState>,
    requests: &Arc<Mutex<Vec<RecordedRequest>>>,
    stop: &AtomicBool,
) {
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let state = Arc::clone(state);
                let requests = Arc::clone(requests);
                thread::spawn(move || serve(stream, &state, &requests));
            }
            Err(_) => thread::sleep(Duration::from_millis(5)),
        }
    }
}

/// One connection: read the head, route it if it is a GET, record it, answer it.
fn serve(mut stream: UnixStream, state: &FakeState, requests: &Mutex<Vec<RecordedRequest>>) {
    if stream.set_nonblocking(false).is_err()
        || stream.set_read_timeout(Some(IO_TIMEOUT)).is_err()
        || stream.set_write_timeout(Some(IO_TIMEOUT)).is_err()
    {
        return;
    }
    let head = match read_head(&mut stream) {
        Ok(Some(head)) => head,
        Ok(None) => return,
        Err(_) => Vec::new(),
    };
    let (method, target, status, outcome) = match request_line(&head) {
        Some(("GET", target)) => {
            let (status, outcome) = route(state, target);
            ("GET".to_string(), target.to_string(), status, outcome)
        }
        Some((method, target)) => (
            method.to_string(),
            target.to_string(),
            405,
            Err("method not allowed".to_string()),
        ),
        None => (
            String::new(),
            String::new(),
            400,
            Err("bad request".to_string()),
        ),
    };
    record(requests, &method, &target, status);
    let extra = if status == 405 { "Allow: GET\r\n" } else { "" };
    let _ = respond(
        &mut stream,
        status,
        reason(status),
        extra,
        &envelope(status, outcome),
    );
    drain(&mut stream);
}

/// The method and target of a head's request line, or `None` when the line is not a request line.
fn request_line(head: &[u8]) -> Option<(&str, &str)> {
    let text = std::str::from_utf8(head).ok()?;
    parse_request_line(text.lines().next()?)
}

fn record(requests: &Mutex<Vec<RecordedRequest>>, method: &str, path: &str, status: u16) {
    requests
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(RecordedRequest {
            method: method.to_string(),
            path: path.to_string(),
            status,
        });
}

/// The status and the answer for one GET target. Only the listed routes match, and each query must match exactly.
fn route(state: &FakeState, target: &str) -> (u16, Result<Value, String>) {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    match (path, query) {
        ("/1.0", "") => (200, Ok(state.server.clone())),
        ("/1.0/projects", "recursion=1") => (200, Ok(Value::Array(state.projects.clone()))),
        ("/1.0/instances", "all-projects=true&recursion=1") => {
            (200, Ok(Value::Array(state.instances.clone())))
        }
        ("/1.0/storage-pools", "recursion=1") => {
            let names = state
                .pools
                .iter()
                .map(|(name, _)| json!({ "name": name }))
                .collect();
            (200, Ok(Value::Array(names)))
        }
        _ => {
            let pool = path
                .strip_prefix("/1.0/storage-pools/")
                .and_then(|rest| rest.strip_suffix("/resources"))
                .filter(|pool| !pool.is_empty() && !pool.contains('/'));
            match pool.and_then(|pool| state.pools.iter().find(|(name, _)| name.as_str() == pool)) {
                Some((_, resources)) if query.is_empty() => (200, Ok(resources.clone())),
                _ => (404, Err("not found".to_string())),
            }
        }
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Bad Request",
    }
}

/// The Incus answer envelope: a sync success with `metadata`, or an error with its message.
fn envelope(status: u16, outcome: Result<Value, String>) -> Vec<u8> {
    let value = match outcome {
        Ok(metadata) => json!({
            "type": "sync",
            "status": "Success",
            "status_code": 200,
            "operation": "",
            "error_code": 0,
            "error": "",
            "metadata": metadata,
        }),
        Err(message) => json!({
            "type": "error",
            "status": reason(status),
            "status_code": status,
            "operation": "",
            "error_code": status,
            "error": message,
            "metadata": null,
        }),
    };
    serde_json::to_vec(&value).unwrap_or_default()
}
