//! The Ceph copy of a stopped virtual machine's disks between two Ceph clusters, for `instance_move`.
//!
//! When a linked host names its Ceph cluster (`approval.linkedHosts[].ceph`), a stopped virtual machine on a Ceph pool
//! here moves to its Ceph pool there by `rbd`, not by an Incus push: Incus on the target makes the empty machine and its
//! empty volumes, of the source's exact sizes, and this node copies each image with `rbd export-diff <source> - | rbd
//! import-diff - <target>`: two processes and a pipe, never a shell. Each side names its own cluster
//! (`--cluster <name>` reads `/etc/ceph/<name>.conf` and the client's keyring beside it) and its own client, so the same
//! code runs whichever way the move goes. The source is only read: `rbd status`, `rbd info` and `rbd export-diff`. The
//! bytes stream from the reader to the writer through the pipe: the node holds no more of a disk than the pipe and the
//! two processes' own buffers.
//!
//! The [`Runner`] and the `rbd` command lines are the ones `instance_import` uses for a source pool on another Ceph
//! cluster (`kernel.instanceImport.cephCluster`); they are kept here, small, until the two share one module.

use std::io::Read as _;
use std::process::{Command, Stdio};

use serde_json::Value;

/// What runs a command for the copy.
pub trait Runner: Send + Sync {
    /// Runs `argv` and returns its standard output; a failure carries its standard error.
    fn output(&self, argv: &[String]) -> Result<String, String>;
    /// Runs `first` with its standard output piped into `second`, and waits for both.
    fn pipe(&self, first: &[String], second: &[String]) -> Result<(), String>;
}

/// Runs commands as processes.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessRunner;

/// The last 512 characters of a command's error output, on one line.
fn tail(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let line: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let line = line.trim();
    let count = line.chars().count();
    line.chars().skip(count.saturating_sub(512)).collect()
}

impl Runner for ProcessRunner {
    fn output(&self, argv: &[String]) -> Result<String, String> {
        let (program, rest) = argv.split_first().ok_or("no command")?;
        let out = Command::new(program)
            .args(rest)
            .stdin(Stdio::null())
            .output()
            .map_err(|error| format!("{program} did not start: {error}"))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(format!("{program} failed: {}", tail(&out.stderr)))
        }
    }

    fn pipe(&self, first: &[String], second: &[String]) -> Result<(), String> {
        let (program, rest) = first.split_first().ok_or("no command")?;
        let (into, into_rest) = second.split_first().ok_or("no command")?;
        let mut reader = Command::new(program)
            .args(rest)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("{program} did not start: {error}"))?;
        let stdout = reader.stdout.take().ok_or("the reader has no output")?;
        let writer = Command::new(into)
            .args(into_rest)
            .stdin(Stdio::from(stdout))
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn();
        let writer = match writer {
            Ok(writer) => writer,
            Err(error) => {
                let _ = reader.kill();
                let _ = reader.wait();
                return Err(format!("{into} did not start: {error}"));
            }
        };
        let mut reader_err = Vec::new();
        if let Some(mut stderr) = reader.stderr.take() {
            let _ = stderr.read_to_end(&mut reader_err);
        }
        let read = reader.wait().map_err(|error| error.to_string())?;
        let written = writer
            .wait_with_output()
            .map_err(|error| error.to_string())?;
        match (read.success(), written.status.success()) {
            (true, true) => Ok(()),
            (false, _) => Err(format!("{program} failed: {}", tail(&reader_err))),
            (true, false) => Err(format!("{into} failed: {}", tail(&written.stderr))),
        }
    }
}

/// One Ceph image, with the cluster and the client that reach it from this node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RbdImage {
    /// The cluster, by the name of its config file here (`/etc/ceph/<cluster>.conf`).
    pub cluster: String,
    /// The Ceph client, without its `client.` prefix.
    pub client: String,
    /// The OSD pool.
    pub pool: String,
    pub image: String,
}

impl RbdImage {
    pub fn spec(&self) -> String {
        format!("{}/{}", self.pool, self.image)
    }

    fn names_ok(&self) -> Result<(), String> {
        match [&self.cluster, &self.client, &self.pool, &self.image]
            .into_iter()
            .find(|name| !ceph_name_ok(name))
        {
            Some(name) => Err(format!("{name:?} is not a Ceph name")),
            None => Ok(()),
        }
    }

    fn argv(&self, rest: &[&str]) -> Vec<String> {
        [
            "rbd",
            "--cluster",
            &self.cluster,
            "--id",
            &self.client,
            "--no-progress",
        ]
        .iter()
        .chain(rest)
        .map(|part| (*part).to_string())
        .collect()
    }

    /// The command that lists who holds the image open.
    pub fn status_argv(&self) -> Vec<String> {
        self.argv(&["status", "--format", "json", &self.spec()])
    }

    /// The command that reads the image's size.
    pub fn info_argv(&self) -> Vec<String> {
        self.argv(&["info", "--format", "json", &self.spec()])
    }
}

/// Whether `name` is a plain Ceph name (a cluster, client, pool or image): letters, digits, `.`, `_` and `-`, not
/// starting with `-`, at most 255 bytes.
pub fn ceph_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// The Ceph image Incus keeps a volume in: `virtual-machine_<project>_<name>.block` for a virtual machine's disk,
/// `custom_<project>_<name>` for a custom volume (`.block` added for a block one). The project prefix is left out in
/// the `default` project.
pub fn incus_image(kind: &str, project: &str, name: &str, block: bool) -> String {
    let volume = if project == "default" {
        name.to_string()
    } else {
        format!("{project}_{name}")
    };
    let suffix = if block { ".block" } else { "" };
    format!("{kind}_{volume}{suffix}")
}

