use arca_incus::{FakeIncus, FakeState, IncusRead, UnixIncus};
use arca_kernel::{
    desired::EntryId,
    journal::{Journal, MemJournal},
    plugin::{Ctx, Plugin},
};
use arca_netproject::*;
use arca_providers::incus::{Family, IncusProvider, RetainedSet};
use serde_json::json;
#[test]
fn network_home_and_project_are_owned_and_retry_skips_existing_objects() {
    let dir = std::env::temp_dir().join(format!("arca-netproject-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    let fake = FakeIncus::start(
        &dir,
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![],
            pools: vec![],
            networks: vec![],
        },
    )
    .unwrap();
    let mut provider = IncusProvider::new(
        fake.socket().into(),
        Family {
            projects: vec![PROJECT.into()],
            scratch: false,
            droidd_projects: vec!["other".into()],
            reserved_cidrs: vec!["198.51.100.0/24".into()],
        },
        dir.join("secrets"),
        RetainedSet::empty(),
    )
    .unwrap();
    let mut journal = Journal::open(MemJournal::default()).unwrap();
    let manifest = NetProject::manifest();
    let cfg = NetProjectConfig {
        project: PROJECT.into(),
        bridge: BRIDGE.into(),
        cidr: "192.0.2.0/24".into(),
        virtual_machines: 0,
    };
    let error = NetProject::apply(
        &mut Ctx::new(
            EntryId("net-arcak-svc".into()),
            &manifest,
            &mut provider,
            &mut journal,
            0,
        ),
        &cfg,
    )
    .unwrap_err();
    assert!(
        matches!(error, arca_kernel::plugin::PluginError::Failed(message) if message.contains("storage pool"))
    );
    fake.add_profile(
        "default",
        json!({"name":"default","config":{},"devices":{"root":{"pool":"pool1"}}}),
    );
    for _ in 0..2 {
        NetProject::apply(
            &mut Ctx::new(
                EntryId("net-arcak-svc".into()),
                &manifest,
                &mut provider,
                &mut journal,
                0,
            ),
            &cfg,
        )
        .unwrap();
    }
    let read = UnixIncus::new(fake.socket().into());
    let network = read
        .get(&format!("/1.0/networks/{BRIDGE}?project=default"))
        .unwrap();
    assert_eq!(network["config"]["ipv4.address"], "192.0.2.1/24");
    assert_eq!(network["config"]["ipv4.nat"], "true");
    assert_eq!(network["config"]["ipv6.address"], "none");
    let project = read.get(&format!("/1.0/projects/{PROJECT}")).unwrap();
    assert_eq!(project["config"]["limits.virtual-machines"], "0");
    assert_ne!(project["config"]["features.networks"], "true");
    assert_eq!(project["config"]["restricted"], "true");
    assert_eq!(project["config"]["restricted.networks.access"], BRIDGE);
    assert_eq!(project["config"]["restricted.devices.proxy"], "allow");
    assert!(project["config"].get("restricted.devices.disk").is_none());
    let profile = read
        .get(&format!("/1.0/profiles/{PROFILE}?project={PROJECT}"))
        .unwrap();
    assert_eq!(profile["name"], PROFILE);
    assert_eq!(
        profile["description"],
        format!("Arca service (project {PROJECT}); managed by Arca")
    );
    let mut config = profile["config"].as_object().unwrap().clone();
    config.remove("user.arcakernel.entry");
    assert_eq!(
        config,
        json!({"limits.memory":"2GiB","limits.cpu":"2","limits.cpu.priority":"5",
        "limits.processes":"4096","security.privileged":"false","security.nesting":"false",
        "security.idmap.isolated":"true","security.guestapi":"false"})
        .as_object()
        .unwrap()
        .clone()
    );
    assert_eq!(
        profile["devices"],
        json!({"root":{"type":"disk","path":"/","pool":"pool1","size":"16GiB"}})
    );
    assert_eq!(fake.writes().len(), 3);
    // An older project lacks the proxy allowance. An operator's edit simulates that state.
    let mut legacy = project.clone();
    legacy["config"]
        .as_object_mut()
        .unwrap()
        .remove("restricted.devices.proxy");
    {
        use std::io::{Read, Write};
        let mut stream = std::os::unix::net::UnixStream::connect(fake.socket()).unwrap();
        let body = legacy.to_string();
        write!(stream, "PUT /1.0/projects/{PROJECT} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200"));
    }
    for _ in 0..2 {
        NetProject::apply(
            &mut Ctx::new(
                EntryId("net-arcak-svc".into()),
                &manifest,
                &mut provider,
                &mut journal,
                0,
            ),
            &cfg,
        )
        .unwrap();
    }
    let patched = read.get(&format!("/1.0/projects/{PROJECT}")).unwrap();
    assert_eq!(
        patched["config"], project["config"],
        "the patch preserves every other restriction and grant"
    );
    assert_eq!(
        fake.writes().len(),
        5,
        "one operator edit and one reconciliation write"
    );
    drop(fake);
    std::fs::remove_dir_all(dir).unwrap();
}
#[test]
fn service_cidr_requires_ipv4_24_and_no_reserved_overlap() {
    for bad in ["192.0.2.0/25", "::/24", "bad", "192.0.2.0/33"] {
        assert!(check_cidr(bad, &[]).is_err());
    }
    for range in ["192.0.2.0/24", "192.0.2.128/25"] {
        assert!(check_cidr("192.0.2.0/24", &[range.into()]).is_err());
    }
    assert!(check_cidr("192.0.2.0/24", &["198.51.100.0/24".into()]).is_ok());
}

