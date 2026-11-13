//! `service_update`: a running service made from an image by digest moves to another digest of that image in one call.
//!
//! The steps, each journalled in [`SERVICE_UPDATES_FILE`] (an append-only file of JSON lines in the state directory,
//! one `begin` line before anything is sent to Incus, one line per step and one `end` line; no line holds a secret):
//!
//! 1. the instance is stopped, when it runs, and a snapshot of it is taken where the project allows one (an Arca
//!    project never does, so on a node as Arca makes it there is none);
//! 2. its root is rebuilt from the new digest. Incus keeps the config, the devices (the data volume among them), the
//!    uuid, the secrets and the firewall rules; only the root is the new image's;
//! 3. it is started;
//! 4. the health check, within a timeout: a service with a host port (`lan`) is tried through that port's proxy, a
//!    service whose first port is its droidd port is tried at its address, and any other must stay running for a few
//!    seconds, since the node may reach nothing else of it;
//! 5. the new digest is recorded in the service's desired state, so a restart keeps it.
//!
//! A service whose template declares a data volume that its instance has not keeps its data on the root, which the
//! rebuild replaces: it is refused before anything is written, unless the call states `eraseData`, which only an
//! approved proposal may (see [`ServiceHandler::call_update`]).
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
use std::io::{Read as _, Write as _};
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
/// How long Incus may take over one step, a registry pull included: what a create allows for its fetch.
const STEP_DEADLINE: Duration = Duration::from_secs(600);
/// A service the node cannot reach has to stay running this long (at most the call's timeout).
const STAY_RUNNING_SECONDS: u64 = 10;
/// The old version, started again after a rollback, is given this long to answer.
const OLD_ANSWERS_SECONDS: u64 = 30;

/// One attempt of the health check: whether `address` answers on `port` now.
/// `forwarded` is true when the address is the node's own side of a proxy: the proxy takes the connection before it dials
/// the service, so an answer is a connection that is not closed at once.
pub type Probe = Arc<dyn Fn(&str, u16, bool) -> bool + Send + Sync>;

/// What a node's `service_update` does at its edges: how a port is tried and how long is waited between two tries.
#[derive(Clone)]
pub struct UpdateOptions {
    pub probe: Probe,
    pub pace: Duration,
}

