//! The plugin contract: `Ctx` runs every effect plan, then intent, then apply, then outcome. Every refusal is checked before
//! anything is journalled, a config that does not deserialize never reaches apply, and no config value reaches an error.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;

use arca_kernel::KernelError;
use arca_kernel::canon::Digest;
use arca_kernel::desired::{EntryId, PluginId};
use arca_kernel::journal::{EffectClass, Inverse, Journal, JournalSink, MemJournal};
use arca_kernel::ownership::{IncusKind, ObjectRef};
use arca_kernel::plugin::{
    Ctx, EffectOp, EffectRefusal, Erased, ErasedPlugin, KeySpec, Manifest, Observe, Planned,
    Plugin, PluginError, Provider, op_digest,
};
use serde::Deserialize;
use serde_json::{Value, json};

type Log = Rc<RefCell<Vec<&'static str>>>;

fn entry() -> EntryId {
    EntryId("watch".to_string())
}

fn manifest(effects: Vec<(IncusKind, EffectClass)>, projects: &[&str], observes: bool) -> Manifest {
    Manifest {
        id: PluginId("arca.test".to_string()),
        version: "0.1.0".to_string(),
        provides: vec![KeySpec {
            key: "watch.report".to_string(),
            version: "1".to_string(),
        }],
        effects,
        projects: projects.iter().map(|p| p.to_string()).collect(),
        observes,
    }
}

fn create(project: &str, name: &str) -> EffectOp {
    EffectOp::IncusCreate {
        kind: IncusKind::Instance,
        project: project.to_string(),
        name: name.to_string(),
        body: json!({"type": "container"}),
    }
}

fn patch(project: &str, name: &str, pairs: &[(&str, Option<&str>)]) -> EffectOp {
    EffectOp::IncusPatchConfig {
        object: ObjectRef {
            kind: IncusKind::Instance,
            project: project.to_string(),
            name: name.to_string(),
            uuid: Some("u-9".to_string()),
        },
        config: pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.map(str::to_string)))
            .collect(),
    }
}

/// A provider that records its calls in the shared log, and answers as configured.
struct Host {
    log: Log,
    plans: Cell<usize>,
    effects: usize,
    plan_class: EffectClass,
    plan_digest: Option<Digest>,
    pre_image: Option<BTreeMap<String, Option<String>>>,
    refuse_plan: Option<EffectRefusal>,
    refuse_effect: Option<EffectRefusal>,
    answer_object: Option<ObjectRef>,
    observed: Value,
}

impl Host {
    fn new(log: &Log) -> Host {
        Host {
            log: log.clone(),
            plans: Cell::new(0),
            effects: 0,
            plan_class: EffectClass::A,
            plan_digest: None,
            pre_image: None,
            refuse_plan: None,
            refuse_effect: None,
            answer_object: None,
            observed: json!({"instances": []}),
        }
    }
}

impl Provider for Host {
    fn observe(&self, _query: &Observe) -> Result<Value, EffectRefusal> {
        Ok(self.observed.clone())
    }

    fn plan(&self, _entry: &EntryId, op: &EffectOp) -> Result<Planned, EffectRefusal> {
        self.plans.set(self.plans.get() + 1);
        self.log.borrow_mut().push("plan");
        if let Some(refusal) = &self.refuse_plan {
            return Err(refusal.clone());
        }
        Ok(Planned {
            class: self.plan_class,
            op_digest: self.plan_digest.clone().unwrap_or_else(|| op_digest(op)),
            pre_image: self.pre_image.clone(),
            etag: None,
        })
    }

    fn effect(
        &mut self,
        _entry: &EntryId,
        op: &EffectOp,
        planned: &Planned,
    ) -> Result<(ObjectRef, Inverse), EffectRefusal> {
        self.effects += 1;
        self.log.borrow_mut().push("effect");
        if let Some(refusal) = &self.refuse_effect {
            return Err(refusal.clone());
        }
        let object = match (&self.answer_object, op) {
            (Some(object), _) => object.clone(),
            (
                None,
                EffectOp::IncusCreate {
                    kind,
                    project,
                    name,
                    ..
                },
            ) => ObjectRef {
                kind: *kind,
                project: project.clone(),
                name: name.clone(),
                uuid: Some("u-1".to_string()),
            },
            (None, EffectOp::IncusPatchConfig { object, .. })
            | (None, EffectOp::IncusDelete { object, .. }) => object.clone(),
        };
        let inverse = match op {
            EffectOp::IncusCreate { .. } => Inverse::IncusDelete {
                object: object.clone(),
            },
            EffectOp::IncusPatchConfig { .. } => Inverse::IncusRestoreConfig {
                object: object.clone(),
                config: planned.pre_image.clone().unwrap_or_default(),
            },
            EffectOp::IncusDelete { .. } => Inverse::Emission,
        };
        Ok((object, inverse))
    }
}

