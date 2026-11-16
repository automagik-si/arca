//! Lost PATCH answers must settle from Incus and leave the running node usable.
mod common;

use std::collections::BTreeMap;

use arca_incus::FakeIncus;
use arca_kernel::desired::{DesiredStateGate, EntryId, PluginId};
use arca_kernel::journal::{EffectClass, EffectState, FileJournal, Journal, JournalStore};
use arca_kernel::ownership::{IncusKind, ObjectRef};
use arca_kernel::plugin::{Ctx, EffectOp, Manifest, Plugin, PluginError};
use arca_providers::incus::RetainedSet;
use arca_reconcile::{Outcome, PluginSet, Reconciler};
use common::*;
use serde::Deserialize;
use serde_json::json;

const PATH: &str = "/1.0/instances/box?project=arcak-scratch-toy";

#[derive(Deserialize)]
struct Config {
    value: String,
}

struct Patcher;

impl Plugin for Patcher {
    const ID: &'static str = "test.patcher";
    type Config = Config;

    fn manifest() -> Manifest {
        Manifest {
            id: PluginId(Self::ID.into()),
            version: "0.1.0".into(),
            provides: vec![],
            effects: vec![(IncusKind::Instance, EffectClass::B)],
            projects: vec![PROJECT.into()],
            observes: false,
        }
    }

    fn apply(ctx: &mut Ctx<'_>, cfg: &Config) -> Result<(), PluginError> {
        ctx.effect(EffectOp::IncusPatchConfig {
            object: ObjectRef {
                kind: IncusKind::Instance,
                project: PROJECT.into(),
                parent: None,
                name: "box".into(),
                uuid: None,
            },
            config: BTreeMap::from([("user.note".into(), Some(cfg.value.clone()))]),
        })?;
        Ok(())
    }
}

fn lost_answer(apply: bool, read_fails: bool, late: bool) {
    let scratch = Scratch::new("patch-settle");
    let mut state = fake_state();
    state.instances.push(json!({"name":"box", "project":PROJECT, "config":{
        MARK:"patch", "volatile.uuid":"00000000-0000-4000-8000-000000000001", "user.note":"before"
    }}));
    let fake = FakeIncus::start(&scratch.dir, state).unwrap();
    let retained = RetainedSet::empty();
    let key = node_key();
    let mut plugins = PluginSet::new();
    plugins.add::<Patcher>().unwrap();
    let journal_path = scratch.path("journal.jsonl");
    let mut reconciler = Reconciler::open(
        DesiredStateGate::new(key.public(), None),
        Journal::open(FileJournal::open(&journal_path).unwrap()).unwrap(),
        plugins,
        Box::new(provider(&scratch, &fake, &retained)),
        retained,
        NOW,
    )
    .unwrap();
    let doc = |seq, prev, value| {
        signed(
            &key,
            seq,
            prev,
            vec![entry("patch", Patcher::ID, json!({"value":value}))],
        )
    };
    if late {
        fake.patch_after_next_get(PATH);
    } else {
        fake.drop_patch_answer(PATH, apply);
    }
    fake.set_operations_unreadable(read_fails);
    let first = doc(1, None, "first");
    let outcomes = reconciler.reconcile(&first, NOW);
    if read_fails {
        assert!(
            outcomes
                .iter()
                .any(|out| matches!(out, Outcome::Failed { .. }))
        );
        assert_eq!(reconciler.journal().unresolved().len(), 1);
        let blocked = doc(2, Some(first.document.digest()), "second");
        let outcomes = reconciler.reconcile(&blocked, NOW + 1);
        assert!(
            outcomes.iter().any(
                |out| matches!(out, Outcome::Failed { why, .. } if why.contains("unresolved"))
            )
        );
        assert_eq!(patches(&fake), 1, "an open intent is never replayed");
        fake.set_operations_unreadable(false);
        let next = doc(3, Some(blocked.document.digest()), "third");
        assert!(
            reconciler
                .reconcile(&next, NOW + 2)
                .iter()
                .all(|out| !matches!(out, Outcome::Failed { .. }))
        );
    } else {
        assert!(
            reconciler.journal().unresolved().is_empty(),
            "the lost answer must settle: {outcomes:?}"
        );
        if apply && !late {
            assert!(
                outcomes
                    .iter()
                    .all(|out| !matches!(out, Outcome::Failed { .. }))
            );
        } else {
            assert!(outcomes.iter().any(
                |out| matches!(out, Outcome::Failed { why, .. } if why.contains("may have landed"))
            ));
        }
        let next = doc(2, Some(first.document.digest()), "second");
        assert!(
            reconciler
                .reconcile(&next, NOW + 1)
                .iter()
                .all(|out| !matches!(out, Outcome::Failed { .. }))
        );
    }
    assert!(reconciler.journal().unresolved().is_empty());
    assert_eq!(
        patches(&fake),
        2,
        "one PATCH per intent; no replay during settlement"
    );
    let live = reconciler.journal().live(&EntryId("patch".into())).len();
    drop(reconciler);
    let (records, _) = FileJournal::open(&journal_path).unwrap().load().unwrap();
    let first_outcome = records
        .iter()
        .find(|record| record.id == 1 && record.state != EffectState::Intent)
        .unwrap();
    assert_eq!(first_outcome.state, EffectState::Applied);
    assert_eq!(live, 2);
    let Some(arca_kernel::journal::Inverse::IncusRestoreConfig { config, .. }) =
        &first_outcome.inverse
    else {
        panic!("original inverse must be preserved")
    };
    assert_eq!(config.get("user.note"), Some(&Some("before".into())));
}

fn patches(fake: &FakeIncus) -> usize {
    fake.requests()
        .iter()
        .filter(|request| request.method == "PATCH")
        .count()
}

#[test]
fn a_lost_patch_answer_is_applied_from_read_back_and_the_next_write_works() {
    lost_answer(true, false, false);
}

#[test]
fn an_uncertain_pre_image_keeps_its_original_inverse_and_the_next_write_works() {
    lost_answer(false, false, false);
}

#[test]
fn an_unreadable_patch_stays_blocked_then_a_later_write_settles_it_without_restart() {
    lost_answer(true, true, false);
}

#[test]
fn a_sync_patch_landing_after_the_first_get_keeps_the_original_pre_image() {
    lost_answer(false, false, true);
}

#[test]
fn runtime_settlement_of_an_uncertain_patch_pre_image_keeps_its_inverse() {
    lost_answer(false, true, false);
}
