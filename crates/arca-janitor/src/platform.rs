//! What differs between Linux and macOS: priority, pressure, mounts, live processes and services.
//!
//! No foreign calls: Linux is read from /proc, macOS from its own tools (ps, lsof, sysctl, pgrep). All of it is compiled
//! on both systems, so the macOS parsers are tested on Linux too; `is_macos()` picks the source at run time.

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::util::{on_path, run_cmd};

const TOOL_TIMEOUT: Duration = Duration::from_secs(20);

pub fn is_macos() -> bool {
    cfg!(target_os = "macos")
}

/// The effective uid of this process: /proc/self/status on Linux, `id -u` elsewhere.
pub fn euid() -> u32 {
    if let Ok(status) = fs::read_to_string("/proc/self/status")
        && let Some(uid) = parse_status_euid(&status)
    {
        return uid;
    }
    run_cmd(&["id", "-u"], TOOL_TIMEOUT)
        .and_then(|out| out.stdout.trim().parse().ok())
        .unwrap_or(u32::MAX)
}

/// The effective uid in /proc/<pid>/status: the second number of the `Uid:` line.
pub fn parse_status_euid(status: &str) -> Option<u32> {
    let line = status.lines().find(|l| l.starts_with("Uid:"))?;
    line.split_whitespace().nth(2)?.parse().ok()
}

/// The host's short name: JANITOR_HOSTNAME, else /proc/sys/kernel/hostname, else `hostname -s`.
pub fn hostname() -> String {
    let full = fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .or_else(|| run_cmd(&["hostname", "-s"], TOOL_TIMEOUT).map(|o| o.stdout))
        .unwrap_or_default();
    let short = full.trim().split('.').next().unwrap_or("").to_string();
    if short.is_empty() {
        "localhost".to_string()
    } else {
        short
    }
}

// ------------------------------------------------------------------------------------------------ priority

/// The env var that marks a daemon already re-executed to lower itself, so it never loops.
pub const REEXEC_GUARD: &str = "JANITOR_PRIORITY_REEXEC";

/// How the daemon's CPU and IO priority were read back.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Priority {
    /// The nice value, when it could be read.
    pub nice: Option<i32>,
    /// The IO class (`idle`, `best-effort`, `none`, `realtime`; on macOS `background` when started under taskpolicy -b).
    pub io: Option<String>,
    /// Whether the process is at nice 19 with idle IO: the only state heavy jobs run in.
    pub lowered: bool,
    /// How it was read, for the status line.
    pub detail: String,
}

/// The nice value from /proc/<pid>/stat: field 19, counted after the command name in parentheses (which may itself
/// hold spaces and parentheses).
pub fn parse_stat_nice(stat: &str) -> Option<i32> {
    let after = &stat[stat.rfind(')')? + 1..];
    // After ')' come fields 3 (state) onwards; nice is field 19.
    after.split_whitespace().nth(16)?.parse().ok()
}

/// The class `ionice -p` prints: `idle`, `none: prio 0`, `best-effort: prio 4`, `realtime: prio 0`.
pub fn parse_ionice(out: &str) -> Option<String> {
    let first = out.lines().next()?.trim();
    let class = first.split(':').next()?.trim();
    if class.is_empty() {
        None
    } else {
        Some(class.to_string())
    }
}

/// Whether a read-back counts as lowered.
pub fn judge(nice: Option<i32>, io: Option<&str>) -> bool {
    nice.is_some_and(|n| n >= 19) && matches!(io, Some("idle") | Some("background"))
}

/// Read back this process's priority.
pub fn read_priority() -> Priority {
    let pid = std::process::id().to_string();
    if is_macos() {
        let nice = run_cmd(&["ps", "-o", "nice=", "-p", &pid], TOOL_TIMEOUT)
            .and_then(|o| o.stdout.trim().parse().ok());
        // taskpolicy has no read-back that prints the policy; the re-exec records that it ran under -b.
        let io = (std::env::var(REEXEC_GUARD).as_deref() == Ok("taskpolicy"))
            .then(|| "background".to_string());
        let lowered = judge(nice, io.as_deref());
        return Priority {
            nice,
            io,
            lowered,
            detail: "ps -o nice; background from the taskpolicy -b re-exec".to_string(),
        };
    }
    let nice = fs::read_to_string("/proc/self/stat")
        .ok()
        .and_then(|s| parse_stat_nice(&s));
    let io = run_cmd(&["ionice", "-p", &pid], TOOL_TIMEOUT)
        .filter(|o| o.success)
        .and_then(|o| parse_ionice(&o.stdout));
    let lowered = judge(nice, io.as_deref());
    Priority {
        nice,
        io,
        lowered,
        detail: "/proc/self/stat and ionice -p".to_string(),
    }
}

