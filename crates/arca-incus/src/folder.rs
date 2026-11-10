//! Folder snapshots: an owner-approved copy of one host folder (such as `/home`) into a custom volume in
//! [`SNAPSHOT_PROJECT`] on a snapshot pool.
//!
//! The kernel runs as an unprivileged user and never copies bytes itself. Incus does the work, in four steps:
//!
//! 1. A custom volume named after the snapshot is made on the snapshot pool ([`folder_volume_body`]), sized at 1.2 times
//!    the folder's space estimate ([`space_estimate`]: the blocks its files take on disk plus 4 KiB per path; at least
//!    1 GiB), or with no size limit when some folders could not be measured, with `security.shifted=true` so the files keep
//!    their real owners on disk, and with the usual `user.arcasnapshot.*` marks (source kind `folder`, the source path,
//!    the file system UUID it lives on, when it was made).
//! 2. A short-lived copy helper container is made in [`SNAPSHOT_PROJECT`] from a small cached image, with its root disk on
//!    a regular pool of the machine and never on the snapshot pool (an unprivileged container cannot walk a snapshot
//!    pool's folder, so it would not start),
//!    ([`HELPER_IMAGE_ALIAS`]) and started ([`helper_body`]): the host folder is a read-only disk at `/src` (`shift=true`,
//!    so owners are kept), the volume is at `/dst`, it has no network device and no profile, it never starts on its own,
//!    it is unprivileged, and it gets 2 CPUs, 1 GiB of memory and the read/write caps the hosts use for heavy work (when
//!    Incus refuses the caps, `tar` throttles itself instead). It is read back before it starts, and one that is not
//!    exactly that is deleted.
//! 3. One command runs in it ([`COPY_SCRIPT`]): `tar` copies the folder preserving owners, modes, times, symlinks, hard
//!    links, sparse files, ACLs and extended attributes; sockets and device nodes are left out and counted as skipped;
//!    both sides are counted (paths, and bytes of regular files); and the manifest [`MANIFEST_NAME`] is written at the top
//!    of the volume.
//! 4. The helper is deleted on every exit path. The volume gets `user.arcasnapshot.verified=true` (with the counts) only when
//!    both sides counted the same; otherwise it keeps [`INCOMPLETE_KEY`] with the reason. Files that changed while `tar`
//!    read them are marked on a verified copy ([`CHANGED_KEY`]).
//!
//! A helper left behind by a process that stopped mid-copy is removed by [`SnapshotWriter::sweep_copy_helpers`], which
//! the kernel runs when it starts, and its volume is marked incomplete.

use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::read::IncusError;
use crate::snapshot::{
    CREATED_KEY, FS_UUID_KEY, SNAPSHOT_PROJECT, SOURCE_KIND_KEY, SOURCE_NAME_KEY, SnapshotWriter,
    VERIFIED_KEY, decode, is_snapshot_pool, plain_name, refused,
};

/// Every copy helper's name starts with this; an instance is a copy helper only with this name and [`HELPER_KEY`].
pub const HELPER_PREFIX: &str = "arca-copy-";
/// On a copy helper: the name of the folder snapshot it copies into. Only an instance carrying it is ever started,
/// stopped or given a command, and the snapshot list never shows it.
pub const HELPER_KEY: &str = "user.arcasnapshot.copy-helper-for";
/// On a copy helper: the snapshot pool its volume is on.
pub const HELPER_POOL_KEY: &str = "user.arcasnapshot.copy-helper-pool";
/// A folder snapshot's source folder, as the host resolved it.
pub const SOURCE_PATH_KEY: &str = "user.arcasnapshot.source-path";
/// Why a folder snapshot is incomplete, in plain words.
pub const INCOMPLETE_KEY: &str = "user.arcasnapshot.incomplete";
/// The number of paths a verified folder snapshot holds (the manifest left out).
pub const PATHS_KEY: &str = "user.arcasnapshot.paths";
/// The bytes of regular files a verified folder snapshot holds.
pub const BYTES_KEY: &str = "user.arcasnapshot.bytes";
/// On a verified folder snapshot: whether the copy ran with the read/write caps (`true` or `false`).
pub const IO_CAPS_KEY: &str = "user.arcasnapshot.io-caps";
/// On a verified folder snapshot: how many files changed while they were copied (absent when none did).
pub const CHANGED_KEY: &str = "user.arcasnapshot.changed-during-copy";
/// On a verified folder snapshot: the sockets left out of the copy (they cannot be copied, and apps make them again).
pub const SKIPPED_SOCKETS_KEY: &str = "user.arcasnapshot.skipped-sockets";
/// On a verified folder snapshot: the device nodes left out of the copy (an unprivileged helper cannot make them).
pub const SKIPPED_DEVICES_KEY: &str = "user.arcasnapshot.skipped-devices";
/// The space each path is allowed on top of its blocks in [`space_estimate`] (its inode and folder entry).
pub const PATH_ALLOWANCE: u64 = 4096;
/// The manifest at the top of a folder snapshot's volume.
pub const MANIFEST_NAME: &str = ".arca-manifest.json";
/// The local alias of the copy helper's image in [`SNAPSHOT_PROJECT`]. It is fetched once and kept.
pub const HELPER_IMAGE_ALIAS: &str = "arca-folder-copy";
/// Where the helper image comes from: Incus's default image server.
pub const HELPER_IMAGE_SERVER: &str = "https://images.linuxcontainers.org";
/// The helper image on that server. Debian rather than Alpine: BusyBox `tar` keeps no extended attributes or ACLs, and
/// GNU `tar`, `find` and `awk` are in the Debian image already, so the helper needs no network.
pub const HELPER_IMAGE_REMOTE_ALIAS: &str = "debian/13";
/// The helper's read cap on the volume, as the hosts use for heavy work.
pub const READ_CAP: &str = "150MB";
/// The helper's write cap on the volume.
pub const WRITE_CAP: &str = "80MB";
const HELPER_CPU: &str = "2";
const HELPER_MEMORY: &str = "1GiB";
const GIB: u64 = 1024 * 1024 * 1024;
/// `tar` records between one-second pauses when it throttles itself: 7800 records of 10240 bytes is about 80 MB, so the
/// copy reads at most about 80 MB a second, the hosts' write cap.
const THROTTLE_RECORDS: &str = "7800";
/// How long the helper image may take to fetch.
const IMAGE_WAIT: Duration = Duration::from_secs(30 * 60);
/// How long one helper step (create, start, stop, delete) or the volume create may take.
const STEP_WAIT: Duration = Duration::from_secs(10 * 60);
/// How long the copy itself is followed.
const COPY_WAIT: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// The reason a copy cut short by a restart is incomplete.
pub const RESTART_REASON: &str =
    "the copy stopped when Arca restarted; delete it and take the snapshot again";

