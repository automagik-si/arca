//! The ownership model. Each Incus object's owner is read from the object's own `config`,
//! never from `expanded_config`, so a profile cannot change who owns an object.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::canon::Digest;
use crate::desired::{EntryId, PluginId};

/// The kernel's entry mark. Its value is the id of the entry that owns the object.
pub const KERNEL_ENTRY_KEY: &str = "user.arcakernel.entry";
/// The kernel's retiring tombstone, set by a destructive inverse before its delete. It only accompanies an entry mark.
pub const KERNEL_RETIRING_KEY: &str = "user.arcakernel.retiring";
/// Droidd's mark namespaces. Any key under one of them means droidd owns the object. A bare namespace key (for example
/// `user.arca`) counts too, and matching ignores ASCII case, so a near-miss can only make the kernel refuse.
pub const DROIDD_MARK_PREFIXES: [&str; 3] = ["user.droid.", "user.tactu.", "user.arca."];

/// The Incus object kinds the kernel knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IncusKind {
    Project,
    Instance,
    Network,
    Acl,
    Profile,
    Volume,
    Zone,
    Forward,
    Bucket,
}

impl IncusKind {
    /// Whether Incus gives an object of this kind a uuid: only an instance, through its `volatile.uuid` config key. Incus 7.3
    /// gives a custom volume none. Every other kind is identified by its kind, project, parent and name.
    pub fn has_uuid(self) -> bool {
        matches!(self, IncusKind::Instance)
    }
}

/// An Incus object's address. A project's `project` and `name` are both its own name. `parent` is the storage pool of a
/// volume or a bucket and the network of a forward; every other kind has none, and its JSON omits the key, so the digests of
/// those objects do not change. `uuid` is set only for a kind that has one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectRef {
    pub kind: IncusKind,
    pub project: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub name: String,
    pub uuid: Option<String>,
}

/// Who owns an object. `Contested` is marked by both droidd and the kernel; nobody writes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "class", rename_all = "lowercase")]
pub enum OwnerClass {
    Droidd,
    Foreign,
    Kernel {
        entry: EntryId,
    },
    Retained {
        entry: EntryId,
        plugin: PluginId,
        plan: Digest,
    },
    Contested,
}

/// Classes an object from its own config and the retained entries. `own_config` must be the object's `config`, not its
/// `expanded_config`.
pub fn classify(
    own_config: &BTreeMap<String, String>,
    retained: &BTreeMap<EntryId, (PluginId, Digest)>,
) -> OwnerClass {
    // Droidd's marks and the kernel's are checked together: an object carrying both sides is contested, and nobody writes it.
    let droidd = own_config.keys().any(|key| is_droidd_key(key));
    let entry_value = own_config.get(KERNEL_ENTRY_KEY);
    let kernel_side = entry_value.is_some() || own_config.contains_key(KERNEL_RETIRING_KEY);
    if droidd && kernel_side {
        return OwnerClass::Contested;
    }
    if droidd {
        return OwnerClass::Droidd;
    }
    // A kernel mark must name a valid entry. A tombstone without one names no owner, so the object is foreign.
    let Some(entry) = entry_value.and_then(|value| EntryId::parse(value)) else {
        return OwnerClass::Foreign;
    };
    match retained.get(&entry) {
        Some((plugin, plan)) => OwnerClass::Retained {
            entry,
            plugin: plugin.clone(),
            plan: plan.clone(),
        },
        None => OwnerClass::Kernel { entry },
    }
}

