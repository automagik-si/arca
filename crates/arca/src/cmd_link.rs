//! Node link commands. Pairing links never appear in errors.
use crate::{Failure, parse_flags, unknown_word};
use arca_link::{Link, Network, PairLink, State};
use serde_json::{Value, json};
use tokio::signal::unix::{SignalKind, signal};

fn failure(error: arca_link::Error) -> Failure {
    match error {
        arca_link::Error::InvalidLink => Failure::Usage(error.to_string()),
        arca_link::Error::Config(_) => Failure::Config(error.to_string()),
        arca_link::Error::Runtime(_) => Failure::Runtime(error.to_string()),
    }
}

pub fn link(args: &[String]) -> Result<Option<Value>, Failure> {
    let actions = ["serve", "pair", "join", "peers", "remove"];
    let (command, mut args) = match args.split_first() {
        Some((command, args)) if actions.contains(&command.as_str()) => (command, args),
        other => {
            return Err(unknown_word(
                "a link action",
                other.map(|(command, _)| command.as_str()),
                &actions,
            ));
        }
    };
    let positional = if command == "join" || command == "remove" {
        let end_of_options = args.first().is_some_and(|arg| arg == "--");
        if end_of_options {
            args = &args[1..];
        }
        let Some((value, rest)) = args
            .split_first()
            .filter(|(v, _)| end_of_options || !v.starts_with("--"))
        else {
            return Err(Failure::Usage(if command == "join" {
                "link join needs the link the other node's `arca link pair` printed: arca link join <link>"
                    .to_string()
            } else {
                "link remove needs the name of the peer to remove; arca link peers lists them: arca link remove <name>"
                    .to_string()
            }));
        };
        args = rest;
        Some(value.as_str())
    } else {
        None
    };
    // Parse the secret-bearing positional before the general flag parser can echo unknown keys.
    let parsed_link = if command == "join" {
        Some(PairLink::parse(positional.unwrap_or_default()).map_err(failure)?)
    } else {
        None
    };
    let allowed = if command == "pair" {
        &["config", "minutes"][..]
    } else {
        &["config"][..]
    };
    let flags = parse_flags(args, allowed, &[])?;
    // Listing the peers only reads; removing one cuts a peer off; every other action runs or changes the node's link.
    let does = match command.as_str() {
        "peers" => crate::Does::Reads,
        "remove" => crate::Does::Removes,
        _ => crate::Does::Acts,
    };
    // Removing a peer cuts it off from one node, and pairing or joining hands a peer out on one node: the variable alone
    // never picks that node. Running the link and listing its peers follow the variable as every command does.
    let (_, config) = if matches!(command.as_str(), "remove" | "pair" | "join") {
        crate::load_node_config_to(&flags, does)?
    } else {
        crate::load_config_to(&flags, does)?
    };
    let mut link_config = config.link.clone().unwrap_or_default();
    if command == "serve" {
        link_config
            .forward
            .entry("arca/mcp/1".to_string())
            .or_insert(config.listen.unix);
    }
    let state = State::new(config.state_dir).map_err(failure)?;
    match command.as_str() {
        "peers" => return Ok(Some(json!(state.peers().map_err(failure)?))),
        "pair" => {
            let minutes = flags
                .value("minutes")
                .map(|s| {
                    s.parse::<u64>()
                        .map_err(|_| Failure::Usage("--minutes must be a whole number".to_string()))
                })
                .transpose()?
                .or(link_config.pair_minutes)
                .unwrap_or(10);
            let output = state.pair(minutes).map_err(failure)?;
            eprint!("{}", arca_link::qr(&output.link).map_err(failure)?);
            return Ok(Some(json!(output)));
        }
        _ => {}
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| Failure::Runtime("cannot start link runtime".to_string()))?;
    runtime.block_on(async {
        if command == "remove" {
            let name = positional.unwrap_or_default();
            let count = if state.status().is_ok() {
                arca_link::remove_running(&state, name)
                    .await
                    .map_err(failure)?
            } else {
                state.remove(name).map_err(failure)?
            };
            return Ok(Some(json!({"removed": count})));
        }
        let network = if link_config.local_only {
            Network::local()
        } else {
            Network::N0
        };
        if let Some(parsed) = parsed_link {
            let peer = if state.status().is_ok() {
                arca_link::join_running(&state, positional.unwrap_or_default())
                    .await
                    .map_err(failure)?
            } else {
                let node = Link::start(state, link_config, network)
                    .await
                    .map_err(failure)?;
                let result = node.join(&parsed).await;
                node.shutdown().await;
                result.map_err(failure)?
            };
            return Ok(Some(json!({"id": peer.id, "name": peer.name})));
        }
        // Register both handlers before start can publish status.json.
        let mut terminate = signal(SignalKind::terminate())
            .map_err(|_| Failure::Runtime("cannot register link stop signal".to_string()))?;
        let mut interrupt = signal(SignalKind::interrupt())
            .map_err(|_| Failure::Runtime("cannot register link stop signal".to_string()))?;
        let mut node = Link::start(state, link_config, network)
            .await
            .map_err(failure)?;
        tokio::select! {
            _ = terminate.recv() => {},
            _ = interrupt.recv() => {},
            _ = node.stopped() => return Err(Failure::Runtime("link serve stopped".to_string())),
        }
        node.shutdown().await;
        Ok(None)
    })
}
