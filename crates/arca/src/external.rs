//! The record of programs that run outside Incus, `<stateDir>/external.jsonl`: a host program that this node only knows
//! about, such as a LaunchAgent on the machine that holds the Incus VM. A line is one event, `add` or `remove`, written
//! whole and synced; the file is its own journal, and the programs it records now are what its lines leave when they are
//! read in order. Nothing here touches the machine: a record is a name, a kind, an address and a label, and `status` asks
//! one thing of the program, whether a TCP connection to its host and port opens within three seconds.
//!
//! The file is the owner's alone, as the adoption record is: a link, a file that is not regular, one not owned by this user,
//! or one whose mode is not 0600 is refused. A torn last line, which a crash mid-write leaves, was never synced as a whole
//! line, so it is dropped.

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The record's file name in the state directory.
pub const EXTERNAL_FILE: &str = "external.jsonl";

/// The one kind there is.
pub const HOST_PROGRAM: &str = "host-program";

/// How long the reachability check waits, in all.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// The words every row of a recorded program carries.
pub const NOTE: &str = "Recorded by this node, not managed by Arca: Arca does not start, stop, update or publish it. external_remove removes the record.";

/// One recorded program.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct External {
    pub name: String,
    pub kind: String,
    pub address: String,
    /// The name of a tool the program answers. A label: nothing calls it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "camelCase")]
enum Line {
    Add { at: u64, record: External },
    Remove { at: u64, name: String },
}

fn refused(why: &str) -> String {
    format!("the record of programs outside Incus ({EXTERNAL_FILE}) {why}")
}

/// Whether the file at `path` may be trusted: absent, or a regular file of this user's with mode 0600.
fn checked(path: &Path) -> Result<bool, String> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(refused(&format!("cannot be read: {error}"))),
    };
    if !meta.file_type().is_file() {
        return Err(refused("is not a regular file"));
    }
    if meta.uid() != rustix::process::getuid().as_raw() {
        return Err(refused("is not owned by the user this node runs as"));
    }
    if meta.mode() & 0o777 != 0o600 {
        return Err(refused("must have mode 0600"));
    }
    Ok(true)
}

fn lines(path: &Path) -> Result<Vec<Line>, String> {
    if !checked(path)? {
        return Ok(Vec::new());
    }
    let text =
        fs::read_to_string(path).map_err(|error| refused(&format!("cannot be read: {error}")))?;
    // The text after the last newline is empty, or a line a crash cut short: never synced as a whole line, so dropped.
    let mut parts: Vec<&str> = text.split('\n').collect();
    parts.pop();
    let mut out = Vec::new();
    for part in parts {
        out.push(
            serde_json::from_str::<Line>(part)
                .map_err(|_| refused("holds a line that does not read"))?,
        );
    }
    Ok(out)
}

/// The programs the record holds now, by name.
pub fn read(state_dir: &Path) -> Result<Vec<External>, String> {
    let mut held: Vec<External> = Vec::new();
    for line in lines(&state_dir.join(EXTERNAL_FILE))? {
        match line {
            Line::Add { record, .. } => {
                held.retain(|other| other.name != record.name);
                held.push(record);
            }
            Line::Remove { name, .. } => held.retain(|other| other.name != name),
        }
    }
    held.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(held)
}

/// The record named `name`, if the file holds one.
pub fn find(state_dir: &Path, name: &str) -> Result<Option<External>, String> {
    Ok(read(state_dir)?
        .into_iter()
        .find(|record| record.name == name))
}

fn append(state_dir: &Path, line: &Line) -> Result<(), String> {
    let path = state_dir.join(EXTERNAL_FILE);
    let existed = checked(&path)?;
    let mut text = serde_json::to_string(line).map_err(|_| refused("cannot be written"))?;
    text.push('\n');
    let mut file = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(&path)
        .map_err(|error| refused(&format!("cannot be written: {error}")))?;
    // A torn last line was never a whole line: it goes before the new one is written, so the two are not joined.
    if existed
        && let Ok(bytes) = fs::read(&path)
        && bytes.last().is_some_and(|last| *last != b'\n')
    {
        let whole = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |at| at + 1);
        file.set_len(whole as u64)
            .map_err(|error| refused(&format!("cannot be written: {error}")))?;
    }
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|error| refused(&format!("cannot be written: {error}")))
}

