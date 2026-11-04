//! All state mutations hold a cross-process lock and replace complete files.

use crate::{Error, Result, now_ms, runtime};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use iroh::SecretKey;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Peer {
    pub id: String,
    pub name: String,
    pub paired: u64,
    pub seen: u64,
}

#[derive(Serialize, Deserialize)]
pub struct Status {
    pub pid: u32,
    pub id: String,
    pub ticket: String,
    pub name: String,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Pending {
    pub code: String,
    pub expires: u64,
    pub name: String,
    pub failures: u32,
    pub tries: std::collections::BTreeMap<String, u32>,
}

#[derive(Clone)]
pub struct State {
    pub(crate) dir: PathBuf,
}

impl State {
    pub fn new(dir: impl Into<PathBuf>) -> Result<Self> {
        let state = Self { dir: dir.into() };
        runtime(fs::create_dir_all(&state.dir))?;
        Ok(state)
    }

    pub(crate) fn lock(&self) -> Result<File> {
        let file = self.open_lock(".link.lock")?;
        runtime(file.lock())?;
        Ok(file)
    }

    pub(crate) fn open_lock(&self, name: &str) -> Result<File> {
        runtime(
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(self.dir.join(name)),
        )
    }

    pub fn secret(&self) -> Result<SecretKey> {
        let _lock = self.lock()?;
        let path = self.dir.join("secret.key");
        match fs::read(&path) {
            Ok(bytes) => {
                let bytes: [u8; 32] = bytes
                    .try_into()
                    .map_err(|_| Error::Config("invalid link secret file"))?;
                runtime(fs::set_permissions(path, fs::Permissions::from_mode(0o600)))?;
                Ok(SecretKey::from_bytes(&bytes))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut bytes = [0; 32];
                runtime(getrandom::fill(&mut bytes))?;
                self.write_bytes("secret.key", &bytes)?;
                Ok(SecretKey::from_bytes(&bytes))
            }
            Err(_) => Err(Error::Runtime("cannot read link secret file")),
        }
    }

    pub(crate) fn read<T: DeserializeOwned>(&self, name: &str) -> Result<Option<T>> {
        match fs::read(self.dir.join(name)) {
            Ok(bytes) => runtime(serde_json::from_slice(&bytes)).map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(Error::Runtime("cannot read link state")),
        }
    }

    pub(crate) fn write<T: Serialize>(&self, name: &str, value: &T) -> Result<()> {
        self.write_bytes(name, &runtime(serde_json::to_vec(value))?)
    }

    fn write_bytes(&self, name: &str, bytes: &[u8]) -> Result<()> {
        let mut suffix = [0; 16];
        runtime(getrandom::fill(&mut suffix))?;
        let tmp = self
            .dir
            .join(format!(".{name}.{}", URL_SAFE_NO_PAD.encode(suffix)));
        let result = (|| {
            let mut file = runtime(
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&tmp),
            )?;
            runtime(file.write_all(bytes))?;
            runtime(file.sync_all())?;
            runtime(fs::rename(&tmp, self.dir.join(name)))
        })();
        if result.is_err() {
            let _ = fs::remove_file(tmp);
        }
        result
    }

    pub fn peers(&self) -> Result<Vec<Peer>> {
        Ok(self.read("peers.json")?.unwrap_or_default())
    }

    pub(crate) fn add(&self, id: String, name: String) -> Result<Peer> {
        let _lock = self.lock()?;
        self.add_locked(id, name)
    }

    pub(crate) fn add_locked(&self, id: String, name: String) -> Result<Peer> {
        let mut peers = self.peers()?;
        let now = now_ms();
        let paired = peers.iter().find(|p| p.id == id).map_or(now, |p| p.paired);
        peers.retain(|p| p.id != id);
        let peer = Peer {
            id,
            name,
            paired,
            seen: now,
        };
        peers.push(peer.clone());
        self.write("peers.json", &peers)?;
        Ok(peer)
    }

    pub(crate) fn admit(&self, id: &str) -> Result<bool> {
        let _lock = self.lock()?;
        let mut peers = self.peers()?;
        let Some(peer) = peers.iter_mut().find(|p| p.id == id) else {
            return Ok(false);
        };
        peer.seen = now_ms();
        self.write("peers.json", &peers)?;
        Ok(true)
    }

    pub fn remove(&self, name: &str) -> Result<usize> {
        let _lock = self.lock()?;
        let mut peers = self.peers()?;
        let before = peers.len();
        peers.retain(|p| p.name != name);
        self.write("peers.json", &peers)?;
        Ok(before - peers.len())
    }

    pub fn status(&self) -> Result<Status> {
        let lock = self.open_lock(".link-serve.lock")?;
        match lock.try_lock() {
            Err(std::fs::TryLockError::WouldBlock) => {}
            _ => return Err(Error::Runtime("link serve is not running")),
        }
        self.read("status.json")?
            .ok_or(Error::Runtime("link serve status is absent"))
    }

    pub(crate) fn delete(&self, name: &str) -> Result<()> {
        match fs::remove_file(self.dir.join(name)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(Error::Runtime("cannot remove link state")),
        }
    }
}
