//! `service_reconfigure`: a container service is made what its template says now, behind one owner card.
//!
//! The service's record is rendered again from its CURRENT template: its environment (every value the template gives,
//! then what each of its links provides, as `service_connect` gave it), the secrets the template declares, its ports,
//! its `rawLxc` lines and its no-new-privileges. The difference from the record is the `changes` of the proposal: the
//! node works it out when the proposal is made, the card shows it row by row, and the approval binds it. When the
//! approved call runs it works the difference out again and refuses, changing nothing, unless it is the one the owner
//! approved; so what is applied is exactly what the card showed.
//!
//! A secret is named, never shown: an environment value that holds a secret, or that a link provides, is shown by its
//! key and the names of the secrets it holds. A secret the template declares and the service does not hold is shown
//! as a secret whose value is set separately (or that Arca makes now, when the template says `generate`). Nothing a
//! secret holds reaches the card, the answer or the journal.
//!
//! Then, in that order and once each: the record is saved (the node writes the instance's config from it), the
//! service is restarted when it runs, and its config is read back and compared with what was applied. A write the
//! node refused, or a restart that failed, puts the old record back. The steps are journalled in the service update
//! journal with key and secret names only. The image and the data volume are not touched.
use super::service_update::{Claim, Journal, SERVICE_UPDATES_FILE, state_changer_for};
use super::*;
use crate::service::{ServiceConfig, Template};
use arca_incus::StateAction;
use arca_kernel::desired::Entry;
use serde_json::Map;

/// What stands for a secret the template makes and the service does not hold yet, while a proposal is worked out: it
/// is made only when the approved call runs. Nothing a template renders can hold this text.
const NOT_MADE_YET: &str = "\u{0}not made yet\u{0}";

/// How an environment value of the service is shown.
#[derive(Clone, PartialEq)]
enum Kind {
    /// From the template alone: the value is shown.
    Plain,
    /// From the template, holding these secrets: shown by their names.
    Secrets(Vec<String>),
    /// What the link to this provider gives: never shown, since it may hold the provider's secrets.
    Link(String),
}

/// The service as its template says now, and how that differs from its record.
pub(super) struct Plan {
    /// The record as it will be. Its environment holds each value as it is, not yet behind a reference.
    next: ServiceConfig,
    /// The environment keys whose values are kept behind a reference: those that hold a secret or come from a link.
    hidden: BTreeSet<String>,
    /// The secrets the template makes that the service does not hold yet, with their prefixes.
    generate: Vec<(String, String)>,
    /// The difference, as the card shows it and the approval binds it.
    pub(super) changes: Value,
    /// Whether the record changes: false when the only difference is a secret whose value is set separately.
    writes: bool,
}

/// The names of the secrets `pattern` holds, in order, each once.
fn secrets_in(pattern: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = pattern;
    while let Some(at) = rest.find("{{secret:") {
        let after = &rest[at + "{{secret:".len()..];
        let Some(end) = after.find("}}") else { break };
        let name = after[..end].to_string();
        if !names.contains(&name) {
            names.push(name);
        }
        rest = &after[end + 2..];
    }
    names
}

/// The value of each secret `template` declares that the service `config` holds.
fn held_values(
    node: &NodeConfig,
    config: &ServiceConfig,
    template: &Template,
) -> BTreeMap<String, String> {
    template
        .secrets
        .iter()
        .filter_map(|key| {
            let reference = service_wiring::secret_reference_in(node, config, key)?;
            let value = service_wiring::read_reference_in(node, &reference).ok()?;
            Some((key.clone(), value))
        })
        .collect()
}

