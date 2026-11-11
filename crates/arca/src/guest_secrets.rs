//! The secrets of a virtual-machine service, written into its guest at each start.
//!
//! A container's secret reaches it as an environment value in its Incus config. A virtual machine's never does: its
//! Incus config, the desired state and the effect journal hold a reference at most. The value is read from the secret
//! store (the private files `service_secret_set` writes under `<stateDir>/secrets`) by the provider's own rule
//! (`arca_providers::incus::read_entry_secret`) and sent as the body of an Incus
//! file request, which Incus hands to the guest agent: `/etc/arca-secrets` (root's, mode 0700) and one file per secret
//! in it, named by the secret (root's, mode 0600). The files are on the guest's own disk, so a guest that restarts by
//! itself still has them; `instance_start` and `instance_restart` write them again, which is how a new value arrives.
//!
//! Nothing here is an effect of a plugin: the call is made after the instance action, for a service record whose
//! instance is this entry's own by its journalled project and uuid, and its outcome is a line of the instance action
//! journal that names the secrets and never a value. A service in a tenant is served the same way, in its tenant's
//! project: the project is the one the effect journal holds for the instance, never the one a call names, and the
//! writer reaches that tenant's project and no other tenant's. Commands (the exec seam) stay in the node's own project.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arca_incus::exec::{GUEST_SECRETS_DIR, GuestSecretsDir, SecretName, ServiceFile};
use arca_providers::incus::read_entry_secret;

use crate::config::NodeConfig;
use crate::kernel::Kernel;
use crate::service::ServiceConfig;
use crate::service_exec::{ExecTarget, writer_for};

/// How long a start waits for the guest agent to take the first file. A new virtual machine boots before it answers.
const AGENT_WAIT: Duration = Duration::from_secs(120);
/// The pause between two tries while the agent does not answer.
const AGENT_PAUSE: Duration = Duration::from_secs(3);
/// The time one file request may take.
const REQUEST_BUDGET: Duration = Duration::from_secs(15);

/// Why a virtual machine's secrets were not written into its guest. The words never hold a value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotWritten {
    /// In plain words, what was not done and why.
    pub why: String,
    /// Whether the same call may succeed when it is made again with nothing else changed: the guest's agent was not up
    /// yet, or a request did not get through. `false` when something has to be put right first.
    pub retry: bool,
}

impl NotWritten {
    fn later(why: impl Into<String>) -> Self {
        Self {
            why: why.into(),
            retry: true,
        }
    }
    fn fixed(why: impl Into<String>) -> Self {
        Self {
            why: why.into(),
            retry: false,
        }
    }
}

/// Writes a virtual-machine service's secrets into its guest.
pub struct GuestSecrets {
    cfg: Arc<NodeConfig>,
    kernel: Arc<Mutex<Kernel>>,
    wait: Duration,
    pause: Duration,
}

impl GuestSecrets {
    pub fn new(cfg: Arc<NodeConfig>, kernel: Arc<Mutex<Kernel>>) -> Self {
        Self {
            cfg,
            kernel,
            wait: AGENT_WAIT,
            pause: AGENT_PAUSE,
        }
    }

    /// The same writer with another wait for the guest agent, so a test need not wait two minutes.
    pub fn with_wait(mut self, wait: Duration, pause: Duration) -> Self {
        self.wait = wait;
        self.pause = pause;
        self
    }

