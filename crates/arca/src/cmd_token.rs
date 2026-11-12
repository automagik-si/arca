//! `token mint --scope <s> --id <id> --out <f>` with `--config <c>` or `--hash-dir <d>`: one scoped bearer token. The hash file goes
//! into the token directory and the secret into a file the caller names, both at mode 0400. Only the id and the scope are
//! printed. Nothing is overwritten: an id or an output path that exists is refused.
//!
//! `token reload --config <c>` makes the node that runs on that config read its tokens again, and says what happened. A
//! node reads its tokens when it starts and on SIGHUP, and `token mint` sends no signal, so this is the step after a mint
//! on a running node. It signals only a process it has checked to be `arca serve` on that very config.

use std::fs::{self, OpenOptions, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use arca_api::{Scope, may_call, verb};
use arca_mcp::mint_with_tools;
use serde_json::{Value, json};

use crate::cmd_serve::{ReloadRecord, SERVE_PID_FILE, SERVE_RELOAD_FILE};
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
        if let Some(pid) = serving_pid(Path::new(path), &cfg.state_dir) {
            reload_signalled = send_hangup(pid, Path::new(path));
        }
    }
    Ok(Some(json!({
        "id": id,
        "revoked": true,
        "reloadSignalled": reload_signalled
    })))
}

/// How long `token reload` waits for the node to record that it read its tokens: a bound for a busy machine, since the node
/// answers in milliseconds.
const RELOAD_WAIT: Duration = Duration::from_secs(30);

