//! The adoption record, `<stateDir>/adoptions.jsonl`: one JSON line per event, each written whole and synced before the step it
//! names goes on. A plan line holds the plan, its digest and the pre-image; then each step has its intent line before its
//! request and its outcome line after. An adoption is never a desired-state entry, and its steps are not journal effects: this
//! record and the instance's own mark are the only places it lives.
//!
//! A service import (`service_import`, `service_release`) is recorded here too, as its own kind of line: the intent before
//! the one key is written or cleared, and the outcome after, each with the instance's uuid and the digest of its pre-image.
//! An import has no plan and no steps, so it is never an open adoption and never one of [`AdoptionRecord::adopted`]'s entries.
//!
//! A move that Incus ran as a copy gives the instance a new uuid. The writer confirms it by the copy rule, and a uuid line
//! records it before the move's outcome line; every later step, and `verify`, expect the uuid the record holds last.
//!
//! The file is the owner's alone: a link, a file that is not regular, one not owned by this user, or one whose mode is not 0600
//! is refused. A torn last line, which a crash mid-write leaves, was never synced as a whole line, so it is dropped: its step
//! either had not started or reads its outcome back from Incus.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use arca_incus::adopt::{
    AdoptStep, AdoptTarget, Allowances, PreImage, SNAPSHOT_PREFIX, SOURCE_PROJECT, uuid_ok,
};
use arca_kernel::canon::{Digest, canonical_json};
use arca_kernel::desired::EntryId;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Failure;

pub use arca_incus::adopt::ADOPT_ENTRY_PREFIX;

/// The record's file name in the state directory.
pub const ADOPTIONS_FILE: &str = "adoptions.jsonl";

/// One adoption's plan: the instance, the projects it moves between, its pre-image, what it allows and localizes, the
/// snapshot's name and the steps. The digest is the hex SHA-256 of the plan's canonical JSON without `digest` and `snapshot`,
/// and the snapshot's name is `arca-adopt-` and the digest's first 12 characters. So the owner who approves the digest approves
/// the allowances with it. A plan that allows and localizes nothing leaves `allowances` out, so its digest is the one it had
/// before plans carried them. A plan that found the instance stopped says so in its pre-image ([`PreImage::stopped`]): its
/// steps neither stop nor start it, and Mark is its last.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AdoptionPlan {
    pub instance: String,
    pub from: String,
    pub to: String,
    pub pre_image: PreImage,
    #[serde(default, skip_serializing_if = "Allowances::is_empty")]
    pub allowances: Allowances,
    pub snapshot: String,
    pub steps: Vec<AdoptStep>,
    pub digest: String,
}

impl AdoptionPlan {
    /// The plan that adopts `target` from `pre_image` with `allowances`, with its digest and its snapshot's name. Its steps
    /// hold LocalizeRoot only when it localizes a device, and Stop and Start only when the pre-image is of a running instance.
    pub fn new(
        target: &AdoptTarget,
        pre_image: PreImage,
        allowances: Allowances,
    ) -> Result<Self, Failure> {
        let steps = AdoptStep::adopt_steps_of(!allowances.localize.is_empty(), pre_image.stopped);
        let mut plan = AdoptionPlan {
            instance: target.instance.clone(),
            from: SOURCE_PROJECT.to_string(),
            to: target.to.clone(),
            pre_image,
            allowances,
            snapshot: String::new(),
            steps,
            digest: String::new(),
        };
        plan.digest = plan.own_digest()?;
        plan.snapshot = format!("{SNAPSHOT_PREFIX}{}", &plan.digest[..12]);
        Ok(plan)
    }

    /// The digest the plan's own fields give, `digest` and `snapshot` left out.
    pub fn own_digest(&self) -> Result<String, Failure> {
        let mut value = serde_json::to_value(self)
            .map_err(|_| Failure::Runtime("the plan does not serialize".to_string()))?;
        if let Value::Object(map) = &mut value {
            map.remove("digest");
            map.remove("snapshot");
        }
        let text = canonical_json(&value)
            .map_err(|_| Failure::Runtime("the plan has no canonical form".to_string()))?;
        Ok(Digest::of(text.as_bytes()).to_hex())
    }

