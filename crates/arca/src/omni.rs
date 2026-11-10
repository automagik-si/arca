//! The Omni reach: what the three Omni verbs (`omni_instance_create`, `omni_pair`, `omni_status`) send to an omni-api
//! service, and how they read its answers. An Omni instance lives in a service Arca created from the `omni-api` template;
//! the verbs reach nothing of Omni's API but four fixed requests (`Route`), each a fixed method and path with at most a
//! checked instance id in it. Arca follows no redirect, reads at most `BODY_MAX` bytes of an answer, and waits at most
//! `TIMEOUT` for one.
//!
//! The secrets: every request sends `x-api-key` from the service's `OMNI_API_KEY` secret (made at `service_create`); the
//! Telegram bot token and the WhatsApp number are the owner's (`service_secret_set` over the owner's socket). They go into
//! a request's headers and body alone. Every string an answer or an error carries is redacted of the three values (the
//! number also in any formatting, without its `+`, as a WhatsApp JID carries it, and in Brazil with or without the mobile
//! 9; the token's part after its colon) and of any Omni key or bot token, and is then cleaned to one line.
//!
//! The client is plain HTTP/1.1 over TCP, one request per connection: the omni-api service answers on the service bridge,
//! where its ACL lets the bridge gateway reach its template's `droiddPort` and nothing else.

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The template every Omni verb's service is made from.
pub const OMNI_TEMPLATE: &str = "omni-api";
/// The service secret Arca made at create: Omni's API key.
pub const API_KEY: &str = "OMNI_API_KEY";
/// The owner's Telegram bot token, for a `telegram` instance.
pub const TELEGRAM_TOKEN: &str = "telegram_bot_token";
/// The owner's WhatsApp number, for `omni_pair`.
pub const PHONE: &str = "whatsapp_phone_number";
/// The channels `omni_instance_create` makes.
pub const CHANNELS: [&str; 2] = ["whatsapp-baileys", "telegram"];
/// The most of one answer Arca reads.
pub const BODY_MAX: usize = 1 << 20;
/// How long one request may take.
pub const TIMEOUT: Duration = Duration::from_secs(15);
/// How long a pairing attempt that reached Omni but gave no code holds off the next one (Omni's codes last 60 s).
pub const PAIR_COOLDOWN_S: u64 = 60;
/// The most instances `omni_status` lists.
pub const LIST_MAX: usize = 100;
/// What a redacted value reads as.
pub const REDACTED: &str = "[secret]";
/// The most of a response head Arca reads.
const HEAD_MAX: usize = 64 * 1024;

/// Where a service's Omni answers, given its name, address and port. Only tests replace it, with a fake Omni.
pub type OmniBase = Arc<dyn Fn(&str, Ipv4Addr, u16) -> SocketAddr + Send + Sync>;

/// How the Omni verbs reach Omni. The default is the service's own address and its template's port, `TIMEOUT` and
/// `BODY_MAX`; the node config cannot change it.
#[derive(Clone)]
pub struct OmniOptions {
    pub base: Option<OmniBase>,
    pub timeout: Duration,
    pub body_max: usize,
}

impl Default for OmniOptions {
    fn default() -> Self {
        Self {
            base: None,
            timeout: TIMEOUT,
            body_max: BODY_MAX,
        }
    }
}

/// Every request Arca makes to Omni.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route<'a> {
    /// `POST /api/v2/instances`
    Create,
    /// `GET /api/v2/instances?limit=100`
    List,
    /// `GET /api/v2/instances/<id>/status`
    Status(&'a str),
    /// `POST /api/v2/instances/<id>/pair`
    Pair(&'a str),
}

impl Route<'_> {
    pub fn method(&self) -> &'static str {
        match self {
            Self::Create | Self::Pair(_) => "POST",
            Self::List | Self::Status(_) => "GET",
        }
    }

    /// The request target. An id that is not a UUID is never put in a path: the caller checks it first.
    pub fn target(&self) -> String {
        match self {
            Self::Create => "/api/v2/instances".into(),
            Self::List => "/api/v2/instances?limit=100".into(),
            Self::Status(id) => format!("/api/v2/instances/{id}/status"),
            Self::Pair(id) => format!("/api/v2/instances/{id}/pair"),
        }
    }

    /// The method and path, without the query, as a message names the request.
    pub fn what(&self) -> String {
        let target = self.target();
        let path = target.split('?').next().unwrap_or_default();
        format!("{} {path}", self.method())
    }
}

