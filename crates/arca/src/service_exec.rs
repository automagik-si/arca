//! Audited service exec outside the effect journal. Provider and plugin effects have no access to this capability.
//! Lock order is publish_gate, writes, kernel. Journal reads acquire no such lock and have four permits.

use crate::{config::NodeConfig, service::ServiceConfig};
use arca_incus::{
    IncusError, Owned, ProjectFamily, UnixIncusWriter,
    exec::{ExecCommand, SERVICE_PROJECT, node_ok, valid_operation_id},
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
        if cfg.source["protocol"] == "oci" {
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
            let owned = self.check(target, &writer)?;
            {
                let mut hung = self.hung.lock().map_err(|_| ExecError::Audit)?;
                if hung
                    .get(&target.service.name)
                    .is_some_and(|uuid| Some(uuid) != target.uuid.as_ref())
                {
                    hung.remove(&target.service.name);
                }
                if hung.contains_key(&target.service.name) && !command.cleanup() {
                    return Err(ExecError::Refused("hung"));
                }
            }
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
        if attempted {
            let post_budget = until.map_or(Duration::from_secs(3), |end| {
                Duration::from_secs(3).min(end.saturating_duration_since(Instant::now()))
            });
            let post = self.writer(target, post_budget)?;
            let object = ObjectRef {
                kind: IncusKind::Instance,
                project: target.service.project.clone(),
                parent: None,
                name: target.service.instance.clone(),
                uuid: target.uuid.clone(),
            };
            if !post
                .read_class(&object)
                .is_ok_and(|fresh| fresh.uuid == target.uuid)
            {
                result = Err(ExecError::Refused("instance_changed"));
            }
        }
        // Try both deletions, even if waiting, parsing, reading, or appending the operation failed.
        let mut output_left = false;
        if let Some(op) = &op {
            let cleanup = self.writer(
                target,
                until.map_or(Duration::from_secs(6), |end| {
                    Duration::from_secs(6).min(end.saturating_duration_since(Instant::now()))
                }),
            )?;
            for stderr in [false, true] {
                if cleanup
                    .delete_service_output(&target.service.name, op, stderr)
                    .is_err()
                {
                    output_left = true;
                }
            }
        }
        let outcome = match &result {
            Ok(_) => "success",
            Err(ExecError::Refused(_)) => "refused",
            Err(ExecError::Audit) => "audit",
            Err(_) => "failed",
        };
        self.append(&json!({"kind":"end","id":id,"outcome":outcome,"exit":exit,"duration_ms":started.elapsed().as_millis(),"stdout_bytes":bytes,"output_left":output_left}))?;
        result
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
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}
