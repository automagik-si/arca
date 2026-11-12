//! The function verbs. A function is a service record with one more key (`crate::function`), so its create, its
//! removal and its secrets go the way a service's do: journalled config and ACL effects, secret values in private
//! files. What is a function's own is here: its definition comes from the call, a create and a removal run only from a
//! proposal the owner approved, and a run (`function_run`, and the timer) is neither an effect nor a proposal.
use super::*;
use crate::function::{self, Definition};
use crate::function_run::Refusal;
use crate::service::{ServiceConfig, ServiceGrant, ServiceLink};
use arca_kernel::desired::Entry;
use arca_netproject::destination::Destination;

/// How long a removal waits for a run that is going to be stopped.
/// It covers the longest thing a run does that a removal cannot cut short: one rebuild of its root, which Incus is
/// given a minute for, twice over. A removal therefore never gives up, its approval spent, on a run that was only
/// fetching its image.
const RETIRE_WAIT: Duration = Duration::from_secs(150);

/// Functions and services share one name space, because Incus refuses two instances of one name on a bridge. A verb
/// of one kind that names a record of the other is refused here, in words, before it does anything.
pub(super) fn kind_refusal(verb: &str, args: &Value, entries: &[Entry]) -> Option<CallError> {
    let records = service_entries(entries);
    let function = |name: &str| {
        records
            .iter()
            .find(|(_, config)| config.name == name)
            .map(|(_, config)| config.function.is_some())
    };
    if verb.starts_with("function_") {
        if matches!(verb, "function_create" | "function_list") {
            return None;
        }
        let name = args["name"].as_str()?;
        return (function(name) == Some(false)).then(|| {
            service_refused(format!(
                "{name} is a service, not a function: the service verbs act on it (service_status, service_remove)"
            ))
        });
    }
    if verb == "service_create" {
        let name = args["name"].as_str()?;
        return (function(name) == Some(true)).then(|| {
            service_refused(format!(
                "a function named {name} exists: functions and services share one name space, so a service cannot take the name"
            ))
        });
    }
    ["name", "from", "to"].into_iter().find_map(|key| {
        let name = args[key].as_str()?;
        (function(name) == Some(true)).then(|| {
            service_refused(format!(
                "{name} is a function, not a service: the function verbs act on it (function_list, function_runs, function_remove), and what it reaches is part of its definition"
            ))
        })
    })
}

/// What a `function_create` proposal names beyond what its caller wrote: the private destinations this node's config
/// gives the function's name, exactly as a service's are filled in. Everything the create itself would refuse for
/// what the node holds now is refused here, so the owner is never shown a card for it.
pub(super) fn enrich_function_create(
    cfg: &NodeConfig,
    entries: &[Entry],
    payload: &Value,
) -> Result<Value, CallError> {
    let payload = function::prepare(payload).map_err(CallError::Invalid)?;
    let definition = function::definition(&payload).map_err(CallError::Invalid)?;
    if cfg.kernel.service_cidr.is_none() {
        return Err(no_managed_network());
    }
    name_free(&definition.name, entries)?;
    providers(&definition, entries)?;
    let destinations = destinations_for(cfg, &definition.name, false, None)?;
    if payload
        .get("destinations")
        .is_some_and(|given| destinations.is_empty() || *given != json!(destinations))
    {
        return Err(destinations_mismatch(
            &definition.name,
            &destinations,
            false,
        ));
    }
    let mut payload = payload;
    if !destinations.is_empty() {
        if let Ok(hosts) = host_observation(&UnixIncus::new(cfg.incus.socket.clone()))
            && let Some(why) = own_address_refusal(&definition.name, &destinations, &hosts)
        {
            return Err(service_refused(why));
        }
        payload["destinations"] = json!(destinations);
    }
    Ok(payload)
}

/// Why the service `name` of the node's own project is not removed now: a function links to it. A function's links
/// are part of the definition its owner approved, so a service is never taken from under one: the function goes
/// first. `None` when no function links to it. `spent` says whether an owner's approval was already used up for this
/// removal, so the words about it are true on every path.
pub(super) fn linked_service_refusal(
    entries: &[Entry],
    name: &str,
    spent: bool,
) -> Option<CallError> {
    let functions: Vec<String> = service_entries(entries)
        .into_iter()
        .filter(|(_, config)| {
            config.function.is_some()
                && config.tenant.is_none()
                && config.links.iter().any(|link| link.to == name)
        })
        .map(|(_, config)| config.name)
        .collect();
    (!functions.is_empty()).then(|| linked_functions_refusal(name, &functions, spent))
}

