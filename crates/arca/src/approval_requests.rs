//! Owner approval requests: an agent asks for the owner's approval of one Arca action, the kernel holds the request, the
//! owner approves it on Arca's passkey page with one tap, and the agent reads the approval string with
//! `owner_approval_status` and hands it to the call it approves (`service_remove`, `service_publish`), which checks it in
//! full and spends it. The behaviour reference is droidd's `owner-plugins/approval-requests.ts`.
//!
//! Tools:
//! - `owner_approval_request` (doctor scope): `{kind: "arca", action, target, args}`. The kernel builds the request's subject
//!   (`action`, `target`, and the `paramsDigest` of `args`, at most 4 KiB of JSON, no `approval` key, no control or bidi
//!   character and no string that reads as an approval, shown to the owner as data). A kind `arca` request without an
//!   action is refused as droidd refuses it: `an arca request needs action`. An identical pending request of the same token
//!   is the same request; beyond 5 pending per token or 20 in all the call is refused (`rate`, "429" in its message).
//!   Kind `upgrade` is refused with `upgrade_not_here`: an identity phone's upgrade step is approved through droidd's MCP.
//!   Action `enroll` asks the owner to let a service on this node make the person showing a code one of its owners: the
//!   target is that code (6 to 12 uppercase letters and digits) and the args are `{service: <name>}`, nothing else.
//! - `enroll_confirm {target, args, approval}` (doctor scope): spends an `enroll` approval. The kernel checks it with its
//!   own verifier against `enroll`, the code and the args' digest, spends its nonce once and answers `{confirmed: true}`,
//!   or refuses with `owner_approval`. Only the token whose request the owner approved spends it, and only an approval
//!   a request holds: a statement built and signed outside a request confirms nothing, whoever signed it. An `enroll` approval changes nothing in the kernel: the service that asked acts on
//!   the answer.
//! - `owner_approval_status {id, waitMs?}` (doctor scope, not read-only): the request's state, waiting up to 30 s while it is
//!   pending. The approval string goes to the token that asked, and to no other.
//! - `owner_approval_list` (viewer scope, read-only, untrusted): the last 24 h of requests, every pending one first, then the
//!   newest others, 50 in all; never an approval string.
//!
//! Page (see `crate::approve_page`): `begin` mints the statement the passkey signs (the request's fields, this kernel's host,
//! a fresh nonce, issued now, valid 15 minutes) and keeps it on the request; `approve`, within 120 s of `begin`, takes the
//! request id, that statement and the passkey's proof, checks the statement in full against the request (fields, digest,
//! host, window, an unused nonce no other request holds), then the proof as the spend will check it
//! (`ApprovalVerifier::preview`: nothing spent, no sign count advanced), and marks the request approved with its approval
//! string; `deny` needs no passkey, since it only takes away.
//!
//! Expiry: a pending request lives 60 minutes; an approval lives as its statement does (15 minutes). The approval string is
//! dropped once its nonce is spent or its statement has expired (the request is then `expired`), on every tool call and page
//! route that reads the requests.
//!
//! Files, both in the state directory, mode 0600: `approval-requests.json`, the requests (written whole, then renamed), and
//! `approval-requests-audit.jsonl`, one line per request made, approved or denied. No approval string, proof, credential id
//! or challenge reaches the audit file, a log line or an error; logs keep nonce and key-fingerprint prefixes only.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use arca_approval::{
    ARCA_NAMESPACE, ArcaAction, ISSUE_SKEW_MS, TTL_MAX_MS, challenge_for, iso_ms, parse_proof,
    parse_statement, split_approval, valid_enroll_code,
};
use arca_kernel::canon::{canonical_json, params_digest};
use arca_mcp::{CallError, Caller};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::handlers::iso_utc;
use crate::kernel::Kernel;

/// The requests file in the state directory.
pub const REQUESTS_FILE: &str = "approval-requests.json";
/// The requests' audit file in the state directory.
pub const AUDIT_FILE: &str = "approval-requests-audit.jsonl";
/// How long a pending request lives.
pub const REQUEST_TTL_MS: u64 = 60 * 60 * 1000;
/// How long a begun statement may wait for its approval.
pub const BEGUN_MAX_MS: u64 = 120_000;
/// How long an approval minted on the page is valid: the longest an approval may be.
pub const APPROVAL_TTL_MS: u64 = TTL_MAX_MS;
/// Pending requests per token, and in all.
pub const PENDING_PER_TOKEN: usize = 5;
pub const PENDING_PER_KERNEL: usize = 20;
/// Bytes of JSON a request's `args` may hold.
pub const ARGS_MAX: usize = 4096;
/// The longest top-level args key, and the longest requester name: the bounds `owner_approval_list`'s result carries.
pub const ARGS_KEY_MAX: usize = 64;
pub const REQUESTER_MAX: usize = 200;
/// The longest passkey page address a request names.
pub const DASHBOARD_MAX: usize = 300;
/// The longest `owner_approval_status` waits.
pub const WAIT_MAX_MS: u64 = 30_000;
/// The audit tools of a request's life.
pub const REQUESTED_AUDIT: &str = "owner_approval_requested";
pub const APPROVED_AUDIT: &str = "owner_approval_approved";
pub const DENIED_AUDIT: &str = "owner_approval_denied";
/// The audit tool of an `enroll` approval spent through `enroll_confirm`.
pub const ENROLLED_AUDIT: &str = "owner_enroll_confirmed";
/// The action whose approval changes nothing in the kernel: `enroll_confirm` spends it for the service that asked.
pub const ENROLL: &str = "enroll";
/// The principal the page's decisions are audited as: the owner, proved by the passkey for an approval.
pub const PAGE_PRINCIPAL: &str = "page:owner";
/// The actions a request may ask for: droidd's `ARCA_ACTIONS`, and `enroll`.
const ACTIONS: [&str; 4] = ["delete", "publish", "spend", ENROLL];
/// The keys of an upgrade request, which a kind `arca` request never takes.
const UPGRADE_KEYS: [&str; 8] = [
    "droid",
    "step",
    "fromFp",
    "toFp",
    "look",
    "eyes",
    "openWhatsApp",
    "confirmSnapshotLoss",
];
/// The keys a kind `arca` request needs, in droidd's order.
const ARCA_KEYS: [&str; 3] = ["action", "target", "args"];
const LIST_MS: u64 = 24 * 60 * 60 * 1000;
const LIST_MAX: usize = 50;
const ARGS_DEPTH: usize = 8;
/// `O_NOFOLLOW`: the audit file is never opened through a link.
const NOFOLLOW: i32 = 0o400_000;
/// The largest requests file this build reads. A larger one is set aside. The sweep keeps the file far below it (at most 20
/// pending rows and 50 decided ones, each under 25 KiB), so the rows it cannot drop, approvals not yet spent, are never
/// set aside for the file's size.
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;

