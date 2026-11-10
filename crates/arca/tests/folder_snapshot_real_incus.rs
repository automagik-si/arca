//! The folder snapshot against a REAL Incus. Ignored, and a no-op unless `ARCA_REAL_INCUS_PROOF=1`: it makes a folder
//! pool, the snapshot project, the helper image and one folder copy on the Incus it is pointed at, so it is only ever run
//! by hand. The copy's volume is deleted afterwards; the pool, the project and the cached helper image are left for the
//! person running it to remove.
//!
//! - `ARCA_PROOF_SOCKET`: the Incus socket (default `/var/lib/incus/unix.socket`).
//! - `ARCA_PROOF_FOLDER`: a scratch folder the snapshot pool is made in (Arca makes `<folder>/arca-pool`, as `storage_add`
//!   does). An existing snapshot pool of that name is reused.
//! - `ARCA_PROOF_POOL`: the snapshot pool's name (default `arcaproof`).
//! - `ARCA_PROOF_ROOT_POOL`: the machine's regular pool the helper's root disk goes on (default: the default profile's
//!   root pool), as `incus.pools` names it for the kernel.
//! - `ARCA_PROOF_SOURCE`: a scratch folder that does not exist yet. The test fills it with files of several modes, a
//!   folder, a symlink, a hard link and an extended attribute, copies it, and removes it.
//!
//! It runs the same calls the approved `snapshot_create` makes after its checks: the volume, the helper (read-only folder,
//! no NIC), the copy, the counts and the helper's removal. The copy is then read back through a short-lived checker
//! container in the snapshot project (the folder read-only at `/src`, the copy's volume read-only at `/dst`, no NIC,
//! deleted afterwards), since the pool's files are root's and this test runs unprivileged: `find -printf '%M %U:%G %n
//! %T@ %p %l'` and each path's extended attributes (from a `tar --xattrs` header) must match on both sides.

use std::io::{Read as _, Write as _};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use arca::storage::{folder_snapshot_name, helper_name, make_pool_folder, measure_folder};
use arca_incus::folder::{FolderCopy, HELPER_IMAGE_ALIAS, HELPER_KEY, HELPER_POOL_KEY};
use arca_incus::{IncusError, IncusRead, PoolSource, SNAPSHOT_PROJECT, SnapshotWriter, UnixIncus};
use serde_json::{Value, json};

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn build_source(source: &Path) -> bool {
    std::fs::create_dir_all(source.join("docs/private")).expect("the source");
    std::fs::write(source.join("readme.txt"), b"hello from the proof\n").expect("a file");
    std::fs::write(source.join("docs/report.bin"), vec![42u8; 256 * 1024]).expect("a file");
    std::fs::write(source.join("docs/private/secret"), b"only me\n").expect("a file");
    std::fs::write(source.join("run.sh"), b"#!/bin/sh\necho hi\n").expect("a file");
    for (path, mode) in [
        ("readme.txt", 0o644),
        ("docs/report.bin", 0o640),
        ("docs/private/secret", 0o600),
        ("docs/private", 0o700),
        ("run.sh", 0o755),
        ("docs", 0o750),
    ] {
        std::fs::set_permissions(source.join(path), std::fs::Permissions::from_mode(mode))
            .expect("a mode");
    }
    std::os::unix::fs::symlink("docs/report.bin", source.join("report-link")).expect("a link");
    std::fs::hard_link(
        source.join("readme.txt"),
        source.join("docs/readme-again.txt"),
    )
    .expect("a hard link");
    let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
    std::fs::File::options()
        .write(true)
        .open(source.join("readme.txt"))
        .and_then(|file| file.set_modified(old))
        .expect("an old time");
    std::process::Command::new("setfattr")
        .args(["-n", "user.arca.proof", "-v", "kept"])
        .arg(source.join("run.sh"))
        .status()
        .is_ok_and(|status| status.success())
}

