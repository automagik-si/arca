//! A fake Incus for the storage and snapshot paths. It serves pools, projects, instances and custom volumes from memory on a
//! unix socket, and records every request with its method, target and body, so a test can check exactly what was sent.
//!
//! GET: `/1.0/storage-pools?recursion=1`, `/1.0/storage-pools/<p>`, `/1.0/storage-pools/<p>/resources`, `/1.0/projects/<p>`,
//! `/1.0/instances/<n>?project=<p>`, `/1.0/instances?project=<p>&recursion=1`,
//! `/1.0/instances?all-projects=true&recursion=1`, `/1.0/instances/<n>/state?project=<p>`,
//! `/1.0/storage-pools/<p>/volumes/custom?project=<p>&recursion=1`, `/1.0/storage-pools/<p>/volumes/custom/<n>?project=<p>`,
//! its `/state`, and `/1.0/operations/<id>/wait`. Writes: `POST /1.0/storage-pools`, `POST /1.0/projects`, `POST` of an
//! instance or a volume with a `copy` source, `PUT` of an instance, and `DELETE` of either. An instance or volume create or
//! delete is answered with an operation that has already ended, unless the test holds operations
//! ([`FakeStorageIncus::hold_operations`]) or fails the next copy ([`FakeStorageIncus::fail_next_copy`]). A created instance
//! is `Stopped` unless its body says `start: true`. Any other request is 404 for a GET and 405 otherwise. Nothing here writes
//! a disk; a `dir` pool's source folder is only looked at, and the create fails as Incus's dir driver does when it is
//! missing (`Source path '<p>' doesn't exist`) or has anything in it (`Source path '<p>' isn't empty`).
//!
//! For folder snapshots it also serves `GET /1.0/images/aliases/<a>`, `POST /1.0/images` (which only records the alias),
//! an instance `POST` from a local image alias, `PUT .../state` (start and stop; a start can be failed with
//! [`FakeStorageIncus::fail_next_start`]), `POST .../exec` on a running instance (answered with the exit code and output
//! set by [`FakeStorageIncus::set_exec`], or failed with [`FakeStorageIncus::fail_next_exec`]), the recorded output as raw
//! text, a `POST` of a new empty volume, a volume `PUT` and `GET /1.0/profiles/<p>`. A running instance cannot be
//! deleted, and a stopped one cannot be stopped again, as in Incus. As on a real host, a failed start leaves the instance
//! reading back as stopped while Incus still holds it, so a delete is refused until it is stopped; and an instance whose
//! root disk is on a snapshot pool fails to start, since its unprivileged root cannot walk that pool's folder.
//!
//! An instance copy behaves as Incus's own does (`createFromCopy`): every local config key and local device of the source
//! that the request leaves unset is carried into the copy (volatile keys aside), an empty config value is not stored, the
//! source's profiles are used when the request names none, and a disk device on a custom volume the target project does not
//! have fails the create. A `PUT` replaces the config, devices and profiles, and refuses one that drops a volatile idmap
//! key, as Incus does. A volume copy takes the source's config only when the request has none.

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{Value, json};

use crate::shim::{drain, head_end, parse_request_line, respond};

/// One request the fake received.
#[derive(Debug, Clone, PartialEq)]
pub struct StorageRequest {
    pub method: String,
    pub target: String,
    pub body: Value,
}

#[derive(Default)]
struct State {
    pools: BTreeMap<String, Value>,
    resources: BTreeMap<String, Value>,
    projects: BTreeMap<String, Value>,
    instances: BTreeMap<(String, String), Value>,
    usage: BTreeMap<(String, String), u64>,
    volumes: BTreeMap<(String, String, String), Value>,
    operations: BTreeMap<String, Value>,
    requests: Vec<StorageRequest>,
    counter: u64,
    hold: bool,
    fail_next: Option<String>,
    ignore_puts: bool,
    aliases: std::collections::BTreeSet<(String, String)>,
    exec: (i64, String),
    fail_start: Option<String>,
    fail_exec: Option<String>,
    logs: BTreeMap<String, String>,
    profiles: BTreeMap<(String, String), Value>,
    half_started: std::collections::BTreeSet<(String, String)>,
}

/// The running fake. Dropping it stops it.
pub struct FakeStorageIncus {
    socket: PathBuf,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}

