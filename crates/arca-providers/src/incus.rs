//! The kernel's Incus write provider. It is the only code path in the kernel that writes to Incus. It sends only the writer's
//! checked requests: a POST for a create, a config change under an `If-Match` (a PATCH, or for a project a PUT of its whole
//! config), one ACL rule array under `If-Match`, and the tombstone-then-DELETE sequence for a retirement. Each effect is planned read-only first. `plan` refuses
//! before anything is journalled, and `effect` checks the family, the features, the ownership and the plan again before it
//! applies anything. `docs/OWNERSHIP.md` states the rules.
//!
//! A write that Incus answered with an error, or that did not finish, may still have landed. The provider reads the object back
//! before it decides the outcome, unless the answer could not be read at all: then it makes no read, since a read after a write
//! still running proves nothing. It reports a failure only when the object shows the write did not happen, and reports
//! [`EffectRefusal::Unresolved`] for anything it cannot prove either way.
//!
//! The read-back that settles an intent after a restart, or finds a removed entry's object gone, reads the object only once no
//! operation Incus still runs that may write it names it: it waits on each such operation up to the operation deadline, and an
//! operation still running then, or an operations listing that cannot be read, is [`EffectRefusal::Unresolved`]. An exec or a
//! console session, and a token operation, write nothing and hold nothing (`arca_incus::RunningOp::may_write`).

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arca_incus::write::OPERATION_DEADLINE;
use arca_incus::{
    IncusError, IncusRead, Owned, ProjectFamily, Reading, UnixIncus, UnixIncusWriter,
    wait_operation, writing_on,
};
use arca_kernel::canon::Digest;
use arca_kernel::desired::{EntryId, PluginId};
use arca_kernel::journal::{AclPatch, EffectClass, Inverse};
use arca_kernel::ownership::{
    IncusKind, KERNEL_ENTRY_KEY, KERNEL_RETIRING_KEY, ObjectRef, classify_object, kernel_may_write,
};
pub use arca_kernel::plugin::Retirement;
use arca_kernel::plugin::{
    EffectOp, EffectRefusal, Landed, Observe, Planned, Provider, ReadBack, op_digest,
};
use serde_json::{Map, Value, json};

/// The project a network, a forward and an ACL are addressed in. The kernel's bridges live here, a forward hangs off its network,
/// and Incus 7.3 keeps a kernel project's ACLs here.
pub use arca_incus::write::NETWORK_HOME;
/// The one image server, and its protocol, an instance's source may name. Incus pulls the image into the project's own store.
pub use arca_incus::write::{IMAGE_PROTOCOL, IMAGE_SERVER};
/// The two projects the kernel never writes, whatever its family names. Incus gives them to nobody else, and droidd and the
/// host use them. The one exception is a network, a forward and an ACL, which the kernel addresses in `default` (Rule 40 of the
/// ownership document) and nowhere else.
pub const DROIDD_PROJECTS_FIXED: [&str; 2] = ["bench", "default"];
/// The prefix every kernel project carries in a scratch run.
const SCRATCH_PREFIX: &str = "arcak-scratch-";
/// The prefix of a config value that names a secret file rather than being the value itself.
pub const SECRET_PREFIX: &str = "arca-secret:";
const FOREIGN_SECRET_MARKER: &str = "<redacted: another entry's secret>";
/// The largest secret file the provider reads, in bytes.
const SECRET_MAX_BYTES: u64 = 64 * 1024;
/// How deep the provider looks for secret files under its directory.
const SECRET_MAX_DEPTH: usize = 4;
/// The project keys every kernel project is created with. A plugin may not set any `restricted*` or `features*` key another way.
/// `features.networks` is not among them: the kernel's bridges, forwards and ACLs live in the default project, which Incus
/// shares with a project that does not enable networks. `features.images` is: a kernel project keeps its own image store, so an
/// image an instance pulls lands inside the project and never in the default project's store, and the project's retirement
/// deletes it. Incus 7.3 sets it by default; the kernel sets it so it never depends on that.
const PROJECT_KEYS: [(&str, &str); 5] = [
    ("features.images", "true"),
    ("features.networks.zones", "true"),
    ("features.profiles", "true"),
    ("features.storage.volumes", "true"),
    ("features.storage.buckets", "true"),
];
/// The key every kernel project is created restricted with.
const RESTRICTED_KEY: &str = "restricted";
/// The longest one wait on a running operation is held by Incus while a read-back waits for it.
const READ_BACK_HOLD: Duration = Duration::from_secs(5);
/// How much longer than its hold one wait request may take before it is a timeout.
const READ_BACK_MARGIN: Duration = Duration::from_secs(25);
/// The pause before the operations are listed again after a wait, so a server that answers at once is not spun on.
const READ_BACK_PAUSE: Duration = Duration::from_millis(100);
/// The restriction key a plugin may set on a project create: the bridges the project's instances may use. Each name must be a
/// bridge in the default project that carries the entry's mark.
pub const NETWORK_ACCESS_KEY: &str = "restricted.networks.access";
/// The config keys a bridge network may set. A plan's bridge sets each of them, and nothing else: a route, an external interface,
/// a `raw.*` key, DNS and every other key is refused.
const BRIDGE_KEYS: [&str; 3] = ["ipv4.address", "ipv4.nat", "ipv6.address"];

/// The two entries that may own the service project: the managed network's and the bridged project's. Whichever made the
/// project widens it when the node gains the other network (Rules 54 and 55 of the ownership document).
const SERVICE_PROJECT: &str = "arcak-svc";
const NET_ENTRY: &str = "net-arcak-svc";
const BRIDGED_ENTRY: &str = "bridged-arcak-svc";

// Only an entry that may own the service project may grant proxy devices, and only in that project.
fn service_proxy_allowance(entry: &EntryId, project: &str, key: &str, value: Option<&str>) -> bool {
    (entry.0 == NET_ENTRY || entry.0 == BRIDGED_ENTRY)
        && project == SERVICE_PROJECT
        && key == "restricted.devices.proxy"
        && value == Some("allow")
}

// Only an entry that may own the service project may let a NIC name a host bridge Incus does not manage, and only in that
// project: the bridged entry when it creates the project, the network entry by a patch when its node gains host bridges.
fn host_nic_allowance(
    entry: &EntryId,
    project: &str,
    key: &str,
    value: Option<&str>,
    create: bool,
) -> bool {
    entry.0 == if create { BRIDGED_ENTRY } else { NET_ENTRY }
        && project == SERVICE_PROJECT
        && key == "restricted.devices.nic"
        && value == Some("allow")
}

// Only the network entry may drop the service project's bridge grant, with the patch that allows host-bridge NICs: Incus
// checks a host bridge against the grant, and the grant may list only bridges the entry made.
fn bridge_grant_removal(entry: &EntryId, project: &str, key: &str, value: Option<&str>) -> bool {
    entry.0 == NET_ENTRY
        && project == SERVICE_PROJECT
        && key == NETWORK_ACCESS_KEY
        && value.is_none()
}

/// The managed service bridge every tenant's project is granted.
const SERVICE_BRIDGE: &str = "arcak-svcbr";

/// The restrictions a tenant's entry may set on its own project's create, each at its tighter value (Rule 62).
const TENANT_RESTRICTIONS: [(&str, &str); 8] = [
    ("restricted.devices.proxy", "block"),
    ("restricted.devices.gpu", "block"),
    ("restricted.devices.disk", "managed"),
    ("restricted.devices.nic", "managed"),
    ("restricted.containers.nesting", "block"),
    ("restricted.containers.privilege", "unprivileged"),
    ("restricted.containers.lowlevel", "block"),
    ("restricted.virtual-machines.lowlevel", "block"),
];

/// Whether `entry` is a tenant's and `project` is that tenant's own project.
fn tenant_owns(entry: &EntryId, project: &str) -> bool {
    arca_kernel::ownership::tenant_of_entry(entry)
        .is_some_and(|id| arca_kernel::ownership::tenant_of_project(project) == Some(id))
}

// A tenant's entry may tighten its own project at its create, and nothing else.
fn tenant_restriction(entry: &EntryId, project: &str, key: &str, value: Option<&str>) -> bool {
    tenant_owns(entry, project)
        && TENANT_RESTRICTIONS
            .iter()
            .any(|(allowed, set)| *allowed == key && value == Some(*set))
}

/// Whether a project key is guarded: `restricted`, any `restricted.*` key, and any `features.*` key.
fn guarded_project_key(key: &str) -> bool {
    key == RESTRICTED_KEY || key.starts_with("restricted.") || key.starts_with("features.")
}

/// The entries whose plans were declined, each with the plugin and the plan it was declined under. Every clone shares one set, so a
/// reconciler that gives the boxed provider away can still update it, and the provider reads the set at every plan, effect and
/// retirement. The set is never absent: a provider is built only with one, and "nothing retained" is [`RetainedSet::empty`].
#[derive(Debug, Clone)]
pub struct RetainedSet(Arc<RwLock<BTreeMap<EntryId, (PluginId, Digest)>>>);

impl RetainedSet {
    /// The set that holds `entries`.
    pub fn new(entries: BTreeMap<EntryId, (PluginId, Digest)>) -> Self {
        RetainedSet(Arc::new(RwLock::new(entries)))
    }

    /// The set that retains no entry, written out so that it is never mistaken for a set that was not given.
    pub fn empty() -> Self {
        Self::new(BTreeMap::new())
    }

    /// Retains `entry`'s objects under the plugin and the declined plan. Every provider holding a clone of this set sees it.
    pub fn retain(
        &self,
        entry: EntryId,
        plugin: PluginId,
        plan: Digest,
    ) -> Result<(), EffectRefusal> {
        self.0
            .write()
            .map_err(|_| unreadable_retained_set())?
            .insert(entry, (plugin, plan));
        Ok(())
    }

