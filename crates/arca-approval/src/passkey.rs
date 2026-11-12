//! The passkey verifier: a WebAuthn ES256 assertion over an approval statement's challenge, by a passkey that the root-owned
//! passkeys file lists for Arca's namespace, with a sign count that only grows. It reads no CBOR: an assertion's
//! `authenticatorData` is a fixed 37 bytes.
//!
//! The passkeys file `/etc/arca/owner-passkeys` is read under the signers file's trust rule. Each line is exactly
//!
//! ```text
//! owner namespaces="arca-kernel-approve" rp="<rp>" origin="https://<rp or a name under it>[:<port>]" alg="ES256" cred="<base64url>" key="<base64>"[ <comment>]
//! ```
//!
//! One origin that is not `https://` is a line's too: `http://localhost:<port>` with the rp `localhost`, the page of a node
//! on its own machine's loopback listener. A browser runs WebAuthn there because `localhost` is a secure context without
//! a certificate. No other `http://` origin is: not an IP address, which no browser takes as an rp, and not another name.
//!
//! with single spaces, a credential id of 16 to 1023 bytes in unpadded base64url, and a P-256 public key as 91 bytes of SPKI
//! DER in padded base64. A blank line or a `#` comment is skipped. Any other line refuses the whole file by its number, and no
//! refusal echoes a field. A line whose list does not name [`ARCA_NAMESPACE`] is read and checked, then not kept.
//!
//! A proof is droidd's: the canonical JSON of exactly `alg`, `authenticatorData`, `clientDataJSON`, `credentialId`, `kind`,
//! `signature` and `v`, with `v` 1, `kind` `webauthn`, `alg` `ES256`, and each buffer unpadded base64url. The checks are
//! droidd's, in droidd's order: the credential is on a line; `clientDataJSON` is a `webauthn.get` over this statement's
//! challenge, from the line's exact origin, with no `crossOrigin` and no `topOrigin`; `authenticatorData` is 37 bytes with the
//! line's rpIdHash, user presence and user verification set, no attested data or extensions, and a backup state only with
//! backup eligibility; and the ES256 signature over `authenticatorData || SHA-256(clientDataJSON)` verifies, in strict DER,
//! a high-S signature normalised to its low-S twin. The challenge's prefix keeps a droidd assertion from verifying here.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use arca_kernel::canon::canonical_json;
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use p256::ecdsa::signature::Verifier as _;
use p256::ecdsa::{Signature, VerifyingKey};
use serde_json::{Map, Value};
use sha2::{Digest as _, Sha256};

use crate::foreign::ForeignPasskeys;
use crate::nonces::open_records;
use crate::peer::PeerPasskeys;
use crate::signers::{TrustRoot, effective_uid, namespaces_ok, read_trusted, trust_for, untrusted};
use crate::statement::valid_hex;
use crate::verifier::SignatureVerifier;
use crate::{ARCA_NAMESPACE, ApprovalRefusal};

/// The owner's passkeys file in production. Root-owned, under the trust rule of the signers file.
pub const OWNER_PASSKEYS: &str = "/etc/arca/owner-passkeys";
/// The passkeys file Arca owns in its state directory, under the state-dir ownership rule.
pub const STATE_PASSKEYS_FILE: &str = "owner-passkeys";
/// The scratch-only passkeys injection, honoured under the signers injection's rules.
pub const TEST_PASSKEYS_ENV: &str = "ARCA_TEST_OWNER_PASSKEYS";
/// What a passkey's challenge hashes before the namespace.
pub const PASSKEY_CHALLENGE_PREFIX: &str = "arca-kernel-approval/v1\n";
/// The sign-count file's name in the state directory.
pub const PASSKEY_COUNTS_FILE: &str = "passkey-counts.jsonl";

/// The largest passkey proof, in bytes.
const PROOF_MAX_BYTES: usize = 8_192;
/// The keys a proof carries, in their sorted order.
const PROOF_KEYS: [&str; 7] = [
    "alg",
    "authenticatorData",
    "clientDataJSON",
    "credentialId",
    "kind",
    "signature",
    "v",
];
/// A credential id's length bounds, in bytes.
const CRED_MIN: usize = 16;
const CRED_MAX: usize = 1023;
/// The bytes of a P-256 public key's SPKI DER before its point's 64 coordinate bytes.
const P256_SPKI_HEADER: [u8; 27] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a,
    0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00, 0x04,
];
const P256_SPKI_LEN: usize = 91;
/// The SEC1 point inside the SPKI starts at its `04` byte.
const SPKI_POINT_AT: usize = 26;
/// `authenticatorData`: the rpIdHash (32), the flags (1) and the sign count (4).
const AUTH_DATA_LEN: usize = 37;
const FLAGS_AT: usize = 32;
const COUNT_AT: usize = 33;
const FLAG_UP: u8 = 0x01;
const FLAG_UV: u8 = 0x04;
const FLAG_BE: u8 = 0x08;
const FLAG_BS: u8 = 0x10;
const FLAG_AT: u8 = 0x40;
const FLAG_ED: u8 = 0x80;
/// The port a browser leaves out of an `https` origin, so no origin it writes names it.
const HTTPS_DEFAULT_PORT: u16 = 443;
/// The port a browser leaves out of an `http` origin, so no origin it writes names it.
const HTTP_DEFAULT_PORT: u16 = 80;
/// The rp of a page on its own machine's loopback: the one name a browser runs WebAuthn for over plain `http://`.
pub const LOCALHOST_RP: &str = "localhost";

