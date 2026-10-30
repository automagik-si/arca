//! The acceptance gate: replay, fork, foreign-node, signer and expiry refusals. Every refusal leaves the applied state
//! where it was.

use arca_kernel::canon::Digest;
use arca_kernel::desired::{
    Applied, DesiredStateGate, DocRefusal, Document, Entry, EntryId, PluginId, SignedDocument,
    Target, instant_ms, sign_document,
};
use arca_kernel::identity::{NodeId, NodeKey};
use serde_json::{Value, json};

const ISSUED: &str = "2026-10-30T12:00:00Z";
const EXPIRES: &str = "2026-10-31T12:00:00Z";
/// 2026-10-30T13:00:00Z: inside the window every document below is issued for.
const NOW_MS: u64 = 1_793_365_200_000;

fn entry(id: &str, config: Value) -> Entry {
    Entry {
        id: EntryId(id.to_string()),
        plugin: PluginId("arca.watch".to_string()),
        version: "0.1.0".to_string(),
        digest: Digest::of(id.as_bytes()),
        signer: "node".to_string(),
        config,
        disabled: false,
    }
}

fn doc(node: NodeId, seq: u64, prev: Option<Digest>, entries: Vec<Entry>) -> Document {
    Document {
        node,
        seq,
        prev,
        issued_at: ISSUED.to_string(),
        expires_at: EXPIRES.to_string(),
        entries,
    }
}

fn with_expiry(mut d: Document, expires: &str) -> Document {
    d.expires_at = expires.to_string();
    d
}

struct Node {
    key: NodeKey,
    gate: DesiredStateGate,
}

fn node() -> Node {
    let key = NodeKey::generate();
    let gate = DesiredStateGate::new(key.public(), None);
    Node { key, gate }
}

fn sign(key: &NodeKey, d: Document) -> SignedDocument {
    sign_document(d, key)
}

fn applied(seq: u64, d: &Document) -> Applied {
    Applied {
        seq,
        digest: d.digest(),
    }
}

/// A node with documents seq 1 and seq 2 applied, returned with both documents for the tests to replay or fork.
fn two_applied() -> (Node, Document, Document) {
    let mut n = node();
    let id = n.key.id();
    let d1 = doc(id, 1, None, vec![entry("watch", json!({"on": true}))]);
    let d2 = doc(
        id,
        2,
        Some(d1.digest()),
        vec![entry("watch", json!({"on": false}))],
    );
    assert_eq!(
        n.gate.accept(&sign(&n.key, d1.clone()), NOW_MS),
        Ok(Target::Apply(applied(1, &d1)))
    );
    assert_eq!(
        n.gate.accept(&sign(&n.key, d2.clone()), NOW_MS),
        Ok(Target::Apply(applied(2, &d2)))
    );
    (n, d1, d2)
}

#[test]
fn a_first_document_applies_and_becomes_the_applied_state() {
    let mut n = node();
    let d1 = doc(n.key.id(), 1, None, vec![entry("watch", json!({}))]);
    assert_eq!(
        n.gate.accept(&sign(&n.key, d1.clone()), NOW_MS),
        Ok(Target::Apply(applied(1, &d1)))
    );
    assert_eq!(n.gate.applied(), Some(&applied(1, &d1)));
}

#[test]
fn a_successor_that_names_the_applied_digest_applies() {
    let (mut n, d1, d2) = two_applied();
    let d3 = doc(
        n.key.id(),
        3,
        Some(d2.digest()),
        vec![entry("watch", json!({"on": true}))],
    );
    assert_eq!(
        n.gate.accept(&sign(&n.key, d3.clone()), NOW_MS),
        Ok(Target::Apply(applied(3, &d3)))
    );
    assert_ne!(d1.digest(), d3.digest());
}

