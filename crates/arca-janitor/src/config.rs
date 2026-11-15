//! The janitor's settings: defaults, then `cleanup.conf` (every host), then `cleanup.<hostname>.conf` (one host).
//!
//! Lines are `KEY=value` or `KEY+=value`. A list key given with `=` becomes that one value; `+=` adds one. A leading
//! `~` means the home. `#` starts a comment. Nothing in a file is run. An unknown key or a number that is not one is
//! an error, so a typo never silently turns a protection off.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct Config {
    /// The home of the user the janitor runs as.
    pub home: PathBuf,
    /// The short host name the per-host file is picked by.
    pub host: String,
    /// Folders whose top-level entries are scratch: stale ones are quarantined.
    pub scratch_roots: Vec<PathBuf>,
    /// Folders searched for build outputs.
    pub project_roots: Vec<PathBuf>,
    /// Paths never touched, nor anything inside or around them.
    pub protect: Vec<PathBuf>,
    /// Names of top-level scratch folders that are kept warm (caches of models, browsers, toolchains).
    pub cache_keep: Vec<String>,
    pub stale_days: u64,
    pub trash_days: u64,
    pub build_idle_days: u64,
    pub scratch_build_idle_days: u64,
    pub npm_max_mb: u64,
    pub gobuild_max_mb: u64,
    pub cargo_max_mb: u64,
    pub nodegyp_max_mb: u64,
    pub max_depth: u64,
    /// IO pressure (percent, avg10) above which a job pauses. On macOS: one-minute load per CPU, in percent.
    pub psi_threshold: u64,
    /// Where IO pressure is read (Linux).
    pub psi_path: PathBuf,
    /// A job's time budget, in minutes.
    pub job_budget_min: u64,
    /// The nightly run refuses to free more than this many GB.
    pub nightly_max_gb: u64,
    /// The nightly run refuses to move more than this many GB to trash.
    pub nightly_max_trash_gb: u64,
    /// A nightly plan naming any of these in a path is refused as a whole.
    pub stop_words: Vec<String>,
    /// The files that were read, in order.
    pub files: Vec<PathBuf>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ConfigError {
    pub file: String,
    pub line: usize,
    pub message: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "config {} line {}: {}",
            self.file, self.line, self.message
        )
    }
}

impl std::error::Error for ConfigError {}

const DEFAULT_CACHE_KEEP: &[&str] = &[
    "huggingface",
    "torch",
    "whisper",
    "lm-studio",
    "ollama",
    "ms-playwright",
    "puppeteer",
    "JetBrains",
    "pip",
    "uv",
    "go-build",
    "node-gyp",
    "pnpm",
    "yarn",
    "bun",
    "deno",
    "mesa_shader_cache",
    "nvidia",
    "fontconfig",
];

/// Words that stop a nightly run when a path to remove or move holds one: things people keep on purpose.
const DEFAULT_STOP_WORDS: &[&str] = &[
    "release", "backup", "archive", "evidence", "signed", ".app/",
];

impl Config {
    /// The same config with every path in its real, link-free form, the form /proc reports (`/home` linked to a
    /// data disk, macOS's `/tmp` in `/private/tmp`). The home and PROTECT paths are resolved fully. A root's own
    /// last component is kept as it is, so a root that is itself a link stays refused; only the folders above it
    /// are resolved. A path that does not exist is kept unchanged.
    pub fn canonical(&self) -> Config {
        let full = |p: &PathBuf| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
        let above = |p: &PathBuf| match (p.parent(), p.file_name()) {
            (Some(parent), Some(name)) => std::fs::canonicalize(parent)
                .map(|real| real.join(name))
                .unwrap_or_else(|_| p.clone()),
            _ => p.clone(),
        };
        let mut c = self.clone();
        c.home = full(&self.home);
        c.scratch_roots = self.scratch_roots.iter().map(above).collect();
        c.project_roots = self.project_roots.iter().map(above).collect();
        // PROTECT keeps both forms, so a path given through a link is matched either way.
        c.protect = self.protect.iter().map(full).collect();
        for p in &self.protect {
            if !c.protect.contains(p) {
                c.protect.push(p.clone());
            }
        }
        c
    }

