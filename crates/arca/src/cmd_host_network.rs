//! `arca host-network`: the host's forwarding and firewall as declared state. `status` tells what the root part found,
//! `run` is the root part itself (the system unit runs it) and `install` is the takeover that declares what the host has now
//! and starts the root part. See "Host forwarding and the firewall" in docs/OPERATING.md.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread;
use std::time::Duration;

use arca_hostnet::root::{Host, now_ms};
use arca_hostnet::runner::{Runner, System};
use arca_hostnet::status::Status;
use arca_hostnet::{Paths, client, install, plan, status as status_file};
use serde_json::{Value, json};

use crate::{Failure, Flags, parse_flags, unknown_word};

/// How long `install` waits for the started root part to answer on its socket before it says so.
const START_PATIENCE: Duration = Duration::from_secs(6);

/// `host-network status | run | install`.
pub fn host_network(rest: &[String]) -> Result<Option<Value>, Failure> {
    match rest.split_first() {
        Some((action, rest)) if action == "status" => {
            status(&parse_flags(rest, &["config"], &["live"])?)
        }
        Some((action, rest)) if action == "run" => run(&parse_flags(rest, &[], &[])?),
        Some((action, rest)) if action == "install" => install(&parse_flags(
            rest,
            &["takeover", "bridges"],
            &["replace", "dry-run"],
        )?),
        other => Err(unknown_word(
            "a host-network action",
            other.map(|(action, _)| action.as_str()),
            &["status", "run", "install"],
        )),
    }
}

/// The paths of the root part's files as this node's config names them, or the places the install writes them when there
/// is no config to read: `status` is for anyone, with or without a node.
fn node_paths(flags: &Flags) -> Paths {
    let mut paths = Paths::system();
    if let Ok((_, config)) = crate::load_chosen_config(flags) {
        paths.socket = config.host_network.socket;
        paths.status = config.host_network.status;
    }
    paths
}

/// A status as the command prints it: the status file's own fields, its age, and whether it is too old to trust.
pub fn status_value(status: &Status, now: u64, source: &str) -> Value {
    let age_ms = now.saturating_sub(status.at);
    json!({
        "source": source,
        "at": status.at,
        "ageSeconds": age_ms / 1_000,
        "stale": age_ms > arca_hostnet::STATUS_MAX_AGE_MS,
        "declared": status.declared,
        "inSync": status.in_sync,
        "drift": status.drift,
        "applied": status.applied,
        "errors": status.errors,
        "version": status.version,
    })
}

/// `host-network status [--live] [--config <file>]`.
fn status(flags: &Flags) -> Result<Option<Value>, Failure> {
    let paths = node_paths(flags);
    if flags.switch("live") {
        return match client::read(&paths.socket) {
            Ok(status) => Ok(Some(status_value(&status, now_ms(), "socket"))),
            Err(error) => Err(Failure::Runtime(format!(
                "the root part does not answer on {}: {error:?}. It is not installed or not running: see \"Host forwarding and the firewall\" in docs/OPERATING.md",
                paths.socket.display()
            ))),
        };
    }
    match status_file::read(&paths.status) {
        Some(status) => Ok(Some(status_value(&status, now_ms(), "file"))),
        None => Err(Failure::Runtime(format!(
            "there is no status at {}: the root part is not installed or not running",
            paths.status.display()
        ))),
    }
}

/// The user running this process is root.
fn is_root() -> bool {
    rustix::process::geteuid().is_root()
}

/// `host-network run`: the root part, until the system stops it.
fn run(_flags: &Flags) -> Result<Option<Value>, Failure> {
    if !is_root() {
        return Err(Failure::Refused(
            "host-network run changes the host's firewall, so it runs only as root: the system unit runs it".to_string(),
        ));
    }
    let host = Arc::new(Host::new(System, Paths::system(), Some(0)));
    let stop = Arc::new(AtomicBool::new(false));
    arca_hostnet::root::run(
        host,
        Duration::from_secs(arca_hostnet::REREAD_SECONDS),
        stop,
    )
    .map_err(|error| {
        Failure::Runtime(format!(
            "the root part could not listen on {}: {error}",
            arca_hostnet::SOCKET_FILE
        ))
    })?;
    Ok(None)
}

/// The bridges of `--bridges a,b`, in the order given.
fn bridge_list(text: Option<&str>) -> Vec<String> {
    text.map(|text| {
        text.split(',')
            .map(str::trim)
            .filter(|bridge| !bridge.is_empty())
            .map(String::from)
            .collect()
    })
    .unwrap_or_default()
}

/// `host-network install [--takeover <unit>] [--bridges <a,b>] [--replace] [--dry-run]`.
fn install(flags: &Flags) -> Result<Option<Value>, Failure> {
    let dry_run = flags.switch("dry-run");
    if !dry_run && !is_root() {
        return Err(Failure::Refused(
            "host-network install writes /etc and starts a system unit, so it runs only as root; --dry-run prints every step without root and changes nothing".to_string(),
        ));
    }
    let options = install::Options {
        takeover: flags.value("takeover").map(String::from),
        bridges: bridge_list(flags.value("bridges")),
        replace: flags.switch("replace"),
    };
    let exe = std::env::current_exe().map_err(|error| {
        Failure::Runtime(format!("the running binary could not be found: {error}"))
    })?;
    install_with(&System, &Paths::system(), &options, &exe, dry_run)
}

