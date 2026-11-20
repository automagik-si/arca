//! In-place updates to the non-secret settings of an existing VM.
use super::service_update::{Claim, Journal, SERVICE_UPDATES_FILE, health_seconds, stays_running};
use super::*;
use crate::service::{self, ServiceConfig, Vm};
use crate::service_exec::{ExecTarget, ServiceExec};
use arca_incus::{
    IncusError, InstanceState, Owned, StateAction, StateChanger,
    exec::{ServiceFile, settings_agent_unavailable},
};
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

#[derive(Default)]
struct GuestChanges {
    /// The settings write succeeded or its effect is unknown.
    settings: bool,
    /// A power action was attempted.
    power: bool,
}

impl<'a> Guest<'a> {
    fn new(
        handler: &'a ServiceHandler,
        entry: &Entry,
        service: &ServiceConfig,
    ) -> Result<Self, CallError> {
        let owned = handler.live_owned(entry, service)?;
        let changer = handler.state_changer(service)?;
        let (initial, instance) = changer
            .read_with(&service.project, &service.instance)
            .map_err(|_| service_refused("could not read the VM power state"))?;
        if initial.uuid.as_deref() != owned.uuid() || initial.kind != "virtual-machine" {
            return Err(service_refused("the VM instance changed since it was read"));
        }
        Ok(Self {
            exec: &handler.exec,
            target: handler.exec_target(entry, service)?,
            owned,
            changer,
            first_boot: initial.is_stopped() && instance["last_used_at"] == "0001-01-01T00:00:00Z",
            initial,
        })
    }

    fn write_file(
        &self,
        vm: &Vm,
        wait_for_boot: bool,
        settings_changed: &mut bool,
    ) -> Result<(), CallError> {
        let value = vm.guest_settings().unwrap_or_else(|| json!({}));
        let bytes = serde_json::to_vec(&value).map_err(|_| internal())?;
        let until = Instant::now() + Duration::from_secs(120);
        loop {
            let writer = self
                .exec
                .writer(&self.target, Duration::from_secs(15))
                .map_err(|e| e.call_error())?;
            let mut write = |file, bytes: &[u8]| {
                let result = writer.write_service_file(&self.owned, file, bytes);
                *settings_changed |= result.as_ref().err().is_none_or(|error| {
                    !matches!(error, IncusError::Refused(_)) && !settings_agent_unavailable(error)
                });
                result
            };
            let result = (|| {
                if self
                    .target
                    .service
                    .vm
                    .as_ref()
                    .is_some_and(|vm| vm.vendor_data.is_none())
                {
                    write(&ServiceFile::GuestSettingsDir, &[])?;
                }
                write(&ServiceFile::GuestSettings, &bytes)
            })();
            match result {
                Ok(()) => return Ok(()),
                Err(_) if wait_for_boot && Instant::now() < until => {
                    thread::sleep(Duration::from_secs(2))
                }
                Err(_) => return Err(service_refused("could not write the guest settings file")),
            }
        }
    }

    fn write_seed(&self, vm: &Vm, settings_changed: &mut bool) -> Result<(), CallError> {
        // A new VM can be waiting for secrets or have a long-running first-boot script.
        // Change the seed without starting that script.
        let writer = self
            .exec
            .writer(&self.target, Duration::from_secs(15))
            .map_err(|e| e.call_error())?;
        let owned = writer
            .read_owned(self.owned.target())
            .map_err(|_| service_refused("could not read the VM's ownership"))?;
        let result = writer.patch(
            &owned,
            &BTreeMap::from([("cloud-init.vendor-data".into(), vm.vendor_data.clone())]),
        );
        *settings_changed |= !matches!(&result, Err(IncusError::Refused(_)));
        result
            .map_err(|_| service_refused("could not write the VM's first-boot settings"))
            .map(|_| ())
    }

    fn power(&self) -> Result<InstanceState, CallError> {
        self.changer
            .read(&self.initial.project, &self.initial.name)
            .map_err(|_| service_refused("could not read the VM power state"))
    }

    fn start_for_write(&self, power_attempted: &mut bool) -> Result<bool, CallError> {
        let stopped = self.power()?.is_stopped();
        if stopped {
            *power_attempted = true;
            self.changer
                .change(&self.initial, StateAction::Start)
                .map_err(|_| service_refused("could not start the VM to write its settings"))?;
        }
        Ok(stopped)
    }

