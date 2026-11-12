//! The journalled Incus writer, compiled with the `write` feature. Adoption and the closed service exec/file module use
//! checked transport helpers for their separate capabilities; neither adds an exec operation to Provider.
//!
//! Every write is gated twice. The project family is checked first: a target outside the projects the kernel's configuration
//! names is refused before any request. The ownership classifier is checked next: a read returns an [`Owned`] only when
//! `kernel_may_write` allows the writer's entry to change the object, so a PATCH, a tombstone or a DELETE can name only an
//! object the entry owns. A config change and a tombstone send the ETag of the read they were checked against as `If-Match`. A
//! DELETE reads the object again first, because Incus checks no ETag on a DELETE. A create is refused unless its body classes as
//! the entry's own, its source is an image (a local one, or one on the one pinned image server), and its name is free. A retiring
//! project's DELETE is preceded by the DELETE of each image in its own store, which is the project's by containment.
//!
//! Writes are POST, PATCH, PUT and DELETE. A PATCH carries the config keys it changes. For an ACL it also sends the description
//! and both arrays from the read, replacing only the requested field: Incus merges only config on PATCH. A project's config
//! change and its tombstone are a PUT of the project's whole config, with the change applied, and of
//! the description, both as the read showed them: Incus 7.3's project PATCH replaces the whole config with the keys it names
//! (`projectPatch` in `cmd/incusd/api_project.go` copies the old keys into the raw request map, not into the request it applies),
//! so a PATCH of one key would drop the kernel's mark, `restricted` and every feature. A PUT also force-stops a running instance
//! before its DELETE. Nothing sends a PATCH to a project.
//!
//! A write whose answer is sync, or names no operation, is done. One that names an operation is waited on until it succeeds,
//! fails, or passes the writer's deadline. A write that Incus accepted and whose read-back does not confirm it is
//! [`IncusError::AppliedUnconfirmed`]. [`IncusError::Refused`] means that no write was sent.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use arca_kernel::canon::Digest;
use arca_kernel::desired::{EntryId, PluginId};
use arca_kernel::journal::AclField;
use arca_kernel::ownership::{
    IncusKind, KERNEL_RETIRING_KEY, ObjectRef, OwnerClass, classify_object, kernel_may_write,
};
use serde_json::{Map, Value, json};

use crate::read::{
    Framed, IncusError, TIMEOUT, Target, Unconfirmed, cap_message, check_path, checked_envelope,
    exchange,
};

/// The largest write body the writer sends, in bytes.
const MAX_WRITE_BODY: usize = 1024 * 1024;
/// The overall deadline of one operation, from its acceptance to its end, when the writer is not given another.
pub const OPERATION_DEADLINE: Duration = Duration::from_secs(600);
/// The `timeout` a wait request carries, in seconds. The server holds the wait that long at most.
const WAIT_SECONDS: u64 = 5;
/// The deadline of one wait request. It is longer than the server's hold, so the hold never reads as a failure.
const WAIT_TIMEOUT: Duration = Duration::from_secs(30);
/// The pause before a wait that answered "running" is asked again, so a server that answers at once is not spun on.
const POLL_PAUSE: Duration = Duration::from_millis(100);
/// Projects the writer never changes, whatever the family names.
const RESERVED_PROJECTS: [&str; 2] = ["bench", "default"];
/// The project a network, a forward and an ACL are addressed in. Incus holds the kernel's bridges here, a forward hangs off its
/// network, and Incus 7.3 keeps the ACLs of a project that does not enable networks here too. The writer sends no other kind here.
pub const NETWORK_HOME: &str = "default";

/// The one image server a create's source may name: the public server Incus ships as its `images:` remote. Incus pulls an
/// instance's image from it into the image store the instance's project reads, which for a kernel project is its own, and the
/// project's retirement deletes it there.
pub const IMAGE_SERVER: &str = "https://images.linuxcontainers.org";
/// The protocol a source that names [`IMAGE_SERVER`] must name with it.
pub const IMAGE_PROTOCOL: &str = "simplestreams";
/// The project config key that gives a project an image store of its own.
const IMAGES_KEY: &str = "features.images";

/// Whether a kind is addressed in [`NETWORK_HOME`]: a network; a forward, which is made under its network; and an ACL, which
/// Incus keeps in the default project for a kernel project, since a kernel project does not enable networks.
pub fn network_home(kind: IncusKind) -> bool {
    matches!(
        kind,
        IncusKind::Network | IncusKind::Forward | IncusKind::Acl
    )
}

/// An object the writer read and classed as its entry's own. Its fields are private, and only [`UnixIncusWriter`] makes one,
/// so holding one is proof that the classifier allowed the entry to change the object at that read. Its `etag` is the one
/// the read returned, and a PATCH sends it as `If-Match`.
#[derive(Clone, PartialEq, Eq)]
pub struct Owned {
    ingress: Option<Vec<Value>>,
    egress: Option<Vec<Value>>,
    entry: EntryId,
    target: Target,
    etag: String,
    uuid: Option<String>,
    config: BTreeMap<String, String>,
    tombstone: Option<String>,
    /// The object's description, as read. A project's PUT sends it back unchanged, since a PUT replaces it too.
    description: String,
}

// Config and rule values can contain secrets and must never appear in diagnostics.
impl std::fmt::Debug for Owned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Owned")
            .field("entry", &self.entry)
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

impl Owned {
    /// The rule array from the same ETag-bearing read as the ownership config.
    pub fn acl_rules(&self, field: AclField) -> Option<&Vec<Value>> {
        match field {
            AclField::Ingress => self.ingress.as_ref(),
            AclField::Egress => self.egress.as_ref(),
        }
    }

    /// The entry that owns the object.
    pub fn entry(&self) -> &EntryId {
        &self.entry
    }

    pub fn target(&self) -> &Target {
        &self.target
    }

    /// The ETag the read returned.
    pub fn etag(&self) -> &str {
        &self.etag
    }

    /// The object's uuid, for an instance: its `volatile.uuid` config key. Other kinds carry none, a volume included, and they are
    /// identified by name and ETag.
    pub fn uuid(&self) -> Option<&str> {
        self.uuid.as_deref()
    }

    /// The object's own config, as read.
    pub fn config(&self) -> &BTreeMap<String, String> {
        &self.config
    }

