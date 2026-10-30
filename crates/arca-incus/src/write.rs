//! The Incus writer: the one module in this crate that sends a method other than GET.
//!
//! Every write is gated by the ownership classifier. A read returns an [`Owned`] only when `kernel_may_write` allows the
//! writer's entry to change the object, and a PATCH, a tombstone or a DELETE takes an [`Owned`], so it can name only an
//! object the entry owns. A PATCH sends the `If-Match` of the read it was checked against, so it lands only on that state.
//! A create sends only a config that classes as the entry's own, and it reads the object back to confirm the mark. Every
//! write waits on its operation, and a failed operation is an [`IncusError::Status`]. Writes are POST, PATCH and DELETE;
//! nothing here sends PUT.

use std::collections::BTreeMap;
use std::path::PathBuf;

use arca_kernel::canon::Digest;
use arca_kernel::desired::{EntryId, PluginId};
use arca_kernel::ownership::{
    IncusKind, KERNEL_RETIRING_KEY, ObjectRef, OwnerClass, classify, kernel_may_write,
};
use serde_json::{Map, Value, json};

use crate::read::{
    Framed, IncusError, TIMEOUT, cap_message, check_path, checked_envelope, exchange,
};

/// The largest write body the writer sends, in bytes.
const MAX_WRITE_BODY: usize = 1024 * 1024;

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

    /// The object's uuid, from its `uuid` field or its `volatile.uuid` config key, when it has one.
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

/// The writer for one kernel entry. It names the entry it writes for and the retained entries whose objects it may not
/// write. Every mutating method refuses an object the classifier does not class as this entry's own, before it sends.
#[derive(Debug, Clone)]
pub struct UnixIncusWriter {
    socket: PathBuf,
    entry: EntryId,
    retained: BTreeMap<EntryId, (PluginId, Digest)>,
}

impl UnixIncusWriter {
    /// A writer for `entry` over the Incus socket at `socket`. `retained` holds the entries whose plans were declined, so
    /// their objects classify as retained and are never written.
    pub fn new(
        socket: PathBuf,
        entry: EntryId,
        retained: BTreeMap<EntryId, (PluginId, Digest)>,
    ) -> Self {
        Self {
            socket,
            entry,
            retained,
        }
    }

    /// The entry this writer writes for.
    pub fn entry(&self) -> &EntryId {
        &self.entry
    }

    /// A GET of an object's metadata and its ETag. Every owner may be read, so no classification applies here.
    pub fn get_etag(&self, target: &Target) -> Result<(Value, String), IncusError> {
        self.fetch(target)
    }

    /// Reads an object and returns it as [`Owned`] when this entry may change it. Any other class is refused, and nothing is
    /// written.
    pub fn read_owned(&self, target: &Target) -> Result<Owned, IncusError> {
        let (metadata, etag) = self.fetch(target)?;
        let config = own_config(&metadata)?;
        self.owned(target, etag, &metadata, config)
    }

