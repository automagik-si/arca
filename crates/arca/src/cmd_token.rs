//! `token mint --scope <s> --id <id> --out <f>` with `--config <c>` or `--hash-dir <d>`: one scoped bearer token. The hash file goes
//! into the token directory and the secret into a file the caller names, both at mode 0400. Only the id and the scope are
//! printed. Nothing is overwritten: an id or an output path that exists is refused.

use std::fs::{self, OpenOptions, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use arca_api::{Scope, may_call, verb};
use arca_mcp::mint_with_tools;
use serde_json::{Value, json};

use crate::cmd_serve::SERVE_PID_FILE;
use crate::config;
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
    let hash_dir = hash_dir_of(flags)?;
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
    let tools = match flags.value("tools") {
        Some(text) => Some(parse_tools(text, scope)?),
        None => None,
    };

    let (secret, hash_text) = mint_with_tools(scope, tools.as_deref());
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

/// `token revoke <id> --config <c> | --hash-dir <d>`: removes the token's hash file and sends a running serve SIGHUP.
pub fn revoke_token(id: &str, flags: &Flags) -> Result<Option<Value>, Failure> {
    if !valid_id(id) {
        return Err(Failure::Usage(
            "--id is 1 to 64 lowercase letters, digits or hyphens, starting with a letter or a digit"
                .to_string(),
        ));
    }
    let hash_dir = hash_dir_of(flags)?;
    check_hash_dir(&hash_dir)?;
    let hash_path = hash_dir.join(format!("{id}.hash"));
    if !exists(&hash_path) {
        return Err(Failure::Config(format!("no token with id {id} exists")));
    }
    fs::remove_file(&hash_path)
        .map_err(|e| Failure::Runtime(format!("cannot remove the hash file: {e}")))?;
    let mut reload_signalled = false;
    if let Some(path) = flags.value("config") {
        let cfg = config::load(Path::new(path))?;
        let pid_path = cfg.state_dir.join(SERVE_PID_FILE);
        let from_file = fs::read_to_string(&pid_path)
            .ok()
            .and_then(|text| text.trim().parse::<u32>().ok())
            .filter(|pid| pid_is_arca_serve(*pid));
        let pid = from_file.or_else(|| serve_pid_for_config(Path::new(path)));
        if let Some(pid) = pid {
            let status = Command::new("kill")
                .arg("-HUP")
                .arg(pid.to_string())
                .status();
            reload_signalled = status.is_ok_and(|status| status.success());
        }
    }
    Ok(Some(json!({
        "id": id,
        "revoked": true,
        "reloadSignalled": reload_signalled
    })))
}

/// Finds a live `arca serve --config <config>` process for this config, for the window before its post-bind pid file lands.
fn serve_pid_for_config(config: &Path) -> Option<u32> {
    let wanted = fs::canonicalize(config).unwrap_or_else(|_| config.to_path_buf());
    for entry in fs::read_dir("/proc").ok()?.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        let Ok(bytes) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let args: Vec<&[u8]> = bytes
            .split(|byte| *byte == 0)
            .filter(|arg| !arg.is_empty())
            .collect();
        let Some(program) = args.first() else {
            continue;
        };
        let program = String::from_utf8_lossy(program);
        if !program
            .rsplit('/')
            .next()
            .is_some_and(|name| name == "arca")
        {
            continue;
        }
        if !args.iter().any(|arg| *arg == b"serve") {
            continue;
        }
        for pair in args.windows(2) {
            if pair[0] == b"--config" {
                let named_text = String::from_utf8_lossy(pair[1]).into_owned();
                let named = Path::new(&named_text);
                let named = fs::canonicalize(named).unwrap_or_else(|_| named.to_path_buf());
                if named == wanted {
                    return Some(pid);
                }
            }
        }
    }
    None
}

/// Whether `/proc/<pid>/cmdline` names an `arca serve` process. A stale or recycled pid is not signalled.
fn pid_is_arca_serve(pid: u32) -> bool {
    let Ok(bytes) = fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let mut args = bytes.split(|byte| *byte == 0).filter(|arg| !arg.is_empty());
    let Some(program) = args.next() else {
        return false;
    };
    let program = String::from_utf8_lossy(program);
    let named_arca = program
        .rsplit('/')
        .next()
        .is_some_and(|name| name == "arca");
    named_arca && args.any(|arg| arg == b"serve")
}

fn parse_tools(text: &str, scope: Scope) -> Result<Vec<String>, Failure> {
    let mut tools = Vec::new();
    for name in text.split(',') {
        if name.is_empty() {
            return Err(Failure::Usage(
                "--tools is a comma list of tool names".to_string(),
            ));
        }
        let Some(verb) = verb(name) else {
            return Err(Failure::Usage(format!(
                "--tools names an unknown tool: {name}"
            )));
        };
        if !may_call(scope, verb) {
            return Err(Failure::Usage(format!(
                "--tools names {name}, which the {scope:?} scope may not call"
            )));
        }
        if tools.iter().any(|seen: &String| seen == name) {
            return Err(Failure::Usage(format!("--tools names {name} twice")));
        }
        tools.push(name.to_string());
    }
    Ok(tools)
}

/// The directory the hash file goes into: `--hash-dir`, or `tokens.hashDir` of `--config`. A config is loaded and checked as
/// every other command checks it. When both are given they must name the same directory, and when neither is given the mint is
/// refused.
fn hash_dir_of(flags: &Flags) -> Result<PathBuf, Failure> {
    let given = flags.value("hash-dir").map(PathBuf::from);
    let configured = flags
        .value("config")
        .map(|path| config::load(Path::new(path)).map(|cfg| cfg.tokens.hash_dir))
        .transpose()?;
    match (given, configured) {
        (Some(given), Some(configured)) if same_dir(&given, &configured) => Ok(given),
        (Some(_), Some(_)) => Err(Failure::Usage(
            "--hash-dir differs from tokens.hashDir in --config; give one of them, or the same directory in both".to_string(),
        )),
        (Some(dir), None) | (None, Some(dir)) => Ok(dir),
        (None, None) => Err(Failure::Usage(
            "token mint needs --hash-dir or --config".to_string(),
        )),
    }
}

/// Whether two paths name one directory. Both are resolved when they exist. Otherwise they are compared as written, which can
/// only refuse a match that is really the same directory, never accept a different one.
fn same_dir(left: &Path, right: &Path) -> bool {
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(resolved_left), Ok(resolved_right)) => resolved_left == resolved_right,
        _ => left == right,
    }
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

/// Creates `path` exclusively and writes `bytes`, then sets the mode to 0400 on the open file, so the mode is not set again by
/// path. A file this call created is removed again when a write fails; a file that already existed is never touched.
fn write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o400)
        .open(path)?;
    let written = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .and_then(|()| file.set_permissions(Permissions::from_mode(0o400)));
    drop(file);
    if written.is_err() {
        let _ = fs::remove_file(path);
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_must_name_known_verbs_the_scope_may_call() {
        assert_eq!(
            parse_tools("node_metrics,journal_list", Scope::Viewer).expect("known reads"),
            vec!["node_metrics".to_string(), "journal_list".to_string()]
        );
        assert!(parse_tools("no_such_tool", Scope::Operator).is_err());
        assert!(parse_tools("token_list", Scope::Viewer).is_err());
        assert!(parse_tools("node_metrics,node_metrics", Scope::Viewer).is_err());
        assert!(parse_tools("", Scope::Viewer).is_err());
    }
}
