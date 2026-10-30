//! The effect journal: the closed inverse vocabulary, the intent-then-outcome fold, the removal partition, and the file
//! format's crash and damage boundaries.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use arca_kernel::KernelError;
use arca_kernel::canon::Digest;
use arca_kernel::desired::EntryId;
use arca_kernel::journal::{
    EffectClass, EffectRecord, EffectState, FileJournal, Inverse, Journal, JournalSink,
    JournalStore, MemJournal, removal,
};
use arca_kernel::ownership::{IncusKind, ObjectRef};
use serde_json::json;

/// A private directory under the system temp directory, unique per process and per call, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "arca-kernel-journal-{tag}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("the scratch directory is created");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("scratch mode");
        Scratch(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn entry(id: &str) -> EntryId {
    EntryId(id.to_string())
}

fn digest(tag: &str) -> Digest {
    Digest::of(tag.as_bytes())
}

fn instance(project: &str, name: &str, uuid: Option<&str>) -> ObjectRef {
    ObjectRef {
        kind: IncusKind::Instance,
        project: project.to_string(),
        name: name.to_string(),
        uuid: uuid.map(str::to_string),
    }
}

fn restore(object: &ObjectRef, pairs: &[(&str, Option<&str>)]) -> Inverse {
    Inverse::IncusRestoreConfig {
        object: object.clone(),
        config: pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.map(str::to_string)))
            .collect(),
    }
}

/// The object an intent names in tests that do not care which one it is.
fn target() -> ObjectRef {
    instance("p", "n", None)
}

/// Records one whole effect through the sink: intent, then applied. A restore's intent carries its config as the pre-image,
/// as a plan would, and any other effect carries none.
fn apply_one(
    journal: &mut impl JournalSink,
    entry_id: &str,
    class: EffectClass,
    object: ObjectRef,
    inverse: Inverse,
) -> u64 {
    let pre_image = match &inverse {
        Inverse::IncusRestoreConfig { config, .. } => Some(config.clone()),
        _ => None,
    };
    let id = journal
        .intend(
            &entry(entry_id),
            class,
            digest(entry_id),
            object.clone(),
            pre_image,
            1_000,
        )
        .expect("the intent is journalled");
    journal
        .applied(id, object, inverse)
        .expect("the outcome is journalled");
    id
}

fn record(
    id: u64,
    state: EffectState,
    object: Option<ObjectRef>,
    inverse: Option<Inverse>,
) -> EffectRecord {
    EffectRecord {
        id,
        entry: entry("watch"),
        class: EffectClass::A,
        op_digest: digest("watch"),
        target: instance("p", "n", None),
        pre_image: None,
        object,
        inverse,
        state,
        at_ms: 1_000,
    }
}

// The closed inverse vocabulary.

#[test]
fn every_inverse_variant_round_trips_through_serde() {
    let object = instance("arcak-svc", "web1", Some("u-1"));
    let inverses = vec![
        Inverse::IncusDelete {
            object: object.clone(),
        },
        restore(&object, &[("limits.cpu", Some("2")), ("user.note", None)]),
        Inverse::AclRuleRemove {
            project: "arcak-svc".to_string(),
            acl: "acl1".to_string(),
            rule: digest("rule"),
        },
        Inverse::FileRemove {
            path: "units/web1.service".to_string(),
            digest: digest("unit"),
        },
        Inverse::Emission,
    ];
    for inverse in inverses {
        let text = serde_json::to_string(&inverse).expect("an inverse serializes");
        let back: Inverse = serde_json::from_str(&text).expect("an inverse reads back");
        assert_eq!(back, inverse, "round trip of {text}");
    }
}

