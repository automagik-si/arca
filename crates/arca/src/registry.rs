//! Registry credentials: what a registry asks for before it gives an image.
//!
//! Arca reads no vault. Whoever holds a registry's credential sets it into the node by the registry's address
//! (`registry_secret_set`), and from then on every image the node's own projects fetch from that registry is fetched
//! with it. The user name and the password are files of the node's secret store, under a name no entry has
//! (`arca_providers::incus::registry_owner`); no record, proposal, card or journal line ever holds them. They leave
//! the store in one place: the provider reads them at the moment of a create and the writer puts them into the
//! address it sends to Incus, which is how Incus takes a registry's credential.
//!
//! Incus keeps that address in the record of the image it fetched, for as long as it keeps the image. So the
//! credential set here must be one that can only read, and only what this node pulls.

use std::path::PathBuf;
use std::sync::Arc;

use arca_api::Scope;
use arca_mcp::{CallError, Caller, ToolHandler};
use serde_json::{Value, json};

use crate::config::NodeConfig;

/// The three registry verbs.
pub const REGISTRY_VERBS: [&str; 3] = [
    "registry_secret_set",
    "registry_list",
    "registry_secret_remove",
];

/// The directory, in the state directory, that names the registries a credential is set for: one file for each,
/// named as its secrets are, holding the registry's address. It holds no secret.
const REGISTRIES_DIR: &str = "registries";

pub fn registry_handlers(cfg: Arc<NodeConfig>) -> Vec<(&'static str, Arc<dyn ToolHandler>)> {
    REGISTRY_VERBS
        .into_iter()
        .map(|verb| {
            (
                verb,
                Arc::new(RegistryHandler {
                    verb,
                    cfg: cfg.clone(),
                }) as Arc<dyn ToolHandler>,
            )
        })
        .collect()
}

struct RegistryHandler {
    verb: &'static str,
    cfg: Arc<NodeConfig>,
}

fn refused(message: impl Into<String>) -> CallError {
    CallError::Refused {
        code: "refused".into(),
        message: message.into(),
    }
}

/// Whether a registry credential was ever set on this node. A node where none was sends Incus nothing it did not
/// send before there were any: no image of its store can hold one.
pub fn ever_set(cfg: &NodeConfig) -> bool {
    cfg.state_dir.join(REGISTRIES_DIR).is_dir()
}

/// Whether this node holds a credential for exactly the registry `server` (`https://<host>`).
pub fn credential_set(cfg: &NodeConfig, server: &str) -> bool {
    credential(cfg, server).is_some()
}

/// The credential this node holds for exactly the registry `server`, read from the secret store at this moment by
/// the provider's own rules: a private directory, files of mode 0600 owned by this user, no link, and the address
/// stored with the credential equal to `server`.
pub fn credential(cfg: &NodeConfig, server: &str) -> Option<arca_incus::RegistryCredential> {
    arca_providers::incus::registry_credential(&cfg.state_dir.join("secrets"), server)
}

/// The `Registry` row of a create's card, in the owner's words: the image is fetched with a credential, named by
/// the registry it is for and never shown, and where that credential then rests.
pub fn card_words(server: &str) -> String {
    format!(
        "{server}: the image is fetched with the credential set for this registry on this node, which this card names by the registry and never shows. Incus keeps that credential with the image for as long as the image is kept, and whoever may use Incus on this machine can read it there"
    )
}

/// What a create must say about its image's registry, and why it is refused when it does not. A create whose image
/// is on a registry this node holds a credential for states `registry`, that registry's address: a proposal fills
/// it in, so the owner's card says it and the approval binds it. A create that states one this node holds no
/// credential for is refused as well: the credential was removed since the card was shown. And one that states it
/// in a call of its own, with no approved proposal behind it (`preapproved` false), is refused like one that says
/// nothing: the owner saw no card.
pub fn stated(
    cfg: &NodeConfig,
    server: Option<&str>,
    stated: Option<&Value>,
    preapproved: bool,
) -> Result<(), CallError> {
    let held = server.filter(|server| credential_set(cfg, server));
    match (held, stated.and_then(Value::as_str)) {
        (None, None) => Ok(()),
        // Saying it is not enough: only a call that runs an approved proposal has shown the owner the card.
        (Some(server), Some(said)) if said == server && preapproved => Ok(()),
        (Some(server), _) => Err(CallError::Refused {
            code: "NotApproved".into(),
            message: format!(
                "this node holds a credential for the registry {server}, and an image fetched with it leaves that credential with Incus: the create runs only from a proposal whose card says so. Propose it with action_propose"
            ),
        }),
        (None, Some(said)) => Err(refused(format!(
            "registry: this node holds no credential for {said} now, so the card that was approved no longer says what would happen. Propose the create again"
        ))),
    }
}

