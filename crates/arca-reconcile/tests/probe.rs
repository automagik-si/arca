//! The probe harness on the fake Incus. It creates one object of each kind in a scratch project through `Ctx::effect` over a real
//! `FileJournal`, then removes them through the journalled retirement. Each kill case runs the creates in a child process that
//! aborts at one effect boundary. The parent then restarts the reconciler on the same journal, and checks that the journal and the
//! fake agree once the probe completes.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use arca_approval::{ApprovalVerifier, FileNonces, SshSigVerifier};
use arca_kernel::canon::Digest;
use arca_kernel::desired::{DesiredStateGate, EntryId};
use arca_kernel::journal::Inverse;
use arca_kernel::journal::{FileJournal, Journal};
use arca_kernel::ownership::{IncusKind, ObjectRef};
use arca_kernel::plugin::{
    EffectOp, EffectRefusal, Observe, Planned, Provider, ReadBack, Retirement,
};
use arca_providers::incus::{Family, IMAGE_PROTOCOL, IMAGE_SERVER, IncusProvider, RetainedSet};
use arca_reconcile::probe::{
    DEFAULT_PROBE_IMAGE, PROBE_BRIDGE, PROBE_PLUGIN, ProbeOutcome, ProbeSpec, kind_name,
};
use arca_reconcile::{PluginSet, Reconciler};
use common::*;
use serde_json::{Value, json};

/// The scratch project the probe makes. Its name carries the scratch prefix the family requires.
const PROJECT: &str = "arcak-scratch-probe";
/// The probe's entry.
const ENTRY: &str = "probe-scratch";
/// How many creates one probe run makes: the project and one object of each of the eight kinds.
const CREATES: usize = 9;

/// Where a child aborts relative to the effect it is told to stop at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// After the intent is journalled, before the write is sent.
    Before,
    /// After the write is answered, before its outcome is journalled.
    After,
}

/// A provider that aborts the process at the `at`th effect, before or after it runs. It delegates everything else.
struct Killing {
    inner: IncusProvider,
    at: usize,
    phase: Phase,
    seen: usize,
}

impl Provider for Killing {
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
        let index = self.seen;
        self.seen += 1;
        if index == self.at && self.phase == Phase::Before {
            std::process::abort();
        }
        let answered = self.inner.effect(entry, op, planned);
        if index == self.at && self.phase == Phase::After {
            std::process::abort();
        }
        answered
    }

    fn read_back(
        &self,
        entry: &EntryId,
        target: &ObjectRef,
        pre_image: Option<&std::collections::BTreeMap<String, Option<String>>>,
    ) -> Result<ReadBack, EffectRefusal> {
        self.inner.read_back(entry, target, pre_image)
    }

    fn retire(
        &mut self,
        entry: &EntryId,
        object: &ObjectRef,
        plan: &Digest,
    ) -> Result<Retirement, EffectRefusal> {
        Provider::retire(&mut self.inner, entry, object, plan)
    }
}

fn spec() -> ProbeSpec {
    ProbeSpec {
        entry: EntryId(ENTRY.to_string()),
        project: PROJECT.to_string(),
        cidr: "203.0.113.1/24".to_string(),
        listen: "203.0.113.10".to_string(),
        pool: "default".to_string(),
        image: DEFAULT_PROBE_IMAGE.to_string(),
    }
}

/// The body of the one create of `kind` the spec makes.
fn body_of(spec: &ProbeSpec, kind: IncusKind) -> Value {
    spec.ops()
        .into_iter()
        .find_map(|op| match op {
            EffectOp::IncusCreate {
                kind: made, body, ..
            } if made == kind => Some(body),
            _ => None,
        })
        .expect("the spec makes one create of this kind")
}

/// The family the probe writes in: its scratch project, and the droidd project it must never touch.
fn probe_family() -> Family {
    Family {
        projects: vec![PROJECT.to_string()],
        scratch: true,
        droidd_projects: vec![DROIDD_PROJECT.to_string()],
        reserved_cidrs: vec!["198.51.100.0/24".to_string()],
    }
}

