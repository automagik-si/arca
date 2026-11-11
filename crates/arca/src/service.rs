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
    /// The template's settings, as a `cloud-init.vendor-data` that writes them to [`GUEST_SETTINGS_PATH`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor_data: Option<String>,
}

/// The kernel access an instance's own devices show: its `kernel` device, when it is exactly the device Arca makes for
/// some port and some services socket path ([`kernel_device`]). `None` for an instance with no such device, or with one
/// of any other shape. A record made again from an instance's marks reads it, so the record says what the instance has.
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

impl ServiceConfig {
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
    arca_netproject::tenant::egress_rules(gateway, hosts, block)
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
    /// A container template whose service may talk to this node's kernel: `{ "port": <n> }`, the port inside the
    /// instance where the kernel answers on loopback. Only a service of the node's own project is created from such a
    /// template, never one in a tenant.
    #[serde(default)]
    pub kernel_access: Option<KernelAccess>,
}

impl Template {
    pub fn is_vm(&self) -> bool {
        self.kind == VM_KIND
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
    Ok(template)
}

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
    if template.first_boot.is_some() || template.settings.is_some() {
        return Err(TemplateError::refused(format!(
            "template {name}: only a virtual-machine template has a firstBoot or settings"
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
    let Some(first_boot) = &template.first_boot else {
        return Ok(None);
    };
    let name = &template.name;
    let file = first_boot_file(first_boot)
        .ok_or_else(|| TemplateError::refused(format!("template {name}: invalid firstBoot")))?;
    let script = std::fs::read_to_string(dir.join(file)).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            TemplateError::refused(format!(
                "template {name}: its first-boot script is not installed (templates/{file} does not exist). \
                 The script belongs beside templates/{name}.json in the state directory; for a template Arca ships, \
                 `arca templates install --config <file>` puts the template and its script there"
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
    use base64::Engine as _;
    let Some(settings) = &template.settings else {
        return Ok(None);
    };
    let text = serde_json::to_string(settings)
        .ok()
        .filter(|text| settings.is_object() && text.len() <= SETTINGS_MAX)
        .ok_or_else(|| {
            TemplateError::refused(format!("template {}: invalid settings", template.name))
        })?;
    let content = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    Ok(Some(format!(
        "#cloud-config\nwrite_files:\n- path: {GUEST_SETTINGS_PATH}\n  permissions: '0644'\n  encoding: b64\n  content: {content}\n"
    )))
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
        "local" => Ok(json!({ "type": "image", "alias": alias })),
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
    let Some(parts) = &template.entrypoint else {
        return Ok(None);
    };
    if parts.is_empty() || parts.len() > 64 {
        return Err(TemplateError::refused("entrypoint: invalid length"));
    }
    let mut out = Vec::new();
    for part in parts {
        match part {
            Value::String(text) => {
                if text.len() > 16_384 || text.contains('\0') || text.contains("{{") {
                    return Err(TemplateError::refused("entrypoint: invalid argument"));
                }
                out.push(shell_quote(text));
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
                let path = dir.join(file);
                let script = std::fs::read_to_string(&path)
                    .map_err(|error| script_unreadable(&template.name, file, &error))?;
                out.push(shell_quote(script.trim_end_matches('\n')));
            }
            _ => return Err(TemplateError::refused("entrypoint: invalid argument")),
        }
    }
    let joined = out.join(" ");
    if joined.len() > 32_768 {
        return Err(TemplateError::refused("entrypoint: too long"));
    }
    Ok(Some(joined))
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
             `arca templates install --config <file>` puts the template and its script there"
        ));
    }
    TemplateError::refused(format!(
        "template {template}: its entrypoint script templates/{file} cannot be read ({})",
        error.kind()
    ))
}

fn shell_quote(value: &str) -> String {
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

/// Replaces every stored/rendered secret value (longest first) and then every credential pattern.
pub fn redact_log(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_string();
    let mut values: Vec<&String> = secrets.iter().filter(|value| value.len() >= 8).collect();
    values.sort_by_key(|value| std::cmp::Reverse(value.len()));
    for value in values {
        out = out.replace(value, "[secret]");
    }
    for (pattern, replacement) in credential_patterns() {
        out = pattern.replace_all(&out, *replacement).into_owned();
    }
    out
}
