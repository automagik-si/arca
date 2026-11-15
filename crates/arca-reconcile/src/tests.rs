//! The reconciler's own rules, run against a stub provider that records every retirement it is asked for. The integration suite
//! runs the same rules against the fake Incus through the real provider and the real approval verifier.

use super::*;
use arca_approval::MemNonces;
use arca_kernel::canon::{canonical_json, params_digest};
use arca_kernel::desired::{Document, sign_document};
use arca_kernel::identity::NodeKey;
use arca_kernel::journal::MemJournal;
use arca_kernel::ownership::IncusKind;
use arca_kernel::plugin::{Observe, Planned, PluginError};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

/// 2026-10-30T12:00:00.000Z: the clock every unit test runs at.
const NOW: u64 = 1_793_361_600_000;
const ISSUED: &str = "2026-10-30T12:00:00.000Z";
const EXPIRES: &str = "2026-10-30T12:10:00.000Z";
const NONCE: &str = "00000000000000000000000000000001";
const UNIT_PROJECT: &str = "unit-project";
const DUMMY: &str = "unit.dummy";

/// Accepts any signature. The approval statement itself is still parsed and checked by the verifier.
struct AcceptAll;

impl SignatureVerifier for AcceptAll {
    fn verify(
        &self,
        _namespace: &str,
        _statement: &[u8],
        _signature: &[u8],
    ) -> Result<String, ApprovalRefusal> {
        Ok("fingerprint".to_string())
    }
}

/// A plugin with no effects of its own: the unit tests drive the journal directly.
struct Dummy;

impl Plugin for Dummy {
    const ID: &'static str = DUMMY;
    type Config = Value;

    fn manifest() -> Manifest {
        Manifest {
            id: PluginId(DUMMY.to_string()),
            version: "0.1.0".to_string(),
            provides: Vec::new(),
            effects: vec![
                (IncusKind::Acl, EffectClass::A),
                (IncusKind::Instance, EffectClass::B),
            ],
            projects: vec![UNIT_PROJECT.to_string()],
            observes: false,
        }
    }

    fn apply(_ctx: &mut Ctx<'_>, _cfg: &Value) -> Result<(), PluginError> {
        Ok(())
    }
}

/// A provider that records each object it is asked to retire. An object named in `unresolved` gets an outcome it cannot vouch
/// for, so its intent stays open.
#[derive(Clone)]
struct Stub {
    asked: Arc<Mutex<Vec<String>>>,
    unresolved: Vec<String>,
}

impl Stub {
    fn new(unresolved: &[&str]) -> Self {
        Stub {
            asked: Arc::new(Mutex::new(Vec::new())),
            unresolved: unresolved.iter().map(|name| name.to_string()).collect(),
        }
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().expect("the log is readable").clone()
    }
}

impl Provider for Stub {
    fn observe(
        &self,
        _entry: &EntryId,
        _projects: &[String],
        _query: &Observe,
    ) -> Result<Value, EffectRefusal> {
        Err(EffectRefusal::NotDeclared)
    }

    fn plan(&self, _entry: &EntryId, _op: &EffectOp) -> Result<Planned, EffectRefusal> {
        Err(EffectRefusal::NotDeclared)
    }

    fn effect(
        &mut self,
        _entry: &EntryId,
        _op: &EffectOp,
        _planned: &Planned,
    ) -> Result<(ObjectRef, Inverse), EffectRefusal> {
        Err(EffectRefusal::NotDeclared)
    }

    fn retire(
        &mut self,
        _entry: &EntryId,
        object: &ObjectRef,
        _plan: &Digest,
    ) -> Result<Retirement, EffectRefusal> {
        self.asked
            .lock()
            .expect("the log is writable")
            .push(object.name.clone());
        if self.unresolved.contains(&object.name) {
            return Err(EffectRefusal::Unresolved);
        }
        Ok(Retirement::Deleted(object.clone()))
    }
}

fn object(name: &str, kind: IncusKind) -> ObjectRef {
    ObjectRef {
        kind,
        project: UNIT_PROJECT.to_string(),
        parent: None,
        name: name.to_string(),
        uuid: None,
    }
}

