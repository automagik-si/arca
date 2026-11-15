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

/// What an emptying refuses when an entry `/run` lists cannot be read back by its name (Incus lists a name that is not
/// UTF-8 with U+FFFD in its place, and a read of that name answers 404): its owner is not known, and it would stay on
/// the disk, where Incus mounts no memory `/run`.
pub const UNREADABLE: &str = "an entry /run lists cannot be read by its name, so its owner is not known: /run is left on the disk";

/// What an emptying refuses when an entry under `/run` is not root's: a non-root image made it at its build (a socket
/// directory of its user, say), and its user cannot make it again under a memory `/run`, which is root's with mode 0755.
pub const NOT_ROOTS: &str =
    "an entry under /run is not root's, so its image needs it there: /run is left on the disk";

/// Empties `/run` on the root disk of the container `name` in `project`, which must be stopped (the caller's check): every
/// entry under it is removed, a directory after what it holds, and `/run` itself stays. Returns how many entries were
/// removed. A container with no `/run` has nothing to remove.
///
/// The whole of `/run` is read before anything is removed, and nothing is removed when an entry there is not owned by
/// the container's root ([`NOT_ROOTS`]): an image that runs as another user and keeps its own directory in `/run` needs
/// it, and nothing when an entry it lists cannot be read by its name ([`UNREADABLE`]). A `/run` that is not a directory,
/// more than [`RUN_ENTRIES_MAX`] entries or [`RUN_DEPTH_MAX`] levels, a name that
/// cannot be one entry's, and any refused request are errors too. Only a removal can fail once removals began, and what
/// was removed by then stays removed.
pub(crate) fn empty_run(send: &Send<'_>, project: &str, name: &str) -> Result<usize, IncusError> {
    if !plain_name(project) || !plain_name(name) {
        return Err(refused("the project or instance name is not plain"));
    }
    let names = match look(send, project, name, "/run")?.0 {
        Found::Missing => return Ok(0),
        Found::Directory(names) => names,
        Found::Other => return Err(refused("/run is not a directory")),
    };
    let mut order = Vec::new();
    for entry in names {
        scan(send, project, name, &format!("/run/{entry}"), 0, &mut order)?;
    }
    for path in &order {
        delete(send, project, name, path)?;
    }
    Ok(order.len())
}

/// Reads `path` and, for a directory, everything in it, and adds each to `order` after what it holds, so removing in
/// that order removes a directory once it is empty. Refuses an entry that is not root's, or whose owner is not said, and
/// one its parent lists that cannot be read ([`UNREADABLE`]). A link is one entry: its target is never read.
fn scan(
    send: &Send<'_>,
    project: &str,
    name: &str,
    path: &str,
    depth: usize,
    order: &mut Vec<String>,
) -> Result<(), IncusError> {
    if order.len() >= RUN_ENTRIES_MAX {
        return Err(refused("/run holds more entries than Arca removes"));
    }
    let (found, uid) = look(send, project, name, path)?;
    if matches!(found, Found::Missing) {
        return Err(refused(UNREADABLE));
    }
    if uid != Some(0) {
        return Err(refused(NOT_ROOTS));
    }
    if let Found::Directory(names) = found {
        if depth >= RUN_DEPTH_MAX {
            return Err(refused("/run is nested deeper than Arca walks"));
        }
        for entry in names {
            scan(
                send,
                project,
                name,
                &format!("{path}/{entry}"),
                depth + 1,
                order,
            )?;
        }
    }
    order.push(path.to_string());
    Ok(())
}

fn delete(send: &Send<'_>, project: &str, name: &str, path: &str) -> Result<(), IncusError> {
    checked_envelope(send("DELETE", &files_url(project, name, path))?).map(|_| ())
}

