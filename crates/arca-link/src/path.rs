//! Named addresses win while they work; the path a peer is reached on is reported.
//!
//! iroh 1.4.1 has no per-peer "prefer this address" setting. The one lever is its `PathSelector` trait, which picks
//! the path that carries data among the open ones (iroh-1.4.1 `src/socket/remote_map/remote_state.rs`, `trait
//! PathSelector`), installed with `endpoint::Builder::path_selector` (`src/endpoint.rs`). Both sit behind the
//! `unstable-custom-transports` feature, which the workspace pins at `iroh = "=1.4.1"`. The built-in selector
//! (`src/socket/biased_rtt_path_selector.rs`) is crate-private, so its rule is repeated here with one more tier.
use crate::{Config, Peer, now_ms};
use iroh::{
    Endpoint, EndpointAddr, TransportAddr,
    endpoint::{
        TransportAddrUsage,
        transports::{Addr, PathSelection, PathSelectionContext, PathSelector},
    },
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddr,
    sync::Arc,
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

/// The addresses the config names for each peer, by peer name.
#[derive(Debug, Clone, Default)]
pub(crate) struct Named(Arc<BTreeMap<String, Vec<SocketAddr>>>);

impl Named {
    pub(crate) fn new(config: &Config) -> Self {
        Self(Arc::new(config.peers.clone()))
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub(crate) fn all(&self) -> BTreeSet<SocketAddr> {
        self.0.values().flatten().copied().collect()
    }
    /// The peer's recorded address plus every address the config names for it.
    pub(crate) fn dial(&self, name: &str, addr: EndpointAddr) -> EndpointAddr {
        self.0
            .get(name)
            .into_iter()
            .flatten()
            .fold(addr, |addr, ip| addr.with_ip_addr(*ip))
    }
}

/// Rank a path: a named address first, then any other direct address, then the relay.
#[derive(Debug)]
pub(crate) struct Prefer(pub(crate) BTreeSet<SocketAddr>);

const SWITCH_MIN: Duration = Duration::from_millis(5);

impl Prefer {
    fn tier(&self, path: &iroh::endpoint::transports::FourTuple) -> u8 {
        match path.remote() {
            Addr::Ip(addr) if self.0.contains(&addr) => 0,
            Addr::Ip(_) => 1,
            _ => 2,
        }
    }
}

impl PathSelector for Prefer {
    fn select(&self, ctx: &PathSelectionContext<'_>) -> PathSelection {
        let current = ctx.current();
        let mut best = None;
        let mut current_key = None;
        for data in ctx.paths() {
            let Some(stats) = data.stats() else { continue };
            let key = (self.tier(data.network_path()), stats.rtt);
            if Some(data.network_path()) == current && current_key.is_none_or(|c| key < c) {
                current_key = Some(key);
            }
            if best.as_ref().is_none_or(|(_, b)| key < *b) {
                best = Some((data, key));
            }
        }
        let mut selection = PathSelection::none();
        let Some((data, (tier, rtt))) = best else {
            return selection;
        };
        // Switch at once across tiers, and inside one tier only for a clear RTT gain, so a path does not flap.
        match current_key {
            Some((current_tier, current_rtt))
                if tier == current_tier && rtt + SWITCH_MIN > current_rtt => {}
            _ => selection.set(&data),
        }
        selection
    }
}

/// The path in use for each paired peer, by peer name.
pub(crate) async fn now(
    endpoint: &Endpoint,
    peers: &[Peer],
    named: &Named,
) -> BTreeMap<String, PathNow> {
    let at = now_ms();
    let all = named.all();
    let mut out = BTreeMap::new();
    for peer in peers {
        let Ok(id) = peer.id.parse() else { continue };
        let Some(info) = endpoint.remote_info(id).await else {
            continue;
        };
        // A named address in use wins the report, then any other direct one, then the relay.
        let active = info
            .addrs()
            .filter(|a| matches!(a.usage(), TransportAddrUsage::Active))
            .min_by_key(|a| match a.addr() {
                TransportAddr::Ip(ip) if all.contains(ip) => 0,
                TransportAddr::Ip(_) => 1,
                _ => 2,
            });
        let path = match active.map(|a| a.addr().clone()) {
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
        out.insert(peer.name.clone(), path);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::{EndpointId, RelayUrl, SecretKey, endpoint::transports::FourTuple};

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
        let named = Named::new(&config);
        let base = EndpointAddr::new(id);
        assert_eq!(
            named.dial("host", base.clone()),
            base.clone().with_ip_addr(addr)
        );
        assert_eq!(named.dial("other", base.clone()), base);
    }
}