    /// The digest of the delete plan whose tombstone the object carries, when it carries one.
    pub fn tombstone(&self) -> Option<&str> {
        self.tombstone.as_deref()
    }
}

/// What a read of an object shows of its owner. See [`UnixIncusWriter::read_class`].
#[derive(Clone, PartialEq, Eq)]
pub struct Reading {
    /// The class the object's own config gives.
    pub class: OwnerClass,
    /// The ETag the read returned.
    pub etag: String,
    /// The object's uuid, for a kind that has one.
    pub uuid: Option<String>,
    /// The object's own config, as read. It may hold values, so it is never printed or journalled.
    pub config: BTreeMap<String, String>,
}

impl std::fmt::Debug for Reading {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reading")
            .field("class", &self.class)
            .field("uuid", &self.uuid)
            .finish_non_exhaustive()
    }
}

/// The projects the kernel may write in: the names its configuration lists and, on a node with tenants, the project of
/// each tenant whose entry exists ([`ProjectFamily::with_tenants`]). Nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectFamily {
    projects: BTreeSet<String>,
}

impl ProjectFamily {
    /// The family the kernel's list names. A name that is not a plain segment, a reserved project, and a project the other
    /// system uses are each refused, so a configuration that names one does not start.
    pub fn new<'a>(
        kernel: impl IntoIterator<Item = &'a str>,
        droidd: impl IntoIterator<Item = &'a str>,
    ) -> Result<Self, String> {
        let droidd: BTreeSet<&str> = droidd.into_iter().collect();
        let mut projects = BTreeSet::new();
        for name in kernel {
            if segment(name).is_err() {
                return Err("a kernel project is not a plain name".to_string());
            }
            if RESERVED_PROJECTS.contains(&name) || droidd.contains(name) {
                return Err("a kernel project is reserved for another owner".to_string());
            }
            projects.insert(name.to_string());
        }
        Ok(Self { projects })
    }

    /// The family with the projects of the tenants whose entries exist. A name that is not a tenant project's (a tenant
    /// prefix and a tenant id) is refused, so nothing but a tenant's project joins a family this way; a reserved project
    /// never does.
    pub fn with_tenants<'a>(
        mut self,
        tenants: impl IntoIterator<Item = &'a str>,
    ) -> Result<Self, String> {
        for name in tenants {
            if arca_kernel::ownership::tenant_of_project(name).is_none() || segment(name).is_err() {
                return Err("a tenant project does not carry the tenant prefix".to_string());
            }
            self.projects.insert(name.to_string());
        }
        Ok(self)
    }

    /// Whether a write of `kind` may name the project. A network, a forward and an ACL are addressed in [`NETWORK_HOME`] and
    /// nowhere else. Every other kind is addressed in a project the configuration lists. Whether the object is the entry's own is
    /// checked on each read and write, not here.
    pub fn allows(&self, kind: IncusKind, project: &str) -> bool {
        if network_home(kind) {
            return project == NETWORK_HOME;
        }
        !RESERVED_PROJECTS.contains(&project) && self.projects.contains(project)
    }
}

/// The writer for one kernel entry. It names the entry it writes for, the retained entries whose objects it may not write,
/// and the project family it may write in. Every mutating method refuses an object the classifier does not class as this
/// entry's own, before it sends.
#[derive(Debug, Clone)]
pub struct UnixIncusWriter {
    pub(crate) socket: PathBuf,
    entry: EntryId,
    retained: BTreeMap<EntryId, (PluginId, Digest)>,
    family: ProjectFamily,
    deadline: Duration,
    budget: Option<Instant>,
}

impl UnixIncusWriter {
    /// A writer for `entry` over the Incus socket at `socket`. `retained` holds the entries whose plans were declined, so
    /// their objects classify as retained and are never written. `family` is the set of projects it may write in.
    pub fn new(
        socket: PathBuf,
        entry: EntryId,
        retained: BTreeMap<EntryId, (PluginId, Digest)>,
        family: ProjectFamily,
    ) -> Self {
        Self {
            socket,
            entry,
            retained,
            family,
            deadline: OPERATION_DEADLINE,
            budget: None,
        }
    }

