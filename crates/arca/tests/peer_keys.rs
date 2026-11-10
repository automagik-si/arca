//! Approvals on another machine: the page host (alpha) keeps its owner passkeys in sync on a kernel on another machine
//! (gamma) with `peer_keys_sync`, over gamma's MCP socket forwarded to a path on alpha, and relays the owner's approvals
//! there. Gamma trusts the synced passkeys for action approvals only.
//!
//! Every key is a fixture: a software P-256 passkey from a fixed seed, written into the test's own scratch directory. The
//! forwarded socket is a byte pipe between two unix sockets, as `ssh -L <local.sock>:<remote.sock>` would be.

mod common;
#[path = "../../arca-approval/tests/common/webauthn.rs"]
mod webauthn;

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arca::approve_page::{Page, SetupCodeDisplay};
use arca::config::LinkedHost;
use arca::handlers::{
    ActionExecutor, ActionSlot, ApprovalStatus, Clock, StatusHandler, plan_handlers_with,
};
use arca::kernel::{AppliedSeq, Kernel, PeerSync};
use arca::peer_sync::{PeerSyncer, SyncOutcome};
use arca_api::Scope;
use arca_approval::{
    ApprovalRefusal, ApprovalVerifier, FileNonces, FileSignCounts, ForeignPasskeys, OwnerKeys,
    PEER_PASSKEYS_FILE, PasskeyVerifier, PeerPasskeys, SshSigVerifier, normal_lines,
    parse_passkeys, peer_file_name, peer_target, set_digest,
};
use arca_kernel::canon::{Digest, canonical_json, params_digest};
use arca_mcp::{CallError, Caller, Server, TokenStore, ToolHandler, mint_with_tools, serve_unix};
use arca_metrics::{HostMode, HostModeChoice};
use common::*;
use serde_json::{Value, json};
use tokio::runtime::Runtime;
use webauthn::{Assert, ORIGIN, SoftPasskey, passkey_approval};

/// The host gamma's approval statements name.
const GAMMA_HOST: &str = "gamma.example";
/// The origin of gamma's own page: its own passkey answers only there.
const GAMMA_ORIGIN: &str = "https://gamma.example.com:8443";

struct NoDisplay;

impl SetupCodeDisplay for NoDisplay {
    fn show(&self, _code: &str) -> Result<(), String> {
        Err("No screen to show the code on".to_string())
    }
}

struct CountingAction {
    runs: Arc<AtomicU64>,
}

impl ToolHandler for CountingAction {
    fn call(&self, _args: Value, _caller: &Caller) -> Result<Value, CallError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        Ok(json!({ "created": true }))
    }
}

fn executor(runs: &Arc<AtomicU64>) -> ActionExecutor {
    ActionExecutor::new([(
        "service_create",
        Arc::new(CountingAction {
            runs: Arc::clone(runs),
        }) as Arc<dyn ToolHandler>,
    )])
}

fn private_dir(path: &Path) {
    fs::create_dir(path).expect("a directory");
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("mode 0700");
}

fn write_mode(path: &Path, text: &str, mode: u32) {
    fs::write(path, text).expect("the file is written");
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("the mode is set");
}

/// A passkey of gamma's own, answering only gamma's page origin.
fn gamma_key(seed: u8) -> SoftPasskey {
    let mut key = SoftPasskey::from_seed(seed);
    key.rp_id = "gamma.example.com".to_string();
    key.origin = GAMMA_ORIGIN.to_string();
    key
}

/// A passkey line in its normal form (no comment), as the page host sends it.
fn sent(key: &SoftPasskey) -> String {
    normal_lines(&parse_passkeys(format!("{}\n", key.line(NAMESPACE)).as_bytes()).expect("a line"))
        .expect("normal form")
        .remove(0)
}

fn set_of(keys: &[&SoftPasskey]) -> Vec<String> {
    keys.iter().map(|key| sent(key)).collect()
}

/// Gamma alone: its kernel, keeping alpha's (`HOST`'s) passkeys for alpha's page origin in its peer passkeys file.
struct Gamma {
    scratch: Scratch,
    _fake: arca_incus::FakeIncus,
    kernel: Arc<Mutex<Kernel>>,
    peer: PeerPasskeys,
    file: PathBuf,
}

impl Gamma {
    fn new(tag: &str) -> Gamma {
        Gamma::keeping(tag, HOST)
    }

    /// Gamma keeping the passkeys of the page host named `from`.
    fn keeping(tag: &str, from: &str) -> Gamma {
        let scratch = Scratch::new(tag);
        let fake = start_fake(&scratch);
        let file = scratch.path(PEER_PASSKEYS_FILE);
        let peer = PeerPasskeys::new(file.clone(), from.to_string(), vec![ORIGIN.to_string()]);
        let own = parse_passkeys(format!("{}\n", gamma_key(2).line(NAMESPACE)).as_bytes())
            .expect("gamma's own line parses");
        let verifier = ApprovalVerifier::new(
            GAMMA_HOST.to_string(),
            OwnerKeys::new(
                SshSigVerifier::new(Vec::new()),
                PasskeyVerifier::new(
                    own,
                    FileSignCounts::open(&scratch.path("gamma-counts.jsonl")).expect("counts"),
                )
                .with_peer(peer.clone()),
            ),
            FileNonces::open(&scratch.path("gamma-nonces.jsonl")).expect("nonces"),
        );
        let kernel = Arc::new(Mutex::new(
            kernel_on_host(&scratch, &fake, verifier, NOW, None, GAMMA_HOST).expect("gamma opens"),
        ));
        Gamma {
            scratch,
            _fake: fake,
            kernel,
            peer,
            file,
        }
    }

    fn sync(
        &self,
        lines: &[String],
        signed: Option<(&str, &[u8])>,
    ) -> Result<PeerSync, ApprovalRefusal> {
        self.kernel
            .lock()
            .expect("the kernel lock")
            .peer_keys_sync(lines, signed, NOW)
    }

    /// The statement gamma issues for `lines`, which differ from the kept set.
    fn statement_for(&self, lines: &[String]) -> String {
        match self.sync(lines, None).expect("the sync answers") {
            PeerSync::NeedsApproval { statement, .. } => statement,
            other => panic!("a changed set needs an approval, not {other:?}"),
        }
    }

    fn signed_sync(
        &self,
        lines: &[String],
        statement: &str,
        key: &SoftPasskey,
        count: u32,
    ) -> Result<PeerSync, ApprovalRefusal> {
        let mut assert = Assert::over(NAMESPACE, statement.as_bytes());
        assert.sign_count = count;
        let proof = key.assert(&assert);
        self.sync(lines, Some((statement, &proof)))
    }

    fn status(&self) -> Value {
        status_of(Some(self.peer.clone()))
    }
}

fn status_of(peer: Option<PeerPasskeys>) -> Value {
    StatusHandler {
        node: "c".repeat(64),
        mode: "kernel",
        applied_seq: AppliedSeq::default(),
        started: Instant::now(),
        socket: PathBuf::from("/nonexistent/incus.sock"),
        metrics_source: Arc::new(Mutex::new(HostModeChoice {
            mode: HostMode::Local,
            reason: "the test names the local mode".to_string(),
            provisional: false,
        })),
        approval: Some(ApprovalStatus {
            linked_hosts: Vec::new(),
            foreign: ForeignPasskeys::default(),
            peer,
            peer_sources: None,
        }),
    }
    .call(json!({}), &caller(Scope::Viewer))
    .expect("arca_status answers")
}

fn action_approval(key: &SoftPasskey, statement: &str) -> String {
    passkey_approval(
        statement,
        &key.assert(&Assert::over(NAMESPACE, statement.as_bytes())),
    )
}

