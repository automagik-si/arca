//! The spend-once check: the order of droidd's `owner-approvals.ts`, with the signature verifier and the nonce store as seams.

use std::collections::HashMap;

use arca_kernel::canon::params_digest;
use serde_json::Value;

use crate::statement::{ArcaAction, iso_ms, parse_statement, split_approval, valid_target};
use crate::{ARCA_NAMESPACE, ApprovalRefusal, ISSUE_SKEW_MS, TTL_MAX_MS};

/// Checks a signature over a statement's bytes in a namespace, and names the signing key on a pass.
pub trait SignatureVerifier: Send + Sync {
    /// Returns the signing key's fingerprint when the signature verifies in `namespace`, else a refusal.
    fn verify(
        &self,
        namespace: &str,
        statement: &[u8],
        signature: &[u8],
    ) -> Result<String, ApprovalRefusal>;
}

/// The verifier until one is installed: every well-formed approval is refused with [`ApprovalRefusal::NoVerifier`].
#[derive(Debug, Clone, Copy, Default)]
pub struct RefuseAll;

impl SignatureVerifier for RefuseAll {
    fn verify(
        &self,
        namespace: &str,
        statement: &[u8],
        signature: &[u8],
    ) -> Result<String, ApprovalRefusal> {
        let _ = (namespace, statement, signature);
        Err(ApprovalRefusal::NoVerifier)
    }
}

/// The nonces spent so far. A nonce is spent once, and never again.
pub trait NonceStore: Send {
    /// Whether `nonce` has been spent.
    fn spent(&self, nonce: &str) -> bool;
    /// Spends `nonce` at `at_ms`, or refuses with [`ApprovalRefusal::Replayed`] when it is already spent.
    fn spend(&mut self, nonce: &str, at_ms: u64) -> Result<(), ApprovalRefusal>;
}

/// An in-memory nonce store.
#[derive(Debug, Default)]
pub struct MemNonces {
    spent: HashMap<String, u64>,
}

impl NonceStore for MemNonces {
    fn spent(&self, nonce: &str) -> bool {
        self.spent.contains_key(nonce)
    }

    fn spend(&mut self, nonce: &str, at_ms: u64) -> Result<(), ApprovalRefusal> {
        if self.spent.contains_key(nonce) {
            return Err(ApprovalRefusal::Replayed);
        }
        self.spent.insert(nonce.to_string(), at_ms);
        Ok(())
    }
}

/// What a spent approval admits: its nonce, and the fingerprint of the key that signed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spent {
    pub nonce: String,
    pub key: String,
}

/// Checks an approval for one call and spends it. No nonce is spent on any refusal; a counting passkey's sign count is advanced
/// once its signature verifies, before the bound fields and the time window are checked (see `PasskeyVerifier`).
pub struct ApprovalVerifier<V: SignatureVerifier, N: NonceStore> {
    host: String,
    verifier: V,
    nonces: N,
}

impl<V: SignatureVerifier, N: NonceStore> ApprovalVerifier<V, N> {
    /// A verifier for the host named `host`.
    pub fn new(host: String, verifier: V, nonces: N) -> Self {
        Self {
            host,
            verifier,
            nonces,
        }
    }

    /// Checks `approval` for `action` on `target` with the call's `args` (the digest covers all of them but `approval`), at
    /// `now_ms`, and spends it. The order is droidd's: the call's own inputs, the approval's parse, the signature, the bound
    /// fields, the time window, and last the nonce. The nonce is read only after the signature has verified.
    pub fn check(
        &mut self,
        action: ArcaAction,
        target: &str,
        args: &Value,
        approval: &str,
        now_ms: u64,
    ) -> Result<Spent, ApprovalRefusal> {
        // No approval can name a target the grammar refuses, so such a call is refused as the wrong target.
        if !valid_target(target) {
            return Err(ApprovalRefusal::WrongTarget);
        }
        let digest = params_digest(args).map_err(|_| {
            ApprovalRefusal::Malformed("the call's arguments are not a JSON object".to_string())
        })?;
        let (statement, signature) = split_approval(approval)?;
        let parsed = parse_statement(&statement)?;
        let key = self
            .verifier
            .verify(ARCA_NAMESPACE, &statement, &signature)?;

        if parsed.host != self.host {
            return Err(ApprovalRefusal::WrongHost);
        }
        if parsed.action != action {
            return Err(ApprovalRefusal::WrongAction);
        }
        if parsed.target != target {
            return Err(ApprovalRefusal::WrongTarget);
        }
        if parsed.params_digest != digest {
            return Err(ApprovalRefusal::WrongParams);
        }

        let now = i128::from(now_ms);
        let issued = window_ms(&parsed.issued_at)?;
        let expires = window_ms(&parsed.expires_at)?;
        let skew = i128::from(ISSUE_SKEW_MS);
        let ttl = i128::from(TTL_MAX_MS);
        if issued > now + skew {
            return Err(ApprovalRefusal::IssuedAhead);
        }
        if expires <= issued || expires - issued > ttl {
            return Err(ApprovalRefusal::TtlTooLong);
        }
        if expires <= now {
            return Err(ApprovalRefusal::Expired);
        }
        if expires - now > ttl {
            return Err(ApprovalRefusal::TtlTooLong);
        }

        if self.nonces.spent(&parsed.nonce) {
            return Err(ApprovalRefusal::Replayed);
        }
        self.nonces.spend(&parsed.nonce, now_ms)?;
        Ok(Spent {
            nonce: parsed.nonce,
            key,
        })
    }
}

/// A statement's time as epoch milliseconds. The parse has already checked its form, so the refusal is a guard only.
fn window_ms(text: &str) -> Result<i128, ApprovalRefusal> {
    iso_ms(text).map(i128::from).ok_or_else(|| {
        ApprovalRefusal::Malformed("the statement's time is not toISOString form".to_string())
    })
}