fn lock(state: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

impl FakeStorageIncus {
    /// Binds `incus.sock` under `dir`. The `default` project exists.
    pub fn start(dir: &Path) -> io::Result<FakeStorageIncus> {
        let socket = dir.join("incus.sock");
        let listener = UnixListener::bind(&socket)?;
        listener.set_nonblocking(true)?;
        let state = Arc::new(Mutex::new(State::default()));
        lock(&state).projects.insert(
            "default".to_string(),
            json!({ "name": "default", "config": { "features.networks": "true" } }),
        );
        let stop = Arc::new(AtomicBool::new(false));
        let accept = {
            let state = Arc::clone(&state);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => serve(stream, &state),
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        Ok(FakeStorageIncus {
            socket,
            state,
            stop,
            accept: Some(accept),
        })
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Adds a pool with its config and its space.
    pub fn add_pool(
        &self,
        name: &str,
        driver: &str,
        description: &str,
        config: Value,
        used: u64,
        total: u64,
    ) {
        let mut state = lock(&self.state);
        state.pools.insert(
            name.to_string(),
            json!({ "name": name, "driver": driver, "description": description, "config": config, "status": "Created" }),
        );
        state.resources.insert(
            name.to_string(),
            json!({ "space": { "used": used, "total": total } }),
        );
    }

    /// Adds a project with its config.
    pub fn add_project(&self, name: &str, config: Value) {
        lock(&self.state)
            .projects
            .insert(name.to_string(), json!({ "name": name, "config": config }));
    }

    /// Adds an instance as `GET /1.0/instances/<n>` answers it. Its `project` field names its project.
    pub fn add_instance(&self, instance: Value) {
        let project = instance["project"]
            .as_str()
            .unwrap_or("default")
            .to_string();
        let name = instance["name"].as_str().unwrap_or_default().to_string();
        lock(&self.state)
            .instances
            .insert((project, name), instance);
    }

    /// Adds a custom volume on `pool` as Incus answers it. Its `project` field names its project.
    pub fn add_volume(&self, pool: &str, volume: Value) {
        let project = volume["project"].as_str().unwrap_or("default").to_string();
        let name = volume["name"].as_str().unwrap_or_default().to_string();
        lock(&self.state)
            .volumes
            .insert((pool.to_string(), project, name), volume);
    }

    /// Sets the disk use an instance's or volume's state reports.
    pub fn set_usage(&self, project: &str, name: &str, bytes: u64) {
        lock(&self.state)
            .usage
            .insert((project.to_string(), name.to_string()), bytes);
    }

    /// Keeps every new operation running, so a wait answers that it is still running.
    pub fn hold_operations(&self) {
        lock(&self.state).hold = true;
    }

    /// Ends every held operation successfully, and stops holding new ones.
    pub fn release_operations(&self) {
        let mut state = lock(&self.state);
        state.hold = false;
        for operation in state.operations.values_mut() {
            if operation["status_code"] == json!(103) {
                operation["status_code"] = json!(200);
            }
        }
    }

    /// Answers every instance `PUT` as done without applying it, as a rewrite that did not take would.
    pub fn ignore_puts(&self) {
        lock(&self.state).ignore_puts = true;
    }

    /// Fails the next instance or volume copy with Incus's message.
    pub fn fail_next_copy(&self, message: &str) {
        lock(&self.state).fail_next = Some(message.to_string());
    }

    /// Answers every later exec with this exit code and recorded output.
    pub fn set_exec(&self, code: i64, stdout: &str) {
        lock(&self.state).exec = (code, stdout.to_string());
    }

    /// Fails the next instance start with Incus's message.
    pub fn fail_next_start(&self, message: &str) {
        lock(&self.state).fail_start = Some(message.to_string());
    }

    /// Fails the next exec operation with Incus's message.
    pub fn fail_next_exec(&self, message: &str) {
        lock(&self.state).fail_exec = Some(message.to_string());
    }

    /// Adds a profile in `project` as Incus answers it.
    pub fn add_profile(&self, project: &str, profile: Value) {
        let name = profile["name"].as_str().unwrap_or_default().to_string();
        lock(&self.state)
            .profiles
            .insert((project.to_string(), name), profile);
    }

    /// Adds a local image alias in `project`.
    pub fn add_image_alias(&self, project: &str, alias: &str) {
        lock(&self.state)
            .aliases
            .insert((project.to_string(), alias.to_string()));
    }

    pub fn requests(&self) -> Vec<StorageRequest> {
        lock(&self.state).requests.clone()
    }

    /// The requests that were not GETs.
    pub fn writes(&self) -> Vec<StorageRequest> {
        self.requests()
            .into_iter()
            .filter(|request| request.method != "GET")
            .collect()
    }

    pub fn instance(&self, project: &str, name: &str) -> Option<Value> {
        lock(&self.state)
            .instances
            .get(&(project.to_string(), name.to_string()))
            .cloned()
    }

    pub fn instances(&self) -> Vec<Value> {
        lock(&self.state).instances.values().cloned().collect()
    }

    pub fn volume(&self, pool: &str, project: &str, name: &str) -> Option<Value> {
        lock(&self.state)
            .volumes
            .get(&(pool.to_string(), project.to_string(), name.to_string()))
            .cloned()
    }

    pub fn pool(&self, name: &str) -> Option<Value> {
        lock(&self.state).pools.get(name).cloned()
    }

    pub fn project(&self, name: &str) -> Option<Value> {
        lock(&self.state).projects.get(name).cloned()
    }
}

impl Drop for FakeStorageIncus {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

fn serve(mut stream: UnixStream, state: &Mutex<State>) {
    if stream.set_nonblocking(false).is_err()
        || stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .is_err()
    {
        return;
    }
    let Ok(Some((method, target, body))) = read_request(&mut stream) else {
        let _ = respond(&mut stream, 400, "Bad Request", "", b"");
        drain(&mut stream);
        return;
    };
    let (status, envelope) = {
        let mut state = lock(state);
        state.requests.push(StorageRequest {
            method: method.clone(),
            target: target.clone(),
            body: body.clone(),
        });
        handle(&mut state, &method, &target, &body)
    };
    // A recorded exec output is answered as raw text, as Incus does.
    let bytes = match envelope.get("raw-text").and_then(Value::as_str) {
        Some(text) => text.as_bytes().to_vec(),
        None => serde_json::to_vec(&envelope).unwrap_or_default(),
    };
    let reason = match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        _ => "Bad Request",
    };
    let _ = respond(&mut stream, status, reason, "", &bytes);
    drain(&mut stream);
}

fn read_request(stream: &mut UnixStream) -> io::Result<Option<(String, String, Value)>> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_len = loop {
        if let Some(end) = head_end(&buffer) {
            break end;
        }
        match stream.read(&mut chunk)? {
            0 => return Ok(None),
            n => buffer.extend_from_slice(&chunk[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buffer[..head_len]).to_string();
    let mut lines = head.lines();
    let Some((method, target)) = parse_request_line(lines.next().unwrap_or("")) else {
        return Ok(None);
    };
    let (method, target) = (method.to_string(), target.to_string());
    let length = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = buffer[head_len..].to_vec();
    while body.len() < length {
        match stream.read(&mut chunk)? {
            0 => break,
            n => body.extend_from_slice(&chunk[..n]),
        }
    }
    let body = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap_or(Value::Null)
    };
    Ok(Some((method, target, body)))
}

fn sync(metadata: Value) -> (u16, Value) {
    (
        200,
        json!({ "type": "sync", "status": "Success", "status_code": 200, "metadata": metadata }),
    )
}

fn error(code: u16, message: &str) -> (u16, Value) {
    (
        code,
        json!({ "type": "error", "error": message, "error_code": code }),
    )
}

fn operation(state: &mut State, failure: Option<String>) -> (u16, Value) {
    operation_with(state, failure, Value::Null)
}

fn operation_with(state: &mut State, failure: Option<String>, metadata: Value) -> (u16, Value) {
    state.counter += 1;
    let id = format!("00000000-0000-4000-8000-{:012x}", state.counter);
    let (code, err) = match (failure, state.hold) {
        (Some(message), _) => (400, message),
        (None, true) => (103, String::new()),
        (None, false) => (200, String::new()),
    };
    let op = json!({ "id": id, "status_code": code, "err": err, "metadata": metadata });
    state.operations.insert(id.clone(), op.clone());
    (
        202,
        json!({ "type": "async", "status": "Operation created", "status_code": 100,
                "operation": format!("/1.0/operations/{id}"), "metadata": op }),
    )
}

/// The config, devices and profiles of an instance copy, as Incus's `createFromCopy` makes them: the request's own, with
/// every local key and device of the source the request leaves unset carried in (volatile keys aside, except the two Incus
/// keeps), and without empty values. A new copy gets its own volatile keys.
fn merged_copy(body: &Value, source: &Value) -> (Value, Value, Value) {
    let mut config = body["config"].as_object().cloned().unwrap_or_default();
    for (key, value) in source["config"].as_object().into_iter().flatten() {
        let kept = !key.starts_with("volatile.")
            || key == "volatile.base_image"
            || key == "volatile.last_state.idmap";
        if kept && !config.contains_key(key) {
            config.insert(key.clone(), value.clone());
        }
    }
    config.retain(|_, value| value.as_str().is_some_and(|text| !text.is_empty()));
    config.insert(
        "volatile.uuid".to_string(),
        json!("00000000-0000-4000-8000-00000000c0b1"),
    );
    config.insert("volatile.idmap.base".to_string(), json!("0"));
    let mut devices = body["devices"].as_object().cloned().unwrap_or_default();
    for (name, device) in source["devices"].as_object().into_iter().flatten() {
        devices
            .entry(name.clone())
            .or_insert_with(|| device.clone());
    }
    let profiles = if body["profiles"].is_null() {
        source["profiles"].clone()
    } else {
        body["profiles"].clone()
    };
    (Value::Object(config), Value::Object(devices), profiles)
}

fn handle(state: &mut State, method: &str, target: &str, body: &Value) -> (u16, Value) {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let params: BTreeMap<&str, &str> = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .collect();
    let project = params
        .get("project")
        .copied()
        .unwrap_or("default")
        .to_string();
    let segments: Vec<&str> = path.trim_start_matches("/1.0/").split('/').collect();
    match (method, segments.as_slice()) {
        ("GET", ["storage-pools"]) => {
            sync(json!(state.pools.values().cloned().collect::<Vec<_>>()))
        }
        ("GET", ["storage-pools", pool]) => match state.pools.get(*pool) {
            Some(found) => sync(found.clone()),
            None => error(404, "Storage pool not found"),
        },
        ("GET", ["storage-pools", pool, "resources"]) => match state.resources.get(*pool) {
            Some(found) => sync(found.clone()),
            None => error(404, "Storage pool not found"),
        },
        ("POST", ["storage-pools"]) => {
            let name = body["name"].as_str().unwrap_or_default().to_string();
            if state.pools.contains_key(&name) {
                return error(409, "Storage pool already exists");
            }
            // As Incus's dir driver: the source folder must already be there, and be empty.
            if body["driver"] == json!("dir") {
                let source = body["config"]["source"].as_str().unwrap_or_default();
                if std::fs::symlink_metadata(source).is_err() {
                    return error(500, &format!("Source path '{source}' doesn't exist"));
                }
                let empty = std::fs::read_dir(source)
                    .map(|mut entries| entries.next().is_none())
                    .unwrap_or(false);
                if !empty {
                    return error(500, &format!("Source path '{source}' isn't empty"));
                }
            }
            let mut pool = body.clone();
            pool["status"] = json!("Created");
            state.pools.insert(name.clone(), pool);
            state.resources.insert(
                name,
                json!({ "space": { "used": 0, "total": 1_000_000_000_000u64 } }),
            );
            sync(Value::Null)
        }
        ("GET", ["projects", name]) => match state.projects.get(*name) {
            Some(found) => sync(found.clone()),
            None => error(404, "Project not found"),
        },
        ("POST", ["projects"]) => {
            let name = body["name"].as_str().unwrap_or_default().to_string();
            if state.projects.contains_key(&name) {
                return error(409, "Project already exists");
            }
            state.projects.insert(name, body.clone());
            sync(Value::Null)
        }
        ("GET", ["instances"]) if params.get("all-projects") == Some(&"true") => {
            sync(json!(state.instances.values().cloned().collect::<Vec<_>>()))
        }
        ("GET", ["instances"]) => {
            if !state.projects.contains_key(&project) {
                return error(404, "Project not found");
            }
            let rows: Vec<Value> = state
                .instances
                .iter()
                .filter(|((owner, _), _)| *owner == project)
                .map(|(_, row)| row.clone())
                .collect();
            sync(json!(rows))
        }
        ("GET", ["instances", name]) => {
            match state.instances.get(&(project.clone(), (*name).to_string())) {
                Some(found) => sync(found.clone()),
                None => error(404, "Instance not found"),
            }
        }
        ("GET", ["instances", name, "state"]) => {
            let key = (project.clone(), (*name).to_string());
            match state.instances.get(&key) {
                Some(found) => {
                    let usage = state.usage.get(&key).copied().unwrap_or(0);
                    sync(
                        json!({ "status": found["status"], "disk": { "root": { "usage": usage } } }),
                    )
                }
                None => error(404, "Instance not found"),
            }
        }
        ("POST", ["instances"]) => {
            if !state.projects.contains_key(&project) {
                return error(404, "Project not found");
            }
            let name = body["name"].as_str().unwrap_or_default().to_string();
            if state
                .instances
                .contains_key(&(project.clone(), name.clone()))
            {
                return error(409, "Instance already exists");
            }
            if body["source"]["type"] == json!("image") {
                return create_from_image(state, &project, &name, body);
            }
            let from_project = body["source"]["project"]
                .as_str()
                .unwrap_or("default")
                .to_string();
            let from = body["source"]["source"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let Some(source) = (body["source"]["type"] == json!("copy"))
                .then(|| state.instances.get(&(from_project, from)))
                .flatten()
                .cloned()
            else {
                return error(404, "Source instance not found");
            };
            let (config, devices, profiles) = merged_copy(body, &source);
            for (device_name, device) in devices.as_object().into_iter().flatten() {
                let on_volume = device["type"] == json!("disk")
                    && device.get("path") != Some(&json!("/"))
                    && device["source"]
                        .as_str()
                        .is_some_and(|source| !source.starts_with('/'));
                if on_volume {
                    let key = (
                        device["pool"].as_str().unwrap_or_default().to_string(),
                        project.clone(),
                        device["source"].as_str().unwrap_or_default().to_string(),
                    );
                    if !state.volumes.contains_key(&key) {
                        return error(
                            400,
                            &format!(
                                "Failed add validation for device \"{device_name}\": Failed loading custom volume: Storage volume not found"
                            ),
                        );
                    }
                }
            }
            if let Some(message) = state.fail_next.take() {
                return operation(state, Some(message));
            }
            let running = body["start"] == json!(true);
            let row = json!({
                "name": name, "project": project, "type": body["type"], "architecture": "x86_64",
                "description": body["description"], "profiles": profiles,
                "config": config, "devices": devices,
                "expanded_config": config, "expanded_devices": devices,
                "status": if running { "Running" } else { "Stopped" },
                "created_at": "2026-11-09T14:05:00Z",
            });
            state.instances.insert((project, name), row);
            operation(state, None)
        }
        ("PUT", ["instances", name]) => {
            let key = (project.clone(), (*name).to_string());
            let Some(found) = state.instances.get(&key).cloned() else {
                return error(404, "Instance not found");
            };
            let dropped_idmap =
                found["config"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .any(|(key, _)| {
                        key.starts_with("volatile.idmap.") && body["config"].get(key).is_none()
                    });
            if dropped_idmap {
                return error(400, "Volatile idmap keys can't be deleted by the user");
            }
            if state.ignore_puts {
                return operation(state, None);
            }
            let mut row = found;
            for field in ["config", "devices", "profiles", "description"] {
                row[field] = body[field].clone();
            }
            row["expanded_config"] = body["config"].clone();
            row["expanded_devices"] = body["devices"].clone();
            state.instances.insert(key, row);
            operation(state, None)
        }
        ("DELETE", ["instances", name]) => {
            let key = (project, (*name).to_string());
            match state.instances.get(&key) {
                None => return error(404, "Instance not found"),
                Some(found)
                    if found["status"] == json!("Running") || state.half_started.contains(&key) =>
                {
                    return error(400, "The instance is currently running, stop it first");
                }
                Some(_) => {}
            }
            state.instances.remove(&key);
            operation(state, None)
        }
        ("PUT", ["instances", name, "state"]) => {
            let key = (project, (*name).to_string());
            if !state.instances.contains_key(&key) {
                return error(404, "Instance not found");
            }
            let root_pool = state.instances[&key]["expanded_devices"]["root"]["pool"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let root_on_snapshots = state.pools.get(&root_pool).is_some_and(|pool| {
                pool["config"].get("user.arcasnapshot.storage") == Some(&pool["name"])
            });
            let status = match body["action"].as_str() {
                Some("start") => {
                    let failure = state.fail_start.take().or_else(|| {
                        root_on_snapshots.then(|| {
                            "Failed to run: Permission denied - Failed to mount \"credentials\" onto \"/dev/.incus-systemd-credentials\"".to_string()
                        })
                    });
                    if let Some(message) = failure {
                        state.half_started.insert(key);
                        return operation(state, Some(message));
                    }
                    "Running"
                }
                Some("stop") => {
                    let held = state.half_started.remove(&key);
                    if !held && state.instances[&key]["status"] == json!("Stopped") {
                        return operation(
                            state,
                            Some("The instance is already stopped".to_string()),
                        );
                    }
                    "Stopped"
                }
                _ => return error(400, "Unknown action"),
            };
            if let Some(row) = state.instances.get_mut(&key) {
                row["status"] = json!(status);
            }
            operation(state, None)
        }
        ("POST", ["instances", name, "exec"]) => {
            let key = (project, (*name).to_string());
            match state.instances.get(&key) {
                None => return error(404, "Instance not found"),
                Some(found) if found["status"] != json!("Running") => {
                    return error(400, "Instance is not running");
                }
                Some(_) => {}
            }
            if let Some(message) = state.fail_exec.take() {
                return operation(state, Some(message));
            }
            let (code, stdout) = state.exec.clone();
            let file = format!("exec_{:04}.stdout", state.counter + 1);
            state.logs.insert(format!("{name}/{file}"), stdout);
            let output = format!("/1.0/instances/{name}/logs/exec-output/{file}");
            operation_with(
                state,
                None,
                json!({ "return": code, "output": { "1": output, "2": output.replace(".stdout", ".stderr") } }),
            )
        }
        ("GET", ["instances", name, "logs", "exec-output", file]) => {
            match state.logs.get(&format!("{name}/{file}")) {
                Some(text) => (200, json!({ "raw-text": text })),
                None => error(404, "not found"),
            }
        }
        ("GET", ["profiles", name]) => match state.profiles.get(&(project, (*name).to_string())) {
            Some(found) => sync(found.clone()),
            None => error(404, "Profile not found"),
        },
        ("GET", ["images", "aliases", alias]) => {
            if state.aliases.contains(&(project, (*alias).to_string())) {
                sync(json!({ "name": alias, "target": "f00d" }))
            } else {
                error(404, "Image alias not found")
            }
        }
        ("POST", ["images"]) => {
            for alias in body["aliases"].as_array().into_iter().flatten() {
                if let Some(name) = alias["name"].as_str() {
                    state.aliases.insert((project.clone(), name.to_string()));
                }
            }
            operation(state, None)
        }
        ("PUT", ["storage-pools", pool, "volumes", "custom", name]) => {
            let key = ((*pool).to_string(), project, (*name).to_string());
            let Some(row) = state.volumes.get_mut(&key) else {
                return error(404, "Storage volume not found");
            };
            row["config"] = body["config"].clone();
            row["description"] = body["description"].clone();
            sync(Value::Null)
        }
        ("GET", ["storage-pools", pool, "volumes", "custom"]) => {
            let rows: Vec<Value> = state
                .volumes
                .iter()
                .filter(|((on, owner, _), _)| on == pool && *owner == project)
                .map(|(_, row)| row.clone())
                .collect();
            sync(json!(rows))
        }
        ("GET", ["storage-pools", pool, "volumes", "custom", name]) => {
            match state
                .volumes
                .get(&((*pool).to_string(), project.clone(), (*name).to_string()))
            {
                Some(found) => sync(found.clone()),
                None => error(404, "Storage volume not found"),
            }
        }
        ("GET", ["storage-pools", pool, "volumes", "custom", name, "state"]) => {
            if !state.volumes.contains_key(&(
                (*pool).to_string(),
                project.clone(),
                (*name).to_string(),
            )) {
                return error(404, "Storage volume not found");
            }
            let usage = state
                .usage
                .get(&(project, (*name).to_string()))
                .copied()
                .unwrap_or(0);
            sync(json!({ "usage": { "used": usage } }))
        }
        ("POST", ["storage-pools", pool, "volumes", "custom"]) => {
            let name = body["name"].as_str().unwrap_or_default().to_string();
            let key = ((*pool).to_string(), project.clone(), name.clone());
            if state.volumes.contains_key(&key) {
                return error(409, "Storage volume already exists");
            }
            if body.get("source").is_none_or(Value::is_null) {
                let row = json!({
                    "name": name, "project": project, "pool": pool, "type": "custom",
                    "content_type": body["content_type"], "description": body["description"],
                    "config": body["config"], "created_at": "2026-11-09T14:05:00Z",
                });
                state.volumes.insert(key, row);
                return operation(state, None);
            }
            let source = (
                body["source"]["pool"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                body["source"]["project"]
                    .as_str()
                    .unwrap_or("default")
                    .to_string(),
                body["source"]["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            );
            if !state.volumes.contains_key(&source) {
                return error(404, "Source volume not found");
            }
            if let Some(message) = state.fail_next.take() {
                return operation(state, Some(message));
            }
            let config = if body["config"].is_null() {
                state.volumes[&source]["config"].clone()
            } else {
                body["config"].clone()
            };
            let row = json!({
                "name": name, "project": project, "pool": pool, "type": "custom",
                "content_type": body["content_type"], "description": body["description"],
                "config": config, "created_at": "2026-11-09T14:05:00Z",
            });
            state.volumes.insert(key, row);
            operation(state, None)
        }
        ("DELETE", ["storage-pools", pool, "volumes", "custom", name]) => {
            if state
                .volumes
                .remove(&((*pool).to_string(), project, (*name).to_string()))
                .is_none()
            {
                return error(404, "Storage volume not found");
            }
            sync(Value::Null)
        }
        ("GET", ["operations", id, "wait"]) => match state.operations.get(*id) {
            Some(found) => sync(found.clone()),
            None => error(404, "Operation not found"),
        },
        ("GET", _) => error(404, "not found"),
        _ => error(405, "method not allowed"),
    }
}

/// An instance made from a local image alias, with exactly the request's config, devices and profiles. A disk on a custom
/// volume the project does not have fails the create, as in Incus.
fn create_from_image(state: &mut State, project: &str, name: &str, body: &Value) -> (u16, Value) {
    let alias = body["source"]["alias"].as_str().unwrap_or_default();
    if !state
        .aliases
        .contains(&(project.to_string(), alias.to_string()))
    {
        return error(404, "Image alias not found");
    }
    for (device_name, device) in body["devices"].as_object().into_iter().flatten() {
        let on_volume = device["type"] == json!("disk")
            && device.get("path") != Some(&json!("/"))
            && device.get("pool").is_some();
        let key = (
            device["pool"].as_str().unwrap_or_default().to_string(),
            project.to_string(),
            device["source"].as_str().unwrap_or_default().to_string(),
        );
        if on_volume && !state.volumes.contains_key(&key) {
            return error(
                400,
                &format!(
                    "Failed add validation for device \"{device_name}\": Failed loading custom volume: Storage volume not found"
                ),
            );
        }
    }
    if let Some(message) = state.fail_next.take() {
        return operation(state, Some(message));
    }
    let mut config = body["config"].as_object().cloned().unwrap_or_default();
    config.insert(
        "volatile.uuid".to_string(),
        json!("00000000-0000-4000-8000-00000000c0b2"),
    );
    let config = Value::Object(config);
    let row = json!({
        "name": name, "project": project, "type": body["type"], "architecture": "x86_64",
        "description": body["description"], "profiles": body["profiles"],
        "config": config, "devices": body["devices"],
        "expanded_config": config, "expanded_devices": body["devices"],
        "status": "Stopped", "created_at": "2026-11-09T14:05:00Z",
    });
    state
        .instances
        .insert((project.to_string(), name.to_string()), row);
    operation(state, None)
}
