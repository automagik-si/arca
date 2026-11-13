//! Asking the root part's socket. The user-level node reads and applies through these two calls and has no other way in.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use crate::socket::{APPLY, READ};
use crate::status::Status;

/// How long a call waits for an answer. An apply runs a handful of commands.
const PATIENCE: Duration = Duration::from_secs(30);

/// Why a call has no status.
#[derive(Debug, PartialEq, Eq)]
pub enum ClientError {
    /// Nothing answers on the socket: the unit is not installed or not running.
    Unreachable(String),
    /// The root part refused the request, with its reason.
    Refused(String),
    /// The answer is not one the root part gives.
    Malformed,
}

#[derive(Deserialize)]
struct Reply {
    ok: bool,
    status: Option<Status>,
    error: Option<String>,
}

fn call(socket: &Path, verb: &str) -> Result<Status, ClientError> {
    let mut stream =
        UnixStream::connect(socket).map_err(|error| ClientError::Unreachable(error.to_string()))?;
    stream
        .set_read_timeout(Some(PATIENCE))
        .and_then(|()| stream.set_write_timeout(Some(PATIENCE)))
        .map_err(|error| ClientError::Unreachable(error.to_string()))?;
    stream
        .write_all(format!("{verb}\n").as_bytes())
        .map_err(|error| ClientError::Unreachable(error.to_string()))?;
    let mut bytes = Vec::new();
    stream
        .take(1024 * 1024)
        .read_to_end(&mut bytes)
        .map_err(|error| ClientError::Unreachable(error.to_string()))?;
    let reply: Reply = serde_json::from_slice(&bytes).map_err(|_| ClientError::Malformed)?;
    match (reply.ok, reply.status, reply.error) {
        (true, Some(status), _) => Ok(status),
        (false, _, Some(error)) => Err(ClientError::Refused(error)),
        _ => Err(ClientError::Malformed),
    }
}

/// Reads the host against the declared file, live.
pub fn read(socket: &Path) -> Result<Status, ClientError> {
    call(socket, READ)
}

/// Asks the root part to put back the declared state, and returns what it read back after.
pub fn apply(socket: &Path) -> Result<Status, ClientError> {
    call(socket, APPLY)
}
