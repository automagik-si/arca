//! The declared state: one root-owned JSON file.
//!
//! ```json
//! { "forwarding": true, "bridges": ["incusbr0"], "forwardPolicy": "DROP" }
//! ```
//!
//! `forwarding` is whether the host forwards packets. `bridges` are the bridges whose containers may reach out. `forwardPolicy`
//! is the policy the filter table's `FORWARD` chain is expected to have, which the takeover records from the host; without it
//! the policy is not watched. The root part refuses a file that root does not own or that anyone else can write, since what
//! it holds becomes firewall rules.

use std::fmt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// The most bridges one declared file lists.
pub const MAX_BRIDGES: usize = 32;
/// The most bytes a declared file may hold.
const MAX_BYTES: u64 = 64 * 1024;

/// The policy of the `FORWARD` chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Policy {
    Accept,
    Drop,
}

impl Policy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Accept => "ACCEPT",
            Self::Drop => "DROP",
        }
    }

    pub fn parse(text: &str) -> Option<Policy> {
        match text {
            "ACCEPT" => Some(Self::Accept),
            "DROP" => Some(Self::Drop),
            _ => None,
        }
    }
}

/// What the owner declares about the host's forwarding and firewall.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Declared {
    pub forwarding: bool,
    #[serde(default)]
    pub bridges: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forward_policy: Option<Policy>,
}

/// Why a declared file is refused.
#[derive(Debug, PartialEq, Eq)]
pub enum DeclaredError {
    /// There is no file.
    Missing,
    /// The file is there and is refused, with the reason.
    Refused(String),
}

impl fmt::Display for DeclaredError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => f.write_str("there is no declared state"),
            Self::Refused(why) => f.write_str(why),
        }
    }
}

