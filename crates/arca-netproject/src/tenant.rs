//! Tenants: one restricted project per tenant, on the node's managed service bridge. A tenant is the entry `tenant-<id>`
//! (plugin `arca.tenant`). It owns its project and that project's service profile, and nothing else: it makes no bridge and
//! no network, and its instances sit on the bridge the node's services already use. Each tenant holds a /28 of the service
//! /24, and its instances take their addresses from it (Rules 61 to 65 of the ownership document).
use crate::destination::{self, Destination};
use crate::{BRIDGE, PROFILE, check_cidr, create, observed, project_ref};
use arca_kernel::{
    desired::PluginId,
    journal::EffectClass,
    ownership::{
        IncusKind, SCRATCH_TENANT_PROJECT_PREFIX, TENANT_PROJECT_PREFIX, tenant_entry,
        tenant_id_ok, tenant_project,
    },
    plugin::{Ctx, EffectOp, Manifest, Plugin, PluginError},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::net::Ipv4Addr;

/// The most addresses a tenant's block holds: a /28. The lowest /28 of the service subnet is never a tenant's.
pub const BLOCK_SIZE: u32 = 16;
/// The root disk size of a container in a tenant. The project's `limits.disk` counts it, so it is always stated.
pub const CONTAINER_ROOT_SIZE: &str = "16GiB";
/// The CPUs and memory a container takes from the tenant's service profile when its template states none.
pub const PROFILE_CPU: u32 = 2;
pub const PROFILE_MEMORY: &str = "2GiB";

/// The caps on everything in a tenant's project together: its `limits.*` keys.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TenantCaps {
    pub instances: u32,
    pub containers: u32,
    pub virtual_machines: u32,
    pub cpu: u32,
    pub memory: String,
    pub disk: String,
}

