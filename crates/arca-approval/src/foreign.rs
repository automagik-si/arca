//! Owner passkeys another kernel on this machine keeps, trusted here read-only for the origins the config lists.
//!
//! A node config may name passkeys files that belong to another kernel on the same machine (`approval.trustOwnerKeysFrom`)
//! and the page origins it accepts from them (`approval.origins`). Each file is opened read-only, without following a link,
//! every time an approval is checked, so a passkey the owner enrols on the other kernel is trusted here without a restart,
//! and one the owner removes there stops verifying here. Nothing here writes or copies a file, and no key is cached.
//!
//! Each read keeps only the files' states (a count and a reason, never a key). `arca_status` reports those through
//! [`ForeignPasskeys::recent_states`], which re-reads the files only when the last read is older than its window, so status
//! calls never read the files more often than that.
//!
//! A file is read only when it is a regular file owned by root or by this process's uid, with no group or other write bit,
//! and parses as a whole under [`parse_passkeys`]. A line is kept only when its origin is one of the listed origins. A file
//! that is missing, unreadable or refused adds no key (fail closed), and its plain reason is reported for `arca_status`; the
//! other files and this kernel's own passkeys are unaffected.

use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::ApprovalRefusal;
use crate::passkey::{PasskeyLine, parse_passkeys};
use crate::signers::effective_uid;

/// `O_NOFOLLOW` on Linux: the open fails on a symbolic link instead of following it.
#[cfg(target_os = "linux")]
const NOFOLLOW: i32 = 0o400_000;
#[cfg(not(target_os = "linux"))]
const NOFOLLOW: i32 = 0;
/// The largest trusted passkeys file read, in bytes.
const MAX_BYTES: u64 = 1024 * 1024;

/// The passkeys files of other kernels this kernel also trusts, and the page origins it accepts from them. A clone shares
/// the last read's states with the original, so the approval check and `arca_status` see one record.
#[derive(Debug, Clone, Default)]
pub struct ForeignPasskeys {
    sources: Vec<PathBuf>,
    origins: Vec<String>,
    last: Arc<Mutex<Option<LastRead>>>,
}

/// When the files were last read, and each file's state then.
type LastRead = (Instant, Vec<SourceState>);

/// What one trusted file gave at its last read: how many passkeys it added, or why it added none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceState {
    pub path: PathBuf,
    /// The passkeys this file added: its lines for Arca's namespace whose origin is listed.
    pub keys: usize,
    /// Why the file added no passkey, in plain words. Never a key, a credential or a line's content.
    pub reason: Option<String>,
}

impl ForeignPasskeys {
    /// Trust the passkeys files `sources` for the page origins `origins`. The config check has already refused an origin
    /// that is not an approval origin, and a source list without an origin.
    pub fn new(sources: Vec<PathBuf>, origins: Vec<String>) -> Self {
        Self {
            sources,
            origins,
            last: Arc::default(),
        }
    }

    /// Whether no file is named, so this trusts nothing more than the kernel's own passkeys.
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    /// The origins a trusted line must name.
    pub fn origins(&self) -> &[String] {
        &self.origins
    }

    /// Reads every file now. Returns the passkey lines they add, in file order with no credential or key twice, and each
    /// file's state. A file that cannot be trusted adds nothing and names its reason.
    pub fn load(&self) -> (Vec<PasskeyLine>, Vec<SourceState>) {
        let mut lines: Vec<PasskeyLine> = Vec::new();
        let mut states = Vec::with_capacity(self.sources.len());
        for path in &self.sources {
            match read_source(path) {
                Ok(read) => {
                    let mut added = 0;
                    for line in read {
                        if !self.origins.contains(&line.origin) {
                            continue;
                        }
                        if lines.iter().any(|seen| {
                            seen.credential_id == line.credential_id
                                || seen.fingerprint == line.fingerprint
                        }) {
                            continue;
                        }
                        lines.push(line);
                        added += 1;
                    }
                    let reason = (added == 0)
                        .then(|| "no passkey in it answers a listed origin".to_string());
                    states.push(SourceState {
                        path: path.clone(),
                        keys: added,
                        reason,
                    });
                }
                Err(reason) => states.push(SourceState {
                    path: path.clone(),
                    keys: 0,
                    reason: Some(reason),
                }),
            }
        }
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) =
            Some((Instant::now(), states.clone()));
        (lines, states)
    }

    /// Each file's state from the last read, by this value or a clone of it, when that read is younger than `window`;
    /// otherwise the files are read now. An approval check is such a read, so a status call right after one reports what
    /// that check saw.
    pub fn recent_states(&self, window: Duration) -> Vec<SourceState> {
        let recent = self
            .last
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .filter(|(at, _)| at.elapsed() < window)
            .map(|(_, states)| states.clone());
        recent.unwrap_or_else(|| self.load().1)
    }
}

/// The lines of one trusted file, or the plain reason it is not read. It is opened read-only without following a link,
/// and its owner and mode are checked on the open file, so the bytes read are the bytes checked.
fn read_source(path: &Path) -> Result<Vec<PasskeyLine>, String> {
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err("the file is missing".to_string());
        }
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err("the file cannot be read by this kernel".to_string());
        }
        Err(_) => return Err("the file cannot be opened, or it is a link".to_string()),
    };
    let meta = file
        .metadata()
        .map_err(|_| "the file cannot be read by this kernel".to_string())?;
    if !meta.is_file() {
        return Err("the file is not a regular file".to_string());
    }
    let uid = effective_uid().map_err(|refusal| refusal.to_string())?;
    if meta.uid() != 0 && meta.uid() != uid {
        return Err("the file is owned by neither root nor this kernel's user".to_string());
    }
    if meta.mode() & 0o022 != 0 {
        return Err("the file is writable by group or others".to_string());
    }
    if meta.len() > MAX_BYTES {
        return Err("the file is larger than a passkeys file can be".to_string());
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "the file cannot be read by this kernel".to_string())?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err("the file is larger than a passkeys file can be".to_string());
    }
    parse_passkeys(&bytes).map_err(|refusal| match refusal {
        ApprovalRefusal::Untrusted(why) => why,
        other => other.to_string(),
    })
}
