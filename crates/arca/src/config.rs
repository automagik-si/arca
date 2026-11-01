//! The node config file: one JSON document in camelCase. Every key is named and every unknown key is refused. The rules
//! the kernel depends on are checked here, before any command reads Incus: the tailnet and proxy addresses, the project
//! names the kernel may never write in, the scratch prefix, and the adoption allowlist.

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

// The adoption types come from the writer, whose feature reaches this crate through `arca-providers`, the one crate that
// enables it (docs/OWNERSHIP.md, Rule 28).
pub use arca_incus::adopt::AdoptTarget;
use arca_mcp::{proxy_addr_ok, tailnet_addr_ok};
use serde::Deserialize;
use serde_json::error::Category;

/// The projects no config may name as a kernel project: the host's default project and the bench.
const FORBIDDEN_PROJECTS: [&str; 2] = ["bench", "default"];

/// The prefix every project of a scratch run carries.
const SCRATCH_PREFIX: &str = "arcak-scratch-";

/// The whole node config.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NodeConfig {
    /// The directory of the node's own state: the node key lives in it.
    pub state_dir: PathBuf,
    pub incus: IncusConfig,
    /// The names of the phones that are this node's identity phones. The watch flags an instance with one of these names.
    #[serde(default)]
    pub identity_phones: Vec<String>,
    pub listen: ListenConfig,
    pub tokens: TokenConfig,
    pub kernel: KernelConfig,
    /// The host name every owner approval statement names. When it is not set, the system's host name is used.
    #[serde(default)]
    pub approval_host: Option<String>,
}

/// Where Incus listens, and which storage pools the sampler reports. The pools come from Incus's own pool list. `pools` is an
/// allow-filter over that list: an absent key or `[]` admits every pool Incus lists, and a name Incus does not list has no row.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IncusConfig {
    pub socket: PathBuf,
    #[serde(default)]
    pub pools: Vec<String>,
}

/// The listeners: the unix socket, one tailnet address when one is named, and one proxy address when one is named.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ListenConfig {
    pub unix: PathBuf,
    #[serde(default)]
    pub tailnet: Option<SocketAddr>,
    #[serde(default)]
    pub proxy: Option<ProxyListen>,
}

/// The proxy listener: one fixed address outside the tailnet ranges, served to the single peer address `peer`, which is a
/// reverse proxy on the same host. Any other peer is closed with no bytes sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProxyListen {
    pub addr: SocketAddr,
    pub peer: IpAddr,
}

/// The directory of the hash files the server reads.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TokenConfig {
    pub hash_dir: PathBuf,
}

/// The kernel's project family and the projects that belong to droidd. `droidd_projects` is required; it may be empty only
/// while `projects` is.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KernelConfig {
    #[serde(default)]
    pub projects: Vec<String>,
    #[serde(default)]
    pub scratch: bool,
    pub droidd_projects: Vec<String>,
    #[serde(default)]
    pub reserved_cidrs: Vec<String>,
    /// The containers `arca adopt` may take from the default project, each with its adoption project. A config that lists any
    /// is for `arca adopt` alone: `serve` refuses it.
    #[serde(default)]
    pub adopt: Vec<AdoptTarget>,
    /// The bridges in the default project an adoption project's grant may name, though they carry no kernel mark. Nothing
    /// writes them.
    #[serde(default)]
    pub attach_networks: Vec<String>,
}