/// What the checker prints for one side: every path below it (the manifest left out) with its type and mode, owner,
/// link count (`-` for a folder, whose count depends on the file system), whole-second time, path and link target; then
/// each path's extended attributes with their values, read from a `tar --xattrs` header; then one file's bytes.
const CHECK_SCRIPT: &str = r#"list() {
  cd "$1" || exit 9
  find . -mindepth 1 ! -path ./.arca-manifest.json \( -type d -printf '%M %U:%G - %T@ %p\n' -o -printf '%M %U:%G %n %T@ %p %l\n' \) \
    | sed -E 's/ ([0-9]+)\.[0-9]+ / \1 /' | LC_ALL=C sort
  echo '--- xattrs'
  find . -mindepth 1 ! -path ./.arca-manifest.json | LC_ALL=C sort | while IFS= read -r f; do
    x=$(tar --format=posix --xattrs --xattrs-include='*' --no-recursion -cf - "$f" \
      | grep -ao 'SCHILY\.xattr\.[^=]*=[^[:cntrl:]]*' | LC_ALL=C sort | tr '\n' ';')
    if [ -n "$x" ]; then printf '%s %s\n' "$f" "$x"; fi
  done
  echo '--- secret'
  cat ./docs/private/secret
}
list /src
echo '=== copy'
list /dst
"#;

/// One write to Incus, sent by hand: the checker is test machinery, not part of Arca's narrow writer. The answer is
/// waited on when it is an operation, and a failed one panics with Incus's message.
fn incus(socket: &str, method: &str, path: &str, body: &Value) -> Value {
    let bytes = serde_json::to_vec(body).expect("a body");
    let mut stream = std::os::unix::net::UnixStream::connect(socket).expect("the Incus socket");
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: incus\r\nAccept: application/json\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        bytes.len()
    )
    .and_then(|()| stream.write_all(&bytes))
    .expect("the request is sent");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("the answer");
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("an HTTP answer");
    let head = String::from_utf8_lossy(&raw[..split]).to_ascii_lowercase();
    let mut body = raw[split + 4..].to_vec();
    if head.contains("transfer-encoding: chunked") {
        let (mut out, mut rest) = (Vec::new(), body.as_slice());
        while let Some(end) = rest.windows(2).position(|window| window == b"\r\n") {
            let size = usize::from_str_radix(String::from_utf8_lossy(&rest[..end]).trim(), 16)
                .unwrap_or(0);
            if size == 0 || rest.len() < end + 2 + size {
                break;
            }
            out.extend_from_slice(&rest[end + 2..end + 2 + size]);
            rest = &rest[(end + 4 + size).min(rest.len())..];
        }
        body = out;
    }
    let envelope: Value = serde_json::from_slice(&body).expect("a JSON answer");
    assert_ne!(
        envelope["type"],
        json!("error"),
        "{method} {path}: {envelope}"
    );
    if envelope["type"] != json!("async") {
        return envelope["metadata"].clone();
    }
    let id = envelope["operation"]
        .as_str()
        .and_then(|url| url.rsplit('/').next())
        .expect("an operation");
    let done = UnixIncus::new(PathBuf::from(socket))
        .with_timeout(Duration::from_secs(700))
        .get(&format!("/1.0/operations/{id}/wait?timeout=600"))
        .expect("the operation ends");
    assert_eq!(done["status_code"], json!(200), "{method} {path}: {done}");
    done
}

/// Deletes the checker however the test ends: it carries the copy-helper mark, so the writer's own removal takes it.
struct Checker<'a> {
    writer: &'a SnapshotWriter,
    name: String,
}

impl Drop for Checker<'_> {
    fn drop(&mut self) {
        match self.writer.remove_helper(&self.name) {
            Ok(()) => println!("CHECKER {} deleted", self.name),
            Err(error) => eprintln!("CHECKER {} NOT deleted: {error}", self.name),
        }
    }
}

