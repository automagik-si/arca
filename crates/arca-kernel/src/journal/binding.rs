//! An atomic change of the addresses an entry's existing effects follow. This
//! records ownership already checked by the caller; it grants no provider write.
use std::collections::{BTreeMap, BTreeSet};

use super::{EffectRecord, EffectState, Inverse, Journal, JournalStore, closed};
use crate::{
    KernelError,
    desired::EntryId,
    ownership::{IncusKind, ObjectRef},
};
use serde::{Deserialize, Serialize};

/// The instance and storage a binding follows. The first source root pool must
/// be checked against the service record by the caller: effects do not store it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BindingState {
    #[serde(deserialize_with = "closed::object")]
    pub instance: ObjectRef,
    pub root_pool: String,
    #[serde(default, deserialize_with = "closed::optional")]
    pub data_volume: Option<ObjectRef>,
}

/// A single durable transfer of an entry's live effect lineage. The source must
/// still be current. No create, undo, host write or deletion is implied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BindingEvent {
    pub entry: EntryId,
    pub from: BindingState,
    pub to: BindingState,
    pub at_ms: u64,
}

/// The closed tag of a binding line, distinct from every legacy effect line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BindingKind {
    #[serde(rename = "binding")]
    Binding,
}

/// A journal line. Legacy effect records keep their original wire shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum JournalRecord {
    Effect(EffectRecord),
    Binding {
        kind: BindingKind,
        binding: BindingEvent,
    },
}

impl<S: JournalStore> Journal<S> {
    /// Appends and syncs one binding before changing the effective view. An
    /// uncertain append poisons the journal until reopen, as an effect does.
    /// Only a caller that has checked ownership and approval may invoke this;
    /// the plugin context deliberately exposes no binding operation.
    pub fn bind(&mut self, event: BindingEvent) -> Result<(), KernelError> {
        if let Some(error) = &self.poison {
            return Err(error.clone());
        }
        let (create, ids) = validate(&self.latest, &self.bindings, &event)?;
        if let Err(error) = self.store.append_binding(&event) {
            self.poison = Some(error.clone());
            return Err(error);
        }
        for id in ids {
            translate(self.latest.get_mut(&id).expect("validated effect"), &event);
        }
        self.bindings.insert(create, event);
        Ok(())
    }

    /// Current storage facts, replayed on open before recovery or reconciliation.
    /// A removed create's facts do not apply to a later incarnation of its entry.
    pub fn bindings(&self) -> impl Iterator<Item = &BindingEvent> {
        self.bindings.iter().filter_map(|(id, event)| {
            self.latest
                .get(id)
                .filter(|record| live(record))
                .map(|_| event)
        })
    }
}

fn live(record: &EffectRecord) -> bool {
    !matches!(record.state, EffectState::Reverted | EffectState::Failed)
}

fn refused() -> KernelError {
    KernelError::Journal("binding does not name the current live objects".into())
}

fn same_place(a: &ObjectRef, b: &ObjectRef) -> bool {
    a.kind == b.kind && a.project == b.project && a.parent == b.parent && a.name == b.name
}

fn volume_ok(volume: &ObjectRef, project: &str) -> bool {
    volume.kind == IncusKind::Volume
        && volume.project == project
        && !volume.name.is_empty()
        && volume.parent.as_ref().is_some_and(|pool| !pool.is_empty())
        && volume.uuid.is_none()
}

