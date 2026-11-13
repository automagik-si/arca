//! Hints addresses win while they work; the path a peer is reached on is reported.
//!
//! iroh 1.4.1 has no per-peer "prefer this address" setting. The one lever is its `PathSelector` trait, which picks
//! the path that carries data among the open ones (iroh-1.4.1 `src/socket/remote_map/remote_state.rs`, `trait
//! PathSelector`), installed with `endpoint::Builder::path_selector` (`src/endpoint.rs`). Both sit behind the
//! `unstable-custom-transports` feature, which the workspace pins at `iroh = "=1.4.1"`. The built-in selector
//! (`src/socket/biased_rtt_path_selector.rs`) is crate-private, so its rule is written again here: lowest round trip
//! wins inside a tier with a 5 ms switching margin, IPv6 gets the same 3 ms advantage, the relay is a backup tier. The
//! one difference is the first tier, a named address, and that a custom transport ranks with the relay.
//!
//! Re-check when iroh is updated (the exact pin hides all three; each fails silently, not at compile time):
//! - when `select` is called: `remote_state.rs` calls it on a connection added and a path established or abandoned;
//! - the report below uses only `Connection::paths()` and `Path::is_selected`, never `TransportAddrUsage::Active`,
//!   which stays set after a connection closes;
//! - only the QUIC client opens and closes paths (`open_path_on_conn`, `apply_selected_path`), so a node's `peers`
//!   governs the connections it opens.
use crate::{Config, Peer, now_ms};
use iroh::{
    EndpointAddr, EndpointId, TransportAddr,
    endpoint::{
        Connection, WeakConnectionHandle,
        transports::{Addr, FourTuple, PathSelection, PathSelectionContext, PathSelector},
    },
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

/// The path one peer is reached on now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathNow {
    /// `direct`, `relay` or `idle` (no path in use at the moment).
    pub via: String,
    /// The address in use: the peer's ip:port when direct, the relay's URL when relayed.
    pub addr: Option<String>,
    /// Whether that address is one the config's `peers` names.
    pub named: bool,
    pub at: u64,
}

/// The addresses the config names for each peer, by peer name, and the connections to report paths from.
#[derive(Debug, Clone, Default)]
pub(crate) struct Hints {
    named: Arc<BTreeMap<String, Vec<SocketAddr>>>,
    live: Arc<Mutex<Vec<(EndpointId, WeakConnectionHandle)>>>,
}

impl Hints {
    pub(crate) fn new(config: &Config) -> Self {
        Self {
            named: Arc::new(config.peers.clone()),
            live: Arc::default(),
        }
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.named.is_empty()
    }
    pub(crate) fn all(&self) -> BTreeSet<SocketAddr> {
        self.named.values().flatten().copied().collect()
    }
    /// Remember a connection, without keeping it open, so the report can read its selected path.
    pub(crate) fn track(&self, connection: &Connection) {
        let mut live = self.live.lock().expect("live lock");
        live.retain(|(_, weak)| weak.upgrade().is_some_and(|c| c.close_reason().is_none()));
        live.push((connection.remote_id(), connection.weak_handle()));
    }
    /// The peer's recorded address plus every address the config names for it.
    pub(crate) fn dial(&self, name: &str, addr: EndpointAddr) -> EndpointAddr {
        self.named
            .get(name)
            .into_iter()
            .flatten()
            .fold(addr, |addr, ip| addr.with_ip_addr(*ip))
    }
}

/// A rank a path has: a named address first, then any other direct address, then the relay. The number is the
/// round trip in nanoseconds, less 3 ms for IPv6 as in iroh's own rule.
type Key = (u8, i128);
const SWITCH_MIN: Duration = Duration::from_millis(5);
const IPV6_ADVANTAGE: Duration = Duration::from_millis(3);

#[derive(Debug)]
pub(crate) struct Prefer(pub(crate) BTreeSet<SocketAddr>);

impl Prefer {
    fn tier(&self, path: &FourTuple) -> u8 {
        match path.remote() {
            Addr::Ip(addr) if self.0.contains(&addr) => 0,
            Addr::Ip(_) => 1,
            _ => 2,
        }
    }
    fn key(&self, path: &FourTuple, rtt: Duration) -> Key {
        let bias = match path.remote() {
            Addr::Ip(addr) if addr.is_ipv6() => IPV6_ADVANTAGE.as_nanos() as i128,
            _ => 0,
        };
        (self.tier(path), rtt.as_nanos() as i128 - bias)
    }
}

/// Whether to move from the current path to the best one: at once across tiers, inside a tier only for a clear gain,
/// and always when the current path is gone.
fn switch(current: Option<Key>, best: Key) -> bool {
    match current {
        Some((tier, rtt)) if tier == best.0 => best.1 + SWITCH_MIN.as_nanos() as i128 <= rtt,
        _ => true,
    }
}

