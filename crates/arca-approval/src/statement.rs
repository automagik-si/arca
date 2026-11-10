//! The approval string and the statement it carries, read as droidd's `owner-approvals.ts` reads them.

use arca_kernel::canon::canonical_json;
use base64::Engine as _;
use base64::alphabet;
use base64::engine::GeneralPurpose;
use base64::engine::general_purpose::PAD_INDIFFERENT;
use serde_json::{Map, Value};

use crate::ApprovalRefusal;
use crate::passkey::parse_proof;

/// What an owner approval admits. Droidd's `ARCA_ACTIONS`, plus the service and database verbs an action proposal names.
/// `install` belongs to the recipes tier and is refused here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArcaAction {
    Delete,
    Publish,
    Spend,
    PasskeyAdd,
    PasskeyRemove,
    /// A change to the passkeys another machine's page host keeps in sync here (`peer_keys_sync`).
    PeerKeysSet,
    ServiceCreate,
    ServiceRemove,
    ServicePublish,
    ServiceUnpublish,
    ServiceConnect,
    ServiceDisconnect,
    PostgresCreate,
    PostgresDelete,
    GpuAttach,
    GpuDetach,
    StorageAdd,
    SnapshotCreate,
    SnapshotDelete,
    InstanceStart,
    InstanceStop,
    InstanceRestart,
}

impl ArcaAction {
    /// The action's name as a statement writes it. For an action proposal it is the gated verb's own name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Delete => "delete",
            Self::Publish => "publish",
            Self::Spend => "spend",
            Self::PasskeyAdd => "passkey_add",
            Self::PasskeyRemove => "passkey_remove",
            Self::PeerKeysSet => "peer_keys_set",
            Self::ServiceCreate => "service_create",
            Self::ServiceRemove => "service_remove",
            Self::ServicePublish => "service_publish",
            Self::ServiceUnpublish => "service_unpublish",
            Self::ServiceConnect => "service_connect",
            Self::ServiceDisconnect => "service_disconnect",
            Self::PostgresCreate => "postgres_create",
            Self::PostgresDelete => "postgres_delete",
            Self::GpuAttach => "gpu_attach",
            Self::GpuDetach => "gpu_detach",
            Self::StorageAdd => "storage_add",
            Self::SnapshotCreate => "snapshot_create",
            Self::SnapshotDelete => "snapshot_delete",
            Self::InstanceStart => "instance_start",
            Self::InstanceStop => "instance_stop",
            Self::InstanceRestart => "instance_restart",
        }
    }

    /// The action a verb name writes, or `None` when no approval admits it.
    pub fn from_name(text: &str) -> Option<ArcaAction> {
        match text {
            "delete" => Some(Self::Delete),
            "publish" => Some(Self::Publish),
            "spend" => Some(Self::Spend),
            "passkey_add" => Some(Self::PasskeyAdd),
            "passkey_remove" => Some(Self::PasskeyRemove),
            "peer_keys_set" => Some(Self::PeerKeysSet),
            "service_create" => Some(Self::ServiceCreate),
            "service_remove" => Some(Self::ServiceRemove),
            "service_publish" => Some(Self::ServicePublish),
            "service_unpublish" => Some(Self::ServiceUnpublish),
            "service_connect" => Some(Self::ServiceConnect),
            "service_disconnect" => Some(Self::ServiceDisconnect),
            "postgres_create" => Some(Self::PostgresCreate),
            "postgres_delete" => Some(Self::PostgresDelete),
            "gpu_attach" => Some(Self::GpuAttach),
            "gpu_detach" => Some(Self::GpuDetach),
            "storage_add" => Some(Self::StorageAdd),
            "snapshot_create" => Some(Self::SnapshotCreate),
            "snapshot_delete" => Some(Self::SnapshotDelete),
            "instance_start" => Some(Self::InstanceStart),
            "instance_stop" => Some(Self::InstanceStop),
            "instance_restart" => Some(Self::InstanceRestart),
            _ => None,
        }
    }

    fn from_statement(text: &str) -> Option<ArcaAction> {
        Self::from_name(text)
    }
}