    /// Whether the digest and the snapshot's name are the ones the plan's fields give.
    pub fn verify(&self) -> bool {
        self.own_digest().is_ok_and(|digest| {
            digest == self.digest && self.snapshot == format!("{SNAPSHOT_PREFIX}{}", &digest[..12])
        })
    }

    /// Whether the plan makes a profile's device the instance's own before the move, and so takes it back off on release.
    pub fn localizes(&self) -> bool {
        !self.allowances.localize.is_empty()
    }

    /// Whether the plan found the instance stopped, so that neither its adoption nor its release stops or starts it.
    pub fn stopped(&self) -> bool {
        self.pre_image.stopped
    }

    /// The adoption steps a plan with these allowances and this pre-image runs.
    pub fn adopt_steps(&self) -> Vec<AdoptStep> {
        AdoptStep::adopt_steps_of(self.localizes(), self.stopped())
    }

    /// The release steps a plan with these allowances and this pre-image runs, in full.
    pub fn release_steps(&self) -> Vec<AdoptStep> {
        AdoptStep::release_steps_of(self.localizes(), self.stopped())
    }
}

/// Where a step stands: its intent is written, it is done, or it failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Phase {
    Intent,
    Done,
    Failed,
}

/// What the preflight found beside the pre-image: the snapshots the instance had, and its root's pool and size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Facts {
    pub snapshots: usize,
    pub pool: String,
    pub root_size: String,
}

/// One line of the record.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum Line {
    Plan {
        plan: Box<AdoptionPlan>,
    },
    Facts {
        instance: String,
        facts: Facts,
    },
    Step {
        instance: String,
        step: AdoptStep,
        phase: Phase,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        why: Option<String>,
    },
    /// The new uuid a move's copy gave the instance, written before that move's outcome line.
    Uuid {
        instance: String,
        step: AdoptStep,
        uuid: String,
    },
    /// A service import's mark: its intent before the PATCH, its outcome after.
    Import {
        event: ImportEvent,
    },
    /// A service release's unmark: its intent before the PATCH, its outcome after.
    Release {
        event: ImportEvent,
    },
}

/// One line of a service import or of its release: the service, the project its instance is in, the instance's uuid, the
/// digest of the instance's image (for an import, before the mark; for a release, after the mark is cleared), where the
/// write stands, and for a failure why. No config value is recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImportEvent {
    pub name: String,
    pub project: String,
    pub uuid: String,
    pub pre_image: String,
    pub phase: Phase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

/// The adoption record of one state directory. The steps it is told about are the active adoption's: the one `begin` started,
/// the one `select` chose, or, on open, the one adoption that has not completed.
#[derive(Debug)]
pub struct AdoptionRecord {
    path: PathBuf,
    lines: Vec<Line>,
    /// The length of the file's whole lines, when a torn tail follows them. The first write cuts the file there.
    torn_at: Option<u64>,
    active: Option<String>,
}

impl AdoptionRecord {
    /// Reads the record in `state_dir`. A missing file is an empty record, and nothing is created until the first write. A link,
    /// a file that is not regular, one another user owns, or one whose mode is not 0600 is refused (exit 2), and so is a whole
    /// line that does not decode (exit 1).
    pub fn open(state_dir: &Path) -> Result<Self, Failure> {
        let path = state_dir.join(ADOPTIONS_FILE);
        let mut record = AdoptionRecord {
            path,
            lines: Vec::new(),
            torn_at: None,
            active: None,
        };
        let Some(bytes) = record.read()? else {
            return Ok(record);
        };
        let mut start = 0;
        while start < bytes.len() {
            let rest = &bytes[start..];
            let Some(end) = rest.iter().position(|b| *b == b'\n') else {
                record.torn_at = Some(start as u64);
                break;
            };
            let line = serde_json::from_slice::<Line>(&rest[..end]).map_err(|_| {
                Failure::Runtime(format!(
                    "{} holds a line that does not decode",
                    record.path.display()
                ))
            })?;
            record.lines.push(line);
            start += end + 1;
        }
        let open = record.incomplete();
        if let [only] = open.as_slice() {
            record.active = Some(only.clone());
        }
        Ok(record)
    }

