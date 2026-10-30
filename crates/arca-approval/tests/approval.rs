//! Owner approvals through the public API: the statement's canonical form, the order of the checks, and the spend-once rule.
//! Each refusal was tested before the code that refuses it existed.

use std::sync::{Arc, Mutex};

use arca_approval::{
    ARCA_NAMESPACE, ApprovalRefusal, ApprovalVerifier, ArcaAction, MemNonces, NonceStore,
    OwnerApproval, RefuseAll, SignatureVerifier, Spent, parse_statement, split_approval,
};
use arca_kernel::canon::{canonical_json, params_digest};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use serde_json::{Value, json};

/// Droidd's approval vectors, byte for byte as the kernel crate carries them.
const FIXTURE: &str = include_str!("../../arca-kernel/tests/fixtures/droidd-vectors.json");
const HOST: &str = "node1";
/// A plan's digest, from the fixture's statement.
const PLAN: &str = "e39aa63ab2e4396ebd91016fbdad78d25c7ce79fac77aa248a494d93a52ffb10";
const NONCE: &str = "0123456789abcdef0123456789abcdef";
/// 2026-10-30T12:00:00.000Z in epoch milliseconds (`date -u -d '2026-10-30T12:00:00Z' +%s`, times 1000).
const NOW: u64 = 1_793_361_600_000;
const KEY: &str = "SHA256:test-key";
/// An armoured SSH signature, shaped as `ssh-keygen -Y sign` writes one. The body is test bytes, not a real signature.
const SIG: &[u8] =
    b"-----BEGIN SSH SIGNATURE-----\nU1NIU0lHAAAAAQAAAAAAAAAAAAAAAA==\n-----END SSH SIGNATURE-----\n";
/// An armoured signature the test verifier does not accept.
const FORGED: &[u8] = b"-----BEGIN SSH SIGNATURE-----\nZm9yZ2Vk\n-----END SSH SIGNATURE-----\n";

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("the vectors fixture is JSON")
}

fn target() -> String {
    format!("plan-{PLAN}")
}

fn args() -> Value {
    json!({ "plan": PLAN })
}

/// The time `offset_ms` after `NOW`, in `toISOString` form. Offsets stay inside the same day.
fn at(offset_ms: i64) -> String {
    let total = 12 * 3_600_000 + offset_ms;
    format!(
        "2026-10-30T{:02}:{:02}:{:02}.{:03}Z",
        total / 3_600_000,
        total % 3_600_000 / 60_000,
        total % 60_000 / 1000,
        total % 1000
    )
}

/// A valid statement's fields for the default call, with `overrides` applied on top.
fn fields(overrides: Value) -> Value {
    let mut base = json!({
        "host": HOST,
        "action": "delete",
        "target": target(),
        "paramsDigest": params_digest(&args()).expect("the args are an object"),
        "issuedAt": at(0),
        "expiresAt": at(10 * 60_000),
        "nonce": NONCE,
    });
    for (key, value) in overrides.as_object().expect("overrides are an object") {
        base[key.as_str()] = value.clone();
    }
    base
}

fn text_of(fields: &Value) -> String {
    canonical_json(fields).expect("the test fields canonicalize")
}

/// An approval string: the statement's base64, a dot, and the signature's base64.
fn approval_signed(statement: &str, signature: &[u8]) -> String {
    format!(
        "{}.{}",
        STANDARD.encode(statement),
        STANDARD.encode(signature)
    )
}

fn approval(statement: &str) -> String {
    approval_signed(statement, SIG)
}

/// Accepts the signature only in the Arca namespace and only over the test signature bytes.
struct Accept;

impl SignatureVerifier for Accept {
    fn verify(
        &self,
        namespace: &str,
        _statement: &[u8],
        signature: &[u8],
    ) -> Result<String, ApprovalRefusal> {
        if namespace == ARCA_NAMESPACE && signature == SIG {
            Ok(KEY.to_string())
        } else {
            Err(ApprovalRefusal::BadSignature)
        }
    }
}

