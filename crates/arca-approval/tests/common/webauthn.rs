//! A software WebAuthn authenticator for the passkey tests: a P-256 key from a fixed seed, never an owner's. It answers as a
//! browser's passkey would, and every field a test may need to bend can be set: the flags, the sign count, the origin, the
//! relying party, the ceremony type, the challenge, the credential and the signature's S half. No helper prints a proof, a
//! credential id or a key.
#![allow(dead_code)]

use arca_kernel::canon::canonical_json;
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use p256::ecdsa::signature::Signer as _;
use p256::ecdsa::{Signature, SigningKey};
use p256::elliptic_curve::PrimeField as _;
use p256::{FieldBytes, Scalar};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

pub const UP: u8 = 0x01;
pub const UV: u8 = 0x04;
pub const BE: u8 = 0x08;
pub const BS: u8 = 0x10;
pub const AT: u8 = 0x40;
pub const ED: u8 = 0x80;

/// The relying party and origin of every fixture line: documentation names only.
pub const RP: &str = "box1.example.com";
pub const ORIGIN: &str = "https://box1.example.com:8443";
/// The bytes of a P-256 SPKI DER before the point's coordinates.
pub const SPKI_HEADER: [u8; 27] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a,
    0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00, 0x04,
];

/// How an assertion's signature is encoded: as signed, forced to its high-S twin, or forced to its low-S twin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SForm {
    AsSigned,
    High,
    Low,
}

/// What an assertion holds. `Assert::over(namespace, statement)` is a valid one; a test bends the fields it needs.
#[derive(Debug, Clone)]
pub struct Assert {
    pub namespace: String,
    pub statement: Vec<u8>,
    /// Replaces the challenge derived from the namespace and statement.
    pub challenge: Option<[u8; 32]>,
    /// The challenge prefix the derived challenge uses.
    pub prefix: String,
    pub ceremony: String,
    /// Replaces the authenticator's origin.
    pub origin: Option<String>,
    /// `crossOrigin` as the browser writes it; `None` leaves the key out.
    pub cross_origin: Option<Value>,
    pub top_origin: Option<String>,
    /// The RP whose hash leads `authenticatorData`; `None` is the authenticator's.
    pub rp_id: Option<String>,
    pub flags: u8,
    pub sign_count: u32,
    /// Bytes after the 37 of `authenticatorData`.
    pub trailing: Vec<u8>,
    /// Replaces the credential id the proof names.
    pub credential_id: Option<Vec<u8>>,
    /// Replaces `clientDataJSON`'s bytes outright.
    pub client_data: Option<Vec<u8>>,
    pub s_form: SForm,
}

impl Assert {
    pub fn over(namespace: &str, statement: &[u8]) -> Self {
        Self {
            namespace: namespace.to_string(),
            statement: statement.to_vec(),
            challenge: None,
            prefix: "arca-kernel-approval/v1\n".to_string(),
            ceremony: "webauthn.get".to_string(),
            origin: None,
            cross_origin: Some(json!(false)),
            top_origin: None,
            rp_id: None,
            flags: UP | UV,
            sign_count: 0,
            trailing: Vec::new(),
            credential_id: None,
            client_data: None,
            s_form: SForm::AsSigned,
        }
    }
}

/// A software passkey: one credential for one relying party and origin.
pub struct SoftPasskey {
    pub signing: SigningKey,
    pub credential_id: Vec<u8>,
    pub rp_id: String,
    pub origin: String,
}

impl SoftPasskey {
    /// A passkey whose key and credential id come from `seed`, for the fixture relying party and origin.
    pub fn from_seed(seed: u8) -> Self {
        let signing =
            SigningKey::from_slice(&[seed; 32]).expect("a small seed is a valid P-256 scalar");
        Self {
            signing,
            credential_id: vec![seed ^ 0x5a; 20],
            rp_id: RP.to_string(),
            origin: ORIGIN.to_string(),
        }
    }

    /// The public key, SPKI DER (91 bytes).
    pub fn spki(&self) -> Vec<u8> {
        let point = self.signing.verifying_key().to_encoded_point(false);
        let mut out = SPKI_HEADER[..26].to_vec();
        out.extend_from_slice(point.as_bytes());
        out
    }