/// Records `record`, in place of one of the same name.
pub fn add(state_dir: &Path, record: External, now_ms: u64) -> Result<(), String> {
    append(state_dir, &Line::Add { at: now_ms, record })
}

/// Takes the record named `name` out. `false` when the record held none.
pub fn remove(state_dir: &Path, name: &str, now_ms: u64) -> Result<bool, String> {
    if find(state_dir, name)?.is_none() {
        return Ok(false);
    }
    append(
        state_dir,
        &Line::Remove {
            at: now_ms,
            name: name.to_string(),
        },
    )?;
    Ok(true)
}

/// The host and the port of an `http` or `https` address, or why it is not one. No user, no password; the port is the
/// scheme's own when the address names none.
pub fn host_port(address: &str) -> Result<(String, u16), String> {
    let (scheme, rest) = address
        .split_once("://")
        .ok_or_else(|| "address: expected an http or https URL".to_string())?;
    let default = match scheme {
        "http" => 80,
        "https" => 443,
        _ => return Err("address: expected an http or https URL".into()),
    };
    if address.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("address: expected an http or https URL without spaces".into());
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains('@') {
        return Err("address: a user name or password is not part of an address".into());
    }
    let (host, port) = if let Some(inner) = authority.strip_prefix('[') {
        let (host, after) = inner
            .split_once(']')
            .ok_or_else(|| "address: the bracketed host is not closed".to_string())?;
        if !after.is_empty() && !after.starts_with(':') {
            return Err("address: expected a port after the bracketed host".into());
        }
        (host, after.strip_prefix(':'))
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    if host.is_empty() {
        return Err("address: expected a host".into());
    }
    let port = match port {
        None | Some("") => default,
        Some(port) => port
            .parse::<u16>()
            .ok()
            .filter(|port| *port > 0)
            .ok_or_else(|| "address: the port is not a number from 1 to 65535".to_string())?,
    };
    Ok((host.to_string(), port))
}

/// Whether a TCP connection to the program's host and port opens within [`PROBE_TIMEOUT`]. It sends nothing.
pub fn reachable(address: &str) -> bool {
    let Ok((host, port)) = host_port(address) else {
        return false;
    };
    let deadline = Instant::now() + PROBE_TIMEOUT;
    // The name lookup has no timeout of its own, so it runs apart and is waited for only until the deadline.
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = send.send((host.as_str(), port).to_socket_addrs().map(Iterator::collect::<Vec<_>>));
    });
    let Ok(Ok(addresses)) = receive.recv_timeout(PROBE_TIMEOUT) else {
        return false;
    };
    for target in addresses {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        if TcpStream::connect_timeout(&target, left).is_ok() {
            return true;
        }
    }
    false
}

/// The row `service_list` and `service_status` show for a recorded program. `state` is `recorded` where nothing was
/// asked of the program, and `up` or `down` where its address was tried.
pub fn view(record: &External, state: &str) -> Value {
    let mut view = json!({
        "name": record.name,
        "template": "",
        "state": state,
        "ipv4": "",
        "ports": [],
        "links": [],
        "published": null,
        "lanPort": null,
        "kind": record.kind,
        "address": record.address,
        "managed": false,
        "recorded": true,
        "note": NOTE
    });
    if let Some(health) = &record.health {
        view["health"] = json!(health);
    }
    view
}

/// What a verb that acts on a service says when it names a recorded program.
pub fn refusal(name: &str, verb: &str) -> String {
    format!(
        "{name} is a {HOST_PROGRAM} that this node only records, not a service Arca manages, so {verb} does not act on it. \
         Nothing was changed. external_remove {name} removes the record"
    )
}
