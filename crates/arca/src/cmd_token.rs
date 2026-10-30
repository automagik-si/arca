//! `token mint --scope <s> --id <id> --hash-dir <d> --out <f>`: one scoped bearer token. The hash file goes into the token
//! directory and the secret into a file the caller names, both at mode 0400. Only the id and the scope are printed. Nothing
//! is overwritten: an id or an output path that exists is refused.

use std::fs::{self, OpenOptions, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use arca_api::Scope;
use arca_mcp::mint;
use serde_json::{Value, json};

use crate::{Failure, Flags};

/// The longest id a token may have.
const MAX_ID_LEN: usize = 64;

pub fn mint_token(flags: &Flags) -> Result<Option<Value>, Failure> {
    let scope = parse_scope(flags.required("scope")?)?;
    let id = flags.required("id")?;
    if !valid_id(id) {
        return Err(Failure::Usage(format!(
            "--id is 1 to {MAX_ID_LEN} lowercase letters, digits or hyphens, starting with a letter or a digit"
        )));
    }
    let hash_dir = PathBuf::from(flags.required("hash-dir")?);
    let out = PathBuf::from(flags.required("out")?);
    check_hash_dir(&hash_dir)?;
    if exists(&out) {
        return Err(Failure::Config(
            "the output file already exists; choose a new path".to_string(),
        ));
    }
    let hash_path = hash_dir.join(format!("{id}.hash"));
    if exists(&hash_path) {
        return Err(Failure::Config(format!(
            "a token with id {id} already exists"
        )));
    }

    let (secret, hash_text) = mint(scope);
    write_new(&hash_path, hash_text.as_bytes())
        .map_err(|e| Failure::Runtime(format!("cannot write the hash file: {e}")))?;
    if let Err(e) = write_new(&out, format!("{secret}\n").as_bytes()) {
        // The hash without its secret is useless, so it goes too.
        let _ = fs::remove_file(&hash_path);
        return Err(Failure::Runtime(format!(
            "cannot write the secret file: {e}"
        )));
    }
    Ok(Some(json!({ "id": id, "scope": scope.as_str() })))
}

fn parse_scope(text: &str) -> Result<Scope, Failure> {
    match text {
        "viewer" => Ok(Scope::Viewer),
        "doctor" => Ok(Scope::Doctor),
        "operator" => Ok(Scope::Operator),
        _ => Err(Failure::Usage(
            "--scope is viewer, doctor or operator".to_string(),
        )),
    }
}

/// The same rule the token store applies on load: a lowercase letter or digit, then lowercase letters, digits and hyphens.
fn valid_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    let first_ok = bytes
        .first()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    first_ok
        && bytes.len() <= MAX_ID_LEN
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// The hash directory must exist, be a real directory, and have no group or other write bit. The server checks the same and
/// also the owner, when it loads the directory.
fn check_hash_dir(dir: &Path) -> Result<(), Failure> {
    let meta = fs::symlink_metadata(dir)
        .map_err(|_| Failure::Config("the hash directory does not exist".to_string()))?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(Failure::Config(
            "the hash directory is not a directory".to_string(),
        ));
    }
    if meta.permissions().mode() & 0o022 != 0 {
        return Err(Failure::Config(
            "the hash directory is writable by its group or by others; use mode 0700 or 0500"
                .to_string(),
        ));
    }
    Ok(())
}

fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// Creates `path` exclusively at mode 0400 and writes `bytes`. A file this call created is removed again when a write fails;
/// a file that already existed is never touched.
fn write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o400)
        .open(path)?;
    let written = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .and_then(|()| fs::set_permissions(path, Permissions::from_mode(0o400)));
    drop(file);
    if written.is_err() {
        let _ = fs::remove_file(path);
    }
    written
}
