//! Passkey approvals: the passkeys file and its trust rule, the proof's strict form, every check of the assertion, the sign
//! count that only grows, and the spend through `ApprovalVerifier<OwnerKeys, FileNonces>`. Each refusal was tested before the
//! code that refuses it existed. Every key is a software P-256 key from a fixed seed, every file lives in the test's own
//! scratch directory, and no test prints a proof, a credential id, a key or an approval string.

mod common;
#[path = "common/webauthn.rs"]
mod webauthn;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use ApprovalRefusal::{
    BadSignature, Malformed, NoUserPresence, NoUserVerification, NoVerifier, NotCanonical,
    Replayed, SignCountRegressed, Storage, Untrusted, WrongNamespace, WrongOrigin, WrongParams,
    WrongRp,
};
use arca_approval::{
    ARCA_NAMESPACE, ApprovalRefusal, ApprovalVerifier, ArcaAction, FileNonces, FileSignCounts,
    OWNER_PASSKEYS, OwnerKeys, PASSKEY_CHALLENGE_PREFIX, PASSKEY_COUNTS_FILE, PasskeyLine,
    PasskeyVerifier, SignatureVerifier, SshSigVerifier, TEST_PASSKEYS_ENV, TrustRoot,
    challenge_for, load_passkeys, load_signers, parse_passkeys, parse_proof, passkeys_trust,
    signers_trust, split_approval, verify_assertion,
};
use arca_kernel::canon::canonical_json;
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use common::{NONCE, NOW, Scratch, SkKey, approval_of, args, owner_uid, statement_with};
use common::{target, write_mode};
use serde_json::{Value, json};
use webauthn::{AT, Assert, BE, BS, ED, ORIGIN, RP, SForm, SoftPasskey, UP, UV};
use webauthn::{challenge_with, high_s, is_high, passkey_approval};

const NONCE_B: &str = "1123456789abcdef0123456789abcdef";
const NONCE_C: &str = "2123456789abcdef0123456789abcdef";
const NONCE_D: &str = "3123456789abcdef0123456789abcdef";
/// droidd's passkey challenge prefix, which must never verify here.
const DROIDD_PREFIX: &str = "droid-owner-approval/v1\n";

type Verifier = ApprovalVerifier<OwnerKeys, FileNonces>;
type Bend = Box<dyn FnOnce(&mut Assert)>;

fn projects() -> Vec<String> {
    vec!["arcak-scratch-a".to_string()]
}

fn scratch_trust(dir: &Scratch) -> TrustRoot {
    TrustRoot::Scratch {
        uid: owner_uid(&dir.dir),
        up_to: dir.dir.clone(),
    }
}

/// The passkeys file text that lists each of `keys` for Arca's namespace.
fn lines_for(keys: &[&SoftPasskey]) -> String {
    keys.iter()
        .map(|k| format!("{}\n", k.line(ARCA_NAMESPACE)))
        .collect()
}

/// The SSH verifier for `keys`, through a signers file in `scratch` and the scratch injection.
fn ssh_for(scratch: &Scratch, keys: &[&SkKey]) -> SshSigVerifier {
    let file = scratch.path("owner-signers");
    let text: String = keys
        .iter()
        .map(|k| format!("{}\n", k.line(ARCA_NAMESPACE)))
        .collect();
    write_mode(&file, text.as_bytes(), 0o600);
    let (path, trust) = signers_trust(true, &projects(), Some(file)).expect("the injection");
    SshSigVerifier::new(load_signers(&path, &trust).expect("the signers file loads"))
}

/// The owner keys the kernel builds over `scratch`: `ssh` through the signers file, `passkeys` through the passkeys file,
/// both read back through the scratch injection, and the sign counts in the state directory.
fn owner_keys(scratch: &Scratch, ssh: &[&SkKey], passkeys: &str) -> OwnerKeys {
    let file = scratch.path("owner-passkeys");
    write_mode(&file, passkeys.as_bytes(), 0o600);
    let (path, trust) = passkeys_trust(true, &projects(), Some(file)).expect("the injection");
    let lines = load_passkeys(&path, &trust).expect("the passkeys file loads");
    let counts = FileSignCounts::open(&scratch.path(PASSKEY_COUNTS_FILE)).expect("counts open");
    OwnerKeys::new(ssh_for(scratch, ssh), PasskeyVerifier::new(lines, counts))
}

/// The verifier the kernel holds over `scratch`'s files. Opening it again after the last one is dropped is a restart.
fn verifier(scratch: &Scratch, ssh: &[&SkKey], passkeys: &str) -> Verifier {
    let nonces = FileNonces::open(&scratch.path("approval-nonces.jsonl")).expect("nonces open");
    ApprovalVerifier::new(
        common::HOST.to_string(),
        owner_keys(scratch, ssh, passkeys),
        nonces,
    )
}

/// A verifier that trusts the passkey `key` alone.
fn verifier_of(tag: &str, key: &SoftPasskey) -> (Scratch, Verifier) {
    let scratch = Scratch::new(tag);
    let v = verifier(&scratch, &[], &lines_for(&[key]));
    (scratch, v)
}

