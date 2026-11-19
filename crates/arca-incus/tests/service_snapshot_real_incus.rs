#![cfg(feature = "write")]
//! A service's snapshot with its volumes against a REAL Incus. Ignored, and a no-op unless `ARCA_REAL_INCUS_PROOF=1`: it
//! writes into, stops, starts, rebuilds and restores the instance it is pointed at, so it is only ever run by hand, on a
//! test node, against a container made for it. It removes the snapshots it took; the container is the person's to remove.
//!
//! - `ARCA_PROOF_SOCKET`: the Incus socket (default `/var/lib/incus/unix.socket`). The `incus` command on the same node
//!   writes and reads the files inside the container.
//! - `ARCA_PROOF_INSTANCE`: a container in the service project `arcak-svc`, made for the proof, whose project holds
//!   snapshots (`restricted.snapshots=allow`). Its config carries `user.arcakernel.entry=service-<name>`, it has `/bin/sh`,
//!   and it mounts one custom volume at `/data` whose config carries the same mark.
//! - `ARCA_PROOF_IMAGE`: `<alias>@<fingerprint>` of another image in the project's own image store, for the rebuild.
//!
//! Each test prints what it found; the one past a newer snapshot prints what the pool's driver answered.

use std::process::Command;
use std::time::Duration;

use arca_incus::{
    IncusRead, InstanceState, ProjectFamily, SnapshotPart, StateAction, StateChanger, UnixIncus,
};
use serde_json::json;

const PROJECT: &str = "arcak-svc";

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

struct Proof {
    socket: String,
    instance: String,
    changer: StateChanger,
}

/// The proof's target, or `None` (and a line saying so) when the proof is not asked for.
fn proof() -> Option<Proof> {
    if env("ARCA_REAL_INCUS_PROOF").as_deref() != Some("1") {
        eprintln!("ARCA_REAL_INCUS_PROOF is not 1; nothing was done");
        return None;
    }
    let socket = env("ARCA_PROOF_SOCKET").unwrap_or_else(|| "/var/lib/incus/unix.socket".into());
    let instance = env("ARCA_PROOF_INSTANCE").expect("ARCA_PROOF_INSTANCE");
    let family = ProjectFamily::new([PROJECT], std::iter::empty::<&str>()).expect("family");
    let changer =
        StateChanger::new(socket.clone().into(), family).with_deadline(Duration::from_secs(300));
    Some(Proof {
        socket,
        instance,
        changer,
    })
}

impl Proof {
    fn seen(&self) -> InstanceState {
        self.changer
            .read(PROJECT, &self.instance)
            .expect("the instance reads")
    }

    fn power(&self, action: StateAction) {
        let seen = self.seen();
        let stop = matches!(action, StateAction::Stop);
        if !(stop && seen.is_stopped() || !stop && seen.is_running()) {
            self.changer
                .change(&seen, action)
                .expect("the state change");
        }
    }

