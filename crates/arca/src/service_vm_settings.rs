//! Owner-approved updates to the non-secret settings file of an existing VM.
use super::service_update::{Journal, SERVICE_UPDATES_FILE};
use super::*;
use crate::service::{ServiceConfig, Vm};
use crate::service_exec::{ExecTarget, ServiceExec};
use arca_incus::{InstanceState, Owned, StateAction, StateChanger, exec::ServiceFile};
use arca_kernel::desired::Entry;
use serde_json::Map;

pub(super) struct Plan {
    entry: Entry,
    service: ServiceConfig,
    merged: Value,
    overrides: Map<String, Value>,
    pub(super) changes: Value,
}

pub(super) fn plan(node: &NodeConfig, entries: &[Entry], args: &Value) -> Result<Plan, CallError> {
    if let Some(key) = [
        "digest",
        "eraseData",
        "follow",
        "withTemplate",
        "noNewPrivileges",
        "timeoutSeconds",
    ]
    .into_iter()
    .find(|key| args.get(key).is_some())
    {
        return Err(CallError::Invalid(format!(
            "settings: give VM settings separately from {key}"
        )));
    }
    let name = service_name(args)?;
    let tenant = tenants::tenant_arg(node, args, entries)?;
    let scoped = tenants::scoped_entries(entries, tenant.as_ref());
    let (entry, service) = service_entries(&scoped)
        .into_iter()
        .find(|(_, s)| s.name == name)
        .ok_or_else(|| service_not_found(format!("service {name}: not found")))?;
    let vm = service
        .vm
        .as_ref()
        .ok_or_else(|| service_refused("settings requires a virtual-machine service"))?;
    if service.adopted {
        return Err(service_wiring::adopted(&name));
    }
    let template =
        crate::service::load_template(&node.state_dir.join("templates"), &service.template)
            .map_err(service_template_error)?;
    let current = vm.guest_settings().unwrap_or_else(|| json!({}));
    let result = crate::service::merge_settings(&template, Some(&current), args.get("settings"))
        .map_err(service_template_error)?;
    let mut overrides = vm.settings.clone().unwrap_or_default();
    let mut changes = Map::new();
    for (key, value) in result.replaced.unwrap_or_default() {
        if current.get(&key) != Some(&value) {
            let mut change = json!({"new": value});
            if let Some(old) = current.get(&key) {
                change["old"] = old.clone();
            }
            changes.insert(key.clone(), change);
            overrides.insert(key, value);
        }
    }
    Ok(Plan {
        entry,
        service,
        merged: result.merged.expect("supplied current settings"),
        overrides,
        changes: json!({"settings": changes}),
    })
}

/// All writes, including recovery, address the same owned guest and journalled UUID.
struct Guest<'a> {
    exec: &'a ServiceExec,
    target: ExecTarget,
    owned: Owned,
    changer: StateChanger,
    initial: InstanceState,
    first_boot: bool,
}

impl Guest<'_> {
    fn write_file(&self, value: &Value, wait_for_boot: bool) -> Result<(), CallError> {
        let bytes = serde_json::to_vec(value).map_err(|_| internal())?;
        let until = Instant::now() + Duration::from_secs(120);
        loop {
            let writer = self
                .exec
                .writer(&self.target, Duration::from_secs(15))
                .map_err(|e| e.call_error())?;
            let write = || {
                if self
                    .target
                    .service
                    .vm
                    .as_ref()
                    .is_some_and(|vm| vm.vendor_data.is_none())
                {
                    writer.write_service_file(&self.owned, &ServiceFile::GuestSettingsDir, &[])?;
                }
                writer.write_service_file(&self.owned, &ServiceFile::GuestSettings, &bytes)
            };
            match write() {
                Ok(()) => return Ok(()),
                Err(_) if wait_for_boot && Instant::now() < until => {
                    thread::sleep(Duration::from_secs(2))
                }
                Err(_) => return Err(service_refused("could not write the guest settings file")),
            }
        }
    }

    fn deliver(&self, vm: &Vm) -> Result<(), CallError> {
        if self.first_boot {
            // A new VM can be waiting for secrets or have a long-running first-boot script.
            // Change the seed without starting that script.
            let writer = self
                .exec
                .writer(&self.target, Duration::from_secs(15))
                .map_err(|e| e.call_error())?;
            let owned = writer
                .read_owned(self.owned.target())
                .map_err(|_| service_refused("could not read the VM's ownership"))?;
            writer
                .patch(
                    &owned,
                    &BTreeMap::from([("cloud-init.vendor-data".into(), vm.vendor_data.clone())]),
                )
                .map_err(|_| service_refused("could not write the VM's first-boot settings"))?;
            return Ok(());
        }

        let now = self
            .changer
            .read(&self.initial.project, &self.initial.name)
            .map_err(|_| service_refused("could not read the VM power state"))?;
        if now.is_stopped() {
            self.changer
                .change(&self.initial, StateAction::Start)
                .map_err(|_| service_refused("could not start the VM to write its settings"))?;
        }
        let value = vm.guest_settings().unwrap_or_else(|| json!({}));
        let written = self.write_file(&value, now.is_stopped());
        if self.initial.is_stopped() {
            // Return a temporarily booted VM to its original state even if the write failed.
            self.changer
                .change(&self.initial, StateAction::Stop)
                .map_err(|_| service_refused("could not return the VM to its stopped state"))?;
            written
        } else {
            written?;
            self.changer
                .change(&self.initial, StateAction::Restart)
                .map_err(|_| service_refused("could not restart the VM after writing settings"))?;
            Ok(())
        }
    }
}