impl Default for UpdateOptions {
    fn default() -> Self {
        Self {
            probe: Arc::new(|address, port, forwarded| {
                let Ok(ip) = address.parse::<std::net::IpAddr>() else {
                    return false;
                };
                let Ok(mut stream) =
                    TcpStream::connect_timeout(&SocketAddr::new(ip, port), CONNECT_WAIT)
                else {
                    return false;
                };
                if !forwarded {
                    return true;
                }
                // The proxy closes a connection whose target does not answer; a service that waits for the client
                // to speak first leaves it open.
                let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                match stream.read(&mut [0u8; 1]) {
                    Ok(read) => read > 0,
                    Err(error) => matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ),
                }
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
    /// Incus did not answer within its deadline: what it was asked may still be going on.
    unsettled: bool,
}

impl Failure {
    fn of(step: &'static str, error: &IncusError) -> Failure {
        Failure {
            step,
            why: incus_words(error),
            unsettled: matches!(error, IncusError::Timeout),
        }
    }

    fn said(step: &'static str, why: String) -> Failure {
        Failure {
            step,
            why,
            unsettled: false,
        }
    }
}

fn incus_words(error: &IncusError) -> String {
    match error {
        IncusError::Status { code, message } => format!("Incus answered {code}: {message}"),
        IncusError::Timeout => "Incus did not answer in time".to_string(),
        IncusError::Refused(why) => why.clone(),
        other => format!("Incus: {other}"),
    }
}

pub(super) struct Journal {
    pub(super) path: PathBuf,
    pub(super) id: String,
    pub(super) service: String,
    pub(super) clock: Clock,
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
    pub(super) fn step(&self, step: &str, ok: bool, detail: Value) {
        if let Err(error) = self.line(step, ok, detail) {
            eprintln!("arca: the service update journal could not be written: {error}");
        }
    }
}

/// The writer of the power state and of the root of the instance `config` names.
fn state_changer_for(cfg: &NodeConfig, config: &ServiceConfig) -> Result<StateChanger, CallError> {
    let family = arca_incus::ProjectFamily::new(
        [arca_netproject::PROJECT],
        cfg.kernel.droidd_projects.iter().map(String::as_str),
    )
    .map_err(|_| internal())?;
    let mut changer =
        StateChanger::new(cfg.incus.socket.clone(), family).with_deadline(STEP_DEADLINE);
    if config.tenant.is_some() {
        changer = changer
            .with_tenants([config.project.as_str()])
            .map_err(|_| internal())?;
    }
    Ok(changer)
}

/// Where the service's template declares a data volume while its instance has no `data` device: the path its data is
/// kept at inside the container, which a rebuild of the root erases. The template is read as the record kept it, then
/// as installed on the node, then as this build ships it: an installed file that is stale, edited or half installed
/// must not hide a service that keeps its data on its root. `None` when none of them declares a volume, or the
/// instance has it.
fn data_on_the_root(
    cfg: &NodeConfig,
    config: &ServiceConfig,
    seen: &InstanceState,
) -> Option<String> {
    if seen.own_devices.contains("data") {
        return None;
    }
    if let Some(data) = &config.data {
        return Some(data.path.clone());
    }
    crate::service::load_template(&cfg.state_dir.join("templates"), &config.template)
        .ok()
        .and_then(|template| template.data.map(|data| data.path))
        .or_else(|| shipped_data_path(&config.template))
}

/// The data path of the template of this name that this build ships, when it declares one.
fn shipped_data_path(template: &str) -> Option<String> {
    let file = format!("{template}.json");
    let (_, text) = crate::cmd_templates::SHIPPED
        .iter()
        .find(|(name, _)| *name == file)?;
    let shipped: Value = serde_json::from_str(text).ok()?;
    shipped["data"]["path"].as_str().map(str::to_string)
}

/// The refusal of an update that would erase the data a service keeps inside its container.
fn erasing(config: &ServiceConfig, path: &str) -> CallError {
    let (name, template) = (&config.name, &config.template);
    refused(format!(
        "update service {name}: this update would erase the service's data: {name}, made from the {template} template, keeps its data inside the container, and that template declares a data volume at {path} which {name} does not have. Give eraseData: true, which needs the owner's approval, to update it and start over with no data. Nothing was changed"
    ))
}

/// What the service's template says about no-new-privileges when it differs from what the service has: the setting an
/// update gives it. `None` when they agree, when the template cannot be read, and in a tenant. The run and the
/// proposal work it out the same way.
fn key_change(cfg: &NodeConfig, config: &ServiceConfig) -> Option<bool> {
    config
        .tenant
        .is_none()
        .then(|| {
            crate::service::load_template(&cfg.state_dir.join("templates"), &config.template).ok()
        })
        .flatten()
        .map(|template| template.no_new_privileges)
        .filter(|wanted| *wanted != config.no_new_privileges)
}

/// The refusal of an update that moves the digest while the template's no-new-privileges differs from the service's.
fn key_first(name: &str, wanted: bool) -> CallError {
    refused(format!(
        "update service {name}: its template now {} noNewPrivileges, which is applied by its own update: call service_update with the digest the service already runs, then move the digest. Or move the digest first and change the template's key afterwards, which is safer for an image that needs a setuid helper. Nothing was changed",
        if wanted { "sets" } else { "drops" }
    ))
}

/// What a proposal of `service_update` is held to when it is made, so the owner is never shown a card for an update
/// the node would refuse: one that mixes `follow` with what it does not go with, or that would erase the service's
/// data without `eraseData`. Answers the arguments to record. A card that states `eraseData` for a service that has
/// its data volume would say the data is erased when it is not, so the node leaves `eraseData` out of such a
/// proposal. What cannot be read now (an instance, a record) is left to the run, which checks again.
pub(super) fn check_proposal(
    cfg: &NodeConfig,
    entries: &[Entry],
    payload: &Value,
) -> Result<Value, CallError> {
    if payload.get("follow").is_some() {
        conflicts(payload)?;
        return Ok(payload.clone());
    }
    let erase = payload.get("eraseData").and_then(Value::as_bool) == Some(true);
    let Some(name) = payload["name"].as_str() else {
        return Ok(payload.clone());
    };
    let scope = tenants::tenant_arg(cfg, payload, entries)?;
    let scoped = tenants::scoped_entries(entries, scope.as_ref());
    let Some((_, config)) = service_entries(&scoped)
        .into_iter()
        .find(|(_, config)| config.name == name)
    else {
        return Ok(payload.clone());
    };
    if config.adopted || config.function.is_some() || config.vm.is_some() {
        return Ok(payload.clone());
    }
    // The key change the run will make, worked out as the run works it out. A same-digest proposal is that change
    // alone, with a card of its own: it restarts the service and erases nothing, so it needs no `eraseData`, and a
    // card that states it is refused. A digest move while the key differs is refused, so the owner never sees a card
    // the run would refuse.
    let wanted = key_change(cfg, &config);
    let moves = payload["digest"]
        .as_str()
        .and_then(|digest| source_with(&config.source, digest))
        .is_some_and(|source| source != config.source);
    match (
        wanted,
        payload.get("noNewPrivileges").and_then(Value::as_bool),
    ) {
        (Some(wanted), _) if moves => return Err(key_first(name, wanted)),
        (Some(wanted), stated) => {
            if stated.is_some_and(|stated| stated != wanted) || erase {
                return Err(CallError::Invalid(
                    "noNewPrivileges, eraseData: the update at the digest the service runs only sets no-new-privileges to what its template says, and erases nothing".into(),
                ));
            }
            if wanted
                && let Some(why) = crate::handlers::no_new_privileges_refusal(
                    &arca_incus::UnixIncus::new(cfg.incus.socket.clone()),
                    &config.template,
                )
            {
                return Err(refused(format!(
                    "update service {name}: {}",
                    why.replace("Nothing was created", "Nothing was changed")
                )));
            }
            let mut keyed = payload.clone();
            keyed["noNewPrivileges"] = json!(wanted);
            return Ok(keyed);
        }
        (None, Some(_)) => {
            return Err(CallError::Invalid(
                "noNewPrivileges: the service's template says what the service already has".into(),
            ));
        }
        (None, None) => {}
    }
    let Ok(changer) = state_changer_for(cfg, &config) else {
        return Ok(payload.clone());
    };
    let Ok(seen) = changer.read(&config.project, &config.instance) else {
        return Ok(payload.clone());
    };
    match (data_on_the_root(cfg, &config, &seen), erase) {
        (Some(path), false) => Err(erasing(&config, &path)),
        (None, true) => {
            let mut plain = payload.clone();
            if let Some(object) = plain.as_object_mut() {
                object.remove("eraseData");
            }
            Ok(plain)
        }
        _ => Ok(payload.clone()),
    }
}

/// What `follow` does not go with: a digest, a timeout, `eraseData`. Said in the words the call uses.
fn conflicts(args: &Value) -> Result<(), CallError> {
    if args.get("digest").is_some() || args.get("timeoutSeconds").is_some() {
        return Err(CallError::Invalid(
            "follow: give it alone; a call moves a service to a digest or sets whether it follows Automagik, not both".into(),
        ));
    }
    if args.get("eraseData").is_some() {
        return Err(CallError::Invalid(
            "eraseData: give it with a digest, not with follow".into(),
        ));
    }
    if args.get("noNewPrivileges").is_some() {
        return Err(CallError::Invalid(
            "noNewPrivileges: it is set by an update at the service's digest, not with follow"
                .into(),
        ));
    }
    Ok(())
}

/// How an update puts the old version back.
enum Back {
    /// The snapshot taken before the rebuild.
    Snapshot(String),
    /// A project that does not allow snapshots: the root is rebuilt from the old image.
    OldImage,
}

impl ServiceHandler {
    /// The writer of the power state and of the root of this service's instance.
    pub(super) fn state_changer(&self, config: &ServiceConfig) -> Result<StateChanger, CallError> {
        state_changer_for(&self.cfg, config)
    }

    /// Changes one service's record as the kernel holds it now, under the node's write gate: a change that came
    /// meanwhile is kept. Answers the record before and after.
    pub(super) fn amend(
        &self,
        entry_id: &str,
        name: &str,
        change: impl FnOnce(&mut ServiceConfig),
    ) -> Result<(ServiceConfig, ServiceConfig), CallError> {
        let _write = self.writes.lock().map_err(|_| internal())?;
        let mut kernel = lock(&self.kernel)?;
        let before = service_entries(kernel.entries())
            .into_iter()
            .find(|(entry, _)| entry.id.0 == entry_id)
            .map(|(_, config)| config)
            .ok_or_else(|| service_not_found(format!("service {name}: not found")))?;
        let mut after = before.clone();
        change(&mut after);
        let change = service_wiring::change(&after)?;
        let ids = [entry_id];
        let outcomes = kernel
            .propose(vec![change], (self.clock)())
            .map_err(|error| service_wiring::state_not_saved("service_update", &error))?;
        if service_wiring::failed_for(&outcomes, &ids) {
            return Err(refused(format!(
                "service {name}: {}",
                service_wiring::failure_reason(&outcomes, &ids)
                    .unwrap_or_else(|| "the node did not apply the change".to_string())
            )));
        }
        Ok((before, after))
    }

    /// `service_update` with `follow`: whether the node's follower moves this service to Automagik's newest published
    /// server image. Only a container of the node's own project made from that image by digest can say it.
    fn set_follow(&self, name: &str, follow: bool, entries: &[Entry]) -> Result<Value, CallError> {
        let Some((entry, config)) = service_entries(entries)
            .into_iter()
            .find(|(_, config)| config.name == name)
        else {
            return Err(service_not_found(format!("service {name}: not found")));
        };
        if !config.can_follow_automagik() {
            return Err(refused(format!(
                "service {name}: only a container of this node's own project made from {} by digest can follow Automagik. Nothing was changed",
                crate::service::AUTOMAGIK_IMAGE
            )));
        }
        let value = if follow {
            crate::service::FOLLOW_AUTOMAGIK
        } else {
            crate::service::FOLLOW_NONE
        };
        let (before, _) = self.amend(&entry.id.0, name, |record| {
            record.follow = Some(value.to_string());
        })?;
        let changed = before.follows_automagik() != follow;
        if changed {
            Journal {
                path: self.cfg.state_dir.join(SERVICE_UPDATES_FILE),
                id: format!("follow-{}-{}", entry.id.0, (self.clock)()),
                service: name.to_string(),
                clock: self.clock.clone(),
            }
            .step("follow set", true, json!({ "follow": follow }));
        }
        Ok(json!({
            "updated": false,
            "service": name,
            "follow": follow,
            "message": if follow {
                format!("service {name} follows Automagik's releases")
            } else {
                format!("service {name} stays where it is: it no longer follows Automagik's releases")
            },
        }))
    }

    /// `service_update`: moves the service `name` to `digest`. `entries` are the ones the call may see, a tenant's when
    /// it names one. `approved` is true when the call runs from an approved proposal: only then may `eraseData` be
    /// given, since it lets the update erase what the service keeps inside its container.
    pub(super) fn call_update(
        &self,
        args: &Value,
        entries: &[Entry],
        approved: bool,
    ) -> Result<Value, CallError> {
        let name = service_name(args)?;
        let erase = match args.get("eraseData") {
            None => false,
            Some(value) => value
                .as_bool()
                .ok_or_else(|| CallError::Invalid("eraseData: expected true or false".into()))?,
        };
        if args.get("follow").is_some() {
            conflicts(args)?;
        }
        if erase && !approved {
            return Err(refused(format!(
                "update service {name}: eraseData erases the service's data, so it needs the owner's approval: propose the update with action_propose, and the card says in words that the data is erased. Nothing was changed"
            )));
        }
        if let Some(follow) = args.get("follow") {
            if args.get("digest").is_some() || args.get("timeoutSeconds").is_some() {
                return Err(CallError::Invalid(
                    "follow: give it alone; a call moves a service to a digest or sets whether it follows Automagik, not both".into(),
                ));
            }
            let follow = follow
                .as_bool()
                .ok_or_else(|| CallError::Invalid("follow: expected true or false".into()))?;
            return self.set_follow(&name, follow, entries);
        }
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
        // How the node can tell that it answers: the host port's proxy, the droidd port, or only that it runs.
        let reach = if let Some(lan) = &config.lan {
            Reach::Lan(lan.port)
        } else if config.droidd_port == Some(port) {
            Reach::Direct(port)
        } else {
            Reach::Running
        };
        // What the template says now about no-new-privileges, against what the service has. A difference is applied
        // as a config write and a restart, never a rebuild, so the data stays where it is. It is its own step: an
        // update that also moves the digest is refused, so no answer mixes the two and a failure of one never
        // leaves the other half done.
        let wanted = key_change(&self.cfg, &config);
        if let Some(wanted) = wanted {
            if new_source != config.source {
                return Err(key_first(&name, wanted));
            }
            // A card the owner approved names the setting it gave: the template must still say it.
            if let Some(stated) = args.get("noNewPrivileges")
                && stated.as_bool() != Some(wanted)
            {
                return Err(said(
                    "noNewPrivileges is not what the template says now. Nothing was changed",
                ));
            }
            let key = KeyUpdate {
                port,
                reach,
                seconds,
            };
            return self.change_no_new_privileges(&entry, &config, wanted, &digest, key);
        }
        if args.get("noNewPrivileges").is_some() {
            return Err(said(
                "noNewPrivileges: the service's template says what the service already has. Nothing was changed",
            ));
        }
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
        let changer = self.state_changer(&config)?;
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
        // The rebuild replaces the root disk. A service whose template declares a data volume that its instance does not
        // have keeps its data on that root disk, so the rebuild would erase all of it: refused before any write unless
        // the call states eraseData (which only an approved proposal may).
        if !erase && let Some(path) = data_on_the_root(&self.cfg, &config, &seen) {
            return Err(erasing(&config, &path));
        }
        // A rebuild of a container that has never run leaves it unable to start (Incus 7.5.1), as a function's does.
        if !seen
            .own_config
            .contains_key(arca_incus::function_run::STARTED_KEY)
        {
            return Err(said(
                "the service has never been started, and a rebuild would leave it unable to start: start it with instance_start first",
            ));
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
                    "eraseData": erase,
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
            reach,
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

impl ServiceHandler {
    /// Makes a service's no-new-privileges what its template says: the record, the instance's `raw.lxc` and a restart
    /// when it runs, in that order, and nothing else. The image and the data volume are not touched. The project is
    /// checked first when the key is gained; a record the node saved and could not apply, or a restart that failed,
    /// is put back, so every answer says what is true.
    fn change_no_new_privileges(
        &self,
        entry: &Entry,
        config: &ServiceConfig,
        wanted: bool,
        digest: &str,
        key: KeyUpdate,
    ) -> Result<Value, CallError> {
        let name = &config.name;
        let said = |why: &str| refused(format!("update service {name}: {why}"));
        let _claim = Claim::take(&format!("{}\n{}", self.cfg.state_dir.display(), entry.id.0))
            .ok_or_else(|| {
                said("another update of this service is running. Nothing was changed")
            })?;
        let changer = self.state_changer(config)?;
        let seen = changer
            .read(&config.project, &config.instance)
            .map_err(|error| {
                said(&format!(
                    "its instance was not read ({}). Nothing was changed",
                    incus_words(&error)
                ))
            })?;
        let journalled = lock(&self.kernel)?.service_uuid(&entry.id, &config.instance);
        if seen.kind != "container" || journalled.is_none() || journalled != seen.uuid {
            return Err(said(
                "its instance is not the one Arca created for it. Nothing was changed",
            ));
        }
        if wanted
            && let Some(why) = crate::handlers::no_new_privileges_refusal(
                &arca_incus::UnixIncus::new(self.cfg.incus.socket.clone()),
                &config.template,
            )
        {
            return Err(refused(format!(
                "update service {name}: {}",
                why.replace("Nothing was created", "Nothing was changed")
            )));
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
                json!({ "noNewPrivileges": wanted, "digest": digest, "rebuild": false }),
            )
            .map_err(|_| said("the update journal could not be written. Nothing was changed"))?;
        // The node saves the record before it writes the instance, so a refused write leaves the new value saved:
        // it is put back here, and the answer says whether that worked.
        let put_back = || {
            self.amend(&entry.id.0, name, |record| {
                record.no_new_privileges = !wanted
            })
            .is_ok()
        };
        if let Err(error) = self.amend(&entry.id.0, name, |record| {
            record.no_new_privileges = wanted
        }) {
            let said_so = match &error {
                CallError::Refused { message, .. } | CallError::Invalid(message) => message.clone(),
                _ => "the node did not apply it".to_string(),
            };
            let restored = put_back();
            journal.step("apply", false, json!({ "restored": restored }));
            return Err(refused(format!(
                "update service {name}: noNewPrivileges was not applied: {said_so}. {}",
                if restored {
                    "The service's record is as it was. Nothing was changed"
                } else {
                    "The service's record could not be put back: service_status shows what it says"
                }
            )));
        }
        journal.step("apply", true, json!({}));
        let was_running = seen.is_running();
        if was_running && let Err(error) = changer.change(&seen, StateAction::Restart) {
            let restored = put_back();
            let running = changer
                .read(&config.project, &config.instance)
                .map(|state| state.is_running())
                .unwrap_or(false);
            let started = running
                || changer
                    .read(&config.project, &config.instance)
                    .and_then(|state| changer.change(&state, StateAction::Start))
                    .is_ok();
            journal.step(
                "restart",
                false,
                json!({ "restored": restored, "running": started }),
            );
            return Err(refused(format!(
                "update service {name}: the restart failed ({}). {} {}",
                incus_words(&error),
                if restored {
                    "The service's record and its instance's config are as they were."
                } else {
                    "The service's record could not be put back: service_status shows what it says."
                },
                if started {
                    "The service is running."
                } else {
                    "The service is stopped: instance_start starts it."
                },
            )));
        }
        journal.step("restart", true, json!({ "restarted": was_running }));
        // The same health check a digest update runs, within the call's timeout: a service that does not come back
        // (an image whose setuid helper breaks under the key, say) gets the old setting back and a restart.
        if was_running {
            let run = Run {
                handler: self,
                changer: &changer,
                config,
                entry,
                new_source: &config.source,
                digest,
                port: key.port,
                reach: key.reach,
                seconds: key.seconds,
                registry: None,
                journal: &journal,
                was_running,
            };
            if let Err(failure) = run.healthy() {
                journal.step("health", false, json!({ "why": failure.why }));
                let restored = put_back();
                let restarted = changer
                    .read(&config.project, &config.instance)
                    .and_then(|state| {
                        // Incus refuses to restart a stopped instance: start it, restart a running one.
                        let action = if state.is_running() {
                            StateAction::Restart
                        } else {
                            StateAction::Start
                        };
                        changer.change(&state, action)
                    })
                    .is_ok();
                let answers = restarted && run.answers_within(key.seconds.min(OLD_ANSWERS_SECONDS));
                journal.step(
                    "end",
                    false,
                    json!({ "rolledBack": restored, "restarted": restarted, "answers": answers }),
                );
                return Err(CallError::Refused {
                    code: "rolled_back".into(),
                    message: format!(
                        "update service {name}: it did not come back healthy with no-new-privileges {}: {}. {} {}",
                        if wanted { "on" } else { "off" },
                        failure.why,
                        if restored {
                            "The old setting is back, in its record and in its instance's config."
                        } else {
                            "The old setting could not be put back in its record: service_status shows what it says."
                        },
                        if !restarted {
                            "It could not be restarted again: instance_start starts it."
                        } else if answers {
                            "It was restarted again and its port answers."
                        } else {
                            "It was restarted again, but its port did not answer yet: service_status and service_logs show how it is."
                        },
                    ),
                });
            }
            journal.step("health", true, json!({ "port": key.port }));
        }
        journal.step("end", true, json!({ "noNewPrivileges": wanted }));
        Ok(json!({
            "updated": true,
            "service": name,
            "digest": digest,
            "noNewPrivileges": wanted,
            "restarted": was_running,
            "message": format!(
                "the service now runs {} no-new-privileges, as its template says{}; its image and data were not touched",
                if wanted { "with" } else { "without" },
                if was_running { ", and was restarted for it (the restart interrupts it) and answered its health check" } else { ", from its next start" },
            ),
        }))
    }
}

/// What the health check after a key update needs: the port it tries, how it reaches it, and how long it waits.
struct KeyUpdate {
    port: u16,
    reach: Reach,
    seconds: u64,
}

/// How the node tells that a service answers.
#[derive(Clone, Copy)]
enum Reach {
    /// Through the proxy of the service's host port: the node's own loopback, this port.
    Lan(u16),
    /// At the service's address, on a port its firewall opens to the node.
    Direct(u16),
    /// The node can reach nothing of it: the instance must stay running.
    Running,
}

struct Run<'a> {
    handler: &'a ServiceHandler,
    changer: &'a StateChanger,
    config: &'a ServiceConfig,
    entry: &'a Entry,
    new_source: &'a Value,
    digest: &'a str,
    port: u16,
    reach: Reach,
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
        if failure.unsettled && failure.step == "rebuild" {
            self.journal.step(
                "end",
                false,
                json!({ "rolledBack": false, "unsettled": true }),
            );
            return Err(CallError::Refused {
                code: "update_unsettled".into(),
                message: format!(
                    "update service {name} to {}: Incus did not finish the rebuild in time ({}) and may still be at it. Nothing was rolled back, so as not to rebuild twice at once: service_status shows what is there, and the same update may be asked for again once Incus is idle",
                    self.digest, failure.why,
                ),
            });
        }
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
        match self.roll_back(&mut instance, &back, failure.step) {
            Ok(back) => {
                self.journal.step(
                    "end",
                    false,
                    json!({ "rolledBack": true, "rootKept": back.root_kept, "oldAnswers": back.answers }),
                );
                Err(CallError::Refused {
                    code: "rolled_back".into(),
                    message: format!(
                        "update service {name} to {}: {} failed: {}. The update was rolled back: {}the old version is {}{}",
                        self.digest,
                        failure.step,
                        failure.why,
                        if back.root_kept {
                            "Incus could not fetch the image again, so the old root was left as it was, and "
                        } else {
                            ""
                        },
                        if self.was_running {
                            "running again"
                        } else {
                            "in place, and the service is stopped as it was"
                        },
                        if self.was_running && !back.answers {
                            ", but its port did not answer yet: service_status and service_logs show how it is"
                        } else {
                            ""
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
            .map_err(|error| Failure::of("start", &error))
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
            .map_err(|error| Failure::of("stop", &error))
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
            // A project that forbids snapshots (every project Arca makes does, and Incus answers 500 or 403): the old version is put back by a rebuild from its
            // own digest instead.
            Err(IncusError::Status { code, message })
                if code == 403 || message.contains("snapshot creation") =>
            {
                self.journal.step(
                    "snapshot",
                    false,
                    json!({ "skipped": "the project does not allow snapshots", "incus": message }),
                );
                None
            }
            Err(error) => {
                return Err(Failure::of("snapshot", &error));
            }
        };
        *back = Some(match &held {
            Some(name) => Back::Snapshot(name.clone()),
            None => Back::OldImage,
        });
        self.changer
            .rebuild_from_registry(instance, self.new_source, self.registry)
            .map_err(|error| Failure::of("rebuild", &error))?;
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

    /// The service answers within the timeout, as its reach lets the node tell.
    fn healthy(&self) -> Result<(), Failure> {
        if self.answers_within(self.seconds) {
            Ok(())
        } else {
            Err(Failure::said(
                "health check",
                match self.reach {
                    Reach::Running => "the instance did not stay running".to_string(),
                    _ => format!(
                        "port {} did not answer within {} second{}",
                        self.port,
                        self.seconds,
                        if self.seconds == 1 { "" } else { "s" }
                    ),
                },
            ))
        }
    }

    fn answers_within(&self, seconds: u64) -> bool {
        let options = &self.handler.update;
        let begun = Instant::now();
        if let Reach::Running = self.reach {
            // Looked at over a few seconds: one look at the first moment says little.
            let limit = Duration::from_secs(seconds.min(STAY_RUNNING_SECONDS));
            loop {
                let up = self
                    .changer
                    .read(&self.config.project, &self.config.instance)
                    .is_ok_and(|state| state.is_running());
                if !up {
                    return false;
                }
                if begun.elapsed() >= limit {
                    return true;
                }
                std::thread::sleep(options.pace);
            }
        }
        let limit = Duration::from_secs(seconds);
        loop {
            let up = match self.reach {
                Reach::Lan(port) => (options.probe)("127.0.0.1", port, true),
                Reach::Direct(port) => (options.probe)(&self.config.ipv4, port, false),
                Reach::Running => true,
            };
            if up {
                return true;
            }
            if begun.elapsed() >= limit {
                return false;
            }
            std::thread::sleep(options.pace);
        }
    }

    /// The new digest goes into the service's record, so a restart keeps it.
    fn record(&self) -> Result<(), Failure> {
        let failed = |why: &str| Failure::said("record", why.to_string());
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
    /// the health check passed, so it still names the old digest. A failed fetch of the old image after a failed
    /// fetch of the new one leaves the old root as it was (Incus resolves the image before it takes the root away), so
    /// the old version is simply started.
    fn roll_back(
        &self,
        instance: &mut InstanceState,
        back: &Back,
        after: &str,
    ) -> Result<Rolled, Failure> {
        self.stopped(instance)?;
        let mut root_kept = false;
        match back {
            Back::Snapshot(snapshot) => {
                self.changer
                    .restore_snapshot(instance, snapshot)
                    .map_err(|error| Failure::of("restore", &error))?;
                self.journal
                    .step("restore", true, json!({ "snapshot": snapshot }));
            }
            Back::OldImage => {
                match self.changer.rebuild_from_registry(
                    instance,
                    &self.config.source,
                    self.registry,
                ) {
                    Ok(()) => self.journal.step("rebuild back", true, json!({})),
                    Err(error) if after == "rebuild" && !matches!(error, IncusError::Timeout) => {
                        root_kept = true;
                        self.journal.step(
                            "rebuild back",
                            false,
                            json!({ "why": incus_words(&error) }),
                        );
                    }
                    Err(error) => return Err(Failure::of("rebuild back", &error)),
                }
            }
        }
        *instance = self
            .changer
            .read(&instance.project, &instance.name)
            .map_err(|error| Failure::of("read", &error))?;
        self.start_again(instance)?;
        let mut answers = true;
        if self.was_running {
            self.journal.step("start", true, json!({ "old": true }));
            answers = self.answers_within(self.seconds.min(OLD_ANSWERS_SECONDS));
            self.journal.step("old answers", answers, json!({}));
        }
        Ok(Rolled { root_kept, answers })
    }
}

/// What a rollback did.
struct Rolled {
    /// The old root was never taken away.
    root_kept: bool,
    /// The old version answered after it was started.
    answers: bool,
}
