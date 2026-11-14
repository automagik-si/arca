//! Each pool's sign-in state: whether its last exchange with GitHub failed for a credential reason. Arca's alerts read
//! it on every tick (rule `runner-signin`), so a pool that cannot sign in is seen, not left retrying in silence.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// Why a pool cannot sign in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SignInReason {
    /// 401: a bad key, client ID or token, or a revoked one.
    BadCredentials,
    /// 404 on the organisation's runner registration: the App is not installed there, or was uninstalled.
    AppNotInstalled,
    /// 403: the credential lacks the "Self-hosted runners" permission.
    Forbidden,
    /// The kernel holds no credential for the pool's binding.
    NoCredential,
}

impl SignInReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BadCredentials => "bad-credentials",
            Self::AppNotInstalled => "app-not-installed",
            Self::Forbidden => "forbidden",
            Self::NoCredential => "no-credential",
        }
    }
}

/// One pool's state, as of `at_ms`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignIn {
    /// `None` when the last exchange that says anything about the credential worked.
    pub failing: Option<SignInReason>,
    pub at_ms: u64,
}

/// The board every pool writes its state on and the alerts read. Clones share it.
#[derive(Debug, Clone, Default)]
pub struct SignInBoard(Arc<Mutex<BTreeMap<String, SignIn>>>);

impl SignInBoard {
    pub fn new() -> Self {
        Self::default()
    }

    /// The pool's last exchange signed in.
    pub fn ok(&self, pool: &str) {
        self.set(pool, None);
    }

    /// The pool's last exchange failed for this reason.
    pub fn failing(&self, pool: &str, reason: SignInReason) {
        self.set(pool, Some(reason));
    }

    fn set(&self, pool: &str, failing: Option<SignInReason>) {
        if let Ok(mut board) = self.0.lock() {
            board.insert(
                pool.to_string(),
                SignIn {
                    failing,
                    at_ms: crate::time::now_ms(),
                },
            );
        }
    }

    /// Every pool's state, by pool name.
    pub fn read(&self) -> BTreeMap<String, SignIn> {
        self.0.lock().map(|board| board.clone()).unwrap_or_default()
    }

    /// Forgets a pool that no longer exists.
    pub fn remove(&self, pool: &str) {
        if let Ok(mut board) = self.0.lock() {
            board.remove(pool);
        }
    }
}
