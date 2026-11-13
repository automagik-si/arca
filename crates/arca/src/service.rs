//! Services: one template-derived container in the service project, with its own NIC ACL. A virtual-machine service is
//! in the same project. On the managed service network it has an address of the same plan and its own NIC ACL, like a
//! container. On a host bridge that already exists it has no ACL, no fixed address and no Incus-managed network.
//!
//! The desired-state entry is the record. Its config names the template, address, ports, links and publication; the
//! live state comes from Incus on every read. Secret values stay in files and enter the Incus config only as references.

use arca_kernel::{
    journal::EffectClass,
    ownership::IncusKind,
    plugin::{Ctx, EffectOp, Manifest, Observe, Plugin, PluginError},
};
use arca_netproject::destination::Destination;
use arca_netproject::{BRIDGE, PROFILE, PROJECT, create, manifest, name_ok, observed};
use arca_providers::incus::SECRET_PREFIX;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

pub const COOKIE: &str = "user.arca.managed";
pub const ACL_INFIX: &str = "svc";
pub const TEMPLATE_KEY: &str = "user.arca.template";
pub const ADDRESS_KEY: &str = "user.arca.address";
pub const PORTS_KEY: &str = "user.arca.ports";
pub const PUBLISHED_KEY: &str = "user.arca.published";
pub const LINKS_KEY: &str = "user.arca.links";
pub const ENVKEYS_KEY: &str = "user.arca.envkeys";
/// The key that marks a function's instance. The writer of a run refuses an instance without it.
pub const FUNCTION_KEY: &str = arca_incus::function_run::FUNCTION_KEY;
/// The template kind, and the Incus instance type, of a virtual-machine service.
pub const VM_KIND: &str = "virtual-machine";
/// Incus caps a disk device path at 255 bytes; "/" is the rootfs disk's path, not a data path.
const DATA_PATH_MAX: usize = 255;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ServiceLink {
    pub to: String,
    pub port: u16,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub uuid: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ServiceGrant {
    pub consumer: String,
    pub address: String,
    pub port: u16,
    pub uuid: String,
    pub provider_uuid: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Published {
    pub node: String,
    pub port: u16,
    pub reach: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    #[serde(default)]
    pub cpu: Option<String>,
    #[serde(default)]
    pub memory: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Data {
    pub path: String,
    #[serde(default)]
    pub uid: Option<u32>,
    #[serde(default)]
    pub gid: Option<u32>,
    #[serde(default)]
    pub size: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Lan {
    pub port: u16,
}

/// A virtual-machine template's `update`: the key of its `settings` that holds the version its service runs, an image
/// by digest. It is how `service_status` knows the version a machine was created with from the one the installed
/// template names now. A container template needs none: its version is its `digest`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Update {
    pub pin: String,
}

/// A template's `kernelAccess`: the port, inside the instance, where the service finds this node's kernel on loopback.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct KernelAccess {
    pub port: u16,
}

/// What a service with kernel access records: the port inside its instance, and the path of the socket on the host its
/// device connects to, which is the node's `listen.services` as it was when the service was created.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct KernelLink {
    pub port: u16,
    pub connect: String,
}

/// Whether the destinations a record carries are ones a service may have: the service is the node's own, on the
/// managed network, and each is an address and port [`arca_netproject::destination::refusal`] takes, named once.
pub fn destinations_ok(cfg: &ServiceConfig) -> bool {
    let Ok(gateway) = cfg.gateway.parse::<Ipv4Addr>() else {
        return false;
    };
    cfg.tenant.is_none()
        && !cfg.adopted
        && cfg.project == PROJECT
        && cfg.bridge == BRIDGE
        && cfg.destinations.len() <= arca_netproject::destination::MAX_PER_SERVICE
        && cfg
            .destinations
            .iter()
            .enumerate()
            .all(|(index, destination)| {
                arca_netproject::destination::refusal(
                    destination.address,
                    destination.port,
                    gateway,
                )
                .is_none()
                    && !cfg.destinations[..index].contains(destination)
            })
}

/// The name of the device that gives a service kernel access.
pub const KERNEL_DEVICE: &str = "kernel";
/// The lowest port a template's `kernelAccess` may name.
pub const KERNEL_ACCESS_PORT_MIN: u16 = 1024;

/// The socket on the host a kernel access device may connect to: a path the services socket may have
/// ([`arca_mcp::services_path_ok`]). No address of any network is one.
pub fn kernel_connect_ok(connect: &str) -> bool {
    arca_mcp::services_path_ok(Path::new(connect)).is_ok()
}

/// The device of a service with kernel access. Incus listens inside the instance (`bind=instance`) on its loopback and
/// connects from the host to the kernel's services socket, so nothing of it is on the bridge or on any host port, and
/// the instance's NIC ACL is not involved. That socket serves the MCP routes alone, so whoever connects still needs a
/// bearer token: the device carries bytes and no authority.
pub fn kernel_device(link: &KernelLink) -> Value {
    json!({
        "type": "proxy",
        "bind": "instance",
        "listen": format!("tcp:127.0.0.1:{}", link.port),
        "connect": format!("unix:{}", link.connect)
    })
}

/// The name every virtual machine's NIC is given inside the guest: the one interface the public images configure. The
/// agent renames the NIC to it, whatever PCI slot the host's QEMU gave it.
pub const VM_NIC_NAME: &str = "enp5s0";

/// What only a virtual-machine service records: its root disk size, the fixed MAC the request gave, if any, and what
/// its template gave cloud-init for the first boot. A record without the last two is exactly the one an earlier kernel
/// wrote.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Vm {
    pub root_size: String,
    #[serde(default)]
    pub mac: Option<String>,
    /// The template's first-boot script, as the instance's `cloud-init.user-data`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_data: Option<String>,
    /// The service's settings, as a `cloud-init.vendor-data` that writes them to [`GUEST_SETTINGS_PATH`]: the
    /// template's, with the values its create replaced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor_data: Option<String>,
    /// The settings its create replaced, by key, when it replaced any. The machine got them merged into the
    /// template's ([`Vm::guest_settings`]); they are kept apart here so the record says what was asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<Map<String, Value>>,
}

impl Vm {
    /// The settings object the machine was created with, exactly as its guest reads it at
    /// [`GUEST_SETTINGS_PATH`]: read back from the cloud-init data the create carried, so a template edited since
    /// changes nothing here. `None` for a machine without settings.
    pub fn guest_settings(&self) -> Option<Value> {
        use base64::Engine as _;
        let content = self
            .vendor_data
            .as_deref()?
            .lines()
            .find_map(|line| line.strip_prefix("  content: "))?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(content.trim())
            .ok()?;
        serde_json::from_slice::<Value>(&bytes)
            .ok()
            .filter(Value::is_object)
    }
}

/// The kernel access an instance's own devices show: its `kernel` device, when it is exactly the device Arca makes for
/// some port and some services socket path ([`kernel_device`]). `None` for an instance with no such device, or with one
/// of any other shape. A record made again from an instance's marks reads it, when it connects to the node's own
/// services socket, so the record says what the instance has.
pub fn kernel_link_of(row: &Value) -> Option<KernelLink> {
    let device = &row["devices"][KERNEL_DEVICE];
    let link = KernelLink {
        port: device["listen"]
            .as_str()?
            .strip_prefix("tcp:127.0.0.1:")?
            .parse()
            .ok()?,
        connect: device["connect"]
            .as_str()?
            .strip_prefix("unix:")?
            .to_string(),
    };
    (link.port >= KERNEL_ACCESS_PORT_MIN
        && kernel_connect_ok(&link.connect)
        && *device == kernel_device(&link))
    .then_some(link)
}

/// The `raw.lxc` value of a container whose template says `noNewPrivileges`: LXC sets `PR_SET_NO_NEW_PRIVS` on the
/// container's init, so every process inside inherits it. Incus 7.5 has no config key of its own for this.
pub const NO_NEW_PRIVS_RAW_LXC: &str = "lxc.no_new_privs=1";

/// Where a virtual machine's guest finds its template's `settings`, as one JSON object.
pub const GUEST_SETTINGS_PATH: &str = "/etc/arca/settings.json";
/// The largest first-boot script a template may carry, in bytes.
const FIRST_BOOT_MAX: usize = 256 * 1024;
/// The largest `settings` object a template may carry, as compact JSON, in bytes.
const SETTINGS_MAX: usize = 16 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ServiceConfig {
    pub name: String,
    pub template: String,
    pub instance: String,
    pub ipv4: String,
    pub ports: Vec<u16>,
    #[serde(default)]
    pub links: Vec<ServiceLink>,
    #[serde(default)]
    pub incoming: Vec<ServiceGrant>,
    #[serde(default)]
    pub secret_refs: BTreeMap<String, String>,
    /// Existing instances apply only these environment keys. Unrelated rendered values need not be available locally.
    #[serde(default)]
    pub environment_patch: Option<BTreeSet<String>>,
    #[serde(default)]
    pub published: Option<Published>,
    pub created: u64,
    pub source: Value,
    pub pool: String,
    pub project: String,
    pub bridge: String,
    pub gateway: String,
    #[serde(default)]
    pub entrypoint: Option<String>,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    #[serde(default)]
    pub droidd_port: Option<u16>,
    #[serde(default)]
    pub limits: Option<Limits>,
    #[serde(default)]
    pub autostart: bool,
    #[serde(default)]
    pub data: Option<Data>,
    #[serde(default)]
    pub lan: Option<Lan>,
    #[serde(default)]
    pub start: bool,
    /// An instance Arca adopted instead of creating; its project is the instance's own.
    #[serde(default)]
    pub adopted: bool,
    /// A record whose create has not settled, shown as `creating` after a restart.
    #[serde(default)]
    pub creating: bool,
    /// Present on a virtual machine. A container's record never carries the key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vm: Option<Vm>,
    /// Present on a service in a tenant. A service of the node's own project never carries the key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<TenantRef>,
    /// Present on a service whose template declares `kernelAccess`. No other record carries the key, and no service in
    /// a tenant ever does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel_access: Option<KernelLink>,
    /// The private addresses this service may reach through the node, each on one tcp port: what the node's config
    /// (`kernel.serviceDestinations`) named for it when the owner approved its create. No other record carries the
    /// key, and no service in a tenant ever does. Its NIC rules are built from this list and from nothing else.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub destinations: Vec<Destination>,
    /// Present on a function: a container the kernel starts for one run and stops itself (`crate::function`). No
    /// service's record carries the key, and nothing in a tenant ever does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function: Option<crate::function::Function>,
    /// What the node's follower does with this service: `automagik` moves it to Automagik's newest published server
    /// image, `none` never does. Absent, a service made from the `automagik-server` template follows and every other
    /// service does not ([`ServiceConfig::follows_automagik`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow: Option<String>,
    /// The version the image at `digest` is, as the follower learned it (from Automagik's record or the template's pin),
    /// so the follower can order the service's image against a newer one after any restart. It holds for the image it
    /// names only: a service moved to another digest by hand has no known version until the follower learns it again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_version: Option<FollowVersion>,
    /// Set from the template's `noNewPrivileges`: the container runs with no-new-privileges, so a setuid or setcap
    /// binary inside it cannot raise its privileges. Off in a record that never says it, so every older record and every
    /// template without the key is created as it always was. Only a container of the node's own project carries it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_new_privileges: bool,
}

/// An image digest and the version Automagik published it as.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FollowVersion {
    pub digest: String,
    pub version: String,
}

/// What only a service in a tenant records: the tenant, its address block, and the address of the node's front service
/// when the node names one.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TenantRef {
    pub id: String,
    /// The tenant's block of the service subnet, as `a.b.c.d/29` or `a.b.c.d/28`.
    pub block: String,
    /// The address that reaches this service's declared ports from the node's own project (`kernel.tenants.front`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub front: Option<String>,
    /// The deploy this service was created in, when its create named one. It is provisional until the deploy is
    /// confirmed or 15 minutes old, and its creator may roll the deploy back until then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deploy: Option<String>,
}

/// The template a database is made from on a node whose config names no `kernel.postgresImage`: `postgres_create`
/// there makes the service `service_create` makes from this template, and `postgres_list` shows every service of the
/// node's own project made from it.
pub const DATABASE_TEMPLATE: &str = "postgres-autopg";

/// The name of the Incus instance of the service `name`: the service's own name, and `<name>-<id>` in tenant `id`. Incus
/// refuses two instances of one name on one bridge, whatever their projects, since the name is the instance's DNS name
/// there; the tenant's id keeps a tenant's instance apart from every other tenant's and from the node's own.
pub fn instance_name(tenant: Option<&str>, name: &str) -> String {
    match tenant {
        Some(id) => format!("{name}-{id}"),
        None => name.to_string(),
    }
}

/// The id of the entry that records the service `name`: `service-<name>` in the node's own project, and
/// `t-<id>-service-<name>` in tenant `id`, so two tenants may each hold a service of one name.
pub fn entry_id(tenant: Option<&str>, name: &str) -> String {
    match tenant {
        Some(id) => format!("t-{id}-service-{name}"),
        None => format!("service-{name}"),
    }
}

/// The image repository the Automagik follower moves a service within.
pub const AUTOMAGIK_IMAGE: &str = "ghcr.io/automagik-si/automagik-server";
/// The template that follows Automagik's releases unless its service says `follow: none`.
pub const AUTOMAGIK_TEMPLATE: &str = "automagik-server";
/// The `follow` value that moves a service to Automagik's newest published server image.
pub const FOLLOW_AUTOMAGIK: &str = "automagik";
/// The `follow` value that keeps a service where it is.
pub const FOLLOW_NONE: &str = "none";

impl ServiceConfig {
    /// Whether a service of this record can follow Automagik at all: a container of the node's own project, made from
    /// Automagik's server image by digest. A virtual machine, a function, an adopted service and a tenant's service
    /// cannot.
    pub fn can_follow_automagik(&self) -> bool {
        !self.adopted
            && self.vm.is_none()
            && self.function.is_none()
            && self.tenant.is_none()
            && self.source["server"]
                .as_str()
                .and_then(|server| server.strip_prefix("https://"))
                .zip(
                    self.source["alias"]
                        .as_str()
                        .and_then(|alias| alias.rsplit_once("@sha256:")),
                )
                .is_some_and(|(server, (repository, _))| {
                    AUTOMAGIK_IMAGE
                        .strip_prefix(server)
                        .and_then(|rest| rest.strip_prefix('/'))
                        == Some(repository)
                })
    }

    /// Whether the node's follower moves this service: it can follow, and is not opted out.
    pub fn follows_automagik(&self) -> bool {
        self.can_follow_automagik()
            && match self.follow.as_deref() {
                Some(value) => value == FOLLOW_AUTOMAGIK,
                None => self.template == AUTOMAGIK_TEMPLATE,
            }
    }

    /// The version the follower recorded for `digest`, the image this service runs now, when it recorded one.
    pub fn followed_version(&self, digest: &str) -> Option<&str> {
        self.follow_version
            .as_ref()
            .filter(|known| known.digest == digest)
            .map(|known| known.version.as_str())
    }

    pub fn acl(&self) -> String {
        acl_name(&self.project, &self.name)
    }

    /// The id of the entry that records this service.
    pub fn entry_id(&self) -> String {
        entry_id(
            self.tenant.as_ref().map(|tenant| tenant.id.as_str()),
            &self.name,
        )
    }

    /// The name this service's Incus instance must have ([`instance_name`]).
    pub fn instance_name(&self) -> String {
        instance_name(
            self.tenant.as_ref().map(|tenant| tenant.id.as_str()),
            &self.name,
        )
    }

    /// Whether the record's project is the one its place requires: the node's service project, or its tenant's project
    /// under either tenant prefix.
    fn project_ok(&self) -> bool {
        match &self.tenant {
            None => self.project == PROJECT,
            Some(tenant) => {
                arca_kernel::ownership::tenant_id_ok(&tenant.id)
                    && arca_kernel::ownership::tenant_of_project(&self.project)
                        == Some(tenant.id.as_str())
            }
        }
    }

    /// The tenant's block, when the service is in a tenant: `Err` unless the record's block is a tenant block of the
    /// gateway's subnet and the service's address is inside it.
    fn tenant_block(&self) -> Option<Result<arca_netproject::tenant::Block, ()>> {
        let tenant = self.tenant.as_ref()?;
        let block = self
            .gateway
            .parse::<Ipv4Addr>()
            .ok()
            .and_then(|gateway| {
                let subnet = Ipv4Addr::from(u32::from(gateway) & 0xffff_ff00);
                arca_netproject::tenant::parse_block(&tenant.block, &format!("{subnet}/24"))
            })
            .filter(|block| {
                self.ipv4
                    .parse::<Ipv4Addr>()
                    .is_ok_and(|ip| block.holds(ip))
                    && tenant
                        .front
                        .as_deref()
                        .is_none_or(|front| front.parse::<Ipv4Addr>().is_ok())
            });
        Some(block.ok_or(()))
    }

    /// Whether this is a virtual machine on a host bridge: one with no address, ACL or link. A virtual machine on the
    /// managed service network names the service bridge, as a container does.
    pub fn on_host_bridge(&self) -> bool {
        self.vm.is_some() && self.bridge != BRIDGE
    }
}

pub struct Service;