/// A fresh action statement for gamma's proposal `id`.
fn gamma_action_statement(kernel: &Arc<Mutex<Kernel>>, id: &str, nonce: &str) -> String {
    let kernel = kernel.lock().expect("the kernel lock");
    let proposal = kernel
        .action_proposals()
        .iter()
        .find(|proposal| proposal.id == id)
        .expect("the proposal waits")
        .clone();
    arca::handlers::action_statement(
        GAMMA_HOST,
        id,
        &proposal.action,
        &proposal.args_digest,
        NOW,
        nonce,
    )
    .expect("a statement")
}

fn propose(kernel: &Arc<Mutex<Kernel>>) -> String {
    kernel
        .lock()
        .expect("the kernel lock")
        .propose_action(
            arca::action_proposal::ActionKind::ServiceCreate,
            json!({ "name": "nats", "template": "nats" }),
            NOW,
            None,
        )
        .expect("a proposal")
        .id
}

#[test]
fn the_first_sync_is_trusted_as_is_and_arca_status_says_since_when() {
    let gamma = Gamma::new("pk-first");
    let before = gamma.status();
    assert_eq!(
        before["approval"]["peerKeys"],
        json!({ "from": HOST, "keys": 0, "since": null, "reason": "no passkey has been synced from node1 yet" })
    );

    let owner = SoftPasskey::from_seed(1);
    assert_eq!(
        gamma.sync(&set_of(&[&owner]), None),
        Ok(PeerSync::Trusted { keys: 1 })
    );
    let meta = fs::symlink_metadata(&gamma.file).expect("the file is written");
    assert!(meta.file_type().is_file());
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);

    let status = gamma.status();
    let peer = &status["approval"]["peerKeys"];
    assert_eq!(peer["from"], json!(HOST));
    assert_eq!(peer["keys"], json!(1));
    assert_eq!(peer["since"], json!("2026-10-30T12:00:00.000Z"));
    assert_eq!(
        peer["reason"],
        json!("trusting node1's passkeys since 2026-10-30T12:00:00.000Z (first sync)")
    );
    let text = status.to_string();
    assert!(
        !text.contains(&owner.cred_text()),
        "no credential in status"
    );
    assert!(!text.contains(&owner.key_text()), "no key in status");
    assert!(
        !text.contains(&*gamma.scratch.dir.display().to_string()),
        "no path"
    );
    assert!(
        arca_api::check::conforms(
            &(arca_api::verb("arca_status").expect("the verb").result)(),
            &status
        )
        .is_ok(),
        "the status fits its schema"
    );
}

#[test]
fn a_second_identical_sync_changes_nothing_and_needs_no_signature() {
    let gamma = Gamma::new("pk-same");
    let owner = SoftPasskey::from_seed(1);
    gamma
        .sync(&set_of(&[&owner]), None)
        .expect("the first sync");
    let before = fs::read(&gamma.file).expect("the file");
    // The same set with a comment and another order is the same set.
    let commented = vec![owner.line(NAMESPACE)];
    assert_eq!(
        gamma.sync(&commented, None),
        Ok(PeerSync::Unchanged { keys: 1 })
    );
    assert_eq!(fs::read(&gamma.file).expect("the file"), before);
}

#[test]
fn a_changed_set_without_a_signature_is_not_kept_and_gets_a_statement_to_sign() {
    let gamma = Gamma::new("pk-unsigned");
    let owner = SoftPasskey::from_seed(1);
    let added = SoftPasskey::from_seed(3);
    gamma
        .sync(&set_of(&[&owner]), None)
        .expect("the first sync");
    let before = fs::read(&gamma.file).expect("the file");

    let statement = gamma.statement_for(&set_of(&[&owner, &added]));
    let parsed: Value = serde_json::from_str(&statement).expect("JSON");
    assert_eq!(parsed["host"], json!(GAMMA_HOST));
    assert_eq!(parsed["action"], json!("peer_keys_set"));
    let mut sorted = set_of(&[&owner, &added]);
    sorted.sort();
    let digest = arca_approval::set_digest(&sorted);
    assert_eq!(parsed["target"], json!(format!("peer-keys-{digest}")));
    assert_eq!(
        parsed["paramsDigest"],
        json!(params_digest(&json!({ "sha256": digest })).expect("digest"))
    );
    assert!(
        !statement.contains(&added.cred_text()),
        "no line in the statement"
    );
    assert_eq!(
        fs::read(&gamma.file).expect("the file"),
        before,
        "nothing written"
    );
    assert_eq!(gamma.status()["approval"]["peerKeys"]["keys"], json!(1));

    // A statement without its proof is refused at the verb, before the kernel.
    let handlers: std::collections::HashMap<_, _> = plan_handlers_with(
        Arc::clone(&gamma.kernel),
        Arc::new(|| NOW),
        None,
        None,
        ActionSlot::default(),
    )
    .into_iter()
    .collect();
    let refused = handlers["peer_keys_sync"]
        .call(
            json!({ "lines": set_of(&[&owner, &added]), "statement": statement }),
            &caller(Scope::Doctor),
        )
        .expect_err("half a signed sync is refused");
    assert!(matches!(refused, CallError::Invalid(_)));
    let answer = handlers["peer_keys_sync"]
        .call(
            json!({ "lines": set_of(&[&owner, &added]) }),
            &caller(Scope::Doctor),
        )
        .expect("the verb answers");
    assert_eq!(answer["status"], json!("needs_approval"));
    assert_eq!(answer["keys"], json!(2));
    assert!(!answer.to_string().contains(&added.cred_text()));
    assert_eq!(fs::read(&gamma.file).expect("the file"), before);
}

#[test]
fn a_changed_set_signed_by_a_kept_peer_key_is_kept() {
    let gamma = Gamma::new("pk-signed");
    let owner = SoftPasskey::from_seed(1);
    let added = SoftPasskey::from_seed(3);
    gamma
        .sync(&set_of(&[&owner]), None)
        .expect("the first sync");
    let next = set_of(&[&added, &owner]);
    let statement = gamma.statement_for(&next);
    assert_eq!(
        gamma.signed_sync(&next, &statement, &owner, 0),
        Ok(PeerSync::Changed { keys: 2 })
    );
    let status = gamma.status();
    assert_eq!(status["approval"]["peerKeys"]["keys"], json!(2));
    assert_eq!(
        status["approval"]["peerKeys"]["since"],
        json!("2026-10-30T12:00:00.000Z"),
        "the first sync's time stays"
    );
    assert!(
        status["approval"]["peerKeys"]["reason"]
            .as_str()
            .expect("a reason")
            .contains("last changed with a passkey approval")
    );

    // The added key is now trusted for gamma's actions.
    let id = propose(&gamma.kernel);
    let statement = gamma_action_statement(&gamma.kernel, &id, &"e".repeat(32));
    gamma
        .kernel
        .lock()
        .expect("the kernel lock")
        .approve_action(&id, &action_approval(&added, &statement), NOW)
        .expect("the synced key approves an action");

    // A list that drops the first key, signed by the added one, is kept too.
    let next = set_of(&[&added]);
    let statement = gamma.statement_for(&next);
    assert_eq!(
        gamma.signed_sync(&next, &statement, &added, 0),
        Ok(PeerSync::Changed { keys: 1 })
    );
}