fn check(v: &mut Verifier, approval: &str) -> Result<String, ApprovalRefusal> {
    v.check(ArcaAction::Delete, &target(), &args(), approval, NOW)
        .map(|spent| spent.key)
}

/// A `plan_list` statement for the default plan, with `nonce`.
fn statement(nonce: &str) -> String {
    statement_with(json!({ "nonce": nonce }))
}

/// The approval `key` makes over `statement`, with the assertion bent by `bend`.
fn approve(key: &SoftPasskey, statement: &str, bend: impl FnOnce(&mut Assert)) -> String {
    let mut a = Assert::over(ARCA_NAMESPACE, statement.as_bytes());
    bend(&mut a);
    passkey_approval(statement, &key.assert(&a))
}

fn genuine(key: &SoftPasskey, statement: &str) -> String {
    approve(key, statement, |_| {})
}

/// `key`'s proof object over `statement`, with `field` set to `value`, as canonical JSON text.
fn proof_with(key: &SoftPasskey, statement: &str, field: &str, value: Value) -> String {
    let mut proof = key.proof_value(&Assert::over(ARCA_NAMESPACE, statement.as_bytes()));
    proof[field] = value;
    canonical_json(&proof).expect("the proof canonicalizes")
}

/// Checks that `approval` is refused with `want`, and that it spent nothing: the genuine approval over the same statement
/// passes afterwards.
fn refused_unspent(
    tag: &str,
    approval: impl FnOnce(&SoftPasskey, &str) -> String,
    want: ApprovalRefusal,
) {
    let key = SoftPasskey::from_seed(1);
    let (_scratch, mut v) = verifier_of(tag, &key);
    let s = statement(NONCE);
    assert_eq!(check(&mut v, &approval(&key, &s)), Err(want), "{tag}");
    check(&mut v, &genuine(&key, &s)).unwrap_or_else(|r| panic!("{tag}: then refused {r:?}"));
}

// ---- the spend through ApprovalVerifier<OwnerKeys, FileNonces> ----------------------------------------------------------

#[test]
fn a_passkey_assertion_over_a_plan_list_statement_verifies_and_names_the_passkey() {
    let key = SoftPasskey::from_seed(1);
    let (_scratch, mut v) = verifier_of("pk-verifies", &key);
    let spent = check(&mut v, &genuine(&key, &statement(NONCE))).expect("verifies");
    assert_eq!(spent, format!("passkey:{}", key.fingerprint()));
}

#[test]
fn a_replay_is_refused_by_its_count_or_its_nonce_also_after_a_restart() {
    // A counting passkey's replay fails its count; a synced passkey (count 0) passes its count and fails its nonce.
    for (tag, count, flags, want) in [
        ("pk-replay-count", 5, UP | UV, SignCountRegressed),
        ("pk-replay-synced", 0, UP | UV | BE | BS, Replayed),
    ] {
        let scratch = Scratch::new(tag);
        let key = SoftPasskey::from_seed(1);
        let text = lines_for(&[&key]);
        let approval = approve(&key, &statement(NONCE), |a| {
            (a.sign_count, a.flags) = (count, flags)
        });
        {
            let mut v = verifier(&scratch, &[], &text);
            check(&mut v, &approval).expect("the first use verifies");
            assert_eq!(check(&mut v, &approval), Err(want.clone()), "{tag}");
        }
        let mut v = verifier(&scratch, &[], &text);
        assert_eq!(
            check(&mut v, &approval),
            Err(want),
            "{tag} after the restart"
        );
    }
}

#[test]
fn a_high_s_signature_verifies_and_its_low_s_twin_is_no_new_approval() {
    let key = SoftPasskey::from_seed(1);
    let (_scratch, mut v) = verifier_of("pk-high-s", &key);
    let s = statement(NONCE);
    check(&mut v, &approve(&key, &s, |a| a.s_form = SForm::High)).expect("high-S verifies");
    let low = approve(&key, &s, |a| a.s_form = SForm::Low);
    assert_eq!(
        check(&mut v, &low),
        Err(Replayed),
        "a synced twin is a replay"
    );

    let b = statement(NONCE_B);
    let bend = |form| move |a: &mut Assert| (a.sign_count, a.s_form) = (9, form);
    check(&mut v, &approve(&key, &b, bend(SForm::Low))).expect("low-S verifies");
    let twin = approve(&key, &b, bend(SForm::High));
    assert_eq!(
        check(&mut v, &twin),
        Err(SignCountRegressed),
        "a counting twin is a replay"
    );

    use p256::ecdsa::signature::Signer as _;
    for message in [b"one".as_slice(), b"two", b"three", b"four"] {
        assert!(
            is_high(&high_s(&key.signing.sign(message))),
            "the helper makes high-S"
        );
    }
}

