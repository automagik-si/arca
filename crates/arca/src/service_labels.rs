//! Caller-owned labels live only on the instance, outside desired state and the effect journal.
use super::*;
use arca_incus::{IncusError, UnixIncusWriter};
use arca_kernel::desired::Entry;
use arca_kernel::ownership::ObjectRef;
use std::collections::BTreeMap;

const KEY: &str = "user.arca.labels";
const ATTEMPTS: usize = 3;
type Labels = BTreeMap<String, String>;
type Delta = BTreeMap<String, Option<String>>;

pub(crate) fn view(live: Option<&Value>) -> Value {
    let labels = live
        .and_then(|row| row["config"][KEY].as_str())
        .and_then(|text| serde_json::from_str::<Labels>(text).ok())
        .unwrap_or_default();
    json!(labels)
}

fn read(config: &BTreeMap<String, String>) -> Result<Labels, CallError> {
    match config.get(KEY) {
        None => Ok(Labels::new()),
        Some(text) => serde_json::from_str(text)
            .map_err(|_| service_refused("the instance's labels are not a string map")),
    }
}

fn merge(mut labels: Labels, delta: &Delta) -> Result<String, CallError> {
    for (key, value) in delta {
        match value {
            Some(value) => {
                labels.insert(key.clone(), value.clone());
            }
            None => {
                labels.remove(key);
            }
        }
    }
    let text = serde_json::to_string(&labels).map_err(|_| internal())?;
    if labels.len() > 128
        || text.len() > 65536
        || labels
            .iter()
            .any(|(key, value)| key.len() > 128 || value.len() > 4096)
    {
        return Err(CallError::Invalid("labels: merged map exceeds 128 keys, 128 bytes per key, 4096 bytes per value or 65536 serialized bytes".into()));
    }
    Ok(text)
}

fn unconfirmed() -> CallError {
    CallError::Refused {
        code: "labels_unconfirmed".into(),
        message: "the labels PATCH may have applied, but its read-back is unavailable or ownership changed; read service_status again before deciding what to do".into(),
    }
}

fn write(writer: &UnixIncusWriter, object: &ObjectRef, delta: &Delta) -> Result<Value, CallError> {
    for attempt in 0..ATTEMPTS {
        // Each retry checks the original journal UUID and ownership, then merges the ORIGINAL delta.
        let owned = writer
            .read_owned(object)
            .map_err(|_| service_refused("service ownership, UUID or availability changed"))?;
        let text = merge(read(owned.config())?, delta)?;
        match writer.patch(&owned, &BTreeMap::from([(KEY.into(), Some(text))])) {
            Ok(fresh) => return Ok(json!({"labels": read(fresh.config())?, "code": "confirmed"})),
            Err(IncusError::Status { code: 412, .. }) if attempt + 1 < ATTEMPTS => continue,
            Err(IncusError::Status { code: 412, .. }) => {
                return Err(service_refused(
                    "labels changed concurrently; retry the original delta",
                ));
            }
            Err(IncusError::Refused(_)) => {
                return Err(service_refused(
                    "the labels write was refused before sending",
                ));
            }
            Err(_) => {
                // Config PATCH is synchronous but may land after transport failure. Even an old map is NOT proof
                // it did not apply. Never replay an uncertain write, and never call that read a confirmed result.
                let fresh = writer.read_owned(object).map_err(|_| unconfirmed())?;
                let labels = read(fresh.config()).map_err(|_| unconfirmed())?;
                return Ok(json!({"labels": labels, "code": "unconfirmed"}));
            }
        }
    }
    unreachable!("bounded retries return on their last attempt")
}

/// Repair only the label key after restoring a snapshot. Never replay an uncertain PATCH.
fn put_back(writer: &UnixIncusWriter, owned: &arca_incus::Owned) -> Result<(), IncusError> {
    let labels = owned.config().get(KEY).cloned();
    for attempt in 0..ATTEMPTS {
        let fresh = writer.read_owned(owned.target())?;
        if fresh.config().get(KEY) == labels.as_ref() {
            return Ok(());
        }
        match writer.patch(&fresh, &BTreeMap::from([(KEY.into(), labels.clone())])) {
            Ok(_) => return Ok(()),
            Err(IncusError::Status { code: 412, .. }) if attempt + 1 < ATTEMPTS => continue,
            Err(error) => {
                let _ = writer.read_owned(owned.target());
                return Err(error);
            }
        }
    }
    unreachable!("bounded retries return on their last attempt")
}

