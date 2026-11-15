//! Running an approved import, and its undo.
//!
//! [`run`] does the steps of [`Plan`] in order through three seams: [`ImportIncus`] (the Incus objects), [`DiskCopy`]
//! (Ceph) and [`Probe`] (the read-back's port check and its pause). A failure before the start removes, newest first,
//! what the run made; a failed read-back leaves the VM running and says so. [`undo`] removes one import by its instance.

use std::net::Ipv4Addr;

use serde_json::{Value, json};

use crate::copy::{DiskCopy, SourceImage, TargetImage};
use crate::plan::{Plan, Target};

/// How many times the read-back asks the guest's port, with a [`Probe::pause`] between: about five minutes.
pub const READ_BACK_TRIES: u32 = 60;

/// What Incus says of the target pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolInfo {
    pub driver: String,
    /// `ceph.osd.pool_name`, else the pool's own name.
    pub osd_pool: String,
    /// `ceph.cluster_name`, else `ceph`.
    pub cluster: String,
    /// `ceph.user.name`, else `admin`.
    pub client: String,
}

/// Where the import's objects go, as Incus says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    pub pool: PoolInfo,
    /// The project custom volumes of the target project live in: its own with `features.storage.volumes`, else
    /// `default`.
    pub volume_project: String,
    /// The project its ACLs live in: its own with `features.networks`, else `default`.
    pub acl_project: String,
    /// Whether Incus manages the host bridge; `None` when the request names none.
    pub managed: Option<bool>,
}

/// What a removed instance held that the import also made.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Held {
    /// Custom volumes its disk devices named, by pool and name.
    pub volumes: Vec<(String, String)>,
    /// ACLs its NICs named.
    pub acls: Vec<String>,
}

/// The Incus side of an import.
pub trait ImportIncus {
    /// Reads the target pool, the project's features and the bridge. A pool or bridge Incus does not have is an error.
    fn place(&self, project: &str, pool: &str, bridge: Option<&str>) -> Result<Place, String>;
    /// The projects the custom volumes and the ACLs of `project` live in, as [`Place`] names them.
    fn homes(&self, project: &str) -> Result<(String, String), String>;
    fn instance_exists(&self, project: &str, name: &str) -> Result<bool, String>;
    fn create_acl(&self, project: &str, body: &Value) -> Result<(), String>;
    fn create_volume(&self, project: &str, pool: &str, body: &Value) -> Result<(), String>;
    fn create_instance(&self, project: &str, body: &Value) -> Result<(), String>;
    fn start(&self, project: &str, name: &str) -> Result<(), String>;
    /// Whether the guest's Incus agent answers now.
    fn agent_answers(&self, project: &str, name: &str) -> bool;
    /// Stops and deletes an instance that carries the import mark, and says what it held. `None` when it is gone.
    fn remove_instance(&self, project: &str, name: &str) -> Result<Option<Held>, String>;
    /// Deletes a custom volume that carries the import mark. `false` when it is gone.
    fn remove_volume(&self, project: &str, pool: &str, name: &str) -> Result<bool, String>;
    /// Deletes an ACL that carries the import mark. `false` when it is gone.
    fn remove_acl(&self, project: &str, name: &str) -> Result<bool, String>;
}

/// The read-back's view of the guest.
pub trait Probe {
    /// Whether `ip` accepts a TCP connection on `port` now.
    fn port_open(&self, ip: Ipv4Addr, port: u16) -> bool;
    /// The wait between two tries.
    fn pause(&self);
}

/// The Ceph image Incus makes for a block volume: `virtual-machine_<project>_<name>.block` for a VM's root,
/// `custom_<project>_<name>.block` for a custom volume. The project prefix is left out in the `default` project.
pub fn incus_rbd_image(kind: &str, project: &str, name: &str) -> String {
    let volume = if project == "default" {
        name.to_string()
    } else {
        format!("{project}_{name}")
    };
    format!("{kind}_{volume}.block")
}

/// Why a run stopped, and what its undo did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// The step that failed, in words.
    pub step: &'static str,
    pub message: String,
    /// What the undo removed, newest first.
    pub removed: Vec<String>,
    /// What the undo could not remove, with why.
    pub left: Vec<String>,
}

impl Failure {
    /// The failure in one paragraph, for the answer and the journal.
    pub fn words(&self) -> String {
        let mut words = format!("{}: {}.", self.step, self.message);
        if !self.removed.is_empty() {
            words.push_str(&format!(
                " Removed what it made: {}.",
                self.removed.join(", ")
            ));
        }
        if !self.left.is_empty() {
            words.push_str(&format!(" Could not remove: {}.", self.left.join("; ")));
        } else if self.removed.is_empty() {
            words.push_str(" Nothing was made.");
        }
        words.push_str(" The source was not written.");
        words
    }
}

/// One object the run made, for its undo.
enum Made {
    Acl(String, String),
    Volume(String, String, String),
    Instance(String, String),
}

