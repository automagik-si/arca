//! A container service's `/run` in memory.
//!
//! Incus mounts a memory file system (tmpfs, mode 0755) on an OCI container's `/run` at its start when nothing else is
//! mounted there and the container's root holds nothing under `/run` (Incus 7.5, `driver_lxc.go`). An image that ships
//! files there (a Debian base keeps `/run/lock`) gets no such mount, and `/run` is then a directory on the container's
//! disk: a short-lived secret staged there touches the disk. Arca empties `/run` on the root disk of a stopped container
//! whose service keeps `/run` in memory, so every later start mounts the memory one. Nothing else of the root is touched,
//! and Incus's own rule decides the mount: no project restriction is loosened for it.
//!
//! The helpers use only the instance files API of one instance: a listing or a removal under `/run`. A running
//! container's mount table cannot be read through that API (its `/proc` files read as empty), so where a running
//! container's `/run` is comes from what this process saw when it emptied that container: [`note_emptied`] keeps the
//! outcome of each emptying, by socket, instance and uuid, and [`emptied`] reads it back.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::Value;

use crate::lifecycle::{checked_envelope, plain_name, refused};
use crate::read::{Framed, IncusError};

/// The most entries one emptying removes: an image's `/run` holds a handful.
pub const RUN_ENTRIES_MAX: usize = 256;
/// The deepest directory under `/run` one emptying walks into.
const RUN_DEPTH_MAX: usize = 16;

/// Sends one request: the method and the origin-form target.
pub(crate) type Send<'a> = dyn Fn(&str, &str) -> Result<Framed, IncusError> + 'a;

/// What is at a path on the container's root.
enum Found {
    Missing,
    Directory(Vec<String>),
    Other,
}

/// Empties `/run` on the root disk of the container `name` in `project`, which must be stopped (the caller's check): every
/// entry under it is removed, a directory after what it holds, and `/run` itself stays. Returns how many entries were
/// removed. A container with no `/run` has nothing to remove. A `/run` that is not a directory, more than
/// [`RUN_ENTRIES_MAX`] entries, a name that cannot be one entry's, and any refused request are errors, and what was
/// removed by then stays removed.
pub(crate) fn empty_run(send: &Send<'_>, project: &str, name: &str) -> Result<usize, IncusError> {
    if !plain_name(project) || !plain_name(name) {
        return Err(refused("the project or instance name is not plain"));
    }
    let names = match look(send, project, name, "/run")? {
        Found::Missing => return Ok(0),
        Found::Directory(names) => names,
        Found::Other => return Err(refused("/run is not a directory")),
    };
    let mut removed = 0;
    for entry in names {
        remove(
            send,
            project,
            name,
            &format!("/run/{entry}"),
            0,
            &mut removed,
        )?;
    }
    Ok(removed)
}

/// Removes `path` and, for a directory, everything in it first. Incus removes a file, a link or an empty directory in
/// one request and refuses a directory that holds something, which is then listed and emptied.
fn remove(
    send: &Send<'_>,
    project: &str,
    name: &str,
    path: &str,
    depth: usize,
    removed: &mut usize,
) -> Result<(), IncusError> {
    if *removed >= RUN_ENTRIES_MAX {
        return Err(refused("/run holds more entries than Arca removes"));
    }
    let first = match delete(send, project, name, path) {
        Ok(()) => {
            *removed += 1;
            return Ok(());
        }
        Err(error @ IncusError::Status { .. }) => error,
        Err(error) => return Err(error),
    };
    let Found::Directory(names) = look(send, project, name, path)? else {
        return Err(first);
    };
    if depth >= RUN_DEPTH_MAX {
        return Err(refused("/run is nested deeper than Arca walks"));
    }
    for entry in names {
        remove(
            send,
            project,
            name,
            &format!("{path}/{entry}"),
            depth + 1,
            removed,
        )?;
    }
    delete(send, project, name, path)?;
    *removed += 1;
    Ok(())
}

fn delete(send: &Send<'_>, project: &str, name: &str, path: &str) -> Result<(), IncusError> {
    checked_envelope(send("DELETE", &files_url(project, name, path))?).map(|_| ())
}

