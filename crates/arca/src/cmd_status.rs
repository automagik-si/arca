//! `status [--json] [--config <file>] [--token-file <file>]`: the node's status in words. It asks the running node, with the
//! same call `arca call arca_status` makes, so what it says is what the MCP tool `arca_status` answers. `--json` prints that
//! answer as it came.
//!
//! The words add two things the answer does not carry and the config does: where the MCP door is, and the next step for a
//! node that only looks.

use serde_json::{Value, json};

use crate::cmd_call::{Addressed, read_token, run_call};
use crate::cmd_init::DirectTokens;
use std::path::Path;

use crate::config::NodeConfig;
use crate::{Failure, Flags};

/// The verb whose answer this command prints.
const VERB: &str = "arca_status";

pub fn status(flags: &Flags) -> Result<Option<Value>, Failure> {
    let (config, cfg) = crate::load_chosen_config(flags)?;
    if flags.switch("security") {
        if flags.switch("json") {
            return Err(Failure::Usage(
                "--security prints words, and --json prints the node's answer: pass one of them"
                    .to_string(),
            ));
        }
        // What to know about this node is in its config and its token directory: the node is not asked, and need
        // not be running.
        println!(
            "{}",
            security(&cfg, &config.path, &crate::cmd_init::direct_tokens(&cfg))
        );
        return Ok(None);
    }
    let node = Addressed::new(&config, false);
    let token = read_token(&crate::token_path(flags)?, &node)?;
    let answer =
        run_call(&cfg.listen.unix, None, &token, VERB, json!({}), &node)?.unwrap_or(Value::Null);
    if flags.switch("json") {
        return Ok(Some(answer));
    }
    // Which tokens change things without the page is in the node's token directory, read as `serve` reads it.
    let tokens = crate::cmd_init::direct_tokens(&cfg);
    println!(
        "{}",
        words(&answer, &cfg, &config.path, &tokens, has_screen())
    );
    Ok(None)
}

