//! The kernel's contract for objects that have no uuid, for parents, and for an outcome that is not known. A kind without a
//! uuid is undone by name: its inverse is an `IncusDelete` whose object carries no uuid, and the object's kind, project,
//! parent and name are what the inverse is keyed by. A volume or a bucket is named by its pool, so two volumes with one name
//! in two pools are two objects. An effect whose outcome is not known stays an open intent, and no outcome is journalled for it.

use std::cell::RefCell;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use arca_kernel::canon::Digest;
use arca_kernel::desired::{EntryId, PluginId};
use arca_kernel::journal::{
    EffectClass, EffectRecord, EffectState, FileJournal, Inverse, Journal, MemJournal, removal,
};
use arca_kernel::ownership::{IncusKind, ObjectRef};
use arca_kernel::plugin::{
    Ctx, EffectOp, EffectRefusal, Manifest, Observe, Planned, Provider, op_digest,
};
use serde_json::{Value, json};

fn entry() -> EntryId {
    EntryId("toy".to_string())
}

fn manifest(kind: IncusKind, class: EffectClass) -> Manifest {
    Manifest {
        id: PluginId("arca.test".to_string()),
        version: "0.1.0".to_string(),
        provides: Vec::new(),
        effects: vec![(kind, class)],
        projects: vec!["kproj".to_string()],
        observes: false,
    }
}

/// A provider that plans as its class says and answers its effect with a fixed object and inverse, or a refusal.
struct Answer {
    class: EffectClass,
    object: Option<ObjectRef>,
    inverse: Option<Inverse>,
    refuse: Option<EffectRefusal>,
    effects: RefCell<usize>,
}

impl Answer {
    fn new(class: EffectClass) -> Answer {
        Answer {
            class,
            object: None,
            inverse: None,
            refuse: None,
            effects: RefCell::new(0),
        }
    }
}

impl Provider for Answer {
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
            class: self.class,
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
        *self.effects.borrow_mut() += 1;
        if let Some(refusal) = &self.refuse {
            return Err(refusal.clone());
        }
        let object = self.object.clone().unwrap_or_else(|| match op {
            EffectOp::IncusCreate {
                kind,
                project,
                parent,
                name,
                ..
            } => ObjectRef {
                kind: *kind,
                project: project.clone(),
                parent: parent.clone(),
                name: name.clone(),
                uuid: None,
            },
            _ => panic!("the test only creates"),
        });
        let inverse = self
            .inverse
            .clone()
            .unwrap_or_else(|| Inverse::IncusDelete {
                object: object.clone(),
            });
        Ok((object, inverse))
    }
}

fn create(kind: IncusKind, project: &str, parent: Option<&str>, name: &str) -> EffectOp {
    EffectOp::IncusCreate {
        kind,
        project: project.to_string(),
        parent: parent.map(str::to_string),
        name: name.to_string(),
        body: json!({}),
    }
}

fn object(kind: IncusKind, project: &str, parent: Option<&str>, name: &str) -> ObjectRef {
    ObjectRef {
        kind,
        project: project.to_string(),
        parent: parent.map(str::to_string),
        name: name.to_string(),
        uuid: None,
    }
}

fn scratch(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .subsec_nanos();
    let dir =
        std::env::temp_dir().join(format!("arca-inverse-{}-{nanos}-{tag}", std::process::id()));
    fs::create_dir_all(&dir).expect("create the scratch directory");
    dir
}

#[test]
fn a_create_of_a_kind_without_a_uuid_takes_a_delete_by_name_inverse() {
    // A project has no uuid. Its create is undone by name: the inverse is an IncusDelete with no uuid, and the effect is applied.
    let manifest = manifest(IncusKind::Project, EffectClass::B);
    let mut provider = Answer::new(EffectClass::B);
    let mut journal = Journal::open(MemJournal::new()).expect("opens");
    let op = create(IncusKind::Project, "kproj", None, "kproj");
    {
        let mut ctx = Ctx::new(entry(), &manifest, &mut provider, &mut journal, 7);
        let made = ctx.effect(op).expect("the create applies");
        assert_eq!(made.uuid, None, "a project carries no uuid");
    }
    let live = journal.live(&entry());
    assert_eq!(live.len(), 1, "{live:?}");
    assert_eq!(live[0].state, EffectState::Applied, "{live:?}");
    assert_eq!(
        live[0].inverse,
        Some(Inverse::IncusDelete {
            object: object(IncusKind::Project, "kproj", None, "kproj"),
        }),
        "the inverse deletes the project by its name"
    );
    assert!(journal.unresolved().is_empty());
}

