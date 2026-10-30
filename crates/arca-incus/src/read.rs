//! The GET-only Incus client.
//!
//! A request is written with one constant method, `GET`, and [`IncusRead`] has no other method, so this module cannot send
//! anything else. The response reader accepts a `Content-Length` body, a chunked body and a body that ends with the
//! connection. A body over [`MAX_BODY`] is refused as [`IncusError::TooLarge`], and an answer that does not finish by the
//! request's deadline is refused as [`IncusError::Timeout`]. No error carries a header byte or a raw body byte. A status
//! error keeps only the envelope's `error` text, or the reason phrase, cut to 512 bytes.

use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use arca_kernel::ownership::{ObjectRef, OwnerClass};
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
    /// the kernel's project family, or the request is not a plain one. A read may have been made. The text names the reason
    /// and never a value of the object.
    Refused(String),
    /// The write was accepted and applied, but reading the object back did not confirm it. Boxed, so the error stays small.
    AppliedUnconfirmed(Box<Unconfirmed>),
}

/// A write that Incus accepted whose read-back did not confirm it: the object is not this entry's own, or it does not show the
/// change. `owner` is the class the read found, when it found one. Nothing is retried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unconfirmed {
    pub object: ObjectRef,
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
/// builds the request bytes; [`UnixIncus::get`] is the only caller that builds a GET.
pub(crate) fn exchange(
    socket: &Path,
    timeout: Duration,
    request: &[u8],
) -> Result<Framed, IncusError> {
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
