//! A template's `appUpdate`: the formula that updates the app inside a service, read strictly when the template is read.
//! Nothing here runs a command. It fixes the shape: which kind of template takes which part, the limits of each part,
//! the plain version a call may name, and the argument list that version makes of the formula's update command.
//!
//! The construction keeps a version one plain argument. A command is a list of arguments whose first is an absolute
//! path, never one line, so no shell reads it. `{version}` stands in exactly one argument of the update command, never
//! the first, so a version never picks the program and never becomes a second argument. A version is 1 to 64 bytes of
//! ASCII letters, digits and `.` `_` `+` `~` `:` `-`, starting with a letter or a digit, so it is never read as an
//! option. What the named program does with its arguments is the template's: a template that hands the version to a
//! program which runs it (`env`, a shell with `-c`) has given it that power, and no rule here refuses that.

use serde::{Deserialize, Serialize};
use std::ops::RangeInclusive;

/// The one placeholder a formula has: where the version stands in the update command.
pub const VERSION_PLACEHOLDER: &str = "{version}";
/// At most this many elements in a command, the program among them.
pub const COMMAND_MAX_ELEMENTS: usize = 32;
/// At most this many bytes in one element of a command.
pub const ELEMENT_MAX_BYTES: usize = 512;
/// At most this many bytes in the whole field, as compact JSON.
pub const FIELD_MAX_BYTES: usize = 8 * 1024;
/// At most this many bytes in a version.
pub const VERSION_MAX_BYTES: usize = 64;
/// How long the update command may run. No default: the template's author knows the app.
pub const UPDATE_SECONDS: RangeInclusive<u64> = 10..=7200;
/// How long health has to come, in all: the range and default of `service_update`'s `timeoutSeconds`.
pub const HEALTH_SECONDS: RangeInclusive<u64> = 1..=900;
pub const HEALTH_SECONDS_DEFAULT: u64 = 120;
/// How long the version reader may run.
pub const VERSION_SECONDS: RangeInclusive<u64> = 1..=60;
pub const VERSION_SECONDS_DEFAULT: u64 = 15;

/// A template's `appUpdate`. An OCI (by-digest) template has `health` and `version`, since its update is the move to
/// another image digest. A system container template has `update`, `health` and `version`. No other kind has the field.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AppUpdate {
    /// The command that moves the app to a version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<Step>,
    /// The command whose exit code 0 says the app is healthy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<Step>,
    /// The command that prints the app's version as the first line of its output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<Step>,
}

/// One part of a formula: a command as a list of arguments, and how long it may take.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seconds: Option<u64>,
}

/// A version a call may name: checked once ([`version_ok`]), so the replacement takes no other text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppVersion(String);

impl AppVersion {
    /// The version, or the plain words of why it is not one.
    pub fn parse(text: &str) -> Result<Self, String> {
        if version_ok(text) {
            Ok(Self(text.to_owned()))
        } else {
            Err(format!(
                "version: 1 to {VERSION_MAX_BYTES} bytes, each an ASCII letter, a digit, or one of . _ + ~ : -, the first a letter or a digit"
            ))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Whether `text` is a plain version: 1 to 64 bytes, the first an ASCII letter or digit, every other one an ASCII letter,
/// a digit, or one of `.` `_` `+` `~` `:` `-`. Such a version holds no space, no quote, no `$` and no `;`, and a version
/// that fills a whole argument is never read as an option. It covers a package version with an epoch and a revision,
/// such as `5:27.0.3-1~ubuntu.24.04`.
pub fn version_ok(text: &str) -> bool {
    let bytes = text.as_bytes();
    let Some((first, rest)) = bytes.split_first() else {
        return false;
    };
    bytes.len() <= VERSION_MAX_BYTES
        && first.is_ascii_alphanumeric()
        && rest.iter().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'~' | b':' | b'-')
        })
}