/// One signature check as the verifier saw it: the namespace, the statement's bytes and the signature's bytes.
type Check = (String, Vec<u8>, Vec<u8>);

/// Records what the verifier is given, and accepts everything.
struct Recorder {
    seen: Arc<Mutex<Vec<Check>>>,
}

impl SignatureVerifier for Recorder {
    fn verify(
        &self,
        namespace: &str,
        statement: &[u8],
        signature: &[u8],
    ) -> Result<String, ApprovalRefusal> {
        self.seen.lock().expect("the record is not poisoned").push((
            namespace.to_string(),
            statement.to_vec(),
            signature.to_vec(),
        ));
        Ok(KEY.to_string())
    }
}

/// A nonce store that fails the test if anything reads or spends a nonce.
struct PanicNonces;

impl NonceStore for PanicNonces {
    fn spent(&self, _nonce: &str) -> bool {
        panic!("a nonce was read before the signature was checked")
    }

    fn spend(&mut self, _nonce: &str, _at_ms: u64) -> Result<(), ApprovalRefusal> {
        panic!("a nonce was spent without a checked signature")
    }
}

fn verifier() -> ApprovalVerifier<Accept, MemNonces> {
    ApprovalVerifier::new(HOST.to_string(), Accept, MemNonces::default())
}

/// Checks `approval_text` for `action` on `target` with `args`, at `NOW`.
fn check_call<V: SignatureVerifier, N: NonceStore>(
    v: &mut ApprovalVerifier<V, N>,
    action: ArcaAction,
    target: &str,
    args: &Value,
    approval_text: &str,
) -> Result<Spent, ApprovalRefusal> {
    v.check(action, target, args, approval_text, NOW)
}

/// The refusal of the default call for a statement with these fields.
fn refusal_of(fields: &Value) -> ApprovalRefusal {
    check_call(
        &mut verifier(),
        ArcaAction::Delete,
        &target(),
        &args(),
        &approval(&text_of(fields)),
    )
    .expect_err("the statement is refused")
}

fn statement_refusal(text: &str) -> ApprovalRefusal {
    parse_statement(text.as_bytes()).expect_err("the statement is refused")
}

fn assert_malformed(refusal: &ApprovalRefusal) {
    assert!(
        matches!(refusal, ApprovalRefusal::Malformed(_)),
        "expected Malformed, got {refusal:?}"
    );
}

fn digest_of_default_args() -> String {
    params_digest(&args()).expect("the args are an object")
}

// ---- the statement: canonical form and fields ----------------------------------------------------------------------------

#[test]
fn the_droidd_vector_statement_parses_to_its_fields() {
    let f = fixture();
    let text = f["statement"]["text"]
        .as_str()
        .expect("the statement text is a string");
    let want = &f["statement"]["fields"];
    let s = |key: &str| want[key].as_str().expect("a string field").to_string();
    assert_eq!(s("action"), "delete", "the vector is a delete");
    let expected = OwnerApproval {
        host: s("host"),
        action: ArcaAction::Delete,
        target: s("target"),
        params_digest: s("paramsDigest"),
        issued_at: s("issuedAt"),
        expires_at: s("expiresAt"),
        nonce: s("nonce"),
    };
    assert_eq!(parse_statement(text.as_bytes()), Ok(expected));
}

#[test]
fn a_nonce_of_32_or_64_lowercase_hex_digits_parses() {
    for nonce in ["0".repeat(32), "f".repeat(64)] {
        let text = text_of(&fields(json!({ "nonce": nonce })));
        assert!(
            parse_statement(text.as_bytes()).is_ok(),
            "nonce of {} digits",
            nonce.len()
        );
    }
}

#[test]
fn a_statement_that_is_not_utf8_is_malformed() {
    let bytes = [b'{', 0xff, b'}'];
    assert_malformed(&parse_statement(&bytes).expect_err("not UTF-8"));
}

#[test]
fn a_statement_that_is_not_json_is_malformed() {
    assert_malformed(&statement_refusal("{"));
}

