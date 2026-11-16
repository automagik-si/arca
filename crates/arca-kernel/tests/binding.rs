//! A binding changes the journal's effective addresses without rewriting its history.
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use arca_kernel::canon::Digest;
use arca_kernel::desired::EntryId;
use arca_kernel::journal::{
    BindingEvent, EffectClass, EffectRecord, FileJournal, Inverse, Journal, JournalRecord,
    JournalSink, JournalStore, read_journal, removal,
};
use arca_kernel::ownership::{IncusKind, ObjectRef};
use serde_json::{Value, json};

static SEQ: AtomicU64 = AtomicU64::new(0);
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "arca-binding-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        Self(dir.join("journal.jsonl"))
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(self.0.parent().unwrap());
    }
}
fn instance(uuid: &str) -> ObjectRef {
    ObjectRef {
        kind: IncusKind::Instance,
        project: "arcak-svc".into(),
        parent: None,
        name: "sample".into(),
        uuid: Some(uuid.into()),
    }
}
fn volume(pool: &str) -> ObjectRef {
    ObjectRef {
        kind: IncusKind::Volume,
        project: "arcak-svc".into(),
        parent: Some(pool.into()),
        name: "sample-data".into(),
        uuid: None,
    }
}
fn event() -> Value {
    json!({"kind":"binding", "binding":{
        "entry":"service-sample", "at_ms":20,
        "from":{"instance":instance("old"), "root_pool":"pool1", "data_volume":volume("pool1")},
        "to":{"instance":instance("new"), "root_pool":"pool2", "data_volume":volume("pool2")}
    }})
}
fn seed(path: &std::path::Path) {
    let mut journal = Journal::open(FileJournal::open(path).unwrap()).unwrap();
    for object in [volume("pool1"), instance("old")] {
        let mut target = object.clone();
        target.uuid = None;
        let id = journal
            .intend(
                &EntryId("service-sample".into()),
                EffectClass::B,
                Digest::of(b"create"),
                target,
                None,
                10,
            )
            .unwrap();
        journal
            .applied(id, object.clone(), Inverse::IncusDelete { object })
            .unwrap();
    }
    for uuid in [Some("old"), None] {
        let mut target = instance("old");
        target.uuid = uuid.map(str::to_string);
        let config = BTreeMap::from([("user.note".into(), Some("before".into()))]);
        let id = journal
            .intend(
                &EntryId("service-sample".into()),
                EffectClass::A,
                Digest::of(b"patch"),
                target.clone(),
                Some(config.clone()),
                11,
            )
            .unwrap();
        journal
            .applied(
                id,
                instance("old"),
                Inverse::IncusRestoreConfig {
                    object: target,
                    config,
                },
            )
            .unwrap();
    }
}
fn append(path: &std::path::Path, event: &Value) {
    let mut file = OpenOptions::new().append(true).open(path).unwrap();
    writeln!(file, "{event}").unwrap();
    file.sync_all().unwrap();
}

#[test]
fn a_committed_binding_reopens_with_new_instance_volume_and_patch_inverse() {
    let scratch = Scratch::new();
    seed(&scratch.0);
    let before = fs::read(&scratch.0).unwrap();
    append(&scratch.0, &event());
    let journal = Journal::open(
        FileJournal::open(&scratch.0).expect("a committed binding is a known journal record"),
    )
    .expect("the binding follows the current live objects");
    let removed = removal(&journal, &EntryId("service-sample".into()), 30);
    let plan = removed.plan.unwrap();
    assert_eq!(plan.objects, [volume("pool2"), instance("new")]);
    assert!(matches!(&removed.revert_now[0].inverse,
        Some(Inverse::IncusRestoreConfig { object, .. }) if *object == instance("new")));
    assert!(
        fs::read(&scratch.0).unwrap().starts_with(&before),
        "history bytes are unchanged"
    );
}