    /// Whether `entry` is retained. A set whose lock was poisoned is refused, so the caller never reads it as "not retained".
    pub fn holds(&self, entry: &EntryId) -> Result<bool, EffectRefusal> {
        Ok(self.snapshot()?.contains_key(entry))
    }

    /// Every retained entry with the plugin and the declined plan it was retained under, so the set can be written to disk. A
    /// set whose lock was poisoned is refused.
    pub fn entries(&self) -> Result<BTreeMap<EntryId, (PluginId, Digest)>, EffectRefusal> {
        self.snapshot()
    }

    /// A copy of the set as it is now. A set whose lock was poisoned is refused, never read as empty.
    fn snapshot(&self) -> Result<BTreeMap<EntryId, (PluginId, Digest)>, EffectRefusal> {
        self.0
            .read()
            .map(|set| set.clone())
            .map_err(|_| unreadable_retained_set())
    }
}

/// The projects of the tenants whose entries exist: the part of the project family that follows the desired state. Every
/// clone shares one set. The kernel keeps it: a tenant's project is in it from the moment its entry is in the desired
/// state until nothing of the entry is left on the node, and no other name ever is. The names are built here from the
/// tenants' ids, so each carries the tenant prefix (the scratch one in a scratch run).
#[derive(Debug, Clone, Default)]
pub struct TenantProjects {
    scratch: bool,
    projects: Arc<RwLock<std::collections::BTreeSet<String>>>,
}

impl TenantProjects {
    /// The empty set of a node, for a scratch run or not.
    pub fn new(scratch: bool) -> Self {
        TenantProjects {
            scratch,
            projects: Arc::default(),
        }
    }

    /// Replaces the set with the projects of `entries`. An entry that is not a tenant's (`tenant-<id>`) adds nothing.
    pub fn set<'a>(&self, entries: impl IntoIterator<Item = &'a EntryId>) {
        let next = entries
            .into_iter()
            .filter_map(arca_kernel::ownership::tenant_of_entry)
            .map(|id| arca_kernel::ownership::tenant_project(id, self.scratch))
            .collect();
        if let Ok(mut projects) = self.projects.write() {
            *projects = next;
        }
    }

    /// The set as it is now. A set whose lock was poisoned is refused, never read as empty.
    pub fn snapshot(&self) -> Result<std::collections::BTreeSet<String>, EffectRefusal> {
        self.projects.read().map(|set| set.clone()).map_err(|_| {
            EffectRefusal::Provider(
                "the tenant set is unreadable, so nothing is written".to_string(),
            )
        })
    }
}

fn unreadable_retained_set() -> EffectRefusal {
    EffectRefusal::Provider("the retained set is unreadable, so nothing is written".to_string())
}

/// The projects the kernel may write in, and the rules its configuration gives them. A family is refused at
/// [`IncusProvider::new`] when it names a reserved or a droidd project, a project outside the scratch prefix in a scratch run, or
/// a reserved range that is not an IPv4 CIDR.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Family {
    /// The kernel's projects.
    pub projects: Vec<String>,
    /// Whether this is a scratch run, in which every project carries [`SCRATCH_PREFIX`].
    pub scratch: bool,
    /// The projects droidd uses. The kernel refuses to write any of them.
    pub droidd_projects: Vec<String>,
    /// IPv4 ranges a kernel bridge must not overlap, each in CIDR form.
    pub reserved_cidrs: Vec<String>,
}

impl Family {
    /// Whether the kernel may write in `project`: it is listed, it is not reserved or droidd's, and in a scratch run it has the
    /// prefix. The writer checks the same family again before it sends a request.
    fn allows(&self, project: &str) -> bool {
        self.projects.iter().any(|listed| listed == project)
            && !DROIDD_PROJECTS_FIXED.contains(&project)
            && !self.droidd_projects.iter().any(|listed| listed == project)
            && (!self.scratch || project.starts_with(SCRATCH_PREFIX))
    }

    /// The writer's family for this configuration, after every rule the configuration must meet.
    fn validate(&self) -> Result<ProjectFamily, EffectRefusal> {
        if self.scratch
            && self
                .projects
                .iter()
                .any(|project| !project.starts_with(SCRATCH_PREFIX))
        {
            return Err(EffectRefusal::Provider(
                "in a scratch run every kernel project carries the scratch prefix".to_string(),
            ));
        }
        if self
            .reserved_cidrs
            .iter()
            .any(|range| parse_cidr(range).is_none())
        {
            return Err(EffectRefusal::Provider(
                "a reserved range is not an IPv4 CIDR".to_string(),
            ));
        }
        ProjectFamily::new(
            self.projects.iter().map(String::as_str),
            self.droidd_projects.iter().map(String::as_str),
        )
        .map_err(EffectRefusal::Provider)
    }
}

/// The Incus write provider. It writes only inside its family, only the objects an entry owns, and only through the writer.
#[derive(Debug)]
pub struct IncusProvider {
    socket: PathBuf,
    reader: UnixIncus,
    family: Family,
    writer_family: ProjectFamily,
    secrets_dir: PathBuf,
    uid: u32,
    deadline: Option<Duration>,
    retained: RetainedSet,
    tenants: TenantProjects,
}

impl IncusProvider {
    /// The same provider with the tenant projects the kernel keeps. Without it no tenant project is in the family.
    pub fn with_tenants(mut self, tenants: TenantProjects) -> Self {
        self.tenants = tenants;
        self
    }

    /// Whether the kernel may write in `project`: the configured family, or the project of a tenant whose entry exists.
    /// A tenant's project carries the tenant prefix of this run and is never a reserved or a droidd project.
    fn allows(&self, project: &str) -> bool {
        if self.family.allows(project) {
            return true;
        }
        let prefix = if self.family.scratch {
            arca_kernel::ownership::SCRATCH_TENANT_PROJECT_PREFIX
        } else {
            arca_kernel::ownership::TENANT_PROJECT_PREFIX
        };
        project
            .strip_prefix(prefix)
            .is_some_and(arca_kernel::ownership::tenant_id_ok)
            && !self.family.droidd_projects.iter().any(|p| p == project)
            && self
                .tenants
                .snapshot()
                .is_ok_and(|set| set.contains(project))
    }

    /// A provider over the Incus socket at `socket`, for `family`, with secret files under `secrets_dir`, and with the entries
    /// in `retained` never written. The family is refused when it breaks a rule of [`Family`].
    pub fn new(
        socket: PathBuf,
        family: Family,
        secrets_dir: PathBuf,
        retained: RetainedSet,
    ) -> Result<Self, EffectRefusal> {
        let writer_family = family.validate()?;
        let uid = process_uid()?;
        Ok(IncusProvider {
            reader: UnixIncus::new(socket.clone()),
            socket,
            family,
            writer_family,
            secrets_dir,
            uid,
            deadline: None,
            retained,
            tenants: TenantProjects::default(),
        })
    }

