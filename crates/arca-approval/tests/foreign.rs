//! Another kernel's passkeys file, trusted read-only: the file rule, the origin filter, and the split between the approval
//! check (which reads it) and the sign-in check (which never does). Every key is a software P-256 key from a fixed seed, and
//! every file lives in the test's own scratch directory.

mod common;
#[path = "common/webauthn.rs"]
mod webauthn;

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;

use arca_approval::{
    ARCA_NAMESPACE, ApprovalRefusal, FileSignCounts, ForeignPasskeys, PASSKEY_COUNTS_FILE,
    PasskeyVerifier, SignatureVerifier,
};
use common::{Scratch, write_mode};
use webauthn::{Assert, ORIGIN, SoftPasskey};

const STATEMENT: &[u8] = br#"{"action":"service_create","host":"beta.example"}"#;

fn foreign(paths: Vec<PathBuf>) -> ForeignPasskeys {
    ForeignPasskeys::new(paths, vec![ORIGIN.to_string()])
}

fn one_reason(trust: &ForeignPasskeys) -> (usize, Option<String>) {
    let (lines, states) = trust.load();
    assert_eq!(states.len(), 1);
    assert_eq!(lines.len(), states[0].keys);
    (states[0].keys, states[0].reason.clone())
}

#[test]
fn a_trusted_file_adds_its_lines_for_the_listed_origins_only() {
    let scratch = Scratch::new("foreign-origins");
    let owner = SoftPasskey::from_seed(1);
    let mut elsewhere = SoftPasskey::from_seed(3);
    elsewhere.rp_id = "other.example.com".to_string();
    elsewhere.origin = "https://other.example.com".to_string();
    let path = scratch.path("owner-passkeys");
    let text = format!(
        "# alpha's passkeys\n{}\n{}\n",
        owner.line(ARCA_NAMESPACE),
        elsewhere.line(ARCA_NAMESPACE)
    );
    write_mode(&path, text.as_bytes(), 0o644);
    let (lines, states) = foreign(vec![path.clone(), path]).load();
    assert_eq!(
        lines.len(),
        1,
        "one line answers the listed origin, and it is not added twice"
    );
    assert_eq!(lines[0].origin, ORIGIN);
    assert_eq!(states[0].keys, 1);
    assert_eq!(states[0].reason, None);
    assert_eq!(states[1].keys, 0);

    write_mode(
        &scratch.path("other-only"),
        format!("{}\n", elsewhere.line(ARCA_NAMESPACE)).as_bytes(),
        0o600,
    );
    assert_eq!(
        one_reason(&foreign(vec![scratch.path("other-only")])),
        (
            0,
            Some("no passkey in it answers a listed origin".to_string())
        )
    );
}

#[test]
fn a_file_that_cannot_be_trusted_adds_no_key_and_names_why() {
    let scratch = Scratch::new("foreign-refused");
    let owner = SoftPasskey::from_seed(1);
    let line = format!("{}\n", owner.line(ARCA_NAMESPACE));

    assert_eq!(
        one_reason(&foreign(vec![scratch.path("missing")])),
        (0, Some("the file is missing".to_string()))
    );

    let unreadable = scratch.path("unreadable");
    write_mode(&unreadable, line.as_bytes(), 0o000);
    assert_eq!(
        one_reason(&foreign(vec![unreadable])),
        (
            0,
            Some("the file cannot be read by this kernel".to_string())
        )
    );

    let writable = scratch.path("writable");
    write_mode(&writable, line.as_bytes(), 0o664);
    assert_eq!(
        one_reason(&foreign(vec![writable])),
        (
            0,
            Some("the file is writable by group or others".to_string())
        )
    );

    let target = scratch.path("target");
    write_mode(&target, line.as_bytes(), 0o600);
    let link = scratch.path("link");
    symlink(&target, &link).expect("a link");
    assert_eq!(
        one_reason(&foreign(vec![link])),
        (
            0,
            Some("the file cannot be opened, or it is a link".to_string())
        )
    );

    let dir = scratch.path("dir");
    fs::create_dir(&dir).expect("a directory");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("mode");
    assert_eq!(
        one_reason(&foreign(vec![dir])),
        (0, Some("the file is not a regular file".to_string()))
    );

    let malformed = scratch.path("malformed");
    write_mode(
        &malformed,
        format!("{line}not a passkey line\n").as_bytes(),
        0o600,
    );
    let (keys, reason) = one_reason(&foreign(vec![malformed]));
    assert_eq!(keys, 0, "one bad line refuses the whole file");
    let reason = reason.expect("a reason");
    assert!(
        reason.starts_with("line 2 of the passkeys file"),
        "{reason}"
    );
    assert!(
        !reason.contains(&owner.cred_text()),
        "the reason never echoes a credential"
    );
}

