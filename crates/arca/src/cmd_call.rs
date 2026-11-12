//! `call <verb> [--peer <name>] [--config <file>] [--token-file <file>] [--args <json> | --args-file <file>]`: one tool call against the node that
//! `serve` runs. The config and the token file default to the ones the installer writes. `call` alone lists the verbs.
//!
//! A call with `--peer` sends its token to another node, which could send it back here through the link. So such a call
//! takes its token from `--token-file` alone, a token the peer minted, and its config from `--config` alone: this node's
//! own token is never sent to a peer by default. The token is read from its own file and sent only in the bearer header of each request. An approval is read
//! from the arguments file, never from the command line, and neither the token nor an approval is ever printed.
//!
//! Each request uses its own connection, since the server answers `Connection: close`. The call first sends `initialize`, so
//! a token the node refuses is found before the verb is named again.

use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use arca_api::{verb, verbs};
use arca_mcp::PROTOCOL_VERSION;
use serde_json::{Value, json};

use crate::{Chosen, Failure, Flags, INSTALL_UNIT, Origin, closest, parse_flags};

/// The node a call addresses, for the next step a refusal names: every command in a hint names this node's own config, so
/// following it never acts on another node.
pub(crate) struct Addressed {
    /// The config that names the node.
    config: String,
    /// Whether that config is the installer's own, whose node the installer runs as a service.
    installed: bool,
    /// Whether the call goes to a paired peer through this node's link, and not to this node.
    peer: bool,
}

impl Addressed {
    pub(crate) fn new(config: &Chosen, peer: bool) -> Addressed {
        Addressed {
            config: config.path.display().to_string(),
            installed: config.origin == Origin::Default,
            peer,
        }
    }

    /// The command that makes a running node read its tokens again. `serve` reads them when it starts and on SIGHUP;
    /// `token reload` finds this config's node, signals no other process, and says what happened.
    fn reload(&self) -> String {
        format!("arca token reload --config {}", self.config)
    }

    /// How a token of `scope` is made for this node into `out`, and the step that makes a running node take it: `token
    /// mint` writes the hash and sends no signal.
    fn mint(&self, scope: &str, out: &str) -> String {
        format!(
            "arca token mint --scope {scope} --id <id> --out {out} --config {}. A node reads its tokens only when it starts or on SIGHUP, so a node that is running needs this after the mint: {}",
            self.config,
            self.reload()
        )
    }

    /// What to do about a token file that is missing or holds no token.
    pub(crate) fn token_advice(&self, file: &Chosen) -> String {
        if self.peer {
            return "a call with --peer sends a token the peer minted: ask the peer's owner for one".to_string();
        }
        match file.origin {
            // Minted to the installer's own path, the token is found again with no flag.
            Origin::Default => format!(
                "mint one with: {}",
                self.mint("viewer", &file.path.display().to_string())
            ),
            Origin::Flag | Origin::Variable => {
                format!("mint one with: {}", self.mint("viewer", "<file>"))
            }
        }
    }

    /// How the node is started.
    fn start(&self) -> String {
        if self.installed {
            format!(
                "systemctl --user start {INSTALL_UNIT} (the installer's service), or: arca serve --config {}",
                self.config
            )
        } else {
            format!("arca serve --config {}", self.config)
        }
    }
}

/// The longest a request may take on the socket, in either direction.
const TIMEOUT: Duration = Duration::from_secs(60);
/// The prefix every token carries.
const TOKEN_PREFIX: &str = "arca_";
/// The length of a token's secret: 32 bytes in unpadded base64url.
const TOKEN_SECRET_LEN: usize = 43;
/// The name of the `approval` argument, which may be passed only in the arguments file.
const APPROVAL_KEY: &str = "approval";

