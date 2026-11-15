//! `instance_import` and `instance_import_undo`: a virtual machine moved in from an outside hypervisor, and taken out
//! again. `crates/arca-vmimport/DESIGN.md` states the design.
//!
//! Both verbs run only with the one-shot grant of an action proposal the owner approved. The plan is a pure function of
//! the approved arguments ([`arca_vmimport::plan_of`]), so the run does what the card said. The node's
//! `kernel.instanceImport` names the one source pool an import reads, the Ceph client that reads it and the Ceph cluster
//! it is on, which may be another cluster than the Incus pool's; a node without it refuses a proposal before the owner is
//! asked. The card names both clusters.

use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arca_incus::VmImportWriter;
use arca_mcp::{CallError, Caller, ToolHandler};
use arca_vmimport::{DiskCopy, Held, ImportIncus, Place, PoolInfo, Probe, ProcessRunner, RbdCopy};
use serde_json::Value;

use crate::action_proposal::ActionKind;
use crate::config::NodeConfig;
use crate::kernel::Kernel;

/// What an import runs through: the Incus writer, the disk copy and the read-back's probe, with the node's source pool.
pub struct ImportBackend {
    writer: VmImportWriter,
    source_pool: String,
    copy: Arc<dyn DiskCopy>,
    probe: Arc<dyn Probe + Send + Sync>,
}

impl ImportBackend {
    pub fn new(
        socket: PathBuf,
        source_pool: &str,
        copy: Arc<dyn DiskCopy>,
        probe: Arc<dyn Probe + Send + Sync>,
    ) -> Self {
        Self {
            writer: VmImportWriter::new(socket),
            source_pool: source_pool.to_string(),
            copy,
            probe,
        }
    }

    /// The backend of a served node: `rbd` processes and real TCP connections. `None` without `kernel.instanceImport`.
    pub fn from_node(node: &NodeConfig) -> Option<Self> {
        let import = node.kernel.instance_import.as_ref()?;
        Some(Self::new(
            node.incus.socket.clone(),
            &import.source_pool,
            Arc::new(RbdCopy::new(
                &import.ceph_cluster,
                &import.ceph_client,
                ProcessRunner,
            )),
            Arc::new(TcpProbe),
        ))
    }
}

/// The read-back's probe: a TCP connection with a three-second limit, and five seconds between tries.
pub struct TcpProbe;

impl Probe for TcpProbe {
    fn port_open(&self, ip: Ipv4Addr, port: u16) -> bool {
        TcpStream::connect_timeout(&SocketAddr::from((ip, port)), Duration::from_secs(3)).is_ok()
    }

    fn pause(&self) {
        std::thread::sleep(Duration::from_secs(5));
    }
}

/// The Incus side of [`arca_vmimport::run`], through the import writer.
struct Incus<'a>(&'a VmImportWriter);

fn said(error: arca_incus::IncusError) -> String {
    error.to_string()
}

impl Incus<'_> {
    /// The project an object of a kind with this feature lives in: the project itself when the feature is on, else
    /// `default`.
    fn home(&self, project: &str, feature: &str) -> Result<String, String> {
        let read = self
            .0
            .project(project)
            .map_err(said)?
            .ok_or_else(|| format!("Incus has no project {project}"))?;
        let on = read
            .pointer("/config")
            .and_then(|config| config.get(feature))
            .and_then(Value::as_str)
            == Some("true");
        Ok(if on {
            project.to_string()
        } else {
            "default".to_string()
        })
    }
}

