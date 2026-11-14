//! Approving a plan from the terminal of the client that asked for it, with MCP elicitation.
//!
//! Only a caller the node says may be asked ([`TerminalApprover::may_ask`]: the node's one `admin` token, or the owner's
//! own signed-in session, an issued token whose grant includes `approvals`), from a client that declared the elicitation
//! capability, can reach this. When such a call's answer is a plan that waits for the owner, the server asks the client,
//! in words, whether to approve it: on `2025-11-25` as a `elicitation/create` request on the call's own SSE response
//! stream (Streamable HTTP, "Sending Messages to the Server"; the client's answer comes back as a second `POST`, which the
//! server answers `202`), and on `2026-07-28` as an `input_required` result the client answers by sending the call again
//! with `inputResponses` (the multi round-trip pattern, which replaces server-to-client requests in that version). An
//! explicit accept with `yes` approves the plan through [`TerminalApprover::approve`]; a decline, a cancel, a timeout or
//! anything else leaves the plan waiting with its normal card, and the call answers as it would without elicitation.
//!
//! A listing verb can name several waiting plans ([`TerminalApprover::waiting`]): each is asked in turn on the stream, or
//! all at once as one `inputRequests` entry each. A session whose passkey is too old to be asked is first sent a URL-mode
//! elicitation to the page where one passkey tap confirms it ([`Asking::StepUp`]).
//!
//! The server keeps no session. Which clients can be asked is remembered per token id from its last `initialize`
//! (`2025-11-25`) and read from each request's own capabilities (`2026-07-28`). A `2026-07-28` retry carries a
//! `requestState` the server signed (HMAC-SHA256 under a key made at start): it names the token, the plan or plans, the
//! verb with the digest of its arguments and an expiry, so a state is never taken for another caller's or another call's.

use std::collections::HashMap;
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

/// The key of the one URL-mode input request that sends the owner to confirm with a passkey.
pub const STEP_UP_KEY: &str = "stepUp";

/// A plan that waits for the owner: its id, the question that states it in the card's words, and when it was made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPlan {
    pub id: String,
    pub message: String,
    pub created_at: u64,
}

/// Whether the terminal of a caller may be asked to approve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Asking {
    /// It may be asked now.
    Yes,
    /// It may be asked once the owner confirms with a passkey at `url`, which a URL-mode elicitation opens.
    StepUp { url: String },
    /// It is never asked.
    No,
}

/// What the server asks of the node to approve from the terminal. The node supplies it; this crate only asks.
pub trait TerminalApprover: Send + Sync {
    /// Whether `caller`'s terminal may be asked. By default only the admin token's.
    fn may_ask(&self, caller: &Caller) -> Asking {
        if caller.scope == arca_api::Scope::Admin {
            Asking::Yes
        } else {
            Asking::No
        }
    }

    /// Whether `result`, the answer of `verb` to `caller`, is a plan waiting for the owner, and the question that states it.
    fn pending(&self, verb: &str, result: &Value, caller: &Caller) -> Option<PendingPlan>;

    /// The waiting plans a listing `verb` answered with that `caller`'s terminal may be asked about, one question each.
    /// None by default.
    fn waiting(&self, _verb: &str, _result: &Value, _caller: &Caller) -> Vec<PendingPlan> {
        Vec::new()
    }

    /// Approves plan `plan` in the terminal of `caller` and runs it, as the web card's approval does,
    /// recording that it was approved in the terminal by that token. Returns what the run answered.
    ///
    /// `created_at` is the creation time of the very proposal the question stated. It approves that record only: a plan
    /// the owner decided on the card meanwhile, and proposed again after, is another record with the same id, and is
    /// refused.
    fn approve(&self, plan: &str, created_at: u64, caller: &Caller) -> Result<Value, CallError>;
}