pub fn manifest_for() -> Manifest {
    let mut manifest = manifest(
        Service::ID,
        vec![
            (IncusKind::Acl, EffectClass::A),
            (IncusKind::Volume, EffectClass::B),
            (IncusKind::Instance, EffectClass::B),
        ],
    );
    // A service may be in a tenant's project. The provider admits only the project of a tenant whose entry exists.
    manifest.projects.extend([
        format!("{}*", arca_kernel::ownership::TENANT_PROJECT_PREFIX),
        format!("{}*", arca_kernel::ownership::SCRATCH_TENANT_PROJECT_PREFIX),
    ]);
    manifest
}

pub fn acl_name(project: &str, name: &str) -> String {
    format!("{project}-{ACL_INFIX}-{name}")
}

pub fn service_name_ok(name: &str) -> bool {
    name_ok(name, "service", 40)
}

pub fn template_name_ok(name: &str) -> bool {
    name_ok(name, "template", 40)
}

/// Whether `name` can be a host bridge: a Linux interface name of 1 to 15 letters, digits, `-`, `_` and `.`, starting
/// with a letter or digit.
pub fn host_bridge_ok(name: &str) -> bool {
    (1..=15).contains(&name.len())
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// A fixed MAC in the one form Arca records: six lowercase hex octets joined by `:`. A group (multicast or broadcast)
/// address and the all-zero address are refused, since a NIC cannot hold them.
pub fn normalize_mac(text: &str) -> Option<String> {
    let octets: Vec<&str> = text.split(':').collect();
    if octets.len() != 6
        || !octets
            .iter()
            .all(|octet| octet.len() == 2 && octet.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return None;
    }
    let mac = text.to_ascii_lowercase();
    let first = u8::from_str_radix(&mac[..2], 16).ok()?;
    (first & 1 == 0 && mac != "00:00:00:00:00:00").then_some(mac)
}

/// The config keys every virtual machine carries: its template, both limits, autostart, and the switch that has the
/// agent rename the NIC to its device's `name`.
fn vm_config(cfg: &ServiceConfig) -> Map<String, Value> {
    let mut config = Map::new();
    config.insert(TEMPLATE_KEY.to_string(), json!(cfg.template));
    if let Some(limits) = &cfg.limits {
        if let Some(cpu) = &limits.cpu {
            config.insert("limits.cpu".into(), json!(cpu));
        }
        if let Some(memory) = &limits.memory {
            config.insert("limits.memory".into(), json!(memory));
        }
    }
    if cfg.autostart {
        config.insert("boot.autostart".into(), json!("true"));
    }
    config.insert("agent.nic_config".into(), json!("true"));
    // What cloud-init reads at the first boot. Neither value ever holds a secret: a virtual machine's secrets go into
    // its guest as files at each start, and never into its config.
    if let Some(vm) = &cfg.vm {
        if let Some(user_data) = &vm.user_data {
            config.insert("cloud-init.user-data".into(), json!(user_data));
        }
        if let Some(vendor_data) = &vm.vendor_data {
            config.insert("cloud-init.vendor-data".into(), json!(vendor_data));
        }
    }
    config
}

/// The devices every virtual machine carries: its own root disk on the node's pool, the NIC as `eth0`, and the agent's
/// config share as a drive. Incus 7.6 offers that share over 9p only, which a QEMU built without virtio-9p cannot give the
/// guest, so without the drive the agent never starts. The NIC is named for the guest: the public images configure
/// `enp5s0` alone, and the slot name the guest's kernel picks depends on the devices of the host's QEMU.
fn vm_devices(cfg: &ServiceConfig, vm: &Vm, mut nic: Map<String, Value>) -> Value {
    nic.insert("type".into(), json!("nic"));
    nic.insert("name".into(), json!(VM_NIC_NAME));
    json!({
        "root": { "type": "disk", "path": "/", "pool": cfg.pool, "size": vm.root_size },
        "eth0": nic,
        "agent": { "type": "disk", "source": "agent:config" }
    })
}

/// The create body of a virtual machine on a host bridge. It names no profile, no Incus network, no address and no ACL
/// or filter, so Incus adds no firewall rule for it. Its only `user.arca.*` key is the template: the kernel's ownership
/// rule admits no new one, and the bridge is the NIC's `parent`.
fn vm_body(cfg: &ServiceConfig, vm: &Vm) -> Value {
    let mut nic = Map::new();
    nic.insert("nictype".into(), json!("bridged"));
    nic.insert("parent".into(), json!(cfg.bridge));
    if let Some(mac) = &vm.mac {
        nic.insert("hwaddr".into(), json!(mac));
    }
    json!({
        "name": cfg.instance,
        "type": VM_KIND,
        "source": cfg.source,
        "profiles": [],
        "config": vm_config(cfg),
        "devices": vm_devices(cfg, vm, nic),
        "start": cfg.start
    })
}

/// The create body of a virtual machine on the managed service network: the address, filters and ACL of a container's
/// NIC, and no profile, since the service profile holds container keys.
fn managed_vm_body(cfg: &ServiceConfig, vm: &Vm, links: &[String]) -> Value {
    let mut config = vm_config(cfg);
    config.insert(ADDRESS_KEY.to_string(), json!(cfg.ipv4));
    if !cfg.ports.is_empty() {
        let ports = cfg
            .ports
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(",");
        config.insert(PORTS_KEY.to_string(), json!(ports));
    }
    if !links.is_empty() {
        config.insert(LINKS_KEY.to_string(), json!(links.join(",")));
    }
    let nic = json!({
        "network": cfg.bridge,
        "ipv4.address": cfg.ipv4,
        "security.port_isolation": "false",
        "security.mac_filtering": "true",
        "security.ipv4_filtering": "true",
        "security.acls": cfg.acl(),
        "security.acls.default.ingress.action": "reject",
        "security.acls.default.egress.action": "reject"
    });
    let nic = tenant_nic(cfg, nic);
    json!({
        "name": cfg.instance,
        "type": VM_KIND,
        "source": cfg.source,
        "profiles": [],
        "config": config,
        "devices": vm_devices(cfg, vm, nic.as_object().cloned().unwrap_or_default()),
        "start": cfg.start
    })
}

/// Whether a virtual machine's record holds what its create needs: a pool, both limits, a root disk size, and a MAC in
/// the recorded form when it has one.
fn vm_record_ok(cfg: &ServiceConfig, vm: &Vm) -> bool {
    !cfg.pool.is_empty()
        && cfg.limits.as_ref().is_some_and(|limits| {
            limits.cpu.as_deref().is_some_and(decimal)
                && limits.memory.as_deref().is_some_and(size_ok)
        })
        && size_ok(&vm.root_size)
        && vm
            .mac
            .as_deref()
            .is_none_or(|mac| normalize_mac(mac).as_deref() == Some(mac))
}

/// A virtual machine on a host bridge: the instance alone. Nothing on it is changed after its create.
fn apply_vm(ctx: &mut Ctx<'_>, cfg: &ServiceConfig, vm: &Vm) -> Result<(), PluginError> {
    if cfg.tenant.is_some()
        || cfg.project != PROJECT
        || !host_bridge_ok(&cfg.bridge)
        || cfg.bridge == BRIDGE
    {
        return Err(PluginError::Config(
            "a virtual machine is in the service project, on a host bridge".into(),
        ));
    }
    if !service_name_ok(&cfg.name) || ctx.entry().0 != cfg.entry_id() {
        return Err(PluginError::Config("invalid service name".into()));
    }
    if !vm_record_ok(cfg, vm) {
        return Err(PluginError::Config(
            "invalid virtual machine pool, limits, disk size or MAC".into(),
        ));
    }
    match observed(
        ctx,
        format!("/1.0/instances/{}?project={}", cfg.instance, cfg.project),
    )? {
        None => create(
            ctx,
            IncusKind::Instance,
            &cfg.project,
            &cfg.instance,
            vm_body(cfg, vm),
        ),
        Some(instance)
            if instance["type"].as_str() == Some(VM_KIND)
                && instance["config"][TEMPLATE_KEY].as_str() == Some(cfg.template.as_str())
                && instance["devices"]["eth0"]["parent"].as_str() == Some(cfg.bridge.as_str()) =>
        {
            Ok(())
        }
        Some(_) => Err(PluginError::Config(
            "the existing instance is not the marked virtual machine on its bridge".into(),
        )),
    }
}

/// The ACL's ingress rules: the bridge gateway reaches a declared management port, and each link opens its port.
pub fn ingress(cfg: &ServiceConfig) -> Vec<Value> {
    let mut rules = Vec::new();
    if let Some(port) = cfg.droidd_port {
        rules.push(rule(json!({
            "action": "allow",
            "source": format!("{}/32", cfg.gateway),
            "protocol": "tcp",
            "destination_port": port.to_string(),
            "description": format!("droidd from the host bridge address to port {port}"),
            "state": "enabled"
        })));
    }
    for grant in &cfg.incoming {
        rules.push(rule(json!({"action":"allow","source":format!("{}/32",grant.address),"protocol":"tcp",
            "destination_port":grant.port.to_string(),"description":format!("link from {} to port {}",grant.address,grant.port),"state":"enabled"})));
    }
    // The node's front service reaches every port a tenant's service declares, and nothing else of it.
    if let Some(front) = cfg.tenant.as_ref().and_then(|tenant| tenant.front.as_ref())
        && !cfg.ports.is_empty()
    {
        let ports = cfg
            .ports
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(",");
        rules.push(rule(json!({"action":"allow","source":format!("{front}/32"),"protocol":"tcp",
            "destination_port":ports,"description":"the node's front service to the declared ports","state":"enabled"})));
    }
    rules
}

use arca_netproject::tenant::rule;

/// Reject private ranges outside this service /24 before allowing internet traffic. A tenant's service also rejects
/// the service /24 outside its tenant's block.
pub fn egress(cfg: &ServiceConfig, hosts: &Value) -> Vec<Value> {
    let gateway = cfg.gateway.parse::<Ipv4Addr>().expect("validated gateway");
    let block = match cfg.tenant_block() {
        Some(Ok(block)) => Some(block),
        _ => None,
    };
    // A function reaches only what its definition names: its linked services and its destinations, and no more.
    if let Some(function) = &cfg.function {
        return arca_netproject::tenant::function_egress_rules(
            gateway,
            hosts,
            &cfg.destinations,
            &function.reach,
        );
    }
    arca_netproject::tenant::egress_rules(gateway, hosts, block, &cfg.destinations)
}

/// A tenant's NIC also filters IPv6. The managed bridge has no IPv6 address, so the NIC is given none, and Incus then
/// drops every IPv6 frame the instance sends: one tenant cannot reach another over the bridge's link-local addresses.
/// A service of the node's own project keeps the NIC it always had.
fn tenant_nic(cfg: &ServiceConfig, mut nic: Value) -> Value {
    if cfg.tenant.is_some() {
        nic["security.ipv6_filtering"] = json!("true");
    }
    nic
}

fn instance_body(cfg: &ServiceConfig, links: &[String]) -> Value {
    if let Some(vm) = &cfg.vm {
        return managed_vm_body(cfg, vm, links);
    }
    let mut config = Map::new();
    config.insert(TEMPLATE_KEY.to_string(), json!(cfg.template));
    config.insert(ADDRESS_KEY.to_string(), json!(cfg.ipv4));
    if !cfg.ports.is_empty() {
        let ports = cfg
            .ports
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(",");
        config.insert(PORTS_KEY.to_string(), json!(ports));
    }
    if let Some(published) = &cfg.published {
        config.insert(PUBLISHED_KEY.to_string(), json!(published.node));
    }
    if let Some(entrypoint) = &cfg.entrypoint {
        config.insert("oci.entrypoint".to_string(), json!(entrypoint));
    }
    for (key, value) in &cfg.environment {
        config.insert(format!("environment.{key}"), json!(value));
    }
    if !cfg.environment.is_empty() {
        let keys = cfg
            .environment
            .keys()
            .cloned()
            .collect::<Vec<_>>()
            .join(",");
        config.insert(ENVKEYS_KEY.to_string(), json!(keys));
    }
    if !links.is_empty() {
        config.insert(LINKS_KEY.to_string(), json!(links.join(",")));
    }
    if let Some(limits) = &cfg.limits {
        if let Some(cpu) = &limits.cpu {
            config.insert("limits.cpu".into(), json!(cpu));
        }
        if let Some(memory) = &limits.memory {
            config.insert("limits.memory".into(), json!(memory));
        }
    }
    if cfg.autostart {
        config.insert("boot.autostart".into(), json!("true"));
    }
    if cfg.no_new_privileges {
        config.insert("raw.lxc".into(), json!(NO_NEW_PRIVS_RAW_LXC));
    }
    if cfg.function.is_some() {
        // The mark the writer of a run looks for, and a memory limit that swap does not stretch.
        config.insert(FUNCTION_KEY.into(), json!("true"));
        config.insert("limits.memory.swap".into(), json!("false"));
        // Never started by Incus. With the key unset Incus puts an instance back in the power state it had when the
        // host went down: a function that was running then would be started with nothing watching it, no time
        // limit and no line in the run log.
        config.insert("boot.autostart".into(), json!("false"));
    }
    let mut devices = Map::new();
    if let Some(data) = &cfg.data {
        devices.insert(
            "data".into(),
            json!({"type":"disk", "path":data.path, "pool":cfg.pool,
            "source":format!("{}-data", cfg.name)}),
        );
    }
    if let Some(lan) = &cfg.lan {
        // The wildcard bind exposes this port on every IPv4 interface, including a tailnet interface.
        devices.insert(
            "web".into(),
            json!({"type":"proxy",
            "listen":format!("tcp:0.0.0.0:{}", lan.port),
            "connect":format!("tcp:127.0.0.1:{}", lan.port)}),
        );
    }
    if let Some(link) = &cfg.kernel_access {
        devices.insert(KERNEL_DEVICE.into(), kernel_device(link));
    }
    let mut root = json!({ "type": "disk", "path": "/", "pool": cfg.pool });
    if cfg.tenant.is_some() {
        // The tenant project's disk cap counts a root disk by its size, so a tenant's container states one.
        root["size"] = json!(arca_netproject::tenant::CONTAINER_ROOT_SIZE);
    }
    devices.insert("root".to_string(), root);
    devices.insert(
        "eth0".to_string(),
        tenant_nic(
            cfg,
            json!({
            "type": "nic",
            "name": "eth0",
            "network": cfg.bridge,
            "ipv4.address": cfg.ipv4,
            "security.port_isolation": "false",
            "security.mac_filtering": "true",
            "security.ipv4_filtering": "true",
            "security.acls": cfg.acl(),
            "security.acls.default.ingress.action": "reject",
            "security.acls.default.egress.action": "reject"
            }),
        ),
    );
    json!({
        "name": cfg.instance,
        "type": "container",
        "source": cfg.source,
        "profiles": [PROFILE],
        "config": config,
        "devices": devices,
        "start": cfg.start
    })
}

/// The body of the create of a service on the managed network, exactly as its record gives it. The effect journal
/// records the digest of the create that carries it, so that digest covers every device, a kernel access device included.
pub fn create_body(cfg: &ServiceConfig) -> Value {
    instance_body(
        cfg,
        &cfg.links
            .iter()
            .map(|link| format!("{}:{}:{}", link.to, link.port, link.uuid))
            .collect::<Vec<_>>(),
    )
}

impl Plugin for Service {
    const ID: &'static str = "arca.service";
    type Config = ServiceConfig;

    fn manifest() -> Manifest {
        manifest_for()
    }

    /// The service's NIC names its ACL, and Incus refuses to delete an ACL in use: the ACL is retired after the instance.
    fn retired_after_plan() -> &'static [IncusKind] {
        &[IncusKind::Acl]
    }

    fn apply(ctx: &mut Ctx<'_>, cfg: &ServiceConfig) -> Result<(), PluginError> {
        // An adopted instance is owned through its adopt entry. The service record never writes it.
        if cfg.adopted {
            return Ok(());
        }
        // Kernel access is a container's, in the node's own project: a record that says otherwise is never applied,
        // whatever else it holds.
        if let Some(link) = &cfg.kernel_access
            && (cfg.tenant.is_some()
                || cfg.project != PROJECT
                || cfg.vm.is_some()
                || link.port < KERNEL_ACCESS_PORT_MIN
                || cfg.ports.contains(&link.port)
                || !kernel_connect_ok(&link.connect))
        {
            return Err(PluginError::Config(
                "kernel access is for a container of the node's own service project, on a port of 1024 or above that is not one of its own, to the kernel's services socket".into(),
            ));
        }
        // A destination is for a service of the node's own project on the managed network, whose NIC rules Arca
        // writes: a record that says otherwise is never applied, whatever else it holds.
        if !cfg.destinations.is_empty() && !destinations_ok(cfg) {
            return Err(PluginError::Config(
                "a private destination is for a service of the node's own project on the managed service network: one private or tailnet address outside the service subnet and one tcp port each, at most 4".into(),
            ));
        }
        // A function is a container of the node's own project that only the kernel starts: a record that says
        // otherwise is never applied, whatever else it holds.
        if !crate::function::record_ok(cfg) {
            return Err(PluginError::Config(
                "a function is a container of the node's own service project, created stopped, with no port, and with one opened address for each of its links".into(),
            ));
        }
        if let Some(vm) = &cfg.vm
            && cfg.on_host_bridge()
        {
            return apply_vm(ctx, cfg, vm);
        }
        // From here the service is on the managed network: a container, or a virtual machine with the same address plan
        // and NIC ACL.
        if let Some(vm) = &cfg.vm
            && (!vm_record_ok(cfg, vm) || vm.mac.is_some() || cfg.data.is_some())
        {
            return Err(PluginError::Config(
                "invalid virtual machine pool, limits or disk size".into(),
            ));
        }
        if !cfg.project_ok() || cfg.bridge != BRIDGE {
            return Err(PluginError::Config(
                "the service project and bridge are required".into(),
            ));
        }
        if !service_name_ok(&cfg.name)
            || ctx.entry().0 != cfg.entry_id()
            || cfg.instance != cfg.instance_name()
        {
            return Err(PluginError::Config("invalid service name".into()));
        }
        // A tenant's service takes its address from the tenant's block, and has no host listener.
        if matches!(cfg.tenant_block(), Some(Err(())))
            || (cfg.tenant.is_some() && cfg.lan.is_some())
        {
            return Err(PluginError::Config(
                "a tenant's service has an address in its tenant's block and no host port".into(),
            ));
        }
        if cfg.ipv4.parse::<Ipv4Addr>().is_err()
            || cfg.gateway.parse::<Ipv4Addr>().is_err()
            || cfg.pool.is_empty()
        {
            return Err(PluginError::Config(
                "invalid service address or pool".into(),
            ));
        }
        let acl = cfg.acl();
        let current = observed(ctx, format!("/1.0/network-acls/{acl}?project=default"))?;
        let mut guarded = cfg.clone();
        guarded.incoming.retain(|grant| {
            let Ok(value) = ctx.observe(Observe::IncusGet {
                path: format!(
                    "/1.0/instances/{}?project={}",
                    instance_name(
                        cfg.tenant.as_ref().map(|tenant| tenant.id.as_str()),
                        &grant.consumer
                    ),
                    cfg.project
                ),
            }) else {
                return false;
            };
            let own = ctx
                .observe(Observe::IncusGet {
                    path: format!("/1.0/instances/{}?project={}", cfg.instance, cfg.project),
                })
                .ok();
            value["config"]["volatile.uuid"].as_str() == Some(grant.uuid.as_str())
                && value["devices"]["eth0"]["ipv4.address"].as_str() == Some(grant.address.as_str())
                && own
                    .as_ref()
                    .and_then(|v| v["config"]["volatile.uuid"].as_str())
                    == Some(grant.provider_uuid.as_str())
                && value["config"][LINKS_KEY].as_str().is_some_and(|links| {
                    links.split(',').any(|v| {
                        v == format!("{}:{}:{}", cfg.name, grant.port, grant.provider_uuid)
                    })
                })
        });
        let rules = ingress(&guarded);
        let outgoing = egress(cfg, &ctx.observe(Observe::HostAddresses)?);
        match current {
            None => {
                create(
                    ctx,
                    IncusKind::Acl,
                    "default",
                    &acl,
                    json!({
                        "name": acl,
                        "description": format!("Arca service {} NIC ACL", cfg.name),
                        "config": { COOKIE: "true" },
                        "ingress": rules,
                        "egress": outgoing
                    }),
                )?;
            }
            Some(current) => {
                if current["config"][COOKIE].as_str() != Some("true") {
                    return Err(PluginError::Config(
                        "the existing ACL is not managed by a service".into(),
                    ));
                }
                for (field, key, desired) in [
                    (arca_kernel::journal::AclField::Ingress, "ingress", rules),
                    (arca_kernel::journal::AclField::Egress, "egress", outgoing),
                ] {
                    if !current[key]
                        .as_array()
                        .is_some_and(|rules| arca_incus::acl_rules_match(rules, &desired))
                    {
                        ctx.effect(EffectOp::IncusPatchAclField {
                            object: arca_kernel::ownership::ObjectRef {
                                kind: IncusKind::Acl,
                                project: "default".into(),
                                parent: None,
                                name: acl.clone(),
                                uuid: None,
                            },
                            field,
                            rules: desired,
                        })?;
                    }
                }
            }
        }
        if let Some(data) = &cfg.data {
            let name = format!("{}-data", cfg.name);
            // `observed` refuses a volume that does not carry this entry's mark, so an existing volume is never adopted.
            if observed(
                ctx,
                format!(
                    "/1.0/storage-pools/{}/volumes/custom/{name}?project={}",
                    cfg.pool, cfg.project
                ),
            )?
            .is_none()
            {
                let mut config = Map::new();
                if let Some(uid) = data.uid {
                    config.insert("initial.uid".into(), json!(uid.to_string()));
                }
                if let Some(gid) = data.gid {
                    config.insert("initial.gid".into(), json!(gid.to_string()));
                }
                if let Some(size) = &data.size {
                    config.insert("size".into(), json!(size));
                }
                ctx.effect(EffectOp::IncusCreate {
                    kind: IncusKind::Volume,
                    project: cfg.project.clone(),
                    parent: Some(cfg.pool.clone()),
                    name: name.clone(),
                    body: json!({"name":name, "config":config}),
                })?;
            }
        }
        let instance = observed(
            ctx,
            format!("/1.0/instances/{}?project={}", cfg.instance, cfg.project),
        )?;
        let kind = if cfg.vm.is_some() {
            VM_KIND
        } else {
            "container"
        };
        if let Some(instance) = &instance
            && (instance["type"].as_str().is_some_and(|found| found != kind)
                || instance["config"][TEMPLATE_KEY].as_str() != Some(cfg.template.as_str())
                || instance["config"][ADDRESS_KEY].as_str() != Some(cfg.ipv4.as_str())
                || instance["devices"]["eth0"]["ipv4.address"].as_str() != Some(cfg.ipv4.as_str())
                || instance["devices"]["eth0"]["security.acls"].as_str() != Some(acl.as_str()))
        {
            return Err(PluginError::Config(
                "the existing instance is not the marked service with its NIC ACL".into(),
            ));
        }
        if instance.is_none() {
            create(
                ctx,
                IncusKind::Instance,
                &cfg.project,
                &cfg.instance,
                create_body(cfg),
            )?;
        } else if let Some(instance) = instance {
            let desired = instance_body(
                cfg,
                &cfg.links
                    .iter()
                    .map(|link| format!("{}:{}:{}", link.to, link.port, link.uuid))
                    .collect::<Vec<_>>(),
            );
            let current = instance["config"]
                .as_object()
                .ok_or_else(|| PluginError::Config("instance has no config".into()))?;
            let mut changes = BTreeMap::new();
            let previous = current
                .get(ENVKEYS_KEY)
                .and_then(Value::as_str)
                .unwrap_or("");
            for key in previous.split(',').filter(|key| !key.is_empty()) {
                if cfg
                    .environment_patch
                    .as_ref()
                    .is_none_or(|keys| keys.contains(key))
                    && !cfg.environment.contains_key(key)
                {
                    changes.insert(format!("environment.{key}"), None);
                }
            }
            // `raw.lxc` follows the record's `noNewPrivileges`. While the record says true, `raw.lxc` is Arca's: it is
            // written as `lxc.no_new_privs=1`, replacing any other value. When the record says false, it is removed
            // only if it is exactly that value; any other `raw.lxc` is not Arca's and is left as it is.
            let raw = current.get("raw.lxc").and_then(Value::as_str);
            if cfg.no_new_privileges {
                if raw != Some(NO_NEW_PRIVS_RAW_LXC) {
                    changes.insert("raw.lxc".into(), Some(NO_NEW_PRIVS_RAW_LXC.into()));
                }
            } else if raw == Some(NO_NEW_PRIVS_RAW_LXC) {
                changes.insert("raw.lxc".into(), None);
            }
            for key in [ENVKEYS_KEY, LINKS_KEY, PUBLISHED_KEY] {
                let value = desired["config"]
                    .get(key)
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                if current.get(key).and_then(Value::as_str) != value.as_deref() {
                    changes.insert(key.into(), value);
                }
            }
            for (key, value) in &cfg.environment {
                if cfg
                    .environment_patch
                    .as_ref()
                    .is_some_and(|keys| !keys.contains(key))
                {
                    continue;
                }
                if current
                    .get(&format!("environment.{key}"))
                    .and_then(Value::as_str)
                    != Some(value.as_str())
                {
                    changes.insert(format!("environment.{key}"), Some(value.clone()));
                }
            }
            if !changes.is_empty() {
                let uuid = instance["config"]["volatile.uuid"]
                    .as_str()
                    .ok_or_else(|| PluginError::Config("instance has no UUID".into()))?;
                ctx.effect(EffectOp::IncusPatchConfig {
                    object: arca_kernel::ownership::ObjectRef {
                        kind: IncusKind::Instance,
                        project: cfg.project.clone(),
                        parent: None,
                        name: cfg.instance.clone(),
                        uuid: Some(uuid.into()),
                    },
                    config: changes,
                })?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Template {
    pub name: String,
    pub kind: String,
    pub image: String,
    #[serde(default)]
    pub digest: Option<String>,
    #[serde(default)]
    pub commit: Option<String>,
    #[serde(default)]
    pub ports: Vec<u16>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub provides: Option<Value>,
    #[serde(default)]
    pub secrets: Vec<String>,
    #[serde(default)]
    pub publishable: bool,
    #[serde(default)]
    pub dash_port: Option<u16>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub entrypoint: Option<Vec<Value>>,
    #[serde(default)]
    pub generate: BTreeMap<String, String>,
    #[serde(default)]
    pub droidd_port: Option<u16>,
    #[serde(default)]
    pub limits: Option<Limits>,
    #[serde(default)]
    pub autostart: bool,
    #[serde(default)]
    pub data: Option<Data>,
    #[serde(default)]
    pub lan: Option<Lan>,
    /// The root disk size of a virtual-machine template, such as `20GiB`. No other kind sets it.
    #[serde(default)]
    pub root_size: Option<String>,
    /// A virtual-machine template's first-boot script: `{ "file": "name.sh" }`, a file beside the template that
    /// cloud-init runs once, at the machine's first boot. No other kind sets it.
    #[serde(default)]
    pub first_boot: Option<Value>,
    /// A virtual-machine template's settings: one JSON object the guest reads at [`GUEST_SETTINGS_PATH`]. No other
    /// kind sets it, and it never holds a secret.
    #[serde(default)]
    pub settings: Option<Value>,
    /// The top-level keys of `settings` that a `service_create` may replace for one service. Each is a key of
    /// `settings`, whose value there is the default and gives the type. No other kind sets it.
    #[serde(default)]
    pub create_settings: Vec<String>,
    /// What must hold between a virtual-machine template's settings, for the defaults and for every create: each
    /// rule says one amount is at least another ([`SettingRule`]). The machine's own first-boot script may refuse
    /// the same; stated here, a create that would not start is refused before anything is made. No other kind sets it.
    #[serde(default)]
    pub setting_rules: Vec<SettingRule>,
    /// A container template whose service may talk to this node's kernel: `{ "port": <n> }`, the port inside the
    /// instance where the kernel answers on loopback. Only a service of the node's own project is created from such a
    /// template, never one in a tenant.
    #[serde(default)]
    pub kernel_access: Option<KernelAccess>,
    /// A container template whose service runs with no-new-privileges: a setuid or setcap binary inside it cannot raise
    /// privileges. Off unless the template says `true`. Incus has no key of its own for it, so it is the LXC key
    /// `lxc.no_new_privs` in the instance's `raw.lxc` ([`NO_NEW_PRIVS_RAW_LXC`]). Only a service of the node's own
    /// project is created from such a template, never one in a tenant, whose project refuses `raw.lxc`.
    #[serde(default)]
    pub no_new_privileges: bool,
    /// A virtual-machine template whose service runs a version the template names: `{ "pin": "<a key of settings>" }`.
    /// No other kind sets it.
    #[serde(default)]
    pub update: Option<Update>,
}

/// One rule of a template's `settingRules`: `{ "need": <amount>, "atLeast": <amount>, "why": "<words>" }`. An amount
/// is a whole number, the name of a top-level setting whose value is a whole number, or `{ "count": "<name>" }`, the
/// number of entries of a top-level setting that is a list. The kernel compares the two amounts and knows nothing
/// of what they mean; `why` is the template's own reason, told after the kernel's words when a create is refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SettingRule {
    pub need: Value,
    pub at_least: Value,
    pub why: String,
}

/// One side of a [`SettingRule`].
enum Amount<'a> {
    Number(u64),
    Setting(&'a str),
    Count(&'a str),
}

impl<'a> Amount<'a> {
    /// The amount a rule's side states, or `None` for anything but the three forms.
    fn of(side: &'a Value) -> Option<Self> {
        match side {
            Value::Number(number) => number.as_u64().map(Amount::Number),
            Value::String(name) => Some(Amount::Setting(name)),
            Value::Object(object) if object.len() == 1 => {
                object.get("count")?.as_str().map(Amount::Count)
            }
            _ => None,
        }
    }

    /// Its value in `settings`: `Err` says what is wrong with the setting it names.
    fn value(&self, settings: Option<&Value>) -> Result<i128, String> {
        let setting = |name: &str| {
            settings
                .and_then(|settings| settings.get(name))
                .ok_or_else(|| format!("names {name}, which is not a setting of the template"))
        };
        match self {
            Amount::Number(number) => Ok(i128::from(*number)),
            Amount::Setting(name) => match setting(name)? {
                Value::Number(number) if number.is_u64() || number.is_i64() => Ok(number
                    .as_i64()
                    .map(i128::from)
                    .or(number.as_u64().map(i128::from))
                    .unwrap_or_default()),
                _ => Err(format!("names {name}, whose value is not a whole number")),
            },
            Amount::Count(name) => match setting(name)? {
                Value::Array(entries) => Ok(entries.len() as i128),
                _ => Err(format!("counts {name}, whose value is not a list")),
            },
        }
    }

    /// The amount in the words of a refusal.
    fn words(&self) -> String {
        match self {
            Amount::Number(number) => number.to_string(),
            Amount::Setting(name) => format!("settings.{name}"),
            Amount::Count(name) => format!("the number of entries in settings.{name}"),
        }
    }
}

/// How a [`SettingRule`] stands against one settings object.
enum RuleCheck {
    Holds,
    /// The rule cannot be read against these settings: what is wrong with it.
    Unreadable(String),
    /// The settings do not pass it: the kernel's words, without the template's reason.
    Fails(String),
}

fn check_rule(rule: &SettingRule, settings: Option<&Value>) -> RuleCheck {
    let (Some(need), Some(least)) = (Amount::of(&rule.need), Amount::of(&rule.at_least)) else {
        return RuleCheck::Unreadable(
            "need and atLeast are each a whole number, the name of a setting, or { \"count\": \"<the name of a setting that is a list>\" }".into(),
        );
    };
    if matches!((&need, &least), (Amount::Number(_), Amount::Number(_))) {
        return RuleCheck::Unreadable(
            "need and atLeast are both numbers, so it says nothing about a setting".into(),
        );
    }
    let (have, floor) = match (need.value(settings), least.value(settings)) {
        (Ok(have), Ok(floor)) => (have, floor),
        (Err(why), _) => return RuleCheck::Unreadable(format!("need {why}")),
        (_, Err(why)) => return RuleCheck::Unreadable(format!("atLeast {why}")),
    };
    if have >= floor {
        return RuleCheck::Holds;
    }
    // Said about the setting, not about the number: a rule whose `need` is a number is a ceiling on the other side.
    RuleCheck::Fails(match (&need, &least) {
        (Amount::Number(_), _) => format!("{} is {floor}, above {have}", least.words()),
        (_, Amount::Number(_)) => format!("{} is {have}, below {floor}", need.words()),
        _ => format!(
            "{} is {have}, below {} ({floor})",
            need.words(),
            least.words()
        ),
    })
}

/// The longest name of a setting that a rule may give, and that a refusal shows of a name a create gave.
const SETTING_NAME_MAX: usize = 64;

/// Why a template's `settingRules` are not ones it may state, or `None`: each is readable against the template's own
/// settings, gives a plain reason, and holds for the template's own defaults.
fn setting_rules_refusal(template: &Template) -> Option<String> {
    if template.setting_rules.len() > 16 {
        return Some("settingRules lists at most 16 rules".into());
    }
    template
        .setting_rules
        .iter()
        .enumerate()
        .find_map(|(n, rule)| {
            let why = rule.why.trim();
            if why.is_empty() || rule.why.chars().count() > 200 || !plain_text(&rule.why) {
                return Some(format!(
                    "settingRules[{n}]: why is its reason in plain words, 1 to 200 characters"
                ));
            }
            // A name a rule gives is told back in a refusal, of the template and of a create: it is held to the
            // length and the characters a create's own setting names are shown with, and one that is not is never
            // told back.
            let named = |side: &Value| match Amount::of(side) {
                Some(Amount::Setting(name) | Amount::Count(name)) => {
                    name.is_empty() || name.chars().count() > SETTING_NAME_MAX || !plain_text(name)
                }
                _ => false,
            };
            if named(&rule.need) || named(&rule.at_least) {
                return Some(format!(
                    "settingRules[{n}]: a setting is named in 1 to {SETTING_NAME_MAX} plain characters"
                ));
            }
            match check_rule(rule, template.settings.as_ref()) {
                RuleCheck::Holds => None,
                RuleCheck::Unreadable(what) => Some(format!("settingRules[{n}]: {what}")),
                RuleCheck::Fails(what) => Some(format!(
                    "settingRules[{n}]: the template's own settings do not pass it: {what}"
                )),
            }
        })
}

impl Template {
    pub fn is_vm(&self) -> bool {
        self.kind == VM_KIND
    }

    /// The alias of a template's local image, and the fingerprint the template pins it to, when its image is a local
    /// one (`local:<alias>`) that carries a `digest`. A local image is one somebody put in the service project by
    /// hand; without a pin the alias is trusted for whatever stands under it.
    pub fn local_image_pin(&self) -> Option<(&str, &str)> {
        let alias = self.image.strip_prefix("local:")?;
        let fingerprint = self.digest.as_deref()?.strip_prefix("sha256:")?;
        (self.kind != "oci").then_some((alias, fingerprint))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateError {
    pub not_found: bool,
    pub message: String,
}

impl TemplateError {
    fn refused(message: impl Into<String>) -> Self {
        Self {
            not_found: false,
            message: message.into(),
        }
    }
    fn missing(message: impl Into<String>) -> Self {
        Self {
            not_found: true,
            message: message.into(),
        }
    }
}

pub fn load_template(dir: &Path, name: &str) -> Result<Template, TemplateError> {
    read_template(dir, name).map(|(template, _)| template)
}

/// The template `name` and the text of its file, from one read of it.
fn read_template(dir: &Path, name: &str) -> Result<(Template, String), TemplateError> {
    if !template_name_ok(name) {
        return Err(TemplateError::refused("template: invalid name"));
    }
    let path = dir.join(format!("{name}.json"));
    let text = std::fs::read_to_string(&path).map_err(|_| {
        TemplateError::missing(format!("no template {name} (templates/{name}.json)"))
    })?;
    let template: Template = serde_json::from_str(&text)
        .map_err(|_| TemplateError::refused(format!("template {name}: cannot be read")))?;
    if template.name != name {
        return Err(TemplateError::refused(format!(
            "template {name}: its name is {0}",
            template.name
        )));
    }
    if !template_name_ok(&template.name) {
        return Err(TemplateError::refused(format!(
            "template {name}: invalid name"
        )));
    }
    validate_template(&template, dir)?;
    Ok((template, text))
}

/// A template as this node holds it at one moment: its file and every script it names, each read once, and the
/// fingerprint of exactly those bytes. A create that is made from this value is made from what the fingerprint
/// names, with no second read between the check and the create.
#[derive(Debug, Clone)]
pub struct TemplateContent {
    pub template: Template,
    /// The scripts the template names, by file name, as they were read, or why one could not be.
    scripts: BTreeMap<String, Result<String, std::io::ErrorKind>>,
    /// SHA-256, in hex, of the template's file and of each script it names ([`template_digest`]). `None` when a
    /// script it names could not be read: such a template is no content a card can show.
    pub digest: Option<String>,
}

impl TemplateContent {
    /// The entrypoint, from the scripts read with the template ([`entrypoint`]).
    pub fn entrypoint(&self) -> Result<Option<String>, TemplateError> {
        entrypoint_with(&self.template, &mut |file| self.script(file))
    }

    /// The first-boot script, from the scripts read with the template ([`first_boot`]).
    pub fn first_boot(&self) -> Result<Option<String>, TemplateError> {
        first_boot_with(&self.template, &mut |file| self.script(file))
    }

    /// The fingerprint of the content, or the refusal a create gives for the script that stands in its way: one
    /// that is missing, cannot be read, or is not what its place takes.
    pub fn bound(&self) -> Result<&str, TemplateError> {
        self.entrypoint()?;
        self.first_boot()?;
        self.digest
            .as_deref()
            .ok_or_else(|| TemplateError::refused("a script of the template could not be read"))
    }

    fn script(&self, file: &str) -> std::io::Result<String> {
        match self.scripts.get(file) {
            Some(Ok(script)) => Ok(script.clone()),
            Some(Err(kind)) => Err(std::io::Error::from(*kind)),
            None => Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
        }
    }
}

/// The template `name` with everything it is made of on this node: its file, and the script its entrypoint or its
/// first boot names, each read once. A script that cannot be read does not fail the load: the builder that needs it
/// refuses in its own words, where a create has always met it ([`TemplateContent::entrypoint`],
/// [`TemplateContent::first_boot`]), and the content then has no fingerprint.
pub fn load_template_content(dir: &Path, name: &str) -> Result<TemplateContent, TemplateError> {
    let (template, text) = read_template(dir, name)?;
    let scripts: BTreeMap<String, Result<String, std::io::ErrorKind>> = script_files(&template)
        .into_iter()
        .map(|file| {
            let read = std::fs::read_to_string(dir.join(&file)).map_err(|error| error.kind());
            (file, read)
        })
        .collect();
    let read: Option<BTreeMap<&str, &str>> = scripts
        .iter()
        .map(|(file, script)| Some((file.as_str(), script.as_deref().ok()?)))
        .collect();
    let digest = read.map(|scripts| template_digest(name, &text, &scripts));
    Ok(TemplateContent {
        template,
        scripts,
        digest,
    })
}

/// The plain file names a template names: the `{ "file": ... }` parts of its entrypoint, and its first-boot script.
/// A name that is not a plain file name is left out here and refused by the builder that meets it.
fn script_files(template: &Template) -> BTreeSet<String> {
    let mut files = BTreeSet::new();
    for part in template.entrypoint.iter().flatten() {
        if let Some(file) = part
            .get("file")
            .and_then(Value::as_str)
            .filter(|file| script_name_ok(file))
        {
            files.insert(file.to_string());
        }
    }
    if let Some(file) = template.first_boot.as_ref().and_then(first_boot_file) {
        files.insert(file.to_string());
    }
    files
}

/// The fingerprint of a template as a node holds it: SHA-256 over the bytes of its file and of each script it
/// names, lowest file name first. Every part is framed by its name and its length, so no byte can pass from one part
/// to another, and the bytes are hashed as they stand: any edit, a changed space among them, gives another value.
pub fn template_digest(name: &str, text: &str, scripts: &BTreeMap<&str, &str>) -> String {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(b"arca-template-content-v1\0");
    let mut part = |name: &str, bytes: &[u8]| {
        hash.update((name.len() as u64).to_be_bytes());
        hash.update(name.as_bytes());
        hash.update((bytes.len() as u64).to_be_bytes());
        hash.update(bytes);
    };
    part(&format!("{name}.json"), text.as_bytes());
    for (file, script) in scripts {
        part(file, script.as_bytes());
    }
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The image of the services made from a template, as a proposal binds it and a card shows it: what Incus makes the
/// instance from, `<image>@<digest>` when the template gives a digest (an OCI image by its registry digest, a system
/// or local image by its fingerprint) and the bare `<image>` when it gives none. A virtual machine whose template
/// names a version (`update.pin`) adds ` running <version>`: the machine's own system image first, then what it
/// runs. No image name holds a space, so the two parts are told apart by that word alone.
pub fn template_image(template: &Template) -> String {
    let boots = match &template.digest {
        Some(digest) => format!("{}@{digest}", template.image),
        None => template.image.clone(),
    };
    match template_pin(template).filter(|_| template.is_vm()) {
        Some(version) => format!("{boots}{IMAGE_RUNNING}{version}"),
        None => boots,
    }
}

/// What stands between a machine's own image and the version it runs in [`template_image`].
pub const IMAGE_RUNNING: &str = " running ";

fn validate_template(template: &Template, dir: &Path) -> Result<(), TemplateError> {
    let name = &template.name;
    if template.is_vm() {
        return validate_vm_template(template);
    }
    if template.kind != "oci" && template.kind != "system" {
        return Err(TemplateError::refused(format!(
            "template {name}: kind is oci, system or virtual-machine"
        )));
    }
    if template.root_size.is_some() {
        return Err(TemplateError::refused(format!(
            "template {name}: only a virtual-machine template has a rootSize"
        )));
    }
    if template.first_boot.is_some()
        || template.settings.is_some()
        || !template.create_settings.is_empty()
    {
        return Err(TemplateError::refused(format!(
            "template {name}: only a virtual-machine template has a firstBoot, settings or createSettings"
        )));
    }
    if !template.setting_rules.is_empty() {
        return Err(TemplateError::refused(format!(
            "template {name}: only a virtual-machine template has settingRules"
        )));
    }
    if template.update.is_some() {
        return Err(TemplateError::refused(format!(
            "template {name}: only a virtual-machine template has an update; a container's version is its digest"
        )));
    }
    // A local image may carry a digest, which pins the alias to one image: it has the shape every digest has.
    if template.kind != "oci"
        && template.image.starts_with("local:")
        && template.digest.as_deref().is_some_and(|digest| {
            // Lowercase only: it is compared with the fingerprint as Incus writes it.
            !digest.strip_prefix("sha256:").is_some_and(|hex| {
                hex.len() == 64
                    && hex
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
        })
    {
        return Err(TemplateError::refused(format!(
            "template {name}: the digest of a local image is sha256: and 64 lowercase hex digits, as Incus writes a fingerprint"
        )));
    }
    if template.kind == "oci" && template.digest.is_none() {
        return Err(TemplateError::refused(format!(
            "template {name}: an OCI image needs a digest"
        )));
    }
    if template.ports.len() > 16 {
        return Err(TemplateError::refused(format!(
            "template {name}: at most 16 ports"
        )));
    }
    let mut seen = BTreeSet::new();
    for port in &template.ports {
        if *port == 0 || !seen.insert(*port) {
            return Err(TemplateError::refused(format!(
                "template {name}: ports are distinct and 1..65535"
            )));
        }
    }
    if let Some(limits) = &template.limits
        && ((limits.cpu.is_none() && limits.memory.is_none())
            || limits.cpu.as_deref().is_some_and(|cpu| !decimal(cpu))
            || limits
                .memory
                .as_deref()
                .is_some_and(|memory| !size_ok(memory)))
    {
        return Err(TemplateError::refused(format!(
            "template {name}: invalid limits"
        )));
    }
    if let Some(data) = &template.data
        && (path_is_root(&data.path)
            || data.path.len() > DATA_PATH_MAX
            || !data.path.starts_with('/')
            || data.path.chars().any(|c| c.is_control())
            || data.path.split('/').any(|part| part == "..")
            || data.size.as_deref().is_some_and(|size| !size_ok(size)))
    {
        return Err(TemplateError::refused(format!(
            "template {name}: invalid data path or size"
        )));
    }
    if let Some(lan) = &template.lan
        && !template.ports.contains(&lan.port)
    {
        return Err(TemplateError::refused(format!(
            "template {name}: lan port is one of its ports"
        )));
    }
    if let Some(access) = &template.kernel_access
        && (access.port < KERNEL_ACCESS_PORT_MIN
            || template.ports.contains(&access.port)
            || template.dash_port == Some(access.port))
    {
        return Err(TemplateError::refused(format!(
            "template {name}: kernelAccess port is 1024..65535 and not one of the template's own ports"
        )));
    }
    if template.secrets.len() > 16 {
        return Err(TemplateError::refused(format!(
            "template {name}: at most 16 secrets"
        )));
    }
    for key in &template.secrets {
        if !env_name_ok(key) {
            return Err(TemplateError::refused(format!(
                "template {name}: invalid secret name"
            )));
        }
    }
    if let Some(port) = template.droidd_port
        && !template.ports.contains(&port)
    {
        return Err(TemplateError::refused(format!(
            "template {name}: droiddPort is one of its ports"
        )));
    }
    if template.entrypoint.is_some() && template.kind != "oci" {
        return Err(TemplateError::refused(format!(
            "template {name}: only an OCI template has an entrypoint"
        )));
    }
    for (key, prefix) in &template.generate {
        if !template.secrets.contains(key) {
            return Err(TemplateError::refused(format!(
                "template {name}: a generated key is a declared secret"
            )));
        }
        if prefix.len() > 16
            || !prefix
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err(TemplateError::refused(format!(
                "template {name}: a generated prefix is plain"
            )));
        }
    }
    if template.publishable && template.dash_port.is_none() {
        return Err(TemplateError::refused(format!(
            "template {name}: a publishable template names dashPort"
        )));
    }
    if template.publishable && template.kind == "oci" {
        return Err(TemplateError::refused(format!(
            "template {name}: an OCI template is not publishable"
        )));
    }
    if let Some(description) = &template.description
        && description.chars().count() > 500
    {
        return Err(TemplateError::refused(format!(
            "template {name}: description is at most 500 characters"
        )));
    }
    let _ = dir;
    Ok(())
}

/// A virtual-machine template: a system image, both limits and a root disk size. It may declare `ports` and `provides`,
/// which a link from a container opens in the NIC ACL of the managed service network; a create on a host bridge refuses
/// such a template. It may carry a first-boot script and settings, which cloud-init reads at the first boot, and
/// declare `secrets`, which are written into the guest as files at each start. A lan proxy, a tailnet dash, environment
/// and generated secrets depend on a container's devices, image or environment, and no virtual machine has them.
fn validate_vm_template(template: &Template) -> Result<(), TemplateError> {
    let name = &template.name;
    let refused = |why: &str| Err(TemplateError::refused(format!("template {name}: {why}")));
    if !template.limits.as_ref().is_some_and(|l| {
        l.cpu.as_deref().is_some_and(decimal) && l.memory.as_deref().is_some_and(size_ok)
    }) {
        return refused("a virtual machine needs limits with both cpu and memory");
    }
    if !template.root_size.as_deref().is_some_and(size_ok) {
        return refused("a virtual machine needs a rootSize such as 20GiB");
    }
    if template.ports.len() > 16 {
        return refused("at most 16 ports");
    }
    let mut seen = BTreeSet::new();
    if !template
        .ports
        .iter()
        .all(|port| *port != 0 && seen.insert(*port))
    {
        return refused("ports are distinct and 1..65535");
    }
    if template.provides.is_some() && template.ports.is_empty() {
        return refused("provides needs a port to offer");
    }
    if !template.env.is_empty()
        || !template.generate.is_empty()
        || template.entrypoint.is_some()
        || template.publishable
        || template.dash_port.is_some()
        || template.droidd_port.is_some()
        || template.data.is_some()
        || template.lan.is_some()
        || template.kernel_access.is_some()
    {
        return refused(
            "a virtual machine has no env, generate, entrypoint, publishable, dashPort, droiddPort, data, lan or kernelAccess: they need a container's environment, image or devices",
        );
    }
    if template.no_new_privileges {
        return refused(
            "a virtual machine has no noNewPrivileges: it is the LXC config of a container",
        );
    }
    if template.secrets.len() > 16 {
        return refused("at most 16 secrets");
    }
    let mut named = BTreeSet::new();
    if !template
        .secrets
        .iter()
        .all(|key| env_name_ok(key) && named.insert(key.as_str()))
    {
        return refused("invalid secret name");
    }
    if let Some(first_boot) = &template.first_boot
        && first_boot_file(first_boot).is_none()
    {
        return refused(
            "firstBoot is { \"file\": \"name\" }, a plain file name in the templates directory: letters, digits, dots, dashes and underscores, not starting with a dot",
        );
    }
    if let Some(settings) = &template.settings
        && !(settings.is_object()
            && serde_json::to_string(settings).is_ok_and(|text| text.len() <= SETTINGS_MAX))
    {
        return refused("settings is one JSON object of at most 16KiB");
    }
    let mut listed = BTreeSet::new();
    if template.create_settings.len() > 32
        || !template.create_settings.iter().all(|key| {
            listed.insert(key.as_str())
                && template
                    .settings
                    .as_ref()
                    .is_some_and(|settings| settings.get(key).is_some())
        })
    {
        return refused(
            "createSettings lists at most 32 distinct keys of settings, each with its default there",
        );
    }
    if let Some(why) = setting_rules_refusal(template) {
        return refused(&why);
    }
    if let Some(update) = &template.update
        && (listed.contains(update.pin.as_str())
            || !template
                .settings
                .as_ref()
                .and_then(|settings| settings.get(&update.pin))
                .and_then(Value::as_str)
                .is_some_and(image_by_digest))
    {
        return refused(
            "update.pin names a key of settings that createSettings does not list, whose value is an image by digest: <registry>/<name>@sha256:<64 hex digits>",
        );
    }
    if let Some(description) = &template.description
        && description.chars().count() > 500
    {
        return refused("description is at most 500 characters");
    }
    source(template).map(|_| ())
}

/// The file a `firstBoot` value names, when it is `{ "file": "<a plain file name>" }` and nothing else.
fn first_boot_file(first_boot: &Value) -> Option<&str> {
    let object = first_boot.as_object().filter(|object| object.len() == 1)?;
    object
        .get("file")
        .and_then(Value::as_str)
        .filter(|file| script_name_ok(file))
}

/// A virtual-machine template's first-boot script, read from the template directory as an entrypoint script is: the
/// text cloud-init is given as user data, so it begins `#!` (a script, run once at the first boot) or `#cloud-config`.
/// `None` for a template without one.
pub fn first_boot(template: &Template, dir: &Path) -> Result<Option<String>, TemplateError> {
    first_boot_with(template, &mut |file| {
        std::fs::read_to_string(dir.join(file))
    })
}

/// [`first_boot`], with the script given by `read`: the file itself, or what was read with the template.
fn first_boot_with(
    template: &Template,
    read: &mut dyn FnMut(&str) -> std::io::Result<String>,
) -> Result<Option<String>, TemplateError> {
    let Some(first_boot) = &template.first_boot else {
        return Ok(None);
    };
    let name = &template.name;
    let file = first_boot_file(first_boot)
        .ok_or_else(|| TemplateError::refused(format!("template {name}: invalid firstBoot")))?;
    let script = read(file).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            TemplateError::refused(format!(
                "template {name}: its first-boot script is not installed (templates/{file} does not exist). \
                 The script belongs beside templates/{name}.json in the state directory; for a template Arca ships, \
                 `arca templates install --config CONFIG_FILE` (this node's config in place of CONFIG_FILE) puts the template and its script there"
            ))
        } else {
            TemplateError::refused(format!(
                "template {name}: its first-boot script templates/{file} cannot be read ({})",
                error.kind()
            ))
        }
    })?;
    if script.len() > FIRST_BOOT_MAX
        || script.contains('\0')
        || !(script.starts_with("#!") || script.starts_with("#cloud-config"))
        || script.starts_with(SECRET_PREFIX)
    {
        return Err(TemplateError::refused(format!(
            "template {name}: its first-boot script templates/{file} is not cloud-init user data: it begins #! or #cloud-config and is at most 256KiB"
        )));
    }
    Ok(Some(script))
}

/// The cloud-init vendor data that writes a virtual-machine template's `settings` to [`GUEST_SETTINGS_PATH`], readable
/// by every user of the guest: one `write_files` entry whose content is base64, so no setting can change the YAML
/// around it. `None` for a template without settings.
pub fn settings_vendor_data(template: &Template) -> Result<Option<String>, TemplateError> {
    vendor_data_of(&template.name, template.settings.as_ref())
}

/// [`settings_vendor_data`] for one settings object: a template's own, or a service's merged one.
pub fn vendor_data_of(
    template: &str,
    settings: Option<&Value>,
) -> Result<Option<String>, TemplateError> {
    use base64::Engine as _;
    let Some(settings) = settings else {
        return Ok(None);
    };
    let text = serde_json::to_string(settings)
        .ok()
        .filter(|text| settings.is_object() && text.len() <= SETTINGS_MAX)
        .ok_or_else(|| TemplateError::refused(format!("template {template}: invalid settings")))?;
    let content = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    Ok(Some(format!(
        "#cloud-config\nwrite_files:\n- path: {GUEST_SETTINGS_PATH}\n  permissions: '0644'\n  encoding: b64\n  content: {content}\n"
    )))
}

/// The settings of one new service, as [`create_settings`] works them out.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ServiceSettings {
    /// What the guest reads: the template's settings with the create's values in place of the defaults. `None` for
    /// a template without settings.
    pub merged: Option<Value>,
    /// The values the create replaced, by key. `None` when it gave none.
    pub replaced: Option<Map<String, Value>>,
}

/// The settings of one new service: the template's, with the values its `service_create` gave in `given` put in place
/// of the defaults. Every refusal is in plain words and names the setting, down to the element of a list:
///
/// - only a key the template lists in `createSettings` may be given;
/// - a value has the shape of the template's default ([`setting_refusal`]): the same JSON type at every level, each
///   element of a list like the default's first element, and no key the default does not have;
/// - a number is one that reads back the same from the canonical form an approval is made over: a whole number
///   below 2^53 written without a fraction, or a number with a fraction. A whole number whose default is not negative
///   is not negative;
/// - no string or key holds a control character or a character that reorders or hides text (the bidirectional
///   controls, zero-width characters), and a value nests at most eight deep;
/// - where the default is an `https://` address the value is one too, with a plain host and path, on the default's
///   own host: a create changes the path only. Such a value decides where the machine sends a credential;
/// - where the default is a size such as `4GiB` the value is one too, above zero and of at most six digits, and under
///   a key named `memory` at most the machine's own memory (the template's `limits.memory`);
/// - the merged object stays within the size a template's settings may have.
pub fn create_settings(
    template: &Template,
    given: Option<&Value>,
) -> Result<ServiceSettings, TemplateError> {
    let name = &template.name;
    let refused = |why: String| Err(TemplateError::refused(why));
    let unchanged = || ServiceSettings {
        merged: template.settings.clone(),
        replaced: None,
    };
    let Some(given) = given else {
        return Ok(unchanged());
    };
    let Some(given) = given.as_object() else {
        return refused("settings: expected an object of setting names and values".into());
    };
    if given.is_empty() {
        return Ok(unchanged());
    }
    let Some(defaults) = template
        .settings
        .as_ref()
        .and_then(Value::as_object)
        .filter(|_| template.is_vm() && !template.create_settings.is_empty())
    else {
        return refused(format!(
            "settings: template {name} lets a create set none (its createSettings lists none)"
        ));
    };
    let mut merged = defaults.clone();
    for (key, value) in given {
        let Some(default) = defaults
            .get(key)
            .filter(|_| template.create_settings.iter().any(|listed| listed == key))
        else {
            let shown: String = key
                .chars()
                .filter(|c| plain_text(&c.to_string()))
                .take(SETTING_NAME_MAX)
                .collect();
            return refused(format!(
                "settings.{shown}: template {name} does not let a create set it; it lets a create set {}",
                template.create_settings.join(", ")
            ));
        };
        if let Some(why) =
            setting_refusal(&format!("settings.{key}"), key, value, default, template, 0)
        {
            return refused(why);
        }
        merged.insert(key.clone(), value.clone());
    }
    let merged = Value::Object(merged);
    if serde_json::to_string(&merged).map_or(true, |text| text.len() > SETTINGS_MAX) {
        return refused(format!(
            "settings: with these values the settings of template {name} pass 16KiB"
        ));
    }
    // What the template says must hold between its settings, asked of the values this service would have. The
    // kernel's words say which amounts, the template's own say why.
    for rule in &template.setting_rules {
        match check_rule(rule, Some(&merged)) {
            RuleCheck::Holds => {}
            RuleCheck::Fails(what) => {
                return refused(format!("settings: {what}: {}", rule.why.trim()));
            }
            // A value of another type than its default was refused above, so a rule the template passed when it
            // was read is readable here too.
            RuleCheck::Unreadable(what) => {
                return refused(format!("settings: a rule of template {name} {what}"));
            }
        }
    }
    Ok(ServiceSettings {
        merged: Some(merged),
        replaced: Some(given.clone()),
    })
}

/// The JSON type of a value, in the words a refusal uses. A whole number and a number with a fraction are two types:
/// a setting that counts something is not given `2.5`.
fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "true or false",
        Value::Number(number) if number.is_u64() || number.is_i64() => "a whole number",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}

/// Why `value` cannot stand in place of `default` in a template's settings, or `None`. `at` names the place for the
/// refusal (`settings.classes[2].memory`) and `key` is the name the value is under. See [`create_settings`] for the
/// rules; each is checked here at every level of the value.
fn setting_refusal(
    at: &str,
    key: &str,
    value: &Value,
    default: &Value,
    template: &Template,
    depth: usize,
) -> Option<String> {
    if depth > 8 {
        return Some(format!("{at}: a value nests at most eight deep"));
    }
    let kind = json_kind(default);
    if json_kind(value) != kind {
        return Some(format!(
            "{at}: expected {kind}, the type of the template's default"
        ));
    }
    match (value, default) {
        (Value::Number(number), Value::Number(default)) => {
            if !number_reads_back(number) {
                return Some(format!(
                    "{at}: a number is a whole number below 2^53 written without a fraction, or a number with a fraction: anything else does not read back the same when the create is approved"
                ));
            }
            if default.as_i64().is_some_and(|default| default >= 0) && number.as_u64().is_none() {
                return Some(format!("{at}: expected a whole number of 0 or more"));
            }
            None
        }
        (Value::String(text), Value::String(default)) => {
            if !plain_text(text) {
                return Some(format!(
                    "{at}: a value holds no control character and no character that reorders or hides text"
                ));
            }
            if default.starts_with("https://") {
                if !https_address_ok(text) {
                    return Some(format!(
                        "{at}: expected an https address like the template's default, https://<host>/<path>, with a plain host name and path: no user or password (@), no query (?), no fragment (#) and nothing percent-encoded"
                    ));
                }
                let host = |address: &str| {
                    address["https://".len()..]
                        .split('/')
                        .next()
                        .unwrap_or("")
                        .to_ascii_lowercase()
                };
                if host(text) != host(default) {
                    return Some(format!(
                        "{at}: its host is {}, as in the template's default: a create changes the path only, since the machine sends its credential to that host",
                        host(default)
                    ));
                }
            }
            if size_ok(default) {
                let machine = template
                    .limits
                    .as_ref()
                    .and_then(|limits| limits.memory.as_deref())
                    .filter(|_| key == "memory");
                let digits = text.bytes().take_while(u8::is_ascii_digit).count();
                let fits = size_ok(text)
                    && digits <= 6
                    && size_bytes(text).is_some_and(|bytes| {
                        machine
                            .and_then(size_bytes)
                            .is_none_or(|machine| bytes <= machine)
                    });
                if !fits {
                    return Some(match machine {
                        Some(machine) => format!(
                            "{at}: expected a size such as {default}, above zero and at most the machine's memory ({machine})"
                        ),
                        None => format!(
                            "{at}: expected a size such as {default}, above zero and of at most six digits"
                        ),
                    });
                }
            }
            None
        }
        (Value::Array(items), Value::Array(defaults)) => {
            items.iter().enumerate().find_map(|(index, item)| {
                let at = format!("{at}[{}]", index + 1);
                match defaults.first() {
                    // Every element is like the default's first one.
                    Some(default) => setting_refusal(&at, key, item, default, template, depth + 1),
                    // A default with no element says nothing about one: the element is still plain.
                    None => (!plain_json(item, depth + 1)).then(|| {
                        format!(
                            "{at}: a value holds no control character and no character that reorders or hides text, a number that reads back the same, and nests at most eight deep"
                        )
                    }),
                }
            })
        }
        (Value::Object(map), Value::Object(defaults)) => map.iter().find_map(|(name, item)| {
            if !plain_text(name) {
                return Some(format!(
                    "{at}: a key holds no control character and no character that reorders or hides text"
                ));
            }
            match defaults.get(name) {
                Some(default) => setting_refusal(
                    &format!("{at}.{name}"),
                    name,
                    item,
                    default,
                    template,
                    depth + 1,
                ),
                None => Some(format!(
                    "{at}.{name}: the template's default has no such key (it has {})",
                    defaults.keys().cloned().collect::<Vec<_>>().join(", ")
                )),
            }
        }),
        _ => None,
    }
}

/// Whether a number reads back the same from the canonical JSON an approval's digest is made over. That form writes a
/// number as JavaScript does: `2.0` is written `2` and reads back as a whole number, and a whole number of 2^53 or
/// more is rounded to a double. Either would make the approved arguments differ from the proposed ones.
fn number_reads_back(number: &serde_json::Number) -> bool {
    const SAFE: u64 = (1 << 53) - 1;
    if let Some(whole) = number.as_u64() {
        return whole <= SAFE;
    }
    if let Some(whole) = number.as_i64() {
        return whole.unsigned_abs() <= SAFE;
    }
    number
        .as_f64()
        .is_some_and(|float| float.is_finite() && float.fract() != 0.0)
}

/// Whether `text` holds no control character and no character that reorders, joins or hides the text around it: the
/// bidirectional marks, embeddings, overrides and isolates, the zero-width characters, the line and paragraph
/// separators, the byte-order mark and the invisible operators. An owner reads these values on a card before
/// approving, so what is shown must be what is there.
pub(crate) fn plain_text(text: &str) -> bool {
    !text.chars().any(|c| {
        c.is_control()
            || matches!(
                c,
                '\u{00AD}'
                    | '\u{061C}'
                    | '\u{180E}'
                    | '\u{200B}'..='\u{200F}'
                    | '\u{2028}'..='\u{202E}'
                    | '\u{2060}'..='\u{206F}'
                    | '\u{FEFF}'
                    | '\u{FFF9}'..='\u{FFFB}'
                    | '\u{E0000}'..='\u{E007F}'
            )
    })
}

/// Whether every string and key in `value` is [`plain_text`], every number reads back the same, and it nests at most
/// eight deep.
fn plain_json(value: &Value, depth: usize) -> bool {
    match value {
        Value::String(text) => plain_text(text),
        Value::Number(number) => number_reads_back(number),
        Value::Array(items) => depth < 8 && items.iter().all(|item| plain_json(item, depth + 1)),
        Value::Object(map) => {
            depth < 8
                && map
                    .iter()
                    .all(|(key, item)| plain_text(key) && plain_json(item, depth + 1))
        }
        _ => true,
    }
}

/// Whether `text` is `https://<host>[:<port>][/<path>]` with a plain host and path. The host is letters, digits, dots
/// and dashes, beginning and ending with a letter or a digit; the path is letters, digits and `. _ ~ - /`. So there is
/// no `@` (a user or password, which would make the part before it something else than the host), no `?`, no `#`, no
/// `%`, no space and no backslash.
pub fn https_address_ok(text: &str) -> bool {
    let Some(rest) = text.strip_prefix("https://") else {
        return false;
    };
    let (authority, path) = match rest.find('/') {
        Some(at) => rest.split_at(at),
        None => (rest, ""),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (authority, None),
    };
    let edge = |byte: Option<u8>| byte.is_some_and(|b| b.is_ascii_alphanumeric());
    text.len() <= 2048
        && (1..=253).contains(&host.len())
        && edge(host.bytes().next())
        && edge(host.bytes().last())
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
        && !host.contains("..")
        && port.is_none_or(|port| {
            (1..=5).contains(&port.len()) && port.bytes().all(|b| b.is_ascii_digit())
        })
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'~' | b'-' | b'/'))
        && !path.contains("//")
        && !path.split('/').any(|part| part == ".." || part == ".")
}

/// Whether the path is the root once repeated separators and `.` segments are dropped.
fn path_is_root(path: &str) -> bool {
    path.split('/').all(|part| part.is_empty() || part == ".")
}

fn decimal(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|first| matches!(first, b'1'..=b'9'))
        && bytes.all(|b| b.is_ascii_digit())
}