impl ImportIncus for Incus<'_> {
    fn place(&self, project: &str, pool: &str, bridge: Option<&str>) -> Result<Place, String> {
        let read = self
            .0
            .pool(pool)
            .map_err(said)?
            .ok_or_else(|| format!("Incus has no storage pool {pool}"))?;
        let config = |key: &str| {
            read.pointer("/config")
                .and_then(|config| config.get(key))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };
        let pool_info = PoolInfo {
            driver: read
                .get("driver")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
            osd_pool: config("ceph.osd.pool_name").unwrap_or_else(|| pool.to_string()),
            cluster: config("ceph.cluster_name").unwrap_or_else(|| "ceph".to_string()),
            client: config("ceph.user.name").unwrap_or_else(|| "admin".to_string()),
        };
        let managed = match bridge {
            None => None,
            Some(bridge) => Some(
                self.0
                    .network(bridge)
                    .map_err(said)?
                    .ok_or_else(|| format!("Incus knows no network or host bridge {bridge}"))?
                    .get("managed")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            ),
        };
        let (volume_project, acl_project) = self.homes(project)?;
        Ok(Place {
            pool: pool_info,
            volume_project,
            acl_project,
            managed,
        })
    }

    fn homes(&self, project: &str) -> Result<(String, String), String> {
        Ok((
            self.home(project, "features.storage.volumes")?,
            self.home(project, "features.networks")?,
        ))
    }

    fn instance_exists(&self, project: &str, name: &str) -> Result<bool, String> {
        self.0
            .instance(project, name)
            .map(|read| read.is_some())
            .map_err(said)
    }

    fn create_acl(&self, project: &str, body: &Value) -> Result<(), String> {
        self.0.create_acl(project, body).map_err(said)
    }

    fn create_volume(&self, project: &str, pool: &str, body: &Value) -> Result<(), String> {
        self.0.create_volume(project, pool, body).map_err(said)
    }

    fn create_instance(&self, project: &str, body: &Value) -> Result<(), String> {
        self.0.create_instance(project, body).map_err(said)
    }

    fn start(&self, project: &str, name: &str) -> Result<(), String> {
        self.0.start(project, name).map_err(said)
    }

    fn agent_answers(&self, project: &str, name: &str) -> bool {
        self.0.agent_answers(project, name)
    }

    fn remove_instance(&self, project: &str, name: &str) -> Result<Option<Held>, String> {
        let Some(read) = self.0.remove_instance(project, name).map_err(said)? else {
            return Ok(None);
        };
        let devices = read
            .get("devices")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let mut held = Held::default();
        for device in devices.values() {
            let text = |key: &str| device.get(key).and_then(Value::as_str).map(str::to_string);
            if text("type").as_deref() == Some("disk")
                && let (Some(pool), Some(source)) = (text("pool"), text("source"))
            {
                held.volumes.push((pool, source));
            }
            if let Some(acls) = text("security.acls") {
                for acl in acls.split(',').map(str::trim) {
                    if acl.starts_with(arca_vmimport::ACL_PREFIX)
                        && !held.acls.iter().any(|seen| seen == acl)
                    {
                        held.acls.push(acl.to_string());
                    }
                }
            }
        }
        Ok(Some(held))
    }

    fn remove_volume(&self, project: &str, pool: &str, name: &str) -> Result<bool, String> {
        self.0.remove_volume(project, pool, name).map_err(said)
    }

    fn remove_acl(&self, project: &str, name: &str) -> Result<bool, String> {
        self.0.remove_acl(project, name).map_err(said)
    }
}

fn disabled() -> CallError {
    CallError::Refused {
        code: "InstanceImportDisabled".to_string(),
        message: "this node moves no VM in from an outside hypervisor: its config has no kernel.instanceImport".to_string(),
    }
}

/// How long the proposal waits for Incus to say which Ceph cluster the target pool is on.
const POOL_READ: Duration = Duration::from_secs(5);

