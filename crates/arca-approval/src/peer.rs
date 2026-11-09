//! The owner passkeys of another machine's page host, kept in sync here and trusted for that host's listed origins.
//!
//! A node config may name, under `approval.peerKeys`, the page host (`from`) whose owner passkeys this kernel keeps, and the
//! page origins it accepts from them (`origins`). The page host sends its passkey lines with `peer_keys_sync`; this kernel
//! keeps them in its own state file [`PEER_PASSKEYS_FILE`] (mode 0600, written whole and atomically, never through a link).
//! They are trusted exactly as `approval.trustOwnerKeysFrom` keys are: for action approvals only, never for sign-in, never
//! for a change to this kernel's own passkeys, and only for a line whose origin is listed.
//!
//! The file is read under the rule [`crate::foreign`] reads another kernel's file by: opened without following a link, a
//! regular file owned by root or this process's uid, with no group or other write bit, and parsed whole by
//! [`parse_passkeys`]. Its first line is a header this module writes, naming the page host and the times of the first sync
//! and of the last signed change. A file without that header, or with another page host's name, adds no key.
//!
//! Each read keeps only the file's state (a count, the times and a reason, never a key), so `arca_status` can report it
//! without reading the file more often than its window.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use sha2::{Digest as _, Sha256};

use crate::ApprovalRefusal;
use crate::foreign::{NOFOLLOW, read_trusted_bytes};
use crate::passkey::{PasskeyLine, format_passkey_line, parse_passkeys};

/// The peer passkeys file's name in the state directory.
pub const PEER_PASSKEYS_FILE: &str = "peer-passkeys";
/// The header's opening words. The whole header is `# arca peer-keys from=<host> since=<ms>[ changed=<ms>]`.
const HEADER: &str = "# arca peer-keys";
/// The most lines one set may hold.
pub const PEER_SET_MAX: usize = 64;

/// The passkeys another machine's page host keeps in sync here, the page host's name, and the origins its lines may answer
/// from. A clone shares the last read's state with the original, so the approval check and `arca_status` see one record.
#[derive(Debug, Clone)]
pub struct PeerPasskeys {
    path: PathBuf,
    from: String,
    origins: Vec<String>,
    last: Arc<Mutex<Option<(Instant, PeerState)>>>,
}

/// What the file gave at its last read: how many passkeys it added, when the first sync and the last signed change were,
/// or why it added none.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PeerState {
    /// The passkeys the file added: its lines whose origin is listed.
    pub keys: usize,
    /// When the first sync was accepted, in epoch milliseconds.
    pub since: Option<u64>,
    /// When the set last changed under a passkey approval, in epoch milliseconds.
    pub changed: Option<u64>,
    /// Why the file added no passkey, in plain words. Never a key, a credential, a line's content or a path.
    pub reason: Option<String>,
}

/// The set the file holds now: its lines in their normal form, sorted, and the time of the first sync. An empty set is a
/// file that is not there yet, or one that holds no line.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PeerSet {
    pub lines: Vec<String>,
    pub since: Option<u64>,
}

impl PeerPasskeys {
    /// Keep the passkeys page host `from` sends in the file `path`, trusted for the page origins `origins`. The config
    /// check has already refused a name that is not a host name, and an origin that is not an approval origin.
    pub fn new(path: PathBuf, from: String, origins: Vec<String>) -> Self {
        Self {
            path,
            from,
            origins,
            last: Arc::default(),
        }
    }

    /// The page host whose passkeys are kept here.
    pub fn from(&self) -> &str {
        &self.from
    }

    /// The origins a kept line must name to be trusted.
    pub fn origins(&self) -> &[String] {
        &self.origins
    }

