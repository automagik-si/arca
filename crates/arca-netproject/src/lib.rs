//! The shared service project and its bridge. The project is owned by the entry that first needed it: [`NetProject`] when
//! the node's first service used the managed network, [`BridgedProject`] when it was a virtual machine on a host bridge.
//! A node that later gains the other network keeps the one project, and its owner widens it (Rules 52 and 53 of the
//! ownership document).
use arca_kernel::{
    desired::PluginId,
    journal::EffectClass,
    ownership::IncusKind,
    plugin::{Ctx, EffectOp, EffectRefusal, Manifest, Observe, Plugin, PluginError},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::net::Ipv4Addr;

pub const PROJECT: &str = "arcak-svc";
pub const BRIDGE: &str = "arcak-svcbr";
pub const PROFILE: &str = "arca-service";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NetProjectConfig {
    pub project: String,
    pub bridge: String,
    pub cidr: String,
    /// The project's `limits.virtual-machines`: `0` until the node's first virtual machine, then the node's cap.
    pub virtual_machines: u32,
    /// The project is the bridged entry's ([`BridgedProject`]): this entry makes the bridge and the profile, and never
    /// writes the project. An entry an earlier kernel wrote has no such key and owns its project.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub shared_project: bool,
    /// The project's NICs may name a host bridge: `restricted.devices.nic` is `allow` and the bridge grant is gone. Set
    /// with the node's first virtual machine on a host bridge.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub host_bridges: bool,
}
pub struct NetProject;

/// Validate a service /24 and return its gateway. Errors never contain addresses.
pub fn check_cidr(cidr: &str, reserved: &[String]) -> Result<Ipv4Addr, &'static str> {
    let parse = |s: &str| -> Option<(u32, u32)> {
        let (ip, prefix) = s.split_once('/')?;
        let ip = u32::from(ip.parse::<Ipv4Addr>().ok()?);
        let prefix = prefix.parse::<u32>().ok().filter(|n| *n <= 32)?;
        let mask = u32::MAX.checked_shl(32 - prefix).unwrap_or(0);
        Some((ip & mask, mask))
    };
    let (ip, mask) = parse(cidr)
        .filter(|(_, m)| *m == 0xffffff00)
        .ok_or("service CIDR must be an IPv4 /24")?;
    for range in reserved {
        let (other, other_mask) = parse(range).ok_or("reserved CIDR must be IPv4")?;
        let common = mask & other_mask;
        if ip & common == other & common {
            return Err("service CIDR overlaps a reserved CIDR");
        }
    }
    Ok(Ipv4Addr::from(ip | 1))
}

/// A static manifest for the node's service project and network home.
pub fn manifest(id: &str, effects: Vec<(IncusKind, EffectClass)>) -> Manifest {
    Manifest {
        id: PluginId(id.into()),
        version: "0.1.0".into(),
        provides: vec![],
        effects,
        projects: vec![PROJECT.into(), "default".into()],
        observes: true,
    }
}

