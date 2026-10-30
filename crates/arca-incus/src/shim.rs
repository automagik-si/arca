//! The recording shim: a unix-socket proxy between a client and Incus. It forwards a GET and refuses everything else.
//!
//! A request is forwarded only when its method is exactly `GET`, its head declares no body (no `Transfer-Encoding`, and
//! no `Content-Length` other than `0`), and it asks for no protocol upgrade (no `Upgrade` header). Every header name must
//! be an HTTP token. Only the head is forwarded, so bytes after it never reach Incus. Every other request, a malformed one
//! included, is answered `405` or `400` and not forwarded. The log holds one JSON line per request with `method`, `path`,
//! `status` and `forwarded`. The path is the target without its query string. The log never holds a header or a body byte.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde::Serialize;

/// The deadline for each read and write on either side of the shim, and on the fake.
pub(crate) const IO_TIMEOUT: Duration = Duration::from_secs(10);
/// The most bytes a request head may take before the shim refuses it.
const MAX_HEAD: usize = 16 * 1024;
/// The longest path written to the log, in characters.
const MAX_LOGGED: usize = 512;
/// How long the shim waits for a refused client to finish sending its body.
const DRAIN_IDLE: Duration = Duration::from_millis(200);
/// The most body bytes read and discarded from a refused client.
const DRAIN_CAP: usize = 1024 * 1024;

/// What the shim did. `get` counts GET requests whose head reached the upstream. `refused` counts requests answered without
/// forwarding: a non-GET (405), a malformed or body-carrying request (400), and a GET the upstream could not take (502).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ShimSummary {
    pub get: u64,
    pub refused: u64,
}

/// A recording shim. [`RecordingShim::start`] binds it and returns the handle that stops it.
pub struct RecordingShim;

/// A running shim. Stop it with [`ShimHandle::stop`], which joins every connection and returns the summary.
pub struct ShimHandle {
    socket: PathBuf,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}

struct Shared {
    upstream: PathBuf,
    log: Mutex<File>,
    log_failed: AtomicBool,
    get: AtomicU64,
    refused: AtomicU64,
}

impl RecordingShim {
    /// Binds the shim at `listen`, forwarding to `upstream` and appending to `log`. The log is opened in append mode.
    pub fn start(upstream: &Path, listen: &Path, log: &Path) -> io::Result<ShimHandle> {
        let log_file = OpenOptions::new().create(true).append(true).open(log)?;
        let listener = UnixListener::bind(listen)?;
        listener.set_nonblocking(true)?;
        let shared = Arc::new(Shared {
            upstream: upstream.to_path_buf(),
            log: Mutex::new(log_file),
            log_failed: AtomicBool::new(false),
            get: AtomicU64::new(0),
            refused: AtomicU64::new(0),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let accept = {
            let shared = Arc::clone(&shared);
            let stop = Arc::clone(&stop);
            thread::spawn(move || accept_loop(&listener, &shared, &stop))
        };
        Ok(ShimHandle {
            socket: listen.to_path_buf(),
            shared,
            stop,
            accept: Some(accept),
        })
    }
}

impl ShimHandle {
    /// The socket the shim listens on.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Stops accepting, waits for every connection to finish, removes the socket file and returns the counts. It fails if
    /// the log could not be written, so a missing record is never a pass.
    pub fn stop(mut self) -> io::Result<ShimSummary> {
        self.halt();
        let _ = std::fs::remove_file(&self.socket);
        if self.shared.log_failed.load(Ordering::SeqCst) {
            return Err(io::Error::other("the shim could not write its log"));
        }
        Ok(ShimSummary {
            get: self.shared.get.load(Ordering::SeqCst),
            refused: self.shared.refused.load(Ordering::SeqCst),
        })
    }

    fn halt(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
    }
}

impl Drop for ShimHandle {
    fn drop(&mut self) {
        self.halt();
    }
}

fn accept_loop(listener: &UnixListener, shared: &Arc<Shared>, stop: &AtomicBool) {
    let mut connections: Vec<JoinHandle<()>> = Vec::new();
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                connections.retain(|connection| !connection.is_finished());
                let shared = Arc::clone(shared);
                connections.push(thread::spawn(move || handle(stream, &shared)));
            }
            Err(_) => thread::sleep(Duration::from_millis(5)),
        }
    }
    for connection in connections {
        let _ = connection.join();
    }
}