    /// The file's bytes, or `None` when it does not exist, after the trust checks.
    fn read(&self) -> Result<Option<Vec<u8>>, Failure> {
        let shown = self.path.display();
        let meta = match fs::symlink_metadata(&self.path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(Failure::Runtime(format!("{shown} cannot be read"))),
        };
        let uid = arca_approval::effective_uid()
            .map_err(|_| Failure::Runtime("the effective uid cannot be read".to_string()))?;
        if meta.file_type().is_symlink() || !meta.is_file() {
            return Err(Failure::Config(format!("{shown} is not a regular file")));
        }
        if meta.uid() != uid || meta.mode() & 0o777 != 0o600 {
            return Err(Failure::Config(format!(
                "{shown} must be mode 0600 and owned by this user"
            )));
        }
        let mut file = File::open(&self.path)
            .map_err(|_| Failure::Runtime(format!("{shown} cannot be opened")))?;
        same_file(&self.path, &file)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|_| Failure::Runtime(format!("{shown} cannot be read")))?;
        Ok(Some(bytes))
    }

    /// Starts an adoption with `plan`, which becomes the active one. Refused while another adoption has not completed, and for
    /// a plan whose digest is not its own.
    pub fn begin(&mut self, plan: &AdoptionPlan) -> Result<(), Failure> {
        if !plan.verify() {
            return Err(Failure::Refused(
                "the plan's digest is not its own".to_string(),
            ));
        }
        if let Some(open) = self.incomplete().first() {
            return Err(Failure::Refused(format!(
                "the adoption of {open} has not completed; only one may be open at a time"
            )));
        }
        self.append(Line::Plan {
            plan: Box::new(plan.clone()),
        })?;
        self.active = Some(plan.instance.clone());
        Ok(())
    }

    /// Records what the preflight found beside the active adoption's pre-image.
    pub fn facts(&mut self, facts: &Facts) -> Result<(), Failure> {
        let instance = self.active()?;
        self.append(Line::Facts {
            instance,
            facts: facts.clone(),
        })
    }

    /// Makes the recorded adoption of `instance` the active one, for a resume or a release.
    pub fn select(&mut self, instance: &str) -> Result<(), Failure> {
        if self.plan(instance).is_none() {
            return Err(Failure::Refused(format!(
                "the record holds no adoption of {instance}"
            )));
        }
        self.active = Some(instance.to_string());
        Ok(())
    }

    /// Records the intent to run `step` of the active adoption, before its request is sent. It is refused while an
    /// intent is pending.
    pub fn intend(&mut self, step: AdoptStep) -> Result<(), Failure> {
        self.step_line(step, Phase::Intent, None)
    }

    /// Records that `step`, whose intent is the last line of the active adoption, is done.
    pub fn done(&mut self, step: AdoptStep) -> Result<(), Failure> {
        self.step_line(step, Phase::Done, None)
    }

    /// Records that `step`, whose intent is the last line of the active adoption, failed, and why.
    pub fn failed(&mut self, step: AdoptStep, why: &str) -> Result<(), Failure> {
        self.step_line(step, Phase::Failed, Some(why.to_string()))
    }

    /// Records that `step`, a Move or a MoveBack whose intent is the last line of the active adoption, left the instance under
    /// the new `uuid`: Incus ran the move as a copy. It is written before the step's outcome line.
    pub fn moved(&mut self, step: AdoptStep, uuid: &str) -> Result<(), Failure> {
        let instance = self.active()?;
        if !matches!(step, AdoptStep::Move | AdoptStep::MoveBack)
            || self.last(&instance) != Some((step, Phase::Intent))
            || !uuid_ok(uuid)
        {
            return Err(Failure::Runtime(format!(
                "the record of {instance} does not stand where a new uuid can be recorded"
            )));
        }
        self.append(Line::Uuid {
            instance,
            step,
            uuid: uuid.to_string(),
        })
    }

    /// The uuid the latest adoption of `instance` expects the instance to have: the last one a copy gave it, else the
    /// pre-image's. `None` when the record holds no adoption of it.
    pub fn uuid(&self, instance: &str) -> Option<String> {
        self.lines.iter().rev().find_map(|line| match line {
            Line::Uuid {
                instance: named,
                uuid,
                ..
            } if named == instance => Some(uuid.clone()),
            Line::Plan { plan } if plan.instance == instance => Some(plan.pre_image.uuid.clone()),
            _ => None,
        })
    }

    fn step_line(
        &mut self,
        step: AdoptStep,
        phase: Phase,
        why: Option<String>,
    ) -> Result<(), Failure> {
        let instance = self.active()?;
        // An intent waits for no other intent; an outcome closes the intent of its own step. A resume after a crash at
        // an intent line runs that step and records its outcome, without a second intent.
        let last = self.last(&instance);
        let fits = match phase {
            Phase::Intent => !matches!(last, Some((_, Phase::Intent))),
            Phase::Done | Phase::Failed => last == Some((step, Phase::Intent)),
        };
        if !fits {
            return Err(Failure::Runtime(format!(
                "the record of {instance} does not stand where {step:?} can be recorded"
            )));
        }
        self.append(Line::Step {
            instance,
            step,
            phase,
            why,
        })
    }

    fn active(&self) -> Result<String, Failure> {
        self.active
            .clone()
            .ok_or_else(|| Failure::Runtime("no adoption is active in the record".to_string()))
    }

    /// The latest plan recorded for `instance`.
    pub fn plan(&self, instance: &str) -> Option<AdoptionPlan> {
        self.lines.iter().rev().find_map(|line| match line {
            Line::Plan { plan } if plan.instance == instance => Some(plan.as_ref().clone()),
            _ => None,
        })
    }

    /// The last step line of the latest adoption of `instance`, as its step and phase: `None` before its first step.
    pub fn last(&self, instance: &str) -> Option<(AdoptStep, Phase)> {
        for line in self.lines.iter().rev() {
            match line {
                Line::Plan { plan } if plan.instance == instance => return None,
                Line::Step {
                    instance: named,
                    step,
                    phase,
                    ..
                } if named == instance => return Some((*step, *phase)),
                _ => {}
            }
        }
        None
    }

    /// Whether the latest adoption of `instance` is complete: the last of its plan's adoption steps is done. That is Start, or
    /// Mark for a plan that found the instance stopped.
    pub fn complete(&self, instance: &str) -> bool {
        let Some(plan) = self.plan(instance) else {
            return false;
        };
        let last = self.last(instance);
        plan.adopt_steps()
            .last()
            .is_some_and(|step| last == Some((*step, Phase::Done)))
    }

    /// Whether the latest adoption of `instance` has been released in full: its DeleteProject is done. A release recorded
    /// before releases deleted the adoption project ends at ReleaseStart and is not; `release` finishes it.
    pub fn released(&self, instance: &str) -> bool {
        self.last(instance) == Some((AdoptStep::DeleteProject, Phase::Done))
    }

    /// The instances whose latest adoption has neither completed nor been released in full.
    pub fn incomplete(&self) -> Vec<String> {
        self.instances()
            .into_iter()
            .filter(|instance| !self.complete(instance) && !self.released(instance))
            .collect()
    }

    /// The entry of every instance the record holds whose latest adoption has not been released in full: `adopt-<instance>`.
    /// The watch counts these as live entries, so the adoption project, which carries the mark until DeleteProject, is never
    /// an orphan.
    pub fn adopted(&self) -> BTreeSet<EntryId> {
        self.instances()
            .into_iter()
            .filter(|instance| !self.released(instance))
            .filter_map(|instance| EntryId::parse(&format!("{ADOPT_ENTRY_PREFIX}{instance}")))
            .collect()
    }

    /// Records one line of a service import.
    pub fn import(&mut self, event: ImportEvent) -> Result<(), Failure> {
        self.append(Line::Import { event })
    }

    /// Records one line of a service release.
    pub fn release(&mut self, event: ImportEvent) -> Result<(), Failure> {
        self.append(Line::Release { event })
    }

    /// The import of the service `name` that stands now: its last done import line, unless a done release follows it.
    pub fn imported(&self, name: &str) -> Option<ImportEvent> {
        for line in self.lines.iter().rev() {
            match line {
                Line::Release { event } if event.name == name && event.phase == Phase::Done => {
                    return None;
                }
                Line::Import { event } if event.name == name && event.phase == Phase::Done => {
                    return Some(event.clone());
                }
                _ => {}
            }
        }
        None
    }

    /// Every service import and release line, oldest first, with whether it is a release.
    pub fn import_events(&self) -> Vec<(bool, ImportEvent)> {
        self.lines
            .iter()
            .filter_map(|line| match line {
                Line::Import { event } => Some((false, event.clone())),
                Line::Release { event } => Some((true, event.clone())),
                _ => None,
            })
            .collect()
    }

    fn instances(&self) -> BTreeSet<String> {
        self.lines
            .iter()
            .filter_map(|line| match line {
                Line::Plan { plan } => Some(plan.instance.clone()),
                _ => None,
            })
            .collect()
    }

    /// Appends one line, whole, and syncs it. The file is created at mode 0600 on the first write, and a torn tail is cut
    /// first. The trust checks run again before every write.
    fn append(&mut self, line: Line) -> Result<(), Failure> {
        let shown = self.path.display().to_string();
        let fail = |what: &str| Failure::Runtime(format!("{shown} {what}"));
        let created = self.read()?.is_none();
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(&self.path)
            .map_err(|_| fail("cannot be opened for writing"))?;
        same_file(&self.path, &file)?;
        if let Some(keep) = self.torn_at.take() {
            file.set_len(keep)
                .map_err(|_| fail("cannot drop its torn last line"))?;
        }
        let mut bytes = serde_json::to_vec(&line).map_err(|_| fail("cannot encode a line"))?;
        bytes.push(b'\n');
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|_| fail("cannot be written"))?;
        if created {
            let dir = self.path.parent().unwrap_or(Path::new("."));
            File::open(dir)
                .and_then(|dir| dir.sync_all())
                .map_err(|_| fail("cannot be synced into its directory"))?;
        }
        self.lines.push(line);
        Ok(())
    }
}