/// The refusal of a removal of the service `name` that `functions` link to.
pub(super) fn linked_functions_refusal(name: &str, functions: &[String], spent: bool) -> CallError {
    let (links, them) = match functions {
        [one] => (
            format!("the function {one} links"),
            format!("the function {one}"),
        ),
        many => (
            format!("the functions {} link", many.join(", ")),
            "them".to_string(),
        ),
    };
    service_refused(format!(
        "remove service {name}: {links} to it, and what a function reaches is part of what its owner approved. Remove {them} first (function_remove); nothing was removed, {}",
        if spent {
            "and the owner's approval is spent, so another try needs a new one"
        } else {
            "and no approval was spent"
        }
    ))
}

/// A `function_remove` proposal for a name that is not a function's is refused when it is made.
pub(super) fn check_function_remove(entries: &[Entry], payload: &Value) -> Result<(), CallError> {
    let name = service_name(payload)?;
    match service_entries(entries)
        .into_iter()
        .find(|(_, config)| config.name == name)
    {
        Some((_, config)) if config.function.is_some() => Ok(()),
        Some(_) => Err(service_refused(format!(
            "{name} is a service, not a function: service_remove removes it"
        ))),
        None => Err(service_not_found(format!("function {name}: not found"))),
    }
}

/// A create proposal with the registry its image is fetched from written in, when this node holds a credential for
/// that registry: the card then says where the credential will rest. A caller that states another, or one where no
/// credential is held, is refused.
pub(super) fn with_registry(
    cfg: &NodeConfig,
    action: ActionKind,
    payload: Value,
) -> Result<Value, CallError> {
    let server = match action {
        ActionKind::FunctionCreate => payload["image"]
            .as_str()
            .and_then(crate::registry::server_of),
        _ if payload.get("tenant").is_some() => None,
        _ => payload["template"]
            .as_str()
            .and_then(|name| {
                crate::service::load_template(&cfg.state_dir.join("templates"), name).ok()
            })
            .filter(|template| template.kind == "oci")
            .and_then(|template| crate::registry::server_of(&template.image)),
    };
    let held = server.filter(|server| crate::registry::credential_set(cfg, server));
    let mut payload = payload;
    match (held, payload.get("registry").and_then(Value::as_str)) {
        (Some(server), None) => {
            payload["registry"] = json!(server);
            Ok(payload)
        }
        (Some(server), Some(said)) if said == server => Ok(payload),
        (None, None) => Ok(payload),
        _ => Err(CallError::Invalid(
            "args/registry: it is filled in from what this node holds: the address of the image's registry when a credential is set for it, and nothing otherwise".into(),
        )),
    }
}

fn no_managed_network() -> CallError {
    service_refused(
        "a function runs on the managed service network, where Arca writes the rules of what it may reach, and this node's config names none (kernel.serviceCidr)",
    )
}

fn function_not_found(name: &str) -> CallError {
    service_not_found(format!("function {name}: not found"))
}

/// Refuses a name a service or a function of this node already holds.
fn name_free(name: &str, entries: &[Entry]) -> Result<(), CallError> {
    match service_entries(entries)
        .into_iter()
        .find(|(_, config)| config.name == name)
    {
        Some((_, config)) if config.function.is_some() => {
            Err(service_refused(format!("function {name} exists")))
        }
        Some(_) => Err(service_refused(format!(
            "a service named {name} exists: functions and services share one name space, so a function cannot take the name"
        ))),
        None => Ok(()),
    }
}

