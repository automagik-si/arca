//! A node's host forwarding and firewall as declared state.
//!
//! The owner's declared state is one root-owned file: whether the host forwards packets, and the bridges whose containers
//! may reach out. A root part of the `arca` binary, run by a systemd system unit, sets the two forwarding sysctls and, when
//! Docker is there, puts one accept for each bridge into Docker's `DOCKER-USER` chain (a drop in Docker's own chains beats
//! any accept elsewhere, so that chain is the only place an accept holds). It reads the host again every minute and writes
//! a status file the user-level node reads. It answers on a root-owned unix socket with two verbs, `read` and
//! `apply-declared-state`, and neither takes an argument, so no caller can make root do anything but read, or put back what
//! the declared file says.
//!
//! Every command the root part runs goes through a [`runner::Runner`], so the tests use a fake host and no test needs root,
//! iptables or sysctl.

pub mod client;
pub mod declared;
pub mod fake;
pub mod firewall;
pub mod install;
pub mod plan;
pub mod root;
pub mod runner;
pub mod socket;
pub mod status;

use std::path::PathBuf;

/// The declared state, owned by root.
pub const DECLARED_FILE: &str = "/etc/arca/host.json";
/// The status file the root part writes and the user-level node reads. It lives in a directory of the unit's own: the
/// node's sockets are in `/run/arca`, which a unit's `RuntimeDirectory` would chown to root and remove when it stops.
pub const STATUS_FILE: &str = "/run/arca-host-network/status.json";
/// The socket the root part listens on.
pub const SOCKET_FILE: &str = "/run/arca-host-network/host-network.sock";
/// The systemd system unit the install writes.
pub const UNIT_NAME: &str = "arca-host-network.service";
/// Where the unit's file is written.
pub const UNIT_FILE: &str = "/etc/systemd/system/arca-host-network.service";
/// The root-owned copy of the `arca` binary the unit runs. A binary in a user's home would hand that user root.
pub const INSTALLED_BINARY: &str = "/usr/local/libexec/arca/arca";
/// How often the root part reads the host again, in seconds.
pub const REREAD_SECONDS: u64 = 60;
/// A status file older than this is stale, in milliseconds: three rereads.
pub const STATUS_MAX_AGE_MS: u64 = 3 * REREAD_SECONDS * 1_000;

/// Every file the root part and the install touch. A test gives its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub declared: PathBuf,
    pub status: PathBuf,
    pub socket: PathBuf,
    pub unit_file: PathBuf,
    pub installed_binary: PathBuf,
}

impl Paths {
    /// The paths of a real host.
    pub fn system() -> Paths {
        Paths {
            declared: PathBuf::from(DECLARED_FILE),
            status: PathBuf::from(STATUS_FILE),
            socket: PathBuf::from(SOCKET_FILE),
            unit_file: PathBuf::from(UNIT_FILE),
            installed_binary: PathBuf::from(INSTALLED_BINARY),
        }
    }
}
