//! One approved copy into a tenant's image store, with durable compensation records.
use super::*;
use arca_incus::image::{ImageWriter, alias_ok, fingerprint_ok};
use arca_kernel::{
    desired::Entry,
    journal::{EffectClass, EffectState},
    ownership::{IncusKind, KERNEL_ENTRY_KEY, tenant_entry},
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
};

const RECORD: &str = "tenant-image-imports.jsonl";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Plan {
    tenant: String,
    project: String,
    fingerprint: String,
    alias: String,
    tenant_incarnation: String,
    properties: BTreeMap<String, String>,
}

fn refused(why: impl Into<String>) -> CallError {
    CallError::Refused {
        code: "refused".into(),
        message: why.into(),
    }
}
fn failed(error: impl std::fmt::Display) -> CallError {
    refused(error.to_string())
}
fn words(error: &CallError) -> &str {
    match error {
        CallError::Refused { message, .. }
        | CallError::UntrustedRefused { message, .. }
        | CallError::RefusedWith { message, .. } => message,
        CallError::Invalid(why) => why,
        _ => "the import did not complete",
    }
}

fn incarnation(cfg: &NodeConfig, scope: &tenants::TenantScope) -> Result<String, CallError> {
    let (records, torn) = read_journal(&cfg.state_dir.join(JOURNAL_FILE)).map_err(failed)?;
    if torn.is_some() {
        return Err(refused("the tenant's creation journal is incomplete"));
    }
    let row = records
        .iter()
        .filter(|row| {
            row.entry.0 == tenant_entry(&scope.id)
                && row.class == EffectClass::B
                && row.target.kind == IncusKind::Project
                && row.target.name == scope.project
        })
        .max_by_key(|row| row.id)
        .ok_or_else(|| refused("the tenant's project has no recorded creation"))?;
    if !matches!(row.state, EffectState::Applied | EffectState::Confirmed) || row.intent.is_some() {
        return Err(refused("the tenant's project creation has not settled"));
    }
    Ok(row.id.to_string())
}

fn current(cfg: &NodeConfig, entries: &[Entry], args: &Value) -> Result<Plan, CallError> {
    let scope = tenants::tenant_arg(cfg, args, entries)?.ok_or_else(tenants::no_tenants)?;
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
    let read = UnixIncus::new(cfg.incus.socket.clone());
    for project in [arca_netproject::PROJECT, scope.project.as_str()] {
        let state = read
            .get(&format!("/1.0/projects/{project}"))
            .map_err(failed)?;
        if project != arca_netproject::PROJECT
            && state["config"][KERNEL_ENTRY_KEY].as_str() != Some(tenant_entry(&scope.id).as_str())
        {
            return Err(refused(
                "the image store's project is no longer owned by this node's entry",
            ));
        }
        if state["config"]["features.images"] != "true" {
            return Err(refused("both projects must have their own image stores"));
        }
    }
    let source = read
        .get(&format!(
            "/1.0/images/{fingerprint}?project={}",
            arca_netproject::PROJECT
        ))
        .map_err(failed)?;
    if source["fingerprint"] != fingerprint {
        return Err(refused("the source image fingerprint changed"));
    }
    let properties = serde_json::from_value(source.get("properties").cloned().unwrap_or(json!({})))
        .map_err(failed)?;
    let plan = Plan {
        tenant: scope.id.clone(),
        project: scope.project.clone(),
        fingerprint: fingerprint.into(),
        alias: alias.into(),
        tenant_incarnation: incarnation(cfg, &scope)?,
        properties,
    };
    Ok(plan)
}

fn plan(cfg: &NodeConfig, entries: &[Entry], args: &Value) -> Result<Plan, CallError> {
    let plan = current(cfg, entries, args)?;
    // Validate the destination before any write, including an alias collision that appeared after the card.
    let writer = image_writer(cfg, &plan)?;
    if let Some(alias) = writer.alias().map_err(failed)?
        && alias["target"] != plan.fingerprint
    {
        return Err(refused("the tenant's alias names another image"));
    }
    if let Some(image) = writer.image().map_err(failed)?
        && check_image(&plan, &image).is_err()
        && !pending_copy(cfg, &plan)?
    {
        check_image(&plan, &image)?;
    }
    Ok(plan)
}

