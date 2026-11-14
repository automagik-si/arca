//! The per-job runner machines: one Incus virtual machine per CI job, cloned from the runner image, used once, deleted.
//!
//! This writer is outside the desired-state journal, like a function run: a job machine lives for one job and is never
//! declared. It writes in one project only, the pool's own, and every instance it touches carries the runner mark
//! ([`RUNNER_KEY`]) with the pool's name as its value. A create sets the mark; every later write reads the instance first
//! and refuses one that does not carry this pool's mark, so no other instance of the project, and no instance of
//! another pool, is ever started, written into or deleted from here.
//!
//! What it sends: an instance create (a VM, started by the create), files written into and read from the guest through
//! the Incus agent, one command started through the agent, a forced stop and a delete. Nothing else.

use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::lifecycle::{checked_envelope, plain_name, refused};
use crate::read::{IncusError, exchange_capped};

/// The key, in a job machine's own config, that marks it as a runner pool's: its value is the pool's name.
pub const RUNNER_KEY: &str = "user.arca.runner";
/// The largest file read back from a guest, in bytes: a job's packed logs.
pub const PULL_MAX: usize = 64 * 1024 * 1024;
/// The largest file written into a guest, in bytes.
pub const PUSH_MAX: usize = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(10);
const WAIT_SECONDS: u64 = 5;
const POLL_PAUSE: Duration = Duration::from_millis(200);

/// Where a job machine's root disk comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VmSource {
    /// A local image, by alias or fingerprint: on ZFS and Ceph the instance's root is a clone of the image's volume.
    Image(String),
    /// A snapshot of a stopped template instance in the same project (`<instance>/<snapshot>`): the root is a clone of
    /// the snapshot.
    Snapshot { instance: String, snapshot: String },
}

/// One job machine to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmSpec {
    pub name: String,
    pub source: VmSource,
    pub cpu: u32,
    pub memory_mib: u64,
    /// The root disk's size, in GiB; `None` keeps the image's.
    pub root_gib: Option<u64>,
    /// The storage pool of the root disk; `None` takes the profile's.
    pub storage_pool: Option<String>,
    /// The profiles, in order. Empty takes the project's `default`.
    pub profiles: Vec<String>,
}

/// What became of a command started in a guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecState {
    Running,
    /// Ended with this exit code.
    Exited(i64),
    /// Incus no longer holds the operation: it ended, and its code is gone. The guest's own record says the rest.
    Gone,
}

/// The writer for one pool's job machines.
#[derive(Debug, Clone)]
pub struct RunnerVms {
    socket: PathBuf,
    project: String,
    pool: String,
    deadline: Duration,
}

impl RunnerVms {
    /// A writer for the machines of `pool` in `project`. Both must be plain names.
    pub fn new(socket: PathBuf, project: &str, pool: &str) -> Result<Self, IncusError> {
        if !plain_name(project) || !plain_name(pool) {
            return Err(refused("the project or the pool is not a plain name"));
        }
        Ok(Self {
            socket,
            project: project.to_string(),
            pool: pool.to_string(),
            deadline: Duration::from_secs(600),
        })
    }