/// What the link of `config` to `provider` gives now, by the provider's template: each value it provides, rendered.
/// `None` when the provider cannot be read or one of its values cannot be rendered: the link's values stay as they are.
fn link_values(
    node: &NodeConfig,
    provider: &ServiceConfig,
    port: u16,
) -> Option<BTreeMap<String, String>> {
    if provider.adopted || provider.function.is_some() {
        return None;
    }
    let template =
        crate::service::load_template(&node.state_dir.join("templates"), &provider.template)
            .ok()?;
    let values = held_values(node, provider, &template);
    let mut env = BTreeMap::new();
    for (key, pattern) in template
        .provides
        .as_ref()
        .and_then(|provides| provides["env"].as_object())
        .into_iter()
        .flatten()
    {
        let rendered = crate::service::render(
            pattern.as_str()?,
            &provider.name,
            &provider.ipv4,
            &provider.gateway,
            Some(port),
            &values,
        )?;
        env.insert(key.clone(), rendered);
    }
    Some(env)
}

/// The record's value of each environment key, as it is: a reference is read. `None` for a value that cannot be read.
/// The second part is whether the record holds the value itself, which then never held a secret and may be shown.
fn current_values(
    node: &NodeConfig,
    config: &ServiceConfig,
) -> BTreeMap<String, (Option<String>, bool)> {
    config
        .environment
        .iter()
        .map(|(key, value)| {
            let current = if value.contains(arca_providers::incus::SECRET_PREFIX) {
                (
                    value
                        .starts_with(arca_providers::incus::SECRET_PREFIX)
                        .then(|| service_wiring::read_reference_in(node, value).ok())
                        .flatten(),
                    false,
                )
            } else {
                (Some(value.clone()), true)
            };
            (key.clone(), current)
        })
        .collect()
}