/// A journal that logs each sink call in the shared log, and refuses the intent when asked to.
struct Sink {
    log: Log,
    journal: Journal<MemJournal>,
    refuse_intend: bool,
}

impl Sink {
    fn new(log: &Log) -> Sink {
        Sink {
            log: log.clone(),
            journal: Journal::open(MemJournal::new()).expect("opens"),
            refuse_intend: false,
        }
    }
}

impl JournalSink for Sink {
    fn intend(
        &mut self,
        entry: &EntryId,
        class: EffectClass,
        op_digest: Digest,
        now_ms: u64,
    ) -> Result<u64, KernelError> {
        self.log.borrow_mut().push("intend");
        if self.refuse_intend {
            return Err(KernelError::Io {
                path: "journal".to_string(),
                why: "the write was refused".to_string(),
            });
        }
        self.journal.intend(entry, class, op_digest, now_ms)
    }

    fn applied(&mut self, id: u64, object: ObjectRef, inverse: Inverse) -> Result<(), KernelError> {
        self.log.borrow_mut().push("applied");
        self.journal.applied(id, object, inverse)
    }

    fn failed(&mut self, id: u64, why: &str) -> Result<(), KernelError> {
        self.log.borrow_mut().push("failed");
        self.journal.failed(id, why)
    }
}

fn new_log() -> Log {
    Rc::new(RefCell::new(Vec::new()))
}

// Ordering and the journal's record of each effect.

#[test]
fn an_effect_is_planned_then_intended_then_applied_then_recorded_with_its_inverse() {
    let log = new_log();
    let manifest = manifest(
        vec![(IncusKind::Instance, EffectClass::A)],
        &["arcak-svc"],
        false,
    );
    let mut host = Host::new(&log);
    let mut sink = Sink::new(&log);
    let op = create("arcak-svc", "web1");
    let expected_digest = op_digest(&op);
    let object = {
        let mut ctx = Ctx::new(entry(), &manifest, &mut host, &mut sink, 42);
        ctx.effect(op).expect("the effect applies")
    };
    assert_eq!(
        *log.borrow(),
        vec!["plan", "intend", "effect", "applied"],
        "the plan comes first, the intent is written before the provider applies, and the outcome after"
    );
    assert_eq!(object.uuid.as_deref(), Some("u-1"));
    let live = sink.journal.live(&entry());
    assert_eq!(live.len(), 1);
    assert_eq!(
        live[0].op_digest, expected_digest,
        "the intent names the op it ran"
    );
    assert_eq!(live[0].at_ms, 42);
    assert_eq!(live[0].object, Some(object.clone()));
    assert_eq!(live[0].inverse, Some(Inverse::IncusDelete { object }));
}

#[test]
fn the_intent_is_on_the_journal_before_the_provider_applies() {
    // The sink's log shows the intent before the provider's effect; `effect` is the only call that writes.
    let log = new_log();
    let manifest = manifest(
        vec![(IncusKind::Instance, EffectClass::A)],
        &["arcak-svc"],
        false,
    );
    let mut host = Host::new(&log);
    let mut sink = Sink::new(&log);
    {
        let mut ctx = Ctx::new(entry(), &manifest, &mut host, &mut sink, 1);
        ctx.effect(create("arcak-svc", "web1")).expect("applies");
    }
    let order = log.borrow().clone();
    let intend = order
        .iter()
        .position(|e| *e == "intend")
        .expect("intent logged");
    let effect = order
        .iter()
        .position(|e| *e == "effect")
        .expect("effect logged");
    assert!(intend < effect, "order: {order:?}");
    assert_eq!(host.effects, 1);
}