/// Class an addressed kernel service's public metadata without changing adoption's classifier.
/// Only exact service object names and exact metadata keys accompany the kernel entry mark. A tenant's service is
/// classed the same way, in its own tenant's project and nowhere else.
/// Every other owner key still contests the object, and retained entries remain retained.
pub fn classify_object(
    object: &ObjectRef,
    own_config: &BTreeMap<String, String>,
    retained: &BTreeMap<EntryId, (PluginId, Digest)>,
) -> OwnerClass {
    // The service an entry mark names, with its projects: `service-<name>` in the node's service project, or
    // `t-<id>-service-<name>` in tenant `id`'s project under either tenant prefix.
    let mark = own_config.get(KERNEL_ENTRY_KEY).map(String::as_str);
    // A tenant's instance is named `<name>-<id>`: one bridge holds one instance of a name.
    let service: Option<(&str, String, Vec<String>)> = mark.and_then(|entry| {
        if let Some(name) = entry.strip_prefix("service-") {
            return Some((name, name.to_string(), vec!["arcak-svc".to_string()]));
        }
        let (id, name) = entry.strip_prefix("t-")?.split_once("-service-")?;
        tenant_id_ok(id).then(|| {
            (
                name,
                format!("{name}-{id}"),
                vec![tenant_project(id, false), tenant_project(id, true)],
            )
        })
    });
    let Some((name, instance, projects)) = service.filter(|(name, _, _)| {
        !name.is_empty() && EntryId::parse(&format!("service-{name}")).is_some()
    }) else {
        return classify(own_config, retained);
    };
    let keys: &[&str] = match object.kind {
        IncusKind::Instance
            if projects.contains(&object.project)
                && object.name == instance
                && object.parent.is_none() =>
        {
            // A function's instance carries one more: the mark the writer of a run looks for before it starts or
            // stops one. A function lives in the node's own service project and nowhere else, so the key is the
            // entry's own there alone: on a tenant's instance it is another owner's key, as it always was. So is
            // the mark of the `raw.lxc` a template's `rawLxc` lines gave, which only a service there carries.
            if object.project == "arcak-svc" {
                &[
                    "user.arca.template",
                    "user.arca.address",
                    "user.arca.ports",
                    "user.arca.links",
                    "user.arca.published",
                    "user.arca.envkeys",
                    "user.arca.function",
                    "user.arca.rawlxc",
                ]
            } else {
                &[
                    "user.arca.template",
                    "user.arca.address",
                    "user.arca.ports",
                    "user.arca.links",
                    "user.arca.published",
                    "user.arca.envkeys",
                ]
            }
        }
        IncusKind::Acl
            if object.project == "default"
                && projects
                    .iter()
                    .any(|project| object.name == format!("{project}-svc-{name}"))
                && object.parent.is_none() =>
        {
            &["user.arca.managed"]
        }
        _ => &[],
    };
    let ownership = own_config
        .iter()
        .filter(|(key, _)| !keys.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    classify(&ownership, retained)
}

fn is_droidd_key(key: &str) -> bool {
    DROIDD_MARK_PREFIXES.iter().any(|prefix| {
        let bare = prefix.trim_end_matches('.');
        key.eq_ignore_ascii_case(bare)
            || key
                .get(..prefix.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
    })
}

/// Whether the kernel may write an object of `class` for the entry `entry`. True only for `Kernel` with that same entry.
pub fn kernel_may_write(class: &OwnerClass, entry: &EntryId) -> bool {
    matches!(class, OwnerClass::Kernel { entry: owner } if owner == entry)
}

/// The prefix of a tenant's project: `arcak-t-<id>`. In a scratch run it is [`SCRATCH_TENANT_PROJECT_PREFIX`].
pub const TENANT_PROJECT_PREFIX: &str = "arcak-t-";
/// The prefix of a tenant's project in a scratch run: `arcak-scratch-t-<id>`.
pub const SCRATCH_TENANT_PROJECT_PREFIX: &str = "arcak-scratch-t-";
/// The prefix of a tenant's entry: `tenant-<id>`.
pub const TENANT_ENTRY_PREFIX: &str = "tenant-";

/// Whether `id` is a tenant id: 8 lowercase base32 characters (`a` to `z`, `2` to `7`). It is random, never a name.
pub fn tenant_id_ok(id: &str) -> bool {
    id.len() == 8
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b))
}

/// The id of the entry that owns tenant `id`.
pub fn tenant_entry(id: &str) -> String {
    format!("{TENANT_ENTRY_PREFIX}{id}")
}

/// The project of tenant `id`, under the scratch prefix in a scratch run.
pub fn tenant_project(id: &str, scratch: bool) -> String {
    let prefix = if scratch {
        SCRATCH_TENANT_PROJECT_PREFIX
    } else {
        TENANT_PROJECT_PREFIX
    };
    format!("{prefix}{id}")
}

/// The tenant id an entry id names, when the entry is a tenant's: `tenant-<id>` with a valid id.
pub fn tenant_of_entry(entry: &EntryId) -> Option<&str> {
    entry
        .0
        .strip_prefix(TENANT_ENTRY_PREFIX)
        .filter(|id| tenant_id_ok(id))
}

/// The tenant id a project name carries, when the name is a tenant project's under either prefix.
pub fn tenant_of_project(project: &str) -> Option<&str> {
    project
        .strip_prefix(SCRATCH_TENANT_PROJECT_PREFIX)
        .or_else(|| project.strip_prefix(TENANT_PROJECT_PREFIX))
        .filter(|id| tenant_id_ok(id))
}

/// Whether a manifest's project list names `project`: by its exact name, or, for a tenant's project, by a tenant prefix
/// followed by `*`. No other pattern exists. Naming a tenant project here admits nothing by itself: the provider writes
/// only in the project of a tenant whose entry exists.
pub fn manifest_names_project(projects: &[String], project: &str) -> bool {
    projects.iter().any(|named| {
        named == project
            || [TENANT_PROJECT_PREFIX, SCRATCH_TENANT_PROJECT_PREFIX]
                .into_iter()
                .any(|prefix| {
                    named.strip_suffix('*') == Some(prefix)
                        && project.strip_prefix(prefix).is_some_and(tenant_id_ok)
                })
    })
}
