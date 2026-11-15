//! `service_create` at a chosen build of the template's own image (`digest`), a multi-arch index digest passed to
//! Incus untouched by a create and by an update, and the `Version` row a proposal's card gives Omni's image from a
//! fake registry. No network: the registry is a fake that counts what it is asked.
use super::*;
use arca::handlers::{UpdateOptions, service_handlers_updating};
use arca::image_version::{Page, RegistryRound, TagRegistry, Unanswered, VersionLookup};
use std::sync::atomic::{AtomicU64, AtomicUsize};
use std::time::Duration;

/// Omni's v2.261024.2 as ghcr.io serves it: an OCI image index (amd64, arm64 and two attestation manifests).
const INDEX_FIXTURE: &str = include_str!("../fixtures/omni-api-v2.261024.2-index.json");
const OMNI_REPOSITORY: &str = "automagik-si/omni-api";

/// The index's own digest, worked out from the fixture's bytes, as a registry names it.
fn index_digest() -> String {
    format!("sha256:{}", Digest::of(INDEX_FIXTURE.as_bytes()).to_hex())
}

/// The digests of the per-platform manifests the index lists: none of them may reach Incus.
fn platform_digests() -> Vec<String> {
    let index: Value = serde_json::from_str(INDEX_FIXTURE).expect("the fixture is JSON");
    assert_eq!(
        index["mediaType"],
        "application/vnd.oci.image.index.v1+json"
    );
    let digests: Vec<String> = index["manifests"]
        .as_array()
        .expect("manifests")
        .iter()
        .map(|manifest| manifest["digest"].as_str().expect("a digest").to_string())
        .collect();
    let platforms: Vec<&str> = index["manifests"]
        .as_array()
        .expect("manifests")
        .iter()
        .filter_map(|manifest| manifest["platform"]["architecture"].as_str())
        .collect();
    assert!(platforms.contains(&"amd64") && platforms.contains(&"arm64"));
    digests
}

fn omni_template() -> Value {
    json!({
        "name": "omni-api", "kind": "oci", "image": "ghcr.io/automagik-si/omni-api",
        "digest": DIGEST, "ports": [8882],
        "data": { "path": "/app/data", "uid": 1000, "gid": 1000 },
        "env": {}, "droiddPort": 8882, "publishable": false
    })
}

/// A node with the omni-api template, whose update handler's health check always answers. `key` is the owner's.
fn omni_node(key: Option<&common::SkKey>) -> Rig {
    let mut rig = Rig::with_key(
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![],
            networks: vec![],
            pools: vec![("pool1".into(), json!({}))],
        },
        vec![PROJECT.to_string()],
        Some("192.0.2.0/24"),
        key,
    );
    rig.handlers = service_handlers_updating(
        rig.kernel.clone(),
        Arc::new(|| NOW),
        rig.cfg.clone(),
        None,
        UpdateOptions {
            probe: Arc::new(|_, _, _| true),
            pace: Duration::from_millis(10),
        },
    )
    .into_iter()
    .collect();
    rig.template("omni-api", omni_template());
    rig
}

/// The alias the service's record keeps, the source Incus is given.
fn recorded_alias(rig: &Rig, service: &str) -> String {
    rig.entries()
        .into_iter()
        .find(|entry| entry.id.0 == format!("service-{service}"))
        .expect("the record")
        .config["source"]["alias"]
        .as_str()
        .expect("an alias")
        .to_string()
}

/// The aliases Incus was asked to fetch, as it keeps them on the images it pulled into the service project's store.
fn fetched_aliases(rig: &Rig) -> Vec<String> {
    rig.get(&format!("/1.0/images?project={PROJECT}&recursion=1"))
        .expect("the images")
        .as_array()
        .expect("a list")
        .iter()
        .filter_map(|image| image["update_source"]["alias"].as_str().map(str::to_string))
        .collect()
}

/// A registry that counts what it is asked: `tags` with their digests, or every round refused.
struct FakeRegistry {
    tags: Vec<(String, String)>,
    refuse: bool,
    opens: AtomicUsize,
    requests: AtomicUsize,
}