#[test]
fn a_patch_records_the_pre_image_as_its_inverse() {
    // The provider holds a=old and no b. The patch sets a=new and b=x, so the inverse restores a to old and removes b.
    let log = new_log();
    let manifest = manifest(
        vec![(IncusKind::Instance, EffectClass::A)],
        &["arcak-svc"],
        false,
    );
    let mut host = Host::new(&log);
    host.pre_image = Some(BTreeMap::from([
        ("a".to_string(), Some("old".to_string())),
        ("b".to_string(), None),
    ]));
    let mut sink = Sink::new(&log);
    {
        let mut ctx = Ctx::new(entry(), &manifest, &mut host, &mut sink, 1);
        ctx.effect(patch(
            "arcak-svc",
            "web1",
            &[("a", Some("new")), ("b", Some("x"))],
        ))
        .expect("applies");
    }
    let live = sink.journal.live(&entry());
    assert_eq!(
        live[0].inverse,
        Some(Inverse::IncusRestoreConfig {
            object: ObjectRef {
                kind: IncusKind::Instance,
                project: "arcak-svc".to_string(),
                name: "web1".to_string(),
                uuid: Some("u-9".to_string()),
            },
            config: BTreeMap::from([
                ("a".to_string(), Some("old".to_string())),
                ("b".to_string(), None),
            ]),
        })
    );
}

// Refusals that come before the intent write nothing.

#[test]
fn an_undeclared_kind_is_refused_before_any_plan_and_writes_nothing() {
    let log = new_log();
    let manifest = manifest(
        vec![(IncusKind::Instance, EffectClass::A)],
        &["arcak-svc"],
        false,
    );
    let mut host = Host::new(&log);
    let mut sink = Sink::new(&log);
    let refusal = {
        let mut ctx = Ctx::new(entry(), &manifest, &mut host, &mut sink, 1);
        ctx.effect(EffectOp::IncusCreate {
            kind: IncusKind::Network,
            project: "arcak-svc".to_string(),
            name: "net1".to_string(),
            body: json!({}),
        })
    };
    assert_eq!(refusal, Err(EffectRefusal::NotDeclared));
    assert_eq!(host.plans.get(), 0, "no plan was asked for");
    assert!(log.borrow().is_empty());
    assert!(sink.journal.live(&entry()).is_empty());
    assert!(sink.journal.unresolved().is_empty());
}

#[test]
fn an_undeclared_project_is_refused_before_any_plan_and_writes_nothing() {
    let log = new_log();
    let manifest = manifest(
        vec![(IncusKind::Instance, EffectClass::A)],
        &["arcak-svc"],
        false,
    );
    let mut host = Host::new(&log);
    let mut sink = Sink::new(&log);
    let refusal = {
        let mut ctx = Ctx::new(entry(), &manifest, &mut host, &mut sink, 1);
        ctx.effect(create("bench", "web1"))
    };
    assert_eq!(refusal, Err(EffectRefusal::ProjectNotAllowed));
    assert_eq!(host.plans.get(), 0);
    assert!(log.borrow().is_empty());
}

#[test]
fn a_watch_only_manifest_refuses_every_effect_before_any_plan() {
    let log = new_log();
    let manifest = manifest(Vec::new(), &["arcak-svc"], true);
    let mut host = Host::new(&log);
    let mut sink = Sink::new(&log);
    let refusal = {
        let mut ctx = Ctx::new(entry(), &manifest, &mut host, &mut sink, 1);
        ctx.effect(create("arcak-svc", "web1"))
    };
    assert_eq!(refusal, Err(EffectRefusal::WatchOnly));
    assert_eq!(host.plans.get(), 0);
    assert!(log.borrow().is_empty());
}

#[test]
fn a_plan_in_an_undeclared_class_is_refused_with_no_intent() {
    let log = new_log();
    let manifest = manifest(
        vec![(IncusKind::Instance, EffectClass::A)],
        &["arcak-svc"],
        false,
    );
    let mut host = Host::new(&log);
    host.plan_class = EffectClass::B;
    let mut sink = Sink::new(&log);
    let refusal = {
        let mut ctx = Ctx::new(entry(), &manifest, &mut host, &mut sink, 1);
        ctx.effect(create("arcak-svc", "db1"))
    };
    assert_eq!(refusal, Err(EffectRefusal::NotDeclared));
    assert_eq!(
        *log.borrow(),
        vec!["plan"],
        "planned, then refused before any intent"
    );
    assert_eq!(host.effects, 0, "the provider never applied anything");
    assert!(sink.journal.unresolved().is_empty());
}