    /// The defaults for a home and host, before any file is read.
    pub fn defaults(home: &Path, host: &str) -> Config {
        Config {
            home: home.to_path_buf(),
            host: host.to_string(),
            scratch_roots: vec![home.join(".cache")],
            project_roots: vec![home.to_path_buf()],
            protect: Vec::new(),
            cache_keep: DEFAULT_CACHE_KEEP.iter().map(|s| s.to_string()).collect(),
            stale_days: 14,
            trash_days: 7,
            build_idle_days: 14,
            scratch_build_idle_days: 3,
            npm_max_mb: 2048,
            gobuild_max_mb: 2048,
            cargo_max_mb: 1024,
            nodegyp_max_mb: 512,
            max_depth: 6,
            psi_threshold: 10,
            psi_path: PathBuf::from("/proc/pressure/io"),
            job_budget_min: 120,
            nightly_max_gb: 150,
            nightly_max_trash_gb: 100,
            stop_words: DEFAULT_STOP_WORDS.iter().map(|s| s.to_string()).collect(),
            files: Vec::new(),
        }
    }

    /// The defaults plus `cleanup.conf` and `cleanup.<host>.conf` from `dir`, those that exist.
    pub fn load(dir: &Path, home: &Path, host: &str) -> Result<Config, ConfigError> {
        let mut config = Config::defaults(home, host);
        for name in ["cleanup.conf".to_string(), format!("cleanup.{host}.conf")] {
            let path = dir.join(&name);
            if let Ok(text) = fs::read_to_string(&path) {
                config.apply(&path.display().to_string(), &text)?;
                config.files.push(path);
            }
        }
        Ok(config)
    }

    /// The config the environment points at: JANITOR_CONFIG_DIR (else ~/.config/janitor), HOME, JANITOR_HOSTNAME.
    pub fn from_env() -> Result<Config, ConfigError> {
        let home = home_dir();
        let host =
            std::env::var("JANITOR_HOSTNAME").unwrap_or_else(|_| crate::platform::hostname());
        let dir = std::env::var_os("JANITOR_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config/janitor"));
        Config::load(&dir, &home, &host)
    }

    /// Apply one file's text on top of this config.
    pub fn apply(&mut self, file: &str, text: &str) -> Result<(), ConfigError> {
        for (index, raw) in text.lines().enumerate() {
            let err = |message: String| ConfigError {
                file: file.to_string(),
                line: index + 1,
                message,
            };
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let Some(eq) = line.find('=') else {
                return Err(err(format!("cannot read: {line}")));
            };
            let (mut key, value) = (&line[..eq], &line[eq + 1..]);
            let append = key.ends_with('+');
            if append {
                key = &key[..key.len() - 1];
            }
            if key.is_empty() || !key.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
                return Err(err(format!("cannot read: {line}")));
            }
            let path_value = || expand_home(value, &self.home);
            match key {
                "SCRATCH_ROOTS" => set_list(&mut self.scratch_roots, path_value(), append),
                "PROJECT_ROOTS" => set_list(&mut self.project_roots, path_value(), append),
                "PROTECT" => set_list(&mut self.protect, path_value(), append),
                "CACHE_KEEP" => set_list(&mut self.cache_keep, value.to_string(), append),
                "STOP_WORDS" => set_list(&mut self.stop_words, value.to_string(), append),
                "PSI_PATH" => self.psi_path = path_value(),
                _ => {
                    let slot = match key {
                        "STALE_DAYS" => &mut self.stale_days,
                        "TRASH_DAYS" => &mut self.trash_days,
                        "BUILD_IDLE_DAYS" => &mut self.build_idle_days,
                        "SCRATCH_BUILD_IDLE_DAYS" => &mut self.scratch_build_idle_days,
                        "NPM_MAX_MB" => &mut self.npm_max_mb,
                        "GOBUILD_MAX_MB" => &mut self.gobuild_max_mb,
                        "CARGO_MAX_MB" => &mut self.cargo_max_mb,
                        "NODEGYP_MAX_MB" => &mut self.nodegyp_max_mb,
                        "MAX_DEPTH" => &mut self.max_depth,
                        "PSI_THRESHOLD" => &mut self.psi_threshold,
                        "JOB_BUDGET_MIN" => &mut self.job_budget_min,
                        "NIGHTLY_MAX_GB" => &mut self.nightly_max_gb,
                        "NIGHTLY_MAX_TRASH_GB" => &mut self.nightly_max_trash_gb,
                        _ => return Err(err(format!("unknown key {key}"))),
                    };
                    if append {
                        return Err(err(format!("{key} takes one number, not +=")));
                    }
                    if value.is_empty() || !value.chars().all(|c| c.is_ascii_digit()) {
                        return Err(err(format!("{key} needs a number")));
                    }
                    *slot = value
                        .parse()
                        .map_err(|_| err(format!("{key} needs a number")))?;
                }
            }
        }
        Ok(())
    }
}