#[test]
fn a_replayed_older_document_is_refused_and_the_applied_state_does_not_move() {
    let (mut n, d1, d2) = two_applied();
    let before = n.gate.applied().cloned();
    let replay = sign(&n.key, d1);
    assert_eq!(
        n.gate.accept(&replay, NOW_MS),
        Err(DocRefusal::Stale { applied: 2, got: 1 }),
        "a valid signed document below the applied seq is a replay"
    );
    assert_eq!(n.gate.applied().cloned(), before);
    assert_eq!(n.gate.applied(), Some(&applied(2, &d2)));
}

#[test]
fn a_replay_of_an_earlier_publish_cannot_bring_back_a_removed_entry() {
    // Seq 1 publishes an entry; seq 2 removes it. Replaying seq 1 must not bring the publish back or undo the removal.
    let mut n = node();
    let id = n.key.id();
    let publish = doc(id, 1, None, vec![entry("route", json!({"port": 443}))]);
    let remove = doc(id, 2, Some(publish.digest()), vec![]);
    n.gate
        .accept(&sign(&n.key, publish.clone()), NOW_MS)
        .expect("seq 1 applies");
    n.gate
        .accept(&sign(&n.key, remove.clone()), NOW_MS)
        .expect("seq 2 applies");
    assert_eq!(
        n.gate.accept(&sign(&n.key, publish), NOW_MS),
        Err(DocRefusal::Stale { applied: 2, got: 1 })
    );
    assert_eq!(n.gate.applied(), Some(&applied(2, &remove)));
}

#[test]
fn an_equal_seq_with_another_digest_is_refused() {
    let (mut n, d1, d2) = two_applied();
    // Same seq as the applied document, same prev, different entries: a second document claiming the applied seq.
    let rival = doc(
        n.key.id(),
        2,
        Some(d1.digest()),
        vec![entry("watch", json!({"on": "rival"}))],
    );
    assert_ne!(rival.digest(), d2.digest());
    assert_eq!(
        n.gate.accept(&sign(&n.key, rival), NOW_MS),
        Err(DocRefusal::EqualSeqOtherDigest { seq: 2 })
    );
    assert_eq!(n.gate.applied(), Some(&applied(2, &d2)));
}

#[test]
fn replaying_the_applied_document_is_idempotent() {
    let (mut n, _d1, d2) = two_applied();
    assert_eq!(
        n.gate.accept(&sign(&n.key, d2.clone()), NOW_MS),
        Ok(Target::Apply(applied(2, &d2))),
        "the same seq with the same digest applies again without change"
    );
    assert_eq!(n.gate.applied(), Some(&applied(2, &d2)));
}

#[test]
fn a_fork_whose_prev_is_an_older_document_is_refused() {
    let (mut n, d1, d2) = two_applied();
    let fork = doc(
        n.key.id(),
        3,
        Some(d1.digest()),
        vec![entry("watch", json!({"fork": true}))],
    );
    assert_eq!(
        n.gate.accept(&sign(&n.key, fork), NOW_MS),
        Err(DocRefusal::Forked {
            expected_prev: d2.digest()
        }),
        "seq 3 names seq 1, not the applied seq 2"
    );
    assert_eq!(n.gate.applied(), Some(&applied(2, &d2)));
}

#[test]
fn a_fork_with_no_prev_is_refused_once_something_is_applied() {
    let (mut n, _d1, d2) = two_applied();
    let fork = doc(n.key.id(), 3, None, vec![entry("watch", json!({}))]);
    assert_eq!(
        n.gate.accept(&sign(&n.key, fork), NOW_MS),
        Err(DocRefusal::Forked {
            expected_prev: d2.digest()
        })
    );
    assert_eq!(n.gate.applied(), Some(&applied(2, &d2)));
}

#[test]
fn a_document_for_another_node_is_refused_even_when_this_node_signed_it() {
    let mut n = node();
    let other = NodeKey::generate();
    let foreign = doc(other.id(), 1, None, vec![entry("watch", json!({}))]);
    assert_eq!(
        n.gate.accept(&sign(&n.key, foreign), NOW_MS),
        Err(DocRefusal::ForeignNode)
    );
    assert_eq!(n.gate.applied(), None);
}