pub fn call(args: &[String]) -> Result<Option<Value>, Failure> {
    let (prefix_peer, args) = match args {
        [flag, peer, rest @ ..] if flag == "--peer" && !peer.starts_with("--") => {
            (Some(peer.as_str()), rest)
        }
        [flag, ..] if flag == "--peer" => {
            return Err(Failure::Usage("--peer needs a value".to_string()));
        }
        _ => (None, args),
    };
    let names = || verbs().iter().map(|verb| verb.name);
    let Some((name, rest)) = args.split_first() else {
        return Err(Failure::Usage(format!(
            "call needs a verb: arca call <verb>. The verbs are: {}",
            names().collect::<Vec<_>>().join(", ")
        )));
    };
    if verb(name).is_none() {
        // The word given is never echoed: only a known verb is named.
        return Err(Failure::Usage(match closest(name, names()) {
            Some(near) => format!(
                "the verb is not one the node answers; did you mean {near}? Run arca call alone to list the verbs"
            ),
            None => "the verb is not one the node answers: run arca call alone to list the verbs"
                .to_string(),
        }));
    }
    let flags = parse_flags(
        rest,
        &["config", "token-file", "args", "args-file", "peer"],
        &[],
    )?;
    if prefix_peer.is_some() && flags.value("peer").is_some() {
        return Err(Failure::Usage("--peer is given twice".to_string()));
    }
    let peer = flags.value("peer").or(prefix_peer);
    // The token of a peer call leaves this node, so it is the one the caller names and no other: never the installer's
    // token, and never the variable's. The config is found as every command finds it, and a peer call, which acts
    // through this node's link, says which config that is when the flag did not name it.
    let (config, cfg, token_file) = if peer.is_some() {
        let Some(token_file) = flags.value("token-file") else {
            return Err(Failure::Usage(
                "--token-file is required with --peer: the token is sent to the peer, so it is one the peer minted and the caller names, never this node's own token from the default file or from ARCA_TOKEN_FILE".to_string(),
            ));
        };
        let (config, cfg) = crate::load_config_to(&flags, crate::Does::Acts)?;
        let token_file = Chosen {
            path: PathBuf::from(token_file),
            origin: Origin::Flag,
        };
        (config, cfg, token_file)
    } else {
        let (config, cfg) = crate::load_chosen_config(&flags)?;
        (config, cfg, crate::token_path(&flags)?)
    };
    let node = Addressed::new(&config, peer.is_some());
    let token = read_token(&token_file, &node)?;
    let arguments = call_arguments(&flags)?;
    let socket = if peer.is_some() {
        cfg.state_dir.join(".link-control.sock")
    } else {
        cfg.listen.unix
    };
    run_call(&socket, peer, &token, name, arguments, &node)
}

/// The arguments a call sends. `--args` is a JSON object that may not carry an approval; `--args-file` is one, and may.
fn call_arguments(flags: &Flags) -> Result<Value, Failure> {
    match (flags.value("args"), flags.value("args-file")) {
        (Some(_), Some(_)) => Err(Failure::Usage(
            "--args and --args-file are not both given".to_string(),
        )),
        (Some(text), None) => {
            let value = object(text)?;
            if value.get(APPROVAL_KEY).is_some() {
                return Err(Failure::Usage(
                    "an approval is not passed in --args; pass it in --args-file".to_string(),
                ));
            }
            Ok(value)
        }
        (None, Some(path)) => object(&read_private(
            Path::new(path),
            "the arguments file",
            "--args-file names a JSON file that holds the arguments",
        )?),
        (None, None) => Ok(json!({})),
    }
}

fn object(text: &str) -> Result<Value, Failure> {
    match serde_json::from_str::<Value>(text) {
        Ok(value) if value.is_object() => Ok(value),
        _ => Err(Failure::Usage(
            "the arguments are not a JSON object".to_string(),
        )),
    }
}

