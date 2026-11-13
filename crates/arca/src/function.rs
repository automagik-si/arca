//! Functions: what one is, and what its owner is shown.
//!
//! A function is an OCI image by digest that holds one program, and a rule for when to run it. Arca keeps it as a
//! service record with one more key, `function` ([`Function`]): a container of the node's own service project, on the
//! managed network, created stopped and never started by anyone but the kernel. One run is one start of that container
//! (`crate::function_run`). Everything else is the service machinery as it is: the image by digest, the NIC with its
//! own rules, the secrets by name, the links, the private destinations the node's config names, and the owner's card.
//!
//! What is a function's alone:
//! - Its definition comes from the arguments of `function_create`, which the owner approves on a card, and not from a
//!   template file. [`template`] gives the template those arguments stand for, so the code that sets a secret reads a
//!   function as it reads a service.
//! - It reaches only what its definition names: its NIC rules have no rule that lets everything else out
//!   (`arca_netproject::tenant::function_egress_rules`).
//! - A link gives it a way to a service and nothing else: no connection value is put in its environment. What it
//!   connects with is a secret its definition names.
//!
//! `function_create` and `function_remove` run only from a proposal the owner approved. A run is not a proposal.

use std::collections::{BTreeMap, BTreeSet};

use arca_netproject::destination::Destination;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::service::{Limits, ServiceConfig, Template};

/// The name a function's record and instance carry where a service's carry its template's.
pub const TEMPLATE: &str = "function";
/// The shortest and the longest timer, in seconds.
pub const EVERY_MIN: u64 = 10;
pub const EVERY_MAX: u64 = 86_400;
/// The time one run may take when the definition names none, and the most it may name, in seconds.
pub const SECONDS_DEFAULT: u64 = 60;
pub const SECONDS_MAX: u64 = 900;
/// The memory one run may use when the definition names none, and the least it may name, in bytes.
pub const MEMORY_DEFAULT: &str = "128MiB";
pub const MEMORY_MIN: u128 = 8 * 1024 * 1024;
/// The CPUs one run may use when the definition names none, and the most it may name.
pub const CPU_DEFAULT: u64 = 1;
pub const CPU_MAX: u64 = 64;
/// How many secrets, plain environment values, links and command parts a definition may name.
pub const SECRETS_MAX: usize = 16;
pub const ENV_MAX: usize = 32;
pub const LINKS_MAX: usize = 8;
pub const COMMAND_MAX: usize = 32;
/// The most characters one card row shows of a command part or of an environment value: a definition whose value
/// would need more is refused when it is proposed, so a card never cuts what the owner approves.
pub const ROW_MAX: usize = crate::action_proposal::SETTING_ROW_MAX;

/// What only a function's record carries.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Function {
    /// The image, as `<registry>/<repository>`, and its digest: the program the owner approved.
    pub image: String,
    pub digest: String,
    /// The program and its arguments, when the definition names them. Without them the image's own entry point runs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// The plain environment values the definition gives, by name. Never a secret: the card shows each.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// The names of its secrets. A run receives these and no other; `function_secret_set` sets each value.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<String>,
    /// The timer: a run starts every this many seconds, unless one is still running.
    pub every_seconds: u64,
    /// The longest one run may take, in seconds. The kernel stops a run that passes it.
    pub seconds: u64,
    /// The address and port of each service it is linked to, as they were when it was created: its NIC rules open
    /// these and nothing else of the service network.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reach: Vec<Destination>,
    /// The fingerprint Incus gave the image when the function was created: the copy in the node's own store that each
    /// run's root is rebuilt from. It is read once, from the image record whose registry and digest are the approved
    /// ones, and a run uses no other image of the store, whatever the instance itself says it was made from. Absent
    /// when it could not be read: each run then asks the registry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
}

/// One link of a definition: a service of this node, and the tcp port of it the function may reach.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub to: String,
    pub port: u16,
}

/// A definition as `function_create` takes it, after [`prepare`].
#[derive(Clone, Debug)]
pub struct Definition {
    pub name: String,
    pub function: Function,
    pub limits: Limits,
    pub links: Vec<Link>,
}

fn field(path: &str, why: &str) -> String {
    format!("args/{path}: {why}")
}