    /// The same provider with another overall deadline for an Incus operation.
    pub fn with_operation_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = Some(deadline);
        self
    }

    /// The writer for `entry`, with this provider's family, the retained set as it is now, and its deadline. A set that cannot
    /// be read refuses the write.
    fn writer(&self, entry: &EntryId) -> Result<UnixIncusWriter, EffectRefusal> {
        let tenants = self.tenants.snapshot()?;
        let family = self
            .writer_family
            .clone()
            .with_tenants(
                tenants
                    .iter()
                    .map(String::as_str)
                    .filter(|project| self.allows(project)),
            )
            .map_err(EffectRefusal::Provider)?;
        let writer = UnixIncusWriter::new(
            self.socket.clone(),
            entry.clone(),
            self.retained.snapshot()?,
            family,
        );
        Ok(match self.deadline {
            Some(deadline) => writer.with_operation_deadline(deadline),
            None => writer,
        })
    }

    /// Refuses a target outside the family before any request is made. A network, a forward and an ACL are addressed in the
    /// default project and in no other; every other kind is addressed in a kernel project.
    fn check_family(&self, kind: IncusKind, project: &str) -> Result<(), EffectRefusal> {
        let allowed = if arca_incus::write::network_home(kind) {
            project == NETWORK_HOME
        } else {
            self.allows(project)
        };
        if allowed {
            Ok(())
        } else {
            Err(EffectRefusal::ProjectNotAllowed)
        }
    }

    /// Where Incus holds the object a journal record names. A network, a forward and an ACL are addressed in the default project.
    /// A record an earlier kernel wrote may name an ACL in its kernel project instead: Incus 7.3 keeps the ACLs of a project that
    /// does not enable `features.networks` in the default project, so the ACL was made there, and it is read and retired there.
    /// The project's own config is read first. A project that enables `features.networks` keeps its own ACLs, so its record keeps
    /// its address, which the family refuses. A project that is gone holds nothing, so its record is addressed in the default
    /// project. Any other kind, and a record already addressed in the default project, is returned as it is.
    fn served_at(&self, entry: &EntryId, object: &ObjectRef) -> Result<ObjectRef, EffectRefusal> {
        if !arca_incus::write::network_home(object.kind)
            || object.project == NETWORK_HOME
            || !self.allows(&object.project)
        {
            return Ok(object.clone());
        }
        let project = ObjectRef {
            kind: IncusKind::Project,
            project: object.project.clone(),
            parent: None,
            name: object.project.clone(),
            uuid: None,
        };
        let enables = match self.writer(entry)?.get_etag(&project) {
            Ok((metadata, _)) => {
                text_config(metadata.get("config"))
                    .get("features.networks")
                    .map(String::as_str)
                    == Some("true")
            }
            Err(IncusError::Status { code: 404, .. }) => false,
            Err(error) => return Err(incus_refusal(error)),
        };
        if enables {
            return Ok(object.clone());
        }
        Ok(ObjectRef {
            project: NETWORK_HOME.to_string(),
            ..object.clone()
        })
    }

    /// The config of a project, read with a GET. A project's features live in its own config.
    fn project_config(
        &self,
        entry: &EntryId,
        project: &str,
    ) -> Result<BTreeMap<String, String>, EffectRefusal> {
        let target = ObjectRef {
            kind: IncusKind::Project,
            project: project.to_string(),
            parent: None,
            name: project.to_string(),
            uuid: None,
        };
        let (metadata, _) = self
            .writer(entry)?
            .get_etag(&target)
            .map_err(incus_refusal)?;
        Ok(text_config(metadata.get("config")))
    }

    /// Refuses a write whose project does not enable the feature its kind needs. Without the feature, Incus routes the write to
    /// the default project. An instance and a project need no feature, and neither does a network, a forward or an ACL: those are
    /// addressed in the default project, which holds them. The check runs for every write, a delete included.
    fn check_features(
        &self,
        entry: &EntryId,
        kind: IncusKind,
        project: &str,
    ) -> Result<(), EffectRefusal> {
        let needed = match kind {
            IncusKind::Project
            | IncusKind::Instance
            | IncusKind::Network
            | IncusKind::Forward
            | IncusKind::Acl => return Ok(()),
            IncusKind::Zone => "features.networks.zones",
            IncusKind::Profile => "features.profiles",
            IncusKind::Volume => "features.storage.volumes",
            IncusKind::Bucket => "features.storage.buckets",
        };
        let config = self.project_config(entry, project)?;
        if config.get(needed).map(String::as_str) == Some("true") {
            Ok(())
        } else {
            Err(EffectRefusal::Provider(format!(
                "the project does not enable {needed}"
            )))
        }
    }

    /// Reads an object for a write the plan makes, and refuses an object the entry may not change. A change that names a uuid
    /// must see that uuid.
    fn read_for_plan(&self, entry: &EntryId, object: &ObjectRef) -> Result<Reading, EffectRefusal> {
        let reading = self
            .writer(entry)?
            .read_class(object)
            .map_err(incus_refusal)?;
        if !kernel_may_write(&reading.class, entry) {
            return Err(EffectRefusal::NotOwned(reading.class));
        }
        if object.uuid.is_some() && object.uuid != reading.uuid {
            return Err(EffectRefusal::Provider(
                "the object is not the one the op names".to_string(),
            ));
        }
        Ok(reading)
    }

    /// The owned value an effect writes through. A refusal names the owner the read found, when it found one.
    fn owned_for_write(
        &self,
        entry: &EntryId,
        writer: &UnixIncusWriter,
        object: &ObjectRef,
    ) -> Result<Owned, EffectRefusal> {
        match writer.read_owned(object) {
            Ok(owned) => Ok(owned),
            Err(IncusError::Refused(reason)) => match writer.read_class(object) {
                Ok(reading) if !kernel_may_write(&reading.class, entry) => {
                    Err(EffectRefusal::NotOwned(reading.class))
                }
                _ => Err(EffectRefusal::Provider(reason)),
            },
            Err(error) => Err(incus_refusal(error)),
        }
    }

    /// The checks a create's body must pass before any request: the mark names this entry, a project's kernel keys are not set
    /// another way, a bridge fits the reserved ranges, and every secret reference resolves to a usable file. The result is the
    /// body the writer sends, with each secret reference replaced by the file's content.
    fn prepared_create(
        &self,
        entry: &EntryId,
        kind: IncusKind,
        project: &str,
        parent: Option<&str>,
        name: &str,
        body: &Value,
    ) -> Result<Value, EffectRefusal> {
        if kind == IncusKind::Project && project != name {
            return Err(EffectRefusal::Provider(
                "a project is named by its own name".to_string(),
            ));
        }
        let Value::Object(fields) = body else {
            return Err(EffectRefusal::Provider(
                "a create body is not an object".to_string(),
            ));
        };
        let mut config = match fields.get("config") {
            None => Map::new(),
            Some(Value::Object(config)) => config.clone(),
            Some(_) => {
                return Err(EffectRefusal::Provider(
                    "a create's config is not an object".to_string(),
                ));
            }
        };
        if config
            .get(KERNEL_ENTRY_KEY)
            .is_some_and(|mark| mark.as_str() != Some(entry.0.as_str()))
        {
            return Err(EffectRefusal::Provider(
                "the create names another entry's mark".to_string(),
            ));
        }
        if config.contains_key(KERNEL_RETIRING_KEY) {
            return Err(EffectRefusal::Provider(
                "a create may not carry a retiring tombstone".to_string(),
            ));
        }
        config.insert(KERNEL_ENTRY_KEY.to_string(), Value::String(entry.0.clone()));
        if kind == IncusKind::Project {
            let pinned: Vec<(&str, &str)> = PROJECT_KEYS
                .iter()
                .copied()
                .chain([(RESTRICTED_KEY, "true")])
                .collect();
            // A plugin may set a guarded key only to the value the kernel pins it at. Any other guarded key, pinned or not,
            // would loosen the project, so it is refused before any request. Bridge grants are checked name by name;
            // an entry that may own the service project may also allow proxy devices there, and the bridged project
            // entry may allow NICs on a host bridge there.
            for (key, set) in &config {
                if key == NETWORK_ACCESS_KEY {
                    let list = set.as_str().ok_or_else(|| {
                        EffectRefusal::Provider("the bridge grant is not text".to_string())
                    })?;
                    // A tenant's project is granted the node's managed service bridge and no other. The bridge is
                    // the network entry's, not the tenant's: it must exist and carry that entry's mark.
                    if tenant_owns(entry, project) {
                        if list != SERVICE_BRIDGE {
                            return Err(EffectRefusal::Provider(
                                "a tenant's project is granted the managed service bridge only"
                                    .to_string(),
                            ));
                        }
                        let bridge = self
                            .reader
                            .get(&format!(
                                "/1.0/networks/{SERVICE_BRIDGE}?project={NETWORK_HOME}"
                            ))
                            .map_err(incus_refusal)?;
                        if bridge["config"][KERNEL_ENTRY_KEY].as_str() != Some(NET_ENTRY) {
                            return Err(EffectRefusal::Provider(
                                "the managed service bridge is not one Arca made on this node"
                                    .to_string(),
                            ));
                        }
                        continue;
                    }
                    self.check_network_access(entry, list)?;
                    continue;
                }
                if tenant_restriction(entry, project, key, set.as_str()) {
                    continue;
                }
                if service_proxy_allowance(entry, project, key, set.as_str()) {
                    continue;
                }
                if host_nic_allowance(entry, project, key, set.as_str(), true) {
                    continue;
                }
                if !guarded_project_key(key) {
                    continue;
                }
                let kept = pinned
                    .iter()
                    .any(|(pin, value)| *pin == key.as_str() && set.as_str() == Some(*value));
                if !kept {
                    return Err(EffectRefusal::Provider(format!(
                        "a plugin may not set {key} on a project, except to the value the kernel pins"
                    )));
                }
            }
            for (key, value) in pinned {
                config.insert(key.to_string(), Value::String(value.to_string()));
            }
        }
        if kind == IncusKind::Network {
            check_bridge_create(fields, &self.family.reserved_cidrs)?;
        }
        if kind == IncusKind::Forward {
            // A forward is made under its network, so the network must be the entry's own. A forward on another owner's bridge
            // would change that bridge.
            let network = parent.ok_or_else(|| {
                EffectRefusal::Provider("a forward names its network".to_string())
            })?;
            self.read_for_plan(entry, &network_home_target(network))?;
        }
        let mut text = BTreeMap::new();
        for (key, value) in &config {
            let Some(value) = value.as_str() else {
                return Err(EffectRefusal::Provider(
                    "a config value in a create is not text".to_string(),
                ));
            };
            text.insert(key.clone(), value.to_string());
        }
        let mut resolved = Map::new();
        for (key, value) in &text {
            resolved.insert(key.clone(), Value::String(self.resolve_text(entry, value)?));
        }
        let mut sent = fields.clone();
        sent.insert("config".to_string(), Value::Object(resolved));
        Ok(Value::Object(sent))
    }

    /// Checks the bridges a project grants its instances. The list is comma-separated, and each name is a plain name of a bridge in
    /// the default project that carries the entry's mark. One name that fails refuses the whole list.
    fn check_network_access(&self, entry: &EntryId, list: &str) -> Result<(), EffectRefusal> {
        for name in list.split(',') {
            if !plain_name(name) {
                return Err(EffectRefusal::Provider(
                    "the bridge grant names a network that is not a plain name".to_string(),
                ));
            }
            self.read_for_plan(entry, &network_home_target(name))?;
        }
        Ok(())
    }

    /// The checks a change must pass against the object's current config: the keys are plain and not reserved, the object stays
    /// the entry's own, and a network stays a bridge inside the rules.
    fn check_change(
        &self,
        entry: &EntryId,
        object: &ObjectRef,
        changes: &BTreeMap<String, Option<String>>,
        current: &BTreeMap<String, String>,
    ) -> Result<(), EffectRefusal> {
        check_patch_keys(entry, object, changes)?;
        let after = apply_changes(current, changes);
        if !kernel_may_write(
            &classify_object(object, &after, &self.retained.snapshot()?),
            entry,
        ) {
            return Err(EffectRefusal::Provider(
                "the change would leave the object owned by another owner".to_string(),
            ));
        }
        if object.kind == IncusKind::Network {
            check_bridge_config(&after, &self.family.reserved_cidrs)?;
        }
        Ok(())
    }

    /// A change's or a delete's read-only plan: the family, the owner, the features, the ETag, the pre-image of each changed key,
    /// and the class. The pre-image holds a secret's reference where the value is a secret's content.
    fn plan_for_object(
        &self,
        entry: &EntryId,
        object: &ObjectRef,
        changes: Option<&BTreeMap<String, Option<String>>>,
        digest: Digest,
    ) -> Result<Planned, EffectRefusal> {
        self.check_family(object.kind, &object.project)?;
        // The key rules come before any read, so a refused key sends no request at all.
        if let Some(changes) = changes {
            check_patch_keys(entry, object, changes)?;
        }
        let reading = self.read_for_plan(entry, object)?;
        self.check_features(entry, object.kind, &object.project)?;
        let class = class_of(object.kind);
        let Some(changes) = changes else {
            return Ok(Planned {
                acl_patch: None,
                class,
                op_digest: digest,
                pre_image: None,
                etag: Some(reading.etag),
            });
        };
        self.check_change(entry, object, changes, &reading.config)?;
        self.resolve_changes(entry, changes)?;
        let pre_image = self.pre_image_of(entry, changes, &reading.config)?;
        Ok(Planned {
            acl_patch: None,
            class,
            op_digest: digest,
            pre_image: Some(pre_image),
            etag: Some(reading.etag),
        })
    }

    /// The pre-image of a patch, as the journal holds it. For each changed key the object holds a value, that value is journalled
    /// as a current secret's reference when its content is one, and otherwise as the value itself, which the inverse restores.
    /// A key the patch sets to a secret reference, and whose value is no current secret, is journalled as `None`: a redacted
    /// marker whose inverse removes the key. A key the object does not hold is `None`. A value that starts with the reference
    /// prefix and matches no current secret is refused, since an inverse that restored it would read it as a reference.
    /// Another entry's secret matches no usable current secret and follows the same rules as any other unmatched value.
    fn pre_image_of(
        &self,
        entry: &EntryId,
        changes: &BTreeMap<String, Option<String>>,
        current: &BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, Option<String>>, EffectRefusal> {
        let secrets = self.secrets_by_value(entry);
        let mut pre_image = BTreeMap::new();
        for (key, new) in changes {
            let value = match current.get(key) {
                None => None,
                Some(text) => match secret_reference(text, &secrets) {
                    Some(reference) => Some(reference),
                    None if new.as_deref().is_some_and(is_reference) => None,
                    None if is_reference(text) => {
                        return Err(EffectRefusal::Provider(
                            "the patch replaces a value that names a secret which no current secret matches; the journal could not restore it"
                                .to_string(),
                        ));
                    }
                    None => Some(text.clone()),
                },
            };
            pre_image.insert(key.clone(), value);
        }
        Ok(pre_image)
    }

    /// Resolves one config value: a secret reference becomes the file's content, and any other text passes through.
    fn resolve_text(&self, entry: &EntryId, text: &str) -> Result<String, EffectRefusal> {
        if text.starts_with(SECRET_PREFIX) {
            self.read_secret(entry, text)
        } else {
            Ok(text.to_string())
        }
    }

    /// Resolves the secret references in a change. A removed key stays removed.
    fn resolve_changes(
        &self,
        entry: &EntryId,
        changes: &BTreeMap<String, Option<String>>,
    ) -> Result<BTreeMap<String, Option<String>>, EffectRefusal> {
        let mut resolved = BTreeMap::new();
        for (key, value) in changes {
            let value = value
                .as_deref()
                .map(|text| self.resolve_text(entry, text))
                .transpose()?;
            resolved.insert(key.clone(), value);
        }
        Ok(resolved)
    }

    /// Whether the secrets directory is a real directory, not a symlink, owned by this process, and private: no group or other
    /// bits. While it is not, no reference resolves and no secret is known, so nothing is read from it.
    fn secrets_root_private(&self) -> bool {
        secrets_root_private(&self.secrets_dir, self.uid)
    }

    /// The content of the secret a reference names. The name is checked as a plain path. The secrets directory must be private,
    /// and so must each directory on the way: a real directory, owned by this process, with no group or other bits. The file must
    /// be a regular file, mode 0600, owned by this process, and small. A reference that fails any rule is refused with a fixed
    /// reason, and the value is never read into an error.
    fn read_secret(&self, entry: &EntryId, reference: &str) -> Result<String, EffectRefusal> {
        let name = reference
            .strip_prefix(SECRET_PREFIX)
            .filter(|name| valid_secret_name(name))
            .ok_or_else(|| {
                EffectRefusal::Provider("a secret reference names no valid secret".to_string())
            })?;
        if !may_use_secret(entry, name) {
            return Err(EffectRefusal::Provider(
                "a secret reference names another entry's secret".to_string(),
            ));
        }
        self.read_secret_file(reference)
    }

    /// Reads a usable secret file without an entry check, including during the directory walk.
    fn read_secret_file(&self, reference: &str) -> Result<String, EffectRefusal> {
        read_secret_file(&self.secrets_dir, self.uid, reference)
    }

    /// Shared and own usable secrets, as (reference, content). Unusable files are skipped.
    fn secrets_by_value(&self, entry: &EntryId) -> Vec<(String, String)> {
        let mut found = Vec::new();
        if self.secrets_root_private() {
            self.collect_secrets(&self.secrets_dir, "", 0, &mut found);
        }
        found.retain(|(reference, _)| may_use_secret(entry, &reference[SECRET_PREFIX.len()..]));
        found
    }

    /// Usable secrets owned by another entry, as (reference, content).
    fn foreign_secrets_by_value(&self, entry: &EntryId) -> Vec<(String, String)> {
        let mut found = Vec::new();
        if self.secrets_root_private() {
            self.collect_secrets(&self.secrets_dir, "", 0, &mut found);
        }
        found.retain(|(reference, _)| !may_use_secret(entry, &reference[SECRET_PREFIX.len()..]));
        found
    }

    fn collect_secrets(
        &self,
        dir: &Path,
        prefix: &str,
        depth: usize,
        found: &mut Vec<(String, String)>,
    ) {
        if depth > SECRET_MAX_DEPTH {
            return;
        }
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(file_name) = entry.file_name().into_string() else {
                continue;
            };
            let Ok(meta) = fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            let name = if prefix.is_empty() {
                file_name
            } else {
                format!("{prefix}/{file_name}")
            };
            if meta.is_dir() {
                self.collect_secrets(&entry.path(), &name, depth + 1, found);
            } else if meta.is_file() && valid_secret_name(&name) {
                let reference = format!("{SECRET_PREFIX}{name}");
                if let Some(content) = self
                    .read_secret_file(&reference)
                    .ok()
                    .filter(|content| !content.is_empty())
                {
                    found.push((reference, content));
                }
            }
        }
    }

    /// Whether the config holds every pre-image value, with a reference resolved to the file it names.
    fn shows_pre_image(
        &self,
        entry: &EntryId,
        config: &BTreeMap<String, String>,
        pre_image: &BTreeMap<String, Option<String>>,
    ) -> bool {
        pre_image.iter().all(|(key, value)| {
            let expected = match value {
                Some(text) => match self.resolve_text(entry, text) {
                    Ok(content) => Some(content),
                    Err(_) => return false,
                },
                None => None,
            };
            config.get(key).cloned() == expected
        })
    }

    /// The settled refusal for a create whose request failed. A refusal before the write is final. A failed create is
    /// recorded as failed only when Incus answered with a status error and a GET of the name then answers 404. One case
    /// more is settled, as [`EffectRefusal::Landed`]: an instance whose create asked for a start, that Incus answered a
    /// status error for, and that now reads as stopped and as the entry's own, with no operation still at work on it.
    /// Incus creates and then starts, and a failed start leaves the instance stopped, so the create landed and the start
    /// did not: the effect is journalled as applied and the plugin is told Incus's reason. Any other failure, including a
    /// transport, decode, timeout or wait failure, leaves the outcome unknown, since the answer says nothing about
    /// whether the create landed.
    fn settle_create(
        &self,
        entry: &EntryId,
        writer: &UnixIncusWriter,
        target: &ObjectRef,
        sent: &Value,
        error: IncusError,
    ) -> EffectRefusal {
        match error {
            IncusError::Refused(reason) => EffectRefusal::Provider(reason),
            IncusError::Status { message, .. } => match writer.get_etag(target) {
                Err(IncusError::Status { code: 404, .. }) => EffectRefusal::Provider(format!(
                    "the create did not land: {}",
                    self.incus_reason(entry, &message)
                )),
                Ok((found, _))
                    if target.kind == IncusKind::Instance
                        && sent.get("start") != Some(&Value::Bool(false))
                        && found["status"].as_str() == Some("Stopped") =>
                {
                    if self.settle_operations(target).is_err() {
                        return EffectRefusal::Unresolved;
                    }
                    match writer.read_owned(target) {
                        Ok(owned) if owned.uuid().is_some() && owned.tombstone().is_none() => {
                            let object = ObjectRef {
                                uuid: owned.uuid().map(str::to_string),
                                ..target.clone()
                            };
                            EffectRefusal::Landed(Box::new(Landed {
                                inverse: Inverse::IncusDelete {
                                    object: object.clone(),
                                },
                                object,
                                why: format!(
                                    "the instance was created, and Incus did not start it: {}",
                                    self.incus_reason(entry, &message)
                                ),
                            }))
                        }
                        _ => EffectRefusal::Unresolved,
                    }
                }
                _ => EffectRefusal::Unresolved,
            },
            IncusError::Io(_)
            | IncusError::Decode(_)
            | IncusError::Timeout
            | IncusError::TooLarge
            | IncusError::AppliedUnconfirmed(_) => EffectRefusal::Unresolved,
        }
    }

    /// Incus's own reason for refusing a request, as it may be told and journalled: every secret file's content, this
    /// entry's or another's, is replaced, since a create's body carries resolved secrets and Incus may quote it.
    fn incus_reason(&self, entry: &EntryId, message: &str) -> String {
        let mut values: Vec<String> = self
            .secrets_by_value(entry)
            .into_iter()
            .chain(self.foreign_secrets_by_value(entry))
            .map(|(_, value)| value)
            .filter(|value| !value.is_empty())
            .collect();
        values.sort_by_key(|value| std::cmp::Reverse(value.len()));
        let mut reason = message.trim().to_string();
        for value in values {
            reason = reason.replace(&value, "[secret]");
        }
        if reason.is_empty() {
            "Incus gave no reason".to_string()
        } else {
            format!("Incus said \"{reason}\"")
        }
    }

    /// The settled outcome of a patch whose request failed. A refusal before the write is final. A 412 is a stale plan, and changes
    /// nothing. An answer that could not be read, an unconfirmed write and a timeout are unresolved with no read-back: an operation
    /// still running can apply after a read that shows the pre-image. Any other error is settled by a read-back. A patch the
    /// read-back shows is landed; a patch the read-back shows without its change did not land; anything else is unresolved. The
    /// read-back's uuid is used only for the writer's own checks, so the answer names the object as the op did.
    fn settle_patch(
        &self,
        entry: &EntryId,
        writer: &UnixIncusWriter,
        object: &ObjectRef,
        changes: &BTreeMap<String, Option<String>>,
        pre_image: &BTreeMap<String, Option<String>>,
        error: IncusError,
    ) -> Result<(), EffectRefusal> {
        match error {
            IncusError::Refused(reason) => Err(EffectRefusal::Provider(reason)),
            IncusError::Status { code: 412, .. } => Err(EffectRefusal::Stale),
            IncusError::AppliedUnconfirmed(_)
            | IncusError::Timeout
            | IncusError::Io(_)
            | IncusError::Decode(_) => Err(EffectRefusal::Unresolved),
            _ => {
                let Ok(reading) = writer.read_class(object) else {
                    return Err(EffectRefusal::Unresolved);
                };
                if !kernel_may_write(&reading.class, entry) {
                    return Err(EffectRefusal::Unresolved);
                }
                if shows(&reading.config, changes) {
                    return Ok(());
                }
                if self.shows_pre_image(entry, &reading.config, pre_image) {
                    return Err(EffectRefusal::Provider(
                        "the patch did not land".to_string(),
                    ));
                }
                Err(EffectRefusal::Unresolved)
            }
        }
    }

    /// Waits until no operation Incus still runs that may write `place` names it, for at most the operation deadline from the
    /// call. An exec or a console session and a token operation are left out ([`writing_on`]): a session stays running for as
    /// long as it is open and writes nothing. The operations listing is read, and the first operation left that names the place
    /// is waited on, a few seconds per wait; then, after a short pause, the listing is read again, so an operation that started
    /// meanwhile is waited on too, and one the wait says has ended but the listing still shows is not spun on. Only GETs are
    /// sent. An operation still running at the deadline, a listing that cannot be read and a wait that cannot be answered are
    /// each [`EffectRefusal::Unresolved`], and the place is not read.
    fn settle_operations(&self, place: &ObjectRef) -> Result<(), EffectRefusal> {
        let deadline = self.deadline.unwrap_or(OPERATION_DEADLINE);
        let started = Instant::now();
        loop {
            let running = writing_on(
                &self.reader,
                place.kind,
                &place.project,
                place.parent.as_deref(),
                &place.name,
            )
            .map_err(|_| EffectRefusal::Unresolved)?;
            let Some(operation) = running.first() else {
                return Ok(());
            };
            let left = deadline.saturating_sub(started.elapsed());
            if left.is_zero() {
                return Err(EffectRefusal::Unresolved);
            }
            let hold = left.min(READ_BACK_HOLD);
            let waiter = UnixIncus::new(self.socket.clone()).with_timeout(hold + READ_BACK_MARGIN);
            match wait_operation(&waiter, &operation.id, hold) {
                // Ended or still running, the listing is read again only after the pause.
                Ok(_) => thread::sleep(READ_BACK_PAUSE.min(left)),
                Err(_) => return Err(EffectRefusal::Unresolved),
            }
        }
    }

    /// Retires an object the entry owns, through an approved plan. The object is read, then tombstoned under its ETag and read
    /// back; the delete reads it again and sends nothing unless it still shows the tombstone and the same uuid. A change the reads
    /// show is drift, and skips the delete. A stale ETag stops the sequence. A tombstone Incus refused with another 4xx is read
    /// back (see [`settle_tombstone`]).
    pub fn retire(
        &self,
        entry: &EntryId,
        object: &ObjectRef,
        plan: &Digest,
    ) -> Result<Retirement, EffectRefusal> {
        // An instance is named by its uuid. A retirement that names none is refused before any request, since the object it would
        // reach is not the one the approved plan names. Every other kind, a volume included, is named by its place.
        if object.kind.has_uuid() && object.uuid.is_none() {
            return Err(EffectRefusal::Provider(
                "a retirement names the object's uuid".to_string(),
            ));
        }
        let object = &self.served_at(entry, object)?;
        self.check_family(object.kind, &object.project)?;
        self.check_features(entry, object.kind, &object.project)?;
        let writer = self.writer(entry)?;
        let owned = match writer.read_owned(object) {
            Ok(owned) => owned,
            // Gone already, so there is nothing for the delete to remove. The caller settles the create the delete undid.
            Err(IncusError::Status { code: 404, .. }) => {
                return Ok(Retirement::Deleted(object.clone()));
            }
            Err(IncusError::Refused(reason)) => {
                return match writer.read_class(object) {
                    Ok(reading) if kernel_may_write(&reading.class, entry) => {
                        Ok(Retirement::Drift(reason))
                    }
                    Ok(reading) => Err(EffectRefusal::NotOwned(reading.class)),
                    Err(_) => Err(EffectRefusal::Provider(reason)),
                };
            }
            Err(error) => return Err(incus_refusal(error)),
        };
        let tombstoned = match writer.tombstone(&owned, plan) {
            Ok(tombstoned) => tombstoned,
            Err(IncusError::AppliedUnconfirmed(unconfirmed)) => {
                return Ok(Retirement::Drift(unconfirmed.reason));
            }
            Err(IncusError::Refused(reason)) => return Ok(Retirement::Drift(reason)),
            Err(IncusError::Status { code: 412, .. }) => return Err(EffectRefusal::Stale),
            Err(IncusError::Status { code, .. }) if (400..500).contains(&code) => {
                return settle_tombstone(&writer, object, plan, code);
            }
            Err(_) => return Err(EffectRefusal::Unresolved),
        };
        match writer.delete(&tombstoned) {
            Ok(()) => Ok(Retirement::Deleted(ObjectRef {
                uuid: tombstoned.uuid().map(str::to_string),
                ..object.clone()
            })),
            // Refused before any write, or answered by Incus with an error status (a running instance, a project that is not
            // empty, or a failed delete operation). Either way the object stays, so the delete is a settled failure, not an open one.
            Err(IncusError::Refused(reason)) => Ok(Retirement::Drift(reason)),
            Err(IncusError::Status { code, .. }) => Ok(Retirement::Drift(format!(
                "Incus refused the delete with status {code}"
            ))),
            // Anything else may follow a delete that landed, so the outcome is not known.
            Err(_) => Err(EffectRefusal::Unresolved),
        }
    }
}