    /// Writes the stored secrets of the virtual-machine service whose instance is `instance` in `project` into its
    /// guest. `Ok(None)` when there is nothing to write: no service record of Arca's names that instance in that
    /// project, it is not a virtual machine Arca created, or none of its secrets is set. `Ok(Some(names))` names the
    /// secrets written.
    ///
    /// `project` and `instance` are the caller's words and only find the record. Where the files are sent is Arca's
    /// own: the instance the record's entry created, as the effect journal holds it (project and uuid), which must be
    /// the project the record names and, for a service in a tenant, that tenant's own project while the tenant's entry
    /// exists. The writer is bound to that entry and reaches that one tenant's project and no other.
    pub fn deliver(
        &self,
        project: &str,
        instance: &str,
    ) -> Result<Option<Vec<String>>, NotWritten> {
        if project != arca_netproject::PROJECT
            && arca_kernel::ownership::tenant_of_project(project).is_none()
        {
            return Ok(None);
        }
        let (entry, record, object, retained) = {
            let kernel = self.kernel.lock().map_err(|_| {
                // A poisoned lock stays poisoned until Arca is restarted: asking again cannot help.
                NotWritten::fixed("the kernel could not be read; Arca has to be restarted")
            })?;
            let Some((entry, record)) = kernel
                .entries()
                .iter()
                .filter(|found| found.plugin.0 == "arca.service")
                .filter_map(|found| {
                    serde_json::from_value::<ServiceConfig>(found.config.clone())
                        .ok()
                        .map(|record| (found.id.clone(), record))
                })
                .find(|(id, record)| {
                    record.project == project
                        && record.instance == instance
                        && id.0 == record.entry_id()
                })
            else {
                return Ok(None);
            };
            if record.vm.is_none() || record.adopted || record.secret_refs.is_empty() {
                return Ok(None);
            }
            let object = kernel.service_object(&entry, &record.instance);
            // Where the instance is comes from Arca's own records, never from the call.
            if let Some(why) = misplaced(&record, kernel.entries(), object.as_ref()) {
                return Err(why);
            }
            let object = object.ok_or_else(|| NotWritten::fixed(NO_UUID))?;
            let retained = kernel
                .retained_entries()
                .map_err(|_| NotWritten::later("the kernel's retained set could not be read"))?;
            (entry, record, object, retained)
        };
        // Every value is read before anything is sent, so a secret that cannot be read leaves the guest as it was. It
        // is read by the provider's own rule: this entry's secret, in private directories, a private file of this
        // process's user, and the file that was checked is the file that is read.
        let secrets = self.cfg.state_dir.join("secrets");
        let mut files = Vec::new();
        for (key, reference) in &record.secret_refs {
            let name = SecretName::new(key).map_err(|_| {
                NotWritten::fixed(format!("the secret name {key} cannot be a file name"))
            })?;
            let value = read_entry_secret(&secrets, &entry, reference).map_err(|why| {
                NotWritten::fixed(format!(
                    "the stored secret {key} could not be read ({why}); set it again with service_secret_set"
                ))
            })?;
            files.push((key.clone(), name, value));
        }
        // The writer of the exec seam: bound to this entry, over the node's family and, for a service in a tenant,
        // that tenant's project alone.
        let target = ExecTarget {
            entry: entry.clone(),
            service: record,
            uuid: object.uuid.clone(),
            retained,
        };
        let writer = || {
            writer_for(&self.cfg, &target, REQUEST_BUDGET).map_err(|_| {
                NotWritten::fixed(
                    "its record's project is not one this node writes; nothing was sent",
                )
            })
        };
        let owned = writer()?.read_owned(&object).map_err(|error| match error {
            // The read was answered and the instance is not this entry's own: it was made again, or its mark is
            // gone. Asking again finds the same.
            arca_incus::IncusError::Refused(_) => NotWritten::fixed(
                "its instance is no longer the one Arca created; nothing was sent",
            ),
            arca_incus::IncusError::Status { code: 404, .. } => {
                NotWritten::fixed("its instance is gone; nothing was sent")
            }
            // The read did not get through (a timeout, a broken connection, an answer that could not be read).
            _ => NotWritten::later("its instance could not be read from Incus; nothing was sent"),
        })?;
        if owned.tombstone().is_some() {
            return Err(NotWritten::fixed("its instance is being removed"));
        }
        // The directory first: a new file is made before its mode is set, and only root enters the directory. It is
        // read before it is made, so a directory an earlier start made is left as it is and nothing depends on how
        // Incus answers a request to make one that exists. Anything else at the path is refused: a secret is never
        // written under a directory another user can enter.
        let until = Instant::now() + self.wait;
        loop {
            match writer()?.guest_secrets_dir(&owned) {
                Ok(GuestSecretsDir::Private) => break,
                Ok(GuestSecretsDir::NotPrivate) => {
                    return Err(NotWritten::fixed(format!(
                        "{GUEST_SECRETS_DIR} in the guest is not a directory that only root can enter (mode 0700, owner root), and Arca did not make it so; nothing was written"
                    )));
                }
                Ok(GuestSecretsDir::Missing) => {
                    if writer()?
                        .write_service_file(&owned, &ServiceFile::GuestSecretsDir, &[])
                        .is_ok()
                    {
                        break;
                    }
                }
                // The agent does not answer yet.
                Err(_) => {}
            }
            if Instant::now() + self.pause > until {
                return Err(NotWritten::later(format!(
                    "the guest took no file within {} seconds: its agent is not running yet, or its image has none",
                    self.wait.as_secs()
                )));
            }
            std::thread::sleep(self.pause);
        }
        let mut written = Vec::new();
        for (key, name, value) in files {
            writer()?
                .write_service_file(&owned, &ServiceFile::GuestSecret(name), value.as_bytes())
                .map_err(|_| {
                    NotWritten::later(format!(
                        "the secret {key} could not be written to {GUEST_SECRETS_DIR} in the guest"
                    ))
                })?;
            written.push(key);
        }
        Ok(Some(written))
    }
}

