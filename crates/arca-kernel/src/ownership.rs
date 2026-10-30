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

/// An Incus object's address. A project's `project` and `name` are both its own name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectRef {
    pub kind: IncusKind,
    pub project: String,
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
