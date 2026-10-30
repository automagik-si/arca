//! The watch-only plugin. It classes every instance in the projects it is given by owner, flags privilege and identity, and
//! reports drift and a config hash per object. It holds no write path: its manifest declares no effect, so `Ctx` refuses every
//! write before any provider is asked, and its only read is one listing of every instance.
//!
//! The plugin reaches the host only through `Ctx`. It names no Incus client, provider or approval verifier, which the boundary
//! test checks against this crate's `[dependencies]`.

pub mod report;

use std::collections::{BTreeMap, BTreeSet};

use arca_kernel::canon::Digest;
use arca_kernel::desired::{EntryId, PluginId};
use arca_kernel::journal::{Journal, MemJournal};
use arca_kernel::ownership::{KERNEL_ENTRY_KEY, KERNEL_RETIRING_KEY, OwnerClass, classify};
use arca_kernel::plugin::{Ctx, KeySpec, Manifest, Observe, Plugin, PluginError, Provider};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use report::{Drift, InstanceRow, ProjectRows, WatchReport, config_hash};

/// The one read the watch makes: every instance in every project, recursed. A provider answers it only for the projects the
/// watch covers.
pub const INSTANCES_PATH: &str = "/1.0/instances?all-projects=true&recursion=1";

/// The key the report is provided under.
pub const REPORT_KEY: &str = "watch.report";

/// The entry a one-shot watch runs for.
const WATCH_ENTRY: &str = "watch";

/// The `security.privileged` key, read from an instance's expanded config.
const PRIVILEGED_KEY: &str = "security.privileged";

/// The watch-only plugin.
pub struct Watch;

/// What a watch is told: the identity phones by name, the retained entries, the live entries, and the projects it covers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WatchConfig {
    #[serde(default)]
    pub identity_phones: BTreeSet<String>,
    #[serde(default)]
    pub retained: BTreeMap<EntryId, (PluginId, Digest)>,
    #[serde(default)]
    pub live_entries: BTreeSet<EntryId>,
    /// The projects the watch covers. Every read is scoped to them, and the report names only them.
    #[serde(default)]
    pub projects: BTreeSet<String>,
}

impl Plugin for Watch {
    const ID: &'static str = "arca.watch";
    type Config = WatchConfig;

    fn manifest() -> Manifest {
        Manifest {
            id: PluginId(Self::ID.to_string()),
            version: "0.1.0".to_string(),
            provides: vec![KeySpec {
                key: REPORT_KEY.to_string(),
                version: "0.1.0".to_string(),
            }],
            effects: Vec::new(),
            projects: Vec::new(),
            observes: true,
        }
    }

    fn apply(ctx: &mut Ctx<'_>, cfg: &WatchConfig) -> Result<(), PluginError> {
        let listing = ctx.observe(Observe::IncusGet {
            path: INSTANCES_PATH.to_string(),
        })?;
        let report = assemble(&listing, cfg)?;
        let value = serde_json::to_value(&report)
            .map_err(|_| PluginError::Failed("the report does not serialize".to_string()))?;
        let verb = arca_api::verb("watch_report")
            .ok_or_else(|| PluginError::Failed("the watch_report verb is missing".to_string()))?;
        arca_api::conforms(&(verb.result)(), &value)
            .map_err(|_| PluginError::Failed("the report does not match its schema".to_string()))?;
        ctx.provide(REPORT_KEY, value)?;
        Ok(())
    }
}

/// Runs the watch once, through a `Ctx` whose manifest covers `cfg.projects`, and returns the report stamped with `now_ms`.
///
/// `Ctx` gives a plugin no clock, so the plugin provides `at` as zero and this function stamps it. A caller that runs `Watch`
/// some other way must stamp it too.
pub fn watch_once(
    provider: &mut dyn Provider,
    cfg: &WatchConfig,
    now_ms: u64,
) -> Result<WatchReport, PluginError> {
    let mut manifest = Watch::manifest();
    manifest.projects = cfg.projects.iter().cloned().collect();
    let mut journal = Journal::open(MemJournal::new())
        .map_err(|e| PluginError::Failed(format!("the watch journal: {e}")))?;
    let provided = {
        let entry = EntryId(WATCH_ENTRY.to_string());
        let mut ctx = Ctx::new(entry, &manifest, provider, &mut journal, now_ms);
        Watch::apply(&mut ctx, cfg)?;
        ctx.provided()
    };
    let value = provided
        .get(REPORT_KEY)
        .cloned()
        .ok_or_else(|| PluginError::Failed("the watch provided no report".to_string()))?;
    let mut report: WatchReport = serde_json::from_value(value)
        .map_err(|_| PluginError::Failed("the report does not read back".to_string()))?;
    report.at = now_ms;
    Ok(report)
}

