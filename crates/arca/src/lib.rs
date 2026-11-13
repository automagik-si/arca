//! The `arca` binary: one node's one-shot watch and metrics reads, scoped token minting, the recording shim, and the Arca MCP
//! server. Every command that needs the node reads one config file, and every command finds it the same way: the one
//! `--config` names, else the one the `ARCA_CONFIG` variable names, else the file the installer writes. A command that
//! writes to a node or acts on one, and did not get its config from the flag, says on stderr which file it uses before it
//! does anything; one that removes or cuts something off names the node's state directory too.
//!
//! Exit codes: 0 for success; 1 for a runtime failure (a socket, a read or a write that failed); 2 for a usage or config
//! failure (a bad command line, a refused config, or a token directory that is unsafe or missing); 3 for a tool error: a refusal,
//! whose JSON-RPC error object `call` prints on stdout, or a tool result with `isError` (arguments the tool does not
//! take, or its own failure), printed as `{"code": ..., "message": ...}`; 4 for a token the node refuses, HTTP 401 or 403. `arca adopt` exits 2 too when
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
pub mod function;
pub mod function_run;
pub mod gpu;
pub mod guest_secrets;
pub mod handlers;
pub mod kernel;
pub mod lifecycle;
pub mod linked;
pub mod omni;
mod passkey_change;
pub mod peer_sync;
pub mod registry;
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
  status [--json | --security] [--config <file>] [--token-file <file>]
  watch --once --json [--config <file>]
  metrics --json [--sample-for <seconds, 0 to 60>] [--config <file>]
  link serve [--config <file>] (forwards arca/mcp/1 to listen.unix by default)
  link pair [--config <file>] [--minutes <n>]
  link join [--] <link> [--config <file>]
  link peers [--config <file>]
  link remove [--] <name> [--config <file>]
  probe consumer add --name <n> --image <fingerprint> [--config <file>]
  probe consumer remove --name <n> [--config <file>]
  serve [--config <file>]
  init --enable-changes [--config <file>] [--yes] [--service-cidr <a.b.c.0/24>] (let this node make changes; what is proposed waits for the owner's approval)
  init --watch-only [--config <file>] (back to a node that only looks)
  setup-code [--config <file>] (the code for the first passkey, on this terminal, for a machine with no screen)
  call <verb> [--config <file>] [--token-file <file>] [--args <json> | --args-file <file>]
  call --peer <name> <verb> --token-file <file> [--config <file>] [--args <json> | --args-file <file>]
  call (alone: lists the verbs)
  token mint --scope <viewer|doctor|operator> --id <id> --out <file> [--tools <comma list>] [--config <file> | --hash-dir <dir>]
  token revoke <id> [--config <file> | --hash-dir <dir>]
  token reload [--config <file>] (the running node reads its tokens again)
  store check [--config <file>]
  templates install [--config <file>] [--replace]
  service import plan --name <service> [--config <file>]
  shim --upstream <socket> --listen <socket> --log <file> [--log-max-bytes <n>]
  adopt plan --instance <name> --expect-ip <ipv4> --json [--config <file>]
  adopt apply --instance <name> --plan-digest <hex> [--expect-ip <ipv4>] [--dry-run] [--config <file>]
  adopt release --instance <name> [--dry-run] [--config <file>]
  adopt verify --instance <name> [--port <n> [--http-path <path>] | --url <http url>] [--wait <seconds>] [--config <file>]
  adopt status --instance <name> --json [--config <file>]

defaults, the files the installer writes:
  --config      ~/.config/arca/node.json
  --token-file  ~/.config/arca/assistant-1.secret
  The order is: the flag, then the variable (ARCA_CONFIG, ARCA_TOKEN_FILE), then the default.
  status alone takes one step more: with no token file named and none at the default, it uses a viewer token of this
  node from the default's directory (the viewer.secret of a node installed before that default existed).
  Every command finds its config this way. One that writes to a node or acts on it, without --config, first says on
  stderr which config it uses; one that removes or cuts something off also names the node's state directory.
  A call with --peer sends its token to another node, so its --token-file is always named: it has no default.
  token mint, token revoke, token reload, call --peer, link pair, link join, link remove, probe consumer remove,
  adopt apply, adopt release and init hand out, remove or rewrite something on one node, so the variable alone never
  picks it: when ARCA_CONFIG names another file than the default, they refuse and say the --config to run them with.";

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
    /// A tool call answered with an error: a refusal's JSON-RPC error object, or `{"code", "message"}` for a tool
    /// result with `isError`. It is printed on stdout, and nothing on stderr.
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

/// The known word closest to a mistyped one, when one is close enough to be what was meant. It is the verb table's own
/// measure, so a mistyped command, flag, verb or field is answered the same way.
pub use arca_api::closest;

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
    if let Some(path) = set_variable(variable) {
        // A relative path in the variable is kept, shown and handed on as the whole path it names from here.
        let path = PathBuf::from(path);
        return Ok(Chosen {
            path: std::path::absolute(&path).unwrap_or(path),
            origin: Origin::Variable,
        });
    }
    let path = installed_path(file).ok_or_else(|| {
        Failure::Usage(format!(
            "--{flag} is required here: neither {variable} nor HOME is set, so there is no default to read"
        ))
    })?;
    Ok(Chosen {
        path,
        origin: Origin::Default,
    })
}