impl TenantCaps {
    /// The project keys the caps are written as, each with its value.
    pub fn limits(&self) -> [(&'static str, String); 6] {
        [
            ("limits.instances", self.instances.to_string()),
            ("limits.containers", self.containers.to_string()),
            ("limits.virtual-machines", self.virtual_machines.to_string()),
            ("limits.cpu", self.cpu.to_string()),
            ("limits.memory", self.memory.clone()),
            ("limits.disk", self.disk.clone()),
        ]
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TenantConfig {
    pub id: String,
    pub project: String,
    pub bridge: String,
    /// The node's service /24.
    pub cidr: String,
    /// The tenant's block of it, with its size: `a.b.c.d/29`, or `a.b.c.d/28` for a tenant of more than 8 instances.
    pub block: String,
    /// The storage pool the profile's root disk is on.
    pub pool: String,
    pub caps: TenantCaps,
    pub created: u64,
}

/// The restrictions a tenant's project is created with, beside `restricted` itself. Each is the tighter value: no proxy
/// device, no GPU, disks from a pool only, NICs on a granted network only, and no nesting, privileged container or
/// low-level key. The Incus provider admits exactly these keys, with exactly these values, for a tenant's entry in its own
/// project.
pub const RESTRICTIONS: [(&str, &str); 8] = [
    ("restricted.devices.proxy", "block"),
    ("restricted.devices.gpu", "block"),
    ("restricted.devices.disk", "managed"),
    ("restricted.devices.nic", "managed"),
    ("restricted.containers.nesting", "block"),
    ("restricted.containers.privilege", "unprivileged"),
    ("restricted.containers.lowlevel", "block"),
    ("restricted.virtual-machines.lowlevel", "block"),
];

/// A tenant's block of the service /24: its first address and its prefix length, 29 (8 addresses) or 28 (16).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Block {
    pub base: Ipv4Addr,
    pub prefix: u32,
}

impl Block {
    /// How many addresses the block holds.
    pub fn size(self) -> u32 {
        1 << (32 - self.prefix)
    }

    /// Whether `ip` is inside the block.
    pub fn holds(self, ip: Ipv4Addr) -> bool {
        u32::from(ip) & !(self.size() - 1) == u32::from(self.base)
    }

    /// Whether the two blocks share an address.
    pub fn overlaps(self, other: Block) -> bool {
        self.holds(other.base) || other.holds(self.base)
    }

    /// The addresses an instance may take: every one of the block but the /24's broadcast address.
    pub fn addresses(self) -> Vec<Ipv4Addr> {
        (0..self.size())
            .map(|offset| u32::from(self.base) + offset)
            .filter(|ip| ip & 0xff != 0xff)
            .map(Ipv4Addr::from)
            .collect()
    }

    /// The service /24 without the block, as the fewest CIDR ranges: one of each length from /25 to the block's own. A
    /// tenant's NIC ACL rejects them, so no instance of it reaches another tenant's block or the node's own services.
    pub fn outside(self) -> Vec<String> {
        let block = u32::from(self.base);
        let mut ranges: Vec<(u32, u32)> = (25..=self.prefix)
            .map(|length| {
                let size = 1u32 << (32 - length);
                ((block & !(size - 1)) ^ size, length)
            })
            .collect();
        ranges.sort_unstable();
        ranges
            .into_iter()
            .map(|(ip, length)| format!("{}/{length}", Ipv4Addr::from(ip)))
            .collect()
    }

    /// The block as an entry records it: `a.b.c.d/29` or `a.b.c.d/28`.
    pub fn text(self) -> String {
        format!("{}/{}", self.base, self.prefix)
    }
}

/// The prefix length of the block a tenant with these caps holds: a /29, or a /28 when it may hold more than 8 instances.
pub fn block_prefix(caps: &TenantCaps) -> u32 {
    if caps.instances > 8 { 28 } else { 29 }
}

/// The blocks of prefix length `prefix` a tenant may hold, from the top of the service /24 down. None is inside the
/// lowest /28: that one holds the gateway.
pub fn blocks(cidr: &str, prefix: u32) -> Result<Vec<Block>, &'static str> {
    if !(28..=29).contains(&prefix) {
        return Err("a tenant's block is a /29 or a /28");
    }
    let base = u32::from(check_cidr(cidr, &[])?) & 0xffff_ff00;
    let size = 1u32 << (32 - prefix);
    Ok((BLOCK_SIZE / size..256 / size)
        .rev()
        .map(|index| Block {
            base: Ipv4Addr::from(base | (index * size)),
            prefix,
        })
        .collect())
}

/// The block `block` names, when it is one of `cidr`'s tenant blocks written as `a.b.c.d/29` or `a.b.c.d/28`.
pub fn parse_block(block: &str, cidr: &str) -> Option<Block> {
    let (address, prefix) = block.split_once('/')?;
    let prefix = prefix.parse::<u32>().ok()?;
    let found = Block {
        base: address.parse::<Ipv4Addr>().ok()?,
        prefix,
    };
    blocks(cidr, prefix).ok()?.contains(&found).then_some(found)
}

/// Match Incus' complete ACL rule shape, including its empty optional fields.
pub fn rule(fields: Value) -> Value {
    let mut out = json!({"action":"", "source":"", "destination":"", "protocol":"",
        "source_port":"", "destination_port":"", "icmp_type":"", "icmp_code":"",
        "description":"", "state":"enabled"});
    out.as_object_mut()
        .unwrap()
        .extend(fields.as_object().unwrap().clone());
    out
}

/// The egress rules of a NIC on the managed service bridge, whose gateway is `gateway`: reject private ranges outside
/// the service /24 before allowing internet traffic. With `tenant`, the NIC is a tenant's: it also rejects the service
/// /24 outside the tenant's block, and it never has a destination.
///
/// `destinations` are the private addresses this one service may reach through the node, each on one tcp port. Incus
/// weighs a reject above an allow whatever the order of the rules, so an allow alone opens nothing: each address is
/// cut out of the reject of the private ranges, one allow opens tcp to it on the named port, and the last allow, which
/// lets everything else out to the internet, names every address but the opened ones. A packet to an opened address
/// that is not tcp on its port then matches no rule at all and meets the NIC's default action, which every service NIC
/// sets to reject: that closes every IP protocol, the ones a rule cannot name (GRE, IPIP, ESP, SCTP) among them. Three
/// rejects of the address's own (every other tcp port, udp, icmp) say the same again, should a NIC's default ever be
/// another. The reject of the host's own addresses is never cut, and neither is anything for a network or broadcast
/// address of the host's networks: none of them is a destination, whatever a record says. With no destination the
/// rules are the ones every service had before there were any.
pub fn egress_rules(
    gateway: Ipv4Addr,
    hosts: &Value,
    tenant_block: Option<Block>,
    destinations: &[Destination],
) -> Vec<Value> {
    let own = u32::from(gateway) & 0xffffff00;
    let mut host4 = std::collections::BTreeSet::from([gateway]);
    host4.extend(
        hosts
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|host| host.as_str()?.split('/').next()?.parse::<Ipv4Addr>().ok()),
    );
    let open = if tenant_block.is_some() {
        BTreeMap::new()
    } else {
        let mut closed = destination::node_own(hosts);
        closed.insert(gateway);
        destination::by_address(destinations, gateway, &closed)
    };
    let mut ranges = Vec::new();
    for (base, prefix) in [
        (0x0a000000u32, 8u32),
        (0xac100000, 12),
        (0xc0a80000, 16),
        (0x64400000, 10),
        (0xa9fe0000, 16),
        (0x7f000000, 8),
    ] {
        if destination::holds(base, prefix, own) {
            ranges.extend(destination::without(prefix, own, 24));
        } else {
            ranges.push((base, prefix));
        }
    }
    let rejected = destination::cut(ranges, open.keys().copied())
        .into_iter()
        .map(|(ip, length)| format!("{}/{length}", Ipv4Addr::from(ip)))
        .collect::<Vec<_>>();
    let host_list = host4
        .iter()
        .map(|ip| format!("{ip}/32"))
        .collect::<Vec<_>>()
        .join(",");
    let mut tenant = Vec::new();
    if let Some(block) = tenant_block {
        // Inside the service subnet a tenant's service reaches its own block only. Incus weighs a reject above an
        // allow, so the gateway stays rejected as it is for every service, and its DHCP and DNS answer through the
        // rules Incus keeps for a bridge's own address; the allows state that those two are meant to pass.
        tenant.push(rule(json!({"action":"reject",
            "destination":block.outside().join(","),
            "description":"the service subnet outside this tenant's block"})));
        for (protocol, port, what) in [
            ("udp", "67", "DHCP"),
            ("udp", "53", "DNS"),
            ("tcp", "53", "DNS"),
        ] {
            tenant.push(rule(
                json!({"action":"allow", "destination":format!("{gateway}/32"),
                "protocol":protocol, "destination_port":port,
                "description":format!("{what} on the bridge gateway")}),
            ));
        }
    }
    let mut rules = tenant;
    rules.extend(vec![
        rule(
            json!({"action":"reject", "destination":rejected.join(","), "description":"RFC1918 outside the service subnet, CGNAT (tailnet), link-local, loopback"}),
        ),
        rule(
            json!({"action":"reject", "destination":"::/0", "description":"all IPv6 (IPv4-only bridge), host addresses included"}),
        ),
        rule(
            json!({"action":"reject", "destination":"224.0.0.0/4,255.255.255.255/32,0.0.0.0/8,240.0.0.0/4", "description":"multicast, broadcast, this-network, reserved (as desktops)"}),
        ),
        rule(
            json!({"action":"reject", "destination":host_list, "description":"host addresses, the bridge gateway among them"}),
        ),
    ]);
    for (address, ports) in &open {
        let to = format!("{address}/32");
        let named = ports
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let others = destination::other_ports(ports);
        // An empty port list means every port: no rule is written for an address whose ports leave none over.
        if !others.is_empty() {
            rules.push(rule(json!({"action":"reject", "destination":to,
                "protocol":"tcp", "destination_port":others,
                "description":format!("the private address {address}: every tcp port but {named}")})));
        }
        rules.push(rule(
            json!({"action":"reject", "destination":to, "protocol":"udp",
            "description":format!("the private address {address}: udp")}),
        ));
        rules.push(rule(
            json!({"action":"reject", "destination":to, "protocol":"icmp4",
            "description":format!("the private address {address}: icmp")}),
        ));
        rules.push(rule(json!({"action":"allow", "destination":to,
            "protocol":"tcp", "destination_port":named,
            "description":format!("the private address {address}, tcp port {named}, named in the node's config")})));
    }
    if open.is_empty() {
        rules.push(rule(json!({"action":"allow", "description":"internet"})));
    } else {
        // Every address but the opened ones. An empty destination would match them too, and with them every IP
        // protocol no reject can name.
        let internet = destination::cut(vec![(0, 0)], open.keys().copied())
            .into_iter()
            .map(|(ip, length)| format!("{}/{length}", Ipv4Addr::from(ip)))
            .collect::<Vec<_>>()
            .join(",");
        rules.push(rule(
            json!({"action":"allow", "destination":internet, "description":"internet"}),
        ));
    }
    rules
}

