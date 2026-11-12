//! A private address a service of the node's own project may reach through the node: one IPv4 address and one tcp
//! port. The rules of a service's NIC reject every private range; a destination is the one address and port cut out
//! of that. Only the node's config names one, and only a create the owner approved records it.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::net::Ipv4Addr;

/// One address and tcp port, as a service's record carries it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Destination {
    pub address: Ipv4Addr,
    pub port: u16,
}

/// How many destinations one service may have.
pub const MAX_PER_SERVICE: usize = 4;
/// How many destinations a node's config may name in all.
pub const MAX_PER_NODE: usize = 16;

/// The ranges a destination is in: the private ranges and the one tailnets use. An address outside them is reached
/// without a destination, since a service's rules allow it already.
pub const RANGES: [(u32, u32); 4] = [
    (0x0a00_0000, 8),
    (0xac10_0000, 12),
    (0xc0a8_0000, 16),
    (0x6440_0000, 10),
];

fn mask(prefix: u32) -> u32 {
    u32::MAX.checked_shl(32 - prefix).unwrap_or(0)
}

/// Whether the range `base/prefix` holds `ip`.
pub fn holds(base: u32, prefix: u32, ip: u32) -> bool {
    ip & mask(prefix) == base
}

/// Two addresses of the tailnet range that are never a destination: the one every Tailscale answers on itself (its
/// DNS, and its own web page), which for a packet the node forwards is the node's own Tailscale, and the one a cloud's
/// metadata service answers on.
pub const NEVER: [Ipv4Addr; 2] = [
    Ipv4Addr::new(100, 100, 100, 100),
    Ipv4Addr::new(100, 100, 100, 200),
];

/// Why `address` and `port` are not a destination of a service whose bridge gateway is `gateway`, as the rule a
/// config breaks. `None` when they are one. Loopback, link-local (where a cloud's metadata answers), multicast and
/// every public address are outside the four ranges, and the service subnet is what links are for.
pub fn refusal(address: Ipv4Addr, port: u16, gateway: Ipv4Addr) -> Option<&'static str> {
    let ip = u32::from(address);
    if port == 0 {
        return Some("each port must be a tcp port from 1 to 65535");
    }
    if holds(u32::from(gateway) & mask(24), 24, ip) {
        return Some(
            "an address is inside kernel.serviceCidr: one service reaches another through a link",
        );
    }
    if NEVER.contains(&address) {
        return Some(
            "100.100.100.100 and 100.100.100.200 are never opened: the node's own Tailscale answers on the first, and a cloud's metadata service on the second",
        );
    }
    if !RANGES
        .iter()
        .any(|(base, prefix)| holds(*base, *prefix, ip))
    {
        return Some(
            "each address must be inside a private range (RFC 1918) or the tailnet range 100.64.0.0/10: a public address is reached already, and loopback, link-local and every other range are never opened",
        );
    }
    None
}

/// The IPv4 addresses that are the node's own among `hosts`, the host's addresses as Incus lists them (`a.b.c.d`, or
/// `a.b.c.d/len` when Incus gave the netmask): each address, and the network and broadcast addresses of each of its
/// networks, which the host itself answers on. None of them is ever a destination.
pub fn node_own(hosts: &serde_json::Value) -> BTreeSet<Ipv4Addr> {
    let mut own = BTreeSet::new();
    for host in hosts
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str())
    {
        let (address, length) = host.split_once('/').unwrap_or((host, "32"));
        let Ok(address) = address.parse::<Ipv4Addr>() else {
            continue;
        };
        own.insert(address);
        if let Some(length) = length.parse::<u32>().ok().filter(|n| (1..=30).contains(n)) {
            let ip = u32::from(address);
            own.insert(Ipv4Addr::from(ip & mask(length)));
            own.insert(Ipv4Addr::from(ip | !mask(length)));
        }
    }
    own
}

/// The range `base/prefix` without the smaller range `inner/inner_prefix` it holds, as the fewest CIDR ranges: one of
/// each length from `prefix + 1` to `inner_prefix`, lowest address first.
pub fn without(prefix: u32, inner: u32, inner_prefix: u32) -> Vec<(u32, u32)> {
    let mut ranges: Vec<(u32, u32)> = (prefix + 1..=inner_prefix)
        .map(|length| {
            let size = 1u32 << (32 - length);
            ((inner & mask(length)) ^ size, length)
        })
        .collect();
    ranges.sort_unstable();
    ranges
}

/// `ranges` without each address of `open`: the range that holds an address is replaced, where it stood, by the ranges
/// around that address.
pub fn cut(ranges: Vec<(u32, u32)>, open: impl IntoIterator<Item = Ipv4Addr>) -> Vec<(u32, u32)> {
    let mut ranges = ranges;
    for address in open {
        let ip = u32::from(address);
        ranges = ranges
            .into_iter()
            .flat_map(|(base, prefix)| {
                if holds(base, prefix, ip) {
                    without(prefix, ip, 32)
                } else {
                    vec![(base, prefix)]
                }
            })
            .collect();
    }
    ranges
}

