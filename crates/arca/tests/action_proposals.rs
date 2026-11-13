//! Action proposals: the inert record, its kernel-built summary, the closed action list, the waiting limit, and the
//! approval checks that gate exactly one run. All of it runs in-process against the toy kernel and fake Incus; no service or
//! database is touched.

mod common;
#[path = "../../arca-approval/tests/common/webauthn.rs"]
mod webauthn;

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arca::action_proposal::{ACTION_PROPOSALS_FILE, ActionKind, MAX_WAITING, PROPOSAL_RATE_MAX};
use arca::handlers::{ActionExecutor, plan_handlers};
use arca::kernel::Kernel;
use arca_api::{Scope, verb};
use arca_approval::{ApprovalRefusal, ArcaAction, parse_statement};
use arca_mcp::{CallError, Caller, Server, TokenStore, ToolHandler, mint, serve_unix};
use arca_reconcile::PLAN_TTL_MS;
use common::*;
use serde_json::{Value, json};
use tokio::runtime::Runtime;

/// A digest-pinned image the test kernel configures as its postgres default.
const TEST_PG_IMAGE: &str = "docker.io/library/postgres@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// A handler that counts its runs. It stands in for the gated verb, so the test can prove the page ran it once.
struct Counting {
    runs: Arc<AtomicU64>,
}

impl ToolHandler for Counting {
    fn call(&self, _args: Value, _caller: &Caller) -> Result<Value, CallError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        Ok(json!({ "created": true }))
    }
}

struct Rig {
    _scratch: Scratch,
    _fake: arca_incus::FakeIncus,
    kernel: Arc<Mutex<Kernel>>,
    clock: Arc<AtomicU64>,
    handlers: std::collections::HashMap<&'static str, Arc<dyn ToolHandler>>,
    runs: Arc<AtomicU64>,
    executor: ActionExecutor,
    gpu: Arc<arca::gpu::FakeGpu>,
}

impl Rig {
    fn new(tag: &str, key: &SkKey) -> Rig {
        Self::with_image(tag, key, Some(TEST_PG_IMAGE))
    }

    fn with_image(tag: &str, key: &SkKey, postgres_image: Option<&str>) -> Rig {
        let scratch = Scratch::new(tag);
        let fake = start_fake(&scratch);
        let verifier = verifier_for(&scratch, &[key], tag);
        let kernel = Arc::new(Mutex::new(
            kernel_with_image(&scratch, &fake, verifier, NOW, postgres_image)
                .expect("the kernel opens"),
        ));
        let clock = Arc::new(AtomicU64::new(NOW));
        let gpu = Arc::new(
            arca::gpu::FakeGpu::new(
                gpu_cards(),
                vec![
                    gpu_instance("alpha", "arcak-gpu", "container"),
                    gpu_instance("beta", "arcak-gpu", "container"),
                    gpu_instance("foreign", "droidd", "container"),
                    gpu_instance("benchy", "bench", "container"),
                    arca::gpu::GpuInstance {
                        project: "arcak-gpu".to_string(),
                        name: "unowned".to_string(),
                        kind: "container".to_string(),
                        uuid: Some("00000000-0000-4000-8000-0000000unowned".to_string()),
                        own_config: std::collections::BTreeMap::new(),
                    },
                ],
            )
            .with_family(["arcak-gpu".to_string()]),
        );
        let handlers: std::collections::HashMap<&'static str, Arc<dyn ToolHandler>> =
            plan_handlers(
                Arc::clone(&kernel),
                {
                    let clock = Arc::clone(&clock);
                    Arc::new(move || clock.load(Ordering::SeqCst))
                },
                postgres_image.map(str::to_string),
                Some(Arc::clone(&gpu) as Arc<dyn arca::gpu::GpuBackend>),
            )
            .into_iter()
            .collect();
        let runs = Arc::new(AtomicU64::new(0));
        let executor = ActionExecutor::new([
            (
                "service_create",
                Arc::new(Counting {
                    runs: Arc::clone(&runs),
                }) as Arc<dyn ToolHandler>,
            ),
            (
                "gpu_attach",
                Arc::clone(&handlers["gpu_attach"]) as Arc<dyn ToolHandler>,
            ),
            (
                "gpu_detach",
                Arc::clone(&handlers["gpu_detach"]) as Arc<dyn ToolHandler>,
            ),
        ]);
        Rig {
            _scratch: scratch,
            _fake: fake,
            kernel,
            clock,
            handlers,
            runs,
            executor,
            gpu,
        }
    }

    fn propose(&self, action: &str, args: Value) -> Result<Value, CallError> {
        call_verb(
            &self.handlers,
            "action_propose",
            json!({ "action": action, "args": args }),
        )
    }

    fn list(&self) -> Value {
        call_verb(&self.handlers, "plan_list", json!({})).expect("plan_list answers")
    }

    fn at(&self, now: u64) {
        self.clock.store(now, Ordering::SeqCst);
    }

    fn approve(
        &self,
        id: &str,
        approval: &str,
        now: u64,
    ) -> Result<arca::kernel::ApprovedAction, CallError> {
        let mut kernel = self.kernel.lock().expect("the kernel lock");
        arca::handlers::approve_action(
            &mut kernel,
            &json!({ "plan": id, "approval": approval }),
            now,
        )
    }
}

/// A create as a proposal holds it: bound to its template's content, as the node binds it when it is proposed. An
/// unbound one is never approved.
fn create_args(name: &str) -> Value {
    json!({ "name": name, "template": "nats", "templateDigest": "ab".repeat(32) })
}

/// Two identical cards, as the second RTX 4060 Ti will be: the same product string, two PCI addresses.
fn gpu_cards() -> Vec<arca_metrics::GpuCard> {
    let mut first = arca_metrics::GpuCard::new("0000:01:00.0", "NVIDIA GeForce RTX 4060 Ti");
    first.uuid = Some("GPU-11111111-1111-1111-1111-111111111111".to_string());
    first.vram_total_bytes = Some(17_179_869_184);
    let mut second = arca_metrics::GpuCard::new("0000:02:00.0", "NVIDIA GeForce RTX 4060 Ti");
    second.uuid = Some("GPU-22222222-2222-2222-2222-222222222222".to_string());
    second.vram_total_bytes = Some(17_179_869_184);
    vec![first, second]
}

fn gpu_instance(name: &str, project: &str, kind: &str) -> arca::gpu::GpuInstance {
    arca::gpu::GpuInstance {
        project: project.to_string(),
        name: name.to_string(),
        kind: kind.to_string(),
        uuid: Some(format!("00000000-0000-4000-8000-{name:0>12}")),
        own_config: std::collections::BTreeMap::from([(
            "user.arcakernel.entry".to_string(),
            "gpu-alpha".to_string(),
        )]),
    }
}

fn statement(host: &str, id: &str, action: &str, digest: &str, nonce: &str) -> String {
    arca::handlers::action_statement(host, id, action, digest, NOW, nonce)
        .expect("a kernel-built statement")
}

#[test]
fn a_viewer_may_propose_and_nothing_changes() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("proposal-inert", &key);
    let before = rig.kernel.lock().expect("lock").entries().to_vec();
    let viewer = Caller {
        token_id: "viewer".to_string(),
        scope: Scope::Viewer,
        tools: None,
    };
    let proposed = rig.handlers["action_propose"]
        .call(
            json!({ "action": "service_create", "args": create_args("nats") }),
            &viewer,
        )
        .expect("a viewer may propose");
    assert_eq!(proposed["kind"], json!("action"));
    assert!(
        proposed["details"]
            .as_array()
            .is_some_and(|rows| rows.len() == 3),
        "the owner sees every argument"
    );
    assert_eq!(proposed["action"], json!("service_create"));
    assert_eq!(
        proposed["summary"],
        json!("Create the service nats from template nats")
    );
    assert!(
        proposed["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("act-") && id.len() == 16)
    );
    assert_eq!(
        rig.kernel.lock().expect("lock").entries(),
        before.as_slice(),
        "the desired state is unchanged"
    );
    assert_eq!(rig.list()["plans"].as_array().map(Vec::len), Some(1));
    assert!(
        rig.list()["plans"][0].get("statement").is_none(),
        "plan_list leaves the action statement to the approval page"
    );
    assert!(
        !arca_api::may_call(Scope::Viewer, verb("approval_submit").expect("present")),
        "a viewer can never approve"
    );
    for name in [
        "service_create",
        "service_remove",
        "service_publish",
        "service_unpublish",
        "service_connect",
        "service_disconnect",
        "postgres_create",
    ] {
        assert!(
            !arca_api::may_call(Scope::Viewer, verb(name).expect("present")),
            "a viewer can never apply {name}"
        );
    }
}

#[test]
fn the_summary_is_built_by_the_kernel_and_an_extra_field_is_refused() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("proposal-summary", &key);
    let proposed = rig
        .propose("service_remove", json!({ "name": "nats" }))
        .expect("a removal proposal");
    assert_eq!(proposed["summary"], json!("Remove the service nats"));
    let injected = rig
        .propose(
            "service_create",
            json!({ "name": "nats", "template": "nats", "summary": "caller text" }),
        )
        .expect_err("the schema refuses an unlisted field");
    assert!(matches!(injected, CallError::Invalid(_)));
    assert_eq!(
        rig.list()["plans"].as_array().map(Vec::len),
        Some(1),
        "the refusal recorded nothing"
    );
}