pub fn size_ok(value: &str) -> bool {
    [
        "B", "kB", "MB", "GB", "TB", "PB", "EB", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB",
    ]
    .iter()
    .any(|unit| value.strip_suffix(unit).is_some_and(decimal))
}

/// The bytes a size string names, as Incus reads its units: `kB` to `EB` are powers of 1000, `KiB` to `EiB` powers of
/// 1024. `None` for a string [`size_ok`] refuses, or a number too large to hold.
pub fn size_bytes(value: &str) -> Option<u128> {
    const UNITS: [(&str, u128); 13] = [
        ("KiB", 1 << 10),
        ("MiB", 1 << 20),
        ("GiB", 1 << 30),
        ("TiB", 1 << 40),
        ("PiB", 1 << 50),
        ("EiB", 1 << 60),
        ("kB", 1_000),
        ("MB", 1_000_000),
        ("GB", 1_000_000_000),
        ("TB", 1_000_000_000_000),
        ("PB", 1_000_000_000_000_000),
        ("EB", 1_000_000_000_000_000_000),
        ("B", 1),
    ];
    let (number, scale) = UNITS
        .iter()
        .find_map(|(unit, scale)| Some((value.strip_suffix(unit)?, *scale)))?;
    if !decimal(number) {
        return None;
    }
    number.parse::<u128>().ok()?.checked_mul(scale)
}

