//! RS256 passkeys (COSE -257): the RSA keys some Windows Hello and TPM authenticators make, beside ES256. A line names
//! `alg="RS256"` with the key's RSA SPKI DER, a proof names `alg` `RS256`, and the signature is RSASSA-PKCS1-v1_5 with
//! SHA-256 over `authenticatorData || SHA-256(clientDataJSON)`.
//!
//! Two kinds of vectors. One is a real authenticator's RS256 assertion, the one py_webauthn's tests verify
//! (`tests/test_verify_authentication_response.py`, `test_verify_authentication_response_with_RSA_public_key`, BSD-3-Clause,
//! duo-labs/py_webauthn): its COSE key is written here as the SPKI DER a browser's `getPublicKey()` returns, and its
//! signature is checked as Arca checks one. The other is a software RSA-2048 passkey from two fixed primes, never an
//! owner's, that answers Arca's own challenge so the whole assertion path runs. No test prints a proof, a credential id or a
//! key.

#[path = "common/webauthn.rs"]
mod webauthn;

use ApprovalRefusal::{BadSignature, Malformed, Untrusted};
use arca_approval::{
    ARCA_NAMESPACE, ApprovalRefusal, PasskeyAlg, format_passkey_line, parse_passkeys, parse_proof,
    verify_assertion, verify_passkey_signature,
};
use arca_kernel::canon::canonical_json;
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use rsa::pkcs1v15::Pkcs1v15Sign;
use rsa::pkcs8::EncodePublicKey as _;
use rsa::{BigUint, RsaPrivateKey};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use webauthn::{ORIGIN, RP, SoftPasskey, UP, UV, challenge_with};

// ---- the real authenticator's RS256 assertion ------------------------------------------------------------------------------

/// Its public key as SPKI DER, padded base64: an RSA-2048 key with exponent 65537.
const REAL_SPKI: &str = "MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA31dtHqc70D/h7XHQ6V/nBs3Tscu91kBL7FOw56/VFiaKYRH6Z4KLr4J0S12hFJ/3fBxpKfxyMfK66ZMeAVbOl/wemY4S5Xs4yHSWy21Xm/dgWhLJjZ9R1tjfV49kDPHB/ssdvP7wo3/NmoUPYMgK+edgZ/ehttp/I6hUUCnVaTvn/m76b2j9yEPReSwl+wlGsabYG6INUhTuhSOqG+UpVVQdNJVV7GmIPHCA2cQpJBDZBohT4MBGme/feUgm4sgqVCWzKk6CzIKIz5AIVnspLbu05SulAVnSTB3NxTwCLNJR/9v9oSkvphiNbmQBVQH1tV/psyi9HM1Jtj9VJVKMewIDAQAB";
/// Its COSE key, as py_webauthn holds it: kty 3 (RSA), alg -257, n, e.
const REAL_COSE: &str = "pAEDAzkBACBZAQDfV20epzvQP-HtcdDpX-cGzdOxy73WQEvsU7Dnr9UWJophEfpngouvgnRLXaEUn_d8HGkp_HIx8rrpkx4BVs6X_B6ZjhLlezjIdJbLbVeb92BaEsmNn1HW2N9Xj2QM8cH-yx28_vCjf82ahQ9gyAr552Bn96G22n8jqFRQKdVpO-f-bvpvaP3IQ9F5LCX7CUaxptgbog1SFO6FI6ob5SlVVB00lVXsaYg8cIDZxCkkENkGiFPgwEaZ7995SCbiyCpUJbMqToLMgojPkAhWeyktu7TlK6UBWdJMHc3FPAIs0lH_2_2hKS-mGI1uZAFVAfW1X-mzKL0czUm2P1UlUox7IUMBAAE";
const REAL_CRED: &str = "ZoIKP1JQvKdrYj1bTUPJ2eTUsbLeFkv-X5xJQNr4k6s";
/// rpIdHash of `localhost`, flags UP and UV, sign count 1.
const REAL_AUTH: &str = "SZYN5YgOjGh0NBcPZHZgW4_krrmihjLHmVzzuoMdl2MFAAAAAQ";
const REAL_CLIENT: &str = "eyJ0eXBlIjoid2ViYXV0aG4uZ2V0IiwiY2hhbGxlbmdlIjoiaVBtQWkxUHAxWEw2b0FncTNQV1p0WlBuWmExekZVRG9HYmFRMF9LdlZHMWxGMnMzUnRfM280dVN6Y2N5MHRtY1RJcFRUVDRCVTFULUk0bWFhdm5kalEiLCJvcmlnaW4iOiJodHRwOi8vbG9jYWxob3N0OjUwMDAiLCJjcm9zc09yaWdpbiI6ZmFsc2V9";
const REAL_SIGNATURE: &str = "iOHKX3erU5_OYP_r_9HLZ-CexCE4bQRrxM8WmuoKTDdhAnZSeTP0sjECjvjfeS8MJzN1ArmvV0H0C3yy_FdRFfcpUPZzdZ7bBcmPh1XPdxRwY747OrIzcTLTFQUPdn1U-izCZtP_78VGw9pCpdMsv4CUzZdJbEcRtQuRS03qUjqDaovoJhOqEBmxJn9Wu8tBi_Qx7A33RbYjlfyLm_EDqimzDZhyietyop6XUcpKarKqVH0M6mMrM5zTjp8xf3W7odFCadXEJg-ERZqFM0-9Uup6kJNLbr6C5J4NDYmSm3HCSA6lp2iEiMPKU8Ii7QZ61kybXLxsX4w4Dm3fOLjmDw";