/// The port of `origin` when it is exactly `http://localhost:<port>`, as a browser writes it: the port 1 to 65535, without
/// a leading zero and not 80, and nothing after it. `None` for every other origin: `https://`, `http://localhost` with no
/// port, an IP address (`http://127.0.0.1:<port>`), a name under `localhost`, and any other name. This is the only
/// `http://` origin a passkey of Arca's may answer from.
pub fn localhost_origin_port(origin: &str) -> Option<u16> {
    let port = origin.strip_prefix("http://localhost:")?;
    if port.starts_with('0') || !port.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    port.parse::<u16>()
        .ok()
        .filter(|port| *port != 0 && *port != HTTP_DEFAULT_PORT)
}

/// A line of the passkeys file that lists Arca's namespace.
#[derive(Clone, PartialEq, Eq)]
pub struct PasskeyLine {
    /// The line's number in the file, from 1.
    pub line: usize,
    /// The relying party the passkey was made for.
    pub rp_id: String,
    /// The one origin the passkey may answer from.
    pub origin: String,
    pub credential_id: Vec<u8>,
    /// The P-256 public key, SPKI DER (91 bytes).
    pub spki: Vec<u8>,
    /// The SHA-256 (lowercase hex) of `spki`.
    pub fingerprint: String,
}

/// A byte field as `Debug` shows it: its length, never its bytes.
struct Redacted(usize);

impl std::fmt::Debug for Redacted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<{} bytes>", self.0)
    }
}

/// The credential id and the key are not printed: only their lengths.
impl std::fmt::Debug for PasskeyLine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PasskeyLine")
            .field("line", &self.line)
            .field("rp_id", &self.rp_id)
            .field("origin", &self.origin)
            .field("credential_id", &Redacted(self.credential_id.len()))
            .field("spki", &Redacted(self.spki.len()))
            .finish_non_exhaustive()
    }
}

/// A passkey proof's decoded parts.
#[derive(Clone, PartialEq, Eq)]
pub struct PasskeyProof {
    pub credential_id: Vec<u8>,
    pub authenticator_data: Vec<u8>,
    pub client_data_json: Vec<u8>,
    pub signature: Vec<u8>,
}

/// No part of a proof is printed: only their lengths.
impl std::fmt::Debug for PasskeyProof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PasskeyProof")
            .field("credential_id", &Redacted(self.credential_id.len()))
            .field(
                "authenticator_data",
                &Redacted(self.authenticator_data.len()),
            )
            .field("client_data_json", &Redacted(self.client_data_json.len()))
            .field("signature", &Redacted(self.signature.len()))
            .finish()
    }
}

/// What a verified assertion names: the passkey's fingerprint, the count it reported, and whether it is backed up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asserted {
    pub fingerprint: String,
    pub sign_count: u32,
    pub backed_up: bool,
}

fn malformed(why: &str) -> ApprovalRefusal {
    ApprovalRefusal::Malformed(why.to_string())
}

/// The passkeys file to read and the trust it must carry, under the signers file's rules.
pub fn passkeys_trust(
    scratch: bool,
    projects: &[String],
    test: Option<PathBuf>,
) -> Result<(PathBuf, TrustRoot), ApprovalRefusal> {
    trust_for("passkeys", OWNER_PASSKEYS, scratch, projects, test)
}

/// The passkeys file's lines for Arca's namespace, read under `trust`. A file that is not there is no passkey.
pub fn load_passkeys(path: &Path, trust: &TrustRoot) -> Result<Vec<PasskeyLine>, ApprovalRefusal> {
    if let Err(error) = std::fs::symlink_metadata(path)
        && error.kind() == std::io::ErrorKind::NotFound
    {
        return Ok(Vec::new());
    }
    let bytes = read_trusted(path, trust, "passkeys")?;
    parse_passkeys(&bytes)
}

/// Loads the passkeys file Arca keeps in its own state directory. The file has no root anchor: it must be a regular file
/// owned by this process's uid with mode exactly 0600, not a link, and it lives in the 0700 state directory `serve` already
/// checks. A file that is not there is no passkey. A bad file is refused, so the caller can make the passkey verifier refuse
/// while the SSH path still verifies; the start itself is never refused.
pub fn load_state_passkeys(path: &Path) -> Result<Vec<PasskeyLine>, ApprovalRefusal> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(untrusted(format!(
                "the state passkeys file cannot be read ({})",
                error.kind()
            )));
        }
    };
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(untrusted(
            "the state passkeys file is a link or not a regular file".to_string(),
        ));
    }
    let uid = effective_uid()?;
    if meta.uid() != uid {
        return Err(untrusted(
            "the state passkeys file is not owned by the serve user".to_string(),
        ));
    }
    if meta.mode() & 0o777 != 0o600 {
        return Err(untrusted(
            "the state passkeys file does not have mode 0600".to_string(),
        ));
    }
    let bytes = std::fs::read(path)
        .map_err(|_| untrusted("the state passkeys file cannot be read".to_string()))?;
    parse_passkeys(&bytes)
}

