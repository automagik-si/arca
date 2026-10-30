//! Shared test helpers. Every key here is a software key from a fixed seed, and every file lives in a scratch directory
//! the test owns. No hardware key is used, and no owner's signers file is read or written. No helper prints a signature
//! or an approval string.
#![allow(dead_code)]

use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use arca_approval::SignerLine;
use arca_kernel::canon::{canonical_json, params_digest};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{Value, json};
use sha2::{Digest, Sha256, Sha512};
use ssh_key::{HashAlg, PublicKey};

pub const SK_TYPE: &str = "sk-ssh-ed25519@openssh.com";
pub const ED_TYPE: &str = "ssh-ed25519";
/// The FIDO application string droidd's test keys use.
pub const APP: &str = "ssh:";
pub const HOST: &str = "node1";
/// 2026-10-30T12:00:00.000Z in epoch milliseconds.
pub const NOW: u64 = 1_793_361_600_000;
pub const NONCE: &str = "0123456789abcdef0123456789abcdef";
/// A plan's digest, the target and argument the approvals below name.
pub const PLAN: &str = "e39aa63ab2e4396ebd91016fbdad78d25c7ce79fac77aa248a494d93a52ffb10";

/// A scratch directory under the system temporary directory, mode 0700, removed on drop.
pub struct Scratch {
    pub dir: PathBuf,
}

impl Scratch {
    pub fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("the clock is after the epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "arca-approval-{tag}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir(&dir).expect("the scratch directory is created");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("mode 0700");
        Self { dir }
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700));
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// The uid that owns `dir`, which a test created itself.
pub fn owner_uid(dir: &Path) -> u32 {
    fs::metadata(dir).expect("the directory is readable").uid()
}

/// Writes `contents` at `path` with `mode`.
pub fn write_mode(path: &Path, contents: &[u8], mode: u32) {
    let mut file = fs::File::create(path).expect("the file is created");
    file.write_all(contents).expect("the file is written");
    file.sync_all().expect("the file is synced");
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("the mode is set");
}

fn put(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
}

/// A software `sk-ssh-ed25519@openssh.com` key from a fixed seed.
pub struct SkKey {
    pub signing: SigningKey,
}

impl SkKey {
    pub fn from_seed(seed: u8) -> Self {
        Self {
            signing: SigningKey::from_bytes(&[seed; 32]),
        }
    }

    /// The SSH public key blob: the type, the ed25519 key, and the application.
    pub fn blob(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put(&mut out, SK_TYPE.as_bytes());
        put(&mut out, &self.signing.verifying_key().to_bytes());
        put(&mut out, APP.as_bytes());
        out
    }

    /// The signers-file line this key gets, listing `namespaces`.
    pub fn line(&self, namespaces: &str) -> String {
        format!(
            "owner namespaces=\"{namespaces}\" {SK_TYPE} {} test-key",
            STANDARD.encode(self.blob())
        )
    }

    /// The `SignerLine` a loader keeps for this key.
    pub fn signer(&self) -> SignerLine {
        SignerLine {
            key_type: SK_TYPE.to_string(),
            public: self.blob(),
            fingerprint: fingerprint(&self.blob()),
        }
    }

    /// An armoured SSHSIG signature of `message` in `namespace`, with its user-presence flag set (`flags` 0x01).
    pub fn sign(&self, namespace: &str, message: &[u8]) -> String {
        self.sign_flags(namespace, message, 0x01, 0x01)
    }

    /// As `sign`, with the flags the authenticator signed (`signed`) and the flags the signature carries (`sent`).
    /// Equal values give a signature a real authenticator could make. Unequal ones give a forgery attempt.
    pub fn sign_flags(&self, namespace: &str, message: &[u8], signed: u8, sent: u8) -> String {
        let (ed, counter) = self.ed25519(namespace, message, signed, false);
        let mut sig = Vec::new();
        put(&mut sig, SK_TYPE.as_bytes());
        put(&mut sig, &ed);
        sig.push(sent);
        sig.extend_from_slice(&counter.to_be_bytes());
        armour(&sshsig(&self.blob(), namespace, &sig))
    }