/// The command that re-executes the daemon lowered, or None when it must not (already lowered, already re-executed,
/// or the tools are missing).
pub fn reexec_command(
    current: &Priority,
    guard_set: bool,
    macos: bool,
    have: &dyn Fn(&str) -> bool,
    exe: &str,
    args: &[String],
) -> Option<Vec<String>> {
    if current.lowered || guard_set {
        return None;
    }
    let mut argv: Vec<String> = if macos {
        if !(have("taskpolicy") && have("nice")) {
            return None;
        }
        vec!["taskpolicy".into(), "-b".into()]
    } else {
        if !(have("ionice") && have("nice")) {
            return None;
        }
        vec!["ionice".into(), "-c".into(), "3".into()]
    };
    argv.extend(["nice".to_string(), "-n".to_string(), "19".to_string()]);
    argv.push(exe.to_string());
    argv.extend(args.iter().cloned());
    Some(argv)
}

/// Lower the whole daemon once at start: re-exec through the system's tools unless already lowered. Returns only when
/// no re-exec happened (or it failed), with the priority read back.
pub fn ensure_lowered(args: &[String]) -> Priority {
    let current = read_priority();
    let guard_set = std::env::var_os(REEXEC_GUARD).is_some();
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    if let Some(argv) = reexec_command(&current, guard_set, is_macos(), &on_path, &exe, args) {
        use std::os::unix::process::CommandExt;
        let guard = if is_macos() { "taskpolicy" } else { "ionice" };
        let err = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .env(REEXEC_GUARD, guard)
            .exec();
        eprintln!("arca-janitor: could not re-exec lowered ({err}); heavy jobs will be refused");
    }
    current
}

// ------------------------------------------------------------------------------------------------ pressure

/// `some avg10=` of a PSI file, in percent.
pub fn parse_psi_avg10(text: &str) -> Option<f64> {
    let line = text.lines().find(|l| l.starts_with("some"))?;
    let field = line.split_whitespace().find(|f| f.starts_with("avg10="))?;
    field["avg10=".len()..].parse().ok()
}

/// The one-minute load of `/proc/loadavg` (`0.52 0.58 ...`) or `sysctl -n vm.loadavg` (`{ 1.23 1.10 1.00 }`).
pub fn parse_loadavg(text: &str) -> Option<f64> {
    text.split_whitespace()
        .find(|w| *w != "{")
        .and_then(|w| w.parse().ok())
}

/// What the throttle reads: IO pressure in percent (Linux PSI) or, where PSI is missing, the one-minute load per CPU
/// in percent.
#[derive(Clone, Debug, PartialEq)]
pub struct Pressure {
    pub percent: f64,
    pub source: &'static str,
}

pub fn pressure(psi_path: &Path) -> Option<Pressure> {
    if let Some(percent) = fs::read_to_string(psi_path)
        .ok()
        .and_then(|t| parse_psi_avg10(&t))
    {
        return Some(Pressure {
            percent,
            source: "io pressure avg10",
        });
    }
    let load = if is_macos() {
        run_cmd(&["sysctl", "-n", "vm.loadavg"], TOOL_TIMEOUT)
            .and_then(|o| parse_loadavg(&o.stdout))
    } else {
        fs::read_to_string("/proc/loadavg")
            .ok()
            .and_then(|t| parse_loadavg(&t))
    }?;
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get()) as f64;
    Some(Pressure {
        percent: load / cpus * 100.0,
        source: "load per cpu",
    })
}

// ------------------------------------------------------------------------------------------------ mounts

