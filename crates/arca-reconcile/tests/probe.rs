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
use arca_providers::incus::{Family, IncusProvider, RetainedSet};
use arca_reconcile::probe::{PROBE_PLUGIN, ProbeSpec};
use arca_reconcile::{PluginSet, Reconciler};
use common::*;
use serde_json::Value;

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
    }
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
        4,
        "the instance, the volume, the bucket and the project"
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
        reconciler
            .journal()
            .live(&EntryId(ENTRY.to_string()))
            .is_empty()
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