#[test]
fn a_plan_refusal_writes_no_record() {
    let log = new_log();
    let manifest = manifest(
        vec![(IncusKind::Instance, EffectClass::A)],
        &["arcak-svc"],
        false,
    );
    let mut host = Host::new(&log);
    host.refuse_plan = Some(EffectRefusal::Stale);
    let mut sink = Sink::new(&log);
    let refusal = {
        let mut ctx = Ctx::new(entry(), &manifest, &mut host, &mut sink, 1);
        ctx.effect(create("arcak-svc", "web1"))
    };
    assert_eq!(refusal, Err(EffectRefusal::Stale));
    assert_eq!(*log.borrow(), vec!["plan"]);
    assert_eq!(host.effects, 0);
}

#[test]
fn a_plan_that_names_another_op_is_refused_with_no_intent() {
    let log = new_log();
    let manifest = manifest(
        vec![(IncusKind::Instance, EffectClass::A)],
        &["arcak-svc"],
        false,
    );
    let mut host = Host::new(&log);
    host.plan_digest = Some(Digest::of(b"some other op"));
    let mut sink = Sink::new(&log);
    let refusal = {
        let mut ctx = Ctx::new(entry(), &manifest, &mut host, &mut sink, 1);
        ctx.effect(create("arcak-svc", "web1"))
    };
    assert!(matches!(refusal, Err(EffectRefusal::Provider(_))));
    assert_eq!(
        *log.borrow(),
        vec!["plan"],
        "a plan for another op journals nothing"
    );
    assert_eq!(host.effects, 0);
}

#[test]
fn a_project_kind_whose_name_is_not_its_project_is_refused() {
    let log = new_log();
    let manifest = manifest(
        vec![(IncusKind::Project, EffectClass::A)],
        &["arcak-svc"],
        false,
    );
    let mut host = Host::new(&log);
    let mut sink = Sink::new(&log);
    let refusal = {
        let mut ctx = Ctx::new(entry(), &manifest, &mut host, &mut sink, 1);
        ctx.effect(EffectOp::IncusCreate {
            kind: IncusKind::Project,
            project: "arcak-svc".to_string(),
            name: "arcak-other".to_string(),
            body: json!({}),
        })
    };
    assert!(matches!(refusal, Err(EffectRefusal::Provider(_))));
    assert_eq!(host.plans.get(), 0);
}

#[test]
fn a_journal_that_refuses_the_intent_means_the_effect_never_runs() {
    let log = new_log();
    let manifest = manifest(
        vec![(IncusKind::Instance, EffectClass::A)],
        &["arcak-svc"],
        false,
    );
    let mut host = Host::new(&log);
    let mut sink = Sink::new(&log);
    sink.refuse_intend = true;
    let refusal = {
        let mut ctx = Ctx::new(entry(), &manifest, &mut host, &mut sink, 1);
        ctx.effect(create("arcak-svc", "web1"))
    };
    assert!(matches!(refusal, Err(EffectRefusal::Provider(_))));
    assert_eq!(host.effects, 0, "no intent, no effect");
    assert_eq!(*log.borrow(), vec!["plan", "intend"]);
}

// Refusals after the intent are journalled, and the intent is resolved or left open as the rules say.

#[test]
fn a_provider_refusal_is_journalled_as_failed_and_leaves_nothing_unresolved() {
    let log = new_log();
    let manifest = manifest(
        vec![(IncusKind::Instance, EffectClass::A)],
        &["arcak-svc"],
        false,
    );
    let mut host = Host::new(&log);
    host.refuse_effect = Some(EffectRefusal::Provider("the instance is busy".to_string()));
    let mut sink = Sink::new(&log);
    let refusal = {
        let mut ctx = Ctx::new(entry(), &manifest, &mut host, &mut sink, 1);
        ctx.effect(create("arcak-svc", "web1"))
    };
    assert_eq!(
        refusal,
        Err(EffectRefusal::Provider("the instance is busy".to_string()))
    );
    assert_eq!(*log.borrow(), vec!["plan", "intend", "effect", "failed"]);
    assert!(
        sink.journal.unresolved().is_empty(),
        "a failed intent is resolved"
    );
    assert!(sink.journal.live(&entry()).is_empty());
}