#[test]
fn the_emission_inverse_is_only_its_op_tag() {
    let text = serde_json::to_string(&Inverse::Emission).expect("serializes");
    assert_eq!(text, r#"{"op":"Emission"}"#);
}

#[test]
fn an_unknown_op_is_refused() {
    let err = serde_json::from_str::<Inverse>(r#"{"op":"IncusReplace","object":{}}"#);
    assert!(err.is_err());
    let missing = serde_json::from_str::<Inverse>(r#"{"object":{}}"#);
    assert!(missing.is_err(), "an inverse with no op is refused");
}

#[test]
fn an_extra_field_is_refused_on_every_variant_including_the_unit_one() {
    let object = json!({"kind": "instance", "project": "p", "name": "n", "uuid": null});
    let cases = [
        json!({"op": "IncusDelete", "object": object, "extra": 1}),
        json!({"op": "IncusRestoreConfig", "object": object, "config": {}, "extra": 1}),
        json!({"op": "AclRuleRemove", "project": "p", "acl": "a", "rule": digest("r").to_hex(), "extra": 1}),
        json!({"op": "FileRemove", "path": "x", "digest": digest("d").to_hex(), "extra": 1}),
        json!({"op": "Emission", "extra": 1}),
    ];
    for case in cases {
        let text = case.to_string();
        assert!(
            serde_json::from_str::<Inverse>(&text).is_err(),
            "an extra field must be refused: {text}"
        );
    }
}

#[test]
fn an_extra_field_inside_the_object_reference_is_refused() {
    let text = json!({
        "op": "IncusDelete",
        "object": {"kind": "instance", "project": "p", "name": "n", "uuid": null, "owner": "x"}
    })
    .to_string();
    assert!(
        serde_json::from_str::<Inverse>(&text).is_err(),
        "ObjectRef ignores unknown fields, so the journal's closed reader must refuse them"
    );
    let record = json!({
        "id": 1,
        "entry": "watch",
        "class": "A",
        "op_digest": digest("watch").to_hex(),
        "object": {"kind": "instance", "project": "p", "name": "n", "uuid": null, "owner": "x"},
        "inverse": null,
        "state": "Intent",
        "at_ms": 1
    })
    .to_string();
    assert!(
        serde_json::from_str::<EffectRecord>(&record).is_err(),
        "a record's object reference is closed too"
    );
}

#[test]
fn an_unknown_field_on_an_effect_record_is_refused() {
    let text = json!({
        "id": 1,
        "entry": "watch",
        "class": "A",
        "op_digest": digest("watch").to_hex(),
        "object": null,
        "inverse": null,
        "state": "Intent",
        "at_ms": 1,
        "note": "x"
    })
    .to_string();
    assert!(serde_json::from_str::<EffectRecord>(&text).is_err());
}

// The fold: intent, then outcome.

#[test]
fn an_applied_then_confirmed_effect_is_one_live_record_with_its_inverse() {
    let mut journal = Journal::open(MemJournal::new()).expect("opens");
    let object = instance("arcak-svc", "web1", Some("u-1"));
    let inverse = Inverse::IncusDelete {
        object: object.clone(),
    };
    let id = apply_one(
        &mut journal,
        "watch",
        EffectClass::B,
        object.clone(),
        inverse.clone(),
    );
    journal.confirmed(id);
    let live = journal.live(&entry("watch"));
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].state, EffectState::Confirmed);
    assert_eq!(live[0].object, Some(object));
    assert_eq!(live[0].inverse, Some(inverse));
    assert!(journal.unresolved().is_empty());
    assert!(journal.poisoned().is_none());
}

#[test]
fn an_intent_with_no_outcome_is_unresolved_and_live_for_its_entry_only() {
    let mut journal = Journal::open(MemJournal::new()).expect("opens");
    let id = journal
        .intend(
            &entry("watch"),
            EffectClass::A,
            digest("w"),
            target(),
            None,
            5,
        )
        .expect("intent");
    let other = journal
        .intend(
            &entry("other"),
            EffectClass::A,
            digest("o"),
            target(),
            None,
            6,
        )
        .expect("intent");
    assert_ne!(id, other, "ids are unique across entries");
    let unresolved = journal.unresolved();
    assert_eq!(unresolved.len(), 2);
    assert!(unresolved.iter().all(|r| r.state == EffectState::Intent));
    let live = journal.live(&entry("watch"));
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].id, id);
    assert!(live[0].object.is_none() && live[0].inverse.is_none());
}

#[test]
fn a_failed_effect_is_neither_live_nor_unresolved() {
    let mut journal = Journal::open(MemJournal::new()).expect("opens");
    let id = journal
        .intend(
            &entry("watch"),
            EffectClass::A,
            digest("w"),
            target(),
            None,
            5,
        )
        .expect("intent");
    journal
        .failed(id, "refused by the provider")
        .expect("failure is journalled");
    assert!(journal.live(&entry("watch")).is_empty());
    assert!(journal.unresolved().is_empty());
}

#[test]
fn a_reverted_effect_is_no_longer_live() {
    let mut journal = Journal::open(MemJournal::new()).expect("opens");
    let object = instance("arcak-svc", "web1", None);
    let id = apply_one(
        &mut journal,
        "watch",
        EffectClass::A,
        object.clone(),
        restore(&object, &[("a", None)]),
    );
    assert_eq!(journal.live(&entry("watch")).len(), 1);
    journal.reverted(id);
    assert!(journal.live(&entry("watch")).is_empty());
    assert!(journal.poisoned().is_none());
}

#[test]
fn an_outcome_for_an_unknown_id_is_refused() {
    let mut journal = Journal::open(MemJournal::new()).expect("opens");
    let object = instance("p", "n", None);
    let refused = journal.applied(7, object.clone(), Inverse::IncusDelete { object });
    assert!(matches!(refused, Err(KernelError::Journal(_))));
    assert!(
        journal.live(&entry("watch")).is_empty(),
        "no record was written"
    );
}

