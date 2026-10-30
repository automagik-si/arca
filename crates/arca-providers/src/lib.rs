//! Arca's read-only Incus provider. It serves plugins that observe and never write: it answers the one listing a watch reads,
//! scoped to the projects the calling manifest names, and it refuses every plan and every effect with `WatchOnly`. It holds
//! an [`IncusRead`], whose only method is GET, so no path through this type can send a write.

use arca_incus::{IncusError, IncusRead};
use arca_kernel::desired::EntryId;
use arca_kernel::journal::Inverse;
use arca_kernel::ownership::ObjectRef;
use arca_kernel::plugin::{EffectOp, EffectRefusal, Observe, Planned, Provider};
use serde_json::Value;

/// The one listing a plugin may read: every instance in every project. It is the path `arca_watch::INSTANCES_PATH` names.
pub const INSTANCES_ALL_PROJECTS: &str = "/1.0/instances?all-projects=true&recursion=1";

/// The host's project index. Only the host side reads it, to name the projects a watch covers. No plugin can reach it.
const PROJECTS_PATH: &str = "/1.0/projects?recursion=1";

/// A provider that reads Incus and nothing else.
pub struct ReadOnlyIncusProvider<R: IncusRead> {
    incus: R,
}

impl<R: IncusRead> ReadOnlyIncusProvider<R> {
    /// A read-only provider over `incus`.
    pub fn new(incus: R) -> Self {
        Self { incus }
    }

    /// The name of every project on the host, from the project index. This is the host side's enumeration, used to choose the
    /// projects a watch covers. It is an inherent method, not a [`Provider`] answer, so a plugin cannot call it.
    pub fn project_names(&self) -> Result<Vec<String>, EffectRefusal> {
        let listing = self.incus.get(PROJECTS_PATH).map_err(read_refusal)?;
        let rows = listing.as_array().ok_or_else(not_a_list)?;
        rows.iter()
            .map(|row| {
                row.get("name")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .ok_or_else(|| {
                        EffectRefusal::Provider("a project row names no project".to_string())
                    })
            })
            .collect()
    }
}

impl<R: IncusRead> Provider for ReadOnlyIncusProvider<R> {
    /// Answers the listing for the projects the manifest names. A row in another project is dropped before the caller sees it.
    /// Every other path is refused, so a plugin reads nothing it did not ask the provider for by name.
    fn observe(
        &self,
        _entry: &EntryId,
        projects: &[String],
        query: &Observe,
    ) -> Result<Value, EffectRefusal> {
        match query {
            Observe::IncusGet { path } if path == INSTANCES_ALL_PROJECTS => {
                let listing = self
                    .incus
                    .get(INSTANCES_ALL_PROJECTS)
                    .map_err(read_refusal)?;
                let rows = listing.as_array().ok_or_else(not_a_list)?;
                let mut kept = Vec::with_capacity(rows.len());
                for row in rows {
                    let project = row.get("project").and_then(Value::as_str).ok_or_else(|| {
                        EffectRefusal::Provider("an instance row names no project".to_string())
                    })?;
                    if projects.iter().any(|allowed| allowed == project) {
                        kept.push(row.clone());
                    }
                }
                Ok(Value::Array(kept))
            }
            Observe::IncusGet { .. } => Err(EffectRefusal::NotDeclared),
            Observe::HostAddresses => {
                Err(EffectRefusal::Provider("kernel provider only".to_string()))
            }
        }
    }

    /// Refused: a read-only provider plans nothing, so no plan reaches Incus.
    fn plan(&self, _entry: &EntryId, _op: &EffectOp) -> Result<Planned, EffectRefusal> {
        Err(EffectRefusal::WatchOnly)
    }

    /// Refused: a read-only provider applies nothing.
    fn effect(
        &mut self,
        _entry: &EntryId,
        _op: &EffectOp,
        _planned: &Planned,
    ) -> Result<(ObjectRef, Inverse), EffectRefusal> {
        Err(EffectRefusal::WatchOnly)
    }
}

/// A read failure as a fixed reason. Incus's own message is left out, since it can echo a response body.
fn read_refusal(error: IncusError) -> EffectRefusal {
    let reason = match error {
        IncusError::Io(_) => "the Incus socket failed".to_string(),
        IncusError::Status { code, .. } => format!("Incus answered {code}"),
        IncusError::Decode(_) => "the Incus answer is not in the expected shape".to_string(),
        IncusError::Timeout => "Incus did not answer in time".to_string(),
        IncusError::TooLarge => "the Incus answer is too large".to_string(),
    };
    EffectRefusal::Provider(reason)
}

fn not_a_list() -> EffectRefusal {
    EffectRefusal::Provider("the Incus answer is not a list".to_string())
}
