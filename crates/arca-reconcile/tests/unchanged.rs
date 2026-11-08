//! A converged, unchanged entry is skipped unless its plugin opts into being reconciled every pass. The service project's
//! network plugin repairs its proxy allowance from live state, so it opts in.

mod common;

use arca_incus::{FakeIncus, FakeState};
use arca_kernel::{
    desired::{DesiredStateGate, Entry, EntryId},
    identity::NodeKey,
    journal::{FileJournal, Journal},
    plugin::Plugin,
};
use arca_netproject::{NetProject, PROJECT};
use arca_providers::incus::{Family, IncusProvider, RetainedSet};
use arca_reconcile::{Outcome, PluginSet, Reconciler};
use common::*;
use serde_json::{Value, json};

const ENTRY: &str = "net-arcak-svc";

fn net_entry() -> Entry {
    entry(
        ENTRY,
        NetProject::ID,
        json!({
            "project": PROJECT,
            "bridge": "arcak-svcbr",
            "cidr": "192.0.2.0/24",
            "virtualMachines": 0
        }),
    )
}

fn open_with(
    scratch: &Scratch,
    fake: &FakeIncus,
    key: &NodeKey,
    converged: Vec<Entry>,
) -> Reconciler<FileJournal> {
    let retained = RetainedSet::empty();
    let provider = IncusProvider::new(
        fake.socket().to_path_buf(),
        Family {
            projects: vec![PROJECT.into()],
            scratch: false,
            droidd_projects: vec!["droid".into()],
            reserved_cidrs: vec!["198.51.100.0/24".into()],
        },
        scratch.path("secrets"),
        retained.clone(),
    )
    .expect("the provider is valid");
    let journal = Journal::open(
        FileJournal::open(&scratch.path("journal.jsonl")).expect("the journal opens"),
    )
    .expect("the journal folds");
    let mut plugins = PluginSet::new();
    plugins.add::<NetProject>().expect("the plugin is valid");
    Reconciler::open_resuming(
        DesiredStateGate::new(key.public(), None),
        journal,
        plugins,
        Box::new(provider),
        retained,
        converged,
        NOW,
    )
    .expect("the journal has no unresolved intent")
}

fn start() -> (Scratch, FakeIncus, NodeKey, Reconciler<FileJournal>) {
    let scratch = Scratch::new("unchanged");
    let fake = FakeIncus::start(
        &scratch.dir,
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![],
            networks: vec![],
            pools: vec![],
        },
    )
    .expect("the fake serves");
    fake.add_profile(
        "default",
        json!({"name":"default","config":{},"devices":{"root":{"pool":"pool1"}}}),
    );
    let key = node_key();
    let reconciler = open_with(&scratch, &fake, &key, Vec::new());
    (scratch, fake, key, reconciler)
}

fn applied() -> Vec<Outcome> {
    vec![Outcome::Applied {
        entry: EntryId(ENTRY.into()),
    }]
}

fn project_path() -> String {
    format!("/1.0/projects/{PROJECT}")
}

#[test]
fn a_converged_unchanged_netproject_entry_repairs_the_missing_proxy_allowance_once() {
    let (_scratch, fake, key, mut reconciler) = start();
    let first = signed(&key, 1, None, vec![net_entry()]);
    assert_eq!(reconciler.reconcile(&first, NOW), applied());

    let path = project_path();
    let project = get(&fake, &path).expect("the project exists");
    let mut legacy = project.clone();
    legacy["config"]
        .as_object_mut()
        .unwrap()
        .remove("restricted.devices.proxy");
    let (status, _) = raw(&fake, "PUT", &path, Some(&legacy));
    assert_eq!(status, 200, "the operator's edit lands");

    // The same entry arrives again: converged and unchanged.
    let second = signed(&key, 2, Some(first.document.digest()), vec![net_entry()]);
    assert_eq!(
        reconciler.reconcile(&second, NOW),
        applied(),
        "the entry is reconciled although its config did not change"
    );

    let writes = fake.writes();
    // A project's config patch is sent as a whole-config PUT, so the operator's edit and the repair share a method.
    let edit = writes
        .iter()
        .rposition(|write| {
            write.method == "PUT"
                && write.path.starts_with(&path)
                && !write
                    .config_keys
                    .iter()
                    .any(|key| key == "restricted.devices.proxy")
        })
        .expect("the operator's edit is recorded");
    let new = &writes[edit + 1..];
    assert_eq!(new.len(), 1, "exactly one effect is sent: {new:?}");
    assert_eq!(new[0].method, "PUT");
    assert!(
        new[0]
            .path
            .starts_with(&format!("{path}?project={PROJECT}"))
            || new[0].path == path,
        "the repair addresses the project: {:?}",
        new[0].path
    );
    assert!(
        new[0]
            .config_keys
            .iter()
            .any(|key| key == "restricted.devices.proxy"),
        "the patch carries the allowance: {:?}",
        new[0].config_keys
    );
    let repaired = get(&fake, &path).expect("the project remains");
    let mut expected: Value = legacy["config"].clone();
    expected["restricted.devices.proxy"] = json!("allow");
    assert_eq!(repaired["config"], expected, "only the allowance changed");
}

#[test]
fn a_converged_unchanged_netproject_entry_writes_nothing_when_the_allowance_is_present() {
    let (_scratch, fake, key, mut reconciler) = start();
    let first = signed(&key, 1, None, vec![net_entry()]);
    assert_eq!(reconciler.reconcile(&first, NOW), applied());
    let project = get(&fake, &project_path()).expect("the project exists");
    assert_eq!(project["config"]["restricted.devices.proxy"], "allow");
    let writes = fake.writes().len();

    let second = signed(&key, 2, Some(first.document.digest()), vec![net_entry()]);
    assert_eq!(
        reconciler.reconcile(&second, NOW),
        applied(),
        "the opt-in entry is still reconciled"
    );
    assert_eq!(fake.writes().len(), writes, "no project write is sent");
}

#[test]
fn a_restart_with_stored_converged_entries_repairs_the_missing_proxy_allowance() {
    let (scratch, fake, key, mut reconciler) = start();
    let first = signed(&key, 1, None, vec![net_entry()]);
    assert_eq!(reconciler.reconcile(&first, NOW), applied());
    let converged = reconciler.converged();
    assert_eq!(
        converged,
        vec![net_entry()],
        "the entry is stored as converged"
    );
    drop(reconciler);

    let path = project_path();
    let project = get(&fake, &path).expect("the project exists");
    let mut legacy = project.clone();
    legacy["config"]
        .as_object_mut()
        .unwrap()
        .remove("restricted.devices.proxy");
    let (status, _) = raw(&fake, "PUT", &path, Some(&legacy));
    assert_eq!(status, 200, "the operator's edit lands");

    // The restart resumes from the stored converged entries and re-offers the same document.
    let mut resumed = open_with(&scratch, &fake, &key, converged);
    assert_eq!(
        resumed.reconcile(&first, NOW),
        applied(),
        "the resumed entry is reconciled against live state"
    );
    let repaired = get(&fake, &path).expect("the project remains");
    assert_eq!(
        repaired["config"]["restricted.devices.proxy"], "allow",
        "the startup pass repairs the allowance"
    );
}
