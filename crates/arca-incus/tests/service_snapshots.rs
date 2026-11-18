#![cfg(feature = "write")]
//! The snapshots of a service's instance with its custom volumes, against the fake Incus: each request, each refusal,
//! no stop and no start, and the removal of the parts taken when one part fails. Every value is synthetic.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use arca_incus::{
    FakeIncus, FakeState, IncusError, IncusRead, InstanceState, NotTaken, PartFailure,
    ProjectFamily, SnapshotPart, StateChanger, UnixIncus,
};
use serde_json::{Value, json};

const PROJECT: &str = "arcak-svc";
const TENANT: &str = "arcak-t-abcdefgh";
const ADOPTION: &str = "arcak-adopt-old";
const ELSEWHERE: &str = "elsewhere";
const MARK: &str = "user.arcakernel.entry";
static SEQ: AtomicU64 = AtomicU64::new(0);

fn disk(source: &str, path: &str) -> Value {
    json!({ "type": "disk", "pool": "pool1", "source": source, "path": path })
}

fn instance(project: &str, name: &str, status: &str, mark: Option<&str>, devices: Value) -> Value {
    let mut config = json!({ "volatile.uuid": format!("00000000-0000-4000-8000-{name:0>12}") });
    if let Some(mark) = mark {
        config[MARK] = json!(mark);
    }
    json!({
        "name": name, "project": project, "type": "container", "status": status,
        "profiles": [], "config": config, "devices": devices,
    })
}

fn project(name: &str, snapshots: bool) -> Value {
    let mut config = json!({ "restricted": "true" });
    if snapshots {
        config["restricted.snapshots"] = json!("allow");
    }
    json!({ "name": name, "config": config })
}

struct Rig {
    _dir: PathBuf,
    fake: FakeIncus,
    /// The changer a service's snapshot uses: the node's projects, the adoption project and a tenant's.
    changer: StateChanger,
    /// A changer for every project, which only reads the targets of the refusals.
    wide: StateChanger,
}

