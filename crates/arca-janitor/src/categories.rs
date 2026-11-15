//! The clean categories: what each one picks, and the guards each pick must pass.
//!
//! Linux: caches, claude-versions, arca-backups, docker, runner-caches, build-outputs, stale-scratch.
//! macOS: caches, xcode, docker, claude-versions, app-caches, logs, build-outputs, stale-scratch.
//! Methods: a tool's own command first; else only the children of a fixed cache folder; a cache is skipped while its
//! tool runs; build outputs only in idle projects; scratch only when untouched, and moved to trash, not deleted.
//! Never documents, mail, backups, archives, app data, containers or volumes. The macOS Trash and Time Machine's local
//! snapshots are left to a person (one is theirs, the other needs sudo).

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::jobs::clean::{Action, Env, Guard, Item, Origin, Skip, origin_of};
use crate::jobs::{Profile, Stop};
use crate::platform::is_macos;
use crate::util::{human, on_path, run_cmd};
use crate::walk::{Visit, Walk, mtime, usage};

const LINUX: [&str; 7] = [
    "caches",
    "claude-versions",
    "arca-backups",
    "docker",
    "runner-caches",
    "build-outputs",
    "stale-scratch",
];
const MACOS: [&str; 8] = [
    "caches",
    "xcode",
    "docker",
    "claude-versions",
    "app-caches",
    "logs",
    "build-outputs",
    "stale-scratch",
];

/// The categories of this system, in the order they run.
pub fn names() -> Vec<&'static str> {
    if is_macos() {
        MACOS.to_vec()
    } else {
        LINUX.to_vec()
    }
}

#[derive(Default)]
pub struct Found {
    pub items: Vec<Item>,
    pub skipped: Vec<Skip>,
}

impl Found {
    fn skip(&mut self, category: &str, label: &str, bytes: u64, reason: impl Into<String>) {
        self.skipped.push(Skip {
            category: category.to_string(),
            label: label.to_string(),
            bytes,
            reason: reason.into(),
        });
    }
}

fn item(
    category: &str,
    action: Action,
    bytes: u64,
    reason: String,
    guards: Vec<Guard>,
    origin: Origin,
) -> Item {
    Item {
        id: 0,
        category: category.to_string(),
        action,
        bytes,
        reason,
        guards,
        origin,
    }
}

fn simple_origin(pattern: &str, creator: Option<&str>) -> Origin {
    Origin {
        pattern: pattern.to_string(),
        creator: creator.map(str::to_string),
        ..Origin::default()
    }
}

fn owners(names: &[&str]) -> Guard {
    Guard::OwnersIdle {
        names: names.iter().map(|s| s.to_string()).collect(),
    }
}

fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(|s| s.to_string()).collect()
}

fn size(env: &Env, path: &Path) -> Result<u64, Stop> {
    usage(path, env.mounts(), env.ctx)
}

fn is_real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
}

pub fn run(name: &str, env: &Env) -> Result<Found, Stop> {
    let mut found = Found::default();
    match name {
        "caches" if is_macos() => mac_caches(env, &mut found)?,
        "caches" => linux_caches(env, &mut found)?,
        "claude-versions" => claude_versions(env, &mut found)?,
        "arca-backups" => arca_backups(env, &mut found)?,
        "docker" => docker(env, &mut found),
        "runner-caches" => runner_caches(env, &mut found),
        "build-outputs" => build_outputs(env, &mut found)?,
        "stale-scratch" => stale_scratch(env, &mut found)?,
        "xcode" => xcode(env, &mut found)?,
        "app-caches" => app_caches(env, &mut found)?,
        "logs" => logs(env, &mut found)?,
        _ => {}
    }
    Ok(found)
}

// ------------------------------------------------------------------------------------------------ caches

