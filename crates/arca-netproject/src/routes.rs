//! The host's IPv4 routing table, as `/proc/net/route` prints it: what `arca init` keeps a new service network clear of,
//! and what a runner pool's own bridge keeps clear of.

use std::net::Ipv4Addr;

/// Where the kernel prints its main IPv4 routing table.
pub const ROUTES: &str = "/proc/net/route";

/// The routes of `table`: one a line after the heading, the destination and the mask as eight hexadecimal digits in
/// the machine's byte order, each as `(network, prefix length, interface)`. The default route is every address and
/// says nothing of what is in use, so it is left out.
pub fn parse(table: &str) -> Vec<(Ipv4Addr, u32, String)> {
    unimplemented!("{table}")
}

/// [`parse`] as `(destination CIDR, interface)`.
pub fn routed_networks(table: &str) -> Vec<(String, String)> {
    parse(table)
        .into_iter()
        .map(|(network, prefix, interface)| (format!("{network}/{prefix}"), interface))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_routes_with_their_interfaces_and_never_the_default_route() {
        // The kernel writes the destination and the mask in the machine's byte order: 192.0.2.0/24 and 10.47.0.0/16.
        let table = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
             eth0\t00000000\t010200C0\t0003\t0\t0\t100\t00000000\t0\t0\t0\n\
             eth0\t000200C0\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0\n\
             wg0\t00002F0A\t00000000\t0001\t0\t0\t0\t0000FFFF\t0\t0\t0\n\
             bad\tnothex\n";
        assert_eq!(
            routed_networks(table),
            vec![
                ("192.0.2.0/24".to_string(), "eth0".to_string()),
                ("10.47.0.0/16".to_string(), "wg0".to_string()),
            ]
        );
    }
}
