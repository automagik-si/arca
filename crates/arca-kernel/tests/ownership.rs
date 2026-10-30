//! The ownership classifier: an object's owner is read from its own config. Droidd's marks and the kernel's
//! marks are disjoint, and an object marked by both sides is contested, which nobody writes.

use std::collections::BTreeMap;

use arca_kernel::canon::Digest;
use arca_kernel::desired::{EntryId, PluginId};
use arca_kernel::ownership::{
    DROIDD_MARK_PREFIXES, IncusKind, KERNEL_ENTRY_KEY, KERNEL_RETIRING_KEY, ObjectRef, OwnerClass,
    classify, kernel_may_write,
};
use serde_json::json;

type Retained = BTreeMap<EntryId, (PluginId, Digest)>;

fn config(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn none() -> Retained {
    BTreeMap::new()
}

fn retained(entry: &str) -> Retained {
    let mut map = BTreeMap::new();
    map.insert(
        EntryId(entry.to_string()),
        (PluginId("arca.watch".to_string()), Digest::of(b"plan")),
    );
    map
}

fn kernel(entry: &str) -> OwnerClass {
    OwnerClass::Kernel {
        entry: EntryId(entry.to_string()),
    }
}

#[test]
fn the_postgres_template_mark_that_droidd_wrote_is_droidd_owned() {
    // A template mark that droidd writes. The kernel must never touch an object carrying it.
    let c = config(&[("user.arca.template", "postgres-autopg")]);
    assert_eq!(classify(&c, &none()), OwnerClass::Droidd);
}

#[test]
fn droidd_lease_and_tactu_marks_are_droidd_owned() {
    assert_eq!(
        classify(&config(&[("user.droid.lease", "x")]), &none()),
        OwnerClass::Droidd
    );
    assert_eq!(
        classify(&config(&[("user.tactu.desktop", "x")]), &none()),
        OwnerClass::Droidd
    );
}

#[test]
fn a_bare_droidd_namespace_key_is_droidd_owned() {
    assert_eq!(
        classify(&config(&[("user.arca", "x")]), &none()),
        OwnerClass::Droidd
    );
}

#[test]
fn droidd_namespace_matching_ignores_ascii_case_so_a_near_miss_only_refuses() {
    assert_eq!(
        classify(&config(&[("User.Arca.Template", "x")]), &none()),
        OwnerClass::Droidd
    );
    assert_eq!(
        classify(&config(&[("USER.DROID.LEASE", "x")]), &none()),
        OwnerClass::Droidd
    );
}

#[test]
fn the_kernel_mark_is_not_read_as_a_droidd_namespace_key() {
    // "user.arcakernel.entry" starts with "user.arca", but not with "user.arca.".
    let c = config(&[(KERNEL_ENTRY_KEY, "watch")]);
    assert_eq!(classify(&c, &none()), kernel("watch"));
}

#[test]
fn a_kernel_entry_mark_is_kernel_owned_by_that_entry() {
    let c = config(&[(KERNEL_ENTRY_KEY, "watch")]);
    assert_eq!(classify(&c, &none()), kernel("watch"));
}

#[test]
fn a_kernel_entry_mark_for_a_retained_entry_is_retained() {
    let c = config(&[(KERNEL_ENTRY_KEY, "watch")]);
    assert_eq!(
        classify(&c, &retained("watch")),
        OwnerClass::Retained {
            entry: EntryId("watch".to_string()),
            plugin: PluginId("arca.watch".to_string()),
            plan: Digest::of(b"plan"),
        }
    );
}

#[test]
fn a_retained_map_for_another_entry_does_not_retain_this_one() {
    let c = config(&[(KERNEL_ENTRY_KEY, "watch")]);
    assert_eq!(classify(&c, &retained("other")), kernel("watch"));
}

#[test]
fn a_kernel_entry_with_a_droidd_mark_is_contested_retained_or_not() {
    let c = config(&[(KERNEL_ENTRY_KEY, "watch"), ("user.arca.template", "x")]);
    assert_eq!(classify(&c, &none()), OwnerClass::Contested);
    assert_eq!(classify(&c, &retained("watch")), OwnerClass::Contested);
}

#[test]
fn a_kernel_tombstone_with_a_droidd_mark_is_contested() {
    let c = config(&[(KERNEL_RETIRING_KEY, "plan"), ("user.droid.lease", "x")]);
    assert_eq!(classify(&c, &none()), OwnerClass::Contested);
}

#[test]
fn the_retiring_tombstone_beside_its_entry_is_still_kernel_owned() {
    // A destructive inverse sets the tombstone on the kernel's own object before it deletes it.
    let c = config(&[(KERNEL_ENTRY_KEY, "watch"), (KERNEL_RETIRING_KEY, "plan")]);
    assert_eq!(classify(&c, &none()), kernel("watch"));
}

#[test]
fn an_object_with_no_mark_is_foreign() {
    assert_eq!(classify(&config(&[]), &none()), OwnerClass::Foreign);
    assert_eq!(
        classify(
            &config(&[("user.something", "x"), ("limits.cpu", "2")]),
            &none()
        ),
        OwnerClass::Foreign
    );
}

#[test]
fn a_kernel_namespace_key_that_is_not_the_entry_key_is_foreign() {
    let c = config(&[
        ("user.arcakernel.entryx", "watch"),
        ("user.arcakernel.other", "x"),
    ]);
    assert_eq!(classify(&c, &none()), OwnerClass::Foreign);
}

#[test]
fn a_kernel_entry_mark_whose_value_is_not_an_entry_id_is_foreign() {
    let c = config(&[(KERNEL_ENTRY_KEY, "Not A Name")]);
    assert_eq!(classify(&c, &none()), OwnerClass::Foreign);
}

#[test]
fn a_retiring_tombstone_without_an_entry_is_foreign() {
    let c = config(&[(KERNEL_RETIRING_KEY, "plan")]);
    assert_eq!(classify(&c, &none()), OwnerClass::Foreign);
}

#[test]
fn a_mark_that_only_a_profile_carries_does_not_class_the_object() {
    // The caller passes the object's own config. An expanded config, which merges profile keys, is never passed here,
    // so a mark that exists only in a profile leaves the object foreign.
    let own = config(&[]);
    let expanded = config(&[(KERNEL_ENTRY_KEY, "watch")]);
    assert_eq!(classify(&own, &none()), OwnerClass::Foreign);
    assert_eq!(
        classify(&expanded, &none()),
        kernel("watch"),
        "only the caller decides which config it passes"
    );
}

#[test]
fn the_kernel_may_write_only_an_object_of_its_own_entry() {
    let watch = EntryId("watch".to_string());
    let other = EntryId("other".to_string());
    assert!(kernel_may_write(&kernel("watch"), &watch));
    assert!(
        !kernel_may_write(&kernel("watch"), &other),
        "another entry's object"
    );
    let classes = [
        OwnerClass::Droidd,
        OwnerClass::Foreign,
        OwnerClass::Contested,
        OwnerClass::Retained {
            entry: watch.clone(),
            plugin: PluginId("arca.watch".to_string()),
            plan: Digest::of(b"plan"),
        },
    ];
    for class in &classes {
        assert!(
            !kernel_may_write(class, &watch),
            "{class:?} is never written by the kernel"
        );
    }
}

#[test]
fn an_owner_class_serializes_with_a_lowercase_class_tag() {
    assert_eq!(
        serde_json::to_value(kernel("watch")).expect("serializes"),
        json!({"class": "kernel", "entry": "watch"})
    );
    assert_eq!(
        serde_json::to_value(OwnerClass::Contested).expect("serializes"),
        json!({"class": "contested"})
    );
    assert_eq!(
        serde_json::to_value(OwnerClass::Droidd).expect("serializes"),
        json!({"class": "droidd"})
    );
    let back: OwnerClass = serde_json::from_value(json!({"class": "foreign"})).expect("reads back");
    assert_eq!(back, OwnerClass::Foreign);
}

#[test]
fn incus_kinds_serialize_lowercase_and_object_refs_keep_their_names() {
    assert_eq!(
        serde_json::to_value(IncusKind::Instance).expect("serializes"),
        json!("instance")
    );
    assert_eq!(
        serde_json::to_value(IncusKind::Acl).expect("serializes"),
        json!("acl")
    );
    let object = ObjectRef {
        parent: None,
        kind: IncusKind::Project,
        project: "arcak-scratch-p3".to_string(),
        name: "arcak-scratch-p3".to_string(),
        uuid: None,
    };
    assert_eq!(
        serde_json::to_value(&object).expect("serializes"),
        json!({"kind": "project", "project": "arcak-scratch-p3", "name": "arcak-scratch-p3", "uuid": null})
    );
}

#[test]
fn the_mark_keys_and_droidd_prefixes_are_the_decided_ones() {
    assert_eq!(KERNEL_ENTRY_KEY, "user.arcakernel.entry");
    assert_eq!(KERNEL_RETIRING_KEY, "user.arcakernel.retiring");
    assert_eq!(
        DROIDD_MARK_PREFIXES,
        ["user.droid.", "user.tactu.", "user.arca."]
    );
}