#[test]
fn every_argument_of_every_allowed_verb_is_in_details() {
    fn sample(action: ActionKind) -> Value {
        match action {
            ActionKind::ServiceCreate => json!({
                "name": "nats", "template": "nats", "bridge": "br0", "mac": "02:aa:bb:00:00:01",
                "tenant": "abcdefgh", "deploy": "d1", "start": false,
                "settings": { "url": "https://git.example/example-org" },
                "kernelAccess": { "port": 7461 },
                "destinations": [{ "address": "10.20.30.40", "port": 443 }],
                // What the node fills in from the template when the proposal is made: a machine's own system
                // image, which has no digest here, with the version it runs, and the template's content.
                "image": format!("images:debian/13 running {TEST_PG_IMAGE}"),
                "templateDigest": "ab".repeat(32),
                "registry": "https://registry.example",
            }),
            ActionKind::ServiceRemove => json!({ "name": "nats", "tenant": "abcdefgh" }),
            ActionKind::ServiceUpdate => json!({
                "name": "nats", "tenant": "abcdefgh", "timeoutSeconds": 120, "eraseData": true,
                "digest": format!("sha256:{}", "2".repeat(64)),
            }),
            ActionKind::ServiceUnpublish
            | ActionKind::ServiceImport
            | ActionKind::ServiceRelease => json!({ "name": "nats" }),
            ActionKind::TenantCreate => json!({ "id": "abcdefgh", "caps": {
                "instances": 4, "containers": 4, "virtualMachines": 1, "cpu": 8,
                "memory": "16GiB", "disk": "100GiB" } }),
            ActionKind::TenantRemove => json!({ "id": "abcdefgh" }),
            ActionKind::DeployRemove => json!({ "tenant": "abcdefgh", "deploy": "d1" }),
            ActionKind::ServicePublish => json!({ "name": "nats", "reach": "tailnet" }),
            ActionKind::ServiceConnect | ActionKind::ServiceDisconnect => {
                json!({ "from": "api", "to": "nats", "tenant": "abcdefgh" })
            }
            ActionKind::PostgresCreate => json!({ "name": "orders" }),
            ActionKind::GpuAttach | ActionKind::GpuDetach => json!({
                "project": "arcak-gpu",
                "instance": "alpha",
                "kind": "container",
                "uuid": "00000000-0000-4000-8000-0000000000a1",
                "pci": "0000:01:00.0",
            }),
            ActionKind::StorageAdd => json!({
                "name": "usb1", "device": "sdb", "disk": "sdb", "model": "Acme Portable 1T",
                "serial": "FAKE-SDB", "transport": "usb", "size": 1_000_000_000_000u64,
                "contents": "sdb1 ext4 \"PHOTOS\" 1000.0 GB", "inUseCheck": "system lists",
            }),
            ActionKind::SnapshotCreate => json!({
                "project": "shop", "instance": "web", "pool": "usb1", "kind": "container",
                "uuid": null, "name": "shop--web--20261030T1200",
            }),
            ActionKind::SnapshotDelete => json!({
                "name": "shop--web--20261030T1200", "kind": "instance", "pool": "usb1",
                "source": "shop/web",
            }),
            ActionKind::InstanceStart | ActionKind::InstanceStop | ActionKind::InstanceRestart => {
                json!({ "project": "arcak-svc", "instance": "web" })
            }
            ActionKind::FunctionCreate => json!({
                "name": "relay", "kind": "container", "concurrency": 1,
                "image": "registry.example/acme/relay",
                "digest": format!("sha256:{}", "a".repeat(64)),
                "command": ["/relay", "--once"],
                "env": { "BATCH": "500" },
                "secrets": ["DATABASE_URL"],
                "links": [{ "to": "nats", "port": 4222 }],
                "destinations": [{ "address": "10.20.30.40", "port": 443 }],
                "registry": "https://registry.example",
                "triggers": [{ "timer": { "everySeconds": 30 } }],
                "limits": { "seconds": 60, "memory": "128MiB", "cpu": 1 },
            }),
            ActionKind::FunctionRemove => json!({ "name": "relay" }),
            ActionKind::HostNetworkRestore => json!({}),
        }
    }

    for action in ActionKind::ALL {
        let prepared = action
            .prepare(&sample(action), Some(TEST_PG_IMAGE))
            .expect("the sample is valid");
        // A database made from the node's template carries every field of its verb: the kernel fills the template
        // and the image in when the proposal is made, on a node whose config names no image.
        let prepared = if action == ActionKind::PostgresCreate {
            action
                .prepare(
                    &json!({
                        "name": "orders", "template": "postgres-autopg", "image": TEST_PG_IMAGE,
                        "templateDigest": "ab".repeat(32)
                    }),
                    None,
                )
                .expect("the filled sample is valid")
        } else {
            prepared
        };
        let details = action.details(&prepared);
        if matches!(action, ActionKind::GpuAttach | ActionKind::GpuDetach) {
            let labels: Vec<&str> = details.iter().map(|row| row.label.as_str()).collect();
            assert_eq!(labels, ["Project", "Instance", "GPU", "Effect"]);
            assert_eq!(details[0].value, "arcak-gpu");
            assert_eq!(details[1].value, "alpha (container)");
            assert!(details[2].value.contains("0000:01:00.0"));
            assert!(
                details[3].value.contains("shared with every other user")
                    || details[3].value.contains("Removes")
            );
            assert_eq!(
                prepared["pci"], "0000:01:00.0",
                "the address is normalized before the digest"
            );
            continue;
        }
        if action.is_storage() {
            // Every value the owner approves is on the card in plain words; the kernel-read facts are bound in args.
            let shown: String = details
                .iter()
                .map(|row| format!("{}={};", row.label, row.value))
                .collect();
            let expected: &[&str] = match action {
                ActionKind::StorageAdd => &[
                    "usb1",
                    "/dev/sdb",
                    "Acme Portable 1T",
                    "1000.0 GB",
                    "USB",
                    "PHOTOS",
                    "Everything on this disk will be erased.",
                    "In-use check through system lists (Arca can't open the disk directly).",
                ],
                ActionKind::SnapshotCreate => &[
                    "shop",
                    "web",
                    "usb1",
                    "shop--web--20261030T1200",
                    "never starts",
                ],
                _ => &[
                    "shop--web--20261030T1200",
                    "shop/web",
                    "usb1",
                    "not touched",
                ],
            };
            for value in expected {
                assert!(
                    shown.contains(value),
                    "{action:?} card lacks {value}: {shown}"
                );
            }
            continue;
        }
        if action.is_lifecycle() {
            let labels: Vec<&str> = details.iter().map(|row| row.label.as_str()).collect();
            assert_eq!(labels, ["Project", "Instance", "Effect"]);
            assert_eq!(details[0].value, "arcak-svc");
            assert_eq!(details[1].value, "web");
            assert!(details[2].value.contains("web"), "{action:?} effect");
            assert!(
                action.params_schema()["properties"]
                    .as_object()
                    .expect("properties")
                    .keys()
                    .all(|field| ["project", "instance", "service", "tenant"]
                        .contains(&field.as_str())),
                "{action:?} has a field the card does not show"
            );
            continue;
        }
        if matches!(
            action,
            ActionKind::ServiceImport | ActionKind::ServiceRelease
        ) {
            // The one argument, and in plain words the one key the action writes or clears.
            let labels: Vec<&str> = details.iter().map(|row| row.label.as_str()).collect();
            assert_eq!(labels, ["Name", "Effect"]);
            assert_eq!(details[0].value, "nats");
            assert!(details[1].value.contains("one key"), "{action:?} effect");
            assert!(
                action.params_schema()["properties"]
                    .as_object()
                    .expect("properties")
                    .keys()
                    .all(|field| field == "name"),
                "{action:?} has a field the card does not show"
            );
            continue;
        }
        if action == ActionKind::FunctionCreate {
            // Every argument of a definition has its row, in the owner's words.
            let row = |label: &str| {
                details
                    .iter()
                    .find(|row| row.label == label)
                    .unwrap_or_else(|| panic!("the function card lacks {label}"))
                    .value
                    .as_str()
            };
            for field in action.params_schema()["properties"]
                .as_object()
                .expect("properties")
                .keys()
            {
                let (label, holds) = match field.as_str() {
                    "name" => ("Function", "relay"),
                    "kind" => ("Kind", "container"),
                    "image" => ("Program", "registry.example/acme/relay@sha256:"),
                    "digest" => ("Program", "@sha256:aaaaaaaa"),
                    "command" => ("Command", "/relay"),
                    "env" => ("Setting BATCH", "500"),
                    "secrets" => ("Secrets", "DATABASE_URL: each run is given these by name"),
                    "links" => ("Services on this node", "nats on tcp port 4222"),
                    "destinations" => ("Private network", "10.20.30.40 on tcp port 443"),
                    "triggers" => ("Runs", "every 30 seconds"),
                    "concurrency" => ("Runs", "one run at a time"),
                    "limits" => ("Limits", "at most 60 seconds, 128MiB of memory and 1 CPU"),
                    "registry" => (
                        "Registry",
                        "https://registry.example: the image is fetched with the credential",
                    ),
                    other => panic!("function_create field {other} has no owner label"),
                };
                assert!(row(label).contains(holds), "{label}: {}", row(label));
            }
            assert_eq!(row("Command argument 1"), "--once");
            assert!(row("Internet").starts_with("No:"));
            continue;
        }
        if action == ActionKind::ServiceUpdate {
            // Every argument has its row, and the erasing of the data is said in words.
            let shown: Vec<(&str, &str)> = details
                .iter()
                .map(|row| (row.label.as_str(), row.value.as_str()))
                .collect();
            assert_eq!(shown[0], ("Name", "nats"));
            assert_eq!(shown[1].0, "Digest");
            assert_eq!(shown[2], ("Health check", "120 seconds"));
            assert_eq!(
                shown[3],
                ("Erase data", arca::action_proposal::ERASE_DATA_WORDS)
            );
            assert_eq!(shown[4], ("Tenant", "abcdefgh"));
            assert_eq!(shown.len(), 5);
            let schema = action.params_schema();
            assert!(
                schema["properties"]
                    .as_object()
                    .expect("properties")
                    .keys()
                    .all(|field| [
                        "name",
                        "digest",
                        "follow",
                        "timeoutSeconds",
                        "eraseData",
                        "noNewPrivileges",
                        "tenant"
                    ]
                    .contains(&field.as_str())),
                "the schema has a field the card does not show"
            );
            continue;
        }
        if action == ActionKind::FunctionRemove {
            let labels: Vec<&str> = details.iter().map(|row| row.label.as_str()).collect();
            assert_eq!(labels, ["Function", "Effect"]);
            assert_eq!(details[0].value, "relay");
            assert!(details[1].value.contains("run log are kept"));
            continue;
        }
        if action == ActionKind::TenantCreate {
            // Every cap the owner approves is on the card, with the tenant's id.
            let shown: Vec<(&str, &str)> = details
                .iter()
                .map(|row| (row.label.as_str(), row.value.as_str()))
                .collect();
            assert_eq!(
                shown,
                [
                    ("Tenant", "abcdefgh"),
                    ("Instances", "4"),
                    ("Containers", "4"),
                    ("Virtual machines", "1"),
                    ("CPUs", "8"),
                    ("Memory", "16GiB"),
                    ("Disk", "100GiB"),
                ]
            );
            continue;
        }
        if action == ActionKind::HostNetworkRestore {
            let labels: Vec<&str> = details.iter().map(|row| row.label.as_str()).collect();
            assert_eq!(labels, ["Host", "Now", "Effect"]);
            assert_eq!(details[1].value, "read when the restore runs");
            assert!(details[2].value.contains("No container"));
            assert_eq!(
                action.summary(&prepared),
                "Put this host's forwarding and firewall rules back as its declared file says"
            );
            assert!(action.undo_words().contains("nothing to undo"));
            continue;
        }
        if action == ActionKind::DeployRemove {
            let shown: Vec<(&str, &str)> = details
                .iter()
                .map(|row| (row.label.as_str(), row.value.as_str()))
                .collect();
            assert_eq!(shown[0], ("Deploy", "d1"));
            assert_eq!(shown[1].0, "Parts");
            assert_eq!(shown[2].0, "Effect");
            assert!(shown[2].1.contains("with its data"));
            assert_eq!(shown[3], ("Tenant", "abcdefgh"));
            assert_eq!(shown.len(), 4);
            continue;
        }
        // With nothing read from the node, a tenant's removal says what it holds is read when the removal runs.
        if action == ActionKind::TenantRemove {
            let labels: Vec<&str> = details.iter().map(|row| row.label.as_str()).collect();
            assert_eq!(labels, ["Tenant", "Services", "Effect"]);
            assert_eq!(details[0].value, "abcdefgh");
            assert_eq!(details[1].value, "read when the removal runs");
            assert!(details[2].value.contains("with its data"));
            continue;
        }
        let schema = action.params_schema();
        let fields = schema["properties"].as_object().expect("properties");
        let mut seen = 0;
        for field in fields.keys() {
            // A database proposal that names a tenant is refused when it is proposed, so no card ever shows one: the
            // field is in the schema only to carry that refusal.
            if action == ActionKind::PostgresCreate && field == "tenant" {
                continue;
            }
            // The settings a create replaces have one row each, named for the setting, with its value.
            if action == ActionKind::ServiceCreate && field == "settings" {
                for (key, value) in prepared["settings"].as_object().expect("settings") {
                    let row = details
                        .iter()
                        .find(|row| row.label == format!("Setting {key}"))
                        .unwrap_or_else(|| panic!("{action:?} details lack the setting {key}"));
                    assert_eq!(row.value, value.as_str().expect("a string setting"));
                    seen += 1;
                }
                continue;
            }
            let label = match (action, field.as_str()) {
                (_, "tenant") => "Tenant",
                (_, "deploy") => "Deploy",
                (ActionKind::PostgresCreate, "name") => "Database",
                (_, "name") => "Name",
                (_, "template") => "Template",
                (_, "reach") => "Reach",
                (_, "from") => "Service",
                (_, "to") => "To",
                (_, "image") => "Image",
                (_, "bridge") => "Host bridge",
                (_, "mac") => "MAC address",
                (ActionKind::ServiceCreate, "start") => "Start",
                (ActionKind::ServiceCreate, "kernelAccess") => "Kernel access",
                (ActionKind::ServiceCreate, "destinations") => "Private network",
                (_, "templateDigest") => "Template content",
                (ActionKind::ServiceCreate, "registry") => "Registry",
                _ => panic!("{action:?} field {field} has no owner label"),
            };
            let row = details
                .iter()
                .find(|row| row.label == label)
                .unwrap_or_else(|| panic!("{action:?} details lack {label}"));
            // The registry is said in a sentence that names it: where its credential then rests.
            if field == "registry" {
                assert!(
                    row.value.starts_with(
                        "https://registry.example: the image is fetched with the credential"
                    ) && row
                        .value
                        .contains("whoever may use Incus on this machine can read it there"),
                    "{}",
                    row.value
                );
                seen += 1;
                continue;
            }
            // The fields that are not strings are shown in words.
            let access: String;
            let expected = match &prepared[field.as_str()] {
                Value::Bool(start) => arca::action_proposal::start_words(*start),
                stated @ Value::Array(_) => {
                    access = arca::action_proposal::destinations_words(stated);
                    assert_eq!(
                        access,
                        "Yes: this service, and everything that runs in it, can reach 10.20.30.40 on tcp port 443 through this node, and is seen there as this node itself: whatever trusts this node's address there trusts everything in this service. Every other private address stays closed."
                    );
                    access.as_str()
                }
                Value::Object(stated) => {
                    access = arca::action_proposal::kernel_access_words(
                        stated["port"].as_u64().expect("a port"),
                    );
                    access.as_str()
                }
                // A create's image is shown with what its name leaves open, and the version a machine runs.
                Value::String(image) if action == ActionKind::ServiceCreate && field == "image" => {
                    access = arca::action_proposal::image_words(image);
                    assert_eq!(
                        access,
                        format!(
                            "images:debian/13 (no digest: Incus takes what that name gives when the service is created), running {TEST_PG_IMAGE}"
                        )
                    );
                    access.as_str()
                }
                // The template's content is shown whole, with what it means, and it is the card's last row.
                Value::String(digest) if field == "templateDigest" => {
                    access = arca::action_proposal::template_content_words(digest);
                    assert_eq!(
                        access,
                        format!(
                            "{}. Arca creates this only if the template on the node is still exactly this one.",
                            "ab".repeat(32)
                        )
                    );
                    assert_eq!(details.last().map(|row| row.label.as_str()), Some(label));
                    access.as_str()
                }
                value => value.as_str().expect("a string field"),
            };
            assert_eq!(row.value, expected, "{action:?} {label}");
            seen += 1;
        }
        // The card of a template database also says, in words, what the owner gets and which verbs reach it.
        if action == ActionKind::PostgresCreate {
            let made = details
                .iter()
                .find(|row| row.label == "Made as")
                .expect("its row");
            assert_eq!(
                made.value,
                arca::action_proposal::database_service_words("postgres-autopg", false)
            );
            seen += 1;
        }
        assert_eq!(seen, details.len(), "{action:?} has an extra detail row");
    }
    let traversal = ActionKind::ServiceCreate
        .prepare(&json!({ "name": "nats", "template": "../outside" }), None);
    assert!(
        traversal.is_err(),
        "the template field is constrained to the catalog's name grammar"
    );
}

