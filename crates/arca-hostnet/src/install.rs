//! The takeover install: writes the declared file from what the host has now, copies the binary to a root-owned place, writes
//! and starts the system unit, and disables the hand-made unit named on the command line, so the hand fix is replaced and
//! not doubled. [`plan`] decides every step and touches nothing; [`execute`] runs them; [`render`] prints them for a dry run.

use std::path::{Path, PathBuf};

use std::io::Write;

use crate::declared::{Declared, valid_bridge};
use crate::plan::{Firewall, Observed};
use crate::runner::{Cmd, Runner};
use crate::{Paths, UNIT_NAME};

/// What the install is told.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// The hand-made unit to disable, by name (`something.service`).
    pub takeover: Option<String>,
    /// Bridges to declare besides the ones the host already accepts.
    pub bridges: Vec<String>,
    /// Write over a declared file that is already there.
    pub replace: bool,
}

/// One step of the install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Write a file whole, with this mode, and replace what was there.
    Write {
        path: PathBuf,
        mode: u32,
        content: String,
    },
    /// Copy a file to a place, with this mode.
    Copy {
        from: PathBuf,
        to: PathBuf,
        mode: u32,
    },
    Run(Cmd),
}

/// The declared state of a host as it is now: forwarding as it is, the bridges `DOCKER-USER` already accepts and the ones
/// named, and the `FORWARD` policy as it is.
pub fn declared_from(observed: &Observed, extra: &[String]) -> Declared {
    let mut bridges: Vec<String> = Vec::new();
    if let Firewall::Rules(rules) = &observed.firewall {
        bridges.extend(rules.accepted_bridges());
    }
    for bridge in extra {
        if !bridges.contains(bridge) {
            bridges.push(bridge.clone());
        }
    }
    bridges.sort();
    Declared {
        forwarding: observed.ipv4.unwrap_or(false),
        bridges,
        forward_policy: match &observed.firewall {
            Firewall::Rules(rules) => rules.forward_policy(),
            _ => None,
        },
    }
}

/// The text of the system unit that runs the installed binary.
pub fn unit_text(paths: &Paths) -> String {
    format!(
        "# The host's forwarding and firewall, as declared in {declared}. Written by arca host-network install.\n\
[Unit]\n\
Description=Arca host forwarding and firewall, as declared\n\
After=network-online.target docker.service\n\
Wants=network-online.target\n\
\n\
[Service]\n\
Type=simple\n\
ExecStart={binary} host-network run\n\
Restart=on-failure\n\
RestartSec=10\n\
RuntimeDirectory=arca\n\
RuntimeDirectoryMode=0755\n\
\n\
[Install]\n\
WantedBy=multi-user.target\n",
        declared = paths.declared.display(),
        binary = paths.installed_binary.display()
    )
}

/// Whether `name` is a unit this install may disable: a plain `.service` name that is not its own.
pub fn valid_takeover(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".service") else {
        return false;
    };
    name != UNIT_NAME
        && !stem.is_empty()
        && !stem.starts_with(['-', '.'])
        && stem
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '.' | '@' | '-'))
}

/// Every step of the install, or why it cannot go ahead. `declared_exists` is whether the declared file is already there.
pub fn plan(
    options: &Options,
    observed: &Observed,
    paths: &Paths,
    current_exe: &Path,
    declared_exists: bool,
) -> Result<Vec<Step>, String> {
    if let Some(unit) = &options.takeover
        && !valid_takeover(unit)
    {
        return Err(
            "--takeover names a unit this install may not disable: a plain name ending in .service, other than the install's own".to_string(),
        );
    }
    if let Some(bridge) = options.bridges.iter().find(|bridge| !valid_bridge(bridge)) {
        return Err(format!(
            "--bridges names {bridge:?}, which is not a bridge name"
        ));
    }
    if let Firewall::Unreadable(why) = &observed.firewall {
        return Err(format!(
            "the host's firewall could not be read, so it cannot be declared from what the host has now: {why}"
        ));
    }
    if declared_exists && !options.replace {
        return Err(format!(
            "{} is already there and is kept: pass --replace to declare the host again from what it has now",
            paths.declared.display()
        ));
    }
    let declared = declared_from(observed, &options.bridges);
    let systemctl = |args: &[&str]| Step::Run(Cmd::new("systemctl", args));
    let mut steps = vec![
        Step::Write {
            path: paths.declared.clone(),
            mode: 0o644,
            content: declared.to_json(),
        },
        Step::Copy {
            from: current_exe.to_path_buf(),
            to: paths.installed_binary.clone(),
            mode: 0o755,
        },
        Step::Write {
            path: paths.unit_file.clone(),
            mode: 0o644,
            content: unit_text(paths),
        },
        systemctl(&["daemon-reload"]),
        systemctl(&["enable", UNIT_NAME]),
        systemctl(&["restart", UNIT_NAME]),
    ];
    // Last, and without stopping it: the new unit has adopted the rules the old one put in.
    if let Some(unit) = &options.takeover {
        steps.push(systemctl(&["disable", unit]));
    }
    Ok(steps)
}

