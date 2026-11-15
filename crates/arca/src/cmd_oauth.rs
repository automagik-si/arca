//! `oauth list` and `oauth revoke <client>`: the MCP clients the owner connected through the approval page (see
//! `crate::mcp_oauth`), on this terminal.
//!
//! The command asks the node that `serve` runs, over its unix socket, as `arca setup-code` does: it first makes an empty
//! file with a random name in the node's state directory, which only the node's user can, and sends that name. `list`
//! prints every client that is not revoked, as JSON; `revoke` ends every grant of one client, at once.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

use crate::approve_page::{LOCAL_PROOF_LEN, LOCAL_PROOF_PREFIX};
use crate::mcp_oauth::{LOCAL_CLIENTS_PATH, LOCAL_REVOKE_PATH};
use crate::{Failure, Flags};

/// The longest the node may take to answer.
const TIMEOUT: Duration = Duration::from_secs(10);

/// `oauth list`: every client that is not revoked, with its grants.
pub fn list(flags: &Flags) -> Result<Option<Value>, Failure> {
    let body = ask(flags, LOCAL_CLIENTS_PATH, json!({}))?;
    Ok(Some(body))
}

/// `oauth revoke <client>`: every grant of the client ends, and it must register and be approved again.
pub fn revoke(client: &str, flags: &Flags) -> Result<Option<Value>, Failure> {
    let body = ask(flags, LOCAL_REVOKE_PATH, json!({ "clientId": client }))?;
    eprintln!("arca: {client} is disconnected: its tokens no longer work");
    Ok(Some(body))
}

fn ask(flags: &Flags, path: &str, mut body: Value) -> Result<Value, Failure> {
    let (_, cfg) = crate::load_config_to(flags, crate::Does::Acts)?;
    if cfg.approval_origin.is_none() {
        return Err(Failure::Config(
            "this node has no approval page, so no app was connected through it: approvalOrigin is not set in its config"
                .to_string(),
        ));
    }
    let proof = fresh_proof()?;
    let file = cfg.state_dir.join(format!("{LOCAL_PROOF_PREFIX}{proof}"));
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&file)
        .map_err(|_| {
            Failure::Runtime(format!(
                "the node's state directory {} cannot be written: run this as the user who runs the node",
                cfg.state_dir.display()
            ))
        })?;
    body["proof"] = json!(proof);
    let answered = post(&cfg.listen.unix, path, &body);
    let _ = fs::remove_file(&file);
    let (status, reply) = answered?;
    let reply: Value = serde_json::from_slice(&reply).unwrap_or(Value::Null);
    match status {
        200 => Ok(reply),
        404 if reply.get("refusal").is_none() => Err(Failure::Runtime(
            "the node that runs does not offer this on its socket: it is an older program, or it started before approvalOrigin was set; start it again"
                .to_string(),
        )),
        _ => Err(Failure::Refused(
            reply
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("the node refused")
                .to_string(),
        )),
    }
}

/// 16 random bytes from the operating system, in lowercase hexadecimal.
fn fresh_proof() -> Result<String, Failure> {
    let mut bytes = [0u8; LOCAL_PROOF_LEN / 2];
    getrandom::fill(&mut bytes).map_err(|_| {
        Failure::Runtime("the operating system's random source is not available".to_string())
    })?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// One POST to the node's unix socket. The reply's status and body.
fn post(socket: &Path, path: &str, body: &Value) -> Result<(u16, Vec<u8>), Failure> {
    let unreachable =
        || Failure::Runtime("the node's socket does not answer: is the node running?".to_string());
    let mut stream = UnixStream::connect(socket).map_err(|_| unreachable())?;
    stream
        .set_read_timeout(Some(TIMEOUT))
        .and_then(|()| stream.set_write_timeout(Some(TIMEOUT)))
        .map_err(|_| unreachable())?;
    let payload = body.to_string();
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: arca\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|_| unreachable())?;
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply).map_err(|_| unreachable())?;
    crate::cmd_call::split_reply(&reply)
}
