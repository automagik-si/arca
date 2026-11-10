//! The Omni verbs over the service model: each names a service Arca created from the `omni-api` template and reaches its
//! Omni through `crate::omni`. They read the service record and its secrets; they change no desired state, write nothing
//! to Incus and run no process. Every result is untrusted (Omni and the people on its channels wrote it), and every
//! refusal that quotes Omni says so.
use super::service_wiring::{read_reference_in, secret_reference_in};
use super::*;
use crate::omni::{self, OmniOptions, Route};
use crate::service::ServiceConfig;

pub(super) struct OmniHandler {
    pub(super) verb: &'static str,
    pub(super) kernel: Arc<Mutex<Kernel>>,
    pub(super) clock: Clock,
    pub(super) cfg: Arc<NodeConfig>,
    pub(super) options: OmniOptions,
    /// The instances a pairing attempt reached Omni for, by service and instance id, and until when (epoch ms) that
    /// holds the next one off. Shared by the three handlers.
    pub(super) pairing: Arc<Mutex<HashMap<(String, String), u64>>>,
}

/// An omni-api service as the verbs reach it: its record and the port its template names.
struct Reach {
    cfg: ServiceConfig,
    ipv4: std::net::Ipv4Addr,
    port: u16,
}

fn untrusted(code: &str, message: String) -> CallError {
    CallError::UntrustedRefused {
        code: code.into(),
        message,
    }
}

fn object_of(value: &Value) -> Option<&serde_json::Map<String, Value>> {
    value.as_object()
}

impl OmniHandler {
    /// Service `name`, Arca's own and of the `omni-api` template, and the port Arca calls it at. Its state is not
    /// checked: a service that is not up answers nothing anyway.
    fn service(&self, name: &str) -> Result<Reach, CallError> {
        let entries = lock(&self.kernel)?.entries().to_vec();
        let (_, cfg) = service_entries(&entries)
            .into_iter()
            .find(|(_, config)| config.name == name)
            .ok_or_else(|| service_not_found(format!("service {name}: not found")))?;
        if cfg.adopted {
            return Err(service_wiring::adopted(name));
        }
        if cfg.template != omni::OMNI_TEMPLATE {
            return Err(service_refused(format!(
                "{name} is a {} service, not {}: the omni tools reach Omni's API alone",
                cfg.template,
                omni::OMNI_TEMPLATE
            )));
        }
        let template = crate::service::load_template(
            &self.cfg.state_dir.join("templates"),
            omni::OMNI_TEMPLATE,
        )
        .map_err(service_template_error)?;
        let port = template.droidd_port.ok_or_else(|| {
            service_refused(format!(
                "template {} names no droiddPort, so Arca has no reach to its API",
                omni::OMNI_TEMPLATE
            ))
        })?;
        let ipv4 = cfg.ipv4.parse().map_err(|_| internal())?;
        Ok(Reach { cfg, ipv4, port })
    }

    /// Secret `key` of the service; `refused`, naming the slot and who sets it, while it is unset.
    fn secret(&self, reach: &Reach, key: &str) -> Result<String, CallError> {
        if let Some(reference) = secret_reference_in(&self.cfg, &reach.cfg, key) {
            return read_reference_in(&self.cfg, &reference);
        }
        let name = &reach.cfg.name;
        if key == omni::API_KEY {
            return Err(service_refused(format!(
                "{name} has no {key} secret: Arca makes it at service_create (a service whose secrets were lost has lost it, and Omni keeps the key it first started with)"
            )));
        }
        Err(service_refused(format!(
            "{name} has no {key} secret yet: the owner sets it with service_secret_set (name {name}, key {key}); Arca keeps it and no tool shows it"
        )))
    }

    /// What `omni::redact` takes out of Omni's text for the service: its three secrets, and the token's part after its
    /// colon, longest first.
    fn secrets_of(&self, reach: &Reach) -> Vec<String> {
        let mut out = Vec::new();
        for key in [omni::API_KEY, omni::TELEGRAM_TOKEN, omni::PHONE] {
            let Some(value) = secret_reference_in(&self.cfg, &reach.cfg, key)
                .and_then(|reference| read_reference_in(&self.cfg, &reference).ok())
            else {
                continue;
            };
            if key == omni::TELEGRAM_TOKEN
                && let Some((_, after)) = value.split_once(':')
            {
                out.push(after.to_string());
            }
            out.push(value);
        }
        out.retain(|value| value.chars().count() >= 8);
        out.sort_by_key(|value| std::cmp::Reverse(value.len()));
        out
    }

