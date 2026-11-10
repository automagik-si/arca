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

/// The unix socket of the local Incus daemon on a host that installs Incus from its package. Any other socket path is a
/// proxy until the config says otherwise, because a proxy can reach another machine.
pub const LOCAL_INCUS_SOCKET: &str = "/var/lib/incus/unix.socket";

/// The local daemon's unprivileged unix socket, when Incus offers one. It reaches the same local daemon.
const LOCAL_INCUS_USER_SOCKET: &str = "/var/lib/incus/unix.socket.user";

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
    /// The page origin the owner approval page is served at: `https://` plus a host name and at most a non-443 port.
    /// It is the WebAuthn rp id and origin; it is not the host an approval statement names.
    #[serde(default)]
    pub approval_origin: Option<String>,
    /// The loopback DSH origin the remote canvas proxies: `http://127.0.0.1:3081` by default. A literal loopback IP and port
    /// are required; a host name, a non-loopback address, a path or https is refused before anything listens.
    #[serde(default = "default_canvas_origin")]
    pub canvas_origin: String,
    /// This process's serve DNS name and the owner's tailnet label, used by service publication.
    #[serde(default)]
    pub serve_host: Option<String>,
    #[serde(default)]
    pub tailnet: Option<String>,
    /// The node link endpoint and opaque ALPN routes.
    #[serde(default)]
    pub link: Option<arca_link::Config>,
    /// Multi-host approvals on one machine: the other kernels' passkeys files this kernel also trusts, and the linked hosts
    /// whose proposals this kernel's approval page lists and relays. Absent, a kernel trusts only its own owner keys and its
    /// page serves only its own proposals.
    #[serde(default)]
    pub approval: Option<ApprovalConfig>,
}

/// The `approval` key. Every list may be empty.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovalConfig {
    /// Other kernels' owner passkeys files, read read-only at each approval check. A file that is missing or cannot be trusted
    /// adds no key.
    #[serde(default)]
    pub trust_owner_keys_from: Vec<PathBuf>,
    /// The page origins a passkey from those files may answer from. Required when `trustOwnerKeysFrom` names a file.
    #[serde(default)]
    pub origins: Vec<String>,
    /// The kernels whose waiting proposals this kernel's approval page lists and relays approvals to: on this machine, or on
    /// another machine whose MCP socket is forwarded to a path here.
    #[serde(default)]
    pub linked_hosts: Vec<LinkedHost>,
    /// Another machine's page host whose owner passkeys this kernel keeps in sync (`peer_keys_sync`) and trusts for action
    /// approvals. Absent, `peer_keys_sync` is not served and no peer passkey is trusted.
    #[serde(default)]
    pub peer_keys: Option<PeerKeysConfig>,
}

/// The `approval.peerKeys` key: the page host's name, as its own kernel names itself (its approval host), and the page
/// origins its passkeys may answer from.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PeerKeysConfig {
    pub from: String,
    pub origins: Vec<String>,
}

/// One linked kernel: the name the canvas knows it by, its MCP unix socket, and the file holding the token the page relays
/// with (scope `doctor`, and ideally limited to `plan_list`, `action_approve` and `plan_decline`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LinkedHost {
    pub name: String,
    pub socket: PathBuf,
    pub token_file: PathBuf,
}

/// Where Incus listens, and which storage pools the sampler reports. The pools come from Incus's own pool list. `pools` is an
/// allow-filter over that list: an absent key or `[]` admits every pool Incus lists, and a name Incus does not list has no row.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IncusConfig {
    pub socket: PathBuf,
    #[serde(default)]
    pub pools: Vec<String>,
    /// Whether `socket` reaches the local Incus daemon. When absent, the runtime check compares the Incus server name with
    /// the local host name; the socket path is only the fallback. An explicit value wins.
    #[serde(default)]
    pub local: Option<bool>,
}

