//! The kernel's Incus write provider, tested against the fake Incus and against small servers that answer a scripted sequence.
//! Every refusal is checked twice: the provider returns the refusal, and the server sees no write. No test touches a real Incus
//! socket, and every scratch directory is under the default temporary directory.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::net::IpAddr;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use arca_incus::{FakeIncus, FakeState, IncusRead, UnixIncus};
use arca_kernel::canon::Digest;
use arca_kernel::desired::{EntryId, PluginId};
use arca_kernel::journal::{
    EffectClass, EffectState, FileJournal, Inverse, Journal, JournalStore, MemJournal,
};
use arca_kernel::ownership::{IncusKind, ObjectRef, OwnerClass};
use arca_kernel::plugin::{
    Ctx, EffectOp, EffectRefusal, Manifest, Observe, Planned, Provider, ReadBack, op_digest,
};
use arca_providers::incus::{
    DROIDD_PROJECTS_FIXED, Family, IncusProvider, RetainedSet, Retirement, SECRET_PREFIX,
    host_addresses, kind_path,
};
use serde_json::{Value, json};

const MARK: &str = "user.arcakernel.entry";
const RETIRING: &str = "user.arcakernel.retiring";
const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../arca-incus/tests/fixtures/state.json"
);
const SECRET_VALUE: &str = "SECRET-VALUE-7731";
const READ_UUID: &str = "00000000-0000-4000-8000-0000000000e1";
const OTHER_UUID: &str = "00000000-0000-4000-8000-0000000000e2";

/// The process-wide sequence every scratch directory name carries. Two directories made in one clock tick still differ.
static SCRATCH_SEQ: AtomicU64 = AtomicU64::new(0);

/// A fresh directory under the default temporary directory, removed when the test ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .subsec_nanos();
        let seq = SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "arca-prov-{}-{nanos}-{seq}-{tag}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create the scratch directory");
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn entry(name: &str) -> EntryId {
    EntryId(name.to_string())
}

fn family() -> Family {
    Family {
        projects: vec!["svc".into(), "kfam".into(), "kbare".into()],
        scratch: false,
        droidd_projects: vec!["droid".into()],
        reserved_cidrs: vec!["198.51.100.0/24".into()],
    }
}

fn features(values: &[(&str, &str)]) -> Value {
    let mut map = serde_json::Map::new();
    for (key, value) in values {
        map.insert((*key).to_string(), json!(value));
    }
    Value::Object(map)
}

/// The fixture the provider tests run against. `svc` has every feature the kernel needs; `kbare` has networks but no volumes.
/// `guest1` is a foreign object, moved into `svc` so the family reaches it. `k1` is the kernel's, for entry `toy`.
fn state() -> FakeState {
    let mut state = FakeState::load(Path::new(FIXTURE)).expect("load the fixture");
    for project in &mut state.projects {
        if project["name"].as_str() == Some("svc") {
            project["config"] = features(&[
                ("features.networks", "true"),
                ("features.profiles", "true"),
                ("features.storage.volumes", "true"),
                ("features.storage.buckets", "true"),
            ]);
        }
    }
    state.projects.push(json!({
        "name": "kbare", "description": "", "config": features(&[
            ("features.networks", "true"),
            ("features.profiles", "true"),
            ("features.storage.volumes", "false"),
            ("features.storage.buckets", "true"),
        ])
    }));
    for instance in &mut state.instances {
        if instance["name"] == "guest1" {
            instance["project"] = json!("svc");
        }
    }
    // The bridges the host keeps in `default`: `kbr` is the entry `toy`'s, `plainbr` carries no mark, and `droidbr` is droidd's.
    state.networks = vec![
        json!({"name": "kbr", "project": "default", "type": "bridge", "config": {
            "ipv4.address": "192.0.2.1/24", "ipv6.address": "none", MARK: "toy"}}),
        json!({"name": "plainbr", "project": "default", "type": "bridge", "config": {
            "ipv4.address": "192.0.2.2/24", "ipv6.address": "none"}}),
        json!({"name": "droidbr", "project": "default", "type": "bridge", "config": {
            "ipv4.address": "192.0.2.3/24", "ipv6.address": "none", "user.droid.lease": "1"}}),
    ];
    state
}

fn start(tag: &str) -> (Scratch, FakeIncus) {
    let dir = Scratch::new(tag);
    let fake = FakeIncus::start(&dir.0, state()).expect("start the fake");
    (dir, fake)
}

fn provider(fake: &FakeIncus, secrets: &Path) -> IncusProvider {
    IncusProvider::new(
        fake.socket().to_path_buf(),
        family(),
        secrets.to_path_buf(),
        RetainedSet::empty(),
    )
    .expect("a valid family")
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

fn create(
    kind: IncusKind,
    project: &str,
    parent: Option<&str>,
    name: &str,
    body: Value,
) -> EffectOp {
    EffectOp::IncusCreate {
        kind,
        project: project.to_string(),
        parent: parent.map(str::to_string),
        name: name.to_string(),
        body,
    }
}

fn patch(object: ObjectRef, pairs: &[(&str, Option<&str>)]) -> EffectOp {
    EffectOp::IncusPatchConfig {
        object,
        config: pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), value.map(str::to_string)))
            .collect(),
    }
}

fn changes(pairs: &[(&str, Option<&str>)]) -> BTreeMap<String, Option<String>> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_string(), value.map(str::to_string)))
        .collect()
}

/// The planned answer a provider's `plan` gives for `op`, for a test that calls `effect` without a plan of its own.
fn forged(op: &EffectOp, class: EffectClass, etag: Option<String>) -> Planned {
    Planned {
        class,
        op_digest: op_digest(op),
        pre_image: None,
        etag,
    }
}

fn manifest() -> Manifest {
    Manifest {
        id: PluginId("arca.test".to_string()),
        version: "0.1.0".to_string(),
        provides: Vec::new(),
        effects: vec![
            (IncusKind::Project, EffectClass::B),
            (IncusKind::Instance, EffectClass::B),
            (IncusKind::Volume, EffectClass::B),
            (IncusKind::Bucket, EffectClass::B),
            (IncusKind::Network, EffectClass::B),
            (IncusKind::Acl, EffectClass::A),
            (IncusKind::Profile, EffectClass::A),
            (IncusKind::Zone, EffectClass::A),
            (IncusKind::Forward, EffectClass::A),
        ],
        projects: vec!["svc".into(), "kfam".into(), "kbare".into()],
        observes: false,
    }
}

fn writes(fake: &FakeIncus) -> Vec<(String, String)> {
    fake.writes()
        .into_iter()
        .map(|w| (w.method, w.path))
        .collect()
}

fn only_gets(fake: &FakeIncus) -> bool {
    fake.requests().iter().all(|r| r.method == "GET")
}

/// Reads an object's metadata through the GET-only client.
fn read(fake: &FakeIncus, path: &str) -> Value {
    UnixIncus::new(fake.socket().to_path_buf())
        .get(path)
        .expect("a GET of the object")
}

fn secrets_dir(scratch: &Scratch) -> PathBuf {
    let dir = scratch.0.join("secrets");
    fs::create_dir_all(&dir).expect("create the secrets directory");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("mode 0700");
    dir
}

/// Writes a secret file with mode 0600, as the provider requires.
fn write_secret(dir: &Path, name: &str, value: &str) {
    let path = dir.join(name);
    fs::write(&path, value).expect("write the secret");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("mode 0600");
}

// ---------------------------------------------------------------------------------------------------------------------------
// Construction
// ---------------------------------------------------------------------------------------------------------------------------