#[test]
fn a_document_signed_by_another_key_is_refused_as_an_unknown_signer() {
    let mut n = node();
    let intruder = NodeKey::generate();
    let d1 = doc(n.key.id(), 1, None, vec![entry("watch", json!({}))]);
    assert_eq!(
        n.gate.accept(&sign(&intruder, d1), NOW_MS),
        Err(DocRefusal::UnknownSigner)
    );
    assert_eq!(n.gate.applied(), None);
}

#[test]
fn a_forged_signer_field_over_a_foreign_signature_is_refused_as_a_bad_signature() {
    let mut n = node();
    let intruder = NodeKey::generate();
    let d1 = doc(n.key.id(), 1, None, vec![entry("watch", json!({}))]);
    let mut forged = sign(&intruder, d1);
    forged.signer = n.key.public();
    assert_eq!(
        n.gate.accept(&forged, NOW_MS),
        Err(DocRefusal::BadSignature)
    );
    assert_eq!(n.gate.applied(), None);
}

#[test]
fn a_tampered_body_under_a_valid_signature_is_refused() {
    let mut n = node();
    let d1 = doc(
        n.key.id(),
        1,
        None,
        vec![entry("watch", json!({"on": true}))],
    );
    let mut signed = sign(&n.key, d1);
    signed.document.entries[0].config = json!({"on": false});
    assert_eq!(
        n.gate.accept(&signed, NOW_MS),
        Err(DocRefusal::BadSignature)
    );
    assert_eq!(n.gate.applied(), None);
}

#[test]
fn a_duplicate_entry_id_is_refused() {
    let mut n = node();
    let d1 = doc(
        n.key.id(),
        1,
        None,
        vec![
            entry("watch", json!({})),
            entry("watch", json!({"again": true})),
        ],
    );
    assert_eq!(
        n.gate.accept(&sign(&n.key, d1), NOW_MS),
        Err(DocRefusal::DuplicateEntry(EntryId("watch".to_string())))
    );
    assert_eq!(n.gate.applied(), None);
}

#[test]
fn a_first_document_that_names_a_previous_one_is_refused() {
    let mut n = node();
    let d1 = doc(
        n.key.id(),
        1,
        Some(Digest::of(b"earlier")),
        vec![entry("watch", json!({}))],
    );
    assert!(matches!(
        n.gate.accept(&sign(&n.key, d1), NOW_MS),
        Err(DocRefusal::Malformed(_))
    ));
    assert_eq!(n.gate.applied(), None);
}

#[test]
fn seq_zero_is_refused() {
    let mut n = node();
    let d0 = doc(n.key.id(), 0, None, vec![entry("watch", json!({}))]);
    assert!(matches!(
        n.gate.accept(&sign(&n.key, d0), NOW_MS),
        Err(DocRefusal::Malformed(_))
    ));
    assert_eq!(n.gate.applied(), None);
}

#[test]
fn an_entry_id_outside_the_name_rule_is_refused() {
    let mut n = node();
    let bad = doc(n.key.id(), 1, None, vec![entry("Bad_Name", json!({}))]);
    assert!(matches!(
        n.gate.accept(&sign(&n.key, bad), NOW_MS),
        Err(DocRefusal::Malformed(_))
    ));
    assert_eq!(n.gate.applied(), None);
}

#[test]
fn an_entry_config_that_is_not_an_object_is_refused() {
    let mut n = node();
    let bad = doc(n.key.id(), 1, None, vec![entry("watch", json!([1, 2]))]);
    assert!(matches!(
        n.gate.accept(&sign(&n.key, bad), NOW_MS),
        Err(DocRefusal::Malformed(_))
    ));
}

#[test]
fn an_expired_document_holds_the_last_applied_state_and_reports_stale() {
    let (mut n, _d1, d2) = two_applied();
    let d3 = with_expiry(
        doc(
            n.key.id(),
            3,
            Some(d2.digest()),
            vec![entry("watch", json!({}))],
        ),
        "2026-10-30T12:30:00Z",
    );
    assert_eq!(
        n.gate.accept(&sign(&n.key, d3), NOW_MS),
        Ok(Target::Hold {
            last: Some(applied(2, &d2)),
            stale: true
        })
    );
    assert_eq!(n.gate.applied(), Some(&applied(2, &d2)));
}