    fn deliver(&self, vm: &Vm) -> Result<(), CallError> {
        let mut changes = GuestChanges::default();
        if self.first_boot {
            return self.write_seed(vm, &mut changes.settings);
        }
        let started = self.start_for_write(&mut changes.power)?;
        let written = self.write_file(vm, started, &mut changes.settings);
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

    fn write_pin_settings(&self, vm: &Vm, changes: &mut GuestChanges) -> Result<(), CallError> {
        if self.first_boot {
            self.write_seed(vm, &mut changes.settings)
        } else {
            let started = self.start_for_write(&mut changes.power)?;
            self.write_file(vm, started, &mut changes.settings)
        }
    }

    fn finish_pin(&self, power_attempted: &mut bool) -> Result<(), CallError> {
        if self.first_boot {
            return Ok(());
        }
        let stopped = self.power()?.is_stopped();
        let action = match (self.initial.is_stopped(), stopped) {
            (true, true) => return Ok(()),
            (true, false) => StateAction::Stop,
            (false, true) => StateAction::Start,
            (false, false) => StateAction::Restart,
        };
        *power_attempted = true;
        self.changer
            .change(&self.initial, action)
            .map_err(|_| {
                service_refused(format!(
                    "could not {} the VM after writing settings",
                    action.as_str()
                ))
            })
            .map(|_| ())
    }

    fn check_health(&self, seconds: u64, pace: Duration) -> Result<(), CallError> {
        stays_running(seconds, pace, || {
            self.power().is_ok_and(|state| {
                state.is_running()
                    && state.uuid == self.initial.uuid
                    && state.kind == self.initial.kind
            })
        })
        .then_some(())
        .ok_or_else(|| service_refused("health check: the VM did not stay running"))
    }
}

fn pin_error_why(error: &CallError) -> &str {
    error
        .refusal()
        .map_or("could not save or apply VM settings", |(_, why)| why)
}

fn pin_step(
    journal: &Journal,
    name: &str,
    run: impl FnOnce() -> Result<(), CallError>,
) -> Result<(), CallError> {
    let result = run();
    let detail = match &result {
        Ok(()) => json!({}),
        Err(error) => json!({"why": pin_error_why(error)}),
    };
    journal.step(name, result.is_ok(), detail);
    result
}

fn pin_digest(args: &Value) -> Result<&str, CallError> {
    // Require a registry with an optional port and nonempty OCI repository components.
    static NAME: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(concat!(
            r"^[a-z0-9]+(?:-+[a-z0-9]+)*(?:\.[a-z0-9]+(?:-+[a-z0-9]+)*)*(?::[0-9]+)?",
            r"(?:/[a-z0-9]+(?:(?:\.|__?|-+)[a-z0-9]+)*)+$"
        ))
        .expect("image name pattern")
    });
    args["digest"]
        .as_str()
        .filter(|pin| {
            service::image_by_digest(pin)
                && pin
                    .split_once('@')
                    .is_some_and(|(name, _)| NAME.is_match(name))
        })
        .ok_or_else(|| {
            CallError::Invalid(
                "digest: expected <registry>/<name>@sha256:<64 lower-case hex digits>".into(),
            )
        })
}