#[test]
fn a_change_signed_by_a_key_not_kept_from_the_page_host_is_refused() {
    let gamma = Gamma::new("pk-unknown");
    let owner = SoftPasskey::from_seed(1);
    let stranger = SoftPasskey::from_seed(4);
    gamma
        .sync(&set_of(&[&owner]), None)
        .expect("the first sync");
    let before = fs::read(&gamma.file).expect("the file");

    // A key only in the new set cannot approve its own entry.
    let next = set_of(&[&owner, &stranger]);
    let statement = gamma.statement_for(&next);
    assert_eq!(
        gamma.signed_sync(&next, &statement, &stranger, 0),
        Err(ApprovalRefusal::BadSignature)
    );
    // Gamma's own passkey keeps gamma's trust, not alpha's list.
    assert_eq!(
        gamma.signed_sync(&next, &statement, &gamma_key(2), 0),
        Err(ApprovalRefusal::BadSignature)
    );
    // A statement for another action is refused before any key is asked.
    let other = statement.replace("peer_keys_set", "passkey_add");
    assert_eq!(
        gamma.signed_sync(&next, &other, &owner, 0),
        Err(ApprovalRefusal::WrongAction)
    );
    // A signature by the kept key over the statement for another set is refused.
    let third = SoftPasskey::from_seed(5);
    assert_eq!(
        gamma.signed_sync(&set_of(&[&owner, &third]), &statement, &owner, 0),
        Err(ApprovalRefusal::WrongTarget)
    );
    assert_eq!(fs::read(&gamma.file).expect("the file"), before);
}

#[test]
fn a_replayed_signed_change_is_refused() {
    let gamma = Gamma::new("pk-replay");
    let owner = SoftPasskey::from_seed(1);
    let added = SoftPasskey::from_seed(3);
    gamma
        .sync(&set_of(&[&owner]), None)
        .expect("the first sync");
    let next = set_of(&[&owner, &added]);
    let statement = gamma.statement_for(&next);
    gamma
        .signed_sync(&next, &statement, &owner, 0)
        .expect("kept once");

    // Back to the first set, then the same signed change again: its nonce is spent.
    let back = set_of(&[&owner]);
    let undo = gamma.statement_for(&back);
    gamma
        .signed_sync(&back, &undo, &owner, 0)
        .expect("the undo is kept");
    assert_eq!(
        gamma.signed_sync(&next, &statement, &owner, 0),
        Err(ApprovalRefusal::Replayed)
    );
    // A counting passkey's replay is refused by its sign count first.
    let counted = gamma.statement_for(&next);
    gamma
        .signed_sync(&next, &counted, &owner, 7)
        .expect("a counting passkey keeps it");
    let back_again = gamma.statement_for(&back);
    gamma
        .signed_sync(&back, &back_again, &owner, 8)
        .expect("the undo is kept");
    assert_eq!(
        gamma.signed_sync(&next, &counted, &owner, 7),
        Err(ApprovalRefusal::SignCountRegressed)
    );
    assert_eq!(gamma.status()["approval"]["peerKeys"]["keys"], json!(1));
}

#[test]
fn removing_the_last_key_is_refused() {
    let gamma = Gamma::new("pk-last");
    let owner = SoftPasskey::from_seed(1);
    match gamma.sync(&[], None) {
        Err(ApprovalRefusal::Untrusted(why)) => assert!(why.contains("last trusted passkey")),
        other => panic!("an empty first sync is refused, not {other:?}"),
    }
    assert!(!gamma.file.exists(), "nothing is written");
    gamma
        .sync(&set_of(&[&owner]), None)
        .expect("the first sync");
    let before = fs::read(&gamma.file).expect("the file");
    match gamma.sync(&[], None) {
        Err(ApprovalRefusal::Untrusted(why)) => assert!(why.contains("last trusted passkey")),
        other => panic!("an empty set is refused, not {other:?}"),
    }
    assert_eq!(fs::read(&gamma.file).expect("the file"), before);
}

#[test]
fn peer_keys_approve_actions_only_never_a_change_to_the_targets_own_passkeys_or_a_sign_in() {
    let gamma = Gamma::new("pk-scope");
    let owner = SoftPasskey::from_seed(1);
    gamma
        .sync(&set_of(&[&owner]), None)
        .expect("the first sync");
    let state = gamma.scratch.dir.join("owner-passkeys");

    // A passkey add on gamma signed by alpha's synced key is refused with a plain reason, and nothing is written.
    let line = gamma_key(6).line(NAMESPACE);
    let change = canonical_json(&json!({
        "action": "passkey_add",
        "host": GAMMA_HOST,
        "target": format!("passkey-{}", Digest::of(line.as_bytes()).to_hex()),
        "paramsDigest": params_digest(&json!({ "line": line })).expect("the digest"),
        "issuedAt": arca::handlers::iso_utc(NOW),
        "expiresAt": arca::handlers::iso_utc(NOW + 600_000),
        "nonce": "f".repeat(32),
    }))
    .expect("the statement");
    let proof = owner.assert(&Assert::over(NAMESPACE, change.as_bytes()));
    let refused = gamma
        .kernel
        .lock()
        .expect("the kernel lock")
        .approve_passkey_change(&gamma.scratch.dir, &line, &change, &proof, NOW);
    match refused {
        Err(ApprovalRefusal::Untrusted(why)) => assert!(why.contains("another kernel"), "{why}"),
        other => panic!("the synced key cannot change gamma's passkeys, not {other:?}"),
    }
    assert!(!state.exists(), "nothing is written");
    assert_eq!(
        gamma
            .kernel
            .lock()
            .expect("the kernel lock")
            .owner_passkey_count(),
        1
    );

    // The sign-in check asks gamma's own passkeys only.
    let challenge = "sign-in statement";
    let proof = owner.assert(&Assert::over(NAMESPACE, challenge.as_bytes()));
    assert_eq!(
        gamma
            .kernel
            .lock()
            .expect("the kernel lock")
            .verify_passkey(NAMESPACE, challenge.as_bytes(), &proof),
        Err(ApprovalRefusal::BadSignature)
    );

    // And the same key approves gamma's actions.
    let id = propose(&gamma.kernel);
    let statement = gamma_action_statement(&gamma.kernel, &id, &"9".repeat(32));
    gamma
        .kernel
        .lock()
        .expect("the kernel lock")
        .approve_action(&id, &action_approval(&owner, &statement), NOW)
        .expect("the synced key approves an action");
}

#[test]
fn a_synced_key_for_an_unlisted_origin_is_kept_but_not_trusted() {
    let gamma = Gamma::new("pk-origin");
    let elsewhere = gamma_key(7);
    assert_eq!(
        gamma.sync(&set_of(&[&elsewhere]), None),
        Ok(PeerSync::Trusted { keys: 1 })
    );
    let peer = &gamma.status()["approval"]["peerKeys"];
    assert_eq!(peer["keys"], json!(0));
    assert_eq!(
        peer["reason"],
        json!("no synced passkey answers a listed origin")
    );
    let id = propose(&gamma.kernel);
    let statement = gamma_action_statement(&gamma.kernel, &id, &"8".repeat(32));
    let mut assert = Assert::over(NAMESPACE, statement.as_bytes());
    assert.origin = Some(GAMMA_ORIGIN.to_string());
    assert!(
        gamma
            .kernel
            .lock()
            .expect("the kernel lock")
            .approve_action(
                &id,
                &passkey_approval(&statement, &elsewhere.assert(&assert)),
                NOW
            )
            .is_err()
    );
}

