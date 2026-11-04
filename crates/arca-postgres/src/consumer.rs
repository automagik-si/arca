//! A consumer writes only its own instance and references only its own URL files.
use super::*;
use std::collections::BTreeMap;
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConsumerLink {
    pub database: String,
    pub env: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConsumerConfig {
    pub name: String,
    pub image: String,
    pub pool: String,
    pub project: String,
    pub bridge: String,
    pub links: Vec<ConsumerLink>,
}
pub struct Consumer;
impl Plugin for Consumer {
    const ID: &'static str = "arca.consumer";
    type Config = ConsumerConfig;
    fn manifest() -> Manifest {
        manifest(Self::ID, vec![(IncusKind::Instance, EffectClass::B)])
    }
    fn apply(ctx: &mut Ctx<'_>, cfg: &ConsumerConfig) -> Result<(), PluginError> {
        service(&cfg.project, &cfg.bridge)?;
        if !name_ok(&cfg.name, "consumer", 63)
            || ctx.entry().0 != format!("consumer-{}", cfg.name)
            || !hex64(&cfg.image)
            || !pool_ok(&cfg.pool)
            || cfg.links.iter().any(|l| {
                !name_ok(&l.database, "postgres", 31)
                    || !(l.env == "DATABASE_URL"
                        || l.env
                            == format!(
                                "DATABASE_URL_{}",
                                l.database.to_uppercase().replace('-', "_")
                            ))
            })
        {
            return Err(PluginError::Config("invalid consumer config".into()));
        }
        let current = observed(
            ctx,
            format!("/1.0/instances/{}?project={}", cfg.name, cfg.project),
        )?;
        if current.is_none() {
            create(
                ctx,
                IncusKind::Instance,
                &cfg.project,
                &cfg.name,
                instance_body(&cfg.name, &cfg.image, &cfg.bridge, &cfg.pool),
            )?;
        }
        let mut changes = BTreeMap::new();
        for link in &cfg.links {
            changes.insert(
                format!("environment.{}", link.env),
                Some(format!(
                    "arca-secret:{}/{}-url",
                    ctx.entry().0,
                    link.database
                )),
            );
        }
        let prefix = format!("arca-secret:{}/", ctx.entry().0);
        if let Some(config) = current.as_ref().and_then(|v| v["config"].as_object()) {
            for (key, value) in config {
                if let Some(db) = value
                    .as_str()
                    .and_then(|v| v.strip_prefix(&prefix))
                    .and_then(|s| s.strip_suffix("-url"))
                    && key.starts_with("environment.")
                    && name_ok(db, "postgres", 31)
                    && !cfg.links.iter().any(|l| l.database == db)
                {
                    changes.insert(key.clone(), None);
                }
            }
        }
        if !changes.is_empty() {
            ctx.effect(EffectOp::IncusPatchConfig {
                object: object(IncusKind::Instance, &cfg.project, &cfg.name),
                config: changes,
            })?;
        }
        Ok(())
    }
}