#[test]
fn a_statement_that_is_not_a_json_object_is_malformed() {
    assert_malformed(&statement_refusal("[1]"));
    assert_malformed(&statement_refusal("\"delete\""));
}

#[test]
fn whitespace_makes_a_statement_non_canonical() {
    let text = text_of(&fields(json!({})));
    let spaced = text.replacen(':', ": ", 1);
    assert_eq!(statement_refusal(&spaced), ApprovalRefusal::NotCanonical);
}

#[test]
fn a_duplicate_key_makes_a_statement_non_canonical() {
    let text = text_of(&fields(json!({})));
    let duplicated = format!("{{\"action\":\"delete\",{}", &text[1..]);
    assert_eq!(
        statement_refusal(&duplicated),
        ApprovalRefusal::NotCanonical
    );
}

#[test]
fn an_escaped_slash_makes_a_statement_non_canonical() {
    let text = text_of(&fields(json!({})));
    let escaped = text.replacen("plan-", "plan\\/", 1);
    assert_eq!(statement_refusal(&escaped), ApprovalRefusal::NotCanonical);
}

#[test]
fn unsorted_keys_make_a_statement_non_canonical() {
    let f = fields(json!({}));
    let unsorted = format!(
        r#"{{"host":"{HOST}","action":"delete","target":"{}","paramsDigest":"{}","issuedAt":"{}","expiresAt":"{}","nonce":"{NONCE}"}}"#,
        target(),
        f["paramsDigest"].as_str().expect("a digest"),
        at(0),
        at(600_000)
    );
    assert_eq!(statement_refusal(&unsorted), ApprovalRefusal::NotCanonical);
}

#[test]
fn a_droidd_key_is_refused() {
    let refusal = refusal_of(&fields(json!({ "droidd": "droid" })));
    assert!(
        matches!(&refusal, ApprovalRefusal::Malformed(why) if why.contains("droidd")),
        "a droidd-bound approval is refused by name: {refusal:?}"
    );
}

#[test]
fn an_unknown_key_is_refused() {
    let mut f = fields(json!({}));
    f["extra"] = json!(1);
    assert_malformed(&refusal_of(&f));
}

#[test]
fn a_missing_key_is_refused() {
    let mut f = fields(json!({}));
    f.as_object_mut().expect("an object").remove("nonce");
    assert_malformed(&refusal_of(&f));
}

#[test]
fn the_action_install_is_refused() {
    assert_malformed(&refusal_of(&fields(json!({ "action": "install" }))));
}

#[test]
fn each_arca_action_parses_and_nothing_else_does() {
    for (name, want) in [
        ("delete", ArcaAction::Delete),
        ("publish", ArcaAction::Publish),
        ("spend", ArcaAction::Spend),
    ] {
        let parsed = parse_statement(text_of(&fields(json!({ "action": name }))).as_bytes());
        assert_eq!(
            parsed.expect("an Arca action parses").action,
            want,
            "{name}"
        );
    }
    assert_malformed(&refusal_of(&fields(json!({ "action": "Delete" }))));
}

#[test]
fn a_host_outside_the_name_pattern_is_refused() {
    assert_malformed(&refusal_of(&fields(json!({ "host": "-node" }))));
    assert_malformed(&refusal_of(&fields(json!({ "host": "no de" }))));
    assert_malformed(&refusal_of(&fields(json!({ "host": "h".repeat(254) }))));
}

#[test]
fn a_target_outside_the_name_pattern_is_refused() {
    let long = "t".repeat(129);
    for bad in ["plan 1", ":plan", "", long.as_str()] {
        assert_malformed(&refusal_of(&fields(json!({ "target": bad }))));
    }
}

#[test]
fn params_digest_must_be_64_lowercase_hex() {
    let upper = digest_of_default_args().to_uppercase();
    assert_malformed(&refusal_of(&fields(json!({ "paramsDigest": upper }))));
    assert_malformed(&refusal_of(&fields(
        json!({ "paramsDigest": "a".repeat(63) }),
    )));
}

