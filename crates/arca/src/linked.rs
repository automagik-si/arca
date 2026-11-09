//! The approval page's relay to the linked kernels on this machine (`approval.linkedHosts`). The page lists a linked
//! kernel's waiting proposals with `plan_list` and hands the owner's signed approval to that kernel's `action_approve`, or
//! a decline to its `plan_decline`, over the kernel's own MCP unix socket. The page never checks, spends or stores a
//! linked kernel's approval: the target verifies it with its own verifier, nonce store and sign counts, and its answer is
//! returned as it came.
//!
//! The token is read from its file at every call, and sent only in the bearer header. It is never logged or returned.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use serde_json::{Value, json};

use crate::config::LinkedHost;

/// The longest a relayed approval or decline may take, in either direction: the linked kernel runs the action before it
/// answers.
const TIMEOUT: Duration = Duration::from_secs(30);
/// The longest a listing may take. The canvas polls it, so a linked kernel that hangs must not hold the page's own list.
const LIST_TIMEOUT: Duration = Duration::from_secs(5);
/// The largest reply read from a linked kernel.
const REPLY_MAX: u64 = 4 * 1024 * 1024;
/// A JSON-RPC refusal from the MCP door: `CallError::Refused`.
const REFUSED: i64 = -32000;

/// Why a relayed call did not return a result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayError {
    /// The linked kernel's socket does not answer, or its reply is not one the door writes.
    Unreachable,
    /// The token file cannot be read, or the linked kernel refuses the token or its scope.
    Token,
    /// The linked kernel refused the call: its refusal code and message, as it sent them.
    Refused { code: String, message: String },
    /// The linked kernel found the call's arguments invalid, or failed inside the tool.
    Failed(String),
}

/// Calls `verb` with `arguments` on the linked kernel `host`, and returns its structured result.
pub fn call(host: &LinkedHost, verb: &str, arguments: Value) -> Result<Value, RelayError> {
    let token = read_token(host)?;
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": verb, "arguments": arguments },
    });
    let timeout = if verb == "plan_list" {
        LIST_TIMEOUT
    } else {
        TIMEOUT
    };
    let (status, body) = post(host, &token, &request, timeout)?;
    match status {
        200 => {}
        401 | 403 => return Err(RelayError::Token),
        _ => return Err(RelayError::Unreachable),
    }
    let reply: Value = serde_json::from_slice(&body).map_err(|_| RelayError::Unreachable)?;
    if let Some(error) = reply.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("the linked machine refused the call")
            .to_string();
        if error.get("code").and_then(Value::as_i64) == Some(REFUSED) {
            let code = error
                .pointer("/data/refusal")
                .and_then(Value::as_str)
                .unwrap_or("Refused")
                .to_string();
            return Err(RelayError::Refused { code, message });
        }
        return Err(RelayError::Failed(message));
    }
    let result = reply.get("result").ok_or(RelayError::Unreachable)?;
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return Err(RelayError::Failed(
            "the linked machine could not complete the call".to_string(),
        ));
    }
    result
        .get("structuredContent")
        .cloned()
        .ok_or(RelayError::Unreachable)
}

/// The token in the host's token file: a regular file with no group or other permission bit, trimmed of its newline.
fn read_token(host: &LinkedHost) -> Result<String, RelayError> {
    let meta = fs::symlink_metadata(&host.token_file).map_err(|_| RelayError::Token)?;
    if !meta.is_file() || meta.permissions().mode() & 0o077 != 0 {
        return Err(RelayError::Token);
    }
    let text = fs::read_to_string(&host.token_file).map_err(|_| RelayError::Token)?;
    let token = text.trim_end();
    let shaped = token.starts_with("arca_")
        && token.len() > "arca_".len()
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    if !shaped {
        return Err(RelayError::Token);
    }
    Ok(token.to_string())
}

/// One POST of `body` to the linked kernel's `/mcp`. The reply is read to its end, up to [`REPLY_MAX`].
fn post(
    host: &LinkedHost,
    token: &str,
    body: &Value,
    timeout: Duration,
) -> Result<(u16, Vec<u8>), RelayError> {
    let mut stream = UnixStream::connect(&host.socket).map_err(|_| RelayError::Unreachable)?;
    stream
        .set_read_timeout(Some(timeout))
        .and_then(|()| stream.set_write_timeout(Some(timeout)))
        .map_err(|_| RelayError::Unreachable)?;
    let payload = body.to_string();
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: arca\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|_| RelayError::Unreachable)?;
    let mut reply = Vec::new();
    stream
        .take(REPLY_MAX)
        .read_to_end(&mut reply)
        .map_err(|_| RelayError::Unreachable)?;
    let end = reply
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or(RelayError::Unreachable)?;
    let head = std::str::from_utf8(&reply[..end]).map_err(|_| RelayError::Unreachable)?;
    if head
        .lines()
        .any(|line| line.to_ascii_lowercase().starts_with("transfer-encoding:"))
    {
        return Err(RelayError::Unreachable);
    }
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or(RelayError::Unreachable)?;
    Ok((status, reply[end + 4..].to_vec()))
}
