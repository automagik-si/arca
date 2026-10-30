//! The approval signature gate: a software `sk-ssh-ed25519` SSHSIG is checked by `ssh-key` and by `ssh-keygen -Y verify`,
//! and every refusal path of [`SshSigVerifier`] is a typed [`ApprovalRefusal`]. Each refusal was tested before the code
//! that refuses it existed.

mod common;

use std::fs;
use std::process::Command;

use arca_approval::{
    ARCA_NAMESPACE, ApprovalRefusal, ApprovalVerifier, ArcaAction, MemNonces, SignatureVerifier,
    SignerLine, SshSigVerifier,
};
use common::{
    ED_TYPE, EdKey, HOST, NOW, Scratch, SkKey, approval_of, args, armour, fingerprint,
    ssh_keygen_verify, statement_with, target, unarmour,
};

const OWNER_SEED: u8 = 7;
const OTHER_SEED: u8 = 9;

/// The owner's key as the signers file would hold it for Arca's namespace.
fn owner() -> SkKey {
    SkKey::from_seed(OWNER_SEED)
}

fn verifier_for(key: &SkKey) -> SshSigVerifier {
    SshSigVerifier::new(vec![key.signer()])
}

fn refusal(
    verifier: &SshSigVerifier,
    namespace: &str,
    statement: &str,
    signature: &str,
) -> ApprovalRefusal {
    verifier
        .verify(namespace, statement.as_bytes(), signature.as_bytes())
        .expect_err("the signature is refused")
}

// ---- the gate ---------------------------------------------------------------------------------------------------------

#[test]
fn a_software_sk_signature_in_the_arca_namespace_verifies_with_ssh_key() {
    let key = owner();
    let statement = statement_with(serde_json::json!({}));
    let signature = key.sign(ARCA_NAMESPACE, statement.as_bytes());
    let fp = verifier_for(&key)
        .verify(ARCA_NAMESPACE, statement.as_bytes(), signature.as_bytes())
        .expect("the owner's signature verifies");
    assert_eq!(fp, fingerprint(&key.blob()));
    assert!(fp.starts_with("SHA256:"));
}

#[test]
fn ssh_keygen_verifies_the_same_signature_as_ssh_key() {
    let key = owner();
    let scratch = Scratch::new("gate-keygen");
    let statement = statement_with(serde_json::json!({}));
    let signature = key.sign(ARCA_NAMESPACE, statement.as_bytes());
    assert!(
        ssh_keygen_verify(
            &scratch,
            &key.line(ARCA_NAMESPACE),
            ARCA_NAMESPACE,
            &statement,
            &signature,
        ),
        "ssh-keygen -Y verify must accept the software sk signature"
    );
}

#[test]
fn a_real_ssh_keygen_signature_is_canonical_and_refused_only_as_not_hardware() {
    // Owner tooling signs with `ssh-keygen -Y sign`, which writes canonical SSHSIG. The canonical check must pass its
    // output, so the refusal here is the hardware-key one, reached after every encoding check. The key is a software
    // ed25519 key made in the test's own scratch directory.
    let scratch = Scratch::new("real-keygen");
    let key = scratch.path("plain");
    let made = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&key)
        .status()
        .expect("ssh-keygen is on PATH for these tests");
    assert!(made.success(), "ssh-keygen makes a software test key");
    let statement_file = scratch.path("statement.json");
    let statement = statement_with(serde_json::json!({}));
    fs::write(&statement_file, statement.as_bytes()).expect("the statement is written");
    let signed = Command::new("ssh-keygen")
        .args(["-Y", "sign", "-n", ARCA_NAMESPACE, "-f"])
        .arg(&key)
        .arg(&statement_file)
        .output()
        .expect("ssh-keygen is on PATH for these tests");
    assert!(signed.status.success(), "ssh-keygen signs the statement");
    let armoured = fs::read(scratch.path("statement.json.sig")).expect("the signature file exists");
    let verifier = SshSigVerifier::new(Vec::new());
    assert_eq!(
        verifier.verify(ARCA_NAMESPACE, statement.as_bytes(), &armoured),
        Err(ApprovalRefusal::NotHardwareKey)
    );
}

#[test]
fn ssh_keygen_does_not_check_the_touch_flag_so_the_verifier_must() {
    // Observed fact: OpenSSH's verifier accepts an sk signature whose user-presence flag is
    // clear, so the flag is checked here, as droidd checks it. This test pins that fact and fails if OpenSSH changes.
    let key = owner();
    let scratch = Scratch::new("gate-no-touch-keygen");
    let statement = statement_with(serde_json::json!({}));
    let signature = key.sign_flags(ARCA_NAMESPACE, statement.as_bytes(), 0x00, 0x00);
    assert!(ssh_keygen_verify(
        &scratch,
        &key.line(ARCA_NAMESPACE),
        ARCA_NAMESPACE,
        &statement,
        &signature,
    ));
}

