//! A plugin that swallows a refusal cannot write past an effect whose outcome is not known. The journal refuses the next intent while
//! the first one has no outcome, so the provider is not called again and the apply is not applied.

mod common;

use std::sync::{Arc, Mutex};

use arca_kernel::desired::{DesiredStateGate, EntryId, PluginId};
use arca_kernel::journal::{EffectClass, Inverse, Journal, MemJournal};
use arca_kernel::ownership::{IncusKind, ObjectRef};
use arca_kernel::plugin::{
    Ctx, EffectOp, EffectRefusal, Manifest, Observe, Planned, Plugin, PluginError, Provider,
    op_digest,
};
use arca_providers::incus::RetainedSet;
use arca_reconcile::{Outcome, PluginSet, Reconciler};
use common::*;
use serde_json::{Value, json};

const PERSIST: &str = "unit.persist";

/// A provider that cannot vouch for its first write, and would apply every later one.
struct Unsure {
    calls: Arc<Mutex<u32>>,
}

impl Provider for Unsure {
    fn observe(
        &self,
        _entry: &EntryId,
        _projects: &[String],
        _query: &Observe,
    ) -> Result<Value, EffectRefusal> {
        Err(EffectRefusal::NotDeclared)
    }

    fn plan(&self, _entry: &EntryId, op: &EffectOp) -> Result<Planned, EffectRefusal> {
        Ok(Planned {
            acl_patch: None,
            class: EffectClass::A,
            op_digest: op_digest(op),
            pre_image: None,
            etag: None,
        })
    }

    fn effect(
        &mut self,
        _entry: &EntryId,
        op: &EffectOp,
        _planned: &Planned,
    ) -> Result<(ObjectRef, Inverse), EffectRefusal> {
        let mut calls = self.calls.lock().expect("the count is readable");
        *calls += 1;
        if *calls == 1 {
            return Err(EffectRefusal::Unresolved);
        }
        let EffectOp::IncusCreate {
            kind,
            project,
            name,
            ..
        } = op
        else {
            return Err(EffectRefusal::NotDeclared);
        };
        let object = ObjectRef {
            kind: *kind,
            project: project.clone(),
            parent: None,
            name: name.clone(),
            uuid: None,
        };
        Ok((object.clone(), Inverse::IncusDelete { object }))
    }
}

/// A plugin that creates two ACLs and carries on when the first create is refused, as a plugin that logs an error would.
struct Persist;

impl Plugin for Persist {
    const ID: &'static str = PERSIST;
    type Config = Value;

    fn manifest() -> Manifest {
        Manifest {
            id: PluginId(PERSIST.to_string()),
            version: "0.1.0".to_string(),
            provides: Vec::new(),
            effects: vec![(IncusKind::Acl, EffectClass::A)],
            projects: vec![PROJECT.to_string()],
            observes: false,
        }
    }

    fn apply(ctx: &mut Ctx<'_>, _cfg: &Value) -> Result<(), PluginError> {
        for name in ["acl-1", "acl-2"] {
            let _ = ctx.effect(EffectOp::IncusCreate {
                kind: IncusKind::Acl,
                project: PROJECT.to_string(),
                parent: None,
                name: name.to_string(),
                body: json!({ "name": name, "config": {} }),
            });
        }
        Ok(())
    }
}

#[test]
fn a_plugin_that_swallows_an_unresolved_effect_cannot_make_a_second_write() {
    let key = node_key();
    let calls = Arc::new(Mutex::new(0u32));
    let mut plugins = PluginSet::new();
    plugins
        .add::<Persist>()
        .expect("the persist plugin is valid");
    let journal = Journal::open(MemJournal::new()).expect("an empty journal opens");
    let mut r = Reconciler::open(
        DesiredStateGate::new(key.public(), None),
        journal,
        plugins,
        Box::new(Unsure {
            calls: calls.clone(),
        }),
        RetainedSet::empty(),
        NOW,
    )
    .expect("an empty journal opens the reconciler");
    let entry = entry("p1", PERSIST, json!({}));

    let outcomes = r.reconcile(&signed(&key, 1, None, vec![entry]), NOW);

    assert_eq!(
        *calls.lock().expect("the count is readable"),
        1,
        "the provider is called once: the second intent is refused before any write"
    );
    assert!(
        !outcomes
            .iter()
            .any(|outcome| matches!(outcome, Outcome::Applied { .. })),
        "an apply that wrote past an unknown outcome is not applied: {outcomes:?}"
    );
    assert_eq!(
        r.journal().unresolved().len(),
        1,
        "the first intent stays open"
    );
    assert_eq!(
        r.journal().live(&EntryId("p1".to_string())).len(),
        1,
        "the refused intent is not on the journal"
    );
}
