//! What must never be touched, worked out fresh on every plan and every apply.
//!
//! Every path something live depends on (a user service, a pm2 app, a container's bind mount, an Incus disk device,
//! a Lima mount, a live process's working folder, executable or open file), every PROTECT path and every folder
//! holding a `.cleanup-keep` file is turned into the top-level folder it sits in under the deepest root (its anchor).
//! Nothing equal to, inside, or around an anchor is touched. The home's tool dot-folders (`~/.cargo`, `~/.bun`,
//! `~/.local`...) hold global installs, not projects, and are skipped too.

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use crate::config::Config;
use crate::jobs::{Ctx, Stop};
use crate::platform::{self, Live};
use crate::util::{on_path, run_cmd};
use crate::walk::{Visit, Walk};

const TOOL_TIMEOUT: Duration = Duration::from_secs(30);

/// The source name of a live process's path.
pub const LIVE: &str = "live process";

/// The paths things depend on, each with where it came from.
#[derive(Clone, Debug, Default)]
pub struct Sources {
    pub entries: Vec<(PathBuf, &'static str)>,
    pub live: Live,
}

/// The roots that are real folders of this user's (not links, not someone else's).
pub fn keep_roots(roots: &[PathBuf], uid: u32) -> Vec<PathBuf> {
    roots
        .iter()
        .filter(|r| {
            fs::symlink_metadata(r)
                .is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink() && m.uid() == uid)
        })
        .cloned()
        .collect()
}

/// Ask the system what depends on which paths.
pub fn gather(
    config: &Config,
    uid: u32,
    mounts: &HashSet<PathBuf>,
    ctx: Option<&Ctx>,
) -> Result<Sources, Stop> {
    let mut s = Sources::default();
    for p in &config.protect {
        s.entries.push((p.clone(), "PROTECT"));
    }
    for p in platform::service_paths(&config.home) {
        s.entries.push((p, "user service"));
    }
    for p in pm2_paths(&config.home) {
        s.entries.push((p, "pm2 app"));
    }
    for p in docker_bind_mounts() {
        s.entries.push((p, "container bind mount"));
    }
    for p in incus_disks() {
        s.entries.push((p, "incus disk device"));
    }
    for p in lima_mounts(&config.home) {
        s.entries.push((p, "lima mount"));
    }
    s.live = platform::live(uid);
    for p in &s.live.paths {
        s.entries.push((p.clone(), LIVE));
    }
    let mut roots = keep_roots(&config.scratch_roots, uid);
    roots.extend(keep_roots(&config.project_roots, uid));
    for root in &roots {
        let depth = usize::try_from(config.max_depth).unwrap_or(6);
        Walk::new(mounts, ctx)
            .depth(depth)
            .run(root, &mut |p, md, _| {
                if !md.is_dir()
                    && p.file_name().and_then(|n| n.to_str()) == Some(".cleanup-keep")
                    && let Some(parent) = p.parent()
                {
                    s.entries.push((parent.to_path_buf(), ".cleanup-keep"));
                }
                Visit::Continue
            })?;
    }
    Ok(s)
}

/// `pm2 jlist`'s apps: `pm2_env.pm_cwd` and `pm2_env.pm_exec_path`.
pub fn parse_pm2_jlist(text: &str) -> Vec<PathBuf> {
    let Ok(Value::Array(apps)) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    apps.iter()
        .flat_map(|a| {
            ["pm_cwd", "pm_exec_path"]
                .into_iter()
                .filter_map(move |k| a.get("pm2_env")?.get(k)?.as_str().map(PathBuf::from))
        })
        .collect()
}

/// Only a pm2 daemon that already runs is asked: `pm2 jlist` would start one otherwise.
fn pm2_paths(home: &Path) -> Vec<PathBuf> {
    if !home.join(".pm2/rpc.sock").exists() {
        return Vec::new();
    }
    let bun_pm2 = home.join(".bun/bin/pm2");
    let program = if on_path("pm2") {
        "pm2".to_string()
    } else if bun_pm2.is_file() {
        bun_pm2.display().to_string()
    } else {
        return Vec::new();
    };
    run_cmd(&[&program, "jlist"], TOOL_TIMEOUT)
        .map(|o| parse_pm2_jlist(&o.stdout))
        .unwrap_or_default()
}

/// `docker inspect`'s bind mount sources.
pub fn parse_docker_inspect(text: &str) -> Vec<PathBuf> {
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|c| c.get("Mounts")?.as_array())
        .flatten()
        .filter(|m| m.get("Type").and_then(Value::as_str) == Some("bind"))
        .filter_map(|m| m.get("Source")?.as_str().map(PathBuf::from))
        .collect()
}

