//! Folder snapshots: an owner-approved copy of one host folder (such as `/home`) into a custom volume in
//! [`SNAPSHOT_PROJECT`] on a snapshot pool.
//!
//! The kernel runs as an unprivileged user and never copies bytes itself. Incus does the work, in four steps:
//!
//! 1. A custom volume named after the snapshot is made on the snapshot pool ([`folder_volume_body`]), sized at 1.2 times
//!    the folder's measured size (at least 1 GiB), with `security.shifted=true` so the files keep their real owners on
//!    disk, and with the usual `user.arcasnapshot.*` marks (source kind `folder`, the source path, the file system UUID it
//!    lives on, when it was made).
//! 2. A short-lived copy helper container is made in [`SNAPSHOT_PROJECT`] from a small cached image, with its root disk on
//!    a regular pool of the machine and never on the snapshot pool (an unprivileged container cannot walk a snapshot
//!    pool's folder, so it would not start),
//!    ([`HELPER_IMAGE_ALIAS`]) and started ([`helper_body`]): the host folder is a read-only disk at `/src` (`shift=true`,
//!    so owners are kept), the volume is at `/dst`, it has no network device and no profile, it never starts on its own,
//!    it is unprivileged, and it gets 2 CPUs, 1 GiB of memory and the read/write caps the hosts use for heavy work. It is
//!    read back before it starts, and one that is not exactly that is deleted.
//! 3. One command runs in it ([`COPY_SCRIPT`]): `tar` copies the folder preserving owners, modes, times, symlinks, hard
//!    links, ACLs and extended attributes; both sides are counted (paths, and bytes of regular files); and the manifest
//!    [`MANIFEST_NAME`] is written at the top of the volume.
//! 4. The helper is deleted on every exit path. The volume gets `user.arcasnapshot.verified=true` (with the counts) only when
//!    both sides counted the same; otherwise it keeps [`INCOMPLETE_KEY`] with the reason.
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
/// How long the helper image may take to fetch.
const IMAGE_WAIT: Duration = Duration::from_secs(30 * 60);
/// How long one helper step (create, start, stop, delete) or the volume create may take.
const STEP_WAIT: Duration = Duration::from_secs(10 * 60);
/// How long the copy itself is followed.
const COPY_WAIT: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// The reason a copy cut short by a restart is incomplete.
pub const RESTART_REASON: &str =
    "the copy stopped when Arca restarted; delete it and take the snapshot again";

/// The copy command, run by `bash` in the helper. `ARCA_SRC` and `ARCA_DST` default to `/src` and `/dst`;
/// `ARCA_SNAPSHOT` is the plain snapshot name, and `ARCA_SOURCE` and `ARCA_FS_UUID` are JSON strings. It prints the
/// manifest as its last line and exits 0, 20 when reading the folder failed (`tar` exit above 1; 1 only means a file
/// changed while it was read), or 21 when writing the copy failed. A top-level `lost+found` and the manifest are left out
/// of both counts.
pub const COPY_SCRIPT: &str = r#"set -u -o pipefail
src="${ARCA_SRC:-/src}"
dst="${ARCA_DST:-/dst}"
count() {
  find "$1" -mindepth 1 -xdev \( -path "$1/lost+found" -o -path "$1/.arca-manifest.json" \) -prune -o -printf '%y %s\n' \
    | awk 'BEGIN { n = 0; b = 0 } { n++ } $1 == "f" { b += $2 } END { printf "%.0f %.0f\n", n, b }'
}
started=$(date -u +%Y-%m-%dT%H:%M:%SZ)
tar -C "$src" --one-file-system --numeric-owner --acls --xattrs --xattrs-include='*' -cpf - . \
  | tar -C "$dst" --numeric-owner --same-owner --acls --xattrs --xattrs-include='*' -xpf -