/// An Omni instance id: a lowercase UUID.
pub fn is_uuid(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => b.is_ascii_digit() || (b'a'..=b'f').contains(b),
        })
}

/// A name Arca gives an Omni instance: `^[a-z0-9][a-z0-9_-]{0,62}$`.
pub fn instance_name_ok(text: &str) -> bool {
    let bytes = text.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 63
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_' || *b == b'-')
}

/// The WhatsApp number's form: `+` and 10-15 digits, the first not 0. Omni echoes a number it refuses, so Arca checks
/// the form before it sends one.
pub fn phone_ok(text: &str) -> bool {
    let Some(digits) = text.strip_prefix('+') else {
        return false;
    };
    (10..=15).contains(&digits.len())
        && digits.bytes().all(|b| b.is_ascii_digit())
        && !digits.starts_with('0')
}

/// A pairing code as Omni formats it: 8 letters or digits, optionally as XXXX-XXXX.
pub fn pair_code_ok(text: &str) -> bool {
    let plain: String = text.chars().filter(|c| *c != '-').collect();
    let dashes = text.len() - plain.len();
    plain.len() == 8
        && plain.chars().all(|c| c.is_ascii_alphanumeric())
        && (dashes == 0 || (dashes == 1 && text.as_bytes().get(4) == Some(&b'-')))
}

/// A number (`phone_ok`) as a result shows it: the `+` and two digits, then its last two.
pub fn mask_phone(phone: &str) -> String {
    let chars: Vec<char> = phone.chars().collect();
    if chars.len() < 5 {
        return "*".repeat(chars.len());
    }
    let head: String = chars[..3].iter().collect();
    let tail: String = chars[chars.len() - 2..].iter().collect();
    format!("{head}{}{tail}", "*".repeat(chars.len() - 5))
}

/// The digit strings a phone number (`+` and 8-15 digits) is written as: its digits, and a Brazilian mobile's
/// (`55 DD 9XXXXXXXX`) also without the 9 that older WhatsApp JIDs drop, or a 12-digit Brazilian one also with it.
pub fn phone_digits(phone: &str) -> Vec<String> {
    let digits = phone.strip_prefix('+').unwrap_or(phone);
    if !(8..=15).contains(&digits.len()) || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Vec::new();
    }
    let mut out = vec![digits.to_string()];
    if digits.starts_with("55") && digits.len() == 13 && digits.as_bytes()[4] == b'9' {
        out.push(format!("{}{}", &digits[..4], &digits[5..]));
    }
    if digits.starts_with("55") && digits.len() == 12 {
        out.push(format!("{}9{}", &digits[..4], &digits[4..]));
    }
    out
}

/// `text` with phone number `phone` replaced by `REDACTED` wherever its digits (`phone_digits`) appear whole, however
/// written: with or without the `+`, spaced, dashed, dotted, bracketed, or inside a WhatsApp JID. A longer digit run is
/// another number and stays. Any other `phone` changes nothing.
pub fn redact_phone(text: &str, phone: &str) -> String {
    let well_formed = phone
        .strip_prefix('+')
        .is_some_and(|d| (8..=15).contains(&d.len()) && d.bytes().all(|b| b.is_ascii_digit()));
    if !well_formed {
        return text.to_string();
    }
    let mut out = text.to_string();
    for digits in phone_digits(phone) {
        let body = digits
            .chars()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(r"[\s().-]*");
        let pattern = regex::Regex::new(&format!(r"\+?{body}")).expect("a digit pattern is valid");
        let mut kept = String::with_capacity(out.len());
        let mut copied = 0;
        let mut from = 0;
        while let Some(found) = pattern.find_at(&out, from) {
            let before = out[..found.start()].chars().next_back();
            let after = out[found.end()..].chars().next();
            if before.is_some_and(|c| c.is_ascii_digit())
                || after.is_some_and(|c| c.is_ascii_digit())
            {
                from = found.start()
                    + out[found.start()..]
                        .chars()
                        .next()
                        .map_or(1, char::len_utf8);
                continue;
            }
            kept.push_str(&out[copied..found.start()]);
            kept.push_str(REDACTED);
            copied = found.end();
            from = found.end();
        }
        kept.push_str(&out[copied..]);
        out = kept;
    }
    out
}