/// The passkey lines of a passkeys file that list Arca's namespace, strictly (see the module comment). A control character
/// anywhere refuses, as does a non-ASCII byte outside a comment line. A blank line or a comment is skipped. Every other line
/// must be a passkey line of the exact shape, with a credential id and a key that decode as the shape says and repeat no
/// earlier line's, or the whole file refuses by the line's number.
pub fn parse_passkeys(bytes: &[u8]) -> Result<Vec<PasskeyLine>, ApprovalRefusal> {
    let mut lines: Vec<&[u8]> = bytes.split(|b| *b == b'\n').collect();
    if lines.last().is_some_and(|last| last.is_empty()) {
        lines.pop();
    }
    let mut read: Vec<(PasskeyLine, bool)> = Vec::new();
    for (index, raw) in lines.iter().enumerate() {
        let number = index + 1;
        let refuse = |why: &str| untrusted(format!("line {number} of the passkeys file {why}"));
        if raw.iter().any(|b| (*b < 0x20 && *b != b'\t') || *b == 0x7f) {
            return Err(refuse("carries a control character"));
        }
        let body = raw.trim_ascii_start();
        if body.is_empty() || body[0] == b'#' {
            continue;
        }
        if !raw.is_ascii() {
            return Err(refuse("carries a non-ASCII byte outside a comment"));
        }
        let text = std::str::from_utf8(raw).map_err(|_| refuse("is not text"))?;
        let Some(parts) = line_parts(text) else {
            return Err(refuse(
                "is not blank, a comment or a passkey line (owner namespaces=\"arca-kernel-approve\" rp=\"<rp>\" \
                 origin=\"https://<rp>[:<port>]\" alg=\"ES256\" cred=\"<base64url>\" key=\"<base64>\" [comment])",
            ));
        };
        if !origin_ok(parts.origin, parts.rp) {
            return Err(refuse(
                "names an origin that is not https:// and its rp or a name under it, with at most a port that is not 443, \
                 nor http://localhost:<port> with the rp localhost",
            ));
        }
        let credential_id = decode_exact(parts.cred, &URL_SAFE_NO_PAD)
            .filter(|id| (CRED_MIN..=CRED_MAX).contains(&id.len()))
            .ok_or_else(|| {
                refuse("holds a credential id that is not 16 to 1023 bytes of unpadded base64url")
            })?;
        let spki = decode_exact(parts.key, &STANDARD)
            .ok_or_else(|| refuse("holds a key that is not canonical base64"))?;
        if spki.len() != P256_SPKI_LEN || spki[..P256_SPKI_HEADER.len()] != P256_SPKI_HEADER {
            return Err(refuse(
                "holds a key that is not a 91-byte P-256 public key (SPKI DER)",
            ));
        }
        if p256_key(&spki).is_none() {
            return Err(refuse("holds a P-256 key that is not a point on the curve"));
        }
        let fingerprint = hex(&Sha256::digest(&spki));
        if let Some((twin, _)) = read.iter().find(|(seen, _)| {
            seen.credential_id == credential_id || seen.fingerprint == fingerprint
        }) {
            let what = if twin.credential_id == credential_id {
                "credential id"
            } else {
                "key"
            };
            return Err(refuse(&format!("repeats the {what} of line {}", twin.line)));
        }
        let arca = parts.namespaces.split(',').any(|ns| ns == ARCA_NAMESPACE);
        read.push((
            PasskeyLine {
                line: number,
                rp_id: parts.rp.to_string(),
                origin: parts.origin.to_string(),
                credential_id,
                spki,
                fingerprint,
            },
            arca,
        ));
    }
    Ok(read
        .into_iter()
        .filter_map(|(line, arca)| arca.then_some(line))
        .collect())
}

/// Formats one passkeys-file line for a registration: the same shape [`parse_passkeys`] accepts, with `rp` as the relying
/// party, `origin` as its exact page origin, `credential_id` as unpadded base64url and `public_key` as canonical base64 of a
/// 91-byte P-256 SPKI. It refuses anything the parser would refuse, before returning a line.
pub fn format_passkey_line(
    rp: &str,
    origin: &str,
    credential_id: &str,
    public_key: &str,
) -> Result<String, ApprovalRefusal> {
    if !dns_name_ok(rp) {
        return Err(malformed("the rp is not a lowercase DNS name"));
    }
    if !origin_ok(origin, rp) {
        return Err(malformed(
            "the origin is not https:// and its rp or a name under it, with at most a port that is not 443, nor \
             http://localhost:<port> with the rp localhost",
        ));
    }
    decode_exact(credential_id, &URL_SAFE_NO_PAD)
        .filter(|id| (CRED_MIN..=CRED_MAX).contains(&id.len()))
        .ok_or_else(|| {
            malformed("the credential id is not 16 to 1023 bytes of unpadded base64url")
        })?;
    let spki = decode_exact(public_key, &STANDARD)
        .ok_or_else(|| malformed("the public key is not canonical base64"))?;
    if spki.len() != P256_SPKI_LEN || spki[..P256_SPKI_HEADER.len()] != P256_SPKI_HEADER {
        return Err(malformed(
            "the public key is not a 91-byte P-256 public key (SPKI DER)",
        ));
    }
    if p256_key(&spki).is_none() {
        return Err(malformed(
            "the public key is not a P-256 point on the curve",
        ));
    }
    Ok(format!(
        "owner namespaces=\"{ARCA_NAMESPACE}\" rp=\"{rp}\" origin=\"{origin}\" alg=\"ES256\" cred=\"{credential_id}\" key=\"{public_key}\""
    ))
}

