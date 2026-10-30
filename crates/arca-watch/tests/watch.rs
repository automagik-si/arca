//! The watch plugin against an in-memory provider that answers the one listing from the fixture. The plugin's tests need no
//! Incus crate, so the in-memory provider is the only host this file meets.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};

use arca_api::{conforms, verb};
use arca_kernel::canon::Digest;
use arca_kernel::desired::{EntryId, PluginId};
use arca_kernel::journal::{Journal, MemJournal};
use arca_kernel::ownership::{IncusKind, ObjectRef, OwnerClass};
use arca_kernel::plugin::{
    Ctx, EffectOp, EffectRefusal, Erased, ErasedPlugin, Observe, Planned, Plugin, PluginError,
    Provider,
};
use arca_watch::report::{Drift, WatchReport, config_hash};
use arca_watch::{INSTANCES_PATH, Watch, WatchConfig, watch_once};
use serde_json::{Value, json};

const FIXTURE: &str = include_str!("../../arca-incus/tests/fixtures/state.json");

fn fixture_listing() -> Value {
    let state: Value = serde_json::from_str(FIXTURE).expect("the fixture is JSON");
    state["instances"].clone()
}

fn all_projects() -> BTreeSet<String> {
    ["default", "droid", "bench", "svc"]
        .iter()
        .map(|p| p.to_string())
        .collect()
}

/// Answers the listing with whatever rows it was given, unscoped, so the plugin's own scoping is what the tests check.
struct Listing {
    rows: Value,
    plans: Cell<usize>,
    effects: Cell<usize>,
}

impl Listing {
    fn of(rows: Value) -> Listing {
        Listing {
            rows,
            plans: Cell::new(0),
            effects: Cell::new(0),
        }
    }
}

impl Provider for Listing {
    fn observe(
        &self,
        _entry: &EntryId,
        _projects: &[String],
        query: &Observe,
    ) -> Result<Value, EffectRefusal> {
        match query {
            Observe::IncusGet { path } if path == INSTANCES_PATH => Ok(self.rows.clone()),
            _ => Err(EffectRefusal::NotDeclared),
        }
    }

    fn plan(&self, _entry: &EntryId, _op: &EffectOp) -> Result<Planned, EffectRefusal> {
        self.plans.set(self.plans.get() + 1);
        Err(EffectRefusal::WatchOnly)
    }

    fn effect(
        &mut self,
        _entry: &EntryId,
        _op: &EffectOp,
        _planned: &Planned,
    ) -> Result<(ObjectRef, arca_kernel::journal::Inverse), EffectRefusal> {
        self.effects.set(self.effects.get() + 1);
        Err(EffectRefusal::WatchOnly)
    }
}

fn run(rows: Value, cfg: &WatchConfig) -> WatchReport {
    let mut host = Listing::of(rows);
    watch_once(&mut host, cfg, 42).expect("the watch runs")
}

fn default_cfg() -> WatchConfig {
    WatchConfig {
        projects: all_projects(),
        complete: true,
        ..WatchConfig::default()
    }
}

fn find<'a>(report: &'a WatchReport, project: &str, name: &str) -> &'a arca_watch::InstanceRow {
    report
        .projects
        .iter()
        .filter(|p| p.project == project)
        .flat_map(|p| p.instances.iter())
        .find(|i| i.name == name)
        .unwrap_or_else(|| panic!("{project}/{name} is in the report"))
}

fn every_row(report: &WatchReport) -> Vec<(String, String)> {
    report
        .projects
        .iter()
        .flat_map(|p| {
            p.instances
                .iter()
                .map(move |i| (p.project.clone(), i.name.clone()))
        })
        .collect()
}

fn entry(name: &str) -> EntryId {
    EntryId(name.to_string())
}

fn digest() -> Digest {
    Digest::of(b"a plan")
}

fn instance(project: &str, name: &str, config: Value, expanded: Value) -> Value {
    json!({
        "name": name,
        "project": project,
        "type": "container",
        "status": "Running",
        "config": config,
        "expanded_config": expanded,
    })
}