/// Checks a proposed `instance_import` against the node: the node must name a source pool, and every disk of the
/// source must be in it. The plan itself is made and checked by [`ActionKind::prepare`]. Answers the card's fact: the
/// `Ceph` row, which names the source's cluster and the target pool's, as Incus says now. An Incus that does not answer
/// refuses nothing: the row then says the target's cluster is read when the import runs.
pub fn check_proposal(node: &NodeConfig, args: &Value) -> Result<String, CallError> {
    let import = node.kernel.instance_import.as_ref().ok_or_else(disabled)?;
    let plan = arca_vmimport::plan_of(args).map_err(CallError::Invalid)?;
    plan.check_source_pool(&import.source_pool)
        .map_err(|message| CallError::Refused {
            code: "InstanceImportRefused".to_string(),
            message,
        })?;
    let pool = &plan.request.pool;
    let target = VmImportWriter::new(node.incus.socket.clone())
        .with_deadline(POOL_READ)
        .pool(pool)
        .ok()
        .flatten()
        .map(|read| {
            read.pointer("/config/ceph.cluster_name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .unwrap_or("ceph")
                .to_string()
        });
    let row = ceph_words(
        &import.ceph_cluster,
        &import.ceph_client,
        &import.source_pool,
        pool,
        target.as_deref(),
    );
    Ok(serde_json::to_string(&serde_json::json!({ "rows": [["Ceph", row]] })).unwrap_or_default())
}

/// The card's `Ceph` row: where the disks are read and where they are written. `target` is the Incus pool's cluster,
/// `None` when Incus did not say.
fn ceph_words(
    cluster: &str,
    client: &str,
    source_pool: &str,
    pool: &str,
    target: Option<&str>,
) -> String {
    let from = format!(
        "Read from the Ceph cluster {cluster} (the pool {source_pool}, as client.{client}, which may only read)"
    );
    match target {
        Some(target) if target == cluster => {
            format!("{from}, and written on the same cluster into the Incus pool {pool}")
        }
        Some(target) => format!(
            "{from}, and copied to the Ceph cluster {target}, into the Incus pool {pool}. The source cluster is never written"
        ),
        None => format!(
            "{from}, and written into the Incus pool {pool}, on the cluster Incus names for it when the import runs. The source cluster is never written"
        ),
    }
}

/// A proposal's arguments as anyone who lists it sees them: `plan_list`, the approval page (and the linked page hosts that
/// read it) and the `action_propose` answer. An import's description is shown without its user data, which can hold a
/// password; the proposal keeps, binds and runs the arguments as they were stored, so the digest is unchanged.
pub fn shown_args(action: &str, args: &Value) -> Value {
    let mut shown = args.clone();
    if action == ActionKind::InstanceImport.as_str()
        && let Some(source) = args.get("source").and_then(Value::as_str)
    {
        shown["source"] = Value::String(arca_vmimport::reduce_shown(source).unwrap_or_default());
    }
    shown
}

/// `instance_import` and `instance_import_undo`, each run only from an approved proposal.
struct ImportHandler {
    action: ActionKind,
    kernel: Arc<Mutex<Kernel>>,
    backend: Option<Arc<ImportBackend>>,
}

impl ToolHandler for ImportHandler {
    fn call(&self, args: Value, _caller: &Caller) -> Result<Value, CallError> {
        let Some(backend) = &self.backend else {
            return Err(disabled());
        };
        let mut bare = args;
        let grant = bare
            .as_object_mut()
            .and_then(|object| object.remove("approval"))
            .and_then(|value| value.as_str().map(str::to_string))
            .ok_or_else(|| CallError::Invalid("approval is required".to_string()))?;
        let args = self
            .action
            .prepare(&bare, None)
            .map_err(CallError::Invalid)?;
        {
            let mut kernel = self
                .kernel
                .lock()
                .map_err(|_| CallError::Internal("the kernel lock is poisoned".to_string()))?;
            if !kernel.consume_action_grant(self.action.as_str(), &args, &grant) {
                return Err(CallError::Refused {
                    code: "NotApproved".to_string(),
                    message: "this action runs only from an approved proposal".to_string(),
                });
            }
        }
        let incus = Incus(&backend.writer);
        match self.action {
            ActionKind::InstanceImport => {
                let plan = arca_vmimport::plan_of(&args).map_err(CallError::Invalid)?;
                arca_vmimport::run(
                    &plan,
                    &backend.source_pool,
                    &incus,
                    backend.copy.as_ref(),
                    backend.probe.as_ref(),
                )
                .map_err(|failure| CallError::Refused {
                    code: "InstanceImportFailed".to_string(),
                    message: failure.words(),
                })
            }
            _ => arca_vmimport::undo(
                &incus,
                args["project"].as_str().unwrap_or_default(),
                args["name"].as_str().unwrap_or_default(),
            )
            .map_err(|message| CallError::Refused {
                code: "InstanceImportUndoFailed".to_string(),
                message,
            }),
        }
    }
}

/// The two verbs' handlers. `backend` is `None` on a node without `kernel.instanceImport`, where both refuse.
pub fn handlers(
    kernel: Arc<Mutex<Kernel>>,
    backend: Option<Arc<ImportBackend>>,
) -> Vec<(&'static str, Arc<dyn ToolHandler>)> {
    [ActionKind::InstanceImport, ActionKind::InstanceImportUndo]
        .into_iter()
        .map(|action| {
            (
                action.as_str(),
                Arc::new(ImportHandler {
                    action,
                    kernel: Arc::clone(&kernel),
                    backend: backend.clone(),
                }) as Arc<dyn ToolHandler>,
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_ceph_row_names_both_clusters() {
        assert_eq!(
            super::ceph_words(
                "source-b",
                "arca-import",
                "src-pool",
                "remote",
                Some("ceph")
            ),
            "Read from the Ceph cluster source-b (the pool src-pool, as client.arca-import, which may only read), and copied to the Ceph cluster ceph, into the Incus pool remote. The source cluster is never written"
        );
        assert_eq!(
            super::ceph_words("ceph", "arca-import", "src-pool", "remote", Some("ceph")),
            "Read from the Ceph cluster ceph (the pool src-pool, as client.arca-import, which may only read), and written on the same cluster into the Incus pool remote"
        );
        assert!(
            super::ceph_words("source-b", "arca-import", "src-pool", "remote", None)
                .contains("on the cluster Incus names for it when the import runs")
        );
    }

    #[test]
    fn the_writer_and_the_plan_name_one_mark_and_one_acl_prefix() {
        assert_eq!(arca_incus::IMPORT_KEY, arca_vmimport::IMPORT_KEY);
        assert_eq!(arca_incus::IMPORT_ACL_PREFIX, arca_vmimport::ACL_PREFIX);
    }
}
