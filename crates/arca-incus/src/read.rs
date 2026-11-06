//! The GET-only Incus client.
//!
//! A request is written with one constant method, `GET`, and [`IncusRead`] has no other method, so this module cannot send
//! anything else. The response reader accepts a `Content-Length` body, a chunked body and a body that ends with the
//! connection. A body over [`MAX_BODY`] is refused as [`IncusError::TooLarge`], and an answer that does not finish by the
//! request's deadline is refused as [`IncusError::Timeout`]. No error carries a header byte or a raw body byte. A status
//! error keeps only the envelope's `error` text, or the reason phrase, cut to 512 bytes.
//!
//! Every request in this crate, the writer's included, is sent through one function, `exchange`, which refuses a PATCH to a
//! project before any byte is written (see `guard_request`).
//!
//! The operations reads ([`running_operations`], [`running_on`] and [`wait_operation`]) tell whether Incus is still working on
//! an object, so a caller does not judge the object from a read taken while an operation can still change it.

use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use arca_kernel::ownership::{IncusKind, ObjectRef, OwnerClass};
use serde_json::Value;

/// The largest response body the client accepts, in bytes.
pub const MAX_BODY: usize = 16 * 1024 * 1024;
/// The deadline for one request, from connect to the last body byte.
pub const TIMEOUT: Duration = Duration::from_secs(10);
/// The most bytes the status line and headers may take.
const MAX_HEAD: usize = 64 * 1024;
/// The longest request path the client sends.
const MAX_PATH: usize = 2048;
/// The longest message a status error keeps, in bytes. A longer one is cut at a character boundary.
const MAX_MESSAGE: usize = 512;

/// Why a read failed. No variant carries a header byte or a raw body byte. A `Status` message is the envelope's own
/// `error` text, or the reason phrase, and is at most 512 bytes.
///
/// Only [`IncusError::Refused`] means that no write was sent. Every other variant may follow a write that landed: an
/// accepted write whose operation fails, times out or answers unreadably, or whose read-back fails or does not confirm it.
/// A caller that gets any of them after a write must read the object before it decides anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IncusError {
    /// The socket could not be connected, written or read.
    Io(String),
    /// Incus answered with a status that is not success, or with an error envelope. The message is at most 512 bytes.
    Status { code: u16, message: String },
    /// The answer is not the HTTP or JSON shape the client reads.
    Decode(String),
    /// The answer did not finish by the deadline.
    Timeout,
    /// The answer is over [`MAX_BODY`].
    TooLarge,
    /// The writer refused before it sent any write, because the object is not the caller's to change, the target is outside
    /// the kernel's project family, or the request is not a plain one; or the guard every request passes refused a PATCH to a
    /// project. A read may have been made. The text names the reason and never a value of the object.
    Refused(String),
    /// The write was accepted and applied, but reading the object back did not confirm it. Boxed, so the error stays small.
    AppliedUnconfirmed(Box<Unconfirmed>),
}

/// The object a write names: its kind, project, parent and name, and the uuid only for a kind that has one. A create names no
/// uuid, since it learns one only as it applies. The writer checks every name before a request uses it.
pub type Target = ObjectRef;

/// A write that Incus accepted whose read-back did not confirm it: the object is not this entry's own, or it does not show the
/// change. `target` is the full target the write named, pool or network included. `uuid` is the uuid the read-back showed, when
/// it showed one, so a caller can tell what landed without a second read. `owner` is the class the read found, when it found
/// one. Nothing is retried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unconfirmed {
    pub target: Target,
    pub uuid: Option<String>,
    pub owner: Option<OwnerClass>,
    pub reason: String,
}

impl fmt::Display for IncusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(message) => write!(f, "socket error: {message}"),
            Self::Status { code, message } => write!(f, "Incus answered {code}: {message}"),
            Self::Decode(message) => write!(f, "could not read the Incus answer: {message}"),
            Self::Timeout => f.write_str("Incus did not answer by the deadline"),
            Self::TooLarge => f.write_str("the Incus answer is over the size cap"),
            Self::Refused(reason) => write!(f, "refused before any write was sent: {reason}"),
            Self::AppliedUnconfirmed(unconfirmed) => write!(
                f,
                "the write landed, but its read-back is not confirmed: {}",
                unconfirmed.reason
            ),
        }
    }
}

impl std::error::Error for IncusError {}

/// Reads Incus. The only method is GET, and the result is the answer's unwrapped `metadata`.
pub trait IncusRead: Send + Sync {
    fn get(&self, path: &str) -> Result<Value, IncusError>;
}

/// A GET-only client for Incus's unix socket.
#[derive(Debug, Clone)]
pub struct UnixIncus {
    socket: PathBuf,
    timeout: Duration,
}

impl UnixIncus {
    /// A client for the Incus socket at `socket`, with the default deadline.
    pub fn new(socket: PathBuf) -> Self {
        Self {
            socket,
            timeout: TIMEOUT,
        }
    }

    /// The same client with another deadline for each request. A deadline too long for the clock to hold is no deadline at
    /// all, so the caller's unbounded wait does not panic.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// The instant a request's deadline falls at, or `None` when the timeout reaches past the clock's range.
fn deadline_for(timeout: Duration) -> Option<Instant> {
    Instant::now().checked_add(timeout)
}

impl UnixIncus {
    /// Reads a raw text endpoint, such as an instance's console log. A non-success status is an Incus status error.
    pub fn get_text(&self, path: &str) -> Result<String, IncusError> {
        check_path(path)?;
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: incus\r\nAccept: text/plain\r\nConnection: close\r\n\r\n"
        );
        let framed = exchange(&self.socket, self.timeout, request.as_bytes())?;
        if !(200..300).contains(&framed.status) {
            return Err(IncusError::Status {
                code: framed.status,
                message: framed.reason,
            });
        }
        String::from_utf8(framed.body)
            .map_err(|_| IncusError::Decode("the text is not UTF-8".to_string()))
    }
}