/// A request's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RequestState {
    Pending,
    Approved,
    Denied,
    Expired,
}

impl RequestState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Denied => "denied",
            Self::Expired => "expired",
        }
    }
}

/// One request as the file keeps it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Row {
    pub id: String,
    pub kind: String,
    /// The fields its statement carries: `action`, `target`, `paramsDigest`. Every digest a statement or an audit row names
    /// is taken from here.
    pub subject: Map<String, Value>,
    /// The call's arguments the digest is over, as the agent gave them.
    pub args: Value,
    /// The digest as a column of its own. Nothing reads it back to mint or audit (droidd's review L3).
    pub params_digest: String,
    /// The token that asked, and its scope.
    pub requester: String,
    pub requester_kind: String,
    pub created_at: u64,
    pub expires_at: u64,
    pub state: RequestState,
    /// The statement the last `begin` minted, and when.
    pub statement: Option<String>,
    pub begun_at: Option<u64>,
    /// The approval string, until its nonce is spent or its statement expires.
    pub approval: Option<String>,
    pub approval_expires_at: Option<u64>,
    pub decided_at: Option<u64>,
    /// Raised by every change, so a decision made on a row that changed since it was read is refused as stale.
    #[serde(default)]
    pub version: u64,
}

impl Row {
    fn field(&self, key: &str) -> &str {
        self.subject.get(key).and_then(Value::as_str).unwrap_or("")
    }

    /// The request in words for a log line: `delete nats`.
    fn subject_line(&self) -> String {
        format!("{} {}", self.field("action"), self.field("target"))
    }

    /// The request in an audit outcome: kind, action, target, the subject's digest and the requester.
    fn outcome(&self) -> String {
        format!(
            "arca:{}:{}:params={}:requester={}",
            self.field("action"),
            self.field("target"),
            self.field("paramsDigest"),
            self.requester
        )
    }
}

/// A page refusal: its HTTP status, a code the page reads, words, and any extra fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageRefusal {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
    pub extra: Map<String, Value>,
}

impl PageRefusal {
    fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            extra: Map::new(),
        }
    }
}

/// A test seam: called with the kernel and the request id after a page decision's checks and before its write.
pub type DecideHook = Box<dyn Fn(&Kernel, &str) + Send + Sync>;

struct Inner {
    rows: Vec<Row>,
    audit: File,
}

/// The kernel's owner approval requests.
pub struct ApprovalRequests {
    inner: Mutex<Inner>,
    changed: Condvar,
    state_dir: PathBuf,
    dashboard: Option<String>,
    log: Arc<dyn Fn(&str) + Send + Sync>,
    hook: Mutex<Option<DecideHook>>,
}

impl ApprovalRequests {
    /// Opens the requests of `state_dir`. A requests file that is not a regular file, is too large or does not parse is set
    /// aside as `<name>.bad-<ms>` and the kernel starts with no requests, so this file never stops a start. `dashboard` is
    /// the passkey page's address, when the node serves one.
    pub fn open(state_dir: &Path, dashboard: Option<String>, now_ms: u64) -> io::Result<Self> {
        let path = state_dir.join(REQUESTS_FILE);
        let rows = match read_rows(&path) {
            Ok(rows) => rows,
            Err(error) => {
                if error.kind() != io::ErrorKind::NotFound {
                    let aside = state_dir.join(format!("{REQUESTS_FILE}.bad-{now_ms}"));
                    eprintln!(
                        "arca: the approval requests file was set aside ({}); no request is kept",
                        error.kind()
                    );
                    let _ = fs::rename(&path, aside);
                }
                Vec::new()
            }
        };
        // owner_approval_request's result holds a dashboard of at most 300 characters; a longer page address is left out
        // rather than failing every request.
        let dashboard = dashboard.filter(|address| {
            let fits = address.chars().count() <= DASHBOARD_MAX;
            if !fits {
                eprintln!(
                    "arca: the passkey page address is longer than {DASHBOARD_MAX} characters; requests name no dashboard"
                );
            }
            fits
        });
        let audit = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .custom_flags(NOFOLLOW)
            .open(state_dir.join(AUDIT_FILE))?;
        Ok(Self {
            inner: Mutex::new(Inner { rows, audit }),
            changed: Condvar::new(),
            state_dir: state_dir.to_path_buf(),
            dashboard,
            log: Arc::new(|line| eprintln!("{line}")),
            hook: Mutex::new(None),
        })
    }

    /// Replaces the log sink; tests capture the lines.
    pub fn with_log(mut self, log: Arc<dyn Fn(&str) + Send + Sync>) -> Self {
        self.log = log;
        self
    }

    /// Sets the hook a page decision calls between its checks and its write. Tests only.
    #[doc(hidden)]
    pub fn set_decide_hook(&self, hook: Option<DecideHook>) {
        if let Ok(mut slot) = self.hook.lock() {
            *slot = hook;
        }
    }

    /// One request as the page and the list show it, after the sweep; `None` for an unknown id.
    pub fn view(&self, kernel: &Kernel, id: &str, now: u64) -> Option<Value> {
        let mut inner = self.lock().ok()?;
        self.sweep(&mut inner, kernel, now);
        inner
            .rows
            .iter()
            .find(|row| row.id == id)
            .map(|row| view_of(row, kernel.host()))
    }