#[test]
fn the_ssh_path_still_verifies_beside_the_passkeys_and_both_are_counted() {
    let scratch = Scratch::new("pk-ssh-beside");
    let (owner, key) = (SkKey::from_seed(1), SoftPasskey::from_seed(1));
    let mut v = verifier(
        &scratch,
        &[&owner, &SkKey::from_seed(2)],
        &lines_for(&[&key]),
    );
    let s = statement(NONCE);
    let ssh = approval_of(&s, &owner.sign(ARCA_NAMESPACE, s.as_bytes()));
    assert!(
        check(&mut v, &ssh)
            .expect("the SSH approval verifies")
            .starts_with("SHA256:")
    );
    check(&mut v, &genuine(&key, &statement(NONCE_B))).expect("the passkey approval verifies");
    let keys = owner_keys(
        &Scratch::new("pk-counts-both"),
        &[&owner],
        &lines_for(&[&key]),
    );
    assert_eq!(keys.key_counts(), (1, 1));
}

#[test]
fn without_a_passkey_a_passkey_proof_is_refused_no_verifier_and_never_reaches_ssh() {
    let scratch = Scratch::new("pk-dispatch");
    let mut v = verifier(&scratch, &[&SkKey::from_seed(1)], "");
    let approval = genuine(&SoftPasskey::from_seed(1), &statement(NONCE));
    assert_eq!(check(&mut v, &approval), Err(NoVerifier));
    let missing = scratch.path("not-there");
    assert_eq!(
        load_passkeys(&missing, &scratch_trust(&scratch)),
        Ok(vec![])
    );
}

// ---- each refusal spends no nonce and advances no count -----------------------------------------------------------------

#[test]
fn every_bent_assertion_is_refused_with_its_variant_and_spends_nothing() {
    let m = |why: &str| Malformed(why.to_string());
    let origin = |o: String| -> Bend { Box::new(move |a| a.origin = Some(o)) };
    let other: Bend = Box::new(|a| {
        let s = statement(NONCE_B);
        a.challenge = Some(challenge_with(
            PASSKEY_CHALLENGE_PREFIX,
            ARCA_NAMESPACE,
            s.as_bytes(),
        ));
    });
    let droidd: Bend = Box::new(|a| {
        a.challenge = Some(challenge_with(DROIDD_PREFIX, "arca-approve", &a.statement));
    });
    let cases: Vec<(&str, Bend, ApprovalRefusal)> = vec![
        (
            "unknown-cred",
            Box::new(|a| a.credential_id = Some(vec![0x77; 20])),
            BadSignature,
        ),
        ("other-statement", other, BadSignature),
        (
            "droidd-prefix",
            Box::new(|a| a.prefix = DROIDD_PREFIX.to_string()),
            BadSignature,
        ),
        ("droidd-challenge", droidd, BadSignature),
        (
            "rp",
            Box::new(|a| a.rp_id = Some("box2.example.com".into())),
            WrongRp,
        ),
        (
            "port",
            origin("https://box1.example.com:9443".into()),
            WrongOrigin,
        ),
        (
            "no-port",
            origin("https://box1.example.com".into()),
            WrongOrigin,
        ),
        (
            "http",
            origin("http://box1.example.com:8443".into()),
            WrongOrigin,
        ),
        (
            "sub",
            origin("https://a.box1.example.com:8443".into()),
            WrongOrigin,
        ),
        ("slash", origin(format!("{ORIGIN}/")), WrongOrigin),
        (
            "cross-true",
            Box::new(|a| a.cross_origin = Some(json!(true))),
            WrongOrigin,
        ),
        (
            "cross-null",
            Box::new(|a| a.cross_origin = Some(Value::Null)),
            WrongOrigin,
        ),
        (
            "cross-text",
            Box::new(|a| a.cross_origin = Some(json!("false"))),
            WrongOrigin,
        ),
        (
            "top-origin",
            Box::new(|a| a.top_origin = Some(ORIGIN.to_string())),
            WrongOrigin,
        ),
        ("no-up", Box::new(|a| a.flags = UV), NoUserPresence),
        ("no-uv", Box::new(|a| a.flags = UP), NoUserVerification),
        (
            "create",
            Box::new(|a| a.ceremony = "webauthn.create".into()),
            m("the passkey answered another ceremony than webauthn.get"),
        ),
        (
            "at",
            Box::new(|a| a.flags = UP | UV | AT),
            m("the passkey sent attested credential data in an assertion (AT set)"),
        ),
        (
            "ed",
            Box::new(|a| a.flags = UP | UV | ED),
            m("the passkey sent extension data Arca does not read (ED set)"),
        ),
        (
            "trailing",
            Box::new(|a| a.trailing = vec![0xa0]),
            m("the passkey's authenticatorData has bytes after its 37"),
        ),
        (
            "bs-no-be",
            Box::new(|a| a.flags = UP | UV | BS),
            m("the passkey reports a backup state without backup eligibility (BS without BE)"),
        ),
    ];
    for (tag, bend, want) in cases {
        refused_unspent(
            &format!("pk-bent-{tag}"),
            |key, s| approve(key, s, bend),
            want,
        );
    }
}

#[test]
fn an_assertion_without_cross_origin_verifies() {
    let key = SoftPasskey::from_seed(1);
    let (_scratch, mut v) = verifier_of("pk-no-cross", &key);
    let approval = approve(&key, &statement(NONCE), |a| a.cross_origin = None);
    check(&mut v, &approval).expect("crossOrigin is optional");
}

