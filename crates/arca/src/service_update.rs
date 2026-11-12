//! `service_update`: a running service made from an image by digest moves to another digest of that image in one call.
//!
//! The steps, each journalled in [`SERVICE_UPDATES_FILE`] (an append-only file of JSON lines in the state directory,
//! one `begin` line before anything is sent to Incus, one line per step and one `end` line; no line holds a secret):
//!
//! 1. the instance is stopped, when it runs, and a snapshot of it is taken;
//! 2. its root is rebuilt from the new digest. Incus keeps the config, the devices (the data volume among them), the
//!    uuid, the secrets and the firewall rules; only the root is the new image's;
//! 3. it is started;
//! 4. the health check: the first port the service's record names answers TCP, within a timeout;
//! 5. the new digest is recorded in the service's desired state, so a restart keeps it.
//!
//! A failure at any step puts the old version back: the snapshot is restored (or, in a project that does not allow
//! snapshots, the root is rebuilt from the old digest), the old version is started when it was running, and the answer
//! is the error and the words "rolled back". Nothing is asked of the owner: the verb is a doctor's, and works in a
//! tenant as it does outside one. The snapshot holds the root, the config and the devices of the instance; the data
//! volume is the service's own and is not snapshotted, so what the new version wrote there stays.
use super::*;
use crate::service::ServiceConfig;
use arca_incus::{IncusError, InstanceState, StateAction, StateChanger};
use arca_kernel::desired::Entry;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::net::{SocketAddr, TcpStream};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

/// The update journal's file name in the state directory.
pub const SERVICE_UPDATES_FILE: &str = "service-updates.jsonl";
/// A journal past this size is set aside as `<name>.1`, replacing the one set aside before, and a new one is begun.
const MAX_JOURNAL_BYTES: u64 = 8 * 1024 * 1024;
/// How long the first port has to answer, when the call states no timeout.
pub const DEFAULT_HEALTH_SECONDS: u64 = 120;
/// The longest timeout a call may state.
pub const MAX_HEALTH_SECONDS: u64 = 900;
/// One TCP attempt of the health check gives up after this long.
const CONNECT_WAIT: Duration = Duration::from_secs(2);

/// One attempt of the health check: whether `address` answers on `port` now.
pub type Probe = Arc<dyn Fn(&str, u16) -> bool + Send + Sync>;

/// What a node's `service_update` does at its edges: how a port is tried and how long is waited between two tries.
#[derive(Clone)]
pub struct UpdateOptions {
    pub probe: Probe,
    pub pace: Duration,
}

impl Default for UpdateOptions {
    fn default() -> Self {
        Self {
            probe: Arc::new(|address, port| {
                address.parse::<std::net::IpAddr>().ok().is_some_and(|ip| {
                    TcpStream::connect_timeout(&SocketAddr::new(ip, port), CONNECT_WAIT).is_ok()
                })
            }),
            pace: Duration::from_secs(1),
        }
    }
}

/// The services being updated now, so one service is never moved by two calls at once.
fn updating() -> &'static Mutex<BTreeSet<String>> {
    static SET: std::sync::OnceLock<Mutex<BTreeSet<String>>> = std::sync::OnceLock::new();
    SET.get_or_init(|| Mutex::new(BTreeSet::new()))
}

struct Claim(String);

impl Claim {
    fn take(id: &str) -> Option<Claim> {
        let mut set = updating().lock().unwrap_or_else(PoisonError::into_inner);
        set.insert(id.to_string()).then(|| Claim(id.to_string()))
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        updating()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.0);
    }
}