fn dummy_entry(id: &str, config: Value) -> Entry {
    let text = canonical_json(&config).expect("the config canonicalizes");
    Entry {
        id: EntryId(id.to_string()),
        plugin: PluginId(DUMMY.to_string()),
        version: "0.1.0".to_string(),
        digest: Digest::of(text.as_bytes()),
        signer: "node".to_string(),
        config,
        disabled: false,
    }
}

fn signed(key: &NodeKey, seq: u64, prev: Option<Digest>, entries: Vec<Entry>) -> SignedDocument {
    sign_document(
        Document {
            node: key.id(),
            seq,
            prev,
            issued_at: ISSUED.to_string(),
            expires_at: "2026-10-30T13:00:00.000Z".to_string(),
            entries,
        },
        key,
    )
}

fn open(stub: &Stub, key: &NodeKey) -> Reconciler<MemJournal> {
    let mut plugins = PluginSet::new();
    plugins.add::<Dummy>().expect("the dummy plugin is valid");
    let journal = Journal::open(MemJournal::new()).expect("an empty journal opens");
    Reconciler::open(
        DesiredStateGate::new(key.public(), None),
        journal,
        plugins,
        Box::new(stub.clone()),
        RetainedSet::empty(),
        NOW,
    )
    .expect("an empty journal opens the reconciler")
}

/// Journals an applied effect of `class` on `object` for `entry`, the way a plugin's effect would be journalled.
fn record<S: JournalStore>(
    r: &mut Reconciler<S>,
    entry: &str,
    class: EffectClass,
    object: ObjectRef,
) -> u64 {
    let id = r
        .journal
        .intend(
            &EntryId(entry.to_string()),
            class,
            Digest::of(object.name.as_bytes()),
            object.clone(),
            None,
            NOW,
        )
        .expect("the intent is journalled");
    let inverse = if class == EffectClass::C {
        Inverse::Emission
    } else {
        Inverse::IncusDelete {
            object: object.clone(),
        }
    };
    r.journal
        .applied(id, object, inverse)
        .expect("the outcome is journalled");
    id
}

/// The entry `entry` as an applied one: the reconciler knows its plugin, so a removal can plan its class-B objects.
fn known<S: JournalStore>(r: &mut Reconciler<S>, entry: &str) {
    r.applied_by
        .insert(EntryId(entry.to_string()), PluginId(DUMMY.to_string()));
}

/// An approval of `plan` that the stub verifier accepts, with the statement the kernel's verifier parses.
fn approval_of(plan: &Digest, nonce: &str) -> String {
    let args = json!({ "plan": plan.to_hex() });
    let statement = json!({
        "host": "node1",
        "action": "delete",
        "target": format!("plan-{}", plan.to_hex()),
        "paramsDigest": params_digest(&args).expect("the arguments are an object"),
        "issuedAt": ISSUED,
        "expiresAt": EXPIRES,
        "nonce": nonce,
    });
    let text = canonical_json(&statement).expect("the statement canonicalizes");
    // The parser requires an armoured signature shape; the stub verifier accepts whatever it is given.
    let armour = "-----BEGIN SSH SIGNATURE-----\nAAAA\n-----END SSH SIGNATURE-----\n";
    format!("{}.{}", STANDARD.encode(text), STANDARD.encode(armour))
}

fn verifier() -> ApprovalVerifier<AcceptAll, MemNonces> {
    ApprovalVerifier::new("node1".to_string(), AcceptAll, MemNonces::default())
}

fn awaiting(outcomes: &[Outcome]) -> DeletePlan {
    outcomes
        .iter()
        .find_map(|outcome| match outcome {
            Outcome::AwaitingApproval { plan } => Some(plan.clone()),
            _ => None,
        })
        .expect("a plan awaits approval")
}

