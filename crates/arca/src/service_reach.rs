//! Approved connector creation and removal through the existing service paths.
use super::*;
use crate::service;
use arca_kernel::desired::Entry;

pub(super) fn connect_name(args: &Value, host: &str) -> Result<String, CallError> {
    if args.get("name").is_some() {
        return service_name(args);
    }
    let name = format!("{host}-ts");
    Ok(if service::service_name_ok(&name) {
        name
    } else {
        "tailscale".into()
    })
}

pub(super) fn edge_name(entries: &[Entry]) -> Result<String, CallError> {
    service_entries(entries).into_iter().filter(|(_, cfg)| cfg.edge.is_some() && cfg.tenant.is_none() && !cfg.adopted).map(|(_, cfg)| cfg.name).min().ok_or_else(|| service_refused("this node has no edge yet: create one from the arca-edge template, then connect to Tailscale"))
}

impl ServiceHandler {
    pub(super) fn disconnect_reach(
        &self,
        name: &str,
        caller: &Caller,
        entries: &[Entry],
        now: u64,
    ) -> Result<Value, CallError> {
        let records = service_entries(entries);
        let (_, connector) = records
            .iter()
            .find(|(_, record)| record.name == name)
            .ok_or_else(|| service_not_found(format!("service {name}: not found")))?;
        if crate::reach::kind(&self.cfg, connector).is_none() {
            return Err(service_refused(format!("service {name}: not a connector")));
        }
        // A failed link may have saved either side only. The shared disconnect repairs both sides.
        let edge = records.iter().find(|(_, record)| {
            record.edge.is_some()
                && (connector.links.iter().any(|link| link.to == record.name)
                    || record.incoming.iter().any(|grant| grant.consumer == name))
        });
        let Some((_, edge)) = edge else {
            return Ok(Value::Null);
        };
        let _write = self.writes.lock().map_err(|_| internal())?;
        let answer = self.link(
            &json!({"from":name,"to":edge.name}),
            caller,
            entries,
            now,
            true,
        )?;
        Ok(answer.get("edge").cloned().unwrap_or(Value::Null))
    }

    pub(super) fn finish_reach(
        &self,
        name: &str,
        caller: &Caller,
        now: u64,
        autostart: bool,
    ) -> Result<Value, CallError> {
        let partial = |step: &str| {
            service_refused(format!(
                "created service {name}, but {step} failed. The service remains: service_status shows it; disconnect and remove it to undo this attempt"
            ))
        };
        let entries = lock(&self.kernel)
            .map_err(|_| partial("reading its record"))?
            .entries()
            .to_vec();
        let edge = edge_name(&entries).map_err(|_| partial("finding the edge"))?;
        let linked = self
            .link(
                &json!({"from":name, "to":edge}),
                caller,
                &entries,
                now,
                false,
            )
            .map_err(|_| partial("linking it to the edge"))?;
        if linked["edge"]["applied"] == false {
            return Err(partial("writing the edge's config"));
        }
        let (_, mut connector) = service_entries(lock(&self.kernel)?.entries())
            .into_iter()
            .find(|(_, record)| record.name == name)
            .ok_or_else(|| partial("reading its record"))?;
        connector.start = true;
        connector.autostart = autostart;
        connector.environment_patch = Some(BTreeSet::new());
        self.put(&[connector], now)
            .map_err(|_| partial("saving its start settings"))?;
        let kernel = self.kernel.clone();
        let life = crate::lifecycle::InstanceLifecycle::new(
            self.cfg.incus.socket.clone(),
            arca_incus::ProjectFamily::new([arca_netproject::PROJECT], [])
                .map_err(|_| partial("starting it"))?,
            move || {
                kernel
                    .lock()
                    .map_err(|_| "kernel unavailable".to_string())?
                    .retained_entries()
                    .map_err(|error| error.to_string())
            },
            &self.cfg.state_dir,
        );
        life.run(
            arca_incus::StateAction::Start,
            arca_netproject::PROJECT,
            name,
            &caller.token_id,
            None,
            now,
        )
        .map_err(|_| partial("starting it"))?;
        crate::reach::reach_rows(&self.kernel, &self.cfg, &self.exec, caller)
            .map_err(|_| partial("reading its state"))?
            .into_iter()
            .find(|row| row["name"] == name)
            .ok_or_else(|| partial("reading its state"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_connector_name_falls_back_only_when_the_node_name_does_not_fit() {
        assert_eq!(connect_name(&json!({}), "node1").unwrap(), "node1-ts");
        assert_eq!(
            connect_name(&json!({}), "node.example.com").unwrap(),
            "tailscale"
        );
        assert_eq!(
            connect_name(&json!({}), &"a".repeat(40)).unwrap(),
            "tailscale"
        );
        assert_eq!(
            connect_name(&json!({"name":"chosen"}), "node1").unwrap(),
            "chosen"
        );
    }
}