fn docker_bind_mounts() -> Vec<PathBuf> {
    if !on_path("docker") {
        return Vec::new();
    }
    let Some(ids) = run_cmd(&["docker", "ps", "-aq"], TOOL_TIMEOUT).filter(|o| o.success) else {
        return Vec::new();
    };
    let ids: Vec<&str> = ids.stdout.split_whitespace().collect();
    if ids.is_empty() {
        return Vec::new();
    }
    let mut argv = vec!["docker", "inspect"];
    argv.extend(ids);
    run_cmd(&argv, TOOL_TIMEOUT)
        .map(|o| parse_docker_inspect(&o.stdout))
        .unwrap_or_default()
}

/// `incus query /1.0/instances?recursion=1`'s disk devices' sources.
pub fn parse_incus_instances(text: &str) -> Vec<PathBuf> {
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|i| i.get("expanded_devices")?.as_object())
        .flat_map(|d| d.values())
        .filter(|d| d.get("type").and_then(Value::as_str) == Some("disk"))
        .filter_map(|d| d.get("source")?.as_str())
        .filter(|s| s.starts_with('/'))
        .map(PathBuf::from)
        .collect()
}

fn incus_disks() -> Vec<PathBuf> {
    if !on_path("incus") {
        return Vec::new();
    }
    run_cmd(
        &[
            "incus",
            "query",
            "/1.0/instances?all-projects=true&recursion=1",
        ],
        TOOL_TIMEOUT,
    )
    .filter(|o| o.success)
    .map(|o| parse_incus_instances(&o.stdout))
    .unwrap_or_default()
}

/// The `location:` lines of a Lima instance's lima.yaml (`~` is the home).
pub fn parse_lima_yaml(text: &str, home: &Path) -> Vec<PathBuf> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim().trim_start_matches("- ").trim();
            l.strip_prefix("location:")
        })
        .map(|v| v.trim().trim_matches(['"', '\'']))
        .filter(|v| v.starts_with('/') || v.starts_with('~'))
        .map(|v| crate::config::expand_home(v, home))
        .collect()
}

fn lima_mounts(home: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(home.join(".lima")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| fs::read_to_string(e.path().join("lima.yaml")).ok())
        .flat_map(|t| parse_lima_yaml(&t, home))
        .collect()
}

