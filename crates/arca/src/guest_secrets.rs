//! The secrets of a virtual-machine service, written into its guest at each start.
//!
//! A container's secret reaches it as an environment value in its Incus config. A virtual machine's never does: its
//! Incus config, the desired state and the effect journal hold a reference at most. The value is read from the secret
//! store (the private files `service_secret_set` writes under `<stateDir>/secrets`) and sent as the body of an Incus
//! file request, which Incus hands to the guest agent: `/etc/arca-secrets` (root's, mode 0700) and one file per secret
//! in it, named by the secret (root's, mode 0600). The files are on the guest's own disk, so a guest that restarts by
//! itself still has them; `instance_start` and `instance_restart` write them again, which is how a new value arrives.
//!
//! Nothing here is an effect of a plugin: the call is made after the instance action, for a service record whose
//! instance is this entry's own by its journalled uuid, and its outcome is a line of the instance action journal that
//! names the secrets and never a value.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arca_incus::exec::{GUEST_SECRETS_DIR, SecretName, ServiceFile};
use arca_incus::{ProjectFamily, UnixIncusWriter};
use arca_kernel::desired::EntryId;
use arca_kernel::ownership::{IncusKind, ObjectRef};
use arca_providers::incus::SECRET_PREFIX;

use crate::config::NodeConfig;
use crate::kernel::Kernel;
use crate::service::ServiceConfig;

/// How long a start waits for the guest agent to take the first file. A new virtual machine boots before it answers.
const AGENT_WAIT: Duration = Duration::from_secs(120);
/// The pause between two tries while the agent does not answer.
const AGENT_PAUSE: Duration = Duration::from_secs(3);
/// The time one file request may take.
const REQUEST_BUDGET: Duration = Duration::from_secs(15);
/// The largest secret file read from the store, in bytes.
const SECRET_MAX_BYTES: u64 = 64 * 1024;

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
        // Every value is read before anything is sent, so a secret that cannot be read leaves the guest as it was.
        let mut files = Vec::new();
        for (key, reference) in &record.secret_refs {
            let name = SecretName::new(key)
                .map_err(|_| format!("the secret name {key} cannot be a file name"))?;
            let value = read_secret(&self.cfg.state_dir.join("secrets"), reference)
                .ok_or_else(|| format!("the stored secret {key} could not be read"))?;
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
        // The directory first: a new file is made before its mode is set, and only root enters the directory.
        let until = Instant::now() + self.wait;
        loop {
            if writer()
                .write_service_file(&owned, &ServiceFile::GuestSecretsDir, &[])
                .is_ok()
            {
                break;
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

/// The value a reference names in the secret store: a private regular file, never a link, under private directories.
fn read_secret(secrets: &Path, reference: &str) -> Option<String> {
    let name = reference.strip_prefix(SECRET_PREFIX)?;
    let mut path = secrets.to_path_buf();
    for part in name.split('/') {
        if part.is_empty()
            || !part
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        {
            return None;
        }
        path.push(part);
    }
    let meta = std::fs::symlink_metadata(&path).ok()?;
    if !meta.is_file() || meta.permissions().mode() & 0o077 != 0 || meta.len() > SECRET_MAX_BYTES {
        return None;
    }
    std::fs::read_to_string(path).ok()
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