#[test]
fn a_second_outcome_for_one_intent_is_refused() {
    let mut journal = Journal::open(MemJournal::new()).expect("opens");
    let object = instance("p", "n", None);
    let id = journal
        .intend(
            &entry("watch"),
            EffectClass::A,
            digest("w"),
            target(),
            None,
            5,
        )
        .expect("intent");
    journal
        .applied(id, object.clone(), Inverse::Emission)
        .expect("first outcome");
    let second = journal.applied(id, object, Inverse::Emission);
    assert!(matches!(second, Err(KernelError::Journal(_))));
    let failed = journal.failed(id, "late");
    assert!(
        matches!(failed, Err(KernelError::Journal(_))),
        "an applied effect cannot be marked failed"
    );
}

#[test]
fn an_illegal_void_transition_poisons_the_journal() {
    let mut journal = Journal::open(MemJournal::new()).expect("opens");
    let id = journal
        .intend(
            &entry("watch"),
            EffectClass::A,
            digest("w"),
            target(),
            None,
            5,
        )
        .expect("intent");
    // Confirming an intent that never applied is not a legal move. The void method cannot return it, so the journal stops.
    journal.confirmed(id);
    assert!(
        journal.poisoned().is_some(),
        "the illegal move poisons the journal"
    );
    let refused = journal.intend(
        &entry("watch"),
        EffectClass::A,
        digest("x"),
        target(),
        None,
        6,
    );
    assert!(
        matches!(refused, Err(KernelError::Journal(_))),
        "a poisoned journal refuses every later write"
    );
}

/// A store that refuses the append at index `fail_at` without recording it, and accepts every other append.
#[derive(Debug)]
struct FlakyStore {
    records: Vec<EffectRecord>,
    calls: usize,
    fail_at: usize,
}

impl JournalStore for FlakyStore {
    fn append(&mut self, record: &EffectRecord) -> Result<(), KernelError> {
        let call = self.calls;
        self.calls += 1;
        if call == self.fail_at {
            return Err(KernelError::Io {
                path: "journal".to_string(),
                why: "the write was refused".to_string(),
            });
        }
        self.records.push(record.clone());
        Ok(())
    }

    fn load(&self) -> Result<(Vec<EffectRecord>, Option<String>), KernelError> {
        Ok((self.records.clone(), None))
    }
}

#[test]
fn a_store_write_failure_poisons_the_journal_even_when_the_store_recovers() {
    let store = FlakyStore {
        records: Vec::new(),
        calls: 0,
        fail_at: 0,
    };
    let mut journal = Journal::open(store).expect("opens");
    let first = journal.intend(
        &entry("watch"),
        EffectClass::A,
        digest("w"),
        target(),
        None,
        5,
    );
    assert!(matches!(first, Err(KernelError::Io { .. })));
    // The store accepts writes again, but the journal stopped at the failure and writes nothing more.
    let second = journal.intend(
        &entry("watch"),
        EffectClass::A,
        digest("w"),
        target(),
        None,
        6,
    );
    assert!(matches!(second, Err(KernelError::Journal(_))));
    assert!(journal.poisoned().is_some());
    assert!(
        journal.unresolved().is_empty(),
        "the refused intent is not in the journal"
    );
}

#[test]
fn a_failed_outcome_write_leaves_the_intent_unresolved_for_recovery() {
    // Append 0 is the intent; append 1 is the applied record, which the store refuses.
    let store = FlakyStore {
        records: Vec::new(),
        calls: 0,
        fail_at: 1,
    };
    let mut journal = Journal::open(store).expect("opens");
    let id = journal
        .intend(
            &entry("watch"),
            EffectClass::A,
            digest("w"),
            target(),
            None,
            5,
        )
        .expect("the intent is written");
    let object = instance("p", "n", None);
    let outcome = journal.applied(id, object.clone(), Inverse::IncusDelete { object });
    assert!(outcome.is_err());
    assert_eq!(
        journal.unresolved().len(),
        1,
        "the intent has no outcome in the journal, so recovery must resolve it"
    );
}

#[test]
fn opening_refuses_an_outcome_with_no_intent() {
    let mut store = MemJournal::new();
    let object = instance("p", "n", None);
    store
        .append(&record(
            3,
            EffectState::Applied,
            Some(object.clone()),
            Some(Inverse::IncusDelete { object }),
        ))
        .expect("append");
    assert!(matches!(Journal::open(store), Err(KernelError::Journal(_))));
}

#[test]
fn opening_refuses_two_intents_with_one_id() {
    let mut store = MemJournal::new();
    store
        .append(&record(1, EffectState::Intent, None, None))
        .expect("append");
    store
        .append(&record(1, EffectState::Intent, None, None))
        .expect("append");
    assert!(matches!(Journal::open(store), Err(KernelError::Journal(_))));
}