const NO_UUID: &str = "its instance has no journalled uuid";

/// Why the instance of `record` is not where Arca's own records put it, or `None` when it is. `entries` is the desired
/// state and `object` the instance the effect journal holds for the record's entry.
///
/// - The record's place is the node's own project, or the project of its own tenant: the project must carry that
///   tenant's id, and the tenant's entry must exist and name that same project. A record that names a tenant whose
///   entry is gone, or another project than its tenant's, is refused.
/// - The record's instance is the one its name and tenant give.
/// - The journal holds the instance, with its uuid, in the project the record names.
///
/// Each is a refusal that asking again cannot change.
fn misplaced(
    record: &ServiceConfig,
    entries: &[arca_kernel::desired::Entry],
    object: Option<&arca_kernel::ownership::ObjectRef>,
) -> Option<NotWritten> {
    let in_place = match &record.tenant {
        None => record.project == arca_netproject::PROJECT,
        Some(tenant) => {
            arca_kernel::ownership::tenant_of_project(&record.project) == Some(tenant.id.as_str())
                && entries.iter().any(|found| {
                    found.plugin.0 == "arca.tenant"
                        && found.id.0 == arca_kernel::ownership::tenant_entry(&tenant.id)
                        && found.config["project"].as_str() == Some(record.project.as_str())
                })
        }
    };
    if !in_place || record.instance != record.instance_name() {
        return Some(NotWritten::fixed(
            "its record does not name its own project and instance; nothing was sent",
        ));
    }
    let Some(object) = object.filter(|object| object.uuid.is_some()) else {
        return Some(NotWritten::fixed(NO_UUID));
    };
    if object.project != record.project {
        return Some(NotWritten::fixed(
            "the effect journal holds its instance in another project than its record names; nothing was sent",
        ));
    }
    None
}