#[test]
fn an_expired_first_document_holds_nothing_applied() {
    let mut n = node();
    let d1 = with_expiry(
        doc(n.key.id(), 1, None, vec![entry("watch", json!({}))]),
        "2026-10-30T12:30:00Z",
    );
    assert_eq!(
        n.gate.accept(&sign(&n.key, d1), NOW_MS),
        Ok(Target::Hold {
            last: None,
            stale: true
        })
    );
    assert_eq!(n.gate.applied(), None);
}

#[test]
fn a_missing_document_holds_the_last_applied_state_and_never_applies() {
    let (n, _d1, d2) = two_applied();
    assert_eq!(
        n.gate.missing(),
        Target::Hold {
            last: Some(applied(2, &d2)),
            stale: true
        }
    );
    let fresh = node();
    assert_eq!(
        fresh.gate.missing(),
        Target::Hold {
            last: None,
            stale: true
        },
        "nothing applied and nothing offered is a hold, never an empty apply"
    );
}

#[test]
fn an_expiry_that_is_not_after_the_issue_time_is_refused() {
    let mut n = node();
    let d1 = with_expiry(
        doc(n.key.id(), 1, None, vec![entry("watch", json!({}))]),
        ISSUED,
    );
    assert!(matches!(
        n.gate.accept(&sign(&n.key, d1), NOW_MS),
        Err(DocRefusal::Malformed(_))
    ));
}

#[test]
fn an_unparseable_time_is_refused() {
    let mut n = node();
    let mut d1 = doc(n.key.id(), 1, None, vec![entry("watch", json!({}))]);
    d1.issued_at = "yesterday".to_string();
    assert!(matches!(
        n.gate.accept(&sign(&n.key, d1), NOW_MS),
        Err(DocRefusal::Malformed(_))
    ));
}

#[test]
fn a_gap_in_seq_with_the_applied_digest_as_prev_applies() {
    let (mut n, _d1, d2) = two_applied();
    let d7 = doc(
        n.key.id(),
        7,
        Some(d2.digest()),
        vec![entry("watch", json!({}))],
    );
    assert_eq!(
        n.gate.accept(&sign(&n.key, d7.clone()), NOW_MS),
        Ok(Target::Apply(applied(7, &d7))),
        "seq only has to increase; the prev digest carries the chain"
    );
}

#[test]
fn a_signed_document_survives_json_and_still_verifies() {
    let mut n = node();
    let d1 = doc(n.key.id(), 1, None, vec![entry("watch", json!({"n": 1.5}))]);
    let signed = sign(&n.key, d1.clone());
    let text = serde_json::to_string(&signed).expect("serializes");
    let back: SignedDocument = serde_json::from_str(&text).expect("reads back");
    assert_eq!(back, signed);
    assert_eq!(
        n.gate.accept(&back, NOW_MS),
        Ok(Target::Apply(applied(1, &d1)))
    );
}

#[test]
fn the_digest_covers_every_field() {
    let id = NodeKey::generate().id();
    let base = doc(id, 1, None, vec![entry("watch", json!({"a": 1}))]);
    let variants = [
        Document {
            node: NodeKey::generate().id(),
            ..base.clone()
        },
        Document {
            seq: 2,
            ..base.clone()
        },
        Document {
            prev: Some(Digest::of(b"x")),
            ..base.clone()
        },
        Document {
            issued_at: "2026-10-30T12:00:01Z".to_string(),
            ..base.clone()
        },
        Document {
            expires_at: "2026-10-31T12:00:01Z".to_string(),
            ..base.clone()
        },
        Document {
            entries: vec![entry("watch", json!({"a": 2}))],
            ..base.clone()
        },
    ];
    for variant in variants {
        assert_ne!(variant.digest(), base.digest());
    }
    assert_eq!(base.digest(), base.clone().digest(), "the digest is stable");
}