/// The install over a runner and paths, so a test gives a fake host.
pub fn install_with(
    runner: &dyn Runner,
    paths: &Paths,
    options: &install::Options,
    exe: &Path,
    dry_run: bool,
) -> Result<Option<Value>, Failure> {
    let observed = plan::observe(runner);
    let steps = install::plan(options, &observed, paths, exe, paths.declared.exists())
        .map_err(Failure::Refused)?;
    let declared = match steps.first() {
        Some(install::Step::Write { content, .. }) => {
            serde_json::from_str::<Value>(content).unwrap_or(Value::Null)
        }
        _ => Value::Null,
    };
    if dry_run {
        return Ok(Some(json!({
            "dryRun": true,
            "steps": install::render(&steps),
            "declared": declared,
        })));
    }
    let done = install::execute(&steps, runner).map_err(Failure::Runtime)?;
    // The unit applies the declared state as it starts: read it back.
    let started = std::time::Instant::now();
    let read_back = loop {
        if let Ok(status) = client::read(&paths.socket) {
            break Some(status_value(&status, now_ms(), "socket"));
        }
        if started.elapsed() >= START_PATIENCE {
            break None;
        }
        thread::sleep(Duration::from_millis(300));
    };
    Ok(Some(json!({
        "dryRun": false,
        "steps": done,
        "declared": declared,
        "status": read_back,
        "note": if read_back.is_none() {
            json!("the unit was started but its socket did not answer in time: run arca host-network status --live")
        } else {
            Value::Null
        },
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_hostnet::fake::FakeHost;
    use arca_hostnet::status::DriftItem;

    fn sample(at: u64) -> Status {
        Status {
            at,
            declared: true,
            in_sync: false,
            drift: vec![DriftItem {
                item: "ipv4-forwarding".to_string(),
                detail: "IPv4 forwarding is off, and it is declared on".to_string(),
            }],
            applied: Vec::new(),
            errors: Vec::new(),
            version: "0.0.0".to_string(),
        }
    }

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_string()).collect()
    }

    #[test]
    fn a_status_prints_its_age_and_says_when_it_is_too_old_to_trust() {
        let fresh = status_value(&sample(1_000_000), 1_030_000, "file");
        assert_eq!(fresh["ageSeconds"], 30);
        assert_eq!(fresh["stale"], false);
        assert_eq!(fresh["inSync"], false);
        assert_eq!(fresh["drift"][0]["item"], "ipv4-forwarding");
        assert_eq!(fresh["source"], "file");
        let old = status_value(
            &sample(1_000_000),
            1_000_000 + arca_hostnet::STATUS_MAX_AGE_MS + 1,
            "file",
        );
        assert_eq!(old["stale"], true);
        // A clock behind the file's is not a negative age.
        assert_eq!(status_value(&sample(5_000), 1_000, "file")["ageSeconds"], 0);
    }

    #[test]
    fn an_unknown_action_is_answered_with_the_ones_there_are() {
        let refused = host_network(&args(&["instal"])).expect_err("refused");
        assert!(
            refused.message().contains("did you mean install"),
            "{}",
            refused.message()
        );
        assert!(host_network(&[]).is_err());
        // Root-only actions refuse before they change anything when the caller is not root.
        if !is_root() {
            assert!(matches!(
                host_network(&args(&["run"])),
                Err(Failure::Refused(_))
            ));
            assert!(matches!(
                host_network(&args(&["install"])),
                Err(Failure::Refused(_))
            ));
        }
    }

    #[test]
    fn bridges_are_split_on_commas_and_blanks_are_dropped() {
        assert_eq!(bridge_list(Some("a, b,,c")), ["a", "b", "c"]);
        assert!(bridge_list(None).is_empty());
    }

    #[test]
    fn a_dry_run_prints_every_step_and_the_declared_state_and_changes_nothing() {
        let host = FakeHost::new()
            .forwarding(true, true)
            .docker()
            .hand_fix("incusbr0");
        let paths = Paths::system();
        let options = install::Options {
            takeover: Some("hand-fix.service".to_string()),
            ..install::Options::default()
        };
        let value = install_with(&host, &paths, &options, Path::new("/x/arca"), true)
            .expect("plans")
            .expect("a result");
        assert_eq!(value["dryRun"], true);
        assert_eq!(value["declared"]["bridges"][0], "incusbr0");
        assert_eq!(value["declared"]["forwarding"], true);
        assert_eq!(value["declared"]["forwardPolicy"], "DROP");
        let steps = value["steps"].as_array().expect("steps");
        assert_eq!(steps.len(), 7);
        assert_eq!(steps[6], "run systemctl disable hand-fix.service");
        assert!(host.changes().is_empty(), "a dry run runs no change");
    }

    #[test]
    fn an_install_that_cannot_go_ahead_is_a_refusal_before_any_write() {
        let host = FakeHost::new().iptables_exits(4);
        let refused = install_with(
            &host,
            &Paths::system(),
            &install::Options::default(),
            Path::new("/x/arca"),
            false,
        )
        .expect_err("refused");
        assert!(matches!(refused, Failure::Refused(_)));
        assert!(host.changes().is_empty());
    }
}