impl ServiceHandler {
    /// Keep labels best effort under the existing write gate. Label failures never prevent the restore or
    /// change its result. A failed pre-read drops the gate before restoring, as without label preservation.
    pub(super) fn with_labels_preserved(
        &self,
        entry: &Entry,
        config: &crate::service::ServiceConfig,
        restore: impl FnOnce() -> Result<(), IncusError>,
    ) -> Result<(), IncusError> {
        let keeping = self.writes.lock().ok().and_then(|gate| {
            let owned = self.live_owned(entry, config).ok()?;
            let target = self.exec_target(entry, config).ok()?;
            Some((gate, owned, target))
        });
        let Some((_write, owned, target)) = keeping else {
            eprintln!(
                "arca: service {}: labels could not be read before rollback; restoring snapshot without keeping labels",
                config.name
            );
            return restore();
        };
        let result = restore();
        if result.is_ok() {
            // The restore has its own operation budget. Keep the label repair bounded separately.
            let kept = crate::service_exec::writer_for(&self.cfg, &target, Duration::from_secs(90))
                .ok()
                .is_some_and(|writer| put_back(&writer, &owned).is_ok());
            if !kept {
                eprintln!(
                    "arca: service {}: rollback succeeded but labels could not be kept; its labels read as of the snapshot",
                    config.name
                );
            }
        }
        result
    }

    pub(super) fn call_labels(&self, args: &Value, entries: &[Entry]) -> Result<Value, CallError> {
        let name = service_name(args)?;
        let delta: Delta = serde_json::from_value(args["labels"].clone()).map_err(|_| {
            CallError::Invalid("labels: expected an object of strings or null".into())
        })?;
        let (entry, config) = service_entries(entries)
            .into_iter()
            .find(|(_, config)| config.name == name)
            .ok_or_else(|| service_not_found("service not found"))?;
        if config.adopted {
            return Err(service_wiring::adopted(&name));
        }
        // Reuse the normal service target checks, including project, name and journal UUID.
        let owned = self.live_owned(&entry, &config)?;
        let target = self.exec_target(&entry, &config)?;
        // Leave a separate budget for read-back after a timed-out PATCH.
        let writer = crate::service_exec::writer_for(&self.cfg, &target, Duration::from_secs(90))
            .map_err(|error| error.call_error())?;
        write(&writer, owned.target(), &delta)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_bounds_apply_to_the_merged_map_in_bytes_and_serialized_bytes() {
        let empty = Labels::new();
        assert!(
            merge(
                empty.clone(),
                &Delta::from([("é".repeat(64), Some("v".repeat(4096)))])
            )
            .is_ok()
        );
        assert!(
            merge(
                empty.clone(),
                &Delta::from([("é".repeat(65), Some("v".into()))])
            )
            .is_err()
        );
        assert!(
            merge(
                empty.clone(),
                &Delta::from([("k".into(), Some("é".repeat(2049)))])
            )
            .is_err()
        );
        let full: Labels = (0..128).map(|n| (format!("k{n}"), "v".into())).collect();
        assert!(merge(full.clone(), &Delta::new()).is_ok());
        assert!(
            merge(
                full.clone(),
                &Delta::from([("extra".into(), Some("".into()))])
            )
            .is_err()
        );
        assert!(
            merge(
                full,
                &Delta::from([("k0".into(), None), ("extra".into(), Some("".into()))])
            )
            .is_ok()
        );
        let big: Labels = (0..16)
            .map(|n| (format!("k{n}"), "v".repeat(4096)))
            .collect();
        assert!(merge(big.clone(), &Delta::new()).is_err());
        assert!(merge(big, &Delta::from([("k0".into(), None)])).is_ok());
        let escaped: Labels = (0..3)
            .map(|n| (format!("k{n}"), "\0".repeat(4096)))
            .collect();
        assert!(
            merge(escaped, &Delta::new()).is_err(),
            "serialized escape expansion counts"
        );
        // An oversized removal key is harmless: bounds apply after the merge.
        assert!(merge(empty, &Delta::from([("k".repeat(129), None)])).is_ok());
    }
}