/// The statement the owner signs. Times are UTC in `Date.prototype.toISOString` form, as the statement writes them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerApproval {
    pub host: String,
    pub action: ArcaAction,
    pub target: String,
    pub params_digest: String,
    pub issued_at: String,
    pub expires_at: String,
    pub nonce: String,
}

/// The seven keys an Arca statement carries.
const KEYS: [&str; 7] = [
    "host",
    "action",
    "target",
    "paramsDigest",
    "issuedAt",
    "expiresAt",
    "nonce",
];
/// The key droidd writes and Arca refuses: a droidd-bound approval cannot be spent here.
const DROIDD_KEY: &str = "droidd";
/// Droidd's size caps: an approval string (characters), its statement and its signature (decoded bytes).
const APPROVAL_MAX_CHARS: usize = 16_384;
const STATEMENT_MAX_BYTES: usize = 4_096;
const SIGNATURE_MAX_BYTES: usize = 8_192;
/// The first byte of a passkey proof, which is a JSON object. No armoured SSH signature starts with it.
pub(crate) const PASSKEY_PROOF_OPEN: u8 = b'{';

/// The base64 reader for droidd's approval parts: the standard alphabet, padding optional, and trailing bits tolerated as
/// Node tolerates them (`YR==` reads as `YQ==`). It is stricter than Node, which also decodes `QUJD=`, `QUJD==`, `QUJDR`
/// and `QUJ==`. This reader refuses those as malformed, so an approval droidd reads can be refused here, and never the
/// other way round.
const NODE_BASE64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    PAD_INDIFFERENT.with_decode_allow_trailing_bits(true),
);

fn malformed(why: impl Into<String>) -> ApprovalRefusal {
    ApprovalRefusal::Malformed(why.into())
}

/// Reads an approval string, `<base64 statement>.<base64 signature>`, into its statement bytes and its signature bytes.
/// Surrounding whitespace is trimmed, as droidd trims it. A signature whose first byte opens a JSON object (`{`) must be a
/// passkey proof of at most 8192 bytes, read as [`parse_proof`] reads one. Any other signature must be an armoured SSH
/// signature, as droidd's `armoredOf` requires. So a proof of neither form is refused before a verifier is asked about it.
pub fn split_approval(approval: &str) -> Result<(Vec<u8>, Vec<u8>), ApprovalRefusal> {
    if approval.is_empty() {
        return Err(malformed("this call needs the owner's signed approval"));
    }
    if approval.len() > APPROVAL_MAX_CHARS {
        return Err(malformed("the approval is longer than 16384 characters"));
    }
    let text = approval.trim_matches(is_js_space);
    let parts: Vec<&str> = text.split('.').collect();
    let [statement, signature] = parts[..] else {
        return Err(malformed(
            "the approval is not <base64 statement>.<base64 signature>",
        ));
    };
    let statement = decode_part(statement)?;
    let signature = decode_part(signature)?;
    if statement.is_empty() || statement.len() > STATEMENT_MAX_BYTES {
        return Err(malformed(
            "the statement is empty or longer than 4096 bytes",
        ));
    }
    if signature.is_empty() || signature.len() > SIGNATURE_MAX_BYTES {
        return Err(malformed(
            "the signature is empty or longer than 8192 bytes",
        ));
    }
    if signature.first() == Some(&PASSKEY_PROOF_OPEN) {
        parse_proof(&signature)?;
        return Ok((statement, signature));
    }
    if !is_armoured_ssh_signature(&signature) {
        return Err(malformed(
            "the signature is not an armoured SSH signature (ssh-keygen -Y sign)",
        ));
    }
    Ok((statement, signature))
}

