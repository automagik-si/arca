//! The `arca` binary: one node's one-shot watch and metrics reads, scoped token minting, the recording shim, and the Arca MCP
//! server. Every command that needs the node reads one config file. A command that only reads a node takes the one
//! `--config` names, else the one the `ARCA_CONFIG` variable names, else the file the installer writes. A command that
//! writes to a node, starts one or calls a peer takes it from `--config` alone.
//!
//! Exit codes: 0 for success; 1 for a runtime failure (a socket, a read or a write that failed); 2 for a usage or config
//! failure (a bad command line, a refused config, or a token directory that is unsafe or missing); 3 for a tool error, whose
//! JSON-RPC error object `call` prints on stdout; 4 for a token the node refuses, HTTP 401 or 403. `arca adopt` exits 2 too when
//! it refuses before any write: a preflight check, a stale plan digest, an instance the config does not list, or a record that
//! does not hold the adoption it is asked to resume, release or verify.

pub mod action_proposal;
pub mod adoptions;
pub mod approval_requests;
pub mod approve_page;
pub mod canvas;
pub mod cmd_adopt;
pub mod cmd_call;
pub mod cmd_init;
pub mod cmd_link;
pub mod cmd_probe;
pub mod cmd_serve;
pub mod cmd_setup_code;
pub mod cmd_status;
pub mod cmd_store;
pub mod cmd_templates;
pub mod cmd_token;
pub mod cmd_watch;
pub mod config;
pub mod desired_store;
pub mod gpu;
pub mod guest_secrets;
pub mod handlers;
pub mod kernel;
pub mod lifecycle;
pub mod linked;
pub mod omni;
mod passkey_change;
pub mod peer_sync;
pub mod service;
pub mod service_exec;
pub mod service_import;
pub mod storage;

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};

use arca_incus::{RecordingShim, ShimHandle};
use arca_mcp::CallError;
use serde_json::Value;
use tokio::signal::unix::{SignalKind, signal};

use crate::config::{ConfigError, NodeConfig};

