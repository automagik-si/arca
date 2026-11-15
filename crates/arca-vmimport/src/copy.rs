//! The disk copy: who holds a source image, the size of the image Incus made, and the copy from one into the other.
//!
//! [`RbdCopy`] does it with the `rbd` command, run through a [`Runner`]: two processes and a pipe, never a shell. It reads
//! the source with its own Ceph client (read-only on the source pool) of the source's cluster, and writes the target with
//! the Ceph identity Incus uses for the target pool, of the target pool's cluster. The two may be one Ceph cluster or two:
//! `rbd --cluster <name>` reads `/etc/ceph/<name>.conf` and the client's keyring beside it, so each side reaches its own
//! cluster with its own key. The bytes stream from the reader to the writer through an OS pipe: no disk byte passes
//! through this process, and the node holds no more of a disk than the pipe and the two `rbd` processes' own buffers.
//! A failure names its side, the cluster and the client. [`FakeCopy`] keeps everything in memory, for tests.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::process::{Command, Stdio};
use std::sync::Mutex;

use serde_json::Value;

/// One source image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceImage {
    pub pool: String,
    pub image: String,
    pub size_mib: u64,
}

impl SourceImage {
    pub fn spec(&self) -> String {
        format!("{}/{}", self.pool, self.image)
    }
}

/// The Ceph image Incus made for a disk, with the identity that may write it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetImage {
    /// The Ceph cluster's name.
    pub cluster: String,
    /// The Ceph client Incus uses for the pool (its `ceph.user.name`).
    pub client: String,
    /// The OSD pool (the Incus pool's `ceph.osd.pool_name`).
    pub pool: String,
    pub image: String,
}

impl TargetImage {
    pub fn spec(&self) -> String {
        format!("{}/{}", self.pool, self.image)
    }
}

/// The disk copy an import runs.
pub trait DiskCopy: Send + Sync {
    /// The cluster the source pool is on, by the name its config file has on this node (`/etc/ceph/<name>.conf`). It
    /// may differ from the target pool's.
    fn cluster(&self) -> &str;
    /// Who holds the source image open, by address. Empty when no one does.
    fn watchers(&self, source: &SourceImage) -> Result<Vec<String>, String>;
    /// The size of the target image, in bytes.
    fn target_size(&self, target: &TargetImage) -> Result<u64, String>;
    /// Copies every byte the source holds into the target, which exists and has the source's size.
    fn copy(&self, source: &SourceImage, target: &TargetImage) -> Result<(), String>;
}

/// What runs a command for [`RbdCopy`].
pub trait Runner: Send + Sync {
    /// Runs `argv` and returns its standard output; a failure carries its standard error.
    fn output(&self, argv: &[String]) -> Result<String, String>;
    /// Runs `first` with its standard output piped into `second`, and waits for both. A failure says which side failed,
    /// or both: a writer that stops first also fails its reader, which can no longer write into the pipe.
    fn pipe(&self, first: &[String], second: &[String]) -> Result<(), PipeFailure>;
}

/// Why a [`Runner::pipe`] failed: the reader's error, the writer's, or both.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PipeFailure {
    pub reader: Option<String>,
    pub writer: Option<String>,
}

impl PipeFailure {
    /// The reader failed.
    pub fn reader(why: impl Into<String>) -> Self {
        Self {
            reader: Some(why.into()),
            writer: None,
        }
    }

    /// The writer failed.
    pub fn writer(why: impl Into<String>) -> Self {
        Self {
            reader: None,
            writer: Some(why.into()),
        }
    }
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

    fn pipe(&self, first: &[String], second: &[String]) -> Result<(), PipeFailure> {
        let (program, rest) = first
            .split_first()
            .ok_or_else(|| PipeFailure::reader("no command"))?;
        let (into, into_rest) = second
            .split_first()
            .ok_or_else(|| PipeFailure::writer("no command"))?;
        let mut reader = Command::new(program)
            .args(rest)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| PipeFailure::reader(format!("{program} did not start: {error}")))?;
        let stdout = reader
            .stdout
            .take()
            .ok_or_else(|| PipeFailure::reader("the reader has no output"))?;
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
                return Err(PipeFailure::writer(format!(
                    "{into} did not start: {error}"
                )));
            }
        };
        let mut reader_err = Vec::new();
        if let Some(mut stderr) = reader.stderr.take() {
            let _ = stderr.read_to_end(&mut reader_err);
        }
        let read = reader
            .wait()
            .map_err(|error| PipeFailure::reader(error.to_string()))?;
        let written = writer
            .wait_with_output()
            .map_err(|error| PipeFailure::writer(error.to_string()))?;
        let failure = PipeFailure {
            reader: (!read.success()).then(|| format!("{program} failed: {}", tail(&reader_err))),
            writer: (!written.status.success())
                .then(|| format!("{into} failed: {}", tail(&written.stderr))),
        };
        if failure == PipeFailure::default() {
            Ok(())
        } else {
            Err(failure)
        }
    }
}