fn fail(step: &'static str, message: impl Into<String>) -> Failure {
    Failure {
        step,
        message: message.into(),
        removed: Vec::new(),
        left: Vec::new(),
    }
}

/// Removes what a run made, newest first.
fn undo_made(incus: &dyn ImportIncus, made: Vec<Made>, failure: &mut Failure) {
    for object in made.into_iter().rev() {
        let (what, outcome) = match &object {
            Made::Instance(project, name) => (
                format!("the instance {name}"),
                incus
                    .remove_instance(project, name)
                    .map(|held| held.is_some()),
            ),
            Made::Volume(project, pool, name) => (
                format!("the volume {name}"),
                incus.remove_volume(project, pool, name),
            ),
            Made::Acl(project, name) => {
                (format!("the ACL {name}"), incus.remove_acl(project, name))
            }
        };
        match outcome {
            Ok(_) => failure.removed.push(what),
            Err(why) => failure.left.push(format!("{what} ({why})")),
        }
    }
}

/// Runs `plan`. `source_pool` is the one source pool this node reads. The source pool may be on another Ceph cluster than
/// the Incus pool: `copy` reads it on its own cluster ([`DiskCopy::cluster`]) and writes on the Incus pool's, and the
/// answer names both.
pub fn run(
    plan: &Plan,
    source_pool: &str,
    incus: &dyn ImportIncus,
    copy: &dyn DiskCopy,
    probe: &dyn Probe,
) -> Result<Value, Failure> {
    let request = &plan.request;
    let project = request.project.as_str();
    plan.check_source_pool(source_pool)
        .map_err(|why| fail("Checking the source", why))?;
    let place = incus
        .place(project, &request.pool, request.bridge.as_deref())
        .map_err(|why| fail("Reading Incus", why))?;
    if place.pool.driver != "ceph" {
        return Err(fail(
            "Checking the target pool",
            format!(
                "the Incus pool {} uses the {} driver; an import copies into a Ceph pool",
                request.pool, place.pool.driver
            ),
        ));
    }
    if !plan.acls.is_empty() && place.managed == Some(false) {
        return Err(fail(
            "Checking the bridge",
            format!(
                "the host bridge {} is not managed by Incus, so its NICs cannot carry the network ACL that holds the source's security-group rules; leaving them out would open the VM",
                request.bridge.as_deref().unwrap_or_default()
            ),
        ));
    }
    if incus
        .instance_exists(project, &request.name)
        .map_err(|why| fail("Reading Incus", why))?
    {
        return Err(fail(
            "Checking the name",
            format!(
                "an instance named {} already exists in project {project}",
                request.name
            ),
        ));
    }
    let sources: Vec<SourceImage> = plan
        .copies
        .iter()
        .map(|step| SourceImage {
            pool: step.source.pool.clone(),
            image: step.source.image.clone(),
            size_mib: step.source.size_mib,
        })
        .collect();
    for source in &sources {
        let watchers = copy
            .watchers(source)
            .map_err(|why| fail("Checking the source disks", why))?;
        if !watchers.is_empty() {
            return Err(fail(
                "Checking the source disks",
                format!(
                    "the source still runs: its disk {} is held open by {}. Power the source off in the outside hypervisor first",
                    source.spec(),
                    watchers.join(", ")
                ),
            ));
        }
    }
    let mut made: Vec<Made> = Vec::new();
    let made_result = (|| -> Result<(), Failure> {
        for acl in &plan.acls {
            incus
                .create_acl(&place.acl_project, &plan.acl_body(acl))
                .map_err(|why| fail("Making the network ACL", why))?;
            made.push(Made::Acl(place.acl_project.clone(), acl.name.clone()));
        }
        for (name, size) in plan.volumes() {
            incus
                .create_volume(
                    &place.volume_project,
                    &request.pool,
                    &plan.volume_body(&name, size),
                )
                .map_err(|why| fail("Making a disk volume", why))?;
            made.push(Made::Volume(
                place.volume_project.clone(),
                request.pool.clone(),
                name,
            ));
        }
        incus
            .create_instance(project, &plan.instance_body(place.managed.unwrap_or(false)))
            .map_err(|why| fail("Making the VM", why))?;
        made.push(Made::Instance(project.to_string(), request.name.clone()));
        for (step, source) in plan.copies.iter().zip(&sources) {
            let image = match &step.target {
                Target::Root => incus_rbd_image("virtual-machine", project, &request.name),
                Target::Volume(name) => incus_rbd_image("custom", &place.volume_project, name),
            };
            let target = TargetImage {
                cluster: place.pool.cluster.clone(),
                client: place.pool.client.clone(),
                pool: place.pool.osd_pool.clone(),
                image,
            };
            let size = copy
                .target_size(&target)
                .map_err(|why| fail("Finding the disk Incus made", why))?;
            let want = source.size_mib * 1024 * 1024;
            if size != want {
                return Err(fail(
                    "Finding the disk Incus made",
                    format!(
                        "the image {} is {size} bytes and the source disk {want}; nothing was copied into it",
                        target.spec()
                    ),
                ));
            }
            copy.copy(source, &target).map_err(|why| {
                fail(
                    "Copying a disk",
                    format!("{} into {}: {why}", source.spec(), target.spec()),
                )
            })?;
        }
        if request.start {
            incus
                .start(project, &request.name)
                .map_err(|why| fail("Starting the VM", why))?;
        }
        Ok(())
    })();
    if let Err(mut failure) = made_result {
        undo_made(incus, made, &mut failure);
        return Err(failure);
    }
    let (agent, answered) = if request.start {
        read_back(plan, incus, probe)
    } else {
        (None, None)
    };
    let note = match (request.start, answered) {
        (false, _) => "The VM was made and its disks copied; it is stopped, as asked.".to_string(),
        (true, Some(false)) => format!(
            "The VM runs, but {} did not answer on port {} within the read-back. It is left running so its console can be looked at; instance_import_undo removes it.",
            plan.read_back
                .map(|(ip, _)| ip.to_string())
                .unwrap_or_default(),
            crate::plan::READ_BACK_PORT
        ),
        (true, _) => "The VM runs and read back.".to_string(),
    };
    Ok(json!({
        "instance": request.name,
        "project": project,
        "source": { "id": plan.source.id, "name": plan.source.name, "digest": plan.source.digest },
        "mark": plan.mark,
        "acls": plan.acls.iter().map(|acl| acl.name.clone()).collect::<Vec<_>>(),
        "volumes": plan.volumes().into_iter().map(|(name, _)| name).collect::<Vec<_>>(),
        "disks": plan.copies.iter().map(|step| json!({
            "device": step.device,
            "from": format!("{}/{}", step.source.pool, step.source.image),
            "sizeMib": step.source.size_mib,
        })).collect::<Vec<_>>(),
        "ceph": {
            "from": copy.cluster(),
            "to": place.pool.cluster,
            "copied": copied_words(source_pool, copy.cluster(), &request.pool, &place.pool),
        },
        "started": request.start,
        "readBack": {
            "agent": agent,
            "address": plan.read_back.filter(|_| request.start).map(|(ip, port)| format!("{ip}:{port}")),
            "answered": answered,
        },
        "note": note,
    }))
}