#[test]
fn an_unresolved_intent_stops_every_later_effect() {
    let key = NodeKey::generate();
    let stub = Stub::new(&["acl-y"]);
    let mut r = open(&stub, &key);
    known(&mut r, "e1");
    record(
        &mut r,
        "e1",
        EffectClass::A,
        object("acl-x", IncusKind::Acl),
    );
    record(
        &mut r,
        "e1",
        EffectClass::A,
        object("acl-y", IncusKind::Acl),
    );

    let doc1 = signed(&key, 1, None, vec![]);
    let first = r.reconcile(&doc1, NOW);
    assert!(
        first
            .iter()
            .any(|outcome| matches!(outcome, Outcome::Failed { .. })),
        "the undo that is not known is reported: {first:?}"
    );
    assert_eq!(
        stub.asked(),
        vec!["acl-y".to_string()],
        "the older undo never ran"
    );
    assert_eq!(r.journal.unresolved().len(), 1, "the intent stays open");

    let doc2 = signed(
        &key,
        2,
        Some(doc1.document.digest()),
        vec![dummy_entry("e2", json!({}))],
    );
    let second = r.reconcile(&doc2, NOW);
    assert_eq!(
        stub.asked(),
        vec!["acl-y".to_string()],
        "no retirement ran while blocked"
    );
    assert!(
        second
            .iter()
            .all(|outcome| !matches!(outcome, Outcome::Applied { .. })),
        "a new entry is not applied while blocked: {second:?}"
    );
}

#[test]
fn an_unresolved_intent_blocks_a_run_of_an_approved_plan() {
    let key = NodeKey::generate();
    let stub = Stub::new(&["box-x"]);
    let mut r = open(&stub, &key);
    known(&mut r, "e1");
    record(
        &mut r,
        "e1",
        EffectClass::B,
        object("box-x", IncusKind::Instance),
    );
    let plan = awaiting(&r.reconcile(&signed(&key, 1, None, vec![]), NOW)).digest;
    // An intent with no outcome, from a write this process never finished, blocks the run before the approval is checked.
    r.journal
        .intend(
            &EntryId("e9".to_string()),
            EffectClass::A,
            Digest::of(b"other"),
            object("acl-stray", IncusKind::Acl),
            None,
            NOW,
        )
        .expect("the intent is journalled");

    assert_eq!(
        r.run_plan(&plan, &approval_of(&plan, NONCE), &mut verifier(), NOW),
        Err(ApprovalRefusal::Unresolved)
    );
    assert!(
        stub.asked().is_empty(),
        "nothing was deleted: {:?}",
        stub.asked()
    );
}

#[test]
fn a_subset_of_a_signed_plan_runs_without_a_new_approval() {
    let key = NodeKey::generate();
    let stub = Stub::new(&[]);
    let mut r = open(&stub, &key);
    known(&mut r, "e1");
    let first = record(
        &mut r,
        "e1",
        EffectClass::B,
        object("box-x", IncusKind::Instance),
    );
    record(
        &mut r,
        "e1",
        EffectClass::B,
        object("box-y", IncusKind::Instance),
    );
    let plan = awaiting(&r.reconcile(&signed(&key, 1, None, vec![]), NOW));
    assert_eq!(plan.objects.len(), 2);
    // The first object's record is settled outside the plan run, so the live objects are now a subset of the signed plan.
    r.journal.reverted(first);

    let run = r
        .run_plan(
            &plan.digest,
            &approval_of(&plan.digest, NONCE),
            &mut verifier(),
            NOW,
        )
        .expect("the subset runs under the approval of the signed plan");

    assert_eq!(
        run.deleted
            .iter()
            .map(|o| o.name.as_str())
            .collect::<Vec<_>>(),
        ["box-y"]
    );
    assert!(!run.partial);
    assert_eq!(stub.asked(), vec!["box-y".to_string()]);
    assert!(r.pending_plans().is_empty(), "a full run retires its plan");
}

