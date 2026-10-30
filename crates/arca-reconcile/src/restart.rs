//! Settling the journal after a restart. A killed process leaves intents with no outcome. Before any effect runs, each one is read
//! back with a GET and settled from what the host shows, so the journal is a legal history again and the removal that follows sees
//! what landed.
//!
//! The rules, by the kind of intent:
//!
//! - A create. An object the entry owns at the intent's place is adopted, with the delete that undoes it. Nothing there means the
//!   create never landed, and the intent is reverted. Any other object is not the entry's, so the intent is failed and reported.
//! - A patch. Its entry is held, so nothing is written for it until a document is accepted again. An object that shows the pre-image
//!   did not change, and the intent is failed. Any other value means the patch may have landed, so the intent is settled with the
//!   pre-image as its inverse, which is what a later removal restores.
//! - A delete (an intent marked [`IntentKind::Delete`]). An object still there is kept, and the intent is failed, so the next removal
//!   retries it. An absent object means the delete landed, so the intent and the create it undid are reverted.
//!
//! Nothing here sends a write. A read that cannot be answered leaves its intent open, and [`Reconciler::open`] then refuses.

use std::collections::BTreeMap;

use arca_kernel::KernelError;
use arca_kernel::desired::EntryId;
use arca_kernel::journal::{EffectRecord, IntentKind, Inverse, JournalSink, JournalStore};
use arca_kernel::ownership::ObjectRef;
use arca_kernel::plugin::ReadBack;

use crate::Reconciler;

/// Why a create's intent is failed when the name holds an object the entry does not own.
const NAME_HELD: &str = "the name holds an object the entry does not own; the create did not land";
/// Why a delete or a patch is failed when its object is not the one the entry owns.
const NOT_THE_ENTRYS: &str =
    "the object at the intent's place is not the one the entry owns; nothing is sent";
/// Why a delete is failed when its object is still there.
const KEPT: &str = "the object is still there after a restart; its delete did not land";
/// Why a patch is failed when its object is gone.
const GONE: &str = "the object the patch names is gone";
/// Why a patch is failed when the object shows its pre-image.
const NOT_LANDED: &str = "the object shows its pre-image; the patch did not land";

/// What one resolution settled. Every list is in the order the intents were settled, which is the order they were journalled.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolution {
    /// Creates found by their mark, now recorded as applied, with the delete that undoes each.
    pub adopted: Vec<ObjectRef>,
    /// The ids of intents settled as reverted: nothing of theirs was left.
    pub reverted: Vec<u64>,
    /// The entries whose patches were unresolved. Nothing is written for them until a document is accepted again.
    pub held: Vec<EntryId>,
    /// The objects whose live state differs from what their intent left, so a person can see it.
    pub drift: Vec<ObjectRef>,
    /// The clock the resolution ran at, in milliseconds.
    pub at_ms: u64,
}

/// Whether an intent was settled, or must stay open.
enum Step {
    Settled,
    Open,
}

impl<S: JournalStore> Reconciler<S> {
    /// Settles every intent a restart left open, before any effect runs. Each one is read back and settled by the rules in the
    /// module's documentation. The resolution is kept for [`Reconciler::resolution`].
    ///
    /// Returns [`KernelError::Unresolved`] with the ids of the intents it could not settle, because a read could not be answered.
    /// Those intents stay open, so the reconciler does not open. Calling it again settles nothing that is already settled.
    pub fn resolve(&mut self, now_ms: u64) -> Result<Resolution, KernelError> {
        let mut resolution = Resolution {
            at_ms: now_ms,
            ..Resolution::default()
        };
        let mut open = Vec::new();
        for record in self.journal.unresolved() {
            if let Step::Open = self.settle(&record, &mut resolution)? {
                open.push(record.id);
            }
        }
        self.check_journal()?;
        if !open.is_empty() {
            return Err(KernelError::Unresolved { ids: open });
        }
        resolution.held.sort();
        resolution.held.dedup();
        for entry in &resolution.held {
            // A held entry has not converged, so the next accepted document applies it again.
            self.converged.remove(entry);
        }
        self.resolution = resolution.clone();
        Ok(resolution)
    }

    /// Rebuilds which plugin each entry belongs to, from the plugin each live effect was journalled with. It runs once, when the
    /// reconciler opens, before any removal can need a plugin.
    pub(crate) fn rebuild_applied_by(&mut self) {
        for entry in self.journal.live_entries() {
            let plugin = self
                .journal
                .live(&entry)
                .into_iter()
                .rev()
                .find_map(|record| record.plugin);
            if let Some(plugin) = plugin {
                self.applied_by.insert(entry, plugin);
            }
        }
    }