    /// A signature whose ed25519 bytes are altered after signing, with the flags as they were signed.
    pub fn sign_tampered(&self, namespace: &str, message: &[u8]) -> String {
        let (ed, counter) = self.ed25519(namespace, message, 0x01, true);
        let mut sig = Vec::new();
        put(&mut sig, SK_TYPE.as_bytes());
        put(&mut sig, &ed);
        sig.push(0x01);
        sig.extend_from_slice(&counter.to_be_bytes());
        armour(&sshsig(&self.blob(), namespace, &sig))
    }

    /// A genuine signature with its algorithm name changed to `label`. The name and the framing after it are outside
    /// what the authenticator signs, so the relabel keeps the ed25519 signature valid. The ed25519 bytes, flags and
    /// counter are packed into one string, the only framing another algorithm name reads without trailing data.
    pub fn sign_as(&self, namespace: &str, message: &[u8], label: &str) -> String {
        let (ed, counter) = self.ed25519(namespace, message, 0x01, false);
        let mut packed = ed.to_vec();
        packed.push(0x01);
        packed.extend_from_slice(&counter.to_be_bytes());
        let mut sig = Vec::new();
        put(&mut sig, label.as_bytes());
        put(&mut sig, &packed);
        armour(&sshsig(&self.blob(), namespace, &sig))
    }

    /// The ed25519 signature over the sk signed data for `signed` flags, and the counter the signature carries.
    fn ed25519(
        &self,
        namespace: &str,
        message: &[u8],
        signed: u8,
        tamper: bool,
    ) -> ([u8; 64], u32) {
        let counter: u32 = 7;
        let data = signed_data(namespace, message);
        let mut inner = Vec::new();
        inner.extend_from_slice(&Sha256::digest(APP.as_bytes()));
        inner.push(signed);
        inner.extend_from_slice(&counter.to_be_bytes());
        inner.extend_from_slice(&Sha256::digest(&data));
        let mut ed = self.signing.sign(&inner).to_bytes();
        if tamper {
            ed[0] ^= 0x01;
        }
        (ed, counter)
    }
}

/// An ordinary ed25519 key from a fixed seed, which signs in SSHSIG form with the same layout.
pub struct EdKey {
    pub signing: SigningKey,
}

impl EdKey {
    pub fn from_seed(seed: u8) -> Self {
        Self {
            signing: SigningKey::from_bytes(&[seed; 32]),
        }
    }

    pub fn blob(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put(&mut out, ED_TYPE.as_bytes());
        put(&mut out, &self.signing.verifying_key().to_bytes());
        out
    }

    /// The signers-file line an ordinary ed25519 key would have.
    pub fn line(&self, namespaces: &str) -> String {
        format!(
            "owner namespaces=\"{namespaces}\" {ED_TYPE} {} test-plain",
            STANDARD.encode(self.blob())
        )
    }

    pub fn sign(&self, namespace: &str, message: &[u8]) -> String {
        let signature = self
            .signing
            .sign(&signed_data(namespace, message))
            .to_bytes();
        let mut sig = Vec::new();
        put(&mut sig, ED_TYPE.as_bytes());
        put(&mut sig, &signature);
        armour(&sshsig(&self.blob(), namespace, &sig))
    }
}

/// The data an SSHSIG signature covers: the preamble, the namespace, the reserved field, the hash name and the digest.
pub fn signed_data(namespace: &str, message: &[u8]) -> Vec<u8> {
    let mut out = b"SSHSIG".to_vec();
    put(&mut out, namespace.as_bytes());
    put(&mut out, b"");
    put(&mut out, b"sha512");
    put(&mut out, &Sha512::digest(message));
    out
}

