//! The snapshot copy against a REAL Incus. Ignored, and a no-op unless `ARCA_REAL_INCUS_PROOF=1`: it creates a folder
//! pool, the snapshot project and two copies on the Incus it is pointed at, so it is only ever run by hand, against a
//! scratch source project made for it, and everything it makes is removed afterwards (the copies here; the pool and the
//! project by the person running it).
//!
//! - `ARCA_PROOF_SOCKET`: the Incus socket (default `/var/lib/incus/unix.socket`).
//! - `ARCA_PROOF_PROJECT`: the scratch source project.
//! - `ARCA_PROOF_INSTANCE`: the source instance in it (default `src`): a stopped container with local devices (a proxy, a
//!   disk on a custom volume, a host folder) and live ownership marks.
//! - `ARCA_PROOF_VOLUME`: `<pool>/<volume>` of a custom volume in the project with a live mark.
//! - `ARCA_PROOF_FOLDER`: a folder the snapshot pool is made in: Arca makes `<folder>/arca-pool`, as `storage_add` does.
//! - `ARCA_PROOF_POOL`: the snapshot pool's name (default `arcaproof`).
//!
//! It runs the same calls the approved `snapshot_create` makes after its checks: the copy body, the copy, the clean-up and
//! the read-back.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use arca::storage::{make_pool_folder, snapshot_name};
use arca_incus::snapshot::{
    MARK_PREFIX, clean_parts, copy_problems, instance_copy_body, ownership_key, volume_copy_body,
    volume_problems,
};
use arca_incus::{PoolSource, Progress, SNAPSHOT_PROJECT, SnapshotWriter};
use serde_json::{Value, json};

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn shown(value: &Value, fields: &[&str]) -> Value {
    let mut out = serde_json::Map::new();
    for field in fields {
        out.insert(field.to_string(), value[*field].clone());
    }
    Value::Object(out)
}

#[test]
#[ignore = "touches a real Incus; run by hand with ARCA_REAL_INCUS_PROOF=1"]
fn a_real_incus_copy_is_clean_and_never_started() {
    if env("ARCA_REAL_INCUS_PROOF").as_deref() != Some("1") {
        eprintln!("ARCA_REAL_INCUS_PROOF is not 1; nothing was done");
        return;
    }
    let socket = env("ARCA_PROOF_SOCKET").unwrap_or_else(|| "/var/lib/incus/unix.socket".into());
    let project = env("ARCA_PROOF_PROJECT").expect("ARCA_PROOF_PROJECT");
    let instance = env("ARCA_PROOF_INSTANCE").unwrap_or_else(|| "src".into());
    let volume = env("ARCA_PROOF_VOLUME").expect("ARCA_PROOF_VOLUME");
    let (source_pool, volume) = volume.split_once('/').expect("<pool>/<volume>");
    let folder = env("ARCA_PROOF_FOLDER").expect("ARCA_PROOF_FOLDER");
    let pool = env("ARCA_PROOF_POOL").unwrap_or_else(|| "arcaproof".into());
    assert!(
        project.starts_with("arcak-scratch-"),
        "the source must be a scratch project"
    );

    let writer = SnapshotWriter::new(socket.into()).with_wait(Duration::from_secs(300));
    // As the approved storage_add does: Arca makes <folder>/arca-pool, then Incus makes the pool in it.
    make_pool_folder(&folder).expect("the pool folder");
    writer
        .create_pool(&pool, &PoolSource::Folder(folder))
        .expect("the snapshot pool is added");
    writer.ensure_project().expect("the snapshot project");
    let now = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("a clock")
            .as_millis(),
    )
    .expect("a time");
    let created = "proof";

    // The app.
    let source_path = format!("/1.0/instances/{instance}?project={project}");
    let source_before = writer.get(&source_path).expect("the source");
    println!(
        "SOURCE {}",
        serde_json::to_string_pretty(&shown(
            &source_before,
            &["name", "status", "profiles", "config", "devices"]
        ))
        .unwrap_or_default()
    );
    let networks = writer
        .get(&format!("/1.0/projects/{project}"))
        .expect("the project")
        .pointer("/config/features.networks")
        .and_then(Value::as_str)
        == Some("true");
    let name = snapshot_name(&project, &instance, now).expect("a name");
    let (body, dropped) =
        instance_copy_body(&source_before, &project, networks, &name, &pool, created)
            .expect("a body");
    println!("DROPPED {dropped:?}");
    let progress = writer.copy_instance(&pool, &body).expect("the copy");
    assert_eq!(progress, Progress::Done);
    let copy = writer
        .get(&format!("/1.0/instances/{name}?project={SNAPSHOT_PROJECT}"))
        .expect("the copy");
    println!(
        "COPY {}",
        serde_json::to_string_pretty(&shown(
            &copy,
            &[
                "name",
                "status",
                "profiles",
                "expanded_config",
                "expanded_devices"
            ]
        ))
        .unwrap_or_default()
    );
    let (_, devices) = clean_parts(&body);
    assert_eq!(copy_problems(&copy, &pool, &devices), Vec::<String>::new());
    assert_eq!(copy["status"], json!("Stopped"));
    assert_eq!(copy["expanded_config"]["boot.autostart"], json!("false"));
    let names: Vec<&String> = copy["expanded_devices"]
        .as_object()
        .expect("devices")
        .keys()
        .collect();
    for (device_name, device) in copy["expanded_devices"].as_object().expect("devices") {
        let kind = device["type"].as_str().unwrap_or_default();
        assert!(
            kind == "nic" || (kind == "disk" && device["path"] == json!("/")),
            "{device_name} is {device}"
        );
    }
    assert_eq!(copy["expanded_devices"]["root"]["pool"], json!(pool));
    for key in copy["expanded_config"].as_object().expect("config").keys() {
        assert!(!ownership_key(key), "the live mark {key} travelled");
    }
    let marks = copy["expanded_config"]
        .as_object()
        .expect("config")
        .keys()
        .filter(|key| key.starts_with(MARK_PREFIX))
        .count();
    println!("COPY DEVICES {names:?}, KEPT MARKS {marks}");
    let source_after = writer.get(&source_path).expect("the source");
    for field in ["status", "config", "devices", "profiles"] {
        assert_eq!(
            source_after[field], source_before[field],
            "the source's {field}"
        );
    }

    // The data volume.
    let volume_path =
        format!("/1.0/storage-pools/{source_pool}/volumes/custom/{volume}?project={project}");
    let volume_row = writer.get(&volume_path).expect("the source volume");
    let volume_name = snapshot_name(&project, volume, now).expect("a name");
    let volume_body = volume_copy_body(&volume_row, &project, source_pool, &volume_name, created)
        .expect("a body");
    assert_eq!(
        writer
            .copy_volume(&pool, &volume_body)
            .expect("the volume copy"),
        Progress::Done
    );
    let volume_copy = writer
        .get(&format!(
            "/1.0/storage-pools/{pool}/volumes/custom/{volume_name}?project={SNAPSHOT_PROJECT}"
        ))
        .expect("the volume copy");
    println!(
        "VOLUME COPY {}",
        serde_json::to_string_pretty(&shown(&volume_copy, &["name", "config"])).unwrap_or_default()
    );
    assert_eq!(volume_problems(&volume_copy), Vec::<String>::new());

    // Remove the copies through the same narrow writer.
    assert_eq!(writer.delete_instance(&name), Ok(Progress::Done));
    assert_eq!(
        writer.delete_volume(&pool, &volume_name),
        Ok(Progress::Done)
    );
    println!("DELETED {name} and {volume_name}");
}