impl FakeRegistry {
    fn new(tags: &[(&str, &str)]) -> Arc<Self> {
        Arc::new(Self {
            tags: tags
                .iter()
                .map(|(tag, digest)| (tag.to_string(), digest.to_string()))
                .collect(),
            refuse: false,
            opens: AtomicUsize::new(0),
            requests: AtomicUsize::new(0),
        })
    }

    fn refusing() -> Arc<Self> {
        Arc::new(Self {
            tags: vec![],
            refuse: true,
            opens: AtomicUsize::new(0),
            requests: AtomicUsize::new(0),
        })
    }

    fn asked(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

struct Shared(Arc<FakeRegistry>);
struct Round<'a>(&'a FakeRegistry);

impl TagRegistry for Shared {
    fn open<'a>(
        &'a self,
        repository: &str,
        _wait: Duration,
    ) -> Result<Box<dyn RegistryRound + 'a>, Unanswered> {
        assert_eq!(repository, OMNI_REPOSITORY);
        self.0.opens.fetch_add(1, Ordering::SeqCst);
        self.0.requests.fetch_add(1, Ordering::SeqCst);
        if self.0.refuse {
            return Err(Unanswered);
        }
        Ok(Box::new(Round(&self.0)))
    }
}

impl RegistryRound for Round<'_> {
    fn tags(&mut self, _last: Option<&str>, _wait: Duration) -> Result<Page, Unanswered> {
        self.0.requests.fetch_add(1, Ordering::SeqCst);
        Ok(Page {
            tags: self.0.tags.iter().map(|(tag, _)| tag.clone()).collect(),
            more: false,
        })
    }

    fn digest(&mut self, tag: &str, _wait: Duration) -> Result<String, Unanswered> {
        self.0.requests.fetch_add(1, Ordering::SeqCst);
        self.0
            .tags
            .iter()
            .find(|(known, _)| known == tag)
            .map(|(_, digest)| digest.clone())
            .ok_or(Unanswered)
    }
}

fn versions(registry: &Arc<FakeRegistry>, clock: &Arc<AtomicU64>) -> Arc<VersionLookup> {
    let clock = Arc::clone(clock);
    Arc::new(VersionLookup::new(
        Arc::new(Shared(Arc::clone(registry))),
        Arc::new(move || clock.load(Ordering::SeqCst)),
    ))
}

fn plans(rig: &Rig, versions: &Arc<VersionLookup>) -> HashMap<&'static str, Arc<dyn ToolHandler>> {
    arca::handlers::plan_handlers_versioned(
        rig.kernel.clone(),
        Arc::new(|| NOW),
        None,
        None,
        None,
        None,
        arca::handlers::ActionSlot::default(),
        Some(rig.cfg.clone()),
        Arc::clone(versions),
    )
    .into_iter()
    .collect()
}

fn propose(
    plans: &HashMap<&'static str, Arc<dyn ToolHandler>>,
    action: &str,
    args: Value,
) -> Result<Value, CallError> {
    plans["action_propose"].call(json!({ "action": action, "args": args }), &doctor())
}

/// The card's row under `label`, when it has one.
fn row(proposal: &Value, label: &str) -> Option<String> {
    proposal["details"]
        .as_array()
        .expect("details")
        .iter()
        .find(|row| row["label"] == label)
        .map(|row| row["value"].as_str().expect("a value").to_string())
}

#[test]
fn a_create_with_a_digest_is_made_from_that_build_and_records_it() {
    let rig = omni_node(None);
    let index = index_digest();
    rig.call(
        "service_create",
        json!({ "name": "omni", "template": "omni-api", "digest": index }),
    )
    .expect("create");
    let alias = format!("{OMNI_REPOSITORY}@{index}");
    assert_eq!(recorded_alias(&rig, "omni"), alias);
    assert_eq!(fetched_aliases(&rig), vec![alias]);
}

#[test]
fn a_create_without_a_digest_is_made_from_the_templates_pin_as_before() {
    let rig = omni_node(None);
    rig.call(
        "service_create",
        json!({ "name": "omni", "template": "omni-api" }),
    )
    .expect("create");
    let alias = format!("{OMNI_REPOSITORY}@{DIGEST}");
    assert_eq!(recorded_alias(&rig, "omni"), alias);
    assert_eq!(fetched_aliases(&rig), vec![alias]);
}