impl PathSelector for Prefer {
    fn select(&self, ctx: &PathSelectionContext<'_>) -> PathSelection {
        let current = ctx.current();
        let mut best = None;
        let mut current_key = None;
        for data in ctx.paths() {
            let Some(stats) = data.stats() else { continue };
            let key = self.key(data.network_path(), stats.rtt);
            if Some(data.network_path()) == current && current_key.is_none_or(|c| key < c) {
                current_key = Some(key);
            }
            if best.as_ref().is_none_or(|(_, b)| key < *b) {
                best = Some((data, key));
            }
        }
        let mut selection = PathSelection::none();
        if let Some((data, key)) = best
            && switch(current_key, key)
        {
            selection.set(&data);
        }
        selection
    }
}

/// The path in use for each paired peer, by peer name, read from the connections that are open now: each one's
/// selected path, a named address first when several are open. A peer with no open connection is `idle`.
pub(crate) fn now(peers: &[Peer], hints: &Hints) -> BTreeMap<String, PathNow> {
    let at = now_ms();
    let all = hints.all();
    let live = hints.live.lock().expect("live lock");
    let open: Vec<Connection> = live
        .iter()
        .filter_map(|(_, weak)| weak.upgrade())
        .filter(|c| c.close_reason().is_none())
        .collect();
    drop(live);
    peers
        .iter()
        .map(|peer| {
            let selected = open
                .iter()
                .filter(|c| c.remote_id().to_string() == peer.id)
                .filter_map(|c| {
                    c.paths()
                        .iter()
                        .find(|p| p.is_selected())
                        .map(|p| p.remote_addr().clone())
                })
                .min_by_key(|addr| match addr {
                    TransportAddr::Ip(ip) if all.contains(ip) => 0,
                    TransportAddr::Ip(_) => 1,
                    _ => 2,
                });
            let path = match selected {
                Some(TransportAddr::Ip(ip)) => PathNow {
                    via: "direct".into(),
                    addr: Some(ip.to_string()),
                    named: all.contains(&ip),
                    at,
                },
                Some(TransportAddr::Relay(url)) => PathNow {
                    via: "relay".into(),
                    addr: Some(url.to_string()),
                    named: false,
                    at,
                },
                _ => PathNow {
                    via: "idle".into(),
                    addr: None,
                    named: false,
                    at,
                },
            };
            (peer.name.clone(), path)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::{RelayUrl, SecretKey};

    #[test]
    fn a_named_address_ranks_before_any_other_direct_address_and_the_relay() {
        let named: SocketAddr = "10.0.0.2:7000".parse().unwrap();
        let prefer = Prefer(BTreeSet::from([named]));
        let id: EndpointId = SecretKey::from_bytes(&[7; 32]).public();
        let ip = |a: SocketAddr| FourTuple::from_remote(Addr::Ip(a));
        let relay = FourTuple::from_remote(Addr::Relay(
            "https://relay.example.com".parse::<RelayUrl>().unwrap(),
            id,
        ));
        assert_eq!(prefer.tier(&ip(named)), 0);
        assert_eq!(prefer.tier(&ip("10.0.0.2:7001".parse().unwrap())), 1);
        assert_eq!(prefer.tier(&ip("192.0.2.9:7000".parse().unwrap())), 1);
        assert_eq!(prefer.tier(&relay), 2);
    }

    #[test]
    fn dial_adds_only_the_named_peers_addresses() {
        let id: EndpointId = SecretKey::from_bytes(&[7; 32]).public();
        let addr: SocketAddr = "10.0.0.2:7000".parse().unwrap();
        let config = Config {
            peers: BTreeMap::from([("host".to_string(), vec![addr])]),
            ..Config::default()
        };
        let named = Hints::new(&config);
        let base = EndpointAddr::new(id);
        assert_eq!(
            named.dial("host", base.clone()),
            base.clone().with_ip_addr(addr)
        );
        assert_eq!(named.dial("other", base.clone()), base);
    }

    #[test]
    fn a_switch_needs_another_tier_a_clear_gain_or_a_lost_current_path() {
        let ms = |n: i128| n * 1_000_000;
        assert!(switch(None, (1, ms(30))), "no current path");
        assert!(
            switch(Some((2, ms(1))), (1, ms(50))),
            "a better tier wins at once"
        );
        assert!(
            !switch(Some((1, ms(20))), (1, ms(16))),
            "4 ms is not enough"
        );
        assert!(switch(Some((1, ms(20))), (1, ms(15))), "5 ms is");
        assert!(
            !switch(Some((0, ms(40))), (0, ms(40))),
            "the same path stays"
        );
    }
}