/// A tool's clean command, sized by its cache folder when it has one; daily runs keep a cache under its cap warm.
#[allow(clippy::too_many_arguments)]
fn tool_command(
    env: &Env,
    found: &mut Found,
    label: &str,
    program: &str,
    words: &[&str],
    cache: Option<PathBuf>,
    max_mb: Option<u64>,
    owner_names: &[&str],
) -> Result<(), Stop> {
    if !on_path(program) {
        return Ok(());
    }
    let bytes = match &cache {
        Some(dir) if is_real_dir(dir) => size(env, dir)?,
        Some(_) if max_mb.is_some() => return Ok(()),
        _ => 0,
    };
    if let Some(max) = max_mb
        && env.profile == Profile::Daily
        && bytes < max * 1_048_576
    {
        found.skip("caches", label, bytes, format!("under {max} MB, kept warm"));
        return Ok(());
    }
    found.items.push(item(
        "caches",
        Action::Command { argv: argv(words) },
        bytes,
        format!("{label}: refilled by {program} on the next install or build"),
        vec![
            Guard::Tool {
                program: program.to_string(),
            },
            owners(owner_names),
        ],
        simple_origin("tool-cache", Some(program)),
    ));
    Ok(())
}

/// Empty a fixed cache folder (its children only), unless its tool runs.
fn cache_folder(
    env: &Env,
    found: &mut Found,
    category: &str,
    label: &str,
    dir: PathBuf,
    max_mb: Option<u64>,
    owner_names: &[&str],
) -> Result<(), Stop> {
    if !is_real_dir(&dir) {
        return Ok(());
    }
    let bytes = size(env, &dir)?;
    let own = fs::symlink_metadata(&dir)
        .map(|m| m.blocks() * 512)
        .unwrap_or(0);
    if bytes <= own {
        return Ok(());
    }
    if let Some(max) = max_mb
        && env.profile == Profile::Daily
        && bytes < max * 1_048_576
    {
        found.skip(category, label, bytes, format!("under {max} MB, kept warm"));
        return Ok(());
    }
    found.items.push(item(
        category,
        Action::EmptyDir { path: dir },
        bytes,
        format!("{label}: emptied, folder kept; its tool refills it"),
        vec![owners(owner_names)],
        simple_origin("tool-cache", owner_names.first().copied()),
    ));
    Ok(())
}

fn linux_caches(env: &Env, found: &mut Found) -> Result<(), Stop> {
    let c = env.config;
    let home = &c.home;
    tool_command(
        env,
        found,
        "npm",
        "npm",
        &["npm", "cache", "clean", "--force"],
        Some(home.join(".npm/_cacache")),
        Some(c.npm_max_mb),
        &["npm", "npx"],
    )?;
    tool_command(
        env,
        found,
        "pnpm",
        "pnpm",
        &["pnpm", "store", "prune"],
        None,
        None,
        &["pnpm"],
    )?;
    tool_command(
        env,
        found,
        "uv",
        "uv",
        &["uv", "cache", "prune"],
        None,
        None,
        &["uv", "uvx"],
    )?;
    cache_folder(
        env,
        found,
        "caches",
        "go-build",
        home.join(".cache/go-build"),
        Some(c.gobuild_max_mb),
        &["go", "gopls"],
    )?;
    cache_folder(
        env,
        found,
        "caches",
        "node-gyp",
        home.join(".cache/node-gyp"),
        Some(c.nodegyp_max_mb),
        &["node-gyp"],
    )?;
    cache_folder(
        env,
        found,
        "caches",
        "cargo downloads",
        home.join(".cargo/registry/cache"),
        Some(c.cargo_max_mb),
        &["cargo", "rustc"],
    )
}

