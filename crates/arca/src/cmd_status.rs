//! `status [--json] [--config <file>] [--token-file <file>]`: the node's status in words. It asks the running node, with the
//! same call `arca call arca_status` makes, so what it says is what the MCP tool `arca_status` answers. `--json` prints that
//! answer as it came.
//!
//! The words add two things the answer does not carry and the config does: where the MCP door is, and the next step for a
//! node that only looks.

use serde_json::{Value, json};

use crate::cmd_call::{Addressed, read_token, run_call};
use crate::config::NodeConfig;
use crate::{Failure, Flags};

/// The verb whose answer this command prints.
const VERB: &str = "arca_status";

pub fn status(flags: &Flags) -> Result<Option<Value>, Failure> {
    let (config, cfg) = crate::load_chosen_config(flags)?;
    let node = Addressed::new(&config, false);
    let token = read_token(&crate::token_path(flags)?, &node)?;
    let answer =
        run_call(&cfg.listen.unix, None, &token, VERB, json!({}), &node)?.unwrap_or(Value::Null);
    if flags.switch("json") {
        return Ok(Some(answer));
    }
    println!("{}", words(&answer, &cfg));
    Ok(None)
}

/// The status in words: one line per fact, and a last line that says what to do next.
pub fn words(status: &Value, cfg: &NodeConfig) -> String {
    let text = |key: &str| status[key].as_str().unwrap_or("unknown");
    let watch_only = text("mode") == "watch-only";
    let incus = status["incusReachable"].as_bool();
    let mut lines = vec![
        format!("Automagik Arca {}, node {}", text("version"), text("node")),
        format!(
            "Mode: {}",
            if watch_only {
                "watch-only (it looks, and changes nothing)".to_string()
            } else {
                format!("{} (it may make changes)", text("mode"))
            }
        ),
        format!("Up for: {}", uptime(status["uptimeS"].as_u64())),
        format!(
            "Incus: {}",
            match incus {
                Some(true) => "answers",
                Some(false) => "does not answer",
                None => "unknown",
            }
        ),
        format!(
            "Last applied change: {}",
            match status["appliedSeq"].as_u64() {
                Some(seq) => format!("number {seq}"),
                None => "none yet".to_string(),
            }
        ),
        format!(
            "Host metrics: read from the {} host ({})",
            text("metricsSource"),
            text("metricsSourceReason")
        ),
        // The node answered on this socket, so this door is open. The other listeners are only what the config file
        // names: nothing here asked them, and a listener may still be starting, or the file may have changed since.
        format!(
            "MCP door: open, on the unix socket {}",
            cfg.listen.unix.display()
        ),
    ];
    // The loopback door is the one other listener the node speaks for: its answer says whether it holds the address.
    // A port another program took first leaves the node running and this door closed, and whatever answers there is
    // not this node.
    match status.get("loopback").filter(|door| door.is_object()) {
        Some(door) => {
            let addr = door["addr"]
                .as_str()
                .unwrap_or("an address the node did not name");
            lines.push(if door["open"] == json!(true) {
                format!("MCP door for a client on this machine: open, at http://{addr}/mcp")
            } else {
                format!(
                    "MCP door for a client on this machine: CLOSED. The node does not hold {addr} ({}). Send no token there; the node keeps trying the address",
                    door["reason"].as_str().unwrap_or("it does not say why")
                )
            });
        }
        // A node from before the door's report, or one started before the config named the door.
        None => {
            if let Some(addr) = cfg.listen.loopback {
                lines.push(format!(
                    "Config also names: the loopback address {addr} (from the config file, not checked)"
                ));
            }
        }
    }
    if let Some(addr) = cfg.listen.tailnet {
        lines.push(format!(
            "Config also names: the tailnet address {addr} (from the config file, not checked)"
        ));
    }
    if let Some(proxy) = &cfg.listen.proxy {
        lines.push(format!(
            "Config also names: {}, for the reverse proxy at {} (from the config file, not checked)",
            proxy.addr, proxy.peer
        ));
    }
    lines.push(format!("Next: {}", next_step(watch_only, incus)));
    lines.join("\n")
}

/// The one thing to do next: the fault first, then what a node that only looks is missing.
fn next_step(watch_only: bool, incus: Option<bool>) -> &'static str {
    if incus == Some(false) {
        "Incus does not answer on incus.socket of the config. Check that Incus runs and that this user may use its socket."
    } else if watch_only {
        "this node only looks. To let it make changes, the config must name kernel.projects (and kernel.serviceCidr for services); then restart the node."
    } else {
        "nothing is needed. Ask your assistant, or read the node with: arca call watch_report"
    }
}

