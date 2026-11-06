//! Services: one template-derived container in the service project, with its own NIC ACL.
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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ServiceLink {
    pub to: String,
    pub port: u16,
    #[serde(default)]
    pub env: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Published {
    pub node: String,
    pub port: u16,
    pub reach: String,
}

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
    pub start: bool,
    /// An instance Arca adopted instead of creating; its project is the instance's own.
    #[serde(default)]
    pub adopted: bool,
    /// A record whose create has not settled, shown as `creating` after a restart.
    #[serde(default)]
    pub creating: bool,
}

impl ServiceConfig {
    pub fn acl(&self) -> String {
        acl_name(&self.project, &self.name)
    }
}

pub struct Service;

pub fn manifest_for() -> Manifest {
    manifest(
        Service::ID,
        vec![
            (IncusKind::Acl, EffectClass::A),
            (IncusKind::Instance, EffectClass::B),
        ],
    )
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
    let mut links: Vec<&ServiceLink> = cfg.links.iter().collect();
    links.sort_by(|a, b| a.to.cmp(&b.to).then(a.port.cmp(&b.port)));
    for link in links {
        rules.push(rule(json!({
            "action": "allow",
            "source": format!("{}/32", cfg.ipv4),
            "protocol": "tcp",
            "destination_port": link.port.to_string(),
            "description": format!("link from {} to port {}", cfg.ipv4, link.port),
            "state": "enabled"
        })));
    }
    rules
}

/// Match Incus' complete ACL rule shape, including its empty optional fields.
fn rule(fields: Value) -> Value {
    let mut out = json!({"action":"", "source":"", "destination":"", "protocol":"",
        "source_port":"", "destination_port":"", "icmp_type":"", "icmp_code":"",
        "description":"", "state":"enabled"});
    out.as_object_mut()
        .unwrap()
        .extend(fields.as_object().unwrap().clone());
    out
}

/// Reject private ranges outside this service /24 before allowing internet traffic.
pub fn egress(cfg: &ServiceConfig, hosts: &Value) -> Vec<Value> {
    let own = u32::from(cfg.gateway.parse::<Ipv4Addr>().expect("validated gateway")) & 0xffffff00;
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
    let mut host4 = BTreeSet::from([cfg.gateway.parse::<Ipv4Addr>().expect("validated gateway")]);
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
    vec![
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
    ]
}

fn instance_body(cfg: &ServiceConfig, links: &[String]) -> Value {
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
    let mut devices = Map::new();
    devices.insert(
        "root".to_string(),
        json!({ "type": "disk", "path": "/", "pool": cfg.pool }),
    );
    devices.insert(
        "eth0".to_string(),
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
    );
    json!({
        "name": cfg.name,
        "type": "container",
        "source": cfg.source,
        "profiles": [PROFILE],
        "config": config,
        "devices": devices,
        "start": cfg.start
    })
}

impl Plugin for Service {
    const ID: &'static str = "arca.service";
    type Config = ServiceConfig;

    fn manifest() -> Manifest {
        manifest_for()
    }

    fn apply(ctx: &mut Ctx<'_>, cfg: &ServiceConfig) -> Result<(), PluginError> {
        // An adopted instance is owned through its adopt entry. The service record never writes it.
        if cfg.adopted {
            return Ok(());
        }
        if cfg.project != PROJECT || cfg.bridge != BRIDGE {
            return Err(PluginError::Config(
                "the service project and bridge are required".into(),
            ));
        }
        if !service_name_ok(&cfg.name) || ctx.entry().0 != format!("service-{}", cfg.name) {
            return Err(PluginError::Config("invalid service name".into()));
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
        let rules = ingress(cfg);
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
                    if current[key].as_array() != Some(&desired) {
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
        let instance = observed(
            ctx,
            format!("/1.0/instances/{}?project={}", cfg.name, cfg.project),
        )?;
        if let Some(instance) = &instance
            && (instance["config"][TEMPLATE_KEY].as_str() != Some(cfg.template.as_str())
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
                &cfg.name,
                instance_body(
                    cfg,
                    &cfg.links
                        .iter()
                        .map(|link| {
                            let provider = ctx.observe(Observe::IncusGet {
                                path: format!("/1.0/instances/{}?project={}", link.to, cfg.project),
                            })?;
                            let uuid = provider["config"]["volatile.uuid"]
                                .as_str()
                                .filter(|uuid| !uuid.is_empty())
                                .ok_or_else(|| {
                                    PluginError::Failed(
                                        "linked provider has no instance UUID".into(),
                                    )
                                })?;
                            Ok(format!("{}:{}:{uuid}", link.to, link.port))
                        })
                        .collect::<Result<Vec<_>, PluginError>>()?,
                ),
            )?;
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
    if template.kind != "oci" && template.kind != "system" {
        return Err(TemplateError::refused(format!(
            "template {name}: kind is oci or system"
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
    name: &str,
    ipv4: &str,
    gateway: &str,
    secrets_dir: &Path,
) -> Result<(BTreeMap<String, String>, bool), TemplateError> {
    let mut start = true;
    let mut references = BTreeMap::new();
    for key in &template.secrets {
        let file = secret_file(secrets_dir, name, key);
        if let Some(reference) = file
            .as_ref()
            .filter(|path| path.is_file())
            .map(|_| secret_reference(name, key))
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
            references.insert(key.clone(), secret_reference(name, key));
        } else {
            start = false;
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

fn secret_reference(name: &str, key: &str) -> String {
    format!("{SECRET_PREFIX}service-{name}/{}", secret_name(key))
}

/// A secret's file name: lowercase, with `_` mapped to `-`, so it is inside the provider's secret-name alphabet.
fn secret_name(key: &str) -> String {
    key.to_ascii_lowercase().replace('_', "-")
}

pub fn secret_file(secrets_dir: &Path, name: &str, key: &str) -> Option<PathBuf> {
    if !env_name_ok(key) {
        return None;
    }
    Some(
        secrets_dir
            .join(format!("service-{name}"))
            .join(secret_name(key)),
    )
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

fn write_secret(path: &Path, value: &str) -> Result<(), TemplateError> {
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
fn render(
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
                let script = std::fs::read_to_string(dir.join(file))
                    .map_err(|_| TemplateError::refused("entrypoint: file cannot be read"))?;
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
    json!({
        "name": cfg.name,
        "template": cfg.template,
        "state": state,
        "ipv4": ipv4,
        "ports": cfg.ports,
        "links": links,
        "published": published
    })
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
        "published": null
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
        "published": null
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
            (r"\btskey-[A-Za-z0-9-]+", "[secret]"),
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