/// The adopt steps a resume runs, from the recorded last step: all of them before the first; from a step whose intent or
/// failure is the last line, that step again; and after a done step, the ones after it. A release step, a step the plan does not
/// have, or a plan whose steps are not the ones its allowances and its pre-image give, leaves nothing to resume.
pub fn resume_steps(plan: &AdoptionPlan, last: Option<(AdoptStep, Phase)>) -> Vec<AdoptStep> {
    if plan.steps != plan.adopt_steps() {
        return Vec::new();
    }
    let Some((step, phase)) = last else {
        return plan.steps.clone();
    };
    match plan.steps.iter().position(|planned| *planned == step) {
        Some(at) if phase == Phase::Done => plan.steps[at + 1..].to_vec(),
        Some(at) => plan.steps[at..].to_vec(),
        None => Vec::new(),
    }
}

/// The release steps of `plan` left after the recorded last step. Before the move, the instance never left `default`, so
/// RestoreRoot, ReleaseStart and DeleteProject are left: a root LocalizeRoot may have made its own comes back off, and the
/// project goes. After Move, MoveBack is the first; after Mark, Unmark comes before it; and after Start, the whole release
/// runs. Within a release, a step whose intent or failure is the last line runs again. A plan that localizes nothing has no
/// RestoreRoot, and one that found the instance stopped has no ReleaseStop and no ReleaseStart. Each step still reads first and
/// sends nothing when its outcome holds, so a step that may or may not have landed is safe to list.
pub fn release_steps(plan: &AdoptionPlan, last: Option<(AdoptStep, Phase)>) -> Vec<AdoptStep> {
    use AdoptStep as S;
    let from = match last {
        None => 3,
        Some((step, phase)) => {
            let done = usize::from(phase == Phase::Done);
            match step {
                S::CreateProject | S::Stop | S::Snapshot | S::PinNic | S::LocalizeRoot => 3,
                S::Move => 2,
                S::Mark => 1,
                S::Start => 0,
                S::ReleaseStop => done,
                S::Unmark => 1 + done,
                S::MoveBack => 2 + done,
                S::RestoreRoot => 3 + done,
                S::ReleaseStart => 4 + done,
                S::DeleteProject => 5 + done,
            }
        }
    };
    let planned = plan.release_steps();
    AdoptStep::RELEASE[from..]
        .iter()
        .copied()
        .filter(|step| planned.contains(step))
        .collect()
}