#[test]
fn a_peer_file_that_is_a_link_or_open_to_others_is_not_trusted_and_not_overwritten() {
    let gamma = Gamma::new("pk-file");
    let owner = SoftPasskey::from_seed(1);
    let real = gamma.scratch.path("elsewhere");
    write_mode(
        &real,
        &format!(
            "# arca peer-keys from={HOST} since={NOW}\n{}\n",
            sent(&owner)
        ),
        0o600,
    );
    std::os::unix::fs::symlink(&real, &gamma.file).expect("a link");
    let peer = &gamma.status()["approval"]["peerKeys"];
    assert_eq!(peer["keys"], json!(0));
    assert_eq!(
        peer["reason"],
        json!("the file cannot be opened, or it is a link")
    );
    assert!(matches!(
        gamma.sync(&set_of(&[&owner]), None),
        Err(ApprovalRefusal::Untrusted(_))
    ));
    assert!(
        fs::symlink_metadata(&gamma.file)
            .expect("the link")
            .file_type()
            .is_symlink(),
        "a refused file is never taken for an empty one"
    );

    fs::remove_file(&gamma.file).expect("the link is removed");
    fs::copy(&real, &gamma.file).expect("a copy");
    fs::set_permissions(&gamma.file, fs::Permissions::from_mode(0o622)).expect("mode 0622");
    // A fresh read (the status keeps its last read for ten seconds).
    assert_eq!(
        gamma.peer.load().1.reason.as_deref(),
        Some("the file is writable by group or others")
    );
    assert_eq!(
        gamma.status()["approval"]["peerKeys"]["reason"],
        json!("the file is writable by group or others"),
        "the status reports that read"
    );
}

// ---- several sources: `approval.peerKeys` as a list ---------------------------------------------------------------------

/// The second page host of the list form, and the one origin its passkeys answer from.
const CLOUD: &str = "cloud.example";
const CLOUD_ORIGIN: &str = "https://cloud.example.com";

/// A passkey of the second page host, answering only its origin.
fn cloud_key(seed: u8) -> SoftPasskey {
    let mut key = SoftPasskey::from_seed(seed);
    key.rp_id = "cloud.example.com".to_string();
    key.origin = CLOUD_ORIGIN.to_string();
    key
}

fn single_object() -> Value {
    json!({ "peerKeys": { "from": HOST, "origins": [ORIGIN] } })
}

fn two_sources() -> Value {
    json!({ "peerKeys": [
        { "from": HOST, "origins": [ORIGIN] },
        { "from": CLOUD, "origins": [CLOUD_ORIGIN] },
    ] })
}

/// Gamma built from an `approval` config as `arca serve` builds it: one `PeerPasskeys` per source, in the config's order, each
/// in the state file its place names.
struct Sources {
    scratch: Scratch,
    _fake: arca_incus::FakeIncus,
    kernel: Arc<Mutex<Kernel>>,
    peers: Vec<PeerPasskeys>,
    list: bool,
}

impl Sources {
    fn new(tag: &str, approval: Value) -> Sources {
        let approval: arca::config::ApprovalConfig =
            serde_json::from_value(approval).expect("the approval key parses");
        let keys = approval.peer_keys.expect("peerKeys is set");
        let scratch = Scratch::new(tag);
        let fake = start_fake(&scratch);
        let peers: Vec<PeerPasskeys> = keys
            .sources()
            .iter()
            .enumerate()
            .map(|(index, source)| {
                PeerPasskeys::new(
                    scratch.path(&peer_file_name(index, &source.from)),
                    source.from.clone(),
                    source.origins.clone(),
                )
            })
            .collect();
        let own = parse_passkeys(format!("{}\n", gamma_key(2).line(NAMESPACE)).as_bytes())
            .expect("gamma's own line parses");
        let passkeys = peers.iter().cloned().fold(
            PasskeyVerifier::new(
                own,
                FileSignCounts::open(&scratch.path("gamma-counts.jsonl")).expect("counts"),
            ),
            PasskeyVerifier::with_peer,
        );
        let verifier = ApprovalVerifier::new(
            GAMMA_HOST.to_string(),
            OwnerKeys::new(SshSigVerifier::new(Vec::new()), passkeys),
            FileNonces::open(&scratch.path("gamma-nonces.jsonl")).expect("nonces"),
        );
        let kernel = Arc::new(Mutex::new(
            kernel_on_host(&scratch, &fake, verifier, NOW, None, GAMMA_HOST).expect("gamma opens"),
        ));
        Sources {
            scratch,
            _fake: fake,
            kernel,
            peers,
            list: keys.is_list(),
        }
    }

    fn sync(
        &self,
        from: Option<&str>,
        lines: &[String],
        signed: Option<(&str, &[u8])>,
    ) -> Result<PeerSync, ApprovalRefusal> {
        self.kernel
            .lock()
            .expect("the kernel lock")
            .peer_keys_sync_from(from, lines, signed, NOW)
    }

    fn statement_for(&self, from: Option<&str>, lines: &[String]) -> String {
        match self.sync(from, lines, None).expect("the sync answers") {
            PeerSync::NeedsApproval { statement, .. } => statement,
            other => panic!("a changed set needs an approval, not {other:?}"),
        }
    }

    fn signed_sync(
        &self,
        from: Option<&str>,
        lines: &[String],
        statement: &str,
        key: &SoftPasskey,
    ) -> Result<PeerSync, ApprovalRefusal> {
        let proof = key.assert(&Assert::over(NAMESPACE, statement.as_bytes()));
        self.sync(from, lines, Some((statement, &proof)))
    }

    /// `arca_status`'s `approval`, as `arca serve` fills it for this config.
    fn approval_status(&self) -> Value {
        let status = StatusHandler {
            node: "c".repeat(64),
            mode: "kernel",
            applied_seq: AppliedSeq::default(),
            started: Instant::now(),
            socket: PathBuf::from("/nonexistent/incus.sock"),
            metrics_source: Arc::new(Mutex::new(HostModeChoice {
                mode: HostMode::Local,
                reason: "the test names the local mode".to_string(),
                provisional: false,
            })),
            approval: Some(ApprovalStatus {
                linked_hosts: Vec::new(),
                foreign: ForeignPasskeys::default(),
                peer: self.peers.first().cloned(),
                peer_sources: self.list.then(|| self.peers.clone()),
            }),
        }
        .call(json!({}), &caller(Scope::Viewer))
        .expect("arca_status answers");
        assert!(
            arca_api::check::conforms(
                &(arca_api::verb("arca_status").expect("the verb").result)(),
                &status
            )
            .is_ok(),
            "the status fits its schema"
        );
        status["approval"].clone()
    }

    fn file(&self, name: &str) -> String {
        fs::read_to_string(self.scratch.path(name)).expect("the file is there")
    }