/// The reconciler the probe runs under, on `journal`. With `kill` set, its provider aborts at that effect.
fn reconciler_at(
    dir: &Path,
    socket: &Path,
    journal: &Path,
    kill: Option<(usize, Phase)>,
) -> Reconciler<FileJournal> {
    let retained = RetainedSet::empty();
    let inner = IncusProvider::new(
        socket.to_path_buf(),
        probe_family(),
        dir.join("secrets"),
        retained.clone(),
    )
    .expect("the probe family is valid");
    let provider: Box<dyn Provider + Send> = match kill {
        Some((at, phase)) => Box::new(Killing {
            inner,
            at,
            phase,
            seen: 0,
        }),
        None => Box::new(inner),
    };
    let store = FileJournal::open(journal).expect("the journal opens");
    Reconciler::open(
        DesiredStateGate::new(node_key().public(), None),
        Journal::open(store).expect("the journal folds"),
        PluginSet::new(),
        provider,
        retained,
        NOW,
    )
    .expect("the journal has no unresolved intent it cannot settle")
}

/// The child a kill case runs. It does nothing unless the parent sets its environment.
#[test]
fn probe_child() {
    let Some(dir) = std::env::var_os("ARCA_PROBE_DIR") else {
        return;
    };
    let text = |name: &str| std::env::var(name).expect("the parent sets the child's environment");
    let socket = PathBuf::from(text("ARCA_PROBE_SOCKET"));
    let journal = PathBuf::from(text("ARCA_PROBE_JOURNAL"));
    let crash = text("ARCA_PROBE_CRASH");
    let (at, phase) = crash
        .split_once(':')
        .expect("the crash point is index:phase");
    let phase = match phase {
        "before" => Phase::Before,
        _ => Phase::After,
    };
    let at: usize = at.parse().expect("the crash index is a number");
    let mut reconciler = reconciler_at(Path::new(&dir), &socket, &journal, Some((at, phase)));
    reconciler
        .probe_create(&spec(), NOW)
        .expect("the probe creates every object");
}

/// The GET path of an object, as the fake serves it.
fn path_of(object: &ObjectRef) -> String {
    let (name, project) = (&object.name, &object.project);
    let parent = object.parent.clone().unwrap_or_default();
    match object.kind {
        IncusKind::Project => format!("/1.0/projects/{name}"),
        IncusKind::Instance => format!("/1.0/instances/{name}?project={project}"),
        IncusKind::Network => format!("/1.0/networks/{name}?project={project}"),
        IncusKind::Acl => format!("/1.0/network-acls/{name}?project={project}"),
        IncusKind::Profile => format!("/1.0/profiles/{name}?project={project}"),
        IncusKind::Zone => format!("/1.0/network-zones/{name}?project={project}"),
        IncusKind::Forward => format!("/1.0/networks/{parent}/forwards/{name}?project={project}"),
        IncusKind::Volume => {
            format!("/1.0/storage-pools/{parent}/volumes/custom/{name}?project={project}")
        }
        IncusKind::Bucket => {
            format!("/1.0/storage-pools/{parent}/buckets/{name}?project={project}")
        }
    }
}

/// Whether the fake holds `object`.
fn present(fake: &arca_incus::FakeIncus, object: &ObjectRef) -> bool {
    get(fake, &path_of(object)).is_some()
}

/// Kernel creates start by default; deletion fixtures must stop their instance.
fn stop_probe(fake: &arca_incus::FakeIncus) {
    let path = format!("/1.0/instances/probe-box/state?project={PROJECT}");
    assert_eq!(get(fake, &path).unwrap()["status"], "Running");
    let (status, _) = raw(fake, "PUT", &path, Some(&json!({"action":"stop"})));
    assert_eq!(status, 202);
    assert_eq!(get(fake, &path).unwrap()["status"], "Stopped");
}

/// The place an object names, without its uuid.
fn place(object: &ObjectRef) -> (IncusKind, String, Option<String>, String) {
    (
        object.kind,
        object.project.clone(),
        object.parent.clone(),
        object.name.clone(),
    )
}

/// The live journal records of the probe's entry, as the journal holds them now.
fn live_places(
    reconciler: &Reconciler<FileJournal>,
) -> Vec<(IncusKind, String, Option<String>, String)> {
    reconciler
        .journal()
        .live(&EntryId(ENTRY.to_string()))
        .iter()
        .map(|record| place(&record.target))
        .collect()
}