#[test]
fn every_fixture_instance_appears_once_with_its_class() {
    let report = run(fixture_listing(), &default_cfg());
    let rows = every_row(&report);
    assert_eq!(rows.len(), 6, "six fixture instances: {rows:?}");
    let unique: BTreeSet<_> = rows.iter().collect();
    assert_eq!(unique.len(), 6, "no instance appears twice");

    assert_eq!(find(&report, "svc", "db1").owner, OwnerClass::Droidd);
    assert_eq!(find(&report, "droid", "phone1").owner, OwnerClass::Droidd);
    assert_eq!(find(&report, "droid", "desk1").owner, OwnerClass::Droidd);
    assert_eq!(
        find(&report, "default", "guest1").owner,
        OwnerClass::Foreign
    );
    assert_eq!(
        find(&report, "svc", "k1").owner,
        OwnerClass::Kernel {
            entry: entry("toy")
        }
    );
    assert_eq!(find(&report, "svc", "x1").owner, OwnerClass::Contested);
}

#[test]
fn privileged_comes_from_the_expanded_config_and_identity_from_the_phone_names() {
    let cfg = WatchConfig {
        identity_phones: ["phone1".to_string()].into_iter().collect(),
        projects: all_projects(),
        complete: true,
        ..WatchConfig::default()
    };
    let report = run(fixture_listing(), &cfg);
    let phone = find(&report, "droid", "phone1");
    assert!(
        phone.privileged,
        "security.privileged is in the expanded config only"
    );
    assert!(phone.identity, "phone1 is an identity phone");
    let desk = find(&report, "droid", "desk1");
    assert!(!desk.privileged);
    assert!(!desk.identity, "desk1 is not named");
}

#[test]
fn an_unlisted_kernel_entry_is_reported_as_marked_without_entry() {
    let report = run(fixture_listing(), &default_cfg());
    assert!(
        report.drift.contains(&Drift::MarkedWithoutEntry {
            project: "svc".to_string(),
            name: "k1".to_string(),
            entry: Some(entry("toy")),
        }),
        "k1's entry is not live: {:?}",
        report.drift
    );
}

#[test]
fn a_live_kernel_entry_with_its_object_gives_no_drift() {
    let cfg = WatchConfig {
        live_entries: [entry("toy")].into_iter().collect(),
        projects: all_projects(),
        complete: true,
        ..WatchConfig::default()
    };
    let report = run(fixture_listing(), &cfg);
    assert!(
        !report
            .drift
            .iter()
            .any(|d| matches!(d, Drift::MarkedWithoutEntry { .. })),
        "toy is live, so k1 and x1 are not drift: {:?}",
        report.drift
    );
    assert!(
        !report
            .drift
            .iter()
            .any(|d| matches!(d, Drift::EntryWithoutObject { .. })),
        "toy has objects: {:?}",
        report.drift
    );
}

#[test]
fn a_live_entry_with_no_object_is_entry_without_object() {
    let cfg = WatchConfig {
        live_entries: [entry("ghost")].into_iter().collect(),
        projects: all_projects(),
        complete: true,
        ..WatchConfig::default()
    };
    let report = run(fixture_listing(), &cfg);
    assert!(
        report.drift.contains(&Drift::EntryWithoutObject {
            entry: entry("ghost")
        }),
        "ghost has no object: {:?}",
        report.drift
    );
}

#[test]
fn a_retained_entry_is_owned_and_not_drift() {
    let plan = digest();
    let cfg = WatchConfig {
        retained: [(
            entry("toy"),
            (PluginId("arca.test".to_string()), plan.clone()),
        )]
        .into_iter()
        .collect(),
        projects: all_projects(),
        complete: true,
        ..WatchConfig::default()
    };
    let report = run(fixture_listing(), &cfg);
    assert_eq!(
        find(&report, "svc", "k1").owner,
        OwnerClass::Retained {
            entry: entry("toy"),
            plugin: PluginId("arca.test".to_string()),
            plan,
        }
    );
    assert!(
        !report
            .drift
            .iter()
            .any(|d| matches!(d, Drift::MarkedWithoutEntry { name, .. } if name == "k1")),
        "a retained object is shown as owned, not drift: {:?}",
        report.drift
    );
}

#[test]
fn a_tombstone_only_object_is_drift_with_an_empty_entry() {
    let rows = json!([instance(
        "svc",
        "t1",
        json!({ "user.arcakernel.retiring": "1" }),
        json!({ "user.arcakernel.retiring": "1" }),
    )]);
    let report = run(rows, &default_cfg());
    assert_eq!(find(&report, "svc", "t1").owner, OwnerClass::Foreign);
    assert!(report.drift.contains(&Drift::MarkedWithoutEntry {
        project: "svc".to_string(),
        name: "t1".to_string(),
        entry: None,
    }));
    let wire = serde_json::to_value(&report.drift).expect("drift serializes");
    assert_eq!(wire[0]["kind"], json!("markedWithoutEntry"));
    assert_eq!(wire[0]["entry"], json!(""), "a tombstone names no entry");
}