impl ServiceHandler {
    /// The caller holds the node write gate, so another settings or wiring change cannot be lost.
    pub(super) fn update_vm_settings(&self, args: &Value) -> Result<Value, CallError> {
        let plan = plan(&self.cfg, lock(&self.kernel)?.entries(), args)?;
        if args.get("changes") != Some(&plan.changes) {
            return Err(service_refused(
                "VM settings changed since the owner's card was made. Propose the update again",
            ));
        }
        let service = &plan.service;
        let result = |updated: bool, restarted: bool, message: &str| {
            json!({
                "service": service.name, "updated": updated, "restarted": restarted,
                "changes": plan.changes, "message": message
            })
        };
        if plan.changes["settings"]
            .as_object()
            .is_some_and(Map::is_empty)
        {
            return Ok(result(
                false,
                false,
                "The VM already has the requested settings.",
            ));
        }

        let owned = self.live_owned(&plan.entry, service)?;
        let changer = self.state_changer(service)?;
        let (initial, instance) = changer
            .read_with(&service.project, &service.instance)
            .map_err(|_| service_refused("could not read the VM power state"))?;
        if initial.uuid.as_deref() != owned.uuid() || initial.kind != "virtual-machine" {
            return Err(service_refused("the VM instance changed since it was read"));
        }
        let first_boot = initial.is_stopped() && instance["last_used_at"] == "0001-01-01T00:00:00Z";
        let guest = Guest {
            exec: &self.exec,
            target: self.exec_target(&plan.entry, service)?,
            owned,
            changer,
            initial,
            first_boot,
        };
        let mut next = service.clone();
        let vm = next.vm.as_mut().expect("VM plan");
        vm.settings = Some(plan.overrides);
        if first_boot {
            vm.vendor_data = crate::service::vendor_data_of(&service.template, Some(&plan.merged))
                .map_err(service_template_error)?;
        }
        let journal = Journal {
            path: self.cfg.state_dir.join(SERVICE_UPDATES_FILE),
            id: format!("settings-{}-{}", plan.entry.id.0, (self.clock)()),
            service: service.name.clone(),
            clock: self.clock.clone(),
        };
        journal
            .line("begin", true, plan.changes.clone())
            .map_err(|_| service_refused("could not write the service update journal"))?;

        // Commit the effective values only after delivery and the power change. A retry after
        // interruption can repeat both operations with the previous record as its starting point.
        let delivered = guest.deliver(vm);
        let attempted_save = delivered.is_ok();
        let applied = delivered.and_then(|()| self.put(&[next], (self.clock)()));
        if let Err(error) = applied {
            // Reconcile may have saved the new record before reporting an error.
            let record = !attempted_save
                || self
                    .put(std::slice::from_ref(service), (self.clock)())
                    .is_ok();
            let restored = guest.deliver(service.vm.as_ref().expect("VM plan")).is_ok();
            journal.step(
                "end",
                false,
                json!({"recordRestored":record,"guestRestored":restored}),
            );
            let why = error
                .refusal()
                .map_or("could not save VM settings", |(_, why)| why);
            return Err(service_refused(format!(
                "{why}. Previous record restored: {record}. Previous guest settings and power state confirmed: {restored}."
            )));
        }
        journal.step("end", true, json!({}));
        if guest.initial.is_stopped() {
            Ok(result(
                true,
                false,
                "Updated VM settings. The VM is stopped and will read them on its next boot.",
            ))
        } else {
            Ok(result(
                true,
                true,
                "Updated VM settings and restarted the VM.",
            ))
        }
    }
}
