//! The shared service project and its bridge, owned by one desired-state entry.
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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NetProjectConfig {
    pub project: String,
    pub bridge: String,
    pub cidr: String,
    pub virtual_machines: u32,
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

impl Plugin for NetProject {
    const ID: &'static str = "arca.netproject";
    type Config = NetProjectConfig;
    fn manifest() -> Manifest {
        manifest(
            Self::ID,
            vec![
                (IncusKind::Project, EffectClass::B),
                (IncusKind::Network, EffectClass::B),
            ],
        )
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
        if observed(
            ctx,
            format!("/1.0/projects/{}?project={}", cfg.project, cfg.project),
        )?
        .is_none()
        {
            create(
                ctx,
                IncusKind::Project,
                &cfg.project,
                &cfg.project,
                json!({"name":cfg.project,"config":{"limits.virtual-machines":cfg.virtual_machines.to_string(),"restricted.networks.access":cfg.bridge}}),
            )?;
        }
        Ok(())
    }
}