fn env_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Whether `text` is an image by digest, as a guest's keeper takes one: `<registry>/<name>@sha256:<64 hex digits>`, in
/// lower case.
pub(crate) fn image_by_digest(text: &str) -> bool {
    let Some((name, digest)) = text.split_once("@sha256:") else {
        return false;
    };
    name.len() <= 255
        && name.contains('/')
        && name
            .bytes()
            .next()
            .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._/:-".contains(&byte)
        })
        && digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// The version an OCI source names: `<registry>/<repository>@sha256:<digest>`. `None` for any other source.
fn source_pin(source: &Value) -> Option<String> {
    if source["protocol"].as_str() != Some("oci") {
        return None;
    }
    let server = source["server"].as_str()?.strip_prefix("https://")?;
    let alias = source["alias"].as_str()?;
    Some(format!("{server}/{alias}"))
}

/// The version a template names for the services made from it: an OCI template's image and digest, or the image by
/// digest under a virtual-machine template's `update.pin`. `None` for a template that names none: a system image, or
/// a virtual machine without `update`.
pub fn template_pin(template: &Template) -> Option<String> {
    if template.is_vm() {
        let key = &template.update.as_ref()?.pin;
        return template
            .settings
            .as_ref()?
            .get(key)?
            .as_str()
            .map(str::to_owned);
    }
    source_pin(&source(template).ok()?)
}