    /// The same writer with another overall deadline for an operation, from its acceptance to its end.
    pub fn with_operation_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }

    pub fn with_total_budget(mut self, budget: Duration) -> Self {
        self.budget = Instant::now().checked_add(budget);
        self
    }

    pub(crate) fn request_timeout(&self, timeout: Duration) -> Duration {
        self.budget.map_or(timeout, |until| {
            timeout.min(until.saturating_duration_since(Instant::now()))
        })
    }

    /// The entry this writer writes for.
    pub fn entry(&self) -> &EntryId {
        &self.entry
    }

    /// A GET of an object's metadata and its ETag. Every owner may be read, so no classification applies here.
    pub fn get_etag(&self, target: &Target) -> Result<(Value, String), IncusError> {
        self.fetch(target)
    }

    /// Reads an object's owner without refusing it: the class, the ETag, the uuid and the own config. Only a GET is sent. The
    /// family is still checked first, so a target outside it sends nothing. A caller that refuses a write uses this to name the
    /// class it refused; a write itself still needs [`Self::read_owned`].
    pub fn read_class(&self, target: &Target) -> Result<Reading, IncusError> {
        self.allow_target(target)?;
        let (metadata, etag) = self.fetch(target)?;
        let config = own_config(&metadata)?;
        Ok(Reading {
            class: classify_object(target, &config, &self.retained),
            etag,
            uuid: uuid_of(target.kind, &config),
            config,
        })
    }

    /// Reads an object and returns it as [`Owned`] when this entry may change it. A target outside the family, an object of
    /// another class, and a target whose uuid is not the object's are refused, and nothing is written.
    pub fn read_owned(&self, target: &Target) -> Result<Owned, IncusError> {
        self.allow_target(target)?;
        let (metadata, etag) = self.fetch(target)?;
        let config = own_config(&metadata)?;
        let description = own_description(&metadata)?;
        self.classed(target, etag, config, description, metadata)
            .map_err(|unread| refused(&unread.reason))
    }

    /// Creates an object. The target's project must be in the family, and the target names no uuid yet. The body's source,
    /// when it has one, must be an image. Every key in the body, at any depth, must be plain ASCII, and the body's config may
    /// not set a `volatile.*` key. The body's config must class as this entry's own, so the object belongs to the entry from
    /// the moment it exists. The name must be free: its GET must answer 404. The POST is then sent, and the object is read
    /// back, which must class as this entry's own. An instance starts by default unless its body explicitly sets `start: false`.
    pub fn create(&self, target: &Target, body: &Value) -> Result<Owned, IncusError> {
        self.create_with_registry(target, body, None)
    }

    /// [`Self::create`], for an instance whose image is on a registry that asks for a credential. `body` is the body
    /// a create always takes, with no credential in it, and every check is made on it as it is: a server address
    /// that carries one is still refused. Only the body that is sent names the registry as
    /// its address with the user name and the password before the host, which is how Incus takes a registry's credential. Nothing else of the
    /// body changes, and a body whose source is not an OCI pull is sent as it is.
    ///
    /// Incus keeps that address in the record of the image it fetches. It is told not to use it again by itself: once
    /// the instance exists, `auto_update` is turned off on the image it was made from.
    pub fn create_with_registry(
        &self,
        target: &Target,
        body: &Value,
        registry: Option<&RegistryCredential>,
    ) -> Result<Owned, IncusError> {
        self.allow_target(target)?;
        if target.uuid.is_some() {
            return Err(refused("a create names no uuid yet"));
        }
        let urls = urls(target)?;
        let Value::Object(fields) = body else {
            return Err(refused("a create body is not an object"));
        };
        if !plain_keys(body) {
            return Err(refused("a create body names a key that is not plain ASCII"));
        }
        check_source(fields)?;
        let config = match fields.get("config") {
            Some(Value::Object(config)) => {
                if config.keys().any(|key| volatile(key)) {
                    return Err(refused("a create may not set a volatile config key"));
                }
                text_map(config)
                    .ok_or_else(|| refused("a config value in a create body is not text"))?
            }
            _ => BTreeMap::new(),
        };
        if config.contains_key(KERNEL_RETIRING_KEY) {
            return Err(refused("a create may not carry a retiring tombstone"));
        }
        let class = classify_object(target, &config, &self.retained);
        if !kernel_may_write(&class, &self.entry) {
            return Err(refused(
                "a created object must carry this entry's mark and no other owner's key",
            ));
        }
        let mut sent = fields.clone();
        sent.insert("name".to_string(), json!(target.name));
        if target.kind == IncusKind::Instance {
            sent.entry("start").or_insert(json!(true));
        }
        if target.kind == IncusKind::Forward {
            sent.insert("listen_address".to_string(), json!(target.name));
        }
        if let (IncusKind::Forward, Some(network)) = (target.kind, target.parent.as_deref()) {
            self.network_is_own(network)?;
        }
        self.require_free(&urls.object)?;
        let with_credential = registry.and_then(|registry| {
            let source = sent.get("source")?;
            if target.kind != IncusKind::Instance || source["protocol"] != "oci" {
                return None;
            }
            let address = registry.address(source["server"].as_str()?)?;
            let mut source = source.clone();
            source["server"] = json!(address);
            Some(source)
        });
        let fetched = with_credential.is_some();
        if let Some(source) = with_credential {
            sent.insert("source".to_string(), source);
        }
        self.mutate("POST", &urls.collection, None, Some(&Value::Object(sent)))?;
        let owned = self.confirm(target, |_| Ok(()))?;
        if fetched {
            // The image's record now holds the address it was fetched from. An image by digest never changes, so
            // Incus has no reason to go back to the registry for it with that address.
            if let Some(image) = owned
                .config()
                .get("volatile.base_image")
                .filter(|text| text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit()))
            {
                let url = format!("/1.0/images/{image}?project={}", target.project);
                if self
                    .mutate("PATCH", &url, None, Some(&json!({ "auto_update": false })))
                    .is_err()
                {
                    eprintln!(
                        "arca: auto_update could not be turned off on the image of {}: Incus may ask its registry again by itself",
                        target.name
                    );
                }
            }
        }
        Ok(owned)
    }

    /// Sets and removes config keys of an object this entry owns, under the ETag of `owned`. The change must leave the object
    /// classed as this entry's own, and it may not name the tombstone key, a key that is not plain ASCII, or a `volatile.*` key.
    /// It is sent as [`Self::send_config`] sends it: a PATCH of the changed keys, where a removed key is the empty string, or for a
    /// project a PUT of the whole config. The object is read back, and the read must show the change, and for a project every key
    /// the PUT sent.
    pub fn patch(
        &self,
        owned: &Owned,
        changes: &BTreeMap<String, Option<String>>,
    ) -> Result<Owned, IncusError> {
        self.check(owned)?;
        if owned.tombstone.is_some() {
            return Err(refused("a retiring object takes no other change"));
        }
        if changes.contains_key(KERNEL_RETIRING_KEY) {
            return Err(refused("the tombstone is set only by tombstone"));
        }
        if changes.keys().any(|key| !plain_key(key)) {
            return Err(refused(
                "a patch names a config key that is not plain ASCII",
            ));
        }
        if changes.keys().any(|key| volatile(key)) {
            return Err(refused("a patch may not set a volatile config key"));
        }
        let after = apply(&owned.config, changes);
        if !kernel_may_write(
            &classify_object(&owned.target, &after, &self.retained),
            &self.entry,
        ) {
            return Err(refused(
                "the change would leave the object owned by another owner",
            ));
        }
        let whole = self.send_config(owned, changes, &after)?;
        let expected = changes.clone();
        self.confirm(&owned.target, move |fresh| {
            let shows = expected.iter().all(|(key, value)| {
                fresh.config().get(key).map(String::as_str) == value.as_deref()
            });
            if !shows {
                Err("the read-back does not show the change".to_string())
            } else if !holds_sent(fresh, whole.as_ref()) {
                Err("the read-back lost a key the whole config sent".to_string())
            } else {
                Ok(())
            }
        })
    }

    /// Replaces one ACL ingress or egress array under the ownership read's ETag.
    pub fn patch_acl_field(
        &self,
        owned: &Owned,
        field: AclField,
        rules: &[Value],
    ) -> Result<Owned, IncusError> {
        self.check(owned)?;
        if owned.target.kind != IncusKind::Acl
            || owned.tombstone.is_some()
            || !rules.iter().all(Value::is_object)
            || owned.acl_rules(field).is_none()
        {
            return Err(refused(
                "an ACL patch requires a non-retiring ACL and an array of rule objects",
            ));
        }
        let mut body = acl_patch(owned, json!({}))?;
        body[field.as_str()] = json!(rules);
        self.mutate(
            "PATCH",
            &urls(&owned.target)?.object,
            Some(&owned.etag),
            Some(&body),
        )?;
        self.confirm(&owned.target, |fresh| {
            if fresh
                .acl_rules(field)
                .is_some_and(|fresh| acl_rules_match(fresh, rules))
                && fresh.acl_rules(other_acl_field(field))
                    == owned.acl_rules(other_acl_field(field))
                && fresh.description == owned.description
                && fresh.config == owned.config
            {
                Ok(())
            } else {
                Err("the read-back does not show the ACL array".into())
            }
        })
    }

    /// Marks an object as retiring with the digest of its delete plan, under the ETag of `owned`, then reads it back. It is sent
    /// as [`Self::send_config`] sends a change, so a project's tombstone is a PUT of its whole config with the tombstone added. The
    /// read must show the same uuid, this entry's mark and the tombstone, and for a project every key the PUT sent. The result is
    /// the only input [`Self::delete`] takes.
    pub fn tombstone(&self, owned: &Owned, plan: &Digest) -> Result<Owned, IncusError> {
        self.check(owned)?;
        let value = plan.to_hex();
        match owned.tombstone.as_deref() {
            Some(existing) if existing == value.as_str() => return Ok(owned.clone()),
            Some(_) => return Err(refused("the object carries another plan's tombstone")),
            None => {}
        }
        if owned.uuid.is_none() && owned.target.kind.has_uuid() {
            return Err(refused("an object without a uuid cannot be retired"));
        }
        let changes = BTreeMap::from([(KERNEL_RETIRING_KEY.to_string(), Some(value.clone()))]);
        let after = apply(&owned.config, &changes);
        if !kernel_may_write(
            &classify_object(&owned.target, &after, &self.retained),
            &self.entry,
        ) {
            return Err(refused(
                "the tombstone would leave the object owned by another owner",
            ));
        }
        let whole = self.send_config(owned, &changes, &after)?;
        let uuid = owned.uuid.clone();
        self.confirm(&owned.target, move |fresh| {
            if fresh.uuid() != uuid.as_deref() {
                Err("the read-back names another uuid".to_string())
            } else if fresh.tombstone() != Some(value.as_str()) {
                Err("the read-back does not show the tombstone".to_string())
            } else if !holds_sent(fresh, whole.as_ref()) {
                Err("the read-back lost a key the whole config sent".to_string())
            } else {
                Ok(())
            }
        })
    }

    /// Sends a config change under the ETag of `owned`, which the classifier checked. An ACL's PATCH also preserves its
    /// description and both rule arrays, which Incus replaces even when omitted. A project takes a PUT of `after`, its whole
    /// config with the change applied, and of the description
    /// the read showed: Incus 7.3's project PATCH would replace the whole config with the keys it named. Incus checks a PUT's
    /// `If-Match` against the project's description and config, so a project changed since the read is 412 and changes nothing,
    /// as a stale PATCH is. Returns the whole config a PUT sent, which the read-back must hold.
    fn send_config(
        &self,
        owned: &Owned,
        changes: &BTreeMap<String, Option<String>>,
        after: &BTreeMap<String, String>,
    ) -> Result<Option<BTreeMap<String, String>>, IncusError> {
        let urls = urls(&owned.target)?;
        if owned.target.kind != IncusKind::Project {
            let mut body = config_patch(changes);
            if owned.target.kind == IncusKind::Acl {
                body = acl_patch(owned, body["config"].clone())?;
            }
            self.mutate("PATCH", &urls.object, Some(&owned.etag), Some(&body))?;
            return Ok(None);
        }
        let body = json!({ "description": owned.description, "config": after });
        self.mutate("PUT", &urls.object, Some(&owned.etag), Some(&body))?;
        Ok(Some(after.clone()))
    }

    /// Deletes an object that carries its tombstone. The object is first read again, and it must show the same tombstone, and the same uuid where Incus gives one. A
    /// running instance is forcibly stopped and its stop operation waited on; a project has the images in its own store deleted
    /// (see [`Self::clear_images`]). The DELETE carries no `If-Match`, because Incus checks ETags only on PUT and PATCH.
    pub fn delete(&self, owned: &Owned) -> Result<(), IncusError> {
        self.check(owned)?;
        let Some(plan) = owned.tombstone.as_deref() else {
            return Err(refused("an object is deleted only after its tombstone"));
        };
        if owned.uuid.is_none() && owned.target.kind.has_uuid() {
            return Err(refused("an object without a uuid cannot be deleted"));
        }
        let fresh = self.read_owned(&owned.target)?;
        if fresh.uuid != owned.uuid || fresh.tombstone.as_deref() != Some(plan) {
            return Err(refused("the object changed after its tombstone"));
        }
        let urls = urls(&owned.target)?;
        if owned.target.kind == IncusKind::Instance {
            let (object, query) = urls
                .object
                .split_once('?')
                .ok_or_else(|| decode("an instance URL has no project"))?;
            let state = format!("{object}/state?{query}");
            let framed = self.send("GET", &state, None, None)?;
            let envelope = checked_envelope(&framed.into_raw())?;
            let status = envelope["metadata"]["status"]
                .as_str()
                .ok_or_else(|| decode("the instance state has no status"))?;
            if status == "Running" {
                self.mutate(
                    "PUT",
                    &state,
                    None,
                    Some(&json!({"action": "stop", "force": true})),
                )?;
            }
        }
        if owned.target.kind == IncusKind::Project {
            self.clear_images(&fresh)?;
        }
        self.mutate("DELETE", &urls.object, None, None)
    }

    /// Deletes every image in a retiring project's own image store, so that Incus lets the project go. An image carries no mark:
    /// one in a kernel project's own store is the project's by containment, since only a write into the project could have put it
    /// there, and it goes with the project. The project must be this entry's own and carry its tombstone. Its own config must set
    /// `features.images` to `"true"`; otherwise the project reads the default project's store, no image in it is the project's,
    /// and nothing is listed or deleted. Each listed image must name a plain fingerprint and name this project; any other image,
    /// including one that names no project, is refused, so nothing outside the project is deleted.
    fn clear_images(&self, project: &Owned) -> Result<(), IncusError> {
        self.check(project)?;
        if project.target.kind != IncusKind::Project || project.tombstone.is_none() {
            return Err(refused("only a retiring project's images are deleted"));
        }
        if project.config.get(IMAGES_KEY).map(String::as_str) != Some("true") {
            return Ok(());
        }
        let name = segment(&project.target.name)?;
        let framed = self.send(
            "GET",
            &format!("/1.0/images?project={name}&recursion=1"),
            None,
            None,
        )?;
        let envelope = checked_envelope(&framed.into_raw())?;
        let images = envelope
            .get("metadata")
            .and_then(Value::as_array)
            .ok_or_else(|| decode("the image listing is not a list"))?;
        let mut fingerprints = Vec::new();
        for image in images {
            let fingerprint = image
                .get("fingerprint")
                .and_then(Value::as_str)
                .filter(|fingerprint| image_fingerprint(fingerprint))
                .ok_or_else(|| decode("an image names no plain fingerprint"))?;
            if image.get("project").and_then(Value::as_str) != Some(name) {
                return Err(refused(
                    "the listing names an image that is not the project's own",
                ));
            }
            fingerprints.push(fingerprint.to_string());
        }
        for fingerprint in fingerprints {
            self.mutate(
                "DELETE",
                &format!("/1.0/images/{fingerprint}?project={name}"),
                None,
                None,
            )?;
        }
        Ok(())
    }

    /// Refuses a target whose kind and project are outside the family, before any request is made.
    fn allow_target(&self, target: &Target) -> Result<(), IncusError> {
        if self.family.allows(target.kind, &target.project) {
            Ok(())
        } else {
            Err(refused("the project is outside the kernel's family"))
        }
    }

    /// Refuses a forward whose network is not this entry's own. A forward is made under its network, so one under another
    /// owner's bridge would change that bridge. The network is read, and it must class as this entry's own.
    fn network_is_own(&self, network: &str) -> Result<(), IncusError> {
        let bridge = ObjectRef {
            kind: IncusKind::Network,
            project: NETWORK_HOME.to_string(),
            parent: None,
            name: network.to_string(),
            uuid: None,
        };
        self.read_owned(&bridge).map(|_| ())
    }

    /// Classes a read's object for this entry: it must be the entry's own, and a target that names a uuid must see that uuid.
    /// The class a refusal found is kept, so an unconfirmed write can report it.
    fn classed(
        &self,
        target: &Target,
        etag: String,
        config: BTreeMap<String, String>,
        description: String,
        metadata: Value,
    ) -> Result<Owned, Unread> {
        let class = classify_object(target, &config, &self.retained);
        if !kernel_may_write(&class, &self.entry) {
            return Err(Unread {
                reason: refusal_reason(&class).to_string(),
                owner: Some(class),
            });
        }
        let uuid = uuid_of(target.kind, &config);
        if target.uuid.is_some() && target.uuid != uuid {
            return Err(Unread {
                reason: "the object is not the one the target names".to_string(),
                owner: Some(class),
            });
        }
        Ok(Owned {
            ingress: (target.kind == IncusKind::Acl)
                .then(|| metadata.get("ingress").and_then(Value::as_array).cloned())
                .flatten(),
            egress: (target.kind == IncusKind::Acl)
                .then(|| metadata.get("egress").and_then(Value::as_array).cloned())
                .flatten(),
            entry: self.entry.clone(),
            target: target.clone(),
            etag,
            uuid,
            tombstone: config.get(KERNEL_RETIRING_KEY).cloned(),
            config,
            description,
        })
    }

    /// Reads an object back after a write that Incus accepted. The read must class as this entry's own and pass `expect`.
    /// Anything else is [`IncusError::AppliedUnconfirmed`], carrying the class the read found when it found one.
    fn confirm(
        &self,
        target: &Target,
        expect: impl FnOnce(&Owned) -> Result<(), String>,
    ) -> Result<Owned, IncusError> {
        // The uuid the read-back showed is carried when the read showed one. A failed read showed none.
        let landed = |uuid: Option<String>, owner: Option<OwnerClass>, reason: String| {
            IncusError::AppliedUnconfirmed(Box::new(Unconfirmed {
                target: target.clone(),
                uuid,
                owner,
                reason,
            }))
        };
        let (metadata, etag) = self
            .fetch(target)
            .map_err(|error| landed(None, None, format!("the read-back failed: {error}")))?;
        let config = own_config(&metadata)
            .map_err(|error| landed(None, None, format!("the read-back failed: {error}")))?;
        let description = own_description(&metadata)
            .map_err(|error| landed(None, None, format!("the read-back failed: {error}")))?;
        let uuid = uuid_of(target.kind, &config);
        let owned = self
            .classed(target, etag, config, description, metadata)
            .map_err(|unread| landed(uuid.clone(), unread.owner, unread.reason))?;
        expect(&owned).map_err(|reason| {
            landed(
                uuid,
                Some(OwnerClass::Kernel {
                    entry: self.entry.clone(),
                }),
                reason,
            )
        })?;
        Ok(owned)
    }

    /// Re-checks an [`Owned`] against this writer before a write: it must name this entry, its project must be in the family,
    /// and its config must still class as this entry's own.
    fn check(&self, owned: &Owned) -> Result<(), IncusError> {
        if owned.entry != self.entry {
            return Err(refused("the object was read for another entry"));
        }
        self.allow_target(&owned.target)?;
        let class = classify_object(&owned.target, &owned.config, &self.retained);
        if !kernel_may_write(&class, &self.entry) {
            return Err(refused(refusal_reason(&class)));
        }
        Ok(())
    }

    /// Checks that a name is free: the object's GET must answer 404. An object there is refused, and so is any other answer.
    fn require_free(&self, object: &str) -> Result<(), IncusError> {
        let framed = self.send("GET", object, None, None)?;
        match checked_envelope(&framed.into_raw()) {
            Ok(_) => Err(refused("the name is already taken")),
            Err(IncusError::Status { code: 404, .. }) => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn fetch(&self, target: &Target) -> Result<(Value, String), IncusError> {
        let urls = urls(target)?;
        let framed = self.send("GET", &urls.object, None, None)?;
        let etag = framed
            .header("etag")
            .filter(|tag| plain(tag))
            .map(str::to_string);
        // The status is judged first, so a 404 or a refused read is a status error, not a missing ETag.
        let envelope = checked_envelope(&framed.into_raw())?;
        let metadata = envelope
            .get("metadata")
            .cloned()
            .ok_or_else(|| decode("the answer has no metadata"))?;
        let etag = etag.ok_or_else(|| decode("the object carries no usable ETag"))?;
        Ok((metadata, etag))
    }

    /// Sends a write and settles it. A sync answer, or one that names no operation, is done. One that names an operation is
    /// waited on, and a failed operation is a status error.
    fn mutate(
        &self,
        method: &str,
        url: &str,
        if_match: Option<&str>,
        body: Option<&Value>,
    ) -> Result<(), IncusError> {
        let framed = self.send(method, url, if_match, body)?;
        let envelope = checked_envelope(&framed.into_raw())?;
        self.wait_for(&envelope)
    }

    fn wait_for(&self, envelope: &Value) -> Result<(), IncusError> {
        if envelope.get("type").and_then(Value::as_str) == Some("sync") {
            return Ok(());
        }
        let Some(operation) = envelope
            .get("operation")
            .and_then(Value::as_str)
            .filter(|operation| !operation.is_empty())
        else {
            return Ok(());
        };
        let id = operation
            .strip_prefix("/1.0/operations/")
            .filter(|id| operation_id(id))
            .ok_or_else(|| decode("the answer names no operation"))?;
        self.wait_on(id).map(|_| ())
    }

    /// Waits on operation `id` until it ends or the writer's deadline passes. Only status code 200 with the status `Success`
    /// is done. A running code (1xx) is waited on again. A 4xx or 5xx in the operation's metadata is the operation's own failure,
    /// and any other code is a failure too. An error answer to the wait request itself is not the operation's status: it is a
    /// decode failure, so the outcome of the write stays unknown. The adoption writer waits through this too.
    pub(crate) fn wait_on(&self, id: &str) -> Result<Value, IncusError> {
        self.wait_on_in(id, None)
    }

    pub(crate) fn wait_on_in(&self, id: &str, project: Option<&str>) -> Result<Value, IncusError> {
        if !operation_id(id) {
            return Err(decode("invalid operation id"));
        }
        let started = Instant::now();
        loop {
            if started.elapsed() >= self.deadline {
                return Err(IncusError::Timeout);
            }
            let mut path = format!("/1.0/operations/{id}/wait?timeout={WAIT_SECONDS}");
            if let Some(project) = project {
                segment(project)?;
                path.push_str(&format!("&project={project}"));
            }
            let remaining = self.deadline.saturating_sub(started.elapsed());
            let framed = self.send_within("GET", &path, None, None, WAIT_TIMEOUT.min(remaining))?;
            let answer = checked_envelope(&framed.into_raw()).map_err(|error| match error {
                IncusError::Status { .. } => {
                    decode("the wait answered without the operation's status")
                }
                other => other,
            })?;
            let operation = answer
                .get("metadata")
                .and_then(Value::as_object)
                .ok_or_else(|| decode("the wait has no operation"))?;
            if operation.get("id").and_then(Value::as_str) != Some(id) {
                return Err(decode("the wait answered for another operation"));
            }
            let code = operation
                .get("status_code")
                .and_then(Value::as_u64)
                .ok_or_else(|| decode("the operation has no status code"))?;
            let status = operation
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("");
            match code {
                200 if status == "Success" => {
                    return Ok(operation.get("metadata").cloned().unwrap_or(Value::Null));
                }
                200 => return Err(decode("the operation reads 200 without Success")),
                100..=199 => thread::sleep(POLL_PAUSE),
                400..=599 => {
                    let message = operation
                        .get("err")
                        .and_then(Value::as_str)
                        .filter(|text| !text.is_empty())
                        .unwrap_or(status);
                    return Err(IncusError::Status {
                        code: u16::try_from(code).unwrap_or(500),
                        message: cap_message(message.to_string()),
                    });
                }
                _ => return Err(decode("the operation ended without success")),
            }
        }
    }

    fn send(
        &self,
        method: &str,
        url: &str,
        if_match: Option<&str>,
        body: Option<&Value>,
    ) -> Result<Framed, IncusError> {
        self.send_within(method, url, if_match, body, TIMEOUT)
    }

    /// Sends one request the caller has already checked, under `timeout`, as [`Self::send_within`] sends it. It skips the
    /// family and the ownership checks, so the adoption and service writers call it only for their closed tables. The
    /// request still passes `exchange`, so its guard applies.
    pub(crate) fn send_checked(
        &self,
        method: &str,
        url: &str,
        if_match: Option<&str>,
        body: Option<&Value>,
        timeout: Duration,
    ) -> Result<Framed, IncusError> {
        self.send_within(method, url, if_match, body, timeout)
    }

    /// Builds the request and exchanges it under `timeout`. `method` is one of the four this module names, and `url` is
    /// checked as a plain origin-form target before the socket is touched. A request refused here is never sent.
    fn send_within(
        &self,
        method: &str,
        url: &str,
        if_match: Option<&str>,
        body: Option<&Value>,
        timeout: Duration,
    ) -> Result<Framed, IncusError> {
        check_path(url)
            .map_err(|_| refused("the request path is not a plain origin-form target"))?;
        let payload = match body {
            Some(value) => Some(
                serde_json::to_vec(value).map_err(|_| decode("a body that does not serialize"))?,
            ),
            None => None,
        };
        if payload
            .as_ref()
            .is_some_and(|bytes| bytes.len() > MAX_WRITE_BODY)
        {
            return Err(refused("the write body is over the size cap"));
        }
        let mut head = format!(
            "{method} {url} HTTP/1.1\r\nHost: incus\r\nAccept: application/json\r\nConnection: close\r\n"
        );
        if let Some(tag) = if_match {
            if !plain(tag) {
                return Err(refused("an ETag that is not plain text"));
            }
            head.push_str(&format!("If-Match: {tag}\r\n"));
        }
        if let Some(bytes) = &payload {
            head.push_str(&format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                bytes.len()
            ));
        }
        head.push_str("\r\n");
        let mut request = head.into_bytes();
        if let Some(bytes) = payload {
            request.extend_from_slice(&bytes);
        }
        exchange(&self.socket, self.request_timeout(timeout), &request)
    }
}

