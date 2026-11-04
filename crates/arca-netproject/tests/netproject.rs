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
    assert_eq!(fake.writes().len(), 2);
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