#[test]
fn concurrent_constructions_never_collide_on_the_uid_probe() {
    // The uid probe names its directory from the pid and the clock. Two constructions that read the same clock value would name
    // the same directory, and the second would be refused. The name carries a process-wide sequence, so no two probes share it.
    let failures = Arc::new(AtomicUsize::new(0));
    let threads: Vec<JoinHandle<()>> = (0..32)
        .map(|_| {
            let failures = Arc::clone(&failures);
            thread::spawn(move || {
                for _ in 0..200 {
                    let built = IncusProvider::new(
                        PathBuf::from("/nonexistent/probe.sock"),
                        family(),
                        PathBuf::from("/nonexistent/secrets"),
                        RetainedSet::empty(),
                    );
                    if built.is_err() {
                        failures.fetch_add(1, Ordering::SeqCst);
                    }
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().expect("a construction thread");
    }
    let refused = failures.load(Ordering::SeqCst);
    assert_eq!(
        refused, 0,
        "constructions the uid probe refused: {refused} of 6400"
    );
}

#[test]
fn concurrent_scratch_directories_are_distinct() {
    // Two scratch directories with one tag, made in one clock tick, would be one directory, and dropping either would remove both.
    let threads: Vec<JoinHandle<Vec<Scratch>>> = (0..32)
        .map(|_| thread::spawn(move || (0..200).map(|_| Scratch::new("same-tag")).collect()))
        .collect();
    let mut made: Vec<Scratch> = Vec::new();
    for thread in threads {
        made.extend(thread.join().expect("a scratch thread"));
    }
    let distinct: BTreeSet<&PathBuf> = made.iter().map(|scratch| &scratch.0).collect();
    assert_eq!(
        distinct.len(),
        made.len(),
        "scratch directories share a name"
    );
}

#[test]
fn a_family_that_names_bench_or_default_is_refused_at_construction() {
    for reserved in DROIDD_PROJECTS_FIXED {
        let mut bad = family();
        bad.projects.push(reserved.to_string());
        let result = IncusProvider::new(
            PathBuf::from("/nonexistent/incus.sock"),
            bad,
            PathBuf::from("/nonexistent"),
            RetainedSet::empty(),
        );
        assert!(
            matches!(result, Err(EffectRefusal::Provider(_))),
            "{reserved}: {result:?}"
        );
    }
}

#[test]
fn a_family_that_names_a_droidd_project_is_refused_at_construction() {
    let mut bad = family();
    bad.projects.push("droid".into());
    let result = IncusProvider::new(
        PathBuf::from("/nonexistent/incus.sock"),
        bad,
        PathBuf::from("/nonexistent"),
        RetainedSet::empty(),
    );
    assert!(
        matches!(result, Err(EffectRefusal::Provider(_))),
        "{result:?}"
    );
}

#[test]
fn a_scratch_family_names_only_scratch_projects_and_a_bad_reserved_range_is_refused() {
    let mut scratch = family();
    scratch.scratch = true;
    let result = IncusProvider::new(
        PathBuf::from("/nonexistent/incus.sock"),
        scratch.clone(),
        PathBuf::from("/nonexistent"),
        RetainedSet::empty(),
    );
    assert!(
        matches!(result, Err(EffectRefusal::Provider(_))),
        "a project without the prefix: {result:?}"
    );

    scratch.projects = vec!["arcak-scratch-1".into()];
    IncusProvider::new(
        PathBuf::from("/nonexistent/incus.sock"),
        scratch.clone(),
        PathBuf::from("/nonexistent"),
        RetainedSet::empty(),
    )
    .expect("a scratch family of scratch projects is accepted");

    scratch.reserved_cidrs = vec!["not-a-range".into()];
    let result = IncusProvider::new(
        PathBuf::from("/nonexistent/incus.sock"),
        scratch,
        PathBuf::from("/nonexistent"),
        RetainedSet::empty(),
    );
    assert!(
        matches!(result, Err(EffectRefusal::Provider(_))),
        "{result:?}"
    );
}

// ---------------------------------------------------------------------------------------------------------------------------
// The family and ownership, checked in plan
// ---------------------------------------------------------------------------------------------------------------------------

#[test]
fn a_create_outside_the_family_is_refused_before_any_request() {
    let (scratch, fake) = start("family");
    let provider = provider(&fake, &secrets_dir(&scratch));
    for project in ["bench", "default", "droid", "other"] {
        let op = create(IncusKind::Instance, project, None, "i9", json!({}));
        assert_eq!(
            provider.plan(&entry("toy"), &op),
            Err(EffectRefusal::ProjectNotAllowed),
            "{project}"
        );
    }
    let op = create(IncusKind::Project, "bench", None, "bench", json!({}));
    assert_eq!(
        provider.plan(&entry("toy"), &op),
        Err(EffectRefusal::ProjectNotAllowed)
    );
    assert!(
        fake.requests().is_empty(),
        "no request was made: {:?}",
        fake.requests()
    );
}

#[test]
fn a_patch_or_delete_of_an_object_not_owned_by_the_entry_is_refused_and_only_read() {
    let (scratch, fake) = start("not-owned");
    let provider = provider(&fake, &secrets_dir(&scratch));
    // db1 is droidd's, guest1 is foreign, x1 is contested, and k1 belongs to entry `toy`, not `other`.
    let cases = [
        ("db1", OwnerClass::Droidd),
        ("guest1", OwnerClass::Foreign),
        ("x1", OwnerClass::Contested),
        (
            "k1",
            OwnerClass::Kernel {
                entry: entry("toy"),
            },
        ),
    ];
    for (name, class) in cases {
        let target = object(IncusKind::Instance, "svc", None, name);
        let op = patch(target.clone(), &[("user.note", Some("x"))]);
        assert_eq!(
            provider.plan(&entry("other"), &op),
            Err(EffectRefusal::NotOwned(class.clone())),
            "patch of {name}"
        );
        let delete = EffectOp::IncusDelete {
            object: target,
            plan: Digest::of(b"plan"),
        };
        assert_eq!(
            provider.plan(&entry("other"), &delete),
            Err(EffectRefusal::NotOwned(class)),
            "delete of {name}"
        );
    }
    assert!(
        writes(&fake).is_empty(),
        "nothing was written: {:?}",
        writes(&fake)
    );
    assert!(
        only_gets(&fake),
        "only GET reached the fake: {:?}",
        fake.requests()
    );
}

#[test]
fn a_create_naming_another_entrys_mark_is_refused_before_any_write() {
    let (scratch, fake) = start("mark-conflict");
    let mut provider = provider(&fake, &secrets_dir(&scratch));
    let op = create(
        IncusKind::Instance,
        "svc",
        None,
        "i9",
        json!({"config": {MARK: "other"}}),
    );
    // The plan refuses the body, so no intent is journalled for it. The effect refuses it again if it is reached.
    assert!(matches!(
        provider.plan(&entry("toy"), &op),
        Err(EffectRefusal::Provider(_))
    ));
    let planned = forged(&op, EffectClass::B, None);
    assert!(matches!(
        provider.effect(&entry("toy"), &op, &planned),
        Err(EffectRefusal::Provider(_))
    ));
    assert!(writes(&fake).is_empty(), "nothing was written");
}

// ---------------------------------------------------------------------------------------------------------------------------
// Plans and effects through the kernel's Ctx
// ---------------------------------------------------------------------------------------------------------------------------

#[test]
fn a_patch_journals_the_pre_image_read_at_plan_and_an_absent_key_restores_as_absent() {
    // The object holds a secret reference at a and no b. The patch sets a=new and b=x, so the inverse restores a to the reference
    // and removes b. A value that is no secret cannot be replaced by a patch, so the old value is a secret.
    let (scratch, fake) = start("pre-image");
    let secrets = secrets_dir(&scratch);
    write_secret(&secrets, "t1", SECRET_VALUE);
    let reference = format!("{SECRET_PREFIX}t1");
    let mut provider = provider(&fake, &secrets);
    let manifest = manifest();
    let mut journal = Journal::open(MemJournal::new()).expect("opens");
    let made = Ctx::new(entry("toy"), &manifest, &mut provider, &mut journal, 1)
        .effect(create(
            IncusKind::Instance,
            "svc",
            None,
            "p1",
            json!({"config": {MARK: "toy", "a": reference}}),
        ))
        .expect("create");
    let target = ObjectRef {
        uuid: made.uuid.clone(),
        ..made
    };
    let op = patch(target.clone(), &[("a", Some("new")), ("b", Some("x"))]);
    Ctx::new(entry("toy"), &manifest, &mut provider, &mut journal, 2)
        .effect(op)
        .expect("the patch applies");
    let patched = journal
        .live(&entry("toy"))
        .into_iter()
        .find(|r| matches!(r.inverse, Some(Inverse::IncusRestoreConfig { .. })))
        .expect("the patch is journalled");
    assert_eq!(patched.state, EffectState::Applied);
    assert_eq!(
        patched.inverse,
        Some(Inverse::IncusRestoreConfig {
            object: target,
            config: changes(&[("a", Some(reference.as_str())), ("b", None)]),
        })
    );
}

#[test]
fn an_etag_that_changes_between_plan_and_effect_is_stale_and_writes_nothing() {
    let (scratch, fake) = start("stale");
    let secrets = secrets_dir(&scratch);
    let mut provider = provider(&fake, &secrets);
    let target = object(IncusKind::Instance, "svc", None, "k1");
    let op = patch(target.clone(), &[("user.note", Some("planned"))]);
    let planned = provider
        .plan(&entry("toy"), &op)
        .expect("the plan reads the object");
    // Another write from the entry changes the object, so the ETag the plan read is out of date.
    let writer = arca_incus::UnixIncusWriter::new(
        fake.socket().to_path_buf(),
        entry("toy"),
        BTreeMap::new(),
        arca_incus::ProjectFamily::new(["svc"], ["droid"]).expect("family"),
    );
    writer
        .patch(
            &writer.read_owned(&target).expect("read"),
            &changes(&[("user.other", Some("1"))]),
        )
        .expect("the other write lands");
    let before = writes(&fake).len();
    let result = provider.effect(&entry("toy"), &op, &planned);
    assert_eq!(result, Err(EffectRefusal::Stale));
    assert_eq!(writes(&fake).len(), before, "the stale patch sent no write");
}

#[test]
fn every_create_carries_the_entry_mark_in_its_post_body() {
    let (scratch, fake) = start("mark-in-body");
    let mut provider = provider(&fake, &secrets_dir(&scratch));
    let manifest = manifest();
    let mut journal = Journal::open(MemJournal::new()).expect("opens");
    Ctx::new(entry("toy"), &manifest, &mut provider, &mut journal, 1)
        .effect(create(
            IncusKind::Instance,
            "svc",
            None,
            "m5",
            json!({"config": {"user.note": "n"}}),
        ))
        .expect("create");
    let post = fake
        .writes()
        .into_iter()
        .find(|w| w.method == "POST")
        .expect("one POST");
    assert!(
        post.config_keys.contains(&MARK.to_string()),
        "{:?}",
        post.config_keys
    );
    assert!(post.config_keys.contains(&"user.note".to_string()));
}

#[test]
fn a_project_create_carries_the_mark_the_features_restricted_and_the_op_config() {
    let (scratch, fake) = start("project-create");
    let mut provider = provider(&fake, &secrets_dir(&scratch));
    let manifest = manifest();
    let mut journal = Journal::open(MemJournal::new()).expect("opens");
    let made = Ctx::new(entry("toy"), &manifest, &mut provider, &mut journal, 1)
        .effect(create(
            IncusKind::Project,
            "kfam",
            None,
            "kfam",
            json!({"config": {"limits.virtual-machines": "4"}}),
        ))
        .expect("the project is created");
    assert_eq!(made.uuid, None, "a project has no uuid");
    let config = read(&fake, "/1.0/projects/kfam")["config"].clone();
    assert_eq!(config[MARK], json!("toy"));
    assert!(
        config.get("features.networks").is_none(),
        "networks live in default, so the kernel project does not set features.networks: {config}"
    );
    assert_eq!(config["features.profiles"], json!("true"));
    assert_eq!(config["features.storage.volumes"], json!("true"));
    assert_eq!(config["features.storage.buckets"], json!("true"));
    assert_eq!(
        config["features.images"],
        json!("true"),
        "a kernel project keeps its own image store, which its retirement clears: {config}"
    );
    assert_eq!(config["restricted"], json!("true"));
    assert_eq!(config["limits.virtual-machines"], json!("4"));
}

#[test]
fn a_project_create_that_sets_a_kernel_key_another_way_is_refused() {
    let (scratch, fake) = start("project-conflict");
    let provider = provider(&fake, &secrets_dir(&scratch));
    let op = create(
        IncusKind::Project,
        "kfam",
        None,
        "kfam",
        json!({"config": {"features.networks": "false"}}),
    );
    let planned = forged(&op, EffectClass::B, None);
    let mut provider = provider;
    assert!(matches!(
        provider.effect(&entry("toy"), &op, &planned),
        Err(EffectRefusal::Provider(_))
    ));
    assert!(writes(&fake).is_empty());
}

#[test]
fn a_project_create_gets_a_delete_by_name_inverse_through_the_kernel() {
    let (scratch, fake) = start("project-inverse");
    let mut provider = provider(&fake, &secrets_dir(&scratch));
    let manifest = manifest();
    let mut journal = Journal::open(MemJournal::new()).expect("opens");
    Ctx::new(entry("toy"), &manifest, &mut provider, &mut journal, 1)
        .effect(create(IncusKind::Project, "kfam", None, "kfam", json!({})))
        .expect("the project is created");
    let live = journal.live(&entry("toy"));
    assert_eq!(live[0].state, EffectState::Applied);
    assert_eq!(
        live[0].inverse,
        Some(Inverse::IncusDelete {
            object: object(IncusKind::Project, "kfam", None, "kfam"),
        })
    );
}

#[test]
fn a_volume_create_names_its_pool_and_its_inverse_names_the_same_pool() {
    let (scratch, fake) = start("volume");
    let mut provider = provider(&fake, &secrets_dir(&scratch));
    let manifest = manifest();
    let mut journal = Journal::open(MemJournal::new()).expect("opens");
    Ctx::new(entry("toy"), &manifest, &mut provider, &mut journal, 1)
        .effect(create(
            IncusKind::Volume,
            "svc",
            Some("fast"),
            "vol1",
            json!({"config": {}}),
        ))
        .expect("the volume is created");
    let live = journal.live(&entry("toy"));
    let Some(Inverse::IncusDelete { object: named }) = live[0].inverse.clone() else {
        panic!("a delete inverse: {:?}", live[0].inverse);
    };
    assert_eq!(named.parent.as_deref(), Some("fast"));
    assert_eq!(
        named.uuid, None,
        "a volume has no uuid, so its inverse deletes it by name"
    );
}

#[test]
fn a_bridge_network_is_refused_without_ipv6_none_outside_ipv4_or_inside_a_reserved_range() {
    let (scratch, fake) = start("bridge");
    let provider = provider(&fake, &secrets_dir(&scratch));
    let bridge = |config: Value| json!({"type": "bridge", "config": config});
    let bad = [
        (
            "no ipv6 setting",
            bridge(json!({"ipv4.address": "192.0.2.1/24"})),
        ),
        (
            "ipv6 not none",
            bridge(json!({"ipv4.address": "192.0.2.1/24", "ipv6.address": "auto"})),
        ),
        (
            "inside the reserved range",
            bridge(json!({"ipv4.address": "198.51.100.1/24", "ipv6.address": "none"})),
        ),
        (
            "overlapping the reserved range",
            bridge(json!({"ipv4.address": "198.51.0.0/16", "ipv6.address": "none"})),
        ),
        ("no ipv4", bridge(json!({"ipv6.address": "none"}))),
        (
            "not a bridge",
            json!({"type": "macvlan", "config": {"ipv6.address": "none"}}),
        ),
    ];
    for (why, body) in bad {
        let op = create(IncusKind::Network, "default", None, "br1", body);
        let result = provider.plan(&entry("toy"), &op);
        assert!(
            matches!(result, Err(EffectRefusal::Provider(_))),
            "{why}: {result:?}"
        );
    }
    assert!(
        writes(&fake).is_empty(),
        "no bridge was created: {:?}",
        writes(&fake)
    );

    let good = create(
        IncusKind::Network,
        "default",
        None,
        "br2",
        bridge(json!({"ipv4.address": "192.0.2.1/24", "ipv6.address": "none"})),
    );
    let planned = provider
        .plan(&entry("toy"), &good)
        .expect("a bridge outside the reserved range plans");
    let mut provider = provider;
    provider
        .effect(&entry("toy"), &good, &planned)
        .expect("the bridge is created");
    assert_eq!(
        writes(&fake),
        vec![(
            "POST".to_string(),
            "/1.0/networks?project=default".to_string()
        )]
    );
}

#[test]
fn a_non_instance_write_is_refused_before_any_write_when_its_project_lacks_the_feature() {
    // `kbare` has its storage volumes feature off, so a volume there would route to the default project.
    let (scratch, fake) = start("features");
    let provider = provider(&fake, &secrets_dir(&scratch));
    let volume = create(IncusKind::Volume, "kbare", Some("default"), "v9", json!({}));
    assert!(matches!(
        provider.plan(&entry("toy"), &volume),
        Err(EffectRefusal::Provider(_))
    ));
    let network = create(
        IncusKind::Network,
        "default",
        None,
        "br9",
        json!({"type": "bridge", "config": {"ipv6.address": "none", "ipv4.address": "203.0.113.1/24"}}),
    );
    provider
        .plan(&entry("toy"), &network)
        .expect("a network is held by default, which needs no feature in the kernel project");
    assert!(writes(&fake).is_empty());
    assert!(only_gets(&fake));
}

#[test]
fn an_effect_rechecks_the_family_for_a_plan_the_caller_made_by_hand() {
    // `Planned` has public fields, so a caller can hand the provider an effect for a project outside the family. The effect
    // checks the family again and sends nothing.
    let (scratch, fake) = start("forged");
    let mut provider = provider(&fake, &secrets_dir(&scratch));
    for project in ["bench", "default", "droid"] {
        let op = create(IncusKind::Instance, project, None, "i9", json!({}));
        let planned = forged(&op, EffectClass::B, None);
        assert_eq!(
            provider.effect(&entry("toy"), &op, &planned),
            Err(EffectRefusal::ProjectNotAllowed),
            "{project}"
        );
    }
    assert!(fake.requests().is_empty(), "{:?}", fake.requests());
}

#[test]
fn an_effect_whose_plan_names_another_op_is_refused_before_any_write() {
    let (scratch, fake) = start("other-op");
    let mut provider = provider(&fake, &secrets_dir(&scratch));
    let op = create(IncusKind::Instance, "svc", None, "i8", json!({}));
    let other = create(IncusKind::Instance, "svc", None, "i7", json!({}));
    let planned = forged(&other, EffectClass::B, None);
    assert!(matches!(
        provider.effect(&entry("toy"), &op, &planned),
        Err(EffectRefusal::Provider(_))
    ));
    assert!(writes(&fake).is_empty());
}

#[test]
fn an_incus_get_observation_is_refused_and_host_addresses_is_answered_only_by_listings() {
    let (scratch, fake) = start("observe");
    let provider = provider(&fake, &secrets_dir(&scratch));
    let refused = provider.observe(
        &entry("toy"),
        &["svc".into()],
        &Observe::IncusGet {
            path: "/1.0/instances".into(),
        },
    );
    assert_eq!(refused, Err(EffectRefusal::NotDeclared));
    assert!(
        fake.requests().is_empty(),
        "the refused read reached nothing"
    );
}

#[test]
fn host_addresses_collects_inet_and_inet6_and_reads_only_the_listing_and_each_state() {
    let (_scratch, fake) = start("host-addresses");
    fake.set_host_interfaces(vec![
        (
            "eth0".to_string(),
            vec![
                "203.0.113.5".parse::<IpAddr>().unwrap(),
                "2001:db8::5".parse().unwrap(),
            ],
        ),
        ("br0".to_string(), vec!["192.0.2.1".parse().unwrap()]),
    ]);
    let read = UnixIncus::new(fake.socket().to_path_buf());
    let mut found = host_addresses(&read).expect("the host's addresses");
    found.sort();
    let mut expected: Vec<IpAddr> = vec![
        "203.0.113.5".parse().unwrap(),
        "2001:db8::5".parse().unwrap(),
        "192.0.2.1".parse().unwrap(),
    ];
    expected.sort();
    assert_eq!(found, expected);
    let paths: Vec<String> = fake.requests().into_iter().map(|r| r.path).collect();
    assert_eq!(
        paths,
        vec![
            "/1.0/networks?recursion=1".to_string(),
            "/1.0/networks/eth0/state".to_string(),
            "/1.0/networks/br0/state".to_string(),
        ]
    );
    assert!(only_gets(&fake));
}

#[test]
fn host_addresses_through_the_kernel_needs_the_plugin_to_declare_observes() {
    let (scratch, fake) = start("observes");
    fake.set_host_interfaces(vec![(
        "eth0".to_string(),
        vec!["203.0.113.5".parse().unwrap()],
    )]);
    let mut provider = provider(&fake, &secrets_dir(&scratch));
    let mut declared = manifest();
    declared.observes = true;
    let mut journal = Journal::open(MemJournal::new()).expect("opens");
    let answer = Ctx::new(entry("toy"), &declared, &mut provider, &mut journal, 1)
        .observe(Observe::HostAddresses);
    let answer = answer.expect("an observing plugin reads the host");
    assert_eq!(answer, json!(["203.0.113.5"]));

    let undeclared = manifest();
    let refused = Ctx::new(entry("toy"), &undeclared, &mut provider, &mut journal, 1)
        .observe(Observe::HostAddresses);
    assert_eq!(refused, Err(EffectRefusal::NotDeclared));
}

#[test]
fn kind_path_names_each_kind_and_has_no_path_for_a_kind_that_needs_its_parent() {
    assert_eq!(
        kind_path(IncusKind::Project, "kfam", Some("kfam")).as_deref(),
        Some("/1.0/projects/kfam")
    );
    assert_eq!(
        kind_path(IncusKind::Project, "kfam", None).as_deref(),
        Some("/1.0/projects")
    );
    assert_eq!(
        kind_path(IncusKind::Instance, "svc", Some("db1")).as_deref(),
        Some("/1.0/instances/db1?project=svc")
    );
    assert_eq!(
        kind_path(IncusKind::Network, "svc", Some("br0")).as_deref(),
        Some("/1.0/networks/br0?project=svc")
    );
    assert_eq!(
        kind_path(IncusKind::Acl, "svc", None).as_deref(),
        Some("/1.0/network-acls?project=svc")
    );
    assert_eq!(
        kind_path(IncusKind::Profile, "svc", Some("p")).as_deref(),
        Some("/1.0/profiles/p?project=svc")
    );
    assert_eq!(
        kind_path(IncusKind::Zone, "svc", None).as_deref(),
        Some("/1.0/network-zones?project=svc")
    );
    for kind in [IncusKind::Volume, IncusKind::Bucket, IncusKind::Forward] {
        assert_eq!(
            kind_path(kind, "svc", Some("x")),
            None,
            "{kind:?} is addressed with its parent"
        );
    }
}

// ---------------------------------------------------------------------------------------------------------------------------
// Secrets
// ---------------------------------------------------------------------------------------------------------------------------

#[test]
fn a_secret_reference_is_sent_as_the_file_content_and_the_value_is_never_journalled() {
    let (scratch, fake) = start("secret-create");
    let secrets = secrets_dir(&scratch);
    write_secret(&secrets, "t1", SECRET_VALUE);
    let mut provider = provider(&fake, &secrets);
    let manifest = manifest();
    let journal_path = scratch.0.join("journal.jsonl");
    let mut journal =
        Journal::open(FileJournal::open(&journal_path).expect("open the journal")).expect("folds");
    let made = Ctx::new(entry("toy"), &manifest, &mut provider, &mut journal, 1)
        .effect(create(
            IncusKind::Instance,
            "svc",
            None,
            "s1",
            json!({"config": {"user.key": format!("{SECRET_PREFIX}t1")}}),
        ))
        .expect("the create applies");
    let sent = read(&fake, &format!("/1.0/instances/{}?project=svc", made.name));
    assert_eq!(
        sent["config"]["user.key"],
        json!(SECRET_VALUE),
        "the request carried the file's content"
    );
    let on_disk = fs::read_to_string(&journal_path).expect("the journal file");
    assert!(
        !on_disk.contains(SECRET_VALUE),
        "the journal holds no secret value"
    );
    assert!(
        !format!("{:?}", fake.writes()).contains(SECRET_VALUE),
        "the fake records no value"
    );
}

#[test]
fn a_patch_that_clears_a_key_holding_a_secret_journals_the_reference_as_its_pre_image() {
    let (scratch, fake) = start("secret-clear");
    let secrets = secrets_dir(&scratch);
    write_secret(&secrets, "t1", SECRET_VALUE);
    let mut provider = provider(&fake, &secrets);
    let manifest = manifest();
    let journal_path = scratch.0.join("journal.jsonl");
    let mut journal =
        Journal::open(FileJournal::open(&journal_path).expect("open")).expect("folds");
    let made = Ctx::new(entry("toy"), &manifest, &mut provider, &mut journal, 1)
        .effect(create(
            IncusKind::Instance,
            "svc",
            None,
            "s2",
            json!({"config": {"user.key": format!("{SECRET_PREFIX}t1")}}),
        ))
        .expect("create");
    let target = ObjectRef {
        uuid: made.uuid.clone(),
        ..made
    };
    Ctx::new(entry("toy"), &manifest, &mut provider, &mut journal, 2)
        .effect(patch(target.clone(), &[("user.key", None)]))
        .expect("the clear applies");
    let on_disk = fs::read_to_string(&journal_path).expect("the journal file");
    assert!(
        !on_disk.contains(SECRET_VALUE),
        "the journal holds no secret value"
    );
    let restore = journal
        .live(&entry("toy"))
        .into_iter()
        .find_map(|r| match r.inverse {
            Some(Inverse::IncusRestoreConfig { config, .. }) => Some(config),
            _ => None,
        })
        .expect("the clear is journalled with its restore");
    assert_eq!(
        restore,
        changes(&[("user.key", Some(&format!("{SECRET_PREFIX}t1")))])
    );
}

#[test]
fn a_secret_reference_that_is_not_a_valid_name_or_has_no_file_is_refused_at_plan() {
    let (scratch, fake) = start("secret-bad");
    let secrets = secrets_dir(&scratch);
    // Decoys: a usable 0600 file the traversal reference reaches, and one under a name that is not a valid secret name. Only
    // the name check refuses them, so a name check that is missing lets them through and the test fails.
    write_secret(&scratch.0, "escape", SECRET_VALUE);
    write_secret(&secrets, "Bad_Name", SECRET_VALUE);
    let provider = provider(&fake, &secrets);
    for reference in [
        format!("{SECRET_PREFIX}../escape"),
        format!("{SECRET_PREFIX}Bad_Name"),
        format!("{SECRET_PREFIX}missing"),
    ] {
        let op = create(
            IncusKind::Instance,
            "svc",
            None,
            "s3",
            json!({"config": {"user.key": reference}}),
        );
        assert!(matches!(
            provider.plan(&entry("toy"), &op),
            Err(EffectRefusal::Provider(_))
        ));
    }
    assert!(writes(&fake).is_empty());
}

#[test]
fn a_secret_file_that_is_a_symlink_or_is_not_mode_0600_is_refused_at_plan() {
    let (scratch, fake) = start("secret-mode");
    let secrets = secrets_dir(&scratch);
    write_secret(&secrets, "loose", SECRET_VALUE);
    fs::set_permissions(secrets.join("loose"), fs::Permissions::from_mode(0o644))
        .expect("mode 0644");
    symlink(secrets.join("loose"), secrets.join("linked")).expect("make the symlink");
    let provider = provider(&fake, &secrets);
    for name in ["loose", "linked"] {
        let op = create(
            IncusKind::Instance,
            "svc",
            None,
            "s4",
            json!({"config": {"user.key": format!("{SECRET_PREFIX}{name}")}}),
        );
        assert!(
            matches!(
                provider.plan(&entry("toy"), &op),
                Err(EffectRefusal::Provider(_))
            ),
            "{name}"
        );
    }
    assert!(writes(&fake).is_empty());
}

// ---------------------------------------------------------------------------------------------------------------------------
// Outcomes that are not known: a write that answered with an error, or timed out, may have landed
// ---------------------------------------------------------------------------------------------------------------------------

/// Answers scripted replies in order, records each request's method and target, and stops when dropped.
struct Script {
    _dir: Scratch,
    socket: PathBuf,
    seen: Arc<Mutex<Vec<(String, String)>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Script {
    fn start(tag: &str, mut answer: impl FnMut(&str, &str) -> Vec<u8> + Send + 'static) -> Script {
        let dir = Scratch::new(tag);
        let socket = dir.0.join("script.sock");
        let listener = UnixListener::bind(&socket).expect("bind");
        listener.set_nonblocking(true).expect("non-blocking");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let seen = Arc::clone(&seen);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => serve_one(stream, &mut answer, &seen),
                        Err(_) => thread::sleep(Duration::from_millis(2)),
                    }
                }
            })
        };
        Script {
            _dir: dir,
            socket,
            seen,
            stop,
            thread: Some(thread),
        }
    }

    fn provider(&self, secrets: &Path) -> IncusProvider {
        IncusProvider::new(
            self.socket.clone(),
            family(),
            secrets.to_path_buf(),
            RetainedSet::empty(),
        )
        .expect("a valid family")
    }

    fn seen(&self) -> Vec<(String, String)> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn count(&self, method: &str) -> usize {
        self.seen().iter().filter(|(m, _)| m == method).count()
    }
}