/// Each step as a line, for a dry run.
pub fn render(steps: &[Step]) -> Vec<String> {
    steps
        .iter()
        .map(|step| match step {
            Step::Write { path, mode, .. } => format!("write {} (mode {mode:04o})", path.display()),
            Step::Copy { from, to, mode } => {
                format!(
                    "copy {} to {} (mode {mode:04o})",
                    from.display(),
                    to.display()
                )
            }
            Step::Run(cmd) => format!("run {}", cmd.line()),
        })
        .collect()
}

/// Runs the steps in order and stops at the first that fails. Returns the lines of the steps done.
pub fn execute(steps: &[Step], runner: &dyn Runner) -> Result<Vec<String>, String> {
    let mut done = Vec::new();
    for (step, line) in steps.iter().zip(render(steps)) {
        match step {
            Step::Write {
                path,
                mode,
                content,
            } => {
                put(path, *mode, |file| file.write_all(content.as_bytes()))
                    .map_err(|error| format!("{line}: {error}"))?;
            }
            Step::Copy { from, to, mode } => {
                let bytes = std::fs::read(from).map_err(|error| format!("{line}: {error}"))?;
                put(to, *mode, |file| file.write_all(&bytes))
                    .map_err(|error| format!("{line}: {error}"))?;
            }
            Step::Run(cmd) => match runner.run(cmd) {
                Ok(output) if output.code == 0 => {}
                Ok(output) => {
                    return Err(format!(
                        "{line} exited {}: {}",
                        output.code,
                        output.stderr.trim().chars().take(200).collect::<String>()
                    ));
                }
                Err(why) => return Err(format!("{line}: {why}")),
            },
        }
        done.push(line);
    }
    Ok(done)
}