/// Why a read is not an [`Owned`]: the class it found, when it found one, and the reason in words.
struct Unread {
    owner: Option<OwnerClass>,
    reason: String,
}

/// The paths a target's writes use: the collection a create posts to, and the object itself. Both carry the project query,
/// except a project, whose path names it alone.
struct Urls {
    collection: String,
    object: String,
}

fn other_acl_field(field: AclField) -> AclField {
    match field {
        AclField::Ingress => AclField::Egress,
        AclField::Egress => AclField::Ingress,
    }
}

/// Rules in the shape Incus 7.x answers with: a field that is "" or null is left out. Older servers list every field, empty
/// ones as "", so two rule arrays are compared in this shape.
pub fn canonical_acl_rules(rules: &[Value]) -> Vec<Value> {
    rules
        .iter()
        .map(|rule| match rule {
            Value::Object(fields) => Value::Object(
                fields
                    .iter()
                    .filter(|(_, value)| !value.is_null() && value.as_str() != Some(""))
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect(),
            ),
            other => other.clone(),
        })
        .collect()
}

/// Whether two rule arrays hold the same rules once empty fields are left out.
pub fn acl_rules_match(left: &[Value], right: &[Value]) -> bool {
    canonical_acl_rules(left) == canonical_acl_rules(right)
}