impl Drop for Script {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve_one(
    mut stream: UnixStream,
    answer: &mut impl FnMut(&str, &str) -> Vec<u8>,
    seen: &Mutex<Vec<(String, String)>>,
) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(1) => head.push(byte[0]),
            _ => return,
        }
    }
    let text = String::from_utf8_lossy(&head).into_owned();
    let mut parts = text.lines().next().unwrap_or("").split(' ');
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    let length = text
        .lines()
        .find_map(|line| line.strip_prefix("Content-Length: "))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    if stream.read_exact(&mut body).is_err() {
        return;
    }
    seen.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push((method.clone(), target.clone()));
    let reply = answer(&method, &target);
    let _ = stream.write_all(&reply);
}

fn http(status: u16, etag: Option<&str>, envelope: &Value) -> Vec<u8> {
    let text = envelope.to_string();
    let tag = etag.map(|t| format!("ETag: {t}\r\n")).unwrap_or_default();
    format!(
        "HTTP/1.1 {status} Reply\r\n{tag}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        text.len()
    )
    .into_bytes()
}

fn sync_env(metadata: Value) -> Value {
    json!({"type": "sync", "status": "Success", "status_code": 200, "operation": "", "error_code": 0, "error": "", "metadata": metadata})
}

fn error_env(status: u16) -> Value {
    json!({"type": "error", "status": "Failure", "status_code": status, "operation": "", "error_code": status, "error": "refused", "metadata": null})
}

/// An object's answer to a GET: its metadata with the ETag the object carries.
fn object_reply(metadata: &Value, etag: &str) -> Vec<u8> {
    http(200, Some(etag), &sync_env(metadata.clone()))
}

fn ok_reply() -> Vec<u8> {
    http(200, None, &sync_env(Value::Null))
}

fn not_found() -> Vec<u8> {
    http(404, None, &error_env(404))
}

fn instance_meta(config: Value) -> Value {
    json!({"name": "c1", "project": "svc", "config": config})
}

#[test]
fn a_create_whose_answer_is_an_error_and_whose_read_back_finds_nothing_is_a_failed_effect() {
    let script = Script::start("create-failed", move |method, _| {
        if method == "POST" {
            return http(500, None, &error_env(500));
        }
        not_found()
    });
    let secrets = Scratch::new("secrets-failed");
    let mut provider = script.provider(&secrets.0);
    let op = create(
        IncusKind::Instance,
        "svc",
        None,
        "c1",
        json!({"config": {MARK: "toy"}}),
    );
    let planned = provider.plan(&entry("toy"), &op).expect("plan");
    let result = provider.effect(&entry("toy"), &op, &planned);
    assert!(
        matches!(result, Err(EffectRefusal::Provider(_))),
        "the create did not land: {result:?}"
    );
    assert_eq!(script.count("POST"), 1);
}