/// `status --security`: what a person should know before sharing this machine, or a token of this node, with anyone
/// else. The installer's report shows one line to connect an assistant and names this command for the rest. Every line
/// is about a door the config opens; a config that opens none says so.
///
/// It is also where the rest of `arca init`'s words live. Init's report is a handful of lines and one pointer here:
/// what the node may touch, which tokens do not wait for the page and what they may do, where the templates and the
/// config as it was are kept, the way to the page from another computer, and what a node set back to watch-only
/// keeps.
pub(crate) fn security(cfg: &NodeConfig, config: &Path, tokens: &DirectTokens) -> String {
    let mut lines = vec![format!(
        "What to know about the doors of the node that {} describes.",
        config.display()
    )];
    lines.push(String::new());
    lines.push(format!(
        "The unix socket {} answers only the user who runs the node. `arca call` and `arca status` use it, and it has no port for anyone to take.",
        cfg.listen.unix.display()
    ));
    match cfg.listen.loopback {
        Some(door) => {
            lines.push(String::new());
            lines.push(format!(
                "The HTTP door, http://{door}/mcp, answers this machine only, and only with a token."
            ));
            for caution in [
                "A client you connect keeps the token in its own config, in clear text: whoever can read that file can ask this node whatever the token may ask.".to_string(),
                "Running the `claude mcp add` line puts the token in that command's arguments for a moment, where other users of this machine can read it. Pasting the entry into the client's config avoids that.".to_string(),
                "Use the address in the URL, not the name localhost: the name can also mean ::1, where this door does not listen and another program could.".to_string(),
                format!(
                    "On a machine shared with other accounts: while the node is not running, another account can take port {}, and would then receive what a client sends there, the token included. `arca status` says whether the node holds the door.",
                    door.port()
                ),
                "To have no HTTP door at all: sh install.sh --no-door, or \"loopback\": null in \"listen\" of the config.".to_string(),
            ] {
                lines.push(format!("  - {caution}"));
            }
        }
        None => {
            lines.push(String::new());
            lines.push(
                "This node has no HTTP door on this machine: its config names no listen.loopback."
                    .to_string(),
            );
        }
    }
    if !cfg.kernel.projects.is_empty() {
        lines.push(String::new());
        lines.push(format!(
            "This node may change things. It may touch the Incus project {} and the bridge {}{}.",
            arca_netproject::PROJECT,
            arca_netproject::BRIDGE,
            cfg.kernel
                .service_cidr
                .as_deref()
                .map(|cidr| format!(" on {cidr}"))
                .unwrap_or_default()
        ));
        for more in [
            "And, each only from a card you approve: snapshots (the pools they are kept in and the project arcak-snapshots) and the GPU devices of its own instances. Nothing else.".to_string(),
            "Arca never writes in \"default\"; that entry in kernel.droiddProjects only names it as not Arca's.".to_string(),
            format!(
                "The service templates it creates from are in {}. A database comes from the postgres-autopg one.",
                cfg.state_dir.join("templates").display()
            ),
            "What an assistant proposes runs only after you approve it with your passkey on the approval page.".to_string(),
        ] {
            lines.push(format!("  - {more}"));
        }
        // Who does not wait for the page, by name, and what such a token may do: init says it before it asks, and
        // this is where it stays readable afterwards.
        lines.push(String::new());
        for line in tokens.words(config, cfg.kernel.tenants.is_some()) {
            lines.push(line.replace(" may then change ", " may change "));
        }
    } else if cfg.kernel.service_cidr.is_some() {
        // A node that was set back to watch-only: what `arca init --watch-only` no longer prints.
        lines.push(String::new());
        lines.push(format!(
            "This node only looks: it starts, stops, creates and removes nothing, and a proposal that waits cannot run. If it made changes before, nothing was removed: the Incus project {}, its bridge, the services in it, your passkeys and every other key of the config are as they were.",
            arca_netproject::PROJECT
        ));
        lines.push(
            "  - The reading tools still show what is there (service_list, service_status, service_logs and the others), and every answer says that the node only looks. Every tool that would change something is refused with that same sentence.".to_string(),
        );
    }
    let kept = {
        let mut name = config.as_os_str().to_owned();
        name.push(".before-init");
        std::path::PathBuf::from(name)
    };
    if kept.is_file() {
        lines.push(String::new());
        lines.push(format!(
            "arca init kept the config as it was before its last change at {}.",
            kept.display()
        ));
    }
    if let Some(origin) = &cfg.approval_origin {
        lines.push(String::new());
        if let Some(port) = arca_approval::localhost_origin_port(origin) {
            lines.push(format!(
                "The approval page, {origin}/approve, is the page of this machine alone."
            ));
            for caution in [
                "It is for a machine where every account, and every agent that shares its network, is trusted: a program that holds the page's port while the node does not is the page, and what you type or sign there is that program's.",
                "Before you type a setup code or approve for the first time, check that `arca status` says the page is open.",
                "Other accounts of this machine can read the waiting cards and decline them. They cannot approve.",
                "A phone cannot open this page. Approving from a phone needs the node reachable under a name, on a tailnet or a domain; the guide's section \"The approval page\" says how.",
            ] {
                lines.push(format!("  - {caution}"));
            }
            lines.push(format!(
                "  - To open it from your own computer: ssh -o ExitOnForwardFailure=yes -L localhost:{port}:127.0.0.1:{port} <this machine>, then open {origin}/approve in its browser. If ssh warns that it could not bind the port, close it: another port will not work, since the page's address names this one."
            ));
        } else {
            lines.push(format!(
                "The approval page is {origin}/approve. Who can reach it is decided by what serves that name in front of the node."
            ));
        }
    }
    if cfg.listen.tailnet.is_some() || cfg.listen.proxy.is_some() {
        lines.push(String::new());
        lines.push(
            "This config also opens a tailnet or proxy address. Every call there needs a token too; the guide's sections \"Tailnet listener\" and \"Proxy listener\" say who can reach each.".to_string(),
        );
    }
    lines.push(String::new());
    lines.push(
        "The operating guide says each of these in full: \"Loopback listener\" and \"The page on this machine alone\" in docs/OPERATING.md."
            .to_string(),
    );
    lines.join("\n")
}