impl Provider for IncusProvider {
    /// Answers host addresses, scoped instance reads, and this entry's own ACL reads.
    fn observe(
        &self,
        entry: &EntryId,
        projects: &[String],
        query: &Observe,
    ) -> Result<Value, EffectRefusal> {
        match query {
            Observe::HostAddresses => {
                let addresses = host_addresses(&self.reader)?;
                Ok(Value::Array(
                    addresses.iter().map(|ip| json!(ip.to_string())).collect(),
                ))
            }
            Observe::IncusGet { path } => {
                let (route, query) = path.split_once('?').ok_or(EffectRefusal::NotDeclared)?;
                let project = query
                    .strip_prefix("project=")
                    .filter(|p| plain_name(p))
                    .ok_or(EffectRefusal::NotDeclared)?;
                let parts: Vec<_> = route.split('/').collect();
                let kind = match parts.as_slice() {
                    ["", "1.0", "projects", name] if plain_name(name) && *name == project => {
                        IncusKind::Project
                    }
                    ["", "1.0", "networks", name] if plain_name(name) => IncusKind::Network,
                    ["", "1.0", "instances", name] if plain_name(name) => IncusKind::Instance,
                    ["", "1.0", "instances", name, "state"] if plain_name(name) => {
                        IncusKind::Instance
                    }
                    ["", "1.0", "network-acls", name] if plain_name(name) => IncusKind::Acl,
                    ["", "1.0", "profiles", name] if plain_name(name) => IncusKind::Profile,
                    ["", "1.0", "storage-pools", pool, "volumes", "custom", name]
                        if plain_name(pool) && plain_name(name) =>
                    {
                        IncusKind::Volume
                    }
                    _ => return Err(EffectRefusal::NotDeclared),
                };
                // The shared service profile derives its pool from this read-only default profile.
                if entry.0 == "net-arcak-svc" && path == "/1.0/profiles/default?project=default" {
                    let value = self.reader.get(path).map_err(incus_refusal)?;
                    return Ok(
                        json!({"devices":{"root":{"pool":value["devices"]["root"]["pool"]}}}),
                    );
                }
                self.check_family(kind, project)?;
                if kind != IncusKind::Acl
                    && !arca_kernel::ownership::manifest_names_project(projects, project)
                {
                    return Err(EffectRefusal::ProjectNotAllowed);
                }
                let usable = self.secrets_by_value(entry);
                let foreign = self.foreign_secrets_by_value(entry);
                let mut value = self.reader.get(path).map_err(incus_refusal)?;
                if matches!(
                    kind,
                    IncusKind::Acl
                        | IncusKind::Project
                        | IncusKind::Network
                        | IncusKind::Profile
                        | IncusKind::Volume
                ) {
                    let config = value
                        .get("config")
                        .cloned()
                        .ok_or(EffectRefusal::NotDeclared)?;
                    let config: BTreeMap<String, String> =
                        serde_json::from_value(config).map_err(|_| EffectRefusal::NotDeclared)?;
                    let object = ObjectRef {
                        kind,
                        project: project.into(),
                        parent: (kind == IncusKind::Volume).then(|| parts[3].into()),
                        name: parts.last().unwrap().to_string(),
                        uuid: None,
                    };
                    let class = classify_object(&object, &config, &self.retained.snapshot()?);
                    if !kernel_may_write(&class, entry) {
                        return Err(EffectRefusal::NotOwned(class));
                    }
                }
                // Redact equal secret contents throughout the answer, including expanded_config.
                redact_secret_values(&mut value, &usable, &foreign);
                Ok(value)
            }
        }
    }

