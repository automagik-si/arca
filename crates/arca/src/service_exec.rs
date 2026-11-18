//! Audited service exec outside the effect journal. Provider and plugin effects have no access to this capability.
//! Lock order is publish_gate, writes, kernel. Journal reads acquire no such lock and have four permits.
//!
//! Two kinds of command run here. The closed list ([`ExecCommand`], Rule 50) through [`ServiceExec::run`], and a
//! command a template's formula declares ([`DeclaredCommand`], Rule 82) through [`ServiceExec::run_formula`]. Both
//! share the audit, the identity checks and the hung mark; a declared command also runs in an OCI container, keeps the
//! tail of its output in place of a cap, and is never run in an instance that is marked.

use crate::{
    app_formula::{AppUpdate, AppVersion, version_ok},
    config::NodeConfig,
    service::ServiceConfig,
};
use arca_incus::{
    IncusError, Owned, ProjectFamily, UnixIncusWriter,
    exec::{
        ExecCommand, FormulaCommand, FormulaState, SERVICE_PROJECT, node_ok, past_limit,
        valid_operation_id,
    },
    read::Tail,
};
use arca_kernel::{
    canon::Digest,
    desired::{EntryId, PluginId},
    ownership::{IncusKind, ObjectRef},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub const AUDIT_FILE: &str = "exec-audit.jsonl";
#[derive(Debug)]
pub enum ExecError {
    Refused(&'static str),
    Failed(String),
    Audit,
}
impl ExecError {
    pub fn ownership(&self) -> bool {
        matches!(
            self,
            Self::Refused("owner" | "uuid" | "tombstone" | "instance_changed" | "target")
        )
    }
    pub fn call_error(&self) -> arca_mcp::CallError {
        match self {
            Self::Audit => arca_mcp::CallError::Internal("exec audit unavailable".into()),
            Self::Refused(code) => arca_mcp::CallError::Refused {
                code: (*code).into(),
                message: format!("service exec refused: {code}"),
            },
            Self::Failed(text) => arca_mcp::CallError::Refused {
                code: "exec".into(),
                message: text.clone(),
            },
        }
    }
}

/// Which part of a formula a declared command is. What is kept of its output depends on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormulaPart {
    /// The command that moves the app to a version: its last [`UPDATE_LINES`] lines are kept.
    Update,
    /// One try of the health check: its last [`HEALTH_LINES`] lines are kept, only when it fails.
    Health,
    /// The version reader: only its first line, and only when that is a plain version.
    Version,
}

impl FormulaPart {
    /// The step the exec audit names.
    pub fn step(self) -> &'static str {
        match self {
            Self::Update => "formula_update",
            Self::Health => "formula_health",
            Self::Version => "formula_version",
        }
    }
}

/// How many of the update command's last lines are kept.
pub const UPDATE_LINES: usize = 60;
/// How many of a failed health check's last lines are kept.
pub const HEALTH_LINES: usize = 5;
/// The most characters of one kept line.
pub const LINE_CHARS: usize = 300;
/// The longest one try of the health check runs.
pub const HEALTH_TRY_SECONDS: u64 = 30;
/// How much longer than its in-instance limit Arca waits for a declared command before it marks the instance hung.
pub const FORMULA_MARGIN: Duration = Duration::from_secs(5);

/// A command a template's formula declares, ready to run in a service's container. It is made only from a formula that
/// passes its checks for the template's kind (`AppUpdate::check`) and, for the update command, a checked version, so
/// the version is one plain argument and never the program.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeclaredCommand {
    part: FormulaPart,
    command: FormulaCommand,
}