#[test]
fn a_new_step_outside_the_signed_plan_is_refused_as_wrong_params() {
    let key = NodeKey::generate();
    let stub = Stub::new(&[]);
    let mut r = open(&stub, &key);
    known(&mut r, "e1");
    record(
        &mut r,
        "e1",
        EffectClass::B,
        object("box-x", IncusKind::Instance),
    );
    let plan = awaiting(&r.reconcile(&signed(&key, 1, None, vec![]), NOW)).digest;
    record(
        &mut r,
        "e1",
        EffectClass::B,
        object("box-new", IncusKind::Instance),
    );

    assert_eq!(
        r.run_plan(&plan, &approval_of(&plan, NONCE), &mut verifier(), NOW),
        Err(ApprovalRefusal::WrongParams)
    );
    assert!(stub.asked().is_empty(), "the refused run deleted nothing");
    assert_eq!(r.pending_plans().len(), 1, "the plan is still pending");
}

#[test]
fn a_class_c_emission_is_never_reverted() {
    let key = NodeKey::generate();
    let stub = Stub::new(&[]);
    let mut r = open(&stub, &key);
    known(&mut r, "e1");
    record(
        &mut r,
        "e1",
        EffectClass::C,
        object("mail-1", IncusKind::Instance),
    );

    let outcomes = r.reconcile(&signed(&key, 1, None, vec![]), NOW);

    assert!(
        outcomes.is_empty(),
        "an emission is not undone: {outcomes:?}"
    );
    assert!(stub.asked().is_empty());
    assert_eq!(
        r.journal.live(&EntryId("e1".to_string())).len(),
        1,
        "the emission still counts"
    );
}

#[test]
fn a_live_class_b_record_without_an_applied_entry_fails_closed() {
    let key = NodeKey::generate();
    let stub = Stub::new(&[]);
    let mut r = open(&stub, &key);
    record(
        &mut r,
        "e1",
        EffectClass::B,
        object("box-x", IncusKind::Instance),
    );

    let outcomes = r.reconcile(&signed(&key, 1, None, vec![]), NOW);

    assert_eq!(
        outcomes,
        vec![Outcome::Failed {
            entry: EntryId("e1".to_string()),
            why: "plugin not known".to_string(),
        }]
    );
    assert!(r.pending_plans().is_empty());
    assert!(stub.asked().is_empty());
}

#[test]
fn an_unchanged_applied_entry_is_not_applied_again_and_a_changed_one_is() {
    let key = NodeKey::generate();
    let stub = Stub::new(&[]);
    let mut r = open(&stub, &key);
    let first = signed(&key, 1, None, vec![dummy_entry("e1", json!({ "v": 1 }))]);
    assert_eq!(
        r.reconcile(&first, NOW),
        vec![Outcome::Applied {
            entry: EntryId("e1".to_string())
        }]
    );

    let same = signed(
        &key,
        2,
        Some(first.document.digest()),
        vec![dummy_entry("e1", json!({ "v": 1 }))],
    );
    assert!(
        r.reconcile(&same, NOW).is_empty(),
        "the unchanged entry is not applied again"
    );

    let changed = signed(
        &key,
        3,
        Some(same.document.digest()),
        vec![dummy_entry("e1", json!({ "v": 2 }))],
    );
    assert_eq!(
        r.reconcile(&changed, NOW),
        vec![Outcome::Applied {
            entry: EntryId("e1".to_string())
        }]
    );
}

/// An in-memory journal that counts every line appended to it, so a test can show a call wrote nothing.
#[derive(Clone, Default)]
struct Counting {
    inner: MemJournal,
    appended: Arc<Mutex<usize>>,
}

impl Counting {
    fn appended(&self) -> usize {
        *self.appended.lock().expect("the count is readable")
    }
}

impl JournalStore for Counting {
    fn append(&mut self, record: &EffectRecord) -> Result<(), KernelError> {
        *self.appended.lock().expect("the count is writable") += 1;
        self.inner.append(record)
    }

    fn load(&self) -> Result<(Vec<EffectRecord>, Option<String>), KernelError> {
        self.inner.load()
    }
}

/// The reconciler over a counting journal, with the retained set the test can reach.
fn open_counting(stub: &Stub, key: &NodeKey, store: &Counting) -> Reconciler<Counting> {
    let mut plugins = PluginSet::new();
    plugins.add::<Dummy>().expect("the dummy plugin is valid");
    let journal = Journal::open(store.clone()).expect("an empty journal opens");
    Reconciler::open(
        DesiredStateGate::new(key.public(), None),
        journal,
        plugins,
        Box::new(stub.clone()),
        RetainedSet::empty(),
        NOW,
    )
    .expect("an empty journal opens the reconciler")
}