/// Reads the source and the copy side by side in a short-lived checker container: the folder read-only at `/src`, the
/// copy's volume read-only at `/dst`, root on the regular pool, no NIC, no profile. Returns the two listings.
fn check_through_container(
    socket: &str,
    writer: &SnapshotWriter,
    copy: &FolderCopy,
) -> (String, String) {
    let checker = Checker {
        writer,
        name: format!("arca-check-{}", &copy.helper["arca-copy-".len()..]),
    };
    let project = format!("?project={SNAPSHOT_PROJECT}");
    incus(
        socket,
        "POST",
        &format!("/1.0/instances{project}"),
        &json!({
            "name": checker.name, "type": "container",
            "source": { "type": "image", "alias": HELPER_IMAGE_ALIAS },
            "profiles": [], "ephemeral": false,
            "config": {
                "boot.autostart": "false", "security.privileged": "false",
                "limits.cpu": "1", "limits.memory": "512MiB",
                HELPER_KEY: format!("check of {}", copy.name), HELPER_POOL_KEY: copy.pool,
            },
            "devices": {
                "root": { "type": "disk", "path": "/", "pool": copy.root_pool },
                "src": { "type": "disk", "source": copy.folder, "path": "/src", "readonly": "true", "shift": "true" },
                "dst": { "type": "disk", "pool": copy.pool, "source": copy.name, "path": "/dst", "readonly": "true" },
            },
        }),
    );
    let row = writer
        .get(&format!("/1.0/instances/{}{project}", checker.name))
        .expect("the checker");
    for (name, device) in row["expanded_devices"].as_object().expect("devices") {
        assert_ne!(
            device["type"],
            json!("nic"),
            "the checker has no NIC: {name}"
        );
    }
    incus(
        socket,
        "PUT",
        &format!("/1.0/instances/{}/state{project}", checker.name),
        &json!({ "action": "start" }),
    );
    let done = incus(
        socket,
        "POST",
        &format!("/1.0/instances/{}/exec{project}", checker.name),
        &json!({
            "command": ["/bin/bash", "-c", CHECK_SCRIPT],
            "environment": { "PATH": "/usr/sbin:/usr/bin:/sbin:/bin", "LC_ALL": "C" },
            "wait-for-websocket": false, "interactive": false, "record-output": true,
        }),
    );
    assert_eq!(
        done["metadata"]["return"],
        json!(0),
        "the check ran: {done}"
    );
    let stdout = done["metadata"]["output"]["1"]
        .as_str()
        .expect("its output");
    let text = UnixIncus::new(PathBuf::from(socket))
        .get_text(&format!("{stdout}{project}"))
        .expect("the checker's output");
    let (source, copied) = text.split_once("=== copy\n").expect("both sides");
    (source.to_string(), copied.to_string())
}

