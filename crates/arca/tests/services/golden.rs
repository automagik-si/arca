//! The golden request log of the services suite. Every rig of this suite records, when its test ends, a digest of each
//! request its fake Incus received (method, path and status, in order) and of each write (the same, with the names of the
//! config keys it carried). `tests/golden/services-requests.txt` holds the digest of every rig whose log is the same on
//! every run, as the suite sent it before a node could hold tenants. No rig of this suite sets `kernel.tenants`, so a
//! digest that differs means a node without the key no longer sends the requests it did.
//!
//! A rig whose log depends on timing (a poll, a held operation, two calls at once) is not in the file and is not compared.
//! `ARCA_GOLDEN_OUT=<file>` appends each rig's line to that file instead of comparing; the golden file is the lines that
//! were equal over several such runs.
//!
//! Equal runs on one fast machine do not show that a log is free of timing. The fake writes a request down when it
//! answers it, not when it arrives, and it holds a wait on a running operation for up to a quarter of a second. So a
//! wait on a held operation is in the log late, and once more for each quarter of a second the operation stays held: a
//! machine slow enough has another log. A rig with such a wait belongs in the file only when its test fixes where that
//! line lands, as `g5_hung_mark_allows_key_cleanup_and_stop_and_clears_on_uuid_change` does, or when it is one of
//! [`WITHOUT_WAITS`], whose logs are compared with their waits left out.
//!
//! A digest names a log and does not show it. `ARCA_GOLDEN_DUMP=<directory>` writes the text each digest is made from,
//! one file for each rig and digest, so two runs that differ leave two files to compare. A rig that fails the comparison
//! prints the same text.
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

/// The rigs whose log is compared without its waits on an operation. In each, one call waits on a held operation while
/// the test makes other calls and then lets the operation go. The fake answers such a wait after a quarter of a second
/// and the node asks again, so how many waits the log holds, and between which of the test's own requests, is how long
/// the test took and nothing the node decides. No order can be forced on them. Every other request of these rigs is
/// sent in one order and stays compared, and every other rig is compared whole.
const WITHOUT_WAITS: [&str; 2] = [
    "wiring::g5_publication_preserves_secret_updates_made_during_its_exec#0",
    "wiring::g5_bad_removal_approval_runs_no_exec_and_other_verbs_answer_during_logout#0",
];

thread_local! {
    /// How many rigs this test's thread has recorded, so a test with several rigs names each.
    static RIGS: Cell<usize> = const { Cell::new(0) };
}

/// Whether a request is a wait on an operation.
fn is_wait(request: &arca_incus::RecordedRequest) -> bool {
    request.method == "GET"
        && request.path.starts_with("/1.0/operations/")
        && request.path.contains("/wait?")
}

/// The text a rig's digest is made from: each request in the order the fake recorded it, then each write. With `waits`
/// false the waits on an operation are left out.
fn log_text(fake: &FakeIncus, waits: bool) -> String {
    let mut text = String::new();
    for request in fake.requests() {
        if !waits && is_wait(&request) {
            continue;
        }
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
    text
}

/// Writes one rig's log text into the dump directory, named by its rig and by its digest.
fn dump(directory: &std::ffi::OsStr, key: &str, found: &str, text: &str) {
    let directory = std::path::Path::new(directory);
    fs::create_dir_all(directory).expect("the golden dump directory exists");
    let name = format!("{}.{}.txt", key.replace("::", "."), &found[..12]);
    fs::write(directory.join(name), text).expect("the golden dump is written");
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
    let text = log_text(fake, !WITHOUT_WAITS.contains(&key.as_str()));
    let found = Digest::of(text.as_bytes()).to_hex();
    if let Some(directory) = std::env::var_os("ARCA_GOLDEN_DUMP") {
        dump(&directory, &key, &found, &text);
    }
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
            "{key}: a node without kernel.tenants sends other Incus requests than the golden log holds. It sent:\n{text}"
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
