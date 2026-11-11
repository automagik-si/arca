//! The one write gate: every handler that picks an address or a block, and the front pass, waits for it.
use super::*;
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn every_create_and_the_front_pass_wait_for_the_kernels_write_gate() {
    let mut fronted = policy();
    fronted["front"] = json!("edge");
    let rig = Arc::new(two_tenants(fronted));
    // A tenant service with no front rule yet, and then the front: the pass has a record to change.
    rig.call(
        "service_create",
        json!({ "name": "web", "template": "app", "tenant": A }),
    )
    .unwrap();
    // Hold the gate, as a create in flight does.
    let gate = rig.kernel.lock().unwrap().write_gate();
    let held = gate.lock().unwrap();
    let (done, finished) = mpsc::channel::<&'static str>();
    let start = |what: &'static str, run: Box<dyn FnOnce(&Rig) -> bool + Send>| {
        let rig = Arc::clone(&rig);
        let done = done.clone();
        std::thread::spawn(move || {
            assert!(run(&rig), "{what} succeeds once the gate is free");
            done.send(what).unwrap();
        })
    };
    let threads = vec![
        start(
            "service_create",
            Box::new(|rig| {
                rig.call(
                    "service_create",
                    json!({ "name": "edge", "template": "app" }),
                )
                .is_ok()
            }),
        ),
        start(
            "postgres_create",
            Box::new(|rig| {
                rig.call(
                    "postgres_create",
                    json!({ "name": "db", "image": PG_IMAGE }),
                )
                .is_ok()
            }),
        ),
        start(
            "tenant_create",
            Box::new(|rig| rig.create_tenant(json!({ "id": "cccccccc" })).is_ok()),
        ),
        start(
            "sync_front",
            Box::new(|rig| {
                arca::handlers::sync_front(&rig.kernel, &rig.config(), rig.now()).is_ok()
            }),
        ),
    ];
    // None of them gets past the gate while it is held: nothing finishes, and nothing is written.
    let writes = rig.fake().writes().len();
    assert!(
        finished.recv_timeout(Duration::from_millis(700)).is_err(),
        "a handler finished while the write gate was held"
    );
    assert_eq!(rig.fake().writes().len(), writes);
    assert!(
        rig.entries()
            .iter()
            .all(|entry| entry.id.0 != "tenant-cccccccc")
    );
    drop(held);
    let mut seen: Vec<&str> = (0..4)
        .map(|_| {
            finished
                .recv_timeout(Duration::from_secs(30))
                .expect("each finishes")
        })
        .collect();
    seen.sort_unstable();
    assert_eq!(
        seen,
        [
            "postgres_create",
            "service_create",
            "sync_front",
            "tenant_create"
        ]
    );
    for thread in threads {
        thread.join().unwrap();
    }
}
