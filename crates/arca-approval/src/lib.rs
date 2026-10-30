//! Arca's owner approvals. The owner signs a statement on their own machine with a hardware key or a passkey, in the
//! namespace [`ARCA_NAMESPACE`], and an approval is that statement with its signature. This crate reads the approval as droidd's
//! `owner-approvals.ts` reads one, verifies the signature, checks the statement against the call in droidd's order, and
//! spends its nonce once.
//!
//! Until a signature verifier is installed, [`RefuseAll`] refuses every well-formed approval with
//! [`ApprovalRefusal::NoVerifier`], before any nonce is read. A malformed approval is refused as malformed before the
//! verifier is reached.
//!
//! The verifier that ships is [`OwnerKeys`], which sends each proof to one of two verifiers by its first byte:
//!
//! - [`SshSigVerifier`]: an `sk-ssh-ed25519@openssh.com` signature in [`ARCA_NAMESPACE`], by a key that the root-owned
//!   signers file lists for that namespace, with the user-presence flag set.
//! - [`PasskeyVerifier`]: a WebAuthn ES256 assertion over the statement's challenge, by a passkey that the root-owned
//!   passkeys file lists for that namespace, with user presence and user verification set, and a sign count that only grows.
//!
//! Nonces are kept by [`FileNonces`], which makes each spend durable before the action it admits may run.

pub mod nonces;
pub mod owner;
pub mod passkey;
pub mod signers;
pub mod sshsig;
pub mod statement;
pub mod verifier;

pub use nonces::FileNonces;
pub use owner::OwnerKeys;
pub use passkey::{
    Asserted, FileSignCounts, OWNER_PASSKEYS, PASSKEY_CHALLENGE_PREFIX, PASSKEY_COUNTS_FILE,
    PasskeyLine, PasskeyProof, PasskeyVerifier, TEST_PASSKEYS_ENV, challenge_for, load_passkeys,
    parse_passkeys, parse_proof, passkeys_trust, verify_assertion,
};
pub use signers::{SignerLine, TrustRoot, effective_uid, load_signers, signers_trust};
pub use sshsig::SshSigVerifier;
pub use statement::{ArcaAction, OwnerApproval, parse_statement, split_approval};
pub use verifier::{ApprovalVerifier, MemNonces, NonceStore, RefuseAll, SignatureVerifier, Spent};

/// The namespace an Arca owner signs an approval in (`ssh-keygen -Y sign -n`), and the namespace a signers-file key line
/// must list for Arca to keep it.
///
/// It is not droidd's `arca-approve`. Droidd checks the same statement keys and target grammar in that namespace, and an
/// SSH signature covers its namespace. Giving Arca its own namespace means no signature droidd accepts can verify here,
/// whatever its statement says, so one approval cannot be spent once in each.
pub const ARCA_NAMESPACE: &str = "arca-kernel-approve";
/// The longest an approval may be valid, both from its issue and from its check. Droidd's `ARCA_TTL_MAX_MS`.
pub const TTL_MAX_MS: u64 = 900_000;
/// How far ahead of this host's clock an approval's `issuedAt` may be. Droidd's `ARCA_ISSUE_SKEW_MS`.
pub const ISSUE_SKEW_MS: u64 = 300_000;