/// The value of an environment variable that is set to something.
fn set_variable(name: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

/// Where the installer writes `file`, when there is a home directory to hold it.
fn installed_path(file: &str) -> Option<PathBuf> {
    set_variable("HOME").map(|home| Path::new(&home).join(INSTALL_DIR).join(file))
}

/// The config a command reads: `--config`, else `ARCA_CONFIG`, else the installer's.
pub(crate) fn config_path(flags: &Flags) -> Result<Chosen, Failure> {
    chosen(flags, "config", CONFIG_VARIABLE, INSTALL_CONFIG)
}

/// The token file a command reads: `--token-file`, else `ARCA_TOKEN_FILE`, else the installer's.
pub(crate) fn token_path(flags: &Flags) -> Result<Chosen, Failure> {
    chosen(flags, "token-file", TOKEN_VARIABLE, INSTALL_TOKEN)
}

/// What a command does to the node its config names, which decides what it says before it does it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Does {
    /// It only reads the node. It says nothing.
    Reads,
    /// It writes to the node, starts something on it or acts through it.
    Acts,
    /// It removes something from the node, cuts something off from it or overwrites what was there.
    Removes,
}

/// The line a command that acts says before it does, when nobody named its config with the flag: which config file it
/// uses and how it was found, and for a command that removes, the node's state directory too. `None` for a command that
/// only reads, and for a config the flag named: the caller wrote that path themselves.
fn announcement(chosen: &Chosen, config: &NodeConfig, does: Does) -> Option<String> {
    let found = match chosen.origin {
        Origin::Flag => return None,
        Origin::Variable => format!("from {CONFIG_VARIABLE}"),
        Origin::Default => "the default".to_string(),
    };
    let using = format!("using the config {} ({found})", chosen.path.display());
    match does {
        Does::Reads => None,
        Does::Acts => Some(using),
        Does::Removes => Some(format!(
            "{using}; this node's state directory is {}",
            config.state_dir.display()
        )),
    }
}

/// The name of the user with the id `uid`, as `/etc/passwd` has it, or the id itself.
fn user_of(uid: u32) -> String {
    std::fs::read_to_string("/etc/passwd")
        .ok()
        .and_then(|users| {
            users.lines().find_map(|line| {
                let mut fields = line.split(':');
                let name = fields.next()?;
                (fields.nth(1)?.parse::<u32>().ok()? == uid).then(|| name.to_string())
            })
        })
        .unwrap_or_else(|| format!("the user with the id {uid}"))
}

/// Whether a config the flag did not name is one a writing command refuses: it belongs to another user. A reading
/// command reads it; a command the flag named a config for acts on what it was told to.
fn another_users(owner: u32, caller: u32, origin: Origin, does: Does) -> bool {
    owner != caller && origin != Origin::Flag && does != Does::Reads
}