impl ServiceHandler {
    pub(super) fn update_vm_pin(
        &self,
        seen: &Entry,
        name: &str,
        args: &Value,
    ) -> Result<Value, CallError> {
        let said = |why: &str| {
            service_refused(format!("update service {name}: {why}. Nothing was changed"))
        };
        let _claim = Claim::take(&format!("{}\n{}", self.cfg.state_dir.display(), seen.id.0))
            .ok_or_else(|| said("another update of this service is running"))?;
        let _write = self.writes.lock().map_err(|_| internal())?;
        let seconds = health_seconds(args)?;
        let (entry, service) = service_entries(lock(&self.kernel)?.entries())
            .into_iter()
            .find(|(entry, service)| {
                entry.id == seen.id
                    && entry.config["id"] == seen.config["id"]
                    && service.vm.is_some()
                    && !service.adopted
            })
            .ok_or_else(|| said("the service's record changed before the update"))?;
        let old_vm = service.vm.as_ref().expect("VM record");
        let template =
            service::load_template(&self.cfg.state_dir.join("templates"), &service.template)
                .ok()
                .filter(service::Template::is_vm);
        let Some(key) = template
            .as_ref()
            .and_then(|template| template.update.as_ref())
            .map(|update| &update.pin)
        else {
            return Err(said(
                "only a container service made from an image by digest or a local image pinned by fingerprint can be updated",
            ));
        };
        let digest = pin_digest(args)?;
        let old_pin = service::created_pin(&service, template.as_ref());
        let result = |updated, restarted, message: &str| json!({ "service": name, "digest": digest, "updated": updated, "restarted": restarted, "message": message });
        if old_pin.as_deref() == Some(digest) {
            return Ok(result(
                false,
                false,
                "The VM already has the requested pin.",
            ));
        }

        let guest = Guest::new(self, &entry, &service)?;
        let mut next = service.clone();
        let vm = next.vm.as_mut().expect("VM record");
        vm.settings
            .get_or_insert_default()
            .insert(key.clone(), json!(digest));
        let merged = vm
            .guest_settings()
            .ok_or_else(|| said("the VM's recorded settings could not be read"))?;
        let seed = service::vendor_data_of(&service.template, Some(&merged))
            .map_err(service_template_error)?;
        if guest.first_boot {
            vm.vendor_data = seed;
        }
        let vm = next.vm.as_ref().expect("VM record");
        let running = !guest.initial.is_stopped();
        let journal = Journal {
            path: self.cfg.state_dir.join(SERVICE_UPDATES_FILE),
            id: format!("pin-{}-{}", entry.id.0, (self.clock)()),
            service: name.to_string(),
            clock: self.clock.clone(),
        };
        journal.line("begin", true, json!({
            "pin": key, "from": old_pin.filter(|pin| service::image_by_digest(pin)), "to": digest, "timeoutSeconds": seconds,
        })).map_err(|_| said("the update journal could not be written"))?;
        let mut changes = GuestChanges::default();
        let mut record_attempted = false;
        let applied = (|| {
            pin_step(&journal, "guest settings", || {
                guest.write_pin_settings(vm, &mut changes)
            })?;
            pin_step(&journal, "power", || guest.finish_pin(&mut changes.power))?;
            if running {
                pin_step(&journal, "health", || {
                    guest.check_health(seconds, self.update.pace)
                })?;
            }
            // Keep the old record until the delivery and restart are complete so a retry
            // after interruption still writes the pin and applies it to the guest.
            record_attempted = true;
            pin_step(&journal, "record", || {
                self.put(std::slice::from_ref(&next), (self.clock)())
            })
        })();
        if let Err(error) = applied {
            let why = pin_error_why(&error);
            if !changes.settings && !changes.power && !record_attempted {
                journal.step("end", false, json!({"why": why}));
                return Err(said(why));
            }
            let record = !record_attempted
                || pin_step(&journal, "restore record", || {
                    self.put(std::slice::from_ref(&service), (self.clock)())
                })
                .is_ok();
            let mut recovery = GuestChanges::default();
            let file = !changes.settings
                || pin_step(&journal, "restore guest settings", || {
                    guest.write_pin_settings(old_vm, &mut recovery)
                })
                .is_ok();
            let power = if running && !file {
                // An unconfirmed file is unsafe to activate. The VM may still be
                // in its original power state if no power action was needed.
                !changes.power && !recovery.power
            } else {
                pin_step(&journal, "restore power", || {
                    guest.finish_pin(&mut recovery.power)
                })
                .is_ok()
                    && (!running
                        || pin_step(&journal, "restore health", || {
                            guest.check_health(seconds, self.update.pace)
                        })
                        .is_ok())
            };
            let restored = record && file && power;
            journal.step("end", false, json!({ "why": why, "rolledBack": restored, "recordRestored": record, "guestRestored": file, "powerRestored": power }));
            return Err(CallError::Refused {
                code: if restored {
                    "rolled_back"
                } else {
                    "rollback_failed"
                }
                .into(),
                message: if restored {
                    format!("update service {name}: {why}. The VM pin was rolled back.")
                } else {
                    format!(
                        "update service {name}: {why}. Rollback could not be confirmed. Previous record restored: {record}. Previous guest settings restored: {file}. Previous power state restored: {power}."
                    )
                },
            });
        }
        journal.step("end", true, json!({"restarted": running}));
        Ok(result(
            true,
            running,
            if running {
                "Updated the VM pin and restarted the VM."
            } else {
                "Updated the VM pin. The VM is stopped and will read it on its next boot."
            },
        ))
    }

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

        let guest = Guest::new(self, &plan.entry, service)?;
        let mut next = service.clone();
        let vm = next.vm.as_mut().expect("VM plan");
        vm.settings = Some(plan.overrides);
        if guest.first_boot {
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