/// Asserts the journal and the fake agree: every planned object is held by the fake exactly when the journal holds it live,
/// and every live record names an object the fake holds.
fn assert_agree(
    fake: &arca_incus::FakeIncus,
    reconciler: &Reconciler<FileJournal>,
    targets: &[ObjectRef],
) {
    let live = reconciler.journal().live(&EntryId(ENTRY.to_string()));
    for target in targets {
        let held = present(fake, target);
        let journalled = live
            .iter()
            .any(|record| place(&record.target) == place(target));
        assert_eq!(
            held, journalled,
            "the journal and the fake disagree on {:?} {}",
            target.kind, target.name
        );
    }
    for record in &live {
        assert_eq!(
            record.plugin.as_ref().map(|plugin| plugin.0.as_str()),
            Some(PROBE_PLUGIN),
            "a probe record names the probe plugin"
        );
        assert!(
            present(fake, &record.target),
            "a live record names an object the fake does not hold: {:?} {}",
            record.target.kind,
            record.target.name
        );
    }
}

/// Runs one kill case: the child aborts at effect `index` in `phase`, the parent restarts on the same journal, the probe completes,
/// and the probe is removed. A `None` case runs the probe to the end with no kill.
fn kill_then_restart(case: Option<(usize, Phase)>) {
    let scratch = Scratch::new("probe");
    let fake = start_fake(&scratch);
    let journal = scratch.path("journal.jsonl");
    let spec = spec();
    let targets = spec.targets();
    assert_eq!(targets.len(), CREATES);

    let mut command = Command::new(std::env::current_exe().expect("the test binary is known"));
    command
        .args(["probe_child", "--exact", "--nocapture"])
        .env("ARCA_PROBE_DIR", &scratch.dir)
        .env("ARCA_PROBE_SOCKET", fake.socket())
        .env("ARCA_PROBE_JOURNAL", &journal)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some((index, phase)) = case {
        let phase = if phase == Phase::Before {
            "before"
        } else {
            "after"
        };
        command.env("ARCA_PROBE_CRASH", format!("{index}:{phase}"));
    } else {
        command.env("ARCA_PROBE_CRASH", format!("{CREATES}:before"));
    }
    let status = command.status().expect("the child runs");

    match case {
        Some((index, phase)) => {
            assert!(!status.success(), "the child was killed at effect {index}");
            // The kill landed at the named boundary: the object at `index` exists exactly when the kill came after its write.
            for (position, target) in targets.iter().enumerate() {
                let expected = position < index || (position == index && phase == Phase::After);
                assert_eq!(
                    present(&fake, target),
                    expected,
                    "effect {position} after a kill at {index} {phase:?}"
                );
            }
            // Every object the killed process made has a journal record, even when its outcome is unknown.
            let reopened = Journal::open(FileJournal::open(&journal).expect("the journal opens"))
                .expect("the journal folds");
            let live = live_places_of(&reopened);
            for target in &targets {
                if present(&fake, target) {
                    assert!(
                        live.contains(&place(target)),
                        "the journal holds no record for a made {:?} {}",
                        target.kind,
                        target.name
                    );
                }
            }
        }
        None => assert!(status.success(), "the uninterrupted child succeeds"),
    }

    // Restart on the same journal. The probe completes, and the journal and the fake agree.
    let mut reconciler = reconciler_at(&scratch.dir, fake.socket(), &journal, None);
    reconciler
        .probe_create(&spec, NOW)
        .expect("the probe completes after the restart");
    assert_agree(&fake, &reconciler, &targets);
    assert_eq!(live_places(&reconciler).len(), CREATES);
    assert_eq!(
        fake.images()
            .iter()
            .map(|(project, _)| project.as_str())
            .collect::<Vec<_>>(),
        vec![PROJECT],
        "the instance's image was pulled into the probe project's own store"
    );

    stop_probe(&fake);
    // Removal goes through the journalled retirement: class-A objects revert at once and class-B objects run under an approval.
    let key = SkKey::from_seed(7);
    let mut verifier: ApprovalVerifier<SshSigVerifier, FileNonces> =
        verifier_for(&scratch, &key, "probe");
    let run = reconciler
        .probe_remove(
            &spec,
            NOW,
            |plan: &Digest| approval_for_plan(&key, plan, 1),
            &mut verifier,
        )
        .expect("the probe is removed under its approval");
    assert!(!run.partial, "every class-B object is deleted");
    assert_eq!(
        run.deleted.len(),
        5,
        "the instance, the volume, the bucket, the project and the bridge"
    );
    for target in &targets {
        assert!(
            !present(&fake, target),
            "{:?} {} is gone",
            target.kind,
            target.name
        );
    }
    assert!(
        fake.images().is_empty(),
        "the project's image went with it: {:?}",
        fake.images()
    );
    assert!(
        reconciler
            .journal()
            .live(&EntryId(ENTRY.to_string()))
            .is_empty()
    );
    assert_eq!(
        project_patches(&fake),
        Vec::new(),
        "no project PATCH was sent"
    );
}