// A card may authorize settling an unfinished copy, but never rewriting an unrelated pre-existing image.
fn pending_copy(cfg: &NodeConfig, plan: &Plan) -> Result<bool, CallError> {
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(
            rustix::fs::OFlags::NOFOLLOW.bits() as i32 | rustix::fs::OFlags::NONBLOCK.bits() as i32,
        )
        .open(cfg.state_dir.join(RECORD))
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(failed(error)),
    };
    Record::check_file(&file)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(failed)?;
    let mut rows = BTreeMap::new();
    for line in bytes
        .split_inclusive(|b| *b == b'\n')
        .filter(|line| line.ends_with(b"\n"))
    {
        let row: Row = serde_json::from_slice(line).map_err(failed)?;
        rows.insert(row.id, row);
    }
    Ok(rows.values().any(|row| {
        row.new_image
            && row.plan.project == plan.project
            && row.plan.tenant_incarnation == plan.tenant_incarnation
            && row.plan.fingerprint == plan.fingerprint
            && !matches!(
                row.phase,
                Phase::Done | Phase::Reverted | Phase::Retained | Phase::CopyIntent
            )
    }))
}

pub(super) fn enrich(
    cfg: &NodeConfig,
    entries: &[Entry],
    args: &Value,
) -> Result<Value, CallError> {
    let mut args = args.clone();
    for key in ["project", "tenantIncarnation", "properties"] {
        args.as_object_mut()
            .ok_or_else(|| CallError::Invalid("args: expected an object".into()))?
            .remove(key);
    }
    ActionKind::TenantImageImport
        .prepare(&args, None)
        .map_err(CallError::Invalid)?;
    serde_json::to_value(plan(cfg, entries, &args)?).map_err(failed)
}

fn image_writer(cfg: &NodeConfig, plan: &Plan) -> Result<ImageWriter, CallError> {
    ImageWriter::new(
        cfg.incus.socket.clone(),
        &plan.project,
        &plan.fingerprint,
        &plan.alias,
    )
    .map_err(failed)
}

