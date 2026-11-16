//! Approved, idempotent additions to one tenant's image store. No record and no delete.
use super::*;
use arca_incus::image::{ImageWriter, SOURCE_PROJECT, alias_ok, fingerprint_ok};
use arca_kernel::{
    desired::Entry,
    journal::{EffectClass, EffectState},
    ownership::{IncusKind, KERNEL_ENTRY_KEY, tenant_entry},
};
use serde::{Deserialize, Serialize};

type Properties = BTreeMap<String, String>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum ImageState {
    Absent,
    SameProperties,
    DifferentProperties,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Destination {
    kind: ImageState,
    properties: Option<Properties>,
    alias_target: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Plan {
    tenant: String,
    project: String,
    fingerprint: String,
    alias: String,
    tenant_incarnation: String,
    properties: Properties,
    destination: Destination,
}

#[derive(Clone, Copy, Default)]
struct Writes {
    image: bool,
    alias: bool,
}

fn image_properties(
    writer: &ImageWriter,
    fingerprint: &str,
) -> Result<Option<Properties>, arca_incus::IncusError> {
    writer
        .image()?
        .map(|image| {
            if image["fingerprint"] != fingerprint {
                return Err(arca_incus::IncusError::Decode(
                    "image identity changed".into(),
                ));
            }
            serde_json::from_value(image.get("properties").cloned().unwrap_or(json!({})))
                .map_err(|_| arca_incus::IncusError::Decode("invalid image properties".into()))
        })
        .transpose()
}

fn alias_target(writer: &ImageWriter) -> Result<Option<String>, arca_incus::IncusError> {
    writer
        .alias()?
        .map(|alias| {
            alias["target"]
                .as_str()
                .filter(|s| fingerprint_ok(s))
                .map(str::to_string)
                .ok_or_else(|| arca_incus::IncusError::Decode("invalid alias target".into()))
        })
        .transpose()
}

fn image_kind(properties: &Option<Properties>, approved: &Properties) -> ImageState {
    match properties {
        None => ImageState::Absent,
        Some(found) if found == approved => ImageState::SameProperties,
        Some(_) => ImageState::DifferentProperties,
    }
}

fn destination(
    writer: &ImageWriter,
    fingerprint: &str,
    approved: &Properties,
) -> Result<Destination, arca_incus::IncusError> {
    let properties = image_properties(writer, fingerprint)?;
    Ok(Destination {
        kind: image_kind(&properties, approved),
        properties,
        alias_target: alias_target(writer)?,
    })
}

// Error data is a closed snapshot, including unknown observations when Incus cannot be read.
fn fault(
    mut code: &str,
    writer: Option<&ImageWriter>,
    fingerprint: &str,
    writes: Writes,
) -> CallError {
    let mut data = json!({ "imageState": "unknown", "aliasState": "unknown", "imageWrite": writes.image, "aliasWrite": writes.alias });
    if fingerprint_ok(fingerprint) {
        data["fingerprint"] = json!(fingerprint);
    }
    if let Some(writer) = writer {
        match image_properties(writer, fingerprint) {
            Ok(None) => data["imageState"] = json!("absent"),
            Ok(Some(properties)) => {
                data["imageState"] = json!("present");
                data["properties"] = json!(properties);
            }
            Err(_) => {}
        }
        match alias_target(writer) {
            Ok(None) => data["aliasState"] = json!("absent"),
            Ok(Some(target)) => {
                data["aliasState"] = json!(if target == fingerprint {
                    "matching"
                } else {
                    "taken"
                });
                data["aliasTarget"] = json!(target);
            }
            Err(_) => {}
        }
    }
    if code == "ImportFailed" {
        if writes.alias && data["aliasState"] == "taken" {
            code = "AliasTaken";
        } else if (writes.image || writes.alias) && data["imageState"] == "present" {
            code = "PartialImport";
        }
    }
    let message = match code {
        "StaleCard" => {
            "The source, tenant or destination no longer matches the card. Make a new proposal."
        }
        "SourceMissing" => "The node's own store does not have this fingerprint.",
        "AliasTaken" => {
            "The alias names another fingerprint and was not replaced. Any copied image stays in the tenant's store."
        }
        "PartialImport" => {
            "The image is in the tenant's store, but a later step did not complete. Nothing was deleted; propose again to complete it."
        }
        _ => {
            "The import did not complete. The data describes what could be read; nothing was deleted."
        }
    };
    CallError::RefusedWith {
        code: code.into(),
        message: message.into(),
        fields: data.as_object().unwrap().clone(),
    }
}

fn incarnation(cfg: &NodeConfig, scope: &tenants::TenantScope) -> Result<String, ()> {
    let (records, torn) = read_journal(&cfg.state_dir.join(JOURNAL_FILE)).map_err(|_| ())?;
    if torn.is_some() {
        return Err(());
    }
    let row = records
        .iter()
        .filter(|row| {
            row.entry.0 == tenant_entry(&scope.id)
                && row.class == EffectClass::B
                && row.target.kind == IncusKind::Project
                && row.target.name == scope.project
                && row.pre_image.is_none()
        })
        .max_by_key(|row| row.id)
        .ok_or(())?;
    if !matches!(row.state, EffectState::Applied | EffectState::Confirmed) || row.intent.is_some() {
        return Err(());
    }
    Ok(row.id.to_string())
}

fn current(cfg: &NodeConfig, entries: &[Entry], args: &Value) -> Result<Plan, CallError> {
    let fingerprint = args["fingerprint"]
        .as_str()
        .filter(|s| fingerprint_ok(s))
        .ok_or_else(|| {
            CallError::Invalid("fingerprint: expected 64 lowercase hex digits".into())
        })?;
    let alias = args["alias"]
        .as_str()
        .filter(|s| alias_ok(s))
        .ok_or_else(|| {
            CallError::Invalid(
                "alias: expected 1-128 letters, digits, dots, underscores or dashes".into(),
            )
        })?;
    let scope = tenants::tenant_arg(cfg, args, entries)
        .map_err(|_| fault("StaleCard", None, fingerprint, Writes::default()))?
        .ok_or_else(tenants::no_tenants)?;
    let writer = ImageWriter::new(cfg.incus.socket.clone(), &scope.project, fingerprint, alias)
        .map_err(|_| fault("ImportFailed", None, fingerprint, Writes::default()))?;
    let unavailable = || {
        fault(
            "ImportFailed",
            Some(&writer),
            fingerprint,
            Writes::default(),
        )
    };
    let read = UnixIncus::new(cfg.incus.socket.clone());
    for project in [SOURCE_PROJECT, scope.project.as_str()] {
        let state = read
            .get(&format!("/1.0/projects/{project}"))
            .map_err(|_| unavailable())?;
        if state["config"]["features.images"] != "true"
            || (project != SOURCE_PROJECT
                && state["config"][KERNEL_ENTRY_KEY].as_str()
                    != Some(tenant_entry(&scope.id).as_str()))
        {
            return Err(fault(
                "StaleCard",
                Some(&writer),
                fingerprint,
                Writes::default(),
            ));
        }
    }
    let source = read
        .get(&format!(
            "/1.0/images/{fingerprint}?project={SOURCE_PROJECT}"
        ))
        .map_err(|error| match error {
            arca_incus::IncusError::Status { code: 404, .. } => fault(
                "SourceMissing",
                Some(&writer),
                fingerprint,
                Writes::default(),
            ),
            _ => unavailable(),
        })?;
    if source["fingerprint"] != fingerprint {
        return Err(unavailable());
    }
    let properties = serde_json::from_value(source.get("properties").cloned().unwrap_or(json!({})))
        .map_err(|_| unavailable())?;
    let tenant_incarnation = incarnation(cfg, &scope).map_err(|_| unavailable())?;
    // These are the last reads before a write: Incus has no create-only image copy.
    let destination = destination(&writer, fingerprint, &properties).map_err(|_| unavailable())?;
    Ok(Plan {
        tenant: scope.id,
        project: scope.project,
        fingerprint: fingerprint.into(),
        alias: alias.into(),
        tenant_incarnation,
        properties,
        destination,
    })
}

pub(super) fn enrich(
    cfg: &NodeConfig,
    entries: &[Entry],
    args: &Value,
) -> Result<Value, CallError> {
    let mut args = args.clone();
    for key in ["project", "tenantIncarnation", "properties", "destination"] {
        args.as_object_mut()
            .ok_or_else(|| CallError::Invalid("args: expected an object".into()))?
            .remove(key);
    }
    ActionKind::TenantImageImport
        .prepare(&args, None)
        .map_err(CallError::Invalid)?;
    let plan = current(cfg, entries, &args)?;
    let writer = ImageWriter::new(
        cfg.incus.socket.clone(),
        &plan.project,
        &plan.fingerprint,
        &plan.alias,
    )
    .map_err(|_| fault("ImportFailed", None, &plan.fingerprint, Writes::default()))?;
    if plan
        .destination
        .alias_target
        .as_ref()
        .is_some_and(|target| target != &plan.fingerprint)
    {
        return Err(fault(
            "AliasTaken",
            Some(&writer),
            &plan.fingerprint,
            Writes::default(),
        ));
    }
    serde_json::to_value(plan).map_err(|_| internal())
}

pub(super) struct Handler {
    pub kernel: Arc<Mutex<Kernel>>,
    pub cfg: Arc<NodeConfig>,
    pub writes: Arc<Mutex<()>>,
}
impl ToolHandler for Handler {
    fn call(&self, args: Value, caller: &Caller) -> Result<Value, CallError> {
        if caller.scope < Scope::Doctor {
            return Err(CallError::Forbidden {
                verb: "tenant_image_import".into(),
            });
        }
        arca_api::conforms(
            &(arca_api::verb("tenant_image_import").unwrap().params)(),
            &args,
        )
        .map_err(CallError::Invalid)?;
        if node_only_looks(&self.cfg) {
            return Err(fault(
                "ImportFailed",
                None,
                args["fingerprint"].as_str().unwrap_or(""),
                Writes::default(),
            ));
        }
        let approved = args["approval"].as_str().is_some_and(|grant| {
            lock(&self.kernel)
                .map(|mut kernel| kernel.consume_action_grant("tenant_image_import", &args, grant))
                .unwrap_or(false)
        });
        if !approved {
            return Err(import_not_approved());
        }
        let result = (|| {
            let _write = self.writes.lock().map_err(|_| internal())?;
            let entries = lock(&self.kernel)?.entries().to_vec();
            let mut bare = args.clone();
            bare.as_object_mut().unwrap().remove("approval");
            let plan: Plan = serde_json::from_value(bare).map_err(|_| {
                fault(
                    "StaleCard",
                    None,
                    args["fingerprint"].as_str().unwrap_or(""),
                    Writes::default(),
                )
            })?;
            let writer = ImageWriter::new(
                self.cfg.incus.socket.clone(),
                &plan.project,
                &plan.fingerprint,
                &plan.alias,
            )
            .map_err(|_| fault("StaleCard", None, &plan.fingerprint, Writes::default()))?;
            self.run(plan, &entries, &writer)
        })();
        result.map_err(|mut error| {
            if let CallError::RefusedWith { fields, .. } = &mut error {
                fields.insert("proposeAgain".into(), json!(true));
            }
            error
        })
    }
}

impl Handler {
    fn recheck(
        &self,
        expected: &Plan,
        entries: &[Entry],
        writer: &ImageWriter,
        writes: Writes,
    ) -> Result<(), CallError> {
        let args = serde_json::to_value(expected).map_err(|_| internal())?;
        let actual = current(&self.cfg, entries, &args).map_err(|error| {
            let code = match error.refusal().map(|(code, _)| code) {
                Some("SourceMissing" | "StaleCard") => "StaleCard",
                _ => "ImportFailed",
            };
            fault(code, Some(writer), &expected.fingerprint, writes)
        })?;
        if &actual != expected {
            let code = if writes.image
                && actual
                    .destination
                    .alias_target
                    .as_ref()
                    .is_some_and(|target| target != &expected.fingerprint)
            {
                "AliasTaken"
            } else {
                "StaleCard"
            };
            return Err(fault(code, Some(writer), &expected.fingerprint, writes));
        }
        Ok(())
    }

    fn run(
        &self,
        mut plan: Plan,
        entries: &[Entry],
        writer: &ImageWriter,
    ) -> Result<Value, CallError> {
        let mut writes = Writes::default();
        let failed = |writes| fault("ImportFailed", Some(writer), &plan.fingerprint, writes);
        self.recheck(&plan, entries, writer, writes)?;
        let copied = plan.destination.properties.is_none();
        if copied {
            writes.image = true;
            finish(writer, writer.copy(&json!(plan.properties))).map_err(|_| failed(writes))?;
            plan.destination.properties =
                image_properties(writer, &plan.fingerprint).map_err(|_| failed(writes))?;
            if plan.destination.properties.is_none() {
                return Err(failed(writes));
            }
            plan.destination.kind = image_kind(&plan.destination.properties, &plan.properties);
        }
        if plan.destination.properties.as_ref() != Some(&plan.properties) {
            self.recheck(&plan, entries, writer, writes)?;
            writes.image = true;
            finish(writer, writer.set_properties(&json!(plan.properties)))
                .map_err(|_| failed(writes))?;
            if image_properties(writer, &plan.fingerprint)
                .ok()
                .flatten()
                .as_ref()
                != Some(&plan.properties)
            {
                return Err(failed(writes));
            }
            plan.destination.properties = Some(plan.properties.clone());
            plan.destination.kind = ImageState::SameProperties;
        }
        if plan.destination.alias_target.is_none() {
            self.recheck(&plan, entries, writer, writes)?;
            writes.alias = true;
            finish(writer, writer.add_alias()).map_err(|_| failed(writes))?;
        }
        let found =
            destination(writer, &plan.fingerprint, &plan.properties).map_err(|_| failed(writes))?;
        if found.kind != ImageState::SameProperties
            || found.alias_target.as_deref() != Some(&plan.fingerprint)
        {
            return Err(failed(writes));
        }
        Ok(
            json!({ "tenant": plan.tenant, "project": plan.project, "fingerprint": plan.fingerprint, "alias": plan.alias,
            "kind": if copied { "imported" } else if writes.image || writes.alias { "completed" } else { "alreadyPresent" },
            "imageWrite": writes.image, "aliasWrite": writes.alias }),
        )
    }
}

fn finish(
    writer: &ImageWriter,
    operation: Result<Option<String>, arca_incus::IncusError>,
) -> Result<(), arca_incus::IncusError> {
    operation.and_then(|operation| operation.map_or(Ok(()), |id| writer.wait(&id)))
}