fn b64url(text: &str) -> Vec<u8> {
    URL_SAFE_NO_PAD
        .decode(text)
        .expect("a fixture is base64url")
}

/// The real key's SPKI, checked against its COSE form: the same modulus and exponent.
fn real_spki() -> Vec<u8> {
    let spki = STANDARD.decode(REAL_SPKI).expect("a fixture is base64");
    let cose = b64url(REAL_COSE);
    let modulus_at = cose
        .windows(4)
        .position(|w| w == [0x20, 0x59, 0x01, 0x00])
        .expect("the COSE key holds a 256-byte n")
        + 4;
    let modulus = &cose[modulus_at..modulus_at + 256];
    assert_eq!(&cose[modulus_at + 256..], [0x21, 0x43, 0x01, 0x00, 0x01]);
    assert!(
        spki.windows(256).any(|w| w == modulus),
        "the SPKI holds the COSE key's modulus"
    );
    spki
}

#[test]
fn a_real_authenticators_rs256_assertion_verifies() {
    let spki = real_spki();
    let (auth, client, signature) = (
        b64url(REAL_AUTH),
        b64url(REAL_CLIENT),
        b64url(REAL_SIGNATURE),
    );
    assert_eq!(signature.len(), 256);
    assert_eq!(PasskeyAlg::of_key(&spki), Some(PasskeyAlg::Rs256));
    assert_eq!(
        verify_passkey_signature(&spki, &auth, &client, &signature),
        Ok(())
    );
}

#[test]
fn a_real_rs256_assertion_bent_anywhere_is_refused() {
    let spki = real_spki();
    let (auth, client, signature) = (
        b64url(REAL_AUTH),
        b64url(REAL_CLIENT),
        b64url(REAL_SIGNATURE),
    );
    let mut flipped = signature.clone();
    flipped[100] ^= 0x01;
    let mut count = auth.clone();
    count[36] = 2;
    let mut other_client = client.clone();
    other_client[10] ^= 0x01;
    for (auth, client, signature) in [
        (&auth, &client, &flipped),
        (&auth, &client, &signature[1..].to_vec()),
        (&auth, &client, &[&[0u8][..], &signature].concat()),
        (&count, &client, &signature),
        (&auth, &other_client, &signature),
    ] {
        assert_eq!(
            verify_passkey_signature(&spki, auth, client, signature),
            Err(BadSignature)
        );
    }
}