impl IncusRead for UnixIncus {
    fn get(&self, path: &str) -> Result<Value, IncusError> {
        check_path(path)?;
        // The one method this client writes is the constant below. The writer module builds its own requests.
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: incus\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
        );
        let framed = exchange(&self.socket, self.timeout, request.as_bytes())?;
        decode(framed.into_raw())
    }
}

/// One answer off the wire, with its headers kept for the writer's ETags.
#[derive(Debug)]
pub(crate) struct Framed {
    pub(crate) status: u16,
    pub(crate) reason: String,
    /// The headers, kept for the writer's ETags. Only the writer reads them.
    #[cfg(feature = "write")]
    headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

impl Framed {
    /// The first value of a header whose lower-case name is `name`.
    #[cfg(feature = "write")]
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    pub(crate) fn into_raw(self) -> RawResponse {
        RawResponse {
            status: self.status,
            reason: self.reason,
            body: self.body,
        }
    }
}

/// Sends one request over the socket and reads the answer, under one deadline of `timeout` from the connect. The caller
/// builds the request bytes; [`UnixIncus::get`] is the only caller that builds a GET. Every request any client in this crate
/// sends passes here, and [`guard_request`] runs first, so a PATCH to a project is refused before the socket is touched.
pub(crate) fn exchange(
    socket: &Path,
    timeout: Duration,
    request: &[u8],
) -> Result<Framed, IncusError> {
    guard_request(request)?;
    if timeout.is_zero() {
        return Err(IncusError::Timeout);
    }
    let deadline = deadline_for(timeout);
    let mut stream = UnixStream::connect(socket).map_err(io_error)?;
    stream.set_write_timeout(Some(timeout)).map_err(io_error)?;
    stream.write_all(request).map_err(io_error)?;
    let mut reader = BufReader::new(Deadline {
        stream: &stream,
        deadline,
    });
    read_framed(&mut reader, MAX_BODY)
}

/// Why [`exchange`] refuses a PATCH to a project.
const PROJECT_PATCH: &str = "a project is never sent a PATCH";

/// Refuses a request no client may send, before any byte of it is written: a PATCH to a project. Incus 7.3's project PATCH
/// replaces the whole config with the keys it names (`docs/OWNERSHIP.md`, Rule 43), so one PATCH would erase the kernel's mark,
/// `restricted` and every feature; a project's config is written only as a PUT of the whole config.
///
/// The request line is read as `<method> <origin-form target> HTTP/1.x`, after any line breaks before it. The method is compared
/// without regard to case. A PATCH is refused when the target's path names `/1.0/projects` or anything under it: the query and
/// the fragment are cut off, the path is percent-decoded until it no longer changes, its empty and `.` segments are dropped, its
/// `..` segments are applied, and its first two segments are compared without regard to case. A request line that cannot be
/// read this way is refused too, since every client in this crate builds a plain one.
pub(crate) fn guard_request(request: &[u8]) -> Result<(), IncusError> {
    let unreadable = || IncusError::Refused("the request line is not a plain one".to_string());
    let start = request
        .iter()
        .position(|b| *b != b'\r' && *b != b'\n')
        .unwrap_or(request.len());
    let rest = &request[start..];
    let end = rest
        .iter()
        .position(|b| *b == b'\r' || *b == b'\n')
        .ok_or_else(unreadable)?;
    let line = std::str::from_utf8(&rest[..end]).map_err(|_| unreadable())?;
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(unreadable());
    };
    let token = !method.is_empty() && method.bytes().all(|b| b.is_ascii_alphabetic());
    let origin_form = target.starts_with('/') && target.bytes().all(|b| (0x21..=0x7e).contains(&b));
    if !token || !origin_form || !version.starts_with("HTTP/1.") {
        return Err(unreadable());
    }
    if method.eq_ignore_ascii_case("PATCH") && names_projects(target) {
        return Err(IncusError::Refused(PROJECT_PATCH.to_string()));
    }
    Ok(())
}

/// Whether a request target's path is `/1.0/projects` or a path under it, read as [`guard_request`] states.
fn names_projects(target: &str) -> bool {
    let mut path = cut_query(target).to_string();
    loop {
        let decoded = percent_decode(&path);
        let decoded = cut_query(&decoded).to_string();
        if decoded == path {
            break;
        }
        path = decoded;
    }
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    matches!(
        segments.as_slice(),
        [api, projects, ..] if api.eq_ignore_ascii_case("1.0") && projects.eq_ignore_ascii_case("projects")
    )
}

/// A target without its query and its fragment.
fn cut_query(target: &str) -> &str {
    target.split(['?', '#']).next().unwrap_or("")
}

/// Decodes each `%XX` escape once. An escape that is not two hex digits is kept as it is, and bytes that are not UTF-8 are
/// replaced, so the result is always text.
pub(crate) fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let escape = (bytes[at] == b'%')
            .then(|| bytes.get(at + 1..at + 3))
            .flatten()
            .filter(|hex| hex.iter().all(u8::is_ascii_hexdigit))
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match escape {
            Some(byte) => {
                out.push(byte);
                at += 3;
            }
            None => {
                out.push(bytes[at]);
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The request target must be a plain origin-form path: ASCII, no spaces or controls, no fragment, and short. Anything else
/// could split the request line, so it is refused before the socket is touched.
pub(crate) fn check_path(path: &str) -> Result<(), IncusError> {
    let plain = path.len() <= MAX_PATH
        && path.starts_with('/')
        && path
            .bytes()
            .all(|b| (0x21..=0x7e).contains(&b) && b != b'#');
    if plain {
        Ok(())
    } else {
        Err(IncusError::Io(
            "refused a request path that is not a plain origin-form target".to_string(),
        ))
    }
}

/// A reader that gives up when the request's deadline passes, whatever the socket is doing. With no deadline it waits as
/// long as the socket does.
struct Deadline<'a> {
    stream: &'a UnixStream,
    deadline: Option<Instant>,
}

impl Read for Deadline<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self
            .deadline
            .map(|deadline| {
                deadline
                    .checked_duration_since(Instant::now())
                    .filter(|left| !left.is_zero())
                    .ok_or(io::ErrorKind::TimedOut)
            })
            .transpose()?;
        self.stream.set_read_timeout(left)?;
        let mut stream: &UnixStream = self.stream;
        stream.read(buf)
    }
}

