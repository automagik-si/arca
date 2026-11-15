//! Every protection, on temporary trees. Each test first shows the item IS planned without the protection (the red
//! case), then adds the protection and shows it is not (the green case), so the protection is what changes the outcome.
//! Only the build-outputs, stale-scratch, claude-versions and arca-backups categories run here: they touch nothing
//! outside the temporary home. Caches, docker and runner-caches would call real tools and are never planned in tests.

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

use arca_janitor::config::Config;
use arca_janitor::jobs::clean::{
    Action, Env, Guard, Item, Origin, apply_plan, basic_check, check, make_plan,
};
use arca_janitor::jobs::{CleanPlanParams, Profile};
use arca_janitor::platform;
use arca_janitor::protect::{Protections, Sources};
use arca_janitor::testutil::{TempDir, age_tree};

struct Fixture {
    _t: TempDir,
    home: PathBuf,
    config: Config,
}

impl Fixture {
    fn new(label: &str) -> Fixture {
        let t = TempDir::new(label);
        let home = t.path().join("home");
        fs::create_dir_all(home.join("work")).unwrap();
        fs::create_dir_all(home.join("scratch")).unwrap();
        let mut config = Config::defaults(&home, "testhost");
        config.project_roots = vec![home.join("work")];
        config.scratch_roots = vec![home.join("scratch")];
        Fixture {
            _t: t,
            home,
            config,
        }
    }

