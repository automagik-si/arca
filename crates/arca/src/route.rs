//! Routes: the names a person types to reach an instance of this node, served by the node's edge.
//!
//! A route is one hostname, the instance that serves it (in any project of this node), the instance's tcp port and
//! address, and its reach. The routes of a node live in the desired-state record of its edge: the one service made
//! from a template that says `"edge": true` (the shipped `arca-edge`, Caddy with the Cloudflare DNS module). From them
//! the kernel renders the edge's whole Caddyfile ([`caddyfile`]), writes it into the edge and reloads Caddy, and opens
//! each target address and port in the edge's NIC rules ([`destinations`]).
//!
//! The render is a pure function of the routes: sorted by hostname, every value checked when the route was made, so
//! nothing a caller gives can add a line of its own to the file.

use std::net::Ipv4Addr;

use arca_netproject::destination::Destination;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Where Caddy reads its config in the edge: the path the `arca-edge` template's entrypoint names.
pub use arca_incus::exec::EDGE_CONFIG as EDGE_CONFIG_PATH;

/// The address ranges a private route answers. Every other client gets 403.
pub const PRIVATE_RANGES: [&str; 4] = [
    "10.0.0.0/8",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "100.64.0.0/10",
];

/// Who may reach a route.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Reach {
    /// The local networks and the tailnet ([`PRIVATE_RANGES`]); every other client gets 403.
    Private,
    /// Anyone who reaches the edge, and (later) the internet through the node's Cloudflare tunnel.
    Public,
}

impl Reach {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "private" => Some(Self::Private),
            "public" => Some(Self::Public),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Public => "public",
        }
    }

    /// The words an approval card and a list say for this reach.
    pub fn words(self) -> &'static str {
        match self {
            Self::Private => {
                "only from the local networks and the tailnet (10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, 100.64.0.0/10); every other address gets 403"
            }
            Self::Public => "from any address that reaches the edge, with no address filter",
        }
    }
}

/// One route, as the edge's record carries it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Route {
    pub hostname: String,
    /// The Incus instance that serves the route, and its project on this node.
    pub instance: String,
    pub project: String,
    /// The instance's own IPv4 address, read from Incus when the route was added: what the edge connects to.
    pub address: Ipv4Addr,
    pub port: u16,
    pub reach: Reach,
}

impl Route {
    /// Whether the record is one the render may write: a hostname [`hostname_ok`] takes, a port, and an instance name.
    pub fn ok(&self) -> bool {
        hostname_ok(&self.hostname)
            && self.port != 0
            && arca_incus::exec::node_ok(&self.instance)
            && project_ok(&self.project)
    }

    /// The route as `route_list` and the verbs answer it.
    pub fn view(&self) -> Value {
        json!({
            "hostname": self.hostname,
            "url": format!("https://{}", self.hostname),
            "instance": self.instance,
            "project": self.project,
            "address": self.address.to_string(),
            "port": self.port,
            "reach": self.reach.as_str(),
        })
    }
}

/// What an edge's record holds: its routes, sorted by hostname, one per hostname.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EdgeRecord {
    #[serde(default)]
    pub routes: Vec<Route>,
}

impl EdgeRecord {
    /// Puts `route` in place of one of the same hostname, keeping the list sorted.
    pub fn put(&mut self, route: Route) {
        self.routes.retain(|held| held.hostname != route.hostname);
        self.routes.push(route);
        self.routes.sort_by(|a, b| a.hostname.cmp(&b.hostname));
    }

    /// Takes the route of `hostname` out; the route it held, if any.
    pub fn remove(&mut self, hostname: &str) -> Option<Route> {
        let at = self
            .routes
            .iter()
            .position(|held| held.hostname == hostname)?;
        Some(self.routes.remove(at))
    }

    pub fn find(&self, hostname: &str) -> Option<&Route> {
        self.routes.iter().find(|held| held.hostname == hostname)
    }
}