/// The copy by `rbd`.
#[derive(Debug, Clone)]
pub struct RbdCopy<R: Runner> {
    cluster: String,
    client: String,
    runner: R,
}

impl<R: Runner> RbdCopy<R> {
    /// The copy reading the source pool as `client` (without the `client.` prefix) of `cluster`.
    pub fn new(cluster: &str, client: &str, runner: R) -> Self {
        Self {
            cluster: cluster.to_string(),
            client: client.to_string(),
            runner,
        }
    }

    /// What runs the commands.
    pub fn runner(&self) -> &R {
        &self.runner
    }

    fn rbd(&self, client: &str, cluster: &str, rest: &[&str]) -> Vec<String> {
        ["rbd", "--cluster", cluster, "--id", client, "--no-progress"]
            .iter()
            .chain(rest)
            .map(|part| (*part).to_string())
            .collect()
    }

    /// The command that lists who watches the source image.
    pub fn watchers_argv(&self, source: &SourceImage) -> Vec<String> {
        self.rbd(
            &self.client,
            &self.cluster,
            &["status", "--format", "json", &source.spec()],
        )
    }

    /// The command that reads the target image's size.
    pub fn info_argv(&self, target: &TargetImage) -> Vec<String> {
        self.rbd(
            &target.client,
            &target.cluster,
            &["info", "--format", "json", &target.spec()],
        )
    }

    /// The two commands of the copy: the reader, then the writer it pipes into.
    pub fn copy_argv(
        &self,
        source: &SourceImage,
        target: &TargetImage,
    ) -> (Vec<String>, Vec<String>) {
        (
            self.rbd(
                &self.client,
                &self.cluster,
                &["export-diff", &source.spec(), "-"],
            ),
            self.rbd(
                &target.client,
                &target.cluster,
                &["import-diff", "-", &target.spec()],
            ),
        )
    }
}

/// An error of one side of the copy, with that side named: `reading the Ceph cluster b as client.arca-import: …`.
fn side(doing: &str, cluster: &str, client: &str, why: &str) -> String {
    format!("{doing} the Ceph cluster {cluster} as client.{client}: {why}")
}

fn names_ok(names: &[&str]) -> Result<(), String> {
    match names.iter().find(|name| !crate::source::ceph_name_ok(name)) {
        Some(name) => Err(format!("{name:?} is not a Ceph name")),
        None => Ok(()),
    }
}

impl<R: Runner> DiskCopy for RbdCopy<R> {
    fn cluster(&self) -> &str {
        &self.cluster
    }

    fn watchers(&self, source: &SourceImage) -> Result<Vec<String>, String> {
        names_ok(&[&self.cluster, &self.client, &source.pool, &source.image])?;
        let out = self
            .runner
            .output(&self.watchers_argv(source))
            .map_err(|why| side("reading", &self.cluster, &self.client, &why))?;
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

    fn target_size(&self, target: &TargetImage) -> Result<u64, String> {
        names_ok(&[&target.cluster, &target.client, &target.pool, &target.image])?;
        let out = self
            .runner
            .output(&self.info_argv(target))
            .map_err(|why| side("reading", &target.cluster, &target.client, &why))?;
        let info: Value =
            serde_json::from_str(&out).map_err(|_| "rbd info did not answer JSON".to_string())?;
        info.get("size")
            .and_then(Value::as_u64)
            .ok_or_else(|| "rbd info names no size".to_string())
    }

    fn copy(&self, source: &SourceImage, target: &TargetImage) -> Result<(), String> {
        names_ok(&[
            &self.cluster,
            &self.client,
            &source.pool,
            &source.image,
            &target.cluster,
            &target.client,
            &target.pool,
            &target.image,
        ])?;
        let (reader, writer) = self.copy_argv(source, target);
        self.runner.pipe(&reader, &writer).map_err(|failure| {
            let mut words = Vec::new();
            if let Some(why) = &failure.reader {
                words.push(side("reading", &self.cluster, &self.client, why));
            }
            if let Some(why) = &failure.writer {
                words.push(side("writing", &target.cluster, &target.client, why));
            }
            words.join("; ")
        })
    }
}

/// A disk copy in memory: source images with their watchers, target images with their size and what was copied in.
/// Its source pool is on the cluster `ceph` unless [`FakeCopy::on_cluster`] names another.
#[derive(Debug)]
pub struct FakeCopy {
    cluster: String,
    state: Mutex<FakeCopyState>,
}

impl Default for FakeCopy {
    fn default() -> Self {
        Self::on_cluster("ceph")
    }
}

#[derive(Debug, Default)]
struct FakeCopyState {
    watchers: BTreeMap<String, Vec<String>>,
    targets: BTreeMap<String, u64>,
    copied: Vec<(String, String)>,
    fail_copy: Option<String>,
}

impl FakeCopy {
    /// A fake whose source pool is on the cluster `cluster`.
    pub fn on_cluster(cluster: &str) -> Self {
        Self {
            cluster: cluster.to_string(),
            state: Mutex::new(FakeCopyState::default()),
        }
    }