/// One client connection: read the head, decide, answer.
fn handle(mut client: UnixStream, shared: &Shared) {
    if client.set_nonblocking(false).is_err()
        || client.set_read_timeout(Some(IO_TIMEOUT)).is_err()
        || client.set_write_timeout(Some(IO_TIMEOUT)).is_err()
    {
        return;
    }
    let head = match read_head(&mut client) {
        Ok(Some(head)) => head,
        Ok(None) => return,
        Err(_) => {
            refuse(&mut client, shared, "", "", 400);
            return;
        }
    };
    let Some(request) = parse_request(&head) else {
        refuse(&mut client, shared, "", "", 400);
        return;
    };
    if request.method != "GET" {
        refuse(&mut client, shared, request.method, request.target, 405);
        return;
    }
    if request.has_body() || request.upgrades() {
        refuse(&mut client, shared, request.method, request.target, 400);
        return;
    }
    forward(client, shared, &head, request.target);
}

/// A parsed request head. The method and target are borrowed from the head bytes.
struct Request<'a> {
    method: &'a str,
    target: &'a str,
    headers: Vec<(&'a str, &'a str)>,
}

impl Request<'_> {
    /// True when the head declares any body: a transfer coding, or a content length other than zero.
    fn has_body(&self) -> bool {
        self.headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("transfer-encoding")
                || (name.eq_ignore_ascii_case("content-length") && *value != "0")
        })
    }

    /// True when the head asks to switch protocols: it carries an `Upgrade` header, whatever its value.
    fn upgrades(&self) -> bool {
        self.headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("upgrade"))
    }
}

/// True for a non-empty HTTP token: the characters RFC 9110 allows in a header name, which are ASCII letters, digits and a
/// fixed set of punctuation marks.
fn is_token(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

fn parse_request(head: &[u8]) -> Option<Request<'_>> {
    let text = std::str::from_utf8(head).ok()?;
    let mut lines = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line));
    let (method, target) = parse_request_line(lines.next()?)?;
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':')?;
        if !is_token(name) {
            return None;
        }
        headers.push((name, value.trim()));
    }
    Some(Request {
        method,
        target,
        headers,
    })
}

/// The method and target of a request line, which must be three single-space-separated parts ending in an HTTP/1 version.
pub(crate) fn parse_request_line(line: &str) -> Option<(&str, &str)> {
    let mut parts = line.split(' ');
    let method = parts.next()?;
    let target = parts.next()?;
    let version = parts.next()?;
    if parts.next().is_some()
        || method.is_empty()
        || target.is_empty()
        || !version.starts_with("HTTP/1.")
    {
        return None;
    }
    Some((method, target))
}

/// Reads one request head, through its blank line, and discards any bytes after it. `None` means the client sent nothing
/// before it closed or went quiet. A head over the cap, or one cut short, is an error.
pub(crate) fn read_head<R: Read>(stream: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut head = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(end) = head_end(&head) {
            head.truncate(end);
            return Ok(Some(head));
        }
        if head.len() >= MAX_HEAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the request head is too large",
            ));
        }
        match stream.read(&mut chunk) {
            Ok(0) if head.is_empty() => return Ok(None),
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the request ended before its head",
                ));
            }
            Ok(n) => head.extend_from_slice(&chunk[..n]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) && head.is_empty() =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        }
    }
}

/// The length of the head that `bytes` starts with, through the blank line. Both `\r\n` and bare `\n` endings are read.
pub(crate) fn head_end(bytes: &[u8]) -> Option<usize> {
    for (index, &byte) in bytes.iter().enumerate() {
        if byte != b'\n' {
            continue;
        }
        match &bytes[index + 1..] {
            [b'\n', ..] => return Some(index + 2),
            [b'\r', b'\n', ..] => return Some(index + 3),
            _ => {}
        }
    }
    None
}