/// Read an own object, distinguishing absence from every other refusal.
pub fn observed(ctx: &Ctx<'_>, path: String) -> Result<Option<Value>, PluginError> {
    match ctx.observe(Observe::IncusGet { path }) {
        Ok(value) => {
            if value["config"]["user.arcakernel.entry"].as_str() != Some(ctx.entry().0.as_str()) {
                return Err(PluginError::Failed(
                    "the existing object belongs to another entry".into(),
                ));
            }
            Ok(Some(value))
        }
        Err(EffectRefusal::Provider(why)) if why == "Incus answered 404" => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Names are entry suffixes, so paths and scoped secret names cannot escape their owner.
pub fn name_ok(name: &str, prefix: &str, max: usize) -> bool {
    !name.is_empty()
        && name.len() <= max
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && arca_kernel::desired::EntryId::parse(&format!("{prefix}-{name}")).is_some()
}

pub fn create(
    ctx: &mut Ctx<'_>,
    kind: IncusKind,
    project: &str,
    name: &str,
    body: Value,
) -> Result<(), PluginError> {
    ctx.effect(EffectOp::IncusCreate {
        kind,
        project: project.into(),
        parent: None,
        name: name.into(),
        body,
    })?;
    Ok(())
}

/// The entry that owns the service project on a bridged node.
pub const BRIDGED_ENTRY: &str = "bridged-arcak-svc";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BridgedProjectConfig {
    pub project: String,
    /// The caps on everything in the project together, set as its `limits.*` keys when it is created.
    pub virtual_machines: u32,
    pub cpu: u32,
    pub memory: String,
    pub disk: String,
    /// The node also has the managed service network, so the project holds containers too: no container cap, no
    /// aggregate CPU, memory or disk cap (Incus would count the containers against them), and proxy devices allowed. Set
    /// with the node's first service on the managed network. An entry an earlier kernel wrote has no such key.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub managed: bool,
}

/// The `limits.*` keys a bridged-only project carries and a project that also holds containers does not.
const BRIDGED_ONLY_LIMITS: [&str; 4] = [
    "limits.containers",
    "limits.cpu",
    "limits.memory",
    "limits.disk",
];

fn project_ref(project: &str) -> arca_kernel::ownership::ObjectRef {
    arca_kernel::ownership::ObjectRef {
        kind: IncusKind::Project,
        project: project.into(),
        parent: None,
        name: project.into(),
        uuid: None,
    }
}

/// The service project of a node whose first service was a virtual machine on a host bridge. It creates the project and
/// nothing else: no Incus network, no profile. The project lets a NIC name a host bridge
/// (`restricted.devices.nic=allow`) and carries the node's caps as `limits.*`. While `managed` is unset nothing changes
/// the project after its create: new caps are set by hand. With `managed` the project is widened once, by a journaled
/// config write, so the managed service network's containers fit in it.
pub struct BridgedProject;

impl Plugin for BridgedProject {
    const ID: &'static str = "arca.bridgedproject";
    type Config = BridgedProjectConfig;
    fn manifest() -> Manifest {
        Manifest {
            id: PluginId(Self::ID.into()),
            version: "0.1.0".into(),
            provides: vec![],
            effects: vec![(IncusKind::Project, EffectClass::B)],
            projects: vec![PROJECT.into()],
            observes: true,
        }
    }
    fn apply(ctx: &mut Ctx<'_>, cfg: &BridgedProjectConfig) -> Result<(), PluginError> {
        if cfg.project != PROJECT || ctx.entry().0 != BRIDGED_ENTRY {
            return Err(PluginError::Config(
                "the bridged entry owns the service project".into(),
            ));
        }
        // `observed` refuses a project another entry made: a project the managed network's entry owns is widened by that
        // entry, never by this one.
        let current = observed(
            ctx,
            format!("/1.0/projects/{}?project={}", cfg.project, cfg.project),
        )?;
        let Some(current) = current else {
            let mut config = json!({
                "restricted.devices.nic": "allow",
                "limits.virtual-machines": cfg.virtual_machines.to_string()
            });
            if cfg.managed {
                config["restricted.devices.proxy"] = json!("allow");
            } else {
                config["limits.containers"] = json!("0");
                config["limits.cpu"] = json!(cfg.cpu.to_string());
                config["limits.memory"] = json!(cfg.memory);
                config["limits.disk"] = json!(cfg.disk);
            }
            return create(
                ctx,
                IncusKind::Project,
                &cfg.project,
                &cfg.project,
                json!({"name": cfg.project, "config": config}),
            );
        };
        if !cfg.managed {
            return Ok(());
        }
        let mut changes = std::collections::BTreeMap::new();
        for key in BRIDGED_ONLY_LIMITS {
            if current["config"].get(key).is_some() {
                changes.insert(key.to_string(), None);
            }
        }
        if current["config"]["restricted.devices.proxy"].as_str() != Some("allow") {
            changes.insert("restricted.devices.proxy".into(), Some("allow".into()));
        }
        if !changes.is_empty() {
            ctx.effect(EffectOp::IncusPatchConfig {
                object: project_ref(&cfg.project),
                config: changes,
            })?;
        }
        Ok(())
    }
}

impl Plugin for NetProject {
    const ID: &'static str = "arca.netproject";
    type Config = NetProjectConfig;
    fn manifest() -> Manifest {
        manifest(
            Self::ID,
            vec![
                (IncusKind::Project, EffectClass::B),
                (IncusKind::Network, EffectClass::B),
                (IncusKind::Profile, EffectClass::A),
            ],
        )
    }
    /// The project's proxy allowance is repaired on every pass, even when the entry is unchanged. Opting in also recreates
    /// a bridge, project or profile deleted outside Arca on the next pass.
    fn reconciles_when_unchanged() -> bool {
        true
    }
    fn apply(ctx: &mut Ctx<'_>, cfg: &NetProjectConfig) -> Result<(), PluginError> {
        if cfg.project != PROJECT || cfg.bridge != BRIDGE {
            return Err(PluginError::Config(
                "service project and bridge required".into(),
            ));
        }
        let gateway = check_cidr(&cfg.cidr, &[]).map_err(|e| PluginError::Config(e.into()))?;
        // The bridge precedes its project's restricted network grant.
        if observed(ctx, format!("/1.0/networks/{}?project=default", cfg.bridge))?.is_none() {
            create(
                ctx,
                IncusKind::Network,
                "default",
                &cfg.bridge,
                json!({"name":cfg.bridge,"type":"bridge","config":{"ipv4.address":format!("{gateway}/24"),"ipv4.nat":"true","ipv6.address":"none"}}),
            )?;
        }
        if !cfg.shared_project {
            let project = observed(
                ctx,
                format!("/1.0/projects/{}?project={}", cfg.project, cfg.project),
            )?;
            let virtual_machines = cfg.virtual_machines.to_string();
            let config = match project {
                Some(project) => project["config"].clone(),
                None => {
                    let config = json!({"limits.virtual-machines":virtual_machines,"restricted.networks.access":cfg.bridge,"restricted.devices.proxy":"allow"});
                    create(
                        ctx,
                        IncusKind::Project,
                        &cfg.project,
                        &cfg.project,
                        json!({"name":cfg.project,"config":config}),
                    )?;
                    config
                }
            };
            let mut changes = std::collections::BTreeMap::new();
            if config["restricted.devices.proxy"].as_str() != Some("allow") {
                changes.insert("restricted.devices.proxy".into(), Some("allow".into()));
            }
            // `0` is what every project was created with before a node could hold both kinds: it is never written back
            // over a value set by hand.
            if cfg.virtual_machines != 0
                && config["limits.virtual-machines"].as_str() != Some(virtual_machines.as_str())
            {
                changes.insert("limits.virtual-machines".into(), Some(virtual_machines));
            }
            if cfg.host_bridges {
                if config["restricted.devices.nic"].as_str() != Some("allow") {
                    changes.insert("restricted.devices.nic".into(), Some("allow".into()));
                }
                if config.get("restricted.networks.access").is_some() {
                    changes.insert("restricted.networks.access".into(), None);
                }
            }
            if !changes.is_empty() {
                ctx.effect(EffectOp::IncusPatchConfig {
                    object: project_ref(&cfg.project),
                    config: changes,
                })?;
            }
        }
        if observed(
            ctx,
            format!("/1.0/profiles/{PROFILE}?project={}", cfg.project),
        )?
        .is_none()
        {
            let default = ctx
                .observe(Observe::IncusGet {
                    path: "/1.0/profiles/default?project=default".into(),
                })
                .map_err(|_| {
                    PluginError::Failed("the default profile does not name a storage pool".into())
                })?;
            let pool = default["devices"]["root"]["pool"]
                .as_str()
                .filter(|pool| !pool.is_empty())
                .ok_or_else(|| {
                    PluginError::Failed("the default profile does not name a storage pool".into())
                })?;
            create(
                ctx,
                IncusKind::Profile,
                &cfg.project,
                PROFILE,
                json!({"name": PROFILE,
                    "description": format!("Arca service (project {}); managed by Arca", cfg.project),
                    "config": {"limits.memory":"2GiB", "limits.cpu":"2", "limits.cpu.priority":"5",
                        "limits.processes":"4096", "security.privileged":"false", "security.nesting":"false",
                        "security.idmap.isolated":"true", "security.guestapi":"false"},
                    "devices": {"root":{"type":"disk", "path":"/", "pool":pool, "size":"16GiB"}}}),
            )?;
        }
        Ok(())
    }
}