/// Why a config is refused. No variant carries a config value, except a project name the rules themselves name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// The file cannot be read. `why` is a fixed reason.
    Io { path: String, why: String },
    /// The file does not parse, or a key is unknown or missing. The text is serde's naming of the key, with values removed.
    Parse(String),
    /// The tailnet address fails the tailnet rules.
    TailnetAddr(String),
    /// The proxy address fails the proxy rules, or its peer is not one address.
    ProxyAddr(String),
    /// A kernel project is a name the kernel may never write in.
    ProjectForbidden(String),
    /// A scratch run names a project outside the scratch prefix.
    ScratchPrefix(String),
    /// The approval host is not a host name. The value is left out of the message.
    ApprovalHost,
    /// Kernel projects are named, and no droidd project is listed to exclude.
    DroiddProjectsEmpty,
    /// The adoption allowlist or its attach networks break a rule. The text names the rule, never a value.
    Adopt(&'static str),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, why } => write!(f, "cannot read the config file {path}: {why}"),
            Self::Parse(why) => write!(f, "the config file is refused: {why}"),
            Self::TailnetAddr(why) => write!(f, "listen.tailnet is refused: {why}"),
            Self::ProxyAddr(why) => write!(f, "listen.proxy is refused: {why}"),
            Self::ProjectForbidden(name) => write!(
                f,
                "kernel project {name} is reserved: it is the host's default or bench project, or a droidd project"
            ),
            Self::ScratchPrefix(name) => write!(
                f,
                "a scratch run takes only projects starting {SCRATCH_PREFIX}, and {name} does not"
            ),
            Self::DroiddProjectsEmpty => write!(
                f,
                "kernel.droiddProjects must list droidd's projects when kernel.projects is not empty"
            ),
            Self::Adopt(why) => write!(f, "kernel.adopt is refused: {why}"),
            Self::ApprovalHost => write!(
                f,
                "approvalHost is refused: it must be a host name of letters, digits, dots and hyphens"
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Reads and checks the config at `path`.
pub fn load(path: &Path) -> Result<NodeConfig, ConfigError> {
    let text = fs::read_to_string(path).map_err(|e| ConfigError::Io {
        path: path.display().to_string(),
        why: io_reason(&e),
    })?;
    let config: NodeConfig =
        serde_json::from_str(&text).map_err(|e| ConfigError::Parse(parse_reason(&e)))?;
    check(&config)?;
    Ok(config)
}

/// The rules a parsed config must meet.
pub fn check(config: &NodeConfig) -> Result<(), ConfigError> {
    if let Some(addr) = config.listen.tailnet {
        tailnet_addr_ok(&addr).map_err(ConfigError::TailnetAddr)?;
    }
    if let Some(proxy) = &config.listen.proxy {
        proxy_addr_ok(&proxy.addr).map_err(ConfigError::ProxyAddr)?;
        if proxy.peer.to_canonical().is_unspecified() {
            return Err(ConfigError::ProxyAddr(
                "the peer must be one address, not a wildcard".to_string(),
            ));
        }
    }
    let kernel = &config.kernel;
    for name in &kernel.projects {
        if FORBIDDEN_PROJECTS.contains(&name.as_str()) || kernel.droidd_projects.contains(name) {
            return Err(ConfigError::ProjectForbidden(name.clone()));
        }
    }
    if !kernel.projects.is_empty() && kernel.droidd_projects.is_empty() {
        return Err(ConfigError::DroiddProjectsEmpty);
    }
    check_adopt(kernel)?;
    if kernel.scratch {
        for name in &kernel.projects {
            if !name.starts_with(SCRATCH_PREFIX) {
                return Err(ConfigError::ScratchPrefix(name.clone()));
            }
        }
    }
    if let Some(host) = &config.approval_host
        && !host_name_ok(host)
    {
        return Err(ConfigError::ApprovalHost);
    }
    Ok(())
}

/// The adoption rules: plain names, each adoption project starting `arcak-adopt-` and listed in `kernel.projects`, no
/// instance or project listed twice, plain attach networks, and no scratch run.
fn check_adopt(kernel: &KernelConfig) -> Result<(), ConfigError> {
    if !kernel.adopt.is_empty() && kernel.scratch {
        return Err(ConfigError::Adopt(
            "kernel.scratch must be false when kernel.adopt is set",
        ));
    }
    let (mut instances, mut projects) = (BTreeSet::new(), BTreeSet::new());
    for target in &kernel.adopt {
        target.check().map_err(ConfigError::Adopt)?;
        if !kernel.projects.contains(&target.to) {
            return Err(ConfigError::Adopt(
                "each adoption project must be listed in kernel.projects",
            ));
        }
        if !instances.insert(&target.instance) || !projects.insert(&target.to) {
            return Err(ConfigError::Adopt(
                "an instance or an adoption project is listed twice",
            ));
        }
    }
    let plain = |name: &String| {
        (1..=63).contains(&name.len())
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    };
    if !kernel.attach_networks.iter().all(plain) {
        return Err(ConfigError::Adopt("an attach network is not a plain name"));
    }
    Ok(())
}

/// Whether `host` is a host name the approval statement may carry: one letter or digit to start, then up to 252 letters,
/// digits, dots or hyphens. An empty name is refused.
pub fn host_name_ok(host: &str) -> bool {
    let bytes = host.as_bytes();
    let Some((first, rest)) = bytes.split_first() else {
        return false;
    };
    first.is_ascii_alphanumeric()
        && rest.len() <= 252
        && rest
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'.' || *b == b'-')
}

/// A fixed reason for a read failure. The operating system's text is left out.
fn io_reason(error: &io::Error) -> String {
    match error.kind() {
        io::ErrorKind::NotFound => "no such file".to_string(),
        io::ErrorKind::PermissionDenied => "permission denied".to_string(),
        _ => "the file could not be read".to_string(),
    }
}

/// serde's account of a parse failure, with the rejected value removed. Only the two key errors name anything, and they
/// name keys; every other data error is a fixed phrase.
fn parse_reason(error: &serde_json::Error) -> String {
    let position = format!("line {}, column {}", error.line(), error.column());
    let message = match error.classify() {
        Category::Data => {
            let text = error.to_string();
            let head = text
                .rsplit_once(" at line ")
                .map_or(text.as_str(), |(head, _)| head);
            if head.starts_with("unknown field") || head.starts_with("missing field") {
                head.to_string()
            } else {
                "a value has the wrong type or form".to_string()
            }
        }
        Category::Syntax | Category::Eof => "the file is not valid JSON".to_string(),
        Category::Io => "the file could not be read".to_string(),
    };
    format!("{message} ({position})")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(approval_host: Option<&str>) -> NodeConfig {
        let mut value = serde_json::json!({
            "stateDir": "/nonexistent/state",
            "incus": { "socket": "/nonexistent/incus.sock" },
            "listen": { "unix": "/nonexistent/arca.sock" },
            "tokens": { "hashDir": "/nonexistent/tokens" },
            "kernel": { "droiddProjects": ["droid"] }
        });
        if let Some(host) = approval_host {
            value["approvalHost"] = serde_json::json!(host);
        }
        serde_json::from_value(value).expect("the fixture config parses")
    }

    #[test]
    fn an_approval_host_that_is_not_a_host_name_is_refused() {
        for bad in ["under_score", "-leading", "has space", "", "node/1"] {
            let refused = check(&config_with(Some(bad)));
            assert_eq!(
                refused,
                Err(ConfigError::ApprovalHost),
                "{bad:?} is not a host name"
            );
        }
    }

    fn adopting(kernel: serde_json::Value) -> Result<(), ConfigError> {
        let mut value = serde_json::json!({
            "stateDir": "/nonexistent/state",
            "incus": { "socket": "/nonexistent/incus.sock" },
            "listen": { "unix": "/nonexistent/arca.sock" },
            "tokens": { "hashDir": "/nonexistent/tokens" },
            "kernel": { "droiddProjects": ["droid"], "projects": ["arcak-adopt-svc1"],
                "adopt": [{ "instance": "svc1", "to": "arcak-adopt-svc1" }], "attachNetworks": ["br1"] }
        });
        if let serde_json::Value::Object(extra) = kernel {
            value["kernel"]
                .as_object_mut()
                .expect("kernel")
                .extend(extra);
        }
        let config: NodeConfig =
            serde_json::from_value(value).map_err(|e| ConfigError::Parse(e.to_string()))?;
        check(&config)
    }

    #[test]
    fn an_adoption_allowlist_is_refused_unless_every_rule_holds() {
        use serde_json::json;
        assert_eq!(adopting(json!({})), Ok(()));
        let refused = [
            json!({ "scratch": true }),
            json!({ "adopt": [{ "instance": "svc1", "to": "arcak-scratch-svc1" }], "projects": ["arcak-scratch-svc1"] }),
            json!({ "adopt": [{ "instance": "svc1", "to": "arcak-adopt-other" }] }),
            json!({ "adopt": [{ "instance": "Svc 1", "to": "arcak-adopt-svc1" }] }),
            json!({ "adopt": [{ "instance": "svc1", "to": "arcak-adopt-svc1" }, { "instance": "svc1", "to": "arcak-adopt-svc1" }] }),
            json!({ "attachNetworks": ["br1,br2"] }),
        ];
        for kernel in refused {
            assert!(
                matches!(
                    adopting(kernel.clone()),
                    Err(ConfigError::Adopt(_) | ConfigError::ScratchPrefix(_))
                ),
                "{kernel}"
            );
        }
        assert!(
            adopting(
                json!({ "adopt": [{ "instance": "svc1", "to": "arcak-adopt-svc1", "x": 1 }] })
            )
            .is_err()
        );
    }

    #[test]
    fn an_approval_host_that_is_a_host_name_or_none_is_accepted() {
        assert_eq!(check(&config_with(Some("node1.example"))), Ok(()));
        assert_eq!(check(&config_with(None)), Ok(()));
    }
}