const USAGE: &str = "usage: arca <command>
  version (or --version, -V)
  status [--json] [--config <file>] [--token-file <file>]
  watch --once --json [--config <file>]
  metrics --json [--sample-for <seconds, 0 to 60>] [--config <file>]
  link serve --config <file> (forwards arca/mcp/1 to listen.unix by default)
  link pair --config <file> [--minutes <n>]
  link join [--] <link> --config <file>
  link peers [--config <file>]
  link remove [--] <name> --config <file>
  probe consumer add --name <n> --image <fingerprint> --config <file>
  probe consumer remove --name <n> --config <file>
  serve --config <file>
  init --enable-changes --config <file> [--yes] [--service-cidr <a.b.c.0/24>] (let this node make changes; what is proposed waits for the owner's approval)
  init --watch-only --config <file> (back to a node that only looks)
  setup-code --config <file> (the code for the first passkey, on this terminal, for a machine with no screen)
  call <verb> [--config <file>] [--token-file <file>] [--args <json> | --args-file <file>]
  call --peer <name> <verb> --config <file> --token-file <file> [--args <json> | --args-file <file>]
  call (alone: lists the verbs)
  token mint --scope <viewer|doctor|operator> --id <id> --out <file> [--tools <comma list>] (--config <file> | --hash-dir <dir>)
  token revoke <id> (--config <file> | --hash-dir <dir>)
  token reload --config <file> (the running node reads its tokens again)
  store check --config <file>
  templates install --config <file> [--replace]
  service import plan --name <service> [--config <file>]
  shim --upstream <socket> --listen <socket> --log <file> [--log-max-bytes <n>]
  adopt plan --instance <name> --expect-ip <ipv4> --json --config <file>
  adopt apply --instance <name> --plan-digest <hex> [--expect-ip <ipv4>] [--dry-run] --config <file>
  adopt release --instance <name> [--dry-run] --config <file>
  adopt verify --instance <name> [--port <n> [--http-path <path>] | --url <http url>] [--wait <seconds>] --config <file>
  adopt status --instance <name> --json --config <file>

defaults, the files the installer writes:
  --config      ~/.config/arca/node.json
  --token-file  ~/.config/arca/assistant-1.secret
  The order is: the flag, then the variable (ARCA_CONFIG, ARCA_TOKEN_FILE), then the default.
  Only a command that reads a node takes them; one that writes to a node, starts one or calls a peer takes the flag alone.";

/// The variable that names the config when `--config` is not given.
pub const CONFIG_VARIABLE: &str = "ARCA_CONFIG";
/// The variable that names the token file when `--token-file` is not given.
pub const TOKEN_VARIABLE: &str = "ARCA_TOKEN_FILE";
/// The directory the installer writes the config and the first token into, under the home directory. The installer uses
/// `$HOME/.config` and never reads `XDG_CONFIG_HOME`, so neither does this: the default must be the file it wrote.
const INSTALL_DIR: &str = ".config/arca";
/// The config the installer writes, in [`INSTALL_DIR`].
const INSTALL_CONFIG: &str = "node.json";
/// The token the installer mints, in [`INSTALL_DIR`].
const INSTALL_TOKEN: &str = "assistant-1.secret";
/// The service the installer starts the node as.
pub(crate) const INSTALL_UNIT: &str = "arca.service";

/// Why a command ended without a result. The message never carries a token, a secret or a key.
#[derive(Debug)]
pub enum Failure {
    /// The command line is wrong.
    Usage(String),
    /// The config, or a token directory, is refused.
    Config(String),
    /// A socket, read or write failed while the command ran.
    Runtime(String),
    /// A tool call answered with an error. The JSON-RPC error object is printed on stdout, and nothing on stderr.
    Tool(Value),
    /// The node refuses the token, or the token's scope may not make the call. The text names the refusal.
    Unauthorized(String),
    /// `arca adopt` refused before it sent any write. The text names the check.
    Refused(String),
}

impl Failure {
    pub fn code(&self) -> i32 {
        match self {
            Self::Usage(_) | Self::Config(_) | Self::Refused(_) => 2,
            Self::Runtime(_) => 1,
            Self::Tool(_) => 3,
            Self::Unauthorized(_) => 4,
        }
    }

    /// The text a failure prints on stderr. A tool error prints its object on stdout instead, so its text is empty.
    pub fn message(&self) -> &str {
        match self {
            Self::Usage(text)
            | Self::Config(text)
            | Self::Runtime(text)
            | Self::Unauthorized(text)
            | Self::Refused(text) => text,
            Self::Tool(_) => "",
        }
    }
}

impl From<ConfigError> for Failure {
    fn from(error: ConfigError) -> Self {
        Self::Config(error.to_string())
    }
}

/// A failed MCP call, as a failure of the command that made it.
pub fn call_failure(error: CallError) -> Failure {
    match error {
        CallError::Internal(text) => Failure::Runtime(text),
        CallError::Refused { message, .. }
        | CallError::UntrustedRefused { message, .. }
        | CallError::RefusedWith { message, .. } => Failure::Runtime(message),
        _ => Failure::Runtime("the read failed".to_string()),
    }
}

fn runtime_failure(error: io::Error) -> Failure {
    Failure::Runtime(error.to_string())
}

/// What `arca --version` and `arca version` print: the product name, the version, and in brackets the short commit the
/// binary was built from and its target triple. The commit reads `unknown` when the build had no git checkout.
pub fn version_line() -> String {
    format!(
        "Automagik Arca {} ({}, {})",
        env!("CARGO_PKG_VERSION"),
        env!("ARCA_BUILD_COMMIT"),
        env!("ARCA_BUILD_TARGET")
    )
}

pub fn usage_failure() -> Failure {
    Failure::Usage(USAGE.to_string())
}

/// The usage lines of one command, and of one of its actions when `action` names one that has lines of its own.
fn usage_of(command: &str, action: Option<&str>) -> Vec<&'static str> {
    // The command lines end at the first empty line; the notes under them are not usage.
    let lines = |prefix: String| -> Vec<&'static str> {
        USAGE
            .lines()
            .take_while(|line| !line.is_empty())
            .filter(|line| *line == prefix.trim_end() || line.starts_with(&prefix))
            .collect()
    };
    let narrowed = action
        .map(|action| lines(format!("  {command} {action} ")))
        .unwrap_or_default();
    if narrowed.is_empty() {
        lines(format!("  {command} "))
    } else {
        narrowed
    }
}