codes=("${PIPESTATUS[@]}")
read_code=${codes[0]}
write_code=${codes[1]:-1}
set -- $(count "$src")
src_paths=${1:-0}
src_bytes=${2:-0}
set -- $(count "$dst")
dst_paths=${1:-0}
dst_bytes=${2:-0}
finished=$(date -u +%Y-%m-%dT%H:%M:%SZ)
manifest=$(printf '{"version":1,"snapshot":"%s","sourcePath":%s,"fsUuid":%s,"started":"%s","finished":"%s","source":{"paths":%s,"bytes":%s},"copy":{"paths":%s,"bytes":%s},"tar":{"read":%s,"write":%s}}' \
  "$ARCA_SNAPSHOT" "$ARCA_SOURCE" "$ARCA_FS_UUID" "$started" "$finished" \
  "$src_paths" "$src_bytes" "$dst_paths" "$dst_bytes" "$read_code" "$write_code")
printf '%s\n' "$manifest" > "$dst/.arca-manifest.json" && chmod 0644 "$dst/.arca-manifest.json"
printf '%s\n' "$manifest"
if [ "$read_code" -gt 1 ]; then exit 20; fi
if [ "$write_code" -ne 0 ]; then exit 21; fi
exit 0
"#;

/// One approved folder snapshot, with every fact the owner's digest bound.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// When the snapshot was made, in UTC.
    pub created: String,
}

impl FolderCopy {
    /// The volume's size: 1.2 times the measured size, at least 1 GiB.
    pub fn volume_size(&self) -> u64 {
        (self.size.saturating_mul(6) / 5).max(GIB)
    }
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
    /// Whether the read/write caps applied (Incus may refuse them on some pools).
    pub io_caps: bool,
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
    json!({
        "name": copy.name,
        "type": "custom",
        "content_type": "filesystem",
        "description": format!("Snapshot of the folder {}", copy.folder),
        "config": {
            "size": format!("{}B", copy.volume_size()),
            "security.shifted": "true",
            SOURCE_KIND_KEY: "folder",
            SOURCE_NAME_KEY: copy.folder,
            SOURCE_PATH_KEY: copy.folder,
            FS_UUID_KEY: copy.fs_uuid,
            CREATED_KEY: copy.created,
        },
    })
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

/// Reads a copy command's manifest line, the last line of its output.
pub fn manifest_counts(stdout: &str) -> Option<(Counts, Counts)> {
    let line = stdout.lines().rev().find(|line| !line.trim().is_empty())?;
    let manifest: Value = serde_json::from_str(line).ok()?;
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
    let mut outcome = FolderOutcome {
        source: counts.map(|(source, _)| source),
        copy: counts.map(|(_, copy)| copy),
        io_caps,
        ..FolderOutcome::default()
    };
    let reason = match (code, counts) {
        (20, _) => Some("reading the folder failed".to_string()),
        (21, _) => Some("writing the copy failed (is the snapshot storage full?)".to_string()),
        (0, Some((source, copy))) if source == copy => None,
        (0, Some((source, copy))) => Some(format!(
            "the counts differ: the folder has {} paths ({} bytes), the copy has {} paths ({} bytes); files may have changed during the copy",
            source.paths, source.bytes, copy.paths, copy.bytes
        )),
        (0, None) => Some("the copy's counts could not be read".to_string()),
        (other, _) => Some(format!("the copy command failed with exit code {other}")),
    };
    outcome.verified = reason.is_none();
    outcome.reason = reason;
    outcome
}

/// Whether Incus refused a helper because of its read/write caps, so it is made again without them.
fn caps_refused(error: &IncusError) -> bool {
    let text = error.to_string().to_ascii_lowercase();
    [
        "limits.read",
        "limits.write",
        "i/o limit",
        "io limit",
        "blkio",
        "io.max",
        "block i/o",
    ]
    .iter()
    .any(|needle| text.contains(needle))
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
                .exec_copy(copy)
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
        self.run_operation(
            "POST",
            &snapshot_path("/1.0/images"),
            Some(&helper_image_body()),
            IMAGE_WAIT,
        )?;
        Ok(())
    }

