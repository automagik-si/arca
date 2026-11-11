//! The golden request log of the services suite. Every rig of this suite records, when its test ends, a digest of each
//! request its fake Incus received (method, path and status, in order) and of each write (the same, with the names of the
//! config keys it carried). `tests/golden/services-requests.txt` holds the digest of every rig whose log is the same on
//! every run, as the suite sent it before a node could hold tenants. No rig of this suite sets `kernel.tenants`, so a
//! digest that differs means a node without the key no longer sends the requests it did.
//!
//! A rig whose log depends on timing (a poll, a held operation, two calls at once) is not in the file and is not compared.
//! `ARCA_GOLDEN_OUT=<file>` appends each rig's line to that file instead of comparing; the golden file is the lines that
//! were equal over several such runs.
use super::*;
use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::LazyLock;

const GOLDEN: &str = include_str!("../golden/services-requests.txt");

static EXPECTED: LazyLock<BTreeMap<&'static str, &'static str>> = LazyLock::new(|| {
    GOLDEN
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
        .filter_map(|line| line.split_once(' '))
        .collect()
});

thread_local! {
    /// How many rigs this test's thread has recorded, so a test with several rigs names each.
    static RIGS: Cell<usize> = const { Cell::new(0) };
}

fn digest(fake: &FakeIncus) -> String {
    let mut text = String::new();
    for request in fake.requests() {
        text.push_str(&format!(
            "{} {} {}\n",
            request.method, request.path, request.status
        ));
    }
    for write in fake.writes() {
        text.push_str(&format!(
            "w {} {} {} {}\n",
            write.method,
            write.path,
            write.status,
            write.config_keys.join(",")
        ));
    }
    Digest::of(text.as_bytes()).to_hex()
}

/// Records one rig's request log as its test ends. Called from the rig's `Drop`, and silent while the test is failing.
pub(super) fn record(fake: &FakeIncus) {
    if std::thread::panicking() {
        return;
    }
    // A test's thread carries the test's name. A rig made on any other thread is not one test's whole log.
    let thread = std::thread::current();
    let Some(test) = thread.name().filter(|name| *name != "main") else {
        return;
    };
    let index = RIGS.with(|rigs| {
        let index = rigs.get();
        rigs.set(index + 1);
        index
    });
    let key = format!("{test}#{index}");
    let found = digest(fake);
    if let Some(out) = std::env::var_os("ARCA_GOLDEN_OUT") {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(out)
            .expect("the golden output file opens");
        writeln!(file, "{key} {found}").expect("the golden line is written");
        return;
    }
    if let Some(expected) = EXPECTED.get(key.as_str()) {
        assert_eq!(
            found, *expected,
            "{key}: a node without kernel.tenants sends other Incus requests than the golden log holds"
        );
    }
}

#[test]
fn the_golden_log_covers_the_suite() {
    // The file is the suite's own log: it names most of its rigs, and each line is a rig key and a sha256.
    assert!(EXPECTED.len() >= 100, "{} golden rigs", EXPECTED.len());
    for (key, digest) in EXPECTED.iter() {
        assert!(key.contains('#'), "{key}");
        assert!(
            digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
            "{key}"
        );
    }
    // Removal, creation, links and virtual machines are each in it.
    for test in [
        "service_remove_deletes_the_instance_and_only_then_its_acl#0",
        "service_remove_is_refused_without_a_valid_approval_and_removes_with_one#0",
        "creates_a_service_in_the_service_project_only_on_its_one_profile_and_nic_marked_refuses_an_unmarked_one_or_a_nic_without_its_acl#0",
    ] {
        assert!(EXPECTED.contains_key(test), "{test} is in the golden log");
    }
}