/// Refuses an opened file that is not the regular file the path names now, or that is not this user's at mode 0600, so a link
/// swapped in after the checks is never read or written.
fn same_file(path: &Path, file: &File) -> Result<(), Failure> {
    let shown = path.display();
    let opened = file
        .metadata()
        .map_err(|_| Failure::Runtime(format!("{shown} cannot be read")))?;
    let linked = fs::symlink_metadata(path)
        .map_err(|_| Failure::Runtime(format!("{shown} cannot be read")))?;
    let uid = arca_approval::effective_uid()
        .map_err(|_| Failure::Runtime("the effective uid cannot be read".to_string()))?;
    if linked.file_type().is_symlink()
        || (linked.dev(), linked.ino()) != (opened.dev(), opened.ino())
        || !opened.is_file()
        || opened.uid() != uid
        || opened.mode() & 0o777 != 0o600
    {
        return Err(Failure::Config(format!(
            "{shown} changed while it was opened, or is not this user's at mode 0600"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("arca-adoptions-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("the directory");
        dir
    }

    fn plan() -> AdoptionPlan {
        let pre = PreImage {
            uuid: "u1".to_string(),
            created_at: String::new(),
            mac: "10:66:6a:00:00:01".to_string(),
            ipv4: "192.0.2.131".parse().expect("an address"),
            profiles: vec!["default".to_string()],
            config: BTreeMap::new(),
            devices: BTreeMap::new(),
            config_hash: String::new(),
            stopped: false,
        };
        let target = AdoptTarget {
            instance: "svc1".to_string(),
            to: "arcak-adopt-svc1".to_string(),
        };
        AdoptionPlan::new(&target, pre, Allowances::default()).expect("the plan")
    }

    #[test]
    fn a_record_that_is_a_link_or_not_mode_0600_is_refused() {
        let state = dir("trust");
        let file = state.join(ADOPTIONS_FILE);
        fs::write(&file, b"").expect("the file");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).expect("mode");
        assert!(matches!(
            AdoptionRecord::open(&state),
            Err(Failure::Config(_))
        ));
        fs::remove_file(&file).expect("remove");
        let real = state.join("real.jsonl");
        fs::write(&real, b"").expect("the target");
        fs::set_permissions(&real, fs::Permissions::from_mode(0o600)).expect("mode");
        symlink(&real, &file).expect("the link");
        assert!(matches!(
            AdoptionRecord::open(&state),
            Err(Failure::Config(_))
        ));
        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn the_record_keeps_the_plan_and_each_step_and_drops_a_torn_tail() {
        let state = dir("lines");
        let plan = plan();
        let mut record = AdoptionRecord::open(&state).expect("an empty record");
        assert!(
            record.intend(AdoptStep::CreateProject).is_err(),
            "no adoption is active"
        );
        record.begin(&plan).expect("begin");
        let mode = fs::metadata(state.join(ADOPTIONS_FILE))
            .expect("the file")
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        record.intend(AdoptStep::CreateProject).expect("intent");
        assert!(
            record.intend(AdoptStep::Stop).is_err(),
            "an intent is pending"
        );
        record.done(AdoptStep::CreateProject).expect("done");
        record.intend(AdoptStep::Stop).expect("intent");
        // A crash mid-write leaves a torn line, which the next open drops.
        let mut file = OpenOptions::new()
            .append(true)
            .open(state.join(ADOPTIONS_FILE))
            .expect("open");
        file.write_all(b"{\"kind\":\"step\",\"inst").expect("torn");
        let mut again = AdoptionRecord::open(&state).expect("reopen");
        assert_eq!(again.plan("svc1"), Some(plan.clone()));
        assert_eq!(again.last("svc1"), Some((AdoptStep::Stop, Phase::Intent)));
        assert_eq!(again.incomplete(), vec!["svc1".to_string()]);
        assert!(again.begin(&plan).is_err(), "one adoption is open");
        again
            .failed(AdoptStep::Stop, "it timed out")
            .expect("failed");
        let reread = AdoptionRecord::open(&state).expect("reopen");
        assert_eq!(reread.last("svc1"), Some((AdoptStep::Stop, Phase::Failed)));
        assert_eq!(
            reread.adopted(),
            BTreeSet::from([EntryId("adopt-svc1".to_string())])
        );
        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn a_plan_digest_covers_the_pre_image_and_names_the_snapshot() {
        let plan = plan();
        assert!(plan.verify());
        assert_eq!(plan.snapshot, format!("arca-adopt-{}", &plan.digest[..12]));
        let mut changed = plan.clone();
        changed.pre_image.ipv4 = "192.0.2.132".parse().expect("an address");
        assert!(
            !changed.verify(),
            "a changed pre-image does not keep the digest"
        );
        let again = AdoptionPlan::new(
            &AdoptTarget {
                instance: plan.instance.clone(),
                to: plan.to.clone(),
            },
            plan.pre_image.clone(),
            Allowances::default(),
        )
        .expect("the same plan");
        assert_eq!(again.digest, plan.digest, "the digest is stable");
    }

    #[test]
    fn a_plan_digest_covers_its_allowances_and_a_plan_without_any_keeps_its_old_form() {
        let plan = plan();
        let text = serde_json::to_value(&plan).expect("the plan serializes");
        assert!(
            text.get("allowances").is_none(),
            "a plan that allows nothing has no allowances field, so its digest is the one it had before"
        );
        let target = AdoptTarget {
            instance: plan.instance.clone(),
            to: plan.to.clone(),
        };
        let mut allowances = Allowances::default();
        allowances.project.insert(
            "restricted.containers.nesting".to_string(),
            "allow".to_string(),
        );
        let allowing = AdoptionPlan::new(&target, plan.pre_image.clone(), allowances.clone())
            .expect("a plan that allows nesting");
        assert_ne!(
            allowing.digest, plan.digest,
            "the digest covers the allowances"
        );
        assert!(allowing.verify());
        let mut widened = allowing.clone();
        widened
            .allowances
            .project
            .insert("restricted.devices.gpu".to_string(), "allow".to_string());
        assert!(
            !widened.verify(),
            "a widened allowance does not keep the digest"
        );
        assert_eq!(allowing.steps, AdoptStep::adopt_steps(false));

        allowances.localize.insert(
            "root".to_string(),
            BTreeMap::from([
                ("type".to_string(), "disk".to_string()),
                ("path".to_string(), "/".to_string()),
                ("pool".to_string(), "pool1".to_string()),
            ]),
        );
        let localizing = AdoptionPlan::new(&target, plan.pre_image.clone(), allowances)
            .expect("a plan that localizes the root");
        assert_eq!(localizing.steps, AdoptStep::ADOPT.to_vec());
        assert_eq!(localizing.release_steps(), AdoptStep::RELEASE.to_vec());
        let decoded: AdoptionPlan =
            serde_json::from_value(serde_json::to_value(&localizing).expect("serializes"))
                .expect("decodes");
        assert_eq!(decoded, localizing);
        assert!(decoded.verify());
    }

    /// A running instance's plan, as the record held it before stopped instances could be planned: one that localizes nothing
    /// and one that localizes the root.
    const RECORDED_PLAIN: &str = r#"{"instance":"svc1","from":"default","to":"arcak-adopt-svc1","preImage":{"uuid":"00000000-0000-4000-8000-0000000000a1","createdAt":"2026-01-01T00:00:00Z","mac":"10:66:6a:00:00:01","ipv4":"192.0.2.131","profiles":["default"],"config":{"image.os":"debian"},"devices":{"eth0":{"name":"eth0","network":"br1","type":"nic"},"root":{"path":"/","pool":"pool1","size":"60GiB","type":"disk"}},"configHash":"0000000000000000000000000000000000000000000000000000000000000000"},"snapshot":"arca-adopt-e7f71f3818da","steps":["createProject","stop","snapshot","pinNic","move","mark","start"],"digest":"e7f71f3818da2cbfb2e83ab0eb739b683a129f5301e521a16b83a7bb708bd90a"}"#;
    const RECORDED_ROOTED: &str = r#"{"instance":"svc1","from":"default","to":"arcak-adopt-svc1","preImage":{"uuid":"00000000-0000-4000-8000-0000000000a1","createdAt":"2026-01-01T00:00:00Z","mac":"10:66:6a:00:00:01","ipv4":"192.0.2.131","profiles":["default"],"config":{"image.os":"debian"},"devices":{"eth0":{"name":"eth0","network":"br1","type":"nic"},"root":{"path":"/","pool":"pool1","size":"60GiB","type":"disk"}},"configHash":"0000000000000000000000000000000000000000000000000000000000000000"},"allowances":{"localize":{"root":{"path":"/","pool":"pool1","size":"60GiB","type":"disk"}}},"snapshot":"arca-adopt-b4cd1ab66f2f","steps":["createProject","stop","snapshot","pinNic","localizeRoot","move","mark","start"],"digest":"b4cd1ab66f2fda0460301b182e1a0b499cabd817f710681b0a4a69b348f64a2a"}"#;

    #[test]
    fn a_running_plan_keeps_its_digest_and_a_recorded_plan_stays_valid() {
        let target = AdoptTarget {
            instance: "svc1".to_string(),
            to: "arcak-adopt-svc1".to_string(),
        };
        for recorded in [RECORDED_PLAIN, RECORDED_ROOTED] {
            let old: AdoptionPlan =
                serde_json::from_str(recorded).expect("the recorded plan decodes");
            assert!(old.verify(), "its digest is still its own");
            assert!(!old.pre_image.stopped);
            assert_eq!(
                old.steps,
                old.adopt_steps(),
                "its steps are still the ones it runs"
            );
            assert!(old.steps.contains(&AdoptStep::Stop) && old.steps.contains(&AdoptStep::Start));
            // The same pre-image and allowances plan again to the same bytes and the same digest.
            let again = AdoptionPlan::new(&target, old.pre_image.clone(), old.allowances.clone())
                .expect("the plan");
            assert_eq!(serde_json::to_string(&again).expect("serializes"), recorded);
            assert_eq!(again.digest, old.digest);
        }

        // A record that holds the old plan line and its steps through Start reads as complete, and a release of it runs in full.
        let state = dir("old-record");
        let path = state.join(ADOPTIONS_FILE);
        let mut lines = format!("{{\"kind\":\"plan\",\"plan\":{RECORDED_PLAIN}}}\n");
        for step in [
            "createProject",
            "stop",
            "snapshot",
            "pinNic",
            "move",
            "mark",
            "start",
        ] {
            for phase in ["intent", "done"] {
                lines.push_str(&format!(
                    "{{\"kind\":\"step\",\"instance\":\"svc1\",\"step\":\"{step}\",\"phase\":\"{phase}\"}}\n"
                ));
            }
        }
        fs::write(&path, lines).expect("the record");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("mode");
        let record = AdoptionRecord::open(&state).expect("the old record opens");
        let plan = record.plan("svc1").expect("the plan");
        assert!(plan.verify());
        assert!(record.complete("svc1"));
        assert!(record.incomplete().is_empty());
        assert_eq!(
            release_steps(&plan, record.last("svc1")),
            AdoptStep::release_steps(false)
        );
        let _ = fs::remove_dir_all(&state);
    }
}