impl DeclaredCommand {
    /// The update command of `formula`, with `version` in its one `{version}` argument, limited to `update.seconds`.
    pub fn update(formula: &AppUpdate, kind: &str, version: &AppVersion) -> Result<Self, String> {
        formula.check(kind)?;
        let argv = formula
            .update_arguments(version)
            .ok_or("appUpdate has no update command")?;
        Self::made(
            FormulaPart::Update,
            argv,
            formula.update_seconds().unwrap_or(0),
        )
    }
    /// One try of the health check of `formula`: cut at 30 seconds, or at `health.seconds` when that is less.
    pub fn health(formula: &AppUpdate, kind: &str) -> Result<Self, String> {
        formula.check(kind)?;
        let step = formula.health.as_ref().ok_or("appUpdate has no health")?;
        Self::made(
            FormulaPart::Health,
            step.command.clone(),
            formula.health_seconds().min(HEALTH_TRY_SECONDS),
        )
    }
    /// The version reader of `formula`, limited to `version.seconds`.
    pub fn version(formula: &AppUpdate, kind: &str) -> Result<Self, String> {
        formula.check(kind)?;
        let step = formula.version.as_ref().ok_or("appUpdate has no version")?;
        Self::made(
            FormulaPart::Version,
            step.command.clone(),
            formula.version_seconds(),
        )
    }
    fn made(part: FormulaPart, argv: Vec<String>, seconds: u64) -> Result<Self, String> {
        let command = FormulaCommand::new(argv, seconds)
            .map_err(|_| "appUpdate's command is not one a service runs".to_string())?;
        Ok(Self { part, command })
    }
    /// The same command with a shorter in-instance limit (what is left of a health check's time), never a longer one.
    pub fn within(mut self, seconds: u64) -> Self {
        self.command = self.command.within(seconds);
        self
    }
    pub fn part(&self) -> FormulaPart {
        self.part
    }
    /// How long the command may run inside the instance.
    pub fn seconds(&self) -> u64 {
        self.command.seconds()
    }
    /// The argument list Incus is asked to start.
    pub fn argv(&self) -> Vec<String> {
        self.command.argv()
    }
}

/// What a declared command left. A non-zero exit is an answer here, not an error: the caller decides what it means.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormulaRun {
    /// The command's exit code.
    pub exit: i64,
    /// Whether the in-instance limit ended it (`/usr/bin/timeout`'s 124 or 137).
    pub past_limit: bool,
    /// Whether an output was longer than the 64 KiB kept of it.
    pub output_cut: bool,
    /// The kept lines, masked: the app's words, untrusted. Empty for the version reader and for a health check that
    /// passed.
    pub lines: Vec<String>,
    /// The version reader's first line, when it is a plain version. `None` for every other part.
    pub version: Option<String>,
}

#[derive(Clone)]
pub struct ExecTarget {
    pub entry: EntryId,
    pub service: ServiceConfig,
    pub uuid: Option<String>,
    pub retained: BTreeMap<EntryId, (PluginId, Digest)>,
}

pub struct ServiceExec {
    cfg: Arc<NodeConfig>,
    audit_lock: Mutex<()>,
    sequence: AtomicU64,
    hung: Mutex<BTreeMap<String, String>>,
    journal_reads: AtomicUsize,
    pub publish_gate: tokio::sync::Mutex<()>,
}