/// Refuses a writing command a config that it found by itself and that belongs to another user. This is root with
/// another user's home directory kept (`sudo` without `-H`, `su` without `-`): the default path, or the variable, then
/// leads to that user's node, and nothing said so.
fn refuse_another_users_config(chosen: &Chosen, does: Does) -> Result<(), Failure> {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = std::fs::metadata(&chosen.path) else {
        // A file that cannot be read is refused by the load, in its own words.
        return Ok(());
    };
    let caller = rustix::process::geteuid().as_raw();
    if !another_users(meta.uid(), caller, chosen.origin, does) {
        return Ok(());
    }
    let owner = user_of(meta.uid());
    Err(Failure::Config(format!(
        "the config {} belongs to {owner}, not to the user running this command, and --config did not name it, so nothing was done. Run this as {owner}, or name the config: --config {}",
        chosen.path.display(),
        chosen.path.display()
    )))
}

/// The config at the path `chosen`, read and checked, with the line a command that acts says first.
pub(crate) fn load_found(chosen: Chosen, does: Does) -> Result<(Chosen, NodeConfig), Failure> {
    if chosen.origin == Origin::Default && std::fs::symlink_metadata(&chosen.path).is_err() {
        return Err(Failure::Config(format!(
            "there is no config at {}, where the installer writes it: install Arca for this user, or name another config with --config CONFIG_FILE (its path in place of CONFIG_FILE) or {CONFIG_VARIABLE}",
            chosen.path.display()
        )));
    }
    refuse_another_users_config(&chosen, does)?;
    let config = config::load(&chosen.path)?;
    if let Some(line) = announcement(&chosen, &config, does) {
        eprintln!("arca: {line}");
    }
    Ok((chosen, config))
}

/// The config a command uses, found the one way every command finds it. A command that acts on the node, or removes
/// something from it, and was not given its config by the flag, says on stderr which node it is about to touch, as the
/// first thing it prints and before it does anything.
pub(crate) fn load_config_to(flags: &Flags, does: Does) -> Result<(Chosen, NodeConfig), Failure> {
    load_found(config_path(flags)?, does)
}

/// Whether two paths name one file. Both are resolved when they exist; otherwise they are compared as written, which can
/// only miss a match, never make one.
fn same_file(left: &Path, right: &Path) -> bool {
    let resolved = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    resolved(left) == resolved(right)
}

/// Whether the variable alone may choose the node: only when it names the very file the default names. `default` is the
/// installer's config, when there is a home directory to hold one.
fn variable_names_the_default(variable: &Path, default: Option<&Path>) -> bool {
    default.is_some_and(|default| same_file(variable, default))
}

/// The config of a command that hands out, sends or removes a credential, a peer or an instance, or rewrites what a node
/// is: `token mint`, `token revoke`, `token reload` (the step that makes a revoke take effect), `call --peer`, `link
/// pair` and `link join` (which hand out a peer), `link remove`, `probe consumer remove`, `adopt apply`, `adopt release`
/// and both modes of `init`. It is found as every
/// command finds it, with one exception: THE VARIABLE ALONE NEVER PICKS THE NODE. A variable left set for another node
/// would send such a command there, it would succeed, and the line on stderr is not something a script or an assistant
/// stops for. So with the variable set and no flag, the command goes on only when the variable names the same file as
/// the default; when it names another, the command refuses before it reads anything and says the flag to run it with.
pub(crate) fn load_node_config_to(
    flags: &Flags,
    does: Does,
) -> Result<(Chosen, NodeConfig), Failure> {
    load_found(node_config_path(flags)?, does)
}

/// Where the config of such a command is, or the refusal of a variable that names another node than the default. It
/// reads nothing: a command with checks of its own to make before the config is read (`init`) takes the path, makes
/// them, and then calls [`load_found`].
pub(crate) fn node_config_path(flags: &Flags) -> Result<Chosen, Failure> {
    let chosen = config_path(flags)?;
    if chosen.origin == Origin::Variable {
        let default = installed_path(INSTALL_CONFIG);
        if !variable_names_the_default(&chosen.path, default.as_deref()) {
            let elsewhere = match &default {
                Some(default) => format!("not the default config ({})", default.display()),
                None => "and there is no default config to compare it with".to_string(),
            };
            return Err(Failure::Usage(format!(
                "{CONFIG_VARIABLE} names {}, {elsewhere}. This command hands out, removes or rewrites something on one node, so the variable alone does not choose that node, and nothing was done. To act on the node {CONFIG_VARIABLE} names, run the same command with: --config {}",
                chosen.path.display(),
                chosen.path.display()
            )));
        }
    }
    Ok(chosen)
}