// ---- refusals of the signature ----------------------------------------------------------------------------------------

#[test]
fn a_signature_without_user_presence_is_refused_as_no_user_presence() {
    let key = owner();
    let statement = statement_with(serde_json::json!({}));
    let signature = key.sign_flags(ARCA_NAMESPACE, statement.as_bytes(), 0x00, 0x00);
    assert_eq!(
        refusal(&verifier_for(&key), ARCA_NAMESPACE, &statement, &signature),
        ApprovalRefusal::NoUserPresence
    );
}

#[test]
fn a_user_presence_flag_the_key_did_not_sign_is_refused_as_bad_signature() {
    // The flags byte is inside the signed data, so a flag set after signing does not verify.
    let key = owner();
    let statement = statement_with(serde_json::json!({}));
    let signature = key.sign_flags(ARCA_NAMESPACE, statement.as_bytes(), 0x00, 0x01);
    assert_eq!(
        refusal(&verifier_for(&key), ARCA_NAMESPACE, &statement, &signature),
        ApprovalRefusal::BadSignature
    );
}

#[test]
fn a_tampered_ed25519_signature_is_refused_as_bad_signature() {
    let key = owner();
    let statement = statement_with(serde_json::json!({}));
    let signature = key.sign_tampered(ARCA_NAMESPACE, statement.as_bytes());
    assert_eq!(
        refusal(&verifier_for(&key), ARCA_NAMESPACE, &statement, &signature),
        ApprovalRefusal::BadSignature
    );
}

#[test]
fn a_signature_over_other_bytes_is_refused_as_bad_signature() {
    let key = owner();
    let statement = statement_with(serde_json::json!({}));
    let other = statement_with(serde_json::json!({ "host": "node2" }));
    let signature = key.sign(ARCA_NAMESPACE, other.as_bytes());
    assert_eq!(
        refusal(&verifier_for(&key), ARCA_NAMESPACE, &statement, &signature),
        ApprovalRefusal::BadSignature
    );
}

#[test]
fn every_single_bit_flip_of_a_valid_signature_is_refused() {
    // Any change to the signed blob, its key, namespace, hash, reserved field, flags or counter, is refused, and no
    // change panics. The flip is made on the decoded bytes, so each variant is a different blob.
    let key = owner();
    let statement = statement_with(serde_json::json!({}));
    let blob = unarmour(&key.sign(ARCA_NAMESPACE, statement.as_bytes()));
    let verifier = verifier_for(&key);
    for index in 0..blob.len() {
        let mut flipped = blob.clone();
        flipped[index] ^= 0x01;
        let refused = verifier.verify(
            ARCA_NAMESPACE,
            statement.as_bytes(),
            armour(&flipped).as_bytes(),
        );
        assert!(
            refused.is_err(),
            "the bit flip at byte {index} must be refused"
        );
    }
}

#[test]
fn an_sshsig_version_other_than_one_is_malformed() {
    // The version is not in the signed data, so a reader that takes any version up to one would accept a relabelled
    // signature. Only version 1 is read.
    let key = owner();
    let statement = statement_with(serde_json::json!({}));
    let mut blob = unarmour(&key.sign(ARCA_NAMESPACE, statement.as_bytes()));
    assert_eq!(blob[9], 1, "the fixture's version is 1");
    blob[9] = 0;
    assert!(matches!(
        refusal(
            &verifier_for(&key),
            ARCA_NAMESPACE,
            &statement,
            &armour(&blob),
        ),
        ApprovalRefusal::Malformed(_)
    ));
}

#[test]
fn a_signature_in_another_namespace_is_refused_as_wrong_namespace() {
    // The cross-spend case: an approval droidd signed in its own `arca-approve` namespace, by a key this line also lists
    // for Arca, must not verify here.
    let key = owner();
    let statement = statement_with(serde_json::json!({}));
    let signature = key.sign("arca-approve", statement.as_bytes());
    assert_eq!(
        refusal(&verifier_for(&key), "arca-approve", &statement, &signature),
        ApprovalRefusal::WrongNamespace
    );
    assert_eq!(
        refusal(&verifier_for(&key), ARCA_NAMESPACE, &statement, &signature),
        ApprovalRefusal::WrongNamespace
    );
}

#[test]
fn an_ordinary_ed25519_signature_is_refused_as_not_hardware() {
    let plain = EdKey::from_seed(OWNER_SEED);
    let statement = statement_with(serde_json::json!({}));
    let signature = plain.sign(ARCA_NAMESPACE, statement.as_bytes());
    let trusted = SshSigVerifier::new(vec![SignerLine {
        key_type: ED_TYPE.to_string(),
        public: plain.blob(),
        fingerprint: fingerprint(&plain.blob()),
    }]);
    assert_eq!(
        refusal(&trusted, ARCA_NAMESPACE, &statement, &signature),
        ApprovalRefusal::NotHardwareKey
    );
}