/// The registry of an image named as `<host>/<repository>`, as an address.
pub fn server_of(image: &str) -> Option<String> {
    let host = image.split('/').next().filter(|host| !host.is_empty())?;
    Some(format!("https://{}", host.to_ascii_lowercase()))
}

impl RegistryHandler {
    fn secrets(&self, owner: &str) -> PathBuf {
        self.cfg.state_dir.join("secrets").join(owner)
    }

    fn listed(&self, owner: &str) -> PathBuf {
        self.cfg.state_dir.join(REGISTRIES_DIR).join(owner)
    }

    /// The registry a call names, as its address and the name its secrets are kept under.
    fn server(&self, args: &Value) -> Result<(String, String), CallError> {
        let server = args["server"]
            .as_str()
            .map(str::to_ascii_lowercase)
            .ok_or_else(|| CallError::Invalid("server: expected a registry address".into()))?;
        let owner = arca_providers::incus::registry_owner(&server).ok_or_else(|| {
            CallError::Invalid(
                "server: expected a registry as https://<host>, with an optional :<port> and nothing after it".into(),
            )
        })?;
        Ok((server, owner))
    }
}

impl ToolHandler for RegistryHandler {
    fn call(&self, args: Value, caller: &Caller) -> Result<Value, CallError> {
        let allowed: &[&str] = match self.verb {
            "registry_secret_set" => &["server", "username", "password"],
            "registry_secret_remove" => &["server"],
            _ => &[],
        };
        let object = args
            .as_object()
            .ok_or_else(|| CallError::Invalid("arguments: expected an object".into()))?;
        if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
            return Err(CallError::Invalid(arca_api::unknown_field(
                "", key, allowed,
            )));
        }
        if self.verb != "registry_list" && caller.scope < Scope::Doctor {
            return Err(CallError::Forbidden {
                verb: self.verb.into(),
            });
        }
        match self.verb {
            "registry_list" => {
                let mut registries: Vec<Value> =
                    std::fs::read_dir(self.cfg.state_dir.join(REGISTRIES_DIR))
                        .into_iter()
                        .flatten()
                        .flatten()
                        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
                        .map(|server| server.trim().to_string())
                        .filter(|server| credential_set(&self.cfg, server))
                        .map(|server| json!({ "server": server }))
                        .collect();
                registries.sort_by(|a, b| a["server"].as_str().cmp(&b["server"].as_str()));
                Ok(json!({ "registries": registries }))
            }
            "registry_secret_set" => {
                let (server, owner) = self.server(&args)?;
                let username = args["username"]
                    .as_str()
                    .filter(|text| {
                        (1..=255).contains(&text.chars().count())
                            && !text.chars().any(|c| c.is_control() || c.is_whitespace())
                    })
                    .ok_or_else(|| {
                        CallError::Invalid(
                            "username: 1 to 255 characters, with no space and no control character"
                                .into(),
                        )
                    })?;
                // The password stands in an address, and only what needs no rewriting there is known to reach the
                // registry as it is: a registry's token is made of such characters.
                let password = args["password"]
                    .as_str()
                    .filter(|text| {
                        (8..=4096).contains(&text.len())
                            && text
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b))
                    })
                    .ok_or_else(|| {
                        CallError::Invalid(
                            "password: 8 to 4096 of letters, digits and - . _ ~ (a registry's token is such a word; no other character is taken in this build)".into(),
                        )
                    })?;
                let dir = self.secrets(&owner);
                let _ = std::fs::remove_dir_all(&dir);
                // The address is stored with the credential: it is given to that address and to no other.
                let stored = crate::service::write_secret(&dir.join("server"), &server)
                    .and_then(|()| crate::service::write_secret(&dir.join("username"), username))
                    .and_then(|()| crate::service::write_secret(&dir.join("password"), password))
                    .and_then(|()| {
                        let _ = std::fs::remove_file(self.listed(&owner));
                        crate::service::write_secret(&self.listed(&owner), &server)
                    });
                if stored.is_err() {
                    let _ = std::fs::remove_dir_all(&dir);
                    return Err(refused(
                        "the registry's credential could not be stored in the node's secret store; nothing is set",
                    ));
                }
                Ok(json!({ "ok": true, "server": server }))
            }
            "registry_secret_remove" => {
                let (server, owner) = self.server(&args)?;
                if !credential_set(&self.cfg, &server) {
                    return Err(CallError::Refused {
                        code: "not_found".into(),
                        message: format!("registry {server}: no credential is set for it"),
                    });
                }
                let _ = std::fs::remove_dir_all(self.secrets(&owner));
                let _ = std::fs::remove_file(self.listed(&owner));
                Ok(json!({ "removed": true, "server": server }))
            }
            _ => Err(refused("unknown registry verb")),
        }
    }
}