/// The config of a command that only reads a node, and where it came from.
pub(crate) fn load_chosen_config(flags: &Flags) -> Result<(Chosen, NodeConfig), Failure> {
    load_config_to(flags, Does::Reads)
}

/// The config of a command that only reads a node.
pub(crate) fn load_config(flags: &Flags) -> Result<NodeConfig, Failure> {
    load_config_to(flags, Does::Reads).map(|(_, config)| config)
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
        "status" => cmd_status::status(&parse_flags(
            rest,
            &["config", "token-file"],
            &["json", "security"],
        )?),
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
                        "token revoke needs the id of the token to revoke: arca token revoke TOKEN_ID, with that id in place of TOKEN_ID"
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
            "--minutes needs a value\nusage:\n  arca link pair [--config <file>] [--minutes <n>]"
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
    fn a_command_says_its_config_only_when_it_acts_and_the_flag_did_not_name_it() {
        let config: NodeConfig = serde_json::from_value(serde_json::json!({
            "stateDir": "/nonexistent/state",
            "incus": { "socket": "/nonexistent/incus.sock" },
            "listen": { "unix": "/nonexistent/arca.sock" },
            "tokens": { "hashDir": "/nonexistent/tokens" },
            "kernel": { "droiddProjects": [] }
        }))
        .expect("the fixture config parses");
        let chosen = |origin: Origin| Chosen {
            path: PathBuf::from("/nonexistent/node.json"),
            origin,
        };
        let says = |origin: Origin, does: Does| announcement(&chosen(origin), &config, does);
        // A config the flag named is never said back, whatever the command does.
        for does in [Does::Reads, Does::Acts, Does::Removes] {
            assert_eq!(says(Origin::Flag, does), None);
        }
        // A command that only reads says nothing, wherever its config came from.
        assert_eq!(says(Origin::Default, Does::Reads), None);
        assert_eq!(says(Origin::Variable, Does::Reads), None);
        // A command that acts says the file and how it was found.
        assert_eq!(
            says(Origin::Default, Does::Acts).as_deref(),
            Some("using the config /nonexistent/node.json (the default)")
        );
        assert_eq!(
            says(Origin::Variable, Does::Acts).as_deref(),
            Some("using the config /nonexistent/node.json (from ARCA_CONFIG)")
        );
        // A command that removes names the node's state directory too.
        assert_eq!(
            says(Origin::Variable, Does::Removes).as_deref(),
            Some(
                "using the config /nonexistent/node.json (from ARCA_CONFIG); this node's state directory is /nonexistent/state"
            )
        );
    }

    #[test]
    fn a_writing_command_refuses_a_config_it_found_by_itself_and_does_not_own() {
        // Root with another user's home kept: the default, or the variable, leads to that user's config.
        assert!(another_users(1000, 0, Origin::Default, Does::Acts));
        assert!(another_users(1000, 0, Origin::Variable, Does::Removes));
        assert!(another_users(1000, 1001, Origin::Default, Does::Acts));
        // The caller's own config, a config the flag named, and a command that only reads are let through.
        assert!(!another_users(1000, 1000, Origin::Default, Does::Removes));
        assert!(!another_users(1000, 0, Origin::Flag, Does::Removes));
        assert!(!another_users(1000, 0, Origin::Default, Does::Reads));
        // The owner is named as the system names it.
        assert_eq!(user_of(0), "root");
        assert_eq!(user_of(4_294_967_000), "the user with the id 4294967000");
    }

    #[test]
    fn the_variable_chooses_alone_only_when_it_names_the_default_file() {
        let dir = std::env::temp_dir().join(format!("arca-same-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch");
        let default = dir.join("node.json");
        let other = dir.join("other.json");
        std::fs::write(&default, "{}").expect("default");
        std::fs::write(&other, "{}").expect("other");
        assert!(variable_names_the_default(&default, Some(&default)));
        // The same file by another spelling, and through a link.
        assert!(variable_names_the_default(
            &dir.join("./node.json"),
            Some(&default)
        ));
        let link = dir.join("link.json");
        std::os::unix::fs::symlink(&default, &link).expect("link");
        assert!(variable_names_the_default(&link, Some(&default)));
        // Another file, and no default at all.
        assert!(!variable_names_the_default(&other, Some(&default)));
        assert!(!variable_names_the_default(&default, None));
        let _ = std::fs::remove_dir_all(dir);
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