impl AppUpdate {
    /// Whether the formula fits a template of `kind`, and every part its limits: `Err` with the plain words of what is
    /// wrong, naming the field.
    pub fn check(&self, kind: &str) -> Result<(), String> {
        match (kind, &self.update) {
            ("oci", Some(_)) => {
                return Err(
                    "appUpdate.update is only for a system container template: an OCI template's update is the move to another image digest".into(),
                );
            }
            ("system", None) => {
                return Err(
                    "appUpdate.update is required on a system container template: the command that moves the app to a version".into(),
                );
            }
            ("oci" | "system", _) => {}
            _ => {
                return Err("appUpdate is only for an OCI or a system container template".into());
            }
        }
        let Some(health) = &self.health else {
            return Err("appUpdate.health is required: the command whose exit code 0 says the app is healthy".into());
        };
        let Some(version) = &self.version else {
            return Err(
                "appUpdate.version is required: the command that prints the app's version".into(),
            );
        };
        if let Some(update) = &self.update {
            check_command("update", &update.command)?;
            let occurrences: usize = update
                .command
                .iter()
                .map(|element| element.matches(VERSION_PLACEHOLDER).count())
                .sum();
            if occurrences != 1 || update.command[0].contains(VERSION_PLACEHOLDER) {
                return Err(format!(
                    "appUpdate.update.command holds {VERSION_PLACEHOLDER} exactly once, in one argument that is not the first"
                ));
            }
            if !update.seconds.is_some_and(|s| UPDATE_SECONDS.contains(&s)) {
                return Err(format!(
                    "appUpdate.update.seconds is required, from {} to {}",
                    UPDATE_SECONDS.start(),
                    UPDATE_SECONDS.end()
                ));
            }
        }
        for (part, step, range) in [
            ("health", health, HEALTH_SECONDS),
            ("version", version, VERSION_SECONDS),
        ] {
            check_command(part, &step.command)?;
            if step
                .command
                .iter()
                .any(|element| element.contains(VERSION_PLACEHOLDER))
            {
                return Err(format!(
                    "appUpdate.{part}.command does not hold {VERSION_PLACEHOLDER}: only update.command does"
                ));
            }
            if step.seconds.is_some_and(|s| !range.contains(&s)) {
                return Err(format!(
                    "appUpdate.{part}.seconds is from {} to {}",
                    range.start(),
                    range.end()
                ));
            }
        }
        let size = serde_json::to_vec(self).map_or(usize::MAX, |bytes| bytes.len());
        if size > FIELD_MAX_BYTES {
            return Err(format!(
                "appUpdate is at most {FIELD_MAX_BYTES} bytes as compact JSON, and this one is {size}"
            ));
        }
        Ok(())
    }

    /// How long the update command may run, from a checked formula: `None` for a formula with no update part.
    pub fn update_seconds(&self) -> Option<u64> {
        self.update.as_ref()?.seconds
    }

    /// How long health has to come, in all: the formula's own or 120.
    pub fn health_seconds(&self) -> u64 {
        self.health
            .as_ref()
            .and_then(|step| step.seconds)
            .unwrap_or(HEALTH_SECONDS_DEFAULT)
    }

    /// How long the version reader may run: the formula's own or 15.
    pub fn version_seconds(&self) -> u64 {
        self.version
            .as_ref()
            .and_then(|step| step.seconds)
            .unwrap_or(VERSION_SECONDS_DEFAULT)
    }

    /// The update command of a checked formula as the argument list Arca would start, with `{version}` replaced
    /// inside its one argument: as many elements as the formula's, the first unchanged. `None` for a formula with no
    /// update part. Arca builds no command line from it and no shell reads it.
    pub fn update_arguments(&self, version: &AppVersion) -> Option<Vec<String>> {
        let update = self.update.as_ref()?;
        Some(
            update
                .command
                .iter()
                .enumerate()
                .map(|(at, element)| {
                    if at == 0 {
                        element.clone()
                    } else {
                        element.replacen(VERSION_PLACEHOLDER, version.as_str(), 1)
                    }
                })
                .collect(),
        )
    }
}

/// A command's list limits: 1 to 32 elements, each 1 to 512 bytes of plain text, the first an absolute path.
fn check_command(part: &str, command: &[String]) -> Result<(), String> {
    if command.is_empty() || command.len() > COMMAND_MAX_ELEMENTS {
        return Err(format!(
            "appUpdate.{part}.command is a list of 1 to {COMMAND_MAX_ELEMENTS} arguments"
        ));
    }
    if !command.iter().all(|element| {
        !element.is_empty()
            && element.len() <= ELEMENT_MAX_BYTES
            && crate::service::plain_text(element)
    }) {
        return Err(format!(
            "appUpdate.{part}.command: each argument is 1 to {ELEMENT_MAX_BYTES} bytes of plain text, with no control or hidden character"
        ));
    }
    if !command[0].starts_with('/') {
        return Err(format!(
            "appUpdate.{part}.command: the first argument is the absolute path of the program, such as /usr/bin/curl"
        ));
    }
    Ok(())
}