#[test]
fn opening_refuses_an_illegal_transition_on_disk() {
    let mut store = MemJournal::new();
    store
        .append(&record(1, EffectState::Intent, None, None))
        .expect("append");
    store
        .append(&record(1, EffectState::Confirmed, None, None))
        .expect("append");
    assert!(matches!(Journal::open(store), Err(KernelError::Journal(_))));
}

#[test]
fn opening_refuses_an_outcome_whose_fields_do_not_match_its_state() {
    let mut store = MemJournal::new();
    let object = instance("p", "n", None);
    store
        .append(&record(1, EffectState::Intent, Some(object), None))
        .expect("append");
    assert!(matches!(Journal::open(store), Err(KernelError::Journal(_))));
}

#[test]
fn opening_refuses_a_state_record_that_changes_the_digest_of_its_intent() {
    let mut store = MemJournal::new();
    store
        .append(&record(1, EffectState::Intent, None, None))
        .expect("append");
    let object = instance("p", "n", None);
    let mut applied = record(
        1,
        EffectState::Applied,
        Some(object.clone()),
        Some(Inverse::IncusDelete { object }),
    );
    applied.op_digest = digest("another op");
    store.append(&applied).expect("append");
    assert!(matches!(Journal::open(store), Err(KernelError::Journal(_))));
}

#[test]
fn opening_folds_a_legal_history_and_continues_its_ids() {
    let mut store = MemJournal::new();
    let object = instance("p", "n", Some("u-1"));
    store
        .append(&record(1, EffectState::Intent, None, None))
        .expect("append");
    store
        .append(&record(
            1,
            EffectState::Applied,
            Some(object.clone()),
            Some(Inverse::IncusDelete { object }),
        ))
        .expect("append");
    let mut journal = Journal::open(store).expect("a legal history opens");
    assert_eq!(journal.live(&entry("watch")).len(), 1);
    let next = journal
        .intend(
            &entry("watch"),
            EffectClass::A,
            digest("n"),
            target(),
            None,
            9,
        )
        .expect("intent");
    assert_eq!(next, 2, "the next id follows the newest id on disk");
}

// Removal: the partition by class.

#[test]
fn removal_partitions_each_live_effect_into_exactly_one_part() {
    let mut journal = Journal::open(MemJournal::new()).expect("opens");
    let a = instance("arcak-svc", "net-a", None);
    let b = instance("arcak-svc", "db1", Some("u-db"));
    let c = instance("arcak-svc", "mail", None);

    let a_id = apply_one(
        &mut journal,
        "watch",
        EffectClass::A,
        a.clone(),
        restore(&a, &[("k", None)]),
    );
    let b_id = apply_one(
        &mut journal,
        "watch",
        EffectClass::B,
        b.clone(),
        Inverse::IncusDelete { object: b.clone() },
    );
    let c_id = apply_one(
        &mut journal,
        "watch",
        EffectClass::C,
        c.clone(),
        Inverse::Emission,
    );
    let intent = journal
        .intend(
            &entry("watch"),
            EffectClass::A,
            digest("pending"),
            target(),
            None,
            7,
        )
        .expect("intent");

    // Reverted, failed and other-entry effects must not appear in any part.
    let reverted = apply_one(
        &mut journal,
        "watch",
        EffectClass::A,
        a.clone(),
        restore(&a, &[("x", None)]),
    );
    journal.reverted(reverted);
    let failed = journal
        .intend(
            &entry("watch"),
            EffectClass::B,
            digest("gone"),
            target(),
            None,
            8,
        )
        .expect("intent");
    journal.failed(failed, "refused").expect("failure");
    apply_one(
        &mut journal,
        "other",
        EffectClass::A,
        a.clone(),
        Inverse::Emission,
    );

    let removed = removal(&journal, &entry("watch"), 99);
    let revert_ids: Vec<u64> = removed.revert_now.iter().map(|r| r.id).collect();
    assert_eq!(revert_ids, vec![a_id], "class A is reverted at once");
    let never_ids: Vec<u64> = removed.never.iter().map(|r| r.id).collect();
    assert_eq!(never_ids, vec![c_id], "class C is never reverted");
    let unresolved_ids: Vec<u64> = removed.unresolved.iter().map(|r| r.id).collect();
    assert_eq!(
        unresolved_ids,
        vec![intent],
        "an intent with no outcome is unresolved"
    );
    let plan = removed.plan.expect("class B makes one plan");
    assert_eq!(
        plan.objects,
        vec![b],
        "the plan lists exactly the class-B object"
    );
    assert_eq!(plan.created_ms, 99);
    assert_eq!(
        b_id, 2,
        "the class-B effect is the second intent of this journal"
    );
}