    /// A Rust project idle for 30 days, with a target/ of some size.
    fn project(&self, name: &str) -> PathBuf {
        let p = self.home.join("work").join(name);
        fs::create_dir_all(p.join("target/debug")).unwrap();
        fs::create_dir_all(p.join("src")).unwrap();
        fs::write(p.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        fs::write(p.join("target/debug/blob"), vec![7u8; 50_000]).unwrap();
        age_tree(&p, 30);
        p
    }

    fn plan(&self, only: &[&str]) -> arca_janitor::jobs::clean::Plan {
        let params = CleanPlanParams {
            profile: Profile::Full,
            only: only.iter().map(|s| s.to_string()).collect(),
        };
        make_plan(&self.config, None, &params).unwrap_or_else(|e| panic!("plan: {e:?}"))
    }

    fn planned(&self, only: &[&str], path: &Path) -> bool {
        self.plan(only)
            .items
            .iter()
            .any(|i| i.action.path() == Some(path))
    }

    fn skip_reason(&self, only: &[&str], path: &Path) -> Option<String> {
        let label = path.display().to_string();
        self.plan(only)
            .skipped
            .into_iter()
            .find(|s| s.label == label)
            .map(|s| s.reason)
    }
}

/// A process of ours sitting with its working folder (and so a live path) inside `dir`.
struct Sleeper(Child);
impl Sleeper {
    fn in_dir(dir: &Path) -> Sleeper {
        let child = Command::new("sleep")
            .arg("60")
            .current_dir(dir)
            .spawn()
            .unwrap();
        // Give /proc a moment to show the new process.
        std::thread::sleep(std::time::Duration::from_millis(100));
        Sleeper(child)
    }
}
impl Drop for Sleeper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

const BUILD: &[&str] = &["build-outputs"];

#[test]
fn an_idle_build_output_is_planned_and_a_recent_one_is_not() {
    let f = Fixture::new("idle");
    let p = f.project("idle");
    assert!(
        f.planned(BUILD, &p.join("target")),
        "red: idle output planned"
    );
    fs::write(p.join("src/main.rs"), "fn main() {}").unwrap();
    assert!(!f.planned(BUILD, &p.join("target")));
    let why = f.skip_reason(BUILD, &p.join("target")).unwrap();
    assert!(why.contains("changed in the last 14 days"), "{why}");
}

#[test]
fn a_live_process_protects_its_top_level_folder() {
    let f = Fixture::new("live");
    let p = f.project("svc");
    assert!(
        f.planned(BUILD, &p.join("target")),
        "red: planned with no process in it"
    );
    let _sleeper = Sleeper::in_dir(&p.join("src"));
    let why = f.skip_reason(BUILD, &p.join("target")).expect("skipped");
    assert!(why.contains("live process"), "{why}");
}

#[test]
fn protect_paths_are_never_planned() {
    let mut f = Fixture::new("protect");
    let p = f.project("keepme");
    assert!(f.planned(BUILD, &p.join("target")), "red");
    f.config.protect = vec![p.clone()];
    let why = f.skip_reason(BUILD, &p.join("target")).expect("skipped");
    assert!(why.contains("PROTECT"), "{why}");
}

#[test]
fn a_cleanup_keep_file_protects_its_folder() {
    let f = Fixture::new("keep");
    let p = f.project("kept");
    assert!(f.planned(BUILD, &p.join("target")), "red");
    fs::write(p.join(".cleanup-keep"), "").unwrap();
    // Age it again so recency is not what keeps it.
    age_tree(&p, 30);
    let why = f.skip_reason(BUILD, &p.join("target")).expect("skipped");
    assert!(why.contains(".cleanup-keep"), "{why}");
}

#[test]
fn a_user_service_protects_its_working_folder() {
    let f = Fixture::new("unit");
    let p = f.project("served");
    assert!(f.planned(BUILD, &p.join("target")), "red");
    if platform::is_macos() {
        return;
    }
    let units = f.home.join(".config/systemd/user");
    fs::create_dir_all(&units).unwrap();
    fs::write(
        units.join("app.service"),
        format!(
            "[Service]\nWorkingDirectory={}\nExecStart={}/run\n",
            p.display(),
            p.display()
        ),
    )
    .unwrap();
    let why = f.skip_reason(BUILD, &p.join("target")).expect("skipped");
    assert!(why.contains("user service"), "{why}");
}

#[test]
fn container_mounts_pm2_apps_and_disk_devices_protect_through_their_sources() {
    let f = Fixture::new("sources");
    let p = f.project("mounted");
    let uid = platform::euid();
    let target = p.join("target");
    let item = Item {
        id: 0,
        category: "build-outputs".into(),
        action: Action::Remove {
            path: target.clone(),
        },
        bytes: 1,
        reason: String::new(),
        guards: vec![Guard::Protected],
        origin: Origin::default(),
    };
    for source in [
        "container bind mount",
        "pm2 app",
        "incus disk device",
        "lima mount",
    ] {
        let env = |entries: Vec<(PathBuf, &'static str)>| Env {
            config: &f.config,
            prot: Protections::new(
                &f.config,
                uid,
                HashSet::new(),
                Sources {
                    entries,
                    live: Default::default(),
                },
            ),
            ctx: None,
            now: arca_janitor::util::now(),
            profile: Profile::Full,
        };
        assert_eq!(check(&item, &env(vec![])), Ok(()), "red: {source}");
        let why = check(&item, &env(vec![(p.join("data"), source)])).unwrap_err();
        assert!(why.contains(source), "{why}");
    }
}

#[test]
fn tool_dot_folders_of_the_home_are_never_planned() {
    let mut f = Fixture::new("tools");
    f.config.project_roots = vec![f.home.clone()];
    for dir in ["visible/app", ".tooly/global"] {
        let p = f.home.join(dir);
        fs::create_dir_all(p.join("node_modules/pkg")).unwrap();
        fs::write(p.join("package.json"), "{}").unwrap();
        age_tree(&p, 30);
    }
    age_tree(&f.home.join(".tooly"), 30);
    age_tree(&f.home.join("visible"), 30);
    let plan = f.plan(BUILD);
    let planned: Vec<&Path> = plan.items.iter().filter_map(|i| i.action.path()).collect();
    assert!(
        planned.contains(&f.home.join("visible/app/node_modules").as_path()),
        "red: {planned:?}"
    );
    assert!(
        !planned.contains(&f.home.join(".tooly/global/node_modules").as_path()),
        "{planned:?}"
    );
}

#[test]
fn links_are_never_followed_or_removed() {
    let f = Fixture::new("links");
    let real = f.project("real");
    assert!(
        f.planned(BUILD, &real.join("target")),
        "red: a real target/ is planned"
    );
    // A project whose target/ is a link to the real one: not a candidate at all.
    let linked = f.home.join("work/linked");
    fs::create_dir_all(&linked).unwrap();
    fs::write(linked.join("Cargo.toml"), "").unwrap();
    symlink(real.join("target"), linked.join("target")).unwrap();
    age_tree(&linked, 30);
    assert!(!f.planned(BUILD, &linked.join("target")));
    // A planned folder replaced by a link before apply is refused, and the link's target survives.
    let plan = f.plan(BUILD);
    let other = f.project("other");
    let plan2 = f.plan(BUILD);
    assert!(
        plan2
            .items
            .iter()
            .any(|i| i.action.path() == Some(other.join("target").as_path()))
    );
    fs::remove_dir_all(other.join("target")).unwrap();
    symlink(real.join("target"), other.join("target")).unwrap();
    let only_other = arca_janitor::jobs::clean::Plan {
        items: plan2
            .items
            .into_iter()
            .filter(|i| i.action.path() == Some(other.join("target").as_path()))
            .collect(),
        ..plan
    };
    let result = apply_plan(&only_other, &f.config, None).unwrap();
    assert!(result.done.is_empty(), "{result:?}");
    assert!(
        result.skipped[0].reason.contains("link"),
        "{:?}",
        result.skipped
    );
    assert!(real.join("target/debug/blob").exists());
}

#[test]
fn mount_points_and_other_owners_are_refused() {
    let f = Fixture::new("mounts");
    let p = f.project("m");
    let uid = platform::euid();
    let env = |mounts: HashSet<PathBuf>| Env {
        config: &f.config,
        prot: Protections::new(&f.config, uid, mounts, Sources::default()),
        ctx: None,
        now: arca_janitor::util::now(),
        profile: Profile::Full,
    };
    assert_eq!(
        basic_check(&p.join("target"), &env(HashSet::new())),
        Ok(()),
        "red"
    );
    let mounted: HashSet<PathBuf> = [p.join("target")].into_iter().collect();
    assert_eq!(
        basic_check(&p.join("target"), &env(mounted)),
        Err("refused: a mount point".to_string())
    );
    // A folder owned by root: only checked, never touched.
    if uid != 0 {
        assert_eq!(
            basic_check(Path::new("/usr"), &env(HashSet::new())),
            Err("refused: not owned by this user".to_string())
        );
    }
    assert_eq!(
        basic_check(Path::new("relative/path"), &env(HashSet::new())),
        Err("refused: odd path".to_string())
    );
    assert_eq!(
        basic_check(&p.join("target/../target"), &env(HashSet::new())),
        Err("refused: odd path".to_string())
    );
}

#[test]
fn apply_checks_every_item_again() {
    let f = Fixture::new("recheck");
    let a = f.project("a");
    let b = f.project("b");
    let c = f.project("c");
    let d = f.project("d");
    let plan = f.plan(BUILD);
    assert_eq!(plan.items.len(), 4, "{:?}", plan.skipped);
    // After the plan: a changes, b gets a .cleanup-keep, a process starts in c; d stays as it was.
    fs::write(a.join("src/lib.rs"), "").unwrap();
    fs::write(b.join(".cleanup-keep"), "").unwrap();
    age_tree(&b, 30);
    let _sleeper = Sleeper::in_dir(&c.join("src"));
    let result = apply_plan(&plan, &f.config, None).unwrap();
    let applied: Vec<&str> = result.done.iter().map(|x| x.label.as_str()).collect();
    assert_eq!(applied, vec![d.join("target").display().to_string()]);
    assert!(!d.join("target").exists());
    for (dir, why) in [
        (&a, "changed in the last"),
        (&b, ".cleanup-keep"),
        (&c, "live process"),
    ] {
        assert!(dir.join("target/debug/blob").exists(), "{}", dir.display());
        let label = dir.join("target").display().to_string();
        let skip = result.skipped.iter().find(|s| s.label == label).unwrap();
        assert!(skip.reason.contains(why), "{}: {}", label, skip.reason);
    }
}

#[test]
fn stale_scratch_goes_to_trash_and_old_trash_is_deleted() {
    let f = Fixture::new("scratch");
    let scratch = f.home.join("scratch");
    for name in ["old-run", "fresh-run", "huggingface"] {
        fs::create_dir_all(scratch.join(name)).unwrap();
        fs::write(scratch.join(name).join("f"), "x").unwrap();
    }
    age_tree(&scratch.join("old-run"), 30);
    age_tree(&scratch.join("huggingface"), 30);
    let old_day = scratch.join(".cleanup-trash/2000-01-01");
    fs::create_dir_all(old_day.join("gone")).unwrap();
    symlink(&f.home, scratch.join("a-link")).unwrap();
    let plan = f.plan(&["stale-scratch"]);
    let labels: Vec<String> = plan.items.iter().map(|i| i.action.label()).collect();
    assert!(
        labels.contains(&scratch.join("old-run").display().to_string()),
        "{labels:?}"
    );
    assert!(
        labels.contains(&old_day.display().to_string()),
        "{labels:?}"
    );
    assert_eq!(
        labels.len(),
        2,
        "fresh, CACHE_KEEP and links stay: {labels:?}"
    );
    let keep = plan
        .skipped
        .iter()
        .find(|s| s.label.ends_with("huggingface"))
        .unwrap();
    assert!(keep.reason.contains("CACHE_KEEP"));
    let result = apply_plan(&plan, &f.config, None).unwrap();
    assert!(result.failed.is_empty(), "{:?}", result.failed);
    let moved = result.done.iter().find_map(|a| a.moved_to.clone()).unwrap();
    assert!(moved.starts_with(scratch.join(".cleanup-trash")));
    assert!(moved.join("f").exists(), "quarantine keeps the content");
    assert!(!scratch.join("old-run").exists());
    assert!(!old_day.exists());
    assert!(scratch.join("a-link").exists() && f.home.join("work").exists());
}

#[test]
fn old_claude_builds_go_but_the_active_newest_and_running_stay() {
    let f = Fixture::new("claude");
    let versions = f.home.join(".local/share/claude/versions");
    fs::create_dir_all(&versions).unwrap();
    fs::create_dir_all(f.home.join(".local/bin")).unwrap();
    let sleep = ["/bin/sleep", "/usr/bin/sleep"]
        .into_iter()
        .find(|p| Path::new(p).exists())
        .expect("a sleep binary");
    // Each build is a folder holding a binary still named `sleep` (multi-call coreutils pick the tool by name).
    for (i, v) in ["1.0.0", "1.1.0", "1.2.0", "1.3.0"].iter().enumerate() {
        fs::create_dir_all(versions.join(v)).unwrap();
        fs::copy(sleep, versions.join(v).join("sleep")).unwrap();
        arca_janitor::testutil::age(&versions.join(v), 10 - i as u64);
    }
    // Active: 1.2.0. Newest other: 1.3.0. Candidates: 1.1.0 and 1.0.0.
    symlink(versions.join("1.2.0"), f.home.join(".local/bin/claude")).unwrap();
    let plan = f.plan(&["claude-versions"]);
    let names: Vec<String> = plan
        .items
        .iter()
        .map(|i| {
            i.action
                .path()
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string()
        })
        .collect();
    assert_eq!(
        names,
        vec!["1.1.0", "1.0.0"],
        "red: both old builds planned"
    );
    let mut running = Command::new(versions.join("1.0.0/sleep"))
        .arg("60")
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(100));
    let plan = f.plan(&["claude-versions"]);
    let names: Vec<String> = plan
        .items
        .iter()
        .map(|i| {
            i.action
                .path()
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string()
        })
        .collect();
    let _ = running.kill();
    let _ = running.wait();
    assert_eq!(names, vec!["1.1.0"]);
    assert!(
        plan.skipped
            .iter()
            .any(|s| s.label.ends_with("1.0.0") && s.reason.contains("runs it"))
    );
}

#[test]
fn arca_backups_keep_the_newest_two() {
    let f = Fixture::new("backups");
    let bin = f.home.join(".local/bin");
    fs::create_dir_all(&bin).unwrap();
    for d in ["20260101", "20260201", "20260301", "20260401"] {
        fs::write(bin.join(format!("arca.bak-{d}")), "x").unwrap();
    }
    let plan = f.plan(&["arca-backups"]);
    let mut names: Vec<String> = plan.items.iter().map(|i| i.action.label()).collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            bin.join("arca.bak-20260101").display().to_string(),
            bin.join("arca.bak-20260201").display().to_string()
        ]
    );
}

#[test]
fn a_plan_from_another_host_is_refused() {
    let f = Fixture::new("host");
    f.project("x");
    let mut plan = f.plan(BUILD);
    plan.host = "elsewhere".into();
    let err = apply_plan(&plan, &f.config, None).unwrap_err();
    assert!(format!("{err:?}").contains("elsewhere"));
}
