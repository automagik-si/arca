//! Approving a plan from the terminal of the client that asked for it, with MCP elicitation.
//!
//! Only a call by the node's one `admin` token, from a client that declared the elicitation capability, can reach this.
//! When such a call's answer is a plan that waits for the owner, the server asks the client, in words, whether to approve
//! it: on `2025-11-25` as a `elicitation/create` request on the call's own SSE response stream (Streamable HTTP, "Sending
//! Messages to the Server"; the client's answer comes back as a second `POST`, which the server answers `202`), and on
//! `2026-07-28` as an `input_required` result the client answers by sending the call again with `inputResponses` (the
//! multi round-trip pattern, which replaces server-to-client requests in that version). An explicit accept with `yes`
//! approves the plan through [`TerminalApprover::approve`]; a decline, a cancel, a timeout or anything else leaves the plan
//! waiting with its normal card, and the call answers as it would without elicitation.
//!
//! The server keeps no session. Which clients can be asked is remembered per admin token id from its last `initialize`
//! (`2025-11-25`) and read from each request's own capabilities (`2026-07-28`). A `2026-07-28` retry carries a
//! `requestState` the server signed (HMAC-SHA256 under a key made at start): it names the token, the plan, the verb with
//! the digest of its arguments and an expiry, so a state is never taken for another caller's or another call's.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;

use crate::auth::Caller;
use crate::tools::CallError;

/// How long a plan's question waits for the terminal by default: under the five minutes a client may keep an idle HTTP call.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(240);

/// The key of the one input request, in `inputRequests` and `inputResponses`.
pub const APPROVE_KEY: &str = "approve";

/// A plan that waits for the owner: its id, and the question that states it in the card's words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPlan {
    pub id: String,
    pub message: String,
}

/// What the server asks of the node to approve from the terminal. The node supplies it; this crate only asks.
pub trait TerminalApprover: Send + Sync {
    /// Whether `result`, the answer of `verb`, is a plan waiting for the owner, and the question that states it.
    fn pending(&self, verb: &str, result: &Value) -> Option<PendingPlan>;

    /// Approves plan `plan` in the terminal for the admin token `caller` and runs it, as the web card's approval does,
    /// recording that it was approved in the terminal by that token. Returns what the run answered.
    fn approve(&self, plan: &str, caller: &Caller) -> Result<Value, CallError>;
}

/// The server's side of terminal approval.
pub(crate) struct Terminal {
    pub(crate) approver: Arc<dyn TerminalApprover>,
    pub(crate) timeout: Duration,
    /// The elicitation requests sent and not yet answered: request id to the token that was asked and where its answer goes.
    waiting: Mutex<HashMap<String, (String, oneshot::Sender<Value>)>>,
    /// The ids of the admin tokens whose last `initialize` declared the elicitation capability.
    capable: Mutex<HashSet<String>>,
    key: [u8; 32],
}

impl Terminal {
    pub(crate) fn new(approver: Arc<dyn TerminalApprover>, timeout: Duration) -> Terminal {
        let mut key = [0u8; 32];
        getrandom::fill(&mut key).expect("the operating system's random source is available");
        Terminal {
            approver,
            timeout,
            waiting: Mutex::new(HashMap::new()),
            capable: Mutex::new(HashSet::new()),
            key,
        }
    }

    /// Remembers whether the client of `token` declared elicitation at `initialize`.
    pub(crate) fn note_initialize(&self, token: &str, params: &Value) {
        let declared = declares_elicitation(params.get("capabilities"));
        if let Ok(mut capable) = self.capable.lock() {
            if declared {
                capable.insert(token.to_string());
            } else {
                capable.remove(token);
            }
        }
    }

    pub(crate) fn is_capable(&self, token: &str) -> bool {
        self.capable
            .lock()
            .map(|capable| capable.contains(token))
            .unwrap_or(false)
    }

    /// Registers a request id for `token` and returns where its answer arrives.
    pub(crate) fn expect(&self, token: &str) -> Option<(String, oneshot::Receiver<Value>)> {
        let mut raw = [0u8; 12];
        getrandom::fill(&mut raw).ok()?;
        let id = format!("arca-approve-{}", URL_SAFE_NO_PAD.encode(raw));
        let (tx, rx) = oneshot::channel();
        self.waiting
            .lock()
            .ok()?
            .insert(id.clone(), (token.to_string(), tx));
        Some((id, rx))
    }

