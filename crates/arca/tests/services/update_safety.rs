//! `service_update` where the snapshot, the rollback or the restore does not go the plain way: a project that will not
//! hold the volume's snapshot, an instance that another takes the place of, a rebuild that fails after the root is
//! replaced, a restore that stops halfway, a snapshot list Incus renders short, and a snapshot name that is taken on one
//! volume of two. Every value is synthetic.
use super::*;
use arca_incus::fake::RebuildStep;
use std::sync::Mutex;

/// The snapshot name the first update of the fixed test clock takes.
fn first_name() -> String {
    format!("pre-update-{NOW}")
}

/// Gives the service `app` a second volume of its own, `more`, mounted at `/more`. Its device sorts after `data`, so
/// it is snapshotted and restored after `app-data`.
fn second_volume(rig: &Rig) {
    rig.edit(
        "POST",
        &format!("/1.0/storage-pools/pool1/volumes/custom?project={PROJECT}"),
        json!({
            "name": "more",
            "type": "custom",
            "content_type": "filesystem",
            "config": { "user.arcakernel.entry": "service-app" },
        }),
    );
    let mut device = instance(rig)["devices"]["data"].clone();
    device["source"] = json!("more");
    device["path"] = json!("/more");
    rig.fake.set_instance_device(PROJECT, "app", "more", device);
}

fn more_restores(rig: &Rig) -> Vec<String> {
    rig.fake.volume_restores(PROJECT, "pool1", "more")
}

fn volume_snapshot(rig: &Rig, volume: &str, snapshot: &str) -> Option<Value> {
    rig.get(&format!(
        "/1.0/storage-pools/pool1/volumes/custom/{volume}/snapshots/{snapshot}?project={PROJECT}"
    ))
}

#[test]
fn a_permission_write_incus_refuses_leaves_no_update_without_a_snapshot_on_this_call_or_the_next() {
    let up = Arc::new(AtomicBool::new(false));
    let rig = bare_rig(&up);
    rig.fake.set_reject_put(&format!("/1.0/projects/{PROJECT}"));
    let actions = rig.fake.state_actions(PROJECT, "app");

    let (code, one) = refusal_words(update(
        &rig,
        json!({ "name": "app", "digest": NEW, "timeoutSeconds": 1 }),
    ));
    assert_eq!(code, "refused", "{one}");
    assert!(one.contains("snapshots allowed failed"), "{one}");
    assert!(one.contains("does not rebuild without a snapshot"), "{one}");
    // The kernel saved the owner's step before Incus refused it: the answer says so, and that nothing else changed.
    assert!(one.contains("Nothing else was changed"), "{one}");
    assert!(one.contains("saved with its snapshots step"), "{one}");

    // The step is saved and the project still refuses: the next call does not rebuild without a snapshot either.
    let (code, two) = refusal_words(update(
        &rig,
        json!({ "name": "app", "digest": NEW, "timeoutSeconds": 1 }),
    ));
    assert_eq!(code, "refused", "{two}");
    assert!(two.contains("already lets it hold snapshots"), "{two}");
    assert!(two.ends_with("Nothing was changed"), "{two}");
    assert!(rig.fake.rebuilds(PROJECT, "app").is_empty());
    assert!(volume_restores(&rig).is_empty());
    assert_eq!(alias_of_record(&rig), "app@".to_string() + DIGEST);
    // Each call stopped the service and started it again, and nothing more.
    let mut expected = actions;
    expected.extend(["stop", "start", "stop", "start"].map(String::from));
    assert_eq!(rig.fake.state_actions(PROJECT, "app"), expected);
    assert_eq!(instance(&rig)["status"], "Running");
}