/// A refusal about a flag (a missing one, an unknown one, a value that is missing or refused) with the command's own usage
/// lines under it, so the reader sees what the command takes without asking for the whole help.
fn with_usage(failure: Failure, command: &str, action: Option<&str>) -> Failure {
    let Failure::Usage(text) = failure else {
        return failure;
    };
    let about_a_flag = text.starts_with("--")
        || text.starts_with("unknown flag")
        || text.starts_with("unexpected argument");
    let lines = usage_of(command, action);
    if !about_a_flag || lines.is_empty() || text.contains('\n') {
        return Failure::Usage(text);
    }
    let lines: Vec<String> = lines
        .iter()
        .map(|line| format!("  arca {}", line.trim_start()))
        .collect();
    Failure::Usage(format!("{text}\nusage:\n{}", lines.join("\n")))
}

/// The number of single-character edits (insert, remove, replace) that turn `a` into `b`.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, x) in a.chars().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, y) in b.iter().enumerate() {
            let replaced = previous + usize::from(x != *y);
            previous = row[j + 1];
            row[j + 1] = replaced.min(previous + 1).min(row[j] + 1);
        }
    }
    row[b.len()]
}

/// The known word closest to `word`, when one is close enough to be what was meant: at most two edits away, or a third of
/// the word's length when that is more. Two words equally close give the first in `known`'s order.
pub fn closest<'a>(word: &str, known: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let limit = (word.chars().count() / 3).max(2);
    known
        .into_iter()
        .map(|candidate| (edit_distance(word, candidate), candidate))
        .filter(|(distance, candidate)| *distance <= limit && *distance < candidate.chars().count())
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, candidate)| candidate)
}

/// The refusal of a word that is not one of `known`: what kind of word it is, the closest known one when there is one,
/// and all of them. The word itself is never echoed.
pub(crate) fn unknown_word(kind: &str, given: Option<&str>, known: &[&str]) -> Failure {
    let all = known.join(", ");
    Failure::Usage(match given {
        None => format!("{kind} is missing: it is one of {all}"),
        Some(word) => match closest(word, known.iter().copied()) {
            Some(near) => format!("that is not {kind}; did you mean {near}? It is one of {all}"),
            None => format!("that is not {kind}: it is one of {all}"),
        },
    })
}

/// Where a path the command reads came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    /// The flag named it.
    Flag,
    /// The variable named it.
    Variable,
    /// Nothing named it: it is the file the installer writes.
    Default,
}

/// A path a command reads, and where it came from.
pub(crate) struct Chosen {
    pub path: PathBuf,
    pub origin: Origin,
}

/// The path `--flag` names, else the one `variable` names, else `file` in the installer's directory under the home
/// directory. An empty variable is no variable.
fn chosen(flags: &Flags, flag: &str, variable: &str, file: &str) -> Result<Chosen, Failure> {
    if let Some(path) = flags.value(flag) {
        return Ok(Chosen {
            path: PathBuf::from(path),
            origin: Origin::Flag,
        });
    }
    let set = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty());
    if let Some(path) = set(variable) {
        return Ok(Chosen {
            path: PathBuf::from(path),
            origin: Origin::Variable,
        });
    }
    let home = set("HOME").ok_or_else(|| {
        Failure::Usage(format!(
            "--{flag} is required here: neither {variable} nor HOME is set, so there is no default to read"
        ))
    })?;
    Ok(Chosen {
        path: Path::new(&home).join(INSTALL_DIR).join(file),
        origin: Origin::Default,
    })
}

/// The config a command reads: `--config`, else `ARCA_CONFIG`, else the installer's.
pub(crate) fn config_path(flags: &Flags) -> Result<Chosen, Failure> {
    chosen(flags, "config", CONFIG_VARIABLE, INSTALL_CONFIG)
}