#[test]
fn an_instance_answered_without_a_uuid_is_refused_and_its_intent_stays_open() {
    // An instance has a uuid, so an answer without one is not the object the create made. Its intent stays open.
    let manifest = manifest(IncusKind::Instance, EffectClass::B);
    let mut provider = Answer::new(EffectClass::B);
    let mut journal = Journal::open(MemJournal::new()).expect("opens");
    let op = create(IncusKind::Instance, "kproj", None, "i1");
    let result = Ctx::new(entry(), &manifest, &mut provider, &mut journal, 7).effect(op);
    assert!(
        matches!(result, Err(EffectRefusal::Provider(_))),
        "{result:?}"
    );
    assert_eq!(journal.unresolved().len(), 1, "the intent stays open");
    assert!(
        journal
            .live(&entry())
            .iter()
            .all(|r| r.state == EffectState::Intent)
    );
}

#[test]
fn a_volume_answered_in_another_pool_is_refused_and_its_intent_stays_open() {
    // A volume is named by its pool, so an answer that names another pool is another object.
    let manifest = manifest(IncusKind::Volume, EffectClass::B);
    let mut provider = Answer::new(EffectClass::B);
    provider.object = Some(ObjectRef {
        uuid: Some("u-volume-1".to_string()),
        ..object(IncusKind::Volume, "kproj", Some("default"), "data")
    });
    let mut journal = Journal::open(MemJournal::new()).expect("opens");
    let op = create(IncusKind::Volume, "kproj", Some("fast"), "data");
    let result = Ctx::new(entry(), &manifest, &mut provider, &mut journal, 7).effect(op);
    assert!(
        matches!(result, Err(EffectRefusal::Provider(_))),
        "{result:?}"
    );
    assert_eq!(journal.unresolved().len(), 1, "the intent stays open");
    assert!(
        journal
            .live(&entry())
            .iter()
            .all(|r| r.state == EffectState::Intent),
        "no applied record for another pool"
    );
}

#[test]
fn an_unresolved_outcome_leaves_the_intent_open_and_journals_no_failure() {
    // A provider that cannot tell whether its write landed says so with Unresolved. The intent is not journalled as failed,
    // since a failed effect is one that did not happen, and the entry stays suspended with the intent unresolved.
    let manifest = manifest(IncusKind::Network, EffectClass::A);
    let mut provider = Answer::new(EffectClass::A);
    provider.refuse = Some(EffectRefusal::Unresolved);
    let mut journal = Journal::open(MemJournal::new()).expect("opens");
    let op = create(IncusKind::Network, "kproj", None, "net1");
    let result = Ctx::new(entry(), &manifest, &mut provider, &mut journal, 7).effect(op);
    assert_eq!(result, Err(EffectRefusal::Unresolved));
    let unresolved = journal.unresolved();
    assert_eq!(unresolved.len(), 1, "{unresolved:?}");
    assert_eq!(unresolved[0].state, EffectState::Intent);
    assert!(
        journal
            .live(&entry())
            .iter()
            .all(|r| r.state != EffectState::Failed),
        "no failed record"
    );
}

#[test]
fn a_refusal_that_is_not_unresolved_is_journalled_as_failed() {
    // The ordinary refusal still records that the effect did not happen.
    let manifest = manifest(IncusKind::Network, EffectClass::A);
    let mut provider = Answer::new(EffectClass::A);
    provider.refuse = Some(EffectRefusal::Stale);
    let mut journal = Journal::open(MemJournal::new()).expect("opens");
    let op = create(IncusKind::Network, "kproj", None, "net1");
    let result = Ctx::new(entry(), &manifest, &mut provider, &mut journal, 7).effect(op);
    assert_eq!(result, Err(EffectRefusal::Stale));
    assert!(journal.unresolved().is_empty());
    assert!(
        journal.live(&entry()).is_empty(),
        "a failed effect is not live"
    );
}