/// The shell functions [`COPY_SCRIPT`] is built on, apart so a test can run them alone. `listed DIR [find actions]` walks
/// what is copied: every path on the folder's own file system below `DIR`, without a top-level `lost+found`, the manifest,
/// sockets and device nodes. `count DIR` prints its paths and the bytes of its regular files; `skipped DIR` prints the
/// sockets and device nodes left out.
macro_rules! copy_functions {
    () => {
        r#"listed() {
  find "$1" -mindepth 1 -xdev \( -path "$1/lost+found" -o -path "$1/.arca-manifest.json" \) -prune \
    -o ! -type s ! -type b ! -type c "${@:2}"
}
count() {
  listed "$1" -printf '%y %s\n' \
    | awk 'BEGIN { n = 0; b = 0 } { n++ } $1 == "f" { b += $2 } END { printf "%.0f %.0f\n", n, b }'
}
skipped() {
  find "$1" -mindepth 1 -xdev \( -path "$1/lost+found" -o -path "$1/.arca-manifest.json" \) -prune \
    -o -type s -printf 's\n' -o \( -type b -o -type c \) -printf 'd\n' \
    | awk 'BEGIN { s = 0; d = 0 } $1 == "s" { s++ } $1 == "d" { d++ } END { printf "%d %d\n", s, d }'
}
"#
    };
}

/// The copy command, run by `bash` in the helper. `ARCA_SRC` and `ARCA_DST` default to `/src` and `/dst`;
/// `ARCA_SNAPSHOT` is the plain snapshot name, `ARCA_SOURCE` and `ARCA_FS_UUID` are JSON strings, `ARCA_SEPARATE` is the
/// JSON of the separate file systems the kernel found inside the folder (recorded, not copied), and `ARCA_THROTTLE=1`
/// makes `tar` pause one second after about every 80 MB (when Incus refused the read/write caps).
///
/// `find` lists what is copied ([`copy_functions`]) and `tar` copies exactly that list (`--sparse`, owners, modes, times,
/// links, ACLs and extended attributes), so sockets and device nodes are left out of the copy and of both counts alike,
/// and counted as skipped. A file that changed or vanished while it was read is a change, not a failure: the count of
/// them and the first ten messages go in the manifest. The manifest is printed as the last line. Exit 0, 20 when reading
/// the folder failed, 21 when writing the copy failed, 22 when the command could not start.
pub const COPY_SCRIPT: &str = concat!(
    "set -u -o pipefail\nsrc=\"${ARCA_SRC:-/src}\"\ndst=\"${ARCA_DST:-/dst}\"\n",
    copy_functions!(),
    r#"changes='file changed as we read it|File removed before we read it|No such file or directory|File shrank by'
throttle=()
io=incus-caps
if [ "${ARCA_THROTTLE:-0}" = 1 ]; then
  throttle=(--checkpoint="${ARCA_THROTTLE_RECORDS:-7800}" --checkpoint-action=sleep=1)
  io=throttled-by-tar
fi
separate="${ARCA_SEPARATE:-}"
if [ -z "$separate" ]; then separate='{"count":0,"paths":[]}'; fi
errs=$(mktemp) || exit 22
started=$(date -u +%Y-%m-%dT%H:%M:%SZ)
set -- $(skipped "$src")
skipped_sockets=${1:-0}
skipped_devices=${2:-0}
{ printf '.\0'; listed "$src" -printf '%P\0'; } 2>>"$errs" \
  | tar -cf - -C "$src" --null --no-recursion --sparse --ignore-failed-read --numeric-owner --acls --xattrs \
      --xattrs-include='*' "${throttle[@]}" -T - 2>>"$errs" \
  | tar -C "$dst" --numeric-owner --same-owner --acls --xattrs --xattrs-include='*' -xpf -
codes=("${PIPESTATUS[@]}")
list_code=${codes[0]}
read_code=${codes[1]:-2}
write_code=${codes[2]:-1}
cat "$errs" >&2
changed=$(grep -cE "$changes" "$errs" || true)
changed_head=$(grep -E "$changes" "$errs" | head -n 10 | LC_ALL=C awk 'BEGIN { printf "[" }
  { sub(/^(tar|find): /, ""); gsub(/[^ -~]|["\\]/, "?"); printf "%s\"%s\"", (NR > 1 ? "," : ""), $0 }
  END { printf "]" }')
rm -f "$errs"
if [ "$read_code" -eq 1 ] && [ "${changed:-0}" -eq 0 ]; then changed=1; fi
read_failed=0
if [ "$read_code" -gt 1 ]; then read_failed=1; fi
if [ "$list_code" -ne 0 ] && [ "${changed:-0}" -eq 0 ]; then read_failed=1; fi
set -- $(count "$src")
src_paths=${1:-0}
src_bytes=${2:-0}
set -- $(count "$dst")
dst_paths=${1:-0}
dst_bytes=${2:-0}
finished=$(date -u +%Y-%m-%dT%H:%M:%SZ)
manifest=$(printf '{"version":2,"snapshot":"%s","sourcePath":%s,"fsUuid":%s,"started":"%s","finished":"%s","source":{"paths":%s,"bytes":%s},"copy":{"paths":%s,"bytes":%s},"skipped":{"sockets":%s,"devices":%s},"changed":{"files":%s,"head":%s},"separate":%s,"io":"%s","tar":{"list":%s,"read":%s,"write":%s}}' \
  "$ARCA_SNAPSHOT" "$ARCA_SOURCE" "$ARCA_FS_UUID" "$started" "$finished" \
  "$src_paths" "$src_bytes" "$dst_paths" "$dst_bytes" "$skipped_sockets" "$skipped_devices" \
  "${changed:-0}" "${changed_head:-[]}" "$separate" "$io" "$list_code" "$read_code" "$write_code")
printf '%s\n' "$manifest" > "$dst/.arca-manifest.json" && chmod 0644 "$dst/.arca-manifest.json"
printf '%s\n' "$manifest"
if [ "$read_failed" -eq 1 ]; then exit 20; fi
if [ "$write_code" -ne 0 ]; then exit 21; fi
exit 0
"#
);

/// One approved folder snapshot, with every fact the owner's digest bound.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FolderCopy {
    /// The snapshot's name: the volume's.
    pub name: String,
    /// The copy helper's instance name.
    pub helper: String,
    /// The snapshot pool: only the copy's volume is on it.
    pub pool: String,
    /// The regular (not snapshot) pool the helper's root disk is on.
    pub root_pool: String,
    /// The folder, as the host resolved it (absolute, no links).
    pub folder: String,
    /// The UUID of the file system the folder is on (empty when the host shows none).
    pub fs_uuid: String,
    /// The bytes of regular files measured when the proposal was made.
    pub size: u64,
    /// The paths measured when the proposal was made.
    pub paths: u64,
    /// The bytes the measured paths take on disk (their blocks), when the proposal was made.
    pub allocated: u64,
    /// The folders that could not be opened when the proposal was made; with any, the volume gets no size limit.
    pub unreadable: u64,
    /// The separate file systems inside the folder (mounts, btrfs subvolumes), at most [`SEPARATE_SHOWN`] of them: they
    /// are not copied, and the manifest records them.
    pub separate: Vec<String>,
    /// How many separate file systems there are in all.
    pub separate_count: u64,
    /// When the snapshot was made, in UTC.
    pub created: String,
}

/// How many separate file systems a folder snapshot names (the card shows the first ten).
pub const SEPARATE_SHOWN: usize = 100;

/// The space a folder's copy may take: the blocks its paths take on disk plus [`PATH_ALLOWANCE`] for each path. Sparse
/// files count only their written blocks, as `tar --sparse` writes them; a hard link counts once per name.
pub fn space_estimate(allocated: u64, paths: u64) -> u64 {
    allocated.saturating_add(paths.saturating_mul(PATH_ALLOWANCE))
}

impl FolderCopy {
    /// The volume's size limit: 1.2 times [`space_estimate`], at least 1 GiB; none when some folders could not be
    /// measured (the pool's free space still bounds it).
    pub fn volume_size(&self) -> Option<u64> {
        (self.unreadable == 0)
            .then(|| (space_estimate(self.allocated, self.paths).saturating_mul(6) / 5).max(GIB))
    }

    /// The separate file systems as the manifest records them.
    pub fn separate_json(&self) -> Value {
        json!({ "count": self.separate_count, "paths": self.separate })
    }
}

/// The note a verified copy carries when files changed while they were read.
pub fn changed_note(files: u64) -> String {
    let what = if files == 1 { "file" } else { "files" };
    format!(
        "{files} {what} changed while being copied; take another snapshot with the apps stopped"
    )
}

/// What both sides of a copy counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counts {
    /// Every path under the folder, the folder itself left out.
    pub paths: u64,
    /// The bytes of regular files.
    pub bytes: u64,
}

/// How a folder copy ended.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FolderOutcome {
    /// Both sides counted the same and the volume is marked verified.
    pub verified: bool,
    /// The folder's counts, when the copy command reported them.
    pub source: Option<Counts>,
    /// The copy's counts, when the copy command reported them.
    pub copy: Option<Counts>,
    /// Why the copy is incomplete, or what went wrong after it (the helper could not be deleted).
    pub reason: Option<String>,
    /// Whether the read/write caps applied (Incus may refuse them on some pools); without them `tar` throttled itself.
    pub io_caps: bool,
    /// How many files changed or vanished while they were read (the copy can still be verified).
    pub changed: u64,
    /// The first of those changes, as `tar` reported them.
    pub changed_head: Vec<String>,
    /// The sockets left out of the copy.
    pub skipped_sockets: u64,
    /// The device nodes left out of the copy.
    pub skipped_devices: u64,
}

