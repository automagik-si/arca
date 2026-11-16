//! Checkout delivery uses real Git and a local source; no GitHub or Docker service is contacted.
use super::*;

fn target(w: &Workspace) -> PathBuf {
    w.dir.join("data/workspace")
}
fn marker(w: &Workspace) -> PathBuf {
    w.dir.join("checkout.json")
}
fn boot(w: &mut Workspace) {
    w.start();
    says(w, "status:");
    assert!(
        marker(w).exists(),
        "checkout marker must precede the server"
    );
}
fn roster_file(w: &Workspace, path: &str, text: &str) {
    let file = target(w).join(path);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(file, text).unwrap();
}
fn import(w: &Workspace) {
    fs::create_dir_all(w.dir.join("data")).unwrap();
    let out = Command::new(which("git"))
        .args(["clone", "--quiet"])
        .arg(w.dir.join("source"))
        .arg(target(w))
        .output()
        .unwrap();
    assert!(out.status.success());
    git_at(
        &target(w),
        &[
            "remote",
            "set-url",
            "origin",
            "https://github.com/example/project",
        ],
    );
}
fn blocked(w: &mut Workspace, cause: &str) {
    w.start();
    says(w, cause);
    assert!(w.runs("automagik-server").is_empty());
}
fn record(w: &Workspace) -> Value {
    serde_json::from_str(&fs::read_to_string(marker(w)).unwrap()).unwrap()
}
fn save(w: &Workspace, value: &Value) {
    fs::write(marker(w), value.to_string()).unwrap();
}
fn git_bytes(w: &Workspace) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(p: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        let mut entries: Vec<_> = fs::read_dir(p).unwrap().map(Result::unwrap).collect();
        entries.sort_by_key(|e| e.path());
        for e in entries {
            if e.path().is_dir() {
                walk(&e.path(), out)
            } else {
                out.push((e.path(), fs::read(e.path()).unwrap()));
            }
        }
    }
    let mut out = vec![];
    walk(&target(w).join(".git"), &mut out);
    out
}