    /// Plans an op with reads only. Each refusal here leaves no record, since the kernel journals only what a plan allowed.
    fn plan(&self, entry: &EntryId, op: &EffectOp) -> Result<Planned, EffectRefusal> {
        let digest = op_digest(op);
        match op {
            EffectOp::IncusCreate {
                kind,
                project,
                parent,
                name,
                body,
            } => {
                self.check_family(*kind, project)?;
                self.prepared_create(entry, *kind, project, parent.as_deref(), name, body)?;
                self.check_features(entry, *kind, project)?;
                Ok(Planned {
                    acl_patch: None,
                    class: class_of(*kind),
                    op_digest: digest,
                    pre_image: None,
                    etag: None,
                })
            }
            EffectOp::IncusPatchConfig { object, config } => {
                self.plan_for_object(entry, object, Some(config), digest)
            }
            EffectOp::IncusPatchAclField {
                object,
                field,
                rules,
            } => {
                check_acl_patch(object, rules)?;
                self.check_family(object.kind, &object.project)?;
                let writer = self.writer(entry)?;
                let owned = self.owned_for_write(entry, &writer, object)?;
                if owned.tombstone().is_some() {
                    return Err(EffectRefusal::NotDeclared);
                }
                let previous = owned
                    .acl_rules(*field)
                    .filter(|rules| rules.iter().all(Value::is_object))
                    .ok_or(EffectRefusal::NotDeclared)?
                    .clone();
                Ok(Planned {
                    class: EffectClass::A,
                    op_digest: digest,
                    pre_image: None,
                    acl_patch: Some(AclPatch {
                        field: *field,
                        previous,
                        rules: rules.clone(),
                    }),
                    etag: Some(owned.etag().into()),
                })
            }
            EffectOp::IncusDelete { object, .. } => {
                self.plan_for_object(entry, object, None, digest)
            }
        }
    }