#[test]
fn an_incus_that_still_refuses_after_the_owners_step_is_written_leaves_the_root_alone() {
    let up = Arc::new(AtomicBool::new(false));
    let rig = bare_rig(&up);
    rig.fake.refuse_volume_snapshots(PROJECT);

    let (code, message) = refusal_words(update(
        &rig,
        json!({ "name": "app", "digest": NEW, "timeoutSeconds": 1 }),
    ));
    assert_eq!(code, "refused", "{message}");
    assert!(message.contains("still refuses a snapshot"), "{message}");
    assert!(message.contains("now lets it hold snapshots"), "{message}");
    assert_eq!(project_config(&rig)["restricted.snapshots"], "allow");
    assert!(rig.fake.rebuilds(PROJECT, "app").is_empty());
    assert_eq!(alias_of_record(&rig), "app@".to_string() + DIGEST);
    assert_eq!(instance(&rig)["status"], "Running");
}

#[test]
fn an_instance_that_takes_the_services_place_during_the_health_check_gets_no_write_from_the_rollback()
 {
    let up = Arc::new(AtomicBool::new(false));
    let rig = rig(&up);
    rig.fake
        .recreate_at_next_start(PROJECT, "app", "00000000-0000-4000-8000-0000000000bb");
    let actions = rig.fake.state_actions(PROJECT, "app").len();

    let (code, message) = refusal_words(update(
        &rig,
        json!({ "name": "app", "digest": NEW, "timeoutSeconds": 1 }),
    ));
    assert_eq!(code, "rollback_failed", "{message}");
    assert!(message.contains("another instance has taken"), "{message}");
    assert!(message.contains("Another instance is there"), "{message}");
    // The update's own stop and start, then nothing: no stop, rebuild, restore or start of the newcomer.
    assert_eq!(
        rig.fake.state_actions(PROJECT, "app")[actions..],
        ["stop".to_string(), "start".to_string()]
    );
    assert_eq!(rig.fake.rebuilds(PROJECT, "app"), vec!["registry"]);
    assert!(volume_restores(&rig).is_empty());
    assert_eq!(instance(&rig)["status"], "Running");
}

#[test]
fn a_rebuild_that_fails_after_the_new_root_landed_is_not_reported_as_the_old_root_kept() {
    let up = Arc::new(AtomicBool::new(true));
    let rig = rig(&up);
    rig.fake
        .land_then_fail_rebuild(PROJECT, "app", "Failed applying the image's OCI config");
    let actions = rig.fake.state_actions(PROJECT, "app").len();

    let (code, message) = refusal_words(update(&rig, json!({ "name": "app", "digest": NEW })));
    assert_eq!(code, "rollback_failed", "{message}");
    assert!(message.contains("may be the new version's"), "{message}");
    assert!(message.contains("The service is stopped"), "{message}");
    // Stopped for the update and never started on a root that may be the new one.
    assert_eq!(
        rig.fake.state_actions(PROJECT, "app")[actions..],
        ["stop".to_string()]
    );
    assert_eq!(instance(&rig)["status"], "Stopped");
    assert!(!journal(&rig).contains(&("rebuild back".to_string(), true)));
}