#[test]
fn before_the_append_and_after_a_torn_append_the_old_binding_stays_current() {
    let line = format!("{}\n", event());
    for cut in 0..line.len() {
        let scratch = Scratch::new();
        seed(&scratch.0);
        let before = fs::read(&scratch.0).unwrap();
        OpenOptions::new()
            .append(true)
            .open(&scratch.0)
            .unwrap()
            .write_all(&line.as_bytes()[..cut])
            .unwrap();
        let journal = Journal::open(FileJournal::open(&scratch.0).unwrap()).unwrap();
        assert_eq!(
            removal(&journal, &EntryId("service-sample".into()), 30)
                .plan
                .unwrap()
                .objects,
            [volume("pool1"), instance("old")]
        );
        assert_eq!(
            fs::read(&scratch.0).unwrap(),
            before,
            "only the torn tail was dropped"
        );
    }
}

#[test]
fn a_binding_that_does_not_name_the_current_instance_or_volume_is_refused() {
    for (pointer, value) in [
        ("/binding/from/instance/uuid", json!("another")),
        ("/binding/from/data_volume/parent", json!("another-pool")),
        ("/binding/entry", json!("another-entry")),
    ] {
        let scratch = Scratch::new();
        seed(&scratch.0);
        let mut event = event();
        *event.pointer_mut(pointer).unwrap() = value;
        append(&scratch.0, &event);
        let bytes = fs::read(&scratch.0).unwrap();
        assert!(Journal::open(FileJournal::open(&scratch.0).unwrap()).is_err());
        assert_eq!(
            fs::read(&scratch.0).unwrap(),
            bytes,
            "a whole bad event is never dropped"
        );
    }
}

#[test]
fn the_binding_wire_is_closed_at_every_level_and_a_legacy_reader_refuses_its_kind() {
    // EffectRecord is the complete closed wire shape used by an older reader.
    assert!(serde_json::from_value::<arca_kernel::journal::EffectRecord>(event()).is_err());
    for pointer in [
        "",
        "/binding",
        "/binding/from",
        "/binding/to/instance",
        "/binding/to/data_volume",
    ] {
        let scratch = Scratch::new();
        seed(&scratch.0);
        let mut event = event();
        event
            .pointer_mut(pointer)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("extra".into(), json!(true));
        append(&scratch.0, &event);
        assert!(
            FileJournal::open(&scratch.0).is_err(),
            "extra field at {pointer}"
        );
    }
    let scratch = Scratch::new();
    seed(&scratch.0);
    let mut unknown = event();
    unknown["kind"] = json!("unknown");
    append(&scratch.0, &unknown);
    assert!(FileJournal::open(&scratch.0).is_err());
}

#[test]
fn later_effect_states_and_a_second_binding_follow_the_effective_history() {
    let scratch = Scratch::new();
    seed(&scratch.0);
    append(&scratch.0, &event());
    {
        let mut journal = Journal::open(FileJournal::open(&scratch.0).unwrap()).unwrap();
        journal.confirmed(2);
        assert!(journal.poisoned().is_none());
    }
    let mut next = event();
    next["binding"]["from"] = next["binding"]["to"].clone();
    next["binding"]["to"]["instance"]["uuid"] = json!("third");
    next["binding"]["to"]["root_pool"] = json!("pool3");
    next["binding"]["to"]["data_volume"]["parent"] = json!("pool3");
    append(&scratch.0, &next);
    let mut journal = Journal::open(FileJournal::open(&scratch.0).unwrap()).unwrap();
    assert_eq!(
        removal(&journal, &EntryId("service-sample".into()), 30)
            .plan
            .unwrap()
            .objects,
        [volume("pool3"), instance("third")]
    );
    journal.reverted(3);
    journal.reverted(4);
    assert!(
        journal.poisoned().is_none(),
        "translated patch inverses still settle"
    );
    drop(journal);
    let journal = Journal::open(FileJournal::open(&scratch.0).unwrap()).unwrap();
    assert_eq!(journal.live(&EntryId("service-sample".into())).len(), 2);
}