/// The server's side of terminal approval.
pub(crate) struct Terminal {
    pub(crate) approver: Arc<dyn TerminalApprover>,
    pub(crate) timeout: Duration,
    /// The elicitation requests sent and not yet answered: request id to the token that was asked and where its answer goes.
    waiting: Mutex<HashMap<String, (String, oneshot::Sender<Value>)>>,
    /// The ids of the tokens whose last `initialize` declared the elicitation capability, with whether it named URL mode.
    capable: Mutex<HashMap<String, Modes>>,
    /// The states already used to approve, each until it expires: a state approves once.
    spent: Mutex<HashMap<String, u64>>,
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
            capable: Mutex::new(HashMap::new()),
            spent: Mutex::new(HashMap::new()),
            key,
        }
    }

    /// Remembers whether the client of `token` declared elicitation at `initialize`.
    pub(crate) fn note_initialize(&self, token: &str, params: &Value) {
        let modes = Modes {
            form: declares_elicitation(params.get("capabilities")),
            url: declares_url(params.get("capabilities")),
        };
        if let Ok(mut capable) = self.capable.lock() {
            if modes.form || modes.url {
                capable.insert(token.to_string(), modes);
            } else {
                capable.remove(token);
            }
        }
    }

    /// What the client of `token` declared at its last `initialize`.
    pub(crate) fn modes(&self, token: &str) -> Modes {
        self.capable
            .lock()
            .ok()
            .and_then(|capable| capable.get(token).copied())
            .unwrap_or_default()
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

    /// Spends `state`: `true` the first time, `false` when it was spent already. A spent state is remembered until it would
    /// have expired anyway, so a replay inside its lifetime cannot approve an identical new proposal.
    pub(crate) fn claim(&self, state: &str, now_s: u64) -> bool {
        let Ok(mut spent) = self.spent.lock() else {
            return false;
        };
        spent.retain(|_, until| *until >= now_s);
        spent
            .insert(state.to_string(), now_s + self.timeout.as_secs() + 1)
            .is_none()
    }

    /// The `requestState` for a plan: signed, naming the token, the plan, the proposal that was shown (so the reply is built
    /// from it and the approval binds its creation time), the call and an expiry.
    pub(crate) fn sign(
        &self,
        token: &str,
        plan: &str,
        proposal: &Value,
        call: &str,
        now_s: u64,
    ) -> String {
        self.seal(json!({
            "t": token, "p": plan, "v": proposal, "c": call, "x": now_s + self.timeout.as_secs()
        }))
    }

    /// The `requestState` for several plans asked at once: each plan's id and creation time, and the listing that was shown.
    pub(crate) fn sign_many(
        &self,
        token: &str,
        plans: &[PendingPlan],
        listing: &Value,
        call: &str,
        now_s: u64,
    ) -> String {
        let plans: Vec<Value> = plans
            .iter()
            .map(|plan| json!([plan.id, plan.created_at]))
            .collect();
        self.seal(json!({
            "t": token, "m": plans, "v": listing, "c": call, "x": now_s + self.timeout.as_secs()
        }))
    }

    /// The `requestState` of a call that sent the owner to confirm with a passkey: its retry asks again, never twice.
    pub(crate) fn sign_step_up(&self, token: &str, call: &str, now_s: u64) -> String {
        self.seal(json!({ "t": token, "s": true, "c": call, "x": now_s + self.timeout.as_secs() }))
    }

    fn seal(&self, payload: Value) -> String {
        let body = URL_SAFE_NO_PAD.encode(payload.to_string());
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
        if payload["s"] == true {
            return State::StepUp;
        }
        if let (Some(plans), Some(listing)) = (payload["m"].as_array(), payload.get("v"))
            && listing.is_object()
        {
            let plans: Option<Vec<(String, u64)>> = plans
                .iter()
                .map(|plan| Some((plan[0].as_str()?.to_string(), plan[1].as_u64()?)))
                .collect();
            return match plans {
                Some(plans) => State::Many {
                    plans,
                    listing: listing.clone(),
                },
                None => State::Invalid,
            };
        }
        match (payload["p"].as_str(), payload.get("v")) {
            (Some(plan), Some(proposal)) if proposal.is_object() => State::Plan {
                plan: plan.to_string(),
                proposal: proposal.clone(),
            },
            _ => State::Invalid,
        }
    }
}

/// The elicitation modes a client declared.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Modes {
    pub(crate) form: bool,
    pub(crate) url: bool,
}

/// A `requestState` as [`Terminal::verify`] reads it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum State {
    Plan {
        plan: String,
        proposal: Value,
    },
    /// Several plans asked at once: each one's id and creation time, and the listing the call answered.
    Many {
        plans: Vec<(String, u64)>,
        listing: Value,
    },
    /// The owner was sent to confirm with a passkey.
    StepUp,
    Expired,
    Invalid,
}

/// Whether a `capabilities` object declares elicitation. `{"elicitation":{}}` is form mode, which is all this asks for.
pub(crate) fn declares_elicitation(capabilities: Option<&Value>) -> bool {
    // `{}` is form mode; a client that declared `url` alone cannot be sent a form.
    capabilities
        .and_then(|c| c.get("elicitation"))
        .and_then(Value::as_object)
        .is_some_and(|modes| modes.is_empty() || modes.contains_key("form"))
}

/// Whether a `capabilities` object declares URL-mode elicitation.
pub(crate) fn declares_url(capabilities: Option<&Value>) -> bool {
    capabilities
        .and_then(|c| c.get("elicitation"))
        .and_then(Value::as_object)
        .is_some_and(|modes| modes.contains_key("url"))
}

/// The `elicitation/create` params that send the owner to `url` to confirm with a passkey (URL mode).
pub(crate) fn url_params(message: &str, url: &str, elicitation_id: &str) -> Value {
    json!({
        "mode": "url",
        "message": message,
        "url": url,
        "elicitationId": elicitation_id
    })
}

/// The words of the URL-mode request that sends the owner to confirm with a passkey.
pub const STEP_UP_MESSAGE: &str = "Plans are waiting for your approval. Confirm it is you with your passkey on Arca's page first; this session can then ask you about each plan for the next 12 hours.";

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