    /// The last 24 h of requests, newest first, after the sweep: every pending one, then the newest others, 50 in all.
    pub fn list(&self, kernel: &Kernel, now: u64) -> Result<Vec<Value>, CallError> {
        let mut inner = self.lock()?;
        self.sweep(&mut inner, kernel, now);
        let since = now.saturating_sub(LIST_MS);
        let mut recent: Vec<&Row> = inner
            .rows
            .iter()
            .filter(|row| row.created_at >= since)
            .collect();
        recent.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(a.id.cmp(&b.id)));
        let pending = recent
            .iter()
            .filter(|row| row.state == RequestState::Pending)
            .count();
        let mut others = LIST_MAX.saturating_sub(pending);
        let kept: Vec<Value> = recent
            .into_iter()
            .filter(|row| {
                if row.state == RequestState::Pending {
                    return true;
                }
                if others == 0 {
                    return false;
                }
                others -= 1;
                true
            })
            .take(LIST_MAX)
            .map(|row| view_of(row, kernel.host()))
            .collect();
        Ok(kept)
    }

    /// `owner_approval_request`: checks the request's fields as its statement will carry them, then returns the pending
    /// request it names (the same one for an identical pending request of the same token).
    pub fn request(
        &self,
        kernel: &Kernel,
        args: &Value,
        caller: &Caller,
        now: u64,
    ) -> Result<Value, CallError> {
        let (subject, call_args, digest) = fields_of(args, kernel.host(), now)?;
        if caller.token_id.chars().count() > REQUESTER_MAX {
            return Err(invalid(format!(
                "the token's name is longer than {REQUESTER_MAX} characters, more than the owner's list shows"
            )));
        }
        let mut inner = self.lock()?;
        self.sweep(&mut inner, kernel, now);
        if let Some(same) = inner.rows.iter().find(|row| {
            row.state == RequestState::Pending
                && row.kind == "arca"
                && row.subject == subject
                && row.requester == caller.token_id
        }) {
            return Ok(self.created(same));
        }
        let pending = |row: &&Row| row.state == RequestState::Pending;
        let mine = inner
            .rows
            .iter()
            .filter(pending)
            .filter(|row| row.requester == caller.token_id)
            .count();
        // Both caps are `rate` refusals: what is full is the caller's own queue to the owner, not the node, and waiting
        // ends it. The wait told is the longest it can be, until the first of the waiting requests expires; one that
        // is decided makes room sooner.
        let first_to_expire = |rows: &mut dyn Iterator<Item = &Row>| {
            rows.map(|row| row.expires_at)
                .min()
                .map(|at| arca_api::refusals::seconds_until(at, now))
        };
        if mine >= PENDING_PER_TOKEN {
            let wait = first_to_expire(
                &mut inner
                    .rows
                    .iter()
                    .filter(pending)
                    .filter(|row| row.requester == caller.token_id),
            );
            return Err(CallError::rate(
                format!(
                    "too many pending requests (429): {} has {PENDING_PER_TOKEN} waiting for the owner; wait for one to be decided or to expire",
                    caller.token_id
                ),
                wait,
            ));
        }
        if inner.rows.iter().filter(pending).count() >= PENDING_PER_KERNEL {
            let wait = first_to_expire(&mut inner.rows.iter().filter(pending));
            return Err(CallError::rate(
                format!(
                    "too many pending requests (429): Arca has {PENDING_PER_KERNEL} waiting for the owner; wait for one to be decided or to expire"
                ),
                wait,
            ));
        }
        let row = Row {
            id: request_id()?,
            kind: "arca".to_string(),
            subject,
            args: call_args,
            params_digest: digest,
            requester: caller.token_id.clone(),
            requester_kind: caller.scope.as_str().to_string(),
            created_at: now,
            expires_at: now.saturating_add(REQUEST_TTL_MS),
            state: RequestState::Pending,
            statement: None,
            begun_at: None,
            approval: None,
            approval_expires_at: None,
            decided_at: None,
            version: 0,
        };
        // Everything a request keeps must come back through its own list: one the list's result schema refuses would fail
        // every owner_approval_list call while it is in the list's window.
        if let Err(why) = listable(&row, kernel.host()) {
            return Err(invalid(format!(
                "the request could not be shown in the owner's list as it is ({why})"
            )));
        }
        let principal = format!("{}:{}", row.requester_kind, row.requester);
        let outcome = format!("requested:{}:{}", row.id, row.outcome());
        let before = inner.rows.clone();
        inner.rows.push(row.clone());
        self.commit(
            &mut inner,
            before,
            now,
            &principal,
            REQUESTED_AUDIT,
            &outcome,
        )
        .map_err(|_| internal())?;
        (self.log)(&format!(
            "arca: approval request {} ({}) by {}",
            row.id,
            row.subject_line(),
            row.requester
        ));
        Ok(self.created(&row))
    }

    fn created(&self, row: &Row) -> Value {
        json!({
            "id": row.id,
            "state": "pending",
            "expiresAt": iso_utc(row.expires_at),
            "dashboard": self.dashboard,
        })
    }

    /// `owner_approval_status`: the request's state, after waiting up to `wait_ms` while it is pending. The kernel is locked
    /// only to read, never while waiting. The approval string goes to the token that asked.
    pub fn status(
        &self,
        kernel: &Mutex<Kernel>,
        id: &str,
        wait_ms: u64,
        caller: &Caller,
        clock: &(dyn Fn() -> u64 + Send + Sync),
    ) -> Result<Value, CallError> {
        let pending = {
            let kernel = kernel.lock().map_err(|_| internal())?;
            let mut inner = self.lock()?;
            self.sweep(&mut inner, &kernel, clock());
            find(&inner.rows, id)?.state == RequestState::Pending
        };
        let wait = wait_ms.min(WAIT_MAX_MS);
        if pending && wait > 0 {
            let until = Instant::now() + Duration::from_millis(wait);
            let mut inner = self.lock()?;
            loop {
                // A begin changes the row and leaves it pending: only a decision (or the end of the wait) answers.
                let moved = inner
                    .rows
                    .iter()
                    .find(|row| row.id == id)
                    .is_none_or(|row| row.state != RequestState::Pending);
                let left = until.saturating_duration_since(Instant::now());
                if moved || left.is_zero() {
                    break;
                }
                inner = self
                    .changed
                    .wait_timeout(inner, left)
                    .map_err(|_| internal())?
                    .0;
            }
        }
        let kernel = kernel.lock().map_err(|_| internal())?;
        let mut inner = self.lock()?;
        self.sweep(&mut inner, &kernel, clock());
        let row = find(&inner.rows, id)?;
        let mut out = json!({
            "id": row.id,
            "state": row.state.as_str(),
            "expiresAt": iso_utc(row.expires_at),
        });
        if row.state == RequestState::Approved
            && let Some(at) = row.approval_expires_at
        {
            out["approvalExpiresAt"] = json!(iso_utc(at));
        }
        if let Some(approval) = &row.approval
            && row.requester == caller.token_id
        {
            out["approval"] = json!(approval);
        }
        Ok(out)
    }

    /// `enroll_confirm`: checks an `enroll` approval against the code and the args the caller names, with the kernel's own
    /// verifier, and spends its nonce. Nothing else in the kernel changes. The approval must be the one an approved
    /// `enroll` request of this same token holds, unspent and unexpired: another token's, or one that came from no
    /// request, is refused before the verifier is asked and spends nothing. A confirmation that cannot be audited is not
    /// given: the approval is spent and the owner approves again.
    pub fn enroll_confirm(
        &self,
        kernel: &mut Kernel,
        args: &Value,
        caller: &Caller,
        now: u64,
    ) -> Result<Value, CallError> {
        let Some(input) = args.as_object() else {
            return Err(invalid("the arguments are not a JSON object"));
        };
        if let Some(key) = input
            .keys()
            .find(|key| !["target", "args", "approval"].contains(&key.as_str()))
        {
            let key = clean_key(key);
            return Err(invalid(format!("enroll_confirm takes no {key}")));
        }
        let target = input.get("target").and_then(Value::as_str).unwrap_or("");
        let call_args = input.get("args").unwrap_or(&Value::Null);
        check_enroll(target, call_args)?;
        let approval = input
            .get("approval")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("approval: expected a string"))?;
        let mut inner = self.lock()?;
        self.sweep(&mut inner, kernel, now);
        let Some(row) = inner
            .rows
            .iter()
            .find(|row| {
                row.state == RequestState::Approved
                    && row.kind == "arca"
                    && row.field("action") == ENROLL
                    && row.requester == caller.token_id
                    && row.approval.as_deref() == Some(approval)
            })
            .cloned()
        else {
            (self.log)(&format!(
                "arca: enroll_confirm by {} was refused: no approved enroll request of this token holds that approval",
                caller.token_id
            ));
            return Err(refused(
                "owner_approval",
                "this is not the approval of an enroll request this token made and the owner approved: it was asked by another token, came from no request, is spent or has expired",
            ));
        };
        kernel
            .check_approval(ArcaAction::Enroll, target, call_args, approval, now)
            .map_err(|refusal| {
                (self.log)(&format!(
                    "arca: enroll_confirm by {} was refused: {}",
                    caller.token_id,
                    crate::handlers::refusal_code(&refusal)
                ));
                refused("owner_approval", refusal.to_string())
            })?;
        // The spent approval's request loses its approval string now.
        self.sweep(&mut inner, kernel, now);
        let principal = format!("{}:{}", caller.scope.as_str(), caller.token_id);
        // The request's own subject and requester, as its other audit lines name them.
        let outcome = format!("confirmed:{}:{}", row.id, row.outcome());
        audit(&mut inner.audit, now, &principal, ENROLLED_AUDIT, &outcome)
            .map_err(|_| internal())?;
        (self.log)(&format!(
            "arca: enroll {target} confirmed for {}",
            caller.token_id
        ));
        Ok(json!({ "confirmed": true }))
    }

    /// The page's `begin`: mints the statement the passkey signs for request `id` and keeps it on the request, replacing
    /// any earlier one. `rp_id` is the page's relying party: only the owner's passkeys made for it are offered.
    pub fn begin(
        &self,
        kernel: &Kernel,
        id: &str,
        rp_id: &str,
        now: u64,
    ) -> Result<Value, PageRefusal> {
        let mut inner = self.lock().map_err(|_| page_internal())?;
        self.sweep(&mut inner, kernel, now);
        let row = page_row(&inner.rows, id)?.clone();
        if row.state != RequestState::Pending {
            return Err(not_pending(&row));
        }
        let lines = passkeys_for(kernel, rp_id)?;
        let nonce = crate::handlers::fresh_nonce().map_err(|_| page_internal())?;
        let statement =
            statement_of(&row, kernel.host(), &nonce, now).map_err(|_| page_internal())?;
        let challenge =
            challenge_for(ARCA_NAMESPACE, statement.as_bytes()).map_err(|_| page_internal())?;
        let stored = inner
            .rows
            .iter_mut()
            .find(|kept| kept.id == row.id)
            .ok_or_else(page_internal)?;
        stored.statement = Some(statement.clone());
        stored.begun_at = Some(now);
        stored.version += 1;
        self.persist(&inner).map_err(|_| page_internal())?;
        self.changed.notify_all();
        (self.log)(&format!(
            "arca: approval request {} begun ({}): nonce {}…, {} passkey(s) for {rp_id}",
            row.id,
            row.subject_line(),
            &nonce[..8],
            lines.len()
        ));
        Ok(json!({
            "id": row.id,
            "namespace": ARCA_NAMESPACE,
            "statement": statement,
            "challenge": URL_SAFE_NO_PAD.encode(challenge),
            "rpId": rp_id,
            "allowCredentials": lines
                .iter()
                .map(|line| json!({ "id": URL_SAFE_NO_PAD.encode(&line.credential_id) }))
                .collect::<Vec<_>>(),
            "timeoutMs": BEGUN_MAX_MS,
        }))
    }

    /// The page's `approve`: request `id` approved with the passkey's `proof` over `statement`, the statement `begin` minted
    /// for it. The statement is checked in full against the request first, then that it is the begun one, then the proof as
    /// the spend will check it, with nothing spent. The approval string is kept for the requester; the page gets the state.
    pub fn approve(
        &self,
        kernel: &Kernel,
        id: &str,
        statement: &str,
        proof: &Value,
        now: u64,
    ) -> Result<Value, PageRefusal> {
        let row = {
            let mut inner = self.lock().map_err(|_| page_internal())?;
            self.sweep(&mut inner, kernel, now);
            let row = page_row(&inner.rows, id)?.clone();
            if row.state != RequestState::Pending {
                return Err(not_pending(&row));
            }
            check_against(&inner.rows, &row, kernel, statement, now)?;
            row
        };
        let proof_bytes = canonical_json(proof).map_err(|_| finish_shape())?;
        let parsed = parse_proof(proof_bytes.as_bytes()).map_err(|_| finish_shape())?;
        let (Some(begun), Some(begun_at)) = (&row.statement, row.begun_at) else {
            return Err(PageRefusal::new(
                409,
                "not_begun",
                "the request was not begun: tap Approve again",
            ));
        };
        if now.saturating_sub(begun_at) > BEGUN_MAX_MS {
            return Err(PageRefusal::new(
                409,
                "begin_expired",
                format!(
                    "more than {} s passed since begin: tap Approve again",
                    BEGUN_MAX_MS / 1000
                ),
            ));
        }
        if begun != statement {
            return Err(PageRefusal::new(
                409,
                "stale",
                "the request was begun again since: tap Approve again",
            ));
        }
        let approval = format!(
            "{}.{}",
            STANDARD.encode(statement),
            STANDARD.encode(&proof_bytes)
        );
        let action = action_of(&row).ok_or_else(page_internal)?;
        let spent = kernel
            .preview_approval(action, row.field("target"), &row.args, &approval, now)
            .map_err(|refusal| {
                let why = refusal.to_string();
                (self.log)(&format!(
                    "arca: approval request {}: the passkey was refused: {}",
                    row.id,
                    crate::handlers::refusal_code(&refusal)
                ));
                PageRefusal::new(403, "assertion_refused", why)
            })?;
        let expires = parse_statement(statement.as_bytes())
            .ok()
            .and_then(|s| iso_ms(&s.expires_at))
            .and_then(|ms| u64::try_from(ms).ok())
            .ok_or_else(page_internal)?;
        let data = &parsed.authenticator_data;
        let count = data
            .get(33..37)
            .map_or(0, |b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
        let synced = data.get(32).is_some_and(|flags| flags & 0x10 != 0);
        self.call_hook(kernel, &row.id);
        let mut inner = self.lock().map_err(|_| page_internal())?;
        let current = page_row(&inner.rows, id)?;
        if current.version != row.version || current.state != RequestState::Pending {
            return Err(stale());
        }
        let outcome = format!(
            "approved:{}:{}:key={}:via=passkey:count={count}:synced={}",
            row.id,
            row.outcome(),
            spent.key,
            u8::from(synced)
        );
        let before = inner.rows.clone();
        let stored = inner
            .rows
            .iter_mut()
            .find(|kept| kept.id == row.id)
            .ok_or_else(page_internal)?;
        stored.state = RequestState::Approved;
        stored.approval = Some(approval);
        stored.approval_expires_at = Some(expires);
        stored.decided_at = Some(now);
        stored.version += 1;
        self.commit(
            &mut inner,
            before,
            now,
            PAGE_PRINCIPAL,
            APPROVED_AUDIT,
            &outcome,
        )
        .map_err(|_| page_internal())?;
        self.changed.notify_all();
        (self.log)(&format!(
            "arca: approval request {} approved ({}): nonce {}…, key {}…, valid until {}",
            row.id,
            row.subject_line(),
            &spent.nonce[..8],
            spent.key.chars().take(24).collect::<String>(),
            iso_utc(expires)
        ));
        Ok(json!({ "state": "approved", "approvalExpiresAt": iso_utc(expires) }))
    }

    /// The page's `deny`: final, audited, and needing no passkey.
    pub fn deny(&self, kernel: &Kernel, id: &str, now: u64) -> Result<Value, PageRefusal> {
        let row = {
            let mut inner = self.lock().map_err(|_| page_internal())?;
            self.sweep(&mut inner, kernel, now);
            let row = page_row(&inner.rows, id)?.clone();
            if row.state != RequestState::Pending {
                return Err(not_pending(&row));
            }
            row
        };
        self.call_hook(kernel, &row.id);
        let mut inner = self.lock().map_err(|_| page_internal())?;
        let current = page_row(&inner.rows, id)?;
        if current.version != row.version || current.state != RequestState::Pending {
            return Err(stale());
        }
        let outcome = format!("denied:{}:{}", row.id, row.outcome());
        let before = inner.rows.clone();
        let stored = inner
            .rows
            .iter_mut()
            .find(|kept| kept.id == row.id)
            .ok_or_else(page_internal)?;
        stored.state = RequestState::Denied;
        stored.statement = None;
        stored.begun_at = None;
        stored.decided_at = Some(now);
        stored.version += 1;
        self.commit(
            &mut inner,
            before,
            now,
            PAGE_PRINCIPAL,
            DENIED_AUDIT,
            &outcome,
        )
        .map_err(|_| page_internal())?;
        self.changed.notify_all();
        (self.log)(&format!(
            "arca: approval request {} denied ({})",
            row.id,
            row.subject_line()
        ));
        Ok(json!({ "state": "denied" }))
    }

    fn call_hook(&self, kernel: &Kernel, id: &str) {
        if let Ok(hook) = self.hook.lock()
            && let Some(hook) = hook.as_ref()
        {
            hook(kernel, id);
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, Inner>, CallError> {
        self.inner.lock().map_err(|_| internal())
    }

    /// A pending request past its 60 minutes is `expired`; an approved one whose nonce is spent keeps its state and loses its
    /// approval string; one unspent past its statement's expiry is `expired` and loses it too. A statement that no longer
    /// parses holds no approval worth keeping.
    fn sweep(&self, inner: &mut Inner, kernel: &Kernel, now: u64) {
        let mut changed = false;
        for row in &mut inner.rows {
            if row.state == RequestState::Pending && row.expires_at <= now {
                row.state = RequestState::Expired;
                row.statement = None;
                row.begun_at = None;
                row.decided_at = Some(now);
                row.version += 1;
                changed = true;
                continue;
            }
            if row.approval.is_none() {
                continue;
            }
            let spent = row
                .statement
                .as_deref()
                .and_then(|text| parse_statement(text.as_bytes()).ok())
                .is_none_or(|s| kernel.approval_nonce_spent(&s.nonce));
            if spent {
                row.approval = None;
            } else if row.approval_expires_at.unwrap_or(0) <= now {
                row.state = RequestState::Expired;
                row.approval = None;
            } else {
                continue;
            }
            row.version += 1;
            changed = true;
        }
        // Keep every pending request and every approval not yet spent; of the rest, the newest LIST_MAX of the last 24 h,
        // which is all the list can show. The file stays bounded, and nothing it drops was still worth anything.
        let live = |row: &Row| row.state == RequestState::Pending || row.approval.is_some();
        let since = now.saturating_sub(LIST_MS);
        let mut others: Vec<(u64, String)> = inner
            .rows
            .iter()
            .filter(|row| !live(row) && row.created_at >= since)
            .map(|row| (row.created_at, row.id.clone()))
            .collect();
        others.sort_by(|a, b| b.cmp(a));
        others.truncate(LIST_MAX);
        let count = inner.rows.len();
        inner
            .rows
            .retain(|row| live(row) || others.iter().any(|(_, id)| *id == row.id));
        changed |= inner.rows.len() != count;
        if changed {
            if let Err(error) = self.persist(inner) {
                (self.log)(&format!(
                    "arca: the approval requests were not saved after a sweep ({})",
                    error.kind()
                ));
            }
            self.changed.notify_all();
        }
    }

    /// Saves the rows as they are now, then appends their audit line: a line is never written for a change that was not
    /// saved. When either fails the rows go back to `before` (saved again when the line was what failed) and nothing is
    /// decided.
    fn commit(
        &self,
        inner: &mut Inner,
        before: Vec<Row>,
        now: u64,
        principal: &str,
        tool: &str,
        outcome: &str,
    ) -> io::Result<()> {
        if let Err(error) = self.persist(inner) {
            inner.rows = before;
            return Err(error);
        }
        if let Err(error) = audit(&mut inner.audit, now, principal, tool, outcome) {
            inner.rows = before;
            if self.persist(inner).is_err() {
                (self.log)(
                    "arca: an approval request change was saved and could not be audited or undone",
                );
            }
            return Err(error);
        }
        Ok(())
    }

    /// Writes the requests whole to a temporary file, syncs it, renames it over the requests file and syncs the directory.
    fn persist(&self, inner: &Inner) -> io::Result<()> {
        let path = self.state_dir.join(REQUESTS_FILE);
        let temp = self
            .state_dir
            .join(format!("{REQUESTS_FILE}.tmp-{}", std::process::id()));
        let bytes = serde_json::to_vec(&inner.rows).map_err(io::Error::other)?;
        let _ = fs::remove_file(&temp);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, &path)?;
        File::open(&self.state_dir)?.sync_all()
    }
}

/// The rows a requests file holds, when it is a private regular file of at most 4 MiB that parses.
fn read_rows(path: &Path) -> io::Result<Vec<Row>> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES || meta.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))
}