fn decode_exact(text: &str, engine: &impl Engine) -> Option<Vec<u8>> {
    let bytes = engine.decode(text).ok()?;
    (!bytes.is_empty() && engine.encode(&bytes) == text).then_some(bytes)
}

struct LineParts<'a> {
    namespaces: &'a str,
    rp: &'a str,
    origin: &'a str,
    cred: &'a str,
    key: &'a str,
}

/// `owner namespaces="<list>" rp="<rp>" origin="<origin>" alg="ES256" cred="<base64url>" key="<base64>"[ <comment>]` as its
/// parts, or `None` for any other shape. The list is a `namespaces=` list, the rp a lowercase DNS name, the origin holds no
/// quote (`origin_ok` checks the rest), the credential holds only base64url characters, the key only base64 characters with
/// at most two `=`, and the comment is printable ASCII.
fn line_parts(text: &str) -> Option<LineParts<'_>> {
    let rest = text.strip_prefix("owner namespaces=\"")?;
    let (namespaces, rest) = rest.split_once('"')?;
    let (rp, rest) = rest.strip_prefix(" rp=\"")?.split_once('"')?;
    let (origin, rest) = rest.strip_prefix(" origin=\"")?.split_once('"')?;
    let (cred, rest) = rest
        .strip_prefix(" alg=\"ES256\" cred=\"")?
        .split_once('"')?;
    let (key, rest) = rest.strip_prefix(" key=\"")?.split_once('"')?;
    let comment = match rest {
        "" => "",
        _ => rest.strip_prefix(' ')?,
    };
    let cred_ok = !cred.is_empty()
        && cred
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    let body = key.trim_end_matches('=');
    let key_ok = !body.is_empty()
        && key.len() - body.len() <= 2
        && body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/');
    let comment_ok = comment.bytes().all(|b| (0x20..=0x7e).contains(&b));
    (namespaces_ok(namespaces) && dns_name_ok(rp) && cred_ok && key_ok && comment_ok).then_some(
        LineParts {
            namespaces,
            rp,
            origin,
            cred,
            key,
        },
    )
}

