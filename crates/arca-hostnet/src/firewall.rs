//! Reading the host: the output of `iptables -S` and of `sysctl -n`, and the commands that read and write them.

use std::collections::{BTreeMap, BTreeSet};

use crate::declared::Policy;
use crate::runner::Cmd;

/// Docker's chain for rules that run before its own.
pub const DOCKER_USER: &str = "DOCKER-USER";
/// The IPv4 forwarding sysctl.
pub const IPV4_FORWARDING: &str = "net.ipv4.ip_forward";
/// The IPv6 forwarding sysctl.
pub const IPV6_FORWARDING: &str = "net.ipv6.conf.all.forwarding";

/// The filter table as `iptables -S` lists it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ruleset {
    /// Each built-in chain's policy.
    pub policies: BTreeMap<String, String>,
    /// Every chain that exists.
    pub chains: BTreeSet<String>,
    /// Each chain's rules, in order, as the words after `-A <chain>`.
    pub rules: BTreeMap<String, Vec<Vec<String>>>,
}

impl Ruleset {
    /// Whether `chain` has a rule of exactly these words.
    pub fn has_rule(&self, chain: &str, words: &[String]) -> bool {
        self.rules
            .get(chain)
            .is_some_and(|rules| rules.iter().any(|rule| rule == words))
    }

    /// The policy of the `FORWARD` chain, when it is one this build knows.
    pub fn forward_policy(&self) -> Option<Policy> {
        self.policies
            .get("FORWARD")
            .and_then(|policy| Policy::parse(policy))
    }

    /// Whether Docker has made its chain: the one place an accept holds.
    pub fn has_docker(&self) -> bool {
        self.chains.contains(DOCKER_USER)
    }

    /// The bridges `DOCKER-USER` accepts traffic from now: the rules of the form `-i <bridge> -j ACCEPT`, which is what a
    /// hand-made fix writes.
    pub fn accepted_bridges(&self) -> Vec<String> {
        let mut bridges: Vec<String> = Vec::new();
        for rule in self.rules.get(DOCKER_USER).into_iter().flatten() {
            if let [flag, bridge, j, accept] = rule.as_slice()
                && flag == "-i"
                && j == "-j"
                && accept == "ACCEPT"
                && !bridges.contains(bridge)
            {
                bridges.push(bridge.clone());
            }
        }
        bridges
    }
}