/// Undo the octal escapes of /proc/self/mountinfo (`\040` is a space).
fn unescape_mount(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            let digits = &bytes[i + 1..i + 4];
            if digits.iter().all(|d| (b'0'..=b'7').contains(d)) {
                let v = digits
                    .iter()
                    .fold(0u32, |acc, d| acc * 8 + u32::from(d - b'0'));
                if let Ok(v) = u8::try_from(v) {
                    out.push(v);
                    i += 4;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// The mount points in a mountinfo text (field 5).
pub fn parse_mountinfo(text: &str) -> HashSet<PathBuf> {
    text.lines()
        .filter_map(|l| l.split_whitespace().nth(4))
        .map(|f| PathBuf::from(unescape_mount(f)))
        .collect()
}

/// Every mount point this process sees (Linux); empty elsewhere, where a device change marks a mount.
pub fn mount_points() -> HashSet<PathBuf> {
    fs::read_to_string("/proc/self/mountinfo")
        .map(|t| parse_mountinfo(&t))
        .unwrap_or_default()
}

/// Whether a folder is a mount point: listed as one, or on another device than its parent.
pub fn is_mount_point(path: &Path, mounts: &HashSet<PathBuf>) -> bool {
    if mounts.contains(path) {
        return true;
    }
    let (Ok(me), Some(parent)) = (fs::symlink_metadata(path), path.parent()) else {
        return false;
    };
    fs::symlink_metadata(parent).is_ok_and(|p| p.dev() != me.dev())
}

// ------------------------------------------------------------------------------------------------ processes

/// What this user's live processes hold: every path (cwd, executable, open file) and the executables alone.
#[derive(Clone, Debug, Default)]
pub struct Live {
    pub paths: Vec<PathBuf>,
    pub exes: Vec<PathBuf>,
}

fn strip_deleted(link: PathBuf) -> PathBuf {
    let text = link.to_string_lossy();
    match text.strip_suffix(" (deleted)") {
        Some(rest) => PathBuf::from(rest),
        None => link,
    }
}

/// This user's live processes, from /proc (Linux) or lsof (macOS).
pub fn live(uid: u32) -> Live {
    if is_macos() {
        let uid = uid.to_string();
        return run_cmd(&["lsof", "-w", "-u", &uid, "-Ffn"], Duration::from_secs(60))
            .map(|o| parse_lsof(&o.stdout))
            .unwrap_or_default();
    }
    let mut found = Live::default();
    let Ok(entries) = fs::read_dir("/proc") else {
        return found;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        let is_pid = dir
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()));
        if !is_pid || fs::metadata(&dir).map(|m| m.uid()).ok() != Some(uid) {
            continue;
        }
        if let Ok(cwd) = fs::read_link(dir.join("cwd")) {
            found.paths.push(strip_deleted(cwd));
        }
        if let Ok(exe) = fs::read_link(dir.join("exe")) {
            let exe = strip_deleted(exe);
            found.paths.push(exe.clone());
            found.exes.push(exe);
        }
        if let Ok(fds) = fs::read_dir(dir.join("fd")) {
            for fd in fds.flatten() {
                if let Ok(target) = fs::read_link(fd.path())
                    && target.is_absolute()
                {
                    found.paths.push(strip_deleted(target));
                }
            }
        }
    }
    found
}

/// lsof's field output (`-Ffn`): `p<pid>`, then pairs of `f<fd>` and `n<name>`; `ftxt` marks an executable or library.
pub fn parse_lsof(text: &str) -> Live {
    let mut found = Live::default();
    let mut fd = "";
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix('f') {
            fd = rest;
        } else if let Some(name) = line.strip_prefix('n')
            && name.starts_with('/')
        {
            let path = PathBuf::from(name);
            if fd == "txt" {
                found.exes.push(path.clone());
            }
            found.paths.push(path);
        }
    }
    found
}