#[test]
#[ignore = "touches a real Incus; run by hand with ARCA_REAL_INCUS_PROOF=1"]
fn a_real_incus_folder_copy_keeps_owners_modes_and_links() {
    if env("ARCA_REAL_INCUS_PROOF").as_deref() != Some("1") {
        eprintln!("ARCA_REAL_INCUS_PROOF is not 1; nothing was done");
        return;
    }
    let socket = env("ARCA_PROOF_SOCKET").unwrap_or_else(|| "/var/lib/incus/unix.socket".into());
    let folder = env("ARCA_PROOF_FOLDER").expect("ARCA_PROOF_FOLDER");
    let pool = env("ARCA_PROOF_POOL").unwrap_or_else(|| "arcaproof".into());
    let source = PathBuf::from(env("ARCA_PROOF_SOURCE").expect("ARCA_PROOF_SOURCE"));
    assert!(
        std::fs::symlink_metadata(&source).is_err(),
        "ARCA_PROOF_SOURCE must not exist yet"
    );

    let writer = SnapshotWriter::new(socket.clone().into()).with_wait(Duration::from_secs(600));
    match writer.snapshot_pool(&pool) {
        Ok(_) => println!("REUSING POOL {pool}"),
        Err(IncusError::Status { code: 404, .. }) => {
            let _pool_folder = make_pool_folder(&folder).expect("the pool folder");
            writer
                .create_pool(
                    &pool,
                    &PoolSource::Folder {
                        path: folder.clone(),
                        fs_uuid: None,
                    },
                )
                .expect("the snapshot pool is added");
        }
        Err(error) => panic!("the pool: {error}"),
    }
    writer.ensure_project().expect("the snapshot project");
    let root_pool = env("ARCA_PROOF_ROOT_POOL").unwrap_or_else(|| {
        writer
            .get("/1.0/profiles/default?project=default")
            .expect("the default profile")
            .pointer("/devices/root/pool")
            .and_then(Value::as_str)
            .expect("the default profile's root pool; set ARCA_PROOF_ROOT_POOL")
            .to_string()
    });

    let xattr = build_source(&source);
    let source_text = source.to_str().expect("a UTF-8 path").to_string();
    let measured = measure_folder(&source_text);
    let now = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("a clock")
            .as_millis(),
    )
    .expect("a time");
    let name = folder_snapshot_name(&source_text, now).expect("a name");
    let copy = FolderCopy {
        helper: helper_name(&name),
        name: name.clone(),
        pool: pool.clone(),
        root_pool: root_pool.clone(),
        folder: source_text.clone(),
        fs_uuid: "proof".to_string(),
        size: measured.bytes,
        paths: measured.paths,
        created: "proof".to_string(),
    };
    println!("COPY {copy:?}");
    let started = std::time::Instant::now();
    writer.create_folder_volume(&copy).expect("the volume");
    let outcome = writer.run_folder_copy(&copy);
    println!("OUTCOME {outcome:?} in {:?}", started.elapsed());
    assert!(outcome.verified, "{outcome:?}");
    assert_eq!(
        outcome.source.map(|counts| (counts.paths, counts.bytes)),
        Some((measured.paths, measured.bytes)),
        "the helper counts what the kernel measured"
    );
    let helper = writer.get(&format!(
        "/1.0/instances/{}?project={SNAPSHOT_PROJECT}",
        copy.helper
    ));
    assert!(
        matches!(helper, Err(IncusError::Status { code: 404, .. })),
        "the helper is gone: {helper:?}"
    );
    let volume = writer
        .get(&format!(
            "/1.0/storage-pools/{pool}/volumes/custom/{name}?project={SNAPSHOT_PROJECT}"
        ))
        .expect("the volume");
    println!("VOLUME CONFIG {}", volume["config"]);
    assert_eq!(
        volume["config"]["user.arcasnapshot.verified"],
        Value::from("true")
    );

    // The copy, read back beside its source in a short-lived checker container.
    let (want, got) = check_through_container(&socket, &writer, &copy);
    println!("SOURCE\n{want}COPY\n{got}");
    assert_eq!(
        got, want,
        "owners, modes, link counts, times, links, attributes and bytes match"
    );
    let uid_gid = {
        use std::os::unix::fs::MetadataExt as _;
        let meta = std::fs::metadata(source.join("readme.txt")).expect("the source");
        format!("{}:{}", meta.uid(), meta.gid())
    };
    for expected in [
        format!("-rw------- {uid_gid} 1 "),
        format!("-rw-r----- {uid_gid} 1 "),
        format!("drwx------ {uid_gid} - "),
        format!("drwxr-x--- {uid_gid} - "),
        format!("-rwxr-xr-x {uid_gid} 1 "),
        format!("-rw-r--r-- {uid_gid} 2 1600000000 ./readme.txt"),
        "./report-link docs/report.bin".to_string(),
        "only me".to_string(),
    ] {
        assert!(got.contains(&expected), "the copy shows {expected:?}");
    }
    if xattr {
        assert!(
            got.contains("SCHILY.xattr.user.arca.proof=kept"),
            "the attribute is kept"
        );
    }

    writer
        .delete_volume(&pool, &name)
        .expect("the copy is deleted");
    std::fs::remove_dir_all(&source).expect("the scratch source is removed");
    println!("DELETED {name}; the pool {pool}, the project and the helper image stay");
}