#[test]
fn a_restore_that_stops_at_the_second_volume_is_finished_by_the_next_update_before_it_rebuilds() {
    let up = Arc::new(AtomicBool::new(false));
    let rig = rig(&up);
    second_volume(&rig);
    rig.fake
        .set_reject_put("/1.0/storage-pools/pool1/volumes/custom/more");

    let (code, message) = refusal_words(update(
        &rig,
        json!({ "name": "app", "digest": NEW, "timeoutSeconds": 1 }),
    ));
    assert_eq!(code, "rollback_failed", "{message}");
    let snapshot = first_name();
    assert!(
        message.contains(&format!(
            "the volumes were to go back to the snapshot {snapshot}: put back: app-data (pool1); failed: more (pool1)"
        )),
        "{message}"
    );
    assert!(message.contains("The service is stopped"), "{message}");
    assert_eq!(volume_restores(&rig), vec![snapshot.clone()]);
    assert!(more_restores(&rig).is_empty());
    assert!(
        rig.dir
            .join("service-update-restore-service-app.json")
            .exists()
    );
    let rebuilds = rig.fake.rebuilds(PROJECT, "app").len();

    // The volumes still refuse: the retry does not rebuild on them.
    up.store(true, Ordering::SeqCst);
    let (code, message) = refusal_words(update(&rig, json!({ "name": "app", "digest": NEW })));
    assert_eq!(code, "refused", "{message}");
    assert!(message.contains("restore pending failed"), "{message}");
    // It put app-data back again before `more` refused: that is said, not "Nothing was changed".
    assert!(
        message.contains(&format!(
            "Nothing else was changed: its volumes app-data (pool1) were put back to the snapshot {}",
            first_name()
        )),
        "{message}"
    );
    assert_eq!(rig.fake.rebuilds(PROJECT, "app").len(), rebuilds);

    // Once they take it, every volume goes back to the first snapshot, then the update runs as asked.
    rig.fake.clear_reject_put();
    let done = update(&rig, json!({ "name": "app", "digest": NEW })).expect("the update");
    assert_eq!(done["updated"], true);
    assert_eq!(more_restores(&rig), vec![snapshot.clone()]);
    // app-data went back by the rollback, by the retry that stopped at `more` again, and by this one.
    assert_eq!(
        volume_restores(&rig),
        vec![snapshot.clone(), snapshot.clone(), snapshot]
    );
    assert!(
        !rig.dir
            .join("service-update-restore-service-app.json")
            .exists()
    );
    assert!(journal(&rig).contains(&("restore pending".to_string(), true)));
    assert_eq!(alias_of_record(&rig), format!("app@{NEW}"));
}

#[test]
fn a_running_service_left_part_restored_is_not_stopped_for_the_restore_unasked() {
    let up = Arc::new(AtomicBool::new(false));
    let rig = rig(&up);
    second_volume(&rig);
    rig.fake
        .set_reject_put("/1.0/storage-pools/pool1/volumes/custom/more");
    let _ = update(
        &rig,
        json!({ "name": "app", "digest": NEW, "timeoutSeconds": 1 }),
    );
    rig.fake.clear_reject_put();
    // Started by hand, from elsewhere.
    rig.edit(
        "PUT",
        &format!("/1.0/instances/app/state?project={PROJECT}"),
        json!({ "action": "start" }),
    );
    let actions = rig.fake.state_actions(PROJECT, "app");

    let (code, message) = refusal_words(update(&rig, json!({ "name": "app", "digest": NEW })));
    assert_eq!(code, "refused", "{message}");
    assert!(message.contains("instance_stop"), "{message}");
    assert_eq!(rig.fake.state_actions(PROJECT, "app"), actions);
}

#[test]
fn a_snapshot_the_recursive_list_leaves_out_still_stops_the_update_before_any_change() {
    let up = Arc::new(AtomicBool::new(true));
    let rig = rig(&up);
    snapshot_by_hand(&rig, "hidden");
    // Incus 7.6 drops a snapshot it cannot render from the recursive list and still answers success.
    rig.fake.answer_reads(
        &format!("/1.0/instances/app/snapshots?project={PROJECT}&recursion=1"),
        10,
        json!([]),
    );
    let actions = rig.fake.state_actions(PROJECT, "app");

    let (code, message) = refusal_words(update(&rig, json!({ "name": "app", "digest": NEW })));
    assert_eq!(code, "refused", "{message}");
    assert!(message.contains("snapshot hidden"), "{message}");
    assert_eq!(rig.fake.state_actions(PROJECT, "app"), actions);
    assert!(rig.fake.rebuilds(PROJECT, "app").is_empty());
}