#[test]
fn an_index_digest_reaches_incus_untouched_at_create_and_at_update() {
    let index = index_digest();
    let platforms = platform_digests();
    let sent_index_only = |rig: &Rig| {
        let aliases = fetched_aliases(rig);
        assert!(
            aliases.contains(&format!("{OMNI_REPOSITORY}@{index}")),
            "{aliases:?}"
        );
        for alias in aliases.iter().chain([recorded_alias(rig, "omni")].iter()) {
            for platform in &platforms {
                assert!(!alias.contains(&platform[7..]), "{alias} names {platform}");
            }
        }
    };
    // Created at the index.
    let rig = omni_node(None);
    rig.call(
        "service_create",
        json!({ "name": "omni", "template": "omni-api", "digest": index }),
    )
    .expect("create");
    sent_index_only(&rig);

    // Created at the pin, then moved to the index.
    let rig = omni_node(None);
    rig.call(
        "service_create",
        json!({ "name": "omni", "template": "omni-api" }),
    )
    .expect("create");
    let moved = rig
        .call("service_update", json!({ "name": "omni", "digest": index }))
        .expect("update");
    assert_eq!(moved["updated"], true);
    assert_eq!(rig.fake.rebuilds(PROJECT, "omni"), vec!["registry"]);
    assert_eq!(
        recorded_alias(&rig, "omni"),
        format!("{OMNI_REPOSITORY}@{index}")
    );
    sent_index_only(&rig);
}

#[test]
fn a_digest_of_any_other_shape_is_an_invalid_argument_and_creates_nothing() {
    let hex = "b".repeat(64);
    for digest in [
        json!(format!("ghcr.io/someone/else@sha256:{hex}")),
        json!(format!("{OMNI_REPOSITORY}@sha256:{hex}")),
        json!("v2.261024.2"),
        json!(format!("sha256:{}", hex.to_uppercase())),
        json!("sha256:bbbb"),
        json!(42),
    ] {
        let rig = omni_node(None);
        let writes = rig.fake.writes().len();
        let result = rig.call(
            "service_create",
            json!({ "name": "omni", "template": "omni-api", "digest": digest }),
        );
        assert!(
            matches!(result, Err(CallError::Invalid(ref why)) if why.starts_with("digest:")),
            "{digest}: {result:?}"
        );
        assert_eq!(rig.fake.writes().len(), writes, "{digest}");
        assert!(
            rig.entries()
                .iter()
                .all(|entry| entry.id.0 != "service-omni")
        );
        // A proposal of it is refused the same way, and no card is recorded.
        let registry = FakeRegistry::new(&[]);
        let plans = plans(&rig, &versions(&registry, &Arc::new(AtomicU64::new(0))));
        assert!(
            propose(
                &plans,
                "service_create",
                json!({ "name": "omni", "template": "omni-api", "digest": digest })
            )
            .is_err(),
            "{digest}"
        );
        assert!(
            rig.kernel
                .lock()
                .expect("kernel")
                .action_proposals()
                .is_empty()
        );
    }
}

#[test]
fn a_digest_with_a_template_that_is_not_an_oci_image_is_an_invalid_argument() {
    let rig = omni_node(None);
    rig.template(
        "dash",
        json!({
            "name": "dash", "kind": "system", "image": "images:debian/13",
            "ports": [], "env": {}, "publishable": false
        }),
    );
    let writes = rig.fake.writes().len();
    let result = rig.call(
        "service_create",
        json!({ "name": "dash", "template": "dash", "digest": index_digest() }),
    );
    assert!(
        matches!(result, Err(CallError::Invalid(ref why)) if why.contains("not an OCI image")),
        "{result:?}"
    );
    assert_eq!(rig.fake.writes().len(), writes);
}