/// Writes a file whole beside its place and renames it over, with `mode`, creating the directory it is in.
fn put(
    path: &Path,
    mode: u32,
    write: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".new");
    let temporary = PathBuf::from(temporary);
    let _ = std::fs::remove_file(&temporary);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&temporary)?;
    write(&mut file)?;
    file.sync_all()?;
    std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(mode))?;
    std::fs::rename(&temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::declared::Policy;
    use crate::fake::FakeHost;
    use crate::plan::observe;
    use std::os::unix::fs::PermissionsExt;

    fn paths() -> Paths {
        Paths::system()
    }

    fn hand_fixed() -> Observed {
        observe(
            &FakeHost::new()
                .forwarding(true, true)
                .docker()
                .hand_fix("incusbr0")
                .hand_fix("br-two"),
        )
    }

    #[test]
    fn the_declared_state_is_what_the_host_has_now() {
        let declared = declared_from(
            &hand_fixed(),
            &["extra0".to_string(), "incusbr0".to_string()],
        );
        assert!(declared.forwarding);
        assert_eq!(declared.bridges, ["br-two", "extra0", "incusbr0"]);
        assert_eq!(declared.forward_policy, Some(Policy::Drop));
        // A host with nothing set up declares what it has: forwarding off, no bridges, the policy it shows.
        let bare = declared_from(&observe(&FakeHost::new()), &[]);
        assert!(!bare.forwarding && bare.bridges.is_empty());
        let none = declared_from(&observe(&FakeHost::new().without_iptables()), &[]);
        assert_eq!(none.forward_policy, None);
    }

    #[test]
    fn the_unit_runs_the_root_owned_copy_after_docker() {
        let text = unit_text(&paths());
        assert!(
            text.contains("After=network-online.target docker.service\n"),
            "{text}"
        );
        assert!(text.contains("ExecStart=/usr/local/libexec/arca/arca host-network run\n"));
        assert!(
            text.contains("RuntimeDirectory=arca\n")
                && text.contains("RuntimeDirectoryMode=0755\n")
        );
        assert!(text.contains("WantedBy=multi-user.target"));
        // Docker is not required: a host without it still starts the unit.
        assert!(!text.contains("Requires=") && !text.contains("Wants=docker"));
    }

    #[test]
    fn only_a_plain_service_name_is_taken_over() {
        for good in [
            "incus-forward.service",
            "arca-fix@1.service",
            "a.b_c-d:e.service",
        ] {
            assert!(valid_takeover(good), "{good}");
        }
        for bad in [
            "",
            "incus-forward",
            "-x.service",
            "../x.service",
            "a b.service",
            "a/b.service",
            "a;b.service",
            UNIT_NAME,
            ".service",
        ] {
            assert!(!valid_takeover(bad), "{bad}");
        }
    }

    #[test]
    fn the_install_writes_the_file_then_the_binary_and_unit_then_starts_ours_and_only_then_disables_the_hand_one()
     {
        let options = Options {
            takeover: Some("hand-fix.service".to_string()),
            ..Options::default()
        };
        let steps = plan(
            &options,
            &hand_fixed(),
            &paths(),
            Path::new("/home/someone/.local/bin/arca"),
            false,
        )
        .expect("plans");
        let lines = render(&steps);
        assert_eq!(
            lines,
            [
                "write /etc/arca/host.json (mode 0644)",
                "copy /home/someone/.local/bin/arca to /usr/local/libexec/arca/arca (mode 0755)",
                "write /etc/systemd/system/arca-host-network.service (mode 0644)",
                "run systemctl daemon-reload",
                "run systemctl enable arca-host-network.service",
                "run systemctl restart arca-host-network.service",
                "run systemctl disable hand-fix.service",
            ]
        );
        let Step::Write { content, .. } = &steps[0] else {
            panic!("the declared file comes first");
        };
        assert_eq!(
            Declared::parse(content.as_bytes()).expect("a declared file"),
            declared_from(&hand_fixed(), &[])
        );
        // The hand-made unit is disabled, not stopped: stopping could remove the rules the new unit has just adopted.
        assert!(
            !lines
                .iter()
                .any(|line| line.contains("--now") || line.contains("stop"))
        );
    }

    #[test]
    fn without_a_takeover_nothing_is_disabled() {
        let steps = plan(
            &Options::default(),
            &hand_fixed(),
            &paths(),
            Path::new("/x/arca"),
            false,
        )
        .expect("plans");
        assert_eq!(steps.len(), 6);
        assert!(!render(&steps).iter().any(|line| line.contains("disable")));
    }

    #[test]
    fn the_install_refuses_what_it_could_do_wrong_before_it_does_anything() {
        let observed = hand_fixed();
        let refused = |options: &Options, exists: bool| {
            plan(options, &observed, &paths(), Path::new("/x/arca"), exists)
        };
        // A declared file that is there is kept unless the install is told to replace it.
        assert!(
            refused(&Options::default(), true)
                .expect_err("kept")
                .contains("--replace")
        );
        assert!(
            refused(
                &Options {
                    replace: true,
                    ..Options::default()
                },
                true
            )
            .is_ok()
        );
        let own = Options {
            takeover: Some(UNIT_NAME.to_string()),
            ..Options::default()
        };
        assert!(refused(&own, false).is_err());
        let bad_unit = Options {
            takeover: Some("a b.service".to_string()),
            ..Options::default()
        };
        assert!(refused(&bad_unit, false).is_err());
        let bad_bridge = Options {
            bridges: vec!["-j".to_string()],
            ..Options::default()
        };
        assert!(refused(&bad_bridge, false).is_err());
        // A host whose firewall cannot be read is not recorded as having none.
        let unreadable = observe(&FakeHost::new().iptables_exits(4));
        assert!(
            plan(
                &Options::default(),
                &unreadable,
                &paths(),
                Path::new("/x/arca"),
                false
            )
            .is_err()
        );
    }

    #[test]
    fn executing_writes_the_files_with_their_modes_and_runs_the_commands_in_order() {
        let dir = std::env::temp_dir().join(format!("arca-hostnet-install-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        let exe = dir.join("arca-user");
        std::fs::write(&exe, "binary").expect("exe");
        let paths = Paths {
            declared: dir.join("etc/arca/host.json"),
            status: dir.join("run/status.json"),
            socket: dir.join("run/host.sock"),
            unit_file: dir.join("etc/systemd/arca-host-network.service"),
            installed_binary: dir.join("libexec/arca/arca"),
        };
        let options = Options {
            takeover: Some("hand-fix.service".to_string()),
            ..Options::default()
        };
        let steps = plan(&options, &hand_fixed(), &paths, &exe, false).expect("plans");
        let fake = FakeHost::new();
        let done = execute(&steps, &fake).expect("runs");
        assert_eq!(done.len(), steps.len());
        assert!(
            std::fs::read_to_string(&paths.declared)
                .expect("declared")
                .contains("incusbr0")
        );
        let mode =
            |path: &Path| std::fs::metadata(path).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode(&paths.declared), 0o644);
        assert_eq!(mode(&paths.installed_binary), 0o755);
        assert_eq!(
            std::fs::read_to_string(&paths.installed_binary).expect("copy"),
            "binary"
        );
        assert!(
            std::fs::read_to_string(&paths.unit_file)
                .expect("unit")
                .contains(&format!(
                    "ExecStart={} host-network run",
                    paths.installed_binary.display()
                ))
        );
        assert_eq!(
            fake.log(),
            [
                "systemctl daemon-reload",
                "systemctl enable arca-host-network.service",
                "systemctl restart arca-host-network.service",
                "systemctl disable hand-fix.service",
            ]
        );
        // A step that fails stops the install there.
        let failing = FakeHost::new().failing("enable");
        let error = execute(&steps, &failing).expect_err("stops");
        assert!(
            error.contains("systemctl enable arca-host-network.service"),
            "{error}"
        );
        assert_eq!(failing.log().len(), 2, "nothing runs after the failure");
        let _ = std::fs::remove_dir_all(dir);
    }
}