/// The token in its file, with the trailing newline removed. Its shape is checked; its text never leaves this function.
/// A path the caller typed (in the flag or the variable) is never named in a refusal; the installer's own path is, since
/// nobody typed it and the reader cannot know which file was looked for.
pub(crate) fn read_token(file: &Chosen, node: &Addressed) -> Result<String, Failure> {
    let advice = node.token_advice(file);
    let text = match file.origin {
        Origin::Default => read_private(
            &file.path,
            &format!(
                "the token file {}, where the installer writes it,",
                file.path.display()
            ),
            &advice,
        ),
        Origin::Flag | Origin::Variable => read_private(&file.path, "the token file", &advice),
    }?;
    let token = text.trim_end();
    let secret = token.strip_prefix(TOKEN_PREFIX).unwrap_or_default();
    let shaped = secret.len() == TOKEN_SECRET_LEN
        && secret
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    if !shaped {
        return Err(Failure::Usage(format!(
            "the token file does not hold a token: a token is one line, {TOKEN_PREFIX} and {TOKEN_SECRET_LEN} characters, as arca token mint writes it. For a new one, {advice}"
        )));
    }
    Ok(token.to_string())
}

/// The text of a file only its owner may read: a regular file with no group or other permission bit. The text is never echoed.
/// `role` names the file in a refusal (the token file or the arguments file); the path is in it only when the caller put it
/// there. Each refusal says what is wrong with the file, and `make` says how to get one when there is none.
fn read_private(path: &Path, role: &str, make: &str) -> Result<String, Failure> {
    let meta = fs::symlink_metadata(path).map_err(|error| {
        Failure::Usage(match error.kind() {
            ErrorKind::NotFound => format!("{role} does not exist: {make}"),
            ErrorKind::PermissionDenied => {
                format!("{role} cannot be reached: this user may not read the directory it is in")
            }
            _ => format!("{role} cannot be read"),
        })
    })?;
    if !meta.is_file() {
        return Err(Failure::Usage(format!(
            "{role} is not a regular file: a directory or a link is refused. {make}"
        )));
    }
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(Failure::Usage(format!(
            "{role} can be read by its group or by others: run chmod 600 on it, so only its owner reads it"
        )));
    }
    fs::read_to_string(path).map_err(|error| {
        Failure::Usage(match error.kind() {
            ErrorKind::PermissionDenied => {
                format!("{role} belongs to another user: this user may not read it")
            }
            _ => format!("{role} cannot be read as text"),
        })
    })
}

/// The two requests of a call: `initialize`, then `tools/call` for `verb`.
pub(crate) fn run_call(
    socket: &Path,
    peer: Option<&str>,
    token: &str,
    verb: &str,
    arguments: Value,
    node: &Addressed,
) -> Result<Option<Value>, Failure> {
    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "arca", "version": env!("CARGO_PKG_VERSION") },
        },
    });
    let (status, _) = post(socket, peer, token, &initialize, node)?;
    accepted(status, verb, node)?;
    let request = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": { "name": verb, "arguments": arguments },
    });
    let (status, body) = post(socket, peer, token, &request, node)?;
    accepted(status, verb, node)?;
    let reply: Value = serde_json::from_slice(&body)
        .map_err(|_| Failure::Runtime("the node's reply is not JSON".to_string()))?;
    if let Some(error) = reply.get("error") {
        return Err(Failure::Tool(error.clone()));
    }
    let result = reply
        .get("result")
        .ok_or_else(|| Failure::Runtime("the node's reply has no result".to_string()))?;
    let content = result
        .get("structuredContent")
        .cloned()
        .unwrap_or(Value::Null);
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return Err(Failure::Tool(content));
    }
    Ok(Some(content))
}