#[test]
fn approvals_read_the_trusted_file_now_and_sign_in_never_does() {
    let scratch = Scratch::new("foreign-verify");
    let owner = SoftPasskey::from_seed(1);
    let path = scratch.path("alpha-passkeys");
    write_mode(&path, b"", 0o600);
    let verifier = PasskeyVerifier::new(
        Vec::new(),
        FileSignCounts::open(&scratch.path(PASSKEY_COUNTS_FILE)).expect("counts"),
    )
    .with_foreign(foreign(vec![path.clone()]));
    let mut assert = Assert::over(ARCA_NAMESPACE, STATEMENT);
    assert.sign_count = 1;
    let proof = owner.assert(&assert);

    assert_eq!(
        verifier.verify(ARCA_NAMESPACE, STATEMENT, &proof),
        Err(ApprovalRefusal::NoVerifier),
        "an empty file trusts nothing"
    );

    // The owner enrols on the other kernel: the next check here reads it, with no restart.
    write_mode(
        &path,
        format!("{}\n", owner.line(ARCA_NAMESPACE)).as_bytes(),
        0o600,
    );
    assert_eq!(
        verifier.verify_own(ARCA_NAMESPACE, STATEMENT, &proof),
        Err(ApprovalRefusal::NoVerifier),
        "sign-in sees only this kernel's own passkeys"
    );
    assert_eq!(
        verifier.verify(ARCA_NAMESPACE, STATEMENT, &proof),
        Ok(format!("passkey:{}", owner.fingerprint()))
    );
    assert_eq!(
        verifier.verify(ARCA_NAMESPACE, STATEMENT, &proof),
        Err(ApprovalRefusal::SignCountRegressed),
        "this kernel keeps its own sign count for a trusted passkey"
    );

    // The owner removes it there: it stops verifying here.
    write_mode(&path, b"", 0o600);
    assert.sign_count = 2;
    assert_eq!(
        verifier.verify(ARCA_NAMESPACE, STATEMENT, &owner.assert(&assert)),
        Err(ApprovalRefusal::NoVerifier)
    );
}

#[test]
fn a_verifier_whose_own_file_was_refused_refuses_trusted_keys_too() {
    let scratch = Scratch::new("foreign-refusing");
    let owner = SoftPasskey::from_seed(1);
    let path = scratch.path("alpha-passkeys");
    write_mode(
        &path,
        format!("{}\n", owner.line(ARCA_NAMESPACE)).as_bytes(),
        0o600,
    );
    let refusal = ApprovalRefusal::Untrusted("the passkeys file is writable by others".to_string());
    let verifier = PasskeyVerifier::refusing(refusal.clone()).with_foreign(foreign(vec![path]));
    let proof = owner.assert(&Assert::over(ARCA_NAMESPACE, STATEMENT));
    assert_eq!(
        verifier.verify(ARCA_NAMESPACE, STATEMENT, &proof),
        Err(refusal)
    );
}

#[test]
fn the_status_reuses_the_last_read_within_its_window_and_reads_again_after() {
    let scratch = Scratch::new("foreign-recent");
    let owner = SoftPasskey::from_seed(1);
    let path = scratch.path("owner-passkeys");
    write_mode(
        &path,
        format!("{}\n", owner.line(ARCA_NAMESPACE)).as_bytes(),
        0o600,
    );
    let trust = foreign(vec![path.clone()]);
    // A clone (the status's copy) shares the read the original (the verifier's) made.
    let status = trust.clone();
    assert_eq!(trust.load().1[0].keys, 1);
    fs::remove_file(&path).expect("the file is removed");
    let window = std::time::Duration::from_secs(60);
    assert_eq!(
        status.recent_states(window)[0].keys,
        1,
        "the last read is reused, the file is not read again"
    );
    let states = status.recent_states(std::time::Duration::ZERO);
    assert_eq!(states[0].keys, 0, "an old read is replaced by a new one");
    assert_eq!(states[0].reason.as_deref(), Some("the file is missing"));
    assert_eq!(
        trust.recent_states(window)[0].reason.as_deref(),
        Some("the file is missing"),
        "the new read is shared back"
    );
}