pub struct JournalPermit<'a>(&'a AtomicUsize);
impl Drop for JournalPermit<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
impl ServiceExec {
    /// Construct only after the kernel holds its exclusive state lock. Startup sweep precedes exposure of this handle.
    pub fn new(cfg: Arc<NodeConfig>) -> Self {
        let seam = Self {
            cfg,
            audit_lock: Mutex::new(()),
            sequence: AtomicU64::new(1),
            hung: Mutex::new(BTreeMap::new()),
            journal_reads: AtomicUsize::new(0),
            publish_gate: tokio::sync::Mutex::new(()),
        };
        seam.sweep();
        seam
    }
    pub fn journal_permit(&self) -> Option<JournalPermit<'_>> {
        self.journal_reads
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < 4).then_some(n + 1)
            })
            .ok()
            .map(|_| JournalPermit(&self.journal_reads))
    }
    fn path(&self) -> PathBuf {
        self.cfg.state_dir.join(AUDIT_FILE)
    }
    fn append(&self, row: &Value) -> Result<(), ExecError> {
        let _guard = self.audit_lock.lock().map_err(|_| ExecError::Audit)?;
        if let Ok(meta) = fs::symlink_metadata(self.path())
            && (!meta.is_file() || meta.permissions().mode() & 0o777 != 0o600)
        {
            return Err(ExecError::Audit);
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(self.path())
            .map_err(|_| ExecError::Audit)?;
        let mut bytes = serde_json::to_vec(row).map_err(|_| ExecError::Audit)?;
        bytes.push(b'\n');
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|_| ExecError::Audit)
    }
    pub fn audit_refusal(
        &self,
        token: &str,
        verb: &str,
        entry: &str,
        instance: &str,
    ) -> Result<(), ExecError> {
        let id = self.sequence.fetch_add(1, Ordering::SeqCst);
        self.append(&json!({"kind":"start","id":id,"at_ms":now(),"token_id":token,"verb":verb,"step":"preflight","entry":entry,"project":SERVICE_PROJECT,"instance":instance,"uuid":null,"lines":null}))?;
        self.append(&json!({"kind":"end","id":id,"outcome":"refused","exit":null,"duration_ms":0,"stdout_bytes":0,"output_left":false}))
    }
    pub fn writer(
        &self,
        target: &ExecTarget,
        budget: Duration,
    ) -> Result<UnixIncusWriter, ExecError> {
        writer_for(&self.cfg, target, budget)
    }
    pub fn check(&self, target: &ExecTarget, writer: &UnixIncusWriter) -> Result<Owned, ExecError> {
        self.check_command(target, writer, false)
    }
    fn check_command(
        &self,
        target: &ExecTarget,
        writer: &UnixIncusWriter,
        connector_status: bool,
    ) -> Result<Owned, ExecError> {
        let cfg = &target.service;
        if cfg.adopted {
            return Err(ExecError::Refused("adopted"));
        }
        if cfg.project != SERVICE_PROJECT
            || cfg.instance != cfg.name
            || target.entry.0 != format!("service-{}", cfg.name)
            || !node_ok(&cfg.name)
        {
            return Err(ExecError::Refused("target"));
        }
        if connector_status && crate::reach::kind(&self.cfg, cfg).is_none() {
            return Err(ExecError::Refused("connector"));
        }
        // The connector's status read is the sole new OCI command; every existing command keeps its old rule.
        if cfg.source["protocol"] == "oci" && cfg.edge.is_none() && !connector_status {
            return Err(ExecError::Refused("oci"));
        }
        let uuid = target.uuid.as_ref().ok_or(ExecError::Refused("uuid"))?;
        let object = ObjectRef {
            kind: IncusKind::Instance,
            project: cfg.project.clone(),
            parent: None,
            name: cfg.name.clone(),
            uuid: Some(uuid.clone()),
        };
        let owned = writer
            .read_owned(&object)
            .map_err(|_| ExecError::Refused("owner"))?;
        if owned.tombstone().is_some() {
            return Err(ExecError::Refused("tombstone"));
        }
        let (state, _) = writer
            .get_etag(&object)
            .map_err(|_| ExecError::Failed("service state unavailable".into()))?;
        if state["status"] != "Running" {
            return Err(ExecError::Refused("stopped"));
        }
        Ok(owned)
    }
    pub fn run(
        &self,
        target: &ExecTarget,
        token: &str,
        verb: &str,
        command: &ExecCommand,
        secrets: &[String],
        until: Option<Instant>,
    ) -> Result<Vec<u8>, ExecError> {
        let started = Instant::now();
        let id = self.sequence.fetch_add(1, Ordering::SeqCst);
        self.append(&json!({"kind":"start","id":id,"at_ms":now(),"token_id":token,"verb":verb,"step":command.step(),"entry":target.entry.0,"project":target.service.project,"instance":target.service.instance,"uuid":target.uuid,"lines":command.lines()}))?;
        let limit = Duration::from_secs(command.seconds() + 3);
        let budget = until.map_or(limit, |end| {
            limit.min(end.saturating_duration_since(Instant::now()))
        });
        let writer = self.writer(target, budget)?.with_operation_deadline(budget);
        let mut op = None;
        let mut exit = None;
        let mut bytes = 0;
        let mut attempted = false;
        let mut result = (|| {
            let owned = self.check_command(
                target,
                &writer,
                matches!(command, ExecCommand::ConnectorStatus),
            )?;
            self.refuse_hung(target, command.cleanup())?;
            attempted = true;
            let operation = writer
                .start_service_exec(&owned, command, budget.min(Duration::from_secs(10)))
                .map_err(|e| self.error(target, e))?;
            op = Some(operation.clone());
            self.append(&json!({"kind":"op","id":id,"op":operation,"instance":target.service.name,"project":SERVICE_PROJECT}))?;
            let status = writer
                .wait_service_exec(&operation)
                .map_err(|e| self.error(target, e))?;
            exit = Some(status);
            let stdout = writer
                .service_output(&owned, &operation, false)
                .map_err(|e| self.error(target, e))?;
            bytes = stdout.len();
            let stderr = if matches!(command, ExecCommand::Join(_)) {
                writer
                    .service_output(&owned, &operation, true)
                    .map_err(|e| self.error(target, e))?
            } else {
                Vec::new()
            };
            if status != 0 {
                let text = if stderr.is_empty() {
                    String::new()
                } else {
                    format!(
                        "; untrusted: {}",
                        failure_text(&String::from_utf8_lossy(&stderr), secrets)
                    )
                };
                return Err(ExecError::Failed(format!(
                    "{} exited {status}{text}",
                    command.step()
                )));
            }
            Ok(stdout)
        })();
        // Failures and over-cap answers must also check identity before callers consider a console fallback.
        if attempted && self.changed(target, until)? {
            result = Err(ExecError::Refused("instance_changed"));
        }
        // Try both deletions, even if waiting, parsing, reading, or appending the operation failed.
        let output_left = self.delete_outputs(target, op.as_deref(), until)?;
        self.end(id, &result, exit, started, bytes, output_left)?;
        result
    }
    /// Runs a command a template's formula declares (Rule 82), in a `system` or an OCI container of the node's own
    /// service project, with Rule 50's identity checks and audit. It is started detached and waited on in bounded waits
    /// until its in-instance limit and [`FORMULA_MARGIN`] have passed, or `until` when that comes first. A command Incus
    /// does not answer for by then marks the instance hung, and no declared command runs in a marked instance. The last
    /// 64 KiB of each output is kept, masked as `service_logs` masks a log, and both recordings are deleted on every
    /// outcome; an instance whose uuid changed meanwhile has its output discarded.
    pub fn run_formula(
        &self,
        target: &ExecTarget,
        token: &str,
        verb: &str,
        command: &DeclaredCommand,
        until: Option<Instant>,
    ) -> Result<FormulaRun, ExecError> {
        let started = Instant::now();
        let id = self.sequence.fetch_add(1, Ordering::SeqCst);
        self.append(&json!({"kind":"start","id":id,"at_ms":now(),"token_id":token,"verb":verb,"step":command.part.step(),"entry":target.entry.0,"project":target.service.project,"instance":target.service.instance,"uuid":target.uuid,"lines":null}))?;
        let limit = Duration::from_secs(command.seconds()) + FORMULA_MARGIN;
        let deadline = started
            + until.map_or(limit, |end| {
                limit.min(end.saturating_duration_since(Instant::now()))
            });
        let mut op = None;
        let mut exit = None;
        let mut bytes = 0;
        let mut attempted = false;
        let mut result = (|| {
            formula_target(target)?;
            let writer = self.writer(target, deadline.saturating_duration_since(Instant::now()))?;
            let (owned, instance) = self.check_formula(target, &writer)?;
            self.refuse_hung(target, false)?;
            attempted = true;
            let operation = writer
                .start_formula_exec(
                    &owned,
                    &command.command,
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_secs(10)),
                )
                .map_err(|e| self.error(target, e))?;
            op = Some(operation.clone());
            self.append(&json!({"kind":"op","id":id,"op":operation,"instance":target.service.name,"project":SERVICE_PROJECT}))?;
            let status = loop {
                let left = deadline.saturating_duration_since(Instant::now());
                // Each wait asks Incus to answer a little before the deadline, so its own request ends by then.
                let wait = left
                    .saturating_sub(Duration::from_secs(1))
                    .min(Duration::from_secs(30));
                if wait.is_zero() {
                    return Err(self.error(target, IncusError::Timeout));
                }
                match writer.formula_exec_state(&operation, wait) {
                    Ok(FormulaState::Running) => continue,
                    Ok(FormulaState::Exited(code)) => break code,
                    Err(error) => return Err(self.error(target, error)),
                }
            };
            exit = Some(status);
            // The command has ended: its outputs are read under a budget of their own, not what is left of its time.
            let reader = self.writer(target, budget_until(Duration::from_secs(20), until))?;
            let unread = |_| ExecError::Failed("service exec output unavailable".into());
            let stdout = reader
                .formula_output(&owned, &operation, false)
                .map_err(unread)?;
            let stderr = reader
                .formula_output(&owned, &operation, true)
                .map_err(unread)?;
            bytes = stdout.bytes.len() + stderr.bytes.len();
            Ok((status, stdout, stderr, instance))
        })();
        if attempted && self.changed(target, until)? {
            result = Err(ExecError::Refused("instance_changed"));
        }
        let output_left = self.delete_outputs(target, op.as_deref(), until)?;
        self.end(id, &result, exit, started, bytes, output_left)?;
        let (exit, stdout, stderr, instance) = result?;
        let mut secrets =
            crate::handlers::service_secret_values(&self.cfg.state_dir.join("secrets"));
        secrets.extend(environment_values(&instance));
        Ok(kept(command.part, exit, &stdout, &stderr, &secrets))
    }
    /// Whether `target`'s instance is marked hung: a command of either kind that Incus did not answer for by Arca's
    /// deadline, in the instance with this uuid.
    pub fn hung(&self, target: &ExecTarget) -> bool {
        self.hung.lock().is_ok_and(|hung| {
            hung.get(&target.service.name)
                .is_some_and(|uuid| Some(uuid) == target.uuid.as_ref())
        })
    }
    /// Clears `target`'s hung mark when Incus shows its instance, with the same uuid, stopped: a stopped instance runs
    /// nothing, so the command that did not answer has ended. `Ok(true)` when the instance is stopped and holds no mark
    /// any more, `Ok(false)` when it is not stopped and a mark stays. This makes no stop of its own.
    pub fn clear_hung_after_stop(&self, target: &ExecTarget) -> Result<bool, ExecError> {
        let uuid = target.uuid.as_ref().ok_or(ExecError::Refused("uuid"))?;
        let writer = self.writer(target, Duration::from_secs(10))?;
        let object = instance_object(target, uuid);
        writer
            .read_owned(&object)
            .map_err(|_| ExecError::Refused("owner"))?;
        let (state, _) = writer
            .get_etag(&object)
            .map_err(|_| ExecError::Failed("service state unavailable".into()))?;
        if state["status"] != "Stopped" {
            return Ok(false);
        }
        let mut hung = self.hung.lock().map_err(|_| ExecError::Audit)?;
        if hung.get(&target.service.name) == Some(uuid) {
            hung.remove(&target.service.name);
        }
        Ok(true)
    }
    /// Refuses a command in an instance that is marked hung. A mark of another uuid is dropped: that instance is gone.
    /// A cleanup command of the closed list is let through.
    fn refuse_hung(&self, target: &ExecTarget, cleanup: bool) -> Result<(), ExecError> {
        let mut hung = self.hung.lock().map_err(|_| ExecError::Audit)?;
        if hung
            .get(&target.service.name)
            .is_some_and(|uuid| Some(uuid) != target.uuid.as_ref())
        {
            hung.remove(&target.service.name);
        }
        if hung.contains_key(&target.service.name) && !cleanup {
            return Err(ExecError::Refused("hung"));
        }
        Ok(())
    }
    /// Whether the instance's uuid changed while a command ran in it.
    fn changed(&self, target: &ExecTarget, until: Option<Instant>) -> Result<bool, ExecError> {
        let post = self.writer(target, budget_until(Duration::from_secs(3), until))?;
        let object = ObjectRef {
            kind: IncusKind::Instance,
            project: target.service.project.clone(),
            parent: None,
            name: target.service.instance.clone(),
            uuid: target.uuid.clone(),
        };
        Ok(!post
            .read_class(&object)
            .is_ok_and(|fresh| fresh.uuid == target.uuid))
    }
    /// Deletes both recordings of an operation, trying each whatever became of the other. Whether one is left.
    fn delete_outputs(
        &self,
        target: &ExecTarget,
        op: Option<&str>,
        until: Option<Instant>,
    ) -> Result<bool, ExecError> {
        let mut output_left = false;
        if let Some(op) = op {
            let cleanup = self.writer(target, budget_until(Duration::from_secs(6), until))?;
            for stderr in [false, true] {
                if cleanup
                    .delete_service_output(&target.service.name, op, stderr)
                    .is_err()
                {
                    output_left = true;
                }
            }
        }
        Ok(output_left)
    }
    /// The audit's end row of a command: its outcome and sizes, never its output.
    fn end<T>(
        &self,
        id: u64,
        result: &Result<T, ExecError>,
        exit: Option<i64>,
        started: Instant,
        bytes: usize,
        output_left: bool,
    ) -> Result<(), ExecError> {
        let outcome = match result {
            Ok(_) => "success",
            Err(ExecError::Refused(_)) => "refused",
            Err(ExecError::Audit) => "audit",
            Err(_) => "failed",
        };
        self.append(&json!({"kind":"end","id":id,"outcome":outcome,"exit":exit,"duration_ms":started.elapsed().as_millis(),"stdout_bytes":bytes,"output_left":output_left}))
    }
    /// Rule 50's checks of the instance as Incus shows it now, for a declared command whose record passed
    /// [`formula_target`]: the uuid, the owner's mark, no tombstone, and a running instance.
    fn check_formula(
        &self,
        target: &ExecTarget,
        writer: &UnixIncusWriter,
    ) -> Result<(Owned, Value), ExecError> {
        let uuid = target.uuid.as_ref().ok_or(ExecError::Refused("uuid"))?;
        let object = instance_object(target, uuid);
        let owned = writer
            .read_owned(&object)
            .map_err(|_| ExecError::Refused("owner"))?;
        if owned.tombstone().is_some() {
            return Err(ExecError::Refused("tombstone"));
        }
        let (state, _) = writer
            .get_etag(&object)
            .map_err(|_| ExecError::Failed("service state unavailable".into()))?;
        if state["status"] != "Running" {
            return Err(ExecError::Refused("stopped"));
        }
        Ok((owned, state))
    }
    fn error(&self, target: &ExecTarget, error: IncusError) -> ExecError {
        if error == IncusError::Timeout {
            if let Some(uuid) = &target.uuid
                && let Ok(mut hung) = self.hung.lock()
            {
                hung.insert(target.service.name.clone(), uuid.clone());
            }
            return ExecError::Failed("service exec deadline passed; instance marked hung".into());
        }
        ExecError::Failed(
            match error {
                IncusError::TooLarge => "service exec output exceeds 64 KiB",
                _ => "service exec failed",
            }
            .into(),
        )
    }
    /// Only validated audit op rows without an end are eligible. No instance file listing is consulted.
    fn sweep(&self) {
        let Ok(file) = fs::File::open(self.path()) else {
            return;
        };
        let mut pending = BTreeMap::new();
        let mut ended = BTreeSet::new();
        let mut maximum = 0;
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            if line.len() > 65536 {
                continue;
            }
            let Ok(row) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let Some(id) = row["id"].as_u64() else {
                continue;
            };
            maximum = maximum.max(id);
            match row["kind"].as_str() {
                Some("op") if row["project"] == SERVICE_PROJECT => {
                    if let (Some(op), Some(instance)) =
                        (row["op"].as_str(), row["instance"].as_str())
                        && valid_operation_id(op)
                        && node_ok(instance)
                    {
                        pending.insert(id, (op.to_owned(), instance.to_owned()));
                    }
                }
                Some("end") => {
                    ended.insert(id);
                }
                _ => {}
            }
        }
        self.sequence
            .store(maximum.saturating_add(1), Ordering::SeqCst);
        pending.retain(|id, _| !ended.contains(id));
        let left = pending.len().saturating_sub(256);
        if left > 0 {
            eprintln!("arca: {left} exec recordings deferred to another startup");
        }
        let Ok(family) = ProjectFamily::new([SERVICE_PROJECT], std::iter::empty()) else {
            return;
        };
        let writer = UnixIncusWriter::new(
            self.cfg.incus.socket.clone(),
            EntryId("service-sweep".into()),
            BTreeMap::new(),
            family,
        );
        for (id, (op, instance)) in pending.into_iter().rev().take(256) {
            let a = writer.delete_service_output(&instance, &op, false);
            let b = writer.delete_service_output(&instance, &op, true);
            let output_left = a.is_err() || b.is_err();
            let _ = self.append(&json!({"kind":"swept","id":id,"op":op,"output_left":output_left}));
            if !output_left {
                let _=self.append(&json!({"kind":"end","id":id,"outcome":"swept","exit":null,"duration_ms":0,"stdout_bytes":0,"output_left":false}));
            }
        }
    }
}