#[test]
fn the_real_keys_line_is_written_and_read_as_rs256() {
    let line = format_passkey_line(LOCALHOST, "http://localhost:5000", REAL_CRED, REAL_SPKI)
        .expect("an RSA-2048 SPKI is a passkey key");
    assert_eq!(
        line,
        format!(
            "owner namespaces=\"{ARCA_NAMESPACE}\" rp=\"localhost\" origin=\"http://localhost:5000\" alg=\"RS256\" \
             cred=\"{REAL_CRED}\" key=\"{REAL_SPKI}\""
        )
    );
    let parsed = parse_passkeys(line.as_bytes()).expect("the line parses");
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].spki, real_spki());
    assert_eq!(parsed[0].alg(), PasskeyAlg::Rs256);
    assert_eq!(parsed[0].fingerprint, hex(&Sha256::digest(real_spki())));
}

const LOCALHOST: &str = "localhost";

// ---- a software RSA-2048 passkey ---------------------------------------------------------------------------------------------

/// Two fixed 1024-bit primes: an RSA-2048 test key, never an owner's.
const P: &str = "e71e6ccb7527166806ff2fd1b69e22aca466d0bb83508bcdf2f93a87deffd47e7f08654cef8e6eea0dc5cb0378b0967fd110aaafd021b521a1cd9ee9abb220a0c9e266e25ac964ddf39b14caad0fdc7aaea48d2c2d197375894b26485243dd5fc72b72392005917e32bb0f267ba5f6bf2d6182d0d9aca83d9ba1ae435a6ec8a1";
const Q: &str = "e64fa9a0515f9164091a0e98d126dd6adb64fb3c88d27f9475f16d12b42ff41b70521c1f0881a9283dba68b008e09773a78d776a425604486a8d79c2270ea25108f2686f7bf6cc3cf02c7f2304194fddfc567c24d9deb653a8a00cb64807b4cc9c964123d4b8216cb521c855ef4662c005f2fe5c5d0fbd0325e02ac1a69d6079";
/// Two fixed 512-bit primes: an RSA-1024 key, too short for a passkey here.
const P_SHORT: &str = "f6d95e80e8ea77b355247d8146b45b5b452c9cc44a7fd391670ef63a5f660bd082277d8c358c83f4b14a95041a8782090146a924220c3da5990138f7bd2c79bd";
const Q_SHORT: &str = "c099fcc0326c5f125608fcf4b906a3a8c4d26c52ed6c005fbebbc16a2b70759d247c47dab854ad8b0b9033abdc390e179a03ce2015534d723c6cb6ab2d73a57d";

/// The DER DigestInfo prefix of a SHA-256 hash (RFC 8017 section 9.2, note 1).
const SHA256_DIGEST_INFO: [u8; 19] = [
    0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05,
    0x00, 0x04, 0x20,
];

struct RsaPasskey {
    key: RsaPrivateKey,
    credential_id: Vec<u8>,
}

impl RsaPasskey {
    fn from_primes(p: &str, q: &str) -> Self {
        let prime = |hex| BigUint::parse_bytes(hex, 16).expect("a fixture prime is hex");
        let key = RsaPrivateKey::from_p_q(
            prime(p.as_bytes()),
            prime(q.as_bytes()),
            BigUint::from(65_537u32),
        )
        .expect("two primes make a key");
        Self {
            key,
            credential_id: vec![0x3c; 32],
        }
    }

    fn spki(&self) -> Vec<u8> {
        self.key
            .to_public_key()
            .to_public_key_der()
            .expect("a public key encodes")
            .as_bytes()
            .to_vec()
    }

    fn line(&self) -> String {
        format!(
            "owner namespaces=\"{ARCA_NAMESPACE}\" rp=\"{RP}\" origin=\"{ORIGIN}\" alg=\"RS256\" cred=\"{}\" key=\"{}\"",
            URL_SAFE_NO_PAD.encode(&self.credential_id),
            STANDARD.encode(self.spki())
        )
    }

