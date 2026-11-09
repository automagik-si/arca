//! The runtime choice of where a node's host readings come from. The config's explicit `incus.local` wins; otherwise the
//! node asks its Incus for its server name and compares it with the local host name. The socket path is only the fallback,
//! because the shipped units reach the local daemon through the recording shim, whose path is not the daemon's own. A
//! container that mounts the host's Incus socket at the standard path is still remote: its host name and the container's
//! differ, so the sampler never reports the container's `/proc` as the managed host's.

use std::path::Path;
use std::time::Duration;

use arca_incus::{IncusError, IncusRead};
use serde_json::Value;

use crate::procfs;

/// The Incus server document: its `environment.server_name` names the daemon.
pub const SERVER_PATH: &str = "/1.0";

/// The most a host-mode probe may take. The startup probe and each retry of a provisional choice use it, so a hung Incus
/// never delays startup or a sample by the sampler's ordinary deadline.
pub const MODE_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Where a node's host readings come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostMode {
    /// The node's Incus is the local daemon, so its host is this machine.
    Local,
    /// The node's Incus serves another machine.
    Remote,
}

impl HostMode {
    /// The label `arca_status.metricsSource` carries.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Remote => "remote",
        }
    }
}

/// The chosen mode and a short, value-free reason: it never carries a host name or any other host value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostModeChoice {
    pub mode: HostMode,
    pub reason: String,
    /// True when the choice is only the socket-path fallback. The sampler retries the server-name check on later samples
    /// until it gets a real answer, then keeps that answer.
    pub provisional: bool,
}

impl HostModeChoice {
    /// Whether the sampler may read the local `/proc` and `/sys` trees as the managed host's.
    pub fn is_local(&self) -> bool {
        self.mode == HostMode::Local
    }
}

/// How to retry a provisional choice: the explicit config value and the socket-path rule, which are all the resolver
/// needs besides the Incus client and the local host name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModeRetry {
    pub explicit: Option<bool>,
    pub path_is_local: bool,
}

/// Chooses the host mode. An explicit `local` wins. Otherwise `GET /1.0`'s `environment.server_name` is compared, without
/// case, with the local host name from `proc_root`. The socket path rule (`path_is_local`) is the fallback only when the
/// server or the local host name cannot be read.
pub fn choose_host_mode<R: IncusRead>(
    incus: &R,
    explicit: Option<bool>,
    path_is_local: bool,
    proc_root: &Path,
) -> HostModeChoice {
    resolve(incus.get(SERVER_PATH), explicit, path_is_local, proc_root)
}

/// Like [`choose_host_mode`], but the `/1.0` read may take at most `timeout`.
pub fn choose_host_mode_within<R: IncusRead>(
    incus: &R,
    explicit: Option<bool>,
    path_is_local: bool,
    proc_root: &Path,
    timeout: Duration,
) -> HostModeChoice {
    resolve(
        incus.get_within(SERVER_PATH, timeout),
        explicit,
        path_is_local,
        proc_root,
    )
}

fn resolve(
    server: Result<Value, IncusError>,
    explicit: Option<bool>,
    path_is_local: bool,
    proc_root: &Path,
) -> HostModeChoice {
    if let Some(local) = explicit {
        return choice(
            if local {
                "incus.local is true"
            } else {
                "incus.local is false"
            },
            local,
            false,
        );
    }
    let path_reason = if path_is_local {
        "the socket path is a standard local Incus socket"
    } else {
        "the socket path is not a standard local Incus socket"
    };
    let server = match server {
        Ok(document) => document,
        Err(_) => {
            return choice(
                &format!("the Incus server could not be read; {path_reason}"),
                path_is_local,
                true,
            );
        }
    };
    let Some(server_name) = server_name(&server) else {
        return choice(
            &format!("the Incus server did not name itself; {path_reason}"),
            path_is_local,
            true,
        );
    };
    let Some(hostname) = procfs::read_hostname(proc_root).and_then(|name| usable_name(&name))
    else {
        return choice(
            &format!("the local host name could not be read; {path_reason}"),
            path_is_local,
            true,
        );
    };
    if server_name.eq_ignore_ascii_case(&hostname) {
        choice(
            "the Incus server name matches the local host name",
            true,
            false,
        )
    } else {
        choice(
            "the Incus server name does not match the local host name",
            false,
            false,
        )
    }
}

fn choice(reason: &str, local: bool, provisional: bool) -> HostModeChoice {
    HostModeChoice {
        mode: if local {
            HostMode::Local
        } else {
            HostMode::Remote
        },
        reason: reason.to_string(),
        provisional,
    }
}

/// The `environment.server_name` of a server document, when it is a name that identifies a daemon. Empty, `localhost` and
/// `localhost.localdomain` name no machine, so they leave the choice to the socket-path fallback.
fn server_name(document: &Value) -> Option<String> {
    usable_name(document.pointer("/environment/server_name")?.as_str()?)
}