#[test]
fn a_line_without_the_arca_namespace_or_a_forged_signature_is_refused_bad_signature() {
    let (key, other) = (SoftPasskey::from_seed(1), SoftPasskey::from_seed(2));
    let s = statement(NONCE);
    let text = format!(
        "{}\n{}\n",
        other.line(ARCA_NAMESPACE),
        key.line("arca-approve,x")
    );
    let mut v = verifier(&Scratch::new("pk-line-ns"), &[], &text);
    assert_eq!(check(&mut v, &genuine(&key, &s)), Err(BadSignature));
    check(&mut v, &genuine(&other, &s)).expect("the nonce is unspent");

    // Alone in its file, such a line is no passkey at all.
    let mut v = verifier(
        &Scratch::new("pk-line-ns-alone"),
        &[],
        &key.line("arca-approve"),
    );
    assert_eq!(check(&mut v, &genuine(&key, &s)), Err(NoVerifier));

    let mut forger = SoftPasskey::from_seed(2);
    forger.credential_id = key.credential_id.clone();
    refused_unspent("pk-forger", |_, s| genuine(&forger, s), BadSignature);
}

#[test]
fn a_tampered_or_loosely_encoded_signature_is_refused_bad_signature() {
    let key = SoftPasskey::from_seed(1);
    let value = key.proof_value(&Assert::over(ARCA_NAMESPACE, statement(NONCE).as_bytes()));
    let der = URL_SAFE_NO_PAD
        .decode(value["signature"].as_str().expect("sig"))
        .expect("b64");
    let mut flipped = der.clone();
    *flipped.last_mut().expect("bytes") ^= 0x01;
    // A zero byte before r, which minimal DER never writes; a long-form length; a trailing byte.
    let padded = [&[0x30, der[1] + 1, 0x02, der[3] + 1, 0x00][..], &der[4..]].concat();
    let long_form = [&[0x30, 0x81, der[1]][..], &der[2..]].concat();
    let trailing = [&der[..], &[0x00]].concat();
    for (tag, bytes) in [
        ("flip", flipped),
        ("pad", padded),
        ("long", long_form),
        ("tail", trailing),
    ] {
        let field = json!(URL_SAFE_NO_PAD.encode(bytes));
        let approval = |k: &SoftPasskey, s: &str| {
            passkey_approval(s, proof_with(k, s, "signature", field).as_bytes())
        };
        refused_unspent(&format!("pk-sig-{tag}"), approval, BadSignature);
    }
}

#[test]
fn an_assertion_in_another_namespace_is_refused_wrong_namespace() {
    let key = SoftPasskey::from_seed(1);
    let keys = owner_keys(&Scratch::new("pk-namespace"), &[], &lines_for(&[&key]));
    let s = statement(NONCE);
    let proof = key.assert(&Assert::over("arca-approve", s.as_bytes()));
    assert_eq!(
        keys.verify("arca-approve", s.as_bytes(), &proof),
        Err(WrongNamespace)
    );
    assert_eq!(
        challenge_for("arca-approve", s.as_bytes()),
        Err(WrongNamespace)
    );
    let want = challenge_with(PASSKEY_CHALLENGE_PREFIX, ARCA_NAMESPACE, s.as_bytes());
    assert_eq!(challenge_for(ARCA_NAMESPACE, s.as_bytes()), Ok(want));
}

// ---- the sign count ------------------------------------------------------------------------------------------------------

#[test]
fn a_count_must_rise_above_the_kept_one_and_zero_is_kept_only_until_the_passkey_counts() {
    let (one, two) = (SoftPasskey::from_seed(1), SoftPasskey::from_seed(2));
    let mut v = verifier(&Scratch::new("pk-count"), &[], &lines_for(&[&one, &two]));
    let at = |key: &SoftPasskey, nonce: &str, count: u32| {
        approve(key, &statement(nonce), move |a| a.sign_count = count)
    };
    check(&mut v, &at(&one, NONCE, 0)).expect("0 verifies while nothing is kept");
    check(&mut v, &at(&one, NONCE_B, 0)).expect("0 verifies again");
    check(&mut v, &at(&one, NONCE_C, 5)).expect("5 verifies");
    for count in [5, 4, 1, 0] {
        assert_eq!(
            check(&mut v, &at(&one, NONCE_D, count)),
            Err(SignCountRegressed),
            "{count}"
        );
    }
    check(&mut v, &at(&one, NONCE_D, 6)).expect("6 verifies: the refusals spent nothing");
    check(&mut v, &at(&two, "4123456789abcdef0123456789abcdef", 1)).expect("each keeps its own");
}

