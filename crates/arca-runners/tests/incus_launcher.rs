//! The Incus launcher against the repository's fake Incus: a job machine is a virtual machine of exactly its class's
//! size, carrying the pool's mark; only marked machines are listed and destroyed.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;

use arca_incus::{FakeIncus, FakeState};
use arca_runners::incus::IncusLauncher;
use arca_runners::launcher::{Launcher, MachineSpec};
use serde_json::{Value, json};

const PROJECT: &str = "arcak-runners";

fn fake() -> (FakeIncus, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "arca-runners-incus-{}-{}",
        std::process::id(),
        arca_runners::time::now_ms()
    ));
    fs::create_dir_all(&dir).expect("scratch");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("0700");
    let fake = FakeIncus::start(
        &dir,
        FakeState {
            server: json!({}),
            projects: vec![json!({ "name": PROJECT, "config": { "features.images": "false", "features.profiles": "false" } })],
            instances: vec![json!({
                "name": "someone-else",
                "project": PROJECT,
                "type": "virtual-machine",
                "status": "Running",
                "config": { "volatile.uuid": "00000000-0000-4000-8000-000000000001" },
                "devices": {},
            })],
            networks: vec![],
            pools: vec![],
        },
    )
    .expect("fake");
    fake.add_profile(
        "default",
        json!({"name":"default","config":{},"devices":{"root":{"type":"disk","pool":"pool1","path":"/"}}}),
    );
    (fake, dir)
}

fn instance(fake: &FakeIncus, name: &str) -> Option<Value> {
    let read = arca_incus::UnixIncus::new(fake.socket().to_path_buf());
    arca_incus::IncusRead::get(&read, &format!("/1.0/instances/{name}?project={PROJECT}")).ok()
}

#[test]
fn a_job_machine_is_a_marked_vm_of_its_size_and_only_marked_machines_go() {
    let (fake, dir) = fake();
    let launcher = IncusLauncher::new(
        fake.socket().to_path_buf(),
        PROJECT,
        "acme",
        "runner-ubuntu-2404",
        vec![],
        None,
    )
    .expect("launcher");
    let spec = MachineSpec {
        name: "acme-0-0a1b2c3d".to_string(),
        cpu: 4,
        memory_mib: 16 * 1024,
        disk_gib: None,
    };
    launcher.create(&spec).expect("create");
    let made = instance(&fake, &spec.name).expect("the machine exists");
    let made = made.get("metadata").unwrap_or(&made).clone();
    assert_eq!(made["type"], "virtual-machine");
    assert_eq!(made["config"]["user.arca.runner"], "acme");
    assert_eq!(made["config"]["limits.cpu"], "4");
    assert_eq!(made["config"]["limits.memory"], "16384MiB");
    // A second machine of that name is refused.
    assert!(launcher.create(&spec).is_err());
    // Only the pool's own machine is listed; another instance of the project is not touched.
    assert_eq!(launcher.existing().expect("list"), vec![spec.name.clone()]);
    assert!(launcher.destroy("someone-else").is_err());
    assert!(instance(&fake, "someone-else").is_some());
    // Another pool's launcher sees none of this pool's machines.
    let other = IncusLauncher::new(
        fake.socket().to_path_buf(),
        PROJECT,
        "other",
        "runner-ubuntu-2404",
        vec![],
        None,
    )
    .expect("launcher");
    assert!(other.existing().expect("list").is_empty());
    assert!(other.destroy(&spec.name).is_err());
    launcher.destroy(&spec.name).expect("destroy");
    assert!(instance(&fake, &spec.name).is_none());
    assert!(launcher.existing().expect("list").is_empty());
    // A machine that is gone is not an error.
    launcher.destroy(&spec.name).expect("destroy again");
    let writes = fake.writes();
    assert!(writes.iter().any(|write| write.method == "POST"
        && write.config_keys.contains(&"user.arca.runner".to_string())));
    let _ = fs::remove_dir_all(dir);
}