#[test]
fn nonce_must_be_32_to_64_lowercase_hex() {
    assert_malformed(&refusal_of(&fields(json!({ "nonce": "0".repeat(31) }))));
    assert_malformed(&refusal_of(&fields(json!({ "nonce": "0".repeat(65) }))));
    assert_malformed(&refusal_of(&fields(json!({ "nonce": "A".repeat(32) }))));
}

#[test]
fn times_must_be_in_to_iso_string_form_and_real_dates() {
    for bad in [
        "2026-10-30T12:00:00Z",
        "2026-10-30T12:00:00.000+00:00",
        "2026-02-29T00:00:00.000Z",
        "2026-10-30T24:00:00.000Z",
        "2026-10-30T12:60:00.000Z",
        "2026-13-01T00:00:00.000Z",
    ] {
        assert_malformed(&refusal_of(&fields(json!({ "issuedAt": bad }))));
    }
    let leap = text_of(&fields(
        json!({ "issuedAt": "2028-02-29T00:00:00.000Z", "expiresAt": "2028-02-29T00:05:00.000Z" }),
    ));
    assert!(
        parse_statement(leap.as_bytes()).is_ok(),
        "2028 is a leap year"
    );
}

// ---- the approval string -------------------------------------------------------------------------------------------

#[test]
fn split_takes_the_two_base64_parts() {
    let text = text_of(&fields(json!({})));
    let (statement, signature) = split_approval(&approval(&text)).expect("two parts");
    assert_eq!(statement, text.as_bytes());
    assert_eq!(signature, SIG);
}

#[test]
fn split_refuses_a_missing_or_extra_dot() {
    assert_malformed(&split_approval("YWJj").expect_err("no dot"));
    assert_malformed(&split_approval("YWJj.YWJj.YWJj").expect_err("two dots"));
}

#[test]
fn split_refuses_non_base64_and_empty_parts() {
    assert_malformed(&split_approval("a!b.Y2ln").expect_err("a character outside base64"));
    assert_malformed(&split_approval(".Y2ln").expect_err("an empty statement"));
    assert_malformed(&split_approval("YQ==.").expect_err("an empty signature"));
    assert_malformed(&split_approval("YQ===.Y2ln").expect_err("three padding characters"));
}

#[test]
fn split_refuses_an_oversize_statement_or_approval() {
    let big_statement = approval_signed(&"a".repeat(4097), SIG);
    assert_malformed(&split_approval(&big_statement).expect_err("a statement over 4096 bytes"));
    let long = format!("{}.QQ==", "A".repeat(16_385));
    assert_malformed(&split_approval(&long).expect_err("an approval over 16384 characters"));
}

#[test]
fn split_trims_the_outer_whitespace_as_droidd_does() {
    let text = text_of(&fields(json!({})));
    let padded = format!("  {}\n", approval(&text));
    assert!(split_approval(&padded).is_ok());
}

#[test]
fn split_refuses_a_signature_that_is_not_armoured_ssh() {
    let text = text_of(&fields(json!({})));
    for signature in [
        "test signature bytes",
        "{\"passkey\":true}",
        "-----BEGIN SSH SIGNATURE-----\nAAAA\n",
        "-----BEGIN PGP SIGNATURE-----\nAAAA\n-----END SSH SIGNATURE-----\n",
        "-----BEGIN SSH SIGNATURE-----\nAA*A\n-----END SSH SIGNATURE-----\n",
        "-----BEGIN SSH SIGNATURE-----\n-----END SSH SIGNATURE-----\n",
        "-----BEGIN SSH SIGNATURE-----\nAAAA\n-----END SSH SIGNATURE-----\ntrailing text\n",
        "-----BEGIN SSH SIGNATURE-----\r\r\nAAAA\n-----END SSH SIGNATURE-----\n",
        " -----BEGIN SSH SIGNATURE-----\nAAAA\n-----END SSH SIGNATURE-----\n",
    ] {
        let refusal = split_approval(&approval_signed(&text, signature.as_bytes()))
            .expect_err("a signature that is not armoured is refused");
        assert_malformed(&refusal);
    }
}

