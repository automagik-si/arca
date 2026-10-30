//! Desired state: entries, documents, signed documents and the acceptance gate.
//!
//! A document is a full entry tree, never a diff. It carries the node it is for, a strictly increasing `seq`, the digest of
//! the document it follows, and its validity window. The gate accepts a signed document only when it is for this node, is
//! signed by this node's key, and continues the applied chain. Anything else is refused and the applied state does not
//! move. An expired or missing document holds the last applied state; the kernel never reconciles toward empty.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::canon::{Digest, canonical_json};
use crate::identity::{NodeId, NodeKey, verify};

/// The context prepended to a document's canonical bytes before signing, so a node-key signature over any other Arca
/// message cannot be replayed as a document.
pub const DOCUMENT_CONTEXT: &[u8] = b"arca.desired-state.v1\n";

/// An entry's id: `^[a-z][a-z0-9-]{0,62}$`. Deserialization refuses anything else.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct EntryId(pub String);

/// A plugin's id: `^[a-z][a-z0-9.-]{0,62}$`. Deserialization refuses anything else.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PluginId(pub String);

impl EntryId {
    /// The id, when `s` matches `^[a-z][a-z0-9-]{0,62}$`.
    pub fn parse(s: &str) -> Option<EntryId> {
        valid_name(s, |c| c == b'-').then(|| EntryId(s.to_string()))
    }
}

impl PluginId {
    /// The id, when `s` matches `^[a-z][a-z0-9.-]{0,62}$`.
    pub fn parse(s: &str) -> Option<PluginId> {
        valid_name(s, |c| c == b'-' || c == b'.').then(|| PluginId(s.to_string()))
    }
}

fn valid_name(s: &str, extra: impl Fn(u8) -> bool) -> bool {
    let bytes = s.as_bytes();
    let [first, rest @ ..] = bytes else {
        return false;
    };
    bytes.len() <= 63
        && first.is_ascii_lowercase()
        && rest
            .iter()
            .all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || extra(c))
}

impl TryFrom<String> for EntryId {
    type Error = String;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        EntryId::parse(&s)
            .ok_or_else(|| "an entry id must match ^[a-z][a-z0-9-]{0,62}$".to_string())
    }
}

impl From<EntryId> for String {
    fn from(id: EntryId) -> String {
        id.0
    }
}

impl TryFrom<String> for PluginId {
    type Error = String;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        PluginId::parse(&s)
            .ok_or_else(|| "a plugin id must match ^[a-z][a-z0-9.-]{0,62}$".to_string())
    }
}

impl From<PluginId> for String {
    fn from(id: PluginId) -> String {
        id.0
    }
}

/// One desired object: which plugin owns it, at which version, with what config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: EntryId,
    pub plugin: PluginId,
    pub version: String,
    pub digest: Digest,
    pub signer: String,
    pub config: Value,
    pub disabled: bool,
}

/// A full desired-state document for one node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Document {
    pub node: NodeId,
    pub seq: u64,
    pub prev: Option<Digest>,
    pub issued_at: String,
    pub expires_at: String,
    pub entries: Vec<Entry>,
}

impl Document {
    /// The canonical JSON of the document, as bytes. Every object's keys are sorted, so the bytes depend only on content.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        // A `Value` cannot hold a non-finite number, and a document is serializable, so neither step can fail.
        let value = serde_json::to_value(self).expect("a document serializes to JSON");
        canonical_json(&value)
            .expect("a JSON value holds no non-finite number")
            .into_bytes()
    }

    /// The SHA-256 of the canonical bytes.
    pub fn digest(&self) -> Digest {
        Digest::of(&self.canonical_bytes())
    }
}

/// A document with the node key that signed it. The signature covers [`DOCUMENT_CONTEXT`] followed by the canonical bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedDocument {
    pub document: Document,
    #[serde(with = "hex_array")]
    pub signer: [u8; 32],
    #[serde(with = "hex_array")]
    pub signature: [u8; 64],
}

/// Signs `doc` with the node's own key.
pub fn sign_document(doc: Document, key: &NodeKey) -> SignedDocument {
    let signature = key.sign(&signed_message(&doc));
    SignedDocument {
        document: doc,
        signer: key.public(),
        signature,
    }
}

fn signed_message(doc: &Document) -> Vec<u8> {
    let mut msg = DOCUMENT_CONTEXT.to_vec();
    msg.extend_from_slice(&doc.canonical_bytes());
    msg
}