    /// The proof bytes a browser's page sends for `statement`, naming `alg`, with `count` as the sign count.
    fn proof(&self, statement: &[u8], alg: &str, count: u32) -> Vec<u8> {
        let challenge = challenge_with("arca-kernel-approval/v1\n", ARCA_NAMESPACE, statement);
        let client = format!(
            "{{\"type\":\"webauthn.get\",\"challenge\":\"{}\",\"origin\":\"{ORIGIN}\",\"crossOrigin\":false}}",
            URL_SAFE_NO_PAD.encode(challenge)
        )
        .into_bytes();
        let mut auth = Sha256::digest(RP.as_bytes()).to_vec();
        auth.push(UP | UV);
        auth.extend_from_slice(&count.to_be_bytes());
        let mut signed = auth.clone();
        signed.extend_from_slice(&Sha256::digest(&client));
        let padding = Pkcs1v15Sign {
            hash_len: Some(32),
            prefix: SHA256_DIGEST_INFO.to_vec().into_boxed_slice(),
        };
        let signature = self
            .key
            .sign(padding, &Sha256::digest(&signed))
            .expect("the key signs");
        let value: Value = json!({
            "v": 1,
            "kind": "webauthn",
            "alg": alg,
            "credentialId": URL_SAFE_NO_PAD.encode(&self.credential_id),
            "authenticatorData": URL_SAFE_NO_PAD.encode(&auth),
            "clientDataJSON": URL_SAFE_NO_PAD.encode(&client),
            "signature": URL_SAFE_NO_PAD.encode(&signature),
        });
        canonical_json(&value)
            .expect("the proof canonicalizes")
            .into_bytes()
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

const STATEMENT: &[u8] = b"{\"an\":\"approval statement\"}";

#[test]
fn an_rs256_passkeys_assertion_over_arcas_challenge_verifies() {
    let key = RsaPasskey::from_primes(P, Q);
    let lines = parse_passkeys(key.line().as_bytes()).expect("an RS256 line parses");
    let proof = parse_proof(&key.proof(STATEMENT, "RS256", 9)).expect("an RS256 proof parses");
    assert_eq!(proof.alg, PasskeyAlg::Rs256);
    let asserted = verify_assertion(ARCA_NAMESPACE, STATEMENT, &proof, &lines).expect("verifies");
    assert_eq!(asserted.fingerprint, hex(&Sha256::digest(key.spki())));
    assert_eq!(asserted.sign_count, 9);
}

#[test]
fn an_rs256_passkey_beside_an_es256_one_each_verify_their_own_proofs() {
    let rsa_key = RsaPasskey::from_primes(P, Q);
    let ec_key = SoftPasskey::from_seed(7);
    let file = format!("{}\n{}\n", ec_key.line(ARCA_NAMESPACE), rsa_key.line());
    let lines = parse_passkeys(file.as_bytes()).expect("both lines parse");
    assert_eq!(
        lines.iter().map(|l| l.alg()).collect::<Vec<_>>(),
        [PasskeyAlg::Es256, PasskeyAlg::Rs256]
    );
    let ec_proof = ec_key.assert(&webauthn::Assert::over(ARCA_NAMESPACE, STATEMENT));
    let rsa_proof = rsa_key.proof(STATEMENT, "RS256", 1);
    for proof in [ec_proof, rsa_proof] {
        let proof = parse_proof(&proof).expect("parses");
        verify_assertion(ARCA_NAMESPACE, STATEMENT, &proof, &lines).expect("verifies");
    }
}

#[test]
fn a_proof_whose_alg_is_not_its_passkeys_is_refused() {
    let key = RsaPasskey::from_primes(P, Q);
    let lines = parse_passkeys(key.line().as_bytes()).expect("parses");
    let proof = parse_proof(&key.proof(STATEMENT, "ES256", 1)).expect("an ES256 proof parses");
    assert_eq!(
        verify_assertion(ARCA_NAMESPACE, STATEMENT, &proof, &lines),
        Err(BadSignature)
    );
    let ec_key = SoftPasskey::from_seed(7);
    let ec_lines = parse_passkeys(ec_key.line(ARCA_NAMESPACE).as_bytes()).expect("parses");
    let mut value: Value =
        serde_json::from_slice(&ec_key.assert(&webauthn::Assert::over(ARCA_NAMESPACE, STATEMENT)))
            .expect("json");
    value["alg"] = json!("RS256");
    let bytes = canonical_json(&value).expect("canonical").into_bytes();
    let proof = parse_proof(&bytes).expect("an RS256 proof parses");
    assert_eq!(
        verify_assertion(ARCA_NAMESPACE, STATEMENT, &proof, &ec_lines),
        Err(BadSignature)
    );
}

#[test]
fn a_proof_names_es256_or_rs256_and_nothing_else() {
    let key = RsaPasskey::from_primes(P, Q);
    for alg in ["EdDSA", "PS256", "RS384", "rs256", ""] {
        assert!(
            matches!(
                parse_proof(&key.proof(STATEMENT, alg, 1)),
                Err(Malformed(_))
            ),
            "{alg}"
        );
    }
}

#[test]
fn format_passkey_line_names_the_alg_its_key_holds() {
    let rsa_key = RsaPasskey::from_primes(P, Q);
    let cred = URL_SAFE_NO_PAD.encode(&rsa_key.credential_id);
    let line = format_passkey_line(RP, ORIGIN, &cred, &STANDARD.encode(rsa_key.spki()))
        .expect("an RSA-2048 key formats");
    assert_eq!(line, rsa_key.line());
    let ec_key = SoftPasskey::from_seed(7);
    let ec_line = format_passkey_line(RP, ORIGIN, &ec_key.cred_text(), &ec_key.key_text())
        .expect("a P-256 key formats");
    assert!(ec_line.contains(" alg=\"ES256\" "), "{ec_line}");
    let short = RsaPasskey::from_primes(P_SHORT, Q_SHORT);
    assert!(matches!(
        format_passkey_line(RP, ORIGIN, &cred, &STANDARD.encode(short.spki())),
        Err(Malformed(_))
    ));
}

#[test]
fn a_line_whose_key_is_not_its_alg_or_is_short_refuses_the_file() {
    let rsa_key = RsaPasskey::from_primes(P, Q);
    let ec_key = SoftPasskey::from_seed(7);
    let rsa_as_es256 = rsa_key.line().replace("alg=\"RS256\"", "alg=\"ES256\"");
    let ec_as_rs256 = ec_key
        .line(ARCA_NAMESPACE)
        .replace("alg=\"ES256\"", "alg=\"RS256\"");
    let short = RsaPasskey::from_primes(P_SHORT, Q_SHORT).line();
    let mut padded_spki = rsa_key.spki();
    padded_spki.push(0);
    let trailing = rsa_key.line().replace(
        &STANDARD.encode(rsa_key.spki()),
        &STANDARD.encode(&padded_spki),
    );
    for line in [rsa_as_es256, ec_as_rs256, short, trailing] {
        assert!(
            matches!(parse_passkeys(line.as_bytes()), Err(Untrusted(_))),
            "{line}"
        );
    }
}

#[test]
fn passkey_algs_name_their_cose_numbers() {
    assert_eq!(PasskeyAlg::from_cose(-7), Some(PasskeyAlg::Es256));
    assert_eq!(PasskeyAlg::from_cose(-257), Some(PasskeyAlg::Rs256));
    assert_eq!(PasskeyAlg::from_cose(-8), None);
    assert_eq!(PasskeyAlg::Rs256.name(), "RS256");
    assert_eq!(PasskeyAlg::Es256.cose(), -7);
}