    /// Applies a planned op. A refusal before a write is a plain failure. A write that may have landed is reported as
    /// [`EffectRefusal::Unresolved`] unless the object shows what happened.
    fn effect(
        &mut self,
        entry: &EntryId,
        op: &EffectOp,
        planned: &Planned,
    ) -> Result<(ObjectRef, Inverse), EffectRefusal> {
        if planned.op_digest != op_digest(op) {
            return Err(EffectRefusal::Provider(
                "the plan names another op".to_string(),
            ));
        }
        match op {
            EffectOp::IncusCreate {
                kind,
                project,
                parent,
                name,
                body,
            } => {
                // The plan's class and absence of a pre-image are checked again, since a plan may be made by hand.
                if planned.class != class_of(*kind) {
                    return Err(EffectRefusal::Provider(
                        "the plan names another class for this kind".to_string(),
                    ));
                }
                if planned.pre_image.is_some() || planned.acl_patch.is_some() {
                    return Err(EffectRefusal::Provider(
                        "a create's plan carries no pre-image".to_string(),
                    ));
                }
                self.check_family(*kind, project)?;
                let sent =
                    self.prepared_create(entry, *kind, project, parent.as_deref(), name, body)?;
                self.check_features(entry, *kind, project)?;
                let target = ObjectRef {
                    kind: *kind,
                    project: project.clone(),
                    parent: parent.clone(),
                    name: name.clone(),
                    uuid: None,
                };
                let writer = self.writer(entry)?;
                match writer.create(&target, &sent) {
                    Ok(owned) => {
                        let object = ObjectRef {
                            uuid: owned.uuid().map(str::to_string),
                            ..target
                        };
                        Ok((object.clone(), Inverse::IncusDelete { object }))
                    }
                    Err(error) => Err(self.settle_create(entry, &writer, &target, &sent, error)),
                }
            }
            EffectOp::IncusPatchConfig { object, config } => {
                if planned.acl_patch.is_some() {
                    return Err(EffectRefusal::NotDeclared);
                }
                if planned.class != class_of(object.kind) {
                    return Err(EffectRefusal::Provider(
                        "the plan names another class for this kind".to_string(),
                    ));
                }
                self.check_family(object.kind, &object.project)?;
                self.check_features(entry, object.kind, &object.project)?;
                let writer = self.writer(entry)?;
                let owned = self.owned_for_write(entry, &writer, object)?;
                if planned.etag.as_deref() != Some(owned.etag()) {
                    return Err(EffectRefusal::Stale);
                }
                self.check_change(entry, object, config, owned.config())?;
                let Some(pre_image) = planned.pre_image.clone() else {
                    return Err(EffectRefusal::Provider(
                        "the plan carries no pre-image".to_string(),
                    ));
                };
                // The pre-image is derived again from the object as it is now, and must be the plan's.
                if self.pre_image_of(entry, config, owned.config())? != pre_image {
                    return Err(EffectRefusal::Provider(
                        "the plan's pre-image is not the one the object holds".to_string(),
                    ));
                }
                let sent = self.resolve_changes(entry, config)?;
                match writer.patch(&owned, &sent) {
                    Ok(_) => {}
                    Err(error) => {
                        self.settle_patch(entry, &writer, object, &sent, &pre_image, error)?
                    }
                }
                // The answer and the inverse name the object exactly as the op named it. A by-name op gets a by-name inverse, so
                // the kernel's check of the inverse matches. The uuid the writer read back guarded the write and is not carried.
                Ok((
                    object.clone(),
                    Inverse::IncusRestoreConfig {
                        object: object.clone(),
                        config: pre_image,
                    },
                ))
            }
            EffectOp::IncusPatchAclField {
                object,
                field,
                rules,
            } => {
                check_acl_patch(object, rules)?;
                self.check_family(object.kind, &object.project)?;
                let patch = planned
                    .acl_patch
                    .as_ref()
                    .ok_or(EffectRefusal::NotDeclared)?;
                if planned.class != EffectClass::A
                    || planned.pre_image.is_some()
                    || patch.field != *field
                    || patch.rules != *rules
                {
                    return Err(EffectRefusal::NotDeclared);
                }
                let writer = self.writer(entry)?;
                let owned = self.owned_for_write(entry, &writer, object)?;
                if planned.etag.as_deref() != Some(owned.etag()) {
                    return Err(EffectRefusal::Stale);
                }
                if owned.acl_rules(*field) != Some(&patch.previous)
                    || !patch.previous.iter().all(Value::is_object)
                {
                    return Err(EffectRefusal::NotDeclared);
                }
                match writer.patch_acl_field(&owned, *field, rules) {
                    Ok(_) => {}
                    Err(IncusError::Status { code: 412, .. }) => return Err(EffectRefusal::Stale),
                    Err(IncusError::Refused(reason)) => {
                        return Err(EffectRefusal::Provider(reason));
                    }
                    Err(_) => {
                        // Even an empty answer or an unconfirmed write can be
                        // settled once no operation can still change this ACL.
                        match self
                            .read_back_acl(entry, object, patch)
                            .map_err(|_| EffectRefusal::Unresolved)?
                        {
                            ReadBack::Present {
                                owned: true,
                                holds_pre_image: false,
                                ..
                            } => {}
                            ReadBack::Present {
                                owned: true,
                                holds_pre_image: true,
                                ..
                            } => {
                                return Err(EffectRefusal::Provider(
                                    "the ACL array patch did not land".into(),
                                ));
                            }
                            _ => return Err(EffectRefusal::Unresolved),
                        }
                    }
                }
                Ok((
                    object.clone(),
                    Inverse::IncusRestoreAclField {
                        object: object.clone(),
                        field: *field,
                        rules: patch.previous.clone(),
                    },
                ))
            }
            // A delete runs only through `retire`, after an approved plan. The kernel's own path refuses it.
            EffectOp::IncusDelete { .. } => Err(EffectRefusal::NotDeclared),
        }
    }

    /// Reads the object an unresolved intent names, with one GET. A target outside the family is refused before any request, and so
    /// is a target whose project does not enable its kind's feature, as a write to it is: Incus would answer for the default project
    /// instead. The object is read only once no operation Incus still runs names it (see [`IncusProvider::settle_operations`]), so a
    /// create still running is not read as absent, nor one about to fail as present. An object that answers 404 is absent. Any other
    /// answer that is not a read of the object is an error, so the intent stays open.
    fn read_back(
        &self,
        entry: &EntryId,
        target: &ObjectRef,
        pre_image: Option<&BTreeMap<String, Option<String>>>,
    ) -> Result<ReadBack, EffectRefusal> {
        let place = self.served_at(entry, target)?;
        self.check_family(place.kind, &place.project)?;
        self.check_features(entry, place.kind, &place.project)?;
        self.settle_operations(&place)?;
        let reading = match self.writer(entry)?.read_class(&place) {
            Ok(reading) => reading,
            Err(IncusError::Status { code: 404, .. }) => return Ok(ReadBack::Absent),
            Err(error) => return Err(incus_refusal(error)),
        };
        let holds_pre_image =
            pre_image.is_some_and(|pre| self.shows_pre_image(entry, &reading.config, pre));
        Ok(ReadBack::Present {
            object: ObjectRef {
                uuid: reading.uuid.clone(),
                ..target.clone()
            },
            owned: kernel_may_write(&reading.class, entry),
            holds_pre_image,
        })
    }

