//! `instance_import` and `instance_import_undo`: a virtual machine moved in from an outside hypervisor, and taken out
//! again. `crates/arca-vmimport/DESIGN.md` states the design.
//!
//! Both verbs run only with the one-shot grant of an action proposal the owner approved. The plan is a pure function of
//! the approved arguments ([`arca_vmimport::plan_of`]), so the run does what the card said. The node's
//! `kernel.instanceImport` names the one source pool an import reads, the Ceph client that reads it and the Ceph cluster
//! it is on, which may be another cluster than the Incus pool's; a node without it refuses a proposal before the owner is
//! asked. The card names both clusters.

use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
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

/// Where the node keeps each Ceph cluster's config file, `<name>.conf`.
const CEPH_DIR: &str = "/etc/ceph";

/// How every `Ceph` row of an import's card starts. A journal's row that does not is not one this kernel wrote.
pub const CEPH_ROW_START: &str = "Arca reads the source pool ";

/// The target pool as the proposal reads it from Incus.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TargetPool {
    /// A Ceph pool on the cluster of this name. `one_fsid` is whether the two config files name one cluster under two
    /// names (the same `fsid`).
    Ceph { cluster: String, one_fsid: bool },
    /// A pool of another driver: the run stops before it makes anything.
    NotCeph(String),
    /// Incus did not answer, or has no such pool.
    Unread,
}

/// The `fsid` a Ceph config file names: the value of its `fsid` line, in any section. `None` when the file cannot be
/// read or names none.
fn conf_fsid(dir: &Path, cluster: &str) -> Option<String> {
    let text = std::fs::read_to_string(dir.join(format!("{cluster}.conf"))).ok()?;
    text.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        (key.trim() == "fsid")
            .then(|| {
                value
                    .split(['#', ';'])
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_string()
            })
            .filter(|fsid| !fsid.is_empty())
    })
}

