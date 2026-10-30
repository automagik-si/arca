//! The Incus writer: the one module in this crate that sends a method other than GET. It is compiled only with the `write`
//! feature, which only the provider crate enables.
//!
//! Every write is gated twice. The project family is checked first: a target outside the projects the kernel's configuration
//! names is refused before any request. The ownership classifier is checked next: a read returns an [`Owned`] only when
//! `kernel_may_write` allows the writer's entry to change the object, so a PATCH, a tombstone or a DELETE can name only an
//! object the entry owns. A PATCH and a tombstone send the ETag of the read they were checked against as `If-Match`. A DELETE
//! reads the object again first, because Incus checks no ETag on a DELETE. A create is refused unless its body classes as the
//! entry's own, its source is an image, and its name is free. Writes are POST, PATCH and DELETE; nothing here sends PUT.
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
use arca_kernel::ownership::{
    IncusKind, KERNEL_RETIRING_KEY, ObjectRef, OwnerClass, classify, kernel_may_write,
};
use serde_json::{Map, Value, json};

use crate::read::{
    Framed, IncusError, TIMEOUT, Unconfirmed, cap_message, check_path, checked_envelope, exchange,
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

/// The object a write names. `parent` is the storage pool of a volume or a bucket, and the network of a forward. It is
/// required for those kinds and refused for every other kind. Every name must be a plain path segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub object: ObjectRef,
    pub parent: Option<String>,
}

/// An object the writer read and classed as its entry's own. Its fields are private, and only [`UnixIncusWriter`] makes one,
/// so holding one is proof that the classifier allowed the entry to change the object at that read. Its `etag` is the one
/// the read returned, and a PATCH sends it as `If-Match`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owned {
    entry: EntryId,
    target: Target,
    etag: String,
    uuid: Option<String>,
    config: BTreeMap<String, String>,
    tombstone: Option<String>,
}

impl Owned {
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

    /// The object's uuid, for an instance or a volume: its `volatile.uuid` config key. Other kinds carry none, and they are
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

/// The projects the kernel may write in: the names its configuration lists, and nothing else.
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

    /// Whether a write may name the project.
    pub fn allows(&self, project: &str) -> bool {
        !RESERVED_PROJECTS.contains(&project) && self.projects.contains(project)
    }
}