    fn read_back_acl(
        &self,
        entry: &EntryId,
        target: &ObjectRef,
        patch: &AclPatch,
    ) -> Result<ReadBack, EffectRefusal> {
        check_acl_patch(target, &patch.rules)?;
        self.check_family(target.kind, &target.project)?;
        self.settle_operations(target)?;
        let writer = self.writer(entry)?;
        let owned = match self.owned_for_write(entry, &writer, target) {
            Ok(owned) => owned,
            Err(EffectRefusal::Provider(_)) => match writer.read_class(target) {
                Err(IncusError::Status { code: 404, .. }) => return Ok(ReadBack::Absent),
                _ => return Err(EffectRefusal::Unresolved),
            },
            Err(error) => return Err(error),
        };
        let current = owned
            .acl_rules(patch.field)
            .ok_or(EffectRefusal::Unresolved)?;
        let is_previous = arca_incus::acl_rules_match(current, &patch.previous);
        let is_wanted = arca_incus::acl_rules_match(current, &patch.rules);
        if !is_previous && !is_wanted {
            return Err(EffectRefusal::Unresolved);
        }
        Ok(ReadBack::Present {
            object: target.clone(),
            owned: true,
            // A no-op satisfies the wanted state even though it also holds the
            // pre-image. Live effects and restart settlement must agree on this.
            holds_pre_image: is_previous && !is_wanted,
        })
    }

    /// The reconciler's delete, through [`IncusProvider::retire`]: the tombstone, the read-back and the delete, in that order.
    fn retire(
        &mut self,
        entry: &EntryId,
        object: &ObjectRef,
        plan: &Digest,
    ) -> Result<Retirement, EffectRefusal> {
        IncusProvider::retire(self, entry, object, plan)
    }
}

/// The address of a bridge in the default project, by its name. The provider reads it to check the entry's mark.
fn network_home_target(name: &str) -> ObjectRef {
    ObjectRef {
        kind: IncusKind::Network,
        project: NETWORK_HOME.to_string(),
        parent: None,
        name: name.to_string(),
        uuid: None,
    }
}

/// The class an effect on a kind is journalled under. A project, an instance, a volume, a bucket and a network are destructive to
/// remove: a network is a bridge the default project shares, and an instance attached to it must go first, so its removal waits
/// for an approved plan, which deletes the most recent object first. Every other kind is revertible without loss.
fn class_of(kind: IncusKind) -> EffectClass {
    match kind {
        IncusKind::Project
        | IncusKind::Instance
        | IncusKind::Volume
        | IncusKind::Bucket
        | IncusKind::Network => EffectClass::B,
        IncusKind::Acl | IncusKind::Profile | IncusKind::Zone | IncusKind::Forward => {
            EffectClass::A
        }
    }
}

/// The Incus path of an object. A project is named by its own name; every other kind carries `?project=`. A volume, a bucket and a
/// forward need their parent to be addressed, so no path is given for them: the writer builds those.
pub fn kind_path(kind: IncusKind, project: &str, name: Option<&str>) -> Option<String> {
    let item = |collection: &str| match name {
        Some(name) => format!("{collection}/{name}?project={project}"),
        None => format!("{collection}?project={project}"),
    };
    match kind {
        IncusKind::Project => Some(match name {
            Some(name) => format!("/1.0/projects/{name}"),
            None => "/1.0/projects".to_string(),
        }),
        IncusKind::Instance => Some(item("/1.0/instances")),
        IncusKind::Network => Some(item("/1.0/networks")),
        IncusKind::Acl => Some(item("/1.0/network-acls")),
        IncusKind::Profile => Some(item("/1.0/profiles")),
        IncusKind::Zone => Some(item("/1.0/network-zones")),
        IncusKind::Volume | IncusKind::Bucket | IncusKind::Forward => None,
    }
}

/// Every address of the host's own interfaces, as Incus lists them: the listing of `/1.0/networks?recursion=1`, then the state of
/// each listed interface. Only those two GETs are sent, and an address of another family is skipped.
pub fn host_addresses(read: &dyn IncusRead) -> Result<Vec<IpAddr>, EffectRefusal> {
    let listing = read
        .get("/1.0/networks?recursion=1")
        .map_err(incus_refusal)?;
    let rows = listing
        .as_array()
        .ok_or_else(|| EffectRefusal::Provider("the Incus answer is not a list".to_string()))?;
    let mut found = Vec::new();
    for row in rows {
        let name = row
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| plain_name(name))
            .ok_or_else(|| {
                EffectRefusal::Provider("a host interface names no usable name".to_string())
            })?;
        let state = read
            .get(&format!("/1.0/networks/{name}/state"))
            .map_err(incus_refusal)?;
        let Some(addresses) = state.get("addresses").and_then(Value::as_array) else {
            continue;
        };
        for address in addresses {
            let family = address.get("family").and_then(Value::as_str).unwrap_or("");
            if family != "inet" && family != "inet6" {
                continue;
            }
            let ip = address
                .get("address")
                .and_then(Value::as_str)
                .and_then(|text| text.parse::<IpAddr>().ok())
                .ok_or_else(|| {
                    EffectRefusal::Provider("an address is not an IP address".to_string())
                })?;
            found.push(ip);
        }
    }
    Ok(found)
}

/// The settled outcome of a tombstone Incus refused with a 4xx other than 412. A 4xx is Incus's refusal of the request, so the
/// object is read back to see it. One that shows no tombstone of this plan, or is gone, was not tombstoned, so no delete followed
/// and the retirement is a settled [`Retirement::Drift`]: the delete's intent is failed, not left open, and a later removal tries
/// again. A read-back that shows the tombstone, or that cannot be read, cannot vouch for what happened, so it is
/// [`EffectRefusal::Unresolved`].
fn settle_tombstone(
    writer: &UnixIncusWriter,
    object: &ObjectRef,
    plan: &Digest,
    code: u16,
) -> Result<Retirement, EffectRefusal> {
    let tombstone = plan.to_hex();
    match writer.read_class(object) {
        Ok(reading)
            if reading.config.get(KERNEL_RETIRING_KEY).map(String::as_str)
                == Some(tombstone.as_str()) =>
        {
            Err(EffectRefusal::Unresolved)
        }
        Ok(_) | Err(IncusError::Status { code: 404, .. }) => Ok(Retirement::Drift(format!(
            "Incus refused the tombstone with status {code}, and the object shows no tombstone"
        ))),
        Err(_) => Err(EffectRefusal::Unresolved),
    }
}

/// Whether the config holds every changed key with its new value: a removed key must be absent.
fn shows(config: &BTreeMap<String, String>, changes: &BTreeMap<String, Option<String>>) -> bool {
    changes
        .iter()
        .all(|(key, value)| config.get(key).cloned() == *value)
}

/// The config after a change: a `Some` sets a key and a `None` removes it.
fn apply_changes(
    config: &BTreeMap<String, String>,
    changes: &BTreeMap<String, Option<String>>,
) -> BTreeMap<String, String> {
    let mut after = config.clone();
    for (key, value) in changes {
        match value {
            Some(text) => {
                after.insert(key.clone(), text.clone());
            }
            None => {
                after.remove(key);
            }
        }
    }
    after
}

/// The keys a change may name: plain, and never the kernel's tombstone or Incus's own `volatile` namespace. On a project, no
/// guarded key except the widening of the service project by an entry that may own it: its proxy allowance, and for the
/// network entry the host-bridge NIC allowance with the removal of the bridge grant. The plan refuses them before any
/// read, so a refused change sends no request.
fn check_patch_keys(
    entry: &EntryId,
    object: &ObjectRef,
    changes: &BTreeMap<String, Option<String>>,
) -> Result<(), EffectRefusal> {
    for (key, value) in changes {
        if object.kind == IncusKind::Project
            && guarded_project_key(key)
            && !service_proxy_allowance(entry, &object.project, key, value.as_deref())
            && !host_nic_allowance(entry, &object.project, key, value.as_deref(), false)
            && !bridge_grant_removal(entry, &object.project, key, value.as_deref())
        {
            return Err(EffectRefusal::Provider(
                "a plugin may not change a restricted or features key on a project".to_string(),
            ));
        }
        if key == KERNEL_RETIRING_KEY {
            return Err(EffectRefusal::Provider(
                "the tombstone is set only by a retirement".to_string(),
            ));
        }
        if !plain_config_key(key) {
            return Err(EffectRefusal::Provider(
                "a change names a config key that is not plain".to_string(),
            ));
        }
        if key == "volatile" || key.starts_with("volatile.") {
            return Err(EffectRefusal::Provider(
                "a change may not set a volatile config key".to_string(),
            ));
        }
    }
    Ok(())
}

/// A config key is `[a-z0-9_.-]`, plus uppercase ASCII after a leading `environment.`.
fn plain_config_key(key: &str) -> bool {
    let environment = key.starts_with("environment.");
    !key.is_empty()
        && key.bytes().all(|b| {
            b.is_ascii_lowercase()
                || b.is_ascii_digit()
                || matches!(b, b'_' | b'.' | b'-')
                || (environment && b.is_ascii_uppercase())
        })
}

