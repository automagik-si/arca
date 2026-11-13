//! A fake host for tests: it answers the commands the root part runs from state it keeps, and records every command. No
//! test needs root, `iptables` or `sysctl`. The `arca` crate's tests use it too.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::firewall::{self, DOCKER_USER, IPV4_FORWARDING, IPV6_FORWARDING};
use crate::runner::{Cmd, Output, Runner};

struct State {
    sysctls: BTreeMap<String, String>,
    policy: String,
    docker: bool,
    docker_user: Vec<Vec<String>>,
    iptables_present: bool,
    iptables_exit: i32,
    failing: Vec<String>,
    log: Vec<String>,
}

/// A host that holds a forwarding switch for each family, a `FORWARD` policy and, when Docker is on it, a `DOCKER-USER`
/// chain.
pub struct FakeHost {
    state: Mutex<State>,
}

impl Default for FakeHost {
    fn default() -> Self {
        Self::new()
    }
}

fn words(text: &str) -> Vec<String> {
    text.split(' ').map(String::from).collect()
}

impl FakeHost {
    /// A host with forwarding off, policy `ACCEPT` and no Docker.
    pub fn new() -> FakeHost {
        FakeHost {
            state: Mutex::new(State {
                sysctls: BTreeMap::from([
                    (IPV4_FORWARDING.to_string(), "0".to_string()),
                    (IPV6_FORWARDING.to_string(), "0".to_string()),
                ]),
                policy: "ACCEPT".to_string(),
                docker: false,
                docker_user: Vec::new(),
                iptables_present: true,
                iptables_exit: 0,
                failing: Vec::new(),
                log: Vec::new(),
            }),
        }
    }

    fn with(self, change: impl FnOnce(&mut State)) -> FakeHost {
        change(&mut self.state.lock().expect("state"));
        self
    }

    /// Forwarding on or off for both families.
    pub fn forwarding(self, v4: bool, v6: bool) -> FakeHost {
        self.with(|state| {
            state
                .sysctls
                .insert(IPV4_FORWARDING.to_string(), u8::from(v4).to_string());
            state
                .sysctls
                .insert(IPV6_FORWARDING.to_string(), u8::from(v6).to_string());
        })
    }

    /// Docker is running: its chain exists with its own last rule, and it has set the `FORWARD` policy to `DROP`.
    pub fn docker(self) -> FakeHost {
        self.with(|state| {
            state.docker = true;
            state.policy = "DROP".to_string();
            state.docker_user = vec![words("-j RETURN")];
        })
    }

    /// The `FORWARD` policy.
    pub fn policy(self, policy: &str) -> FakeHost {
        self.with(|state| state.policy = policy.to_string())
    }

    /// The accepts a hand-made fix puts in `DOCKER-USER` for `bridge`.
    pub fn hand_fix(self, bridge: &str) -> FakeHost {
        self.with(|state| {
            state.docker_user.insert(0, firewall::accept_out(bridge));
            state.docker_user.insert(0, firewall::accept_in(bridge));
        })
    }

    /// No `iptables` program on the host.
    pub fn without_iptables(self) -> FakeHost {
        self.with(|state| state.iptables_present = false)
    }

    /// `iptables -S` exits with this code.
    pub fn iptables_exits(self, code: i32) -> FakeHost {
        self.with(|state| state.iptables_exit = code)
    }

    /// Every command whose line contains `part` fails.
    pub fn failing(self, part: &str) -> FakeHost {
        self.with(|state| state.failing.push(part.to_string()))
    }

    /// Every command run, in order.
    pub fn log(&self) -> Vec<String> {
        self.state.lock().expect("state").log.clone()
    }

    /// The commands that changed something: every one that is not a read.
    pub fn changes(&self) -> Vec<String> {
        self.log()
            .into_iter()
            .filter(|line| !line.starts_with("sysctl -n ") && line != "iptables -w 5 -S")
            .collect()
    }

    /// The rules of `DOCKER-USER`, each as one line.
    pub fn docker_user(&self) -> Vec<String> {
        let state = self.state.lock().expect("state");
        state
            .docker_user
            .iter()
            .map(|rule| rule.join(" "))
            .collect()
    }

    /// The `FORWARD` policy.
    pub fn forward_policy(&self) -> String {
        self.state.lock().expect("state").policy.clone()
    }

    /// One sysctl's value.
    pub fn sysctl(&self, key: &str) -> Option<String> {
        self.state.lock().expect("state").sysctls.get(key).cloned()
    }
}

fn ok(stdout: &str) -> Result<Output, String> {
    Ok(Output {
        code: 0,
        stdout: stdout.to_string(),
        stderr: String::new(),
    })
}

fn failed(stderr: &str) -> Result<Output, String> {
    Ok(Output {
        code: 1,
        stdout: String::new(),
        stderr: stderr.to_string(),
    })
}

impl Runner for FakeHost {
    fn run(&self, cmd: &Cmd) -> Result<Output, String> {
        let mut state = self.state.lock().expect("state");
        let line = cmd.line();
        state.log.push(line.clone());
        if state
            .failing
            .iter()
            .any(|part| line.contains(part.as_str()))
        {
            return failed("the fake host refuses this command");
        }
        let args: Vec<&str> = cmd.args.iter().map(String::as_str).collect();
        match (cmd.program.as_str(), args.as_slice()) {
            ("sysctl", ["-n", key]) => match state.sysctls.get(*key) {
                Some(value) => ok(&format!("{value}\n")),
                None => failed("cannot stat"),
            },
            ("sysctl", ["-w", pair]) => {
                let Some((key, value)) = pair.split_once('=') else {
                    return failed("bad pair");
                };
                state.sysctls.insert(key.to_string(), value.to_string());
                ok(&format!("{pair}\n"))
            }
            ("iptables", _) if !state.iptables_present => {
                Err("iptables could not start".to_string())
            }
            ("iptables", ["-w", "5", "-S"]) => {
                if state.iptables_exit != 0 {
                    return Ok(Output {
                        code: state.iptables_exit,
                        stdout: String::new(),
                        stderr: "Permission denied".to_string(),
                    });
                }
                let mut text = format!(
                    "-P INPUT ACCEPT\n-P FORWARD {}\n-P OUTPUT ACCEPT\n",
                    state.policy
                );
                if state.docker {
                    text.push_str("-N DOCKER\n-N DOCKER-USER\n-A FORWARD -j DOCKER-USER\n");
                    for rule in &state.docker_user {
                        text.push_str(&format!("-A {DOCKER_USER} {}\n", rule.join(" ")));
                    }
                }
                ok(&text)
            }
            ("iptables", ["-w", "5", "-I", chain, rule @ ..]) if *chain == DOCKER_USER => {
                if !state.docker {
                    return failed("No chain/target/match by that name");
                }
                state
                    .docker_user
                    .insert(0, rule.iter().map(|word| (*word).to_string()).collect());
                ok("")
            }
            ("iptables", ["-w", "5", "-P", "FORWARD", policy]) => {
                state.policy = (*policy).to_string();
                ok("")
            }
            ("systemctl", _) => ok(""),
            _ => Err(format!("the fake host does not know: {line}")),
        }
    }
}