#[test]
fn a_create_that_timed_out_stays_open_even_when_the_read_back_finds_nothing() {
    // A timed-out wait says nothing about the object: a create still in progress can land after the read.
    let script = Script::start("create-timeout", move |method, target| {
        if method == "POST" {
            return http(
                202,
                None,
                &json!({"type": "async", "status": "Operation created", "status_code": 100, "operation": "/1.0/operations/00000000-0000-4000-8000-0000000000ab", "error_code": 0, "error": "", "metadata": null}),
            );
        }
        if target.contains("/wait") {
            return http(
                200,
                None,
                &json!({"type": "sync", "status": "Success", "status_code": 200, "operation": "", "error_code": 0, "error": "", "metadata": {"id": "00000000-0000-4000-8000-0000000000ab", "status_code": 103, "status": "Running", "err": ""}}),
            );
        }
        not_found()
    });
    let secrets = Scratch::new("secrets-timeout");
    let mut provider = script
        .provider(&secrets.0)
        .with_operation_deadline(Duration::from_millis(300));
    let op = create(
        IncusKind::Instance,
        "svc",
        None,
        "c2",
        json!({"config": {MARK: "toy"}}),
    );
    let planned = provider.plan(&entry("toy"), &op).expect("plan");
    let result = provider.effect(&entry("toy"), &op, &planned);
    assert_eq!(
        result,
        Err(EffectRefusal::Unresolved),
        "a timed-out create is not known"
    );
}

#[test]
fn a_create_whose_read_back_finds_an_object_is_unresolved_not_failed() {
    let mut posted = false;
    let script = Script::start("create-landed", move |method, target| {
        if method == "POST" {
            posted = true;
            return http(500, None, &error_env(500));
        }
        if method == "GET" && target.starts_with("/1.0/instances/c3") {
            return if posted {
                object_reply(
                    &instance_meta(json!({MARK: "toy", "volatile.uuid": READ_UUID})),
                    "\"e9\"",
                )
            } else {
                not_found()
            };
        }
        not_found()
    });
    let secrets = Scratch::new("secrets-landed");
    let mut provider = script.provider(&secrets.0);
    let op = create(
        IncusKind::Instance,
        "svc",
        None,
        "c3",
        json!({"config": {MARK: "toy"}}),
    );
    let planned = provider.plan(&entry("toy"), &op).expect("plan");
    assert_eq!(
        provider.effect(&entry("toy"), &op, &planned),
        Err(EffectRefusal::Unresolved)
    );
}

#[test]
fn a_patch_answered_412_is_stale_and_not_unresolved() {
    let script = Script::start("patch-412", move |method, target| {
        if method == "GET" && target.starts_with("/1.0/instances/p9") {
            return object_reply(&instance_meta(json!({MARK: "toy"})), "\"e1\"");
        }
        if method == "PATCH" {
            return http(412, None, &error_env(412));
        }
        not_found()
    });
    let secrets = Scratch::new("secrets-412");
    let mut provider = script.provider(&secrets.0);
    let op = patch(
        object(IncusKind::Instance, "svc", None, "p9"),
        &[("user.note", Some("x"))],
    );
    let planned = provider.plan(&entry("toy"), &op).expect("plan");
    assert_eq!(
        provider.effect(&entry("toy"), &op, &planned),
        Err(EffectRefusal::Stale)
    );
}

// ---------------------------------------------------------------------------------------------------------------------------
// Retirement: the tombstone and the delete, in order, with drift and staleness stopping the delete
// ---------------------------------------------------------------------------------------------------------------------------

#[test]
fn retire_tombstones_then_deletes_an_owned_instance_in_order() {
    let (scratch, fake) = start("retire");
    let provider = provider(&fake, &secrets_dir(&scratch));
    let writer = arca_incus::UnixIncusWriter::new(
        fake.socket().to_path_buf(),
        entry("toy"),
        BTreeMap::new(),
        arca_incus::ProjectFamily::new(["svc"], ["droid"]).expect("family"),
    );
    let made = writer
        .create(
            &object(IncusKind::Instance, "svc", None, "r1"),
            &json!({"config": {MARK: "toy"}}),
        )
        .expect("create");
    let uuid = made.uuid().map(str::to_string);
    assert!(uuid.is_some(), "an instance has a uuid");
    let plan = Digest::of(b"the approved plan");
    // The approved plan names the instance by its uuid, so the retirement names it too.
    let named = ObjectRef {
        uuid: uuid.clone(),
        ..made.target().clone()
    };
    let outcome = provider.retire(&entry("toy"), &named, &plan);
    assert_eq!(
        outcome,
        Ok(Retirement::Deleted(ObjectRef {
            uuid,
            ..object(IncusKind::Instance, "svc", None, "r1")
        }))
    );
    let methods: Vec<String> = writes(&fake).into_iter().map(|(m, _)| m).collect();
    assert_eq!(
        methods,
        vec![
            "POST".to_string(),
            "PATCH".to_string(),
            "DELETE".to_string()
        ]
    );
    let gone = UnixIncus::new(fake.socket().to_path_buf()).get("/1.0/instances/r1?project=svc");
    assert!(
        matches!(gone, Err(arca_incus::IncusError::Status { code: 404, .. })),
        "{gone:?}"
    );
}

/// The instance `r2` as a read sees it, owned by `toy` with the given uuid and tombstone.
fn r2(uuid: &str, tombstone: Option<&str>) -> Value {
    let mut config = json!({MARK: "toy", "volatile.uuid": uuid});
    if let Some(plan) = tombstone {
        config[RETIRING] = json!(plan);
    }
    instance_meta(config)
}

#[test]
fn retire_skips_the_delete_as_drift_when_the_delete_read_back_shows_another_uuid() {
    let plan = Digest::of(b"the approved plan");
    let plan_hex = plan.to_hex();
    let mut step = 0;
    let script = Script::start("drift-uuid", move |method, _| {
        step += 1;
        match (method, step) {
            ("GET", 1) => object_reply(&r2(READ_UUID, None), "\"e1\""),
            ("PATCH", 2) => ok_reply(),
            ("GET", 3) => object_reply(&r2(READ_UUID, Some(&plan_hex)), "\"e2\""),
            ("GET", 4) => object_reply(&r2(OTHER_UUID, Some(&plan_hex)), "\"e3\""),
            _ => not_found(),
        }
    });
    let secrets = Scratch::new("secrets-drift");
    let provider = script.provider(&secrets.0);
    let target = ObjectRef {
        uuid: Some(READ_UUID.to_string()),
        ..object(IncusKind::Instance, "svc", None, "r2")
    };
    let outcome = provider
        .retire(&entry("toy"), &target, &plan)
        .expect("the retirement reports drift");
    assert!(matches!(outcome, Retirement::Drift(_)), "{outcome:?}");
    assert_eq!(script.count("DELETE"), 0, "no DELETE was sent");
}

#[test]
fn retire_skips_the_delete_as_drift_when_the_tombstone_is_gone_at_delete() {
    let plan = Digest::of(b"the approved plan");
    let plan_hex = plan.to_hex();
    let mut step = 0;
    let script = Script::start("drift-tombstone", move |method, _| {
        step += 1;
        match (method, step) {
            ("GET", 1) => object_reply(&r2(READ_UUID, None), "\"e1\""),
            ("PATCH", 2) => ok_reply(),
            ("GET", 3) => object_reply(&r2(READ_UUID, Some(&plan_hex)), "\"e2\""),
            ("GET", 4) => object_reply(&r2(READ_UUID, None), "\"e4\""),
            _ => not_found(),
        }
    });
    let secrets = Scratch::new("secrets-drift2");
    let provider = script.provider(&secrets.0);
    let target = ObjectRef {
        uuid: Some(READ_UUID.to_string()),
        ..object(IncusKind::Instance, "svc", None, "r2")
    };
    let outcome = provider
        .retire(&entry("toy"), &target, &plan)
        .expect("the retirement reports drift");
    assert!(matches!(outcome, Retirement::Drift(_)), "{outcome:?}");
    assert_eq!(script.count("DELETE"), 0);
}

#[test]
fn retire_stops_on_a_stale_etag_and_sends_no_delete() {
    let plan = Digest::of(b"the approved plan");
    let script = Script::start("retire-412", move |method, _| match method {
        "GET" => object_reply(&r2(READ_UUID, None), "\"e1\""),
        "PATCH" => http(412, None, &error_env(412)),
        _ => not_found(),
    });
    let secrets = Scratch::new("secrets-412b");
    let provider = script.provider(&secrets.0);
    let target = ObjectRef {
        uuid: Some(READ_UUID.to_string()),
        ..object(IncusKind::Instance, "svc", None, "r2")
    };
    assert_eq!(
        provider.retire(&entry("toy"), &target, &plan),
        Err(EffectRefusal::Stale)
    );
    assert_eq!(script.count("DELETE"), 0);
}

// ---------------------------------------------------------------------------------------------------------------------------
// The fixtures a test relies on must hold: the object families, and the fixture the provider tests build on
// ---------------------------------------------------------------------------------------------------------------------------

#[test]
fn a_retained_entrys_object_is_refused_for_a_patch_and_a_retirement_and_nothing_is_written() {
    let (scratch, fake) = start("retained");
    // The object is made while nothing is retained, so it carries a uuid for the retirement to name.
    let writer = arca_incus::UnixIncusWriter::new(
        fake.socket().to_path_buf(),
        entry("toy"),
        BTreeMap::new(),
        arca_incus::ProjectFamily::new(["svc"], ["droid"]).expect("family"),
    );
    let made = writer
        .create(
            &object(IncusKind::Instance, "svc", None, "rk1"),
            &json!({"config": {MARK: "toy"}}),
        )
        .expect("create");
    let before = writes(&fake).len();
    let retained = BTreeMap::from([(
        entry("toy"),
        (PluginId("toy-plugin".to_string()), Digest::of(b"declined")),
    )]);
    let provider = IncusProvider::new(
        fake.socket().to_path_buf(),
        family(),
        secrets_dir(&scratch),
        RetainedSet::new(retained),
    )
    .expect("a valid family");
    let rk1 = object(IncusKind::Instance, "svc", None, "rk1");
    let op = patch(rk1.clone(), &[("user.note", Some("x"))]);
    let planned = provider.plan(&entry("toy"), &op);
    assert!(
        matches!(
            planned,
            Err(EffectRefusal::NotOwned(OwnerClass::Retained { .. }))
        ),
        "{planned:?}"
    );
    let named = ObjectRef {
        uuid: made.uuid().map(str::to_string),
        ..rk1
    };
    let retired = provider.retire(&entry("toy"), &named, &Digest::of(b"approved"));
    assert!(
        matches!(
            retired,
            Err(EffectRefusal::NotOwned(OwnerClass::Retained { .. }))
        ),
        "{retired:?}"
    );
    assert_eq!(
        writes(&fake).len(),
        before,
        "nothing was written after the create"
    );
}

#[test]
fn a_patch_that_sets_the_tombstone_a_volatile_key_or_another_owners_mark_is_refused_at_plan() {
    let (scratch, fake) = start("patch-keys");
    let provider = provider(&fake, &secrets_dir(&scratch));
    let k1 = object(IncusKind::Instance, "svc", None, "k1");
    for key in [RETIRING, "volatile.uuid", "user.droid.x"] {
        let op = patch(k1.clone(), &[(key, Some("1"))]);
        assert!(
            matches!(
                provider.plan(&entry("toy"), &op),
                Err(EffectRefusal::Provider(_))
            ),
            "{key}"
        );
    }
    assert!(writes(&fake).is_empty(), "nothing was written");
}

#[test]
fn a_create_whose_body_carries_a_retiring_tombstone_is_refused_at_plan_and_sends_nothing() {
    let (scratch, fake) = start("create-tombstone");
    let provider = provider(&fake, &secrets_dir(&scratch));
    let op = create(
        IncusKind::Instance,
        "svc",
        None,
        "i10",
        json!({"config": {MARK: "toy", RETIRING: "abc"}}),
    );
    assert!(matches!(
        provider.plan(&entry("toy"), &op),
        Err(EffectRefusal::Provider(_))
    ));
    assert!(writes(&fake).is_empty(), "nothing was written");
}

#[test]
fn a_create_whose_body_is_not_an_object_or_holds_a_config_value_that_is_not_text_is_refused_at_plan()
 {
    let (scratch, fake) = start("create-shape");
    let provider = provider(&fake, &secrets_dir(&scratch));
    for body in [
        json!("not an object"),
        json!({"config": "text"}),
        json!({"config": {"user.n": 3}}),
    ] {
        let op = create(IncusKind::Instance, "svc", None, "i11", body.clone());
        assert!(
            matches!(
                provider.plan(&entry("toy"), &op),
                Err(EffectRefusal::Provider(_))
            ),
            "{body}"
        );
    }
    assert!(writes(&fake).is_empty(), "nothing was written");
}

#[test]
fn the_fixture_gives_the_objects_the_tests_name_their_owners() {
    let (scratch, fake) = start("fixture-owners");
    let writer = arca_incus::UnixIncusWriter::new(
        fake.socket().to_path_buf(),
        entry("toy"),
        BTreeMap::new(),
        arca_incus::ProjectFamily::new(["svc"], ["droid"]).expect("family"),
    );
    let read_class = |name: &str| {
        writer
            .read_class(&object(IncusKind::Instance, "svc", None, name))
            .expect("a read")
            .class
    };
    assert_eq!(read_class("db1"), OwnerClass::Droidd);
    assert_eq!(read_class("guest1"), OwnerClass::Foreign);
    assert_eq!(read_class("x1"), OwnerClass::Contested);
    assert_eq!(
        read_class("k1"),
        OwnerClass::Kernel {
            entry: entry("toy")
        }
    );
    let _ = scratch;
}

// ---------------------------------------------------------------------------------------------------------------------------
// Project keys: the zones feature, and no plugin key that loosens the pinned project
// ---------------------------------------------------------------------------------------------------------------------------