/// Why an approval is refused. No variant carries the value that failed: a reason names the field, never its content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalRefusal {
    /// The approval string, the statement or one of its fields is not what an approval is. The text names the part.
    Malformed(String),
    /// The statement's bytes, or a passkey proof's, are not their own canonical JSON (whitespace, a duplicate key, a needless
    /// escape, unsorted keys).
    NotCanonical,
    /// No signature verifier is installed, or none of the owner's keys is trusted for this namespace.
    NoVerifier,
    /// The signature does not verify in the Arca namespace, or its key is not one the signers file lists for it. For a
    /// passkey: no line of the passkeys file holds its credential for the Arca namespace, it signed another challenge than
    /// this statement's, or its signature does not verify against the line's key.
    BadSignature,
    /// The signature was not made by a hardware (`sk-`) key, so it cannot be an owner's approval.
    NotHardwareKey,
    /// The signature was made without a touch of the hardware key (its user-presence flag is clear), or the passkey reports
    /// no user presence (UP unset).
    NoUserPresence,
    /// The passkey reports no user verification (UV unset): no biometric or PIN was checked.
    NoUserVerification,
    /// The passkey answered a page other than the origin its line names, or a page framed inside another (`crossOrigin`,
    /// `topOrigin`).
    WrongOrigin,
    /// The passkey's assertion is for another relying party than the one its line names (the rpIdHash differs).
    WrongRp,
    /// The passkey's sign count is not above the last one kept for it, or is 0 after it has counted: a replayed assertion or
    /// a cloned passkey.
    SignCountRegressed,
    /// The signature is in another namespace than the Arca namespace.
    WrongNamespace,
    /// The statement names another host.
    WrongHost,
    /// The statement approves another action.
    WrongAction,
    /// The statement approves another target.
    WrongTarget,
    /// The statement approves other arguments: its `paramsDigest` is not the call's.
    WrongParams,
    /// The statement was issued more than five minutes ahead of this host's clock.
    IssuedAhead,
    /// The statement has expired.
    Expired,
    /// The statement's window runs over fifteen minutes, from its issue or from now, or does not run forward at all.
    TtlTooLong,
    /// The statement's nonce is spent.
    Replayed,
    /// A file that holds the owner's trust (the signers file, the passkeys file, the nonce file or the sign-count file)
    /// cannot be trusted, or a directory above the signers or passkeys file cannot be. The state files' directory is not
    /// checked: the caller keeps the state directory at mode 0700. The text names the file, the rule it broke and at most
    /// a line number, never a key or a credential.
    Untrusted(String),
    /// The nonce store or the sign-count store could not be read, or a spend or a count could not be made durable. Nothing
    /// was allowed.
    Storage(String),
    /// The journal holds an intent with no recorded outcome, so no delete may run until it is resolved. Nothing was spent.
    Unresolved,
}

impl std::fmt::Display for ApprovalRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(why) => write!(f, "the approval is malformed: {why}"),
            Self::NotCanonical => f.write_str("the approval's statement is not canonical JSON"),
            Self::NoVerifier => {
                f.write_str("no owner signature verifier trusts a key for this namespace")
            }
            Self::BadSignature => {
                f.write_str("the approval's signature does not verify against a trusted owner key")
            }
            Self::NotHardwareKey => {
                f.write_str("the approval was not signed by a hardware (sk-) key")
            }
            Self::NoUserPresence => {
                f.write_str("the approval was signed without a touch of the hardware key or passkey")
            }
            Self::NoUserVerification => f.write_str(
                "the passkey reports no user verification (no biometric or PIN was checked)",
            ),
            Self::WrongOrigin => f.write_str(
                "the passkey answered another page than the origin its line names, or a framed page",
            ),
            Self::WrongRp => {
                f.write_str("the passkey's assertion is for another relying party than its line's")
            }
            Self::SignCountRegressed => f.write_str(
                "the passkey's sign count is not above the last one kept for it: a replay or a cloned passkey",
            ),
            Self::WrongNamespace => f.write_str("the approval's signature is in another namespace"),
            Self::WrongHost => f.write_str("the approval names another host"),
            Self::WrongAction => f.write_str("the approval names another action"),
            Self::WrongTarget => f.write_str("the approval names another target"),
            Self::WrongParams => f.write_str("the approval names other arguments"),
            Self::IssuedAhead => f.write_str(
                "the approval is issued more than five minutes ahead of this host's clock",
            ),
            Self::Expired => f.write_str("the approval has expired"),
            Self::TtlTooLong => f.write_str(
                "the approval's window is longer than fifteen minutes, or does not run forward",
            ),
            Self::Replayed => f.write_str("the approval's nonce is spent"),
            Self::Untrusted(why) => write!(f, "an owner trust file cannot be trusted: {why}"),
            Self::Storage(why) => {
                write!(f, "an approval store cannot be read or made durable: {why}")
            }
            Self::Unresolved => f.write_str(
                "the journal holds an unresolved intent; no delete runs until it is resolved",
            ),
        }
    }
}

impl std::error::Error for ApprovalRefusal {}