/// `token reload --config <c>`: sends the `arca serve` that runs on this config SIGHUP, waits for the node's own record of
/// the reload, and prints what happened. It exits 0 only when the node read its tokens again. A node that is not there, a
/// node that could not read its token directory, and a signal whose effect cannot be told are each said in words, with a
/// failure's exit code. It takes `--config` as a flag alone: it acts on a node, so it reads no default.
pub fn reload_tokens(flags: &Flags) -> Result<Option<Value>, Failure> {
    let cfg = crate::load_named_config(flags, "token reload signals a node")?;
    let config_text = flags.value("config").unwrap_or_default();
    let config_path = Path::new(config_text);
    // The process is checked through /proc. Without it nothing can be checked, so nothing is signalled.
    if fs::read_dir("/proc").is_err() {
        return Err(Failure::Runtime(
            "could not tell whether a node runs on this config: /proc cannot be read, and the node's process is checked there. Nothing was signalled".to_string(),
        ));
    }
    let Some(pid) = serving_pid(config_path, &cfg.state_dir) else {
        return Err(Failure::Runtime(format!(
            "no node runs on this config, so nothing was reloaded. A node reads its tokens when it starts: arca serve --config {config_text}"
        )));
    };
    let record_path = cfg.state_dir.join(SERVE_RELOAD_FILE);
    let before = ReloadRecord::read(&record_path).filter(|record| record.pid == pid);
    if !send_hangup(pid, config_path) {
        return Err(Failure::Runtime(format!(
            "could not tell whether the node on this config (pid {pid}) read its tokens: it could not be signalled. It may have just stopped, or it runs as another user"
        )));
    }
    let deadline = Instant::now() + RELOAD_WAIT;
    loop {
        let record = ReloadRecord::read(&record_path)
            .filter(|record| record.pid == pid && Some(*record) != before);
        match record {
            Some(ReloadRecord {
                tokens: Some(tokens),
                ..
            }) => {
                println!(
                    "reloaded: the node on this config (pid {pid}) read its tokens again and holds {tokens} token(s)"
                );
                return Ok(None);
            }
            Some(ReloadRecord { tokens: None, .. }) => {
                return Err(Failure::Runtime(format!(
                    "not reloaded: the node on this config (pid {pid}) could not read its token directory and keeps the tokens it had. Its own log says why"
                )));
            }
            None => {}
        }
        if !pid_serves_config(pid, config_path) {
            return Err(Failure::Runtime(format!(
                "could not tell whether the node on this config read its tokens: it (pid {pid}) stopped before it said so"
            )));
        }
        if Instant::now() >= deadline {
            return Err(Failure::Runtime(format!(
                "could not tell whether the node on this config (pid {pid}) read its tokens: it was signalled and did not say so within {} seconds. A node older than this command reloads without saying so; a call with the new token shows whether it did",
                RELOAD_WAIT.as_secs()
            )));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// The pid of the `arca serve` that runs on the config at `config`, when one does. The pid file in `state_dir` is believed
/// only when its pid is, right now, an `arca serve` on this very config: a file left by a node that crashed names a pid
/// that is gone or is another program's, and is not followed. With no such file (a node that has not written it, or
/// could not), the node is found by its command line.
fn serving_pid(config: &Path, state_dir: &Path) -> Option<u32> {
    let from_file = fs::read_to_string(state_dir.join(SERVE_PID_FILE))
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
        .filter(|pid| pid_serves_config(*pid, config));
    from_file.or_else(|| serve_pid_for_config(config))
}

/// Sends SIGHUP to the `arca serve` on `config` that has the pid `pid`, and to nothing else. Whether the signal was
/// delivered.
///
/// A pid is only a number: between the caller's check and a signal sent by number, the process could end and another
/// could take the number. So the process is first held by a descriptor, which names that one process for as long as it is
/// open; it is checked again while held, and the signal goes through the descriptor. A process that took over the number
/// fails the second check, and a process that ended cannot be signalled through its descriptor.
#[cfg(target_os = "linux")]
fn send_hangup(pid: u32, config: &Path) -> bool {
    use rustix::process::{Pid, PidfdFlags, Signal, pidfd_open, pidfd_send_signal};
    let Some(process) = i32::try_from(pid).ok().and_then(Pid::from_raw) else {
        return false;
    };
    let Ok(held) = pidfd_open(process, PidfdFlags::empty()) else {
        return false;
    };
    pid_serves_config(pid, config) && pidfd_send_signal(&held, Signal::HUP).is_ok()
}

/// Off Linux there is no `/proc` to check a process in, so no process is ever found to signal, and none is signalled.
#[cfg(not(target_os = "linux"))]
fn send_hangup(_pid: u32, _config: &Path) -> bool {
    false
}

/// Finds a live `arca serve --config <config>` process for this config, for the window before its post-bind pid file lands.
fn serve_pid_for_config(config: &Path) -> Option<u32> {
    for entry in fs::read_dir("/proc").ok()?.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        if pid_serves_config(pid, config) {
            return Some(pid);
        }
    }
    None
}

/// Reads `/proc/<pid>/cmdline` and `/proc/<pid>/cwd` and compares the named config with `config`.
fn pid_serves_config(pid: u32, config: &Path) -> bool {
    let Ok(bytes) = fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let args: Vec<Vec<u8>> = bytes
        .split(|byte| *byte == 0)
        .filter(|arg| !arg.is_empty())
        .map(<[u8]>::to_vec)
        .collect();
    let cwd = fs::read_link(format!("/proc/{pid}/cwd")).ok();
    serve_args_match_config(&args, cwd.as_deref(), config)
}

/// Whether `args` name `arca serve --config <config>`, resolving a relative config against the process's cwd.
fn serve_args_match_config(args: &[Vec<u8>], cwd: Option<&Path>, config: &Path) -> bool {
    let Some(program) = args.first() else {
        return false;
    };
    let program = String::from_utf8_lossy(program);
    if !program
        .rsplit('/')
        .next()
        .is_some_and(|name| name == "arca")
    {
        return false;
    }
    if !args.iter().any(|arg| arg.as_slice() == b"serve") {
        return false;
    }
    let wanted = fs::canonicalize(config).unwrap_or_else(|_| config.to_path_buf());
    for pair in args.windows(2) {
        if pair[0].as_slice() != b"--config" {
            continue;
        }
        let named_text = String::from_utf8_lossy(&pair[1]).into_owned();
        let named = Path::new(&named_text);
        let named = if named.is_relative() {
            match cwd {
                Some(cwd) => cwd.join(named),
                None => named.to_path_buf(),
            }
        } else {
            named.to_path_buf()
        };
        let named = fs::canonicalize(&named).unwrap_or(named);
        if named == wanted {
            return true;
        }
    }
    false
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
/// every other command checks it. When both are given they must name the same directory, and when neither is given the
/// command is refused: a credential is never written to, or removed from, a node nobody named.
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
            "--config or --hash-dir is required: a token is a credential of one node, so the command reads no default config".to_string(),
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

#[cfg(test)]
mod pid_match_tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<Vec<u8>> {
        items.iter().map(|item| item.as_bytes().to_vec()).collect()
    }

    #[test]
    fn a_relative_config_is_resolved_against_the_process_cwd() {
        let dir = std::env::temp_dir().join(format!("arca-cfg-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("scratch");
        let config = dir.join("node.json");
        fs::write(&config, "{}").expect("config");
        let process = args(&["/home/me/.local/bin/arca", "serve", "--config", "node.json"]);
        assert!(serve_args_match_config(&process, Some(&dir), &config));
        assert!(!serve_args_match_config(&process, None, &config));
        let other = dir.join("other.json");
        fs::write(&other, "{}").expect("other");
        assert!(!serve_args_match_config(&process, Some(&dir), &other));
        assert!(!pid_serves_config(std::process::id(), &config));
        let _ = fs::remove_dir_all(dir);
    }
}
