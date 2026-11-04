//! Database instances and their own ingress ACL. Secret values never enter plugin configs.
pub mod consumer;
use arca_kernel::{
    journal::{AclField, EffectClass},
    ownership::{IncusKind, ObjectRef},
    plugin::{Ctx, EffectOp, Manifest, Observe, Plugin, PluginError},
};
use arca_netproject::{BRIDGE, PROJECT, create, manifest, name_ok, observed};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::net::Ipv4Addr;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PostgresConfig {
    pub name: String,
    pub image: String,
    pub pool: String,
    pub project: String,
    pub bridge: String,
    pub allow: Vec<String>,
}
pub struct Postgres;

pub fn fingerprint(image: &str) -> Result<&str, PluginError> {
    let (reference, digest) = image
        .rsplit_once("@sha256:")
        .filter(|(r, _)| !r.is_empty() && !r.contains('@') && !r.chars().any(char::is_whitespace))
        .ok_or_else(|| PluginError::Config("a digest-pinned image is required".into()))?;
    let _ = reference;
    if !hex64(digest) {
        return Err(PluginError::Config(
            "a digest-pinned image is required".into(),
        ));
    }
    Ok(digest)
}
pub fn hex64(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub fn object(kind: IncusKind, project: &str, name: &str) -> ObjectRef {
    ObjectRef {
        kind,
        project: project.into(),
        parent: None,
        name: name.into(),
        uuid: None,
    }
}
pub fn service(project: &str, bridge: &str) -> Result<(), PluginError> {
    if project != PROJECT || bridge != BRIDGE {
        return Err(PluginError::Config(
            "service project and bridge required".into(),
        ));
    }
    Ok(())
}
pub fn ipv4(state: &Value) -> Option<Ipv4Addr> {
    state["network"]
        .as_object()?
        .iter()
        .filter(|(name, _)| name.as_str() != "lo")
        .flat_map(|(_, nic)| nic["addresses"].as_array().into_iter().flatten())
        .find_map(|a| {
            (a["family"] == "inet" && a["scope"] == "global")
                .then(|| a["address"].as_str()?.parse::<Ipv4Addr>().ok())
                .flatten()
                .filter(|ip| !ip.is_loopback() && !ip.is_unspecified() && !ip.is_link_local())
        })
}
pub fn pool_ok(pool: &str) -> bool {
    (1..=63).contains(&pool.len())
        && pool.as_bytes()[0].is_ascii_alphanumeric()
        && pool
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}
pub fn instance_body(name: &str, image: &str, bridge: &str, pool: &str) -> Value {
    json!({"name":name,"type":"container","source":{"type":"image","fingerprint":image},"config":{},"devices":{"root":{"type":"disk","pool":pool,"path":"/"},"eth0":{"type":"nic","network":bridge,"name":"eth0"}}})
}
impl Plugin for Postgres {
    const ID: &'static str = "arca.postgres";
    type Config = PostgresConfig;
    fn manifest() -> Manifest {
        manifest(
            Self::ID,
            vec![
                (IncusKind::Acl, EffectClass::A),
                (IncusKind::Instance, EffectClass::B),
            ],
        )
    }
    fn apply(ctx: &mut Ctx<'_>, cfg: &PostgresConfig) -> Result<(), PluginError> {
        let image = fingerprint(&cfg.image)?;
        if !pool_ok(&cfg.pool) {
            return Err(PluginError::Config("invalid storage pool name".into()));
        }
        service(&cfg.project, &cfg.bridge)?;
        if !name_ok(&cfg.name, "postgres", 31)
            || ctx.entry().0 != format!("postgres-{}", cfg.name)
            || cfg.allow.iter().any(|c| !name_ok(c, "consumer", 63))
        {
            return Err(PluginError::Config(
                "invalid database or consumer name".into(),
            ));
        }
        let mut rules = Vec::new();
        for consumer in &cfg.allow {
            let state = ctx.observe(Observe::IncusGet {
                path: format!("/1.0/instances/{consumer}/state?project={}", cfg.project),
            })?;
            let ip = ipv4(&state).ok_or_else(|| {
                PluginError::Failed("a consumer has no IPv4 address; retry later".into())
            })?;
            rules.push(json!({"action":"allow","source":format!("{ip}/32"),"protocol":"tcp","destination_port":"5432","state":"enabled"}));
        }
        let acl = format!("postgres-{}", cfg.name);
        if observed(ctx, format!("/1.0/network-acls/{acl}?project=default"))?.is_none() {
            create(
                ctx,
                IncusKind::Acl,
                "default",
                &acl,
                json!({"name":acl,"config":{},"ingress":[],"egress":[]}),
            )?;
        }
        ctx.effect(EffectOp::IncusPatchAclField {
            object: object(IncusKind::Acl, "default", &acl),
            field: AclField::Ingress,
            rules,
        })?;
        if observed(
            ctx,
            format!("/1.0/instances/{}?project={}", cfg.name, cfg.project),
        )?
        .is_none()
        {
            let mut body = instance_body(&cfg.name, image, &cfg.bridge, &cfg.pool);
            body["config"]["environment.POSTGRES_PASSWORD"] =
                json!(format!("arca-secret:{acl}/password"));
            body["devices"]["eth0"]["security.acls"] = json!(acl);
            body["devices"]["eth0"]["security.acls.default.ingress.action"] = json!("reject");
            create(ctx, IncusKind::Instance, &cfg.project, &cfg.name, body)?;
        }
        Ok(())
    }
}