impl FolderOutcome {
    fn failed(reason: String) -> FolderOutcome {
        FolderOutcome {
            reason: Some(reason),
            ..FolderOutcome::default()
        }
    }

    fn add_reason(&mut self, more: String) {
        self.reason = Some(match self.reason.take() {
            Some(reason) => format!("{reason}; {more}"),
            None => more,
        });
    }
}

/// The body that makes a folder snapshot's volume on its pool, in [`SNAPSHOT_PROJECT`].
pub fn folder_volume_body(copy: &FolderCopy) -> Value {
    let mut body = json!({
        "name": copy.name,
        "type": "custom",
        "content_type": "filesystem",
        "description": format!("Snapshot of the folder {}", copy.folder),
        "config": {
            "security.shifted": "true",
            SOURCE_KIND_KEY: "folder",
            SOURCE_NAME_KEY: copy.folder,
            SOURCE_PATH_KEY: copy.folder,
            FS_UUID_KEY: copy.fs_uuid,
            CREATED_KEY: copy.created,
        },
    });
    if let Some(size) = copy.volume_size() {
        body["config"]["size"] = json!(format!("{size}B"));
    }
    body
}

/// The body that makes the copy helper. `io_caps` adds the read/write caps on the volume device.
pub fn helper_body(copy: &FolderCopy, io_caps: bool) -> Value {
    let mut dst = json!({ "type": "disk", "pool": copy.pool, "source": copy.name, "path": "/dst" });
    if io_caps {
        dst["limits.read"] = json!(READ_CAP);
        dst["limits.write"] = json!(WRITE_CAP);
    }
    json!({
        "name": copy.helper,
        "type": "container",
        "description": format!("Arca copy helper for the folder snapshot {}; it is deleted when the copy ends", copy.name),
        "source": { "type": "image", "alias": HELPER_IMAGE_ALIAS },
        "profiles": [],
        "ephemeral": false,
        "config": {
            "boot.autostart": "false",
            "security.privileged": "false",
            "security.nesting": "false",
            "limits.cpu": HELPER_CPU,
            "limits.memory": HELPER_MEMORY,
            HELPER_KEY: copy.name,
            HELPER_POOL_KEY: copy.pool,
        },
        "devices": {
            "root": { "type": "disk", "path": "/", "pool": copy.root_pool },
            "src": { "type": "disk", "source": copy.folder, "path": "/src", "readonly": "true", "shift": "true" },
            "dst": dst,
        },
    })
}

/// The body that fetches the helper image into [`SNAPSHOT_PROJECT`] under [`HELPER_IMAGE_ALIAS`].
pub fn helper_image_body() -> Value {
    json!({
        "source": {
            "type": "image",
            "mode": "pull",
            "server": HELPER_IMAGE_SERVER,
            "protocol": "simplestreams",
            "alias": HELPER_IMAGE_REMOTE_ALIAS,
            "image_type": "container",
        },
        "aliases": [{ "name": HELPER_IMAGE_ALIAS, "description": "Arca folder snapshot copy helper" }],
        "auto_update": false,
        "public": false,
    })
}