/// The words a start's answer adds for the secrets it wrote, by name.
pub fn written_words(names: &[String]) -> String {
    format!(
        "Its {} ({}) {} written into the guest, in {GUEST_SECRETS_DIR}.",
        if names.len() == 1 {
            "secret".to_string()
        } else {
            format!("{} secrets", names.len())
        },
        names.join(", "),
        if names.len() == 1 { "was" } else { "were" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_kernel::ownership::{IncusKind, ObjectRef};
    use serde_json::{Value, json};

    const A: &str = "aaaaaaaa";
    const B: &str = "bbbbbbbb";

    /// The record of a virtual machine `box`: in tenant `tenant`'s project, or in the node's own.
    fn record(tenant: Option<&str>) -> ServiceConfig {
        let (project, instance) = match tenant {
            Some(id) => (format!("arcak-t-{id}"), format!("box-{id}")),
            None => (arca_netproject::PROJECT.to_string(), "box".to_string()),
        };
        let mut value = json!({
            "name": "box", "template": "keys", "instance": instance, "ipv4": "192.0.2.10", "ports": [],
            "created": 1, "source": {}, "pool": "pool1", "project": project, "bridge": "arcak-svcbr",
            "gateway": "192.0.2.1", "vm": { "rootSize": "20GiB" }
        });
        if let Some(id) = tenant {
            value["tenant"] = json!({ "id": id, "block": "192.0.2.248/29" });
        }
        serde_json::from_value(value).expect("a record")
    }

    fn tenant_entry(id: &str, project: &str) -> arca_kernel::desired::Entry {
        crate::handlers::service_entry(
            &format!("tenant-{id}"),
            "arca.tenant",
            json!({ "id": id, "project": project }),
        )
    }

    fn object(project: &str, name: &str, uuid: Option<&str>) -> ObjectRef {
        ObjectRef {
            kind: IncusKind::Instance,
            project: project.to_string(),
            parent: None,
            name: name.to_string(),
            uuid: uuid.map(str::to_string),
        }
    }

    fn why(found: Option<NotWritten>) -> Option<(String, bool)> {
        found.map(|not_written| (not_written.why, not_written.retry))
    }

    const RECORD: &str = "its record does not name its own project and instance; nothing was sent";
    const JOURNAL: &str = "the effect journal holds its instance in another project than its record names; nothing was sent";

    #[test]
    fn a_record_in_its_own_place_with_its_journalled_instance_is_served() {
        let a = [tenant_entry(A, "arcak-t-aaaaaaaa")];
        assert_eq!(
            why(misplaced(
                &record(Some(A)),
                &a,
                Some(&object("arcak-t-aaaaaaaa", "box-aaaaaaaa", Some("u1")))
            )),
            None
        );
        assert_eq!(
            why(misplaced(
                &record(None),
                &[],
                Some(&object("arcak-svc", "box", Some("u1")))
            )),
            None
        );
    }

    #[test]
    fn a_record_whose_tenant_entry_is_missing_or_names_another_project_is_refused() {
        let own = object("arcak-t-aaaaaaaa", "box-aaaaaaaa", Some("u1"));
        // No tenant entry at all: the tenant was removed, or never was.
        assert_eq!(
            why(misplaced(&record(Some(A)), &[], Some(&own))),
            Some((RECORD.to_string(), false))
        );
        // Another tenant's entry does not stand in for it.
        assert_eq!(
            why(misplaced(
                &record(Some(A)),
                &[tenant_entry(B, "arcak-t-bbbbbbbb")],
                Some(&own)
            )),
            Some((RECORD.to_string(), false))
        );
        // The tenant's entry names another project than the record's.
        assert_eq!(
            why(misplaced(
                &record(Some(A)),
                &[tenant_entry(A, "arcak-t-bbbbbbbb")],
                Some(&own)
            )),
            Some((RECORD.to_string(), false))
        );
        // An entry of another plugin under the tenant's id is no tenant entry.
        let mut other = tenant_entry(A, "arcak-t-aaaaaaaa");
        other.plugin.0 = "arca.service".into();
        assert_eq!(
            why(misplaced(&record(Some(A)), &[other], Some(&own))),
            Some((RECORD.to_string(), false))
        );
    }

    #[test]
    fn a_record_that_names_another_project_or_instance_than_its_own_is_refused() {
        let a = [
            tenant_entry(A, "arcak-t-aaaaaaaa"),
            tenant_entry(B, "arcak-t-bbbbbbbb"),
        ];
        let edit = |change: &dyn Fn(&mut Value)| -> ServiceConfig {
            let mut value = serde_json::to_value(record(Some(A))).expect("json");
            change(&mut value);
            serde_json::from_value(value).expect("a record")
        };
        for crossed in [
            // Tenant A's record in tenant B's project, in the node's own project, and outside both.
            edit(&|value| value["project"] = json!("arcak-t-bbbbbbbb")),
            edit(&|value| value["project"] = json!("arcak-svc")),
            edit(&|value| value["project"] = json!("default")),
            // Another instance than the one its name and tenant give.
            edit(&|value| value["instance"] = json!("box-bbbbbbbb")),
            edit(&|value| value["instance"] = json!("box")),
        ] {
            let journal = object(&crossed.project, &crossed.instance, Some("u1"));
            assert_eq!(
                why(misplaced(&crossed, &a, Some(&journal))),
                Some((RECORD.to_string(), false)),
                "{} {}",
                crossed.project,
                crossed.instance
            );
        }
        // A record of the node's own that names a tenant's project.
        let mut own = serde_json::to_value(record(None)).expect("json");
        own["project"] = json!("arcak-t-aaaaaaaa");
        let own: ServiceConfig = serde_json::from_value(own).expect("a record");
        assert_eq!(
            why(misplaced(
                &own,
                &a,
                Some(&object("arcak-t-aaaaaaaa", "box", Some("u1")))
            )),
            Some((RECORD.to_string(), false))
        );
    }

    #[test]
    fn the_journal_must_hold_the_instance_with_a_uuid_in_the_records_project() {
        let a = [
            tenant_entry(A, "arcak-t-aaaaaaaa"),
            tenant_entry(B, "arcak-t-bbbbbbbb"),
        ];
        // No journalled instance, or one without a uuid.
        assert_eq!(
            why(misplaced(&record(Some(A)), &a, None)),
            Some((NO_UUID.to_string(), false))
        );
        assert_eq!(
            why(misplaced(
                &record(Some(A)),
                &a,
                Some(&object("arcak-t-aaaaaaaa", "box-aaaaaaaa", None))
            )),
            Some((NO_UUID.to_string(), false))
        );
        // The journal holds the instance in another tenant's project, or in the node's own: nothing is sent there.
        for project in ["arcak-t-bbbbbbbb", "arcak-svc"] {
            assert_eq!(
                why(misplaced(
                    &record(Some(A)),
                    &a,
                    Some(&object(project, "box-aaaaaaaa", Some("u1")))
                )),
                Some((JOURNAL.to_string(), false)),
                "{project}"
            );
        }
        assert_eq!(
            why(misplaced(
                &record(None),
                &a,
                Some(&object("arcak-t-aaaaaaaa", "box", Some("u1")))
            )),
            Some((JOURNAL.to_string(), false))
        );
    }
}
