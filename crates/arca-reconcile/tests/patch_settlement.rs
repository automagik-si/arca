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

fn lost_answer(apply: bool, read_fails: bool) {
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
    fake.drop_patch_answer(PATH, apply);
    let operation = fake.hold_operation(&ObjectRef {
        kind: IncusKind::Instance,
        project: PROJECT.into(),
        parent: None,
        name: "box".into(),
        uuid: None,
    });
    let first = doc(1, None, "first");
    let outcomes = std::thread::scope(|scope| {
        let release = scope.spawn(|| {
            let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !fake
                .requests()
                .iter()
                .any(|request| request.path.contains("/wait"))
            {
                assert!(
                    std::time::Instant::now() < until,
                    "settlement must wait for the operation"
                );
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            if read_fails {
                fake.fail_reads(PATH, 1, 500);
            }
            fake.release_operation(&operation);
        });
        let outcomes = reconciler.reconcile(&first, NOW);
        release.join().unwrap();
        outcomes
    });
    if read_fails {
        assert!(
            outcomes
                .iter()
                .any(|out| matches!(out, Outcome::Failed { .. }))
        );
        assert_eq!(reconciler.journal().unresolved().len(), 1);
        let blocked = doc(2, Some(first.document.digest()), "second");
        fake.fail_reads(PATH, 1, 500);
        let outcomes = reconciler.reconcile(&blocked, NOW + 1);
        assert!(
            outcomes.iter().any(
                |out| matches!(out, Outcome::Failed { why, .. } if why.contains("unresolved"))
            )
        );
        assert_eq!(patches(&fake), 1, "an open intent is never replayed");
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
        if apply {
            assert!(
                outcomes
                    .iter()
                    .all(|out| !matches!(out, Outcome::Failed { .. }))
            );
        } else {
            assert!(outcomes.iter().any(
                |out| matches!(out, Outcome::Failed { why, .. } if why.contains("did not land"))
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
    assert_eq!(
        first_outcome.state,
        if apply {
            EffectState::Applied
        } else {
            EffectState::Failed
        }
    );
    assert_eq!(live, if apply { 2 } else { 1 });
}

fn patches(fake: &FakeIncus) -> usize {
    fake.requests()
        .iter()
        .filter(|request| request.method == "PATCH")
        .count()
}

#[test]
fn a_lost_patch_answer_is_applied_from_read_back_and_the_next_write_works() {
    lost_answer(true, false);
}

#[test]
fn a_lost_patch_that_did_not_land_is_failed_and_the_next_write_works() {
    lost_answer(false, false);
}

#[test]
fn an_unreadable_patch_stays_blocked_then_a_later_write_settles_it_without_restart() {
    lost_answer(true, true);
}