#[test]
fn a_signature_that_is_not_armoured_is_refused_before_the_verifier_runs() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut v = ApprovalVerifier::new(
        HOST.to_string(),
        Recorder {
            seen: Arc::clone(&seen),
        },
        MemNonces::default(),
    );
    let text = text_of(&fields(json!({})));
    let refusal = check_call(
        &mut v,
        ArcaAction::Delete,
        &target(),
        &args(),
        &approval_signed(&text, b"test signature bytes"),
    )
    .expect_err("a signature that is not armoured is refused");
    assert_malformed(&refusal);
    assert!(
        seen.lock().expect("not poisoned").is_empty(),
        "the verifier is not asked about an unarmoured signature"
    );
}

#[test]
fn an_armoured_signature_reads_with_either_line_ending() {
    let text = text_of(&fields(json!({})));
    for signature in [
        "-----BEGIN SSH SIGNATURE-----\nAAAA\n-----END SSH SIGNATURE-----\n",
        "-----BEGIN SSH SIGNATURE-----\r\nAAAA\r\nBBBB\r\n-----END SSH SIGNATURE-----\r\n",
        "-----BEGIN SSH SIGNATURE-----\nAAAA\n-----END SSH SIGNATURE-----",
    ] {
        let (_, bytes) = split_approval(&approval_signed(&text, signature.as_bytes()))
            .expect("an armoured signature is read");
        assert_eq!(bytes, signature.as_bytes());
    }
}

#[test]
fn an_unpadded_base64_part_is_read_as_droidd_reads_it() {
    let text = text_of(&fields(json!({})));
    assert_ne!(
        text.len() % 3,
        0,
        "the statement needs padding in its padded form"
    );
    let unpadded = format!(
        "{}.{}",
        STANDARD_NO_PAD.encode(&text),
        STANDARD_NO_PAD.encode(SIG)
    );
    let (statement, signature) = split_approval(&unpadded).expect("unpadded parts are read");
    assert_eq!(statement, text.as_bytes());
    assert_eq!(signature, SIG);
}

/// Sets the lowest bit of a padded base64 string's last data character. That bit is a trailing bit, which a canonical
/// encoder leaves zero.
fn with_trailing_bits(encoded: &str) -> String {
    let alphabet = base64::alphabet::STANDARD.as_str().as_bytes();
    let pad = encoded.len() - encoded.trim_end_matches('=').len();
    assert!(pad > 0, "only a padded encoding has trailing bits");
    let at = encoded.len() - pad - 1;
    let mut bytes = encoded.as_bytes().to_vec();
    let value = alphabet
        .iter()
        .position(|c| *c == bytes[at])
        .expect("a standard base64 character");
    bytes[at] = alphabet[value ^ 1];
    String::from_utf8(bytes).expect("base64 is ASCII")
}

#[test]
fn non_zero_trailing_bits_are_read_as_droidd_reads_them() {
    let text = text_of(&fields(json!({})));
    assert_ne!(
        text.len() % 3,
        0,
        "the statement needs padding for trailing bits"
    );
    let tampered = with_trailing_bits(&STANDARD.encode(&text));
    assert!(
        STANDARD.decode(&tampered).is_err(),
        "the strict decoder refuses trailing bits, so this encoding is not canonical"
    );
    let approval_text = format!("{tampered}.{}", STANDARD.encode(SIG));
    let spent = check_call(
        &mut verifier(),
        ArcaAction::Delete,
        &target(),
        &args(),
        &approval_text,
    )
    .expect("droidd reads trailing bits, so the approval is read");
    assert_eq!(spent.nonce, NONCE);
}

#[test]
fn a_non_string_field_is_refused_as_malformed() {
    for key in [
        "host",
        "action",
        "target",
        "paramsDigest",
        "issuedAt",
        "expiresAt",
        "nonce",
    ] {
        let mut f = fields(json!({}));
        f[key] = json!(1);
        let refusal = refusal_of(&f);
        assert!(
            matches!(&refusal, ApprovalRefusal::Malformed(why) if why.contains(key)),
            "a number in {key} is refused by name: {refusal:?}"
        );
    }
}

