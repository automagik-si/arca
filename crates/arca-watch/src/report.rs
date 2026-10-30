//! The watch report: every instance in the projects a watch covers, classed by owner, with its privilege, identity flag and
//! config hash, and the drift between the marks on the host and the live desired state. Its JSON is the `watch_report`
//! result schema of `arca-api`.

use std::collections::BTreeMap;

use arca_kernel::canon::{Digest, canonical_json};
use arca_kernel::desired::EntryId;
use arca_kernel::ownership::OwnerClass;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

/// The prefix of the keys an instance's life changes (a start, a stop, a lease address). A config hash leaves them out.
pub const VOLATILE_PREFIX: &str = "volatile.";

/// One instance as the watch reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceRow {
    pub name: String,
    /// `container` or `virtual-machine`, as Incus names the type.
    pub kind: String,
    pub status: String,
    pub owner: OwnerClass,
    /// Whether `security.privileged` is `true` in the instance's expanded config.
    pub privileged: bool,
    /// Whether the instance's name is one of the node's identity phones.
    pub identity: bool,
    /// The lowercase hex SHA-256 of the canonical JSON of the instance's own config, without its `volatile.*` keys.
    pub config_hash: String,
}

/// The instances of one project, sorted by name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRows {
    pub project: String,
    pub instances: Vec<InstanceRow>,
}

/// A difference between the marks on the host and the desired state.
///
/// A tombstone without an entry, and a mark that names no valid entry, are both `MarkedWithoutEntry` with an empty `entry`.
/// The released schema's drift kinds are closed, and an empty entry is the one value that says the mark names no entry. The
/// raw mark value is never reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Drift {
    /// An object carries a kernel mark for an entry that is not live, or a mark or tombstone that names no valid entry.
    MarkedWithoutEntry {
        project: String,
        name: String,
        #[serde(with = "entry_or_empty")]
        entry: Option<EntryId>,
    },
    /// A live entry has no object carrying its mark. Only a complete watch, one that covers every project, reports it.
    EntryWithoutObject { entry: EntryId },
}

/// The whole report. `at` is the host clock's milliseconds at which the watch ran.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchReport {
    pub at: u64,
    pub projects: Vec<ProjectRows>,
    pub drift: Vec<Drift>,
}

/// The config hash of one instance: the lowercase hex SHA-256 of the canonical JSON of its own config, with every `volatile.*`
/// key removed. `own_config` must be the instance's `config`, never its `expanded_config`.
pub fn config_hash(own_config: &BTreeMap<String, String>) -> String {
    let object: serde_json::Map<String, Value> = own_config
        .iter()
        .filter(|(key, _)| !key.starts_with(VOLATILE_PREFIX))
        .map(|(key, value)| (key.clone(), Value::String(value.clone())))
        .collect();
    // A map of strings always has a canonical form, so the expect cannot fire.
    let text =
        canonical_json(&Value::Object(object)).expect("a map of strings has a canonical form");
    Digest::of(text.as_bytes()).to_hex()
}

/// An entry on the wire is its text, and an absent entry is the empty text.
mod entry_or_empty {
    use super::{Deserialize, Deserializer, EntryId, Serializer};

    pub fn serialize<S: Serializer>(
        entry: &Option<EntryId>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(entry.as_ref().map_or("", |entry| entry.0.as_str()))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<EntryId>, D::Error> {
        let text = String::deserialize(deserializer)?;
        if text.is_empty() {
            return Ok(None);
        }
        EntryId::parse(&text).map(Some).ok_or_else(|| {
            serde::de::Error::custom("an entry id must match ^[a-z][a-z0-9-]{0,62}$")
        })
    }
}