#[test]
fn a_proposed_postgres_create_uses_only_the_configured_pinned_image() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("proposal-postgres", &key);
    let proposed = rig
        .propose("postgres_create", json!({ "name": "orders" }))
        .expect("the default image is filled in");
    assert_eq!(proposed["args"]["image"], json!(TEST_PG_IMAGE));
    assert_eq!(
        proposed["details"],
        json!([
            { "label": "Database", "value": "orders" },
            { "label": "Image", "value": TEST_PG_IMAGE }
        ])
    );
    let foreign = "docker.io/library/postgres@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let error = rig
        .propose(
            "postgres_create",
            json!({ "name": "other", "image": foreign }),
        )
        .expect_err("a proposal cannot choose an image");
    assert!(matches!(error, CallError::Invalid(_)));
    assert_eq!(
        rig.list()["plans"].as_array().map(Vec::len),
        Some(1),
        "the refused image recorded nothing"
    );

    let no_default = Rig::with_image("proposal-postgres-none", &key, None);
    let error = no_default
        .propose("postgres_create", json!({ "name": "orders" }))
        .expect_err("no configured image");
    assert!(matches!(error, CallError::Invalid(_)));
    // Only the node names the template and its image, from its own template: a proposal that states them by hand
    // is not recorded, so no card ever shows an image the node did not read.
    let error = no_default
        .propose(
            "postgres_create",
            json!({ "name": "orders", "template": "postgres-autopg", "image": foreign }),
        )
        .expect_err("a hand-made template proposal");
    assert!(matches!(error, CallError::Invalid(_)));
    assert_eq!(no_default.list()["plans"].as_array().map(Vec::len), Some(0));

    let unpinned = Rig::with_image(
        "proposal-postgres-unpinned",
        &key,
        Some("docker.io/library/postgres:18"),
    );
    let error = unpinned
        .propose("postgres_create", json!({ "name": "orders" }))
        .expect_err("an unpinned default is refused");
    assert!(matches!(error, CallError::Invalid(_)));
}