/// A host name that can identify a machine, without case. Empty, `localhost` and `localhost.localdomain` are unknown.
fn usable_name(name: &str) -> Option<String> {
    let name = name.trim();
    (!name.is_empty()
        && !name.eq_ignore_ascii_case("localhost")
        && !name.eq_ignore_ascii_case("localhost.localdomain"))
    .then(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    use arca_incus::IncusError;

    use super::*;

    /// A fresh temporary directory, removed when the test ends.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .subsec_nanos();
            let dir = std::env::temp_dir()
                .join(format!("arca-mode-{}-{nanos}-{tag}", std::process::id()));
            fs::create_dir_all(&dir).expect("create the scratch directory");
            Scratch(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        /// Writes the local host name where the real `/proc` keeps it.
        fn hostname(&self, name: &str) {
            let path = self.0.join("sys/kernel/hostname");
            fs::create_dir_all(path.parent().expect("a parent")).expect("create the tree");
            fs::write(path, format!("{name}\n")).expect("write the host name");
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// An Incus that serves one `/1.0` answer, or the error it was given, and records every path.
    #[derive(Clone)]
    struct Server(
        Result<Value, IncusError>,
        std::sync::Arc<Mutex<Vec<String>>>,
    );

    impl Server {
        fn naming(name: &str) -> Server {
            Server(
                Ok(serde_json::json!({ "environment": { "server_name": name } })),
                Default::default(),
            )
        }

        fn unreadable() -> Server {
            Server(
                Err(IncusError::Status {
                    code: 503,
                    message: "unavailable".to_string(),
                }),
                Default::default(),
            )
        }

        fn reads(&self) -> Vec<String> {
            self.1.lock().expect("the reads are not poisoned").clone()
        }
    }

    impl IncusRead for Server {
        fn get(&self, path: &str) -> Result<Value, IncusError> {
            self.1
                .lock()
                .expect("the reads are not poisoned")
                .push(path.to_string());
            if path != SERVER_PATH {
                return Err(IncusError::Status {
                    code: 404,
                    message: "not found".to_string(),
                });
            }
            self.0.clone()
        }
    }

    #[test]
    fn a_shim_style_path_to_a_local_daemon_is_local() {
        let scratch = Scratch::new("shim-local");
        scratch.hostname("node1");
        let incus = Server::naming("node1");
        let choice = choose_host_mode(&incus, None, false, scratch.path());
        assert_eq!(choice.mode, HostMode::Local);
        assert_eq!(
            choice.reason,
            "the Incus server name matches the local host name"
        );
        assert!(!choice.provisional, "a name comparison is a real answer");
        assert_eq!(incus.reads(), vec![SERVER_PATH.to_string()]);
    }

    #[test]
    fn a_proxy_to_another_host_is_remote() {
        let scratch = Scratch::new("proxy-remote");
        scratch.hostname("node1");
        let incus = Server::naming("other");
        let choice = choose_host_mode(&incus, None, false, scratch.path());
        assert_eq!(choice.mode, HostMode::Remote);
        assert_eq!(
            choice.reason,
            "the Incus server name does not match the local host name"
        );
        assert!(!choice.provisional, "a name comparison is a real answer");
    }

    #[test]
    fn the_server_name_comparison_ignores_case() {
        let scratch = Scratch::new("case");
        scratch.hostname("Node1");
        let incus = Server::naming("node1");
        assert_eq!(
            choose_host_mode(&incus, None, false, scratch.path()).mode,
            HostMode::Local
        );
    }

    #[test]
    fn an_explicit_local_or_remote_wins_over_the_server_name() {
        let scratch = Scratch::new("explicit");
        scratch.hostname("node1");
        let incus = Server::naming("node1");
        let forced_remote = choose_host_mode(&incus, Some(false), true, scratch.path());
        assert_eq!(forced_remote.mode, HostMode::Remote);
        assert_eq!(forced_remote.reason, "incus.local is false");
        assert!(
            !forced_remote.provisional,
            "an explicit value is a real answer"
        );

        let incus = Server::naming("other");
        let forced_local = choose_host_mode(&incus, Some(true), false, scratch.path());
        assert_eq!(forced_local.mode, HostMode::Local);
        assert_eq!(forced_local.reason, "incus.local is true");
        assert!(
            !forced_local.provisional,
            "an explicit value is a real answer"
        );
    }

    #[test]
    fn an_unreadable_server_falls_back_to_the_socket_path_rule() {
        let scratch = Scratch::new("unreadable");
        scratch.hostname("node1");
        let incus = Server::unreadable();
        let local = choose_host_mode(&incus, None, true, scratch.path());
        assert_eq!(local.mode, HostMode::Local);
        assert_eq!(
            local.reason,
            "the Incus server could not be read; the socket path is a standard local Incus socket"
        );
        assert!(local.provisional, "a path fallback is worth retrying");
        let remote = choose_host_mode(&incus, None, false, scratch.path());
        assert_eq!(remote.mode, HostMode::Remote);
        assert_eq!(
            remote.reason,
            "the Incus server could not be read; the socket path is not a standard local Incus socket"
        );
        assert!(remote.provisional, "a path fallback is worth retrying");
        assert_eq!(incus.reads().len(), 2);
    }

    #[test]
    fn a_server_that_does_not_name_itself_falls_back_to_the_socket_path() {
        let scratch = Scratch::new("no-name");
        scratch.hostname("node1");
        let incus = Server(
            Ok(serde_json::json!({ "environment": {} })),
            Default::default(),
        );
        let choice = choose_host_mode(&incus, None, true, scratch.path());
        assert_eq!(choice.mode, HostMode::Local);
        assert_eq!(
            choice.reason,
            "the Incus server did not name itself; the socket path is a standard local Incus socket"
        );
        assert!(choice.provisional, "a path fallback is worth retrying");
    }

    #[test]
    fn an_unreadable_local_host_name_falls_back_to_the_socket_path() {
        let scratch = Scratch::new("no-hostname");
        let incus = Server::naming("node1");
        let choice = choose_host_mode(&incus, None, false, scratch.path());
        assert_eq!(choice.mode, HostMode::Remote);
        assert_eq!(
            choice.reason,
            "the local host name could not be read; the socket path is not a standard local Incus socket"
        );
        assert!(choice.provisional, "a path fallback is worth retrying");
    }

    #[test]
    fn localhost_and_empty_names_are_unknown_and_leave_the_path_fallback() {
        for name in ["localhost", "LOCALHOST.localdomain", ""] {
            let scratch = Scratch::new("reserved-server");
            scratch.hostname("node1");
            let incus = Server::naming(name);
            let choice = choose_host_mode(&incus, None, false, scratch.path());
            assert_eq!(choice.mode, HostMode::Remote, "{name:?}");
            assert_eq!(
                choice.reason,
                "the Incus server did not name itself; the socket path is not a standard local Incus socket",
                "{name:?}"
            );
            assert!(choice.provisional, "{name:?}");
        }

        for name in ["localhost", "localhost.localdomain", ""] {
            let scratch = Scratch::new("reserved-host");
            scratch.hostname(name);
            let incus = Server::naming("node1");
            let choice = choose_host_mode(&incus, None, true, scratch.path());
            assert_eq!(choice.mode, HostMode::Local, "{name:?}");
            assert_eq!(
                choice.reason,
                "the local host name could not be read; the socket path is a standard local Incus socket",
                "{name:?}"
            );
            assert!(choice.provisional, "{name:?}");
        }
    }

    #[test]
    fn an_unreadable_server_and_a_server_that_does_not_name_itself_say_which_happened() {
        let scratch = Scratch::new("reason-text");
        scratch.hostname("node1");
        let unreadable = choose_host_mode(&Server::unreadable(), None, false, scratch.path());
        let unnamed = choose_host_mode(
            &Server(
                Ok(serde_json::json!({ "environment": {} })),
                Default::default(),
            ),
            None,
            false,
            scratch.path(),
        );
        assert_eq!(
            unreadable.reason,
            "the Incus server could not be read; the socket path is not a standard local Incus socket"
        );
        assert_eq!(
            unnamed.reason,
            "the Incus server did not name itself; the socket path is not a standard local Incus socket"
        );
    }

    #[test]
    fn default_image_names_are_ordinary_names() {
        // A stock default such as "ubuntu" is a real name. Two machines can share it, which the comparison cannot tell
        // apart; a match still reads as this machine, as it did before, and the limitation is documented.
        let scratch = Scratch::new("default-match");
        scratch.hostname("ubuntu");
        let incus = Server::naming("ubuntu");
        let choice = choose_host_mode(&incus, None, false, scratch.path());
        assert_eq!(choice.mode, HostMode::Local);
        assert!(!choice.provisional, "a name match is a real answer");
        assert_eq!(
            choice.reason,
            "the Incus server name matches the local host name"
        );

        // Two different names stay decisive, even when both are image defaults.
        let scratch = Scratch::new("default-mismatch");
        scratch.hostname("debian");
        let incus = Server::naming("ubuntu");
        let choice = choose_host_mode(&incus, None, false, scratch.path());
        assert_eq!(choice.mode, HostMode::Remote);
        assert!(!choice.provisional, "a name mismatch is a real answer");
        assert_eq!(
            choice.reason,
            "the Incus server name does not match the local host name"
        );
    }
}
