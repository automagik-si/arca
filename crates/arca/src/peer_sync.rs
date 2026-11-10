//! The page host's half of approvals on another machine: it keeps its own owner passkeys in sync on each linked kernel that
//! asks for them (an `approval.peerKeys` source's `from` names this host), so that kernel can verify the owner's approvals
//! itself.
//!
//! On start, every ten minutes, and when the approval page asks, the page host reads each linked kernel's `arca_status`. A
//! kernel whose `approval.peerKeys.from` is this host's approval host is sent this kernel's own passkey lines (the root file
//! and the state file, as the verifier holds them), in their normal form, with `peer_keys_sync`. The first set is kept there as
//! is. A set that differs from the kept one waits as a card on the approval page ("Trust your new passkey list on <host>")
//! until the owner signs the statement the target issued; that signed statement is relayed with the set.
//!
//! A kernel may keep several page hosts' passkeys (`peerKeys` as a list; its status lists them in `peerKeySources`). When
//! this host is its first source, the calls are what they have always been. When it is a further source, each call names
//! this host in `from`, so the target keeps the list in this host's own file.
//!
//! Lines are read from the kernel at each sync, sent only in the relayed call's body, and never logged, stored or returned.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::config::LinkedHost;
use crate::kernel::Kernel;
use crate::linked::{self, RelayError};

/// How often the page host syncs on its own.
pub const SYNC_EVERY: Duration = Duration::from_secs(600);
/// The shortest gap between two syncs the open approval page asks for. A request inside it answers from the last one.
pub const ON_DEMAND_GAP: Duration = Duration::from_secs(30);

/// A changed passkey list waiting for the owner's approval on one linked host: the statement the target issued and how many
/// passkeys the new list holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingTrust {
    pub statement: String,
    pub keys: usize,
}

/// What one sync with one linked host came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncOutcome {
    /// The host does not keep this host's passkeys (no `peerKeys`, or it names another page host).
    NotAsked,
    /// The host answered `peer_keys_sync` with this status (`trusted`, `unchanged`, `needs_approval` or `changed`).
    Synced(String),
    /// The host could not be asked, or refused.
    Failed(String),
}

/// The page host's syncer: its kernel (for its own passkeys and approval host), the linked hosts, and the lists waiting for
/// the owner.
#[derive(Clone)]
pub struct PeerSyncer {
    kernel: Arc<Mutex<Kernel>>,
    linked: Arc<Vec<LinkedHost>>,
    pending: Arc<Mutex<BTreeMap<String, PendingTrust>>>,
    /// The linked hosts that keep this host's passkeys as a further source: calls to them name this host in `from`.
    named: Arc<Mutex<BTreeSet<String>>>,
    log: Arc<dyn Fn(&str) + Send + Sync>,
    /// The last sync the page asked for: when it ran and what each host came to.
    on_demand: Arc<Mutex<Option<OnDemand>>>,
}

type OnDemand = (Instant, Vec<(String, SyncOutcome)>);

impl PeerSyncer {
    pub fn new(kernel: Arc<Mutex<Kernel>>, linked: Vec<LinkedHost>) -> Self {
        Self {
            kernel,
            linked: Arc::new(linked),
            pending: Arc::default(),
            named: Arc::default(),
            log: Arc::new(|line| eprintln!("{line}")),
            on_demand: Arc::default(),
        }
    }

    /// Replaces the log sink; tests capture what the syncer logs.
    pub fn with_log(mut self, log: Arc<dyn Fn(&str) + Send + Sync>) -> Self {
        self.log = log;
        self
    }

    /// The linked host named `name`.
    pub fn host(&self, name: &str) -> Option<LinkedHost> {
        self.linked.iter().find(|host| host.name == name).cloned()
    }

