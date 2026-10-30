//! Arca's owner approvals. The owner signs a statement on their own machine with a hardware key, in the namespace
//! [`ARCA_NAMESPACE`], and an approval is that statement with its signature. This crate reads the approval as droidd's
//! `owner-approvals.ts` reads one, verifies the signature, checks the statement against the call in droidd's order, and
//! spends its nonce once.
//!
//! Until a signature verifier is installed, [`RefuseAll`] refuses every well-formed approval with
//! [`ApprovalRefusal::NoVerifier`], before any nonce is read. A malformed approval is refused as malformed before the
//! verifier is reached.
//!
//! The verifier that ships is [`SshSigVerifier`]: an `sk-ssh-ed25519@openssh.com` signature in [`ARCA_NAMESPACE`], by a key
//! that the root-owned signers file lists for that namespace, with the user-presence flag set. Its nonces are kept by
//! [`FileNonces`], which makes each spend durable before the action it admits may run.

pub mod nonces;
pub mod signers;
pub mod sshsig;
pub mod statement;
pub mod verifier;

pub use nonces::FileNonces;
pub use signers::{SignerLine, TrustRoot, load_signers, signers_trust};
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
    /// The statement's bytes are not its own canonical JSON (whitespace, a duplicate key, a needless escape, unsorted keys).
    NotCanonical,
    /// No signature verifier is installed, or none of the owner's keys is trusted for this namespace.
    NoVerifier,
    /// The signature does not verify in the Arca namespace, or its key is not one the signers file lists for it.
    BadSignature,
    /// The signature was not made by a hardware (`sk-`) key, so it cannot be an owner's approval.
    NotHardwareKey,
    /// The signature was made without a touch of the hardware key (its user-presence flag is clear).
    NoUserPresence,
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
    /// A file that holds the owner's trust (the signers file, or the nonce file) cannot be trusted, or a directory
    /// above the signers file cannot be. The nonce file's directory is not checked: the caller keeps the state
    /// directory at mode 0700. The text names the file and the rule it broke, never a key.
    Untrusted(String),
    /// The nonce store could not be read, or a spend could not be made durable. Nothing was allowed.
    Storage(String),
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
                f.write_str("the approval was signed without a touch of the hardware key")
            }
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
                write!(f, "the nonce store cannot be read or made durable: {why}")
            }
        }
    }
}

impl std::error::Error for ApprovalRefusal {}