    /// The peer passkeys files in the state directory, sorted.
    fn files(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(&self.scratch.dir)
            .expect("the state directory")
            .map(|entry| {
                entry
                    .expect("an entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .filter(|name| name.contains("peer-passkeys"))
            .collect();
        names.sort();
        names
    }

    fn approve(&self, id: &str, approval: &str) -> bool {
        self.kernel
            .lock()
            .expect("the kernel lock")
            .approve_action(id, approval, NOW)
            .is_ok()
    }
}

fn field(statement: &str, key: &str) -> String {
    let value: Value = serde_json::from_str(statement).expect("a statement");
    value[key].as_str().expect("a field").to_string()
}

#[test]
fn the_single_object_config_keeps_the_same_file_answers_and_status_as_before() {
    let gamma = Sources::new("pk-golden", single_object());
    assert!(!gamma.list);
    assert_eq!(gamma.peers.len(), 1);
    assert_eq!(peer_file_name(0, HOST), PEER_PASSKEYS_FILE);
    assert_eq!(
        gamma.approval_status(),
        json!({
            "linkedHosts": [],
            "trustedKeyFiles": [],
            "peerKeys": { "from": HOST, "keys": 0, "since": null, "reason": "no passkey has been synced from node1 yet" },
        })
    );

    let owner = SoftPasskey::from_seed(1);
    let added = SoftPasskey::from_seed(3);
    assert_eq!(
        gamma.sync(None, &set_of(&[&owner]), None),
        Ok(PeerSync::Trusted { keys: 1 })
    );
    assert_eq!(gamma.files(), [PEER_PASSKEYS_FILE]);
    assert_eq!(
        gamma.file(PEER_PASSKEYS_FILE),
        format!(
            "# arca peer-keys from=node1 since={NOW}\n{}\n",
            sent(&owner)
        )
    );
    assert_eq!(
        gamma.approval_status(),
        json!({
            "linkedHosts": [],
            "trustedKeyFiles": [],
            "peerKeys": {
                "from": HOST,
                "keys": 1,
                "since": "2026-10-30T12:00:00.000Z",
                "reason": "trusting node1's passkeys since 2026-10-30T12:00:00.000Z (first sync)",
            },
        })
    );
    assert_eq!(
        gamma.sync(None, &set_of(&[&owner]), None),
        Ok(PeerSync::Unchanged { keys: 1 })
    );
    // Naming the one source is the same call.
    assert_eq!(
        gamma.sync(Some(HOST), &set_of(&[&owner]), None),
        Ok(PeerSync::Unchanged { keys: 1 })
    );

    // A changed set: the statement binds the set's digest and nothing else, as it always has.
    let mut next = set_of(&[&added, &owner]);
    next.sort();
    let digest = set_digest(&next);
    let statement = gamma.statement_for(None, &next);
    assert_eq!(field(&statement, "action"), "peer_keys_set");
    assert_eq!(field(&statement, "host"), GAMMA_HOST);
    assert_eq!(field(&statement, "target"), peer_target(&digest));
    assert_eq!(
        field(&statement, "paramsDigest"),
        params_digest(&json!({ "sha256": digest })).expect("the digest")
    );
    assert_eq!(
        gamma.signed_sync(None, &next, &statement, &owner),
        Ok(PeerSync::Changed { keys: 2 })
    );
    assert_eq!(gamma.files(), [PEER_PASSKEYS_FILE]);
    assert_eq!(
        gamma.file(PEER_PASSKEYS_FILE),
        format!(
            "# arca peer-keys from=node1 since={NOW} changed={NOW}\n{}\n{}\n",
            next[0], next[1]
        )
    );
    let status = gamma.approval_status();
    assert!(status.get("peerKeySources").is_none(), "{status}");
    assert_eq!(status["peerKeys"]["keys"], json!(2));
}

#[test]
fn two_sources_sync_independently_each_into_its_own_file() {
    let gamma = Sources::new("pk-two", two_sources());
    assert!(gamma.list);
    let alpha = SoftPasskey::from_seed(1);
    let cloud = cloud_key(4);
    let second = format!("{PEER_PASSKEYS_FILE}-{CLOUD}");

    assert_eq!(
        gamma.sync(None, &set_of(&[&alpha]), None),
        Ok(PeerSync::Trusted { keys: 1 }),
        "no from is the first source"
    );
    assert_eq!(gamma.files(), [PEER_PASSKEYS_FILE]);
    let waiting = gamma.approval_status();
    assert_eq!(waiting["peerKeySources"][0], waiting["peerKeys"]);
    assert_eq!(
        waiting["peerKeySources"][1],
        json!({ "from": CLOUD, "keys": 0, "since": null, "reason": "no passkey has been synced from cloud.example yet" })
    );

    assert_eq!(
        gamma.sync(Some(CLOUD), &set_of(&[&cloud]), None),
        Ok(PeerSync::Trusted { keys: 1 }),
        "the second source's first sync is its own"
    );
    assert_eq!(
        gamma.files(),
        [PEER_PASSKEYS_FILE.to_string(), second.clone()]
    );
    let first_file = format!(
        "# arca peer-keys from=node1 since={NOW}\n{}\n",
        sent(&alpha)
    );
    assert_eq!(gamma.file(PEER_PASSKEYS_FILE), first_file);
    assert_eq!(
        gamma.file(&second),
        format!(
            "# arca peer-keys from={CLOUD} since={NOW}\n{}\n",
            sent(&cloud)
        )
    );
    let meta = fs::symlink_metadata(gamma.scratch.path(&second)).expect("the second file");
    assert!(meta.file_type().is_file());
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);

    let status = gamma.approval_status();
    assert_eq!(status["peerKeys"]["from"], json!(HOST));
    let sources = status["peerKeySources"].as_array().expect("each source");
    assert_eq!(sources.len(), 2);
    assert_eq!(sources[0], status["peerKeys"]);
    assert_eq!(sources[1]["from"], json!(CLOUD));
    assert_eq!(sources[1]["keys"], json!(1));
    let text = status.to_string();
    assert!(!text.contains(&cloud.cred_text()) && !text.contains(&cloud.key_text()));

    // Each answers for its own set only.
    assert_eq!(
        gamma.sync(Some(HOST), &set_of(&[&alpha]), None),
        Ok(PeerSync::Unchanged { keys: 1 })
    );
    assert_eq!(
        gamma.sync(Some(CLOUD), &set_of(&[&cloud]), None),
        Ok(PeerSync::Unchanged { keys: 1 })
    );
    assert!(matches!(
        gamma.sync(Some(CLOUD), &set_of(&[&alpha]), None),
        Ok(PeerSync::NeedsApproval { .. })
    ));
    match gamma.sync(Some("other.example"), &set_of(&[&alpha]), None) {
        Err(ApprovalRefusal::Untrusted(why)) => {
            assert!(why.contains("does not keep that page host"), "{why}")
        }
        other => panic!("an unknown source is refused, not {other:?}"),
    }

    // A change to the second source's set, signed by its own kept key, leaves the first source's file as it was. Its
    // statement names its page host too.
    let more = cloud_key(5);
    let mut next = set_of(&[&cloud, &more]);
    next.sort();
    let digest = set_digest(&next);
    let statement = gamma.statement_for(Some(CLOUD), &next);
    assert_eq!(
        field(&statement, "paramsDigest"),
        params_digest(&json!({ "sha256": digest, "from": CLOUD })).expect("the digest")
    );
    assert_eq!(
        gamma.signed_sync(Some(CLOUD), &next, &statement, &cloud),
        Ok(PeerSync::Changed { keys: 2 })
    );
    assert_eq!(gamma.file(PEER_PASSKEYS_FILE), first_file);
    assert_eq!(
        gamma.approval_status()["peerKeySources"][1]["keys"],
        json!(2)
    );

    // Both sources' keys approve gamma's actions, each from its own origin.
    for (key, nonce) in [(&alpha, "a"), (&more, "b")] {
        let id = propose(&gamma.kernel);
        let statement = gamma_action_statement(&gamma.kernel, &id, &nonce.repeat(32));
        assert!(gamma.approve(&id, &action_approval(key, &statement)));
    }
}

#[test]
fn a_key_of_one_source_cannot_approve_a_change_to_another_sources_set() {
    let gamma = Sources::new("pk-cross", two_sources());
    let alpha = SoftPasskey::from_seed(1);
    let cloud = cloud_key(4);
    gamma
        .sync(None, &set_of(&[&alpha]), None)
        .expect("the first source syncs");
    gamma
        .sync(Some(CLOUD), &set_of(&[&cloud]), None)
        .expect("the second source syncs");
    let first = gamma.file(PEER_PASSKEYS_FILE);
    let second = gamma.file(&peer_file_name(1, CLOUD));

    // The second source's key signs a change to the first source's set, in every way it can be asked.
    let wanted = set_of(&[&alpha, &cloud_key(5)]);
    let statement = gamma.statement_for(None, &wanted);
    assert_eq!(
        gamma.signed_sync(None, &wanted, &statement, &cloud),
        Err(ApprovalRefusal::BadSignature)
    );
    assert_eq!(
        gamma.signed_sync(Some(HOST), &wanted, &statement, &cloud),
        Err(ApprovalRefusal::BadSignature)
    );
    // The first source's key cannot change the second's either, and a statement for one set is not one for the other.
    let wanted_cloud = set_of(&[&cloud, &SoftPasskey::from_seed(6)]);
    let cloud_statement = gamma.statement_for(Some(CLOUD), &wanted_cloud);
    assert_eq!(
        gamma.signed_sync(Some(CLOUD), &wanted_cloud, &cloud_statement, &alpha),
        Err(ApprovalRefusal::BadSignature)
    );
    assert!(
        gamma
            .signed_sync(Some(CLOUD), &wanted, &statement, &cloud)
            .is_err(),
        "the first source's statement changes nothing of the second's"
    );
    assert_eq!(gamma.file(PEER_PASSKEYS_FILE), first);
    assert_eq!(gamma.file(&peer_file_name(1, CLOUD)), second);

    // Neither source's key changes gamma's own passkeys.
    let line = gamma_key(6).line(NAMESPACE);
    let change = canonical_json(&json!({
        "action": "passkey_add",
        "host": GAMMA_HOST,
        "target": format!("passkey-{}", Digest::of(line.as_bytes()).to_hex()),
        "paramsDigest": params_digest(&json!({ "line": line })).expect("the digest"),
        "issuedAt": arca::handlers::iso_utc(NOW),
        "expiresAt": arca::handlers::iso_utc(NOW + 600_000),
        "nonce": "f".repeat(32),
    }))
    .expect("the statement");
    let proof = cloud.assert(&Assert::over(NAMESPACE, change.as_bytes()));
    match gamma
        .kernel
        .lock()
        .expect("the kernel lock")
        .approve_passkey_change(&gamma.scratch.dir, &line, &change, &proof, NOW)
    {
        Err(ApprovalRefusal::Untrusted(why)) => assert!(why.contains("another kernel"), "{why}"),
        other => panic!("a further source's key cannot change gamma's passkeys, not {other:?}"),
    }

    // The owner of each source still changes its own set.
    assert_eq!(
        gamma.signed_sync(None, &wanted, &gamma.statement_for(None, &wanted), &alpha),
        Ok(PeerSync::Changed { keys: 2 })
    );
}

#[test]
fn a_line_one_source_sent_under_another_sources_credential_id_never_stands_in_for_it() {
    // The cloud service is the first source here, so its lines are read before the page host's.
    let gamma = Sources::new(
        "pk-forged",
        json!({ "peerKeys": [
            { "from": CLOUD, "origins": [CLOUD_ORIGIN] },
            { "from": HOST, "origins": [ORIGIN] },
        ] }),
    );
    let alpha = SoftPasskey::from_seed(1);
    let mut forged = cloud_key(4);
    forged.credential_id = alpha.credential_id.clone();
    gamma
        .sync(None, &set_of(&[&forged]), None)
        .expect("the cloud source syncs");
    gamma
        .sync(Some(HOST), &set_of(&[&alpha]), None)
        .expect("the page host syncs");
    let kept = gamma.file(&peer_file_name(1, HOST));

    let wanted = set_of(&[&alpha, &SoftPasskey::from_seed(6)]);
    let statement = gamma.statement_for(Some(HOST), &wanted);
    assert_eq!(
        gamma.signed_sync(Some(HOST), &wanted, &statement, &forged),
        Err(ApprovalRefusal::BadSignature),
        "the cloud's key under the page host's credential id changes nothing of the page host's set"
    );
    assert_eq!(gamma.file(&peer_file_name(1, HOST)), kept);
}

#[test]
fn a_sources_key_answers_only_from_its_own_origins() {
    let gamma = Sources::new("pk-origins", two_sources());
    let alpha = SoftPasskey::from_seed(1);
    let cloud = cloud_key(4);
    gamma
        .sync(None, &set_of(&[&alpha]), None)
        .expect("the first source syncs");
    gamma
        .sync(Some(CLOUD), &set_of(&[&cloud]), None)
        .expect("the second source syncs");

    // The second source's key, asserting from the first source's origin, approves nothing; and the other way round.
    for (key, origin, nonce) in [(&cloud, ORIGIN, "1"), (&alpha, CLOUD_ORIGIN, "2")] {
        let id = propose(&gamma.kernel);
        let statement = gamma_action_statement(&gamma.kernel, &id, &nonce.repeat(32));
        let mut assert = Assert::over(NAMESPACE, statement.as_bytes());
        assert.origin = Some(origin.to_string());
        assert!(
            !gamma.approve(&id, &passkey_approval(&statement, &key.assert(&assert))),
            "{origin}"
        );
        assert!(
            gamma.approve(&id, &action_approval(key, &statement)),
            "from its own origin it approves"
        );
    }

    // A line the second source sends for the first source's origin is kept in its file and trusted for nothing.
    let gamma = Sources::new("pk-claimed", two_sources());
    let mut claimed = cloud_key(7);
    claimed.rp_id = webauthn::RP.to_string();
    claimed.origin = ORIGIN.to_string();
    assert_eq!(
        gamma.sync(Some(CLOUD), &set_of(&[&claimed]), None),
        Ok(PeerSync::Trusted { keys: 1 })
    );
    let status = gamma.approval_status();
    assert_eq!(status["peerKeySources"][1]["keys"], json!(0));
    assert_eq!(
        status["peerKeySources"][1]["reason"],
        json!("no synced passkey answers a listed origin")
    );
    let id = propose(&gamma.kernel);
    let statement = gamma_action_statement(&gamma.kernel, &id, &"3".repeat(32));
    assert!(!gamma.approve(&id, &action_approval(&claimed, &statement)));
}

#[test]
fn through_the_verb_peer_keys_sync_takes_an_optional_from() {
    let gamma = Sources::new("pk-verb", two_sources());
    let clock: Clock = Arc::new(|| NOW);
    let handlers: std::collections::HashMap<_, _> = plan_handlers_with(
        Arc::clone(&gamma.kernel),
        clock,
        None,
        None,
        ActionSlot::default(),
    )
    .into_iter()
    .collect();
    let verb = arca_api::verb("peer_keys_sync").expect("the verb");
    let call = |args: Value| {
        assert!(
            arca_api::check::conforms(&(verb.params)(), &args).is_ok(),
            "the arguments fit the schema"
        );
        handlers["peer_keys_sync"].call(args, &caller(Scope::Doctor))
    };
    let alpha = SoftPasskey::from_seed(1);
    let cloud = cloud_key(4);
    assert_eq!(
        call(json!({ "lines": set_of(&[&alpha]) })).expect("the first source"),
        json!({ "status": "trusted", "keys": 1 })
    );
    assert_eq!(
        call(json!({ "lines": set_of(&[&cloud]), "from": CLOUD })).expect("the second source"),
        json!({ "status": "trusted", "keys": 1 })
    );
    assert_eq!(
        call(json!({ "lines": set_of(&[&cloud]), "from": CLOUD })).expect("again"),
        json!({ "status": "unchanged", "keys": 1 })
    );
    assert!(call(json!({ "lines": set_of(&[&cloud]), "from": "other.example" })).is_err());
    assert!(
        arca_api::check::conforms(
            &(verb.params)(),
            &json!({ "lines": [], "from": "not a host" })
        )
        .is_err()
    );
}

/// A forwarded unix socket: every connection to `from` is piped to `to`, as an SSH local forward of a remote socket is.
fn forward(from: PathBuf, to: PathBuf) {
    let listener = UnixListener::bind(&from).expect("the forward listens");
    std::thread::spawn(move || {
        for client in listener.incoming() {
            let Ok(client) = client else { continue };
            let Ok(server) = UnixStream::connect(&to) else {
                continue;
            };
            let (mut client_in, mut server_out) = (
                client.try_clone().expect("a clone"),
                server.try_clone().expect("a clone"),
            );
            std::thread::spawn(move || {
                let _ = std::io::copy(&mut client_in, &mut server_out);
                let _ = server_out.shutdown(std::net::Shutdown::Write);
            });
            let (mut server_in, mut client_out) = (server, client);
            std::thread::spawn(move || {
                let _ = std::io::copy(&mut server_in, &mut client_out);
                let _ = client_out.shutdown(std::net::Shutdown::Both);
            });
        }
    });
}

/// Alpha's page, linked to gamma through a forwarded socket, and gamma's MCP door.
struct Remote {
    _alpha_scratch: Scratch,
    _alpha_fake: arca_incus::FakeIncus,
    alpha: Arc<Mutex<Kernel>>,
    alpha_socket: PathBuf,
    alpha_state: PathBuf,
    gamma: Gamma,
    gamma_runs: Arc<AtomicU64>,
    syncer: PeerSyncer,
    logs: Arc<Mutex<Vec<String>>>,
    _runtime: Runtime,
}

impl Remote {
    fn new(tag: &str) -> (Remote, SoftPasskey) {
        Remote::keeping(tag, HOST)
    }

    /// Alpha linked to a gamma that keeps the passkeys of the page host named `from`.
    fn keeping(tag: &str, from: &str) -> (Remote, SoftPasskey) {
        let alpha_scratch = Scratch::new(&format!("{tag}-alpha"));
        let alpha_fake = start_fake(&alpha_scratch);
        let owner = SoftPasskey::from_seed(1);
        let alpha_verifier = verifier_with_passkeys(
            &alpha_scratch,
            &[],
            &format!("{}\n", owner.line(NAMESPACE)),
            "alpha",
        );
        let alpha = Arc::new(Mutex::new(
            kernel(&alpha_scratch, &alpha_fake, alpha_verifier, NOW).expect("alpha opens"),
        ));
        let gamma = Gamma::keeping(&format!("{tag}-gamma"), from);

        // Gamma's MCP door, with a doctor token limited to the relayed verbs and arca_status.
        let tokens_dir = gamma.scratch.path("tokens");
        private_dir(&tokens_dir);
        let tools = [
            "arca_status",
            "plan_list",
            "action_approve",
            "plan_decline",
            "peer_keys_sync",
        ]
        .map(str::to_string);
        let (secret, hash) = mint_with_tools(Scope::Doctor, Some(&tools));
        write_mode(&tokens_dir.join("relay.hash"), &hash, 0o400);
        let token_file = alpha_scratch.path("gamma-relay.secret");
        write_mode(&token_file, &format!("{secret}\n"), 0o600);
        let gamma_runs = Arc::new(AtomicU64::new(0));
        let slot = ActionSlot::default();
        let clock: Clock = Arc::new(|| NOW);
        let mut gamma_server = Server::new(
            arca_api::verbs(),
            TokenStore::load(&tokens_dir).expect("the token store loads"),
        );
        for (verb, handler) in plan_handlers_with(
            Arc::clone(&gamma.kernel),
            Arc::clone(&clock),
            None,
            None,
            Arc::clone(&slot),
        ) {
            gamma_server = gamma_server.handle(verb, handler);
        }
        gamma_server = gamma_server.handle(
            "arca_status",
            Arc::new(StatusHandler {
                node: "c".repeat(64),
                mode: "kernel",
                applied_seq: AppliedSeq::default(),
                started: Instant::now(),
                socket: PathBuf::from("/nonexistent/incus.sock"),
                metrics_source: Arc::new(Mutex::new(HostModeChoice {
                    mode: HostMode::Remote,
                    reason: "gamma is another machine".to_string(),
                    provisional: false,
                })),
                approval: Some(ApprovalStatus {
                    linked_hosts: Vec::new(),
                    foreign: ForeignPasskeys::default(),
                    peer: Some(gamma.peer.clone()),
                    peer_sources: None,
                }),
            }),
        );
        let _ = slot.set(executor(&gamma_runs));
        let gamma_socket = gamma.scratch.path("gamma.sock");

        // Alpha reaches gamma only through the forwarded path on its own side.
        let forwarded = alpha_scratch.path("gamma-forwarded.sock");
        let linked = vec![LinkedHost {
            name: "gamma".to_string(),
            socket: forwarded.clone(),
            token_file,
        }];
        let logs: Arc<Mutex<Vec<String>>> = Arc::default();
        let sink = Arc::clone(&logs);
        let syncer = PeerSyncer::new(Arc::clone(&alpha), linked.clone()).with_log(Arc::new(
            move |line: &str| sink.lock().expect("the log").push(line.to_string()),
        ));
        let alpha_state = alpha_scratch.path("state");
        private_dir(&alpha_state);
        let page_logs = Arc::clone(&logs);
        let page = Page::new(
            ORIGIN.to_string(),
            Arc::clone(&alpha),
            Arc::clone(&clock),
            alpha_state.clone(),
            Arc::new(NoDisplay),
        )
        .with_actions(executor(&Arc::new(AtomicU64::new(0))))
        .with_linked_hosts(linked)
        .with_peer_sync(syncer.clone())
        .with_log(Arc::new(move |line: &str| {
            page_logs.lock().expect("the log").push(line.to_string())
        }));
        let alpha_tokens = alpha_scratch.path("alpha-tokens");
        private_dir(&alpha_tokens);
        let alpha_router = Server::new(
            arca_api::verbs(),
            TokenStore::load(&alpha_tokens).expect("tokens"),
        )
        .router()
        .merge(page.router(false));
        let alpha_socket = alpha_scratch.path("alpha.sock");

        let runtime = Runtime::new().expect("a runtime");
        let served_gamma = gamma_socket.clone();
        let gamma_router = gamma_server.router();
        runtime.spawn(async move {
            let _ = serve_unix(gamma_router, &served_gamma).await;
        });
        let served_alpha = alpha_socket.clone();
        runtime.spawn(async move {
            let _ = serve_unix(alpha_router, &served_alpha).await;
        });
        let until = Instant::now() + Duration::from_secs(5);
        while !gamma_socket.exists() || !alpha_socket.exists() {
            assert!(Instant::now() < until, "both servers started");
            std::thread::sleep(Duration::from_millis(10));
        }
        forward(forwarded, gamma_socket);
        (
            Remote {
                _alpha_scratch: alpha_scratch,
                _alpha_fake: alpha_fake,
                alpha,
                alpha_socket,
                alpha_state,
                gamma,
                gamma_runs,
                syncer,
                logs,
                _runtime: runtime,
            },
            owner,
        )
    }

    fn request(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let payload = body.map(|value| value.to_string()).unwrap_or_default();
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: box1.example.com\r\nConnection: close\r\nContent-Length: {}\r\n",
            payload.len()
        );
        if method == "POST" {
            head.push_str(&format!(
                "Origin: {ORIGIN}\r\nContent-Type: application/json\r\n"
            ));
        }
        head.push_str("\r\n");
        let mut stream = UnixStream::connect(&self.alpha_socket).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .expect("read timeout");
        stream
            .write_all(format!("{head}{payload}").as_bytes())
            .expect("write");
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).expect("read");
        let split = raw
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("a response head");
        let status = String::from_utf8_lossy(&raw[..split])
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|status| status.parse().ok())
            .expect("a status");
        let body = serde_json::from_slice(&raw[split + 4..]).unwrap_or(Value::Null);
        (status, body)
    }

    fn plans(&self) -> Value {
        let (status, body) = self.request("GET", "/approve/plans", None);
        assert_eq!(status, 200, "{body}");
        body
    }

    fn approve_gamma_action(&self, key: &SoftPasskey) -> (u16, Value) {
        let id = propose(&self.gamma.kernel);
        let row = self.plans()["plans"]
            .as_array()
            .expect("plans")
            .iter()
            .find(|row| row["id"] == json!(id) && row["linkedHost"] == json!("gamma"))
            .cloned()
            .expect("gamma's row, listed through the forwarded socket");
        let statement = row["statement"].as_str().expect("a statement");
        self.request(
            "POST",
            "/approve/approve",
            Some(
                json!({ "plan": id, "approval": action_approval(key, statement), "host": "gamma" }),
            ),
        )
    }
}