/// The places of the live records of the probe entry in a reopened journal.
fn live_places_of(
    journal: &Journal<FileJournal>,
) -> Vec<(IncusKind, String, Option<String>, String)> {
    journal
        .live(&EntryId(ENTRY.to_string()))
        .iter()
        .map(|record| place(&record.target))
        .collect()
}

#[test]
fn an_uninterrupted_probe_creates_and_removes_every_object() {
    kill_then_restart(None);
}

#[test]
fn a_kill_before_each_effect_restarts_to_agreement() {
    for index in 0..CREATES {
        kill_then_restart(Some((index, Phase::Before)));
    }
}

#[test]
fn a_kill_after_each_effect_restarts_to_agreement() {
    for index in 0..CREATES {
        kill_then_restart(Some((index, Phase::After)));
    }
}

#[test]
fn the_probe_instance_has_an_image_source_a_root_disk_and_a_nic_on_the_probe_bridge() {
    let spec = spec();
    let instance = body_of(&spec, IncusKind::Instance);
    assert_eq!(
        instance["source"],
        json!({"type": "image", "alias": DEFAULT_PROBE_IMAGE, "server": IMAGE_SERVER, "protocol": IMAGE_PROTOCOL}),
        "the image is pulled from the image server into the project's own store, with no copy by hand"
    );
    assert_eq!(
        instance["devices"]["root"],
        json!({"type": "disk", "path": "/", "pool": "default"})
    );
    assert_eq!(
        instance["devices"]["eth0"],
        json!({"type": "nic", "network": PROBE_BRIDGE})
    );

    let other = ProbeSpec {
        image: "busybox/1.36".to_string(),
        ..spec.clone()
    };
    assert_eq!(
        body_of(&other, IncusKind::Instance)["source"]["alias"],
        json!("busybox/1.36"),
        "the image alias is configurable"
    );

    let targets = spec.targets();
    let bridge = targets
        .iter()
        .find(|target| target.kind == IncusKind::Network)
        .expect("the spec makes a network");
    assert_eq!(
        (bridge.project.as_str(), bridge.name.as_str()),
        ("default", PROBE_BRIDGE),
        "the bridge is addressed in default"
    );
    let forward = targets
        .iter()
        .find(|target| target.kind == IncusKind::Forward)
        .expect("the spec makes a forward");
    assert_eq!(
        (forward.project.as_str(), forward.parent.as_deref()),
        ("default", Some(PROBE_BRIDGE)),
        "the forward hangs off the bridge in default"
    );
    assert_eq!(
        body_of(&spec, IncusKind::Project)["config"]["restricted.networks.access"],
        json!(PROBE_BRIDGE),
        "the project grants its instances the bridge"
    );
    let acl = targets
        .iter()
        .find(|target| target.kind == IncusKind::Acl)
        .expect("the spec makes an ACL");
    assert_eq!(
        acl.project, "default",
        "the ACL is addressed in default, where Incus keeps it"
    );
}