#[test]
fn revert_now_runs_the_most_recent_effect_first() {
    let mut journal = Journal::open(MemJournal::new()).expect("opens");
    let net = instance("arcak-svc", "net", None);
    let acl = instance("arcak-svc", "acl", None);
    let first = apply_one(
        &mut journal,
        "watch",
        EffectClass::A,
        net.clone(),
        restore(&net, &[("k", None)]),
    );
    let second = apply_one(
        &mut journal,
        "watch",
        EffectClass::A,
        acl.clone(),
        restore(&acl, &[("k", None)]),
    );
    let removed = removal(&journal, &entry("watch"), 1);
    let order: Vec<u64> = removed.revert_now.iter().map(|r| r.id).collect();
    assert_eq!(order, vec![second, first]);
    assert!(removed.plan.is_none(), "no class-B object means no plan");
}

#[test]
fn a_delete_plan_digest_changes_with_any_object_or_uuid_and_not_with_time() {
    let plan_for = |uuid: &str, name: &str, now: u64| {
        let mut journal = Journal::open(MemJournal::new()).expect("opens");
        let object = instance("arcak-svc", name, Some(uuid));
        apply_one(
            &mut journal,
            "watch",
            EffectClass::B,
            object.clone(),
            Inverse::IncusDelete { object },
        );
        removal(&journal, &entry("watch"), now)
            .plan
            .expect("a plan")
            .digest
    };
    let base = plan_for("u-1", "db1", 1);
    assert_eq!(
        base,
        plan_for("u-1", "db1", 2),
        "the time is not part of the digest"
    );
    assert_ne!(
        base,
        plan_for("u-2", "db1", 1),
        "a changed uuid changes the digest"
    );
    assert_ne!(
        base,
        plan_for("u-1", "db2", 1),
        "a changed name changes the digest"
    );
}

#[test]
fn a_removal_of_an_entry_with_no_live_effects_is_empty() {
    let journal = Journal::open(MemJournal::new()).expect("opens");
    let removed = removal(&journal, &entry("nothing"), 1);
    assert!(removed.revert_now.is_empty());
    assert!(removed.plan.is_none());
    assert!(removed.never.is_empty());
    assert!(removed.unresolved.is_empty());
}

// The file journal: mode, links, locks, torn tails and damage.

#[test]
fn a_new_journal_file_is_created_with_mode_0600() {
    let dir = Scratch::new("mode");
    let path = dir.path("journal.jsonl");
    let _journal = FileJournal::open(&path).expect("created");
    assert_eq!(mode_of(&path), 0o600);
}

#[test]
fn a_directory_at_the_journal_path_is_refused() {
    let dir = Scratch::new("dir");
    let path = dir.path("journal.jsonl");
    fs::create_dir(&path).expect("a directory");
    assert!(matches!(
        FileJournal::open(&path),
        Err(KernelError::Io { .. })
    ));
}

#[test]
fn an_existing_journal_with_a_group_or_other_bit_is_refused() {
    let dir = Scratch::new("wide");
    let path = dir.path("journal.jsonl");
    fs::write(&path, b"").expect("written");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("mode");
    assert!(matches!(
        FileJournal::open(&path),
        Err(KernelError::Io { .. })
    ));
    assert_eq!(mode_of(&path), 0o644, "a refused journal is not changed");
}

#[test]
fn a_symlinked_journal_is_refused_and_its_target_is_not_written() {
    let dir = Scratch::new("link");
    let target = dir.path("target.jsonl");
    fs::write(&target, b"").expect("written");
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).expect("mode");
    let link = dir.path("journal.jsonl");
    symlink(&target, &link).expect("link");
    assert!(matches!(
        FileJournal::open(&link),
        Err(KernelError::Io { .. })
    ));

    let dangling = dir.path("dangling.jsonl");
    symlink(dir.path("absent.jsonl"), &dangling).expect("link");
    assert!(matches!(
        FileJournal::open(&dangling),
        Err(KernelError::Io { .. })
    ));
    assert!(
        !dir.path("absent.jsonl").exists(),
        "the dangling target is not created"
    );
}

#[test]
fn a_second_opener_is_refused_while_the_journal_is_held() {
    let dir = Scratch::new("lock");
    let path = dir.path("journal.jsonl");
    let first = FileJournal::open(&path).expect("first opener");
    let second = FileJournal::open(&path);
    assert!(
        matches!(second, Err(KernelError::Journal(_))),
        "two writers would interleave lines"
    );
    drop(first);
    assert!(
        FileJournal::open(&path).is_ok(),
        "the lock is released with the handle"
    );
}