/// The first of `names` this user runs, if any (pgrep -x -u).
pub fn running(uid: u32, names: &[&str]) -> Option<String> {
    if names.is_empty() {
        return None;
    }
    if is_macos() {
        let uid = uid.to_string();
        return names
            .iter()
            .find(|n| {
                run_cmd(&["pgrep", "-x", "-u", &uid, n], TOOL_TIMEOUT).is_some_and(|o| o.success)
            })
            .map(|n| n.to_string());
    }
    let entries = fs::read_dir("/proc").ok()?;
    for entry in entries.flatten() {
        let dir = entry.path();
        if fs::metadata(&dir).map(|m| m.uid()).ok() != Some(uid) {
            continue;
        }
        if let Ok(comm) = fs::read_to_string(dir.join("comm")) {
            let comm = comm.trim();
            // The kernel keeps 15 bytes of a command name.
            if let Some(n) = names.iter().find(|n| {
                let short = if n.len() > 15 { &n[..15] } else { n };
                comm == short
            }) {
                return Some(n.to_string());
            }
        }
    }
    None
}

// ------------------------------------------------------------------------------------------------ services

/// Absolute paths a line of a systemd unit names, for the keys that point at what a service runs or reads.
pub fn parse_unit_paths(text: &str) -> Vec<PathBuf> {
    const KEYS: [&str; 4] = [
        "WorkingDirectory=",
        "ExecStart=",
        "ExecStartPre=",
        "EnvironmentFile=",
    ];
    let mut found = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let Some(value) = KEYS.iter().find_map(|k| line.strip_prefix(k)) else {
            continue;
        };
        for word in value.split_whitespace() {
            let word = word
                .trim_start_matches(['-', '@', '!', '+', ':'])
                .trim_matches(['"', '\'', ';']);
            if word.starts_with('/') {
                found.push(PathBuf::from(word));
            }
        }
    }
    found
}

/// Absolute paths a launchd plist (XML) names in `WorkingDirectory` and `ProgramArguments`/`Program`.
pub fn parse_plist_paths(text: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut want = false;
    let mut in_array = false;
    for raw in text.split('<').skip(1) {
        let (tag, value) = raw.split_once('>').unwrap_or((raw, ""));
        match tag {
            "key" => {
                want = matches!(
                    value.trim(),
                    "WorkingDirectory" | "ProgramArguments" | "Program"
                );
            }
            "array" if want => in_array = true,
            "/array" => {
                in_array = false;
                want = false;
            }
            "string" if want => {
                let value = value.trim();
                if value.starts_with('/') {
                    found.push(PathBuf::from(value));
                }
                if !in_array {
                    want = false;
                }
            }
            _ => {}
        }
    }
    found
}

