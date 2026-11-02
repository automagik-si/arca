//! The four health components: `incus`, `arca`, `watch` and `shim`. The sampler thread runs them on each tick, so each check
//! is bounded by [`CHECK_TIMEOUT`]. A check sends at most one request, and the only non-GET request is the tokenless
//! `POST /mcp` to arca's own socket, which the server answers with 401 before it reads the body.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use arca_incus::{IncusRead, UnixIncus};

use crate::eval::{Component, ComponentName, Timed, WATCH_MAX_AGE_MS};

/// The most time one health check may take, from connect to its answer.
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(2);

/// The sockets the checks reach. `shim` exists only when `alerts.json` names a shim socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckTargets {
    pub incus: PathBuf,
    pub arca: PathBuf,
    pub shim: Option<PathBuf>,
}

/// The components at `now_ms`, in the order of [`ComponentName`]. `watch_end_ms` is the end of the newest successful watch
/// sweep, or `None` when no sweep has succeeded. `watch` is ok while that end is no older than [`WATCH_MAX_AGE_MS`], the same
/// boundary the evaluator uses for the drift reading. `shim` is listed only when `t.shim` is set.
pub fn components(
    t: &CheckTargets,
    watch_end_ms: Option<u64>,
    now_ms: u64,
) -> Timed<Vec<Component>> {
    let mut components = vec![
        Component {
            name: ComponentName::Incus,
            ok: get_ok(&t.incus),
        },
        Component {
            name: ComponentName::Arca,
            ok: arca_ok(&t.arca),
        },
        Component {
            name: ComponentName::Watch,
            ok: watch_end_ms.is_some_and(|end| now_ms.saturating_sub(end) <= WATCH_MAX_AGE_MS),
        },
    ];
    if let Some(shim) = &t.shim {
        components.push(Component {
            name: ComponentName::Shim,
            ok: get_ok(shim),
        });
    }
    Timed {
        at: now_ms,
        value: components,
    }
}

/// Whether `GET /1.0` on `socket` answers 200 within the check timeout. An absent socket, a refused connection and a hang are
/// all not ok.
fn get_ok(socket: &Path) -> bool {
    UnixIncus::new(socket.to_path_buf())
        .with_timeout(CHECK_TIMEOUT)
        .get("/1.0")
        .is_ok()
}

/// Whether a tokenless `POST /mcp` to arca's socket answers 401 within the check timeout.
fn arca_ok(socket: &Path) -> bool {
    post_status(socket, "/mcp", CHECK_TIMEOUT) == Some(401)
}

/// The status code of a body-less `POST` to `path` on `socket`, read from the status line. The whole exchange shares one
/// deadline of `timeout`, and a connection that answers no status line by then gives `None`.
fn post_status(socket: &Path, path: &str, timeout: Duration) -> Option<u16> {
    let deadline = Instant::now().checked_add(timeout)?;
    let mut stream = UnixStream::connect(socket).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: arca\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).ok()?;
    let mut head = Vec::with_capacity(64);
    let mut chunk = [0u8; 64];
    while !head.windows(2).any(|pair| pair == b"\r\n") && head.len() < 512 {
        let left = deadline.checked_duration_since(Instant::now())?;
        if left.is_zero() {
            return None;
        }
        stream.set_read_timeout(Some(left)).ok()?;
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => head.extend_from_slice(&chunk[..read]),
            Err(_) => return None,
        }
    }
    status_of(&head)
}

/// The status code of an HTTP/1.x status line at the start of `head`.
fn status_of(head: &[u8]) -> Option<u16> {
    let line = head.split(|byte| *byte == b'\r' || *byte == b'\n').next()?;
    let text = std::str::from_utf8(line).ok()?;
    let mut parts = text.split_whitespace();
    let version = parts.next()?;
    if !version.starts_with("HTTP/1.") {
        return None;
    }
    parts.next()?.parse().ok()
}
