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

const REMOVED_TTL_MS: u64 = 30 * 24 * 60 * 60 * 1_000;
const REMOVED_LIMIT: usize = 64;
const REMOVED_SKEW_MS: u64 = 5 * 60 * 1_000;

fn prune_removed(removed: &mut BTreeMap<String, u64>, now: u64) -> bool {
    let mut changed = false;
    let latest = now.saturating_add(REMOVED_SKEW_MS);
    removed.retain(|_, at| {
        if *at > latest {
            *at = latest;
            changed = true;
        }
        let keep = now.saturating_sub(*at) <= REMOVED_TTL_MS;
        changed |= !keep;
        keep
    });
    if removed.len() > REMOVED_LIMIT {
        let mut oldest: Vec<_> = removed.iter().map(|(id, at)| (*at, id.clone())).collect();
        oldest.sort_unstable();
        for (_, id) in oldest.into_iter().take(removed.len() - REMOVED_LIMIT) {
            removed.remove(&id);
        }
        changed = true;
    }
    changed
}

const PATHS_FRESH_MS: u64 = 10_000;

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

    /// Read optional address hints without changing the public peer record or its writer.
    pub(crate) fn peer_addr(&self, name: &str) -> Result<EndpointAddr> {
        let peers = self.peers()?;
        let mut matches = peers.iter().filter(|peer| peer.name == name);
        let peer = matches.next().ok_or(Error::Runtime("unknown peer name"))?;
        if matches.next().is_some() {
            return Err(Error::Runtime("ambiguous peer name"));
        }
        peer.addr()
    }

    /// The path each peer was reached on when the running link last looked, by peer name. Empty when the link is
    /// not running or its record is older than `PATHS_FRESH_MS`.
    pub fn paths(&self) -> Result<std::collections::BTreeMap<String, crate::PathNow>> {
        let mut paths: std::collections::BTreeMap<String, crate::PathNow> =
            self.read("paths.json")?.unwrap_or_default();
        let now = now_ms();
        paths.retain(|_, p| now.saturating_sub(p.at) <= PATHS_FRESH_MS);
        Ok(paths)
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
        prune_removed(&mut removed, now);
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
        let now = now_ms();
        for peer in peers.iter().filter(|p| p.name == name) {
            let at = now
                .max(peer.paired)
                .min(now.saturating_add(REMOVED_SKEW_MS));
            removed
                .entry(peer.id.clone())
                .and_modify(|old| *old = (*old).max(at))
                .or_insert(at);
        }
        prune_removed(&mut removed, now);
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
        let mut removed = self.removed()?;
        if prune_removed(&mut removed, now_ms()) {
            self.write("removed.json", &removed)?;
        }
        Ok(Snapshot {
            peers: self.peers()?,
            removed: removed
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
        let now = now_ms();
        for tombstone in snapshot.removed {
            let at = tombstone.at.min(now.saturating_add(REMOVED_SKEW_MS));
            removed
                .entry(tombstone.id)
                .and_modify(|old| *old = (*old).max(at))
                .or_insert(at);
        }
        prune_removed(&mut removed, now);
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

    /// The gate around every try of the serve lock. Asking whether `serve` runs takes the serve lock for a moment when
    /// none runs, and a `serve` that tried it in that moment found it held and stopped as "already running". Both now
    /// try the serve lock only inside this gate, so neither ever finds the other's moment. It is held for one try.
    fn serve_gate(&self) -> Result<File> {
        let gate = self.open_lock(".link-serve-gate.lock")?;
        runtime(gate.lock())?;
        Ok(gate)
    }

    /// Takes the lock `serve` holds for as long as it runs, or says that another `serve` holds it.
    pub(crate) fn hold_serve(&self) -> Result<File> {
        let _gate = self.serve_gate()?;
        let lock = self.open_lock(".link-serve.lock")?;
        lock.try_lock()
            .map_err(|_| Error::Runtime("link serve is already running"))?;
        Ok(lock)
    }

    pub fn status(&self) -> Result<Status> {
        let running = {
            let _gate = self.serve_gate()?;
            let lock = self.open_lock(".link-serve.lock")?;
            // Dropped before the gate: a lock taken here is let go inside it.
            matches!(lock.try_lock(), Err(std::fs::TryLockError::WouldBlock))
        };
        if !running {
            return Err(Error::Runtime("link serve is not running"));
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
    use serde_json::json;

    struct Fixture(State);

    impl Fixture {
        fn new() -> Self {
            let mut bytes = [0; 32];
            getrandom::fill(&mut bytes).unwrap();
            let dir =
                std::env::temp_dir().join(format!("arca-merge-{}", URL_SAFE_NO_PAD.encode(bytes)));
            Self(State::new(dir).unwrap())
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0.dir).unwrap();
        }
    }

    // `status` is asked while `serve` starts, by the commands that use a running link and by whatever waits for it to
    // be up. Asking must never make the start fail.
    #[test]
    fn asking_for_the_status_never_makes_a_starting_serve_think_another_one_runs() {
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        use std::sync::{Arc, Barrier};
        use std::time::{Duration, Instant};

        let fixture = Fixture::new();
        let state = fixture.0.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let asked = Arc::new(AtomicU64::new(0));
        // Both sides start together, and the starts go on until the status has been asked 2000 times beside them: the
        // two race for the whole loop, on a quiet machine or a loaded one.
        let together = Arc::new(Barrier::new(2));
        let asker = {
            let (state, stop, asked, together) =
                (state.clone(), stop.clone(), asked.clone(), together.clone());
            std::thread::spawn(move || {
                together.wait();
                while !stop.load(Ordering::SeqCst) {
                    let _ = state.status();
                    asked.fetch_add(1, Ordering::SeqCst);
                }
            })
        };
        // Stops the asker and waits for it, so that nothing touches the state directory once the test ends, and a
        // panic of the asker is shown as its own.
        let finish = |asker: std::thread::JoinHandle<()>| {
            stop.store(true, Ordering::SeqCst);
            if let Err(panic) = asker.join() {
                std::panic::resume_unwind(panic);
            }
        };
        together.wait();
        let waited = Duration::from_secs(30);
        let deadline = Instant::now() + waited;
        let (mut starts, mut refused) = (0u64, 0u64);
        while starts < 2000 || asked.load(Ordering::SeqCst) < 2000 {
            if asker.is_finished() || Instant::now() >= deadline {
                finish(asker);
                panic!(
                    "after {waited:?}, {starts} starts ({refused} of them refused as 'already running') and {} status \
                     checks beside them; the test needs 2000 of each",
                    asked.load(Ordering::SeqCst)
                );
            }
            match state.hold_serve() {
                Ok(lock) => drop(lock),
                Err(_) => refused += 1,
            }
            starts += 1;
        }
        finish(asker);
        assert_eq!(
            refused,
            0,
            "{refused} of {starts} starts were refused as 'already running' while the status was asked {} times beside \
             them, and no other serve ran",
            asked.load(Ordering::SeqCst)
        );
        // A second serve is still refused.
        let held = state.hold_serve().unwrap();
        assert!(state.hold_serve().is_err());
        drop(held);
    }

    fn peer(index: u64, paired: u64) -> Peer {
        let mut bytes = [0; 32];
        bytes[..8].copy_from_slice(&index.to_le_bytes());
        Peer {
            id: SecretKey::from_bytes(&bytes).public().to_string(),
            name: format!("peer-{index}"),
            paired,
            seen: paired,
            ticket: None,
        }
    }

    #[test]
    fn merge_timestamps_tombstones_and_legacy_records() {
        let fixture = Fixture::new();
        let state = &fixture.0;
        let now = now_ms();
        let id = peer(0, now).id;
        let legacy = serde_json::json!({"id": id, "name": "first", "paired": now, "seen": now + 1});
        let peer: Peer = serde_json::from_value(legacy).unwrap();
        assert!(peer.ticket.is_none());
        assert!(serde_json::to_value(&peer).unwrap().get("ticket").is_none());
        assert_eq!(peer.addr().unwrap(), EndpointAddr::new(id.parse().unwrap()));
        let snapshot = |peers, at| Snapshot {
            peers,
            removed: vec![Removed { id: id.clone(), at }],
        };
        state
            .merge(snapshot(vec![peer.clone()], now - 1), "self")
            .unwrap();
        assert_eq!(state.peers().unwrap(), vec![peer.clone()]);
        assert!(state.removed().unwrap().is_empty());
        let mut older = peer.clone();
        older.name = "older".to_string();
        older.paired = now - 1;
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
        state.merge(snapshot(vec![], now), "self").unwrap();
        assert!(state.peers().unwrap().is_empty());
        state
            .merge(snapshot(vec![peer.clone()], now - 1), "self")
            .unwrap();
        assert!(state.peers().unwrap().is_empty());
        let mut newer = peer;
        newer.paired = now + 1;
        state
            .merge(snapshot(vec![newer.clone()], now), "self")
            .unwrap();
        assert_eq!(state.peers().unwrap(), vec![newer]);
        assert!(state.removed().unwrap().is_empty());
        state.remove("first").unwrap();
        assert!(!state.removed().unwrap().is_empty());
        let added = state.add(id.clone(), "first".to_string(), None).unwrap();
        assert!(state.removed().unwrap().is_empty());
        assert!(added.paired > now + 1);
    }

    #[test]
    fn many_removals_keep_newest_and_snapshot_below_line_limit() {
        let fixture = Fixture::new();
        let state = &fixture.0;
        let now = now_ms();
        let peers: Vec<_> = (0..320)
            .map(|index| peer(index, now + REMOVED_SKEW_MS / 2 + index))
            .collect();
        state.write("peers.json", &peers).unwrap();
        for peer in &peers {
            assert_eq!(state.remove(&peer.name).unwrap(), 1);
            assert!(state.removed().unwrap().len() <= REMOVED_LIMIT);
        }
        let expected: BTreeMap<_, _> = peers
            .iter()
            .rev()
            .take(REMOVED_LIMIT)
            .map(|peer| (peer.id.clone(), peer.paired))
            .collect();
        assert_eq!(state.removed().unwrap(), expected);
        let snapshot = state.snapshot().unwrap();
        assert_eq!(snapshot.removed.len(), REMOVED_LIMIT);
        assert!(snapshot.peers.is_empty());
        assert!(serde_json::to_vec(&snapshot).unwrap().len() < crate::mesh::LINE_LIMIT / 4);
    }

    #[test]
    fn pruning_expires_clamps_and_breaks_ties_deterministically() {
        let now = REMOVED_TTL_MS + 100;
        let mut removed: BTreeMap<_, _> =
            (0..100).map(|index| (peer(index, now).id, now)).collect();
        let mut expected = removed.clone();
        while expected.len() > REMOVED_LIMIT {
            expected.pop_first();
        }
        assert!(prune_removed(&mut removed, now));
        assert_eq!(removed, expected);
        assert!(!prune_removed(&mut removed, now));

        let (expired, boundary, future) = (peer(100, 0).id, peer(101, 0).id, peer(102, 0).id);
        let mut removed = BTreeMap::from([
            (expired.clone(), now - REMOVED_TTL_MS - 1),
            (boundary.clone(), now - REMOVED_TTL_MS),
            (future.clone(), u64::MAX),
        ]);
        assert!(prune_removed(&mut removed, now));
        assert!(!removed.contains_key(&expired));
        assert_eq!(removed[&boundary], now - REMOVED_TTL_MS);
        assert_eq!(removed[&future], now + REMOVED_SKEW_MS);
        assert!(!prune_removed(&mut removed, now));
        let mut removed = BTreeMap::from([(future, u64::MAX)]);
        assert!(!prune_removed(&mut removed, u64::MAX - 1));
    }

    #[test]
    fn snapshot_prunes_and_persists_legacy_tombstones() {
        let fixture = Fixture::new();
        let state = &fixture.0;
        let now = now_ms();
        let mut removed: BTreeMap<_, _> =
            (0..320).map(|index| (peer(index, now).id, now)).collect();
        let expired = peer(320, 0).id;
        let future = peer(321, 0).id;
        removed.insert(expired.clone(), now - REMOVED_TTL_MS - 1);
        removed.insert(future.clone(), u64::MAX);
        state.write("removed.json", &removed).unwrap();
        let snapshot = state.snapshot().unwrap();
        let persisted = state.removed().unwrap();
        assert_eq!(persisted.len(), REMOVED_LIMIT);
        assert!(!persisted.contains_key(&expired));
        assert!(persisted[&future] <= now_ms() + REMOVED_SKEW_MS);
        assert_eq!(
            snapshot.removed,
            persisted
                .into_iter()
                .map(|(id, at)| Removed { id, at })
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn merge_clamps_future_tombstones_before_eviction() {
        let fixture = Fixture::new();
        let state = &fixture.0;
        let now = now_ms();
        let older = peer(0, now);
        let newer = peer(1, now + 2 * REMOVED_SKEW_MS);
        let removed = [&older, &newer]
            .into_iter()
            .map(|peer| Removed {
                id: peer.id.clone(),
                at: u64::MAX,
            })
            .collect();
        assert!(
            state
                .merge(
                    Snapshot {
                        peers: vec![older.clone(), newer.clone()],
                        removed
                    },
                    "self"
                )
                .unwrap()
        );
        assert_eq!(state.peers().unwrap(), vec![newer]);
        let removed = state.removed().unwrap();
        assert_eq!(removed.len(), 1);
        assert!(removed[&older.id] >= now + REMOVED_SKEW_MS);
        assert!(removed[&older.id] <= now_ms() + REMOVED_SKEW_MS);
    }

    #[test]
    fn merge_prunes_union_and_expired_tombstones() {
        let fixture = Fixture::new();
        let state = &fixture.0;
        let now = now_ms();
        let previous: BTreeMap<_, _> = (0..64).map(|index| (peer(index, now).id, now)).collect();
        state.write("removed.json", &previous).unwrap();
        let newest: BTreeMap<_, _> = (64..128)
            .map(|index| (peer(index, now).id, now + 1))
            .collect();
        let mut removed: Vec<_> = newest
            .iter()
            .map(|(id, at)| Removed {
                id: id.clone(),
                at: *at,
            })
            .collect();
        removed.push(Removed {
            id: peer(128, 0).id,
            at: now - REMOVED_TTL_MS - 1,
        });
        assert!(
            state
                .merge(
                    Snapshot {
                        peers: vec![],
                        removed
                    },
                    "self"
                )
                .unwrap()
        );
        assert_eq!(state.removed().unwrap(), newest);
    }

    #[test]
    fn remove_clamps_future_pairing_timestamp() {
        let fixture = Fixture::new();
        let state = &fixture.0;
        let peer = peer(0, u64::MAX);
        state.write("peers.json", &vec![peer.clone()]).unwrap();
        assert_eq!(state.remove(&peer.name).unwrap(), 1);
        assert!(state.removed().unwrap()[&peer.id] <= now_ms() + REMOVED_SKEW_MS);
    }

    #[test]
    fn pairing_clears_own_tombstone_and_prunes_others() {
        let fixture = Fixture::new();
        let state = &fixture.0;
        let now = now_ms();
        let own = peer(0, now);
        let expired = peer(100, 0).id;
        let mut removed: BTreeMap<_, _> =
            (0..100).map(|index| (peer(index, now).id, now)).collect();
        removed.insert(expired.clone(), now - REMOVED_TTL_MS - 1);
        state.write("removed.json", &removed).unwrap();
        let added = state.add(own.id.clone(), own.name, None).unwrap();
        assert!(added.paired > now);
        let removed = state.removed().unwrap();
        assert_eq!(removed.len(), REMOVED_LIMIT);
        assert!(!removed.contains_key(&own.id));
        assert!(!removed.contains_key(&expired));
    }

    struct Temp(PathBuf);

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn peer_addresses_accept_legacy_records_and_verified_tickets() {
        let mut random = [0; 16];
        getrandom::fill(&mut random).unwrap();
        let temp = Temp(
            std::env::temp_dir().join(format!("arca-peer-{}", URL_SAFE_NO_PAD.encode(random))),
        );
        let state = State::new(&temp.0).unwrap();
        let id = state.secret().unwrap().public();
        state.add(id.to_string(), "peer".into(), None).unwrap();
        let addr = state.peer_addr("peer").unwrap();
        assert_eq!(addr, EndpointAddr::new(id));

        let addr = addr.with_ip_addr((std::net::Ipv4Addr::LOCALHOST, 12345).into());
        let ticket = EndpointTicket::new(addr.clone()).to_string();
        state
            .write(
                "peers.json",
                &json!([{"id": id.to_string(), "name": "peer", "paired": 0, "seen": 0, "ticket": ticket}]),
            )
            .unwrap();
        assert_eq!(state.peer_addr("peer").unwrap(), addr);
        assert_eq!(
            state.peer_addr("missing").unwrap_err().to_string(),
            "unknown peer name"
        );
    }

    #[test]
    fn peer_addresses_refuse_ambiguous_names_invalid_tickets_and_id_mismatches() {
        let mut random = [0; 16];
        getrandom::fill(&mut random).unwrap();
        let temp = Temp(
            std::env::temp_dir().join(format!("arca-peer-{}", URL_SAFE_NO_PAD.encode(random))),
        );
        let state = State::new(&temp.0).unwrap();
        let id = state.secret().unwrap().public();
        let mut key = [0; 32];
        getrandom::fill(&mut key).unwrap();
        let other = SecretKey::from_bytes(&key).public();
        let record = json!({"id": id.to_string(), "name": "peer", "paired": 0, "seen": 0});
        state.write("peers.json", &json!([record, record])).unwrap();
        assert_eq!(
            state.peer_addr("peer").unwrap_err().to_string(),
            "ambiguous peer name"
        );
        // A ticket that does not parse fails like any other link operation.
        state
            .write(
                "peers.json",
                &json!([{"id": id.to_string(), "name": "peer", "paired": 0, "seen": 0, "ticket": "invalid"}]),
            )
            .unwrap();
        assert!(state.peer_addr("peer").is_err());
        // A ticket for another endpoint is refused by identity, as Peer::addr refuses it.
        state
            .write(
                "peers.json",
                &json!([{"id": id.to_string(), "name": "peer", "paired": 0, "seen": 0,
                         "ticket": EndpointTicket::new(EndpointAddr::new(other)).to_string()}]),
            )
            .unwrap();
        assert_eq!(
            state.peer_addr("peer").unwrap_err().to_string(),
            "peer ticket identity mismatch"
        );
    }
}