impl Rig {
    /// The service `web` runs with two volumes and a host folder, `cold` is stopped with one volume, and beside them
    /// instances that are not a service's of the node's own project. `snapshots` says whether the projects allow them.
    fn new(snapshots: bool) -> Rig {
        let dir = std::env::temp_dir().join(format!(
            "arca-service-snapshots-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir(&dir).expect("scratch");
        let web = json!({
            "root": { "type": "disk", "pool": "pool1", "path": "/" },
            "data": disk("web-data", "/data"), "logs": disk("web-logs", "/logs"),
            "host": { "type": "disk", "source": "/srv/web", "path": "/mnt" },
            "eth0": { "type": "nic", "network": "arcak-svcbr" },
        });
        let mut instances = vec![instance(
            PROJECT,
            "web",
            "Running",
            Some("service-web"),
            web,
        )];
        // Each with its mark (none when empty) and the one volume it mounts at /data (none when empty).
        for (project, name, mark, volume) in [
            (PROJECT, "cold", "service-cold", "cold-data"),
            (PROJECT, "mixed", "service-mixed", "db-data"),
            (PROJECT, "stray", "", ""),
            (PROJECT, "imported", "adopt-imported", ""),
            (ADOPTION, "old", "adopt-old", ""),
            (TENANT, "app", "service-app", "app-data"),
            (ELSEWHERE, "far", "service-far", ""),
        ] {
            let status = if matches!(name, "cold" | "mixed") {
                "Stopped"
            } else {
                "Running"
            };
            let devices = if volume.is_empty() {
                json!({})
            } else {
                json!({ "data": disk(volume, "/data") })
            };
            let mark = Some(mark).filter(|mark| !mark.is_empty());
            instances.push(instance(project, name, status, mark, devices));
        }
        let state = FakeState {
            server: json!({}),
            projects: [PROJECT, TENANT, ADOPTION, ELSEWHERE]
                .iter()
                .map(|name| project(name, snapshots))
                .collect(),
            instances,
            networks: vec![],
            pools: vec![("pool1".into(), json!({}))],
        };
        let fake = FakeIncus::start(&dir, state).expect("the fake serves");
        for (project, volume, mark) in [
            (PROJECT, "web-data", "service-web"),
            (PROJECT, "web-logs", "service-web"),
            (PROJECT, "cold-data", "service-cold"),
            (PROJECT, "db-data", "service-db"),
            (TENANT, "app-data", "service-app"),
        ] {
            let body = json!({ "config": { MARK: mark } });
            fake.add_volume(project, "pool1", volume, body);
        }
        let socket = fake.socket().to_path_buf();
        let family = ProjectFamily::new([PROJECT, ADOPTION], std::iter::empty::<&str>())
            .expect("family")
            .with_tenants([TENANT])
            .expect("tenant");
        let wide = ProjectFamily::new([PROJECT, ADOPTION, ELSEWHERE], std::iter::empty::<&str>())
            .expect("family")
            .with_tenants([TENANT])
            .expect("tenant");
        Rig {
            _dir: dir,
            changer: StateChanger::new(socket.clone(), family),
            wide: StateChanger::new(socket, wide),
            fake,
        }
    }

    fn seen(&self, project: &str, name: &str) -> InstanceState {
        self.wide.read(project, name).expect("the instance reads")
    }

    /// The writes the fake took, as `METHOD path`.
    fn writes(&self) -> Vec<String> {
        self.fake
            .writes()
            .iter()
            .map(|write| format!("{} {}", write.method, write.path))
            .collect()
    }

    fn get(&self, path: &str) -> Option<Value> {
        UnixIncus::new(self.fake.socket().to_path_buf())
            .get(path)
            .ok()
    }
}

fn volume(name: &str) -> SnapshotPart {
    SnapshotPart::Volume {
        pool: "pool1".into(),
        volume: name.into(),
    }
}

fn instance_snapshot(name: &str, snapshot: &str) -> String {
    format!("/1.0/instances/{name}/snapshots/{snapshot}?project={PROJECT}")
}

fn volume_snapshots(volume: &str) -> String {
    format!("/1.0/storage-pools/pool1/volumes/custom/{volume}/snapshots")
}

#[test]
fn a_running_and_a_stopped_instance_are_snapshotted_with_their_volumes_and_never_stopped_or_started()
 {
    for (name, status, volumes) in [
        ("web", "Running", vec!["web-data", "web-logs"]),
        ("cold", "Stopped", vec!["cold-data"]),
    ] {
        let rig = Rig::new(true);
        let seen = rig.seen(PROJECT, name);
        let taken = rig
            .changer
            .snapshot_with_volumes(&seen, "pre-app-update-1")
            .expect("taken");

        assert_eq!(taken.name, "pre-app-update-1");
        let parts: Vec<SnapshotPart> = taken.taken.iter().map(|t| t.part.clone()).collect();
        let mut wanted = vec![SnapshotPart::Instance];
        wanted.extend(volumes.iter().map(|v| volume(v)));
        assert_eq!(
            parts, wanted,
            "{name}: the instance first, then each volume"
        );
        assert!(taken.taken.iter().all(|t| t.at_ms > 0));
        let mut writes = vec![format!(
            "POST /1.0/instances/{name}/snapshots?project={PROJECT}"
        )];
        writes.extend(
            volumes
                .iter()
                .map(|v| format!("POST {}?project={PROJECT}", volume_snapshots(v))),
        );
        assert_eq!(
            rig.writes(),
            writes,
            "{name}: one POST per part, and no other write"
        );
        // The body of a volume's snapshot is its name alone; the fake keeps it as sent.
        for v in &volumes {
            assert_eq!(
                rig.get(&format!(
                    "{}/pre-app-update-1?project={PROJECT}",
                    volume_snapshots(v)
                )),
                Some(json!({ "name": "pre-app-update-1" }))
            );
        }
        assert!(
            rig.fake.state_actions(PROJECT, name).is_empty(),
            "no stop, no start"
        );
        assert_eq!(rig.seen(PROJECT, name).status, status);
    }
}

#[test]
fn a_disk_that_is_no_custom_volume_is_named_as_not_taken() {
    let rig = Rig::new(true);
    let taken = rig
        .changer
        .snapshot_with_volumes(&rig.seen(PROJECT, "web"), "pre-app-update-1")
        .expect("taken");
    assert_eq!(
        taken.not_taken,
        vec![NotTaken {
            device: "host".into(),
            why: "a path of the host is in no Incus snapshot",
        }]
    );
    let cold = rig
        .changer
        .snapshot_with_volumes(&rig.seen(PROJECT, "cold"), "pre-app-update-1")
        .expect("taken");
    assert!(cold.not_taken.is_empty());
}

#[test]
fn a_restore_puts_back_the_parts_it_names_and_only_those() {
    let rig = Rig::new(true);
    let seen = rig.seen(PROJECT, "cold");
    rig.changer
        .snapshot_with_volumes(&seen, "pre-app-update-1")
        .expect("taken");
    let before = rig.fake.writes().len();

    let done = rig
        .changer
        .restore_with_volumes(
            &seen,
            "pre-app-update-1",
            &[volume("cold-data"), SnapshotPart::Instance],
        )
        .expect("restored");
    assert_eq!(
        done,
        vec![SnapshotPart::Instance, volume("cold-data")],
        "the instance first"
    );
    assert_eq!(
        rig.writes()[before..],
        [
            format!("PUT /1.0/instances/cold?project={PROJECT}"),
            format!("PUT /1.0/storage-pools/pool1/volumes/custom/cold-data?project={PROJECT}"),
        ]
    );
    assert_eq!(rig.fake.restores(PROJECT, "cold"), ["pre-app-update-1"]);
    assert_eq!(
        rig.fake.volume_restores(PROJECT, "pool1", "cold-data"),
        ["pre-app-update-1"]
    );

    // The volume alone: the instance is not restored again.
    rig.changer
        .restore_volume(&seen, "pool1", "cold-data", "pre-app-update-1")
        .expect("the volume alone");
    assert_eq!(rig.fake.restores(PROJECT, "cold").len(), 1);
    assert_eq!(
        rig.fake
            .volume_restores(PROJECT, "pool1", "cold-data")
            .len(),
        2
    );
    assert!(rig.fake.state_actions(PROJECT, "cold").is_empty());
}

#[test]
fn a_running_instance_is_not_restored_and_nothing_is_written() {
    let rig = Rig::new(true);
    let seen = rig.seen(PROJECT, "web");
    rig.changer
        .snapshot_with_volumes(&seen, "pre-app-update-1")
        .expect("taken");
    let before = rig.fake.writes().len();
    let failure = rig
        .changer
        .restore_with_volumes(
            &seen,
            "pre-app-update-1",
            &[SnapshotPart::Instance, volume("web-data")],
        )
        .expect_err("running");
    assert_eq!(
        failure.error,
        IncusError::Refused("the instance is not stopped".into())
    );
    assert!(matches!(
        rig.changer
            .restore_volume(&seen, "pool1", "web-data", "pre-app-update-1"),
        Err(IncusError::Refused(_))
    ));
    assert_eq!(rig.fake.writes().len(), before);
}

#[test]
fn each_snapshot_is_removed_by_its_own_delete() {
    let rig = Rig::new(true);
    let seen = rig.seen(PROJECT, "web");
    rig.changer
        .snapshot_with_volumes(&seen, "pre-app-update-1")
        .expect("taken");
    let before = rig.fake.writes().len();
    rig.changer
        .remove_instance_snapshot(&seen, "pre-app-update-1")
        .expect("instance snapshot removed");
    rig.changer
        .remove_volume_snapshot(&seen, "pool1", "web-data", "pre-app-update-1")
        .expect("volume snapshot removed");
    assert_eq!(
        rig.writes()[before..],
        [
            format!("DELETE {}", instance_snapshot("web", "pre-app-update-1")),
            format!(
                "DELETE {}/pre-app-update-1?project={PROJECT}",
                volume_snapshots("web-data")
            ),
        ]
    );
    assert!(
        rig.get(&instance_snapshot("web", "pre-app-update-1"))
            .is_none()
    );

    assert!(rig.fake.state_actions(PROJECT, "web").is_empty());
}

#[test]
fn a_part_that_fails_has_the_parts_taken_removed_and_is_named() {
    let rig = Rig::new(true);
    let seen = rig.seen(PROJECT, "web");
    // A snapshot of that name on the last volume already: Incus refuses the second.
    rig.changer
        .snapshot_volume(&seen, "pool1", "web-logs", "pre-app-update-2")
        .expect("the earlier one");
    let before = rig.fake.writes().len();

    let failure = rig
        .changer
        .snapshot_with_volumes(&seen, "pre-app-update-2")
        .expect_err("the last volume fails");
    assert_eq!(failure.part, volume("web-logs"));
    assert!(matches!(
        failure.error,
        IncusError::Status { code: 409, .. }
    ));
    assert_eq!(failure.done, [volume("web-data"), SnapshotPart::Instance]);
    assert!(failure.left.is_empty());
    assert!(
        failure
            .to_string()
            .contains("the volume web-logs of the pool pool1 failed")
    );
    let writes = rig.writes();
    assert_eq!(
        writes[before + 3..],
        [
            format!(
                "DELETE {}/pre-app-update-2?project={PROJECT}",
                volume_snapshots("web-data")
            ),
            format!("DELETE {}", instance_snapshot("web", "pre-app-update-2")),
        ],
        "the two parts taken are removed, the last first"
    );
    assert!(
        rig.get(&instance_snapshot("web", "pre-app-update-2"))
            .is_none()
    );
    assert!(
        rig.get(&format!(
            "{}/pre-app-update-2?project={PROJECT}",
            volume_snapshots("web-logs")
        ))
        .is_some(),
        "the snapshot that was there before is not this call's to remove"
    );
    assert!(rig.fake.state_actions(PROJECT, "web").is_empty());
}

#[test]
fn a_removal_that_fails_too_is_named_beside_the_failing_part() {
    let rig = Rig::new(true);
    let seen = rig.seen(PROJECT, "web");
    rig.changer
        .snapshot_volume(&seen, "pool1", "web-logs", "pre-app-update-3")
        .expect("the earlier one");
    rig.fake.set_reject_delete("pre-app-update-3");
    let failure = rig
        .changer
        .snapshot_with_volumes(&seen, "pre-app-update-3")
        .expect_err("fails");
    assert_eq!(failure.part, volume("web-logs"));
    assert!(failure.done.is_empty());
    let left: Vec<SnapshotPart> = failure.left.iter().map(|(part, _)| part.clone()).collect();
    assert_eq!(left, [volume("web-data"), SnapshotPart::Instance]);
    assert!(failure.to_string().contains("the instance was not removed"));
}

#[test]
fn a_project_that_refuses_snapshots_refuses_the_first_part_and_nothing_is_left() {
    let rig = Rig::new(false);
    let seen = rig.seen(PROJECT, "web");
    let failure: Box<PartFailure> = rig
        .changer
        .snapshot_with_volumes(&seen, "pre-app-update-1")
        .expect_err("refused");
    assert_eq!(failure.part, SnapshotPart::Instance);
    assert!(
        matches!(&failure.error, IncusError::Status { message, .. } if message.contains("doesn't allow for snapshot creation"))
    );
    assert!(failure.done.is_empty() && failure.left.is_empty());
    // The fake refuses a volume's snapshot there as Incus does.
    assert!(matches!(
        rig.changer
            .snapshot_volume(&seen, "pool1", "web-data", "pre-app-update-1"),
        Err(IncusError::Status { code: 500, .. })
    ));
    assert!(
        rig.get(&format!(
            "{}/pre-app-update-1?project={PROJECT}",
            volume_snapshots("web-data")
        ))
        .is_none()
    );
}

#[test]
fn every_new_write_refuses_a_target_that_is_not_a_service_of_the_nodes_own_project_before_any_request()
 {
    let rig = Rig::new(true);
    let cases = [
        (
            ELSEWHERE,
            "far",
            "the project is outside the kernel's family",
        ),
        (TENANT, "app", "a tenant's instance is not snapshotted here"),
        (
            ADOPTION,
            "old",
            "an adopted instance is not snapshotted here",
        ),
        (
            PROJECT,
            "imported",
            "an adopted instance is not snapshotted here",
        ),
        (
            PROJECT,
            "stray",
            "the instance is not a service's that Arca made",
        ),
    ];
    for (project, name, why) in cases {
        let seen = rig.seen(project, name);
        let want = IncusError::Refused(why.into());
        let before = rig.fake.requests().len();
        let c = &rig.changer;
        let s = "pre-app-update-1";
        let answers: Vec<(&str, Result<(), IncusError>)> = vec![
            (
                "snapshot_volume",
                c.snapshot_volume(&seen, "pool1", "web-data", s),
            ),
            (
                "restore_volume",
                c.restore_volume(&seen, "pool1", "web-data", s),
            ),
            (
                "remove_instance_snapshot",
                c.remove_instance_snapshot(&seen, s),
            ),
            (
                "remove_volume_snapshot",
                c.remove_volume_snapshot(&seen, "pool1", "web-data", s),
            ),
            (
                "snapshot_with_volumes",
                c.snapshot_with_volumes(&seen, s)
                    .map(|_| ())
                    .map_err(|f| f.error),
            ),
            (
                "restore_with_volumes",
                c.restore_with_volumes(&seen, s, &[SnapshotPart::Instance])
                    .map(|_| ())
                    .map_err(|f| f.error),
            ),
        ];
        for (write, answer) in answers {
            assert_eq!(answer, Err(want.clone()), "{write} on {project}/{name}");
        }
        assert_eq!(
            rig.fake.requests().len(),
            before,
            "{project}/{name}: no request"
        );
    }
    assert!(rig.fake.writes().is_empty());
}

#[test]
fn a_volume_that_is_not_the_instances_own_is_refused_before_any_write() {
    let rig = Rig::new(true);
    let mixed = rig.seen(PROJECT, "mixed");
    let failure = rig
        .changer
        .snapshot_with_volumes(&mixed, "pre-app-update-1")
        .expect_err("another entry's volume");
    assert_eq!(failure.part, volume("db-data"));
    assert_eq!(
        failure.error,
        IncusError::Refused("the volume is not this service's own".into())
    );
    let web = rig.seen(PROJECT, "web");
    assert_eq!(
        rig.changer
            .snapshot_volume(&web, "pool1", "cold-data", "pre-app-update-1"),
        Err(IncusError::Refused(
            "the instance does not mount that volume".into()
        ))
    );
    assert!(rig.fake.writes().is_empty());
}

#[test]
fn an_instance_made_again_since_it_was_read_is_refused() {
    let rig = Rig::new(true);
    let mut seen = rig.seen(PROJECT, "web");
    seen.uuid = Some("00000000-0000-4000-8000-000000000999".into());
    assert_eq!(
        rig.changer
            .snapshot_with_volumes(&seen, "pre-app-update-1")
            .map(|_| ())
            .map_err(|f| f.error),
        Err(IncusError::Refused(
            "the instance was recreated since it was read".into()
        ))
    );
    assert_eq!(
        rig.changer
            .snapshot_volume(&rig.seen(PROJECT, "web"), "pool1", "web-data", "../x"),
        Err(IncusError::Refused("the snapshot name is not plain".into()))
    );
    assert!(rig.fake.writes().is_empty());
}
