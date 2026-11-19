//! `service_update`: a running service made from an image by digest, or from a local image pinned by fingerprint,
//! moves to another image in one call.
//!
//! The steps, each journalled in [`SERVICE_UPDATES_FILE`] (an append-only file of JSON lines in the state directory,
//! one `begin` line before anything is sent to Incus, one line per step and one `end` line; no line holds a secret):
//!
//! 1. the instance is stopped, when it runs, and each custom volume of the service's own (its data volume) is
//!    snapshotted, `pre-update-<time>`; the instance itself is not, since Incus 7.6 refuses to rebuild an instance that
//!    holds a snapshot, and a volume's snapshot does not stand in a rebuild's way. A project Arca makes holds no snapshot
//!    until its owner's entry (the service project's, or the tenant's) has its `snapshots` step: the first update whose
//!    snapshot the project refuses writes it, with no card, and takes the snapshot then;
//! 2. its root is rebuilt from the new digest, or, for a local image, from the project's own copy of the new
//!    fingerprint. Incus keeps the config, the devices (the data volume among them), the uuid, the secrets and the
//!    firewall rules; only the root is the new image's;
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
//! With `withTemplate`, which only an approved proposal may state, the same run also makes the service what its
//! CURRENT template says, as `service_reconfigure` would (environment, links' values, declared secrets, ports,
//! `rawLxc`, no-new-privileges, entrypoint): the difference is worked out when the proposal is made and recorded in
//! it as `changes`, so the card shows it and the approval binds it; the run works it out again and changes nothing
//! unless it is still that difference. The record is written after the rebuild, while the instance is stopped, so the
//! node writes its config before the one start: one interruption for the image and the settings. A failure after that
//! write puts the old record back too, before the old version is started.
//!
//! An instance that already holds an Incus snapshot is refused before anything is written, naming its snapshots: the
//! rebuild would fail in the middle of the run. Removing them is a plan of its own, on the owner's passkey card.
//!
//! A failure at any step puts the old version back: the root is rebuilt from the old digest, then each volume snapshot
//! is restored, so what the new version wrote to the data volume is undone; the old version is started when it was
//! running, and the answer is the error and the words "rolled back". The volume snapshot is kept either way. Nothing
//! is asked of the owner: the verb is a doctor's, and works in a tenant as it does outside one.
use super::service_reconfigure::{self, Doing, Ready, Written};
use super::*;
use crate::service::ServiceConfig;
use arca_incus::{IncusError, InstanceState, SnapshotPart, StateAction, StateChanger};
use arca_kernel::desired::Entry;
use arca_kernel::ownership::KERNEL_ENTRY_KEY;
use std::cell::{Cell, RefCell};
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
pub const STEP_DEADLINE: Duration = Duration::from_secs(600);
/// A service the node cannot reach has to stay running this long (at most the call's timeout).
const STAY_RUNNING_SECONDS: u64 = 10;
/// How many names an update tries for its volume snapshot when earlier updates kept the ones before.
const MAX_SNAPSHOT_NAMES: u32 = 100;
/// The old version, started again after a rollback, is given this long to answer.
pub const OLD_ANSWERS_SECONDS: u64 = 30;

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

pub(super) struct Claim(String);