/// The document that the gate has applied: its sequence number and digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub seq: u64,
    pub digest: Digest,
}

/// Why the gate refused a document. No variant carries file contents; `Malformed` carries a fixed reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocRefusal {
    BadSignature,
    UnknownSigner,
    ForeignNode,
    Stale { applied: u64, got: u64 },
    EqualSeqOtherDigest { seq: u64 },
    Forked { expected_prev: Digest },
    DuplicateEntry(EntryId),
    Malformed(String),
}

impl fmt::Display for DocRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadSignature => f.write_str("the signature does not verify"),
            Self::UnknownSigner => f.write_str("the document is not signed by this node's key"),
            Self::ForeignNode => f.write_str("the document is for another node"),
            Self::Stale { applied, got } => {
                write!(f, "seq {got} is below the applied seq {applied}")
            }
            Self::EqualSeqOtherDigest { seq } => {
                write!(f, "seq {seq} is applied with another digest")
            }
            Self::Forked { .. } => f.write_str("prev does not name the applied document"),
            Self::DuplicateEntry(id) => write!(f, "entry {} appears twice", id.0),
            Self::Malformed(why) => write!(f, "malformed: {why}"),
        }
    }
}

/// What the node should do with the desired state it has been offered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Reconcile toward this document, which is now the applied state.
    Apply(Applied),
    /// Keep the last applied state. `stale` is true when the document is expired or missing.
    Hold { last: Option<Applied>, stale: bool },
}

/// The acceptance gate for one node's desired state.
#[derive(Debug, Clone)]
pub struct DesiredStateGate {
    node: NodeId,
    signer: [u8; 32],
    applied: Option<Applied>,
}

impl DesiredStateGate {
    /// A gate for `node`, which accepts documents signed by `signer` and continues from `applied`.
    pub fn new(node: NodeId, signer: [u8; 32], applied: Option<Applied>) -> Self {
        Self {
            node,
            signer,
            applied,
        }
    }

    /// The applied state, if any document has been applied.
    pub fn applied(&self) -> Option<&Applied> {
        self.applied.as_ref()
    }

    /// Judges a signed document at `now_ms` (milliseconds since the Unix epoch). A refusal leaves the applied state unchanged.
    ///
    /// Order: the signer is pinned, the signature must verify, the document must be for this node, its content must be
    /// well formed, and only then are the seq and the chain compared. An expired document holds the applied state.
    pub fn accept(&mut self, signed: &SignedDocument, now_ms: u64) -> Result<Target, DocRefusal> {
        if signed.signer != self.signer {
            return Err(DocRefusal::UnknownSigner);
        }
        if !verify(
            &self.signer,
            &signed_message(&signed.document),
            &signed.signature,
        ) {
            return Err(DocRefusal::BadSignature);
        }
        let doc = &signed.document;
        if doc.node != self.node {
            return Err(DocRefusal::ForeignNode);
        }
        let expires_ms = check_content(doc)?;
        let digest = doc.digest();
        match &self.applied {
            None => {
                if doc.prev.is_some() {
                    return Err(DocRefusal::Malformed(
                        "the first document names no previous one".to_string(),
                    ));
                }
            }
            Some(applied) => {
                if doc.seq < applied.seq {
                    return Err(DocRefusal::Stale {
                        applied: applied.seq,
                        got: doc.seq,
                    });
                }
                if doc.seq == applied.seq {
                    // The applied document offered again is idempotent; another document at the same seq is a rival.
                    if digest != applied.digest {
                        return Err(DocRefusal::EqualSeqOtherDigest { seq: doc.seq });
                    }
                } else if doc.prev.as_ref() != Some(&applied.digest) {
                    return Err(DocRefusal::Forked {
                        expected_prev: applied.digest.clone(),
                    });
                }
            }
        }
        if now_ms >= expires_ms {
            return Ok(Target::Hold {
                last: self.applied.clone(),
                stale: true,
            });
        }
        let next = Applied {
            seq: doc.seq,
            digest,
        };
        self.applied = Some(next.clone());
        Ok(Target::Apply(next))
    }

    /// The answer when no document is available: hold the last applied state, reported as stale. Never an empty apply.
    pub fn missing(&self) -> Target {
        Target::Hold {
            last: self.applied.clone(),
            stale: true,
        }
    }
}

