//! The desired-state store: one signed document per proposal, each continuing the last, kept in the state directory under a lock
//! and refused at open when the file on disk is not this node's own signed state.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};

use arca::desired_store::{DesiredStore, EntryChange};
use arca_kernel::KernelError;
use arca_kernel::canon::Digest;
use arca_kernel::desired::{Applied, DesiredStateGate, DocRefusal, EntryId, PluginId, Target};
use common::*;
use serde_json::json;

#[test]
fn a_second_open_of_a_state_dir_is_refused_locked_while_the_first_holds_it() {
    let scratch = Scratch::new("locked");
    let first = DesiredStore::open(&scratch.dir, node_key_at(&scratch.dir)).expect("opens");
    let second = DesiredStore::open(&scratch.dir, node_key_at(&scratch.dir));
    assert!(
        matches!(second, Err(KernelError::Locked { .. })),
        "a second store is refused while the first holds the lock"
    );
    drop(first);
    DesiredStore::open(&scratch.dir, node_key_at(&scratch.dir))
        .expect("the lock is free once the first store is gone");
}

#[test]
fn the_first_proposal_is_seq_one_with_no_previous_digest_and_the_gate_accepts_it() {
    let scratch = Scratch::new("first");
    let key = node_key_at(&scratch.dir);
    let public = key.public();
    let mut store = DesiredStore::open(&scratch.dir, key).expect("opens");
    let signed = store
        .propose(vec![EntryChange::Put(toy_entry("t1"))], NOW)
        .expect("the first proposal is made");
    assert_eq!(signed.document.seq, 1);
    assert_eq!(signed.document.prev, None);
    assert_eq!(signed.document.entries, vec![toy_entry("t1")]);
    let mut gate = DesiredStateGate::new(public, None);
    assert!(matches!(gate.accept(&signed, NOW), Ok(Target::Apply(_))));
}

#[test]
fn each_proposal_continues_the_chain_and_the_gate_accepts_it_from_the_last_applied() {
    let scratch = Scratch::new("chain");
    let key = node_key_at(&scratch.dir);
    let public = key.public();
    let mut store = DesiredStore::open(&scratch.dir, key).expect("opens");
    let first = store
        .propose(vec![EntryChange::Put(toy_entry("t1"))], NOW)
        .expect("the first proposal");
    let second = store
        .propose(vec![EntryChange::Put(second_toy_entry("t2"))], NOW + 1_000)
        .expect("the second proposal");
    assert_eq!(second.document.seq, 2);
    assert_eq!(second.document.prev, Some(first.document.digest()));
    assert_eq!(
        second.document.entries,
        vec![toy_entry("t1"), second_toy_entry("t2")],
        "a put adds to the entries the last document held"
    );
    let applied = Applied {
        seq: 1,
        digest: first.document.digest(),
    };
    let mut gate = DesiredStateGate::new(public, Some(applied));
    assert!(matches!(gate.accept(&second, NOW), Ok(Target::Apply(_))));
    assert_eq!(store.current(), Some(&second));
}

#[test]
fn a_change_list_that_names_one_entry_twice_is_refused_and_persists_nothing() {
    let scratch = Scratch::new("dup");
    let mut store = DesiredStore::open(&scratch.dir, node_key_at(&scratch.dir)).expect("opens");
    let refused = store.propose(
        vec![
            EntryChange::Put(toy_entry("t1")),
            EntryChange::Remove(EntryId("toy-scratch".to_string())),
        ],
        NOW,
    );
    assert!(
        matches!(
            refused,
            Err(KernelError::Refused(DocRefusal::DuplicateEntry(_)))
        ),
        "{refused:?}"
    );
    assert!(store.current().is_none(), "nothing is held");
    drop(store);
    let reopened = DesiredStore::open(&scratch.dir, node_key_at(&scratch.dir)).expect("opens");
    assert!(reopened.current().is_none(), "nothing is persisted");
}

#[test]
fn a_removal_of_an_entry_the_document_does_not_hold_refuses_the_whole_change_list() {
    // Both or neither: the put that comes first must not land when the removal after it is refused.
    let scratch = Scratch::new("both");
    let mut store = DesiredStore::open(&scratch.dir, node_key_at(&scratch.dir)).expect("opens");
    store
        .propose(vec![EntryChange::Put(toy_entry("t1"))], NOW)
        .expect("the first proposal");
    let refused = store.propose(
        vec![
            EntryChange::Put(second_toy_entry("t2")),
            EntryChange::Remove(EntryId("never-held".to_string())),
        ],
        NOW + 1_000,
    );
    assert!(
        matches!(refused, Err(KernelError::Refused(DocRefusal::Malformed(_)))),
        "{refused:?}"
    );
    assert_eq!(store.current().expect("seq 1 held").document.seq, 1);
    drop(store);
    let reopened = DesiredStore::open(&scratch.dir, node_key_at(&scratch.dir)).expect("opens");
    assert_eq!(
        reopened
            .current()
            .expect("seq 1 persisted")
            .document
            .entries,
        vec![toy_entry("t1")],
        "the refused put did not land"
    );
}