/// The writer bound to `target`'s entry: over the node's configured family, and for a service in a tenant over that
/// tenant's project too and no other tenant's. A record whose project is not its own tenant's is refused.
pub fn writer_for(
    cfg: &NodeConfig,
    target: &ExecTarget,
    budget: Duration,
) -> Result<UnixIncusWriter, ExecError> {
    let family = ProjectFamily::new(
        cfg.kernel.projects.iter().map(String::as_str),
        cfg.kernel.droidd_projects.iter().map(String::as_str),
    )
    .map_err(|_| ExecError::Refused("target"))?;
    // A tenant's service is read in its own tenant's project, and in no other tenant's.
    let family = match &target.service.tenant {
        Some(tenant)
            if arca_kernel::ownership::tenant_of_project(&target.service.project)
                == Some(tenant.id.as_str()) =>
        {
            family
                .with_tenants([target.service.project.as_str()])
                .map_err(|_| ExecError::Refused("target"))?
        }
        Some(_) => return Err(ExecError::Refused("target")),
        None => family,
    };
    Ok(UnixIncusWriter::new(
        cfg.incus.socket.clone(),
        target.entry.clone(),
        target.retained.clone(),
        family,
    )
    .with_total_budget(budget))
}

/// A failure's words as they are shown: every secret and PEM block hidden in the text as it came, before cleaning
/// joins its lines (a value of several lines is only found while its lines are apart), and again after.
pub fn failure_text(text: &str, secrets: &[String]) -> String {
    let redacted = crate::service::redact_log(text, secrets);
    crate::service::redact_log(&crate::service::clean_log(&redacted), secrets)
        .chars()
        .take(300)
        .collect()
}
/// Which records a declared command may run for, read from the record alone. Unlike the closed list, an OCI container
/// is one; an adopted instance, a tenant's, a virtual machine, another project and an instance that is not the
/// service's own are refused.
fn formula_target(target: &ExecTarget) -> Result<(), ExecError> {
    let cfg = &target.service;
    if cfg.adopted {
        return Err(ExecError::Refused("adopted"));
    }
    if cfg.tenant.is_some() {
        return Err(ExecError::Refused("tenant"));
    }
    if cfg.vm.is_some() {
        return Err(ExecError::Refused("vm"));
    }
    if cfg.project != SERVICE_PROJECT
        || cfg.instance != cfg.name
        || target.entry.0 != format!("service-{}", cfg.name)
        || !node_ok(&cfg.name)
    {
        return Err(ExecError::Refused("target"));
    }
    Ok(())
}