/// Appends one audit line and syncs it. A line that cannot be made durable decides nothing.
fn audit(file: &mut File, at: u64, principal: &str, tool: &str, outcome: &str) -> io::Result<()> {
    let line = json!({ "at": at, "principal": principal, "tool": tool, "outcome": outcome });
    let mut text = serde_json::to_string(&line).map_err(io::Error::other)?;
    text.push('\n');
    file.write_all(text.as_bytes())?;
    file.sync_all()
}

/// Whether `row`'s list view meets `owner_approval_list`'s result schema. The reason names a path, never a value.
fn listable(row: &Row, host: &str) -> Result<(), String> {
    let Some(verb) = arca_api::verb("owner_approval_list") else {
        return Ok(());
    };
    arca_api::conforms(
        &(verb.result)(),
        &json!({ "requests": [view_of(row, host)] }),
    )
}

/// A request as the page and the list show it: never its approval string.
fn view_of(row: &Row, host: &str) -> Value {
    let mut view = json!({
        "id": row.id,
        "kind": row.kind,
        "fields": row.subject,
        "args": row.args,
        "droidd": "arca",
        "host": host,
        "requester": { "kind": row.requester_kind, "name": row.requester },
        "createdAt": iso_utc(row.created_at),
        "expiresAt": iso_utc(row.expires_at),
        "state": row.state.as_str(),
    });
    if row.state == RequestState::Approved
        && let Some(at) = row.approval_expires_at
    {
        view["approvalExpiresAt"] = json!(iso_utc(at));
    }
    view
}