    /// SHA-256 (lowercase hex) of the SPKI.
    pub fn fingerprint(&self) -> String {
        Sha256::digest(self.spki())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    pub fn cred_text(&self) -> String {
        URL_SAFE_NO_PAD.encode(&self.credential_id)
    }

    pub fn key_text(&self) -> String {
        STANDARD.encode(self.spki())
    }

    /// The passkeys-file line for this passkey, listing `namespaces`.
    pub fn line(&self, namespaces: &str) -> String {
        format!(
            "owner namespaces=\"{namespaces}\" rp=\"{}\" origin=\"{}\" alg=\"ES256\" cred=\"{}\" key=\"{}\" test-passkey",
            self.rp_id,
            self.origin,
            self.cred_text(),
            self.key_text()
        )
    }

    /// The proof's JSON object for `a`, before it is written as bytes.
    pub fn proof_value(&self, a: &Assert) -> Value {
        let challenge = a
            .challenge
            .unwrap_or_else(|| challenge_with(&a.prefix, &a.namespace, &a.statement));
        let client = a
            .client_data
            .clone()
            .unwrap_or_else(|| self.client_data(&challenge, a));
        let rp = a.rp_id.as_deref().unwrap_or(&self.rp_id);
        let mut auth = Sha256::digest(rp.as_bytes()).to_vec();
        auth.push(a.flags);
        auth.extend_from_slice(&a.sign_count.to_be_bytes());
        auth.extend_from_slice(&a.trailing);
        let mut signed = auth.clone();
        signed.extend_from_slice(&Sha256::digest(&client));
        let signature: Signature = self.signing.sign(&signed);
        let signature = match a.s_form {
            SForm::AsSigned => signature,
            SForm::High => high_s(&signature),
            SForm::Low => signature.normalize_s().unwrap_or(signature),
        };
        json!({
            "v": 1,
            "kind": "webauthn",
            "alg": "ES256",
            "credentialId": URL_SAFE_NO_PAD.encode(a.credential_id.as_deref().unwrap_or(&self.credential_id)),
            "authenticatorData": URL_SAFE_NO_PAD.encode(&auth),
            "clientDataJSON": URL_SAFE_NO_PAD.encode(&client),
            "signature": URL_SAFE_NO_PAD.encode(der(&signature)),
        })
    }

    /// The proof's bytes for `a`: the canonical JSON of its object.
    pub fn assert(&self, a: &Assert) -> Vec<u8> {
        canonical_json(&self.proof_value(a))
            .expect("the proof canonicalizes")
            .into_bytes()
    }

    /// `clientDataJSON` as a browser writes it: type, challenge, origin, crossOrigin, then topOrigin.
    fn client_data(&self, challenge: &[u8; 32], a: &Assert) -> Vec<u8> {
        let text = |s: &str| serde_json::to_string(s).expect("a string serializes");
        let mut out = format!(
            "{{\"type\":{},\"challenge\":{},\"origin\":{}",
            text(&a.ceremony),
            text(&URL_SAFE_NO_PAD.encode(challenge)),
            text(a.origin.as_deref().unwrap_or(&self.origin)),
        );
        if let Some(cross) = &a.cross_origin {
            out.push_str(&format!(",\"crossOrigin\":{cross}"));
        }
        if let Some(top) = &a.top_origin {
            out.push_str(&format!(",\"topOrigin\":{}", text(top)));
        }
        out.push('}');
        out.into_bytes()
    }
}

/// `SHA-256(prefix + namespace + "\n" || statement)`.
pub fn challenge_with(prefix: &str, namespace: &str, statement: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(prefix.as_bytes());
    hash.update(namespace.as_bytes());
    hash.update(b"\n");
    hash.update(statement);
    hash.finalize().into()
}

/// The signature with `N - s` for its `s` when its `s` is low, so it is high-S. A high-S signature is returned as it is.
pub fn high_s(signature: &Signature) -> Signature {
    if signature.normalize_s().is_some() {
        return *signature;
    }
    let (r, s) = signature.split_bytes();
    let s = Option::<Scalar>::from(Scalar::from_repr(s)).expect("s is a scalar");
    let twin: FieldBytes = (-s).to_repr();
    Signature::from_scalars(r, twin).expect("the twin is a signature")
}

/// Whether a signature's `s` is in the high half.
pub fn is_high(signature: &Signature) -> bool {
    signature.normalize_s().is_some()
}

/// The DER of a signature: `SEQUENCE { INTEGER r, INTEGER s }`, each minimal and positive.
pub fn der(signature: &Signature) -> Vec<u8> {
    let (r, s) = signature.split_bytes();
    let mut body = integer(&r);
    body.extend(integer(&s));
    let mut out = vec![0x30, body.len() as u8];
    out.extend(body);
    out
}

fn integer(bytes: &[u8]) -> Vec<u8> {
    let start = bytes
        .iter()
        .position(|b| *b != 0)
        .unwrap_or(bytes.len() - 1);
    let mut digits = bytes[start..].to_vec();
    if digits[0] & 0x80 != 0 {
        digits.insert(0, 0);
    }
    let mut out = vec![0x02, digits.len() as u8];
    out.extend(digits);
    out
}

/// The approval string for a statement and a proof's bytes: `<base64 statement>.<base64 proof>`.
pub fn passkey_approval(statement: &str, proof: &[u8]) -> String {
    format!("{}.{}", STANDARD.encode(statement), STANDARD.encode(proof))
}