#[test]
fn the_bridged_project_is_created_alone_with_no_network_or_profile_and_a_retry_skips_it() {
    let dir = std::env::temp_dir().join(format!("arca-bridgedproject-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    let fake = FakeIncus::start(
        &dir,
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![],
            pools: vec![],
            networks: vec![],
        },
    )
    .unwrap();
    let mut provider = IncusProvider::new(
        fake.socket().into(),
        Family {
            projects: vec![PROJECT.into()],
            scratch: false,
            droidd_projects: vec!["other".into()],
            reserved_cidrs: vec![],
        },
        dir.join("secrets"),
        RetainedSet::empty(),
    )
    .unwrap();
    let mut journal = Journal::open(MemJournal::default()).unwrap();
    let manifest = BridgedProject::manifest();
    let caps = |project: &str| BridgedProjectConfig {
        project: project.into(),
        virtual_machines: 4,
        cpu: 8,
        memory: "64GiB".into(),
        disk: "1TiB".into(),
    };
    let cfg = caps(PROJECT);
    // Another entry id, or another project, is refused before any request.
    for (entry, cfg) in [
        ("net-arcak-svc", cfg.clone()),
        (BRIDGED_ENTRY, caps("other")),
    ] {
        let error = BridgedProject::apply(
            &mut Ctx::new(
                EntryId(entry.into()),
                &manifest,
                &mut provider,
                &mut journal,
                0,
            ),
            &cfg,
        )
        .unwrap_err();
        assert!(matches!(error, arca_kernel::plugin::PluginError::Config(_)));
    }
    assert!(fake.requests().is_empty());
    for _ in 0..2 {
        BridgedProject::apply(
            &mut Ctx::new(
                EntryId(BRIDGED_ENTRY.into()),
                &manifest,
                &mut provider,
                &mut journal,
                0,
            ),
            &cfg,
        )
        .unwrap();
    }
    let read = UnixIncus::new(fake.socket().into());
    let project = read
        .get(&format!("/1.0/projects/{PROJECT}?project={PROJECT}"))
        .unwrap();
    assert_eq!(project["config"]["user.arcakernel.entry"], BRIDGED_ENTRY);
    assert_eq!(project["config"]["restricted"], "true");
    assert_eq!(project["config"]["restricted.devices.nic"], "allow");
    assert_eq!(project["config"]["limits.containers"], "0");
    assert_eq!(project["config"]["limits.virtual-machines"], "4");
    assert_eq!(project["config"]["limits.cpu"], "8");
    assert_eq!(project["config"]["limits.memory"], "64GiB");
    assert_eq!(project["config"]["limits.disk"], "1TiB");
    assert!(
        project["config"]
            .get("restricted.networks.access")
            .is_none()
    );
    let writes = fake.writes();
    assert_eq!(writes.len(), 1, "one project and nothing else");
    assert!(writes[0].path.starts_with("/1.0/projects"));
    assert!(
        fake.requests()
            .iter()
            .all(|request| !request.path.contains("/1.0/networks")
                && !request.path.contains("/1.0/profiles")),
        "no network and no profile is read or written"
    );
    std::fs::remove_dir_all(dir).unwrap();
}
