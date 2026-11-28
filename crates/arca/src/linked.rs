//! The approval page's relay to the linked kernels (`approval.linkedHosts`): on this machine, or on another machine whose
//! MCP socket is forwarded to a path on this one (an SSH-forwarded unix socket, say). The page lists a linked
//! kernel's waiting proposals with `plan_list` and hands the owner's signed approval to that kernel's `action_approve`, or
//! a decline to its `plan_decline`, over the kernel's own MCP unix socket. Owner approval request steps use the same
//! relay. The page never checks, spends or stores a
//! linked kernel's approval: the target verifies it with its own verifier, nonce store and sign counts, and its answer is
//! returned as it came.
//!
//! The token is read from its file at every call, without following a link, and sent only in the bearer header. It is
//! never logged or returned.

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use arca_mcp::CallError;

use crate::config::LinkedHost;

/// The longest a relayed decline or any other relayed write may take, in either direction.
const TIMEOUT: Duration = Duration::from_secs(30);
/// The margin a relayed approval waits beyond the longest action it can run.
const APPROVAL_MARGIN: Duration = Duration::from_secs(30);
/// The longest a relayed approval may take: the linked kernel runs the approved action (or the plan's delete) before it
/// answers, and the door sends nothing until then. A graceful instance stop or restart may take its whole call budget, so
/// the relay waits past it: a slow stop still answers through the relay, as done or as its own refusal, never as "could
/// not be reached" while it goes on. An action with its own longer wait gets that too ([`approval_wait`]).
pub const APPROVAL_TIMEOUT: Duration =
    Duration::from_secs(arca_incus::CALL_BUDGET.as_secs() + APPROVAL_MARGIN.as_secs());
/// The longest a relayed `github_link` step may take: four of GitHub's own request limits, so a slow GitHub answers through
/// the relay rather than as "could not be reached" while the node goes on.
pub const LINK_TIMEOUT: Duration =
    Duration::from_secs(crate::github_api::REQUEST_BUDGET.as_secs() * 4);
/// The longest a listing or a status read may take. The canvas polls the list, so a linked kernel that hangs must not hold
/// the page's own list.
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
    call_within(host, verb, arguments, timeout_for(verb))
}

/// [`call`], waiting up to `timeout` for the answer: an approval waits as long as its own action can run
/// ([`approval_wait`]).
pub fn call_within(
    host: &LinkedHost,
    verb: &str,
    arguments: Value,
    timeout: Duration,
) -> Result<Value, RelayError> {
    answer_of(&rpc(
        host,
        "tools/call",
        json!({ "name": verb, "arguments": arguments }),
        timeout,
    )?)
}

/// Calls `verb` with `arguments` on the linked kernel `host`, and returns its JSON-RPC reply as it wrote it: an object
/// with its `result` or its `error`.
pub fn reply(host: &LinkedHost, verb: &str, arguments: Value) -> Result<Value, RelayError> {
    rpc(
        host,
        "tools/call",
        json!({ "name": verb, "arguments": arguments }),
        timeout_for(verb),
    )
}

/// Whether the token this kernel holds for `host` admits `verb` there, as the linked kernel's own `tools/list` says (the
/// list is the caller's own: its scope and its list of tools). `None` when the list cannot be read. It only reads.
pub fn admits(host: &LinkedHost, verb: &str) -> Option<bool> {
    let listed = rpc(host, "tools/list", json!({}), LIST_TIMEOUT).ok()?;
    let tools = listed.pointer("/result/tools")?.as_array()?;
    Some(tools.iter().any(|tool| tool["name"] == json!(verb)))
}

/// How long the page waits for a linked kernel's answer to the approval of action `action` with arguments `args`: the
/// kernel runs the action before it answers. A `service_update` can take far longer than any other action: one Incus
/// step of its rebuild may take up to `STEP_DEADLINE` (600 s, a registry pull included), its new version then has
/// `timeoutSeconds` (120 by default) to answer, and after a rollback the old version has up to `OLD_ANSWERS_SECONDS`
/// (30 s, never more than `timeoutSeconds`). It waits all of that beyond every other action's wait.
pub fn approval_wait(action: &str, args: &Value) -> Duration {
    if action != "service_update" {
        return APPROVAL_TIMEOUT;
    }
    let health = args
        .get("timeoutSeconds")
        .and_then(Value::as_u64)
        .unwrap_or(crate::handlers::DEFAULT_HEALTH_SECONDS)
        .min(crate::handlers::MAX_HEALTH_SECONDS);
    let old_answers = health.min(crate::handlers::OLD_ANSWERS_SECONDS);
    APPROVAL_TIMEOUT + crate::handlers::STEP_DEADLINE + Duration::from_secs(health + old_answers)
}