/// Credentials a text from Omni may carry that are not this service's: any Omni key, whole or masked, and any bot token.
fn foreign() -> &'static [regex::Regex] {
    use std::sync::OnceLock;
    static PATTERNS: OnceLock<Vec<regex::Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            r"\bomni_sk_[A-Za-z0-9*]+",
            r"\b[0-9]{6,12}:[A-Za-z0-9_-]{30,}",
        ]
        .into_iter()
        .map(|p| regex::Regex::new(p).expect("the credential pattern is valid"))
        .collect()
    })
}

/// Characters that reorder, hide or smuggle text: bidi controls, zero-width and invisible format characters, tag
/// characters and the variation selectors other than VS15 and VS16.
fn invisible(c: char) -> bool {
    matches!(c,
        '\u{ad}' | '\u{34f}' | '\u{61c}' | '\u{115f}' | '\u{1160}' | '\u{17b4}' | '\u{17b5}' | '\u{180b}'..='\u{180f}'
        | '\u{200b}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2064}'
        | '\u{2066}'..='\u{206f}' | '\u{3164}' | '\u{fe00}'..='\u{fe0d}' | '\u{feff}' | '\u{ffa0}'
        | '\u{fff9}'..='\u{fffb}' | '\u{1d173}'..='\u{1d17a}' | '\u{e0000}'..='\u{e007f}' | '\u{e0100}'..='\u{e01ef}')
}

/// `text` as one clean line of at most `max` characters: escape sequences, controls and invisible characters gone,
/// whitespace runs one space, trimmed, `…` where it was cut.
pub fn clean_line(text: &str, max: usize) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.next() {
                // CSI: parameters and intermediates, then one final byte.
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC, DCS, SOS, PM, APC: up to BEL or ST.
                Some(']' | 'P' | 'X' | '^' | '_') => {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        if c.is_whitespace() {
            plain.push(' ');
        } else if !c.is_control() && !invisible(c) {
            plain.push(c);
        }
    }
    let line = plain.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= max {
        return line;
    }
    if max == 0 {
        return String::new();
    }
    let mut cut: String = line.chars().take(max - 1).collect();
    cut.push('…');
    cut
}

/// `text` from Omni as one clean line of at most `max` characters, without `secrets` (a phone number also as
/// `redact_phone` finds it) or any foreign credential.
pub fn redact(text: &str, secrets: &[String], max: usize) -> String {
    let mut out = text.to_string();
    for value in secrets {
        out = out.replace(value.as_str(), REDACTED);
    }
    for value in secrets {
        out = redact_phone(&out, value);
    }
    for pattern in foreign() {
        out = pattern.replace_all(&out, REDACTED).into_owned();
    }
    clean_line(&out, max)
}

/// One answer: its status and its body.
#[derive(Debug)]
pub struct Answer {
    pub status: u16,
    pub body: Vec<u8>,
}

