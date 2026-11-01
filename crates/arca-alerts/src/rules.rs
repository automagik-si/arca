//! `alerts.json`: the rule thresholds, the optional shim socket and the latency targets. The node's state directory holds it, and
//! it is read once, at start. A missing file gives the defaults. A file that is not JSON, names an unknown rule or key, repeats
//! a rule, or has a value of the wrong type or range is refused. Every refusal names a key or a position, never a value.

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::eval::{Rule, RuleId, Rules, Severity};

/// The file's name in the state directory.
pub const ALERTS_FILE: &str = "alerts.json";

/// What `alerts.json` sets: the rules, the optional shim socket that the `shim` health component reads, and the latency targets.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AlertsFile {
    pub rules: Rules,
    pub shim_socket: Option<PathBuf>,
    pub probes: Probes,
}

/// The latency targets, each a TCP connect address. A target that is not named is not probed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Probes {
    pub gateway: Option<SocketAddr>,
    pub internet: Option<SocketAddr>,
    pub relay: Option<SocketAddr>,
}

/// Why `alerts.json` was refused. The text names a key or a position, never a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RulesError {
    Io(String),
    Parse(String),
    UnknownRule(String),
}

impl fmt::Display for RulesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(why) => write!(f, "{ALERTS_FILE} cannot be read: {why}"),
            Self::Parse(why) => write!(f, "{ALERTS_FILE} is not valid: {why}"),
            Self::UnknownRule(key) => write!(f, "{ALERTS_FILE}: {key} names no rule"),
        }
    }
}

impl std::error::Error for RulesError {}

/// The file's top level. Unknown keys are refused.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FileEntry {
    #[serde(default)]
    rules: Vec<RuleEntry>,
    shim_socket: Option<PathBuf>,
    probes: Option<ProbesEntry>,
}

/// One rule as the file names it. Every field but the id is optional, and an omitted field keeps its default.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RuleEntry {
    id: String,
    enabled: Option<bool>,
    threshold: Option<f64>,
    for_s: Option<u64>,
    clear_for_s: Option<u64>,
    clear_margin: Option<f64>,
    severity: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProbesEntry {
    gateway: Option<String>,
    internet: Option<String>,
    relay: Option<String>,
}

/// Reads `<state_dir>/alerts.json`. A missing file gives [`AlertsFile::default`].
pub fn load(state_dir: &Path) -> Result<AlertsFile, RulesError> {
    match fs::read(state_dir.join(ALERTS_FILE)) {
        Ok(bytes) => parse(&bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(AlertsFile::default()),
        Err(error) => Err(RulesError::Io(error.kind().to_string())),
    }
}

/// Parses the bytes of an `alerts.json`. Rules the file names replace the default of their id, field by field; the rest keep
/// their defaults.
pub fn parse(bytes: &[u8]) -> Result<AlertsFile, RulesError> {
    let file: FileEntry = serde_json::from_slice(bytes).map_err(parse_error)?;
    let mut out = AlertsFile {
        shim_socket: file.shim_socket,
        ..AlertsFile::default()
    };
    let mut seen = BTreeSet::new();
    for (index, entry) in file.rules.into_iter().enumerate() {
        let key = format!("rules[{index}]");
        let id =
            RuleId::parse(&entry.id).ok_or_else(|| RulesError::UnknownRule(format!("{key}.id")))?;
        if !seen.insert(id) {
            return Err(RulesError::Parse(format!(
                "{key}.id repeats a rule already listed"
            )));
        }
        if let Some(rule) = out.rules.0.iter_mut().find(|rule| rule.id == id) {
            apply_entry(rule, entry, &key)?;
        }
    }
    if let Some(probes) = file.probes {
        out.probes = Probes {
            gateway: address(probes.gateway, "probes.gateway")?,
            internet: address(probes.internet, "probes.internet")?,
            relay: address(probes.relay, "probes.relay")?,
        };
    }
    Ok(out)
}

/// Writes the fields a rule entry names over `rule`, after checking their range.
fn apply_entry(rule: &mut Rule, entry: RuleEntry, key: &str) -> Result<(), RulesError> {
    if let Some(enabled) = entry.enabled {
        rule.enabled = enabled;
    }
    if let Some(threshold) = entry.threshold {
        if !threshold.is_finite() {
            return Err(out_of_range(key, "threshold"));
        }
        rule.threshold = threshold;
    }
    if let Some(for_s) = entry.for_s {
        rule.for_s = for_s;
    }
    if let Some(clear_for_s) = entry.clear_for_s {
        rule.clear_for_s = clear_for_s;
    }
    if let Some(margin) = entry.clear_margin {
        if !(margin.is_finite() && margin >= 0.0) {
            return Err(out_of_range(key, "clearMargin"));
        }
        rule.clear_margin = margin;
    }
    if let Some(severity) = entry.severity {
        rule.severity = match severity.as_str() {
            "critical" => Severity::Critical,
            "warning" => Severity::Warning,
            _ => {
                return Err(RulesError::Parse(format!(
                    "{key}.severity is not critical or warning"
                )));
            }
        };
    }
    Ok(())
}

fn out_of_range(key: &str, field: &str) -> RulesError {
    RulesError::Parse(format!("{key}.{field} is out of range"))
}

fn address(text: Option<String>, key: &str) -> Result<Option<SocketAddr>, RulesError> {
    text.map(|text| {
        text.parse()
            .map_err(|_| RulesError::Parse(format!("{key} is not an ip:port address")))
    })
    .transpose()
}

/// A JSON error with its position. serde names an unknown key in backticks, and that name is passed on. Every other data error
/// can echo a value, so its text is dropped and only the position is kept.
fn parse_error(error: serde_json::Error) -> RulesError {
    let at = format!("line {} column {}", error.line(), error.column());
    if error.is_data() {
        let text = error.to_string();
        if let Some(name) = backticked(&text, "unknown field `") {
            return RulesError::Parse(format!("unknown key `{name}` at {at}"));
        }
        if let Some(name) = backticked(&text, "missing field `") {
            return RulesError::Parse(format!("missing key `{name}` at {at}"));
        }
        return RulesError::Parse(format!("a value has the wrong type or shape at {at}"));
    }
    RulesError::Parse(format!("the text is not JSON at {at}"))
}

/// The name between `prefix` and the next backtick of `text`, when `text` starts with `prefix`.
fn backticked<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    text.strip_prefix(prefix)?.split('`').next()
}