/// One minute after a plan made at `NOW` has lapsed.
const LAPSED: u64 = NOW + PLAN_TTL_MS + 60_000;

/// The entry `e1`, applied with one class-B instance `box-x` and one class-A ACL, then removed at `NOW`: its plan waits.
fn removed_e1<S: JournalStore>(r: &mut Reconciler<S>, key: &NodeKey) -> (DeletePlan, u64) {
    known(r, "e1");
    record(r, "e1", EffectClass::A, object("acl-x", IncusKind::Acl));
    let instance = record(
        r,
        "e1",
        EffectClass::B,
        object("box-x", IncusKind::Instance),
    );
    let plan = awaiting(&r.reconcile(&signed(key, 1, None, vec![]), NOW));
    (plan, instance)
}

#[test]
fn a_lapsed_plan_is_renewed_with_the_same_id_made_now_and_its_run_deletes() {
    let key = NodeKey::generate();
    let stub = Stub::new(&[]);
    let mut r = open(&stub, &key);
    let (plan, _) = removed_e1(&mut r, &key);
    assert_eq!(
        r.run_plan_authorized(&plan.digest, LAPSED),
        Err(ApprovalRefusal::Expired),
        "the lapsed plan does not run"
    );

    let renewal = r.renew_lapsed(LAPSED);

    assert!(renewal.not_renewed.is_empty(), "{renewal:?}");
    assert_eq!(
        renewal.renewed,
        vec![DeletePlan {
            created_ms: LAPSED,
            ..plan.clone()
        }],
        "the same entry and objects, so the same id, made now"
    );
    assert_eq!(r.pending_plans(), renewal.renewed);
    let run = r
        .run_plan_authorized(&plan.digest, LAPSED)
        .expect("the renewed plan runs");
    assert_eq!(run.deleted, vec![object("box-x", IncusKind::Instance)]);
    assert!(!run.partial);
    assert!(r.pending_plans().is_empty(), "a full run retires its plan");
}

#[test]
fn a_plan_inside_its_fifteen_minutes_keeps_its_creation_time() {
    let key = NodeKey::generate();
    let stub = Stub::new(&[]);
    let mut r = open(&stub, &key);
    let (plan, _) = removed_e1(&mut r, &key);

    let renewal = r.renew_lapsed(NOW + PLAN_TTL_MS);

    assert!(renewal.renewed.is_empty(), "{renewal:?}");
    assert!(renewal.not_renewed.is_empty(), "{renewal:?}");
    assert_eq!(r.pending_plans(), vec![plan], "its age does not restart");
}

#[test]
fn a_declined_entry_is_never_renewed() {
    let key = NodeKey::generate();
    let stub = Stub::new(&[]);
    let mut r = open(&stub, &key);
    let (plan, _) = removed_e1(&mut r, &key);
    assert_eq!(
        r.decline(&plan.digest),
        vec![object("box-x", IncusKind::Instance)]
    );

    let renewal = r.renew_lapsed(LAPSED);

    assert!(renewal.renewed.is_empty(), "{renewal:?}");
    assert!(r.pending_plans().is_empty(), "no plan comes back for it");
}

#[test]
fn a_lapsed_plan_whose_entry_is_retained_meanwhile_is_not_renewed_and_says_why() {
    let key = NodeKey::generate();
    let stub = Stub::new(&[]);
    let mut r = open(&stub, &key);
    let (plan, _) = removed_e1(&mut r, &key);
    // The set is shared with the provider, so an entry can be retained while its plan still waits.
    r.retained
        .retain(
            EntryId("e1".to_string()),
            PluginId(DUMMY.to_string()),
            plan.digest.clone(),
        )
        .expect("the set is writable");

    let renewal = r.renew_lapsed(LAPSED);

    assert!(renewal.renewed.is_empty(), "{renewal:?}");
    assert_eq!(renewal.not_renewed.len(), 1, "{renewal:?}");
    assert_eq!(renewal.not_renewed[0].plan, plan);
    assert!(
        renewal.not_renewed[0].why.contains("declined"),
        "{renewal:?}"
    );
    assert_eq!(r.pending_plans(), vec![plan], "the lapsed plan is left as it was");
}