    pub(crate) fn forget(&self, id: &str) {
        if let Ok(mut waiting) = self.waiting.lock() {
            waiting.remove(id);
        }
    }

    /// Hands a client's JSON-RPC response to the call that asked. `true` when `id` was one this token was asked.
    pub(crate) fn deliver(&self, id: &str, token: &str, response: Value) -> bool {
        let Ok(mut waiting) = self.waiting.lock() else {
            return false;
        };
        match waiting.get(id) {
            Some((asked, _)) if asked == token => {}
            _ => return false,
        }
        if let Some((_, tx)) = waiting.remove(id) {
            let _ = tx.send(response);
        }
        true
    }

    /// The `requestState` for a plan: signed, naming the token, the plan, the call and an expiry.
    pub(crate) fn sign(&self, token: &str, plan: &str, call: &str, now_s: u64) -> String {
        let payload = json!({
            "t": token, "p": plan, "c": call, "x": now_s + self.timeout.as_secs()
        })
        .to_string();
        let body = URL_SAFE_NO_PAD.encode(payload);
        let mac = URL_SAFE_NO_PAD.encode(hmac_sha256(&self.key, body.as_bytes()));
        format!("{body}.{mac}")
    }

    /// What a `requestState` says, when it is ours, for `token` and the call `call`.
    pub(crate) fn verify(&self, state: &str, token: &str, call: &str, now_s: u64) -> State {
        let Some((body, mac)) = state.split_once('.') else {
            return State::Invalid;
        };
        let Ok(mac) = URL_SAFE_NO_PAD.decode(mac) else {
            return State::Invalid;
        };
        let expected = hmac_sha256(&self.key, body.as_bytes());
        if mac.len() != 32 || !constant_time_eq(&mac, &expected) {
            return State::Invalid;
        }
        let Some(payload) = URL_SAFE_NO_PAD
            .decode(body)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        else {
            return State::Invalid;
        };
        if payload["t"] != token || payload["c"] != call {
            return State::Invalid;
        }
        if payload["x"].as_u64().is_none_or(|x| x < now_s) {
            return State::Expired;
        }
        match payload["p"].as_str() {
            Some(plan) => State::Plan(plan.to_string()),
            None => State::Invalid,
        }
    }
}

/// A `requestState` as [`Terminal::verify`] reads it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum State {
    Plan(String),
    Expired,
    Invalid,
}

/// Whether a `capabilities` object declares elicitation. `{"elicitation":{}}` is form mode, which is all this asks for.
pub(crate) fn declares_elicitation(capabilities: Option<&Value>) -> bool {
    capabilities
        .and_then(|c| c.get("elicitation"))
        .is_some_and(Value::is_object)
}

/// The `elicitation/create` params that ask the one question: approve, yes or no. A flat object of one enum, as form mode allows.
pub(crate) fn request_params(message: &str) -> Value {
    json!({
        "mode": "form",
        "message": message,
        "requestedSchema": {
            "type": "object",
            "properties": {
                "approve": {
                    "type": "string",
                    "title": "Approve this plan?",
                    "description": "Choose yes to approve it and run it now. Anything else leaves it waiting on its card.",
                    "enum": ["yes", "no"],
                    "default": "no"
                }
            },
            "required": ["approve"]
        }
    })
}

/// Whether a client's answer is an explicit accept with yes.
pub(crate) fn is_yes(answer: &Value) -> bool {
    answer["action"] == "accept" && answer["content"]["approve"] == "yes"
}

/// The digest of a call, for a state to name it: the verb and its arguments.
pub(crate) fn call_digest(verb: &str, arguments: &Value) -> String {
    let text = json!({ "verb": verb, "arguments": arguments }).to_string();
    to_hex(&Sha256::digest(text.as_bytes()))
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// HMAC-SHA256 (RFC 2104).
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(block.map(|b| b ^ 0x36));
    inner.update(message);
    let mut outer = Sha256::new();
    outer.update(block.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_the_rfc_4231_vector() {
        // Test case 2 of RFC 4231: key "Jefe", data "what do ya want for nothing?".
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            to_hex(&mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }
}