// Incus v7.5.1/v7.6.0 networkACLPut merges only config on PATCH. It
// replaces description and both arrays, and needs a non-null config map
// to merge existing keys. Preserve the fields from the If-Match read.
fn acl_patch(owned: &Owned, config: Value) -> Result<Value, IncusError> {
    let (Some(ingress), Some(egress)) = (&owned.ingress, &owned.egress) else {
        return Err(refused("an ACL patch requires both rule arrays"));
    };
    Ok(json!({
        "description": owned.description,
        "ingress": ingress,
        "egress": egress,
        "config": config,
    }))
}

fn urls(target: &Target) -> Result<Urls, IncusError> {
    let object = target;
    let name = segment(&object.name)?;
    let project = segment(&object.project)?;
    let parent = target.parent.as_deref().map(segment).transpose()?;
    let needs_parent = matches!(
        object.kind,
        IncusKind::Volume | IncusKind::Bucket | IncusKind::Forward
    );
    match (needs_parent, parent) {
        (true, None) => return Err(refused("this kind names its storage pool or network")),
        (false, Some(_)) => return Err(refused("this kind takes no parent")),
        _ => {}
    }
    let pool_or_network = parent.unwrap_or_default();
    let (collection, item) = match object.kind {
        IncusKind::Project => {
            if project != name {
                return Err(refused("a project is named by its own name"));
            }
            return Ok(Urls {
                collection: "/1.0/projects".to_string(),
                object: format!("/1.0/projects/{name}"),
            });
        }
        IncusKind::Instance => (
            "/1.0/instances".to_string(),
            format!("/1.0/instances/{name}"),
        ),
        IncusKind::Network => ("/1.0/networks".to_string(), format!("/1.0/networks/{name}")),
        IncusKind::Acl => (
            "/1.0/network-acls".to_string(),
            format!("/1.0/network-acls/{name}"),
        ),
        IncusKind::Profile => ("/1.0/profiles".to_string(), format!("/1.0/profiles/{name}")),
        IncusKind::Zone => (
            "/1.0/network-zones".to_string(),
            format!("/1.0/network-zones/{name}"),
        ),
        IncusKind::Volume => (
            format!("/1.0/storage-pools/{pool_or_network}/volumes/custom"),
            format!("/1.0/storage-pools/{pool_or_network}/volumes/custom/{name}"),
        ),
        IncusKind::Bucket => (
            format!("/1.0/storage-pools/{pool_or_network}/buckets"),
            format!("/1.0/storage-pools/{pool_or_network}/buckets/{name}"),
        ),
        IncusKind::Forward => (
            format!("/1.0/networks/{pool_or_network}/forwards"),
            format!("/1.0/networks/{pool_or_network}/forwards/{name}"),
        ),
    };
    let query = format!("?project={project}");
    Ok(Urls {
        collection: format!("{collection}{query}"),
        object: format!("{item}{query}"),
    })
}