/// Whether `name` is a bridge name the kernel accepts and no command line can misread: 1 to 15 characters of letters,
/// digits, `.`, `_` and `-`, not starting with `-` or `.`.
pub fn valid_bridge(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 15
        && !name.starts_with(['-', '.'])
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

impl Declared {
    /// Reads declared state from the bytes of a file. Unknown fields, an invalid or repeated bridge name and too many
    /// bridges are refused.
    pub fn parse(bytes: &[u8]) -> Result<Declared, DeclaredError> {
        let declared: Declared = serde_json::from_slice(bytes).map_err(|error| {
            DeclaredError::Refused(format!("the declared file is refused: {error}"))
        })?;
        if declared.bridges.len() > MAX_BRIDGES {
            return Err(DeclaredError::Refused(format!(
                "the declared file lists more than {MAX_BRIDGES} bridges"
            )));
        }
        for (index, bridge) in declared.bridges.iter().enumerate() {
            if !valid_bridge(bridge) {
                return Err(DeclaredError::Refused(
                    "the declared file names a bridge that is not a bridge name: 1 to 15 letters, digits, '.', '_' or '-', not starting with '-' or '.'".to_string(),
                ));
            }
            if declared.bridges[..index].contains(bridge) {
                return Err(DeclaredError::Refused(
                    "the declared file names a bridge twice".to_string(),
                ));
            }
        }
        Ok(declared)
    }

    /// The declared state as the file holds it.
    pub fn to_json(&self) -> String {
        let mut text = serde_json::to_string_pretty(self).unwrap_or_default();
        text.push('\n');
        text
    }
}

/// Reads the declared file at `path`. The file must be a regular file nobody but its owner can write, and when
/// `trusted_owner` is set, owned by that user: the root part passes root.
pub fn load(path: &Path, trusted_owner: Option<u32>) -> Result<Declared, DeclaredError> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(DeclaredError::Missing);
        }
        Err(error) => {
            return Err(DeclaredError::Refused(format!(
                "the declared file cannot be read: {error}"
            )));
        }
    };
    if !meta.file_type().is_file() {
        return Err(DeclaredError::Refused(
            "the declared file is not a regular file".to_string(),
        ));
    }
    if meta.mode() & 0o022 != 0 {
        return Err(DeclaredError::Refused(
            "the declared file can be written by someone besides its owner".to_string(),
        ));
    }
    if trusted_owner.is_some_and(|owner| meta.uid() != owner) {
        return Err(DeclaredError::Refused(
            "the declared file is not owned by the user it must belong to".to_string(),
        ));
    }
    if meta.len() > MAX_BYTES {
        return Err(DeclaredError::Refused(
            "the declared file is too large".to_string(),
        ));
    }
    let bytes = std::fs::read(path).map_err(|error| {
        DeclaredError::Refused(format!("the declared file cannot be read: {error}"))
    })?;
    Declared::parse(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "arca-hostnet-declared-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    #[test]
    fn a_declared_file_parses_and_reads_back_the_same() {
        let declared = Declared::parse(
            br#"{"forwarding":true,"bridges":["incusbr0","br-test.1"],"forwardPolicy":"DROP"}"#,
        )
        .expect("parses");
        assert!(declared.forwarding);
        assert_eq!(declared.bridges, ["incusbr0", "br-test.1"]);
        assert_eq!(declared.forward_policy, Some(Policy::Drop));
        assert_eq!(Declared::parse(declared.to_json().as_bytes()), Ok(declared));
        // Bridges and the policy are optional.
        let bare = Declared::parse(br#"{"forwarding":false}"#).expect("parses");
        assert!(bare.bridges.is_empty() && bare.forward_policy.is_none());
    }

    #[test]
    fn a_declared_file_that_could_be_misread_is_refused() {
        for bad in [
            r#"{"forwarding":true,"extra":1}"#,
            r#"{"bridges":[]}"#,
            r#"{"forwarding":"yes"}"#,
            r#"{"forwarding":true,"forwardPolicy":"REJECT"}"#,
            r#"{"forwarding":true,"bridges":["a b"]}"#,
            r#"{"forwarding":true,"bridges":[""]}"#,
            r#"{"forwarding":true,"bridges":["-j"]}"#,
            r#"{"forwarding":true,"bridges":["../x"]}"#,
            r#"{"forwarding":true,"bridges":["sixteencharsbrid"]}"#,
            r#"{"forwarding":true,"bridges":["incusbr0","incusbr0"]}"#,
            "not json",
        ] {
            assert!(
                matches!(
                    Declared::parse(bad.as_bytes()),
                    Err(DeclaredError::Refused(_))
                ),
                "{bad} is refused"
            );
        }
        let many: Vec<String> = (0..=MAX_BRIDGES).map(|n| format!("br{n}")).collect();
        let text = serde_json::json!({"forwarding": true, "bridges": many}).to_string();
        assert!(matches!(
            Declared::parse(text.as_bytes()),
            Err(DeclaredError::Refused(_))
        ));
    }

    #[test]
    fn the_file_must_be_there_and_nobody_but_its_owner_may_write_it() {
        let dir = scratch("load");
        let path = dir.join("host.json");
        assert_eq!(load(&path, None), Err(DeclaredError::Missing));
        std::fs::write(&path, r#"{"forwarding":true}"#).expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("mode");
        assert!(load(&path, None).is_ok());
        // Another owner than the one trusted.
        let me = std::fs::metadata(&path).expect("meta").uid();
        assert!(load(&path, Some(me)).is_ok());
        assert!(matches!(
            load(&path, Some(me + 1)),
            Err(DeclaredError::Refused(_))
        ));
        // A file a group or anyone else can write.
        for mode in [0o664, 0o646] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("mode");
            assert!(
                matches!(load(&path, None), Err(DeclaredError::Refused(_))),
                "{mode:o}"
            );
        }
        // A link is not followed to a file elsewhere.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("mode");
        let link = dir.join("link.json");
        std::os::unix::fs::symlink(&path, &link).expect("link");
        assert!(matches!(load(&link, None), Err(DeclaredError::Refused(_))));
        let _ = std::fs::remove_dir_all(dir);
    }
}
