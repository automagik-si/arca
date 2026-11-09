//! Action proposals: the inert record, its kernel-built summary, the closed action list, the waiting limit, and the
//! approval checks that gate exactly one run. All of it runs in-process against the toy kernel and fake Incus; no service or
//! database is touched.

mod common;

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
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

fn create_args(name: &str) -> Value {
    json!({ "name": name, "template": "nats" })
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
            .is_some_and(|rows| rows.len() == 2),
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
    assert_eq!(
        arca_api::may_call(Scope::Viewer, verb("approval_submit").expect("present")),
        false,
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
            ActionKind::ServiceCreate => json!({ "name": "nats", "template": "nats" }),
            ActionKind::ServiceRemove | ActionKind::ServiceUnpublish => json!({ "name": "nats" }),
            ActionKind::ServicePublish => json!({ "name": "nats", "reach": "tailnet" }),
            ActionKind::ServiceConnect | ActionKind::ServiceDisconnect => {
                json!({ "from": "api", "to": "nats" })
            }
            ActionKind::PostgresCreate => json!({ "name": "orders" }),
            ActionKind::GpuAttach | ActionKind::GpuDetach => json!({
                "project": "arcak-gpu",
                "instance": "alpha",
                "kind": "container",
                "uuid": "00000000-0000-4000-8000-0000000000a1",
                "pci": "0000:01:00.0",
            }),
        }
    }

    for action in ActionKind::ALL {
        let prepared = action
            .prepare(&sample(action), Some(TEST_PG_IMAGE))
            .expect("the sample is valid");
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
        let schema = action.params_schema();
        let fields = schema["properties"].as_object().expect("properties");
        let mut seen = 0;
        for field in fields.keys() {
            let label = match (action, field.as_str()) {
                (ActionKind::PostgresCreate, "name") => "Database",
                (_, "name") => "Name",
                (_, "template") => "Template",
                (_, "reach") => "Reach",
                (_, "from") => "Service",
                (_, "to") => "To",
                (_, "image") => "Image",
                _ => panic!("{action:?} field {field} has no owner label"),
            };
            let row = details
                .iter()
                .find(|row| row.label == label)
                .unwrap_or_else(|| panic!("{action:?} details lack {label}"));
            assert_eq!(
                row.value,
                prepared[field.as_str()].as_str().expect("a string field"),
                "{action:?} {label}"
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

#[test]
fn action_propose_succeeds_through_the_mcp_door() {
    let key = SkKey::from_seed(1);
    let rig = Rig::new("proposal-mcp", &key);
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
    let call = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "action_propose",
            "arguments": {
                "action": "service_create",
                "args": { "name": "nats", "template": "nats" }
            }
        }
    });
    let body = serde_json::to_vec(&call).expect("the call");
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {secret}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut stream = UnixStream::connect(&socket).expect("connect to MCP");
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
    let reply: Value = serde_json::from_slice(&raw[split + 4..]).expect("JSON-RPC");
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
    let error = rig
        .propose("service_create", create_args("rate-extra"))
        .expect_err("the token's window is full");
    assert!(matches!(
        error,
        CallError::Refused { ref code, .. } if code == "RateLimited"
    ));
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
    let error = rig
        .propose("service_create", create_args("new-extra"))
        .expect_err("the thirty accepted proposals fill the window");
    assert!(matches!(
        error,
        CallError::Refused { ref code, .. } if code == "RateLimited"
    ));
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
    let error = rig
        .propose("service_create", create_args("spent-extra"))
        .expect_err("the refusals counted nothing, so thirty new proposals fill the window");
    assert!(matches!(
        error,
        CallError::Refused { ref code, .. } if code == "RateLimited"
    ));
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
    match ninth {
        CallError::Refused { code, .. } => assert_eq!(code, "TooManyProposals"),
        other => panic!("a refusal was expected, got {other:?}"),
    }
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
        .journal_action_result(&approved, true, None);
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