/// Whether `name` is a lowercase DNS name: 1 to 253 bytes of dot-separated labels, each 1 to 63 lowercase letters, digits
/// and dashes, with no dash at either end.
fn dns_name_ok(name: &str) -> bool {
    (1..=253).contains(&name.len())
        && name.split('.').all(|label| {
            (1..=63).contains(&label.len())
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

/// Whether `origin` is an origin as a browser serializes one for a page under `rp`: `https://`, then `rp` or a name under
/// it, then at most `:<port>`, the port 1 to 65535 without a leading zero and not 443, which a browser leaves out. The one
/// other origin is `http://localhost:<port>` for the rp `localhost` itself (see [`localhost_origin_port`]): never a name
/// under it, never another rp, and never an IP address.
fn origin_ok(origin: &str, rp: &str) -> bool {
    if localhost_origin_port(origin).is_some() {
        return rp == LOCALHOST_RP;
    }
    let Some(authority) = origin.strip_prefix("https://") else {
        return false;
    };
    let (host, port) = match authority.split_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (authority, None),
    };
    let under_rp = host == rp
        || host
            .strip_suffix(rp)
            .is_some_and(|head| head.ends_with('.'));
    let port_ok = port.is_none_or(|port| {
        !port.starts_with('0')
            && port.bytes().all(|b| b.is_ascii_digit())
            && port
                .parse::<u16>()
                .is_ok_and(|p| p != 0 && p != HTTPS_DEFAULT_PORT)
    });
    dns_name_ok(host) && under_rp && port_ok
}

/// The challenge a passkey signs for `statement` in `namespace`:
/// `SHA-256(UTF-8(PASSKEY_CHALLENGE_PREFIX + namespace + "\n") || statement)`.
/// Arca's passkeys approve in [`ARCA_NAMESPACE`] only, so any other namespace is refused.
pub fn challenge_for(namespace: &str, statement: &[u8]) -> Result<[u8; 32], ApprovalRefusal> {
    if namespace != ARCA_NAMESPACE {
        return Err(ApprovalRefusal::WrongNamespace);
    }
    let mut hash = Sha256::new();
    hash.update(PASSKEY_CHALLENGE_PREFIX.as_bytes());
    hash.update(namespace.as_bytes());
    hash.update(b"\n");
    hash.update(statement);
    Ok(hash.finalize().into())
}

/// A passkey proof's bytes as its parts: at most 8192 bytes of UTF-8 that are their own canonical JSON (else
/// [`ApprovalRefusal::NotCanonical`]), an object of exactly the proof's seven keys, `v` 1, `kind` `webauthn`, `alg` `ES256`,
/// and each buffer unpadded base64url of at least one byte.
pub fn parse_proof(bytes: &[u8]) -> Result<PasskeyProof, ApprovalRefusal> {
    if bytes.is_empty() || bytes.len() > PROOF_MAX_BYTES {
        return Err(malformed(
            "the passkey proof is empty or longer than 8192 bytes",
        ));
    }
    let text =
        std::str::from_utf8(bytes).map_err(|_| malformed("the passkey proof is not UTF-8"))?;
    let value: Value =
        serde_json::from_str(text).map_err(|_| malformed("the passkey proof is not JSON"))?;
    let Value::Object(map) = &value else {
        return Err(malformed("the passkey proof is not a JSON object"));
    };
    if canonical_json(&value).ok().as_deref() != Some(text) {
        return Err(ApprovalRefusal::NotCanonical);
    }
    if map.len() != PROOF_KEYS.len() || !PROOF_KEYS.iter().all(|key| map.contains_key(*key)) {
        return Err(malformed(
            "the passkey proof is not exactly alg, authenticatorData, clientDataJSON, credentialId, kind, signature and v",
        ));
    }
    if map.get("v").and_then(Value::as_u64) != Some(1)
        || map.get("kind").and_then(Value::as_str) != Some("webauthn")
        || map.get("alg").and_then(Value::as_str) != Some("ES256")
    {
        return Err(malformed(
            "the passkey proof is not v 1, kind webauthn, alg ES256",
        ));
    }
    let part = |key: &str| -> Result<Vec<u8>, ApprovalRefusal> {
        map.get(key)
            .and_then(Value::as_str)
            .and_then(|text| decode_exact(text, &URL_SAFE_NO_PAD))
            .ok_or_else(|| {
                ApprovalRefusal::Malformed(format!(
                    "the passkey proof's {key} is not unpadded base64url"
                ))
            })
    };
    Ok(PasskeyProof {
        credential_id: part("credentialId")?,
        authenticator_data: part("authenticatorData")?,
        client_data_json: part("clientDataJSON")?,
        signature: part("signature")?,
    })
}

/// Verifies `proof` over `statement` in `namespace` against the passkeys file's `lines`, in droidd's order (see the module
/// comment), and names the passkey, the sign count it reported and whether it is backed up. A pure function: the count is
/// not checked against any store here. No refusal names a field the passkey sent.
pub fn verify_assertion(
    namespace: &str,
    statement: &[u8],
    proof: &PasskeyProof,
    lines: &[PasskeyLine],
) -> Result<Asserted, ApprovalRefusal> {
    let challenge = challenge_for(namespace, statement)?;
    if lines.is_empty() {
        return Err(ApprovalRefusal::NoVerifier);
    }
    let line = lines
        .iter()
        .find(|line| line.credential_id == proof.credential_id)
        .ok_or(ApprovalRefusal::BadSignature)?;
    check_client_data(&proof.client_data_json, &challenge, &line.origin)?;

    let data = &proof.authenticator_data;
    if data.len() < AUTH_DATA_LEN {
        return Err(malformed(
            "the passkey's authenticatorData is shorter than 37 bytes",
        ));
    }
    if data[..FLAGS_AT] != Sha256::digest(line.rp_id.as_bytes())[..] {
        return Err(ApprovalRefusal::WrongRp);
    }
    let flags = data[FLAGS_AT];
    if flags & FLAG_UP == 0 {
        return Err(ApprovalRefusal::NoUserPresence);
    }
    if flags & FLAG_UV == 0 {
        return Err(ApprovalRefusal::NoUserVerification);
    }
    if flags & FLAG_BS != 0 && flags & FLAG_BE == 0 {
        return Err(malformed(
            "the passkey reports a backup state without backup eligibility (BS without BE)",
        ));
    }
    if flags & FLAG_AT != 0 {
        return Err(malformed(
            "the passkey sent attested credential data in an assertion (AT set)",
        ));
    }
    if flags & FLAG_ED != 0 {
        return Err(malformed(
            "the passkey sent extension data Arca does not read (ED set)",
        ));
    }
    if data.len() != AUTH_DATA_LEN {
        return Err(malformed(
            "the passkey's authenticatorData has bytes after its 37",
        ));
    }

    let key = p256_key(&line.spki).ok_or(ApprovalRefusal::BadSignature)?;
    let signature = der_signature(&proof.signature).ok_or(ApprovalRefusal::BadSignature)?;
    let mut signed = data.clone();
    signed.extend_from_slice(&Sha256::digest(&proof.client_data_json));
    key.verify(&signed, &signature)
        .map_err(|_| ApprovalRefusal::BadSignature)?;
    let count = [
        data[COUNT_AT],
        data[COUNT_AT + 1],
        data[COUNT_AT + 2],
        data[COUNT_AT + 3],
    ];
    Ok(Asserted {
        fingerprint: line.fingerprint.clone(),
        sign_count: u32::from_be_bytes(count),
        backed_up: flags & FLAG_BS != 0,
    })
}

/// `clientDataJSON` checked for an assertion: a JSON object whose `type` is `webauthn.get`, whose `challenge` is the unpadded
/// base64url of `challenge`, whose `origin` is exactly `origin`, with `crossOrigin` absent or `false` and no `topOrigin`.
/// Its other keys are the browser's.
fn check_client_data(
    bytes: &[u8],
    challenge: &[u8; 32],
    origin: &str,
) -> Result<(), ApprovalRefusal> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| malformed("the passkey's clientDataJSON is not UTF-8"))?;
    let value: Map<String, Value> = serde_json::from_str(text)
        .map_err(|_| malformed("the passkey's clientDataJSON is not a JSON object"))?;
    if value.get("type").and_then(Value::as_str) != Some("webauthn.get") {
        return Err(malformed(
            "the passkey answered another ceremony than webauthn.get",
        ));
    }
    if value.get("challenge").and_then(Value::as_str) != Some(&URL_SAFE_NO_PAD.encode(challenge)) {
        return Err(ApprovalRefusal::BadSignature);
    }
    if value.get("origin").and_then(Value::as_str) != Some(origin) {
        return Err(ApprovalRefusal::WrongOrigin);
    }
    if value
        .get("crossOrigin")
        .is_some_and(|cross| cross != &Value::Bool(false))
        || value.contains_key("topOrigin")
    {
        return Err(ApprovalRefusal::WrongOrigin);
    }
    Ok(())
}

