//! `service_update` of a tenant's service: the tenant's entry gains the `snapshots` step the service project's owner has,
//! the update snapshots the service's data volume and never its instance, a failed health check restores that volume
//! and leaves the old digest running, and an instance that holds a snapshot is refused before any change. Every value
//! is synthetic.
use super::*;
use arca::handlers::{UpdateOptions, service_handlers_updating};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// The instance the tenant's service `web` runs in.
const WEB: &str = "web-aaaaaaaa";
const NEW: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";

/// A tenant `A` with the service `web` running, which keeps its data on a volume of its own.
fn tenant_rig() -> Rig {
    let rig = Rig::new(Some(policy()));
    let mut template = app_template("app");
    template["data"] = json!({ "path": "/data", "uid": 1000, "gid": 1000, "size": "1GiB" });
    rig.template("app", template);
    rig.create_tenant(json!({ "id": A })).expect("tenant");
    rig.call(
        "service_create",
        json!({ "name": "web", "template": "app", "tenant": A }),
    )
    .expect("create");
    rig
}

/// The update verb of `rig`'s node, whose health check answers as `up` says.
fn update(rig: &Rig, up: &Arc<AtomicBool>, args: Value) -> Result<Value, CallError> {
    let answers = Arc::clone(up);
    let handlers: HashMap<&'static str, Arc<dyn ToolHandler>> = service_handlers_updating(
        rig.kernel.clone(),
        Arc::new(|| NOW),
        rig.cfg.clone(),
        None,
        UpdateOptions {
            probe: Arc::new(move |_, _, _| answers.load(Ordering::SeqCst)),
            pace: Duration::from_millis(10),
        },
    )
    .into_iter()
    .collect();
    handlers["service_update"].call(
        args,
        &Caller {
            token_id: "doctor".into(),
            scope: Scope::Doctor,
            tools: None,
            limits: Default::default(),
        },
    )
}

fn web(rig: &Rig) -> Value {
    rig.get(&format!("/1.0/instances/{WEB}?project={}", project(A)))
        .expect("the instance")
}

/// The custom volume the service's instance mounts for its data.
fn data_volume(rig: &Rig) -> String {
    web(rig)["devices"]["data"]["source"]
        .as_str()
        .expect("a data volume")
        .to_string()
}

/// The image the service's record names.
fn alias(rig: &Rig) -> String {
    rig.entries()
        .into_iter()
        .find(|entry| entry.plugin.0 == "arca.service" && entry.config["name"] == "web")
        .expect("the record")
        .config["source"]["alias"]
        .as_str()
        .expect("alias")
        .to_string()
}

fn edit(rig: &Rig, method: &str, path: &str, body: Value) {
    use std::io::{Read, Write};
    let mut stream = std::os::unix::net::UnixStream::connect(rig.fake().socket()).expect("socket");
    let body = body.to_string();
    write!(stream, "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).expect("write");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read");
    assert!(
        response.starts_with("HTTP/1.1 20"),
        "fixture edit failed: {}",
        response.lines().next().unwrap_or_default()
    );
}

#[test]
fn a_tenants_failed_update_restores_its_data_volume_and_leaves_the_old_digest_running() {
    let rig = tenant_rig();
    let project = project(A);
    let volume = data_volume(&rig);
    let old = alias(&rig);
    let tenant_project = |rig: &Rig| {
        rig.get(&format!("/1.0/projects/{project}"))
            .expect("the tenant's project")["config"]
            .clone()
    };
    assert!(tenant_project(&rig).get("restricted.snapshots").is_none());
    assert!(rig.entry(&format!("tenant-{A}")).get("snapshots").is_none());

    // The node reaches no port of a tenant's service, so its check is that it stays running: this one exits.
    rig.fake().exit_after_start(&project, WEB, 1, "exited");
    let down = Arc::new(AtomicBool::new(false));
    let (code, message) = refusal(update(
        &rig,
        &down,
        json!({ "name": "web", "digest": NEW, "timeoutSeconds": 1, "tenant": A }),
    ));
    assert_eq!(code, "rolled_back", "{message}");
    // The first snapshot the tenant's project refused gave the tenant's entry its step, and the project follows it.
    assert_eq!(rig.entry(&format!("tenant-{A}"))["snapshots"], true);
    assert_eq!(tenant_project(&rig)["restricted.snapshots"], "allow");
    assert_eq!(tenant_project(&rig)["restricted"], "true");
    // The root was rebuilt from the new digest and back from the old one; the volume went back to its snapshot; the
    // instance was never snapshotted or restored.
    assert_eq!(
        rig.fake().rebuilds(&project, WEB),
        vec!["registry", "registry"]
    );
    let restored = rig.fake().volume_restores(&project, "pool1", &volume);
    assert_eq!(restored.len(), 1, "{restored:?}");
    assert!(restored[0].starts_with("pre-update-"));
    assert!(rig.fake().restores(&project, WEB).is_empty());
    assert_eq!(
        rig.get(&format!("/1.0/instances/{WEB}/snapshots?project={project}")),
        Some(json!([]))
    );
    assert_eq!(alias(&rig), old, "the record keeps the old digest");
    assert_eq!(
        rig.fake()
            .state_actions(&project, WEB)
            .last()
            .map(String::as_str),
        Some("start"),
        "the old version was started again"
    );
    rig.fake().run_until_stopped(&project, WEB);

    // The next update takes its own volume snapshot, and the kept one does not stand in the rebuild's way.
    let up = Arc::new(AtomicBool::new(true));
    let done = update(
        &rig,
        &up,
        json!({ "name": "web", "digest": NEW, "tenant": A }),
    )
    .expect("the update");
    assert_eq!(done["updated"], true);
    let snapshot = done["snapshot"].as_str().expect("a snapshot");
    assert!(snapshot.starts_with("pre-update-"));
    assert!(
        rig.get(&format!(
            "/1.0/storage-pools/pool1/volumes/custom/{volume}/snapshots/{snapshot}?project={project}"
        ))
        .is_some()
    );
    let repository = old.split_once('@').expect("<repository>@<digest>").0;
    assert_eq!(alias(&rig), format!("{repository}@{NEW}"));
}

#[test]
fn a_tenants_instance_that_holds_a_snapshot_is_named_before_any_change_and_nothing_is_rebuilt() {
    let rig = tenant_rig();
    let project = project(A);
    // A snapshot taken by hand, in a project that someone let hold one.
    let path = format!("/1.0/projects/{project}");
    let mut config = rig.get(&path).expect("the project")["config"].clone();
    config["restricted.snapshots"] = json!("allow");
    edit(&rig, "PATCH", &path, json!({ "config": config }));
    edit(
        &rig,
        "POST",
        &format!("/1.0/instances/{WEB}/snapshots?project={project}"),
        json!({ "name": "by-hand", "stateful": false }),
    );
    let before = web(&rig);
    let actions = rig.fake().state_actions(&project, WEB);
    let writes = rig.fake().writes().len();

    let up = Arc::new(AtomicBool::new(true));
    let (code, message) = refusal(update(
        &rig,
        &up,
        json!({ "name": "web", "digest": NEW, "tenant": A }),
    ));
    assert_eq!(code, "refused");
    assert!(message.contains("snapshot by-hand"), "{message}");
    assert!(message.contains("passkey"), "{message}");
    assert!(message.ends_with("Nothing was changed"), "{message}");
    assert!(rig.fake().rebuilds(&project, WEB).is_empty());
    assert_eq!(rig.fake().state_actions(&project, WEB), actions);
    assert_eq!(rig.fake().writes().len(), writes);
    assert_eq!(web(&rig), before);
}
