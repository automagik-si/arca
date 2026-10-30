//! The node config file: one JSON document in camelCase. Every key is named and every unknown key is refused. The rules
//! the kernel depends on are checked here, before any command reads Incus: the tailnet address, the project names the
//! kernel may never write in, and the scratch prefix.

use std::fmt;
use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use arca_mcp::tailnet_addr_ok;
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
    /// The approval host, when one is set. Nothing in this build reads it.
    #[serde(default)]
    pub approval_host: Option<String>,
}

/// Where Incus listens, and the storage pools the sampler reads.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IncusConfig {
    pub socket: PathBuf,
    #[serde(default)]
    pub pools: Vec<String>,
}

/// The two listeners: the unix socket, and one tailnet address when one is named.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ListenConfig {
    pub unix: PathBuf,
    #[serde(default)]
    pub tailnet: Option<SocketAddr>,
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
    /// A kernel project is a name the kernel may never write in.
    ProjectForbidden(String),
    /// A scratch run names a project outside the scratch prefix.
    ScratchPrefix(String),
    /// Kernel projects are named, and no droidd project is listed to exclude.
    DroiddProjectsEmpty,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, why } => write!(f, "cannot read the config file {path}: {why}"),
            Self::Parse(why) => write!(f, "the config file is refused: {why}"),
            Self::TailnetAddr(why) => write!(f, "listen.tailnet is refused: {why}"),
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
    let kernel = &config.kernel;
    for name in &kernel.projects {
        if FORBIDDEN_PROJECTS.contains(&name.as_str()) || kernel.droidd_projects.contains(name) {
            return Err(ConfigError::ProjectForbidden(name.clone()));
        }
    }
    if !kernel.projects.is_empty() && kernel.droidd_projects.is_empty() {
        return Err(ConfigError::DroiddProjectsEmpty);
    }
    if kernel.scratch {
        for name in &kernel.projects {
            if !name.starts_with(SCRATCH_PREFIX) {
                return Err(ConfigError::ScratchPrefix(name.clone()));
            }
        }
    }
    Ok(())
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