/// The protections in force for one plan or apply.
#[derive(Clone, Debug)]
pub struct Protections {
    pub home: PathBuf,
    pub uid: u32,
    pub scratch_roots: Vec<PathBuf>,
    pub project_roots: Vec<PathBuf>,
    /// Anchored folders and the PROTECT paths themselves, each with its reason.
    pub protected: Vec<(PathBuf, &'static str)>,
    pub protect: Vec<PathBuf>,
    pub live: Live,
    pub mounts: HashSet<PathBuf>,
    /// The sources other than live processes, each also in its real form, kept to rebuild `protected` when live
    /// processes are looked at again.
    pub entries: Vec<(PathBuf, &'static str)>,
}

impl Protections {
    pub fn new(
        config: &Config,
        uid: u32,
        mounts: HashSet<PathBuf>,
        sources: Sources,
    ) -> Protections {
        let mut p = Protections {
            home: config.home.clone(),
            uid,
            scratch_roots: keep_roots(&config.scratch_roots, uid),
            project_roots: keep_roots(&config.project_roots, uid),
            protected: Vec::new(),
            protect: config.protect.clone(),
            live: sources.live.clone(),
            mounts,
            entries: Vec::new(),
        };
        // /proc reports real paths, so a source given through a link (a unit's WorkingDirectory under a linked
        // /home, say) is matched in its real form too. Live processes come from /proc and are real already.
        let known: HashSet<PathBuf> = p.live.paths.iter().cloned().collect();
        for (path, why) in sources.entries {
            if why == LIVE {
                if !known.contains(&path) {
                    p.live.paths.push(path);
                }
                continue;
            }
            if !path.is_absolute() {
                continue;
            }
            if let Ok(real) = fs::canonicalize(&path)
                && real != path
            {
                p.entries.push((real, why));
            }
            p.entries.push((path, why));
        }
        p.rebuild();
        p
    }

    /// Work out the anchored folders from the sources and the live processes.
    fn rebuild(&mut self) {
        let mut protected = Vec::new();
        let mut seen = HashSet::new();
        let live = self.live.paths.iter().map(|p| (p, LIVE));
        for (path, why) in self.entries.iter().map(|(p, w)| (p, *w)).chain(live) {
            if !path.is_absolute() {
                continue;
            }
            if let Some(anchor) = self.anchor(path)
                && seen.insert(anchor.clone())
            {
                protected.push((anchor, why));
            }
        }
        for path in &self.protect {
            protected.push((path.clone(), "PROTECT"));
        }
        self.protected = protected;
    }

    /// Look at live processes and mounts again (cheap), keeping the other sources as they were gathered.
    pub fn refresh_live(&mut self, live: Live, mounts: HashSet<PathBuf>) {
        self.live = live;
        self.mounts = mounts;
        self.rebuild();
    }

    /// The top-level folder under the deepest root that holds `path`; None when `path` is under no root (or is one).
    pub fn anchor(&self, path: &Path) -> Option<PathBuf> {
        let best = self
            .scratch_roots
            .iter()
            .chain(self.project_roots.iter())
            .filter(|r| path != r.as_path() && path.starts_with(r))
            .max_by_key(|r| r.components().count())?;
        let first = path.strip_prefix(best).ok()?.components().next()?;
        Some(best.join(first))
    }

    /// Why a path may not be touched, if anything protects it: equal to, inside, or around a protected folder.
    pub fn protected_by(&self, path: &Path) -> Option<String> {
        self.protected
            .iter()
            .find(|(x, _)| path == x || path.starts_with(x) || x.starts_with(path))
            .map(|(x, why)| format!("protected ({why}: {})", x.display()))
    }

    /// A top-level dot-folder of the home that is not a scratch root holds tools, whose node_modules are global
    /// installs.
    pub fn in_tool_home(&self, path: &Path) -> bool {
        self.anchor(path).is_some_and(|a| {
            a.parent() == Some(self.home.as_path())
                && a.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with('.'))
        })
    }

    /// A live process's path inside this one (cwd, executable, open file), if any.
    pub fn live_inside(&self, path: &Path) -> Option<PathBuf> {
        self.live
            .paths
            .iter()
            .find(|p| p.starts_with(path))
            .cloned()
    }

    /// Whether a live process runs this file, or something inside this folder.
    pub fn exe_in_use(&self, path: &Path) -> bool {
        self.live.exes.iter().any(|e| e.starts_with(path))
    }

    pub fn in_scratch(&self, path: &Path) -> bool {
        self.scratch_roots
            .iter()
            .any(|r| path != r && path.starts_with(r))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prot(
        home: &Path,
        scratch: Vec<PathBuf>,
        projects: Vec<PathBuf>,
        entries: Vec<(PathBuf, &'static str)>,
    ) -> Protections {
        let mut config = Config::defaults(home, "box");
        config.scratch_roots = scratch;
        config.project_roots = projects;
        let sources = Sources {
            entries,
            live: Live::default(),
        };
        Protections::new(&config, platform::euid(), HashSet::new(), sources)
    }

    #[test]
    fn anchors_pick_the_deepest_root() {
        let t = crate::testutil::TempDir::new("anchor");
        let home = t.path().to_path_buf();
        fs::create_dir_all(home.join(".cache/x")).unwrap();
        let p = prot(&home, vec![home.join(".cache")], vec![home.clone()], vec![]);
        assert_eq!(
            p.anchor(&home.join(".cache/x/y/z")),
            Some(home.join(".cache/x"))
        );
        assert_eq!(
            p.anchor(&home.join("work/repo/target")),
            Some(home.join("work"))
        );
        assert_eq!(p.anchor(&home), None, "a root itself has no anchor");
        assert_eq!(p.anchor(Path::new("/elsewhere/x")), None);
        assert!(p.in_tool_home(&home.join(".bun/install/node_modules")));
        assert!(!p.in_tool_home(&home.join(".cache/x/node_modules")));
        assert!(!p.in_tool_home(&home.join("work/node_modules")));
    }

    #[test]
    fn a_live_path_protects_its_whole_top_level_folder() {
        let t = crate::testutil::TempDir::new("live");
        let home = t.path().to_path_buf();
        let p = prot(
            &home,
            vec![],
            vec![home.clone()],
            vec![(home.join("work/app/src/main.rs"), "live process")],
        );
        let why = p.protected_by(&home.join("work/other/target")).unwrap();
        assert!(why.contains("live process"), "{why}");
        assert!(p.protected_by(&home).is_some(), "around it");
        assert_eq!(p.protected_by(&home.join("play/target")), None);
    }

    #[test]
    fn parsers_for_pm2_docker_incus_and_lima() {
        let pm2 = r#"[{"name":"a","pm2_env":{"pm_cwd":"/srv/a","pm_exec_path":"/srv/a/index.js"}},{"name":"b"}]"#;
        assert_eq!(
            parse_pm2_jlist(pm2),
            vec![PathBuf::from("/srv/a"), PathBuf::from("/srv/a/index.js")]
        );
        let docker = r#"[{"Mounts":[{"Type":"bind","Source":"/srv/data"},{"Type":"volume","Source":"/var/lib/docker/volumes/v"}]}]"#;
        assert_eq!(
            parse_docker_inspect(docker),
            vec![PathBuf::from("/srv/data")]
        );
        let incus = r#"[{"expanded_devices":{"root":{"type":"disk","path":"/","pool":"default"},"w":{"type":"disk","source":"/srv/w","path":"/w"},"eth0":{"type":"nic"}}}]"#;
        assert_eq!(parse_incus_instances(incus), vec![PathBuf::from("/srv/w")]);
        let lima = "mounts:\n- location: \"~\"\n- location: /tmp/lima\n  writable: true\n";
        assert_eq!(
            parse_lima_yaml(lima, Path::new("/Users/acme")),
            vec![PathBuf::from("/Users/acme"), PathBuf::from("/tmp/lima")]
        );
        assert!(parse_pm2_jlist("not json").is_empty());
    }
}