/// Who holds `image` open, by address. Empty when no one does.
pub fn watchers(runner: &dyn Runner, image: &RbdImage) -> Result<Vec<String>, String> {
    image.names_ok()?;
    let out = runner.output(&image.status_argv())?;
    let status: Value =
        serde_json::from_str(&out).map_err(|_| "rbd status did not answer JSON".to_string())?;
    Ok(status
        .get("watchers")
        .and_then(Value::as_array)
        .map(|watchers| {
            watchers
                .iter()
                .map(|watcher| {
                    watcher
                        .get("address")
                        .and_then(Value::as_str)
                        .unwrap_or("an unnamed client")
                        .to_string()
                })
                .collect()
        })
        .unwrap_or_default())
}

/// The size of `image`, in bytes.
pub fn size(runner: &dyn Runner, image: &RbdImage) -> Result<u64, String> {
    image.names_ok()?;
    let out = runner.output(&image.info_argv())?;
    let info: Value =
        serde_json::from_str(&out).map_err(|_| "rbd info did not answer JSON".to_string())?;
    info.get("size")
        .and_then(Value::as_u64)
        .ok_or_else(|| "rbd info names no size".to_string())
}

/// The two commands of a copy: the reader on the source's cluster, then the writer on the target's it pipes into.
pub fn copy_argv(source: &RbdImage, target: &RbdImage) -> (Vec<String>, Vec<String>) {
    (
        source.argv(&["export-diff", &source.spec(), "-"]),
        target.argv(&["import-diff", "-", &target.spec()]),
    )
}

/// Copies every byte `source` holds into `target`, which exists and has the source's size.
pub fn copy(runner: &dyn Runner, source: &RbdImage, target: &RbdImage) -> Result<(), String> {
    source.names_ok()?;
    target.names_ok()?;
    let (reader, writer) = copy_argv(source, target);
    runner.pipe(&reader, &writer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder {
        calls: Mutex<Vec<Vec<String>>>,
        answer: String,
    }

    impl Runner for Recorder {
        fn output(&self, argv: &[String]) -> Result<String, String> {
            self.calls.lock().expect("calls").push(argv.to_vec());
            Ok(self.answer.clone())
        }
        fn pipe(&self, first: &[String], second: &[String]) -> Result<(), String> {
            let mut calls = self.calls.lock().expect("calls");
            calls.push(first.to_vec());
            calls.push(second.to_vec());
            Ok(())
        }
    }

    fn image(cluster: &str, client: &str, pool: &str) -> RbdImage {
        RbdImage {
            cluster: cluster.into(),
            client: client.into(),
            pool: pool.into(),
            image: incus_image("virtual-machine", "shop", "db", true),
        }
    }

    #[test]
    fn each_side_names_its_own_cluster_and_client() {
        let runner = Recorder::default();
        let from = image("east", "incus-east", "osd-east");
        let to = image("west", "incus-west", "osd-west");
        copy(&runner, &from, &to).expect("copied");
        let calls = runner.calls.lock().expect("calls").clone();
        assert_eq!(
            calls[0],
            [
                "rbd",
                "--cluster",
                "east",
                "--id",
                "incus-east",
                "--no-progress",
                "export-diff",
                "osd-east/virtual-machine_shop_db.block",
                "-"
            ]
        );
        assert_eq!(
            calls[1],
            [
                "rbd",
                "--cluster",
                "west",
                "--id",
                "incus-west",
                "--no-progress",
                "import-diff",
                "-",
                "osd-west/virtual-machine_shop_db.block"
            ]
        );
    }

    #[test]
    fn watchers_and_size_are_read_from_rbd_json() {
        let runner = Recorder {
            answer: r#"{"watchers":[{"address":"192.0.2.9:0/1"}],"size":2147483648}"#.into(),
            ..Recorder::default()
        };
        let from = image("east", "incus-east", "osd-east");
        assert_eq!(watchers(&runner, &from).expect("read"), ["192.0.2.9:0/1"]);
        assert_eq!(size(&runner, &from).expect("read"), 2_147_483_648);
        let calls = runner.calls.lock().expect("calls").clone();
        assert_eq!(calls[0][6..], ["status", "--format", "json", &from.spec()]);
        assert_eq!(calls[1][6], "info");
    }

    #[test]
    fn a_name_that_is_not_a_ceph_name_runs_nothing() {
        let runner = Recorder::default();
        let mut bad = image("east", "incus-east", "osd-east");
        bad.image = "--help".into();
        assert!(copy(&runner, &bad, &image("west", "w", "p")).is_err());
        bad.image = "a b".into();
        assert!(size(&runner, &bad).is_err());
        assert!(runner.calls.lock().expect("calls").is_empty());
    }

    #[test]
    fn incus_names_its_images_by_kind_project_and_name() {
        assert_eq!(
            incus_image("virtual-machine", "default", "db", true),
            "virtual-machine_db.block"
        );
        assert_eq!(
            incus_image("custom", "shop", "data", false),
            "custom_shop_data"
        );
        assert_eq!(
            incus_image("custom", "shop", "raw", true),
            "custom_shop_raw.block"
        );
    }
}