#[test]
fn an_entry_the_desired_state_carries_again_is_not_renewed() {
    let key = NodeKey::generate();
    let stub = Stub::new(&[]);
    let mut r = open(&stub, &key);
    let doc1 = signed(&key, 1, None, vec![]);
    known(&mut r, "e1");
    record(
        &mut r,
        "e1",
        EffectClass::B,
        object("box-x", IncusKind::Instance),
    );
    awaiting(&r.reconcile(&doc1, NOW));
    let doc2 = signed(
        &key,
        2,
        Some(doc1.document.digest()),
        vec![dummy_entry("e1", json!({}))],
    );
    r.reconcile(&doc2, LAPSED);
    assert!(r.pending_plans().is_empty(), "the plan is withdrawn as today");

    let renewal = r.renew_lapsed(LAPSED);

    assert!(renewal.renewed.is_empty(), "{renewal:?}");
    assert!(renewal.not_renewed.is_empty(), "{renewal:?}");
    assert!(r.pending_plans().is_empty());
}

#[test]
fn an_entry_whose_objects_are_all_gone_leaves_no_plan() {
    let key = NodeKey::generate();
    let stub = Stub::new(&[]);
    let mut r = open(&stub, &key);
    let (plan, instance) = removed_e1(&mut r, &key);
    // The instance's record is settled outside a run, so the journal holds nothing of the entry's to delete.
    r.journal.reverted(instance);

    let renewal = r.renew_lapsed(LAPSED);

    assert!(renewal.renewed.is_empty(), "{renewal:?}");
    assert_eq!(renewal.not_renewed.len(), 1, "{renewal:?}");
    assert_eq!(renewal.not_renewed[0].plan, plan);
    assert!(r.pending_plans().is_empty(), "no plan is left for it");
}

#[test]
fn an_unresolved_intent_renews_nothing_and_says_why() {
    let key = NodeKey::generate();
    let stub = Stub::new(&[]);
    let mut r = open(&stub, &key);
    let (plan, _) = removed_e1(&mut r, &key);
    r.journal
        .intend(
            &EntryId("e9".to_string()),
            EffectClass::A,
            Digest::of(b"other"),
            object("acl-stray", IncusKind::Acl),
            None,
            NOW,
        )
        .expect("the intent is journalled");

    let renewal = r.renew_lapsed(LAPSED);

    assert!(renewal.renewed.is_empty(), "{renewal:?}");
    assert_eq!(renewal.not_renewed.len(), 1, "{renewal:?}");
    assert_eq!(renewal.not_renewed[0].plan, plan);
    assert!(
        renewal.not_renewed[0].why.contains("unresolved"),
        "{renewal:?}"
    );
    assert_eq!(r.pending_plans(), vec![plan], "the lapsed plan is left as it was");
}

#[test]
fn a_renewal_writes_nothing_to_the_provider_or_the_journal() {
    let key = NodeKey::generate();
    let stub = Stub::new(&[]);
    let store = Counting::default();
    let mut r = open_counting(&stub, &key, &store);
    let (plan, _) = removed_e1(&mut r, &key);
    let asked = stub.asked();
    let appended = store.appended();
    let live = r.journal.live(&EntryId("e1".to_string()));

    let renewal = r.renew_lapsed(LAPSED);

    assert_eq!(renewal.renewed.len(), 1, "{renewal:?}");
    assert_eq!(renewal.renewed[0].digest, plan.digest);
    assert_eq!(stub.asked(), asked, "no retirement was asked for");
    assert_eq!(store.appended(), appended, "no journal line was written");
    assert_eq!(
        r.journal.live(&EntryId("e1".to_string())),
        live,
        "no record of the entry moved"
    );
}