#[test]
fn a_proposed_create_at_a_digest_binds_it_and_its_card_names_the_version() {
    let key = common::SkKey::from_seed(21);
    let rig = omni_node(Some(&key));
    let index = index_digest();
    let registry = FakeRegistry::new(&[("v2.261024.2", &index), ("v2.261023.1", DIGEST)]);
    let plans = plans(
        &rig,
        &versions(&registry, &Arc::new(AtomicU64::new(5_000_000))),
    );
    let proposed = propose(
        &plans,
        "service_create",
        json!({ "name": "omni", "template": "omni-api", "digest": index }),
    )
    .expect("proposed");
    assert_eq!(proposed["args"]["digest"], index);
    assert_eq!(row(&proposed, "Digest").as_deref(), Some(index.as_str()));
    assert_eq!(
        proposed["args"]["image"],
        format!("ghcr.io/automagik-si/omni-api@{index}")
    );
    assert_eq!(
        row(&proposed, "Image").expect("an image row"),
        format!("ghcr.io/automagik-si/omni-api@{index}")
    );
    let version = row(&proposed, "Version").expect("a version row");
    assert!(
        version.starts_with(&format!("Omni v2.261024.2 ({index})")),
        "{version}"
    );
    assert!(
        version.contains("ghcr.io gave this digest at most ten minutes before this was proposed"),
        "{version}"
    );
    // The version is a label: it is no argument, so the approval binds the digest alone.
    assert!(proposed["args"].get("version").is_none());

    // The owner approves it, and the service is made at that digest.
    let statement = arca::handlers::action_statement(
        "node1",
        proposed["id"].as_str().expect("id"),
        "service_create",
        proposed["argsDigest"].as_str().expect("digest"),
        NOW,
        &"6".repeat(32),
    )
    .expect("the statement");
    let approval = common::approve_statement(&key, &statement);
    let approved = {
        let mut kernel = rig.kernel.lock().expect("kernel");
        arca::handlers::approve_action(
            &mut kernel,
            &json!({ "plan": proposed["id"], "approval": approval }),
            NOW,
        )
        .expect("the owner's approval verifies")
    };
    arca::handlers::ActionExecutor::new(
        rig.handlers
            .iter()
            .map(|(verb, handler)| (*verb, Arc::clone(handler))),
    )
    .run(&approved.action, &approved.args, &approved.grant)
    .expect("the approved create runs");
    assert_eq!(
        recorded_alias(&rig, "omni"),
        format!("{OMNI_REPOSITORY}@{index}")
    );
}

#[test]
fn a_miss_or_a_registry_failure_says_version_unknown_and_the_proposal_still_succeeds() {
    let index = index_digest();
    let args = json!({ "name": "omni", "template": "omni-api", "digest": index });
    let unknown = format!("{index} (version unknown)");
    let mut digests = Vec::new();
    for registry in [
        FakeRegistry::new(&[("v2.261023.1", DIGEST)]),
        FakeRegistry::refusing(),
        FakeRegistry::new(&[("v2.261024.2", &index)]),
    ] {
        let rig = omni_node(None);
        let plans = plans(
            &rig,
            &versions(&registry, &Arc::new(AtomicU64::new(5_000_000))),
        );
        let proposed = propose(&plans, "service_create", args.clone()).expect("proposed");
        digests.push(proposed["argsDigest"].clone());
        if registry.refuse || registry.tags.iter().all(|(_, digest)| *digest != index) {
            assert_eq!(row(&proposed, "Version").as_deref(), Some(unknown.as_str()));
        }
    }
    // The args digest the owner signs is the same whatever the registry answered.
    assert!(
        digests.windows(2).all(|pair| pair[0] == pair[1]),
        "{digests:?}"
    );
}