#[test]
fn checkout_01_clone_ref_marker_precedes_seed() {
    let mut value = settings(URL);
    value["repoRef"] = json!("topic");
    let mut w = Workspace::new(&value);
    git_at(&w.dir.join("source"), &["checkout", "-b", "topic"]);
    fs::write(w.dir.join("source/topic"), "topic").unwrap();
    git_at(&w.dir.join("source"), &["add", "."]);
    git_at(&w.dir.join("source"), &["commit", "-m", "topic"]);
    boot(&mut w);
    assert!(target(&w).join("topic").exists());
    assert_eq!(git_at(&target(&w), &["branch", "--show-current"]), "topic");
    assert_eq!(record(&w)["repo"], "github.com/example/project");
    assert!(record(&w)["seedStartedAt"].as_u64().is_some());
    let before_docker_return: Value =
        serde_json::from_str(&w.read("docker/seed-marker.json")).unwrap();
    assert_eq!(
        before_docker_return["seedStartedAt"],
        record(&w)["seedStartedAt"]
    );
    assert_eq!(
        fs::metadata(marker(&w)).unwrap().permissions().mode() & 0o777,
        0o600
    );
    use std::os::unix::fs::MetadataExt;
    let ids: Vec<u32> = me().split(':').map(|id| id.parse().unwrap()).collect();
    assert_eq!(fs::metadata(target(&w)).unwrap().uid(), ids[0]);
    assert_eq!(fs::metadata(target(&w)).unwrap().gid(), ids[1]);
    assert!(
        fs::read_to_string(script())
            .unwrap()
            .contains("${WS_DATA_OWNER:=1000:1000}")
    );
}
#[test]
fn checkout_02_restart_preserves_local_work_without_git_network() {
    let mut w = Workspace::new(&settings(URL));
    boot(&mut w);
    w.stop();
    git_at(&target(&w), &["config", "user.name", "Owner"]);
    git_at(&target(&w), &["config", "user.email", "owner@example.org"]);
    fs::write(target(&w).join("local"), "committed").unwrap();
    git_at(&target(&w), &["add", "."]);
    git_at(&target(&w), &["commit", "-m", "local"]);
    let head = git_at(&target(&w), &["rev-parse", "HEAD"]);
    fs::write(target(&w).join("untracked"), "keep").unwrap();
    fs::rename(w.dir.join("source"), w.dir.join("unreachable")).unwrap();
    boot(&mut w);
    assert_eq!(git_at(&target(&w), &["rev-parse", "HEAD"]), head);
    assert_eq!(
        fs::read_to_string(target(&w).join("untracked")).unwrap(),
        "keep"
    );
}
#[test]
fn checkout_03_origin_and_repository_id_mismatch_preserve_git() {
    for id in [false, true] {
        let mut w = Workspace::new(&settings(URL));
        boot(&mut w);
        w.stop();
        if id {
            fs::write(w.dir.join("api.json"), r#"{"id":99}"#).unwrap();
        } else {
            git_at(
                &target(&w),
                &[
                    "remote",
                    "set-url",
                    "origin",
                    "https://github.com/other/project.git",
                ],
            );
        }
        let before = git_bytes(&w);
        let runs = w.runs("automagik-server").len();
        w.start();
        says(
            &w,
            if id {
                "repository id changed"
            } else {
                "origin is github.com/other/project"
            },
        );
        assert_eq!(git_bytes(&w), before);
        assert_eq!(w.runs("automagik-server").len(), runs);
    }
}
#[test]
fn checkout_04_origin_credentials_are_never_logged() {
    let mut w = Workspace::new(&settings(URL));
    import(&w);
    let secret = ["synthetic", "origin", "password"].join("-");
    let url = format!(
        "{}{}{}",
        "https://user:", secret, "@github.com/OTHER/project.git"
    );
    git_at(&target(&w), &["remote", "set-url", "origin", &url]);
    blocked(
        &mut w,
        "origin is github.com/other/project, repoUrl is github.com/example/project",
    );
    assert!(!w.output().contains(&secret));
    w.stop();
    git_at(
        &target(&w),
        &[
            "remote",
            "set-url",
            "origin",
            "https://unrecognised.example/secret",
        ],
    );
    blocked(&mut w, "origin is not a GitHub URL");
    assert!(!w.output().contains("unrecognised.example"));
}
#[test]
fn checkout_05_foreign_rosters_are_kept_and_block_seed() {
    for (path, version) in [(".herdr/bots.json", 1), (".automagik/bots/bots.json", 2)] {
        for tracked in [false, true] {
            let mut w = Workspace::new(&settings(URL));
            let text = format!(r#"{{"version":{version},"bots":[]}}"#);
            if tracked {
                let source = w.dir.join("source");
                fs::create_dir_all(source.join(path).parent().unwrap()).unwrap();
                fs::write(source.join(path), &text).unwrap();
                git_at(&source, &["add", "."]);
                git_at(&source, &["commit", "-m", "roster"]);
            } else {
                import(&w);
                save(
                    &w,
                    &json!({"repo":"github.com/example/project","repoId":42,"clonedAt":1,"rosterTrackedAtClone":false,"rosterBeforeSeed":null,"seedStartedAt":null}),
                );
                roster_file(&w, path, &text);
            }
            blocked(&mut w, "foreign roster");
            if !tracked {
                assert_eq!(record(&w)["rosterBeforeSeed"], true);
            }
            assert_eq!(fs::read_to_string(target(&w).join(path)).unwrap(), text);
        }
    }
}
#[test]
fn checkout_06_own_v1_v2_roster_survives_and_invalid_roster_blocks() {
    for (version, path, valid) in [
        ("1", ".herdr/bots.json", true),
        ("2", ".automagik/bots/bots.json", true),
        ("2", ".herdr/bots.json", false),
        ("99", ".herdr/bots.json", false),
    ] {
        let mut w = Workspace::new(&settings(URL));
        let text = format!(r#"{{"version":{version},"bots":[]}}"#);
        if version == "1" {
            fs::write(w.dir.join("docker/seed-roster"), &text).unwrap();
        }
        boot(&mut w);
        w.stop();
        roster_file(&w, path, &text);
        if !valid {
            w.start();
            says(&w, "foreign roster");
        } else {
            boot(&mut w);
        }
        assert_eq!(fs::read_to_string(target(&w).join(path)).unwrap(), text);
    }
}
#[test]
fn checkout_07_identity_and_helper_only_when_unset() {
    for login in ["Project-Custom-Repositories[bot]", "arca-repositories[bot]"] {
        for level in ["none", "global", "local", "partial"] {
            let mut w = Workspace::new(&settings(URL));
            import(&w);
            w.secret_named(
                "GITHUB_BOT_IDENTITY",
                &json!({"id":"42","login":login}).to_string(),
            );
            // A stale legacy scalar must never be combined with the verified login bundle.
            w.secret_named("GITHUB_BOT_ID", "9999");
            if level == "local" || level == "partial" {
                git_at(&target(&w), &["config", "user.name", "Owner"]);
            }
            if level == "local" {
                git_at(&target(&w), &["config", "user.email", "owner@example.org"]);
            }
            if level == "global" {
                fs::create_dir_all(w.dir.join("data")).unwrap();
                fs::write(w.dir.join("data/.gitconfig"), "[user]\n name = Owner\n email = owner@example.org\n[credential \"https://github.com\"]\n helper = owner-helper\n").unwrap();
                // A real global identity is visible to the checkout's effective config too.
                let mut global = w.read("git-global");
                global.push_str(&w.read("data/.gitconfig"));
                fs::write(w.dir.join("git-global"), global).unwrap();
            }
            boot(&mut w);
            let global = w.read("data/.gitconfig");
            if level == "none" {
                assert_eq!(
                    git_at(
                        &target(&w),
                        &[
                            "config",
                            "--file",
                            w.dir.join("data/.gitconfig").to_str().unwrap(),
                            "--get",
                            "user.name"
                        ]
                    ),
                    login
                );
                assert_eq!(
                    git_at(
                        &target(&w),
                        &[
                            "config",
                            "--file",
                            w.dir.join("data/.gitconfig").to_str().unwrap(),
                            "--get",
                            "user.email"
                        ]
                    ),
                    format!("42+{login}@users.noreply.github.com")
                );
            } else {
                assert!(!global.contains(login));
            }
            if level == "global" {
                assert!(global.contains("owner-helper"));
                assert!(!global.contains("github-token"));
            } else {
                assert!(global.contains("github-token"));
            }
        }
    }
}
#[test]
fn checkout_08_recovery_keeps_data_and_unknown_rosters_block() {
    for (version, path) in [(1, ".herdr/bots.json"), (2, ".automagik/bots/bots.json")] {
        for corrupt in [false, true] {
            let mut w = Workspace::new(&settings(URL));
            import(&w);
            let text = format!(r#"{{"version":{version},"bots":[]}}"#);
            roster_file(&w, path, &text);
            if corrupt {
                fs::write(marker(&w), "broken").unwrap();
            }
            blocked(
                &mut w,
                "roster provenance unknown: marker missing; the roster is kept",
            );
            assert!(!marker(&w).exists());
            assert_eq!(w.dir.join("checkout.json.corrupt").exists(), corrupt);
            let other = if version == 1 {
                ".automagik/bots/bots.json"
            } else {
                ".herdr/bots.json"
            };
            assert!(
                !target(&w).join(other).exists(),
                "no second roster is generated"
            );
            assert!(!w.dir.join("data/bots/bots.json").exists());
            assert_eq!(fs::read_to_string(target(&w).join(path)).unwrap(), text);
        }
    }
    for corrupt in [false, true] {
        let mut w = Workspace::new(&settings(URL));
        import(&w);
        if corrupt {
            fs::write(marker(&w), "broken").unwrap();
        }
        boot(&mut w);
        assert!(marker(&w).exists());
    }
    // The directory rename completed, but the marker rename did not.
    let mut w = Workspace::new(&settings(URL));
    boot(&mut w);
    w.stop();
    fs::rename(marker(&w), w.dir.join("checkout.json.new")).unwrap();
    boot(&mut w);
    assert!(marker(&w).exists());
    // A crash before the directory rename left only a disposable clone and pending marker.
    let mut w = Workspace::new(&settings(URL));
    fs::create_dir_all(w.dir.join("data/workspace.arca-clone")).unwrap();
    fs::write(
        w.dir.join("data/workspace.arca-clone/partial"),
        "unfinished clone",
    )
    .unwrap();
    fs::write(w.dir.join("checkout.json.new"), "unfinished marker").unwrap();
    boot(&mut w);
    assert!(!target(&w).join("partial").exists());
    assert!(!w.dir.join("checkout.json.new").exists());
}
#[test]
fn checkout_09_offline_boot_with_marker_is_ready() {
    let mut w = Workspace::new(&settings(URL));
    boot(&mut w);
    w.stop();
    fs::write(w.dir.join("api-fail"), "offline").unwrap();
    boot(&mut w);
    says(&w, "repository id check skipped");
}
#[test]
fn checkout_10_clone_failure_retries_on_new_token() {
    let mut w = Workspace::new(&settings(URL));
    fs::remove_file(w.dir.join("secrets/GITHUB_TOKEN")).unwrap();
    blocked(&mut w, "checkout waits for GITHUB_TOKEN");
    assert!(!target(&w).join(".git").exists());
    w.secret_named("GITHUB_TOKEN", "synthetic-new-repository-token");
    says(&w, "the server is started from");
    assert!(target(&w).join(".git").exists());
    let mut w = Workspace::new(&settings(URL));
    fs::rename(w.dir.join("source"), w.dir.join("unreachable")).unwrap();
    blocked(&mut w, "clone failed");
    assert!(!w.dir.join("data/workspace.arca-clone").exists());
    assert!(!w.dir.join("checkout.json.new").exists());
    fs::rename(w.dir.join("unreachable"), w.dir.join("source")).unwrap();
    w.secret_named("GITHUB_TOKEN", "synthetic-credential-for-immediate-retry");
    says(&w, "the server is started from");
}
#[test]
fn checkout_11_token_never_reaches_arguments_logs_or_checkout() {
    let mut w = Workspace::new(&settings(URL));
    boot(&mut w);
    assert!(
        !w.everything_written()
            .contains("synthetic-repository-credential")
    );
    assert_eq!(
        git_at(&target(&w), &["remote", "get-url", "origin"]),
        "https://github.com/example/project"
    );
}
#[test]
fn checkout_12_refuses_unsafe_settings() {
    for (key, value) in [
        ("repoUrl", ""),
        ("repoUrl", "https://user@github.com/a/b"),
        ("repoUrl", "git@github.com:a/b"),
        ("repoUrl", "http://github.com/a/b"),
        ("repoUrl", "https://other.example/a/b"),
        ("repoRef", "--upload-pack=bad"),
        ("repoRef", "main~1"),
        ("repoRef", "main\nother"),
    ] {
        let mut settings = settings(URL);
        settings[key] = json!(value);
        let mut w = Workspace::new(&settings);
        blocked(&mut w, &format!("settings.{key}"));
    }
}
#[test]
fn checkout_13_nonrepo_is_untouched_and_member_target_is_used() {
    let mut w = Workspace::new(&settings(URL));
    fs::create_dir_all(target(&w)).unwrap();
    fs::write(target(&w).join("keep"), "data").unwrap();
    blocked(&mut w, "not empty and has no .git");
    assert_eq!(fs::read_to_string(target(&w).join("keep")).unwrap(), "data");
    let mut value = settings(URL);
    value["memberRunner"] = json!(true);
    let mut w = Workspace::new(&value);
    fs::create_dir_all(w.dir.join("runner-sockets")).unwrap();
    fs::create_dir_all(w.dir.join("workspace")).unwrap();
    fs::set_permissions(w.dir.join("workspace"), fs::Permissions::from_mode(0o711)).unwrap();
    boot(&mut w);
    assert_eq!(fs::metadata(w.dir.join("workspace")).unwrap().permissions().mode() & 0o777, 0o711, "members must retain traversal through the workspace root");
    w.stop(); boot(&mut w);
    assert_eq!(fs::metadata(w.dir.join("workspace")).unwrap().permissions().mode() & 0o777, 0o711);
    assert!(w.dir.join("workspace/.git").exists());
    assert!(!target(&w).join(".git").exists());
}
#[test]
fn checkout_14_token_rotation_reaches_private_file_without_restart() {
    let mut w = Workspace::new(&settings(URL));
    boot(&mut w);
    w.secret_named("GITHUB_TOKEN", "synthetic-rotated-repository-token");
    wait_for(&w, "rotated GitHub credential", || {
        w.read("data/secrets/github-token") == "synthetic-rotated-repository-token"
    });
    assert_eq!(w.runs("automagik-server").len(), 1);
    let meta = fs::metadata(w.dir.join("data/secrets/github-token")).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
}

#[test]
fn checkout_roster_readback_covers_both_repo_formats_and_the_separate_home_registry() {
    let mut w = Workspace::new(&settings(URL));
    boot(&mut w);
    w.stop();
    roster_file(&w, ".herdr/bots.json", r#"{"version":1,"bots":[]}"#);
    roster_file(
        &w,
        ".automagik/bots/bots.json",
        r#"{"version":2,"bots":[]}"#,
    );
    fs::write(
        w.dir.join("data/bots/bots.json"),
        r#"{"version":2,"bots":[],"projects":[{"folder":"/data/workspace"}],"shown":[]}"#,
    )
    .unwrap();
    let files = [
        ("data/workspace/.herdr/bots.json", 1, "OWN"),
        ("data/workspace/.automagik/bots/bots.json", 2, "OWN"),
        (
            "data/bots/bots.json",
            2,
            "UNKNOWN: outside checkout provenance",
        ),
    ];
    let before: Vec<_> = files
        .iter()
        .map(|(path, _, _)| fs::read(w.dir.join(path)).unwrap())
        .collect();
    boot(&mut w);
    let readback: Vec<_> = files
        .iter()
        .zip(before)
        .map(|((path, version, verdict), bytes)| {
            let content = fs::read(w.dir.join(path)).unwrap();
            assert_eq!(content, bytes, "the keeper must retain every roster byte");
            let value: Value = serde_json::from_slice(&content).unwrap();
            assert_eq!(value["version"], *version);
            json!({"path":path, "exists":true, "version":value["version"], "provenance":verdict})
        })
        .collect();
    println!("roster read-back: {}", json!(readback));
    assert_eq!(readback.len(), 3);
    // No home registry receipt exists: neither matching repository origin nor its roster proves home provenance.
    assert_eq!(record(&w)["rosterTrackedAtClone"], false);
    assert_eq!(record(&w)["rosterBeforeSeed"], false);
}