    fn sh(&self, script: &str) -> String {
        let args = [
            "exec",
            "--project",
            PROJECT,
            &self.instance,
            "--",
            "sh",
            "-c",
            script,
        ];
        let out = Command::new("incus")
            .args(args)
            .output()
            .expect("incus exec");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{script}: {err}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Writes `mark` on the root and on the data volume, while the instance runs.
    fn mark(&self, mark: &str) {
        self.power(StateAction::Start);
        self.sh(&format!(
            "echo {mark} > /root/arca-proof && echo {mark} > /data/arca-proof && sync"
        ));
    }

    /// What the root and the data volume hold, read while the instance runs.
    fn read_marks(&self) -> (String, String) {
        self.power(StateAction::Start);
        (
            self.sh("cat /root/arca-proof 2>/dev/null || echo none"),
            self.sh("cat /data/arca-proof 2>/dev/null || echo none"),
        )
    }

    fn parts(&self, name: &str) -> Vec<SnapshotPart> {
        let taken = self
            .changer
            .snapshot_with_volumes(&self.seen(), name)
            .expect("taken");
        eprintln!("taken {:?}, not taken {:?}", taken.taken, taken.not_taken);
        assert!(taken.taken.len() >= 2, "the instance and its data volume");
        taken.taken.into_iter().map(|t| t.part).collect()
    }

    fn restore(&self, name: &str, parts: &[SnapshotPart]) {
        self.power(StateAction::Stop);
        self.changer
            .restore_with_volumes(&self.seen(), name, parts)
            .expect("restored");
    }

    fn remove(&self, name: &str, parts: &[SnapshotPart]) {
        let seen = self.seen();
        for part in parts {
            let answer = match part {
                SnapshotPart::Instance => self.changer.remove_instance_snapshot(&seen, name),
                SnapshotPart::Volume { pool, volume } => self
                    .changer
                    .remove_volume_snapshot(&seen, pool, volume, name),
            };
            eprintln!("removal of {name} of {part}: {answer:?}");
        }
    }

    fn driver(&self) -> String {
        let read = UnixIncus::new(self.socket.clone().into());
        let get = |path: String| read.get(&path).expect("a read");
        let instance = get(format!(
            "/1.0/instances/{}?project={PROJECT}",
            self.instance
        ));
        let pool = instance["expanded_devices"]["root"]["pool"]
            .as_str()
            .unwrap_or("");
        let pool = get(format!("/1.0/storage-pools/{pool}"));
        pool["driver"].as_str().unwrap_or("").to_string()
    }
}

#[test]
#[ignore = "touches a real Incus; run by hand with ARCA_REAL_INCUS_PROOF=1"]
fn a_running_instance_that_writes_is_snapshotted_without_a_stop_and_both_disks_come_back() {
    let Some(proof) = proof() else { return };
    proof.mark("before");
    // A writer that runs through the snapshot.
    proof.sh(
        "nohup sh -c 'while :; do date >> /data/arca-ticks; sleep 0.1; done' >/dev/null 2>&1 &",
    );
    let parts = proof.parts("arca-proof-running");
    assert!(proof.seen().is_running(), "the snapshot sent no stop");
    proof.mark("after");
    proof.restore("arca-proof-running", &parts);
    assert_eq!(proof.read_marks(), ("before".into(), "before".into()));
    proof.remove("arca-proof-running", &parts);
}

#[test]
#[ignore = "touches a real Incus; run by hand with ARCA_REAL_INCUS_PROOF=1"]
fn a_stopped_instance_is_snapshotted_exactly_and_both_disks_come_back() {
    let Some(proof) = proof() else { return };
    proof.mark("before");
    proof.power(StateAction::Stop);
    let parts = proof.parts("arca-proof-stopped");
    assert!(proof.seen().is_stopped(), "the snapshot sent no start");
    proof.mark("after");
    proof.restore("arca-proof-stopped", &parts);
    assert_eq!(proof.read_marks(), ("before".into(), "before".into()));
    proof.remove("arca-proof-stopped", &parts);
}

#[test]
#[ignore = "touches a real Incus; run by hand with ARCA_REAL_INCUS_PROOF=1"]
fn a_rebuild_is_refused_past_an_instance_snapshot_and_a_volume_snapshot_undoes_what_the_new_root_wrote()
 {
    let Some(proof) = proof() else { return };
    let image = env("ARCA_PROOF_IMAGE").expect("ARCA_PROOF_IMAGE");
    let (alias, fingerprint) = image.split_once('@').expect("<alias>@<fingerprint>");
    let source = json!({ "type": "image", "alias": alias, "fingerprint": fingerprint });
    proof.mark("before");
    proof.power(StateAction::Stop);
    // Incus 7.6 refuses to rebuild an instance that holds a snapshot of its own.
    let held = proof.parts("arca-proof-held");
    let refused = proof.changer.rebuild_from_local(&proof.seen(), &source);
    eprintln!("a rebuild past an instance snapshot answered {refused:?}");
    assert!(refused.is_err(), "the rebuild is refused");
    proof.remove("arca-proof-held", &held);
    // A snapshot of the data volume alone does not stand in its way, and undoes what the new root wrote there.
    let taken = proof
        .changer
        .snapshot_own_volumes(&proof.seen(), "arca-proof-rebuild")
        .expect("taken");
    eprintln!("taken {:?}, not taken {:?}", taken.taken, taken.not_taken);
    let parts: Vec<SnapshotPart> = taken.taken.into_iter().map(|t| t.part).collect();
    assert!(!parts.is_empty() && !parts.contains(&SnapshotPart::Instance));
    proof
        .changer
        .rebuild_from_local(&proof.seen(), &source)
        .expect("rebuilt");
    let rebuilt = proof.read_marks();
    eprintln!("after the rebuild: {rebuilt:?}");
    assert_eq!(rebuilt.0, "none", "a new root");
    assert_eq!(rebuilt.1, "before", "the volume is kept by the rebuild");
    proof.mark("after");
    proof.restore("arca-proof-rebuild", &parts);
    assert_eq!(proof.read_marks().1, "before", "the volume is back");
    proof.remove("arca-proof-rebuild", &parts);
}

#[test]
#[ignore = "touches a real Incus; run by hand with ARCA_REAL_INCUS_PROOF=1"]
fn a_restore_past_a_newer_snapshot_records_what_the_pools_driver_answers() {
    let Some(proof) = proof() else { return };
    proof.mark("old");
    let parts = proof.parts("arca-proof-old");
    proof.mark("new");
    proof.parts("arca-proof-new");
    proof.mark("after");
    proof.power(StateAction::Stop);
    let answer = proof
        .changer
        .restore_with_volumes(&proof.seen(), "arca-proof-old", &parts);
    eprintln!(
        "driver {}: a restore past a newer snapshot answered {answer:?}",
        proof.driver()
    );
    match &answer {
        Ok(_) => assert_eq!(proof.read_marks(), ("old".into(), "old".into())),
        Err(failure) => eprintln!("refused at {} ({})", failure.part, failure.error),
    }
    proof.remove("arca-proof-new", &parts);
    proof.remove("arca-proof-old", &parts);
}
