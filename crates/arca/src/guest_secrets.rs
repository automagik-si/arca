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
//! instance is this entry's own by its journalled uuid, and its outcome is a line of the instance action journal that
//! names the secrets and never a value.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arca_incus::exec::{GUEST_SECRETS_DIR, GuestSecretsDir, SecretName, ServiceFile};
use arca_incus::{ProjectFamily, UnixIncusWriter};
use arca_kernel::desired::EntryId;
use arca_kernel::ownership::{IncusKind, ObjectRef};
use arca_providers::incus::read_entry_secret;

use crate::config::NodeConfig;
use crate::kernel::Kernel;
use crate::service::ServiceConfig;

/// How long a start waits for the guest agent to take the first file. A new virtual machine boots before it answers.
const AGENT_WAIT: Duration = Duration::from_secs(120);
/// The pause between two tries while the agent does not answer.
const AGENT_PAUSE: Duration = Duration::from_secs(3);
/// The time one file request may take.
const REQUEST_BUDGET: Duration = Duration::from_secs(15);

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

    /// Writes the stored secrets of the virtual-machine service `instance` in `project` into its guest. `Ok(None)`
    /// when there is nothing to write: the instance is not a virtual-machine service Arca created, or none of its
    /// secrets is set. `Ok(Some(names))` names the secrets written. An error says in plain words what was not done,
    /// and never holds a value.
    pub fn deliver(&self, project: &str, instance: &str) -> Result<Option<Vec<String>>, String> {
        if project != arca_netproject::PROJECT {
            return Ok(None);
        }
        let entry = EntryId(format!("service-{instance}"));
        let (record, uuid, retained) = {
            let kernel = self
                .kernel
                .lock()
                .map_err(|_| "the kernel could not be read".to_string())?;
            let Some(record) = kernel
                .entries()
                .iter()
                .find(|found| found.id == entry && found.plugin.0 == "arca.service")
                .and_then(|found| {
                    serde_json::from_value::<ServiceConfig>(found.config.clone()).ok()
                })
            else {
                return Ok(None);
            };
            if record.vm.is_none() || record.adopted || record.secret_refs.is_empty() {
                return Ok(None);
            }
            (
                record,
                kernel.service_uuid(&entry, instance),
                kernel
                    .retained_entries()
                    .map_err(|_| "the kernel's retained set could not be read".to_string())?,
            )
        };
        if record.project != project || record.instance != instance || record.name != instance {
            return Err("its record names another instance".into());
        }
        let uuid = uuid.ok_or_else(|| "its instance has no journalled uuid".to_string())?;
        // Every value is read before anything is sent, so a secret that cannot be read leaves the guest as it was. It
        // is read by the provider's own rule: this entry's secret, in private directories, a private file of this
        // process's user, and the file that was checked is the file that is read.
        let secrets = self.cfg.state_dir.join("secrets");
        let mut files = Vec::new();
        for (key, reference) in &record.secret_refs {
            let name = SecretName::new(key)
                .map_err(|_| format!("the secret name {key} cannot be a file name"))?;
            let value = read_entry_secret(&secrets, &entry, reference)
                .map_err(|why| format!("the stored secret {key} could not be read ({why})"))?;
            files.push((key.clone(), name, value));
        }
        let family = ProjectFamily::new(
            self.cfg.kernel.projects.iter().map(String::as_str),
            self.cfg.kernel.droidd_projects.iter().map(String::as_str),
        )
        .map_err(|_| "the kernel's project family is invalid".to_string())?;
        let writer = || {
            UnixIncusWriter::new(
                self.cfg.incus.socket.clone(),
                entry.clone(),
                retained.clone(),
                family.clone(),
            )
            .with_total_budget(REQUEST_BUDGET)
        };
        let object = ObjectRef {
            kind: IncusKind::Instance,
            project: project.to_string(),
            parent: None,
            name: instance.to_string(),
            uuid: Some(uuid),
        };
        let owned = writer()
            .read_owned(&object)
            .map_err(|_| "its instance is no longer the one Arca created".to_string())?;
        if owned.tombstone().is_some() {
            return Err("its instance is being removed".into());
        }
        // The directory first: a new file is made before its mode is set, and only root enters the directory. It is
        // read before it is made, so a directory an earlier start made is left as it is and nothing depends on how
        // Incus answers a request to make one that exists. Anything else at the path is refused: a secret is never
        // written under a directory another user can enter.
        let until = Instant::now() + self.wait;
        loop {
            match writer().guest_secrets_dir(&owned) {
                Ok(GuestSecretsDir::Private) => break,
                Ok(GuestSecretsDir::NotPrivate) => {
                    return Err(format!(
                        "{GUEST_SECRETS_DIR} in the guest is not a directory that only root can enter (mode 0700, owner root), and Arca did not make it so; nothing was written"
                    ));
                }
                Ok(GuestSecretsDir::Missing) => {
                    if writer()
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
                return Err(format!(
                    "the guest took no file within {} seconds: its agent is not running yet, or its image has none",
                    self.wait.as_secs()
                ));
            }
            std::thread::sleep(self.pause);
        }
        let mut written = Vec::new();
        for (key, name, value) in files {
            writer()
                .write_service_file(&owned, &ServiceFile::GuestSecret(name), value.as_bytes())
                .map_err(|_| {
                    format!(
                        "the secret {key} could not be written to {GUEST_SECRETS_DIR} in the guest"
                    )
                })?;
            written.push(key);
        }
        Ok(Some(written))
    }
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