/// The statement a passkey signs for `row`: its subject, this host, a fresh nonce, issued now and valid 15 minutes, as
/// canonical JSON that the spend's own parser reads back.
fn statement_of(row: &Row, host: &str, nonce: &str, now: u64) -> Result<String, CallError> {
    let text = canonical_json(&json!({
        "host": host,
        "action": row.field("action"),
        "target": row.field("target"),
        "paramsDigest": row.field("paramsDigest"),
        "issuedAt": iso_utc(now),
        "expiresAt": iso_utc(now.saturating_add(APPROVAL_TTL_MS)),
        "nonce": nonce,
    }))
    .map_err(|_| internal())?;
    parse_statement(text.as_bytes()).map_err(|_| internal())?;
    Ok(text)
}

fn action_of(row: &Row) -> Option<ArcaAction> {
    ArcaAction::from_name(row.field("action"))
}

/// The request's subject, its arguments and their digest, checked as its statement will carry them.
fn fields_of(
    args: &Value,
    host: &str,
    now: u64,
) -> Result<(Map<String, Value>, Value, String), CallError> {
    let Some(input) = args.as_object() else {
        return Err(invalid("the arguments are not a JSON object"));
    };
    match input.get("kind").and_then(Value::as_str) {
        Some("arca") => {}
        Some("upgrade") => {
            return Err(refused(
                "upgrade_not_here",
                "an identity phone's upgrade step is approved through droidd's MCP (owner_approval_request, kind upgrade, on droidd); Arca approves kind arca only",
            ));
        }
        _ => return Err(invalid("kind must be arca or upgrade")),
    }
    let stray: Vec<&str> = UPGRADE_KEYS
        .iter()
        .copied()
        .filter(|key| input.contains_key(*key))
        .collect();
    if !stray.is_empty() {
        return Err(invalid(format!(
            "an arca request takes no {}",
            stray.join(", ")
        )));
    }
    if input.contains_key("droidd") {
        return Err(invalid(
            "an arca request names no droidd: the approval is spent on this Arca, whose statements name no droidd",
        ));
    }
    let missing: Vec<&str> = ARCA_KEYS
        .iter()
        .copied()
        .filter(|key| !input.contains_key(*key))
        .collect();
    if !missing.is_empty() {
        return Err(invalid(format!(
            "an arca request needs {}",
            missing.join(", ")
        )));
    }
    if let Some(key) = input
        .keys()
        .find(|key| !["kind", "action", "target", "args"].contains(&key.as_str()))
    {
        let key = clean_key(key);
        return Err(invalid(format!("an arca request takes no {key}")));
    }
    let action = input["action"]
        .as_str()
        .filter(|action| ACTIONS.contains(action))
        .ok_or_else(|| invalid("action must be delete, publish, spend or enroll"))?;
    let target = input["target"]
        .as_str()
        .filter(|target| valid_target(target))
        .ok_or_else(|| invalid("target must match ^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$"))?;
    if names_a_plan(target) {
        return Err(invalid(
            "the target names a delete plan: a plan is approved through its own card on the passkey page, not through a request",
        ));
    }
    if action == ENROLL {
        check_enroll(target, &input["args"])?;
    }
    let mut owned_args = input["args"].clone();
    let Some(object) = owned_args.as_object_mut() else {
        return Err(invalid("args must be a JSON object"));
    };
    // A destructive call's `confirm` is taken out at Arca's door before the approval is checked, so the statement binds
    // the arguments without it: `true` is dropped here, and `false` asks for nothing. Any other value stays and is bound,
    // and the call that would spend it is refused by its schema.
    match object.get(arca_api::verbs::CONFIRM) {
        Some(Value::Bool(true)) => {
            object.remove(arca_api::verbs::CONFIRM);
        }
        Some(Value::Bool(false)) => {
            return Err(invalid("confirm is false: no approval was asked for"));
        }
        _ => {}
    }
    let call_args = &owned_args;
    let object = call_args.as_object().ok_or_else(internal)?;
    if object.contains_key("approval") {
        return Err(invalid(
            "args are the call's arguments without approval: the approval is what this request asks for",
        ));
    }
    check_shown(call_args, "args", 0)?;
    if object.keys().any(|key| key.chars().count() > ARGS_KEY_MAX) {
        return Err(invalid(format!(
            "an args key is longer than {ARGS_KEY_MAX} characters, more than the owner's list shows"
        )));
    }
    let text = serde_json::to_string(call_args).map_err(|_| internal())?;
    if text.len() > ARGS_MAX {
        return Err(invalid(format!(
            "args hold more than {ARGS_MAX} bytes of JSON"
        )));
    }
    let digest = params_digest(call_args).map_err(|_| invalid("args have no digest"))?;
    let mut subject = Map::new();
    subject.insert("action".to_string(), json!(action));
    subject.insert("target".to_string(), json!(target));
    subject.insert("paramsDigest".to_string(), json!(digest));
    // The statement begin will mint, parsed as its spend parses it: a request no statement can carry is refused now.
    let text = canonical_json(&json!({
        "host": host,
        "action": action,
        "target": target,
        "paramsDigest": digest,
        "issuedAt": iso_utc(now),
        "expiresAt": iso_utc(now.saturating_add(APPROVAL_TTL_MS)),
        "nonce": "0".repeat(32),
    }))
    .map_err(|_| internal())?;
    if let Err(refusal) = parse_statement(text.as_bytes()) {
        let why = refusal.to_string();
        let why = why
            .strip_prefix("the approval is malformed: ")
            .unwrap_or(&why);
        return Err(invalid(format!(
            "the request's fields make no statement Arca spends ({why})"
        )));
    }
    Ok((subject, call_args.clone(), digest))
}