    /// Settles one unresolved intent.
    fn settle(&mut self, record: &EffectRecord, out: &mut Resolution) -> Result<Step, KernelError> {
        if record.intent == Some(IntentKind::Delete) {
            return self.settle_delete(record, out);
        }
        match record.pre_image.clone() {
            Some(pre_image) => self.settle_patch(record, &pre_image, out),
            None => self.settle_create(record, out),
        }
    }

    /// The object an intent names, read back. `None` when the read cannot be answered, so the intent stays open.
    fn read(
        &self,
        record: &EffectRecord,
        pre_image: Option<&BTreeMap<String, Option<String>>>,
    ) -> Option<ReadBack> {
        self.provider
            .read_back(&record.entry, &record.target, pre_image)
            .ok()
    }

    fn settle_create(
        &mut self,
        record: &EffectRecord,
        out: &mut Resolution,
    ) -> Result<Step, KernelError> {
        let Some(back) = self.read(record, None) else {
            return Ok(Step::Open);
        };
        match back {
            ReadBack::Absent => {
                self.journal.reverted(record.id);
                out.reverted.push(record.id);
            }
            ReadBack::Present {
                object,
                owned: true,
                ..
            } if object.kind.has_uuid() == object.uuid.is_some() => {
                let inverse = Inverse::IncusDelete {
                    object: object.clone(),
                };
                self.journal.applied(record.id, object.clone(), inverse)?;
                out.adopted.push(object);
            }
            // Owned, but the uuid does not fit the kind, so no delete could name it. Only a person can settle that.
            ReadBack::Present { owned: true, .. } => return Ok(Step::Open),
            ReadBack::Present { object, .. } => {
                self.journal.failed(record.id, NAME_HELD)?;
                out.drift.push(object);
            }
        }
        self.check_journal()?;
        Ok(Step::Settled)
    }

    fn settle_patch(
        &mut self,
        record: &EffectRecord,
        pre_image: &BTreeMap<String, Option<String>>,
        out: &mut Resolution,
    ) -> Result<Step, KernelError> {
        let Some(back) = self.read(record, Some(pre_image)) else {
            return Ok(Step::Open);
        };
        out.held.push(record.entry.clone());
        match back {
            ReadBack::Absent => {
                self.journal.failed(record.id, GONE)?;
                out.drift.push(record.target.clone());
            }
            ReadBack::Present {
                object,
                owned,
                holds_pre_image,
            } => {
                let same = record.target.uuid.is_none() || record.target.uuid == object.uuid;
                if !owned || !same {
                    self.journal.failed(record.id, NOT_THE_ENTRYS)?;
                    out.drift.push(object);
                } else if holds_pre_image {
                    self.journal.failed(record.id, NOT_LANDED)?;
                } else {
                    // The patch may have landed. Its inverse is the pre-image, the values it replaced, so the removal restores them.
                    let inverse = Inverse::IncusRestoreConfig {
                        object: object.clone(),
                        config: pre_image.clone(),
                    };
                    self.journal.applied(record.id, object.clone(), inverse)?;
                    out.drift.push(object);
                }
            }
        }
        self.check_journal()?;
        Ok(Step::Settled)
    }

    fn settle_delete(
        &mut self,
        record: &EffectRecord,
        out: &mut Resolution,
    ) -> Result<Step, KernelError> {
        let Some(back) = self.read(record, None) else {
            return Ok(Step::Open);
        };
        match back {
            ReadBack::Absent => {
                // The delete landed. Its intent is done, and so is the create it undid.
                self.journal.reverted(record.id);
                self.settle_deleted(&record.entry, &record.target);
                out.reverted.push(record.id);
            }
            ReadBack::Present { object, owned, .. } => {
                let same = record.target.uuid.is_none() || record.target.uuid == object.uuid;
                if owned && same {
                    // The object is still the entry's, so its delete did not land. The entry's live effects stay, and the next
                    // removal retries.
                    self.journal.failed(record.id, KEPT)?;
                } else {
                    self.journal.failed(record.id, NOT_THE_ENTRYS)?;
                    out.drift.push(object);
                }
            }
        }
        self.check_journal()?;
        Ok(Step::Settled)
    }

    /// The journal's own refusal, if it has stopped, since the void settlements cannot return one.
    fn check_journal(&self) -> Result<(), KernelError> {
        match self.journal.poisoned() {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }
}