/// An MCP door over `rig`'s `action_propose`, for one viewer token: the runtime that serves it, its socket and the
/// token's secret.
fn propose_door(rig: &Rig) -> (Runtime, PathBuf, String) {
    let tokens_dir = rig._scratch.path("mcp-tokens");
    std::fs::create_dir(&tokens_dir).expect("the token directory");
    std::fs::set_permissions(&tokens_dir, std::fs::Permissions::from_mode(0o700))
        .expect("mode 0700");
    let (secret, hash) = mint(Scope::Viewer);
    let hash_path = tokens_dir.join("viewer.hash");
    std::fs::write(&hash_path, hash).expect("the hash file");
    std::fs::set_permissions(&hash_path, std::fs::Permissions::from_mode(0o400))
        .expect("mode 0400");
    let tokens = TokenStore::load(&tokens_dir).expect("the token store loads");
    let server = Server::new(arca_api::verbs(), tokens).handle(
        "action_propose",
        Arc::clone(&rig.handlers["action_propose"]),
    );
    let socket = rig._scratch.path("mcp.sock");
    let runtime = Runtime::new().expect("a runtime");
    let served = socket.clone();
    runtime.spawn(async move {
        let _ = serve_unix(server.router(), &served).await;
    });
    let until = Instant::now() + Duration::from_secs(5);
    while !socket.exists() {
        assert!(Instant::now() < until, "the MCP socket started");
        std::thread::sleep(Duration::from_millis(10));
    }
    (runtime, socket, secret)
}

/// One `action_propose` call through the door with `arguments`, and the JSON-RPC reply. The door answers 200.
fn propose_through(socket: &Path, secret: &str, arguments: Value) -> Value {
    let call = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": "action_propose", "arguments": arguments }
    });
    let body = serde_json::to_vec(&call).expect("the call");
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {secret}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut stream = UnixStream::connect(socket).expect("connect to MCP");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    stream.write_all(request.as_bytes()).expect("the head");
    stream.write_all(&body).expect("the body");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("the answer");
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("a response head");
    assert!(
        String::from_utf8_lossy(&raw[..split]).starts_with("HTTP/1.1 200"),
        "the MCP door answers 200"
    );
    serde_json::from_slice(&raw[split + 4..]).expect("JSON-RPC")
}

#[test]
fn action_propose_succeeds_through_the_mcp_door() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("proposal-mcp", &key);
    let (_runtime, socket, secret) = propose_door(&rig);
    let reply = propose_through(
        &socket,
        &secret,
        json!({
            "action": "service_create",
            "args": { "name": "nats", "template": "nats" }
        }),
    );
    assert_eq!(reply["result"]["isError"], json!(false), "{reply}");
    let proposal = &reply["result"]["structuredContent"];
    assert_eq!(proposal["kind"], json!("action"));
    assert_eq!(proposal["action"], json!("service_create"));
    assert_eq!(
        proposal["summary"],
        json!("Create the service nats from template nats")
    );
    assert_eq!(proposal["args"]["template"], json!("nats"));
    assert!(
        proposal["details"]
            .as_array()
            .is_some_and(|rows| rows.len() == 2),
        "the owner sees both arguments"
    );
    assert!(
        proposal["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("act-") && id.len() == 16)
    );
    let texts: Vec<&str> = reply["result"]["content"]
        .as_array()
        .expect("content blocks")
        .iter()
        .filter_map(|block| block["text"].as_str())
        .collect();
    let next = proposal["nextStep"].as_str().expect("the next step");
    assert!(
        texts.iter().any(|text| text.ends_with(next)),
        "the text a model reads carries the next step: {texts:?}"
    );
}

/// A node config whose approval page is served at `origin`, or that serves none.
fn node_with_page(origin: Option<&str>) -> arca::config::NodeConfig {
    let mut config: arca::config::NodeConfig = serde_json::from_value(json!({
        "stateDir": "/nonexistent/state",
        "incus": { "socket": "/nonexistent/incus.sock", "pools": [] },
        "listen": { "unix": "/nonexistent/arca.sock" },
        "tokens": { "hashDir": "/nonexistent/tokens" },
        "kernel": { "droiddProjects": ["droid"] }
    }))
    .expect("the fixture config parses");
    config.approval_origin = origin.map(str::to_string);
    config
}

/// The served `action_propose` of `rig`'s kernel and clock, on a node whose config is `node`.
fn served_propose(rig: &Rig, node: arca::config::NodeConfig) -> Arc<dyn ToolHandler> {
    let clock = Arc::clone(&rig.clock);
    let handlers: std::collections::HashMap<&'static str, Arc<dyn ToolHandler>> =
        arca::handlers::plan_handlers_served(
            Arc::clone(&rig.kernel),
            Arc::new(move || clock.load(Ordering::SeqCst)),
            Some(TEST_PG_IMAGE.to_string()),
            None,
            None,
            None,
            arca::handlers::ActionSlot::default(),
            Some(Arc::new(node)),
        )
        .into_iter()
        .collect();
    Arc::clone(&handlers["action_propose"])
}

#[test]
fn a_proposal_says_where_to_approve_it_when_it_expires_and_how_to_enrol_a_passkey() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("proposal-next-step", &key);
    let until = "The card expires at 12:15 UTC on 2026-10-30, 15 minutes after it was proposed; after that it must be proposed again.";

    let nowhere = rig
        .propose("service_create", create_args("nats"))
        .expect("proposed");
    assert_eq!(nowhere["passkeysTrusted"], json!(0));
    assert_eq!(nowhere.get("approvalUrl"), None);
    assert_eq!(
        nowhere["nextStep"],
        json!(format!(
            "This node has no approval page, so nothing can be approved here: the guide's section \"The approval page\" says how to give it one. {until}"
        ))
    );

    let local = served_propose(&rig, node_with_page(Some("http://localhost:8443")));
    let caller = Caller {
        token_id: "viewer".to_string(),
        scope: Scope::Viewer,
        tools: None,
    };
    // A removal: a create on a node with a config reads its template from the node's state, which this one has none of.
    let remove = |name: &str| json!({ "action": "service_remove", "args": { "name": name } });
    let enrol = local.call(remove("redis"), &caller).expect("proposed");
    assert_eq!(enrol["approvalUrl"], json!("http://localhost:8443/approve"));
    assert_eq!(enrol["passkeysTrusted"], json!(0));
    assert_eq!(
        enrol["nextStep"],
        json!(format!(
            "No passkey is trusted on this node yet, so nothing can be approved until the owner enrols one: open http://localhost:8443/approve and press \"Get a setup code\", or, on a machine with no screen, print the code there with arca setup-code and type it on the page. Then approve it at http://localhost:8443/approve. {until} The page is on this machine only: from another computer, open it through ssh -o ExitOnForwardFailure=yes -L localhost:8443:127.0.0.1:8443 and this machine's name or address."
        ))
    );

    let passkey = webauthn::SoftPasskey::from_seed(7);
    rig.kernel.lock().expect("lock").replace_owner_passkeys(
        arca_approval::parse_passkeys(passkey.line(NAMESPACE).as_bytes())
            .expect("the passkey line"),
    );
    let public = served_propose(&rig, node_with_page(Some("https://arca.example.org")));
    let approve = public.call(remove("cache"), &caller).expect("proposed");
    assert_eq!(
        approve["approvalUrl"],
        json!("https://arca.example.org/approve")
    );
    assert_eq!(approve["passkeysTrusted"], json!(1));
    assert_eq!(
        approve["nextStep"],
        json!(format!(
            "The owner approves it at https://arca.example.org/approve. {until}"
        ))
    );
}

#[test]
fn a_proposal_on_a_node_that_trusts_another_page_hosts_keys_says_it_is_approved_there() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("proposal-next-step-linked", &key);
    let until = "The card expires at 12:15 UTC on 2026-10-30, 15 minutes after it was proposed; after that it must be proposed again.";
    let caller = Caller {
        token_id: "viewer".to_string(),
        scope: Scope::Viewer,
        tools: None,
    };
    let remove = |name: &str| json!({ "action": "service_remove", "args": { "name": name } });
    let linked = |origin: Option<&str>| {
        let mut node = node_with_page(origin);
        node.approval = Some(arca::config::ApprovalConfig {
            trust_owner_keys_from: vec!["/nonexistent/owner-passkeys".into()],
            origins: vec!["https://page.example.org".to_string()],
            ..Default::default()
        });
        node
    };
    let said = format!(
        "This node trusts the passkeys of another page host, so the owner approves it on that host's approval page, where this card is approved and relayed here; no passkey is needed on this node. {until}"
    );

    // Without a page of its own, and with a page of its own but no passkey of its own: both are approved on the page host.
    let no_page = served_propose(&rig, linked(None))
        .call(remove("redis"), &caller)
        .expect("proposed");
    assert_eq!(no_page["passkeysTrusted"], json!(0));
    assert_eq!(no_page.get("approvalUrl"), None);
    assert_eq!(no_page["nextStep"], json!(said));
    let own_page = served_propose(&rig, linked(Some("http://localhost:8443")))
        .call(remove("cache"), &caller)
        .expect("proposed");
    assert_eq!(own_page["nextStep"], json!(said));

    // The same node with a passkey of its own is approved at its own page, as any node is.
    let passkey = webauthn::SoftPasskey::from_seed(7);
    rig.kernel.lock().expect("lock").replace_owner_passkeys(
        arca_approval::parse_passkeys(passkey.line(NAMESPACE).as_bytes())
            .expect("the passkey line"),
    );
    let own = served_propose(&rig, linked(Some("https://arca.example.org")))
        .call(remove("db"), &caller)
        .expect("proposed");
    assert_eq!(
        own["nextStep"],
        json!(format!(
            "The owner approves it at https://arca.example.org/approve. {until}"
        ))
    );
}

