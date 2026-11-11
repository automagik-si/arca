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
fn this_checkout_s_changelog_has_the_heading_the_release_writes_under() {
    let text = fs::read_to_string(Path::new(REPO).join("CHANGELOG.md")).expect("CHANGELOG.md");
    assert_eq!(
        text.lines().filter(|line| *line == "## Unreleased").count(),
        1
    );
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

    scratch.answer(&["changelog", "0.8.1", "2026-11-13"]);
    assert_eq!(
        scratch.read("CHANGELOG.md"),
        "# Changelog\n\n## Unreleased\n\n## 0.8.1 (2026-11-13)\n\n\
         - feat(a): uses `touch pwned-1` and $(touch pwned-2) and ${HOME} and ; touch pwned-3\n\
         - [31m## after controls\n\
         - \\## 0.9.9 a heading\n\
         - fix(a): one## 9.9.9 (2026-01-01)A forged section\n"
    );
    // One heading for the release and no other, no control character, and nothing was run.
    let text = scratch.read("CHANGELOG.md");
    assert_eq!(text.lines().filter(|line| line.starts_with('#')).count(), 3);
    assert!(!text.chars().any(|c| c.is_control() && c != '\n'));
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
            "scripts/",
            "cargo",
            "rustup",
            "npm",
            "npx",
            "node",
            "apt-get",
            "curl",
            "wget",
            "eval",
            "source ",
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
    let publish = job(&rules, "publish");
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
    assert!(publish.contains("sha256sum --check --strict --quiet SHA256SUMS"));
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

/// A stand-in for the two machines of a release: a bare repository as the remote, clones of it for the jobs, and a
/// directory whose `gh` answers `release list` from `releases.txt` and writes each `release create` to `created.log`.
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

impl Lab {
    /// A remote whose dev holds the version script, a manifest, a changelog and one source file, tagged as the last
    /// release of the old numbering, which is published.
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
        for (path, content) in [
            ("scripts/version.sh", script.as_str()),
            (
                "Cargo.toml",
                "[workspace.package]\nversion = \"0.1.0-alpha.7\"\n",
            ),
            ("CHANGELOG.md", "# Changelog\n\n## Unreleased\n"),
            ("crates/a/src/lib.rs", "// a\n"),
        ] {
            let file = seed.join(path);
            fs::create_dir_all(file.parent().expect("a parent")).expect("a directory");
            fs::write(file, content).expect("write");
        }
        fs::set_permissions(
            seed.join("scripts/version.sh"),
            fs::Permissions::from_mode(0o755),
        )
        .expect("mode");
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
        let file = other.join(path);
        fs::create_dir_all(file.parent().expect("a parent")).expect("a directory");
        fs::write(file, format!("{subject}\n")).expect("write");
        git_at(&other, &["add", "-A"]);
        git_at(&other, &["commit", "-q", "-m", subject]);
        git_at(&other, &["push", "-q", "origin", "dev"]);
    }

    /// What the cutting job hands over for `version`, cut from the remote's dev as it is now: the release commit and
    /// its tag as a bundle, the binary, its checksum and the notes. `twist` names one thing done wrong.
    fn hand_over(&self, version: &str, twist: &str) -> PathBuf {
        let cut = self.clone_as("cut");
        let tag = format!("v{version}");
        if twist == "an extra commit" {
            fs::write(cut.join("crates/a/src/lib.rs"), "// slipped in\n").expect("write");
            git_at(&cut, &["commit", "-q", "-am", "feat(a): slipped in"]);
        }
        fs::write(
            cut.join("Cargo.toml"),
            format!("[workspace.package]\nversion = \"{version}\"\n"),
        )
        .expect("write");
        fs::write(
            cut.join("CHANGELOG.md"),
            format!("# Changelog\n\n## Unreleased\n\n## {version} (2026-11-12)\n\n- a change\n"),
        )
        .expect("write");
        if twist == "another file" {
            fs::write(cut.join("crates/a/src/lib.rs"), "// slipped in\n").expect("write");
        }
        let subject = if twist == "another subject" {
            "feat(a): not a release commit".to_string()
        } else {
            format!("chore(release): {tag}")
        };
        git_at(&cut, &["commit", "-q", "-am", &subject]);
        if twist == "a plain tag" {
            git_at(&cut, &["tag", &tag]);
        } else {
            git_at(&cut, &["tag", "-a", &tag, "-m", "Automagik Arca"]);
        }
        let handover = self.dir().join("handover");
        let _ = fs::remove_dir_all(&handover);
        fs::create_dir_all(&handover).expect("the handover directory");
        let bundle = handover.join("release.bundle");
        let prerequisite = if twist == "an extra commit" {
            "^HEAD~2"
        } else {
            "^HEAD~1"
        };
        git_at(
            &cut,
            &[
                "bundle",
                "create",
                "-q",
                bundle.to_str().expect("a path"),
                &format!("refs/tags/{tag}"),
                prerequisite,
            ],
        );
        self.files(&handover, version, twist);
        handover
    }

    /// The three release files, as scripts/release.sh leaves them.
    fn files(&self, handover: &Path, version: &str, twist: &str) {
        fs::create_dir_all(handover).expect("the handover directory");
        let asset = format!("arca-{version}-linux-x86_64");
        fs::write(handover.join(&asset), "a binary\n").expect("write");
        let sums = Command::new("sha256sum")
            .arg(&asset)
            .current_dir(handover)
            .output()
            .expect("run sha256sum");
        fs::write(handover.join("SHA256SUMS"), sums.stdout).expect("write");
        fs::write(handover.join("RELEASE-NOTES.md"), "- a change\n").expect("write");
        match twist {
            "another binary" => {
                fs::write(handover.join(&asset), "another binary\n").expect("write")
            }
            "an extra file" => fs::write(handover.join("install.sh"), "echo\n").expect("write"),
            "a second checksum" => {
                let mut text = fs::read_to_string(handover.join("SHA256SUMS")).expect("read");
                text.push_str(&text.clone().replace(&asset, "RELEASE-NOTES.md"));
                fs::write(handover.join("SHA256SUMS"), text).expect("write");
            }
            "a link" => {
                fs::remove_file(handover.join("RELEASE-NOTES.md")).expect("remove");
                std::os::unix::fs::symlink("/etc/hostname", handover.join("RELEASE-NOTES.md"))
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
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .env_remove("ARCA_VERSION_ROOT")
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

    /// The publishing job's check of what was handed over, in a fresh checkout of the remote's dev.
    fn check(&self, mode: &str, version: &str, handover: &Path) -> (PathBuf, Ran) {
        let publish = self.clone_as("publish");
        let ran = self.step(
            "Check what was handed over",
            &publish,
            &[
                ("MODE", mode),
                ("VERSION", version),
                ("HANDOVER", handover.to_str().expect("a path")),
            ],
        );
        (publish, ran)
    }
}

#[test]
fn the_publishing_job_checks_what_it_was_handed_pushes_both_at_once_and_publishes() {
    let lab = Lab::new("publish");
    let before = lab.remote(&["rev-parse", "dev"]);
    let handover = lab.hand_over("0.8.0", "");
    let (publish, check) = lab.check("cut", "0.8.0", &handover);
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
        ("HANDOVER", handover.to_str().expect("a path")),
    ];
    let push = lab.step("Push dev and the tag", &publish, &env);
    assert_eq!(push.code, Some(0), "{}", push.said);
    assert_eq!(push.outputs.get("pushed").map(String::as_str), Some("true"));
    assert_eq!(lab.remote(&["rev-parse", "dev"]), commit);
    assert_eq!(lab.remote(&["rev-parse", "v0.8.0^{commit}"]), commit);
    assert_eq!(lab.remote(&["cat-file", "-t", "v0.8.0"]), "tag");
    assert_eq!(lab.remote(&["rev-parse", "dev~1"]), before);

    let done = lab.step("Publish the release", &publish, &env);
    assert_eq!(done.code, Some(0), "{}", done.said);
    assert_eq!(
        lab.created(),
        "release create v0.8.0 arca-0.8.0-linux-x86_64 SHA256SUMS --latest --repo automagik-si/arca --verify-tag \
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
}

#[test]
fn when_dev_moved_before_the_check_nothing_is_pushed_and_the_run_ends_cleanly() {
    let lab = Lab::new("moved");
    let handover = lab.hand_over("0.8.0", "");
    lab.someone_pushes("crates/a/src/b.rs", "feat(a): a newer push");
    let newer = lab.remote(&["rev-parse", "dev"]);

    let (_, check) = lab.check("cut", "0.8.0", &handover);
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
    let handover = lab.hand_over("0.8.0", "");
    let (publish, check) = lab.check("cut", "0.8.0", &handover);
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

#[test]
fn the_publishing_job_refuses_what_is_not_exactly_a_release() {
    for twist in [
        "an extra commit",
        "another file",
        "another subject",
        "a plain tag",
        "another binary",
        "an extra file",
        "a second checksum",
        "a link",
    ] {
        let lab = Lab::new("twist");
        let before = lab.remote(&["rev-parse", "dev"]);
        let handover = lab.hand_over("0.8.0", twist);
        let (_, check) = lab.check("cut", "0.8.0", &handover);
        assert_eq!(check.code, Some(1), "{twist} is refused: {}", check.said);
        assert!(check.said.contains("::error::"), "{twist}: {}", check.said);
        assert!(!check.outputs.contains_key("commit"), "{twist}");
        assert_eq!(lab.remote(&["rev-parse", "dev"]), before, "{twist}");
        assert!(!lab.remote_has_tag("v0.8.0"), "{twist}");
    }

    // A version that is not one, a tag of another name than the version, a tag the remote already has, and a release
    // that is already published.
    let lab = Lab::new("names");
    let handover = lab.hand_over("0.8.0", "");
    for version in [
        "0.8",
        "v0.8.0",
        "0.8.0 --latest",
        "0.8.0\nx",
        "$(id)",
        "",
        "0.08.0",
    ] {
        let (_, check) = lab.check("cut", version, &handover);
        assert_eq!(check.code, Some(1), "{version:?}: {}", check.said);
    }
    let (_, other) = lab.check("cut", "0.9.0", &handover);
    assert_eq!(
        other.code,
        Some(1),
        "the files and the bundle are 0.8.0's: {}",
        other.said
    );
    let (_, mode) = lab.check("push", "0.8.0", &handover);
    assert_eq!(mode.code, Some(1));
    lab.releases("v0.8.0 published\nv0.1.0-alpha.7 published\n");
    let (_, published) = lab.check("cut", "0.8.0", &handover);
    assert_eq!(published.code, Some(1));
    assert!(published.said.contains("v0.8.0 is already published"));
    lab.releases("v0.1.0-alpha.7 published\n");
    let cut = lab.dir().join("cut");
    git_at(&cut, &["push", "-q", "origin", "v0.8.0"]);
    let (_, taken) = lab.check("cut", "0.8.0", &handover);
    assert_eq!(taken.code, Some(1));
    assert!(taken.said.contains("v0.8.0 is already on the remote"));
}

#[test]
fn a_tag_that_was_pushed_and_never_published_is_finished_and_nothing_new_is_cut() {
    let lab = Lab::new("finish");
    // An earlier run pushed the release commit and its tag, and its publishing failed.
    let handover = lab.hand_over("0.8.0", "");
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

    // The publishing job, with the files and no bundle: it pushes nothing and publishes the tag's commit.
    fs::remove_file(handover.join("release.bundle")).expect("no bundle in a finish");
    let (publish, check) = lab.check("finish", "0.8.0", &handover);
    assert_eq!(check.code, Some(0), "{}", check.said);
    assert_eq!(check.outputs.get("commit"), Some(&released));
    let done = lab.step(
        "Publish the release",
        &publish,
        &[
            ("VERSION", "0.8.0"),
            ("COMMIT", released.as_str()),
            ("PRERELEASE", "false"),
            ("HANDOVER", handover.to_str().expect("a path")),
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

    // A finish for a tag the remote does not have, or with a bundle beside the files, is refused.
    let (_, absent) = lab.check("finish", "0.9.0", &{
        let other = lab.dir().join("handover-9");
        lab.files(&other, "0.9.0", "");
        other
    });
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