/// The writer for one kernel entry. It names the entry it writes for, the retained entries whose objects it may not write,
/// and the project family it may write in. Every mutating method refuses an object the classifier does not class as this
/// entry's own, before it sends.
#[derive(Debug, Clone)]
pub struct UnixIncusWriter {
    socket: PathBuf,
    entry: EntryId,
    retained: BTreeMap<EntryId, (PluginId, Digest)>,
    family: ProjectFamily,
    deadline: Duration,
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
        }
    }

    /// The same writer with another overall deadline for an operation, from its acceptance to its end.
    pub fn with_operation_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }

    /// The entry this writer writes for.
    pub fn entry(&self) -> &EntryId {
        &self.entry
    }

    /// A GET of an object's metadata and its ETag. Every owner may be read, so no classification applies here.
    pub fn get_etag(&self, target: &Target) -> Result<(Value, String), IncusError> {
        self.fetch(target)
    }

    /// Reads an object and returns it as [`Owned`] when this entry may change it. A target outside the family, an object of
    /// another class, and a target whose uuid is not the object's are refused, and nothing is written.
    pub fn read_owned(&self, target: &Target) -> Result<Owned, IncusError> {
        self.allow_project(&target.object.project)?;
        let (metadata, etag) = self.fetch(target)?;
        let config = own_config(&metadata)?;
        self.classed(target, etag, config)
            .map_err(|unread| refused(&unread.reason))
    }

    /// Creates an object. The target's project must be in the family, and the target names no uuid yet. The body's source,
    /// when it has one, must be an image. Every key in the body, at any depth, must be plain ASCII, and the body's config may
    /// not set a `volatile.*` key. The body's config must class as this entry's own, so the object belongs to the entry from
    /// the moment it exists. The name must be free: its GET must answer 404. The POST is then sent, and the object is read
    /// back, which must class as this entry's own.
    pub fn create(&self, target: &Target, body: &Value) -> Result<Owned, IncusError> {
        self.allow_project(&target.object.project)?;
        if target.object.uuid.is_some() {
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
        let class = classify(&config, &self.retained);
        if !kernel_may_write(&class, &self.entry) {
            return Err(refused(
                "a created object must carry this entry's mark and no other owner's key",
            ));
        }
        let mut sent = fields.clone();
        sent.insert("name".to_string(), json!(target.object.name));
        if target.object.kind == IncusKind::Forward {
            sent.insert("listen_address".to_string(), json!(target.object.name));
        }
        self.require_free(&urls.object)?;
        self.mutate("POST", &urls.collection, None, Some(&Value::Object(sent)))?;
        self.confirm(target, |_| Ok(()))
    }

    /// Sets and removes config keys of an object this entry owns, under the ETag of `owned`. A removed key is sent as the empty
    /// string. The change must leave the object classed as this entry's own, and it may not name the tombstone key, a key that
    /// is not plain ASCII, or a `volatile.*` key. The object is read back, and the read must show the change.
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
        if !kernel_may_write(&classify(&after, &self.retained), &self.entry) {
            return Err(refused(
                "the change would leave the object owned by another owner",
            ));
        }
        let urls = urls(&owned.target)?;
        self.mutate(
            "PATCH",
            &urls.object,
            Some(&owned.etag),
            Some(&config_patch(changes)),
        )?;
        let expected = changes.clone();
        self.confirm(&owned.target, move |fresh| {
            let shows = expected.iter().all(|(key, value)| {
                fresh.config().get(key).map(String::as_str) == value.as_deref()
            });
            if shows {
                Ok(())
            } else {
                Err("the read-back does not show the change".to_string())
            }
        })
    }

    /// Marks an object as retiring with the digest of its delete plan, under the ETag of `owned`, then reads it back. The
    /// read must show the same uuid, this entry's mark and the tombstone. The result is the only input [`Self::delete`]
    /// takes.
    pub fn tombstone(&self, owned: &Owned, plan: &Digest) -> Result<Owned, IncusError> {
        self.check(owned)?;
        let value = plan.to_hex();
        match owned.tombstone.as_deref() {
            Some(existing) if existing == value.as_str() => return Ok(owned.clone()),
            Some(_) => return Err(refused("the object carries another plan's tombstone")),
            None => {}
        }
        if owned.uuid.is_none() && has_uuid(owned.target.object.kind) {
            return Err(refused("an object without a uuid cannot be retired"));
        }
        let changes = BTreeMap::from([(KERNEL_RETIRING_KEY.to_string(), Some(value.clone()))]);
        let after = apply(&owned.config, &changes);
        if !kernel_may_write(&classify(&after, &self.retained), &self.entry) {
            return Err(refused(
                "the tombstone would leave the object owned by another owner",
            ));
        }
        let urls = urls(&owned.target)?;
        self.mutate(
            "PATCH",
            &urls.object,
            Some(&owned.etag),
            Some(&config_patch(&changes)),
        )?;
        let uuid = owned.uuid.clone();
        self.confirm(&owned.target, move |fresh| {
            if fresh.uuid() != uuid.as_deref() {
                Err("the read-back names another uuid".to_string())
            } else if fresh.tombstone() != Some(value.as_str()) {
                Err("the read-back does not show the tombstone".to_string())
            } else {
                Ok(())
            }
        })
    }

    /// Deletes an object that carries its tombstone. The object is read again first, and it must show the same tombstone,
    /// and the same uuid where Incus gives one. The DELETE carries no `If-Match`, because Incus checks ETags only on PUT and
    /// PATCH.
    pub fn delete(&self, owned: &Owned) -> Result<(), IncusError> {
        self.check(owned)?;
        let Some(plan) = owned.tombstone.as_deref() else {
            return Err(refused("an object is deleted only after its tombstone"));
        };
        if owned.uuid.is_none() && has_uuid(owned.target.object.kind) {
            return Err(refused("an object without a uuid cannot be deleted"));
        }
        let fresh = self.read_owned(&owned.target)?;
        if fresh.uuid != owned.uuid || fresh.tombstone.as_deref() != Some(plan) {
            return Err(refused("the object changed after its tombstone"));
        }
        let urls = urls(&owned.target)?;
        self.mutate("DELETE", &urls.object, None, None)
    }

    /// Refuses a target whose project is outside the family, before any request is made.
    fn allow_project(&self, project: &str) -> Result<(), IncusError> {
        if self.family.allows(project) {
            Ok(())
        } else {
            Err(refused("the project is outside the kernel's family"))
        }
    }

    /// Classes a read's object for this entry: it must be the entry's own, and a target that names a uuid must see that uuid.
    /// The class a refusal found is kept, so an unconfirmed write can report it.
    fn classed(
        &self,
        target: &Target,
        etag: String,
        config: BTreeMap<String, String>,
    ) -> Result<Owned, Unread> {
        let class = classify(&config, &self.retained);
        if !kernel_may_write(&class, &self.entry) {
            return Err(Unread {
                reason: refusal_reason(&class).to_string(),
                owner: Some(class),
            });
        }
        let uuid = uuid_of(target.object.kind, &config);
        if target.object.uuid.is_some() && target.object.uuid != uuid {
            return Err(Unread {
                reason: "the object is not the one the target names".to_string(),
                owner: Some(class),
            });
        }
        Ok(Owned {
            entry: self.entry.clone(),
            target: target.clone(),
            etag,
            uuid,
            tombstone: config.get(KERNEL_RETIRING_KEY).cloned(),
            config,
        })
    }

    /// Reads an object back after a write that Incus accepted. The read must class as this entry's own and pass `expect`.
    /// Anything else is [`IncusError::AppliedUnconfirmed`], carrying the class the read found when it found one.
    fn confirm(
        &self,
        target: &Target,
        expect: impl FnOnce(&Owned) -> Result<(), String>,
    ) -> Result<Owned, IncusError> {
        let landed = |owner: Option<OwnerClass>, reason: String| {
            IncusError::AppliedUnconfirmed(Box::new(Unconfirmed {
                object: target.object.clone(),
                owner,
                reason,
            }))
        };
        let (metadata, etag) = self
            .fetch(target)
            .map_err(|error| landed(None, format!("the read-back failed: {error}")))?;
        let config = own_config(&metadata)
            .map_err(|error| landed(None, format!("the read-back failed: {error}")))?;
        let owned = self
            .classed(target, etag, config)
            .map_err(|unread| landed(unread.owner, unread.reason))?;
        expect(&owned).map_err(|reason| {
            landed(
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
        self.allow_project(&owned.target.object.project)?;
        let class = classify(&owned.config, &self.retained);
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
        self.wait_on(id)
    }

    /// Waits on operation `id` until it ends or the writer's deadline passes. Only status code 200 with the status `Success`
    /// is done. A running code (1xx) is waited on again. A 4xx or 5xx is the operation's own failure, and any other code is
    /// a failure too.
    fn wait_on(&self, id: &str) -> Result<(), IncusError> {
        let started = Instant::now();
        loop {
            if started.elapsed() >= self.deadline {
                return Err(IncusError::Timeout);
            }
            let path = format!("/1.0/operations/{id}/wait?timeout={WAIT_SECONDS}");
            let framed = self.send_within("GET", &path, None, None, WAIT_TIMEOUT)?;
            let answer = checked_envelope(&framed.into_raw())?;
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
                200 if status == "Success" => return Ok(()),
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

    /// Builds the request and exchanges it under `timeout`. `method` is one of the three this module names, and `url` is
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
        exchange(&self.socket, timeout, &request)
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

fn urls(target: &Target) -> Result<Urls, IncusError> {
    let object = &target.object;
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
/// copy, a migration, a refresh, a remote and a server each reach another object or another server, so each is refused.
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
            "type" | "alias" | "fingerprint" | "properties"
        )
    });
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

/// A key the writer sends: plain ASCII `[a-z0-9_.-]`, and not empty. Incus's decoder matches a key to a field whatever its case,
/// and folds some Unicode letters onto ASCII ones, so a key outside this set can reach a field the checks did not read.
fn plain_key(key: &str) -> bool {
    !key.is_empty()
        && key.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-')
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

/// Whether Incus gives an object of this kind a uuid: instances and volumes do, through their `volatile.uuid` key.
fn has_uuid(kind: IncusKind) -> bool {
    matches!(kind, IncusKind::Instance | IncusKind::Volume)
}

/// The uuid of an object of this kind. Only the kinds Incus gives one have a uuid, so any other kind has none.
fn uuid_of(kind: IncusKind, config: &BTreeMap<String, String>) -> Option<String> {
    if !has_uuid(kind) {
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

/// An operation id: the characters of a uuid.
fn operation_id(id: &str) -> bool {
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