    /// One request to the service's Omni: the answer of a 2xx as a JSON object, or a refusal that names the request and
    /// its status and quotes Omni only redacted. `reached` runs once the request may have reached Omni.
    fn request(
        &self,
        reach: &Reach,
        route: Route<'_>,
        body: Option<Value>,
        reached: &mut dyn FnMut(),
    ) -> Result<serde_json::Map<String, Value>, CallError> {
        let key = self.secret(reach, omni::API_KEY)?;
        if let Route::Status(id) | Route::Pair(id) = route
            && !omni::is_uuid(id)
        {
            return Err(CallError::Invalid(
                "instance: an Omni instance id is a UUID".into(),
            ));
        }
        let name = &reach.cfg.name;
        let addr = match &self.options.base {
            Some(base) => base(name, reach.ipv4, reach.port),
            None => std::net::SocketAddr::from((reach.ipv4, reach.port)),
        };
        let what = route.what();
        let secrets = self.secrets_of(reach);
        let body = body.map(|body| body.to_string().into_bytes());
        let answer = match omni::request(
            addr,
            &route,
            &key,
            body.as_deref(),
            self.options.timeout,
            self.options.body_max,
        ) {
            Ok(answer) => {
                reached();
                answer
            }
            Err(failure) => {
                if failure.reached() {
                    reached();
                }
                let why = match failure {
                    omni::HttpFailure::TooLarge => {
                        return Err(service_refused_code(
                            "too_large",
                            format!("Omni answered more than {} bytes", self.options.body_max),
                        ));
                    }
                    omni::HttpFailure::Timeout => "no answer in time",
                    omni::HttpFailure::Unreachable(code) | omni::HttpFailure::Broken(code) => code,
                    omni::HttpFailure::Malformed => "not HTTP",
                };
                return Err(service_refused(format!(
                    "{name} does not answer {what} at {}:{} ({why}): is it running, and linked to postgres-autopg and nats?",
                    reach.ipv4, reach.port
                )));
            }
        };
        let status = answer.status;
        if (300..400).contains(&status) {
            return Err(service_refused(format!(
                "{name} answered {what} with a redirect ({status}); Arca follows none"
            )));
        }
        let Ok(json) = serde_json::from_slice::<Value>(&answer.body) else {
            return Err(service_refused(format!(
                "{name} answered {what} with {status} and no JSON"
            )));
        };
        if (200..300).contains(&status)
            && let Value::Object(map) = json
        {
            return Ok(map);
        }
        let error = json.get("error").and_then(object_of);
        let field = |key: &str, max: usize| {
            error
                .and_then(|e| e.get(key))
                .and_then(Value::as_str)
                .map(|text| omni::redact(text, &secrets, max))
                .unwrap_or_default()
        };
        let code = field("code", 40);
        let message = field("message", 300);
        let said = format!(
            "{code}{}{message}",
            if !code.is_empty() && !message.is_empty() {
                ": "
            } else {
                ""
            }
        );
        Err(match status {
            401 | 403 => untrusted(
                "refused",
                format!(
                    "{name} refused Arca's {} on {what} ({status}{}). Omni keeps the key it first started with a database: one set after that is not its key",
                    omni::API_KEY,
                    if said.is_empty() {
                        String::new()
                    } else {
                        format!(", {said}")
                    }
                ),
            ),
            404 => untrusted(
                "not_found",
                format!(
                    "{name} answered {what} with 404{}: no such instance",
                    if said.is_empty() {
                        String::new()
                    } else {
                        format!(" ({said})")
                    }
                ),
            ),
            _ => untrusted(
                "refused",
                format!(
                    "{name} answered {what} with {status}{}",
                    if said.is_empty() {
                        String::new()
                    } else {
                        format!(": {said}")
                    }
                ),
            ),
        })
    }