/// An `enroll` request's target and args: the code the person shows, and `{service: <name>}` alone.
fn check_enroll(target: &str, args: &Value) -> Result<(), CallError> {
    if !valid_enroll_code(target) {
        return Err(invalid(
            "an enroll target is the code the person shows: 6 to 12 uppercase letters and digits (^[A-Z0-9]{6,12}$)",
        ));
    }
    let service = args
        .as_object()
        .filter(|object| object.len() == 1)
        .and_then(|object| object.get("service"))
        .and_then(Value::as_str)
        .filter(|service| valid_target(service));
    if service.is_none() {
        return Err(invalid(
            "enroll args are {service: <name>} and nothing else: the name of the service that makes the owner (^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$)",
        ));
    }
    Ok(())
}

/// Whether `target` is a delete plan's: `plan-` and its 64-digit digest.
fn names_a_plan(target: &str) -> bool {
    target
        .strip_prefix("plan-")
        .is_some_and(|digest| digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Whether `text` is an approval's target, as the statement's grammar has it.
fn valid_target(text: &str) -> bool {
    let bytes = text.as_bytes();
    (1..=128).contains(&bytes.len())
        && bytes[0].is_ascii_alphanumeric()
        && bytes[1..]
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// C0 and C1 controls and the bidi controls: none in a string the owner reads as data.
fn unshown(c: char) -> bool {
    matches!(c,
        '\u{0}'..='\u{1f}'
            | '\u{7f}'..='\u{9f}'
            | '\u{61c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}')
}

/// A key as a path names it: at most 32 characters.
fn clean_key(key: &str) -> String {
    key.chars().filter(|c| !unshown(*c)).take(32).collect()
}

/// A string the owner reads, or a key of one: no control or bidi character, and nothing that reads as an approval string
/// (an approval is what a request asks for, never one of its arguments). Refuses naming where, never quoting the value.
fn check_shown(value: &Value, at: &str, depth: usize) -> Result<(), CallError> {
    if depth > ARGS_DEPTH {
        return Err(invalid(format!(
            "{at} is nested deeper than {ARGS_DEPTH} levels"
        )));
    }
    match value {
        Value::String(text) => shown_string(text, at),
        Value::Array(items) => items
            .iter()
            .enumerate()
            .try_for_each(|(i, item)| check_shown(item, &format!("{at}[{i}]"), depth + 1)),
        Value::Object(map) => map.iter().try_for_each(|(key, item)| {
            shown_string(key, &format!("{at} key"))?;
            check_shown(item, &format!("{at}.{}", clean_key(key)), depth + 1)
        }),
        _ => Ok(()),
    }
}

fn shown_string(text: &str, at: &str) -> Result<(), CallError> {
    if text.chars().any(unshown) {
        return Err(invalid(format!(
            "{at} holds a control or bidi character, which the owner could not read as it is"
        )));
    }
    if split_approval(text).is_ok() {
        return Err(invalid(format!(
            "{at} is an approval string: an approval is what this request asks the owner for, never one of its arguments"
        )));
    }
    Ok(())
}

/// The full check of a statement against its request: its fields, digest, host and window, and a nonce neither spent nor
/// held by another request's approval. Each refusal names what does not match, never the statement's bytes.
fn check_against(
    rows: &[Row],
    row: &Row,
    kernel: &Kernel,
    statement: &str,
    now: u64,
) -> Result<(), PageRefusal> {
    let refuse = |why: String| {
        PageRefusal::new(
            403,
            "owner_approval",
            format!(
                "the approval for request {} ({}) is refused: {why}; the request stays pending",
                row.id,
                row.subject_line()
            ),
        )
    };
    let parsed = parse_statement(statement.as_bytes()).map_err(|refusal| {
        let why = refusal.to_string();
        refuse(
            why.strip_prefix("the approval is malformed: ")
                .unwrap_or(&why)
                .to_string(),
        )
    })?;
    for (key, value) in [
        ("action", parsed.action.as_str()),
        ("target", parsed.target.as_str()),
        ("paramsDigest", parsed.params_digest.as_str()),
    ] {
        if row.field(key) != value {
            return Err(refuse(format!(
                "it approves another {key} than the request's"
            )));
        }
    }
    if parsed.host != kernel.host() {
        return Err(refuse(format!(
            "it is for host {}, and this is {}",
            parsed.host,
            kernel.host()
        )));
    }
    let time = |text: &str| iso_ms(text).map(i128::from).unwrap_or_default();
    let (issued, expires, now) = (
        time(&parsed.issued_at),
        time(&parsed.expires_at),
        i128::from(now),
    );
    let ttl = i128::from(APPROVAL_TTL_MS);
    if issued > now + i128::from(ISSUE_SKEW_MS) {
        return Err(refuse(
            "it is issued in the future (more than 5 min ahead of this host's clock)".to_string(),
        ));
    }
    if expires <= issued || expires - issued > ttl || expires - now > ttl {
        return Err(refuse(
            "a passkey approval is valid for at most 15 min, from its issue and from now"
                .to_string(),
        ));
    }
    if expires <= now {
        return Err(refuse("it has expired".to_string()));
    }
    if kernel.approval_nonce_spent(&parsed.nonce) {
        return Err(refuse("its nonce is spent".to_string()));
    }
    let held = rows.iter().any(|other| {
        other.id != row.id
            && other.state == RequestState::Approved
            && other
                .statement
                .as_deref()
                .and_then(|text| parse_statement(text.as_bytes()).ok())
                .is_some_and(|s| s.nonce == parsed.nonce)
    });
    if held {
        return Err(refuse(
            "it was approved for another request already".to_string(),
        ));
    }
    Ok(())
}

/// The owner's passkeys a ceremony on `rp_id` may use, or why there are none.
fn passkeys_for(
    kernel: &Kernel,
    rp_id: &str,
) -> Result<Vec<arca_approval::PasskeyLine>, PageRefusal> {
    if let Some(refusal) = kernel.owner_passkey_refusal() {
        return Err(PageRefusal::new(
            503,
            "passkeys_refused",
            refusal.to_string(),
        ));
    }
    let lines = kernel.owner_passkey_lines();
    if lines.is_empty() {
        return Err(PageRefusal::new(
            409,
            "no_passkey",
            "no passkey is set up for Arca: add one on this page first",
        ));
    }
    let here: Vec<_> = lines
        .iter()
        .filter(|line| line.rp_id == rp_id)
        .cloned()
        .collect();
    if !here.is_empty() {
        return Ok(here);
    }
    let mut was: Vec<String> = lines.iter().map(|line| line.rp_id.clone()).collect();
    was.sort();
    was.dedup();
    let mut refusal = PageRefusal::new(
        409,
        "passkey_rp_changed",
        format!(
            "Your passkey was made for {}. This page is now {rp_id}. Add your passkey again on this page.",
            was.join(", ")
        ),
    );
    refusal.extra.insert("passkeyRpIds".to_string(), json!(was));
    refusal.extra.insert("rpId".to_string(), json!(rp_id));
    Err(refusal)
}

/// The owner's passkeys as the page's panel counts them: how many, how many for this page's relying party, that relying
/// party, and the others the lines name. A refused passkeys file counts none.
pub fn passkey_summary(kernel: &Kernel, rp_id: &str) -> Value {
    let lines = kernel.owner_passkey_lines();
    let mut others: Vec<String> = lines
        .iter()
        .map(|line| line.rp_id.clone())
        .filter(|rp| rp != rp_id)
        .collect();
    others.sort();
    others.dedup();
    json!({
        "enrolled": lines.len(),
        "forThisAddress": lines.iter().filter(|line| line.rp_id == rp_id).count(),
        "rpId": rp_id,
        "otherRpIds": others,
    })
}

fn find<'a>(rows: &'a [Row], id: &str) -> Result<&'a Row, CallError> {
    rows.iter()
        .find(|row| row.id == id)
        .ok_or_else(|| refused("not_found", format!("no approval request {id}")))
}

fn page_row<'a>(rows: &'a [Row], id: &str) -> Result<&'a Row, PageRefusal> {
    rows.iter()
        .find(|row| row.id == id && valid_id(id))
        .ok_or_else(|| PageRefusal::new(404, "not_found", "no such approval request"))
}