#[test]
fn the_append_api_writes_one_line_and_replays_storage_facts_without_rewriting_history() {
    let scratch = Scratch::new();
    seed(&scratch.0);
    let before = fs::read(&scratch.0).unwrap();
    let event: BindingEvent = serde_json::from_value(event()["binding"].clone()).unwrap();
    let mut journal = Journal::open(FileJournal::open(&scratch.0).unwrap()).unwrap();
    journal.bind(event.clone()).unwrap();
    assert_eq!(journal.bindings().next().unwrap().to.root_pool, "pool2");
    let after = fs::read(&scratch.0).unwrap();
    assert!(after.starts_with(&before));
    assert_eq!(
        after[before.len()..]
            .iter()
            .filter(|byte| **byte == b'\n')
            .count(),
        1
    );
    assert!(
        journal.bind(event).is_err(),
        "a stale or duplicate source is refused"
    );
    assert_eq!(fs::read(&scratch.0).unwrap(), after);
    let (listed, _) = read_journal(&scratch.0).unwrap();
    assert!(
        listed
            .iter()
            .filter(|record| record.id == 2)
            .all(|record| record.target.uuid.as_deref() == Some("new"))
    );
    drop(journal);
    let journal = Journal::open(FileJournal::open(&scratch.0).unwrap()).unwrap();
    let facts = journal.bindings().next().unwrap();
    assert_eq!(facts.to.root_pool, "pool2");
    assert_eq!(facts.to.data_volume, Some(volume("pool2")));
    assert!(
        journal.unresolved().is_empty(),
        "a binding has no external effect to resolve"
    );
}

#[test]
fn a_failed_binding_append_poisons_the_writer_and_reopen_decides_what_landed() {
    struct Uncertain {
        inner: FileJournal,
        land: bool,
    }
    impl JournalStore for Uncertain {
        fn append(&mut self, record: &EffectRecord) -> Result<(), arca_kernel::KernelError> {
            self.inner.append(record)
        }
        fn load(&self) -> Result<(Vec<EffectRecord>, Option<String>), arca_kernel::KernelError> {
            self.inner.load()
        }
        fn history(
            &self,
        ) -> Result<(Vec<JournalRecord>, Option<String>), arca_kernel::KernelError> {
            self.inner.history()
        }
        fn append_binding(&mut self, event: &BindingEvent) -> Result<(), arca_kernel::KernelError> {
            if self.land {
                self.inner.append_binding(event)?;
            }
            Err(arca_kernel::KernelError::Journal("uncertain append".into()))
        }
    }
    for land in [false, true] {
        let scratch = Scratch::new();
        seed(&scratch.0);
        let mut journal = Journal::open(Uncertain {
            inner: FileJournal::open(&scratch.0).unwrap(),
            land,
        })
        .unwrap();
        let event: BindingEvent = serde_json::from_value(event()["binding"].clone()).unwrap();
        assert!(journal.bind(event.clone()).is_err());
        assert!(journal.poisoned().is_some());
        assert!(journal.bind(event).is_err());
        assert!(
            journal.bindings().next().is_none(),
            "memory changes only after append succeeds"
        );
        drop(journal);
        let journal = Journal::open(FileJournal::open(&scratch.0).unwrap()).unwrap();
        assert_eq!(journal.bindings().next().is_some(), land);
        assert_eq!(
            removal(&journal, &EntryId("service-sample".into()), 30)
                .plan
                .unwrap()
                .objects,
            if land {
                vec![volume("pool2"), instance("new")]
            } else {
                vec![volume("pool1"), instance("old")]
            }
        );
    }
}

#[test]
fn a_pending_effect_or_invalid_destination_refuses_a_binding_before_any_append() {
    for (pointer, value) in [
        ("/to/instance/uuid", json!("old")),
        ("/to/instance/project", json!("another-project")),
        ("/to/data_volume/project", json!("another-project")),
        ("/to/data_volume/uuid", json!("not-a-volume-identity")),
        ("/to/data_volume", Value::Null),
        ("/to/root_pool", json!("")),
    ] {
        let scratch = Scratch::new();
        seed(&scratch.0);
        let before = fs::read(&scratch.0).unwrap();
        let mut data = event()["binding"].clone();
        *data.pointer_mut(pointer).unwrap() = value;
        let mut journal = Journal::open(FileJournal::open(&scratch.0).unwrap()).unwrap();
        assert!(
            journal.bind(serde_json::from_value(data).unwrap()).is_err(),
            "{pointer}"
        );
        assert_eq!(fs::read(&scratch.0).unwrap(), before);
        assert!(
            journal.poisoned().is_none(),
            "a bad request does not poison a healthy journal"
        );
    }
    let scratch = Scratch::new();
    seed(&scratch.0);
    let mut journal = Journal::open(FileJournal::open(&scratch.0).unwrap()).unwrap();
    let id = journal
        .intend(
            &EntryId("service-sample".into()),
            EffectClass::A,
            Digest::of(b"open"),
            instance("old"),
            Some(BTreeMap::new()),
            12,
        )
        .unwrap();
    let before = fs::read(&scratch.0).unwrap();
    assert!(
        journal
            .bind(serde_json::from_value(event()["binding"].clone()).unwrap())
            .is_err()
    );
    assert_eq!(fs::read(&scratch.0).unwrap(), before);
    journal.failed(id, "not applied").unwrap();
    journal
        .bind(serde_json::from_value(event()["binding"].clone()).unwrap())
        .unwrap();
}