/// The bridge rule for a create: a bridge network, and its config passes [`check_bridge_config`].
fn check_bridge_create(
    fields: &Map<String, Value>,
    reserved: &[String],
) -> Result<(), EffectRefusal> {
    if fields.get("type").and_then(Value::as_str) != Some("bridge") {
        return Err(EffectRefusal::Provider(
            "the kernel creates only bridge networks".to_string(),
        ));
    }
    let config: BTreeMap<String, String> = fields
        .get("config")
        .and_then(Value::as_object)
        .map(|config| {
            config
                .iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|text| (key.clone(), text.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();
    check_bridge_config(&config, reserved)
}

/// The bridge rule. Every key must be on the allowlist, which is the keys a plan's bridge sets. `ipv6.address` is `none`, the IPv4
/// range is a CIDR, and it does not overlap a reserved range. A route, an external interface and a `raw.*` key are refused by
/// the allowlist, each under its own reason.
fn check_bridge_config(
    config: &BTreeMap<String, String>,
    reserved: &[String],
) -> Result<(), EffectRefusal> {
    for key in config.keys() {
        // The kernel's own mark is on every object it made. Ownership already checked it, so it is not a setting to refuse.
        if key != KERNEL_ENTRY_KEY && !BRIDGE_KEYS.contains(&key.as_str()) {
            return Err(EffectRefusal::Provider(bridge_key_refusal(key).to_string()));
        }
    }
    if config.get("ipv6.address").map(String::as_str) != Some("none") {
        return Err(EffectRefusal::Provider(
            "a bridge sets ipv6.address to none".to_string(),
        ));
    }
    let ipv4 = config
        .get("ipv4.address")
        .and_then(|text| parse_cidr(text))
        .ok_or_else(|| EffectRefusal::Provider("a bridge names an IPv4 range".to_string()))?;
    for range in reserved {
        let range = parse_cidr(range).ok_or_else(|| {
            EffectRefusal::Provider("a reserved range is not an IPv4 CIDR".to_string())
        })?;
        if overlaps(ipv4, range) {
            return Err(EffectRefusal::Provider(
                "the bridge's IPv4 range overlaps a reserved range".to_string(),
            ));
        }
    }
    Ok(())
}

/// An IPv4 CIDR, as its address and prefix length.
fn parse_cidr(text: &str) -> Option<(Ipv4Addr, u8)> {
    let (address, length) = text.split_once('/')?;
    let address: Ipv4Addr = address.parse().ok()?;
    let length: u8 = length.parse().ok()?;
    (length <= 32).then_some((address, length))
}

/// Whether two IPv4 ranges share an address.
fn overlaps(a: (Ipv4Addr, u8), b: (Ipv4Addr, u8)) -> bool {
    let length = a.1.min(b.1);
    let mask = if length == 0 {
        0
    } else {
        u32::MAX << (32 - length)
    };
    (u32::from(a.0) & mask) == (u32::from(b.0) & mask)
}

/// Whether `secrets_dir` is a real directory, not a symlink, owned by `uid`, and private: no group or other bits.
fn secrets_root_private(secrets_dir: &Path, uid: u32) -> bool {
    fs::symlink_metadata(secrets_dir).is_ok_and(|meta| {
        meta.is_dir()
            && !meta.file_type().is_symlink()
            && meta.uid() == uid
            && meta.mode() & 0o077 == 0
    })
}

/// The content of the secret `reference` names, read for `entry` by the provider's own rules: the name is a valid secret
/// name, the secret is the entry's own or a shared one, and the file passes every check [`read_secret_file`] makes for the
/// user this process runs as. The error is a fixed reason and never holds the value. A caller outside the effect journal
/// (a virtual machine's secrets, written into its guest) reads a secret through this and through nothing looser.
pub fn read_entry_secret(
    secrets_dir: &Path,
    entry: &EntryId,
    reference: &str,
) -> Result<String, &'static str> {
    let name = reference
        .strip_prefix(SECRET_PREFIX)
        .filter(|name| valid_secret_name(name))
        .ok_or("a secret reference names no valid secret")?;
    if !may_use_secret(entry, name) {
        return Err("a secret reference names another entry's secret");
    }
    let uid = process_uid().map_err(|_| "this process's user could not be read")?;
    read_secret_file(secrets_dir, uid, reference).map_err(|_| "a secret is not available")
}

/// Reads a usable secret file under `secrets_dir` without an entry check. The secrets directory must be private, and so
/// must each directory on the way: a real directory, owned by `uid`, with no group or other bits. The file must be a
/// regular file, mode 0600, owned by `uid`, and small, and the file opened must be the one that was checked.
fn read_secret_file(
    secrets_dir: &Path,
    uid: u32,
    reference: &str,
) -> Result<String, EffectRefusal> {
    let name = reference
        .strip_prefix(SECRET_PREFIX)
        .filter(|name| valid_secret_name(name))
        .ok_or_else(|| {
            EffectRefusal::Provider("a secret reference names no valid secret".to_string())
        })?;
    let unavailable = || EffectRefusal::Provider("a secret is not available".to_string());
    if !secrets_root_private(secrets_dir, uid) {
        return Err(unavailable());
    }
    let parts: Vec<&str> = name.split('/').collect();
    if parts.iter().any(|part| part.is_empty()) {
        return Err(unavailable());
    }
    let mut path = secrets_dir.to_path_buf();
    for (index, part) in parts.iter().enumerate() {
        path.push(part);
        let meta = fs::symlink_metadata(&path).map_err(|_| unavailable())?;
        if meta.file_type().is_symlink() {
            return Err(unavailable());
        }
        if index + 1 < parts.len() {
            let private = meta.is_dir() && meta.uid() == uid && meta.mode() & 0o077 == 0;
            if !private {
                return Err(unavailable());
            }
            continue;
        }
        if !meta.is_file()
            || (meta.mode() & 0o777) != 0o600
            || meta.uid() != uid
            || meta.len() > SECRET_MAX_BYTES
        {
            return Err(unavailable());
        }
        let opened = File::open(&path).map_err(|_| unavailable())?;
        let opened_meta = opened.metadata().map_err(|_| unavailable())?;
        if opened_meta.dev() != meta.dev() || opened_meta.ino() != meta.ino() {
            return Err(unavailable());
        }
        let mut text = String::new();
        opened
            .take(SECRET_MAX_BYTES + 1)
            .read_to_string(&mut text)
            .map_err(|_| unavailable())?;
        let len = text.len() as u64;
        if len > SECRET_MAX_BYTES {
            return Err(unavailable());
        }
        return Ok(text);
    }
    Err(unavailable())
}

/// The entry a secret name belongs to: its first segment, when that parses as an entry id and another segment follows.
fn secret_owner(name: &str) -> Option<&str> {
    let (owner, _) = name.split_once('/')?;
    EntryId::parse(owner).map(|_| owner)
}

fn may_use_secret(entry: &EntryId, name: &str) -> bool {
    secret_owner(name).is_none_or(|owner| owner == entry.0)
}

/// A secret name: `^[a-z0-9][a-z0-9/-]{0,127}$`. It is checked by hand, so no pattern library is needed.
fn valid_secret_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'/' | b'-'))
}

/// A name that is a plain path segment: letters, digits and `-_.:`, not `.` or `..`, and at most 253 bytes.
fn plain_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 253
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}

/// The reference of the current secret whose content is `value`, when one is. A match is equal content: equal bytes have equal
/// sha256 digests, and the provider already holds the content it compares against.
fn secret_reference(value: &str, secrets: &[(String, String)]) -> Option<String> {
    secrets
        .iter()
        .find(|(_, content)| content == value)
        .map(|(reference, _)| reference.clone())
}

/// Whether a config value names a secret file rather than being the value itself.
fn is_reference(text: &str) -> bool {
    text.starts_with(SECRET_PREFIX)
}

/// Why a bridge config key is refused. The text names the rule and never a value.
fn bridge_key_refusal(key: &str) -> &'static str {
    if key.starts_with("raw.") {
        "a bridge may not set a raw key"
    } else if key == "bridge.external_interfaces" {
        "a bridge may not take an external interface"
    } else if key.ends_with(".routes") {
        "a bridge may not set a route"
    } else {
        "a bridge may set only its address and NAT keys"
    }
}

/// An object's string-valued config, or nothing when it has none.
fn text_config(config: Option<&Value>) -> BTreeMap<String, String> {
    config
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|text| (key.clone(), text.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A read failure as a fixed reason. Incus's own message is left out, since it can echo a response body.
fn incus_refusal(error: IncusError) -> EffectRefusal {
    match error {
        IncusError::Refused(reason) => EffectRefusal::Provider(reason),
        IncusError::Io(_) => EffectRefusal::Provider("the Incus socket failed".to_string()),
        IncusError::Status { code, .. } => {
            EffectRefusal::Provider(format!("Incus answered {code}"))
        }
        IncusError::Decode(_) => {
            EffectRefusal::Provider("the Incus answer is not in the expected shape".to_string())
        }
        IncusError::Timeout => EffectRefusal::Provider("Incus did not answer in time".to_string()),
        IncusError::TooLarge => {
            EffectRefusal::Provider("the Incus answer is too large".to_string())
        }
        IncusError::AppliedUnconfirmed(_) => EffectRefusal::Unresolved,
    }
}

/// The sequence every uid probe's directory name carries. Two probes that read the same clock value still name different directories.
static UID_PROBE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// The uid this process runs as, read from the owner of a file it creates now in a fresh directory. No libc call is needed.
fn process_uid() -> Result<u32, EffectRefusal> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or(0);
    let sequence = UID_PROBE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "arca-uid-{}-{nanos}-{sequence}",
        std::process::id()
    ));
    fs::create_dir(&dir)
        .map_err(|_| EffectRefusal::Provider("the uid probe failed".to_string()))?;
    let made = File::create_new(dir.join("uid")).and_then(|file| file.metadata());
    let _ = fs::remove_dir_all(&dir);
    made.map(|meta| meta.uid())
        .map_err(|_| EffectRefusal::Provider("the uid probe failed".to_string()))
}

/// Validate the closed top-level patch vocabulary before any read.
fn check_acl_patch(object: &ObjectRef, rules: &[Value]) -> Result<(), EffectRefusal> {
    if object.kind != IncusKind::Acl
        || object.project != NETWORK_HOME
        || object.parent.is_some()
        || object.uuid.is_some()
        || !rules.iter().all(Value::is_object)
    {
        return Err(EffectRefusal::NotDeclared);
    }
    Ok(())
}

fn redact_secret_values(
    value: &mut Value,
    usable: &[(String, String)],
    foreign: &[(String, String)],
) {
    match value {
        Value::String(text) => {
            if let Some(reference) = secret_reference(text, usable) {
                *text = reference;
            } else if secret_reference(text, foreign).is_some() {
                *text = FOREIGN_SECRET_MARKER.to_string();
            }
        }
        Value::Object(map) => {
            for value in map.values_mut() {
                redact_secret_values(value, usable, foreign);
            }
        }
        Value::Array(items) => {
            for value in items {
                redact_secret_values(value, usable, foreign);
            }
        }
        _ => {}
    }
}
