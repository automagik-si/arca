//! Runtime resolution must distinguish a still-landing sync request and must not wait for operations.
mod common;

use arca_incus::{FakeIncus, OPERATIONS_PATH};
use arca_kernel::canon::Digest;
use arca_kernel::desired::{DesiredStateGate, EntryId, PluginId, SignedDocument};
use arca_kernel::identity::NodeKey;
use arca_kernel::journal::{EffectClass, EffectState, FileJournal, Inverse, Journal};
use arca_kernel::ownership::{IncusKind, ObjectRef};
use arca_kernel::plugin::{
    Ctx, EffectOp, EffectRefusal, Manifest, Observe, Planned, Plugin, PluginError, Provider,
    ReadBack, Retirement,
};
use arca_providers::incus::{IncusProvider, RetainedSet};
use arca_reconcile::{PluginSet, Reconciler};
use common::*;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

#[derive(Default)]
struct Fault {
    create: bool,
    delete: bool,
    lands: bool,
    creates: usize,
    deletes: usize,
}
struct Unsure {
    inner: IncusProvider,
    fault: Arc<Mutex<Fault>>,
}
impl Provider for Unsure {
    fn observe(
        &self,
        entry: &EntryId,
        projects: &[String],
        query: &Observe,
    ) -> Result<Value, EffectRefusal> {
        self.inner.observe(entry, projects, query)
    }
    fn plan(&self, entry: &EntryId, op: &EffectOp) -> Result<Planned, EffectRefusal> {
        self.inner.plan(entry, op)
    }
    fn effect(
        &mut self,
        entry: &EntryId,
        op: &EffectOp,
        planned: &Planned,
    ) -> Result<(ObjectRef, Inverse), EffectRefusal> {
        let (uncertain, lands) = {
            let mut f = self.fault.lock().unwrap();
            f.creates += 1;
            (std::mem::take(&mut f.create), f.lands)
        };
        if uncertain {
            if lands {
                self.inner.effect(entry, op, planned)?;
            }
            return Err(EffectRefusal::Unresolved);
        }
        self.inner.effect(entry, op, planned)
    }
    fn read_back(
        &self,
        entry: &EntryId,
        target: &ObjectRef,
        pre: Option<&BTreeMap<String, Option<String>>>,
    ) -> Result<ReadBack, EffectRefusal> {
        self.inner.read_back(entry, target, pre)
    }
    fn retire(
        &mut self,
        entry: &EntryId,
        object: &ObjectRef,
        plan: &Digest,
    ) -> Result<Retirement, EffectRefusal> {
        let (uncertain, lands) = {
            let mut f = self.fault.lock().unwrap();
            f.deletes += 1;
            (std::mem::take(&mut f.delete), f.lands)
        };
        if uncertain {
            if lands {
                self.inner.retire(entry, object, plan)?;
            }
            return Err(EffectRefusal::Unresolved);
        }
        self.inner.retire(entry, object, plan)
    }
}
#[derive(Deserialize)]
struct Config {
    instance: bool,
}
struct Creator;
impl Plugin for Creator {
    const ID: &'static str = "test.creator";
    type Config = Config;
    fn manifest() -> Manifest {
        Manifest {
            id: PluginId(Self::ID.into()),
            version: "0.1.0".into(),
            provides: vec![],
            effects: vec![
                (IncusKind::Acl, EffectClass::A),
                (IncusKind::Instance, EffectClass::B),
            ],
            projects: vec![PROJECT.into(), "default".into()],
            observes: true,
        }
    }
    fn apply(ctx: &mut Ctx<'_>, cfg: &Config) -> Result<(), PluginError> {
        let (kind, project, path) = if cfg.instance {
            (
                IncusKind::Instance,
                PROJECT,
                format!("/1.0/instances/box?project={PROJECT}"),
            )
        } else {
            (
                IncusKind::Acl,
                "default",
                "/1.0/network-acls/box?project=default".into(),
            )
        };
        if ctx.observe(Observe::IncusGet { path }).is_ok() {
            return Ok(());
        }
        ctx.effect(EffectOp::IncusCreate {
            kind,
            project: project.into(),
            parent: None,
            name: "box".into(),
            body: json!({"name":"box","config":{}}),
        })?;
        Ok(())
    }
}
struct Rig {
    scratch: Scratch,
    fake: Arc<FakeIncus>,
    reconciler: Reconciler<FileJournal>,
    fault: Arc<Mutex<Fault>>,
    key: NodeKey,
    first: SignedDocument,
    instance: bool,
}
impl Rig {
    fn new(instance: bool, uncertain: bool, lands: bool) -> Self {
        let scratch = Scratch::new("runtime");
        let fake = Arc::new(start_fake(&scratch));
        let fault = Arc::new(Mutex::new(Fault {
            create: uncertain,
            lands,
            ..Fault::default()
        }));
        let retained = RetainedSet::empty();
        let key = node_key();
        let mut plugins = PluginSet::new();
        plugins.add::<Creator>().unwrap();
        let inner =
            provider(&scratch, &fake, &retained).with_operation_deadline(Duration::from_secs(2));
        let mut reconciler = Reconciler::open(
            DesiredStateGate::new(key.public(), None),
            Journal::open(FileJournal::open(&scratch.path("journal")).unwrap()).unwrap(),
            plugins,
            Box::new(Unsure {
                inner,
                fault: fault.clone(),
            }),
            retained,
            NOW,
        )
        .unwrap();
        let first = signed(
            &key,
            1,
            None,
            vec![entry("creator", Creator::ID, json!({"instance":instance}))],
        );
        reconciler.reconcile(&first, NOW);
        Self {
            scratch,
            fake,
            reconciler,
            fault,
            key,
            first,
            instance,
        }
    }
    fn next(&self, seq: u64, prev: &SignedDocument, carried: bool) -> SignedDocument {
        signed(
            &self.key,
            seq,
            Some(prev.document.digest()),
            if carried {
                vec![entry(
                    "creator",
                    Creator::ID,
                    json!({"instance":self.instance}),
                )]
            } else {
                vec![]
            },
        )
    }
    fn states(&self, id: u64) -> Vec<EffectState> {
        std::fs::read_to_string(self.scratch.path("journal"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<arca_kernel::journal::EffectRecord>(line).unwrap())
            .filter(|r| r.id == id)
            .map(|r| r.state)
            .collect()
    }
}
#[test]
fn runtime_adopts_an_open_create_from_its_owned_read_back() {
    let mut r = Rig::new(false, true, true);
    assert_eq!(r.reconciler.journal().unresolved().len(), 1);
    let next = r.next(2, &r.first, true);
    r.reconciler.reconcile(&next, NOW + 1);
    assert!(r.reconciler.journal().unresolved().is_empty());
    assert_eq!(
        r.reconciler
            .journal()
            .live(&EntryId("creator".into()))
            .len(),
        1
    );
    assert_eq!(r.fault.lock().unwrap().creates, 1);
    assert_eq!(r.states(1), vec![EffectState::Intent, EffectState::Applied]);
}
#[test]
fn runtime_absent_sync_create_stays_open_until_the_late_object_is_adopted() {
    let mut r = Rig::new(false, true, false);
    let next = r.next(2, &r.first, true);
    r.reconciler.reconcile(&next, NOW + 1);
    assert_eq!(
        r.reconciler.journal().unresolved().len(),
        1,
        "a sync create can still land after an absent read"
    );
    assert_eq!(r.fault.lock().unwrap().creates, 1, "no repeat effect");
    // The daemon finishes the original synchronous create after the client's absent read.
    assert_eq!(
        raw(
            &r.fake,
            "POST",
            "/1.0/network-acls?project=default",
            Some(&json!({"name":"box","config":{MARK:"creator"}}))
        )
        .0,
        201
    );
    let later = r.next(3, &next, true);
    r.reconciler.reconcile(&later, NOW + 2);
    assert!(r.reconciler.journal().unresolved().is_empty());
    assert_eq!(r.states(1), vec![EffectState::Intent, EffectState::Applied]);
    assert_eq!(r.fault.lock().unwrap().creates, 1);
}
#[test]
fn runtime_absent_instance_create_is_reverted_before_a_new_intent() {
    let mut r = Rig::new(true, true, false);
    let next = r.next(2, &r.first, true);
    r.reconciler.reconcile(&next, NOW + 1);
    assert!(r.reconciler.journal().unresolved().is_empty());
    assert_eq!(
        r.states(1),
        vec![EffectState::Intent, EffectState::Reverted]
    );
    assert_eq!(
        r.fault.lock().unwrap().creates,
        2,
        "the retry has a new intent"
    );
}
fn deleted(lands: bool) {
    let mut r = Rig::new(false, false, false);
    {
        let mut f = r.fault.lock().unwrap();
        f.delete = true;
        f.lands = lands;
    }
    let remove = r.next(2, &r.first, false);
    r.reconciler.reconcile(&remove, NOW + 1);
    assert_eq!(r.reconciler.journal().unresolved().len(), 1);
    let next = r.next(3, &remove, false);
    r.reconciler.reconcile(&next, NOW + 2);
    assert!(r.reconciler.journal().unresolved().is_empty());
    assert!(
        r.reconciler
            .journal()
            .live(&EntryId("creator".into()))
            .is_empty()
    );
    assert_eq!(r.fault.lock().unwrap().deletes, if lands { 1 } else { 2 });
    assert_eq!(
        r.states(2),
        vec![
            EffectState::Intent,
            if lands {
                EffectState::Reverted
            } else {
                EffectState::Failed
            }
        ]
    );
}
#[test]
fn runtime_absent_delete_is_settled_without_repeating_the_effect() {
    deleted(true);
}
#[test]
fn runtime_present_delete_is_failed_before_a_new_retirement_intent() {
    deleted(false);
}
#[test]
fn runtime_pending_operation_does_not_hold_the_mutex_needed_by_approval_and_sign_in() {
    let r = Rig::new(true, true, true);
    let object = ObjectRef {
        kind: IncusKind::Instance,
        project: PROJECT.into(),
        parent: None,
        name: "box".into(),
        uuid: None,
    };
    let op = r.fake.hold_operation(&object);
    let next = r.next(2, &r.first, true);
    let from = r.fake.requests().len();
    let fake = r.fake.clone();
    let mutex = Arc::new(Mutex::new(r.reconciler));
    let worker = mutex.clone();
    let (tx, rx) = mpsc::channel();
    let write = std::thread::spawn(move || {
        let start = Instant::now();
        worker.lock().unwrap().reconcile(&next, NOW + 1);
        tx.send(start.elapsed()).unwrap();
    });
    let until = Instant::now() + Duration::from_secs(3);
    while !fake.requests()[from..]
        .iter()
        .any(|q| q.path == OPERATIONS_PATH)
    {
        assert!(Instant::now() < until, "runtime resolution started");
        std::thread::sleep(Duration::from_millis(1));
    }
    let owner = mutex.clone();
    let (owner_tx, owner_rx) = mpsc::channel();
    let read = std::thread::spawn(move || {
        let _guard = owner.lock().unwrap();
        owner_tx.send(()).unwrap();
    });
    let prompt = rx.recv_timeout(Duration::from_millis(500)).is_ok();
    let owner_prompt = owner_rx.recv_timeout(Duration::from_millis(50)).is_ok();
    fake.release_operation(&op);
    write.join().unwrap();
    read.join().unwrap();
    assert!(
        prompt && owner_prompt,
        "a later call must refuse promptly and release the owner-facing mutex"
    );
    assert!(
        !fake.requests()[from..]
            .iter()
            .any(|q| q.path.contains("/wait")),
        "runtime settlement must never wait on an operation"
    );
}