/// The service `config` as its template says now. Reads files and nothing else: no secret is made and nothing written.
/// `made` names the secrets this run has just generated: they are shown as generated, as the card showed them, and
/// rendered with the values now in their files.
pub(super) fn plan(
    node: &NodeConfig,
    entries: &[Entry],
    config: &ServiceConfig,
    made: &BTreeSet<String>,
) -> Result<Plan, CallError> {
    let name = &config.name;
    let template =
        crate::service::load_template(&node.state_dir.join("templates"), &config.template)
            .map_err(service_template_error)?;
    if template.is_vm() {
        return Err(service_refused(format!(
            "reconfigure service {name}: its template {} is now a virtual machine's, and a container is not made one by a reconfigure. Nothing was changed",
            config.template
        )));
    }
    // The secrets: the ones the service holds, and the ones the template makes that it does not hold yet.
    let mut values = held_values(node, config, &template);
    let mut generate = Vec::new();
    let mut secret_rows = Vec::new();
    for key in &template.secrets {
        if made.contains(key) && values.contains_key(key) {
            secret_rows.push(json!({ "name": key, "change": "generated" }));
            continue;
        }
        if values.contains_key(key) {
            continue;
        }
        if let Some(prefix) = template.generate.get(key) {
            generate.push((key.clone(), prefix.clone()));
            values.insert(key.clone(), NOT_MADE_YET.to_string());
            secret_rows.push(json!({ "name": key, "change": "generated" }));
        } else {
            secret_rows.push(json!({ "name": key, "change": "declared" }));
        }
    }
    let mut secret_refs = config.secret_refs.clone();
    for key in config.secret_refs.keys() {
        if !template.secrets.contains(key) {
            secret_rows.push(json!({ "name": key, "change": "removed" }));
            secret_refs.remove(key);
        }
    }
    // The environment: the template's values, then what each link gives, which wins over the template's.
    let mut next_env: BTreeMap<String, (String, Kind)> = BTreeMap::new();
    let mut waiting: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (key, pattern) in &template.env {
        let held = secrets_in(pattern);
        match crate::service::render(
            pattern,
            name,
            &config.ipv4,
            &config.gateway,
            template.ports.first().copied(),
            &values,
        ) {
            Some(value) => {
                let kind = if held.is_empty() {
                    Kind::Plain
                } else {
                    Kind::Secrets(held)
                };
                next_env.insert(key.clone(), (value, kind));
            }
            None => {
                waiting.insert(
                    key.clone(),
                    held.into_iter()
                        .filter(|secret| !values.contains_key(secret))
                        .collect(),
                );
            }
        }
    }
    let current = current_values(node, config);
    let mut links = config.links.clone();
    for link in &mut links {
        let provider = service_entries(entries)
            .into_iter()
            .map(|(_, provider)| provider)
            .find(|provider| provider.name == link.to);
        match provider.and_then(|provider| link_values(node, &provider, link.port)) {
            Some(env) => {
                link.env = env.keys().cloned().collect();
                for (key, value) in env {
                    waiting.remove(&key);
                    next_env.insert(key, (value, Kind::Link(link.to.clone())));
                }
            }
            // A provider that cannot be read keeps giving what it gave.
            None => {
                for key in &link.env {
                    if let Some((Some(value), _)) = current.get(key) {
                        waiting.remove(key);
                        next_env.insert(key.clone(), (value.clone(), Kind::Link(link.to.clone())));
                    }
                }
            }
        }
    }
    // The rows: removed, then changed, then added, each in key order.
    let mut removed = Vec::new();
    let mut changed = Vec::new();
    let mut added = Vec::new();
    for (key, (value, kind)) in &next_env {
        let mut row = json!({ "key": key });
        match kind {
            Kind::Plain => row["value"] = json!(value),
            Kind::Secrets(names) => row["secrets"] = json!(names),
            Kind::Link(provider) => row["link"] = json!(provider),
        }
        match current.get(key) {
            None => {
                row["change"] = json!("added");
                added.push(row);
            }
            Some((now, shown)) if now.as_deref() != Some(value.as_str()) => {
                row["change"] = json!("changed");
                if *shown && *kind == Kind::Plain {
                    row["was"] = json!(now);
                }
                changed.push(row);
            }
            Some(_) => {}
        }
    }
    for key in current.keys().filter(|key| !next_env.contains_key(*key)) {
        let mut row = json!({ "key": key, "change": "removed" });
        if let Some(names) = waiting.get(key).filter(|names| !names.is_empty()) {
            row["needs"] = json!(names);
        }
        removed.push(row);
    }
    let environment: Vec<Value> = removed.into_iter().chain(changed).chain(added).collect();
    let mut changes = Map::new();
    if !environment.is_empty() {
        changes.insert("environment".into(), json!(environment));
    }
    if !secret_rows.is_empty() {
        changes.insert("secrets".into(), json!(secret_rows));
    }
    if template.ports != config.ports {
        changes.insert(
            "ports".into(),
            json!({ "from": config.ports, "to": template.ports }),
        );
    }
    // Low-level LXC config is the node's own project's alone: a tenant's service keeps what it has.
    let (raw_lxc, no_new_privileges) = if config.tenant.is_none() {
        (template.raw_lxc.clone(), template.no_new_privileges)
    } else {
        (config.raw_lxc.clone(), config.no_new_privileges)
    };
    // The card shows `raw.lxc` whole, as the instance carries it: `lxc.no_new_privs=1` (when set) beside the
    // template's own lines.
    let lines = |record: &ServiceConfig| -> Vec<String> {
        crate::service::raw_lxc_value(record)
            .map(|value| value.split('\n').map(str::to_string).collect())
            .unwrap_or_default()
    };
    let mut wanted = config.clone();
    wanted.raw_lxc = raw_lxc.clone();
    wanted.no_new_privileges = no_new_privileges;
    if raw_lxc != config.raw_lxc {
        changes.insert(
            "rawLxc".into(),
            json!({ "from": lines(config), "to": lines(&wanted) }),
        );
    }
    if no_new_privileges != config.no_new_privileges {
        changes.insert(
            "noNewPrivileges".into(),
            json!({ "from": config.no_new_privileges, "to": no_new_privileges }),
        );
    }
    let writes = changes.keys().any(|section| section != "secrets")
        || secret_refs.len() != config.secret_refs.len()
        || !generate.is_empty();
    let hidden = next_env
        .iter()
        .filter(|(_, (_, kind))| *kind != Kind::Plain)
        .map(|(key, _)| key.clone())
        .collect();
    let mut next = config.clone();
    next.environment = next_env
        .into_iter()
        .map(|(key, (value, _))| (key, value))
        .collect();
    next.environment_patch = None;
    next.secret_refs = secret_refs;
    next.ports = template.ports.clone();
    next.links = links;
    next.raw_lxc = raw_lxc;
    next.no_new_privileges = no_new_privileges;
    Ok(Plan {
        next,
        hidden,
        generate,
        changes: Value::Object(changes),
        writes,
    })
}