    /// An instance from Omni's answer, its allowlisted fields alone; a refusal for one that is not an instance.
    fn instance_of(
        &self,
        reach: &Reach,
        value: &Value,
        secrets: &[String],
    ) -> Result<Value, CallError> {
        let refused = || {
            service_refused(format!(
                "{} answered with something that is not an Omni instance",
                reach.cfg.name
            ))
        };
        let object = value.as_object().ok_or_else(refused)?;
        let id = object
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| omni::is_uuid(id))
            .ok_or_else(refused)?;
        let name = object
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(refused)?;
        let channel = object
            .get("channel")
            .and_then(Value::as_str)
            .ok_or_else(refused)?;
        Ok(json!({
            "id": id,
            "name": omni::redact(name, secrets, 64),
            "channel": omni::redact(channel, secrets, 32),
            "isActive": object.get("isActive") == Some(&Value::Bool(true)),
        }))
    }

    fn list(&self, reach: &Reach) -> Result<Value, CallError> {
        let body = self.request(reach, Route::List, None, &mut || {})?;
        let secrets = self.secrets_of(reach);
        let items = body.get("items").and_then(Value::as_array).ok_or_else(|| {
            service_refused(format!(
                "{} answered its instance list without items",
                reach.cfg.name
            ))
        })?;
        let instances = items
            .iter()
            .take(omni::LIST_MAX)
            .map(|item| self.instance_of(reach, item, &secrets))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(json!({ "instances": instances }))
    }

    /// The id of `instance` (an id, or a name looked up in the list).
    fn resolve(&self, reach: &Reach, instance: &str) -> Result<String, CallError> {
        if omni::is_uuid(instance) {
            return Ok(instance.to_string());
        }
        let name = &reach.cfg.name;
        let listed = self.list(reach)?;
        let found: Vec<&str> = listed["instances"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|i| i["name"].as_str() == Some(instance))
            .filter_map(|i| i["id"].as_str())
            .collect();
        match found.as_slice() {
            [id] => Ok((*id).to_string()),
            [] => Err(service_not_found(format!(
                "{name} has no instance(s) named {instance} (omni_status lists them)"
            ))),
            many => Err(service_refused_code(
                "ambiguous",
                format!(
                    "{name} has {} instance(s) named {instance}: name it by its id",
                    many.len()
                ),
            )),
        }
    }

    fn create(&self, name: &str, args: &Value) -> Result<Value, CallError> {
        let instance = args["instance"].as_str().unwrap_or_default();
        if !omni::instance_name_ok(instance) {
            return Err(CallError::Invalid(
                "instance: an Omni instance name matches ^[a-z0-9][a-z0-9_-]{0,62}$".into(),
            ));
        }
        let channel = args["channel"].as_str().unwrap_or_default();
        if !omni::CHANNELS.contains(&channel) {
            return Err(CallError::Invalid(
                "channel: whatsapp-baileys or telegram".into(),
            ));
        }
        let reach = self.service(name)?;
        let mut body = json!({ "name": instance, "channel": channel });
        if channel == "telegram" {
            body["telegramBotToken"] = json!(self.secret(&reach, omni::TELEGRAM_TOKEN)?);
        }
        let answer = self.request(&reach, Route::Create, Some(body), &mut || {})?;
        let secrets = self.secrets_of(&reach);
        self.instance_of(&reach, answer.get("data").unwrap_or(&Value::Null), &secrets)
    }

    fn pair(&self, name: &str, args: &Value) -> Result<Value, CallError> {
        let reach = self.service(name)?;
        let phone = self.secret(&reach, omni::PHONE)?;
        if !omni::phone_ok(&phone) {
            return Err(service_refused(format!(
                "{name}'s {} secret is not a number in international form (+ and 10-15 digits, no spaces): the owner sets it again with service_secret_set",
                omni::PHONE
            )));
        }
        let instance = instance_arg(args)?;
        let id = self.resolve(&reach, instance)?;
        let slot = (name.to_string(), id.clone());
        let now = (self.clock)();
        {
            let mut pairing = self.pairing.lock().map_err(|_| internal())?;
            pairing.retain(|_, until| *until > now);
            if let Some(until) = pairing.get(&slot).copied() {
                return Err(service_refused(format!(
                    "a pairing code for {id} was issued and lasts {} s more: enter that one on the phone; never ask for codes in a loop",
                    (until - now).div_ceil(1000)
                )));
            }
            // Held while this attempt runs, so a concurrent one is refused too.
            let running = u64::try_from(self.options.timeout.as_millis()).unwrap_or(u64::MAX);
            pairing.insert(
                slot.clone(),
                now.saturating_add(running)
                    .saturating_add(omni::PAIR_COOLDOWN_S * 1000),
            );
        }
        // The cooldown starts with every attempt that reaches Omni, whatever it answers: WhatsApp counts the attempts,
        // not the codes Arca got. One that reached nothing (Omni down) holds nothing off.
        let mut reached = false;
        let answer = self.request(
            &reach,
            Route::Pair(&id),
            Some(json!({ "phoneNumber": phone })),
            &mut || reached = true,
        );
        let hold = |until: Option<u64>| -> Result<(), CallError> {
            let mut pairing = self.pairing.lock().map_err(|_| internal())?;
            match until {
                Some(until) => pairing.insert(slot.clone(), until),
                None => pairing.remove(&slot),
            };
            Ok(())
        };
        let answer = match answer {
            Ok(answer) => answer,
            Err(error) => {
                let now = (self.clock)();
                hold(reached.then_some(now + omni::PAIR_COOLDOWN_S * 1000))?;
                return Err(error);
            }
        };
        let data = answer.get("data").and_then(object_of);
        let code = data
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let expires_in = data
            .and_then(|d| d.get("expiresIn"))
            .and_then(Value::as_u64)
            .filter(|s| (1..=3600).contains(s))
            .unwrap_or(60);
        let now = (self.clock)();
        if !omni::pair_code_ok(code) {
            hold(Some(now + omni::PAIR_COOLDOWN_S * 1000))?;
            return Err(service_refused(format!(
                "{name} answered the pairing without a pairing code"
            )));
        }
        hold(Some(now + expires_in * 1000))?;
        Ok(json!({
            "instance": id,
            "code": code,
            "expiresIn": expires_in,
            "phoneNumber": omni::mask_phone(&phone),
        }))
    }

    fn status(&self, name: &str, args: &Value) -> Result<Value, CallError> {
        let Some(instance) = args.get("instance") else {
            let reach = self.service(name)?;
            return self.list(&reach);
        };
        let instance = instance
            .as_str()
            .ok_or_else(|| CallError::Invalid("instance: expected a string".into()))?;
        // An instance that is neither an id nor a name is refused before anything is read.
        instance_arg(&json!({ "instance": instance }))?;
        let reach = self.service(name)?;
        let id = self.resolve(&reach, instance)?;
        let answer = self.request(&reach, Route::Status(&id), None, &mut || {})?;
        let secrets = self.secrets_of(&reach);
        let empty = serde_json::Map::new();
        let data = answer.get("data").and_then(object_of).unwrap_or(&empty);
        let text = |key: &str, max: usize| match data.get(key).and_then(Value::as_str) {
            Some(text) if !text.is_empty() => json!(omni::redact(text, &secrets, max)),
            _ => Value::Null,
        };
        let state = match text("state", 32) {
            Value::String(state) => state,
            _ => "unknown".to_string(),
        };
        Ok(json!({
            "instance": id,
            "state": state,
            "isConnected": data.get("isConnected") == Some(&Value::Bool(true)),
            "profileName": text("profileName", 100),
            "message": text("message", 300),
        }))
    }
}

