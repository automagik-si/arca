//! The status file: what the root part found the last time it read the host, for the user-level node to read.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// One thing that is not as declared. `item` is its short name and is what an alert carries; `detail` says it in words.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriftItem {
    pub item: String,
    pub detail: String,
}

/// What the root part found, and did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    /// When it was read, in epoch milliseconds.
    pub at: u64,
    /// Whether a valid declared file was read.
    pub declared: bool,
    /// True when a declared file was read and the host is as it says.
    pub in_sync: bool,
    /// What is not as declared, after the apply when one ran.
    pub drift: Vec<DriftItem>,
    /// The commands an apply ran, in order.
    pub applied: Vec<String>,
    /// What went wrong while reading or applying.
    pub errors: Vec<String>,
    /// The version of the binary that wrote this.
    pub version: String,
}

/// Writes `status` to `path` as one JSON file, atomically, readable by everyone and writable by its owner only.
pub fn write(path: &Path, status: &Status) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut text = serde_json::to_string(status).map_err(io::Error::other)?;
    text.push('\n');
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = std::path::PathBuf::from(temporary);
    let _ = std::fs::remove_file(&temporary);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&temporary)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o644))?;
    std::fs::rename(&temporary, path)
}

/// Reads a status file. `None` when there is none or it is not a status.
pub fn read(path: &Path) -> Option<Status> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Status {
        Status {
            at: 1_000,
            declared: true,
            in_sync: false,
            drift: vec![DriftItem {
                item: "ipv4-forwarding".to_string(),
                detail: "forwarding is off".to_string(),
            }],
            applied: vec!["sysctl -w net.ipv4.ip_forward=1".to_string()],
            errors: Vec::new(),
            version: "0.0.0".to_string(),
        }
    }

    #[test]
    fn a_status_is_written_atomically_world_readable_and_reads_back() {
        let dir = std::env::temp_dir().join(format!("arca-hostnet-status-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        let path = dir.join("status.json");
        assert_eq!(read(&path), None);
        write(&path, &sample()).expect("writes");
        assert_eq!(read(&path), Some(sample()));
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
        // No temporary file is left, and a second write replaces the first.
        let mut newer = sample();
        newer.at = 2_000;
        write(&path, &newer).expect("writes again");
        assert_eq!(read(&path).map(|status| status.at), Some(2_000));
        assert_eq!(std::fs::read_dir(&dir).expect("dir").count(), 1);
        // A file that is not a status reads as none.
        std::fs::write(&path, "[]").expect("junk");
        assert_eq!(read(&path), None);
        let _ = std::fs::remove_dir_all(dir);
    }
}