#[test]
fn a_counting_passkeys_approval_refused_for_its_arguments_is_used_up_and_a_synced_ones_is_not() {
    // The count is kept once the signature verifies, before the host, the action, the target, the arguments and the time are
    // checked. So the same approval sent again for the right call is a replay of its count.
    let other = json!({ "plan": args()["plan"], "note": "another call" });
    for (count, then) in [(5, Err(SignCountRegressed)), (0, Ok(()))] {
        let key = SoftPasskey::from_seed(1);
        let (_scratch, mut v) = verifier_of(&format!("pk-used-up-{count}"), &key);
        let approval = approve(&key, &statement(NONCE), move |a| a.sign_count = count);
        let wrong = v.check(ArcaAction::Delete, &target(), &other, &approval, NOW);
        assert_eq!(wrong.map(|_| ()), Err(WrongParams), "count {count}");
        assert_eq!(check(&mut v, &approval).map(|_| ()), then, "count {count}");
    }
}

#[test]
fn the_count_file_keeps_counts_across_a_reopen_and_writes_nothing_for_zero() {
    let scratch = Scratch::new("pk-count-file");
    let path = scratch.path(PASSKEY_COUNTS_FILE);
    let fp = SoftPasskey::from_seed(1).fingerprint();
    let len = || fs::metadata(&path).expect("meta").len();
    {
        let mut counts = FileSignCounts::open(&path).expect("the file is created");
        counts
            .advance(&fp, 0)
            .expect("0 is accepted while nothing is kept");
        for bad in ["", "ABCDEF", &"g".repeat(64), &"a".repeat(63), "x\"}\n{"] {
            assert!(
                matches!(counts.advance(bad, 1), Err(Malformed(_))),
                "fingerprint {bad:?}"
            );
        }
        assert_eq!(len(), 0, "neither a 0 nor a bad fingerprint writes");
        counts.advance(&fp, 3).expect("3 is kept");
    }
    assert_eq!(
        fs::metadata(&path).expect("meta").permissions().mode() & 0o777,
        0o600
    );
    let mut counts = FileSignCounts::open(&path).expect("the file reopens");
    assert_eq!(counts.advance(&fp, 3), Err(SignCountRegressed));
    assert_eq!(counts.advance(&fp, 0), Err(SignCountRegressed));
    counts.advance(&fp, 4).expect("4 is kept");
}

#[test]
fn a_count_file_that_is_a_link_or_not_0600_or_held_is_refused() {
    let scratch = Scratch::new("pk-count-trust");
    let (real, link, loose) = (
        scratch.path("real"),
        scratch.path("link"),
        scratch.path("loose"),
    );
    write_mode(&real, b"", 0o600);
    write_mode(&loose, b"", 0o644);
    std::os::unix::fs::symlink(&real, &link).expect("the link is made");
    assert!(matches!(FileSignCounts::open(&link), Err(Untrusted(_))));
    assert!(matches!(FileSignCounts::open(&loose), Err(Untrusted(_))));
    let _held = FileSignCounts::open(&real).expect("the first store opens");
    assert!(matches!(FileSignCounts::open(&real), Err(Storage(_))));
}

#[test]
fn a_torn_count_record_is_dropped_and_a_record_that_does_not_advance_refuses_the_file() {
    let fp = SoftPasskey::from_seed(1).fingerprint();
    let record = |count: u32| format!("{{\"count\":{count},\"fingerprint\":\"{fp}\"}}\n");
    let scratch = Scratch::new("pk-count-torn");
    let path = scratch.path(PASSKEY_COUNTS_FILE);
    let torn = format!("{}{}", record(2), record(9).trim_end_matches("}\n"));
    write_mode(&path, torn.as_bytes(), 0o600);
    let mut counts = FileSignCounts::open(&path).expect("the file opens");
    counts
        .advance(&fp, 3)
        .expect("the torn 9 was never kept, so 3 advances");

    for (tag, text) in [
        ("regress", format!("{}{}", record(5), record(3))),
        ("repeat", format!("{}{}", record(5), record(5))),
        ("zero", record(0)),
        ("spaced", record(5).replacen(':', ": ", 1)),
        (
            "short-fp",
            "{\"count\":5,\"fingerprint\":\"abc\"}\n".to_string(),
        ),
        ("nonce", format!("{{\"at\":1,\"nonce\":\"{NONCE}\"}}\n")),
    ] {
        let scratch = Scratch::new(&format!("pk-count-{tag}"));
        let path = scratch.path(PASSKEY_COUNTS_FILE);
        write_mode(&path, text.as_bytes(), 0o600);
        assert!(
            matches!(FileSignCounts::open(&path), Err(Storage(_))),
            "{tag}"
        );
    }
}

// ---- the proof's form -----------------------------------------------------------------------------------------------------

#[test]
fn a_proof_that_is_not_its_own_canonical_json_is_refused_and_spends_nothing() {
    let key = SoftPasskey::from_seed(1);
    let value = key.proof_value(&Assert::over(ARCA_NAMESPACE, statement(NONCE).as_bytes()));
    let canonical = canonical_json(&value).expect("canonical");
    let order = [
        "v",
        "kind",
        "alg",
        "credentialId",
        "authenticatorData",
        "clientDataJSON",
    ];
    let parts: Vec<String> = order
        .iter()
        .map(|k| format!("\"{k}\":{},", value[*k]))
        .collect();
    let alg = "{\"alg\":\"ES256\",";
    for (tag, text) in [
        (
            "spaced",
            serde_json::to_string_pretty(&value).expect("pretty"),
        ),
        (
            "unsorted",
            format!("{{{}\"signature\":{}}}", parts.concat(), value["signature"]),
        ),
        (
            "duplicate",
            canonical.replacen(alg, &format!("{alg}\"alg\":\"ES256\","), 1),
        ),
        (
            "escaped",
            canonical.replacen("\"ES256\"", "\"\\u0045S256\"", 1),
        ),
    ] {
        let approval = |_: &SoftPasskey, s: &str| passkey_approval(s, text.as_bytes());
        refused_unspent(&format!("pk-noncanon-{tag}"), approval, NotCanonical);
    }
}