    /// Someone at `address` holds the source image `pool/image` open.
    pub fn watch(&self, spec: &str, address: &str) {
        self.lock()
            .watchers
            .entry(spec.to_string())
            .or_default()
            .push(address.to_string());
    }

    /// Incus made the target image `pool/image`, of `bytes` bytes.
    pub fn target(&self, spec: &str, bytes: u64) {
        self.lock().targets.insert(spec.to_string(), bytes);
    }

    /// The copy into the target `pool/image` fails.
    pub fn fail_copy_into(&self, spec: &str) {
        self.lock().fail_copy = Some(spec.to_string());
    }

    /// Every copy so far: the source spec and the target spec, in order.
    pub fn copied(&self) -> Vec<(String, String)> {
        self.lock().copied.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeCopyState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl DiskCopy for FakeCopy {
    fn cluster(&self) -> &str {
        &self.cluster
    }

    fn watchers(&self, source: &SourceImage) -> Result<Vec<String>, String> {
        Ok(self
            .lock()
            .watchers
            .get(&source.spec())
            .cloned()
            .unwrap_or_default())
    }

    fn target_size(&self, target: &TargetImage) -> Result<u64, String> {
        self.lock()
            .targets
            .get(&target.spec())
            .copied()
            .ok_or_else(|| {
                format!(
                    "rbd: error opening image {}: No such file or directory",
                    target.image
                )
            })
    }

    fn copy(&self, source: &SourceImage, target: &TargetImage) -> Result<(), String> {
        let mut state = self.lock();
        if state.fail_copy.as_deref() == Some(target.spec().as_str()) {
            return Err("rbd import-diff failed: the fake was told to fail".to_string());
        }
        state.copied.push((source.spec(), target.spec()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Recorder {
        calls: Mutex<Vec<Vec<String>>>,
        answer: String,
    }

    impl Runner for Recorder {
        fn output(&self, argv: &[String]) -> Result<String, String> {
            self.calls.lock().expect("calls").push(argv.to_vec());
            if self.answer == "output fails" {
                return Err("rbd failed: timed out".to_string());
            }
            Ok(self.answer.clone())
        }
        fn pipe(&self, first: &[String], second: &[String]) -> Result<(), PipeFailure> {
            let mut calls = self.calls.lock().expect("calls");
            calls.push(first.to_vec());
            calls.push(second.to_vec());
            match self.answer.as_str() {
                "reader fails" => Err(PipeFailure::reader("rbd failed: no keyring")),
                "both fail" => Err(PipeFailure {
                    reader: Some("rbd failed: Broken pipe".into()),
                    writer: Some("rbd failed: Permission denied".into()),
                }),
                _ => Ok(()),
            }
        }
    }

    fn source() -> SourceImage {
        SourceImage {
            pool: "src-pool".into(),
            image: "img-7-12-0".into(),
            size_mib: 2048,
        }
    }

    fn target() -> TargetImage {
        TargetImage {
            cluster: "ceph".into(),
            client: "incus".into(),
            pool: "incus-osd".into(),
            image: "virtual-machine_people_web.block".into(),
        }
    }

    #[test]
    fn the_copy_reads_as_the_source_client_and_writes_as_incus_own() {
        let copy = RbdCopy::new("ceph", "arca-import", Recorder::default());
        copy.copy(&source(), &target()).expect("copied");
        let calls = copy.runner.calls.lock().expect("calls").clone();
        assert_eq!(
            calls[0],
            [
                "rbd",
                "--cluster",
                "ceph",
                "--id",
                "arca-import",
                "--no-progress",
                "export-diff",
                "src-pool/img-7-12-0",
                "-"
            ]
        );
        assert_eq!(
            calls[1],
            [
                "rbd",
                "--cluster",
                "ceph",
                "--id",
                "incus",
                "--no-progress",
                "import-diff",
                "-",
                "incus-osd/virtual-machine_people_web.block"
            ]
        );
    }

    #[test]
    fn across_two_clusters_each_side_names_its_own_cluster_and_client() {
        let copy = RbdCopy::new("source-b", "arca-import", Recorder::default());
        copy.copy(&source(), &target()).expect("copied");
        copy.watchers(&source())
            .expect_err("no JSON from the recorder");
        let calls = copy.runner.calls.lock().expect("calls").clone();
        let (reader, writer, status) = (&calls[0], &calls[1], &calls[2]);
        assert_eq!(
            reader[1..5],
            ["--cluster", "source-b", "--id", "arca-import"]
        );
        assert_eq!(reader[6], "export-diff");
        assert_eq!(writer[1..5], ["--cluster", "ceph", "--id", "incus"]);
        assert_eq!(writer[6], "import-diff");
        assert_eq!(
            status[1..5],
            ["--cluster", "source-b", "--id", "arca-import"]
        );
        assert_eq!(status[6], "status");
    }

    #[test]
    fn a_failed_copy_names_the_side_its_cluster_and_its_client() {
        let reader = RbdCopy::new(
            "source-b",
            "arca-import",
            Recorder {
                answer: "reader fails".into(),
                ..Recorder::default()
            },
        );
        assert_eq!(
            reader.copy(&source(), &target()).expect_err("fails"),
            "reading the Ceph cluster source-b as client.arca-import: rbd failed: no keyring"
        );
        let both = RbdCopy::new(
            "source-b",
            "arca-import",
            Recorder {
                answer: "both fail".into(),
                ..Recorder::default()
            },
        );
        assert_eq!(
            both.copy(&source(), &target()).expect_err("fails"),
            "reading the Ceph cluster source-b as client.arca-import: rbd failed: Broken pipe; writing the Ceph cluster ceph as client.incus: rbd failed: Permission denied"
        );
        // A read that fails names its side too.
        let reads = RbdCopy::new(
            "source-b",
            "arca-import",
            Recorder {
                answer: "output fails".into(),
                ..Recorder::default()
            },
        );
        assert_eq!(
            reads.watchers(&source()).expect_err("fails"),
            "reading the Ceph cluster source-b as client.arca-import: rbd failed: timed out"
        );
        assert_eq!(
            reads.target_size(&target()).expect_err("fails"),
            "reading the Ceph cluster ceph as client.incus: rbd failed: timed out"
        );
    }

    #[test]
    fn watchers_and_size_are_read_from_rbd_json() {
        let copy = RbdCopy::new(
            "ceph",
            "arca-import",
            Recorder {
                answer: r#"{"watchers":[{"address":"192.0.2.9:0/1","client":4,"cookie":1}],"size":2147483648}"#.into(),
                ..Recorder::default()
            },
        );
        assert_eq!(copy.watchers(&source()).expect("read"), ["192.0.2.9:0/1"]);
        assert_eq!(copy.target_size(&target()).expect("read"), 2_147_483_648);
        let calls = copy.runner.calls.lock().expect("calls").clone();
        assert_eq!(
            calls[0],
            [
                "rbd",
                "--cluster",
                "ceph",
                "--id",
                "arca-import",
                "--no-progress",
                "status",
                "--format",
                "json",
                "src-pool/img-7-12-0"
            ]
        );
        assert_eq!(calls[1][4], "incus");
        assert_eq!(calls[1][6], "info");
    }

    #[test]
    fn a_name_that_is_not_a_ceph_name_runs_nothing() {
        let copy = RbdCopy::new("ceph", "arca-import", Recorder::default());
        let mut bad = source();
        bad.image = "--help".into();
        assert!(copy.copy(&bad, &target()).is_err());
        assert!(copy.runner.calls.lock().expect("calls").is_empty());
    }
}