fn mac_caches(env: &Env, found: &mut Found) -> Result<(), Stop> {
    let c = env.config;
    let lib = c.home.join("Library/Caches");
    tool_command(
        env,
        found,
        "Homebrew old downloads",
        "brew",
        &["brew", "cleanup", "--prune=30"],
        Some(lib.join("Homebrew")),
        None,
        &["brew"],
    )?;
    tool_command(
        env,
        found,
        "npm",
        "npm",
        &["npm", "cache", "clean", "--force"],
        Some(c.home.join(".npm/_cacache")),
        Some(c.npm_max_mb),
        &["npm", "npx"],
    )?;
    tool_command(
        env,
        found,
        "pnpm",
        "pnpm",
        &["pnpm", "store", "prune"],
        None,
        None,
        &["pnpm"],
    )?;
    tool_command(
        env,
        found,
        "Yarn",
        "yarn",
        &["yarn", "cache", "clean"],
        Some(lib.join("Yarn")),
        None,
        &["yarn"],
    )?;
    tool_command(
        env,
        found,
        "pip",
        "pip3",
        &["pip3", "cache", "purge"],
        Some(lib.join("pip")),
        None,
        &["pip", "pip3", "python3"],
    )?;
    tool_command(
        env,
        found,
        "uv",
        "uv",
        &["uv", "cache", "prune"],
        None,
        None,
        &["uv", "uvx"],
    )?;
    tool_command(
        env,
        found,
        "Go build cache",
        "go",
        &["go", "clean", "-cache"],
        Some(lib.join("go-build")),
        Some(c.gobuild_max_mb),
        &["go", "gopls"],
    )?;
    tool_command(
        env,
        found,
        "CocoaPods",
        "pod",
        &["pod", "cache", "clean", "--all"],
        Some(lib.join("CocoaPods")),
        None,
        &["pod"],
    )?;
    cache_folder(
        env,
        found,
        "caches",
        "cargo downloads",
        c.home.join(".cargo/registry/cache"),
        Some(c.cargo_max_mb),
        &["cargo", "rustc"],
    )?;
    cache_folder(
        env,
        found,
        "caches",
        "node-gyp headers",
        lib.join("node-gyp"),
        Some(c.nodegyp_max_mb),
        &["node-gyp"],
    )
}

fn xcode(env: &Env, found: &mut Found) -> Result<(), Stop> {
    let dev = env.config.home.join("Library/Developer");
    if !is_real_dir(&dev) {
        return Ok(());
    }
    cache_folder(
        env,
        found,
        "xcode",
        "Xcode DerivedData",
        dev.join("Xcode/DerivedData"),
        None,
        &["Xcode"],
    )?;
    if on_path("xcrun") {
        found.items.push(item(
            "xcode",
            Action::Command {
                argv: argv(&["xcrun", "simctl", "delete", "unavailable"]),
            },
            0,
            "simulators for removed runtimes".to_string(),
            vec![
                Guard::Tool {
                    program: "xcrun".into(),
                },
                owners(&["Simulator"]),
            ],
            simple_origin("tool-cache", Some("xcrun")),
        ));
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------------ old builds

fn entries_by_mtime(dir: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<(u64, PathBuf)> = fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let md = fs::symlink_metadata(e.path()).ok()?;
                    Some((mtime(&md), e.path()))
                })
                .collect()
        })
        .unwrap_or_default();
    entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    entries.into_iter().map(|(_, p)| p).collect()
}

/// Old Claude Code builds: keeps the active one, the newest other, and any a live process runs.
fn claude_versions(env: &Env, found: &mut Found) -> Result<(), Stop> {
    let home = &env.config.home;
    let dir = home.join(".local/share/claude/versions");
    let link = home.join(".local/bin/claude");
    let is_link = fs::symlink_metadata(&link).is_ok_and(|m| m.file_type().is_symlink());
    if !is_real_dir(&dir) || !is_link {
        return Ok(());
    }
    let Ok(active) = fs::canonicalize(&link) else {
        return Ok(());
    };
    let mut others = 0;
    for path in entries_by_mtime(&dir) {
        if fs::canonicalize(&path).is_ok_and(|p| p == active) || active.starts_with(&path) {
            continue;
        }
        others += 1;
        if others == 1 {
            continue;
        }
        let bytes = size(env, &path)?;
        found.items.push(item(
            "claude-versions",
            Action::Remove { path },
            bytes,
            "old Claude Code build (the active one and the newest other are kept)".to_string(),
            vec![Guard::NotRunning],
            simple_origin("old-build", Some("claude")),
        ));
    }
    Ok(())
}

