//! The spent-nonce file: each spend is one JSON line, written and fsynced before the spend returns, so the action an
//! approval admits can run only after its nonce is durable.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use serde_json::Value;

use crate::ApprovalRefusal;
use crate::signers::effective_uid;
use crate::statement::valid_hex;
use crate::verifier::NonceStore;

/// The file's mode: owner read and write only.
const FILE_MODE: u32 = 0o600;

/// Spent nonces, kept in a file that survives a restart.
///
/// A record is `{"at":<epoch ms>,"nonce":"<hex>"}` and a newline. A spend returns only after the record is written and
/// fsynced. A record without its newline was never acknowledged, so opening the file drops it and its nonce is not spent.
/// The file is locked for the life of the store, so two kernels cannot spend from one file.
pub struct FileNonces {
    file: File,
    spent: BTreeSet<String>,
    /// Set once a write or fsync fails. Such a store spends nothing more, because the file's state is then unknown.
    failed: bool,
}

impl FileNonces {
    /// Opens the spent-nonce file at `path`, creating it mode 0600 if it does not exist.
    ///
    /// Refuses a symbolic link, a file that is not a regular file owned by this uid with mode exactly 0600, or a file that
    /// another open store holds. Refuses a complete record that is not a spent-nonce record, or a nonce spent twice.
    ///
    /// The directory that holds the file is not checked. The caller keeps the state directory at mode 0700.
    pub fn open(path: &Path) -> Result<Self, ApprovalRefusal> {
        let existed = match fs::symlink_metadata(path) {
            Ok(meta) => {
                if meta.file_type().is_symlink() || !meta.is_file() {
                    return Err(ApprovalRefusal::Untrusted(format!(
                        "the nonce file {} is not a regular file",
                        path.display()
                    )));
                }
                true
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => false,
            Err(e) => {
                return Err(storage(format!(
                    "the nonce file cannot be read ({})",
                    e.kind()
                )));
            }
        };

        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .mode(FILE_MODE)
            .open(path)
            .map_err(|e| storage(format!("the nonce file cannot be opened ({})", e.kind())))?;
        let opened = file
            .metadata()
            .map_err(|e| storage(format!("the nonce file cannot be inspected ({})", e.kind())))?;
        let linked = fs::symlink_metadata(path)
            .map_err(|e| storage(format!("the nonce file cannot be inspected ({})", e.kind())))?;
        if linked.file_type().is_symlink()
            || (linked.dev(), linked.ino()) != (opened.dev(), opened.ino())
        {
            return Err(ApprovalRefusal::Untrusted(format!(
                "the nonce file {} changed while it was opened",
                path.display()
            )));
        }
        if !opened.is_file() {
            return Err(ApprovalRefusal::Untrusted(format!(
                "the nonce file {} is not a regular file",
                path.display()
            )));
        }
        if opened.uid() != effective_uid()? {
            return Err(ApprovalRefusal::Untrusted(format!(
                "the nonce file {} is not owned by this kernel's uid",
                path.display()
            )));
        }
        if opened.mode() & 0o777 != FILE_MODE {
            return Err(ApprovalRefusal::Untrusted(format!(
                "the nonce file {} is not mode 0600",
                path.display()
            )));
        }
        if !existed {
            sync_parent(path).map_err(|e| {
                storage(format!(
                    "the nonce file's directory cannot be synced ({})",
                    e.kind()
                ))
            })?;
        }
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(storage(
                    "the nonce file is held by another store".to_string(),
                ));
            }
            Err(TryLockError::Error(e)) => {
                return Err(storage(format!(
                    "the nonce file cannot be locked ({})",
                    e.kind()
                )));
            }
        }

        let mut bytes = Vec::new();
        (&file)
            .read_to_end(&mut bytes)
            .map_err(|e| storage(format!("the nonce file cannot be read ({})", e.kind())))?;
        let complete = bytes
            .iter()
            .rposition(|b| *b == b'\n')
            .map_or(0, |at| at + 1);
        let mut spent = BTreeSet::new();
        let body = &bytes[..complete];
        if let Some(records) = body.strip_suffix(b"\n") {
            for (index, line) in records.split(|b| *b == b'\n').enumerate() {
                let nonce = parse_record(line).ok_or_else(|| {
                    storage(format!("line {} is not a spent-nonce record", index + 1))
                })?;
                if !spent.insert(nonce) {
                    return Err(storage(format!("line {} repeats a spent nonce", index + 1)));
                }
            }
        }
        if complete < bytes.len() {
            // The unterminated tail is a record whose spend never returned, so nothing was allowed on its account.
            file.set_len(complete as u64)
                .and_then(|()| file.sync_all())
                .map_err(|e| {
                    storage(format!("the torn record cannot be dropped ({})", e.kind()))
                })?;
        }
        Ok(Self {
            file,
            spent,
            failed: false,
        })
    }
}

impl NonceStore for FileNonces {
    fn spent(&self, nonce: &str) -> bool {
        self.spent.contains(nonce)
    }

    /// Writes and fsyncs the nonce's record, then remembers it. Refuses a nonce that is not 32 to 64 lowercase hex digits
    /// before anything is written, and refuses a nonce already spent. A failed write or fsync leaves the store failed.
    fn spend(&mut self, nonce: &str, at_ms: u64) -> Result<(), ApprovalRefusal> {
        if !valid_hex(nonce, 32, 64) {
            return Err(ApprovalRefusal::Malformed(
                "the nonce is not 32 to 64 lowercase hex digits".to_string(),
            ));
        }
        if self.failed {
            return Err(storage(
                "an earlier write failed, so this store spends nothing more; restart the kernel"
                    .to_string(),
            ));
        }
        if self.spent.contains(nonce) {
            return Err(ApprovalRefusal::Replayed);
        }
        let record = format!("{}\n", record_text(nonce, at_ms));
        let durable = self
            .file
            .write_all(record.as_bytes())
            .and_then(|()| self.file.sync_all());
        if let Err(e) = durable {
            self.failed = true;
            return Err(storage(format!(
                "the spend cannot be written and synced ({})",
                e.kind()
            )));
        }
        self.spent.insert(nonce.to_string());
        Ok(())
    }
}

fn storage(why: String) -> ApprovalRefusal {
    ApprovalRefusal::Storage(why)
}

/// A record's text without its newline, with keys in sorted order.
fn record_text(nonce: &str, at_ms: u64) -> String {
    format!("{{\"at\":{at_ms},\"nonce\":\"{nonce}\"}}")
}

/// The nonce a complete record spends, if the line is exactly a spent-nonce record as this module writes one.
fn parse_record(line: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(line).ok()?;
    let Value::Object(map) = &value else {
        return None;
    };
    if map.len() != 2 {
        return None;
    }
    let at = map.get("at")?.as_u64()?;
    let nonce = map.get("nonce")?.as_str()?;
    if !valid_hex(nonce, 32, 64) || record_text(nonce, at).as_bytes() != line {
        return None;
    }
    Some(nonce.to_string())
}

/// Syncs the directory that holds `path`, so a file created there is durable.
fn sync_parent(path: &Path) -> io::Result<()> {
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    File::open(parent)?.sync_all()
}
