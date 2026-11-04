//! All state mutations hold a cross-process lock and replace complete files.

use crate::{Error, Result, now_ms, runtime};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use iroh::{EndpointAddr, SecretKey};
use iroh_tickets::endpoint::EndpointTicket;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::BTreeMap;
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::PathBuf,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Peer {
    pub id: String,
    pub name: String,
    pub paired: u64,
    pub seen: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket: Option<String>,
}

impl Peer {
    pub fn addr(&self) -> Result<EndpointAddr> {
        let id = runtime(self.id.parse())?;
        match &self.ticket {
            Some(ticket) => {
                let ticket: EndpointTicket = runtime(ticket.parse())?;
                if ticket.endpoint_addr().id != id {
                    return Err(Error::Runtime("peer ticket identity mismatch"));
                }
                Ok(ticket.endpoint_addr().clone())
            }
            None => Ok(EndpointAddr::new(id)),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Removed {
    pub id: String,
    pub at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Snapshot {
    pub peers: Vec<Peer>,
    pub removed: Vec<Removed>,
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
        runtime(
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&state.dir),
        )?;
        if runtime(fs::metadata(&state.dir))?.permissions().mode() & 0o077 != 0 {
            return Err(Error::Config("link state directory must have mode 0700"));
        }
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

    pub(crate) fn add(&self, id: String, name: String, ticket: Option<String>) -> Result<Peer> {
        let _lock = self.lock()?;
        self.add_locked(id, name, ticket)
    }

    pub(crate) fn add_locked(
        &self,
        id: String,
        name: String,
        ticket: Option<String>,
    ) -> Result<Peer> {
        let mut peers = self.peers()?;
        let now = now_ms();
        let mut removed = self.removed()?;
        let paired = now
            .max(removed.remove(&id).unwrap_or(0).saturating_add(1))
            .max(
                peers
                    .iter()
                    .find(|p| p.id == id)
                    .map_or(0, |p| p.paired.saturating_add(1)),
            );
        peers.retain(|p| p.id != id);
        let peer = Peer {
            id,
            name,
            paired,
            seen: now,
            ticket,
        };
        peers.push(peer.clone());
        self.write("removed.json", &removed)?;
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
        let mut removed = self.removed()?;
        for peer in peers.iter().filter(|p| p.name == name) {
            let at = now_ms().max(peer.paired);
            removed
                .entry(peer.id.clone())
                .and_modify(|old| *old = (*old).max(at))
                .or_insert(at);
        }
        peers.retain(|p| p.name != name);
        self.write("removed.json", &removed)?;
        self.write("peers.json", &peers)?;
        Ok(before - peers.len())
    }

    fn removed(&self) -> Result<BTreeMap<String, u64>> {
        Ok(self.read("removed.json")?.unwrap_or_default())
    }

    pub(crate) fn snapshot(&self) -> Result<Snapshot> {
        let _lock = self.lock()?;
        Ok(Snapshot {
            peers: self.peers()?,
            removed: self
                .removed()?
                .into_iter()
                .map(|(id, at)| Removed { id, at })
                .collect(),
        })
    }

    /// Any admitted member may introduce peers. Validate before mutating any state.
    pub(crate) fn merge(&self, snapshot: Snapshot, own_id: &str) -> Result<bool> {
        for peer in &snapshot.peers {
            peer.addr()?;
        }
        for removed in &snapshot.removed {
            runtime(removed.id.parse::<iroh::EndpointId>())?;
        }
        let _lock = self.lock()?;
        let mut peers = self.peers()?;
        let mut removed = self.removed()?;
        let before = (peers.clone(), removed.clone());
        for tombstone in snapshot.removed {
            removed
                .entry(tombstone.id)
                .and_modify(|at| *at = (*at).max(tombstone.at))
                .or_insert(tombstone.at);
        }
        for peer in snapshot.peers {
            if peer.id == own_id {
                continue;
            }
            if let Some(existing) = peers.iter_mut().find(|p| p.id == peer.id) {
                if peer.paired > existing.paired {
                    *existing = peer;
                }
            } else {
                peers.push(peer);
            }
        }
        peers.retain(|peer| match removed.get(&peer.id) {
            Some(at) if *at >= peer.paired => false,
            Some(_) => {
                removed.remove(&peer.id);
                true
            }
            None => true,
        });
        let changed = before != (peers.clone(), removed.clone());
        if changed {
            self.write("removed.json", &removed)?;
            self.write("peers.json", &peers)?;
        }
        Ok(changed)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_timestamps_tombstones_and_legacy_records() {
        let mut bytes = [0; 32];
        getrandom::fill(&mut bytes).unwrap();
        let id = SecretKey::from_bytes(&bytes).public().to_string();
        let dir =
            std::env::temp_dir().join(format!("arca-merge-{}", URL_SAFE_NO_PAD.encode(bytes)));
        let state = State::new(&dir).unwrap();
        let legacy = serde_json::json!({"id": id, "name": "first", "paired": 10, "seen": 11});
        let peer: Peer = serde_json::from_value(legacy).unwrap();
        assert!(peer.ticket.is_none());
        assert!(serde_json::to_value(&peer).unwrap().get("ticket").is_none());
        assert_eq!(peer.addr().unwrap(), EndpointAddr::new(id.parse().unwrap()));
        let snapshot = |peers, at| Snapshot {
            peers,
            removed: vec![Removed { id: id.clone(), at }],
        };
        state
            .merge(snapshot(vec![peer.clone()], 9), "self")
            .unwrap();
        assert_eq!(state.peers().unwrap(), vec![peer.clone()]);
        assert!(state.removed().unwrap().is_empty());
        let mut older = peer.clone();
        older.name = "older".to_string();
        older.paired = 9;
        state
            .merge(
                Snapshot {
                    peers: vec![older],
                    removed: vec![],
                },
                "self",
            )
            .unwrap();
        assert_eq!(state.peers().unwrap(), vec![peer.clone()]);
        state.merge(snapshot(vec![], 10), "self").unwrap();
        assert!(state.peers().unwrap().is_empty());
        state
            .merge(snapshot(vec![peer.clone()], 9), "self")
            .unwrap();
        assert!(state.peers().unwrap().is_empty());
        let mut newer = peer;
        newer.paired = 11;
        state
            .merge(snapshot(vec![newer.clone()], 10), "self")
            .unwrap();
        assert_eq!(state.peers().unwrap(), vec![newer]);
        assert!(state.removed().unwrap().is_empty());
        state.remove("first").unwrap();
        assert!(!state.removed().unwrap().is_empty());
        let added = state.add(id.clone(), "first".to_string(), None).unwrap();
        assert!(state.removed().unwrap().is_empty());
        assert!(added.paired > 11);
        fs::remove_dir_all(dir).unwrap();
    }
}