/// The SSHSIG blob around a signature blob.
fn sshsig(public: &[u8], namespace: &str, signature: &[u8]) -> Vec<u8> {
    let mut out = b"SSHSIG".to_vec();
    out.extend_from_slice(&1u32.to_be_bytes());
    put(&mut out, public);
    put(&mut out, namespace.as_bytes());
    put(&mut out, b"");
    put(&mut out, b"sha512");
    put(&mut out, signature);
    out
}

/// The armoured form `ssh-keygen -Y sign` writes: the BEGIN line, base64 in 70-column lines, the END line.
pub fn armour(blob: &[u8]) -> String {
    let body = STANDARD.encode(blob);
    let mut out = String::from("-----BEGIN SSH SIGNATURE-----\n");
    for chunk in body.as_bytes().chunks(70) {
        out.push_str(std::str::from_utf8(chunk).expect("base64 is ASCII"));
        out.push('\n');
    }
    out.push_str("-----END SSH SIGNATURE-----\n");
    out
}

/// The blob an armoured signature carries: the base64 between its BEGIN and END lines.
pub fn unarmour(text: &str) -> Vec<u8> {
    let body: String = text
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect();
    STANDARD.decode(body).expect("the test armour is base64")
}

/// `SHA256:<base64>` of a public key blob, as `ssh-keygen` prints it.
pub fn fingerprint(blob: &[u8]) -> String {
    PublicKey::from_bytes(blob)
        .expect("the test key blob parses")
        .fingerprint(HashAlg::Sha256)
        .to_string()
}

/// The approval string for a statement: `<base64 statement>.<base64 signature>`.
pub fn approval_of(statement: &str, armoured: &str) -> String {
    format!(
        "{}.{}",
        STANDARD.encode(statement),
        STANDARD.encode(armoured)
    )
}

/// The time `offset_ms` after `NOW`, in `toISOString` form. Offsets stay inside the same day.
pub fn at(offset_ms: i64) -> String {
    let total = 12 * 3_600_000 + offset_ms;
    format!(
        "2026-10-30T{:02}:{:02}:{:02}.{:03}Z",
        total / 3_600_000,
        total % 3_600_000 / 60_000,
        total % 60_000 / 1000,
        total % 1000
    )
}

pub fn args() -> Value {
    json!({ "plan": PLAN })
}

pub fn target() -> String {
    format!("plan-{PLAN}")
}

/// A valid statement's fields for the default call, with `overrides` applied on top, as canonical JSON text.
pub fn statement_with(overrides: Value) -> String {
    let mut base = json!({
        "host": HOST,
        "action": "delete",
        "target": target(),
        "paramsDigest": params_digest(&args()).expect("the args are an object"),
        "issuedAt": at(0),
        "expiresAt": at(10 * 60_000),
        "nonce": NONCE,
    });
    for (key, value) in overrides.as_object().expect("overrides are an object") {
        base[key.as_str()] = value.clone();
    }
    canonical_json(&base).expect("the test fields canonicalize")
}

/// `ssh-keygen -Y verify` over `statement` with one signers line, in `namespace`. Returns whether it exited 0 and printed
/// a `Good` line. The owner's key is never involved: the signers file is a temporary file the test wrote.
pub fn ssh_keygen_verify(
    scratch: &Scratch,
    signers_line: &str,
    namespace: &str,
    statement: &str,
    armoured: &str,
) -> bool {
    let allowed = scratch.path("allowed_signers");
    fs::write(&allowed, format!("{signers_line}\n")).expect("the allowed signers file is written");
    let sig = scratch.path("statement.sig");
    fs::write(&sig, armoured).expect("the signature file is written");
    let mut child = Command::new("ssh-keygen")
        .args(["-Y", "verify", "-f"])
        .arg(&allowed)
        .args(["-I", "owner", "-n", namespace, "-s"])
        .arg(&sig)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("ssh-keygen is on PATH for these tests");
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(statement.as_bytes())
        .expect("the statement is written");
    let output = child.wait_with_output().expect("ssh-keygen finishes");
    output.status.success()
        && String::from_utf8_lossy(&output.stdout)
            .contains(&format!("Good \"{namespace}\" signature for owner"))
}