/// The token file a command reads: `--token-file`, else `ARCA_TOKEN_FILE`, else the installer's.
pub(crate) fn token_path(flags: &Flags) -> Result<Chosen, Failure> {
    chosen(flags, "token-file", TOKEN_VARIABLE, INSTALL_TOKEN)
}

/// Reads and checks the config of a command that only reads a node, and says where it came from. A default that is not
/// there is not a read failure: nothing was installed for this user, and the refusal says so.
pub(crate) fn load_chosen_config(flags: &Flags) -> Result<(Chosen, NodeConfig), Failure> {
    let chosen = config_path(flags)?;
    if chosen.origin == Origin::Default && std::fs::symlink_metadata(&chosen.path).is_err() {
        return Err(Failure::Config(format!(
            "there is no config at {}, where the installer writes it: install Arca for this user, or name another config with --config <file> or {CONFIG_VARIABLE}",
            chosen.path.display()
        )));
    }
    let config = config::load(&chosen.path)?;
    Ok((chosen, config))
}

/// The config of a command that only reads a node: `--config`, else `ARCA_CONFIG`, else the installer's.
pub(crate) fn load_config(flags: &Flags) -> Result<NodeConfig, Failure> {
    load_chosen_config(flags).map(|(_, config)| config)
}

/// Reads and checks the config of a command that writes to a node, starts one or calls a peer. Such a command takes it
/// from `--config` alone, never from a variable or a default; `why` says so in the refusal.
pub(crate) fn load_named_config(flags: &Flags, why: &str) -> Result<NodeConfig, Failure> {
    let path = flags.value("config").ok_or_else(|| {
        Failure::Usage(format!(
            "--config is required: {why}, so it reads no default config"
        ))
    })?;
    Ok(config::load(Path::new(path))?)
}

/// The flags of one command: the named values and the bare switches, each given at most once.
pub struct Flags {
    values: BTreeMap<String, String>,
    switches: BTreeSet<String>,
}

impl Flags {
    /// The value of `--name`, when it was given.
    pub fn value(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    /// The value of `--name`, which must be given.
    pub fn required(&self, name: &str) -> Result<&str, Failure> {
        self.value(name)
            .ok_or_else(|| Failure::Usage(format!("--{name} is required")))
    }

    /// Whether the switch `--name` was given.
    pub fn switch(&self, name: &str) -> bool {
        self.switches.contains(name)
    }
}

/// Reads the flags of one command. A flag that is not in `takes_value` or `switches` is refused, and so is a value that is
/// missing or a flag given twice. A value is never echoed back.
pub(crate) fn parse_flags(
    args: &[String],
    takes_value: &[&str],
    switches: &[&str],
) -> Result<Flags, Failure> {
    let mut flags = Flags {
        values: BTreeMap::new(),
        switches: BTreeSet::new(),
    };
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let Some(name) = arg.strip_prefix("--") else {
            return Err(Failure::Usage(
                "unexpected argument: flags are named with --".to_string(),
            ));
        };
        if takes_value.contains(&name) {
            let value = rest
                .next()
                .filter(|value| !value.starts_with("--"))
                .ok_or_else(|| Failure::Usage(format!("--{name} needs a value")))?;
            if flags
                .values
                .insert(name.to_string(), value.clone())
                .is_some()
            {
                return Err(Failure::Usage(format!("--{name} is given twice")));
            }
        } else if switches.contains(&name) {
            if !flags.switches.insert(name.to_string()) {
                return Err(Failure::Usage(format!("--{name} is given twice")));
            }
        } else {
            let known = takes_value.iter().chain(switches).copied();
            return Err(Failure::Usage(match closest(name, known) {
                Some(near) => format!("unknown flag --{name}; did you mean --{near}?"),
                None => format!("unknown flag --{name}"),
            }));
        }
    }
    Ok(flags)
}

/// The commands, in the order the usage lists them.
const COMMANDS: &[&str] = &[
    "version",
    "status",
    "watch",
    "metrics",
    "link",
    "probe",
    "serve",
    "init",
    "setup-code",
    "call",
    "token",
    "store",
    "templates",
    "service",
    "shim",
    "adopt",
    "help",
];