/// The create body's `source`, when it has one, names an image by alias, fingerprint or properties, and nothing else. A
/// copy, a migration, a refresh and a remote each reach another object or another server, so each is refused. A server is
/// refused too, except [`IMAGE_SERVER`] named with [`IMAGE_PROTOCOL`], exactly and together.
fn check_source(fields: &Map<String, Value>) -> Result<(), IncusError> {
    let Some(source) = fields.get("source") else {
        return Ok(());
    };
    let Some(source) = source.as_object() else {
        return Err(refused("a create may name only an image source"));
    };
    let image = source.get("type").and_then(Value::as_str) == Some("image");
    let allowed_keys = source.keys().all(|key| {
        matches!(
            key.as_str(),
            "type" | "alias" | "fingerprint" | "properties" | "server" | "protocol" | "mode"
        )
    });
    let server = source.get("server").map(|value| value.as_str());
    let protocol = source.get("protocol").map(|value| value.as_str());
    let pinned = match (server, protocol) {
        (None, None) => true,
        (Some(server), Some(protocol)) => {
            (server == Some(IMAGE_SERVER) && protocol == Some(IMAGE_PROTOCOL))
                || (protocol == Some("oci")
                    && source.get("mode").and_then(Value::as_str) == Some("pull")
                    && server.is_some_and(oci_server_ok)
                    && source
                        .get("alias")
                        .and_then(Value::as_str)
                        .is_some_and(oci_alias_ok))
        }
        _ => false,
    };
    if !pinned {
        return Err(refused(
            "a create may name no image server but the pinned one, with its protocol",
        ));
    }
    let text = source
        .iter()
        .all(|(key, value)| match (key.as_str(), value) {
            ("properties", Value::Object(properties)) => properties.values().all(Value::is_string),
            ("properties", _) => false,
            _ => value.is_string(),
        });
    if image && allowed_keys && text {
        Ok(())
    } else {
        Err(refused("a create may name only an image source"))
    }
}