    /// Makes, checks and starts the helper, with the read/write caps when Incus takes them. Returns whether it has them.
    fn start_helper(&self, copy: &FolderCopy) -> Result<bool, String> {
        match self.try_helper(copy, true) {
            Ok(()) => Ok(true),
            Err(error) if caps_refused(&error) => {
                let _ = self.remove_helper(&copy.helper);
                self.try_helper(copy, false)
                    .map(|()| false)
                    .map_err(|error| format!("the copy helper did not start: {error}"))
            }
            Err(error) => Err(format!("the copy helper did not start: {error}")),
        }
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

    /// Runs [`COPY_SCRIPT`] in the helper and returns its exit code and output.
    fn exec_copy(&self, copy: &FolderCopy) -> Result<(i64, String), IncusError> {
        let quoted = |text: &str| serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into());
        let body = json!({
            "command": ["/bin/bash", "-c", COPY_SCRIPT],
            "environment": {
                "PATH": "/usr/sbin:/usr/bin:/sbin:/bin",
                "LC_ALL": "C",
                "ARCA_SNAPSHOT": copy.name,
                "ARCA_SOURCE": quoted(&copy.folder),
                "ARCA_FS_UUID": quoted(&copy.fs_uuid),
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

    /// Stops (forcibly) and deletes one copy helper. An instance that is not a copy helper is refused; one already gone
    /// is fine.
    pub fn remove_helper(&self, name: &str) -> Result<(), IncusError> {
        if !plain_name(name) {
            return Err(refused("the helper name is not plain"));
        }
        let path = snapshot_path(&format!("/1.0/instances/{name}"));
        let row = match self.get(&path) {
            Ok(row) => row,
            Err(IncusError::Status { code: 404, .. }) => return Ok(()),
            Err(error) => return Err(error),
        };
        let helper = row
            .pointer(&format!("/config/{HELPER_KEY}"))
            .and_then(Value::as_str)
            .is_some_and(|target| !target.is_empty());
        if !helper {
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
        for key in [VERIFIED_KEY, INCOMPLETE_KEY, PATHS_KEY, BYTES_KEY] {
            config.remove(key);
        }
        if outcome.verified {
            config.insert(VERIFIED_KEY.to_string(), json!("true"));
            if let Some(copy) = outcome.copy {
                config.insert(PATHS_KEY.to_string(), json!(copy.paths.to_string()));
                config.insert(BYTES_KEY.to_string(), json!(copy.bytes.to_string()));
            }
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
            if !plain_name(name) || live(target) {
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
            created: "2026-11-09T14:05:00.000Z".to_string(),
        }
    }

    #[test]
    fn the_volume_is_sized_with_room_and_carries_its_marks() {
        let mut copy = plan();
        let body = folder_volume_body(&copy);
        assert_eq!(body["config"]["size"], json!(format!("{}B", 12 * GIB)));
        assert_eq!(body["config"]["security.shifted"], json!("true"));
        assert_eq!(body["config"][SOURCE_KIND_KEY], json!("folder"));
        assert_eq!(body["config"][SOURCE_PATH_KEY], json!("/home"));
        assert_eq!(body["config"][FS_UUID_KEY], json!("uuid-home"));
        assert!(body.get("source").is_none(), "a new empty volume");
        copy.size = 10;
        assert_eq!(copy.volume_size(), GIB, "at least 1 GiB");
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

    /// The copy command itself, run on this host between two scratch folders: owners, modes, times, symlinks, hard links
    /// and extended attributes survive, and both sides count the same.
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
            .output()
            .expect("bash runs");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{stdout} {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let outcome = judge(0, &stdout, true);
        assert!(outcome.verified, "{outcome:?}");
        assert_eq!(outcome.source.map(|c| c.paths), Some(6));
        assert_eq!(outcome.source.map(|c| c.bytes), Some(5 + 4096 + 5));
        let meta = |path: &std::path::Path| std::fs::symlink_metadata(path).expect("meta");
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
        assert_eq!(manifest["copy"]["paths"], json!(6));
        let _ = std::fs::remove_dir_all(&root);
    }
}