    /// Reads the file now. Returns the passkey lines it adds (those whose origin is listed) and its state.
    pub fn load(&self) -> (Vec<PasskeyLine>, PeerState) {
        let (lines, state) = match self.read() {
            Ok(Some((lines, since, changed))) => {
                let kept: Vec<PasskeyLine> = lines
                    .into_iter()
                    .filter(|line| self.origins.contains(&line.origin))
                    .collect();
                let reason = kept
                    .is_empty()
                    .then(|| "no synced passkey answers a listed origin".to_string());
                let state = PeerState {
                    keys: kept.len(),
                    since: Some(since),
                    changed,
                    reason,
                };
                (kept, state)
            }
            Ok(None) => (
                Vec::new(),
                PeerState {
                    reason: Some(format!("no passkey has been synced from {} yet", self.from)),
                    ..PeerState::default()
                },
            ),
            Err(reason) => (
                Vec::new(),
                PeerState {
                    reason: Some(reason),
                    ..PeerState::default()
                },
            ),
        };
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) =
            Some((Instant::now(), state.clone()));
        (lines, state)
    }

    /// The state of the last read, by this value or a clone, when it is younger than `window`; otherwise the file is read
    /// now.
    pub fn recent_state(&self, window: Duration) -> PeerState {
        let recent = self
            .last
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .filter(|(at, _)| at.elapsed() < window)
            .map(|(_, state)| state.clone());
        recent.unwrap_or_else(|| self.load().1)
    }

    /// The set the file holds now, every line in its normal form whatever its origin, for a sync to compare with. A file
    /// that is not there, or one whose header names another page host, is the empty set: the next sync is a first sync. A
    /// file that cannot be trusted is refused with its plain reason, so a bad file is never taken for an empty one.
    pub fn current(&self) -> Result<PeerSet, ApprovalRefusal> {
        match self.read() {
            Ok(Some((lines, since, _))) => Ok(PeerSet {
                lines: normal_lines(&lines)?,
                since: Some(since),
            }),
            Ok(None) => Ok(PeerSet::default()),
            Err(reason) if reason == self.other_host() => Ok(PeerSet::default()),
            Err(reason) => Err(ApprovalRefusal::Untrusted(format!(
                "the peer passkeys file cannot be trusted: {reason}"
            ))),
        }
    }

    /// Replaces the file with `lines` (already in normal form and sorted), recording the first sync at `since` and the last
    /// signed change at `changed`. The new file is written beside the old one at mode 0600 without following a link, synced,
    /// and renamed over it; the directory is synced after.
    pub fn write(&self, lines: &[String], since: u64, changed: Option<u64>) -> std::io::Result<()> {
        let dir = self
            .path
            .parent()
            .ok_or_else(|| std::io::Error::other("the peer passkeys file has no directory"))?;
        let name = self
            .path
            .file_name()
            .ok_or_else(|| std::io::Error::other("the peer passkeys file has no name"))?
            .to_string_lossy()
            .into_owned();
        let mut body = format!("{HEADER} from={} since={since}", self.from);
        if let Some(changed) = changed {
            body.push_str(&format!(" changed={changed}"));
        }
        body.push('\n');
        for line in lines {
            body.push_str(line);
            body.push('\n');
        }
        let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
        let _ = fs::remove_file(&tmp);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(NOFOLLOW)
            .open(&tmp)?;
        let written = file
            .write_all(body.as_bytes())
            .and_then(|()| file.sync_all());
        drop(file);
        if let Err(error) = written.and_then(|()| fs::rename(&tmp, &self.path)) {
            let _ = fs::remove_file(&tmp);
            return Err(error);
        }
        fs::File::open(dir)?.sync_all()
    }

    /// The reason a file synced from another page host is not trusted.
    fn other_host(&self) -> String {
        "the file was synced from another page host".to_string()
    }

    /// The file's lines, its first-sync time and its last change, or `None` when it is not there. A refused file is its
    /// plain reason.
    #[allow(clippy::type_complexity)]
    fn read(&self) -> Result<Option<(Vec<PasskeyLine>, u64, Option<u64>)>, String> {
        let bytes = match read_trusted_bytes(&self.path) {
            Ok(bytes) => bytes,
            Err(reason) if reason == "the file is missing" => return Ok(None),
            Err(reason) => return Err(reason),
        };
        if bytes.is_empty() {
            return Ok(None);
        }
        let first = bytes.split(|b| *b == b'\n').next().unwrap_or_default();
        let (from, since, changed) = std::str::from_utf8(first)
            .ok()
            .and_then(parse_header)
            .ok_or_else(|| "the file does not start with Arca's peer-keys header".to_string())?;
        if from != self.from {
            return Err(self.other_host());
        }
        let lines = parse_passkeys(&bytes).map_err(|refusal| match refusal {
            ApprovalRefusal::Untrusted(why) => why,
            other => other.to_string(),
        })?;
        Ok(Some((lines, since, changed)))
    }
}