#[test]
fn two_volumes_with_one_name_in_two_pools_are_two_objects_in_a_removal_plan() {
    // The removal plan lists each class-B object once. Two volumes named `data` in two pools differ by parent, so both are listed.
    let mut journal = Journal::open(MemJournal::new()).expect("opens");
    for (id, pool) in [(1u64, "default"), (2, "fast")] {
        let target = object(IncusKind::Volume, "kproj", Some(pool), "data");
        let op = create(IncusKind::Volume, "kproj", Some(pool), "data");
        let digest = op_digest(&op);
        let intent = arca_kernel::journal::JournalSink::intend(
            &mut journal,
            &entry(),
            EffectClass::B,
            digest,
            target.clone(),
            None,
            7,
        )
        .expect("intent");
        assert_eq!(intent, id);
        arca_kernel::journal::JournalSink::applied(
            &mut journal,
            intent,
            target.clone(),
            Inverse::IncusDelete { object: target },
        )
        .expect("applied");
    }
    let plan = removal(&journal, &entry(), 9).plan.expect("a plan");
    assert_eq!(plan.objects.len(), 2, "{:?}", plan.objects);
    assert_eq!(plan.objects[0].parent.as_deref(), Some("default"));
    assert_eq!(plan.objects[1].parent.as_deref(), Some("fast"));
}

#[test]
fn an_object_ref_without_a_parent_serializes_as_it_did_before_the_parent_field() {
    // Only the kinds with a parent carry the key. Every other object's JSON, and so every digest over it, is unchanged.
    let value =
        serde_json::to_value(object(IncusKind::Instance, "svc", None, "db1")).expect("serializes");
    assert_eq!(
        value,
        json!({"kind": "instance", "project": "svc", "name": "db1", "uuid": null})
    );
    let volume = serde_json::to_value(object(IncusKind::Volume, "svc", Some("fast"), "v"))
        .expect("serializes");
    assert_eq!(volume["parent"], json!("fast"));
}

#[test]
fn a_record_written_before_the_parent_field_still_decodes_with_no_parent() {
    // A journal line from before the parent field has no `parent` key. It loads, with the parent read as none.
    let dir = scratch("old-record");
    let path = dir.join("journal.jsonl");
    let digest = Digest::of(b"op").to_hex();
    let target = json!({"kind": "network", "project": "svc", "name": "net1", "uuid": null});
    let intent = json!({
        "id": 1, "entry": "toy", "class": "A", "op_digest": digest,
        "target": target, "pre_image": null, "object": null, "inverse": null,
        "state": "Intent", "at_ms": 7
    });
    let applied = json!({
        "id": 1, "entry": "toy", "class": "A", "op_digest": digest,
        "target": target, "pre_image": null,
        "object": target,
        "inverse": {"op": "IncusDelete", "object": target},
        "state": "Applied", "at_ms": 7
    });
    let line = format!("{intent}\n{applied}");
    fs::write(&path, format!("{line}\n")).expect("write the old records");
    // The journal refuses a file with any group or other bit, so the old file is made as a journal file is.
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("mode 0600");
    let file = FileJournal::open(&path).expect("opens with the old record");
    let journal = Journal::open(file).expect("the journal folds the old record");
    let live = journal.live(&entry());
    assert_eq!(live.len(), 1, "{live:?}");
    let record: &EffectRecord = &live[0];
    assert_eq!(record.target.parent, None);
    assert_eq!(record.state, EffectState::Applied);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_provider_that_answers_for_another_parent_is_refused_at_the_applied_record() {
    // The intent names a volume in `fast`. A provider whose answer names the volume in `default` is refused, even when the
    // kind, project and name agree.
    let manifest = manifest(IncusKind::Bucket, EffectClass::B);
    let mut provider = Answer::new(EffectClass::B);
    provider.object = Some(object(IncusKind::Bucket, "kproj", Some("default"), "b1"));
    let mut journal = Journal::open(MemJournal::new()).expect("opens");
    let op = create(IncusKind::Bucket, "kproj", Some("fast"), "b1");
    let result = Ctx::new(entry(), &manifest, &mut provider, &mut journal, 7).effect(op);
    assert!(
        matches!(result, Err(EffectRefusal::Provider(_))),
        "{result:?}"
    );
    assert_eq!(journal.unresolved().len(), 1);
}