/// A registry's credential: the user name and the password a registry asks for before it gives an image. It is
/// read from the node's secret store at the moment of a create and lives nowhere else in Arca.
#[derive(Clone)]
pub struct RegistryCredential {
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for RegistryCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RegistryCredential(..)")
    }
}

impl RegistryCredential {
    /// The registry `server` (`https://<host>`) as Incus takes it with this credential:
    /// the same address with the user name, a colon, the password and an `@` before the host, each written as an
    /// address writes them
    /// (everything but letters, digits and `- . _ ~` as `%XX`). `None` for a server that is not a plain registry
    /// address, or an empty user name or password.
    pub fn address(&self, server: &str) -> Option<String> {
        if !oci_server_ok(server) || self.username.is_empty() || self.password.is_empty() {
            return None;
        }
        let host = server.strip_prefix("https://")?;
        let encoded = |text: &str| -> String {
            text.bytes()
                .map(|byte| {
                    if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                        (byte as char).to_string()
                    } else {
                        format!("%{byte:02X}")
                    }
                })
                .collect()
        };
        let mut address = String::from("https://");
        address.push_str(&encoded(&self.username));
        address.push(':');
        address.push_str(&encoded(&self.password));
        address.push('@');
        address.push_str(host);
        Some(address)
    }
}

