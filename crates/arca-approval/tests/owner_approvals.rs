//! Group 7's owner-approval titles (droidd's owner-approvals suite), at the verifier the kernel holds: the SSH signers file
//! and the passkeys file read back through the scratch injection, the nonce file and the sign counts in the test's own
//! scratch directory. Each test is named for its title with a `g7_` prefix. Every key is a software key from a fixed seed;
//! no test prints a proof, a key or an approval string.

mod common;
#[path = "common/webauthn.rs"]
mod webauthn;

use std::fs;
use std::sync::{Arc, Barrier, Mutex};

use arca_approval::ApprovalRefusal::{
    BadSignature, Expired, NoUserVerification, Replayed, SignCountRegressed, Storage, WrongAction,
    WrongHost, WrongOrigin, WrongParams, WrongRp, WrongTarget,
};
use arca_approval::{
    ARCA_NAMESPACE, ApprovalRefusal, ApprovalVerifier, ArcaAction, FileNonces, FileSignCounts,
    NonceStore, OwnerKeys, PASSKEY_COUNTS_FILE, PasskeyVerifier, SshSigVerifier, TrustRoot,
    load_passkeys, load_signers, parse_statement, passkeys_trust, signers_trust, split_approval,
};
use arca_kernel::canon::{canonical_json, params_digest};
use common::{HOST, NONCE, NOW, Scratch, SkKey, approval_of, args, at, statement_with, target};
use common::{owner_uid, write_mode};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use webauthn::{Assert, BE, BS, SForm, SoftPasskey, UP, UV, passkey_approval};

const NONCE_B: &str = "1123456789abcdef0123456789abcdef";
const NONCE_C: &str = "2123456789abcdef0123456789abcdef";
const NONCE_D: &str = "3123456789abcdef0123456789abcdef";
const NONCES_FILE: &str = "approval-nonces.jsonl";

type Verifier = ApprovalVerifier<OwnerKeys, FileNonces>;

fn projects() -> Vec<String> {
    vec!["arcak-scratch-a".to_string()]
}

/// The verifier the kernel builds over `scratch`: the SSH keys `ssh`, the passkeys file text `passkeys` (a file that
/// refuses makes a passkey verifier that refuses, as the kernel does), the nonce file and the sign counts. Opening it again
/// after the last one is dropped is a restart.
fn verifier(scratch: &Scratch, ssh: &[&SkKey], passkeys: &str) -> Verifier {
    let file = scratch.path("owner-passkeys");
    write_mode(&file, passkeys.as_bytes(), 0o600);
    let (path, trust) = passkeys_trust(true, &projects(), Some(file)).expect("the injection");
    let passkeys = match load_passkeys(&path, &trust) {
        Ok(lines) => PasskeyVerifier::new(
            lines,
            FileSignCounts::open(&scratch.path(PASSKEY_COUNTS_FILE)).expect("the counts open"),
        ),
        Err(refusal) => PasskeyVerifier::refusing(refusal),
    };
    with_passkeys(scratch, ssh, passkeys)
}

/// The verifier over `scratch` with the SSH keys `ssh` and the passkey verifier `passkeys`.
fn with_passkeys(scratch: &Scratch, ssh: &[&SkKey], passkeys: PasskeyVerifier) -> Verifier {
    ApprovalVerifier::new(
        HOST.to_string(),
        OwnerKeys::new(ssh_for(scratch, ssh), passkeys),
        FileNonces::open(&scratch.path(NONCES_FILE)).expect("the nonces open"),
    )
}

/// The SSH verifier for `keys`, through a signers file in `scratch` read back through the scratch injection.
fn ssh_for(scratch: &Scratch, keys: &[&SkKey]) -> SshSigVerifier {
    let signers = scratch.path("owner-signers");
    let text: String = keys
        .iter()
        .map(|k| format!("{}\n", k.line(ARCA_NAMESPACE)))
        .collect();
    write_mode(&signers, text.as_bytes(), 0o600);
    let (path, trust) = signers_trust(true, &projects(), Some(signers)).expect("the injection");
    SshSigVerifier::new(load_signers(&path, &trust).expect("the signers file loads"))
}