/// The target pool from its Incus document, against the source's cluster `source`, with the config files in `dir`.
fn target_pool(read: Option<&Value>, source: &str, dir: &Path) -> TargetPool {
    let Some(read) = read else {
        return TargetPool::Unread;
    };
    let driver = read
        .get("driver")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if driver != "ceph" {
        return TargetPool::NotCeph(driver.to_string());
    }
    let cluster = read
        .pointer("/config/ceph.cluster_name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .unwrap_or("ceph")
        .to_string();
    let one_fsid = cluster != source
        && conf_fsid(dir, source).is_some_and(|fsid| conf_fsid(dir, &cluster) == Some(fsid));
    TargetPool::Ceph { cluster, one_fsid }
}

/// Checks a proposed `instance_import` against the node: the node must name a source pool, and every disk of the
/// source must be in it. The plan itself is made and checked by [`ActionKind::prepare`]. Answers the card's fact: the
/// `Ceph` row, which names the source's cluster and the target pool's, as the node's config and Incus say now. An Incus
/// that does not answer, or a pool that is not Ceph, refuses nothing: the row says so.
pub fn check_proposal(node: &NodeConfig, args: &Value) -> Result<String, CallError> {
    let import = node.kernel.instance_import.as_ref().ok_or_else(disabled)?;
    let plan = arca_vmimport::plan_of(args).map_err(CallError::Invalid)?;
    plan.check_source_pool(&import.source_pool)
        .map_err(|message| CallError::Refused {
            code: "InstanceImportRefused".to_string(),
            message,
        })?;
    let pool = &plan.request.pool;
    let read = VmImportWriter::new(node.incus.socket.clone())
        .with_deadline(POOL_READ)
        .pool(pool)
        .ok()
        .flatten();
    let target = target_pool(read.as_ref(), &import.ceph_cluster, Path::new(CEPH_DIR));
    let row = ceph_words(
        &import.ceph_cluster,
        &import.ceph_client,
        &import.source_pool,
        pool,
        &target,
    );
    Ok(ceph_fact(&row))
}

/// The card fact that carries the `Ceph` row `row`.
pub fn ceph_fact(row: &str) -> String {
    serde_json::to_string(&serde_json::json!({ "rows": [["Ceph", row]] })).unwrap_or_default()
}

/// The card's `Ceph` row: where the disks are read and where they are written. It says what Arca's own commands do:
/// they read the source pool and write only the images Incus made. The source client's Ceph grant is the operator's
/// to set; the row calls it set up to only read.
fn ceph_words(
    cluster: &str,
    client: &str,
    source_pool: &str,
    pool: &str,
    target: &TargetPool,
) -> String {
    let from = format!(
        "{CEPH_ROW_START}{source_pool} on the Ceph cluster {cluster} as client.{client}, set up to only read, and never writes it"
    );
    match target {
        TargetPool::Ceph { cluster: to, .. } if to == cluster => format!(
            "{from}. It writes only the images Incus made in the Incus pool {pool}, on the same cluster"
        ),
        TargetPool::Ceph {
            cluster: to,
            one_fsid: true,
        } => format!(
            "{from}. It writes only the images Incus made in the Incus pool {pool}, on the Ceph cluster {to}: the config files of {cluster} and {to} name one fsid, so this is one cluster under two names"
        ),
        TargetPool::Ceph { cluster: to, .. } => format!(
            "{from}. It copies to the Ceph cluster {to}, writing only the images Incus made in the Incus pool {pool}"
        ),
        TargetPool::NotCeph(driver) => format!(
            "{from}. The Incus pool {pool} is not a Ceph pool (its driver is {driver}), so the import stops before it makes anything"
        ),
        TargetPool::Unread => format!(
            "{from}. It writes only the images Incus makes in the Incus pool {pool}, on the cluster Incus names for it when the import runs"
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
    use super::{TargetPool, ceph_words, conf_fsid, target_pool};
    use serde_json::json;

    fn words(target: &TargetPool) -> String {
        ceph_words("source-b", "arca-import", "src-pool", "remote", target)
    }

    #[test]
    fn the_ceph_row_names_both_clusters() {
        let other = TargetPool::Ceph {
            cluster: "ceph".into(),
            one_fsid: false,
        };
        assert_eq!(
            words(&other),
            "Arca reads the source pool src-pool on the Ceph cluster source-b as client.arca-import, set up to only read, and never writes it. It copies to the Ceph cluster ceph, writing only the images Incus made in the Incus pool remote"
        );
        let same = TargetPool::Ceph {
            cluster: "source-b".into(),
            one_fsid: false,
        };
        assert!(words(&same).ends_with("in the Incus pool remote, on the same cluster"));
        let renamed = TargetPool::Ceph {
            cluster: "ceph".into(),
            one_fsid: true,
        };
        assert!(words(&renamed).ends_with("so this is one cluster under two names"));
        assert!(
            words(&TargetPool::NotCeph("dir".into())).ends_with(
                "The Incus pool remote is not a Ceph pool (its driver is dir), so the import stops before it makes anything"
            )
        );
        assert!(
            words(&TargetPool::Unread)
                .ends_with("on the cluster Incus names for it when the import runs")
        );
        for target in [other, same, renamed, TargetPool::Unread] {
            assert!(words(&target).starts_with(super::CEPH_ROW_START));
        }
    }

    #[test]
    fn the_target_pool_is_read_from_its_driver_its_cluster_name_and_the_fsids() {
        let dir = std::env::temp_dir().join(format!("arca-vmimport-fsid-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a directory");
        std::fs::write(
            dir.join("ceph.conf"),
            "[global]\n\tfsid = 0b1e2c3d-0000-4000-8000-00000000000a\n\tmon_host = 192.0.2.1\n",
        )
        .expect("a conf");
        std::fs::write(
            dir.join("same.conf"),
            "[global]\nfsid=0b1e2c3d-0000-4000-8000-00000000000a # the one cluster\n",
        )
        .expect("a conf");
        std::fs::write(
            dir.join("other.conf"),
            "[global]\nfsid = 0b1e2c3d-0000-4000-8000-00000000000b\n",
        )
        .expect("a conf");
        assert_eq!(
            conf_fsid(&dir, "same").as_deref(),
            Some("0b1e2c3d-0000-4000-8000-00000000000a")
        );
        assert_eq!(conf_fsid(&dir, "missing"), None);
        let ceph = json!({ "driver": "ceph", "config": {} });
        assert_eq!(
            target_pool(Some(&ceph), "same", &dir),
            TargetPool::Ceph {
                cluster: "ceph".into(),
                one_fsid: true
            }
        );
        assert_eq!(
            target_pool(Some(&ceph), "other", &dir),
            TargetPool::Ceph {
                cluster: "ceph".into(),
                one_fsid: false
            }
        );
        let named = json!({ "driver": "ceph", "config": { "ceph.cluster_name": "cluster-b" } });
        assert_eq!(
            target_pool(Some(&named), "other", &dir),
            TargetPool::Ceph {
                cluster: "cluster-b".into(),
                one_fsid: false
            }
        );
        assert_eq!(
            target_pool(
                Some(&json!({ "driver": "zfs", "config": {} })),
                "other",
                &dir
            ),
            TargetPool::NotCeph("zfs".into())
        );
        assert_eq!(target_pool(None, "other", &dir), TargetPool::Unread);
        std::fs::remove_dir_all(&dir).expect("removed");
    }

    #[test]
    fn the_writer_and_the_plan_name_one_mark_and_one_acl_prefix() {
        assert_eq!(arca_incus::IMPORT_KEY, arca_vmimport::IMPORT_KEY);
        assert_eq!(arca_incus::IMPORT_ACL_PREFIX, arca_vmimport::ACL_PREFIX);
    }
}