/// The service each link of a definition names, with its record: a service of the node's own project on the managed
/// network that Arca created, and that declares the port. Anything else is refused in words.
fn providers(
    definition: &Definition,
    entries: &[Entry],
) -> Result<Vec<(Entry, ServiceConfig, u16)>, CallError> {
    let records = service_entries(entries);
    let mut found = Vec::new();
    for link in &definition.links {
        let Some((entry, config)) = records
            .iter()
            .find(|(_, config)| config.name == link.to && config.tenant.is_none())
        else {
            return Err(service_refused(format!(
                "links: no service named {} exists on this node; a function links to a service that is already there",
                link.to
            )));
        };
        if config.function.is_some() {
            return Err(service_refused(format!(
                "links: {} is a function, and a function serves nothing: a link goes to a service",
                link.to
            )));
        }
        if config.adopted {
            return Err(service_wiring::adopted(&link.to));
        }
        if config.on_host_bridge() {
            return Err(service_refused(format!(
                "links: the service {} is on a host bridge, where Arca writes no rules: a link needs the managed service network",
                link.to
            )));
        }
        if !config.ports.contains(&link.port) {
            let declared = config
                .ports
                .iter()
                .map(u16::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            return Err(service_refused(format!(
                "links: the service {} does not declare tcp port {}; {}",
                link.to,
                link.port,
                if declared.is_empty() {
                    "it declares no port".to_string()
                } else {
                    format!("it declares {declared}")
                }
            )));
        }
        found.push((entry.clone(), config.clone(), link.port));
    }
    Ok(found)
}

/// One function as `function_list` shows it.
fn function_view(config: &ServiceConfig, runner: &crate::function_run::FunctionRunner) -> Value {
    let Some(function) = &config.function else {
        return Value::Null;
    };
    let limits = config.limits.as_ref();
    let mut view = json!({
        "name": config.name,
        "kind": "container",
        "image": function.image,
        "digest": function.digest,
        "fingerprint": function.fingerprint,
        "command": function.command,
        "env": function.env,
        "secrets": function.secrets.iter().map(|secret| json!({
            "name": secret,
            "set": function::secret_set(config, secret),
        })).collect::<Vec<_>>(),
        "links": config.links.iter().map(|link| json!({ "to": link.to, "port": link.port })).collect::<Vec<_>>(),
        "triggers": [{ "timer": { "everySeconds": function.every_seconds } }],
        "limits": {
            "seconds": function.seconds,
            "memory": limits.and_then(|limits| limits.memory.clone()),
            "cpu": limits.and_then(|limits| limits.cpu.as_deref()).and_then(|cpu| cpu.parse::<u64>().ok()),
        },
        "concurrency": 1,
        "ipv4": config.ipv4,
        "created": config.created,
        "running": runner.running(&config.name),
        "lastRun": runner.runs(&config.name, 1).into_iter().next(),
    });
    if !config.destinations.is_empty() {
        view["destinations"] = json!(config.destinations);
    }
    view
}

impl ServiceHandler {
    /// After a removal: deletes each image of the node's store that Incus fetched for `config`'s registry and digest
    /// with a credential in the address, when no instance was made from it any more. Incus keeps that address in the
    /// image's record, so the credential leaves Incus with the image. An image fetched without a credential is left
    /// to Incus's own keeping, as before. A failure is said on the log: the removal itself stands.
    pub(super) fn drop_fetched_images(&self, read: &UnixIncus, config: &ServiceConfig) {
        // A node where no registry credential was ever set has no such image, and asks Incus nothing.
        if !crate::registry::ever_set(&self.cfg) {
            return;
        }
        let Ok(images) = read.get(&format!(
            "/1.0/images?project={}&recursion=1",
            config.project
        )) else {
            return;
        };
        let (Some(alias), Some(host)) = (
            config.source["alias"].as_str(),
            config.source["server"]
                .as_str()
                .and_then(function::registry_host),
        ) else {
            return;
        };
        let Ok(family) = arca_incus::ProjectFamily::new(
            [arca_netproject::PROJECT],
            self.cfg.kernel.droidd_projects.iter().map(String::as_str),
        ) else {
            return;
        };
        let writer =
            arca_incus::function_run::FunctionRun::new(self.cfg.incus.socket.clone(), family);
        for image in images.as_array().into_iter().flatten() {
            let from = &image["update_source"];
            let kept = from["server"].as_str().unwrap_or_default();
            let with_credential = kept
                .strip_prefix("https://")
                .and_then(|rest| rest.split(['/', '?', '#']).next())
                .is_some_and(|authority| authority.contains('@'));
            if from["protocol"] == "oci"
                && from["alias"].as_str() == Some(alias)
                && function::registry_host(kept).as_deref() == Some(host.as_str())
                && with_credential
                && let Some(fingerprint) = image["fingerprint"].as_str()
                && writer
                    .drop_unused_image(&config.project, fingerprint)
                    .is_err()
            {
                eprintln!(
                    "arca: the image {} was fetched with a registry's credential and could not be deleted after its last user was removed: Incus still holds that credential with it",
                    &fingerprint[..fingerprint.len().min(12)]
                );
            }
        }
    }

    fn runner(&self) -> Result<&Arc<crate::function_run::FunctionRunner>, CallError> {
        self.functions
            .as_ref()
            .ok_or_else(|| service_refused("this node runs no functions"))
    }

    /// The function verbs but `function_secret_set`, which is the service wiring's own secret path.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn call_function(
        &self,
        args: &Value,
        read: &UnixIncus,
        entries: &[Entry],
        rows: &[Value],
        cidr: &str,
        gateway: &str,
        now: u64,
        preapproved: bool,
    ) -> Result<Value, CallError> {
        match self.verb {
            "function_list" => {
                let runner = self.runner()?;
                let mut views: Vec<Value> = service_entries(entries)
                    .iter()
                    .filter(|(_, config)| config.function.is_some())
                    .map(|(_, config)| function_view(config, runner))
                    .collect();
                views.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
                Ok(json!({ "functions": views }))
            }
            "function_run" => {
                let runner = self.runner()?;
                let name = service_name(args)?;
                match runner.spawn(&name, "manual") {
                    Ok(run) => Ok(json!({ "function": name, "run": run, "started": true })),
                    Err(Refusal::Unknown) => Err(function_not_found(&name)),
                    Err(Refusal::Busy) => Err(CallError::Refused {
                        code: "FunctionBusy".into(),
                        message: format!(
                            "a run of the function {name} is going, and one runs at a time: function_runs shows it when it ends"
                        ),
                    }),
                    Err(Refusal::Retiring) => Err(service_refused(format!(
                        "the function {name} is being removed: nothing was started"
                    ))),
                }
            }
            "function_runs" => {
                let runner = self.runner()?;
                let name = service_name(args)?;
                if !service_entries(entries)
                    .iter()
                    .any(|(_, config)| config.name == name && config.function.is_some())
                {
                    return Err(function_not_found(&name));
                }
                if let Some(run) = args.get("run") {
                    let run = run
                        .as_str()
                        .filter(|run| crate::function_run::run_id_ok(run))
                        .ok_or_else(|| CallError::Invalid("run: expected a run id".into()))?;
                    let Some((line, output)) = runner.run_output(&name, run) else {
                        return Err(service_not_found(format!(
                            "function {name}: no run {run} among its newest {}",
                            crate::function_run::RECENT
                        )));
                    };
                    return Ok(json!({
                        "function": name,
                        "run": line,
                        "output": output.map(|text| text.lines().map(str::to_string).collect::<Vec<_>>()),
                    }));
                }
                let limit = match args.get("limit") {
                    None => 20,
                    Some(value) => value
                        .as_u64()
                        .filter(|limit| (1..=crate::function_run::RECENT as u64).contains(limit))
                        .ok_or_else(|| {
                            CallError::Invalid(format!(
                                "limit: expected 1 to {}",
                                crate::function_run::RECENT
                            ))
                        })?,
                };
                Ok(json!({
                    "function": name,
                    "running": runner.running(&name),
                    "runs": runner.runs(&name, limit as usize),
                }))
            }
            "function_remove" => {
                let runner = self.runner()?;
                let name = service_name(args)?;
                if !preapproved {
                    return Err(import_not_approved());
                }
                let Some((entry, config)) = service_entries(entries)
                    .into_iter()
                    .find(|(_, config)| config.name == name && config.function.is_some())
                else {
                    return Err(function_not_found(&name));
                };
                // No run begins from here on, and the one that is going is stopped: Incus deletes a stopped instance.
                if !runner.retire(&name, RETIRE_WAIT) {
                    runner.restore(&name);
                    return Err(service_refused(format!(
                        "remove function {name}: a run of it is still going and could not be stopped; nothing was removed, and the owner's approval is spent, so another try needs a new one"
                    )));
                }
                let id = entry.id.clone();
                let removal = {
                    let _write = self.writes.lock().map_err(|_| internal())?;
                    let mut kernel = lock(&self.kernel)?;
                    let spent = kernel.spend_service_removal_authorized(&id);
                    kernel.remove_service_spent(spent, now)
                };
                let removal = match removal {
                    Ok(removal) => removal,
                    Err(crate::kernel::ServiceRemovalError::LinkedFunctions(functions)) => {
                        runner.restore(&name);
                        return Err(linked_functions_refusal(&name, &functions, true));
                    }
                    Err(_) => {
                        runner.restore(&name);
                        return Err(service_removal_refused(read, &config));
                    }
                };
                let changed = removal
                    .changed
                    .iter()
                    .map(|entry| entry.0.as_str())
                    .collect::<Vec<_>>();
                if service_wiring::failed_for(&removal.outcomes, &changed)
                    || removal
                        .run
                        .as_ref()
                        .map_or(true, |run| run.as_ref().is_some_and(|run| run.partial))
                {
                    runner.restore(&name);
                    return Err(service_removal_refused(read, &config));
                }
                let _ = std::fs::remove_dir_all(self.cfg.state_dir.join("secrets").join(&id.0));
                runner.forget(&name);
                // The image it was fetched from with a registry's credential goes with its last user.
                self.drop_fetched_images(read, &config);
                Ok(json!({ "removed": true }))
            }
            "function_create" => {
                self.runner()?;
                if !preapproved {
                    return Err(import_not_approved());
                }
                self.create_function(args, read, entries, rows, cidr, gateway, now)
            }
            _ => Err(service_not_found("unknown function verb")),
        }
    }

    /// Creates a function from the arguments an approved proposal carries: its record, its ACL and its stopped
    /// instance in one change, then one rule on each linked service's NIC that lets the function in.
    #[allow(clippy::too_many_arguments)]
    fn create_function(
        &self,
        args: &Value,
        read: &UnixIncus,
        entries: &[Entry],
        rows: &[Value],
        cidr: &str,
        gateway: &str,
        now: u64,
    ) -> Result<Value, CallError> {
        let definition = function::definition(args).map_err(CallError::Invalid)?;
        let name = definition.name.clone();
        if self.cfg.kernel.service_cidr.is_none() {
            return Err(no_managed_network());
        }
        name_free(&name, entries)?;
        if rows
            .iter()
            .any(|row| row["name"].as_str() == Some(name.as_str()))
        {
            return Err(service_refused(format!(
                "an instance named {name} already exists that is not Arca's function; it is left alone"
            )));
        }
        let providers = providers(&definition, entries)?;
        // An image fetched with this node's credential for its registry is said on the approved card, or not run.
        crate::registry::stated(
            &self.cfg,
            crate::registry::server_of(&definition.function.image).as_deref(),
            args.get("registry"),
            // A function is created only from an approved proposal: the check above this call.
            true,
        )?;
        // The private destinations are the ones this node's config names for the function now, and the approved
        // proposal states exactly those: a config that changed since the card was shown refuses the create.
        let destinations = destinations_for(
            &self.cfg,
            &name,
            false,
            Some(DestinationsCall {
                stated: args.get("destinations"),
                approved: true,
            }),
        )?;
        if !destinations.is_empty() {
            let hosts = host_observation(read)?;
            if let Some(why) = own_address_refusal(&name, &destinations, &hosts) {
                return Err(service_refused(why));
            }
        }
        // Each linked service must be live and Arca's own before anything is written: its uuid is part of the link.
        let mut linked = Vec::new();
        for (entry, config, port) in &providers {
            let owned = self.live_owned(entry, config)?;
            linked.push((
                entry.clone(),
                config.clone(),
                *port,
                owned.uuid().unwrap_or("").to_string(),
            ));
        }
        let mut function = definition.function.clone();
        function.reach = linked
            .iter()
            .filter_map(|(_, config, port, _)| {
                Some(Destination {
                    address: config.ipv4.parse().ok()?,
                    port: *port,
                })
            })
            .collect();
        if function.reach.len() != linked.len() {
            return Err(internal());
        }
        let id = crate::service::entry_id(None, &name);
        let ipv4 = self.free_address(read, cidr, rows, entries, None)?;
        let pool = service_pool(&self.cfg)
            .map_err(|_| service_refused("the default profile does not name a storage pool"))?;
        let mut changes = ensure_netproject(entries, &self.cfg).map_err(no_service_project)?;
        let mut config = ServiceConfig {
            name: name.clone(),
            template: function::TEMPLATE.to_string(),
            instance: name.clone(),
            ipv4,
            ports: Vec::new(),
            links: linked
                .iter()
                .map(|(_, provider, port, uuid)| ServiceLink {
                    to: provider.name.clone(),
                    port: *port,
                    env: Vec::new(),
                    uuid: uuid.clone(),
                })
                .collect(),
            incoming: Vec::new(),
            secret_refs: BTreeMap::new(),
            environment_patch: None,
            published: None,
            created: now,
            source: json!(null),
            pool,
            project: arca_netproject::PROJECT.into(),
            bridge: arca_netproject::BRIDGE.into(),
            gateway: gateway.to_string(),
            entrypoint: function::entrypoint(&function.command),
            // The plain values. A secret's value joins them when it is set, as a reference to its file.
            environment: function.env.clone(),
            droidd_port: None,
            limits: Some(definition.limits.clone()),
            autostart: false,
            data: None,
            lan: None,
            start: false,
            adopted: false,
            creating: false,
            vm: None,
            tenant: None,
            kernel_access: None,
            destinations,
            function: Some(function),
        };
        let template = function::template(&config).ok_or_else(internal)?;
        config.source = crate::service::source(&template).map_err(service_template_error)?;
        changes.push(crate::desired_store::EntryChange::Put(service_entry(
            &id,
            "arca.service",
            serde_json::to_value(&config).map_err(|_| internal())?,
        )));
        self.settle_create(read, &name, function::TEMPLATE, changes, &config, None, now)?;
        // The image Incus fetched for it is written into its record: the one copy each run's root is rebuilt from.
        // It is the record of the node's store whose registry and digest are the approved ones.
        if let Ok(images) = read.get(&format!(
            "/1.0/images?project={}&recursion=1",
            config.project
        )) && let Ok(made) = read.get(&format!(
            "/1.0/instances/{}?project={}",
            config.instance, config.project
        )) && let Some(fingerprint) = function::fetched_fingerprint(
            &images,
            &config.source,
            made["config"]["volatile.base_image"].as_str(),
        ) {
            let Some((_, mut held)) = service_entries(lock(&self.kernel)?.entries())
                .into_iter()
                .find(|(entry, _)| entry.id.0 == id)
            else {
                return Err(internal());
            };
            if let Some(function) = held.function.as_mut() {
                function.fingerprint = Some(fingerprint);
            }
            held.environment_patch = Some(BTreeSet::new());
            self.put(&[held], now)?;
        }
        // The function exists, stopped. Each linked service now lets it in: one rule on that service's NIC, bound to
        // this instance's uuid and address, exactly as a link between two services is.
        let created = lock(&self.kernel)?.entries().to_vec();
        let Some((entry, config)) = service_entries(&created)
            .into_iter()
            .find(|(entry, _)| entry.id.0 == id)
        else {
            return Err(internal());
        };
        let opened: Result<(), CallError> = (|| {
            if linked.is_empty() {
                return Ok(());
            }
            let own = self.live_owned(&entry, &config)?;
            for (provider_entry, _, port, uuid) in &linked {
                let Some((_, mut provider)) = service_entries(lock(&self.kernel)?.entries())
                    .into_iter()
                    .find(|(found, _)| found.id == provider_entry.id)
                else {
                    return Err(service_refused("a linked service is gone"));
                };
                provider.environment_patch = Some(BTreeSet::new());
                provider.incoming.retain(|grant| grant.consumer != name);
                provider.incoming.push(ServiceGrant {
                    consumer: name.clone(),
                    address: config.ipv4.clone(),
                    port: *port,
                    uuid: own.uuid().unwrap_or("").into(),
                    provider_uuid: uuid.clone(),
                });
                self.put(&[provider], now)?;
            }
            Ok(())
        })();
        if let Err(error) = opened {
            let why = match &error {
                CallError::Refused { message, .. } => message.clone(),
                _ => "Arca hit an internal error".to_string(),
            };
            return Err(service_refused(format!(
                "create function {name}: the function was created, but a linked service could not be opened to it ({why}). Its runs cannot reach that service: function_remove removes it, and the create is proposed again"
            )));
        }
        let runner = self.runner()?;
        Ok(function_view(&config, runner))
    }
}