#[test]
fn a_non_canonical_entry_config_is_refused_by_the_gate_and_persists_nothing() {
    // `5.0` writes as `5` in canonical JSON, so the document would not be its own canonical form.
    let scratch = Scratch::new("canon");
    let mut store = DesiredStore::open(&scratch.dir, node_key_at(&scratch.dir)).expect("opens");
    let refused = store.propose(
        vec![EntryChange::Put(entry(
            "toy-scratch",
            "toy.scratch",
            json!({ "tag": 5.0 }),
        ))],
        NOW,
    );
    assert!(
        matches!(refused, Err(KernelError::Refused(DocRefusal::Malformed(_)))),
        "{refused:?}"
    );
    assert!(store.current().is_none());
}

#[test]
fn a_state_file_signed_by_another_node_is_refused_at_open() {
    let mine = Scratch::new("mine");
    let theirs = Scratch::new("theirs");
    let mut other = DesiredStore::open(&theirs.dir, node_key_at(&theirs.dir)).expect("opens");
    other
        .propose(vec![EntryChange::Put(toy_entry("t1"))], NOW)
        .expect("the other node proposes");
    drop(other);
    fs::copy(theirs.path("desired.json"), mine.path("desired.json")).expect("the file is copied");
    fs::set_permissions(mine.path("desired.json"), fs::Permissions::from_mode(0o600))
        .expect("mode 0600");
    let refused = DesiredStore::open(&mine.dir, node_key_at(&mine.dir));
    assert!(
        matches!(refused, Err(KernelError::Refused(_))),
        "a document of another node is refused: {refused:?}"
    );
}

#[test]
fn a_state_file_with_a_group_or_other_bit_is_refused_at_open() {
    let scratch = Scratch::new("mode");
    let mut store = DesiredStore::open(&scratch.dir, node_key_at(&scratch.dir)).expect("opens");
    store
        .propose(vec![EntryChange::Put(toy_entry("t1"))], NOW)
        .expect("the proposal");
    drop(store);
    fs::set_permissions(
        scratch.path("desired.json"),
        fs::Permissions::from_mode(0o640),
    )
    .expect("mode 0640");
    let refused = DesiredStore::open(&scratch.dir, node_key_at(&scratch.dir));
    assert!(
        matches!(refused, Err(KernelError::Io { .. })),
        "a group bit is refused: {refused:?}"
    );
}

#[test]
fn the_state_file_and_the_lock_are_private_to_the_owner() {
    let scratch = Scratch::new("private");
    let mut store = DesiredStore::open(&scratch.dir, node_key_at(&scratch.dir)).expect("opens");
    store
        .propose(vec![EntryChange::Put(toy_entry("t1"))], NOW)
        .expect("the proposal");
    for name in ["desired.json", "desired.lock"] {
        let mode = fs::metadata(scratch.path(name))
            .expect("the file exists")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "{name} is mode 0600");
    }
}

#[test]
fn the_converged_set_and_the_retained_set_survive_a_reopen() {
    let scratch = Scratch::new("kept");
    let mut store = DesiredStore::open(&scratch.dir, node_key_at(&scratch.dir)).expect("opens");
    let plan = Digest::of(b"a declined plan");
    let mut retained = BTreeMap::new();
    retained.insert(
        EntryId("old-scratch".to_string()),
        (PluginId("toy.scratch".to_string()), plan.clone()),
    );
    store
        .save_applied(vec![toy_entry("t1")], retained.clone())
        .expect("the applied state is saved");
    drop(store);
    let reopened = DesiredStore::open(&scratch.dir, node_key_at(&scratch.dir)).expect("opens");
    assert_eq!(reopened.converged(), &[toy_entry("t1")][..]);
    assert_eq!(reopened.retained(), &retained);
    assert!(reopened.current().is_none(), "no document was proposed");
}

#[test]
fn a_lock_that_is_a_link_to_a_private_file_is_refused_and_the_target_is_left_alone() {
    // Opening the lock must not follow the link: a chmod on the target would change a file the store does not own.
    let scratch = Scratch::new("lock-link");
    let victim = scratch.path("victim.txt");
    fs::write(&victim, "keep").expect("the target is written");
    fs::set_permissions(&victim, fs::Permissions::from_mode(0o644)).expect("mode 0644");
    symlink(&victim, scratch.path("desired.lock")).expect("the link is made");
    let refused = DesiredStore::open(&scratch.dir, node_key_at(&scratch.dir));
    assert!(
        matches!(refused, Err(KernelError::Io { .. })),
        "a link is refused: {refused:?}"
    );
    assert_eq!(
        fs::read_to_string(&victim).expect("the target reads"),
        "keep"
    );
    assert_eq!(
        fs::metadata(&victim)
            .expect("the target exists")
            .permissions()
            .mode()
            & 0o777,
        0o644,
        "the target keeps its mode"
    );
}

#[test]
fn a_dangling_lock_link_is_refused_and_creates_no_target() {
    let scratch = Scratch::new("lock-dangling");
    let target = scratch.path("created-by-lock");
    symlink(&target, scratch.path("desired.lock")).expect("the link is made");
    let refused = DesiredStore::open(&scratch.dir, node_key_at(&scratch.dir));
    assert!(
        matches!(refused, Err(KernelError::Io { .. })),
        "a dangling link is refused: {refused:?}"
    );
    assert!(!target.exists(), "opening the lock creates no target");
}