    /// The same writer with another limit on one create or delete.
    pub fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }

    pub fn project(&self) -> &str {
        &self.project
    }

    /// The names of this pool's machines in the project, whatever their state.
    pub fn list(&self) -> Result<Vec<String>, IncusError> {
        let path = format!("/1.0/instances?project={}&recursion=1", self.project);
        let answer = self.json("GET", &path, None, TIMEOUT)?;
        let rows = answer
            .get("metadata")
            .and_then(Value::as_array)
            .ok_or_else(|| decode("the instance listing is not a list"))?;
        Ok(rows
            .iter()
            .filter(|row| self.marked(row))
            .filter_map(|row| row.get("name").and_then(Value::as_str))
            .map(str::to_string)
            .collect())
    }

    /// Creates one machine and starts it. The name must be free; the instance is a virtual machine carrying this pool's
    /// mark, with exactly the CPUs and memory of the spec.
    pub fn create(&self, spec: &VmSpec) -> Result<(), IncusError> {
        if !plain_name(&spec.name) || spec.cpu == 0 || spec.memory_mib == 0 {
            return Err(refused(
                "the machine's name is not plain, or it has no CPU or memory",
            ));
        }
        let source = match &spec.source {
            VmSource::Image(name) if plain_name(name) => {
                if is_fingerprint(name) {
                    json!({ "type": "image", "fingerprint": name })
                } else {
                    json!({ "type": "image", "alias": name })
                }
            }
            VmSource::Snapshot { instance, snapshot }
                if plain_name(instance) && plain_name(snapshot) =>
            {
                json!({ "type": "copy", "source": format!("{instance}/{snapshot}"), "instance_only": true })
            }
            _ => return Err(refused("the machine's source is not a plain name")),
        };
        let mut devices = serde_json::Map::new();
        if spec.root_gib.is_some() || spec.storage_pool.is_some() {
            let mut root = json!({ "type": "disk", "path": "/" });
            if let Some(gib) = spec.root_gib {
                root["size"] = json!(format!("{gib}GiB"));
            }
            if let Some(pool) = &spec.storage_pool {
                if !plain_name(pool) {
                    return Err(refused("the storage pool is not a plain name"));
                }
                root["pool"] = json!(pool);
            } else {
                // A root device names its pool; without one Incus takes it from the profile, which a device that is
                // given here replaces. So a size alone is sent only with the pool the profile has.
                return Err(refused("a root disk size needs the storage pool named"));
            }
            devices.insert("root".to_string(), root);
        }
        if spec.profiles.iter().any(|profile| !plain_name(profile)) {
            return Err(refused("a profile is not a plain name"));
        }
        let mut body = json!({
            "name": spec.name,
            "type": "virtual-machine",
            "source": source,
            "start": true,
            "config": {
                RUNNER_KEY: self.pool,
                "limits.cpu": spec.cpu.to_string(),
                "limits.memory": format!("{}MiB", spec.memory_mib),
            },
            "devices": devices,
        });
        if !spec.profiles.is_empty() {
            body["profiles"] = json!(spec.profiles);
        }
        match self.read(&spec.name) {
            Err(IncusError::Status { code: 404, .. }) => {}
            Ok(_) => return Err(refused("an instance of that name exists")),
            Err(error) => return Err(error),
        }
        let path = format!("/1.0/instances?project={}", self.project);
        let envelope = self.json("POST", &path, Some(&body), TIMEOUT)?;
        self.wait_for(&envelope)?;
        let made = self.read(&spec.name)?;
        if !self.marked(&made) {
            return Err(refused(
                "the created instance does not carry the pool's mark",
            ));
        }
        Ok(())
    }

    /// Whether the machine's Incus agent answers: a read of `/` in the guest succeeds.
    pub fn agent_ready(&self, name: &str) -> Result<bool, IncusError> {
        self.owned(name)?;
        let path = self.files_path(name, "/")?;
        let framed = self.raw("GET", &path, &[], &[], TIMEOUT, 64 * 1024)?;
        Ok((200..300).contains(&framed.status))
    }

    /// Writes one file into the guest through the agent, replacing what is there. `bytes` is sent only as the body.
    pub fn push_file(
        &self,
        name: &str,
        guest_path: &str,
        bytes: &[u8],
        uid: u32,
        gid: u32,
        mode: u32,
    ) -> Result<(), IncusError> {
        if bytes.len() > PUSH_MAX {
            return Err(refused("the file is larger than a push may be"));
        }
        self.owned(name)?;
        let path = self.files_path(name, guest_path)?;
        let headers = [
            ("X-Incus-type", "file".to_string()),
            ("X-Incus-uid", uid.to_string()),
            ("X-Incus-gid", gid.to_string()),
            ("X-Incus-mode", format!("{mode:04o}")),
            ("X-Incus-write", "overwrite".to_string()),
        ];
        let framed = self.raw("POST", &path, &headers, bytes, TIMEOUT, 64 * 1024)?;
        checked_envelope(framed).map(|_| ())
    }

    /// Reads one file from the guest through the agent. `None` when the guest has no such file.
    pub fn pull_file(&self, name: &str, guest_path: &str) -> Result<Option<Vec<u8>>, IncusError> {
        self.owned(name)?;
        let path = self.files_path(name, guest_path)?;
        let framed = self.raw("GET", &path, &[], &[], Duration::from_secs(60), PULL_MAX)?;
        match framed.status {
            404 => Ok(None),
            200..=299 if framed.header("x-incus-type") == Some("file") => Ok(Some(framed.body)),
            200..=299 => Err(decode("the guest path is not a file")),
            _ => checked_envelope(framed).map(|_| None),
        }
    }

    /// Starts one command in the guest, as root, detached: no terminal, no websocket, output not recorded. Returns the
    /// operation's id. `argv` and `env` never carry a credential; a credential reaches the guest only as a pushed file.
    pub fn exec(
        &self,
        name: &str,
        argv: &[&str],
        env: &[(&str, &str)],
    ) -> Result<String, IncusError> {
        self.owned(name)?;
        if argv.is_empty() {
            return Err(refused("a command names a program"));
        }
        let environment: serde_json::Map<String, Value> = env
            .iter()
            .map(|(key, value)| ((*key).to_string(), json!(value)))
            .collect();
        let body = json!({
            "command": argv,
            "environment": environment,
            "wait-for-websocket": false,
            "interactive": false,
            "record-output": false,
        });
        let path = format!("/1.0/instances/{name}/exec?project={}", self.project);
        let envelope = self.json("POST", &path, Some(&body), TIMEOUT)?;
        operation_of(&envelope).ok_or_else(|| decode("the exec answer names no operation"))
    }

    /// What became of a command [`Self::exec`] started, waiting up to `wait` for it to end.
    pub fn exec_state(&self, operation: &str, wait: Duration) -> Result<ExecState, IncusError> {
        if !operation_id(operation) {
            return Err(refused("an operation id that is not plain"));
        }
        let path = format!(
            "/1.0/operations/{operation}/wait?timeout={}",
            wait.as_secs().max(1)
        );
        let framed = self.raw("GET", &path, &[], &[], wait + TIMEOUT, 1024 * 1024)?;
        if framed.status == 404 {
            return Ok(ExecState::Gone);
        }
        let answer = checked_envelope(framed)?;
        let op = answer
            .get("metadata")
            .ok_or_else(|| decode("the wait has no operation"))?;
        let code = op.get("status_code").and_then(Value::as_u64).unwrap_or(0);
        match code {
            100..=199 => Ok(ExecState::Running),
            200 => Ok(ExecState::Exited(
                op.pointer("/metadata/return")
                    .and_then(Value::as_i64)
                    .unwrap_or(-1),
            )),
            _ => Ok(ExecState::Exited(-1)),
        }
    }

    /// Stops the machine at once and deletes it. A machine that is already gone is not an error.
    pub fn destroy(&self, name: &str) -> Result<(), IncusError> {
        match self.owned(name) {
            Err(IncusError::Status { code: 404, .. }) => return Ok(()),
            Err(error) => return Err(error),
            Ok(state) => {
                if state.get("status").and_then(Value::as_str) != Some("Stopped") {
                    let path = format!("/1.0/instances/{name}/state?project={}", self.project);
                    let body = json!({ "action": "stop", "force": true, "timeout": -1 });
                    match self.json("PUT", &path, Some(&body), TIMEOUT) {
                        Ok(envelope) => {
                            // A machine that stopped by itself meanwhile fails the stop; the delete below still runs.
                            let _ = self.wait_for(&envelope);
                        }
                        Err(IncusError::Status { .. }) => {}
                        Err(error) => return Err(error),
                    }
                }
            }
        }
        let path = format!("/1.0/instances/{name}?project={}", self.project);
        match self.json("DELETE", &path, None, TIMEOUT) {
            Ok(envelope) => self.wait_for(&envelope),
            Err(IncusError::Status { code: 404, .. }) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Reads one instance's metadata, and refuses it unless it carries this pool's mark.
    fn owned(&self, name: &str) -> Result<Value, IncusError> {
        let state = self.read(name)?;
        if !self.marked(&state) {
            return Err(refused("the instance is not this pool's runner machine"));
        }
        Ok(state)
    }

    fn read(&self, name: &str) -> Result<Value, IncusError> {
        if !plain_name(name) {
            return Err(refused("the instance name is not plain"));
        }
        let path = format!("/1.0/instances/{name}?project={}", self.project);
        self.json("GET", &path, None, TIMEOUT)?
            .get("metadata")
            .cloned()
            .ok_or_else(|| decode("the answer has no metadata"))
    }

    fn marked(&self, metadata: &Value) -> bool {
        metadata.get("type").and_then(Value::as_str) == Some("virtual-machine")
            && metadata
                .get("config")
                .and_then(|config| config.get(RUNNER_KEY))
                .and_then(Value::as_str)
                == Some(self.pool.as_str())
    }

    fn files_path(&self, name: &str, guest_path: &str) -> Result<String, IncusError> {
        let plain = guest_path.starts_with('/')
            && guest_path.len() <= 256
            && !guest_path.split('/').any(|part| part == "..")
            && guest_path.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.')
            });
        if !plain || !plain_name(name) {
            return Err(refused("the guest path is not a plain absolute path"));
        }
        Ok(format!(
            "/1.0/instances/{name}/files?project={}&path={guest_path}",
            self.project
        ))
    }

    fn wait_for(&self, envelope: &Value) -> Result<(), IncusError> {
        if envelope.get("type").and_then(Value::as_str) == Some("sync") {
            return Ok(());
        }
        let id = operation_of(envelope).ok_or_else(|| decode("the answer names no operation"))?;
        let started = Instant::now();
        loop {
            if started.elapsed() >= self.deadline {
                return Err(IncusError::Timeout);
            }
            let path = format!("/1.0/operations/{id}/wait?timeout={WAIT_SECONDS}");
            let framed = self.raw(
                "GET",
                &path,
                &[],
                &[],
                Duration::from_secs(WAIT_SECONDS) + TIMEOUT,
                1024 * 1024,
            )?;
            if framed.status == 404 {
                // Incus drops an ended operation after a while; the caller reads the object to see what it did.
                return Ok(());
            }
            let answer = checked_envelope(framed)?;
            let op = answer
                .get("metadata")
                .ok_or_else(|| decode("the wait has no operation"))?;
            let code = op.get("status_code").and_then(Value::as_u64).unwrap_or(0);
            match code {
                200 => return Ok(()),
                100..=199 => thread::sleep(POLL_PAUSE),
                _ => {
                    return Err(IncusError::Status {
                        code: u16::try_from(code).unwrap_or(500),
                        message: op
                            .get("err")
                            .and_then(Value::as_str)
                            .unwrap_or("the operation failed")
                            .chars()
                            .take(256)
                            .collect(),
                    });
                }
            }
        }
    }

    fn json(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        timeout: Duration,
    ) -> Result<Value, IncusError> {
        let bytes = body
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|_| decode("the request body does not serialize"))?
            .unwrap_or_default();
        let headers: Vec<(&str, String)> = if body.is_some() {
            vec![("Content-Type", "application/json".to_string())]
        } else {
            Vec::new()
        };
        let framed = self.raw(method, path, &headers, &bytes, timeout, 16 * 1024 * 1024)?;
        checked_envelope(framed)
    }

    fn raw(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, String)],
        body: &[u8],
        timeout: Duration,
        cap: usize,
    ) -> Result<crate::read::Framed, IncusError> {
        let mut head = format!("{method} {path} HTTP/1.1\r\nHost: incus\r\nConnection: close\r\n");
        for (key, value) in headers {
            head.push_str(&format!("{key}: {value}\r\n"));
        }
        if !body.is_empty() || method == "POST" || method == "PUT" {
            head.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        head.push_str("\r\n");
        let mut request = head.into_bytes();
        request.extend_from_slice(body);
        exchange_capped(&self.socket, timeout, &request, cap)
    }
}

fn operation_of(envelope: &Value) -> Option<String> {
    let named = envelope
        .get("operation")
        .and_then(Value::as_str)
        .and_then(|operation| operation.strip_prefix("/1.0/operations/"))
        .map(|id| id.split('?').next().unwrap_or(id).to_string());
    let direct = || {
        envelope
            .pointer("/metadata/id")
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    named.or_else(direct).filter(|id| operation_id(id))
}

fn operation_id(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn is_fingerprint(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn decode(reason: &str) -> IncusError {
    IncusError::Decode(reason.to_string())
}