/// `sha256:` and 64 lower-case hex digits.
pub fn digest_ok(text: &str) -> bool {
    text.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

/// `source` with its image's digest replaced: `<repository>@sha256:<new>`. `None` when `source` is not an OCI image by
/// digest on a registry, the one kind of source an update moves.
pub fn source_with(source: &Value, digest: &str) -> Option<Value> {
    let fields = source.as_object()?;
    if fields.len() != 5
        || source["type"] != "image"
        || source["mode"] != "pull"
        || source["protocol"] != "oci"
        || !source["server"].as_str()?.starts_with("https://")
    {
        return None;
    }
    let (repository, old) = source["alias"].as_str()?.rsplit_once("@sha256:")?;
    if repository.is_empty() || !digest_ok(&format!("sha256:{old}")) {
        return None;
    }
    let mut next = source.clone();
    next["alias"] = json!(format!("{repository}@{digest}"));
    Some(next)
}

fn refused(message: impl Into<String>) -> CallError {
    CallError::Refused {
        code: "refused".into(),
        message: message.into(),
    }
}

/// Why one step failed.
struct Failure {
    step: &'static str,
    why: String,
}

fn incus_words(error: &IncusError) -> String {
    match error {
        IncusError::Status { code, message } => format!("Incus answered {code}: {message}"),
        IncusError::Timeout => "Incus did not answer in time".to_string(),
        IncusError::Refused(why) => why.clone(),
        other => format!("Incus: {other}"),
    }
}

struct Journal {
    path: PathBuf,
    id: String,
    service: String,
    clock: Clock,
}

impl Journal {
    fn line(&self, step: &str, ok: bool, detail: Value) -> std::io::Result<()> {
        let row = json!({
            "update": self.id,
            "at": (self.clock)(),
            "service": self.service,
            "step": step,
            "ok": ok,
            "detail": detail,
        });
        match fs::symlink_metadata(&self.path) {
            Ok(meta) if !meta.is_file() || meta.permissions().mode() & 0o777 != 0o600 => {
                return Err(std::io::Error::other(
                    "the service update journal is not a private file",
                ));
            }
            Ok(meta) if meta.len() > MAX_JOURNAL_BYTES => {
                let mut aside = self.path.clone().into_os_string();
                aside.push(".1");
                fs::rename(&self.path, aside)?;
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.path)?;
        let mut bytes = serde_json::to_vec(&row).map_err(std::io::Error::other)?;
        bytes.push(b'\n');
        file.write_all(&bytes)?;
        file.sync_all()
    }

    /// A line after the first: a journal that cannot be written is said on the log and does not stop an update that is
    /// already under way, which must still be able to put the old version back.
    fn step(&self, step: &str, ok: bool, detail: Value) {
        if let Err(error) = self.line(step, ok, detail) {
            eprintln!("arca: the service update journal could not be written: {error}");
        }
    }
}

/// How an update puts the old version back.
enum Back {
    /// The snapshot taken before the rebuild.
    Snapshot(String),
    /// A project that does not allow snapshots: the root is rebuilt from the old image.
    OldImage,
}

impl ServiceHandler {
    /// `service_update`: moves the service `name` to `digest`. `entries` are the ones the call may see, a tenant's when
    /// it names one.
    pub(super) fn call_update(&self, args: &Value, entries: &[Entry]) -> Result<Value, CallError> {
        let name = service_name(args)?;
        let digest = args["digest"]
            .as_str()
            .filter(|digest| digest_ok(digest))
            .ok_or_else(|| {
                CallError::Invalid("digest: expected sha256: and 64 lower-case hex digits".into())
            })?
            .to_string();
        let seconds = match args.get("timeoutSeconds") {
            None => DEFAULT_HEALTH_SECONDS,
            Some(value) => value
                .as_u64()
                .filter(|seconds| (1..=MAX_HEALTH_SECONDS).contains(seconds))
                .ok_or_else(|| {
                    CallError::Invalid(format!(
                        "timeoutSeconds: expected a whole number from 1 to {MAX_HEALTH_SECONDS}"
                    ))
                })?,
        };
        let Some((entry, config)) = service_entries(entries)
            .into_iter()
            .find(|(_, config)| config.name == name)
        else {
            return Err(service_not_found(format!("service {name}: not found")));
        };
        let said =
            |why: &str| refused(format!("update service {name}: {why}. Nothing was changed"));
        if config.adopted {
            return Err(service_wiring::adopted(&name));
        }
        if config.function.is_some() || config.vm.is_some() {
            return Err(said(
                "only a container service made from an image by digest can be updated",
            ));
        }
        let Some(new_source) = source_with(&config.source, &digest) else {
            return Err(said(
                "the service is not made from an OCI image by digest on a registry",
            ));
        };
        let Some(port) = config.ports.first().copied() else {
            return Err(said(
                "its template declares no port, so there is nothing to check after the start",
            ));
        };
        if new_source == config.source {
            return Ok(json!({
                "updated": false,
                "service": name,
                "digest": digest,
                "message": "the service already runs this digest; nothing was changed",
            }));
        }
        let claim = Claim::take(&format!("{}\n{}", self.cfg.state_dir.display(), entry.id.0))
            .ok_or_else(|| said("another update of this service is running"))?;
        let family = arca_incus::ProjectFamily::new(
            [arca_netproject::PROJECT],
            self.cfg.kernel.droidd_projects.iter().map(String::as_str),
        )
        .map_err(|_| internal())?;
        let mut changer = StateChanger::new(self.cfg.incus.socket.clone(), family);
        if config.tenant.is_some() {
            changer = changer
                .with_tenants([config.project.as_str()])
                .map_err(|_| internal())?;
        }
        let seen = changer
            .read(&config.project, &config.instance)
            .map_err(|error| {
                said(&format!(
                    "its instance was not read ({})",
                    incus_words(&error)
                ))
            })?;
        let journalled = lock(&self.kernel)?.service_uuid(&entry.id, &config.instance);
        if seen.kind != "container" || journalled.is_none() || journalled != seen.uuid {
            return Err(said("its instance is not the one Arca created for it"));
        }
        let journal = Journal {
            path: self.cfg.state_dir.join(SERVICE_UPDATES_FILE),
            id: format!("{}-{}", entry.id.0, (self.clock)()),
            service: name.clone(),
            clock: self.clock.clone(),
        };
        journal
            .line(
                "begin",
                true,
                json!({
                    "from": config.source["alias"],
                    "to": digest,
                    "timeoutSeconds": seconds,
                    "tenant": config.tenant.as_ref().map(|tenant| tenant.id.clone()),
                }),
            )
            .map_err(|_| said("the update journal could not be written"))?;
        // Nothing in a tenant's project is fetched with the node's credential.
        let registry = if config.tenant.is_some() {
            None
        } else {
            config.source["server"]
                .as_str()
                .and_then(|server| crate::registry::credential(&self.cfg, server))
        };
        let run = Run {
            handler: self,
            changer: &changer,
            config: &config,
            entry: &entry,
            new_source: &new_source,
            digest: &digest,
            port,
            seconds,
            registry: registry.as_ref(),
            journal: &journal,
            was_running: seen.is_running(),
        };
        let outcome = run.go(seen);
        drop(claim);
        outcome
    }
}

struct Run<'a> {
    handler: &'a ServiceHandler,
    changer: &'a StateChanger,
    config: &'a ServiceConfig,
    entry: &'a Entry,
    new_source: &'a Value,
    digest: &'a str,
    port: u16,
    seconds: u64,
    registry: Option<&'a arca_incus::RegistryCredential>,
    journal: &'a Journal,
    was_running: bool,
}

impl Run<'_> {
    fn go(&self, seen: InstanceState) -> Result<Value, CallError> {
        let name = &self.config.name;
        let mut instance = seen;
        let mut back: Option<Back> = None;
        let failure = match self.forward(&mut instance, &mut back) {
            Ok(snapshot) => {
                self.journal
                    .step("end", true, json!({ "digest": self.digest }));
                return Ok(json!({
                    "updated": true,
                    "service": name,
                    "digest": self.digest,
                    "snapshot": snapshot,
                    "message": format!(
                        "service {name} runs {} and its port {} answered",
                        self.digest, self.port,
                    ),
                }));
            }
            Err(failure) => failure,
        };
        self.journal
            .step(failure.step, false, json!({ "why": failure.why }));
        let Some(back) = back else {
            // Nothing was changed but the power state: the old version is started again when it ran.
            let restarted = self.start_again(&mut instance);
            self.journal.step(
                "end",
                false,
                json!({ "rolledBack": false, "changedAnything": false }),
            );
            return Err(refused(format!(
                "update service {name}: {} failed: {}. Nothing was changed{}",
                failure.step,
                failure.why,
                if restarted.is_ok() {
                    ""
                } else {
                    ", but the service could not be started again: instance_start starts it"
                },
            )));
        };
        match self.roll_back(&mut instance, &back) {
            Ok(()) => {
                self.journal
                    .step("end", false, json!({ "rolledBack": true }));
                Err(CallError::Refused {
                    code: "rolled_back".into(),
                    message: format!(
                        "update service {name} to {}: {} failed: {}. The update was rolled back: the old version is {}",
                        self.digest,
                        failure.step,
                        failure.why,
                        if self.was_running {
                            "running again"
                        } else {
                            "in place, and the service is stopped as it was"
                        },
                    ),
                })
            }
            Err(second) => {
                self.journal.step(
                    "end",
                    false,
                    json!({ "rolledBack": false, "why": second.why }),
                );
                Err(CallError::Refused {
                    code: "rollback_failed".into(),
                    message: format!(
                        "update service {name} to {}: {} failed: {}. It was NOT rolled back: {} failed: {}. The service is stopped; service_status shows what is there",
                        self.digest, failure.step, failure.why, second.step, second.why,
                    ),
                })
            }
        }
    }

    fn started(&self, instance: &mut InstanceState) -> Result<(), Failure> {
        self.changer
            .change(instance, StateAction::Start)
            .map(|state| *instance = state)
            .map_err(|error| Failure {
                step: "start",
                why: incus_words(&error),
            })
    }

    fn start_again(&self, instance: &mut InstanceState) -> Result<(), Failure> {
        if self.was_running && !instance.is_running() {
            self.started(instance)?;
        }
        Ok(())
    }

    fn stopped(&self, instance: &mut InstanceState) -> Result<(), Failure> {
        if instance.is_stopped() {
            return Ok(());
        }
        self.changer
            .change(instance, StateAction::Stop)
            .map(|state| *instance = state)
            .map_err(|error| Failure {
                step: "stop",
                why: incus_words(&error),
            })
    }

    /// Steps 1 to 5. `back` is set as soon as something that must be undone is about to be done.
    fn forward(
        &self,
        instance: &mut InstanceState,
        back: &mut Option<Back>,
    ) -> Result<Option<String>, Failure> {
        self.stopped(instance)?;
        self.journal.step("stop", true, json!({}));
        let snapshot = format!("pre-update-{}", (self.journal.clock)());
        let held = match self.changer.snapshot_instance(instance, &snapshot) {
            Ok(()) => {
                self.journal
                    .step("snapshot", true, json!({ "snapshot": snapshot }));
                Some(snapshot)
            }
            // A project that forbids snapshots (a tenant's does): the old version is put back by a rebuild from its
            // own digest instead.
            Err(IncusError::Status { code: 403, message }) => {
                self.journal.step(
                    "snapshot",
                    false,
                    json!({ "skipped": "the project does not allow snapshots", "incus": message }),
                );
                None
            }
            Err(error) => {
                return Err(Failure {
                    step: "snapshot",
                    why: incus_words(&error),
                });
            }
        };
        *back = Some(match &held {
            Some(name) => Back::Snapshot(name.clone()),
            None => Back::OldImage,
        });
        self.changer
            .rebuild_from_registry(instance, self.new_source, self.registry)
            .map_err(|error| Failure {
                step: "rebuild",
                why: incus_words(&error),
            })?;
        self.journal
            .step("rebuild", true, json!({ "digest": self.digest }));
        self.started(instance)?;
        self.journal.step("start", true, json!({}));
        self.healthy()?;
        self.journal
            .step("health", true, json!({ "port": self.port }));
        self.record()?;
        self.journal.step("record", true, json!({}));
        Ok(held)
    }

    /// The first port answers TCP within the timeout.
    fn healthy(&self) -> Result<(), Failure> {
        let options = &self.handler.update;
        let begun = Instant::now();
        let limit = Duration::from_secs(self.seconds);
        loop {
            if (options.probe)(&self.config.ipv4, self.port) {
                return Ok(());
            }
            if begun.elapsed() >= limit {
                return Err(Failure {
                    step: "health check",
                    why: format!(
                        "port {} did not answer within {} second{}",
                        self.port,
                        self.seconds,
                        if self.seconds == 1 { "" } else { "s" }
                    ),
                });
            }
            std::thread::sleep(options.pace);
        }
    }

    /// The new digest goes into the service's record, so a restart keeps it.
    fn record(&self) -> Result<(), Failure> {
        let failed = |why: &str| Failure {
            step: "record",
            why: why.to_string(),
        };
        let _write = self
            .handler
            .writes
            .lock()
            .map_err(|_| failed("the node's write gate is not usable"))?;
        let mut kernel = self
            .handler
            .kernel
            .lock()
            .map_err(|_| failed("the kernel is not usable"))?;
        // The record as the kernel holds it now: a change that came meanwhile is kept.
        let mut next = service_entries(kernel.entries())
            .into_iter()
            .find(|(entry, _)| entry.id == self.entry.id)
            .map(|(_, config)| config)
            .ok_or_else(|| failed("the service's record is gone"))?;
        if next.source != self.config.source {
            return Err(failed("the service's record changed while it was updated"));
        }
        next.source = self.new_source.clone();
        let change =
            service_wiring::change(&next).map_err(|_| failed("the record could not be made"))?;
        let ids = [self.entry.id.0.as_str()];
        let outcomes = kernel
            .propose(vec![change], (self.handler.clock)())
            .map_err(|_| failed("the node could not save the change"))?;
        if service_wiring::failed_for(&outcomes, &ids) {
            return Err(failed(
                &service_wiring::failure_reason(&outcomes, &ids)
                    .unwrap_or_else(|| "the node did not apply the change".to_string()),
            ));
        }
        Ok(())
    }

    /// Puts the old version back, and starts it when it ran before the update. The record was never changed before
    /// the health check passed, so it still names the old digest.
    fn roll_back(&self, instance: &mut InstanceState, back: &Back) -> Result<(), Failure> {
        self.stopped(instance)?;
        match back {
            Back::Snapshot(snapshot) => {
                self.changer
                    .restore_snapshot(instance, snapshot)
                    .map_err(|error| Failure {
                        step: "restore",
                        why: incus_words(&error),
                    })?;
                self.journal
                    .step("restore", true, json!({ "snapshot": snapshot }));
            }
            Back::OldImage => {
                self.changer
                    .rebuild_from_registry(instance, &self.config.source, self.registry)
                    .map_err(|error| Failure {
                        step: "rebuild back",
                        why: incus_words(&error),
                    })?;
                self.journal.step("rebuild back", true, json!({}));
            }
        }
        *instance = self
            .changer
            .read(&instance.project, &instance.name)
            .map_err(|error| Failure {
                step: "read",
                why: incus_words(&error),
            })?;
        self.start_again(instance)?;
        if self.was_running {
            self.journal.step("start", true, json!({ "old": true }));
        }
        Ok(())
    }
}