/// A count of seconds as the two largest units that are not zero.
fn uptime(seconds: Option<u64>) -> String {
    let Some(seconds) = seconds else {
        return "unknown".to_string();
    };
    let units = [(86_400, "d"), (3_600, "h"), (60, "min"), (1, "s")];
    let parts: Vec<String> = units
        .iter()
        .scan(seconds, |left, (size, name)| {
            let count = *left / size;
            *left %= size;
            Some((count, name))
        })
        .filter(|(count, _)| *count > 0)
        .take(2)
        .map(|(count, name)| format!("{count} {name}"))
        .collect();
    if parts.is_empty() {
        "0 s".to_string()
    } else {
        parts.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(extra_listen: Value) -> NodeConfig {
        let mut listen = json!({ "unix": "/run/user/1000/arca.sock" });
        if let (Some(listen), Some(extra)) = (listen.as_object_mut(), extra_listen.as_object()) {
            listen.extend(extra.clone());
        }
        serde_json::from_value(json!({
            "stateDir": "/nonexistent/state",
            "incus": { "socket": "/nonexistent/incus.sock" },
            "listen": listen,
            "tokens": { "hashDir": "/nonexistent/tokens" },
            "kernel": { "droiddProjects": ["droid"] }
        }))
        .expect("the fixture config parses")
    }

    fn answer(mode: &str, incus: bool) -> Value {
        json!({
            "version": "0.10.0",
            "node": "node-1",
            "mode": mode,
            "uptimeS": 11_580,
            "incusReachable": incus,
            "appliedSeq": null,
            "metricsSource": "local",
            "metricsSourceReason": "Incus runs on this host",
        })
    }

    #[test]
    fn a_watch_only_node_reads_in_words_and_says_how_to_let_it_change_things() {
        let text = words(&answer("watch-only", true), &config(json!({})));
        assert_eq!(
            text,
            "Automagik Arca 0.10.0, node node-1\n\
             Mode: watch-only (it looks, and changes nothing)\n\
             Up for: 3 h 13 min\n\
             Incus: answers\n\
             Last applied change: none yet\n\
             Host metrics: read from the local host (Incus runs on this host)\n\
             MCP door: open, on the unix socket /run/user/1000/arca.sock\n\
             Next: this node only looks. To let it make changes, the config must name kernel.projects (and kernel.serviceCidr for services); then restart the node."
        );
    }

    #[test]
    fn a_node_whose_incus_does_not_answer_says_that_first() {
        let text = words(&answer("kernel", false), &config(json!({})));
        assert!(
            text.contains("Mode: kernel (it may make changes)"),
            "{text}"
        );
        assert!(text.contains("Incus: does not answer"), "{text}");
        assert!(
            text.ends_with("Next: Incus does not answer on incus.socket of the config. Check that Incus runs and that this user may use its socket."),
            "{text}"
        );
    }

    #[test]
    fn the_loopback_door_is_what_the_node_says_of_it_open_or_closed_and_never_a_guess() {
        let cfg = config(json!({ "loopback": "127.0.0.1:7471" }));
        let mut open = answer("watch-only", true);
        open["loopback"] = json!({ "addr": "127.0.0.1:7471", "open": true, "reason": null });
        let text = words(&open, &cfg);
        assert!(
            text.contains(
                "MCP door for a client on this machine: open, at http://127.0.0.1:7471/mcp"
            ),
            "{text}"
        );
        // Another program holds the port: the node runs, the door is closed, and the words say not to send the
        // token to whatever answers there.
        let mut closed = answer("watch-only", true);
        closed["loopback"] = json!({
            "addr": "127.0.0.1:7471", "open": false,
            "reason": "address in use: another program holds this port"
        });
        let text = words(&closed, &cfg);
        assert!(
            text.contains("MCP door for a client on this machine: CLOSED. The node does not hold 127.0.0.1:7471 (address in use: another program holds this port). Send no token there; the node keeps trying the address"),
            "{text}"
        );
        assert!(!text.contains("http://127.0.0.1:7471/mcp"), "{text}");
        // A node that says nothing of its door (an older one): only what the config names, marked as not checked.
        let text = words(&answer("watch-only", true), &cfg);
        assert!(
            text.contains("Config also names: the loopback address 127.0.0.1:7471 (from the config file, not checked)"),
            "{text}"
        );
        // And no line at all for a config with no door.
        let text = words(
            &answer("watch-only", true),
            &config(json!({ "loopback": null })),
        );
        assert!(
            !text.contains("loopback") && !text.contains("client on this machine"),
            "{text}"
        );
    }

    #[test]
    fn every_door_the_config_opens_is_named() {
        let cfg = config(json!({
            "tailnet": "100.64.0.1:8443",
            "proxy": { "addr": "127.0.0.1:8080", "peer": "127.0.0.1" }
        }));
        let text = words(&answer("kernel", true), &cfg);
        assert!(
            text.contains("Config also names: the tailnet address 100.64.0.1:8443 (from the config file, not checked)"),
            "{text}"
        );
        assert!(
            text.contains("Config also names: 127.0.0.1:8080, for the reverse proxy at 127.0.0.1 (from the config file, not checked)"),
            "{text}"
        );
        assert!(text.ends_with("arca call watch_report"), "{text}");
    }

    #[test]
    fn an_uptime_is_its_two_largest_units() {
        assert_eq!(uptime(Some(0)), "0 s");
        assert_eq!(uptime(Some(59)), "59 s");
        assert_eq!(uptime(Some(3_600)), "1 h");
        assert_eq!(uptime(Some(90_061)), "1 d 1 h");
        assert_eq!(uptime(None), "unknown");
    }
}