/// The version a service was created with, from its own record: a container's OCI source, or what its machine's
/// settings hold under the key its template's `update.pin` names. The record keeps both from the create, so a template
/// edited since changes nothing here. `None` for an adopted instance, a system image, and a machine whose installed
/// template names no such key.
pub fn created_pin(cfg: &ServiceConfig, template: Option<&Template>) -> Option<String> {
    if cfg.adopted {
        return None;
    }
    match &cfg.vm {
        Some(vm) => {
            let key = &template?.update.as_ref()?.pin;
            vm.guest_settings()?.get(key)?.as_str().map(str::to_owned)
        }
        None => source_pin(&cfg.source),
    }
}

/// What `service_status` answers under `update`: the version the service was created with (`pin`), the one its
/// installed template names now (`templatePin`), and whether the two are the `same` or the template's `differs`. Two
/// digests carry no order, so nothing here says which is newer. `unknown`, with the reason in `problem`, when the
/// installed template cannot say: it is gone, cannot be read, or is of another kind now. `None` for a service whose
/// own version is not known. Nothing is asked of any registry: both come from this node's own files and records.
pub fn update_view(
    cfg: &ServiceConfig,
    template: Result<&Template, &TemplateError>,
) -> Option<Value> {
    let pin = created_pin(cfg, template.ok())?;
    let unknown =
        |problem: String| Some(json!({ "pin": pin, "state": "unknown", "problem": problem }));
    let template = match template {
        Ok(template) => template,
        Err(error) if error.not_found => {
            return unknown(format!(
                "template {} is not installed on this node any more",
                cfg.template
            ));
        }
        Err(_) => {
            return unknown(format!(
                "template {} on this node cannot be used as it is",
                cfg.template
            ));
        }
    };
    let Some(now) = template_pin(template).filter(|_| template.is_vm() == cfg.vm.is_some()) else {
        return unknown(format!(
            "template {} on this node is of another kind now, or names no version",
            cfg.template
        ));
    };
    let state = if now == pin { "same" } else { "differs" };
    Some(json!({ "pin": pin, "templatePin": now, "state": state }))
}