#[test]
fn alpha_approves_a_change_on_gamma_through_a_forwarded_socket() {
    let (remote, owner) = Remote::new("pk-e2e");

    // Before any sync, gamma does not know alpha's passkey.
    let (status, answer) = remote.approve_gamma_action(&owner);
    assert_eq!(status, 403, "{answer}");
    assert_eq!(answer["refusal"], json!("BadSignature"));
    assert_eq!(remote.gamma_runs.load(Ordering::SeqCst), 0);

    // The page host's sync: gamma's status asks for alpha's keys, so the first sync is kept as is.
    assert_eq!(
        remote.syncer.sync_all(),
        vec![(
            "gamma".to_string(),
            SyncOutcome::Synced("trusted".to_string())
        )]
    );
    assert_eq!(
        remote.gamma.status()["approval"]["peerKeys"]["keys"],
        json!(1)
    );
    // An approval made on alpha's page now runs on gamma, once.
    let (status, answer) = remote.approve_gamma_action(&owner);
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["host"], json!("gamma"));
    assert_eq!(remote.gamma_runs.load(Ordering::SeqCst), 1);

    // On demand from the page, an unchanged list is a no-op and leaves no card.
    let (status, synced) = remote.request("POST", "/approve/peer-keys/sync", Some(json!({})));
    assert_eq!(status, 200, "{synced}");
    assert_eq!(
        synced["hosts"],
        json!([{ "name": "gamma", "status": "unchanged" }])
    );
    assert_eq!(synced["peerKeys"], json!([]));

    // The owner adds a passkey on alpha: the next sync needs their approval, shown as a card.
    let added = SoftPasskey::from_seed(3);
    {
        let mut alpha = remote.alpha.lock().expect("the kernel lock");
        let mut lines = alpha.owner_passkey_lines();
        lines.extend(
            parse_passkeys(format!("{}\n", added.line(NAMESPACE)).as_bytes()).expect("a line"),
        );
        alpha.replace_owner_passkeys(lines);
    }
    // The open page's sync runs at most once per thirty seconds: inside that window it answers from the last one.
    let (status, synced) = remote.request("POST", "/approve/peer-keys/sync", Some(json!({})));
    assert_eq!(status, 200, "{synced}");
    assert_eq!(
        synced["hosts"],
        json!([{ "name": "gamma", "status": "unchanged" }])
    );
    // The page host's own timed sync is not held by that window.
    assert_eq!(
        remote.syncer.sync_all(),
        vec![(
            "gamma".to_string(),
            SyncOutcome::Synced("needs_approval".to_string())
        )]
    );
    let cards = remote.plans()["peerKeys"].clone();
    assert_eq!(cards.as_array().map(Vec::len), Some(1), "{cards}");
    let card = &cards[0];
    assert_eq!(card["linkedHost"], json!("gamma"));
    assert_eq!(card["title"], json!("Trust your new passkey list on gamma"));
    assert_eq!(card["keys"], json!(2));
    assert!(!cards.to_string().contains(&added.cred_text()));
    // The added passkey is not trusted on gamma until the owner approves the list.
    let (status, _) = remote.approve_gamma_action(&added);
    assert_eq!(status, 403);

    // The owner approves the card with the passkey gamma already trusts; the page relays it with the list.
    let statement = card["statement"].as_str().expect("a statement");
    let proof: Value =
        serde_json::from_slice(&owner.assert(&Assert::over(NAMESPACE, statement.as_bytes())))
            .expect("the proof is JSON");
    let (status, answer) = remote.request(
        "POST",
        "/approve/peer-keys",
        Some(json!({ "host": "gamma", "statement": statement, "proof": proof.clone() })),
    );
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["status"], json!("changed"));
    assert_eq!(answer["host"], json!("gamma"));
    assert_eq!(remote.plans()["peerKeys"], json!([]), "the card is gone");
    assert_eq!(
        remote.gamma.status()["approval"]["peerKeys"]["keys"],
        json!(2)
    );
    let (status, answer) = remote.approve_gamma_action(&added);
    assert_eq!(status, 200, "{answer}");
    assert_eq!(remote.gamma_runs.load(Ordering::SeqCst), 2);

    // The same signed card again is refused by gamma (its nonce is spent), and an unlisted host is never asked.
    let (status, answer) = remote.request(
        "POST",
        "/approve/peer-keys",
        Some(json!({ "host": "gamma", "statement": statement, "proof": proof })),
    );
    assert_eq!(status, 200, "an identical list is a no-op: {answer}");
    assert_eq!(answer["status"], json!("unchanged"));
    let (status, answer) = remote.request(
        "POST",
        "/approve/peer-keys",
        Some(json!({ "host": "delta", "statement": statement, "proof": {} })),
    );
    assert_eq!(status, 403, "{answer}");
    assert_eq!(answer["refusal"], json!("NotLinked"));

    // Alpha's own state directory holds no copy of gamma's file, and nothing it logged carries a line, a credential or a key.
    assert!(!remote.alpha_state.join(PEER_PASSKEYS_FILE).exists());
    let logs = remote.logs.lock().expect("the log").clone();
    assert!(!logs.is_empty(), "the syncs were logged");
    for key in [&owner, &added] {
        for line in &logs {
            assert!(!line.contains(&key.cred_text()), "{line}");
            assert!(!line.contains(&key.key_text()), "{line}");
            assert!(!line.contains("owner namespaces="), "{line}");
        }
    }
}