/// Every tcp port from 1 to 65535 but `ports`, as Incus writes a list of ports and ranges: `1-442,444-65535`. Empty
/// when `ports` leaves none, and an empty port list in a rule means every port, so a caller writes no rule then.
pub fn other_ports(ports: &BTreeSet<u16>) -> String {
    let mut out = Vec::new();
    let mut next = 1u32;
    for port in ports
        .iter()
        .map(|port| u32::from(*port))
        .filter(|p| *p != 0)
    {
        if port > next {
            out.push((next, port - 1));
        }
        next = port + 1;
    }
    if next <= 65535 {
        out.push((next, 65535));
    }
    out.into_iter()
        .map(|(first, last)| {
            if first == last {
                first.to_string()
            } else {
                format!("{first}-{last}")
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// The destinations by address, each with its ports: what a rule set is built from. An address `closed` holds (the
/// node's own addresses, the bridge gateway and each network's broadcast address among them) is left out, so nothing
/// is ever cut for it, and so is one [`refusal`] would refuse.
pub fn by_address(
    destinations: &[Destination],
    gateway: Ipv4Addr,
    closed: &BTreeSet<Ipv4Addr>,
) -> BTreeMap<Ipv4Addr, BTreeSet<u16>> {
    let mut open: BTreeMap<Ipv4Addr, BTreeSet<u16>> = BTreeMap::new();
    for destination in destinations {
        if closed.contains(&destination.address)
            || refusal(destination.address, destination.port, gateway).is_some()
        {
            continue;
        }
        open.entry(destination.address)
            .or_default()
            .insert(destination.port);
    }
    open
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_destination_is_one_private_or_tailnet_address_outside_the_service_subnet() {
        let gateway = Ipv4Addr::new(10, 90, 0, 1);
        for ok in [
            "10.4.0.9",
            "172.16.0.1",
            "172.31.255.254",
            "100.64.0.1",
            "100.127.255.254",
            "10.90.1.1",
        ] {
            assert_eq!(refusal(ok.parse().unwrap(), 443, gateway), None, "{ok}");
        }
        for bad in [
            "8.8.8.8",
            "127.0.0.1",
            "169.254.169.254",
            "172.32.0.1",
            "100.128.0.1",
            "224.0.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "10.90.0.1",
            "10.90.0.77",
        ] {
            assert!(
                refusal(bad.parse().unwrap(), 443, gateway).is_some(),
                "{bad}"
            );
        }
        assert_eq!(refusal(Ipv4Addr::new(192, 168, 1, 5), 443, gateway), None);
        assert!(refusal("10.4.0.9".parse().unwrap(), 0, gateway).is_some());
        // The two addresses of the tailnet range that are never opened, and their neighbours, which are.
        for never in NEVER {
            assert!(refusal(never, 443, gateway).is_some(), "{never}");
            assert_eq!(
                refusal(Ipv4Addr::from(u32::from(never) + 1), 443, gateway),
                None
            );
        }
        // Both ends of every range are destinations like any other address of it; one step outside is not.
        for (base, prefix) in RANGES {
            let last = base | !mask(prefix);
            for inside in [base, last] {
                assert_eq!(refusal(Ipv4Addr::from(inside), 443, gateway), None);
            }
            for outside in [base - 1, last + 1] {
                assert!(refusal(Ipv4Addr::from(outside), 443, gateway).is_some());
            }
        }
        // The addresses beside the service /24 are outside it.
        for beside in ["10.89.255.255", "10.90.1.0"] {
            assert_eq!(
                refusal(beside.parse().unwrap(), 443, gateway),
                None,
                "{beside}"
            );
        }
    }

    #[test]
    fn the_nodes_own_addresses_are_each_address_and_the_ends_of_each_of_its_networks() {
        let own = node_own(&serde_json::json!([
            "10.20.30.1/24",
            "100.99.1.2/32",
            "172.20.7.5",
            "10.40.0.9/31",
            "10.50.3.77/22",
            "fd00::1/64",
            "nonsense"
        ]));
        let expected: BTreeSet<Ipv4Addr> = [
            "10.20.30.0",
            "10.20.30.1",
            "10.20.30.255",
            "100.99.1.2",
            "172.20.7.5",
            "10.40.0.9",
            "10.50.0.0",
            "10.50.3.77",
            "10.50.3.255",
        ]
        .iter()
        .map(|ip| ip.parse().unwrap())
        .collect();
        assert_eq!(own, expected);
    }

    #[test]
    fn a_range_without_one_address_is_every_other_address_of_it_and_nothing_else() {
        let address = Ipv4Addr::new(10, 4, 0, 9);
        let ranges = cut(vec![(0x0a00_0000, 8), (0xac10_0000, 12)], [address]);
        // One range of each length from /9 to /32, and the untouched range after them.
        assert_eq!(ranges.len(), 25);
        assert_eq!(ranges[24], (0xac10_0000, 12));
        let covered: u64 = ranges[..24]
            .iter()
            .map(|(_, prefix)| 1u64 << (32 - prefix))
            .sum();
        assert_eq!(covered, (1u64 << 24) - 1);
        let ip = u32::from(address);
        assert!(
            !ranges
                .iter()
                .any(|(base, prefix)| holds(*base, *prefix, ip))
        );
        for near in [ip - 1, ip + 1, 0x0a00_0000, 0x0aff_ffff] {
            assert_eq!(
                ranges
                    .iter()
                    .filter(|(base, prefix)| holds(*base, *prefix, near))
                    .count(),
                1
            );
        }
        // A second address of the same range cuts one of the ranges the first one left.
        let two = cut(ranges, [Ipv4Addr::new(10, 200, 0, 1)]);
        let covered: u64 = two[..two.len() - 1]
            .iter()
            .map(|(_, prefix)| 1u64 << (32 - prefix))
            .sum();
        assert_eq!(covered, (1u64 << 24) - 2);
    }

    #[test]
    fn the_other_ports_are_every_port_but_the_named_ones() {
        let ports = |list: &[u16]| other_ports(&list.iter().copied().collect());
        assert_eq!(ports(&[443]), "1-442,444-65535");
        assert_eq!(ports(&[1]), "2-65535");
        assert_eq!(ports(&[65535]), "1-65534");
        assert_eq!(ports(&[2, 443, 444]), "1,3-442,445-65535");
        assert_eq!(ports(&[]), "1-65535");
    }
}