#[test]
fn a_proposal_on_a_public_page_with_no_passkey_says_to_enrol_one_without_an_ssh_line() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("proposal-next-step-https", &key);
    let until = "The card expires at 12:15 UTC on 2026-10-30, 15 minutes after it was proposed; after that it must be proposed again.";
    let caller = Caller {
        token_id: "viewer".to_string(),
        scope: Scope::Viewer,
        tools: None,
    };
    let answer = served_propose(&rig, node_with_page(Some("https://arca.example.org")))
        .call(
            json!({ "action": "service_remove", "args": { "name": "redis" } }),
            &caller,
        )
        .expect("proposed");
    assert_eq!(answer["passkeysTrusted"], json!(0));
    assert_eq!(
        answer["nextStep"],
        json!(format!(
            "No passkey is trusted on this node yet, so nothing can be approved until the owner enrols one: open https://arca.example.org/approve and press \"Get a setup code\", or, on a machine with no screen, print the code there with arca setup-code and type it on the page. Then approve it at https://arca.example.org/approve. {until}"
        ))
    );
}

/// The proposal window's refusal: the code it has always had, its sentence, and the wait beside the code, checked
/// against the document's list. Returns the wait in seconds.
fn rate_limited(result: Result<Value, CallError>) -> u64 {
    let (code, message, fields) = common::listed_refusal(result);
    assert_eq!(code, "RateLimited");
    assert_eq!(
        message,
        "too many proposals from this token; try again in ten minutes"
    );
    fields["retryAfterSeconds"]
        .as_u64()
        .expect("the wait is told")
}

#[test]
fn the_per_token_proposal_rate_holds_across_propose_and_decline() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("proposal-rate", &key);
    for index in 0..PROPOSAL_RATE_MAX {
        let proposed = rig
            .propose("service_create", create_args(&format!("rate{index}")))
            .expect("a proposal inside the window");
        let id = proposed["id"].as_str().expect("an id");
        call_verb(&rig.handlers, "plan_decline", json!({ "plan": id }))
            .expect("the proposal is declined");
    }
    // The sentence rounds the wait up to the whole window. The field beside the code is exact: a minute after the
    // thirty proposals, nine minutes are left until the first of them leaves the window.
    rig.at(NOW + 60_000);
    assert_eq!(
        rate_limited(rig.propose("service_create", create_args("rate-extra"))),
        540,
        "the token's window is full"
    );
}

#[test]
fn a_deduplicated_proposal_does_not_consume_the_token_window() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("proposal-rate-dedupe", &key);
    let first = rig
        .propose("service_create", create_args("nats"))
        .expect("the first new proposal");
    for _ in 0..PROPOSAL_RATE_MAX * 2 {
        assert_eq!(
            rig.propose("service_create", create_args("nats"))
                .expect("the duplicate still answers"),
            first,
            "a deduplicated proposal returns the waiting record"
        );
    }
    // The duplicates counted nothing: one new proposal plus twenty-nine more still fit the window.
    for index in 0..PROPOSAL_RATE_MAX - 1 {
        let proposed = rig
            .propose("service_create", create_args(&format!("new{index}")))
            .expect("a new proposal inside the window");
        let id = proposed["id"].as_str().expect("an id");
        call_verb(&rig.handlers, "plan_decline", json!({ "plan": id }))
            .expect("the proposal is declined");
    }
    assert_eq!(
        rate_limited(rig.propose("service_create", create_args("new-extra"))),
        600,
        "the thirty accepted proposals fill the window"
    );
    assert_eq!(
        rig.propose("service_create", create_args("nats"))
            .expect("a duplicate answers even with a full window"),
        first,
        "a duplicate records nothing, so the full window does not refuse it"
    );
}

#[test]
fn a_refused_proposal_does_not_consume_the_token_window() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("proposal-rate-refused", &key);
    let mut waiting = Vec::new();
    for index in 0..MAX_WAITING {
        let proposed = rig
            .propose("service_create", create_args(&format!("busy{index}")))
            .expect("a proposal inside the window");
        waiting.push(proposed["id"].as_str().expect("an id").to_string());
    }
    // Forty refusals for a full waiting set. If they counted, this loop would turn into RateLimited well before its end.
    for index in 0..40 {
        let error = rig
            .propose("service_create", create_args(&format!("overflow{index}")))
            .expect_err("a ninth proposal waits for room");
        assert!(
            matches!(error, CallError::Refused { ref code, .. } if code == "TooManyProposals"),
            "a refusal, not a rate limit: {error:?}"
        );
    }
    for id in &waiting {
        call_verb(&rig.handlers, "plan_decline", json!({ "plan": id }))
            .expect("the proposal is declined");
    }
    // Eight accepted proposals are already counted, so twenty-two more fill the window exactly.
    for index in 0..PROPOSAL_RATE_MAX - MAX_WAITING {
        let proposed = rig
            .propose("service_create", create_args(&format!("spent{index}")))
            .expect("a new proposal inside the window");
        let id = proposed["id"].as_str().expect("an id");
        call_verb(&rig.handlers, "plan_decline", json!({ "plan": id }))
            .expect("the proposal is declined");
    }
    assert_eq!(
        rate_limited(rig.propose("service_create", create_args("spent-extra"))),
        600,
        "the refusals counted nothing, so thirty new proposals fill the window"
    );
}

#[test]
fn a_disallowed_action_is_refused_and_records_nothing() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("proposal-disallowed", &key);
    for action in [
        "service_secret_set",
        "reboot",
        "install",
        "postgres_delete",
        "",
    ] {
        let error = rig
            .propose(action, json!({ "name": "nats" }))
            .expect_err("not on the list");
        assert!(matches!(error, CallError::Invalid(_)), "{action}");
    }
    assert_eq!(rig.list()["plans"], json!([]));
}

#[test]
fn the_same_digest_returns_the_same_record_and_a_ninth_is_refused() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("proposal-limit", &key);
    let first = rig
        .propose("service_create", create_args("nats"))
        .expect("the first");
    let again = rig
        .propose("service_create", create_args("nats"))
        .expect("the duplicate");
    assert_eq!(first, again, "the same digest returns the existing record");
    for index in 1..MAX_WAITING {
        rig.propose("service_create", create_args(&format!("svc{index}")))
            .expect("a distinct proposal");
    }
    assert_eq!(
        rig.list()["plans"].as_array().map(Vec::len),
        Some(MAX_WAITING)
    );
    let ninth = rig
        .propose("service_create", create_args("ninth"))
        .expect_err("the limit is eight");
    // The code it has always had, its sentence, and no field: the document lists it in the family `rate`.
    assert_eq!(
        common::listed_refusal(Err(ninth)),
        (
            "TooManyProposals".to_string(),
            "eight action proposals already wait; approve or decline one first".to_string(),
            json!({})
        )
    );
    assert_eq!(
        common::listed_refusal_entry("TooManyProposals").expect("listed")["family"],
        "rate"
    );
    assert_eq!(
        rig.propose("service_create", create_args("nats"))
            .expect("the dedupe still answers at the limit"),
        first
    );
}

#[test]
fn a_lapsed_proposal_is_left_out_and_a_fresh_one_fits() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("proposal-lapse", &key);
    for index in 0..MAX_WAITING {
        rig.propose("service_create", create_args(&format!("svc{index}")))
            .expect("a proposal");
    }
    rig.at(NOW + PLAN_TTL_MS + 1);
    assert_eq!(rig.list()["plans"], json!([]), "all eight lapsed");
    rig.propose("service_create", create_args("fresh"))
        .expect("the lapse made room");
}

#[test]
fn plan_decline_accepts_an_action_id_and_removes_it() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("proposal-decline", &key);
    let proposed = rig
        .propose("service_create", create_args("nats"))
        .expect("a proposal");
    let id = proposed["id"].as_str().expect("an id");
    let declined = call_verb(&rig.handlers, "plan_decline", json!({ "plan": id }))
        .expect("plan_decline declines an action proposal");
    assert_eq!(declined["plan"], json!(id));
    assert_eq!(declined["retained"], json!([]));
    assert_eq!(rig.list()["plans"], json!([]));
    let again = call_verb(&rig.handlers, "plan_decline", json!({ "plan": id }))
        .expect_err("a second decline finds nothing");
    assert!(matches!(
        again,
        CallError::Refused { ref code, .. } if code == "NoSuchPlan"
    ));
}

#[test]
fn a_valid_owner_approval_runs_once_and_a_replay_does_not_run_again() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("proposal-once", &key);
    let proposed = rig
        .propose("service_create", create_args("nats"))
        .expect("a proposal");
    let id = proposed["id"].as_str().expect("an id");
    let digest = proposed["argsDigest"].as_str().expect("a digest");
    let statement = statement(HOST, id, "service_create", digest, &"a".repeat(32));
    let approval = approve_statement(&key, &statement);
    let approved = rig
        .approve(id, &approval, NOW)
        .expect("the approval verifies");
    assert_eq!(approved.id, id);
    rig.executor
        .run(&approved.action, &approved.args, &approved.grant)
        .expect("the action runs");
    assert_eq!(rig.runs.load(Ordering::SeqCst), 1);
    rig.kernel
        .lock()
        .expect("lock")
        .journal_action_result(&approved, true, None, None);
    let journal = std::fs::read_to_string(rig._scratch.path(ACTION_PROPOSALS_FILE))
        .expect("the proposals journal");
    let events: Vec<Value> = journal
        .lines()
        .map(|line| serde_json::from_str(line).expect("an event"))
        .collect();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0]["event"], json!("proposed"));
    assert_eq!(events[1]["event"], json!("approved"));
    assert_eq!(events[2]["event"], json!("result"));
    assert_eq!(events[2]["ok"], json!(true));
    assert_eq!(rig.list()["plans"], json!([]), "the proposal is retired");

    let replay = rig
        .approve(id, &approval, NOW)
        .expect_err("the replay is refused");
    match replay {
        CallError::Refused { code, .. } => assert_eq!(code, "NoSuchPlan"),
        other => panic!("a refusal was expected, got {other:?}"),
    }
    assert_eq!(rig.runs.load(Ordering::SeqCst), 1, "still one run");
}

