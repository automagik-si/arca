//! Arca's owner approvals. The owner signs a statement on their own machine, in the namespace `arca-approve`, and an approval
//! is that statement with its signature. This crate reads the approval as droidd's `owner-approvals.ts` reads one, checks it
//! in droidd's order, and spends its nonce once.
//!
//! Until a signature verifier is installed, [`RefuseAll`] refuses every well-formed approval with
//! [`ApprovalRefusal::NoVerifier`], before any nonce is read. A malformed approval is refused as malformed before the
//! verifier is reached.

pub mod statement;
pub mod verifier;

pub use statement::{ArcaAction, OwnerApproval, parse_statement, split_approval};
pub use verifier::{ApprovalVerifier, MemNonces, NonceStore, RefuseAll, SignatureVerifier, Spent};

/// The namespace an owner signs an approval in (`ssh-keygen -Y sign -n`). Droidd's `ARCA_NAMESPACE`.
pub const ARCA_NAMESPACE: &str = "arca-approve";
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
    /// No signature verifier is installed.
    NoVerifier,
    /// The signature does not verify in the `arca-approve` namespace.
    BadSignature,
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
}

impl std::fmt::Display for ApprovalRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(why) => write!(f, "the approval is malformed: {why}"),
            Self::NotCanonical => f.write_str("the approval's statement is not canonical JSON"),
            Self::NoVerifier => f.write_str("no owner signature verifier is installed"),
            Self::BadSignature => f.write_str("the approval's signature does not verify"),
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
        }
    }
}

impl std::error::Error for ApprovalRefusal {}