// ---- the checks, in droidd's order ---------------------------------------------------------------------------------

#[test]
fn refuse_all_refuses_with_no_verifier_before_any_nonce_is_read() {
    let mut v = ApprovalVerifier::new(HOST.to_string(), RefuseAll, PanicNonces);
    let text = text_of(&fields(json!({})));
    let refusal = check_call(
        &mut v,
        ArcaAction::Delete,
        &target(),
        &args(),
        &approval(&text),
    )
    .expect_err("RefuseAll refuses");
    assert_eq!(refusal, ApprovalRefusal::NoVerifier);
}

#[test]
fn the_verifier_receives_the_arca_namespace_and_the_raw_bytes() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut v = ApprovalVerifier::new(
        HOST.to_string(),
        Recorder {
            seen: Arc::clone(&seen),
        },
        MemNonces::default(),
    );
    let text = text_of(&fields(json!({})));
    check_call(
        &mut v,
        ArcaAction::Delete,
        &target(),
        &args(),
        &approval(&text),
    )
    .expect("accepted");
    let record = seen.lock().expect("not poisoned");
    assert_eq!(record.len(), 1, "one signature check");
    assert_eq!(record[0].0, ARCA_NAMESPACE);
    assert_eq!(
        record[0].1,
        text.as_bytes(),
        "the statement's own bytes are checked"
    );
    assert_eq!(record[0].2, SIG);
}

#[test]
fn a_bad_signature_is_refused_before_any_bound_field_or_nonce() {
    let mut nonces = MemNonces::default();
    nonces
        .spend(NONCE, 1)
        .expect("the test seeds one spent nonce");
    let mut v = ApprovalVerifier::new(HOST.to_string(), Accept, nonces);
    let text = text_of(&fields(json!({ "host": "other-host" })));
    let refusal = check_call(
        &mut v,
        ArcaAction::Delete,
        &target(),
        &args(),
        &approval_signed(&text, FORGED),
    )
    .expect_err("a forged signature is refused");
    assert_eq!(refusal, ApprovalRefusal::BadSignature);
}

#[test]
fn a_wrong_host_is_refused() {
    assert_eq!(
        refusal_of(&fields(json!({ "host": "node2" }))),
        ApprovalRefusal::WrongHost
    );
}

#[test]
fn a_wrong_action_is_refused() {
    let text = text_of(&fields(json!({})));
    let refusal = check_call(
        &mut verifier(),
        ArcaAction::Spend,
        &target(),
        &args(),
        &approval(&text),
    )
    .expect_err("a delete does not admit a spend");
    assert_eq!(refusal, ApprovalRefusal::WrongAction);
}

#[test]
fn a_wrong_target_is_refused_and_so_is_a_call_with_an_unnameable_target() {
    let other = format!("plan-{}", "1".repeat(64));
    assert_eq!(
        refusal_of(&fields(json!({ "target": other }))),
        ApprovalRefusal::WrongTarget
    );
    let text = text_of(&fields(json!({})));
    let refusal = check_call(
        &mut verifier(),
        ArcaAction::Delete,
        "plan 1",
        &args(),
        &approval(&text),
    )
    .expect_err("no approval names a target with a space");
    assert_eq!(refusal, ApprovalRefusal::WrongTarget);
}

#[test]
fn other_arguments_are_refused_as_wrong_parameters() {
    let text = text_of(&fields(json!({})));
    let other_args = json!({ "plan": PLAN, "extra": true });
    let refusal = check_call(
        &mut verifier(),
        ArcaAction::Delete,
        &target(),
        &other_args,
        &approval(&text),
    )
    .expect_err("the digest names other arguments");
    assert_eq!(refusal, ApprovalRefusal::WrongParams);
}