/// The outcome the probe recorded for `kind`.
fn outcome_of(rows: &[arca_reconcile::probe::ProbeRow], kind: IncusKind) -> ProbeOutcome {
    rows.iter()
        .find(|row| row.kind == kind)
        .map(|row| row.outcome.clone())
        .expect("the probe records a row for every kind")
}

#[test]
fn a_refused_create_does_not_stop_the_probe_and_each_kind_is_recorded() {
    let scratch = Scratch::new("probe-refused");
    let fake = start_fake(&scratch);
    // Incus refuses an ACL that carries a config. Every other kind is still made.
    fake.set_reject_config_on_post("/1.0/network-acls");
    let journal = scratch.path("journal.jsonl");
    let mut reconciler = reconciler_at(&scratch.dir, fake.socket(), &journal, None);
    let rows = reconciler
        .probe_create(&spec(), NOW)
        .expect("the probe runs to its end");
    assert_eq!(rows.len(), CREATES, "one row per kind");
    assert!(
        matches!(outcome_of(&rows, IncusKind::Acl), ProbeOutcome::Refused(_)),
        "{:?}",
        outcome_of(&rows, IncusKind::Acl)
    );
    for kind in [
        IncusKind::Project,
        IncusKind::Instance,
        IncusKind::Network,
        IncusKind::Profile,
        IncusKind::Volume,
        IncusKind::Zone,
        IncusKind::Forward,
        IncusKind::Bucket,
    ] {
        assert_eq!(
            outcome_of(&rows, kind),
            ProbeOutcome::Created,
            "{kind:?} is made after the refusal"
        );
    }
    for target in spec().targets() {
        if target.kind != IncusKind::Acl {
            assert!(
                present(&fake, &target),
                "{:?} {} exists",
                target.kind,
                target.name
            );
        }
    }
}

#[test]
fn a_kind_on_a_failed_dependency_is_not_attempted_and_names_what_it_needs() {
    let scratch = Scratch::new("probe-needs");
    let fake = start_fake(&scratch);
    // Incus refuses the bridge's create. The project grants the bridge, so it needs the bridge; every other kind needs
    // the project, or the bridge for a forward.
    fake.set_reject_config_on_post("/1.0/networks");
    let journal = scratch.path("journal.jsonl");
    let mut reconciler = reconciler_at(&scratch.dir, fake.socket(), &journal, None);
    let rows = reconciler
        .probe_create(&spec(), NOW)
        .expect("the probe runs to its end");
    assert!(matches!(
        outcome_of(&rows, IncusKind::Network),
        ProbeOutcome::Refused(_)
    ));
    assert_eq!(
        outcome_of(&rows, IncusKind::Project),
        ProbeOutcome::NotAttempted {
            needs: IncusKind::Network
        }
    );
    assert_eq!(
        outcome_of(&rows, IncusKind::Instance),
        ProbeOutcome::NotAttempted {
            needs: IncusKind::Project
        }
    );
    assert_eq!(
        outcome_of(&rows, IncusKind::Forward),
        ProbeOutcome::NotAttempted {
            needs: IncusKind::Network
        }
    );
    assert_eq!(
        outcome_of(&rows, IncusKind::Bucket).to_string(),
        "not-attempted: needs project"
    );
    for target in spec().targets() {
        assert!(
            !present(&fake, &target),
            "{:?} {} was not made",
            target.kind,
            target.name
        );
    }
}

/// A journal an earlier kernel wrote during a probe, before a custom volume was known to have no uuid. Its names are the test
/// spec's. Its creates are the bridge and the project (applied), the instance (failed: its image was not in the project's
/// store), the ACL (sent to the kernel project, which Incus served from `default`) and the profile (applied), and the volume,
/// whose intent has no outcome: the volume landed and carries the mark, but the earlier kernel refused an answer without a uuid.
const EARLIER_JOURNAL: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/journal-volume-without-uuid.jsonl"
);

/// Copies the earlier journal to `path`, with the mode a journal file has.
fn earlier_journal_at(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::copy(EARLIER_JOURNAL, path).expect("the fixture is copied");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("mode 0600");
}