pub(super) fn validate(
    latest: &BTreeMap<u64, EffectRecord>,
    bindings: &BTreeMap<u64, BindingEvent>,
    event: &BindingEvent,
) -> Result<(u64, BTreeSet<u64>), KernelError> {
    let (from, to) = (&event.from, &event.to);
    if EntryId::parse(&event.entry.0).is_none()
        || from.instance.kind != IncusKind::Instance
        || from.instance.parent.is_some()
        || !same_place(&from.instance, &to.instance)
        || from.instance.project.is_empty()
        || from.instance.name.is_empty()
        || !from
            .instance
            .uuid
            .as_ref()
            .is_some_and(|uuid| !uuid.is_empty())
        || !to
            .instance
            .uuid
            .as_ref()
            .is_some_and(|uuid| !uuid.is_empty())
        || from.instance.uuid == to.instance.uuid
        || from.root_pool.is_empty()
        || to.root_pool.is_empty()
        || latest.values().any(|r| r.state == EffectState::Intent)
    {
        return Err(refused());
    }
    match (&from.data_volume, &to.data_volume) {
        (None, None) => {}
        (Some(a), Some(b))
            if volume_ok(a, &from.instance.project) && volume_ok(b, &to.instance.project) => {}
        _ => return Err(refused()),
    }
    let records: Vec<_> = latest
        .values()
        .filter(|r| r.entry == event.entry && live(r))
        .collect();
    if records.iter().any(|r| r.intent.is_some()) {
        return Err(refused());
    }
    let creates: Vec<_> = records
        .iter()
        .filter_map(|r| match &r.inverse {
            Some(Inverse::IncusDelete { object }) => Some((r.id, r.class, object)),
            _ => None,
        })
        .collect();
    let instances: Vec<_> = creates
        .iter()
        .filter(|(_, _, o)| o.kind == IncusKind::Instance)
        .collect();
    let [(create, super::EffectClass::B, object)] = instances.as_slice() else {
        return Err(refused());
    };
    if **object != from.instance
        || bindings
            .get(create)
            .is_some_and(|previous| previous.to != *from)
    {
        return Err(refused());
    }
    let volumes: Vec<_> = creates
        .iter()
        .filter(|(_, _, o)| o.kind == IncusKind::Volume)
        .collect();
    match (from.data_volume.as_ref(), volumes.as_slice()) {
        (None, []) => {}
        (Some(expected), [(_, super::EffectClass::B, object)]) if **object == *expected => {}
        _ => return Err(refused()),
    }
    for record in &records {
        for object in std::iter::once(&record.target)
            .chain(record.object.iter())
            .chain(inverse_object(&record.inverse))
        {
            if same_place(object, &from.instance)
                && object.uuid.is_some()
                && object.uuid != from.instance.uuid
            {
                return Err(refused());
            }
        }
    }
    // Another entry must neither own the destination nor keep undo on the source volume.
    if latest
        .values()
        .filter(|r| r.entry != event.entry && live(r))
        .any(|r| {
            std::iter::once(&r.target)
                .chain(r.object.iter())
                .chain(inverse_object(&r.inverse))
                .any(|object| {
                    same_place(object, &to.instance)
                        || (object.kind == IncusKind::Instance && object.uuid == to.instance.uuid)
                        || to
                            .data_volume
                            .as_ref()
                            .is_some_and(|volume| same_place(object, volume))
                        || from
                            .data_volume
                            .as_ref()
                            .is_some_and(|volume| same_place(object, volume))
                })
        })
    {
        return Err(refused());
    }
    Ok((*create, records.iter().map(|r| r.id).collect()))
}

fn inverse_object(inverse: &Option<Inverse>) -> Option<&ObjectRef> {
    match inverse {
        Some(
            Inverse::IncusDelete { object }
            | Inverse::IncusRestoreConfig { object, .. }
            | Inverse::IncusRestoreAclField { object, .. },
        ) => Some(object),
        _ => None,
    }
}

/// Translate both UUID-addressed and by-name inverses. A by-name instance patch
/// gains the replacement UUID, so undo can never land on a later namesake.
pub(super) fn translate(record: &mut EffectRecord, event: &BindingEvent) {
    let address = |object: &mut ObjectRef| {
        for (from, to) in std::iter::once((&event.from.instance, &event.to.instance)).chain(
            event
                .from
                .data_volume
                .iter()
                .zip(event.to.data_volume.iter()),
        ) {
            if same_place(object, from) && (object.uuid.is_none() || object.uuid == from.uuid) {
                *object = to.clone();
                break;
            }
        }
    };
    address(&mut record.target);
    if let Some(object) = &mut record.object {
        address(object);
    }
    if let Some(
        Inverse::IncusDelete { object }
        | Inverse::IncusRestoreConfig { object, .. }
        | Inverse::IncusRestoreAclField { object, .. },
    ) = &mut record.inverse
    {
        address(object);
    }
}