/// One base64 part, matched as droidd's `^[A-Za-z0-9+/]+={0,2}$` matches it, then decoded.
fn decode_part(part: &str) -> Result<Vec<u8>, ApprovalRefusal> {
    let body = part.trim_end_matches('=');
    let padding = part.len() - body.len();
    let well_formed = !body.is_empty()
        && padding <= 2
        && body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/');
    if !well_formed {
        return Err(malformed("an approval part is not base64"));
    }
    NODE_BASE64
        .decode(part)
        .map_err(|_| malformed("an approval part is not base64"))
}

/// The lines `ssh-keygen -Y sign` writes around a signature's body.
const ARMOUR_BEGIN: &[u8] = b"-----BEGIN SSH SIGNATURE-----";
const ARMOUR_END: &[u8] = b"-----END SSH SIGNATURE-----";

/// Whether `bytes` is an armoured SSH signature: the BEGIN line, a body of base64 characters and line breaks, the END line,
/// then at most one line break. This is droidd's `armoredOf` pattern, matched by hand.
fn is_armoured_ssh_signature(bytes: &[u8]) -> bool {
    let Some(rest) = bytes.strip_prefix(ARMOUR_BEGIN) else {
        return false;
    };
    let Some(rest) = rest
        .strip_prefix(b"\r\n")
        .or_else(|| rest.strip_prefix(b"\n"))
    else {
        return false;
    };
    // The body holds no dash, so the END line starts at the first dash after the BEGIN line.
    let Some(end) = rest.iter().position(|b| *b == b'-') else {
        return false;
    };
    let (body, tail) = rest.split_at(end);
    let Some(tail) = tail.strip_prefix(ARMOUR_END) else {
        return false;
    };
    !body.is_empty()
        && body
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(*b, b'+' | b'/' | b'=' | b'\r' | b'\n'))
        && matches!(tail, b"" | b"\r" | b"\n" | b"\r\n")
}

/// The characters `String.prototype.trim` removes in JavaScript, which droidd's approval trim uses. Rust's own `trim`
/// differs on U+0085 and U+FEFF, so the set is spelled out.
fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

/// Parses a statement's bytes into an [`OwnerApproval`]. The bytes must be their own canonical JSON (sorted keys, no
/// whitespace, no duplicate key, no needless escape). Then the keys, then each field's form, in droidd's order.
pub fn parse_statement(bytes: &[u8]) -> Result<OwnerApproval, ApprovalRefusal> {
    let text = std::str::from_utf8(bytes).map_err(|_| malformed("the statement is not UTF-8"))?;
    let value: Value =
        serde_json::from_str(text).map_err(|_| malformed("the statement is not JSON"))?;
    let Value::Object(map) = &value else {
        return Err(malformed("the statement is not a JSON object"));
    };
    if canonical_json(&value).ok().as_deref() != Some(text) {
        return Err(ApprovalRefusal::NotCanonical);
    }
    check_keys(map)?;
    let field = |key: &str| map.get(key).and_then(Value::as_str).unwrap_or_default();
    let bad = |key: &str, want: &str| malformed(format!("the statement's {key} must be {want}"));

    let host = field("host");
    if !valid_host(host) {
        return Err(bad("host", "the droidd host's name"));
    }
    let action = ArcaAction::from_statement(field("action")).ok_or_else(|| {
        bad(
            "action",
            "delete, publish, spend, passkey_add, passkey_remove, service_create, service_remove, service_publish, service_unpublish, service_connect, service_disconnect, postgres_create, postgres_delete, gpu_attach, gpu_detach, storage_add, snapshot_create, snapshot_delete, instance_start, instance_stop or instance_restart",
        )
    })?;
    let target = field("target");
    if !valid_target(target) {
        return Err(bad(
            "target",
            "a name of 1 to 128 letters, digits, dots, dashes or underscores",
        ));
    }
    let params_digest = field("paramsDigest");
    if !valid_hex(params_digest, 64, 64) {
        return Err(bad(
            "paramsDigest",
            "the call's arguments' SHA-256 (64 lowercase hex)",
        ));
    }
    let issued_at = field("issuedAt");
    let expires_at = field("expiresAt");
    for (key, time) in [("issuedAt", issued_at), ("expiresAt", expires_at)] {
        if iso_ms(time).is_none() {
            return Err(bad(
                key,
                "a UTC time as toISOString writes it (2026-10-24T15:00:00.000Z)",
            ));
        }
    }
    let nonce = field("nonce");
    if !valid_hex(nonce, 32, 64) {
        return Err(bad("nonce", "32 to 64 lowercase hex digits"));
    }
    Ok(OwnerApproval {
        host: host.to_string(),
        action,
        target: target.to_string(),
        params_digest: params_digest.to_string(),
        issued_at: issued_at.to_string(),
        expires_at: expires_at.to_string(),
        nonce: nonce.to_string(),
    })
}

