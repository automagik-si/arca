//! Small helpers: time, sizes, atomic files, and commands with a timeout.

use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Seconds since the epoch of a modification time.
pub fn secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// (year, month, day, hour, minute, second) in UTC.
pub fn civil(epoch: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (epoch / 86_400) as i64;
    let rem = epoch % 86_400;
    // Howard Hinnant's days-to-civil.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (
        y,
        m,
        d,
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
        (rem % 60) as u32,
    )
}

/// `2026-01-02` (UTC).
pub fn date(epoch: u64) -> String {
    let (y, m, d, ..) = civil(epoch);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `2026-01-02 03:04 UTC`.
pub fn stamp(epoch: u64) -> String {
    let (y, m, d, h, mi, _) = civil(epoch);
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02} UTC")
}

/// The epoch of a `YYYY-MM-DD` date (UTC midnight), if it is one.
pub fn parse_date(text: &str) -> Option<u64> {
    let parts: Vec<&str> = text.split('-').collect();
    if parts.len() != 3 || parts[0].len() != 4 || parts[1].len() != 2 || parts[2].len() != 2 {
        return None;
    }
    let y: i64 = parts[0].parse().ok()?;
    let m: i64 = parts[1].parse().ok()?;
    let d: i64 = parts[2].parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // Civil-to-days.
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days).ok().map(|d| d * 86_400)
}

/// A unique id: a prefix, the UTC time, and a counter.
pub fn new_id(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let (y, m, d, h, mi, s) = civil(now());
    format!(
        "{prefix}{y:04}{m:02}{d:02}-{h:02}{mi:02}{s:02}-{:x}{n:03}",
        std::process::id() % 0x1000
    )
}

/// Bytes in IEC units: `1.5 GiB`.
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Write a file by writing a sibling and renaming it, so a reader never sees half of it.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
}

/// Append one line to a file.
pub fn append_line(path: &Path, line: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(line.as_bytes())?;
    f.write_all(b"\n")
}

/// What a command did.
#[derive(Debug)]
pub struct CmdOutput {
    pub success: bool,
    /// The exit code; None when killed by a signal or not waited for.
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

/// Run a program (no shell) with a timeout; None when it cannot be started at all.
pub fn run_cmd(argv: &[&str], timeout: Duration) -> Option<CmdOutput> {
    let (program, args) = argv.split_first()?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let mut out = child.stdout.take()?;
    let mut err = child.stderr.take()?;
    let reader_out = thread::spawn(move || {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s);
        s
    });
    let reader_err = thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    });
    let start = Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if start.elapsed() >= timeout => {
                let _ = child.kill();
                timed_out = true;
                break child.wait().ok();
            }
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(_) => break None,
        }
    };
    Some(CmdOutput {
        success: status.is_some_and(|s| s.success()) && !timed_out,
        status: status.and_then(|s| s.code()),
        stdout: reader_out.join().unwrap_or_default(),
        stderr: reader_err.join().unwrap_or_default(),
        timed_out,
    })
}

/// Whether a program is on PATH.
pub fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| {
            let candidate = dir.join(program);
            candidate.is_file()
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_round_trip() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(stamp(1_700_000_000), "2023-11-14 22:13 UTC");
        for epoch in [0u64, 951_782_400, 1_709_164_800, 4_102_444_800] {
            let d = date(epoch);
            assert_eq!(parse_date(&d), Some(epoch - epoch % 86_400), "{d}");
        }
        assert_eq!(parse_date("2024-13-01"), None);
        assert_eq!(parse_date("notadate"), None);
    }

    #[test]
    fn sizes_read_like_people_write_them() {
        assert_eq!(human(10), "10 B");
        assert_eq!(human(1536), "1.5 KiB");
        assert_eq!(human(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn a_command_that_overruns_is_killed() {
        let out = run_cmd(&["sleep", "5"], Duration::from_millis(100)).unwrap();
        assert!(out.timed_out && !out.success);
        let out = run_cmd(&["echo", "hi"], Duration::from_secs(5)).unwrap();
        assert_eq!(out.stdout.trim(), "hi");
        assert!(run_cmd(&["/nonexistent/program"], Duration::from_secs(1)).is_none());
    }
}