/// Runs one command. A command with a result returns it, and `Ok(None)` prints nothing. A refusal about a flag carries the
/// command's usage lines.
pub fn run(argv: &[String]) -> Result<Option<Value>, Failure> {
    let Some((command, rest)) = argv.split_first() else {
        return Err(usage_failure());
    };
    let action = rest.first().map(String::as_str);
    run_command(command, rest).map_err(|failure| with_usage(failure, command, action))
}

fn run_command(command: &str, rest: &[String]) -> Result<Option<Value>, Failure> {
    match command {
        "status" => cmd_status::status(&parse_flags(rest, &["config", "token-file"], &["json"])?),
        "watch" => cmd_watch::watch(&parse_flags(rest, &["config"], &["once", "json"])?),
        "metrics" => cmd_watch::metrics(&parse_flags(rest, &["config", "sample-for"], &["json"])?),
        "serve" => cmd_serve::serve(&parse_flags(rest, &["config"], &[])?),
        "setup-code" => cmd_setup_code::setup_code(&parse_flags(rest, &["config"], &[])?),
        "init" => cmd_init::init(&parse_flags(
            rest,
            &["config", "service-cidr", "token-file"],
            &["enable-changes", "watch-only", "yes"],
        )?),
        "call" => cmd_call::call(rest),
        "link" => cmd_link::link(rest),
        "probe" => cmd_probe::probe(rest),
        "adopt" => cmd_adopt::adopt(rest),
        "service" => service_import::command(rest),
        "store" => match rest.split_first() {
            Some((action, rest)) if action == "check" => {
                cmd_store::check(&parse_flags(rest, &["config"], &[])?)
            }
            other => Err(unknown_word(
                "a store action",
                other.map(|(action, _)| action.as_str()),
                &["check"],
            )),
        },
        "templates" => match rest.split_first() {
            Some((action, rest)) if action == "install" => {
                cmd_templates::install(&parse_flags(rest, &["config"], &["replace"])?)
            }
            other => Err(unknown_word(
                "a templates action",
                other.map(|(action, _)| action.as_str()),
                &["install"],
            )),
        },
        "token" => match rest.split_first() {
            Some((action, rest)) if action == "mint" => cmd_token::mint_token(&parse_flags(
                rest,
                &["scope", "id", "hash-dir", "out", "config", "tools"],
                &[],
            )?),
            Some((action, rest)) if action == "revoke" => {
                let (id, rest) = rest.split_first().ok_or_else(|| {
                    Failure::Usage(
                        "token revoke needs the id of the token to revoke: arca token revoke <id>"
                            .to_string(),
                    )
                })?;
                cmd_token::revoke_token(id, &parse_flags(rest, &["hash-dir", "config"], &[])?)
            }
            Some((action, rest)) if action == "reload" => {
                cmd_token::reload_tokens(&parse_flags(rest, &["config"], &[])?)
            }
            other => Err(unknown_word(
                "a token action",
                other.map(|(action, _)| action.as_str()),
                &["mint", "revoke", "reload"],
            )),
        },
        "shim" => run_shim(&parse_flags(
            rest,
            &["upstream", "listen", "log", "log-max-bytes"],
            &[],
        )?),
        "help" | "--help" | "-h" => {
            println!("{USAGE}");
            Ok(None)
        }
        "version" | "--version" | "-V" if rest.is_empty() => {
            println!("{}", version_line());
            Ok(None)
        }
        "version" | "--version" | "-V" => Err(Failure::Usage(
            "version takes no argument: run arca version".to_string(),
        )),
        _ => Err(Failure::Usage(
            match closest(command, COMMANDS.iter().copied()) {
                Some(near) => format!(
                    "that is not an arca command; did you mean {near}? Run arca --help for all of them"
                ),
                None => "that is not an arca command: run arca --help for all of them".to_string(),
            },
        )),
    }
}