/// One JSON-RPC `method` with `params` to the linked kernel `host`, and its reply as it wrote it.
fn rpc(
    host: &LinkedHost,
    method: &str,
    params: Value,
    timeout: Duration,
) -> Result<Value, RelayError> {
    let token = read_token(host)?;
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    let (status, body) = post(host, &token, &request, timeout)?;
    match status {
        200 => {}
        401 | 403 => return Err(RelayError::Token),
        _ => return Err(RelayError::Unreachable),
    }
    let reply: Value = serde_json::from_slice(&body).map_err(|_| RelayError::Unreachable)?;
    let answered = reply.get("result").is_some_and(Value::is_object)
        || reply.get("error").is_some_and(Value::is_object);
    if !answered {
        return Err(RelayError::Unreachable);
    }
    Ok(reply)
}

/// The MCP door's relay for a call that names a linked node (`node`): this kernel's linked hosts, by name. The door has
/// checked the caller on this kernel already; the call goes to the named host's socket with its token, as every relay of
/// this module does, and the host's reply comes back as it wrote it.
pub struct LinkedNodes {
    pub hosts: Vec<LinkedHost>,
}

impl arca_mcp::LinkedNodes for LinkedNodes {
    fn relay(&self, node: &str, verb: &str, arguments: Value) -> Result<Value, CallError> {
        // The refusal names the linked nodes and never echoes the name it was given, as every argument refusal.
        let Some(host) = self.hosts.iter().find(|host| host.name == node) else {
            let names: Vec<&str> = self.hosts.iter().map(|host| host.name.as_str()).collect();
            return Err(CallError::Invalid(format!(
                "/node: not a linked node of this kernel; the linked nodes are {}",
                names.join(", ")
            )));
        };
        reply(host, verb, arguments).map_err(|error| match error {
            RelayError::Token => CallError::Refused {
                code: "LinkToken".to_string(),
                message: format!(
                    "the linked node {node} does not accept this kernel's token for {verb}"
                ),
            },
            // `reply` answers only these two; a refusal or a failure is in the reply it returns.
            _ => CallError::Refused {
                code: "Unreachable".to_string(),
                message: format!("the linked node {node} could not be reached just now"),
            },
        })
    }
}

/// A linked kernel's JSON-RPC reply as the relay's answer: its structured result, its refusal as it sent it, or why it
/// failed. Arguments it does not take are a tool result with `isError`, and their words are passed on as it wrote them;
/// a failure inside its tool is said in a plain sentence, as its own caller is told no more.
fn answer_of(reply: &Value) -> Result<Value, RelayError> {
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
        let invalid = arca_mcp::jsonrpc::INVALID_ARGUMENTS;
        let words = (result
            .pointer("/_meta/droidd~1error")
            .and_then(Value::as_str)
            == Some(invalid))
        .then(|| result.pointer("/content/0/text").and_then(Value::as_str))
        .flatten()
        .and_then(|text| text.strip_prefix(invalid)?.strip_prefix(": "));
        return Err(RelayError::Failed(words.map_or_else(
            || "the linked machine could not complete the call".to_string(),
            str::to_string,
        )));
    }
    result
        .get("structuredContent")
        .cloned()
        .ok_or(RelayError::Unreachable)
}

/// How long a relayed call of `verb` waits for the linked kernel's answer.
pub fn timeout_for(verb: &str) -> Duration {
    match verb {
        "plan_list" | "arca_status" | "owner_approval_list" => LIST_TIMEOUT,
        "action_approve" | "approval_submit" => APPROVAL_TIMEOUT,
        // GitHub's returns: the node converts GitHub's code, or reads the installation and puts its runner group in
        // place, several GitHub requests of their own limit each, before it answers.
        "github_link" => LINK_TIMEOUT,
        _ => TIMEOUT,
    }
}

/// `O_NOFOLLOW` on Linux: the open fails on a symbolic link instead of following it.
#[cfg(target_os = "linux")]
const NOFOLLOW: i32 = 0o400_000;
#[cfg(not(target_os = "linux"))]
const NOFOLLOW: i32 = 0;
/// The largest token file read, in bytes. A token is far shorter.
const TOKEN_MAX: u64 = 4096;

/// The token in the host's token file, trimmed of its newline. The file is opened read-only without following a link, and
/// its type, owner and mode are checked on the open handle, so the bytes read are the bytes checked: a regular file owned
/// by root or this kernel's user, with no group or other permission bit (it is a secret, so not even a read bit).
fn read_token(host: &LinkedHost) -> Result<String, RelayError> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(NOFOLLOW)
        .open(&host.token_file)
        .map_err(|_| RelayError::Token)?;
    let meta = file.metadata().map_err(|_| RelayError::Token)?;
    let uid = arca_approval::effective_uid().map_err(|_| RelayError::Token)?;
    if !meta.is_file()
        || (meta.uid() != 0 && meta.uid() != uid)
        || meta.mode() & 0o077 != 0
        || meta.len() > TOKEN_MAX
    {
        return Err(RelayError::Token);
    }
    let mut text = String::new();
    file.take(TOKEN_MAX)
        .read_to_string(&mut text)
        .map_err(|_| RelayError::Token)?;
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