/// Splits one line of `iptables -S` into its words. A word in double quotes is one word, without the quotes.
fn words(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let mut started = false;
    for c in line.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            c if c.is_whitespace() && !quoted => {
                if started {
                    out.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            c => {
                word.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(word);
    }
    out
}

/// Reads the output of `iptables -S`: `-P` lines, `-N` lines and `-A` lines. Any other line is left out.
pub fn parse_iptables(text: &str) -> Ruleset {
    let mut set = Ruleset::default();
    for line in text.lines() {
        let line = words(line);
        match line.as_slice() {
            [flag, chain, policy] if flag == "-P" => {
                set.policies.insert(chain.clone(), policy.clone());
            }
            [flag, chain] if flag == "-N" => {
                set.chains.insert(chain.clone());
            }
            [flag, chain, rule @ ..] if flag == "-A" && !rule.is_empty() => {
                set.rules
                    .entry(chain.clone())
                    .or_default()
                    .push(rule.to_vec());
            }
            _ => {}
        }
    }
    set
}

/// Reads the output of `sysctl -n`, or of `sysctl` without it (`key = value`): `Some(true)` for 1, `Some(false)` for 0.
pub fn parse_sysctl(text: &str) -> Option<bool> {
    let value = text.trim();
    let value = value
        .rsplit_once('=')
        .map_or(value, |(_, value)| value.trim());
    match value {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    }
}

/// The words of the rule that lets everything coming in from `bridge` through.
pub fn accept_in(bridge: &str) -> Vec<String> {
    ["-i", bridge, "-j", "ACCEPT"].map(String::from).to_vec()
}

/// The words of the rule that lets answers go out to `bridge`.
pub fn accept_out(bridge: &str) -> Vec<String> {
    [
        "-o",
        bridge,
        "-m",
        "conntrack",
        "--ctstate",
        "RELATED,ESTABLISHED",
        "-j",
        "ACCEPT",
    ]
    .map(String::from)
    .to_vec()
}

/// The command that lists the filter table. `-w 5` waits for the table's lock instead of failing.
pub fn list_command() -> Cmd {
    Cmd::new("iptables", &["-w", "5", "-S"])
}

/// The command that reads one sysctl.
pub fn sysctl_read(key: &str) -> Cmd {
    Cmd::new("sysctl", &["-n", key])
}

/// The command that sets one sysctl.
pub fn sysctl_write(key: &str, on: bool) -> Cmd {
    Cmd::new("sysctl", &["-w", &format!("{key}={}", u8::from(on))])
}

/// The command that puts a rule first in `DOCKER-USER`, before Docker's own `RETURN`.
pub fn insert_rule(words: &[String]) -> Cmd {
    let mut args: Vec<&str> = vec!["-w", "5", "-I", DOCKER_USER];
    args.extend(words.iter().map(String::as_str));
    Cmd::new("iptables", &args)
}

/// The command that sets the policy of the `FORWARD` chain.
pub fn set_forward_policy(policy: Policy) -> Cmd {
    Cmd::new("iptables", &["-w", "5", "-P", "FORWARD", policy.as_str()])
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOCKER_HOST: &str = "-P INPUT ACCEPT
-P FORWARD DROP
-P OUTPUT ACCEPT
-N DOCKER
-N DOCKER-USER
-A INPUT -m comment --comment \"a b\" -j ACCEPT
-A FORWARD -j DOCKER-USER
-A DOCKER-USER -i incusbr0 -j ACCEPT
-A DOCKER-USER -o incusbr0 -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT
-A DOCKER-USER -i br-two -j ACCEPT
-A DOCKER-USER -j RETURN
";

    #[test]
    fn iptables_s_reads_into_policies_chains_and_rules() {
        let set = parse_iptables(DOCKER_HOST);
        assert_eq!(set.forward_policy(), Some(Policy::Drop));
        assert_eq!(
            set.policies.get("INPUT").map(String::as_str),
            Some("ACCEPT")
        );
        assert!(set.has_docker());
        assert!(set.chains.contains("DOCKER"));
        assert_eq!(set.rules["DOCKER-USER"].len(), 4);
        assert!(set.has_rule(DOCKER_USER, &accept_in("incusbr0")));
        assert!(set.has_rule(DOCKER_USER, &accept_out("incusbr0")));
        assert!(!set.has_rule(DOCKER_USER, &accept_out("br-two")));
        // A quoted word is one word.
        assert_eq!(
            set.rules["INPUT"][0],
            ["-m", "comment", "--comment", "a b", "-j", "ACCEPT"]
        );
        assert_eq!(set.accepted_bridges(), ["incusbr0", "br-two"]);
    }

    #[test]
    fn a_host_without_docker_has_no_docker_chain_and_junk_lines_are_left_out() {
        let set = parse_iptables("-P FORWARD ACCEPT\n*filter\nCOMMIT\n# a comment\n\n-A\n-P\n");
        assert!(!set.has_docker());
        assert_eq!(set.forward_policy(), Some(Policy::Accept));
        assert!(set.rules.is_empty());
        assert!(parse_iptables("").policies.is_empty());
    }

    #[test]
    fn a_sysctl_read_is_on_off_or_unknown() {
        assert_eq!(parse_sysctl("1\n"), Some(true));
        assert_eq!(parse_sysctl("0\n"), Some(false));
        assert_eq!(parse_sysctl("net.ipv4.ip_forward = 1\n"), Some(true));
        assert_eq!(parse_sysctl("net.ipv4.ip_forward = 0"), Some(false));
        assert_eq!(parse_sysctl("2\n"), None);
        assert_eq!(parse_sysctl(""), None);
        assert_eq!(parse_sysctl("sysctl: cannot stat\n"), None);
    }

    #[test]
    fn the_commands_are_the_ones_a_hand_fix_runs() {
        assert_eq!(list_command().line(), "iptables -w 5 -S");
        assert_eq!(
            sysctl_read(IPV4_FORWARDING).line(),
            "sysctl -n net.ipv4.ip_forward"
        );
        assert_eq!(
            sysctl_write(IPV6_FORWARDING, true).line(),
            "sysctl -w net.ipv6.conf.all.forwarding=1"
        );
        assert_eq!(
            sysctl_write(IPV4_FORWARDING, false).line(),
            "sysctl -w net.ipv4.ip_forward=0"
        );
        assert_eq!(
            insert_rule(&accept_in("incusbr0")).line(),
            "iptables -w 5 -I DOCKER-USER -i incusbr0 -j ACCEPT"
        );
        assert_eq!(
            insert_rule(&accept_out("incusbr0")).line(),
            "iptables -w 5 -I DOCKER-USER -o incusbr0 -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT"
        );
        assert_eq!(
            set_forward_policy(Policy::Drop).line(),
            "iptables -w 5 -P FORWARD DROP"
        );
    }
}