fn io_error(error: io::Error) -> IncusError {
    match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => IncusError::Timeout,
        _ => IncusError::Io(error.to_string()),
    }
}

fn read_error(error: io::Error) -> IncusError {
    if error.kind() == io::ErrorKind::UnexpectedEof {
        IncusError::Decode("the answer ended early".to_string())
    } else {
        io_error(error)
    }
}

fn decode_error(message: &str) -> IncusError {
    IncusError::Decode(message.to_string())
}

/// One HTTP/1.1 answer, read off the wire with its body already framed.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RawResponse {
    pub(crate) status: u16,
    pub(crate) reason: String,
    pub(crate) body: Vec<u8>,
}

/// Reads one answer and drops its headers. The unit tests use it; the clients read the headers too, through
/// [`read_framed`].
#[cfg(test)]
fn read_response<R: BufRead>(reader: &mut R, max_body: usize) -> Result<RawResponse, IncusError> {
    read_framed(reader, max_body).map(Framed::into_raw)
}

/// Reads one answer. The head shares one byte budget. The body is framed by a chunked coding, a `Content-Length`, or the
/// connection's end, in that order, and never grows past `max_body`.
fn read_framed<R: BufRead>(reader: &mut R, max_body: usize) -> Result<Framed, IncusError> {
    let mut budget = MAX_HEAD;
    let status_line = read_line(reader, &mut budget)?;
    let (status, reason) = parse_status_line(&status_line)?;
    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        let line = read_line(reader, &mut budget)?;
        if line.is_empty() {
            break;
        }
        headers.push(parse_header(&line)?);
    }

    let body = if status == 204 || status == 304 {
        Vec::new()
    } else if let Some(coding) = single_header(&headers, "transfer-encoding")? {
        if !coding.eq_ignore_ascii_case("chunked") {
            return Err(decode_error("an unsupported transfer coding"));
        }
        read_chunked(reader, max_body)?
    } else if let Some(length) = single_header(&headers, "content-length")? {
        let length = parse_length(length)?;
        if length > max_body as u64 {
            return Err(IncusError::TooLarge);
        }
        let mut body = vec![0u8; length as usize];
        reader.read_exact(&mut body).map_err(read_error)?;
        body
    } else {
        read_to_cap(reader, max_body)?
    };
    Ok(Framed {
        status,
        reason,
        #[cfg(feature = "write")]
        headers,
        body,
    })
}

fn parse_status_line(line: &[u8]) -> Result<(u16, String), IncusError> {
    let text =
        std::str::from_utf8(line).map_err(|_| decode_error("a status line that is not text"))?;
    let mut parts = text.splitn(3, ' ');
    let version = parts.next().unwrap_or("");
    let code = parts.next().unwrap_or("");
    let reason = parts.next().unwrap_or("");
    if !version.starts_with("HTTP/1.") {
        return Err(decode_error("a status line that is not HTTP/1"));
    }
    if code.len() != 3 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return Err(decode_error("a status code that is not three digits"));
    }
    let status: u16 = code
        .parse()
        .map_err(|_| decode_error("a status code that is not three digits"))?;
    if !(200..=599).contains(&status) {
        return Err(decode_error("an interim or unknown status code"));
    }
    Ok((status, reason.to_string()))
}

fn parse_header(line: &[u8]) -> Result<(String, String), IncusError> {
    let text = std::str::from_utf8(line).map_err(|_| decode_error("a header that is not text"))?;
    let (name, value) = text
        .split_once(':')
        .ok_or_else(|| decode_error("a header without a colon"))?;
    if name.is_empty() || name.bytes().any(|b| b.is_ascii_whitespace()) {
        return Err(decode_error("a header name that is not a token"));
    }
    Ok((name.to_ascii_lowercase(), value.trim().to_string()))
}

/// The one value of a header that must not repeat, or `None` when it is absent.
fn single_header<'a>(
    headers: &'a [(String, String)],
    name: &str,
) -> Result<Option<&'a str>, IncusError> {
    let mut values = headers.iter().filter(|(key, _)| key.as_str() == name);
    let first = values.next().map(|(_, value)| value.as_str());
    if values.next().is_some() {
        return Err(decode_error("a header that appears twice"));
    }
    Ok(first)
}

fn parse_length(value: &str) -> Result<u64, IncusError> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(decode_error("a content length that is not digits"));
    }
    value.parse().map_err(|_| IncusError::TooLarge)
}

/// One line without its line break, read against the head budget. A line the budget cannot hold is `TooLarge`.
fn read_line<R: BufRead>(reader: &mut R, budget: &mut usize) -> Result<Vec<u8>, IncusError> {
    let mut line = Vec::new();
    let read = reader
        .by_ref()
        .take(*budget as u64 + 1)
        .read_until(b'\n', &mut line)
        .map_err(read_error)?;
    if read == 0 {
        return Err(decode_error("the answer ended before its head"));
    }
    if read > *budget {
        return Err(IncusError::TooLarge);
    }
    *budget -= read;
    if line.last() != Some(&b'\n') {
        return Err(decode_error("the answer ended mid-line"));
    }
    line.pop();
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    Ok(line)
}