#[test]
fn an_expired_proposal_is_refused_before_its_approval_is_checked() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("proposal-expired", &key);
    let proposed = rig
        .propose("service_create", create_args("nats"))
        .expect("a proposal");
    let id = proposed["id"].as_str().expect("an id");
    let digest = proposed["argsDigest"].as_str().expect("a digest");
    let approval = approve_statement(
        &key,
        &statement(HOST, id, "service_create", digest, &"b".repeat(32)),
    );
    let error = rig
        .approve(id, &approval, NOW + PLAN_TTL_MS + 1)
        .expect_err("the proposal lapsed");
    match error {
        CallError::Refused { code, .. } => assert_eq!(code, "Expired"),
        other => panic!("a refusal was expected, got {other:?}"),
    }
}

#[test]
fn tampered_arguments_or_another_id_are_wrong_params_and_wrong_target() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("proposal-tamper", &key);
    let proposed = rig
        .propose("service_create", create_args("nats"))
        .expect("a proposal");
    let id = proposed["id"].as_str().expect("an id");
    let digest = proposed["argsDigest"].as_str().expect("a digest");

    let other_digest = "c".repeat(64);
    let tampered = approve_statement(
        &key,
        &statement(HOST, id, "service_create", &other_digest, &"d".repeat(32)),
    );
    let error = rig
        .approve(id, &tampered, NOW)
        .expect_err("the digest differs");
    assert!(matches!(
        error,
        CallError::Refused { ref code, .. } if code == "WrongParams"
    ));

    let wrong_id = "act-000000000000";
    let other_target = approve_statement(
        &key,
        &statement(HOST, wrong_id, "service_create", digest, &"e".repeat(32)),
    );
    let error = rig
        .approve(id, &other_target, NOW)
        .expect_err("the target differs");
    assert!(matches!(
        error,
        CallError::Refused { ref code, .. } if code == "WrongTarget"
    ));
    assert!(
        rig.list()["plans"]
            .as_array()
            .is_some_and(|plans| plans.len() == 1)
    );
}

#[test]
fn the_action_statement_names_the_verb_the_id_and_the_args_digest() {
    let statement = statement(
        HOST,
        "act-0123456789ab",
        "service_publish",
        &"f".repeat(64),
        &"1".repeat(32),
    );
    let parsed = parse_statement(statement.as_bytes()).expect("the kernel parser reads it");
    assert_eq!(parsed.action, ArcaAction::ServicePublish);
    assert_eq!(parsed.action.as_str(), "service_publish");
    assert_eq!(parsed.target, "act-0123456789ab");
    assert_eq!(parsed.params_digest, "f".repeat(64));
    assert!(!statement.ends_with('\n'));

    let text = arca::handlers::action_statement(
        HOST,
        "act-0123456789ab",
        "service_publish",
        &"f".repeat(64),
        NOW,
        &"1".repeat(32),
    )
    .expect("a statement");
    assert!(text.starts_with(r#"{"action":"service_publish","expiresAt":""#));
    assert!(text.ends_with(r#""target":"act-0123456789ab"}"#));
}

#[test]
fn a_statement_with_an_unlisted_action_does_not_parse() {
    let text = statement(
        HOST,
        "act-0123456789ab",
        "service_create",
        &"f".repeat(64),
        &"1".repeat(32),
    )
    .replace("service_create", "reboot");
    assert!(matches!(
        parse_statement(text.as_bytes()),
        Err(ApprovalRefusal::Malformed(_))
    ));
}

/// Marks `pci` attached in the fake backend, as an earlier approved action would have.
fn mark_attached(rig: &Rig, name: &str, pci: &str) {
    let instance = gpu_instance(name, "arcak-gpu", "container");
    arca::gpu::GpuBackend::attach(&*rig.gpu, &instance, pci)
        .expect("the fake marks the device attached");
}

#[test]
fn two_identical_gpus_stay_apart_in_proposals_and_only_an_approval_runs_one() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("proposal-gpu", &key);
    let second = rig
        .propose(
            "gpu_attach",
            json!({ "instance": "alpha", "pci": "0000:02:00.0" }),
        )
        .expect("the second card is in the inventory");
    assert_eq!(second["action"], json!("gpu_attach"));
    assert_eq!(second["args"]["project"], json!("arcak-gpu"));
    assert_eq!(second["args"]["kind"], json!("container"));
    assert!(
        second["args"]["uuid"].is_string(),
        "the instance identity is bound into the digest"
    );
    assert_eq!(
        second["details"],
        json!([
            { "label": "Project", "value": "arcak-gpu" },
            { "label": "Instance", "value": "alpha (container)" },
            {
                "label": "GPU",
                "value": "NVIDIA GeForce RTX 4060 Ti (0000:02:00.0)"
            },
            {
                "label": "Effect",
                "value": "Adds the card to alpha as a physical GPU device. Its memory is shared with every other user of the card, not isolated."
            }
        ])
    );
    assert!(
        second["details"][3]["value"]
            .as_str()
            .is_some_and(
                |effect| effect.contains("not isolated") && !effect.contains("isolated VRAM")
            ),
        "two users of one card are never described as isolated VRAM"
    );
    assert!(
        second["summary"]
            .as_str()
            .is_some_and(|summary| summary.contains("arcak-gpu")),
        "the summary names the bound project"
    );
    let first = rig
        .propose(
            "gpu_attach",
            json!({ "instance": "alpha", "pci": "0000:01:00.0" }),
        )
        .expect("the first card is in the inventory");
    assert_ne!(second["id"], first["id"], "the cards are two proposals");
    assert!(
        first["details"][2]["value"]
            .as_str()
            .is_some_and(|gpu| gpu.ends_with("(0000:01:00.0)"))
    );

    // A direct call without the one-shot grant changes nothing and is refused, even with the resolved fields.
    let direct = call_verb(
        &rig.handlers,
        "gpu_attach",
        json!({
            "project": "arcak-gpu", "instance": "alpha", "kind": "container",
            "uuid": second["args"]["uuid"], "pci": "0000:02:00.0", "approval": "not-a-grant"
        }),
    )
    .expect_err("a direct call has no owner approval");
    assert!(matches!(
        direct,
        CallError::Refused { ref code, .. } if code == "NotApproved"
    ));
    assert!(rig.gpu.calls().is_empty(), "nothing was attached");

    // The owner's approval runs exactly the card the proposal names, once.
    let id = second["id"].as_str().expect("an id");
    let digest = second["argsDigest"].as_str().expect("a digest");
    let statement = statement(HOST, id, "gpu_attach", digest, &"a".repeat(32));
    let approval = approve_statement(&key, &statement);
    let approved = rig
        .approve(id, &approval, NOW)
        .expect("the approval verifies");
    let result = rig
        .executor
        .run(&approved.action, &approved.args, &approved.grant)
        .expect("the approved attach runs");
    assert_eq!(result["instance"], json!("alpha"));
    assert_eq!(result["project"], json!("arcak-gpu"));
    assert_eq!(result["pci"], json!("0000:02:00.0"));
    assert_eq!(result["attached"], json!(true));
    assert_eq!(
        rig.gpu.calls(),
        vec![arca::gpu::GpuCall::Attach {
            project: "arcak-gpu".to_string(),
            instance: "alpha".to_string(),
            pci: "0000:02:00.0".to_string(),
        }],
        "the first card was never confused with the second"
    );
    let replay = rig
        .executor
        .run(&approved.action, &approved.args, &approved.grant)
        .expect_err("the grant was consumed");
    assert!(matches!(
        replay,
        CallError::Refused { ref code, .. } if code == "NotApproved"
    ));
    assert_eq!(rig.gpu.calls().len(), 1, "the replay attached nothing");
}

#[test]
fn a_gpu_proposal_binds_identity_and_execution_refuses_a_recreated_instance() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("proposal-gpu-identity", &key);
    let proposed = rig
        .propose(
            "gpu_attach",
            json!({ "instance": "alpha", "pci": "0000:02:00.0" }),
        )
        .expect("a proposal");
    let uuid = proposed["args"]["uuid"].as_str().expect("a uuid");
    assert_eq!(proposed["details"][0]["value"], json!("arcak-gpu"));

    // The same name in the same project, but a recreated instance: its volatile.uuid changed.
    let mut recreated = gpu_instance("alpha", "arcak-gpu", "container");
    recreated.uuid = Some("00000000-0000-4000-8000-00000000ffff".to_string());
    rig.gpu.set_instances(vec![recreated]);
    let id = proposed["id"].as_str().expect("an id");
    let digest = proposed["argsDigest"].as_str().expect("a digest");
    let approval = approve_statement(
        &key,
        &statement(HOST, id, "gpu_attach", digest, &"c".repeat(32)),
    );
    let approved = rig.approve(id, &approval, NOW).expect("the approval");
    let refused = rig
        .executor
        .run(&approved.action, &approved.args, &approved.grant)
        .expect_err("the recreated instance is refused");
    assert!(
        matches!(refused, CallError::Refused { ref code, .. } if code == "GpuInstanceChanged"),
        "{refused:?}"
    );
    assert!(rig.gpu.calls().is_empty(), "nothing was attached");
    assert_eq!(
        approved.args["uuid"],
        json!(uuid),
        "the old uuid is still bound"
    );
}

#[test]
fn an_instance_outside_the_kernel_family_is_refused() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("proposal-gpu-family", &key);
    for name in ["foreign", "benchy"] {
        let refused = rig
            .propose(
                "gpu_attach",
                json!({ "instance": name, "pci": "0000:02:00.0" }),
            )
            .expect_err("an instance outside the family is refused");
        assert!(
            matches!(refused, CallError::Refused { ref code, .. } if code == "GpuProjectRefused"),
            "{refused:?}"
        );
    }
}

#[test]
fn an_unknown_or_vfio_card_is_refused_for_attach() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("proposal-gpu-refuse", &key);
    let unknown = rig
        .propose(
            "gpu_attach",
            json!({ "instance": "alpha", "pci": "0000:03:00.0" }),
        )
        .expect_err("the card is not in the inventory");
    assert!(matches!(
        unknown,
        CallError::Refused { ref code, .. } if code == "GpuNotInInventory"
    ));

    let mut bound = arca_metrics::GpuCard::new("0000:02:00.0", "NVIDIA GeForce RTX 4060 Ti");
    bound.usage = arca_metrics::GpuUse::VfioPci;
    bound.driver = Some("vfio-pci".to_string());
    rig.gpu.set_cards(vec![bound]);
    let container = rig
        .propose(
            "gpu_attach",
            json!({ "instance": "alpha", "pci": "0000:02:00.0" }),
        )
        .expect_err("a container cannot take a vfio-pci card");
    assert!(matches!(
        container,
        CallError::Refused { ref code, .. } if code == "GpuBoundToVfio"
    ));

    // A virtual machine is exactly the case vfio-pci exists for, so the same card is allowed there.
    rig.gpu
        .set_instances(vec![gpu_instance("alpha", "arcak-gpu", "virtual-machine")]);
    rig.propose(
        "gpu_attach",
        json!({ "instance": "alpha", "pci": "0000:02:00.0" }),
    )
    .expect("a VM may take a vfio-pci card");
}