/// The fixture with one more kernel project, `kzone`, that enables network zones as well as networks.
fn zone_fixture() -> FakeState {
    let mut state = state();
    state.projects.push(json!({
        "name": "kzone", "description": "", "config": features(&[
            ("features.networks", "true"),
            ("features.networks.zones", "true"),
            ("features.profiles", "true"),
            ("features.storage.volumes", "true"),
            ("features.storage.buckets", "true"),
        ])
    }));
    state
}

#[test]
fn a_zone_write_is_refused_before_any_write_in_a_project_without_the_zones_key() {
    let dir = Scratch::new("zones");
    let fake = FakeIncus::start(&dir.0, zone_fixture()).expect("start the fake");
    let secrets = secrets_dir(&dir);
    let fam = Family {
        projects: vec!["svc".into(), "kzone".into()],
        ..family()
    };
    let provider = IncusProvider::new(
        fake.socket().to_path_buf(),
        fam,
        secrets,
        RetainedSet::empty(),
    )
    .expect("a valid family");
    // `svc` enables networks but not network zones, so a zone there would route to the default project.
    let in_svc = create(IncusKind::Zone, "svc", None, "z1", json!({}));
    assert!(
        matches!(
            provider.plan(&entry("toy"), &in_svc),
            Err(EffectRefusal::Provider(_))
        ),
        "a zone without the zones key is refused"
    );
    assert!(
        writes(&fake).is_empty(),
        "no zone was written: {:?}",
        writes(&fake)
    );
    // `kzone` enables the zones key, so the same zone plans and is created there.
    let in_kzone = create(IncusKind::Zone, "kzone", None, "z1", json!({}));
    let planned = provider
        .plan(&entry("toy"), &in_kzone)
        .expect("a zone in a project with the zones key plans");
    let mut provider = provider;
    provider
        .effect(&entry("toy"), &in_kzone, &planned)
        .expect("the zone is created");
    assert_eq!(
        writes(&fake),
        vec![(
            "POST".to_string(),
            "/1.0/network-zones?project=kzone".to_string()
        )]
    );
}

#[test]
fn a_project_create_refuses_a_restricted_or_features_key_that_loosens_the_pinned_project() {
    let (scratch, fake) = start("project-loosen");
    let provider = provider(&fake, &secrets_dir(&scratch));
    for (key, value) in [
        ("restricted", "false"),
        ("restricted.containers.privilege", "allow"),
        ("restricted.devices.disk", "allow"),
        ("restricted.containers.lowlevel", "allow"),
        ("features.images", "false"),
        ("features.networks", "false"),
        ("features.networks.zones", "false"),
    ] {
        let op = create(
            IncusKind::Project,
            "kfam",
            None,
            "kfam",
            json!({"config": {key: value}}),
        );
        assert!(
            matches!(
                provider.plan(&entry("toy"), &op),
                Err(EffectRefusal::Provider(_))
            ),
            "{key}={value} is refused"
        );
    }
    // The pinned values are accepted, and none of these requests reaches the fake.
    let pinned = create(
        IncusKind::Project,
        "kfam",
        None,
        "kfam",
        json!({"config": {"restricted": "true", "features.networks.zones": "true"}}),
    );
    provider
        .plan(&entry("toy"), &pinned)
        .expect("the pinned values plan");
    assert!(
        fake.requests().is_empty(),
        "no request was made: {:?}",
        fake.requests()
    );
}

#[test]
fn a_project_patch_refuses_every_restricted_or_features_key_before_any_request() {
    let (scratch, fake) = start("project-patch");
    let provider = provider(&fake, &secrets_dir(&scratch));
    let project = object(IncusKind::Project, "svc", None, "svc");
    for (key, value) in [
        ("restricted", Some("false")),
        ("restricted.containers.privilege", Some("allow")),
        ("features.images", Some("false")),
        ("features.networks", Some("true")),
        ("features.networks.zones", None),
        ("restricted", None),
    ] {
        let op = patch(project.clone(), &[(key, value)]);
        assert!(
            matches!(
                provider.plan(&entry("toy"), &op),
                Err(EffectRefusal::Provider(_))
            ),
            "{key} is refused on a project patch"
        );
    }
    assert!(
        fake.requests().is_empty(),
        "no request was made: {:?}",
        fake.requests()
    );
}

#[test]
fn a_project_create_carries_the_zones_key_pinned_to_true() {
    let (scratch, fake) = start("project-zones");
    let mut provider = provider(&fake, &secrets_dir(&scratch));
    let manifest = manifest();
    let mut journal = Journal::open(MemJournal::new()).expect("opens");
    Ctx::new(entry("toy"), &manifest, &mut provider, &mut journal, 1)
        .effect(create(
            IncusKind::Project,
            "kfam",
            None,
            "kfam",
            json!({"config": {}}),
        ))
        .expect("the project is created");
    let config = read(&fake, "/1.0/projects/kfam")["config"].clone();
    assert_eq!(config["features.networks.zones"], json!("true"), "{config}");
}

// ---------------------------------------------------------------------------------------------------------------------------
// By-name patches: the answer and the inverse name the object as the op did
// ---------------------------------------------------------------------------------------------------------------------------

#[test]
fn a_by_name_patch_of_an_instance_with_a_read_back_uuid_lands_and_its_inverse_names_the_op_object()
{
    let (scratch, fake) = start("by-name");
    let secrets = secrets_dir(&scratch);
    write_secret(&secrets, "n1", SECRET_VALUE);
    let mut provider = provider(&fake, &secrets);
    let manifest = manifest();
    let mut journal = Journal::open(MemJournal::new()).expect("opens");
    Ctx::new(entry("toy"), &manifest, &mut provider, &mut journal, 1)
        .effect(create(
            IncusKind::Instance,
            "svc",
            None,
            "bn1",
            json!({"config": {MARK: "toy"}}),
        ))
        .expect("the instance is created");
    // The instance now carries a uuid the read-back shows. The op names it by name only. The value is a secret reference, so
    // the reverse patch may replace it.
    let by_name = object(IncusKind::Instance, "svc", None, "bn1");
    let reference = format!("{SECRET_PREFIX}n1");
    let answered = Ctx::new(entry("toy"), &manifest, &mut provider, &mut journal, 2)
        .effect(patch(
            by_name.clone(),
            &[("user.note", Some(reference.as_str()))],
        ))
        .expect("the by-name patch applies");
    assert_eq!(
        answered, by_name,
        "the answer names the object as the op did"
    );
    assert!(journal.unresolved().is_empty(), "no intent stays open");
    let restore = journal
        .live(&entry("toy"))
        .into_iter()
        .find_map(|record| match record.inverse {
            Some(Inverse::IncusRestoreConfig { object, config }) if object.name == "bn1" => {
                Some((object, config))
            }
            _ => None,
        })
        .expect("the patch is journalled with its restore");
    assert_eq!(restore, (by_name, changes(&[("user.note", None)])));
    Ctx::new(entry("toy"), &manifest, &mut provider, &mut journal, 3)
        .effect(patch(restore.0, &[("user.note", None)]))
        .expect("the reverse patch applies");
    let config = read(&fake, "/1.0/instances/bn1?project=svc")["config"].clone();
    assert!(config.get("user.note").is_none(), "{config}");
}

// ---------------------------------------------------------------------------------------------------------------------------
// Failed creates: only a status error that a GET settles as absent is recorded as failed
// ---------------------------------------------------------------------------------------------------------------------------

#[test]
fn a_create_whose_wait_fails_without_an_answer_stays_unresolved_when_the_name_is_free() {
    // The request was accepted and named an operation. The wait then gives no answer the writer can read. The name is still
    // free, but a transport or decode failure says nothing about whether the create landed.
    let waits: [(&str, Vec<u8>); 2] = [
        ("a garbled wait", b"not an http answer\r\n\r\n".to_vec()),
        ("a wait the server drops", Vec::new()),
    ];
    for (why, wait_reply) in waits {
        let script = Script::start("create-wait", move |method, target| {
            if method == "POST" {
                return http(
                    202,
                    None,
                    &json!({"type": "async", "status": "Operation created", "status_code": 100, "operation": "/1.0/operations/00000000-0000-4000-8000-0000000000ac", "error_code": 0, "error": "", "metadata": null}),
                );
            }
            if target.contains("/wait") {
                return wait_reply.clone();
            }
            not_found()
        });
        let secrets = Scratch::new("secrets-wait");
        let mut provider = script.provider(&secrets.0);
        let op = create(
            IncusKind::Instance,
            "svc",
            None,
            "c4",
            json!({"config": {MARK: "toy"}}),
        );
        let planned = provider.plan(&entry("toy"), &op).expect("plan");
        assert_eq!(
            provider.effect(&entry("toy"), &op, &planned),
            Err(EffectRefusal::Unresolved),
            "{why}"
        );
    }
}

#[test]
fn a_create_whose_answer_is_garbled_stays_unresolved_when_the_name_is_free() {
    let script = Script::start("create-garbled", move |method, _| {
        if method == "POST" {
            return b"not an http answer\r\n\r\n".to_vec();
        }
        not_found()
    });
    let secrets = Scratch::new("secrets-garbled");
    let mut provider = script.provider(&secrets.0);
    let op = create(
        IncusKind::Instance,
        "svc",
        None,
        "c5",
        json!({"config": {MARK: "toy"}}),
    );
    let planned = provider.plan(&entry("toy"), &op).expect("plan");
    assert_eq!(
        provider.effect(&entry("toy"), &op, &planned),
        Err(EffectRefusal::Unresolved)
    );
}

#[test]
fn a_create_whose_wait_answers_an_error_stays_unresolved_when_the_name_is_free() {
    // The wait request itself is answered 500, so the operation's status is unknown. The name is free, but that says nothing
    // about a create that may still be running.
    let script = Script::start("create-wait-500", move |method, target| {
        if method == "POST" {
            return http(
                202,
                None,
                &json!({"type": "async", "status": "Operation created", "status_code": 100, "operation": "/1.0/operations/00000000-0000-4000-8000-0000000000ad", "error_code": 0, "error": "", "metadata": null}),
            );
        }
        if target.contains("/wait") {
            return http(500, None, &error_env(500));
        }
        not_found()
    });
    let secrets = Scratch::new("secrets-wait-500");
    let mut provider = script.provider(&secrets.0);
    let op = create(
        IncusKind::Instance,
        "svc",
        None,
        "c6",
        json!({"config": {MARK: "toy"}}),
    );
    let planned = provider.plan(&entry("toy"), &op).expect("plan");
    assert_eq!(
        provider.effect(&entry("toy"), &op, &planned),
        Err(EffectRefusal::Unresolved)
    );
    assert_eq!(script.count("POST"), 1);
}

/// Answers one connection of a patch test. A GET of the object is answered with the object as it stands. A PATCH's head is read
/// and then the connection is dropped with its body unread, so the client's read of the answer fails with a reset.
fn serve_patch_then_drop(mut stream: UnixStream, seen: &Mutex<Vec<(String, String)>>) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(1) => head.push(byte[0]),
            _ => return,
        }
    }
    let text = String::from_utf8_lossy(&head).into_owned();
    let mut parts = text.lines().next().unwrap_or("").split(' ');
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    seen.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push((method.clone(), target.clone()));
    if method == "PATCH" {
        return;
    }
    let reply = if method == "GET" && target == "/1.0/instances/io1?project=svc" {
        object_reply(&instance_meta(json!({MARK: "toy"})), "\"e1\"")
    } else {
        not_found()
    };
    let _ = stream.write_all(&reply);
}

#[test]
fn a_patch_whose_answer_is_an_io_error_is_unresolved_and_is_not_read_back() {
    // The object still shows its pre-image, which a read-back would report as "did not land". An I/O error says nothing about the
    // write, since an operation still running can apply after such a read, so the outcome is unresolved and no read follows.
    let dir = Scratch::new("patch-io");
    let socket = dir.0.join("patch-io.sock");
    let listener = UnixListener::bind(&socket).expect("bind");
    listener.set_nonblocking(true).expect("non-blocking");
    let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let server = {
        let seen = Arc::clone(&seen);
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => serve_patch_then_drop(stream, &seen),
                    Err(_) => thread::sleep(Duration::from_millis(2)),
                }
            }
        })
    };
    let secrets = Scratch::new("secrets-patch-io");
    let mut provider =
        IncusProvider::new(socket, family(), secrets.0.clone(), RetainedSet::empty())
            .expect("a valid family");
    let op = patch(
        object(IncusKind::Instance, "svc", None, "io1"),
        &[("user.note", Some("x"))],
    );
    let planned = provider.plan(&entry("toy"), &op).expect("plan");
    let result = provider.effect(&entry("toy"), &op, &planned);
    stop.store(true, Ordering::SeqCst);
    server.join().expect("the server thread");
    assert_eq!(result, Err(EffectRefusal::Unresolved));
    let seen = seen.lock().unwrap_or_else(PoisonError::into_inner).clone();
    assert_eq!(
        seen.last().map(|(method, _)| method.as_str()),
        Some("PATCH"),
        "no read-back followed the I/O error: {seen:?}"
    );
}

// ---------------------------------------------------------------------------------------------------------------------------
// Bridges: only the keys the plan's bridges set, and no reserved route, external interface or raw key
// ---------------------------------------------------------------------------------------------------------------------------