/// A chunked body. Each chunk's size, extension and trailers are read; the total stays within `max_body`.
fn read_chunked<R: BufRead>(reader: &mut R, max_body: usize) -> Result<Vec<u8>, IncusError> {
    let mut body = Vec::new();
    loop {
        let mut budget = MAX_HEAD;
        let line = read_line(reader, &mut budget)?;
        let size_field = line.split(|b| *b == b';').next().unwrap_or_default();
        let size = parse_chunk_size(size_field)?;
        if size == 0 {
            while !read_line(reader, &mut budget)?.is_empty() {}
            return Ok(body);
        }
        let room = max_body - body.len();
        if size > room as u64 {
            return Err(IncusError::TooLarge);
        }
        let start = body.len();
        body.resize(start + size as usize, 0);
        reader.read_exact(&mut body[start..]).map_err(read_error)?;
        if !read_line(reader, &mut budget)?.is_empty() {
            return Err(decode_error("a chunk was not followed by a line break"));
        }
    }
}

fn parse_chunk_size(field: &[u8]) -> Result<u64, IncusError> {
    let text = field.trim_ascii();
    if text.is_empty() {
        return Err(decode_error("an empty chunk size"));
    }
    let mut size: u64 = 0;
    for &byte in text {
        let digit = (byte as char)
            .to_digit(16)
            .ok_or_else(|| decode_error("a chunk size that is not hex"))?;
        size = size
            .checked_mul(16)
            .and_then(|s| s.checked_add(u64::from(digit)))
            .ok_or(IncusError::TooLarge)?;
    }
    Ok(size)
}

/// A body that ends with the connection. One byte past the cap is `TooLarge`.
fn read_to_cap<R: BufRead>(reader: &mut R, max_body: usize) -> Result<Vec<u8>, IncusError> {
    let mut body = Vec::new();
    reader
        .by_ref()
        .take(max_body as u64 + 1)
        .read_to_end(&mut body)
        .map_err(read_error)?;
    if body.len() > max_body {
        return Err(IncusError::TooLarge);
    }
    Ok(body)
}

/// Cuts a status message to at most 512 bytes, stepping back to a character boundary when the cut falls inside one.
pub(crate) fn cap_message(mut message: String) -> String {
    if message.len() > MAX_MESSAGE {
        let mut end = MAX_MESSAGE;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
    }
    message
}

/// The envelope of a success answer. A non-success status is a `Status` error, whose message is the envelope's `error` text,
/// or the reason phrase when the body is not an envelope. An error envelope under any status is a `Status` error too. Only
/// that text is kept, capped at 512 bytes.
pub(crate) fn checked_envelope(response: &RawResponse) -> Result<Value, IncusError> {
    if !(200..300).contains(&response.status) {
        let message = serde_json::from_slice::<Value>(&response.body)
            .ok()
            .and_then(|envelope| {
                envelope
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .filter(|message| !message.is_empty())
            .unwrap_or_else(|| response.reason.clone());
        return Err(IncusError::Status {
            code: response.status,
            message: cap_message(message),
        });
    }
    let envelope: Value =
        serde_json::from_slice(&response.body).map_err(|_| decode_error("the body is not JSON"))?;
    let object = envelope
        .as_object()
        .ok_or_else(|| decode_error("the body is not a JSON object"))?;
    if object.get("type").and_then(Value::as_str) == Some("error") {
        let code = object
            .get("error_code")
            .and_then(Value::as_u64)
            .and_then(|code| u16::try_from(code).ok())
            .filter(|code| (400..600).contains(code))
            .unwrap_or(response.status);
        let message = object
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        return Err(IncusError::Status {
            code,
            message: cap_message(message),
        });
    }
    Ok(envelope)
}

/// The metadata of a success envelope.
fn decode(response: RawResponse) -> Result<Value, IncusError> {
    let envelope = checked_envelope(&response)?;
    envelope
        .get("metadata")
        .cloned()
        .ok_or_else(|| decode_error("the answer has no metadata"))
}

/// The listing of every operation Incus holds, in every project, each with its body. Incus keys the listing by each operation's
/// status in lower case (`running`, `success`, ...), and keeps an ended operation listed for a few seconds.
pub const OPERATIONS_PATH: &str = "/1.0/operations?all-projects=true&recursion=1";

/// An operation that has not ended: its id, its status code, the URL of each object it names, its class and its description,
/// as Incus renders them. Incus gives an operation pending (105), running (103) or cancelling (104) while it works, and 200, 400
/// or 401 once it ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningOp {
    pub id: String,
    pub status_code: u16,
    pub resources: Vec<String>,
    /// `task`, `websocket` or `token`, when the operation gives its class as text.
    pub class: Option<String>,
    /// Incus's name for the operation's type, such as `Creating instance`, when the operation gives it as text.
    pub description: Option<String>,
}

/// The descriptions Incus gives the two operation types that only attach to a running instance: a command run in it
/// (`CommandExec`, a websocket operation, or a task one when no websocket is asked for) and its console (`ConsoleShow`, a
/// websocket operation). Neither creates, changes or deletes the instance, and an interactive one stays running for as long as
/// its session is open. Incus's `internal/server/db/operationtype/operation_type.go` names them.
pub const SESSION_DESCRIPTIONS: [&str; 2] = ["Executing command", "Showing console"];

/// The class of an operation that runs nothing: Incus refuses a run hook for it, and holds it only to carry a token.
pub const TOKEN_CLASS: &str = "token";

impl RunningOp {
    /// Whether the operation can still create, change or delete what it names. A session ([`SESSION_DESCRIPTIONS`], matched
    /// exactly) and a token operation ([`TOKEN_CLASS`]) cannot. Every other operation can, one with no class or description
    /// included, and so can a websocket one: Incus runs a push migration's create as a websocket operation.
    pub fn may_write(&self) -> bool {
        let session = self
            .description
            .as_deref()
            .is_some_and(|description| SESSION_DESCRIPTIONS.contains(&description));
        !session && self.class.as_deref() != Some(TOKEN_CLASS)
    }
}

/// Whether an operation's status code says it has not ended. Every 1xx code counts, which covers the three Incus gives an
/// operation at work (105, 103 and 104), so a code Incus adds later holds an object rather than freeing it.
fn not_ended(code: u16) -> bool {
    (100..200).contains(&code)
}

/// An operation id: the characters of a uuid.
fn plain_operation_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

/// One operation's body: its id, which must be plain, its status code, and its resources, a map of lists of URLs. An absent or
/// null `resources` names nothing. Any other shape is a decode error. A class or a description that is not text is read as
/// none, which leaves the operation free to write ([`RunningOp::may_write`]).
fn parse_operation(body: &Value) -> Result<RunningOp, IncusError> {
    let object = body
        .as_object()
        .ok_or_else(|| decode_error("an operation is not an object"))?;
    let id = object
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| plain_operation_id(id))
        .ok_or_else(|| decode_error("an operation has no plain id"))?;
    let status_code = object
        .get("status_code")
        .and_then(Value::as_u64)
        .and_then(|code| u16::try_from(code).ok())
        .ok_or_else(|| decode_error("an operation has no status code"))?;
    let mut resources = Vec::new();
    match object.get("resources") {
        None | Some(Value::Null) => {}
        Some(Value::Object(kinds)) => {
            for urls in kinds.values() {
                match urls {
                    Value::Null => {}
                    Value::Array(urls) => {
                        for url in urls {
                            let url = url
                                .as_str()
                                .ok_or_else(|| decode_error("an operation resource is not text"))?;
                            resources.push(url.to_string());
                        }
                    }
                    _ => return Err(decode_error("an operation's resources are not lists")),
                }
            }
        }
        Some(_) => return Err(decode_error("an operation's resources are not a map")),
    }
    let text = |key: &str| object.get(key).and_then(Value::as_str).map(str::to_string);
    Ok(RunningOp {
        id: id.to_string(),
        status_code,
        resources,
        class: text("class"),
        description: text("description"),
    })
}