#[test]
fn the_page_host_does_not_sync_to_a_host_that_keeps_another_page_hosts_keys() {
    let (remote, _owner) = Remote::keeping("pk-other-from", "delta");
    assert_eq!(
        remote.syncer.sync_all(),
        vec![("gamma".to_string(), SyncOutcome::NotAsked)]
    );
    assert!(!remote.gamma.file.exists(), "nothing was sent");
    assert_eq!(remote.plans()["peerKeys"], json!([]));
}

#[test]
fn the_page_host_syncs_to_a_host_that_names_it_in_another_letter_case() {
    let (remote, _owner) = Remote::keeping("pk-case", &HOST.to_ascii_uppercase());
    assert_ne!(HOST, HOST.to_ascii_uppercase());
    assert_eq!(
        remote.syncer.sync_all(),
        vec![(
            "gamma".to_string(),
            SyncOutcome::Synced("trusted".to_string())
        )]
    );
    assert!(remote.gamma.file.exists(), "the list was sent and kept");
}

#[test]
fn the_approval_page_renders_the_passkey_list_card_and_posts_a_linked_rows_host() {
    let html = include_str!("../src/approve_page.html");
    assert!(html.contains("for (const card of peerKeys) target.append(peerKeysCard(card));"));
    assert!(
        html.contains(
            "JSON.stringify({ host: card.linkedHost, statement: card.statement, proof })"
        )
    );
    assert!(html.contains("{ plan: id, approval, host: plan.linkedHost }"));
}