#[test]
fn a_bridge_key_outside_the_allowlist_or_a_route_external_interface_or_raw_key_is_refused_at_plan()
{
    let (scratch, fake) = start("bridge-keys");
    let provider = provider(&fake, &secrets_dir(&scratch));
    let bridge_with = |key: &str, value: &str| {
        let mut config = json!({"ipv4.address": "192.0.2.1/24", "ipv6.address": "none"});
        config[key] = json!(value);
        create(
            IncusKind::Network,
            "default",
            None,
            "br-x",
            json!({"type": "bridge", "config": config}),
        )
    };
    for (key, value) in [
        ("ipv4.routes", "198.51.100.0/24"),
        ("ipv4.routes", "203.0.113.0/24"),
        ("ipv6.routes", "2001:db8::/32"),
        ("bridge.external_interfaces", "eth9"),
        ("raw.dnsmasq", "dhcp-option=6,192.0.2.53"),
        ("raw.anything", "x"),
        ("dns.mode", "none"),
        ("dns.domain", "arca.test"),
        ("ipv6.nat", "true"),
        ("ipv4.dhcp", "true"),
    ] {
        assert!(
            matches!(
                provider.plan(&entry("toy"), &bridge_with(key, value)),
                Err(EffectRefusal::Provider(_))
            ),
            "{key}={value} is refused"
        );
    }
    provider
        .plan(&entry("toy"), &bridge_with("ipv4.nat", "true"))
        .expect("ipv4.nat is on the allowlist");
    assert!(writes(&fake).is_empty(), "no bridge was created");
}

#[test]
fn a_bridge_patch_into_a_reserved_range_is_refused_at_plan_and_sends_no_patch() {
    let (scratch, fake) = start("bridge-patch");
    let mut provider = provider(&fake, &secrets_dir(&scratch));
    let bridge = create(
        IncusKind::Network,
        "default",
        None,
        "br-p",
        json!({"type": "bridge", "config": {"ipv4.address": "192.0.2.1/24", "ipv6.address": "none"}}),
    );
    let planned = provider
        .plan(&entry("toy"), &bridge)
        .expect("the bridge plans");
    provider
        .effect(&entry("toy"), &bridge, &planned)
        .expect("the bridge is created");
    let move_into_reserved = patch(
        object(IncusKind::Network, "default", None, "br-p"),
        &[("ipv4.address", Some("198.51.100.1/24"))],
    );
    match provider.plan(&entry("toy"), &move_into_reserved) {
        Err(EffectRefusal::Provider(reason)) => {
            assert!(
                reason.contains("reserved"),
                "the refusal names the range: {reason}"
            )
        }
        other => panic!("a patch into a reserved range is refused: {other:?}"),
    }
    assert_eq!(
        writes(&fake),
        vec![(
            "POST".to_string(),
            "/1.0/networks?project=default".to_string()
        )],
        "only the create was sent"
    );
}

// ---------------------------------------------------------------------------------------------------------------------------
// Plain patches and secret rotation: a plain value is journalled as it is, a secret's content never is, and an inverse is a patch
// ---------------------------------------------------------------------------------------------------------------------------

/// The restore a journal holds for the object's patch: the first restore record in the entry's live records.
fn journalled_restore<S: JournalStore>(
    journal: &Journal<S>,
    entry_name: &str,
) -> BTreeMap<String, Option<String>> {
    journal
        .live(&entry(entry_name))
        .into_iter()
        .find_map(|r| match r.inverse {
            Some(Inverse::IncusRestoreConfig { config, .. }) => Some(config),
            _ => None,
        })
        .expect("the patch is journalled with its restore")
}

#[test]
fn a_plain_patch_journals_a_removal_inverse_that_runs_as_a_patch() {
    let (scratch, fake) = start("plain-add");
    let secrets = secrets_dir(&scratch);
    let mut provider = provider(&fake, &secrets);
    let manifest = manifest();
    let mut journal = Journal::open(MemJournal::new()).expect("opens");
    let made = Ctx::new(entry("toy"), &manifest, &mut provider, &mut journal, 1)
        .effect(create(
            IncusKind::Instance,
            "svc",
            None,
            "pa1",
            json!({"config": {MARK: "toy"}}),
        ))
        .expect("the create applies");
    let target = ObjectRef {
        uuid: made.uuid.clone(),
        ..made
    };
    Ctx::new(entry("toy"), &manifest, &mut provider, &mut journal, 2)
        .effect(patch(target.clone(), &[("user.note", Some("plain"))]))
        .expect("the plain patch lands");
    let restore = journalled_restore(&journal, "toy");
    assert_eq!(restore, changes(&[("user.note", None)]));
    Ctx::new(entry("toy"), &manifest, &mut provider, &mut journal, 3)
        .effect(EffectOp::IncusPatchConfig {
            object: target.clone(),
            config: restore,
        })
        .expect("the inverse runs as a patch");
    let path = format!("/1.0/instances/{}?project=svc", target.name);
    assert!(read(&fake, &path)["config"]["user.note"].is_null());
}

#[test]
fn a_landed_plain_patch_can_be_planned_again() {
    let (scratch, fake) = start("plain-replan");
    let secrets = secrets_dir(&scratch);
    let mut provider = provider(&fake, &secrets);
    let op = patch(
        object(IncusKind::Instance, "svc", None, "k1"),
        &[("user.note", Some("plain"))],
    );
    let planned = provider.plan(&entry("toy"), &op).expect("the first plan");
    provider
        .effect(&entry("toy"), &op, &planned)
        .expect("the patch lands");
    let replanned = provider
        .plan(&entry("toy"), &op)
        .expect("a patch that already landed plans again");
    assert_eq!(
        replanned.pre_image,
        Some(changes(&[("user.note", Some("plain"))]))
    );
    provider
        .effect(&entry("toy"), &op, &replanned)
        .expect("and applies again");
}

#[test]
fn a_plain_value_that_a_patch_replaces_is_journalled_as_it_is_and_its_inverse_restores_it() {
    const OLD: &str = "PLAIN-OLD-VALUE-7713";
    let (scratch, fake) = start("plain-old");
    let secrets = secrets_dir(&scratch);
    let mut provider = provider(&fake, &secrets);
    let manifest = manifest();
    let mut journal = Journal::open(MemJournal::new()).expect("opens");
    let made = Ctx::new(entry("toy"), &manifest, &mut provider, &mut journal, 1)
        .effect(create(
            IncusKind::Instance,
            "svc",
            None,
            "pl1",
            json!({"config": {MARK: "toy", "user.note": OLD}}),
        ))
        .expect("the create applies");
    let target = ObjectRef {
        uuid: made.uuid.clone(),
        ..made
    };
    Ctx::new(entry("toy"), &manifest, &mut provider, &mut journal, 2)
        .effect(patch(target.clone(), &[("user.note", Some("new"))]))
        .expect("the replacement lands");
    let restore = journalled_restore(&journal, "toy");
    assert_eq!(restore, changes(&[("user.note", Some(OLD))]));
    Ctx::new(entry("toy"), &manifest, &mut provider, &mut journal, 3)
        .effect(EffectOp::IncusPatchConfig {
            object: target.clone(),
            config: restore,
        })
        .expect("the inverse restores the old value");
    let path = format!("/1.0/instances/{}?project=svc", target.name);
    assert_eq!(read(&fake, &path)["config"]["user.note"], json!(OLD));
}

#[test]
fn a_patch_that_rotates_a_secret_journals_no_value_and_its_inverse_removes_the_key() {
    const ROTATED: &str = "SECRET-VALUE-ROTATED-4409";
    let (scratch, fake) = start("rotation");
    let secrets = secrets_dir(&scratch);
    write_secret(&secrets, "t1", SECRET_VALUE);
    let reference = format!("{SECRET_PREFIX}t1");
    let mut provider = provider(&fake, &secrets);
    let manifest = manifest();
    let journal_path = scratch.0.join("journal.jsonl");
    let mut journal =
        Journal::open(FileJournal::open(&journal_path).expect("open")).expect("folds");
    let made = Ctx::new(entry("toy"), &manifest, &mut provider, &mut journal, 1)
        .effect(create(
            IncusKind::Instance,
            "svc",
            None,
            "rot1",
            json!({"config": {MARK: "toy", "user.key": reference}}),
        ))
        .expect("the create applies");
    // The file is rotated, and the entry sets the key to the reference again. The object holds the old content, which no
    // secret file holds now.
    write_secret(&secrets, "t1", ROTATED);
    let target = ObjectRef {
        uuid: made.uuid.clone(),
        ..made
    };
    Ctx::new(entry("toy"), &manifest, &mut provider, &mut journal, 2)
        .effect(patch(
            target.clone(),
            &[("user.key", Some(reference.as_str()))],
        ))
        .expect("the rotation applies");
    let on_disk = fs::read_to_string(&journal_path).expect("the journal file");
    assert!(
        !on_disk.contains(SECRET_VALUE) && !on_disk.contains(ROTATED),
        "the journal holds no secret value"
    );
    let restore = journalled_restore(&journal, "toy");
    assert_eq!(
        restore,
        changes(&[("user.key", None)]),
        "a redacted key restores as absent"
    );
    Ctx::new(entry("toy"), &manifest, &mut provider, &mut journal, 3)
        .effect(EffectOp::IncusPatchConfig {
            object: target.clone(),
            config: restore,
        })
        .expect("the inverse runs as a patch");
    let path = format!("/1.0/instances/{}?project=svc", target.name);
    assert!(read(&fake, &path)["config"]["user.key"].is_null());
}

#[test]
fn a_patch_that_sets_a_key_holding_a_current_secret_to_a_plain_value_journals_the_reference() {
    const PLAIN: &str = "plain-after-7702";
    let (scratch, fake) = start("secret-to-plain");
    let secrets = secrets_dir(&scratch);
    write_secret(&secrets, "t1", SECRET_VALUE);
    let reference = format!("{SECRET_PREFIX}t1");
    let mut provider = provider(&fake, &secrets);
    let manifest = manifest();
    let journal_path = scratch.0.join("journal.jsonl");
    let mut journal =
        Journal::open(FileJournal::open(&journal_path).expect("open")).expect("folds");
    let made = Ctx::new(entry("toy"), &manifest, &mut provider, &mut journal, 1)
        .effect(create(
            IncusKind::Instance,
            "svc",
            None,
            "sp1",
            json!({"config": {MARK: "toy", "user.key": reference}}),
        ))
        .expect("the create applies");
    let target = ObjectRef {
        uuid: made.uuid.clone(),
        ..made
    };
    Ctx::new(entry("toy"), &manifest, &mut provider, &mut journal, 2)
        .effect(patch(target, &[("user.key", Some(PLAIN))]))
        .expect("the plain value lands");
    let on_disk = fs::read_to_string(&journal_path).expect("the journal file");
    assert!(
        !on_disk.contains(SECRET_VALUE),
        "the journal holds no secret value"
    );
    assert_eq!(
        journalled_restore(&journal, "toy"),
        changes(&[("user.key", Some(reference.as_str()))]),
        "the old content is journalled as its reference"
    );
}

#[test]
fn a_plain_patch_over_a_reference_shaped_value_that_no_secret_matches_is_refused_and_nothing_is_written()
 {
    // A value that starts with the reference prefix and matches no secret would restore as a reference, so it is refused.
    let mut state = state();
    for instance in &mut state.instances {
        if instance["name"] == "k1" {
            instance["config"]["user.raw"] = json!(format!("{SECRET_PREFIX}absent"));
        }
    }
    let scratch = Scratch::new("reference-shaped");
    let fake = FakeIncus::start(&scratch.0, state).expect("start the fake");
    let secrets = secrets_dir(&scratch);
    let provider = provider(&fake, &secrets);
    let op = patch(
        object(IncusKind::Instance, "svc", None, "k1"),
        &[("user.raw", Some("plain"))],
    );
    assert!(
        matches!(
            provider.plan(&entry("toy"), &op),
            Err(EffectRefusal::Provider(_))
        ),
        "a reference-shaped value is not journalled as it is"
    );
    assert!(writes(&fake).is_empty(), "nothing was written");
}

// ---------------------------------------------------------------------------------------------------------------------------
// Retirement needs the uuid; an effect checks the class and the pre-image it is given
// ---------------------------------------------------------------------------------------------------------------------------

#[test]
fn retire_refuses_an_instance_named_without_its_uuid_before_any_request() {
    let (scratch, fake) = start("retire-no-uuid");
    let provider = provider(&fake, &secrets_dir(&scratch));
    let plan = Digest::of(b"approved");
    let target = object(IncusKind::Instance, "svc", None, "k1");
    assert!(
        matches!(
            provider.retire(&entry("toy"), &target, &plan),
            Err(EffectRefusal::Provider(_))
        ),
        "an instance without a uuid"
    );
    assert!(
        fake.requests().is_empty(),
        "no request was made: {:?}",
        fake.requests()
    );
}