#[test]
fn a_name_taken_on_the_second_volume_is_retried_once_the_first_volumes_snapshot_is_removed() {
    let up = Arc::new(AtomicBool::new(true));
    let rig = rig(&up);
    second_volume(&rig);
    let taken = first_name();
    rig.edit(
        "POST",
        &format!("/1.0/storage-pools/pool1/volumes/custom/more/snapshots?project={PROJECT}"),
        json!({ "name": taken }),
    );

    let done = update(&rig, json!({ "name": "app", "digest": NEW })).expect("the update");
    assert_eq!(done["updated"], true);
    let next = format!("{taken}-2");
    assert_eq!(done["snapshot"], json!(next));
    assert!(volume_snapshot(&rig, "app-data", &taken).is_none());
    assert!(volume_snapshot(&rig, "app-data", &next).is_some());
    assert!(volume_snapshot(&rig, "more", &next).is_some());
}

#[test]
fn a_name_taken_on_the_second_volume_whose_first_snapshot_stays_stops_the_update_and_names_both() {
    let up = Arc::new(AtomicBool::new(true));
    let rig = rig(&up);
    second_volume(&rig);
    let taken = first_name();
    rig.edit(
        "POST",
        &format!("/1.0/storage-pools/pool1/volumes/custom/more/snapshots?project={PROJECT}"),
        json!({ "name": taken }),
    );
    rig.fake.set_reject_delete("pre-update-");

    let (code, message) = refusal_words(update(&rig, json!({ "name": "app", "digest": NEW })));
    assert_eq!(code, "refused", "{message}");
    assert!(
        message.contains(&format!("the snapshot {taken}: the volume more")),
        "{message}"
    );
    assert!(
        message.contains("the volume app-data of the pool pool1 was not removed"),
        "{message}"
    );
    // The snapshot left on app-data is a change, and the answer says so.
    assert!(
        message.contains(&format!(
            "Nothing else was changed: the snapshot {taken} of its volumes app-data (pool1) was taken and could not be removed again"
        )),
        "{message}"
    );
    assert!(rig.fake.rebuilds(PROJECT, "app").is_empty());
    assert!(volume_snapshot(&rig, "app-data", &format!("{taken}-2")).is_none());
    assert_eq!(instance(&rig)["status"], "Running");
}

#[test]
fn an_instance_that_takes_the_services_place_at_the_rollbacks_stop_gets_no_rebuild_restore_or_start()
 {
    let up = Arc::new(AtomicBool::new(false));
    let rig = rig(&up);
    // The update's own stop goes by; at the rollback's stop another, stopped instance takes the name.
    rig.fake
        .recreate_at_stop(PROJECT, "app", "00000000-0000-4000-8000-0000000000cc", 1);
    let actions = rig.fake.state_actions(PROJECT, "app").len();

    let (code, message) = refusal_words(update(
        &rig,
        json!({ "name": "app", "digest": NEW, "timeoutSeconds": 1 }),
    ));
    assert_eq!(code, "rollback_failed", "{message}");
    assert!(message.contains("stop failed"), "{message}");
    assert!(message.contains("Another instance is there"), "{message}");
    assert_eq!(
        rig.fake.state_actions(PROJECT, "app")[actions..],
        ["stop".to_string(), "start".to_string(), "stop".to_string()]
    );
    assert_eq!(rig.fake.rebuilds(PROJECT, "app"), vec!["registry"]);
    assert!(volume_restores(&rig).is_empty());
}

#[test]
fn an_old_rebuild_that_fails_after_its_root_landed_is_not_reported_as_the_old_root_kept() {
    let up = Arc::new(AtomicBool::new(true));
    let rig = rig(&up);
    // The new image is never fetched; the old one is, and its rebuild fails after the old root was taken away.
    rig.fake.script_rebuilds(
        PROJECT,
        "app",
        vec![
            RebuildStep::RefuseFetch,
            RebuildStep::LandThenFail("Failed applying the image's OCI config".to_string()),
        ],
    );
    let actions = rig.fake.state_actions(PROJECT, "app").len();

    let (code, message) = refusal_words(update(&rig, json!({ "name": "app", "digest": NEW })));
    assert_eq!(code, "rollback_failed", "{message}");
    assert!(
        message.contains("no root there is known to be whole"),
        "{message}"
    );
    assert_eq!(
        rig.fake.state_actions(PROJECT, "app")[actions..],
        ["stop".to_string()]
    );
    assert!(volume_restores(&rig).is_empty());
    assert_eq!(instance(&rig)["status"], "Stopped");
}