/// The Incus source for a template: an OCI image by digest, a system image by alias or fingerprint.
pub fn source(template: &Template) -> Result<Value, TemplateError> {
    if template.kind == "oci" {
        let digest = template
            .digest
            .as_deref()
            .and_then(|d| d.strip_prefix("sha256:"))
            .filter(|d| d.len() == 64 && d.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or_else(|| TemplateError::refused("invalid digest"))?;
        let (server, repository) = template
            .image
            .split_once('/')
            .filter(|(server, repo)| !server.is_empty() && !repo.is_empty())
            .ok_or_else(|| TemplateError::refused("invalid OCI image"))?;
        let alias = format!("{repository}@sha256:{digest}");
        return Ok(json!({
            "type": "image",
            "mode": "pull",
            "protocol": "oci",
            "server": format!("https://{server}"),
            "alias": alias
        }));
    }
    let (kind, alias) = template
        .image
        .split_once(':')
        .filter(|(_kind, alias)| !alias.is_empty())
        .ok_or_else(|| TemplateError::refused("invalid system image"))?;
    match kind {
        // A pinned local image is named to Incus by its fingerprint as well. When a source carries a fingerprint Incus
        // uses it and never looks the alias up, so the fingerprint decides which image the instance is made from and
        // the alias is only a name kept in the record.
        "local" => Ok(match template.local_image_pin() {
            Some((alias, fingerprint)) => {
                json!({ "type": "image", "alias": alias, "fingerprint": fingerprint })
            }
            None => json!({ "type": "image", "alias": alias }),
        }),
        "images" => {
            let mut source = json!({
                "type": "image",
                "mode": "pull",
                "protocol": "simplestreams",
                "server": "https://images.linuxcontainers.org",
                "alias": alias
            });
            if let Some(digest) = &template.digest {
                let fingerprint = digest
                    .strip_prefix("sha256:")
                    .filter(|d| d.len() == 64 && d.bytes().all(|b| b.is_ascii_hexdigit()))
                    .ok_or_else(|| TemplateError::refused("invalid digest"))?;
                source["fingerprint"] = json!(fingerprint);
            }
            Ok(source)
        }
        _ => Err(TemplateError::refused("invalid system image")),
    }
}

/// The bridge gateway of a service /24.
pub fn gateway(cidr: &str) -> Result<Ipv4Addr, TemplateError> {
    arca_netproject::check_cidr(cidr, &[]).map_err(TemplateError::refused)
}

/// `.10` through `.239` of a service /24.
pub fn candidates(cidr: &str) -> Result<Vec<Ipv4Addr>, TemplateError> {
    let gateway = gateway(cidr)?;
    let base = u32::from(gateway) & 0xffff_ff00;
    Ok((10..=239)
        .map(|last| Ipv4Addr::from(base | last))
        .filter(|ip| *ip != gateway)
        .collect())
}

/// The rendered environment and whether every value the template needs is present.
pub fn environment(
    template: &Template,
    owner: &str,
    name: &str,
    ipv4: &str,
    gateway: &str,
    secrets_dir: &Path,
) -> Result<(BTreeMap<String, String>, bool), TemplateError> {
    let mut start = true;
    let mut references = BTreeMap::new();
    for key in &template.secrets {
        let file = secret_file(secrets_dir, owner, key);
        if let Some(reference) = file
            .as_ref()
            .filter(|path| path.is_file())
            .map(|_| secret_reference(owner, key))
        {
            references.insert(key.clone(), reference);
            continue;
        }
        if let Some(prefix) = template.generate.get(key) {
            let value = generated_secret(prefix)?;
            write_secret(
                file.as_ref()
                    .ok_or_else(|| TemplateError::refused("invalid secret name"))?,
                &value,
            )?;
            references.insert(key.clone(), secret_reference(owner, key));
        }
    }
    let mut env = BTreeMap::new();
    for (key, value) in &template.env {
        match render(
            value,
            name,
            ipv4,
            gateway,
            template.ports.first().copied(),
            &references,
        ) {
            Some(value) => {
                env.insert(key.clone(), value);
            }
            None => start = false,
        }
    }
    Ok((env, start))
}

/// Why a new service of this template is created stopped whatever the call's `start` says, in words that finish
/// "it is created stopped because ...". `None` when a create starts it. A virtual machine whose template declares
/// secrets is one: they are written into its guest by `instance_start`. A container is one while its environment names
/// a secret that is neither stored for the entry `owner` nor made by the template. Nothing is read but whether each
/// secret's file exists, and nothing is written: a secret is named, never shown.
pub fn template_stopped_reason(
    template: &Template,
    owner: &str,
    secrets_dir: &Path,
) -> Option<String> {
    let named = |keys: &[&str]| match keys {
        [one] => format!("the secret {one}"),
        keys => format!("the secrets {}", keys.join(", ")),
    };
    if template.is_vm() {
        if template.secrets.is_empty() {
            return None;
        }
        let keys: Vec<&str> = template.secrets.iter().map(String::as_str).collect();
        return Some(format!(
            "its template declares {}, and a virtual machine's secrets are written into it by instance_start",
            named(&keys)
        ));
    }
    let mut missing = BTreeSet::new();
    for pattern in template.env.values() {
        let mut rest = pattern.as_str();
        while let Some(at) = rest.find("{{secret:") {
            let after = &rest[at + "{{secret:".len()..];
            let Some(end) = after.find("}}") else { break };
            let key = &after[..end];
            let held = template.secrets.iter().any(|declared| declared == key)
                && (template.generate.contains_key(key)
                    || secret_file(secrets_dir, owner, key).is_some_and(|file| file.is_file()));
            if !held {
                missing.insert(key);
            }
            rest = &after[end + 2..];
        }
    }
    if missing.is_empty() {
        return None;
    }
    let keys: Vec<&str> = missing.into_iter().collect();
    Some(format!(
        "its template needs {}, which {} not set yet",
        named(&keys),
        if keys.len() == 1 { "is" } else { "are" }
    ))
}

/// The reference to the secret `key` of the service whose entry is `owner` ([`entry_id`]).
pub fn secret_reference(owner: &str, key: &str) -> String {
    format!("{SECRET_PREFIX}{owner}/{}", secret_name(key))
}

/// A secret's file name: lowercase, with `_` mapped to `-`, so it is inside the provider's secret-name alphabet.
pub fn secret_name(key: &str) -> String {
    key.to_ascii_lowercase().replace('_', "-")
}

pub fn secret_file(secrets_dir: &Path, owner: &str, key: &str) -> Option<PathBuf> {
    if !env_name_ok(key) {
        return None;
    }
    Some(secrets_dir.join(owner).join(secret_name(key)))
}

fn generated_secret(prefix: &str) -> Result<String, TemplateError> {
    generated_secret_with(prefix, |bytes| getrandom::fill(bytes).map_err(|_| ()))
}

fn generated_secret_with(
    prefix: &str,
    mut fill: impl FnMut(&mut [u8]) -> Result<(), ()>,
) -> Result<String, TemplateError> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut out = String::with_capacity(prefix.len() + 32);
    out.push_str(prefix);
    while out.len() < prefix.len() + 32 {
        let mut bytes = [0u8; 32];
        fill(&mut bytes).map_err(|_| TemplateError::refused("no generated key"))?;
        for byte in bytes {
            if byte < 248 && out.len() < prefix.len() + 32 {
                out.push(ALPHABET[byte as usize % ALPHABET.len()] as char);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_templates_fingerprint_is_of_its_bytes_as_they_stand_and_no_byte_passes_between_parts() {
        let digest = |name: &str, text: &str, scripts: &[(&str, &str)]| {
            template_digest(name, text, &scripts.iter().copied().collect())
        };
        let base = digest("app", "{\"name\":\"app\"}", &[("app.sh", "exec app\n")]);
        assert_eq!(base.len(), 64);
        assert!(
            base.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
        assert_eq!(
            base,
            digest("app", "{\"name\":\"app\"}", &[("app.sh", "exec app\n")])
        );
        // Any edit gives another value: a space in the file, a byte in the script, a script more or less, another
        // name for the template or for the script.
        for other in [
            digest("app", "{\"name\": \"app\"}", &[("app.sh", "exec app\n")]),
            digest("app", "{\"name\":\"app\"}\n", &[("app.sh", "exec app\n")]),
            digest("app", "{\"name\":\"app\"}", &[("app.sh", "exec app")]),
            digest("app", "{\"name\":\"app\"}", &[]),
            digest(
                "app",
                "{\"name\":\"app\"}",
                &[("app.sh", "exec app\n"), ("b.sh", "")],
            ),
            digest("apq", "{\"name\":\"app\"}", &[("app.sh", "exec app\n")]),
            digest("app", "{\"name\":\"app\"}", &[("apq.sh", "exec app\n")]),
        ] {
            assert_ne!(other, base);
        }
        // The same bytes cut at another place are another template: a byte moved from the file's end to the
        // script's name, or from one script to the next, does not give the same value.
        assert_ne!(
            digest("a", "xy", &[("s", "z")]),
            digest("a", "x", &[("ys", "z")])
        );
        assert_ne!(
            digest("a", "x", &[("s", "yz"), ("t", "")]),
            digest("a", "x", &[("s", "y"), ("t", "z")])
        );
        // The scripts are taken lowest name first, whatever order they were named in.
        assert_eq!(
            digest("a", "x", &[("b.sh", "2"), ("a.sh", "1")]),
            digest("a", "x", &[("a.sh", "1"), ("b.sh", "2")])
        );
    }

    #[test]
    fn no_new_privileges_is_a_template_key_off_by_default_and_a_virtual_machine_has_none() {
        let template =
            |value: Value| -> Template { serde_json::from_value(value).expect("a template") };
        let base = json!({ "name": "app", "kind": "oci", "image": "docker.io/library/app" });
        assert!(!template(base.clone()).no_new_privileges);
        let mut on = base.clone();
        on["noNewPrivileges"] = json!(true);
        assert!(template(on).no_new_privileges);
        // The key is spelled camelCase like every other, and a misspelling is refused, not ignored.
        let mut snake = base.clone();
        snake["no_new_privileges"] = json!(true);
        assert!(serde_json::from_value::<Template>(snake).is_err());
        // A virtual machine has no LXC config to carry it.
        let mut vm = json!({
            "name": "vm", "kind": "virtual-machine", "image": "images:debian/13",
            "limits": { "cpu": "2", "memory": "4GiB" }, "rootSize": "20GiB", "noNewPrivileges": true
        });
        let dir = std::path::Path::new("/nonexistent");
        assert!(validate_template(&template(vm.clone()), dir).is_err());
        vm["noNewPrivileges"] = json!(false);
        assert!(validate_template(&template(vm), dir).is_ok());
    }

    #[test]
    fn a_records_no_new_privileges_renders_as_raw_lxc_and_an_older_record_renders_without_it() {
        let record = |extra: Value| -> ServiceConfig {
            let mut value = json!({
                "name": "app", "template": "app", "instance": "app", "ipv4": "192.0.2.10",
                "ports": [8080], "created": 0,
                "source": { "type": "image", "protocol": "oci", "server": "https://docker.io", "alias": "library/app" },
                "pool": "default", "project": PROJECT, "bridge": BRIDGE, "gateway": "192.0.2.1"
            });
            for (key, item) in extra.as_object().expect("object") {
                value[key] = item.clone();
            }
            serde_json::from_value(value).expect("a record")
        };
        // A record written before the key existed is off, renders with no `raw.lxc`, and serializes without the key.
        let old = record(json!({}));
        assert!(!old.no_new_privileges);
        assert!(create_body(&old)["config"].get("raw.lxc").is_none());
        assert!(
            serde_json::to_value(&old)
                .expect("serialized")
                .get("noNewPrivileges")
                .is_none()
        );
        let on = record(json!({ "noNewPrivileges": true }));
        assert_eq!(
            create_body(&on)["config"]["raw.lxc"],
            json!(NO_NEW_PRIVS_RAW_LXC)
        );
        assert_eq!(NO_NEW_PRIVS_RAW_LXC, "lxc.no_new_privs=1");
        assert_eq!(
            serde_json::to_value(&on).expect("serialized")["noNewPrivileges"],
            json!(true)
        );
    }

    #[test]
    fn the_image_of_a_template_is_what_boots_and_then_the_version_a_machine_runs() {
        let template =
            |value: Value| -> Template { serde_json::from_value(value).expect("a template") };
        let digest = format!("sha256:{}", "a".repeat(64));
        // A registry image by its digest: the reference the instance is fetched by.
        let oci = template(json!({
            "name": "app", "kind": "oci", "image": "docker.io/library/app", "digest": digest
        }));
        assert_eq!(
            template_image(&oci),
            format!("docker.io/library/app@{digest}")
        );
        assert_eq!(Some(template_image(&oci)), template_pin(&oci));
        // A system image: with the fingerprint the template gives, or bare when it gives none.
        let bare = template(
            json!({ "name": "vm", "kind": "virtual-machine", "image": "images:debian/13" }),
        );
        assert_eq!(template_image(&bare), "images:debian/13");
        let pinned = template(json!({
            "name": "vm", "kind": "virtual-machine", "image": "images:debian/13", "digest": digest
        }));
        assert_eq!(
            template_image(&pinned),
            format!("images:debian/13@{digest}")
        );
        let local = template(json!({ "name": "built", "kind": "system", "image": "local:built" }));
        assert_eq!(template_image(&local), "local:built");
        // A machine that names the version it runs: its own image first, then that version.
        let running = template(json!({
            "name": "ws", "kind": "virtual-machine", "image": "images:debian/13/cloud",
            "update": { "pin": "serverImage" },
            "settings": { "serverImage": format!("registry.example/server@{digest}") }
        }));
        assert_eq!(
            template_image(&running),
            format!("images:debian/13/cloud running registry.example/server@{digest}")
        );
    }

    #[test]
    fn a_bridged_machines_address_is_its_own_interfaces_and_never_one_the_guest_made() {
        let state = json!({ "network": {
            "docker0": { "hwaddr": "02:42:00:00:00:01", "addresses": [
                { "family": "inet", "address": "172.17.0.1", "scope": "global" } ] },
            "enp5s0": { "hwaddr": "10:66:6A:00:00:07", "addresses": [
                { "family": "inet6", "address": "fe80::1", "scope": "link" },
                { "family": "inet", "address": "198.51.100.7", "scope": "global" } ] },
            "lo": { "hwaddr": "", "addresses": [
                { "family": "inet", "address": "127.0.0.1", "scope": "local" } ] }
        }});
        let by_incus = json!({ "config": { "volatile.eth0.hwaddr": "10:66:6a:00:00:07" } });
        let by_device = json!({ "config": {}, "devices": {
            "eth0": { "type": "nic", "hwaddr": "10:66:6a:00:00:07" } } });
        for row in [by_incus, by_device] {
            assert_eq!(
                bridged_ipv4(&row, &state),
                Some("198.51.100.7".parse().unwrap())
            );
        }
        // The first interface with a global address is the guest's own bridge: the old reading took that one.
        assert_eq!(live_ipv4(&state), Some("172.17.0.1".parse().unwrap()));
        // No interface of the instance's own, or none with an address yet: none.
        let other = json!({ "config": { "volatile.eth0.hwaddr": "10:66:6a:00:00:99" } });
        assert_eq!(bridged_ipv4(&other, &state), None);
        assert_eq!(bridged_ipv4(&other, &json!({})), None);
    }

    #[test]
    fn generated_secrets_reject_biased_bytes_refill_and_stop_at_length() {
        let mut calls = 0;
        let secret = generated_secret_with("key_", |bytes| {
            calls += 1;
            bytes.fill(if calls == 1 { 248 } else { 247 });
            bytes[0] = 255;
            Ok(())
        })
        .unwrap();
        assert_eq!(calls, 3);
        assert_eq!(secret, format!("key_{}", "9".repeat(32)));
        assert!(generated_secret_with("", |_| Err(())).is_err());
        let secret = generated_secret("").unwrap();
        assert_eq!(secret.len(), 32);
        assert!(secret.bytes().all(|b| b.is_ascii_alphanumeric()));
    }
}

/// What a secret is a value of: where it goes decides what it may hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretOf {
    /// A virtual machine's: a file in its guest, so it may have line breaks and end with one.
    Machine,
    /// A container's: an environment value, so it is one line.
    Container,
    /// A function's: an environment value of each run, so it is one line.
    Function,
}

/// The longest secret, in characters. A virtual machine's does not count one line break at its end.
pub const SECRET_MAX: usize = 4096;
/// The shortest secret, in characters. In a virtual machine's, line breaks do not count toward it.
pub const SECRET_MIN: usize = 8;

/// A secret without the one line break at its end that its length does not count.
fn secret_body(value: &str) -> &str {
    value.strip_suffix('\n').unwrap_or(value)
}

/// What a log line or an output must never show for a stored secret: the stored text without the line breaks at its
/// end, since a line never holds them. `None` for a stored text that no secret can be: one longer than the rule
/// allows, counted as [`secret_value`] counts it (in characters, not counting one line break at its end), or shorter
/// than a secret once the line breaks at its end are cut. The two share that count, so no value the node accepts is left out.
pub fn secret_to_hide(stored: &str) -> Option<&str> {
    if secret_body(stored).chars().count() > SECRET_MAX {
        return None;
    }
    let shown = stored.trim_end_matches('\n');
    (shown.chars().count() >= SECRET_MIN).then_some(shown)
}

/// The control character that stands in the way, by what it is. The words never hold the character or where it is.
fn control_named(text: &str) -> &'static str {
    match text.chars().find(|c| c.is_control() && *c != '\n') {
        Some('\0') => "a NUL character",
        Some('\t') => "a tab",
        Some('\r') => "a carriage return that no line feed follows",
        Some(_) => "a control character that is no line break",
        None => "a line break",
    }
}

/// The value a secret verb stores for the value a caller gave, or why it stores none, in words that name the rule and
/// its limit and never the value, a part of it or its length.
///
/// A virtual machine's secret is a file, as a key file is: it is stored byte for byte as given, with the line break
/// at its end when it has one, except that each carriage return and line feed pair is stored as one line feed. It is
/// 8 to 4096 characters, not counting one line break at its end, and at least 8 of them are not line breaks.
///
/// A container's and a function's secret is an environment value: one line of 8 to 4096 characters. A line break at
/// its end would become part of the value the program reads, so it is refused, in words that say it is the only fault.
pub fn secret_value(given: &str, of: SecretOf) -> Result<String, String> {
    if of == SecretOf::Machine {
        let value = given.replace("\r\n", "\n");
        let limits = format!(
            "a virtual machine's secret is {SECRET_MIN} to {SECRET_MAX} characters, not counting one line break at its end; it may have line breaks, and no other control character"
        );
        if value.chars().any(|c| c.is_control() && c != '\n') {
            return Err(format!("{limits}: this one has {}", control_named(&value)));
        }
        let counted = secret_body(&value).chars().count();
        if counted > SECRET_MAX {
            return Err(format!("{limits}: this one is longer"));
        }
        if value.chars().filter(|c| *c != '\n').count() < SECRET_MIN {
            return Err(format!(
                "{limits}: this one has fewer than {SECRET_MIN} characters that are not line breaks"
            ));
        }
        return Ok(value);
    }
    let (whose, why) = match of {
        SecretOf::Function => ("a function's", "each run reads it as an environment value"),
        _ => (
            "a container's",
            "the program reads it as an environment value",
        ),
    };
    let limits = format!(
        "{whose} secret is one line of {SECRET_MIN} to {SECRET_MAX} characters without control characters"
    );
    let length_ok = |text: &str| (SECRET_MIN..=SECRET_MAX).contains(&text.chars().count());
    if !given.chars().any(char::is_control) {
        return if length_ok(given) {
            Ok(given.to_string())
        } else {
            Err(format!(
                "{limits}: this one is not {SECRET_MIN} to {SECRET_MAX} characters"
            ))
        };
    }
    // A value read whole from a file ends with a line break: said as the one thing to change, when it is.
    let cut = given
        .strip_suffix("\r\n")
        .or_else(|| given.strip_suffix('\n'));
    if let Some(cut) = cut
        && !cut.chars().any(char::is_control)
        && length_ok(cut)
    {
        return Err(format!(
            "{limits}: this one ends with a line break, and nothing else is wrong with it. Send it without the line break at its end ({why}, and the line break would be part of it; only a virtual machine's secret, which is a file, may end with one)"
        ));
    }
    Err(format!("{limits}: this one has {}", control_named(given)))
}

pub fn write_secret(path: &Path, value: &str) -> Result<(), TemplateError> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let dir = path
        .parent()
        .ok_or_else(|| TemplateError::refused("invalid secret path"))?;
    let root = dir
        .parent()
        .ok_or_else(|| TemplateError::refused("invalid secret path"))?;
    for path in [root, dir] {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .or_else(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    Ok(())
                } else {
                    Err(e)
                }
            })
            .map_err(|_| TemplateError::refused("the secret directory cannot be made"))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| TemplateError::refused("the secret directory is not private"))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|_| TemplateError::refused("the secret cannot be written"))?;
    file.write_all(value.as_bytes())
        .and_then(|_| file.sync_all())
        .map_err(|_| TemplateError::refused("the secret cannot be written"))
}