#[test]
fn a_volume_without_a_uuid_is_made_and_retired_by_its_pool_and_name() {
    // Incus 7.3 gives a custom volume no uuid. Its create answers with none, its inverse deletes it by name, and its retirement
    // tombstones and deletes it by name.
    let (scratch, fake) = start("volume-by-name");
    let mut provider = provider(&fake, &secrets_dir(&scratch));
    let op = create(IncusKind::Volume, "svc", Some("fast"), "v1", json!({}));
    let planned = provider.plan(&entry("toy"), &op).expect("the plan");
    let (made, inverse) = provider
        .effect(&entry("toy"), &op, &planned)
        .expect("the volume is made");
    let by_name = object(IncusKind::Volume, "svc", Some("fast"), "v1");
    assert_eq!(made, by_name, "a volume carries no uuid");
    assert_eq!(
        inverse,
        Inverse::IncusDelete {
            object: by_name.clone()
        }
    );
    let volume = read(
        &fake,
        "/1.0/storage-pools/fast/volumes/custom/v1?project=svc",
    );
    assert!(volume["config"].get("volatile.uuid").is_none(), "{volume}");

    let plan = Digest::of(b"approved");
    assert_eq!(
        provider.retire(&entry("toy"), &by_name, &plan),
        Ok(Retirement::Deleted(by_name.clone()))
    );
    assert_eq!(
        writes(&fake),
        vec![
            (
                "POST".to_string(),
                "/1.0/storage-pools/fast/volumes/custom?project=svc".to_string()
            ),
            (
                "PATCH".to_string(),
                "/1.0/storage-pools/fast/volumes/custom/v1?project=svc".to_string()
            ),
            (
                "DELETE".to_string(),
                "/1.0/storage-pools/fast/volumes/custom/v1?project=svc".to_string()
            ),
        ]
    );
}

#[test]
fn an_effect_rechecks_the_class_and_the_pre_image_of_a_plan_made_by_hand() {
    let (scratch, fake) = start("forged-plan");
    let mut provider = provider(&fake, &secrets_dir(&scratch));
    let op = patch(
        object(IncusKind::Instance, "svc", None, "k1"),
        &[("user.note", Some("x"))],
    );
    let planned = provider.plan(&entry("toy"), &op).expect("the plan is real");
    assert_eq!(planned.class, EffectClass::B);
    let wrong_class = Planned {
        class: EffectClass::A,
        ..planned.clone()
    };
    assert!(
        matches!(
            provider.effect(&entry("toy"), &op, &wrong_class),
            Err(EffectRefusal::Provider(_))
        ),
        "a class the kind does not give is refused"
    );
    let wrong_pre_image = Planned {
        pre_image: Some(changes(&[("user.note", Some("forged"))])),
        ..planned
    };
    assert!(
        matches!(
            provider.effect(&entry("toy"), &op, &wrong_pre_image),
            Err(EffectRefusal::Provider(_))
        ),
        "a pre-image the object does not hold is refused"
    );
    assert!(
        writes(&fake).is_empty(),
        "no patch was sent: {:?}",
        writes(&fake)
    );
}

// ---------------------------------------------------------------------------------------------------------------------------
// The secrets directory itself: not a symlink, and private
// ---------------------------------------------------------------------------------------------------------------------------

#[test]
fn a_secrets_directory_that_is_a_symlink_or_is_readable_by_others_is_refused_at_plan() {
    let scratch = Scratch::new("secrets-dir");
    let (_fake_dir, fake) = start("secrets-dir-fake");
    let real = scratch.0.join("real");
    fs::create_dir_all(&real).expect("the private directory");
    fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).expect("mode 0700");
    write_secret(&real, "t1", SECRET_VALUE);
    let linked = scratch.0.join("linked");
    symlink(&real, &linked).expect("the symlink");
    let loose = scratch.0.join("loose");
    fs::create_dir_all(&loose).expect("the loose directory");
    fs::set_permissions(&loose, fs::Permissions::from_mode(0o755)).expect("mode 0755");
    write_secret(&loose, "t1", SECRET_VALUE);
    let op = create(
        IncusKind::Instance,
        "svc",
        None,
        "s5",
        json!({"config": {"user.key": format!("{SECRET_PREFIX}t1")}}),
    );
    // The private directory resolves the reference, so the refusals below are about the directories and nothing else.
    provider(&fake, &real)
        .plan(&entry("toy"), &op)
        .expect("the private directory resolves the reference");
    for (why, dir) in [("a symlink", &linked), ("readable by others", &loose)] {
        assert!(
            matches!(
                provider(&fake, dir).plan(&entry("toy"), &op),
                Err(EffectRefusal::Provider(_))
            ),
            "{why} is refused"
        );
    }
    assert!(writes(&fake).is_empty(), "nothing was written");
}

#[test]
fn a_write_is_refused_once_the_retained_set_is_updated_through_a_handle_the_boxed_provider_shares()
{
    // A reconciler boxes the provider and keeps a clone of the set. A plan declined after the boxing must reach the provider.
    let (scratch, fake) = start("retained-handle");
    let retained = RetainedSet::empty();
    let provider = IncusProvider::new(
        fake.socket().to_path_buf(),
        family(),
        secrets_dir(&scratch),
        retained.clone(),
    )
    .expect("a valid family");
    let boxed: Box<dyn Provider> = Box::new(provider);
    let k1 = object(IncusKind::Instance, "svc", None, "k1");
    let op = patch(k1, &[("user.note", Some("x"))]);
    boxed
        .plan(&entry("toy"), &op)
        .expect("before the update, the entry's object is writable");
    retained
        .retain(
            entry("toy"),
            PluginId("toy-plugin".to_string()),
            Digest::of(b"declined"),
        )
        .expect("the set is writable");
    let refused = boxed.plan(&entry("toy"), &op);
    assert!(
        matches!(
            refused,
            Err(EffectRefusal::NotOwned(OwnerClass::Retained { .. }))
        ),
        "{refused:?}"
    );
    assert!(writes(&fake).is_empty(), "nothing was written");
}

#[test]
fn a_read_back_in_a_project_without_the_feature_is_refused_and_sends_only_gets() {
    // `kbare` has no storage volumes feature, so a read there would route to the default project, as a write does.
    let (scratch, fake) = start("read-back-features");
    let provider = provider(&fake, &secrets_dir(&scratch));
    let volume = object(IncusKind::Volume, "kbare", Some("default"), "v9");
    let answer = Provider::read_back(&provider, &entry("toy"), &volume, None);
    assert!(
        matches!(&answer, Err(EffectRefusal::Provider(reason)) if reason.contains("features.storage.volumes")),
        "{answer:?}"
    );
    assert!(writes(&fake).is_empty());
    assert!(only_gets(&fake));
}

// ---------------------------------------------------------------------------------------------------------------------------
// Bridges in default: a network is addressed there, and only a bridge the entry marked is written
// ---------------------------------------------------------------------------------------------------------------------------

/// The manifest of a plugin whose networks and forwards are addressed in `default`, as the probe's is.
fn manifest_with_default() -> Manifest {
    let mut manifest = manifest();
    manifest.projects.push("default".into());
    manifest
}

/// A bridge body with an IPv4 range and no IPv6, as a kernel bridge sets it.
fn bridge_body(cidr: &str) -> Value {
    json!({"type": "bridge", "config": {"ipv4.address": cidr, "ipv6.address": "none"}})
}

fn method_path(method: &str, path: &str) -> (String, String) {
    (method.to_string(), path.to_string())
}

#[test]
fn a_marked_bridge_in_default_is_created_patched_and_removed() {
    let (scratch, fake) = start("bridge-life");
    let mut provider = provider(&fake, &secrets_dir(&scratch));
    let manifest = manifest_with_default();
    let mut journal = Journal::open(MemJournal::new()).expect("opens");
    let bridge = object(IncusKind::Network, "default", None, "br-life");
    Ctx::new(entry("toy"), &manifest, &mut provider, &mut journal, 1)
        .effect(create(
            IncusKind::Network,
            "default",
            None,
            "br-life",
            bridge_body("192.0.2.9/24"),
        ))
        .expect("the bridge is created in default");
    let stored = read(&fake, "/1.0/networks/br-life?project=default");
    assert_eq!(
        stored["config"][MARK],
        json!("toy"),
        "the create stamps the entry's mark: {stored}"
    );

    Ctx::new(entry("toy"), &manifest, &mut provider, &mut journal, 2)
        .effect(patch(bridge.clone(), &[("ipv4.nat", Some("true"))]))
        .expect("the marked bridge is patched");
    assert_eq!(
        read(&fake, "/1.0/networks/br-life?project=default")["config"]["ipv4.nat"],
        json!("true")
    );

    let plan = Digest::of(b"the approved plan");
    assert_eq!(
        provider.retire(&entry("toy"), &bridge, &plan),
        Ok(Retirement::Deleted(bridge.clone()))
    );
    let gone =
        UnixIncus::new(fake.socket().to_path_buf()).get("/1.0/networks/br-life?project=default");
    assert!(
        matches!(gone, Err(arca_incus::IncusError::Status { code: 404, .. })),
        "{gone:?}"
    );
    assert_eq!(
        writes(&fake),
        vec![
            method_path("POST", "/1.0/networks?project=default"),
            method_path("PATCH", "/1.0/networks/br-life?project=default"),
            method_path("PATCH", "/1.0/networks/br-life?project=default"),
            method_path("DELETE", "/1.0/networks/br-life?project=default"),
        ],
        "a set, a tombstone and a delete, in that order"
    );
}

#[test]
fn an_unmarked_bridge_in_default_is_refused_for_patch_and_delete_and_no_write_is_sent() {
    let (scratch, fake) = start("bridge-foreign");
    let provider = provider(&fake, &secrets_dir(&scratch));
    let plan = Digest::of(b"the approved plan");
    for (name, class) in [
        ("plainbr", OwnerClass::Foreign),
        ("droidbr", OwnerClass::Droidd),
    ] {
        let target = object(IncusKind::Network, "default", None, name);
        let op = patch(target.clone(), &[("ipv4.nat", Some("true"))]);
        assert_eq!(
            provider.plan(&entry("toy"), &op),
            Err(EffectRefusal::NotOwned(class.clone())),
            "a patch of {name}"
        );
        assert_eq!(
            provider.retire(&entry("toy"), &target, &plan),
            Err(EffectRefusal::NotOwned(class)),
            "a delete of {name}"
        );
    }
    assert!(
        writes(&fake).is_empty(),
        "no write reached an unmarked bridge: {:?}",
        writes(&fake)
    );
    assert!(
        only_gets(&fake),
        "only reads were sent: {:?}",
        fake.requests()
    );
}

#[test]
fn a_non_network_create_in_default_is_still_refused_before_any_request() {
    let (scratch, fake) = start("default-kinds");
    let provider = provider(&fake, &secrets_dir(&scratch));
    for (kind, parent, name) in [
        (IncusKind::Project, None, "default"),
        (IncusKind::Instance, None, "i9"),
        (IncusKind::Profile, None, "p9"),
        (IncusKind::Zone, None, "z9"),
        (IncusKind::Volume, Some("default"), "v9"),
        (IncusKind::Bucket, Some("default"), "b9"),
    ] {
        let op = create(kind, "default", parent, name, json!({}));
        assert_eq!(
            provider.plan(&entry("toy"), &op),
            Err(EffectRefusal::ProjectNotAllowed),
            "a {kind:?} in default"
        );
    }
    let network_in_kernel = create(
        IncusKind::Network,
        "svc",
        None,
        "br-s",
        bridge_body("192.0.2.9/24"),
    );
    assert_eq!(
        provider.plan(&entry("toy"), &network_in_kernel),
        Err(EffectRefusal::ProjectNotAllowed),
        "a network is addressed in default and nowhere else"
    );
    let forward_in_kernel = create(
        IncusKind::Forward,
        "svc",
        Some("kbr"),
        "192.0.2.10",
        json!({"config": {}}),
    );
    assert_eq!(
        provider.plan(&entry("toy"), &forward_in_kernel),
        Err(EffectRefusal::ProjectNotAllowed),
        "a forward is addressed in default and nowhere else"
    );
    assert!(fake.requests().is_empty(), "{:?}", fake.requests());
}

#[test]
fn a_project_create_may_grant_only_bridges_the_entry_holds_in_default() {
    let (scratch, fake) = start("access-list");
    let provider = provider(&fake, &secrets_dir(&scratch));
    let granting = |list: &str| {
        create(
            IncusKind::Project,
            "kfam",
            None,
            "kfam",
            json!({"config": {"restricted.networks.access": list}}),
        )
    };
    provider
        .plan(&entry("toy"), &granting("kbr"))
        .expect("the entry's own bridge is granted");
    assert_eq!(
        provider.plan(&entry("toy"), &granting("plainbr")),
        Err(EffectRefusal::NotOwned(OwnerClass::Foreign)),
        "an unmarked bridge is not granted"
    );
    assert_eq!(
        provider.plan(&entry("toy"), &granting("kbr,droidbr")),
        Err(EffectRefusal::NotOwned(OwnerClass::Droidd)),
        "one unmarked name refuses the whole list"
    );
    for list in ["", "kbr,", "bad name", "missing"] {
        assert!(
            matches!(
                provider.plan(&entry("toy"), &granting(list)),
                Err(EffectRefusal::Provider(_))
            ),
            "{list:?} is refused"
        );
    }
    assert!(
        only_gets(&fake),
        "only reads were sent: {:?}",
        fake.requests()
    );

    let mut provider = provider;
    let manifest = manifest();
    let mut journal = Journal::open(MemJournal::new()).expect("opens");
    Ctx::new(entry("toy"), &manifest, &mut provider, &mut journal, 1)
        .effect(granting("kbr"))
        .expect("the project is created with the grant");
    let config = read(&fake, "/1.0/projects/kfam")["config"].clone();
    assert_eq!(
        config["restricted.networks.access"],
        json!("kbr"),
        "{config}"
    );
    assert_eq!(config["restricted"], json!("true"), "{config}");
    assert!(
        config.get("features.networks").is_none(),
        "the kernel project does not set features.networks: {config}"
    );
}