/// What a helper, as Incus answers it, has that the approved one must not: a network device, a profile, a privileged or
/// self-starting config, a writable or unshifted source folder, another folder, or any device beyond root, `/src` and
/// `/dst`. An empty list is the helper [`helper_body`] asked for.
pub fn helper_problems(row: &Value, copy: &FolderCopy) -> Vec<String> {
    let mut problems = Vec::new();
    if row
        .get("profiles")
        .and_then(Value::as_array)
        .is_some_and(|profiles| !profiles.is_empty())
    {
        problems.push("the helper uses a profile".to_string());
    }
    let config = |key: &str| {
        row.get("expanded_config")
            .or_else(|| row.get("config"))
            .and_then(|config| config.get(key))
            .and_then(Value::as_str)
    };
    if config(HELPER_KEY) != Some(copy.name.as_str()) {
        problems.push("the helper does not name its snapshot".to_string());
    }
    if config("security.privileged") == Some("true") {
        problems.push("the helper is privileged".to_string());
    }
    if config("boot.autostart") != Some("false") {
        problems.push("the helper may start on its own".to_string());
    }
    let devices = row
        .get("expanded_devices")
        .filter(|devices| devices.as_object().is_some_and(|map| !map.is_empty()))
        .or_else(|| row.get("devices"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for (name, device) in &devices {
        let field = |key: &str| device.get(key).and_then(Value::as_str);
        match (name.as_str(), field("type")) {
            (_, Some("nic")) => problems.push(format!("the helper has the network device {name}")),
            ("root", Some("disk"))
                if field("path") == Some("/") && field("pool") == Some(copy.root_pool.as_str()) => {
            }
            ("src", Some("disk"))
                if field("source") == Some(copy.folder.as_str())
                    && field("path") == Some("/src")
                    && field("readonly") == Some("true")
                    && field("shift") == Some("true") => {}
            ("dst", Some("disk"))
                if field("pool") == Some(copy.pool.as_str())
                    && field("source") == Some(copy.name.as_str())
                    && field("path") == Some("/dst") => {}
            _ => problems.push(format!("the helper has the unexpected device {name}")),
        }
    }
    for needed in ["src", "dst"] {
        if !devices.contains_key(needed) {
            problems.push(format!("the helper has no {needed} device"));
        }
    }
    problems
}

/// Reads a copy command's manifest, the last line of its output.
fn manifest_line(stdout: &str) -> Option<Value> {
    let line = stdout.lines().rev().find(|line| !line.trim().is_empty())?;
    serde_json::from_str(line).ok()
}

/// Reads a copy command's counts from its manifest line: the folder's, then the copy's.
pub fn manifest_counts(stdout: &str) -> Option<(Counts, Counts)> {
    let manifest = manifest_line(stdout)?;
    let counts = |side: &str| {
        Some(Counts {
            paths: manifest.pointer(&format!("/{side}/paths"))?.as_u64()?,
            bytes: manifest.pointer(&format!("/{side}/bytes"))?.as_u64()?,
        })
    };
    Some((counts("source")?, counts("copy")?))
}

/// How a copy command's exit code and output decide the outcome.
pub fn judge(code: i64, stdout: &str, io_caps: bool) -> FolderOutcome {
    let counts = manifest_counts(stdout);
    let manifest = manifest_line(stdout).unwrap_or(Value::Null);
    let number = |pointer: &str| {
        manifest
            .pointer(pointer)
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    let mut outcome = FolderOutcome {
        source: counts.map(|(source, _)| source),
        copy: counts.map(|(_, copy)| copy),
        io_caps,
        changed: number("/changed/files"),
        changed_head: manifest
            .pointer("/changed/head")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .take(10)
            .map(|line| line.chars().take(300).collect())
            .collect(),
        skipped_sockets: number("/skipped/sockets"),
        skipped_devices: number("/skipped/devices"),
        ..FolderOutcome::default()
    };
    let reason = match (code, counts) {
        (20, _) => Some("reading the folder failed".to_string()),
        (21, _) => Some("writing the copy failed (is the snapshot storage full?)".to_string()),
        (22, _) => Some("the copy command could not start".to_string()),
        (0, Some((source, copy))) if source == copy => None,
        (0, Some((source, copy))) => Some(format!(
            "the counts differ: the folder has {} paths ({} bytes), the copy has {} paths ({} bytes); {}",
            source.paths,
            source.bytes,
            copy.paths,
            copy.bytes,
            if outcome.changed > 0 {
                changed_note(outcome.changed)
            } else {
                "files may have changed during the copy".to_string()
            }
        )),
        (0, None) => Some("the copy's counts could not be read".to_string()),
        (other, _) => Some(format!("the copy command failed with exit code {other}")),
    };
    outcome.verified = reason.is_none();
    outcome.reason = reason;
    outcome
}

/// Whether an instance, as Incus answers it, is a copy helper: its name starts with [`HELPER_PREFIX`] and it carries
/// [`HELPER_KEY`]. Arca stops, deletes or runs commands in no other instance.
pub fn is_copy_helper(name: &str, row: &Value) -> bool {
    name.starts_with(HELPER_PREFIX)
        && row
            .pointer(&format!("/config/{HELPER_KEY}"))
            .and_then(Value::as_str)
            .is_some_and(|target| !target.is_empty())
}

fn snapshot_path(path: &str) -> String {
    format!("{path}?project={SNAPSHOT_PROJECT}")
}

impl SnapshotWriter {
    /// Sends one write and waits up to `wait` for its operation, returning the finished operation as Incus answered it
    /// (its `metadata` for a synchronous answer). One still running when the wait ends is an error.
    fn run_operation(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        wait: Duration,
    ) -> Result<Value, IncusError> {
        let envelope = self.send(method, path, body)?;
        if envelope.get("type").and_then(Value::as_str) != Some("async") {
            return Ok(envelope.get("metadata").cloned().unwrap_or(Value::Null));
        }
        let operation = envelope
            .get("operation")
            .and_then(Value::as_str)
            .and_then(|url| url.rsplit('/').next())
            .filter(|id| plain_name(id))
            .ok_or_else(|| decode("the answer names no operation"))?
            .to_string();
        self.clone()
            .with_wait(wait)
            .wait_answer(&operation)?
            .ok_or_else(|| {
                decode(&format!(
                    "{method} {} did not finish within {} minutes",
                    path.split('?').next().unwrap_or(path),
                    wait.as_secs() / 60
                ))
            })
    }

    /// Makes a folder snapshot's volume on its snapshot pool.
    pub fn create_folder_volume(&self, copy: &FolderCopy) -> Result<(), IncusError> {
        self.snapshot_pool(&copy.pool)?;
        if !plain_name(&copy.name) || !plain_name(&copy.helper) {
            return Err(refused("the snapshot or helper name is not plain"));
        }
        let path = snapshot_path(&format!("/1.0/storage-pools/{}/volumes/custom", copy.pool));
        self.run_operation("POST", &path, Some(&folder_volume_body(copy)), STEP_WAIT)?;
        Ok(())
    }

    /// Runs a folder copy into its volume, which [`Self::create_folder_volume`] made: the helper image, the helper, the
    /// copy and the counts. The helper is deleted on every exit path, and the volume is then marked verified or
    /// incomplete. Nothing here returns early without that.
    pub fn run_folder_copy(&self, copy: &FolderCopy) -> FolderOutcome {
        let ran = (|| -> Result<FolderOutcome, String> {
            self.ensure_helper_image().map_err(|error| {
                format!("the copy helper's image could not be fetched: {error}")
            })?;
            let io_caps = self.start_helper(copy)?;
            let (code, stdout) = self
                .exec_copy(copy, io_caps)
                .map_err(|error| format!("the copy did not run to its end: {error}"))?;
            Ok(judge(code, &stdout, io_caps))
        })();
        let mut outcome = ran.unwrap_or_else(FolderOutcome::failed);
        if let Err(error) = self.remove_helper(&copy.helper) {
            outcome.add_reason(format!(
                "the copy helper {} could not be deleted ({error}); Arca removes it when it next starts",
                copy.helper
            ));
        }
        if let Err(error) = self.mark_folder_volume(&copy.pool, &copy.name, &outcome) {
            outcome.verified = false;
            outcome.add_reason(format!(
                "the snapshot's marks could not be written: {error}"
            ));
        }
        outcome
    }

    /// Fetches the helper image into [`SNAPSHOT_PROJECT`] unless it is there already.
    fn ensure_helper_image(&self) -> Result<(), IncusError> {
        match self.get(&snapshot_path(&format!(
            "/1.0/images/aliases/{HELPER_IMAGE_ALIAS}"
        ))) {
            Ok(_) => return Ok(()),
            Err(IncusError::Status { code: 404, .. }) => {}
            Err(error) => return Err(error),
        }
        match self.run_operation(
            "POST",
            &snapshot_path("/1.0/images"),
            Some(&helper_image_body()),
            IMAGE_WAIT,
        ) {
            Ok(_) => Ok(()),
            // Two first copies at once: the other fetch made the alias first ("alias already exists"). Whatever the
            // refusal, an alias that is there now is the image this copy needs.
            Err(error) => match self.get(&snapshot_path(&format!(
                "/1.0/images/aliases/{HELPER_IMAGE_ALIAS}"
            ))) {
                Ok(_) => Ok(()),
                Err(_) => Err(error),
            },
        }
    }

    /// Makes, checks and starts the helper, with the read/write caps when Incus takes them. Returns whether it has them.
    /// Incus's refusal of the caps does not name them (on a pool whose backing file system is not a block device it
    /// answered `Invalid block device "tmpfs"`), so any refusal of the create or the start with the caps on makes the
    /// helper once more without them, and `tar` then throttles itself. Only the writer's own refusals, made before
    /// anything is sent, are not retried.
    fn start_helper(&self, copy: &FolderCopy) -> Result<bool, String> {
        let with_caps = match self.try_helper(copy, true) {
            Ok(()) => return Ok(true),
            Err(error @ IncusError::Refused(_)) => {
                return Err(format!("the copy helper did not start: {error}"));
            }
            Err(error) => error,
        };
        eprintln!(
            "arca: the copy helper {} did not start with the read/write caps ({with_caps}); making it again without them, with tar throttling itself",
            copy.helper
        );
        if let Err(error) = self.remove_helper(&copy.helper) {
            return Err(format!(
                "the copy helper did not start ({with_caps}) and could not be removed before a retry: {error}"
            ));
        }
        self.try_helper(copy, false).map(|()| false).map_err(|error| {
            format!(
                "the copy helper did not start: {error} (with the read/write caps it answered: {with_caps})"
            )
        })
    }

    fn try_helper(&self, copy: &FolderCopy, io_caps: bool) -> Result<(), IncusError> {
        if !plain_name(&copy.root_pool) || copy.root_pool == copy.pool {
            return Err(refused("the helper's root disk must be on a regular pool"));
        }
        let root_pool = self.get(&format!("/1.0/storage-pools/{}", copy.root_pool))?;
        if is_snapshot_pool(&root_pool) {
            return Err(refused(
                "the helper's root disk must be on a regular pool, not on snapshot storage",
            ));
        }
        self.run_operation(
            "POST",
            &snapshot_path("/1.0/instances"),
            Some(&helper_body(copy, io_caps)),
            STEP_WAIT,
        )?;
        let path = snapshot_path(&format!("/1.0/instances/{}", copy.helper));
        let problems = helper_problems(&self.get(&path)?, copy);
        if !problems.is_empty() {
            return Err(refused(&format!(
                "the helper Incus made is not the approved one ({})",
                problems.join("; ")
            )));
        }
        self.run_operation(
            "PUT",
            &snapshot_path(&format!("/1.0/instances/{}/state", copy.helper)),
            Some(&json!({ "action": "start" })),
            STEP_WAIT,
        )?;
        Ok(())
    }

    /// Runs [`COPY_SCRIPT`] in the helper and returns its exit code and output. Without the read/write caps, `tar`
    /// throttles itself.
    fn exec_copy(&self, copy: &FolderCopy, io_caps: bool) -> Result<(i64, String), IncusError> {
        let quoted = |text: &str| serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into());
        let body = json!({
            "command": ["/bin/bash", "-c", COPY_SCRIPT],
            "environment": {
                "PATH": "/usr/sbin:/usr/bin:/sbin:/bin",
                "LC_ALL": "C",
                "ARCA_SNAPSHOT": copy.name,
                "ARCA_SOURCE": quoted(&copy.folder),
                "ARCA_FS_UUID": quoted(&copy.fs_uuid),
                "ARCA_SEPARATE": copy.separate_json().to_string(),
                "ARCA_THROTTLE": if io_caps { "0" } else { "1" },
                "ARCA_THROTTLE_RECORDS": THROTTLE_RECORDS,
            },
            "wait-for-websocket": false,
            "interactive": false,
            "record-output": true,
            "user": 0,
            "group": 0,
            "cwd": "/",
        });
        let answer = self.run_operation(
            "POST",
            &snapshot_path(&format!("/1.0/instances/{}/exec", copy.helper)),
            Some(&body),
            COPY_WAIT,
        )?;
        let code = answer
            .pointer("/metadata/return")
            .and_then(Value::as_i64)
            .ok_or_else(|| decode("the copy command reported no exit code"))?;
        let logs = format!("/1.0/instances/{}/logs/", copy.helper);
        let stdout = answer
            .pointer("/metadata/output/1")
            .and_then(Value::as_str)
            .filter(|path| {
                path.strip_prefix(&logs)
                    .is_some_and(|rest| !rest.is_empty() && rest.split('/').all(plain_name))
            })
            .map(|path| self.reader().get_text(&snapshot_path(path)))
            .transpose()?
            .unwrap_or_default();
        Ok((code, stdout))
    }

    /// Stops (forcibly) and deletes one copy helper. An instance that is not a copy helper ([`is_copy_helper`]) is
    /// refused; one already gone is fine.
    pub fn remove_helper(&self, name: &str) -> Result<(), IncusError> {
        if !plain_name(name) || !name.starts_with(HELPER_PREFIX) {
            return Err(refused("the instance is not a copy helper"));
        }
        let path = snapshot_path(&format!("/1.0/instances/{name}"));
        let row = match self.get(&path) {
            Ok(row) => row,
            Err(IncusError::Status { code: 404, .. }) => return Ok(()),
            Err(error) => return Err(error),
        };
        if !is_copy_helper(name, &row) {
            return Err(refused("the instance is not a copy helper"));
        }
        // A helper whose start failed can read back as stopped while Incus still holds it running, so it is always
        // stopped first; a stop of one already stopped is fine.
        self.force_stop(name)?;
        match self.run_operation("DELETE", &path, None, STEP_WAIT) {
            Ok(_) | Err(IncusError::Status { code: 404, .. }) => Ok(()),
            Err(error) if error.to_string().to_ascii_lowercase().contains("running") => {
                self.force_stop(name)?;
                match self.run_operation("DELETE", &path, None, STEP_WAIT) {
                    Ok(_) | Err(IncusError::Status { code: 404, .. }) => Ok(()),
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(error),
        }
    }

    /// Stops a helper forcibly and waits until it reads back as stopped (at most a minute).
    fn force_stop(&self, name: &str) -> Result<(), IncusError> {
        let state = snapshot_path(&format!("/1.0/instances/{name}/state"));
        match self.run_operation(
            "PUT",
            &state,
            Some(&json!({ "action": "stop", "force": true, "timeout": 30 })),
            STEP_WAIT,
        ) {
            Ok(_) | Err(IncusError::Status { code: 404, .. }) => {}
            Err(error)
                if error
                    .to_string()
                    .to_ascii_lowercase()
                    .contains("already stopped") => {}
            Err(error) => return Err(error),
        }
        for _ in 0..60 {
            match self.get(&state) {
                Ok(found) if found.get("status").and_then(Value::as_str) != Some("Stopped") => {
                    std::thread::sleep(Duration::from_secs(1));
                }
                Ok(_) | Err(IncusError::Status { code: 404, .. }) => return Ok(()),
                Err(error) => return Err(error),
            }
        }
        Err(decode("the copy helper did not stop within a minute"))
    }

    /// Writes a folder volume's outcome: [`VERIFIED_KEY`] and the counts when it is verified, [`INCOMPLETE_KEY`] with the
    /// reason otherwise. A volume that is not a folder snapshot is refused.
    pub fn mark_folder_volume(
        &self,
        pool: &str,
        name: &str,
        outcome: &FolderOutcome,
    ) -> Result<(), IncusError> {
        self.snapshot_pool(pool)?;
        if !plain_name(name) {
            return Err(refused("the snapshot name is not plain"));
        }
        let path = snapshot_path(&format!("/1.0/storage-pools/{pool}/volumes/custom/{name}"));
        let volume = self.get(&path)?;
        let mut config: Map<String, Value> = volume
            .get("config")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if config.get(SOURCE_KIND_KEY).and_then(Value::as_str) != Some("folder") {
            return Err(refused("the volume is not a folder snapshot"));
        }
        for key in [
            VERIFIED_KEY,
            INCOMPLETE_KEY,
            PATHS_KEY,
            BYTES_KEY,
            IO_CAPS_KEY,
            CHANGED_KEY,
            SKIPPED_SOCKETS_KEY,
            SKIPPED_DEVICES_KEY,
        ] {
            config.remove(key);
        }
        if outcome.verified {
            config.insert(VERIFIED_KEY.to_string(), json!("true"));
            config.insert(IO_CAPS_KEY.to_string(), json!(outcome.io_caps.to_string()));
            if let Some(copy) = outcome.copy {
                config.insert(PATHS_KEY.to_string(), json!(copy.paths.to_string()));
                config.insert(BYTES_KEY.to_string(), json!(copy.bytes.to_string()));
            }
            if outcome.changed > 0 {
                config.insert(CHANGED_KEY.to_string(), json!(outcome.changed.to_string()));
            }
            config.insert(
                SKIPPED_SOCKETS_KEY.to_string(),
                json!(outcome.skipped_sockets.to_string()),
            );
            config.insert(
                SKIPPED_DEVICES_KEY.to_string(),
                json!(outcome.skipped_devices.to_string()),
            );
        } else {
            let reason: String = outcome
                .reason
                .as_deref()
                .unwrap_or("the copy did not finish")
                .chars()
                .take(1024)
                .collect();
            config.insert(INCOMPLETE_KEY.to_string(), json!(reason));
        }
        let put = json!({
            "description": volume.get("description").cloned().unwrap_or(json!("")),
            "config": Value::Object(config),
        });
        self.run_operation("PUT", &path, Some(&put), STEP_WAIT)?;
        Ok(())
    }

    /// Removes every copy helper whose copy `live` does not name (on a fresh start, every one), and marks each folder
    /// volume a stopped copy left unfinished as incomplete ([`RESTART_REASON`]). Returns the helpers removed.
    pub fn sweep_copy_helpers(
        &self,
        live: &dyn Fn(&str) -> bool,
    ) -> Result<Vec<String>, IncusError> {
        match self.get(&format!("/1.0/projects/{SNAPSHOT_PROJECT}")) {
            Ok(_) => {}
            Err(IncusError::Status { code: 404, .. }) => return Ok(Vec::new()),
            Err(error) => return Err(error),
        }
        let mut removed = Vec::new();
        let mut failures = Vec::new();
        let rows = self.get(&format!(
            "/1.0/instances?project={SNAPSHOT_PROJECT}&recursion=1"
        ))?;
        for row in rows.as_array().into_iter().flatten() {
            let (Some(name), Some(target)) = (
                row.get("name").and_then(Value::as_str),
                row.pointer(&format!("/config/{HELPER_KEY}"))
                    .and_then(Value::as_str),
            ) else {
                continue;
            };
            if !plain_name(name) || !is_copy_helper(name, row) || live(target) {
                continue;
            }
            match self.remove_helper(name) {
                Ok(()) => removed.push(name.to_string()),
                Err(error) => failures.push(format!("{name}: {error}")),
            }
        }
        let pools = self.get("/1.0/storage-pools?recursion=1")?;
        let stopped = FolderOutcome::failed(RESTART_REASON.to_string());
        for pool in pools
            .as_array()
            .into_iter()
            .flatten()
            .filter(|pool| is_snapshot_pool(pool))
        {
            let Some(pool_name) = pool.get("name").and_then(Value::as_str) else {
                continue;
            };
            if !plain_name(pool_name) {
                continue;
            }
            let volumes = self.get(&format!(
                "/1.0/storage-pools/{pool_name}/volumes/custom?project={SNAPSHOT_PROJECT}&recursion=1"
            ))?;
            for volume in volumes.as_array().into_iter().flatten() {
                let config = |key: &str| {
                    volume
                        .pointer(&format!("/config/{key}"))
                        .and_then(Value::as_str)
                };
                let Some(name) = volume.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let unfinished = config(SOURCE_KIND_KEY) == Some("folder")
                    && config(VERIFIED_KEY) != Some("true")
                    && config(INCOMPLETE_KEY).is_none();
                if unfinished
                    && plain_name(name)
                    && !live(name)
                    && let Err(error) = self.mark_folder_volume(pool_name, name, &stopped)
                {
                    failures.push(format!("{name}: {error}"));
                }
            }
        }
        if failures.is_empty() {
            Ok(removed)
        } else {
            Err(decode(&format!(
                "the copy helper sweep left work undone: {}",
                failures.join("; ")
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> FolderCopy {
        FolderCopy {
            name: "folder--home--20261109T1405".to_string(),
            helper: "arca-copy-0123456789abcdef".to_string(),
            pool: "usb1".to_string(),
            root_pool: "default".to_string(),
            folder: "/home".to_string(),
            fs_uuid: "uuid-home".to_string(),
            size: 10 * GIB,
            paths: 1000,
            allocated: 10 * GIB,
            created: "2026-11-09T14:05:00.000Z".to_string(),
            ..FolderCopy::default()
        }
    }

    #[test]
    fn the_volume_is_sized_with_room_and_carries_its_marks() {
        let mut copy = plan();
        let body = folder_volume_body(&copy);
        let estimate = 10 * GIB + 1000 * PATH_ALLOWANCE;
        assert_eq!(space_estimate(copy.allocated, copy.paths), estimate);
        assert_eq!(
            body["config"]["size"],
            json!(format!("{}B", estimate * 6 / 5)),
            "1.2 times the blocks plus 4 KiB a path"
        );
        assert_eq!(body["config"]["security.shifted"], json!("true"));
        assert_eq!(body["config"][SOURCE_KIND_KEY], json!("folder"));
        assert_eq!(body["config"][SOURCE_PATH_KEY], json!("/home"));
        assert_eq!(body["config"][FS_UUID_KEY], json!("uuid-home"));
        assert!(body.get("source").is_none(), "a new empty volume");
        copy.allocated = 10;
        copy.paths = 1;
        assert_eq!(copy.volume_size(), Some(GIB), "at least 1 GiB");
        copy.unreadable = 2;
        assert_eq!(copy.volume_size(), None);
        assert!(
            folder_volume_body(&copy)["config"].get("size").is_none(),
            "no size limit when some folders were not measured"
        );
    }

    #[test]
    fn the_helper_is_read_only_offline_and_unprivileged() {
        let copy = plan();
        let body = helper_body(&copy, true);
        assert_eq!(body["profiles"], json!([]));
        assert!(body.get("start").is_none());
        assert_eq!(body["devices"]["src"]["readonly"], json!("true"));
        assert_eq!(body["devices"]["src"]["shift"], json!("true"));
        assert_eq!(body["devices"]["dst"]["limits.write"], json!(WRITE_CAP));
        assert!(
            helper_body(&copy, false)["devices"]["dst"]
                .get("limits.read")
                .is_none()
        );
        assert!(
            body["devices"]
                .as_object()
                .is_some_and(|devices| devices.values().all(|d| d["type"] != json!("nic")))
        );
        assert_eq!(
            body["devices"]["root"]["pool"],
            json!("default"),
            "never the snapshot pool"
        );
        let mut row = body.clone();
        row["expanded_devices"] = body["devices"].clone();
        assert!(helper_problems(&row, &copy).is_empty());
        let mut on_snapshots = row.clone();
        on_snapshots["expanded_devices"]["root"]["pool"] = json!("usb1");
        assert!(!helper_problems(&on_snapshots, &copy).is_empty());
        row["expanded_devices"]["eth0"] = json!({ "type": "nic", "network": "incusbr0" });
        row["expanded_devices"]["src"]["readonly"] = json!("false");
        let problems = helper_problems(&row, &copy);
        assert!(
            problems.iter().any(|p| p.contains("network device eth0")),
            "{problems:?}"
        );
        assert!(
            problems.iter().any(|p| p.contains("device src")),
            "{problems:?}"
        );
    }

    #[test]
    fn the_outcome_is_verified_only_when_both_sides_count_the_same() {
        let line = |source: (u64, u64), copy: (u64, u64)| {
            format!(
                "noise\n{{\"source\":{{\"paths\":{},\"bytes\":{}}},\"copy\":{{\"paths\":{},\"bytes\":{}}}}}\n",
                source.0, source.1, copy.0, copy.1
            )
        };
        assert!(judge(0, &line((3, 10), (3, 10)), true).verified);
        let differ = judge(0, &line((3, 10), (2, 10)), true);
        assert!(!differ.verified);
        assert!(
            differ
                .reason
                .as_deref()
                .is_some_and(|r| r.contains("counts differ"))
        );
        assert!(!judge(21, &line((3, 10), (3, 10)), true).verified);
        assert!(!judge(0, "not json", true).verified);
        assert!(!judge(7, "", true).verified);
        // Files that changed while tar read them leave the copy verified, with the count and the first messages.
        let changed = judge(
            0,
            r#"{"source":{"paths":3,"bytes":10},"copy":{"paths":3,"bytes":10},"changed":{"files":2,"head":["./a: file changed as we read it"]},"skipped":{"sockets":4,"devices":1}}"#,
            false,
        );
        assert!(changed.verified, "{changed:?}");
        assert_eq!(changed.changed, 2);
        assert_eq!(
            changed.changed_head,
            vec!["./a: file changed as we read it"]
        );
        assert_eq!((changed.skipped_sockets, changed.skipped_devices), (4, 1));
        let grew = judge(
            0,
            r#"{"source":{"paths":3,"bytes":12},"copy":{"paths":3,"bytes":10},"changed":{"files":1,"head":[]}}"#,
            true,
        );
        assert!(!grew.verified);
        assert!(
            grew.reason
                .as_deref()
                .is_some_and(|r| r.contains("1 file changed while being copied")),
            "{grew:?}"
        );
        assert_eq!(
            changed_note(2),
            "2 files changed while being copied; take another snapshot with the apps stopped"
        );
    }

    /// A helper whose root disk is on snapshot storage is refused before it is made, and the fake, like a real host,
    /// fails to start one; a failed start that leaves the helper held is still stopped and deleted.
    #[test]
    fn a_helper_never_has_its_root_on_snapshot_storage_and_a_failed_start_is_cleaned_up() {
        use crate::fake_storage::FakeStorageIncus;
        let dir = std::env::temp_dir().join(format!("arca-folder-fake-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a dir");
        let incus = FakeStorageIncus::start(&dir).expect("the fake");
        let marker = json!({ "user.arcasnapshot.storage": "usb1", "source": "/x" });
        incus.add_pool("usb1", "dir", "", marker, 0, 1 << 40);
        incus.add_pool("default", "dir", "", json!({ "source": "/y" }), 0, 1 << 40);
        incus.add_project(SNAPSHOT_PROJECT, json!({}));
        incus.add_image_alias(SNAPSHOT_PROJECT, HELPER_IMAGE_ALIAS);
        let writer =
            SnapshotWriter::new(incus.socket().to_path_buf()).with_wait(Duration::from_secs(1));
        let mut copy = plan();
        copy.root_pool = "usb1".to_string();
        writer.create_folder_volume(&copy).expect("the volume");
        let outcome = writer.run_folder_copy(&copy);
        assert!(!outcome.verified);
        assert!(
            outcome
                .reason
                .as_deref()
                .is_some_and(|r| r.contains("regular pool")),
            "{outcome:?}"
        );
        assert!(
            incus.instance(SNAPSHOT_PROJECT, &copy.helper).is_none(),
            "nothing was made"
        );

        // The fake's own refusal, reached without the writer's check.
        let body = helper_body(&copy, false);
        writer
            .run_operation(
                "POST",
                &snapshot_path("/1.0/instances"),
                Some(&body),
                STEP_WAIT,
            )
            .expect("made");
        let start = writer.run_operation(
            "PUT",
            &snapshot_path(&format!("/1.0/instances/{}/state", copy.helper)),
            Some(&json!({ "action": "start" })),
            STEP_WAIT,
        );
        assert!(
            start
                .as_ref()
                .is_err_and(|error| error.to_string().contains("Permission denied")),
            "{start:?}"
        );
        // It reads back as stopped, a bare delete is refused as running, and removal stops it first.
        let path = snapshot_path(&format!("/1.0/instances/{}", copy.helper));
        assert!(
            writer
                .run_operation("DELETE", &path, None, STEP_WAIT)
                .is_err()
        );
        assert_eq!(writer.remove_helper(&copy.helper), Ok(()));
        assert!(incus.instance(SNAPSHOT_PROJECT, &copy.helper).is_none());
        drop(incus);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// On a dir snapshot pool the fake, like the host, refuses the caps: the helper is made again without them and `tar`
    /// is told to throttle itself. A fetch of the image that loses a race to another fetch still finds its alias and goes
    /// on. Only an instance named like a copy helper and carrying its mark is ever removed.
    #[test]
    fn a_dir_pool_refuses_the_caps_and_tar_throttles_and_a_lost_image_race_is_fine() {
        use crate::fake_storage::FakeStorageIncus;
        let dir = std::env::temp_dir().join(format!("arca-folder-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a dir");
        let incus = FakeStorageIncus::start(&dir).expect("the fake");
        let marker = json!({ "user.arcasnapshot.storage": "usb1", "source": "/x" });
        incus.add_pool("usb1", "dir", "", marker, 0, 1 << 40);
        incus.add_pool(
            "default",
            "btrfs",
            "",
            json!({ "source": "/y" }),
            0,
            1 << 40,
        );
        incus.add_project(SNAPSHOT_PROJECT, json!({}));
        incus.race_next_image();
        incus.set_exec(
            0,
            r#"{"source":{"paths":3,"bytes":10},"copy":{"paths":3,"bytes":10}}"#,
        );
        let writer =
            SnapshotWriter::new(incus.socket().to_path_buf()).with_wait(Duration::from_secs(1));
        let copy = plan();
        writer.create_folder_volume(&copy).expect("the volume");
        let outcome = writer.run_folder_copy(&copy);
        assert!(outcome.verified, "{outcome:?}");
        assert!(!outcome.io_caps, "the dir pool refused the caps");
        let requests = incus.requests();
        let exec = requests
            .iter()
            .find(|request| request.target.contains("/exec"))
            .expect("the copy ran");
        assert_eq!(exec.body["environment"]["ARCA_THROTTLE"], json!("1"));
        assert_eq!(
            requests
                .iter()
                .filter(
                    |request| request.method == "POST" && request.target.starts_with("/1.0/images")
                )
                .count(),
            1,
            "fetched once; the lost race is not retried"
        );

        // An instance with the mark but without the name is not a copy helper.
        let mut body = helper_body(&copy, false);
        body["name"] = json!("someone-else");
        writer
            .run_operation(
                "POST",
                &snapshot_path("/1.0/instances"),
                Some(&body),
                STEP_WAIT,
            )
            .expect("made");
        assert!(matches!(
            writer.remove_helper("someone-else"),
            Err(IncusError::Refused(_))
        ));
        assert!(incus.instance(SNAPSHOT_PROJECT, "someone-else").is_some());
        let swept = writer.sweep_copy_helpers(&|_| false).expect("the sweep");
        assert!(swept.is_empty(), "{swept:?}");
        assert!(incus.instance(SNAPSHOT_PROJECT, "someone-else").is_some());
        drop(incus);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The copy command itself, run on this host between two scratch folders: owners, modes, times, symlinks, hard links,
    /// fifos, sparse files and extended attributes survive, a socket is left out of the copy and of both counts and is
    /// counted as skipped, and both sides count the same.
    #[test]
    fn the_copy_command_keeps_everything_and_counts_both_sides() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let gnu_tar = std::process::Command::new("tar")
            .arg("--version")
            .output()
            .map(|out| String::from_utf8_lossy(&out.stdout).contains("GNU tar"))
            .unwrap_or(false);
        if !gnu_tar {
            eprintln!("GNU tar is not on this host; the copy command was not run");
            return;
        }
        let root = std::env::temp_dir().join(format!("arca-folder-copy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (src, dst) = (root.join("src"), root.join("dst"));
        std::fs::create_dir_all(src.join("sub/deep")).expect("src");
        std::fs::create_dir_all(&dst).expect("dst");
        std::fs::write(src.join("a.txt"), b"hello").expect("a file");
        std::fs::write(src.join("sub/deep/b.bin"), vec![7u8; 4096]).expect("a file");
        std::fs::set_permissions(src.join("a.txt"), std::fs::Permissions::from_mode(0o640))
            .expect("mode");
        std::fs::hard_link(src.join("a.txt"), src.join("sub/a-link")).expect("a hard link");
        std::os::unix::fs::symlink("../a.txt", src.join("sub/a-sym")).expect("a symlink");
        let _socket =
            std::os::unix::net::UnixListener::bind(src.join("sub/s.sock")).expect("a socket");
        let fifo = std::process::Command::new("mkfifo")
            .arg(src.join("sub/pipe"))
            .status()
            .is_ok_and(|status| status.success());
        std::fs::File::create(src.join("holes"))
            .and_then(|file| file.set_len(64 << 20))
            .expect("a sparse file");
        let xattr = std::process::Command::new("setfattr")
            .args(["-n", "user.arca", "-v", "kept"])
            .arg(src.join("a.txt"))
            .status()
            .is_ok_and(|status| status.success());
        let out = std::process::Command::new("bash")
            .args(["-c", COPY_SCRIPT])
            .env("ARCA_SRC", &src)
            .env("ARCA_DST", &dst)
            .env("ARCA_SNAPSHOT", "folder--x--20261109T1405")
            .env("ARCA_SOURCE", "\"/x\"")
            .env("ARCA_FS_UUID", "\"u\"")
            .env("ARCA_SEPARATE", r#"{"count":1,"paths":["/x/mnt"]}"#)
            .env("ARCA_THROTTLE", "1")
            .output()
            .expect("bash runs");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{stdout} {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let meta = |path: &std::path::Path| std::fs::symlink_metadata(path).expect("meta");
        let outcome = judge(0, &stdout, false);
        assert!(outcome.verified, "{outcome:?}");
        let paths = 7 + u64::from(fifo);
        assert_eq!(
            outcome.source.map(|c| c.paths),
            Some(paths),
            "the socket is not counted"
        );
        assert_eq!(
            outcome.source.map(|c| c.bytes),
            Some(5 + 4096 + 5 + (64 << 20))
        );
        assert_eq!((outcome.skipped_sockets, outcome.skipped_devices), (1, 0));
        assert_eq!(outcome.changed, 0);
        assert!(
            std::fs::symlink_metadata(dst.join("sub/s.sock")).is_err(),
            "no socket in the copy"
        );
        if fifo {
            use std::os::unix::fs::FileTypeExt;
            assert!(
                meta(&dst.join("sub/pipe")).file_type().is_fifo(),
                "the fifo is kept"
            );
        }
        assert!(
            meta(&dst.join("holes")).blocks() * 512 < 1 << 20,
            "the sparse file stays sparse"
        );
        let (a, a2) = (meta(&src.join("a.txt")), meta(&dst.join("a.txt")));
        assert_eq!(a2.mode() & 0o7777, 0o640);
        assert_eq!((a2.uid(), a2.gid()), (a.uid(), a.gid()));
        assert_eq!(a2.mtime(), a.mtime());
        assert_eq!(
            meta(&dst.join("sub/a-link")).ino(),
            a2.ino(),
            "the hard link is kept"
        );
        assert_eq!(
            std::fs::read_link(dst.join("sub/a-sym")).expect("a link"),
            std::path::PathBuf::from("../a.txt")
        );
        if xattr {
            let got = std::process::Command::new("getfattr")
                .args(["--only-values", "-n", "user.arca"])
                .arg(dst.join("a.txt"))
                .output()
                .expect("getfattr");
            assert_eq!(String::from_utf8_lossy(&got.stdout), "kept");
        }
        let manifest: Value = serde_json::from_str(
            &std::fs::read_to_string(dst.join(MANIFEST_NAME)).expect("the manifest"),
        )
        .expect("json");
        assert_eq!(manifest["sourcePath"], json!("/x"));
        assert_eq!(manifest["copy"]["paths"], json!(paths));
        assert_eq!(manifest["skipped"], json!({ "sockets": 1, "devices": 0 }));
        assert_eq!(manifest["separate"]["paths"], json!(["/x/mnt"]));
        assert_eq!(manifest["io"], json!("throttled-by-tar"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Device nodes cannot be made here without privilege, so the walk is checked on the host's own `/dev`: they are left
    /// out of what is copied and counted, and counted as skipped.
    #[test]
    fn device_nodes_are_left_out_of_the_copy_and_counted_as_skipped() {
        use std::os::unix::fs::FileTypeExt;
        let null = std::fs::symlink_metadata("/dev/null");
        if !null.is_ok_and(|meta| meta.file_type().is_char_device()) {
            eprintln!("/dev/null is not a device node here; nothing to check");
            return;
        }
        let script = concat!(
            copy_functions!(),
            "skipped /dev\necho ---\nlisted /dev -printf '%y\\n' | sort -u\n"
        );
        let out = std::process::Command::new("bash")
            .args(["-c", script])
            .output()
            .expect("bash runs");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let (skipped, kinds) = stdout.split_once("---\n").expect("both parts");
        let devices: u64 = skipped
            .split_whitespace()
            .nth(1)
            .and_then(|n| n.parse().ok())
            .expect("a device count");
        assert!(devices >= 1, "{stdout}");
        assert!(
            kinds.lines().all(|kind| !matches!(kind, "b" | "c" | "s")),
            "no device node or socket is listed: {kinds}"
        );
    }
}
