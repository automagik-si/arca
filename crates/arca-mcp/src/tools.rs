//! The handler trait and the errors a call can end with.

use crate::auth::Caller;

/// Answers one verb. The server runs it on a blocking thread.
pub trait ToolHandler: Send + Sync {
    fn call(
        &self,
        args: serde_json::Value,
        caller: &Caller,
    ) -> Result<serde_json::Value, CallError>;
}

/// Why a call did not return a result. The HTTP layer answers `Unauthenticated` with 401, `Forbidden` with 403 and
/// `Internal` with 500, with no body. `NotFound`, `Invalid` and `Refused` are JSON-RPC errors inside a 200.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    /// No valid bearer. Only the server answers this; a handler has no reason to.
    Unauthenticated,
    /// The caller's scope may not call the verb.
    Forbidden { verb: String },
    /// The verb has no handler.
    NotFound,
    /// The arguments are not valid for the verb, or a handler rejects them. The text names a path and a reason, never a value.
    Invalid(String),
    /// The call was refused by a rule of the verb. `code` names the refusal, such as `Replayed`.
    Refused { code: String, message: String },
    /// A refusal whose message quotes text produced outside Arca (an answer of a service's own API): the door marks it
    /// `untrusted` beside its `refusal` code, so a caller treats the message as data, never as instructions.
    UntrustedRefused { code: String, message: String },
    /// A failure the caller must not see in detail. The text stays inside the server.
    Internal(String),
}