    /// Creates an object. The body's config must class as this entry's own, so the object belongs to the entry from the
    /// moment it exists, and no other owner's key may be in it. The body's name is set from the target. The object is then
    /// read back, and the read must class as this entry's own.
    pub fn create(&self, target: &Target, body: &Value) -> Result<Owned, IncusError> {
        let urls = urls(target)?;
        let Value::Object(fields) = body else {
            return Err(refused("a create body is not an object"));
        };
        let config = match fields.get("config") {
            Some(Value::Object(config)) => text_map(config)
                .ok_or_else(|| refused("a config value in a create body is not text"))?,
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
        self.mutate("POST", &urls.collection, None, Some(&Value::Object(sent)))?;
        self.read_owned(target)
    }

    /// Sets and removes config keys of an object this entry owns, under the ETag of `owned`. A removed key is sent as the empty
    /// string. The change must leave the object classed as this entry's own, and it may not name the tombstone key. Returns
    /// the object as it reads back.
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
        self.read_owned(&owned.target)
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
        if owned.uuid.is_none() {
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
        let fresh = self.read_owned(&owned.target)?;
        if fresh.uuid != owned.uuid || fresh.tombstone.as_deref() != Some(value.as_str()) {
            return Err(refused(
                "the tombstone did not read back on the same object",
            ));
        }
        Ok(fresh)
    }

    /// Deletes an object that carries its tombstone. The object is read again first, and it must show the same uuid and the
    /// same tombstone. The DELETE carries no `If-Match`, because Incus checks ETags only on PUT and PATCH.
    pub fn delete(&self, owned: &Owned) -> Result<(), IncusError> {
        self.check(owned)?;
        let Some(plan) = owned.tombstone.as_deref() else {
            return Err(refused("an object is deleted only after its tombstone"));
        };
        let Some(uuid) = owned.uuid.as_deref() else {
            return Err(refused("an object without a uuid cannot be deleted"));
        };
        let fresh = self.read_owned(&owned.target)?;
        if fresh.uuid.as_deref() != Some(uuid) || fresh.tombstone.as_deref() != Some(plan) {
            return Err(refused("the object changed after its tombstone"));
        }
        let urls = urls(&owned.target)?;
        self.mutate("DELETE", &urls.object, None, None)
    }

    fn owned(
        &self,
        target: &Target,
        etag: String,
        metadata: &Value,
        config: BTreeMap<String, String>,
    ) -> Result<Owned, IncusError> {
        let class = classify(&config, &self.retained);
        if !kernel_may_write(&class, &self.entry) {
            return Err(refusal(&class));
        }
        Ok(Owned {
            entry: self.entry.clone(),
            target: target.clone(),
            etag,
            uuid: uuid_of(metadata, &config),
            tombstone: config.get(KERNEL_RETIRING_KEY).cloned(),
            config,
        })
    }

    /// Re-checks an [`Owned`] against this writer before a write: it must name this entry, and its config must still class as
    /// this entry's own.
    fn check(&self, owned: &Owned) -> Result<(), IncusError> {
        if owned.entry != self.entry {
            return Err(refused("the object was read for another entry"));
        }
        let class = classify(&owned.config, &self.retained);
        if !kernel_may_write(&class, &self.entry) {
            return Err(refusal(&class));
        }
        Ok(())
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

    /// Sends a write and waits on the operation it starts. A failed operation is a status error.
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
        let Some(operation) = envelope.get("operation").and_then(Value::as_str) else {
            return Ok(());
        };
        let id = operation
            .strip_prefix("/1.0/operations/")
            .filter(|id| operation_id(id))
            .ok_or_else(|| decode("the answer names no operation"))?;
        let framed = self.send("GET", &format!("/1.0/operations/{id}/wait"), None, None)?;
        let answer = checked_envelope(&framed.into_raw())?;
        let metadata = answer
            .get("metadata")
            .ok_or_else(|| decode("the operation has no metadata"))?;
        let code = metadata
            .get("status_code")
            .and_then(Value::as_u64)
            .ok_or_else(|| decode("the operation has no status"))?;
        if code < 400 {
            return Ok(());
        }
        let code = u16::try_from(code)
            .ok()
            .filter(|code| (400..600).contains(code))
            .unwrap_or(500);
        let message = metadata
            .get("err")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        Err(IncusError::Status {
            code,
            message: cap_message(message),
        })
    }

    /// Builds the request and exchanges it. `method` is one of the four this module names, and `url` is checked as a plain
    /// origin-form target before the socket is touched.
    fn send(
        &self,
        method: &str,
        url: &str,
        if_match: Option<&str>,
        body: Option<&Value>,
    ) -> Result<Framed, IncusError> {
        check_path(url)?;
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
            return Err(IncusError::TooLarge);
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
        exchange(&self.socket, TIMEOUT, &request)
    }
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

/// The object's uuid: its `uuid` field, else its `volatile.uuid` config key.
fn uuid_of(metadata: &Value, config: &BTreeMap<String, String>) -> Option<String> {
    metadata
        .get("uuid")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| config.get("volatile.uuid").cloned())
        .filter(|uuid| !uuid.is_empty())
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
fn refusal(class: &OwnerClass) -> IncusError {
    refused(match class {
        OwnerClass::Droidd => "the object is owned by another system",
        OwnerClass::Foreign => "the object carries no kernel mark",
        OwnerClass::Kernel { .. } => "the object belongs to another kernel entry",
        OwnerClass::Retained { .. } => "the object is retained from a declined plan",
        OwnerClass::Contested => "the object carries marks from two owners",
    })
}
