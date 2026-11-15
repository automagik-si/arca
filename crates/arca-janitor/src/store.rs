//! Where the janitor keeps its state: jobs, plans, snapshots and two histories, all plain JSON files.
//!
//! ```text
//! <state>/jobs/<id>.json        one job each
//! <state>/plans/<id>.json       one clean plan each
//! <state>/snapshots/<id>.json   one space report each
//! <state>/history.jsonl         one line per clean run (report reads it)
//! <state>/origins.jsonl         one line per planned, applied or skipped item (insights reads it)
//! ```

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::util::{append_line, atomic_write};

#[derive(Clone, Debug)]
pub struct Store {
    pub root: PathBuf,
}

/// Ids are made by the janitor; anything else (a slash, a dot) is refused before it becomes a path.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 80 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Store {
        Store { root: root.into() }
    }

    /// JANITOR_STATE_DIR, else $XDG_STATE_HOME/janitor, else ~/.local/state/janitor.
    pub fn from_env() -> Store {
        if let Some(dir) = std::env::var_os("JANITOR_STATE_DIR") {
            return Store::new(dir);
        }
        if let Some(dir) = std::env::var_os("XDG_STATE_HOME") {
            return Store::new(PathBuf::from(dir).join("janitor"));
        }
        Store::new(crate::config::home_dir().join(".local/state/janitor"))
    }

    pub fn jobs_dir(&self) -> PathBuf {
        self.root.join("jobs")
    }
    pub fn plans_dir(&self) -> PathBuf {
        self.root.join("plans")
    }
    pub fn snapshots_dir(&self) -> PathBuf {
        self.root.join("snapshots")
    }
    pub fn history_path(&self) -> PathBuf {
        self.root.join("history.jsonl")
    }
    pub fn origins_path(&self) -> PathBuf {
        self.root.join("origins.jsonl")
    }

    fn write_json<T: Serialize>(&self, dir: &Path, id: &str, value: &T) -> io::Result<()> {
        if !valid_id(id) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "bad id"));
        }
        let bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
        atomic_write(&dir.join(format!("{id}.json")), &bytes)
    }

    fn read_json<T: DeserializeOwned>(&self, dir: &Path, id: &str) -> io::Result<T> {
        if !valid_id(id) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "bad id"));
        }
        let bytes = fs::read(dir.join(format!("{id}.json")))?;
        serde_json::from_slice(&bytes).map_err(io::Error::other)
    }

    fn list_json<T: DeserializeOwned>(&self, dir: &Path) -> Vec<T> {
        let Ok(entries) = fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut found = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(bytes) = fs::read(&path)
                && let Ok(value) = serde_json::from_slice(&bytes)
            {
                found.push(value);
            }
        }
        found
    }

    pub fn save_job<T: Serialize>(&self, id: &str, job: &T) -> io::Result<()> {
        self.write_json(&self.jobs_dir(), id, job)
    }
    pub fn load_job<T: DeserializeOwned>(&self, id: &str) -> io::Result<T> {
        self.read_json(&self.jobs_dir(), id)
    }
    pub fn list_jobs<T: DeserializeOwned>(&self) -> Vec<T> {
        self.list_json(&self.jobs_dir())
    }

    pub fn save_plan<T: Serialize>(&self, id: &str, plan: &T) -> io::Result<()> {
        self.write_json(&self.plans_dir(), id, plan)
    }
    pub fn load_plan<T: DeserializeOwned>(&self, id: &str) -> io::Result<T> {
        self.read_json(&self.plans_dir(), id)
    }

    pub fn save_snapshot<T: Serialize>(&self, id: &str, snap: &T) -> io::Result<()> {
        self.write_json(&self.snapshots_dir(), id, snap)
    }
    pub fn list_snapshots<T: DeserializeOwned>(&self) -> Vec<T> {
        self.list_json(&self.snapshots_dir())
    }

    pub fn append<T: Serialize>(&self, path: &Path, value: &T) -> io::Result<()> {
        let line = serde_json::to_string(value).map_err(io::Error::other)?;
        append_line(path, &line)
    }

    /// Every line of a JSONL file that parses; a torn last line is skipped.
    pub fn read_lines<T: DeserializeOwned>(&self, path: &Path) -> Vec<T> {
        fs::read_to_string(path)
            .map(|text| {
                text.lines()
                    .filter_map(|l| serde_json::from_str(l).ok())
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// The daemon's socket: JANITOR_SOCKET, else $XDG_RUNTIME_DIR/janitor.sock, else <state>/janitor.sock.
pub fn socket_path(store: &Store) -> PathBuf {
    if let Some(path) = std::env::var_os("JANITOR_SOCKET") {
        return PathBuf::from(path);
    }
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("janitor.sock");
    }
    store.root.join("janitor.sock")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_cannot_escape_the_state_folder() {
        assert!(valid_id("j20261115-010203-1a000"));
        for bad in ["", "../x", "a/b", "a.b", "x y"] {
            assert!(!valid_id(bad), "{bad}");
        }
        let store = Store::new(std::env::temp_dir().join("janitor-never-written"));
        assert!(store.save_job("../escape", &1).is_err());
    }
}