/// Where the disks were copied from and to, in words: `Copied from the Ceph cluster X to the Ceph cluster Y`.
pub fn copied_words(
    source_pool: &str,
    source_cluster: &str,
    pool: &str,
    target: &PoolInfo,
) -> String {
    let to = if source_cluster == target.cluster {
        format!("the same cluster, into the Incus pool {pool}")
    } else {
        format!(
            "the Ceph cluster {}, into the Incus pool {pool}",
            target.cluster
        )
    };
    format!(
        "Copied from the Ceph cluster {source_cluster} (the pool {source_pool}, read only) to {to} (written as client.{})",
        target.client
    )
}

/// Asks the agent and the port until the port answers or the tries run out.
fn read_back(
    plan: &Plan,
    incus: &dyn ImportIncus,
    probe: &dyn Probe,
) -> (Option<bool>, Option<bool>) {
    let (project, name) = (&plan.request.project, &plan.request.name);
    let mut agent = false;
    for attempt in 0..READ_BACK_TRIES {
        if attempt > 0 {
            probe.pause();
        }
        agent = agent || incus.agent_answers(project, name);
        match plan.read_back {
            Some((ip, port)) if probe.port_open(ip, port) => return (Some(agent), Some(true)),
            Some(_) => {}
            // No address to ask: the agent is all there is to read back.
            None if agent => return (Some(true), None),
            None => {}
        }
    }
    (Some(agent), plan.read_back.map(|_| false))
}

/// Removes the import whose instance is `name` in `project`: the instance, then the volumes and ACLs it named that
/// carry the import mark. The source is not touched.
pub fn undo(incus: &dyn ImportIncus, project: &str, name: &str) -> Result<Value, String> {
    let (volume_project, acl_project) = incus.homes(project)?;
    let held = incus
        .remove_instance(project, name)?
        .ok_or_else(|| format!("there is no instance {name} in project {project}"))?;
    let mut removed = vec![format!("the instance {name}")];
    let mut left = Vec::new();
    for (pool, volume) in &held.volumes {
        match incus.remove_volume(&volume_project, pool, volume) {
            Ok(true) => removed.push(format!("the volume {volume}")),
            Ok(false) => {}
            Err(why) => left.push(format!("the volume {volume} ({why})")),
        }
    }
    for acl in &held.acls {
        match incus.remove_acl(&acl_project, acl) {
            Ok(true) => removed.push(format!("the ACL {acl}")),
            Ok(false) => {}
            Err(why) => left.push(format!("the ACL {acl} ({why})")),
        }
    }
    Ok(json!({
        "instance": name,
        "project": project,
        "removed": removed,
        "left": left,
        "note": "The source was not touched: power it on again in the outside hypervisor to go back.",
    }))
}