fn lines_for(keys: &[&SoftPasskey]) -> String {
    keys.iter()
        .map(|k| format!("{}\n", k.line(ARCA_NAMESPACE)))
        .collect()
}

fn statement(nonce: &str) -> String {
    statement_with(json!({ "nonce": nonce }))
}

fn ssh_approval(key: &SkKey, statement: &str) -> String {
    approval_of(statement, &key.sign(ARCA_NAMESPACE, statement.as_bytes()))
}

fn passkey(key: &SoftPasskey, statement: &str, bend: impl FnOnce(&mut Assert)) -> String {
    let mut a = Assert::over(ARCA_NAMESPACE, statement.as_bytes());
    bend(&mut a);
    passkey_approval(statement, &key.assert(&a))
}

/// The default call: delete of the default plan with its arguments. Returns the key that signed.
fn check<V: arca_approval::SignatureVerifier, N: NonceStore>(
    v: &mut ApprovalVerifier<V, N>,
    approval: &str,
) -> Result<String, ApprovalRefusal> {
    v.check(ArcaAction::Delete, &target(), &args(), approval, NOW)
        .map(|spent| spent.key)
}

/// The nonce file's records: one line per spent nonce.
fn records(scratch: &Scratch) -> Vec<String> {
    fs::read_to_string(scratch.path(NONCES_FILE))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn g7_refuses_each_approval_it_must_with_owner_approval_spending_nothing() {
    let scratch = Scratch::new("g7-refusals");
    let owner = SkKey::from_seed(1);
    let mut v = verifier(&scratch, &[&owner], "");
    let s = statement(NONCE);
    let signed = |overrides: Value| {
        let mut fields = overrides;
        fields["nonce"] = json!(NONCE);
        ssh_approval(&owner, &statement_with(fields))
    };
    let cases: Vec<(&str, String, ApprovalRefusal)> = vec![
        (
            "another key",
            ssh_approval(&SkKey::from_seed(2), &s),
            BadSignature,
        ),
        (
            "another host",
            signed(json!({ "host": "node2" })),
            WrongHost,
        ),
        (
            "another action",
            signed(json!({ "action": "publish" })),
            WrongAction,
        ),
        (
            "another target",
            signed(json!({ "target": "plan-other" })),
            WrongTarget,
        ),
        (
            "other arguments",
            signed(json!({ "paramsDigest": "a".repeat(64) })),
            WrongParams,
        ),
        (
            "expired",
            signed(json!({ "issuedAt": at(-10 * 60_000), "expiresAt": at(-60_000) })),
            Expired,
        ),
    ];
    for (name, approval, want) in cases {
        assert_eq!(check(&mut v, &approval), Err(want.clone()), "{name}");
        // owner_approval is a policy refusal that says what is wrong in plain words, never quoting the approval.
        let text = want.to_string();
        assert!(text.starts_with("the approval"), "{name}: {text}");
        assert!(!text.contains(&approval), "{name}");
    }
    assert!(records(&scratch).is_empty(), "no refusal spent a nonce");
    check(&mut v, &ssh_approval(&owner, &s)).expect("the genuine approval then spends");
    assert_eq!(records(&scratch).len(), 1);
}

#[test]
fn g7_a_valid_approval_is_spent_once_with_its_row_and_two_concurrent_checks_of_one_give_one() {
    let scratch = Scratch::new("g7-once");
    let owner = SkKey::from_seed(1);
    let v = Arc::new(Mutex::new(verifier(&scratch, &[&owner], "")));
    let approval = ssh_approval(&owner, &statement(NONCE));
    let gate = Arc::new(Barrier::new(2));
    let outcomes: Vec<Result<String, ApprovalRefusal>> = (0..2)
        .map(|_| {
            let (v, gate, approval) = (Arc::clone(&v), Arc::clone(&gate), approval.clone());
            std::thread::spawn(move || {
                gate.wait();
                check(&mut *v.lock().expect("the verifier"), &approval)
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|handle| handle.join().expect("the check thread"))
        .collect();
    assert_eq!(
        outcomes.iter().filter(|o| o.is_ok()).count(),
        1,
        "one spends"
    );
    assert!(outcomes.contains(&Err(Replayed)), "the other is a replay");
    let rows = records(&scratch);
    assert_eq!(rows.len(), 1, "one record for the spend");
    assert!(rows[0].contains(NONCE), "the record names the nonce");
}

/// A nonce store whose spend cannot be made durable.
struct FailingNonces;

impl NonceStore for FailingNonces {
    fn spent(&self, _nonce: &str) -> bool {
        false
    }
    fn spend(&mut self, _nonce: &str, _at_ms: u64) -> Result<(), ApprovalRefusal> {
        Err(Storage("the disk is full".to_string()))
    }
}

#[test]
fn g7_the_spend_is_one_transaction_a_record_that_fails_spends_nothing_and_a_spent_row_is_never_changed()
 {
    // Arca's spend record is the nonce file's line (no database, no caller transaction): the spend is that one append.
    let scratch = Scratch::new("g7-transaction");
    let owner = SkKey::from_seed(1);
    let keys = OwnerKeys::new(
        ssh_for(&scratch, &[&owner]),
        PasskeyVerifier::refusing(ApprovalRefusal::NoVerifier),
    );
    let mut failing = ApprovalVerifier::new(HOST.to_string(), keys, FailingNonces);
    let approval = ssh_approval(&owner, &statement(NONCE));
    assert!(matches!(check(&mut failing, &approval), Err(Storage(_))));
    drop(failing);

    let mut v = verifier(&scratch, &[&owner], "");
    check(&mut v, &approval).expect("the failed spend spent nothing: the approval still spends");
    let first = fs::read(scratch.path(NONCES_FILE)).expect("the nonce file");
    check(&mut v, &ssh_approval(&owner, &statement(NONCE_B))).expect("a second approval spends");
    assert_eq!(check(&mut v, &approval), Err(Replayed));
    let after = fs::read(scratch.path(NONCES_FILE)).expect("the nonce file");
    assert!(
        after.starts_with(&first),
        "the spent record is never changed or removed: the file only grows"
    );
    assert_eq!(records(&scratch).len(), 2);
}

#[test]
fn g7_a_spent_nonce_stays_spent_across_a_rollback_of_the_desired_state() {
    // Arca keeps the spent nonces in their own append-only file in the state directory, outside the desired state a
    // rollback restores: a restart (the rollback's restart included) reads the file back, and the replay is refused.
    let scratch = Scratch::new("g7-rollback");
    let owner = SkKey::from_seed(1);
    let approval = ssh_approval(&owner, &statement(NONCE));
    {
        let mut v = verifier(&scratch, &[&owner], "");
        check(&mut v, &approval).expect("spent before the rollback");
    }
    let mut v = verifier(&scratch, &[&owner], "");
    assert_eq!(check(&mut v, &approval), Err(Replayed));
}

#[test]
fn g7_a_key_line_lists_its_namespaces_and_its_key_signs_only_in_what_it_lists() {
    let s = statement(NONCE);
    for (namespaces, approves) in [
        ("arca-kernel-approve", true),
        ("droid-upgrade,arca-kernel-approve", true),
        ("droid-upgrade", false),
        ("arca-approve", false),
        ("droid-upgrade,arca-approve", false),
    ] {
        let scratch = Scratch::new("g7-namespaces");
        let owner = SkKey::from_seed(1);
        let signers = scratch.path("owner-signers");
        write_mode(
            &signers,
            format!("{}\n", owner.line(namespaces)).as_bytes(),
            0o600,
        );
        let (path, trust) = signers_trust(true, &projects(), Some(signers)).expect("injection");
        let lines = load_signers(&path, &trust).expect("the file is trusted");
        assert_eq!(lines.len(), usize::from(approves), "{namespaces}");
        let keys = OwnerKeys::new(
            SshSigVerifier::new(lines),
            PasskeyVerifier::refusing(ApprovalRefusal::NoVerifier),
        );
        let nonces = FileNonces::open(&scratch.path(NONCES_FILE)).expect("nonces");
        let mut v = ApprovalVerifier::new(HOST.to_string(), keys, nonces);
        assert_eq!(
            check(&mut v, &ssh_approval(&owner, &s)).is_ok(),
            approves,
            "{namespaces}"
        );
        // A signature made in another namespace never approves here, whatever the line lists.
        let other = approval_of(&s, &owner.sign("droid-upgrade", s.as_bytes()));
        assert!(check(&mut v, &other).is_err(), "{namespaces}");
    }
}

#[test]
fn g7_signers_trust_root_by_default_and_a_test_signers_file_only_on_a_scratch_kernel() {
    let (path, trust) = signers_trust(false, &[], None).expect("no injection");
    assert_eq!(path, std::path::PathBuf::from("/etc/arca/owner-signers"));
    assert_eq!(trust, TrustRoot::Root, "root's file by default");
    let named = std::path::PathBuf::from("/tmp/arca-g7-not-scratch/owner-signers");
    for (scratch, names) in [
        (false, vec!["arcak-scratch-a"]),
        (true, vec!["arcak-scratch-a", "arcak-svc"]),
        (true, vec![]),
    ] {
        let names: Vec<String> = names.into_iter().map(str::to_string).collect();
        assert!(
            matches!(
                signers_trust(scratch, &names, Some(named.clone())),
                Err(ApprovalRefusal::Untrusted(_))
            ),
            "a test signers file is refused unless the kernel is scratch: {names:?}"
        );
    }
    let dir = Scratch::new("g7-trust");
    let file = dir.path("owner-signers");
    write_mode(
        &file,
        SkKey::from_seed(7).line(ARCA_NAMESPACE).as_bytes(),
        0o600,
    );
    assert!(
        matches!(
            load_signers(&file, &TrustRoot::Root),
            Err(ApprovalRefusal::Untrusted(_))
        ),
        "a file the test user owns is never trusted as root's"
    );
    let scratch_root = TrustRoot::Scratch {
        uid: owner_uid(&dir.dir),
        up_to: dir.dir.clone(),
    };
    assert_eq!(
        load_signers(&file, &scratch_root).expect("scratch").len(),
        1
    );
}

#[test]
fn g7_params_digest_statement_parse_and_the_owner_approval_refusal() {
    let call = json!({ "plan": args()["plan"], "approval": "left out" });
    let without = json!({ "plan": args()["plan"] });
    let text = canonical_json(&without).expect("canonical");
    let want: String = Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(params_digest(&without).expect("digest"), want);
    let s = statement(NONCE);
    let parsed = parse_statement(s.as_bytes()).expect("the statement parses");
    assert_eq!(parsed.host, HOST);
    assert_eq!(parsed.action, ArcaAction::Delete);
    assert_eq!(parsed.target, target());
    assert_eq!(parsed.params_digest, want);
    assert_eq!(parsed.nonce, NONCE);
    assert_eq!(parsed.issued_at, at(0));
    // The digest leaves the approval out: the call that carries its approval spends it.
    let scratch = Scratch::new("g7-digest");
    let owner = SkKey::from_seed(1);
    let mut v = verifier(&scratch, &[&owner], "");
    let approval = ssh_approval(&owner, &s);
    let mut with = call.clone();
    with["approval"] = json!(approval);
    v.check(ArcaAction::Delete, &target(), &with, &approval, NOW)
        .expect("the digest leaves the approval key out");
    assert!(split_approval(&approval).is_ok());
    assert_eq!(
        WrongParams.to_string(),
        "the approval names other arguments",
        "a policy refusal in plain words"
    );
}

#[test]
fn g7_a_passkey_approval_spends_once_beside_the_ssh_path_with_via_count_and_synced() {
    let scratch = Scratch::new("g7-pk-beside");
    let (owner, key) = (SkKey::from_seed(1), SoftPasskey::from_seed(1));
    let mut v = verifier(&scratch, &[&owner], &lines_for(&[&key]));
    let approval = passkey(&key, &statement(NONCE), |a| a.sign_count = 3);
    let spent = check(&mut v, &approval).expect("the passkey approval spends");
    assert_eq!(
        spent,
        format!("passkey:{}", key.fingerprint()),
        "via passkey"
    );
    assert_eq!(check(&mut v, &approval), Err(SignCountRegressed), "once");
    let ssh = ssh_approval(&owner, &statement(NONCE_B));
    assert!(
        check(&mut v, &ssh)
            .expect("an SSH approval still spends beside it")
            .starts_with("SHA256:")
    );
    let synced = passkey(&key, &statement(NONCE_C), |a| {
        (a.sign_count, a.flags) = (0, UP | UV | BE | BS)
    });
    let counts = fs::read_to_string(scratch.path(PASSKEY_COUNTS_FILE)).expect("the counts");
    check(&mut v, &synced).expect_err("0 after the passkey counted is refused");
    assert_eq!(
        fs::read_to_string(scratch.path(PASSKEY_COUNTS_FILE)).expect("the counts"),
        counts,
        "a refused count writes nothing"
    );
    assert_eq!(
        records(&scratch).len(),
        2,
        "two spends: the passkey's and the ssh one"
    );
}

#[test]
fn g7_refuses_each_passkey_approval_it_must_with_owner_approval_spending_and_counting_nothing() {
    let key = SoftPasskey::from_seed(1);
    let scratch = Scratch::new("g7-pk-refusals");
    let mut v = verifier(&scratch, &[], &lines_for(&[&key]));
    let s = statement(NONCE);
    type Bend = Box<dyn FnOnce(&mut Assert)>;
    let cases: Vec<(&str, Bend, ApprovalRefusal)> = vec![
        (
            "no user verification",
            Box::new(|a| (a.sign_count, a.flags) = (50, UP)),
            NoUserVerification,
        ),
        (
            "another origin",
            Box::new(|a| {
                a.sign_count = 50;
                a.origin = Some("https://box1.example.com:9443".to_string());
            }),
            WrongOrigin,
        ),
        (
            "another relying party",
            Box::new(|a| {
                a.sign_count = 50;
                a.rp_id = Some("box2.example.com".to_string());
            }),
            WrongRp,
        ),
        (
            "another challenge",
            Box::new(|a| {
                a.sign_count = 50;
                a.challenge = Some([7; 32]);
            }),
            BadSignature,
        ),
    ];
    for (name, bend, want) in cases {
        assert_eq!(check(&mut v, &passkey(&key, &s, bend)), Err(want), "{name}");
    }
    assert!(records(&scratch).is_empty(), "nothing spent");
    assert!(
        fs::read_to_string(scratch.path(PASSKEY_COUNTS_FILE))
            .unwrap_or_default()
            .is_empty(),
        "nothing counted"
    );
    check(&mut v, &passkey(&key, &s, |a| a.sign_count = 1))
        .expect("count 1 still spends: no refusal counted 50");
}

#[test]
fn g7_a_missing_untrusted_or_malformed_passkeys_file_refuses_passkey_approvals_while_ssh_spends() {
    let (owner, key) = (SkKey::from_seed(1), SoftPasskey::from_seed(1));
    for (name, text, mode) in [
        (
            "malformed",
            format!("{}\nnot a passkey line\n", key.line(ARCA_NAMESPACE)),
            0o600,
        ),
        ("untrusted", lines_for(&[&key]), 0o666),
    ] {
        let scratch = Scratch::new("g7-bad-passkeys");
        let file = scratch.path("owner-passkeys");
        write_mode(&file, text.as_bytes(), mode);
        let (path, trust) = passkeys_trust(true, &projects(), Some(file)).expect("injection");
        let refusal = load_passkeys(&path, &trust).expect_err(name);
        // As the kernel builds it: the refusing file makes a passkey verifier that refuses.
        let mut v = with_passkeys(
            &scratch,
            &[&owner],
            PasskeyVerifier::refusing(refusal.clone()),
        );
        let s = statement(NONCE);
        assert_eq!(
            check(&mut v, &passkey(&key, &s, |_| {})),
            Err(refusal),
            "{name}"
        );
        check(&mut v, &ssh_approval(&owner, &s)).expect("the SSH approval still spends");
    }
    // A missing passkeys file: no passkey is trusted, and the SSH path still spends.
    let scratch = Scratch::new("g7-no-passkeys");
    let mut v = verifier(&scratch, &[&owner], "");
    let s = statement(NONCE);
    assert!(check(&mut v, &passkey(&key, &s, |_| {})).is_err());
    check(&mut v, &ssh_approval(&owner, &s)).expect("the SSH approval spends");
}

#[test]
fn g7_passkey_sign_counts_rise_above_the_kept_one_a_preview_counts_nothing_and_a_refusal_after_the_signature_uses_it_up()
 {
    let key = SoftPasskey::from_seed(1);
    let scratch = Scratch::new("g7-counts");
    let mut v = verifier(&scratch, &[], &lines_for(&[&key]));
    let at_count =
        |nonce: &str, count: u32| passkey(&key, &statement(nonce), move |a| a.sign_count = count);
    check(&mut v, &at_count(NONCE, 0)).expect("0 while the kept count is 0");
    let five = at_count(NONCE_B, 5);
    // The approval page's preview checks all of it and neither counts nor spends: the spend that follows still passes.
    v.preview(ArcaAction::Delete, &target(), &args(), &five, NOW)
        .expect("the preview passes");
    v.preview(ArcaAction::Delete, &target(), &args(), &five, NOW)
        .expect("a second preview passes: nothing was counted");
    check(&mut v, &five).expect("5 is above the kept 0");
    for count in [5, 4, 0] {
        assert_eq!(
            check(&mut v, &at_count(NONCE_C, count)),
            Err(SignCountRegressed),
            "{count}"
        );
    }
    // A refusal before the signature verifies leaves the count as it was.
    let wrong_rp = passkey(&key, &statement(NONCE_C), |a| {
        a.sign_count = 40;
        a.rp_id = Some("box2.example.com".to_string());
    });
    assert_eq!(check(&mut v, &wrong_rp), Err(WrongRp));
    check(&mut v, &at_count(NONCE_C, 6)).expect("6: the refusals left the count at 5");
    // Arca's deliberate difference (its own pinned design): a counting passkey's approval refused for a bound field, after
    // its signature verified, is used up.
    let seven = at_count(NONCE_D, 7);
    let other = json!({ "plan": args()["plan"], "note": "another call" });
    assert_eq!(
        v.check(ArcaAction::Delete, &target(), &other, &seven, NOW)
            .map(|_| ()),
        Err(WrongParams)
    );
    assert_eq!(check(&mut v, &seven), Err(SignCountRegressed));
}

#[test]
fn g7_a_spent_approval_re_encoded_as_its_high_s_twin_verifies_and_is_refused_as_a_replay_by_its_nonce()
 {
    let key = SoftPasskey::from_seed(1);
    let scratch = Scratch::new("g7-high-s");
    let mut v = verifier(&scratch, &[], &lines_for(&[&key]));
    let s = statement(NONCE);
    let synced = |form| move |a: &mut Assert| (a.flags, a.s_form) = (UP | UV | BE | BS, form);
    let low = passkey(&key, &s, synced(SForm::Low));
    let high = passkey(&key, &s, synced(SForm::High));
    assert_ne!(low, high, "two encodings of one assertion");
    v.preview(ArcaAction::Delete, &target(), &args(), &high, NOW)
        .expect("the high-S twin verifies");
    check(&mut v, &low).expect("the low-S approval spends");
    assert_eq!(check(&mut v, &high), Err(Replayed), "the twin is a replay");
}

#[test]
fn g7_statement_parse_without_the_droidd_key_and_with_it_refused() {
    let s = statement(NONCE);
    assert!(
        parse_statement(s.as_bytes()).is_ok(),
        "no droidd key parses"
    );
    // droidd reads its own name when present; an Arca statement names no droidd, so a statement with the key is refused.
    let with = statement_with(json!({ "droidd": "box1" }));
    assert!(matches!(
        parse_statement(with.as_bytes()),
        Err(ApprovalRefusal::Malformed(_))
    ));
}