/// The names in `changes`, for the journal: each key and secret with what happens to it, and never a value.
fn names_of(changes: &Value) -> Value {
    let rows = |section: &str, field: &str| -> Vec<String> {
        changes[section]
            .as_array()
            .into_iter()
            .flatten()
            .map(|row| {
                format!(
                    "{} {}",
                    row["change"].as_str().unwrap_or_default(),
                    row[field].as_str().unwrap_or_default()
                )
            })
            .collect()
    };
    json!({
        "environment": rows("environment", "key"),
        "secrets": rows("secrets", "name"),
        "ports": changes.get("ports").is_some(),
        "rawLxc": changes.get("rawLxc").is_some(),
        "noNewPrivileges": changes.get("noNewPrivileges").is_some(),
    })
}

/// What a proposal of `service_reconfigure` is given when it is made: the difference the run would apply, so the card
/// shows it and the approval binds it. A service that cannot be found or is not a container Arca made is left to the
/// run, which says why.
pub(super) fn check_proposal(
    node: &NodeConfig,
    entries: &[Entry],
    payload: &Value,
) -> Result<Value, CallError> {
    let mut payload = payload.clone();
    if let Some(object) = payload.as_object_mut() {
        object.remove("changes");
    }
    let Some(name) = payload["name"].as_str() else {
        return Ok(payload);
    };
    let scope = tenants::tenant_arg(node, &payload, entries)?;
    let scoped = tenants::scoped_entries(entries, scope.as_ref());
    let Some((_, config)) = service_entries(&scoped)
        .into_iter()
        .find(|(_, config)| config.name == name)
    else {
        return Ok(payload);
    };
    if config.adopted || config.function.is_some() || config.vm.is_some() {
        return Ok(payload);
    }
    payload["changes"] = plan(node, &scoped, &config, &BTreeSet::new())?.changes;
    Ok(payload)
}

