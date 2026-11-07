//! Service wiring uses normal journalled config and ACL effects. Secret values live in immutable private files;
//! desired state and inverses contain references only. Instance start/restart is not a plugin capability.
use super::*;
use crate::{
    service::{self, Published, ServiceConfig, ServiceGrant, ServiceLink},
    service_exec::{ExecError, ExecTarget},
};
use arca_approval::ArcaAction;
use arca_incus::exec::{DashPort, ExecCommand, ServiceFile, TailnetNode};
use arca_kernel::desired::Entry;

pub(super) fn adopted(name: &str) -> CallError {
    CallError::Refused {
        code: "adopted".into(),
        message: format!("service {name} was adopted; this build does not write to it"),
    }
}
fn record(entries: &[Entry], name: &str) -> Result<(Entry, ServiceConfig), CallError> {
    service_entries(entries)
        .into_iter()
        .find(|(_, c)| c.name == name)
        .ok_or_else(|| service_not_found(format!("service {name}: not found")))
}
fn arg_name(args: &Value, key: &str) -> Result<String, CallError> {
    args[key]
        .as_str()
        .filter(|s| service::service_name_ok(s))
        .map(str::to_owned)
        .ok_or_else(|| CallError::Invalid(format!("{key}: expected a service name")))
}
fn change(config: &ServiceConfig) -> Result<crate::desired_store::EntryChange, CallError> {
    Ok(crate::desired_store::EntryChange::Put(service_entry(
        &format!("service-{}", config.name),
        "arca.service",
        serde_json::to_value(config).map_err(|_| internal())?,
    )))
}
fn failed(outcomes: &[Outcome]) -> bool {
    outcomes.iter().any(|o| {
        matches!(
            o,
            Outcome::Failed { .. } | Outcome::Refused(_) | Outcome::Held { .. }
        )
    })
}