#[test]
fn instant_ms_reads_rfc3339_utc_and_refuses_everything_else() {
    assert_eq!(instant_ms("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(instant_ms("2000-01-01T00:00:00Z"), Some(946_684_800_000));
    assert_eq!(
        instant_ms("2024-02-29T00:00:00Z"),
        Some(1_709_164_800_000),
        "a leap day"
    );
    assert_eq!(instant_ms("2026-10-30T13:00:00Z"), Some(NOW_MS));
    assert_eq!(instant_ms("2026-10-30T13:00:00.250Z"), Some(NOW_MS + 250));
    for bad in [
        "2026-02-29T00:00:00Z",
        "2026-13-01T00:00:00Z",
        "2026-10-30 13:00:00Z",
        "2026-10-30T13:00:00",
        "2026-10-30T13:00:60Z",
        "2026-10-30T13:00:00.Z",
        "2026-10-30T13:00:00.1234567890Z",
        "1969-12-31T23:59:59Z",
        "",
    ] {
        assert_eq!(instant_ms(bad), None, "{bad:?}");
    }
}

/// 2^53, the largest integer up to which every integer is a double, so the JSON text of each integer is exact.
const TWO_POW_53: u64 = 9_007_199_254_740_992;

/// Replaces the one occurrence of `from` in the serialized signed document with `to`, then parses the result. This is how a
/// document tampered in transit reaches the gate: the signature still covers the untouched canonical bytes.
fn tamper_wire(signed: &SignedDocument, from: &str, to: &str) -> SignedDocument {
    let text = serde_json::to_string(signed).expect("serializes");
    assert_eq!(
        text.matches(from).count(),
        1,
        "the text to tamper occurs once: {from}"
    );
    serde_json::from_str(&text.replacen(from, to, 1)).expect("the tampered wire document parses")
}

fn not_canonical() -> DocRefusal {
    DocRefusal::Malformed("not canonical".to_string())
}

#[test]
fn an_integer_that_the_wire_writes_as_a_float_is_refused() {
    // Signed as 5. The wire carries 5.0: the canonical bytes are the same, so the signature still verifies.
    let mut n = node();
    let d1 = doc(n.key.id(), 1, None, vec![entry("watch", json!({"n": 5}))]);
    let tampered = tamper_wire(&sign(&n.key, d1), "\"n\":5}", "\"n\":5.0}");
    assert_eq!(n.gate.accept(&tampered, NOW_MS), Err(not_canonical()));
    assert_eq!(n.gate.applied(), None);
}

#[test]
fn zero_that_the_wire_writes_as_negative_zero_is_refused() {
    let mut n = node();
    let d1 = doc(n.key.id(), 1, None, vec![entry("watch", json!({"n": 0}))]);
    let tampered = tamper_wire(&sign(&n.key, d1), "\"n\":0}", "\"n\":-0.0}");
    assert_eq!(n.gate.accept(&tampered, NOW_MS), Err(not_canonical()));
    assert_eq!(n.gate.applied(), None);
}

#[test]
fn an_integer_above_2_pow_53_on_the_wire_is_refused_when_signed_as_2_pow_53() {
    // Signed as 2^53, whose canonical text is the same as 2^53 + 1's. The wire carries 2^53 + 1.
    let mut n = node();
    let d1 = doc(
        n.key.id(),
        1,
        None,
        vec![entry("watch", json!({"n": TWO_POW_53}))],
    );
    let tampered = tamper_wire(
        &sign(&n.key, d1),
        "\"n\":9007199254740992}",
        "\"n\":9007199254740993}",
    );
    assert_eq!(n.gate.accept(&tampered, NOW_MS), Err(not_canonical()));
    assert_eq!(n.gate.applied(), None);
}

#[test]
fn a_config_integer_above_2_pow_53_is_refused_when_signed() {
    // The canonical text of 2^53 + 1 reads back as 2^53, so the document does not round-trip and is refused.
    let mut n = node();
    let d1 = doc(
        n.key.id(),
        1,
        None,
        vec![entry("watch", json!({"n": TWO_POW_53 + 1}))],
    );
    assert_eq!(
        n.gate.accept(&sign(&n.key, d1), NOW_MS),
        Err(not_canonical())
    );
    assert_eq!(n.gate.applied(), None);
}

#[test]
fn a_config_float_with_an_integer_value_is_refused_when_signed() {
    let mut n = node();
    let d1 = doc(n.key.id(), 1, None, vec![entry("watch", json!({"n": 5.0}))]);
    assert_eq!(
        n.gate.accept(&sign(&n.key, d1), NOW_MS),
        Err(not_canonical())
    );
    assert_eq!(n.gate.applied(), None);
}

#[test]
fn a_seq_above_2_pow_53_is_refused_when_signed() {
    let mut n = node();
    let d1 = doc(
        n.key.id(),
        TWO_POW_53 + 1,
        None,
        vec![entry("watch", json!({}))],
    );
    assert_eq!(
        n.gate.accept(&sign(&n.key, d1), NOW_MS),
        Err(not_canonical())
    );
    assert_eq!(n.gate.applied(), None);
}

#[test]
fn a_seq_at_2_pow_53_relabelled_above_it_on_the_wire_is_refused() {
    let mut n = node();
    let d1 = doc(
        n.key.id(),
        TWO_POW_53,
        None,
        vec![entry("watch", json!({}))],
    );
    let tampered = tamper_wire(
        &sign(&n.key, d1),
        "\"seq\":9007199254740992,",
        "\"seq\":9007199254740993,",
    );
    assert_eq!(n.gate.accept(&tampered, NOW_MS), Err(not_canonical()));
    assert_eq!(n.gate.applied(), None);
}

#[test]
fn integers_at_the_edge_of_the_exact_range_apply() {
    let mut n = node();
    let d1 = doc(
        n.key.id(),
        1,
        None,
        vec![entry("watch", json!({"n": TWO_POW_53, "m": -7, "f": 1.5}))],
    );
    assert_eq!(
        n.gate.accept(&sign(&n.key, d1.clone()), NOW_MS),
        Ok(Target::Apply(applied(1, &d1)))
    );
}

#[test]
fn the_gate_is_for_the_node_whose_key_it_pins() {
    // A gate takes only its signer's key, and its node is that key's id: a document for any other node is foreign even
    // when the pinned key signed it, and the pinned node's own document applies.
    let key = NodeKey::generate();
    let other = NodeKey::generate();
    let mut gate = DesiredStateGate::new(key.public(), None);
    let foreign = doc(other.id(), 1, None, vec![entry("watch", json!({}))]);
    assert_eq!(
        gate.accept(&sign(&key, foreign), NOW_MS),
        Err(DocRefusal::ForeignNode)
    );
    assert_eq!(gate.applied(), None);
    let own = doc(key.id(), 1, None, vec![entry("watch", json!({}))]);
    assert_eq!(
        gate.accept(&sign(&key, own.clone()), NOW_MS),
        Ok(Target::Apply(applied(1, &own)))
    );
}

#[test]
fn an_unknown_field_at_any_level_is_refused() {
    let n = node();
    let signed = sign(
        &n.key,
        doc(n.key.id(), 1, None, vec![entry("watch", json!({}))]),
    );
    let base: Value = serde_json::to_value(&signed).expect("serializes");
    assert!(
        serde_json::from_value::<SignedDocument>(base.clone()).is_ok(),
        "the base document parses"
    );
    let mut top = base.clone();
    top["extra"] = json!(1);
    let mut document = base.clone();
    document["document"]["extra"] = json!(1);
    let mut entry_level = base.clone();
    entry_level["document"]["entries"][0]["extra"] = json!(1);
    for (level, value) in [
        ("signed", top),
        ("document", document),
        ("entry", entry_level),
    ] {
        assert!(
            serde_json::from_value::<SignedDocument>(value).is_err(),
            "an unknown field at the {level} level is refused"
        );
    }
}