/// Replaces `{{name}}`, `{{host}}`, `{{port}}` and `{{secret:KEY}}`; a missing secret makes the whole value absent.
pub fn render(
    text: &str,
    name: &str,
    ipv4: &str,
    gateway: &str,
    port: Option<u16>,
    secrets: &BTreeMap<String, String>,
) -> Option<String> {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find("}}")?;
        let key = &after[..end];
        let value = match key {
            "name" => name.to_string(),
            "host" => ipv4.to_string(),
            "port" => port?.to_string(),
            _ => {
                let secret = key.strip_prefix("secret:")?;
                secrets.get(secret)?.clone()
            }
        };
        out.push_str(&value);
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    if out.contains("{{") {
        None
    } else {
        let _ = gateway;
        Some(out)
    }
}

/// A template entrypoint, with a `{ "file": "name.sh" }` element read from the template directory.
pub fn entrypoint(template: &Template, dir: &Path) -> Result<Option<String>, TemplateError> {
    entrypoint_with(template, &mut |file| {
        std::fs::read_to_string(dir.join(file))
    })
}

/// [`entrypoint`], with each script given by `read`: the file itself, or what was read with the template.
fn entrypoint_with(
    template: &Template,
    read: &mut dyn FnMut(&str) -> std::io::Result<String>,
) -> Result<Option<String>, TemplateError> {
    let Some(parts) = &template.entrypoint else {
        return Ok(None);
    };
    if parts.is_empty() || parts.len() > 64 {
        return Err(TemplateError::refused("entrypoint: invalid length"));
    }
    let mut out = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        let text = match part {
            Value::String(text) => {
                if text.len() > 16_384 || text.contains('\0') || text.contains("{{") {
                    return Err(TemplateError::refused("entrypoint: invalid argument"));
                }
                text.clone()
            }
            Value::Object(object) => {
                let file = object
                    .get("file")
                    .and_then(Value::as_str)
                    .ok_or_else(|| TemplateError::refused("entrypoint: invalid file"))?;
                if !script_name_ok(file) {
                    return Err(TemplateError::refused(
                        "entrypoint: invalid file (a plain file name in the templates directory: \
                         letters, digits, dots, dashes and underscores, not starting with a dot)",
                    ));
                }
                let script =
                    read(file).map_err(|error| script_unreadable(&template.name, file, &error))?;
                script.trim_end_matches('\n').to_string()
            }
            _ => return Err(TemplateError::refused("entrypoint: invalid argument")),
        };
        // A word with whitespace in it, or one that starts with a quote, reaches liblxc 5.0 and older as other words.
        let split_by_old_liblxc = text.contains([' ', '\t', '\u{b}', '\u{c}', '\n', '\r'])
            || text.starts_with(['\'', '"']);
        if split_by_old_liblxc && is_shell_script_argument(parts, index) {
            out.push(shell_quote(&one_line_script(&text)));
        } else if text.contains(['\n', '\r']) {
            return Err(TemplateError::refused(
                "entrypoint: an argument with a line break must be the script of a shell, as in \
                 [\"/bin/sh\", \"-ec\", <script>, ...]: Incus refuses a line break anywhere in oci.entrypoint",
            ));
        } else {
            out.push(shell_quote(&text));
        }
    }
    let joined = out.join(" ");
    if joined.len() > 32_768 {
        return Err(TemplateError::refused("entrypoint: too long"));
    }
    Ok(Some(joined))
}

/// Whether `parts[index]` is the script of a shell started as `sh -c <script>` (or `-ec`, `-eu -c` and the like): the
/// one argument that Arca can carry whitespace, a line break or a leading quote in, because it can decode it again
/// inside the shell.
fn is_shell_script_argument(parts: &[Value], index: usize) -> bool {
    let Some(flag) = index.checked_sub(1).and_then(|i| parts[i].as_str()) else {
        return false;
    };
    let shell = parts[0].as_str().and_then(|s| s.rsplit('/').next());
    matches!(shell, Some("sh" | "bash" | "ash" | "dash"))
        && flag.len() > 1
        && flag.starts_with('-')
        && !flag.starts_with("--")
        && flag.ends_with('c')
        && flag[1..].bytes().all(|b| b.is_ascii_lowercase())
}

/// A shell script as one word that runs the same script. Incus 7.6 refuses a line break in `oci.entrypoint`, and
/// liblxc 5.0 and older (which Incus 7.6 may link) split `lxc.execute.cmd` on spaces alone, keeping the quotes Incus
/// puts round a word with whitespace, so the word has no whitespace either and does not start with a quote; liblxc
/// 6.0 and later split it the same way, since a quote later in a word is a plain character to both. The script goes
/// as base64 without wrapping, decoded by the image's own `base64` (busybox and coreutils both have it) and run by
/// `eval` in the same shell, so `$0`, `$1` and the options (`-e`) are the script's own. Each `${IFS}` stands for a
/// space: unquoted, the shell splits it into fields, so the words are `echo` and the text, `base64` and `-d`, `eval`
/// and the script, as if typed. A failed decode ends the shell with its status and runs nothing. The text is the
/// script's bytes exactly. This holds on Incus 7.3 and later, whose `lxcEncodeCmd` writes the word as it is; Incus 7.0
/// to 7.2 wrote the command with `shellquote.Join`, which escapes the word's `$`, `(` and `|`, and the script does not
/// run there.
fn one_line_script(script: &str) -> String {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(script.as_bytes());
    format!(
        "__arca_script=$(echo${{IFS}}{encoded}|base64${{IFS}}-d)&&eval${{IFS}}\"$__arca_script\""
    )
}

/// Whether `file` is a plain file name, so an entrypoint script is read from the template directory and nowhere else.
fn script_name_ok(file: &str) -> bool {
    !file.is_empty()
        && file.len() <= 128
        && !file.starts_with('.')
        && file
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// The refusal for an entrypoint script that cannot be read: the file that was tried, relative to the state directory,
/// and for a missing file where it belongs. A template's JSON copied in without its script is the usual cause.
fn script_unreadable(template: &str, file: &str, error: &std::io::Error) -> TemplateError {
    if error.kind() == std::io::ErrorKind::NotFound {
        return TemplateError::refused(format!(
            "template {template}: its entrypoint script is not installed (templates/{file} does not exist). \
             The script belongs beside templates/{template}.json in the state directory; for a template Arca ships, \
             `arca templates install --config CONFIG_FILE` (this node's config in place of CONFIG_FILE) puts the template and its script there"
        ));
    }
    TemplateError::refused(format!(
        "template {template}: its entrypoint script templates/{file} cannot be read ({})",
        error.kind()
    ))
}

pub(crate) fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'/' | b'_' | b'-' | b'.' | b':' | b'@' | b'%' | b'+' | b',' | b'='
                )
        })
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// The live status of one service instance, as its view reports it.
pub fn state(status: Option<&str>) -> &'static str {
    match status.unwrap_or("") {
        "Running" => "running",
        "Stopped" | "Frozen" => "stopped",
        "Error" => "failed",
        "" => "failed",
        _ => "unknown",
    }
}

/// The address of the first global IPv4 on a live instance state.
pub fn live_ipv4(state: &Value) -> Option<Ipv4Addr> {
    state["network"]
        .as_object()?
        .iter()
        .filter(|(name, _)| name.as_str() != "lo")
        .flat_map(|(_, nic)| nic["addresses"].as_array().into_iter().flatten())
        .find_map(|address| {
            (address["family"] == "inet" && address["scope"] == "global")
                .then(|| address["address"].as_str()?.parse::<Ipv4Addr>().ok())
                .flatten()
        })
}

/// The address a machine on a host bridge holds now: the first global IPv4 address of an interface whose hardware
/// address is one the instance's own network devices have, as Incus gave it (`volatile.<device>.hwaddr` in `row`'s
/// config) or as its device states it (`hwaddr`). An interface the guest made for itself, such as a container bridge
/// inside it, has an address too, and is never the one meant.
pub fn bridged_ipv4(row: &Value, state: &Value) -> Option<Ipv4Addr> {
    let mut given: Vec<&str> = row["config"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(key, _)| key.starts_with("volatile.") && key.ends_with(".hwaddr"))
        .filter_map(|(_, value)| value.as_str())
        .collect();
    for devices in ["devices", "expanded_devices"] {
        given.extend(
            row[devices]
                .as_object()
                .into_iter()
                .flat_map(|devices| devices.values())
                .filter(|device| device["type"] == "nic")
                .filter_map(|device| device["hwaddr"].as_str()),
        );
    }
    state["network"]
        .as_object()?
        .values()
        .filter(|nic| {
            nic["hwaddr"]
                .as_str()
                .is_some_and(|held| given.iter().any(|mac| mac.eq_ignore_ascii_case(held)))
        })
        .flat_map(|nic| nic["addresses"].as_array().into_iter().flatten())
        .find_map(|address| {
            (address["family"] == "inet" && address["scope"] == "global")
                .then(|| address["address"].as_str()?.parse::<Ipv4Addr>().ok())
                .flatten()
        })
}

/// A view row, for a record or for an instance Arca did not create.
pub fn view(cfg: &ServiceConfig, live: Option<&Value>) -> Value {
    let (state, ipv4) = match live {
        None if cfg.creating => ("creating", cfg.ipv4.clone()),
        Some(live) => (
            state(live["status"].as_str()),
            live_ipv4(live)
                .map(|ip| ip.to_string())
                .unwrap_or_else(|| cfg.ipv4.clone()),
        ),
        None => ("failed", cfg.ipv4.clone()),
    };
    let links: Vec<Value> = cfg
        .links
        .iter()
        .map(|link| json!({"to": link.to, "port": link.port}))
        .collect();
    let published = cfg
        .published
        .as_ref()
        .map(|published| json!({"reach": "tailnet", "node": published.node, "port": 8443}));
    let mut view = json!({
        "name": cfg.name,
        "template": cfg.template,
        "state": state,
        "ipv4": ipv4,
        "ports": cfg.ports,
        "links": links,
        "published": published,
        // The host port only exists when the service has a lan proxy. Container ports are not host listeners.
        "lanPort": cfg.lan.as_ref().map(|lan| lan.port)
    });
    // Only a service with kernel access carries the key, so every other view is the one it always was.
    if let Some(link) = &cfg.kernel_access {
        view["kernelAccess"] = json!({ "port": link.port, "connect": link.connect });
    }
    // Only a service that runs with no-new-privileges carries the key.
    if cfg.no_new_privileges {
        view["noNewPrivileges"] = json!(true);
    }
    // Only a service that may reach a private address carries the key.
    if !cfg.destinations.is_empty() {
        view["destinations"] = json!(cfg.destinations);
    }
    view
}

/// The fingerprint of the image an instance was created from, as Incus records it on the instance
/// (`volatile.base_image`): 64 hex digits. `None` when the row carries none.
pub fn base_image(row: &Value) -> Option<&str> {
    row["config"]["volatile.base_image"]
        .as_str()
        .filter(|image| image.len() == 64 && image.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// A project instance with no service record.
pub fn unknown_view(row: &Value) -> Option<Value> {
    let name = row["name"].as_str()?;
    if !service_name_ok(name) {
        return None;
    }
    let ipv4 = row["devices"]
        .as_object()
        .into_iter()
        .flat_map(|devices| devices.values())
        .find_map(|device| device["ipv4.address"].as_str())
        .unwrap_or("");
    Some(json!({
        "name": name,
        "template": "",
        "state": "unknown",
        "ipv4": ipv4,
        "ports": [],
        "links": [],
        "published": null,
        "lanPort": null
    }))
}

/// A marked instance without a readable record: listed as failed and left alone.
pub fn failed_view(row: &Value) -> Option<Value> {
    let name = row["name"].as_str()?;
    if !service_name_ok(name) {
        return None;
    }
    let ipv4 = row["devices"]["eth0"]["ipv4.address"]
        .as_str()
        .unwrap_or("");
    Some(json!({
        "name": name,
        "template": row["config"][TEMPLATE_KEY].as_str().unwrap_or(""),
        "state": "failed",
        "ipv4": ipv4,
        "ports": [],
        "links": [],
        "published": null,
        "lanPort": null
    }))
}

/// The service rows of a project listing.
pub fn project_instances(listing: &Value, project: &str) -> Vec<Value> {
    listing
        .as_array()
        .into_iter()
        .flatten()
        .filter(|row| row["project"].as_str() == Some(project))
        .cloned()
        .collect()
}

/// A log line cleaned the way the MCP door cleans untrusted text: ANSI/control bytes gone, whitespace runs one space, trimmed.
pub fn clean_log(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_escape = false;
    for ch in text.chars() {
        if in_escape {
            if ch.is_ascii_alphabetic() {
                in_escape = false;
            }
            continue;
        }
        if ch == '\u{1b}' {
            in_escape = true;
            continue;
        }
        if ch.is_control() && ch != '\t' {
            continue;
        }
        out.push(ch);
    }
    let mut words = out.split_whitespace();
    let mut clean = words.next().unwrap_or("").to_string();
    for word in words {
        clean.push(' ');
        clean.push_str(word);
    }
    clean
}

fn credential_patterns() -> &'static [(regex::Regex, &'static str)] {
    use std::sync::OnceLock;
    static PATTERNS: OnceLock<Vec<(regex::Regex, &'static str)>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            (r#"(?i)\b((?:access_|refresh_|id_|auth_)?token|api[_-]?key|apikey|auth[_-]?key|client_secret|secret|passw(?:or)?d)=[^\s&"'<>]+"#, "$1=[secret]"),
            (r#"(?i)(authorization["']?\s*[:=]\s*["']?)(?:(?:bearer|basic|token)\s+)?[^\s"',]+"#, "$1[secret]"),
            (r#"(?i)\b(bearer)\s+[A-Za-z0-9._~+/-]{16,}=*"#, "$1 [secret]"),
            (r"\btskey-[A-Za-z0-9-]*", "[secret]"),
            (r"(?i)\b([a-z][a-z0-9+.-]*://[^\s:@/?#]*):[^\s@/?#]+@", "$1:[secret]@"),
            (r"\bomni_sk_[A-Za-z0-9*]+", "[secret]"),
        ]
        .into_iter()
        .map(|(pattern, replacement)| {
            (regex::Regex::new(pattern).expect("the credential pattern is valid"), replacement)
        })
        .collect()
    })
}