/// The P-256 key an SPKI holds, when it is the 91-byte SPKI of a point on the curve.
fn p256_key(spki: &[u8]) -> Option<VerifyingKey> {
    if spki.len() != P256_SPKI_LEN || spki[..P256_SPKI_HEADER.len()] != P256_SPKI_HEADER {
        return None;
    }
    VerifyingKey::from_sec1_bytes(&spki[SPKI_POINT_AT..]).ok()
}

/// An ECDSA P-256 signature in strict DER: `SEQUENCE { INTEGER r, INTEGER s }`, each a minimal positive integer of at most
/// 33 bytes, every length one byte, and nothing after the sequence. A high-S signature is normalised to its low-S twin.
fn der_signature(der: &[u8]) -> Option<Signature> {
    let body = der.strip_prefix(&[0x30])?;
    let (&length, body) = body.split_first()?;
    if length >= 0x80 || usize::from(length) != body.len() {
        return None;
    }
    let (r, body) = der_integer(body)?;
    let (s, rest) = der_integer(body)?;
    if !rest.is_empty() {
        return None;
    }
    let signature = Signature::from_scalars(r, s).ok()?;
    Some(signature.normalize_s().unwrap_or(signature))
}

/// One DER INTEGER at the head of `bytes`, as 32 big-endian bytes, and what follows it.
fn der_integer(bytes: &[u8]) -> Option<([u8; 32], &[u8])> {
    let bytes = bytes.strip_prefix(&[0x02])?;
    let (&length, bytes) = bytes.split_first()?;
    let length = usize::from(length);
    if length == 0 || length > 33 || bytes.len() < length {
        return None;
    }
    let (value, rest) = bytes.split_at(length);
    // Minimal and positive: no sign bit set, and a leading zero only before a byte whose top bit is set.
    if value[0] & 0x80 != 0 || (value[0] == 0 && (value.len() == 1 || value[1] & 0x80 == 0)) {
        return None;
    }
    let digits = if value[0] == 0 { &value[1..] } else { value };
    if digits.len() > 32 {
        return None;
    }
    let mut out = [0u8; 32];
    out[32 - digits.len()..].copy_from_slice(digits);
    Some((out, rest))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The last sign count kept for each passkey, in a file that survives a restart.
///
/// A record is `{"count":<n>,"fingerprint":"<64 hex>"}` and a newline, written and fsynced before [`FileSignCounts::advance`]
/// returns. Each record of a passkey holds a count above its last one, and no record holds 0. A record without its newline
/// was never acknowledged, so opening the file drops it and its count is not kept. The file is locked for the life of the
/// store, so two kernels cannot keep counts in one file.
pub struct FileSignCounts {
    file: File,
    counts: BTreeMap<String, u32>,
    /// Set once a write or fsync fails. Such a store advances nothing more, because the file's state is then unknown.
    failed: bool,
}

impl FileSignCounts {
    /// Opens the sign-count file at `path`, creating it mode 0600 if it does not exist.
    ///
    /// Refuses a symbolic link, a file that is not a regular file owned by this uid with mode exactly 0600, or a file another
    /// open store holds. Refuses a complete record that is not a sign-count record, or one that does not advance its passkey's
    /// count. The directory that holds the file is not checked. The caller keeps the state directory at mode 0700.
    pub fn open(path: &Path) -> Result<Self, ApprovalRefusal> {
        let (file, body) = open_records(path, "sign-count file")?;
        let mut counts = BTreeMap::new();
        if let Some(records) = body.strip_suffix(b"\n") {
            for (index, line) in records.split(|b| *b == b'\n').enumerate() {
                let number = index + 1;
                let (fingerprint, count) = parse_count_record(line).ok_or_else(|| {
                    ApprovalRefusal::Storage(format!("line {number} is not a sign-count record"))
                })?;
                if counts.get(&fingerprint).is_some_and(|last| count <= *last) {
                    return Err(ApprovalRefusal::Storage(format!(
                        "line {number} does not advance its passkey's sign count"
                    )));
                }
                counts.insert(fingerprint, count);
            }
        }
        Ok(Self {
            file,
            counts,
            failed: false,
        })
    }

    /// Checks `count` against the last count kept for `fingerprint` (64 lowercase hex) and keeps it. A count above 0 must be
    /// above the kept one. A count of 0 is accepted only while the kept one is 0 or there is none, and then nothing is
    /// written, as a synced passkey always reports 0. Else [`ApprovalRefusal::SignCountRegressed`]. A count is kept only once
    /// its record is written and fsynced. A failed write or fsync leaves the store failed.
    pub fn advance(&mut self, fingerprint: &str, count: u32) -> Result<(), ApprovalRefusal> {
        self.check(fingerprint, count)?;
        if count == 0 {
            return Ok(());
        }
        let record = format!("{}\n", count_record(fingerprint, count));
        let durable = self
            .file
            .write_all(record.as_bytes())
            .and_then(|()| self.file.sync_all());
        if let Err(e) = durable {
            self.failed = true;
            return Err(ApprovalRefusal::Storage(format!(
                "the sign count cannot be written and synced ({})",
                e.kind()
            )));
        }
        self.counts.insert(fingerprint.to_string(), count);
        Ok(())
    }

    /// The refusals of [`FileSignCounts::advance`] for `count`, with nothing written or kept.
    pub fn check(&self, fingerprint: &str, count: u32) -> Result<(), ApprovalRefusal> {
        if !valid_hex(fingerprint, 64, 64) {
            return Err(malformed(
                "the passkey's fingerprint is not 64 lowercase hex digits",
            ));
        }
        if self.failed {
            return Err(ApprovalRefusal::Storage(
                "an earlier write failed, so this store advances nothing more; restart the kernel"
                    .to_string(),
            ));
        }
        let last = self.counts.get(fingerprint).copied().unwrap_or(0);
        if (count > 0 && count <= last) || (count == 0 && last != 0) {
            return Err(ApprovalRefusal::SignCountRegressed);
        }
        Ok(())
    }
}

/// A sign-count record's text without its newline, with keys in sorted order.
fn count_record(fingerprint: &str, count: u32) -> String {
    format!("{{\"count\":{count},\"fingerprint\":\"{fingerprint}\"}}")
}

/// The fingerprint and count of a complete record, if the line is exactly a sign-count record as this module writes one.
fn parse_count_record(line: &[u8]) -> Option<(String, u32)> {
    let value: Value = serde_json::from_slice(line).ok()?;
    let count = u32::try_from(value.get("count")?.as_u64()?).ok()?;
    let fingerprint = value.get("fingerprint")?.as_str()?;
    let exact = count > 0
        && valid_hex(fingerprint, 64, 64)
        && count_record(fingerprint, count).as_bytes() == line;
    exact.then(|| (fingerprint.to_string(), count))
}

/// Verifies passkey proofs against the passkeys file's lines, and advances each passkey's sign count as it verifies.
///
/// The count is checked and advanced inside [`SignatureVerifier::verify`], before `ApprovalVerifier::check` reads the
/// statement's nonce. So a replay of a counting passkey's approval is refused as [`ApprovalRefusal::SignCountRegressed`],
/// and a replay of a synced passkey's (count 0) as [`ApprovalRefusal::Replayed`].
///
/// The count is advanced before `check` compares the statement's host, action, target and arguments with the call and checks
/// its time window. So a counting passkey's approval that one of those checks refuses is used up: sent again, even for the
/// call it names, it is refused as [`ApprovalRefusal::SignCountRegressed`], and the owner signs a new statement. Its nonce is
/// not spent, and the passkey's next assertion carries a higher count, so later approvals are not affected. A synced
/// passkey's approval (count 0) is not used up.
pub struct PasskeyVerifier {
    keys: Keys,
    /// Other kernels' passkeys files this one also trusts for approvals, read at each check. Empty by default.
    foreign: ForeignPasskeys,
    /// Other machines' page hosts' passkeys kept in sync here, one value per source in the config's order, trusted for
    /// approvals and read at each check. None by default.
    peers: Vec<PeerPasskeys>,
}

enum Keys {
    Kept {
        lines: Vec<PasskeyLine>,
        counts: Mutex<FileSignCounts>,
    },
    Refused(ApprovalRefusal),
}

impl PasskeyVerifier {
    /// A verifier that trusts `lines`, keeping their sign counts in `counts`.
    pub fn new(lines: Vec<PasskeyLine>, counts: FileSignCounts) -> Self {
        Self {
            keys: Keys::Kept {
                lines,
                counts: Mutex::new(counts),
            },
            foreign: ForeignPasskeys::default(),
            peers: Vec::new(),
        }
    }

    /// Also trusts, for approvals, the passkeys `foreign` reads from other kernels' files. The sign-in route and the passkey
    /// list never see them, and a verifier whose own file was refused still refuses every proof.
    pub fn with_foreign(mut self, foreign: ForeignPasskeys) -> Self {
        self.foreign = foreign;
        self
    }

    /// The other kernels' passkeys files this verifier trusts.
    pub fn foreign(&self) -> &ForeignPasskeys {
        &self.foreign
    }

    /// Also trusts, for approvals, the passkeys another machine's page host keeps in sync in `peer`. Like the foreign files,
    /// the sign-in route and the passkey list never see them. A refusing verifier keeps the value (so a sync can still be
    /// answered) and still refuses every proof. Called once per source, in the config's order: the first call names the
    /// first source.
    pub fn with_peer(mut self, peer: PeerPasskeys) -> Self {
        self.peers.push(peer);
        self
    }

    /// The first source's peer passkeys, when the config keeps any.
    pub fn peer(&self) -> Option<&PeerPasskeys> {
        self.peers.first()
    }

    /// Every source's peer passkeys, in the config's order.
    pub fn peers(&self) -> &[PeerPasskeys] {
        &self.peers
    }

    /// A verifier for a passkeys file that could not be trusted or read: every passkey proof is refused with `refusal`, which
    /// names the file, the rule and at most a line number. The SSH path beside it is unaffected.
    pub fn refusing(refusal: ApprovalRefusal) -> Self {
        Self {
            keys: Keys::Refused(refusal),
            foreign: ForeignPasskeys::default(),
            peers: Vec::new(),
        }
    }

    /// The number of passkeys this verifier trusts.
    pub fn key_count(&self) -> usize {
        match &self.keys {
            Keys::Kept { lines, .. } => lines.len(),
            Keys::Refused(_) => 0,
        }
    }

    /// Why this verifier refuses every passkey proof, when its passkeys file could not be trusted or read.
    pub fn refusal(&self) -> Option<&ApprovalRefusal> {
        match &self.keys {
            Keys::Kept { .. } => None,
            Keys::Refused(refusal) => Some(refusal),
        }
    }

    /// The passkey lines this verifier trusts. A refusing verifier trusts none.
    pub fn lines(&self) -> &[PasskeyLine] {
        match &self.keys {
            Keys::Kept { lines, .. } => lines,
            Keys::Refused(_) => &[],
        }
    }

    /// Replaces the trusted lines while keeping the sign-count store. An enrolment uses this, so a count already kept for a
    /// line that stays trusted is not lost. A verifier that refused its file stays refusing; the caller cannot recover it
    /// without a restart.
    pub fn replace_lines(&mut self, lines: Vec<PasskeyLine>) {
        if let Keys::Kept {
            lines: kept,
            counts: _,
        } = &mut self.keys
        {
            *kept = lines;
        }
    }
}

impl PasskeyVerifier {
    /// Verifies a proof against this kernel's own passkeys only: the sign-in route's check.
    pub fn verify_own(
        &self,
        namespace: &str,
        statement: &[u8],
        signature: &[u8],
    ) -> Result<String, ApprovalRefusal> {
        self.verify_with(namespace, statement, signature, false, true)
    }

    /// Refuses in this order: a passkeys file that could not be trusted; a namespace other than Arca's; a proof that is not
    /// one strict proof; every check of [`verify_assertion`]; a sign count that does not advance, or a count that cannot be
    /// made durable. With `foreign`, the lines of the trusted foreign files and the peer files are read now and added after the
    /// kernel's own; such a line never replaces an own line, or an earlier trusted one, with the same credential or key.
    ///
    /// Without `advance`, the count is checked as `advance` would check it and nothing is written: [`SignatureVerifier::peek`].
    fn verify_with(
        &self,
        namespace: &str,
        statement: &[u8],
        signature: &[u8],
        foreign: bool,
        advance: bool,
    ) -> Result<String, ApprovalRefusal> {
        let (lines, counts) = match &self.keys {
            Keys::Kept { lines, counts } => (lines, counts),
            Keys::Refused(refusal) => return Err(refusal.clone()),
        };
        if namespace != ARCA_NAMESPACE {
            return Err(ApprovalRefusal::WrongNamespace);
        }
        let proof = parse_proof(signature)?;
        let asserted = if foreign && (!self.foreign.is_empty() || !self.peers.is_empty()) {
            let mut all = lines.clone();
            let trusted = self
                .foreign
                .load()
                .0
                .into_iter()
                .chain(self.peers.iter().flat_map(|peer| peer.load().0.into_iter()));
            for line in trusted {
                if !all.iter().any(|own| {
                    own.credential_id == line.credential_id || own.fingerprint == line.fingerprint
                }) {
                    all.push(line);
                }
            }
            verify_assertion(namespace, statement, &proof, &all)?
        } else {
            verify_assertion(namespace, statement, &proof, lines)?
        };
        let mut counts = counts.lock().map_err(|_| {
            ApprovalRefusal::Storage("the sign-count store is poisoned".to_string())
        })?;
        if advance {
            counts.advance(&asserted.fingerprint, asserted.sign_count)?;
        } else {
            counts.check(&asserted.fingerprint, asserted.sign_count)?;
        }
        Ok(format!("passkey:{}", asserted.fingerprint))
    }
}

impl SignatureVerifier for PasskeyVerifier {
    /// The approval check: this kernel's own passkeys, then the trusted foreign files' (see [`PasskeyVerifier::with_foreign`]).
    /// Returns `passkey:<fingerprint>` on a pass.
    fn verify(
        &self,
        namespace: &str,
        statement: &[u8],
        signature: &[u8],
    ) -> Result<String, ApprovalRefusal> {
        self.verify_with(namespace, statement, signature, true, true)
    }

    /// The approval check of `verify`, over the same lines, with the sign count checked and not advanced.
    fn peek(
        &self,
        namespace: &str,
        statement: &[u8],
        signature: &[u8],
    ) -> Result<String, ApprovalRefusal> {
        self.verify_with(namespace, statement, signature, true, false)
    }
}