/// An OCI registry server: `https://` and a host, with an optional port. No path, credentials or query.
fn oci_server_ok(server: &str) -> bool {
    let Some(host) = server.strip_prefix("https://") else {
        return false;
    };
    !host.is_empty()
        && !host.contains(['/', '@', '?', '#'])
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':'))
}

/// An OCI alias: a repository and a digest, `repo@sha256:<64 lowercase hex>`.
fn oci_alias_ok(alias: &str) -> bool {
    let Some((repository, digest)) = alias.rsplit_once("@sha256:") else {
        return false;
    };
    !repository.is_empty()
        && repository
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-'))
        && digest.len() == 64
        && digest.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A key the writer sends: `[a-z0-9_.-]`, plus uppercase ASCII after a leading `environment.`. Incus's decoder matches a key to a field whatever its case,
/// and folds some Unicode letters onto ASCII ones, so a key outside this set can reach a field the checks did not read.
fn plain_key(key: &str) -> bool {
    let environment = key.starts_with("environment.");
    !key.is_empty()
        && key.bytes().all(|b| {
            b.is_ascii_lowercase()
                || b.is_ascii_digit()
                || matches!(b, b'_' | b'.' | b'-')
                || (environment && b.is_ascii_uppercase())
        })
}

/// Whether every object key in a value is plain, at any depth through objects and arrays.
fn plain_keys(value: &Value) -> bool {
    match value {
        Value::Object(map) => map
            .iter()
            .all(|(key, value)| plain_key(key) && plain_keys(value)),
        Value::Array(items) => items.iter().all(plain_keys),
        _ => true,
    }
}

/// Whether a config key is in Incus's `volatile` namespace, which Incus sets for itself.
fn volatile(key: &str) -> bool {
    key == "volatile" || key.starts_with("volatile.")
}

/// The uuid of an object of this kind. Only the kinds Incus gives one have a uuid, so any other kind has none.
fn uuid_of(kind: IncusKind, config: &BTreeMap<String, String>) -> Option<String> {
    if !kind.has_uuid() {
        return None;
    }
    config
        .get("volatile.uuid")
        .filter(|uuid| !uuid.is_empty())
        .cloned()
}

/// A name that is safe as one path segment or query value: letters, digits and `-_.:`, not `.` or `..`, at most 253 bytes.
fn segment(text: &str) -> Result<&str, IncusError> {
    let plain = !text.is_empty()
        && text.len() <= 253
        && text != "."
        && text != ".."
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'));
    if plain {
        Ok(text)
    } else {
        Err(refused("a name that is not a plain path segment"))
    }
}

/// An image fingerprint: the 64 lowercase hex characters of a SHA-256.
fn image_fingerprint(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// An operation id: the characters of a uuid.
pub(crate) fn operation_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

/// A header value the writer may send or accept: visible ASCII and spaces, with no line break.
fn plain(tag: &str) -> bool {
    !tag.is_empty() && tag.bytes().all(|b| (0x20..=0x7e).contains(&b))
}

/// The string-valued config of an object, or `None` when a value is not text.
fn text_map(config: &Map<String, Value>) -> Option<BTreeMap<String, String>> {
    config
        .iter()
        .map(|(key, value)| value.as_str().map(|text| (key.clone(), text.to_string())))
        .collect()
}

/// The object's own config, read from its `config` field. A value that is not text is a decode error.
fn own_config(metadata: &Value) -> Result<BTreeMap<String, String>, IncusError> {
    let config = metadata
        .get("config")
        .and_then(Value::as_object)
        .ok_or_else(|| decode("the object has no config object"))?;
    text_map(config).ok_or_else(|| decode("a config value is not text"))
}

/// The object's description, from its `description` field: empty when the object has none, and a decode error when it is not
/// text.
fn own_description(metadata: &Value) -> Result<String, IncusError> {
    match metadata.get("description") {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(text)) => Ok(text.clone()),
        Some(_) => Err(decode("the object's description is not text")),
    }
}

/// Whether a read-back holds every key of the whole config a PUT sent, with the value it sent. A write that was not a PUT sent
/// no whole config, so there is nothing more to hold.
fn holds_sent(fresh: &Owned, whole: Option<&BTreeMap<String, String>>) -> bool {
    whole.is_none_or(|sent| {
        sent.iter()
            .all(|(key, value)| fresh.config().get(key) == Some(value))
    })
}

/// The config after a change: a `Some` sets a key and a `None` removes it.
fn apply(
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

/// The PATCH body for a change to config: each key is set, and a removed key is sent as the empty string.
fn config_patch(changes: &BTreeMap<String, Option<String>>) -> Value {
    let config: Map<String, Value> = changes
        .iter()
        .map(|(key, value)| {
            (
                key.clone(),
                Value::String(value.clone().unwrap_or_default()),
            )
        })
        .collect();
    json!({ "config": config })
}

fn refused(reason: &str) -> IncusError {
    IncusError::Refused(reason.to_string())
}

fn decode(reason: &str) -> IncusError {
    IncusError::Decode(reason.to_string())
}

/// The refusal for an object whose class does not let this entry write it. The text names the class and never a value.
fn refusal_reason(class: &OwnerClass) -> &'static str {
    match class {
        OwnerClass::Droidd => "the object is owned by another system",
        OwnerClass::Foreign => "the object carries no kernel mark",
        OwnerClass::Kernel { .. } => "the object belongs to another kernel entry",
        OwnerClass::Retained { .. } => "the object is retained from a declined plan",
        OwnerClass::Contested => "the object carries marks from two owners",
    }
}