fn not_pending(row: &Row) -> PageRefusal {
    PageRefusal::new(
        409,
        "not_pending",
        format!("the request is {}", row.state.as_str()),
    )
}

fn stale() -> PageRefusal {
    PageRefusal::new(
        409,
        "stale",
        "the request changed since it was read: look again",
    )
}

fn finish_shape() -> PageRefusal {
    PageRefusal::new(
        400,
        "invalid",
        "the proof is the passkey's answer: alg, authenticatorData, clientDataJSON, credentialId, kind, signature and v",
    )
}

fn page_internal() -> PageRefusal {
    PageRefusal::new(500, "internal", "the request could not be read or saved")
}

/// Whether `id` is a request id: `ar_` and 16 lowercase hex digits.
pub fn valid_id(id: &str) -> bool {
    id.strip_prefix("ar_").is_some_and(|hex| {
        hex.len() == 16
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

fn request_id() -> Result<String, CallError> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(|_| internal())?;
    Ok(format!(
        "ar_{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ))
}

fn invalid(why: impl Into<String>) -> CallError {
    CallError::Invalid(why.into())
}

fn refused(code: &str, message: impl Into<String>) -> CallError {
    CallError::Refused {
        code: code.to_string(),
        message: message.into(),
    }
}

fn internal() -> CallError {
    CallError::Internal("the approval requests could not be read or saved".to_string())
}

/// The verbs over the requests, and `enroll_confirm`.
pub struct ApprovalRequestHandler {
    pub verb: &'static str,
    pub kernel: Arc<Mutex<Kernel>>,
    pub clock: crate::handlers::Clock,
    pub requests: Arc<ApprovalRequests>,
}

impl arca_mcp::ToolHandler for ApprovalRequestHandler {
    fn call(&self, args: Value, caller: &Caller) -> Result<Value, CallError> {
        // The door holds a viewer token to its verbs already; a request or a status (which may carry an approval string) is
        // never a viewer's, whatever door it came through.
        if self.verb != "owner_approval_list" && caller.scope < arca_api::Scope::Doctor {
            return Err(CallError::Forbidden {
                verb: self.verb.to_string(),
            });
        }
        match self.verb {
            "owner_approval_request" => {
                let kernel = self.kernel.lock().map_err(|_| internal())?;
                self.requests
                    .request(&kernel, &args, caller, (self.clock)())
            }
            "owner_approval_status" => {
                let id = args
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| valid_id(id))
                    .ok_or_else(|| invalid("id must match ^ar_[0-9a-f]{16}$"))?;
                let wait = match args.get("waitMs") {
                    None => 0,
                    Some(value) => value
                        .as_u64()
                        .filter(|ms| *ms <= WAIT_MAX_MS)
                        .ok_or_else(|| invalid("waitMs must be an integer from 0 to 30000"))?,
                };
                self.requests
                    .status(&self.kernel, id, wait, caller, self.clock.as_ref())
            }
            "enroll_confirm" => {
                let mut kernel = self.kernel.lock().map_err(|_| internal())?;
                self.requests
                    .enroll_confirm(&mut kernel, &args, caller, (self.clock)())
            }
            "owner_approval_list" => {
                let kernel = self.kernel.lock().map_err(|_| internal())?;
                let requests = self.requests.list(&kernel, (self.clock)())?;
                Ok(json!({ "requests": requests }))
            }
            _ => Err(CallError::NotFound),
        }
    }
}

/// The handlers of the request verbs and `enroll_confirm`, keyed by verb name.
pub fn handlers(
    kernel: Arc<Mutex<Kernel>>,
    clock: crate::handlers::Clock,
    requests: Arc<ApprovalRequests>,
) -> Vec<(&'static str, Arc<dyn arca_mcp::ToolHandler>)> {
    [
        "owner_approval_request",
        "owner_approval_status",
        "owner_approval_list",
        "enroll_confirm",
    ]
    .into_iter()
    .map(|verb| {
        (
            verb,
            Arc::new(ApprovalRequestHandler {
                verb,
                kernel: Arc::clone(&kernel),
                clock: clock.clone(),
                requests: Arc::clone(&requests),
            }) as Arc<dyn arca_mcp::ToolHandler>,
        )
    })
    .collect()
}