impl ServiceHandler {
    pub(super) fn exec_target(
        &self,
        entry: &Entry,
        cfg: &ServiceConfig,
    ) -> Result<ExecTarget, CallError> {
        let kernel = lock(&self.kernel)?;
        Ok(ExecTarget {
            entry: entry.id.clone(),
            service: cfg.clone(),
            uuid: kernel.service_uuid(&entry.id, &cfg.instance),
            retained: kernel.retained_entries().map_err(|_| internal())?,
        })
    }
    fn put(&self, configs: &[ServiceConfig], now: u64) -> Result<(), CallError> {
        let changes = configs.iter().map(change).collect::<Result<Vec<_>, _>>()?;
        let outcomes = lock(&self.kernel)?
            .propose(changes, now)
            .map_err(|_| internal())?;
        if failed(&outcomes) {
            return Err(service_refused("service config or ACL update failed"));
        }
        Ok(())
    }
    fn template(&self, cfg: &ServiceConfig) -> Result<service::Template, CallError> {
        service::load_template(&self.cfg.state_dir.join("templates"), &cfg.template)
            .map_err(service_template_error)
    }
    fn live_owned(
        &self,
        entry: &Entry,
        cfg: &ServiceConfig,
    ) -> Result<arca_incus::Owned, CallError> {
        if cfg.project != arca_netproject::PROJECT || cfg.instance != cfg.name {
            return Err(service_refused("service target mismatch"));
        }
        let target = self.exec_target(entry, cfg)?;
        let uuid = target
            .uuid
            .clone()
            .ok_or_else(|| service_refused("service has no journalled UUID"))?;
        let writer = self
            .exec
            .writer(&target, Duration::from_secs(10))
            .map_err(|e| e.call_error())?;
        let object = ObjectRef {
            kind: arca_kernel::ownership::IncusKind::Instance,
            project: cfg.project.clone(),
            parent: None,
            name: cfg.instance.clone(),
            uuid: Some(uuid),
        };
        let owned = writer
            .read_owned(&object)
            .map_err(|_| service_refused("service ownership or UUID changed"))?;
        if owned.tombstone().is_some() {
            return Err(service_refused("service is retiring"));
        }
        Ok(owned)
    }
    fn secret_reference(&self, cfg: &ServiceConfig, key: &str) -> Option<String> {
        cfg.secret_refs.get(key).cloned().or_else(|| {
            service::secret_file(&self.cfg.state_dir.join("secrets"), &cfg.name, key)
                .filter(|f| f.is_file())
                .map(|_| service::secret_reference(&cfg.name, key))
        })
    }
    fn read_reference(&self, reference: &str) -> Result<String, CallError> {
        let name = reference
            .strip_prefix(arca_providers::incus::SECRET_PREFIX)
            .ok_or_else(internal)?;
        if name.split('/').any(|part| {
            part.is_empty()
                || !part
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        }) {
            return Err(internal());
        }
        let path = self.cfg.state_dir.join("secrets").join(name);
        let meta =
            std::fs::symlink_metadata(&path).map_err(|_| service_refused("secret unavailable"))?;
        use std::os::unix::fs::PermissionsExt;
        if !meta.is_file() || meta.permissions().mode() & 0o077 != 0 || meta.len() > 65536 {
            return Err(service_refused("secret unavailable"));
        }
        std::fs::read_to_string(path).map_err(|_| service_refused("secret unavailable"))
    }
    fn values(
        &self,
        cfg: &ServiceConfig,
        t: &service::Template,
    ) -> Result<BTreeMap<String, String>, CallError> {
        let mut values = BTreeMap::new();
        for key in &t.secrets {
            if let Some(reference) = self.secret_reference(cfg, key)
                && let Ok(value) = self.read_reference(&reference)
            {
                values.insert(key.clone(), value);
            }
        }
        Ok(values)
    }
    fn store_value(&self, name: &str, value: &str) -> Result<String, CallError> {
        let file = format!("v-{}", Digest::of(value.as_bytes()).to_hex());
        let path = self
            .cfg
            .state_dir
            .join("secrets")
            .join(format!("service-{name}"))
            .join(&file);
        if !path.exists() {
            service::write_secret(&path, value).map_err(service_template_error)?;
        }
        Ok(format!(
            "{}service-{name}/{file}",
            arca_providers::incus::SECRET_PREFIX
        ))
    }
    fn provided(
        &self,
        provider: &ServiceConfig,
        t: &service::Template,
        consumer: &str,
        port: u16,
    ) -> Result<BTreeMap<String, String>, CallError> {
        let values = self.values(provider, t)?;
        let mut env = BTreeMap::new();
        for (key, value) in t
            .provides
            .as_ref()
            .and_then(|v| v["env"].as_object())
            .into_iter()
            .flatten()
        {
            let value = value
                .as_str()
                .ok_or_else(|| service_refused("invalid provided variable"))?;
            let rendered = service::render(
                value,
                &provider.name,
                &provider.ipv4,
                &provider.gateway,
                Some(port),
                &values,
            )
            .ok_or_else(|| {
                service_refused("provider needs a secret: set it first with service_secret_set")
            })?;
            env.insert(key.clone(), self.store_value(consumer, &rendered)?);
        }
        Ok(env)
    }
    pub(super) fn call_wiring(
        &self,
        args: &Value,
        caller: &Caller,
        entries: &[Entry],
        now: u64,
    ) -> Result<Value, CallError> {
        if matches!(self.verb, "service_connect" | "service_disconnect") {
            return self.link(args, entries, now);
        }
        let name = service_name(args)?;
        let (entry, mut cfg) = record(entries, &name)?;
        if cfg.adopted {
            if matches!(self.verb, "service_publish" | "service_unpublish") {
                self.exec
                    .audit_refusal(&caller.token_id, self.verb, &entry.id.0, &name)
                    .map_err(|e| e.call_error())?;
            }
            return Err(adopted(&name));
        }
        if self.verb == "service_secret_set" {
            let key = args["key"]
                .as_str()
                .ok_or_else(|| CallError::Invalid("key: expected a secret name".into()))?;
            if caller.scope == Scope::Doctor
                && [
                    "OMNI_API_KEY",
                    "telegram_bot_token",
                    "whatsapp_phone_number",
                ]
                .contains(&key)
            {
                return Err(service_refused(format!(
                    "{key} is the owner's to set: a doctor token cannot set it. The owner sets it themselves, over cli.sock (the runbook's secret command)"
                )));
            }
            let t = self.template(&cfg)?;
            if !t.secrets.iter().any(|k| k == key) {
                return Err(service_refused("template does not declare that secret"));
            }
            let value = args["value"]
                .as_str()
                .filter(|s| {
                    (8..=4096).contains(&s.chars().count()) && !s.chars().any(char::is_control)
                })
                .ok_or_else(|| {
                    CallError::Invalid(
                        "a secret is 8 to 4096 characters without control characters".into(),
                    )
                })?;
            self.live_owned(&entry, &cfg)?;
            let reference = self.store_value(&name, value)?;
            cfg.secret_refs.insert(key.into(), reference);
            let mut values = BTreeMap::from([(key.to_owned(), value.to_owned())]);
            let affected = t
                .env
                .iter()
                .filter(|(_, pattern)| pattern.contains(&format!("{{{{secret:{key}}}}}")))
                .collect::<Vec<_>>();
            for other in &t.secrets {
                if other != key
                    && affected
                        .iter()
                        .any(|(_, pattern)| pattern.contains(&format!("{{{{secret:{other}}}}}")))
                    && let Some(reference) = self.secret_reference(&cfg, other)
                {
                    values.insert(other.clone(), self.read_reference(&reference)?);
                }
            }
            cfg.environment_patch = Some(affected.iter().map(|(env, _)| (*env).clone()).collect());
            for (env, pattern) in affected {
                if let Some(rendered) = service::render(
                    pattern,
                    &name,
                    &cfg.ipv4,
                    &cfg.gateway,
                    t.ports.first().copied(),
                    &values,
                ) {
                    cfg.environment
                        .insert(env.clone(), self.store_value(&name, &rendered)?);
                }
            }
            self.put(std::slice::from_ref(&cfg), now)?;
            for (consumer_entry, mut consumer) in service_entries(entries) {
                if consumer.adopted {
                    continue;
                }
                if let Some(link) = consumer.links.iter().find(|l| l.to == name).cloned() {
                    self.live_owned(&consumer_entry, &consumer)?;
                    let env = self.provided(&cfg, &t, &consumer.name, link.port)?;
                    consumer.environment_patch = Some(env.keys().cloned().collect());
                    consumer.environment.extend(env);
                    self.put(&[consumer], now)?;
                }
            }
            return Ok(json!({"ok":true}));
        }
        self.publication(args, caller, &entry, &mut cfg, now)
    }
    fn link(&self, args: &Value, entries: &[Entry], now: u64) -> Result<Value, CallError> {
        let from = arg_name(args, "from")?;
        let to = arg_name(args, "to")?;
        if from == to {
            return Err(service_refused("a service cannot link to itself"));
        }
        let (ce, mut consumer) = record(entries, &from)?;
        let (pe, mut provider) = record(entries, &to)?;
        if consumer.adopted {
            return Err(adopted(&from));
        }
        if provider.adopted {
            return Err(adopted(&to));
        }
        let co = self.live_owned(&ce, &consumer)?;
        let po = self.live_owned(&pe, &provider)?;
        let original_consumer = consumer.clone();
        let original_provider = provider.clone();
        if self.verb == "service_disconnect" {
            let names = consumer
                .links
                .iter()
                .filter(|l| l.to == to)
                .flat_map(|l| l.env.clone())
                .collect::<Vec<_>>();
            provider.incoming.retain(|g| g.consumer != from);
            provider.environment_patch = Some(BTreeSet::new());
            self.put(std::slice::from_ref(&provider), now)?;
            consumer.links.retain(|l| l.to != to);
            let template = self.template(&consumer)?;
            let values = self.values(&consumer, &template)?;
            let mut remaining = BTreeMap::new();
            for (key, pattern) in &template.env {
                if let Some(value) = service::render(
                    pattern,
                    &consumer.name,
                    &consumer.ipv4,
                    &consumer.gateway,
                    template.ports.first().copied(),
                    &values,
                ) {
                    remaining.insert(key.clone(), self.store_value(&consumer.name, &value)?);
                }
            }
            for link in &consumer.links {
                let (_, other) = record(entries, &link.to)?;
                remaining.extend(self.provided(
                    &other,
                    &self.template(&other)?,
                    &consumer.name,
                    link.port,
                )?);
            }
            consumer.environment_patch = Some(names.iter().cloned().collect());
            for key in &names {
                consumer.environment.remove(key);
                if let Some(value) = remaining.remove(key) {
                    consumer.environment.insert(key.clone(), value);
                }
            }
            self.put(&[consumer], now)?;
            return Ok(json!({"env":names}));
        }
        let t = self.template(&provider)?;
        let port = t
            .ports
            .first()
            .copied()
            .ok_or_else(|| service_refused("provider has no port"))?;
        let env = self.provided(&provider, &t, &from, port)?;
        let names = env.keys().cloned().collect::<Vec<_>>();
        consumer.links.retain(|l| l.to != to);
        consumer.links.push(ServiceLink {
            to: to.clone(),
            port,
            env: names.clone(),
            uuid: po.uuid().unwrap_or("").into(),
        });
        // The live link marker is the restoration guard. Environment is updated only after its ACL succeeds.
        consumer.environment_patch = Some(BTreeSet::new());
        self.put(std::slice::from_ref(&consumer), now)?;
        provider.environment_patch = Some(BTreeSet::new());
        provider.incoming.retain(|g| g.consumer != from);
        provider.incoming.push(ServiceGrant {
            consumer: from.clone(),
            address: consumer.ipv4.clone(),
            port,
            uuid: co.uuid().unwrap_or("").into(),
            provider_uuid: po.uuid().unwrap_or("").into(),
        });
        if let Err(error) = self.put(&[provider], now) {
            let _ = self.put(&[original_provider, original_consumer], now);
            return Err(error);
        }
        consumer.environment_patch = Some(env.keys().cloned().collect());
        consumer.environment.extend(env);
        self.put(&[consumer], now)?;
        Ok(json!({"env":names}))
    }
}