/// `budget`, or what is left until `until` when that is less.
fn budget_until(budget: Duration, until: Option<Instant>) -> Duration {
    until.map_or(budget, |end| {
        budget.min(end.saturating_duration_since(Instant::now()))
    })
}

fn instance_object(target: &ExecTarget, uuid: &str) -> ObjectRef {
    ObjectRef {
        kind: IncusKind::Instance,
        project: target.service.project.clone(),
        parent: None,
        name: target.service.name.clone(),
        uuid: Some(uuid.to_owned()),
    }
}

/// The instance's own environment values, as `service_logs` hides them: each key its config lists, of at most 4096 bytes.
fn environment_values(instance: &Value) -> Vec<String> {
    let config = &instance["config"];
    config[crate::service::ENVKEYS_KEY]
        .as_str()
        .unwrap_or("")
        .split(',')
        .filter(|key| !key.is_empty())
        .filter_map(|key| config[format!("environment.{key}")].as_str())
        .filter(|value| value.len() <= 4096)
        .map(str::to_owned)
        .collect()
}

/// One output's lines as `service_logs` shows a log: every PEM block hidden whole first, a text that begins inside a
/// key hidden up to the key's end, then each line cleaned of control bytes and its secret values and credential shapes
/// replaced by `[secret]`. Blank lines are left out, and each line is cut at [`LINE_CHARS`] characters.
pub fn masked_lines(output: &[u8], secrets: &[String]) -> Vec<String> {
    let text = crate::service::redact_blocks(&String::from_utf8_lossy(output));
    let hidden = crate::service::values_to_hide(secrets);
    text.split(['\n', '\r'])
        .map(crate::service::clean_log)
        .filter(|line| !line.is_empty())
        .map(|line| {
            crate::service::redact_hidden(&line, &hidden)
                .chars()
                .take(LINE_CHARS)
                .collect()
        })
        .collect()
}

