//! What a pool keeps on the host: one line per job in `jobs.jsonl`, and each job machine's packed runner output and
//! `_diag` in its own folder. Folders are 0700 and files 0600: root reads them, nobody else. Logs older than the pool's
//! `logDays` are deleted.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::Serialize;

/// `jobs.jsonl` is set aside as `jobs.jsonl.1` past this size.
const JOBS_MAX: u64 = 16 * 1024 * 1024;

/// One finished machine: the job it ran, if any, and how it went.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobRecord {
    pub runner: String,
    pub class: String,
    pub repository: String,
    pub workflow_ref: String,
    pub run_id: i64,
    pub job_id: String,
    pub job_name: String,
    pub event: String,
    /// GitHub's word: `succeeded`, `failed`, `canceled`; empty when the machine ran no job.
    pub result: String,
    pub queued_at: String,
    pub assigned_at: String,
    pub finished_at: String,
    /// From GitHub's queue time to the runner's assignment, in seconds.
    pub queue_seconds: Option<f64>,
    /// From the machine's create to its runner's start, in seconds.
    pub boot_seconds: Option<f64>,
    /// From the machine's create to its delete, in seconds.
    pub machine_seconds: f64,
    pub runner_exit: Option<i64>,
    /// Where the logs are, relative to the pool's folder.
    pub logs: Option<String>,
    /// Why the machine ended without a job, or what failed around it.
    pub note: Option<String>,
}

/// The pool's folder on the host.
#[derive(Debug, Clone)]
pub struct Recorder {
    dir: PathBuf,
    keep: Duration,
}

impl Recorder {
    /// The recorder of the pool whose folder is `dir`, keeping logs `days` days. The folder is made 0700.
    pub fn open(dir: PathBuf, days: u32) -> std::io::Result<Self> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;
        Ok(Self {
            dir,
            keep: Duration::from_secs(u64::from(days) * 86_400),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Appends one job line.
    pub fn job(&self, record: &JobRecord) -> std::io::Result<()> {
        let path = self.dir.join("jobs.jsonl");
        if fs::metadata(&path).is_ok_and(|meta| meta.len() > JOBS_MAX) {
            fs::rename(&path, self.dir.join("jobs.jsonl.1"))?;
        }
        let mut line = serde_json::to_vec(record).map_err(std::io::Error::other)?;
        line.push(b'\n');
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(&line)
    }

    /// Keeps one machine's packed logs. Returns their path relative to the pool's folder.
    pub fn logs(&self, runner: &str, bytes: &[u8]) -> std::io::Result<String> {
        let folder = self.dir.join("logs").join(runner);
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&folder)?;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(folder.join("logs.tar.gz"))?;
        file.write_all(bytes)?;
        Ok(format!("logs/{runner}/logs.tar.gz"))
    }

    /// Deletes the logs older than the pool keeps them. Returns how many folders went.
    pub fn sweep(&self) -> usize {
        let Ok(entries) = fs::read_dir(self.dir.join("logs")) else {
            return 0;
        };
        let now = SystemTime::now();
        let mut removed = 0;
        for entry in entries.flatten() {
            let old = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age > self.keep);
            if old && fs::remove_dir_all(entry.path()).is_ok() {
                removed += 1;
            }
        }
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn logs_and_jobs_are_private_to_root() {
        let dir = std::env::temp_dir().join(format!("arca-runners-record-{}", std::process::id()));
        let recorder = Recorder::open(dir.clone(), 7).expect("open");
        let path = recorder.logs("r-1", b"packed").expect("logs");
        recorder
            .job(&JobRecord {
                runner: "r-1".to_string(),
                logs: Some(path.clone()),
                ..JobRecord::default()
            })
            .expect("job");
        let mode = |p: &Path| fs::metadata(p).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("logs/r-1")), 0o700);
        assert_eq!(mode(&dir.join(&path)), 0o600);
        assert_eq!(mode(&dir.join("jobs.jsonl")), 0o600);
        assert_eq!(recorder.sweep(), 0);
        let _ = fs::remove_dir_all(dir);
    }
}