#[test]
fn a_bad_mark_is_drift_and_its_raw_value_never_reaches_the_report() {
    let planted = "not an entry id with spaces and a long tail of text";
    let rows = json!([instance(
        "svc",
        "b1",
        json!({ "user.arcakernel.entry": planted }),
        json!({ "user.arcakernel.entry": planted }),
    )]);
    let report = run(rows, &default_cfg());
    assert_eq!(find(&report, "svc", "b1").owner, OwnerClass::Foreign);
    assert!(report.drift.contains(&Drift::MarkedWithoutEntry {
        project: "svc".to_string(),
        name: "b1".to_string(),
        entry: None,
    }));
    let text = serde_json::to_string(&report).expect("the report serializes");
    assert!(
        !text.contains(planted),
        "the raw mark value is never echoed"
    );
}

#[test]
fn ownership_is_classed_from_own_config_and_a_profile_cannot_supply_a_mark() {
    // A mark that only the expanded config carries (a profile supplied it) does not class the object: the classifier is given
    // the instance's own config, never its expanded one.
    let rows = json!([
        instance("svc", "p1", json!({}), json!({ "user.droid.lease": "1" }),),
        instance(
            "svc",
            "p2",
            json!({}),
            json!({ "user.arcakernel.entry": "toy" }),
        ),
    ]);
    let cfg = WatchConfig {
        live_entries: [entry("toy")].into_iter().collect(),
        projects: all_projects(),
        complete: true,
        ..WatchConfig::default()
    };
    let report = run(rows, &cfg);
    assert_eq!(
        find(&report, "svc", "p1").owner,
        OwnerClass::Foreign,
        "a profile's droidd mark does not class p1"
    );
    assert_eq!(
        find(&report, "svc", "p2").owner,
        OwnerClass::Foreign,
        "a profile's kernel mark does not class p2"
    );
    // toy is live, and its only mark is in p2's expanded config, which counts for nothing: toy has no object.
    assert_eq!(
        report.drift,
        vec![Drift::EntryWithoutObject {
            entry: entry("toy")
        }],
        "the expanded mark is no object for toy, and it adds no other drift"
    );
}

#[test]
fn a_foreign_object_with_no_kernel_key_gives_no_drift() {
    let report = run(fixture_listing(), &default_cfg());
    assert!(
        !report
            .drift
            .iter()
            .any(|d| matches!(d, Drift::MarkedWithoutEntry { name, .. } if name == "guest1")),
        "guest1 carries no mark: {:?}",
        report.drift
    );
}

#[test]
fn config_hash_ignores_volatile_keys_and_expanded_config_but_not_own_keys() {
    let base: BTreeMap<String, String> = [
        ("user.arca.template".to_string(), "postgres".to_string()),
        ("limits.cpu".to_string(), "2".to_string()),
    ]
    .into_iter()
    .collect();
    let mut with_volatile = base.clone();
    with_volatile.insert(
        "volatile.eth0.hwaddr".to_string(),
        "00:16:3e:aa".to_string(),
    );
    assert_eq!(
        config_hash(&base),
        config_hash(&with_volatile),
        "a volatile key does not change the hash"
    );

    // Two listings that differ only in expanded_config give the same hash, because the hash reads own config only.
    let plain = run(
        json!([instance(
            "svc",
            "h1",
            json!({ "limits.cpu": "2" }),
            json!({ "limits.cpu": "2" }),
        )]),
        &default_cfg(),
    );
    let expanded = run(
        json!([instance(
            "svc",
            "h1",
            json!({ "limits.cpu": "2" }),
            json!({ "limits.cpu": "2", "security.privileged": "true" }),
        )]),
        &default_cfg(),
    );
    assert_eq!(
        find(&plain, "svc", "h1").config_hash,
        find(&expanded, "svc", "h1").config_hash,
        "expanded_config does not change the hash"
    );

    let mut changed = base.clone();
    changed.insert("limits.cpu".to_string(), "4".to_string());
    assert_ne!(
        config_hash(&base),
        config_hash(&changed),
        "an own key changes the hash"
    );
    assert_eq!(
        config_hash(&base).len(),
        64,
        "the hash is 64 hex characters"
    );
}