/// Every operation Incus is still working on, in every project, from one GET of [`OPERATIONS_PATH`]. An operation that has
/// ended is left out. A listing that is not a map of lists of operations, or an operation that does not parse, is a decode
/// error, so a caller that cannot read the listing learns nothing from it and holds what it was about to judge.
pub fn running_operations(read: &dyn IncusRead) -> Result<Vec<RunningOp>, IncusError> {
    let listing = read.get(OPERATIONS_PATH)?;
    let groups = listing
        .as_object()
        .ok_or_else(|| decode_error("the operations listing is not a map"))?;
    let mut running = Vec::new();
    for group in groups.values() {
        let operations = match group {
            Value::Null => continue,
            Value::Array(operations) => operations,
            _ => return Err(decode_error("an operations group is not a list")),
        };
        for operation in operations {
            let operation = parse_operation(operation)?;
            if not_ended(operation.status_code) {
                running.push(operation);
            }
        }
    }
    running.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(running)
}

/// The path segments, after `/1.0`, that name an object of `kind`. `None` stands for any one segment: a volume's, a bucket's
/// or a forward's parent when the caller names none. An instance is named under `instances`, and under the `containers` and
/// `virtual-machines` paths Incus also accepts.
fn object_patterns<'a>(
    kind: IncusKind,
    parent: Option<&'a str>,
    name: &'a str,
) -> Vec<Vec<Option<&'a str>>> {
    let name = Some(name);
    match kind {
        IncusKind::Project => vec![vec![Some("projects"), name]],
        IncusKind::Instance => ["instances", "containers", "virtual-machines"]
            .into_iter()
            .map(|collection| vec![Some(collection), name])
            .collect(),
        IncusKind::Network => vec![vec![Some("networks"), name]],
        IncusKind::Acl => vec![vec![Some("network-acls"), name]],
        IncusKind::Profile => vec![vec![Some("profiles"), name]],
        IncusKind::Zone => vec![vec![Some("network-zones"), name]],
        IncusKind::Volume => vec![vec![
            Some("storage-pools"),
            parent,
            Some("volumes"),
            Some("custom"),
            name,
        ]],
        IncusKind::Bucket => vec![vec![Some("storage-pools"), parent, Some("buckets"), name]],
        IncusKind::Forward => vec![vec![Some("networks"), parent, Some("forwards"), name]],
    }
}

/// The values of a query's `project` parameter, percent-decoded.
fn query_projects(query: &str) -> Vec<String> {
    query
        .split('&')
        .filter_map(|pair| pair.strip_prefix("project="))
        .map(percent_decode)
        .collect()
}

/// Whether `op` names the object of `kind` called `name` in `project` (with `parent` for a volume, a bucket or a forward).
///
/// Incus renders a resource URL with `?project=<p>` unless the operation's project is `default`, so a URL without a `project`
/// query names an object in `default`. The URL's path is percent-decoded and its empty segments dropped. It names the object when
/// it is the object's path or a path under it, such as a snapshot of the instance. A network, a forward and an ACL are compared in
/// `default` (`docs/OWNERSHIP.md`, Rule 40): Incus keeps them there for a kernel project, so their URL's project is not compared. A
/// project is named by its path alone.
pub fn operation_names(
    op: &RunningOp,
    kind: IncusKind,
    project: &str,
    parent: Option<&str>,
    name: &str,
) -> bool {
    let patterns = object_patterns(kind, parent, name);
    let any_project = matches!(
        kind,
        IncusKind::Project | IncusKind::Network | IncusKind::Forward | IncusKind::Acl
    );
    op.resources.iter().any(|url| {
        let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
        let query = query.split('#').next().unwrap_or("");
        let path = percent_decode(path);
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let Some(("1.0", rest)) = segments.split_first().map(|(first, rest)| (*first, rest)) else {
            return false;
        };
        let projects = query_projects(query);
        let in_project = any_project
            || if projects.is_empty() {
                project == "default"
            } else {
                projects.iter().any(|named| named == project)
            };
        in_project
            && patterns.iter().any(|pattern| {
                rest.len() >= pattern.len()
                    && pattern
                        .iter()
                        .zip(rest)
                        .all(|(want, got)| want.is_none_or(|want| want == *got))
            })
    })
}