/// Why a request has no answer.
#[derive(Debug, PartialEq, Eq)]
pub enum HttpFailure {
    /// No connection: nothing reached Omni. The text is a short system code (`ECONNREFUSED`).
    Unreachable(&'static str),
    /// Connected, then failed before a whole answer: it may have reached Omni.
    Broken(&'static str),
    /// No answer before the timeout. A request that timed out may have reached Omni.
    Timeout,
    /// The answer is longer than the cap.
    TooLarge,
    /// The answer is not HTTP.
    Malformed,
}

impl HttpFailure {
    /// Whether the request may have reached Omni: everything but a connection that never opened.
    pub fn reached(&self) -> bool {
        !matches!(self, Self::Unreachable(_))
    }
}

fn io_code(error: &io::Error) -> &'static str {
    match error.kind() {
        io::ErrorKind::ConnectionRefused => "ECONNREFUSED",
        io::ErrorKind::ConnectionReset => "ECONNRESET",
        io::ErrorKind::ConnectionAborted => "ECONNABORTED",
        io::ErrorKind::HostUnreachable => "EHOSTUNREACH",
        io::ErrorKind::NetworkUnreachable => "ENETUNREACH",
        io::ErrorKind::AddrNotAvailable => "EADDRNOTAVAIL",
        io::ErrorKind::BrokenPipe => "EPIPE",
        io::ErrorKind::UnexpectedEof => "EOF",
        _ => "error",
    }
}

fn timed_out(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    )
}

/// One HTTP/1.1 request to `addr`: `x-api-key: key`, `accept: application/json`, `body` as JSON when there is one, and
/// `connection: close`. Waits at most `timeout` in all and reads at most `body_max` bytes of the body. No redirect is
/// followed: a 3xx is an answer like any other.
pub fn request(
    addr: SocketAddr,
    route: &Route<'_>,
    key: &str,
    body: Option<&[u8]>,
    timeout: Duration,
    body_max: usize,
) -> Result<Answer, HttpFailure> {
    let deadline = Instant::now() + timeout;
    let left = || {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or(HttpFailure::Timeout)
    };
    let mut stream = match TcpStream::connect_timeout(&addr, left()?) {
        Ok(stream) => stream,
        Err(error) if timed_out(&error) => return Err(HttpFailure::Timeout),
        Err(error) => return Err(HttpFailure::Unreachable(io_code(&error))),
    };
    let broken = |error: io::Error| {
        if timed_out(&error) {
            HttpFailure::Timeout
        } else {
            HttpFailure::Broken(io_code(&error))
        }
    };
    let mut head = format!(
        "{} {} HTTP/1.1\r\nhost: {addr}\r\nx-api-key: {key}\r\naccept: application/json\r\nconnection: close\r\n",
        route.method(),
        route.target()
    );
    if let Some(body) = body {
        head.push_str(&format!(
            "content-type: application/json\r\ncontent-length: {}\r\n",
            body.len()
        ));
    }
    head.push_str("\r\n");
    let mut out = head.into_bytes();
    out.extend_from_slice(body.unwrap_or_default());
    stream
        .set_write_timeout(Some(left()?))
        .map_err(|_| HttpFailure::Broken("error"))?;
    stream.write_all(&out).map_err(broken)?;
    let mut raw = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    // The head first.
    let head_end = loop {
        if let Some(at) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break at;
        }
        if raw.len() > HEAD_MAX {
            return Err(HttpFailure::Malformed);
        }
        stream
            .set_read_timeout(Some(left()?))
            .map_err(|_| HttpFailure::Broken("error"))?;
        match stream.read(&mut chunk) {
            Ok(0) => return Err(HttpFailure::Malformed),
            Ok(n) => raw.extend_from_slice(&chunk[..n]),
            Err(error) => return Err(broken(error)),
        }
    };
    let head = std::str::from_utf8(&raw[..head_end]).map_err(|_| HttpFailure::Malformed)?;
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| {
            let mut parts = line.split(' ');
            let version = parts.next()?;
            version.starts_with("HTTP/1.").then_some(())?;
            parts.next()?.parse::<u16>().ok()
        })
        .filter(|status| (100..=599).contains(status))
        .ok_or(HttpFailure::Malformed)?;
    let mut length = None;
    let mut chunked = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            return Err(HttpFailure::Malformed);
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            length = Some(value.parse::<usize>().map_err(|_| HttpFailure::Malformed)?);
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = value.to_ascii_lowercase().contains("chunked");
        }
    }
    if let (Some(length), false) = (length, chunked)
        && length > body_max
    {
        return Err(HttpFailure::TooLarge);
    }
    let mut rest = raw.split_off(head_end + 4);
    // Chunk framing adds a few bytes per chunk; more than this is surely past the cap.
    let raw_max = if chunked {
        body_max.saturating_add(body_max / 8 + 1024)
    } else {
        body_max
    };
    loop {
        if !chunked && length.is_some_and(|length| rest.len() >= length) {
            break;
        }
        if rest.len() > raw_max {
            return Err(HttpFailure::TooLarge);
        }
        stream
            .set_read_timeout(Some(left()?))
            .map_err(|_| HttpFailure::Broken("error"))?;
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => rest.extend_from_slice(&chunk[..n]),
            Err(error) => return Err(broken(error)),
        }
    }
    let body = if chunked {
        dechunk(&rest).ok_or(HttpFailure::Malformed)?
    } else if let Some(length) = length {
        if rest.len() < length {
            return Err(HttpFailure::Broken("EOF"));
        }
        rest.truncate(length);
        rest
    } else {
        rest
    };
    if body.len() > body_max {
        return Err(HttpFailure::TooLarge);
    }
    Ok(Answer { status, body })
}

