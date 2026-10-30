//! The GET-only Incus client.
//!
//! A request is written with one constant method, `GET`, and [`IncusRead`] has no other method, so this module cannot send
//! anything else. The response reader accepts a `Content-Length` body, a chunked body and a body that ends with the
//! connection. A body over [`MAX_BODY`] is refused as [`IncusError::TooLarge`], and an answer that does not finish by the
//! request's deadline is refused as [`IncusError::Timeout`]. No error carries a header or a body byte.

use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::Value;

/// The largest response body the client accepts, in bytes.
pub const MAX_BODY: usize = 16 * 1024 * 1024;
/// The deadline for one request, from connect to the last body byte.
pub const TIMEOUT: Duration = Duration::from_secs(10);
/// The most bytes the status line and headers may take.
const MAX_HEAD: usize = 64 * 1024;
/// The longest request path the client sends.
const MAX_PATH: usize = 2048;

/// Why a read failed. No variant carries a header or a body byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IncusError {
    /// The socket could not be connected, written or read.
    Io(String),
    /// Incus answered with a status that is not success, or with an error envelope.
    Status { code: u16, message: String },
    /// The answer is not the HTTP or JSON shape the client reads.
    Decode(String),
    /// The answer did not finish by the deadline.
    Timeout,
    /// The answer is over [`MAX_BODY`].
    TooLarge,
}

impl fmt::Display for IncusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(message) => write!(f, "socket error: {message}"),
            Self::Status { code, message } => write!(f, "Incus answered {code}: {message}"),
            Self::Decode(message) => write!(f, "could not read the Incus answer: {message}"),
            Self::Timeout => f.write_str("Incus did not answer by the deadline"),
            Self::TooLarge => f.write_str("the Incus answer is over the size cap"),
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

    /// The same client with another deadline for each request.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

impl IncusRead for UnixIncus {
    fn get(&self, path: &str) -> Result<Value, IncusError> {
        check_path(path)?;
        if self.timeout.is_zero() {
            return Err(IncusError::Timeout);
        }
        let deadline = Instant::now() + self.timeout;
        let mut stream = UnixStream::connect(&self.socket).map_err(io_error)?;
        stream
            .set_write_timeout(Some(self.timeout))
            .map_err(io_error)?;
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: incus\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).map_err(io_error)?;
        let mut reader = BufReader::new(Deadline {
            stream: &stream,
            deadline,
        });
        let response = read_response(&mut reader, MAX_BODY)?;
        decode(response)
    }
}

/// The request target must be a plain origin-form path: ASCII, no spaces or controls, no fragment, and short. Anything else
/// could split the request line, so it is refused before the socket is touched.
fn check_path(path: &str) -> Result<(), IncusError> {
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

/// A reader that gives up when the request's deadline passes, whatever the socket is doing.
struct Deadline<'a> {
    stream: &'a UnixStream,
    deadline: Instant,
}

impl Read for Deadline<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self
            .deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
            .ok_or(io::ErrorKind::TimedOut)?;
        self.stream.set_read_timeout(Some(left))?;
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
struct RawResponse {
    status: u16,
    reason: String,
    body: Vec<u8>,
}

/// Reads one answer. The head shares one byte budget. The body is framed by a chunked coding, a `Content-Length`, or the
/// connection's end, in that order, and never grows past `max_body`.
fn read_response<R: BufRead>(reader: &mut R, max_body: usize) -> Result<RawResponse, IncusError> {
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
    Ok(RawResponse {
        status,
        reason,
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

/// The metadata of a success envelope. A non-success status is a `Status` error, whose message is the envelope's `error`
/// text, or the reason phrase when the body is not an envelope. The body itself is never put in an error.
fn decode(response: RawResponse) -> Result<Value, IncusError> {
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
            .unwrap_or(response.reason);
        return Err(IncusError::Status {
            code: response.status,
            message,
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
        return Err(IncusError::Status { code, message });
    }
    object
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
}