/// Every operation Incus is still working on that names the object (see [`operation_names`]), from one GET of the listing.
pub fn running_on(
    read: &dyn IncusRead,
    kind: IncusKind,
    project: &str,
    parent: Option<&str>,
    name: &str,
) -> Result<Vec<RunningOp>, IncusError> {
    Ok(running_operations(read)?
        .into_iter()
        .filter(|op| operation_names(op, kind, project, parent, name))
        .collect())
}

/// Every operation Incus is still working on that names the object and may still create, change or delete it
/// ([`RunningOp::may_write`]), from one GET of the listing: [`running_on`] without the sessions and the token operations.
pub fn writing_on(
    read: &dyn IncusRead,
    kind: IncusKind,
    project: &str,
    parent: Option<&str>,
    name: &str,
) -> Result<Vec<RunningOp>, IncusError> {
    Ok(running_on(read, kind, project, parent, name)?
        .into_iter()
        .filter(RunningOp::may_write)
        .collect())
}

/// Waits on operation `id` for at most `timeout`, in whole seconds, with one GET of `/1.0/operations/<id>/wait?timeout=<s>`.
/// Incus answers when the operation ends or the timeout passes, with the operation as it is then. Returns whether it is still
/// running. An operation Incus no longer holds (404) has ended. The answer must name `id`. The read client's own deadline must be
/// longer than `timeout`, or the hold reads as [`IncusError::Timeout`]. An id that is not plain is refused before any request.
pub fn wait_operation(
    read: &dyn IncusRead,
    id: &str,
    timeout: Duration,
) -> Result<bool, IncusError> {
    if !plain_operation_id(id) {
        return Err(IncusError::Refused(
            "an operation id that is not plain".to_string(),
        ));
    }
    let path = format!("/1.0/operations/{id}/wait?timeout={}", timeout.as_secs());
    match read.get(&path) {
        Ok(body) => {
            let operation = parse_operation(&body)?;
            if operation.id != id {
                return Err(decode_error("the wait answered for another operation"));
            }
            Ok(not_ended(operation.status_code))
        }
        Err(IncusError::Status { code: 404, .. }) => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use serde_json::json;

    use super::*;

    fn parse(bytes: &[u8], max_body: usize) -> Result<RawResponse, IncusError> {
        read_response(&mut BufReader::new(Cursor::new(bytes.to_vec())), max_body)
    }

    fn raw(status: u16, reason: &str, body: &[u8]) -> RawResponse {
        RawResponse {
            status,
            reason: reason.to_string(),
            body: body.to_vec(),
        }
    }

    #[test]
    fn a_content_length_body_is_read_to_its_length_and_no_further() {
        let raw = parse(
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nokEXTRA",
            MAX_BODY,
        )
        .expect("a framed body");
        assert_eq!(raw.status, 200);
        assert_eq!(raw.body, b"ok");
    }

    #[test]
    fn a_bare_line_feed_is_accepted_as_a_line_break() {
        let raw =
            parse(b"HTTP/1.1 200 OK\nContent-Length: 1\n\nx", MAX_BODY).expect("a bare LF head");
        assert_eq!(raw.body, b"x");
    }

    #[test]
    fn a_chunked_body_with_extensions_and_trailers_is_decoded() {
        let bytes = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5;name=v\r\nhello\r\n6\r\n world\r\n0\r\nX-Trailer: 1\r\n\r\n";
        assert_eq!(
            parse(bytes, MAX_BODY).expect("chunked").body,
            b"hello world"
        );
    }

    #[test]
    fn a_chunked_body_whose_total_passes_the_cap_is_too_large() {
        let single =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        assert_eq!(parse(single, 4), Err(IncusError::TooLarge));
        let summed = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n3\r\ndef\r\n0\r\n\r\n";
        assert_eq!(parse(summed, 5), Err(IncusError::TooLarge));
        assert_eq!(parse(summed, 6).expect("exactly the cap").body, b"abcdef");
    }

    #[test]
    fn a_chunk_size_that_overflows_is_too_large_and_one_that_is_not_hex_is_a_decode_error() {
        let overflow =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nFFFFFFFFFFFFFFFFFF\r\n";
        assert_eq!(parse(overflow, MAX_BODY), Err(IncusError::TooLarge));
        let not_hex = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n";
        assert!(matches!(
            parse(not_hex, MAX_BODY),
            Err(IncusError::Decode(_))
        ));
        let empty = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\r\n";
        assert!(matches!(parse(empty, MAX_BODY), Err(IncusError::Decode(_))));
    }

    #[test]
    fn a_chunk_not_followed_by_a_line_break_is_refused() {
        let bytes = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabcXX0\r\n\r\n";
        assert!(matches!(parse(bytes, MAX_BODY), Err(IncusError::Decode(_))));
    }

    #[test]
    fn a_body_that_ends_with_the_connection_is_read_to_the_end() {
        let raw = parse(
            b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{\"a\":1}",
            MAX_BODY,
        )
        .expect("close-delimited");
        assert_eq!(raw.body, b"{\"a\":1}");
    }

    #[test]
    fn a_close_delimited_body_one_byte_over_the_cap_is_too_large() {
        let bytes = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nabcde";
        assert_eq!(parse(bytes, 4), Err(IncusError::TooLarge));
        assert_eq!(
            parse(&bytes[..bytes.len() - 1], 4)
                .expect("exactly the cap")
                .body,
            b"abcd"
        );
    }

    #[test]
    fn a_declared_length_over_the_cap_is_refused_before_any_body_byte_is_read() {
        // No body follows the head, so a reader that tried to read the body would report the answer as ended early.
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY + 1
        );
        assert_eq!(parse(head.as_bytes(), MAX_BODY), Err(IncusError::TooLarge));
    }

    #[test]
    fn a_body_of_exactly_the_cap_is_accepted_and_one_byte_more_is_refused() {
        let prefix: &[u8] = br#"{"metadata":""#;
        let suffix: &[u8] = br#""}"#;
        let fill = MAX_BODY - prefix.len() - suffix.len();
        let mut body = prefix.to_vec();
        body.extend(std::iter::repeat_n(b'a', fill));
        body.extend_from_slice(suffix);
        assert_eq!(body.len(), MAX_BODY);

        let mut exact =
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
        exact.extend_from_slice(&body);
        assert_eq!(
            parse(&exact, MAX_BODY).expect("exactly the cap").body.len(),
            MAX_BODY
        );

        let mut over = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
        over.extend_from_slice(&body);
        over.push(b'}');
        assert_eq!(parse(&over, MAX_BODY), Err(IncusError::TooLarge));
    }

    #[test]
    fn a_status_line_that_is_not_plain_http_is_refused() {
        for bytes in [
            &b"SPDY/3 200 OK\r\n\r\n"[..],
            b"HTTP/1.1 99 Odd\r\n\r\n",
            b"HTTP/1.1 2000 Odd\r\n\r\n",
            b"HTTP/1.1 abc\r\n\r\n",
            b"HTTP/1.1 600 Odd\r\n\r\n",
        ] {
            assert!(
                matches!(parse(bytes, MAX_BODY), Err(IncusError::Decode(_))),
                "{bytes:?}"
            );
        }
    }

    #[test]
    fn a_header_that_repeats_or_names_an_unknown_coding_is_refused() {
        let repeated = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nContent-Length: 0\r\n\r\n";
        assert!(matches!(
            parse(repeated, MAX_BODY),
            Err(IncusError::Decode(_))
        ));
        let gzip = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\n\r\n";
        assert!(matches!(parse(gzip, MAX_BODY), Err(IncusError::Decode(_))));
        let no_colon = b"HTTP/1.1 200 OK\r\nNoColon\r\n\r\n";
        assert!(matches!(
            parse(no_colon, MAX_BODY),
            Err(IncusError::Decode(_))
        ));
        let spaced = b"HTTP/1.1 200 OK\r\nBad Name: x\r\n\r\n";
        assert!(matches!(
            parse(spaced, MAX_BODY),
            Err(IncusError::Decode(_))
        ));
    }

    #[test]
    fn a_head_over_its_budget_is_too_large() {
        let mut bytes = b"HTTP/1.1 200 OK\r\nX: ".to_vec();
        bytes.extend(std::iter::repeat_n(b'a', MAX_HEAD));
        bytes.extend_from_slice(b"\r\n\r\n");
        assert_eq!(parse(&bytes, MAX_BODY), Err(IncusError::TooLarge));
    }

    #[test]
    fn an_answer_that_ends_early_is_a_decode_error() {
        let short_body = b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabc";
        assert!(matches!(
            parse(short_body, MAX_BODY),
            Err(IncusError::Decode(_))
        ));
        let no_blank_line = b"HTTP/1.1 200 OK\r\nX: y\r\n";
        assert!(matches!(
            parse(no_blank_line, MAX_BODY),
            Err(IncusError::Decode(_))
        ));
        assert!(matches!(parse(b"", MAX_BODY), Err(IncusError::Decode(_))));
    }

    #[test]
    fn a_success_envelope_yields_its_metadata() {
        let body = br#"{"type":"sync","status":"Success","status_code":200,"metadata":{"x":1}}"#;
        assert_eq!(decode(raw(200, "OK", body)), Ok(json!({"x": 1})));
    }

    #[test]
    fn an_error_status_yields_the_envelope_message() {
        let body = br#"{"type":"error","error":"not found","error_code":404,"metadata":null}"#;
        assert_eq!(
            decode(raw(404, "Not Found", body)),
            Err(IncusError::Status {
                code: 404,
                message: "not found".to_string()
            })
        );
    }

    #[test]
    fn an_error_status_without_an_envelope_uses_the_reason_and_never_the_body() {
        let body = b"<html>body-marker-9d1e</html>";
        let error = decode(raw(502, "Bad Gateway", body)).expect_err("a 502");
        assert_eq!(
            error,
            IncusError::Status {
                code: 502,
                message: "Bad Gateway".to_string()
            }
        );
        assert!(!format!("{error:?}").contains("body-marker"));
    }

    #[test]
    fn a_success_status_with_an_error_envelope_is_a_status_error() {
        let body = br#"{"type":"error","error":"denied","error_code":403}"#;
        assert_eq!(
            decode(raw(200, "OK", body)),
            Err(IncusError::Status {
                code: 403,
                message: "denied".to_string()
            })
        );
    }

    #[test]
    fn a_success_body_that_is_not_json_or_has_no_metadata_is_a_decode_error() {
        assert!(matches!(
            decode(raw(200, "OK", b"not json")),
            Err(IncusError::Decode(_))
        ));
        assert!(matches!(
            decode(raw(200, "OK", b"[1,2]")),
            Err(IncusError::Decode(_))
        ));
        assert!(matches!(
            decode(raw(200, "OK", br#"{"type":"sync"}"#)),
            Err(IncusError::Decode(_))
        ));
        assert!(matches!(
            decode(raw(204, "No Content", b"")),
            Err(IncusError::Decode(_))
        ));
    }

    #[test]
    fn the_request_path_is_refused_unless_it_is_a_plain_origin_target() {
        assert!(check_path("/1.0").is_ok());
        assert!(check_path("/1.0/instances?all-projects=true&recursion=1").is_ok());
        assert!(check_path(&format!("/{}", "a".repeat(MAX_PATH - 1))).is_ok());
        assert!(check_path(&format!("/{}", "a".repeat(MAX_PATH))).is_err());
        for bad in [
            "", "1.0", "/1.0 x", "/1.0\r\n", "/1.0\t", "/1.0#x", "/\u{0}", "/\u{7f}",
        ] {
            assert!(check_path(bad).is_err(), "{bad:?}");
        }
    }

    fn project_patch_refusal() -> IncusError {
        IncusError::Refused("a project is never sent a PATCH".to_string())
    }

    /// Exchanges `request` with a socket path nothing listens on. A request the guard let through fails to connect, as
    /// `IncusError::Io`, so a `Refused` answer shows the guard ran before any connection was made.
    fn sent(request: &[u8]) -> Result<(), IncusError> {
        let socket = std::env::temp_dir().join(format!("arca-guard-{}.sock", std::process::id()));
        exchange(&socket, Duration::from_millis(200), request).map(|_| ())
    }

    #[test]
    fn a_patch_to_a_project_path_is_refused_by_exchange_before_any_byte_is_sent() {
        for target in [
            "/1.0/projects/p1",
            "/1.0/projects",
            "/1.0/projects/",
            "/1.0/projects/p1/",
            "/1.0/projects?recursion=1",
            "/1.0/projects/p1?project=p1",
            "/1.0/projects/?x=1",
            "/1.0/%70rojects/p1",
            "/1.0/PROJECTS/p1",
            "/1.0%2Fprojects/p1",
            "/1.0/projects%2Fp1",
            "/1.0/projects%2fp1",
            "/1.0/%2570rojects/p1",
            "//1.0/projects/p1",
            "/1.0//projects/p1",
            "/1.0/./projects/p1",
            "/1.0/x/../projects/p1",
            "/1.0/projects#x",
            "/1.0/projects%3Fx",
        ] {
            for method in ["PATCH", "patch", "Patch"] {
                let request = format!(
                    "{method} {target} HTTP/1.1\r\nHost: incus\r\nContent-Length: 2\r\n\r\n{{}}"
                );
                assert_eq!(
                    sent(request.as_bytes()),
                    Err(project_patch_refusal()),
                    "{method} {target}"
                );
            }
            let leading = format!("\r\nPATCH {target} HTTP/1.1\r\nHost: incus\r\n\r\n");
            assert_eq!(
                sent(leading.as_bytes()),
                Err(project_patch_refusal()),
                "a leading line break before PATCH {target}"
            );
        }
    }

    #[test]
    fn a_request_whose_line_cannot_be_read_is_refused_before_any_byte_is_sent() {
        for request in [
            &b""[..],
            b"\r\n\r\n",
            b"PATCH\r\n\r\n",
            b"PATCH http://incus/1.0/projects/p1 HTTP/1.1\r\n\r\n",
            b"PATCH /1.0/\xffprojects HTTP/1.1\r\n\r\n",
            b"PATCH\t/1.0/projects/p1 HTTP/1.1\r\n\r\n",
            b"PATCH /1.0/projects/p1 HTTP/1.1",
        ] {
            assert!(
                matches!(sent(request), Err(IncusError::Refused(_))),
                "{request:?}"
            );
        }
    }

    #[test]
    fn the_guard_passes_every_other_method_and_every_patch_outside_the_projects_path() {
        for line in [
            "GET /1.0/projects/p1 HTTP/1.1",
            "PUT /1.0/projects/p1 HTTP/1.1",
            "POST /1.0/projects HTTP/1.1",
            "DELETE /1.0/projects/p1 HTTP/1.1",
            "PATCH /1.0/instances/c1?project=p1 HTTP/1.1",
            "PATCH /1.0/profiles/default?project=projects HTTP/1.1",
            "PATCH /1.0/network-acls/projects?project=default HTTP/1.1",
            "PATCH /1.0/projectsx HTTP/1.1",
            "PATCH /1.0/storage-pools/projects/volumes/custom/v1?project=p1 HTTP/1.1",
            "PATCH /2.0/projects/p1 HTTP/1.1",
        ] {
            let request = format!("{line}\r\nHost: incus\r\n\r\n");
            assert_eq!(guard_request(request.as_bytes()), Ok(()), "{line}");
        }
    }

    #[test]
    fn a_percent_escape_is_decoded_only_when_it_is_two_hex_digits() {
        assert_eq!(percent_decode("/a%2Fb%70"), "/a/bp");
        assert_eq!(percent_decode("%+f%zz%4"), "%+f%zz%4");
        assert_eq!(percent_decode("%2570"), "%70");
    }

    #[test]
    fn an_error_message_is_capped_at_512_bytes() {
        let body = format!(
            r#"{{"type":"error","error":"{}","error_code":500}}"#,
            "a".repeat(2000)
        );
        let error = decode(raw(500, "Internal Server Error", body.as_bytes())).expect_err("a 500");
        assert_eq!(
            error,
            IncusError::Status {
                code: 500,
                message: "a".repeat(512)
            }
        );
    }

    #[test]
    fn the_message_cap_never_splits_a_character() {
        // One ASCII byte, then two-byte characters: byte 512 falls inside a character, so the cap steps back to 511.
        let text = format!("a{}", "é".repeat(300));
        let body = format!(r#"{{"type":"error","error":"{text}","error_code":404}}"#);
        let IncusError::Status { message, .. } =
            decode(raw(404, "Not Found", body.as_bytes())).expect_err("a 404")
        else {
            panic!("expected a status error");
        };
        assert_eq!(message.len(), 511);
        assert!(message.chars().all(|c| c == 'a' || c == 'é'));
    }

    #[test]
    fn the_message_cap_applies_to_a_reason_phrase_and_to_a_success_status_envelope() {
        let reason = decode(raw(502, &"r".repeat(2000), b"")).expect_err("a 502");
        assert_eq!(
            reason,
            IncusError::Status {
                code: 502,
                message: "r".repeat(512)
            }
        );
        let body = format!(
            r#"{{"type":"error","error":"{}","error_code":403}}"#,
            "d".repeat(2000)
        );
        let denied = decode(raw(200, "OK", body.as_bytes())).expect_err("a success status");
        assert_eq!(
            denied,
            IncusError::Status {
                code: 403,
                message: "d".repeat(512)
            }
        );
    }
}