/// One POST of `body` to the linked kernel's `/mcp`. The reply is read to its end, up to [`REPLY_MAX`], all within
/// `timeout`: one deadline for the whole reply, so a node that sends a byte at a time is cut off at it.
fn post(
    host: &LinkedHost,
    token: &str,
    body: &Value,
    timeout: Duration,
) -> Result<(u16, Vec<u8>), RelayError> {
    let deadline = Instant::now() + timeout;
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
    let mut chunk = [0u8; 8192];
    while (reply.len() as u64) < REPLY_MAX {
        let left = deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
            .ok_or(RelayError::Unreachable)?;
        stream
            .set_read_timeout(Some(left))
            .map_err(|_| RelayError::Unreachable)?;
        let room = (REPLY_MAX - reply.len() as u64).min(chunk.len() as u64) as usize;
        match stream.read(&mut chunk[..room]) {
            Ok(0) => break,
            Ok(read) => reply.extend_from_slice(&chunk[..read]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return Err(RelayError::Unreachable),
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Arguments the linked kernel does not take come back in its own words; its tool's own failure in a plain sentence.
    #[test]
    fn a_linked_kernels_invalid_arguments_are_passed_on_in_its_words() {
        let failed = |meta: &str, text: &str| {
            answer_of(&json!({ "jsonrpc": "2.0", "id": 1, "result": {
                "isError": true,
                "content": [{ "type": "text", "text": text }],
                "_meta": { "droidd/error": meta }
            }}))
        };
        assert_eq!(
            failed(
                "invalid_arguments",
                "invalid_arguments: /plan: does not match the pattern"
            ),
            Err(RelayError::Failed(
                "/plan: does not match the pattern".to_string()
            ))
        );
        assert_eq!(
            failed("internal", "internal: the tool failed"),
            Err(RelayError::Failed(
                "the linked machine could not complete the call".to_string()
            ))
        );
        assert_eq!(
            answer_of(&json!({ "jsonrpc": "2.0", "id": 1, "result": {
                "isError": false, "structuredContent": { "plans": [] }
            }})),
            Ok(json!({ "plans": [] }))
        );
    }

    #[test]
    fn a_relayed_approval_waits_past_the_longest_action_it_can_run() {
        // An approved instance stop gives Incus 60 s and may take its whole call budget; the relay must outlast it, or the
        // owner reads "could not be reached" while the stop goes on.
        let approval = timeout_for("action_approve");
        assert!(approval >= arca_incus::CALL_BUDGET + APPROVAL_MARGIN);
        assert!(approval > Duration::from_secs(arca_incus::GRACE_SECONDS));
        // A relayed delete plan waits as an action does.
        assert_eq!(timeout_for("approval_submit"), APPROVAL_TIMEOUT);
        // The polled reads stay short, and a decline runs nothing.
        assert_eq!(timeout_for("plan_list"), LIST_TIMEOUT);
        assert_eq!(timeout_for("arca_status"), LIST_TIMEOUT);
        assert_eq!(timeout_for("plan_decline"), TIMEOUT);
    }

    #[test]
    fn a_service_update_waits_for_one_slow_step_and_both_health_checks_beyond_the_approval_wait() {
        let step = crate::handlers::STEP_DEADLINE;
        // The 2026-11-15 card: service_update with timeoutSeconds 300 ran past the old flat 180 s. It waits one 600 s
        // Incus step, 300 s for the new version and 30 s for the old one after a rollback.
        let card = approval_wait("service_update", &json!({ "timeoutSeconds": 300 }));
        assert_eq!(card, APPROVAL_TIMEOUT + step + Duration::from_secs(330));
        assert_eq!(card, Duration::from_secs(310 + 600 + 330));
        // Without timeoutSeconds the update checks for its default 120 s.
        assert_eq!(
            approval_wait("service_update", &json!({})),
            APPROVAL_TIMEOUT + step + Duration::from_secs(150)
        );
        // A timeout under 30 s gives the old version no more than that.
        assert_eq!(
            approval_wait("service_update", &json!({ "timeoutSeconds": 10 })),
            APPROVAL_TIMEOUT + step + Duration::from_secs(20)
        );
        // A value past the most the update takes waits no longer than that most.
        assert_eq!(
            approval_wait("service_update", &json!({ "timeoutSeconds": 100_000 })),
            APPROVAL_TIMEOUT + step + Duration::from_secs(crate::handlers::MAX_HEALTH_SECONDS + 30)
        );
        // Every other action waits as before.
        assert_eq!(
            approval_wait("instance_stop", &json!({ "timeoutSeconds": 300 })),
            APPROVAL_TIMEOUT
        );
    }
}