/// Paths the user's own services use: systemd user units (Linux) or LaunchAgents (macOS).
pub fn service_paths(home: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if is_macos() {
        let Ok(entries) = fs::read_dir(home.join("Library/LaunchAgents")) else {
            return found;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("plist") {
                continue;
            }
            let Ok(bytes) = fs::read(&path) else { continue };
            let text = if bytes.starts_with(b"bplist") {
                let p = path.display().to_string();
                match run_cmd(&["plutil", "-convert", "xml1", "-o", "-", &p], TOOL_TIMEOUT) {
                    Some(o) if o.success => o.stdout,
                    _ => continue,
                }
            } else {
                String::from_utf8_lossy(&bytes).to_string()
            };
            found.extend(parse_plist_paths(&text));
        }
        return found;
    }
    let Ok(entries) = fs::read_dir(home.join(".config/systemd/user")) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("service")
            && let Ok(text) = fs::read_to_string(&path)
        {
            found.extend(parse_unit_paths(&text));
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nice_is_read_after_a_command_name_with_spaces_and_parens() {
        let stat = "4242 (my (odd) cmd) S 1 4242 4242 0 -1 4194560 120 0 0 0 1 2 0 0 39 19 3 0 100 1000 200";
        assert_eq!(parse_stat_nice(stat), Some(19));
        let stat = "1 (init) S 0 1 1 0 -1 4194560 1 0 0 0 0 0 0 0 20 0 1 0 1 1 1";
        assert_eq!(parse_stat_nice(stat), Some(0));
        assert_eq!(parse_stat_nice("garbage"), None);
    }

    #[test]
    fn ionice_classes() {
        assert_eq!(parse_ionice("idle\n").as_deref(), Some("idle"));
        assert_eq!(parse_ionice("none: prio 0\n").as_deref(), Some("none"));
        assert_eq!(
            parse_ionice("best-effort: prio 7").as_deref(),
            Some("best-effort")
        );
        assert_eq!(parse_ionice(""), None);
        assert!(judge(Some(19), Some("idle")));
        assert!(!judge(Some(19), Some("none")));
        assert!(!judge(Some(10), Some("idle")));
        assert!(!judge(None, Some("idle")));
    }

    fn prio(lowered: bool) -> Priority {
        Priority {
            nice: Some(if lowered { 19 } else { 0 }),
            io: Some(if lowered { "idle" } else { "none" }.into()),
            lowered,
            detail: String::new(),
        }
    }

    #[test]
    fn the_reexec_happens_once_and_only_when_needed() {
        let all = |_: &str| true;
        let none = |_: &str| false;
        let args = vec!["serve".to_string(), "--foreground".to_string()];
        let argv = reexec_command(&prio(false), false, false, &all, "/x/jan", &args).unwrap();
        assert_eq!(
            argv.join(" "),
            "ionice -c 3 nice -n 19 /x/jan serve --foreground"
        );
        let argv = reexec_command(&prio(false), false, true, &all, "/x/jan", &args).unwrap();
        assert_eq!(
            argv.join(" "),
            "taskpolicy -b nice -n 19 /x/jan serve --foreground"
        );
        // The guard stops a second re-exec even when the first did not lower anything.
        assert_eq!(
            reexec_command(&prio(false), true, false, &all, "/x/jan", &args),
            None
        );
        assert_eq!(
            reexec_command(&prio(true), false, false, &all, "/x/jan", &args),
            None
        );
        assert_eq!(
            reexec_command(&prio(false), false, false, &none, "/x/jan", &args),
            None
        );
    }

    #[test]
    fn pressure_and_load_fixtures() {
        let psi = "some avg10=23.50 avg60=10.00 avg300=5.00 total=1\nfull avg10=1.00 avg60=0 avg300=0 total=1\n";
        assert_eq!(parse_psi_avg10(psi), Some(23.5));
        assert_eq!(parse_psi_avg10("nonsense"), None);
        assert_eq!(parse_loadavg("0.52 0.58 0.59 1/400 1234\n"), Some(0.52));
        assert_eq!(parse_loadavg("{ 1.23 1.10 1.00 }\n"), Some(1.23));
    }

    #[test]
    fn mountinfo_fixture_with_an_escaped_space() {
        let text = "22 1 0:21 / / rw - ext4 /dev/x rw\n30 22 0:30 / /mnt/my\\040disk rw - ext4 /dev/y rw\n";
        let mounts = parse_mountinfo(text);
        assert!(mounts.contains(Path::new("/")));
        assert!(mounts.contains(Path::new("/mnt/my disk")));
    }

    #[test]
    fn status_uid_lsof_units_and_plists() {
        assert_eq!(
            parse_status_euid("Name:\tx\nUid:\t1000\t1001\t1000\t1000\n"),
            Some(1001)
        );
        let live = parse_lsof(
            "p12\nfcwd\nn/work/app\nftxt\nn/opt/tool/bin/tool\nf3\nn/work/app/log.txt\nf4\nnTCP x\n",
        );
        assert_eq!(live.paths.len(), 3);
        assert_eq!(live.exes, vec![PathBuf::from("/opt/tool/bin/tool")]);
        let unit = "[Service]\nWorkingDirectory=/srv/app\nExecStart=-/usr/bin/env /srv/app/run.sh --x\nEnvironmentFile=-/srv/app/.env\nUser=x\n";
        assert_eq!(
            parse_unit_paths(unit),
            vec![
                PathBuf::from("/srv/app"),
                PathBuf::from("/usr/bin/env"),
                PathBuf::from("/srv/app/run.sh"),
                PathBuf::from("/srv/app/.env")
            ]
        );
        let plist = "<plist><dict><key>Label</key><string>com.acme.x</string>\
            <key>ProgramArguments</key><array><string>/Users/acme/bin/x</string><string>--flag</string></array>\
            <key>WorkingDirectory</key><string>/Users/acme/work</string>\
            <key>StandardOutPath</key><string>/tmp/out.log</string></dict></plist>";
        assert_eq!(
            parse_plist_paths(plist),
            vec![
                PathBuf::from("/Users/acme/bin/x"),
                PathBuf::from("/Users/acme/work")
            ]
        );
    }
}