/// Answers a request with an empty body and the given status line, then reads and discards the client's remaining body.
pub(crate) fn respond<W: Write>(
    stream: &mut W,
    status: u16,
    reason: &str,
    extra: &str,
    body: &[u8],
) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// Closes the write side so the client sees the end of the answer, then discards what the client still sends, briefly.
pub(crate) fn drain(stream: &mut UnixStream) {
    let _ = stream.shutdown(std::net::Shutdown::Write);
    if stream.set_read_timeout(Some(DRAIN_IDLE)).is_err() {
        return;
    }
    let mut sink = [0u8; 4096];
    let mut total = 0usize;
    while total < DRAIN_CAP {
        match stream.read(&mut sink) {
            Ok(0) | Err(_) => break,
            Ok(n) => total += n,
        }
    }
}

/// A refusal: count it, log it, then answer. The log line is written before the answer, so a client that has its answer
/// can rely on the record being there.
fn refuse(client: &mut UnixStream, shared: &Shared, method: &str, target: &str, status: u16) {
    shared.refused.fetch_add(1, Ordering::SeqCst);
    log(shared, method, target, status, false);
    let (reason, extra) = match status {
        405 => ("Method Not Allowed", "Allow: GET\r\n"),
        502 => ("Bad Gateway", ""),
        _ => ("Bad Request", ""),
    };
    let _ = respond(client, status, reason, extra, b"");
    drain(client);
}

/// A forwarded GET: the head goes upstream verbatim, and the upstream's answer streams back. The log line is written after
/// the answer ends, with the status the answer carried.
fn forward(mut client: UnixStream, shared: &Shared, head: &[u8], target: &str) {
    let Ok(mut upstream) = UnixStream::connect(&shared.upstream) else {
        refuse(&mut client, shared, "GET", target, 502);
        return;
    };
    if upstream.set_read_timeout(Some(IO_TIMEOUT)).is_err()
        || upstream.set_write_timeout(Some(IO_TIMEOUT)).is_err()
        || upstream.write_all(head).is_err()
    {
        refuse(&mut client, shared, "GET", target, 502);
        return;
    }
    shared.get.fetch_add(1, Ordering::SeqCst);

    let mut status: Option<u16> = None;
    let mut opening: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let read = match upstream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        if status.is_none() {
            opening.extend_from_slice(&buf[..read]);
            status = upstream_status(&opening);
        }
        if client.write_all(&buf[..read]).is_err() {
            break;
        }
    }
    log(shared, "GET", target, status.unwrap_or(0), true);
}

/// The status code of an answer's status line, once that line is complete or long enough to be wrong. `None` while the
/// line is still arriving.
fn upstream_status(opening: &[u8]) -> Option<u16> {
    let line_end = opening.iter().position(|byte| *byte == b'\n');
    if line_end.is_none() && opening.len() < 64 {
        return None;
    }
    let line = &opening[..line_end.unwrap_or(opening.len())];
    let code = std::str::from_utf8(line)
        .ok()
        .and_then(|text| text.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    Some(code)
}

#[derive(Serialize)]
struct LogLine<'a> {
    method: &'a str,
    path: &'a str,
    status: u16,
    forwarded: bool,
}

fn log(shared: &Shared, method: &str, target: &str, status: u16, forwarded: bool) {
    // The query string is dropped: it can carry credentials or other values a log must not keep.
    let without_query = target.split(['?', '#']).next().unwrap_or("");
    let path: String = without_query.chars().take(MAX_LOGGED).collect();
    let line = LogLine {
        method: logged_method(method),
        path: &path,
        status,
        forwarded,
    };
    let mut bytes = match serde_json::to_vec(&line) {
        Ok(bytes) => bytes,
        Err(_) => {
            shared.log_failed.store(true, Ordering::SeqCst);
            return;
        }
    };
    bytes.push(b'\n');
    let mut file = shared.log.lock().unwrap_or_else(PoisonError::into_inner);
    if file.write_all(&bytes).is_err() {
        shared.log_failed.store(true, Ordering::SeqCst);
    }
}

/// A method token as the log may hold it: short ASCII letters. Anything else, and no method at all, is named plainly.
fn logged_method(method: &str) -> &str {
    if method.is_empty() {
        ""
    } else if (1..=16).contains(&method.len()) && method.bytes().all(|b| b.is_ascii_alphabetic()) {
        method
    } else {
        "invalid"
    }
}