/// Makes on the fake what the earlier probe left on the host: each object it made, carrying the probe's mark. The ACL is sent
/// to the kernel project, as the earlier kernel sent it.
fn seed_earlier_objects(fake: &arca_incus::FakeIncus) {
    let mark = json!(ENTRY);
    let made = [
        (
            "/1.0/networks?project=default".to_string(),
            json!({"name": PROBE_BRIDGE, "type": "bridge", "config": {
                "ipv4.address": "203.0.113.1/24", "ipv6.address": "none", MARK: mark}}),
        ),
        (
            "/1.0/projects".to_string(),
            json!({"name": PROJECT, "config": {
                MARK: mark, "restricted": "true", "restricted.networks.access": PROBE_BRIDGE,
                "features.images": "true", "features.networks.zones": "true", "features.profiles": "true",
                "features.storage.volumes": "true", "features.storage.buckets": "true"}}),
        ),
        (
            format!("/1.0/network-acls?project={PROJECT}"),
            json!({"name": "probe-acl", "config": {MARK: mark}}),
        ),
        (
            format!("/1.0/profiles?project={PROJECT}"),
            json!({"name": "probe-profile", "config": {MARK: mark}}),
        ),
        (
            format!("/1.0/storage-pools/default/volumes/custom?project={PROJECT}"),
            json!({"name": "probe-volume", "config": {MARK: mark}}),
        ),
    ];
    for (path, body) in made {
        let (status, _) = raw(fake, "POST", &path, Some(&body));
        assert!((200..300).contains(&status), "{path}: {status}");
    }
}

#[test]
fn the_journal_an_earlier_probe_left_opens_with_its_volume_adopted_by_name() {
    let scratch = Scratch::new("probe-earlier-open");
    let fake = start_fake(&scratch);
    seed_earlier_objects(&fake);
    let journal = scratch.path("journal.jsonl");
    earlier_journal_at(&journal);

    let reconciler = reconciler_at(&scratch.dir, fake.socket(), &journal, None);
    let volume = ObjectRef {
        kind: IncusKind::Volume,
        project: PROJECT.to_string(),
        parent: Some("default".to_string()),
        name: "probe-volume".to_string(),
        uuid: None,
    };
    assert_eq!(
        reconciler.resolution().adopted,
        vec![volume.clone()],
        "the volume is adopted by its pool and name"
    );
    let record = reconciler
        .journal()
        .live(&EntryId(ENTRY.to_string()))
        .into_iter()
        .find(|record| record.target.kind == IncusKind::Volume)
        .expect("the volume's record is live");
    assert_eq!(
        record.inverse,
        Some(Inverse::IncusDelete { object: volume })
    );
}

#[test]
fn the_journal_an_earlier_probe_left_is_removed_to_nothing_and_a_fresh_probe_then_runs() {
    let scratch = Scratch::new("probe-earlier-remove");
    let fake = start_fake(&scratch);
    seed_earlier_objects(&fake);
    // The image copied by hand into the project's own store, after the instance's create was refused.
    fake.add_image(PROJECT, DEFAULT_PROBE_IMAGE);
    assert!(
        get(&fake, "/1.0/network-acls/probe-acl?project=default").is_some(),
        "the ACL sent to the kernel project is held in default"
    );
    let journal = scratch.path("journal.jsonl");
    earlier_journal_at(&journal);
    let mut reconciler = reconciler_at(&scratch.dir, fake.socket(), &journal, None);
    let spec = spec();
    let key = SkKey::from_seed(7);
    let mut verifier: ApprovalVerifier<SshSigVerifier, FileNonces> =
        verifier_for(&scratch, &key, "probe");

    let run = reconciler
        .probe_remove(
            &spec,
            NOW,
            |plan: &Digest| approval_for_plan(&key, plan, 1),
            &mut verifier,
        )
        .expect("the earlier probe is removed under its approval");
    assert!(!run.partial, "every class-B object is deleted: {run:?}");
    assert_eq!(
        run.deleted.len(),
        3,
        "the volume, the project and the bridge"
    );
    for path in [
        "/1.0/networks/probe-net?project=default".to_string(),
        "/1.0/network-acls/probe-acl?project=default".to_string(),
        format!("/1.0/projects/{PROJECT}"),
        format!("/1.0/profiles/probe-profile?project={PROJECT}"),
        format!("/1.0/storage-pools/default/volumes/custom/probe-volume?project={PROJECT}"),
    ] {
        assert!(get(&fake, &path).is_none(), "{path} is gone");
    }
    assert!(
        fake.images().is_empty(),
        "the image inside the project went with it: {:?}",
        fake.images()
    );
    assert!(
        reconciler
            .journal()
            .live(&EntryId(ENTRY.to_string()))
            .is_empty(),
        "the journal holds nothing live"
    );

    // A fresh probe of every kind then runs on the same journal, and its removal leaves nothing.
    let rows = reconciler
        .probe_create(&spec, NOW)
        .expect("the fresh probe runs");
    assert!(
        rows.iter().all(|row| row.outcome == ProbeOutcome::Created),
        "{rows:?}"
    );
    stop_probe(&fake);
    let run = reconciler
        .probe_remove(
            &spec,
            NOW,
            |plan: &Digest| approval_for_plan(&key, plan, 2),
            &mut verifier,
        )
        .expect("the fresh probe is removed under its approval");
    assert!(!run.partial, "{run:?}");
    for target in spec.targets() {
        assert!(
            !present(&fake, &target),
            "{:?} {} is gone",
            target.kind,
            target.name
        );
    }
    assert!(fake.images().is_empty(), "{:?}", fake.images());
    assert!(
        reconciler
            .journal()
            .live(&EntryId(ENTRY.to_string()))
            .is_empty()
    );
    assert_eq!(
        project_patches(&fake),
        Vec::new(),
        "no project PATCH was sent"
    );
}