#[test]
fn a_signature_relabelled_to_another_algorithm_is_refused_as_not_hardware() {
    // A genuine owner signature whose algorithm name is changed, with its bytes framed to match, still verifies under
    // the key: neither the name nor the framing is signed. Only sk-ssh-ed25519 may pass.
    let key = owner();
    let statement = statement_with(serde_json::json!({}));
    let verifier = verifier_for(&key);
    for label in ["rsa-sha2-512", "x@example.com"] {
        let relabelled = key.sign_as(ARCA_NAMESPACE, statement.as_bytes(), label);
        assert_eq!(
            refusal(&verifier, ARCA_NAMESPACE, &statement, &relabelled),
            ApprovalRefusal::NotHardwareKey,
            "a signature labelled {label} is not an sk-ssh-ed25519 signature"
        );
    }
}

#[test]
fn a_hardware_key_the_signers_list_does_not_hold_is_refused_as_bad_signature() {
    let listed = owner();
    let stranger = SkKey::from_seed(OTHER_SEED);
    let statement = statement_with(serde_json::json!({}));
    let signature = stranger.sign(ARCA_NAMESPACE, statement.as_bytes());
    assert_eq!(
        refusal(
            &verifier_for(&listed),
            ARCA_NAMESPACE,
            &statement,
            &signature
        ),
        ApprovalRefusal::BadSignature
    );
}

#[test]
fn an_empty_signers_list_refuses_with_no_verifier() {
    let key = owner();
    let statement = statement_with(serde_json::json!({}));
    let signature = key.sign(ARCA_NAMESPACE, statement.as_bytes());
    let none = SshSigVerifier::new(Vec::new());
    assert_eq!(
        refusal(&none, ARCA_NAMESPACE, &statement, &signature),
        ApprovalRefusal::NoVerifier
    );
}

#[test]
fn bytes_that_are_not_an_sshsig_are_malformed() {
    let key = owner();
    let statement = statement_with(serde_json::json!({}));
    let bogus = "-----BEGIN SSH SIGNATURE-----\nZm9yZ2Vk\n-----END SSH SIGNATURE-----\n";
    assert!(matches!(
        refusal(&verifier_for(&key), ARCA_NAMESPACE, &statement, bogus),
        ApprovalRefusal::Malformed(_)
    ));
}

#[test]
fn a_dropped_key_line_that_does_not_parse_leaves_no_trusted_key() {
    // A line whose blob is not a hardware key is dropped by the constructor, so the verifier trusts nothing from it.
    let key = owner();
    let statement = statement_with(serde_json::json!({}));
    let signature = key.sign(ARCA_NAMESPACE, statement.as_bytes());
    let untrusted = SshSigVerifier::new(vec![SignerLine {
        key_type: "sk-ssh-ed25519@openssh.com".to_string(),
        public: vec![0, 0, 0, 1, b'x'],
        fingerprint: String::new(),
    }]);
    assert_eq!(
        refusal(&untrusted, ARCA_NAMESPACE, &statement, &signature),
        ApprovalRefusal::NoVerifier
    );
}

// ---- through the approval verifier ------------------------------------------------------------------------------------

fn check(
    verifier: &mut ApprovalVerifier<SshSigVerifier, MemNonces>,
    approval: &str,
) -> Result<arca_approval::Spent, ApprovalRefusal> {
    verifier.check(ArcaAction::Delete, &target(), &args(), approval, NOW)
}

#[test]
fn a_valid_approval_is_spent_once_through_the_verifier() {
    let key = owner();
    let statement = statement_with(serde_json::json!({}));
    let approval = approval_of(&statement, &key.sign(ARCA_NAMESPACE, statement.as_bytes()));
    let mut verifier =
        ApprovalVerifier::new(HOST.to_string(), verifier_for(&key), MemNonces::default());
    let spent = check(&mut verifier, &approval).expect("the owner's approval is accepted");
    assert_eq!(spent.key, fingerprint(&key.blob()));
    assert_eq!(
        check(&mut verifier, &approval),
        Err(ApprovalRefusal::Replayed)
    );
}

#[test]
fn an_approval_signed_for_droidd_is_refused_here_even_when_its_statement_fits() {
    // The statement is the one Arca would check (this host, this action, this target and these arguments). Only the
    // namespace differs, and that alone refuses it.
    let key = owner();
    let statement = statement_with(serde_json::json!({}));
    let approval = approval_of(&statement, &key.sign("arca-approve", statement.as_bytes()));
    let mut verifier =
        ApprovalVerifier::new(HOST.to_string(), verifier_for(&key), MemNonces::default());
    assert_eq!(
        check(&mut verifier, &approval),
        Err(ApprovalRefusal::WrongNamespace)
    );
}