#[test]
fn the_watch_covers_only_the_projects_it_is_given() {
    let cfg = WatchConfig {
        projects: ["droid".to_string()].into_iter().collect(),
        complete: false,
        ..WatchConfig::default()
    };
    let report = run(fixture_listing(), &cfg);
    let rows = every_row(&report);
    assert!(
        rows.iter().all(|(project, _)| project == "droid"),
        "every row is in the scoped project: {rows:?}"
    );
    assert_eq!(rows.len(), 2);
}

#[test]
fn a_malformed_row_fails_and_never_echoes_its_values() {
    let planted = "SECRET-LOOKING-VALUE-42";
    let rows = json!([{
        "name": "m1",
        "project": "svc",
        "type": "container",
        "status": planted,
        "config": "not an object",
        "expanded_config": {},
    }]);
    let mut host = Listing::of(rows);
    let err = watch_once(&mut host, &default_cfg(), 1).expect_err("a malformed row is refused");
    let text = err.to_string();
    assert!(!text.contains(planted), "the error names no value: {text}");
}

#[test]
fn an_instance_with_an_unclassed_type_fails() {
    let rows = json!([{
        "name": "u1",
        "project": "svc",
        "type": "lxc-weird",
        "status": "Running",
        "config": {},
        "expanded_config": {},
    }]);
    let mut host = Listing::of(rows);
    assert!(matches!(
        watch_once(&mut host, &default_cfg(), 1),
        Err(PluginError::Failed(_))
    ));
}

#[test]
fn an_instance_listed_twice_fails() {
    let twice = json!([
        instance("svc", "d1", json!({}), json!({})),
        instance("svc", "d1", json!({}), json!({})),
    ]);
    let mut host = Listing::of(twice);
    assert!(matches!(
        watch_once(&mut host, &default_cfg(), 1),
        Err(PluginError::Failed(_))
    ));
}

#[test]
fn the_report_conforms_to_the_watch_report_schema() {
    let cfg = WatchConfig {
        identity_phones: ["phone1".to_string()].into_iter().collect(),
        live_entries: [entry("toy")].into_iter().collect(),
        projects: all_projects(),
        complete: true,
        ..WatchConfig::default()
    };
    let report = run(fixture_listing(), &cfg);
    let value = serde_json::to_value(&report).expect("the report serializes");
    let schema = (verb("watch_report").expect("the verb exists").result)();
    conforms(&schema, &value).expect("the report conforms");
}

#[test]
fn watch_once_stamps_the_report_with_the_host_clock() {
    let mut host = Listing::of(fixture_listing());
    let report = watch_once(&mut host, &default_cfg(), 1_234_567).expect("the watch runs");
    assert_eq!(report.at, 1_234_567);
}

#[test]
fn the_watch_manifest_observes_and_declares_no_effect() {
    let manifest = Watch::manifest();
    assert_eq!(manifest.id, PluginId("arca.watch".to_string()));
    assert!(manifest.observes);
    assert!(manifest.effects.is_empty(), "a watch has no effect");
    assert_eq!(manifest.provides.len(), 1);
    assert_eq!(manifest.provides[0].key, "watch.report");
    let erased = Erased::<Watch>::new().expect("the watch is a valid plugin");
    assert_eq!(erased.id(), "arca.watch");
}

#[test]
fn an_effect_attempted_inside_the_watch_is_refused_and_journals_nothing() {
    let manifest = Watch::manifest();
    let mut host = Listing::of(fixture_listing());
    let mut journal = Journal::open(MemJournal::new()).expect("open the journal");
    let op = EffectOp::IncusCreate {
        parent: None,
        kind: IncusKind::Instance,
        project: "svc".to_string(),
        name: "x9".to_string(),
        body: json!({ "type": "container" }),
    };
    {
        let mut ctx = Ctx::new(entry("watch"), &manifest, &mut host, &mut journal, 1);
        assert_eq!(ctx.effect(op), Err(EffectRefusal::WatchOnly));
    }
    assert!(journal.live(&entry("watch")).is_empty());
    assert_eq!(host.plans.get(), 0, "no plan was asked of the provider");
    assert_eq!(host.effects.get(), 0, "no effect was asked of the provider");
}