/// A chunked body, decoded; `None` when its framing is broken.
fn dechunk(mut raw: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = raw.windows(2).position(|w| w == b"\r\n")?;
        let size_text = std::str::from_utf8(&raw[..line_end]).ok()?;
        let size = usize::from_str_radix(size_text.split(';').next()?.trim(), 16).ok()?;
        raw = &raw[line_end + 2..];
        if size == 0 {
            return Some(out);
        }
        if raw.len() < size + 2 || &raw[size..size + 2] != b"\r\n" {
            return None;
        }
        out.extend_from_slice(&raw[..size]);
        raw = &raw[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_are_the_four_fixed_requests() {
        let id = "0f8c4a3e-1b2d-4c5e-8f90-a1b2c3d4e5f6";
        assert_eq!(Route::Create.what(), "POST /api/v2/instances");
        assert_eq!(Route::List.target(), "/api/v2/instances?limit=100");
        assert_eq!(Route::List.what(), "GET /api/v2/instances");
        assert_eq!(
            Route::Status(id).target(),
            format!("/api/v2/instances/{id}/status")
        );
        assert_eq!(Route::Pair(id).method(), "POST");
        assert!(is_uuid(id));
        for bad in [&id.to_uppercase(), "x", &format!("{id}/logout"), &id[1..]] {
            assert!(!is_uuid(bad), "{bad}");
        }
    }

    #[test]
    fn forms_and_masks() {
        assert!(instance_name_ok("wa") && instance_name_ok("a_b-9"));
        for bad in ["", "Has Space", "../x", "_a", &"a".repeat(64), "A"] {
            assert!(!instance_name_ok(bad), "{bad}");
        }
        assert!(phone_ok("+5511987654321"));
        for bad in [
            "+55 11 98765-4321",
            "5511987654321",
            "+0011987654321",
            "+12345678",
        ] {
            assert!(!phone_ok(bad), "{bad}");
        }
        assert_eq!(mask_phone("+5511987654321"), "+55*********21");
        assert!(pair_code_ok("ABCD-1234") && pair_code_ok("abcd1234"));
        assert!(!pair_code_ok("ABC-D1234") && !pair_code_ok("ABCD-123") && !pair_code_ok(""));
    }

    #[test]
    fn clean_line_drops_escapes_controls_and_invisibles_and_caps() {
        assert_eq!(
            clean_line("a\u{1b}[31mred\u{1b}[0m\tb\r\nc\u{200b}d", 300),
            "ared b cd"
        );
        assert_eq!(clean_line("\u{1b}]0;title\u{7}x", 300), "x");
        assert_eq!(clean_line("abcdef", 4), "abc…");
        assert_eq!(clean_line("  spaced   out  ", 300), "spaced out");
    }

    #[test]
    fn dechunk_reads_framing_and_refuses_a_broken_one() {
        assert_eq!(
            dechunk(b"3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n").unwrap(),
            b"abcde"
        );
        assert!(dechunk(b"3\r\nab").is_none());
        assert!(dechunk(b"zz\r\n").is_none());
    }
}