/// The status in words: one line per fact, and a last line that says what to do next.
pub(crate) fn words(
    status: &Value,
    cfg: &NodeConfig,
    config: &Path,
    tokens: &DirectTokens,
    screen: bool,
) -> String {
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
    // Approvals, from the node's own answer: whether it may change things, the page that approves, whether that page
    // is open, and whether a passkey is trusted yet.
    // A node that may change things: what waits for the passkey and what does not. "Each one waits" would be false
    // where a doctor or operator token exists, so the line is the token directory's, not a fixed sentence.
    lines.push(if watch_only {
        "Changes: off. This node only looks".to_string()
    } else {
        tokens.status_line(config)
    });
    let page = status.get("approvalPage").filter(|page| page.is_object());
    match page {
        Some(page) => {
            let url = page["url"].as_str().unwrap_or("an address the node did not name");
            lines.push(if page["open"] == json!(true) {
                format!("Approval page: {url} (open)")
            } else {
                format!(
                    "Approval page: {url} is CLOSED. {}",
                    page["reason"].as_str().unwrap_or("The node does not say why")
                )
            });
            lines.push(match page["passkeys"].as_u64() {
                Some(0) => "Passkeys: none trusted yet, so nothing can be approved".to_string(),
                Some(count) => format!("Passkeys: {count} trusted"),
                None => "Passkeys: the node is busy with a change and did not say".to_string(),
            });
        }
        // The node says nothing of a page: it serves none, or it is from before this report.
        None => lines.push(match &cfg.approval_origin {
            Some(origin) => format!(
                "Approval page: the config names {origin}/approve, and the node did not say whether it serves it (it is from before this report, or it started before the config named the page)"
            ),
            None => "Approval page: none".to_string(),
        }),
    }
    lines.push(format!(
        "Next: {}",
        next_step(watch_only, incus, page, config, screen)
    ));
    // One pointer to everything that is not the next step: who can reach what, what Arca may touch, the way to the
    // page from another computer, and what a phone needs.
    lines.push(format!(
        "More: arca status --security --config {}",
        config.display()
    ));
    lines.join("\n")
}

/// The one thing to do next: the fault first, then what stands between the owner and the first approved change.
fn next_step(
    watch_only: bool,
    incus: Option<bool>,
    page: Option<&Value>,
    config: &Path,
    screen: bool,
) -> String {
    let config = config.display();
    if incus == Some(false) {
        return "Incus does not answer on incus.socket of the config. Check that Incus runs and that this user may use its socket.".to_string();
    }
    if watch_only {
        return format!(
            "this node only looks. To let it make changes, with what is proposed waiting for your approval: arca init --enable-changes --config {config}"
        );
    }
    let Some(page) = page else {
        return "changes are on, and this node has no approval page, so nothing can be approved. The guide's section \"The approval page\" says how to give it one.".to_string();
    };
    let url = page["url"].as_str().unwrap_or_default();
    if page["open"] != json!(true) {
        return "the approval page is closed. Type no setup code and approve nothing at that address until `arca status` says it is open.".to_string();
    }
    if page["passkeys"].as_u64() == Some(0) {
        return enrol_step(url, &config.to_string(), screen);
    }
    format!("nothing is needed. Ask your assistant; what it proposes waits for you at {url}")
}

/// The one step that enrols the first passkey, for the person at this terminal. With a screen it is the page and its
/// button. Without one, the code comes from this terminal, and for the page of this machine alone the way to open it
/// from the person's own computer is part of the step, with what it cannot do; with a screen those lines are not
/// the next step, and `arca status --security` has them.
pub(crate) fn enrol_step(url: &str, config: &str, screen: bool) -> String {
    if screen {
        return format!(
            "enrol your passkey: open {url} and press \"Get a setup code\". Until one is trusted, nothing can be approved."
        );
    }
    let mut step = format!(
        "enrol your passkey: this terminal has no screen, so print the setup code with: arca setup-code --config {config}"
    );
    if let Some(port) = url
        .strip_suffix("/approve")
        .and_then(arca_approval::localhost_origin_port)
    {
        step.push_str(&format!(
            "\n      To open the page from your own computer: ssh -o ExitOnForwardFailure=yes -L localhost:{port}:127.0.0.1:{port} <this machine>, then open {url} there.\n      A phone cannot open this page: that needs the node reachable under a name, on a tailnet or a domain."
        ));
    }
    step
}