/// Whether `name` is a hostname a route may have: lowercase, at least two labels, each label 1 to 63 of `a-z`, `0-9`
/// and `-`, not starting or ending with `-`, 253 characters at most, and no wildcard. Nothing else can reach the
/// Caddyfile, so nothing else is let in.
pub fn hostname_ok(name: &str) -> bool {
    if name.is_empty() || name.len() > 253 {
        return false;
    }
    let labels: Vec<&str> = name.split('.').collect();
    labels.len() >= 2
        && labels.iter().all(|label| {
            (1..=63).contains(&label.len())
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
        && labels
            .last()
            .is_some_and(|tld| tld.bytes().any(|b| b.is_ascii_lowercase()))
}

/// Whether `project` is an Incus project name a route may name: 1 to 63 of letters, digits, `-` and `_`.
pub fn project_ok(project: &str) -> bool {
    (1..=63).contains(&project.len())
        && project
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

/// The first IPv4 address of an instance's NIC as Incus's full instance reading gives it: the address of the first NIC
/// (by device name) with a fixed `ipv4.address`, else the first global IPv4 address the running instance holds on one
/// of its own Incus NICs. A NIC is found in `state.network` by its MAC address (`volatile.<nic>.hwaddr`, or the
/// device's own `hwaddr`), so a bridge the instance runs inside itself (docker, k8s) is never taken for it. This works
/// for a VM too, whose interfaces have the guest's names.
pub fn instance_address(full: &Value) -> Option<Ipv4Addr> {
    for devices in [&full["expanded_devices"], &full["devices"]] {
        let mut fixed: Vec<(&String, Ipv4Addr)> = devices
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(_, device)| device["type"] == "nic")
            .filter_map(|(name, device)| {
                device["ipv4.address"]
                    .as_str()
                    .and_then(|text| text.parse::<Ipv4Addr>().ok())
                    .map(|address| (name, address))
            })
            .collect();
        fixed.sort_by(|a, b| a.0.cmp(b.0));
        if let Some((_, address)) = fixed.first() {
            return Some(*address);
        }
    }
    let devices = full["expanded_devices"]
        .as_object()
        .filter(|devices| !devices.is_empty())
        .or_else(|| full["devices"].as_object())?;
    let mut nics: Vec<(&String, String)> = devices
        .iter()
        .filter(|(_, device)| device["type"] == "nic")
        .filter_map(|(name, device)| {
            full["config"][format!("volatile.{name}.hwaddr")]
                .as_str()
                .or_else(|| device["hwaddr"].as_str())
                .map(|mac| (name, mac.to_ascii_lowercase()))
        })
        .collect();
    nics.sort_by(|a, b| a.0.cmp(b.0));
    let network = full["state"]["network"].as_object()?;
    nics.into_iter().find_map(|(_, mac)| {
        let (_, interface) = network.iter().find(|(_, interface)| {
            interface["hwaddr"]
                .as_str()
                .is_some_and(|hwaddr| hwaddr.eq_ignore_ascii_case(&mac))
        })?;
        interface["addresses"]
            .as_array()?
            .iter()
            .filter(|address| address["family"] == "inet" && address["scope"] == "global")
            .find_map(|address| address["address"].as_str()?.parse::<Ipv4Addr>().ok())
    })
}

/// The addresses and ports the edge's NIC must open for its routes: each route's target, once. An address inside the
/// service /24 is a neighbour the edge reaches through a link, and the rules leave it out themselves.
pub fn destinations(record: &EdgeRecord) -> Vec<Destination> {
    let mut out: Vec<Destination> = record
        .routes
        .iter()
        .map(|route| Destination {
            address: route.address,
            port: route.port,
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The whole Caddyfile of an edge whose record is `record`: the global options, then one site per route in hostname
/// order. The same routes always render the same bytes.
///
/// - Certificates come by ACME DNS challenge through Cloudflare, with the token the edge's environment holds.
/// - The edge is reached through an Incus proxy device that sends the PROXY protocol header, so Caddy reads the
///   client's own address from it; only the device's loopback connection may send one.
/// - HTTP/1.1 and HTTP/2 only: the device carries tcp.
/// - A private site answers 403 to every address outside [`PRIVATE_RANGES`].
/// - Every site streams its answers unbuffered (`flush_interval -1`), for server-sent events and LLM completions.
pub fn caddyfile(record: &EdgeRecord) -> String {
    let mut out = String::new();
    out.push_str("# Written by Arca from this node's routes (route_add, route_remove). Each change replaces the whole file.\n");
    out.push_str("{\n");
    out.push_str("\tadmin localhost:2019\n");
    out.push_str("\tacme_dns cloudflare {env.CLOUDFLARE_API_TOKEN}\n");
    out.push_str("\tservers {\n");
    out.push_str("\t\tlistener_wrappers {\n");
    out.push_str("\t\t\tproxy_protocol {\n");
    out.push_str("\t\t\t\ttimeout 5s\n");
    out.push_str("\t\t\t\tallow 127.0.0.1/32\n");
    out.push_str("\t\t\t}\n");
    out.push_str("\t\t\ttls\n");
    out.push_str("\t\t}\n");
    out.push_str("\t\tprotocols h1 h2\n");
    out.push_str("\t}\n");
    out.push_str("}\n");
    let mut routes: Vec<&Route> = record.routes.iter().filter(|route| route.ok()).collect();
    routes.sort_by(|a, b| a.hostname.cmp(&b.hostname));
    routes.dedup_by(|a, b| a.hostname == b.hostname);
    for route in routes {
        out.push('\n');
        out.push_str(&format!(
            "# {} -> {} ({}) port {}, {}\n",
            route.hostname,
            route.instance,
            route.project,
            route.port,
            route.reach.as_str()
        ));
        out.push_str(&format!("{} {{\n", route.hostname));
        if route.reach == Reach::Private {
            out.push_str(&format!(
                "\t@outside not remote_ip {}\n",
                PRIVATE_RANGES.join(" ")
            ));
            out.push_str("\trespond @outside 403\n");
        }
        out.push_str(&format!(
            "\treverse_proxy {}:{} {{\n",
            route.address, route.port
        ));
        out.push_str("\t\tflush_interval -1\n");
        out.push_str("\t}\n");
        out.push_str("\theader -Server\n");
        out.push_str("}\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(hostname: &str, reach: Reach) -> Route {
        Route {
            hostname: hostname.into(),
            instance: "app".into(),
            project: "default".into(),
            address: Ipv4Addr::new(192, 0, 2, 10),
            port: 8080,
            reach,
        }
    }

    #[test]
    fn a_hostname_is_lowercase_labels_and_nothing_that_could_write_a_line() {
        for good in [
            "llm.example.com",
            "a.b",
            "x-1.example.com",
            "1a.example.org",
        ] {
            assert!(hostname_ok(good), "{good}");
        }
        for bad in [
            "",
            "example",
            "LLM.example.com",
            "*.example.com",
            "-a.example.com",
            "a-.example.com",
            "a..example.com",
            "a.example.com.",
            "a.example.com {",
            "a.example.com\nrespond 200",
            "a b.example.com",
            "a.123",
        ] {
            assert!(!hostname_ok(bad), "{bad:?}");
        }
        assert!(!hostname_ok(&format!("{}.com", "a".repeat(64))));
    }

    #[test]
    fn the_render_is_the_same_bytes_for_the_same_routes_in_any_order() {
        let mut one = EdgeRecord::default();
        one.put(route("b.example.com", Reach::Private));
        one.put(route("a.example.com", Reach::Public));
        let mut two = EdgeRecord::default();
        two.put(route("a.example.com", Reach::Public));
        two.put(route("b.example.com", Reach::Private));
        assert_eq!(caddyfile(&one), caddyfile(&two));
        let text = caddyfile(&one);
        assert!(text.find("a.example.com {").unwrap() < text.find("b.example.com {").unwrap());
    }

    #[test]
    fn a_private_site_answers_403_outside_the_private_ranges_and_a_public_one_has_no_filter() {
        let mut record = EdgeRecord::default();
        record.put(route("private.example.com", Reach::Private));
        record.put(route("public.example.com", Reach::Public));
        let text = caddyfile(&record);
        let private = &text[text.find("private.example.com {").unwrap()..];
        let private = &private[..private.find("\n}\n").unwrap()];
        assert!(private.contains(
            "@outside not remote_ip 10.0.0.0/8 172.16.0.0/12 192.168.0.0/16 100.64.0.0/10"
        ));
        assert!(private.contains("respond @outside 403"));
        assert!(private.contains("reverse_proxy 192.0.2.10:8080"));
        assert!(private.contains("flush_interval -1"));
        let public = &text[text.find("public.example.com {").unwrap()..];
        let public = &public[..public.find("\n}\n").unwrap()];
        assert!(!public.contains("remote_ip"));
        assert!(public.contains("reverse_proxy 192.0.2.10:8080"));
    }

    #[test]
    fn the_global_options_read_the_client_address_from_the_proxy_header_and_use_the_dns_challenge()
    {
        let text = caddyfile(&EdgeRecord::default());
        assert!(text.contains("acme_dns cloudflare {env.CLOUDFLARE_API_TOKEN}"));
        assert!(text.contains("proxy_protocol {"));
        assert!(text.contains("allow 127.0.0.1/32"));
        assert!(text.contains("protocols h1 h2"));
        // With no route there is no site at all.
        assert!(!text.contains("reverse_proxy"));
    }

    #[test]
    fn a_record_that_does_not_check_out_is_never_rendered() {
        let mut record = EdgeRecord::default();
        let mut bad = route("bad.example.com", Reach::Private);
        bad.hostname = "bad.example.com {\n}\nevil.example.com".into();
        record.routes.push(bad);
        let mut zero = route("zero.example.com", Reach::Private);
        zero.port = 0;
        record.routes.push(zero);
        let text = caddyfile(&record);
        assert!(!text.contains("evil"));
        assert!(!text.contains("zero.example.com"));
    }

    #[test]
    fn put_replaces_the_route_of_a_hostname_and_remove_takes_it_out() {
        let mut record = EdgeRecord::default();
        record.put(route("a.example.com", Reach::Private));
        let mut moved = route("a.example.com", Reach::Private);
        moved.address = Ipv4Addr::new(192, 0, 2, 20);
        record.put(moved.clone());
        assert_eq!(record.routes, vec![moved.clone()]);
        assert_eq!(record.remove("a.example.com"), Some(moved));
        assert_eq!(record.remove("a.example.com"), None);
        assert!(record.routes.is_empty());
    }

    #[test]
    fn destinations_are_each_target_once() {
        let mut record = EdgeRecord::default();
        record.put(route("a.example.com", Reach::Private));
        record.put(route("b.example.com", Reach::Public));
        assert_eq!(
            destinations(&record),
            vec![Destination {
                address: Ipv4Addr::new(192, 0, 2, 10),
                port: 8080
            }]
        );
    }

    #[test]
    fn an_instance_address_is_its_fixed_nic_address_or_else_its_own_nics_first_global_one() {
        let fixed = json!({"expanded_devices": {"eth0": {"type":"nic","ipv4.address":"192.0.2.63"},
            "root": {"type":"disk"}}});
        assert_eq!(instance_address(&fixed), Some(Ipv4Addr::new(192, 0, 2, 63)));
        // A docker compose bridge (br-<id>) and a docker0 sort before eth0 and hold global addresses: only the
        // interface whose MAC is the NIC's own is the instance's.
        let dynamic = json!({"expanded_devices": {"eth0": {"type":"nic","network":"br"}},
        "config": {"volatile.eth0.hwaddr": "00:16:3E:00:00:0A"},
        "state": {"network": {
            "lo": {"hwaddr":"","addresses":[{"family":"inet","address":"127.0.0.1","scope":"local"}]},
            "br-0123456789ab": {"hwaddr":"02:42:00:00:00:01","addresses":[{"family":"inet","address":"172.18.0.1","scope":"global"}]},
            "docker0": {"hwaddr":"02:42:00:00:00:02","addresses":[{"family":"inet","address":"198.51.100.1","scope":"global"}]},
            "eth0": {"hwaddr":"00:16:3e:00:00:0a","addresses":[{"family":"inet6","address":"fe80::1","scope":"link"},
                {"family":"inet","address":"192.0.2.10","scope":"global"}]}
        }}});
        assert_eq!(
            instance_address(&dynamic),
            Some(Ipv4Addr::new(192, 0, 2, 10))
        );
        // A VM names its interfaces itself: the MAC still finds the NIC.
        let vm = json!({"expanded_devices": {"eth0": {"type":"nic","network":"br"}},
        "config": {"volatile.eth0.hwaddr": "00:16:3e:00:00:0b"},
        "state": {"network": {
            "enp5s0": {"hwaddr":"00:16:3e:00:00:0b","addresses":[{"family":"inet","address":"192.0.2.11","scope":"global"}]}
        }}});
        assert_eq!(instance_address(&vm), Some(Ipv4Addr::new(192, 0, 2, 11)));
        // With no interface of its own NICs, there is no address, never a bridge's.
        let only_bridges = json!({"expanded_devices": {"eth0": {"type":"nic","network":"br"}},
        "config": {"volatile.eth0.hwaddr": "00:16:3e:00:00:0c"},
        "state": {"network": {
            "br-0123456789ab": {"hwaddr":"02:42:00:00:00:01","addresses":[{"family":"inet","address":"172.18.0.1","scope":"global"}]}
        }}});
        assert_eq!(instance_address(&only_bridges), None);
        assert_eq!(instance_address(&json!({})), None);
    }
}