#[test]
fn a_vm_attach_refuses_an_in_use_nvidia_card_and_states_the_whole_card_effect() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("proposal-gpu-vm", &key);
    rig.gpu
        .set_instances(vec![gpu_instance("alpha", "arcak-gpu", "virtual-machine")]);
    rig.gpu.set_in_use("0000:02:00.0", Some(true));
    let in_use = rig
        .propose(
            "gpu_attach",
            json!({ "instance": "alpha", "pci": "0000:02:00.0" }),
        )
        .expect_err("a VM may not take an in-use nvidia card");
    assert!(matches!(
        in_use,
        CallError::Refused { ref code, .. } if code == "GpuInUse"
    ));

    rig.gpu.set_in_use("0000:02:00.0", Some(false));
    let proposed = rig
        .propose(
            "gpu_attach",
            json!({ "instance": "alpha", "pci": "0000:02:00.0" }),
        )
        .expect("a free card is proposed for the VM");
    assert_eq!(
        proposed["details"][1],
        json!({"label":"Instance","value":"alpha (virtual-machine)"})
    );
    assert_eq!(
        proposed["details"][3]["value"],
        json!(
            "alpha takes the whole card while it runs; the host and every container lose it until alpha stops."
        )
    );
}

#[test]
fn a_vm_attach_refuses_an_unknown_in_use_card_at_proposal_time() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("proposal-gpu-unknown", &key);
    rig.gpu
        .set_instances(vec![gpu_instance("alpha", "arcak-gpu", "virtual-machine")]);
    // The local consumer list cannot be read: unknown is never treated as a free card.
    rig.gpu.set_in_use("0000:02:00.0", None);
    let refused = rig
        .propose(
            "gpu_attach",
            json!({ "instance": "alpha", "pci": "0000:02:00.0" }),
        )
        .expect_err("an unknown in-use state refuses a VM attach");
    match refused {
        CallError::Refused { code, message } => {
            assert_eq!(code, "GpuInUseUnknown");
            assert_eq!(
                message,
                "Arca could not check whether the card is in use; try again"
            );
        }
        other => panic!("a refusal was expected, got {other:?}"),
    }
    assert_eq!(rig.list()["plans"], json!([]), "nothing was recorded");
}

#[test]
fn a_vm_attach_refuses_an_unknown_in_use_card_again_when_it_runs() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("proposal-gpu-unknown-run", &key);
    rig.gpu
        .set_instances(vec![gpu_instance("alpha", "arcak-gpu", "virtual-machine")]);
    rig.gpu.set_in_use("0000:02:00.0", Some(false));
    let proposed = rig
        .propose(
            "gpu_attach",
            json!({ "instance": "alpha", "pci": "0000:02:00.0" }),
        )
        .expect("a free card is proposed");
    let id = proposed["id"].as_str().expect("an id");
    let digest = proposed["argsDigest"].as_str().expect("a digest");
    let approval = approve_statement(
        &key,
        &statement(HOST, id, "gpu_attach", digest, &"d".repeat(32)),
    );
    let approved = rig
        .approve(id, &approval, NOW)
        .expect("the approval verifies");

    // The consumer list becomes unreadable after the proposal: execution re-reads it and refuses, never attaches.
    rig.gpu.set_in_use("0000:02:00.0", None);
    let refused = rig
        .executor
        .run(&approved.action, &approved.args, &approved.grant)
        .expect_err("execution re-reads the in-use state");
    match refused {
        CallError::Refused { code, message } => {
            assert_eq!(code, "GpuInUseUnknown");
            assert_eq!(
                message,
                "Arca could not check whether the card is in use; try again"
            );
        }
        other => panic!("a refusal was expected, got {other:?}"),
    }
    assert!(rig.gpu.calls().is_empty(), "nothing was attached");
}

#[test]
fn a_detach_refuses_an_absent_or_foreign_device_at_proposal_time() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("proposal-gpu-device", &key);
    let absent = rig
        .propose(
            "gpu_detach",
            json!({ "instance": "alpha", "pci": "0000:02:00.0" }),
        )
        .expect_err("there is no device to remove");
    assert!(matches!(
        absent,
        CallError::Refused { ref code, .. } if code == "GpuDeviceAbsent"
    ));

    rig.gpu.set_foreign_device(
        &gpu_instance("alpha", "arcak-gpu", "container"),
        "0000:02:00.0",
    );
    let foreign = rig
        .propose(
            "gpu_detach",
            json!({ "instance": "alpha", "pci": "0000:02:00.0" }),
        )
        .expect_err("the device is not Arca's");
    assert!(matches!(
        foreign,
        CallError::Refused { ref code, .. } if code == "GpuDeviceNotArca"
    ));
}

#[test]
fn a_detach_is_proposed_for_the_card_by_its_pci_address_even_after_it_left_the_inventory() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("proposal-gpu-detach", &key);
    mark_attached(&rig, "alpha", "0000:02:00.0");
    let proposed = rig
        .propose(
            "gpu_detach",
            json!({ "instance": "alpha", "pci": "0000:02:00.0" }),
        )
        .expect("a detach proposal");
    assert_eq!(proposed["action"], json!("gpu_detach"));
    assert_eq!(
        proposed["details"][0],
        json!({"label":"Project","value":"arcak-gpu"})
    );
    assert_eq!(
        proposed["details"][2],
        json!({ "label": "GPU", "value": "NVIDIA GeForce RTX 4060 Ti (0000:02:00.0)" })
    );
    assert!(
        proposed["details"][3]["value"]
            .as_str()
            .is_some_and(|effect| effect.contains("Removes"))
    );

    // The card can leave the inventory between the proposal and the run; the detach still names it by address.
    rig.gpu.set_cards(vec![arca_metrics::GpuCard::new(
        "0000:01:00.0",
        "NVIDIA GeForce RTX 4060 Ti",
    )]);
    let id = proposed["id"].as_str().expect("an id");
    let digest = proposed["argsDigest"].as_str().expect("a digest");
    let statement = statement(HOST, id, "gpu_detach", digest, &"b".repeat(32));
    let approval = approve_statement(&key, &statement);
    let approved = rig
        .approve(id, &approval, NOW)
        .expect("the approval verifies");
    let result = rig
        .executor
        .run(&approved.action, &approved.args, &approved.grant)
        .expect("the approved detach runs");
    assert_eq!(result["attached"], json!(false));
    assert_eq!(result["changed"], json!(true));
    assert_eq!(result["pci"], json!("0000:02:00.0"));
    assert_eq!(
        rig.gpu.calls(),
        vec![
            arca::gpu::GpuCall::Attach {
                project: "arcak-gpu".to_string(),
                instance: "alpha".to_string(),
                pci: "0000:02:00.0".to_string(),
            },
            arca::gpu::GpuCall::Detach {
                project: "arcak-gpu".to_string(),
                instance: "alpha".to_string(),
                pci: "0000:02:00.0".to_string(),
            },
        ]
    );
}

#[test]
fn a_watch_only_node_has_no_gpu_writer() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("proposal-gpu-disabled", &key);
    let clock = Arc::clone(&rig.clock);
    let handlers: std::collections::HashMap<&'static str, Arc<dyn ToolHandler>> = plan_handlers(
        Arc::clone(&rig.kernel),
        Arc::new(move || clock.load(Ordering::SeqCst)),
        None,
        None,
    )
    .into_iter()
    .collect();
    let refused = call_verb(
        &handlers,
        "action_propose",
        json!({
            "action": "gpu_attach",
            "args": { "instance": "alpha", "pci": "0000:02:00.0" }
        }),
    )
    .expect_err("a watch-only node refuses a GPU proposal");
    assert!(matches!(
        refused,
        CallError::Refused { ref code, .. } if code == "GpuDisabled"
    ));
    let direct = call_verb(
        &handlers,
        "gpu_attach",
        json!({ "instance": "alpha", "pci": "0000:02:00.0", "approval": "x" }),
    )
    .expect_err("a watch-only node has no GPU handler writer");
    assert!(matches!(
        direct,
        CallError::Refused { ref code, .. } if code == "GpuDisabled"
    ));
}