/// Whether the person at this terminal has a screen here: a graphical session names its display. A terminal reached
/// over ssh, or a machine with no desktop, names none. It decides which next step is shown, nothing else.
pub(crate) fn has_screen() -> bool {
    ["DISPLAY", "WAYLAND_DISPLAY"]
        .iter()
        .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
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

    /// The last line of every status: the one pointer to the rest.
    const MORE: &str = "\nMore: arca status --security --config /home/u/.config/arca/node.json";

    /// A node none of whose tokens does more than look and propose.
    fn quiet() -> DirectTokens {
        DirectTokens::Read(Vec::new())
    }

    /// The config's path, as `status` was given it.
    const CONFIG: &str = "/home/u/.config/arca/node.json";

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
        let text = words(
            &answer("watch-only", true),
            &config(json!({})),
            Path::new(CONFIG),
            &quiet(),
            true,
        );
        assert_eq!(
            text,
            "Automagik Arca 0.10.0, node node-1\n\
             Mode: watch-only (it looks, and changes nothing)\n\
             Up for: 3 h 13 min\n\
             Incus: answers\n\
             Last applied change: none yet\n\
             Host metrics: read from the local host (Incus runs on this host)\n\
             MCP door: open, on the unix socket /run/user/1000/arca.sock\n\
             Changes: off. This node only looks\n\
             Approval page: none\n\
             Next: this node only looks. To let it make changes, with what is proposed waiting for your approval: arca init --enable-changes --config /home/u/.config/arca/node.json\n\
             More: arca status --security --config /home/u/.config/arca/node.json"
        );
    }

    #[test]
    fn a_node_whose_incus_does_not_answer_says_that_first() {
        let text = words(
            &answer("kernel", false),
            &config(json!({})),
            Path::new(CONFIG),
            &quiet(),
            true,
        );
        assert!(
            text.contains("Mode: kernel (it may make changes)"),
            "{text}"
        );
        assert!(text.contains("Incus: does not answer"), "{text}");
        assert!(
            text.ends_with(&format!("{}{MORE}", "Next: Incus does not answer on incus.socket of the config. Check that Incus runs and that this user may use its socket.")),
            "{text}"
        );
    }

    #[test]
    fn the_loopback_door_is_what_the_node_says_of_it_open_or_closed_and_never_a_guess() {
        let cfg = config(json!({ "loopback": "127.0.0.1:7471" }));
        let mut open = answer("watch-only", true);
        open["loopback"] = json!({ "addr": "127.0.0.1:7471", "open": true, "reason": null });
        let text = words(&open, &cfg, Path::new(CONFIG), &quiet(), true);
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
        let text = words(&closed, &cfg, Path::new(CONFIG), &quiet(), true);
        assert!(
            text.contains("MCP door for a client on this machine: CLOSED. The node does not hold 127.0.0.1:7471 (address in use: another program holds this port). Send no token there; the node keeps trying the address"),
            "{text}"
        );
        assert!(!text.contains("http://127.0.0.1:7471/mcp"), "{text}");
        // A node that says nothing of its door (an older one): only what the config names, marked as not checked.
        let text = words(
            &answer("watch-only", true),
            &cfg,
            Path::new(CONFIG),
            &quiet(),
            true,
        );
        assert!(
            text.contains("Config also names: the loopback address 127.0.0.1:7471 (from the config file, not checked)"),
            "{text}"
        );
        // And no line at all for a config with no door.
        let text = words(
            &answer("watch-only", true),
            &config(json!({ "loopback": null })),
            Path::new(CONFIG),
            &quiet(),
            true,
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
        let text = words(
            &answer("kernel", true),
            &cfg,
            Path::new(CONFIG),
            &quiet(),
            true,
        );
        assert!(
            text.contains("Config also names: the tailnet address 100.64.0.1:8443 (from the config file, not checked)"),
            "{text}"
        );
        assert!(
            text.contains("Config also names: 127.0.0.1:8080, for the reverse proxy at 127.0.0.1 (from the config file, not checked)"),
            "{text}"
        );
        // Changes are on and the node names no page: nothing could be approved, and the next step says so.
        assert!(
            text.contains("Changes: on. Each one waits for the owner's approval with a passkey: no token of this node may do more than look and propose\nApproval page: none\n"),
            "{text}"
        );
        assert!(
            text.ends_with(&format!("{}{MORE}", "Next: changes are on, and this node has no approval page, so nothing can be approved. The guide's section \"The approval page\" says how to give it one.")),
            "{text}"
        );
    }

    /// The node's answer with its approval page: the address, whether it is open, and the passkeys it trusts.
    fn with_page(url: &str, open: bool, reason: Option<&str>, passkeys: Option<u64>) -> Value {
        let mut status = answer("kernel", true);
        status["approvalPage"] =
            json!({ "url": url, "open": open, "reason": reason, "passkeys": passkeys });
        status
    }

    #[test]
    fn approvals_are_said_from_the_nodes_own_answer_with_the_next_step_to_the_first_approved_change()
     {
        let cfg = config(json!({ "loopback": "127.0.0.1:7471" }));
        let page = "http://localhost:7471/approve";

        // No passkey yet, at a terminal with a screen: the next step is ONE line, the page and its button. The way
        // from another computer and the sentence about a phone are not the next step here, so they are not printed:
        // the pointer names where they are.
        let text = words(
            &with_page(page, true, None, Some(0)),
            &cfg,
            Path::new(CONFIG),
            &quiet(),
            true,
        );
        assert!(
            text.ends_with(&format!(
                "{}{MORE}",
                "Approval page: http://localhost:7471/approve (open)\n\
                 Passkeys: none trusted yet, so nothing can be approved\n\
                 Next: enrol your passkey: open http://localhost:7471/approve and press \"Get a setup code\". Until one is trusted, nothing can be approved."
            )),
            "{text}"
        );
        assert!(!text.contains("ssh ") && !text.contains("phone"), "{text}");
        assert_eq!(
            text.lines()
                .filter(|line| line.starts_with("Next: "))
                .count(),
            1
        );

        // The same node from a terminal with no screen: the code comes from this terminal, and now the way to open
        // the page from the person's own computer, and what a phone needs, are the next step and are said.
        let text = words(
            &with_page(page, true, None, Some(0)),
            &cfg,
            Path::new(CONFIG),
            &quiet(),
            false,
        );
        assert!(
            text.contains("Next: enrol your passkey: this terminal has no screen, so print the setup code with: arca setup-code --config /home/u/.config/arca/node.json\n      To open the page from your own computer: ssh -o ExitOnForwardFailure=yes -L localhost:7471:127.0.0.1:7471 <this machine>, then open http://localhost:7471/approve there.\n      A phone cannot open this page: that needs the node reachable under a name, on a tailnet or a domain."),
            "{text}"
        );
        assert!(text.ends_with(MORE), "{text}");

        // A passkey is trusted: nothing is needed, and the page is where proposals wait.
        let text = words(
            &with_page(page, true, None, Some(2)),
            &cfg,
            Path::new(CONFIG),
            &quiet(),
            true,
        );
        assert!(text.contains("Passkeys: 2 trusted\n"), "{text}");
        assert!(
            text.ends_with(&format!("{}{MORE}", "Next: nothing is needed. Ask your assistant; what it proposes waits for you at http://localhost:7471/approve")),
            "{text}"
        );
        assert!(!text.contains("ssh "), "{text}");

        // The page is closed: its reason in the node's words, and the next step is to approve nothing there.
        let why = "The approval page is closed: this node does not hold [::1]:7471 (address in use: another program holds this port).";
        let text = words(
            &with_page(page, false, Some(why), Some(0)),
            &cfg,
            Path::new(CONFIG),
            &quiet(),
            true,
        );
        assert!(
            text.contains(&format!(
                "Approval page: http://localhost:7471/approve is CLOSED. {why}\n"
            )),
            "{text}"
        );
        assert!(
            text.ends_with(&format!("{}{MORE}", "Next: the approval page is closed. Type no setup code and approve nothing at that address until `arca status` says it is open.")),
            "{text}"
        );
        assert!(!text.contains("press \"Get a setup code\""), "{text}");

        // A page under a name of the owner's own: no ssh line and nothing about a phone, which can open it.
        let text = words(
            &with_page("https://arca.example.com/approve", true, None, Some(0)),
            &cfg,
            Path::new(CONFIG),
            &quiet(),
            true,
        );
        assert!(
            text.contains("open https://arca.example.com/approve and press"),
            "{text}"
        );
        assert!(!text.contains("ssh ") && !text.contains("phone"), "{text}");

        // The kernel was busy and did not count: the words do not invent a number.
        let text = words(
            &with_page(page, true, None, None),
            &cfg,
            Path::new(CONFIG),
            &quiet(),
            true,
        );
        assert!(
            text.contains("Passkeys: the node is busy with a change and did not say\n"),
            "{text}"
        );

        // A node that says nothing of a page while its config names one: said as exactly that, never as open.
        let mut named: NodeConfig = config(json!({ "loopback": "127.0.0.1:7471" }));
        named.approval_origin = Some("http://localhost:7471".to_string());
        let text = words(
            &answer("kernel", true),
            &named,
            Path::new(CONFIG),
            &quiet(),
            true,
        );
        assert!(
            text.contains("Approval page: the config names http://localhost:7471/approve, and the node did not say whether it serves it"),
            "{text}"
        );
        assert!(!text.contains("(open)"), "{text}");
    }

    #[test]
    fn changes_on_says_each_one_waits_only_where_no_token_changes_things_without_the_page() {
        let cfg = config(json!({}));
        let on = |tokens: &DirectTokens| {
            words(
                &answer("kernel", true),
                &cfg,
                Path::new(CONFIG),
                tokens,
                true,
            )
        };
        // A doctor or operator token does not wait for the page: it is named, with the way to take it away, and the
        // plain sentence, which would be false, is not printed. This holds on a node with no page too.
        let text = on(&DirectTokens::Read(vec![
            "doctor-1 (doctor)".to_string(),
            "ops (operator)".to_string(),
        ]));
        assert!(
            text.contains("Changes: on. What an assistant proposes waits for the owner's approval with a passkey. NOT every change does: these tokens change things directly, with no card and no passkey: doctor-1 (doctor), ops (operator). To take one away: arca token revoke <id> --config /home/u/.config/arca/node.json\nApproval page: none\n"),
            "{text}"
        );
        assert!(!text.contains("Each one waits"), "{text}");
        // The token directory could not be read: that is said, and "each one waits" is not.
        let text = on(&DirectTokens::Unread("/home/u/.config/arca/tokens".into()));
        assert!(
            text.contains("The token directory /home/u/.config/arca/tokens could not be read, so a doctor or operator token, which does not wait, may exist"),
            "{text}"
        );
        assert!(!text.contains("Each one waits"), "{text}");
        // Every token read, and none does more than look and propose: the plain sentence, with its reason.
        let text = on(&quiet());
        assert!(
            text.contains("Changes: on. Each one waits for the owner's approval with a passkey: no token of this node may do more than look and propose\n"),
            "{text}"
        );
        // A node that only looks changes nothing, whatever its tokens.
        let text = words(
            &answer("watch-only", true),
            &cfg,
            Path::new(CONFIG),
            &DirectTokens::Read(vec!["doctor-1 (doctor)".to_string()]),
            true,
        );
        assert!(
            text.contains("Changes: off. This node only looks\n"),
            "{text}"
        );
        assert!(!text.contains("doctor-1"), "{text}");
    }

    #[test]
    fn the_cautions_are_one_command_away_and_only_about_the_doors_this_config_opens() {
        // The installer's node: the door, and every caution the installer's report used to print.
        let text = security(
            &config(json!({ "loopback": "127.0.0.1:7471" })),
            Path::new(CONFIG),
            &quiet(),
        );
        for said in [
            "What to know about the doors of the node that /home/u/.config/arca/node.json describes.",
            "The HTTP door, http://127.0.0.1:7471/mcp, answers this machine only, and only with a token.",
            "keeps the token in its own config, in clear text",
            "puts the token in that command's arguments for a moment",
            "not the name localhost: the name can also mean ::1",
            "another account can take port 7471, and would then receive what a client sends there, the token included",
            "sh install.sh --no-door",
        ] {
            assert!(text.contains(said), "{said:?} is missing:\n{text}");
        }
        assert!(!text.contains("approval page"), "{text}");

        // No door: no caution about one.
        let text = security(&config(json!({})), Path::new(CONFIG), &quiet());
        assert!(
            text.contains("This node has no HTTP door on this machine"),
            "{text}"
        );
        assert!(!text.contains("claude mcp add"), "{text}");

        // The page of this machine alone brings its own.
        let mut local: NodeConfig = config(json!({ "loopback": "127.0.0.1:7471" }));
        local.approval_origin = Some("http://localhost:7471".to_string());
        let text = security(&local, Path::new(CONFIG), &quiet());
        for said in [
            "The approval page, http://localhost:7471/approve, is the page of this machine alone.",
            "every account, and every agent that shares its network, is trusted",
            "A phone cannot open this page.",
        ] {
            assert!(text.contains(said), "{said:?} is missing:\n{text}");
        }
    }

    /// `arca init` ends in a handful of lines and one pointer here. Every sentence that left its report is in these
    /// words: what the node may touch, the tokens that do not wait and what they may do, where the templates and the
    /// old config are, the way to the page from another computer, and what a node set back to watch-only keeps.
    #[test]
    fn what_left_inits_report_is_said_by_status_security() {
        let scratch = std::env::temp_dir().join(format!(
            "arca-status-security-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_nanos())
        ));
        std::fs::create_dir_all(&scratch).expect("a scratch directory");
        let path = scratch.join("node.json");

        let mut changing: NodeConfig = config(json!({ "loopback": "127.0.0.1:7471" }));
        changing.approval_origin = Some("http://localhost:7471".to_string());
        changing.kernel.projects = vec![arca_netproject::PROJECT.to_string()];
        changing.kernel.droidd_projects = vec!["default".to_string()];
        changing.kernel.service_cidr = Some("10.47.0.0/24".to_string());
        let direct = DirectTokens::Read(vec!["ops (doctor)".to_string()]);

        let text = security(&changing, &path, &direct);
        for said in [
            format!(
                "This node may change things. It may touch the Incus project {} and the bridge {} on 10.47.0.0/24.",
                arca_netproject::PROJECT,
                arca_netproject::BRIDGE
            ),
            "each only from a card you approve: snapshots (the pools they are kept in and the project arcak-snapshots) and the GPU devices of its own instances. Nothing else.".to_string(),
            "Arca never writes in \"default\"; that entry in kernel.droiddProjects only names it as not Arca's.".to_string(),
            "The service templates it creates from are in /nonexistent/state/templates. A database comes from the postgres-autopg one.".to_string(),
            "NOT every change waits for you. This node has 1 token(s) that may change things directly,".to_string(),
            "    ops (doctor)".to_string(),
            "create a database".to_string(),
            format!("To take one away first: arca token revoke <id> --config {}", path.display()),
            "To open it from your own computer: ssh -o ExitOnForwardFailure=yes -L localhost:7471:127.0.0.1:7471 <this machine>, then open http://localhost:7471/approve in its browser. If ssh warns that it could not bind the port, close it".to_string(),
            "A phone cannot open this page. Approving from a phone needs the node reachable under a name, on a tailnet or a domain".to_string(),
            "check that `arca status` says the page is open".to_string(),
        ] {
            assert!(text.contains(&said), "{said:?} is missing:\n{text}");
        }
        // No config was kept yet, so none is named; once init kept one, it is.
        assert!(!text.contains(".before-init"), "{text}");
        std::fs::write(scratch.join("node.json.before-init"), "{}").expect("the kept config");
        let text = security(&changing, &path, &direct);
        assert!(
            text.contains(&format!(
                "arca init kept the config as it was before its last change at {}.before-init.",
                path.display()
            )),
            "{text}"
        );

        // With no such token, that is said, and nothing is named.
        let text = security(&changing, &path, &quiet());
        assert!(
            text.contains("No token of this node may do more than look and propose, so every change waits for your passkey."),
            "{text}"
        );
        assert!(!text.contains("NOT every change"), "{text}");

        // Set back to watch-only: what was kept, and that the reading tools still answer.
        changing.kernel.projects.clear();
        let text = security(&changing, &path, &direct);
        for said in [
            "This node only looks: it starts, stops, creates and removes nothing, and a proposal that waits cannot run.",
            "nothing was removed: the Incus project arcak-svc, its bridge, the services in it, your passkeys and every other key of the config are as they were.",
            "The reading tools still show what is there (service_list, service_status, service_logs and the others), and every answer says that the node only looks. Every tool that would change something is refused with that same sentence.",
        ] {
            assert!(text.contains(said), "{said:?} is missing:\n{text}");
        }
        assert!(!text.contains("may change things"), "{text}");

        // A node that never changed anything is told none of it.
        let text = security(&config(json!({})), &path, &quiet());
        assert!(
            !text.contains("only looks") && !text.contains("may change things"),
            "{text}"
        );
        std::fs::remove_dir_all(&scratch).ok();
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