fn set_list<T>(list: &mut Vec<T>, value: T, append: bool) {
    if !append {
        list.clear();
    }
    list.push(value);
}

/// `~` or `~/x` means the home; a trailing slash is dropped.
pub fn expand_home(value: &str, home: &Path) -> PathBuf {
    let path = if value == "~" {
        home.to_path_buf()
    } else if let Some(rest) = value.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(value)
    };
    let text = path.to_string_lossy();
    if text.len() > 1 && text.ends_with('/') {
        PathBuf::from(text.trim_end_matches('/'))
    } else {
        path
    }
}

/// The home from $HOME (the janitor never runs as root, so there is always one).
pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conf() -> Config {
        Config::defaults(Path::new("/home/acme"), "box")
    }

    #[test]
    fn equals_replaces_a_list_and_plus_equals_adds() {
        let mut c = conf();
        c.apply("t", "SCRATCH_ROOTS=~/scratch\nSCRATCH_ROOTS+=/srv/tmp/\n")
            .unwrap();
        assert_eq!(
            c.scratch_roots,
            vec![
                PathBuf::from("/home/acme/scratch"),
                PathBuf::from("/srv/tmp")
            ]
        );
        c.apply("t", "PROTECT+=~/prod # keep\nCACHE_KEEP+=models\n")
            .unwrap();
        assert_eq!(c.protect, vec![PathBuf::from("/home/acme/prod")]);
        assert!(c.cache_keep.contains(&"models".to_string()));
        assert!(c.cache_keep.contains(&"huggingface".to_string()));
    }

    #[test]
    fn numbers_comments_and_blank_lines() {
        let mut c = conf();
        c.apply(
            "t",
            "# a comment\n\nSTALE_DAYS=30\n  TRASH_DAYS=2  \nPSI_THRESHOLD=25\n",
        )
        .unwrap();
        assert_eq!((c.stale_days, c.trash_days, c.psi_threshold), (30, 2, 25));
    }

    #[test]
    fn a_typo_or_a_bad_number_is_an_error_with_its_line() {
        let mut c = conf();
        let e = c.apply("f", "STALE_DAYS=3\nSTALE_DAYZ=4\n").unwrap_err();
        assert_eq!((e.line, e.message.as_str()), (2, "unknown key STALE_DAYZ"));
        let e = c.apply("f", "TRASH_DAYS=seven\n").unwrap_err();
        assert_eq!(e.message, "TRASH_DAYS needs a number");
        let e = c.apply("f", "TRASH_DAYS+=1\n").unwrap_err();
        assert!(e.message.contains("+="), "{}", e.message);
        let e = c.apply("f", "just words\n").unwrap_err();
        assert!(e.message.starts_with("cannot read"), "{}", e.message);
        let e = c.apply("f", "lower=1\n").unwrap_err();
        assert!(e.message.starts_with("cannot read"), "{}", e.message);
    }

    #[test]
    fn the_host_file_is_read_after_the_common_one() {
        let dir = std::env::temp_dir().join(format!("janitor-conf-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("cleanup.conf"), "STALE_DAYS=20\nPROTECT=~/a\n").unwrap();
        fs::write(dir.join("cleanup.box.conf"), "STALE_DAYS=9\nPROTECT+=~/b\n").unwrap();
        fs::write(dir.join("cleanup.other.conf"), "STALE_DAYS=1\n").unwrap();
        let c = Config::load(&dir, Path::new("/home/acme"), "box").unwrap();
        assert_eq!(c.stale_days, 9);
        assert_eq!(
            c.protect,
            vec![PathBuf::from("/home/acme/a"), PathBuf::from("/home/acme/b")]
        );
        assert_eq!(c.files.len(), 2);
        fs::remove_dir_all(&dir).unwrap();
    }
}