#[test]
fn a_provider_answer_for_another_object_leaves_the_intent_open_for_recovery() {
    let log = new_log();
    let manifest = manifest(
        vec![(IncusKind::Instance, EffectClass::A)],
        &["arcak-svc"],
        false,
    );
    let mut host = Host::new(&log);
    host.answer_object = Some(ObjectRef {
        kind: IncusKind::Instance,
        project: "arcak-svc".to_string(),
        name: "someone-else".to_string(),
        uuid: Some("u-x".to_string()),
    });
    let mut sink = Sink::new(&log);
    let refusal = {
        let mut ctx = Ctx::new(entry(), &manifest, &mut host, &mut sink, 1);
        ctx.effect(create("arcak-svc", "web1"))
    };
    assert!(matches!(refusal, Err(EffectRefusal::Provider(_))));
    assert_eq!(
        *log.borrow(),
        vec!["plan", "intend", "effect"],
        "neither an outcome nor a failure is written: the effect may have happened"
    );
    assert_eq!(sink.journal.unresolved().len(), 1);
}

#[test]
fn an_outcome_the_journal_refuses_is_returned_as_a_refusal() {
    // The provider applies, but the journal refuses the outcome. The caller learns that the journal failed.
    struct RefusingOutcome(Journal<MemJournal>);
    impl JournalSink for RefusingOutcome {
        fn intend(
            &mut self,
            e: &EntryId,
            c: EffectClass,
            d: Digest,
            now: u64,
        ) -> Result<u64, KernelError> {
            self.0.intend(e, c, d, now)
        }
        fn applied(&mut self, _id: u64, _o: ObjectRef, _i: Inverse) -> Result<(), KernelError> {
            Err(KernelError::Journal(
                "the outcome could not be written".to_string(),
            ))
        }
        fn failed(&mut self, id: u64, why: &str) -> Result<(), KernelError> {
            self.0.failed(id, why)
        }
    }
    let log = new_log();
    let manifest = manifest(
        vec![(IncusKind::Instance, EffectClass::A)],
        &["arcak-svc"],
        false,
    );
    let mut host = Host::new(&log);
    let mut sink = RefusingOutcome(Journal::open(MemJournal::new()).expect("opens"));
    let refusal = {
        let mut ctx = Ctx::new(entry(), &manifest, &mut host, &mut sink, 1);
        ctx.effect(create("arcak-svc", "web1"))
    };
    assert!(matches!(refusal, Err(EffectRefusal::Provider(_))));
    assert_eq!(
        host.effects, 1,
        "the provider did apply, so the open intent is for recovery"
    );
    assert_eq!(sink.0.unresolved().len(), 1);
}

// The read side and the provided keys.

#[test]
fn observe_is_refused_unless_the_manifest_observes() {
    let log = new_log();
    let host = Host::new(&log);
    let blind = manifest(Vec::new(), &["arcak-svc"], false);
    let mut blind_host = host;
    let mut sink = Sink::new(&log);
    {
        let ctx = Ctx::new(entry(), &blind, &mut blind_host, &mut sink, 1);
        assert_eq!(
            ctx.observe(Observe::HostAddresses),
            Err(EffectRefusal::NotDeclared)
        );
    }
    let watching = manifest(Vec::new(), &["arcak-svc"], true);
    let mut host = Host::new(&log);
    let mut sink = Sink::new(&log);
    let ctx = Ctx::new(entry(), &watching, &mut host, &mut sink, 1);
    let answer = ctx
        .observe(Observe::IncusGet {
            path: "/1.0/instances".to_string(),
        })
        .expect("an observing plugin reads");
    assert_eq!(answer, json!({"instances": []}));
}

#[test]
fn provide_accepts_only_a_declared_key_once_and_returns_it_at_the_end() {
    let log = new_log();
    let manifest = manifest(Vec::new(), &["arcak-svc"], true);
    let mut host = Host::new(&log);
    let mut sink = Sink::new(&log);
    let mut ctx = Ctx::new(entry(), &manifest, &mut host, &mut sink, 1);
    assert_eq!(
        ctx.provide("watch.undeclared", json!(1)),
        Err(EffectRefusal::NotDeclared)
    );
    ctx.provide("watch.report", json!({"ok": true}))
        .expect("declared key");
    assert!(matches!(
        ctx.provide("watch.report", json!({"ok": false})),
        Err(EffectRefusal::Provider(_))
    ));
    let provided = ctx.provided();
    assert_eq!(provided.get("watch.report"), Some(&json!({"ok": true})));
    assert_eq!(provided.len(), 1);
}