/// `~/.local/bin/arca.bak-*`: keeps the newest two and any running.
fn arca_backups(env: &Env, found: &mut Found) -> Result<(), Stop> {
    let dir = env.config.home.join(".local/bin");
    let mut backups: Vec<PathBuf> = fs::read_dir(&dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("arca.bak-"))
                        && fs::symlink_metadata(p).is_ok_and(|m| m.is_file())
                })
                .collect()
        })
        .unwrap_or_default();
    backups.sort();
    backups.reverse();
    for path in backups.into_iter().skip(2) {
        let bytes = size(env, &path)?;
        found.items.push(item(
            "arca-backups",
            Action::Remove { path },
            bytes,
            "old arca binary backup (the newest two are kept)".to_string(),
            vec![Guard::NotRunning],
            simple_origin("old-build", Some("arca")),
        ));
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------------ docker

fn docker_ok() -> bool {
    on_path("docker")
        && run_cmd(&["docker", "info"], Duration::from_secs(20)).is_some_and(|o| o.success)
}

/// Untagged images and build cache older than 7 days. Never containers or volumes in use.
fn docker(_env: &Env, found: &mut Found) {
    if !docker_ok() {
        found.skip("docker", "docker", 0, "not reachable for this user");
        return;
    }
    let untagged = run_cmd(
        &["docker", "images", "-qf", "dangling=true"],
        Duration::from_secs(30),
    )
    .map(|o| o.stdout.lines().count())
    .unwrap_or(0);
    let guards = vec![Guard::Tool {
        program: "docker".into(),
    }];
    found.items.push(item(
        "docker",
        Action::Command {
            argv: argv(&["docker", "image", "prune", "-f"]),
        },
        0,
        format!("{untagged} untagged images"),
        guards.clone(),
        simple_origin("docker", Some("docker build")),
    ));
    found.items.push(item(
        "docker",
        Action::Command {
            argv: argv(&["docker", "builder", "prune", "-f", "--filter", "until=168h"]),
        },
        0,
        "build cache older than 7 days".to_string(),
        guards,
        simple_origin("docker", Some("docker build")),
    ));
}

/// A GitLab Runner cache volume's name: `runner-<hex>-cache-<hex>` with an optional `-protected`.
pub fn is_runner_cache(name: &str) -> bool {
    let name = name.strip_suffix("-protected").unwrap_or(name);
    let Some(rest) = name.strip_prefix("runner-") else {
        return false;
    };
    let Some((a, b)) = rest.split_once("-cache-") else {
        return false;
    };
    let hex = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    };
    hex(a) && hex(b)
}

/// The day of the week of an epoch, 0 = Sunday (UTC).
pub fn weekday(epoch: u64) -> u64 {
    (epoch / 86_400 + 4) % 7
}

/// GitLab Runner cache volumes no container uses. Daily: Sundays only, since CI refills them.
fn runner_caches(env: &Env, found: &mut Found) {
    if !docker_ok() {
        found.skip("runner-caches", "docker", 0, "not reachable for this user");
        return;
    }
    if env.profile == Profile::Daily && weekday(env.now) != 0 {
        found.skip(
            "runner-caches",
            "runner caches",
            0,
            "daily runs do this on Sundays only",
        );
        return;
    }
    let volumes = run_cmd(
        &["docker", "volume", "ls", "-q", "--filter", "dangling=true"],
        Duration::from_secs(30),
    )
    .map(|o| o.stdout)
    .unwrap_or_default();
    for v in volumes
        .lines()
        .map(str::trim)
        .filter(|v| is_runner_cache(v))
    {
        found.items.push(item(
            "runner-caches",
            Action::Command {
                argv: argv(&["docker", "volume", "rm", v]),
            },
            0,
            "runner cache volume no container uses (size not measured)".to_string(),
            vec![Guard::Tool {
                program: "docker".into(),
            }],
            simple_origin("docker", Some("gitlab-runner")),
        ));
    }
}

// ------------------------------------------------------------------------------------------------ build outputs

const OUTPUT_NAMES: [&str; 5] = ["target", "node_modules", ".next", ".turbo", ".parcel-cache"];

/// The marker file that makes a folder of this name a build output: target/ beside a Cargo.toml, the rest beside a
/// package.json.
pub fn marker_for(name: &str) -> Option<&'static str> {
    match name {
        "target" => Some("Cargo.toml"),
        "node_modules" | ".next" | ".turbo" | ".parcel-cache" => Some("package.json"),
        _ => None,
    }
}