#[test]
fn the_approval_key_of_the_arguments_is_left_out_of_the_digest() {
    let text = text_of(&fields(json!({})));
    let with_approval = json!({ "plan": PLAN, "approval": "anything at all" });
    check_call(
        &mut verifier(),
        ArcaAction::Delete,
        &target(),
        &with_approval,
        &approval(&text),
    )
    .expect("the digest of the other arguments matches");
}

#[test]
fn arguments_that_are_not_an_object_are_malformed() {
    let text = text_of(&fields(json!({})));
    let refusal = check_call(
        &mut verifier(),
        ArcaAction::Delete,
        &target(),
        &json!([1]),
        &approval(&text),
    )
    .expect_err("the args are not an object");
    assert_malformed(&refusal);
}

#[test]
fn an_approval_issued_more_than_five_minutes_ahead_is_refused() {
    let ahead = text_of(&fields(
        json!({ "issuedAt": at(300_001), "expiresAt": at(900_001) }),
    ));
    let refusal = check_call(
        &mut verifier(),
        ArcaAction::Delete,
        &target(),
        &args(),
        &approval(&ahead),
    )
    .expect_err("issued too far ahead");
    assert_eq!(refusal, ApprovalRefusal::IssuedAhead);
    let exactly_five = text_of(&fields(
        json!({ "issuedAt": at(300_000), "expiresAt": at(900_000) }),
    ));
    check_call(
        &mut verifier(),
        ArcaAction::Delete,
        &target(),
        &args(),
        &approval(&exactly_five),
    )
    .expect("five minutes ahead is allowed");
}

#[test]
fn an_expired_approval_is_refused() {
    let expired = text_of(&fields(
        json!({ "issuedAt": at(-600_000), "expiresAt": at(0) }),
    ));
    let refusal = check_call(
        &mut verifier(),
        ArcaAction::Delete,
        &target(),
        &args(),
        &approval(&expired),
    )
    .expect_err("expires at this instant");
    assert_eq!(refusal, ApprovalRefusal::Expired);
    let one_ms_left = text_of(&fields(
        json!({ "issuedAt": at(-600_000), "expiresAt": at(1) }),
    ));
    check_call(
        &mut verifier(),
        ArcaAction::Delete,
        &target(),
        &args(),
        &approval(&one_ms_left),
    )
    .expect("one millisecond left is still valid");
}

#[test]
fn a_window_longer_than_fifteen_minutes_from_its_issue_is_refused() {
    let long = text_of(&fields(
        json!({ "issuedAt": at(0), "expiresAt": at(900_001) }),
    ));
    let refusal = check_call(
        &mut verifier(),
        ArcaAction::Delete,
        &target(),
        &args(),
        &approval(&long),
    )
    .expect_err("a window over 15 minutes");
    assert_eq!(refusal, ApprovalRefusal::TtlTooLong);
}

#[test]
fn a_window_that_ends_more_than_fifteen_minutes_from_now_is_refused() {
    let late = text_of(&fields(
        json!({ "issuedAt": at(300_000), "expiresAt": at(1_200_000) }),
    ));
    let refusal = check_call(
        &mut verifier(),
        ArcaAction::Delete,
        &target(),
        &args(),
        &approval(&late),
    )
    .expect_err("ends 20 minutes from now");
    assert_eq!(refusal, ApprovalRefusal::TtlTooLong);
}

#[test]
fn a_window_that_does_not_run_forward_is_refused() {
    let flat = text_of(&fields(json!({ "issuedAt": at(0), "expiresAt": at(0) })));
    let refusal = check_call(
        &mut verifier(),
        ArcaAction::Delete,
        &target(),
        &args(),
        &approval(&flat),
    )
    .expect_err("expires as it is issued");
    assert_eq!(refusal, ApprovalRefusal::TtlTooLong);
}

#[test]
fn the_binding_is_checked_before_the_time_window() {
    let expired_elsewhere = text_of(&fields(
        json!({ "host": "node2", "issuedAt": at(-600_000), "expiresAt": at(0) }),
    ));
    let refusal = check_call(
        &mut verifier(),
        ArcaAction::Delete,
        &target(),
        &args(),
        &approval(&expired_elsewhere),
    )
    .expect_err("another host's expired approval");
    assert_eq!(refusal, ApprovalRefusal::WrongHost);
}