impl Claim {
    pub(super) fn take(id: &str) -> Option<Claim> {
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

/// `source` with its image replaced by `digest`: `<repository>@sha256:<new>` for an OCI image by digest on a registry,
/// or the three fields of a local image pinned by the new fingerprint. `pinned_alias` is the alias of the image the
/// installed template pins when that template pins the same fingerprint: a local source takes it, since Incus decides
/// by the fingerprint and the alias is only a name kept in the record. `None` when `source` is neither kind.
pub fn source_with(source: &Value, digest: &str, pinned_alias: Option<&str>) -> Option<Value> {
    let fields = source.as_object()?;
    if fields.len() == 5 {
        if source["type"] != "image"
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
        return Some(next);
    }
    if fields.len() != 3 || source["type"] != "image" {
        return None;
    }
    if !source["fingerprint"]
        .as_str()
        .is_some_and(|hex| digest_ok(&format!("sha256:{hex}")))
    {
        return None;
    }
    if !digest_ok(digest) {
        return None;
    }
    let hex = &digest["sha256:".len()..];
    let alias = pinned_alias
        .or_else(|| source["alias"].as_str())
        .filter(|alias| !alias.is_empty())?;
    Some(json!({ "type": "image", "alias": alias, "fingerprint": hex }))
}

/// The template a `service_create` makes its service from when the call names `digest`: the same template at that
/// digest of its own repository, by the very rule an update moves a service by ([`source_with`] over the template's
/// own source, after [`digest_ok`]), so a create can only name another build of the template's image. The digest is
/// passed to Incus untouched: a multi-arch index digest stays the index's, and each node pulls its own architecture.
/// Without `digest`, the template as it is. A digest of any other shape, or one given with a template that is not an
/// OCI image, is an invalid argument.
pub(super) fn template_at(
    template: &crate::service::Template,
    digest: Option<&Value>,
) -> Result<crate::service::Template, CallError> {
    let Some(digest) = digest else {
        return Ok(template.clone());
    };
    if template.kind != "oci" {
        return Err(CallError::Invalid(format!(
            "digest: template {} is not an OCI image by digest, so a create from it names no digest",
            template.name
        )));
    }
    let moved = digest
        .as_str()
        .filter(|digest| digest_ok(digest))
        .and_then(|digest| {
            let source = crate::service::source(template).ok()?;
            source_with(&source, digest, None).map(|_| digest)
        })
        .ok_or_else(|| {
            CallError::Invalid(
                "digest: expected sha256: and 64 lower-case hex digits, the digest of another build of the template's own image"
                    .into(),
            )
        })?;
    let mut at = template.clone();
    at.digest = Some(moved.to_string());
    Ok(at)
}

/// The image (`<registry>/<repository>`) of the OCI service a `service_update` proposal names, for the card's version
/// lookup. `None` for any other service, or a name no service has.
pub(super) fn proposed_image(
    cfg: &NodeConfig,
    entries: &[Entry],
    payload: &Value,
) -> Option<String> {
    let name = payload["name"].as_str()?;
    let scope = tenants::tenant_arg(cfg, payload, entries).ok()?;
    let scoped = tenants::scoped_entries(entries, scope.as_ref());
    let (_, config) = service_entries(&scoped)
        .into_iter()
        .find(|(_, config)| config.name == name)?;
    if config.source["protocol"] != "oci" {
        return None;
    }
    let host = config.source["server"].as_str()?.strip_prefix("https://")?;
    let (repository, _) = config.source["alias"].as_str()?.rsplit_once("@sha256:")?;
    Some(format!("{host}/{repository}"))
}

/// The source an update to `digest` gives the service `config`: the OCI source with its digest replaced, or the local
/// pinned source with the new fingerprint and the installed template's alias when that template pins the same
/// fingerprint. `None` when the service's own source is neither kind.
fn next_source(cfg: &NodeConfig, config: &ServiceConfig, digest: &str) -> Option<Value> {
    // Only a local source has an alias that the installed template can name; an OCI source keeps its repository.
    let installed = config.source.get("fingerprint").and_then(|_| {
        crate::service::load_template(&cfg.state_dir.join("templates"), &config.template).ok()
    });
    let pinned = installed
        .as_ref()
        .and_then(|template| template.local_image_pin())
        .filter(|(_, fingerprint)| digest.strip_prefix("sha256:") == Some(*fingerprint))
        .map(|(alias, _)| alias);
    source_with(&config.source, digest, pinned)
}

/// Whether two sources name the same image. A local image is the same when its fingerprint is: the alias is only the
/// name the record keeps, and Incus decides by the fingerprint.
fn same_image(one: &Value, other: &Value) -> bool {
    match (one["fingerprint"].as_str(), other["fingerprint"].as_str()) {
        (Some(a), Some(b)) => a == b,
        _ => one == other,
    }
}

/// The refusal of an update to a local image the service project's store does not hold: a local image is copied or
/// built in by hand, and Incus's own answer to a rebuild names no reason.
fn local_image_missing(name: &str, fingerprint: &str, alias: &str, project: &str) -> CallError {
    refused(format!(
        "update service {name}: the image store of the service project {project} holds no image with the fingerprint sha256:{fingerprint} that the update would rebuild from. Put it there first (`incus image copy <image> local: --alias {alias} --project {project}`, or build the image in that project), then call service_update again. Nothing was changed"
    ))
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
    /// A rebuild that failed before it touched the root: Incus could not fetch the image.
    root_untouched: bool,
}

impl Failure {
    fn of(step: &'static str, error: &IncusError) -> Failure {
        Failure {
            step,
            why: incus_words(error),
            unsettled: matches!(error, IncusError::Timeout),
            root_untouched: false,
        }
    }

    fn said(step: &'static str, why: String) -> Failure {
        Failure {
            step,
            why,
            unsettled: false,
            root_untouched: false,
        }
    }
}

/// Whether a rebuild failed on the image it was to fetch, which Incus does before it takes the old root away: its own
/// words when the registry gives no image (`Failed getting remote image info`) and when the store holds none (`Image
/// not provided for instance rebuild`). Any other failure may have come after the old root was deleted.
fn image_not_fetched(error: &IncusError) -> bool {
    matches!(error, IncusError::Status { message, .. }
        if message.contains("Failed getting remote image info")
            || message.contains("Image not provided for instance rebuild"))
}

fn incus_words(error: &IncusError) -> String {
    match error {
        IncusError::Status { code, message } => format!("Incus answered {code}: {message}"),
        IncusError::Timeout => "Incus did not answer in time".to_string(),
        IncusError::Refused(why) => why.clone(),
        other => format!("Incus: {other}"),
    }
}

/// A call's refusal in words, for a step that failed on one.
fn call_words(error: &CallError) -> String {
    match error {
        CallError::Invalid(why) => why.clone(),
        CallError::Refused { message, .. }
        | CallError::UntrustedRefused { message, .. }
        | CallError::RefusedWith { message, .. } => message.clone(),
        _ => "the node could not write it".to_string(),
    }
}

pub(super) struct Journal {
    pub(super) path: PathBuf,
    pub(super) id: String,
    pub(super) service: String,
    pub(super) clock: Clock,
}

impl Journal {
    pub(super) fn line(&self, step: &str, ok: bool, detail: Value) -> std::io::Result<()> {
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
pub(super) fn state_changer_for(
    cfg: &NodeConfig,
    config: &ServiceConfig,
) -> Result<StateChanger, CallError> {
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

/// Why a service that keeps `/run` in memory has it on its disk, as its answer says it.
pub(super) const RUN_ON_DISK: &str = "/run stays on the container's disk, as its image has it: an entry there is not root's (a directory a non-root image keeps for its own user, which it needs), or Arca could not remove what the image keeps there, and Incus mounts a memory /run only on an empty one. A secret staged under /run touches the disk";

/// Says in the answer of a service that keeps `/run` in memory where its container's `/run` is, read from Incus:
/// `run: "memory"` (for a stopped one, from its next start), or `run: "disk"` with `runNote` saying why, or
/// `run: "unknown"` when it could not be read.
pub(super) fn say_run(cfg: &NodeConfig, config: &ServiceConfig, answer: &mut Value) {
    let place = state_changer_for(cfg, config).ok().and_then(|changer| {
        let seen = changer.read(&config.project, &config.instance).ok()?;
        changer.run_in_memory(&seen).ok()
    });
    match place {
        Some(true) => answer["run"] = json!("memory"),
        Some(false) => {
            answer["run"] = json!("disk");
            answer["runNote"] = json!(RUN_ON_DISK);
        }
        None => answer["run"] = json!("unknown"),
    }
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

/// Whether the call states `withTemplate: true`.
fn with_template(args: &Value) -> Result<bool, CallError> {
    match args.get("withTemplate") {
        None => Ok(false),
        Some(value) => value
            .as_bool()
            .ok_or_else(|| CallError::Invalid("withTemplate: expected true or false".into())),
    }
}

/// What `withTemplate` does not go with, and what does not go without it.
fn template_conflicts(args: &Value, with: bool) -> Result<(), CallError> {
    if with && args.get("digest").is_none() {
        return Err(CallError::Invalid(
            "withTemplate: give it with the digest the service moves to".into(),
        ));
    }
    if with && args.get("noNewPrivileges").is_some() {
        return Err(CallError::Invalid(
            "noNewPrivileges: with withTemplate the template's setting is among the changes the card shows".into(),
        ));
    }
    if !with && args.get("changes").is_some() {
        return Err(CallError::Invalid(
            "changes: it is the template's difference that a proposal with withTemplate fills in; give withTemplate: true".into(),
        ));
    }
    Ok(())
}

/// The refusal of `withTemplate` for the digest the service already runs: there is no rebuild to ride on.
fn same_digest_with_template(name: &str) -> CallError {
    refused(format!(
        "update service {name}: the service already runs this digest, so there is no new image for withTemplate to go in with: service_reconfigure applies its template alone. Nothing was changed"
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
    // The template's difference is the node's to work out: what a caller sent there is never kept.
    let mut payload = payload.clone();
    if let Some(object) = payload.as_object_mut() {
        object.remove("changes");
    }
    let payload = &payload;
    if payload.get("follow").is_some() {
        conflicts(payload)?;
        return Ok(payload.clone());
    }
    let with = with_template(payload)?;
    template_conflicts(payload, with)?;
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
    let moves = payload["digest"]
        .as_str()
        .and_then(|digest| next_source(cfg, &config, digest))
        .is_some_and(|source| !same_image(&source, &config.source));
    // With the template: the difference the run would apply, worked out now as `service_reconfigure` works it out,
    // so the card shows it and the approval binds it. No-new-privileges is among it, so the key's own path is not
    // taken.
    let mut payload = payload.clone();
    if with {
        if !moves {
            return Err(same_digest_with_template(name));
        }
        payload["changes"] =
            service_reconfigure::plan(Doing::Update, cfg, &scoped, &config, &BTreeSet::new())?
                .changes;
        return data_check(cfg, &config, payload, erase);
    }
    let payload = &payload;
    // The key change the run will make, worked out as the run works it out. A same-digest proposal is that change
    // alone, with a card of its own: it restarts the service and erases nothing, so it needs no `eraseData`, and a
    // card that states it is refused. A digest move while the key differs is refused, so the owner never sees a card
    // the run would refuse.
    let wanted = key_change(cfg, &config);
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
    data_check(cfg, &config, payload.clone(), erase)
}

/// The data part of [`check_proposal`]: an update that would erase the service's data without `eraseData` is refused,
/// and `eraseData` is left out for a service that has its volume. What cannot be read now is left to the run.
fn data_check(
    cfg: &NodeConfig,
    config: &ServiceConfig,
    payload: Value,
    erase: bool,
) -> Result<Value, CallError> {
    let Ok(changer) = state_changer_for(cfg, config) else {
        return Ok(payload);
    };
    let Ok(seen) = changer.read(&config.project, &config.instance) else {
        return Ok(payload);
    };
    match (data_on_the_root(cfg, config, &seen), erase) {
        (Some(path), false) => Err(erasing(config, &path)),
        (None, true) => {
            let mut plain = payload;
            if let Some(object) = plain.as_object_mut() {
                object.remove("eraseData");
            }
            Ok(plain)
        }
        _ => Ok(payload),
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
    if args.get("withTemplate").is_some() || args.get("changes").is_some() {
        return Err(CallError::Invalid(
            "withTemplate: give it with a digest, not with follow".into(),
        ));
    }
    Ok(())
}

/// How an update puts the old version back: the root is rebuilt from the old image, then each volume in `volumes` is
/// restored to the snapshot `snapshot` taken before the rebuild. A service with no volume of its own has none to restore.
struct Back {
    volumes: Option<(String, Vec<SnapshotPart>)>,
}

/// The file that keeps a rollback's restore of a service's volumes that did not finish, for the service entry `id`.
fn pending_path(cfg: &NodeConfig, id: &str) -> PathBuf {
    cfg.state_dir
        .join(format!("service-update-restore-{id}.json"))
}

/// A rollback's restore of a service's volumes that did not finish: the snapshot, every volume it was to put back,
/// and the ones it did. Until it is finished the volumes may hold data of two versions, so the next update finishes it
/// first, from the same snapshot, before it takes a snapshot of its own.
struct Pending {
    snapshot: String,
    parts: Vec<SnapshotPart>,
    restored: Vec<SnapshotPart>,
}

impl Pending {
    fn read(cfg: &NodeConfig, id: &str) -> Result<Option<Pending>, String> {
        let path = pending_path(cfg, id);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.to_string()),
        };
        let row: Value = serde_json::from_str(&text).map_err(|error| error.to_string())?;
        let parts_of = |key: &str| -> Result<Vec<SnapshotPart>, String> {
            row[key]
                .as_array()
                .ok_or_else(|| format!("it names no {key}"))?
                .iter()
                .map(
                    |volume| match (volume["pool"].as_str(), volume["volume"].as_str()) {
                        (Some(pool), Some(name)) => Ok(SnapshotPart::Volume {
                            pool: pool.to_string(),
                            volume: name.to_string(),
                        }),
                        _ => Err(format!("a volume of its {key} has no pool or name")),
                    },
                )
                .collect()
        };
        Ok(Some(Pending {
            snapshot: row["snapshot"]
                .as_str()
                .ok_or("it names no snapshot")?
                .to_string(),
            parts: parts_of("volumes")?,
            restored: parts_of("restored")?,
        }))
    }

    fn write(&self, cfg: &NodeConfig, id: &str, update: &str) -> std::io::Result<()> {
        let row = json!({
            "update": update,
            "snapshot": self.snapshot,
            "volumes": volume_names(&self.parts),
            "restored": volume_names(&self.restored),
        });
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(pending_path(cfg, id))?;
        file.write_all(&serde_json::to_vec(&row).map_err(std::io::Error::other)?)?;
        file.sync_all()
    }
}

/// The volumes `parts` in words: `app-data (pool1), more (pool1)`, or `none`.
fn volume_words(parts: &[SnapshotPart]) -> String {
    let named: Vec<String> = parts
        .iter()
        .filter_map(|part| match part {
            SnapshotPart::Volume { pool, volume } => Some(format!("{volume} ({pool})")),
            SnapshotPart::Instance => None,
        })
        .collect();
    if named.is_empty() {
        "none".to_string()
    } else {
        named.join(", ")
    }
}

/// A restore of `parts` to `snapshot` that failed, in words: which went back, which failed and which were not tried.
fn restore_words(
    snapshot: &str,
    parts: &[SnapshotPart],
    failure: &arca_incus::PartFailure,
) -> String {
    let untried: Vec<SnapshotPart> = parts
        .iter()
        .filter(|part| **part != failure.part && !failure.done.contains(part))
        .cloned()
        .collect();
    format!(
        "the volumes were to go back to the snapshot {snapshot}: put back: {}; failed: {} ({}); not tried: {}",
        volume_words(&failure.done),
        volume_words(std::slice::from_ref(&failure.part)),
        incus_words(&failure.error),
        volume_words(&untried),
    )
}

/// The refusal of an update of a service whose instance holds Incus snapshots: Incus 7.6 does not rebuild such an
/// instance, and says so only once the instance is stopped.
fn holds_snapshots(name: &str, held: &[String]) -> CallError {
    let (noun, them) = if held.len() == 1 {
        ("snapshot", "it")
    } else {
        ("snapshots", "them")
    };
    refused(format!(
        "update service {name}: its instance holds the Incus {noun} {}, and Incus does not rebuild an instance that holds a snapshot, so the update would fail halfway. Arca has no step that removes an instance's own Incus snapshots yet. Removing {them} is to be a plan of its own, on a card the owner approves with their passkey; ask for the update again after that. Nothing was changed",
        held.join(", "),
    ))
}

/// The volumes of a snapshot, as the journal and the answer name them.
fn volume_names(parts: &[SnapshotPart]) -> Value {
    Value::Array(
        parts
            .iter()
            .filter_map(|part| match part {
                SnapshotPart::Volume { pool, volume } => {
                    Some(json!({ "pool": pool, "volume": volume }))
                }
                SnapshotPart::Instance => None,
            })
            .collect(),
    )
}

/// Whether Incus refused a snapshot because the project does not allow one: its own words, `Project "<p>" doesn't
/// allow for snapshot creation`, whatever the status. Any other refusal, a 403 among them, is not that restriction.
fn project_refuses_snapshots(error: &IncusError) -> bool {
    matches!(error, IncusError::Status { message, .. }
        if message.contains("doesn't allow for snapshot creation"))
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
        let with = if args.get("follow").is_some() {
            false
        } else {
            let with = with_template(args)?;
            template_conflicts(args, with)?;
            with
        };
        if with && !approved {
            return Err(refused(format!(
                "update service {name}: withTemplate also makes the service what its template says now, which runs only from a card the owner approved: propose it with action_propose, and the card shows every change it makes. Nothing was changed"
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
                "only a container service made from an image by digest or a local image pinned by fingerprint can be updated",
            ));
        }
        let Some(new_source) = next_source(&self.cfg, &config, &digest) else {
            return Err(said(
                "the service is not made from an OCI image by digest on a registry, nor from a local image pinned by fingerprint",
            ));
        };
        let Some(port) = config.ports.first().copied() else {
            return Err(said(
                "its template declares no port, so there is nothing to check after the start",
            ));
        };
        let reach = reach_of(&config, port);
        // What the template says now about no-new-privileges, against what the service has. A difference is applied
        // as a config write and a restart, never a rebuild, so the data stays where it is. It is its own step: an
        // update that also moves the digest is refused, so no answer mixes the two and a failure of one never
        // leaves the other half done.
        let wanted = if with {
            None
        } else {
            key_change(&self.cfg, &config)
        };
        if let Some(wanted) = wanted {
            if !same_image(&new_source, &config.source) {
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
        if with && same_image(&new_source, &config.source) {
            return Err(same_digest_with_template(&name));
        }
        if same_image(&new_source, &config.source) {
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
        // A local image is copied or built into the service project by hand: require its fingerprint in the store
        // before anything is stopped or rebuilt, since Incus's own answer to a rebuild names no reason.
        if let Some(fingerprint) = new_source["fingerprint"].as_str() {
            let held = changer
                .image_present(&config.project, fingerprint)
                .map_err(|error| {
                    said(&format!(
                        "the images of the service project {} could not be read to check for sha256:{fingerprint} ({})",
                        config.project,
                        incus_words(&error)
                    ))
                })?;
            if !held {
                return Err(local_image_missing(
                    &name,
                    fingerprint,
                    new_source["alias"].as_str().unwrap_or("the image's alias"),
                    &config.project,
                ));
            }
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
        // Incus 7.6 refuses to rebuild an instance that holds a snapshot, once it is stopped: said now, with nothing changed.
        let held = changer.instance_snapshots(&seen).map_err(|error| {
            said(&format!(
                "its instance's snapshots could not be read ({})",
                incus_words(&error)
            ))
        })?;
        if !held.is_empty() {
            return Err(holds_snapshots(&name, &held));
        }
        // A rollback that did not finish putting the volumes back left them part restored: the update finishes that
        // first, from the same snapshot, once the instance is stopped. A running one runs on those volumes as they are,
        // so it is not stopped for it without being asked.
        let pending = Pending::read(&self.cfg, &entry.id.0).map_err(|why| {
            said(&format!(
                "the record of an unfinished restore of its volumes could not be read ({why})"
            ))
        })?;
        if let Some(pending) = &pending
            && seen.is_running()
        {
            return Err(said(&format!(
                "an earlier update's rollback did not finish putting its volumes back to the snapshot {} (put back: {}; to put back: {}), and the service runs on them as they are. Stop it with instance_stop and ask again: the update then first puts every one of them back to {}, which undoes what was written to them since",
                pending.snapshot,
                volume_words(&pending.restored),
                volume_words(&pending.parts),
                pending.snapshot,
            )));
        }
        // The template's difference, worked out again: the run changes nothing unless it is the one on the card.
        let first = if with {
            let first = service_reconfigure::plan(
                Doing::Update,
                &self.cfg,
                entries,
                &config,
                &BTreeSet::new(),
            )?;
            if args.get("changes") != Some(&first.changes) {
                return Err(said(
                    "the service or its template changed since the owner's card was made, so the card no longer says what would be applied: propose it again",
                ));
            }
            Some(first)
        } else {
            None
        };
        // The new version listens where its template says now, so the health check looks there, never at the old port.
        let (port, reach) = match &first {
            Some(first) => {
                let Some(port) = first.next.ports.first().copied() else {
                    return Err(said(
                        "its template now declares no port, so there is nothing to check after the start",
                    ));
                };
                (port, reach_of(&first.next, port))
            }
            None => (port, reach),
        };
        let journal = Journal {
            path: self.cfg.state_dir.join(SERVICE_UPDATES_FILE),
            id: format!("{}-{}", entry.id.0, (self.clock)()),
            service: name.clone(),
            clock: self.clock.clone(),
        };
        let mut begin = json!({
            "from": config.source["alias"],
            "to": digest,
            "timeoutSeconds": seconds,
            "eraseData": erase,
            "tenant": config.tenant.as_ref().map(|tenant| tenant.id.clone()),
        });
        // A local alias alone does not say which image a rollback would need from the store.
        if let Some(fingerprint) = config.source["fingerprint"].as_str() {
            begin["fromFingerprint"] = json!(fingerprint);
        }
        if let Some(first) = &first {
            begin["template"] = service_reconfigure::names_of(&first.changes);
        }
        journal
            .line("begin", true, begin)
            .map_err(|_| said("the update journal could not be written"))?;
        // The secrets the template makes are made now, and each hidden value put behind a reference, before anything
        // is stopped: a plan that moved meanwhile is refused with nothing changed.
        let template = match first {
            Some(first) => {
                let ready = if first.writes {
                    Some(self.ready_plan(Doing::Update, entries, &config, &first, &journal)?)
                } else {
                    None
                };
                Some(TemplateStep {
                    changes: first.changes,
                    ready,
                    old: config.clone(),
                    written: Cell::new(false),
                })
            }
            None => None,
        };
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
            template: template.as_ref(),
            pending: pending.as_ref(),
            notes: RefCell::new(Vec::new()),
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
                template: None,
                pending: None,
                notes: RefCell::new(Vec::new()),
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

/// How the node can tell that the service `config` answers on its first port `port`: the host port's proxy, the
/// droidd port its firewall opens to the node, or only that it runs.
fn reach_of(config: &ServiceConfig, port: u16) -> Reach {
    if let Some(lan) = &config.lan {
        Reach::Lan(lan.port)
    } else if let Some((address, port)) = config
        .private_port
        .as_ref()
        .and_then(|private| Some((private.address.parse().ok()?, private.port)))
    {
        Reach::Private(address, port)
    } else if config.droidd_port == Some(port) {
        Reach::Direct(port)
    } else {
        Reach::Running
    }
}

/// How the node tells that a service answers.
#[derive(Clone, Copy)]
enum Reach {
    /// Through the proxy of the service's host port: the node's own loopback, this port.
    Lan(u16),
    /// Through the proxy of the service's private port: its one host address, which is the only one it listens on.
    Private(std::net::Ipv4Addr, u16),
    /// At the service's address, on a port its firewall opens to the node.
    Direct(u16),
    /// The node can reach nothing of it: the instance must stay running.
    Running,
}

/// The template's part of an update with `withTemplate`.
struct TemplateStep {
    /// The difference the card showed and the run applies.
    changes: Value,
    /// The record to write, or `None` when the difference writes nothing (a secret whose value is set separately).
    ready: Option<Ready>,
    /// The record a rollback puts back, exactly: one that named no entrypoint names none again, and the node takes
    /// the template's off the instance.
    old: ServiceConfig,
    /// Whether the new record may be saved, so a rollback must put the old one back.
    written: Cell<bool>,
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
    template: Option<&'a TemplateStep>,
    /// The restore an earlier rollback did not finish, which this update finishes first.
    pending: Option<&'a Pending>,
    /// What the update changed before anything was to be rolled back, in words, for an answer that says it.
    notes: RefCell<Vec<String>>,
}

impl Run<'_> {
    fn go(&self, seen: InstanceState) -> Result<Value, CallError> {
        let name = &self.config.name;
        let origin = seen.clone();
        let mut instance = seen;
        let mut back: Option<Back> = None;
        let failure = match self.forward(&mut instance, &mut back) {
            Ok(snapshot) => {
                let Some(template) = self.template else {
                    self.journal
                        .step("end", true, json!({ "digest": self.digest }));
                    let mut answer = json!({
                        "updated": true,
                        "service": name,
                        "digest": self.digest,
                        "snapshot": snapshot,
                        "message": format!(
                            "service {name} runs {} and its port {} answered",
                            self.digest, self.port,
                        ),
                    });
                    if self.config.run_tmpfs {
                        say_run(&self.handler.cfg, self.config, &mut answer);
                    }
                    return Ok(answer);
                };
                let differs = self.read_back(template);
                self.journal
                    .step("end", differs.is_empty(), json!({ "digest": self.digest }));
                let mut answer = json!({
                    "updated": true,
                    "service": name,
                    "digest": self.digest,
                    "snapshot": snapshot,
                    "changes": template.changes,
                    "readBack": { "matches": differs.is_empty(), "differs": differs },
                    "message": format!(
                        "service {name} runs {} and what its template says now, started once for both, and its port {} answered; {}",
                        self.digest,
                        self.port,
                        if differs.is_empty() {
                            "its config read back as applied".to_string()
                        } else {
                            format!("its config read back differs at {}: service_status shows how it is", differs.join(", "))
                        },
                    ),
                });
                let record = template
                    .ready
                    .as_ref()
                    .map_or(self.config, |ready| &ready.next);
                if record.run_tmpfs {
                    say_run(&self.handler.cfg, record, &mut answer);
                }
                return Ok(answer);
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
            // Nothing was changed but the power state and what `notes` says: the old version is started again when it
            // ran, if it is still the instance the update began with.
            let restarted = self
                .reread(&origin)
                .and_then(|mut instance| self.start_again(&mut instance));
            let notes = self.notes.borrow();
            self.journal.step(
                "end",
                false,
                json!({ "rolledBack": false, "changedAnything": !notes.is_empty(), "changed": *notes }),
            );
            return Err(refused(format!(
                "update service {name}: {} failed: {}. {}{}",
                failure.step,
                failure.why,
                if notes.is_empty() {
                    "Nothing was changed".to_string()
                } else {
                    format!("Nothing else was changed: {}", notes.join("; "))
                },
                if restarted.is_ok() {
                    ""
                } else {
                    ", but the service could not be started again: instance_start starts it"
                },
            )));
        };
        match self.roll_back(&mut instance, &origin, &back, &failure) {
            Ok(back) => {
                let mut end = json!({ "rolledBack": true, "rootKept": back.root_kept, "oldAnswers": back.answers });
                if let Some(restored) = back.record {
                    end["recordRestored"] = json!(restored);
                }
                self.journal.step("end", false, end);
                Err(CallError::Refused {
                    code: "rolled_back".into(),
                    message: format!(
                        "update service {name} to {}: {} failed: {}. The update was rolled back: {}{}the old version is {}{}",
                        self.digest,
                        failure.step,
                        failure.why,
                        if back.root_kept {
                            "Incus could not fetch the image again, so the old root was left as it was, and "
                        } else {
                            ""
                        },
                        self.record_words(back.record)
                            .map(|words| format!("{words}; "))
                            .unwrap_or_default(),
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
            Err((second, record)) => {
                let mut end =
                    json!({ "rolledBack": false, "step": second.step, "why": second.why });
                if let Some(restored) = record {
                    end["recordRestored"] = json!(restored);
                }
                self.journal.step("end", false, end);
                Err(CallError::Refused {
                    code: "rollback_failed".into(),
                    message: format!(
                        "update service {name} to {}: {} failed: {}. It was NOT rolled back: {} failed: {}. {}{}; service_status shows what is there",
                        self.digest,
                        failure.step,
                        failure.why,
                        second.step,
                        second.why,
                        self.record_words(record)
                            .map(|words| format!("{}. ", sentence(words)))
                            .unwrap_or_default(),
                        self.power_words(&origin),
                    ),
                })
            }
        }
    }

    /// An edge's rebuilt root holds the image's own Caddyfile: the render of its route record goes there before it
    /// starts. A write Incus does not take is journalled and the update goes on: the entrypoint then puts the copy on
    /// the data volume back, which holds the routes as they were at the edge's last start.
    fn edge_root(&self) {
        if self.config.edge.is_some() {
            // The record as the kernel holds it now, as `record` reads it: a route changed since the call began is in
            // the render too. The lock is let go before the write, which takes it again.
            let now = self.handler.kernel.lock().ok().and_then(|kernel| {
                service_entries(kernel.entries())
                    .into_iter()
                    .find(|(entry, _)| entry.id == self.entry.id)
                    .map(|(_, config)| config)
            });
            let config = now
                .filter(|config| config.edge.is_some())
                .unwrap_or_else(|| self.config.clone());
            let written = self.handler.write_edge_root(self.entry, &config);
            self.journal.step(
                "edge config",
                written.is_ok(),
                match written {
                    Ok(()) => json!({}),
                    Err(why) => json!({ "why": why }),
                },
            );
        }
    }

    /// A service that keeps `/run` in memory has just been given a root (an image's, or its snapshot's), which may keep
    /// files under `/run`: they are removed while the instance is stopped, so its start mounts a memory `/run`. A
    /// removal Incus does not take is journalled and the update goes on, with `/run` on the disk as before.
    /// `memory` is whether the record the instance starts with says so: the template's, when this update writes it, so
    /// a service whose template now keeps `/run` in memory has it from the one start, as a reconfigure gives it.
    fn run_emptied(&self, instance: &InstanceState, memory: bool) {
        if memory {
            let emptied = self.changer.empty_run(instance);
            self.journal.step(
                "empty /run",
                emptied.is_ok(),
                match emptied {
                    Ok(removed) => json!({ "removed": removed }),
                    Err(error) => json!({ "why": incus_words(&error) }),
                },
            );
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
        self.finish_pending(instance)?;
        let held = self.volume_snapshot(instance)?;
        *back = Some(Back {
            volumes: held.clone(),
        });
        self.rebuild_from(instance, self.new_source)
            .map_err(|error| Failure {
                root_untouched: image_not_fetched(&error),
                ..Failure::of("rebuild", &error)
            })?;
        self.journal
            .step("rebuild", true, json!({ "digest": self.digest }));
        self.edge_root();
        // The template's record goes in first: whether `/run` is memory from this start is what it says.
        self.apply_template()?;
        self.run_emptied(instance, self.memory_run());
        self.started(instance)?;
        self.journal.step("start", true, json!({}));
        self.healthy()?;
        self.journal
            .step("health", true, json!({ "port": self.port }));
        self.record()?;
        self.journal.step("record", true, json!({}));
        Ok(held.map(|(snapshot, _)| snapshot))
    }

    /// Finishes the restore an earlier rollback did not: every volume it named goes back to its snapshot, the stopped
    /// instance's own volumes as the restore checks them, and the record of it is removed. A failure keeps the record.
    fn finish_pending(&self, instance: &InstanceState) -> Result<(), Failure> {
        let Some(pending) = self.pending else {
            return Ok(());
        };
        match self
            .changer
            .restore_with_volumes(instance, &pending.snapshot, &pending.parts)
        {
            Ok(_) => {
                self.journal.step(
                    "restore pending",
                    true,
                    json!({ "snapshot": pending.snapshot, "volumes": volume_names(&pending.parts) }),
                );
                self.notes.borrow_mut().push(format!(
                    "its volumes {} were first put back to the snapshot {}, as an earlier rollback had left them part restored",
                    volume_words(&pending.parts),
                    pending.snapshot,
                ));
                if let Err(error) =
                    fs::remove_file(pending_path(&self.handler.cfg, &self.entry.id.0))
                {
                    eprintln!(
                        "arca: the record of a finished restore could not be removed: {error}"
                    );
                }
                Ok(())
            }
            Err(failure) => Err(Failure::said(
                "restore pending",
                format!(
                    "{}; an earlier rollback had left them part restored, and they stay so until a restore of all of them goes through",
                    restore_words(&pending.snapshot, &pending.parts, &failure)
                ),
            )),
        }
    }

    /// Snapshots the service's own volumes, `pre-update-<time>`, and not the instance. A project that refuses the
    /// snapshot (Incus's own words for its `restricted.snapshots`) is given its owner's `snapshots` step, and the
    /// snapshot is taken again. A service that has volumes of its own is never rebuilt without their snapshot: when the
    /// project still refuses it, or the step cannot be written, the update stops there and the old version is started
    /// again. A service with no volume of its own goes on with none. Every update keeps its snapshot, so a name an
    /// earlier one took (two updates in one millisecond) gets `-2`, `-3`, ...: only once every volume snapshotted under
    /// the taken name was removed again.
    fn volume_snapshot(
        &self,
        instance: &InstanceState,
    ) -> Result<Option<(String, Vec<SnapshotPart>)>, Failure> {
        let base = format!("pre-update-{}", (self.journal.clock)());
        let mut snapshot = base.clone();
        let mut taken_name = 1;
        let mut allowed = false;
        let project = &self.config.project;
        loop {
            match self.changer.snapshot_own_volumes(instance, &snapshot) {
                Ok(taken) if taken.taken.is_empty() => {
                    self.journal.step(
                        "snapshot",
                        false,
                        json!({ "skipped": "the service has no volume of its own" }),
                    );
                    return Ok(None);
                }
                Ok(taken) => {
                    let parts: Vec<SnapshotPart> =
                        taken.taken.into_iter().map(|taken| taken.part).collect();
                    self.journal.step(
                        "snapshot",
                        true,
                        json!({ "snapshot": snapshot, "volumes": volume_names(&parts) }),
                    );
                    return Ok(Some((snapshot, parts)));
                }
                Err(failure)
                    if project_refuses_snapshots(&failure.error)
                        && !allowed
                        && failure.done.is_empty()
                        && failure.left.is_empty() =>
                {
                    allowed = true;
                    match self.allow_snapshots() {
                        Ok(true) => {
                            self.journal.step(
                                "snapshots allowed",
                                true,
                                json!({ "project": project, "written": true }),
                            );
                            self.notes.borrow_mut().push(format!(
                                "the owner's entry of the project {project} now lets it hold snapshots, and stays so"
                            ));
                        }
                        Ok(false) => {
                            return Err(Failure::said(
                                "snapshot",
                                format!(
                                    "Incus refuses a snapshot of the service's volumes in the project {project} ({}), although the project's owner already lets it hold snapshots: the node has not applied that to Incus yet. The update does not rebuild without a snapshot to roll the volumes back to",
                                    incus_words(&failure.error)
                                ),
                            ));
                        }
                        Err(error) => {
                            let saved = self.snapshots_saved();
                            if saved {
                                self.notes.borrow_mut().push(format!(
                                    "the owner's entry of the project {project} was saved with its snapshots step, which the node did not apply"
                                ));
                            }
                            return Err(Failure::said(
                                "snapshots allowed",
                                format!(
                                    "{}{}. The update does not rebuild without a snapshot to roll the volumes back to",
                                    call_words(&error),
                                    if saved {
                                        "; the owner's entry was saved with the step, and the node tries it again on its later passes"
                                    } else {
                                        "; the owner's entry was not changed"
                                    },
                                ),
                            ));
                        }
                    }
                }
                Err(failure)
                    if project_refuses_snapshots(&failure.error)
                        && failure.done.is_empty()
                        && failure.left.is_empty() =>
                {
                    return Err(Failure::said(
                        "snapshot",
                        format!(
                            "Incus still refuses a snapshot of the service's volumes in the project {project} ({}) after its owner's entry was given the step. The update does not rebuild without a snapshot to roll the volumes back to",
                            incus_words(&failure.error)
                        ),
                    ));
                }
                Err(failure)
                    if matches!(failure.error, IncusError::Status { code: 409, .. })
                        && failure.left.is_empty()
                        && taken_name < MAX_SNAPSHOT_NAMES =>
                {
                    taken_name += 1;
                    snapshot = format!("{base}-{taken_name}");
                }
                Err(failure) => {
                    return Err(Failure::said(
                        "snapshot",
                        format!("the snapshot {snapshot}: {failure}"),
                    ));
                }
            }
        }
    }

    /// Whether the owner's entry of the service's project has the `snapshots` step saved in the desired document.
    fn snapshots_saved(&self) -> bool {
        match &self.config.tenant {
            Some(tenant) => {
                super::tenants::tenant_snapshots_saved(&self.handler.kernel, &tenant.id)
            }
            None => service_snapshots_saved(&self.handler.kernel, &self.handler.cfg),
        }
    }

    /// The instance read again, refused before anything is written unless it is still the one the update began with:
    /// the same uuid, kind and mark. Only its power state is taken from the read.
    fn reread(&self, origin: &InstanceState) -> Result<InstanceState, Failure> {
        let fresh = self
            .changer
            .read(&origin.project, &origin.name)
            .map_err(|error| Failure::of("read", &error))?;
        if fresh.uuid != origin.uuid
            || fresh.kind != origin.kind
            || fresh.own_config.get(KERNEL_ENTRY_KEY) != origin.own_config.get(KERNEL_ENTRY_KEY)
        {
            return Err(Failure::said(
                "read",
                "another instance has taken the service's place under its name since the update began, and nothing was written to it".to_string(),
            ));
        }
        Ok(fresh)
    }

    /// The service's power state as a read of it shows it now, in words, to end a sentence that is not rolled back.
    fn power_words(&self, origin: &InstanceState) -> String {
        match self.reread(origin) {
            Ok(fresh) if fresh.is_stopped() => "The service is stopped".to_string(),
            Ok(fresh) if fresh.is_running() => "The service is running".to_string(),
            Ok(fresh) => format!("The service reads {}", fresh.status),
            Err(failure)
                if failure.step == "read" && failure.why.starts_with("another instance") =>
            {
                "Another instance is there under the service's name now".to_string()
            }
            Err(_) => "Whether the service runs could not be read".to_string(),
        }
    }

    /// Gives the service's project its owner's `snapshots` step: the tenant's entry for a tenant's service, else the
    /// service project's owner. Under the node's write gate. Answers whether the entry was written.
    fn allow_snapshots(&self) -> Result<bool, CallError> {
        let _write = self.handler.writes.lock().map_err(|_| internal())?;
        let now = (self.handler.clock)();
        match &self.config.tenant {
            Some(tenant) => {
                super::tenants::allow_tenant_snapshots(&self.handler.kernel, &tenant.id, now)
            }
            None => allow_service_snapshots(&self.handler.kernel, &self.handler.cfg, now),
        }
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
                        match self.reach {
                            Reach::Lan(port) | Reach::Private(_, port) | Reach::Direct(port) =>
                                port,
                            Reach::Running => self.port,
                        },
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
                Reach::Private(address, port) => (options.probe)(&address.to_string(), port, true),
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

    /// Gives the stopped instance the root of `source`: a local image's from the project's own store, by its
    /// fingerprint, or an OCI image's from its registry.
    fn rebuild_from(&self, instance: &mut InstanceState, source: &Value) -> Result<(), IncusError> {
        if source.get("fingerprint").is_some() {
            self.changer.rebuild_from_local(instance, source)
        } else {
            self.changer
                .rebuild_from_registry(instance, source, self.registry)
        }
    }

    /// Puts the old version back, and starts it when it ran before the update. The record was never changed before
    /// the health check passed, so it still names the old image. Every step is sent only to the instance the update
    /// began with (`origin`'s uuid, kind and mark). A failed rebuild from the old image after a rebuild from the new one
    /// that failed on its image leaves the old root as it was (Incus fetches the image before it takes the root away),
    /// so the old version is simply started; after any other failure of the new rebuild the root may be the new one,
    /// and nothing is started on it.
    fn roll_back(
        &self,
        instance: &mut InstanceState,
        origin: &InstanceState,
        back: &Back,
        failure: &Failure,
    ) -> Result<Rolled, (Failure, Option<bool>)> {
        // The template's settings go back first, whatever happens to the root after: the node writes the old config
        // while the instance is still stopped (or running, which takes it at its next start).
        let record = self.put_template_back();
        self.root_back(instance, origin, back, failure, record)
            .map_err(|failure| (failure, record))
    }

    fn root_back(
        &self,
        instance: &mut InstanceState,
        origin: &InstanceState,
        back: &Back,
        failure: &Failure,
        record: Option<bool>,
    ) -> Result<Rolled, Failure> {
        // Read again first: a new version that did not stay running has stopped by itself, and a stop of it is refused.
        *instance = self.reread(origin)?;
        self.stopped(instance)?;
        let mut root_kept = false;
        match self.rebuild_from(instance, &self.config.source) {
            Ok(()) => {
                self.journal.step("rebuild back", true, json!({}));
                self.edge_root();
                self.run_emptied(instance, self.config.run_tmpfs);
            }
            Err(error)
                if failure.step == "rebuild"
                    && failure.root_untouched
                    && !matches!(error, IncusError::Timeout) =>
            {
                root_kept = true;
                self.journal
                    .step("rebuild back", false, json!({ "why": incus_words(&error) }));
            }
            Err(error) if failure.step == "rebuild" => {
                return Err(Failure::said(
                    "rebuild back",
                    format!(
                        "{}. The rebuild from the new image failed after Incus may have taken the old root away, so the root there may be the new version's, and it was not started",
                        incus_words(&error)
                    ),
                ));
            }
            Err(error) => return Err(Failure::of("rebuild back", &error)),
        }
        // What the new version wrote to its volumes is undone: each goes back to the snapshot taken before the rebuild.
        if let Some((snapshot, parts)) = &back.volumes {
            self.changer
                .restore_with_volumes(instance, snapshot, parts)
                .map_err(|failure| self.restore_failed(snapshot, parts, &failure))?;
            self.journal.step(
                "restore",
                true,
                json!({ "snapshot": snapshot, "volumes": volume_names(parts) }),
            );
        }
        *instance = self.reread(origin)?;
        self.start_again(instance)?;
        let mut answers = true;
        if self.was_running {
            self.journal.step("start", true, json!({ "old": true }));
            answers = self.answers_within(self.seconds.min(OLD_ANSWERS_SECONDS));
            self.journal.step("old answers", answers, json!({}));
        }
        Ok(Rolled {
            root_kept,
            answers,
            record,
        })
    }

    /// A rollback's restore of the volumes that failed: it is kept on file, so the next update finishes it before it
    /// does anything else, and said with what went back and what did not.
    fn restore_failed(
        &self,
        snapshot: &str,
        parts: &[SnapshotPart],
        failure: &arca_incus::PartFailure,
    ) -> Failure {
        let pending = Pending {
            snapshot: snapshot.to_string(),
            parts: parts.to_vec(),
            restored: failure.done.clone(),
        };
        let kept = pending.write(&self.handler.cfg, &self.entry.id.0, &self.journal.id);
        self.journal.step(
            "restore",
            false,
            json!({
                "snapshot": snapshot,
                "volumes": volume_names(parts),
                "restored": volume_names(&failure.done),
                "failed": volume_names(std::slice::from_ref(&failure.part)),
                "pendingKept": kept.is_ok(),
            }),
        );
        Failure::said(
            "restore",
            format!(
                "{}. {}",
                restore_words(snapshot, parts, failure),
                match kept {
                    Ok(()) => format!(
                        "The next update of the service puts every one of them back to {snapshot} first, once the service is stopped"
                    ),
                    Err(error) => format!(
                        "The node could not keep that on file ({error}): restore every one of them to {snapshot} before the service runs again"
                    ),
                },
            ),
        )
    }

    /// With the template: its record is written, under the node's write gate and only while the record is the one the
    /// card was made from. The node writes the instance's config from it while the instance is stopped, so the one
    /// start that follows takes the new image and the new settings together.
    fn apply_template(&self) -> Result<(), Failure> {
        let Some(ready) = self.template.and_then(|step| step.ready.as_ref()) else {
            return Ok(());
        };
        let step = self.template.expect("a template step");
        match self
            .handler
            .write_plan(&self.entry.id.0, self.config, &ready.next)
        {
            Written::Done => {
                step.written.set(true);
                self.journal.step("apply", true, json!({}));
                Ok(())
            }
            Written::Stale { .. } => Err(Failure::said(
                "apply",
                "the service's record changed since the card was made (a link or a secret was set meanwhile), so its template's settings were not written: propose it again".to_string(),
            )),
            Written::Refused(why) => {
                step.written.set(true);
                Err(Failure::said(
                    "apply",
                    format!("the node did not apply the template's settings: {why}"),
                ))
            }
        }
    }

    /// Whether the instance keeps `/run` in memory from the start this update gives it: as the template's record says
    /// when this update writes one, else as the service's record does.
    fn memory_run(&self) -> bool {
        self.template
            .and_then(|step| step.ready.as_ref())
            .map_or(self.config.run_tmpfs, |ready| ready.next.run_tmpfs)
    }

    /// Puts the old record back when the template's was written: `None` when there was nothing to put back, else whether
    /// it worked.
    fn put_template_back(&self) -> Option<bool> {
        let step = self.template.filter(|step| step.written.get())?;
        let restored = self
            .handler
            .put_plan_back(&self.entry.id.0, &step.old, true);
        self.journal.step("put back", restored, json!({}));
        Some(restored)
    }

    /// What a refusal says of the record a rollback put back, or `None` when there was nothing to put back.
    fn record_words(&self, record: Option<bool>) -> Option<&'static str> {
        record.map(|restored| {
            if restored {
                "its old settings are back in its record and its instance's config, its entrypoint among them"
            } else {
                "its old settings could not be put back in its record (service_status shows what it says)"
            }
        })
    }

    /// The config read back after the start, against the template's settings that were applied: the keys that differ,
    /// never a value. Journalled.
    fn read_back(&self, template: &TemplateStep) -> Vec<String> {
        let Some(ready) = &template.ready else {
            return Vec::new();
        };
        let differs = match self
            .changer
            .read(&self.config.project, &self.config.instance)
        {
            Ok(state) => service_reconfigure::read_back_differs(
                &state,
                &ready.applied,
                self.config,
                &ready.next,
            ),
            Err(_) => vec!["(the instance could not be read)".to_string()],
        };
        self.journal.step(
            "read back",
            differs.is_empty(),
            json!({ "differs": differs }),
        );
        differs
    }
}

/// `words` with its first letter in upper case, to begin a sentence.
fn sentence(words: &str) -> String {
    let mut letters = words.chars();
    letters
        .next()
        .map(|first| first.to_uppercase().chain(letters).collect())
        .unwrap_or_default()
}

/// What a rollback did.
struct Rolled {
    /// The old root was never taken away.
    root_kept: bool,
    /// The old version answered after it was started.
    answers: bool,
    /// With the template: whether its old record was put back, or `None` when there was nothing to put back.
    record: Option<bool>,
}