fn json_output(bytes: &[u8]) -> Result<Value, ExecError> {
    if bytes.len() > arca_incus::exec::OUTPUT_CAP {
        return Err(ExecError::Failed("service answer exceeds 64 KiB".into()));
    }
    serde_json::from_slice(bytes)
        .map_err(|_| ExecError::Failed("untrusted service answer is not JSON".into()))
}
fn logged_out(bytes: &[u8]) -> Result<bool, ExecError> {
    let value = json_output(bytes)?;
    match value["BackendState"].as_str() {
        Some("NeedsLogin" | "NoState") => Ok(true),
        Some(state) if state.len() <= 32 => Ok(false),
        _ => Err(ExecError::Failed(
            "untrusted tailnet status has no valid BackendState".into(),
        )),
    }
}
fn serve_has_port(bytes: &[u8]) -> Result<bool, ExecError> {
    let value = json_output(bytes)?;
    match value.get("TCP") {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Object(tcp)) => Ok(tcp.contains_key("8443")),
        _ => Err(ExecError::Failed(
            "untrusted serve status has invalid TCP".into(),
        )),
    }
}
fn valid_serving(value: &Value, fqdn: &str, port: u16) -> bool {
    let Some(tcp) = value["TCP"].as_object() else {
        return false;
    };
    let Some(web) = value["Web"].as_object() else {
        return false;
    };
    let address = format!("{fqdn}:8443");
    tcp.len() == 1
        && tcp.get("8443") == Some(&json!({"HTTPS":true}))
        && web.len() == 1
        && web.get(&address)
            == Some(&json!({"Handlers":{"/":{"Proxy":format!("http://127.0.0.1:{port}")}}}))
        && value
            .get("AllowFunnel")
            .is_none_or(|v| v.is_null() || v.as_object().is_some_and(|m| m.is_empty()))
        && value
            .get("Foreground")
            .is_none_or(|v| v.is_null() || v.as_object().is_some_and(|m| m.is_empty()))
}

