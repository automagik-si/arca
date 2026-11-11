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
/// `Internal` with 500, with no body. `NotFound`, `Invalid` and the three refusals (`Refused`, `UntrustedRefused` and
/// `RefusedWith`) are JSON-RPC errors inside a 200.
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
    /// A refusal that carries fields beside its code: `capacity` says whose cap, which cap and the amounts, `rate` says
    /// how long to wait. The door writes each field beside `refusal`. The message is the whole reason in words, as for
    /// any refusal. Only a code the OpenRPC document lists under `x-arca-refusals` is carried this way
    /// ([`arca_api::refusals`]).
    RefusedWith {
        code: String,
        message: String,
        fields: serde_json::Map<String, serde_json::Value>,
    },
    /// A failure the caller must not see in detail. The text stays inside the server.
    Internal(String),
}

impl CallError {
    /// A refusal for want of room: the call would pass a cap of a tenant or of the node. Its code is `capacity`.
    pub fn capacity(message: impl Into<String>, room: arca_api::refusals::Room) -> CallError {
        CallError::RefusedWith {
            code: arca_api::refusals::CAPACITY.to_string(),
            message: message.into(),
            fields: room.fields(),
        }
    }

    /// A refusal for asking too often, or for having too much waiting: the same call is allowed again after a wait, of
    /// `retry_after_seconds` when that is known. Its code is `rate`.
    pub fn rate(message: impl Into<String>, retry_after_seconds: Option<u64>) -> CallError {
        CallError::RefusedWith {
            code: arca_api::refusals::RATE.to_string(),
            message: message.into(),
            fields: arca_api::refusals::retry_after(retry_after_seconds),
        }
    }

    /// The code and the message of a refusal, whichever of the three refusal variants carries it.
    pub fn refusal(&self) -> Option<(&str, &str)> {
        match self {
            CallError::Refused { code, message }
            | CallError::UntrustedRefused { code, message }
            | CallError::RefusedWith { code, message, .. } => Some((code, message)),
            _ => None,
        }
    }

    /// The `data` of the refusal's JSON-RPC error: `refusal` with the code, `untrusted` for a refusal that quotes a
    /// service, and the fields of a refusal that carries some. `None` for an error that is not a refusal.
    pub fn refusal_data(&self) -> Option<serde_json::Value> {
        let mut data = serde_json::Map::new();
        match self {
            CallError::Refused { code, .. } => {
                data.insert("refusal".into(), code.as_str().into());
            }
            CallError::UntrustedRefused { code, .. } => {
                data.insert("refusal".into(), code.as_str().into());
                data.insert("untrusted".into(), true.into());
            }
            CallError::RefusedWith { code, fields, .. } => {
                // The code is written last, so no field can stand in for it.
                data.extend(fields.clone());
                data.insert("refusal".into(), code.as_str().into());
            }
            _ => return None,
        }
        Some(serde_json::Value::Object(data))
    }
}