/// An instant in RFC 3339 UTC form (`YYYY-MM-DDTHH:MM:SSZ`, with an optional fraction of one to nine digits) as
/// milliseconds since the Unix epoch. Instants before 1970 and any other form give `None`. Digits are read as bytes, so no
/// input can split a character.
pub fn instant_ms(text: &str) -> Option<u64> {
    let body = text.strip_suffix('Z')?;
    let (whole, frac) = match body.split_once('.') {
        Some((whole, frac)) => (whole, Some(frac)),
        None => (body, None),
    };
    let b = whole.as_bytes();
    if b.len() != 19 || [b[4], b[7]] != *b"--" || b[10] != b'T' || [b[13], b[16]] != *b"::" {
        return None;
    }
    let num = |range: std::ops::Range<usize>| -> Option<u64> {
        b.get(range)?.iter().try_fold(0u64, |acc, &d| {
            d.is_ascii_digit().then(|| acc * 10 + u64::from(d - b'0'))
        })
    };
    let year = num(0..4)?;
    let month = num(5..7)?;
    let day = num(8..10)?;
    let hour = num(11..13)?;
    let minute = num(14..16)?;
    let second = num(17..19)?;
    if year < 1970
        || !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let millis = match frac {
        None => 0,
        Some(f) => {
            if f.is_empty() || f.len() > 9 || !f.bytes().all(|c| c.is_ascii_digit()) {
                return None;
            }
            let three: String = f.chars().chain(std::iter::repeat('0')).take(3).collect();
            three.parse::<u64>().ok()?
        }
    };
    let seconds = days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second;
    seconds.checked_mul(1_000)?.checked_add(millis)
}

fn days_in_month(year: u64, month: u64) -> u64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) => {
            29
        }
        _ => 28,
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date, for a year of at least 1970 (Howard Hinnant's civil-day count).
fn days_from_civil(year: u64, month: u64, day: u64) -> u64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The content checks that do not depend on the applied state. Returns the expiry in milliseconds.
fn check_content(doc: &Document) -> Result<u64, DocRefusal> {
    if doc.seq == 0 {
        return Err(DocRefusal::Malformed("seq starts at 1".to_string()));
    }
    let mut seen = BTreeSet::new();
    for entry in &doc.entries {
        if EntryId::parse(&entry.id.0).is_none() {
            return Err(DocRefusal::Malformed(
                "an entry id is outside the name rule".to_string(),
            ));
        }
        if PluginId::parse(&entry.plugin.0).is_none() {
            return Err(DocRefusal::Malformed(
                "a plugin id is outside the name rule".to_string(),
            ));
        }
        if !seen.insert(entry.id.0.as_str()) {
            return Err(DocRefusal::DuplicateEntry(entry.id.clone()));
        }
        if !entry.config.is_object() {
            return Err(DocRefusal::Malformed(
                "an entry's config is not an object".to_string(),
            ));
        }
    }
    let issued = instant_ms(&doc.issued_at).ok_or_else(|| {
        DocRefusal::Malformed("issued_at is not an RFC 3339 UTC instant".to_string())
    })?;
    let expires = instant_ms(&doc.expires_at).ok_or_else(|| {
        DocRefusal::Malformed("expires_at is not an RFC 3339 UTC instant".to_string())
    })?;
    if expires <= issued {
        return Err(DocRefusal::Malformed(
            "expires_at is not after issued_at".to_string(),
        ));
    }
    Ok(expires)
}

mod hex_array {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer, const N: usize>(
        bytes: &[u8; N],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let text: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        serializer.serialize_str(&text)
    }

    /// Lowercase hex of exactly `N` bytes. Uppercase is refused, as it is for a digest.
    pub fn deserialize<'de, D: Deserializer<'de>, const N: usize>(
        deserializer: D,
    ) -> Result<[u8; N], D::Error> {
        let text = String::deserialize(deserializer)?;
        let bad = || serde::de::Error::custom("expected lowercase hex of the right length");
        let nibble = |c: u8| match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            _ => None,
        };
        if text.len() != 2 * N {
            return Err(bad());
        }
        let mut out = [0u8; N];
        for (slot, pair) in out.iter_mut().zip(text.as_bytes().chunks_exact(2)) {
            let hi = nibble(pair[0]).ok_or_else(bad)?;
            let lo = nibble(pair[1]).ok_or_else(bad)?;
            *slot = (hi << 4) | lo;
        }
        Ok(out)
    }
}