#[test]
fn the_restore_is_on_file_before_its_first_volume_is_put_back() {
    let up = Arc::new(AtomicBool::new(false));
    let rig = rig(&up);
    second_volume(&rig);
    let record = rig.dir.join("service-update-restore-service-app.json");
    let seen: Arc<Mutex<Vec<(String, bool)>>> = Arc::new(Mutex::new(Vec::new()));
    let (looked, at) = (Arc::clone(&seen), record.clone());
    rig.fake.observe_volume_restores(Arc::new(move |volume| {
        looked
            .lock()
            .expect("the list")
            .push((volume.to_string(), at.exists()));
    }));

    let (code, message) = refusal_words(update(
        &rig,
        json!({ "name": "app", "digest": NEW, "timeoutSeconds": 1 }),
    ));
    assert_eq!(code, "rolled_back", "{message}");
    assert_eq!(
        *seen.lock().expect("the list"),
        vec![
            ("pool1/app-data".to_string(), true),
            ("pool1/more".to_string(), true)
        ]
    );
    // Off file once every volume is back.
    assert!(!record.exists());
}

#[test]
fn a_finished_restore_whose_record_cannot_be_removed_stops_the_update_before_the_rebuild() {
    use std::os::unix::fs::PermissionsExt;
    let up = Arc::new(AtomicBool::new(false));
    let rig = rig(&up);
    second_volume(&rig);
    rig.fake
        .set_reject_put("/1.0/storage-pools/pool1/volumes/custom/more");
    let _ = update(
        &rig,
        json!({ "name": "app", "digest": NEW, "timeoutSeconds": 1 }),
    );
    rig.fake.clear_reject_put();
    let rebuilds = rig.fake.rebuilds(PROJECT, "app").len();
    up.store(true, Ordering::SeqCst);

    // The node's state folder takes no removal: the restore goes through, its record cannot come off file.
    let mode = fs::metadata(&rig.dir)
        .expect("the state folder")
        .permissions()
        .mode();
    fs::set_permissions(&rig.dir, fs::Permissions::from_mode(0o500)).expect("read-only");
    let result = update(&rig, json!({ "name": "app", "digest": NEW }));
    fs::set_permissions(&rig.dir, fs::Permissions::from_mode(mode)).expect("writable again");

    let (code, message) = refusal_words(result);
    assert_eq!(code, "refused", "{message}");
    assert!(message.contains("could not be taken off file"), "{message}");
    assert_eq!(more_restores(&rig), vec![first_name()]);
    assert_eq!(rig.fake.rebuilds(PROJECT, "app").len(), rebuilds);
    assert!(
        rig.dir
            .join("service-update-restore-service-app.json")
            .exists()
    );
    assert_eq!(alias_of_record(&rig), "app@".to_string() + DIGEST);
}

#[test]
fn a_restore_left_on_file_is_finished_even_by_an_update_to_the_digest_the_record_names() {
    let up = Arc::new(AtomicBool::new(false));
    let rig = rig(&up);
    second_volume(&rig);
    rig.fake
        .set_reject_put("/1.0/storage-pools/pool1/volumes/custom/more");
    let _ = update(
        &rig,
        json!({ "name": "app", "digest": NEW, "timeoutSeconds": 1 }),
    );
    rig.fake.clear_reject_put();
    up.store(true, Ordering::SeqCst);

    // The record names the digest asked for, but the volumes are part restored: no "already runs this digest".
    let done = update(&rig, json!({ "name": "app", "digest": DIGEST })).expect("the update");
    assert_eq!(done["updated"], true, "{done}");
    assert_eq!(more_restores(&rig), vec![first_name()]);
    assert!(journal(&rig).contains(&("restore pending".to_string(), true)));
    assert!(
        !rig.dir
            .join("service-update-restore-service-app.json")
            .exists()
    );
}