impl ServiceHandler {
    /// `service_reconfigure`: makes the service `name` what its template says now, exactly as the approved card
    /// showed. `entries` are the ones the call may see, a tenant's when it names one. Only an approved proposal runs it.
    pub(super) fn call_reconfigure(
        &self,
        args: &Value,
        entries: &[Entry],
        approved: bool,
    ) -> Result<Value, CallError> {
        let name = service_name(args)?;
        let said = |why: &str| {
            service_refused(format!(
                "reconfigure service {name}: {why}. Nothing was changed"
            ))
        };
        if !approved {
            return Err(said(
                "it runs only from a card the owner approved: propose it with action_propose, and the card shows every change it makes",
            ));
        }
        let Some((entry, config)) = service_entries(entries)
            .into_iter()
            .find(|(_, config)| config.name == name)
        else {
            return Err(service_not_found(format!("service {name}: not found")));
        };
        if config.adopted {
            return Err(service_wiring::adopted(&name));
        }
        if config.function.is_some() || config.vm.is_some() {
            return Err(said(
                "only a container service is reconfigured from its template",
            ));
        }
        let _claim = Claim::take(&format!("{}\n{}", self.cfg.state_dir.display(), entry.id.0))
            .ok_or_else(|| said("another update of this service is running"))?;
        let first = plan(&self.cfg, entries, &config, &BTreeSet::new())?;
        if args.get("changes") != Some(&first.changes) {
            return Err(said(
                "the service or its template changed since the owner's card was made, so the card no longer says what would be applied: propose it again",
            ));
        }
        if !first.writes {
            return Ok(json!({
                "reconfigured": false,
                "service": name,
                "changes": first.changes,
                "restarted": false,
                "message": if first.changes.as_object().is_some_and(Map::is_empty) {
                    "the service already is what its template says; nothing was changed".to_string()
                } else {
                    "the only difference is a secret whose value is set separately (service_secret_set); nothing was changed and the service was not restarted".to_string()
                },
            }));
        }
        let changer = state_changer_for(&self.cfg, &config)?;
        let seen = changer
            .read(&config.project, &config.instance)
            .map_err(|_| said("its instance was not read"))?;
        let journalled = lock(&self.kernel)?.service_uuid(&entry.id, &config.instance);
        if seen.kind != "container" || journalled.is_none() || journalled != seen.uuid {
            return Err(said("its instance is not the one Arca created for it"));
        }
        let journal = Journal {
            path: self.cfg.state_dir.join(SERVICE_UPDATES_FILE),
            id: format!("reconfigure-{}-{}", entry.id.0, (self.clock)()),
            service: name.clone(),
            clock: self.clock.clone(),
        };
        journal
            .line("begin", true, names_of(&first.changes))
            .map_err(|_| said("the update journal could not be written"))?;
        // The secrets the template makes are made now, as a create makes them, and the values rendered again with them.
        let owner = config.entry_id();
        let secrets_dir = self.cfg.state_dir.join("secrets");
        for (key, prefix) in &first.generate {
            crate::service::generate_secret(&secrets_dir, &owner, key, prefix)
                .map_err(service_template_error)?;
        }
        let generated: BTreeSet<String> =
            first.generate.iter().map(|(key, _)| key.clone()).collect();
        // By now the secrets made above are files on the node, so a refusal from here on says the service was not
        // changed, and names those secrets, which stay for the next card.
        let kept = if generated.is_empty() {
            String::new()
        } else {
            format!(
                " The secrets Arca made for it ({}) are kept for the next card.",
                generated.iter().cloned().collect::<Vec<_>>().join(", ")
            )
        };
        let made = plan(&self.cfg, entries, &config, &generated)?;
        if made.changes != first.changes || !made.generate.is_empty() {
            journal.step("end", false, json!({ "why": "the plan moved" }));
            return Err(service_refused(format!(
                "reconfigure service {name}: the service changed while it was reconfigured: propose it again. The service was not changed.{kept}"
            )));
        }
        let applied = made.next.environment.clone();
        let mut next = made.next;
        for key in &made.hidden {
            if let Some(value) = next.environment.get(key).cloned() {
                next.environment
                    .insert(key.clone(), self.store_value(&owner, &value)?);
            }
        }
        let fields = |record: &mut ServiceConfig, from: &ServiceConfig| {
            record.environment = from.environment.clone();
            record.environment_patch = from.environment_patch.clone();
            record.secret_refs = from.secret_refs.clone();
            record.ports = from.ports.clone();
            record.links = from.links.clone();
            record.raw_lxc = from.raw_lxc.clone();
            record.no_new_privileges = from.no_new_privileges;
        };
        // The node saves the record before it writes the instance, so a refused write leaves the new record saved:
        // it is put back, and the answer says whether that worked. The old record's environment applies whole.
        let mut old = config.clone();
        old.environment_patch = None;
        let put_back = || {
            self.amend(&entry.id.0, &name, |record| fields(record, &old))
                .is_ok()
        };
        // The record is read again under the node's write gate right before it is written: a link, a secret or a
        // port that changed since the plan was made is never overwritten by a difference the card did not show.
        let planned = |record: &ServiceConfig| {
            serde_json::to_value((
                &record.environment,
                &record.links,
                &record.secret_refs,
                &record.ports,
                &record.raw_lxc,
                record.no_new_privileges,
            ))
            .ok()
        };
        let mut stale = false;
        let written = self.amend(&entry.id.0, &name, |record| {
            if planned(record) == planned(&config) {
                fields(record, &next);
            } else {
                stale = true;
            }
        });
        // A stale record was left as it is, so it is never put back, even when the node refused to save it: putting
        // the old record back would undo the change made meanwhile.
        if stale {
            journal.step(
                "end",
                false,
                json!({ "why": "the record changed since the plan", "saved": written.is_ok() }),
            );
            return Err(service_refused(format!(
                "reconfigure service {name}: its record changed since the card was made (a link or a secret was set meanwhile): propose it again. The service was not changed.{kept}"
            )));
        }
        if let Err(error) = written {
            let why = match &error {
                CallError::Refused { message, .. } | CallError::Invalid(message) => message.clone(),
                _ => "the node did not apply it".to_string(),
            };
            let restored = put_back();
            journal.step("apply", false, json!({ "restored": restored }));
            return Err(service_refused(format!(
                "reconfigure service {name}: the change was not applied: {why}. {}",
                if restored {
                    format!(
                        "The service's record is as it was, and the service was not changed.{kept}"
                    )
                } else {
                    "The service's record could not be put back: service_status shows what it says"
                        .to_string()
                }
            )));
        }
        journal.step("apply", true, json!({}));
        let was_running = seen.is_running();
        if was_running && let Err(error) = changer.change(&seen, StateAction::Restart) {
            let restored = put_back();
            let started = changer
                .read(&config.project, &config.instance)
                .is_ok_and(|state| {
                    state.is_running() || changer.change(&state, StateAction::Start).is_ok()
                });
            journal.step(
                "restart",
                false,
                json!({ "restored": restored, "running": started }),
            );
            return Err(service_refused(format!(
                "reconfigure service {name}: the restart failed ({error}). {} {}",
                if restored {
                    "The service's record and its instance's config are as they were."
                } else {
                    "The service's record could not be put back: service_status shows what it says."
                },
                if started {
                    "The service is running."
                } else {
                    "The service is stopped: instance_start starts it."
                },
            )));
        }
        journal.step("restart", true, json!({ "restarted": was_running }));
        // The config read back, against what was applied: by key, never by value.
        let read = changer.read(&config.project, &config.instance);
        let (running, differs) = match &read {
            Ok(state) => {
                let mut differs: Vec<String> = Vec::new();
                for (key, value) in &applied {
                    if state.own_config.get(&format!("environment.{key}")) != Some(value) {
                        differs.push(key.clone());
                    }
                }
                for key in config.environment.keys() {
                    if !applied.contains_key(key)
                        && state.own_config.contains_key(&format!("environment.{key}"))
                    {
                        differs.push(key.clone());
                    }
                }
                if state.own_config.get("raw.lxc").cloned() != crate::service::raw_lxc_value(&next)
                {
                    differs.push("raw.lxc".to_string());
                }
                (state.is_running(), differs)
            }
            Err(_) => (false, vec!["(the instance could not be read)".to_string()]),
        };
        journal.step(
            "read back",
            differs.is_empty(),
            json!({ "differs": differs, "running": running }),
        );
        journal.step("end", differs.is_empty(), json!({}));
        Ok(json!({
            "reconfigured": true,
            "service": name,
            "changes": first.changes,
            "restarted": was_running,
            "running": running,
            "readBack": { "matches": differs.is_empty(), "differs": differs },
            "message": format!(
                "service {name} is what its template says now{}; {}",
                if was_running { ", and was restarted once for it" } else { ", from its next start" },
                if differs.is_empty() {
                    "its config read back as applied".to_string()
                } else {
                    format!("its config read back differs at {}: service_status shows how it is", differs.join(", "))
                }
            ),
        }))
    }
}