/// What is at `path`, and its owner as the container sees it (Incus says it in a header, `0` for the container's root).
/// Incus answers a directory with the list of its names and says the type in a header; a file or a link it answers with
/// its bytes or its target, which are not read.
fn look(
    send: &Send<'_>,
    project: &str,
    name: &str,
    path: &str,
) -> Result<(Found, Option<u32>), IncusError> {
    let framed = send("GET", &files_url(project, name, path))?;
    if framed.status == 404 {
        return Ok((Found::Missing, None));
    }
    let uid = framed
        .header("x-incus-uid")
        .and_then(|uid| uid.trim().parse::<u32>().ok());
    let directory = framed.header("x-incus-type") == Some("directory");
    if !directory {
        if !(200..300).contains(&framed.status) {
            checked_envelope(framed)?;
            return Err(refused("Incus refused a read under /run"));
        }
        return Ok((Found::Other, uid));
    }
    let envelope = checked_envelope(framed)?;
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
    Ok((Found::Directory(names), uid))
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
    Ok(match look(send, project, name, "/run")?.0 {
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

    use std::cell::RefCell;

    /// One path of a test root: its names when it is a directory (`None`: a file), and its owner's uid.
    type Entry<'a> = (&'a str, Option<&'a [&'a str]>, Option<u32>);
    /// The same, owned.
    type Kept = (Option<Vec<String>>, Option<u32>);

    /// A container root as the files API answers it: each path is a directory (its names), a file, or not there, with
    /// its owner's uid (`None`: no `X-Incus-uid` header). The requests sent are kept as "METHOD path".
    struct Root {
        entries: BTreeMap<String, Kept>,
        sent: RefCell<Vec<String>>,
    }

    impl Root {
        fn new(entries: &[Entry<'_>]) -> Self {
            Self {
                entries: entries
                    .iter()
                    .map(|(path, names, uid)| {
                        (
                            path.to_string(),
                            (
                                names.map(|names| names.iter().map(|n| n.to_string()).collect()),
                                *uid,
                            ),
                        )
                    })
                    .collect(),
                sent: RefCell::new(Vec::new()),
            }
        }

        fn send(&self, method: &str, target: &str) -> Result<Framed, IncusError> {
            let path = target.split_once("&path=").expect("a path").1.to_string();
            self.sent.borrow_mut().push(format!("{method} {path}"));
            if method == "DELETE" {
                return Ok(Framed::for_test(
                    200,
                    &[],
                    br#"{"type":"sync","status":"Success","status_code":200,"metadata":{}}"#,
                ));
            }
            let Some((names, uid)) = self.entries.get(&path) else {
                return Ok(Framed::for_test(
                    404,
                    &[],
                    br#"{"type":"error","error":"not found","error_code":404}"#,
                ));
            };
            let uid = uid.map(|uid| uid.to_string());
            let mut headers = vec![];
            if let Some(uid) = &uid {
                headers.push(("x-incus-uid", uid.as_str()));
            }
            match names {
                Some(names) => {
                    headers.push(("x-incus-type", "directory"));
                    let body = serde_json::json!({
                        "type": "sync", "status": "Success", "status_code": 200, "metadata": names,
                    });
                    Ok(Framed::for_test(200, &headers, body.to_string().as_bytes()))
                }
                None => {
                    headers.push(("x-incus-type", "file"));
                    Ok(Framed::for_test(200, &headers, b"bytes"))
                }
            }
        }

        fn deletes(&self) -> Vec<String> {
            self.sent
                .borrow()
                .iter()
                .filter(|line| line.starts_with("DELETE "))
                .cloned()
                .collect()
        }
    }

    fn empty(root: &Root) -> Result<usize, IncusError> {
        empty_run(&|method, target| root.send(method, target), "p", "c")
    }

    fn refusal(result: Result<usize, IncusError>) -> String {
        match result {
            Err(error) => error.to_string(),
            Ok(removed) => panic!("refused, not {removed} removed"),
        }
    }

    #[test]
    fn roots_entries_are_removed_deepest_first() {
        let root = Root::new(&[
            ("/run", Some(&["lock", "motd"]), Some(0)),
            ("/run/lock", Some(&[".keep"]), Some(0)),
            ("/run/lock/.keep", None, Some(0)),
            ("/run/motd", None, Some(0)),
        ]);
        assert_eq!(empty(&root).expect("emptied"), 3);
        assert_eq!(
            root.deletes(),
            [
                "DELETE /run/lock/.keep",
                "DELETE /run/lock",
                "DELETE /run/motd"
            ]
        );
    }

    #[test]
    fn an_entry_whose_owner_is_not_said_is_refused_and_nothing_is_removed() {
        let root = Root::new(&[
            ("/run", Some(&["lock", "app"]), Some(0)),
            ("/run/lock", Some(&[]), Some(0)),
            ("/run/app", None, None),
        ]);
        let why = refusal(empty(&root));
        assert!(why.contains(NOT_ROOTS), "{why}");
        assert!(root.deletes().is_empty(), "{:?}", root.sent.borrow());
    }

    #[test]
    fn a_listed_entry_that_cannot_be_read_is_refused_and_nothing_is_removed() {
        // Incus lists a name that is not UTF-8 with U+FFFD in its place, and a read of that name answers 404.
        let root = Root::new(&[
            ("/run", Some(&["lock", "app\u{fffd}"]), Some(0)),
            ("/run/lock", Some(&[]), Some(0)),
        ]);
        let why = refusal(empty(&root));
        assert!(why.contains(UNREADABLE), "{why}");
        assert!(root.deletes().is_empty(), "{:?}", root.sent.borrow());
        // A container with no /run at all has nothing to remove.
        assert_eq!(empty(&Root::new(&[])).expect("no /run"), 0);
    }

    #[test]
    fn a_run_nested_past_the_depth_cap_is_refused_and_nothing_is_removed() {
        let mut entries: Vec<(String, Option<Vec<String>>)> = Vec::new();
        let mut path = "/run".to_string();
        for _ in 0..=RUN_DEPTH_MAX + 1 {
            entries.push((path.clone(), Some(vec!["d".to_string()])));
            path.push_str("/d");
        }
        entries.push((path, None));
        type Named<'a> = (&'a str, Option<Vec<&'a str>>, Option<u32>);
        let owned: Vec<Named<'_>> = entries
            .iter()
            .map(|(path, names)| {
                (
                    path.as_str(),
                    names
                        .as_ref()
                        .map(|names| names.iter().map(String::as_str).collect()),
                    Some(0),
                )
            })
            .collect();
        let refs: Vec<Entry<'_>> = owned
            .iter()
            .map(|(path, names, uid)| (*path, names.as_deref(), *uid))
            .collect();
        let root = Root::new(&refs);
        let why = refusal(empty(&root));
        assert!(why.contains("deeper than Arca walks"), "{why}");
        assert!(root.deletes().is_empty());

        // One level less is walked whole and emptied.
        let shallow: Vec<Entry<'_>> = refs
            .iter()
            .filter(|(path, _, _)| path.matches("/d").count() <= RUN_DEPTH_MAX)
            .map(|(path, names, uid)| {
                if path.matches("/d").count() == RUN_DEPTH_MAX {
                    (*path, None, *uid)
                } else {
                    (*path, *names, *uid)
                }
            })
            .collect();
        let root = Root::new(&shallow);
        assert_eq!(empty(&root).expect("emptied"), RUN_DEPTH_MAX);
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