/// Whether the caps are ones a tenant may hold: at least one instance and at most a block's worth, neither kind above
/// the count, a CPU, and both sizes readable.
pub fn caps_ok(caps: &TenantCaps, size_ok: impl Fn(&str) -> bool) -> bool {
    (1..=BLOCK_SIZE).contains(&caps.instances)
        && caps.containers <= caps.instances
        && caps.virtual_machines <= caps.instances
        && caps.cpu != 0
        && size_ok(&caps.memory)
        && size_ok(&caps.disk)
}

fn size_text_ok(value: &str) -> bool {
    [
        "B", "kB", "MB", "GB", "TB", "PB", "EB", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB",
    ]
    .iter()
    .any(|unit| {
        value.strip_suffix(unit).is_some_and(|number| {
            let mut bytes = number.bytes();
            bytes
                .next()
                .is_some_and(|first| matches!(first, b'1'..=b'9'))
                && bytes.all(|b| b.is_ascii_digit())
        })
    })
}

pub struct Tenant;

impl Plugin for Tenant {
    const ID: &'static str = "arca.tenant";
    type Config = TenantConfig;

    fn manifest() -> Manifest {
        Manifest {
            id: PluginId(Self::ID.into()),
            version: "0.1.0".into(),
            provides: vec![],
            effects: vec![
                (IncusKind::Project, EffectClass::B),
                (IncusKind::Profile, EffectClass::A),
            ],
            // A tenant's project is named by its id, so the manifest names the two tenant prefixes. The provider admits
            // only the project of a tenant whose entry exists.
            projects: vec![
                format!("{TENANT_PROJECT_PREFIX}*"),
                format!("{SCRATCH_TENANT_PROJECT_PREFIX}*"),
            ],
            observes: true,
        }
    }

