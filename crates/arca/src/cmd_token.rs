//! `token mint --scope <s> --id <id> --out <f>` with `--config <c>`, `--hash-dir <d>` or neither (the config every command
//! finds): one scoped bearer token. The hash file goes
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
use crate::config::NodeConfig;
use crate::{Chosen, Does, Failure, Flags};

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
    let hash_dir = token_node(flags, Does::Acts)?.hash_dir;
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

/// `token revoke <id> [--config <c> | --hash-dir <d>]`: removes the token's hash file and sends a running serve SIGHUP.
pub fn revoke_token(id: &str, flags: &Flags) -> Result<Option<Value>, Failure> {
    if !valid_id(id) {
        return Err(Failure::Usage(
            "--id is 1 to 64 lowercase letters, digits or hyphens, starting with a letter or a digit"
                .to_string(),
        ));
    }
    let node = token_node(flags, Does::Removes)?;
    let hash_dir = &node.hash_dir;
    check_hash_dir(hash_dir)?;
    let hash_path = hash_dir.join(format!("{id}.hash"));
    if !exists(&hash_path) {
        return Err(Failure::Config(format!("no token with id {id} exists")));
    }
    fs::remove_file(&hash_path)
        .map_err(|e| Failure::Runtime(format!("cannot remove the hash file: {e}")))?;
    let mut reload_signalled = false;
    if let Some((config, cfg)) = &node.config {
        let config_path = config.path.as_path();
        let path = config_path.display();
        match serving_pid(config_path, &cfg.state_dir) {
            Some(pid) => {
                let sent = send_hangup(pid, config_path);
                match sent {
                    Hangup::Sent | Hangup::NotSent => {}
                    Hangup::NoDescriptor => eprintln!(
                        "arca: the node was not signalled: {NO_DESCRIPTOR}. Restart the node for the revoke to take effect"
                    ),
                    // A node that is starting may have read the token before its file went, and a signal now would end
                    // it. So it is left alone, and the caller is told how to make sure.
                    Hangup::Starting => eprintln!(
                        "arca: the node on this config (pid {pid}) is still starting, so it was not signalled, and it may have read the token before this revoke. Run this in a moment to be sure: arca token reload --config {path}"
                    ),
                }
                reload_signalled = sent == Hangup::Sent;
            }
            None => {
                if let Some(pid) = other_users_node(config_path) {
                    eprintln!(
                        "arca: no node of yours runs on this config, so none was signalled: one runs as another user (pid {pid}) and keeps the token until that user reloads or restarts it"
                    );
                }
            }
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
/// failure's exit code. It finds its config as every command does, and says which one when the flag did not name it.
pub fn reload_tokens(flags: &Flags) -> Result<Option<Value>, Failure> {
    let (config, cfg) = crate::load_config_to(flags, Does::Acts)?;
    let config_path = config.path.as_path();
    let config_text = config_path.display();
    // The process is checked through /proc. Without it nothing can be checked, so nothing is signalled.
    if fs::read_dir("/proc").is_err() {
        return Err(Failure::Runtime(
            "could not tell whether a node runs on this config: /proc cannot be read, and the node's process is checked there. Nothing was signalled".to_string(),
        ));
    }
    let Some(pid) = serving_pid(config_path, &cfg.state_dir) else {
        // A node another user runs is not this caller's to signal, root's included: say whose it is.
        return Err(Failure::Runtime(match other_users_node(config_path) {
            Some(other) => format!(
                "no node of yours runs on this config, so nothing was reloaded: one runs as another user (pid {other}). Run this as that user"
            ),
            None => format!(
                "no node runs on this config, so nothing was reloaded. A node reads its tokens when it starts: arca serve --config {config_text}"
            ),
        }));
    };
    let record_path = cfg.state_dir.join(SERVE_RELOAD_FILE);
    let before = ReloadRecord::read(&record_path).filter(|record| record.pid == pid);
    match send_hangup(pid, config_path) {
        Hangup::Sent => {}
        Hangup::NoDescriptor => {
            return Err(Failure::Runtime(format!(
                "could not tell whether the node on this config (pid {pid}) read its tokens: nothing was signalled, because {NO_DESCRIPTOR}. Restart the node to have it read its tokens"
            )));
        }
        Hangup::Starting => {
            return Err(Failure::Runtime(format!(
                "the node on this config (pid {pid}) is still starting; nothing was signalled. Run this again in a moment"
            )));
        }
        Hangup::NotSent => {
            return Err(Failure::Runtime(format!(
                "could not tell whether the node on this config (pid {pid}) read its tokens: it could not be signalled. It may have just stopped"
            )));
        }
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
                "could not tell whether the node on this config (pid {pid}) read its tokens: it was signalled and did not say so within {} seconds. A node that is still starting says so once it is up, and a node older than this command reloads without saying so; a call with the new token shows whether it did",
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

/// What became of a SIGHUP for a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hangup {
    /// The signal was delivered to the node.
    Sent,
    /// The process could not be held by a descriptor at all, so nothing was signalled.
    NoDescriptor,
    /// The process is the node, but it does not catch SIGHUP yet: it is still starting, and the signal would end it.
    /// Nothing was signalled.
    Starting,
    /// The process was gone, was no longer the node, or refused the signal.
    NotSent,
}

/// Why a kernel, or a sandbox around this command, gives no pid descriptor.
const NO_DESCRIPTOR: &str = "this kernel cannot signal through a pid descriptor (that needs Linux 5.3 or later, and a sandbox may forbid it)";

/// Sends SIGHUP to the `arca serve` on `config` that has the pid `pid`, and to nothing else.
///
/// A pid is only a number: between the caller's check and a signal sent by number, the process could end and another
/// could take the number. So the process is first held by a descriptor, which names that one process for as long as it is
/// open; it is checked again while held, and the signal goes through the descriptor. A process that took over the number
/// fails the second check, and a process that ended cannot be signalled through its descriptor.
///
/// The order is the whole of the protection, so keep it: open the descriptor, THEN check (who the process is, and that
/// it catches the signal), THEN signal through the descriptor. A check moved before the open, or a signal sent by pid
/// number, brings the race back.
#[cfg(target_os = "linux")]
fn send_hangup(pid: u32, config: &Path) -> Hangup {
    use rustix::io::Errno;
    use rustix::process::{Pid, PidfdFlags, Signal, pidfd_open, pidfd_send_signal};
    let Some(process) = i32::try_from(pid).ok().and_then(Pid::from_raw) else {
        return Hangup::NotSent;
    };
    let held = match pidfd_open(process, PidfdFlags::empty()) {
        Ok(held) => held,
        // No such call on this kernel, or a sandbox that forbids it: neither says anything about the process.
        Err(Errno::NOSYS | Errno::PERM) => return Hangup::NoDescriptor,
        Err(_) => return Hangup::NotSent,
    };
    if !pid_serves_config(pid, config) {
        return Hangup::NotSent;
    }
    // Who the process is does not say that it is ready. Until `serve` has taken SIGHUP for itself, the signal's default
    // action ends the process, and systemd counts that as a clean stop and leaves the node down. So the signal goes only
    // to a process that catches it. A handler, once set, is never taken away, so this check cannot go stale.
    if !catches_hangup(pid) {
        return Hangup::Starting;
    }
    if pidfd_send_signal(&held, Signal::HUP).is_ok() {
        Hangup::Sent
    } else {
        Hangup::NotSent
    }
}

/// Off Linux there is no `/proc` to check a process in, so no process is ever found to signal, and none is signalled.
#[cfg(not(target_os = "linux"))]
fn send_hangup(_pid: u32, _config: &Path) -> Hangup {
    Hangup::NotSent
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

/// Whether the process `pid` is, right now, this user's `arca serve` on `config`: read from `/proc/<pid>`. Its owner
/// comes first, so another user's process is never a candidate, whatever its command line says.
fn pid_serves_config(pid: u32, config: &Path) -> bool {
    let own = rustix::process::geteuid().as_raw();
    fs::read_to_string(format!("/proc/{pid}/status"))
        .is_ok_and(|status| status_names_owner(&status, own))
        && pid_runs_serve_on(pid, config)
}

/// Whether the command line of the process `pid` is `arca serve` on `config`, whoever owns the process.
fn pid_runs_serve_on(pid: u32, config: &Path) -> bool {
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

/// The pid of an `arca serve` on `config` that is NOT this user's, when there is one: for the words of a refusal only.
/// Such a process is never signalled.
fn other_users_node(config: &Path) -> Option<u32> {
    fs::read_dir("/proc")
        .ok()?
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .find(|pid| pid_runs_serve_on(*pid, config) && !pid_serves_config(*pid, config))
}

/// Whether the process `pid` catches SIGHUP, read from `/proc/<pid>/status`.
fn catches_hangup(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/status"))
        .is_ok_and(|status| status_catches_hangup(&status))
}

/// Whether the text of a `/proc/<pid>/status` says the process has a handler for SIGHUP: bit 0 of `SigCgt`, the mask of
/// the signals it catches, since SIGHUP is signal 1.
fn status_catches_hangup(status: &str) -> bool {
    status
        .lines()
        .find_map(|line| line.strip_prefix("SigCgt:"))
        .and_then(|mask| u64::from_str_radix(mask.trim(), 16).ok())
        .is_some_and(|mask| mask & 1 == 1)
}

/// Whether the text of a `/proc/<pid>/status` says the process's real uid and its effective uid are both `uid`.
fn status_names_owner(status: &str, uid: u32) -> bool {
    status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .is_some_and(|ids| {
            let mut ids = ids.split_whitespace().map(|id| id.parse::<u32>().ok());
            ids.next().flatten() == Some(uid) && ids.next().flatten() == Some(uid)
        })
}

/// Whether `args` are the command line `arca serve --config <config>`, resolving a relative config against the process's
/// cwd. `serve` must be the command itself, the first word after the program: the word anywhere else is another command's
/// (`arca link serve`, the node's own link, which a SIGHUP would end) or one of its values (a token or a peer named
/// `serve`). The config is looked for among the command's own flags, after it.
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
    if args.get(1).map(Vec::as_slice) != Some(b"serve".as_slice()) {
        return false;
    }
    let wanted = fs::canonicalize(config).unwrap_or_else(|_| config.to_path_buf());
    for pair in args[2..].windows(2) {
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

/// The node a token command acts on: the directory its hash files are in, and the config that named it, when one did.
struct TokenNode {
    hash_dir: PathBuf,
    config: Option<(Chosen, NodeConfig)>,
}

/// The node the flags name. The hash directory is `--hash-dir`, or `tokens.hashDir` of the config, which is found as every
/// command finds it: `--config`, else `ARCA_CONFIG`, else the installer's. A `--hash-dir` given alone names the directory
/// by itself and no config is read, not even the variable's: the installer mints that way before any config exists. When
/// `--hash-dir` and `--config` are both given they must name the same directory; neither is taken over the other.
fn token_node(flags: &Flags, does: Does) -> Result<TokenNode, Failure> {
    let given = flags.value("hash-dir").map(PathBuf::from);
    let Some(given) = given else {
        let (chosen, cfg) = crate::load_config_to(flags, does)?;
        return Ok(TokenNode {
            hash_dir: cfg.tokens.hash_dir.clone(),
            config: Some((chosen, cfg)),
        });
    };
    if flags.value("config").is_none() {
        return Ok(TokenNode {
            hash_dir: given,
            config: None,
        });
    }
    let (chosen, cfg) = crate::load_config_to(flags, does)?;
    if !same_dir(&given, &cfg.tokens.hash_dir) {
        return Err(Failure::Usage(
            "--hash-dir differs from tokens.hashDir in --config; give one of them, or the same directory in both".to_string(),
        ));
    }
    Ok(TokenNode {
        hash_dir: given,
        config: Some((chosen, cfg)),
    })
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

    #[test]
    fn only_serve_as_the_command_itself_is_the_node() {
        let config = Path::new("/nonexistent/arca-node.json");
        let matches = |items: &[&str]| serve_args_match_config(&args(items), None, config);
        let path = "/nonexistent/arca-node.json";
        assert!(matches(&["arca", "serve", "--config", path]));
        assert!(matches(&["/usr/local/bin/arca", "serve", "--config", path]));
        // The node's own link: `serve` is its action, not its command, and a SIGHUP would end it.
        assert!(!matches(&["arca", "link", "serve", "--config", path]));
        // `serve` as a value: a token id, a peer name.
        assert!(!matches(&[
            "arca", "token", "revoke", "serve", "--config", path
        ]));
        assert!(!matches(&[
            "arca", "link", "remove", "serve", "--config", path
        ]));
        // Another program that names arca's words among its arguments.
        assert!(!matches(&[
            "arca", "-c", "script", "serve", "--config", path
        ]));
        assert!(!matches(&["sh", "serve", "--config", path]));
        // The node of another config, and a command line that stops short.
        assert!(!matches(&[
            "arca",
            "serve",
            "--config",
            "/nonexistent/other.json"
        ]));
        assert!(!matches(&["arca", "serve"]));
        assert!(!matches(&["arca"]));
        assert!(!matches(&[]));
    }

    #[test]
    fn a_process_is_ready_for_sighup_only_when_its_status_says_it_catches_it() {
        let status = |caught: &str| {
            format!("Name:\tarca\nSigIgn:\t0000000000001000\nSigCgt:\t{caught}\nCapInh:\t0\n")
        };
        // Bit 0 is SIGHUP. A node that has taken it, alone or among others.
        assert!(status_catches_hangup(&status("0000000000000001")));
        assert!(status_catches_hangup(&status("0000000000004003")));
        // A node that has not yet: SIGTERM and SIGINT only, or nothing.
        assert!(!status_catches_hangup(&status("0000000000004002")));
        assert!(!status_catches_hangup(&status("0000000000000000")));
        // Ignoring the signal is not catching it, and a status with no such line, or no mask, is not ready.
        assert!(!status_catches_hangup(
            "Name:\tarca\nSigIgn:\t0000000000000001\n"
        ));
        assert!(!status_catches_hangup(&status("not-hex")));
        // This test process has no handler for SIGHUP.
        assert!(!catches_hangup(std::process::id()));
    }

    #[test]
    fn a_process_is_this_users_only_when_its_real_and_effective_uid_both_are() {
        let status =
            |uids: &str| format!("Name:\tarca\nPid:\t42\nUid:\t{uids}\nGid:\t7\t7\t7\t7\n");
        assert!(status_names_owner(&status("1000\t1000\t1000\t1000"), 1000));
        assert!(!status_names_owner(&status("1001\t1001\t1001\t1001"), 1000));
        // A program that changed its effective uid, either way, is not plainly this user's.
        assert!(!status_names_owner(&status("1000\t0\t0\t0"), 1000));
        assert!(!status_names_owner(&status("0\t1000\t1000\t1000"), 1000));
        assert!(!status_names_owner("Name:\tarca\n", 1000));
        assert!(!status_names_owner(&status("x\ty\tz\tw"), 1000));
        // This process is its own user's.
        let own = fs::read_to_string("/proc/self/status").expect("this process's status");
        assert!(status_names_owner(
            &own,
            rustix::process::geteuid().as_raw()
        ));
    }
}
