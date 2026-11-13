//! Reading the host, finding what is not as declared, and planning the commands that put it back.

use crate::declared::{Declared, Policy};
use crate::firewall::{self, Ruleset};
use crate::runner::{Cmd, Runner};
use crate::status::DriftItem;

/// What the host's firewall tool answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Firewall {
    /// There is no `iptables` on the host, so there is no Docker firewall to tend.
    Absent,
    /// `iptables -S` failed, with why.
    Unreadable(String),
    Rules(Ruleset),
}

/// What the host is now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed {
    /// IPv4 forwarding, `None` when it could not be read.
    pub ipv4: Option<bool>,
    /// IPv6 forwarding, `None` when it could not be read (a host without IPv6 has no such switch).
    pub ipv6: Option<bool>,
    pub firewall: Firewall,
}

/// Reads the host: both forwarding switches and the filter table.
pub fn observe(runner: &dyn Runner) -> Observed {
    let read = |key: &str| {
        runner
            .run(&firewall::sysctl_read(key))
            .ok()
            .filter(|output| output.code == 0)
            .and_then(|output| firewall::parse_sysctl(&output.stdout))
    };
    let firewall = match runner.run(&firewall::list_command()) {
        Err(_) => Firewall::Absent,
        Ok(output) if output.code == 0 => Firewall::Rules(firewall::parse_iptables(&output.stdout)),
        Ok(output) => Firewall::Unreadable(format!(
            "iptables -S exited {}: {}",
            output.code,
            output.stderr.trim().chars().take(200).collect::<String>()
        )),
    };
    Observed {
        ipv4: read(firewall::IPV4_FORWARDING),
        ipv6: read(firewall::IPV6_FORWARDING),
        firewall,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Family {
    V4,
    V6,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Direction {
    In,
    Out,
}

/// One way the host is not as declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Drift {
    /// Forwarding of a family is not what is declared.
    Forwarding { family: Family, declared: bool },
    /// The rule that lets a bridge's containers reach out is missing from `DOCKER-USER`.
    RuleMissing {
        bridge: String,
        direction: Direction,
    },
    /// The `FORWARD` policy is not the declared one.
    ForwardPolicy {
        declared: Policy,
        actual: Option<Policy>,
    },
    /// The firewall could not be read.
    FirewallUnreadable(String),
}

impl Drift {
    /// The short name an alert carries.
    pub fn item(&self) -> String {
        match self {
            Self::Forwarding {
                family: Family::V4, ..
            } => "ipv4-forwarding".to_string(),
            Self::Forwarding {
                family: Family::V6, ..
            } => "ipv6-forwarding".to_string(),
            Self::RuleMissing {
                bridge,
                direction: Direction::In,
            } => format!("rule-in:{bridge}"),
            Self::RuleMissing {
                bridge,
                direction: Direction::Out,
            } => format!("rule-out:{bridge}"),
            Self::ForwardPolicy { .. } => "forward-policy".to_string(),
            Self::FirewallUnreadable(_) => "firewall-unreadable".to_string(),
        }
    }

    /// The drift in words.
    pub fn detail(&self) -> String {
        let state = |on: bool| if on { "on" } else { "off" };
        match self {
            Self::Forwarding { family, declared } => {
                let name = match family {
                    Family::V4 => "IPv4",
                    Family::V6 => "IPv6",
                };
                format!(
                    "{name} forwarding is {}, and it is declared {}",
                    state(!declared),
                    state(*declared)
                )
            }
            Self::RuleMissing {
                bridge,
                direction: Direction::In,
            } => {
                format!("DOCKER-USER does not accept what comes in from the bridge {bridge}")
            }
            Self::RuleMissing {
                bridge,
                direction: Direction::Out,
            } => {
                format!("DOCKER-USER does not accept the answers going out to the bridge {bridge}")
            }
            Self::ForwardPolicy { declared, actual } => format!(
                "the FORWARD policy is {}, and it is declared {}",
                actual.map_or("unknown", Policy::as_str),
                declared.as_str()
            ),
            Self::FirewallUnreadable(why) => format!("the firewall could not be read: {why}"),
        }
    }

    pub fn as_item(&self) -> DriftItem {
        DriftItem {
            item: self.item(),
            detail: self.detail(),
        }
    }
}

/// Everything that is not as declared, in the order it is put back: forwarding, then the rules, then the policy. Rules and
/// the policy are judged only where Docker has made its chain, since without Docker nothing drops what the bridges send.
pub fn drift(declared: &Declared, observed: &Observed) -> Vec<Drift> {
    let mut found = Vec::new();
    for (family, seen) in [(Family::V4, observed.ipv4), (Family::V6, observed.ipv6)] {
        if seen.is_some_and(|on| on != declared.forwarding) {
            found.push(Drift::Forwarding {
                family,
                declared: declared.forwarding,
            });
        }
    }
    match &observed.firewall {
        Firewall::Absent => {}
        Firewall::Unreadable(why) => found.push(Drift::FirewallUnreadable(why.clone())),
        Firewall::Rules(rules) if rules.has_docker() => {
            if declared.forwarding {
                for bridge in &declared.bridges {
                    for (direction, words) in [
                        (Direction::In, firewall::accept_in(bridge)),
                        (Direction::Out, firewall::accept_out(bridge)),
                    ] {
                        if !rules.has_rule(firewall::DOCKER_USER, &words) {
                            found.push(Drift::RuleMissing {
                                bridge: bridge.clone(),
                                direction,
                            });
                        }
                    }
                }
            }
            if let Some(policy) = declared.forward_policy
                && rules.forward_policy() != Some(policy)
            {
                found.push(Drift::ForwardPolicy {
                    declared: policy,
                    actual: rules.forward_policy(),
                });
            }
        }
        Firewall::Rules(_) => {}
    }
    found
}

/// The commands that put back what `drifts` names. The rules come before the policy, so a policy of `DROP` is never set
/// before the accepts that let the bridges through.
pub fn plan(declared: &Declared, drifts: &[Drift]) -> Vec<Cmd> {
    let _ = declared;
    let mut commands = Vec::new();
    for drift in drifts {
        if let Drift::Forwarding { family, declared } = drift {
            let key = match family {
                Family::V4 => firewall::IPV4_FORWARDING,
                Family::V6 => firewall::IPV6_FORWARDING,
            };
            commands.push(firewall::sysctl_write(key, *declared));
        }
    }
    for drift in drifts {
        if let Drift::RuleMissing { bridge, direction } = drift {
            let words = match direction {
                Direction::In => firewall::accept_in(bridge),
                Direction::Out => firewall::accept_out(bridge),
            };
            commands.push(firewall::insert_rule(&words));
        }
    }
    for drift in drifts {
        if let Drift::ForwardPolicy { declared, .. } = drift {
            commands.push(firewall::set_forward_policy(*declared));
        }
    }
    commands
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeHost;

    fn declared() -> Declared {
        Declared {
            forwarding: true,
            bridges: vec!["incusbr0".to_string()],
            forward_policy: Some(Policy::Drop),
        }
    }

    fn items(host: &FakeHost, declared: &Declared) -> Vec<String> {
        drift(declared, &observe(host))
            .iter()
            .map(Drift::item)
            .collect()
    }

    fn synced() -> FakeHost {
        FakeHost::new()
            .forwarding(true, true)
            .docker()
            .hand_fix("incusbr0")
    }

    #[test]
    fn observing_reads_both_switches_and_the_filter_table() {
        let seen = observe(&synced());
        assert_eq!((seen.ipv4, seen.ipv6), (Some(true), Some(true)));
        let Firewall::Rules(rules) = seen.firewall else {
            panic!("the rules are read");
        };
        assert!(rules.has_docker());
        assert_eq!(rules.forward_policy(), Some(Policy::Drop));
        // No iptables, a failing iptables and a missing switch are each their own reading.
        assert_eq!(
            observe(&FakeHost::new().without_iptables()).firewall,
            Firewall::Absent
        );
        assert!(matches!(
            observe(&FakeHost::new().iptables_exits(4)).firewall,
            Firewall::Unreadable(_)
        ));
        let host = FakeHost::new().failing("sysctl -n net.ipv6");
        assert_eq!(observe(&host).ipv6, None);
    }

    #[test]
    fn a_host_as_declared_has_no_drift_and_nothing_to_do() {
        assert!(items(&synced(), &declared()).is_empty());
        assert!(plan(&declared(), &[]).is_empty());
    }

    #[test]
    fn each_kind_of_drift_is_found_and_named() {
        let want = declared();
        assert_eq!(
            items(
                &FakeHost::new()
                    .forwarding(false, true)
                    .docker()
                    .hand_fix("incusbr0"),
                &want
            ),
            ["ipv4-forwarding"]
        );
        assert_eq!(
            items(
                &FakeHost::new()
                    .forwarding(true, false)
                    .docker()
                    .hand_fix("incusbr0"),
                &want
            ),
            ["ipv6-forwarding"]
        );
        assert_eq!(
            items(
                &FakeHost::new()
                    .forwarding(true, true)
                    .docker()
                    .hand_fix("incusbr0")
                    .policy("ACCEPT"),
                &want
            ),
            ["forward-policy"]
        );
        // Docker is up and none of the accepts is there.
        assert_eq!(
            items(&FakeHost::new().forwarding(true, true).docker(), &want),
            ["rule-in:incusbr0", "rule-out:incusbr0"]
        );
        // Only the answers' accept is gone.
        let host = FakeHost::new()
            .forwarding(true, true)
            .docker()
            .hand_fix("incusbr0");
        let mut one_bridge = want.clone();
        one_bridge.bridges.push("br-two".to_string());
        assert_eq!(
            items(&host, &one_bridge),
            ["rule-in:br-two", "rule-out:br-two"]
        );
        assert_eq!(
            items(
                &FakeHost::new().forwarding(true, true).iptables_exits(4),
                &want
            ),
            ["firewall-unreadable"]
        );
    }

    #[test]
    fn the_details_say_what_is_wrong_in_words() {
        let drifts = drift(
            &declared(),
            &observe(&FakeHost::new().docker().policy("ACCEPT")),
        );
        let details: Vec<String> = drifts.iter().map(Drift::detail).collect();
        assert_eq!(details[0], "IPv4 forwarding is off, and it is declared on");
        assert_eq!(details[1], "IPv6 forwarding is off, and it is declared on");
        assert_eq!(
            details[2],
            "DOCKER-USER does not accept what comes in from the bridge incusbr0"
        );
        assert_eq!(
            details[3],
            "DOCKER-USER does not accept the answers going out to the bridge incusbr0"
        );
        assert_eq!(
            details[4],
            "the FORWARD policy is ACCEPT, and it is declared DROP"
        );
    }

    #[test]
    fn without_docker_the_rules_and_the_policy_are_not_judged() {
        // A host with no Docker chain: nothing drops what the bridges send, so no accept is missing.
        assert!(items(&FakeHost::new().forwarding(true, true), &declared()).is_empty());
        assert!(
            items(
                &FakeHost::new().forwarding(true, true).without_iptables(),
                &declared()
            )
            .is_empty()
        );
    }

    #[test]
    fn forwarding_declared_off_is_held_and_asks_for_no_rule() {
        let off = Declared {
            forwarding: false,
            bridges: vec!["incusbr0".to_string()],
            forward_policy: None,
        };
        assert!(items(&FakeHost::new().docker(), &off).is_empty());
        let host = FakeHost::new().forwarding(true, false).docker();
        assert_eq!(items(&host, &off), ["ipv4-forwarding"]);
        let commands = plan(&off, &drift(&off, &observe(&host)));
        assert_eq!(
            commands.iter().map(Cmd::line).collect::<Vec<_>>(),
            ["sysctl -w net.ipv4.ip_forward=0"]
        );
    }

    #[test]
    fn the_policy_is_not_watched_when_the_file_does_not_declare_one() {
        let mut open = declared();
        open.forward_policy = None;
        assert!(
            items(
                &FakeHost::new()
                    .forwarding(true, true)
                    .docker()
                    .hand_fix("incusbr0")
                    .policy("ACCEPT"),
                &open
            )
            .is_empty()
        );
    }

    #[test]
    fn the_plan_sets_forwarding_then_the_rules_then_the_policy() {
        let host = FakeHost::new().docker().policy("ACCEPT");
        let want = declared();
        let lines: Vec<String> = plan(&want, &drift(&want, &observe(&host)))
            .iter()
            .map(Cmd::line)
            .collect();
        assert_eq!(
            lines,
            [
                "sysctl -w net.ipv4.ip_forward=1",
                "sysctl -w net.ipv6.conf.all.forwarding=1",
                "iptables -w 5 -I DOCKER-USER -i incusbr0 -j ACCEPT",
                "iptables -w 5 -I DOCKER-USER -o incusbr0 -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT",
                "iptables -w 5 -P FORWARD DROP",
            ]
        );
        // A firewall that could not be read is never written to.
        let unreadable = [Drift::FirewallUnreadable("x".to_string())];
        assert!(plan(&want, &unreadable).is_empty());
    }
}