    fn apply(ctx: &mut Ctx<'_>, cfg: &TenantConfig) -> Result<(), PluginError> {
        if !tenant_id_ok(&cfg.id)
            || ctx.entry().0 != tenant_entry(&cfg.id)
            || (cfg.project != tenant_project(&cfg.id, false)
                && cfg.project != tenant_project(&cfg.id, true))
            || cfg.bridge != BRIDGE
        {
            return Err(PluginError::Config(
                "a tenant's entry, project and bridge follow from its id".into(),
            ));
        }
        if !caps_ok(&cfg.caps, size_text_ok) || cfg.pool.is_empty() {
            return Err(PluginError::Config("invalid tenant caps or pool".into()));
        }
        // The block holds an address for every instance the caps allow. The subnet's top block has one fewer: its
        // last address is the broadcast address.
        if !parse_block(&cfg.block, &cfg.cidr)
            .is_some_and(|block| block.addresses().len() >= cfg.caps.instances as usize)
        {
            return Err(PluginError::Config(
                "a tenant's block is a /29 or a /28 of the service subnet, above the gateway's, that holds its instances".into(),
            ));
        }
        let current = observed(ctx, format!("/1.0/projects/{0}?project={0}", cfg.project))?;
        match current {
            None => {
                let mut config = serde_json::Map::new();
                config.insert("restricted.networks.access".into(), json!(cfg.bridge));
                for (key, value) in RESTRICTIONS {
                    config.insert(key.into(), json!(value));
                }
                for (key, value) in cfg.caps.limits() {
                    config.insert(key.into(), json!(value));
                }
                create(
                    ctx,
                    IncusKind::Project,
                    &cfg.project,
                    &cfg.project,
                    json!({
                        "name": cfg.project,
                        "description": "Arca tenant; managed by Arca",
                        "config": Value::Object(config)
                    }),
                )?;
            }
            Some(current) => {
                // The caps follow the entry: a changed cap is one journaled write of the project's whole config, under
                // the ETag of this read (Rule 43).
                let mut changes = BTreeMap::new();
                for (key, value) in cfg.caps.limits() {
                    if current["config"][key].as_str() != Some(value.as_str()) {
                        changes.insert(key.to_string(), Some(value));
                    }
                }
                if !changes.is_empty() {
                    ctx.effect(EffectOp::IncusPatchConfig {
                        object: project_ref(&cfg.project),
                        config: changes,
                    })?;
                }
            }
        }
        if observed(
            ctx,
            format!("/1.0/profiles/{PROFILE}?project={}", cfg.project),
        )?
        .is_none()
        {
            create(
                ctx,
                IncusKind::Profile,
                &cfg.project,
                PROFILE,
                json!({"name": PROFILE,
                    "description": format!("Arca service (project {}); managed by Arca", cfg.project),
                    "config": {"limits.memory": PROFILE_MEMORY, "limits.cpu": PROFILE_CPU.to_string(),
                        "limits.cpu.priority":"5", "limits.processes":"4096", "security.privileged":"false",
                        "security.nesting":"false", "security.idmap.isolated":"true", "security.guestapi":"false"},
                    "devices": {"root":{"type":"disk", "path":"/", "pool":cfg.pool, "size":CONTAINER_ROOT_SIZE}}}),
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rules as they were built before a service could have a destination, kept word for word: what the rules of a
    /// service with none must still be, and what a destination's rules are measured against.
    fn egress_rules_before(
        gateway: Ipv4Addr,
        hosts: &Value,
        tenant_block: Option<Block>,
    ) -> Vec<Value> {
        let own = u32::from(gateway) & 0xffffff00;
        let mut rejected = Vec::new();
        for (base, prefix) in [
            (0x0a000000u32, 8u32),
            (0xac100000, 12),
            (0xc0a80000, 16),
            (0x64400000, 10),
            (0xa9fe0000, 16),
            (0x7f000000, 8),
        ] {
            if own & (u32::MAX << (32 - prefix)) == base {
                let mut blocks = Vec::new();
                for length in prefix + 1..=24 {
                    let size = 1u32 << (32 - length);
                    blocks.push(((own & (u32::MAX << (32 - length))) ^ size, length));
                }
                blocks.sort_unstable();
                rejected.extend(
                    blocks
                        .into_iter()
                        .map(|(ip, length)| format!("{}/{length}", Ipv4Addr::from(ip))),
                );
            } else {
                rejected.push(format!("{}/{prefix}", Ipv4Addr::from(base)));
            }
        }
        let mut host4 = std::collections::BTreeSet::from([gateway]);
        host4.extend(
            hosts
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|host| host.as_str()?.split('/').next()?.parse::<Ipv4Addr>().ok()),
        );
        let destinations = host4
            .iter()
            .map(|ip| format!("{ip}/32"))
            .collect::<Vec<_>>()
            .join(",");
        let mut tenant = Vec::new();
        if let Some(block) = tenant_block {
            // Inside the service subnet a tenant's service reaches its own block only. Incus weighs a reject above an
            // allow, so the gateway stays rejected as it is for every service, and its DHCP and DNS answer through the
            // rules Incus keeps for a bridge's own address; the allows state that those two are meant to pass.
            tenant.push(rule(json!({"action":"reject",
                "destination":block.outside().join(","),
                "description":"the service subnet outside this tenant's block"})));
            for (protocol, port, what) in [
                ("udp", "67", "DHCP"),
                ("udp", "53", "DNS"),
                ("tcp", "53", "DNS"),
            ] {
                tenant.push(rule(
                    json!({"action":"allow", "destination":format!("{gateway}/32"),
                    "protocol":protocol, "destination_port":port,
                    "description":format!("{what} on the bridge gateway")}),
                ));
            }
        }
        let mut rules = tenant;
        rules.extend(vec![
            rule(
                json!({"action":"reject", "destination":rejected.join(","), "description":"RFC1918 outside the service subnet, CGNAT (tailnet), link-local, loopback"}),
            ),
            rule(
                json!({"action":"reject", "destination":"::/0", "description":"all IPv6 (IPv4-only bridge), host addresses included"}),
            ),
            rule(
                json!({"action":"reject", "destination":"224.0.0.0/4,255.255.255.255/32,0.0.0.0/8,240.0.0.0/4", "description":"multicast, broadcast, this-network, reserved (as desktops)"}),
            ),
            rule(
                json!({"action":"reject", "destination":destinations, "description":"host addresses, the bridge gateway among them"}),
            ),
            rule(json!({"action":"allow", "description":"internet"})),
        ]);
        rules
    }

    /// What Incus does with a packet under `rules`, as its documentation states it: the order of the rules does not
    /// count, a reject is weighed before an allow, and a packet no rule matches is rejected. `protocol` is `tcp`, `udp`,
    /// `icmp4` or any other name for an IP protocol no rule can name.
    fn allowed(rules: &[Value], to: Ipv4Addr, protocol: &str, port: u16) -> bool {
        let matches = |rule: &Value| {
            let destination = rule["destination"].as_str().unwrap();
            let inside = destination.is_empty()
                || destination.split(',').any(|range| {
                    let (base, prefix) = range.split_once('/').expect("a CIDR");
                    let Ok(base) = base.parse::<Ipv4Addr>() else {
                        return false;
                    };
                    destination::holds(u32::from(base), prefix.parse().unwrap(), u32::from(to))
                });
            let wanted = rule["protocol"].as_str().unwrap();
            let ports = rule["destination_port"].as_str().unwrap();
            assert!(ports.is_empty() || matches!(wanted, "tcp" | "udp"));
            assert!(matches!(wanted, "" | "tcp" | "udp" | "icmp4"));
            inside
                && (wanted.is_empty() || wanted == protocol)
                && (ports.is_empty()
                    || ports.split(',').any(|range| {
                        let (first, last) = range.split_once('-').unwrap_or((range, range));
                        (first.parse::<u16>().unwrap()..=last.parse::<u16>().unwrap())
                            .contains(&port)
                    }))
        };
        let of = |action: &str| {
            rules
                .iter()
                .filter(|rule| rule["action"] == action)
                .any(matches)
        };
        assert!(
            rules
                .iter()
                .all(|rule| rule["action"] == "reject" || rule["action"] == "allow")
        );
        !of("reject") && of("allow")
    }

    /// Every address where a decision could change: both ends of every range any rule of either set names, the address
    /// before and after each, and a spread of others.
    fn edges(sets: &[&[Value]], more: &[Ipv4Addr]) -> Vec<Ipv4Addr> {
        let mut out = std::collections::BTreeSet::new();
        for range in sets
            .iter()
            .flat_map(|rules| rules.iter())
            .flat_map(|rule| rule["destination"].as_str().unwrap().split(','))
        {
            let Some((base, prefix)) = range.split_once('/') else {
                continue;
            };
            let Ok(base) = base.parse::<Ipv4Addr>() else {
                continue;
            };
            let first = u32::from(base);
            let last = first
                | !u32::MAX
                    .checked_shl(32 - prefix.parse::<u32>().unwrap())
                    .unwrap_or(0);
            for ip in [first, last] {
                out.extend([ip.wrapping_sub(1), ip, ip.wrapping_add(1)]);
            }
        }
        out.extend(more.iter().map(|ip| u32::from(*ip)));
        // And one address in every /8 there is, whatever the rules name.
        out.extend((0..256u32).map(|n| (n << 24) | 0x0005_0607));
        out.into_iter().map(Ipv4Addr::from).collect()
    }

    fn to(address: [u8; 4], port: u16) -> Destination {
        Destination {
            address: Ipv4Addr::from(address),
            port,
        }
    }

    #[test]
    fn a_service_with_no_destination_has_the_rules_it_always_had() {
        let hosts = json!([
            "10.20.30.1/24",
            "100.99.1.2/32",
            "172.20.7.5/24",
            "fd00::1/64"
        ]);
        for gateway in [
            [10, 90, 0, 1],
            [172, 20, 5, 1],
            [192, 168, 44, 1],
            [100, 70, 3, 1],
            [198, 51, 100, 1],
        ] {
            let gateway = Ipv4Addr::from(gateway);
            let block = blocks(
                &format!("{}/24", Ipv4Addr::from(u32::from(gateway) & 0xffff_ff00)),
                29,
            )
            .unwrap()[3];
            for tenant in [None, Some(block)] {
                assert_eq!(
                    egress_rules(gateway, &hosts, tenant, &[]),
                    egress_rules_before(gateway, &hosts, tenant),
                    "{gateway} {tenant:?}"
                );
            }
            // A tenant's rules take no destination at all.
            assert_eq!(
                egress_rules(
                    gateway,
                    &hosts,
                    Some(block),
                    &[to([10, 4, 0, 9], 443), to([100, 100, 1, 1], 443)]
                ),
                egress_rules_before(gateway, &hosts, Some(block)),
            );
        }
    }

    #[test]
    fn a_destination_opens_one_address_on_one_tcp_port_and_nothing_else() {
        let hosts = json!(["10.20.30.1/24", "100.99.1.2/32", "172.20.7.5/24"]);
        // The two a first user needs: an address on another bridge of the node, and one on the tailnet. The gateway is
        // tried in the same private range as the first, in another, and outside every one.
        let bridge = to([10, 20, 30, 40], 443);
        let tailnet = to([100, 101, 102, 103], 443);
        for gateway in [
            [10, 90, 0, 1],
            [192, 168, 44, 1],
            [100, 70, 3, 1],
            [198, 51, 100, 1],
        ] {
            let gateway = Ipv4Addr::from(gateway);
            for wanted in [
                vec![bridge],
                vec![tailnet],
                vec![bridge, tailnet],
                vec![bridge, to([10, 20, 30, 40], 8443), to([172, 16, 0, 1], 22)],
            ] {
                let before = egress_rules_before(gateway, &hosts, None);
                let after = egress_rules(gateway, &hosts, None, &wanted);
                let named =
                    |ip: Ipv4Addr, port: u16| wanted.contains(&Destination { address: ip, port });
                let more: Vec<Ipv4Addr> = wanted.iter().map(|d| d.address).collect();
                for ip in edges(&[&before, &after], &more) {
                    let is_destination = wanted.iter().any(|d| d.address == ip);
                    // tcp: every port of a destination's address, and the ports around the named ones everywhere.
                    let ports: Vec<u16> = if is_destination {
                        (1..=65535).collect()
                    } else {
                        vec![1, 22, 80, 442, 443, 444, 8443, 65535]
                    };
                    for port in ports {
                        let was = allowed(&before, ip, "tcp", port);
                        let now = allowed(&after, ip, "tcp", port);
                        if named(ip, port) {
                            assert!(!was && now, "{gateway}: tcp {ip}:{port} is the destination");
                        } else {
                            assert_eq!(was, now, "{gateway}: tcp {ip}:{port}");
                            assert!(
                                !(is_destination && now),
                                "{gateway}: tcp {ip}:{port} is open"
                            );
                        }
                    }
                    for port in [1, 53, 123, 443, 65535] {
                        assert_eq!(
                            allowed(&before, ip, "udp", port),
                            allowed(&after, ip, "udp", port),
                            "{gateway}: udp {ip}:{port}"
                        );
                    }
                    assert_eq!(
                        allowed(&before, ip, "icmp4", 0),
                        allowed(&after, ip, "icmp4", 0),
                        "{gateway}: icmp {ip}"
                    );
                    // Every other IP protocol, which no rule can name: decided as before everywhere, so closed
                    // for a destination's address too. Nothing but tcp on the named port is newly open.
                    for protocol in ["gre", "ipip", "esp", "sctp"] {
                        assert_eq!(
                            allowed(&before, ip, protocol, 0),
                            allowed(&after, ip, protocol, 0),
                            "{gateway}: {protocol} {ip}"
                        );
                        assert!(
                            !(is_destination && allowed(&after, ip, protocol, 0)),
                            "{gateway}: {protocol} {ip} is open"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn an_address_of_the_host_or_one_that_is_no_destination_is_never_opened() {
        let hosts = json!(["10.20.30.1/24", "100.99.1.2/32"]);
        let gateway = Ipv4Addr::new(10, 90, 0, 1);
        let before = egress_rules_before(gateway, &hosts, None);
        // The node's own address on the other bridge, its tailnet address, the bridge gateway, an address of the
        // service subnet, loopback, the metadata address, a public address and a port of 0: a record that named any of
        // them changes no rule.
        for closed in [
            to([10, 20, 30, 1], 443),
            to([100, 99, 1, 2], 443),
            to([10, 90, 0, 1], 443),
            to([10, 90, 0, 7], 443),
            to([127, 0, 0, 1], 443),
            to([169, 254, 169, 254], 80),
            to([8, 8, 8, 8], 443),
            to([10, 20, 30, 40], 0),
        ] {
            assert_eq!(
                egress_rules(gateway, &hosts, None, &[closed]),
                before,
                "{closed:?}"
            );
        }
        // The network and broadcast addresses of the node's networks are the node's own too, when Incus tells the
        // netmask: a packet to one is answered by the host itself.
        let with_masks = json!(["10.20.30.1/24", "100.99.1.2/32"]);
        assert_eq!(
            egress_rules(gateway, &with_masks, None, &[]),
            egress_rules_before(gateway, &with_masks, None)
        );
        for closed in [to([10, 20, 30, 0], 443), to([10, 20, 30, 255], 443)] {
            assert_eq!(
                egress_rules(gateway, &with_masks, None, &[closed]),
                before,
                "{closed:?}"
            );
        }
        // Beside a real one it is still left out.
        let rules = egress_rules(
            gateway,
            &hosts,
            None,
            &[to([10, 20, 30, 1], 443), to([10, 20, 30, 40], 443)],
        );
        assert!(!allowed(&rules, Ipv4Addr::new(10, 20, 30, 1), "tcp", 443));
        assert!(allowed(&rules, Ipv4Addr::new(10, 20, 30, 40), "tcp", 443));
    }

    #[test]
    fn a_destination_at_the_end_of_a_range_or_beside_another_opens_only_itself() {
        let hosts = json!(["10.20.30.1/24", "100.99.1.2/32"]);
        let gateway = Ipv4Addr::new(10, 90, 0, 1);
        let before = egress_rules_before(gateway, &hosts, None);
        let cases: Vec<Vec<Destination>> = vec![
            // Both ends of each range, and the addresses beside the service /24.
            vec![to([10, 0, 0, 0], 443)],
            vec![to([10, 255, 255, 255], 443)],
            vec![to([172, 16, 0, 0], 443)],
            vec![to([172, 31, 255, 255], 443)],
            vec![to([192, 168, 0, 0], 443)],
            vec![to([192, 168, 255, 255], 443)],
            vec![to([100, 64, 0, 0], 443)],
            vec![to([100, 127, 255, 255], 443)],
            vec![to([10, 89, 255, 255], 443)],
            vec![to([10, 90, 1, 0], 443)],
            // Two neighbours, with the same port and with another; four in four ranges.
            vec![to([10, 20, 30, 40], 443), to([10, 20, 30, 41], 443)],
            vec![to([10, 20, 30, 41], 22), to([10, 20, 30, 40], 443)],
            vec![
                to([10, 0, 0, 0], 443),
                to([172, 31, 255, 255], 443),
                to([192, 168, 0, 0], 8443),
                to([100, 127, 255, 255], 22),
            ],
        ];
        for wanted in cases {
            let after = egress_rules(gateway, &hosts, None, &wanted);
            let more: Vec<Ipv4Addr> = wanted.iter().map(|d| d.address).collect();
            for ip in edges(&[&before, &after], &more) {
                for port in [1, 21, 22, 23, 442, 443, 444, 8442, 8443, 8444, 65535] {
                    let named = wanted.contains(&Destination { address: ip, port });
                    assert_eq!(
                        allowed(&after, ip, "tcp", port),
                        named || allowed(&before, ip, "tcp", port),
                        "{wanted:?}: tcp {ip}:{port}"
                    );
                    assert!(!(named && allowed(&before, ip, "tcp", port)));
                }
                for protocol in ["udp", "icmp4", "gre", "sctp"] {
                    assert_eq!(
                        allowed(&after, ip, protocol, 443),
                        allowed(&before, ip, protocol, 443),
                        "{wanted:?}: {protocol} {ip}"
                    );
                }
            }
            // Every named one is open, so none was dropped on the way.
            for one in &wanted {
                assert!(allowed(&after, one.address, "tcp", one.port), "{one:?}");
            }
        }
    }

    #[test]
    fn the_rules_of_a_destination_are_written_as_incus_reads_them() {
        let rules = egress_rules(
            Ipv4Addr::new(10, 90, 0, 1),
            &json!([]),
            None,
            &[to([10, 20, 30, 40], 443)],
        );
        let of = |index: usize| {
            let rule = &rules[index];
            [
                rule["action"].as_str().unwrap(),
                rule["destination"].as_str().unwrap(),
                rule["protocol"].as_str().unwrap(),
                rule["destination_port"].as_str().unwrap(),
            ]
            .map(str::to_owned)
        };
        assert_eq!(rules.len(), 9);
        assert_eq!(
            of(4),
            ["reject", "10.20.30.40/32", "tcp", "1-442,444-65535"]
        );
        assert_eq!(of(5), ["reject", "10.20.30.40/32", "udp", ""]);
        assert_eq!(of(6), ["reject", "10.20.30.40/32", "icmp4", ""]);
        assert_eq!(of(7), ["allow", "10.20.30.40/32", "tcp", "443"]);
        // The last allow names every address but the opened one: 32 ranges, one of each length, none of which holds
        // it. So what is not tcp on port 443 to that address matches no rule, and meets the NIC's default reject.
        let internet = of(8);
        assert_eq!(internet[0], "allow");
        assert_eq!(internet[2..], ["".to_owned(), "".to_owned()]);
        let ranges: Vec<&str> = internet[1].split(',').collect();
        assert_eq!(ranges.len(), 32);
        assert!(ranges.contains(&"128.0.0.0/1") && ranges.contains(&"10.20.30.41/32"));
        let opened = u32::from(Ipv4Addr::new(10, 20, 30, 40));
        assert!(!ranges.iter().any(|range| {
            let (base, prefix) = range.split_once('/').unwrap();
            destination::holds(
                u32::from(base.parse::<Ipv4Addr>().unwrap()),
                prefix.parse().unwrap(),
                opened,
            )
        }));
        // 10.0.0.0/8 without the service /24 and without the one address: no range of the reject holds it.
        let reject = rules[0]["destination"].as_str().unwrap();
        assert!(
            !reject
                .split(',')
                .any(|range| range == "10.0.0.0/8" || range == "10.20.30.40/32")
        );
        assert!(reject.split(',').any(|range| range == "10.20.30.41/32"));
        assert!(reject.split(',').any(|range| range == "100.64.0.0/10"));
    }

    fn ip(last: u8) -> Ipv4Addr {
        Ipv4Addr::new(192, 0, 2, last)
    }

    #[test]
    fn blocks_run_from_the_top_of_the_subnet_down_and_never_hold_the_gateway() {
        let small = blocks("192.0.2.0/24", 29).unwrap();
        assert_eq!(small.len(), 30);
        assert_eq!(small[0].base, ip(248));
        assert_eq!(small[29].base, ip(16));
        let large = blocks("192.0.2.0/24", 28).unwrap();
        assert_eq!(large.len(), 15);
        assert_eq!(large[0].base, ip(240));
        assert_eq!(large[14].base, ip(16));
        assert_eq!(
            parse_block("192.0.2.224/28", "192.0.2.0/24"),
            Some(Block {
                base: ip(224),
                prefix: 28
            })
        );
        assert_eq!(
            parse_block("192.0.2.232/29", "192.0.2.0/24"),
            Some(Block {
                base: ip(232),
                prefix: 29
            })
        );
        for bad in [
            "192.0.2.0/28",
            "192.0.2.8/29",
            "192.0.2.225/28",
            "192.0.2.228/29",
            "192.0.2.224/27",
            "192.0.2.224/30",
            "192.0.3.224/28",
            "192.0.2.224",
        ] {
            assert_eq!(parse_block(bad, "192.0.2.0/24"), None, "{bad}");
        }
        assert!(blocks("192.0.2.0/25", 29).is_err());
        assert!(blocks("192.0.2.0/24", 27).is_err());
    }

    #[test]
    fn a_block_is_a_slash_29_unless_the_tenant_may_hold_more_than_eight_instances() {
        let caps = |instances| TenantCaps {
            instances,
            containers: instances,
            virtual_machines: 0,
            cpu: 4,
            memory: "8GiB".into(),
            disk: "64GiB".into(),
        };
        assert_eq!(block_prefix(&caps(1)), 29);
        assert_eq!(block_prefix(&caps(8)), 29);
        assert_eq!(block_prefix(&caps(9)), 28);
        assert_eq!(block_prefix(&caps(16)), 28);
    }

    #[test]
    fn a_block_holds_its_addresses_and_the_top_one_leaves_out_the_broadcast_address() {
        let block = Block {
            base: ip(224),
            prefix: 28,
        };
        assert_eq!(block.addresses().len(), 16);
        assert!(block.holds(ip(239)) && !block.holds(ip(240)) && !block.holds(ip(223)));
        let small = Block {
            base: ip(232),
            prefix: 29,
        };
        assert_eq!(small.addresses(), (232..=239).map(ip).collect::<Vec<_>>());
        assert!(small.overlaps(block) && block.overlaps(small));
        assert!(!small.overlaps(Block {
            base: ip(224),
            prefix: 29
        }));
        let top = Block {
            base: ip(248),
            prefix: 29,
        };
        assert_eq!(top.addresses().len(), 7);
        assert!(!top.addresses().contains(&ip(255)));
        assert_eq!(small.text(), "192.0.2.232/29");
    }

    #[test]
    fn the_ranges_outside_a_block_cover_the_subnet_and_nothing_of_the_block() {
        for block in [
            Block {
                base: ip(224),
                prefix: 28,
            },
            Block {
                base: ip(232),
                prefix: 29,
            },
            Block {
                base: ip(16),
                prefix: 29,
            },
        ] {
            let ranges = block.outside();
            assert_eq!(ranges.len() as u32, block.prefix - 24);
            // Every address of the /24 is in exactly one range, or in the block.
            for last in 0u32..=255 {
                let address = u32::from(ip(0)) | last;
                let covered = ranges
                    .iter()
                    .filter(|range| {
                        let (base, length) = range.split_once('/').unwrap();
                        let length: u32 = length.parse().unwrap();
                        let mask = u32::MAX << (32 - length);
                        address & mask == u32::from(base.parse::<Ipv4Addr>().unwrap())
                    })
                    .count();
                assert_eq!(
                    covered,
                    usize::from(!block.holds(Ipv4Addr::from(address))),
                    "{last}"
                );
            }
        }
        assert_eq!(
            Block {
                base: ip(224),
                prefix: 28
            }
            .outside(),
            [
                "192.0.2.0/25",
                "192.0.2.128/26",
                "192.0.2.192/27",
                "192.0.2.240/28"
            ]
        );
    }

    /// A provider nothing may reach: the block is checked before the first read.
    struct Unreached;

    impl arca_kernel::plugin::Provider for Unreached {
        fn observe(
            &self,
            _: &arca_kernel::desired::EntryId,
            _: &[String],
            _: &arca_kernel::plugin::Observe,
        ) -> Result<Value, arca_kernel::plugin::EffectRefusal> {
            Err(arca_kernel::plugin::EffectRefusal::Provider(
                "reached the provider".into(),
            ))
        }
        fn plan(
            &self,
            _: &arca_kernel::desired::EntryId,
            _: &EffectOp,
        ) -> Result<arca_kernel::plugin::Planned, arca_kernel::plugin::EffectRefusal> {
            unreachable!("no write is planned")
        }
        fn effect(
            &mut self,
            _: &arca_kernel::desired::EntryId,
            _: &EffectOp,
            _: &arca_kernel::plugin::Planned,
        ) -> Result<
            (
                arca_kernel::ownership::ObjectRef,
                arca_kernel::journal::Inverse,
            ),
            arca_kernel::plugin::EffectRefusal,
        > {
            unreachable!("no write is made")
        }
    }

    /// What `Tenant::apply` answers for a tenant of `instances` at `block`: `Config` when the record is refused,
    /// and the provider's refusal when the record passed every check and the first read was made.
    fn applied(instances: u32, block: &str) -> PluginError {
        let cfg = TenantConfig {
            id: "aaaaaaaa".into(),
            project: "arcak-t-aaaaaaaa".into(),
            bridge: BRIDGE.into(),
            cidr: "192.0.2.0/24".into(),
            block: block.into(),
            pool: "pool1".into(),
            caps: TenantCaps {
                instances,
                containers: instances,
                virtual_machines: 0,
                cpu: 4,
                memory: "8GiB".into(),
                disk: "64GiB".into(),
            },
            created: 0,
        };
        let manifest = Tenant::manifest();
        let mut provider = Unreached;
        let mut journal =
            arca_kernel::journal::Journal::open(arca_kernel::journal::MemJournal::default())
                .expect("journal");
        Tenant::apply(
            &mut Ctx::new(
                arca_kernel::desired::EntryId("tenant-aaaaaaaa".into()),
                &manifest,
                &mut provider,
                &mut journal,
                0,
            ),
            &cfg,
        )
        .expect_err("the unreachable provider ends every apply")
    }

    #[test]
    fn apply_refuses_a_block_with_fewer_addresses_than_the_tenant_may_hold() {
        let refused = |error: PluginError| matches!(error, PluginError::Config(_));
        // The subnet's top /29 has 7 addresses: 8 instances do not fit, 7 do, and 8 fit one block lower.
        assert!(refused(applied(8, "192.0.2.248/29")));
        assert!(!refused(applied(7, "192.0.2.248/29")));
        assert!(!refused(applied(8, "192.0.2.240/29")));
        // The top /28 has 15: 16 instances do not fit, 15 do, and 16 fit one block lower.
        assert!(refused(applied(16, "192.0.2.240/28")));
        assert!(!refused(applied(15, "192.0.2.240/28")));
        assert!(!refused(applied(16, "192.0.2.224/28")));
        // A /29 never holds more than 8, wherever it is.
        assert!(refused(applied(9, "192.0.2.232/29")));
    }

    #[test]
    fn caps_are_bounded_by_a_block_and_by_their_own_count() {
        let caps = |instances, containers, virtual_machines| TenantCaps {
            instances,
            containers,
            virtual_machines,
            cpu: 4,
            memory: "8GiB".into(),
            disk: "64GiB".into(),
        };
        assert!(caps_ok(&caps(16, 16, 2), size_text_ok));
        assert!(!caps_ok(&caps(17, 4, 0), size_text_ok));
        assert!(!caps_ok(&caps(0, 0, 0), size_text_ok));
        assert!(!caps_ok(&caps(4, 5, 0), size_text_ok));
        assert!(!caps_ok(&caps(4, 4, 5), size_text_ok));
        let mut bad = caps(4, 4, 1);
        bad.memory = "8".into();
        assert!(!caps_ok(&bad, size_text_ok));
        let mut bad = caps(4, 4, 1);
        bad.cpu = 0;
        assert!(!caps_ok(&bad, size_text_ok));
    }
}