/// What is kept of a declared command's two outputs for its part. Each output is masked on its own, so a key cut at
/// the start of one is hidden whatever the other holds; the lines of the error output come after the others.
fn kept(
    part: FormulaPart,
    exit: i64,
    stdout: &Tail,
    stderr: &Tail,
    secrets: &[String],
) -> FormulaRun {
    let output_cut = stdout.cut || stderr.cut;
    let mut lines = masked_lines(&stdout.bytes, secrets);
    lines.extend(masked_lines(&stderr.bytes, secrets));
    let (lines, version) = match part {
        FormulaPart::Update => (last(lines, UPDATE_LINES), None),
        FormulaPart::Health if exit != 0 => (last(lines, HEALTH_LINES), None),
        FormulaPart::Health => (Vec::new(), None),
        // The first line of a cut output is not the command's first line.
        FormulaPart::Version => (
            Vec::new(),
            (!stdout.cut)
                .then(|| String::from_utf8_lossy(&stdout.bytes).into_owned())
                .and_then(|text| {
                    let first = text.split(['\n', '\r']).next().unwrap_or("").to_owned();
                    masked_lines(first.as_bytes(), secrets).into_iter().next()
                })
                .filter(|line| version_ok(line)),
        ),
    };
    FormulaRun {
        exit,
        past_limit: past_limit(exit),
        output_cut,
        lines,
        version,
    }
}

fn last(mut lines: Vec<String>, count: usize) -> Vec<String> {
    let start = lines.len().saturating_sub(count);
    lines.drain(..start);
    lines
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}