fn check_image(plan: &Plan, image: &Value) -> Result<(), CallError> {
    if image["fingerprint"] != plan.fingerprint
        || image.get("properties").cloned().unwrap_or(json!({})) != json!(plan.properties)
    {
        return Err(refused(
            "the destination image does not carry the approved fingerprint and source properties",
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Phase {
    CopyIntent,
    CopySent,
    PropertiesIntent,
    PropertiesSent,
    ImageReady,
    AliasIntent,
    AliasSent,
    Done,
    UndoAliasIntent,
    UndoAliasSent,
    UndoImageIntent,
    UndoImageSent,
    Reverted,
    Retained,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Row {
    id: u64,
    plan: Plan,
    phase: Phase,
    /// Inverses: remove only the copy/alias absent before this action. The source store is never an inverse target.
    new_image: bool,
    new_alias: bool,
    operation: Option<String>,
    properties_before: Option<Value>,
}

struct Record {
    file: File,
    rows: BTreeMap<u64, Row>,
    poisoned: bool,
}
impl Record {
    fn open(dir: &Path) -> Result<Self, CallError> {
        let path = dir.join(RECORD);
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .mode(0o600)
            .custom_flags(
                rustix::fs::OFlags::NOFOLLOW.bits() as i32
                    | rustix::fs::OFlags::NONBLOCK.bits() as i32,
            )
            .open(&path)
            .map_err(failed)?;
        Self::check_file(&file)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(failed)?;
        let end = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        if end != bytes.len() {
            file.set_len(end as u64).map_err(failed)?;
            file.sync_all().map_err(failed)?;
        }
        let mut rows: BTreeMap<u64, Row> = BTreeMap::new();
        for line in bytes[..end]
            .split(|b| *b == b'\n')
            .filter(|s| !s.is_empty())
        {
            let row: Row = serde_json::from_slice(line).map_err(failed)?;
            if row.id == 0 || rows.get(&row.id).is_some_and(|old| old.plan != row.plan) {
                return Err(refused("the image import record changed identity"));
            }
            rows.insert(row.id, row);
        }
        // Persist the directory entry before the first Incus write too.
        file.flush().map_err(failed)?;
        file.sync_all().map_err(failed)?;
        File::open(dir)
            .and_then(|dir| dir.sync_all())
            .map_err(failed)?;
        Ok(Self {
            file,
            rows,
            poisoned: false,
        })
    }
    fn check_file(file: &File) -> Result<(), CallError> {
        let meta = file.metadata().map_err(failed)?;
        if !meta.is_file()
            || meta.nlink() != 1
            || meta.permissions().mode() & 0o777 != 0o600
            || meta.uid() != arca_approval::effective_uid().map_err(failed)?
            || meta.len() > 64 * 1024 * 1024
        {
            return Err(refused(
                "the image import record is not a private, bounded regular file",
            ));
        }
        Ok(())
    }
    fn append(&mut self, row: &Row) -> Result<(), CallError> {
        if self.poisoned {
            return Err(refused("the import record needs to be reopened"));
        }
        let mut bytes = serde_json::to_vec(row).map_err(failed)?;
        bytes.push(b'\n');
        if let Err(error) = self
            .file
            .write_all(&bytes)
            .and_then(|_| self.file.sync_all())
        {
            self.poisoned = true;
            return Err(failed(error));
        }
        self.rows.insert(row.id, row.clone());
        Ok(())
    }
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
        let object = args
            .as_object()
            .ok_or_else(|| CallError::Invalid("arguments: expected an object".into()))?;
        let allowed = [
            "tenant",
            "fingerprint",
            "alias",
            "approval",
            "project",
            "tenantIncarnation",
            "properties",
        ];
        if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
            return Err(CallError::Invalid(arca_api::unknown_field(
                "", key, &allowed,
            )));
        }
        if node_only_looks(&self.cfg) {
            return Err(only_looks());
        }
        let approved = args["approval"].as_str().is_some_and(|grant| {
            lock(&self.kernel)
                .map(|mut kernel| kernel.consume_action_grant("tenant_image_import", &args, grant))
                .unwrap_or(false)
        });
        if !approved {
            return Err(import_not_approved());
        }
        let _write = self.writes.lock().map_err(|_| internal())?;
        let entries = lock(&self.kernel)?.entries().to_vec();
        let current = plan(&self.cfg, &entries, &args)?;
        let mut bound = args.clone();
        bound.as_object_mut().unwrap().remove("approval");
        let bound: Plan = serde_json::from_value(bound).map_err(failed)?;
        if bound != current {
            return Err(refused(
                "the tenant or source properties changed since the card; propose again. Nothing was written",
            ));
        }
        self.run(current, &entries)
    }
}

impl Handler {
    fn run(&self, plan: Plan, entries: &[Entry]) -> Result<Value, CallError> {
        let writer = image_writer(&self.cfg, &plan)?;
        let mut record = Record::open(&self.cfg.state_dir)?;
        for mut old in record
            .rows
            .values()
            .filter(|r| {
                !matches!(r.phase, Phase::Done | Phase::Reverted | Phase::Retained)
                    && r.plan.project == plan.project
            })
            .cloned()
            .collect::<Vec<_>>()
        {
            if old.plan.tenant_incarnation != plan.tenant_incarnation {
                old.phase = Phase::Retained;
                old.operation = None;
                record.append(&old)?;
                continue;
            }
            let previous = image_writer(&self.cfg, &old.plan)?;
            if let Some(operation) = &old.operation {
                match previous.wait(operation) {
                    Ok(()) | Err(arca_incus::IncusError::Status { .. }) => {}
                    Err(error) => return Err(failed(error)),
                }
            }
            if previous
                .image()
                .map_err(failed)?
                .is_some_and(|image| check_image(&old.plan, &image).is_ok())
                && previous
                    .alias()
                    .map_err(failed)?
                    .is_some_and(|alias| alias["target"] == old.plan.fingerprint)
            {
                old.phase = Phase::Done;
                old.operation = None;
                record.append(&old)?;
                continue;
            }
            self.undo(&mut record, &mut old, entries)?;
        }
        let image = writer.image().map_err(failed)?;
        if let Some(image) = &image {
            check_image(&plan, image)?;
        }
        if image.is_some() && writer.alias().map_err(failed)?.is_some() {
            return Ok(reply(&plan, false));
        }
        let mut row = Row {
            id: record
                .rows
                .keys()
                .next_back()
                .copied()
                .unwrap_or(0)
                .checked_add(1)
                .ok_or_else(internal)?,
            plan: plan.clone(),
            phase: Phase::CopyIntent,
            new_image: image.is_none(),
            new_alias: false,
            operation: None,
            properties_before: None,
        };
        let result = (|| {
            if row.new_image {
                record.append(&row)?;
                self.recheck(&plan, entries)?;
                row.operation = writer.copy(&json!(plan.properties)).map_err(failed)?;
                row.phase = Phase::CopySent;
                record.append(&row)?;
                if let Some(operation) = &row.operation {
                    writer.wait(operation).map_err(failed)?;
                }
            }
            let image = writer
                .image()
                .map_err(failed)?
                .ok_or_else(|| refused("the copy was not read back"))?;
            if row.new_image
                && image.get("properties").cloned().unwrap_or(json!({})) != json!(plan.properties)
            {
                row.phase = Phase::PropertiesIntent;
                row.properties_before = Some(image.get("properties").cloned().unwrap_or(json!({})));
                row.operation = None;
                record.append(&row)?;
                self.recheck(&plan, entries)?;
                row.operation = writer
                    .set_properties(&json!(plan.properties))
                    .map_err(failed)?;
                row.phase = Phase::PropertiesSent;
                record.append(&row)?;
                if let Some(operation) = &row.operation {
                    writer.wait(operation).map_err(failed)?;
                }
            }
            check_image(
                &plan,
                &writer
                    .image()
                    .map_err(failed)?
                    .ok_or_else(|| refused("the copy was not read back"))?,
            )?;
            row.phase = Phase::ImageReady;
            row.operation = None;
            record.append(&row)?;
            // Recheck the alias immediately before its POST; Incus also refuses replacement atomically.
            if let Some(alias) = writer.alias().map_err(failed)? {
                if alias["target"] != plan.fingerprint {
                    return Err(refused("the alias was taken while copying"));
                }
            } else {
                row.phase = Phase::AliasIntent;
                row.new_alias = true;
                record.append(&row)?;
                self.recheck(&plan, entries)?;
                row.operation = writer.add_alias(&mark(&row)).map_err(failed)?;
                row.new_alias = true;
                row.phase = Phase::AliasSent;
                record.append(&row)?;
                if let Some(operation) = &row.operation {
                    writer.wait(operation).map_err(failed)?;
                }
            }
            if writer
                .alias()
                .map_err(failed)?
                .is_none_or(|a| a["target"] != plan.fingerprint)
            {
                return Err(refused("the alias was not read back"));
            }
            row.phase = Phase::Done;
            row.operation = None;
            record.append(&row)?;
            Ok(reply(&plan, true))
        })();
        if let Err(error) = result {
            if record.poisoned {
                return Err(error);
            }
            let cleanup = self.undo(&mut record, &mut row, entries);
            return Err(refused(format!(
                "{}; {}",
                words(&error),
                match cleanup {
                    Ok(()) => "only this action's additions were undone".into(),
                    Err(e) => format!("cleanup incomplete; additions kept: {}", words(&e)),
                }
            )));
        }
        result
    }

    fn recheck(&self, plan: &Plan, entries: &[Entry]) -> Result<(), CallError> {
        let args = serde_json::to_value(plan).map_err(failed)?;
        if current(&self.cfg, entries, &args)? != *plan {
            return Err(refused(
                "the source or tenant changed before the next write",
            ));
        }
        Ok(())
    }

    fn undo(&self, record: &mut Record, row: &mut Row, entries: &[Entry]) -> Result<(), CallError> {
        let writer = image_writer(&self.cfg, &row.plan)?;
        let args = serde_json::to_value(&row.plan).map_err(failed)?;
        let scope =
            tenants::tenant_arg(&self.cfg, &args, entries)?.ok_or_else(tenants::no_tenants)?;
        let project = UnixIncus::new(self.cfg.incus.socket.clone())
            .get(&format!("/1.0/projects/{}", row.plan.project))
            .map_err(failed)?;
        if scope.project != row.plan.project
            || incarnation(&self.cfg, &scope)? != row.plan.tenant_incarnation
            || project["config"][KERNEL_ENTRY_KEY].as_str()
                != Some(tenant_entry(&scope.id).as_str())
            || project["config"]["features.images"] != "true"
        {
            return Err(refused(
                "the inverse target's tenant identity or ownership changed; content was kept",
            ));
        }
        if let Some(operation) = &row.operation {
            // A failed operation has ended; an unreadable/running operation must settle before any inverse write.
            match writer.wait(operation) {
                Ok(()) | Err(arca_incus::IncusError::Status { .. }) => {}
                Err(error) => return Err(failed(error)),
            }
        }
        if row.phase == Phase::CopyIntent {
            // The request's outcome was never recorded: ownership is unproven, so retain any copy it might have made.
            row.new_image = false;
            if writer.image().map_err(failed)?.is_some() {
                row.phase = Phase::Retained;
                row.operation = None;
                record.append(row)?;
                return Err(refused(
                    "the copy's ownership is unproven because its response was not recorded; content was kept",
                ));
            }
        }
        let mark = mark(row);
        let alias = writer.alias().map_err(failed)?;
        if alias
            .as_ref()
            .is_some_and(|a| a["target"] == row.plan.fingerprint && a["description"] == mark)
        {
            row.phase = Phase::UndoAliasIntent;
            row.operation = None;
            record.append(row)?;
            row.operation = writer.delete_alias(&mark).map_err(failed)?;
            row.phase = Phase::UndoAliasSent;
            record.append(row)?;
            if let Some(operation) = &row.operation {
                writer.wait(operation).map_err(failed)?;
            }
            if writer.alias().map_err(failed)?.is_some() {
                return Err(refused("the alias deletion was not read back"));
            }
        }
        if row.new_image
            && let Some(image) = writer.image().map_err(failed)?
        {
            if image["fingerprint"] != row.plan.fingerprint {
                return Err(refused("the inverse image identity was not read back"));
            }
            // Existing services may be updating without the tenant write gate and pin an old image for rollback.
            // Conservatively retain a failed copy while this tenant has any service/instance, history pin or alias.
            let aliases = image["aliases"]
                .as_array()
                .ok_or_else(|| refused("the image's aliases cannot be read"))?;
            let live = UnixIncus::new(self.cfg.incus.socket.clone())
                .get("/1.0/instances?all-projects=true&recursion=1")
                .map_err(failed)?;
            let instances = live
                .as_array()
                .ok_or_else(|| refused("the live instances cannot be read"))?;
            let held = service_entries(entries)
                .iter()
                .any(|(_, service)| service.project == row.plan.project)
                || instances
                    .iter()
                    .any(|instance| instance["project"] == row.plan.project)
                || !aliases.is_empty();
            let mut pinned = false;
            for name in [
                service_update::SERVICE_UPDATES_FILE.to_string(),
                format!("{}.1", service_update::SERVICE_UPDATES_FILE),
            ] {
                match fs::read_to_string(self.cfg.state_dir.join(name)) {
                    Ok(text) => pinned |= text.contains(&row.plan.fingerprint),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(failed(error)),
                }
            }
            if held || pinned {
                row.phase = Phase::Retained;
                row.operation = None;
                record.append(row)?;
                return Err(refused(
                    "the copied image may be used by a container, alias or rollback pin and was retained",
                ));
            }
            row.phase = Phase::UndoImageIntent;
            row.operation = None;
            record.append(row)?;
            row.operation = writer.delete_image().map_err(failed)?;
            row.phase = Phase::UndoImageSent;
            record.append(row)?;
            if let Some(operation) = &row.operation {
                writer.wait(operation).map_err(failed)?;
            }
            if writer.image().map_err(failed)?.is_some() {
                return Err(refused("the image deletion was not read back"));
            }
        }
        row.phase = Phase::Reverted;
        row.operation = None;
        record.append(row)
    }
}

fn mark(row: &Row) -> String {
    format!("arca image import {}", row.id)
}
fn reply(plan: &Plan, imported: bool) -> Value {
    json!({ "tenant": plan.tenant, "project": plan.project, "fingerprint": plan.fingerprint, "alias": plan.alias, "imported": imported })
}