#[test]
fn a_create_without_a_digest_names_the_version_of_the_templates_pin() {
    let rig = omni_node(None);
    let registry = FakeRegistry::new(&[("v2.261024.2", DIGEST)]);
    let plans = plans(
        &rig,
        &versions(&registry, &Arc::new(AtomicU64::new(5_000_000))),
    );
    let proposed = propose(
        &plans,
        "service_create",
        json!({ "name": "omni", "template": "omni-api" }),
    )
    .expect("proposed");
    assert_eq!(
        proposed["args"]["image"],
        format!("ghcr.io/automagik-si/omni-api@{DIGEST}")
    );
    assert!(proposed["args"].get("digest").is_none());
    let version = row(&proposed, "Version").expect("a version row");
    assert!(
        version.starts_with(&format!("Omni v2.261024.2 ({DIGEST})")),
        "{version}"
    );
    assert_eq!(registry.opens.load(Ordering::SeqCst), 1);

    // An image that is not looked up gets no row and asks nothing.
    let rig = omni_node(None);
    rig.template(
        "app",
        json!({
            "name": "app", "kind": "oci", "image": "registry.example/app",
            "digest": DIGEST, "ports": [80], "env": {}, "publishable": false
        }),
    );
    let registry = FakeRegistry::new(&[("v2.261024.2", DIGEST)]);
    let app_plans = self::plans(
        &rig,
        &versions(&registry, &Arc::new(AtomicU64::new(5_000_000))),
    );
    let proposed = propose(
        &app_plans,
        "service_create",
        json!({ "name": "app", "template": "app" }),
    )
    .expect("proposed");
    assert_eq!(row(&proposed, "Version"), None);
    assert_eq!(registry.asked(), 0);
}

#[test]
fn proposals_for_several_digests_share_one_lookup_round_and_a_repeat_asks_nothing() {
    let rig = omni_node(None);
    rig.call(
        "service_create",
        json!({ "name": "omni", "template": "omni-api" }),
    )
    .expect("create");
    let index = index_digest();
    let other = format!("sha256:{}", "c".repeat(64));
    let registry = FakeRegistry::new(&[("v2.261024.2", &index), ("v2.261025.1", &other)]);
    let clock = Arc::new(AtomicU64::new(5_000_000));
    let plans = plans(&rig, &versions(&registry, &clock));

    let update = propose(
        &plans,
        "service_update",
        json!({ "name": "omni", "digest": index }),
    )
    .expect("proposed");
    assert!(
        row(&update, "Version")
            .expect("a version row")
            .starts_with(&format!("Omni v2.261024.2 ({index})"))
    );
    let after_one = registry.asked();
    assert_eq!(registry.opens.load(Ordering::SeqCst), 1);

    clock.fetch_add(20_000, Ordering::SeqCst);
    let create = propose(
        &plans,
        "service_create",
        json!({ "name": "omni2", "template": "omni-api", "digest": other }),
    )
    .expect("proposed");
    assert!(
        row(&create, "Version")
            .expect("a version row")
            .starts_with(&format!("Omni v2.261025.1 ({other})"))
    );
    // The same digest proposed again, and a digest the fresh map does not name: nothing more is asked.
    propose(
        &plans,
        "service_update",
        json!({ "name": "omni", "digest": index, "timeoutSeconds": 30 }),
    )
    .expect("proposed");
    let missing = format!("sha256:{}", "d".repeat(64));
    let unknown = propose(
        &plans,
        "service_update",
        json!({ "name": "omni", "digest": missing }),
    )
    .expect("proposed");
    assert_eq!(
        row(&unknown, "Version").as_deref(),
        Some(format!("{missing} (version unknown)").as_str())
    );
    assert_eq!(
        registry.asked(),
        after_one,
        "one round answered every proposal"
    );
}

#[test]
fn an_update_proposal_for_an_image_that_is_not_omnis_asks_no_registry() {
    let rig = omni_node(None);
    rig.template(
        "app",
        json!({
            "name": "app", "kind": "oci", "image": "registry.example/app", "digest": DIGEST,
            "ports": [8080], "env": {}, "publishable": false
        }),
    );
    rig.call(
        "service_create",
        json!({ "name": "app", "template": "app" }),
    )
    .expect("create");
    let registry = FakeRegistry::new(&[("v2.261024.2", DIGEST)]);
    let plans = plans(
        &rig,
        &versions(&registry, &Arc::new(AtomicU64::new(5_000_000))),
    );
    let proposed = propose(
        &plans,
        "service_update",
        json!({ "name": "app", "digest": index_digest() }),
    )
    .expect("proposed");
    assert_eq!(row(&proposed, "Version"), None);
    assert_eq!(registry.asked(), 0);
}
