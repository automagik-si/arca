//! Signed passkey changes and the state trust file's atomic writer.
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use arca_approval::passkey::{STATE_PASSKEYS_FILE, format_passkey_line};
use arca_approval::{
    ApprovalRefusal, ArcaAction, PasskeyLine, load_state_passkeys, parse_passkeys, parse_statement,
};
use arca_kernel::canon::{Digest, canonical_json, params_digest};
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use serde_json::{Value, json};

use crate::handlers::iso_utc;

pub(crate) fn target(line: &str) -> String {
    format!("passkey-{}", Digest::of(line.as_bytes()).to_hex())
}

pub(crate) fn malformed() -> ApprovalRefusal {
    ApprovalRefusal::Malformed("the passkey change is invalid".into())
}

pub(crate) fn storage() -> ApprovalRefusal {
    ApprovalRefusal::Storage("the passkey file could not be saved or read".into())
}

pub(crate) fn single_line(line: &str) -> Result<PasskeyLine, ApprovalRefusal> {
    if line.contains(['\n', '\r']) {
        return Err(malformed());
    }
    let mut parsed = parse_passkeys(line.as_bytes())?;
    if parsed.len() != 1 {
        return Err(malformed());
    }
    Ok(parsed.remove(0))
}

/// Keep the original full line: comments, label and added date are covered by the signed digest too.
pub(crate) fn state_lines(dir: &Path) -> Result<Vec<String>, ApprovalRefusal> {
    let path = dir.join(STATE_PASSKEYS_FILE);
    let parsed = load_state_passkeys(&path)?;
    if parsed.is_empty() {
        return Ok(Vec::new());
    }
    let body = fs::read_to_string(path).map_err(|_| storage())?;
    let raw: Vec<_> = body.lines().collect();
    parsed
        .iter()
        .map(|key| {
            raw.get(key.line - 1)
                .map(|line| (*line).to_string())
                .ok_or_else(storage)
        })
        .collect()
}

pub(crate) fn key_line(key: &PasskeyLine) -> Result<String, ApprovalRefusal> {
    format_passkey_line(
        &key.rp_id,
        &key.origin,
        &URL_SAFE_NO_PAD.encode(&key.credential_id),
        &STANDARD.encode(&key.spki),
    )
}

/// Metadata stays in the existing optional comment, so no second trust or metadata file is needed.
pub(crate) fn with_metadata(line: &str, label: &str, now: u64) -> Result<String, ApprovalRefusal> {
    if label.is_empty()
        || label.len() > 80
        || !label.bytes().all(|b| b.is_ascii_graphic() || b == b' ')
    {
        return Err(malformed());
    }
    Ok(format!("{line} arca-added={now} label={label}"))
}

pub(crate) fn listing(keys: &[PasskeyLine], dir: &Path) -> Result<Vec<Value>, ApprovalRefusal> {
    let state = state_lines(dir)?;
    keys.iter()
        .map(|key| {
            let full = state
                .iter()
                .find(|line| single_line(line).is_ok_and(|p| p.credential_id == key.credential_id));
            let line = full.cloned().unwrap_or(key_line(key)?);
            let metadata = line
                .split_once(" arca-added=")
                .and_then(|(_, meta)| meta.split_once(" label="));
            let (label, added) = match metadata {
                Some((date, label)) => (label.to_string(), date.parse::<u64>().ok().map(iso_utc)),
                None => (format!("Passkey {}", key.line), None),
            };
            Ok(json!({ "target": target(&line), "label": label, "addedAt": added }))
        })
        .collect()
}

pub(crate) fn statement(
    host: &str,
    action: ArcaAction,
    line: &str,
    now: u64,
) -> Result<String, ApprovalRefusal> {
    if !matches!(action, ArcaAction::PasskeyAdd | ArcaAction::PasskeyRemove) {
        return Err(malformed());
    }
    single_line(line)?;
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).map_err(|_| storage())?;
    let nonce: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
    let params = params_digest(&json!({ "line": line })).map_err(|_| malformed())?;
    let text = canonical_json(&json!({
        "action": action.as_str(), "host": host, "target": target(line), "paramsDigest": params,
        "issuedAt": iso_utc(now), "expiresAt": iso_utc(now.saturating_add(600_000)), "nonce": nonce,
    }))
    .map_err(|_| malformed())?;
    parse_statement(text.as_bytes())?;
    Ok(text)
}

pub(crate) fn write_state_bytes(dir: &Path, body: &[u8]) -> std::io::Result<()> {
    let path = dir.join(STATE_PASSKEYS_FILE);
    let tmp = dir.join(format!(".{STATE_PASSKEYS_FILE}.{}.tmp", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(body)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, &path)?;
    // Keep the directory entry durable too; a failure here is a storage failure, not a trust change.
    fs::File::open(dir)?.sync_all()?;
    Ok(())
}