#[test]
fn a_proof_with_other_keys_values_or_sizes_is_refused_malformed() {
    let key = SoftPasskey::from_seed(1);
    let s = statement(NONCE);
    let mut missing = key.proof_value(&Assert::over(ARCA_NAMESPACE, s.as_bytes()));
    missing.as_object_mut().expect("object").remove("kind");
    let mut texts = vec![
        proof_with(&key, &s, "extra", json!(1)),
        canonical_json(&missing).expect("canonical"),
        "{".to_string(),
        "{]".to_string(),
        "{\"v\":1}x".to_string(),
    ];
    for (field, bad) in [
        ("v", json!(2)),
        ("v", json!("1")),
        ("kind", json!("ssh")),
        ("alg", json!("ES384")),
        ("credentialId", json!(1)),
        ("credentialId", json!("")),
        ("credentialId", json!(format!("{}=", key.cred_text()))),
        ("signature", json!("ab+c")),
        ("authenticatorData", json!("a")),
    ] {
        texts.push(proof_with(&key, &s, field, bad));
    }
    for (index, text) in texts.into_iter().enumerate() {
        let approval = |_: &SoftPasskey, s: &str| passkey_approval(s, text.as_bytes());
        let (_scratch, mut v) = verifier_of(&format!("pk-proof-{index}"), &key);
        let refusal = check(&mut v, &approval(&key, &s));
        assert!(matches!(refusal, Err(Malformed(_))), "case {index}");
        check(&mut v, &genuine(&key, &s)).expect("the nonce is unspent");
    }

    let long = format!("{{\"v\":\"{}\"}}", "a".repeat(8_192));
    let split = split_approval(&passkey_approval(&s, long.as_bytes()));
    assert!(matches!(split, Err(Malformed(_))));
    assert!(matches!(parse_proof(long.as_bytes()), Err(Malformed(_))));
    let bytes = key.assert(&Assert::over(ARCA_NAMESPACE, s.as_bytes()));
    let mut short = parse_proof(&bytes).expect("the proof parses");
    short.authenticator_data.truncate(36);
    let lines = parse_passkeys(lines_for(&[&key]).as_bytes()).expect("the line parses");
    let refusal = verify_assertion(ARCA_NAMESPACE, s.as_bytes(), &short, &lines);
    assert!(
        matches!(refusal, Err(Malformed(_))),
        "a short authenticatorData"
    );
}

#[test]
fn split_approval_reads_a_passkey_proof_as_its_bytes() {
    let s = statement(NONCE);
    let proof = SoftPasskey::from_seed(1).assert(&Assert::over(ARCA_NAMESPACE, s.as_bytes()));
    let split = split_approval(&passkey_approval(&s, &proof)).expect("splits");
    assert_eq!(split, (s.into_bytes(), proof));
}

#[test]
fn verify_assertion_reports_the_count_and_the_backup_state() {
    let key = SoftPasskey::from_seed(1);
    let s = statement(NONCE);
    let lines = parse_passkeys(lines_for(&[&key]).as_bytes()).expect("the line parses");
    for (flags, backed_up) in [
        (UP | UV, false),
        (UP | UV | BE, false),
        (UP | UV | BE | BS, true),
    ] {
        let mut a = Assert::over(ARCA_NAMESPACE, s.as_bytes());
        (a.flags, a.sign_count) = (flags, 77);
        let proof = parse_proof(&key.assert(&a)).expect("the proof parses");
        let asserted = verify_assertion(ARCA_NAMESPACE, s.as_bytes(), &proof, &lines);
        let asserted = asserted.expect("verifies");
        assert_eq!(asserted.fingerprint, key.fingerprint());
        assert_eq!((asserted.sign_count, asserted.backed_up), (77, backed_up));
    }
}

// ---- the passkeys file ----------------------------------------------------------------------------------------------------