#[test]
fn a_config_with_an_unknown_field_does_not_deserialize() {
    let parsed: Result<WatchConfig, _> = serde_json::from_value(json!({ "projectz": ["svc"] }));
    assert!(parsed.is_err());
}

#[test]
fn a_listing_that_is_not_a_list_fails_the_watch() {
    let mut host = Listing::of(json!({ "not": "a list" }));
    assert!(matches!(
        watch_once(&mut host, &default_cfg(), 1),
        Err(PluginError::Failed(_))
    ));
}

#[test]
fn a_scoped_watch_reports_no_entry_without_object_for_an_object_it_does_not_cover() {
    // toy's objects are in svc, which this scoped watch leaves out. The watch cannot see them, so it must not say toy or ghost
    // has no object.
    let cfg = WatchConfig {
        live_entries: [entry("toy"), entry("ghost")].into_iter().collect(),
        projects: ["droid".to_string()].into_iter().collect(),
        complete: false,
        ..WatchConfig::default()
    };
    let report = run(fixture_listing(), &cfg);
    assert!(
        !report
            .drift
            .iter()
            .any(|d| matches!(d, Drift::EntryWithoutObject { .. })),
        "a scoped watch reports no entry without object: {:?}",
        report.drift
    );
}

#[test]
fn a_complete_watch_reports_entry_without_object_only_for_an_entry_with_no_object() {
    let cfg = WatchConfig {
        live_entries: [entry("toy"), entry("ghost")].into_iter().collect(),
        projects: all_projects(),
        complete: true,
        ..WatchConfig::default()
    };
    let report = run(fixture_listing(), &cfg);
    assert_eq!(
        report.drift,
        vec![Drift::EntryWithoutObject {
            entry: entry("ghost")
        }],
        "toy has objects and ghost has none: {:?}",
        report.drift
    );
}

#[test]
fn a_contested_object_alone_does_not_satisfy_a_live_entry() {
    // x1 carries droidd's lease mark and toy's kernel mark, so it is contested. No other object carries toy's mark.
    let rows = json!([instance(
        "svc",
        "x1",
        json!({ "user.droid.lease": "1", "user.arcakernel.entry": "toy" }),
        json!({ "user.droid.lease": "1", "user.arcakernel.entry": "toy" }),
    )]);
    let cfg = WatchConfig {
        live_entries: [entry("toy")].into_iter().collect(),
        projects: all_projects(),
        complete: true,
        ..WatchConfig::default()
    };
    let report = run(rows, &cfg);
    assert_eq!(find(&report, "svc", "x1").owner, OwnerClass::Contested);
    assert_eq!(
        report.drift,
        vec![Drift::EntryWithoutObject {
            entry: entry("toy")
        }],
        "a contested object does not hold toy: {:?}",
        report.drift
    );
}

#[test]
fn a_malformed_row_outside_the_scope_does_not_fail_a_scoped_watch() {
    // svc's row names a type the watch does not classify. The watch covers only droid, so that row is never read.
    let rows = json!([
        instance(
            "droid",
            "phone1",
            json!({ "user.droid.lease": "1" }),
            json!({ "user.droid.lease": "1" }),
        ),
        {
            "name": "u1",
            "project": "svc",
            "type": "lxc-weird",
            "status": "Running",
            "config": {},
            "expanded_config": {},
        },
    ]);
    let cfg = WatchConfig {
        projects: ["droid".to_string()].into_iter().collect(),
        complete: false,
        ..WatchConfig::default()
    };
    let report = run(rows, &cfg);
    let names: Vec<String> = every_row(&report)
        .into_iter()
        .map(|(_, name)| name)
        .collect();
    assert_eq!(names, vec!["phone1".to_string()]);
}

#[test]
fn a_row_that_names_no_project_fails_even_a_scoped_watch() {
    // Without a project a row cannot be placed in or out of the scope, so the watch refuses it.
    let rows = json!([{
        "name": "n1",
        "type": "container",
        "status": "Running",
        "config": {},
        "expanded_config": {},
    }]);
    let cfg = WatchConfig {
        projects: ["droid".to_string()].into_iter().collect(),
        complete: false,
        ..WatchConfig::default()
    };
    let mut host = Listing::of(rows);
    assert!(matches!(
        watch_once(&mut host, &cfg, 1),
        Err(PluginError::Failed(_))
    ));
}