/// A journal the kernel wrote while it removed an earlier probe on Incus 7.3, whose project PATCH replaces the project's whole
/// config. Its names are the test spec's. The removal reverted the profile and the ACL and deleted the volume; then the project's
/// tombstone PATCH named only the tombstone, so it read as turning off every feature of a project that still held an image, and
/// Incus refused it with 400. Nothing landed, but the kernel left the project's delete (intent 10) with no outcome. The bridge was
/// never reached. 22 records.
const PROJECT_TOMBSTONE_JOURNAL: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/journal-project-tombstone-open.jsonl"
);

/// The id of the project's delete intent in [`PROJECT_TOMBSTONE_JOURNAL`].
const OPEN_PROJECT_DELETE: u64 = 10;

/// Makes on the fake what that removal left: the bridge and the project, each carrying the probe's mark, and the image copied by
/// hand into the project's own store, which keeps the project in use.
fn seed_tombstone_leftovers(fake: &arca_incus::FakeIncus) {
    let mark = json!(ENTRY);
    let made = [
        (
            "/1.0/networks?project=default".to_string(),
            json!({"name": PROBE_BRIDGE, "type": "bridge", "config": {
                "ipv4.address": "203.0.113.1/24", "ipv6.address": "none", MARK: mark}}),
        ),
        (
            "/1.0/projects".to_string(),
            json!({"name": PROJECT, "config": {
                MARK: mark, "restricted": "true", "restricted.networks.access": PROBE_BRIDGE,
                "features.images": "true", "features.networks.zones": "true", "features.profiles": "true",
                "features.storage.volumes": "true", "features.storage.buckets": "true"}}),
        ),
    ];
    for (path, body) in made {
        let (status, _) = raw(fake, "POST", &path, Some(&body));
        assert!((200..300).contains(&status), "{path}: {status}");
    }
    fake.add_image(PROJECT, DEFAULT_PROBE_IMAGE);
}

/// The PATCHes the fake received at a project's own path. Incus 7.3 would replace the project's config with the keys one names.
fn project_patches(fake: &arca_incus::FakeIncus) -> Vec<arca_incus::WriteRecord> {
    fake.writes()
        .into_iter()
        .filter(|write| write.method == "PATCH" && write.path.starts_with("/1.0/projects/"))
        .collect()
}

/// Each record of a journal file as `(id, state)`, in the order the file holds them.
fn journal_states(path: &Path) -> Vec<(u64, String)> {
    std::fs::read_to_string(path)
        .expect("the journal reads")
        .lines()
        .map(|line| {
            let record: Value = serde_json::from_str(line).expect("a record is JSON");
            (
                record["id"].as_u64().expect("a record has an id"),
                record["state"]
                    .as_str()
                    .expect("a record has a state")
                    .to_string(),
            )
        })
        .collect()
}

