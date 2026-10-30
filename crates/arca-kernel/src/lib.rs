//! The Arca kernel's contracts: canonical JSON, node identity, signed desired state, the ownership model, the effect journal
//! and the plugin contract.
//!
//! Nothing in this crate touches Incus or a host. The kernel's one-writer rule is stated here as
//! [`ownership::kernel_may_write`]; the providers that write must ask it before they act. Every effect is journalled as
//! an intent before it runs and as its inverse once it applies ([`journal`]); a plugin reaches the host only through
//! [`plugin::Ctx`].

pub mod canon;
pub mod desired;
pub mod identity;
pub mod journal;
pub mod ownership;
pub mod plugin;

use std::fmt;

use canon::CanonError;
use desired::DocRefusal;

/// Every failure the kernel reports. Its `Display` names paths and states reasons; it never prints file contents, keys,
/// signatures or statement bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KernelError {
    /// A file could not be read or written. `why` is the operating system's reason, never the file's contents.
    Io { path: String, why: String },
    /// The node key file has a group or other permission bit, so it is refused.
    KeyMode { path: String },
    /// The node key path is a symbolic link, so it is refused.
    KeySymlink { path: String },
    /// A value could not be canonicalized or a digest could not be read.
    Canon(CanonError),
    /// The effect journal could not be decoded or synced.
    Journal(String),
    /// The journal holds intents with no recorded outcome, so the kernel refuses to open.
    Unresolved { ids: Vec<u64> },
    /// The desired-state store is held by another process.
    Locked { path: String },
    /// A proposed desired-state document was refused by the acceptance gate.
    Refused(DocRefusal),
}

impl fmt::Display for KernelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, why } => write!(f, "{path}: {why}"),
            Self::KeyMode { path } => write!(
                f,
                "node key {path} has a group or other permission bit; it must be mode 0600"
            ),
            Self::KeySymlink { path } => write!(f, "node key {path} is a symbolic link; refused"),
            Self::Canon(e) => write!(f, "canonical JSON: {e}"),
            Self::Journal(why) => write!(f, "journal: {why}"),
            Self::Unresolved { ids } => write!(
                f,
                "the journal holds {} unresolved intent(s); the kernel refuses to open",
                ids.len()
            ),
            Self::Locked { path } => {
                write!(f, "desired-state store {path} is locked by another process")
            }
            Self::Refused(reason) => write!(f, "document refused: {reason}"),
        }
    }
}

impl std::error::Error for KernelError {}

impl From<CanonError> for KernelError {
    fn from(e: CanonError) -> Self {
        Self::Canon(e)
    }
}
