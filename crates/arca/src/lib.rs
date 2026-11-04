//! The `arca` binary: one node's one-shot watch and metrics reads, scoped token minting, the recording shim, and the Arca MCP
//! server. Every command that needs the node reads one config file, given with `--config`.
//!
//! Exit codes: 0 for success; 1 for a runtime failure (a socket, a read or a write that failed); 2 for a usage or config
//! failure (a bad command line, a refused config, or a token directory that is unsafe or missing); 3 for a tool error, whose
//! JSON-RPC error object `call` prints on stdout; 4 for a token the node refuses, HTTP 401 or 403. `arca adopt` exits 2 too when
//! it refuses before any write: a preflight check, a stale plan digest, an instance the config does not list, or a record that
//! does not hold the adoption it is asked to resume, release or verify.

pub mod adoptions;
pub mod cmd_adopt;
pub mod cmd_call;
pub mod cmd_link;
pub mod cmd_probe;
pub mod cmd_serve;
pub mod cmd_store;
pub mod cmd_token;
pub mod cmd_watch;
pub mod config;
pub mod desired_store;
pub mod handlers;
pub mod kernel;

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::PathBuf;

use arca_incus::{RecordingShim, ShimHandle};
use arca_mcp::CallError;
use serde_json::Value;
use tokio::signal::unix::{SignalKind, signal};

use crate::config::ConfigError;

const USAGE: &str = "usage: arca <command>
  watch --once --json --config <file>
  metrics --json [--sample-for <seconds, 0 to 60>] --config <file>
  link serve --config <file>
  link pair --config <file> [--minutes <n>]
  link join <link> --config <file>
  link peers --config <file>
  link remove <name> --config <file>
  probe consumer add --name <n> --image <fingerprint> --config <file>
  probe consumer remove --name <n> --config <file>
  serve --config <file>
  call <verb> --config <file> --token-file <file> [--args <json> | --args-file <file>]
  token mint --scope <viewer|doctor|operator> --id <id> --out <file> (--config <file> | --hash-dir <dir>)
  store check --config <file>
  shim --upstream <socket> --listen <socket> --log <file> [--log-max-bytes <n>]
  adopt plan --instance <name> --expect-ip <ipv4> --json --config <file>
  adopt apply --instance <name> --plan-digest <hex> [--expect-ip <ipv4>] [--dry-run] --config <file>
  adopt release --instance <name> [--dry-run] --config <file>
  adopt verify --instance <name> [--port <n> [--http-path <path>] | --url <http url>] [--wait <seconds>] --config <file>
  adopt status --instance <name> --json --config <file>";

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
        CallError::Refused { message, .. } => Failure::Runtime(message),
        _ => Failure::Runtime("the read failed".to_string()),
    }
}

fn runtime_failure(error: io::Error) -> Failure {
    Failure::Runtime(error.to_string())
}

pub fn usage_failure() -> Failure {
    Failure::Usage(USAGE.to_string())
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
            return Err(Failure::Usage(format!("unknown flag --{name}")));
        }
    }
    Ok(flags)
}

/// Runs one command. A command with a result returns it, and `Ok(None)` prints nothing.
pub fn run(argv: &[String]) -> Result<Option<Value>, Failure> {
    let Some((command, rest)) = argv.split_first() else {
        return Err(usage_failure());
    };
    match command.as_str() {
        "watch" => cmd_watch::watch(&parse_flags(rest, &["config"], &["once", "json"])?),
        "metrics" => cmd_watch::metrics(&parse_flags(rest, &["config", "sample-for"], &["json"])?),
        "serve" => cmd_serve::serve(&parse_flags(rest, &["config"], &[])?),
        "call" => cmd_call::call(rest),
        "link" => cmd_link::link(rest),
        "probe" => cmd_probe::probe(rest),
        "adopt" => cmd_adopt::adopt(rest),
        "store" => match rest.split_first() {
            Some((action, rest)) if action == "check" => {
                cmd_store::check(&parse_flags(rest, &["config"], &[])?)
            }
            _ => Err(usage_failure()),
        },
        "token" => match rest.split_first() {
            Some((action, rest)) if action == "mint" => cmd_token::mint_token(&parse_flags(
                rest,
                &["scope", "id", "hash-dir", "out", "config"],
                &[],
            )?),
            _ => Err(usage_failure()),
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
        _ => Err(usage_failure()),
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