#[test]
fn the_journal_a_refused_project_tombstone_left_opens_and_its_removal_deletes_the_project_its_image_and_the_bridge()
 {
    let scratch = Scratch::new("probe-tombstone-open");
    let fake = start_fake(&scratch);
    seed_tombstone_leftovers(&fake);
    let journal = scratch.path("journal.jsonl");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::copy(PROJECT_TOMBSTONE_JOURNAL, &journal).expect("the fixture is copied");
        std::fs::set_permissions(&journal, std::fs::Permissions::from_mode(0o600))
            .expect("mode 0600");
    }
    let written = journal_states(&journal);
    assert_eq!(
        written.len(),
        22,
        "the journal as the earlier kernel left it"
    );
    let earlier = Journal::open(FileJournal::open(&journal).expect("the journal opens"))
        .expect("the journal folds");
    let open: Vec<u64> = earlier
        .unresolved()
        .iter()
        .map(|record| record.id)
        .collect();
    assert_eq!(
        open,
        vec![OPEN_PROJECT_DELETE],
        "only the project's delete is open"
    );
    drop(earlier);

    // The open reads the project back: it is still there and still the entry's, so its delete did not land. The intent is settled
    // as failed, and the next removal retries the project.
    let mut reconciler = reconciler_at(&scratch.dir, fake.socket(), &journal, None);
    assert!(
        reconciler.journal().unresolved().is_empty(),
        "nothing is left open"
    );
    let resolution = reconciler.resolution().clone();
    assert!(
        resolution.adopted.is_empty()
            && resolution.reverted.is_empty()
            && resolution.held.is_empty()
            && resolution.drift.is_empty(),
        "{resolution:?}"
    );
    let settled = journal_states(&journal);
    assert_eq!(
        settled[written.len()..],
        [(OPEN_PROJECT_DELETE, "Failed".to_string())],
        "the open appended one outcome, for the project's delete"
    );
    let mut live: Vec<(&str, String, String)> = live_places(&reconciler)
        .into_iter()
        .map(|(kind, project, _, name)| (kind_name(kind), project, name))
        .collect();
    live.sort();
    assert_eq!(
        live,
        vec![
            ("network", "default".to_string(), PROBE_BRIDGE.to_string()),
            ("project", PROJECT.to_string(), PROJECT.to_string()),
        ]
    );
    assert!(
        fake.writes().iter().all(|write| write.method == "POST"),
        "the open sent no write: {:?}",
        fake.writes()
    );

    // The removal deletes the project, the image in its own store, and then the bridge, under one approval.
    let spec = spec();
    let key = SkKey::from_seed(7);
    let mut verifier: ApprovalVerifier<SshSigVerifier, FileNonces> =
        verifier_for(&scratch, &key, "probe");
    let run = reconciler
        .probe_remove(
            &spec,
            NOW,
            |plan: &Digest| approval_for_plan(&key, plan, 1),
            &mut verifier,
        )
        .expect("the removal runs under its approval");
    assert!(!run.partial, "{run:?}");
    let deleted: Vec<(&str, String)> = run
        .deleted
        .iter()
        .map(|object| (kind_name(object.kind), object.name.clone()))
        .collect();
    assert_eq!(
        deleted,
        vec![
            ("project", PROJECT.to_string()),
            ("network", PROBE_BRIDGE.to_string()),
        ],
        "the project first, then the bridge its grant named"
    );
    assert!(get(&fake, &format!("/1.0/projects/{PROJECT}")).is_none());
    assert!(get(&fake, "/1.0/networks/probe-net?project=default").is_none());
    assert!(fake.images().is_empty(), "{:?}", fake.images());
    assert!(
        reconciler
            .journal()
            .live(&EntryId(ENTRY.to_string()))
            .is_empty(),
        "the journal holds nothing live"
    );
    assert!(reconciler.journal().unresolved().is_empty());
    assert_eq!(
        project_patches(&fake),
        Vec::new(),
        "no project PATCH was sent"
    );
}