// The typed plugin, erased.

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Cfg {
    count: u32,
    name: String,
}

struct Web;

impl Plugin for Web {
    const ID: &'static str = "arca.test";
    type Config = Cfg;

    fn manifest() -> Manifest {
        manifest(
            vec![(IncusKind::Instance, EffectClass::A)],
            &["arcak-svc"],
            false,
        )
    }

    fn apply(ctx: &mut Ctx<'_>, cfg: &Cfg) -> Result<(), PluginError> {
        for _ in 0..cfg.count {
            ctx.effect(create("arcak-svc", &cfg.name))?;
        }
        Ok(())
    }
}

#[test]
fn a_config_that_does_not_deserialize_never_reaches_apply_and_its_value_is_not_in_the_error() {
    let log = new_log();
    let mut host = Host::new(&log);
    let mut sink = Sink::new(&log);
    let plugin: Box<dyn ErasedPlugin> = Box::new(Erased::<Web>::new());
    let result = {
        let web = Web::manifest();
        let mut ctx = Ctx::new(entry(), &web, &mut host, &mut sink, 1);
        plugin.apply(
            &mut ctx,
            &json!({"count": "secret-token-value", "name": "web1"}),
        )
    };
    let Err(PluginError::Config(text)) = result else {
        panic!("a bad config is a Config error, got {result:?}");
    };
    assert!(
        !text.contains("secret-token-value"),
        "the config value must not reach the error: {text}"
    );
    assert_eq!(host.plans.get(), 0);
    assert!(log.borrow().is_empty());
}

#[test]
fn a_rejected_boolean_value_is_redacted_too() {
    let log = new_log();
    let mut host = Host::new(&log);
    let mut sink = Sink::new(&log);
    let plugin = Erased::<Web>::new();
    let result = {
        let web = Web::manifest();
        let mut ctx = Ctx::new(entry(), &web, &mut host, &mut sink, 1);
        plugin.apply(&mut ctx, &json!({"count": true, "name": "web1"}))
    };
    let Err(PluginError::Config(text)) = result else {
        panic!("a bad config is a Config error, got {result:?}");
    };
    assert_eq!(
        text, "invalid type: boolean …, expected u32",
        "the value is replaced, and the expected type is kept"
    );
}

#[test]
fn a_missing_field_names_the_field_and_nothing_else() {
    let log = new_log();
    let mut host = Host::new(&log);
    let mut sink = Sink::new(&log);
    let plugin = Erased::<Web>::new();
    let result = {
        let web = Web::manifest();
        let mut ctx = Ctx::new(entry(), &web, &mut host, &mut sink, 1);
        plugin.apply(&mut ctx, &json!({"count": 1}))
    };
    let Err(PluginError::Config(text)) = result else {
        panic!("a missing field is a Config error, got {result:?}");
    };
    assert!(text.contains("name"), "the missing field is named: {text}");
    assert_eq!(host.plans.get(), 0);
}

#[test]
fn a_valid_config_applies_through_ctx_and_journals_each_effect() {
    let log = new_log();
    let mut host = Host::new(&log);
    let mut sink = Sink::new(&log);
    let plugin = Erased::<Web>::new();
    {
        let web = Web::manifest();
        let mut ctx = Ctx::new(entry(), &web, &mut host, &mut sink, 1);
        plugin
            .apply(&mut ctx, &json!({"count": 2, "name": "web1"}))
            .expect("a valid config applies");
    }
    assert_eq!(host.effects, 2);
    assert_eq!(sink.journal.live(&entry()).len(), 2);
}

#[test]
fn an_erased_plugin_is_object_safe_and_reports_its_id_and_manifest() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Box<dyn ErasedPlugin>>();
    let boxed: Box<dyn ErasedPlugin> = Box::new(Erased::<Web>::default());
    assert_eq!(boxed.id(), Web::ID);
    assert_eq!(
        boxed.manifest().id.0,
        Web::ID,
        "a manifest names the plugin it belongs to"
    );
    assert!(
        PluginId::parse(Web::ID).is_some(),
        "a plugin id is a valid id"
    );
}