/// The shortest line of a secret of several lines that is hidden by its value wherever it is shown, in characters.
///
/// A secret of several lines (a key file) is shown one line at a time in a log, so each of its lines is hidden as a
/// value of its own. A short line is not: a line of a few characters (`}`, `[server]`, `port = 5432`) is ordinary
/// text that a log holds in many places, and hiding it would blank lines that hold no secret. At 16 characters a line
/// of random text carries 96 bits of a base64 key, more than anyone finds in a log by chance, and a line of a key
/// file (64 characters, or the last one) is hidden whole. What a shorter line can leave shown is at most the last
/// few characters of such a file, too few to rebuild anything from; a PEM block is also hidden whole by its shape.
pub const SECRET_LINE_MIN: usize = 16;

/// A PEM label as RFC 7468 allows it: printable characters other than `-`, joined by single spaces or hyphens, so
/// `RSA PRIVATE KEY`, `OpenVPN Static key V1` and `OpenVPN tls-crypt-v2 server key` are all labels.
const PEM_LABEL: &str = r"[\x21-\x2C\x2E-\x7E](?:[- ]?[\x21-\x2C\x2E-\x7E]){0,63}";

/// What may follow the BEGIN line of a PEM block whose END line was not printed: line breaks (also written as `\n`
/// inside a string), and lines that are only base64 of at least 16 characters (a `/` may be written `\/`) or a PEM
/// header (`Proc-Type: ...`). Ordinary text after it is not taken, so a log or a function's result after such a line
/// stays shown; what can stay shown of a key is a last line shorter than 16 characters.
const PEM_BODY: &str = r"(?:(?:\r\n|\r|\n|\\r|\\n|[ \t])*(?:(?:[A-Za-z0-9+=]|\\?/){16,}|(?:Proc-Type|DEK-Info|Comment|Version|Hash|Charset)[ \t]*:[^\r\n\\]*))*";

/// A PEM block with its END line: from its BEGIN line to the next END line, whatever is between; and one without:
/// from its BEGIN line over the lines that look like a key's ([`PEM_BODY`]).
fn pem_blocks() -> &'static (regex::Regex, regex::Regex) {
    use std::sync::OnceLock;
    static PEM: OnceLock<(regex::Regex, regex::Regex)> = OnceLock::new();
    PEM.get_or_init(|| {
        let ended = format!(r"-----BEGIN {PEM_LABEL}-----(?s:.*?)-----END {PEM_LABEL}-----");
        let unended = format!(r"-----BEGIN {PEM_LABEL}-----{PEM_BODY}");
        (
            regex::Regex::new(&ended).expect("the PEM pattern is valid"),
            regex::Regex::new(&unended).expect("the PEM pattern is valid"),
        )
    })
}

/// A PEM BEGIN line, and a PEM END line.
fn pem_marker(kind: &str) -> regex::Regex {
    regex::Regex::new(&format!(r"-----{kind} {PEM_LABEL}-----"))
        .expect("the PEM marker pattern is valid")
}

/// Hides each whole PEM block in a text as it came, before it is cut into lines or cleaned: a key printed on a
/// console is hidden whatever it is, a stored secret or not. A text that begins inside a block (its END line comes
/// before any BEGIN line, as when the start of a log was cut away) is hidden from its start to that END line.
pub fn redact_blocks(text: &str) -> String {
    use std::sync::OnceLock;
    static MARKERS: OnceLock<(regex::Regex, regex::Regex)> = OnceLock::new();
    let (begin, end) = MARKERS.get_or_init(|| (pem_marker("BEGIN"), pem_marker("END")));
    let mut text = std::borrow::Cow::Borrowed(text);
    if let Some(first_end) = end.find(&text) {
        let begins_before = begin
            .find(&text)
            .is_some_and(|first_begin| first_begin.start() < first_end.start());
        if !begins_before {
            text = std::borrow::Cow::Owned(format!("[secret]{}", &text[first_end.end()..]));
        }
    }
    // A BEGIN line has an END line after it only before the last END line, so the text is cut there once: blocks
    // are paired before it, and a BEGIN line after it is followed over a key's lines only. Searched for its END line
    // in one pattern, each BEGIN line without one was followed to the end of the text, once per such line.
    let last_end = end.find_iter(&text).last().map_or(0, |found| found.end());
    let (with_end, without_end) = text.split_at(last_end);
    let (ended, unended) = pem_blocks();
    let mut hidden = ended.replace_all(with_end, "[secret]").into_owned();
    hidden.push_str(&unended.replace_all(without_end, "[secret]"));
    hidden
}

/// What hides the stored secrets in a text: the values to replace, longest first, and the base64 body of each secret
/// of several lines with its line breaks taken out. Made once by [`values_to_hide`] for a text of many lines and given
/// to [`redact_hidden`] for each line.
#[derive(Debug, Clone, Default)]
pub struct Hidden {
    values: Vec<String>,
    bodies: Vec<String>,
}

/// A run of base64 of at least 16 characters, a `/` written as `\/` too, as a key's body is printed.
fn base64_run() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RUN: OnceLock<regex::Regex> = OnceLock::new();
    RUN.get_or_init(|| {
        regex::Regex::new(r"(?:[A-Za-z0-9+=]|\\?/){16,}").expect("the base64 pattern is valid")
    })
}

/// The texts that hide `secrets`: each value whole, each value as [`clean_log`] shows it, and for a value of several
/// lines each of its lines of at least [`SECRET_LINE_MIN`] characters, as it is and as it is shown cleaned. For a value
/// of several lines, also its base64 body without line breaks, so a piece of it printed at another width, or with each
/// `/` written `\/`, is found too.
pub fn values_to_hide(secrets: &[String]) -> Hidden {
    let mut values: Vec<String> = Vec::new();
    let mut bodies: Vec<String> = Vec::new();
    let mut add = |value: &str, shortest_chars: usize| {
        if value.len() >= 8
            && value.chars().count() >= shortest_chars
            && !values.iter().any(|v| v == value)
        {
            values.push(value.to_string());
        }
    };
    for secret in secrets {
        add(secret, 0);
        add(&clean_log(secret), 0);
        if secret.contains(['\n', '\r']) {
            for line in secret.split(['\n', '\r']) {
                add(line.trim(), SECRET_LINE_MIN);
                add(&clean_log(line), SECRET_LINE_MIN);
            }
            let body: String = secret
                .split(['\n', '\r'])
                .filter(|line| !line.trim_start().starts_with("-----"))
                .flat_map(str::chars)
                .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '='))
                .collect();
            if body.len() >= SECRET_LINE_MIN {
                bodies.push(body);
            }
        }
    }
    values.sort_by_key(|value| std::cmp::Reverse(value.len()));
    Hidden { values, bodies }
}

/// Hides each PEM block, then every stored/rendered secret value (whole and, for one of several lines, line by line;
/// longest first), then each run of base64 that is a piece of a stored body, then every credential pattern.
pub fn redact_log(text: &str, secrets: &[String]) -> String {
    redact_hidden(text, &values_to_hide(secrets))
}

/// [`redact_log`] with what to hide already made by [`values_to_hide`].
pub fn redact_hidden(text: &str, hidden: &Hidden) -> String {
    let mut out = redact_blocks(text);
    for value in &hidden.values {
        out = out.replace(value.as_str(), "[secret]");
    }
    if !hidden.bodies.is_empty() {
        out = base64_run()
            .replace_all(&out, |found: &regex::Captures| {
                let piece = found[0].replace("\\/", "/");
                let held = |piece: &str| hidden.bodies.iter().any(|body| body.contains(piece));
                if held(&piece) {
                    return "[secret]".to_string();
                }
                // A line break written `\n` or `\r` inside a string leaves its letter at the start of the next piece.
                match piece.strip_prefix(['n', 'r']) {
                    Some(rest) if rest.len() >= SECRET_LINE_MIN && held(rest) => {
                        format!("{}[secret]", &piece[..1])
                    }
                    _ => found[0].to_string(),
                }
            })
            .into_owned();
    }
    for (pattern, replacement) in credential_patterns() {
        out = pattern.replace_all(&out, *replacement).into_owned();
    }
    out
}

#[cfg(test)]
mod redaction_tests {
    use super::*;

    /// Lines of letters made from a counter: no value of a real secret.
    fn lines(seed: u32, count: usize, width: usize) -> Vec<String> {
        let mut state = seed;
        (0..count)
            .map(|_| {
                (0..width)
                    .map(|_| {
                        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                        (b'a' + (state >> 24) as u8 % 26) as char
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn a_failures_words_hide_a_secret_of_several_lines_that_cleaning_would_join() {
        // A stored secret of three lines, printed by a failing command on its error stream with the lines apart.
        let secret = format!("{}\n", lines(7, 3, 40).join("\n"));
        let stderr = format!("cannot load:\n{secret}giving up\n");
        let shown = crate::service_exec::failure_text(&stderr, std::slice::from_ref(&secret));
        let leaks = secret.lines().any(|line| shown.contains(line));
        assert!(!leaks, "a line of the secret is in the failure's words");
        assert!(shown.starts_with("cannot load:") && shown.ends_with("giving up"));
    }

    #[test]
    fn a_short_line_of_a_secret_of_several_lines_is_not_hidden_by_its_value() {
        let long = lines(9, 1, SECRET_LINE_MIN).remove(0);
        let short = lines(11, 1, SECRET_LINE_MIN - 1).remove(0);
        let secret = format!("{long}\n{short}\n");
        let hidden = redact_log(&format!("a {long} b"), std::slice::from_ref(&secret));
        assert!(
            hidden == "a [secret] b",
            "a line of the shortest length hidden is not hidden"
        );
        let kept = redact_log(&format!("a {short} b"), &[secret]);
        assert!(kept == format!("a {short} b"), "a shorter line is hidden");
    }

    /// A key file of `lines` body lines of `width` base64 characters between its BEGIN and END lines, from a counter.
    fn key(label: &str, seed: u32, lines: usize, width: usize) -> (String, Vec<String>) {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut state = seed;
        let body: Vec<String> = (0..lines)
            .map(|_| {
                (0..width)
                    .map(|_| {
                        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                        ALPHABET[(state >> 24) as usize % ALPHABET.len()] as char
                    })
                    .collect()
            })
            .collect();
        let text = format!(
            "-----BEGIN {label}-----\n{}\n-----END {label}-----\n",
            body.join("\n")
        );
        (text, body)
    }

    /// Whether `shown` holds any 16 characters of `body` in a row.
    fn shows_any(shown: &str, body: &[String]) -> bool {
        let joined: String = body.concat();
        let shown = shown.replace("\\/", "/");
        (0..joined.len().saturating_sub(15)).any(|at| shown.contains(&joined[at..at + 16]))
    }

    #[test]
    fn a_begin_line_without_its_end_hides_the_key_after_it_and_not_the_text_after_the_key() {
        let word = ["PRI", "VATE"].concat();
        // A line that only names the marker: the marker goes, the words stay.
        let usage =
            "usage: paste a key that starts with -----BEGIN CERTIFICATE----- here\nnext line";
        assert_eq!(
            redact_blocks(usage),
            "usage: paste a key that starts with [secret] here\nnext line"
        );
        // A key cut before its END line: its lines go, the line after it stays.
        let (whole, body) = key(&format!("RSA {word} KEY"), 3, 4, 64);
        let cut: String = whole.lines().take(5).collect::<Vec<_>>().join("\n");
        let shown = redact_blocks(&format!("{cut}\nready"));
        assert!(!shows_any(&shown, &body), "a piece of the cut key is shown");
        assert!(
            shown.ends_with("\nready"),
            "the line after the key is hidden"
        );
        // A function's result after such a line is still one JSON object.
        let result = r#"{"ok":false,"error":"no -----BEGIN CERTIFICATE----- found"}"#;
        assert_eq!(
            redact_blocks(result),
            r#"{"ok":false,"error":"no [secret] found"}"#
        );
    }

    #[test]
    fn a_long_text_of_begin_lines_without_their_end_is_hidden_in_one_pass() {
        // A guest can print a BEGIN line on every line of its console. Each such line was once followed to the end of
        // the text for an END line, so the time grew with the square of the text: 256 KiB took seconds. One pass
        // takes a few milliseconds; the bound is generous, for a loaded machine and an unoptimised build.
        let word = ["PRI", "VATE"].concat();
        let (whole, body) = key(&format!("RSA {word} KEY"), 7, 4, 64);
        let line = "-----BEGIN CERTIFICATE----- request 42 answered\n";
        let lines = 256 * 1024 / line.len();
        let text = format!("{whole}\n{}ready", line.repeat(lines));
        let began = std::time::Instant::now();
        let shown = redact_blocks(&text);
        let took = began.elapsed();
        assert!(
            took < std::time::Duration::from_secs(2),
            "{lines} BEGIN lines took {} ms",
            took.as_millis()
        );
        assert!(
            !shows_any(&shown, &body),
            "a piece of the ended key is shown"
        );
        assert_eq!(shown.matches(" request 42 answered\n").count(), lines);
        assert!(shown.ends_with("ready"), "the last line is hidden");
    }

    #[test]
    fn a_text_that_begins_inside_a_key_is_hidden_from_its_start_to_the_end_line() {
        let word = ["PRI", "VATE"].concat();
        let (whole, body) = key(&format!("EC {word} KEY"), 5, 4, 64);
        let inside: String = whole.lines().skip(2).collect::<Vec<_>>().join("\n");
        let shown = redact_blocks(&format!("{inside}\nlistening\n"));
        assert!(!shows_any(&shown, &body), "a piece of the key is shown");
        assert_eq!(shown, "[secret]\nlistening\n");
        // An END line after its BEGIN line is an ordinary block.
        assert_eq!(redact_blocks(&format!("a\n{whole}b")), "a\n[secret]\nb");
    }

    #[test]
    fn a_label_with_lowercase_letters_or_hyphens_is_a_pem_label() {
        for label in ["OpenVPN Static key V1", "OpenVPN tls-crypt-v2 server key"] {
            let (whole, body) = key(label, 7, 3, 32);
            let shown = redact_blocks(&format!("start\n{whole}done"));
            assert!(
                !shows_any(&shown, &body),
                "{label}: a piece of the key is shown"
            );
            assert_eq!(shown, "start\n[secret]\ndone", "{label}");
        }
    }

    #[test]
    fn a_stored_key_printed_at_another_width_or_with_escaped_slashes_is_hidden() {
        let word = ["PRI", "VATE"].concat();
        let (stored, body) = key(&format!("RSA {word} KEY"), 11, 8, 64);
        let joined = body.concat();
        // The body alone, without its marker lines, wrapped at 76 as another tool prints it.
        let rewrapped: Vec<String> = joined
            .as_bytes()
            .chunks(76)
            .map(|chunk| format!("key: {}", String::from_utf8_lossy(chunk)))
            .collect();
        let hidden = values_to_hide(std::slice::from_ref(&stored));
        for line in &rewrapped {
            let shown = redact_hidden(line, &hidden);
            assert!(
                !shows_any(&shown, &body),
                "a re-wrapped piece of the key is shown"
            );
        }
        // The body as one JSON string whose encoder writes each / as \/.
        let escaped = format!(r#"{{"key":"{}"}}"#, body.join(r"\n").replace('/', r"\/"));
        let shown = redact_hidden(&escaped, &hidden);
        assert!(
            !shows_any(&shown, &body),
            "an escaped piece of the key is shown"
        );
        // A run of base64 that is no piece of the key stays.
        let other = "sha256:0123456789abcdef0123456789abcdef";
        assert_eq!(redact_hidden(other, &hidden), other);
    }

    #[test]
    fn a_failures_words_hide_a_secret_with_two_spaces_inside_a_line() {
        let line = lines(13, 1, 20).remove(0);
        let secret = format!(
            "{}  {}\n{}\n",
            &line[..10],
            &line[10..],
            lines(17, 1, 30).remove(0)
        );
        let stderr = format!("cannot load:\n{secret}giving up\n");
        let shown = crate::service_exec::failure_text(&stderr, std::slice::from_ref(&secret));
        let first = secret.lines().next().expect("a first line");
        let folded = clean_log(first);
        assert!(
            !shown.contains(first) && !shown.contains(&folded),
            "the line with two spaces is in the failure's words"
        );
    }
}