/// The arguments of a `function_create` as the owner approves them: every rule a definition must keep is checked, and
/// what the caller left out is written in (the kind, one run at a time, each limit), so the card shows it and the
/// approval binds it. The error names the argument and the rule.
pub fn prepare(args: &Value) -> Result<Value, String> {
    let mut args = args.clone();
    let object = args
        .as_object_mut()
        .ok_or_else(|| "args: expected an object".to_string())?;
    object
        .entry("kind")
        .or_insert_with(|| Value::String("container".to_string()));
    object.entry("concurrency").or_insert_with(|| json!(1));
    let limits = object
        .entry("limits")
        .or_insert_with(|| Value::Object(Map::new()));
    let limits = limits
        .as_object_mut()
        .ok_or_else(|| field("limits", "expected an object"))?;
    limits
        .entry("seconds")
        .or_insert_with(|| json!(SECONDS_DEFAULT));
    limits
        .entry("memory")
        .or_insert_with(|| json!(MEMORY_DEFAULT));
    limits.entry("cpu").or_insert_with(|| json!(CPU_DEFAULT));
    definition(&args)?;
    Ok(args)
}

/// The definition `args` name, or the argument that breaks a rule and the rule.
pub fn definition(args: &Value) -> Result<Definition, String> {
    let name = args["name"]
        .as_str()
        .filter(|name| crate::service::service_name_ok(name))
        .ok_or_else(|| field("name", "expected a function name"))?
        .to_string();
    if args["kind"].as_str() != Some("container") {
        return Err(field(
            "kind",
            "this build runs a function in a container: the only kind is \"container\"",
        ));
    }
    if args["concurrency"].as_u64() != Some(1) {
        return Err(field(
            "concurrency",
            "this build runs one run of a function at a time: the only value is 1",
        ));
    }
    let image = args["image"].as_str().unwrap_or_default().to_string();
    let digest = args["digest"].as_str().unwrap_or_default().to_string();
    if !digest.starts_with("sha256:")
        || !crate::service::image_by_digest(&format!("{image}@{digest}"))
    {
        return Err(field(
            "image",
            "expected image as <registry>/<repository> in lower case and digest as sha256:<64 hex digits>: a function is an image by digest, never a tag",
        ));
    }
    let mut command = Vec::new();
    for (index, part) in args["command"].as_array().into_iter().flatten().enumerate() {
        let text = part
            .as_str()
            .ok_or_else(|| field("command", "expected a list of strings"))?;
        if text.is_empty()
            || text.chars().count() > ROW_MAX
            || !crate::service::plain_text(text)
            || text.contains("{{")
        {
            return Err(field(
                "command",
                &format!(
                    "each part is 1 to {ROW_MAX} plain characters, without control characters and without \"{{{{\""
                ),
            ));
        }
        if index == 0 && !text.starts_with('/') {
            return Err(field(
                "command",
                "the first part is the program's absolute path inside the image, such as /relay: no shell looks one up",
            ));
        }
        command.push(text.to_string());
    }
    if command.len() > COMMAND_MAX {
        return Err(field("command", &format!("at most {COMMAND_MAX} parts")));
    }
    let mut secrets = Vec::new();
    for secret in args["secrets"].as_array().into_iter().flatten() {
        let key = secret
            .as_str()
            .filter(|key| secret_name_ok(key))
            .ok_or_else(|| {
                field(
                    "secrets",
                    "each is the name of an environment variable: a letter or an underscore, then letters, digits and underscores, at most 64",
                )
            })?;
        if secrets.iter().any(|held| held == key) {
            return Err(field("secrets", &format!("{key} is named twice")));
        }
        secrets.push(key.to_string());
    }
    if secrets.len() > SECRETS_MAX {
        return Err(field("secrets", &format!("at most {SECRETS_MAX} secrets")));
    }
    let mut env = BTreeMap::new();
    for (key, value) in args["env"].as_object().into_iter().flatten() {
        let text = value
            .as_str()
            .ok_or_else(|| field("env", "each value is a string"))?;
        if !secret_name_ok(key) {
            return Err(field(
                "env",
                "each name is the name of an environment variable: a letter or an underscore, then letters, digits and underscores, at most 64",
            ));
        }
        if secrets.iter().any(|secret| secret == key) {
            return Err(field(
                "env",
                &format!(
                    "{key} is one of the secrets: a name is a plain value or a secret, not both"
                ),
            ));
        }
        if text.chars().count() > ROW_MAX
            || !crate::service::plain_text(text)
            || text.contains("{{")
        {
            return Err(field(
                "env",
                &format!(
                    "the value of {key} is at most {ROW_MAX} plain characters, without control characters and without \"{{{{\": the owner's card shows it whole"
                ),
            ));
        }
        env.insert(key.clone(), text.to_string());
    }
    if env.len() > ENV_MAX {
        return Err(field("env", &format!("at most {ENV_MAX} values")));
    }
    let mut links: Vec<Link> = Vec::new();
    for link in args["links"].as_array().into_iter().flatten() {
        let to = link["to"]
            .as_str()
            .filter(|to| crate::service::service_name_ok(to))
            .ok_or_else(|| field("links", "each names a service of this node in to"))?;
        let port = link["port"]
            .as_u64()
            .and_then(|port| u16::try_from(port).ok())
            .filter(|port| *port != 0)
            .ok_or_else(|| field("links", "each names a tcp port from 1 to 65535 in port"))?;
        if to == name {
            return Err(field("links", "a function cannot link to itself"));
        }
        if links.iter().any(|held| held.to == to) {
            return Err(field(
                "links",
                &format!("{to} is named twice: one link to a service, on one port"),
            ));
        }
        links.push(Link {
            to: to.to_string(),
            port,
        });
    }
    if links.len() > LINKS_MAX {
        return Err(field("links", &format!("at most {LINKS_MAX} links")));
    }
    let triggers = args["triggers"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let every_seconds = match triggers {
        [one] if one.as_object().is_some_and(|map| map.len() == 1) => one["timer"]["everySeconds"]
            .as_u64()
            .filter(|every| (EVERY_MIN..=EVERY_MAX).contains(every)),
        _ => None,
    }
    .ok_or_else(|| {
        field(
            "triggers",
            &format!(
                "this build runs a function on a timer: exactly one trigger, {{\"timer\": {{\"everySeconds\": n}}}}, with n from {EVERY_MIN} to {EVERY_MAX}"
            ),
        )
    })?;
    let seconds = args["limits"]["seconds"]
        .as_u64()
        .filter(|seconds| (1..=SECONDS_MAX).contains(seconds))
        .ok_or_else(|| {
            field(
                "limits/seconds",
                &format!("the longest one run may take, from 1 to {SECONDS_MAX} seconds"),
            )
        })?;
    let memory = args["limits"]["memory"]
        .as_str()
        .filter(|memory| {
            crate::service::size_ok(memory)
                && crate::service::size_bytes(memory).is_some_and(|bytes| bytes >= MEMORY_MIN)
        })
        .ok_or_else(|| {
            field(
                "limits/memory",
                "the most memory one run may use, as a size such as 128MiB, at least 8MiB",
            )
        })?;
    let cpu = args["limits"]["cpu"]
        .as_u64()
        .filter(|cpu| (1..=CPU_MAX).contains(cpu))
        .ok_or_else(|| {
            field(
                "limits/cpu",
                &format!("the CPUs one run may use, from 1 to {CPU_MAX}"),
            )
        })?;
    Ok(Definition {
        name,
        function: Function {
            image,
            digest,
            command,
            env,
            secrets,
            every_seconds,
            seconds,
            reach: Vec::new(),
            fingerprint: None,
        },
        limits: Limits {
            cpu: Some(cpu.to_string()),
            memory: Some(memory.to_string()),
        },
        links,
    })
}

/// Whether `name` may name a secret or a plain value of a function: an environment variable's name.
pub fn secret_name_ok(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// The template a function's record stands for: its image by digest, its plain values, and one environment value for
/// each secret, which is that secret. `None` for a record that is not a function's.
pub fn template(cfg: &ServiceConfig) -> Option<Template> {
    let function = cfg.function.as_ref()?;
    let mut env: BTreeMap<String, String> = function.env.clone();
    for secret in &function.secrets {
        env.insert(secret.clone(), format!("{{{{secret:{secret}}}}}"));
    }
    serde_json::from_value(json!({
        "name": TEMPLATE,
        "kind": "oci",
        "image": function.image,
        "digest": function.digest,
        "env": env,
        "secrets": function.secrets,
    }))
    .ok()
}

/// A function's command as Incus takes an entry point: its parts, each quoted, with a space between. `None` for a
/// definition that names no command, whose image's own entry point runs.
pub fn entrypoint(command: &[String]) -> Option<String> {
    (!command.is_empty()).then(|| {
        command
            .iter()
            .map(|part| crate::service::shell_quote(part))
            .collect::<Vec<_>>()
            .join(" ")
    })
}

/// The image source a function's definition names: the registry and the digest its owner approved, and nothing else.
/// A function's record must carry exactly this as its source, and a run's root is rebuilt from no other.
pub fn pinned_source(cfg: &ServiceConfig) -> Option<Value> {
    crate::service::source(&template(cfg)?).ok()
}

/// The fingerprint of the image in `images` (a listing of a project's image store) that Incus fetched from the
/// registry and digest `source` names. It is the one record that Incus itself says it fetched over the OCI protocol,
/// from that registry's host, under that digest; a credential in the address Incus kept is no part of the host.
/// `base_image` is what the new instance says it was made from, read right after its create: the record must be
/// that very image. `None` when no record, or more than one, is the approved image, or when the instance was made
/// from another: a run then asks the registry, by the approved digest, every time.
pub fn fetched_fingerprint(
    images: &Value,
    source: &Value,
    base_image: Option<&str>,
) -> Option<String> {
    let wanted = registry_host(source["server"].as_str()?)?;
    let alias = source["alias"].as_str()?;
    let mut found = images.as_array()?.iter().filter(|image| {
        let from = &image["update_source"];
        from["protocol"].as_str() == Some("oci")
            && from["alias"].as_str() == Some(alias)
            && from["server"].as_str().and_then(registry_host).as_deref() == Some(wanted.as_str())
    });
    let one = found.next()?;
    if found.next().is_some() {
        return None;
    }
    one["fingerprint"]
        .as_str()
        .filter(|text| fingerprint_ok(text) && base_image == Some(*text))
        .map(str::to_string)
}

/// The host, with its port when it names one, of a registry address as Incus keeps it: the scheme, then the
/// authority (an optional credential, an `@`, the host and an optional port), then an optional path. The credential
/// is the part of the authority before its last `@`; an `@` in the path, the query or the fragment is not one, and
/// never moves the host. `None` for an
/// address that is not https, or whose host is empty or holds anything but letters, digits, `.`, `-` and `:`.
pub fn registry_host(server: &str) -> Option<String> {
    let rest = server.strip_prefix("https://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?;
    (!host.is_empty()
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b':')))
    .then(|| host.to_ascii_lowercase())
}

/// Whether `text` is an image fingerprint as Incus writes one: 64 hex digits in lower case.
pub fn fingerprint_ok(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// The secrets of a function that are not set yet, in the definition's order: a run starts only when there is none.
pub fn missing_secrets(cfg: &ServiceConfig) -> Vec<String> {
    cfg.function
        .iter()
        .flat_map(|function| function.secrets.iter())
        .filter(|secret| !secret_set(cfg, secret))
        .cloned()
        .collect()
}

/// Whether the secret `key` of a function is set: `function_secret_set` stored a value for it, and its environment
/// value is the reference to that value. An environment value that got there any other way does not count.
pub fn secret_set(cfg: &ServiceConfig, key: &str) -> bool {
    cfg.secret_refs.contains_key(key)
        && cfg
            .environment
            .get(key)
            .is_some_and(|value| value.starts_with(arca_providers::incus::SECRET_PREFIX))
}

/// Whether a function's environment is exactly its own: each plain value its definition gives, as the definition
/// gives it, and for each secret that is set the reference to its stored value. No other name, whatever wrote it: a
/// run is given what its definition names and nothing else.
fn environment_ok(cfg: &ServiceConfig, function: &Function) -> bool {
    let secrets: BTreeSet<&str> = function.secrets.iter().map(String::as_str).collect();
    cfg.secret_refs
        .keys()
        .all(|key| secrets.contains(key.as_str()))
        && function
            .env
            .iter()
            .all(|(key, value)| cfg.environment.get(key) == Some(value))
        && cfg.environment.iter().all(|(key, value)| {
            function.env.contains_key(key)
                || (secrets.contains(key.as_str())
                    && cfg.secret_refs.contains_key(key)
                    && value.starts_with(arca_providers::incus::SECRET_PREFIX))
        })
}

/// Whether a function's record is one the plugin applies: a container of the node's own project that was created
/// stopped, opens no port and has nothing a function never has, whose links are each opened by exactly one address
/// and port, whose environment holds only what its definition names, and whose definition keeps every rule
/// [`definition`] checks.
pub fn record_ok(cfg: &ServiceConfig) -> bool {
    let Some(function) = &cfg.function else {
        return true;
    };
    let linked: BTreeSet<u16> = cfg.links.iter().map(|link| link.port).collect();
    cfg.template == TEMPLATE
        && cfg.vm.is_none()
        && cfg.tenant.is_none()
        && !cfg.adopted
        && !cfg.start
        && !cfg.autostart
        && cfg.ports.is_empty()
        && cfg.incoming.is_empty()
        && cfg.published.is_none()
        && cfg.droidd_port.is_none()
        && cfg.data.is_none()
        && cfg.lan.is_none()
        && cfg.private_port.is_none()
        && cfg.kernel_access.is_none()
        && cfg.links.len() <= LINKS_MAX
        && cfg.links.iter().all(|link| link.env.is_empty())
        && environment_ok(cfg, function)
        && pinned_source(cfg).as_ref() == Some(&cfg.source)
        && function.fingerprint.as_deref().is_none_or(fingerprint_ok)
        && function.reach.len() == cfg.links.len()
        && function.reach.iter().all(|one| linked.contains(&one.port))
        && (EVERY_MIN..=EVERY_MAX).contains(&function.every_seconds)
        && (1..=SECONDS_MAX).contains(&function.seconds)
        && function.secrets.len() <= SECRETS_MAX
        && function.secrets.iter().all(|secret| secret_name_ok(secret))
        && function
            .env
            .keys()
            .all(|key| secret_name_ok(key) && !function.secrets.contains(key))
        && cfg.limits.as_ref().is_some_and(|limits| {
            limits.cpu.is_some()
                && limits
                    .memory
                    .as_deref()
                    .and_then(crate::service::size_bytes)
                    .is_some_and(|bytes| bytes >= MEMORY_MIN)
        })
}

/// The timer of a definition, in the owner's words.
pub fn timer_words(every_seconds: u64) -> String {
    let every = match every_seconds {
        1 => "every second".to_string(),
        seconds if seconds % 3600 == 0 && seconds >= 3600 => plural(seconds / 3600, "hour"),
        seconds if seconds % 60 == 0 => plural(seconds / 60, "minute"),
        seconds => plural(seconds, "second"),
    };
    format!(
        "{every}, one run at a time: a run that is still going when the next is due is not started again"
    )
}

fn plural(count: u64, unit: &str) -> String {
    if count == 1 {
        format!("every {unit}")
    } else {
        format!("every {count} {unit}s")
    }
}

/// The limits of a definition, in the owner's words.
pub fn limits_words(seconds: u64, memory: &str, cpu: u64) -> String {
    format!(
        "each run: at most {seconds} second{}, {memory} of memory and {cpu} CPU{}; Arca stops a run that passes the time, and one that passes the memory is killed",
        if seconds == 1 { "" } else { "s" },
        if cpu == 1 { "" } else { "s" },
    )
}

/// The `Internet` row of a function's card. It says the one way out that is not closed: names are resolved by this
/// node's DNS, which asks the internet's, so a program can pass a little data out in the names it asks for.
pub const INTERNET_WORDS: &str = "No: it can open no connection to a public address. One way out stays: names are looked up through this node's DNS, which asks the internet's name servers, so a program can send a little data out in the names it asks for";

/// The card rows of a `function_create`, as labels and values: what the function is, when it runs, what one run may
/// use, which secrets it is given by name, and everything it may reach. `args` are a proposal's own, after
/// [`prepare`]; a field that does not read is shown as such, so a card never shows nothing for it.
pub fn create_rows(args: &Value) -> Vec<(String, String)> {
    let text = |value: &Value| value.as_str().unwrap_or("(cannot be read)").to_string();
    let mut rows = vec![
        ("Function".to_string(), text(&args["name"])),
        (
            "Program".to_string(),
            format!("{}@{}", text(&args["image"]), text(&args["digest"])),
        ),
    ];
    rows.push((
        "Kind".to_string(),
        match args["kind"].as_str() {
            Some("container") => {
                "container: each run is one start of the function's own container, which is stopped again when the run ends".to_string()
            }
            _ => "(cannot be read)".to_string(),
        },
    ));
    match args["command"].as_array().filter(|parts| !parts.is_empty()) {
        Some(parts) => {
            for (index, part) in parts.iter().enumerate() {
                let label = if index == 0 {
                    "Command".to_string()
                } else {
                    format!("Command argument {index}")
                };
                rows.push((label, text(part)));
            }
        }
        None => rows.push((
            "Command".to_string(),
            "the image's own entry point".to_string(),
        )),
    }
    let every = args["triggers"][0]["timer"]["everySeconds"].as_u64();
    rows.push((
        "Runs".to_string(),
        every.map_or_else(|| "(cannot be read)".to_string(), timer_words),
    ));
    rows.push((
        "Limits".to_string(),
        limits_words(
            args["limits"]["seconds"].as_u64().unwrap_or(0),
            args["limits"]["memory"]
                .as_str()
                .unwrap_or("(cannot be read)"),
            args["limits"]["cpu"].as_u64().unwrap_or(0),
        ),
    ));
    let secrets: Vec<String> = args["secrets"]
        .as_array()
        .into_iter()
        .flatten()
        .map(&text)
        .collect();
    rows.push((
        "Secrets".to_string(),
        if secrets.is_empty() {
            "None: no run is given a secret".to_string()
        } else {
            format!(
                "{}: each run is given these by name and no other. Their values are set separately (function_secret_set) and are never shown",
                secrets.join(", ")
            )
        },
    ));
    for (key, value) in args["env"].as_object().into_iter().flatten() {
        rows.push((format!("Setting {key}"), text(value)));
    }
    let links: Vec<String> = args["links"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|link| {
            format!(
                "{} on tcp port {}",
                text(&link["to"]),
                link["port"]
                    .as_u64()
                    .map_or_else(|| "(cannot be read)".to_string(), |port| port.to_string())
            )
        })
        .collect();
    rows.push((
        "Services on this node".to_string(),
        if links.is_empty() {
            "None: it reaches no service of this node".to_string()
        } else {
            format!("It can reach {}, and no other service", and_list(&links))
        },
    ));
    if args.get("destinations").is_some() {
        rows.push((
            "Private network".to_string(),
            format!(
                "Yes: this function can reach {} through this node, and is seen there as this node itself: whatever trusts this node's address there trusts this function. Every other private address stays closed.",
                crate::action_proposal::destinations_list(&args["destinations"])
            ),
        ));
    } else {
        rows.push((
            "Private network".to_string(),
            "No: every private address outside this node's services is closed to it".to_string(),
        ));
    }
    rows.push(("Internet".to_string(), INTERNET_WORDS.to_string()));
    if let Some(server) = args.get("registry") {
        rows.push((
            "Registry".to_string(),
            crate::registry::card_words(server.as_str().unwrap_or("(cannot be read)")),
        ));
    }
    rows
}

fn and_list(parts: &[String]) -> String {
    match parts {
        [] => String::new(),
        [one] => one.clone(),
        [first @ .., last] => format!("{} and {last}", first.join(", ")),
    }
}

/// The sentence of a `function_create` card.
pub fn create_summary(args: &Value) -> String {
    let every = args["triggers"][0]["timer"]["everySeconds"]
        .as_u64()
        .unwrap_or(0);
    format!(
        "Create the function {} from {} and run it {}",
        args["name"].as_str().unwrap_or_default(),
        args["image"].as_str().unwrap_or_default(),
        match every {
            seconds if seconds % 3600 == 0 && seconds >= 3600 => plural(seconds / 3600, "hour"),
            seconds if seconds % 60 == 0 && seconds >= 60 => plural(seconds / 60, "minute"),
            seconds => plural(seconds, "second"),
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn relay() -> Value {
        json!({
            "name": "relay",
            "image": "registry.example/acme/relay",
            "digest": DIGEST,
            "command": ["/relay", "--once"],
            "env": { "BATCH": "500" },
            "secrets": ["DATABASE_URL", "NATS_URL"],
            "links": [{ "to": "pg1", "port": 5432 }, { "to": "nats", "port": 4222 }],
            "triggers": [{ "timer": { "everySeconds": 30 } }],
        })
    }

    #[test]
    fn the_approved_image_is_the_one_record_incus_fetched_from_that_host_under_that_digest() {
        let digest = format!("acme/relay@sha256:{}", "a".repeat(64));
        let source = json!({ "type": "image", "mode": "pull", "protocol": "oci",
            "server": "https://registry.example", "alias": digest });
        let (good, other) = ("c".repeat(64), "d".repeat(64));
        let image = |fingerprint: &str, server: &str, protocol: &str| {
            json!({ "fingerprint": fingerprint,
                "update_source": { "server": server, "alias": digest, "protocol": protocol } })
        };
        let pick = |images: Value, base: Option<&str>| fetched_fingerprint(&images, &source, base);
        // An address with a credential before the host, put together here from its parts.
        let with = |user: &str, word: &str, rest: &str| {
            let mut address = String::from("https://");
            address.push_str(user);
            address.push(':');
            address.push_str(word);
            address.push('@');
            address.push_str(rest);
            address
        };
        // The record Incus fetched from the host, with or without a credential in the address it kept.
        for server in [
            "https://registry.example".to_string(),
            with("user", "word", "registry.example"),
            with("us%40er", "w%2Frd", "registry.example/"),
            "https://REGISTRY.example".to_string(),
        ] {
            let server = server.as_str();
            assert_eq!(
                pick(json!([image(&good, server, "oci")]), Some(&good)),
                Some(good.clone()),
                "{server}"
            );
        }
        // An address that only holds the host's name somewhere after an `@` is another host's.
        for server in [
            "https://evil.example/@registry.example",
            "https://evil.example/x?y=@registry.example",
            "https://evil.example#@registry.example",
            "https://registry.example.evil.example",
            "https://registry.example@evil.example",
            "http://registry.example",
            "https://",
        ] {
            assert_eq!(
                pick(json!([image(&good, server, "oci")]), Some(&good)),
                None,
                "{server}"
            );
        }
        assert_eq!(
            registry_host("https://evil.example/@registry.example").as_deref(),
            Some("evil.example")
        );
        assert_eq!(
            registry_host(&with("u", "p", "registry.example:5000/v2")).as_deref(),
            Some("registry.example:5000")
        );
        // Another protocol's record, another digest's, and two records at once are none of them it.
        assert_eq!(
            pick(
                json!([image(&good, "https://registry.example", "simplestreams")]),
                Some(&good)
            ),
            None
        );
        let mut elsewhere = image(&good, "https://registry.example", "oci");
        elsewhere["update_source"]["alias"] =
            json!(format!("acme/relay@sha256:{}", "b".repeat(64)));
        assert_eq!(pick(json!([elsewhere]), Some(&good)), None);
        assert_eq!(
            pick(
                json!([
                    image(&good, "https://registry.example", "oci"),
                    image(&other, &with("a", "b", "registry.example"), "oci")
                ]),
                Some(&good)
            ),
            None
        );
        // And the instance must have been made from that very image, as it says right after its create.
        assert_eq!(
            pick(
                json!([image(&good, "https://registry.example", "oci")]),
                Some(&other)
            ),
            None
        );
        assert_eq!(
            pick(
                json!([image(&good, "https://registry.example", "oci")]),
                None
            ),
            None
        );
    }

    #[test]
    fn prepare_writes_in_what_the_caller_left_out() {
        let prepared = prepare(&relay()).expect("a definition");
        assert_eq!(prepared["kind"], "container");
        assert_eq!(prepared["concurrency"], 1);
        assert_eq!(
            prepared["limits"],
            json!({ "seconds": 60, "memory": "128MiB", "cpu": 1 })
        );
        // What the caller named stays.
        let mut named = relay();
        named["limits"] = json!({ "seconds": 20 });
        let prepared = prepare(&named).expect("a definition");
        assert_eq!(
            prepared["limits"],
            json!({ "seconds": 20, "memory": "128MiB", "cpu": 1 })
        );
        let definition = definition(&prepared).expect("it reads");
        assert_eq!(definition.function.seconds, 20);
        assert_eq!(definition.function.every_seconds, 30);
        assert_eq!(definition.limits.memory.as_deref(), Some("128MiB"));
        assert_eq!(definition.links.len(), 2);
    }

    #[test]
    fn every_rule_of_a_definition_is_refused_with_its_argument() {
        let cases: Vec<(&str, Value, &str)> = vec![
            ("name", json!("Relay"), "args/name"),
            ("kind", json!("machine"), "args/kind"),
            ("concurrency", json!(2), "args/concurrency"),
            ("image", json!("relay"), "args/image"),
            ("image", json!("Registry.example/acme/relay"), "args/image"),
            ("digest", json!("latest"), "args/image"),
            ("digest", json!(DIGEST.replace("sha256:", "")), "args/image"),
            ("command", json!(["relay"]), "args/command"),
            ("command", json!(["/relay", ""]), "args/command"),
            ("command", json!(["/relay", "a\nb"]), "args/command"),
            ("command", json!(["/relay", "{{secret:X}}"]), "args/command"),
            ("secrets", json!(["1BAD"]), "args/secrets"),
            ("secrets", json!(["A", "A"]), "args/secrets"),
            ("env", json!({ "DATABASE_URL": "x" }), "args/env"),
            ("env", json!({ "A": "{{secret:DATABASE_URL}}" }), "args/env"),
            ("env", json!({ "A": "x".repeat(ROW_MAX + 1) }), "args/env"),
            ("env", json!({ "A": 1 }), "args/env"),
            ("links", json!([{ "to": "pg1", "port": 0 }]), "args/links"),
            ("links", json!([{ "to": "relay", "port": 1 }]), "args/links"),
            (
                "links",
                json!([{ "to": "pg1", "port": 1 }, { "to": "pg1", "port": 2 }]),
                "args/links",
            ),
            ("triggers", json!([]), "args/triggers"),
            (
                "triggers",
                json!([{ "timer": { "everySeconds": 9 } }]),
                "args/triggers",
            ),
            (
                "triggers",
                json!([{ "timer": { "everySeconds": 30 } }, { "timer": { "everySeconds": 60 } }]),
                "args/triggers",
            ),
            ("triggers", json!([{ "postgres": {} }]), "args/triggers"),
            ("limits", json!({ "seconds": 0 }), "args/limits/seconds"),
            ("limits", json!({ "seconds": 901 }), "args/limits/seconds"),
            ("limits", json!({ "memory": "1MiB" }), "args/limits/memory"),
            ("limits", json!({ "memory": "lots" }), "args/limits/memory"),
            ("limits", json!({ "cpu": 0 }), "args/limits/cpu"),
        ];
        for (key, value, place) in cases {
            let mut args = relay();
            args[key] = value.clone();
            let refused = prepare(&args).expect_err("a broken rule");
            assert!(refused.starts_with(place), "{key} = {value}: {refused}");
        }
    }

    #[test]
    fn the_card_names_everything_the_function_may_reach() {
        let mut args = prepare(&relay()).expect("a definition");
        let rows = create_rows(&args);
        let row = |label: &str| {
            rows.iter()
                .find(|(found, _)| found == label)
                .map(|(_, value)| value.clone())
                .unwrap_or_else(|| panic!("no row {label}: {rows:?}"))
        };
        assert_eq!(row("Function"), "relay");
        assert_eq!(
            row("Program"),
            format!("registry.example/acme/relay@{DIGEST}")
        );
        assert_eq!(row("Command"), "/relay");
        assert_eq!(row("Command argument 1"), "--once");
        assert!(row("Runs").starts_with("every 30 seconds, one run at a time"));
        assert!(row("Limits").contains("at most 60 seconds, 128MiB of memory and 1 CPU;"));
        assert!(
            row("Secrets").starts_with("DATABASE_URL, NATS_URL: each run is given these by name")
        );
        assert_eq!(row("Setting BATCH"), "500");
        assert_eq!(
            row("Services on this node"),
            "It can reach pg1 on tcp port 5432 and nats on tcp port 4222, and no other service"
        );
        assert!(row("Private network").starts_with("No:"));
        assert!(
            row("Internet").starts_with("No: it can open no connection to a public address.")
                && row("Internet").contains("names are looked up through this node's DNS"),
            "{}",
            row("Internet")
        );
        // A private destination, filled in from the node's config when the proposal is made, is said in words.
        args["destinations"] = json!([{ "address": "10.4.0.9", "port": 443 }]);
        let rows = create_rows(&args);
        let private = &rows
            .iter()
            .find(|(label, _)| label == "Private network")
            .expect("the row")
            .1;
        assert!(private.contains("10.4.0.9 on tcp port 443"), "{private}");
        assert_eq!(
            create_summary(&args),
            "Create the function relay from registry.example/acme/relay and run it every 30 seconds"
        );
        // A function with nothing says so in each row.
        let bare = prepare(&json!({
            "name": "tick", "image": "registry.example/acme/tick", "digest": DIGEST,
            "triggers": [{ "timer": { "everySeconds": 3600 } }],
        }))
        .expect("a definition");
        let rows = create_rows(&bare);
        let value = |label: &str| {
            rows.iter()
                .find(|(found, _)| found == label)
                .unwrap()
                .1
                .clone()
        };
        assert_eq!(value("Command"), "the image's own entry point");
        assert!(value("Secrets").starts_with("None"));
        assert!(value("Services on this node").starts_with("None"));
        assert!(value("Runs").starts_with("every hour,"));
    }
}