// ---- spend once --------------------------------------------------------------------------------------------------

#[test]
fn a_valid_statement_is_spent_once() {
    let mut v = verifier();
    let text = text_of(&fields(json!({})));
    let spent = check_call(
        &mut v,
        ArcaAction::Delete,
        &target(),
        &args(),
        &approval(&text),
    )
    .expect("accepted");
    assert_eq!(
        spent,
        Spent {
            nonce: NONCE.to_string(),
            key: KEY.to_string()
        }
    );
    let replay = check_call(
        &mut v,
        ArcaAction::Delete,
        &target(),
        &args(),
        &approval(&text),
    )
    .expect_err("replayed");
    assert_eq!(replay, ApprovalRefusal::Replayed);
}

#[test]
fn a_nonce_reused_by_another_statement_is_refused() {
    let mut v = verifier();
    let first = text_of(&fields(json!({})));
    check_call(
        &mut v,
        ArcaAction::Delete,
        &target(),
        &args(),
        &approval(&first),
    )
    .expect("the first is accepted");
    let other_target = format!("plan-{}", "2".repeat(64));
    let other = text_of(&fields(
        json!({ "target": other_target, "paramsDigest": params_digest(&args()).expect("object") }),
    ));
    let refusal = check_call(
        &mut v,
        ArcaAction::Delete,
        &other_target,
        &args(),
        &approval(&other),
    )
    .expect_err("the nonce is spent");
    assert_eq!(refusal, ApprovalRefusal::Replayed);
}

#[test]
fn a_nonce_already_spent_in_the_store_is_refused() {
    let mut nonces = MemNonces::default();
    nonces.spend(NONCE, 1).expect("spent once");
    let mut v = ApprovalVerifier::new(HOST.to_string(), Accept, nonces);
    let text = text_of(&fields(json!({})));
    let refusal = check_call(
        &mut v,
        ArcaAction::Delete,
        &target(),
        &args(),
        &approval(&text),
    )
    .expect_err("spent");
    assert_eq!(refusal, ApprovalRefusal::Replayed);
}

#[test]
fn mem_nonces_refuse_a_second_spend_of_one_nonce() {
    let mut nonces = MemNonces::default();
    assert!(!nonces.spent(NONCE));
    nonces.spend(NONCE, NOW).expect("the first spend");
    assert!(nonces.spent(NONCE));
    assert_eq!(nonces.spend(NONCE, NOW + 1), Err(ApprovalRefusal::Replayed));
}

#[test]
fn a_refused_approval_spends_nothing() {
    let mut v = verifier();
    let good = text_of(&fields(json!({})));
    let wrong_host = text_of(&fields(json!({ "host": "node2" })));
    let ahead = text_of(&fields(
        json!({ "issuedAt": at(300_001), "expiresAt": at(900_001) }),
    ));
    assert_eq!(
        check_call(
            &mut v,
            ArcaAction::Delete,
            &target(),
            &args(),
            &approval(&wrong_host)
        ),
        Err(ApprovalRefusal::WrongHost)
    );
    assert_eq!(
        check_call(
            &mut v,
            ArcaAction::Delete,
            &target(),
            &args(),
            &approval(&ahead)
        ),
        Err(ApprovalRefusal::IssuedAhead)
    );
    assert_eq!(
        check_call(
            &mut v,
            ArcaAction::Spend,
            &target(),
            &args(),
            &approval(&good)
        ),
        Err(ApprovalRefusal::WrongAction)
    );
    let spent = check_call(
        &mut v,
        ArcaAction::Delete,
        &target(),
        &args(),
        &approval(&good),
    );
    assert_eq!(
        spent.map(|s| s.nonce),
        Ok(NONCE.to_string()),
        "the nonce was never spent by a refusal"
    );
}