#[test]
fn a_foreign_or_retained_instance_is_refused_even_in_the_family() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("proposal-gpu-owner", &key);
    let unowned = rig
        .propose(
            "gpu_attach",
            json!({ "instance": "unowned", "pci": "0000:02:00.0" }),
        )
        .expect_err("an instance without the kernel mark is refused");
    assert!(
        matches!(unowned, CallError::Refused { ref code, .. } if code == "GpuInstanceNotKernel"),
        "{unowned:?}"
    );

    // The same alpha is a retained object when its entry's plan was declined.
    rig.gpu.retain("gpu-alpha");
    let retained = rig
        .propose(
            "gpu_attach",
            json!({ "instance": "alpha", "pci": "0000:02:00.0" }),
        )
        .expect_err("a retained instance is refused");
    assert!(
        matches!(retained, CallError::Refused { ref code, .. } if code == "GpuInstanceNotKernel"),
        "{retained:?}"
    );
}

#[test]
fn a_proposal_drops_confirm_true_from_what_it_binds_and_proposes_nothing_for_confirm_false() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("proposal-confirm", &key);
    for action in ActionKind::ALL {
        assert!(
            action.params_schema()["properties"]
                .get("confirm")
                .is_none(),
            "{}: an approved action never binds confirm",
            action.as_str()
        );
    }
    let bare = rig
        .propose("service_remove", json!({ "name": "nats" }))
        .expect("a removal proposal");
    let confirmed = rig
        .propose("service_remove", json!({ "name": "nats", "confirm": true }))
        .expect("confirm: true is dropped");
    assert_eq!(confirmed["args"], json!({ "name": "nats" }));
    assert_eq!(confirmed["args"], bare["args"]);
    assert_eq!(confirmed["argsDigest"], bare["argsDigest"]);
    assert_eq!(
        confirmed["id"], bare["id"],
        "with confirm: true it is the very proposal made without it"
    );
    assert_eq!(confirmed["summary"], bare["summary"]);
    let declined = rig
        .propose(
            "service_remove",
            json!({ "name": "nats", "confirm": false }),
        )
        .expect_err("confirm: false proposes nothing");
    assert!(
        matches!(&declined, CallError::Invalid(why) if why == "args/confirm: confirm is false: nothing was proposed"),
        "{declined:?}"
    );
    let yes = rig
        .propose(
            "service_remove",
            json!({ "name": "nats", "confirm": "yes" }),
        )
        .expect_err("a confirm that is not a boolean is refused");
    assert!(matches!(yes, CallError::Invalid(_)));
    assert_eq!(
        rig.list()["plans"].as_array().map(Vec::len),
        Some(1),
        "one proposal, recorded once; the refusals recorded nothing"
    );
}

/// A tenant's card names at most as many services as a tenant may hold instances, and counts the rest; the summary
/// counts every one. A name is cut, and a control character is shown as a space.
#[test]
fn a_tenants_removal_card_names_at_most_sixteen_services_and_counts_the_rest() {
    use arca::action_proposal::{HeldService, TENANT_SERVICES_NAMED, TenantHeld};
    let services = (0..TENANT_SERVICES_NAMED + 2)
        .map(|n| HeldService {
            name: format!("s{n:02}"),
            vm: n == 0,
            database: n == 1 || n == 2,
        })
        .collect();
    let held = TenantHeld {
        block: "192.0.2.16/28".to_string(),
        made_ms: 1_793_361_600_000,
        read_ms: 1_793_361_600_000 + 90 * 60_000,
        services,
    };
    let args = json!({ "id": "abcdefgh" });
    let fact = held.fact();
    let details = ActionKind::TenantRemove.details_with_gpu(&args, Some(&fact));
    let named: Vec<String> = (0..TENANT_SERVICES_NAMED)
        .map(|n| match n {
            0 => "s00 (virtual machine)".to_string(),
            1 | 2 => format!("s{n:02} (database)"),
            _ => format!("s{n:02} (container)"),
        })
        .collect();
    assert_eq!(details[3].label, "Services");
    assert_eq!(
        details[3].value,
        format!(
            "As of 2026-10-30 13:30 UTC, when this was proposed: {}, and 2 more",
            named.join(", ")
        )
    );
    assert_eq!(
        ActionKind::TenantRemove.summary_with(&args, Some(&fact)),
        "Remove the tenant abcdefgh (192.0.2.16/28, made 2026-10-30 12:00 UTC) and everything in it when the removal runs, services made after this proposal included; at 2026-10-30 13:30 UTC it held 18 services, 2 of them databases"
    );
    // A text that is not what the node writes gives the card without what it held.
    assert_eq!(
        ActionKind::TenantRemove.summary_with(&args, Some("not a tenant")),
        "Remove the tenant abcdefgh and everything in it"
    );
    let one = TenantHeld {
        block: "192.0.2.16/29".to_string(),
        made_ms: 1_793_361_600_000,
        read_ms: 1_793_361_600_000,
        services: vec![HeldService {
            name: format!("db\n{}", "x".repeat(80)),
            vm: false,
            database: true,
        }],
    };
    let fact = one.fact();
    assert!(
        ActionKind::TenantRemove
            .summary_with(&args, Some(&fact))
            .ends_with("; at 2026-10-30 12:00 UTC it held 1 service, a database")
    );
    let shown = &ActionKind::TenantRemove.details_with_gpu(&args, Some(&fact))[3].value;
    assert_eq!(
        shown,
        &format!(
            "As of 2026-10-30 12:00 UTC, when this was proposed: db {}… (database)",
            "x".repeat(61)
        )
    );
}

/// Arguments the action does not take are answered as a tool result with `isError`, in the words of the check that
/// refused them, so the model reads them and corrects the call; an action that is not one gets the list and the
/// nearest name. Nothing is recorded. These are the two calls of issue 111.
#[test]
fn arguments_the_action_does_not_take_are_an_is_error_result_in_words_with_the_nearest_action() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("proposal-mcp-invalid", &key);
    let (_runtime, socket, secret) = propose_door(&rig);
    let failure = |arguments: Value| {
        let reply = propose_through(&socket, &secret, arguments);
        assert!(reply.get("error").is_none(), "no protocol error: {reply}");
        let result = &reply["result"];
        assert_eq!(result["isError"], json!(true), "{reply}");
        assert_eq!(result["_meta"]["droidd/error"], json!("invalid_arguments"));
        result["content"][0]["text"]
            .as_str()
            .expect("the words")
            .to_string()
    };
    assert_eq!(
        failure(json!({ "action": "function_create", "args": {} })),
        "invalid_arguments: /args: missing required field name"
    );
    let params = (verb("action_propose").expect("the verb").params)();
    let all: Vec<&str> = params["properties"]["action"]["enum"]
        .as_array()
        .expect("the actions")
        .iter()
        .map(|action| action.as_str().expect("a name"))
        .collect();
    assert_eq!(all.len(), 24);
    assert_eq!(
        failure(json!({ "action": "postgres_creat", "args": { "name": "orders" } })),
        format!(
            "invalid_arguments: /action: not one of the allowed values; it is one of: {}; did you mean postgres_create?",
            all.join(", ")
        )
    );
    assert_eq!(rig.list()["plans"].as_array().map(Vec::len), Some(0));
}

/// Each example the tool's schema gives for an action's `args` is one the action takes: its own check passes it, or,
/// for an action whose card carries what the kernel reads at propose (a GPU's instance kind, a disk's model), it gets
/// past every check of what the caller gives and stops only at the first field the kernel fills in.
#[test]
fn every_example_in_the_schema_of_action_propose_is_one_its_action_takes() {
    let params = (verb("action_propose").expect("the verb").params)();
    let branches = params["properties"]["args"]["anyOf"]
        .as_array()
        .expect("one branch per action");
    assert_eq!(branches.len(), ActionKind::ALL.len());
    for action in ActionKind::ALL {
        let branch = branches
            .iter()
            .find(|branch| branch["title"] == json!(action.as_str()))
            .unwrap_or_else(|| panic!("{action:?} has a branch"));
        let example = &branch["examples"][0];
        if let Err(why) = action.prepare(example, Some(TEST_PG_IMAGE)) {
            assert!(
                why.ends_with(": the kernel resolves it when the proposal is made"),
                "{action:?} takes its example {example}: {why}"
            );
        }
    }
}

/// A field the kernel fills in when the proposal is made passes the door with a value off its own rule, as on 0.17.0:
/// the kernel removes or replaces it before the action's own check. Whatever the node answers next, it is not the
/// door's `invalid_arguments`. Review M1 of PR 136.
#[test]
fn a_field_the_kernel_fills_in_passes_the_door_with_any_value() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("proposal-mcp-kernel-filled", &key);
    let (_runtime, socket, secret) = propose_door(&rig);
    let params = (verb("action_propose").expect("the verb").params)();
    let branches = params["properties"]["args"]["anyOf"]
        .as_array()
        .expect("one branch per action");
    let passes = |action: &str, args: Value| {
        let reply = propose_through(&socket, &secret, json!({ "action": action, "args": args }));
        assert_ne!(
            reply["result"]["_meta"]["droidd/error"],
            json!("invalid_arguments"),
            "{action}: {reply}"
        );
    };
    for (action, fields) in arca_api::verbs::KERNEL_FILLED {
        let branch = branches
            .iter()
            .find(|branch| branch["title"] == json!(action))
            .expect("a branch");
        for field in fields {
            let mut args = branch["examples"][0].clone();
            args[*field] = json!({ "off": "rule" });
            passes(action, args);
        }
    }
    passes(
        "snapshot_delete",
        json!({ "name": "shop--web--20261030T1200", "pool": "usb1", "kind": "container" }),
    );
}