/// target/, node_modules, .next, .turbo and .parcel-cache, in projects nothing changed in for BUILD_IDLE_DAYS
/// (SCRATCH_BUILD_IDLE_DAYS in scratch roots).
fn build_outputs(env: &Env, found: &mut Found) -> Result<(), Stop> {
    let depth = usize::try_from(env.config.max_depth).unwrap_or(6);
    let mut candidates = BTreeSet::new();
    let roots: Vec<PathBuf> = env
        .prot
        .project_roots
        .iter()
        .chain(env.prot.scratch_roots.iter())
        .cloned()
        .collect();
    for root in &roots {
        Walk::new(env.mounts(), env.ctx)
            .depth(depth)
            .prune(&[".cleanup-trash", ".git"])
            .run(root, &mut |p, md, _| {
                if !md.is_dir() {
                    return Visit::Continue;
                }
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if OUTPUT_NAMES.contains(&name) {
                    candidates.insert(p.to_path_buf());
                    return Visit::Prune;
                }
                Visit::Continue
            })?;
    }
    for out in candidates {
        let name = out.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let (Some(marker), Some(project)) = (marker_for(name), out.parent()) else {
            continue;
        };
        if !fs::symlink_metadata(project.join(marker)).is_ok_and(|m| m.is_file()) {
            continue;
        }
        if fs::symlink_metadata(&out).map(|m| m.uid()).ok() != Some(env.uid()) {
            continue;
        }
        if env.prot.in_tool_home(&out) {
            continue;
        }
        let days = if env.prot.in_scratch(&out) {
            env.config.scratch_build_idle_days
        } else {
            env.config.build_idle_days
        };
        let guards = vec![
            Guard::NotToolHome,
            Guard::Protected,
            Guard::ProjectIdle {
                project: project.to_path_buf(),
                days,
            },
        ];
        let origin = origin_of(&out, "build-output", Some(builder(name)), &env.config.home);
        // Size only what will be planned: a protected or recent output is skipped before it is measured.
        let candidate = item(
            "build-outputs",
            Action::Remove { path: out.clone() },
            0,
            String::new(),
            guards,
            origin,
        );
        if let Err(reason) = crate::jobs::clean::check(&candidate, env) {
            found.skip("build-outputs", &out.display().to_string(), 0, reason);
            continue;
        }
        let bytes = size(env, &out)?;
        found.items.push(Item {
            bytes,
            reason: format!("build output, project idle {days} days"),
            ..candidate
        });
    }
    Ok(())
}

fn builder(name: &str) -> &'static str {
    match name {
        "target" => "cargo",
        "node_modules" => "npm/pnpm/bun install",
        ".next" => "next build",
        ".turbo" => "turbo",
        _ => "parcel",
    }
}

// ------------------------------------------------------------------------------------------------ scratch

/// Trash day folders older than TRASH_DAYS, in every scratch root.
pub fn old_trash(env: &Env) -> Result<Vec<Item>, Stop> {
    let mut items = Vec::new();
    for root in &env.prot.scratch_roots {
        let trash = root.join(".cleanup-trash");
        if !is_real_dir(&trash) {
            continue;
        }
        let mut days: Vec<PathBuf> = fs::read_dir(&trash)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| is_real_dir(p))
                    .collect()
            })
            .unwrap_or_default();
        days.sort();
        for day in days {
            let name = day.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let Some(when) = crate::util::parse_date(name) else {
                continue;
            };
            if env.now.saturating_sub(when) / 86_400 < env.config.trash_days {
                continue;
            }
            let bytes = size(env, &day)?;
            items.push(item(
                "stale-scratch",
                Action::Remove { path: day },
                bytes,
                format!("trash older than {} days", env.config.trash_days),
                vec![Guard::TrashOlderThan {
                    days: env.config.trash_days,
                }],
                simple_origin("trash", None),
            ));
        }
    }
    Ok(items)
}