    /// The lists waiting for the owner, by linked host name.
    pub fn pending(&self) -> Vec<(String, PendingTrust)> {
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(name, pending)| (name.clone(), pending.clone()))
            .collect()
    }

    /// Syncs with every linked host, in order, and returns each one's outcome.
    pub fn sync_all(&self) -> Vec<(String, SyncOutcome)> {
        self.linked
            .iter()
            .map(|host| (host.name.clone(), self.sync(host)))
            .collect()
    }

    /// The sync the open approval page asks for: [`PeerSyncer::sync_all`], at most once per [`ON_DEMAND_GAP`]. A request
    /// inside the gap answers with the last outcomes, so an open route cannot make the page host call its linked hosts in a
    /// loop.
    pub fn sync_on_demand(&self) -> Vec<(String, SyncOutcome)> {
        let mut last = self
            .on_demand
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some((at, outcomes)) = last.as_ref()
            && at.elapsed() < ON_DEMAND_GAP
        {
            return outcomes.clone();
        }
        let outcomes = self.sync_all();
        *last = Some((Instant::now(), outcomes.clone()));
        outcomes
    }

    /// Syncs with one linked host: reads its status, and when it keeps this host's passkeys, sends them.
    pub fn sync(&self, host: &LinkedHost) -> SyncOutcome {
        let Some((me, lines)) = self.own() else {
            return SyncOutcome::Failed("this host's own passkeys could not be read".to_string());
        };
        // Whether the host keeps this host's passkeys, and whether as its first source (asked as always) or a further one
        // (asked by name).
        let asked = match linked::call(host, "arca_status", json!({})) {
            Ok(status) => {
                let first = status
                    .pointer("/approval/peerKeys/from")
                    .and_then(Value::as_str)
                    == Some(&me);
                let further = status
                    .pointer("/approval/peerKeySources")
                    .and_then(Value::as_array)
                    .is_some_and(|sources| {
                        sources
                            .iter()
                            .any(|source| source["from"].as_str() == Some(&me))
                    });
                if first {
                    Some(false)
                } else {
                    further.then_some(true)
                }
            }
            // A token that may not read the status belongs to a link set up only for relaying (a kernel on this machine
            // that reads this host's passkeys file itself): that host does not ask for the list.
            Err(RelayError::Token) => None,
            Err(error) => return self.failed(host, &error),
        };
        let Some(by_name) = asked else {
            self.forget(&host.name);
            return SyncOutcome::NotAsked;
        };
        {
            let mut named = self.named.lock().unwrap_or_else(PoisonError::into_inner);
            if by_name {
                named.insert(host.name.clone());
            } else {
                named.remove(&host.name);
            }
        }
        let mut call = json!({ "lines": lines });
        if by_name {
            call["from"] = json!(me);
        }
        match linked::call(host, "peer_keys_sync", call) {
            Ok(answer) => self.record(host, &answer),
            Err(error) => self.failed(host, &error),
        }
    }

    /// Sends this host's passkey list to `host` with the owner's passkey `proof` over the target's `statement`. The list is
    /// read again now, so the target checks the statement against the list it would keep.
    pub fn submit(
        &self,
        host: &LinkedHost,
        statement: &str,
        proof: &str,
    ) -> Result<Value, RelayError> {
        let Some((me, lines)) = self.own() else {
            return Err(RelayError::Failed(
                "this host's own passkeys could not be read".to_string(),
            ));
        };
        let mut call = json!({ "lines": lines, "statement": statement, "approval": proof });
        if self
            .named
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&host.name)
        {
            call["from"] = json!(me);
        }
        let answer = linked::call(host, "peer_keys_sync", call)?;
        self.record(host, &answer);
        Ok(answer)
    }

    /// Runs [`PeerSyncer::sync_all`] now and every [`SYNC_EVERY`] on its own thread.
    pub fn spawn(self) -> std::io::Result<()> {
        std::thread::Builder::new()
            .name("arca-peer-sync".to_string())
            .spawn(move || {
                loop {
                    self.sync_all();
                    std::thread::sleep(SYNC_EVERY);
                }
            })
            .map(|_| ())
    }

    /// This host's approval host name and its own passkey lines in normal form.
    fn own(&self) -> Option<(String, Vec<String>)> {
        let kernel = self.kernel.lock().ok()?;
        let lines = arca_approval::normal_lines(&kernel.owner_passkey_lines()).ok()?;
        Some((kernel.host().to_string(), lines))
    }

    fn record(&self, host: &LinkedHost, answer: &Value) -> SyncOutcome {
        let status = answer["status"].as_str().unwrap_or("unknown").to_string();
        let keys = answer["keys"].as_u64().unwrap_or(0);
        match (status.as_str(), answer["statement"].as_str()) {
            ("needs_approval", Some(statement)) => {
                self.pending
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(
                        host.name.clone(),
                        PendingTrust {
                            statement: statement.to_string(),
                            keys: usize::try_from(keys).unwrap_or(0),
                        },
                    );
            }
            _ => self.forget(&host.name),
        }
        if status != "unchanged" {
            (self.log)(&format!(
                "arca: passkey list on {}: {status} ({keys} passkey(s))",
                host.name
            ));
        }
        SyncOutcome::Synced(status)
    }

    fn failed(&self, host: &LinkedHost, error: &RelayError) -> SyncOutcome {
        let why = match error {
            RelayError::Unreachable => "it could not be reached".to_string(),
            RelayError::Token => "it does not accept this page's token".to_string(),
            RelayError::Refused { code, .. } => format!("it refused ({code})"),
            RelayError::Failed(_) => "it could not complete the call".to_string(),
        };
        (self.log)(&format!(
            "arca: the passkey list was not synced to {}: {why}",
            host.name
        ));
        SyncOutcome::Failed(why)
    }

    fn forget(&self, name: &str) {
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(name);
    }
}
