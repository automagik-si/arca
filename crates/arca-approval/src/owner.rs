//! The owner's keys: the one [`SignatureVerifier`] the kernel holds, which sends each proof to the verifier its first byte
//! names. One seam, one nonce store and one statement format serve both kinds of proof.

use crate::ApprovalRefusal;
use crate::passkey::PasskeyVerifier;
use crate::sshsig::SshSigVerifier;
use crate::statement::PASSKEY_PROOF_OPEN;
use crate::verifier::SignatureVerifier;

/// The owner's SSH hardware keys and passkeys. A proof that opens a JSON object (`{`) is a passkey proof, checked by the
/// passkey verifier. Any other is an armoured SSH signature, checked by the SSH verifier. Neither verifier is asked about
/// the other's proof, so a refusal always comes from the verifier the proof chose.
pub struct OwnerKeys {
    ssh: SshSigVerifier,
    passkeys: PasskeyVerifier,
}

impl OwnerKeys {
    /// The owner keys that `ssh` and `passkeys` trust.
    pub fn new(ssh: SshSigVerifier, passkeys: PasskeyVerifier) -> Self {
        Self { ssh, passkeys }
    }

    /// The number of SSH keys and the number of passkeys trusted, in that order.
    pub fn key_counts(&self) -> (usize, usize) {
        (self.ssh.key_count(), self.passkeys.key_count())
    }
}

impl SignatureVerifier for OwnerKeys {
    fn verify(
        &self,
        namespace: &str,
        statement: &[u8],
        signature: &[u8],
    ) -> Result<String, ApprovalRefusal> {
        if signature.first() == Some(&PASSKEY_PROOF_OPEN) {
            self.passkeys.verify(namespace, statement, signature)
        } else {
            self.ssh.verify(namespace, statement, signature)
        }
    }
}