impl IncusConfig {
    /// The socket-path rule: whether the standard local daemon socket is configured. It is the fallback for the runtime
    /// choice of [`crate::handlers::new_sampler`], never the whole answer: a shim path reaches the local daemon too, and a
    /// host socket mounted at the standard path inside a container does not make the container's `/proc` the host's.
    pub fn socket_is_local(&self) -> bool {
        self.socket == Path::new(LOCAL_INCUS_SOCKET)
            || self.socket == Path::new(LOCAL_INCUS_USER_SOCKET)
    }
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
    #[serde(default)]
    pub service_cidr: Option<String>,
    /// The digest-pinned database image a proposed `postgres_create` uses. A proposal cannot choose another image; when
    /// this is absent, a proposed database create is refused.
    #[serde(default)]
    pub postgres_image: Option<String>,
    /// The containers `arca adopt` may take from the default project, each with its adoption project. A config that lists any
    /// is for `arca adopt` alone: `serve` refuses it.
    #[serde(default)]
    pub adopt: Vec<AdoptTarget>,
    /// The bridges in the default project an adoption project's grant may name, though they carry no kernel mark. Nothing
    /// writes them.
    #[serde(default)]
    pub attach_networks: Vec<String>,
    /// The host bridges, which already exist, that a virtual machine's NIC may name. Without `serviceCidr` the node is
    /// bridged: it runs virtual machines only and Arca creates no Incus network on it. With `serviceCidr` it is mixed.
    #[serde(default)]
    pub bridged: Option<BridgedConfig>,
    /// The virtual-machine caps of a node with `serviceCidr` and no `bridged`. A node with `bridged` states them there.
    #[serde(default)]
    pub vm_limits: Option<BridgedLimits>,
    /// The services another manager created that `service_import` may take over where they stand. Each entry names the
    /// service, the project it is in (one of `droiddProjects`) and that project's service subnet. Empty by default: nothing
    /// is imported, and no write reaches another manager's project.
    #[serde(default)]
    pub import_services: Vec<ImportService>,
}

/// One `kernel.importServices` entry: a service another manager created, by name, the project its instance is in, and the
/// IPv4 /24 its address must be inside.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImportService {
    pub name: String,
    pub project: String,
    pub cidr: String,
}

impl KernelConfig {
    /// The import allowlist entry for the service `name` in `project`, when one is listed.
    pub fn import_service(&self, name: &str, project: &str) -> Option<&ImportService> {
        self.import_services
            .iter()
            .find(|entry| entry.name == name && entry.project == project)
    }

    /// The caps on all of Arca's virtual machines on this node, or `None` on a node that may create none.
    pub fn vm_caps(&self) -> Option<&BridgedLimits> {
        match &self.bridged {
            Some(bridged) => Some(&bridged.limits),
            None => self.vm_limits.as_ref(),
        }
    }
}

/// The `kernel.bridged` key: the host bridges a virtual machine's NIC may name, and the storage pool its root disk is on.
/// Nothing writes the bridges or the pool.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BridgedConfig {
    pub bridges: Vec<String>,
    /// The name of an Incus storage pool.
    pub pool: String,
    pub limits: BridgedLimits,
}

/// The `kernel.bridged.limits` and `kernel.vmLimits` keys: what all of Arca's virtual machines on this node may use
/// together. Every cap is required. Arca checks them before every create.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BridgedLimits {
    pub virtual_machines: u32,
    pub cpu: u32,
    pub memory: String,
    pub disk: String,
}