/// A reply's status as a call's outcome: 200 goes on, 401 and 403 are the node refusing the token, and anything else is the
/// node failing to answer. A refusal of the token says how to get one the node takes, on the node that was asked: a peer's
/// tokens are its owner's to mint, and a token minted here is not read by a running node until it is told to.
fn accepted(status: u16, name: &str, node: &Addressed) -> Result<(), Failure> {
    match status {
        200 => Ok(()),
        401 if node.peer => Err(Failure::Unauthorized(
            "the peer does not accept this token: a call with --peer needs a token the peer minted, and the peer's node reads a new token only when it starts or on SIGHUP. Ask the peer's owner for one their node has read".to_string(),
        )),
        401 => Err(Failure::Unauthorized(format!(
            "the node does not accept this token: it was minted after the node last read its tokens, it was revoked, or it is another node's. If it was just minted, have the node read it: {}. For a new one: {}",
            node.reload(),
            node.mint("viewer", "<file>")
        ))),
        403 if node.peer => Err(Failure::Unauthorized(format!(
            "the token's scope may not make this call on the peer: ask the peer's owner for a token of a higher scope, or one whose --tools list names {name}"
        ))),
        403 => {
            let scope = verb(name)
                .map(|verb| verb.min_scope.as_str())
                .unwrap_or("operator");
            Err(Failure::Unauthorized(format!(
                "the token's scope may not make this call: {name} needs a token of the {scope} scope, and a token minted with --tools must list it. An owner mints one with: {}",
                node.mint(scope, "<file>")
            )))
        }
        _ => Err(Failure::Runtime(
            "the node did not answer the call".to_string(),
        )),
    }
}

/// One POST of `body` to the node's `/mcp` on the unix socket. The reply is read to its end, and its status and body returned.
fn post(
    socket: &Path,
    peer: Option<&str>,
    token: &str,
    body: &Value,
    node: &Addressed,
) -> Result<(u16, Vec<u8>), Failure> {
    let unreachable = || Failure::Runtime("the node's socket does not answer".to_string());
    let mut stream = UnixStream::connect(socket).map_err(|_| {
        if peer.is_some() {
            Failure::Runtime(format!(
                "the link is not running: start it with: arca link serve --config {}, and keep it running",
                node.config
            ))
        } else {
            Failure::Runtime(format!(
                "the node's socket does not answer: the node is not running, or listen.unix in its config names another socket. Start the node with: {}",
                node.start()
            ))
        }
    })?;
    stream
        .set_read_timeout(Some(TIMEOUT))
        .and_then(|()| stream.set_write_timeout(Some(TIMEOUT)))
        .map_err(|_| unreachable())?;
    if let Some(peer) = peer {
        let request = format!("{}\n", json!({"dial": peer}));
        stream
            .write_all(request.as_bytes())
            .map_err(|_| unreachable())?;
        // Read only the control line; the following bytes belong to the HTTP response.
        let mut line = Vec::new();
        for _ in 0..4096 {
            let mut byte = [0];
            stream.read_exact(&mut byte).map_err(|_| unreachable())?;
            line.push(byte[0]);
            if byte[0] == b'\n' {
                break;
            }
        }
        if line.last() != Some(&b'\n') {
            return Err(Failure::Runtime("invalid link control reply".to_string()));
        }
        let reply: Value = serde_json::from_slice(&line)
            .map_err(|_| Failure::Runtime("invalid link control reply".to_string()))?;
        if let Some(error) = reply.get("error").and_then(Value::as_str) {
            return Err(Failure::Runtime(error.to_string()));
        }
        if reply.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(Failure::Runtime("invalid link control reply".to_string()));
        }
    }
    let payload = body.to_string();
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: arca\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|_| unreachable())?;
    // Content-Length frames the POST. Keep the write side open while awaiting
    // the reply: forwarding an EOF can cancel the HTTP server's pending response.
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply).map_err(|_| unreachable())?;
    split_reply(&reply)
}

/// The status and body of a reply. A chunked reply is refused, since the node answers every call with a length.
pub(crate) fn split_reply(reply: &[u8]) -> Result<(u16, Vec<u8>), Failure> {
    let malformed = || Failure::Runtime("the node's reply is malformed".to_string());
    let end = reply
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(malformed)?;
    let head = std::str::from_utf8(&reply[..end]).map_err(|_| malformed())?;
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(malformed)?;
    let chunked = head
        .lines()
        .any(|line| line.to_ascii_lowercase().starts_with("transfer-encoding:"));
    if chunked {
        return Err(malformed());
    }
    Ok((status, reply[end + 4..].to_vec()))
}