#[test]
fn a_forward_is_made_only_on_a_network_the_entry_holds_in_default() {
    let (scratch, fake) = start("forward-parent");
    let mut provider = provider(&fake, &secrets_dir(&scratch));
    let on_foreign = create(
        IncusKind::Forward,
        "default",
        Some("plainbr"),
        "192.0.2.10",
        json!({"config": {}}),
    );
    assert_eq!(
        provider.plan(&entry("toy"), &on_foreign),
        Err(EffectRefusal::NotOwned(OwnerClass::Foreign)),
        "a forward on an unmarked bridge"
    );
    assert!(writes(&fake).is_empty());

    let on_own = create(
        IncusKind::Forward,
        "default",
        Some("kbr"),
        "192.0.2.10",
        json!({"config": {}}),
    );
    let planned = provider
        .plan(&entry("toy"), &on_own)
        .expect("a forward on the entry's own bridge plans");
    provider
        .effect(&entry("toy"), &on_own, &planned)
        .expect("the forward is made under the entry's bridge");
    assert_eq!(
        writes(&fake),
        vec![method_path(
            "POST",
            "/1.0/networks/kbr/forwards?project=default"
        )]
    );
}

#[test]
fn an_acl_is_made_marked_in_default_and_refused_in_a_kernel_project_before_any_request() {
    // Incus 7.3 keeps the ACLs of a project that does not enable networks in the default project, so an ACL is addressed there and
    // nowhere else, as a bridge is.
    let (scratch, fake) = start("acl-home");
    let mut provider = provider(&fake, &secrets_dir(&scratch));
    let in_kernel = create(IncusKind::Acl, "svc", None, "a1", json!({"config": {}}));
    assert_eq!(
        provider.plan(&entry("toy"), &in_kernel),
        Err(EffectRefusal::ProjectNotAllowed)
    );
    assert!(fake.requests().is_empty(), "{:?}", fake.requests());

    let mut manifest = manifest();
    manifest.projects.push("default".into());
    let mut journal = Journal::open(MemJournal::new()).expect("opens");
    let made = Ctx::new(entry("toy"), &manifest, &mut provider, &mut journal, 1)
        .effect(create(
            IncusKind::Acl,
            "default",
            None,
            "a1",
            json!({"config": {}}),
        ))
        .expect("an ACL in default");
    assert_eq!(made, object(IncusKind::Acl, "default", None, "a1"));
    let acl = read(&fake, "/1.0/network-acls/a1?project=default");
    assert_eq!(acl["config"][MARK], json!("toy"), "{acl}");
    assert_eq!(
        provider.plan(
            &entry("other"),
            &patch(made.clone(), &[("user.note", Some("x"))])
        ),
        Err(EffectRefusal::NotOwned(OwnerClass::Kernel {
            entry: entry("toy")
        })),
        "another entry may not change it"
    );
    assert_eq!(
        provider.retire(&entry("toy"), &made, &Digest::of(b"approved")),
        Ok(Retirement::Deleted(made.clone()))
    );
    assert!(
        UnixIncus::new(fake.socket().to_path_buf())
            .get("/1.0/network-acls/a1?project=default")
            .is_err(),
        "the ACL is gone"
    );
}

#[test]
fn an_acl_an_earlier_kernel_named_in_its_kernel_project_is_read_and_retired_in_default() {
    // An earlier kernel sent an ACL to its kernel project, and Incus made it in default, since the project does not enable
    // networks. Its journal record names the kernel project. The provider reads and retires that ACL in default.
    let (scratch, fake) = start("acl-earlier");
    let mut provider = provider(&fake, &secrets_dir(&scratch));
    let mut manifest = manifest();
    manifest.projects.push("default".into());
    let mut journal = Journal::open(MemJournal::new()).expect("opens");
    {
        let mut ctx = Ctx::new(entry("toy"), &manifest, &mut provider, &mut journal, 1);
        ctx.effect(create(
            IncusKind::Project,
            "kfam",
            None,
            "kfam",
            json!({"config": {}}),
        ))
        .expect("a kernel project, without networks");
        ctx.effect(create(
            IncusKind::Acl,
            "default",
            None,
            "a-old",
            json!({"config": {}}),
        ))
        .expect("the ACL, held in default");
    }
    let earlier = object(IncusKind::Acl, "kfam", None, "a-old");
    assert_eq!(
        provider.read_back(&entry("toy"), &earlier, None),
        Ok(ReadBack::Present {
            object: earlier.clone(),
            owned: true,
            holds_pre_image: false,
        }),
        "the record's ACL is read in default, and named as the record names it"
    );
    let plan = Digest::of(b"the create's own digest");
    assert_eq!(
        provider.retire(&entry("toy"), &earlier, &plan),
        Ok(Retirement::Deleted(object(
            IncusKind::Acl,
            "default",
            None,
            "a-old"
        )))
    );
    assert!(
        UnixIncus::new(fake.socket().to_path_buf())
            .get("/1.0/network-acls/a-old?project=default")
            .is_err(),
        "the ACL is gone from default"
    );

    // A project that enables networks keeps its own ACLs, so a record naming one keeps its address, and the family refuses it.
    let before = writes(&fake).len();
    assert_eq!(
        provider.retire(
            &entry("toy"),
            &object(IncusKind::Acl, "svc", None, "a-old"),
            &plan
        ),
        Err(EffectRefusal::ProjectNotAllowed)
    );
    assert_eq!(writes(&fake).len(), before, "nothing was written");
}

// ---------------------------------------------------------------------------------------------------------------------------
// A project's config: every write is a PUT of the whole config, since Incus 7.3's project PATCH replaces it
// ---------------------------------------------------------------------------------------------------------------------------

/// The PATCHes the fake received at a project's own path. A kernel path that sent one would have Incus 7.3 replace the project's
/// whole config with the keys it named.
fn project_patches(fake: &FakeIncus) -> Vec<(String, String)> {
    writes(fake)
        .into_iter()
        .filter(|(method, path)| method == "PATCH" && path.starts_with("/1.0/projects/"))
        .collect()
}

/// Makes the kernel project `kfam` for `toy` through the kernel, then puts an image in its own store, as a probe's instance or
/// a copy by hand would, so the project is in use. Returns the project's address.
fn kernel_project_in_use<S: JournalStore>(
    provider: &mut IncusProvider,
    fake: &FakeIncus,
    journal: &mut Journal<S>,
) -> ObjectRef {
    let manifest = manifest();
    Ctx::new(entry("toy"), &manifest, provider, journal, 1)
        .effect(create(IncusKind::Project, "kfam", None, "kfam", json!({})))
        .expect("the project is created");
    fake.add_image("kfam", "alpine/edge");
    object(IncusKind::Project, "kfam", None, "kfam")
}

#[test]
fn a_project_patch_and_its_restore_are_puts_that_keep_every_other_key() {
    let (scratch, fake) = start("project-patch-put");
    let secrets = secrets_dir(&scratch);
    let mut provider = provider(&fake, &secrets);
    let manifest = manifest();
    let mut journal = Journal::open(MemJournal::new()).expect("opens");
    let project = kernel_project_in_use(&mut provider, &fake, &mut journal);
    let made = read(&fake, "/1.0/projects/kfam")["config"].clone();

    Ctx::new(entry("toy"), &manifest, &mut provider, &mut journal, 2)
        .effect(patch(project.clone(), &[("user.note", Some("plain"))]))
        .expect("the project patch lands");
    let mut noted = made.clone();
    noted["user.note"] = json!("plain");
    assert_eq!(read(&fake, "/1.0/projects/kfam")["config"], noted);

    let restore = journalled_restore(&journal, "toy");
    Ctx::new(entry("toy"), &manifest, &mut provider, &mut journal, 3)
        .effect(EffectOp::IncusPatchConfig {
            object: project,
            config: restore,
        })
        .expect("the restore lands");
    assert_eq!(
        read(&fake, "/1.0/projects/kfam")["config"],
        made,
        "the restore leaves the mark, restricted and every feature as they were made"
    );
    assert_eq!(project_patches(&fake), Vec::<(String, String)>::new());
    let puts: Vec<(String, String)> = writes(&fake)
        .into_iter()
        .filter(|(method, _)| method == "PUT")
        .collect();
    assert_eq!(
        puts,
        vec![
            method_path("PUT", "/1.0/projects/kfam"),
            method_path("PUT", "/1.0/projects/kfam"),
        ]
    );
}

#[test]
fn retire_of_a_project_in_use_tombstones_it_with_a_put_then_deletes_its_image_and_itself() {
    let (scratch, fake) = start("project-retire-put");
    let secrets = secrets_dir(&scratch);
    let mut provider = provider(&fake, &secrets);
    let mut journal = Journal::open(MemJournal::new()).expect("opens");
    let project = kernel_project_in_use(&mut provider, &fake, &mut journal);
    let plan = Digest::of(b"the approved plan");

    assert_eq!(
        provider.retire(&entry("toy"), &project, &plan),
        Ok(Retirement::Deleted(project.clone()))
    );
    assert!(fake.images().is_empty(), "{:?}", fake.images());
    let methods: Vec<String> = writes(&fake)
        .into_iter()
        .map(|(method, path)| format!("{method} {}", path.split('?').next().unwrap_or("")))
        .map(|line| {
            if line.starts_with("DELETE /1.0/images/") {
                "DELETE image".to_string()
            } else {
                line
            }
        })
        .collect();
    assert_eq!(
        methods,
        vec![
            "POST /1.0/projects".to_string(),
            "PUT /1.0/projects/kfam".to_string(),
            "DELETE image".to_string(),
            "DELETE /1.0/projects/kfam".to_string(),
        ]
    );
    assert_eq!(project_patches(&fake), Vec::<(String, String)>::new());
}

#[test]
fn a_tombstone_refused_with_a_4xx_that_reads_back_without_it_is_drift_and_no_delete_is_sent() {
    let plan = Digest::of(b"the approved plan");
    let script = Script::start("tombstone-400", move |method, _| match method {
        "GET" => object_reply(&r2(READ_UUID, None), "\"e1\""),
        "PATCH" => http(400, None, &error_env(400)),
        _ => not_found(),
    });
    let secrets = Scratch::new("secrets-tombstone-400");
    let provider = script.provider(&secrets.0);
    let target = ObjectRef {
        uuid: Some(READ_UUID.to_string()),
        ..object(IncusKind::Instance, "svc", None, "r2")
    };
    let outcome = provider
        .retire(&entry("toy"), &target, &plan)
        .expect("a refused tombstone is settled, not open");
    assert!(matches!(outcome, Retirement::Drift(_)), "{outcome:?}");
    assert_eq!(script.count("DELETE"), 0, "no DELETE was sent");
    assert_eq!(
        script.count("GET"),
        2,
        "the object was read, then read back after the refusal: {:?}",
        script.seen()
    );
}

#[test]
fn a_project_tombstone_incus_refuses_with_400_is_drift_and_not_an_open_outcome() {
    // On Incus 7.3 a project's tombstone was refused with 400, nothing landed, and the delete's intent was left open, so no
    // later run could open the journal. A refusal whose read-back shows no tombstone is a delete that did not happen.
    let plan = Digest::of(b"the approved plan");
    let script = Script::start("project-tombstone-400", move |method, target| {
        match (method, target) {
            ("GET", "/1.0/projects/kfam") => object_reply(
                &json!({"name": "kfam", "description": "", "config": {
                    MARK: "toy", "restricted": "true", "features.images": "true"}}),
                "\"p1\"",
            ),
            ("GET", _) => not_found(),
            _ => http(400, None, &error_env(400)),
        }
    });
    let secrets = Scratch::new("secrets-project-400");
    let provider = script.provider(&secrets.0);
    let outcome = provider
        .retire(
            &entry("toy"),
            &object(IncusKind::Project, "kfam", None, "kfam"),
            &plan,
        )
        .expect("a refused tombstone is settled, not open");
    assert!(matches!(outcome, Retirement::Drift(_)), "{outcome:?}");
    assert_eq!(script.count("DELETE"), 0, "no DELETE was sent");
    assert_eq!(script.count("PATCH"), 0, "a project takes no PATCH");
}

#[test]
fn a_tombstone_that_reads_back_after_a_4xx_or_is_answered_5xx_stays_unresolved() {
    let plan = Digest::of(b"the approved plan");
    let plan_hex = plan.to_hex();
    let target = ObjectRef {
        uuid: Some(READ_UUID.to_string()),
        ..object(IncusKind::Instance, "svc", None, "r2")
    };
    // A 4xx whose read-back shows the tombstone: the write landed after all, so the outcome cannot be vouched for.
    let mut step = 0;
    let landed = Script::start("tombstone-400-landed", move |method, _| {
        step += 1;
        match (method, step) {
            ("GET", 1) => object_reply(&r2(READ_UUID, None), "\"e1\""),
            ("PATCH", 2) => http(400, None, &error_env(400)),
            ("GET", _) => object_reply(&r2(READ_UUID, Some(&plan_hex)), "\"e2\""),
            _ => not_found(),
        }
    });
    let secrets = Scratch::new("secrets-tombstone-landed");
    assert_eq!(
        landed
            .provider(&secrets.0)
            .retire(&entry("toy"), &target, &plan),
        Err(EffectRefusal::Unresolved)
    );
    assert_eq!(landed.count("DELETE"), 0);
    // A 5xx says nothing about whether the write landed, so it stays open.
    let failed = Script::start("tombstone-500", move |method, _| match method {
        "GET" => object_reply(&r2(READ_UUID, None), "\"e1\""),
        "PATCH" => http(500, None, &error_env(500)),
        _ => not_found(),
    });
    assert_eq!(
        failed
            .provider(&secrets.0)
            .retire(&entry("toy"), &target, &plan),
        Err(EffectRefusal::Unresolved)
    );
    assert_eq!(failed.count("DELETE"), 0);
}