/// The page host, the first-sync time and the last change a header names, if the line is exactly a header.
fn parse_header(line: &str) -> Option<(&str, u64, Option<u64>)> {
    let rest = line.strip_prefix(HEADER)?.strip_prefix(" from=")?;
    let (from, rest) = rest.split_once(" since=")?;
    let (since, changed) = match rest.split_once(" changed=") {
        Some((since, changed)) => (since, Some(changed)),
        None => (rest, None),
    };
    let number = |text: &str| -> Option<u64> {
        (!text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()))
            .then(|| text.parse().ok())
            .flatten()
    };
    let since = number(since)?;
    let changed = match changed {
        Some(text) => Some(number(text)?),
        None => None,
    };
    let plain = !from.is_empty()
        && from
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    plain.then_some((from, since, changed))
}

/// One parsed line in its normal form: the shape [`format_passkey_line`] writes, with no comment.
fn normal_line(line: &PasskeyLine) -> Result<String, ApprovalRefusal> {
    format_passkey_line(
        &line.rp_id,
        &line.origin,
        &URL_SAFE_NO_PAD.encode(&line.credential_id),
        &STANDARD.encode(&line.spki),
    )
}

/// Parsed passkey lines in their normal form, sorted: what a page host sends with `peer_keys_sync`. No comment, label or
/// date leaves the host.
pub fn normal_lines(lines: &[PasskeyLine]) -> Result<Vec<String>, ApprovalRefusal> {
    let mut out = lines
        .iter()
        .map(normal_line)
        .collect::<Result<Vec<_>, _>>()?;
    out.sort();
    Ok(out)
}

/// A sent set in its normal form: each entry must be exactly one passkey line for Arca's namespace, no two may share a
/// credential or a key, and there are at most [`PEER_SET_MAX`]. The lines come back without comments, sorted. No refusal
/// echoes a line.
pub fn normalize_set(lines: &[String]) -> Result<Vec<String>, ApprovalRefusal> {
    let refused = |why: &str| ApprovalRefusal::Malformed(format!("the passkey list {why}"));
    if lines.len() > PEER_SET_MAX {
        return Err(refused("holds more than 64 lines"));
    }
    let mut body = String::new();
    for line in lines {
        if line.contains(['\n', '\r']) {
            return Err(refused("holds an entry that is more than one line"));
        }
        body.push_str(line);
        body.push('\n');
    }
    let parsed = parse_passkeys(body.as_bytes()).map_err(|_| {
        refused("holds an entry that is not a passkey line, or repeats a credential or key")
    })?;
    if parsed.len() != lines.len() {
        return Err(refused(
            "holds an entry that is blank, a comment or not for Arca's namespace",
        ));
    }
    normal_lines(&parsed)
}

/// The SHA-256 (lowercase hex) of a set in normal form: each line followed by a newline, in sorted order. A peer-keys
/// statement names it.
pub fn set_digest(lines: &[String]) -> String {
    let mut hash = Sha256::new();
    for line in lines {
        hash.update(line.as_bytes());
        hash.update(b"\n");
    }
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The statement target of a change to the set whose digest is `digest`.
pub fn peer_target(digest: &str) -> String {
    format!("peer-keys-{digest}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_header_is_read_exactly() {
        assert_eq!(
            parse_header("# arca peer-keys from=alpha since=12"),
            Some(("alpha", 12, None))
        );
        assert_eq!(
            parse_header("# arca peer-keys from=alpha.example since=12 changed=13"),
            Some(("alpha.example", 12, Some(13)))
        );
        for bad in [
            "# arca peer-keys from= since=12",
            "# arca peer-keys from=alpha since=",
            "# arca peer-keys from=alpha since=1x",
            "# arca peer-keys from=al pha since=1",
            "# arca peer-keys from=alpha since=1 changed=",
            "# other",
        ] {
            assert_eq!(parse_header(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_digest_is_over_sorted_lines_each_ending_in_a_newline() {
        let digest = set_digest(&["a".to_string(), "b".to_string()]);
        let mut hash = Sha256::new();
        hash.update(b"a\nb\n");
        let want: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(digest, want);
        assert_eq!(peer_target(&digest).len(), "peer-keys-".len() + 64);
    }
}
