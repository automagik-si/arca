use iroh::endpoint::IncomingAddr;
use std::{
    collections::BTreeMap,
    net::IpAddr,
    sync::{Arc, Mutex},
};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Source {
    V4(std::net::Ipv4Addr),
    V6([u8; 8]),
    Relay(iroh::EndpointId),
    Custom(u64),
    Other,
}
impl Source {
    fn from(addr: IncomingAddr) -> Self {
        match addr {
            IncomingAddr::Ip(addr) => match addr.ip().to_canonical() {
                IpAddr::V4(ip) => Self::V4(ip),
                IpAddr::V6(ip) => Self::V6(ip.octets()[..8].try_into().expect("eight bytes")),
            },
            IncomingAddr::Relay { endpoint_id, .. } => Self::Relay(endpoint_id),
            IncomingAddr::Custom(addr) => Self::Custom(addr.id()),
            _ => Self::Other,
        }
    }
}

#[derive(Default)]
struct Counts {
    pending: usize,
    open: usize,
    sources: BTreeMap<Source, usize>,
}
#[derive(Clone, Default)]
pub(crate) struct Guard(Arc<Mutex<Counts>>);
pub(crate) struct Slot {
    guard: Guard,
    source: Source,
    pending: bool,
}
impl Guard {
    pub fn take(&self, addr: IncomingAddr) -> Option<Slot> {
        let source = Source::from(addr);
        let mut counts = self.0.lock().expect("guard lock");
        if counts.pending >= 16
            || counts.open >= 64
            || counts.sources.get(&source).copied().unwrap_or(0) >= 4
        {
            return None;
        }
        counts.pending += 1;
        counts.open += 1;
        *counts.sources.entry(source.clone()).or_default() += 1;
        Some(Slot {
            guard: self.clone(),
            source,
            pending: true,
        })
    }
}
impl Slot {
    pub fn trust(&mut self) {
        if self.pending {
            let mut counts = self.guard.0.lock().expect("guard lock");
            counts.pending -= 1;
            let count = counts
                .sources
                .get_mut(&self.source)
                .expect("pending source");
            *count -= 1;
            if *count == 0 {
                counts.sources.remove(&self.source);
            }
            self.pending = false;
        }
    }
}
impl Drop for Slot {
    fn drop(&mut self) {
        self.trust();
        self.guard.0.lock().expect("guard lock").open -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn address() -> IncomingAddr {
        let mut bytes = [0; 4];
        getrandom::fill(&mut bytes).unwrap();
        IncomingAddr::Ip((std::net::Ipv4Addr::from(bytes), 0).into())
    }

    #[test]
    fn caps_release_pending_and_open_slots() {
        let guard = Guard::default();
        let source = address();
        let mut slots: Vec<_> = (0..4)
            .map(|_| guard.take(source.clone()).unwrap())
            .collect();
        assert!(guard.take(source.clone()).is_none());
        slots.pop();
        assert!(guard.take(source.clone()).is_some());
        for slot in &mut slots {
            slot.trust();
        }
        assert!(guard.take(source).is_some());
        drop(slots);
        let mut slots: Vec<_> = (0..16).map(|_| guard.take(address()).unwrap()).collect();
        assert!(guard.take(address()).is_none());
        for slot in &mut slots {
            slot.trust();
        }
        for _ in 16..64 {
            let mut slot = guard.take(address()).unwrap();
            slot.trust();
            slots.push(slot);
        }
        assert!(guard.take(address()).is_none());
        slots.pop();
        assert!(guard.take(address()).is_some());
        drop(slots);
        let counts = guard.0.lock().unwrap();
        assert_eq!(counts.pending, 0);
        assert_eq!(counts.open, 0);
        assert!(counts.sources.is_empty());
    }

    #[test]
    fn ipv6_sources_share_a_64_bit_prefix() {
        let mut bytes = [0; 16];
        getrandom::fill(&mut bytes).unwrap();
        let first = IncomingAddr::Ip((std::net::Ipv6Addr::from(bytes), 0).into());
        bytes[15] ^= 1;
        let second = IncomingAddr::Ip((std::net::Ipv6Addr::from(bytes), 1).into());
        assert_eq!(Source::from(first), Source::from(second));
    }
}