fn service_refused_code(code: &str, message: impl Into<String>) -> CallError {
    CallError::Refused {
        code: code.into(),
        message: message.into(),
    }
}

/// `instance`: an Omni instance id (a UUID) or a name, never a path of its own.
fn instance_arg(args: &Value) -> Result<&str, CallError> {
    args["instance"]
        .as_str()
        .filter(|i| omni::is_uuid(i) || omni::instance_name_ok(i))
        .ok_or_else(|| {
            CallError::Invalid(
                "instance: an Omni instance id (a UUID) or a name matching ^[a-z0-9][a-z0-9_-]{0,62}$"
                    .into(),
            )
        })
}

impl ToolHandler for OmniHandler {
    fn call(&self, args: Value, caller: &Caller) -> Result<Value, CallError> {
        let allowed: &[&str] = match self.verb {
            "omni_instance_create" => &["name", "instance", "channel"],
            _ => &["name", "instance"],
        };
        let object = args
            .as_object()
            .ok_or_else(|| CallError::Invalid("arguments: expected an object".into()))?;
        if object.keys().any(|key| !allowed.contains(&key.as_str())) {
            return Err(CallError::Invalid(
                "arguments: a field the schema does not name".into(),
            ));
        }
        if caller.scope < Scope::Doctor {
            return Err(CallError::Forbidden {
                verb: self.verb.into(),
            });
        }
        service_project(&self.cfg)?;
        let name = service_name(&args)?;
        match self.verb {
            "omni_instance_create" => self.create(&name, &args),
            "omni_pair" => self.pair(&name, &args),
            "omni_status" => self.status(&name, &args),
            _ => Err(CallError::NotFound),
        }
    }
}