/// `shim --upstream <socket> --listen <socket> --log <file> [--log-max-bytes <n>]`: forwards GET to the upstream socket and
/// refuses every other request, until SIGTERM or SIGINT. Then it prints its summary and exits 0. With `--log-max-bytes`, the
/// log is rotated to `<log>.1` once it holds that many bytes. On SIGUSR1 it prints its summary so far to stderr, as one
/// JSON line, and keeps serving.
fn run_shim(flags: &Flags) -> Result<Option<Value>, Failure> {
    let upstream = PathBuf::from(flags.required("upstream")?);
    let listen = PathBuf::from(flags.required("listen")?);
    let log = PathBuf::from(flags.required("log")?);
    let cap = flags.value("log-max-bytes").map(log_cap).transpose()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(runtime_failure)?;
    runtime.block_on(async {
        // The signals are registered before the shim starts, so a stop that arrives early is still a clean stop.
        let mut terminate = signal(SignalKind::terminate()).map_err(runtime_failure)?;
        let mut interrupt = signal(SignalKind::interrupt()).map_err(runtime_failure)?;
        let mut report = signal(SignalKind::user_defined1()).map_err(runtime_failure)?;
        let handle = match cap {
            Some(cap) => RecordingShim::start_capped(&upstream, &listen, &log, cap),
            None => RecordingShim::start(&upstream, &listen, &log),
        }
        .map_err(runtime_failure)?;
        loop {
            tokio::select! {
                _ = terminate.recv() => break,
                _ = interrupt.recv() => break,
                _ = report.recv() => print_live_summary(&handle),
            }
        }
        let summary = handle.stop().map_err(runtime_failure)?;
        let value = serde_json::to_value(summary)
            .map_err(|_| Failure::Runtime("the summary does not serialize".to_string()))?;
        Ok::<Option<Value>, Failure>(Some(value))
    })
}

/// The value of `--log-max-bytes`: a whole number of bytes, at least 1.
fn log_cap(text: &str) -> Result<u64, Failure> {
    text.parse::<u64>()
        .ok()
        .filter(|cap| *cap > 0)
        .ok_or_else(|| {
            Failure::Usage(
                "--log-max-bytes must be a whole number of bytes, at least 1".to_string(),
            )
        })
}

/// The SIGUSR1 answer: the summary so far, as one JSON line on stderr. When a log line could not be written, a plain line
/// saying so follows it, so a reader that takes the last line as the summary sees no proof.
fn print_live_summary(handle: &ShimHandle) {
    if let Ok(line) = serde_json::to_string(&handle.summary()) {
        eprintln!("{line}");
    }
    if handle.log_failed() {
        eprintln!("arca: the shim could not write its log");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_closest_known_word_is_offered_only_when_it_is_near() {
        let verbs = ["watch_report", "arca_status", "plan_list"];
        assert_eq!(closest("watch_reprot", verbs), Some("watch_report"));
        assert_eq!(closest("stauts", COMMANDS.iter().copied()), Some("status"));
        assert_eq!(closest("jsno", ["once", "json"]), Some("json"));
        assert_eq!(closest("confg", ["config", "token-file"]), Some("config"));
        // A word that is near nothing gets no guess.
        assert_eq!(closest("frobnicate", COMMANDS.iter().copied()), None);
        assert_eq!(closest("x", COMMANDS.iter().copied()), None);
    }

    #[test]
    fn a_refusal_about_a_flag_carries_the_usage_of_its_command_and_action() {
        let refused = with_usage(
            Failure::Usage("--minutes needs a value".to_string()),
            "link",
            Some("pair"),
        );
        assert_eq!(
            refused.message(),
            "--minutes needs a value\nusage:\n  arca link pair --config <file> [--minutes <n>]"
        );
        // A refusal that is not about a flag stays one line, and so does one of another kind.
        let plain = with_usage(
            Failure::Usage("the arguments are not a JSON object".to_string()),
            "call",
            None,
        );
        assert_eq!(plain.message(), "the arguments are not a JSON object");
        let config = with_usage(Failure::Config("--x".to_string()), "call", None);
        assert_eq!(config.message(), "--x");
    }

    #[test]
    fn every_command_the_usage_lists_is_one_a_typo_can_be_matched_to() {
        for line in USAGE.lines().skip(1).take_while(|line| !line.is_empty()) {
            let command = line.split_whitespace().next().expect("a command");
            assert!(
                COMMANDS.contains(&command),
                "{command} is in the list of commands"
            );
        }
    }
}