/// Why a config is refused. No variant carries a config value, except a project name the rules themselves name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// The file cannot be read. `why` is a fixed reason.
    Io {
        path: String,
        why: String,
    },
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
    /// The approval origin is not an https origin with a host name and at most a non-443 port. The value is left out.
    ApprovalOrigin,
    /// The canvas origin is not an http literal loopback IP and port. The value is left out.
    CanvasOrigin,
    /// Kernel projects are named, and no droidd project is listed to exclude.
    DroiddProjectsEmpty,
    /// The link config contains an invalid name or route.
    Link(String),
    ServiceCidr,
    /// The configured postgres image is not digest-pinned.
    PostgresImage,
    /// The adoption allowlist or its attach networks break a rule. The text names the rule, never a value.
    Adopt(&'static str),
    /// The `approval` key breaks a rule. The text names the rule, never a value.
    Approval(&'static str),
    /// The `kernel.bridged` key breaks a rule. The text names the rule, never a value.
    Bridged(&'static str),
    /// The `kernel.importServices` allowlist breaks a rule. The text names the rule, never a value.
    ImportServices(&'static str),
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
            Self::Link(why) => f.write_str(why),
            Self::ServiceCidr => write!(
                f,
                "kernel.serviceCidr must be an IPv4 /24 outside kernel.reservedCidrs"
            ),
            Self::PostgresImage => write!(
                f,
                "kernel.postgresImage must be a digest-pinned image reference (name@sha256:<64 hex>)"
            ),
            Self::Adopt(why) => write!(f, "kernel.adopt is refused: {why}"),
            Self::Approval(why) => write!(f, "approval is refused: {why}"),
            Self::Bridged(why) => write!(f, "kernel.bridged is refused: {why}"),
            Self::ImportServices(why) => write!(f, "kernel.importServices is refused: {why}"),
            Self::ApprovalHost => write!(
                f,
                "approvalHost is refused: it must be a host name of letters, digits, dots and hyphens"
            ),
            Self::ApprovalOrigin => write!(
                f,
                "approvalOrigin is refused: it must be https://, a lowercase host name, and at most a non-443 port, with no path"
            ),
            Self::CanvasOrigin => write!(
                f,
                "canvasOrigin is refused: it must be http:// and a literal loopback IP with a port, and no path"
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
    if let Some(link) = &config.link {
        link.check().map_err(|e| ConfigError::Link(e.to_string()))?;
    }
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
    if let Some(cidr) = &kernel.service_cidr {
        arca_netproject::check_cidr(cidr, &kernel.reserved_cidrs)
            .map_err(|_| ConfigError::ServiceCidr)?;
    }
    if let Some(image) = &kernel.postgres_image
        && arca_postgres::fingerprint(image).is_err()
    {
        return Err(ConfigError::PostgresImage);
    }
    check_adopt(kernel)?;
    check_bridged(kernel)?;
    check_import_services(kernel)?;
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
    if let Some(origin) = &config.approval_origin
        && !approval_origin_ok(origin)
    {
        return Err(ConfigError::ApprovalOrigin);
    }
    if canvas_origin_addr(&config.canvas_origin).is_none() {
        return Err(ConfigError::CanvasOrigin);
    }
    if let Some(approval) = &config.approval {
        check_approval(approval)?;
    }
    Ok(())
}

/// The `approval` rules: absolute trusted file paths with at least one origin, each origin an approval origin, and linked
/// hosts with unique plain names and absolute socket and token paths.
fn check_approval(approval: &ApprovalConfig) -> Result<(), ConfigError> {
    if !approval
        .trust_owner_keys_from
        .iter()
        .all(|path| path.is_absolute())
    {
        return Err(ConfigError::Approval(
            "each trustOwnerKeysFrom file must be an absolute path",
        ));
    }
    if !approval.trust_owner_keys_from.is_empty() && approval.origins.is_empty() {
        return Err(ConfigError::Approval(
            "origins must list the page origins the trusted files' passkeys answer from",
        ));
    }
    if !approval
        .origins
        .iter()
        .all(|origin| approval_origin_ok(origin))
    {
        return Err(ConfigError::Approval(
            "each origin must be https://, a lowercase host name, and at most a non-443 port, with no path",
        ));
    }
    if let Some(peer) = &approval.peer_keys {
        if !host_name_ok(&peer.from) {
            return Err(ConfigError::Approval(
                "peerKeys.from must be the page host's name: letters, digits, dots and hyphens",
            ));
        }
        if peer.origins.is_empty() || !peer.origins.iter().all(|origin| approval_origin_ok(origin))
        {
            return Err(ConfigError::Approval(
                "peerKeys.origins must list at least one page origin, each https://, a lowercase host name, and at most a non-443 port",
            ));
        }
    }
    let mut names = BTreeSet::new();
    for host in &approval.linked_hosts {
        if !host_name_ok(&host.name) || !names.insert(host.name.as_str()) {
            return Err(ConfigError::Approval(
                "each linked host needs a unique name of letters, digits, dots and hyphens",
            ));
        }
        if !host.socket.is_absolute() || !host.token_file.is_absolute() {
            return Err(ConfigError::Approval(
                "each linked host's socket and tokenFile must be absolute paths",
            ));
        }
    }
    Ok(())
}

/// The default canvas DSH origin: literal loopback, the port the stock profile listens on.
pub fn default_canvas_origin() -> String {
    "http://127.0.0.1:3081".to_string()
}

/// The socket address of a literal loopback `http://` origin, or `None` for anything else. A host name, a non-loopback IP, a
/// path, credentials, https, a missing port or port 0 are refused.
pub fn canvas_origin_addr(origin: &str) -> Option<SocketAddr> {
    let authority = origin.strip_prefix("http://")?;
    if authority.is_empty() || authority.contains(['/', '?', '#', '@']) {
        return None;
    }
    let addr: SocketAddr = authority.parse().ok()?;
    (addr.ip().is_loopback() && addr.port() != 0).then_some(addr)
}

/// Whether `origin` is `https://`, a lowercase DNS host name, and at most a port that is not 443, with no path. The host is
/// the WebAuthn rp id, so the passkeys file's stricter DNS rule applies here too.
fn approval_origin_ok(origin: &str) -> bool {
    let Some(authority) = origin.strip_prefix("https://") else {
        return false;
    };
    if authority.is_empty()
        || authority.contains(['/', '?', '#', '@'])
        || authority.contains(':') && authority.matches(':').count() > 1
    {
        return false;
    }
    let (host, port) = match authority.split_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (authority, None),
    };
    let port_ok = port.is_none_or(|port| {
        !port.starts_with('0')
            && port.bytes().all(|b| b.is_ascii_digit())
            && port.parse::<u16>().is_ok_and(|p| p != 0 && p != 443)
    });
    port_ok && dns_name_ok(host)
}

/// Whether `name` is a lowercase DNS name: 1 to 253 bytes of dot-separated labels, each 1 to 63 lowercase letters, digits
/// and dashes, with no dash at either end.
fn dns_name_ok(name: &str) -> bool {
    (1..=253).contains(&name.len())
        && name.split('.').all(|label| {
            (1..=63).contains(&label.len())
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

/// The caps rule: `virtualMachines` and `cpu` of at least 1, `memory` and `disk` as sizes.
fn limits_ok(limits: &BridgedLimits) -> bool {
    limits.virtual_machines != 0
        && limits.cpu != 0
        && crate::service::size_bytes(&limits.memory).is_some()
        && crate::service::size_bytes(&limits.disk).is_some()
}

/// The `kernel.vmLimits` rules: it states the caps of a node that has the managed service network and no
/// `kernel.bridged`, so the caps are stated once.
fn check_vm_limits(kernel: &KernelConfig) -> Result<(), ConfigError> {
    let Some(limits) = &kernel.vm_limits else {
        return Ok(());
    };
    if kernel.bridged.is_some() {
        return Err(ConfigError::Bridged(
            "kernel.vmLimits and kernel.bridged.limits are the same caps: a node with kernel.bridged states them there only",
        ));
    }
    if kernel.service_cidr.is_none() {
        return Err(ConfigError::Bridged(
            "kernel.vmLimits needs kernel.serviceCidr: a virtual machine joins the managed network or a bridge of kernel.bridged",
        ));
    }
    if !limits_ok(limits) {
        return Err(ConfigError::Bridged(
            "vmLimits must set virtualMachines and cpu of at least 1, and memory and disk as sizes such as 64GiB",
        ));
    }
    Ok(())
}

/// The bridged rules: at least one bridge, each a host interface name listed once and never the service bridge Arca
/// manages; a plain pool name; every cap; no scratch run. `kernel.serviceCidr` may be set beside it: the node is then mixed.
fn check_bridged(kernel: &KernelConfig) -> Result<(), ConfigError> {
    check_vm_limits(kernel)?;
    let Some(bridged) = &kernel.bridged else {
        return Ok(());
    };
    if kernel.scratch {
        return Err(ConfigError::Bridged(
            "kernel.scratch must be false on a bridged node",
        ));
    }
    let mut names = BTreeSet::new();
    if bridged.bridges.is_empty()
        || !bridged.bridges.iter().all(|bridge| {
            crate::service::host_bridge_ok(bridge)
                && bridge != arca_netproject::BRIDGE
                && names.insert(bridge)
        })
    {
        return Err(ConfigError::Bridged(
            "bridges must list at least one existing host bridge, each an interface name listed once",
        ));
    }
    if !arca_postgres::pool_ok(&bridged.pool) {
        return Err(ConfigError::Bridged(
            "pool must be the name of an Incus storage pool",
        ));
    }
    if !limits_ok(&bridged.limits) {
        return Err(ConfigError::Bridged(
            "limits must set virtualMachines and cpu of at least 1, and memory and disk as sizes such as 64GiB",
        ));
    }
    Ok(())
}

/// The import rules: each entry names a service by a service name, a project that `droiddProjects` lists, and an IPv4 /24;
/// no name is listed twice; and the node has a service network of its own (a managed or a mixed node, never a bridged
/// one) and is not a scratch run.
fn check_import_services(kernel: &KernelConfig) -> Result<(), ConfigError> {
    if kernel.import_services.is_empty() {
        return Ok(());
    }
    // An import needs the node's own service network. A bridged node has none; a mixed node, which sets
    // kernel.bridged beside kernel.serviceCidr, has it, so the host bridges do not stand in the way.
    if kernel.scratch || kernel.service_cidr.is_none() {
        return Err(ConfigError::ImportServices(
            "it needs kernel.serviceCidr and no kernel.scratch: a bridged node, which has no service network, imports nothing",
        ));
    }
    let mut names = BTreeSet::new();
    for entry in &kernel.import_services {
        if !crate::service::service_name_ok(&entry.name) {
            return Err(ConfigError::ImportServices(
                "each name must be a service name",
            ));
        }
        if !kernel.droidd_projects.contains(&entry.project) {
            return Err(ConfigError::ImportServices(
                "each project must be listed in kernel.droiddProjects",
            ));
        }
        if arca_netproject::check_cidr(&entry.cidr, &[]).is_err() {
            return Err(ConfigError::ImportServices("each cidr must be an IPv4 /24"));
        }
        if !names.insert(&entry.name) {
            return Err(ConfigError::ImportServices("a name is listed twice"));
        }
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
    fn a_configured_postgres_image_must_be_digest_pinned() {
        let mut config = config_with(None);
        config.kernel.postgres_image = Some("docker.io/library/postgres:18".to_string());
        assert_eq!(check(&config), Err(ConfigError::PostgresImage));
        config.kernel.postgres_image = Some(format!(
            "docker.io/library/postgres@sha256:{}",
            "a".repeat(64)
        ));
        assert_eq!(check(&config), Ok(()));
    }

    #[test]
    fn invalid_link_names_are_refused_before_commands_run() {
        let mut config = config_with(None);
        for name in [
            String::new(),
            "x\n".to_string(),
            "\u{0000}\u{001f}".to_string(),
            "x".repeat(61),
        ] {
            config.link = Some(arca_link::Config {
                name: Some(name),
                ..Default::default()
            });
            let error = check(&config).unwrap_err();
            assert!(matches!(error, ConfigError::Link(_)));
            assert!(error.to_string().starts_with("link.name must contain"));
        }
        config.link = Some(arca_link::Config {
            name: Some("λ".repeat(60)),
            ..Default::default()
        });
        assert_eq!(check(&config), Ok(()));
    }

    #[test]
    fn the_approval_key_is_optional_and_its_rules_name_no_value() {
        let mut config = config_with(None);
        assert_eq!(
            config.approval, None,
            "absent, nothing more is trusted or linked"
        );
        let parse = |value: serde_json::Value| -> ApprovalConfig {
            serde_json::from_value(value).expect("the approval key parses")
        };
        config.approval = Some(parse(serde_json::json!({
            "trustOwnerKeysFrom": ["/var/lib/arca-alpha/owner-passkeys"],
            "origins": ["https://alpha.example.com:8443"],
            "linkedHosts": [
                { "name": "beta", "socket": "/run/arca-beta/arca.sock", "tokenFile": "/etc/arca/beta-relay.secret" }
            ]
        })));
        assert_eq!(check(&config), Ok(()));

        for (bad, rule) in [
            (
                serde_json::json!({ "trustOwnerKeysFrom": ["/var/lib/arca-alpha/owner-passkeys"] }),
                "origins must list",
            ),
            (
                serde_json::json!({ "trustOwnerKeysFrom": ["relative/owner-passkeys"], "origins": ["https://alpha.example.com"] }),
                "absolute path",
            ),
            (
                serde_json::json!({ "origins": ["http://alpha.example.com"] }),
                "each origin must be https://",
            ),
            (
                serde_json::json!({ "linkedHosts": [
                    { "name": "beta", "socket": "/run/b.sock", "tokenFile": "/etc/b" },
                    { "name": "beta", "socket": "/run/c.sock", "tokenFile": "/etc/c" }
                ] }),
                "unique name",
            ),
            (
                serde_json::json!({ "linkedHosts": [{ "name": "beta", "socket": "b.sock", "tokenFile": "/etc/b" }] }),
                "absolute paths",
            ),
        ] {
            config.approval = Some(parse(bad));
            let refused = check(&config).expect_err("the approval key is refused");
            assert!(matches!(refused, ConfigError::Approval(_)));
            assert!(refused.to_string().contains(rule), "{refused}");
            assert!(
                !refused.to_string().contains("alpha.example"),
                "no value is echoed"
            );
        }
        config.approval = Some(parse(serde_json::json!({
            "peerKeys": { "from": "alpha", "origins": ["https://alpha.example.com:8443"] }
        })));
        assert_eq!(check(&config), Ok(()), "peerKeys alone is enough");
        for (bad, rule) in [
            (
                serde_json::json!({ "peerKeys": { "from": "alpha", "origins": [] } }),
                "peerKeys.origins",
            ),
            (
                serde_json::json!({ "peerKeys": { "from": "alpha", "origins": ["http://alpha.example.com"] } }),
                "peerKeys.origins",
            ),
            (
                serde_json::json!({ "peerKeys": { "from": "al pha", "origins": ["https://alpha.example.com"] } }),
                "peerKeys.from",
            ),
        ] {
            config.approval = Some(parse(bad));
            let refused = check(&config).expect_err("the peerKeys key is refused");
            assert!(refused.to_string().contains(rule), "{refused}");
            assert!(
                !refused.to_string().contains("alpha.example"),
                "no value is echoed"
            );
        }
        let unknown: Result<ApprovalConfig, _> = serde_json::from_value(
            serde_json::json!({ "peerKeys": { "from": "alpha", "origins": [], "keys": [] } }),
        );
        assert!(unknown.is_err(), "an unknown peerKeys key is refused");
        let unknown: Result<ApprovalConfig, _> =
            serde_json::from_value(serde_json::json!({ "trustOwnerKeys": [] }));
        assert!(unknown.is_err(), "an unknown key is refused");
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

    static LIMITS: std::sync::LazyLock<serde_json::Value> = std::sync::LazyLock::new(
        || serde_json::json!({ "virtualMachines": 4, "cpu": 8, "memory": "64GiB", "disk": "1TiB" }),
    );

    fn bridged(kernel: serde_json::Value) -> Result<(), ConfigError> {
        let mut value = serde_json::json!({
            "stateDir": "/nonexistent/state",
            "incus": { "socket": "/nonexistent/incus.sock" },
            "listen": { "unix": "/nonexistent/arca.sock" },
            "tokens": { "hashDir": "/nonexistent/tokens" },
            "kernel": { "droiddProjects": ["droid"], "projects": ["arcak-svc"],
                "bridged": { "bridges": ["br0", "vmbr1"], "pool": "vmpool", "limits": LIMITS.clone() } }
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

    fn importing(kernel: serde_json::Value) -> Result<NodeConfig, ConfigError> {
        let mut value = serde_json::json!({
            "stateDir": "/nonexistent/state",
            "incus": { "socket": "/nonexistent/incus.sock" },
            "listen": { "unix": "/nonexistent/arca.sock" },
            "tokens": { "hashDir": "/nonexistent/tokens" },
            "kernel": { "droiddProjects": ["app1"], "projects": ["arcak-svc"], "serviceCidr": "192.0.2.0/24" }
        });
        if let serde_json::Value::Object(extra) = kernel {
            value["kernel"]
                .as_object_mut()
                .expect("kernel")
                .extend(extra);
        }
        let config: NodeConfig =
            serde_json::from_value(value).map_err(|e| ConfigError::Parse(e.to_string()))?;
        check(&config)?;
        Ok(config)
    }

    #[test]
    fn the_import_allowlist_is_empty_by_default_and_refused_unless_every_rule_holds() {
        use serde_json::json;
        let none = importing(json!({})).expect("no allowlist");
        assert!(none.kernel.import_services.is_empty());
        let entry = json!({ "name": "db1", "project": "app1", "cidr": "10.0.5.0/24" });
        let listed = importing(json!({ "importServices": [entry] })).expect("one entry");
        assert_eq!(
            listed
                .kernel
                .import_service("db1", "app1")
                .map(|e| e.cidr.as_str()),
            Some("10.0.5.0/24")
        );
        assert!(listed.kernel.import_service("db1", "arcak-svc").is_none());
        assert!(listed.kernel.import_service("db2", "app1").is_none());
        // An unknown key is a parse refusal, never ignored.
        assert!(matches!(
            importing(json!({ "importServices": [
                { "name": "db1", "project": "app1", "cidr": "10.0.5.0/24", "force": true }
            ] })),
            Err(ConfigError::Parse(_))
        ));
        for bad in [
            // A project that is not another manager's: Arca's own, and one nobody listed.
            json!({ "importServices": [{ "name": "db1", "project": "arcak-svc", "cidr": "10.0.5.0/24" }] }),
            json!({ "importServices": [{ "name": "db1", "project": "elsewhere", "cidr": "10.0.5.0/24" }] }),
            json!({ "importServices": [{ "name": "Db1", "project": "app1", "cidr": "10.0.5.0/24" }] }),
            json!({ "importServices": [{ "name": "db1", "project": "app1", "cidr": "10.0.5.0/16" }] }),
            json!({ "importServices": [{ "name": "db1", "project": "app1", "cidr": "nowhere" }] }),
            json!({ "importServices": [entry, entry] }),
            json!({ "importServices": [entry], "serviceCidr": null }),
        ] {
            assert!(
                matches!(importing(bad.clone()), Err(ConfigError::ImportServices(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_mixed_node_may_list_imports_and_a_bridged_node_may_not() {
        use serde_json::json;
        let entry = json!({ "name": "db1", "project": "app1", "cidr": "10.0.5.0/24" });
        let host_bridges =
            json!({ "bridges": ["br0"], "pool": "vmpool", "limits": LIMITS.clone() });
        // The node has its own service network, so the host bridges beside it do not stand in the way.
        let mixed = importing(json!({ "importServices": [entry], "bridged": host_bridges }))
            .expect("a mixed node");
        assert!(mixed.kernel.import_service("db1", "app1").is_some());
        // A bridged node has no service network of its own.
        assert!(matches!(
            importing(
                json!({ "importServices": [entry], "bridged": host_bridges, "serviceCidr": null })
            ),
            Err(ConfigError::ImportServices(_))
        ));
    }

    #[test]
    fn vm_limits_are_the_caps_of_a_managed_node_without_host_bridges() {
        use serde_json::json;
        let managed = |extra: serde_json::Value| {
            let mut kernel = json!({ "bridged": null, "serviceCidr": "192.0.2.0/24" });
            kernel
                .as_object_mut()
                .expect("kernel")
                .extend(extra.as_object().expect("extra").clone());
            bridged(kernel)
        };
        assert_eq!(managed(json!({})), Ok(()));
        assert_eq!(managed(json!({ "vmLimits": LIMITS.clone() })), Ok(()));
        // Without the managed network a virtual machine has nowhere to go but a bridge of kernel.bridged.
        assert!(matches!(
            managed(json!({ "vmLimits": LIMITS.clone(), "serviceCidr": null })),
            Err(ConfigError::Bridged(_))
        ));
        assert!(matches!(
            managed(
                json!({ "vmLimits": { "virtualMachines": 0, "cpu": 8, "memory": "64GiB", "disk": "1TiB" } })
            ),
            Err(ConfigError::Bridged(_))
        ));
        assert!(matches!(
            managed(json!({ "vmLimits": { "virtualMachines": 1, "cpu": 8, "memory": "64GiB" } })),
            Err(ConfigError::Parse(_))
        ));
        let config = |kernel: serde_json::Value| -> KernelConfig {
            serde_json::from_value(kernel).expect("kernel")
        };
        let caps = |kernel: serde_json::Value| config(kernel).vm_caps().map(|caps| caps.cpu);
        assert_eq!(caps(json!({ "droiddProjects": [] })), None);
        assert_eq!(
            caps(json!({ "droiddProjects": [], "vmLimits": LIMITS.clone() })),
            Some(8)
        );
        assert_eq!(
            caps(
                json!({ "droiddProjects": [], "bridged": { "bridges": ["br0"], "pool": "p", "limits": LIMITS.clone() } })
            ),
            Some(8)
        );
    }

    #[test]
    fn a_bridged_node_is_refused_unless_every_rule_holds() {
        use serde_json::json;
        let limits = LIMITS.clone();
        for bad in [
            json!({ "virtualMachines": 0, "cpu": 8, "memory": "64GiB", "disk": "1TiB" }),
            json!({ "virtualMachines": 4, "cpu": 0, "memory": "64GiB", "disk": "1TiB" }),
            json!({ "virtualMachines": 4, "cpu": 8, "memory": "64", "disk": "1TiB" }),
            json!({ "virtualMachines": 4, "cpu": 8, "memory": "64GiB", "disk": "0TiB" }),
        ] {
            assert!(
                matches!(
                    bridged(
                        json!({ "bridged": { "bridges": ["br0"], "pool": "vmpool", "limits": bad } })
                    ),
                    Err(ConfigError::Bridged(_))
                ),
                "{bad}"
            );
        }
        for missing in ["virtualMachines", "cpu", "memory", "disk"] {
            let mut partial = limits.clone();
            partial.as_object_mut().expect("limits").remove(missing);
            assert!(
                matches!(
                    bridged(
                        json!({ "bridged": { "bridges": ["br0"], "pool": "vmpool", "limits": partial } })
                    ),
                    Err(ConfigError::Parse(_))
                ),
                "{missing} is required"
            );
        }
        assert!(matches!(
            bridged(json!({ "bridged": { "bridges": ["br0"], "pool": "vmpool" } })),
            Err(ConfigError::Parse(_))
        ));
        assert_eq!(bridged(json!({})), Ok(()));
        // The managed network beside the host bridges is a mixed node. Its caps are the bridged ones, stated once.
        assert_eq!(bridged(json!({ "serviceCidr": "192.0.2.0/24" })), Ok(()));
        let refused = [
            json!({ "serviceCidr": "192.0.2.0/24", "vmLimits": limits }),
            json!({ "scratch": true }),
            json!({ "bridged": { "bridges": [], "pool": "vmpool", "limits": limits } }),
            json!({ "bridged": { "bridges": ["br0", "br0"], "pool": "vmpool", "limits": limits } }),
            json!({ "bridged": { "bridges": ["br0,br1"], "pool": "vmpool", "limits": limits } }),
            json!({ "bridged": { "bridges": ["a-name-longer-than-an-interface"], "pool": "vmpool", "limits": limits } }),
            json!({ "bridged": { "bridges": ["arcak-svcbr"], "pool": "vmpool", "limits": limits } }),
            json!({ "bridged": { "bridges": ["br0"], "pool": "", "limits": limits } }),
            json!({ "bridged": { "bridges": ["br0"], "pool": "a/b", "limits": limits } }),
        ];
        for kernel in refused {
            assert!(
                matches!(bridged(kernel.clone()), Err(ConfigError::Bridged(_))),
                "{kernel}"
            );
        }
        assert!(matches!(
            bridged(
                json!({ "bridged": { "bridges": ["br0"], "pool": "vmpool", "limits": limits, "cidr": "x" } })
            ),
            Err(ConfigError::Parse(_))
        ));
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

    #[test]
    fn the_incus_socket_path_rule_marks_only_the_standard_local_sockets() {
        let mut config = config_with(None);
        assert_eq!(config.incus.local, None);
        config.incus.socket = PathBuf::from(LOCAL_INCUS_SOCKET);
        assert!(
            config.incus.socket_is_local(),
            "the standard local daemon socket is local"
        );
        config.incus.socket = PathBuf::from("/var/lib/incus/unix.socket.user");
        assert!(
            config.incus.socket_is_local(),
            "the local daemon's unprivileged socket is the same daemon"
        );
        config.incus.socket = PathBuf::from("/nonexistent/incus-proxy.sock");
        assert!(
            !config.incus.socket_is_local(),
            "any other path is a proxy for the fallback"
        );
    }

    #[test]
    fn incus_local_parses_as_an_optional_bool_in_the_incus_section() {
        let mut value = serde_json::json!({
            "stateDir": "/nonexistent/state",
            "incus": { "socket": "/nonexistent/incus.sock", "local": true },
            "listen": { "unix": "/nonexistent/arca.sock" },
            "tokens": { "hashDir": "/nonexistent/tokens" },
            "kernel": { "droiddProjects": ["droid"] }
        });
        let config: NodeConfig =
            serde_json::from_value(value.clone()).expect("the local key parses");
        assert_eq!(config.incus.local, Some(true));

        value["incus"]["local"] = serde_json::json!("yes");
        assert!(
            serde_json::from_value::<NodeConfig>(value).is_err(),
            "a local that is not a bool is refused"
        );
    }

    #[test]
    fn an_approval_origin_must_be_https_a_lowercase_host_and_an_optional_non_443_port() {
        let mut config = config_with(None);
        for bad in [
            "http://box1.example.com",
            "https://",
            "https://box1.example.com/approve",
            "https://box1.example.com:443",
            "https://box1.example.com:",
            "https://box1.example.com:0",
            "https://box1.example.com:65536",
            "https://Box1.example.com",
            "https://-box1.example.com",
            "https://box1.example.com:08443",
        ] {
            config.approval_origin = Some(bad.to_string());
            assert_eq!(
                check(&config),
                Err(ConfigError::ApprovalOrigin),
                "{bad:?} is not an approval origin"
            );
        }
        for good in [
            "https://box1.example.com",
            "https://box1.example.com:8443",
            "https://node1.example",
        ] {
            config.approval_origin = Some(good.to_string());
            assert_eq!(check(&config), Ok(()), "{good:?} is an approval origin");
        }
        config.approval_origin = None;
        assert_eq!(check(&config), Ok(()));
    }

    #[test]
    fn canvas_origin_must_be_http_loopback_literal_with_a_port() {
        let mut config = config_with(None);
        assert_eq!(config.canvas_origin, "http://127.0.0.1:3081");
        for bad in [
            "https://127.0.0.1:3081",
            "http://localhost:3081",
            "http://192.168.1.5:3081",
            "http://127.0.0.1",
            "http://127.0.0.1:0",
            "http://127.0.0.1:3081/",
            "http://127.0.0.1:3081/canvas",
            "http://user@127.0.0.1:3081",
            "http://2130706433:3081",
            "http://0x7f000001:3081",
        ] {
            config.canvas_origin = bad.to_string();
            assert_eq!(
                check(&config),
                Err(ConfigError::CanvasOrigin),
                "{bad:?} is not a loopback canvas origin"
            );
        }
        for good in [
            "http://127.0.0.1:3081",
            "http://127.0.0.2:8080",
            "http://[::1]:3081",
        ] {
            config.canvas_origin = good.to_string();
            assert_eq!(
                check(&config),
                Ok(()),
                "{good:?} is a loopback canvas origin"
            );
        }
    }
}
