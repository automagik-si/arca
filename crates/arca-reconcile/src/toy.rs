//! The toy plugin: one ACL (class A) and one instance (class B) in a scratch project. It exists for the removal suite and is built
//! only with the `toy` feature.

use arca_kernel::desired::PluginId;
use arca_kernel::journal::EffectClass;
use arca_kernel::ownership::IncusKind;
use arca_kernel::plugin::{Ctx, EffectOp, Manifest, Plugin, PluginError};
use arca_providers::incus::NETWORK_HOME;
use serde::Deserialize;
use serde_json::json;

/// The toy plugin's id.
pub const TOY_ID: &str = "toy.scratch";
/// The scratch project the toy writes in. The project exists before the toy applies, and the test family names it.
pub const TOY_PROJECT: &str = "arcak-scratch-toy";

/// The toy's config: a tag that names its objects.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToyConfig {
    pub tag: String,
}

/// The toy plugin. It creates an ACL `toy-acl-<tag>`, which is addressed in the default project as every ACL is, and an instance
/// `toy-box-<tag>` in [`TOY_PROJECT`].
pub struct Toy;

impl Plugin for Toy {
    const ID: &'static str = TOY_ID;
    type Config = ToyConfig;

    fn manifest() -> Manifest {
        Manifest {
            id: PluginId(TOY_ID.to_string()),
            version: "0.1.0".to_string(),
            provides: Vec::new(),
            effects: vec![
                (IncusKind::Acl, EffectClass::A),
                (IncusKind::Instance, EffectClass::B),
            ],
            projects: vec![TOY_PROJECT.to_string(), NETWORK_HOME.to_string()],
            observes: false,
        }
    }

    fn apply(ctx: &mut Ctx<'_>, cfg: &ToyConfig) -> Result<(), PluginError> {
        ctx.effect(EffectOp::IncusCreate {
            kind: IncusKind::Acl,
            project: NETWORK_HOME.to_string(),
            parent: None,
            name: format!("toy-acl-{}", cfg.tag),
            body: json!({ "name": format!("toy-acl-{}", cfg.tag), "config": {} }),
        })?;
        // The removal suite deletes this fixture without a separate stop step.
        ctx.effect(EffectOp::IncusCreate {
            kind: IncusKind::Instance,
            project: TOY_PROJECT.to_string(),
            parent: None,
            name: format!("toy-box-{}", cfg.tag),
            body: json!({ "name": format!("toy-box-{}", cfg.tag), "start": false, "config": {} }),
        })?;
        Ok(())
    }
}
