//! The folder snapshot against a REAL Incus. Ignored, and a no-op unless `ARCA_REAL_INCUS_PROOF=1`: it makes a folder
//! pool, the snapshot project, the helper image and one folder copy on the Incus it is pointed at, so it is only ever run
//! by hand. The copy's volume is deleted afterwards; the pool, the project and the cached helper image are left for the
//! person running it to remove.
//!
//! - `ARCA_PROOF_SOCKET`: the Incus socket (default `/var/lib/incus/unix.socket`).
//! - `ARCA_PROOF_FOLDER`: a scratch folder the snapshot pool is made in (Arca makes `<folder>/arca-pool`, as `storage_add`
//!   does). An existing snapshot pool of that name is reused.
//! - `ARCA_PROOF_POOL`: the snapshot pool's name (default `arcaproof`).
//! - `ARCA_PROOF_SOURCE`: a scratch folder that does not exist yet. The test fills it with files of several modes, a
//!   folder, a symlink, a hard link and an extended attribute, copies it, and removes it.
//!
//! It runs the same calls the approved `snapshot_create` makes after its checks: the volume, the helper (read-only folder,
//! no NIC), the copy, the counts and the helper's removal. The copy is then read on the host, through the pool's folder:
//! owners, modes, times, links and the attribute must match the source.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use arca::storage::{folder_snapshot_name, helper_name, make_pool_folder, measure_folder};
use arca_incus::folder::{FolderCopy, MANIFEST_NAME};
use arca_incus::{IncusError, PoolSource, SNAPSHOT_PROJECT, SnapshotWriter};
use serde_json::Value;

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

/// Every path under `root`, with what a restore needs to match.
fn listing(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)
            .expect("a readable folder")
            .flatten()
        {
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .expect("under the root")
                .display()
                .to_string();
            if relative == MANIFEST_NAME {
                continue;
            }
            let meta = std::fs::symlink_metadata(&path).expect("meta");
            let link = std::fs::read_link(&path)
                .map(|target| target.display().to_string())
                .unwrap_or_default();
            let mtime = if meta.is_dir() { 0 } else { meta.mtime() };
            out.push(format!(
                "{relative} uid={} gid={} mode={:o} size={} links={} mtime={mtime} -> {link}",
                meta.uid(),
                meta.gid(),
                meta.mode(),
                if meta.is_dir() { 0 } else { meta.len() },
                meta.nlink()
            ));
            if meta.is_dir() {
                stack.push(path);
            }
        }
    }
    out.sort();
    out
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

    let writer = SnapshotWriter::new(socket.into()).with_wait(Duration::from_secs(600));
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

    // The copy, read on the host through the pool's folder.
    let pool_row = writer
        .get(&format!("/1.0/storage-pools/{pool}"))
        .expect("the pool");
    let pool_dir = pool_row["config"]["source"]
        .as_str()
        .expect("a folder pool")
        .to_string();
    let copied = PathBuf::from(format!("{pool_dir}/custom/{SNAPSHOT_PROJECT}_{name}"));
    let manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(copied.join(MANIFEST_NAME)).expect("the manifest"),
    )
    .expect("json");
    println!("MANIFEST {manifest}");
    assert_eq!(manifest["sourcePath"], Value::from(source_text.clone()));
    let (want, got) = (listing(&source), listing(&copied));
    for line in &got {
        println!("COPIED {line}");
    }
    assert_eq!(got, want, "owners, modes, sizes, times and links match");
    if xattr {
        let read = std::process::Command::new("getfattr")
            .args(["--only-values", "-n", "user.arca.proof"])
            .arg(copied.join("run.sh"))
            .output()
            .expect("getfattr");
        assert_eq!(
            String::from_utf8_lossy(&read.stdout),
            "kept",
            "the attribute is kept"
        );
    }
    assert_eq!(
        std::fs::read(copied.join("docs/private/secret")).expect("the copy"),
        b"only me\n"
    );

    writer
        .delete_volume(&pool, &name)
        .expect("the copy is deleted");
    std::fs::remove_dir_all(&source).expect("the scratch source is removed");
    println!("DELETED {name}; the pool {pool}, the project and the helper image stay");
}
