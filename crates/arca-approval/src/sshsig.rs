//! The SSH signature verifier: an `sk-ssh-ed25519@openssh.com` SSHSIG in the Arca namespace, by a trusted owner key, with
//! the user-presence flag set.
//!
//! `ssh-key` checks the signature itself, including the flags byte, which the authenticator signs. It does not check the
//! flags' user-presence bit, and `ssh-keygen -Y verify` does not either. That bit is checked here.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ssh_key::{Algorithm, HashAlg, LineEnding, PublicKey, SshSig};

use crate::signers::SignerLine;
use crate::verifier::SignatureVerifier;
use crate::{ARCA_NAMESPACE, ApprovalRefusal};

/// Whether `armoured` is the one canonical encoding of `sshsig`. `ssh-key` reads a length prefix without requiring it to
/// match the bytes it decodes, so a relabelled length would read as the same signature. Re-encoding the parsed signature
/// and comparing the binary bodies refuses every such variant.
fn is_canonical(sshsig: &SshSig, armoured: &[u8]) -> bool {
    let Ok(canonical) = sshsig.to_pem(LineEnding::LF) else {
        return false;
    };
    match (body_bytes(canonical.as_bytes()), body_bytes(armoured)) {
        (Some(want), Some(got)) => want == got,
        _ => false,
    }
}

/// The base64 body of an armoured block, decoded. Armour lines and whitespace are dropped.
fn body_bytes(armoured: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(armoured).ok()?;
    let body: String = text
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .flat_map(str::chars)
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '='))
        .collect();
    STANDARD.decode(body).ok()
}

/// The user-presence bit of an sk signature's flags: the authenticator saw a touch.
const USER_PRESENT: u8 = 0x01;
/// The ed25519 signature inside an sk signature, before its flags byte and its counter.
const ED25519_SIGNATURE_LEN: usize = 64;
/// An sk signature's bytes: the ed25519 signature, the flags byte, and the counter.
const SK_SIGNATURE_LEN: usize = ED25519_SIGNATURE_LEN + 1 + 4;

/// Verifies an approval's signature against the owner keys a signers file listed for Arca.
pub struct SshSigVerifier {
    signers: Vec<PublicKey>,
}

impl SshSigVerifier {
    /// A verifier that trusts `signers`. A line whose blob does not parse as a hardware key is dropped, so every key this
    /// verifier trusts is one it can check.
    pub fn new(signers: Vec<SignerLine>) -> Self {
        let signers = signers
            .into_iter()
            .filter_map(|line| {
                let key = PublicKey::from_bytes(&line.public).ok()?;
                key.key_data().is_sk_ed25519().then_some(key)
            })
            .collect();
        Self { signers }
    }

    /// The number of keys this verifier trusts.
    pub fn key_count(&self) -> usize {
        self.signers.len()
    }
}

impl SignatureVerifier for SshSigVerifier {
    /// Refuses in this order: a namespace other than Arca's; a signature that is not one canonical SSHSIG version 1; a key
    /// that is not a hardware key, or a signature whose algorithm is not `sk-ssh-ed25519@openssh.com`; a signature in
    /// another namespace than the one asked; no trusted key at all; a key the signers file does not hold; a signature
    /// that does not verify over the statement's bytes; a signature without the user-presence flag. Returns the key's
    /// fingerprint on a pass.
    fn verify(
        &self,
        namespace: &str,
        statement: &[u8],
        signature: &[u8],
    ) -> Result<String, ApprovalRefusal> {
        // This verifier trusts only Arca's owner keys, so it verifies only Arca's namespace, whatever its caller asks.
        if namespace != ARCA_NAMESPACE {
            return Err(ApprovalRefusal::WrongNamespace);
        }
        let sshsig = SshSig::from_pem(signature).map_err(|_| {
            ApprovalRefusal::Malformed(
                "the signature cannot be read as an SSH signature".to_string(),
            )
        })?;
        if !is_canonical(&sshsig, signature) {
            return Err(ApprovalRefusal::Malformed(
                "the signature is not in its canonical SSHSIG encoding".to_string(),
            ));
        }
        if sshsig.version() != SshSig::VERSION {
            return Err(ApprovalRefusal::Malformed(
                "the signature is not SSHSIG version 1".to_string(),
            ));
        }
        let key_data = sshsig.public_key();
        // `ssh-key` checks the signature bytes under the key and not the algorithm name, so a genuine signature
        // relabelled to another name would still verify. The name is checked here.
        if key_data.sk_ed25519().is_none() || sshsig.algorithm() != Algorithm::SkEd25519 {
            return Err(ApprovalRefusal::NotHardwareKey);
        }
        if sshsig.namespace() != namespace {
            return Err(ApprovalRefusal::WrongNamespace);
        }
        if self.signers.is_empty() {
            return Err(ApprovalRefusal::NoVerifier);
        }
        let Some(trusted) = self.signers.iter().find(|key| key.key_data() == key_data) else {
            return Err(ApprovalRefusal::BadSignature);
        };
        trusted
            .verify(namespace, statement, &sshsig)
            .map_err(|_| ApprovalRefusal::BadSignature)?;
        let raw = sshsig.signature().as_bytes();
        if raw.len() != SK_SIGNATURE_LEN {
            return Err(ApprovalRefusal::BadSignature);
        }
        if raw[ED25519_SIGNATURE_LEN] & USER_PRESENT == 0 {
            return Err(ApprovalRefusal::NoUserPresence);
        }
        Ok(trusted.fingerprint(HashAlg::Sha256).to_string())
    }
}