/// What is at `path`: Incus answers a directory with the list of its names and says its type in a header.
fn look(send: &Send<'_>, project: &str, name: &str, path: &str) -> Result<Found, IncusError> {
    let framed = send("GET", &files_url(project, name, path))?;
    if framed.status == 404 {
        return Ok(Found::Missing);
    }
    let directory = framed.header("x-incus-type") == Some("directory");
    let envelope = checked_envelope(framed)?;
    if !directory {
        return Ok(Found::Other);
    }
    let names = envelope["metadata"]
        .as_array()
        .ok_or_else(|| IncusError::Decode("a directory listing is not a list".into()))?
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .filter(|entry| entry_name_ok(entry))
                .map(str::to_string)
                .ok_or_else(|| refused("a name under /run is not one entry's"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Found::Directory(names))
}

/// Whether `entry` names one entry of a directory: not empty, not `.` or `..`, no `/`, no control character, and short.
fn entry_name_ok(entry: &str) -> bool {
    !entry.is_empty()
        && entry.len() <= 255
        && entry != "."
        && entry != ".."
        && !entry.contains('/')
        && !entry.chars().any(char::is_control)
}

/// The files API target of `path` in the instance: the path percent-encoded, so a name of any bytes stays one query
/// value.
fn files_url(project: &str, name: &str, path: &str) -> String {
    let mut encoded = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    format!("/1.0/instances/{name}/files?project={project}&path={encoded}")
}

/// Whether `/run` on the root disk of the stopped container `name` in `project` holds nothing, or is not there: its next
/// start then mounts a memory `/run`.
pub(crate) fn run_is_empty(send: &Send<'_>, project: &str, name: &str) -> Result<bool, IncusError> {
    if !plain_name(project) || !plain_name(name) {
        return Err(refused("the project or instance name is not plain"));
    }
    Ok(match look(send, project, name, "/run")? {
        Found::Missing => true,
        Found::Directory(names) => names.is_empty(),
        Found::Other => false,
    })
}

/// One emptied container: the Incus socket, the project, the instance and its uuid.
type Emptied = (PathBuf, String, String, String);

/// The outcome of the last emptying of each container this process emptied: `true` when `/run` held nothing after it.
/// A recreated container has another uuid, so an outcome never outlives its container. Bounded: past the cap the oldest
/// half of the keys is dropped, and a container whose outcome is gone reads as not known.
static EMPTIED: Mutex<BTreeMap<Emptied, (u64, bool)>> = Mutex::new(BTreeMap::new());
/// The most outcomes kept.
const EMPTIED_MAX: usize = 4096;

/// Keeps the outcome of an emptying of the container `name` (uuid `uuid`) in `project` on `socket`.
pub(crate) fn note_emptied(socket: &Path, project: &str, name: &str, uuid: &str, emptied: bool) {
    let Ok(mut kept) = EMPTIED.lock() else {
        return;
    };
    let next = kept.values().map(|(order, _)| order + 1).max().unwrap_or(0);
    kept.insert(
        (
            socket.to_path_buf(),
            project.to_string(),
            name.to_string(),
            uuid.to_string(),
        ),
        (next, emptied),
    );
    if kept.len() > EMPTIED_MAX {
        let mut orders: Vec<u64> = kept.values().map(|(order, _)| *order).collect();
        orders.sort_unstable();
        let cut = orders[orders.len() / 2];
        kept.retain(|_, (order, _)| *order >= cut);
    }
}

/// The outcome of the last emptying this process made of the container `name` (uuid `uuid`) in `project` on `socket`:
/// `Some(true)` when its `/run` was left empty, so every start since mounted a memory one, `Some(false)` when it was
/// not, `None` when this process has not emptied it.
pub(crate) fn emptied(socket: &Path, project: &str, name: &str, uuid: &str) -> Option<bool> {
    let kept = EMPTIED.lock().ok()?;
    kept.get(&(
        socket.to_path_buf(),
        project.to_string(),
        name.to_string(),
        uuid.to_string(),
    ))
    .map(|(_, emptied)| *emptied)
}

/// Whether an instance body asks for a memory `/run`: a container whose config carries the mark.
pub(crate) fn body_wants_memory_run(body: &serde_json::Map<String, Value>) -> bool {
    body.get("type")
        .and_then(Value::as_str)
        .is_none_or(|kind| kind == "container")
        && body
            .get("config")
            .and_then(|config| config.get(arca_kernel::ownership::RUN_TMPFS_KEY))
            .and_then(Value::as_str)
            == Some("true")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_outcome_is_kept_per_container_and_uuid() {
        let socket = Path::new("/nonexistent/run-dir-test.socket");
        assert_eq!(emptied(socket, "p", "c", "u1"), None);
        note_emptied(socket, "p", "c", "u1", true);
        assert_eq!(emptied(socket, "p", "c", "u1"), Some(true));
        assert_eq!(
            emptied(socket, "p", "c", "u2"),
            None,
            "a recreated container"
        );
        note_emptied(socket, "p", "c", "u1", false);
        assert_eq!(
            emptied(socket, "p", "c", "u1"),
            Some(false),
            "the last outcome"
        );
    }

    #[test]
    fn a_path_is_one_query_value() {
        assert_eq!(
            files_url("p", "c", "/run/a b&c"),
            "/1.0/instances/c/files?project=p&path=/run/a%20b%26c"
        );
    }

    #[test]
    fn only_one_entry_name_is_taken() {
        for bad in ["", ".", "..", "a/b", "a\nb"] {
            assert!(!entry_name_ok(bad), "{bad:?}");
        }
        assert!(entry_name_ok("lock"));
        assert!(entry_name_ok(".keep"));
    }
}