/// One listed instance, read from its row. Only the fields the watch uses are kept.
struct Facts {
    name: String,
    project: String,
    kind: String,
    status: String,
    own: BTreeMap<String, String>,
    privileged: bool,
}

fn shape_error() -> PluginError {
    PluginError::Failed("the instance listing is not in the expected shape".to_string())
}

fn read_row(row: &Value) -> Result<Facts, PluginError> {
    let text = |key: &str| -> Result<String, PluginError> {
        row.get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .ok_or_else(shape_error)
    };
    let kind = text("type")?;
    if kind != "container" && kind != "virtual-machine" {
        return Err(PluginError::Failed(
            "an instance has a type the watch does not classify".to_string(),
        ));
    }
    let own = string_map(row.get("config"))?;
    let expanded = string_map(row.get("expanded_config"))?;
    Ok(Facts {
        name: text("name")?,
        project: text("project")?,
        kind,
        status: text("status")?,
        own,
        privileged: expanded.get(PRIVILEGED_KEY).map(String::as_str) == Some("true"),
    })
}

/// A JSON object as a map of strings. Incus values are strings; any other value keeps its key and its JSON text, so a mark
/// is never lost to a type.
fn string_map(value: Option<&Value>) -> Result<BTreeMap<String, String>, PluginError> {
    let object = value.and_then(Value::as_object).ok_or_else(shape_error)?;
    Ok(object
        .iter()
        .map(|(key, value)| {
            let text = value
                .as_str()
                .map_or_else(|| value.to_string(), str::to_string);
            (key.clone(), text)
        })
        .collect())
}

/// Builds the report from the listing. Rows outside the covered projects are dropped, a listed instance twice is refused, and
/// the drift is the marks against the live entries.
fn assemble(listing: &Value, cfg: &WatchConfig) -> Result<WatchReport, PluginError> {
    let rows = listing.as_array().ok_or_else(shape_error)?;
    let mut facts = rows.iter().map(read_row).collect::<Result<Vec<_>, _>>()?;
    facts.retain(|fact| cfg.projects.contains(&fact.project));
    facts.sort_by(|a, b| (&a.project, &a.name).cmp(&(&b.project, &b.name)));
    if facts
        .windows(2)
        .any(|pair| pair[0].project == pair[1].project && pair[0].name == pair[1].name)
    {
        return Err(PluginError::Failed(
            "an instance is listed twice in one project".to_string(),
        ));
    }

    let mut drift = Vec::new();
    let mut observed: BTreeSet<EntryId> = BTreeSet::new();
    let mut by_project: BTreeMap<&str, Vec<InstanceRow>> = cfg
        .projects
        .iter()
        .map(|project| (project.as_str(), Vec::new()))
        .collect();
    for fact in &facts {
        let owner = classify(&fact.own, &cfg.retained);
        if let Some(mark) = fact.own.get(KERNEL_ENTRY_KEY)
            && let Some(entry) = EntryId::parse(mark)
        {
            observed.insert(entry);
        }
        match &owner {
            OwnerClass::Kernel { entry } if !cfg.live_entries.contains(entry) => {
                drift.push(Drift::MarkedWithoutEntry {
                    project: fact.project.clone(),
                    name: fact.name.clone(),
                    entry: Some(entry.clone()),
                });
            }
            // A foreign object with a tombstone, or with a kernel mark that names no valid entry, is drift that names no entry.
            OwnerClass::Foreign
                if fact.own.contains_key(KERNEL_RETIRING_KEY)
                    || fact.own.contains_key(KERNEL_ENTRY_KEY) =>
            {
                drift.push(Drift::MarkedWithoutEntry {
                    project: fact.project.clone(),
                    name: fact.name.clone(),
                    entry: None,
                });
            }
            _ => {}
        }
        if let Some(rows) = by_project.get_mut(fact.project.as_str()) {
            rows.push(InstanceRow {
                name: fact.name.clone(),
                kind: fact.kind.clone(),
                status: fact.status.clone(),
                owner,
                privileged: fact.privileged,
                identity: cfg.identity_phones.contains(&fact.name),
                config_hash: config_hash(&fact.own),
            });
        }
    }
    for entry in &cfg.live_entries {
        if !observed.contains(entry) {
            drift.push(Drift::EntryWithoutObject {
                entry: entry.clone(),
            });
        }
    }
    let projects = by_project
        .into_iter()
        .map(|(project, instances)| ProjectRows {
            project: project.to_string(),
            instances,
        })
        .collect();
    Ok(WatchReport {
        // The plugin has no clock. `watch_once` stamps this.
        at: 0,
        projects,
        drift,
    })
}
