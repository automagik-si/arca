//! Mount safety: a removal never goes into a mount point, even one on the same filesystem (a bind mount) or one the
//! run's mount table does not list, and a refused item deletes nothing.
//!
//! The cases need a user and mount namespace (`unshare -rm`): each one runs this binary again inside a new
//! namespace. The file has its own small harness (`harness = false`) so that where namespaces are not available the
//! cases are reported as ignored, never as passed.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use arca_janitor::platform;
use arca_janitor::testutil::TempDir;
use arca_janitor::walk::remove_tree;

const NS_DIR: &str = "JANITOR_TEST_NS_DIR";

struct Case {
    name: &'static str,
    /// Outside the namespace, before it starts.
    setup: fn(&Path),
    /// Inside the namespace.
    inside: fn(&Path),
    /// Outside the namespace, after it ended.
    after: fn(&Path),
}

const CASES: &[Case] = &[
    Case {
        name: "a_removal_stops_at_a_bind_mount_on_the_same_filesystem_and_deletes_nothing",
        setup: |dir| {
            fs::create_dir_all(dir.join("elsewhere")).unwrap();
            fs::write(dir.join("elsewhere/precious"), b"keep").unwrap();
        },
        inside: |dir| {
            let (tree, inner) = tree_with_mount(dir);
            let source = dir.join("elsewhere");
            mount(&["--bind", source.to_str().unwrap(), inner.to_str().unwrap()]);
            let mounts = platform::mount_points();
            assert!(mounts.contains(&inner), "the bind mount is listed");
            let e = remove_tree(&tree, &mounts, None).expect_err("refused");
            assert!(e.to_string().contains("mount point"), "{e}");
            assert!(
                inner.join("precious").exists(),
                "the bind source is untouched"
            );
            assert!(tree.join("a/b/sibling").exists(), "nothing was removed");
        },
        after: |dir| {
            assert!(dir.join("elsewhere/precious").exists());
            assert!(dir.join("tree/a/b/sibling").exists());
        },
    },
    Case {
        name: "a_removal_stops_at_another_filesystem_the_mount_table_does_not_list",
        setup: |_| {},
        inside: |dir| {
            let (tree, inner) = tree_with_mount(dir);
            mount(&["-t", "tmpfs", "none", inner.to_str().unwrap()]);
            fs::write(inner.join("on-tmpfs"), b"keep").unwrap();
            // A table read before the mount appeared: only the device number shows it.
            let e = remove_tree(&tree, &HashSet::new(), None).expect_err("refused");
            assert!(e.to_string().contains("another filesystem"), "{e}");
            assert!(inner.join("on-tmpfs").exists());
            assert!(tree.join("a/b/sibling").exists(), "nothing was removed");
        },
        after: |dir| {
            assert!(dir.join("tree/a/b/sibling").exists());
        },
    },
];

fn mount(args: &[&str]) {
    let out = Command::new("mount").args(args).output().unwrap();
    assert!(
        out.status.success(),
        "mount {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn tree_with_mount(dir: &Path) -> (PathBuf, PathBuf) {
    let tree = dir.join("tree");
    fs::create_dir_all(tree.join("inner")).unwrap();
    fs::create_dir_all(tree.join("a/b")).unwrap();
    fs::write(tree.join("a/b/sibling"), b"x").unwrap();
    (tree.clone(), tree.join("inner"))
}

fn namespaces_work() -> bool {
    Command::new("unshare")
        .args(["-rm", "true"])
        .output()
        .is_ok_and(|o| o.status.success())
}

fn run(case: &Case) {
    let t = TempDir::new("mounts");
    (case.setup)(t.path());
    let out = Command::new("unshare")
        .arg("-rm")
        .arg(std::env::current_exe().unwrap())
        .arg(case.name)
        .env(NS_DIR, t.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "inside the namespace:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (case.after)(t.path());
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Ok(dir) = std::env::var(NS_DIR) {
        let name = args.first().map(String::as_str);
        let case = CASES
            .iter()
            .find(|c| Some(c.name) == name)
            .expect("a case name");
        (case.inside)(Path::new(&dir));
        return;
    }

    // The libtest arguments cargo passes on: name filters, --exact, --list, --skip; the rest is ignored.
    let mut filters = Vec::new();
    let mut skips = Vec::new();
    let (mut exact, mut list) = (false, false);
    let mut rest = args.iter();
    while let Some(a) = rest.next() {
        match a.as_str() {
            "--exact" => exact = true,
            "--list" => list = true,
            "--skip" => skips.extend(rest.next().cloned()),
            "--test-threads" | "--format" | "--color" | "--logfile" | "-Z" => {
                rest.next();
            }
            a if a.starts_with('-') => {}
            a => filters.push(a.to_string()),
        }
    }
    let matches = |name: &str, pattern: &str| {
        if exact {
            name == pattern
        } else {
            name.contains(pattern)
        }
    };
    let selected: Vec<&Case> = CASES
        .iter()
        .filter(|c| filters.is_empty() || filters.iter().any(|f| matches(c.name, f)))
        .filter(|c| !skips.iter().any(|s| matches(c.name, s)))
        .collect();
    let filtered = CASES.len() - selected.len();
    if list {
        for c in &selected {
            println!("{}: test", c.name);
        }
        return;
    }

    let start = Instant::now();
    let ns = namespaces_work();
    println!("\nrunning {} tests", selected.len());
    let (mut passed, mut ignored) = (0, 0);
    let mut failed = Vec::new();
    for c in &selected {
        if !ns {
            println!(
                "test {} ... ignored, needs user namespaces (unshare -rm)",
                c.name
            );
            ignored += 1;
            continue;
        }
        if std::panic::catch_unwind(|| run(c)).is_ok() {
            println!("test {} ... ok", c.name);
            passed += 1;
        } else {
            println!("test {} ... FAILED", c.name);
            failed.push(c.name);
        }
    }
    if !failed.is_empty() {
        println!("\nfailures:");
        for name in &failed {
            println!("    {name}");
        }
    }
    println!(
        "\ntest result: {}. {passed} passed; {} failed; {ignored} ignored; 0 measured; {filtered} filtered out; \
         finished in {:.2}s\n",
        if failed.is_empty() { "ok" } else { "FAILED" },
        failed.len(),
        start.elapsed().as_secs_f64()
    );
    if !failed.is_empty() {
        std::process::exit(101);
    }
}