fn seed_other(path: &std::path::Path, slot: &str, source_volume: bool) {
    let mut journal = Journal::open(FileJournal::open(path).unwrap()).unwrap();
    let mut object = if source_volume {
        volume("pool1")
    } else {
        instance("new")
    };
    if !source_volume {
        object.name = "other-name".into();
    }
    let mut target = object.clone();
    if slot != "target" {
        target.uuid = None;
    }
    let mut outcome = object.clone();
    if slot == "inverse" {
        outcome.uuid = None;
    }
    let inverse = if slot == "create" {
        Inverse::IncusDelete { object }
    } else if slot == "inverse" {
        Inverse::IncusRestoreConfig {
            object,
            config: BTreeMap::new(),
        }
    } else {
        Inverse::Emission
    };
    let id = journal
        .intend(
            &EntryId("other-entry".into()),
            EffectClass::B,
            Digest::of(b"other"),
            target,
            None,
            12,
        )
        .unwrap();
    journal.applied(id, outcome, inverse).unwrap();
}

#[test]
fn bind_refuses_a_destination_uuid_owned_by_another_entry_at_any_address() {
    for slot in ["create", "target", "object", "inverse"] {
        let scratch = Scratch::new();
        seed(&scratch.0);
        seed_other(&scratch.0, slot, false);
        let before = fs::read(&scratch.0).unwrap();
        let mut journal = Journal::open(FileJournal::open(&scratch.0).unwrap()).unwrap();
        let binding = serde_json::from_value(event()["binding"].clone()).unwrap();
        assert!(journal.bind(binding).is_err(), "collision in {slot}");
        assert_eq!(fs::read(&scratch.0).unwrap(), before);
        assert_eq!(
            removal(&journal, &EntryId("service-sample".into()), 30)
                .plan
                .unwrap()
                .objects,
            [volume("pool1"), instance("old")]
        );
    }
}

#[test]
fn replay_refuses_a_destination_uuid_owned_by_another_entry_at_any_address() {
    for slot in ["create", "target", "object", "inverse"] {
        let scratch = Scratch::new();
        seed(&scratch.0);
        seed_other(&scratch.0, slot, false);
        append(&scratch.0, &event());
        let bytes = fs::read(&scratch.0).unwrap();
        assert!(
            Journal::open(FileJournal::open(&scratch.0).unwrap()).is_err(),
            "collision in {slot}"
        );
        assert_eq!(fs::read(&scratch.0).unwrap(), bytes);
    }
}

#[test]
fn another_entry_on_the_source_volume_refuses_bind_and_replay() {
    for slot in ["target", "object", "inverse"] {
        let scratch = Scratch::new();
        seed(&scratch.0);
        seed_other(&scratch.0, slot, true);
        let before = fs::read(&scratch.0).unwrap();
        {
            let mut journal = Journal::open(FileJournal::open(&scratch.0).unwrap()).unwrap();
            let binding = serde_json::from_value(event()["binding"].clone()).unwrap();
            assert!(journal.bind(binding).is_err(), "source volume in {slot}");
            assert_eq!(fs::read(&scratch.0).unwrap(), before);
        }
        append(&scratch.0, &event());
        assert!(
            Journal::open(FileJournal::open(&scratch.0).unwrap()).is_err(),
            "source volume in {slot}"
        );
    }
}