#[test]
fn a_passkey_line_is_kept_with_its_number_rp_origin_and_fingerprint_when_it_lists_arca() {
    let mut key = SoftPasskey::from_seed(1);
    let text = format!("# the owner's passkeys\n\n{}\n", key.line(ARCA_NAMESPACE));
    let want = PasskeyLine {
        line: 3,
        rp_id: RP.to_string(),
        origin: ORIGIN.to_string(),
        credential_id: key.credential_id.clone(),
        spki: key.spki(),
        fingerprint: key.fingerprint(),
    };
    assert_eq!(parse_passkeys(text.as_bytes()), Ok(vec![want]));
    let several = key.line("arca-approve,arca-kernel-approve");
    assert_eq!(parse_passkeys(several.as_bytes()).expect("parses").len(), 1);
    assert_eq!(
        parse_passkeys(key.line("arca-approve").as_bytes()),
        Ok(vec![])
    );
    for origin in ["https://box1.example.com", "https://a.box1.example.com:1"] {
        key.origin = origin.to_string();
        let lines = parse_passkeys(key.line(ARCA_NAMESPACE).as_bytes()).expect("parses");
        assert_eq!(lines[0].origin, origin);
    }
}

#[test]
fn a_malformed_or_repeated_line_refuses_the_whole_file_by_its_number_alone() {
    let key = SoftPasskey::from_seed(1);
    let line = key.line(ARCA_NAMESPACE);
    let good = SoftPasskey::from_seed(2).line(ARCA_NAMESPACE);
    let (cred, key_text, spki) = (key.cred_text(), key.key_text(), key.spki());
    let bent = |from: &str, to: &str| {
        assert!(line.contains(from), "the line holds the part a case bends");
        line.replacen(from, to, 1)
    };
    let with_byte = |at: usize| {
        let mut bytes = spki.clone();
        bytes[at] ^= 0x01;
        STANDARD.encode(bytes)
    };
    let rp = format!("rp=\"{RP}\"");
    let origin = |to: &str| bent(ORIGIN, to);
    let mut same_key = SoftPasskey::from_seed(1);
    same_key.credential_id = vec![0x11; 20];
    let droidd_shape = format!(
        "owner namespaces=\"{ARCA_NAMESPACE}\" {rp} alg=\"ES256\" cred=\"{cred}\" key=\"{key_text}\""
    );
    let cases = [
        droidd_shape,
        bent("alg=\"ES256\"", "alg=\"ES384\""),
        bent("origin=\"https://", "origin=\"http://"),
        origin("https://box1.example.net:8443"),
        origin("https://xbox1.example.com:8443"),
        origin("https://box1.example.com:8443/x"),
        origin("https://box1.example.com:443"),
        origin("https://box1.example.com:0"),
        origin("https://box1.example.com:65536"),
        origin("https://box1.example.com:08443"),
        origin("https://BOX1.example.com:8443"),
        bent("rp=\"box1", "rp=\"Box1"),
        bent(&rp, &format!("rp=\"{RP}.\"")),
        bent(&rp, "rp=\"-box1.example.com\""),
        bent(
            &format!("namespaces=\"{ARCA_NAMESPACE}\""),
            "namespaces=\"\"",
        ),
        bent(&cred, &URL_SAFE_NO_PAD.encode([1u8; 15])),
        bent(&cred, &URL_SAFE_NO_PAD.encode([1u8; 1024])),
        bent(&cred, &format!("{}==", URL_SAFE_NO_PAD.encode([1u8; 16]))),
        bent(&cred, &STANDARD.encode([0xfbu8; 18])),
        bent(&key_text, key_text.trim_end_matches('=')),
        bent(&key_text, &STANDARD.encode(&spki[26..])),
        bent(&key_text, &with_byte(10)),
        bent(&key_text, &with_byte(90)),
        bent("\" rp=", "\"  rp="),
        bent("\" rp=", "\"\trp="),
        bent("owner ", "admin "),
        bent(&format!(" key=\"{key_text}\""), ""),
        format!("{line}\r"),
        format!("{line} caf\u{e9}"),
        format!("{line}\tx"),
        line.clone(),
        same_key.line(ARCA_NAMESPACE),
    ];
    for (index, bad) in cases.iter().enumerate() {
        let text = format!("{line}\n{bad}\n{good}\n");
        let refusal = parse_passkeys(text.as_bytes()).expect_err("the line refuses the file");
        let Untrusted(why) = &refusal else {
            panic!("case {index} is Untrusted: {refusal:?}")
        };
        assert!(why.contains("line 2 "), "case {index} names line 2: {why}");
        let shown = refusal.to_string();
        assert!(
            !shown.contains(&cred) && !shown.contains(&key_text),
            "case {index} echoes"
        );
    }
}

#[test]
fn a_line_debug_and_a_proof_debug_print_no_credential_and_no_key() {
    let key = SoftPasskey::from_seed(1);
    let lines = parse_passkeys(key.line(ARCA_NAMESPACE).as_bytes()).expect("parses");
    let bytes = key.assert(&Assert::over(ARCA_NAMESPACE, statement(NONCE).as_bytes()));
    let shown = format!("{lines:?} {:?}", parse_proof(&bytes).expect("parses"));
    let value: Value = serde_json::from_slice(&bytes).expect("json");
    let sig = value["signature"].as_str().expect("sig");
    let client = value["clientDataJSON"].as_str().expect("client data");
    let secrets = [
        key.cred_text(),
        key.key_text(),
        format!("{:?}", key.credential_id),
    ];
    for secret in secrets.iter().map(String::as_str).chain([sig, client]) {
        assert!(
            !shown.contains(secret),
            "the debug output carries no proof part or key"
        );
    }
}

