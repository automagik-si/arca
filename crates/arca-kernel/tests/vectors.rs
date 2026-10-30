//! The Rust side reads droidd's approval vectors, which the reference implementation's vector suite generated from droidd's
//! own `paramsDigest` and `parseOwnerStatement`. The fixture is a byte-for-byte copy of that file.

use arca_kernel::canon::{Digest, canonical_json, params_digest};
use serde_json::Value;

const FIXTURE: &str = include_str!("fixtures/droidd-vectors.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("the vectors fixture is JSON")
}

#[test]
fn params_digest_equals_each_digest_droidd_produced() {
    let f = fixture();
    let params = f["params"].as_array().expect("params is an array");
    assert_eq!(
        params.len(),
        3,
        "the fixture carries three paramsDigest vectors"
    );
    for vector in params {
        let want = vector["digest"].as_str().expect("digest is a string");
        assert_eq!(
            params_digest(&vector["args"]).expect("the args are an object"),
            want,
            "args {}",
            vector["args"]
        );
        assert!(
            Digest::from_hex(want).is_ok(),
            "the vector's digest is 64 lowercase hex"
        );
    }
}

#[test]
fn the_statement_fields_canonicalize_to_the_text_droidd_signs() {
    let f = fixture();
    let text = f["statement"]["text"].as_str().expect("statement text");
    let fields = &f["statement"]["fields"];
    assert_eq!(canonical_json(fields).expect("fields canonicalize"), text);
}

#[test]
fn the_statement_text_is_its_own_canonical_form() {
    let f = fixture();
    let text = f["statement"]["text"].as_str().expect("statement text");
    let parsed: Value = serde_json::from_str(text).expect("the statement text is JSON");
    assert_eq!(canonical_json(&parsed).expect("canonicalizes"), text);
}
