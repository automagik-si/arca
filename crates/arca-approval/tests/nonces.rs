//! Persistent nonces: a spend is durable before it returns, a spent nonce stays spent across a reopen, and a file that is
//! not exactly what this crate writes refuses to open. Every file is a temporary file the test made.

mod common;

use std::fs;
use std::os::unix::fs::MetadataExt;

use arca_approval::{
    ApprovalRefusal, ApprovalVerifier, ArcaAction, FileNonces, NonceStore, SshSigVerifier,
};
use common::{
    HOST, NONCE, NOW, Scratch, SkKey, approval_of, args, statement_with, target, write_mode,
};

fn other_nonce() -> String {
    "fedcba9876543210fedcba9876543210".to_string()
}

#[test]
fn a_new_nonce_file_is_created_with_mode_0600() {
    let dir = Scratch::new("mode-new");
    let path = dir.path("nonces.jsonl");
    FileNonces::open(&path).expect("a new store opens");
    let mode = fs::metadata(&path).expect("the file exists").mode() & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn a_spent_nonce_is_refused_after_reopening_the_file() {
    let dir = Scratch::new("reopen");
    let path = dir.path("nonces.jsonl");
    {
        let mut store = FileNonces::open(&path).expect("opens");
        store.spend(NONCE, NOW).expect("the first spend is allowed");
    }
    let mut reopened = FileNonces::open(&path).expect("reopens");
    assert!(reopened.spent(NONCE));
    assert_eq!(reopened.spend(NONCE, NOW), Err(ApprovalRefusal::Replayed));
    assert!(!reopened.spent(&other_nonce()));
}

#[test]
fn the_record_is_on_disk_when_a_spend_returns() {
    // The action an approval admits runs only after `spend` returns, so the record must already be in the file then.
    let dir = Scratch::new("ondisk");
    let path = dir.path("nonces.jsonl");
    let mut store = FileNonces::open(&path).expect("opens");
    store.spend(NONCE, NOW).expect("spent");
    let on_disk = fs::read_to_string(&path).expect("the file is readable while the store is open");
    assert!(
        on_disk.contains(NONCE),
        "the spend's record is written before spend returns"
    );
    assert!(on_disk.ends_with('\n'), "a record is one terminated line");
}

#[test]
fn a_torn_last_record_is_dropped_and_its_nonce_is_not_spent() {
    // A record without its newline was never acknowledged (its spend never returned), so its approval was never allowed.
    let dir = Scratch::new("torn");
    let path = dir.path("nonces.jsonl");
    let complete = format!("{{\"at\":{NOW},\"nonce\":\"{NONCE}\"}}\n");
    let torn = format!("{{\"at\":{NOW},\"nonce\":\"{}\"", other_nonce());
    write_mode(&path, format!("{complete}{torn}").as_bytes(), 0o600);
    let store = FileNonces::open(&path).expect("the torn tail is dropped, not refused");
    assert!(store.spent(NONCE));
    assert!(!store.spent(&other_nonce()));
    assert_eq!(
        fs::read_to_string(&path).expect("readable"),
        complete,
        "the torn tail is truncated"
    );
}

#[test]
fn a_complete_record_that_does_not_parse_refuses_the_open() {
    let dir = Scratch::new("corrupt");
    let path = dir.path("nonces.jsonl");
    write_mode(&path, b"{\"at\":1,\"nonce\":\"not-hex\"}\n", 0o600);
    assert!(matches!(
        FileNonces::open(&path),
        Err(ApprovalRefusal::Storage(_))
    ));
}

#[test]
fn a_record_that_is_not_in_canonical_form_refuses_the_open() {
    let dir = Scratch::new("noncanon");
    let path = dir.path("nonces.jsonl");
    write_mode(
        &path,
        format!("{{ \"at\": {NOW}, \"nonce\": \"{NONCE}\" }}\n").as_bytes(),
        0o600,
    );
    assert!(matches!(
        FileNonces::open(&path),
        Err(ApprovalRefusal::Storage(_))
    ));
}

#[test]
fn a_nonce_repeated_in_the_file_refuses_the_open() {
    let dir = Scratch::new("repeat");
    let path = dir.path("nonces.jsonl");
    let record = format!("{{\"at\":{NOW},\"nonce\":\"{NONCE}\"}}\n");
    write_mode(&path, format!("{record}{record}").as_bytes(), 0o600);
    assert!(matches!(
        FileNonces::open(&path),
        Err(ApprovalRefusal::Storage(_))
    ));
}

#[test]
fn a_nonce_file_with_another_mode_is_refused() {
    let dir = Scratch::new("mode-other");
    let path = dir.path("nonces.jsonl");
    write_mode(&path, b"", 0o644);
    assert!(matches!(
        FileNonces::open(&path),
        Err(ApprovalRefusal::Untrusted(_))
    ));
}

#[test]
fn a_symlinked_nonce_file_is_refused() {
    let dir = Scratch::new("symlink");
    let real = dir.path("real.jsonl");
    write_mode(&real, b"", 0o600);
    let link = dir.path("nonces.jsonl");
    std::os::unix::fs::symlink(&real, &link).expect("the symlink is made");
    assert!(matches!(
        FileNonces::open(&link),
        Err(ApprovalRefusal::Untrusted(_))
    ));
}

#[test]
fn a_dangling_symlink_at_the_nonce_path_is_refused_and_its_target_is_not_created() {
    // An open with create would create the file the link names. The refusal must come before any create.
    let dir = Scratch::new("dangling");
    let target = dir.path("created-by-open.jsonl");
    let link = dir.path("nonces.jsonl");
    std::os::unix::fs::symlink(&target, &link).expect("the symlink is made");
    assert!(matches!(
        FileNonces::open(&link),
        Err(ApprovalRefusal::Untrusted(_))
    ));
    assert!(
        fs::symlink_metadata(&target).is_err(),
        "the link's target must not be created"
    );
}

#[test]
fn a_second_open_of_a_held_file_is_refused() {
    let dir = Scratch::new("locked");
    let path = dir.path("nonces.jsonl");
    let _held = FileNonces::open(&path).expect("the first open holds the file");
    assert!(matches!(
        FileNonces::open(&path),
        Err(ApprovalRefusal::Storage(_))
    ));
}

#[test]
fn a_nonce_that_is_not_hex_is_malformed_on_spend() {
    let dir = Scratch::new("badnonce");
    let path = dir.path("nonces.jsonl");
    let mut store = FileNonces::open(&path).expect("opens");
    assert!(matches!(
        store.spend("not\"hex", NOW),
        Err(ApprovalRefusal::Malformed(_))
    ));
    assert_eq!(
        fs::read(&path).expect("readable"),
        Vec::<u8>::new(),
        "nothing is written for a refused nonce"
    );
}

#[test]
fn an_approval_spent_before_a_restart_is_refused_after_it() {
    let dir = Scratch::new("restart");
    let path = dir.path("nonces.jsonl");
    let key = SkKey::from_seed(7);
    let statement = statement_with(serde_json::json!({}));
    let approval = approval_of(
        &statement,
        &key.sign(arca_approval::ARCA_NAMESPACE, statement.as_bytes()),
    );
    let signers = SshSigVerifier::new(vec![key.signer()]);
    {
        let mut verifier = ApprovalVerifier::new(
            HOST.to_string(),
            signers,
            FileNonces::open(&path).expect("opens"),
        );
        verifier
            .check(ArcaAction::Delete, &target(), &args(), &approval, NOW)
            .expect("the approval is accepted once");
    }
    let key_again = SkKey::from_seed(7);
    let mut restarted = ApprovalVerifier::new(
        HOST.to_string(),
        SshSigVerifier::new(vec![key_again.signer()]),
        FileNonces::open(&path).expect("reopens"),
    );
    assert_eq!(
        restarted.check(ArcaAction::Delete, &target(), &args(), &approval, NOW),
        Err(ApprovalRefusal::Replayed)
    );
}

#[test]
fn a_refused_approval_writes_nothing_to_the_nonce_file() {
    let dir = Scratch::new("refused-writes-nothing");
    let path = dir.path("nonces.jsonl");
    let key = SkKey::from_seed(7);
    let statement = statement_with(serde_json::json!({ "host": "node2" }));
    let approval = approval_of(
        &statement,
        &key.sign(arca_approval::ARCA_NAMESPACE, statement.as_bytes()),
    );
    let mut verifier = ApprovalVerifier::new(
        HOST.to_string(),
        SshSigVerifier::new(vec![key.signer()]),
        FileNonces::open(&path).expect("opens"),
    );
    assert_eq!(
        verifier.check(ArcaAction::Delete, &target(), &args(), &approval, NOW),
        Err(ApprovalRefusal::WrongHost)
    );
    assert_eq!(fs::read(&path).expect("readable"), Vec::<u8>::new());
}