/// A scratch root's top-level folder untouched for STALE_DAYS moves to `<root>/.cleanup-trash/<date>/`; trash older
/// than TRASH_DAYS is deleted.
fn stale_scratch(env: &Env, found: &mut Found) -> Result<(), Stop> {
    found.items.extend(old_trash(env)?);
    for root in &env.prot.scratch_roots {
        let trash = root.join(".cleanup-trash");
        let mut dirs: Vec<PathBuf> = fs::read_dir(root)
            .map(|rd| rd.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        dirs.sort();
        for d in dirs {
            let Ok(md) = fs::symlink_metadata(&d) else {
                continue;
            };
            if !md.is_dir() || d == trash {
                continue;
            }
            let guards = vec![
                Guard::NotCacheKeep,
                Guard::Protected,
                Guard::Untouched {
                    days: env.config.stale_days,
                },
            ];
            let candidate = item(
                "stale-scratch",
                Action::Quarantine {
                    path: d.clone(),
                    trash: trash.clone(),
                },
                0,
                String::new(),
                guards,
                origin_of(&d.join("x"), "scratch", None, &env.config.home),
            );
            if let Err(reason) = crate::jobs::clean::check(&candidate, env) {
                found.skip("stale-scratch", &d.display().to_string(), 0, reason);
                continue;
            }
            let bytes = size(env, &d)?;
            found.items.push(Item {
                bytes,
                reason: format!(
                    "scratch untouched {} days; moved to trash, deleted after {} more",
                    env.config.stale_days, env.config.trash_days
                ),
                ..candidate
            });
        }
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------------ macOS app caches and logs

const APP_CACHE_KEEP: [&str; 7] = [
    "CloudKit",
    "Homebrew",
    "Yarn",
    "pip",
    "go-build",
    "CocoaPods",
    "node-gyp",
];

/// ~/Library/Caches folders untouched for 30 days and over 10 MB; Apple's own and the tool caches above are left.
fn app_caches(env: &Env, found: &mut Found) -> Result<(), Stop> {
    let root = env.config.home.join("Library/Caches");
    let mut dirs: Vec<PathBuf> = fs::read_dir(&root)
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    dirs.sort();
    for d in dirs {
        let name = d
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        if name.starts_with("com.apple.") || APP_CACHE_KEEP.contains(&name.as_str()) {
            continue;
        }
        if fs::symlink_metadata(&d).is_ok_and(|m| m.file_type().is_symlink()) {
            continue;
        }
        let candidate = item(
            "app-caches",
            Action::Remove { path: d.clone() },
            0,
            String::new(),
            vec![Guard::NotLive, Guard::Untouched { days: 30 }],
            simple_origin("app-cache", Some(&name)),
        );
        if let Err(reason) = crate::jobs::clean::check(&candidate, env) {
            found.skip("app-caches", &d.display().to_string(), 0, reason);
            continue;
        }
        let bytes = size(env, &d)?;
        if bytes <= 10 * 1_048_576 {
            continue;
        }
        found.items.push(Item {
            bytes,
            reason: format!("app cache untouched 30 days ({})", human(bytes)),
            ..candidate
        });
    }
    Ok(())
}

/// Files in ~/Library/Logs older than 30 days.
fn logs(env: &Env, found: &mut Found) -> Result<(), Stop> {
    let root = env.config.home.join("Library/Logs");
    let cutoff = env.now.saturating_sub(30 * 86_400);
    let mut files = Vec::new();
    Walk::new(env.mounts(), env.ctx).run(&root, &mut |p, md, _| {
        if md.is_file() && mtime(md) <= cutoff {
            files.push((p.to_path_buf(), md.blocks() * 512));
        }
        Visit::Continue
    })?;
    for (path, bytes) in files {
        found.items.push(item(
            "logs",
            Action::Remove { path },
            bytes,
            "log file older than 30 days".to_string(),
            vec![Guard::NotLive, Guard::Untouched { days: 30 }],
            simple_origin("log", None),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runner_cache_names() {
        assert!(is_runner_cache("runner-abc123-cache-0f0f"));
        assert!(is_runner_cache("runner-abc123-cache-0f0f-protected"));
        assert!(!is_runner_cache("runner-abc123-cache-"));
        assert!(!is_runner_cache("myapp-data"));
        assert!(!is_runner_cache("runner-XYZ-cache-00"));
    }

    #[test]
    fn weekdays_and_markers() {
        assert_eq!(weekday(0), 4, "1970-01-01 was a Thursday");
        assert_eq!(weekday(3 * 86_400), 0, "1970-01-04 was a Sunday");
        assert_eq!(marker_for("target"), Some("Cargo.toml"));
        assert_eq!(marker_for(".turbo"), Some("package.json"));
        assert_eq!(marker_for("dist"), None);
    }
}