#[test]
fn records_survive_a_reopen_and_the_next_id_continues() {
    let dir = Scratch::new("reopen");
    let path = dir.path("journal.jsonl");
    let object = instance("arcak-svc", "web1", Some("u-1"));
    {
        let mut journal = Journal::open(FileJournal::open(&path).expect("opens")).expect("folds");
        apply_one(
            &mut journal,
            "watch",
            EffectClass::B,
            object.clone(),
            Inverse::IncusDelete {
                object: object.clone(),
            },
        );
    }
    let mut journal = Journal::open(FileJournal::open(&path).expect("reopens")).expect("folds");
    let live = journal.live(&entry("watch"));
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].object, Some(object.clone()));
    assert_eq!(
        live[0].inverse,
        Some(Inverse::IncusDelete { object }),
        "the inverse read back is the one written"
    );
    assert!(journal.torn_note().is_none());
    let next = journal
        .intend(
            &entry("watch"),
            EffectClass::A,
            digest("n"),
            target(),
            None,
            2,
        )
        .expect("intent");
    assert_eq!(next, 2);
}

#[test]
fn a_torn_last_line_is_dropped_reported_and_cut_before_the_next_append() {
    let dir = Scratch::new("torn");
    let path = dir.path("journal.jsonl");
    {
        let mut journal = Journal::open(FileJournal::open(&path).expect("opens")).expect("folds");
        apply_one(
            &mut journal,
            "watch",
            EffectClass::A,
            instance("p", "n", None),
            Inverse::Emission,
        );
    }
    // A crash cut the next record short: part of its bytes, and no newline.
    let mut file = OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open for append");
    file.write_all(br#"{"id":2,"entry":"wat"#)
        .expect("partial write");
    drop(file);

    let store = FileJournal::open(&path).expect("a torn tail is not fatal");
    let (records, note) = store.load().expect("loads");
    assert_eq!(records.len(), 2, "the two whole records load");
    let note = note.expect("the drop is reported");
    assert!(note.contains("torn last line"), "note: {note}");
    assert!(
        !note.contains("wat"),
        "the note never shows the dropped bytes"
    );
    drop(store);

    // The file is cut back to its last whole line, so the next append does not join the torn bytes.
    let bytes = fs::read(&path).expect("read");
    assert!(bytes.ends_with(b"\n"));
    let mut journal = Journal::open(FileJournal::open(&path).expect("reopens")).expect("folds");
    assert!(
        journal.torn_note().is_none(),
        "already cut, so not reported again"
    );
    let next = journal
        .intend(
            &entry("watch"),
            EffectClass::A,
            digest("n"),
            target(),
            None,
            3,
        )
        .expect("the append after a cut succeeds");
    assert_eq!(next, 2, "the torn record's id is free again: it never ran");
    drop(journal);
    let reread = Journal::open(FileJournal::open(&path).expect("reopens")).expect("folds");
    assert_eq!(reread.unresolved().len(), 1);
}

#[test]
fn a_torn_tail_that_is_the_only_content_leaves_an_empty_journal() {
    let dir = Scratch::new("only-torn");
    let path = dir.path("journal.jsonl");
    fs::write(&path, br#"{"id":1,"ent"#).expect("written");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("mode");
    let journal = Journal::open(FileJournal::open(&path).expect("opens")).expect("folds");
    assert!(journal.torn_note().is_some());
    assert!(journal.live(&entry("watch")).is_empty());
    assert_eq!(fs::metadata(&path).expect("stat").len(), 0);
}

#[test]
fn a_whole_line_that_does_not_decode_refuses_the_journal_and_is_not_cut() {
    let dir = Scratch::new("damage");
    let path = dir.path("journal.jsonl");
    let mut body = Vec::new();
    body.extend_from_slice(b"not a record\n");
    body.extend_from_slice(b"{\"id\":2}\n");
    fs::write(&path, &body).expect("written");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("mode");
    let refused = FileJournal::open(&path);
    assert!(matches!(refused, Err(KernelError::Journal(_))));
    assert_eq!(
        fs::read(&path).expect("read"),
        body,
        "damage is not cut away"
    );
}

#[test]
fn a_newline_terminated_last_line_that_does_not_decode_also_refuses() {
    let dir = Scratch::new("last-damage");
    let path = dir.path("journal.jsonl");
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&path)
        .expect("open");
    file.write_all(b"{\"id\":1,\"extra\":true}\n")
        .expect("write");
    drop(file);
    assert!(matches!(
        FileJournal::open(&path),
        Err(KernelError::Journal(_))
    ));
}

#[test]
fn a_record_on_disk_with_an_unknown_field_refuses_the_journal() {
    let dir = Scratch::new("closed-disk");
    let path = dir.path("journal.jsonl");
    let line = json!({
        "id": 1,
        "entry": "watch",
        "class": "A",
        "op_digest": digest("watch").to_hex(),
        "target": {"kind": "instance", "project": "p", "name": "n", "uuid": null},
        "pre_image": null,
        "object": null,
        "inverse": null,
        "state": "Intent",
        "at_ms": 1,
        "signed_by": "nobody"
    })
    .to_string()
        + "\n";
    fs::write(&path, line).expect("written");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("mode");
    assert!(matches!(
        FileJournal::open(&path),
        Err(KernelError::Journal(_))
    ));
}

#[test]
fn an_existing_empty_journal_is_a_valid_empty_journal() {
    let dir = Scratch::new("empty");
    let path = dir.path("journal.jsonl");
    fs::write(&path, b"").expect("written");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("mode");
    let journal = Journal::open(FileJournal::open(&path).expect("opens")).expect("folds");
    assert!(journal.unresolved().is_empty());
    assert!(journal.torn_note().is_none());
}

#[test]
fn the_journal_error_text_names_no_record_content() {
    let dir = Scratch::new("text");
    let path = dir.path("journal.jsonl");
    let secret = "super-secret-pre-image-value";
    fs::write(&path, format!("{{\"note\":\"{secret}\"\n")).expect("written");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("mode");
    let err = FileJournal::open(&path).expect_err("refused");
    let text = err.to_string();
    assert!(
        !text.contains(secret),
        "the error must not echo the record: {text}"
    );
}

// A reverted intent, the id space, the applied outcome's object, and the delete plan's source.

#[test]
fn an_intent_that_never_applied_can_be_reverted_without_poisoning_the_journal() {
    // A create the reconciler finds absent is reverted with no outcome ever written.
    let mut journal = Journal::open(MemJournal::new()).expect("opens");
    let id = journal
        .intend(
            &entry("watch"),
            EffectClass::A,
            digest("w"),
            target(),
            None,
            5,
        )
        .expect("intent");
    journal.reverted(id);
    assert!(
        journal.poisoned().is_none(),
        "an intent with no outcome may be reverted"
    );
    assert!(journal.unresolved().is_empty());
    assert!(journal.live(&entry("watch")).is_empty());
    let next = journal
        .intend(
            &entry("watch"),
            EffectClass::A,
            digest("n"),
            target(),
            None,
            6,
        )
        .expect("writes continue after the revert");
    assert_eq!(next, 2);
}

#[test]
fn an_effect_id_at_the_top_of_the_range_is_refused_on_open_without_a_panic() {
    let mut store = MemJournal::new();
    store
        .append(&record(u64::MAX, EffectState::Intent, None, None))
        .expect("append");
    assert!(
        matches!(Journal::open(store), Err(KernelError::Journal(_))),
        "a journal that names the last id has no id to continue from"
    );
}

#[test]
fn a_hostile_file_journal_with_the_top_id_is_refused_without_a_panic() {
    let dir = Scratch::new("top-id");
    let path = dir.path("journal.jsonl");
    {
        let mut journal = Journal::open(FileJournal::open(&path).expect("opens")).expect("folds");
        journal
            .intend(
                &entry("watch"),
                EffectClass::A,
                digest("x"),
                target(),
                None,
                1,
            )
            .expect("intent");
    }
    let text = fs::read_to_string(&path).expect("read");
    let hostile = text.replacen("\"id\":1,", &format!("\"id\":{},", u64::MAX), 1);
    fs::write(&path, hostile).expect("written");
    let refused = Journal::open(FileJournal::open(&path).expect("opens"));
    assert!(matches!(refused, Err(KernelError::Journal(_))));
}

#[test]
fn an_intent_is_refused_when_the_last_id_is_taken_and_writes_nothing() {
    let mut store = MemJournal::new();
    store
        .append(&record(u64::MAX - 1, EffectState::Intent, None, None))
        .expect("append");
    let mut journal = Journal::open(store).expect("opens: the last id is not the top id");
    let refused = journal.intend(
        &entry("watch"),
        EffectClass::A,
        digest("n"),
        target(),
        None,
        2,
    );
    assert!(
        matches!(refused, Err(KernelError::Journal(_))),
        "the top id is never written"
    );
    assert!(
        journal.poisoned().is_none(),
        "a refusal before the write does not poison"
    );
    assert_eq!(journal.unresolved().len(), 1, "no record was written");
}

#[test]
fn an_inverse_naming_another_object_is_refused_and_writes_no_outcome() {
    let mut journal = Journal::open(MemJournal::new()).expect("opens");
    let object = instance("arcak-svc", "web1", Some("u-1"));
    let other = instance("arcak-svc", "web2", Some("u-2"));
    let id = journal
        .intend(
            &entry("watch"),
            EffectClass::A,
            digest("c"),
            object.clone(),
            None,
            5,
        )
        .expect("intent");
    let refused = journal.applied(id, object, Inverse::IncusDelete { object: other });
    assert!(matches!(refused, Err(KernelError::Journal(_))));
    assert_eq!(
        journal.unresolved().len(),
        1,
        "the intent stays open and no outcome is written"
    );
    assert!(journal.poisoned().is_none());
}

#[test]
fn a_create_and_a_patch_of_one_instance_make_one_delete_plan_object() {
    let mut journal = Journal::open(MemJournal::new()).expect("opens");
    let made = instance("arcak-svc", "db1", Some("u-db"));
    apply_one(
        &mut journal,
        "watch",
        EffectClass::B,
        made.clone(),
        Inverse::IncusDelete {
            object: made.clone(),
        },
    );
    apply_one(
        &mut journal,
        "watch",
        EffectClass::B,
        made.clone(),
        restore(&made, &[("k", None)]),
    );
    let plan = removal(&journal, &entry("watch"), 1)
        .plan
        .expect("one class-B object");
    assert_eq!(plan.objects, vec![made], "the object is listed once");
}

#[test]
fn a_patch_alone_makes_no_delete_plan() {
    let mut journal = Journal::open(MemJournal::new()).expect("opens");
    let object = instance("arcak-svc", "db1", Some("u-db"));
    apply_one(
        &mut journal,
        "watch",
        EffectClass::B,
        object.clone(),
        restore(&object, &[("k", None)]),
    );
    assert!(
        removal(&journal, &entry("watch"), 1).plan.is_none(),
        "only a delete inverse names an object a plan may remove"
    );
}

// The intent names its target and, for a patch, the pre-image, durably, before the effect runs.

#[test]
fn an_intent_keeps_its_target_and_pre_image_across_a_reopen_so_recovery_can_tell_create_from_patch()
{
    let dir = Scratch::new("intent-fields");
    let path = dir.path("journal.jsonl");
    let made = instance("arcak-svc", "web1", None);
    let patched = instance("arcak-svc", "db1", Some("u-db"));
    let pre_image = BTreeMap::from([
        ("a".to_string(), Some("old".to_string())),
        ("b".to_string(), None),
    ]);
    {
        let mut journal = Journal::open(FileJournal::open(&path).expect("opens")).expect("folds");
        journal
            .intend(
                &entry("watch"),
                EffectClass::A,
                digest("create"),
                made.clone(),
                None,
                1,
            )
            .expect("the create's intent");
        journal
            .intend(
                &entry("watch"),
                EffectClass::A,
                digest("patch"),
                patched.clone(),
                Some(pre_image.clone()),
                2,
            )
            .expect("the patch's intent");
    }
    let journal = Journal::open(FileJournal::open(&path).expect("reopens")).expect("folds");
    let open = journal.unresolved();
    assert_eq!(open.len(), 2);
    assert_eq!(open[0].target, made);
    assert_eq!(open[0].pre_image, None, "a create has no pre-image");
    assert_eq!(open[1].target, patched);
    assert_eq!(
        open[1].pre_image,
        Some(pre_image),
        "a patch's pre-image is durable before its effect"
    );
    assert!(
        open.iter()
            .all(|r| r.object.is_none() && r.inverse.is_none())
    );
}

#[test]
fn an_applied_object_that_is_not_the_intent_target_is_refused_and_writes_nothing() {
    let mut journal = Journal::open(MemJournal::new()).expect("opens");
    let target = instance("arcak-svc", "web1", None);
    let id = journal
        .intend(
            &entry("watch"),
            EffectClass::A,
            digest("c"),
            target,
            None,
            5,
        )
        .expect("intent");
    let other = instance("arcak-svc", "web2", Some("u-2"));
    let refused = journal.applied(id, other.clone(), Inverse::IncusDelete { object: other });
    assert!(matches!(refused, Err(KernelError::Journal(_))));
    assert_eq!(journal.unresolved().len(), 1);
    assert!(journal.poisoned().is_none());
}

#[test]
fn an_applied_object_with_another_uuid_than_the_target_names_is_refused() {
    let mut journal = Journal::open(MemJournal::new()).expect("opens");
    let target = instance("arcak-svc", "db1", Some("u-1"));
    let id = journal
        .intend(
            &entry("watch"),
            EffectClass::A,
            digest("p"),
            target,
            None,
            5,
        )
        .expect("intent");
    let other = instance("arcak-svc", "db1", Some("u-2"));
    let refused = journal.applied(id, other.clone(), Inverse::IncusDelete { object: other });
    assert!(matches!(refused, Err(KernelError::Journal(_))));
}

#[test]
fn a_create_may_learn_its_uuid_when_it_applies() {
    let mut journal = Journal::open(MemJournal::new()).expect("opens");
    let target = instance("arcak-svc", "web1", None);
    let id = journal
        .intend(
            &entry("watch"),
            EffectClass::A,
            digest("c"),
            target,
            None,
            5,
        )
        .expect("intent");
    let made = instance("arcak-svc", "web1", Some("u-new"));
    journal
        .applied(
            id,
            made.clone(),
            Inverse::IncusDelete {
                object: made.clone(),
            },
        )
        .expect("a create's uuid is known only once it applies");
    assert_eq!(journal.live(&entry("watch"))[0].object, Some(made));
}

fn mode_of(path: &Path) -> u32 {
    fs::metadata(path)
        .expect("the file exists")
        .permissions()
        .mode()
        & 0o777
}
