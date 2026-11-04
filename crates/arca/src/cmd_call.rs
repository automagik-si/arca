//! `call <verb> [--peer <name>] --config <file> --token-file <file> [--args <json> | --args-file <file>]`: one tool call against the node that
//! `serve` runs. The token is read from its own file and sent only in the bearer header of each request. An approval is read
//! from the arguments file, never from the command line, and neither the token nor an approval is ever printed.
//!
//! Each request uses its own connection, since the server answers `Connection: close`. The call first sends `initialize`, so
//! a token the node refuses is found before the verb is named again.

use std::fs;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use arca_api::verb;
use arca_mcp::PROTOCOL_VERSION;
use serde_json::{Value, json};

use crate::config;
use crate::{Failure, Flags, parse_flags};

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
    let Some((name, rest)) = args.split_first() else {
        return Err(crate::usage_failure());
    };
    if verb(name).is_none() {
        return Err(Failure::Usage(
            "the verb is not one the node answers".to_string(),
        ));
    }
    let flags = parse_flags(
        rest,
        &["config", "token-file", "args", "args-file", "peer"],
        &[],
    )?;
    if prefix_peer.is_some() && flags.value("peer").is_some() {
        return Err(Failure::Usage("--peer is given twice".to_string()));
    }
    let cfg = config::load(Path::new(flags.required("config")?))?;
    let token = read_token(Path::new(flags.required("token-file")?))?;
    let arguments = call_arguments(&flags)?;
    let peer = flags.value("peer").or(prefix_peer);
    let socket = if peer.is_some() {
        cfg.state_dir.join(".link-control.sock")
    } else {
        cfg.listen.unix
    };
    run_call(&socket, peer, &token, name, arguments)
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
        (None, Some(path)) => object(&read_private(Path::new(path), "the arguments file")?),
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
fn read_token(path: &Path) -> Result<String, Failure> {
    let text = read_private(path, "the token file")?;
    let token = text.trim_end();
    let secret = token.strip_prefix(TOKEN_PREFIX).unwrap_or_default();
    let shaped = secret.len() == TOKEN_SECRET_LEN
        && secret
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    if !shaped {
        return Err(Failure::Usage(
            "the token file does not hold a token".to_string(),
        ));
    }
    Ok(token.to_string())
}

/// The text of a file only its owner may read: a regular file with no group or other permission bit. The text is never echoed.
/// `role` names the file in a refusal (the token file or the arguments file), so the message names no path.
fn read_private(path: &Path, role: &str) -> Result<String, Failure> {
    let refused = || {
        Failure::Usage(format!(
            "{role} must be a regular file with no group or other permission bit"
        ))
    };
    let meta = fs::symlink_metadata(path).map_err(|_| refused())?;
    if !meta.is_file() || meta.permissions().mode() & 0o077 != 0 {
        return Err(refused());
    }
    fs::read_to_string(path).map_err(|_| refused())
}

/// The two requests of a call: `initialize`, then `tools/call` for `verb`.
fn run_call(
    socket: &Path,
    peer: Option<&str>,
    token: &str,
    verb: &str,
    arguments: Value,
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
    let (status, _) = post(socket, peer, token, &initialize)?;
    accepted(status)?;
    let request = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": { "name": verb, "arguments": arguments },
    });
    let (status, body) = post(socket, peer, token, &request)?;
    accepted(status)?;
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
/// node failing to answer.
fn accepted(status: u16) -> Result<(), Failure> {
    match status {
        200 => Ok(()),
        401 => Err(Failure::Unauthorized(
            "the node does not accept this token".to_string(),
        )),
        403 => Err(Failure::Unauthorized(
            "the token's scope may not make this call".to_string(),
        )),
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
) -> Result<(u16, Vec<u8>), Failure> {
    let unreachable = || Failure::Runtime("the node's socket does not answer".to_string());
    let mut stream = UnixStream::connect(socket).map_err(|_| {
        if peer.is_some() {
            Failure::Runtime("the link is not running".to_string())
        } else {
            unreachable()
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
    if peer.is_some() {
        stream
            .shutdown(Shutdown::Write)
            .map_err(|_| unreachable())?;
    }
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply).map_err(|_| unreachable())?;
    split_reply(&reply)
}

/// The status and body of a reply. A chunked reply is refused, since the node answers every call with a length.
fn split_reply(reply: &[u8]) -> Result<(u16, Vec<u8>), Failure> {
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