impl ServiceHandler {
    fn publication(
        &self,
        args: &Value,
        caller: &Caller,
        entry: &Entry,
        cfg: &mut ServiceConfig,
        now: u64,
    ) -> Result<Value, CallError> {
        let target = self.exec_target(entry, cfg)?;
        let secrets = service_secret_values(&self.cfg.state_dir.join("secrets"));
        let run = |cmd: &ExecCommand| {
            self.exec
                .run(&target, &caller.token_id, self.verb, cmd, &secrets, None)
        };
        if self.verb == "service_unpublish" {
            self.take_off(&target, caller, None)
                .map_err(|e| e.call_error())?;
            let _write = self.writes.lock().map_err(|_| internal())?;
            let (_, mut latest) = record(lock(&self.kernel)?.entries(), &cfg.name)?;
            latest.published = None;
            latest.environment_patch = Some(BTreeSet::new());
            self.put(&[latest], now)?;
            return Ok(json!({"unpublished":true}));
        }
        let preflight = (|| {
            if args["reach"] != "tailnet" {
                return Err(service_refused(
                    "reach is \"tailnet\" only: a dash is published on the owner's tailnet and nowhere else (no public, funnel or LAN reach)",
                ));
            }
            let node = TailnetNode::new(&format!("arca-{}", cfg.name))
                .map_err(|_| service_refused("invalid publication node"))?;
            let host = self
                .cfg
                .serve_host
                .as_deref()
                .ok_or_else(|| service_refused("this process's own serve name is not known yet"))?
                .trim_end_matches('.')
                .to_ascii_lowercase();
            let tailnet = self
                .cfg
                .tailnet
                .as_deref()
                .filter(|s| {
                    !s.is_empty() && s.len() <= 190 && s.split('.').all(arca_incus::exec::node_ok)
                })
                .ok_or_else(|| service_refused("the owner's tailnet is not configured"))?;
            let suffix = format!(".{tailnet}.ts.net");
            let own = host
                .strip_suffix(&suffix)
                .filter(|s| arca_incus::exec::node_ok(s))
                .ok_or_else(|| {
                    service_refused(
                        "this process's serve name is not a name in the configured tailnet",
                    )
                })?;
            if own == node.as_str() {
                return Err(service_refused(
                    "the node would have this process's own serve name",
                ));
            }
            let t = self.template(cfg)?;
            if !t.publishable || t.kind != "system" {
                return Err(service_refused("template is not publishable"));
            }
            if cfg.published.is_some() {
                return Err(service_refused("service is already published"));
            }
            let key = self
                .secret_reference(cfg, "tailscale_authkey")
                .ok_or_else(|| service_refused("service needs its tailscale_authkey secret"))?;
            let key = self.read_reference(&key)?;
            let port = t
                .dash_port
                .filter(|p| *p != 0)
                .ok_or_else(|| service_refused("template needs a dash port"))?;
            Ok((node, tailnet.to_owned(), host, key, port))
        })();
        let (node, tailnet, host, key, port) = match preflight {
            Ok(value) => value,
            Err(error) => {
                self.exec
                    .audit_refusal(&caller.token_id, self.verb, &entry.id.0, &cfg.name)
                    .map_err(|e| e.call_error())?;
                return Err(error);
            }
        };
        // Version is a read. A stopped service and a missing timeout/tailscale binary spend no approval.
        run(&ExecCommand::TailscaleVersion).map_err(|e| e.call_error())?;
        let approval = args["approval"]
            .as_str()
            .ok_or_else(|| CallError::Invalid("approval: expected a string".into()))?;
        lock(&self.kernel)?
            .check_approval(
                ArcaAction::Publish,
                &cfg.name,
                &json!({"name":cfg.name,"reach":"tailnet"}),
                approval,
                now,
            )
            .map_err(|e| CallError::Refused {
                code: "owner_approval".into(),
                message: e.to_string(),
            })?;
        let fqdn = format!("{}.{tailnet}.ts.net", node.as_str());
        let result = (|| {
            run(&ExecCommand::StartDaemon)?;
            run(&ExecCommand::ClearKeyDir)?;
            run(&ExecCommand::MakeKeyDir)?;
            let writer = self.exec.writer(&target, Duration::from_secs(10))?;
            let owned = self.exec.check(&target, &writer)?;
            writer
                .write_service_file(&owned, &ServiceFile::TailnetKey, key.as_bytes())
                .map_err(|_| ExecError::Failed("key file write failed".into()))?;
            let joined = run(&ExecCommand::Join(node.clone()));
            let cleared = run(&ExecCommand::ClearKeyDir); // Always attempted, including when Join marks the instance hung.
            joined?;
            cleared?;
            let status = json_output(&run(&ExecCommand::TailnetStatus)?)?;
            let dns = status["Self"]["DNSName"]
                .as_str()
                .filter(|s| s.len() <= 253)
                .map(|s| s.strip_suffix('.').unwrap_or(s).to_ascii_lowercase());
            if status["BackendState"] != "Running"
                || dns.as_deref() != Some(&fqdn)
                || dns.as_deref() == Some(&host)
                || status["Self"]["Tags"] != json!(["tag:arca-svc"])
            {
                return Err(ExecError::Failed("joined node is not the requested node in the owner's tailnet with exactly tag:arca-svc".into()));
            }
            run(&ExecCommand::ServeOn(
                DashPort::new(port).map_err(|_| ExecError::Refused("port"))?,
            ))?;
            if !valid_serving(&json_output(&run(&ExecCommand::ServeStatus)?)?, &fqdn, port) {
                return Err(ExecError::Failed(
                    "untrusted serve config serves more than the requested HTTPS dash on 8443"
                        .into(),
                ));
            }
            Ok(())
        })();
        if let Err(error) = result {
            let _ = run(&ExecCommand::ServeOff);
            let logout = run(&ExecCommand::Logout).is_ok();
            let _ = run(&ExecCommand::ClearKeyDir);
            let _ = run(&ExecCommand::StopDaemon);
            let detail = match error {
                ExecError::Failed(text) => text,
                ExecError::Refused(code) => code.into(),
                ExecError::Audit => "audit unavailable".into(),
            };
            return Err(service_refused(format!(
                "{}; the approval is spent; logout completed: {logout}; may still be in the tailnet: service_unpublish takes it off",
                crate::service_exec::failure_text(&detail, &[key])
            )));
        }
        cfg.published = Some(Published {
            node: node.as_str().into(),
            port: 8443,
            reach: "tailnet".into(),
        });
        let _write = self.writes.lock().map_err(|_| internal())?;
        let (_, mut latest) = record(lock(&self.kernel)?.entries(), &cfg.name)?;
        latest.published = cfg.published.clone();
        latest.environment_patch = Some(BTreeSet::new());
        let recorded = self.put(&[latest], now);
        drop(_write);
        if recorded.is_err() {
            for cmd in [
                ExecCommand::ServeOff,
                ExecCommand::Logout,
                ExecCommand::ClearKeyDir,
                ExecCommand::StopDaemon,
            ] {
                let _ = run(&cmd);
            }
            return Err(service_refused(
                "publication record failed; the approval is spent; service_unpublish takes it off",
            ));
        }
        Ok(json!({"node":node.as_str(),"url":format!("https://{fqdn}:8443/")}))
    }
    fn take_off(
        &self,
        target: &ExecTarget,
        caller: &Caller,
        until: Option<Instant>,
    ) -> Result<(), ExecError> {
        let secrets = service_secret_values(&self.cfg.state_dir.join("secrets"));
        let run = |cmd: &ExecCommand| {
            self.exec
                .run(target, &caller.token_id, self.verb, cmd, &secrets, until)
        };
        run(&ExecCommand::StartDaemon)?;
        if serve_has_port(&run(&ExecCommand::ServeStatus)?)? {
            run(&ExecCommand::ServeOff)?;
        }
        if !logged_out(&run(&ExecCommand::TailnetStatus)?)? {
            run(&ExecCommand::Logout)
                .map_err(|_| ExecError::Failed("logout failed; node is still published".into()))?;
        }
        if !logged_out(&run(&ExecCommand::TailnetStatus)?)? {
            return Err(ExecError::Failed(
                "node is still published after logout".into(),
            ));
        }
        for cmd in [ExecCommand::ClearKeyDir, ExecCommand::StopDaemon] {
            if run(&cmd).is_err() {
                eprintln!("arca: service logout cleanup failed");
            }
        }
        Ok(())
    }
    pub(super) fn logout_before_remove(&self, entry: &Entry, cfg: &ServiceConfig, caller: &Caller) {
        if cfg.adopted || cfg.source["protocol"] == "oci" {
            return;
        }
        if cfg.published.is_none() && !self.template(cfg).is_ok_and(|t| t.publishable) {
            return;
        }
        let Ok(target) = self.exec_target(entry, cfg) else {
            return;
        };
        let until = Instant::now() + Duration::from_secs(60);
        if self.take_off(&target, caller, Some(until)).is_err() {
            eprintln!("arca: service logout before removal failed; deletion continues");
        }
    }
}