#[test]
fn a_passkeys_file_that_breaks_the_trust_rule_is_refused() {
    let dir = Scratch::new("pk-trust");
    let text = lines_for(&[&SoftPasskey::from_seed(1)]);
    let file = |name: &str, mode: u32| {
        let path = dir.path(name);
        write_mode(&path, text.as_bytes(), mode);
        path
    };
    let refused =
        |path: &Path, trust: &TrustRoot| matches!(load_passkeys(path, trust), Err(Untrusted(_)));
    // The test user owns every file it wrote, so the root rule refusing is the rule working.
    assert!(refused(&file("root-rule", 0o644), &TrustRoot::Root));
    assert!(refused(&file("group", 0o664), &scratch_trust(&dir)));
    assert!(refused(&file("world", 0o646), &scratch_trust(&dir)));
    let link = dir.path("link");
    std::os::unix::fs::symlink(file("real", 0o644), &link).expect("the link is made");
    assert!(refused(&link, &scratch_trust(&dir)));
    let big = dir.path("big");
    write_mode(
        &big,
        format!("{text}# {}\n", "x".repeat(65_536)).as_bytes(),
        0o644,
    );
    assert!(refused(&big, &scratch_trust(&dir)));

    let sub = dir.path("sub");
    fs::create_dir(&sub).expect("the subdirectory");
    let inner = sub.join("owner-passkeys");
    write_mode(&inner, text.as_bytes(), 0o644);
    fs::set_permissions(&sub, fs::Permissions::from_mode(0o770)).expect("mode");
    assert!(refused(&inner, &scratch_trust(&dir)));
    let dotted = dir.dir.join("sub").join("..").join("real");
    assert!(refused(&dotted, &scratch_trust(&dir)));
}

#[test]
fn a_passkeys_file_that_cannot_be_trusted_refuses_every_passkey_and_the_ssh_path_still_works() {
    // As the kernel builds it: a passkeys file that refuses becomes a passkey verifier that refuses, beside the SSH keys.
    let scratch = Scratch::new("pk-refusing");
    let (owner, key) = (SkKey::from_seed(1), SoftPasskey::from_seed(1));
    let path = scratch.path("owner-passkeys");
    let text = format!("{}\nnot a passkey line\n", key.line(ARCA_NAMESPACE));
    write_mode(&path, text.as_bytes(), 0o600);
    let (path, trust) = passkeys_trust(true, &projects(), Some(path)).expect("injection");
    let refusal = load_passkeys(&path, &trust).expect_err("the malformed line refuses the file");
    assert!(matches!(&refusal, Untrusted(why) if why.contains("line 2 ")));
    let passkeys = PasskeyVerifier::refusing(refusal.clone());
    assert_eq!(passkeys.key_count(), 0);

    let nonces = FileNonces::open(&scratch.path("nonces.jsonl")).expect("nonces");
    let keys = OwnerKeys::new(ssh_for(&scratch, &[&owner]), passkeys);
    let mut v = ApprovalVerifier::new(common::HOST.to_string(), keys, nonces);
    let s = statement(NONCE);
    assert_eq!(check(&mut v, &genuine(&key, &s)), Err(refusal));
    let ssh = approval_of(&s, &owner.sign(ARCA_NAMESPACE, s.as_bytes()));
    check(&mut v, &ssh).expect("the SSH approval still verifies, with the nonce unspent");
}

// ---- the scratch-only injection -----------------------------------------------------------------------------------------

#[test]
fn the_passkeys_injection_is_refused_outside_a_scratch_kernel_and_production_is_root_owned() {
    assert_eq!(OWNER_PASSKEYS, "/etc/arca/owner-passkeys");
    assert_eq!(TEST_PASSKEYS_ENV, "ARCA_TEST_OWNER_PASSKEYS");
    let production = passkeys_trust(false, &[], None).expect("no injection");
    assert_eq!(production, (PathBuf::from(OWNER_PASSKEYS), TrustRoot::Root));

    let named = Path::new("/tmp/arca-approval-pk/owner-passkeys");
    let refused = |scratch: bool, names: &[&str], path: &Path| {
        let names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
        let result = passkeys_trust(scratch, &names, Some(path.to_path_buf()));
        matches!(result, Err(Untrusted(why)) if why.contains("passkeys"))
    };
    assert!(refused(false, &["arcak-scratch-a"], named));
    assert!(refused(true, &["arcak-scratch-a", "arcak-svc"], named));
    assert!(refused(true, &[], named));
    assert!(refused(true, &["arcak-scratch-"], named));
    assert!(refused(
        true,
        &["arcak-scratch-a"],
        Path::new("owner-passkeys")
    ));
}

#[cfg(debug_assertions)]
#[test]
fn the_passkeys_injection_is_honoured_for_a_scratch_kernel_in_a_debug_build() {
    let dir = Scratch::new("pk-inject");
    let file = dir.path("owner-passkeys");
    let trust = passkeys_trust(true, &projects(), Some(file.clone())).expect("honoured");
    assert_eq!(trust, (file, scratch_trust(&dir)));
}