/// The statement's keys: not droidd's, none unknown, and each of the seven present. Unknown key names are not echoed.
fn check_keys(map: &Map<String, Value>) -> Result<(), ApprovalRefusal> {
    if map.contains_key(DROIDD_KEY) {
        return Err(malformed(
            "the statement names a droidd, which Arca does not accept",
        ));
    }
    if map.keys().any(|key| !KEYS.contains(&key.as_str())) {
        return Err(malformed("the statement has a key Arca does not read"));
    }
    if let Some(missing) = KEYS.iter().find(|key| !map.contains_key(**key)) {
        return Err(malformed(format!("the statement lacks {missing}")));
    }
    Ok(())
}

/// Whether `text` is a host name: `^[A-Za-z0-9][A-Za-z0-9.-]{0,252}$`, as droidd's `HOST` is.
fn valid_host(text: &str) -> bool {
    let bytes = text.as_bytes();
    (1..=253).contains(&bytes.len())
        && bytes[0].is_ascii_alphanumeric()
        && bytes[1..]
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'.' || *b == b'-')
}

/// Whether `text` is an approval's target: `^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$`, as droidd's `TARGET` is.
pub(crate) fn valid_target(text: &str) -> bool {
    let bytes = text.as_bytes();
    (1..=128).contains(&bytes.len())
        && bytes[0].is_ascii_alphanumeric()
        && bytes[1..]
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'.' || *b == b'_' || *b == b'-')
}

/// Whether `text` is between `min` and `max` lowercase hex digits.
pub(crate) fn valid_hex(text: &str, min: usize, max: usize) -> bool {
    (min..=max).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The epoch milliseconds of a time written as `toISOString` writes it, or `None` when it is not that exact form: the shape
/// `YYYY-MM-DDTHH:MM:SS.sssZ`, a real calendar date and a real clock time.
pub fn iso_ms(text: &str) -> Option<i64> {
    const SHAPE: &[u8; 24] = b"dddd-dd-ddTdd:dd:dd.dddZ";
    let bytes = text.as_bytes();
    if bytes.len() != SHAPE.len() {
        return None;
    }
    let shaped = SHAPE.iter().zip(bytes).all(|(want, got)| match want {
        b'd' => got.is_ascii_digit(),
        literal => got == literal,
    });
    if !shaped {
        return None;
    }
    let number = |from: usize, len: usize| -> i64 {
        bytes[from..from + len]
            .iter()
            .fold(0, |acc, b| acc * 10 + i64::from(b - b'0'))
    };
    let (year, month, day) = (number(0, 4), number(5, 2), number(8, 2));
    let (hour, minute, second, millis) =
        (number(11, 2), number(14, 2), number(17, 2), number(20, 3));
    let real_date = (1..=12).contains(&month) && (1..=days_in_month(year, month)).contains(&day);
    if !real_date || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let seconds = days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second;
    Some(seconds * 1_000 + millis)
}

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days from 1970-01-01 to the given proleptic Gregorian date (Howard Hinnant's `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let year_of_era = y.rem_euclid(400);
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}
